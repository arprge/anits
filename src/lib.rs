//! Core modules for `anits`.

use std::time::{SystemTime, UNIX_EPOCH};

mod api;
mod app;
mod cache;
mod config;
mod domain;
mod ui;

pub use api::AnilistClient;
pub use app::run_app;
pub use config::{load_app_config, load_auth_session};

pub(crate) fn current_unix_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_secs())
}
