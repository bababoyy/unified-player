//! The shared Welcome renderer used by the live application and the offline demo.
//!
//! The page owns one ordered action model in `state::WelcomeStep`. This module
//! projects that model into the workspace page recipe (step rail, content,
//! help inspector) and records hit geometry for the visible controls.
use super::{
    components::{list::list_offset_for_selection, popup_surface},
    utils, Block, Borders, Cell, Constraint, Frame, Layout, Line, Paragraph, PopupState, Rect, Row,
    Span, Style, Table, UIStateGuard, WorkspaceLayoutKind, Wrap,
};
use std::borrow::Cow;

use crate::{
    config::{self, SpotifyPremiumStatus, YouTubeMusicAuthType},
    state::{
        PageState, WelcomeAction, WelcomeLayout, WelcomeOperation, WelcomeStep,
        WorkspaceFocusState, WorkspaceHit,
    },
};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum StatusTone {
    Neutral,
    Success,
    Attention,
}

#[derive(Clone, Debug)]
struct ActionRow {
    index: usize,
    action: WelcomeAction,
    label: String,
}

/// Content wider than this uses the long-form copy and status labels.
const COMPACT_CONTENT_WIDTH: u16 = 90;
/// The detailed readiness table needs room for its three status lines.
const REVIEW_TABLE_MIN_WIDTH: u16 = 42;
const REVIEW_TABLE_HEIGHT: u16 = 7;

pub(super) fn render_welcome_page(
    is_active: bool,
    frame: &mut Frame,
    ui: &mut UIStateGuard,
    rect: Rect,
) {
    let Some((step, selected, layout_choice)) = welcome_position(ui) else {
        return;
    };
    let mut hits = Vec::new();
    if !rect.is_empty() {
        frame.render_widget(Block::default().style(ui.theme.workspace_base()), rect);
        let layout = ui
            .layout_policy()
            .workspace(rect, WorkspaceLayoutKind::Setup);
        // Classic and Centered are demo-selectable variants without the rail.
        let show_rail = layout.show_navigation && layout_choice == WelcomeLayout::Sidebar;
        let content_x = if show_rail { layout.content.x } else { rect.x };
        let content_right = if layout.show_right {
            layout.right.x.saturating_sub(1)
        } else {
            rect.right()
        };
        let content = Rect::new(
            content_x,
            rect.y,
            content_right.saturating_sub(content_x),
            rect.height,
        );
        if let PageState::Welcome { state, .. } = ui.current_page_mut() {
            state.rail_visible = show_rail;
        }
        // Each pane draws its selection as active only while it owns focus.
        let focus = ui.welcome_focus();
        let panes = PaneFocus {
            rail: is_active && focus == WorkspaceFocusState::Navigation,
            rows: is_active && focus == WorkspaceFocusState::Context,
            buttons: is_active && focus == WorkspaceFocusState::Actions,
        };

        if show_rail {
            render_step_rail(frame, ui, layout.navigation, step, panes.rail, &mut hits);
            vertical_rule(frame, ui, layout.navigation.right(), rect);
        }
        if layout.show_right {
            vertical_rule(frame, ui, content.right(), rect);
            render_inspector(frame, ui, layout.right, step, selected);
        }
        render_content(
            frame,
            ui,
            content,
            step,
            selected,
            panes,
            show_rail,
            layout.show_right,
            &mut hits,
        );
    }

    ui.workspace_hits.extend(hits);
}

#[derive(Clone, Copy)]
struct PaneFocus {
    rail: bool,
    rows: bool,
    buttons: bool,
}

fn welcome_position(ui: &UIStateGuard) -> Option<(WelcomeStep, usize, WelcomeLayout)> {
    match ui.current_page() {
        PageState::Welcome { state, .. } => {
            Some((state.step, state.list.selected().unwrap_or(0), state.layout))
        }
        _ => None,
    }
}

fn vertical_rule(frame: &mut Frame, ui: &UIStateGuard, x: u16, rect: Rect) {
    utils::render_vertical_rule(
        frame,
        Rect::new(x, rect.y, 1, rect.height),
        "│",
        ui.theme.workspace_border(),
    );
}

fn horizontal_rule(frame: &mut Frame, ui: &UIStateGuard, rect: Rect) {
    utils::render_horizontal_rule(frame, rect, "─", ui.theme.workspace_border());
}

fn text(frame: &mut Frame, rect: Rect, value: impl Into<String>, style: Style) {
    if rect.is_empty() {
        return;
    }
    frame.render_widget(Paragraph::new(value.into()).style(style), rect);
}

fn wrapped_height(lines: &[Line<'static>], width: u16) -> u16 {
    if lines.is_empty() {
        return 0;
    }
    Paragraph::new(lines.to_vec())
        .wrap(Wrap { trim: true })
        .line_count(width.max(1))
        .min(usize::from(u16::MAX)) as u16
}

fn render_step_rail(
    frame: &mut Frame,
    ui: &UIStateGuard,
    nav: Rect,
    current: WelcomeStep,
    focused: bool,
    hits: &mut Vec<(Rect, WorkspaceHit)>,
) {
    if nav.is_empty() {
        return;
    }
    frame.render_widget(Block::default().style(ui.theme.workspace_panel()), nav);
    text(
        frame,
        Rect::new(
            nav.x.saturating_add(2),
            nav.y.saturating_add(1),
            nav.width.saturating_sub(4),
            1,
        ),
        "Setup",
        ui.theme.workspace_heading(),
    );
    for step in WelcomeStep::ALL {
        let y = nav.y.saturating_add(3 + step.index() as u16 * 2);
        if y >= nav.bottom() {
            break;
        }
        // One column wider than route rows so the marker and the longest
        // step label fit the minimum rail width.
        let row = Rect::new(nav.x.saturating_add(1), y, nav.width.saturating_sub(2), 1);
        let active = step == current;
        let (marker, style) = if active {
            ("›", ui.theme.workspace_navigation_active())
        } else if step.index() < current.index() {
            ("✓", ui.theme.workspace_panel())
        } else {
            ("○", ui.theme.workspace_secondary_text())
        };
        if active {
            frame.render_widget(Block::default().style(style), row);
        }
        let style = if active && focused {
            ui.theme
                .workspace_base()
                .patch(ui.theme.workspace_focus_indicator())
        } else {
            style
        };
        let label_width = row.width.saturating_sub(1);
        text(
            frame,
            Rect::new(row.x.saturating_add(1), y, label_width, 1),
            utils::bounded_text(
                &format!("{marker} {}", rail_label(step)),
                usize::from(label_width),
            ),
            style,
        );
        hits.push((row, WorkspaceHit::WelcomeStep(step)));
    }
}

const fn rail_label(step: WelcomeStep) -> &'static str {
    match step {
        WelcomeStep::ListenBrainz => "ListenBrainz",
        _ => step.title(),
    }
}

fn render_inspector(
    frame: &mut Frame,
    ui: &UIStateGuard,
    rect: Rect,
    step: WelcomeStep,
    selected: usize,
) {
    if rect.is_empty() {
        return;
    }
    frame.render_widget(Block::default().style(ui.theme.workspace_panel()), rect);
    let x = rect.x.saturating_add(2);
    let width = rect.width.saturating_sub(4);
    let action = step.action_at(selected);
    text(
        frame,
        Rect::new(x, rect.y.saturating_add(1), width, 1),
        utils::bounded_text(
            &action.map_or_else(
                || step.title().to_owned(),
                |action| action_label(ui, action),
            ),
            usize::from(width),
        ),
        ui.theme.workspace_heading(),
    );
    frame.render_widget(
        Paragraph::new(action_help(action))
            .style(ui.theme.workspace_secondary_text())
            .wrap(Wrap { trim: true }),
        Rect::new(
            x,
            rect.y.saturating_add(3),
            width,
            rect.height.saturating_sub(4),
        ),
    );
}

/// Body of one content section. Sections share a height budget; each one is
/// either a scrollable action group or a block of wrapped status lines.
enum SectionBody {
    Actions(Vec<ActionRow>),
    Lines(Vec<Line<'static>>),
    /// Review readiness: a detailed table when its full height fits,
    /// otherwise the compact status lines.
    Readiness(Vec<Line<'static>>),
}

struct Section {
    heading: &'static str,
    body: SectionBody,
    need: u16,
    /// Lower values are allocated first when the content is too short.
    priority: u8,
}

fn content_sections(
    ui: &UIStateGuard,
    step: WelcomeStep,
    selected: usize,
    compact: bool,
    width: u16,
) -> (Vec<Section>, Vec<ActionRow>) {
    let actions = |heading, rows: Vec<ActionRow>| {
        let priority = if rows.iter().any(|row| row.index == selected) {
            0
        } else {
            2
        };
        Section {
            heading,
            need: rows.len().min(usize::from(u16::MAX)) as u16,
            body: SectionBody::Actions(rows),
            priority,
        }
    };
    let lines = |heading, lines: Vec<Line<'static>>, priority| Section {
        heading,
        need: wrapped_height(&lines, width),
        body: SectionBody::Lines(lines),
        priority,
    };

    let (leading, trailing, navigation) = provider_action_groups(ui, step);
    let sections = match step {
        WelcomeStep::Spotify | WelcomeStep::YouTube => vec![
            actions("Client", leading),
            lines("Status", provider_base_status_lines(ui, step, compact), 3),
            actions("Actions", trailing),
            lines("Result", provider_result_status_lines(ui, step, compact), 1),
        ],
        WelcomeStep::Review => {
            let lines = review_status_lines(ui, compact);
            let need = if width >= REVIEW_TABLE_MIN_WIDTH {
                REVIEW_TABLE_HEIGHT
            } else {
                wrapped_height(&lines, width)
            };
            vec![
                Section {
                    heading: "Provider readiness",
                    body: SectionBody::Readiness(lines),
                    need,
                    priority: 3,
                },
                actions("Actions", leading.into_iter().chain(trailing).collect()),
            ]
        }
        WelcomeStep::Preferences | WelcomeStep::ListenBrainz => vec![
            lines("Status", step_status_lines(ui, step), 3),
            actions("Actions", leading.into_iter().chain(trailing).collect()),
        ],
    };
    let sections = sections
        .into_iter()
        .filter(|section| section.need > 0)
        .collect();
    (sections, navigation)
}

/// Lay out buttons left to right, or `None` when they do not fit one row.
fn horizontal_buttons(labels: &[&str], area: Rect) -> Option<Vec<Rect>> {
    let widths = labels
        .iter()
        .map(|label| label.chars().count().min(usize::from(u16::MAX)) as u16 + 4)
        .collect::<Vec<_>>();
    let total = widths
        .iter()
        .fold(0u16, |sum, width| sum.saturating_add(*width))
        .saturating_add(2 * labels.len().saturating_sub(1) as u16);
    (total <= area.width).then(|| {
        let mut x = area.x;
        widths
            .into_iter()
            .map(|width| {
                let rect = Rect::new(x, area.y, width, 1);
                x = x.saturating_add(width).saturating_add(2);
                rect
            })
            .collect()
    })
}

#[allow(clippy::too_many_arguments)]
fn render_content(
    frame: &mut Frame,
    ui: &mut UIStateGuard,
    area: Rect,
    step: WelcomeStep,
    selected: usize,
    panes: PaneFocus,
    show_rail: bool,
    show_inspector: bool,
    hits: &mut Vec<(Rect, WorkspaceHit)>,
) {
    let x = area.x.saturating_add(2);
    let width = area.width.saturating_sub(4);
    if width == 0 || area.height == 0 {
        return;
    }
    let compact = area.width < COMPACT_CONTENT_WIDTH;
    let (mut sections, navigation) = content_sections(ui, step, selected, compact, width);
    let description = copy_for_step(step, compact);

    // Fixed rows: title and navigation buttons. Everything else is granted
    // from what remains, sections first, then spacing and secondary chrome.
    let probe = Rect::new(x, 0, width, u16::MAX);
    let labels = navigation
        .iter()
        .map(|row| row.label.as_str())
        .collect::<Vec<_>>();
    let buttons = horizontal_buttons(&labels, probe).unwrap_or_else(|| {
        (0..labels.len())
            .map(|row| Rect::new(x, row as u16, width, 1))
            .collect()
    });
    let button_height = buttons
        .last()
        .map_or(0, |rect| rect.bottom().saturating_sub(probe.y))
        .min(area.height.saturating_sub(1));
    let mut avail = area.height.saturating_sub(1 + button_height);
    let description_height = wrapped_height(&description, width).min(3).min(avail);
    avail -= description_height;

    let mut order = (0..sections.len()).collect::<Vec<_>>();
    order.sort_by_key(|index| sections[*index].priority);
    let mut allocation = vec![0u16; sections.len()];
    for index in order {
        allocation[index] = sections[index].need.min(avail);
        avail -= allocation[index];
    }
    let visible_sections = allocation.iter().filter(|height| **height > 0).count() as u16;
    let mut grant = |rows: u16| {
        let granted = avail >= rows;
        if granted {
            avail -= rows;
        }
        granted
    };
    let show_headings = grant(visible_sections);
    let show_rule = grant(1);
    let help = (!show_inspector)
        .then(|| action_help(step.action_at(selected)))
        .filter(|_| grant(1));
    let description_gap = grant(1);
    let rail_offset = show_rail && grant(1);

    let mut y = area.y.saturating_add(u16::from(rail_offset));
    let title = if show_rail {
        format!(
            "Step {} of {} · {}",
            step.index() + 1,
            WelcomeStep::ALL.len(),
            step.title()
        )
    } else {
        format!(
            "Setup · Step {} of {} · {}",
            step.index() + 1,
            WelcomeStep::ALL.len(),
            step.title()
        )
    };
    text(
        frame,
        Rect::new(x, y, width, 1),
        utils::bounded_text(&title, usize::from(width)),
        ui.theme.workspace_heading(),
    );
    y = y.saturating_add(1);
    if description_height > 0 {
        frame.render_widget(
            Paragraph::new(description)
                .style(ui.theme.workspace_secondary_text())
                .wrap(Wrap { trim: true }),
            Rect::new(x, y, width, description_height),
        );
        y = y.saturating_add(description_height);
    }
    y = y.saturating_add(u16::from(description_gap));

    for (section, height) in sections.iter_mut().zip(allocation) {
        if height == 0 {
            continue;
        }
        if show_headings {
            horizontal_rule(
                frame,
                ui,
                Rect::new(area.x.saturating_add(1), y, area.width.saturating_sub(2), 1),
            );
            text(
                frame,
                Rect::new(
                    x,
                    y,
                    (section.heading.chars().count() as u16 + 1).min(width),
                    1,
                ),
                format!("{} ", section.heading),
                ui.theme.workspace_secondary_text(),
            );
            y = y.saturating_add(1);
        }
        let body = Rect::new(x, y, width, height);
        match &mut section.body {
            SectionBody::Actions(rows) => {
                render_action_rows(frame, ui, area, body, rows, selected, panes.rows, hits);
            }
            SectionBody::Lines(lines) => render_lines(frame, ui, body, std::mem::take(lines)),
            SectionBody::Readiness(lines) => {
                if width >= REVIEW_TABLE_MIN_WIDTH && height >= REVIEW_TABLE_HEIGHT {
                    render_review_table(frame, ui, body);
                } else {
                    render_lines(frame, ui, body, std::mem::take(lines));
                }
            }
        }
        y = y.saturating_add(height);
    }

    let band_top = area.bottom().saturating_sub(button_height);
    let mut line_y = band_top;
    if show_rule {
        line_y = line_y.saturating_sub(1);
        horizontal_rule(frame, ui, Rect::new(area.x, line_y, area.width, 1));
    }
    if let Some(help) = help {
        line_y = line_y.saturating_sub(1);
        text(
            frame,
            Rect::new(x, line_y, width, 1),
            utils::bounded_text(&help, usize::from(width)),
            ui.theme.workspace_secondary_text(),
        );
    }
    let offset = band_top.saturating_sub(probe.y);
    for (row, rect) in navigation.iter().zip(buttons) {
        let rect = Rect::new(rect.x, rect.y.saturating_add(offset), rect.width, 1);
        if rect.y >= area.bottom() {
            break;
        }
        draw_button(
            frame,
            ui,
            rect,
            &row.label,
            row.index == selected,
            panes.buttons,
        );
        hits.push((rect, WorkspaceHit::WelcomeAction(row.index)));
    }
}

fn render_lines(frame: &mut Frame, ui: &UIStateGuard, area: Rect, lines: Vec<Line<'static>>) {
    if area.is_empty() {
        return;
    }
    frame.render_widget(
        Paragraph::new(lines)
            .style(ui.theme.workspace_base())
            .wrap(Wrap { trim: true }),
        area,
    );
}

/// Render one action group into its granted rows. The group holding the
/// selection scrolls to keep it visible; `state.list` stores the global
/// index of that group's first visible row.
#[allow(clippy::too_many_arguments)]
fn render_action_rows(
    frame: &mut Frame,
    ui: &mut UIStateGuard,
    content: Rect,
    area: Rect,
    rows: &[ActionRow],
    selected: usize,
    is_active: bool,
    hits: &mut Vec<(Rect, WorkspaceHit)>,
) {
    if area.is_empty() || rows.is_empty() {
        return;
    }
    let selected_local = rows.iter().position(|row| row.index == selected);
    let offset = match (selected_local, ui.current_page()) {
        (Some(local), PageState::Welcome { state, .. }) => {
            let current = state
                .list
                .offset()
                .checked_sub(rows[0].index)
                .filter(|local| *local < rows.len())
                .unwrap_or(0);
            list_offset_for_selection(Some(local), current, rows.len(), area.height)
        }
        _ => 0,
    };
    if selected_local.is_some() {
        if let PageState::Welcome { state, .. } = ui.current_page_mut() {
            *state.list.offset_mut() = rows[offset].index;
        }
    }
    utils::render_vertical_rule(
        frame,
        Rect::new(content.x.saturating_add(1), area.y, 1, area.height),
        "│",
        if is_active && selected_local.is_some() {
            ui.theme.workspace_focus_indicator()
        } else {
            ui.theme.workspace_border()
        },
    );
    for (line, row) in rows
        .iter()
        .skip(offset)
        .take(usize::from(area.height))
        .enumerate()
    {
        let rect = Rect::new(area.x, area.y.saturating_add(line as u16), area.width, 1);
        draw_row(
            frame,
            ui,
            rect,
            &row.label,
            row.index == selected,
            is_active,
        );
        hits.push((rect, WorkspaceHit::WelcomeAction(row.index)));
    }
}

fn selection_style(ui: &UIStateGuard, is_active: bool) -> Style {
    if is_active {
        ui.theme.workspace_selection_active()
    } else {
        ui.theme.workspace_selection_inactive()
    }
}

fn draw_row(
    frame: &mut Frame,
    ui: &UIStateGuard,
    rect: Rect,
    label: &str,
    focused: bool,
    is_active: bool,
) {
    let style = if focused {
        let selection = selection_style(ui, is_active);
        frame.render_widget(Block::default().style(selection), rect);
        ui.theme.workspace_base().patch(selection)
    } else {
        ui.theme.workspace_base()
    };
    let width = rect.width.saturating_sub(2);
    text(
        frame,
        Rect::new(rect.x.saturating_add(1), rect.y, width, 1),
        utils::focused_row_text(
            label,
            usize::from(width),
            focused && is_active,
            ui.presentation.focused_row_overflow,
            ui.focused_marquee_phase(),
        ),
        style,
    );
}

fn draw_button(
    frame: &mut Frame,
    ui: &UIStateGuard,
    rect: Rect,
    label: &str,
    focused: bool,
    is_active: bool,
) {
    let surface = if focused {
        selection_style(ui, is_active)
    } else {
        ui.theme.workspace_panel()
    };
    frame.render_widget(Block::default().style(surface), rect);
    let width = rect.width.saturating_sub(2);
    text(
        frame,
        Rect::new(rect.x.saturating_add(2), rect.y, width, 1),
        utils::bounded_text(label, usize::from(width)),
        ui.theme.workspace_base().patch(surface),
    );
}

fn provider_action_groups(
    ui: &UIStateGuard,
    step: WelcomeStep,
) -> (Vec<ActionRow>, Vec<ActionRow>, Vec<ActionRow>) {
    let mut leading = Vec::new();
    let mut trailing = Vec::new();
    let mut navigation = Vec::new();
    for row in action_rows(ui, step) {
        if row.action.is_navigation() {
            navigation.push(row);
        } else if matches!(
            row.action,
            WelcomeAction::SpotifyBundledClient | WelcomeAction::SpotifyCustomClient
        ) {
            leading.push(row);
        } else {
            trailing.push(row);
        }
    }
    (leading, trailing, navigation)
}

/// The redirect URI a custom Spotify application must register; sign-in
/// fails in the browser when the dashboard does not list it exactly.
fn spotify_redirect_uri() -> &'static str {
    &config::get_config().app_config.login_redirect_uri
}

fn action_help(action: Option<WelcomeAction>) -> Cow<'static, str> {
    if action == Some(WelcomeAction::SpotifyCustomClient) {
        return Cow::Owned(format!(
            "Save your own client ID (no secret needed). Add {} to the app's Redirect URIs.",
            spotify_redirect_uri()
        ));
    }
    Cow::Borrowed(match action {
        Some(WelcomeAction::ListenBrainzEnterToken) => {
            "Enter a user token from listenbrainz.org/settings/."
        }
        Some(WelcomeAction::ListenBrainzFetchPlaylists) => {
            "Fetch remote playlists, then choose one to import locally."
        }
        Some(WelcomeAction::ListenBrainzCheckToken) => {
            "Validate the saved token without changing integration preferences."
        }
        Some(WelcomeAction::PreferencesStartupProvider) => "Choose which provider opens first.",
        Some(WelcomeAction::PreferencesStartPaused) => {
            "Keep startup playback paused until you choose a track."
        }
        Some(WelcomeAction::SpotifyBundledClient) => "Use the bundled Web API client.",
        Some(WelcomeAction::SpotifySignIn) => {
            "Apply the saved client choice and sign in without restarting."
        }
        Some(WelcomeAction::SpotifyCheckSession) => {
            "Check saved credentials and integrated playback."
        }
        Some(WelcomeAction::YouTubeSignIn) => {
            "Sign in using a dedicated browser; activate again to cancel."
        }
        Some(WelcomeAction::YouTubeChooseBrowser) => {
            "Choose a Helium, Chrome, Chromium, or Edge executable."
        }
        Some(WelcomeAction::YouTubeDetectBrowser) => {
            "Retry automatic browser discovery, including Helium."
        }
        Some(WelcomeAction::YouTubeImportCookies) => {
            "No compatible browser? Import cookies from your signed-in browser."
        }
        Some(WelcomeAction::YouTubeTestAccount) => {
            "Check account access and native playback separately."
        }
        Some(WelcomeAction::ReviewFixSpotify) => {
            "Return to Spotify without starting authentication."
        }
        Some(WelcomeAction::ReviewFixYouTube) => {
            "Return to YouTube without starting authentication."
        }
        Some(
            WelcomeAction::PreferencesNext
            | WelcomeAction::SpotifyBack
            | WelcomeAction::SpotifyNext
            | WelcomeAction::YouTubeBack
            | WelcomeAction::YouTubeNext
            | WelcomeAction::ListenBrainzBack
            | WelcomeAction::ListenBrainzNext
            | WelcomeAction::ReviewBack,
        ) => "Move to the adjacent setup step.",
        Some(WelcomeAction::ReviewContinue) => {
            "Finish setup only when the selected provider is ready."
        }
        Some(WelcomeAction::ReviewSkip) => {
            "Leave setup pending for now and continue with the fallback."
        }
        // Answered above with the configured redirect URI.
        None | Some(WelcomeAction::SpotifyCustomClient) => "Choose an action.",
    })
}

fn action_rows(ui: &UIStateGuard, step: WelcomeStep) -> Vec<ActionRow> {
    step.actions()
        .iter()
        .enumerate()
        .map(|(index, action)| ActionRow {
            index,
            action: *action,
            label: action_label(ui, *action),
        })
        .inspect(|row| debug_assert_eq!(row.action.step(), step))
        .collect()
}

fn action_label(ui: &UIStateGuard, action: WelcomeAction) -> String {
    match action {
        WelcomeAction::PreferencesStartupProvider => format!(
            "Startup provider: {} (switch)",
            ui.setup_state.startup_provider.title()
        ),
        WelcomeAction::PreferencesStartPaused => format!(
            "Start paused: {} (toggle)",
            if ui.setup_state.pause_on_startup {
                "yes"
            } else {
                "no"
            }
        ),
        WelcomeAction::PreferencesNext => "Next: Spotify".to_owned(),
        WelcomeAction::SpotifyBundledClient => format!(
            "{} Bundled client (ncspot)",
            if is_bundled_client(ui) { "●" } else { "○" }
        ),
        WelcomeAction::SpotifyCustomClient => format!(
            "{} Use my own client ID",
            if is_bundled_client(ui) { "○" } else { "●" }
        ),
        WelcomeAction::SpotifySignIn => if ui.welcome_spotify_client_pending
            || ui.setup_state.spotify_reauthentication_required
        {
            "Apply & sign in"
        } else {
            "Sign in with Spotify"
        }
        .to_owned(),
        WelcomeAction::SpotifyCheckSession => "Check existing session".to_owned(),
        WelcomeAction::SpotifyBack => "Back: Preferences".to_owned(),
        WelcomeAction::SpotifyNext => "Next: YouTube".to_owned(),
        WelcomeAction::YouTubeSignIn => if ui.welcome_youtube_login_active {
            "Cancel sign-in / import"
        } else {
            "Sign in with a dedicated browser"
        }
        .to_owned(),
        WelcomeAction::YouTubeChooseBrowser => "Choose browser path".to_owned(),
        WelcomeAction::YouTubeDetectBrowser => "Retry browser detection".to_owned(),
        WelcomeAction::YouTubeImportCookies => "Import cookies (other sign-in option)".to_owned(),
        WelcomeAction::YouTubeTestAccount => "Check account & playback".to_owned(),
        WelcomeAction::YouTubeBack => "Back: Spotify".to_owned(),
        WelcomeAction::YouTubeNext => "Next: ListenBrainz (optional)".to_owned(),
        WelcomeAction::ListenBrainzEnterToken => if ui.welcome_listenbrainz_pending.is_some() {
            "Replace pending token"
        } else {
            "Enter user token"
        }
        .to_owned(),
        WelcomeAction::ListenBrainzCheckToken => "Check saved token".to_owned(),
        WelcomeAction::ListenBrainzFetchPlaylists => {
            if ui.welcome_listenbrainz_identity.is_some() {
                "Fetch my playlists"
            } else {
                "Fetch my playlists (validate token first)"
            }
            .to_owned()
        }
        WelcomeAction::ListenBrainzBack => "Back: YouTube".to_owned(),
        WelcomeAction::ListenBrainzNext => "Next / Skip: Review".to_owned(),
        WelcomeAction::ReviewFixSpotify => "Fix Spotify setup".to_owned(),
        WelcomeAction::ReviewFixYouTube => "Fix YouTube setup".to_owned(),
        WelcomeAction::ReviewBack => "Back: ListenBrainz".to_owned(),
        WelcomeAction::ReviewContinue => {
            format!("Continue with {}", ui.setup_state.startup_provider.title())
        }
        WelcomeAction::ReviewSkip => "Skip setup for now".to_owned(),
    }
}

fn is_bundled_client(ui: &UIStateGuard) -> bool {
    !ui.welcome_spotify_client_command
        && ui.welcome_spotify_client_id == crate::auth::NCSPOT_CLIENT_ID
}

fn copy_for_step(step: WelcomeStep, compact: bool) -> Vec<Line<'static>> {
    let description = match (step, compact) {
        (WelcomeStep::ListenBrainz, _) => vec![
            Line::raw("Optional: validate a ListenBrainz user token."),
            Line::raw("Background integration preferences stay unchanged."),
        ],
        (WelcomeStep::Preferences, true) => vec![
            Line::raw("Choose startup defaults."),
            Line::raw("Draft only; no provider request."),
        ],
        (WelcomeStep::Spotify, true) => vec![
            Line::raw("Choose a client and connect Spotify."),
            Line::raw("Library, playback, Premium separate."),
        ],
        (WelcomeStep::YouTube, true) => vec![
            Line::raw("Connect account and library access."),
            Line::raw("Playback resolves native streams first."),
        ],
        (WelcomeStep::Review, true) => vec![
            Line::raw("Confirm startup choice and readiness."),
            Line::raw("Fix a provider or continue when ready."),
        ],
        (WelcomeStep::Preferences, false) => vec![
            Line::raw("Choose the defaults used when the application opens."),
            Line::raw("Changes are saved as a local draft; no provider request runs here."),
        ],
        (WelcomeStep::Spotify, false) => vec![
            Line::raw("Connect your Spotify account for library and playback."),
            Line::raw("Library, playback, and Premium are separate checks."),
        ],
        (WelcomeStep::YouTube, false) => vec![
            Line::raw("Connect account and library access for YouTube Music."),
            Line::raw("native streams first; sign-in uses a dedicated browser."),
        ],
        (WelcomeStep::Review, false) => vec![
            Line::raw("Confirm the startup preference and provider readiness."),
            Line::raw("Fix a provider before continuing, or skip setup for now."),
        ],
    };
    description
}

/// Status for the steps without a provider-specific status/result split.
fn step_status_lines(ui: &UIStateGuard, step: WelcomeStep) -> Vec<Line<'static>> {
    match step {
        WelcomeStep::Preferences => vec![
            status_line(
                "Spotify",
                spotify_short_status(ui),
                StatusTone::Neutral,
                &ui.theme,
            ),
            status_line(
                "YouTube",
                youtube_short_status(ui),
                StatusTone::Neutral,
                &ui.theme,
            ),
        ],
        WelcomeStep::ListenBrainz => {
            vec![
                Line::raw(ui.welcome_listenbrainz_username.as_ref().map_or_else(
                    || "ListenBrainz is optional.".to_owned(),
                    |name| format!("Connected: {name}"),
                )),
                Line::raw(ui.welcome_listenbrainz_notice.clone().unwrap_or_else(|| {
                    "Enter a user token, check a saved token, or skip.".to_owned()
                })),
            ]
        }
        WelcomeStep::Spotify | WelcomeStep::YouTube | WelcomeStep::Review => Vec::new(),
    }
}

fn provider_base_status_lines(
    ui: &UIStateGuard,
    step: WelcomeStep,
    compact: bool,
) -> Vec<Line<'static>> {
    match step {
        WelcomeStep::Spotify => spotify_base_status_lines(ui, compact),
        WelcomeStep::YouTube => youtube_base_status_lines(ui, compact),
        WelcomeStep::Preferences | WelcomeStep::ListenBrainz | WelcomeStep::Review => Vec::new(),
    }
}

fn provider_result_status_lines(
    ui: &UIStateGuard,
    step: WelcomeStep,
    compact: bool,
) -> Vec<Line<'static>> {
    match step {
        WelcomeStep::Spotify => spotify_result_status_lines(ui, compact),
        WelcomeStep::YouTube => youtube_result_status_lines(ui, compact),
        WelcomeStep::Preferences | WelcomeStep::ListenBrainz | WelcomeStep::Review => Vec::new(),
    }
}

fn status_line(
    label: &str,
    value: String,
    tone: StatusTone,
    theme: &config::Theme,
) -> Line<'static> {
    let style = match tone {
        StatusTone::Neutral => theme.workspace_base(),
        StatusTone::Success => theme.workspace_status_success(),
        StatusTone::Attention => theme.workspace_status_warning(),
    };
    Line::from(vec![
        Span::styled(format!("{label}: "), theme.workspace_secondary_text()),
        Span::styled(value, style),
    ])
}

fn spotify_base_status_lines(ui: &UIStateGuard, _compact: bool) -> Vec<Line<'static>> {
    let library = match ui.welcome_spotify_library_tested {
        Some(true) => ("confirmed".to_owned(), StatusTone::Success),
        Some(false) => ("check failed".to_owned(), StatusTone::Attention),
        None if ui.welcome_spotify_web_token_cached => {
            ("Saved · not checked".to_owned(), StatusTone::Neutral)
        }
        None => ("Not connected · sign in".to_owned(), StatusTone::Neutral),
    };
    let playback = match ui.welcome_spotify_playback_tested {
        Some(true) => ("confirmed".to_owned(), StatusTone::Success),
        Some(false) => ("unavailable · retry".to_owned(), StatusTone::Attention),
        None if ui.spotify_auth_status.session_ready => {
            ("session available".to_owned(), StatusTone::Neutral)
        }
        None => ("not connected".to_owned(), StatusTone::Neutral),
    };
    let premium = match ui.spotify_auth_status.premium {
        SpotifyPremiumStatus::Premium => ("confirmed".to_owned(), StatusTone::Success),
        SpotifyPremiumStatus::NotPremium => ("not Premium".to_owned(), StatusTone::Attention),
        SpotifyPremiumStatus::Unknown => ("unverified".to_owned(), StatusTone::Neutral),
    };
    let mut lines = vec![
        status_line("Library", library.0, library.1, &ui.theme),
        status_line("Playback", playback.0, playback.1, &ui.theme),
        status_line("Premium", premium.0, premium.1, &ui.theme),
    ];
    if ui.welcome_spotify_client_pending || ui.setup_state.spotify_reauthentication_required {
        lines.push(status_line(
            "Client",
            "saved · Apply & sign in".to_owned(),
            StatusTone::Attention,
            &ui.theme,
        ));
    }
    lines
}

fn spotify_result_status_lines(ui: &UIStateGuard, compact: bool) -> Vec<Line<'static>> {
    let mut lines = Vec::new();
    if let Some(operation) = spotify_operation_line(ui) {
        lines.push(operation);
    }
    if let Some(failure) = spotify_setup_failure(ui) {
        lines.push(status_line(
            "Needs",
            if compact {
                spotify_compact_failure(failure).to_owned()
            } else {
                failure.message().to_owned()
            },
            StatusTone::Attention,
            &ui.theme,
        ));
    }
    lines
}

fn spotify_setup_failure(ui: &UIStateGuard) -> Option<config::SetupFailure> {
    ui.setup_state.failure.filter(|failure| {
        matches!(
            *failure,
            config::SetupFailure::MissingSpotifySession
                | config::SetupFailure::MissingSpotifyPremium
                | config::SetupFailure::SpotifySessionUnavailable
        )
    })
}

fn spotify_compact_failure(failure: config::SetupFailure) -> &'static str {
    match failure {
        config::SetupFailure::MissingSpotifySession => "Spotify sign-in required",
        config::SetupFailure::MissingSpotifyPremium => "Premium account required",
        config::SetupFailure::SpotifySessionUnavailable => "Playback session unavailable",
        config::SetupFailure::AuthenticationFailed | config::SetupFailure::PersistenceFailed => {
            "Retry setup"
        }
        config::SetupFailure::MissingYouTubeAccountAuth => "YouTube sign-in required",
    }
}

fn spotify_operation_line(ui: &UIStateGuard) -> Option<Line<'static>> {
    let (value, tone) = match ui.welcome_spotify_operation {
        WelcomeOperation::Idle => {
            return ui.welcome_spotify_notice.as_ref().map(|notice| {
                status_line("Result", notice.clone(), StatusTone::Neutral, &ui.theme)
            });
        }
        WelcomeOperation::SigningIn => ("Signing in…".to_owned(), StatusTone::Neutral),
        WelcomeOperation::Checking => ("Checking…".to_owned(), StatusTone::Neutral),
        WelcomeOperation::Waiting => (
            "Waiting for the current action…".to_owned(),
            StatusTone::Neutral,
        ),
        WelcomeOperation::Succeeded => ("Last action succeeded".to_owned(), StatusTone::Success),
        WelcomeOperation::RateLimited => (
            "Temporarily limited · retry later".to_owned(),
            StatusTone::Attention,
        ),
        WelcomeOperation::Failed => ("Action failed · retry".to_owned(), StatusTone::Attention),
        WelcomeOperation::Cancelled => ("Action cancelled".to_owned(), StatusTone::Neutral),
    };
    Some(status_line(
        "Result",
        ui.welcome_spotify_notice.clone().unwrap_or(value),
        tone,
        &ui.theme,
    ))
}

fn youtube_base_status_lines(ui: &UIStateGuard, compact: bool) -> Vec<Line<'static>> {
    let (auth_value, _) = match (
        ui.youtube_auth_status.auth_type,
        ui.youtube_auth_status.ready,
    ) {
        (YouTubeMusicAuthType::Browser, true) => (
            "Sign-in saved · not checked".to_owned(),
            StatusTone::Neutral,
        ),
        (YouTubeMusicAuthType::Browser, false) => {
            ("No saved sign-in".to_owned(), StatusTone::Neutral)
        }
        (YouTubeMusicAuthType::OAuth, true) => {
            ("OAuth ready · not checked".to_owned(), StatusTone::Neutral)
        }
        (YouTubeMusicAuthType::OAuth, false) => {
            ("OAuth missing · sign in".to_owned(), StatusTone::Neutral)
        }
        (YouTubeMusicAuthType::Unauthenticated, _) => {
            ("Not connected · sign in".to_owned(), StatusTone::Neutral)
        }
    };
    let account = match ui.welcome_youtube_account_tested {
        Some(true) if compact => ("confirmed".to_owned(), StatusTone::Success),
        Some(false) if compact => ("test failed".to_owned(), StatusTone::Attention),
        Some(true) => (format!("{auth_value} · confirmed"), StatusTone::Success),
        Some(false) => (format!("{auth_value} · test failed"), StatusTone::Attention),
        None => (auth_value, StatusTone::Neutral),
    };
    let playback = match ui.welcome_youtube_playback_tested {
        Some(true) => ("confirmed".to_owned(), StatusTone::Success),
        Some(false) => ("test failed · retry".to_owned(), StatusTone::Attention),
        None => ("not tested".to_owned(), StatusTone::Neutral),
    };
    let browser = ui
        .welcome_youtube_browser
        .as_ref()
        .and_then(|path| path.file_name())
        .map_or_else(
            || "not found; import is available".to_owned(),
            |name| name.to_string_lossy().into_owned(),
        );
    let lines = vec![
        status_line("Browser", browser, StatusTone::Neutral, &ui.theme),
        status_line(
            if compact {
                "Account"
            } else {
                "Account/library"
            },
            account.0,
            account.1,
            &ui.theme,
        ),
        status_line("Playback", playback.0, playback.1, &ui.theme),
    ];
    lines
}

fn youtube_result_status_lines(ui: &UIStateGuard, compact: bool) -> Vec<Line<'static>> {
    let mut lines = Vec::new();
    if let Some(operation) = youtube_operation_line(ui) {
        lines.push(operation);
    }
    if let Some(failure) = youtube_setup_failure(ui) {
        lines.push(status_line(
            "Needs",
            if compact {
                "YouTube account sign-in required".to_owned()
            } else {
                failure.message().to_owned()
            },
            StatusTone::Attention,
            &ui.theme,
        ));
    }
    lines
}

fn youtube_setup_failure(ui: &UIStateGuard) -> Option<config::SetupFailure> {
    ui.setup_state
        .failure
        .filter(|failure| matches!(*failure, config::SetupFailure::MissingYouTubeAccountAuth))
}

fn youtube_operation_line(ui: &UIStateGuard) -> Option<Line<'static>> {
    let (value, tone) = match ui.welcome_youtube_operation {
        WelcomeOperation::Idle => {
            return ui.welcome_youtube_notice.as_ref().map(|notice| {
                status_line("Result", notice.clone(), StatusTone::Neutral, &ui.theme)
            });
        }
        WelcomeOperation::SigningIn => ("Signing in…".to_owned(), StatusTone::Neutral),
        WelcomeOperation::Checking => ("Checking…".to_owned(), StatusTone::Neutral),
        WelcomeOperation::Waiting => (
            "Waiting for the current action…".to_owned(),
            StatusTone::Neutral,
        ),
        WelcomeOperation::Succeeded => ("Last action succeeded".to_owned(), StatusTone::Success),
        WelcomeOperation::RateLimited => (
            "Temporarily limited · retry later".to_owned(),
            StatusTone::Attention,
        ),
        WelcomeOperation::Failed => ("Action failed · retry".to_owned(), StatusTone::Attention),
        WelcomeOperation::Cancelled => ("Action cancelled".to_owned(), StatusTone::Neutral),
    };
    Some(status_line(
        "Result",
        ui.welcome_youtube_notice.clone().unwrap_or(value),
        tone,
        &ui.theme,
    ))
}

fn review_status_lines(ui: &UIStateGuard, compact: bool) -> Vec<Line<'static>> {
    let setup = match ui.setup_state.status {
        config::SetupStatus::Pending => "pending",
        config::SetupStatus::Ready => "ready",
        config::SetupStatus::Skipped => "skipped",
        config::SetupStatus::Failed => "needs attention",
    };
    if compact {
        return vec![
            status_line(
                "Startup",
                format!(
                    "{} · paused {}",
                    ui.setup_state.startup_provider.title(),
                    if ui.setup_state.pause_on_startup {
                        "yes"
                    } else {
                        "no"
                    }
                ),
                StatusTone::Neutral,
                &ui.theme,
            ),
            status_line("Readiness", setup.to_owned(), review_tone(ui), &ui.theme),
            status_line(
                "Spotify",
                if ui.setup_auth_snapshot().spotify.ready() {
                    "ready · no fix needed".to_owned()
                } else {
                    "not ready · Fix Spotify".to_owned()
                },
                StatusTone::Neutral,
                &ui.theme,
            ),
            status_line(
                "YouTube",
                if ui.setup_auth_snapshot().youtube.account_ready {
                    "credentials saved · Check account".to_owned()
                } else {
                    "not ready · Fix YouTube".to_owned()
                },
                StatusTone::Neutral,
                &ui.theme,
            ),
        ];
    }
    vec![
        status_line(
            "Startup",
            format!(
                "{} · Start paused: {}",
                ui.setup_state.startup_provider.title(),
                if ui.setup_state.pause_on_startup {
                    "yes"
                } else {
                    "no"
                }
            ),
            StatusTone::Neutral,
            &ui.theme,
        ),
        status_line("Readiness", setup.to_owned(), review_tone(ui), &ui.theme),
        status_line(
            "Spotify",
            spotify_readiness_summary(ui),
            StatusTone::Neutral,
            &ui.theme,
        ),
        status_line(
            "YouTube",
            youtube_readiness_summary(ui),
            StatusTone::Neutral,
            &ui.theme,
        ),
    ]
}

fn review_tone(ui: &UIStateGuard) -> StatusTone {
    match ui.setup_state.status {
        config::SetupStatus::Ready => StatusTone::Success,
        config::SetupStatus::Failed => StatusTone::Attention,
        config::SetupStatus::Pending | config::SetupStatus::Skipped => StatusTone::Neutral,
    }
}

fn spotify_readiness_summary(ui: &UIStateGuard) -> String {
    if ui.setup_auth_snapshot().spotify.ready() {
        "ready · Premium confirmed".to_owned()
    } else {
        "not ready · Premium unverified".to_owned()
    }
}

fn youtube_readiness_summary(ui: &UIStateGuard) -> String {
    let auth = match (
        ui.youtube_auth_status.auth_type,
        ui.youtube_auth_status.ready,
    ) {
        (YouTubeMusicAuthType::Browser, true) => "Sign-in saved",
        (YouTubeMusicAuthType::OAuth, true) => "OAuth ready",
        _ => "not connected",
    };
    let test = match ui.welcome_youtube_account_tested {
        Some(true) => "account confirmed",
        Some(false) => "account test failed",
        None => "account unchecked",
    };
    format!("{auth} · {test}")
}

fn spotify_short_status(ui: &UIStateGuard) -> String {
    if ui.setup_auth_snapshot().spotify.ready() {
        "ready · Premium confirmed".to_owned()
    } else if ui.welcome_spotify_web_token_cached {
        "credentials saved · not checked".to_owned()
    } else {
        "not connected".to_owned()
    }
}

fn youtube_short_status(ui: &UIStateGuard) -> String {
    match (
        ui.youtube_auth_status.auth_type,
        ui.youtube_auth_status.ready,
    ) {
        (YouTubeMusicAuthType::Browser, true) => "Browser credentials saved".to_owned(),
        (YouTubeMusicAuthType::OAuth, true) => "OAuth credentials saved".to_owned(),
        _ => "not connected".to_owned(),
    }
}

fn review_table_rows(ui: &UIStateGuard) -> Vec<Row<'static>> {
    let spotify_next = if ui.setup_auth_snapshot().spotify.ready() {
        "Ready".to_owned()
    } else {
        "Fix Spotify".to_owned()
    };
    let youtube_next = if !ui.setup_auth_snapshot().youtube.account_ready {
        "Fix YouTube".to_owned()
    } else if ui.welcome_youtube_account_tested == Some(true) {
        "Ready".to_owned()
    } else {
        "Check account & playback".to_owned()
    };
    vec![
        Row::new([
            Cell::from("Spotify"),
            Cell::from(spotify_table_status(ui)),
            Cell::from(spotify_next),
        ]),
        Row::new([
            Cell::from("YouTube"),
            Cell::from(youtube_table_status(ui)),
            Cell::from(youtube_next),
        ]),
    ]
}

fn spotify_table_status(ui: &UIStateGuard) -> String {
    let library = if ui.welcome_spotify_library_tested == Some(true) {
        "confirmed"
    } else if ui.welcome_spotify_web_token_cached {
        "saved"
    } else {
        "missing"
    };
    let playback = if ui.welcome_spotify_playback_tested == Some(true) {
        "confirmed"
    } else if ui.spotify_auth_status.session_ready {
        "session"
    } else {
        "missing"
    };
    let premium = match ui.spotify_auth_status.premium {
        SpotifyPremiumStatus::Premium => "confirmed",
        SpotifyPremiumStatus::NotPremium => "not Premium",
        SpotifyPremiumStatus::Unknown => "unverified",
    };
    format!("Library: {library}\nPlayback: {playback}\nPremium: {premium}")
}

fn youtube_table_status(ui: &UIStateGuard) -> String {
    let auth = match (
        ui.youtube_auth_status.auth_type,
        ui.youtube_auth_status.ready,
    ) {
        (YouTubeMusicAuthType::Browser, true) => "Sign-in saved",
        (YouTubeMusicAuthType::OAuth, true) => "OAuth ready",
        _ => "missing",
    };
    let account = match ui.welcome_youtube_account_tested {
        Some(true) => "confirmed",
        Some(false) => "failed",
        None => "unchecked",
    };
    let playback = match ui.welcome_youtube_playback_tested {
        Some(true) => "confirmed",
        Some(false) => "failed",
        None => "untested",
    };
    format!("Auth: {auth}\nAccount: {account}\nPlayback: {playback}")
}

fn render_review_table(frame: &mut Frame, ui: &UIStateGuard, area: Rect) {
    if area.is_empty() {
        return;
    }
    let rows = review_table_rows(ui)
        .into_iter()
        .map(|row| row.height(3).style(ui.theme.workspace_base()))
        .collect::<Vec<_>>();
    let table = Table::new(
        rows,
        [
            Constraint::Length(9),
            Constraint::Fill(1),
            Constraint::Length(12),
        ],
    )
    .header(
        Row::new(["Provider", "Status", "Next action"]).style(ui.theme.workspace_table_header()),
    )
    .column_spacing(1);
    frame.render_widget(table, area);
}

/// The live popup and the credential-free demo share this editor projection.
pub(super) fn render_client_editor(frame: &mut Frame, ui: &mut UIStateGuard, area: Rect) -> Rect {
    let key = match &ui.popup {
        Some(PopupState::ConfigEdit { key, .. }) => key.as_str(),
        _ => "client_id",
    };
    let listenbrainz = matches!(ui.popup, Some(PopupState::ListenBrainzToken { .. }));
    let youtube = key.starts_with("welcome.youtube.");
    let import = key == "welcome.youtube.cookies";
    let spotify = !listenbrainz && !youtube;
    // The Spotify guide carries the redirect URI the dashboard must list.
    let height = if import {
        15
    } else if spotify {
        11
    } else {
        8
    };
    let chunks = Layout::vertical([Constraint::Fill(1), Constraint::Length(height)]).split(area);
    if chunks[1].height == 0 {
        return chunks[0];
    }
    ui.popup_rect = chunks[1];
    let inner = popup_surface::render(
        if listenbrainz {
            "ListenBrainz user token"
        } else if import {
            "Import YouTube cookies"
        } else if youtube {
            "Browser path"
        } else {
            "Spotify client ID"
        },
        &ui.theme,
        Borders::ALL,
        frame,
        chunks[1],
    );
    let input_rect = Rect::new(inner.x, inner.y, inner.width, 1);
    if let Some(PopupState::ConfigEdit { input, .. }) = &ui.popup {
        frame.render_widget(input.widget(true), input_rect);
    }
    if let Some(PopupState::ListenBrainzToken { input }) = &ui.popup {
        frame.render_widget(input.widget(), input_rect);
    }
    let guide: Cow<'static, str> = if spotify {
        Cow::Owned(format!(
            "Application ID only, no client secret. In the Spotify dashboard, add {} to \
             the app's Redirect URIs exactly as shown. http://127.0.0.1 is recommended: \
             sign-in then completes automatically.",
            spotify_redirect_uri()
        ))
    } else if listenbrainz {
        Cow::Borrowed("Copy your User Token from listenbrainz.org/settings/. Validation saves it locally. This optional step does not enable background integrations.")
    } else if import {
        Cow::Borrowed("In your signed-in browser: music.youtube.com > F12 > Network > reload > youtubei request > Headers. Save the Cookie header value to a text file. Enter its path above. Netscape YouTube exports also work. Keep cookies private. Import checks the account; playback is separate.")
    } else {
        Cow::Borrowed("Enter the executable path for Helium, Chrome, Chromium, or Edge. No quotes are needed. This only selects the browser; Sign in opens a separate profile.")
    };
    let notice = if listenbrainz {
        ui.welcome_listenbrainz_notice.as_deref()
    } else if youtube {
        ui.welcome_youtube_notice.as_deref()
    } else {
        ui.welcome_spotify_notice.as_deref()
    };
    let mut lines = Vec::new();
    if let Some(notice) = notice {
        lines.push(Line::styled(notice.to_owned(), ui.theme.workspace_base()));
    }
    lines.push(Line::styled(guide, ui.theme.workspace_secondary_text()));
    frame.render_widget(
        Paragraph::new(lines).wrap(Wrap { trim: true }),
        Rect::new(
            inner.x,
            inner.y.saturating_add(1),
            inner.width,
            inner.height.saturating_sub(2),
        ),
    );
    let confirm = if listenbrainz {
        "Validate"
    } else if import {
        "Import"
    } else {
        "Save"
    };
    let labels = [format!("{confirm} (Enter)"), "Cancel (Esc)".to_owned()];
    let band = Rect::new(inner.x, inner.bottom().saturating_sub(1), inner.width, 1);
    let (save, cancel) = if let Some(&[save, cancel]) =
        horizontal_buttons(&[&labels[0], &labels[1]], band).as_deref()
    {
        (save, cancel)
    } else {
        let halves = Layout::horizontal([Constraint::Fill(1), Constraint::Fill(1)]).split(band);
        (halves[0], halves[1])
    };
    draw_button(frame, ui, save, &labels[0], false, true);
    draw_button(frame, ui, cancel, &labels[1], false, true);
    ui.workspace_hits.extend([
        (input_rect, WorkspaceHit::WelcomeEditorInput),
        (save, WorkspaceHit::WelcomeEditorConfirm),
        (cancel, WorkspaceHit::WelcomeEditorCancel),
    ]);
    chunks[0]
}

/// Action rows drawn with the active selection treatment in the last frame.
#[cfg(test)]
pub(super) fn focused_rows(
    ui: &crate::state::UIState,
    buffer: &ratatui::buffer::Buffer,
) -> Vec<usize> {
    let selection = ui.theme.workspace_selection_active();
    assert!(
        selection.bg.is_some() || !selection.add_modifier.is_empty(),
        "the selection treatment must be observable"
    );
    ui.workspace_hits
        .iter()
        .filter_map(|(rect, hit)| {
            let WorkspaceHit::WelcomeAction(index) = hit else {
                return None;
            };
            let cell = buffer.cell((rect.x, rect.y))?;
            (cell.modifier.contains(selection.add_modifier)
                && selection.bg.is_none_or(|bg| cell.bg == bg))
            .then_some(*index)
        })
        .collect()
}
