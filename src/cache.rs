//! On-disk library snapshots and bounded async cover-image caching.

use std::collections::HashSet;
use std::num::NonZeroUsize;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use lru::LruCache;
use ratatui::layout::Rect;
use ratatui_image::Resize;
use ratatui_image::picker::Picker;
use ratatui_image::protocol::StatefulProtocol;
use serde::{Deserialize, Serialize};
use thiserror::Error;
use tokio::sync::{Semaphore, mpsc};

use crate::domain::{LibraryCollection, MediaId};

/// Current schema version for serialized [`LibrarySnapshot`] files on disk.
pub const SNAPSHOT_SCHEMA_VERSION: u32 = 2;

const DEFAULT_COVER_CACHE_CAPACITY: usize = 64;
const MAX_CONCURRENT_COVER_DOWNLOADS: usize = 4;

static TEMP_FILE_SEQ: AtomicU64 = AtomicU64::new(0);

/// Errors returned when serializing or writing cached library snapshots.
#[derive(Debug, Error)]
pub enum CacheError {
    #[error("I/O error accessing {path}: {source}")]
    Io {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("failed to serialize library snapshot JSON: {0}")]
    Json(#[from] serde_json::Error),
    #[error("background snapshot task failed: {0}")]
    Join(#[from] tokio::task::JoinError),
}

/// Versioned on-disk library snapshot used for instant stale-while-revalidate startup.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LibrarySnapshot {
    pub version: u32,
    pub username: String,
    pub saved_at_unix: u64,
    pub library: LibraryCollection,
}

/// Sanitizes an AniList username into a safe lowercase ASCII filename stem.
#[must_use]
pub fn sanitize_username(username: &str) -> String {
    let trimmed = username.trim();
    if trimmed.is_empty() {
        return "default".to_owned();
    }

    let mut sanitized = String::with_capacity(trimmed.len());
    for ch in trimmed.chars() {
        if ch.is_ascii_alphanumeric() || ch == '_' || ch == '-' {
            sanitized.push(ch.to_ascii_lowercase());
        } else {
            sanitized.push('_');
        }
    }
    sanitized
}

/// Returns the disk path for `username`'s cached library snapshot inside `cache_dir`.
#[must_use]
pub fn snapshot_path(cache_dir: &Path, username: &str) -> PathBuf {
    cache_dir.join(format!("library_{}.json", sanitize_username(username)))
}

/// Synchronously loads a cached [`LibrarySnapshot`] at startup.
///
/// Returns `None` if the file is missing, unreadable, invalid JSON, has a mismatched
/// schema version, or belongs to a different username. Rebuilds search indexes before returning.
#[must_use]
pub fn load_library_snapshot(cache_dir: &Path, username: &str) -> Option<LibrarySnapshot> {
    let path = snapshot_path(cache_dir, username);
    let bytes = std::fs::read(path).ok()?;
    let mut snap: LibrarySnapshot = serde_json::from_slice(&bytes).ok()?;
    if snap.version != SNAPSHOT_SCHEMA_VERSION
        || !snap.username.eq_ignore_ascii_case(username.trim())
    {
        return None;
    }
    snap.library.rebuild_search_indexes();
    Some(snap)
}

/// Atomically writes `library` as a JSON [`LibrarySnapshot`] inside `cache_dir`.
pub async fn save_library_snapshot(
    cache_dir: PathBuf,
    username: String,
    library: LibraryCollection,
) -> Result<(), CacheError> {
    tokio::fs::create_dir_all(&cache_dir)
        .await
        .map_err(|source| CacheError::Io {
            path: cache_dir.clone(),
            source,
        })?;

    let dest = snapshot_path(&cache_dir, &username);
    let saved_at_unix = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_secs());
    let snapshot = LibrarySnapshot {
        version: SNAPSHOT_SCHEMA_VERSION,
        username: username.trim().to_owned(),
        saved_at_unix,
        library,
    };

    let bytes = tokio::task::spawn_blocking(move || serde_json::to_vec(&snapshot)).await??;
    write_file_atomic(&cache_dir, &dest, "library", &bytes).await
}

/// Removes `username`'s cached library snapshot on a best-effort basis (for logout).
pub fn remove_library_snapshot(cache_dir: &Path, username: &str) {
    let path = snapshot_path(cache_dir, username);
    let _ = std::fs::remove_file(path);
}

/// Completion event produced by background cover loading or resizing tasks.
pub struct CoverLoaded {
    pub media_id: MediaId,
    pub result: Result<StatefulProtocol, String>,
}

/// Bounded LRU cache of encoded cover protocols with in-flight and failure deduplication.
pub struct CoverCache {
    entries: LruCache<MediaId, StatefulProtocol>,
    in_flight: HashSet<MediaId>,
    failed: HashSet<MediaId>,
}

impl Default for CoverCache {
    fn default() -> Self {
        Self::new(DEFAULT_COVER_CACHE_CAPACITY)
    }
}

impl CoverCache {
    /// Creates a bounded [`CoverCache`] with capacity `capacity` (clamped to at least 1).
    #[must_use]
    pub fn new(capacity: usize) -> Self {
        let cap = NonZeroUsize::new(capacity).unwrap_or(NonZeroUsize::MIN);
        Self {
            entries: LruCache::new(cap),
            in_flight: HashSet::new(),
            failed: HashSet::new(),
        }
    }

    /// Returns a mutable reference to a cached protocol, promoting it to most-recently-used.
    pub fn get_mut(&mut self, id: MediaId) -> Option<&mut StatefulProtocol> {
        self.entries.get_mut(&id)
    }

    /// Pops a cached protocol out of the cache (for off-thread resize/re-encoding).
    pub fn take(&mut self, id: MediaId) -> Option<StatefulProtocol> {
        self.entries.pop(&id)
    }

    /// Inserts a ready [`StatefulProtocol`] into the cache and clears any loading/failed flags.
    pub fn insert(&mut self, id: MediaId, proto: StatefulProtocol) {
        self.in_flight.remove(&id);
        self.failed.remove(&id);
        self.entries.put(id, proto);
    }

    /// Marks `id` as in-flight and returns `true` unless already cached, in-flight, or failed.
    pub fn mark_in_flight(&mut self, id: MediaId) -> bool {
        if self.entries.contains(&id)
            || self.in_flight.contains(&id)
            || self.failed.contains(&id)
        {
            return false;
        }
        self.in_flight.insert(id);
        true
    }

    /// Removes `id` from the in-flight set.
    pub fn finish_in_flight(&mut self, id: MediaId) {
        self.in_flight.remove(&id);
    }

    /// Marks `id` as failed and removes it from the in-flight set.
    pub fn mark_failed(&mut self, id: MediaId) {
        self.in_flight.remove(&id);
        self.failed.insert(id);
    }

    /// Clears recorded cover failures so previously failed covers can be retried.
    pub fn clear_failures(&mut self) {
        self.failed.clear();
    }

    /// Drops all cached covers, in-flight markers, and failure markers.
    pub fn clear(&mut self) {
        self.entries.clear();
        self.in_flight.clear();
        self.failed.clear();
    }

    /// Returns `true` if `id` is currently stored in the LRU cache.
    #[must_use]
    pub fn contains(&self, id: MediaId) -> bool {
        self.entries.contains(&id)
    }

    /// Returns `true` if `id` currently has a pending load or re-encode request.
    #[must_use]
    pub fn is_loading(&self, id: MediaId) -> bool {
        self.in_flight.contains(&id)
    }
}

/// Asynchronous cover fetcher and off-thread image decoder/encoder.
#[derive(Clone)]
pub struct CoverLoader {
    http: reqwest::Client,
    covers_dir: PathBuf,
    picker: Picker,
    semaphore: Arc<Semaphore>,
}

impl CoverLoader {
    /// Creates a [`CoverLoader`] backed by `http`, caching raw images in `covers_dir`.
    #[must_use]
    pub fn new(http: reqwest::Client, covers_dir: PathBuf, picker: Picker) -> Self {
        Self {
            http,
            covers_dir,
            picker,
            semaphore: Arc::new(Semaphore::new(MAX_CONCURRENT_COVER_DOWNLOADS)),
        }
    }

    /// Returns the disk cache path for `media_id`'s cover bytes.
    #[must_use]
    pub fn cover_path(&self, media_id: MediaId) -> PathBuf {
        self.covers_dir.join(format!("{}.img", media_id.get()))
    }

    /// Spawns a background task that loads or downloads, decodes, and pre-encodes a cover image.
    pub fn request_cover(
        &self,
        media_id: MediaId,
        url: String,
        target_area: Rect,
        tx: mpsc::UnboundedSender<CoverLoaded>,
    ) {
        let http = self.http.clone();
        let covers_dir = self.covers_dir.clone();
        let disk_path = self.cover_path(media_id);
        let picker = self.picker;
        let semaphore = Arc::clone(&self.semaphore);

        tokio::spawn(async move {
            let result = load_or_fetch_cover(
                http,
                covers_dir,
                disk_path,
                picker,
                semaphore,
                url,
                target_area,
            )
            .await;
            let _ = tx.send(CoverLoaded { media_id, result });
        });
    }

    /// Moves an existing [`StatefulProtocol`] off-thread to re-encode it for `target_area`.
    pub fn reencode_cover(
        media_id: MediaId,
        mut proto: StatefulProtocol,
        target_area: Rect,
        tx: mpsc::UnboundedSender<CoverLoaded>,
    ) {
        tokio::spawn(async move {
            let result = tokio::task::spawn_blocking(move || {
                pre_encode_protocol(&mut proto, target_area);
                proto
            })
            .await
            .map_err(|e| format!("cover resize task failed: {e}"));

            let _ = tx.send(CoverLoaded { media_id, result });
        });
    }
}

async fn load_or_fetch_cover(
    http: reqwest::Client,
    covers_dir: PathBuf,
    disk_path: PathBuf,
    picker: Picker,
    semaphore: Arc<Semaphore>,
    url: String,
    target_area: Rect,
) -> Result<StatefulProtocol, String> {
    if let Ok(disk_bytes) = tokio::fs::read(&disk_path).await {
        if !disk_bytes.is_empty() {
            match decode_and_encode(disk_bytes, picker, target_area).await {
                Ok(proto) => return Ok(proto),
                Err(_) => {
                    // Corrupt cached image: delete it and fall back to one fresh download.
                    let _ = tokio::fs::remove_file(&disk_path).await;
                }
            }
        }
    }

    let permit = semaphore
        .acquire_owned()
        .await
        .map_err(|e| format!("cover semaphore closed: {e}"))?;

    let response = http
        .get(&url)
        .send()
        .await
        .map_err(|e| format!("cover download failed: {e}"))?;
    let status = response.status();
    if !status.is_success() {
        return Err(format!("cover download returned HTTP {status}"));
    }
    let body = response
        .bytes()
        .await
        .map_err(|e| format!("cover response read failed: {e}"))?;
    // Release network concurrency slot before blocking decode or disk I/O.
    drop(permit);

    if body.is_empty() {
        return Err("cover download returned empty body".to_owned());
    }

    let proto = decode_and_encode(body.clone(), picker, target_area).await?;
    let _ = write_file_atomic(&covers_dir, &disk_path, "cover", body.as_ref()).await;
    Ok(proto)
}

async fn decode_and_encode<B>(
    bytes: B,
    picker: Picker,
    target_area: Rect,
) -> Result<StatefulProtocol, String>
where
    B: AsRef<[u8]> + Send + 'static,
{
    tokio::task::spawn_blocking(move || {
        let img = image::load_from_memory(bytes.as_ref())
            .map_err(|e| format!("image decode failed: {e}"))?;
        let mut proto = picker.new_resize_protocol(img);
        pre_encode_protocol(&mut proto, target_area);
        Ok(proto)
    })
    .await
    .map_err(|e| format!("cover decode task failed: {e}"))?
}

fn pre_encode_protocol(proto: &mut StatefulProtocol, target_area: Rect) {
    if target_area.width > 0 && target_area.height > 0 {
        if let Some(rect) = proto.needs_resize(&Resize::Fit(None), target_area) {
            let bg = proto.background_color();
            proto.resize_encode(&Resize::Fit(None), bg, rect);
        }
    }
}

fn unique_temp_path(dir: &Path, prefix: &str) -> PathBuf {
    let seq = TEMP_FILE_SEQ.fetch_add(1, Ordering::Relaxed);
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_nanos());
    dir.join(format!(
        ".{prefix}.tmp.{}.{nanos}.{seq}",
        std::process::id()
    ))
}

async fn write_file_atomic(
    dir: &Path,
    dest: &Path,
    prefix: &str,
    bytes: &[u8],
) -> Result<(), CacheError> {
    tokio::fs::create_dir_all(dir)
        .await
        .map_err(|source| CacheError::Io {
            path: dir.to_path_buf(),
            source,
        })?;

    let tmp = unique_temp_path(dir, prefix);
    if let Err(source) = tokio::fs::write(&tmp, bytes).await {
        let _ = tokio::fs::remove_file(&tmp).await;
        return Err(CacheError::Io { path: tmp, source });
    }
    if let Err(source) = tokio::fs::rename(&tmp, dest).await {
        let _ = tokio::fs::remove_file(&tmp).await;
        return Err(CacheError::Io {
            path: dest.to_path_buf(),
            source,
        });
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;
    use std::io::{Read, Write};
    use std::net::TcpListener;
    use std::num::NonZeroU32;

    use image::{DynamicImage, ImageBuffer, ImageFormat, Rgba};

    use super::*;
    use crate::domain::{AnimeItem, EntryId, LibraryEntry, LibraryStatus, Point3Score};

    fn unique_temp_dir(label: &str) -> PathBuf {
        let seq = TEMP_FILE_SEQ.fetch_add(1, Ordering::Relaxed);
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("system clock after unix epoch")
            .as_nanos();
        std::env::temp_dir().join(format!(
            "anilist_tui_cache_test_{label}_{}_{nanos}_{seq}",
            std::process::id()
        ))
    }

    fn media_id(raw: u32) -> MediaId {
        MediaId(NonZeroU32::new(raw).expect("non-zero media id"))
    }

    fn sample_library() -> LibraryCollection {
        let item = AnimeItem::new(
            media_id(42),
            "Frieren: Beyond Journey's End".to_owned(),
            Some("Sousou no Frieren".to_owned()),
            Some(28),
            "FINISHED".to_owned(),
            Some(91),
            vec!["Adventure".to_owned(), "Drama".to_owned()],
            "An elf mage journeys onward.".to_owned(),
            Some("https://example.com/cover.png".to_owned()),
            Some("https://anilist.co/anime/154587".to_owned()),
            LibraryEntry {
                id: NonZeroU32::new(1001).map(EntryId),
                status: LibraryStatus::Watching,
                progress: 12,
                score: Some(Point3Score::Happy.as_score_raw()),
            },
        );
        let mut map = HashMap::new();
        map.insert(LibraryStatus::Watching, vec![item]);
        LibraryCollection::from_lists(map)
    }

    fn test_picker() -> Picker {
        Picker::from_fontsize((8, 16))
    }

    fn dummy_protocol() -> StatefulProtocol {
        let img = DynamicImage::ImageRgba8(ImageBuffer::from_pixel(
            4,
            4,
            Rgba([200, 100, 50, 255]),
        ));
        test_picker().new_resize_protocol(img)
    }

    fn tiny_png_bytes() -> Vec<u8> {
        let img = DynamicImage::ImageRgba8(ImageBuffer::from_pixel(
            8,
            12,
            Rgba([120, 180, 240, 255]),
        ));
        let mut cursor = std::io::Cursor::new(Vec::new());
        img.write_to(&mut cursor, ImageFormat::Png)
            .expect("encode PNG");
        cursor.into_inner()
    }

    #[test]
    fn sanitize_username_prevents_unsafe_paths_and_handles_empty() {
        assert_eq!(sanitize_username(""), "default");
        assert_eq!(sanitize_username("   "), "default");
        assert_eq!(sanitize_username("Nkaet"), "nkaet");
        assert_eq!(sanitize_username("User_Name-1"), "user_name-1");
        assert_eq!(sanitize_username("../../etc/passwd"), "______etc_passwd");
        assert_eq!(sanitize_username("foo\\bar/baz.qux"), "foo_bar_baz_qux");
        assert_eq!(sanitize_username("Alice Bob"), "alice_bob");
        assert_eq!(sanitize_username(" . "), "_");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn snapshot_roundtrip_rebuilds_search_haystack_and_remove_cleans_up() {
        let dir = unique_temp_dir("roundtrip");
        let library = sample_library();

        save_library_snapshot(dir.clone(), "Alice".to_owned(), library.clone())
            .await
            .expect("save snapshot");

        let loaded = load_library_snapshot(&dir, "alice").expect("load snapshot");
        assert_eq!(loaded.version, SNAPSHOT_SCHEMA_VERSION);
        assert_eq!(loaded.username, "Alice");
        assert_eq!(loaded.library, library);

        let item = loaded
            .library
            .get(media_id(42))
            .expect("item exists after reload");
        assert!(item.matches_filter("frieren"));
        assert!(item.matches_filter("SOUSOU"));
        assert!(item.matches_filter("adventure"));
        assert!(!item.matches_filter("mecha"));

        remove_library_snapshot(&dir, "Alice");
        assert!(load_library_snapshot(&dir, "Alice").is_none());

        let _ = std::fs::remove_dir_all(dir);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn snapshot_rejects_schema_version_and_username_mismatch() {
        let dir = unique_temp_dir("mismatch");
        let library = sample_library();

        save_library_snapshot(dir.clone(), "Alice".to_owned(), library)
            .await
            .expect("save snapshot");

        let path = snapshot_path(&dir, "Alice");
        let raw = std::fs::read(&path).expect("read saved snapshot");
        let mut snap: LibrarySnapshot = serde_json::from_slice(&raw).expect("parse snapshot");

        // Username mismatch on same file path.
        assert!(load_library_snapshot(&dir, "Bob").is_none());
        snap.username = "Charlie".to_owned();
        std::fs::write(&path, serde_json::to_vec(&snap).expect("serialize")).expect("write");
        assert!(load_library_snapshot(&dir, "Alice").is_none());

        // Schema version mismatch.
        snap.username = "Alice".to_owned();
        snap.version = SNAPSHOT_SCHEMA_VERSION + 1;
        std::fs::write(&path, serde_json::to_vec(&snap).expect("serialize")).expect("write");
        assert!(load_library_snapshot(&dir, "Alice").is_none());

        // Invalid JSON.
        std::fs::write(&path, b"{not valid json").expect("write invalid json");
        assert!(load_library_snapshot(&dir, "Alice").is_none());

        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn cover_cache_lru_eviction_dedup_and_failure_tracking() {
        let mut cache = CoverCache::new(2);
        let id1 = media_id(1);
        let id2 = media_id(2);
        let id3 = media_id(3);

        // In-flight deduplication and completion.
        assert!(cache.mark_in_flight(id1));
        assert!(cache.is_loading(id1));
        assert!(!cache.mark_in_flight(id1));
        cache.finish_in_flight(id1);
        assert!(!cache.is_loading(id1));

        // Failure blocks retries until clear_failures().
        assert!(cache.mark_in_flight(id1));
        cache.mark_failed(id1);
        assert!(!cache.is_loading(id1));
        assert!(!cache.mark_in_flight(id1));
        cache.clear_failures();
        assert!(cache.mark_in_flight(id1));

        // Insert id1 and id2; touch id1 so id2 becomes LRU and gets evicted by id3.
        cache.insert(id1, dummy_protocol());
        assert!(!cache.is_loading(id1));
        assert!(!cache.mark_in_flight(id1));
        cache.insert(id2, dummy_protocol());

        assert!(cache.get_mut(id1).is_some());
        cache.insert(id3, dummy_protocol());

        assert!(cache.contains(id1));
        assert!(!cache.contains(id2));
        assert!(cache.contains(id3));

        // Take removes from cache and allows re-inserting or re-fetching.
        assert!(cache.take(id1).is_some());
        assert!(!cache.contains(id1));

        // Zero capacity clamps to 1 rather than panicking.
        let mut min_cache = CoverCache::new(0);
        min_cache.insert(id1, dummy_protocol());
        min_cache.insert(id2, dummy_protocol());
        assert!(!min_cache.contains(id1));
        assert!(min_cache.contains(id2));

        min_cache.clear();
        assert!(!min_cache.contains(id2));
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn cover_loader_loads_from_disk_and_reencodes_off_thread() {
        let dir = unique_temp_dir("cover_disk");
        std::fs::create_dir_all(&dir).expect("create covers dir");

        let loader = CoverLoader::new(reqwest::Client::new(), dir.clone(), test_picker());
        let id = media_id(77);
        let disk_path = loader.cover_path(id);
        assert!(disk_path.ends_with("77.img"));
        std::fs::write(&disk_path, tiny_png_bytes()).expect("write valid png");

        let (tx, mut rx) = mpsc::unbounded_channel();
        loader.request_cover(
            id,
            "http://127.0.0.1:9/unused.png".to_owned(),
            Rect::new(0, 0, 10, 10),
            tx.clone(),
        );

        let loaded = rx.recv().await.expect("receive loaded cover");
        assert_eq!(loaded.media_id, id);
        let proto = loaded.result.expect("disk cover decode succeeded");

        CoverLoader::reencode_cover(id, proto, Rect::new(0, 0, 20, 20), tx.clone());
        let resized = rx.recv().await.expect("receive resized cover");
        assert_eq!(resized.media_id, id);
        let proto2 = resized.result.expect("reencode succeeded");

        CoverLoader::reencode_cover(id, proto2, Rect::default(), tx);
        let zero_area = rx.recv().await.expect("receive zero-area reencode");
        assert!(zero_area.result.is_ok());

        let _ = std::fs::remove_dir_all(dir);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn cover_loader_rejects_http_error_and_empty_body_without_writing_cache() {
        let dir = unique_temp_dir("cover_http_err");
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind loopback server");
        let addr = listener.local_addr().expect("local addr");
        let server = std::thread::spawn(move || {
            for response in [
                "HTTP/1.1 404 Not Found\r\nContent-Length: 9\r\nConnection: close\r\n\r\nnot found",
                "HTTP/1.1 200 OK\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
            ] {
                if let Ok((mut stream, _)) = listener.accept() {
                    let mut req_buf = [0u8; 512];
                    let _ = stream.read(&mut req_buf);
                    let _ = stream.write_all(response.as_bytes());
                    let _ = stream.flush();
                }
            }
        });

        let loader = CoverLoader::new(reqwest::Client::new(), dir.clone(), test_picker());
        let (tx, mut rx) = mpsc::unbounded_channel();

        let id_404 = media_id(91);
        loader.request_cover(
            id_404,
            format!("http://{addr}/404.png"),
            Rect::new(0, 0, 8, 8),
            tx.clone(),
        );
        let res_404 = rx.recv().await.expect("receive 404 result");
        assert!(res_404.result.is_err());
        assert!(!loader.cover_path(id_404).exists());

        let id_empty = media_id(92);
        loader.request_cover(
            id_empty,
            format!("http://{addr}/empty.png"),
            Rect::new(0, 0, 8, 8),
            tx,
        );
        let res_empty = rx.recv().await.expect("receive empty body result");
        assert!(res_empty.result.is_err());
        assert!(!loader.cover_path(id_empty).exists());

        let _ = server.join();
        let _ = std::fs::remove_dir_all(dir);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn cover_loader_deletes_corrupt_disk_file_and_downloads_fresh_bytes() {
        let dir = unique_temp_dir("cover_corrupt");
        std::fs::create_dir_all(&dir).expect("create covers dir");

        let png = tiny_png_bytes();
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind loopback server");
        let addr = listener.local_addr().expect("local addr");
        let png_resp = png.clone();
        let server = std::thread::spawn(move || {
            if let Ok((mut stream, _)) = listener.accept() {
                let mut req_buf = [0u8; 512];
                let _ = stream.read(&mut req_buf);
                let header = format!(
                    "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                    png_resp.len()
                );
                let _ = stream.write_all(header.as_bytes());
                let _ = stream.write_all(&png_resp);
                let _ = stream.flush();
            }
        });

        let loader = CoverLoader::new(reqwest::Client::new(), dir.clone(), test_picker());
        let id = media_id(88);
        let disk_path = loader.cover_path(id);
        std::fs::write(&disk_path, b"corrupt image payload").expect("write corrupt cache");

        let (tx, mut rx) = mpsc::unbounded_channel();
        loader.request_cover(
            id,
            format!("http://{addr}/cover.png"),
            Rect::new(0, 0, 12, 12),
            tx,
        );

        let loaded = rx.recv().await.expect("receive recovered cover");
        assert_eq!(loaded.media_id, id);
        assert!(loaded.result.is_ok());

        let persisted = std::fs::read(&disk_path).expect("read refreshed disk cache");
        assert_eq!(persisted, png);

        let _ = server.join();
        let _ = std::fs::remove_dir_all(dir);
    }
}
