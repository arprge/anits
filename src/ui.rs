//! Immediate-mode terminal UI rendering for `anits`.

use std::borrow::Cow;

use ratatui::Frame;
use ratatui::layout::{Alignment, Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{
    Block, BorderType, Borders, Cell, Clear, List, ListItem, ListState, Padding, Paragraph, Row,
    Table, TableState, Wrap,
};
use ratatui_image::{Resize, StatefulImage, protocol::StatefulProtocol};

use crate::api::RateLimitInfo;
use crate::app::{
    ActiveModal, AppState, AuthField, FilterScope, FocusPane, Screen, SidebarItem, ToastLevel,
};
use crate::config::{ANILIST_OAUTH_PIN, ScoreFormatPreference, ThemePalette};
use crate::domain::{
    AnimeItem, LibraryStatus, MediaId, Point3Score, ScoreFormat, ScoreRaw, SortDirection, SortField,
};

const SPINNER_FRAMES: [&str; 10] = ["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];

const AUTH_FOOTER_HINT: &str =
    "Tab/↑↓ Field   Ctrl+O Open OAuth   Enter Confirm   Ctrl+C Quit";
const TABLE_FOOTER_HINT: &str =
    "1-6 List   / Filter   s Sort   j/k Move   +/- Ep   a Move   r Rate   v Web   R Sync   ? Help   q Quit";
const SIDEBAR_FOOTER_HINT: &str =
    "j/k Select   Enter Open/Sign Out   h Workspace   1-6 List   R Sync   ? Help   q Quit";
const DETAILS_FOOTER_HINT: &str =
    "j/k Scroll   +/- Ep   a Move   r Rate   v Web   Tab Next Pane   ? Help   q Quit";
const FILTER_FOOTER_HINT: &str =
    "Filter: Type query   Ctrl+A Toggle List/All   Enter/Esc Done";

struct AuthView<'a> {
    palette: ThemePalette,
    username: Option<&'a str>,
    client_id: &'a str,
    token: &'a str,
    focused: &'a AuthField,
    submitting: bool,
    error: Option<&'a str>,
    oauth_url: Option<&'a str>,
}

#[derive(Clone, Copy)]
struct LibraryOverview {
    total_titles: usize,
    episodes_watched: u64,
    rated_count: usize,
    mean_avg_score: Option<u8>,
}

struct LibraryView<'a> {
    palette: ThemePalette,
    username: Option<&'a str>,
    counts: [usize; 6],
    overview: LibraryOverview,
    active_status: LibraryStatus,
    focus: &'a FocusPane,
    sidebar_index: usize,
    table_selected: usize,
    filter_query: &'a str,
    filter_active: bool,
    filter_scope: &'a FilterScope,
    sort_field: SortField,
    sort_direction: SortDirection,
    description_scroll: u16,
    syncing: bool,
    sync_tick: usize,
    rate_limit: RateLimitInfo,
    score_format: ScoreFormat,
    toast: Option<(&'a str, &'a ToastLevel)>,
    visible_items: &'a [&'a AnimeItem],
    selected_item: Option<&'a AnimeItem>,
}

/// Renders the entire `anits` frame for the current [`AppState`].
pub fn render(frame: &mut Frame<'_>, state: &mut AppState) {
    let root = frame.area();
    fill_background(frame, root, state.palette);
    state.pending_reencode = None;

    if root.width == 0 || root.height == 0 {
        return;
    }

    match &state.screen {
        Screen::Auth(auth) => {
            let oauth_preview = auth.oauth_url_preview();
            let view = AuthView {
                palette: state.palette,
                username: state.username.as_deref(),
                client_id: &auth.client_id,
                token: &auth.token,
                focused: &auth.focused,
                submitting: auth.submitting,
                error: auth.error.as_deref(),
                oauth_url: oauth_preview.as_deref(),
            };
            render_auth_view(frame, root, &view);
        }
        Screen::Library => {
            let palette = state.palette;
            let score_format = state.effective_score_format();
            let mut table_offset = state.table_offset;
            let (cover_target, selected_status, selected_score) = {
                let mut counts = [0usize; 6];
                let mut total_titles = 0usize;
                let mut episodes_watched = 0u64;
                let mut rated_count = 0usize;
                let mut avg_sum = 0u64;
                let mut avg_n = 0u64;

                for status in LibraryStatus::ALL {
                    let items = state.library.items_for(status);
                    counts[status.index()] = items.len();
                    total_titles += items.len();
                    for item in items {
                        episodes_watched += u64::from(item.entry.progress);
                        if item.entry.score.is_some() {
                            rated_count += 1;
                        }
                        if let Some(avg) = item.average_score {
                            avg_sum += u64::from(avg);
                            avg_n += 1;
                        }
                    }
                }

                let overview = LibraryOverview {
                    total_titles,
                    episodes_watched,
                    rated_count,
                    mean_avg_score: (avg_n > 0).then(|| (avg_sum / avg_n) as u8),
                };

                let visible_items = state.visible_items();
                let selected_item = state.selected_item();
                let selected_status = selected_item.map(|item| item.entry.status);
                let selected_score = selected_item.and_then(|item| item.entry.score);
                let toast = state.active_toast().map(|t| (t.message.as_str(), &t.level));

                let view = LibraryView {
                    palette,
                    username: state.username.as_deref(),
                    counts,
                    overview,
                    active_status: state.active_status,
                    focus: &state.focus,
                    sidebar_index: state.sidebar_index,
                    table_selected: state.table_selected,
                    filter_query: &state.filter_query,
                    filter_active: state.filter_active,
                    filter_scope: &state.filter_scope,
                    sort_field: state.sort_field,
                    sort_direction: state.sort_direction,
                    description_scroll: state.description_scroll,
                    syncing: state.syncing,
                    sync_tick: state.sync_tick,
                    rate_limit: state.rate_limit,
                    score_format,
                    toast,
                    visible_items: &visible_items,
                    selected_item,
                };
                (
                    render_library_base(frame, root, &view, &mut table_offset),
                    selected_status,
                    selected_score,
                )
            };
            state.table_offset = table_offset;

            if let Some((media_id, cover_area)) = cover_target {
                if cover_area.width > 0 && cover_area.height > 0 {
                    let is_loading = state.cover_cache.is_loading(media_id);
                    let proto = state.cover_cache.get_mut(media_id);
                    render_cover_slot(
                        frame,
                        CoverSlot {
                            cover_area,
                            media_id,
                            palette,
                            proto,
                            is_loading,
                            last_cover_area: &mut state.last_cover_area,
                            pending_reencode: &mut state.pending_reencode,
                        },
                    );
                }
            }

            let modal_ctx = ModalContext {
                status: selected_status,
                score: selected_score,
                score_format,
                score_pref: state.score_format_pref,
                api_score_format: state.api_score_format,
                theme_name: state.theme_name(),
                rating_input: &state.rating_input,
                rating_input_error: state.rating_input_error.as_deref(),
                sort_field: state.sort_field,
                sort_direction: state.sort_direction,
            };
            render_modal(frame, root, &state.modal, palette, &modal_ctx);
        }
    }
}

fn fill_background(frame: &mut Frame<'_>, area: Rect, palette: ThemePalette) {
    let block = Block::default().style(Style::default().bg(palette.background).fg(palette.text));
    frame.render_widget(block, area);
}

fn contrast_fg_on(bg: Color, palette: ThemePalette) -> Color {
    if palette.background != Color::Reset {
        return palette.background;
    }
    if palette.surface != Color::Reset {
        return palette.surface;
    }
    match bg {
        Color::Rgb(r, g, b) => {
            let luma = u32::from(r) * 299 + u32::from(g) * 587 + u32::from(b) * 114;
            if luma > 128_000 {
                Color::Black
            } else {
                Color::White
            }
        }
        Color::Cyan | Color::Yellow | Color::Green | Color::White | Color::Gray => Color::Black,
        _ => Color::White,
    }
}

fn unfocused_highlight_style(palette: ThemePalette) -> Style {
    // Transparent themes have no panel color to highlight with. Use the
    // accent as an opaque selection background so the row stays readable
    // over whatever the terminal is showing through.
    if palette.background == Color::Reset || palette.surface == Color::Reset {
        return Style::default()
            .bg(palette.accent)
            .fg(contrast_fg_on(palette.accent, palette))
            .add_modifier(Modifier::BOLD);
    }
    let base = Style::default()
        .fg(palette.accent)
        .add_modifier(Modifier::BOLD);
    if palette.surface != palette.background {
        base.bg(palette.surface)
    } else {
        base
    }
}

fn centered_rect(area: Rect, max_width: u16, max_height: u16) -> Rect {
    let width = max_width.min(area.width);
    let height = max_height.min(area.height);
    let x = area.x + area.width.saturating_sub(width) / 2;
    let y = area.y + area.height.saturating_sub(height) / 2;
    Rect::new(x, y, width, height)
}

fn render_auth_view(frame: &mut Frame<'_>, root: Rect, view: &AuthView<'_>) {
    let palette = view.palette;
    let [main_area, footer_area] = if root.height >= 4 {
        Layout::vertical([Constraint::Min(1), Constraint::Length(1)]).areas(root)
    } else {
        [root, Rect::default()]
    };

    let desired_w = if main_area.width > 80 {
        76
    } else if main_area.width > 8 {
        main_area.width.saturating_sub(2)
    } else {
        main_area.width
    };
    let desired_h: u16 = if view.error.is_some() { 20 } else { 18 };
    let modal_area = centered_rect(main_area, desired_w, desired_h);

    let outer_block = Block::bordered()
        .border_type(BorderType::Rounded)
        .padding(Padding::horizontal(1))
        .title(Line::from(vec![
            Span::styled(
                " ◈ ",
                Style::default()
                    .fg(palette.accent)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::styled(
                "AniList Library — Connect Account ",
                Style::default()
                    .fg(palette.text)
                    .add_modifier(Modifier::BOLD),
            ),
        ]))
        .border_style(Style::default().fg(palette.accent))
        .style(Style::default().bg(palette.surface).fg(palette.text));

    let inner = outer_block.inner(modal_area);
    frame.render_widget(outer_block, modal_area);

    if inner.width > 0 && inner.height > 0 {
        let err_height = if view.error.is_some() { 2 } else { 0 };
        let [note_area, step1_area, step2_area, step3_area, step4_area, err_area] =
            if inner.height >= 15 {
                Layout::vertical([
                    Constraint::Length(2),
                    Constraint::Length(3),
                    Constraint::Length(4),
                    Constraint::Length(3),
                    Constraint::Length(3),
                    Constraint::Length(err_height),
                ])
                .areas(inner)
            } else {
                Layout::vertical([
                    Constraint::Length(1),
                    Constraint::Min(1),
                    Constraint::Min(1),
                    Constraint::Min(1),
                    Constraint::Length(1),
                    Constraint::Length(if view.error.is_some() { 1 } else { 0 }),
                ])
                .areas(inner)
            };

        let note_line = match view.username {
            Some(user) => Line::from(vec![
                Span::styled(
                    format!("@{user}"),
                    Style::default()
                        .fg(palette.secondary)
                        .add_modifier(Modifier::BOLD),
                ),
                Span::styled(
                    " — re-authorize or update OAuth2 credentials",
                    Style::default().fg(palette.muted),
                ),
            ]),
            None => Line::styled(
                format!("OAuth2 Client ID + access token. App Redirect URL: {ANILIST_OAUTH_PIN}"),
                Style::default().fg(palette.muted),
            ),
        };
        frame.render_widget(Paragraph::new(note_line).wrap(Wrap { trim: true }), note_area);

        // 01: Client ID
        let cid_focused = matches!(view.focused, AuthField::ClientId);
        let cid_line = if view.client_id.is_empty() {
            let mut spans = Vec::with_capacity(2);
            if cid_focused {
                spans.push(Span::styled("▏", Style::default().fg(palette.accent)));
            }
            spans.push(Span::styled(
                "Enter AniList Client ID (or set ANILIST_CLIENT_ID)",
                Style::default().fg(palette.muted),
            ));
            Line::from(spans)
        } else {
            let mut spans = Vec::with_capacity(2);
            spans.push(Span::styled(view.client_id, Style::default().fg(palette.text)));
            if cid_focused {
                spans.push(Span::styled("▏", Style::default().fg(palette.accent)));
            }
            Line::from(spans)
        };
        render_auth_field_box(
            frame,
            step1_area,
            " 01 Client ID ",
            None,
            cid_line,
            cid_focused,
            palette,
        );

        // 02: OAuth URL link box
        let url_focused = matches!(view.focused, AuthField::OpenUrl);
        let url_line = match view.oauth_url {
            Some(url) => Line::styled(
                url,
                Style::default()
                    .fg(palette.secondary)
                    .add_modifier(Modifier::UNDERLINED),
            ),
            None => Line::styled(
                "Enter Client ID to generate authorization link",
                Style::default().fg(palette.muted),
            ),
        };
        render_auth_field_box(
            frame,
            step2_area,
            " 02 Authorize App ",
            Some(" Enter / Ctrl+O to open in browser "),
            url_line,
            url_focused,
            palette,
        );

        // 03: Access Token masked with '•'
        let tok_focused = matches!(view.focused, AuthField::Token);
        let masked_token = mask_secret(view.token);
        let tok_line = if masked_token.is_empty() {
            let mut spans = Vec::with_capacity(2);
            if tok_focused {
                spans.push(Span::styled("▏", Style::default().fg(palette.accent)));
            }
            spans.push(Span::styled(
                "Paste token or full callback URL",
                Style::default().fg(palette.muted),
            ));
            Line::from(spans)
        } else {
            let mut spans = Vec::with_capacity(2);
            spans.push(Span::styled(masked_token, Style::default().fg(palette.text)));
            if tok_focused {
                spans.push(Span::styled("▏", Style::default().fg(palette.accent)));
            }
            Line::from(spans)
        };
        let step3_hint = if view.token.is_empty() {
            None
        } else {
            Some(" Token or full callback URL ")
        };
        render_auth_field_box(
            frame,
            step3_area,
            " 03 Access Token ",
            step3_hint,
            tok_line,
            tok_focused,
            palette,
        );

        // 04: Submit button
        let submit_focused = matches!(view.focused, AuthField::Submit);
        let btn_label = if view.submitting {
            "Verifying token..."
        } else {
            "Connect & Sync Library"
        };
        let btn_style = if submit_focused {
            Style::default()
                .bg(palette.accent)
                .fg(contrast_fg_on(palette.accent, palette))
                .add_modifier(Modifier::BOLD)
        } else if view.submitting {
            Style::default().bg(palette.surface).fg(palette.muted)
        } else {
            Style::default()
                .bg(palette.surface)
                .fg(palette.text)
                .add_modifier(Modifier::BOLD)
        };

        if step4_area.height >= 3 {
            let btn_block = Block::bordered()
                .border_type(BorderType::Rounded)
                .border_style(Style::default().fg(if submit_focused {
                    palette.accent
                } else {
                    palette.border
                }))
                .style(btn_style);
            frame.render_widget(
                Paragraph::new(Line::styled(btn_label, btn_style))
                    .alignment(Alignment::Center)
                    .block(btn_block),
                step4_area,
            );
        } else if step4_area.height > 0 {
            frame.render_widget(
                Paragraph::new(Line::styled(format!("[ {btn_label} ]"), btn_style))
                    .alignment(Alignment::Center),
                step4_area,
            );
        }

        if let Some(err) = view.error {
            if err_area.height > 0 {
                let err_para = Paragraph::new(Line::styled(
                    err,
                    Style::default()
                        .fg(palette.error)
                        .add_modifier(Modifier::BOLD),
                ))
                .wrap(Wrap { trim: true });
                frame.render_widget(err_para, err_area);
            }
        }
    }

    if footer_area.height > 0 && footer_area.width > 0 {
        let footer = Paragraph::new(Line::styled(
            AUTH_FOOTER_HINT,
            Style::default().fg(palette.muted),
        ))
        .alignment(Alignment::Center);
        frame.render_widget(footer, footer_area);
    }
}

fn render_auth_field_box(
    frame: &mut Frame<'_>,
    area: Rect,
    title: &str,
    bottom_hint: Option<&str>,
    content: Line<'_>,
    focused: bool,
    palette: ThemePalette,
) {
    if area.width == 0 || area.height == 0 {
        return;
    }

    let title_style = if focused {
        Style::default()
            .fg(palette.accent)
            .add_modifier(Modifier::BOLD)
    } else {
        Style::default().fg(palette.muted)
    };

    if area.height >= 3 {
        let mut block = Block::bordered()
            .border_type(BorderType::Rounded)
            .padding(Padding::horizontal(1))
            .title(Line::styled(title, title_style))
            .border_style(Style::default().fg(if focused {
                palette.accent
            } else {
                palette.border
            }))
            .style(Style::default().bg(palette.surface).fg(palette.text));
        if let Some(hint) = bottom_hint {
            block = block.title_bottom(
                Line::styled(
                    hint,
                    Style::default().fg(if focused {
                        palette.accent
                    } else {
                        palette.muted
                    }),
                )
                .alignment(Alignment::Right),
            );
        }
        let para = Paragraph::new(content)
            .wrap(Wrap { trim: false })
            .block(block);
        frame.render_widget(para, area);
    } else {
        let para = Paragraph::new(content).wrap(Wrap { trim: false });
        frame.render_widget(para, area);
    }
}

fn mask_secret(input: &str) -> String {
    let count = input.chars().count();
    let mut masked = String::with_capacity(count * '•'.len_utf8());
    for _ in 0..count {
        masked.push('•');
    }
    masked
}

fn render_library_base(
    frame: &mut Frame<'_>,
    root: Rect,
    view: &LibraryView<'_>,
    table_offset: &mut usize,
) -> Option<(MediaId, Rect)> {
    let [top_area, body_area, footer_area] = if root.height >= 10 {
        Layout::vertical([
            Constraint::Length(2),
            Constraint::Min(6),
            Constraint::Length(1),
        ])
        .areas(root)
    } else {
        Layout::vertical([
            Constraint::Length(1),
            Constraint::Min(1),
            Constraint::Length(1),
        ])
        .areas(root)
    };

    render_top_bar(frame, top_area, view);

    let padded_body = if root.width >= 84 && body_area.height >= 4 {
        let [_, center, _] = Layout::horizontal([
            Constraint::Length(1),
            Constraint::Min(1),
            Constraint::Length(1),
        ])
        .areas(body_area);
        center
    } else {
        body_area
    };

    let cover_target = if padded_body.width > 0 && padded_body.height > 0 {
        if root.width >= 76 {
            let sidebar_w = if root.width >= 112 { 27 } else { 23 };
            let [content_area, _gutter, sidebar_area] = Layout::horizontal([
                Constraint::Min(24),
                Constraint::Length(1),
                Constraint::Length(sidebar_w),
            ])
            .areas(padded_body);

            let cover_target =
                render_left_workspace(frame, root.width, content_area, view, table_offset);
            render_sidebar(frame, sidebar_area, view);
            cover_target
        } else {
            let [strip_area, content_area] =
                Layout::vertical([Constraint::Length(3), Constraint::Min(3)]).areas(padded_body);
            render_compact_strip(frame, strip_area, view);
            render_left_workspace(frame, root.width, content_area, view, table_offset)
        }
    } else {
        None
    };

    render_footer(frame, footer_area, view);
    cover_target
}

fn render_left_workspace(
    frame: &mut Frame<'_>,
    total_width: u16,
    content_area: Rect,
    view: &LibraryView<'_>,
    table_offset: &mut usize,
) -> Option<(MediaId, Rect)> {
    if content_area.width == 0 || content_area.height == 0 {
        return None;
    }

    // Details Spotlight on TOP (~44%), Library Table BELOW (~56%).
    let details_pct = if content_area.height >= 24 { 44 } else { 46 };
    let table_pct = 100 - details_pct;
    let [details_area, table_area] = Layout::vertical([
        Constraint::Percentage(details_pct),
        Constraint::Percentage(table_pct),
    ])
    .areas(content_area);

    let cover_target = render_details_base(frame, total_width, details_area, view);
    render_anime_table(frame, table_area, view, table_offset);
    cover_target
}

fn render_top_bar(frame: &mut Frame<'_>, area: Rect, view: &LibraryView<'_>) {
    if area.width == 0 || area.height == 0 {
        return;
    }

    let palette = view.palette;
    let bar_style = Style::default().bg(palette.background).fg(palette.text);

    let inner = if area.height >= 2 {
        let block = Block::default()
            .borders(Borders::BOTTOM)
            .border_style(Style::default().fg(if view.filter_active {
                palette.accent
            } else {
                palette.border
            }))
            .padding(Padding::horizontal(1))
            .style(bar_style);
        let inner = block.inner(area);
        frame.render_widget(block, area);
        inner
    } else {
        frame.render_widget(Block::default().style(bar_style), area);
        area
    };

    if inner.width == 0 || inner.height == 0 {
        return;
    }

    // Left: minimal brand + user handle
    let mut brand_spans = Vec::with_capacity(4);
    brand_spans.push(Span::styled(
        "◈ ",
        Style::default()
            .fg(palette.accent)
            .add_modifier(Modifier::BOLD),
    ));
    brand_spans.push(Span::styled(
        "AniList",
        Style::default()
            .fg(palette.text)
            .add_modifier(Modifier::BOLD),
    ));
    if let Some(user) = view.username {
        if inner.width >= 68 {
            brand_spans.push(Span::styled(
                format!("  @{user}"),
                Style::default().fg(palette.muted),
            ));
        }
    }
    let brand_line = Line::from(brand_spans);
    let brand_w = if inner.width >= 48 {
        (brand_line.width() as u16).saturating_add(2).min(inner.width / 3)
    } else {
        0
    };

    // Right: calm status indicator + low quota alert
    let mut right_spans = Vec::with_capacity(4);
    if view.syncing {
        let glyph = SPINNER_FRAMES
            .get(view.sync_tick % SPINNER_FRAMES.len())
            .copied()
            .unwrap_or("⠋");
        right_spans.push(Span::styled(
            format!("{glyph} Syncing"),
            Style::default()
                .fg(palette.accent)
                .add_modifier(Modifier::BOLD),
        ));
    } else {
        right_spans.push(Span::styled("● ", Style::default().fg(palette.success)));
        right_spans.push(Span::styled(
            format!("{} titles", view.overview.total_titles),
            Style::default().fg(palette.muted),
        ));
    }

    if let Some(remaining) = view.rate_limit.remaining.filter(|&r| r <= 10) {
        let warn_color = if remaining <= 2 {
            palette.error
        } else {
            palette.warning
        };
        right_spans.push(Span::raw("  "));
        right_spans.push(Span::styled(
            format!("! API:{remaining}"),
            Style::default().fg(warn_color).add_modifier(Modifier::BOLD),
        ));
    }

    let right_line = Line::from(right_spans);
    let right_w = (right_line.width() as u16).min(inner.width / 2);

    let [brand_area, filter_area, right_area] = Layout::horizontal([
        Constraint::Length(brand_w),
        Constraint::Min(1),
        Constraint::Length(right_w),
    ])
    .areas(inner);

    if brand_w > 0 {
        frame.render_widget(Paragraph::new(brand_line), brand_area);
    }

    // Center: quiet inline filter input
    let scope_text = match view.filter_scope {
        FilterScope::CurrentList => "List",
        FilterScope::AllLists => "All",
    };

    let mut filter_spans = Vec::with_capacity(6);
    filter_spans.push(Span::styled(
        "/",
        Style::default()
            .fg(if view.filter_active {
                palette.accent
            } else {
                palette.muted
            })
            .add_modifier(Modifier::BOLD),
    ));
    filter_spans.push(Span::raw(" "));

    if view.filter_query.is_empty() {
        if view.filter_active {
            filter_spans.push(Span::styled("▏", Style::default().fg(palette.accent)));
            filter_spans.push(Span::styled(
                format!("type to filter ({scope_text})..."),
                Style::default().fg(palette.muted),
            ));
        } else {
            let hint = if filter_area.width >= 34 {
                format!("Filter {scope_text}  (Ctrl+A: scope)")
            } else {
                format!("Filter ({scope_text})")
            };
            filter_spans.push(Span::styled(hint, Style::default().fg(palette.muted)));
        }
    } else {
        filter_spans.push(Span::styled(
            view.filter_query,
            Style::default()
                .fg(palette.text)
                .add_modifier(Modifier::BOLD),
        ));
        if view.filter_active {
            filter_spans.push(Span::styled("▏", Style::default().fg(palette.accent)));
        }
        filter_spans.push(Span::styled(
            format!("  ({scope_text})"),
            Style::default().fg(if matches!(view.filter_scope, FilterScope::AllLists) {
                palette.accent
            } else {
                palette.muted
            }),
        ));
    }

    frame.render_widget(Paragraph::new(Line::from(filter_spans)), filter_area);
    frame.render_widget(
        Paragraph::new(right_line).alignment(Alignment::Right),
        right_area,
    );
}

fn render_sidebar(frame: &mut Frame<'_>, area: Rect, view: &LibraryView<'_>) {
    if area.width == 0 || area.height == 0 {
        return;
    }

    let palette = view.palette;
    let is_focused = matches!(view.focus, FocusPane::Sidebar);
    let block = Block::bordered()
        .border_type(BorderType::Rounded)
        .padding(Padding::horizontal(1))
        .title(Line::styled(
            " Collections ",
            Style::default()
                .fg(if is_focused {
                    palette.accent
                } else {
                    palette.muted
                })
                .add_modifier(Modifier::BOLD),
        ))
        .border_style(Style::default().fg(if is_focused {
            palette.accent
        } else {
            palette.border
        }));

    let inner = block.inner(area);
    frame.render_widget(block, area);
    if inner.width == 0 || inner.height == 0 {
        return;
    }

    let width = inner.width as usize;

    // Partition sidebar cleanly: Lists at top, calm Overview in middle space, Sign Out pinned bottom.
    let show_overview = inner.height >= 14;
    let show_logout_divider = inner.height >= 9;
    let logout_h = if show_logout_divider { 2 } else { 1 };
    let lists_h = (LibraryStatus::ALL.len() as u16).min(inner.height.saturating_sub(logout_h));

    let [lists_area, middle_area, logout_area] = Layout::vertical([
        Constraint::Length(lists_h),
        Constraint::Min(0),
        Constraint::Length(logout_h),
    ])
    .areas(inner);

    // 1. Collections (6 statuses)
    let mut list_items = Vec::with_capacity(LibraryStatus::ALL.len());
    let mut visual_selected = None;

    for (idx, status) in LibraryStatus::ALL.into_iter().enumerate() {
        let is_cursor = is_focused && idx == view.sidebar_index;
        if is_cursor {
            visual_selected = Some(idx);
        }

        let is_active = status == view.active_status;
        let hotkey = status.index() + 1;
        let count_str = view.counts[status.index()].to_string();
        let marker = if is_active { "▌" } else { " " };
        let label = status.label();

        let prefix_len = 3 + label.len(); // marker(1) + hotkey(1) + space(1) + label
        let pad = width.saturating_sub(prefix_len + count_str.len());

        let item = if is_cursor {
            let row_text = format!("{marker}{hotkey} {label}{}{count_str}", " ".repeat(pad));
            ListItem::new(Line::styled(
                row_text,
                Style::default()
                    .bg(palette.accent)
                    .fg(contrast_fg_on(palette.accent, palette))
                    .add_modifier(Modifier::BOLD),
            ))
        } else {
            let label_style = if is_active {
                Style::default()
                    .fg(palette.accent)
                    .add_modifier(Modifier::BOLD)
            } else {
                Style::default().fg(palette.text)
            };
            let count_style = if is_active {
                Style::default()
                    .fg(palette.accent)
                    .add_modifier(Modifier::BOLD)
            } else {
                Style::default().fg(palette.muted)
            };
            ListItem::new(Line::from(vec![
                Span::styled(
                    marker,
                    Style::default().fg(if is_active {
                        palette.accent
                    } else {
                        palette.border
                    }),
                ),
                Span::styled(format!("{hotkey} "), Style::default().fg(palette.muted)),
                Span::styled(label, label_style),
                Span::raw(" ".repeat(pad)),
                Span::styled(count_str, count_style),
            ]))
        };
        list_items.push(item);
    }

    let mut list_state = ListState::default().with_selected(visual_selected);
    frame.render_stateful_widget(List::new(list_items), lists_area, &mut list_state);

    // 2. Calm Overview block in the middle space (zero clutter, aligned key-value pairs)
    if show_overview && middle_area.height >= 6 {
        render_sidebar_overview(frame, middle_area, view.overview, palette);
    }

    // 3. Bottom-pinned Sign Out
    if logout_area.height > 0 {
        let logout_idx = SidebarItem::ALL.len().saturating_sub(1);
        let is_logout_cursor = is_focused && view.sidebar_index == logout_idx;
        let logout_style = if is_logout_cursor {
            Style::default()
                .bg(palette.error)
                .fg(contrast_fg_on(palette.error, palette))
                .add_modifier(Modifier::BOLD)
        } else {
            Style::default().fg(palette.muted)
        };

        let mut footer_lines = Vec::with_capacity(2);
        if show_logout_divider && logout_area.height >= 2 {
            footer_lines.push(Line::styled(
                "─".repeat(width),
                Style::default().fg(palette.border),
            ));
        }
        let logout_label = "  Sign Out";
        let pad = width.saturating_sub(logout_label.len());
        footer_lines.push(Line::styled(
            format!("{logout_label}{}", " ".repeat(pad)),
            logout_style,
        ));
        frame.render_widget(Paragraph::new(footer_lines), logout_area);
    }
}

fn render_sidebar_overview(
    frame: &mut Frame<'_>,
    area: Rect,
    overview: LibraryOverview,
    palette: ThemePalette,
) {
    let width = area.width as usize;
    if width < 10 || area.height < 6 {
        return;
    }

    let kv_line = |label: &'static str, value: String| -> Line<'static> {
        let pad = width.saturating_sub(2 + label.len() + value.len());
        Line::from(vec![
            Span::raw("  "),
            Span::styled(label, Style::default().fg(palette.muted)),
            Span::raw(" ".repeat(pad)),
            Span::styled(value, Style::default().fg(palette.text)),
        ])
    };

    let mean_text = overview
        .mean_avg_score
        .map_or_else(|| "—".to_owned(), |s| format!("{s}%"));

    let lines = vec![
        Line::raw(""),
        Line::styled(
            "  OVERVIEW",
            Style::default()
                .fg(palette.muted)
                .add_modifier(Modifier::BOLD),
        ),
        kv_line("Titles", overview.total_titles.to_string()),
        kv_line("Episodes", overview.episodes_watched.to_string()),
        kv_line("Rated", overview.rated_count.to_string()),
        kv_line("Mean Score", mean_text),
    ];

    frame.render_widget(Paragraph::new(lines), area);
}

fn render_compact_strip(frame: &mut Frame<'_>, area: Rect, view: &LibraryView<'_>) {
    if area.width == 0 || area.height == 0 {
        return;
    }

    let palette = view.palette;
    let is_focused = matches!(view.focus, FocusPane::Sidebar);
    let block = Block::bordered()
        .border_type(BorderType::Rounded)
        .padding(Padding::horizontal(1))
        .title(Line::styled(
            " Collections (1-6) ",
            Style::default()
                .fg(if is_focused {
                    palette.accent
                } else {
                    palette.muted
                })
                .add_modifier(Modifier::BOLD),
        ))
        .border_style(Style::default().fg(if is_focused {
            palette.accent
        } else {
            palette.border
        }));

    let inner = block.inner(area);
    frame.render_widget(block, area);
    if inner.width == 0 || inner.height == 0 {
        return;
    }

    let full_total: usize = SidebarItem::ALL
        .iter()
        .copied()
        .enumerate()
        .map(|(i, item)| {
            let label_len = match item {
                SidebarItem::Status(s) => {
                    format!("{}({})", item.label(), view.counts[s.index()]).len()
                }
                SidebarItem::LogOut => "Sign Out".len(),
            };
            label_len + usize::from(i > 0)
        })
        .sum();
    let use_full = full_total <= inner.width as usize;

    let mut spans = Vec::with_capacity(SidebarItem::ALL.len() * 2);
    for (idx, item) in SidebarItem::ALL.iter().copied().enumerate() {
        if idx > 0 {
            spans.push(Span::raw(" "));
        }
        let is_cursor = is_focused && idx == view.sidebar_index;
        match item {
            SidebarItem::Status(status) => {
                let is_active = status == view.active_status;
                let count = view.counts[status.index()];
                let text = if use_full || is_active {
                    format!("{}({count})", item.label())
                } else {
                    format!("{}({count})", compact_status_label(status))
                };
                let style = if is_cursor {
                    Style::default()
                        .bg(palette.accent)
                        .fg(contrast_fg_on(palette.accent, palette))
                        .add_modifier(Modifier::BOLD)
                } else if is_active {
                    Style::default()
                        .fg(palette.accent)
                        .add_modifier(Modifier::BOLD)
                } else {
                    Style::default().fg(palette.text)
                };
                spans.push(Span::styled(text, style));
            }
            SidebarItem::LogOut => {
                let text = if use_full { "Sign Out" } else { "Out" };
                let style = if is_cursor {
                    Style::default()
                        .bg(palette.error)
                        .fg(contrast_fg_on(palette.error, palette))
                        .add_modifier(Modifier::BOLD)
                } else {
                    Style::default().fg(palette.muted)
                };
                spans.push(Span::styled(text, style));
            }
        }
    }

    frame.render_widget(Paragraph::new(Line::from(spans)), inner);
}

const fn compact_status_label(status: LibraryStatus) -> &'static str {
    match status {
        LibraryStatus::Watching => "Watch",
        LibraryStatus::Planning => "Plan",
        LibraryStatus::Completed => "Comp",
        LibraryStatus::Dropped => "Drop",
        LibraryStatus::Paused => "Paus",
        LibraryStatus::Repeating => "Rep",
    }
}

#[derive(Clone, Copy)]
enum TableLayoutMode {
    WideWithList,
    Wide,
    Medium,
    Compact,
}

fn render_anime_table(
    frame: &mut Frame<'_>,
    area: Rect,
    view: &LibraryView<'_>,
    table_offset: &mut usize,
) {
    if area.width == 0 || area.height == 0 {
        return;
    }

    let palette = view.palette;
    let is_focused = matches!(view.focus, FocusPane::Table);
    let show_all_lists = matches!(view.filter_scope, FilterScope::AllLists);
    let count = view.visible_items.len();

    let title = if show_all_lists {
        format!(" All Lists • {count} ")
    } else {
        format!(" {} • {count} ", view.active_status.label())
    };

    let mut block = Block::bordered()
        .border_type(BorderType::Rounded)
        .padding(Padding::horizontal(1))
        .title(Line::styled(
            title,
            Style::default()
                .fg(if is_focused {
                    palette.accent
                } else {
                    palette.muted
                })
                .add_modifier(Modifier::BOLD),
        ))
        .border_style(Style::default().fg(if is_focused {
            palette.accent
        } else {
            palette.border
        }));

    if area.width >= 38 {
        let sort_badge = format!(
            " Sort: {} {} ",
            view.sort_field.short_label(),
            view.sort_direction.arrow()
        );
        block = block.title_top(
            Line::styled(sort_badge, Style::default().fg(palette.muted))
                .alignment(Alignment::Right),
        );
    }

    let inner = block.inner(area);
    frame.render_widget(block, area);
    if inner.width == 0 || inner.height == 0 {
        return;
    }

    if view.visible_items.is_empty() {
        let trimmed = view.filter_query.trim();
        let msg = if trimmed.is_empty() {
            Cow::Borrowed("No anime in this list")
        } else {
            Cow::Owned(format!("No matches for '{trimmed}'"))
        };
        let v_pad = inner.height / 2;
        let msg_area = Rect::new(inner.x, inner.y.saturating_add(v_pad), inner.width, 1);
        frame.render_widget(
            Paragraph::new(Line::styled(msg, Style::default().fg(palette.muted)))
                .alignment(Alignment::Center),
            msg_area,
        );
        return;
    }

    let mode = if inner.width >= 76 && show_all_lists {
        TableLayoutMode::WideWithList
    } else if inner.width >= 68 {
        TableLayoutMode::Wide
    } else if inner.width >= 48 {
        TableLayoutMode::Medium
    } else {
        TableLayoutMode::Compact
    };

    let header_style = Style::default()
        .fg(palette.muted)
        .add_modifier(Modifier::BOLD);

    let (header, widths): (Row<'_>, Vec<Constraint>) = match mode {
        TableLayoutMode::WideWithList => (
            Row::new(["Title", "List", "Progress", "Rate", "Avg", "Airing"]).style(header_style),
            vec![
                Constraint::Min(20),
                Constraint::Length(10),
                Constraint::Length(10),
                Constraint::Length(5),
                Constraint::Length(5),
                Constraint::Length(14),
            ],
        ),
        TableLayoutMode::Wide => (
            Row::new(["Title", "Progress", "Rate", "Avg", "Airing"]).style(header_style),
            vec![
                Constraint::Min(22),
                Constraint::Length(10),
                Constraint::Length(5),
                Constraint::Length(5),
                Constraint::Length(16),
            ],
        ),
        TableLayoutMode::Medium => (
            Row::new(["Title", "Progress", "Rate", "Avg"]).style(header_style),
            vec![
                Constraint::Min(14),
                Constraint::Length(10),
                Constraint::Length(5),
                Constraint::Length(5),
            ],
        ),
        TableLayoutMode::Compact => (
            Row::new(["Title", "Progress", "Rate"]).style(header_style),
            vec![
                Constraint::Min(10),
                Constraint::Length(9),
                Constraint::Length(5),
            ],
        ),
    };

    let mut rows = Vec::with_capacity(view.visible_items.len());
    for item in view.visible_items {
        let title_cell: Cow<'_, str> =
            if show_all_lists && !matches!(mode, TableLayoutMode::WideWithList) {
                Cow::Owned(format!("{} [{}]", item.title, item.entry.status.label()))
            } else {
                Cow::Borrowed(item.title.as_str())
            };
        let progress = format_table_progress(item);
        let rating_cell = rating_cell(item.entry.score, view.score_format, palette);

        let row = match mode {
            TableLayoutMode::WideWithList => {
                let status_str = item.release_status_display();
                let avg = format_avg_score(item.average_score);
                Row::new([
                    Cell::from(title_cell),
                    Cell::from(item.entry.status.label())
                        .style(Style::default().fg(palette.secondary)),
                    Cell::from(progress),
                    rating_cell,
                    Cell::from(avg),
                    Cell::from(status_str).style(Style::default().fg(palette.muted)),
                ])
            }
            TableLayoutMode::Wide => {
                let status_str = item.release_status_display();
                let avg = format_avg_score(item.average_score);
                Row::new([
                    Cell::from(title_cell),
                    Cell::from(progress),
                    rating_cell,
                    Cell::from(avg),
                    Cell::from(status_str).style(Style::default().fg(palette.muted)),
                ])
            }
            TableLayoutMode::Medium => {
                let avg = format_avg_score(item.average_score);
                Row::new([
                    Cell::from(title_cell),
                    Cell::from(progress),
                    rating_cell,
                    Cell::from(avg),
                ])
            }
            TableLayoutMode::Compact => {
                Row::new([Cell::from(title_cell), Cell::from(progress), rating_cell])
            }
        };
        rows.push(row);
    }

    let selected = view
        .table_selected
        .min(view.visible_items.len().saturating_sub(1));
    let mut table_state = TableState::default()
        .with_selected(Some(selected))
        .with_offset(*table_offset);

    let highlight_style = if is_focused {
        Style::default()
            .bg(palette.accent)
            .fg(contrast_fg_on(palette.accent, palette))
            .add_modifier(Modifier::BOLD)
    } else {
        unfocused_highlight_style(palette)
    };

    let table = Table::new(rows, widths)
        .header(header)
        .column_spacing(2)
        .row_highlight_style(highlight_style)
        .highlight_symbol("▌ ");

    frame.render_stateful_widget(table, inner, &mut table_state);
    *table_offset = table_state.offset();
}

fn format_table_progress(item: &AnimeItem) -> String {
    match item.episodes {
        Some(total) => format!("{} / {total}", item.entry.progress),
        None => format!("{} / ?", item.entry.progress),
    }
}

fn rating_cell<'a>(
    score: Option<ScoreRaw>,
    format: ScoreFormat,
    palette: ThemePalette,
) -> Cell<'a> {
    let Some(raw) = score else {
        return Cell::from("·").style(Style::default().fg(palette.muted));
    };
    let color = match raw.get() {
        73..=100 => palette.success,
        48..=72 => palette.warning,
        _ => palette.error,
    };
    Cell::from(format.format_table(Some(raw))).style(Style::default().fg(color))
}

fn format_avg_score(avg: Option<u8>) -> Cow<'static, str> {
    match avg {
        Some(score) => Cow::Owned(format!("{score}%")),
        None => Cow::Borrowed("—"),
    }
}

fn render_details_base(
    frame: &mut Frame<'_>,
    total_width: u16,
    area: Rect,
    view: &LibraryView<'_>,
) -> Option<(MediaId, Rect)> {
    if area.width == 0 || area.height == 0 {
        return None;
    }

    let palette = view.palette;
    let is_focused = matches!(view.focus, FocusPane::Details);
    let title = if is_focused && area.width >= 32 {
        " Spotlight (j/k: Scroll) "
    } else {
        " Spotlight "
    };

    let block = Block::bordered()
        .border_type(BorderType::Rounded)
        .padding(Padding::horizontal(1))
        .title(Line::styled(
            title,
            Style::default()
                .fg(if is_focused {
                    palette.accent
                } else {
                    palette.muted
                })
                .add_modifier(Modifier::BOLD),
        ))
        .border_style(Style::default().fg(if is_focused {
            palette.accent
        } else {
            palette.border
        }));

    let inner = block.inner(area);
    frame.render_widget(block, area);
    if inner.width == 0 || inner.height == 0 {
        return None;
    }

    let Some(item) = view.selected_item else {
        let v_pad = inner.height / 2;
        let msg_area = Rect::new(inner.x, inner.y.saturating_add(v_pad), inner.width, 1);
        frame.render_widget(
            Paragraph::new(Line::styled(
                "Select an anime below to inspect details",
                Style::default().fg(palette.muted),
            ))
            .alignment(Alignment::Center),
            msg_area,
        );
        return None;
    };

    let (cover_area, info_area) = split_details_area(total_width, inner);
    render_details_info(
        frame,
        info_area,
        item,
        palette,
        view.description_scroll,
        view.score_format,
    );

    if cover_area.width > 0 && cover_area.height > 0 {
        Some((item.id, cover_area))
    } else {
        None
    }
}

fn split_details_area(total_width: u16, inner: Rect) -> (Rect, Rect) {
    if total_width >= 62 && inner.width >= 34 && inner.height >= 5 {
        let raw_cover = ((u32::from(inner.width) * 22) / 100) as u16;
        let max_cover = inner.width.saturating_sub(24);
        let cover_w = raw_cover.clamp(10, 26).min(max_cover);
        if cover_w >= 8 {
            let [cover_area, _gap, info_area] = Layout::horizontal([
                Constraint::Length(cover_w),
                Constraint::Length(2),
                Constraint::Min(1),
            ])
            .areas(inner);
            return (cover_area, info_area);
        }
    }
    (Rect::default(), inner)
}

fn render_details_info(
    frame: &mut Frame<'_>,
    area: Rect,
    item: &AnimeItem,
    palette: ThemePalette,
    description_scroll: u16,
    score_format: ScoreFormat,
) {
    if area.width == 0 || area.height == 0 {
        return;
    }

    let title_line = Line::from(vec![Span::styled(
        item.title.as_str(),
        Style::default()
            .fg(palette.accent)
            .add_modifier(Modifier::BOLD),
    )]);

    let alt_line = item
        .alt_title
        .as_deref()
        .filter(|s| !s.is_empty())
        .map(|alt| Line::styled(alt, Style::default().fg(palette.muted)));

    let progress_text = item.progress_display();
    let release_status = item.release_status_display();
    let avg_score = format_avg_score(item.average_score);

    let stats_line = Line::from(vec![
        Span::styled(
            progress_text.as_str(),
            Style::default()
                .fg(palette.text)
                .add_modifier(Modifier::BOLD),
        ),
        Span::styled("  ·  ", Style::default().fg(palette.border)),
        Span::styled(release_status.as_ref(), Style::default().fg(palette.secondary)),
        Span::styled("  ·  ", Style::default().fg(palette.border)),
        Span::styled(
            format!("{avg_score} avg"),
            Style::default().fg(palette.muted),
        ),
    ]);

    let genres_text = if item.genres.is_empty() {
        "No genres listed".to_owned()
    } else {
        item.genres.iter().take(5).cloned().collect::<Vec<_>>().join(" · ")
    };
    let genre_line = Line::styled(genres_text, Style::default().fg(palette.muted));

    let key_style = Style::default()
        .fg(palette.accent)
        .add_modifier(Modifier::BOLD);
    let label_style = Style::default().fg(palette.text);
    let muted_style = Style::default().fg(palette.muted);

    let rate_text = score_format.format_badge(item.entry.score);
    let web_key_style = if item.site_url.is_some() {
        key_style
    } else {
        muted_style
    };
    let web_lbl_style = if item.site_url.is_some() {
        label_style
    } else {
        muted_style
    };

    let controls_line = Line::from(vec![
        Span::styled("-/+", key_style),
        Span::styled(" Ep   ", label_style),
        Span::styled("a", key_style),
        Span::styled(format!(" {}   ", item.entry.status.label()), label_style),
        Span::styled("r", key_style),
        Span::styled(format!(" {rate_text}   "), label_style),
        Span::styled("v", web_key_style),
        Span::styled(" AniList", web_lbl_style),
    ]);

    if area.height >= 8 {
        let alt_rows = u16::from(alt_line.is_some());
        let [
            title_area,
            alt_area,
            stats_area,
            genres_area,
            ctrl_area,
            spacer_area,
            desc_area,
        ] = Layout::vertical([
            Constraint::Length(1),
            Constraint::Length(alt_rows),
            Constraint::Length(1),
            Constraint::Length(1),
            Constraint::Length(1),
            Constraint::Length(1),
            Constraint::Min(1),
        ])
        .areas(area);

        let _ = spacer_area;
        frame.render_widget(Paragraph::new(title_line), title_area);
        if let Some(alt) = alt_line {
            frame.render_widget(Paragraph::new(alt), alt_area);
        }
        frame.render_widget(Paragraph::new(stats_line), stats_area);
        frame.render_widget(Paragraph::new(genre_line), genres_area);
        frame.render_widget(Paragraph::new(controls_line), ctrl_area);
        render_description(
            frame,
            desc_area,
            &item.description_text,
            palette,
            description_scroll,
        );
    } else if area.height >= 4 {
        let [title_area, stats_area, ctrl_area, desc_area] = Layout::vertical([
            Constraint::Length(1),
            Constraint::Length(1),
            Constraint::Length(1),
            Constraint::Min(0),
        ])
        .areas(area);

        frame.render_widget(Paragraph::new(title_line), title_area);
        frame.render_widget(Paragraph::new(stats_line), stats_area);
        frame.render_widget(Paragraph::new(controls_line), ctrl_area);
        if desc_area.height > 0 {
            render_description(
                frame,
                desc_area,
                &item.description_text,
                palette,
                description_scroll,
            );
        }
    } else {
        let [title_area, stats_area] =
            Layout::vertical([Constraint::Length(1), Constraint::Min(0)]).areas(area);
        frame.render_widget(Paragraph::new(title_line), title_area);
        if stats_area.height > 0 {
            frame.render_widget(Paragraph::new(stats_line), stats_area);
        }
    }
}

fn render_description(
    frame: &mut Frame<'_>,
    area: Rect,
    description: &str,
    palette: ThemePalette,
    scroll: u16,
) {
    if area.width == 0 || area.height == 0 {
        return;
    }
    let para = Paragraph::new(description)
        .style(Style::default().fg(palette.text))
        .wrap(Wrap { trim: false })
        .scroll((scroll, 0));
    frame.render_widget(para, area);
}

struct CoverSlot<'a> {
    cover_area: Rect,
    media_id: MediaId,
    palette: ThemePalette,
    proto: Option<&'a mut StatefulProtocol>,
    is_loading: bool,
    last_cover_area: &'a mut Rect,
    pending_reencode: &'a mut Option<(MediaId, Rect)>,
}

fn render_cover_slot(frame: &mut Frame<'_>, slot: CoverSlot<'_>) {
    if slot.cover_area.width == 0 || slot.cover_area.height == 0 {
        return;
    }

    *slot.last_cover_area = slot.cover_area;

    match slot.proto {
        Some(proto) => {
            let resize = Resize::Fit(None);
            if proto.needs_resize(&resize, slot.cover_area).is_some() {
                *slot.pending_reencode = Some((slot.media_id, slot.cover_area));
                render_cover_placeholder(
                    frame,
                    slot.cover_area,
                    "Sizing...",
                    slot.palette,
                );
            } else {
                frame.render_stateful_widget(
                    StatefulImage::default().resize(resize),
                    slot.cover_area,
                    proto,
                );
            }
        }
        None => {
            let label = if slot.is_loading {
                "Loading..."
            } else {
                "No Cover"
            };
            render_cover_placeholder(frame, slot.cover_area, label, slot.palette);
        }
    }
}

fn render_cover_placeholder(
    frame: &mut Frame<'_>,
    area: Rect,
    label: &str,
    palette: ThemePalette,
) {
    let style = Style::default().bg(palette.surface).fg(palette.muted);
    if area.width >= 6 && area.height >= 3 {
        let block = Block::bordered()
            .border_type(BorderType::Rounded)
            .border_style(Style::default().fg(palette.border))
            .style(style);
        let inner = block.inner(area);
        frame.render_widget(block, area);
        if inner.width > 0 && inner.height > 0 {
            let y = inner.y.saturating_add(inner.height / 2);
            let text_area = Rect::new(inner.x, y, inner.width, 1);
            frame.render_widget(
                Paragraph::new(Line::styled(label, Style::default().fg(palette.muted)))
                    .alignment(Alignment::Center),
                text_area,
            );
        }
    } else {
        frame.render_widget(
            Paragraph::new(Line::styled(label, style)).alignment(Alignment::Center),
            area,
        );
    }
}

fn render_footer(frame: &mut Frame<'_>, area: Rect, view: &LibraryView<'_>) {
    if area.width == 0 || area.height == 0 {
        return;
    }

    let palette = view.palette;
    let bar_bg = palette.background;

    if let Some((message, level)) = view.toast {
        let (prefix, color) = match level {
            ToastLevel::Info => ("  ℹ ", palette.secondary),
            ToastLevel::Success => ("  ✓ ", palette.success),
            ToastLevel::Warning => ("  ! ", palette.warning),
            ToastLevel::Error => ("  ✗ ", palette.error),
        };
        let line = Line::from(vec![
            Span::styled(
                prefix,
                Style::default()
                    .bg(bar_bg)
                    .fg(color)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::styled(
                message,
                Style::default()
                    .bg(bar_bg)
                    .fg(color)
                    .add_modifier(Modifier::BOLD),
            ),
        ]);
        frame.render_widget(Paragraph::new(line).style(Style::default().bg(bar_bg)), area);
    } else {
        let hint = if view.filter_active {
            FILTER_FOOTER_HINT
        } else {
            match view.focus {
                FocusPane::Sidebar => SIDEBAR_FOOTER_HINT,
                FocusPane::Table => TABLE_FOOTER_HINT,
                FocusPane::Details => DETAILS_FOOTER_HINT,
            }
        };
        frame.render_widget(
            Paragraph::new(Line::styled(
                format!("  {hint}"),
                Style::default().bg(bar_bg).fg(palette.muted),
            ))
            .style(Style::default().bg(bar_bg)),
            area,
        );
    }
}

struct ModalContext<'a> {
    status: Option<LibraryStatus>,
    score: Option<ScoreRaw>,
    score_format: ScoreFormat,
    score_pref: ScoreFormatPreference,
    api_score_format: Option<ScoreFormat>,
    theme_name: &'a str,
    rating_input: &'a str,
    rating_input_error: Option<&'a str>,
    sort_field: SortField,
    sort_direction: SortDirection,
}

fn render_modal(
    frame: &mut Frame<'_>,
    root: Rect,
    modal: &ActiveModal,
    palette: ThemePalette,
    ctx: &ModalContext<'_>,
) {
    if root.width == 0 || root.height == 0 {
        return;
    }

    match modal {
        ActiveModal::None => {}
        ActiveModal::StatusPicker { selected } => {
            render_status_picker_modal(frame, root, *selected, palette, ctx.status);
        }
        ActiveModal::RatingPicker { selected } => {
            render_rating_picker_modal(frame, root, *selected, palette, ctx);
        }
        ActiveModal::SortPicker { selected } => {
            render_sort_picker_modal(frame, root, *selected, palette, ctx);
        }
        ActiveModal::Help => {
            render_help_modal(frame, root, palette, ctx);
        }
    }
}

fn render_sort_picker_modal(
    frame: &mut Frame<'_>,
    root: Rect,
    selected: usize,
    palette: ThemePalette,
    ctx: &ModalContext<'_>,
) {
    let popup = centered_rect(root, 42, 10);
    frame.render_widget(Clear, popup);

    let block = Block::bordered()
        .border_type(BorderType::Rounded)
        .padding(Padding::horizontal(1))
        .title(Line::styled(
            " Sort By ",
            Style::default()
                .fg(palette.accent)
                .add_modifier(Modifier::BOLD),
        ))
        .border_style(Style::default().fg(palette.accent))
        .style(Style::default().bg(palette.surface).fg(palette.text));

    let inner = block.inner(popup);
    frame.render_widget(block, popup);
    if inner.width == 0 || inner.height == 0 {
        return;
    }

    let [list_area, hint_area] = if inner.height >= 7 {
        Layout::vertical([Constraint::Min(1), Constraint::Length(1)]).areas(inner)
    } else {
        [inner, Rect::default()]
    };

    let selected_idx = selected.min(SortField::ALL.len().saturating_sub(1));
    let items: Vec<ListItem<'_>> = SortField::ALL
        .into_iter()
        .enumerate()
        .map(|(idx, field)| {
            let is_sel = idx == selected_idx;
            let is_cur = field == ctx.sort_field;
            let label = if is_cur {
                format!("{}  ({})", field.label(), ctx.sort_direction.arrow())
            } else {
                field.label().to_owned()
            };
            modal_option_item(&label, is_sel, is_cur, palette)
        })
        .collect();

    let mut list_state = ListState::default().with_selected(Some(selected_idx));
    frame.render_stateful_widget(List::new(items), list_area, &mut list_state);

    if hint_area.height > 0 {
        frame.render_widget(
            Paragraph::new(Line::styled(
                "j/k Move  Enter Apply  r Flip  Esc",
                Style::default().fg(palette.muted),
            ))
            .alignment(Alignment::Center),
            hint_area,
        );
    }
}

fn render_status_picker_modal(
    frame: &mut Frame<'_>,
    root: Rect,
    selected: usize,
    palette: ThemePalette,
    current_status: Option<LibraryStatus>,
) {
    let popup = centered_rect(root, 40, 12);
    frame.render_widget(Clear, popup);

    let block = Block::bordered()
        .border_type(BorderType::Rounded)
        .padding(Padding::horizontal(1))
        .title(Line::styled(
            " Move to Collection ",
            Style::default()
                .fg(palette.accent)
                .add_modifier(Modifier::BOLD),
        ))
        .border_style(Style::default().fg(palette.accent))
        .style(Style::default().bg(palette.surface).fg(palette.text));

    let inner = block.inner(popup);
    frame.render_widget(block, popup);
    if inner.width == 0 || inner.height == 0 {
        return;
    }

    let [list_area, hint_area] = if inner.height >= 9 {
        Layout::vertical([Constraint::Min(1), Constraint::Length(1)]).areas(inner)
    } else {
        [inner, Rect::default()]
    };

    let selected_idx = selected.min(6);
    let mut items = Vec::with_capacity(7);
    for (idx, status) in LibraryStatus::ALL.into_iter().enumerate() {
        let is_sel = idx == selected_idx;
        let is_cur = current_status == Some(status);
        let prefix = if is_sel { "▌ " } else { "  " };
        let suffix = if is_cur { " ✓" } else { "" };
        let text = format!("{prefix}{}{suffix}", status.label());
        let style = if is_sel {
            Style::default()
                .bg(palette.accent)
                .fg(contrast_fg_on(palette.accent, palette))
                .add_modifier(Modifier::BOLD)
        } else if is_cur {
            Style::default()
                .fg(palette.accent)
                .add_modifier(Modifier::BOLD)
        } else {
            Style::default().fg(palette.text)
        };
        items.push(ListItem::new(Line::styled(text, style)));
    }

    let remove_sel = selected_idx == 6;
    let remove_text = if remove_sel {
        "▌ Remove from Library"
    } else {
        "  Remove from Library"
    };
    let remove_style = if remove_sel {
        Style::default()
            .bg(palette.error)
            .fg(contrast_fg_on(palette.error, palette))
            .add_modifier(Modifier::BOLD)
    } else {
        Style::default().fg(palette.error)
    };
    items.push(ListItem::new(Line::styled(remove_text, remove_style)));

    let mut list_state = ListState::default().with_selected(Some(selected_idx));
    frame.render_stateful_widget(List::new(items), list_area, &mut list_state);

    if hint_area.height > 0 {
        frame.render_widget(
            Paragraph::new(Line::styled(
                "j/k Move   Enter Apply   Esc Cancel",
                Style::default().fg(palette.muted),
            ))
            .alignment(Alignment::Center),
            hint_area,
        );
    }
}

fn modal_option_item<'a>(
    label: &str,
    is_sel: bool,
    is_cur: bool,
    palette: ThemePalette,
) -> ListItem<'a> {
    let prefix = if is_sel { "▌ " } else { "  " };
    let suffix = if is_cur { " ✓" } else { "" };
    let text = format!("{prefix}{label}{suffix}");
    let style = if is_sel {
        Style::default()
            .bg(palette.accent)
            .fg(contrast_fg_on(palette.accent, palette))
            .add_modifier(Modifier::BOLD)
    } else if is_cur {
        Style::default()
            .fg(palette.accent)
            .add_modifier(Modifier::BOLD)
    } else {
        Style::default().fg(palette.text)
    };
    ListItem::new(Line::styled(text, style))
}

fn render_rating_picker_modal(
    frame: &mut Frame<'_>,
    root: Rect,
    selected: usize,
    palette: ThemePalette,
    ctx: &ModalContext<'_>,
) {
    if !ctx.score_format.uses_choice_picker() {
        render_numeric_rating_modal(frame, root, palette, ctx);
        return;
    }

    let is_point5 = ctx.score_format == ScoreFormat::Point5;
    let popup_height = if is_point5 { 11 } else { 9 };
    let popup = centered_rect(root, 36, popup_height);
    frame.render_widget(Clear, popup);

    let block = Block::bordered()
        .border_type(BorderType::Rounded)
        .padding(Padding::horizontal(1))
        .title(Line::styled(
            " Rate Anime ",
            Style::default()
                .fg(palette.accent)
                .add_modifier(Modifier::BOLD),
        ))
        .border_style(Style::default().fg(palette.accent))
        .style(Style::default().bg(palette.surface).fg(palette.text));

    let inner = block.inner(popup);
    frame.render_widget(block, popup);
    if inner.width == 0 || inner.height == 0 {
        return;
    }

    let [list_area, hint_area] = if inner.height >= 6 {
        Layout::vertical([Constraint::Min(1), Constraint::Length(1)]).areas(inner)
    } else {
        [inner, Rect::default()]
    };

    let (clear_idx, mut items) = if is_point5 {
        let cur_stars = ctx.score.map(ScoreFormat::raw_to_point5);
        let selected_idx = selected.min(5);
        let list = (0..5u8)
            .map(|idx| {
                let stars = 5 - idx;
                let label = format!("{stars}★");
                modal_option_item(
                    &label,
                    usize::from(idx) == selected_idx,
                    cur_stars == Some(stars),
                    palette,
                )
            })
            .collect::<Vec<_>>();
        (5usize, list)
    } else {
        let cur_p3 = ctx.score.map(Point3Score::from_score_raw);
        let selected_idx = selected.min(3);
        let list = Point3Score::ALL
            .into_iter()
            .enumerate()
            .map(|(idx, score)| {
                modal_option_item(
                    score.label(),
                    idx == selected_idx,
                    cur_p3 == Some(score),
                    palette,
                )
            })
            .collect::<Vec<_>>();
        (3usize, list)
    };

    let selected_idx = selected.min(clear_idx);
    let clear_sel = selected_idx == clear_idx;
    let clear_text = if clear_sel {
        "▌ Clear Rating"
    } else {
        "  Clear Rating"
    };
    let clear_style = if clear_sel {
        Style::default()
            .bg(palette.warning)
            .fg(contrast_fg_on(palette.warning, palette))
            .add_modifier(Modifier::BOLD)
    } else {
        Style::default().fg(palette.warning)
    };
    items.push(ListItem::new(Line::styled(clear_text, clear_style)));

    let mut list_state = ListState::default().with_selected(Some(selected_idx));
    frame.render_stateful_widget(List::new(items), list_area, &mut list_state);

    if hint_area.height > 0 {
        let hint = if is_point5 {
            "j/k Move  1-5 Pick  c Clear  Esc"
        } else {
            "j/k Move  Enter Apply  c Clear  Esc"
        };
        frame.render_widget(
            Paragraph::new(Line::styled(hint, Style::default().fg(palette.muted)))
                .alignment(Alignment::Center),
            hint_area,
        );
    }
}

fn render_numeric_rating_modal(
    frame: &mut Frame<'_>,
    root: Rect,
    palette: ThemePalette,
    ctx: &ModalContext<'_>,
) {
    let popup = centered_rect(root, 44, 9);
    frame.render_widget(Clear, popup);

    let title = format!(" Rate Anime ({}) ", ctx.score_format.short_label());
    let block = Block::bordered()
        .border_type(BorderType::Rounded)
        .padding(Padding::horizontal(1))
        .title(Line::styled(
            title,
            Style::default()
                .fg(palette.accent)
                .add_modifier(Modifier::BOLD),
        ))
        .border_style(Style::default().fg(palette.accent))
        .style(Style::default().bg(palette.surface).fg(palette.text));

    let inner = block.inner(popup);
    frame.render_widget(block, popup);
    if inner.width == 0 || inner.height == 0 {
        return;
    }

    let range_prompt = match ctx.score_format {
        ScoreFormat::Point10Decimal => "Enter score (0.0 – 10.0), or leave empty to clear",
        ScoreFormat::Point10 => "Enter score (0 – 10), or leave empty to clear",
        ScoreFormat::Point100 => "Enter score (0 – 100), or leave empty to clear",
        ScoreFormat::Point5 | ScoreFormat::Point3 => "",
    };

    let [prompt_area, input_area, status_area, hint_area] = if inner.height >= 6 {
        Layout::vertical([
            Constraint::Length(1),
            Constraint::Length(3),
            Constraint::Min(1),
            Constraint::Length(1),
        ])
        .areas(inner)
    } else {
        [inner, Rect::default(), Rect::default(), Rect::default()]
    };

    frame.render_widget(
        Paragraph::new(Line::styled(range_prompt, Style::default().fg(palette.muted))),
        prompt_area,
    );

    if input_area.height > 0 {
        let input_block = Block::bordered()
            .border_type(BorderType::Rounded)
            .border_style(Style::default().fg(palette.accent));
        let display = if ctx.rating_input.is_empty() {
            Line::from(Span::styled(
                "unrated ▏",
                Style::default().fg(palette.muted),
            ))
        } else {
            Line::from(Span::styled(
                format!("{}▏", ctx.rating_input),
                Style::default()
                    .fg(palette.text)
                    .add_modifier(Modifier::BOLD),
            ))
        };
        frame.render_widget(Paragraph::new(display).block(input_block), input_area);
    }

    if status_area.height > 0 {
        if let Some(err) = ctx.rating_input_error {
            frame.render_widget(
                Paragraph::new(Line::styled(err, Style::default().fg(palette.error))),
                status_area,
            );
        } else {
            let current_line = ctx
                .score
                .map(|_| format!("Current: {}", ctx.score_format.format_badge(ctx.score)))
                .unwrap_or_else(|| "Current: unrated".to_owned());
            frame.render_widget(
                Paragraph::new(Line::styled(current_line, Style::default().fg(palette.muted))),
                status_area,
            );
        }
    }

    if hint_area.height > 0 {
        frame.render_widget(
            Paragraph::new(Line::styled(
                "Type / ↑↓ Step   Enter Save   c Clear   Esc",
                Style::default().fg(palette.muted),
            ))
            .alignment(Alignment::Center),
            hint_area,
        );
    }
}

fn render_help_modal(
    frame: &mut Frame<'_>,
    root: Rect,
    palette: ThemePalette,
    ctx: &ModalContext<'_>,
) {
    let popup = centered_rect(root, 70, 30);
    frame.render_widget(Clear, popup);

    let block = Block::bordered()
        .border_type(BorderType::Rounded)
        .padding(Padding::horizontal(1))
        .title(Line::styled(
            " Help & Settings ",
            Style::default()
                .fg(palette.accent)
                .add_modifier(Modifier::BOLD),
        ))
        .border_style(Style::default().fg(palette.accent))
        .style(Style::default().bg(palette.surface).fg(palette.text));

    let inner = block.inner(popup);
    frame.render_widget(block, popup);
    if inner.width == 0 || inner.height == 0 {
        return;
    }

    let section_style = Style::default()
        .fg(palette.accent)
        .add_modifier(Modifier::BOLD);
    let key_style = Style::default()
        .fg(palette.secondary)
        .add_modifier(Modifier::BOLD);
    let desc_style = Style::default().fg(palette.text);
    let val_style = Style::default()
        .fg(palette.accent)
        .add_modifier(Modifier::BOLD);

    let help_row = |keys: &'static str, desc: &'static str| -> Line<'static> {
        Line::from(vec![
            Span::styled(format!("  {keys:<16}"), key_style),
            Span::styled(desc, desc_style),
        ])
    };

    let theme_label = ThemePalette::display_name(ctx.theme_name).to_owned();
    let rating_label = match ctx.score_pref {
        ScoreFormatPreference::Auto => {
            let detail = ctx
                .api_score_format
                .map_or("default 9.8/10", ScoreFormat::short_label);
            format!("Auto (AniList: {detail})")
        }
        ScoreFormatPreference::Override(fmt) => fmt.label().to_owned(),
    };

    let lines = vec![
        Line::styled("Settings (press key to cycle)", section_style),
        Line::from(vec![
            Span::styled(format!("  {:<16}", "t"), key_style),
            Span::styled("Theme: ", desc_style),
            Span::styled(theme_label, val_style),
        ]),
        Line::from(vec![
            Span::styled(format!("  {:<16}", "s"), key_style),
            Span::styled("Rating system: ", desc_style),
            Span::styled(rating_label, val_style),
        ]),
        Line::raw(""),
        Line::styled("Navigation", section_style),
        help_row("Tab / h / l", "Focus left workspace / right Collections"),
        help_row("1 - 6", "Switch directly to library collection"),
        help_row("j / k or ↑ / ↓", "Move cursor or scroll spotlight"),
        help_row("J / K or PgUp/Dn", "Scroll anime synopsis"),
        help_row("g / G", "Jump to top / bottom"),
        help_row("Enter", "Activate collection / confirm"),
        Line::raw(""),
        Line::styled("Filter & Sync", section_style),
        help_row("/", "Focus instant local filter"),
        help_row("Ctrl+A", "Toggle scope (List / All)"),
        help_row("s / S", "Sort list / reverse sort order"),
        help_row("Esc", "Clear filter / unfocus / close modal"),
        help_row("R", "Refresh library from AniList"),
        Line::raw(""),
        Line::styled("Library Actions", section_style),
        help_row("+ / i, - / d", "Increment / decrement episode progress"),
        help_row("a", "Move to collection or remove entry"),
        help_row("r", "Rate anime or clear rating"),
        help_row("v", "Open anime page on AniList in browser"),
        help_row("? / q / Ctrl+C", "Toggle help & settings / Quit"),
    ];

    let [body_area, hint_area] = if inner.height >= 4 {
        Layout::vertical([Constraint::Min(1), Constraint::Length(1)]).areas(inner)
    } else {
        [inner, Rect::default()]
    };

    frame.render_widget(Paragraph::new(lines).wrap(Wrap { trim: false }), body_area);

    if hint_area.height > 0 {
        frame.render_widget(
            Paragraph::new(Line::styled(
                "t Theme   s Rating   Esc / ? Close",
                Style::default().fg(palette.muted),
            ))
            .alignment(Alignment::Center),
            hint_area,
        );
    }
}

#[cfg(test)]
mod tests {
    use std::num::NonZeroU32;

    use image::DynamicImage;
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;
    use ratatui_image::picker::Picker;

    use super::*;
    use crate::domain::LibraryEntry;

    fn buffer_to_string(backend: &TestBackend) -> String {
        let buf = backend.buffer();
        let mut out = String::with_capacity(
            buf.area.width as usize * buf.area.height as usize + buf.area.height as usize,
        );
        for y in 0..buf.area.height {
            for x in 0..buf.area.width {
                if let Some(cell) = buf.cell((x, y)) {
                    out.push_str(cell.symbol());
                }
            }
            out.push('\n');
        }
        out
    }

    fn make_item(
        id: u32,
        title: &str,
        alt_title: Option<&str>,
        status: LibraryStatus,
        progress: u32,
        episodes: Option<u32>,
        score: Option<ScoreRaw>,
    ) -> AnimeItem {
        AnimeItem::new(
            MediaId(NonZeroU32::new(id).unwrap()),
            title.to_owned(),
            alt_title.map(str::to_owned),
            episodes,
            "FINISHED".to_owned(),
            Some(91),
            vec!["Adventure".to_owned(), "Drama".to_owned()],
            "After the party of heroes defeated the Demon King, a new journey begins.".to_owned(),
            Some("https://example.com/cover.jpg".to_owned()),
            Some(format!("https://anilist.co/anime/{id}")),
            LibraryEntry {
                id: NonZeroU32::new(id + 100).map(crate::domain::EntryId),
                status,
                progress,
                score,
            },
        )
    }

    #[test]
    fn auth_screen_renders_focused_fields_masked_token_and_error() {
        let backend = TestBackend::new(90, 26);
        let mut terminal = Terminal::new(backend).unwrap();
        let palette = ThemePalette::from_name("catppuccin-mocha");

        let view = AuthView {
            palette,
            username: Some("nkaet"),
            client_id: "12345",
            token: "secret-tok",
            focused: &AuthField::Token,
            submitting: false,
            error: Some("Invalid access token provided"),
            oauth_url: Some(
                "https://anilist.co/api/v2/oauth/authorize?client_id=12345&response_type=token",
            ),
        };

        terminal
            .draw(|f| {
                let area = f.area();
                fill_background(f, area, palette);
                render_auth_view(f, area, &view);
            })
            .unwrap();

        let rendered = buffer_to_string(terminal.backend());
        assert!(rendered.contains("AniList Library — Connect Account"));
        assert!(rendered.contains("@nkaet"));
        assert!(rendered.contains("12345"));
        assert!(rendered.contains("https://anilist.co/api/v2/oauth/authorize"));
        assert!(rendered.contains("Enter / Ctrl+O to open in browser"));
        assert!(rendered.contains("••••••••••"));
        assert!(!rendered.contains("secret-tok"));
        assert!(rendered.contains("Connect & Sync Library"));
        assert!(rendered.contains("Invalid access token provided"));
        assert!(rendered.contains("Tab/↑↓ Field"));
    }

    #[test]
    fn auth_screen_handles_transparent_background_and_submitting_state() {
        let backend = TestBackend::new(80, 24);
        let mut terminal = Terminal::new(backend).unwrap();
        let palette = ThemePalette::from_name("textual-ansi");
        assert_eq!(palette.background, Color::Reset);

        let view = AuthView {
            palette,
            username: None,
            client_id: "",
            token: "",
            focused: &AuthField::Submit,
            submitting: true,
            error: None,
            oauth_url: None,
        };

        terminal
            .draw(|f| {
                let area = f.area();
                fill_background(f, area, palette);
                render_auth_view(f, area, &view);
            })
            .unwrap();

        let rendered = buffer_to_string(terminal.backend());
        assert!(rendered.contains("Enter AniList Client ID (or set ANILIST_CLIENT_ID)"));
        assert!(rendered.contains("Enter Client ID to generate authorization link"));
        assert!(rendered.contains("Paste token or full callback URL"));
        assert!(rendered.contains("Verifying token..."));

        let corner_cell = terminal.backend().buffer().cell((0, 0)).unwrap();
        assert_eq!(corner_cell.bg, Color::Reset);
    }

    #[test]
    fn library_screen_renders_spotlight_above_table_and_collections_on_right_wide_140x45() {
        let backend = TestBackend::new(140, 45);
        let mut terminal = Terminal::new(backend).unwrap();
        let palette = ThemePalette::from_name("catppuccin-mocha");

        let item1 = make_item(
            154587,
            "Frieren: Beyond Journey's End",
            Some("Sousou no Frieren"),
            LibraryStatus::Watching,
            12,
            Some(28),
            ScoreRaw::new(98),
        );
        let item2 = make_item(
            21,
            "One Piece",
            None,
            LibraryStatus::Watching,
            1100,
            None,
            None,
        );
        let items = [&item1, &item2];

        let view = LibraryView {
            palette,
            username: Some("nkaet"),
            counts: [2, 5, 10, 1, 0, 0],
            overview: LibraryOverview {
                total_titles: 18,
                episodes_watched: 1112,
                rated_count: 9,
                mean_avg_score: Some(86),
            },
            active_status: LibraryStatus::Watching,
            focus: &FocusPane::Table,
            sidebar_index: 0,
            table_selected: 0,
            filter_query: "",
            filter_active: false,
            filter_scope: &FilterScope::CurrentList,
            sort_field: SortField::Updated,
            sort_direction: SortDirection::Desc,
            description_scroll: 0,
            syncing: false,
            sync_tick: 0,
            rate_limit: RateLimitInfo {
                limit: Some(90),
                remaining: Some(85),
                reset_at_unix: None,
            },
            score_format: ScoreFormat::Point10Decimal,
            toast: None,
            visible_items: &items,
            selected_item: Some(&item1),
        };

        let mut cover_rect = None;
        let mut table_offset = 0;
        terminal
            .draw(|f| {
                let area = f.area();
                fill_background(f, area, palette);
                cover_rect = render_library_base(f, area, &view, &mut table_offset);
            })
            .unwrap();

        let rendered = buffer_to_string(terminal.backend());
        assert!(rendered.contains("18 titles"));
        assert!(rendered.contains("@nkaet"));
        assert!(rendered.contains("Collections"));
        assert!(rendered.contains("OVERVIEW"));
        assert!(rendered.contains("1112"));
        assert!(rendered.contains("86%"));
        assert!(rendered.contains("Sign Out"));
        assert!(rendered.contains("Frieren: Beyond Journey's End"));
        assert!(rendered.contains("Sousou no Frieren"));
        assert!(rendered.contains("Ep 12 / 28"));
        assert!(rendered.contains("91% avg"));
        assert!(rendered.contains("Adventure · Drama"));
        assert!(rendered.contains("-/+ Ep"));
        assert!(rendered.contains("a Watching"));
        assert!(rendered.contains("r 9.8/10"));
        assert!(rendered.contains("9.8"));
        assert!(rendered.contains("v AniList"));
        assert!(rendered.contains("After the party of heroes"));
        assert!(cover_rect.is_some());

        // Verify Spotlight is above Watching table, and Collections is to the RIGHT of Spotlight.
        let spotlight_pos = rendered.find("Spotlight").expect("Spotlight title present");
        let collections_pos = rendered
            .find("Collections")
            .expect("Collections title present");
        let table_pos = rendered
            .find("Watching • 2")
            .expect("Watching table title present");
        assert!(
            spotlight_pos < collections_pos,
            "Collections sidebar must appear to the right of Spotlight on the same header row"
        );
        assert!(
            spotlight_pos < table_pos,
            "Spotlight details pane must appear above Watching list table"
        );
    }

    #[test]
    fn library_screen_renders_populated_narrow_60x20() {
        let backend = TestBackend::new(60, 20);
        let mut terminal = Terminal::new(backend).unwrap();
        let palette = ThemePalette::from_name("nord");

        let item1 = make_item(
            154587,
            "Frieren",
            None,
            LibraryStatus::Watching,
            12,
            Some(28),
            Some(Point3Score::Happy.as_score_raw()),
        );
        let items = [&item1];

        let view = LibraryView {
            palette,
            username: Some("nkaet"),
            counts: [1, 3, 7, 0, 0, 0],
            overview: LibraryOverview {
                total_titles: 11,
                episodes_watched: 120,
                rated_count: 5,
                mean_avg_score: Some(82),
            },
            active_status: LibraryStatus::Watching,
            focus: &FocusPane::Details,
            sidebar_index: 0,
            table_selected: 0,
            filter_query: "",
            filter_active: false,
            filter_scope: &FilterScope::AllLists,
            sort_field: SortField::Updated,
            sort_direction: SortDirection::Desc,
            description_scroll: 0,
            syncing: true,
            sync_tick: 3,
            rate_limit: RateLimitInfo {
                limit: Some(90),
                remaining: Some(8),
                reset_at_unix: None,
            },
            score_format: ScoreFormat::Point3,
            toast: None,
            visible_items: &items,
            selected_item: Some(&item1),
        };

        let mut cover_rect = Some((item1.id, Rect::default()));
        let mut table_offset = 0;
        terminal
            .draw(|f| {
                let area = f.area();
                fill_background(f, area, palette);
                cover_rect = render_library_base(f, area, &view, &mut table_offset);
            })
            .unwrap();

        let rendered = buffer_to_string(terminal.backend());
        assert!(rendered.contains("Syncing"));
        assert!(rendered.contains("All"));
        assert!(rendered.contains("API:8"));
        assert!(rendered.contains("Watching(1)"));
        assert!(rendered.contains("Frieren"));
        assert!(rendered.contains("Ep 12 / 28"));
        // Cover is hidden on <76 cols narrow layout so info area stays readable.
        assert!(cover_rect.is_none());
    }

    #[test]
    fn library_screen_renders_empty_states_with_and_without_filter() {
        let backend = TestBackend::new(100, 30);
        let mut terminal = Terminal::new(backend).unwrap();
        let palette = ThemePalette::from_name("tokyo-night");

        let empty_view = LibraryView {
            palette,
            username: None,
            counts: [0; 6],
            overview: LibraryOverview {
                total_titles: 0,
                episodes_watched: 0,
                rated_count: 0,
                mean_avg_score: None,
            },
            active_status: LibraryStatus::Planning,
            focus: &FocusPane::Table,
            sidebar_index: 1,
            table_selected: 0,
            filter_query: "",
            filter_active: false,
            filter_scope: &FilterScope::CurrentList,
            sort_field: SortField::Updated,
            sort_direction: SortDirection::Desc,
            description_scroll: 0,
            syncing: false,
            sync_tick: 0,
            rate_limit: RateLimitInfo::default(),
            score_format: ScoreFormat::Point10Decimal,
            toast: None,
            visible_items: &[],
            selected_item: None,
        };

        let mut table_offset = 0;
        terminal
            .draw(|f| {
                let area = f.area();
                fill_background(f, area, palette);
                render_library_base(f, area, &empty_view, &mut table_offset);
            })
            .unwrap();

        let rendered = buffer_to_string(terminal.backend());
        assert!(rendered.contains("No anime in this list"));
        assert!(rendered.contains("Select an anime below to inspect details"));

        let filtered_empty_view = LibraryView {
            filter_query: "evangelion",
            filter_active: true,
            ..empty_view
        };

        terminal
            .draw(|f| {
                let area = f.area();
                fill_background(f, area, palette);
                render_library_base(f, area, &filtered_empty_view, &mut table_offset);
            })
            .unwrap();

        let rendered_filtered = buffer_to_string(terminal.backend());
        assert!(rendered_filtered.contains("No matches for 'evangelion'"));
    }

    #[test]
    fn modals_status_rating_and_help_render_cleanly() {
        let backend = TestBackend::new(100, 32);
        let mut terminal = Terminal::new(backend).unwrap();
        let palette = ThemePalette::from_name("catppuccin-mocha");

        let empty_ctx = ModalContext {
            status: Some(LibraryStatus::Watching),
            score: None,
            score_format: ScoreFormat::Point3,
            score_pref: ScoreFormatPreference::Auto,
            api_score_format: None,
            theme_name: "anilist",
            rating_input: "",
            rating_input_error: None,
            sort_field: SortField::Updated,
            sort_direction: SortDirection::Desc,
        };

        terminal
            .draw(|f| {
                let area = f.area();
                fill_background(f, area, palette);
                render_modal(
                    f,
                    area,
                    &ActiveModal::StatusPicker { selected: 6 },
                    palette,
                    &empty_ctx,
                );
            })
            .unwrap();
        let status_buf = buffer_to_string(terminal.backend());
        assert!(status_buf.contains("Move to Collection"));
        assert!(status_buf.contains("Watching ✓"));
        assert!(status_buf.contains("Repeating"));
        assert!(status_buf.contains("Remove from Library"));

        let p3_ctx = ModalContext {
            status: None,
            score: Some(Point3Score::Happy.as_score_raw()),
            score_format: ScoreFormat::Point3,
            score_pref: ScoreFormatPreference::Override(ScoreFormat::Point3),
            api_score_format: Some(ScoreFormat::Point3),
            theme_name: "anilist",
            rating_input: "",
            rating_input_error: None,
            sort_field: SortField::Updated,
            sort_direction: SortDirection::Desc,
        };

        terminal
            .draw(|f| {
                let area = f.area();
                fill_background(f, area, palette);
                render_modal(
                    f,
                    area,
                    &ActiveModal::RatingPicker { selected: 0 },
                    palette,
                    &p3_ctx,
                );
            })
            .unwrap();
        let rating_buf = buffer_to_string(terminal.backend());
        assert!(rating_buf.contains("Rate Anime"));
        assert!(rating_buf.contains(":)  Liked ✓"));
        assert!(rating_buf.contains(":|  Neutral"));
        assert!(rating_buf.contains(":(  Disliked"));
        assert!(rating_buf.contains("Clear Rating"));

        // Numeric 9.8/10 rating modal & unified Help & Settings modal.
        let decimal_ctx = ModalContext {
            status: None,
            score: ScoreRaw::new(98),
            score_format: ScoreFormat::Point10Decimal,
            score_pref: ScoreFormatPreference::Auto,
            api_score_format: None,
            theme_name: "anilist",
            rating_input: "9.8",
            rating_input_error: None,
            sort_field: SortField::Score,
            sort_direction: SortDirection::Desc,
        };
        terminal
            .draw(|f| {
                let area = f.area();
                fill_background(f, area, palette);
                render_modal(
                    f,
                    area,
                    &ActiveModal::RatingPicker { selected: 0 },
                    palette,
                    &decimal_ctx,
                );
            })
            .unwrap();
        let decimal_buf = buffer_to_string(terminal.backend());
        assert!(decimal_buf.contains("Rate Anime (9.8/10)"));
        assert!(decimal_buf.contains("0.0 – 10.0"));
        assert!(decimal_buf.contains("9.8▏"));

        terminal
            .draw(|f| {
                let area = f.area();
                fill_background(f, area, palette);
                render_modal(f, area, &ActiveModal::Help, palette, &decimal_ctx);
            })
            .unwrap();
        let help_buf = buffer_to_string(terminal.backend());
        assert!(help_buf.contains("Help & Settings"));
        assert!(help_buf.contains("Theme: AniList"));
        assert!(help_buf.contains("Auto (AniList: default 9.8/10)"));
        assert!(help_buf.contains("Navigation"));
        assert!(help_buf.contains("Filter & Sync"));
        assert!(help_buf.contains("Library Actions"));
        assert!(help_buf.contains("t Theme   s Rating   Esc / ? Close"));

        terminal
            .draw(|f| {
                let area = f.area();
                fill_background(f, area, palette);
                render_modal(
                    f,
                    area,
                    &ActiveModal::SortPicker { selected: 2 },
                    palette,
                    &decimal_ctx,
                );
            })
            .unwrap();
        let sort_buf = buffer_to_string(terminal.backend());
        assert!(sort_buf.contains("Sort By"));
        assert!(sort_buf.contains("Recently Updated"));
        assert!(sort_buf.contains("My Rating  (↓) ✓"));
        assert!(sort_buf.contains("Average Score"));
        assert!(sort_buf.contains("Episode Progress"));
    }

    #[test]
    fn footer_toast_renders_all_levels() {
        let backend = TestBackend::new(80, 10);
        let mut terminal = Terminal::new(backend).unwrap();
        let palette = ThemePalette::from_name("gruvbox");

        for (level, msg) in [
            (ToastLevel::Info, "Loaded cached library"),
            (ToastLevel::Success, "Progress updated"),
            (ToastLevel::Warning, "Rate limit nearly reached"),
            (ToastLevel::Error, "Network timeout"),
        ] {
            let view = LibraryView {
                palette,
                username: None,
                counts: [0; 6],
                overview: LibraryOverview {
                    total_titles: 0,
                    episodes_watched: 0,
                    rated_count: 0,
                    mean_avg_score: None,
                },
                active_status: LibraryStatus::Watching,
                focus: &FocusPane::Table,
                sidebar_index: 0,
                table_selected: 0,
                filter_query: "",
                filter_active: false,
                filter_scope: &FilterScope::CurrentList,
                sort_field: SortField::Updated,
                sort_direction: SortDirection::Desc,
                description_scroll: 0,
                syncing: false,
                sync_tick: 0,
                rate_limit: RateLimitInfo::default(),
                score_format: ScoreFormat::Point10Decimal,
                toast: Some((msg, &level)),
                visible_items: &[],
                selected_item: None,
            };

            terminal
                .draw(|f| {
                    let area = f.area();
                    render_footer(f, Rect::new(0, 0, area.width, 1), &view);
                })
                .unwrap();

            let buf = buffer_to_string(terminal.backend());
            assert!(buf.contains(msg));
        }
    }

    #[test]
    fn cover_slot_tracks_loading_missing_sizing_and_ready_states() {
        let backend = TestBackend::new(40, 20);
        let mut terminal = Terminal::new(backend).unwrap();
        let palette = ThemePalette::from_name("catppuccin-mocha");
        let media_id = MediaId(NonZeroU32::new(42).unwrap());
        let cover_area = Rect::new(2, 2, 20, 12);

        let mut last_cover = Rect::default();
        let mut pending = None;

        // 1. Loading placeholder
        terminal
            .draw(|f| {
                render_cover_slot(
                    f,
                    CoverSlot {
                        cover_area,
                        media_id,
                        palette,
                        proto: None,
                        is_loading: true,
                        last_cover_area: &mut last_cover,
                        pending_reencode: &mut pending,
                    },
                );
            })
            .unwrap();
        assert_eq!(last_cover, cover_area);
        assert!(pending.is_none());
        assert!(buffer_to_string(terminal.backend()).contains("Loading..."));

        // 2. Missing placeholder
        terminal
            .draw(|f| {
                render_cover_slot(
                    f,
                    CoverSlot {
                        cover_area,
                        media_id,
                        palette,
                        proto: None,
                        is_loading: false,
                        last_cover_area: &mut last_cover,
                        pending_reencode: &mut pending,
                    },
                );
            })
            .unwrap();
        assert!(buffer_to_string(terminal.backend()).contains("No Cover"));

        // 3. Unresized StatefulProtocol triggers pending_reencode + Sizing placeholder
        let picker = Picker::from_fontsize((8, 16));
        let mut proto = picker.new_resize_protocol(DynamicImage::new_rgba8(64, 96));
        terminal
            .draw(|f| {
                render_cover_slot(
                    f,
                    CoverSlot {
                        cover_area,
                        media_id,
                        palette,
                        proto: Some(&mut proto),
                        is_loading: false,
                        last_cover_area: &mut last_cover,
                        pending_reencode: &mut pending,
                    },
                );
            })
            .unwrap();
        assert_eq!(pending, Some((media_id, cover_area)));
        assert!(buffer_to_string(terminal.backend()).contains("Sizing..."));

        // 4. After background resize_encode, renders StatefulImage without pending_reencode
        let resize = Resize::Fit(None);
        if let Some(target) = proto.needs_resize(&resize, cover_area) {
            proto.resize_encode(&resize, proto.background_color(), target);
        }
        pending = None;
        terminal
            .draw(|f| {
                render_cover_slot(
                    f,
                    CoverSlot {
                        cover_area,
                        media_id,
                        palette,
                        proto: Some(&mut proto),
                        is_loading: false,
                        last_cover_area: &mut last_cover,
                        pending_reencode: &mut pending,
                    },
                );
            })
            .unwrap();
        assert!(pending.is_none());
    }

    #[test]
    fn extreme_small_terminals_render_without_panic() {
        let palette = ThemePalette::from_name("catppuccin-mocha");
        let item = make_item(
            1,
            "Short",
            None,
            LibraryStatus::Watching,
            1,
            Some(12),
            None,
        );
        let items = [&item];

        for (w, h) in [(0, 0), (1, 1), (8, 3), (20, 8)] {
            let backend = TestBackend::new(w, h);
            let mut terminal = Terminal::new(backend).unwrap();

            let auth_view = AuthView {
                palette,
                username: None,
                client_id: "123",
                token: "abc",
                focused: &AuthField::ClientId,
                submitting: false,
                error: Some("err"),
                oauth_url: Some("https://example.com"),
            };
            terminal
                .draw(|f| {
                    let area = f.area();
                    fill_background(f, area, palette);
                    render_auth_view(f, area, &auth_view);
                })
                .unwrap();

            let lib_view = LibraryView {
                palette,
                username: None,
                counts: [1, 0, 0, 0, 0, 0],
                overview: LibraryOverview {
                    total_titles: 1,
                    episodes_watched: 1,
                    rated_count: 0,
                    mean_avg_score: None,
                },
                active_status: LibraryStatus::Watching,
                focus: &FocusPane::Sidebar,
                sidebar_index: 0,
                table_selected: 0,
                filter_query: "",
                filter_active: false,
                filter_scope: &FilterScope::CurrentList,
                sort_field: SortField::Updated,
                sort_direction: SortDirection::Desc,
                description_scroll: 0,
                syncing: true,
                sync_tick: 1,
                rate_limit: RateLimitInfo::default(),
                score_format: ScoreFormat::Point10Decimal,
                toast: None,
                visible_items: &items,
                selected_item: Some(&item),
            };
            let modal_ctx = ModalContext {
                status: None,
                score: None,
                score_format: ScoreFormat::Point10Decimal,
                score_pref: ScoreFormatPreference::Auto,
                api_score_format: None,
                theme_name: "anilist",
                rating_input: "",
                rating_input_error: None,
                sort_field: SortField::Updated,
                sort_direction: SortDirection::Desc,
            };
            let mut table_offset = 0;
            terminal
                .draw(|f| {
                    let area = f.area();
                    fill_background(f, area, palette);
                    render_library_base(f, area, &lib_view, &mut table_offset);
                    render_modal(f, area, &ActiveModal::Help, palette, &modal_ctx);
                })
                .unwrap();
        }
    }

    #[test]
    fn render_entrypoint_renders_auth_and_library_app_state() {
        use std::collections::HashMap;
        use crate::config::AuthSession;
        use crate::domain::LibraryCollection;

        let palette = ThemePalette::from_name("catppuccin-mocha");

        // Auth screen via public render entrypoint
        let mut auth_state = AppState::new(palette, AuthSession::default());
        if let Screen::Auth(ref mut auth) = auth_state.screen {
            auth.client_id = "999".to_owned();
            auth.token = "tok".to_owned();
            auth.focused = AuthField::Token;
            auth.error = Some("Token expired".to_owned());
        }
        let mut auth_term = Terminal::new(TestBackend::new(90, 26)).unwrap();
        auth_term
            .draw(|f| super::render(f, &mut auth_state))
            .unwrap();
        let auth_buf = buffer_to_string(auth_term.backend());
        assert!(auth_buf.contains("AniList Library — Connect Account"));
        assert!(auth_buf.contains("•••"));
        assert!(auth_buf.contains("Token expired"));

        // Populated Library screen via public render entrypoint
        let session = AuthSession {
            access_token: Some("tok".to_owned()),
            client_id: Some("999".to_owned()),
            username: Some("nkaet".to_owned()),
        };
        let mut lib_state = AppState::new(palette, session);
        let mut lists = HashMap::new();
        lists.insert(
            LibraryStatus::Watching,
            vec![make_item(
                154587,
                "Frieren: Beyond Journey's End",
                Some("Sousou no Frieren"),
                LibraryStatus::Watching,
                12,
                Some(28),
                ScoreRaw::new(98),
            )],
        );
        lib_state.library = LibraryCollection::from_lists(lists);
        lib_state.rebuild_visible_rows();

        let mut lib_term = Terminal::new(TestBackend::new(140, 45)).unwrap();
        lib_term
            .draw(|f| super::render(f, &mut lib_state))
            .unwrap();
        let lib_buf = buffer_to_string(lib_term.backend());
        assert!(lib_buf.contains("Frieren: Beyond Journey's End"));
        assert!(lib_buf.contains("Watching • 1"));
        assert!(lib_buf.contains("Loading...") || lib_buf.contains("No Cover"));
        assert!(lib_state.last_cover_area.width > 0);
        assert!(lib_state.last_cover_area.height > 0);
    }

    #[test]
    fn transparent_theme_selection_stays_opaque_when_unfocused() {
        let palette = ThemePalette::from_name("terminal");
        let style = unfocused_highlight_style(palette);
        assert_eq!(style.bg, Some(palette.accent));
        assert_eq!(style.fg, Some(contrast_fg_on(palette.accent, palette)));
    }

    #[test]
    fn table_scroll_offset_persists_when_moving_up_from_bottom() {
        let backend = TestBackend::new(100, 22);
        let mut terminal = Terminal::new(backend).unwrap();
        let palette = ThemePalette::from_name("terminal");

        let mut items = Vec::new();
        for idx in 0..20 {
            items.push(make_item(
                1000 + idx,
                &format!("Anime Title {idx:02}"),
                None,
                LibraryStatus::Watching,
                1,
                Some(12),
                None,
            ));
        }
        let mut lists = std::collections::HashMap::new();
        lists.insert(LibraryStatus::Watching, items);
        let library = crate::domain::LibraryCollection::from_lists(lists);

        let auth = crate::config::AuthSession {
            client_id: Some("123".to_owned()),
            access_token: Some("tok".to_owned()),
            username: Some("nkaet".to_owned()),
        };
        let mut state = AppState::new(palette, auth);
        state.library = library;
        state.rebuild_visible_rows();

        // Scroll down past the bottom of the viewport.
        state.table_selected = 14;
        terminal.draw(|f| super::render(f, &mut state)).unwrap();
        let offset_at_bottom = state.table_offset;
        assert!(offset_at_bottom > 0, "table should have scrolled down");

        // Pressing Up once should move the highlight up inside the viewport,
        // keeping the scroll offset steady rather than pinning the cursor to the bottom.
        state.table_selected = 13;
        terminal.draw(|f| super::render(f, &mut state)).unwrap();
        assert_eq!(state.table_offset, offset_at_bottom);
    }
}
