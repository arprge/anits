//! Application state machine, optimistic mutation reconciliation, and async event loop.

use std::collections::HashMap;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use crossterm::event::{
    Event as CrosstermEvent, EventStream, KeyCode, KeyEvent, KeyEventKind, KeyModifiers,
};
use futures::StreamExt;
use ratatui::Terminal;
use ratatui::backend::Backend;
use ratatui::layout::Rect;
use ratatui_image::picker::Picker;
use tokio::sync::mpsc;

use crate::api::{AnilistClient, AnilistError, RateLimitInfo};
use crate::cache::{
    CoverCache, CoverLoaded, CoverLoader, load_library_snapshot, remove_library_snapshot,
    save_library_snapshot,
};
use crate::config::{
    ANILIST_OAUTH_PIN, AppConfig, AuthSession, ScoreFormatPreference, ThemePalette, cache_dir,
    oauth_url, save_app_config, save_auth_session,
};
use crate::domain::{
    AnimeItem, EntryId, LibraryCollection, LibraryEntry, LibraryStatus, MediaId, Point3Score,
    ScoreFormat, ScoreRaw, ScoreUpdate, SortDirection, SortField,
};

const PROGRESS_DEBOUNCE: Duration = Duration::from_millis(340);
const STARTUP_SNAPSHOT_FRESHNESS_SECS: u64 = 10;
const TICK_INTERVAL: Duration = Duration::from_millis(120);
const QUIT_FLUSH_TIMEOUT: Duration = Duration::from_secs(3);
const QUIT_FLUSH_PACE_DELAY: Duration = Duration::from_millis(150);
const TOAST_SHORT: Duration = Duration::from_secs(3);
const TOAST_LONG: Duration = Duration::from_secs(5);
const PREFETCH_RADIUS: usize = 5;
const DEFAULT_COVER_AREA: Rect = Rect::new(0, 0, 28, 16);

/// Focused pane on the main library screen.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FocusPane {
    Sidebar,
    Table,
    Details,
}

impl FocusPane {
    /// Advances focus clockwise (`Sidebar -> Table -> Details -> Sidebar`).
    #[must_use]
    pub const fn next(self) -> Self {
        match self {
            Self::Sidebar => Self::Table,
            Self::Table => Self::Details,
            Self::Details => Self::Sidebar,
        }
    }

    /// Moves focus counter-clockwise (`Sidebar -> Details -> Table -> Sidebar`).
    #[must_use]
    pub const fn prev(self) -> Self {
        match self {
            Self::Sidebar => Self::Details,
            Self::Table => Self::Sidebar,
            Self::Details => Self::Table,
        }
    }
}

/// Items displayed in the left navigation sidebar.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SidebarItem {
    Status(LibraryStatus),
    LogOut,
}

impl SidebarItem {
    /// Canonical display order for all sidebar entries.
    pub const ALL: [Self; 7] = [
        Self::Status(LibraryStatus::Watching),
        Self::Status(LibraryStatus::Planning),
        Self::Status(LibraryStatus::Completed),
        Self::Status(LibraryStatus::Dropped),
        Self::Status(LibraryStatus::Paused),
        Self::Status(LibraryStatus::Repeating),
        Self::LogOut,
    ];

    /// Human-readable label shown in the sidebar.
    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::Status(status) => status.label(),
            Self::LogOut => "Log Out",
        }
    }
}

/// Scope for local instant `/` filtering.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FilterScope {
    CurrentList,
    AllLists,
}

impl FilterScope {
    /// Toggles between `CurrentList` and `AllLists`.
    #[must_use]
    pub const fn toggle(self) -> Self {
        match self {
            Self::CurrentList => Self::AllLists,
            Self::AllLists => Self::CurrentList,
        }
    }
}

/// Active modal overlay rendered over the library screen.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ActiveModal {
    None,
    StatusPicker { selected: usize },
    RatingPicker { selected: usize },
    SortPicker { selected: usize },
    Help,
}

/// Interactive controls on the authentication screen.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AuthField {
    ClientId,
    OpenUrl,
    Token,
    Submit,
}

impl AuthField {
    /// Cycles forward through the authentication form fields.
    #[must_use]
    pub const fn next(self) -> Self {
        match self {
            Self::ClientId => Self::OpenUrl,
            Self::OpenUrl => Self::Token,
            Self::Token => Self::Submit,
            Self::Submit => Self::ClientId,
        }
    }

    /// Cycles backward through the authentication form fields.
    #[must_use]
    pub const fn prev(self) -> Self {
        match self {
            Self::ClientId => Self::Submit,
            Self::OpenUrl => Self::ClientId,
            Self::Token => Self::OpenUrl,
            Self::Submit => Self::Token,
        }
    }
}

/// State for the OAuth setup and token entry screen.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AuthScreenState {
    pub client_id: String,
    pub token: String,
    pub focused: AuthField,
    pub submitting: bool,
    pub error: Option<String>,
}

impl AuthScreenState {
    /// Builds the AniList implicit-grant authorization URL when `client_id` is non-empty.
    #[must_use]
    pub fn oauth_url_preview(&self) -> Option<String> {
        oauth_url(&self.client_id).ok()
    }
}

/// Active application screen.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Screen {
    Auth(AuthScreenState),
    Library,
}

/// Severity level for transient toast notifications.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ToastLevel {
    Info,
    Success,
    Warning,
    Error,
}

/// Ephemeral toast notification displayed in the UI.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Toast {
    pub message: String,
    pub level: ToastLevel,
    pub expires_at: Instant,
}

/// Snapshot of pre-mutation state used to roll back failed optimistic updates.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PendingRollback {
    Progress {
        previous_progress: u32,
        seq: u64,
    },
    Status {
        previous_status: LibraryStatus,
        previous_index: usize,
        previous_entry: LibraryEntry,
        seq: u64,
    },
    Score {
        previous_score: Option<ScoreRaw>,
        previous_progress: Option<u32>,
        seq: u64,
    },
    Delete {
        previous_status: LibraryStatus,
        previous_index: usize,
        item: AnimeItem,
        seq: u64,
    },
}

impl PendingRollback {
    /// Returns the monotonic sequence number associated with this optimistic mutation.
    #[must_use]
    pub const fn seq(&self) -> u64 {
        match self {
            Self::Progress { seq, .. }
            | Self::Status { seq, .. }
            | Self::Score { seq, .. }
            | Self::Delete { seq, .. } => *seq,
        }
    }
}

/// Side-effect commands emitted by [`AppState`] for execution by [`run_app`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AppCommand {
    Authenticate {
        client_id: String,
        token: String,
    },
    FetchLibrary,
    ScheduleProgress {
        media_id: MediaId,
        progress: u32,
        rollback: PendingRollback,
    },
    SaveStatus {
        media_id: MediaId,
        status: LibraryStatus,
        rollback: PendingRollback,
    },
    SaveScore {
        media_id: MediaId,
        score: ScoreUpdate,
        rollback: PendingRollback,
    },
    DeleteEntry {
        media_id: MediaId,
        entry_id: EntryId,
        rollback: PendingRollback,
    },
    EnsureCovers(Vec<(MediaId, String)>),
    SaveSnapshot,
    RemoveSnapshot(String),
    SaveAuthSession(AuthSession),
    SaveAppConfig(AppConfig),
    OpenUrl(String),
    Quit,
}

/// Asynchronous and timer events delivered into [`AppState::apply_event`].
pub enum AppEvent {
    AuthSucceeded {
        client_id: String,
        token: String,
        username: String,
        rate_limit: RateLimitInfo,
    },
    AuthFailed {
        error: String,
        rate_limit: RateLimitInfo,
    },
    SnapshotLoaded {
        epoch: u64,
        library: LibraryCollection,
    },
    LibraryLoaded {
        epoch: u64,
        library: LibraryCollection,
        rate_limit: RateLimitInfo,
    },
    LibraryLoadFailed {
        epoch: u64,
        error: String,
        is_auth_error: bool,
        rate_limit: RateLimitInfo,
    },
    EntrySaved {
        epoch: u64,
        media_id: MediaId,
        entry: LibraryEntry,
        rollback: PendingRollback,
        rate_limit: RateLimitInfo,
    },
    EntryDeleted {
        epoch: u64,
        media_id: MediaId,
        rollback: PendingRollback,
        rate_limit: RateLimitInfo,
    },
    MutationFailed {
        epoch: u64,
        media_id: MediaId,
        rollback: PendingRollback,
        error: String,
        is_auth_error: bool,
        retry_after_secs: Option<u64>,
        rate_limit: RateLimitInfo,
    },
    Notice {
        message: String,
        level: ToastLevel,
    },
    CoverLoaded(CoverLoaded),
    Tick,
}

/// Complete UI and domain state for `anits`.
pub struct AppState {
    pub screen: Screen,
    pub palette: ThemePalette,
    pub username: Option<String>,
    pub library: LibraryCollection,
    pub active_status: LibraryStatus,
    pub focus: FocusPane,
    pub sidebar_index: usize,
    pub table_selected: usize,
    pub table_offset: usize,
    pub filter_query: String,
    pub filter_active: bool,
    pub filter_scope: FilterScope,
    pub sort_field: SortField,
    pub sort_direction: SortDirection,
    pub description_scroll: u16,
    pub modal: ActiveModal,
    pub score_format_pref: ScoreFormatPreference,
    pub api_score_format: Option<ScoreFormat>,
    pub rating_input: String,
    pub rating_input_error: Option<String>,
    pub syncing: bool,
    pub sync_tick: usize,
    pub rate_limit: RateLimitInfo,
    pub toast: Option<Toast>,
    pub last_cover_area: Rect,
    pub pending_reencode: Option<(MediaId, Rect)>,
    pub cover_cache: CoverCache,
    app_config: AppConfig,
    auth_session: AuthSession,
    visible_rows: Vec<(LibraryStatus, usize)>,
    session_epoch: u64,
    next_mutation_seq: u64,
    active_mutations: HashMap<MediaId, u64>,
    progress_rollback_base: HashMap<MediaId, u32>,
    rating_pristine: bool,
}

impl AppState {
    /// Creates a new [`AppState`] initialized from theme palette and persisted auth session.
    #[must_use]
    pub fn new(palette: ThemePalette, auth: AuthSession) -> Self {
        Self::new_with_config(palette, AppConfig::default(), auth)
    }

    /// Creates a new [`AppState`] initialized from [`AppConfig`] and persisted auth session.
    #[must_use]
    pub fn from_config(app_config: AppConfig, auth: AuthSession) -> Self {
        let palette = ThemePalette::from_config(&app_config);
        Self::new_with_config(palette, app_config, auth)
    }

    /// Creates a new [`AppState`] with explicit palette, app config, and auth session.
    #[must_use]
    pub fn new_with_config(palette: ThemePalette, app_config: AppConfig, auth: AuthSession) -> Self {
        let screen = if auth.is_authenticated() {
            Screen::Library
        } else {
            Screen::Auth(AuthScreenState {
                client_id: auth.resolve_client_id().unwrap_or_default(),
                token: auth.access_token.clone().unwrap_or_default(),
                focused: AuthField::ClientId,
                submitting: false,
                error: None,
            })
        };

        let username = if auth.is_authenticated() {
            auth.username.clone()
        } else {
            None
        };
        let score_format_pref = app_config.score_format;
        let sort_field = app_config.sort_by;
        let sort_direction = app_config.sort_order;

        Self {
            screen,
            palette,
            username,
            library: LibraryCollection::default(),
            active_status: LibraryStatus::Watching,
            focus: FocusPane::Table,
            sidebar_index: 0,
            table_selected: 0,
            table_offset: 0,
            filter_query: String::new(),
            filter_active: false,
            filter_scope: FilterScope::CurrentList,
            sort_field,
            sort_direction,
            description_scroll: 0,
            modal: ActiveModal::None,
            score_format_pref,
            api_score_format: None,
            rating_input: String::new(),
            rating_input_error: None,
            syncing: false,
            sync_tick: 0,
            rate_limit: RateLimitInfo::default(),
            toast: None,
            last_cover_area: Rect::default(),
            pending_reencode: None,
            cover_cache: CoverCache::default(),
            app_config,
            auth_session: auth,
            visible_rows: Vec::new(),
            session_epoch: 0,
            next_mutation_seq: 0,
            active_mutations: HashMap::new(),
            progress_rollback_base: HashMap::new(),
            rating_pristine: false,
        }
    }

    /// Returns the active rating format resolved from config override, AniList setting, or `9.8/10`.
    #[must_use]
    pub fn effective_score_format(&self) -> ScoreFormat {
        self.score_format_pref.resolve(self.api_score_format)
    }

    /// Returns the currently configured theme key.
    #[must_use]
    pub fn theme_name(&self) -> &str {
        &self.app_config.theme
    }

    /// Returns the monotonic session epoch used to fence stale async responses.
    #[must_use]
    pub const fn session_epoch(&self) -> u64 {
        self.session_epoch
    }

    /// Returns borrowed references to all currently visible rows with zero string allocation.
    #[must_use]
    pub fn visible_items(&self) -> Vec<&AnimeItem> {
        let mut items = Vec::with_capacity(self.visible_rows.len());
        for &(status, idx) in &self.visible_rows {
            if let Some(item) = self.library.items_for(status).get(idx) {
                items.push(item);
            }
        }
        items
    }

    /// Returns the number of rows currently visible in the anime table.
    #[must_use]
    pub fn visible_len(&self) -> usize {
        self.visible_rows.len()
    }

    /// Returns a borrowed reference to the currently selected anime, if any.
    #[must_use]
    pub fn selected_item(&self) -> Option<&AnimeItem> {
        let &(status, idx) = self.visible_rows.get(self.table_selected)?;
        self.library.items_for(status).get(idx)
    }

    /// Returns the currently active (non-expired) toast notification.
    #[must_use]
    pub fn active_toast(&self) -> Option<&Toast> {
        let now = Instant::now();
        self.toast.as_ref().filter(|t| now < t.expires_at)
    }

    /// Returns the number of selectable options in the active modal.
    #[must_use]
    pub fn active_modal_options_count(&self) -> usize {
        match self.modal {
            ActiveModal::StatusPicker { .. } => LibraryStatus::ALL.len() + 1,
            ActiveModal::RatingPicker { .. } => match self.effective_score_format() {
                ScoreFormat::Point3 => Point3Score::ALL.len() + 1,
                ScoreFormat::Point5 => 6,
                _ => 0,
            },
            ActiveModal::SortPicker { .. } => SortField::ALL.len(),
            ActiveModal::Help | ActiveModal::None => 0,
        }
    }

    /// Recomputes cached `(LibraryStatus, usize)` row indices and clamps `table_selected`.
    pub fn rebuild_visible_rows(&mut self) {
        self.visible_rows.clear();
        let trimmed = self.filter_query.trim();
        let needle_lower = if trimmed.is_empty() {
            None
        } else {
            Some(trimmed.to_lowercase())
        };

        let statuses: &[LibraryStatus] = match self.filter_scope {
            FilterScope::CurrentList => std::slice::from_ref(&self.active_status),
            FilterScope::AllLists => &LibraryStatus::ALL,
        };

        for &status in statuses {
            for (idx, item) in self.library.items_for(status).iter().enumerate() {
                let matches = match needle_lower.as_deref() {
                    Some(needle) => item.matches_normalized(needle),
                    None => true,
                };
                if matches {
                    self.visible_rows.push((status, idx));
                }
            }
        }

        match self.sort_field {
            SortField::Updated => {
                if self.sort_direction == SortDirection::Asc {
                    self.visible_rows.reverse();
                }
            }
            field => {
                let direction = self.sort_direction;
                let library = &self.library;
                self.visible_rows.sort_by(|&(sa, ia), &(sb, ib)| {
                    match (library.items_for(sa).get(ia), library.items_for(sb).get(ib)) {
                        (Some(a), Some(b)) => field.compare_items(a, b, direction),
                        _ => sa.index().cmp(&sb.index()).then_with(|| ia.cmp(&ib)),
                    }
                });
            }
        }

        if self.visible_rows.is_empty() {
            self.table_selected = 0;
            self.table_offset = 0;
        } else if self.table_selected >= self.visible_rows.len() {
            self.table_selected = self.visible_rows.len() - 1;
        }
        if self.table_selected == 0 {
            self.table_offset = 0;
        }
    }

    /// Handles a terminal key press and returns side-effect commands to execute.
    pub fn handle_key(&mut self, key: KeyEvent) -> Vec<AppCommand> {
        if key.kind != KeyEventKind::Press {
            return Vec::new();
        }

        if key.code == KeyCode::Char('c') && key.modifiers.contains(KeyModifiers::CONTROL) {
            return vec![AppCommand::Quit];
        }

        match self.screen {
            Screen::Auth(_) => self.handle_auth_key(key),
            Screen::Library => self.handle_library_key(key),
        }
    }

    /// Applies an asynchronous or timer event and returns any follow-up commands.
    pub fn apply_event(&mut self, event: AppEvent) -> Vec<AppCommand> {
        match event {
            AppEvent::AuthSucceeded {
                client_id,
                token,
                username,
                rate_limit,
            } => {
                self.session_epoch = self.session_epoch.wrapping_add(1);
                self.rate_limit = rate_limit;
                self.auth_session.client_id = Some(client_id);
                self.auth_session.access_token = Some(token);
                self.auth_session.username = Some(username.clone());
                self.username = Some(username.clone());
                self.library = LibraryCollection::default();
                self.visible_rows.clear();
                self.cover_cache.clear();
                self.pending_reencode = None;
                self.active_mutations.clear();
                self.progress_rollback_base.clear();
                self.screen = Screen::Library;
                self.focus = FocusPane::Table;
                self.active_status = LibraryStatus::Watching;
                self.sidebar_index = 0;
                self.table_selected = 0;
                self.table_offset = 0;
                self.filter_query.clear();
                self.filter_active = false;
                self.filter_scope = FilterScope::CurrentList;
                self.description_scroll = 0;
                self.modal = ActiveModal::None;
                self.api_score_format = None;
                self.syncing = true;
                self.set_toast(format!("Signed in as {username}"), ToastLevel::Success);

                vec![
                    AppCommand::SaveAuthSession(self.auth_session.clone()),
                    AppCommand::FetchLibrary,
                ]
            }
            AppEvent::AuthFailed { error, rate_limit } => {
                self.rate_limit = rate_limit;
                if let Screen::Auth(auth) = &mut self.screen {
                    auth.submitting = false;
                    auth.error = Some(error);
                }
                Vec::new()
            }
            AppEvent::SnapshotLoaded { epoch, mut library } => {
                if epoch != self.session_epoch
                    || !self.library.is_empty()
                    || !self.active_mutations.is_empty()
                {
                    return Vec::new();
                }
                if library.score_format.is_some() {
                    self.api_score_format = library.score_format;
                }
                library.rebuild_search_indexes();
                self.library = library;
                self.rebuild_visible_rows();
                self.ensure_covers_commands()
            }
            AppEvent::LibraryLoaded {
                epoch,
                mut library,
                rate_limit,
            } => {
                if epoch != self.session_epoch {
                    return Vec::new();
                }
                self.rate_limit = rate_limit;
                self.syncing = false;
                if library.score_format.is_some() {
                    self.api_score_format = library.score_format;
                }
                library.rebuild_search_indexes();

                let prev_selected_id = self.selected_item().map(|item| item.id);

                if !self.active_mutations.is_empty() {
                    let in_flight_ids: Vec<MediaId> =
                        self.active_mutations.keys().copied().collect();
                    for media_id in in_flight_ids {
                        if let Some(local_item) = self.library.get(media_id).cloned() {
                            if let Some((server_status, server_idx)) =
                                library.find_by_media_id(media_id)
                            {
                                if server_status == local_item.entry.status {
                                    if let Some(slot) =
                                        library.items_for_mut(server_status).get_mut(server_idx)
                                    {
                                        slot.entry = local_item.entry;
                                    }
                                } else if let Some((_, _, mut server_item)) =
                                    library.remove_item(media_id)
                                {
                                    server_item.entry = local_item.entry;
                                    library
                                        .items_for_mut(local_item.entry.status)
                                        .insert(0, server_item);
                                }
                            } else {
                                library
                                    .items_for_mut(local_item.entry.status)
                                    .insert(0, local_item);
                            }
                        } else {
                            // Optimistic delete is still in-flight; keep item removed locally.
                            library.remove_item(media_id);
                        }
                    }
                }

                self.library = library;
                self.rebuild_visible_rows();

                if let Some(prev_id) = prev_selected_id {
                    self.select_media_id_if_visible(prev_id);
                }

                let mut cmds = vec![AppCommand::SaveSnapshot];
                cmds.extend(self.ensure_covers_commands());
                cmds
            }
            AppEvent::LibraryLoadFailed {
                epoch,
                error,
                is_auth_error,
                rate_limit,
            } => {
                if epoch != self.session_epoch {
                    return Vec::new();
                }
                self.rate_limit = rate_limit;
                self.syncing = false;
                if is_auth_error {
                    return self.redirect_to_auth(
                        "AniList rejected your token (401). Please re-authenticate.".to_owned(),
                    );
                }
                self.set_toast(error, ToastLevel::Error);
                Vec::new()
            }
            AppEvent::EntrySaved {
                epoch,
                media_id,
                entry,
                rollback,
                rate_limit,
            } => {
                if epoch != self.session_epoch {
                    return Vec::new();
                }
                self.rate_limit = rate_limit;
                let latest_seq = self.active_mutations.get(&media_id).copied();
                if latest_seq != Some(rollback.seq()) {
                    // A newer optimistic edit is pending/in-flight; keep newer local values,
                    // though we can record the newly assigned EntryId if it was missing.
                    if let Some(entry_id) = entry.id {
                        if let Some(item) = self.library.get_mut(media_id) {
                            if item.entry.id.is_none() {
                                item.entry.id = Some(entry_id);
                            }
                        }
                    }
                    return Vec::new();
                }

                self.active_mutations.remove(&media_id);
                self.progress_rollback_base.remove(&media_id);

                let prev_selected_id = self.selected_item().map(|item| item.id);
                if let Some((current_status, idx)) = self.library.find_by_media_id(media_id) {
                    if let Some(item) = self.library.items_for_mut(current_status).get_mut(idx) {
                        item.entry = entry;
                    }
                    if current_status != entry.status {
                        self.library
                            .move_item(media_id, current_status, entry.status);
                    }
                }

                self.rebuild_visible_rows();
                if let Some(prev_id) = prev_selected_id {
                    self.select_media_id_if_visible(prev_id);
                }

                let mut cmds = vec![AppCommand::SaveSnapshot];
                if self.selected_item().map(|item| item.id) != prev_selected_id {
                    self.description_scroll = 0;
                    cmds.extend(self.ensure_covers_commands());
                }
                cmds
            }
            AppEvent::EntryDeleted {
                epoch,
                media_id,
                rollback,
                rate_limit,
            } => {
                if epoch != self.session_epoch {
                    return Vec::new();
                }
                self.rate_limit = rate_limit;
                if self.active_mutations.get(&media_id).copied() == Some(rollback.seq()) {
                    self.active_mutations.remove(&media_id);
                    self.progress_rollback_base.remove(&media_id);
                    self.set_toast("Removed from library.".to_owned(), ToastLevel::Success);
                    return vec![AppCommand::SaveSnapshot];
                }
                Vec::new()
            }
            AppEvent::MutationFailed {
                epoch,
                media_id,
                rollback,
                error,
                is_auth_error,
                retry_after_secs,
                rate_limit,
            } => {
                if epoch != self.session_epoch {
                    return Vec::new();
                }
                self.rate_limit = rate_limit;
                let prev_selected_id = self.selected_item().map(|item| item.id);

                if self.active_mutations.get(&media_id).copied() == Some(rollback.seq()) {
                    self.active_mutations.remove(&media_id);
                    self.progress_rollback_base.remove(&media_id);
                    self.apply_rollback(media_id, rollback);
                    self.rebuild_visible_rows();
                    if let Some(prev_id) = prev_selected_id {
                        self.select_media_id_if_visible(prev_id);
                    }
                }

                if is_auth_error {
                    return self.redirect_to_auth(
                        "Authentication expired or invalid (401). Please enter a new token."
                            .to_owned(),
                    );
                }

                if let Some(secs) = retry_after_secs {
                    self.set_toast(
                        format!("Rate limited by AniList; retry in {secs}s"),
                        ToastLevel::Warning,
                    );
                } else {
                    self.set_toast(error, ToastLevel::Error);
                }

                if self.selected_item().map(|item| item.id) != prev_selected_id {
                    self.description_scroll = 0;
                    return self.ensure_covers_commands();
                }
                Vec::new()
            }
            AppEvent::Notice { message, level } => {
                self.set_toast(message, level);
                Vec::new()
            }
            AppEvent::CoverLoaded(loaded) => {
                match loaded.result {
                    Ok(proto) => self.cover_cache.insert(loaded.media_id, proto),
                    Err(_) => self.cover_cache.mark_failed(loaded.media_id),
                }
                Vec::new()
            }
            AppEvent::Tick => {
                if self.syncing {
                    self.sync_tick = self.sync_tick.wrapping_add(1);
                }
                if let Some(toast) = &self.toast {
                    if Instant::now() >= toast.expires_at {
                        self.toast = None;
                    }
                }
                Vec::new()
            }
        }
    }

    fn handle_auth_key(&mut self, key: KeyEvent) -> Vec<AppCommand> {
        let Screen::Auth(ref mut auth) = self.screen else {
            return Vec::new();
        };

        if auth.submitting {
            return Vec::new();
        }

        let has_ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        let has_alt = key.modifiers.contains(KeyModifiers::ALT);

        if has_ctrl && key.code == KeyCode::Char('o') {
            return open_auth_url(auth);
        }

        match key.code {
            KeyCode::Tab | KeyCode::Down => {
                auth.focused = auth.focused.next();
                Vec::new()
            }
            KeyCode::BackTab | KeyCode::Up => {
                auth.focused = auth.focused.prev();
                Vec::new()
            }
            KeyCode::Esc => vec![AppCommand::Quit],
            KeyCode::Enter => match auth.focused {
                AuthField::ClientId => {
                    auth.focused = auth.focused.next();
                    Vec::new()
                }
                AuthField::OpenUrl => open_auth_url(auth),
                AuthField::Token => {
                    if !auth.client_id.trim().is_empty() && !auth.token.trim().is_empty() {
                        submit_auth_form(auth)
                    } else {
                        auth.focused = auth.focused.next();
                        Vec::new()
                    }
                }
                AuthField::Submit => submit_auth_form(auth),
            },
            KeyCode::Backspace => {
                match auth.focused {
                    AuthField::ClientId => {
                        auth.client_id.pop();
                        auth.error = None;
                    }
                    AuthField::Token => {
                        auth.token.pop();
                        auth.error = None;
                    }
                    AuthField::OpenUrl | AuthField::Submit => {}
                }
                Vec::new()
            }
            KeyCode::Char(ch) if !has_ctrl && !has_alt => match auth.focused {
                AuthField::ClientId => {
                    auth.client_id.push(ch);
                    auth.error = None;
                    Vec::new()
                }
                AuthField::Token => {
                    auth.token.push(ch);
                    auth.error = None;
                    Vec::new()
                }
                AuthField::OpenUrl | AuthField::Submit => {
                    if ch == 'q' {
                        vec![AppCommand::Quit]
                    } else {
                        Vec::new()
                    }
                }
            },
            _ => Vec::new(),
        }
    }

    fn handle_library_key(&mut self, key: KeyEvent) -> Vec<AppCommand> {
        if self.modal != ActiveModal::None {
            return self.handle_modal_key(key);
        }

        if self.filter_active {
            return self.handle_filter_key(key);
        }

        let has_ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        let has_alt = key.modifiers.contains(KeyModifiers::ALT);

        if has_ctrl {
            return match key.code {
                KeyCode::Char('n') => {
                    self.focus = self.focus.next();
                    Vec::new()
                }
                KeyCode::Char('p') => {
                    self.focus = self.focus.prev();
                    Vec::new()
                }
                KeyCode::Char('a') => {
                    self.filter_scope = self.filter_scope.toggle();
                    self.description_scroll = 0;
                    self.rebuild_visible_rows();
                    self.ensure_covers_commands()
                }
                _ => Vec::new(),
            };
        }

        if has_alt {
            return Vec::new();
        }

        match key.code {
            KeyCode::Char('q') => return vec![AppCommand::Quit],
            KeyCode::Char('?') | KeyCode::Char(',') => {
                self.modal = ActiveModal::Help;
                return Vec::new();
            }
            KeyCode::Char('R') => {
                if self.username.is_some() && !self.syncing {
                    self.syncing = true;
                    return vec![AppCommand::FetchLibrary];
                }
                return Vec::new();
            }
            KeyCode::Char('/') => {
                self.filter_active = true;
                return Vec::new();
            }
            KeyCode::Char('s') => {
                self.modal = ActiveModal::SortPicker {
                    selected: self.sort_field.index(),
                };
                return Vec::new();
            }
            KeyCode::Char('S') => {
                return self.apply_sort(self.sort_field, self.sort_direction.toggle());
            }
            KeyCode::Esc => {
                if !self.filter_query.is_empty() {
                    self.filter_query.clear();
                    self.description_scroll = 0;
                    self.rebuild_visible_rows();
                    return self.ensure_covers_commands();
                }
                return Vec::new();
            }
            KeyCode::Tab => {
                self.focus = self.focus.next();
                return Vec::new();
            }
            KeyCode::BackTab => {
                self.focus = self.focus.prev();
                return Vec::new();
            }
            KeyCode::Char('h') | KeyCode::Left => {
                self.focus = match self.focus {
                    FocusPane::Sidebar => FocusPane::Table,
                    FocusPane::Table | FocusPane::Details => FocusPane::Table,
                };
                return Vec::new();
            }
            KeyCode::Char('l') | KeyCode::Right => {
                self.focus = match self.focus {
                    FocusPane::Table | FocusPane::Details => FocusPane::Sidebar,
                    FocusPane::Sidebar => FocusPane::Sidebar,
                };
                return Vec::new();
            }
            KeyCode::Char(c @ '1'..='6') => {
                let idx = (c as usize) - ('1' as usize);
                if let Some(status) = LibraryStatus::from_index(idx) {
                    return self.switch_active_status(status);
                }
                return Vec::new();
            }
            KeyCode::Char('+') | KeyCode::Char('i') => {
                return self.mutate_progress(1);
            }
            KeyCode::Char('-') | KeyCode::Char('d') => {
                return self.mutate_progress(-1);
            }
            KeyCode::Char('a') => {
                if let Some(item) = self.selected_item() {
                    self.modal = ActiveModal::StatusPicker {
                        selected: item.entry.status.index(),
                    };
                }
                return Vec::new();
            }
            KeyCode::Char('r') => {
                if let Some(item) = self.selected_item() {
                    let format = self.effective_score_format();
                    match format {
                        ScoreFormat::Point3 => {
                            let selected = item
                                .entry
                                .score
                                .map(Point3Score::from_score_raw)
                                .and_then(|score| {
                                    Point3Score::ALL.iter().position(|&s| s == score)
                                })
                                .unwrap_or(0);
                            self.modal = ActiveModal::RatingPicker { selected };
                        }
                        ScoreFormat::Point5 => {
                            let selected = item.entry.score.map_or(0, |raw| {
                                usize::from(5u8.saturating_sub(ScoreFormat::raw_to_point5(raw)))
                            });
                            self.modal = ActiveModal::RatingPicker { selected };
                        }
                        ScoreFormat::Point10Decimal
                        | ScoreFormat::Point10
                        | ScoreFormat::Point100 => {
                            self.rating_input = format.format_input_initial(item.entry.score);
                            self.rating_input_error = None;
                            self.rating_pristine = true;
                            self.modal = ActiveModal::RatingPicker { selected: 0 };
                        }
                    }
                }
                return Vec::new();
            }
            KeyCode::Char('v') => {
                if let Some(item) = self.selected_item() {
                    if let Some(url) = item.site_url.as_ref().filter(|u| !u.trim().is_empty()) {
                        return vec![AppCommand::OpenUrl(url.clone())];
                    }
                    self.set_toast(
                        "AniList link is unavailable for this anime.".to_owned(),
                        ToastLevel::Warning,
                    );
                }
                return Vec::new();
            }
            KeyCode::Char('J') => {
                if self.selected_item().is_some() {
                    self.description_scroll = self.description_scroll.saturating_add(1);
                }
                return Vec::new();
            }
            KeyCode::Char('K') => {
                if self.selected_item().is_some() {
                    self.description_scroll = self.description_scroll.saturating_sub(1);
                }
                return Vec::new();
            }
            KeyCode::PageDown => {
                if self.selected_item().is_some() {
                    self.description_scroll = self.description_scroll.saturating_add(5);
                }
                return Vec::new();
            }
            KeyCode::PageUp => {
                if self.selected_item().is_some() {
                    self.description_scroll = self.description_scroll.saturating_sub(5);
                }
                return Vec::new();
            }
            _ => {}
        }

        self.handle_focused_pane_key(key)
    }

    fn handle_focused_pane_key(&mut self, key: KeyEvent) -> Vec<AppCommand> {
        match self.focus {
            FocusPane::Sidebar => match key.code {
                KeyCode::Char('j') | KeyCode::Down => {
                    let max_idx = SidebarItem::ALL.len().saturating_sub(1);
                    if self.sidebar_index < max_idx {
                        self.sidebar_index += 1;
                        if let Some(SidebarItem::Status(status)) =
                            SidebarItem::ALL.get(self.sidebar_index).copied()
                        {
                            return self.switch_active_status(status);
                        }
                    }
                    Vec::new()
                }
                KeyCode::Char('k') | KeyCode::Up => {
                    if self.sidebar_index > 0 {
                        self.sidebar_index -= 1;
                        if let Some(SidebarItem::Status(status)) =
                            SidebarItem::ALL.get(self.sidebar_index).copied()
                        {
                            return self.switch_active_status(status);
                        }
                    }
                    Vec::new()
                }
                KeyCode::Char('g') | KeyCode::Home => {
                    self.sidebar_index = 0;
                    if let Some(SidebarItem::Status(status)) =
                        SidebarItem::ALL.get(self.sidebar_index).copied()
                    {
                        return self.switch_active_status(status);
                    }
                    Vec::new()
                }
                KeyCode::Char('G') | KeyCode::End => {
                    self.sidebar_index = SidebarItem::ALL.len().saturating_sub(1);
                    Vec::new()
                }
                KeyCode::Enter | KeyCode::Char(' ') => {
                    match SidebarItem::ALL.get(self.sidebar_index).copied() {
                        Some(SidebarItem::Status(status)) => {
                            self.focus = FocusPane::Table;
                            self.switch_active_status(status)
                        }
                        Some(SidebarItem::LogOut) => self.perform_logout(),
                        None => Vec::new(),
                    }
                }
                _ => Vec::new(),
            },
            FocusPane::Table => match key.code {
                KeyCode::Char('j') | KeyCode::Down => {
                    if self.table_selected + 1 < self.visible_rows.len() {
                        self.table_selected += 1;
                        self.description_scroll = 0;
                        return self.ensure_covers_commands();
                    }
                    Vec::new()
                }
                KeyCode::Char('k') | KeyCode::Up => {
                    if self.table_selected > 0 {
                        self.table_selected -= 1;
                        self.description_scroll = 0;
                        return self.ensure_covers_commands();
                    }
                    Vec::new()
                }
                KeyCode::Char('g') | KeyCode::Home => {
                    if !self.visible_rows.is_empty() && self.table_selected != 0 {
                        self.table_selected = 0;
                        self.table_offset = 0;
                        self.description_scroll = 0;
                        return self.ensure_covers_commands();
                    }
                    Vec::new()
                }
                KeyCode::Char('G') | KeyCode::End => {
                    if let Some(last) = self.visible_rows.len().checked_sub(1) {
                        if self.table_selected != last {
                            self.table_selected = last;
                            self.description_scroll = 0;
                            return self.ensure_covers_commands();
                        }
                    }
                    Vec::new()
                }
                KeyCode::Enter => {
                    if self.selected_item().is_some() {
                        self.focus = FocusPane::Details;
                    }
                    Vec::new()
                }
                _ => Vec::new(),
            },
            FocusPane::Details => match key.code {
                KeyCode::Char('j') | KeyCode::Down => {
                    self.description_scroll = self.description_scroll.saturating_add(1);
                    Vec::new()
                }
                KeyCode::Char('k') | KeyCode::Up => {
                    self.description_scroll = self.description_scroll.saturating_sub(1);
                    Vec::new()
                }
                KeyCode::Char('g') | KeyCode::Home => {
                    self.description_scroll = 0;
                    Vec::new()
                }
                _ => Vec::new(),
            },
        }
    }

    fn handle_filter_key(&mut self, key: KeyEvent) -> Vec<AppCommand> {
        let has_ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        let has_alt = key.modifiers.contains(KeyModifiers::ALT);

        if has_ctrl {
            return match key.code {
                KeyCode::Char('a') => {
                    self.filter_scope = self.filter_scope.toggle();
                    self.description_scroll = 0;
                    self.rebuild_visible_rows();
                    self.ensure_covers_commands()
                }
                KeyCode::Char('n') => {
                    self.filter_active = false;
                    self.focus = self.focus.next();
                    Vec::new()
                }
                KeyCode::Char('p') => {
                    self.filter_active = false;
                    self.focus = self.focus.prev();
                    Vec::new()
                }
                _ => Vec::new(),
            };
        }

        if has_alt {
            return Vec::new();
        }

        match key.code {
            KeyCode::Esc => {
                self.filter_query.clear();
                self.filter_active = false;
                self.description_scroll = 0;
                self.rebuild_visible_rows();
                self.ensure_covers_commands()
            }
            KeyCode::Enter => {
                self.filter_active = false;
                self.focus = FocusPane::Table;
                Vec::new()
            }
            KeyCode::Tab => {
                self.filter_active = false;
                self.focus = FocusPane::Table;
                Vec::new()
            }
            KeyCode::Backspace => {
                if self.filter_query.pop().is_some() {
                    self.description_scroll = 0;
                    self.rebuild_visible_rows();
                    return self.ensure_covers_commands();
                }
                Vec::new()
            }
            KeyCode::Char(ch) => {
                self.filter_query.push(ch);
                self.description_scroll = 0;
                self.rebuild_visible_rows();
                self.ensure_covers_commands()
            }
            _ => Vec::new(),
        }
    }

    fn handle_modal_key(&mut self, key: KeyEvent) -> Vec<AppCommand> {
        let has_ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        let has_alt = key.modifiers.contains(KeyModifiers::ALT);
        if has_ctrl || has_alt {
            return Vec::new();
        }

        let count = self.active_modal_options_count();

        match self.modal {
            ActiveModal::None => Vec::new(),
            ActiveModal::Help => match key.code {
                KeyCode::Esc | KeyCode::Char('q') | KeyCode::Char('?') | KeyCode::Enter => {
                    self.modal = ActiveModal::None;
                    Vec::new()
                }
                KeyCode::Char('t') => self.cycle_theme(),
                KeyCode::Char('s') => self.cycle_score_format(),
                _ => Vec::new(),
            },
            ActiveModal::StatusPicker { selected } => match key.code {
                KeyCode::Esc | KeyCode::Char('q') => {
                    self.modal = ActiveModal::None;
                    Vec::new()
                }
                KeyCode::Char('j') | KeyCode::Down => {
                    if count > 0 {
                        self.modal = ActiveModal::StatusPicker {
                            selected: (selected + 1).min(count - 1),
                        };
                    }
                    Vec::new()
                }
                KeyCode::Char('k') | KeyCode::Up => {
                    self.modal = ActiveModal::StatusPicker {
                        selected: selected.saturating_sub(1),
                    };
                    Vec::new()
                }
                KeyCode::Enter => {
                    self.modal = ActiveModal::None;
                    self.confirm_status_picker(selected)
                }
                _ => Vec::new(),
            },
            ActiveModal::RatingPicker { selected } => {
                let format = self.effective_score_format();
                self.handle_rating_modal_key(key, format, selected)
            }
            ActiveModal::SortPicker { selected } => match key.code {
                KeyCode::Esc | KeyCode::Char('q') | KeyCode::Char('s') => {
                    self.modal = ActiveModal::None;
                    Vec::new()
                }
                KeyCode::Char('j') | KeyCode::Down => {
                    if count > 0 {
                        self.modal = ActiveModal::SortPicker {
                            selected: (selected + 1).min(count - 1),
                        };
                    }
                    Vec::new()
                }
                KeyCode::Char('k') | KeyCode::Up => {
                    self.modal = ActiveModal::SortPicker {
                        selected: selected.saturating_sub(1),
                    };
                    Vec::new()
                }
                KeyCode::Char('r') | KeyCode::Char(' ') => {
                    self.apply_sort(self.sort_field, self.sort_direction.toggle())
                }
                KeyCode::Char(ch @ '1'..='5') => {
                    let idx = usize::from((ch as u8) - b'1');
                    self.modal = ActiveModal::None;
                    self.confirm_sort_picker(idx)
                }
                KeyCode::Enter => {
                    self.modal = ActiveModal::None;
                    self.confirm_sort_picker(selected)
                }
                _ => Vec::new(),
            },
        }
    }

    fn confirm_sort_picker(&mut self, selected: usize) -> Vec<AppCommand> {
        let Some(field) = SortField::from_index(selected) else {
            return Vec::new();
        };
        let direction = if field == self.sort_field {
            self.sort_direction.toggle()
        } else {
            field.default_direction()
        };
        self.apply_sort(field, direction)
    }

    fn apply_sort(&mut self, field: SortField, direction: SortDirection) -> Vec<AppCommand> {
        let prev_selected_id = self.selected_item().map(|item| item.id);
        self.sort_field = field;
        self.sort_direction = direction;
        self.app_config.sort_by = field;
        self.app_config.sort_order = direction;
        self.rebuild_visible_rows();
        if let Some(prev_id) = prev_selected_id {
            self.select_media_id_if_visible(prev_id);
        }
        self.set_toast(
            format!("Sorted by {} {}", field.short_label(), direction.arrow()),
            ToastLevel::Success,
        );
        let mut cmds = vec![AppCommand::SaveAppConfig(self.app_config.clone())];
        cmds.extend(self.ensure_covers_commands());
        cmds
    }

    fn cycle_theme(&mut self) -> Vec<AppCommand> {
        let next = ThemePalette::next_theme_name(&self.app_config.theme);
        self.app_config.theme = next.to_owned();
        self.palette = ThemePalette::from_config(&self.app_config);
        self.set_toast(
            format!("Theme: {}", ThemePalette::display_name(next)),
            ToastLevel::Success,
        );
        vec![AppCommand::SaveAppConfig(self.app_config.clone())]
    }

    fn cycle_score_format(&mut self) -> Vec<AppCommand> {
        let next = match self.score_format_pref {
            ScoreFormatPreference::Auto => ScoreFormatPreference::Override(ScoreFormat::ALL[0]),
            ScoreFormatPreference::Override(fmt) => ScoreFormat::ALL
                .iter()
                .position(|&candidate| candidate == fmt)
                .and_then(|idx| ScoreFormat::ALL.get(idx + 1).copied())
                .map_or(ScoreFormatPreference::Auto, ScoreFormatPreference::Override),
        };
        self.score_format_pref = next;
        self.app_config.score_format = next;
        let msg = match next {
            ScoreFormatPreference::Auto => format!(
                "Rating system: Auto ({})",
                self.effective_score_format().short_label()
            ),
            ScoreFormatPreference::Override(fmt) => format!("Rating system: {}", fmt.label()),
        };
        self.set_toast(msg, ToastLevel::Success);
        vec![AppCommand::SaveAppConfig(self.app_config.clone())]
    }

    fn handle_rating_modal_key(
        &mut self,
        key: KeyEvent,
        format: ScoreFormat,
        selected: usize,
    ) -> Vec<AppCommand> {
        let count = self.active_modal_options_count();
        if format.uses_choice_picker() {
            return match key.code {
                KeyCode::Esc | KeyCode::Char('q') => {
                    self.modal = ActiveModal::None;
                    Vec::new()
                }
                KeyCode::Char('j') | KeyCode::Down => {
                    if count > 0 {
                        self.modal = ActiveModal::RatingPicker {
                            selected: (selected + 1).min(count - 1),
                        };
                    }
                    Vec::new()
                }
                KeyCode::Char('k') | KeyCode::Up => {
                    self.modal = ActiveModal::RatingPicker {
                        selected: selected.saturating_sub(1),
                    };
                    Vec::new()
                }
                KeyCode::Char('c') | KeyCode::Char('0') => {
                    self.modal = ActiveModal::None;
                    self.apply_score_update(ScoreUpdate::Clear)
                }
                KeyCode::Char(ch @ '1'..='5') if format == ScoreFormat::Point5 => {
                    let stars = (ch as u8) - b'0';
                    self.modal = ActiveModal::None;
                    match ScoreFormat::point5_to_raw(stars) {
                        Some(raw) => self.apply_score_update(ScoreUpdate::Set(raw)),
                        None => Vec::new(),
                    }
                }
                KeyCode::Char(ch @ '1'..='3') if format == ScoreFormat::Point3 => {
                    let idx = usize::from((ch as u8) - b'1');
                    self.modal = ActiveModal::None;
                    self.confirm_rating_choice(format, idx)
                }
                KeyCode::Enter => {
                    self.modal = ActiveModal::None;
                    self.confirm_rating_choice(format, selected)
                }
                _ => Vec::new(),
            };
        }

        match key.code {
            KeyCode::Esc | KeyCode::Char('q') => {
                self.modal = ActiveModal::None;
                self.rating_input_error = None;
                Vec::new()
            }
            KeyCode::Char('c') => {
                self.modal = ActiveModal::None;
                self.rating_input.clear();
                self.rating_input_error = None;
                self.apply_score_update(ScoreUpdate::Clear)
            }
            KeyCode::Up | KeyCode::Char('k') | KeyCode::Char('+') => {
                self.step_rating_input(format, 1);
                Vec::new()
            }
            KeyCode::Down | KeyCode::Char('j') | KeyCode::Char('-') => {
                self.step_rating_input(format, -1);
                Vec::new()
            }
            KeyCode::PageUp => {
                let step = if format == ScoreFormat::Point10 { 2 } else { 10 };
                self.step_rating_input(format, step);
                Vec::new()
            }
            KeyCode::PageDown => {
                let step = if format == ScoreFormat::Point10 { -2 } else { -10 };
                self.step_rating_input(format, step);
                Vec::new()
            }
            KeyCode::Backspace => {
                if self.rating_pristine {
                    self.rating_input.clear();
                    self.rating_pristine = false;
                } else {
                    self.rating_input.pop();
                }
                self.rating_input_error = None;
                Vec::new()
            }
            KeyCode::Char(ch)
                if ch.is_ascii_digit()
                    || (ch == '.'
                        && format == ScoreFormat::Point10Decimal
                        && (self.rating_pristine || !self.rating_input.contains('.'))) =>
            {
                if self.rating_pristine {
                    self.rating_input.clear();
                    self.rating_pristine = false;
                }
                if self.rating_input.len() < 6 {
                    self.rating_input.push(ch);
                }
                self.rating_input_error = None;
                Vec::new()
            }
            KeyCode::Enter => match format.parse_input(&self.rating_input) {
                Ok(update) => {
                    self.modal = ActiveModal::None;
                    self.rating_input_error = None;
                    self.apply_score_update(update)
                }
                Err(msg) => {
                    self.rating_input_error = Some(msg.to_owned());
                    Vec::new()
                }
            },
            _ => Vec::new(),
        }
    }

    fn step_rating_input(&mut self, format: ScoreFormat, delta_steps: i8) {
        let current = if self.rating_input.trim().is_empty() {
            None
        } else {
            match format.parse_input(&self.rating_input) {
                Ok(ScoreUpdate::Set(raw)) => Some(raw),
                Ok(ScoreUpdate::Clear) => None,
                Err(_) => self.selected_item().and_then(|item| item.entry.score),
            }
        };

        match format.step_raw(current, delta_steps) {
            Some(next) => self.rating_input = format.format_input_initial(Some(next)),
            None => self.rating_input.clear(),
        }
        self.rating_pristine = false;
        self.rating_input_error = None;
    }

    fn switch_active_status(&mut self, status: LibraryStatus) -> Vec<AppCommand> {
        self.active_status = status;
        self.sidebar_index = status.index();
        self.filter_scope = FilterScope::CurrentList;
        self.description_scroll = 0;
        self.rebuild_visible_rows();
        self.ensure_covers_commands()
    }

    fn perform_logout(&mut self) -> Vec<AppCommand> {
        self.session_epoch = self.session_epoch.wrapping_add(1);
        let old_username = self.username.take();
        self.auth_session.clear_credentials();
        self.library = LibraryCollection::default();
        self.visible_rows.clear();
        self.active_status = LibraryStatus::Watching;
        self.focus = FocusPane::Table;
        self.sidebar_index = 0;
        self.table_selected = 0;
        self.table_offset = 0;
        self.filter_query.clear();
        self.filter_active = false;
        self.filter_scope = FilterScope::CurrentList;
        self.description_scroll = 0;
        self.modal = ActiveModal::None;
        self.api_score_format = None;
        self.syncing = false;
        self.toast = None;
        self.pending_reencode = None;
        self.cover_cache.clear();
        self.active_mutations.clear();
        self.progress_rollback_base.clear();

        self.screen = Screen::Auth(AuthScreenState {
            client_id: self.auth_session.resolve_client_id().unwrap_or_default(),
            token: String::new(),
            focused: AuthField::ClientId,
            submitting: false,
            error: None,
        });

        let mut cmds = vec![AppCommand::SaveAuthSession(self.auth_session.clone())];
        if let Some(username) = old_username {
            cmds.push(AppCommand::RemoveSnapshot(username));
        }
        cmds
    }

    fn redirect_to_auth(&mut self, error_message: String) -> Vec<AppCommand> {
        self.session_epoch = self.session_epoch.wrapping_add(1);
        self.auth_session.clear_credentials();
        self.username = None;
        self.library = LibraryCollection::default();
        self.visible_rows.clear();
        self.table_selected = 0;
        self.table_offset = 0;
        self.description_scroll = 0;
        self.syncing = false;
        self.modal = ActiveModal::None;
        self.api_score_format = None;
        self.filter_query.clear();
        self.filter_active = false;
        self.pending_reencode = None;
        self.cover_cache.clear();
        self.active_mutations.clear();
        self.progress_rollback_base.clear();

        let client_id = self.auth_session.resolve_client_id().unwrap_or_default();
        let focused = if client_id.is_empty() {
            AuthField::ClientId
        } else {
            AuthField::Token
        };

        self.screen = Screen::Auth(AuthScreenState {
            client_id,
            token: String::new(),
            focused,
            submitting: false,
            error: Some(error_message),
        });

        vec![AppCommand::SaveAuthSession(self.auth_session.clone())]
    }

    fn mutate_progress(&mut self, delta: i32) -> Vec<AppCommand> {
        let Some(&(status, idx)) = self.visible_rows.get(self.table_selected) else {
            return Vec::new();
        };
        let Some(item) = self.library.items_for_mut(status).get_mut(idx) else {
            return Vec::new();
        };

        let current = item.entry.progress;
        let next = if delta >= 0 {
            item.clamp_progress(current.saturating_add(delta as u32))
        } else {
            current.saturating_sub(delta.unsigned_abs())
        };

        if next == current {
            return Vec::new();
        }

        let media_id = item.id;
        item.entry.progress = next;

        let base_progress = *self
            .progress_rollback_base
            .entry(media_id)
            .or_insert(current);
        let seq = self.next_seq();
        self.active_mutations.insert(media_id, seq);

        vec![AppCommand::ScheduleProgress {
            media_id,
            progress: next,
            rollback: PendingRollback::Progress {
                previous_progress: base_progress,
                seq,
            },
        }]
    }

    fn confirm_status_picker(&mut self, selected: usize) -> Vec<AppCommand> {
        let Some(&(bucket_status, idx)) = self.visible_rows.get(self.table_selected) else {
            return Vec::new();
        };
        let Some(item) = self.library.items_for(bucket_status).get(idx) else {
            return Vec::new();
        };
        let media_id = item.id;
        let current_entry = item.entry;

        if selected == LibraryStatus::ALL.len() {
            let Some(entry_id) = current_entry.id else {
                self.set_toast(
                    "Cannot remove entry without list ID; try refreshing (R).".to_owned(),
                    ToastLevel::Warning,
                );
                return Vec::new();
            };

            let Some((previous_status, previous_index, mut removed_item)) =
                self.library.remove_item(media_id)
            else {
                return Vec::new();
            };
            if let Some(base_prog) = self.progress_rollback_base.remove(&media_id) {
                removed_item.entry.progress = base_prog;
            }

            let seq = self.next_seq();
            self.active_mutations.insert(media_id, seq);
            self.description_scroll = 0;
            self.rebuild_visible_rows();

            let mut cmds = vec![AppCommand::DeleteEntry {
                media_id,
                entry_id,
                rollback: PendingRollback::Delete {
                    previous_status,
                    previous_index,
                    item: removed_item,
                    seq,
                },
            }];
            cmds.extend(self.ensure_covers_commands());
            return cmds;
        }

        let Some(target_status) = LibraryStatus::from_index(selected) else {
            return Vec::new();
        };
        if target_status == current_entry.status {
            return Vec::new();
        }

        let previous_index = self
            .library
            .items_for(bucket_status)
            .iter()
            .position(|entry| entry.id == media_id)
            .unwrap_or(idx);
        let mut previous_entry = current_entry;
        if let Some(base_prog) = self.progress_rollback_base.remove(&media_id) {
            previous_entry.progress = base_prog;
        }

        self.library
            .move_item(media_id, bucket_status, target_status);
        let seq = self.next_seq();
        self.active_mutations.insert(media_id, seq);
        self.description_scroll = 0;
        self.rebuild_visible_rows();
        self.select_media_id_if_visible(media_id);

        let mut cmds = vec![AppCommand::SaveStatus {
            media_id,
            status: target_status,
            rollback: PendingRollback::Status {
                previous_status: bucket_status,
                previous_index,
                previous_entry,
                seq,
            },
        }];
        cmds.extend(self.ensure_covers_commands());
        cmds
    }

    fn confirm_rating_choice(&mut self, format: ScoreFormat, selected: usize) -> Vec<AppCommand> {
        let update = match format {
            ScoreFormat::Point3 => {
                if selected == Point3Score::ALL.len() {
                    ScoreUpdate::Clear
                } else if let Some(&score) = Point3Score::ALL.get(selected) {
                    ScoreUpdate::Set(score.as_score_raw())
                } else {
                    return Vec::new();
                }
            }
            ScoreFormat::Point5 => {
                if selected == 5 {
                    ScoreUpdate::Clear
                } else if selected < 5 {
                    let stars = 5u8.saturating_sub(selected as u8);
                    let Some(raw) = ScoreFormat::point5_to_raw(stars) else {
                        return Vec::new();
                    };
                    ScoreUpdate::Set(raw)
                } else {
                    return Vec::new();
                }
            }
            _ => return Vec::new(),
        };

        self.apply_score_update(update)
    }

    fn apply_score_update(&mut self, update: ScoreUpdate) -> Vec<AppCommand> {
        let Some(&(status, idx)) = self.visible_rows.get(self.table_selected) else {
            return Vec::new();
        };
        let Some(item) = self.library.items_for_mut(status).get_mut(idx) else {
            return Vec::new();
        };

        let next_score = match update {
            ScoreUpdate::Clear => None,
            ScoreUpdate::Set(score) => Some(score),
        };
        if next_score == item.entry.score {
            return Vec::new();
        }

        let media_id = item.id;
        let previous_score = item.entry.score;
        item.entry.score = next_score;

        let previous_progress = self.progress_rollback_base.remove(&media_id);
        let seq = self.next_seq();
        self.active_mutations.insert(media_id, seq);

        if self.sort_field == SortField::Score {
            self.rebuild_visible_rows();
            self.select_media_id_if_visible(media_id);
        }

        vec![AppCommand::SaveScore {
            media_id,
            score: update,
            rollback: PendingRollback::Score {
                previous_score,
                previous_progress,
                seq,
            },
        }]
    }

    fn apply_rollback(&mut self, media_id: MediaId, rollback: PendingRollback) {
        match rollback {
            PendingRollback::Progress {
                previous_progress, ..
            } => {
                if let Some(item) = self.library.get_mut(media_id) {
                    item.entry.progress = previous_progress;
                }
            }
            PendingRollback::Status {
                previous_status,
                previous_index,
                previous_entry,
                ..
            } => {
                if let Some((_, _, mut item)) = self.library.remove_item(media_id) {
                    item.entry = previous_entry;
                    self.library
                        .restore_item(previous_status, previous_index, item);
                }
            }
            PendingRollback::Score {
                previous_score,
                previous_progress,
                ..
            } => {
                if let Some(item) = self.library.get_mut(media_id) {
                    item.entry.score = previous_score;
                    if let Some(prev_prog) = previous_progress {
                        item.entry.progress = prev_prog;
                    }
                }
            }
            PendingRollback::Delete {
                previous_status,
                previous_index,
                item,
                ..
            } => {
                self.library
                    .restore_item(previous_status, previous_index, item);
            }
        }
    }

    fn ensure_covers_commands(&self) -> Vec<AppCommand> {
        if self.visible_rows.is_empty() {
            return Vec::new();
        }

        let center = self
            .table_selected
            .min(self.visible_rows.len().saturating_sub(1));
        let start = center.saturating_sub(PREFETCH_RADIUS);
        let end = (center + PREFETCH_RADIUS).min(self.visible_rows.len().saturating_sub(1));

        let mut targets = Vec::with_capacity(end - start + 1);
        self.push_cover_target(center, &mut targets);
        for offset in 1..=PREFETCH_RADIUS {
            let next = center + offset;
            if next <= end {
                self.push_cover_target(next, &mut targets);
            }
            if center >= offset {
                let prev = center - offset;
                if prev >= start {
                    self.push_cover_target(prev, &mut targets);
                }
            }
        }

        if targets.is_empty() {
            Vec::new()
        } else {
            vec![AppCommand::EnsureCovers(targets)]
        }
    }

    fn push_cover_target(&self, visible_idx: usize, targets: &mut Vec<(MediaId, String)>) {
        let Some(&(status, idx)) = self.visible_rows.get(visible_idx) else {
            return;
        };
        let Some(item) = self.library.items_for(status).get(idx) else {
            return;
        };
        if let Some(url) = item.cover_url.as_ref().filter(|u| !u.trim().is_empty()) {
            targets.push((item.id, url.clone()));
        }
    }

    fn select_media_id_if_visible(&mut self, media_id: MediaId) {
        if let Some(pos) = self.visible_rows.iter().position(|&(status, idx)| {
            self.library
                .items_for(status)
                .get(idx)
                .is_some_and(|item| item.id == media_id)
        }) {
            self.table_selected = pos;
        }
    }

    fn next_seq(&mut self) -> u64 {
        self.next_mutation_seq = self.next_mutation_seq.wrapping_add(1);
        self.next_mutation_seq
    }

    fn set_toast(&mut self, message: String, level: ToastLevel) {
        let duration = match level {
            ToastLevel::Info | ToastLevel::Success => TOAST_SHORT,
            ToastLevel::Warning | ToastLevel::Error => TOAST_LONG,
        };
        self.toast = Some(Toast {
            message,
            level,
            expires_at: Instant::now() + duration,
        });
    }
}

fn open_auth_url(auth: &mut AuthScreenState) -> Vec<AppCommand> {
    match oauth_url(&auth.client_id) {
        Ok(url) => {
            auth.error = None;
            vec![AppCommand::OpenUrl(url)]
        }
        Err(_) => {
            auth.error = Some("Enter your AniList Client ID first.".to_owned());
            Vec::new()
        }
    }
}

fn submit_auth_form(auth: &mut AuthScreenState) -> Vec<AppCommand> {
    let client_id = auth.client_id.trim();
    if client_id.is_empty() {
        auth.error = Some("Client ID is required.".to_owned());
        return Vec::new();
    }
    let Some(token) = extract_access_token(&auth.token) else {
        auth.error = Some(if auth.token.contains("error=") {
            format!(
                "AniList returned an OAuth error. Set the app's Redirect URL to {ANILIST_OAUTH_PIN} and try again."
            )
        } else {
            "Access token is required.".to_owned()
        });
        return Vec::new();
    };

    auth.error = None;
    auth.submitting = true;
    vec![AppCommand::Authenticate {
        client_id: client_id.to_owned(),
        token,
    }]
}

/// Extracts an AniList access token from whatever the browser shows.
///
/// Accepts a bare JWT, a `Bearer ...` header value, or a full auth-pin
/// callback URL containing `access_token=...`, so users can paste the
/// callback URL instead of hunting for the token fragment.
fn extract_access_token(input: &str) -> Option<String> {
    let text = input.trim().trim_matches(|c| c == '"' || c == '\'');
    if text.is_empty() {
        return None;
    }
    if text.contains("error=") && !text.contains("access_token=") {
        return None;
    }
    let candidate = match text.split_once("access_token=") {
        Some((_, rest)) => rest.split(|c| c == '&' || c == '#').next().unwrap_or(rest),
        None => text
            .strip_prefix("Bearer ")
            .or_else(|| text.strip_prefix("bearer "))
            .unwrap_or(text),
    };
    let candidate = candidate.trim();
    if candidate.is_empty() {
        None
    } else {
        Some(candidate.to_owned())
    }
}

enum PersistOp {
    SaveSnapshot {
        username: String,
        library: LibraryCollection,
    },
    RemoveSnapshot {
        username: String,
    },
    SaveAuthSession(AuthSession),
    SaveAppConfig(AppConfig),
}

struct PendingProgress {
    epoch: u64,
    progress: u32,
    rollback: PendingRollback,
    deadline: tokio::time::Instant,
}

struct RuntimeContext {
    client: AnilistClient,
    cover_loader: CoverLoader,
    app_tx: mpsc::UnboundedSender<AppEvent>,
    cover_tx: mpsc::UnboundedSender<CoverLoaded>,
    persist_tx: Option<mpsc::UnboundedSender<PersistOp>>,
    pending_progress: HashMap<MediaId, PendingProgress>,
}

/// Runs the interactive terminal event loop until the user quits.
pub async fn run_app<B: Backend>(
    terminal: &mut Terminal<B>,
    app_config: AppConfig,
    auth: AuthSession,
    client: AnilistClient,
    picker: Picker,
) -> anyhow::Result<()> {
    let mut state = AppState::from_config(app_config, auth.clone());

    let resolved_cache_dir =
        cache_dir().unwrap_or_else(|_| std::env::temp_dir().join("anits"));
    let cover_loader = CoverLoader::new(
        client.http().clone(),
        resolved_cache_dir.join("covers"),
        picker,
    );

    let (app_tx, mut app_rx) = mpsc::unbounded_channel::<AppEvent>();
    let (cover_tx, mut cover_rx) = mpsc::unbounded_channel::<CoverLoaded>();
    let (persist_tx, mut persist_rx) = mpsc::unbounded_channel::<PersistOp>();

    let persist_cache_dir = resolved_cache_dir.clone();
    let persist_app_tx = app_tx.clone();
    let persist_handle = tokio::spawn(async move {
        while let Some(op) = persist_rx.recv().await {
            match op {
                PersistOp::SaveSnapshot { username, library } => {
                    let _ = save_library_snapshot(
                        persist_cache_dir.clone(),
                        username,
                        library,
                    )
                    .await;
                }
                PersistOp::RemoveSnapshot { username } => {
                    let dir = persist_cache_dir.clone();
                    let _ = tokio::task::spawn_blocking(move || {
                        remove_library_snapshot(&dir, &username);
                    })
                    .await;
                }
                PersistOp::SaveAuthSession(session) => {
                    let res =
                        tokio::task::spawn_blocking(move || save_auth_session(&session)).await;
                    match res {
                        Ok(Ok(())) => {}
                        Ok(Err(err)) => {
                            let _ = persist_app_tx.send(AppEvent::Notice {
                                message: format!("Failed to save auth session: {err}"),
                                level: ToastLevel::Warning,
                            });
                        }
                        Err(err) => {
                            let _ = persist_app_tx.send(AppEvent::Notice {
                                message: format!("Auth persistence task failed: {err}"),
                                level: ToastLevel::Warning,
                            });
                        }
                    }
                }
                PersistOp::SaveAppConfig(cfg) => {
                    let res =
                        tokio::task::spawn_blocking(move || save_app_config(&cfg)).await;
                    match res {
                        Ok(Ok(())) => {}
                        Ok(Err(err)) => {
                            let _ = persist_app_tx.send(AppEvent::Notice {
                                message: format!("Failed to save config: {err}"),
                                level: ToastLevel::Warning,
                            });
                        }
                        Err(err) => {
                            let _ = persist_app_tx.send(AppEvent::Notice {
                                message: format!("Config persistence task failed: {err}"),
                                level: ToastLevel::Warning,
                            });
                        }
                    }
                }
            }
        }
    });

    let mut ctx = RuntimeContext {
        client,
        cover_loader,
        app_tx,
        cover_tx,
        persist_tx: Some(persist_tx),
        pending_progress: HashMap::new(),
    };

    if auth.is_authenticated() {
        if let Some(username) = state.username.clone() {
            let snap_dir = resolved_cache_dir;
            let snap_epoch = state.session_epoch;
            let mut snapshot_is_fresh = false;
            if let Ok(Some(snapshot)) =
                tokio::task::spawn_blocking(move || load_library_snapshot(&snap_dir, &username))
                    .await
            {
                snapshot_is_fresh = is_snapshot_fresh(snapshot.saved_at_unix, current_unix_secs());
                let snap_cmds = state.apply_event(AppEvent::SnapshotLoaded {
                    epoch: snap_epoch,
                    library: snapshot.library,
                });
                execute_commands(&mut state, &mut ctx, snap_cmds);
            }
            if !snapshot_is_fresh {
                state.syncing = true;
                execute_commands(&mut state, &mut ctx, vec![AppCommand::FetchLibrary]);
            }
        }
    }

    let mut event_stream = EventStream::new();
    let mut tick = tokio::time::interval(TICK_INTERVAL);
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

    let mut needs_redraw = true;
    let mut should_quit = false;

    loop {
        if needs_redraw {
            draw_and_schedule_reencode(terminal, &mut state, &mut ctx)?;
            needs_redraw = false;
        }

        if should_quit {
            break;
        }

        tokio::select! {
            maybe_event = event_stream.next() => {
                match maybe_event {
                    Some(Ok(CrosstermEvent::Key(key))) if key.kind == KeyEventKind::Press => {
                        let cmds = state.handle_key(key);
                        if execute_commands(&mut state, &mut ctx, cmds) {
                            should_quit = true;
                        }
                        needs_redraw = true;
                    }
                    Some(Ok(CrosstermEvent::Resize(_, _))) => {
                        needs_redraw = true;
                    }
                    Some(Ok(_)) => {}
                    Some(Err(_)) | None => {
                        should_quit = true;
                    }
                }
            }
            Some(event) = app_rx.recv() => {
                let cmds = state.apply_event(event);
                if execute_commands(&mut state, &mut ctx, cmds) {
                    should_quit = true;
                }
                needs_redraw = true;
            }
            Some(loaded) = cover_rx.recv() => {
                let media_id = loaded.media_id;
                let cmds = state.apply_event(AppEvent::CoverLoaded(loaded));
                if execute_commands(&mut state, &mut ctx, cmds) {
                    should_quit = true;
                }
                if state.selected_item().is_some_and(|item| item.id == media_id) {
                    needs_redraw = true;
                }
            }
            _ = tick.tick() => {
                let had_toast = state.toast.is_some();
                let was_syncing = state.syncing;
                let cmds = state.apply_event(AppEvent::Tick);
                if execute_commands(&mut state, &mut ctx, cmds) {
                    should_quit = true;
                }
                flush_expired_progress(&state, &mut ctx);
                if was_syncing || (had_toast && state.toast.is_none()) {
                    needs_redraw = true;
                }
            }
        }
    }

    flush_all_progress_on_quit(&state, &mut ctx).await;
    ctx.persist_tx.take();
    let _ = tokio::time::timeout(Duration::from_secs(1), persist_handle).await;
    Ok(())
}

fn draw_and_schedule_reencode<B: Backend>(
    terminal: &mut Terminal<B>,
    state: &mut AppState,
    ctx: &mut RuntimeContext,
) -> anyhow::Result<()> {
    terminal.draw(|frame| crate::ui::render(frame, state))?;

    if let Some((media_id, area)) = state.pending_reencode.take() {
        if area.width > 0 && area.height > 0 {
            if let Some(proto) = state.cover_cache.take(media_id) {
                if state.cover_cache.mark_in_flight(media_id) {
                    CoverLoader::reencode_cover(media_id, proto, area, ctx.cover_tx.clone());
                } else {
                    state.cover_cache.insert(media_id, proto);
                }
            }
        }
    }

    Ok(())
}

fn execute_commands(
    state: &mut AppState,
    ctx: &mut RuntimeContext,
    commands: Vec<AppCommand>,
) -> bool {
    let mut should_quit = false;
    let current_epoch = state.session_epoch;
    ctx.pending_progress.retain(|_, p| p.epoch == current_epoch);

    for cmd in commands {
        match cmd {
            AppCommand::Authenticate { client_id, token } => {
                let client = ctx.client.clone();
                let app_tx = ctx.app_tx.clone();
                tokio::spawn(async move {
                    match client.fetch_viewer_name(&token).await {
                        Ok(username) => {
                            let _ = app_tx.send(AppEvent::AuthSucceeded {
                                client_id,
                                token,
                                username,
                                rate_limit: client.rate_limit_info(),
                            });
                        }
                        Err(err) => {
                            let _ = app_tx.send(AppEvent::AuthFailed {
                                error: format_anilist_error(&err),
                                rate_limit: client.rate_limit_info(),
                            });
                        }
                    }
                });
            }
            AppCommand::FetchLibrary => {
                let Some(username) = state.username.clone() else {
                    state.syncing = false;
                    continue;
                };
                state.cover_cache.clear_failures();
                let token = state.auth_session.access_token.clone();
                let client = ctx.client.clone();
                let app_tx = ctx.app_tx.clone();
                tokio::spawn(async move {
                    match client.fetch_full_library(&username, token.as_deref()).await {
                        Ok(library) => {
                            let _ = app_tx.send(AppEvent::LibraryLoaded {
                                epoch: current_epoch,
                                library,
                                rate_limit: client.rate_limit_info(),
                            });
                        }
                        Err(err) => {
                            let _ = app_tx.send(AppEvent::LibraryLoadFailed {
                                epoch: current_epoch,
                                is_auth_error: err.is_auth_error(),
                                error: format_anilist_error(&err),
                                rate_limit: client.rate_limit_info(),
                            });
                        }
                    }
                });
            }
            AppCommand::ScheduleProgress {
                media_id,
                progress,
                rollback,
            } => {
                let deadline = tokio::time::Instant::now() + PROGRESS_DEBOUNCE;
                ctx.pending_progress.insert(
                    media_id,
                    PendingProgress {
                        epoch: current_epoch,
                        progress,
                        rollback,
                        deadline,
                    },
                );
            }
            AppCommand::SaveStatus {
                media_id,
                status,
                rollback,
            } => {
                let pending_prog = ctx.pending_progress.remove(&media_id).map(|p| p.progress);
                spawn_save_entry(
                    state,
                    ctx,
                    current_epoch,
                    media_id,
                    Some(status),
                    pending_prog,
                    None,
                    rollback,
                );
            }
            AppCommand::SaveScore {
                media_id,
                score,
                rollback,
            } => {
                let pending_prog = ctx.pending_progress.remove(&media_id).map(|p| p.progress);
                spawn_save_entry(
                    state,
                    ctx,
                    current_epoch,
                    media_id,
                    None,
                    pending_prog,
                    Some(score),
                    rollback,
                );
            }
            AppCommand::DeleteEntry {
                media_id,
                entry_id,
                rollback,
            } => {
                ctx.pending_progress.remove(&media_id);
                let Some(token) = state.auth_session.access_token.clone() else {
                    let _ = ctx.app_tx.send(AppEvent::MutationFailed {
                        epoch: current_epoch,
                        media_id,
                        rollback,
                        error: "Missing AniList access token.".to_owned(),
                        is_auth_error: true,
                        retry_after_secs: None,
                        rate_limit: ctx.client.rate_limit_info(),
                    });
                    continue;
                };
                let client = ctx.client.clone();
                let app_tx = ctx.app_tx.clone();
                tokio::spawn(async move {
                    match client.delete_entry(&token, entry_id).await {
                        Ok(true) => {
                            let _ = app_tx.send(AppEvent::EntryDeleted {
                                epoch: current_epoch,
                                media_id,
                                rollback,
                                rate_limit: client.rate_limit_info(),
                            });
                        }
                        Ok(false) => {
                            let _ = app_tx.send(AppEvent::MutationFailed {
                                epoch: current_epoch,
                                media_id,
                                rollback,
                                error: "AniList did not confirm entry deletion.".to_owned(),
                                is_auth_error: false,
                                retry_after_secs: None,
                                rate_limit: client.rate_limit_info(),
                            });
                        }
                        Err(err) => {
                            let _ = app_tx.send(AppEvent::MutationFailed {
                                epoch: current_epoch,
                                media_id,
                                rollback,
                                is_auth_error: err.is_auth_error(),
                                retry_after_secs: extract_retry_after(&err),
                                error: format_anilist_error(&err),
                                rate_limit: client.rate_limit_info(),
                            });
                        }
                    }
                });
            }
            AppCommand::EnsureCovers(targets) => {
                let area = if state.last_cover_area.width > 0 && state.last_cover_area.height > 0 {
                    state.last_cover_area
                } else {
                    DEFAULT_COVER_AREA
                };
                for (media_id, url) in targets {
                    // Touch cached entries so the visible window stays hot in LRU, then
                    // enqueue uncached covers only once via CoverCache deduplication.
                    if state.cover_cache.get_mut(media_id).is_none()
                        && state.cover_cache.mark_in_flight(media_id)
                    {
                        ctx.cover_loader
                            .request_cover(media_id, url, area, ctx.cover_tx.clone());
                    }
                }
            }
            AppCommand::SaveSnapshot => {
                if let (Some(username), Some(tx)) =
                    (state.username.clone(), ctx.persist_tx.as_ref())
                {
                    let _ = tx.send(PersistOp::SaveSnapshot {
                        username,
                        library: state.library.clone(),
                    });
                }
            }
            AppCommand::RemoveSnapshot(username) => {
                ctx.pending_progress.clear();
                if let Some(tx) = ctx.persist_tx.as_ref() {
                    let _ = tx.send(PersistOp::RemoveSnapshot { username });
                }
            }
            AppCommand::SaveAuthSession(session) => {
                if let Some(tx) = ctx.persist_tx.as_ref() {
                    let _ = tx.send(PersistOp::SaveAuthSession(session));
                }
            }
            AppCommand::SaveAppConfig(cfg) => {
                if let Some(tx) = ctx.persist_tx.as_ref() {
                    let _ = tx.send(PersistOp::SaveAppConfig(cfg));
                }
            }
            AppCommand::OpenUrl(url) => {
                let app_tx = ctx.app_tx.clone();
                tokio::task::spawn_blocking(move || {
                    if let Err(err) = open::that_detached(&url) {
                        let _ = app_tx.send(AppEvent::Notice {
                            message: format!("Could not open browser: {err}"),
                            level: ToastLevel::Warning,
                        });
                    }
                });
            }
            AppCommand::Quit => {
                should_quit = true;
            }
        }
    }

    should_quit
}

#[allow(
    clippy::too_many_arguments,
    reason = "internal helper forwards mutation parameters from command dispatch"
)]
fn spawn_save_entry(
    state: &AppState,
    ctx: &RuntimeContext,
    epoch: u64,
    media_id: MediaId,
    status: Option<LibraryStatus>,
    progress: Option<u32>,
    score: Option<ScoreUpdate>,
    rollback: PendingRollback,
) {
    let Some(token) = state.auth_session.access_token.clone() else {
        let _ = ctx.app_tx.send(AppEvent::MutationFailed {
            epoch,
            media_id,
            rollback,
            error: "Missing AniList access token.".to_owned(),
            is_auth_error: true,
            retry_after_secs: None,
            rate_limit: ctx.client.rate_limit_info(),
        });
        return;
    };

    let client = ctx.client.clone();
    let app_tx = ctx.app_tx.clone();
    tokio::spawn(async move {
        match client
            .save_entry(&token, media_id, status, progress, score)
            .await
        {
            Ok(entry) => {
                let _ = app_tx.send(AppEvent::EntrySaved {
                    epoch,
                    media_id,
                    entry,
                    rollback,
                    rate_limit: client.rate_limit_info(),
                });
            }
            Err(err) => {
                let _ = app_tx.send(AppEvent::MutationFailed {
                    epoch,
                    media_id,
                    rollback,
                    is_auth_error: err.is_auth_error(),
                    retry_after_secs: extract_retry_after(&err),
                    error: format_anilist_error(&err),
                    rate_limit: client.rate_limit_info(),
                });
            }
        }
    });
}

fn flush_expired_progress(state: &AppState, ctx: &mut RuntimeContext) {
    let current_epoch = state.session_epoch;
    ctx.pending_progress.retain(|_, p| p.epoch == current_epoch);

    let now = tokio::time::Instant::now();
    let expired_ids: Vec<MediaId> = ctx
        .pending_progress
        .iter()
        .filter_map(|(&media_id, pending)| (pending.deadline <= now).then_some(media_id))
        .collect();

    for media_id in expired_ids {
        if let Some(pending) = ctx.pending_progress.remove(&media_id) {
            spawn_save_entry(
                state,
                ctx,
                pending.epoch,
                media_id,
                None,
                Some(pending.progress),
                None,
                pending.rollback,
            );
        }
    }
}

async fn flush_all_progress_on_quit(state: &AppState, ctx: &mut RuntimeContext) {
    let current_epoch = state.session_epoch;
    ctx.pending_progress.retain(|_, p| p.epoch == current_epoch);
    if ctx.pending_progress.is_empty() {
        return;
    }
    let Some(token) = state.auth_session.access_token.clone() else {
        ctx.pending_progress.clear();
        return;
    };

    let pending: Vec<(MediaId, PendingProgress)> = ctx.pending_progress.drain().collect();
    let client = ctx.client.clone();
    let flush = async move {
        for (idx, (media_id, item)) in pending.into_iter().enumerate() {
            if idx > 0 {
                tokio::time::sleep(QUIT_FLUSH_PACE_DELAY).await;
            }
            let _ = client
                .save_entry(&token, media_id, None, Some(item.progress), None)
                .await;
        }
    };

    let _ = tokio::time::timeout(QUIT_FLUSH_TIMEOUT, flush).await;
}

fn current_unix_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_secs())
}

const fn is_snapshot_fresh(saved_at_unix: u64, now_unix: u64) -> bool {
    now_unix >= saved_at_unix && (now_unix - saved_at_unix) < STARTUP_SNAPSHOT_FRESHNESS_SECS
}

fn extract_retry_after(err: &AnilistError) -> Option<u64> {
    match err {
        AnilistError::RateLimited { retry_after_secs } => Some(*retry_after_secs),
        _ => None,
    }
}

fn format_anilist_error(err: &AnilistError) -> String {
    const MAX_LEN: usize = 140;
    let raw = err.to_string();
    match raw.char_indices().nth(MAX_LEN) {
        Some((idx, _)) => format!("{}…", &raw[..idx]),
        None => raw,
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;
    use std::num::NonZeroU32;

    use super::*;

    fn media_id(id: u32) -> MediaId {
        MediaId(NonZeroU32::new(id).unwrap())
    }

    fn entry_id(id: u32) -> EntryId {
        EntryId(NonZeroU32::new(id).unwrap())
    }

    fn key(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }

    fn ctrl(ch: char) -> KeyEvent {
        KeyEvent::new(KeyCode::Char(ch), KeyModifiers::CONTROL)
    }

    fn sample_item(
        id: u32,
        title: &str,
        alt_title: Option<&str>,
        status: LibraryStatus,
        progress: u32,
        episodes: Option<u32>,
        score: Option<ScoreRaw>,
    ) -> AnimeItem {
        AnimeItem::new(
            media_id(id),
            title.to_owned(),
            alt_title.map(str::to_owned),
            episodes,
            "FINISHED".to_owned(),
            Some(88),
            vec!["Adventure".to_owned(), "Drama".to_owned()],
            "Synopsis text".to_owned(),
            Some(format!("https://img.example.com/{id}.jpg")),
            Some(format!("https://anilist.co/anime/{id}")),
            LibraryEntry {
                id: Some(entry_id(id + 1000)),
                status,
                progress,
                score,
            },
        )
    }

    fn authenticated_state(library: LibraryCollection) -> AppState {
        let auth = AuthSession {
            access_token: Some("tok-abc".to_owned()),
            client_id: Some("cid-999".to_owned()),
            username: Some("nkaet".to_owned()),
        };
        let mut state = AppState::new(ThemePalette::from_name("catppuccin-mocha"), auth);
        state.library = library;
        state.rebuild_visible_rows();
        state
    }

    fn sample_library() -> LibraryCollection {
        let mut lists = HashMap::new();
        lists.insert(
            LibraryStatus::Watching,
            vec![
                sample_item(
                    1,
                    "Frieren: Beyond Journey's End",
                    Some("Sousou no Frieren"),
                    LibraryStatus::Watching,
                    10,
                    Some(28),
                    ScoreRaw::new(98),
                ),
                sample_item(
                    2,
                    "Dandadan",
                    None,
                    LibraryStatus::Watching,
                    4,
                    Some(12),
                    None,
                ),
            ],
        );
        lists.insert(
            LibraryStatus::Planning,
            vec![sample_item(
                3,
                "Delicious in Dungeon",
                Some("Dungeon Meshi"),
                LibraryStatus::Planning,
                0,
                Some(24),
                None,
            )],
        );
        lists.insert(
            LibraryStatus::Completed,
            vec![sample_item(
                4,
                "Steins;Gate",
                None,
                LibraryStatus::Completed,
                24,
                Some(24),
                Some(Point3Score::Happy.as_score_raw()),
            )],
        );
        LibraryCollection::from_lists(lists)
    }

    #[test]
    fn list_switching_keys_1_to_6_and_sidebar_logout() {
        let mut state = authenticated_state(sample_library());
        assert_eq!(state.active_status, LibraryStatus::Watching);
        assert_eq!(state.visible_len(), 2);

        let cmds = state.handle_key(key(KeyCode::Char('2')));
        assert_eq!(state.active_status, LibraryStatus::Planning);
        assert_eq!(state.sidebar_index, 1);
        assert_eq!(state.visible_len(), 1);
        assert_eq!(state.selected_item().unwrap().title, "Delicious in Dungeon");
        assert!(matches!(cmds.as_slice(), [AppCommand::EnsureCovers(_)]));

        let _ = state.handle_key(key(KeyCode::Char('3')));
        assert_eq!(state.active_status, LibraryStatus::Completed);
        assert_eq!(state.sidebar_index, 2);
        assert_eq!(state.selected_item().unwrap().title, "Steins;Gate");

        // Switch focus right to Sidebar (Collections) and move down to LogOut (index 6).
        state.handle_key(key(KeyCode::Char('l')));
        assert_eq!(state.focus, FocusPane::Sidebar);
        state.handle_key(key(KeyCode::Char('h')));
        assert_eq!(state.focus, FocusPane::Table);
        state.handle_key(key(KeyCode::Char('l')));
        assert_eq!(state.focus, FocusPane::Sidebar);
        for _ in 0..4 {
            state.handle_key(key(KeyCode::Char('j')));
        }
        assert_eq!(state.sidebar_index, 6);
        assert!(matches!(state.screen, Screen::Library));

        let cmds = state.handle_key(key(KeyCode::Enter));
        assert!(matches!(state.screen, Screen::Auth(_)));
        assert!(state.username.is_none());
        assert!(state.library.is_empty());
        assert_eq!(state.visible_len(), 0);
        if let Screen::Auth(auth) = &state.screen {
            assert_eq!(auth.client_id, "cid-999");
            assert!(auth.token.is_empty());
        } else {
            panic!("expected Auth screen after logout");
        }
        assert!(cmds.iter().any(|c| matches!(
            c,
            AppCommand::SaveAuthSession(s) if s.access_token.is_none() && s.client_id.as_deref() == Some("cid-999")
        )));
        assert!(cmds
            .iter()
            .any(|c| matches!(c, AppCommand::RemoveSnapshot(u) if u == "nkaet")));
    }

    #[test]
    fn local_filter_searches_current_and_all_lists_including_alt_title() {
        let mut state = authenticated_state(sample_library());

        state.handle_key(key(KeyCode::Char('/')));
        assert!(state.filter_active);

        // Search "sousou" (alt_title of Frieren in Watching).
        for ch in "sousou".chars() {
            let cmds = state.handle_key(key(KeyCode::Char(ch)));
            assert!(cmds.iter().all(|c| !matches!(c, AppCommand::FetchLibrary)));
        }
        assert_eq!(state.visible_len(), 1);
        assert_eq!(
            state.selected_item().unwrap().title,
            "Frieren: Beyond Journey's End"
        );

        // Clear and search "meshi" (Planning alt_title) in CurrentList -> 0 hits, then Ctrl+A -> 1 hit.
        state.handle_key(key(KeyCode::Esc));
        assert!(!state.filter_active);
        assert!(state.filter_query.is_empty());
        assert_eq!(state.visible_len(), 2);

        state.handle_key(key(KeyCode::Char('/')));
        for ch in "meshi".chars() {
            state.handle_key(key(KeyCode::Char(ch)));
        }
        assert_eq!(state.visible_len(), 0);

        state.handle_key(ctrl('a'));
        assert_eq!(state.filter_scope, FilterScope::AllLists);
        assert_eq!(state.visible_len(), 1);
        assert_eq!(state.selected_item().unwrap().title, "Delicious in Dungeon");

        state.handle_key(key(KeyCode::Enter));
        assert!(!state.filter_active);
        assert_eq!(state.focus, FocusPane::Table);

        // Esc outside typing clears non-empty filter.
        state.handle_key(key(KeyCode::Esc));
        assert!(state.filter_query.is_empty());
        assert_eq!(state.visible_len(), 4);
    }

    #[test]
    fn optimistic_progress_clamps_preserves_rollback_and_recovers_on_failure() {
        let mut state = authenticated_state(sample_library());
        // Select Dandadan (progress 4, episodes 12).
        state.handle_key(key(KeyCode::Char('j')));
        assert_eq!(state.selected_item().unwrap().id, media_id(2));

        let cmds1 = state.handle_key(key(KeyCode::Char('+')));
        assert_eq!(state.selected_item().unwrap().entry.progress, 5);
        let cmds2 = state.handle_key(key(KeyCode::Char('i')));
        assert_eq!(state.selected_item().unwrap().entry.progress, 6);

        let rollback2 = match cmds2.as_slice() {
            [AppCommand::ScheduleProgress {
                media_id: id,
                progress: 6,
                rollback,
            }] => {
                assert_eq!(*id, media_id(2));
                rollback.clone()
            }
            other => panic!("unexpected commands: {other:?}"),
        };
        // Rapid burst preserves original pre-burst progress (4) for rollback.
        assert!(matches!(
            rollback2,
            PendingRollback::Progress {
                previous_progress: 4,
                ..
            }
        ));
        assert_ne!(
            match &cmds1[0] {
                AppCommand::ScheduleProgress { rollback, .. } => rollback.seq(),
                _ => 0,
            },
            rollback2.seq()
        );

        // Simulate background LibraryLoaded arriving while optimistic edit is in-flight: must not clobber 6!
        let stale_sync = sample_library();
        let epoch = state.session_epoch();
        state.apply_event(AppEvent::LibraryLoaded {
            epoch,
            library: stale_sync,
            rate_limit: RateLimitInfo::default(),
        });
        assert_eq!(state.library.get(media_id(2)).unwrap().entry.progress, 6);

        // Simulate rate-limit failure -> rolls back to 4 and shows warning toast.
        state.apply_event(AppEvent::MutationFailed {
            epoch,
            media_id: media_id(2),
            rollback: rollback2,
            error: "rate limited".to_owned(),
            is_auth_error: false,
            retry_after_secs: Some(42),
            rate_limit: RateLimitInfo {
                limit: Some(30),
                remaining: Some(0),
                reset_at_unix: Some(1_700_000_042),
            },
        });
        assert_eq!(state.library.get(media_id(2)).unwrap().entry.progress, 4);
        let toast = state.active_toast().unwrap();
        assert_eq!(toast.level, ToastLevel::Warning);
        assert!(toast.message.contains("42s"));

        // Clamp upper bound on Steins;Gate (24 / 24) and lower bound at 0 on Planning (0 / 24).
        state.handle_key(key(KeyCode::Char('3')));
        assert!(state.handle_key(key(KeyCode::Char('+'))).is_empty());
        assert_eq!(state.selected_item().unwrap().entry.progress, 24);

        state.handle_key(key(KeyCode::Char('2')));
        assert!(state.handle_key(key(KeyCode::Char('-'))).is_empty());
        assert_eq!(state.selected_item().unwrap().entry.progress, 0);
    }

    #[test]
    fn status_picker_and_server_auto_status_reconciliation() {
        let mut state = authenticated_state(sample_library());
        // Select Dandadan (Watching, index 1).
        state.handle_key(key(KeyCode::Char('j')));
        assert_eq!(state.selected_item().unwrap().id, media_id(2));

        // Open StatusPicker -> preselected at Watching (0).
        state.handle_key(key(KeyCode::Char('a')));
        assert_eq!(state.modal, ActiveModal::StatusPicker { selected: 0 });
        assert_eq!(state.active_modal_options_count(), 7);

        // Move down twice to Completed (2) and confirm.
        state.handle_key(key(KeyCode::Char('j')));
        state.handle_key(key(KeyCode::Char('j')));
        let cmds = state.handle_key(key(KeyCode::Enter));
        assert_eq!(state.modal, ActiveModal::None);
        assert_eq!(
            state.library.get(media_id(2)).unwrap().entry.status,
            LibraryStatus::Completed
        );
        assert_eq!(state.library.count_for(LibraryStatus::Watching), 1);

        // Now test server auto-status reconciliation on EntrySaved (e.g. Planning -> Watching).
        state.handle_key(key(KeyCode::Char('2')));
        assert_eq!(state.selected_item().unwrap().id, media_id(3));
        let prog_cmds = state.handle_key(key(KeyCode::Char('+')));
        let rollback = match prog_cmds.as_slice() {
            [AppCommand::ScheduleProgress { rollback, .. }] => rollback.clone(),
            other => panic!("expected ScheduleProgress, got {other:?}"),
        };

        let epoch = state.session_epoch();
        let follow_up = state.apply_event(AppEvent::EntrySaved {
            epoch,
            media_id: media_id(3),
            entry: LibraryEntry {
                id: Some(entry_id(1003)),
                status: LibraryStatus::Watching,
                progress: 1,
                score: None,
            },
            rollback,
            rate_limit: RateLimitInfo::default(),
        });
        assert_eq!(state.library.count_for(LibraryStatus::Planning), 0);
        assert_eq!(
            state.library.get(media_id(3)).unwrap().entry.status,
            LibraryStatus::Watching
        );
        assert!(follow_up
            .iter()
            .any(|c| matches!(c, AppCommand::SaveSnapshot)));
        assert!(cmds.iter().any(|c| matches!(
            c,
            AppCommand::SaveStatus {
                status: LibraryStatus::Completed,
                ..
            }
        )));
    }

    #[test]
    fn remove_from_library_optimistic_delete_and_rollback_restoration() {
        let mut state = authenticated_state(sample_library());
        // Select Dandadan at Watching index 1.
        state.handle_key(key(KeyCode::Char('j')));
        assert_eq!(state.selected_item().unwrap().id, media_id(2));

        state.handle_key(key(KeyCode::Char('a')));
        for _ in 0..6 {
            state.handle_key(key(KeyCode::Down));
        }
        assert_eq!(state.modal, ActiveModal::StatusPicker { selected: 6 });

        let cmds = state.handle_key(key(KeyCode::Enter));
        assert!(state.library.get(media_id(2)).is_none());
        assert_eq!(state.library.count_for(LibraryStatus::Watching), 1);

        let rollback = match cmds.first() {
            Some(AppCommand::DeleteEntry {
                media_id: id,
                entry_id: eid,
                rollback,
            }) => {
                assert_eq!(*id, media_id(2));
                assert_eq!(*eid, entry_id(1002));
                rollback.clone()
            }
            other => panic!("expected DeleteEntry, got {other:?}"),
        };

        // Rollback restores Dandadan at Watching index 1.
        let epoch = state.session_epoch();
        state.apply_event(AppEvent::MutationFailed {
            epoch,
            media_id: media_id(2),
            rollback,
            error: "network error".to_owned(),
            is_auth_error: false,
            retry_after_secs: None,
            rate_limit: RateLimitInfo::default(),
        });
        assert_eq!(state.library.count_for(LibraryStatus::Watching), 2);
        assert_eq!(
            state.library.items_for(LibraryStatus::Watching)[1].id,
            media_id(2)
        );
        assert_eq!(state.active_toast().unwrap().level, ToastLevel::Error);
    }

    #[test]
    fn rating_picker_sets_and_clears_decimal_point3_and_point5_with_rollback() {
        // Default fallback without API scoreFormat is 9.8/10 decimal input.
        let mut state = authenticated_state(sample_library());
        assert_eq!(state.effective_score_format(), ScoreFormat::Point10Decimal);

        state.handle_key(key(KeyCode::Char('r')));
        assert_eq!(state.rating_input, "9.8");

        // Typing replaces prefilled pristine text cleanly, and invalid input keeps modal open.
        state.handle_key(key(KeyCode::Char('1')));
        state.handle_key(key(KeyCode::Char('2')));
        state.handle_key(key(KeyCode::Enter));
        assert!(matches!(state.modal, ActiveModal::RatingPicker { .. }));
        assert!(state.rating_input_error.is_some());

        // Enter valid 8.7 decimal score.
        state.handle_key(key(KeyCode::Backspace));
        state.handle_key(key(KeyCode::Backspace));
        for ch in ['8', '.', '7'] {
            state.handle_key(key(KeyCode::Char(ch)));
        }
        let cmds = state.handle_key(key(KeyCode::Enter));
        assert_eq!(state.modal, ActiveModal::None);
        assert_eq!(
            state.selected_item().unwrap().entry.score,
            ScoreRaw::new(87)
        );
        let rollback = match cmds.as_slice() {
            [AppCommand::SaveScore {
                media_id: id,
                score: ScoreUpdate::Set(raw),
                rollback,
            }] => {
                assert_eq!(*id, media_id(1));
                assert_eq!(raw.get(), 87);
                rollback.clone()
            }
            other => panic!("expected SaveScore(87), got {other:?}"),
        };

        let epoch = state.session_epoch();
        state.apply_event(AppEvent::EntrySaved {
            epoch,
            media_id: media_id(1),
            entry: LibraryEntry {
                id: Some(entry_id(1001)),
                status: LibraryStatus::Watching,
                progress: 10,
                score: ScoreRaw::new(87),
            },
            rollback,
            rate_limit: RateLimitInfo::default(),
        });

        // Clear with 'c', then simulate failure rolling back to 87.
        state.handle_key(key(KeyCode::Char('r')));
        let clear_cmds = state.handle_key(key(KeyCode::Char('c')));
        assert_eq!(state.selected_item().unwrap().entry.score, None);
        let clear_rb = match clear_cmds.as_slice() {
            [AppCommand::SaveScore {
                score: ScoreUpdate::Clear,
                rollback,
                ..
            }] => rollback.clone(),
            other => panic!("expected SaveScore(Clear), got {other:?}"),
        };

        state.apply_event(AppEvent::MutationFailed {
            epoch,
            media_id: media_id(1),
            rollback: clear_rb,
            error: "500".to_owned(),
            is_auth_error: false,
            retry_after_secs: None,
            rate_limit: RateLimitInfo::default(),
        });
        assert_eq!(
            state.selected_item().unwrap().entry.score,
            ScoreRaw::new(87)
        );

        // API-provided Point3 switches rating modal to the 4-option smiley picker.
        let mut api_lib = sample_library();
        api_lib.score_format = Some(ScoreFormat::Point3);
        state.apply_event(AppEvent::LibraryLoaded {
            epoch,
            library: api_lib,
            rate_limit: RateLimitInfo::default(),
        });
        assert_eq!(state.effective_score_format(), ScoreFormat::Point3);
        state.handle_key(key(KeyCode::Char('r')));
        assert_eq!(state.modal, ActiveModal::RatingPicker { selected: 0 });
        assert_eq!(state.active_modal_options_count(), 4);
        state.handle_key(key(KeyCode::Char('j')));
        state.handle_key(key(KeyCode::Char('j')));
        let sad_cmds = state.handle_key(key(KeyCode::Enter));
        assert!(matches!(
            sad_cmds.as_slice(),
            [AppCommand::SaveScore {
                score: ScoreUpdate::Set(raw),
                ..
            }] if *raw == Point3Score::Sad.as_score_raw()
        ));

        // Help & Settings (?) cycles rating system (s) to Point5 (5★) and theme (t) to Terminal.
        state.handle_key(key(KeyCode::Char('?')));
        assert_eq!(state.modal, ActiveModal::Help);

        // Cycle theme: anilist -> terminal (transparent).
        assert_eq!(state.theme_name(), "anilist");
        let theme_cmds = state.handle_key(key(KeyCode::Char('t')));
        assert_eq!(state.theme_name(), "terminal");
        assert!(matches!(
            theme_cmds.as_slice(),
            [AppCommand::SaveAppConfig(cfg)] if cfg.theme == "terminal"
        ));

        // Cycle rating system: Auto -> Point10Decimal -> Point100 -> Point10 -> Point5.
        let mut cfg_cmds = Vec::new();
        for _ in 0..4 {
            cfg_cmds = state.handle_key(key(KeyCode::Char('s')));
        }
        state.handle_key(key(KeyCode::Esc));
        assert_eq!(state.modal, ActiveModal::None);
        assert_eq!(
            state.score_format_pref,
            ScoreFormatPreference::Override(ScoreFormat::Point5)
        );
        assert_eq!(state.effective_score_format(), ScoreFormat::Point5);
        assert!(matches!(
            cfg_cmds.as_slice(),
            [AppCommand::SaveAppConfig(cfg)]
                if cfg.score_format == ScoreFormatPreference::Override(ScoreFormat::Point5)
        ));

        state.handle_key(key(KeyCode::Char('r')));
        assert_eq!(state.active_modal_options_count(), 6);
        let star_cmds = state.handle_key(key(KeyCode::Char('4')));
        assert!(matches!(
            star_cmds.as_slice(),
            [AppCommand::SaveScore {
                score: ScoreUpdate::Set(raw),
                ..
            }] if raw.get() == 70
        ));
    }

    #[test]
    fn auth_screen_validation_field_cycling_and_submission() {
        let mut state = AppState::new(
            ThemePalette::from_name("nord"),
            AuthSession::default(),
        );
        let Screen::Auth(auth) = &state.screen else {
            panic!("expected Auth screen");
        };
        assert_eq!(auth.focused, AuthField::ClientId);
        assert!(auth.oauth_url_preview().is_none());

        // Ctrl+O without client ID sets inline error.
        let cmds = state.handle_key(ctrl('o'));
        assert!(cmds.is_empty());
        if let Screen::Auth(auth) = &state.screen {
            assert!(auth.error.is_some());
        }

        // Cycle forward and backward.
        state.handle_key(key(KeyCode::Tab));
        assert!(matches!(
            state.screen,
            Screen::Auth(AuthScreenState {
                focused: AuthField::OpenUrl,
                ..
            })
        ));
        state.handle_key(key(KeyCode::Tab));
        assert!(matches!(
            state.screen,
            Screen::Auth(AuthScreenState {
                focused: AuthField::Token,
                ..
            })
        ));
        state.handle_key(key(KeyCode::Tab));
        assert!(matches!(
            state.screen,
            Screen::Auth(AuthScreenState {
                focused: AuthField::Submit,
                ..
            })
        ));
        state.handle_key(key(KeyCode::BackTab));
        assert!(matches!(
            state.screen,
            Screen::Auth(AuthScreenState {
                focused: AuthField::Token,
                ..
            })
        ));
        state.handle_key(key(KeyCode::Up));
        state.handle_key(key(KeyCode::Up));
        assert!(matches!(
            state.screen,
            Screen::Auth(AuthScreenState {
                focused: AuthField::ClientId,
                ..
            })
        ));

        // Type Client ID (including 'q' which must not quit while typing).
        for ch in "123q".chars() {
            let cmds = state.handle_key(key(KeyCode::Char(ch)));
            assert!(cmds.is_empty());
        }
        state.handle_key(key(KeyCode::Backspace));
        if let Screen::Auth(auth) = &state.screen {
            assert_eq!(auth.client_id, "123");
            assert_eq!(
                auth.oauth_url_preview().as_deref(),
                Some("https://anilist.co/api/v2/oauth/authorize?client_id=123&response_type=token")
            );
        }

        // Open URL via Ctrl+O.
        let open_cmds = state.handle_key(ctrl('o'));
        assert!(matches!(
            open_cmds.as_slice(),
            [AppCommand::OpenUrl(url)] if url.contains("client_id=123")
        ));

        // Advance to Token, try Submit with blank token -> error.
        state.handle_key(key(KeyCode::Enter)); // OpenUrl
        state.handle_key(key(KeyCode::Tab)); // Token
        state.handle_key(key(KeyCode::Enter)); // Submit (token blank)
        let bad_submit = state.handle_key(key(KeyCode::Enter));
        assert!(bad_submit.is_empty());
        if let Screen::Auth(auth) = &state.screen {
            assert!(!auth.submitting);
            assert!(auth.error.is_some());
        }

        // Move back to Token, enter token, submit via Enter.
        state.handle_key(key(KeyCode::BackTab));
        for ch in "secret-tok".chars() {
            state.handle_key(key(KeyCode::Char(ch)));
        }
        let submit_cmds = state.handle_key(key(KeyCode::Enter));
        assert!(matches!(
            submit_cmds.as_slice(),
            [AppCommand::Authenticate { client_id, token }]
                if client_id == "123" && token == "secret-tok"
        ));
        if let Screen::Auth(auth) = &state.screen {
            assert!(auth.submitting);
            assert!(auth.error.is_none());
        }

        // AuthFailed clears submitting and records error.
        state.apply_event(AppEvent::AuthFailed {
            error: "HTTP 401".to_owned(),
            rate_limit: RateLimitInfo::default(),
        });
        if let Screen::Auth(auth) = &state.screen {
            assert!(!auth.submitting);
            assert_eq!(auth.error.as_deref(), Some("HTTP 401"));
        }

        // AuthSucceeded transitions to Library screen.
        let ok_cmds = state.apply_event(AppEvent::AuthSucceeded {
            client_id: "123".to_owned(),
            token: "secret-tok".to_owned(),
            username: "nkaet".to_owned(),
            rate_limit: RateLimitInfo::default(),
        });
        assert_eq!(state.screen, Screen::Library);
        assert_eq!(state.username.as_deref(), Some("nkaet"));
        assert!(state.syncing);
        assert!(ok_cmds
            .iter()
            .any(|c| matches!(c, AppCommand::SaveAuthSession(_))));
        assert!(ok_cmds
            .iter()
            .any(|c| matches!(c, AppCommand::FetchLibrary)));
    }

    #[test]
    fn access_token_input_accepts_raw_headers_and_callback_urls() {
        assert_eq!(extract_access_token("  eyJ.a.b  ").as_deref(), Some("eyJ.a.b"));
        assert_eq!(extract_access_token("\"eyJ.a.b\"").as_deref(), Some("eyJ.a.b"));
        assert_eq!(extract_access_token("Bearer eyJ.a.b").as_deref(), Some("eyJ.a.b"));
        assert_eq!(extract_access_token("bearer eyJ.a.b").as_deref(), Some("eyJ.a.b"));
        assert_eq!(
            extract_access_token(
                "https://anilist.co/api/v2/oauth/pin#access_token=eyJ.a.b&token_type=Bearer"
            )
            .as_deref(),
            Some("eyJ.a.b")
        );
        assert_eq!(
            extract_access_token("https://example.com/callback?access_token=eyJ.a.b").as_deref(),
            Some("eyJ.a.b")
        );
        assert_eq!(extract_access_token("   "), None);
        assert_eq!(
            extract_access_token(
                "https://anilist.co/api/v2/oauth/pin#error=unsupported_grant_type"
            ),
            None
        );
    }

    #[test]
    fn auth_submit_extracts_token_from_callback_url_and_explains_grant_errors() {
        let mut auth = AuthScreenState {
            client_id: "123".to_owned(),
            token: "https://anilist.co/api/v2/oauth/pin#access_token=secret-tok".to_owned(),
            focused: AuthField::Token,
            submitting: false,
            error: None,
        };

        let cmds = submit_auth_form(&mut auth);
        assert!(matches!(
            cmds.as_slice(),
            [AppCommand::Authenticate { client_id, token }]
                if client_id == "123" && token == "secret-tok"
        ));

        auth.submitting = false;
        auth.token = "https://anilist.co/api/v2/oauth/pin#error=unsupported_grant_type".to_owned();
        let cmds = submit_auth_form(&mut auth);
        assert!(cmds.is_empty());
        assert!(!auth.submitting);
        assert!(auth
            .error
            .as_deref()
            .is_some_and(|error| error.contains("Redirect URL")));
    }

    #[test]
    fn focus_cycling_help_modal_description_scroll_refresh_and_view_url() {
        let mut lib = sample_library();
        if let Some(item) = lib.get_mut(media_id(2)) {
            item.site_url = None;
        }
        let mut state = authenticated_state(lib);

        // Pane cycling: Table -> Details -> Sidebar -> Table.
        assert_eq!(state.focus, FocusPane::Table);
        state.handle_key(key(KeyCode::Tab));
        assert_eq!(state.focus, FocusPane::Details);
        state.handle_key(ctrl('n'));
        assert_eq!(state.focus, FocusPane::Sidebar);
        state.handle_key(ctrl('p'));
        assert_eq!(state.focus, FocusPane::Details);
        state.handle_key(key(KeyCode::BackTab));
        assert_eq!(state.focus, FocusPane::Table);

        // Help modal blocks background shortcuts and toggles cleanly.
        state.handle_key(key(KeyCode::Char('?')));
        assert_eq!(state.modal, ActiveModal::Help);
        assert!(state.handle_key(key(KeyCode::Char('+'))).is_empty());
        state.handle_key(key(KeyCode::Char('?')));
        assert_eq!(state.modal, ActiveModal::None);

        // Description scroll via J/K/PageDown/PageUp and resetting on row move.
        state.handle_key(key(KeyCode::Char('J')));
        assert_eq!(state.description_scroll, 1);
        state.handle_key(key(KeyCode::PageDown));
        assert_eq!(state.description_scroll, 6);
        state.handle_key(key(KeyCode::Char('K')));
        assert_eq!(state.description_scroll, 5);
        state.handle_key(key(KeyCode::PageUp));
        assert_eq!(state.description_scroll, 0);

        // v opens site_url when available, warns when missing.
        let open_cmds = state.handle_key(key(KeyCode::Char('v')));
        assert!(matches!(
            open_cmds.as_slice(),
            [AppCommand::OpenUrl(u)] if u == "https://anilist.co/anime/1"
        ));

        state.handle_key(key(KeyCode::Char('j')));
        let missing_cmds = state.handle_key(key(KeyCode::Char('v')));
        assert!(missing_cmds.is_empty());
        assert_eq!(state.active_toast().unwrap().level, ToastLevel::Warning);

        // R triggers FetchLibrary and deduplicates while a sync is already in-flight.
        let refresh_cmds = state.handle_key(key(KeyCode::Char('R')));
        assert!(state.syncing);
        assert_eq!(refresh_cmds, vec![AppCommand::FetchLibrary]);

        let dup_refresh_cmds = state.handle_key(key(KeyCode::Char('R')));
        assert!(state.syncing);
        assert!(dup_refresh_cmds.is_empty());

        // 401 unauthorized redirects to Auth screen preserving client_id and clears prior library.
        let old_epoch = state.session_epoch();
        let auth_cmds = state.apply_event(AppEvent::LibraryLoadFailed {
            epoch: old_epoch,
            error: "Unauthorized".to_owned(),
            is_auth_error: true,
            rate_limit: RateLimitInfo::default(),
        });
        assert!(!state.syncing);
        assert!(state.library.is_empty());
        assert_eq!(state.visible_len(), 0);
        assert!(matches!(
            state.screen,
            Screen::Auth(AuthScreenState {
                focused: AuthField::Token,
                error: Some(_),
                ..
            })
        ));
        assert!(auth_cmds
            .iter()
            .any(|c| matches!(c, AppCommand::SaveAuthSession(s) if s.access_token.is_none())));
    }

    #[test]
    fn stale_session_events_are_dropped_after_logout_or_reauth() {
        let mut state = authenticated_state(sample_library());
        let old_epoch = state.session_epoch();

        // Start a progress mutation in old session, then log out.
        let cmds = state.handle_key(key(KeyCode::Char('+')));
        let old_rollback = match cmds.as_slice() {
            [AppCommand::ScheduleProgress { rollback, .. }] => rollback.clone(),
            other => panic!("expected ScheduleProgress, got {other:?}"),
        };

        state.handle_key(key(KeyCode::Char('l')));
        for _ in 0..6 {
            state.handle_key(key(KeyCode::Char('j')));
        }
        state.handle_key(key(KeyCode::Enter));
        assert!(matches!(state.screen, Screen::Auth(_)));
        let auth_epoch = state.session_epoch();
        assert_ne!(auth_epoch, old_epoch);

        // User starts typing new token on Auth screen.
        state.handle_key(key(KeyCode::Tab));
        state.handle_key(key(KeyCode::Tab));
        state.handle_key(key(KeyCode::Char('x')));

        // Stale 401 from old session must NOT wipe the typed token or re-redirect.
        let stale_err = state.apply_event(AppEvent::LibraryLoadFailed {
            epoch: old_epoch,
            error: "401".to_owned(),
            is_auth_error: true,
            rate_limit: RateLimitInfo::default(),
        });
        assert!(stale_err.is_empty());
        assert!(matches!(
            &state.screen,
            Screen::Auth(auth) if auth.token == "x" && auth.error.is_none()
        ));

        // Stale LibraryLoaded from old session must NOT populate state while on Auth screen.
        let stale_load = state.apply_event(AppEvent::LibraryLoaded {
            epoch: old_epoch,
            library: sample_library(),
            rate_limit: RateLimitInfo::default(),
        });
        assert!(stale_load.is_empty());
        assert!(state.library.is_empty());

        // Sign in as a different user -> epoch advances again.
        state.apply_event(AppEvent::AuthSucceeded {
            client_id: "cid-new".to_owned(),
            token: "tok-new".to_owned(),
            username: "other_user".to_owned(),
            rate_limit: RateLimitInfo::default(),
        });
        let new_epoch = state.session_epoch();
        assert_ne!(new_epoch, auth_epoch);
        assert!(state.library.is_empty());

        // Stale SnapshotLoaded, EntrySaved, and MutationFailed from old_epoch are dropped.
        assert!(state
            .apply_event(AppEvent::SnapshotLoaded {
                epoch: old_epoch,
                library: sample_library(),
            })
            .is_empty());
        assert!(state.library.is_empty());

        assert!(state
            .apply_event(AppEvent::EntrySaved {
                epoch: old_epoch,
                media_id: media_id(1),
                entry: LibraryEntry {
                    id: Some(entry_id(1001)),
                    status: LibraryStatus::Completed,
                    progress: 28,
                    score: None,
                },
                rollback: old_rollback,
                rate_limit: RateLimitInfo::default(),
            })
            .is_empty());
        assert!(state.library.is_empty());

        // Current epoch SnapshotLoaded works cleanly.
        let snap_cmds = state.apply_event(AppEvent::SnapshotLoaded {
            epoch: new_epoch,
            library: sample_library(),
        });
        assert_eq!(state.visible_len(), 2);
        assert!(!snap_cmds.is_empty());
    }

    #[test]
    fn sort_picker_and_reverse_toggle_reorder_visible_rows_and_persist_config() {
        let mut state = authenticated_state(sample_library());
        // sample_library() Watching list has [Frieren (id 1), Cowboy Bebop (id 2)].
        assert_eq!(state.visible_items()[0].id, media_id(1));
        assert_eq!(state.visible_items()[1].id, media_id(2));
        assert_eq!(state.sort_field, SortField::Updated);
        assert_eq!(state.sort_direction, SortDirection::Desc);

        // Open Sort By modal with `s`, move down to Title, and apply with Enter.
        state.handle_key(key(KeyCode::Char('s')));
        assert_eq!(state.modal, ActiveModal::SortPicker { selected: 0 });
        state.handle_key(key(KeyCode::Char('j')));
        assert_eq!(state.modal, ActiveModal::SortPicker { selected: 1 });
        let cmds = state.handle_key(key(KeyCode::Enter));
        assert_eq!(state.modal, ActiveModal::None);
        assert_eq!(state.sort_field, SortField::Title);
        assert_eq!(state.sort_direction, SortDirection::Asc);
        // Cowboy Bebop < Frieren alphabetically, and previous selection (Frieren) is preserved.
        assert_eq!(state.visible_items()[0].id, media_id(2));
        assert_eq!(state.visible_items()[1].id, media_id(1));
        assert_eq!(state.selected_item().map(|i| i.id), Some(media_id(1)));
        assert!(cmds.iter().any(|cmd| matches!(
            cmd,
            AppCommand::SaveAppConfig(cfg)
                if cfg.sort_by == SortField::Title && cfg.sort_order == SortDirection::Asc
        )));

        // Press `S` to reverse direction to Title Desc -> Frieren first again.
        let rev_cmds = state.handle_key(key(KeyCode::Char('S')));
        assert_eq!(state.sort_field, SortField::Title);
        assert_eq!(state.sort_direction, SortDirection::Desc);
        assert_eq!(state.visible_items()[0].id, media_id(1));
        assert_eq!(state.visible_items()[1].id, media_id(2));
        assert!(rev_cmds.iter().any(|cmd| matches!(
            cmd,
            AppCommand::SaveAppConfig(cfg)
                if cfg.sort_by == SortField::Title && cfg.sort_order == SortDirection::Desc
        )));
    }

    #[test]
    fn startup_snapshot_freshness_respects_10_second_window() {
        assert!(is_snapshot_fresh(1_700_000_000, 1_700_000_000));
        assert!(is_snapshot_fresh(1_700_000_000, 1_700_000_009));
        assert!(!is_snapshot_fresh(1_700_000_000, 1_700_000_010));
        assert!(!is_snapshot_fresh(1_700_000_000, 1_700_000_120));
        // Future clock skew is treated as stale.
        assert!(!is_snapshot_fresh(1_700_000_050, 1_700_000_000));
    }
}
