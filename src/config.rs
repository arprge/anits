//! Configuration, authentication persistence, and theme palette mapping.

use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

static TMP_FILE_COUNTER: AtomicU64 = AtomicU64::new(0);

use ratatui::style::Color;
use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::domain::ScoreFormat;

/// Default theme name (AniList dark palette).
pub const DEFAULT_THEME: &str = "anilist";

/// Curated basic themes cycled in the `?` Help & Settings overlay.
pub const CURATED_THEMES: &[(&str, &str)] = &[
    ("anilist", "AniList"),
    ("terminal", "Terminal (Transparent)"),
    ("catppuccin-mocha", "Catppuccin Mocha"),
    ("tokyo-night", "Tokyo Night"),
    ("nord", "Nord"),
    ("gruvbox", "Gruvbox"),
    ("dracula", "Dracula"),
];

/// Errors returned when reading or writing config and auth files.
#[derive(Debug, Error)]
pub enum ConfigError {
    #[error("could not determine user home or config directory")]
    MissingHomeDir,
    #[error("I/O error accessing {path}: {source}")]
    Io {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("failed to parse JSON in {path}: {source}")]
    Json {
        path: PathBuf,
        #[source]
        source: serde_json::Error,
    },
    #[error("failed to serialize TOML for {path}: {source}")]
    Toml {
        path: PathBuf,
        #[source]
        source: toml::ser::Error,
    },
    #[error("client ID is required to construct an OAuth URL")]
    MissingClientId,
}

/// User preference for which AniList rating format to display and input.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ScoreFormatPreference {
    /// Follow the user's AniList `mediaListOptions.scoreFormat` setting, defaulting to `9.8/10`.
    #[default]
    Auto,
    /// Override the rating format explicitly regardless of account setting.
    Override(ScoreFormat),
}

impl ScoreFormatPreference {
    /// Returns the TOML string written to `[ui].score_format`.
    #[must_use]
    pub const fn config_value(self) -> &'static str {
        match self {
            Self::Auto => "auto",
            Self::Override(fmt) => fmt.config_key(),
        }
    }

    /// Parses a `[ui].score_format` string; unrecognized values return `None`.
    #[must_use]
    pub fn from_config_value(raw: &str) -> Option<Self> {
        let trimmed = raw.trim();
        if trimmed.eq_ignore_ascii_case("auto") {
            return Some(Self::Auto);
        }
        ScoreFormat::from_config_key(trimmed).map(Self::Override)
    }

    /// Resolves the active [`ScoreFormat`], defaulting to [`ScoreFormat::Point10Decimal`].
    #[must_use]
    pub fn resolve(self, api_format: Option<ScoreFormat>) -> ScoreFormat {
        match self {
            Self::Auto => api_format.unwrap_or_default(),
            Self::Override(fmt) => fmt,
        }
    }
}

/// User UI configuration loaded from `~/.config/anits/config.toml`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AppConfig {
    pub theme: String,
    pub background: Option<String>,
    pub score_format: ScoreFormatPreference,
}

impl Default for AppConfig {
    fn default() -> Self {
        Self {
            theme: DEFAULT_THEME.to_owned(),
            background: None,
            score_format: ScoreFormatPreference::Auto,
        }
    }
}

#[derive(Debug, Serialize, Deserialize)]
struct RawConfigFile {
    ui: Option<RawUiConfig>,
}

#[derive(Debug, Serialize, Deserialize)]
struct RawUiConfig {
    theme: Option<String>,
    background: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    score_format: Option<String>,
}

/// Persisted AniList authentication state from `~/.config/anits/auth.json`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct AuthSession {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub access_token: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub client_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub username: Option<String>,
}

impl AuthSession {
    /// Returns `true` when both an access token and username are present.
    #[must_use]
    pub fn is_authenticated(&self) -> bool {
        self.access_token
            .as_deref()
            .is_some_and(|t| !t.trim().is_empty())
            && self
                .username
                .as_deref()
                .is_some_and(|u| !u.trim().is_empty())
    }

    /// Resolves the AniList client ID from `ANILIST_CLIENT_ID` first, then saved config.
    #[must_use]
    pub fn resolve_client_id(&self) -> Option<String> {
        if let Ok(env_id) = std::env::var("ANILIST_CLIENT_ID") {
            let trimmed = env_id.trim();
            if !trimmed.is_empty() {
                return Some(trimmed.to_owned());
            }
        }
        self.client_id
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_owned)
    }

    /// Clears `access_token` and `username` while preserving `client_id`.
    pub fn clear_credentials(&mut self) {
        self.access_token = None;
        self.username = None;
    }
}

/// AniList's copy/paste OAuth redirect.
///
/// This must be registered as the application's Redirect URL in the AniList
/// developer settings. Pointing the client at a callback/token server can
/// trigger `unsupported_grant_type`: the browser reaches that server's token
/// endpoint without a `grant_type`.
pub const ANILIST_OAUTH_PIN: &str = "https://anilist.co/api/v2/oauth/pin";

/// Builds the AniList implicit-grant OAuth authorization URL.
///
/// `redirect_uri` is intentionally omitted: AniList rejects implicit-grant
/// requests (`response_type=token`) with `unsupported_grant_type` when
/// `redirect_uri` is passed in the query string, and instead redirects to the
/// Redirect URL configured on the client (`ANILIST_OAUTH_PIN`).
pub fn oauth_url(client_id: &str) -> Result<String, ConfigError> {
    let trimmed = client_id.trim();
    if trimmed.is_empty() {
        return Err(ConfigError::MissingClientId);
    }
    Ok(format!(
        "https://anilist.co/api/v2/oauth/authorize?client_id={trimmed}&response_type=token"
    ))
}

/// Returns the default configuration directory (`~/.config/anits`).
pub fn config_dir() -> Result<PathBuf, ConfigError> {
    if let Some(home) = dirs::home_dir() {
        return Ok(home.join(".config").join("anits"));
    }
    dirs::config_dir()
        .map(|d| d.join("anits"))
        .ok_or(ConfigError::MissingHomeDir)
}

/// Returns the default cache directory (`~/.cache/anits`).
pub fn cache_dir() -> Result<PathBuf, ConfigError> {
    if let Some(home) = dirs::home_dir() {
        return Ok(home.join(".cache").join("anits"));
    }
    dirs::cache_dir()
        .map(|d| d.join("anits"))
        .ok_or(ConfigError::MissingHomeDir)
}

/// Loads `~/.config/anits/config.toml`, creating a default file if missing.
pub fn load_app_config() -> AppConfig {
    let Ok(dir) = config_dir() else {
        return AppConfig::default();
    };
    load_app_config_from(&dir.join("config.toml"))
}

/// Loads an [`AppConfig`] from `path`, creating it with defaults if it does not exist.
pub fn load_app_config_from(path: &Path) -> AppConfig {
    if !path.exists() {
        if let Some(parent) = path.parent() {
            let _ = fs::create_dir_all(parent);
        }
        let default_toml = format!("[ui]\ntheme = \"{DEFAULT_THEME}\"\n");
        let _ = fs::write(path, default_toml);
        return AppConfig::default();
    }

    let Ok(contents) = fs::read_to_string(path) else {
        return AppConfig::default();
    };
    let Ok(parsed) = toml::from_str::<RawConfigFile>(&contents) else {
        return AppConfig::default();
    };
    let Some(ui) = parsed.ui else {
        return AppConfig::default();
    };

    let theme = ui
        .theme
        .map(|t| t.trim().to_owned())
        .filter(|t| !t.is_empty())
        .unwrap_or_else(|| DEFAULT_THEME.to_owned());
    let background = ui
        .background
        .map(|b| b.trim().to_owned())
        .filter(|b| !b.is_empty());
    let score_format = ui
        .score_format
        .as_deref()
        .and_then(ScoreFormatPreference::from_config_value)
        .unwrap_or_default();

    AppConfig {
        theme,
        background,
        score_format,
    }
}

/// Persists [`AppConfig`] to `~/.config/anits/config.toml`.
pub fn save_app_config(cfg: &AppConfig) -> Result<(), ConfigError> {
    let dir = config_dir()?;
    save_app_config_to(&dir.join("config.toml"), cfg)
}

/// Persists [`AppConfig`] atomically to `path`.
pub fn save_app_config_to(path: &Path, cfg: &AppConfig) -> Result<(), ConfigError> {
    let raw = RawConfigFile {
        ui: Some(RawUiConfig {
            theme: Some(cfg.theme.clone()),
            background: cfg.background.clone(),
            score_format: Some(cfg.score_format.config_value().to_owned()),
        }),
    };
    let toml_text = toml::to_string_pretty(&raw).map_err(|source| ConfigError::Toml {
        path: path.to_path_buf(),
        source,
    })?;
    atomic_write_file(path, toml_text.as_bytes(), false)
}

/// Loads [`AuthSession`] from `~/.config/anits/auth.json`.
pub fn load_auth_session() -> Result<AuthSession, ConfigError> {
    let dir = config_dir()?;
    Ok(load_auth_session_from(&dir.join("auth.json")))
}

/// Loads [`AuthSession`] from `path`, returning [`AuthSession::default()`] if missing or invalid.
#[must_use]
pub fn load_auth_session_from(path: &Path) -> AuthSession {
    let Ok(contents) = fs::read_to_string(path) else {
        return AuthSession::default();
    };
    serde_json::from_str(&contents).unwrap_or_default()
}

/// Persists [`AuthSession`] to `~/.config/anits/auth.json`.
pub fn save_auth_session(session: &AuthSession) -> Result<(), ConfigError> {
    let dir = config_dir()?;
    save_auth_session_to(&dir.join("auth.json"), session)
}

/// Persists [`AuthSession`] atomically to `path` with `0o600` permissions on Unix,
/// removing the file if all fields are `None`.
pub fn save_auth_session_to(path: &Path, session: &AuthSession) -> Result<(), ConfigError> {
    if session.access_token.is_none()
        && session.client_id.is_none()
        && session.username.is_none()
    {
        if path.exists() {
            fs::remove_file(path).map_err(|source| ConfigError::Io {
                path: path.to_path_buf(),
                source,
            })?;
        }
        return Ok(());
    }

    let json = serde_json::to_string(session).map_err(|source| ConfigError::Json {
        path: path.to_path_buf(),
        source,
    })?;

    atomic_write_file(path, json.as_bytes(), true)
}

fn atomic_write_file(path: &Path, bytes: &[u8], private: bool) -> Result<(), ConfigError> {
    let parent = path
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    fs::create_dir_all(parent).map_err(|source| ConfigError::Io {
        path: parent.to_path_buf(),
        source,
    })?;

    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_nanos());
    let seq = TMP_FILE_COUNTER.fetch_add(1, Ordering::Relaxed);
    let file_name = path
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("file.tmp");
    let tmp_path = parent.join(format!(
        ".{file_name}.tmp.{}.{nanos}.{seq}",
        std::process::id()
    ));

    let write_res = write_new_file(&tmp_path, bytes, private);
    if let Err(source) = write_res {
        let _ = fs::remove_file(&tmp_path);
        return Err(ConfigError::Io {
            path: path.to_path_buf(),
            source,
        });
    }

    if let Err(source) = fs::rename(&tmp_path, path) {
        let _ = fs::remove_file(&tmp_path);
        return Err(ConfigError::Io {
            path: path.to_path_buf(),
            source,
        });
    }

    Ok(())
}

fn write_new_file(path: &Path, bytes: &[u8], private: bool) -> std::io::Result<()> {
    let mut opts = OpenOptions::new();
    opts.write(true).create_new(true);
    #[cfg(unix)]
    if private {
        use std::os::unix::fs::OpenOptionsExt;
        opts.mode(0o600);
    }
    let mut file = opts.open(path)?;
    file.write_all(bytes)?;
    file.sync_all()?;

    #[cfg(unix)]
    if private {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(path, fs::Permissions::from_mode(0o600))?;
    }

    Ok(())
}

/// Parses `#RRGGBB` hex colors or `"transparent"` / `"none"` into a `ratatui` [`Color`].
#[must_use]
pub fn parse_hex_color(raw: &str) -> Option<Color> {
    let trimmed = raw.trim();
    if trimmed.eq_ignore_ascii_case("transparent") || trimmed.eq_ignore_ascii_case("none") {
        return Some(Color::Reset);
    }
    let hex = trimmed.strip_prefix('#')?;
    if hex.len() != 6 || !hex.is_ascii() {
        return None;
    }
    let r = u8::from_str_radix(&hex[0..2], 16).ok()?;
    let g = u8::from_str_radix(&hex[2..4], 16).ok()?;
    let b = u8::from_str_radix(&hex[4..6], 16).ok()?;
    Some(Color::Rgb(r, g, b))
}

/// Color palette resolved from a configured theme name and optional background override.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ThemePalette {
    pub background: Color,
    pub surface: Color,
    pub text: Color,
    pub muted: Color,
    pub accent: Color,
    pub secondary: Color,
    pub success: Color,
    pub warning: Color,
    pub error: Color,
    pub border: Color,
}

impl ThemePalette {
    /// Resolves a palette from [`AppConfig`], applying any valid `background` override.
    #[must_use]
    pub fn from_config(cfg: &AppConfig) -> Self {
        let mut palette = Self::from_name(&cfg.theme);
        if let Some(bg) = cfg.background.as_deref().and_then(parse_hex_color) {
            palette.background = bg;
        }
        palette
    }

    /// Returns the next curated theme key in [`CURATED_THEMES`], wrapping around.
    #[must_use]
    pub fn next_theme_name(current: &str) -> &'static str {
        let lower = current.trim().to_ascii_lowercase();
        if let Some(pos) = CURATED_THEMES.iter().position(|(key, _)| *key == lower) {
            CURATED_THEMES[(pos + 1) % CURATED_THEMES.len()].0
        } else {
            CURATED_THEMES[0].0
        }
    }

    /// Returns a human-readable display label for a theme key.
    #[must_use]
    pub fn display_name(name: &str) -> &str {
        let lower = name.trim().to_ascii_lowercase();
        CURATED_THEMES
            .iter()
            .find(|(key, _)| *key == lower)
            .map_or(name, |(_, label)| *label)
    }

    /// Maps a configured theme name to its color palette, falling back to `anilist`.
    #[must_use]
    pub fn from_name(name: &str) -> Self {
        match name.trim().to_ascii_lowercase().as_str() {
            "catppuccin-mocha" => Self {
                background: Color::Rgb(30, 30, 46),
                surface: Color::Rgb(24, 24, 37),
                text: Color::Rgb(205, 214, 244),
                muted: Color::Rgb(166, 173, 200),
                accent: Color::Rgb(203, 166, 247),
                secondary: Color::Rgb(137, 180, 250),
                success: Color::Rgb(166, 227, 161),
                warning: Color::Rgb(249, 226, 175),
                error: Color::Rgb(243, 139, 168),
                border: Color::Rgb(88, 91, 112),
            },
            "catppuccin-latte" => Self {
                background: Color::Rgb(239, 241, 245),
                surface: Color::Rgb(230, 233, 239),
                text: Color::Rgb(76, 79, 105),
                muted: Color::Rgb(108, 111, 133),
                accent: Color::Rgb(136, 57, 239),
                secondary: Color::Rgb(30, 102, 245),
                success: Color::Rgb(64, 160, 43),
                warning: Color::Rgb(223, 142, 29),
                error: Color::Rgb(210, 15, 57),
                border: Color::Rgb(172, 176, 190),
            },
            "catppuccin-frappe" => Self {
                background: Color::Rgb(48, 52, 70),
                surface: Color::Rgb(41, 44, 60),
                text: Color::Rgb(198, 208, 245),
                muted: Color::Rgb(165, 173, 206),
                accent: Color::Rgb(202, 158, 230),
                secondary: Color::Rgb(140, 170, 238),
                success: Color::Rgb(166, 209, 137),
                warning: Color::Rgb(229, 200, 144),
                error: Color::Rgb(231, 130, 132),
                border: Color::Rgb(98, 104, 128),
            },
            "catppuccin-macchiato" => Self {
                background: Color::Rgb(36, 39, 58),
                surface: Color::Rgb(30, 32, 48),
                text: Color::Rgb(202, 211, 245),
                muted: Color::Rgb(165, 173, 203),
                accent: Color::Rgb(198, 160, 246),
                secondary: Color::Rgb(138, 173, 244),
                success: Color::Rgb(166, 218, 149),
                warning: Color::Rgb(238, 212, 159),
                error: Color::Rgb(237, 135, 150),
                border: Color::Rgb(91, 96, 120),
            },
            "gruvbox" => Self {
                background: Color::Rgb(40, 40, 40),
                surface: Color::Rgb(50, 48, 47),
                text: Color::Rgb(235, 219, 178),
                muted: Color::Rgb(168, 153, 132),
                accent: Color::Rgb(215, 153, 33),
                secondary: Color::Rgb(131, 165, 152),
                success: Color::Rgb(184, 187, 38),
                warning: Color::Rgb(250, 189, 47),
                error: Color::Rgb(251, 73, 52),
                border: Color::Rgb(102, 92, 84),
            },
            "nord" => Self {
                background: Color::Rgb(46, 52, 64),
                surface: Color::Rgb(59, 66, 82),
                text: Color::Rgb(236, 239, 244),
                muted: Color::Rgb(216, 222, 233),
                accent: Color::Rgb(136, 192, 208),
                secondary: Color::Rgb(129, 161, 193),
                success: Color::Rgb(163, 190, 140),
                warning: Color::Rgb(235, 203, 139),
                error: Color::Rgb(191, 97, 106),
                border: Color::Rgb(76, 86, 106),
            },
            "tokyo-night" => Self {
                background: Color::Rgb(26, 27, 38),
                surface: Color::Rgb(36, 40, 59),
                text: Color::Rgb(192, 202, 245),
                muted: Color::Rgb(169, 177, 214),
                accent: Color::Rgb(122, 162, 247),
                secondary: Color::Rgb(187, 154, 247),
                success: Color::Rgb(158, 206, 106),
                warning: Color::Rgb(224, 175, 104),
                error: Color::Rgb(247, 118, 142),
                border: Color::Rgb(65, 72, 104),
            },
            "dracula" => Self {
                background: Color::Rgb(40, 42, 54),
                surface: Color::Rgb(68, 71, 90),
                text: Color::Rgb(248, 248, 242),
                muted: Color::Rgb(98, 114, 164),
                accent: Color::Rgb(189, 147, 249),
                secondary: Color::Rgb(139, 233, 253),
                success: Color::Rgb(80, 250, 123),
                warning: Color::Rgb(241, 250, 140),
                error: Color::Rgb(255, 85, 85),
                border: Color::Rgb(98, 114, 164),
            },
            "monokai" => Self {
                background: Color::Rgb(39, 40, 34),
                surface: Color::Rgb(56, 56, 48),
                text: Color::Rgb(248, 248, 242),
                muted: Color::Rgb(117, 113, 94),
                accent: Color::Rgb(249, 38, 114),
                secondary: Color::Rgb(102, 217, 239),
                success: Color::Rgb(166, 226, 46),
                warning: Color::Rgb(230, 219, 116),
                error: Color::Rgb(249, 38, 114),
                border: Color::Rgb(90, 88, 76),
            },
            "flexoki" => Self {
                background: Color::Rgb(16, 15, 15),
                surface: Color::Rgb(28, 27, 26),
                text: Color::Rgb(206, 205, 195),
                muted: Color::Rgb(135, 133, 128),
                accent: Color::Rgb(208, 162, 21),
                secondary: Color::Rgb(67, 133, 190),
                success: Color::Rgb(135, 154, 57),
                warning: Color::Rgb(218, 112, 44),
                error: Color::Rgb(209, 77, 65),
                border: Color::Rgb(64, 62, 60),
            },
            "rose-pine" => Self {
                background: Color::Rgb(25, 23, 36),
                surface: Color::Rgb(31, 29, 46),
                text: Color::Rgb(224, 222, 244),
                muted: Color::Rgb(144, 140, 170),
                accent: Color::Rgb(196, 167, 231),
                secondary: Color::Rgb(156, 207, 216),
                success: Color::Rgb(49, 116, 143),
                warning: Color::Rgb(246, 193, 119),
                error: Color::Rgb(235, 111, 146),
                border: Color::Rgb(64, 61, 82),
            },
            "rose-pine-moon" => Self {
                background: Color::Rgb(35, 33, 54),
                surface: Color::Rgb(42, 39, 63),
                text: Color::Rgb(224, 222, 244),
                muted: Color::Rgb(144, 140, 170),
                accent: Color::Rgb(196, 167, 231),
                secondary: Color::Rgb(156, 207, 216),
                success: Color::Rgb(62, 143, 176),
                warning: Color::Rgb(246, 193, 119),
                error: Color::Rgb(235, 111, 146),
                border: Color::Rgb(68, 65, 90),
            },
            "rose-pine-dawn" => Self {
                background: Color::Rgb(250, 244, 237),
                surface: Color::Rgb(242, 233, 225),
                text: Color::Rgb(87, 82, 121),
                muted: Color::Rgb(152, 147, 165),
                accent: Color::Rgb(144, 122, 169),
                secondary: Color::Rgb(86, 148, 159),
                success: Color::Rgb(40, 105, 131),
                warning: Color::Rgb(234, 157, 52),
                error: Color::Rgb(180, 99, 122),
                border: Color::Rgb(215, 206, 199),
            },
            "solarized-light" => Self {
                background: Color::Rgb(253, 246, 227),
                surface: Color::Rgb(238, 232, 213),
                text: Color::Rgb(101, 123, 131),
                muted: Color::Rgb(147, 161, 161),
                accent: Color::Rgb(38, 139, 210),
                secondary: Color::Rgb(42, 161, 152),
                success: Color::Rgb(133, 153, 0),
                warning: Color::Rgb(181, 137, 0),
                error: Color::Rgb(220, 50, 47),
                border: Color::Rgb(147, 161, 161),
            },
            "solarized-dark" => Self {
                background: Color::Rgb(0, 43, 54),
                surface: Color::Rgb(7, 54, 66),
                text: Color::Rgb(131, 148, 150),
                muted: Color::Rgb(88, 110, 117),
                accent: Color::Rgb(38, 139, 210),
                secondary: Color::Rgb(42, 161, 152),
                success: Color::Rgb(133, 153, 0),
                warning: Color::Rgb(181, 137, 0),
                error: Color::Rgb(220, 50, 47),
                border: Color::Rgb(88, 110, 117),
            },
            "atom-one-dark" => Self {
                background: Color::Rgb(40, 44, 52),
                surface: Color::Rgb(33, 37, 43),
                text: Color::Rgb(171, 178, 191),
                muted: Color::Rgb(92, 99, 112),
                accent: Color::Rgb(198, 120, 221),
                secondary: Color::Rgb(97, 175, 239),
                success: Color::Rgb(152, 195, 121),
                warning: Color::Rgb(229, 192, 123),
                error: Color::Rgb(224, 108, 117),
                border: Color::Rgb(62, 68, 81),
            },
            "atom-one-light" => Self {
                background: Color::Rgb(250, 250, 250),
                surface: Color::Rgb(240, 240, 241),
                text: Color::Rgb(56, 58, 66),
                muted: Color::Rgb(160, 161, 167),
                accent: Color::Rgb(166, 38, 164),
                secondary: Color::Rgb(64, 120, 242),
                success: Color::Rgb(80, 161, 79),
                warning: Color::Rgb(193, 132, 1),
                error: Color::Rgb(228, 86, 73),
                border: Color::Rgb(200, 200, 204),
            },
            "textual-dark" => Self {
                background: Color::Rgb(18, 18, 18),
                surface: Color::Rgb(30, 30, 30),
                text: Color::Rgb(224, 224, 224),
                muted: Color::Rgb(140, 140, 140),
                accent: Color::Rgb(1, 120, 212),
                secondary: Color::Rgb(0, 170, 255),
                success: Color::Rgb(78, 191, 113),
                warning: Color::Rgb(255, 166, 43),
                error: Color::Rgb(186, 60, 91),
                border: Color::Rgb(68, 68, 68),
            },
            "textual-light" => Self {
                background: Color::Rgb(248, 249, 250),
                surface: Color::Rgb(233, 236, 239),
                text: Color::Rgb(33, 37, 41),
                muted: Color::Rgb(108, 117, 125),
                accent: Color::Rgb(0, 69, 120),
                secondary: Color::Rgb(1, 120, 212),
                success: Color::Rgb(78, 191, 113),
                warning: Color::Rgb(255, 166, 43),
                error: Color::Rgb(186, 60, 91),
                border: Color::Rgb(173, 181, 189),
            },
            "terminal" | "transparent" | "ansi" | "textual-ansi" => Self {
                // Transparent theme: keep the terminal's own background and
                // default foreground. Accents use the regular ANSI palette
                // (like OpenCode's system theme) instead of the bright
                // variants, which are unreadable on light backgrounds.
                //
                // ANSI black/bright-black are too theme-dependent for muted
                // text and borders - many popular themes make them nearly
                // invisible. Use the xterm gray ramp instead, which gives
                // OpenCode-like contrast on both light and dark terminals.
                background: Color::Reset,
                surface: Color::Reset,
                text: Color::Reset,
                muted: Color::Indexed(243),
                accent: Color::Cyan,
                secondary: Color::Blue,
                success: Color::Green,
                warning: Color::Yellow,
                error: Color::Red,
                border: Color::Indexed(240),
            },
            // Default: AniList dark palette
            _ => Self {
                background: Color::Rgb(11, 22, 34),
                surface: Color::Rgb(21, 31, 46),
                text: Color::Rgb(237, 241, 245),
                muted: Color::Rgb(139, 160, 178),
                accent: Color::Rgb(61, 180, 242),
                secondary: Color::Rgb(146, 173, 200),
                success: Color::Rgb(104, 214, 57),
                warning: Color::Rgb(247, 154, 99),
                error: Color::Rgb(232, 93, 117),
                border: Color::Rgb(31, 45, 64),
            },
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn unique_temp_dir(label: &str) -> PathBuf {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("system clock after unix epoch")
            .as_nanos();
        std::env::temp_dir().join(format!("anilist_tui_test_{label}_{nanos}"))
    }

    #[test]
    fn creates_default_config_when_missing() {
        let dir = unique_temp_dir("default_cfg");
        let path = dir.join("config.toml");

        let cfg = load_app_config_from(&path);
        assert_eq!(cfg, AppConfig::default());
        assert!(path.exists());

        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn parses_theme_and_background_from_toml() {
        let dir = unique_temp_dir("parse_cfg");
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join("config.toml");
        fs::write(
            &path,
            "[ui]\ntheme = \"  nord \"\nbackground = \" #11111b \"\n",
        )
        .unwrap();

        let cfg = load_app_config_from(&path);
        assert_eq!(cfg.theme, "nord");
        assert_eq!(cfg.background.as_deref(), Some("#11111b"));

        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn preserves_background_when_theme_is_omitted() {
        let dir = unique_temp_dir("bg_only_cfg");
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join("config.toml");
        fs::write(&path, "[ui]\nbackground = \"transparent\"\n").unwrap();

        let cfg = load_app_config_from(&path);
        assert_eq!(cfg.theme, DEFAULT_THEME);
        assert_eq!(cfg.background.as_deref(), Some("transparent"));

        let palette = ThemePalette::from_config(&cfg);
        assert_eq!(palette.background, Color::Reset);

        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn parses_hex_and_transparent_colors_for_palette_override() {
        assert_eq!(parse_hex_color("#11111b"), Some(Color::Rgb(17, 17, 27)));
        assert_eq!(parse_hex_color(" #FF00aa "), Some(Color::Rgb(255, 0, 170)));
        assert_eq!(parse_hex_color("TRANSPARENT"), Some(Color::Reset));
        assert_eq!(parse_hex_color("none"), Some(Color::Reset));
        assert_eq!(parse_hex_color("not-a-color"), None);
        assert_eq!(parse_hex_color("#12345"), None);

        let cfg = AppConfig {
            theme: "nord".to_owned(),
            background: Some("#101218".to_owned()),
            score_format: ScoreFormatPreference::Auto,
        };
        let palette = ThemePalette::from_config(&cfg);
        assert_eq!(palette.background, Color::Rgb(16, 18, 24));
        assert_eq!(palette.accent, ThemePalette::from_name("nord").accent);
    }

    #[test]
    fn auth_session_roundtrip_and_clear_preserves_client_id() {
        let dir = unique_temp_dir("auth_rt");
        let path = dir.join("auth.json");

        let mut session = AuthSession {
            access_token: Some("tok-123".into()),
            client_id: Some("cid-456".into()),
            username: Some("nkaet".into()),
        };
        assert!(session.is_authenticated());
        save_auth_session_to(&path, &session).unwrap();

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = fs::metadata(&path).unwrap().permissions().mode() & 0o777;
            assert_eq!(mode, 0o600);
        }

        let loaded = load_auth_session_from(&path);
        assert_eq!(loaded, session);

        session.clear_credentials();
        assert!(!session.is_authenticated());
        save_auth_session_to(&path, &session).unwrap();

        let after_clear = load_auth_session_from(&path);
        assert_eq!(after_clear.client_id.as_deref(), Some("cid-456"));
        assert!(after_clear.access_token.is_none());
        assert!(after_clear.username.is_none());

        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn oauth_url_validates_client_id() {
        assert!(oauth_url("   ").is_err());
        let url = oauth_url("12345").unwrap();
        assert_eq!(
            url,
            "https://anilist.co/api/v2/oauth/authorize?client_id=12345&response_type=token"
        );
        assert_eq!(ANILIST_OAUTH_PIN, "https://anilist.co/api/v2/oauth/pin");
    }

    #[test]
    fn rapid_consecutive_auth_saves_and_relative_parent_filter_work() {
        let dir = unique_temp_dir("rapid_auth_save");
        let path = dir.join("auth.json");

        for idx in 0..10 {
            let session = AuthSession {
                access_token: Some(format!("tok-{idx}")),
                client_id: Some("cid".into()),
                username: Some("user".into()),
            };
            save_auth_session_to(&path, &session).unwrap();
            assert_eq!(
                load_auth_session_from(&path).access_token.as_deref(),
                Some(format!("tok-{idx}").as_str())
            );
        }

        let bare = Path::new("auth.json");
        let resolved_parent = bare
            .parent()
            .filter(|p| !p.as_os_str().is_empty())
            .unwrap_or_else(|| Path::new("."));
        assert_eq!(resolved_parent, Path::new("."));

        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn score_format_config_roundtrip_and_auto_fallback() {
        let dir = unique_temp_dir("score_format_cfg");
        let path = dir.join("config.toml");

        let cfg = AppConfig {
            theme: "nord".to_owned(),
            background: Some("#11111b".to_owned()),
            score_format: ScoreFormatPreference::Override(ScoreFormat::Point5),
        };
        save_app_config_to(&path, &cfg).unwrap();

        let loaded = load_app_config_from(&path);
        assert_eq!(loaded, cfg);

        // Unrecognized or missing score_format safely defaults to Auto.
        fs::write(&path, "[ui]\ntheme = \"nord\"\nscore_format = \"unknown\"\n").unwrap();
        let fallback = load_app_config_from(&path);
        assert_eq!(fallback.score_format, ScoreFormatPreference::Auto);
        assert_eq!(
            fallback.score_format.resolve(None),
            ScoreFormat::Point10Decimal
        );
        assert_eq!(
            fallback.score_format.resolve(Some(ScoreFormat::Point100)),
            ScoreFormat::Point100
        );

        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn default_theme_is_anilist_and_curated_themes_cycle_cleanly() {
        assert_eq!(DEFAULT_THEME, "anilist");
        let anilist = ThemePalette::from_name("anilist");
        assert_eq!(anilist.background, Color::Rgb(11, 22, 34));
        assert_eq!(anilist.accent, Color::Rgb(61, 180, 242));

        let terminal = ThemePalette::from_name("terminal");
        assert_eq!(terminal.background, Color::Reset);
        assert_eq!(terminal.surface, Color::Reset);
        assert_eq!(terminal.text, Color::Reset);
        // Muted text and borders stay readable on both light and dark
        // terminal backgrounds (ANSI 7/8 are too theme-dependent).
        assert_eq!(terminal.muted, Color::Indexed(243));
        assert_eq!(terminal.border, Color::Indexed(240));
        assert_eq!(terminal.accent, Color::Cyan);
        assert_eq!(terminal.secondary, Color::Blue);
        assert_eq!(ThemePalette::from_name("transparent"), terminal);
        assert_eq!(ThemePalette::from_name("ansi"), terminal);

        assert_eq!(ThemePalette::next_theme_name("anilist"), "terminal");
        assert_eq!(ThemePalette::next_theme_name("terminal"), "catppuccin-mocha");
        assert_eq!(ThemePalette::next_theme_name("dracula"), "anilist");
        assert_eq!(ThemePalette::display_name("terminal"), "Terminal (Transparent)");
    }
}
