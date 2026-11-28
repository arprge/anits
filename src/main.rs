//! Entrypoint for the `anits` binary.

use std::io::{self, Stdout};

use anyhow::Result;
use crossterm::cursor::{Hide, Show};
use crossterm::execute;
use crossterm::terminal::{
    EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode,
};
use ratatui::Terminal;
use ratatui::backend::CrosstermBackend;
use ratatui_image::picker::Picker;

use anits::{AnilistClient, load_app_config, load_auth_session, run_app};

fn restore_terminal() {
    let _ = disable_raw_mode();
    let mut stdout = io::stdout();
    let _ = execute!(stdout, LeaveAlternateScreen, Show);
}

struct TerminalGuard;

impl Drop for TerminalGuard {
    fn drop(&mut self) {
        restore_terminal();
    }
}

#[tokio::main]
async fn main() -> Result<()> {
    let app_config = load_app_config();
    let auth = load_auth_session()?;
    let client = AnilistClient::new()?;

    let default_hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |panic_info| {
        restore_terminal();
        default_hook(panic_info);
    }));

    enable_raw_mode()?;
    let _guard = TerminalGuard;

    let mut stdout: Stdout = io::stdout();
    execute!(stdout, EnterAlternateScreen, Hide)?;

    let backend = CrosstermBackend::new(stdout);
    let mut terminal = Terminal::new(backend)?;
    terminal.clear()?;

    // Query terminal graphics capabilities after entering alternate screen and
    // before spawning crossterm's EventStream.
    let picker = Picker::from_query_stdio().unwrap_or_else(|_| Picker::from_fontsize((8, 16)));

    let run_result = run_app(&mut terminal, app_config, auth, client, picker).await;

    drop(_guard);
    run_result
}
