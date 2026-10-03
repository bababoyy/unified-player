use super::{
    components::{
        history::JournalListRowProjection, list::record_visible_row_hits,
        popup_surface::render as construct_and_render_block,
    },
    config, format_rating, utils, Block, Borders, Cell, Constraint, Frame, Layout, ListState,
    Paragraph, PlaylistCreateCurrentField, PlaylistPopupAction, PopupActionEntry, PopupState, Rect,
    Row, SharedState, Table, UIStateGuard, UiViewStatus, Wrap,
};
use crate::state::{ActionListItem, BidiDisplay, PopupActionPayload, YouTubePlaylistPopupAction};
use crate::utils::filtered_items_from_query;

use ratatui::{
    style::{Color, Style},
    text::{Line, Span},
};

const SHORTCUT_TABLE_N_COLUMNS: usize = 3;
const SHORTCUT_TABLE_CONSTRAINS: [Constraint; SHORTCUT_TABLE_N_COLUMNS] =
    [Constraint::Ratio(1, 3); 3];

fn popup_bidi_label(value: &str) -> String {
    utils::to_bidi_string(value)
}

/// Version-control row color for one sync change: added rows read green,
/// removed rows red, modified rows yellow, and anything conflicted bold red
/// through the configurable `sync_*` theme styles.
fn listenbrainz_change_style(
    row: &crate::state::ListenBrainzSyncDetailRow,
    theme: &config::Theme,
) -> Style {
    if row.conflict.is_some() || row.action == crate::state::ListenBrainzSyncDetailAction::Conflict
    {
        theme.sync_conflict()
    } else {
        match row.action {
            crate::state::ListenBrainzSyncDetailAction::Added => theme.sync_clean(),
            crate::state::ListenBrainzSyncDetailAction::Removed => theme.sync_conflict(),
            _ => theme.sync_changed(),
        }
    }
}

fn listenbrainz_change_widths(narrow: bool) -> Vec<Constraint> {
    if narrow {
        vec![
            Constraint::Length(11),
            Constraint::Length(10),
            Constraint::Min(8),
            Constraint::Length(12),
        ]
    } else {
        vec![
            Constraint::Length(11),
            Constraint::Length(10),
            Constraint::Percentage(26),
            Constraint::Percentage(22),
            Constraint::Length(10),
            Constraint::Length(16),
        ]
    }
}

/// Shared pending-change rows for the details popup and the workspace
/// changes pane. Columns match at both surfaces; narrow terminals drop to
/// the essential columns with artist/provider in the selected-row footer.
fn listenbrainz_detail_rows(
    preview: &crate::state::ListenBrainzSyncPreview,
    narrow: bool,
    theme: &config::Theme,
) -> Vec<Row<'static>> {
    preview
        .rows
        .iter()
        .map(|row| {
            let conflict = row.conflict.map_or("", |kind| kind.label());
            let style = listenbrainz_change_style(row, theme);
            if narrow {
                Row::new([
                    Cell::from(row.side.label()),
                    Cell::from(row.action.label()),
                    Cell::from(popup_bidi_label(&row.title)),
                    Cell::from(conflict),
                ])
                .style(style)
            } else {
                Row::new([
                    Cell::from(row.side.label()),
                    Cell::from(row.action.label()),
                    Cell::from(popup_bidi_label(&row.title)),
                    Cell::from(popup_bidi_label(&row.artist)),
                    Cell::from(row.provider.clone()),
                    Cell::from(conflict),
                ])
                .style(style)
            }
        })
        .collect()
}

/// Split a sync summary into lines with the leading status colored through
/// the theme. Only the first line carries the status segment; every other
/// line renders plain so wrapping never detaches the color from its label.
fn styled_summary_lines(text: &str, status_label: &str, status_style: Style) -> Vec<Line<'static>> {
    text.lines()
        .enumerate()
        .map(|(index, line)| {
            if index == 0 {
                if let Some(rest) = line.strip_prefix("ListenBrainz: ") {
                    if let Some(tail) = rest.strip_prefix(status_label) {
                        return Line::from(vec![
                            Span::raw("ListenBrainz: "),
                            Span::styled(status_label.to_owned(), status_style),
                            Span::raw(tail.to_owned()),
                        ]);
                    }
                }
            }
            Line::raw(line.to_owned())
        })
        .collect()
}

fn popup_bidi_bounded(value: &str, maximum: usize) -> String {
    utils::bounded_text(&popup_bidi_label(value), maximum)
}

fn popup_bounded_label(value: &str, total_width: u16) -> String {
    popup_bidi_bounded(value, popup_item_width(total_width))
}

fn unified_playlist_popup_label(name: &str, total_width: u16) -> String {
    const PREFIX: &str = "[Unified] ";
    let name_width = popup_item_width(total_width)
        .saturating_sub(PREFIX.chars().count())
        .min(80);
    format!("{PREFIX}{}", popup_bidi_bounded(name, name_width))
}

fn popup_search_line(query: &str) -> String {
    format!("🔍 {}", popup_bidi_label(query))
}

fn popup_query_line(query: &str) -> String {
    format!("/{}", popup_bidi_label(query))
}

fn popup_confirmation_line(message: &str) -> String {
    format!("{} (y/n)", popup_bidi_label(message))
}

fn popup_wrapped_input_height(line: &str, width: u16) -> u16 {
    let content_width = width.saturating_sub(2).max(1);
    let line_count = Paragraph::new(line)
        .wrap(Wrap { trim: false })
        .line_count(content_width)
        .clamp(1, 3) as u16;
    line_count.saturating_add(2)
}

fn popup_search_height(query: &str, width: u16) -> u16 {
    popup_wrapped_input_height(&popup_search_line(query), width)
}

/// Keep device rows focused on the name users recognize. Device ids are only
/// useful when two devices share that name, so expose a short suffix then
/// instead of making every row look like an internal diagnostic entry.
fn device_popup_items(
    devices: &[crate::state::Device],
    current_device_id: &str,
) -> Vec<(String, bool)> {
    let mut name_counts = std::collections::HashMap::<&str, usize>::new();
    for device in devices {
        *name_counts.entry(device.name.as_str()).or_default() += 1;
    }

    devices
        .iter()
        .map(|device| {
            let mut label = popup_bidi_label(&device.name);
            if name_counts
                .get(device.name.as_str())
                .copied()
                .unwrap_or_default()
                > 1
            {
                let suffix = device
                    .id
                    .chars()
                    .rev()
                    .take(8)
                    .collect::<String>()
                    .chars()
                    .rev()
                    .collect::<String>();
                if !suffix.is_empty() {
                    label.push_str(" [...");
                    label.push_str(&suffix);
                    label.push(']');
                }
            }
            if device.is_integrated {
                label.push_str(" (integrated)");
            }
            (label, current_device_id == device.id)
        })
        .collect()
}

fn config_popup_title(key: &str, suffix: Option<&str>) -> String {
    let label = config::setting_label(key);
    match suffix {
        Some(suffix) => format!("{label} {suffix}"),
        None => label,
    }
}

/// Render a popup (if any) to handle a command or show additional information
/// depending on the current popup state.
///
/// The function returns a rectangle area to render the main layout and
/// a boolean value determining whether the focus should be placed in the main layout.
pub fn render_popup(
    frame: &mut Frame,
    state: &SharedState,
    ui: &mut UIStateGuard,
    rect: Rect,
) -> (Rect, bool) {
    ui.popup_rect = Rect::default();
    #[cfg(feature = "private-capture")]
    if let Some(PopupState::PrivateCaptureSelector { state: selector }) = ui.popup.as_mut() {
        selector.synchronize(&state.private_capture_operator_snapshot().artifacts);
    }
    #[cfg(feature = "private-capture")]
    if let Some(PopupState::PrivateDerivativePreview {
        preview,
        scroll_offset,
        rendered_row_count,
    }) = ui.popup.as_mut()
    {
        *rendered_row_count =
            private_derivative_rendered_rows(preview, rect.width.saturating_sub(2));
        *scroll_offset = (*scroll_offset).min(rendered_row_count.saturating_sub(1));
    }
    let rendered = match ui.popup {
        None => (rect, true),
        Some(ref popup) => match popup {
            PopupState::PlaylistCreate {
                target,
                public,
                name,
                desc,
                current_field,
                ..
            } => {
                let chunks =
                    Layout::vertical([Constraint::Min(0), Constraint::Length(3)]).split(rect);

                let popup_chunks = playlist_create_field_rects(chunks[1]);

                let target_title = if *current_field == PlaylistCreateCurrentField::Target {
                    "Destination (left/right, space visibility)"
                } else {
                    "Destination"
                };
                let target_input = construct_and_render_block(
                    target_title,
                    &ui.theme,
                    Borders::ALL,
                    frame,
                    popup_chunks[0],
                );
                let target_label = if *target == crate::state::PlaylistCreateTarget::Unified {
                    format!("{} · local", target.label())
                } else {
                    format!(
                        "{} · {}",
                        target.label(),
                        if *public { "public" } else { "private" }
                    )
                };
                frame.render_widget(Paragraph::new(target_label), target_input);

                let name_input = construct_and_render_block(
                    "Name",
                    &ui.theme,
                    Borders::ALL,
                    frame,
                    popup_chunks[1],
                );

                let description_title = if target.supports_description() {
                    "Description"
                } else {
                    "Description (Spotify only)"
                };
                let desc_input = construct_and_render_block(
                    description_title,
                    &ui.theme,
                    Borders::ALL,
                    frame,
                    popup_chunks[2],
                );

                frame.render_widget(
                    name.widget(PlaylistCreateCurrentField::Name == *current_field),
                    name_input,
                );
                if target.supports_description() {
                    frame.render_widget(
                        desc.widget(PlaylistCreateCurrentField::Desc == *current_field),
                        desc_input,
                    );
                } else {
                    frame.render_widget(
                        Paragraph::new("Not used for this destination.")
                            .style(ui.theme.secondary_row())
                            .wrap(Wrap { trim: false }),
                        desc_input,
                    );
                }
                (chunks[0], true)
            }
            PopupState::SessionHistoryCreate { items, input } => {
                let chunks =
                    Layout::vertical([Constraint::Fill(0), Constraint::Length(3)]).split(rect);
                let title = format!(
                    "Unified Playlist from {} History Item{} (enter to save)",
                    items.len(),
                    if items.len() == 1 { "" } else { "s" }
                );
                let input_rect =
                    construct_and_render_block(&title, &ui.theme, Borders::ALL, frame, chunks[1]);
                frame.render_widget(input.widget(true), input_rect);
                (chunks[0], false)
            }
            PopupState::TrackNote { input, .. } => {
                let chunks =
                    Layout::vertical([Constraint::Fill(0), Constraint::Length(3)]).split(rect);

                let note_rect = construct_and_render_block(
                    "Track Note (enter to save)",
                    &ui.theme,
                    Borders::ALL,
                    frame,
                    chunks[1],
                );

                frame.render_widget(input.widget(true), note_rect);
                (chunks[0], false)
            }
            PopupState::JournalListName { action, input } => {
                let chunks =
                    Layout::vertical([Constraint::Fill(0), Constraint::Length(3)]).split(rect);

                let title = match action {
                    crate::state::JournalListNameAction::Create
                    | crate::state::JournalListNameAction::CreateWithTracks { .. }
                    | crate::state::JournalListNameAction::CreateWithYouTubeTracks { .. } => {
                        "New Journal List (enter to save)"
                    }
                    crate::state::JournalListNameAction::Rename { .. } => {
                        "Rename Journal List (enter to save)"
                    }
                };
                let name_rect =
                    construct_and_render_block(title, &ui.theme, Borders::ALL, frame, chunks[1]);

                frame.render_widget(input.widget(true), name_rect);
                (chunks[0], false)
            }
            PopupState::PlaylistName { action, input } => {
                let chunks =
                    Layout::vertical([Constraint::Fill(0), Constraint::Length(3)]).split(rect);
                let title = match action {
                    crate::state::PlaylistNameAction::Spotify { .. } => {
                        "Rename Spotify Playlist (enter to save)"
                    }
                    crate::state::PlaylistNameAction::YouTubeMusic { .. } => {
                        "Rename YouTube Music Playlist (enter to save)"
                    }
                    crate::state::PlaylistNameAction::Unified { .. } => {
                        "Rename Unified Playlist (enter to save)"
                    }
                };
                let name_rect =
                    construct_and_render_block(title, &ui.theme, Borders::ALL, frame, chunks[1]);
                frame.render_widget(input.widget(true), name_rect);
                (chunks[0], false)
            }
            PopupState::ListenBrainzPlaylists { .. } => {
                let remaining = render_listenbrainz_playlist_picker(frame, ui, rect);
                if ui.popup_rect.is_empty() {
                    ui.popup_rect = popup_rect_from_remaining(rect, remaining);
                }
                return (remaining, false);
            }
            PopupState::ListenBrainzToken { .. } => {
                let remaining = super::welcome::render_client_editor(frame, ui, rect);
                return (remaining, false);
            }
            PopupState::ConfigEdit { key, input } => {
                if (key == "client_id" || key.starts_with("welcome.youtube."))
                    && matches!(ui.current_page(), crate::state::PageState::Welcome { .. })
                {
                    let remaining = super::welcome::render_client_editor(frame, ui, rect);
                    return (remaining, false);
                }
                let chunks =
                    Layout::vertical([Constraint::Fill(0), Constraint::Length(3)]).split(rect);
                let input_rect = construct_and_render_block(
                    &config_popup_title(key, Some("(enter to save)")),
                    &ui.theme,
                    Borders::ALL,
                    frame,
                    chunks[1],
                );

                frame.render_widget(input.widget(true), input_rect);
                (chunks[0], false)
            }
            PopupState::Search { query } => {
                let chunks = Layout::vertical([
                    Constraint::Fill(0),
                    Constraint::Length(popup_wrapped_input_height(
                        &popup_query_line(query),
                        rect.width,
                    )),
                ])
                .split(rect);

                let rect =
                    construct_and_render_block("Search", &ui.theme, Borders::ALL, frame, chunks[1]);

                frame.render_widget(
                    Paragraph::new(popup_query_line(query)).wrap(Wrap { trim: false }),
                    rect,
                );
                (chunks[0], true)
            }
            PopupState::DeferredAction { title, message } => {
                let detail = format!(
                    "{}\n\nPress Enter or Esc to close.",
                    popup_bidi_label(message)
                );
                let height = popup_detail_height(&detail, rect.width, rect.height, 5);
                let chunks =
                    Layout::vertical([Constraint::Fill(0), Constraint::Length(height)]).split(rect);
                let detail_rect = construct_and_render_block(
                    &utils::bounded_text(title, rect.width.saturating_sub(4) as usize),
                    &ui.theme,
                    Borders::ALL,
                    frame,
                    chunks[1],
                );
                frame.render_widget(
                    Paragraph::new(detail).wrap(Wrap { trim: false }),
                    detail_rect,
                );
                (chunks[0], false)
            }
            PopupState::SpotifyUserSearch { query } => {
                let detail = format!(
                    "Looking up {}...\n\nPress Esc to cancel.",
                    popup_bidi_bounded(query, rect.width.saturating_sub(8) as usize)
                );
                let height = popup_detail_height(&detail, rect.width, rect.height, 5);
                let chunks =
                    Layout::vertical([Constraint::Fill(0), Constraint::Length(height)]).split(rect);
                let detail_rect = construct_and_render_block(
                    "Spotify User Search",
                    &ui.theme,
                    Borders::ALL,
                    frame,
                    chunks[1],
                );
                frame.render_widget(
                    Paragraph::new(detail).wrap(Wrap { trim: false }),
                    detail_rect,
                );
                (chunks[0], false)
            }
            PopupState::SpotifyUserCandidates {
                query, candidates, ..
            } => {
                let items = candidates
                    .iter()
                    .map(|candidate| {
                        let name = candidate
                            .profile
                            .display_name
                            .as_deref()
                            .unwrap_or("Unnamed user");
                        (
                            format!(
                                "{} ({}) - {} matching public playlist(s)",
                                popup_bidi_bounded(name, rect.width.saturating_sub(34) as usize,),
                                utils::bounded_text(
                                    &candidate.profile.id,
                                    rect.width.saturating_sub(34) as usize
                                ),
                                candidate.playlists.len()
                            ),
                            false,
                        )
                    })
                    .collect();
                let title = format!(
                    "Spotify users matching {} (playlist owners)",
                    popup_bidi_bounded(query, rect.width.saturating_sub(42) as usize)
                );
                let rect = render_list_popup(frame, rect, &title, items, 9, ui);
                (rect, false)
            }
            PopupState::SpotifyUserPlaylists {
                profile, playlists, ..
            } => {
                let name = profile.display_name.as_deref().unwrap_or("Unnamed user");
                let items = playlists
                    .iter()
                    .map(|playlist| (popup_bounded_label(&playlist.name, rect.width), false))
                    .collect();
                let title = format!(
                    "Public playlists by {} (Esc closes)",
                    utils::bounded_text(
                        &popup_bidi_label(name),
                        rect.width.saturating_sub(33) as usize,
                    )
                );
                let rect = render_list_popup(frame, rect, &title, items, 9, ui);
                (rect, false)
            }
            PopupState::SpotifyUserProfile { profile } => {
                let height = bounded_popup_height(8, rect.height, 9);
                let chunks =
                    Layout::vertical([Constraint::Fill(0), Constraint::Length(height)]).split(rect);
                let detail_rect = construct_and_render_block(
                    "Spotify User",
                    &ui.theme,
                    Borders::ALL,
                    frame,
                    chunks[1],
                );
                let name = profile.display_name.as_deref().unwrap_or("Unnamed user");
                let note = profile.lookup_note.as_deref().unwrap_or("");
                frame.render_widget(
                    Paragraph::new(format!(
                        "{}\nID: {}\n{}\nEnter: open profile\nEsc: close",
                        utils::bounded_text(
                            &popup_bidi_label(name),
                            rect.width.saturating_sub(8) as usize,
                        ),
                        utils::bounded_text(&profile.id, rect.width.saturating_sub(8) as usize),
                        utils::bounded_text(note, rect.width.saturating_sub(8) as usize),
                    ))
                    .wrap(Wrap { trim: false }),
                    detail_rect,
                );
                (chunks[0], false)
            }
            PopupState::UnifiedPlaylistDestination {
                options,
                item_count,
                ..
            } => {
                let mut items: Vec<_> = options
                    .iter()
                    .map(|(name, _)| (unified_playlist_popup_label(name, rect.width), false))
                    .collect();
                items.push((
                    popup_bounded_label("+ Create new playlist…", rect.width),
                    false,
                ));
                let title = format!(
                    "Add {item_count} item{} to Unified Playlist",
                    if *item_count == 1 { "" } else { "s" }
                );
                let rect = render_list_popup(frame, rect, &title, items, 10, ui);
                (rect, false)
            }
            PopupState::ActionList(item, _) => {
                let (title, items) = action_popup_content(item, rect.width);
                let height = bounded_popup_height(items.len() as u16 + 3, rect.height, 4);
                let rect = render_action_list_popup(
                    frame,
                    rect,
                    &title,
                    items,
                    height,
                    ui,
                    "1-9 Run  Enter Select  Esc Back",
                );
                (rect, false)
            }
            PopupState::AnchoredActionList { .. } | PopupState::Volume { .. } => (rect, true),
            PopupState::DiagnosticActions { actions, .. } => {
                let items = popup_action_items(
                    actions.iter().copied().map(PopupActionPayload::Diagnostic),
                    popup_item_width(rect.width),
                );
                let height = bounded_popup_height(actions.len() as u16 + 3, rect.height, 4);
                let rect = render_action_list_popup(
                    frame,
                    rect,
                    "Diagnostic Actions",
                    items,
                    height,
                    ui,
                    "1-9 Run  Enter Inspect  Esc Back",
                );
                (rect, false)
            }
            PopupState::DiagnosticDetail {
                title,
                lines,
                scroll_offset,
            } => {
                let height = bounded_popup_height(lines.len() as u16 + 2, rect.height, 5);
                let chunks =
                    Layout::vertical([Constraint::Fill(0), Constraint::Length(height)]).split(rect);
                let detail_rect =
                    construct_and_render_block(title, &ui.theme, Borders::ALL, frame, chunks[1]);
                frame.render_widget(
                    Paragraph::new(lines.join("\n"))
                        .scroll(((*scroll_offset).min(u16::MAX as usize) as u16, 0))
                        .wrap(Wrap { trim: false }),
                    detail_rect,
                );
                (chunks[0], false)
            }
            PopupState::CommandHelp { .. } => {
                let height = bounded_popup_height(20, rect.height, 5);
                let chunks =
                    Layout::vertical([Constraint::Fill(0), Constraint::Length(height)]).split(rect);
                let mut scroll_offset = match ui.popup.as_ref() {
                    Some(PopupState::CommandHelp { scroll_offset }) => *scroll_offset,
                    _ => 0,
                };
                super::page::render_commands_help_content(frame, ui, chunks[1], &mut scroll_offset);
                if let Some(PopupState::CommandHelp {
                    scroll_offset: popup_offset,
                }) = ui.popup.as_mut()
                {
                    *popup_offset = scroll_offset;
                }
                (chunks[0], false)
            }
            PopupState::ListenBrainzSyncDetails { preview, state } => {
                let height = bounded_popup_height(
                    u16::try_from(preview.rows.len())
                        .unwrap_or(u16::MAX)
                        .saturating_add(4),
                    rect.height,
                    7,
                );
                let chunks =
                    Layout::vertical([Constraint::Fill(0), Constraint::Length(height)]).split(rect);
                let detail_rect = construct_and_render_block(
                    "ListenBrainz Sync Details",
                    &ui.theme,
                    Borders::ALL,
                    frame,
                    chunks[1],
                );
                let narrow = detail_rect.width < 88;
                let detail_chunks =
                    Layout::vertical([Constraint::Min(1), Constraint::Length(u16::from(narrow))])
                        .split(detail_rect);
                let rows = listenbrainz_detail_rows(preview, narrow, &ui.theme);
                let table = if narrow {
                    Table::new(rows, listenbrainz_change_widths(true))
                        .header(Row::new(["Side", "Action", "Title", "Conflict"]))
                } else {
                    Table::new(rows, listenbrainz_change_widths(false)).header(Row::new([
                        "Side", "Action", "Title", "Artist", "Provider", "Conflict",
                    ]))
                }
                .row_highlight_style(ui.theme.selection(true));
                // Seed the table viewport from the popup's persistent list state
                // and write the offset back, so scrolling a long preview is
                // stable across frames instead of resetting to the top.
                let mut table_state = ratatui::widgets::TableState::default();
                table_state.select(state.selected());
                *table_state.offset_mut() = state.offset();
                utils::render_table_window_with_scrollbar(
                    frame,
                    table,
                    detail_chunks[0],
                    preview.rows.len(),
                    &mut table_state,
                );
                let restored_offset = table_state.offset();
                if narrow {
                    let selected = state
                        .selected()
                        .and_then(|index| preview.rows.get(index))
                        .map_or_else(
                            || "Artist: unavailable | Provider: unknown".to_owned(),
                            |row| {
                                format!(
                                    "Artist: {} | Provider: {}",
                                    if row.artist.is_empty() {
                                        "unavailable"
                                    } else {
                                        row.artist.as_str()
                                    },
                                    row.provider
                                )
                            },
                        );
                    frame.render_widget(
                        Paragraph::new(utils::bounded_text(
                            &popup_bidi_label(&selected),
                            detail_chunks[1].width as usize,
                        )),
                        detail_chunks[1],
                    );
                }
                if let Some(PopupState::ListenBrainzSyncDetails { state, .. }) = ui.popup.as_mut() {
                    *state.offset_mut() = restored_offset;
                }
                (chunks[0], false)
            }
            PopupState::ListenBrainzWorkspace {
                playlist_id,
                playlist_name,
                ..
            } => {
                // Measure against the popup's inner width (borders consume two
                // columns), not the full screen, so the single/multi-line
                // breakpoint and wrapping match what is actually rendered.
                let inner_width = rect.width.saturating_sub(2).max(1);
                let (summary_text, status_label, status_style, in_sync, preview, items) = {
                    let data = state.data.read();
                    let playlist = data
                        .unified_playlists
                        .iter()
                        .find(|playlist| &playlist.id == playlist_id);
                    let link = data
                        .playlist_links
                        .iter()
                        .find(|link| link.unified_playlist_id == *playlist_id);
                    let (lifecycle, preview) = match ui.current_page() {
                        crate::state::PageState::UnifiedPlaylist {
                            id,
                            listenbrainz_sync,
                            listenbrainz_preview,
                            ..
                        } if id == playlist_id => {
                            (*listenbrainz_sync, listenbrainz_preview.clone())
                        }
                        _ => (crate::state::ListenBrainzSyncLifecycle::Idle, None),
                    };
                    let configs = config::get_config();
                    let summary = playlist.map(|playlist| {
                        crate::state::ListenBrainzSyncSummary::project(
                            configs.app_config.listenbrainz.enabled,
                            configs.app_config.listenbrainz.read_only_checking,
                            configs.listenbrainz_token().is_some(),
                            playlist,
                            link,
                            lifecycle,
                        )
                    });
                    let (summary_text, status_label, status_style, in_sync) = summary.map_or_else(
                        || {
                            (
                                "ListenBrainz: playlist unavailable".to_owned(),
                                String::new(),
                                Style::default(),
                                false,
                            )
                        },
                        |summary| {
                            (
                                summary.display_text(inner_width),
                                summary.meaning.label().to_owned(),
                                summary.meaning.status_style(&ui.theme),
                                matches!(
                                    summary.meaning,
                                    crate::state::ListenBrainzSyncMeaning::Clean
                                        | crate::state::ListenBrainzSyncMeaning::BothSame
                                ),
                            )
                        },
                    );
                    let items = popup_action_items(
                        crate::state::LISTENBRAINZ_WORKSPACE_ACTIONS
                            .into_iter()
                            .map(|action| PopupActionPayload::Ordinary(action.descriptor())),
                        popup_item_width(rect.width),
                    );
                    (
                        summary_text,
                        status_label,
                        status_style,
                        in_sync,
                        preview,
                        items,
                    )
                };
                let summary_height = Paragraph::new(summary_text.as_str())
                    .wrap(Wrap { trim: true })
                    .line_count(inner_width)
                    .clamp(2, 4) as u16;
                let change_rows = preview.as_ref().map_or(0, |preview| preview.rows.len());
                let changes_height = if change_rows == 0 {
                    3
                } else {
                    (change_rows as u16 + 2).min(10)
                };
                let height = bounded_popup_height(
                    summary_height + changes_height + items.len() as u16 + 5,
                    rect.height,
                    9,
                );
                let chunks =
                    Layout::vertical([Constraint::Fill(0), Constraint::Length(height)]).split(rect);
                let title = format!(
                    "ListenBrainz: {}",
                    utils::bounded_text(
                        &popup_bidi_label(playlist_name),
                        rect.width.saturating_sub(15) as usize
                    )
                );
                let workspace_rect =
                    construct_and_render_block(&title, &ui.theme, Borders::ALL, frame, chunks[1]);
                let wide = workspace_rect.width >= 88;
                let inner =
                    Layout::vertical([Constraint::Length(summary_height), Constraint::Min(1)])
                        .split(workspace_rect);
                frame.render_widget(
                    Paragraph::new(styled_summary_lines(
                        &summary_text,
                        &status_label,
                        status_style,
                    ))
                    .wrap(Wrap { trim: true }),
                    inner[0],
                );
                let panes = if wide {
                    Layout::horizontal([Constraint::Fill(3), Constraint::Fill(2)]).split(inner[1])
                } else {
                    Layout::vertical([Constraint::Length(changes_height + 1), Constraint::Min(1)])
                        .split(inner[1])
                };
                let changes_rect = construct_and_render_block(
                    &format!("Pending changes ({change_rows})"),
                    &ui.theme,
                    Borders::TOP,
                    frame,
                    panes[0],
                );
                let narrow = changes_rect.width < 88;
                if let Some(preview) = preview.filter(|preview| !preview.rows.is_empty()) {
                    let table = Table::new(
                        listenbrainz_detail_rows(&preview, narrow, &ui.theme),
                        listenbrainz_change_widths(narrow),
                    )
                    .header(if narrow {
                        Row::new(["Side", "Action", "Title", "Conflict"])
                    } else {
                        Row::new(["Side", "Action", "Title", "Artist", "Provider", "Conflict"])
                    })
                    .row_highlight_style(ui.theme.selection(false));
                    utils::render_table_window_with_scrollbar(
                        frame,
                        table,
                        changes_rect,
                        preview.rows.len(),
                        ui.popup
                            .as_mut()
                            .and_then(|popup| match popup {
                                PopupState::ListenBrainzWorkspace { changes, .. } => Some(changes),
                                _ => None,
                            })
                            .expect("workspace popup carries changes state"),
                    );
                } else {
                    frame.render_widget(
                        Paragraph::new(if in_sync {
                            "Everything is in sync."
                        } else {
                            "No pending changes previewed yet — choose “Check for changes”."
                        })
                        .wrap(Wrap { trim: true }),
                        changes_rect,
                    );
                }
                let actions_rect = construct_and_render_block(
                    "Operations",
                    &ui.theme,
                    Borders::TOP,
                    frame,
                    panes[1],
                );
                let selected_index = ui.popup.as_ref().and_then(PopupState::list_selected);
                let (list, len) = utils::construct_list_widget_with_width(
                    &ui.theme,
                    items,
                    true,
                    selected_index,
                    Some(popup_item_width(actions_rect.width)),
                    ui.presentation.focused_row_overflow,
                    ui.focused_marquee_phase(),
                );
                utils::render_list_window_with_scrollbar(
                    frame,
                    list.highlight_symbol("> "),
                    actions_rect,
                    len,
                    ui.popup
                        .as_mut()
                        .and_then(PopupState::list_state_mut)
                        .expect("workspace popup carries list state"),
                );
                let popup_rect = chunks[1];
                let footer = Rect {
                    x: popup_rect.x.saturating_add(1),
                    y: popup_rect.bottom().saturating_sub(1),
                    width: popup_rect.width.saturating_sub(2),
                    height: 1.min(popup_rect.height),
                };
                if footer.width > 0 && footer.height > 0 {
                    frame.render_widget(
                        Paragraph::new(utils::bounded_text(
                            "1-9 Run  Enter Select  Esc Back",
                            footer.width as usize,
                        ))
                        .style(ui.theme.block_title()),
                        footer,
                    );
                }
                (chunks[0], false)
            }
            PopupState::ListenBrainzResolve { menu, .. } => {
                let items = (0..menu.row_count())
                    .filter_map(|index| menu.row_label(index))
                    .enumerate()
                    .map(|(index, label)| {
                        let shortcut = action_shortcut(index);
                        (
                            utils::bounded_text(
                                &format!("{shortcut}{label}"),
                                popup_item_width(rect.width),
                            ),
                            false,
                        )
                    })
                    .collect::<Vec<_>>();
                let title = format!(
                    "Resolve ListenBrainz conflicts: {}",
                    utils::bounded_text(
                        &popup_bidi_label(menu.playlist_name()),
                        rect.width.saturating_sub(33) as usize
                    )
                );
                let height = bounded_popup_height(menu.row_count() as u16 + 3, rect.height, 7);
                let rect = render_action_list_popup(
                    frame,
                    rect,
                    &title,
                    items,
                    height,
                    ui,
                    "1-9 Run  Enter Select  Esc Back",
                );
                (rect, false)
            }
            #[cfg(feature = "private-capture")]
            PopupState::PrivateDerivativePreview {
                preview,
                scroll_offset,
                rendered_row_count,
            } => {
                let line_count = u16::try_from(*rendered_row_count).unwrap_or(u16::MAX);
                let height = bounded_popup_height(line_count.saturating_add(2), rect.height, 5);
                let chunks =
                    Layout::vertical([Constraint::Fill(0), Constraint::Length(height)]).split(rect);
                let detail_rect = construct_and_render_block(
                    "Exact Safe Derivative Preview",
                    &ui.theme,
                    Borders::ALL,
                    frame,
                    chunks[1],
                );
                frame.render_widget(
                    Paragraph::new(preview.rendered_text())
                        .scroll(((*scroll_offset).min(u16::MAX as usize) as u16, 0))
                        .wrap(Wrap { trim: false }),
                    detail_rect,
                );
                (chunks[0], false)
            }
            #[cfg(feature = "private-capture")]
            PopupState::PrivateCaptureSelector { .. } => {
                let snapshot = state.private_capture_operator_snapshot();
                let items = snapshot
                    .artifacts
                    .iter()
                    .map(|artifact| {
                        (
                            format!("{} / {}", artifact.capture_ref, artifact.label.as_str()),
                            snapshot.selected == Some(artifact.capture_ref),
                        )
                    })
                    .collect::<Vec<_>>();
                let height = bounded_popup_height(items.len() as u16 + 2, rect.height, 3);
                let rect =
                    render_list_popup(frame, rect, "Select Private Capture", items, height, ui);
                (rect, false)
            }
            #[cfg(feature = "private-capture")]
            PopupState::PrivateCapturePassphrase { action, input } => {
                let chunks =
                    Layout::vertical([Constraint::Fill(0), Constraint::Length(4)]).split(rect);
                let input_rect = construct_and_render_block(
                    action.title(),
                    &ui.theme,
                    Borders::ALL,
                    frame,
                    chunks[1],
                );
                frame.render_widget(
                    Paragraph::new(vec![
                        ratatui::text::Line::raw(masked_passphrase(input.character_count())),
                        ratatui::text::Line::raw(
                            "Enter confirms locally; Esc cancels without retaining input.",
                        ),
                    ])
                    .wrap(Wrap { trim: true }),
                    input_rect,
                );
                (chunks[0], false)
            }
            #[cfg(feature = "private-capture")]
            PopupState::PrivateCaptureConfirm { action } => {
                let message = popup_confirmation_line(action.message());
                let height = popup_detail_height(&message, rect.width, rect.height, 4);
                let chunks =
                    Layout::vertical([Constraint::Fill(0), Constraint::Length(height)]).split(rect);
                let confirm_rect = construct_and_render_block(
                    "Private Capture Confirmation",
                    &ui.theme,
                    Borders::ALL,
                    frame,
                    chunks[1],
                );
                frame.render_widget(
                    Paragraph::new(message).wrap(Wrap { trim: false }),
                    confirm_rect,
                );
                (chunks[0], false)
            }
            PopupState::TrackRating { track, .. } => {
                let current_rating = state
                    .data
                    .read()
                    .journal
                    .entry_for_track(track)
                    .and_then(|entry| entry.rating);
                let mut items = (1..=10)
                    .map(|rating| {
                        (
                            format_rating(rating),
                            current_rating.is_some_and(|current| current == rating),
                        )
                    })
                    .collect::<Vec<_>>();
                items.push(("Clear rating".to_string(), current_rating.is_none()));

                let rect = render_list_popup(frame, rect, "Rate Track", items, 13, ui);
                (rect, false)
            }
            PopupState::JournalListSelect(..) => {
                let items = state
                    .data
                    .read()
                    .journal
                    .lists
                    .iter()
                    .map(|list| {
                        (
                            JournalListRowProjection::from_list(list).popup_label(),
                            false,
                        )
                    })
                    .collect();

                let rect = render_list_popup(frame, rect, "Journal Lists", items, 10, ui);
                (rect, false)
            }
            PopupState::DeviceList { .. } => {
                let player = state.player.read();

                let current_device_id = player
                    .current_playback()
                    .and_then(|playback| playback.device.id);
                let items = device_popup_items(
                    &player.devices,
                    current_device_id.as_deref().unwrap_or_default(),
                );

                let rect = render_list_popup(frame, rect, "Devices", items, 5, ui);
                (rect, false)
            }
            PopupState::ThemeList(..) => (render_theme_picker(frame, rect, ui), false),
            PopupState::ConfigChoice { key, options, .. } => {
                let title = config_popup_title(key, None);
                let current = match ui.current_page() {
                    crate::state::PageState::Settings { settings, .. } => settings
                        .iter()
                        .find(|setting| setting.key == *key)
                        .map(|setting| setting.value.trim_matches('"').to_string())
                        .unwrap_or_default(),
                    _ => String::new(),
                };
                let items = options
                    .iter()
                    .map(|option| (popup_bidi_label(option), *option == current))
                    .collect();

                let rect = render_list_popup(frame, rect, &title, items, 10, ui);
                (rect, false)
            }
            PopupState::ConfigMultiChoice {
                key,
                options,
                selected,
                ..
            } => {
                let items = options
                    .iter()
                    .zip(selected)
                    .map(|(option, selected)| {
                        let mark = if *selected { "[x]" } else { "[ ]" };
                        (format!("{mark} {}", popup_bidi_label(option)), false)
                    })
                    .collect();

                let title =
                    config_popup_title(key, Some("(enter toggle, backspace save, esc cancel)"));
                let rect = render_list_popup(frame, rect, &title, items, 10, ui);
                (rect, false)
            }
            PopupState::UserPlaylistList(..) => {
                (render_user_playlist_popup(frame, state, ui, rect), false)
            }
            PopupState::YouTubePlaylistList(action, _) => {
                let search_query = match action {
                    YouTubePlaylistPopupAction::AddTrack { search_query, .. }
                    | YouTubePlaylistPopupAction::AddTracks { search_query, .. }
                    | YouTubePlaylistPopupAction::LinkUnified { search_query, .. } => search_query,
                };
                let query = search_query.to_lowercase();
                let linking_unified =
                    matches!(action, YouTubePlaylistPopupAction::LinkUnified { .. });
                // Project owned labels while the lock is held, then release
                // it before the popup layout and terminal draw.
                let display_items = {
                    let data = state.data.read();
                    let mut display_items = data
                        .user_data
                        .youtube_library
                        .playlists
                        .iter()
                        .filter(|playlist| {
                            query.is_empty() || playlist.name.to_lowercase().contains(&query)
                        })
                        .map(|playlist| {
                            (
                                popup_bounded_label(
                                    &format!("{} ({})", playlist.name, playlist.author),
                                    rect.width,
                                ),
                                false,
                            )
                        })
                        .collect::<Vec<_>>();
                    if !linking_unified {
                        display_items.extend(
                            data.unified_playlists
                                .iter()
                                .filter(|playlist| {
                                    query.is_empty()
                                        || playlist.name.to_lowercase().contains(&query)
                                })
                                .map(|playlist| {
                                    (
                                        unified_playlist_popup_label(&playlist.name, rect.width),
                                        false,
                                    )
                                }),
                        );
                        display_items.push((
                            popup_bounded_label("+ Create new playlist…", rect.width),
                            false,
                        ));
                    }
                    display_items
                };

                let chunks = Layout::vertical([
                    Constraint::Length(popup_search_height(search_query, rect.width)),
                    Constraint::Fill(0),
                    Constraint::Length(10),
                ])
                .split(rect);
                let search_rect = construct_and_render_block(
                    "Search YouTube Playlists (type to search, backspace on empty to close)",
                    &ui.theme,
                    Borders::ALL,
                    frame,
                    chunks[0],
                );
                frame.render_widget(
                    Paragraph::new(popup_search_line(search_query)).wrap(Wrap { trim: false }),
                    search_rect,
                );
                let rect = render_list_popup(
                    frame,
                    chunks[2],
                    if linking_unified {
                        "Link Unified Playlist to YouTube"
                    } else {
                        "YouTube Playlists"
                    },
                    display_items,
                    10,
                    ui,
                );
                (rect, false)
            }
            PopupState::YouTubeArtistMenu { details, .. } => {
                let mut items = vec![
                    (
                        popup_bounded_label(
                            if details.subscribed {
                                "Unsubscribe"
                            } else {
                                "Subscribe"
                            },
                            rect.width,
                        ),
                        false,
                    ),
                    (
                        popup_bounded_label(
                            if details.radio_id.is_some() {
                                "Start radio"
                            } else {
                                "Start radio (unavailable)"
                            },
                            rect.width,
                        ),
                        false,
                    ),
                    (popup_bounded_label("Open channel", rect.width), false),
                    (popup_bounded_label("Overview", rect.width), false),
                ];
                if let Some(description) = details.description.as_deref() {
                    if !description.trim().is_empty() {
                        items.push((
                            popup_bounded_label(&format!("Description: {description}"), rect.width),
                            false,
                        ));
                    }
                }
                if let Some(views) = details.views.as_deref() {
                    items.push((
                        popup_bounded_label(&format!("Views: {views}"), rect.width),
                        false,
                    ));
                }
                if let Some(subscribers) = details.subscribers.as_deref() {
                    items.push((
                        popup_bounded_label(&format!("Subscribers: {subscribers}"), rect.width),
                        false,
                    ));
                }
                items.push((
                    popup_bounded_label(&format!("Albums ({})", details.albums.len()), rect.width),
                    false,
                ));
                items.extend(details.albums.iter().map(|release| {
                    (
                        popup_bounded_label(
                            &format!(
                                "  {}{} · {}",
                                release.title,
                                release
                                    .year
                                    .as_deref()
                                    .map(|year| format!(" ({year})"))
                                    .unwrap_or_default(),
                                release.kind
                            ),
                            rect.width,
                        ),
                        false,
                    )
                }));
                items.push((
                    popup_bounded_label(
                        &format!("Singles ({})", details.singles.len()),
                        rect.width,
                    ),
                    false,
                ));
                items.extend(details.singles.iter().map(|release| {
                    (
                        popup_bounded_label(
                            &format!(
                                "  {}{} · {}",
                                release.title,
                                release
                                    .year
                                    .as_deref()
                                    .map(|year| format!(" ({year})"))
                                    .unwrap_or_default(),
                                release.kind
                            ),
                            rect.width,
                        ),
                        false,
                    )
                }));
                items.push((
                    popup_bounded_label(
                        &format!("Related artists ({})", details.related.len()),
                        rect.width,
                    ),
                    false,
                ));
                items.extend(details.related.iter().map(|artist| {
                    (
                        popup_bounded_label(
                            &format!("  {} · {}", artist.name, artist.subscribers),
                            rect.width,
                        ),
                        false,
                    )
                }));
                let title = format!(
                    "YouTube Artist: {}",
                    popup_bidi_bounded(&details.name, rect.width.saturating_sub(24) as usize)
                );
                let rect = render_list_popup(frame, rect, &title, items, 10, ui);
                (rect, false)
            }
            PopupState::UserFollowedArtistList { .. } => {
                let items = state
                    .data
                    .read()
                    .user_data
                    .followed_artists
                    .iter()
                    .map(|a| (a.to_bidi_string(), false))
                    .collect();

                let rect = render_list_popup(frame, rect, "User Followed Artists", items, 7, ui);
                (rect, false)
            }
            PopupState::UserSavedAlbumList { .. } => {
                let items = state
                    .data
                    .read()
                    .user_data
                    .saved_albums
                    .iter()
                    .map(|a| (a.to_bidi_string(), false))
                    .collect();

                let rect = render_list_popup(frame, rect, "User Saved Albums", items, 7, ui);
                (rect, false)
            }
            PopupState::ArtistList(_, artists, ..) => {
                let items = artists
                    .iter()
                    .map(|a| (a.to_bidi_string(), false))
                    .collect();

                let rect = render_list_popup(frame, rect, "Artists", items, 5, ui);
                (rect, false)
            }
            PopupState::ConfirmAction { message, .. } => {
                let chunks = Layout::vertical([
                    Constraint::Fill(0),
                    Constraint::Length(popup_wrapped_input_height(
                        &popup_confirmation_line(message),
                        rect.width,
                    )),
                ])
                .split(rect);

                let confirm_rect = construct_and_render_block(
                    "Confirm",
                    &ui.theme,
                    Borders::ALL,
                    frame,
                    chunks[1],
                );

                frame.render_widget(
                    Paragraph::new(popup_confirmation_line(message)).wrap(Wrap { trim: false }),
                    confirm_rect,
                );

                (chunks[0], true)
            }
            PopupState::WorkspaceScope { .. } => (rect, true),
        },
    };
    if ui.popup.is_some() && ui.popup_rect.is_empty() {
        ui.popup_rect = popup_rect_from_remaining(rect, rendered.0);
    }
    rendered
}

fn popup_rect_from_remaining(area: Rect, remaining: Rect) -> Rect {
    let height = area.height.saturating_sub(remaining.height);
    if height == 0 {
        return Rect::default();
    }
    Rect::new(area.x, remaining.bottom(), area.width, height)
}

fn action_popup_content(item: &ActionListItem, width: u16) -> (String, Vec<(String, bool)>) {
    let descriptors = item.action_descriptors();
    let title_prefix = if !descriptors.is_empty()
        && descriptors
            .iter()
            .all(|descriptor| descriptor.action.is_journal_action())
    {
        "Journal actions"
    } else {
        "Actions"
    };
    let title = format!(
        "{title_prefix}: {}",
        utils::bounded_text(
            &popup_bidi_label(&item.name()),
            width.saturating_sub(12) as usize,
        )
    );
    let items = popup_action_items(
        descriptors
            .iter()
            .copied()
            .map(PopupActionPayload::Ordinary),
        popup_item_width(width),
    );
    (title, items)
}

fn action_popup_dimensions(title: &str, items: &[(String, bool)], body: Rect) -> (u16, u16) {
    let item_width = items
        .iter()
        .map(|(label, _)| label.chars().count())
        .max()
        .unwrap_or_default();
    let popup_width = u16::try_from(
        item_width
            .max(title.chars().count())
            .saturating_add(6)
            .max(18),
    )
    .unwrap_or(u16::MAX)
    .min(body.width);
    let popup_height = bounded_popup_height(items.len() as u16 + 3, body.height, 4);
    (popup_width, popup_height)
}

fn playlist_popup_search_query(action: &PlaylistPopupAction) -> &str {
    match action {
        PlaylistPopupAction::Browse { search_query, .. }
        | PlaylistPopupAction::AddTrack { search_query, .. }
        | PlaylistPopupAction::AddTracks { search_query, .. }
        | PlaylistPopupAction::AddEpisode { search_query, .. } => search_query,
    }
}

fn user_playlist_popup_items(
    data: &crate::state::AppData,
    action: &PlaylistPopupAction,
    width: u16,
) -> Vec<(String, bool)> {
    let search_query = playlist_popup_search_query(action);
    let items = match action {
        PlaylistPopupAction::Browse { folder_id, .. } => {
            data.user_data.folder_playlists_items(*folder_id)
        }
        PlaylistPopupAction::AddTrack { folder_id, .. }
        | PlaylistPopupAction::AddTracks { folder_id, .. }
        | PlaylistPopupAction::AddEpisode { folder_id, .. } => {
            data.user_data.modifiable_playlist_items(Some(*folder_id))
        }
    };

    let filtered_items = filtered_items_from_query(search_query, &items);
    let mut display_items = filtered_items
        .iter()
        .map(|playlist| (popup_bounded_label(&playlist.to_string(), width), false))
        .collect::<Vec<_>>();
    if matches!(
        action,
        PlaylistPopupAction::AddTrack { .. } | PlaylistPopupAction::AddTracks { .. }
    ) {
        let query = search_query.to_lowercase();
        display_items.extend(
            data.unified_playlists
                .iter()
                .filter(|playlist| {
                    query.is_empty() || playlist.name.to_lowercase().contains(&query)
                })
                .map(|playlist| (unified_playlist_popup_label(&playlist.name, width), false)),
        );
        display_items.push((popup_bounded_label("+ Create new playlist…", width), false));
    }
    display_items
}

fn render_user_playlist_popup(
    frame: &mut Frame,
    state: &SharedState,
    ui: &mut UIStateGuard,
    surface: Rect,
) -> Rect {
    let Some(PopupState::UserPlaylistList(action, _)) = ui.popup.as_ref() else {
        return surface;
    };
    if surface.width < 8 || surface.height < 6 {
        return Rect::default();
    }
    let search_query = playlist_popup_search_query(action);
    let display_items = {
        let data = state.data.read();
        user_playlist_popup_items(&data, action, surface.width)
    };
    let chunks = Layout::vertical([
        Constraint::Length(popup_search_height(search_query, surface.width)),
        Constraint::Fill(0),
        Constraint::Length(10),
    ])
    .split(surface);

    // This modal owns the whole surface it receives. Filling it before the
    // child panels are drawn prevents stale sidebar text from showing through
    // the unconstrained middle area between the search and playlist panes.
    frame.render_widget(
        Block::default().style(ui.theme.workspace_elevated_surface()),
        surface,
    );
    let search_rect = construct_and_render_block(
        "Search Playlists (type to search, backspace on empty to close)",
        &ui.theme,
        Borders::ALL,
        frame,
        chunks[0],
    );
    frame.render_widget(
        Paragraph::new(popup_search_line(search_query)).wrap(Wrap { trim: false }),
        search_rect,
    );
    render_list_popup(frame, chunks[2], "User Playlists", display_items, 10, ui);
    ui.popup_rect = surface;
    Rect::default()
}

/// Render the user-playlist browser over the workspace content panel only.
/// The global navigation rail remains visible and is never painted over by
/// the modal or left exposed as stale text above it.
pub(crate) fn render_workspace_user_playlist_popup(
    frame: &mut Frame,
    state: &SharedState,
    ui: &mut UIStateGuard,
) {
    let surface = ui.workspace_layout.content;
    if surface.is_empty() {
        return;
    }
    render_user_playlist_popup(frame, state, ui, surface);
}

/// Render the workspace scope selector after the page so its anchored panel
/// remains an overlay without shrinking or repainting the underlying layout.
pub(crate) fn render_workspace_scope_popup(frame: &mut Frame, ui: &mut UIStateGuard, body: Rect) {
    let (kind, options, anchor, selected_index) = match ui.popup.as_ref() {
        Some(PopupState::WorkspaceScope {
            kind,
            options,
            state,
            anchor,
        }) => (*kind, options.clone(), *anchor, state.selected()),
        _ => return,
    };
    if body.width < 8 || body.height < 3 || options.is_empty() {
        return;
    }

    let title = kind.title();
    let content_width = options
        .iter()
        .map(|option| option.label.chars().count())
        .max()
        .unwrap_or_default()
        .max(title.chars().count())
        .saturating_add(4) as u16;
    let popup_width = content_width.max(18).min(body.width);
    let popup_height = (options.len() as u16).saturating_add(2).min(body.height);
    if popup_height < 3 {
        return;
    }

    let max_x = body.right().saturating_sub(popup_width);
    let x = anchor.x.min(max_x);
    let below = anchor.bottom();
    let y = if below.saturating_add(popup_height) <= body.bottom() {
        below
    } else {
        anchor.y.saturating_sub(popup_height)
    }
    .max(body.y)
    .min(body.bottom().saturating_sub(popup_height));
    let popup_rect = Rect::new(x, y, popup_width, popup_height);
    ui.popup_rect = popup_rect;
    let inner = construct_and_render_block(title, &ui.theme, Borders::ALL, frame, popup_rect);
    if inner.is_empty() {
        return;
    }

    let items = options
        .iter()
        .map(|option| (popup_bidi_label(&option.label), false))
        .collect::<Vec<_>>();
    let items = popup_list_items(
        items,
        popup_rect.width,
        selected_index,
        config::get_config()
            .app_config
            .presentation
            .focused_row_overflow,
        config::get_config().app_config.enable_relative_line_number,
    );
    let (list, len) = utils::construct_list_widget_with_width(
        &ui.theme,
        items,
        true,
        selected_index,
        Some(popup_item_width(popup_rect.width)),
        ui.presentation.focused_row_overflow,
        ui.focused_marquee_phase(),
    );
    let Some(PopupState::WorkspaceScope { state, .. }) = ui.popup.as_mut() else {
        return;
    };
    utils::render_list_window(frame, list, inner, len, state);
    let start = state.offset();
    let end = start
        .saturating_add(inner.height as usize)
        .min(options.len());
    for index in start..end {
        ui.workspace_popup_hits.push((
            Rect::new(
                inner.x,
                inner.y.saturating_add((index - start) as u16),
                inner.width,
                1,
            ),
            index,
        ));
    }
}

/// Render a row action menu below its source row without changing the page's
/// layout. The event layer owns the anchor; this renderer only clamps it to
/// the current body and flips above the row when the footer leaves no room.
pub(crate) fn render_workspace_anchored_action_popup(
    frame: &mut Frame,
    ui: &mut UIStateGuard,
    body: Rect,
) {
    let (item, anchor) = match ui.popup.as_ref() {
        Some(PopupState::AnchoredActionList { item, anchor, .. }) => (item.clone(), *anchor),
        _ => return,
    };
    if body.width < 8 || body.height < 3 {
        return;
    }

    let (title, items) = action_popup_content(&item, body.width);
    let (popup_width, height) = action_popup_dimensions(&title, &items, body);
    if height < 3 {
        return;
    }
    let max_x = body.right().saturating_sub(popup_width);
    let x = anchor.x.max(body.x).min(max_x);
    let below = anchor.bottom();
    let y = if below.saturating_add(height) <= body.bottom() {
        below
    } else {
        anchor.y.saturating_sub(height)
    }
    .max(body.y)
    .min(body.bottom().saturating_sub(height));
    let popup_rect = Rect::new(x, y, popup_width, height);
    render_action_list_popup_at_rect(
        frame,
        popup_rect,
        &title,
        items,
        ui,
        "1-9 Run  Enter Select  Esc Back",
    );
}

const VOLUME_POPUP_WIDTH: u16 = 46;
const VOLUME_POPUP_HEIGHT: u16 = 6;

/// Fine volume control beside its anchor: a long slider with the percentage,
/// a field for typing one, and the keys. The slider is recorded as popup hit
/// 0 so the pointer can set the volume on it.
pub(crate) fn render_volume_popup(
    frame: &mut Frame,
    state: &SharedState,
    ui: &mut UIStateGuard,
    body: Rect,
) {
    let Some(PopupState::Volume { anchor, input }) = ui.popup.as_ref() else {
        return;
    };
    let (anchor, input) = (*anchor, input.clone());
    let width = VOLUME_POPUP_WIDTH.min(body.width);
    if width < 20 || body.height < VOLUME_POPUP_HEIGHT {
        return;
    }
    let x = anchor
        .right()
        .saturating_sub(width)
        .max(body.x)
        .min(body.right().saturating_sub(width));
    let below = anchor.bottom();
    let y = if below.saturating_add(VOLUME_POPUP_HEIGHT) <= body.bottom() {
        below
    } else {
        anchor.y.saturating_sub(VOLUME_POPUP_HEIGHT)
    }
    .max(body.y)
    .min(body.bottom().saturating_sub(VOLUME_POPUP_HEIGHT));
    let popup_rect = Rect::new(x, y, width, VOLUME_POPUP_HEIGHT);
    ui.popup_rect = popup_rect;
    let inner = construct_and_render_block("Volume", &ui.theme, Borders::ALL, frame, popup_rect);
    let inner = Rect::new(
        inner.x.saturating_add(1),
        inner.y,
        inner.width.saturating_sub(2),
        inner.height,
    );

    let volume = {
        let player = state.player.read();
        player.playback_volume(player.effective_playback_provider(ui.active_provider))
    };
    let slider_row = Rect::new(inner.x, inner.y, inner.width, 1);
    match volume {
        Some(volume) => {
            let label = format!(" {volume:>3}%");
            let slider = Rect::new(
                slider_row.x,
                slider_row.y,
                slider_row
                    .width
                    .saturating_sub(label.chars().count() as u16),
                1,
            );
            let filled = u16::try_from(u32::from(slider.width) * u32::from(volume) / 100)
                .unwrap_or(slider.width);
            frame.render_widget(
                Paragraph::new(Line::from(vec![
                    Span::styled(
                        "━".repeat(usize::from(filled)),
                        ui.theme.playback_progress_bar(),
                    ),
                    Span::styled(
                        "━".repeat(usize::from(slider.width - filled)),
                        ui.theme.workspace_progress_remaining(),
                    ),
                    Span::styled(label, ui.theme.workspace_base()),
                ])),
                slider_row,
            );
            ui.workspace_popup_hits.push((slider, 0));
        }
        None => frame.render_widget(
            Paragraph::new("Volume is not available for this playback")
                .style(ui.theme.workspace_secondary_text()),
            slider_row,
        ),
    }

    let field_y = inner.y.saturating_add(2);
    let prompt = "Set to ";
    frame.render_widget(
        Paragraph::new(prompt).style(ui.theme.workspace_secondary_text()),
        Rect::new(inner.x, field_y, inner.width, 1),
    );
    let text_style = ui.theme.workspace_base();
    let cursor_style = Style::default()
        .fg(text_style.bg.unwrap_or(Color::Reset))
        .bg(text_style.fg.unwrap_or(Color::Reset));
    let field_x = inner.x.saturating_add(prompt.len() as u16);
    frame.render_widget(
        input.widget_with_styles(
            true,
            text_style,
            cursor_style,
            "0-100",
            ui.theme.workspace_hint_text(),
        ),
        Rect::new(
            field_x,
            field_y,
            6.min(inner.right().saturating_sub(field_x)),
            1,
        ),
    );
    frame.render_widget(
        Paragraph::new(utils::bounded_text(
            "←→ ±1  ↑↓ ±step  Enter set  Esc close",
            usize::from(inner.width),
        ))
        .style(ui.theme.block_title()),
        Rect::new(inner.x, inner.bottom().saturating_sub(1), inner.width, 1),
    );
}

/// Render a keyboard-opened action list as the same elevated workspace overlay
/// used by anchored context menus. Legacy pages continue using the ordinary
/// popup placement; workspace pages must not fall back to that old layout path.
pub(crate) fn render_workspace_action_popup(frame: &mut Frame, ui: &mut UIStateGuard, body: Rect) {
    let item = match ui.popup.as_ref() {
        Some(PopupState::ActionList(item, _)) => item.clone(),
        _ => return,
    };
    if body.width < 8 || body.height < 3 {
        return;
    }

    let (title, items) = action_popup_content(&item, body.width);
    let (popup_width, height) = action_popup_dimensions(&title, &items, body);
    if height < 3 {
        return;
    }
    let x = body
        .x
        .saturating_add(body.width.saturating_sub(popup_width) / 2);
    let y = body
        .y
        .saturating_add(body.height.saturating_sub(height) / 2);
    render_action_list_popup_at_rect(
        frame,
        Rect::new(x, y, popup_width, height),
        &title,
        items,
        ui,
        "1-9 Run  Enter Select  Esc Back",
    );
}

fn playlist_create_field_rects(rect: Rect) -> [Rect; 3] {
    let chunks = Layout::horizontal([
        Constraint::Percentage(25),
        Constraint::Percentage(37),
        Constraint::Percentage(38),
    ])
    .split(rect);
    [chunks[0], chunks[1], chunks[2]]
}

#[cfg(feature = "private-capture")]
fn masked_passphrase(character_count: usize) -> String {
    const MAX_VISIBLE_MASK: usize = 32;
    let visible = character_count.min(MAX_VISIBLE_MASK);
    let mut mask = "*".repeat(visible);
    if character_count > MAX_VISIBLE_MASK {
        mask.push_str("...");
    }
    mask
}

/// A helper function to render a list popup
fn render_theme_picker(frame: &mut Frame, area: Rect, ui: &mut UIStateGuard) -> Rect {
    let height = bounded_popup_height(20, area.height, 6);
    let surface = Rect::new(
        area.x,
        area.bottom().saturating_sub(height),
        area.width,
        height,
    );
    ui.popup_rect = surface;
    let inner = construct_and_render_block("", &ui.theme, Borders::ALL, frame, surface);
    let padded = Rect::new(
        inner.x.saturating_add(1),
        inner.y,
        inner.width.saturating_sub(2),
        inner.height,
    );
    let sections = Layout::vertical([
        Constraint::Length(2),
        Constraint::Min(1),
        Constraint::Length(1),
    ])
    .split(padded);
    frame.render_widget(
        Paragraph::new("Themes").style(ui.theme.workspace_heading()),
        sections[0],
    );
    frame.render_widget(
        Paragraph::new(utils::bounded_text(
            "↑/↓ Preview · enter Apply · esc Restore",
            usize::from(sections[2].width),
        ))
        .style(ui.theme.workspace_hint_text()),
        sections[2],
    );
    let active_theme = ui.theme.clone();
    let Some(PopupState::ThemeList(themes, state)) = ui.popup.as_mut() else {
        return area;
    };
    utils::adjust_list_offset(state, themes.len(), sections[1].height);
    let selected = state.selected();
    let relative = config::get_config()
        .app_config
        .enable_relative_line_number
        .then_some(selected)
        .flatten();
    let label_width = usize::from(sections[1].width.saturating_sub(19));
    let items = themes
        .iter()
        .enumerate()
        .map(|(index, theme)| {
            let mut style = if selected == Some(index) {
                active_theme.workspace_selection_active()
            } else {
                active_theme.workspace_base()
            };
            style.bg = None;
            let name = popup_bidi_label(&theme.name);
            let label = if index == 0 {
                format!("{name} (current)")
            } else {
                name
            };
            let mut spans = vec![
                Span::styled(if index == 0 { "● " } else { "  " }, style),
                Span::styled(
                    format!("{:>3} ", utils::relative_table_line_number(index, relative)),
                    style,
                ),
            ];
            for color in [
                theme.workspace_base().fg,
                theme.workspace_selection_active().bg,
                theme.workspace_status_success().fg,
            ] {
                spans.push(Span::styled(
                    "██",
                    Style::default().fg(color.unwrap_or(Color::Reset)),
                ));
                spans.push(Span::raw(" "));
            }
            spans.push(Span::styled(
                utils::bounded_text(&label, label_width),
                style,
            ));
            ratatui::widgets::ListItem::new(Line::from(spans))
        })
        .collect::<Vec<_>>();
    let count = items.len();
    let list = ratatui::widgets::List::new(items)
        .style(active_theme.workspace_elevated_surface())
        .highlight_symbol("› ")
        .highlight_spacing(ratatui::widgets::HighlightSpacing::Always)
        .highlight_style(
            Style::default().bg(active_theme
                .workspace_selection_active()
                .bg
                .unwrap_or(Color::Reset)),
        );
    if count == 0 {
        super::render_view_status(frame, &active_theme, UiViewStatus::Empty, sections[1]);
    } else {
        utils::render_list_window(frame, list, sections[1], count, state);
        let start = state.offset();
        record_visible_row_hits(&mut ui.workspace_popup_hits, sections[1], start, count);
    }
    Rect::new(
        area.x,
        area.y,
        area.width,
        area.height.saturating_sub(height),
    )
}

fn render_list_popup(
    frame: &mut Frame,
    rect: Rect,
    title: &str,
    items: Vec<(String, bool)>,
    length: u16,
    ui: &mut UIStateGuard,
) -> Rect {
    let requested = u16::try_from(items.len())
        .unwrap_or(u16::MAX)
        .saturating_add(4)
        .clamp(7, 18)
        .max(length);
    let height = bounded_popup_height(requested, rect.height, 4);
    let chunks = Layout::vertical([Constraint::Fill(0), Constraint::Length(height)]).split(rect);

    ui.popup_rect = chunks[1];
    let inner = construct_and_render_block("", &ui.theme, Borders::ALL, frame, chunks[1]);
    let padded = Rect::new(
        inner.x.saturating_add(1),
        inner.y,
        inner.width.saturating_sub(2),
        inner.height,
    );
    let sections = Layout::vertical([Constraint::Length(2), Constraint::Min(1)]).split(padded);
    frame.render_widget(
        Paragraph::new(title).style(ui.theme.workspace_heading()),
        sections[0],
    );
    let rect = sections[1];
    let popup_width = rect.width;
    let is_empty = items.is_empty();
    let selected_index = ui.popup.as_ref().and_then(PopupState::list_selected);
    let items = popup_list_items(
        items,
        popup_width,
        selected_index,
        config::get_config()
            .app_config
            .presentation
            .focused_row_overflow,
        config::get_config().app_config.enable_relative_line_number,
    );
    let (list, len) = utils::construct_list_widget_with_width(
        &ui.theme,
        items,
        true,
        selected_index,
        Some(popup_item_width(popup_width)),
        ui.presentation.focused_row_overflow,
        ui.focused_marquee_phase(),
    );

    if is_empty {
        super::render_view_status(frame, &ui.theme, UiViewStatus::Empty, rect);
    } else {
        utils::render_list_window(
            frame,
            list,
            rect,
            len,
            ui.popup.as_mut().unwrap().list_state_mut().unwrap(),
        );
        let start = ui
            .popup
            .as_ref()
            .and_then(PopupState::list_state)
            .map(ListState::offset)
            .unwrap_or_default();
        record_visible_row_hits(&mut ui.workspace_popup_hits, rect, start, len);
    }

    chunks[0]
}

/// Render an action list with its interaction legend in the popup border.
/// Keeping the legend out of the list rows preserves the same numbered-row
/// model for ordinary and diagnostic actions, including narrow terminals.
fn render_action_list_popup(
    frame: &mut Frame,
    rect: Rect,
    title: &str,
    items: Vec<(String, bool)>,
    length: u16,
    ui: &mut UIStateGuard,
    legend: &str,
) -> Rect {
    let height = bounded_popup_height(length, rect.height, 3);
    let chunks = Layout::vertical([Constraint::Fill(0), Constraint::Length(height)]).split(rect);
    let popup_rect = chunks[1];
    ui.popup_rect = popup_rect;
    render_action_list_popup_at_rect(frame, popup_rect, title, items, ui, legend);
    chunks[0]
}

fn render_action_list_popup_at_rect(
    frame: &mut Frame,
    popup_rect: Rect,
    title: &str,
    items: Vec<(String, bool)>,
    ui: &mut UIStateGuard,
    legend: &str,
) -> Rect {
    ui.popup_rect = popup_rect;
    let popup_width = popup_rect.width;
    let inner = construct_and_render_block(title, &ui.theme, Borders::ALL, frame, popup_rect);
    let inner_chunks = Layout::vertical([Constraint::Min(0), Constraint::Length(1)]).split(inner);
    let list_rect = inner_chunks[0];
    let footer = inner_chunks[1];
    let is_empty = items.is_empty();
    let selected_index = ui.popup.as_ref().and_then(PopupState::list_selected);
    let items = popup_list_items(
        items,
        popup_width,
        selected_index,
        config::get_config()
            .app_config
            .presentation
            .focused_row_overflow,
        config::get_config().app_config.enable_relative_line_number,
    );
    let (list, len) = utils::construct_list_widget_with_width(
        &ui.theme,
        items,
        true,
        selected_index,
        Some(popup_item_width(popup_width)),
        ui.presentation.focused_row_overflow,
        ui.focused_marquee_phase(),
    );
    if is_empty {
        super::render_view_status(frame, &ui.theme, UiViewStatus::Empty, list_rect);
    } else if !list_rect.is_empty() {
        utils::render_list_window(
            frame,
            list,
            list_rect,
            len,
            ui.popup.as_mut().unwrap().list_state_mut().unwrap(),
        );
        let start = ui
            .popup
            .as_ref()
            .and_then(PopupState::list_state)
            .map(ListState::offset)
            .unwrap_or_default();
        record_visible_row_hits(&mut ui.workspace_popup_hits, list_rect, start, len);
    }
    if footer.width > 0 && footer.height > 0 {
        frame.render_widget(
            Paragraph::new(utils::bounded_text(legend, footer.width as usize))
                .style(ui.theme.block_title()),
            footer,
        );
    }
    list_rect
}

fn popup_list_items(
    items: Vec<(String, bool)>,
    total_width: u16,
    selected_index: Option<usize>,
    overflow: config::FocusedRowOverflow,
    enable_relative_line_number: bool,
) -> Vec<(String, bool)> {
    let relative_prefix_width = if selected_index.is_some() && enable_relative_line_number {
        utils::relative_line_number_prefix_width(items.len())
    } else {
        0
    };
    let maximum = popup_item_width(total_width)
        .saturating_sub(relative_prefix_width)
        .max(1);
    items
        .into_iter()
        .enumerate()
        .map(|(index, (label, is_playing))| {
            let label = if selected_index == Some(index) && overflow.scrolls() {
                label
            } else {
                utils::bounded_text(&label, maximum)
            };
            (label, is_playing)
        })
        .collect()
}

fn bounded_popup_height(requested: u16, available: u16, preferred_minimum: u16) -> u16 {
    if available == 0 {
        return 0;
    }
    let maximum = available.saturating_sub(1).max(1);
    requested.min(maximum).max(preferred_minimum.min(maximum))
}

fn popup_detail_height(text: &str, width: u16, available: u16, preferred_minimum: u16) -> u16 {
    let content_width = width.saturating_sub(2).max(1);
    let line_count = Paragraph::new(text)
        .wrap(Wrap { trim: false })
        .line_count(content_width) as u16;
    bounded_popup_height(line_count.saturating_add(2), available, preferred_minimum)
}

fn popup_item_width(total_width: u16) -> usize {
    // Reserve two cells for the popup border and two for the active-list mark.
    total_width.saturating_sub(4).max(1) as usize
}

#[cfg(test)]
fn session_history_items<'a>(
    entries: impl Iterator<Item = &'a crate::state::SessionEntry>,
    maximum_width: usize,
) -> Vec<(String, bool)> {
    super::components::history::session_history_items_with_selection(
        entries,
        maximum_width,
        &std::collections::BTreeSet::new(),
    )
}

#[cfg(test)]
fn action_menu_items(
    descriptors: &[crate::command::ActionDescriptor],
    maximum_width: usize,
) -> Vec<(String, bool)> {
    numbered_action_items(
        descriptors.iter().map(|descriptor| {
            PopupActionEntry::described(descriptor.label, descriptor.description)
        }),
        maximum_width,
    )
}

#[cfg(feature = "private-capture")]
fn private_derivative_rendered_rows(
    preview: &crate::developer_capture::SafeDerivativePreview,
    width: u16,
) -> usize {
    Paragraph::new(preview.rendered_text())
        .wrap(Wrap { trim: false })
        .line_count(width.max(1))
        .max(1)
}

#[cfg(test)]
fn diagnostic_action_items(
    actions: &[crate::observability::DiagnosticAction],
    maximum_width: usize,
) -> Vec<(String, bool)> {
    numbered_action_items(
        actions
            .iter()
            .map(|action| PopupActionEntry::label(action.label())),
        maximum_width,
    )
}

fn popup_action_items(
    payloads: impl IntoIterator<Item = PopupActionPayload>,
    maximum_width: usize,
) -> Vec<(String, bool)> {
    numbered_action_items(
        payloads.into_iter().map(PopupActionEntry::from_payload),
        maximum_width,
    )
}

/// The 1–9 shortcut column of a numbered popup row; later rows are reached
/// by moving the cursor.
fn action_shortcut(index: usize) -> String {
    if index < 9 {
        format!("{}  ", index + 1)
    } else {
        "   ".to_owned()
    }
}

/// Rows of shortcut, label and description, with the descriptions aligned
/// in a column of their own.
fn numbered_action_items(
    entries: impl IntoIterator<Item = PopupActionEntry>,
    maximum_width: usize,
) -> Vec<(String, bool)> {
    const MAX_LABEL_COLUMN: usize = 24;
    let entries: Vec<PopupActionEntry> = entries.into_iter().collect();
    let label_column = entries
        .iter()
        .filter(|entry| entry.description_text().is_some())
        .map(|entry| entry.label_text().chars().count())
        .max()
        .unwrap_or(0)
        .min(MAX_LABEL_COLUMN);
    entries
        .iter()
        .enumerate()
        .map(|(index, entry)| {
            let label = entry.label_text();
            let shortcut = action_shortcut(index);
            // A label longer than the column pushes only its own description.
            let text = match entry.description_text() {
                Some(description) => {
                    format!("{shortcut}{label:<label_column$}  {description}")
                }
                None => format!("{shortcut}{label}"),
            };
            (utils::bounded_text(&text, maximum_width), false)
        })
        .collect()
}

#[cfg(test)]
mod diagnostic_popup_layout_tests {
    use super::{
        action_menu_items, bounded_popup_height, config_popup_title, device_popup_items,
        diagnostic_action_items, playlist_create_field_rects, popup_bidi_bounded, popup_bidi_label,
        popup_confirmation_line, popup_detail_height, popup_item_width, popup_list_items,
        popup_query_line, popup_search_height, popup_search_line, popup_wrapped_input_height,
        render_action_list_popup, render_list_popup, render_popup, render_workspace_action_popup,
        render_workspace_user_playlist_popup, session_history_items, shortcut_help_cell_width,
        styled_summary_lines, unified_playlist_popup_label, PopupActionEntry, PopupActionPayload,
    };
    use crate::command::Action;
    use crate::observability::DiagnosticAction;
    use crate::state::{
        ActionListItem, PageState, PlaylistPopupAction, PopupState, UIState,
        UnifiedPlaylistContextActionMenu,
    };
    use crate::state::{Device, MediaId, MediaKind, Provider, SessionEntry, SessionHistory};
    use crate::ui::components::history::session_history_items_with_selection;
    use crate::ui::utils;
    use ratatui::{
        backend::TestBackend,
        layout::Rect,
        style::{Color, Style},
        widgets::ListState,
        Terminal,
    };
    use std::collections::VecDeque;
    use std::sync::Arc;

    #[test]
    fn diagnostic_popups_stay_inside_narrow_terminal_height() {
        for available in 0..12 {
            let action_height = bounded_popup_height(20, available, 3);
            let detail_height = bounded_popup_height(80, available, 5);
            assert!(action_height <= available);
            assert!(detail_height <= available);
        }
        assert_eq!(bounded_popup_height(3, 40, 3), 3);
        assert_eq!(bounded_popup_height(80, 40, 5), 39);
    }

    #[test]
    fn playlist_creation_fields_have_distinct_horizontal_cells() {
        let fields = playlist_create_field_rects(Rect::new(0, 0, 100, 3));
        assert_eq!(fields[0].right(), fields[1].x);
        assert_eq!(fields[1].right(), fields[2].x);
        assert!(fields.iter().all(|field| field.height == 3));
    }

    #[test]
    fn diagnostic_action_labels_are_one_based_in_narrow_popups() {
        let items = diagnostic_action_items(
            &[DiagnosticAction::EnableTrace15, DiagnosticAction::StopTrace],
            120,
        );
        assert_eq!(items[0].0, "1  Enable verbose tracing for 15 seconds");
        assert_eq!(items[1].0, "2  Stop temporary verbose tracing");
        assert!(bounded_popup_height(items.len() as u16 + 2, 4, 3) <= 4);
        assert!(
            diagnostic_action_items(&[DiagnosticAction::EnableTrace15], 12)[0]
                .0
                .chars()
                .count()
                <= 12
        );
    }

    #[test]
    fn ordinary_action_labels_are_human_readable_and_bounded() {
        let descriptors = [
            Action::GoToArtist.descriptor(),
            Action::AddToQueue.descriptor(),
            Action::DeleteFromLibrary.descriptor(),
        ];
        let items = action_menu_items(&descriptors, 120);
        // Labels and descriptions sit in aligned columns, not "label: description".
        let description_column = items[0].0.find("Open the item's").unwrap();
        assert!(items[0].0.starts_with("1  Open artist "));
        assert!(!items[0].0.contains(": "));
        assert_eq!(
            items[1].0.find("Add this item").unwrap(),
            description_column
        );
        assert!(items[1].0.starts_with("2  Add to queue "));
        assert!(items[2].0.starts_with("3  Remove from library "));
        assert!(action_menu_items(&descriptors, 20)[0].0.chars().count() <= 20);
        assert_eq!(utils::bounded_text("abcdef", 5), "ab...");
        assert_eq!(utils::bounded_text("abcdef", 3), "abc");
    }

    #[test]
    fn user_facing_popup_labels_use_the_shared_bidi_projection() {
        let label = "Playlist שלום";
        assert_eq!(popup_bidi_label(label), utils::to_bidi_string(label));
    }

    #[test]
    fn unified_playlist_popup_labels_use_one_bounded_projection() {
        let label = unified_playlist_popup_label("Mix שלום", 120);
        assert!(label.starts_with("[Unified] "));
        assert!(label.contains(&utils::to_bidi_string("Mix שלום")));

        let long = unified_playlist_popup_label(&"x".repeat(200), 24);
        assert!(long.chars().count() <= popup_item_width(24));
        assert!(long.ends_with("..."));
    }

    #[test]
    fn popup_list_projection_bounds_unfocused_rows_but_preserves_focused_marquee() {
        let items = vec![("x".repeat(100), false), ("y".repeat(100), true)];
        let truncated = popup_list_items(
            items.clone(),
            24,
            Some(1),
            crate::config::FocusedRowOverflow::Truncate,
            false,
        );
        assert!(truncated
            .iter()
            .all(|(label, _)| label.chars().count() <= popup_item_width(24)));
        assert!(truncated[0].0.ends_with("..."));

        let marquee = popup_list_items(
            items,
            24,
            Some(1),
            crate::config::FocusedRowOverflow::Marquee,
            false,
        );
        assert_eq!(marquee[0].0, truncated[0].0);
        assert_eq!(marquee[1].0.chars().count(), 100);
    }

    #[test]
    fn popup_list_projection_reserves_relative_number_prefix() {
        let items = popup_list_items(
            vec![("x".repeat(100), false), ("y".repeat(100), true)],
            24,
            Some(1),
            crate::config::FocusedRowOverflow::Truncate,
            true,
        );
        let maximum = popup_item_width(24) - utils::relative_line_number_prefix_width(2);
        assert!(items
            .iter()
            .all(|(label, _)| label.chars().count() <= maximum));
    }

    #[test]
    fn device_popup_hides_unique_ids_but_keeps_duplicate_names_distinguishable() {
        let devices = vec![
            Device {
                id: "phone-12345678".to_owned(),
                name: "Phone".to_owned(),
                is_integrated: false,
            },
            Device {
                id: "desktop-device-id".to_owned(),
                name: "Desktop".to_owned(),
                is_integrated: true,
            },
            Device {
                id: "other-abcdefgh".to_owned(),
                name: "Phone".to_owned(),
                is_integrated: false,
            },
        ];

        let items = device_popup_items(&devices, "desktop-device-id");
        assert_eq!(items[0].0, "Phone [...12345678]");
        assert_eq!(items[1].0, "Desktop (integrated)");
        assert_eq!(items[2].0, "Phone [...abcdefgh]");
        assert!(!items[0].1);
        assert!(items[1].1);
    }

    #[test]
    fn config_popups_use_user_facing_setting_titles() {
        assert_eq!(
            config_popup_title("presentation.compact_metadata", None),
            "Compact metadata detail"
        );
        assert_eq!(
            config_popup_title("presentation.focused_row_overflow", Some("(choose one)")),
            "Focused row overflow (choose one)"
        );
    }

    #[test]
    fn display_only_popup_queries_use_bounded_bidi_projection() {
        let query = "Mix שלום";
        assert_eq!(
            popup_bidi_bounded(query, 80),
            utils::bounded_text(&utils::to_bidi_string(query), 80)
        );
    }

    #[test]
    fn playlist_search_query_wraps_inside_its_input_panel() {
        assert_eq!(
            popup_search_line("Mix שלום"),
            format!("🔍 {}", popup_bidi_label("Mix שלום"))
        );
        assert_eq!(popup_search_height("short", 60), 3);
        assert!(
            popup_search_height(
                "A very long playlist query that needs more than one row",
                24
            ) > 3
        );
        assert!(popup_search_height("x".repeat(500).as_str(), 24) <= 5);
    }

    #[test]
    fn ordinary_search_query_uses_the_same_wrapped_input_policy() {
        let query = "A long search query that needs another row";
        assert_eq!(
            popup_query_line(query),
            format!("/{}", popup_bidi_label(query))
        );
        assert!(popup_wrapped_input_height(&popup_query_line(query), 24) > 3);
        assert!(popup_wrapped_input_height("/short", 60) == 3);
    }

    #[test]
    fn confirmation_messages_wrap_and_project_dynamic_text() {
        let message = "Delete a very long playlist name שלום?";
        assert_eq!(
            popup_confirmation_line(message),
            format!("{} (y/n)", popup_bidi_label(message))
        );
        assert!(popup_wrapped_input_height(&popup_confirmation_line(message), 24) > 3);
    }

    #[test]
    fn detail_popups_allocate_rows_for_wrapped_copy() {
        assert_eq!(popup_detail_height("Short detail", 60, 40, 5), 5);
        assert!(
            popup_detail_height(
                "A detail message with enough words to wrap across several rows "
                    .repeat(3)
                    .as_str(),
                24,
                40,
                5
            ) > 5
        );
        assert!(popup_detail_height("x".repeat(2_000).as_str(), 24, 8, 5) <= 8);
    }

    #[test]
    fn shortcut_help_cell_width_is_conservative_on_narrow_terminals() {
        assert_eq!(shortcut_help_cell_width(60), 19);
        assert_eq!(shortcut_help_cell_width(3), 1);
        assert_eq!(shortcut_help_cell_width(0), 1);
    }

    #[test]
    fn popup_item_width_reserves_border_and_selection_cells() {
        assert_eq!(popup_item_width(60), 56);
        assert_eq!(popup_item_width(4), 1);
        assert_eq!(popup_item_width(0), 1);
    }

    #[test]
    fn empty_list_popups_use_shared_status_surface() {
        crate::ui::initialize_test_config();
        let state = crate::state::TrackedMutex::new(UIState::default());
        let mut ui = state.lock();
        ui.popup = Some(PopupState::ThemeList(Vec::new(), ListState::default()));
        let mut terminal = Terminal::new(TestBackend::new(40, 10)).unwrap();
        terminal
            .draw(|frame| {
                render_list_popup(frame, frame.area(), "Themes", Vec::new(), 5, &mut ui);
            })
            .unwrap();
        let rendered = terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .map(|cell| cell.symbol())
            .collect::<String>();
        assert!(rendered.contains("No items were found."));
    }

    #[test]
    fn list_popups_publish_visible_row_hits_and_fill_the_owned_surface() {
        crate::ui::initialize_test_config();
        let state = crate::state::TrackedMutex::new(UIState::default());
        let mut ui = state.lock();
        ui.popup = Some(PopupState::ThemeList(
            vec![crate::config::Theme::default(); 3],
            ListState::default(),
        ));
        let mut terminal = Terminal::new(TestBackend::new(40, 12)).unwrap();
        terminal
            .draw(|frame| {
                render_list_popup(
                    frame,
                    frame.area(),
                    "Themes",
                    vec![
                        ("Default".to_owned(), false),
                        ("Light".to_owned(), false),
                        ("High contrast".to_owned(), false),
                    ],
                    7,
                    &mut ui,
                );
            })
            .unwrap();

        assert_eq!(ui.workspace_popup_hits.len(), 3);
        assert_eq!(ui.popup_rect, Rect::new(0, 5, 40, 7));
        let (first_row, first_index) = ui.workspace_popup_hits[0];
        assert_eq!(first_index, 0);
        assert_eq!(ui.workspace_popup_hit_at(first_row.x, first_row.y), Some(0));
        let surface_gutter = first_row.x.saturating_add(first_row.width);
        assert_eq!(
            terminal.backend().buffer()[(surface_gutter, first_row.y)].bg,
            Color::Rgb(26, 26, 28)
        );
    }

    #[test]
    fn action_popup_keeps_legend_in_the_border_not_the_rows() {
        crate::ui::initialize_test_config();
        let state = crate::state::TrackedMutex::new(UIState::default());
        let mut ui = state.lock();
        ui.popup = Some(PopupState::DiagnosticActions {
            target: crate::observability::DiagnosticRowId::WorkersEmpty,
            actions: vec![DiagnosticAction::ExplainState],
            state: ListState::default(),
        });
        let mut terminal = Terminal::new(TestBackend::new(50, 10)).unwrap();
        terminal
            .draw(|frame| {
                render_action_list_popup(
                    frame,
                    frame.area(),
                    "Actions",
                    vec![("[1] Explain state".to_owned(), false)],
                    4,
                    &mut ui,
                    "1-9 Run  Enter Inspect  Esc Back",
                );
            })
            .unwrap();
        let rendered = terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .map(|cell| cell.symbol())
            .collect::<String>();
        assert!(rendered.contains("1-9 Run"));
        assert!(rendered.contains("[1] Explain state"));
        assert_eq!(ui.popup_rect, Rect::new(0, 6, 50, 4));
        assert_eq!(ui.workspace_popup_hits.len(), 1);
        let (row, index) = ui.workspace_popup_hits[0];
        assert_eq!(index, 0);
        assert_eq!(ui.workspace_popup_hit_at(row.x, row.y), Some(0));
        let surface_gutter = row.x.saturating_add(row.width);
        assert_eq!(
            terminal.backend().buffer()[(surface_gutter, row.y)].bg,
            Color::Rgb(26, 26, 28)
        );
    }

    #[test]
    fn workspace_action_popup_uses_the_elevated_overlay_path() {
        crate::ui::initialize_test_config();
        let state = crate::state::TrackedMutex::new(UIState::default());
        let mut ui = state.lock();
        ui.popup = Some(PopupState::ActionList(
            Box::new(ActionListItem::YouTubeTrack(
                crate::state::YouTubeTrack {
                    id: "video".to_owned(),
                    name: "Example video".to_owned(),
                    artists: "Example artist".to_owned(),
                    album: None,
                    duration: "1:00".to_owned(),
                    explicit: false,
                    thumbnail_url: None,
                    is_video: false,
                },
                vec![Action::AddToQueue],
            )),
            ListState::default(),
        ));
        let mut terminal = Terminal::new(TestBackend::new(60, 16)).unwrap();
        terminal
            .draw(|frame| render_workspace_action_popup(frame, &mut ui, frame.area()))
            .unwrap();

        assert!(ui.popup_rect.width >= 18);
        assert!(ui.popup_rect.y > 0);
        assert_eq!(ui.workspace_popup_hits.len(), 1);
        let (row, _) = ui.workspace_popup_hits[0];
        assert_eq!(ui.workspace_popup_hit_at(row.x, row.y), Some(0));
    }

    #[test]
    fn workspace_user_playlist_popup_owns_content_without_painting_the_navigation_rail() {
        crate::ui::initialize_test_config();
        let ring = std::sync::Arc::new(parking_lot::Mutex::new(std::collections::VecDeque::new()));
        let (diagnostics, _runtime) = crate::observability::disabled(ring);
        let state = std::sync::Arc::new(crate::state::State::new(false, diagnostics));
        let mut ui = state.ui.lock();
        ui.popup = Some(PopupState::UserPlaylistList(
            PlaylistPopupAction::Browse {
                folder_id: 0,
                search_query: String::new(),
            },
            ListState::default(),
        ));
        ui.workspace_layout.content = Rect::new(30, 3, 50, 18);
        let mut terminal = Terminal::new(TestBackend::new(90, 24)).unwrap();
        terminal
            .draw(|frame| {
                frame.buffer_mut()[(2, 5)].set_style(Style::default().fg(Color::Magenta));
                render_workspace_user_playlist_popup(frame, &state, &mut ui);
            })
            .unwrap();

        assert_eq!(ui.popup_rect, Rect::new(30, 3, 50, 18));
        assert!(ui.popup_contains_point(31, 4));
        assert!(!ui.popup_contains_point(2, 5));
        assert_eq!(terminal.backend().buffer()[(2, 5)].fg, Color::Magenta);
    }

    #[test]
    fn listenbrainz_backup_action_renders_in_the_shared_action_popup() {
        crate::ui::initialize_test_config();
        let state = crate::state::TrackedMutex::new(UIState::default());
        let mut ui = state.lock();
        let descriptors = [Action::BackupUnifiedPlaylistToListenBrainz.descriptor()];
        let items = action_menu_items(&descriptors, 70);
        ui.popup = Some(PopupState::ActionList(
            Box::new(ActionListItem::UnifiedPlaylistContext(
                UnifiedPlaylistContextActionMenu::with_actions(
                    "playlist".to_owned(),
                    "Playlist".to_owned(),
                    [Action::BackupUnifiedPlaylistToListenBrainz],
                ),
            )),
            ListState::default(),
        ));
        let mut terminal = Terminal::new(TestBackend::new(70, 10)).unwrap();
        terminal
            .draw(|frame| {
                render_action_list_popup(
                    frame,
                    frame.area(),
                    "Unified playlist actions",
                    items.clone(),
                    4,
                    &mut ui,
                    "Enter Run  Esc Back",
                );
            })
            .unwrap();
        let rendered = terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .map(|cell| cell.symbol())
            .collect::<String>();
        assert!(rendered.contains("Back up to ListenBrainz"));
    }

    #[test]
    fn listenbrainz_details_use_real_columns_and_keep_narrow_metadata_readable() {
        let configs = crate::ui::initialize_test_config();
        let ring = Arc::new(parking_lot::Mutex::new(VecDeque::new()));
        let (diagnostics, _runtime) = crate::observability::disabled(ring);
        let state = Arc::new(crate::state::State::new_with_configs(
            false,
            diagnostics,
            configs,
        ));
        let preview = crate::state::ListenBrainzSyncPreview {
            playlist_id: "captured-playlist".to_owned(),
            operation_reference: "ui-0042".to_owned(),
            rows: vec![crate::state::ListenBrainzSyncDetailRow {
                occurrence: Some(crate::state::PlaylistEntryId(7)),
                side: crate::state::ListenBrainzSyncSide::Both,
                action: crate::state::ListenBrainzSyncDetailAction::IdentityChanged,
                title: "A title that remains bounded".to_owned(),
                artist: "A selected artist".to_owned(),
                provider: "Spotify".to_owned(),
                conflict: Some(crate::state::ListenBrainzSyncConflictKind::Mapping),
            }],
            conflicts: vec![crate::state::ListenBrainzSyncConflictKind::Mapping],
        };

        for (width, expected) in [
            (
                110,
                ["Side", "Action", "Title", "Artist", "Provider", "Conflict"],
            ),
            (
                60,
                [
                    "Side",
                    "Action",
                    "Title",
                    "Artist:",
                    "Provider:",
                    "Conflict",
                ],
            ),
        ] {
            let mut list = ListState::default();
            list.select(Some(0));
            let mut ui = state.ui.lock();
            ui.popup = Some(PopupState::ListenBrainzSyncDetails {
                preview: preview.clone(),
                state: list,
            });
            let mut terminal = Terminal::new(TestBackend::new(width, 12)).unwrap();
            terminal
                .draw(|frame| {
                    render_popup(frame, &state, &mut ui, frame.area());
                })
                .unwrap();
            let rendered = terminal
                .backend()
                .buffer()
                .content()
                .iter()
                .map(|cell| cell.symbol())
                .collect::<String>();
            for label in expected {
                assert!(rendered.contains(label), "missing {label} at width {width}");
            }
            assert!(!rendered.contains("captured-playlist"));
            assert!(!rendered.contains("ui-0042"));
        }
    }

    #[test]
    fn session_history_rows_are_newest_first_and_bounded() {
        let mut history = SessionHistory::default();
        for (id, title) in [("old", "Older"), ("new", "A very long title")] {
            history.entries.push(SessionEntry {
                media_id: MediaId {
                    provider: Provider::Spotify,
                    kind: MediaKind::Track,
                    raw_id: id.to_owned(),
                },
                title: title.to_owned(),
                artists: "Artist".to_owned(),
                album: None,
                duration_ms: Some(61_000),
                started_at: 1,
            });
        }
        let rows = session_history_items(history.newest_first(), 24);
        assert_eq!(rows.len(), 2);
        assert!(rows[0].0.starts_with("Spotify/track"));
        assert!(!rows[0].0.contains("Older"));
        assert!(rows[1].0.contains("Older"));
        assert!(rows.iter().all(|(row, _)| row.chars().count() <= 24));
    }

    #[test]
    fn session_history_projects_provider_metadata_for_bidi_text() {
        let mut history = SessionHistory::default();
        history.entries.push(SessionEntry {
            media_id: MediaId {
                provider: Provider::Spotify,
                kind: MediaKind::Track,
                raw_id: "rtl".to_owned(),
            },
            title: "Mix שלום".to_owned(),
            artists: "Artist עולם".to_owned(),
            album: None,
            duration_ms: None,
            started_at: 1,
        });

        let rows = session_history_items(history.newest_first(), 120);
        assert!(rows[0].0.contains(&utils::to_bidi_string("Mix שלום")));
        assert!(rows[0].0.contains(&utils::to_bidi_string("Artist עולם")));
    }

    #[test]
    fn session_history_selection_marker_is_bounded_and_explicit() {
        let entry = SessionEntry {
            media_id: MediaId {
                provider: Provider::Spotify,
                kind: MediaKind::Track,
                raw_id: "selected".to_owned(),
            },
            title: "Selected".to_owned(),
            artists: "Artist".to_owned(),
            album: None,
            duration_ms: None,
            started_at: 1,
        };
        let selected = std::collections::BTreeSet::from([0usize]);
        let rows = session_history_items_with_selection(std::iter::once(&entry), 32, &selected);
        assert!(rows[0].0.starts_with("[x] Spotify/track"));
        assert!(rows[0].0.chars().count() <= 32);
    }

    #[test]
    fn long_action_lists_leave_rows_beyond_nine_to_navigation() {
        let descriptors = vec![Action::AddToQueue.descriptor(); 12];
        let items = action_menu_items(&descriptors, 120);
        assert_eq!(items.len(), 12);
        assert!(items[8].0.starts_with("9  "));
        assert!(items[9].0.starts_with("   "));
        assert!(items[10].0.starts_with("   "));
        assert!(items[11].0.starts_with("   "));
        assert!(bounded_popup_height(items.len() as u16 + 2, 8, 3) <= 8);
    }

    #[test]
    fn action_entry_projection_is_shared_by_described_and_label_only_rows() {
        assert_eq!(
            PopupActionEntry::described("Add", "Add this item").display_text(),
            "Add: Add this item"
        );
        assert_eq!(
            PopupActionEntry::label("Explain state").display_text(),
            "Explain state"
        );
        assert_eq!(
            PopupActionEntry::from_payload(PopupActionPayload::Ordinary(
                Action::AddToQueue.descriptor(),
            ))
            .display_text(),
            "Add to queue: Add this item to the playback queue"
        );
        assert_eq!(
            PopupActionEntry::from_payload(PopupActionPayload::Diagnostic(
                DiagnosticAction::ExplainState,
            ))
            .display_text(),
            "Explain this diagnostic state"
        );
    }

    #[test]
    fn listenbrainz_workspace_lists_every_operation_with_summary() {
        use crate::state::LISTENBRAINZ_WORKSPACE_ACTIONS;
        let configs = crate::ui::initialize_test_config();
        let ring = Arc::new(parking_lot::Mutex::new(VecDeque::new()));
        let (diagnostics, _runtime) = crate::observability::disabled(ring);
        let state = Arc::new(crate::state::State::new_with_configs(
            false,
            diagnostics,
            configs,
        ));
        for width in [100u16, 60u16] {
            let mut list = ListState::default();
            list.select(Some(0));
            let mut ui = state.ui.lock();
            ui.popup = Some(PopupState::ListenBrainzWorkspace {
                playlist_id: "private-id".to_owned(),
                playlist_name: "Evening mix".to_owned(),
                state: list,
                changes: ratatui::widgets::TableState::default(),
            });
            let mut terminal = Terminal::new(TestBackend::new(width, 24)).unwrap();
            terminal
                .draw(|frame| {
                    render_popup(frame, &state, &mut ui, frame.area());
                })
                .unwrap();
            let rendered = terminal
                .backend()
                .buffer()
                .content()
                .iter()
                .map(|cell| cell.symbol())
                .collect::<String>();
            assert!(rendered.contains("ListenBrainz"), "width {width}");
            assert!(rendered.contains("Evening mix"), "width {width}");
            assert!(rendered.contains("1-9 Run"), "width {width}");
            assert!(rendered.contains("Pending changes"), "width {width}");
            assert!(rendered.contains("Operations"), "width {width}");
            for label in [
                "Back up to ListenBrainz",
                "Start sync tracking",
                "Check for changes",
                "Review outgoing changes",
                "Review incoming changes",
                "Review conflicts",
                "Undo the last pull",
                "Send my changes",
                "Take their changes",
                "Apply my conflict choices",
            ] {
                assert!(rendered.contains(label), "missing {label} at {width}");
            }
            assert!(!rendered.contains("private-id"), "width {width}");
        }
        assert_eq!(LISTENBRAINZ_WORKSPACE_ACTIONS.len(), 13);
    }

    #[test]
    fn listenbrainz_resolve_popup_lists_policies_conflicts_and_apply() {
        use crate::state::{ListenBrainzResolveMenu, ListenBrainzSyncConflictKind};
        let configs = crate::ui::initialize_test_config();
        let ring = Arc::new(parking_lot::Mutex::new(VecDeque::new()));
        let (diagnostics, _runtime) = crate::observability::disabled(ring);
        let state = Arc::new(crate::state::State::new_with_configs(
            false,
            diagnostics,
            configs,
        ));
        for width in [100u16, 60u16] {
            let mut list = ListState::default();
            list.select(Some(0));
            let mut ui = state.ui.lock();
            ui.popup = Some(PopupState::ListenBrainzResolve {
                menu: ListenBrainzResolveMenu::new(
                    "private-id".to_owned(),
                    "Evening mix".to_owned(),
                    vec![
                        ListenBrainzSyncConflictKind::Reorder,
                        ListenBrainzSyncConflictKind::AddAdd,
                    ],
                ),
                state: list,
            });
            let mut terminal = Terminal::new(TestBackend::new(width, 24)).unwrap();
            terminal
                .draw(|frame| {
                    render_popup(frame, &state, &mut ui, frame.area());
                })
                .unwrap();
            let rendered = terminal
                .backend()
                .buffer()
                .content()
                .iter()
                .map(|cell| cell.symbol())
                .collect::<String>();
            assert!(
                rendered.contains("Resolve ListenBrainz conflicts"),
                "width {width}"
            );
            assert!(rendered.contains("Evening mix"), "width {width}");
            for label in [
                "Keep local",
                "Keep ListenBrainz",
                "Merge non-conflicting",
                "reorder",
                "add/add",
                "Apply resolution",
            ] {
                assert!(rendered.contains(label), "missing {label} at {width}");
            }
            assert!(!rendered.contains("private-id"), "width {width}");
        }
    }

    #[test]
    fn listenbrainz_workspace_summary_colors_the_sync_status() {
        use crate::state::ListenBrainzSyncMeaning;
        let styled = styled_summary_lines(
            "ListenBrainz: conflict | Check",
            "conflict",
            Style::default(),
        );
        assert_eq!(styled.len(), 1);
        assert_eq!(
            styled[0].width(),
            "ListenBrainz: conflict | Check".chars().count()
        );
        let plain =
            styled_summary_lines("ListenBrainz: clean | Check", "missing", Style::default());
        assert_eq!(plain.len(), 1);
        let theme = crate::config::Theme::default();
        assert_eq!(
            ListenBrainzSyncMeaning::Conflict.status_style(&theme),
            theme.sync_conflict()
        );
    }

    #[test]
    fn listenbrainz_workspace_shows_colored_pending_changes_beside_operations() {
        let configs = crate::ui::initialize_test_config();
        let ring = Arc::new(parking_lot::Mutex::new(VecDeque::new()));
        let (diagnostics, _runtime) = crate::observability::disabled(ring);
        let state = Arc::new(crate::state::State::new_with_configs(
            false,
            diagnostics,
            configs,
        ));
        let rows = ["New track", "Old track"]
            .into_iter()
            .enumerate()
            .map(|(index, title)| crate::state::ListenBrainzSyncDetailRow {
                occurrence: None,
                side: crate::state::ListenBrainzSyncSide::Local,
                action: if index == 0 {
                    crate::state::ListenBrainzSyncDetailAction::Added
                } else {
                    crate::state::ListenBrainzSyncDetailAction::Removed
                },
                title: title.to_owned(),
                artist: "Artist".to_owned(),
                provider: "Spotify".to_owned(),
                conflict: None,
            })
            .collect::<Vec<_>>();
        for width in [100u16, 60u16] {
            let mut ui = state.ui.lock();
            ui.new_page(PageState::new_unified_playlist("private-id"));
            let PageState::UnifiedPlaylist {
                listenbrainz_preview,
                ..
            } = ui.current_page_mut()
            else {
                unreachable!()
            };
            *listenbrainz_preview = Some(crate::state::ListenBrainzSyncPreview {
                playlist_id: "private-id".to_owned(),
                operation_reference: "ui-0042".to_owned(),
                rows: rows.clone(),
                conflicts: Vec::new(),
            });
            let mut list = ListState::default();
            list.select(Some(0));
            ui.popup = Some(PopupState::ListenBrainzWorkspace {
                playlist_id: "private-id".to_owned(),
                playlist_name: "Evening mix".to_owned(),
                state: list,
                changes: ratatui::widgets::TableState::default(),
            });
            let mut terminal = Terminal::new(TestBackend::new(width, 24)).unwrap();
            terminal
                .draw(|frame| {
                    render_popup(frame, &state, &mut ui, frame.area());
                })
                .unwrap();
            let buffer = terminal.backend().buffer().clone();
            let rendered = buffer
                .content()
                .iter()
                .map(|cell| cell.symbol())
                .collect::<String>();
            assert!(rendered.contains("Pending changes (2)"), "width {width}");
            assert!(rendered.contains("New track"), "width {width}");
            assert!(rendered.contains("Old track"), "width {width}");
            assert!(!rendered.contains("No pending changes"), "width {width}");
            assert!(!rendered.contains("private-id"), "width {width}");
            // The added row keeps its green version-control color on the
            // rendered line.
            let area = buffer.area;
            let added_green = (0..area.height).any(|y| {
                let line: Vec<_> = (0..area.width).map(|x| &buffer[(x, y)]).collect();
                line.windows("New track".chars().count()).any(|window| {
                    window.iter().map(|cell| cell.symbol()).collect::<String>() == "New track"
                        && window
                            .iter()
                            .any(|cell| cell.fg == ratatui::style::Color::Green)
                })
            });
            assert!(added_green, "added row reads green at {width}");
        }
    }

    #[test]
    fn listenbrainz_details_keep_scroll_offset_stable_across_frames() {
        let configs = crate::ui::initialize_test_config();
        let ring = Arc::new(parking_lot::Mutex::new(VecDeque::new()));
        let (diagnostics, _runtime) = crate::observability::disabled(ring);
        let state = Arc::new(crate::state::State::new_with_configs(
            false,
            diagnostics,
            configs,
        ));
        let rows = (0..30)
            .map(|index| crate::state::ListenBrainzSyncDetailRow {
                occurrence: None,
                side: crate::state::ListenBrainzSyncSide::Local,
                action: crate::state::ListenBrainzSyncDetailAction::Added,
                title: format!("Track {index:02}"),
                artist: "Artist".to_owned(),
                provider: "Spotify".to_owned(),
                conflict: None,
            })
            .collect::<Vec<_>>();
        let preview = crate::state::ListenBrainzSyncPreview {
            playlist_id: "captured-playlist".to_owned(),
            operation_reference: "ui-0042".to_owned(),
            rows,
            conflicts: Vec::new(),
        };
        let mut list = ListState::default();
        list.select(Some(25));
        let mut ui = state.ui.lock();
        ui.popup = Some(PopupState::ListenBrainzSyncDetails {
            preview,
            state: list,
        });
        let mut terminal = Terminal::new(TestBackend::new(110, 12)).unwrap();
        for _ in 0..2 {
            terminal
                .draw(|frame| {
                    render_popup(frame, &state, &mut ui, frame.area());
                })
                .unwrap();
        }
        let rendered = terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .map(|cell| cell.symbol())
            .collect::<String>();
        // The selected row stays visible: the viewport follows the selection
        // instead of resetting to the top on every frame.
        assert!(rendered.contains("Track 25"));
        assert!(!rendered.contains("captured-playlist"));
        let offset = ui
            .popup
            .as_ref()
            .and_then(|popup| popup.list_state())
            .map(|list| list.offset())
            .unwrap_or_default();
        assert!(offset > 0, "viewport follows the selected row");
    }
}

#[cfg(all(test, feature = "private-capture"))]
mod private_capture_popup_tests {
    use super::{
        bounded_popup_height, diagnostic_action_items, masked_passphrase,
        private_derivative_rendered_rows, PopupState,
    };
    use crate::developer_capture::{SafeDerivativeState, SafeDerivativeView};
    use crate::observability::DiagnosticAction;

    #[test]
    fn passphrase_mask_is_content_free_and_bounded() {
        assert_eq!(masked_passphrase(0), "");
        assert_eq!(masked_passphrase(3), "***");
        assert_eq!(masked_passphrase(1_024).len(), 35);
        assert!(!masked_passphrase(1_024).contains("secret"));
    }

    #[test]
    fn private_capture_actions_remain_scrollable_on_narrow_terminals() {
        let items = diagnostic_action_items(
            &[
                DiagnosticAction::ExplainCaptureSensitivity,
                DiagnosticAction::ReplayPrivateCaptureFresh,
                DiagnosticAction::CreatePrivateDerivative,
                DiagnosticAction::OpenPrivateCaptureFolder,
                DiagnosticAction::DeletePrivateCapture,
            ],
            120,
        );
        assert!(items.iter().all(|(label, _)| label.starts_with('[')));
        for available in 1..8 {
            assert!(bounded_popup_height(items.len() as u16 + 2, available, 3) <= available);
        }
    }

    #[test]
    fn exact_derivative_preview_is_bounded_and_redacted_in_ui_debug() {
        let view = SafeDerivativeView::new_for_test(
            SafeDerivativeState::Previewed,
            [
                "{\"private_ui_debug_canary\":false}\n",
                "abc  evidence.json\n",
                "{\"privacy_boundary\":\"typed-allowlist-only\"}\n",
            ],
            "unified-player provider diagnostic derivative review\nforbidden_scan=passed\n",
        );
        let preview = view.preview_shared().unwrap();
        assert!(preview.render_text().contains("private_ui_debug_canary"));
        assert!(preview.total_bytes() < 512 * 1024);
        let popup = PopupState::PrivateDerivativePreview {
            preview,
            scroll_offset: 0,
            rendered_row_count: 1,
        };
        let debug = format!("{popup:?}");
        assert!(!debug.contains("private_ui_debug_canary"));
        assert!(!debug.contains("typed-allowlist-only"));
    }

    #[test]
    fn derivative_preview_scroll_rows_follow_the_narrow_terminal_width() {
        let view = SafeDerivativeView::new_for_test(
            SafeDerivativeState::Previewed,
            [
                "{\"one_very_long_allowlisted_field\":\"abcdefghijklmnopqrstuvwxyz\"}\n",
                "abc  evidence.json\n",
                "{\"privacy_boundary\":\"typed-allowlist-only\"}\n",
            ],
            "unified-player provider diagnostic derivative review\nforbidden_scan=passed\n",
        );
        let preview = view.preview().unwrap();
        let narrow = private_derivative_rendered_rows(preview, 12);
        let wide = private_derivative_rendered_rows(preview, 120);
        assert!(narrow > wide);
        assert!(narrow > preview.rendered_line_count());
    }
}

/// Render a shortcut help popup to show the available shortcuts based on user's inputs
pub fn render_shortcut_help_popup(frame: &mut Frame, ui: &mut UIStateGuard, rect: Rect) -> Rect {
    let input = &ui.input_key_sequence;

    let matches = if input.keys.is_empty() {
        vec![]
    } else {
        config::get_config()
            .keymap_config
            .find_matched_prefix_bindings(input)
    };

    if matches.is_empty() {
        rect
    } else {
        let row_count = matches.len().div_ceil(SHORTCUT_TABLE_N_COLUMNS);
        let height = bounded_popup_height(row_count as u16 + 2, rect.height, 3);
        let chunks =
            Layout::vertical([Constraint::Fill(0), Constraint::Length(height)]).split(rect);
        let maximum_cell_width = shortcut_help_cell_width(rect.width);

        let rect =
            construct_and_render_block("Shortcuts", &ui.theme, Borders::ALL, frame, chunks[1]);

        let help_table = Table::new(
            matches
                .into_iter()
                .map(|binding| {
                    utils::bounded_text(
                        &format!("{}: {}", binding.key_sequence, binding.label()),
                        maximum_cell_width,
                    )
                })
                .collect::<Vec<_>>()
                .chunks(SHORTCUT_TABLE_N_COLUMNS)
                .map(|c| Row::new(c.iter().map(|i| Cell::from(i.to_owned()))))
                .collect::<Vec<_>>(),
            SHORTCUT_TABLE_CONSTRAINS,
        );

        frame.render_widget(help_table, rect);
        chunks[0]
    }
}

fn shortcut_help_cell_width(total_width: u16) -> usize {
    (total_width.saturating_sub(2) as usize / SHORTCUT_TABLE_N_COLUMNS).max(1)
}

fn render_listenbrainz_playlist_picker(
    frame: &mut Frame,
    ui: &mut UIStateGuard,
    area: Rect,
) -> Rect {
    let Some(PopupState::ListenBrainzPlaylists { rows, notice, .. }) = &ui.popup else {
        return area;
    };
    let notice = notice.clone();
    let mut items = rows
        .iter()
        .map(|row| {
            (
                format!(
                    "{} · {} items{}",
                    row.title.split_whitespace().collect::<Vec<_>>().join(" "),
                    row.item_count
                        .map_or_else(|| "unknown".to_owned(), |n| n.to_string()),
                    if row.imported {
                        " · already imported"
                    } else {
                        ""
                    }
                ),
                false,
            )
        })
        .collect::<Vec<_>>();
    items.extend([
        ("Refresh list".to_owned(), false),
        ("Cancel".to_owned(), false),
    ]);
    let height = (items.len().saturating_add(4).min(18) as u16).min(area.height);
    let chunks = Layout::vertical([Constraint::Fill(0), Constraint::Length(height)]).split(area);
    ui.popup_rect = chunks[1];
    let inner = construct_and_render_block(
        "ListenBrainz playlists · Enter imports locally",
        &ui.theme,
        Borders::ALL,
        frame,
        chunks[1],
    );
    let sections = Layout::vertical([Constraint::Length(2), Constraint::Fill(1)]).split(inner);
    frame.render_widget(
        Paragraph::new(notice)
            .style(ui.theme.secondary_row())
            .wrap(Wrap { trim: true }),
        sections[0],
    );
    let selected = ui.popup.as_ref().and_then(PopupState::list_selected);
    let (list, count) = utils::construct_list_widget_with_width(
        &ui.theme,
        items,
        true,
        selected,
        Some(sections[1].width as usize),
        ui.presentation.focused_row_overflow,
        ui.focused_marquee_phase(),
    );
    let Some(PopupState::ListenBrainzPlaylists { state, .. }) = &mut ui.popup else {
        return chunks[0];
    };
    utils::render_list_window(frame, list, sections[1], count, state);
    let offset = state.offset();
    super::components::list::record_visible_row_hits(
        &mut ui.workspace_popup_hits,
        sections[1],
        offset,
        count,
    );
    chunks[0]
}

#[cfg(test)]
mod listenbrainz_picker_tests {
    use super::*;
    #[test]
    fn listenbrainz_picker_newlines_scroll_and_resize_keep_exact_row_hits() {
        crate::ui::initialize_test_config();
        let ui = crate::state::TrackedMutex::new(crate::state::UIState::default());
        let mut ui = ui.lock();
        let identity = crate::client::listenbrainz::ValidatedListenBrainzIdentity {
            username: "owner".to_owned(),
            token: crate::client::listenbrainz::ListenBrainzToken::new("private-value".to_owned()),
        };
        let rows = (0..20)
            .map(
                |n| crate::client::listenbrainz::ListenBrainzPlaylistSummary {
                    id: n.to_string(),
                    title: if n == 0 {
                        "First\nInjected".to_owned()
                    } else {
                        format!("Playlist {n}")
                    },
                    item_count: None,
                    imported: false,
                },
            )
            .collect();
        ui.popup = Some(PopupState::ListenBrainzPlaylists {
            operation: 1,
            identity,
            rows,
            state: ratatui::widgets::ListState::default().with_selected(Some(0)),
            busy: false,
            notice: "Choose one".to_owned(),
        });
        for (width, height, selected) in [(40, 12, 0), (30, 9, 17), (80, 20, 19)] {
            ui.popup.as_mut().unwrap().list_select(Some(selected));
            // The render loop clears hit geometry before every frame.
            ui.clear_popup_hit_regions();
            let mut terminal =
                ratatui::Terminal::new(ratatui::backend::TestBackend::new(width, height)).unwrap();
            terminal
                .draw(|frame| {
                    render_listenbrainz_playlist_picker(frame, &mut ui, frame.area());
                })
                .unwrap();
            let Some(PopupState::ListenBrainzPlaylists { state, rows, .. }) = &ui.popup else {
                panic!()
            };
            assert_eq!(rows[0].title, "First\nInjected");
            let hits = ui.workspace_popup_hits.clone();
            assert!(!hits.is_empty());
            for (line, (rect, index)) in hits.iter().enumerate() {
                assert_eq!(*index, state.offset() + line);
                assert_eq!(ui.workspace_popup_hit_at(rect.x, rect.y), Some(*index));
                assert_eq!(
                    ui.workspace_popup_hit_at(rect.x.saturating_sub(1), rect.y),
                    None
                );
                assert_eq!(ui.workspace_popup_hit_at(rect.right(), rect.y), None);
                assert_eq!(rect.height, 1);
            }
            if selected == 0 {
                let (first, _) = hits[0];
                let (second, _) = hits[1];
                let text = (first.x..first.right())
                    .map(|x| terminal.backend().buffer()[(x, first.y)].symbol())
                    .collect::<String>();
                let next = (second.x..second.right())
                    .map(|x| terminal.backend().buffer()[(x, second.y)].symbol())
                    .collect::<String>();
                assert!(text.contains("First Injected"));
                assert!(next.contains("Playlist 1"));
            }
            assert!(!format!("{:?}", *ui).contains("private-value"));
        }
    }
}
