//! Persistent AniList GraphQL client, rate-limit tracking, and response decoding.

use std::collections::HashSet;
use std::num::NonZeroU32;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use reqwest::StatusCode;
use reqwest::header::{ACCEPT, AUTHORIZATION, HeaderMap, HeaderValue, USER_AGENT};
use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::domain::{
    AnimeItem, EntryId, LibraryCollection, LibraryEntry, LibraryStatus, MediaId, ScoreFormat,
    ScoreRaw, ScoreUpdate, clean_description,
};

pub const ANILIST_GRAPHQL_URL: &str = "https://graphql.anilist.co";

const DEFAULT_PER_CHUNK: u32 = 500;
const MAX_CHUNKS: u32 = 20;
const CHUNK_PACE_DELAY: Duration = Duration::from_millis(150);
const READ_RETRY_DELAY: Duration = Duration::from_millis(350);
const DEFAULT_RETRY_AFTER_SECS: u64 = 60;
const MAX_BACKOFF_SECS: u64 = 120;

const FULL_LIBRARY_QUERY: &str = r#"
query ($username: String!, $chunk: Int!, $perChunk: Int!) {
  MediaListCollection(
    userName: $username,
    type: ANIME,
    chunk: $chunk,
    perChunk: $perChunk,
    forceSingleCompletedList: true,
    sort: [UPDATED_TIME_DESC]
  ) {
    hasNextChunk
    user {
      mediaListOptions {
        scoreFormat
      }
    }
    lists {
      status
      isCustomList
      entries {
        id
        status
        progress
        score(format: POINT_100)
        media {
          id
          title {
            userPreferred
            english
            romaji
          }
          siteUrl
          episodes
          description(asHtml: false)
          status
          averageScore
          genres
          coverImage {
            large
            medium
          }
        }
      }
    }
  }
}
"#;

const VIEWER_QUERY: &str = r#"
query {
  Viewer {
    name
  }
}
"#;

const SAVE_ENTRY_MUTATION: &str = r#"
mutation ($mediaId: Int!, $status: MediaListStatus, $progress: Int, $scoreRaw: Int) {
  SaveMediaListEntry(mediaId: $mediaId, status: $status, progress: $progress, scoreRaw: $scoreRaw) {
    id
    status
    progress
    score(format: POINT_100)
  }
}
"#;

const DELETE_ENTRY_MUTATION: &str = r#"
mutation ($id: Int!) {
  DeleteMediaListEntry(id: $id) {
    deleted
  }
}
"#;

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct RateLimitInfo {
    pub limit: Option<u32>,
    pub remaining: Option<u32>,
    pub reset_at_unix: Option<u64>,
}

#[derive(Debug, Default)]
struct RateLimitTracker {
    info: RateLimitInfo,
    blocked_until: Option<Instant>,
}

#[derive(Debug, Error)]
pub enum AnilistError {
    #[error("HTTP request failed: {0}")]
    Http(#[from] reqwest::Error),
    #[error("AniList authentication rejected (HTTP 401): token expired or invalid")]
    Unauthorized,
    #[error("AniList rate limit reached; retry in {retry_after_secs}s")]
    RateLimited { retry_after_secs: u64 },
    #[error("HTTP {status} from AniList API: {body}")]
    HttpStatus { status: u16, body: String },
    #[error("failed to decode AniList JSON response: {0}")]
    Decode(#[from] serde_json::Error),
    #[error("AniList GraphQL errors: {0}")]
    GraphQl(String),
    #[error("missing expected `data` field in AniList response")]
    MissingData,
}

impl AnilistError {
    #[must_use]
    pub const fn is_auth_error(&self) -> bool {
        matches!(self, Self::Unauthorized)
    }

    #[must_use]
    pub const fn is_rate_limited(&self) -> bool {
        matches!(self, Self::RateLimited { .. })
    }

    #[must_use]
    pub fn is_transient(&self) -> bool {
        match self {
            Self::Http(err) => {
                err.is_timeout() || err.is_connect() || err.is_request() || err.is_body()
            }
            Self::HttpStatus { status, .. } => matches!(status, 502..=504),
            _ => false,
        }
    }
}

#[derive(Debug, Clone)]
pub struct AnilistClient {
    http: reqwest::Client,
    endpoint: String,
    rate_limit: Arc<Mutex<RateLimitTracker>>,
}

impl AnilistClient {
    pub fn new() -> Result<Self, AnilistError> {
        let mut headers = HeaderMap::new();
        headers.insert(
            USER_AGENT,
            HeaderValue::from_static(concat!("anits/", env!("CARGO_PKG_VERSION"))),
        );

        let http = reqwest::Client::builder()
            .default_headers(headers)
            .connect_timeout(Duration::from_secs(10))
            .timeout(Duration::from_secs(20))
            .pool_idle_timeout(Duration::from_secs(90))
            .build()?;

        Ok(Self {
            http,
            endpoint: ANILIST_GRAPHQL_URL.to_owned(),
            rate_limit: Arc::new(Mutex::new(RateLimitTracker::default())),
        })
    }

    #[cfg(test)]
    fn with_endpoint(endpoint: String) -> Result<Self, AnilistError> {
        let mut client = Self::new()?;
        client.endpoint = endpoint;
        Ok(client)
    }

    #[must_use]
    pub fn http(&self) -> &reqwest::Client {
        &self.http
    }

    #[must_use]
    pub fn rate_limit_info(&self) -> RateLimitInfo {
        self.rate_limit
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .info
    }

    pub async fn fetch_viewer_name(&self, token: &str) -> Result<String, AnilistError> {
        #[derive(Deserialize)]
        struct ViewerData {
            #[serde(rename = "Viewer")]
            viewer: Option<ViewerNode>,
        }
        #[derive(Deserialize)]
        struct ViewerNode {
            name: String,
        }

        let data: ViewerData = self
            .post_graphql_with_retry(VIEWER_QUERY, serde_json::json!({}), Some(token))
            .await?;
        data.viewer
            .map(|v| v.name)
            .ok_or(AnilistError::MissingData)
    }

    pub async fn fetch_full_library(
        &self,
        username: &str,
        token: Option<&str>,
    ) -> Result<LibraryCollection, AnilistError> {
        let mut grouped: [Vec<AnimeItem>; 6] = Default::default();
        let mut seen_ids = HashSet::new();
        let mut score_format = None;

        for chunk in 1..=MAX_CHUNKS {
            if chunk > 1 {
                tokio::time::sleep(CHUNK_PACE_DELAY).await;
            }
            let vars = serde_json::json!({
                "username": username,
                "chunk": chunk,
                "perChunk": DEFAULT_PER_CHUNK,
            });
            let data: FullLibraryData = self
                .post_graphql_with_retry(FULL_LIBRARY_QUERY, vars, token)
                .await?;
            let has_next =
                merge_library_chunk(data, &mut grouped, &mut seen_ids, &mut score_format);
            if !has_next {
                break;
            }
        }

        Ok(LibraryCollection::from_array(grouped, score_format))
    }

    pub async fn save_entry(
        &self,
        token: &str,
        media_id: MediaId,
        status: Option<LibraryStatus>,
        progress: Option<u32>,
        score: Option<ScoreUpdate>,
    ) -> Result<LibraryEntry, AnilistError> {
        #[derive(Deserialize)]
        struct SaveData {
            #[serde(rename = "SaveMediaListEntry")]
            saved: Option<SavedEntryNode>,
        }
        #[derive(Deserialize)]
        struct SavedEntryNode {
            id: Option<NonZeroU32>,
            status: LibraryStatus,
            progress: Option<u32>,
            score: Option<f64>,
        }

        let vars = build_save_entry_vars(media_id, status, progress, score);
        let data: SaveData = self
            .post_graphql(SAVE_ENTRY_MUTATION, vars, Some(token))
            .await?;
        let saved = data.saved.ok_or(AnilistError::MissingData)?;

        Ok(LibraryEntry {
            id: saved.id.map(EntryId),
            status: saved.status,
            progress: saved.progress.unwrap_or(0),
            score: saved.score.and_then(ScoreRaw::from_api_raw),
        })
    }

    pub async fn delete_entry(
        &self,
        token: &str,
        entry_id: EntryId,
    ) -> Result<bool, AnilistError> {
        #[derive(Deserialize)]
        struct DeleteData {
            #[serde(rename = "DeleteMediaListEntry")]
            deleted: Option<DeletedNode>,
        }
        #[derive(Deserialize)]
        struct DeletedNode {
            deleted: bool,
        }

        let vars = serde_json::json!({ "id": entry_id.get() });
        let data: DeleteData = self
            .post_graphql(DELETE_ENTRY_MUTATION, vars, Some(token))
            .await?;
        Ok(data.deleted.is_some_and(|d| d.deleted))
    }

    async fn post_graphql_with_retry<V: Serialize + Clone, D: for<'de> Deserialize<'de>>(
        &self,
        query: &str,
        variables: V,
        token: Option<&str>,
    ) -> Result<D, AnilistError> {
        match self.post_graphql(query, variables.clone(), token).await {
            Ok(data) => Ok(data),
            Err(err) if err.is_transient() && self.remaining_backoff_secs().is_none() => {
                tokio::time::sleep(READ_RETRY_DELAY).await;
                self.post_graphql(query, variables, token).await
            }
            Err(err) => Err(err),
        }
    }

    async fn post_graphql<V: Serialize, D: for<'de> Deserialize<'de>>(
        &self,
        query: &str,
        variables: V,
        token: Option<&str>,
    ) -> Result<D, AnilistError> {
        #[derive(Serialize)]
        struct GraphQlRequest<'a, V> {
            query: &'a str,
            variables: V,
        }

        if let Some(retry_after_secs) = self.remaining_backoff_secs() {
            return Err(AnilistError::RateLimited { retry_after_secs });
        }

        let mut req = self
            .http
            .post(&self.endpoint)
            .header(ACCEPT, HeaderValue::from_static("application/json"))
            .json(&GraphQlRequest {
                query,
                variables,
            });
        if let Some(tok) = token {
            let value = HeaderValue::from_str(&format!("Bearer {tok}"))
                .map_err(|_| AnilistError::GraphQl("invalid bearer token header".into()))?;
            req = req.header(AUTHORIZATION, value);
        }

        let resp = req.send().await?;
        let status = resp.status();
        let now_unix = crate::current_unix_secs();
        let (info, retry_hint) = parse_rate_limit_headers(resp.headers(), now_unix);
        self.record_rate_limit(status, info, retry_hint);

        if status == StatusCode::UNAUTHORIZED {
            return Err(AnilistError::Unauthorized);
        }
        if status == StatusCode::TOO_MANY_REQUESTS {
            let retry_after_secs = retry_hint
                .unwrap_or(DEFAULT_RETRY_AFTER_SECS)
                .clamp(1, MAX_BACKOFF_SECS);
            return Err(AnilistError::RateLimited { retry_after_secs });
        }

        let body = resp.text().await?;
        if !status.is_success() {
            return Err(AnilistError::HttpStatus {
                status: status.as_u16(),
                body,
            });
        }

        let envelope: GraphQlEnvelope<D> = serde_json::from_str(&body)?;
        if !envelope.errors.is_empty() {
            let msg = envelope
                .errors
                .into_iter()
                .map(|e| e.message)
                .collect::<Vec<_>>()
                .join("; ");
            return Err(AnilistError::GraphQl(msg));
        }

        envelope.data.ok_or(AnilistError::MissingData)
    }

    fn remaining_backoff_secs(&self) -> Option<u64> {
        let now = Instant::now();
        let mut tracker = self.rate_limit.lock().unwrap_or_else(|e| e.into_inner());
        let until = tracker.blocked_until?;
        if now < until {
            Some((until - now).as_secs().max(1))
        } else {
            tracker.blocked_until = None;
            None
        }
    }

    fn record_rate_limit(
        &self,
        status: StatusCode,
        info: RateLimitInfo,
        retry_hint: Option<u64>,
    ) {
        let now = Instant::now();
        let mut tracker = self.rate_limit.lock().unwrap_or_else(|e| e.into_inner());
        if info.limit.is_some() {
            tracker.info.limit = info.limit;
        }
        if info.remaining.is_some() {
            tracker.info.remaining = info.remaining;
        }
        if info.reset_at_unix.is_some() {
            tracker.info.reset_at_unix = info.reset_at_unix;
        }

        if status == StatusCode::TOO_MANY_REQUESTS {
            let wait = retry_hint
                .unwrap_or(DEFAULT_RETRY_AFTER_SECS)
                .clamp(1, MAX_BACKOFF_SECS);
            tracker.blocked_until = Some(now + Duration::from_secs(wait));
        } else if info.remaining == Some(0) {
            if let Some(wait) = retry_hint {
                let capped = wait.clamp(1, MAX_BACKOFF_SECS);
                tracker.blocked_until = Some(now + Duration::from_secs(capped));
            }
        }
    }
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct SaveEntryVars {
    media_id: u32,
    #[serde(skip_serializing_if = "Option::is_none")]
    status: Option<LibraryStatus>,
    #[serde(skip_serializing_if = "Option::is_none")]
    progress: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    score_raw: Option<u8>,
}

fn build_save_entry_vars(
    media_id: MediaId,
    status: Option<LibraryStatus>,
    progress: Option<u32>,
    score: Option<ScoreUpdate>,
) -> SaveEntryVars {
    SaveEntryVars {
        media_id: media_id.get(),
        status,
        progress,
        score_raw: score.map(ScoreUpdate::to_score_raw),
    }
}

fn parse_header_u32(headers: &HeaderMap, name: &str) -> Option<u32> {
    headers
        .get(name)
        .and_then(|v| v.to_str().ok())
        .and_then(|s| s.trim().parse::<u32>().ok())
}

fn parse_header_u64(headers: &HeaderMap, name: &str) -> Option<u64> {
    headers
        .get(name)
        .and_then(|v| v.to_str().ok())
        .and_then(|s| s.trim().parse::<u64>().ok())
}

fn parse_rate_limit_headers(headers: &HeaderMap, now_unix: u64) -> (RateLimitInfo, Option<u64>) {
    let limit = parse_header_u32(headers, "x-ratelimit-limit");
    let remaining = parse_header_u32(headers, "x-ratelimit-remaining");
    let reset_at_unix = parse_header_u64(headers, "x-ratelimit-reset");
    let retry_after = parse_header_u64(headers, "retry-after");

    let retry_hint = retry_after.or_else(|| {
        reset_at_unix.and_then(|reset| reset.checked_sub(now_unix)).filter(|&s| s > 0)
    });

    (
        RateLimitInfo {
            limit,
            remaining,
            reset_at_unix,
        },
        retry_hint,
    )
}

fn deserialize_null_default<'de, D, T>(deserializer: D) -> Result<T, D::Error>
where
    T: Default + Deserialize<'de>,
    D: serde::Deserializer<'de>,
{
    Option::<T>::deserialize(deserializer).map(Option::unwrap_or_default)
}

#[derive(Debug, Deserialize)]
struct GraphQlEnvelope<T> {
    data: Option<T>,
    #[serde(default, deserialize_with = "deserialize_null_default")]
    errors: Vec<GraphQlErrorItem>,
}

#[derive(Debug, Deserialize)]
struct GraphQlErrorItem {
    message: String,
}

#[derive(Debug, Deserialize)]
struct FullLibraryData {
    #[serde(rename = "MediaListCollection")]
    collection: Option<MediaListCollectionNode>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct MediaListCollectionNode {
    has_next_chunk: Option<bool>,
    user: Option<CollectionUserNode>,
    #[serde(default, deserialize_with = "deserialize_null_default")]
    lists: Vec<MediaListGroupNode>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct CollectionUserNode {
    media_list_options: Option<MediaListOptionsNode>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct MediaListOptionsNode {
    score_format: Option<ScoreFormat>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct MediaListGroupNode {
    status: Option<LibraryStatus>,
    is_custom_list: Option<bool>,
    #[serde(default, deserialize_with = "deserialize_null_default")]
    entries: Vec<MediaListEntryNode>,
}

#[derive(Debug, Deserialize)]
struct MediaListEntryNode {
    id: Option<NonZeroU32>,
    status: Option<LibraryStatus>,
    progress: Option<u32>,
    score: Option<f64>,
    media: Option<MediaNode>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct MediaNode {
    id: NonZeroU32,
    title: MediaTitleNode,
    site_url: Option<String>,
    episodes: Option<u32>,
    description: Option<String>,
    status: Option<String>,
    average_score: Option<u8>,
    #[serde(default, deserialize_with = "deserialize_null_default")]
    genres: Vec<String>,
    cover_image: Option<CoverImageNode>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct MediaTitleNode {
    user_preferred: Option<String>,
    english: Option<String>,
    romaji: Option<String>,
}

#[derive(Debug, Deserialize)]
struct CoverImageNode {
    large: Option<String>,
    medium: Option<String>,
}

fn non_empty_trimmed(opt: Option<String>) -> Option<String> {
    opt.and_then(|s| {
        let trimmed = s.trim();
        if trimmed.is_empty() {
            None
        } else if trimmed.len() == s.len() {
            Some(s)
        } else {
            Some(trimmed.to_owned())
        }
    })
}

fn resolve_titles(raw: MediaTitleNode) -> (String, Option<String>) {
    let preferred = non_empty_trimmed(raw.user_preferred);
    let english = non_empty_trimmed(raw.english);
    let romaji = non_empty_trimmed(raw.romaji);

    let title = preferred
        .or_else(|| english.clone())
        .or_else(|| romaji.clone())
        .unwrap_or_else(|| "Unknown Title".to_owned());

    let alt_title = [english, romaji]
        .into_iter()
        .flatten()
        .find(|candidate| candidate != &title);

    (title, alt_title)
}

fn merge_library_chunk(
    data: FullLibraryData,
    grouped: &mut [Vec<AnimeItem>; 6],
    seen_ids: &mut HashSet<MediaId>,
    score_format: &mut Option<ScoreFormat>,
) -> bool {
    let Some(collection) = data.collection else {
        return false;
    };
    let has_next_chunk = collection.has_next_chunk.unwrap_or(false);
    if score_format.is_none() {
        *score_format = collection
            .user
            .and_then(|u| u.media_list_options)
            .and_then(|opts| opts.score_format);
    }

    for group in collection.lists {
        if group.is_custom_list == Some(true) {
            continue;
        }
        for raw_entry in group.entries {
            let Some(status) = raw_entry.status.or(group.status) else {
                continue;
            };
            let Some(media) = raw_entry.media else {
                continue;
            };
            let media_id = MediaId(media.id);
            if !seen_ids.insert(media_id) {
                continue;
            }

            let (title, alt_title) = resolve_titles(media.title);
            let cover_url = media.cover_image.and_then(|c| c.large.or(c.medium));

            let item = AnimeItem::new(
                media_id,
                title,
                alt_title,
                media.episodes,
                media.status.unwrap_or_else(|| "UNKNOWN".to_owned()),
                media.average_score,
                media.genres,
                clean_description(media.description.as_deref()),
                cover_url,
                media.site_url,
                LibraryEntry {
                    id: raw_entry.id.map(EntryId),
                    status,
                    progress: raw_entry.progress.unwrap_or(0),
                    score: raw_entry.score.and_then(ScoreRaw::from_api_raw),
                },
            );

            grouped[status.index()].push(item);
        }
    }

    has_next_chunk
}

#[cfg(test)]
fn build_library_collection(data: FullLibraryData) -> LibraryCollection {
    let mut grouped: [Vec<AnimeItem>; 6] = Default::default();
    let mut seen_ids = HashSet::new();
    let mut score_format = None;
    merge_library_chunk(data, &mut grouped, &mut seen_ids, &mut score_format);
    LibraryCollection::from_array(grouped, score_format)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_full_library_response_with_null_fields_and_user_preferred() {
        let fixture = r#"{
          "data": {
            "MediaListCollection": {
              "hasNextChunk": false,
              "user": {
                "mediaListOptions": {
                  "scoreFormat": "POINT_10_DECIMAL"
                }
              },
              "lists": [
                {
                  "status": "CURRENT",
                  "isCustomList": false,
                  "entries": [
                    {
                      "id": 9001,
                      "status": "CURRENT",
                      "progress": 12,
                      "score": 98,
                      "media": {
                        "id": 154587,
                        "title": {
                          "userPreferred": "Sousou no Frieren",
                          "english": "Frieren: Beyond Journey's End",
                          "romaji": "Sousou no Frieren"
                        },
                        "siteUrl": "https://anilist.co/anime/154587",
                        "episodes": 28,
                        "description": "After the party of heroes defeated the Demon King.<br><i>New journey.</i>",
                        "status": "FINISHED",
                        "averageScore": 91,
                        "genres": ["Adventure", "Drama", "Fantasy"],
                        "coverImage": { "large": "https://example.com/large.jpg", "medium": "https://example.com/med.jpg" }
                      }
                    },
                    {
                      "id": 9002,
                      "status": "CURRENT",
                      "progress": 1,
                      "score": 0,
                      "media": {
                        "id": 21,
                        "title": { "userPreferred": null, "english": "One Piece", "romaji": "ONE PIECE" },
                        "siteUrl": null,
                        "episodes": null,
                        "description": null,
                        "status": "RELEASING",
                        "averageScore": null,
                        "genres": ["Action"],
                        "coverImage": null
                      }
                    }
                  ]
                }
              ]
            }
          }
        }"#;

        let envelope: GraphQlEnvelope<FullLibraryData> = serde_json::from_str(fixture).unwrap();
        let lib = build_library_collection(envelope.data.unwrap());

        assert_eq!(lib.score_format, Some(ScoreFormat::Point10Decimal));
        assert_eq!(lib.count_for(LibraryStatus::Watching), 2);
        let watching = lib.items_for(LibraryStatus::Watching);

        assert_eq!(watching[0].title, "Sousou no Frieren");
        assert_eq!(
            watching[0].alt_title.as_deref(),
            Some("Frieren: Beyond Journey's End")
        );
        assert!(watching[0].matches_filter("beyond journey"));
        assert_eq!(watching[0].entry.score, ScoreRaw::new(98));
        assert_eq!(
            watching[0].description_text,
            "After the party of heroes defeated the Demon King.\nNew journey."
        );

        assert_eq!(watching[1].title, "One Piece");
        assert_eq!(watching[1].alt_title.as_deref(), Some("ONE PIECE"));
        assert_eq!(watching[1].episodes, None);
        assert_eq!(watching[1].entry.score, None);
        assert_eq!(watching[1].description_text, "No description available.");
    }

    #[test]
    fn skips_custom_lists_and_deduplicates_across_chunks() {
        let chunk1 = r#"{
          "data": {
            "MediaListCollection": {
              "hasNextChunk": true,
              "lists": [
                {
                  "status": "COMPLETED",
                  "isCustomList": false,
                  "entries": [
                    {
                      "id": 100,
                      "status": "COMPLETED",
                      "progress": 24,
                      "score": 3,
                      "media": {
                        "id": 9253,
                        "title": { "userPreferred": "Steins;Gate", "english": "Steins;Gate", "romaji": "Steins;Gate" },
                        "siteUrl": null,
                        "episodes": 24,
                        "description": null,
                        "status": "FINISHED",
                        "averageScore": 90,
                        "genres": ["Sci-Fi"],
                        "coverImage": null
                      }
                    }
                  ]
                },
                {
                  "status": null,
                  "isCustomList": true,
                  "entries": [
                    {
                      "id": 100,
                      "status": "COMPLETED",
                      "progress": 24,
                      "score": 3,
                      "media": {
                        "id": 9253,
                        "title": { "userPreferred": "Steins;Gate", "english": null, "romaji": null },
                        "siteUrl": null,
                        "episodes": 24,
                        "description": null,
                        "status": "FINISHED",
                        "averageScore": 90,
                        "genres": [],
                        "coverImage": null
                      }
                    }
                  ]
                }
              ]
            }
          }
        }"#;

        let chunk2 = r#"{
          "data": {
            "MediaListCollection": {
              "hasNextChunk": false,
              "lists": [
                {
                  "status": "COMPLETED",
                  "isCustomList": false,
                  "entries": [
                    {
                      "id": 100,
                      "status": "COMPLETED",
                      "progress": 24,
                      "score": 3,
                      "media": {
                        "id": 9253,
                        "title": { "userPreferred": "Steins;Gate", "english": null, "romaji": null },
                        "siteUrl": null,
                        "episodes": 24,
                        "description": null,
                        "status": "FINISHED",
                        "averageScore": 90,
                        "genres": [],
                        "coverImage": null
                      }
                    },
                    {
                      "id": 101,
                      "status": "PLANNING",
                      "progress": 0,
                      "score": 0,
                      "media": {
                        "id": 1,
                        "title": { "userPreferred": "Cowboy Bebop", "english": "Cowboy Bebop", "romaji": "Cowboy Bebop" },
                        "siteUrl": null,
                        "episodes": 26,
                        "description": null,
                        "status": "FINISHED",
                        "averageScore": 86,
                        "genres": ["Action"],
                        "coverImage": null
                      }
                    }
                  ]
                }
              ]
            }
          }
        }"#;

        let mut grouped: [Vec<AnimeItem>; 6] = Default::default();
        let mut seen = HashSet::new();
        let mut score_format = None;

        let env1: GraphQlEnvelope<FullLibraryData> = serde_json::from_str(chunk1).unwrap();
        assert!(merge_library_chunk(
            env1.data.unwrap(),
            &mut grouped,
            &mut seen,
            &mut score_format
        ));

        let env2: GraphQlEnvelope<FullLibraryData> = serde_json::from_str(chunk2).unwrap();
        assert!(!merge_library_chunk(
            env2.data.unwrap(),
            &mut grouped,
            &mut seen,
            &mut score_format
        ));

        let lib = LibraryCollection::from_array(grouped, score_format);
        assert_eq!(lib.score_format, None);
        assert_eq!(lib.total_count(), 2);
        assert_eq!(lib.count_for(LibraryStatus::Completed), 1);
        assert_eq!(lib.count_for(LibraryStatus::Planning), 1);
    }

    #[test]
    fn save_entry_variables_serialize_score_raw_correctly() {
        let media_id = MediaId(NonZeroU32::new(42).unwrap());

        let set_happy = serde_json::to_value(build_save_entry_vars(
            media_id,
            Some(LibraryStatus::Watching),
            Some(5),
            Some(ScoreUpdate::Set(ScoreRaw::new(85).unwrap())),
        ))
        .unwrap();
        assert_eq!(set_happy["mediaId"], 42);
        assert_eq!(set_happy["status"], "CURRENT");
        assert_eq!(set_happy["progress"], 5);
        assert_eq!(set_happy["scoreRaw"], 85);

        let cleared = serde_json::to_value(build_save_entry_vars(
            media_id,
            None,
            None,
            Some(ScoreUpdate::Clear),
        ))
        .unwrap();
        assert_eq!(cleared["scoreRaw"], 0);
        assert!(cleared.get("status").is_none());
        assert!(cleared.get("progress").is_none());

        let omitted = serde_json::to_value(build_save_entry_vars(
            media_id,
            None,
            Some(3),
            None,
        ))
        .unwrap();
        assert!(omitted.get("scoreRaw").is_none());
    }

    #[test]
    fn parses_anilist_rate_limit_headers_and_retry_after() {
        let mut headers = HeaderMap::new();
        headers.insert("x-ratelimit-limit", HeaderValue::from_static("30"));
        headers.insert("x-ratelimit-remaining", HeaderValue::from_static("0"));
        headers.insert("x-ratelimit-reset", HeaderValue::from_static("1700000045"));
        headers.insert("retry-after", HeaderValue::from_static("42"));

        let (info, retry) = parse_rate_limit_headers(&headers, 1_700_000_000);
        assert_eq!(info.limit, Some(30));
        assert_eq!(info.remaining, Some(0));
        assert_eq!(info.reset_at_unix, Some(1_700_000_045));
        assert_eq!(retry, Some(42));

        headers.remove("retry-after");
        let (_, computed_retry) = parse_rate_limit_headers(&headers, 1_700_000_000);
        assert_eq!(computed_retry, Some(45));
    }

    #[test]
    fn deserializes_explicit_null_arrays_in_graphql_payload() {
        let fixture = r#"{
          "data": {
            "MediaListCollection": {
              "hasNextChunk": false,
              "lists": [
                {
                  "status": "CURRENT",
                  "isCustomList": false,
                  "entries": [
                    {
                      "id": 77,
                      "status": "CURRENT",
                      "progress": 0,
                      "score": 0,
                      "media": {
                        "id": 555,
                        "title": { "userPreferred": "Nullable Show", "english": null, "romaji": null },
                        "siteUrl": null,
                        "episodes": null,
                        "description": null,
                        "status": null,
                        "averageScore": null,
                        "genres": null,
                        "coverImage": null
                      }
                    }
                  ]
                },
                {
                  "status": "PLANNING",
                  "isCustomList": false,
                  "entries": null
                }
              ]
            }
          },
          "errors": null
        }"#;

        let envelope: GraphQlEnvelope<FullLibraryData> = serde_json::from_str(fixture).unwrap();
        assert!(envelope.errors.is_empty());
        let lib = build_library_collection(envelope.data.unwrap());
        assert_eq!(lib.count_for(LibraryStatus::Watching), 1);
        assert!(lib.items_for(LibraryStatus::Watching)[0].genres.is_empty());
    }

    #[test]
    fn transient_error_classification_only_matches_gateway_and_transport_errors() {
        for status in [502, 503, 504] {
            let err = AnilistError::HttpStatus {
                status,
                body: String::new(),
            };
            assert!(err.is_transient());
        }

        for status in [400, 404, 500] {
            let err = AnilistError::HttpStatus {
                status,
                body: String::new(),
            };
            assert!(!err.is_transient());
        }

        assert!(!AnilistError::Unauthorized.is_transient());
        assert!(!AnilistError::RateLimited {
            retry_after_secs: 30
        }
        .is_transient());
        assert!(!AnilistError::GraphQl("boom".to_owned()).is_transient());
    }

    fn minimal_library_json(has_next_chunk: bool, id: u32, title: &str) -> String {
        format!(
            r#"{{"data":{{"MediaListCollection":{{"hasNextChunk":{has_next_chunk},"lists":[{{"status":"CURRENT","isCustomList":false,"entries":[{{"id":{id},"status":"CURRENT","progress":1,"score":0,"media":{{"id":{id},"title":{{"userPreferred":"{title}","english":null,"romaji":null}},"siteUrl":null,"episodes":12,"description":null,"status":"FINISHED","averageScore":80,"genres":[],"coverImage":null}}}}]}}]}}}}}}"#
        )
    }

    fn http_json_response(status_line: &str, extra_headers: &str, body: &str) -> String {
        format!(
            "HTTP/1.1 {status_line}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n{extra_headers}\r\n{body}",
            body.len()
        )
    }

    fn drain_http_request(stream: &mut std::net::TcpStream) {
        use std::io::Read;
        let _ = stream.set_read_timeout(Some(Duration::from_millis(200)));
        let mut buf = [0u8; 4096];
        let _ = stream.read(&mut buf);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn fetch_full_library_recovers_from_cold_start_connection_drop() {
        use std::io::Write;
        use std::net::TcpListener;
        use std::sync::atomic::{AtomicUsize, Ordering};

        let listener = TcpListener::bind("127.0.0.1:0").expect("bind loopback");
        let addr = listener.local_addr().expect("local addr");
        let hits = Arc::new(AtomicUsize::new(0));
        let server_hits = Arc::clone(&hits);
        let ok_body = minimal_library_json(false, 101, "Cold Start Anime");

        let server = std::thread::spawn(move || {
            for attempt in 0..2 {
                if let Ok((mut stream, _)) = listener.accept() {
                    server_hits.fetch_add(1, Ordering::SeqCst);
                    drain_http_request(&mut stream);
                    if attempt == 0 {
                        // Simulate abrupt connection drop / EOF on first launch.
                        drop(stream);
                    } else {
                        let resp = http_json_response("200 OK", "", &ok_body);
                        let _ = stream.write_all(resp.as_bytes());
                        let _ = stream.flush();
                    }
                }
            }
        });

        let client = AnilistClient::with_endpoint(format!("http://{addr}/graphql")).unwrap();
        let lib = client
            .fetch_full_library("alice", None)
            .await
            .expect("should recover on retry after connection drop");

        assert_eq!(hits.load(Ordering::SeqCst), 2);
        assert_eq!(lib.total_count(), 1);
        assert_eq!(lib.items_for(LibraryStatus::Watching)[0].title, "Cold Start Anime");
        let _ = server.join();
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn fetch_full_library_recovers_from_transient_503() {
        use std::io::Write;
        use std::net::TcpListener;
        use std::sync::atomic::{AtomicUsize, Ordering};

        let listener = TcpListener::bind("127.0.0.1:0").expect("bind loopback");
        let addr = listener.local_addr().expect("local addr");
        let hits = Arc::new(AtomicUsize::new(0));
        let server_hits = Arc::clone(&hits);
        let ok_body = minimal_library_json(false, 202, "Gateway Recovery");

        let server = std::thread::spawn(move || {
            for attempt in 0..2 {
                if let Ok((mut stream, _)) = listener.accept() {
                    server_hits.fetch_add(1, Ordering::SeqCst);
                    drain_http_request(&mut stream);
                    let resp = if attempt == 0 {
                        http_json_response("503 Service Unavailable", "", "temporarily unavailable")
                    } else {
                        http_json_response("200 OK", "", &ok_body)
                    };
                    let _ = stream.write_all(resp.as_bytes());
                    let _ = stream.flush();
                }
            }
        });

        let client = AnilistClient::with_endpoint(format!("http://{addr}/graphql")).unwrap();
        let lib = client
            .fetch_full_library("alice", None)
            .await
            .expect("should recover on retry after 503");

        assert_eq!(hits.load(Ordering::SeqCst), 2);
        assert_eq!(lib.total_count(), 1);
        assert_eq!(lib.items_for(LibraryStatus::Watching)[0].title, "Gateway Recovery");
        let _ = server.join();
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn rate_limited_response_is_not_retried_and_sets_local_backoff() {
        use std::io::Write;
        use std::net::TcpListener;
        use std::sync::atomic::{AtomicUsize, Ordering};

        let listener = TcpListener::bind("127.0.0.1:0").expect("bind loopback");
        let addr = listener.local_addr().expect("local addr");
        let hits = Arc::new(AtomicUsize::new(0));
        let server_hits = Arc::clone(&hits);

        let server = std::thread::spawn(move || {
            if let Ok((mut stream, _)) = listener.accept() {
                server_hits.fetch_add(1, Ordering::SeqCst);
                drain_http_request(&mut stream);
                let resp = http_json_response(
                    "429 Too Many Requests",
                    "Retry-After: 45\r\nX-RateLimit-Limit: 30\r\nX-RateLimit-Remaining: 0\r\n",
                    r#"{"data":null,"errors":[{"message":"Too Many Requests."}]}"#,
                );
                let _ = stream.write_all(resp.as_bytes());
                let _ = stream.flush();
            }
        });

        let client = AnilistClient::with_endpoint(format!("http://{addr}/graphql")).unwrap();
        let err = client
            .fetch_full_library("alice", None)
            .await
            .expect_err("429 should not be retried");

        assert!(err.is_rate_limited());
        assert_eq!(hits.load(Ordering::SeqCst), 1);
        assert_eq!(client.rate_limit_info().limit, Some(30));
        assert_eq!(client.rate_limit_info().remaining, Some(0));

        // Subsequent request is blocked locally without hitting the network.
        let blocked = client
            .fetch_full_library("alice", None)
            .await
            .expect_err("backoff should block immediately");
        assert!(blocked.is_rate_limited());
        assert_eq!(hits.load(Ordering::SeqCst), 1);
        let _ = server.join();
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn multi_chunk_fetch_paces_requests_and_merges_chunks() {
        use std::io::Write;
        use std::net::TcpListener;

        let listener = TcpListener::bind("127.0.0.1:0").expect("bind loopback");
        let addr = listener.local_addr().expect("local addr");
        let chunk1 = minimal_library_json(true, 1, "Chunk One");
        let chunk2 = minimal_library_json(false, 2, "Chunk Two");

        let server = std::thread::spawn(move || {
            for body in [chunk1, chunk2] {
                if let Ok((mut stream, _)) = listener.accept() {
                    drain_http_request(&mut stream);
                    let resp = http_json_response("200 OK", "", &body);
                    let _ = stream.write_all(resp.as_bytes());
                    let _ = stream.flush();
                }
            }
        });

        let client = AnilistClient::with_endpoint(format!("http://{addr}/graphql")).unwrap();
        let started = Instant::now();
        let lib = client
            .fetch_full_library("alice", None)
            .await
            .expect("multi-chunk fetch succeeds");
        let elapsed = started.elapsed();

        assert_eq!(lib.total_count(), 2);
        assert!(
            elapsed >= CHUNK_PACE_DELAY,
            "expected >= {:?} pacing between chunks, got {:?}",
            CHUNK_PACE_DELAY,
            elapsed
        );
        let _ = server.join();
    }
}
