use crate::{
    config,
    state::{
        format_rating, BrowsePageUIState, Context, ContextPageUIState, DataReadGuard, Id,
        LibraryFocusState, MutableWindowState, PageState, PageType, PlaybackMetadata,
        PlaylistCreateCurrentField, PlaylistFolderItem, PlaylistPopupAction, PopupActionEntry,
        PopupState, SearchFocusState, SharedState, Track, TrackJournalEntry, UIStateGuard,
        UiOperationState, UiViewStatus, WorkspaceHit, YouTubePlayback,
    },
};
use anyhow::{Context as AnyhowContext, Result};
use ratatui::{
    layout::{Alignment, Constraint, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span, Text},
    widgets::{
        Block, BorderType, Borders, Cell, List, ListItem, ListState, Paragraph, Row, Table,
        TableState, Wrap,
    },
    Frame,
};

#[cfg(feature = "image")]
use crate::state::ImageRenderInfo;

type Terminal = ratatui::Terminal<ratatui::backend::CrosstermBackend<std::io::Stdout>>;

#[cfg(test)]
pub(crate) fn initialize_test_config() -> &'static config::Configs {
    static INIT: std::sync::Once = std::sync::Once::new();
    INIT.call_once(|| {
        let config_root = std::env::temp_dir().join(format!(
            "unified-player-render-tests-{}",
            std::process::id()
        ));
        std::fs::create_dir_all(&config_root).unwrap();
        crate::config::set_config(crate::config::Configs::new(&config_root, &config_root).unwrap());
    });
    config::get_config()
}

pub(crate) mod components;
#[cfg(feature = "image")]
pub mod cover_image;
mod demo;
mod frame_schedule;
mod layout;
mod page;
mod playback;
mod popup;
mod preview;
pub mod single_line_input;
#[cfg(feature = "streaming")]
pub mod streaming;
mod terminal_title;
pub mod utils;
mod welcome;

use components::footer::{FooterHint, FooterLine};
pub(crate) use components::shell::{
    LayoutMode, LayoutPolicy, WorkspaceFrame, WorkspaceLayout, WorkspaceLayoutKind,
};
pub(crate) use demo::{
    render_welcome_demo_layout, run_welcome_demo_interactive, welcome_demo_layout_from_cli,
    welcome_demo_step_from_cli, WelcomeDemoScenario,
};
pub(crate) use preview::{
    preview_size_from_cli, render_screen_preview, run_screen_preview_interactive, PreviewScenario,
    PreviewScreen,
};

/// Run the application UI
pub fn run(state: &SharedState, mut terminal: Terminal) -> Result<()> {
    let mut last_terminal_size = None;
    let mut terminal_title = terminal_title::TerminalTitle::default();
    let mut rendered_frames = 0_u64;

    loop {
        let frame_interval = {
            // Locking for render keeps this loop's own writes from waking it.
            let mut ui = state.ui.lock_untracked();
            let now = std::time::Instant::now();
            ui.expire_operation_status(now);
            if let Some(expires_at) = ui.operation_status.as_ref().and_then(|s| s.expires_at) {
                frame_schedule::request_frame_within(expires_at.saturating_duration_since(now));
            }
            if !ui.is_running {
                if !state.playback_shutdown_complete() {
                    drop(ui);
                    std::thread::sleep(std::time::Duration::from_millis(10));
                    continue;
                }
                if terminal_title.restore(terminal.backend_mut()).is_err() {
                    tracing::warn!("Failed to restore the terminal title");
                }
                clean_up(terminal).context("clean up UI resources")?;
                state.mark_shutdown_phase(crate::runtime::ShutdownPhase::TerminalRestored)?;
                return Ok(());
            }

            let terminal_size = terminal.size()?;
            if Some(terminal_size) != last_terminal_size {
                last_terminal_size = Some(terminal_size);
                ui.clear_playback_hit_regions();
                #[cfg(feature = "image")]
                {
                    // redraw the cover image when the terminal's size changes
                    ui.last_cover_image_render_info = ImageRenderInfo::default();
                }
            }

            let render_started = std::time::Instant::now();
            if let Err(err) = terminal.draw(|frame| {
                // set the background and foreground colors for the application
                let rect = frame.area();
                let block = Block::default().style(ui.theme.workspace_base());
                frame.render_widget(block, rect);

                render_application(frame, state, &mut ui, rect);
            }) {
                crate::observability::log_safe_error!(
                    error,
                    crate::observability::DiagnosticCode::UI_RENDER_FAILED,
                    crate::observability::ErrorCategory::Resource,
                    &err,
                    "Failed to render the application"
                );
            }
            if terminal_title
                .update(terminal.backend_mut(), state, &ui)
                .is_err()
            {
                tracing::debug!("Failed to set the terminal title");
            }
            ui.set_manual_scroll_extent(utils::take_manual_scroll_extent());
            if frame_schedule::take_marquee_scrolled() {
                frame_schedule::request_frame_within(
                    ui.until_next_marquee_phase(std::time::Instant::now()),
                );
            }
            let elapsed = render_started.elapsed();
            rendered_frames = rendered_frames.saturating_add(1);
            let slow = elapsed > std::time::Duration::from_millis(50);
            if slow || rendered_frames.is_multiple_of(100) {
                crate::observability::ui_render_sample(elapsed, ui.diagnostic_revision, slow);
            }
            state
                .diagnostics
                .update_ui_snapshot(&ui.diagnostic_snapshot(state.shutdown_requested()));
            ui.frame_interval
        };

        // The frame interval caps the frame rate; beyond that, sleep until
        // shared state changes or time-driven content is due.
        let next_frame = frame_schedule::take_next_frame(std::time::Instant::now());
        std::thread::sleep(frame_interval);
        state.redraw.wait_until(next_frame);
    }
}

pub fn init_terminal() -> Result<Terminal> {
    let mut stdout = std::io::stdout();
    crossterm::terminal::enable_raw_mode()?;
    crossterm::execute!(
        stdout,
        crossterm::terminal::EnterAlternateScreen,
        crossterm::event::EnableMouseCapture
    )?;
    let backend = ratatui::backend::CrosstermBackend::new(stdout);
    let mut terminal = ratatui::Terminal::new(backend)?;
    terminal.clear()?;
    Ok(terminal)
}

pub(super) fn init_interactive_demo_terminal() -> Result<Terminal> {
    use std::io::IsTerminal;
    anyhow::ensure!(
        std::io::stdin().is_terminal() && std::io::stdout().is_terminal(),
        "interactive Welcome demo requires a terminal on stdin and stdout"
    );
    match init_terminal() {
        Ok(terminal) => Ok(terminal),
        Err(error) => {
            let _ = crossterm::terminal::disable_raw_mode();
            let _ = crossterm::execute!(
                std::io::stdout(),
                crossterm::terminal::LeaveAlternateScreen,
                crossterm::event::DisableMouseCapture
            );
            Err(error).context("initialize interactive Welcome demo terminal")
        }
    }
}

#[cfg(feature = "image")]
/// Query the terminal after `init_terminal` has entered the alternate screen and raw mode.
/// `ratatui-image` expects to exchange its capability query before the event reader starts.
pub fn init_image_picker(state: &SharedState) -> Result<()> {
    let mut ui = state.ui.lock();
    ui.picker = match ratatui_image::picker::Picker::from_query_stdio() {
        Ok(p) => p,
        Err(err) => {
            crate::observability::log_safe_error!(
                warn,
                crate::observability::DiagnosticCode::UI_PICKER_INIT_FAILED,
                crate::observability::ErrorCategory::Resource,
                &err,
                "Failed to initialize the terminal image picker; using half blocks"
            );
            ratatui_image::picker::Picker::halfblocks()
        }
    };

    // ratatui_image might detect the wrong protocol for iTerm2, so override it to the native iTerm2 protocol if detected
    // https://github.com/ratatui/ratatui-image/issues/158
    if is_iterm2() && ui.picker.protocol_type() != ratatui_image::picker::ProtocolType::Iterm2 {
        ui.picker
            .set_protocol_type(ratatui_image::picker::ProtocolType::Iterm2);
        tracing::info!("Detected iTerm2; overriding image protocol to native iTerm2");
    }
    tracing::info!("Image protocol: {:?}", ui.picker.protocol_type());
    Ok(())
}

/// Whether the application is running inside iTerm2.
///
/// iTerm2 sets `TERM_PROGRAM=iTerm.app` locally and forwards `LC_TERMINAL=iTerm2`
/// over SSH, so checking both covers the common cases.
#[cfg(feature = "image")]
fn is_iterm2() -> bool {
    std::env::var("TERM_PROGRAM").is_ok_and(|v| v == "iTerm.app")
        || std::env::var("LC_TERMINAL").is_ok_and(|v| v.eq_ignore_ascii_case("iTerm2"))
}

/// Clean up UI resources before quitting the application
pub(crate) fn clean_up(mut terminal: Terminal) -> Result<()> {
    crossterm::terminal::disable_raw_mode()?;
    crossterm::execute!(
        terminal.backend_mut(),
        crossterm::terminal::LeaveAlternateScreen,
        crossterm::event::DisableMouseCapture,
    )?;
    terminal.show_cursor()?;
    Ok(())
}

/// Render the application
fn render_application(frame: &mut Frame, state: &SharedState, ui: &mut UIStateGuard, rect: Rect) {
    ui.refresh_focused_marquee();
    ui.project_frame_chrome();
    ui.clear_playback_hit_regions();
    render_workspace_application(frame, state, ui, rect);
}

fn render_workspace_application(
    frame: &mut Frame,
    state: &SharedState,
    ui: &mut UIStateGuard,
    rect: Rect,
) {
    let policy = LayoutPolicy::from_size(rect.width, rect.height);
    let chrome: WorkspaceFrame = policy.workspace_frame(rect);
    frame.render_widget(Block::default().style(ui.theme.workspace_base()), rect);
    render_workspace_header(frame, ui, chrome.header);
    render_workspace_separator(frame, ui, chrome.body_transport_separator);
    render_workspace_separator(frame, ui, chrome.transport_footer_separator);

    playback::render_workspace_playback_window(frame, state, ui, chrome.transport);

    let body = chrome.body;
    if body.is_empty() {
        return;
    }
    #[cfg(feature = "streaming")]
    let body = render_workspace_visualization(frame, state, ui, body);
    ui.workspace_popup_hits.clear();
    ui.popup_rect = Rect::default();
    let workspace_overlay_popup = matches!(
        ui.popup,
        Some(
            crate::state::PopupState::WorkspaceScope { .. }
                | crate::state::PopupState::AnchoredActionList { .. }
                | crate::state::PopupState::Volume { .. }
                | crate::state::PopupState::ActionList(..)
                | crate::state::PopupState::UserPlaylistList(..)
        )
    );
    let is_active = workspace_overlay_popup
        || matches!(
            ui.popup,
            None | Some(
                crate::state::PopupState::Search { .. }
                    | crate::state::PopupState::ConfirmAction { .. }
            )
        );
    render_page_content(is_active, frame, state, ui, body);
    let backdrop = ui.theme.workspace_base().bg;
    // Whatever floats over the page; pointer hover may only highlight inside it.
    let mut floating = None;
    if workspace_overlay_popup {
        // Every popup fades the page behind it, anchored menus included.
        dim_area(frame, body, backdrop);
        match ui.popup {
            Some(crate::state::PopupState::WorkspaceScope { .. }) => {
                popup::render_workspace_scope_popup(frame, ui, body);
            }
            Some(crate::state::PopupState::AnchoredActionList { .. }) => {
                popup::render_workspace_anchored_action_popup(frame, ui, body);
            }
            Some(crate::state::PopupState::Volume { .. }) => {
                popup::render_volume_popup(frame, state, ui, body);
            }
            Some(crate::state::PopupState::ActionList(..)) => {
                popup::render_workspace_action_popup(frame, ui, body);
            }
            Some(crate::state::PopupState::UserPlaylistList(..)) => {
                popup::render_workspace_user_playlist_popup(frame, state, ui);
            }
            _ => {}
        }
        floating = Some(ui.popup_rect);
    }
    if !workspace_overlay_popup {
        let content = ui.workspace_layout.content.intersection(body);
        let content = if content.is_empty() { body } else { content };
        let width = workspace_popup_width(content.width);
        let popup_area = Rect::new(
            content
                .x
                .saturating_add(content.width.saturating_sub(width) / 2),
            content.y,
            width,
            content.height.saturating_sub(2).max(1).min(content.height),
        );
        if ui.popup.is_some() {
            render_workspace_modal_popup(frame, state, ui, body, content, popup_area);
            floating = Some(ui.popup_rect);
        } else {
            // The key-sequence hint draws only while a prefix is pending; the
            // first pass tells whether it did, so the page can be faded first.
            let page = frame.buffer_mut().clone();
            let remaining = popup::render_shortcut_help_popup(frame, ui, popup_area);
            if remaining != popup_area {
                *frame.buffer_mut() = page;
                dim_area(frame, body, backdrop);
                popup::render_shortcut_help_popup(frame, ui, popup_area);
                floating = Some(Rect::new(
                    popup_area.x,
                    remaining.bottom(),
                    popup_area.width,
                    popup_area.bottom().saturating_sub(remaining.bottom()),
                ));
            }
        }
    }
    render_workspace_footer(frame, ui, chrome.footer);
    ui.refresh_workspace_hover();
    // Nothing under a popup or the key-sequence hint reacts to the pointer.
    if floating.is_some() {
        ui.suppress_workspace_hover();
    }
    render_pointer_hover(frame, ui);
}

/// Popups grow with the content panel, between a readable minimum and a
/// width beyond which rows become hard to follow.
fn workspace_popup_width(content_width: u16) -> u16 {
    (content_width.saturating_mul(3) / 5)
        .clamp(80, 120)
        .min(content_width)
}

/// Draw a page popup over the content. Popups lay themselves out at the bottom
/// of the area they are given, so the first pass measures the popup and the
/// second draws it vertically centred over a dimmed page. The inline search
/// box keeps its place under the results it filters, without dimming them.
fn render_workspace_modal_popup(
    frame: &mut Frame,
    state: &SharedState,
    ui: &mut UIStateGuard,
    body: Rect,
    content: Rect,
    area: Rect,
) {
    if matches!(ui.popup, Some(crate::state::PopupState::Search { .. })) {
        // Full width, so it covers whole table rows instead of their middles.
        let row = Rect::new(content.x, area.y, content.width, area.height);
        popup::render_popup(frame, state, ui, row);
        return;
    }
    let page = frame.buffer_mut().clone();
    popup::render_popup(frame, state, ui, area);
    let measured = ui.popup_rect;
    *frame.buffer_mut() = page;
    ui.workspace_popup_hits.clear();
    if measured.is_empty() {
        popup::render_popup(frame, state, ui, area);
        return;
    }

    dim_area(frame, body, ui.theme.workspace_base().bg);
    let height = measured.height.min(area.height);
    let top = content
        .y
        .saturating_add(content.height.saturating_sub(height) / 2)
        .max(area.y);
    let centred = Rect::new(
        area.x,
        area.y,
        area.width,
        top.saturating_add(height).saturating_sub(area.y).max(1),
    );
    popup::render_popup(frame, state, ui, centred);
}

/// Fade everything already drawn in `area` toward the theme background, so a
/// modal reads as the focus. Colours that cannot be blended are dimmed.
fn dim_area(frame: &mut Frame, area: Rect, base: Option<ratatui::style::Color>) {
    use ratatui::style::{Color, Modifier};
    let fade = |color: Color| match (color, base) {
        (Color::Rgb(r, g, b), Some(Color::Rgb(br, bg, bb))) => {
            let mix = |c: u8, t: u8| ((u16::from(c) * 2 + u16::from(t) * 3) / 5) as u8;
            Some(Color::Rgb(mix(r, br), mix(g, bg), mix(b, bb)))
        }
        _ => None,
    };
    let buffer = frame.buffer_mut();
    let area = area.intersection(buffer.area);
    for y in area.top()..area.bottom() {
        for x in area.left()..area.right() {
            let cell = &mut buffer[(x, y)];
            match fade(cell.fg) {
                Some(fg) => cell.fg = fg,
                None => cell.modifier.insert(Modifier::DIM),
            }
            if let Some(bg) = fade(cell.bg) {
                cell.bg = bg;
            }
        }
    }
}

/// Rows the page keeps above the visualization strip; shorter bodies drop it.
#[cfg(feature = "streaming")]
const VISUALIZATION_MIN_PAGE_HEIGHT: u16 = 14;
/// The strip takes about a sixth of the body, within these bounds.
#[cfg(feature = "streaming")]
const VISUALIZATION_MIN_ROWS: u16 = 3;

/// Draw the audio visualization as a strip along the bottom of the body, just
/// above the transport, and return the body area left for the page.
#[cfg(feature = "streaming")]
fn render_workspace_visualization(
    frame: &mut Frame,
    state: &SharedState,
    ui: &UIStateGuard,
    body: Rect,
) -> Rect {
    let rows = (body.height / 6).clamp(VISUALIZATION_MIN_ROWS, streaming::VIS_HEIGHT);
    let strip = rows.saturating_add(1);
    if !config::get_config().app_config.enable_audio_visualization
        || !state.is_local_streaming_active()
        // Forms and setup have nothing to accompany; keep their room.
        || matches!(
            ui.current_page(),
            PageState::Settings { .. } | PageState::Welcome { .. }
        )
        || body.height < strip.saturating_add(VISUALIZATION_MIN_PAGE_HEIGHT)
    {
        return body;
    }
    let page = Rect::new(body.x, body.y, body.width, body.height - strip);
    render_workspace_separator(frame, ui, Rect::new(body.x, page.bottom(), body.width, 1));
    let quiet = ui
        .theme
        .workspace_progress_remaining()
        .fg
        .unwrap_or(ratatui::style::Color::DarkGray);
    let loud = ui
        .theme
        .playback_progress_bar()
        .fg
        .unwrap_or(ratatui::style::Color::Cyan);
    // Inset like the transport surface so the bars line up with it.
    streaming::render_audio_visualization(
        frame,
        state,
        Rect::new(
            body.x.saturating_add(2),
            page.bottom().saturating_add(1),
            body.width.saturating_sub(4),
            rows,
        ),
        quiet,
        loud,
    );
    page
}

fn render_pointer_hover(frame: &mut Frame, ui: &UIStateGuard) {
    let Some(rect) = ui.workspace_hover_rect() else {
        return;
    };
    frame.buffer_mut().set_style(rect, ui.theme.hover());
}

fn render_page_content(
    is_active: bool,
    frame: &mut Frame,
    state: &SharedState,
    ui: &mut UIStateGuard,
    rect: Rect,
) {
    let page_type = ui.current_page().page_type();
    match page_type {
        PageType::Home => page::render_home_page(is_active, frame, state, ui, rect),
        PageType::HomeShelfList => page::render_home_shelf_list(is_active, frame, state, ui, rect),
        PageType::Welcome => page::render_welcome_page(is_active, frame, ui, rect),
        PageType::Library => page::render_library_page(is_active, frame, state, ui, rect),
        PageType::Search => page::render_search_page(is_active, frame, state, ui, rect),
        PageType::Context => {
            page::render_workspace_context_page(is_active, frame, state, ui, rect);
        }
        PageType::YouTubeContext => {
            page::render_workspace_youtube_context_page(is_active, frame, state, ui, rect);
        }
        PageType::UnifiedPlaylist => {
            page::render_unified_playlist_page(is_active, frame, state, ui, rect);
        }
        PageType::Browse => page::render_browse_page(is_active, frame, state, ui, rect),
        PageType::Lyrics => page::render_lyrics_page(is_active, frame, state, ui, rect),
        PageType::Journal => page::render_journal_page(is_active, frame, state, ui, rect),
        PageType::JournalLists => {
            page::render_journal_lists_page(is_active, frame, state, ui, rect);
        }
        PageType::JournalList => page::render_journal_list_page(is_active, frame, state, ui, rect),
        PageType::SessionHistory => {
            page::render_session_history_page(is_active, frame, state, ui, rect);
        }
        PageType::Queue => page::render_queue_page(is_active, frame, state, ui, rect),
        PageType::Settings => page::render_settings_page(is_active, frame, ui, rect),
        PageType::CommandHelp => page::render_commands_help_page(frame, ui, rect),
        PageType::Logs => page::render_logs_page(frame, state, ui, rect),
    }
}

fn render_workspace_separator(frame: &mut Frame, ui: &UIStateGuard, rect: Rect) {
    utils::render_horizontal_rule(frame, rect, "─", ui.theme.workspace_border());
}

/// The header scope is dropped rather than shown narrower than this.
const HEADER_MIN_SCOPE_WIDTH: u16 = 12;

fn render_workspace_header(frame: &mut Frame, ui: &mut UIStateGuard, rect: Rect) {
    if rect.is_empty() {
        return;
    }
    frame.render_widget(Block::default().style(ui.theme.workspace_base()), rect);
    if rect.height >= 3 {
        render_workspace_separator(
            frame,
            ui,
            Rect::new(rect.x, rect.bottom().saturating_sub(1), rect.width, 1),
        );
    }
    let welcome = matches!(ui.current_page(), PageState::Welcome { .. });
    let scope = if welcome {
        "First-run setup".to_owned()
    } else if matches!(ui.current_page(), PageState::Search { .. }) {
        "Search · All categories".to_owned()
    } else if matches!(ui.current_page(), PageState::Settings { .. }) {
        format!("Settings · {}", ui.workspace_settings_category.label())
    } else {
        let account = match ui.active_provider {
            config::ActiveProvider::Spotify => ui.spotify_account_label.as_deref(),
            config::ActiveProvider::YouTubeMusic => ui.youtube_account_label.as_deref(),
        }
        .unwrap_or("account unavailable");
        format!("Browsing {} / {account}", ui.active_provider.title())
    };
    let title_x = rect.x.saturating_add(2);
    let row_y = rect
        .y
        .saturating_add(1)
        .min(rect.bottom().saturating_sub(1));
    // Setup is finished or skipped from its Review step; a close control
    // would leave it through the global Back command instead.
    let back_key = (!welcome && rect.width >= 60)
        .then(|| workspace_footer_key(crate::command::Command::PreviousPage))
        .flatten();
    let close_label = if welcome {
        String::new()
    } else if let Some(key) = &back_key {
        format!("{key} Back")
    } else {
        "Back".to_owned()
    };
    let close_width = close_label.chars().count() as u16;
    let close_x = rect.right().saturating_sub(2).saturating_sub(close_width);
    if !welcome {
        let close_rect = Rect::new(close_x, row_y, close_width, 1);
        ui.workspace_hits
            .push((close_rect, WorkspaceHit::CloseWindow));
        frame.render_widget(
            Paragraph::new(if let Some(key) = back_key {
                Line::from(vec![
                    Span::styled(key, ui.theme.workspace_hint_key()),
                    Span::styled(" Back", ui.theme.workspace_hint_text()),
                ])
            } else {
                Line::styled(close_label, ui.theme.workspace_hint_text())
            })
            .alignment(Alignment::Right),
            close_rect,
        );
    }
    // The app name outranks the scope: the scope takes only what is left
    // after the name and both gaps, is drawn only on two-row headers, and is
    // dropped rather than cut to a stub.
    let title_len = "Unified Player".chars().count() as u16;
    let scope_room = if rect.height >= 2 && rect.width >= 12 {
        close_x
            .saturating_sub(title_x)
            .saturating_sub(title_len)
            .saturating_sub(4)
    } else {
        0
    };
    let scope_width = if scope_room >= HEADER_MIN_SCOPE_WIDTH {
        (scope.chars().count() as u16).min(scope_room)
    } else {
        0
    };
    let scope_x = close_x.saturating_sub(2).saturating_sub(scope_width);
    let title_width = title_len.min(close_x.saturating_sub(title_x).saturating_sub(2));
    if title_width > 0 {
        frame.render_widget(
            Paragraph::new("Unified Player").style(ui.theme.workspace_heading()),
            Rect::new(title_x, row_y, title_width, 1),
        );
    }
    if scope_width == 0 {
        return;
    }
    let scope_rect = Rect::new(scope_x, row_y, scope_width, 1);
    frame.render_widget(
        Paragraph::new(utils::bounded_text(&scope, scope_rect.width as usize))
            .style(ui.theme.workspace_secondary_text())
            .alignment(Alignment::Right),
        scope_rect,
    );
}

fn workspace_footer_key(command: crate::command::Command) -> Option<String> {
    config::get_config()
        .keymap_config
        .key_sequence_for_command(command)
        .map(ToString::to_string)
}

fn workspace_footer_line(ui: &UIStateGuard, available: usize) -> FooterLine {
    use crate::command::Command;
    use crate::state::WorkspaceFocusState;

    let bound = |command, fallback: &str, label| {
        FooterHint::new(
            workspace_footer_key(command).unwrap_or_else(|| fallback.to_owned()),
            label,
        )
    };
    let optional =
        |command, label| workspace_footer_key(command).map(|key| FooterHint::optional(key, label));
    let help =
        workspace_footer_key(Command::OpenCommandHelp).map(|key| FooterHint::new(key, "Commands"));

    if matches!(ui.current_page(), PageState::Welcome { .. }) {
        let choose = match ui.welcome_focus() {
            WorkspaceFocusState::Navigation => "Open step",
            WorkspaceFocusState::Context
            | WorkspaceFocusState::Queue
            | WorkspaceFocusState::Actions => "Choose",
        };
        let hints = vec![
            bound(Command::ChooseSelected, "enter", choose),
            bound(Command::FocusNextWindow, "tab", "Next pane"),
            bound(Command::PreviousPage, "backspace", "Previous step"),
        ];
        return FooterLine::layout(hints, help, available);
    }
    if matches!(ui.current_page(), PageState::Settings { .. }) {
        let action = match ui.workspace_focus {
            WorkspaceFocusState::Navigation => "Select category",
            WorkspaceFocusState::Context => "Edit",
            WorkspaceFocusState::Actions | WorkspaceFocusState::Queue => "Choose action",
        };
        let hints = vec![
            bound(Command::ChooseSelected, "enter", action),
            bound(Command::FocusNextWindow, "tab", "Next pane"),
            bound(Command::ClosePopup, "esc", "Back"),
        ];
        return FooterLine::layout(hints, help, available);
    }
    if let PageState::Search { state, .. } = ui.current_page() {
        if ui.workspace_focus != WorkspaceFocusState::Navigation {
            let choose = |label| bound(Command::ChooseSelected, "enter", label);
            let pane = |label| bound(Command::FocusNextWindow, "tab", label);
            // The Search page handles Escape directly rather than through the keymap.
            let esc = |label| FooterHint::new("esc", label);
            let hints = match state.focus {
                SearchFocusState::Category => {
                    vec![choose("Select category"), pane("Query"), esc("Back")]
                }
                SearchFocusState::Input => {
                    vec![choose("Search"), pane("Results"), esc("Leave input")]
                }
                focus => {
                    let action = match focus {
                        SearchFocusState::Tracks
                        | SearchFocusState::Videos
                        | SearchFocusState::Episodes => "Play",
                        _ => "Open",
                    };
                    [choose(action), pane("Next group"), esc("Query")]
                        .into_iter()
                        .chain(optional(Command::ResumePause, "Play/pause"))
                        .chain(optional(Command::Queue, "Queue"))
                        .collect()
                }
            };
            return FooterLine::layout(hints, help, available);
        }
    }
    if matches!(ui.current_page(), PageState::Home { .. })
        && ui.workspace_focus == WorkspaceFocusState::Context
    {
        // Home reads Left/Right directly to move within a shelf.
        let hints = [
            workspace_footer_key(Command::ChooseSelected).map(|key| FooterHint::new(key, "Open")),
            Some(FooterHint::new("←/→", "Card")),
            workspace_footer_key(Command::FocusNextWindow).map(|key| FooterHint::new(key, "Pane")),
            optional(Command::ResumePause, "Play/pause"),
            optional(Command::NextTrack, "Next"),
        ]
        .into_iter()
        .flatten()
        .collect();
        return FooterLine::layout(hints, help, available);
    }
    let focus_label = match ui.workspace_focus {
        WorkspaceFocusState::Navigation | WorkspaceFocusState::Context => "Open",
        WorkspaceFocusState::Queue => "Queue",
        WorkspaceFocusState::Actions => "Choose",
    };
    let hints = [
        workspace_footer_key(Command::ChooseSelected).map(|key| FooterHint::new(key, focus_label)),
        workspace_footer_key(Command::FocusNextWindow).map(|key| FooterHint::new(key, "Pane")),
        optional(Command::ResumePause, "Play/pause"),
        optional(Command::NextTrack, "Next"),
        optional(Command::Queue, "Queue"),
    ]
    .into_iter()
    .flatten()
    .collect();
    FooterLine::layout(hints, help, available)
}

fn render_workspace_footer(frame: &mut Frame, ui: &mut UIStateGuard, rect: Rect) {
    if rect.is_empty() {
        return;
    }
    let footer_rect = Rect::new(
        rect.x.saturating_add(2),
        rect.y,
        rect.width.saturating_sub(4),
        1,
    );
    let available = usize::from(footer_rect.width);
    let status_prefix = ui
        .operation_status
        .as_ref()
        .filter(|_| available >= 12)
        .map(|status| {
            format!(
                "{}   ",
                utils::bounded_text(&status.ordinary_display_line(), available / 2 - 3)
            )
        })
        .unwrap_or_default();
    let prefix_width = status_prefix.chars().count();
    let hints = workspace_footer_line(ui, available.saturating_sub(prefix_width));
    let mut spans = vec![Span::raw(status_prefix)];
    spans.extend(hints.spans(ui.theme.workspace_hint_key()));
    frame.render_widget(
        Paragraph::new(Line::from(spans)).style(if ui.operation_status.is_some() {
            status_footer_style(ui)
        } else {
            ui.theme.workspace_hint_text()
        }),
        footer_rect,
    );
    if let Some((start, width)) = hints.help() {
        let offset = prefix_width.saturating_add(start);
        if offset < footer_rect.width as usize {
            let x = footer_rect.x.saturating_add(offset as u16);
            let width = (width as u16).min(footer_rect.right().saturating_sub(x));
            ui.workspace_hits
                .push((Rect::new(x, footer_rect.y, width, 1), WorkspaceHit::Help));
        }
    }
}

fn status_footer_style(ui: &UIStateGuard) -> Style {
    let status = if let Some(operation) = ui.operation_status.as_ref() {
        Some(match operation.state {
            UiOperationState::Failed => UiViewStatus::Failed {
                code: operation.code,
                message: operation.message,
                next_action: operation
                    .next_action
                    .unwrap_or("Try again or open Diagnostics."),
            },
            UiOperationState::Unsupported => UiViewStatus::Unsupported {
                code: operation.code,
                message: operation.message,
                next_action: operation
                    .next_action
                    .unwrap_or("Choose another supported action."),
            },
            UiOperationState::Superseded => UiViewStatus::Superseded {
                code: operation.code,
                message: operation.message,
                next_action: operation
                    .next_action
                    .unwrap_or("Wait for the current operation."),
            },
            UiOperationState::Partial => UiViewStatus::Partial {
                code: operation.code,
                message: operation.message,
                next_action: operation
                    .next_action
                    .unwrap_or("Open Diagnostics or retry the request."),
            },
            UiOperationState::Running => UiViewStatus::Loading,
            UiOperationState::Completed => UiViewStatus::Ready,
            UiOperationState::Cancelled => UiViewStatus::Empty,
        })
    } else if let PageState::Search {
        state: crate::state::SearchPageUIState {
            search_lifecycle, ..
        },
        ..
    } = ui.current_page()
    {
        Some(search_lifecycle.view_status())
    } else {
        None
    };

    match status {
        None | Some(UiViewStatus::Idle) => ui.theme.workspace_base(),
        Some(UiViewStatus::Failed { .. }) => ui.theme.workspace_status_error(),
        Some(
            UiViewStatus::Partial { .. }
            | UiViewStatus::Unsupported { .. }
            | UiViewStatus::Superseded { .. }
            | UiViewStatus::Empty,
        ) => ui.theme.workspace_status_warning(),
        Some(UiViewStatus::Loading) => ui.theme.workspace_status_busy(),
        Some(UiViewStatus::Ready) => ui.theme.workspace_status_success(),
    }
}

pub(crate) fn status_color(status: UiViewStatus) -> Option<Color> {
    let color = match status {
        UiViewStatus::Failed { .. } => Color::Red,
        UiViewStatus::Partial { .. }
        | UiViewStatus::Unsupported { .. }
        | UiViewStatus::Superseded { .. }
        | UiViewStatus::Empty => Color::Yellow,
        UiViewStatus::Loading => Color::Cyan,
        UiViewStatus::Ready => Color::Green,
        UiViewStatus::Idle => return None,
    };
    Some(color)
}

pub(crate) fn view_status_style(theme: &config::Theme, status: UiViewStatus) -> Style {
    if status == UiViewStatus::Loading {
        return theme.playback_metadata();
    }
    status_color(status).map_or_else(|| theme.page_desc(), |color| Style::default().fg(color))
}

const LOADING_SPINNER_FRAMES: [&str; 10] = ["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];

fn loading_indicator_at(elapsed_ms: u128) -> String {
    let frame = LOADING_SPINNER_FRAMES[(elapsed_ms / 100) as usize % LOADING_SPINNER_FRAMES.len()];
    format!("{frame} Loading...")
}

pub(crate) fn loading_indicator() -> String {
    let elapsed_ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis();
    frame_schedule::request_frame_within(std::time::Duration::from_millis(
        100 - (elapsed_ms % 100) as u64,
    ));
    loading_indicator_at(elapsed_ms)
}

pub(crate) fn render_view_status(
    frame: &mut Frame,
    theme: &config::Theme,
    status: UiViewStatus,
    rect: Rect,
) {
    let message = if status == UiViewStatus::Loading {
        loading_indicator()
    } else {
        view_status_message(status)
    };
    frame.render_widget(
        Paragraph::new(message)
            .style(view_status_style(theme, status))
            .wrap(Wrap { trim: false }),
        rect,
    );
}

pub(crate) fn view_status_message(status: UiViewStatus) -> String {
    view_status_message_with_next_action(status, status.next_action())
}

pub(crate) fn view_status_message_with_next_action(
    status: UiViewStatus,
    next_action: Option<&str>,
) -> String {
    match next_action {
        Some(next_action) => format!("{} Next: {next_action}", status.display_message()),
        None => status.display_message().to_owned(),
    }
}

pub(crate) fn view_status_height(status: UiViewStatus, width: u16) -> u16 {
    Paragraph::new(view_status_message(status))
        .wrap(Wrap { trim: false })
        .line_count(width.max(1))
        .clamp(1, 3) as u16
}

#[cfg(test)]
mod render_tests {
    use std::{collections::VecDeque, sync::Arc};

    use parking_lot::Mutex;

    #[test]
    fn search_footer_stays_on_one_row_while_a_request_is_running() {
        super::initialize_test_config();
        let mutex = crate::state::TrackedMutex::new(crate::state::UIState::default());
        let mut ui = mutex.lock();
        ui.history.clear();
        ui.history.push(crate::state::PageState::Search {
            line_input: crate::ui::single_line_input::LineInput::default(),
            current_query: String::new(),
            state: crate::state::SearchPageUIState::new(),
        });
        ui.begin_search(crate::config::ActiveProvider::Spotify, "neon");
        for (width, height) in [(80, 24), (120, 35), (180, 49), (140, 40)] {
            let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
            terminal
                .draw(|frame| {
                    frame.render_widget(
                        ratatui::widgets::Paragraph::new("#".repeat(usize::from(width))),
                        Rect::new(0, 1, width, 1),
                    );
                    super::render_workspace_footer(frame, &mut ui, Rect::new(0, 0, width, 3));
                })
                .unwrap();
            let buffer = terminal.backend().buffer();
            assert_eq!(
                (0..width)
                    .map(|x| buffer[(x, 1)].symbol())
                    .collect::<String>(),
                "#".repeat(usize::from(width))
            );
            let text = (0..width)
                .map(|x| buffer[(x, 0)].symbol())
                .collect::<String>();
            assert!(text.contains("Searching"));
            assert!(text.contains("enter Search"));
            if width >= 120 {
                assert!(text.contains("? Commands"));
            }
            let key_x = text.find("enter").unwrap() as u16;
            assert_eq!(
                buffer[(key_x, 0)].fg,
                ui.theme.workspace_hint_key().fg.unwrap()
            );
        }
    }
    use ratatui::{backend::TestBackend, layout::Rect, style::Color, Terminal};

    use super::{
        loading_indicator_at, render_application, status_color, view_status_height,
        view_status_message, view_status_message_with_next_action, LayoutPolicy,
    };
    use crate::{
        config,
        observability::UiDiagnosticEntry,
        state::{
            synchronize_unified_playlist_entries, DiagnosticsPageUIState, MediaId, MediaKind,
            PageState, PlaylistEntryId, Provider, SharedState, State, UiViewStatus,
            UnifiedPlaylist, UnifiedPlaylistItem,
        },
    };

    #[test]
    fn narrow_playback_offers_a_volume_symbol_that_anchors_the_volume_popup() {
        let configs = super::initialize_test_config();
        let ui_ring = Arc::new(Mutex::new(VecDeque::<UiDiagnosticEntry>::new()));
        let (diagnostics, _runtime) = crate::observability::disabled(ui_ring);
        let state: SharedState = Arc::new(State::new_with_configs(false, diagnostics, configs));
        {
            let mut player = state.player.write();
            player.active_playback_provider = Some(config::ActiveProvider::YouTubeMusic);
            player.youtube_playback = Some(crate::state::YouTubePlayback {
                track: crate::state::YouTubeTrack {
                    id: "volume-test".into(),
                    name: "Track".into(),
                    artists: "Artist".into(),
                    album: None,
                    duration: "3:00".into(),
                    explicit: false,
                    thumbnail_url: None,
                    is_video: false,
                },
                is_playing: true,
                progress: std::time::Duration::from_secs(45),
                volume: 35,
                mute_state: None,
                route: Default::default(),
            });
        }
        let mut terminal = Terminal::new(TestBackend::new(80, 24)).unwrap();
        let mut ui = state.ui.lock();
        let policy = super::LayoutPolicy::from_size(80, 24);
        ui.orientation = policy.orientation;
        ui.layout_mode = policy.mode;
        terminal
            .draw(|frame| render_application(frame, &state, &mut ui, frame.area()))
            .unwrap();

        assert!(ui
            .workspace_hit_rect(crate::state::WorkspaceHit::PlaybackOption(
                crate::state::WorkspacePlaybackOption::Volume
            ))
            .is_none());
        let icon = ui
            .workspace_hit_rect(crate::state::WorkspaceHit::VolumeMenu)
            .expect("the narrow transport shows the volume symbol");
        assert_eq!(
            terminal.backend().buffer()[(icon.x, icon.y)].symbol(),
            config::get_config().app_config.volume_icon
        );

        ui.popup = Some(crate::state::PopupState::Volume {
            anchor: icon,
            input: crate::ui::single_line_input::LineInput::new(Vec::new()),
        });
        terminal
            .draw(|frame| render_application(frame, &state, &mut ui, frame.area()))
            .unwrap();

        let popup = ui.popup_rect;
        assert!(popup.bottom() <= icon.y, "the popup opens above the symbol");
        assert!(popup.right() <= icon.right());
        let slider = ui
            .workspace_popup_hits
            .iter()
            .find(|(_, index)| *index == 0)
            .map(|(rect, _)| *rect)
            .expect("the popup slider is a pointer target");
        assert!(popup.contains(slider.as_position()));
        assert!(
            slider.width > 20,
            "the popup slider is longer than the inline one"
        );
        let text = (popup.y..popup.bottom())
            .map(|y| {
                (popup.x..popup.right())
                    .map(|x| terminal.backend().buffer()[(x, y)].symbol())
                    .collect::<String>()
            })
            .collect::<Vec<_>>()
            .join("\n");
        assert!(text.contains("Volume"));
        assert!(text.contains(" 35%"));
        assert!(text.contains("Set to"));
        assert!(
            !text.contains("ens first"),
            "page text must not show through the popup"
        );
    }

    #[test]
    fn runtime_layout_switch_preserves_state_and_replaces_rendered_geometry() {
        let configs = super::initialize_test_config();
        let ui_ring = Arc::new(Mutex::new(VecDeque::<UiDiagnosticEntry>::new()));
        let (diagnostics, _runtime) = crate::observability::disabled(ui_ring);
        let state: SharedState = Arc::new(State::new_with_configs(false, diagnostics, configs));
        state.player.write().youtube_playback = Some(crate::state::YouTubePlayback {
            track: crate::state::YouTubeTrack {
                id: "layout-test".into(),
                name: "Track".into(),
                artists: "Artist".into(),
                album: None,
                duration: "3:00".into(),
                explicit: false,
                thumbnail_url: None,
                is_video: false,
            },
            is_playing: true,
            progress: std::time::Duration::from_secs(45),
            volume: 73,
            mute_state: None,
            route: Default::default(),
        });
        let mut app = config::AppConfig::default();
        app.border_type = config::BorderType::Double;
        let mut ui = state.ui.lock();
        ui.active_provider = config::ActiveProvider::YouTubeMusic;
        let settings_folder = tempfile::tempdir().unwrap();
        let settings: Vec<_> = config::app_config_settings(settings_folder.path())
            .unwrap()
            .into_iter()
            .filter(|setting| setting.section == config::AppConfigSection::SharedUi)
            .collect();
        assert!(settings.len() > 12);
        ui.history.push(PageState::Settings {
            list: ratatui::widgets::ListState::default().with_selected(Some(8)),
            shelves: crate::state::SettingsShelves::default(),
            settings,
            saved: false,
            error: None,
            notice: None,
        });
        let mut previous_history = None;
        let mut previous_progress = None;
        for (preset, position, height, width, rows) in [
            (
                config::LayoutPreset::Current,
                config::Position::Top,
                6,
                80,
                24,
            ),
            (
                config::LayoutPreset::Borderless,
                config::Position::Top,
                6,
                80,
                24,
            ),
            (
                config::LayoutPreset::Borderless,
                config::Position::Bottom,
                9,
                80,
                24,
            ),
            (
                config::LayoutPreset::Current,
                config::Position::Bottom,
                6,
                40,
                16,
            ),
        ] {
            app.presentation.layout_preset = preset;
            app.layout.playback_window_position = position;
            app.layout.playback_window_height = height;
            let history = format!("{:?}", ui.history);
            ui.apply_presentation_config(&app);
            assert_eq!(format!("{:?}", ui.history), history);
            assert_eq!(ui.active_provider, config::ActiveProvider::YouTubeMusic);
            assert!(ui.playback_window_rect.is_empty());
            assert!(ui.playback_progress_bar_rect.is_empty());
            // A theme preview replaces Theme: the next frame must re-project chrome.
            ui.theme = config::Theme::default();
            let mut terminal = Terminal::new(TestBackend::new(width, rows)).unwrap();
            terminal
                .draw(|frame| render_application(frame, &state, &mut ui, frame.area()))
                .unwrap();
            let playback = ui.playback_window_rect;
            let progress = ui.playback_progress_bar_rect;
            if playback.height < 3 {
                assert!(progress.is_empty());
            } else {
                assert!(!progress.is_empty());
                assert_eq!(progress.intersection(playback), progress);
            }
            // The workspace transport is never drawn with a bordered frame.
            let corner = terminal.backend().buffer()[(playback.x, playback.y)].symbol();
            assert_ne!(corner, "╔");
            if preset == config::LayoutPreset::Borderless && position == config::Position::Top {
                assert_eq!(Some(progress), previous_progress);
                assert_eq!(Some(format!("{:?}", ui.history)), previous_history);
            }
            if let PageState::Settings { list, .. } = ui.current_page() {
                assert_eq!(list.selected(), Some(8));
            } else {
                panic!("layout switch replaced the focused Settings page");
            }
            previous_history = Some(format!("{:?}", ui.history));
            previous_progress = Some(progress);
            assert!(playback.bottom() <= rows);
            assert!(
                state
                    .player
                    .read()
                    .youtube_playback
                    .as_ref()
                    .unwrap()
                    .is_playing
            );
        }
        state.player.write().youtube_playback = None;
        let mut terminal = Terminal::new(TestBackend::new(40, 16)).unwrap();
        terminal
            .draw(|frame| render_application(frame, &state, &mut ui, frame.area()))
            .unwrap();
        assert!(ui.playback_progress_bar_rect.is_empty());
    }

    #[test]
    fn status_footer_color_tracks_lifecycle_semantics() {
        assert_eq!(status_color(UiViewStatus::Loading), Some(Color::Cyan));
        assert_eq!(status_color(UiViewStatus::Ready), Some(Color::Green));
        assert_eq!(
            status_color(UiViewStatus::Partial {
                code: "PARTIAL",
                message: "partial",
                next_action: "retry",
            }),
            Some(Color::Yellow)
        );
        assert_eq!(
            status_color(UiViewStatus::Unsupported {
                code: "UNAVAILABLE",
                message: "unavailable",
                next_action: "retry",
            }),
            Some(Color::Yellow)
        );
        assert_eq!(status_color(UiViewStatus::Idle), None);
    }

    #[test]
    fn shared_loading_indicator_uses_stable_braille_frames() {
        assert_eq!(loading_indicator_at(0), "⠋ Loading...");
        assert_eq!(loading_indicator_at(100), "⠙ Loading...");
        assert_eq!(loading_indicator_at(1_000), "⠋ Loading...");
    }

    #[test]
    fn application_renderer_survives_supported_terminal_fixtures() {
        let configs = super::initialize_test_config();

        let ui_ring = Arc::new(Mutex::new(VecDeque::<UiDiagnosticEntry>::new()));
        let (diagnostics, _runtime) = crate::observability::disabled(ui_ring);
        let state: SharedState = Arc::new(State::new_with_configs(false, diagnostics, configs));

        for (columns, rows) in [(60, 20), (80, 23), (80, 24), (80, 30), (120, 40)] {
            let policy = LayoutPolicy::from_size(columns, rows);
            let mut terminal = Terminal::new(TestBackend::new(columns, rows)).unwrap();

            let mut ui = state.ui.lock();
            ui.orientation = policy.orientation;
            ui.layout_mode = policy.mode;
            terminal
                .draw(|frame| render_application(frame, &state, &mut ui, frame.area()))
                .unwrap();
        }

        drop(state);
    }

    #[test]
    fn design_v1_library_projects_the_wide_workspace_geometry() {
        let configs = super::initialize_test_config();
        let ui_ring = Arc::new(Mutex::new(VecDeque::<UiDiagnosticEntry>::new()));
        let (diagnostics, _runtime) = crate::observability::disabled(ui_ring);
        let state: SharedState = Arc::new(State::new_with_configs(false, diagnostics, configs));
        let mut terminal = Terminal::new(TestBackend::new(180, 49)).unwrap();

        {
            let mut ui = state.ui.lock();
            ui.history.clear();
            ui.history.push(PageState::Library {
                state: crate::state::LibraryPageUIState::new(),
            });
            terminal
                .draw(|frame| render_application(frame, &state, &mut ui, frame.area()))
                .unwrap();

            assert!(ui.workspace_layout.show_navigation);
            assert_eq!(ui.workspace_layout.navigation.width, 26);
            assert_eq!(
                ui.workspace_layout.content.x,
                ui.workspace_layout.navigation.right() + 1
            );
            assert!(ui
                .workspace_hit_at(
                    ui.workspace_layout.navigation.x + 3,
                    ui.workspace_layout.navigation.y + 5
                )
                .is_some());
        }

        let rendered = terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .map(|cell| cell.symbol())
            .collect::<String>();
        assert!(rendered.contains("Playlists"));
        assert!(rendered.contains("Albums"));
        assert!(rendered.contains("Artists"));
        assert!(rendered.contains("Liked Music"));
        assert!(rendered.contains("Search"));
        assert!(rendered.contains("Queue"));
        assert_eq!(terminal.backend().buffer()[(3, 33)].symbol(), "›");
        assert_eq!(terminal.backend().buffer()[(5, 33)].symbol(), "B");
        assert_eq!(terminal.backend().buffer()[(5, 34)].symbol(), "@");
        assert_eq!(terminal.backend().buffer()[(5, 35)].symbol(), "P");
        assert_eq!(terminal.backend().buffer()[(5, 33)].fg, Color::Green);
        assert_eq!(terminal.backend().buffer()[(5, 35)].fg, Color::Green);
        assert_eq!(terminal.backend().buffer()[(27, 36)].symbol(), "─");

        {
            let mut ui = state.ui.lock();
            ui.active_provider = crate::config::ActiveProvider::YouTubeMusic;
            terminal
                .draw(|frame| render_application(frame, &state, &mut ui, frame.area()))
                .unwrap();
        }
        assert_eq!(terminal.backend().buffer()[(5, 33)].fg, Color::Red);
        assert_eq!(terminal.backend().buffer()[(5, 35)].fg, Color::Red);
    }

    #[test]
    fn design_v1_search_projects_the_canonical_wide_workspace_geometry() {
        let configs = super::initialize_test_config();
        let ui_ring = Arc::new(Mutex::new(VecDeque::<UiDiagnosticEntry>::new()));
        let (diagnostics, _runtime) = crate::observability::disabled(ui_ring);
        let state: SharedState = Arc::new(State::new_with_configs(false, diagnostics, configs));
        let track = crate::state::Track {
            id: crate::state::TrackId::from_id("4iV5W9uYEdYUVa79Axb7Rh")
                .unwrap()
                .into_static(),
            name: "Example track".to_owned(),
            artists: vec![crate::state::Artist {
                id: crate::state::ArtistId::from_id("0OdUWJ0sBjDrqHygGUXeCF")
                    .unwrap()
                    .into_static(),
                name: "Example artist".to_owned(),
            }],
            album: None,
            duration: std::time::Duration::from_secs(180),
            explicit: false,
            added_at: 0,
        };
        state.data.write().caches.search.insert(
            "mozart".to_owned(),
            std::sync::Arc::new(crate::state::SearchResults {
                tracks: vec![track],
                ..crate::state::SearchResults::default()
            }),
            std::time::Duration::from_secs(60),
        );
        let mut terminal = Terminal::new(TestBackend::new(180, 49)).unwrap();

        {
            let mut ui = state.ui.lock();
            ui.history.clear();
            ui.history.push(PageState::Search {
                line_input: crate::ui::single_line_input::LineInput::new(
                    "mozart".chars().collect(),
                ),
                current_query: "mozart".to_owned(),
                state: crate::state::SearchPageUIState::new(),
            });
            ui.workspace_navigation = crate::state::WorkspaceNavigationItem::Search;
            ui.layout_mode = crate::ui::LayoutMode::Wide;
            ui.orientation = crate::ui::Orientation::Horizontal;
            terminal
                .draw(|frame| render_application(frame, &state, &mut ui, frame.area()))
                .unwrap();

            assert_eq!(
                ui.workspace_navigation,
                crate::state::WorkspaceNavigationItem::Search
            );
            assert_eq!(ui.workspace_layout.navigation, Rect::new(0, 3, 26, 35));
            assert_eq!(ui.workspace_layout.content, Rect::new(27, 3, 153, 35));
            assert_eq!(
                ui.workspace_hit_at(30, 7),
                Some(crate::state::WorkspaceHit::SearchInput)
            );
            assert_eq!(
                ui.workspace_hit_at(29, 5),
                Some(crate::state::WorkspaceHit::SearchCategory(None))
            );
            assert_eq!(
                ui.workspace_hit_at(36, 5),
                Some(crate::state::WorkspaceHit::SearchCategory(Some(
                    crate::command::ProviderSearchPane::Tracks
                )))
            );
            assert_eq!(
                ui.workspace_hit_at(32, 12),
                Some(crate::state::WorkspaceHit::SearchRow {
                    focus: crate::state::SearchFocusState::Tracks,
                    index: 0,
                })
            );
        }

        let buffer = terminal.backend().buffer();
        assert_eq!(buffer[(29, 6)].bg, Color::Rgb(26, 26, 28));
        assert_eq!(buffer[(30, 10)].bg, Color::Rgb(11, 11, 12));
        assert_eq!(buffer[(29, 5)].symbol(), "A");
        assert_eq!(buffer[(36, 5)].symbol(), "T");
        assert_eq!(buffer[(153, 7)].symbol(), "e");
        assert_eq!(buffer[(158, 37)].symbol(), "E");
        assert_eq!(buffer[(5, 33)].symbol(), "B");
        assert_eq!(buffer[(31, 10)].symbol(), "T");
        assert_eq!(buffer[(103, 10)].symbol(), "│");
        let rendered = buffer
            .content()
            .iter()
            .map(|cell| cell.symbol())
            .collect::<String>();
        for label in [
            "Search",
            "All",
            "Tracks   Albums",
            "Tracks",
            "Albums",
            "Artists",
            "Playlists",
            "Shows",
            "Episodes",
            "Results for: mozart",
        ] {
            assert!(
                rendered.contains(label),
                "missing {label:?} in Search frame"
            );
        }
        for label in ["› Browse", "› @", "› Play"] {
            assert!(
                rendered.contains(label),
                "missing {label:?} in Search frame"
            );
        }

        {
            let mut ui = state.ui.lock();
            if let PageState::Search { state, .. } = ui.current_page_mut() {
                state.focus = crate::state::SearchFocusState::Tracks;
                state.track_list.select(Some(0));
            }
            terminal
                .draw(|frame| render_application(frame, &state, &mut ui, frame.area()))
                .unwrap();
        }
        let results_buffer = terminal.backend().buffer();
        assert_eq!(results_buffer[(162, 37)].symbol(), "e");
        assert_eq!(results_buffer[(168, 37)].symbol(), "P");

        {
            let mut ui = state.ui.lock();
            if let PageState::Search { state, .. } = ui.current_page_mut() {
                state.category = Some(crate::command::ProviderSearchPane::Albums);
                state.focus = crate::state::SearchFocusState::Category;
            }
            terminal
                .draw(|frame| render_application(frame, &state, &mut ui, frame.area()))
                .unwrap();
        }
        let category_buffer = terminal.backend().buffer();
        assert_eq!(category_buffer[(29, 5)].symbol(), "A");
        assert_eq!(category_buffer[(45, 5)].symbol(), "A");
        assert_eq!(category_buffer[(29, 10)].symbol(), " ");
    }

    #[test]
    fn design_v1_search_hides_navigation_and_unselected_groups_when_compact() {
        let configs = super::initialize_test_config();
        let ui_ring = Arc::new(Mutex::new(VecDeque::<UiDiagnosticEntry>::new()));
        let (diagnostics, _runtime) = crate::observability::disabled(ui_ring);
        let state: SharedState = Arc::new(State::new_with_configs(false, diagnostics, configs));
        let mut terminal = Terminal::new(TestBackend::new(60, 20)).unwrap();

        {
            let mut ui = state.ui.lock();
            ui.history.clear();
            ui.history.push(PageState::Search {
                line_input: crate::ui::single_line_input::LineInput::default(),
                current_query: String::new(),
                state: crate::state::SearchPageUIState::new(),
            });
            ui.workspace_navigation = crate::state::WorkspaceNavigationItem::Search;
            ui.layout_mode = crate::ui::LayoutMode::Compact;
            ui.orientation = crate::ui::Orientation::Vertical;
            terminal
                .draw(|frame| render_application(frame, &state, &mut ui, frame.area()))
                .unwrap();

            assert!(!ui.workspace_layout.show_navigation);
            // A 13-row page uses the compact layout: title and categories on
            // the first row, the one-row query right below it.
            let content = ui.workspace_layout.content;
            assert_eq!(
                ui.workspace_hit_at(3, content.y + 1),
                Some(crate::state::WorkspaceHit::SearchInput)
            );
            assert_eq!(
                ui.workspace_hits
                    .iter()
                    .find(|(_, hit)| *hit == crate::state::WorkspaceHit::SearchInput)
                    .map(|(rect, _)| rect.height),
                Some(1)
            );
            assert!(ui
                .workspace_hits
                .iter()
                .all(|(_, hit)| !matches!(hit, crate::state::WorkspaceHit::Navigation(_))));
        }

        let rendered = terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .map(|cell| cell.symbol())
            .collect::<String>();
        assert!(rendered.contains("Search"));
        assert!(rendered.contains("Tracks"));
    }

    #[test]
    fn settings_rows_stay_above_the_bottom_band_for_every_selection() {
        let configs = super::initialize_test_config();
        let ui_ring = Arc::new(Mutex::new(VecDeque::<UiDiagnosticEntry>::new()));
        let (diagnostics, _runtime) = crate::observability::disabled(ui_ring);
        let state: SharedState = Arc::new(State::new_with_configs(false, diagnostics, configs));
        let settings_folder = tempfile::tempdir().unwrap();
        let settings = config::app_config_settings(settings_folder.path()).unwrap();
        let count = settings.len();
        let (mut saw_inspector, mut saw_description) = (false, false);

        // Canonical (two-row steps), inspector without canonical, and narrow
        // with the description line, at heights of both parities.
        for (width, height, mode) in [
            (180, 49, crate::ui::LayoutMode::Wide),
            (180, 50, crate::ui::LayoutMode::Wide),
            (120, 28, crate::ui::LayoutMode::Wide),
            (80, 24, crate::ui::LayoutMode::Compact),
            (80, 25, crate::ui::LayoutMode::Compact),
        ] {
            let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
            let mut ui = state.ui.lock();
            ui.history.clear();
            ui.history.push(PageState::Settings {
                list: ratatui::widgets::ListState::default().with_selected(Some(0)),
                shelves: crate::state::SettingsShelves::default(),
                settings: settings.clone(),
                saved: false,
                error: None,
                notice: None,
            });
            ui.workspace_focus = crate::state::WorkspaceFocusState::Context;
            ui.layout_mode = mode;
            ui.orientation = crate::ui::Orientation::Horizontal;
            for selected in 0..count {
                ui.current_page_mut().select(selected);
                terminal
                    .draw(|frame| render_application(frame, &state, &mut ui, frame.area()))
                    .unwrap();
                let buffer = terminal.backend().buffer();
                let pending_y = (0..height)
                    .find(|&y| {
                        (0..width)
                            .map(|x| buffer[(x, y)].symbol())
                            .collect::<String>()
                            .contains("No unsaved changes")
                    })
                    .expect("the pending-change line is drawn");
                let band_top = if ui.workspace_layout.right.is_empty() {
                    saw_description = true;
                    pending_y - 2
                } else {
                    saw_inspector = true;
                    pending_y - 1
                };
                for (rect, hit) in &ui.workspace_hits {
                    if matches!(hit, crate::state::WorkspaceHit::SettingsRow(_)) {
                        assert!(
                            rect.y < band_top,
                            "{width}x{height}, selection {selected}: row at {} reaches the band at {band_top}",
                            rect.y
                        );
                    }
                }
            }
        }
        assert!(
            saw_inspector && saw_description,
            "both bottom-band layouts covered"
        );
    }

    #[test]
    fn design_v1_settings_projects_canonical_rail_tiles_and_inspector() {
        let configs = super::initialize_test_config();
        let ui_ring = Arc::new(Mutex::new(VecDeque::<UiDiagnosticEntry>::new()));
        let (diagnostics, _runtime) = crate::observability::disabled(ui_ring);
        let state: SharedState = Arc::new(State::new_with_configs(false, diagnostics, configs));
        let settings_folder = tempfile::tempdir().unwrap();
        let settings = config::app_config_settings(settings_folder.path()).unwrap();
        let mut terminal = Terminal::new(TestBackend::new(180, 49)).unwrap();

        {
            let mut ui = state.ui.lock();
            ui.history.clear();
            ui.history.push(PageState::Settings {
                list: ratatui::widgets::ListState::default().with_selected(Some(0)),
                shelves: crate::state::SettingsShelves::default(),
                settings,
                saved: false,
                error: None,
                notice: None,
            });
            ui.workspace_focus = crate::state::WorkspaceFocusState::Context;
            ui.layout_mode = crate::ui::LayoutMode::Wide;
            ui.orientation = crate::ui::Orientation::Horizontal;
            terminal
                .draw(|frame| render_application(frame, &state, &mut ui, frame.area()))
                .unwrap();

            assert_eq!(ui.workspace_layout.navigation, Rect::new(0, 3, 26, 35));
            assert_eq!(ui.workspace_layout.content, Rect::new(27, 3, 110, 35));
            assert_eq!(ui.workspace_layout.right, Rect::new(138, 3, 42, 35));
            assert!(matches!(
                ui.workspace_hit_at(29, 10),
                Some(crate::state::WorkspaceHit::SettingsRow(_))
            ));
        }

        let buffer = terminal.backend().buffer();
        assert_eq!(buffer[(29, 8)].symbol(), "S");
        assert_eq!(buffer[(29, 10)].bg, Color::Rgb(245, 177, 131));
        let footer_y = state.ui.lock().workspace_layout.navigation.bottom() - 3;
        assert_eq!(buffer[(2, footer_y)].symbol(), "E");
        assert_eq!(buffer[(2, footer_y + 1)].symbol(), "L");
        let rendered = buffer
            .content()
            .iter()
            .map(|cell| cell.symbol())
            .collect::<String>();
        for label in [
            "Unified Player",
            "Settings",
            "Preferences",
            "Accounts",
            "Spotify",
            "Client port",
            "YouTube Music",
            "Control",
            "Saved",
            "Effective",
            "Apply changes",
            "Discard",
        ] {
            assert!(
                rendered.contains(label),
                "missing {label:?} in Settings frame"
            );
        }
    }

    #[test]
    fn home_show_all_lists_every_continue_entry_as_clickable_rows() {
        let configs = super::initialize_test_config();
        let ui_ring = Arc::new(Mutex::new(VecDeque::<UiDiagnosticEntry>::new()));
        let (diagnostics, _runtime) = crate::observability::disabled(ui_ring);
        let state: SharedState = Arc::new(State::new_with_configs(false, diagnostics, configs));
        let folder = tempfile::tempdir().unwrap();
        *state.data.write() = crate::state::AppData::new(folder.path(), folder.path());
        {
            let mut data = state.data.write();
            for index in 0..30 {
                data.context_history
                    .record(crate::state::ContextHistoryEntry::from_unified(
                        &crate::state::UnifiedPlaylist {
                            id: format!("mix-{index}"),
                            name: format!("Mix {index}"),
                            items: Vec::new(),
                            updated_at: 0,
                            next_entry_id: 1,
                        },
                        0,
                    ));
            }
        }
        let mut terminal = Terminal::new(TestBackend::new(160, 45)).unwrap();
        let mut ui = state.ui.lock();
        let mut list = ratatui::widgets::ListState::default();
        list.select(Some(0));
        ui.history = vec![PageState::HomeShelfList {
            shelf: crate::state::HomeShelfKind::Continue,
            list,
        }];
        terminal
            .draw(|frame| render_application(frame, &state, &mut ui, frame.area()))
            .unwrap();

        let text = terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .map(|cell| cell.symbol())
            .collect::<String>();
        assert!(text.contains("Continue where you left off"));
        assert!(text.contains("Mix 29"), "newest first");
        assert!(ui
            .workspace_hits
            .iter()
            .any(|(_, hit)| *hit == crate::state::WorkspaceHit::HomeListRow(0)));
        assert_eq!(
            crate::state::home_shelf_list(
                &state.data.read(),
                ui.home_scope(),
                crate::state::HomeShelfKind::Continue
            )
            .len(),
            30,
            "Show all is not capped at the shelf's 20 cards"
        );
    }

    #[test]
    fn home_keeps_cards_visible_in_a_short_terminal() {
        let configs = super::initialize_test_config();
        let ui_ring = Arc::new(Mutex::new(VecDeque::<UiDiagnosticEntry>::new()));
        let (diagnostics, _runtime) = crate::observability::disabled(ui_ring);
        let state: SharedState = Arc::new(State::new_with_configs(false, diagnostics, configs));
        for (width, height) in [(107, 13), (80, 16), (180, 49)] {
            let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
            let mut ui = state.ui.lock();
            ui.history.clear();
            ui.history.push(PageState::Home {
                state: crate::state::HomePageUIState::default(),
            });
            terminal
                .draw(|frame| render_application(frame, &state, &mut ui, frame.area()))
                .unwrap();

            assert!(
                ui.workspace_hits.iter().any(|(_, hit)| matches!(
                    hit,
                    crate::state::WorkspaceHit::HomeCard {
                        shelf: crate::state::HomeShelfKind::QuickAccess,
                        index: 0,
                    }
                )),
                "Liked Music must be reachable at {width}x{height}"
            );
        }
    }

    #[test]
    fn workspace_hover_paints_the_current_hit_after_rebuilding_the_frame() {
        let configs = super::initialize_test_config();
        let ui_ring = Arc::new(Mutex::new(VecDeque::<UiDiagnosticEntry>::new()));
        let (diagnostics, _runtime) = crate::observability::disabled(ui_ring);
        let state: SharedState = Arc::new(State::new_with_configs(false, diagnostics, configs));
        let mut terminal = Terminal::new(TestBackend::new(180, 49)).unwrap();

        {
            let mut ui = state.ui.lock();
            ui.history.clear();
            ui.history.push(PageState::Library {
                state: crate::state::LibraryPageUIState::new(),
            });
            ui.set_workspace_pointer(5, 8);
            terminal
                .draw(|frame| render_application(frame, &state, &mut ui, frame.area()))
                .unwrap();

            assert_eq!(ui.workspace_hover_rect(), Some(Rect::new(2, 8, 22, 1)));
        }

        let buffer = terminal.backend().buffer();
        assert_eq!(buffer[(5, 8)].bg, Color::Rgb(34, 34, 36));
        assert_ne!(buffer[(5, 10)].bg, Color::Rgb(34, 34, 36));
    }

    #[test]
    fn anchored_workspace_action_popup_stays_at_the_clicked_row_and_exposes_hits() {
        let configs = super::initialize_test_config();
        let ui_ring = Arc::new(Mutex::new(VecDeque::<UiDiagnosticEntry>::new()));
        let (diagnostics, _runtime) = crate::observability::disabled(ui_ring);
        let state: SharedState = Arc::new(State::new_with_configs(false, diagnostics, configs));
        let track = crate::state::Track {
            id: rspotify::model::TrackId::from_id("4iV5W9uYEdYUVa79Axb7Rh")
                .unwrap()
                .into_static(),
            name: "Example track".to_owned(),
            artists: Vec::new(),
            album: None,
            duration: std::time::Duration::from_secs(180),
            explicit: false,
            added_at: 0,
        };
        let anchor = Rect::new(2, 30, 22, 1);
        let mut terminal = Terminal::new(TestBackend::new(180, 49)).unwrap();
        {
            let mut ui = state.ui.lock();
            ui.history.clear();
            ui.history.push(PageState::Library {
                state: crate::state::LibraryPageUIState::new(),
            });
            ui.popup = Some(crate::state::PopupState::AnchoredActionList {
                item: Box::new(crate::state::ActionListItem::Track(
                    track,
                    vec![crate::command::Action::AddToQueue],
                )),
                state: ratatui::widgets::ListState::default(),
                anchor,
            });
            terminal
                .draw(|frame| render_application(frame, &state, &mut ui, frame.area()))
                .unwrap();
            assert_eq!(ui.workspace_popup_hit_at(3, 32), Some(0));
        }
        let rendered = terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .map(|cell| cell.symbol())
            .collect::<String>();
        assert!(rendered.contains("Actions: Example track"));
        assert!(rendered.contains("Add to queue"));
    }

    #[test]
    fn design_v1_settings_removes_rail_and_inspector_on_compact_terminals() {
        let configs = super::initialize_test_config();
        let ui_ring = Arc::new(Mutex::new(VecDeque::<UiDiagnosticEntry>::new()));
        let (diagnostics, _runtime) = crate::observability::disabled(ui_ring);
        let state: SharedState = Arc::new(State::new_with_configs(false, diagnostics, configs));
        let settings_folder = tempfile::tempdir().unwrap();
        let mut terminal = Terminal::new(TestBackend::new(60, 20)).unwrap();

        {
            let mut ui = state.ui.lock();
            ui.history.clear();
            ui.history.push(PageState::Settings {
                list: ratatui::widgets::ListState::default().with_selected(Some(0)),
                shelves: crate::state::SettingsShelves::default(),
                settings: config::app_config_settings(settings_folder.path()).unwrap(),
                saved: false,
                error: None,
                notice: None,
            });
            ui.layout_mode = crate::ui::LayoutMode::Compact;
            ui.orientation = crate::ui::Orientation::Vertical;
            terminal
                .draw(|frame| render_application(frame, &state, &mut ui, frame.area()))
                .unwrap();

            assert!(!ui.workspace_layout.show_navigation);
            assert!(!ui.workspace_layout.show_right);
            assert!(matches!(
                ui.workspace_hit_at(3, 6),
                Some(crate::state::WorkspaceHit::SettingsRow(_))
            ));
        }

        let rendered = terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .map(|cell| cell.symbol())
            .collect::<String>();
        assert!(rendered.contains("Settings · Preferences"));
        assert!(rendered.contains("Spotify"));
        assert!(rendered.contains("Client port"));
        assert!(!rendered.contains("Accounts"));
    }

    #[test]
    fn design_v1_collection_projects_queue_and_action_sidebar() {
        let configs = super::initialize_test_config();
        let ui_ring = Arc::new(Mutex::new(VecDeque::<UiDiagnosticEntry>::new()));
        let (diagnostics, _runtime) = crate::observability::disabled(ui_ring);
        let state: SharedState = Arc::new(State::new_with_configs(false, diagnostics, configs));
        let tracks = (1..=3)
            .map(|index| crate::state::YouTubeTrack {
                id: format!("queue-{index}"),
                name: format!("Queue item {index}"),
                artists: "Queue artist".to_owned(),
                album: None,
                duration: "3:00".to_owned(),
                explicit: false,
                thumbnail_url: None,
                is_video: false,
            })
            .collect::<Vec<_>>();
        {
            let mut player = state.player.write();
            player.active_playback_provider = Some(config::ActiveProvider::YouTubeMusic);
            player.unified_queue = Some(crate::state::UnifiedQueue::new(
                tracks
                    .iter()
                    .cloned()
                    .map(crate::state::PlayableMedia::YouTube)
                    .collect(),
                0,
            ));
            player.youtube_playback = Some(crate::state::YouTubePlayback {
                track: tracks[0].clone(),
                is_playing: true,
                progress: std::time::Duration::from_secs(34),
                volume: 70,
                mute_state: None,
                route: Default::default(),
            });
        }
        let mut terminal = Terminal::new(TestBackend::new(180, 49)).unwrap();
        {
            let mut ui = state.ui.lock();
            ui.active_provider = config::ActiveProvider::YouTubeMusic;
            ui.workspace_focus = crate::state::WorkspaceFocusState::Context;
            ui.history.clear();
            ui.history.push(PageState::YouTubeContext {
                id: crate::state::YouTubeContextId::LikedTracks,
                context: Some(crate::state::YouTubeContext {
                    title: "Queue context".to_owned(),
                    description: None,
                    tracks,
                    playlist_set_video_ids: Vec::new(),
                    artist: None,
                }),
                state: crate::state::YouTubeContextPageUIState::new(),
            });
            ui.current_page_mut().select(2);
            terminal
                .draw(|frame| render_application(frame, &state, &mut ui, frame.area()))
                .unwrap();

            assert!(ui.workspace_layout.show_right);
            assert_eq!(ui.workspace_layout.content.x, 27);
            assert_eq!(ui.workspace_layout.content.width, 118);
            assert_eq!(ui.workspace_layout.right.x, 146);
            assert_eq!(ui.workspace_layout.right.width, 34);
            assert_eq!(
                ui.workspace_hit_at(148, ui.workspace_layout.right.y + 3),
                Some(crate::state::WorkspaceHit::QueueRow(0))
            );
            for (offset, option) in [
                (17, crate::state::WorkspacePlaybackOption::Shuffle),
                (19, crate::state::WorkspacePlaybackOption::Repeat),
                (21, crate::state::WorkspacePlaybackOption::Volume),
            ] {
                assert_eq!(
                    ui.workspace_hit_at(
                        ui.workspace_layout.right.x + 15,
                        ui.workspace_layout.right.y + offset
                    ),
                    Some(crate::state::WorkspaceHit::PlaybackOption(option))
                );
            }
        }
        let buffer = terminal.backend().buffer();
        assert_eq!(buffer[(26, 3)].symbol(), "│");
        assert_eq!(buffer[(145, 3)].symbol(), "│");
        assert_eq!(buffer[(28, 8)].symbol(), "│");
        assert_eq!(buffer[(32, 8)].symbol(), "#");
        assert_eq!(buffer[(37, 8)].symbol(), "T");
        assert_eq!(buffer[(30, 10)].symbol(), "▶");
        assert_eq!(buffer[(37, 10)].symbol(), "Q");
        assert_eq!(buffer[(148, 6)].bg, Color::Rgb(19, 19, 20));
        assert_eq!(buffer[(150, 29)].symbol(), "T");
        assert_eq!(buffer[(150, 31)].symbol(), "e");
        // Labels follow the longest key ("C-space") plus a two-cell gap.
        assert_eq!(buffer[(159, 31)].symbol(), "P");
        assert_eq!(buffer[(148, 28)].bg, Color::Rgb(26, 26, 28));
        assert_eq!(buffer[(45, 14)].bg, Color::Rgb(245, 177, 131));
        assert_eq!(buffer[(0, 3)].bg, Color::Rgb(19, 19, 20));
        assert_eq!(buffer[(28, 3)].bg, Color::Rgb(11, 11, 12));
        assert_eq!(buffer[(2, 39)].bg, Color::Rgb(25, 25, 27));
        assert_eq!(buffer[(0, 38)].fg, Color::Rgb(48, 48, 52));
        assert_eq!(buffer[(0, 2)].symbol(), "─");
        assert_eq!(buffer[(175, 42)].symbol(), "%");
        assert_eq!(buffer[(176, 42)].symbol(), " ");
        assert_eq!(buffer[(5, 33)].symbol(), "B");
        assert_eq!(buffer[(5, 34)].symbol(), "@");
        assert_eq!(buffer[(5, 35)].symbol(), "P");

        let rendered = terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .map(|cell| cell.symbol())
            .collect::<String>();
        assert!(rendered.contains("Up next"));
        assert!(rendered.contains("Queue item 1"));
        assert!(rendered.contains("Playback"));
        assert!(rendered.contains("Track actions"));
    }

    #[test]
    fn collections_below_the_canonical_size_use_the_workspace_table() {
        let configs = super::initialize_test_config();
        let ui_ring = Arc::new(Mutex::new(VecDeque::<UiDiagnosticEntry>::new()));
        let (diagnostics, _runtime) = crate::observability::disabled(ui_ring);
        let state: SharedState = Arc::new(State::new_with_configs(false, diagnostics, configs));
        let tracks = (1..=40)
            .map(|index| crate::state::YouTubeTrack {
                id: format!("compact-{index}"),
                name: format!("Compact item {index}"),
                artists: "Compact artist".to_owned(),
                album: None,
                duration: "3:00".to_owned(),
                explicit: false,
                thumbnail_url: None,
                is_video: false,
            })
            .collect::<Vec<_>>();

        for (columns, rows, stride) in [(80, 24, 1), (100, 30, 1), (60, 20, 1), (120, 40, 2)] {
            let mut terminal = Terminal::new(TestBackend::new(columns, rows)).unwrap();
            let mut ui = state.ui.lock();
            ui.active_provider = config::ActiveProvider::YouTubeMusic;
            ui.workspace_focus = crate::state::WorkspaceFocusState::Context;
            ui.history.clear();
            ui.history.push(PageState::YouTubeContext {
                id: crate::state::YouTubeContextId::LikedTracks,
                context: Some(crate::state::YouTubeContext {
                    title: "Compact context".to_owned(),
                    description: None,
                    tracks: tracks.clone(),
                    playlist_set_video_ids: Vec::new(),
                    artist: None,
                }),
                state: crate::state::YouTubeContextPageUIState::new(),
            });
            terminal
                .draw(|frame| render_application(frame, &state, &mut ui, frame.area()))
                .unwrap();

            let rendered = terminal
                .backend()
                .buffer()
                .content()
                .iter()
                .map(ratatui::buffer::Cell::symbol)
                .collect::<String>();
            assert!(
                rendered.contains("40 tracks shown"),
                "{columns}x{rows} fell back to the legacy context renderer"
            );
            assert!(rendered.contains("Compact context"));
            let content = ui.workspace_layout.content;
            let row_rects = (0..2)
                .map(|index| {
                    ui.workspace_hit_rect(crate::state::WorkspaceHit::ContextRow(index))
                        .unwrap_or_else(|| panic!("{columns}x{rows} has no row {index} hit"))
                })
                .collect::<Vec<_>>();
            assert_eq!(row_rects[1].y - row_rects[0].y, stride, "{columns}x{rows}");
            for row in row_rects {
                assert_eq!(row.intersection(content), row, "{columns}x{rows}");
            }
        }
    }

    #[test]
    fn design_v1_queue_page_uses_shared_navigation_and_row_hits() {
        let configs = super::initialize_test_config();
        let ui_ring = Arc::new(Mutex::new(VecDeque::<UiDiagnosticEntry>::new()));
        let (diagnostics, _runtime) = crate::observability::disabled(ui_ring);
        let state: SharedState = Arc::new(State::new_with_configs(false, diagnostics, configs));
        let tracks = (1..=3)
            .map(|index| crate::state::YouTubeTrack {
                id: format!("queue-page-{index}"),
                name: format!("Queue page item {index}"),
                artists: "Queue page artist".to_owned(),
                album: None,
                duration: "3:00".to_owned(),
                explicit: false,
                thumbnail_url: None,
                is_video: false,
            })
            .collect::<Vec<_>>();
        {
            let mut player = state.player.write();
            player.active_playback_provider = Some(config::ActiveProvider::YouTubeMusic);
            player.unified_queue = Some(crate::state::UnifiedQueue::new(
                tracks
                    .iter()
                    .cloned()
                    .map(crate::state::PlayableMedia::YouTube)
                    .collect(),
                0,
            ));
        }

        let mut terminal = Terminal::new(TestBackend::new(180, 49)).unwrap();
        {
            let mut ui = state.ui.lock();
            ui.history.clear();
            ui.history.push(PageState::new_queue());
            ui.workspace_navigation = crate::state::WorkspaceNavigationItem::Queue;
            ui.layout_mode = crate::ui::LayoutMode::Wide;
            ui.orientation = crate::ui::Orientation::Horizontal;
            terminal
                .draw(|frame| render_application(frame, &state, &mut ui, frame.area()))
                .unwrap();

            assert_eq!(ui.workspace_layout.navigation, Rect::new(0, 3, 26, 35));
            assert_eq!(ui.workspace_layout.content, Rect::new(27, 3, 153, 35));
            assert_eq!(
                ui.workspace_hit_at(30, 10),
                Some(crate::state::WorkspaceHit::QueueRow(0))
            );
        }

        let rendered = terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .map(|cell| cell.symbol())
            .collect::<String>();
        assert!(rendered.contains("Queue"));
        assert!(rendered.contains("Queue page item 1"));
        assert!(rendered.contains("Playlists"));
    }

    #[test]
    fn design_v1_journal_pages_use_shared_shell_and_row_hits() {
        let configs = super::initialize_test_config();
        let ui_ring = Arc::new(Mutex::new(VecDeque::<UiDiagnosticEntry>::new()));
        let (diagnostics, _runtime) = crate::observability::disabled(ui_ring);
        let state: SharedState = Arc::new(State::new_with_configs(false, diagnostics, configs));
        let track = crate::state::Track {
            id: crate::state::TrackId::from_id("4iV5W9uYEdYUVa79Axb7Rh")
                .unwrap()
                .into_static(),
            name: "Journal item".to_owned(),
            artists: Vec::new(),
            album: None,
            duration: std::time::Duration::from_secs(180),
            explicit: false,
            added_at: 0,
        };
        let list_id = {
            let mut data = state.data.write();
            data.journal.set_note(track.clone(), "Note".to_owned());
            let list_id = data.journal.create_list("Favorites".to_owned());
            data.journal.add_tracks_to_list(&list_id, [track]);
            data.session_history
                .entries
                .push(crate::state::SessionEntry {
                    media_id: crate::state::MediaId {
                        provider: crate::state::Provider::Spotify,
                        kind: crate::state::MediaKind::Track,
                        raw_id: "4iV5W9uYEdYUVa79Axb7Rh".to_owned(),
                    },
                    title: "Journal item".to_owned(),
                    artists: "".to_owned(),
                    album: None,
                    duration_ms: Some(180_000),
                    started_at: 1,
                });
            list_id
        };
        let mut terminal = Terminal::new(TestBackend::new(180, 49)).unwrap();
        let mut ui = state.ui.lock();
        ui.layout_mode = crate::ui::LayoutMode::Wide;
        ui.orientation = crate::ui::Orientation::Horizontal;

        ui.history.clear();
        ui.history.push(PageState::Journal {
            table: ratatui::widgets::TableState::default(),
            journal_selection: crate::state::JournalSelection::default(),
        });
        terminal
            .draw(|frame| render_application(frame, &state, &mut ui, frame.area()))
            .unwrap();
        assert_eq!(ui.workspace_layout.content, Rect::new(27, 3, 153, 35));
        assert_eq!(
            ui.workspace_hit_at(30, 6),
            Some(crate::state::WorkspaceHit::JournalRow(0))
        );

        ui.history.clear();
        ui.history.push(PageState::JournalLists {
            list: ratatui::widgets::ListState::default(),
        });
        terminal
            .draw(|frame| render_application(frame, &state, &mut ui, frame.area()))
            .unwrap();
        assert_eq!(
            ui.workspace_hit_at(30, 5),
            Some(crate::state::WorkspaceHit::JournalRow(0))
        );

        ui.history.clear();
        ui.history.push(PageState::JournalList {
            list_id,
            table: ratatui::widgets::TableState::default(),
            journal_selection: crate::state::JournalSelection::default(),
        });
        terminal
            .draw(|frame| render_application(frame, &state, &mut ui, frame.area()))
            .unwrap();
        assert_eq!(
            ui.workspace_hit_at(30, 6),
            Some(crate::state::WorkspaceHit::JournalRow(0))
        );
        let rendered = terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .map(|cell| cell.symbol())
            .collect::<String>();
        assert!(rendered.contains("Journal List"));
        assert!(rendered.contains("Journal item"));

        ui.history.clear();
        ui.history.push(PageState::SessionHistory {
            list: ratatui::widgets::ListState::default(),
        });
        terminal
            .draw(|frame| render_application(frame, &state, &mut ui, frame.area()))
            .unwrap();
        assert_eq!(
            ui.workspace_hit_at(30, 5),
            Some(crate::state::WorkspaceHit::SessionHistoryRow(0))
        );
        let rendered = terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .map(|cell| cell.symbol())
            .collect::<String>();
        assert!(rendered.contains("Session History"));
        assert!(rendered.contains("Journal item"));
    }

    #[test]
    fn design_v1_workspace_adapts_the_shared_frame_at_documented_sizes() {
        let configs = super::initialize_test_config();
        let ui_ring = Arc::new(Mutex::new(VecDeque::<UiDiagnosticEntry>::new()));
        let (diagnostics, _runtime) = crate::observability::disabled(ui_ring);
        let state: SharedState = Arc::new(State::new_with_configs(false, diagnostics, configs));
        {
            let mut ui = state.ui.lock();
            ui.history.clear();
            ui.history.push(PageState::Library {
                state: crate::state::LibraryPageUIState::new(),
            });
        }

        for (columns, rows) in [
            (40, 16),
            (60, 20),
            (80, 24),
            (120, 30),
            (120, 40),
            (180, 49),
        ] {
            let mut terminal = Terminal::new(TestBackend::new(columns, rows)).unwrap();
            let mut ui = state.ui.lock();
            terminal
                .draw(|frame| render_application(frame, &state, &mut ui, frame.area()))
                .unwrap();
            let expected = LayoutPolicy::from_size(columns, rows)
                .workspace_frame(Rect::new(0, 0, columns, rows));
            assert_eq!(ui.playback_window_rect, expected.transport);
            let close_rect = ui
                .workspace_hit_rect(crate::state::WorkspaceHit::CloseWindow)
                .expect("workspace header exposes close control");
            assert!(close_rect.width > 0);
            assert!(close_rect.intersection(expected.header) == close_rect);
            let help_rect = ui
                .workspace_hit_rect(crate::state::WorkspaceHit::Help)
                .expect("workspace footer exposes help control");
            assert!(help_rect.width > 0);
            assert!(help_rect.intersection(expected.footer) == help_rect);
            assert!(ui.workspace_layout.content.width <= columns);
            if (columns, rows) == (120, 30) {
                let rendered = terminal
                    .backend()
                    .buffer()
                    .content()
                    .iter()
                    .map(|cell| cell.symbol())
                    .collect::<String>();
                // The canonical rail is narrow enough to drop the chevrons,
                // but it keeps the verbs and full provider names.
                for label in ["Browse Spotify", "@", "Play "] {
                    assert!(
                        rendered.contains(label),
                        "missing {label:?} in normal Library frame"
                    );
                }
            }
        }
    }

    #[test]
    fn design_v1_workspace_uses_bottom_chrome_and_transport_hits() {
        let configs = super::initialize_test_config();
        let ui_ring = Arc::new(Mutex::new(VecDeque::<UiDiagnosticEntry>::new()));
        let (diagnostics, _runtime) = crate::observability::disabled(ui_ring);
        let state: SharedState = Arc::new(State::new_with_configs(false, diagnostics, configs));
        {
            let mut player = state.player.write();
            player.active_playback_provider = Some(config::ActiveProvider::YouTubeMusic);
            player.youtube_playback = Some(crate::state::YouTubePlayback {
                track: crate::state::YouTubeTrack {
                    id: "transport-track".to_owned(),
                    name: "Transport track".to_owned(),
                    artists: "Transport artist".to_owned(),
                    album: Some("Transport album".to_owned()),
                    duration: "4:29".to_owned(),
                    explicit: false,
                    thumbnail_url: None,
                    is_video: false,
                },
                is_playing: true,
                progress: std::time::Duration::from_secs(34),
                volume: 70,
                mute_state: None,
                route: Default::default(),
            });
            player.unified_queue = Some(crate::state::UnifiedQueue::new(
                vec![crate::state::PlayableMedia::YouTube(
                    crate::state::YouTubeTrack {
                        id: "queue-control-track".to_owned(),
                        name: "Queue control track".to_owned(),
                        artists: "Queue control artist".to_owned(),
                        album: None,
                        duration: "4:29".to_owned(),
                        explicit: false,
                        thumbnail_url: None,
                        is_video: false,
                    },
                )],
                0,
            ));
        }
        let mut terminal = Terminal::new(TestBackend::new(180, 49)).unwrap();
        {
            let mut ui = state.ui.lock();
            ui.active_provider = config::ActiveProvider::YouTubeMusic;
            ui.youtube_account_label = Some("Account 2".to_owned());
            ui.history.clear();
            ui.history.push(PageState::Library {
                state: crate::state::LibraryPageUIState::new(),
            });
            terminal
                .draw(|frame| render_application(frame, &state, &mut ui, frame.area()))
                .unwrap();

            assert_eq!(ui.playback_window_rect, Rect::new(0, 39, 180, 7));
            assert_eq!(ui.playback_toggle_rect, Rect::new(4, 44, 2, 1));
            assert_eq!(ui.playback_progress_bar_rect.y, 44);
            assert!(ui.playback_progress_bar_rect.width > 72);
            for option in [
                crate::state::WorkspacePlaybackOption::Shuffle,
                crate::state::WorkspacePlaybackOption::Repeat,
                crate::state::WorkspacePlaybackOption::Volume,
            ] {
                let option_rect = ui
                    .workspace_hit_rect(crate::state::WorkspaceHit::PlaybackOption(option))
                    .expect("workspace playback option exposes a mouse target");
                assert!(option_rect.intersection(ui.playback_window_rect) == option_rect);
            }
        }

        let rendered = terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .map(|cell| cell.symbol())
            .collect::<String>();
        assert!(rendered.contains("Unified Player"));
        assert!(rendered.contains("backspace Back"));
        assert!(rendered.contains("Browsing YouTube Music / Account 2"));
        assert!(rendered.contains("Volume ["));
        assert!(rendered.contains("Playing"));
        assert!(rendered.contains("Transport track"));
        assert!(rendered.contains("Play/pause"));
    }

    #[test]
    fn duplicate_unified_occurrences_render_independent_selection_in_normal_and_narrow_layouts() {
        let configs = super::initialize_test_config();
        let ui_ring = Arc::new(Mutex::new(VecDeque::<UiDiagnosticEntry>::new()));
        let (diagnostics, _runtime) = crate::observability::disabled(ui_ring);
        let state: SharedState = Arc::new(State::new_with_configs(false, diagnostics, configs));
        let media_id = MediaId {
            provider: Provider::Spotify,
            kind: MediaKind::Track,
            raw_id: "duplicate".to_owned(),
        };
        let first = UnifiedPlaylistItem {
            entry_id: PlaylistEntryId(1),
            media_id: media_id.clone(),
            title: "First occurrence".to_owned(),
            artists: "Artist".to_owned(),
            ..UnifiedPlaylistItem::default()
        };
        let second = UnifiedPlaylistItem {
            entry_id: PlaylistEntryId(2),
            media_id,
            title: "Second occurrence".to_owned(),
            artists: "Artist".to_owned(),
            ..UnifiedPlaylistItem::default()
        };
        state.data.write().unified_playlists.push(UnifiedPlaylist {
            id: "duplicates".to_owned(),
            name: "Duplicates".to_owned(),
            items: vec![first.clone(), second.clone()],
            updated_at: 1,
            next_entry_id: 3,
        });

        {
            let mut ui = state.ui.lock();
            ui.history.clear();
            ui.history
                .push(PageState::new_unified_playlist("duplicates"));
            let entries = [
                (first.media_id.clone(), first.entry_id),
                (second.media_id.clone(), second.entry_id),
            ];
            let selection = ui
                .current_page_mut()
                .unified_playlist_selection_mut()
                .unwrap();
            synchronize_unified_playlist_entries(selection, "duplicates", entries.clone(), entries)
                .unwrap();
            selection.extend_range(1, 1).unwrap();
            ui.current_page_mut().select(1);
            ui.current_page_mut()
                .set_unified_playlist_cursor_entry_id(Some(second.entry_id));
        }

        for (iteration, (columns, rows)) in [(80, 20), (30, 12)].into_iter().enumerate() {
            if iteration == 1 {
                state.data.write().unified_playlists[0].items.swap(0, 1);
            }
            let mut terminal = Terminal::new(TestBackend::new(columns, rows)).unwrap();
            let mut ui = state.ui.lock();
            let policy = LayoutPolicy::from_size(columns, rows);
            ui.orientation = policy.orientation;
            ui.layout_mode = policy.mode;
            if iteration == 1 {
                if let PageState::UnifiedPlaylist { playlist_state, .. } = ui.current_page_mut() {
                    *playlist_state.table_mut().offset_mut() = usize::MAX;
                }
            }
            terminal
                .draw(|frame| {
                    super::page::render_unified_playlist_page(
                        true,
                        frame,
                        &state,
                        &mut ui,
                        frame.area(),
                    )
                })
                .unwrap();
            assert!(ui
                .workspace_hit_rect(crate::state::WorkspaceHit::UnifiedPlaylistRow(
                    1 - iteration,
                ))
                .is_some());
            let rendered_rows = (0..rows)
                .map(|y| {
                    (0..columns)
                        .map(|x| terminal.backend().buffer()[(x, y)].symbol())
                        .collect::<String>()
                })
                .collect::<Vec<_>>();
            let first_row = rendered_rows
                .iter()
                .find(|line| line.contains("First"))
                .expect("first duplicate row remains visible");
            let second_row = rendered_rows
                .iter()
                .find(|line| line.contains("Second"))
                .expect("second duplicate row remains visible");
            assert!(!first_row.contains('>'));
            assert!(second_row.contains('>'));
            assert_eq!(
                ui.current_page().unified_playlist_cursor_entry_id(),
                Some(second.entry_id)
            );
            assert_eq!(ui.current_page().selected_index(), Some(1 - iteration));
            if let PageState::UnifiedPlaylist { playlist_state, .. } = ui.current_page() {
                assert_eq!(playlist_state.table().offset(), 0);
            }
        }
    }

    #[test]
    fn diagnostics_renderer_exposes_workspace_panels_on_compact_terminal() {
        let configs = super::initialize_test_config();

        let ui_ring = Arc::new(Mutex::new(VecDeque::<UiDiagnosticEntry>::new()));
        let (diagnostics, _runtime) = crate::observability::disabled(ui_ring);
        let state: SharedState = Arc::new(State::new_with_configs(false, diagnostics, configs));
        let mut terminal = Terminal::new(TestBackend::new(80, 24)).unwrap();
        {
            let mut ui = state.ui.lock();
            ui.history.clear();
            ui.history.push(PageState::Logs {
                state: DiagnosticsPageUIState::new(),
            });
            terminal
                .draw(|frame| render_application(frame, &state, &mut ui, frame.area()))
                .unwrap();
            assert!(ui
                .workspace_hit_rect(crate::state::WorkspaceHit::DiagnosticRow(0))
                .is_some());
        }

        let rendered = terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .map(|cell| cell.symbol())
            .collect::<String>();
        assert!(rendered.contains("Diagnostics"));
        assert!(rendered.contains("Overview"));
        assert!(rendered.contains("Inspector"));
        assert!(!rendered.contains("Up/Down Navigate"));
        assert!(rendered.contains(" | "));
    }

    #[test]
    fn youtube_context_statuses_do_not_hide_or_mislabel_cached_rows() {
        let configs = super::initialize_test_config();

        let ui_ring = Arc::new(Mutex::new(VecDeque::<UiDiagnosticEntry>::new()));
        let (diagnostics, _runtime) = crate::observability::disabled(ui_ring);
        let state: SharedState = Arc::new(State::new_with_configs(false, diagnostics, configs));
        let mut terminal = Terminal::new(TestBackend::new(80, 24)).unwrap();
        {
            let mut ui = state.ui.lock();
            ui.history.clear();
            let mut page_state = crate::state::YouTubeContextPageUIState::new();
            page_state.status = UiViewStatus::Failed {
                code: crate::state::YOUTUBE_CONTEXT_ERROR_CODE,
                message: crate::state::YOUTUBE_CONTEXT_ERROR_MESSAGE,
                next_action: crate::state::YOUTUBE_CONTEXT_ERROR_NEXT_ACTION,
            };
            ui.history.push(PageState::YouTubeContext {
                id: crate::state::YouTubeContextId::LikedTracks,
                context: Some(crate::state::YouTubeContext {
                    title: "Cached only".to_owned(),
                    description: None,
                    tracks: vec![crate::state::YouTubeTrack {
                        id: "cached-id".to_owned(),
                        name: "Cached only".to_owned(),
                        artists: "Cached artist".to_owned(),
                        album: None,
                        duration: "1:00".to_owned(),
                        explicit: false,
                        thumbnail_url: None,
                        is_video: false,
                    }],
                    playlist_set_video_ids: Vec::new(),
                    artist: None,
                }),
                state: page_state,
            });
            terminal
                .draw(|frame| render_application(frame, &state, &mut ui, frame.area()))
                .unwrap();
        }

        let failed_rendered = terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .map(|cell| cell.symbol())
            .collect::<String>();
        assert!(failed_rendered.contains("Unable to load this context."));
        assert!(failed_rendered.contains("another item."));
        assert!(!failed_rendered.contains("Cached only"));

        {
            let mut ui = state.ui.lock();
            if let PageState::YouTubeContext { state, .. } = ui.current_page_mut() {
                state.status = UiViewStatus::Partial {
                    code: "PARTIAL_RESULTS",
                    message: "Some results could not be loaded.",
                    next_action: "Open Diagnostics or retry the request.",
                };
            }
            terminal
                .draw(|frame| render_application(frame, &state, &mut ui, frame.area()))
                .unwrap();
        }
        let partial_rendered = terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .map(|cell| cell.symbol())
            .collect::<String>();
        assert!(partial_rendered.contains("Some results could not be loaded."));
        assert!(partial_rendered.contains("Cached only"));
        assert!(!partial_rendered.contains("YouTube Album"));
    }

    #[test]
    fn failed_spotify_context_uses_shared_status_surface() {
        let configs = super::initialize_test_config();

        let ui_ring = Arc::new(Mutex::new(VecDeque::<UiDiagnosticEntry>::new()));
        let (diagnostics, _runtime) = crate::observability::disabled(ui_ring);
        let state: SharedState = Arc::new(State::new_with_configs(false, diagnostics, configs));
        let mut terminal = Terminal::new(TestBackend::new(80, 24)).unwrap();
        {
            let mut ui = state.ui.lock();
            ui.history.clear();
            ui.history.push(PageState::Context {
                id: Some(crate::state::ContextId::Tracks(crate::state::TracksId {
                    uri: "spotify:top:tracks".to_owned(),
                    kind: "Top tracks".to_owned(),
                })),
                context_page_type: crate::state::ContextPageType::CurrentPlaying,
                state: Some(crate::state::ContextPageUIState::Failed {
                    status: UiViewStatus::Failed {
                        code: crate::state::CONTEXT_ERROR_CODE,
                        message: crate::state::CONTEXT_ERROR_MESSAGE,
                        next_action: crate::state::CONTEXT_ERROR_NEXT_ACTION,
                    },
                }),
            });
            terminal
                .draw(|frame| render_application(frame, &state, &mut ui, frame.area()))
                .unwrap();
        }

        let rendered = terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .map(|cell| cell.symbol())
            .collect::<String>();
        assert!(rendered.contains("Unable to load this Spotify context."));
    }

    #[test]
    fn lyrics_loading_status_uses_the_full_content_panel() {
        let configs = super::initialize_test_config();

        let ui_ring = Arc::new(Mutex::new(VecDeque::<UiDiagnosticEntry>::new()));
        let (diagnostics, _runtime) = crate::observability::disabled(ui_ring);
        let state: SharedState = Arc::new(State::new_with_configs(false, diagnostics, configs));
        let mut terminal = Terminal::new(TestBackend::new(60, 20)).unwrap();
        {
            let mut ui = state.ui.lock();
            ui.history.clear();
            ui.history.push(PageState::Lyrics {
                provider: crate::config::ActiveProvider::Spotify,
                track_uri: "spotify:track:lyrics".to_owned(),
                track: "Track".to_owned(),
                artists: "Artist".to_owned(),
                youtube_track: None,
                lyrics_provider: None,
                scroll_offset: 0,
                follow_playback: true,
                status: UiViewStatus::Loading,
            });
            terminal
                .draw(|frame| render_application(frame, &state, &mut ui, frame.area()))
                .unwrap();
        }

        let buffer = terminal.backend().buffer();
        let loading_row = (0..buffer.area.height).find(|row| {
            (0..buffer.area.width)
                .map(|column| buffer[(column, *row)].symbol())
                .collect::<String>()
                .contains("Loading...")
        });
        // The compact workspace keeps the loading status at the top of the
        // content surface immediately below the shared transport chrome.
        assert_eq!(loading_row, Some(3));
    }

    #[test]
    fn design_v1_lyrics_uses_the_shared_workspace_shell() {
        let configs = super::initialize_test_config();
        let ui_ring = Arc::new(Mutex::new(VecDeque::<UiDiagnosticEntry>::new()));
        let (diagnostics, _runtime) = crate::observability::disabled(ui_ring);
        let state: SharedState = Arc::new(State::new_with_configs(false, diagnostics, configs));
        let mut terminal = Terminal::new(TestBackend::new(180, 49)).unwrap();
        let mut ui = state.ui.lock();
        ui.history.clear();
        ui.history.push(PageState::Lyrics {
            provider: crate::config::ActiveProvider::Spotify,
            track_uri: "spotify:track:lyrics".to_owned(),
            track: "Track".to_owned(),
            artists: "Artist".to_owned(),
            youtube_track: None,
            lyrics_provider: None,
            scroll_offset: 0,
            follow_playback: true,
            status: UiViewStatus::Loading,
        });
        terminal
            .draw(|frame| render_application(frame, &state, &mut ui, frame.area()))
            .unwrap();

        assert_eq!(ui.workspace_layout.content, Rect::new(27, 3, 153, 35));
        let rendered = terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .map(|cell| cell.symbol())
            .collect::<String>();
        assert!(rendered.contains("Lyrics: Track · Artist"));
        assert!(rendered.contains("Loading..."));
        assert!(rendered.contains("Playlists"));
    }

    #[test]
    fn browse_renderer_explains_unavailable_categories_without_loading_forever() {
        let configs = super::initialize_test_config();
        let ui_ring = Arc::new(Mutex::new(VecDeque::<UiDiagnosticEntry>::new()));
        let (diagnostics, _runtime) = crate::observability::disabled(ui_ring);
        let state: SharedState = Arc::new(State::new_with_configs(false, diagnostics, configs));
        for (width, height) in [(80, 24), (120, 35), (180, 49), (140, 40)] {
            for loaded in [false, true] {
                state.data.write().browse.categories_loaded = loaded;
                let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
                let mut ui = state.ui.lock();
                ui.history.clear();
                ui.history.push(PageState::Browse {
                    state: crate::state::BrowsePageUIState::CategoryList {
                        state: ratatui::widgets::ListState::default(),
                    },
                });
                terminal
                    .draw(|frame| render_application(frame, &state, &mut ui, frame.area()))
                    .unwrap();
                let rendered = terminal
                    .backend()
                    .buffer()
                    .content()
                    .iter()
                    .map(ratatui::buffer::Cell::symbol)
                    .collect::<String>();
                assert!(rendered.contains("Categories"));
                assert!(rendered.contains("Spotify category browsing is unavailable."));
                let content = ui.workspace_layout.content;
                let message = (content.y..content.bottom())
                    .map(|y| {
                        (content.x..content.right())
                            .map(|x| terminal.backend().buffer()[(x, y)].symbol())
                            .collect::<String>()
                    })
                    .collect::<Vec<_>>()
                    .join("\n");
                assert!(
                    message
                        .split_whitespace()
                        .collect::<Vec<_>>()
                        .join(" ")
                        .contains("Use Search, Home"),
                    "{width}x{height}: {message}"
                );
                assert!(!rendered.contains("Loading..."));
            }
        }
    }

    #[test]
    fn design_v1_browse_uses_the_shared_workspace_shell_and_row_hits() {
        let configs = super::initialize_test_config();
        let ui_ring = Arc::new(Mutex::new(VecDeque::<UiDiagnosticEntry>::new()));
        let (diagnostics, _runtime) = crate::observability::disabled(ui_ring);
        let state: SharedState = Arc::new(State::new_with_configs(false, diagnostics, configs));
        state.data.write().browse.categories_loaded = true;
        state.data.write().browse.categories = vec![crate::state::Category {
            id: "mood".to_owned(),
            name: "Mood".to_owned(),
        }];
        let mut terminal = Terminal::new(TestBackend::new(80, 24)).unwrap();
        {
            let mut ui = state.ui.lock();
            ui.history.clear();
            ui.history.push(PageState::Browse {
                state: crate::state::BrowsePageUIState::CategoryList {
                    state: ratatui::widgets::ListState::default().with_selected(Some(0)),
                },
            });
            terminal
                .draw(|frame| render_application(frame, &state, &mut ui, frame.area()))
                .unwrap();
            assert!(ui
                .workspace_hit_rect(crate::state::WorkspaceHit::BrowseRow(0))
                .is_some());
        }

        let rendered = terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .map(|cell| cell.symbol())
            .collect::<String>();
        assert!(rendered.contains("Browse"));
        assert!(rendered.contains("Categories"));
        assert!(rendered.contains("Mood"));
    }

    #[test]
    fn empty_spotify_library_panels_use_shared_status_surface() {
        let configs = super::initialize_test_config();

        let ui_ring = Arc::new(Mutex::new(VecDeque::<UiDiagnosticEntry>::new()));
        let (diagnostics, _runtime) = crate::observability::disabled(ui_ring);
        let state: SharedState = Arc::new(State::new_with_configs(false, diagnostics, configs));
        // Compact fixtures intentionally render only the focused collection.
        // Use the normal-width fixture here because this test verifies the
        // shared empty surface in all three panels.
        let mut terminal = Terminal::new(TestBackend::new(80, 30)).unwrap();
        {
            let mut ui = state.ui.lock();
            ui.history.clear();
            ui.history.push(PageState::Library {
                state: crate::state::LibraryPageUIState::new(),
            });
            let policy = LayoutPolicy::from_size(80, 30);
            ui.layout_mode = policy.mode;
            ui.orientation = policy.orientation;
            terminal
                .draw(|frame| render_application(frame, &state, &mut ui, frame.area()))
                .unwrap();
        }

        let rendered = terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .map(|cell| cell.symbol())
            .collect::<String>();
        assert!(rendered.contains("Playlists"));
        assert!(rendered.contains("Albums"));
        assert!(rendered.contains("Artists"));
        assert!(rendered.contains("No items"));
    }

    #[test]
    fn compact_library_renders_only_the_focused_collection_panel() {
        let configs = super::initialize_test_config();

        let ui_ring = Arc::new(Mutex::new(VecDeque::<UiDiagnosticEntry>::new()));
        let (diagnostics, _runtime) = crate::observability::disabled(ui_ring);
        let state: SharedState = Arc::new(State::new_with_configs(false, diagnostics, configs));
        let mut terminal = Terminal::new(TestBackend::new(60, 20)).unwrap();
        {
            let mut ui = state.ui.lock();
            ui.history.clear();
            ui.history.push(PageState::Library {
                state: crate::state::LibraryPageUIState::new(),
            });
            ui.layout_mode = crate::ui::LayoutMode::Compact;
            ui.orientation = crate::ui::Orientation::Vertical;
            terminal
                .draw(|frame| render_application(frame, &state, &mut ui, frame.area()))
                .unwrap();
        }

        let rendered = terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .map(|cell| cell.symbol())
            .collect::<String>();
        assert!(rendered.contains("Playlists"));
        drop(state);
    }

    #[test]
    fn empty_queue_and_journal_use_bounded_surfaces() {
        let configs = super::initialize_test_config();

        let ui_ring = Arc::new(Mutex::new(VecDeque::<UiDiagnosticEntry>::new()));
        let (diagnostics, _runtime) = crate::observability::disabled(ui_ring);
        let state: SharedState = Arc::new(State::new_with_configs(false, diagnostics, configs));
        let mut terminal = Terminal::new(TestBackend::new(60, 20)).unwrap();
        {
            let mut ui = state.ui.lock();
            ui.history.clear();
            ui.history.push(crate::state::PageState::new_queue());
            ui.orientation = crate::ui::Orientation::Vertical;
            ui.layout_mode = crate::ui::LayoutMode::Compact;
            terminal
                .draw(|frame| render_application(frame, &state, &mut ui, frame.area()))
                .unwrap();
        }

        let rendered = terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .map(|cell| cell.symbol())
            .collect::<String>();
        assert!(rendered.contains("Queue"));
        assert!(rendered.contains("No items were found."));

        let mut journal_terminal = Terminal::new(TestBackend::new(60, 20)).unwrap();
        {
            let mut ui = state.ui.lock();
            ui.history.clear();
            ui.history.push(crate::state::PageState::Journal {
                table: ratatui::widgets::TableState::default(),
                journal_selection: crate::state::JournalSelection::default(),
            });
            journal_terminal
                .draw(|frame| render_application(frame, &state, &mut ui, frame.area()))
                .unwrap();
        }
        let journal_rendered = journal_terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .map(|cell| cell.symbol())
            .collect::<String>();
        assert!(journal_rendered.contains("Journal"));
        assert!(journal_rendered.contains("No items were found."));

        let mut journal_lists_terminal = Terminal::new(TestBackend::new(60, 20)).unwrap();
        {
            let mut ui = state.ui.lock();
            ui.history.clear();
            ui.history.push(crate::state::PageState::JournalLists {
                list: ratatui::widgets::ListState::default(),
            });
            journal_lists_terminal
                .draw(|frame| render_application(frame, &state, &mut ui, frame.area()))
                .unwrap();
        }
        let journal_lists_rendered = journal_lists_terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .map(|cell| cell.symbol())
            .collect::<String>();
        assert!(journal_lists_rendered.contains("Journal Lists"));
        assert!(journal_lists_rendered.contains("No items were found."));

        state
            .data
            .write()
            .upsert_unified_playlist(crate::state::UnifiedPlaylist {
                id: "empty-playlist".to_owned(),
                name: "Empty playlist".to_owned(),
                items: Vec::new(),
                updated_at: 0,
                next_entry_id: 1,
            })
            .unwrap();
        let mut unified_terminal = Terminal::new(TestBackend::new(60, 20)).unwrap();
        {
            let mut ui = state.ui.lock();
            ui.history.clear();
            ui.history
                .push(crate::state::PageState::new_unified_playlist(
                    "empty-playlist".to_owned(),
                ));
            unified_terminal
                .draw(|frame| render_application(frame, &state, &mut ui, frame.area()))
                .unwrap();
        }
        let unified_rendered = unified_terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .map(|cell| cell.symbol())
            .collect::<String>();
        assert!(unified_rendered.contains("Empty playlist"));
        assert!(unified_rendered.contains("ListenBrainz: disabled"));
        assert!(unified_rendered.contains("Enable in"));
        assert!(unified_rendered.contains("manifest-only 0"));
        assert!(unified_rendered.contains("No items were found."));

        let mut missing_unified_terminal = Terminal::new(TestBackend::new(60, 20)).unwrap();
        {
            let mut ui = state.ui.lock();
            ui.history.clear();
            ui.history
                .push(crate::state::PageState::new_unified_playlist(
                    "missing-playlist".to_owned(),
                ));
            missing_unified_terminal
                .draw(|frame| render_application(frame, &state, &mut ui, frame.area()))
                .unwrap();
        }
        let missing_unified_rendered = missing_unified_terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .map(|cell| cell.symbol())
            .collect::<String>();
        assert!(missing_unified_rendered.contains("Unified playlist is no longer"));
        assert!(missing_unified_rendered.contains("Next: Return"));
        assert!(missing_unified_rendered.contains("choose another playlist"));

        drop(state);
    }

    #[test]
    fn compact_command_help_keeps_viewport_stable_while_highlighting_selection() {
        let configs = super::initialize_test_config();

        let ui_ring = Arc::new(Mutex::new(VecDeque::<UiDiagnosticEntry>::new()));
        let (diagnostics, _runtime) = crate::observability::disabled(ui_ring);
        let state: SharedState = Arc::new(State::new_with_configs(false, diagnostics, configs));
        let mut terminal = Terminal::new(TestBackend::new(60, 20)).unwrap();
        {
            let mut ui = state.ui.lock();
            ui.history.clear();
            ui.history.push(PageState::CommandHelp { scroll_offset: 4 });
            ui.layout_mode = crate::ui::LayoutMode::Compact;
            terminal
                .draw(|frame| render_application(frame, &state, &mut ui, frame.area()))
                .unwrap();
        }

        let rendered = terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .map(|cell| cell.symbol())
            .collect::<String>();
        let first_binding = config::get_config()
            .keymap_config
            .resolved_bindings()
            .into_iter()
            .next()
            .expect("default keymap has bindings");
        assert!(rendered.contains("Commands"));
        assert!(rendered.contains("> "));
        assert!(rendered.contains(&format!(
            "{} {}",
            first_binding.key_sequence,
            crate::ui::utils::to_bidi_string(first_binding.label())
        )));
        assert!(!rendered.contains("Command:"));
        drop(state);
    }

    #[test]
    fn command_help_table_viewport_is_retained_across_redraws() {
        let configs = super::initialize_test_config();
        let ui_ring = Arc::new(Mutex::new(VecDeque::<UiDiagnosticEntry>::new()));
        let (diagnostics, _runtime) = crate::observability::disabled(ui_ring);
        let state: SharedState = Arc::new(State::new_with_configs(false, diagnostics, configs));
        let binding_count = config::get_config().keymap_config.resolved_bindings().len();
        assert!(binding_count > 10);
        let selected = binding_count - 1;
        let mut terminal = Terminal::new(TestBackend::new(120, 40)).unwrap();

        let first_offset = {
            let mut ui = state.ui.lock();
            ui.history.clear();
            ui.history.push(PageState::CommandHelp {
                scroll_offset: selected,
            });
            ui.layout_mode = crate::ui::LayoutMode::Wide;
            terminal
                .draw(|frame| render_application(frame, &state, &mut ui, frame.area()))
                .unwrap();
            assert_eq!(ui.command_help_table_state.selected(), Some(selected));
            ui.command_help_table_state.offset()
        };
        assert!(first_offset > 0);

        {
            let mut ui = state.ui.lock();
            terminal
                .draw(|frame| render_application(frame, &state, &mut ui, frame.area()))
                .unwrap();
            assert_eq!(ui.command_help_table_state.offset(), first_offset);
            assert_eq!(ui.command_help_table_state.selected(), Some(selected));
        }
    }

    #[test]
    fn workspace_command_help_is_a_bounded_popup_over_the_current_page() {
        let configs = super::initialize_test_config();
        let ui_ring = Arc::new(Mutex::new(VecDeque::<UiDiagnosticEntry>::new()));
        let (diagnostics, _runtime) = crate::observability::disabled(ui_ring);
        let state: SharedState = Arc::new(State::new_with_configs(false, diagnostics, configs));
        {
            let mut ui = state.ui.lock();
            ui.history.clear();
            ui.history.push(PageState::Library {
                state: crate::state::LibraryPageUIState::new(),
            });
            ui.popup = Some(crate::state::PopupState::CommandHelp { scroll_offset: 0 });
        }

        for (columns, rows) in [(40, 16), (80, 24)] {
            let mut terminal = Terminal::new(TestBackend::new(columns, rows)).unwrap();
            {
                let mut ui = state.ui.lock();
                terminal
                    .draw(|frame| render_application(frame, &state, &mut ui, frame.area()))
                    .unwrap();
                assert!(matches!(ui.current_page(), PageState::Library { .. }));
                assert!(matches!(
                    ui.popup,
                    Some(crate::state::PopupState::CommandHelp { .. })
                ));
                assert!(!ui.workspace_popup_hits.is_empty());
            }

            let rendered = terminal
                .backend()
                .buffer()
                .content()
                .iter()
                .map(|cell| cell.symbol())
                .collect::<String>();
            assert!(rendered.contains("Commands"));
            assert!(rendered.contains("? Commands"));
        }
        drop(state);
    }

    #[test]
    fn welcome_renderer_starts_with_a_single_focused_step() {
        let configs = super::initialize_test_config();

        let ui_ring = Arc::new(Mutex::new(VecDeque::<UiDiagnosticEntry>::new()));
        let (diagnostics, _runtime) = crate::observability::disabled(ui_ring);
        let state: SharedState = Arc::new(State::new_with_configs(false, diagnostics, configs));
        for (columns, rows) in [(60, 20), (80, 24), (120, 40)] {
            let mut terminal = Terminal::new(TestBackend::new(columns, rows)).unwrap();
            {
                let mut ui = state.ui.lock();
                ui.history.clear();
                ui.history.push(PageState::Welcome {
                    state: crate::state::WelcomePageUIState::new(),
                    from_settings: false,
                });
                terminal
                    .draw(|frame| render_application(frame, &state, &mut ui, frame.area()))
                    .unwrap();
            }

            let buffer = terminal.backend().buffer();
            let rendered = buffer
                .content()
                .iter()
                .map(|cell| cell.symbol())
                .collect::<String>();
            // Welcome is drawn inside the shared workspace shell.
            assert!(rendered.contains("Unified Player"));
            assert!(!rendered.contains("X Close"));
            assert!(rendered.contains("Choose"));
            if rows >= 32 {
                assert!(rendered.contains("First-run setup"));
            }
            assert!(rendered.contains("Step 1 of 5 · Preferences"));
            assert_eq!(
                super::welcome::focused_rows(&state.ui.lock(), buffer),
                vec![0]
            );
            if columns >= 80 {
                assert!(rendered.contains("Next: Spotify"));
            }
        }
        drop(state);
    }

    #[test]
    fn status_copy_uses_one_shared_next_action_format() {
        assert_eq!(
            view_status_message_with_next_action(
                UiViewStatus::Empty,
                Some("Try a different search.")
            ),
            "No items were found. Next: Try a different search."
        );
        assert_eq!(view_status_message(UiViewStatus::Loading), "Loading...");
    }

    #[test]
    fn view_status_height_bounds_wrapped_attention_copy() {
        let status = UiViewStatus::Partial {
            code: "PARTIAL",
            message: "Some results could not be loaded.",
            next_action: "Open Diagnostics or retry the request.",
        };
        assert_eq!(view_status_height(status, 120), 1);
        assert!(view_status_height(status, 24) > 1);
        assert!(view_status_height(status, 24) <= 3);
    }
}

#[cfg(feature = "private-capture")]
fn private_capture_global_status(
    snapshot: &crate::developer_capture::SafeOperatorSnapshot,
) -> Option<String> {
    use crate::developer_capture::SafeCaptureState;

    match snapshot.capture.state {
        SafeCaptureState::Armed => Some(format!(
            "armed {}s",
            snapshot.capture.remaining_seconds.unwrap_or_default()
        )),
        SafeCaptureState::Capturing => Some(format!("capturing {}", snapshot.capture.record_count)),
        SafeCaptureState::Finalizing => Some("finalizing".to_owned()),
        _ => None,
    }
}

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Orientation {
    Vertical,
    #[default]
    Horizontal,
}

impl Orientation {
    /// Construct screen orientation based on the terminal's size
    pub fn from_size(columns: u16, rows: u16) -> Self {
        let ratio = f64::from(columns) / f64::from(rows);

        // a larger ratio has to be used since terminal cells aren't square
        if ratio > 2.3 {
            Self::Horizontal
        } else {
            Self::Vertical
        }
    }
}

#[cfg(all(test, feature = "private-capture"))]
mod private_capture_status_tests {
    use super::private_capture_global_status;
    use crate::developer_capture::{SafeCaptureState, SafeOperatorSnapshot};

    #[test]
    fn compact_status_is_visible_only_during_requested_capture_states() {
        let mut snapshot = SafeOperatorSnapshot::default();
        assert_eq!(private_capture_global_status(&snapshot), None);

        snapshot.capture.state = SafeCaptureState::Armed;
        snapshot.capture.remaining_seconds = Some(30);
        assert_eq!(
            private_capture_global_status(&snapshot).as_deref(),
            Some("armed 30s")
        );

        snapshot.capture.state = SafeCaptureState::Capturing;
        snapshot.capture.record_count = 12;
        assert_eq!(
            private_capture_global_status(&snapshot).as_deref(),
            Some("capturing 12")
        );

        snapshot.capture.state = SafeCaptureState::Finalizing;
        assert_eq!(
            private_capture_global_status(&snapshot).as_deref(),
            Some("finalizing")
        );

        for state in [
            SafeCaptureState::Inactive,
            SafeCaptureState::ConsentRequired,
            SafeCaptureState::Claimed,
            SafeCaptureState::Ready,
            SafeCaptureState::Incomplete,
            SafeCaptureState::Expired,
            SafeCaptureState::Failed,
        ] {
            snapshot.capture.state = state;
            assert_eq!(private_capture_global_status(&snapshot), None);
        }
    }
}
