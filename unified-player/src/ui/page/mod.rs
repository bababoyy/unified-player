use ratatui::{
    style::Color,
    text::{Line, Span},
    widgets::ListState,
};

use crate::state::{
    settings_filter_projection, synchronize_context_track_uris, synchronize_journal_uris,
    synchronize_native_spotify_queue, synchronize_unified_queue_items,
    unified_playlist_action_model_with_listenbrainz, ContextTrackPane, JournalSelectionScope,
    MutablePlaylistController, MutablePlaylistProjection, PlaylistActionModel,
    PlaylistCapabilities, PlaylistSnapshot, QueueAuthority, QueueSelectionScope,
    QueueSelectionView, ScopedSelectionStatus, UiViewStatus, YouTubeContextId, YouTubeTrack,
};

use super::components::{
    collection::CollectionTrackRow,
    help::project_command_help_rows,
    history::{
        session_history_items_with_selection, HistoryRowProjection, JournalListRowProjection,
    },
    list::record_visible_hits,
    navigation::{
        label_rect, route_divider_rect, route_row_rect, row_rect as workspace_navigation_row_rect,
        scope_row_rect, workspace_rail_items,
    },
    queue::{row_hit_rect as queue_row_hit_rect, QueueRowProjection, SpotifyQueueLabels},
    search::{
        workspace_search_group_from_items, workspace_search_group_is_visible,
        workspace_search_group_rects, workspace_search_item, workspace_search_loaded_count,
        workspace_search_projection_window, workspace_search_visible_focus, WorkspaceSearchGroup,
        WorkspaceSearchProjectionWindow,
    },
};
use super::{
    config, render_view_status, utils, view_status_height, Alignment, Block, Borders,
    BrowsePageUIState, Cell, Constraint, Context, ContextPageUIState, DataReadGuard, Frame, Id,
    Layout, LibraryFocusState, List, ListItem, Modifier, MutableWindowState, PageState, Paragraph,
    PlaylistFolderItem, PopupState, Rect, Row, SearchFocusState, SharedState, Style, Table,
    TableState, Text, Track, TrackJournalEntry, UIStateGuard, Wrap,
};
use crate::state::BidiDisplay;
use crate::state::{
    page::{SettingsCategory, SettingsRailItem, SettingsWorkspaceAction},
    WorkspaceAction, WorkspaceFocusState, WorkspaceHit, WorkspacePlaybackOption, WorkspaceRailItem,
    WorkspaceScopeKind,
};
use crate::ui::utils::to_bidi_string;
use crate::ui::WorkspaceLayoutKind;

mod artist;
mod diagnostics;
mod home;
mod settings_tiles;
mod shelf;
mod show;

pub use home::{render_home_page, render_home_shelf_list};

pub(super) use diagnostics::render_logs_page;

fn context_filter_query(ui: &UIStateGuard) -> Option<String> {
    match ui.popup.as_ref() {
        Some(PopupState::Search { query }) => Some(query.clone()),
        _ => None,
    }
}

fn synchronize_context_render_selection<'a, I>(
    ui: &mut UIStateGuard,
    pane: ContextTrackPane,
    context_uri: &str,
    complete_tracks: &[Track],
    visible_tracks: I,
) -> Option<Vec<usize>>
where
    I: IntoIterator<Item = &'a Track>,
{
    let provider_epoch = ui.provider_selection_epoch(config::ActiveProvider::Spotify);
    let filter_query = context_filter_query(ui);
    let result = ui
        .current_page_mut()
        .context_track_selection_mut(pane)
        .map(|selection| {
            synchronize_context_track_uris(
                selection,
                provider_epoch,
                context_uri.to_owned(),
                pane,
                filter_query.as_deref(),
                complete_tracks.iter().map(|track| track.id.uri()),
                visible_tracks.into_iter().map(|track| track.id.uri()),
            )
        });
    match result {
        Some(Ok(())) => ui
            .current_page()
            .context_track_selection(pane)
            .map(|selection| selection.selected_visible_indices()),
        Some(Err(error)) => {
            tracing::warn!(
                selection_error = ?error,
                "Context selection projection could not be synchronized for rendering"
            );
            None
        }
        None => None,
    }
}

fn selected_marker(selected_indices: &[usize], row_index: usize) -> bool {
    selected_indices.binary_search(&row_index).is_ok()
}

enum QueueSelectionRows {
    Unified(Option<Vec<(crate::state::MediaId, u64)>>),
    Native(Vec<Option<crate::state::MediaId>>),
}

fn synchronize_queue_render_selection(
    ui: &mut UIStateGuard,
    rows: QueueSelectionRows,
    item_count: usize,
    unified_instance_id: Option<crate::state::UnifiedQueueInstanceId>,
) -> Vec<usize> {
    let provider_epoch = ui.provider_selection_epoch(config::ActiveProvider::Spotify);
    let result = ui
        .current_page_mut()
        .queue_selection_mut()
        .map(|selection| match (unified_instance_id, rows) {
            (Some(instance_id), QueueSelectionRows::Unified(rows)) => match rows {
                Some(rows) => synchronize_unified_queue_items(selection, instance_id, rows),
                None => selection.synchronize_cursor_only(
                    QueueSelectionScope::unified(instance_id),
                    QueueSelectionView::All,
                    item_count,
                ),
            },
            (None, QueueSelectionRows::Native(rows)) => {
                synchronize_native_spotify_queue(selection, provider_epoch, rows)
            }
            (Some(instance_id), QueueSelectionRows::Native(_)) => selection
                .synchronize_cursor_only(
                    QueueSelectionScope::unified(instance_id),
                    QueueSelectionView::All,
                    item_count,
                ),
            (None, QueueSelectionRows::Unified(_)) => selection.synchronize_cursor_only(
                QueueSelectionScope::native_spotify(provider_epoch),
                QueueSelectionView::All,
                item_count,
            ),
        });
    match result {
        Some(Ok(())) => ui
            .current_page()
            .queue_selection()
            .filter(|selection| selection.status() != ScopedSelectionStatus::Ambiguous)
            .map(|selection| selection.selected_visible_indices())
            .unwrap_or_default(),
        Some(Err(error)) => {
            tracing::warn!(
                selection_error = ?error,
                "Queue selection projection could not be synchronized for rendering"
            );
            if let Some(selection) = ui.current_page_mut().queue_selection_mut() {
                selection.clear();
            }
            Vec::new()
        }
        None => Vec::new(),
    }
}

fn synchronize_journal_render_selection<'a>(
    ui: &mut UIStateGuard,
    complete: &[TrackJournalEntry],
    visible: impl IntoIterator<Item = &'a TrackJournalEntry>,
) -> Vec<usize> {
    let provider_epoch = ui.provider_selection_epoch(config::ActiveProvider::Spotify);
    let filter_query = match ui.popup.as_ref() {
        Some(PopupState::Search { query }) => Some(query.clone()),
        _ => None,
    };
    let result = ui
        .current_page_mut()
        .journal_selection_mut()
        .map(|selection| {
            synchronize_journal_uris(
                selection,
                JournalSelectionScope::journal(provider_epoch),
                filter_query.as_deref(),
                complete.iter().map(|entry| entry.track.id.uri()),
                visible.into_iter().map(|entry| entry.track.id.uri()),
            )
        });
    match result {
        Some(Ok(())) => ui
            .current_page()
            .journal_selection()
            .map(|selection| selection.selected_visible_indices())
            .unwrap_or_default(),
        Some(Err(error)) => {
            tracing::warn!(
                selection_error = ?error,
                "Journal selection projection could not be synchronized for rendering"
            );
            if let Some(selection) = ui.current_page_mut().journal_selection_mut() {
                selection.clear();
            }
            Vec::new()
        }
        None => Vec::new(),
    }
}

fn synchronize_journal_list_render_selection<'a>(
    ui: &mut UIStateGuard,
    list_id: &str,
    complete_uris: &[String],
    visible: impl IntoIterator<Item = &'a TrackJournalEntry>,
) -> Vec<usize> {
    let provider_epoch = ui.provider_selection_epoch(config::ActiveProvider::Spotify);
    let filter_query = match ui.popup.as_ref() {
        Some(PopupState::Search { query }) => Some(query.clone()),
        _ => None,
    };
    let result = ui
        .current_page_mut()
        .journal_list_selection_mut()
        .map(|selection| {
            synchronize_journal_uris(
                selection,
                JournalSelectionScope::journal_list(provider_epoch, list_id),
                filter_query.as_deref(),
                complete_uris.iter(),
                visible.into_iter().map(|entry| entry.track.id.uri()),
            )
        });
    match result {
        Some(Ok(())) => ui
            .current_page()
            .journal_list_selection()
            .map(|selection| selection.selected_visible_indices())
            .unwrap_or_default(),
        Some(Err(error)) => {
            tracing::warn!(
                selection_error = ?error,
                "Journal list selection projection could not be synchronized for rendering"
            );
            if let Some(selection) = ui.current_page_mut().journal_list_selection_mut() {
                selection.clear();
            }
            Vec::new()
        }
        None => Vec::new(),
    }
}

fn youtube_tracks_match(left: &YouTubeTrack, right: &YouTubeTrack) -> bool {
    left.id == right.id && left.is_video == right.is_video
}

fn lyrics_page_title(track: &str, artists: &str) -> String {
    let track = track.trim();
    let artists = artists.trim();
    match (track.is_empty(), artists.is_empty()) {
        (true, _) => "Lyrics".to_owned(),
        (_, true) => format!("Lyrics: {}", to_bidi_string(track)),
        (false, false) => format!(
            "Lyrics: {} · {}",
            to_bidi_string(track),
            to_bidi_string(artists)
        ),
    }
}

/// The quiet line under the lyrics title. The title already names the track
/// and artist, so this carries only where the lyrics came from and whether
/// they follow playback.
fn lyrics_metadata_line(source: &str, follow_playback: bool) -> String {
    let follow = if follow_playback {
        "following playback"
    } else {
        "scrolling manually"
    };
    format!("{source} · {follow}")
}

fn lyrics_line_text(line: &str) -> String {
    to_bidi_string(line)
}

fn wrapped_description_height(text: &str, width: u16) -> u16 {
    wrapped_description_height_with_limit(text, width, 3)
}

fn wrapped_description_height_with_limit(text: &str, width: u16, max_height: u16) -> u16 {
    Paragraph::new(text)
        .wrap(Wrap { trim: false })
        .line_count(width.max(1))
        .clamp(1, max_height.max(1) as usize) as u16
}

fn render_wrapped_description(frame: &mut Frame, text: &str, style: Style, rect: Rect) -> Rect {
    let height = wrapped_description_height(text, rect.width);
    render_wrapped_description_with_height(frame, text, style, rect, height)
}

fn render_wrapped_description_with_height(
    frame: &mut Frame,
    text: &str,
    style: Style,
    rect: Rect,
    height: u16,
) -> Rect {
    let chunks = Layout::vertical([Constraint::Length(height), Constraint::Fill(0)]).split(rect);
    frame.render_widget(
        Paragraph::new(text).style(style).wrap(Wrap { trim: false }),
        chunks[0],
    );
    chunks[1]
}

// UI codes to render a page.
// A `render_*_page` function should follow (not strictly) the below steps
// 1. get data from the application's states
// 2. construct the page's layout
// 3. construct the page's widgets
// 4. render the widgets

pub fn render_search_page(
    is_active: bool,
    frame: &mut Frame,
    state: &SharedState,
    ui: &mut UIStateGuard,
    rect: Rect,
) {
    render_workspace_search_page(is_active, frame, state, ui, rect);
}

fn workspace_search_pane_label(
    provider: config::ActiveProvider,
    pane: crate::command::ProviderSearchPane,
) -> &'static str {
    match (provider, pane) {
        (config::ActiveProvider::YouTubeMusic, crate::command::ProviderSearchPane::Tracks) => {
            "Songs"
        }
        (config::ActiveProvider::YouTubeMusic, crate::command::ProviderSearchPane::Shows) => {
            "Podcasts"
        }
        (_, crate::command::ProviderSearchPane::Tracks) => "Tracks",
        (_, crate::command::ProviderSearchPane::Videos) => "Videos",
        (_, crate::command::ProviderSearchPane::Albums) => "Albums",
        (_, crate::command::ProviderSearchPane::Artists) => "Artists",
        (_, crate::command::ProviderSearchPane::Playlists) => "Playlists",
        (_, crate::command::ProviderSearchPane::Shows) => "Shows",
        (_, crate::command::ProviderSearchPane::Episodes) => "Episodes",
    }
}

fn workspace_search_category_label(
    provider: config::ActiveProvider,
    category: Option<crate::command::ProviderSearchPane>,
) -> &'static str {
    category.map_or("All", |pane| workspace_search_pane_label(provider, pane))
}

fn workspace_search_group_len(
    provider: config::ActiveProvider,
    data: &DataReadGuard,
    query: &str,
    focus: SearchFocusState,
) -> usize {
    match provider {
        config::ActiveProvider::Spotify => {
            let Some(results) = data.caches.search.get(query) else {
                return 0;
            };
            match focus {
                SearchFocusState::Tracks => results.tracks.len(),
                SearchFocusState::Albums => results.albums.len(),
                SearchFocusState::Artists => results.artists.len(),
                SearchFocusState::Playlists => results.playlists.len(),
                SearchFocusState::Shows => results.shows.len(),
                SearchFocusState::Episodes => results.episodes.len(),
                SearchFocusState::Videos | SearchFocusState::Category | SearchFocusState::Input => {
                    0
                }
            }
        }
        config::ActiveProvider::YouTubeMusic => {
            let Some(results) = data.caches.youtube_search.get(query) else {
                return 0;
            };
            match focus {
                SearchFocusState::Tracks => results.songs.len(),
                SearchFocusState::Videos => results.videos.len(),
                SearchFocusState::Albums => results.albums.len(),
                SearchFocusState::Artists => results.artists.len(),
                SearchFocusState::Playlists => results.playlists.len(),
                SearchFocusState::Shows => results.podcasts.len(),
                SearchFocusState::Episodes => results.episodes.len(),
                SearchFocusState::Category | SearchFocusState::Input => 0,
            }
        }
    }
}

fn workspace_search_projection_windows(
    page: &mut PageState,
    provider: config::ActiveProvider,
    data: &DataReadGuard,
    query: &str,
    wide: bool,
    visible_focus: Option<SearchFocusState>,
    group_rects: &[Rect],
) -> Vec<(SearchFocusState, WorkspaceSearchProjectionWindow)> {
    let focuses = if wide {
        crate::command::provider_capabilities(provider)
            .search_panes()
            .iter()
            .copied()
            .map(SearchFocusState::from_provider_pane)
            .collect::<Vec<_>>()
    } else {
        visible_focus.into_iter().collect::<Vec<_>>()
    };

    focuses
        .into_iter()
        .enumerate()
        .map(|(index, focus)| {
            let rect = if wide {
                group_rects.get(index).copied().unwrap_or_default()
            } else {
                group_rects.first().copied().unwrap_or_default()
            };
            let viewport = usize::from(rect.height.saturating_sub(2));
            let total_items = workspace_search_group_len(provider, data, query, focus);
            let (offset, selected) =
                if let Some(list_state) = workspace_search_list_state_mut(page, focus) {
                    utils::adjust_list_offset(list_state, total_items, viewport as u16);
                    (list_state.offset(), list_state.selected())
                } else {
                    (0, None)
                };
            (
                focus,
                WorkspaceSearchProjectionWindow {
                    offset,
                    viewport,
                    selected,
                },
            )
        })
        .collect()
}

fn workspace_search_groups(
    provider: config::ActiveProvider,
    data: &DataReadGuard,
    query: &str,
    visible_focus: Option<SearchFocusState>,
    windows: Option<&[(SearchFocusState, WorkspaceSearchProjectionWindow)]>,
) -> Vec<WorkspaceSearchGroup> {
    match provider {
        config::ActiveProvider::Spotify => {
            let results = data.caches.search.get(query);
            [
                workspace_search_group_is_visible(visible_focus, SearchFocusState::Tracks).then(
                    || {
                        let focus = SearchFocusState::Tracks;
                        let window = workspace_search_projection_window(windows, focus);
                        workspace_search_group_from_items(
                            focus,
                            "Tracks",
                            25,
                            window,
                            results.map(|results| results.tracks.as_slice()),
                            |track| {
                                workspace_search_item(
                                    to_bidi_string(&track.name),
                                    to_bidi_string(&track.artists_info()),
                                )
                            },
                        )
                    },
                ),
                workspace_search_group_is_visible(visible_focus, SearchFocusState::Albums).then(
                    || {
                        let focus = SearchFocusState::Albums;
                        let window = workspace_search_projection_window(windows, focus);
                        workspace_search_group_from_items(
                            focus,
                            "Albums",
                            25,
                            window,
                            results.map(|results| results.albums.as_slice()),
                            |album| {
                                let artists = album
                                    .artists
                                    .iter()
                                    .map(|artist| artist.name.as_str())
                                    .collect::<Vec<_>>()
                                    .join(", ");
                                workspace_search_item(
                                    to_bidi_string(&album.name),
                                    to_bidi_string(&artists),
                                )
                            },
                        )
                    },
                ),
                workspace_search_group_is_visible(visible_focus, SearchFocusState::Artists).then(
                    || {
                        let focus = SearchFocusState::Artists;
                        let window = workspace_search_projection_window(windows, focus);
                        workspace_search_group_from_items(
                            focus,
                            "Artists",
                            0,
                            window,
                            results.map(|results| results.artists.as_slice()),
                            |artist| workspace_search_item(to_bidi_string(&artist.name), ""),
                        )
                    },
                ),
                workspace_search_group_is_visible(visible_focus, SearchFocusState::Playlists).then(
                    || {
                        let focus = SearchFocusState::Playlists;
                        let window = workspace_search_projection_window(windows, focus);
                        workspace_search_group_from_items(
                            focus,
                            "Playlists",
                            20,
                            window,
                            results.map(|results| results.playlists.as_slice()),
                            |playlist| {
                                workspace_search_item(
                                    to_bidi_string(&playlist.name),
                                    to_bidi_string(&playlist.owner.0),
                                )
                            },
                        )
                    },
                ),
                workspace_search_group_is_visible(visible_focus, SearchFocusState::Shows).then(
                    || {
                        let focus = SearchFocusState::Shows;
                        let window = workspace_search_projection_window(windows, focus);
                        workspace_search_group_from_items(
                            focus,
                            "Shows",
                            0,
                            window,
                            results.map(|results| results.shows.as_slice()),
                            |show| workspace_search_item(to_bidi_string(&show.name), ""),
                        )
                    },
                ),
                workspace_search_group_is_visible(visible_focus, SearchFocusState::Episodes).then(
                    || {
                        let focus = SearchFocusState::Episodes;
                        let window = workspace_search_projection_window(windows, focus);
                        workspace_search_group_from_items(
                            focus,
                            "Episodes",
                            0,
                            window,
                            results.map(|results| results.episodes.as_slice()),
                            |episode| workspace_search_item(to_bidi_string(&episode.name), ""),
                        )
                    },
                ),
            ]
            .into_iter()
            .flatten()
            .collect()
        }
        config::ActiveProvider::YouTubeMusic => {
            let results = data.caches.youtube_search.get(query);
            [
                workspace_search_group_is_visible(visible_focus, SearchFocusState::Tracks).then(
                    || {
                        let focus = SearchFocusState::Tracks;
                        let window = workspace_search_projection_window(windows, focus);
                        workspace_search_group_from_items(
                            focus,
                            "Songs",
                            25,
                            window,
                            results.map(|results| results.songs.as_slice()),
                            |track| {
                                workspace_search_item(
                                    to_bidi_string(&track.name),
                                    to_bidi_string(&track.artists),
                                )
                            },
                        )
                    },
                ),
                workspace_search_group_is_visible(visible_focus, SearchFocusState::Videos).then(
                    || {
                        let focus = SearchFocusState::Videos;
                        let window = workspace_search_projection_window(windows, focus);
                        workspace_search_group_from_items(
                            focus,
                            "Videos",
                            25,
                            window,
                            results.map(|results| results.videos.as_slice()),
                            |track| {
                                workspace_search_item(
                                    to_bidi_string(&track.name),
                                    to_bidi_string(&track.artists),
                                )
                            },
                        )
                    },
                ),
                workspace_search_group_is_visible(visible_focus, SearchFocusState::Albums).then(
                    || {
                        let focus = SearchFocusState::Albums;
                        let window = workspace_search_projection_window(windows, focus);
                        workspace_search_group_from_items(
                            focus,
                            "Albums",
                            25,
                            window,
                            results.map(|results| results.albums.as_slice()),
                            |album| {
                                workspace_search_item(
                                    to_bidi_string(&album.name),
                                    to_bidi_string(&album.artist),
                                )
                            },
                        )
                    },
                ),
                workspace_search_group_is_visible(visible_focus, SearchFocusState::Artists).then(
                    || {
                        let focus = SearchFocusState::Artists;
                        let window = workspace_search_projection_window(windows, focus);
                        workspace_search_group_from_items(
                            focus,
                            "Artists",
                            0,
                            window,
                            results.map(|results| results.artists.as_slice()),
                            |artist| workspace_search_item(to_bidi_string(&artist.name), ""),
                        )
                    },
                ),
                workspace_search_group_is_visible(visible_focus, SearchFocusState::Playlists).then(
                    || {
                        let focus = SearchFocusState::Playlists;
                        let window = workspace_search_projection_window(windows, focus);
                        workspace_search_group_from_items(
                            focus,
                            "Playlists",
                            20,
                            window,
                            results.map(|results| results.playlists.as_slice()),
                            |playlist| {
                                workspace_search_item(
                                    to_bidi_string(&playlist.name),
                                    to_bidi_string(&playlist.author),
                                )
                            },
                        )
                    },
                ),
                workspace_search_group_is_visible(visible_focus, SearchFocusState::Shows).then(
                    || {
                        let focus = SearchFocusState::Shows;
                        let window = workspace_search_projection_window(windows, focus);
                        workspace_search_group_from_items(
                            focus,
                            "Podcasts",
                            25,
                            window,
                            results.map(|results| results.podcasts.as_slice()),
                            |podcast| {
                                workspace_search_item(
                                    to_bidi_string(&podcast.name),
                                    to_bidi_string(&podcast.publisher),
                                )
                            },
                        )
                    },
                ),
                workspace_search_group_is_visible(visible_focus, SearchFocusState::Episodes).then(
                    || {
                        let focus = SearchFocusState::Episodes;
                        let window = workspace_search_projection_window(windows, focus);
                        workspace_search_group_from_items(
                            focus,
                            "Episodes",
                            0,
                            window,
                            results.map(|results| results.episodes.as_slice()),
                            |episode| {
                                workspace_search_item(to_bidi_string(&episode.track.name), "")
                            },
                        )
                    },
                ),
            ]
            .into_iter()
            .flatten()
            .collect()
        }
    }
}

fn workspace_search_list_state_mut(
    page: &mut PageState,
    focus: SearchFocusState,
) -> Option<&mut ListState> {
    let PageState::Search { state, .. } = page else {
        return None;
    };
    Some(match focus {
        SearchFocusState::Category | SearchFocusState::Input => return None,
        SearchFocusState::Tracks => &mut state.track_list,
        SearchFocusState::Videos => &mut state.video_list,
        SearchFocusState::Albums => &mut state.album_list,
        SearchFocusState::Artists => &mut state.artist_list,
        SearchFocusState::Playlists => &mut state.playlist_list,
        SearchFocusState::Shows => &mut state.show_list,
        SearchFocusState::Episodes => &mut state.episode_list,
    })
}

fn workspace_search_selected(ui: &UIStateGuard, focus: SearchFocusState) -> Option<usize> {
    let PageState::Search { state, .. } = ui.current_page() else {
        return None;
    };
    match focus {
        SearchFocusState::Category | SearchFocusState::Input => None,
        SearchFocusState::Tracks => state.track_list.selected(),
        SearchFocusState::Videos => state.video_list.selected(),
        SearchFocusState::Albums => state.album_list.selected(),
        SearchFocusState::Artists => state.artist_list.selected(),
        SearchFocusState::Playlists => state.playlist_list.selected(),
        SearchFocusState::Shows => state.show_list.selected(),
        SearchFocusState::Episodes => state.episode_list.selected(),
    }
}

fn workspace_search_empty_message(status: UiViewStatus) -> (&'static str, Style) {
    match status {
        UiViewStatus::Loading => ("Searching…", Style::default()),
        UiViewStatus::Empty => ("No results", Style::default()),
        UiViewStatus::Failed { message, .. }
        | UiViewStatus::Unsupported { message, .. }
        | UiViewStatus::Partial { message, .. }
        | UiViewStatus::Superseded { message, .. } => (message, Style::default()),
        UiViewStatus::Idle | UiViewStatus::Ready => ("No items", Style::default()),
    }
}

fn render_workspace_search_panel(
    frame: &mut Frame,
    theme: &config::Theme,
    rect: Rect,
    group: &WorkspaceSearchGroup,
    active: bool,
    status: UiViewStatus,
    state: &mut ListState,
    focused_overflow: config::FocusedRowOverflow,
    focused_phase: usize,
    hits: &mut Vec<(Rect, WorkspaceHit)>,
) {
    if rect.width < 4 || rect.height < 3 {
        return;
    }
    frame.render_widget(Block::default().style(theme.workspace_base()), rect);
    let heading = Rect::new(
        rect.x.saturating_add(2),
        rect.y,
        rect.width.saturating_sub(3),
        1,
    );
    workspace_text(frame, heading, group.label, theme.workspace_heading());
    workspace_rule(
        frame,
        theme,
        Rect::new(
            rect.x.saturating_add(1),
            rect.y.saturating_add(1),
            rect.width.saturating_sub(2),
            1,
        ),
    );

    let viewport = rect.height.saturating_sub(2);
    utils::adjust_list_offset(state, group.total_items, viewport);
    if group.total_items > 0 {
        let count = workspace_search_loaded_count(group.total_items, viewport);
        workspace_text(
            frame,
            Rect::new(
                rect.right()
                    .saturating_sub(count.chars().count() as u16 + 2),
                rect.y,
                count.chars().count() as u16,
                1,
            ),
            count,
            theme.workspace_secondary_text(),
        );
    }
    let offset = state.offset();
    let local_offset = offset.saturating_sub(group.item_offset);
    let end = local_offset
        .saturating_add(viewport as usize)
        .min(group.items.len());
    let row_width = rect.width.saturating_sub(2);
    for local_index in local_offset..end {
        let index = group.item_offset.saturating_add(local_index);
        let y = rect
            .y
            .saturating_add(2)
            .saturating_add((local_index - local_offset) as u16);
        let row = Rect::new(rect.x.saturating_add(1), y, row_width, 1);
        let selected = active && state.selected() == Some(index);
        let row_style = if selected {
            theme.workspace_selection_active()
        } else {
            theme.workspace_base()
        };
        frame.render_widget(Block::default().style(row_style), row);

        let metadata_width = group
            .metadata_width
            .min(row.width.saturating_sub(4) as usize);
        let title_x = row.x.saturating_add(1);
        let metadata_x = row
            .right()
            .saturating_sub(1)
            .saturating_sub(metadata_width as u16);
        let title_width = if metadata_width == 0 {
            row.right().saturating_sub(title_x).saturating_sub(1)
        } else {
            metadata_x.saturating_sub(title_x).saturating_sub(2)
        };
        let text_style = if selected {
            theme.workspace_selection_active()
        } else {
            theme.workspace_base()
        };
        workspace_text(
            frame,
            Rect::new(title_x, y, title_width, 1),
            utils::focused_row_text(
                &group.items[local_index].title,
                title_width as usize,
                selected,
                focused_overflow,
                focused_phase,
            ),
            text_style,
        );
        if metadata_width > 0 && !group.items[local_index].metadata.is_empty() {
            workspace_text(
                frame,
                Rect::new(metadata_x, y, metadata_width as u16, 1),
                utils::focused_row_text(
                    &group.items[local_index].metadata,
                    metadata_width,
                    selected,
                    focused_overflow,
                    focused_phase,
                ),
                if selected {
                    theme.workspace_selection_active()
                } else {
                    theme.workspace_secondary_text()
                },
            );
        }
        hits.push((
            row,
            WorkspaceHit::SearchRow {
                focus: group.focus,
                index,
            },
        ));
    }
    if group.total_items == 0 {
        let (message, fallback_style) = workspace_search_empty_message(status);
        let style = match status {
            UiViewStatus::Failed { .. }
            | UiViewStatus::Unsupported { .. }
            | UiViewStatus::Partial { .. }
            | UiViewStatus::Superseded { .. } => theme.workspace_status_warning(),
            _ => theme.workspace_secondary_text().patch(fallback_style),
        };
        workspace_text(
            frame,
            Rect::new(
                rect.x.saturating_add(2),
                rect.y.saturating_add(2),
                row_width,
                1,
            ),
            message,
            style,
        );
    }

    let rail_style = if active {
        theme.workspace_focus_indicator()
    } else {
        theme.workspace_base()
    };
    utils::render_vertical_rule(
        frame,
        Rect::new(rect.x, rect.y, 1, rect.height),
        if active { "│" } else { " " },
        rail_style,
    );
    if group.total_items > viewport as usize && viewport > 0 {
        let rail = rect.height.saturating_sub(2);
        let thumb = (u32::from(rail)
            .saturating_mul(u32::from(viewport))
            .checked_div(group.total_items as u32)
            .unwrap_or(1)
            .max(1)) as u16;
        let travel = rail.saturating_sub(thumb);
        let max_offset = group.total_items.saturating_sub(viewport as usize);
        let thumb_offset = if max_offset == 0 {
            0
        } else {
            (u32::from(travel)
                .saturating_mul(offset as u32)
                .checked_div(max_offset as u32)
                .unwrap_or(0)) as u16
        };
        let track = Rect::new(
            rect.right().saturating_sub(1),
            rect.y.saturating_add(2),
            1,
            rail,
        );
        utils::render_vertical_rule(frame, track, "│", theme.workspace_scrollbar_track());
        utils::render_vertical_rule(
            frame,
            Rect::new(track.x, track.y.saturating_add(thumb_offset), 1, thumb),
            "┃",
            theme.workspace_scrollbar_thumb(),
        );
    }
}

/// Search pages shorter than this use the compact, results-first layout.
const SEARCH_COMPACT_BELOW_HEIGHT: u16 = 16;

fn render_workspace_search_page(
    is_active: bool,
    frame: &mut Frame,
    state: &SharedState,
    ui: &mut UIStateGuard,
    rect: Rect,
) {
    let (focus, category, query, provider, status, input) = match ui.current_page() {
        PageState::Search {
            state,
            current_query,
            line_input,
        } => (
            state.focus,
            state.category,
            current_query.clone(),
            state.provider.unwrap_or(ui.active_provider),
            state.search_lifecycle.view_status(),
            line_input.clone(),
        ),
        _ => return,
    };
    let mut layout = ui
        .layout_policy()
        .workspace(rect, WorkspaceLayoutKind::Search);
    // Search's navigation is intentionally hidden before the 80-column
    // breakpoint so the query and selected group retain usable width.
    if rect.width < 80 || rect.height < 20 {
        layout = crate::ui::WorkspaceLayout {
            content: rect,
            ..crate::ui::WorkspaceLayout::default()
        };
    }
    ui.workspace_layout = layout;
    if layout.show_navigation {
        let playback_provider = state
            .player
            .read()
            .effective_playback_provider(ui.active_provider);
        render_workspace_navigation(frame, ui, layout.navigation, playback_provider);
        workspace_vertical_rule(
            frame,
            &ui.theme,
            Rect::new(layout.navigation.right(), rect.y, 1, rect.height),
        );
    }
    let content = layout.content;
    if content.width < 8 || content.height < 4 {
        return;
    }
    frame.render_widget(Block::default().style(ui.theme.workspace_base()), content);
    // Short pages keep the results: the title shares its row with the
    // categories, the query box loses its padding, and the spacer and
    // status line go.
    let compact = content.height < SEARCH_COMPACT_BELOW_HEIGHT;

    let title_y = content.y.saturating_add(u16::from(!compact));
    let title = Rect::new(
        content.x.saturating_add(2),
        title_y,
        content.width.saturating_sub(4),
        1,
    );
    workspace_text(frame, title, "Search", ui.theme.workspace_heading());
    let (category_x, category_y) = if compact {
        (content.x.saturating_add(11), title_y)
    } else {
        (content.x.saturating_add(2), content.y.saturating_add(2))
    };
    let category_limit = content.right().saturating_sub(2);
    let mut category_cursor = category_x;
    for (index, pane) in std::iter::once(None)
        .chain(
            crate::command::provider_capabilities(provider)
                .search_panes()
                .iter()
                .copied()
                .map(Some),
        )
        .enumerate()
    {
        let label = workspace_search_category_label(provider, pane);
        let label_width = label.chars().count() as u16;
        if category_cursor >= category_limit {
            break;
        }
        let label_width = label_width.min(category_limit.saturating_sub(category_cursor));
        let selected = category == pane;
        workspace_text(
            frame,
            Rect::new(category_cursor, category_y, label_width, 1),
            utils::bounded_text(label, label_width as usize),
            if selected {
                ui.theme.workspace_heading()
            } else {
                ui.theme.workspace_secondary_text()
            },
        );
        ui.workspace_hits.push((
            Rect::new(category_cursor, category_y, label_width, 1),
            WorkspaceHit::SearchCategory(pane),
        ));
        let gap = if index == 0 { 4 } else { 3 };
        category_cursor = category_cursor
            .saturating_add(label_width)
            .saturating_add(gap);
    }

    let query_rect = if compact {
        Rect::new(
            content.x.saturating_add(2),
            title_y.saturating_add(1),
            content.width.saturating_sub(4),
            1,
        )
    } else {
        Rect::new(
            content.x.saturating_add(2),
            content.y.saturating_add(3),
            content.width.saturating_sub(4),
            3.min(content.height.saturating_sub(3)),
        )
    };
    frame.render_widget(
        Block::default().style(ui.theme.workspace_elevated_surface()),
        query_rect,
    );
    let input_active = is_active
        && ui.workspace_focus == WorkspaceFocusState::Context
        && focus == SearchFocusState::Input;
    if input_active {
        utils::render_vertical_rule(
            frame,
            Rect::new(query_rect.x, query_rect.y, 1, query_rect.height),
            "│",
            ui.theme.workspace_focus_indicator(),
        );
    }
    let prompt_y = query_rect
        .y
        .saturating_add(u16::from(query_rect.height >= 3));
    workspace_text(
        frame,
        Rect::new(query_rect.x.saturating_add(2), prompt_y, 1, 1),
        ">",
        ui.theme.workspace_focus_indicator(),
    );
    let hint = if input_active {
        "enter Search"
    } else {
        "esc Edit query"
    };
    // The design reserves a fixed 24-cell hint region. Keeping it fixed means
    // query text has a deterministic viewport and never runs into the hint
    // when the action label changes.
    let hint_width = 24.min(query_rect.width.saturating_sub(6));
    let hint_rect = Rect::new(
        query_rect.right().saturating_sub(hint_width + 1),
        prompt_y,
        hint_width,
        1,
    );
    workspace_text(frame, hint_rect, hint, ui.theme.workspace_hint_text());
    ui.workspace_hits
        .push((query_rect, WorkspaceHit::SearchInput));

    let text_rect = Rect::new(
        query_rect.x.saturating_add(4),
        prompt_y,
        hint_rect
            .x
            .saturating_sub(query_rect.x.saturating_add(4))
            .saturating_sub(2),
        1,
    );
    let text_style = ui.theme.workspace_base();
    let cursor_style = Style::default()
        .fg(text_style.bg.unwrap_or(Color::Reset))
        .bg(text_style.fg.unwrap_or(Color::Reset));
    frame.render_widget(
        input.widget_with_styles(
            input_active,
            text_style,
            cursor_style,
            "Enter a search term",
            ui.theme.workspace_secondary_text(),
        ),
        text_rect,
    );

    let status_divider_y = if compact {
        content.bottom()
    } else {
        content.bottom().saturating_sub(2)
    };
    let grid_y = if compact {
        query_rect.bottom().saturating_add(1)
    } else {
        content.y.saturating_add(7)
    }
    .min(status_divider_y);
    let grid = Rect::new(
        content.x.saturating_add(2),
        grid_y,
        content.width.saturating_sub(4),
        status_divider_y.saturating_sub(grid_y),
    );
    let wide = category.is_none() && rect.width >= 140 && rect.height >= 32;
    let visible_focus = workspace_search_visible_focus(wide, category, focus);
    let group_count = crate::command::provider_capabilities(provider)
        .search_panes()
        .len();
    let group_rects = if wide {
        workspace_search_group_rects(grid, group_count)
    } else {
        vec![grid]
    };
    let groups = {
        let data = state.data.read();
        let projection_windows = workspace_search_projection_windows(
            ui.current_page_mut(),
            provider,
            &data,
            &query,
            wide,
            visible_focus,
            &group_rects,
        );
        workspace_search_groups(
            provider,
            &data,
            &query,
            visible_focus,
            Some(&projection_windows),
        )
    };
    let theme = ui.theme.clone();
    let focused_overflow = ui.presentation.focused_row_overflow;
    let focused_phase = ui.focused_marquee_phase();
    let mut panel_hits = Vec::new();
    if wide {
        for (group, panel_rect) in groups.iter().zip(group_rects.iter().copied()) {
            let active = is_active
                && ui.workspace_focus == WorkspaceFocusState::Context
                && focus == group.focus;
            if let Some(list_state) =
                workspace_search_list_state_mut(ui.current_page_mut(), group.focus)
            {
                render_workspace_search_panel(
                    frame,
                    &theme,
                    panel_rect,
                    group,
                    active,
                    status,
                    list_state,
                    focused_overflow,
                    focused_phase,
                    &mut panel_hits,
                );
            }
        }
        if groups.len() >= 2 && group_rects.len() >= 2 {
            let left = group_rects[0];
            let right = group_rects[1];
            let divider_x = left.right().saturating_add(1);
            if divider_x < right.x {
                workspace_vertical_rule(
                    frame,
                    &theme,
                    Rect::new(divider_x, grid.y, 1, grid.height),
                );
            }
        }
    } else if let Some(group) = if let Some(category) = category {
        let category_focus = SearchFocusState::from_provider_pane(category);
        groups.iter().find(|group| group.focus == category_focus)
    } else if matches!(focus, SearchFocusState::Category | SearchFocusState::Input) {
        groups.first()
    } else {
        groups.iter().find(|group| group.focus == focus)
    } {
        let active =
            is_active && ui.workspace_focus == WorkspaceFocusState::Context && focus == group.focus;
        if let Some(list_state) =
            workspace_search_list_state_mut(ui.current_page_mut(), group.focus)
        {
            render_workspace_search_panel(
                frame,
                &theme,
                grid,
                group,
                active,
                status,
                list_state,
                focused_overflow,
                focused_phase,
                &mut panel_hits,
            );
        }
    }
    ui.workspace_hits.extend(panel_hits);
    if compact {
        return;
    }

    workspace_rule(
        frame,
        &ui.theme,
        Rect::new(content.x, status_divider_y, content.width, 1),
    );
    let status_label = groups
        .iter()
        .find(|group| group.focus == focus)
        .map_or_else(
            || workspace_search_category_label(provider, category),
            |group| group.label,
        );
    let status_text = if matches!(focus, SearchFocusState::Category | SearchFocusState::Input) {
        format!("Results for: {query}")
    } else {
        let selected = workspace_search_selected(ui, focus)
            .and_then(|index| {
                let group = groups.iter().find(|group| group.focus == focus)?;
                (group.selected_index == Some(index))
                    .then_some(group.selected_item.as_ref())
                    .flatten()
            })
            .map_or("none selected", |item| item.title.as_str());
        format!("{status_label} · {selected}")
    };
    workspace_text(
        frame,
        Rect::new(
            content.x.saturating_add(2),
            status_divider_y.saturating_add(1),
            content.width.saturating_sub(4),
            1,
        ),
        utils::bounded_text(&status_text, content.width.saturating_sub(4) as usize),
        ui.theme.workspace_secondary_text(),
    );
    let status_action_rect = Rect::new(
        content.right().saturating_sub(22),
        status_divider_y.saturating_add(1),
        20.min(content.width.saturating_sub(6)),
        1,
    );
    if !status_action_rect.is_empty() {
        if matches!(focus, SearchFocusState::Category | SearchFocusState::Input) {
            let status_text = if focus == SearchFocusState::Category {
                format!(
                    "enter Select {}",
                    workspace_search_category_label(provider, category)
                )
            } else {
                String::new()
            };
            if focus == SearchFocusState::Category {
                workspace_text(
                    frame,
                    status_action_rect,
                    status_text,
                    ui.theme.workspace_hint_text(),
                );
            } else {
                workspace_text(
                    frame,
                    status_action_rect,
                    "Editing query",
                    ui.theme.workspace_hint_text(),
                );
            }
        } else {
            let action = match focus {
                SearchFocusState::Tracks
                | SearchFocusState::Videos
                | SearchFocusState::Episodes => "Play track",
                SearchFocusState::Albums => "Open album",
                SearchFocusState::Artists => "Open artist",
                SearchFocusState::Playlists => "Open playlist",
                SearchFocusState::Shows => "Open show",
                SearchFocusState::Category | SearchFocusState::Input => unreachable!(),
            };
            let choose = workspace_command_key(crate::command::Command::ChooseSelected);
            let action_line = Line::from(vec![
                Span::styled(format!("{choose} "), ui.theme.workspace_hint_key()),
                Span::styled(action, ui.theme.workspace_hint_text()),
            ]);
            frame.render_widget(
                Paragraph::new(action_line).alignment(Alignment::Right),
                status_action_rect,
            );
        }
    }
}

#[derive(Clone, Debug)]
struct WorkspaceListRow {
    primary: String,
    secondary: String,
}

#[derive(Clone, Debug, Default)]
struct WorkspaceCollectionRows {
    rows: Vec<WorkspaceListRow>,
    total: usize,
    offset: usize,
}

impl WorkspaceCollectionRows {
    fn from_owned(rows: Vec<WorkspaceListRow>) -> Self {
        Self {
            total: rows.len(),
            rows,
            offset: 0,
        }
    }

    fn row_at(&self, index: usize) -> Option<&WorkspaceListRow> {
        index
            .checked_sub(self.offset)
            .and_then(|local_index| self.rows.get(local_index))
    }
}

impl std::fmt::Display for WorkspaceListRow {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(formatter, "{} {}", self.primary, self.secondary)
    }
}

fn workspace_filtered_rows(
    ui: &crate::state::UIState,
    rows: Vec<WorkspaceListRow>,
) -> Vec<WorkspaceListRow> {
    // Keep the unfiltered projection owned by its caller. In particular, the
    // fuzzy-search build used to turn every row into `&WorkspaceListRow` and
    // clone the complete collection back into a new vector even when no
    // search popup was active.
    let Some(query) = ui.search_query().filter(|query| !query.trim().is_empty()) else {
        return rows;
    };

    #[cfg(feature = "fzf")]
    {
        let indices =
            crate::utils::fuzzy_search_item_indices(&rows, &query.to_lowercase(), |row, label| {
                use std::fmt::Write;

                write!(label, "{} {}", row.primary, row.secondary)
                    .expect("writing to a String cannot fail");
            });
        let mut rows = rows.into_iter().map(Some).collect::<Vec<_>>();
        indices
            .into_iter()
            .filter_map(|index| rows.get_mut(index).and_then(Option::take))
            .collect()
    }

    #[cfg(not(feature = "fzf"))]
    {
        let query = query.to_lowercase();
        let terms = query.split(' ').filter(|term| !term.is_empty());
        rows.into_iter()
            .filter(|row| {
                let primary = row.primary.to_lowercase();
                let mut secondary = None;
                terms.clone().all(|term| {
                    primary.contains(term)
                        || secondary
                            .get_or_insert_with(|| row.secondary.to_lowercase())
                            .contains(term)
                })
            })
            .collect()
    }
}

fn workspace_row_window(
    total: usize,
    current_offset: usize,
    selected: Option<usize>,
    viewport: usize,
) -> (usize, usize) {
    // Project from the offset the panel settles on in this frame. The stored
    // offset is the previous frame's, so a selection that just moved past the
    // viewport edge would otherwise fall outside the projected rows and draw
    // blank until a later frame.
    let viewport_rows = u16::try_from(viewport).unwrap_or(u16::MAX);
    let start = utils::list_offset_for_selection(selected, current_offset, total, viewport_rows);
    (start, start.saturating_add(viewport).min(total))
}

fn append_workspace_row_window<T, I, F>(
    rows: &mut Vec<WorkspaceListRow>,
    items: I,
    start: &mut usize,
    remaining: &mut usize,
    make_row: F,
) where
    I: IntoIterator<Item = T>,
    F: Fn(T) -> WorkspaceListRow,
{
    if *remaining == 0 {
        return;
    }
    for item in items {
        if *remaining == 0 {
            break;
        }
        if *start > 0 {
            *start -= 1;
            continue;
        }
        rows.push(make_row(item));
        *remaining -= 1;
    }
}

fn workspace_row_selection_style(ui: &crate::state::UIState, active: bool) -> Style {
    if active {
        ui.theme.workspace_selection_active()
    } else {
        ui.theme.workspace_selection_inactive()
    }
}

fn workspace_rule(frame: &mut Frame, theme: &config::Theme, rect: Rect) {
    utils::render_horizontal_rule(frame, rect, "─", theme.workspace_border());
}

fn workspace_vertical_rule(frame: &mut Frame, theme: &config::Theme, rect: Rect) {
    utils::render_vertical_rule(frame, rect, "│", theme.workspace_border());
}

fn workspace_text(frame: &mut Frame, rect: Rect, text: impl Into<String>, style: Style) {
    if rect.width == 0 || rect.height == 0 {
        return;
    }
    frame.render_widget(Paragraph::new(text.into()).style(style), rect);
}

fn workspace_scope_style(
    ui: &UIStateGuard,
    kind: WorkspaceScopeKind,
    provider: Option<config::ActiveProvider>,
) -> Style {
    match (kind, provider) {
        (WorkspaceScopeKind::Browsing | WorkspaceScopeKind::Playback, Some(provider)) => {
            ui.theme.workspace_provider_scope(provider)
        }
        _ => ui.theme.workspace_secondary_text(),
    }
}

fn render_workspace_navigation(
    frame: &mut Frame,
    ui: &mut UIStateGuard,
    nav: Rect,
    playback_provider: config::ActiveProvider,
) {
    if nav.is_empty() {
        return;
    }
    frame.render_widget(Block::default().style(ui.theme.workspace_panel()), nav);

    workspace_text(
        frame,
        crate::ui::components::navigation::library_heading_rect(nav),
        "Library",
        ui.theme.workspace_heading(),
    );

    let account = match ui.active_provider {
        config::ActiveProvider::Spotify => ui.spotify_account_label.as_deref(),
        config::ActiveProvider::YouTubeMusic => ui.youtube_account_label.as_deref(),
    }
    .unwrap_or("account unavailable");
    let scope_width = nav.width.saturating_sub(4) as usize;
    let short_name = |provider: config::ActiveProvider| match provider {
        config::ActiveProvider::Spotify => "Spotify",
        config::ActiveProvider::YouTubeMusic => "YouTube",
    };
    // All three rows use the same, longest form that fits every one of them,
    // so they never mix full and shortened wording. Narrow rails drop the
    // chevron before they drop the verb.
    let forms: [fn(&str, &str, &str) -> String; 4] = [
        |verb, full, _| format!("› {verb} {full}"),
        |verb, _, short| format!("› {verb} {short}"),
        |verb, _, short| format!("{verb} {short}"),
        |_, _, short| short.to_owned(),
    ];
    let render =
        |form: fn(&str, &str, &str) -> String, verb: &str, provider: config::ActiveProvider| {
            form(verb, provider.title(), short_name(provider))
        };
    let form_index = (0..forms.len())
        .find(|&index| {
            [
                render(forms[index], "Browse", ui.active_provider),
                render(forms[index], "Play", playback_provider),
            ]
            .iter()
            .all(|line| line.chars().count() <= scope_width)
        })
        .unwrap_or(forms.len() - 1);
    let form = forms[form_index];
    let account_line = if form_index < 2 {
        format!("› @{account}")
    } else {
        format!("@{account}")
    };
    let scope_lines = [
        (
            WorkspaceScopeKind::Browsing,
            render(form, "Browse", ui.active_provider),
            Some(ui.active_provider),
        ),
        (WorkspaceScopeKind::Account, account_line, None),
        (
            WorkspaceScopeKind::Playback,
            render(form, "Play", playback_provider),
            Some(playback_provider),
        ),
    ];

    let row_style = ui.theme.workspace_panel();
    for rail_item in workspace_rail_items() {
        match rail_item {
            WorkspaceRailItem::Route(item) => {
                let row = route_row_rect(nav, item);
                if row.is_empty() || row.y >= nav.bottom() {
                    continue;
                }
                let style = if ui.workspace_navigation_is_active(item) {
                    ui.theme.workspace_navigation_active()
                } else {
                    row_style
                };
                frame.render_widget(Block::default().style(style), row);
                workspace_text(frame, label_rect(row), item.label(), style);
                ui.workspace_hits.push((row, rail_item.hit()));
            }
            WorkspaceRailItem::Scope(kind) => {
                let row = scope_row_rect(nav, kind);
                if row.is_empty() || row.y >= nav.bottom() {
                    continue;
                }
                let (_, line, provider) = &scope_lines[kind.index()];
                // Scope text starts one cell in and may use one cell of the
                // rail's right padding; the hit row grows to cover it.
                let row = Rect::new(row.x, row.y, row.width.saturating_add(1), row.height);
                // Scope rows are controls, not passive status text. Give them
                // the same full-row surface used by route hits while keeping
                // their hit variant separate from ordinary navigation.
                frame.render_widget(Block::default().style(row_style), row);
                workspace_text(
                    frame,
                    Rect::new(
                        row.x.saturating_add(1),
                        row.y,
                        row.width.saturating_sub(1),
                        row.height,
                    ),
                    utils::bounded_text(line, scope_width),
                    workspace_scope_style(ui, kind, *provider),
                );
                ui.workspace_hits.push((row, rail_item.hit()));
            }
        }
    }

    let divider = route_divider_rect(nav);
    if divider.y < nav.bottom() {
        workspace_rule(frame, &ui.theme, divider);
    }
}

fn render_workspace_collection_panel(
    frame: &mut Frame,
    theme: &config::Theme,
    rect: Rect,
    title: &str,
    projection: &WorkspaceCollectionRows,
    active: bool,
    metadata_width: usize,
    state: &mut ListState,
    focus: LibraryFocusState,
    focused_overflow: config::FocusedRowOverflow,
    focused_phase: usize,
    hits: &mut Vec<(Rect, WorkspaceHit)>,
) {
    if rect.width < 4 || rect.height < 3 {
        return;
    }
    let heading = Rect::new(
        rect.x.saturating_add(2),
        rect.y,
        rect.width.saturating_sub(3),
        1,
    );
    workspace_text(frame, heading, title, theme.workspace_heading());
    workspace_rule(
        frame,
        theme,
        Rect::new(
            rect.x.saturating_add(1),
            rect.y.saturating_add(1),
            rect.width.saturating_sub(2),
            1,
        ),
    );

    let rail_style = if active {
        theme.workspace_focus_indicator()
    } else {
        theme.workspace_base()
    };
    utils::render_vertical_rule(
        frame,
        Rect::new(rect.x, rect.y, 1, rect.height),
        if active { "│" } else { " " },
        rail_style,
    );

    let list_rect = Rect::new(
        rect.x.saturating_add(1),
        rect.y.saturating_add(2),
        // Keep the panel's final column reserved for the scrollbar even when
        // no thumb is currently needed. The shared list renderer then gives
        // the content/highlight the full `(c+1, w-2)` design-v1 width.
        rect.width.saturating_sub(1),
        rect.height.saturating_sub(2),
    );
    if projection.total == 0 {
        workspace_text(
            frame,
            Rect::new(list_rect.x, list_rect.y, list_rect.width, 1),
            "No items",
            theme.workspace_secondary_text(),
        );
    } else {
        utils::adjust_list_offset(state, projection.total, list_rect.height);
        let content_width = list_rect.width.saturating_sub(1) as usize;
        let metadata_width = metadata_width.min(content_width.saturating_sub(4));
        let primary_width = if metadata_width == 0 {
            content_width.saturating_sub(2)
        } else {
            // Reserve one leading content gutter, the two-cell gap before
            // metadata, and one trailing cell before the scrollbar. This
            // keeps title/metadata anchors stable as selection changes.
            content_width.saturating_sub(metadata_width.saturating_add(4))
        };
        let selected_index = state.selected();
        let start = state.offset();
        let end = start
            .saturating_add(list_rect.height as usize)
            .min(projection.total);
        let items = (start..end)
            .filter_map(|index| {
                projection
                    .row_at(index)
                    .map(|row| (index.saturating_sub(start), row))
            })
            .map(|(visible_index, row)| {
                let selected = selected_index == Some(start + visible_index);
                // Only the focused panel scrolls its selected row.
                let scrolls = selected && active;
                let primary = utils::focused_row_text(
                    &row.primary,
                    primary_width,
                    scrolls,
                    focused_overflow,
                    focused_phase,
                );
                let secondary = utils::focused_row_text(
                    &row.secondary,
                    metadata_width,
                    scrolls,
                    focused_overflow,
                    focused_phase,
                );
                let row_style = if selected {
                    if active {
                        theme.workspace_selection_active()
                    } else {
                        theme.workspace_selection_inactive()
                    }
                } else {
                    theme.workspace_base()
                };
                let mut spans = vec![
                    Span::styled(" ", row_style),
                    Span::styled(format!("{primary:<primary_width$}"), row_style),
                ];
                if metadata_width > 0 {
                    spans.push(Span::styled("  ", row_style));
                    spans.push(Span::styled(
                        secondary,
                        if selected {
                            row_style
                        } else {
                            theme.workspace_secondary_text()
                        },
                    ));
                }
                spans.push(Span::styled(" ", row_style));
                let line = Line::from(spans);
                ListItem::new(line)
            })
            .collect::<Vec<_>>();
        let list = List::new(items).highlight_style(if active {
            theme.workspace_selection_active()
        } else {
            theme.workspace_selection_inactive()
        });
        utils::render_prepared_list_window_with_styled_scrollbar(
            frame,
            list,
            list_rect,
            projection.total,
            state,
            theme.workspace_scrollbar_track(),
            theme.workspace_scrollbar_thumb(),
        );
        let content_width = if list_rect.width >= 2 {
            list_rect.width - 1
        } else {
            list_rect.width
        };
        let start = state.offset();
        let end = start
            .saturating_add(list_rect.height as usize)
            .min(projection.total);
        for index in start..end {
            hits.push((
                Rect::new(
                    list_rect.x,
                    list_rect.y.saturating_add((index - start) as u16),
                    content_width,
                    1,
                ),
                WorkspaceHit::LibraryRow { focus, index },
            ));
        }
    }
}

fn workspace_library_focus_is_visible(
    visible_focus: Option<LibraryFocusState>,
    collection: LibraryFocusState,
) -> bool {
    visible_focus.is_none_or(|visible| visible == collection)
}

fn workspace_library_rows(
    ui: &UIStateGuard,
    data: &DataReadGuard,
    visible_focus: Option<LibraryFocusState>,
    list_offset: usize,
    list_selected: Option<usize>,
    viewport: usize,
) -> (
    WorkspaceCollectionRows,
    WorkspaceCollectionRows,
    WorkspaceCollectionRows,
) {
    let query_active = ui
        .search_query()
        .is_some_and(|query| !query.trim().is_empty());
    let windowed = visible_focus.is_some();
    let project_window = |total: usize| {
        if windowed {
            workspace_row_window(total, list_offset, list_selected, viewport)
        } else {
            (0, total)
        }
    };
    match ui.active_provider {
        config::ActiveProvider::Spotify => {
            let folder_id = match ui.current_page() {
                PageState::Library { state } => state.playlist_folder_id,
                _ => 0,
            };
            let playlists = if workspace_library_focus_is_visible(
                visible_focus,
                LibraryFocusState::Playlists,
            ) {
                let folder_items = data.user_data.playlists.iter().filter(|item| match item {
                    PlaylistFolderItem::Playlist(playlist) => {
                        playlist.current_folder_id == folder_id
                    }
                    PlaylistFolderItem::Folder(folder) => folder.current_id == folder_id,
                });
                let (youtube_account_id, youtube_account_epoch) =
                    current_youtube_projection_scope(ui);
                if query_active || !windowed {
                    let mut rows = folder_items
                        .map(|item| match item {
                            PlaylistFolderItem::Playlist(playlist) => WorkspaceListRow {
                                primary: playlist.name.clone(),
                                secondary: playlist.owner.0.clone(),
                            },
                            PlaylistFolderItem::Folder(folder) => WorkspaceListRow {
                                primary: folder.name.clone(),
                                secondary: "folder".to_owned(),
                            },
                        })
                        .collect::<Vec<_>>();
                    rows.extend(data.unified_playlists.iter().map(|playlist| {
                        let link = data
                            .playlist_links
                            .iter()
                            .find(|link| link.unified_playlist_id == playlist.id);
                        WorkspaceListRow {
                            primary: unified_playlist_library_label(
                                playlist,
                                link,
                                &youtube_account_id,
                                youtube_account_epoch,
                            ),
                            secondary: "Unified".to_owned(),
                        }
                    }));
                    WorkspaceCollectionRows::from_owned(workspace_filtered_rows(ui, rows))
                } else {
                    let folder_count = folder_items.clone().count();
                    let total = folder_count.saturating_add(data.unified_playlists.len());
                    let (start, end) = project_window(total);
                    let mut remaining = end.saturating_sub(start);
                    let mut skip = start;
                    let mut rows = Vec::with_capacity(remaining);
                    append_workspace_row_window(
                        &mut rows,
                        folder_items,
                        &mut skip,
                        &mut remaining,
                        |item| match item {
                            PlaylistFolderItem::Playlist(playlist) => WorkspaceListRow {
                                primary: playlist.name.clone(),
                                secondary: playlist.owner.0.clone(),
                            },
                            PlaylistFolderItem::Folder(folder) => WorkspaceListRow {
                                primary: folder.name.clone(),
                                secondary: "folder".to_owned(),
                            },
                        },
                    );
                    append_workspace_row_window(
                        &mut rows,
                        data.unified_playlists.iter(),
                        &mut skip,
                        &mut remaining,
                        |playlist| {
                            let link = data
                                .playlist_links
                                .iter()
                                .find(|link| link.unified_playlist_id == playlist.id);
                            WorkspaceListRow {
                                primary: unified_playlist_library_label(
                                    playlist,
                                    link,
                                    &youtube_account_id,
                                    youtube_account_epoch,
                                ),
                                secondary: "Unified".to_owned(),
                            }
                        },
                    );
                    WorkspaceCollectionRows {
                        rows,
                        total,
                        offset: start,
                    }
                }
            } else {
                WorkspaceCollectionRows::default()
            };
            let albums = if workspace_library_focus_is_visible(
                visible_focus,
                LibraryFocusState::SavedAlbums,
            ) {
                if query_active || !windowed {
                    WorkspaceCollectionRows::from_owned(workspace_filtered_rows(
                        ui,
                        data.user_data
                            .saved_albums
                            .iter()
                            .map(|album| WorkspaceListRow {
                                primary: album.name.clone(),
                                secondary: album
                                    .artists
                                    .iter()
                                    .map(|artist| artist.name.clone())
                                    .collect::<Vec<_>>()
                                    .join(", "),
                            })
                            .collect(),
                    ))
                } else {
                    let total = data.user_data.saved_albums.len();
                    let (start, end) = project_window(total);
                    WorkspaceCollectionRows {
                        rows: data.user_data.saved_albums[start..end]
                            .iter()
                            .map(|album| WorkspaceListRow {
                                primary: album.name.clone(),
                                secondary: album
                                    .artists
                                    .iter()
                                    .map(|artist| artist.name.clone())
                                    .collect::<Vec<_>>()
                                    .join(", "),
                            })
                            .collect(),
                        total,
                        offset: start,
                    }
                }
            } else {
                WorkspaceCollectionRows::default()
            };
            let artists = if workspace_library_focus_is_visible(
                visible_focus,
                LibraryFocusState::FollowedArtists,
            ) {
                if query_active || !windowed {
                    WorkspaceCollectionRows::from_owned(workspace_filtered_rows(
                        ui,
                        data.user_data
                            .followed_artists
                            .iter()
                            .map(|artist| WorkspaceListRow {
                                primary: artist.name.clone(),
                                secondary: String::new(),
                            })
                            .collect(),
                    ))
                } else {
                    let total = data.user_data.followed_artists.len();
                    let (start, end) = project_window(total);
                    WorkspaceCollectionRows {
                        rows: data.user_data.followed_artists[start..end]
                            .iter()
                            .map(|artist| WorkspaceListRow {
                                primary: artist.name.clone(),
                                secondary: String::new(),
                            })
                            .collect(),
                        total,
                        offset: start,
                    }
                }
            } else {
                WorkspaceCollectionRows::default()
            };
            (playlists, albums, artists)
        }
        config::ActiveProvider::YouTubeMusic => {
            let library = &data.user_data.youtube_library;
            let playlists = if workspace_library_focus_is_visible(
                visible_focus,
                LibraryFocusState::Playlists,
            ) {
                let (youtube_account_id, youtube_account_epoch) =
                    current_youtube_projection_scope(ui);
                if query_active || !windowed {
                    let mut rows = vec![WorkspaceListRow {
                        primary: "[Liked] Music".to_owned(),
                        secondary: String::new(),
                    }];
                    rows.extend(library.playlists.iter().map(|playlist| WorkspaceListRow {
                        primary: playlist.name.clone(),
                        secondary: playlist.author.clone(),
                    }));
                    rows.extend(data.unified_playlists.iter().map(|playlist| {
                        let link = data
                            .playlist_links
                            .iter()
                            .find(|link| link.unified_playlist_id == playlist.id);
                        WorkspaceListRow {
                            primary: unified_playlist_library_label(
                                playlist,
                                link,
                                &youtube_account_id,
                                youtube_account_epoch,
                            ),
                            secondary: "Unified".to_owned(),
                        }
                    }));
                    WorkspaceCollectionRows::from_owned(workspace_filtered_rows(ui, rows))
                } else {
                    let total = 1usize
                        .saturating_add(library.playlists.len())
                        .saturating_add(data.unified_playlists.len());
                    let (start, end) = project_window(total);
                    let mut rows = Vec::with_capacity(end.saturating_sub(start));
                    let mut skip = start;
                    let mut remaining = end.saturating_sub(start);
                    append_workspace_row_window(
                        &mut rows,
                        std::iter::once(()),
                        &mut skip,
                        &mut remaining,
                        |()| WorkspaceListRow {
                            primary: "[Liked] Music".to_owned(),
                            secondary: String::new(),
                        },
                    );
                    append_workspace_row_window(
                        &mut rows,
                        library.playlists.iter(),
                        &mut skip,
                        &mut remaining,
                        |playlist| WorkspaceListRow {
                            primary: playlist.name.clone(),
                            secondary: playlist.author.clone(),
                        },
                    );
                    append_workspace_row_window(
                        &mut rows,
                        data.unified_playlists.iter(),
                        &mut skip,
                        &mut remaining,
                        |playlist| {
                            let link = data
                                .playlist_links
                                .iter()
                                .find(|link| link.unified_playlist_id == playlist.id);
                            WorkspaceListRow {
                                primary: unified_playlist_library_label(
                                    playlist,
                                    link,
                                    &youtube_account_id,
                                    youtube_account_epoch,
                                ),
                                secondary: "Unified".to_owned(),
                            }
                        },
                    );
                    WorkspaceCollectionRows {
                        rows,
                        total,
                        offset: start,
                    }
                }
            } else {
                WorkspaceCollectionRows::default()
            };
            let albums = if workspace_library_focus_is_visible(
                visible_focus,
                LibraryFocusState::SavedAlbums,
            ) {
                if query_active || !windowed {
                    WorkspaceCollectionRows::from_owned(workspace_filtered_rows(
                        ui,
                        library
                            .albums
                            .iter()
                            .map(|album| WorkspaceListRow {
                                primary: album.name.clone(),
                                secondary: format!("{} · {}", album.artist, album.year),
                            })
                            .collect(),
                    ))
                } else {
                    let total = library.albums.len();
                    let (start, end) = project_window(total);
                    WorkspaceCollectionRows {
                        rows: library.albums[start..end]
                            .iter()
                            .map(|album| WorkspaceListRow {
                                primary: album.name.clone(),
                                secondary: format!("{} · {}", album.artist, album.year),
                            })
                            .collect(),
                        total,
                        offset: start,
                    }
                }
            } else {
                WorkspaceCollectionRows::default()
            };
            let artists = if workspace_library_focus_is_visible(
                visible_focus,
                LibraryFocusState::FollowedArtists,
            ) {
                if query_active || !windowed {
                    WorkspaceCollectionRows::from_owned(workspace_filtered_rows(
                        ui,
                        library
                            .artists
                            .iter()
                            .map(|artist| WorkspaceListRow {
                                primary: artist.name.clone(),
                                secondary: artist.byline.clone(),
                            })
                            .collect(),
                    ))
                } else {
                    let total = library.artists.len();
                    let (start, end) = project_window(total);
                    WorkspaceCollectionRows {
                        rows: library.artists[start..end]
                            .iter()
                            .map(|artist| WorkspaceListRow {
                                primary: artist.name.clone(),
                                secondary: artist.byline.clone(),
                            })
                            .collect(),
                        total,
                        offset: start,
                    }
                }
            } else {
                WorkspaceCollectionRows::default()
            };
            (playlists, albums, artists)
        }
    }
}

/// Library pages shorter than this use the compact, collection-first layout.
const LIBRARY_COMPACT_BELOW_HEIGHT: u16 = 16;

fn render_workspace_library_page(
    _is_active: bool,
    frame: &mut Frame,
    state: &SharedState,
    ui: &mut UIStateGuard,
    rect: Rect,
) {
    let layout = ui
        .layout_policy()
        .workspace(rect, WorkspaceLayoutKind::Library);
    ui.workspace_layout = layout;
    if layout.show_navigation {
        let playback_provider = state
            .player
            .read()
            .effective_playback_provider(ui.active_provider);
        render_workspace_navigation(frame, ui, layout.navigation, playback_provider);
        workspace_vertical_rule(
            frame,
            &ui.theme,
            Rect::new(layout.navigation.right(), rect.y, 1, rect.height),
        );
    }
    if layout.content.is_empty() {
        return;
    }

    let focus = match ui.current_page() {
        PageState::Library { state } => state.focus,
        _ => return,
    };
    let visible_focus = Some(focus);
    // Short pages keep the collection: the title moves to the first row, and
    // the subtitle, the closing rule and the selection summary go.
    let compact = layout.content.height < LIBRARY_COMPACT_BELOW_HEIGHT;
    let content = if compact {
        Rect::new(
            layout.content.x,
            layout.content.y.saturating_add(1),
            layout.content.width,
            layout.content.height.saturating_sub(1),
        )
    } else {
        Rect::new(
            layout.content.x,
            layout.content.y.saturating_add(4),
            layout.content.width,
            layout.content.height.saturating_sub(6),
        )
    };
    let viewport = content.height.saturating_sub(2) as usize;
    let (list_offset, list_selected) = match ui.current_page() {
        PageState::Library { state } => {
            let list = match focus {
                LibraryFocusState::Playlists => &state.playlist_list,
                LibraryFocusState::SavedAlbums => &state.saved_album_list,
                LibraryFocusState::FollowedArtists => &state.followed_artist_list,
            };
            (list.offset(), list.selected())
        }
        _ => (0, None),
    };
    let (playlists, albums, artists) = {
        let data = state.data.read();
        workspace_library_rows(
            ui,
            &data,
            visible_focus,
            list_offset,
            list_selected,
            viewport,
        )
    };
    let context_focused = ui.workspace_focus == WorkspaceFocusState::Context;
    workspace_text(
        frame,
        Rect::new(
            layout.content.x.saturating_add(2),
            layout.content.y.saturating_add(u16::from(!compact)),
            layout.content.width.saturating_sub(4),
            1,
        ),
        "Library",
        ui.theme.workspace_heading(),
    );
    if !compact {
        let account = match ui.active_provider {
            config::ActiveProvider::Spotify => ui.spotify_account_label.as_deref(),
            config::ActiveProvider::YouTubeMusic => ui.youtube_account_label.as_deref(),
        }
        .unwrap_or("account unavailable");
        workspace_text(
            frame,
            Rect::new(
                layout.content.x.saturating_add(2),
                layout.content.y.saturating_add(2),
                layout.content.width.saturating_sub(4),
                1,
            ),
            format!("{} · {account}", ui.active_provider.title()),
            ui.theme.workspace_secondary_text(),
        );
    }

    let mut library_hits = Vec::new();
    let (title, projection, metadata_width) = match focus {
        LibraryFocusState::Playlists => ("Playlists", &playlists, 0),
        LibraryFocusState::SavedAlbums => ("Albums", &albums, 23),
        LibraryFocusState::FollowedArtists => ("Artists", &artists, 0),
    };
    let theme = ui.theme.clone();
    let focused_overflow = ui.presentation.focused_row_overflow;
    let focused_phase = ui.focused_marquee_phase();
    let PageState::Library { state: page_state } = ui.current_page_mut() else {
        return;
    };
    let list_state = match focus {
        LibraryFocusState::Playlists => &mut page_state.playlist_list,
        LibraryFocusState::SavedAlbums => &mut page_state.saved_album_list,
        LibraryFocusState::FollowedArtists => &mut page_state.followed_artist_list,
    };
    render_workspace_collection_panel(
        frame,
        &theme,
        content,
        title,
        projection,
        context_focused,
        metadata_width,
        list_state,
        focus,
        focused_overflow,
        focused_phase,
        &mut library_hits,
    );
    ui.workspace_hits.extend(library_hits);
    if compact {
        return;
    }

    let status_y = layout.content.bottom().saturating_sub(1);
    workspace_rule(
        frame,
        &ui.theme,
        Rect::new(
            layout.content.x,
            status_y.saturating_sub(1),
            layout.content.width,
            1,
        ),
    );
    let selected_index = match ui.current_page() {
        PageState::Library { state } => match focus {
            LibraryFocusState::Playlists => state.playlist_list.selected(),
            LibraryFocusState::SavedAlbums => state.saved_album_list.selected(),
            LibraryFocusState::FollowedArtists => state.followed_artist_list.selected(),
        },
        _ => None,
    };
    let projection = match focus {
        LibraryFocusState::Playlists => &playlists,
        LibraryFocusState::SavedAlbums => &albums,
        LibraryFocusState::FollowedArtists => &artists,
    };
    if let Some(row) = selected_index.and_then(|index| projection.row_at(index)) {
        let status = if row.secondary.is_empty() {
            row.primary.clone()
        } else {
            format!("{} · {}", row.primary, row.secondary)
        };
        let action = format!(
            "{} {}",
            workspace_command_key(crate::command::Command::ChooseSelected),
            match focus {
                LibraryFocusState::Playlists => "Open playlist",
                LibraryFocusState::SavedAlbums => "Open album",
                LibraryFocusState::FollowedArtists => "Open artist",
            }
        );
        let action_width = action.chars().count() as u16;
        let available_width = layout.content.width.saturating_sub(4);
        let status_width = available_width.saturating_sub(action_width.saturating_add(3));
        if status_width > 0 {
            workspace_text(
                frame,
                Rect::new(
                    layout.content.x.saturating_add(2),
                    status_y,
                    status_width,
                    1,
                ),
                utils::bounded_text(&status, status_width as usize),
                ui.theme.workspace_secondary_text(),
            );
        }
        if action_width < available_width {
            frame.render_widget(
                Paragraph::new(action)
                    .style(ui.theme.workspace_secondary_text())
                    .alignment(Alignment::Right),
                Rect::new(
                    layout.content.right().saturating_sub(2 + action_width),
                    status_y,
                    action_width,
                    1,
                ),
            );
        }
    }
}

pub fn render_workspace_context_page(
    is_active: bool,
    frame: &mut Frame,
    state: &SharedState,
    ui: &mut UIStateGuard,
    rect: Rect,
) {
    render_workspace_context_shell(is_active, frame, state, ui, rect, false);
}

pub fn render_workspace_youtube_context_page(
    is_active: bool,
    frame: &mut Frame,
    state: &SharedState,
    ui: &mut UIStateGuard,
    rect: Rect,
) {
    render_workspace_context_shell(is_active, frame, state, ui, rect, true);
}

fn workspace_command_key(command: crate::command::Command) -> String {
    config::get_config()
        .keymap_config
        .key_sequence_for_command(command)
        .map_or_else(|| "—".to_owned(), ToString::to_string)
}

fn render_workspace_queue_sidebar(
    frame: &mut Frame,
    state: &SharedState,
    ui: &mut UIStateGuard,
    rect: Rect,
) {
    if rect.width < 8 || rect.height < 4 {
        return;
    }
    frame.render_widget(Block::default().style(ui.theme.workspace_panel()), rect);

    let queue_top = rect.y.saturating_add(3);
    let queue_bottom = rect.y.saturating_add(13).min(rect.bottom());
    let (authority, item_count, items, playback_capabilities) = {
        let player = state.player.read();
        let count = player.queue_display_item_count();
        let playback_provider = player.effective_playback_provider(ui.active_provider);
        let playback_capabilities = match playback_provider {
            config::ActiveProvider::Spotify => {
                player.playback_capabilities(crate::state::Provider::Spotify)
            }
            config::ActiveProvider::YouTubeMusic => {
                player.playback_capabilities(crate::state::Provider::YouTubeMusic)
            }
        };
        let indices = (0..count).take_while(|index| {
            let y = queue_top.saturating_add((*index as u16).saturating_mul(3));
            y.saturating_add(2) <= queue_bottom
        });
        let data = state.data.read();
        let spotify_labels = SpotifyQueueLabels::new(&player, &data, &ui.spotify_queue_labels);
        (
            player.queue_authority(),
            count,
            indices
                .filter_map(|index| {
                    player
                        .queue_display_item_ref(index)
                        .map(|item| (index, QueueRowProjection::from_item(&item, &spotify_labels)))
                })
                .collect::<Vec<_>>(),
            playback_capabilities,
        )
    };
    if item_count == 0 {
        ui.workspace_queue_list.select(None);
    } else if ui
        .workspace_queue_list
        .selected()
        .is_some_and(|selected| selected >= item_count)
    {
        ui.workspace_queue_list.select(Some(item_count - 1));
    } else if ui.workspace_queue_is_active() && ui.workspace_queue_list.selected().is_none() {
        ui.workspace_queue_list.select(Some(0));
    }

    let label_rect = |offset: u16| {
        Rect::new(
            rect.x.saturating_add(2),
            rect.y.saturating_add(offset),
            rect.width.saturating_sub(4),
            1,
        )
    };
    workspace_text(
        frame,
        label_rect(1),
        "Up next",
        ui.theme.workspace_heading(),
    );

    let queue_status = if item_count != 0 {
        None
    } else {
        Some(match authority {
            QueueAuthority::SpotifyNative => {
                let player = state.player.read();
                if player.queue.is_none() {
                    ("Loading queue…", ui.theme.workspace_status_warning())
                } else {
                    ("Queue is empty", ui.theme.workspace_secondary_text())
                }
            }
            QueueAuthority::LocalUnified => ("Queue is empty", ui.theme.workspace_secondary_text()),
            QueueAuthority::LocalProviderSession | QueueAuthority::Unavailable => {
                ("Queue unavailable", ui.theme.workspace_disabled())
            }
        })
    };
    if let Some((status, style)) = queue_status {
        workspace_text(frame, label_rect(3), status, style);
    }

    let row_width = rect.width.saturating_sub(4);
    let selected = ui.workspace_queue_list.selected();
    let row_origin = Rect::new(
        rect.x.saturating_add(2),
        queue_top,
        row_width,
        queue_bottom.saturating_sub(queue_top),
    );
    for (index, projection) in items {
        let Some(row) = queue_row_hit_rect(row_origin, 0, index, 2, 3) else {
            continue;
        };
        let selected_row = selected == Some(index);
        let active = ui.workspace_queue_is_active() && selected_row;
        let selection_style = if active {
            ui.theme.workspace_selection_active()
        } else if selected_row {
            ui.theme.workspace_selection_inactive()
        } else {
            ui.theme.workspace_panel()
        };
        if selected_row {
            frame.render_widget(Block::default().style(selection_style), row);
        }
        workspace_text(
            frame,
            Rect::new(row.x, row.y, row.width, 1),
            utils::bounded_text(&projection.title, row.width as usize),
            ui.theme
                .workspace_base()
                .patch(ui.theme.workspace_panel())
                .patch(selection_style),
        );
        workspace_text(
            frame,
            Rect::new(row.x, row.y.saturating_add(1), row.width, 1),
            utils::bounded_text(&projection.artists, row.width as usize),
            ui.theme.workspace_secondary_text().patch(selection_style),
        );
        ui.workspace_hits.push((row, WorkspaceHit::QueueRow(index)));
    }

    let divider_y = rect.y.saturating_add(13);
    if divider_y < rect.bottom() {
        workspace_rule(
            frame,
            &ui.theme,
            Rect::new(
                rect.x.saturating_add(2),
                divider_y,
                rect.width.saturating_sub(4),
                1,
            ),
        );
    }

    workspace_text(
        frame,
        label_rect(15),
        "Playback",
        ui.theme.workspace_heading(),
    );
    let (shuffle, repeat, volume) = {
        let player = state.player.read();
        let repeat = player.unified_queue.as_ref().map_or(
            rspotify::model::RepeatState::Off,
            crate::state::UnifiedQueue::repeat,
        );
        let shuffle = player
            .unified_queue
            .as_ref()
            .is_some_and(crate::state::UnifiedQueue::is_shuffled);
        let volume = player
            .current_playback()
            .and_then(|playback| playback.device.volume_percent)
            .or_else(|| {
                player
                    .youtube_playback
                    .as_ref()
                    .map(|playback| u32::from(playback.volume))
            })
            .unwrap_or(0);
        (shuffle, repeat, volume)
    };
    for (offset, label, value, option, support) in [
        (
            17_u16,
            "Shuffle",
            if shuffle { "On" } else { "Off" },
            WorkspacePlaybackOption::Shuffle,
            playback_capabilities.shuffle,
        ),
        (
            19,
            "Repeat",
            match repeat {
                rspotify::model::RepeatState::Off => "Off",
                rspotify::model::RepeatState::Track => "Track",
                rspotify::model::RepeatState::Context => "Context",
            },
            WorkspacePlaybackOption::Repeat,
            playback_capabilities.repeat,
        ),
        (
            21,
            "Volume",
            "",
            WorkspacePlaybackOption::Volume,
            playback_capabilities.volume,
        ),
    ] {
        workspace_text(
            frame,
            Rect::new(
                rect.x.saturating_add(2),
                rect.y.saturating_add(offset),
                7,
                1,
            ),
            label,
            ui.theme.workspace_secondary_text(),
        );
        let value = if label == "Volume" {
            format!("{volume}%")
        } else {
            value.to_owned()
        };
        let value_rect = Rect::new(
            rect.x.saturating_add(15),
            rect.y.saturating_add(offset),
            rect.width.saturating_sub(17),
            1,
        );
        workspace_text(
            frame,
            value_rect,
            value,
            ui.theme.workspace_secondary_text(),
        );
        if value_rect.width > 0 && support == crate::state::PlaybackSupport::Supported {
            ui.workspace_hits
                .push((value_rect, WorkspaceHit::PlaybackOption(option)));
        }
    }

    let actions_y = rect.y.saturating_add(25);
    if actions_y >= rect.bottom() || rect.width < 12 {
        return;
    }
    let actions_rect = Rect::new(
        rect.x.saturating_add(2),
        actions_y,
        rect.width.saturating_sub(4),
        rect.bottom().saturating_sub(actions_y).min(9),
    );
    frame.render_widget(
        Block::default().style(ui.theme.workspace_elevated_surface()),
        actions_rect,
    );
    workspace_text(
        frame,
        Rect::new(
            actions_rect.x.saturating_add(2),
            actions_rect.y.saturating_add(1),
            actions_rect.width.saturating_sub(4),
            1,
        ),
        "Track actions",
        ui.theme.workspace_heading(),
    );
    // The key column fits the longest key and keeps a gap before the label.
    let key_column = WorkspaceAction::ALL
        .iter()
        .map(|action| workspace_command_key(action.command()).chars().count())
        .max()
        .unwrap_or(0)
        + 2;
    for action in WorkspaceAction::ALL {
        let y = actions_rect
            .y
            .saturating_add(3 + (action.index() as u16).saturating_mul(2));
        if y >= actions_rect.bottom().saturating_sub(1) {
            break;
        }
        let row = Rect::new(
            actions_rect.x.saturating_add(2),
            y,
            actions_rect.width.saturating_sub(4),
            1,
        );
        let active = ui.workspace_actions_is_active() && ui.workspace_action == action;
        if active {
            frame.render_widget(
                Block::default().style(ui.theme.workspace_selection_active()),
                row,
            );
        }
        let row_style = if active {
            ui.theme.workspace_selection_active()
        } else {
            ui.theme.workspace_elevated_surface()
        };
        let key = workspace_command_key(action.command());
        workspace_text(
            frame,
            row,
            format!("{key:<key_column$}{}", action.label()),
            ui.theme.workspace_base().patch(row_style),
        );
        ui.workspace_hits.push((row, WorkspaceHit::Action(action)));
    }
}

fn render_workspace_context_shell(
    _is_active: bool,
    frame: &mut Frame,
    state: &SharedState,
    ui: &mut UIStateGuard,
    rect: Rect,
    youtube: bool,
) {
    let layout = ui
        .layout_policy()
        .workspace(rect, WorkspaceLayoutKind::Collection);
    ui.workspace_layout = layout;
    if !layout.show_right
        && matches!(
            ui.workspace_focus,
            WorkspaceFocusState::Queue | WorkspaceFocusState::Actions
        )
    {
        ui.workspace_focus = WorkspaceFocusState::Context;
    }
    if layout.show_navigation {
        let playback_provider = state
            .player
            .read()
            .effective_playback_provider(ui.active_provider);
        render_workspace_navigation(frame, ui, layout.navigation, playback_provider);
        workspace_vertical_rule(
            frame,
            &ui.theme,
            Rect::new(layout.navigation.right(), rect.y, 1, rect.height),
        );
    }
    if layout.show_right {
        workspace_vertical_rule(
            frame,
            &ui.theme,
            Rect::new(layout.content.right(), rect.y, 1, rect.height),
        );
    }
    if layout.content.is_empty() {
        return;
    }
    ui.workspace_hits.retain(|(_, hit)| {
        matches!(
            hit,
            WorkspaceHit::CloseWindow
                | WorkspaceHit::Help
                | WorkspaceHit::PlaybackOption(_)
                | WorkspaceHit::Navigation(_)
                | WorkspaceHit::Scope(_)
        )
    });
    let rendered_v1_collection = if youtube {
        render_workspace_youtube_collection(frame, state, ui, layout.content)
    } else {
        render_workspace_spotify_collection(frame, state, ui, layout.content)
    };
    if !rendered_v1_collection {
        // Only reachable if page state and cached data disagree (e.g. an artist
        // page whose cache entry is not an artist); the next frame resolves it.
        tracing::debug!("Context page state and cached data disagree; skipping one frame");
    }
    if layout.show_right {
        render_workspace_queue_sidebar(frame, state, ui, layout.right);
    }
}

fn workspace_context_scope(ui: &UIStateGuard) -> String {
    let account = match ui.active_provider {
        config::ActiveProvider::Spotify => ui.spotify_account_label.as_deref(),
        config::ActiveProvider::YouTubeMusic => ui.youtube_account_label.as_deref(),
    }
    .unwrap_or("account unavailable");
    format!("{} · {account}", ui.active_provider.title())
}

const COMPACT_HEADING_MIN_TITLE_WIDTH: usize = 12;

fn workspace_context_heading(
    frame: &mut Frame,
    theme: &config::Theme,
    rect: Rect,
    title: &str,
    scope: &str,
    full_profile: bool,
) {
    workspace_table_heading(
        frame,
        theme,
        rect,
        title,
        scope,
        full_profile,
        Some("/ Search"),
    );
}

fn workspace_table_heading(
    frame: &mut Frame,
    theme: &config::Theme,
    rect: Rect,
    title: &str,
    scope: &str,
    full_profile: bool,
    hint: Option<&str>,
) {
    let x = rect.x.saturating_add(2);
    let width = rect.width.saturating_sub(4);
    if full_profile {
        workspace_text(
            frame,
            Rect::new(x, rect.y.saturating_add(1), width, 1),
            title,
            theme.workspace_heading(),
        );
        workspace_text(
            frame,
            Rect::new(
                x,
                rect.y.saturating_add(2),
                if hint.is_some() { width.min(89) } else { width },
                1,
            ),
            scope,
            theme.workspace_secondary_text(),
        );
        if let Some(hint) = hint {
            workspace_text(
                frame,
                Rect::new(
                    rect.right().saturating_sub(17),
                    rect.y.saturating_add(2),
                    17.min(rect.width),
                    1,
                ),
                hint,
                theme.workspace_secondary_text(),
            );
        }
        return;
    }
    // Compact heading (DESIGN-SPEC §9.3): scope and the search hint share the
    // title row and are dropped, widest first, before the title is squeezed.
    let title_width = title.chars().count().min(COMPACT_HEADING_MIN_TITLE_WIDTH);
    let trailing = match hint {
        Some(hint) => [format!("{scope}   {hint}"), hint.to_owned()],
        None => [scope.to_owned(), String::new()],
    }
    .into_iter()
    .find(|text| usize::from(width) >= title_width + 2 + text.chars().count());
    let trailing_width = trailing.as_ref().map_or(0, |text| {
        u16::try_from(text.chars().count()).unwrap_or(u16::MAX)
    });
    let title_rect_width = if trailing.is_some() {
        width.saturating_sub(trailing_width.saturating_add(2))
    } else {
        width
    };
    workspace_text(
        frame,
        Rect::new(x, rect.y, title_rect_width, 1),
        utils::bounded_text(title, usize::from(title_rect_width)),
        theme.workspace_heading(),
    );
    if let Some(trailing) = trailing {
        workspace_text(
            frame,
            Rect::new(
                x.saturating_add(width).saturating_sub(trailing_width),
                rect.y,
                trailing_width,
                1,
            ),
            trailing,
            theme.workspace_secondary_text(),
        );
    }
}

/// Whether opened collections use the full vertical profile. DESIGN-SPEC §9.1
/// keys this on the whole viewport height, not on the collection's own rect.
/// Whether collection tables on this page get the spaced, full-height
/// profile: on tall terminals, unless the visualizer has squeezed the page so
/// far that spaced rows would show only a few tracks.
fn collection_full_profile(frame: &Frame, page: Rect) -> bool {
    frame.area().height >= 32 && page.height >= 18
}

/// Row placement for an opened collection's track table (DESIGN-SPEC §9.3).
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct CollectionTableRows {
    header_y: u16,
    data_y: u16,
    status_y: u16,
    stride: u16,
    visible: u16,
}

/// Compact tables keep their status line only when it leaves this many rows.
const COMPACT_STATUS_MIN_ROWS: u16 = 6;

impl CollectionTableRows {
    fn new(rect: Rect, full_profile: bool) -> Self {
        let (header_y, status_y, stride) = if full_profile {
            (rect.y.saturating_add(5), rect.bottom().saturating_sub(2), 2)
        } else {
            let header_y = rect.y.saturating_add(1);
            // The "N tracks shown" line is the first thing to go: below a few
            // rows of tracks, its row shows one more track instead.
            let rows_with_status = rect.bottom().saturating_sub(1).saturating_sub(header_y + 1);
            let status_y = if rows_with_status < COMPACT_STATUS_MIN_ROWS {
                rect.bottom()
            } else {
                rect.bottom().saturating_sub(1)
            };
            (header_y, status_y, 1)
        };
        let data_y = header_y.saturating_add(stride);
        let visible = if data_y >= status_y {
            0
        } else if full_profile {
            // The full profile keeps a spacer row above the status line.
            ((status_y - data_y).saturating_sub(1) / 2).max(1)
        } else {
            status_y - data_y
        };
        Self {
            header_y,
            data_y,
            status_y,
            stride,
            visible,
        }
    }
}

fn workspace_collection_visible_rows(rect: Rect, full_profile: bool) -> u16 {
    CollectionTableRows::new(rect, full_profile).visible
}

/// Column placement for an opened collection's track table (DESIGN-SPEC §7.2).
/// At the canonical 110-cell context width this is the design-v1 table.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct CollectionTableColumns {
    /// `(x, width)` of each optional column.
    number: Option<(u16, u16)>,
    title_x: u16,
    title_width: u16,
    artist: Option<(u16, u16)>,
    time: Option<(u16, u16)>,
}

impl CollectionTableColumns {
    const TITLE_MIN: u16 = 20;
    const ARTIST_MIN: u16 = 12;
    const ARTIST_MAX: u16 = 30;
    const GAP: u16 = 2;
    /// Focus rail, selection edge, marker and the space after it.
    const LEADING: u16 = 5;
    /// Cells between the last column and the pane edge, scrollbar included.
    const TRAILING: u16 = 4;

    /// `total_rows` sizes the index column past 999; `duration_width` is the
    /// widest visible duration, so hour-long items widen time instead of clipping.
    #[cfg(test)]
    fn new(rect: Rect, total_rows: usize, duration_width: u16) -> Self {
        Self::with_detail_max(rect, total_rows, duration_width, Self::ARTIST_MAX)
    }

    /// Like `new`, with the detail column (artist in §7.2) capped at
    /// `detail_max` cells for narrower data such as dates.
    fn with_detail_max(
        rect: Rect,
        total_rows: usize,
        duration_width: u16,
        detail_max: u16,
    ) -> Self {
        let detail_min = Self::ARTIST_MIN.min(detail_max);
        let number_width = (total_rows.max(1).ilog10() as u16 + 1).max(3);
        let time_width = duration_width.clamp(5, 8);
        // Shrink order: artist narrows to its minimum, then artist, index and
        // time are dropped in that order. The title always keeps what is left.
        for (with_number, with_time) in [(true, true), (false, true), (false, false)] {
            let leading = Self::LEADING
                + if with_number {
                    number_width + Self::GAP
                } else {
                    0
                };
            let trailing = Self::TRAILING + if with_time { time_width + Self::GAP } else { 0 };
            let available = rect.width.saturating_sub(leading + trailing);
            let title_x = rect.x.saturating_add(leading);
            let columns = |title_width, artist| Self {
                number: with_number.then(|| (rect.x.saturating_add(Self::LEADING), number_width)),
                title_x,
                title_width,
                artist,
                time: with_time.then(|| {
                    (
                        rect.right()
                            .saturating_sub(Self::TRAILING)
                            .saturating_sub(time_width),
                        time_width,
                    )
                }),
            };
            if with_number && with_time && available >= Self::TITLE_MIN + Self::GAP + detail_min {
                let artist_width = (available - Self::GAP - Self::TITLE_MIN).min(detail_max);
                let title_width = available - Self::GAP - artist_width;
                let artist_x = title_x + title_width + Self::GAP;
                return columns(title_width, Some((artist_x, artist_width)));
            }
            if available >= Self::TITLE_MIN || !with_time {
                return columns(available, None);
            }
        }
        unreachable!("the final layout always returns")
    }
}

/// The collection table's second column: artist for tracks, other metadata
/// (such as release dates) for other row kinds.
#[derive(Clone, Copy, Debug)]
struct CollectionDetailColumn {
    label: &'static str,
    max_width: u16,
}

impl CollectionDetailColumn {
    const ARTIST: Self = Self {
        label: "Artist",
        max_width: CollectionTableColumns::ARTIST_MAX,
    };
    const RELEASE_DATE: Self = Self {
        label: "Released",
        max_width: 10,
    };
}

#[allow(clippy::too_many_arguments)]
fn render_workspace_collection_table(
    frame: &mut Frame,
    theme: &config::Theme,
    rect: Rect,
    rows: &[CollectionTrackRow],
    row_offset: usize,
    total_rows: usize,
    focused_row: Option<usize>,
    context_focused: bool,
    selected_indices: &[usize],
    playing_index: Option<usize>,
    table_state: &mut TableState,
    status: &str,
    status_style: Style,
    detail: CollectionDetailColumn,
    full_profile: bool,
    focused_overflow: config::FocusedRowOverflow,
    focused_phase: usize,
    hits: &mut Vec<(Rect, WorkspaceHit)>,
) {
    let CollectionTableRows {
        header_y,
        data_y,
        status_y,
        stride,
        visible: visible_rows,
    } = CollectionTableRows::new(rect, full_profile);
    if status_y > rect.y && status_y < rect.bottom() {
        workspace_text(
            frame,
            Rect::new(
                rect.x.saturating_add(2),
                status_y,
                rect.width.saturating_sub(4),
                1,
            ),
            status,
            status_style,
        );
    }
    if visible_rows == 0 {
        return;
    }
    utils::adjust_table_offset(table_state, total_rows, visible_rows);

    let focus_x = rect.x.saturating_add(1);
    let selection_x = rect.x.saturating_add(2);
    let marker_x = rect.x.saturating_add(3);
    let duration_width = rows
        .iter()
        .map(|row| u16::try_from(row.duration.chars().count()).unwrap_or(u16::MAX))
        .max()
        .unwrap_or(0);
    let columns =
        CollectionTableColumns::with_detail_max(rect, total_rows, duration_width, detail.max_width);
    let title_x = columns.title_x;
    let title_width = columns.title_width;
    let scrollbar_x = rect.right().saturating_sub(1);
    if context_focused {
        utils::render_vertical_rule(
            frame,
            Rect::new(focus_x, header_y, 1, status_y.saturating_sub(header_y)),
            "│",
            theme.workspace_focus_indicator(),
        );
    }
    if let Some((number_x, number_width)) = columns.number {
        workspace_text(
            frame,
            Rect::new(number_x, header_y, number_width, 1),
            "#",
            theme.workspace_table_header(),
        );
    }
    workspace_text(
        frame,
        Rect::new(title_x, header_y, title_width, 1),
        "Title",
        theme.workspace_table_header(),
    );
    if let Some((artist_x, artist_width)) = columns.artist {
        workspace_text(
            frame,
            Rect::new(artist_x, header_y, artist_width, 1),
            detail.label,
            theme.workspace_table_header(),
        );
    }
    if let Some((time_x, time_width)) = columns.time {
        workspace_text(
            frame,
            Rect::new(time_x + time_width - 4, header_y, 4, 1),
            "Time",
            theme.workspace_table_header(),
        );
    }

    let offset = table_state.offset();
    for visible_index in 0..usize::from(visible_rows) {
        let source_index = offset.saturating_add(visible_index);
        let Some(row) = rows.get(source_index.saturating_sub(row_offset)) else {
            break;
        };
        let y = data_y.saturating_add((visible_index as u16).saturating_mul(stride));
        if y >= status_y {
            break;
        }
        let selected = focused_row == Some(source_index);
        // Only a focused table scrolls its selected row.
        let scrolls = selected && context_focused;
        let selected_style = if context_focused {
            theme.workspace_selection_active()
        } else {
            theme.workspace_selection_inactive()
        };
        let row_style = selected_style;
        let row_rect = Rect::new(selection_x, y, rect.width.saturating_sub(3), 1);
        if selected {
            frame.render_widget(Block::default().style(row_style), row_rect);
        }
        let playing = playing_index == Some(source_index);
        let text_style = if selected {
            row_style
        } else if playing {
            theme.workspace_current_playing()
        } else {
            theme.workspace_base()
        };
        if playing || selected_indices.binary_search(&source_index).is_ok() {
            let marker = if playing { "▶" } else { "*" };
            let marker_style = if selected {
                theme.workspace_selected_indicator()
            } else if playing {
                theme.workspace_current_playing()
            } else {
                theme.workspace_multiselect()
            };
            workspace_text(frame, Rect::new(marker_x, y, 1, 1), marker, marker_style);
        }
        if let Some((number_x, number_width)) = columns.number {
            workspace_text(
                frame,
                Rect::new(number_x, y, number_width, 1),
                format!(
                    "{:>width$}",
                    source_index.saturating_add(1),
                    width = usize::from(number_width)
                ),
                text_style,
            );
        }
        workspace_text(
            frame,
            Rect::new(title_x, y, title_width, 1),
            utils::focused_row_text(
                &row.title,
                usize::from(title_width),
                scrolls,
                focused_overflow,
                focused_phase,
            ),
            text_style,
        );
        if let Some((artist_x, artist_width)) = columns.artist {
            workspace_text(
                frame,
                Rect::new(artist_x, y, artist_width, 1),
                utils::focused_row_text(
                    &row.artist,
                    usize::from(artist_width),
                    scrolls,
                    focused_overflow,
                    focused_phase,
                ),
                text_style,
            );
        }
        if let Some((time_x, time_width)) = columns.time {
            let width = usize::from(time_width);
            workspace_text(
                frame,
                Rect::new(time_x, y, time_width, 1),
                format!("{:>width$}", utils::bounded_text(&row.duration, width)),
                text_style,
            );
        }
        hits.push((row_rect, WorkspaceHit::ContextRow(source_index)));
    }

    if total_rows == 0 {
        workspace_text(
            frame,
            Rect::new(selection_x, data_y, rect.width.saturating_sub(3), 1),
            "No items",
            theme.workspace_secondary_text(),
        );
    }

    let scroll_y = header_y.saturating_add(1);
    let scroll_height = status_y.saturating_sub(scroll_y);
    if total_rows > usize::from(visible_rows) && scroll_height > 0 {
        let thumb = (usize::from(scroll_height).saturating_mul(usize::from(visible_rows))
            / total_rows)
            .max(1) as u16;
        let max_start = scroll_height.saturating_sub(thumb);
        let max_offset = total_rows.saturating_sub(usize::from(visible_rows));
        let start = usize::from(max_start)
            .saturating_mul(table_state.offset())
            .checked_div(max_offset)
            .unwrap_or(0) as u16;
        utils::render_vertical_rule(
            frame,
            Rect::new(scrollbar_x, scroll_y, 1, scroll_height),
            "│",
            theme.workspace_scrollbar_track(),
        );
        utils::render_vertical_rule(
            frame,
            Rect::new(scrollbar_x, scroll_y.saturating_add(start), 1, thumb),
            "┃",
            theme.workspace_scrollbar_thumb(),
        );
    }
}

fn render_workspace_youtube_collection(
    frame: &mut Frame,
    state: &SharedState,
    ui: &mut UIStateGuard,
    rect: Rect,
) -> bool {
    let filter = context_filter_query(ui);
    let provider_epoch = ui.provider_selection_epoch(config::ActiveProvider::YouTubeMusic);
    let playlist_projection = match ui.current_page_mut() {
        PageState::YouTubeContext {
            id,
            context: Some(context),
            state,
            ..
        } if matches!(id, YouTubeContextId::Playlist(_)) => {
            let Some(snapshot) = PlaylistSnapshot::from_youtube_playlist(
                id,
                context,
                provider_epoch,
                state.status,
                PlaylistCapabilities::youtube_music(false),
                PlaylistActionModel::default(),
                |_| Vec::new(),
            ) else {
                return false;
            };
            let Ok(projection) = MutablePlaylistController::synchronize(
                &mut state.mutable_playlist,
                &snapshot,
                filter.as_deref(),
            ) else {
                return false;
            };
            Some(projection)
        }
        _ => None,
    };
    let (context_id, title, track_count, page_status) = match ui.current_page() {
        PageState::YouTubeContext {
            id,
            context: Some(context),
            state,
            ..
        } if !matches!(state.status, UiViewStatus::Failed { .. }) => {
            let title = if context.title.trim().is_empty() {
                id.title().to_owned()
            } else {
                context.title.clone()
            };
            (
                id.clone(),
                title,
                playlist_projection.as_ref().map_or_else(
                    || ui.search_filtered_item_count(&context.tracks),
                    MutablePlaylistProjection::visible_len,
                ),
                state.status,
            )
        }
        PageState::YouTubeContext { id, state, .. } => {
            // A failed load shows nothing from the cached context, its title
            // included; without rows anything else is still loading.
            let title = id.title().to_owned();
            let status = if matches!(state.status, UiViewStatus::Failed { .. }) {
                state.status
            } else {
                UiViewStatus::Loading
            };
            render_workspace_collection_status(frame, ui, rect, &title, status);
            return true;
        }
        _ => return false,
    };
    let full_profile = collection_full_profile(frame, rect);
    workspace_context_heading(
        frame,
        &ui.theme,
        rect,
        &title,
        &workspace_context_scope(ui),
        full_profile,
    );

    let selected_indices = {
        if matches!(context_id, YouTubeContextId::Playlist(_)) {
            ui.current_page()
                .mutable_playlist_state()
                .map(|state| state.selection().selected_visible_indices().clone())
                .unwrap_or_default()
        } else {
            let provider_epoch = ui.provider_selection_epoch(config::ActiveProvider::YouTubeMusic);
            let result = match ui.current_page_mut() {
                PageState::YouTubeContext {
                    id,
                    context: Some(context),
                    state,
                    ..
                } => Some(crate::state::synchronize_filtered_youtube_context_tracks(
                    &mut state.youtube_context_selection,
                    provider_epoch,
                    id,
                    &context.tracks,
                    filter.as_deref(),
                )),
                _ => None,
            };
            match result {
                Some(Ok(())) => ui
                    .current_page()
                    .youtube_context_track_selection()
                    .map(|selection| selection.selected_visible_indices())
                    .unwrap_or_default(),
                _ => Vec::new(),
            }
        }
    };
    let visible_indices = match ui.current_page() {
        PageState::YouTubeContext {
            context: Some(context),
            ..
        } => playlist_projection.as_ref().map_or_else(
            || {
                let visible = ui.search_filtered_items(&context.tracks);
                context
                    .tracks
                    .iter()
                    .enumerate()
                    .filter_map(|(index, track)| {
                        visible
                            .iter()
                            .any(|candidate| std::ptr::eq(*candidate, track))
                            .then_some(index)
                    })
                    .collect::<Vec<_>>()
            },
            |projection| projection.visible_indices().to_vec(),
        ),
        _ => Vec::new(),
    };
    let focused_row = ui.current_page().selected_index();
    let playing_index = {
        let tracks = match ui.current_page() {
            PageState::YouTubeContext {
                context: Some(context),
                ..
            } => &context.tracks,
            _ => return false,
        };
        state
            .player
            .read()
            .youtube_playback
            .as_ref()
            .and_then(|playback| {
                tracks
                    .iter()
                    .enumerate()
                    .filter(|(index, _)| visible_indices.binary_search(index).is_ok())
                    .position(|(_, track)| youtube_tracks_match(&playback.track, track))
            })
    };
    let context_focused = ui.workspace_focus == WorkspaceFocusState::Context;
    let theme = ui.theme.clone();
    let mut hits = Vec::new();
    let visible_rows = workspace_collection_visible_rows(rect, full_profile);
    let row_offset = {
        let PageState::YouTubeContext { state, id, .. } = ui.current_page_mut() else {
            return false;
        };
        let table_state = if matches!(id, YouTubeContextId::Playlist(_)) {
            state.mutable_playlist.table_mut()
        } else {
            &mut state.track_list
        };
        utils::adjust_table_offset(table_state, track_count, visible_rows);
        table_state.offset()
    };
    let rows = match ui.current_page() {
        PageState::YouTubeContext {
            context: Some(context),
            ..
        } => context
            .tracks
            .iter()
            .enumerate()
            .filter(|(index, _)| visible_indices.binary_search(index).is_ok())
            .map(|(_, track)| track)
            .skip(row_offset)
            .take(usize::from(visible_rows))
            .map(CollectionTrackRow::from_youtube)
            .collect::<Vec<_>>(),
        _ => return false,
    };
    let focused_overflow = ui.presentation.focused_row_overflow;
    let focused_phase = ui.focused_marquee_phase();
    let PageState::YouTubeContext { state, id, .. } = ui.current_page_mut() else {
        return false;
    };
    let table_state = if matches!(id, YouTubeContextId::Playlist(_)) {
        state.mutable_playlist.table_mut()
    } else {
        &mut state.track_list
    };
    render_workspace_collection_table(
        frame,
        &theme,
        rect,
        &rows,
        row_offset,
        track_count,
        focused_row,
        context_focused,
        &selected_indices,
        playing_index,
        table_state,
        &collection_status_text(track_count, page_status),
        collection_status_style(&theme, page_status),
        CollectionDetailColumn::ARTIST,
        full_profile,
        focused_overflow,
        focused_phase,
        &mut hits,
    );
    ui.workspace_hits.extend(hits);
    true
}

/// Status line for an opened collection. Cached rows stay visible while a
/// refresh is loading or only partly succeeded, so the line carries that state.
fn collection_status_text(track_count: usize, status: UiViewStatus) -> String {
    match status {
        UiViewStatus::Loading
        | UiViewStatus::Partial { .. }
        | UiViewStatus::Unsupported { .. }
        | UiViewStatus::Superseded { .. } => {
            format!("{track_count} tracks shown · {}", status.display_message())
        }
        _ => format!("{track_count} tracks shown"),
    }
}

fn collection_status_style(theme: &config::Theme, status: UiViewStatus) -> Style {
    match status {
        UiViewStatus::Loading => theme.workspace_status_busy(),
        UiViewStatus::Partial { .. }
        | UiViewStatus::Unsupported { .. }
        | UiViewStatus::Superseded { .. } => theme.workspace_status_warning(),
        _ => theme.workspace_secondary_text(),
    }
}

/// Title for a Spotify collection whose tracks are not available: the library
/// usually already knows the name, otherwise the page type stands in for it.
fn spotify_collection_title(
    data: &crate::state::AppData,
    context_id: &crate::state::ContextId,
    context_page_type: &crate::state::ContextPageType,
) -> String {
    let library_name = match context_id {
        crate::state::ContextId::Playlist(id) => {
            data.user_data.playlists.iter().find_map(|item| match item {
                crate::state::PlaylistFolderItem::Playlist(playlist) if &playlist.id == id => {
                    Some(playlist.name.clone())
                }
                _ => None,
            })
        }
        crate::state::ContextId::Album(id) => data
            .user_data
            .saved_albums
            .iter()
            .find(|album| &album.id == id)
            .map(|album| album.name.clone()),
        crate::state::ContextId::Artist(id) => data
            .user_data
            .followed_artists
            .iter()
            .find(|artist| &artist.id == id)
            .map(|artist| artist.name.clone()),
        crate::state::ContextId::Tracks(tracks) => Some(tracks.kind.clone()),
        crate::state::ContextId::Show(id) => data
            .user_data
            .saved_shows
            .iter()
            .find(|show| &show.id == id)
            .map(|show| show.name.clone()),
    };
    library_name.unwrap_or_else(|| context_page_type.title())
}

/// An opened collection without rows to show (DESIGN-SPEC §8.1): the heading
/// stays, and the body carries the loading or failure message instead of a table.
fn render_workspace_collection_status(
    frame: &mut Frame,
    ui: &mut UIStateGuard,
    rect: Rect,
    title: &str,
    status: UiViewStatus,
) {
    let theme = &ui.theme;
    let lines = match status {
        UiViewStatus::Loading => vec![Line::styled(
            crate::ui::loading_indicator(),
            theme.workspace_status_busy(),
        )],
        status => {
            let style = if matches!(status, UiViewStatus::Failed { .. }) {
                theme.workspace_status_error()
            } else {
                theme.workspace_status_warning()
            };
            let mut lines = vec![Line::styled(status.display_message(), style)];
            if let Some(next_action) = status.next_action() {
                lines.push(Line::styled(next_action, theme.workspace_secondary_text()));
            }
            lines
        }
    };
    render_workspace_collection_message(frame, ui, rect, title, lines);
}

/// The heading of an opened collection with `lines` in place of its table.
fn render_workspace_collection_message(
    frame: &mut Frame,
    ui: &mut UIStateGuard,
    rect: Rect,
    title: &str,
    lines: Vec<Line<'static>>,
) {
    let full_profile = collection_full_profile(frame, rect);
    workspace_context_heading(
        frame,
        &ui.theme,
        rect,
        title,
        &workspace_context_scope(ui),
        full_profile,
    );
    let rows = CollectionTableRows::new(rect, full_profile);
    let body = Rect::new(
        rect.x.saturating_add(2),
        rows.header_y,
        rect.width.saturating_sub(4),
        rows.status_y.saturating_sub(rows.header_y),
    );
    if body.is_empty() {
        return;
    }
    frame.render_widget(Paragraph::new(lines).wrap(Wrap { trim: true }), body);
}

fn render_workspace_spotify_collection(
    frame: &mut Frame,
    state: &SharedState,
    ui: &mut UIStateGuard,
    rect: Rect,
) -> bool {
    let PageState::Context {
        id,
        state: page_state,
        context_page_type,
    } = ui.current_page()
    else {
        return false;
    };
    // The player-event handler fills `id` and `state` shortly after a page opens.
    let (Some(context_id), Some(page_state)) = (id, page_state) else {
        let context_page_type = context_page_type.clone();
        let pending_id = match &context_page_type {
            crate::state::ContextPageType::Browsing(context_id) => Some(context_id.clone()),
            crate::state::ContextPageType::CurrentPlaying => {
                state.player.read().playing_context_id()
            }
        };
        if let Some(context_id) = pending_id {
            let title =
                spotify_collection_title(&state.data.read(), &context_id, &context_page_type);
            render_workspace_collection_status(frame, ui, rect, &title, UiViewStatus::Loading);
        } else {
            let title = context_page_type.title();
            let lines = vec![
                Line::styled("Nothing is playing.", ui.theme.workspace_secondary_text()),
                Line::styled(
                    "Play a playlist, album, artist or podcast to see it here.",
                    ui.theme.workspace_secondary_text(),
                ),
            ];
            render_workspace_collection_message(frame, ui, rect, &title, lines);
        }
        return true;
    };
    let context_id = context_id.clone();
    if let ContextPageUIState::Failed { status } = page_state {
        let status = *status;
        let title = spotify_collection_title(&state.data.read(), &context_id, context_page_type);
        render_workspace_collection_status(frame, ui, rect, &title, status);
        return true;
    }
    let is_artist = matches!(page_state, ContextPageUIState::Artist { .. });
    let is_show = matches!(page_state, ContextPageUIState::Show { .. });
    if !state
        .data
        .read()
        .caches
        .context
        .contains_key(&context_id.uri())
    {
        let title = spotify_collection_title(&state.data.read(), &context_id, context_page_type);
        render_workspace_collection_status(frame, ui, rect, &title, UiViewStatus::Loading);
        return true;
    }
    if is_artist {
        return artist::render_workspace_spotify_artist(frame, state, ui, rect, &context_id);
    }
    if is_show {
        return show::render_workspace_spotify_show(frame, state, ui, rect, &context_id);
    }
    let pane = match ui.current_page() {
        PageState::Context {
            state: Some(ContextPageUIState::Playlist { .. }),
            ..
        } => ContextTrackPane::Playlist,
        PageState::Context {
            state: Some(ContextPageUIState::Album { .. }),
            ..
        } => ContextTrackPane::Album,
        PageState::Context {
            state: Some(ContextPageUIState::Tracks { .. }),
            ..
        } => ContextTrackPane::Tracks,
        _ => return false,
    };
    let context_focused = ui.workspace_focus == WorkspaceFocusState::Context;
    let theme = ui.theme.clone();
    let mut hits = Vec::new();
    let full_profile = collection_full_profile(frame, rect);
    let visible_rows = workspace_collection_visible_rows(rect, full_profile);
    let (title, selected_indices, playing_index, row_offset, track_count, rows) = {
        let data = state.data.read();
        let Some(context) = data.caches.context.get(&context_id.uri()) else {
            return false;
        };
        let (title, tracks) = match context {
            Context::Playlist { playlist, tracks } => (playlist.name.clone(), tracks.as_slice()),
            Context::Album { album, tracks } => (album.name.clone(), tracks.as_slice()),
            Context::Tracks { tracks, .. } => ("Tracks".to_owned(), tracks.as_slice()),
            _ => return false,
        };
        let filter = context_filter_query(ui);
        let visible = if let Context::Playlist { playlist, .. } = context {
            let snapshot = PlaylistSnapshot::from_spotify_playlist(
                playlist,
                tracks,
                ui.provider_selection_epoch(config::ActiveProvider::Spotify),
                PlaylistCapabilities::spotify(false),
                PlaylistActionModel::default(),
                |_| Vec::new(),
            );
            let Some(playlist_state) = ui.current_page_mut().mutable_playlist_state_mut() else {
                return false;
            };
            let Ok(projection) = MutablePlaylistController::synchronize(
                playlist_state,
                &snapshot,
                filter.as_deref(),
            ) else {
                return false;
            };
            projection
                .visible_indices()
                .iter()
                .map(|index| &tracks[*index])
                .collect::<Vec<_>>()
        } else {
            ui.search_filtered_items(tracks)
        };
        let selected_indices = if pane == ContextTrackPane::Playlist {
            ui.current_page()
                .mutable_playlist_state()
                .map(|state| state.selection().selected_visible_indices().clone())
                .unwrap_or_default()
        } else {
            synchronize_context_render_selection(
                ui,
                pane,
                &context_id.uri(),
                tracks,
                visible.iter().copied(),
            )
            .unwrap_or_default()
        };
        let playing_uri =
            state.player.read().playback.as_ref().and_then(|playback| {
                match playback.item.as_ref() {
                    Some(rspotify::model::PlayableItem::Track(track)) => {
                        track.id.as_ref().map(rspotify::prelude::Id::uri)
                    }
                    _ => None,
                }
            });
        let playing_index =
            playing_uri.and_then(|uri| visible.iter().position(|track| track.id.uri() == uri));
        let track_count = visible.len();
        let row_offset = {
            let Some(table_state) = context_track_table_state_mut(ui.current_page_mut(), pane)
            else {
                return false;
            };
            utils::adjust_table_offset(table_state, track_count, visible_rows);
            table_state.offset()
        };
        let rows = visible
            .iter()
            .copied()
            .skip(row_offset)
            .take(usize::from(visible_rows))
            .map(CollectionTrackRow::from_spotify)
            .collect::<Vec<_>>();
        (
            title,
            selected_indices,
            playing_index,
            row_offset,
            track_count,
            rows,
        )
    };
    let focused_row = ui.current_page().selected_index();
    workspace_context_heading(
        frame,
        &ui.theme,
        rect,
        &title,
        &workspace_context_scope(ui),
        full_profile,
    );
    let focused_overflow = ui.presentation.focused_row_overflow;
    let focused_phase = ui.focused_marquee_phase();
    let table_state = match ui.current_page_mut() {
        PageState::Context {
            state: Some(ContextPageUIState::Playlist { playlist_state }),
            ..
        } => playlist_state.table_mut(),
        PageState::Context {
            state: Some(ContextPageUIState::Album { track_table, .. }),
            ..
        }
        | PageState::Context {
            state: Some(ContextPageUIState::Tracks { track_table, .. }),
            ..
        } => track_table,
        _ => return false,
    };
    render_workspace_collection_table(
        frame,
        &theme,
        rect,
        &rows,
        row_offset,
        track_count,
        focused_row,
        context_focused,
        &selected_indices,
        playing_index,
        table_state,
        &format!("{track_count} tracks shown"),
        theme.workspace_secondary_text(),
        CollectionDetailColumn::ARTIST,
        full_profile,
        focused_overflow,
        focused_phase,
        &mut hits,
    );
    ui.workspace_hits.extend(hits);
    true
}

pub fn render_library_page(
    is_active: bool,
    frame: &mut Frame,
    state: &SharedState,
    ui: &mut UIStateGuard,
    rect: Rect,
) {
    render_workspace_library_page(is_active, frame, state, ui, rect);
}

pub fn render_unified_playlist_page(
    is_active: bool,
    frame: &mut Frame,
    state: &SharedState,
    ui: &mut UIStateGuard,
    rect: Rect,
) {
    let (snapshot, linked_youtube_target, projection_status, listenbrainz_summary) = {
        let data = state.data.read();
        let PageState::UnifiedPlaylist {
            id,
            listenbrainz_sync,
            ..
        } = ui.current_page()
        else {
            return;
        };
        let Some(playlist) = data
            .unified_playlists
            .iter()
            .find(|playlist| &playlist.id == id)
        else {
            let rect = {
                let content = render_workspace_page_layout(frame, state, ui, rect);
                workspace_content_frame(frame, ui, content, "Unified Playlist")
            };
            render_view_status(
                frame,
                &ui.theme,
                UiViewStatus::Failed {
                    code: crate::state::UNIFIED_PLAYLIST_ERROR_CODE,
                    message: crate::state::UNIFIED_PLAYLIST_ERROR_MESSAGE,
                    next_action: crate::state::UNIFIED_PLAYLIST_ERROR_NEXT_ACTION,
                },
                rect,
            );
            return;
        };
        let link = data
            .playlist_links
            .iter()
            .find(|link| link.unified_playlist_id == playlist.id);
        let (account_id, account_epoch) = current_youtube_projection_scope(ui);
        let projection_status =
            unified_playlist_projection_status(playlist, link, &account_id, account_epoch);
        let configs = config::get_config();
        let listenbrainz_summary = crate::state::ListenBrainzSyncSummary::project(
            configs.app_config.listenbrainz.enabled,
            configs.app_config.listenbrainz.read_only_checking,
            configs.listenbrainz_token().is_some(),
            playlist,
            link,
            *listenbrainz_sync,
        );
        let action_model = unified_playlist_action_model_with_listenbrainz(
            link.is_some_and(|link| link.youtube_playlist_id.is_some()),
            true,
        );
        let context_actions = action_model.context_actions().iter().copied();
        (
            PlaylistSnapshot::from_unified(
                playlist,
                PlaylistActionModel::default(),
                context_actions,
            ),
            link.and_then(|link| link.youtube_playlist_id.as_ref())
                .map(|target_id| {
                    data.user_data
                        .youtube_library
                        .playlists
                        .iter()
                        .find(|playlist| &playlist.id == target_id)
                        .map_or_else(|| "linked".to_owned(), |playlist| playlist.name.clone())
                }),
            projection_status,
            listenbrainz_summary,
        )
    };

    let title = unified_playlist_title(
        &snapshot.title,
        linked_youtube_target.as_deref(),
        projection_status,
        rect.width,
    );
    let rect = {
        let content = render_workspace_page_layout(frame, state, ui, rect);
        workspace_content_frame(frame, ui, content, &title)
    };
    let description = listenbrainz_summary.display_text(rect.width);
    render_mutable_playlist_snapshot(is_active, frame, ui, rect, Some(&description), &snapshot);
}

fn mutable_playlist_filter_query(ui: &UIStateGuard) -> Option<String> {
    match ui.popup.as_ref() {
        Some(PopupState::Search { query }) => Some(query.clone()),
        _ => None,
    }
}

fn render_mutable_playlist_snapshot(
    is_active: bool,
    frame: &mut Frame,
    ui: &mut UIStateGuard,
    rect: Rect,
    description: Option<&str>,
    snapshot: &PlaylistSnapshot,
) -> Option<MutablePlaylistProjection> {
    if !matches!(
        snapshot.status,
        UiViewStatus::Ready | UiViewStatus::Empty | UiViewStatus::Partial { .. }
    ) {
        render_view_status(frame, &ui.theme, snapshot.status, rect);
        return None;
    }
    let rect = if matches!(snapshot.status, UiViewStatus::Partial { .. }) {
        let status_height = view_status_height(snapshot.status, rect.width);
        let chunks =
            Layout::vertical([Constraint::Length(status_height), Constraint::Min(1)]).split(rect);
        render_view_status(frame, &ui.theme, snapshot.status, chunks[0]);
        chunks[1]
    } else {
        rect
    };
    let rect = match description {
        Some(description) if !description.trim().is_empty() => {
            let height = wrapped_description_height_with_limit(description, rect.width, 5);
            render_wrapped_description_with_height(
                frame,
                description,
                ui.theme.workspace_secondary_text(),
                rect,
                height,
            )
        }
        _ => rect,
    };
    let filter = mutable_playlist_filter_query(ui);
    let projection = {
        let playlist_state = ui.current_page_mut().mutable_playlist_state_mut()?;
        match MutablePlaylistController::synchronize(playlist_state, snapshot, filter.as_deref()) {
            Ok(projection) => projection,
            Err(error) => {
                tracing::warn!(
                    selection_error = ?error,
                    "Mutable playlist projection could not be synchronized"
                );
                playlist_state.selection_mut().clear();
                return None;
            }
        }
    };
    if projection.visible_len() == 0 {
        render_view_status(frame, &ui.theme, UiViewStatus::Empty, rect);
        return Some(projection);
    }

    let focused_row = is_active
        .then(|| ui.current_page().selected_index())
        .flatten();
    let focused_overflow = ui.presentation.focused_row_overflow;
    let focused_phase = ui.focused_marquee_phase();
    let relative_index = config::get_config()
        .app_config
        .enable_relative_line_number
        .then_some(focused_row)
        .flatten();
    let compact = ui.layout_policy().mode.is_compact();
    let show_compact_artists = ui.presentation.compact_metadata.shows_artists();
    let rows = projection
        .visible_indices()
        .iter()
        .enumerate()
        .map(|(visible_index, source_index)| {
            let item = &snapshot.entries[*source_index].item;
            let title = unified_playlist_display_title(
                item.title.as_str(),
                item.media_id.raw_id.as_str(),
                item.provider_url.as_deref(),
            );
            let duration = item
                .duration_ms
                .map(|millis| format!("{}:{:02}", millis / 60_000, (millis / 1_000) % 60))
                .unwrap_or_default();
            let source = unified_playlist_source_label(item);
            Row::new(vec![
                Cell::from(
                    if selected_marker(projection.selected_visible_indices(), visible_index) {
                        ">"
                    } else {
                        ""
                    },
                ),
                Cell::from(utils::relative_table_line_number(
                    visible_index,
                    relative_index,
                )),
                Cell::from(utils::focused_table_text(
                    to_bidi_string(title),
                    focused_row == Some(visible_index),
                    focused_overflow,
                    focused_phase,
                )),
                Cell::from(utils::focused_table_text(
                    to_bidi_string(&item.artists),
                    focused_row == Some(visible_index),
                    focused_overflow,
                    focused_phase,
                )),
                Cell::from(utils::focused_table_text(
                    duration,
                    focused_row == Some(visible_index),
                    focused_overflow,
                    focused_phase,
                )),
                Cell::from(utils::focused_table_text(
                    source.to_owned(),
                    focused_row == Some(visible_index),
                    focused_overflow,
                    focused_phase,
                )),
            ])
        })
        .collect::<Vec<_>>();
    let table = Table::new(
        rows,
        if compact {
            [
                Constraint::Length(1),
                Constraint::Length(4),
                Constraint::Fill(5),
                if show_compact_artists {
                    Constraint::Fill(3)
                } else {
                    Constraint::Length(0)
                },
                Constraint::Length(0),
                Constraint::Fill(2),
            ]
        } else {
            [
                Constraint::Length(1),
                Constraint::Length(4),
                Constraint::Fill(4),
                Constraint::Fill(3),
                Constraint::Length(8),
                Constraint::Fill(2),
            ]
        },
    )
    .style(ui.theme.workspace_base())
    .header(
        Row::new(if compact {
            vec![
                "",
                "#",
                "Title",
                if show_compact_artists { "Artists" } else { "" },
                "",
                "Source",
            ]
        } else {
            vec!["", "#", "Title", "Artists", "Duration", "Source"]
        })
        .style(ui.theme.workspace_table_header()),
    )
    .column_spacing(2)
    .row_highlight_style(workspace_row_selection_style(ui, is_active));
    let start = {
        let table_state = ui
            .current_page_mut()
            .mutable_playlist_state_mut()?
            .table_mut();
        utils::render_table_window(frame, table, rect, projection.visible_len(), table_state);
        table_state.offset()
    };
    record_workspace_table_hits_with(
        ui,
        rect,
        start,
        projection.visible_len(),
        WorkspaceHit::UnifiedPlaylistRow,
    );
    Some(projection)
}

fn unified_playlist_display_title<'a>(
    title: &'a str,
    raw_id: &str,
    provider_url: Option<&str>,
) -> &'a str {
    if title.trim().is_empty() || title == raw_id || provider_url == Some(title) {
        "Unknown track"
    } else {
        title
    }
}

fn unified_playlist_source_label(item: &crate::state::PlaylistSeedItem) -> &'static str {
    if item.metadata_degraded || item.metadata_pending {
        return "Unresolved";
    }
    match item.media_id.provider {
        crate::state::Provider::Spotify => "Spotify",
        crate::state::Provider::YouTubeMusic => "YouTube Music",
    }
}

fn unified_playlist_title(
    name: &str,
    linked_youtube_target: Option<&str>,
    projection_status: Option<crate::state::PlaylistProjectionStatus>,
    width: u16,
) -> String {
    let title = if let Some(target) = linked_youtube_target {
        let state = projection_status
            .and_then(unified_projection_status_label)
            .map(|label| format!(" {label}"))
            .unwrap_or_default();
        format!(
            "{} [YT{}: {}]",
            to_bidi_string(name),
            state,
            to_bidi_string(target)
        )
    } else {
        to_bidi_string(name)
    };
    utils::bounded_text(&title, width.saturating_sub(4) as usize)
}

fn unified_projection_status_label(
    status: crate::state::PlaylistProjectionStatus,
) -> Option<&'static str> {
    match status {
        crate::state::PlaylistProjectionStatus::Clean => None,
        crate::state::PlaylistProjectionStatus::Pending => Some("pending"),
        crate::state::PlaylistProjectionStatus::Drifted => Some("drifted"),
        crate::state::PlaylistProjectionStatus::Conflict => Some("conflict"),
        crate::state::PlaylistProjectionStatus::Partial => Some("partial"),
        crate::state::PlaylistProjectionStatus::OutcomeUnknown => Some("unknown"),
        crate::state::PlaylistProjectionStatus::Detached => Some("detached"),
    }
}

fn unified_playlist_projection_status(
    playlist: &crate::state::UnifiedPlaylist,
    link: Option<&crate::state::PlaylistLink>,
    account_id: &str,
    account_epoch: u64,
) -> Option<crate::state::PlaylistProjectionStatus> {
    let link = link?;
    link.youtube_playlist_id.as_ref()?;
    link.projections
        .iter()
        .find(|projection| {
            projection.target.provider == crate::state::Provider::YouTubeMusic
                && projection.target.account_id == account_id
                && projection.target.account_epoch == account_epoch
                && Some(projection.target.playlist_id.as_str())
                    == link.youtube_playlist_id.as_deref()
        })
        .map(|projection| {
            if projection.is_clean_for(&playlist.snapshot_hash()) {
                crate::state::PlaylistProjectionStatus::Clean
            } else if projection.status == crate::state::PlaylistProjectionStatus::Clean {
                crate::state::PlaylistProjectionStatus::Drifted
            } else {
                projection.status
            }
        })
        .or(Some(crate::state::PlaylistProjectionStatus::OutcomeUnknown))
}

fn current_youtube_projection_scope(ui: &crate::state::UIState) -> (String, u64) {
    let account_id = ui
        .youtube_account_id
        .clone()
        .unwrap_or_else(|| "unknown".to_owned());
    (
        account_id,
        ui.provider_selection_epoch(config::ActiveProvider::YouTubeMusic),
    )
}

#[cfg(test)]
mod projection_scope_tests {
    use super::current_youtube_projection_scope;
    use crate::state::UIState;

    #[test]
    fn youtube_projection_scope_uses_cached_account_identity() {
        let mut ui = UIState::default();
        ui.youtube_account_id = Some("account-id".to_owned());

        assert_eq!(
            current_youtube_projection_scope(&ui),
            ("account-id".to_owned(), 0)
        );

        ui.youtube_account_id = None;
        assert_eq!(
            current_youtube_projection_scope(&ui),
            ("unknown".to_owned(), 0)
        );
    }
}

#[allow(dead_code)]
fn unified_playlist_sync_pending(
    playlist: &crate::state::UnifiedPlaylist,
    link: Option<&crate::state::PlaylistLink>,
) -> bool {
    unified_playlist_sync_pending_for_scope(playlist, link, "unknown", 0)
}

#[allow(dead_code)]
fn unified_playlist_sync_pending_for_scope(
    playlist: &crate::state::UnifiedPlaylist,
    link: Option<&crate::state::PlaylistLink>,
    account_id: &str,
    account_epoch: u64,
) -> bool {
    unified_playlist_projection_status(playlist, link, account_id, account_epoch)
        .is_some_and(|status| !status.is_clean())
}

fn unified_playlist_library_label(
    playlist: &crate::state::UnifiedPlaylist,
    link: Option<&crate::state::PlaylistLink>,
    account_id: &str,
    account_epoch: u64,
) -> String {
    let prefix = if unified_playlist_projection_status(playlist, link, account_id, account_epoch)
        .is_some_and(|status| !status.is_clean())
    {
        "[Unified pending]"
    } else {
        "[Unified]"
    };
    format!("{prefix} {}", to_bidi_string(&playlist.name))
}

#[cfg(test)]
fn unified_playlist_row_label(value: &str) -> String {
    to_bidi_string(value)
}

pub fn render_browse_page(
    is_active: bool,
    frame: &mut Frame,
    state: &SharedState,
    ui: &mut UIStateGuard,
    mut rect: Rect,
) {
    let unavailable = UiViewStatus::Unsupported {
        code: "SPOTIFY_BROWSE_UNAVAILABLE",
        message: "Spotify category browsing is unavailable.",
        next_action: "Use Search, Home, or your Library to find music.",
    };
    let title = match ui.current_page() {
        PageState::Browse {
            state: BrowsePageUIState::CategoryPlaylistList { category, .. },
        } => format!("Browse · {} Playlists", to_bidi_string(&category.name)),
        _ => "Browse · Categories".to_owned(),
    };
    let content = render_workspace_page_layout(frame, state, ui, rect);
    rect = workspace_content_frame(frame, ui, content, &title);

    let (categories_loaded, categories, category_playlists) = {
        let data = state.data.read();
        let category_playlists = match ui.current_page() {
            PageState::Browse {
                state: BrowsePageUIState::CategoryPlaylistList { category, .. },
            } => data.browse.category_playlists.get(&category.id).cloned(),
            _ => None,
        };
        (
            data.browse.categories_loaded,
            data.browse.categories.clone(),
            category_playlists,
        )
    };

    // 2+3. Construct the page's layout and widgets
    let selected_index = if is_active {
        ui.current_page().selected_index()
    } else {
        None
    };
    let (list, len) = match ui.current_page() {
        PageState::Browse { state: ui_state } => match ui_state {
            BrowsePageUIState::CategoryList { .. } => {
                if !categories_loaded || categories.is_empty() {
                    render_view_status(frame, &ui.theme, unavailable, rect);
                    return;
                }

                let visible_categories = ui.search_filtered_items_projection(&categories);
                if visible_categories.is_empty() {
                    render_view_status(frame, &ui.theme, UiViewStatus::Empty, rect);
                    return;
                }

                utils::construct_list_widget_with_width(
                    &ui.theme,
                    visible_categories
                        .iter()
                        .map(|c| (to_bidi_string(&c.name), false))
                        .collect(),
                    is_active,
                    selected_index,
                    Some(rect.width as usize),
                    ui.presentation.focused_row_overflow,
                    ui.focused_marquee_phase(),
                )
            }
            BrowsePageUIState::CategoryPlaylistList { .. } => {
                let Some(playlists) = category_playlists.as_deref() else {
                    render_view_status(frame, &ui.theme, unavailable, rect);
                    return;
                };
                if playlists.is_empty() {
                    render_view_status(frame, &ui.theme, unavailable, rect);
                    return;
                }
                let visible_playlists = ui.search_filtered_items_projection(playlists);
                if visible_playlists.is_empty() {
                    render_view_status(frame, &ui.theme, UiViewStatus::Empty, rect);
                    return;
                }

                utils::construct_list_widget_with_width(
                    &ui.theme,
                    visible_playlists
                        .iter()
                        .map(|c| (c.to_bidi_string(), false))
                        .collect(),
                    is_active,
                    selected_index,
                    Some(rect.width as usize),
                    ui.presentation.focused_row_overflow,
                    ui.focused_marquee_phase(),
                )
            }
        },
        _ => return,
    };

    // 4. Render the page's widget
    let Some(MutableWindowState::List(list_state)) = ui.current_page_mut().focus_window_state_mut()
    else {
        return;
    };
    utils::render_list_window(frame, list, rect, len, list_state);
    let start = list_state.offset();
    record_workspace_list_hits_with(ui, rect, start, len, WorkspaceHit::BrowseRow);
}

fn lyrics_playback_is_playing(state: &SharedState, provider: config::ActiveProvider) -> bool {
    let player = state.player.read();
    match provider {
        config::ActiveProvider::Spotify => player
            .playback
            .as_ref()
            .is_some_and(|playback| playback.is_playing),
        config::ActiveProvider::YouTubeMusic => player
            .youtube_playback
            .as_ref()
            .is_some_and(|playback| playback.is_playing),
    }
}

fn lyrics_playback_progress(
    state: &SharedState,
    provider: config::ActiveProvider,
) -> Option<chrono::Duration> {
    match provider {
        config::ActiveProvider::Spotify => state.player.read().playback_progress(),
        config::ActiveProvider::YouTubeMusic => state
            .player
            .read()
            .youtube_playback
            .as_ref()
            .and_then(|playback| chrono::Duration::from_std(playback.progress).ok()),
    }
}

pub fn render_lyrics_page(
    _is_active: bool,
    frame: &mut Frame,
    state: &SharedState,
    ui: &mut UIStateGuard,
    rect: Rect,
) {
    let theme = ui.theme.clone();

    let (
        provider,
        track_uri,
        track,
        artists,
        lyrics_provider,
        mut scroll_offset,
        follow_playback,
        status,
    ) = match ui.current_page() {
        PageState::Lyrics {
            provider,
            track_uri,
            track,
            artists,
            lyrics_provider,
            scroll_offset,
            follow_playback,
            status,
            ..
        } => (
            *provider,
            track_uri.clone(),
            track.clone(),
            artists.clone(),
            lyrics_provider.clone(),
            *scroll_offset,
            *follow_playback,
            *status,
        ),
        _ => return,
    };

    let cache_key = crate::state::LyricsCacheKey::new(&track_uri, lyrics_provider.as_deref());
    let source = {
        let data = state.data.read();
        data.caches
            .lyrics
            .get(&cache_key)
            .and_then(|lyrics| lyrics.as_ref())
            .map(|lyrics| lyrics.source.clone())
    };

    // 2. Construct the page's layout. Track identity leads the page while
    // source/follow state remains quiet metadata below the title.
    let title = lyrics_page_title(&track, &artists);
    let rect = {
        let content = render_workspace_page_layout(frame, state, ui, rect);
        let body = workspace_content_frame(frame, ui, content, &title);
        // Line the metadata and lyrics up with the title above them.
        Rect::new(
            body.x.saturating_add(1),
            body.y,
            body.width.saturating_sub(1),
            body.height,
        )
    };

    let Some(source) = source else {
        render_view_status(frame, &theme, status, rect);
        return;
    };

    // 4. Render the page's widgets
    // render lyric page description text
    let metadata = lyrics_metadata_line(&source, follow_playback);
    // Keep the metadata readable on narrow terminals. The shared layout
    // helper allocates wrapped rows instead of clipping the source or artist.
    // Short pages give its rows to the lyrics instead.
    let lyrics_rect = if rect.height < LYRICS_METADATA_MIN_HEIGHT {
        rect
    } else {
        render_wrapped_description(frame, &metadata, theme.workspace_secondary_text(), rect)
    };

    let progress = lyrics_playback_progress(state, provider);
    if progress.is_some() && lyrics_playback_is_playing(state, provider) {
        // Synced lines change between progress seconds.
        crate::ui::frame_schedule::request_frame_within(std::time::Duration::from_millis(250));
    }
    let projection = {
        let data = state.data.read();
        data.caches
            .lyrics
            .get(&cache_key)
            .and_then(|lyrics| lyrics.as_ref())
            .map(|lyrics| {
                project_visible_lyrics(
                    lyrics,
                    progress,
                    follow_playback,
                    scroll_offset,
                    lyrics_rect.height as usize,
                    &theme,
                )
            })
    };
    let Some(projection) = projection else {
        render_view_status(frame, &theme, status, lyrics_rect);
        return;
    };
    let Ok(projection) = projection else {
        render_view_status(
            frame,
            &theme,
            UiViewStatus::Unsupported {
                code: crate::state::LYRICS_PLAYBACK_UNAVAILABLE_CODE,
                message: crate::state::LYRICS_PLAYBACK_UNAVAILABLE_MESSAGE,
                next_action: crate::state::LYRICS_PLAYBACK_UNAVAILABLE_NEXT_ACTION,
            },
            lyrics_rect,
        );
        return;
    };
    scroll_offset = projection.scroll_offset;
    if let PageState::Lyrics {
        scroll_offset: page_offset,
        ..
    } = ui.current_page_mut()
    {
        *page_offset = scroll_offset;
    }
    // Wide pages centre the lyrics; a left-aligned column would leave most of
    // the page empty.
    let alignment = if lyrics_rect.width >= LYRICS_CENTRED_MIN_WIDTH {
        ratatui::layout::Alignment::Center
    } else {
        ratatui::layout::Alignment::Left
    };
    frame.render_widget(
        Paragraph::new(projection.lines).alignment(alignment),
        lyrics_rect,
    );
}

const LYRICS_CENTRED_MIN_WIDTH: u16 = 90;
/// Lyrics bodies shorter than this skip the source line.
const LYRICS_METADATA_MIN_HEIGHT: u16 = 8;

struct VisibleLyricsProjection {
    lines: Vec<Line<'static>>,
    scroll_offset: usize,
}

fn project_visible_lyrics(
    lyrics: &crate::state::Lyrics,
    progress: Option<chrono::Duration>,
    follow_playback: bool,
    scroll_offset: usize,
    viewport_height: usize,
    theme: &config::Theme,
) -> Result<VisibleLyricsProjection, ()> {
    let (line_count, last_played_line_id) = match &lyrics.lines {
        crate::state::LyricsLines::Plain(lines) => (lines.len(), None),
        crate::state::LyricsLines::Synced(lines) => {
            let progress = progress.ok_or(())?;
            let last_played_line_id = lines
                .iter()
                .enumerate()
                .filter(|(_, (timestamp, _))| *timestamp <= progress)
                .map(|(id, _)| id + 1)
                .next_back()
                .unwrap_or(0);
            (lines.len(), Some(last_played_line_id))
        }
        crate::state::LyricsLines::Rich(lines) => {
            let progress = progress.ok_or(())?;
            let last_played_line_id = lines
                .iter()
                .enumerate()
                .filter(|(_, line)| line.start <= progress)
                .map(|(id, _)| id + 1)
                .next_back()
                .unwrap_or(0);
            (lines.len(), Some(last_played_line_id))
        }
    };
    let mut scroll_offset = scroll_offset.min(line_count.saturating_sub(1));
    if follow_playback {
        if let Some(last_played_line_id) = last_played_line_id {
            if let Some(offset) = last_played_line_id.checked_sub(viewport_height / 2) {
                scroll_offset = offset.min(line_count.saturating_sub(1));
            }
        }
    }
    let end = scroll_offset
        .saturating_add(viewport_height)
        .min(line_count);
    let lines = match &lyrics.lines {
        crate::state::LyricsLines::Plain(lines) => lines[scroll_offset..end]
            .iter()
            .map(|line| Line::raw(lyrics_line_text(line)))
            .collect(),
        crate::state::LyricsLines::Synced(lines) => {
            let last_played_line_id = last_played_line_id.unwrap_or_default();
            lines[scroll_offset..end]
                .iter()
                .enumerate()
                .map(|(visible_id, (_, line))| {
                    let line_id = scroll_offset + visible_id + 1;
                    match line_id.cmp(&last_played_line_id) {
                        std::cmp::Ordering::Less => {
                            Line::styled(lyrics_line_text(line), theme.lyrics_played())
                        }
                        std::cmp::Ordering::Equal => {
                            Line::styled(lyrics_line_text(line), theme.lyrics_playing())
                        }
                        std::cmp::Ordering::Greater => Line::raw(lyrics_line_text(line)),
                    }
                })
                .collect()
        }
        crate::state::LyricsLines::Rich(lines) => {
            let last_played_line_id = last_played_line_id.unwrap_or_default();
            let progress = progress.expect("rich lyrics require progress");
            lines[scroll_offset..end]
                .iter()
                .enumerate()
                .map(|(visible_id, line)| {
                    let line_id = scroll_offset + visible_id + 1;
                    let style = match line_id.cmp(&last_played_line_id) {
                        std::cmp::Ordering::Less => theme.lyrics_played(),
                        std::cmp::Ordering::Equal => theme.lyrics_playing(),
                        std::cmp::Ordering::Greater => Style::default(),
                    };
                    if line_id == last_played_line_id && !line.parts.is_empty() {
                        let spans = line
                            .parts
                            .iter()
                            .map(|part| {
                                if part.end.is_some_and(|end| progress >= end) {
                                    Span::styled(part.text.clone(), theme.lyrics_played())
                                } else if progress >= part.start {
                                    Span::styled(part.text.clone(), theme.lyrics_playing())
                                } else {
                                    Span::raw(part.text.clone())
                                }
                            })
                            .collect::<Vec<_>>();
                        Line::from(spans)
                    } else {
                        Line::styled(line.text.clone(), style)
                    }
                })
                .collect()
        }
    };
    Ok(VisibleLyricsProjection {
        lines,
        scroll_offset,
    })
}

pub fn render_commands_help_page(frame: &mut Frame, ui: &mut UIStateGuard, rect: Rect) {
    let mut scroll_offset = match ui.current_page() {
        PageState::CommandHelp { scroll_offset } => *scroll_offset,
        _ => return,
    };
    render_commands_help_content(frame, ui, rect, &mut scroll_offset);
    if let PageState::CommandHelp {
        scroll_offset: page_offset,
    } = ui.current_page_mut()
    {
        *page_offset = scroll_offset;
    }
}

pub(crate) fn render_commands_help_content(
    frame: &mut Frame,
    ui: &mut UIStateGuard,
    rect: Rect,
    scroll_offset: &mut usize,
) {
    let configs = config::get_config();
    let bindings = configs.keymap_config.resolved_bindings();
    let rows = project_command_help_rows(ui.search_filtered_items_projection(&bindings).iter());
    let n_bindings = rows.len();
    let has_bindings = !rows.is_empty();

    if has_bindings && *scroll_offset >= rows.len() {
        *scroll_offset = rows.len() - 1;
    }
    let selected_index = *scroll_offset;
    let focused_overflow = ui.presentation.focused_row_overflow;
    let focused_phase = ui.focused_marquee_phase();

    // Help is an elevated surface in both its page and popup presentation.
    // The component owns the complete rectangle before rows are drawn.
    let rect = super::components::popup_surface::render("", &ui.theme, Borders::ALL, frame, rect);

    let padded = Rect::new(
        rect.x.saturating_add(1),
        rect.y,
        rect.width.saturating_sub(2),
        rect.height,
    );
    let sections = Layout::vertical([Constraint::Length(2), Constraint::Min(1)]).split(padded);
    frame.render_widget(
        Paragraph::new("Commands").style(ui.theme.workspace_heading()),
        sections[0],
    );
    let rect = sections[1];

    if !has_bindings {
        render_view_status(frame, &ui.theme, UiViewStatus::Empty, rect);
        return;
    }

    if ui.layout_policy().mode.is_compact() {
        let width = rect.width.saturating_sub(2) as usize;
        let items = rows
            .iter()
            .enumerate()
            .map(|(index, row)| {
                let overflow_mode = if index == selected_index {
                    focused_overflow
                } else {
                    config::FocusedRowOverflow::Truncate
                };
                let phase = if index == selected_index {
                    focused_phase
                } else {
                    0
                };
                ListItem::new(utils::focused_overflow_text(
                    &row.compact_line(),
                    width,
                    overflow_mode,
                    phase,
                ))
                .style(if index % 2 == 0 {
                    ui.theme.workspace_panel()
                } else {
                    ui.theme.workspace_base()
                })
            })
            .collect::<Vec<_>>();
        ui.command_help_list_state.select(Some(selected_index));
        let list = List::new(items)
            .highlight_style(ui.theme.workspace_navigation_active())
            .highlight_symbol("> ");
        let (list_offset, selected_index) = {
            let list_state = &mut ui.command_help_list_state;
            utils::render_list_window(frame, list, rect, n_bindings, list_state);
            (list_state.offset(), list_state.selected())
        };
        record_visible_hits(
            &mut ui.workspace_popup_hits,
            rect,
            list_offset,
            n_bindings,
            0,
            |index| index,
        );
        *scroll_offset = selected_index.unwrap_or_default();
        return;
    }

    let key_width = rows
        .iter()
        .map(|row| row.shortcut.chars().count())
        .max()
        .unwrap_or(4)
        .clamp(4, 14) as u16;
    let show_description = rect.width >= 70;
    let constraints = [
        Constraint::Length(key_width),
        if show_description {
            Constraint::Length(24)
        } else {
            Constraint::Fill(1)
        },
        if show_description {
            Constraint::Fill(1)
        } else {
            Constraint::Length(0)
        },
    ];
    let help_table = Table::new(
        rows.iter()
            .enumerate()
            .map(|(index, row)| {
                let focused = index == selected_index;
                Row::new(vec![
                    Cell::from(utils::focused_table_text(
                        row.shortcut.clone(),
                        focused,
                        focused_overflow,
                        focused_phase,
                    ))
                    .style(ui.theme.workspace_hint_key()),
                    Cell::from(utils::focused_table_text(
                        row.binding.clone(),
                        focused,
                        focused_overflow,
                        focused_phase,
                    )),
                    Cell::from(utils::focused_table_text(
                        row.description.clone(),
                        focused,
                        focused_overflow,
                        focused_phase,
                    )),
                ])
                .style(ui.theme.workspace_elevated_surface())
            })
            .collect::<Vec<_>>(),
        constraints,
    )
    .column_spacing(2)
    .header(
        Row::new(vec![
            "Keys",
            "Command",
            if show_description { "Description" } else { "" },
        ])
        .style(ui.theme.workspace_table_header()),
    );

    ui.command_help_table_state.select(Some(selected_index));
    let help_table = help_table.row_highlight_style(ui.theme.workspace_selection_active());
    let (table_offset, selected_index) = {
        let table_state = &mut ui.command_help_table_state;
        utils::render_table_window(frame, help_table, rect, n_bindings, table_state);
        (table_state.offset(), table_state.selected())
    };
    record_visible_hits(
        &mut ui.workspace_popup_hits,
        rect,
        table_offset,
        n_bindings,
        1,
        |index| index,
    );
    *scroll_offset = selected_index.unwrap_or_default();
}

fn render_workspace_page_layout(
    frame: &mut Frame,
    state: &SharedState,
    ui: &mut UIStateGuard,
    rect: Rect,
) -> Rect {
    let layout = ui
        .layout_policy()
        .workspace(rect, WorkspaceLayoutKind::Library);
    ui.workspace_layout = layout;
    if layout.show_navigation {
        let playback_provider = state
            .player
            .read()
            .effective_playback_provider(ui.active_provider);
        render_workspace_navigation(frame, ui, layout.navigation, playback_provider);
        workspace_vertical_rule(
            frame,
            &ui.theme,
            Rect::new(layout.navigation.right(), rect.y, 1, rect.height),
        );
    }
    layout.content
}

/// Pages shorter than this lose the content frame's padding rows, so the
/// title sits on the first row and the body reaches the bottom.
const TIGHT_CONTENT_FRAME_BELOW_HEIGHT: u16 = 10;

fn workspace_content_frame(frame: &mut Frame, ui: &UIStateGuard, rect: Rect, title: &str) -> Rect {
    frame.render_widget(Block::default().style(ui.theme.workspace_base()), rect);
    let tight = rect.height < TIGHT_CONTENT_FRAME_BELOW_HEIGHT;
    workspace_text(
        frame,
        Rect::new(
            rect.x.saturating_add(2),
            rect.y.saturating_add(u16::from(!tight)),
            rect.width.saturating_sub(4),
            1,
        ),
        title,
        ui.theme.workspace_heading(),
    );
    if tight {
        return Rect::new(
            rect.x.saturating_add(1),
            rect.y.saturating_add(1),
            rect.width.saturating_sub(2),
            rect.height.saturating_sub(1),
        );
    }
    Rect::new(
        rect.x.saturating_add(1),
        rect.y.saturating_add(2),
        rect.width.saturating_sub(2),
        rect.height.saturating_sub(3),
    )
}

fn record_workspace_table_hits(ui: &mut UIStateGuard, rect: Rect, start: usize, item_count: usize) {
    record_workspace_table_hits_with(ui, rect, start, item_count, WorkspaceHit::JournalRow);
}

fn record_workspace_table_hits_with(
    ui: &mut UIStateGuard,
    rect: Rect,
    start: usize,
    item_count: usize,
    hit: impl Fn(usize) -> WorkspaceHit,
) {
    record_workspace_hits_with(ui, rect, start, item_count, 1, hit);
}

fn record_workspace_list_hits(ui: &mut UIStateGuard, rect: Rect, start: usize, item_count: usize) {
    record_workspace_list_hits_with(ui, rect, start, item_count, WorkspaceHit::JournalRow);
}

fn record_workspace_list_hits_with(
    ui: &mut UIStateGuard,
    rect: Rect,
    start: usize,
    item_count: usize,
    hit: impl Fn(usize) -> WorkspaceHit,
) {
    record_workspace_hits_with(ui, rect, start, item_count, 0, hit);
}

fn record_workspace_hits_with(
    ui: &mut UIStateGuard,
    rect: Rect,
    start: usize,
    item_count: usize,
    header_rows: u16,
    hit: impl Fn(usize) -> WorkspaceHit,
) {
    record_visible_hits(
        &mut ui.workspace_hits,
        rect,
        start,
        item_count,
        header_rows,
        hit,
    );
}

pub fn render_journal_page(
    is_active: bool,
    frame: &mut Frame,
    state: &SharedState,
    ui: &mut UIStateGuard,
    rect: Rect,
) {
    let rect = {
        let content = render_workspace_page_layout(frame, state, ui, rect);
        workspace_content_frame(frame, ui, content, "Journal")
    };
    let entries = {
        let data = state.data.read();
        data.journal.entries_sorted()
    };
    let filtered_entries = ui.search_filtered_items_projection(&entries);
    let n_entries = filtered_entries.len();
    let selected_indices =
        synchronize_journal_render_selection(ui, &entries, filtered_entries.iter());

    if filtered_entries.is_empty() {
        render_view_status(frame, &ui.theme, UiViewStatus::Empty, rect);
        return;
    }

    let visible_range = if let PageState::Journal {
        table: table_state, ..
    } = ui.current_page_mut()
    {
        utils::prepare_table_viewport_with_scrollbar(rect, n_entries, table_state)
    } else {
        return;
    };
    let table = journal_entries_table(
        filtered_entries
            .iter()
            .skip(visible_range.start)
            .take(visible_range.len()),
        visible_range.start,
        n_entries,
        is_active,
        ui,
        config::get_config().app_config.enable_relative_line_number,
        &selected_indices,
    );

    let table_offset = if let PageState::Journal {
        table: table_state, ..
    } = ui.current_page_mut()
    {
        utils::render_prepared_table_viewport_with_scrollbar(
            frame,
            table,
            rect,
            n_entries,
            table_state,
        );
        Some(table_state.offset())
    } else {
        None
    };
    if let Some(start) = table_offset {
        record_workspace_table_hits(ui, rect, start, n_entries);
    }
}

pub fn render_session_history_page(
    is_active: bool,
    frame: &mut Frame,
    state: &SharedState,
    ui: &mut UIStateGuard,
    rect: Rect,
) {
    let rect = {
        let content = render_workspace_page_layout(frame, state, ui, rect);
        workspace_content_frame(frame, ui, content, "Session History")
    };

    let selected = ui.session_history_selection.selected_keys();
    let items = {
        let data = state.data.read();
        session_history_items_with_selection(
            data.session_history.entries.iter().rev(),
            rect.width as usize,
            selected,
        )
    };
    if items.is_empty() {
        render_view_status(frame, &ui.theme, UiViewStatus::Empty, rect);
        return;
    }

    let selected_index = is_active
        .then(|| ui.current_page().selected_index())
        .flatten();
    let (list, len) = utils::construct_list_widget_with_width(
        &ui.theme,
        items,
        is_active,
        selected_index,
        Some(rect.width as usize),
        ui.presentation.focused_row_overflow,
        ui.focused_marquee_phase(),
    );
    let list = list.highlight_style(workspace_row_selection_style(ui, is_active));

    let list_offset = if let PageState::SessionHistory { list: list_state } = ui.current_page_mut()
    {
        utils::render_list_window_with_scrollbar(frame, list, rect, len, list_state);
        Some(list_state.offset())
    } else {
        None
    };
    if let Some(start) = list_offset {
        record_workspace_list_hits_with(ui, rect, start, len, WorkspaceHit::SessionHistoryRow);
    }
}

pub fn render_journal_lists_page(
    is_active: bool,
    frame: &mut Frame,
    state: &SharedState,
    ui: &mut UIStateGuard,
    rect: Rect,
) {
    let rect = {
        let content = render_workspace_page_layout(frame, state, ui, rect);
        workspace_content_frame(frame, ui, content, "Journal Lists")
    };
    let lists = {
        let data = state.data.read();
        data.journal.lists.clone()
    };
    let lists = ui.search_filtered_items_projection(&lists);
    let n_lists = lists.len();

    if lists.is_empty() {
        render_view_status(frame, &ui.theme, UiViewStatus::Empty, rect);
        return;
    }
    let visible_range = if let PageState::JournalLists { list: list_state } = ui.current_page_mut()
    {
        utils::prepare_list_viewport_with_scrollbar(rect, n_lists, list_state)
    } else {
        return;
    };
    let items = lists
        .iter()
        .skip(visible_range.start)
        .take(visible_range.len())
        .map(|list| {
            (
                JournalListRowProjection::from_list(list).page_label(),
                false,
            )
        })
        .collect::<Vec<_>>();

    let selected_index = if is_active {
        ui.current_page().selected_index()
    } else {
        None
    };
    let (list, len) = utils::construct_list_widget_with_width_and_placeholder_style_viewport(
        &ui.theme,
        items,
        visible_range.start,
        n_lists,
        is_active,
        selected_index,
        Some(rect.width as usize),
        ui.presentation.focused_row_overflow,
        ui.focused_marquee_phase(),
        false,
        Style::default(),
    );
    let list = list.highlight_style(workspace_row_selection_style(ui, is_active));

    let list_offset = if let PageState::JournalLists { list: list_state } = ui.current_page_mut() {
        utils::render_prepared_list_window(frame, list, rect, len, list_state);
        Some(list_state.offset())
    } else {
        None
    };
    if let Some(start) = list_offset {
        record_workspace_list_hits(ui, rect, start, n_lists);
    }
}

pub fn render_journal_list_page(
    is_active: bool,
    frame: &mut Frame,
    state: &SharedState,
    ui: &mut UIStateGuard,
    rect: Rect,
) {
    let list_id = match ui.current_page() {
        PageState::JournalList { list_id, .. } => list_id.clone(),
        _ => return,
    };

    let (title, complete_uris, entries) = {
        let data = state.data.read();
        let title = data.journal.list(&list_id).map_or_else(
            || "Journal List".to_string(),
            |list| format!("Journal List: {}", to_bidi_string(&list.name)),
        );
        let complete_uris = data
            .journal
            .list(&list_id)
            .map(|list| list.track_uris.clone())
            .unwrap_or_default();
        let entries = data.journal.list_entries(&list_id);
        (title, complete_uris, entries)
    };
    let filtered_entries = ui.search_filtered_items_projection(&entries);
    let n_entries = filtered_entries.len();
    let selected_indices = synchronize_journal_list_render_selection(
        ui,
        &list_id,
        &complete_uris,
        filtered_entries.iter(),
    );
    let rect = {
        let content = render_workspace_page_layout(frame, state, ui, rect);
        workspace_content_frame(frame, ui, content, &title)
    };
    if filtered_entries.is_empty() {
        render_view_status(frame, &ui.theme, UiViewStatus::Empty, rect);
        return;
    }
    let visible_range = if let PageState::JournalList {
        table: table_state, ..
    } = ui.current_page_mut()
    {
        utils::prepare_table_viewport_with_scrollbar(rect, n_entries, table_state)
    } else {
        return;
    };
    let table = journal_entries_table(
        filtered_entries
            .iter()
            .skip(visible_range.start)
            .take(visible_range.len()),
        visible_range.start,
        n_entries,
        is_active,
        ui,
        config::get_config().app_config.enable_relative_line_number,
        &selected_indices,
    );

    let table_offset = if let PageState::JournalList {
        table: table_state, ..
    } = ui.current_page_mut()
    {
        utils::render_prepared_table_viewport_with_scrollbar(
            frame,
            table,
            rect,
            n_entries,
            table_state,
        );
        Some(table_state.offset())
    } else {
        None
    };
    if let Some(start) = table_offset {
        record_workspace_table_hits(ui, rect, start, n_entries);
    }
}

fn journal_entries_table<'a>(
    entries: impl IntoIterator<Item = &'a TrackJournalEntry>,
    item_offset: usize,
    n_entries: usize,
    is_active: bool,
    ui: &mut UIStateGuard,
    enable_relative_line_number: bool,
    selected_indices: &[usize],
) -> Table<'static> {
    let selected_index = if is_active && enable_relative_line_number {
        ui.current_page().selected_index()
    } else {
        None
    };
    let focused_row = if is_active {
        ui.current_page().selected_index()
    } else {
        None
    };
    let focused_overflow = ui.presentation.focused_row_overflow;
    let focused_phase = ui.focused_marquee_phase();
    let rows = entries
        .into_iter()
        .enumerate()
        .map(|(visible_id, entry)| {
            let id = item_offset.saturating_add(visible_id);
            let index = utils::relative_table_line_number(id, selected_index);
            let history_row = HistoryRowProjection::from_journal(entry);
            Row::new(vec![
                Cell::from(if selected_marker(selected_indices, id) {
                    ">"
                } else {
                    ""
                }),
                Cell::from(Text::from(index).alignment(Alignment::Right)),
                Cell::from(if entry.listen_later { "L" } else { "" }),
                Cell::from(if entry.listened { "x" } else { "" }),
                Cell::from(entry.rating_text()),
                Cell::from(utils::focused_table_text(
                    history_row.title,
                    focused_row == Some(id),
                    focused_overflow,
                    focused_phase,
                )),
                Cell::from(utils::focused_table_text(
                    history_row.artist,
                    focused_row == Some(id),
                    focused_overflow,
                    focused_phase,
                )),
                Cell::from(utils::focused_table_text(
                    to_bidi_string(&entry.track.album_info()),
                    focused_row == Some(id),
                    focused_overflow,
                    focused_phase,
                )),
                Cell::from(utils::focused_table_text(
                    entry.note.clone(),
                    focused_row == Some(id),
                    focused_overflow,
                    focused_phase,
                )),
                Cell::from(history_row.time),
            ])
        })
        .collect::<Vec<_>>();

    let compact = ui.layout_policy().mode.is_compact();
    let (show_compact_album, show_compact_updated) =
        journal_compact_columns(compact, ui.presentation.compact_metadata);
    Table::new(
        rows,
        if compact {
            [
                Constraint::Length(1),
                Constraint::Length(if n_entries > 0 {
                    (n_entries.ilog10() + 1) as u16
                } else {
                    1
                }),
                Constraint::Length(1),
                Constraint::Length(1),
                Constraint::Length(4),
                Constraint::Fill(5),
                Constraint::Fill(3),
                if show_compact_album {
                    Constraint::Fill(2)
                } else {
                    Constraint::Length(0)
                },
                Constraint::Fill(5),
                if show_compact_updated {
                    Constraint::Length(12)
                } else {
                    Constraint::Length(0)
                },
            ]
        } else {
            [
                Constraint::Length(1),
                Constraint::Length(if n_entries > 0 {
                    (n_entries.ilog10() + 1) as u16
                } else {
                    1
                }),
                Constraint::Length(1),
                Constraint::Length(1),
                Constraint::Length(4),
                Constraint::Fill(4),
                Constraint::Fill(3),
                Constraint::Fill(3),
                Constraint::Fill(5),
                Constraint::Length(12),
            ]
        },
    )
    .style(ui.theme.workspace_base())
    .header(
        Row::new(if compact {
            vec![
                Cell::from(""),
                Cell::from(Text::from("#").alignment(Alignment::Right)),
                Cell::from("L"),
                Cell::from("D"),
                Cell::from("Rate"),
                Cell::from("Title"),
                Cell::from("Artists"),
                Cell::from(if show_compact_album { "Album" } else { "" }),
                Cell::from("Note"),
                Cell::from(if show_compact_updated { "Updated" } else { "" }),
            ]
        } else {
            vec![
                Cell::from(""),
                Cell::from(Text::from("#").alignment(Alignment::Right)),
                Cell::from("L"),
                Cell::from("D"),
                Cell::from("Rate"),
                Cell::from("Title"),
                Cell::from("Artists"),
                Cell::from("Album"),
                Cell::from("Note"),
                Cell::from("Updated"),
            ]
        })
        .style(ui.theme.workspace_table_header()),
    )
    .column_spacing(2)
    .row_highlight_style(workspace_row_selection_style(ui, is_active))
}

fn journal_compact_columns(compact: bool, metadata: config::CompactMetadataMode) -> (bool, bool) {
    if compact {
        (metadata.shows_album(), metadata.shows_added_at())
    } else {
        (false, false)
    }
}

#[cfg(test)]
fn unified_spotify_queue_label(kind: crate::state::MediaKind) -> &'static str {
    match kind {
        crate::state::MediaKind::Track => "Spotify track",
        crate::state::MediaKind::Episode => "Spotify episode",
        crate::state::MediaKind::Video => "Spotify video",
    }
}

pub fn render_queue_page(
    is_active: bool,
    frame: &mut Frame,
    state: &SharedState,
    ui: &mut UIStateGuard,
    rect: Rect,
) {
    render_workspace_queue_page(is_active, frame, state, ui, rect);
}

fn render_workspace_queue_page(
    is_active: bool,
    frame: &mut Frame,
    state: &SharedState,
    ui: &mut UIStateGuard,
    rect: Rect,
) {
    let layout = ui
        .layout_policy()
        .workspace(rect, WorkspaceLayoutKind::Library);
    ui.workspace_layout = layout;
    if layout.show_navigation {
        let playback_provider = state
            .player
            .read()
            .effective_playback_provider(ui.active_provider);
        render_workspace_navigation(frame, ui, layout.navigation, playback_provider);
        workspace_vertical_rule(
            frame,
            &ui.theme,
            Rect::new(layout.navigation.right(), rect.y, 1, rect.height),
        );
    }
    if layout.content.is_empty() {
        return;
    }
    render_queue_table(is_active, frame, state, ui, layout.content);
}

fn render_queue_table(
    is_active: bool,
    frame: &mut Frame,
    state: &SharedState,
    ui: &mut UIStateGuard,
    rect: Rect,
) {
    let (item_count, unified_instance_id, unified) = {
        let player = state.player.read();
        (
            player.queue_display_item_count(),
            player.authoritative_unified_queue_instance_id(),
            player.authoritative_unified_queue_instance_id().is_some(),
        )
    };
    let selection_rows = {
        let player = state.player.read();
        if unified {
            QueueSelectionRows::Unified(
                (0..item_count)
                    .map(|index| {
                        player
                            .queue_display_item_ref(index)
                            .and_then(|item| item.media_id().zip(item.unified_entry_id()))
                    })
                    .collect::<Option<Vec<_>>>(),
            )
        } else {
            QueueSelectionRows::Native(
                (0..item_count)
                    .map(|index| {
                        player
                            .queue_display_item_ref(index)
                            .and_then(|item| item.media_id())
                    })
                    .collect(),
            )
        }
    };
    let selected_indices =
        synchronize_queue_render_selection(ui, selection_rows, item_count, unified_instance_id);
    frame.render_widget(Block::default().style(ui.theme.workspace_base()), rect);
    let full_profile = collection_full_profile(frame, rect);
    let visible_rows = workspace_collection_visible_rows(rect, full_profile);
    let (row_offset, focused_row) = match ui.current_page_mut() {
        PageState::Queue { table, .. } => {
            utils::adjust_table_offset(table, item_count, visible_rows);
            (table.offset(), table.selected())
        }
        _ => return,
    };
    let (rows, playing_index, scope) = {
        let player = state.player.read();
        let data = state.data.read();
        let labels = SpotifyQueueLabels::new(&player, &data, &ui.spotify_queue_labels);
        let mut sources = std::collections::BTreeMap::<String, usize>::new();
        let mut playing_index = None;
        let mut rows = Vec::new();
        for index in 0..item_count {
            let Some(item) = player.queue_display_item_ref(index) else {
                continue;
            };
            let projection = QueueRowProjection::from_item(&item, &labels);
            *sources.entry(projection.source).or_default() += 1;
            if projection.is_current {
                playing_index = Some(index);
            }
            if (row_offset..row_offset.saturating_add(usize::from(visible_rows))).contains(&index) {
                rows.push(CollectionTrackRow {
                    title: projection.title,
                    artist: projection.artists,
                    duration: projection.duration,
                });
            }
        }
        let scope = sources
            .into_iter()
            .map(|(source, count)| format!("{count} {source}"))
            .collect::<Vec<_>>()
            .join(" · ");
        (rows, playing_index, scope)
    };
    workspace_table_heading(frame, &ui.theme, rect, "Queue", &scope, full_profile, None);
    if item_count == 0 {
        render_view_status(
            frame,
            &ui.theme,
            UiViewStatus::Empty,
            Rect::new(
                rect.x,
                rect.y.saturating_add(4),
                rect.width,
                rect.height.saturating_sub(4),
            ),
        );
        return;
    }
    let theme = ui.theme.clone();
    let focused_overflow = ui.presentation.focused_row_overflow;
    let focused_phase = ui.focused_marquee_phase();
    let mut hits = Vec::new();
    if let PageState::Queue { table, .. } = ui.current_page_mut() {
        render_workspace_collection_table(
            frame,
            &theme,
            rect,
            &rows,
            row_offset,
            item_count,
            focused_row,
            is_active,
            &selected_indices,
            playing_index,
            table,
            &format!("{item_count} queued"),
            theme.workspace_secondary_text(),
            CollectionDetailColumn::ARTIST,
            full_profile,
            focused_overflow,
            focused_phase,
            &mut hits,
        );
    }
    ui.workspace_hits
        .extend(hits.into_iter().map(|(rect, hit)| {
            let hit = match hit {
                WorkspaceHit::ContextRow(index) => WorkspaceHit::QueueRow(index),
                other => other,
            };
            (rect, hit)
        }));
}

#[cfg(test)]
mod queue_layout_tests {
    use super::queue_row_hit_rect;
    use ratatui::layout::Rect;

    #[test]
    fn queue_hit_geometry_supports_compact_and_table_rows() {
        let sidebar = Rect::new(4, 10, 24, 7);
        assert_eq!(
            queue_row_hit_rect(sidebar, 0, 0, 2, 3),
            Some(Rect::new(4, 10, 24, 2))
        );
        assert_eq!(
            queue_row_hit_rect(sidebar, 0, 1, 2, 3),
            Some(Rect::new(4, 13, 24, 2))
        );
        assert_eq!(queue_row_hit_rect(sidebar, 0, 2, 2, 3), None);

        let table = Rect::new(8, 20, 30, 3);
        assert_eq!(
            queue_row_hit_rect(table, 5, 5, 1, 1),
            Some(Rect::new(8, 20, 30, 1))
        );
        assert_eq!(
            queue_row_hit_rect(table, 5, 7, 1, 1),
            Some(Rect::new(8, 22, 30, 1))
        );
        assert_eq!(queue_row_hit_rect(table, 5, 8, 1, 1), None);
    }
}

fn settings_display_label(key: &str) -> String {
    to_bidi_string(&config::setting_label(key))
}

/// A stored setting as a person reads it: strings without TOML quotes or
/// escapes, booleans as On/Off and lists as plain comma-separated items.
fn settings_display_value(value: &str) -> String {
    fn readable(value: &toml::Value) -> String {
        match value {
            toml::Value::String(text) => text.replace('\n', " ⏎ "),
            toml::Value::Boolean(true) => "On".to_owned(),
            toml::Value::Boolean(false) => "Off".to_owned(),
            toml::Value::Array(items) => items.iter().map(readable).collect::<Vec<_>>().join(", "),
            other => other.to_string(),
        }
    }
    let parsed = toml::from_str::<toml::Table>(&format!("value = {value}"))
        .ok()
        .and_then(|mut table| table.remove("value"));
    to_bidi_string(&parsed.as_ref().map_or_else(|| value.to_owned(), readable))
}

fn settings_workspace_projection<'a>(
    settings: &'a [config::AppConfigSetting],
    category: SettingsCategory,
    query: Option<&str>,
) -> Vec<(usize, &'a config::AppConfigSetting)> {
    settings_filter_projection(settings, query)
        .into_iter()
        .filter(|(_, setting)| category.includes(setting))
        .collect()
}

fn settings_workspace_effect(setting: &config::AppConfigSetting) -> &'static str {
    if setting.restart_required {
        "On restart"
    } else {
        match &setting.kind {
            config::AppConfigValueKind::Status => "Read-only",
            config::AppConfigValueKind::Action(_) => "Explicit action",
            _ => "On apply",
        }
    }
}

fn settings_workspace_runtime_note(restart_required: bool) -> &'static str {
    if restart_required {
        "Saved preferences take effect after restart."
    } else {
        "Changes apply to the running UI."
    }
}

fn settings_workspace_description(category: SettingsCategory) -> &'static str {
    match category {
        SettingsCategory::Preferences => "Local preferences",
        SettingsCategory::Accounts => "Saved provider accounts",
    }
}

fn render_workspace_settings_rail(
    frame: &mut Frame,
    ui: &mut UIStateGuard,
    nav: Rect,
    category: SettingsCategory,
) {
    if nav.is_empty() {
        return;
    }
    frame.render_widget(Block::default().style(ui.theme.workspace_panel()), nav);
    workspace_text(
        frame,
        Rect::new(
            nav.x.saturating_add(2),
            nav.y.saturating_add(1),
            nav.width.saturating_sub(4),
            1,
        ),
        "Settings",
        ui.theme.workspace_heading(),
    );

    for (offset, item) in [
        (
            3_u16,
            SettingsRailItem::Category(SettingsCategory::Preferences),
        ),
        (5, SettingsRailItem::Category(SettingsCategory::Accounts)),
        (10, SettingsRailItem::BackToPlayer),
    ] {
        let y = nav.y.saturating_add(offset);
        if y >= nav.bottom() {
            continue;
        }
        let row = workspace_navigation_row_rect(nav, y);
        if row.is_empty() {
            continue;
        }
        let active = matches!(item, SettingsRailItem::Category(item) if item == category);
        let focused = ui.workspace_focus == WorkspaceFocusState::Navigation;
        let style = if active {
            ui.theme.workspace_navigation_active()
        } else {
            ui.theme.workspace_panel()
        };
        frame.render_widget(Block::default().style(style), row);
        let label = match item {
            SettingsRailItem::Category(item) => item.label(),
            SettingsRailItem::BackToPlayer => "Back to player",
        };
        workspace_text(
            frame,
            label_rect(row),
            label,
            if focused && active {
                ui.theme
                    .workspace_base()
                    .patch(ui.theme.workspace_focus_indicator())
            } else {
                style
            },
        );
        ui.workspace_hits
            .push((row, WorkspaceHit::SettingsRail(item)));
    }

    let divider_y = nav.y.saturating_add(8);
    if divider_y < nav.bottom() {
        workspace_rule(
            frame,
            &ui.theme,
            Rect::new(
                nav.x.saturating_add(2),
                divider_y,
                nav.width.saturating_sub(4),
                1,
            ),
        );
    }

    if nav.height < 15 {
        return;
    }
    let scope_start = nav.bottom().saturating_sub(3);
    workspace_text(
        frame,
        Rect::new(
            nav.x.saturating_add(2),
            scope_start,
            nav.width.saturating_sub(4),
            1,
        ),
        "Example profile",
        ui.theme.workspace_secondary_text(),
    );
    workspace_text(
        frame,
        Rect::new(
            nav.x.saturating_add(2),
            scope_start.saturating_add(1),
            nav.width.saturating_sub(4),
            1,
        ),
        "Local preferences",
        ui.theme.workspace_secondary_text(),
    );
}

fn settings_workspace_selected<'a>(
    settings: &'a [config::AppConfigSetting],
    category: SettingsCategory,
    query: Option<&str>,
    selected_source: Option<usize>,
) -> Option<(usize, &'a config::AppConfigSetting)> {
    settings_workspace_projection(settings, category, query)
        .into_iter()
        .find(|(source, _)| Some(*source) == selected_source)
}

fn render_workspace_settings_inspector(
    frame: &mut Frame,
    ui: &UIStateGuard,
    rect: Rect,
    selected: Option<&config::AppConfigSetting>,
) {
    if rect.is_empty() {
        return;
    }
    frame.render_widget(Block::default().style(ui.theme.workspace_panel()), rect);
    let x = rect.x.saturating_add(2);
    let width = rect.width.saturating_sub(4);
    let Some(setting) = selected else {
        workspace_text(
            frame,
            Rect::new(x, rect.y.saturating_add(1), width, 1),
            "No setting selected",
            ui.theme.workspace_heading(),
        );
        return;
    };

    workspace_text(
        frame,
        Rect::new(x, rect.y.saturating_add(1), width, 1),
        settings_display_label(&setting.key),
        ui.theme.workspace_heading(),
    );
    frame.render_widget(
        Paragraph::new(to_bidi_string(config::setting_description(&setting.key)))
            .style(ui.theme.workspace_secondary_text())
            .wrap(Wrap { trim: true }),
        Rect::new(x, rect.y.saturating_add(3), width, 3),
    );

    let fields = [
        (
            "Control".to_owned(),
            match &setting.kind {
                config::AppConfigValueKind::Bool => "Boolean",
                config::AppConfigValueKind::Choice(_) => "Choice",
                config::AppConfigValueKind::MultiChoice(_) => "Multiple choice",
                config::AppConfigValueKind::Value => "Value",
                config::AppConfigValueKind::Status => "Status",
                config::AppConfigValueKind::Action(_) => "Action",
            }
            .to_owned(),
        ),
        (
            "Scope".to_owned(),
            settings_workspace_description(
                if setting.section == config::AppConfigSection::Accounts {
                    SettingsCategory::Accounts
                } else {
                    SettingsCategory::Preferences
                },
            )
            .to_owned(),
        ),
        (
            "Effect".to_owned(),
            settings_workspace_effect(setting).to_owned(),
        ),
        ("Saved".to_owned(), settings_display_value(&setting.value)),
        ("Draft".to_owned(), settings_display_value(&setting.value)),
        (
            "Effective".to_owned(),
            settings_display_value(&setting.value),
        ),
    ];
    for (index, (label, value)) in fields.into_iter().enumerate() {
        let y = rect.y.saturating_add(match index {
            0 => 7,
            1 => 9,
            2 => 11,
            3 => 15,
            4 => 17,
            _ => 19,
        });
        workspace_text(
            frame,
            Rect::new(x, y, 12.min(width), 1),
            label,
            ui.theme.workspace_secondary_text(),
        );
        workspace_text(
            frame,
            Rect::new(x.saturating_add(12), y, width.saturating_sub(12), 1),
            value,
            ui.theme.workspace_base(),
        );
    }
    workspace_text(
        frame,
        Rect::new(x, rect.y.saturating_add(22), width, 1),
        settings_workspace_runtime_note(setting.restart_required),
        ui.theme.workspace_secondary_text(),
    );
    workspace_text(
        frame,
        Rect::new(x, rect.y.saturating_add(24), width, 1),
        if setting.restart_required {
            "Restart required after saving."
        } else {
            "No unsaved draft is pending."
        },
        ui.theme.workspace_secondary_text(),
    );
    workspace_text(
        frame,
        Rect::new(x, rect.y.saturating_add(25), width, 1),
        "Apply saves; Discard reloads saved values.",
        ui.theme.workspace_secondary_text(),
    );
}

/// Settings pages shorter than this use the compact, settings-first layout.
const SETTINGS_COMPACT_BELOW_HEIGHT: u16 = 16;

fn render_workspace_settings_page(
    is_active: bool,
    frame: &mut Frame,
    ui: &mut UIStateGuard,
    rect: Rect,
) {
    let filter_query = match ui.popup.as_ref() {
        Some(PopupState::Search { query }) => Some(query.clone()),
        _ => None,
    };
    let (settings, error, notice) = match ui.current_page() {
        PageState::Settings {
            settings,
            error,
            notice,
            ..
        } => (settings.clone(), error.clone(), notice.clone()),
        _ => return,
    };
    let category = ui.workspace_settings_category;
    let projection = settings_workspace_projection(&settings, category, filter_query.as_deref());
    let selected_source = ui.current_page().selected_index();
    let selected = settings_workspace_selected(
        &settings,
        category,
        filter_query.as_deref(),
        selected_source,
    );
    if selected.is_none() {
        if let Some((source, _)) = projection.first() {
            ui.current_page_mut().select(*source);
        }
    }
    let selected_source = ui.current_page().selected_index();
    let selected = settings_workspace_selected(
        &settings,
        category,
        filter_query.as_deref(),
        selected_source,
    );

    let layout = ui
        .layout_policy()
        .workspace(rect, WorkspaceLayoutKind::Settings);
    ui.workspace_layout = layout;
    if layout.show_navigation {
        render_workspace_settings_rail(frame, ui, layout.navigation, category);
        workspace_vertical_rule(
            frame,
            &ui.theme,
            Rect::new(layout.navigation.right(), rect.y, 1, rect.height),
        );
    }
    if layout.show_right {
        workspace_vertical_rule(
            frame,
            &ui.theme,
            Rect::new(layout.content.right(), rect.y, 1, rect.height),
        );
    }
    if layout.content.is_empty() {
        return;
    }

    let content = layout.content;
    frame.render_widget(Block::default().style(ui.theme.workspace_base()), content);
    if layout.show_navigation {
        workspace_text(
            frame,
            Rect::new(
                content.x.saturating_add(2),
                content.y.saturating_add(1),
                content.width.saturating_sub(4),
                1,
            ),
            category.label(),
            ui.theme.workspace_heading(),
        );
    } else {
        workspace_text(
            frame,
            Rect::new(
                content.x.saturating_add(2),
                content.y,
                content.width.saturating_sub(4),
                1,
            ),
            format!("Settings · {}", category.label()),
            ui.theme.workspace_heading(),
        );
    }
    let title_offset = u16::from(layout.show_navigation);
    // Short pages keep the settings themselves: the subtitle and the
    // selected setting's description go, and the pending line shares one row
    // with the Apply/Discard actions.
    let compact = content.height < SETTINGS_COMPACT_BELOW_HEIGHT;
    if !compact {
        workspace_text(
            frame,
            Rect::new(
                content.x.saturating_add(2),
                content.y.saturating_add(title_offset + 2),
                content.width.saturating_sub(4),
                1,
            ),
            settings_workspace_description(category),
            ui.theme.workspace_secondary_text(),
        );
    }

    let canonical = layout.show_right && content.width >= 100 && content.height >= 30;
    let tiles_top = content.y.saturating_add(if canonical {
        5
    } else if compact {
        title_offset + 1
    } else {
        4
    });
    // Bottom band, top to bottom: the selected setting's description (only
    // without an inspector), a rule, the pending-change line, and actions.
    // The tiles end above the band's first row so they never draw over it.
    let (pending_y, action_y) = if canonical {
        (
            content.bottom().saturating_sub(6),
            content.bottom().saturating_sub(3),
        )
    } else if compact {
        let row = content.bottom().saturating_sub(1);
        (row, row)
    } else {
        (
            content.bottom().saturating_sub(3),
            content.bottom().saturating_sub(1),
        )
    };
    let shows_description = !layout.show_right && !canonical && !compact;
    let tiles_bottom = if compact {
        pending_y
    } else {
        pending_y
            .saturating_sub(1)
            .saturating_sub(u16::from(shows_description))
    };
    let tiles_area = Rect::new(
        content.x.saturating_add(2),
        tiles_top,
        content.width.saturating_sub(4),
        tiles_bottom.saturating_sub(tiles_top),
    );
    if !tiles_area.is_empty() {
        settings_tiles::render_settings_tiles(
            frame,
            ui,
            tiles_area,
            &settings,
            category,
            filter_query.as_deref(),
            is_active && ui.workspace_focus == WorkspaceFocusState::Context,
        );
    }

    if !compact {
        workspace_rule(
            frame,
            &ui.theme,
            Rect::new(content.x, pending_y.saturating_sub(1), content.width, 1),
        );
    }
    // Compact pages put the actions at the right end of the pending row.
    let apply_width = SettingsWorkspaceAction::Apply.label().chars().count() as u16 + 4;
    let discard_width = SettingsWorkspaceAction::Discard.label().chars().count() as u16 + 4;
    let actions_x = if compact {
        content
            .right()
            .saturating_sub(2)
            .saturating_sub(apply_width + 2 + discard_width)
    } else {
        content.x.saturating_add(2)
    };
    let has_error = error.is_some();
    let quiet = error.is_none() && notice.is_none();
    let pending = if let Some(error) = error {
        error
    } else if let Some(notice) = notice {
        notice
    } else {
        "No unsaved changes".to_owned()
    };
    let pending_width = if compact {
        actions_x.saturating_sub(content.x.saturating_add(4))
    } else {
        content.width.saturating_sub(4)
    };
    // "No unsaved changes" says nothing actionable; where it would be cut,
    // leave the row to the actions. Errors and notices are always shown.
    let pending_width = if quiet && pending.chars().count() > usize::from(pending_width) {
        0
    } else {
        pending_width
    };
    workspace_text(
        frame,
        Rect::new(content.x.saturating_add(2), pending_y, pending_width, 1),
        utils::bounded_text(&pending, usize::from(pending_width)),
        if has_error {
            ui.theme.workspace_status_warning()
        } else {
            ui.theme.workspace_secondary_text()
        },
    );

    let actions = [
        (SettingsWorkspaceAction::Apply, actions_x),
        (
            SettingsWorkspaceAction::Discard,
            if compact {
                actions_x.saturating_add(apply_width + 2)
            } else {
                content.x.saturating_add(22)
            },
        ),
    ];
    for (action, x) in actions {
        let width = action.label().chars().count() as u16 + 4;
        let row = Rect::new(x, action_y, width.min(content.right().saturating_sub(x)), 1);
        let focused = ui.workspace_focus == WorkspaceFocusState::Actions
            && ui.workspace_settings_action == action;
        frame.render_widget(
            Block::default().style(if focused {
                ui.theme.workspace_selection_active()
            } else {
                ui.theme.workspace_panel()
            }),
            row,
        );
        let action_text_style = if focused {
            ui.theme
                .workspace_base()
                .patch(ui.theme.workspace_selection_active())
        } else {
            ui.theme.workspace_base()
        };
        workspace_text(
            frame,
            Rect::new(
                row.x.saturating_add(2),
                row.y,
                row.width.saturating_sub(2),
                1,
            ),
            action.label(),
            action_text_style,
        );
        ui.workspace_hits
            .push((row, WorkspaceHit::SettingsAction(action)));
    }

    if layout.show_right {
        render_workspace_settings_inspector(
            frame,
            ui,
            layout.right,
            selected.map(|(_, setting)| setting),
        );
    } else if canonical {
        // Kept as an explicit branch for future wide policy changes; the
        // measured inspector is only shown when its rectangle is present.
    } else if let Some((_, setting)) = selected.filter(|_| !compact) {
        workspace_text(
            frame,
            Rect::new(
                content.x.saturating_add(2),
                pending_y.saturating_sub(2),
                content.width.saturating_sub(4),
                1,
            ),
            utils::bounded_text(
                config::setting_description(&setting.key),
                content.width.saturating_sub(4) as usize,
            ),
            ui.theme.workspace_secondary_text(),
        );
    }
}

pub fn render_settings_page(is_active: bool, frame: &mut Frame, ui: &mut UIStateGuard, rect: Rect) {
    render_workspace_settings_page(is_active, frame, ui, rect);
}

pub fn render_welcome_page(is_active: bool, frame: &mut Frame, ui: &mut UIStateGuard, rect: Rect) {
    super::welcome::render_welcome_page(is_active, frame, ui, rect);
}

/// Render windows for an artist context page, which includes
/// - A top track table
/// - A liked songs table (tracks liked by the user from this artist)
/// - An album table
/// - A related artist list
fn release_date_visible<'a>(dates: impl IntoIterator<Item = &'a str>) -> bool {
    dates.into_iter().any(|date| !date.trim().is_empty())
}

#[derive(Debug, PartialEq, Eq)]
struct ListenBrainzAlbumFallbackRow {
    release_date: String,
    release_type: String,
    name: String,
    listens: String,
    visual: ListenBrainzRowVisual,
}

#[derive(Debug, PartialEq, Eq)]
enum ListenBrainzAlbumFallbackBody {
    Message(String),
    ReleaseGroups(Vec<ListenBrainzAlbumFallbackRow>),
}

#[derive(Debug, PartialEq, Eq)]
struct ListenBrainzAlbumFallbackTable {
    title: &'static str,
    body: ListenBrainzAlbumFallbackBody,
}

fn listenbrainz_album_fallback_rows(
    enrichment: &crate::state::ListenBrainzArtistEnrichment,
    resolutions: &ttl_cache::TtlCache<String, crate::state::ListenBrainzAlbumResolution>,
) -> Option<ListenBrainzAlbumFallbackTable> {
    use crate::state::{ListenBrainzArtistEnrichment, ListenBrainzCollectionStatus};

    match enrichment {
        ListenBrainzArtistEnrichment::NotRequested => None,
        ListenBrainzArtistEnrichment::Pending | ListenBrainzArtistEnrichment::Loading { .. } => {
            Some(ListenBrainzAlbumFallbackTable {
                title: "Albums on ListenBrainz",
                body: ListenBrainzAlbumFallbackBody::Message(
                    "Loading ListenBrainz albums...".to_owned(),
                ),
            })
        }
        ListenBrainzArtistEnrichment::Available {
            release_groups_status,
            release_groups,
            ..
        } => match release_groups_status {
            ListenBrainzCollectionStatus::Available => Some(ListenBrainzAlbumFallbackTable {
                title: "Albums on ListenBrainz",
                body: ListenBrainzAlbumFallbackBody::ReleaseGroups(
                    release_groups
                        .iter()
                        .map(|release| {
                            let key =
                                crate::state::listenbrainz_album_key(&release.release_group_mbid);
                            let visual = match resolutions.get(&key) {
                                None => ListenBrainzRowVisual::Unresolved,
                                Some(crate::state::ListenBrainzAlbumResolution::Resolving {
                                    ..
                                }) => ListenBrainzRowVisual::Resolving,
                                Some(crate::state::ListenBrainzAlbumResolution::Resolved(_)) => {
                                    ListenBrainzRowVisual::Resolved
                                }
                                Some(
                                    crate::state::ListenBrainzAlbumResolution::NoSpotifyRelation
                                    | crate::state::ListenBrainzAlbumResolution::Unavailable,
                                ) => ListenBrainzRowVisual::Unavailable,
                            };
                            ListenBrainzAlbumFallbackRow {
                                release_date: release.release_date.clone().unwrap_or_default(),
                                release_type: release.release_type.clone().unwrap_or_default(),
                                name: release.name.clone(),
                                listens: release
                                    .total_listen_count
                                    .map(|count| count.to_string())
                                    .unwrap_or_default(),
                                visual,
                            }
                        })
                        .collect(),
                ),
            }),
            ListenBrainzCollectionStatus::Empty => Some(ListenBrainzAlbumFallbackTable {
                title: "Albums on ListenBrainz",
                body: ListenBrainzAlbumFallbackBody::Message(
                    "No release groups found on ListenBrainz".to_owned(),
                ),
            }),
            ListenBrainzCollectionStatus::Unavailable => Some(ListenBrainzAlbumFallbackTable {
                title: "Albums on ListenBrainz",
                body: ListenBrainzAlbumFallbackBody::Message(
                    "ListenBrainz albums are unavailable".to_owned(),
                ),
            }),
            ListenBrainzCollectionStatus::NotRequested => None,
        },
        ListenBrainzArtistEnrichment::NoArtistMatch => Some(ListenBrainzAlbumFallbackTable {
            title: "Albums on ListenBrainz",
            body: ListenBrainzAlbumFallbackBody::Message(
                "No MusicBrainz match for this artist".to_owned(),
            ),
        }),
        ListenBrainzArtistEnrichment::Unavailable => Some(ListenBrainzAlbumFallbackTable {
            title: "Albums on ListenBrainz",
            body: ListenBrainzAlbumFallbackBody::Message(
                "ListenBrainz albums are unavailable".to_owned(),
            ),
        }),
    }
}

fn listenbrainz_album_fallback_table(
    fallback: ListenBrainzAlbumFallbackTable,
    theme: &config::Theme,
    compact: bool,
    is_active: bool,
) -> (Table<'static>, usize) {
    match fallback.body {
        ListenBrainzAlbumFallbackBody::Message(message) => {
            let table = Table::new([Row::new([Cell::from(message)])], [Constraint::Fill(1)])
                .row_highlight_style(theme.workspace_selection_inactive());
            (table, 1)
        }
        ListenBrainzAlbumFallbackBody::ReleaseGroups(rows) => {
            let row_count = rows.len();
            let show_date = release_date_visible(rows.iter().map(|row| row.release_date.as_str()));
            let rows = rows.into_iter().map(|row| {
                Row::new([
                    Cell::from(row.release_date),
                    Cell::from(row.release_type),
                    Cell::from(to_bidi_string(&row.name)),
                    Cell::from(Text::from(row.listens).alignment(Alignment::Right)),
                ])
                .style(listenbrainz_row_style(theme, row.visual))
            });
            let table = Table::new(
                rows,
                [
                    if show_date {
                        Constraint::Length(10)
                    } else {
                        Constraint::Length(0)
                    },
                    if compact {
                        Constraint::Length(0)
                    } else {
                        Constraint::Length(8)
                    },
                    Constraint::Fill(1),
                    if compact {
                        Constraint::Length(0)
                    } else {
                        Constraint::Length(9)
                    },
                ],
            )
            .header(
                Row::new([
                    Cell::from(if show_date { "Date" } else { "" }),
                    Cell::from(if compact { "" } else { "Type" }),
                    Cell::from("Name"),
                    Cell::from(if compact { "" } else { "Listens" }),
                ])
                .style(theme.workspace_table_header()),
            )
            .column_spacing(2)
            .row_highlight_style({
                if is_active {
                    theme.workspace_selection_active()
                } else {
                    theme.workspace_selection_inactive()
                }
            });
            (table, row_count)
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ListenBrainzRowVisual {
    Unresolved,
    Resolving,
    Resolved,
    Unavailable,
}

#[derive(Debug, PartialEq, Eq)]
struct ListenBrainzFallbackRow {
    title: String,
    artists: String,
    listens: String,
    duration: String,
    visual: ListenBrainzRowVisual,
}

#[derive(Debug, PartialEq, Eq)]
enum ListenBrainzFallbackBody {
    Message(String),
    Recordings(Vec<ListenBrainzFallbackRow>),
}

#[derive(Debug, PartialEq, Eq)]
struct ListenBrainzFallbackTable {
    title: &'static str,
    body: ListenBrainzFallbackBody,
}

fn listenbrainz_fallback_rows(
    enrichment: &crate::state::ListenBrainzArtistEnrichment,
    resolutions: &ttl_cache::TtlCache<String, crate::state::ListenBrainzRecordingResolution>,
) -> Option<ListenBrainzFallbackTable> {
    use crate::state::ListenBrainzArtistEnrichment;

    match enrichment {
        ListenBrainzArtistEnrichment::NotRequested => None,
        ListenBrainzArtistEnrichment::Pending | ListenBrainzArtistEnrichment::Loading { .. } => {
            Some(ListenBrainzFallbackTable {
                title: "ListenBrainz Fallback",
                body: ListenBrainzFallbackBody::Message(
                    "Loading ListenBrainz enrichment...".to_owned(),
                ),
            })
        }
        ListenBrainzArtistEnrichment::Available {
            artist_name,
            recordings_status,
            recordings,
            ..
        } => match recordings_status {
            crate::state::ListenBrainzCollectionStatus::Available => {
                Some(ListenBrainzFallbackTable {
                    title: "Popular on ListenBrainz",
                    body: ListenBrainzFallbackBody::Recordings(
                        recordings
                            .iter()
                            .map(|recording| {
                        let key =
                            crate::state::listenbrainz_recording_key(&recording.recording_mbid);
                        let (title, artists, duration, visual) = match resolutions.get(&key) {
                            None => (
                                recording.name.clone(),
                                artist_name.clone(),
                                String::new(),
                                ListenBrainzRowVisual::Unresolved,
                            ),
                            Some(crate::state::ListenBrainzRecordingResolution::Resolving {
                                ..
                            }) => (
                                recording.name.clone(),
                                artist_name.clone(),
                                String::new(),
                                ListenBrainzRowVisual::Resolving,
                            ),
                            Some(crate::state::ListenBrainzRecordingResolution::Resolved(
                                track,
                            )) => (
                                track.display_name().into_owned(),
                                track.artists_info(),
                                format!(
                                    "{}:{:02}",
                                    track.duration.as_secs() / 60,
                                    track.duration.as_secs() % 60
                                ),
                                ListenBrainzRowVisual::Resolved,
                            ),
                            Some(
                                crate::state::ListenBrainzRecordingResolution::NoSpotifyRelation
                                | crate::state::ListenBrainzRecordingResolution::Unavailable,
                            ) => (
                                recording.name.clone(),
                                artist_name.clone(),
                                String::new(),
                                ListenBrainzRowVisual::Unavailable,
                            ),
                        };
                        ListenBrainzFallbackRow {
                            title,
                            artists,
                            listens: recording
                                .total_listen_count
                                .map(|count| count.to_string())
                                .unwrap_or_default(),
                            duration,
                            visual,
                        }
                            })
                            .collect(),
                    ),
                })
            }
            crate::state::ListenBrainzCollectionStatus::Empty => {
                Some(ListenBrainzFallbackTable {
                    title: "ListenBrainz Fallback",
                    body: ListenBrainzFallbackBody::Message(
                        "No popular recordings found on ListenBrainz".to_owned(),
                    ),
                })
            }
            crate::state::ListenBrainzCollectionStatus::Unavailable => {
                Some(ListenBrainzFallbackTable {
                    title: "ListenBrainz Fallback",
                    body: ListenBrainzFallbackBody::Message(
                        "ListenBrainz recordings are unavailable".to_owned(),
                    ),
                })
            }
            crate::state::ListenBrainzCollectionStatus::NotRequested => None,
        },
        ListenBrainzArtistEnrichment::NoArtistMatch => Some(ListenBrainzFallbackTable {
            title: "ListenBrainz Fallback",
            body: ListenBrainzFallbackBody::Message(
                "No MusicBrainz match for this artist".to_owned(),
            ),
        }),
        ListenBrainzArtistEnrichment::Unavailable => Some(ListenBrainzFallbackTable {
            title: "ListenBrainz Fallback",
            body: ListenBrainzFallbackBody::Message(
                "ListenBrainz enrichment is unavailable".to_owned(),
            ),
        }),
    }
}

fn listenbrainz_row_style(theme: &config::Theme, visual: ListenBrainzRowVisual) -> Style {
    let secondary_row = theme.workspace_selection_inactive();
    let table_header = theme.workspace_table_header();
    match visual {
        ListenBrainzRowVisual::Unresolved => secondary_row.add_modifier(Modifier::DIM),
        ListenBrainzRowVisual::Resolving => table_header.add_modifier(Modifier::ITALIC),
        ListenBrainzRowVisual::Resolved => Style::default(),
        ListenBrainzRowVisual::Unavailable => {
            secondary_row.add_modifier(Modifier::DIM | Modifier::CROSSED_OUT)
        }
    }
}

fn listenbrainz_fallback_table(
    fallback: ListenBrainzFallbackTable,
    theme: &config::Theme,
    active: bool,
) -> (Table<'static>, usize) {
    match fallback.body {
        ListenBrainzFallbackBody::Message(message) => {
            let table = Table::new([Row::new([Cell::from(message)])], [Constraint::Fill(1)])
                .row_highlight_style(theme.workspace_selection_inactive());
            (table, 1)
        }
        ListenBrainzFallbackBody::Recordings(rows) => {
            let row_count = rows.len();
            let number_width = if row_count > 0 {
                (row_count.ilog10() + 1) as u16
            } else {
                1
            };
            let rows = rows.into_iter().enumerate().map(|(index, row)| {
                Row::new([
                    Cell::from(Text::from((index + 1).to_string()).alignment(Alignment::Right)),
                    Cell::from(to_bidi_string(&row.title)),
                    Cell::from(to_bidi_string(&row.artists)),
                    Cell::from(Text::from(row.listens).alignment(Alignment::Right)),
                    Cell::from(row.duration),
                ])
                .style(listenbrainz_row_style(theme, row.visual))
            });
            let table = Table::new(
                rows,
                [
                    Constraint::Length(number_width),
                    Constraint::Fill(4),
                    Constraint::Fill(3),
                    Constraint::Length(9),
                    Constraint::Length(8),
                ],
            )
            .header(
                Row::new([
                    Cell::from(Text::from("#").alignment(Alignment::Right)),
                    Cell::from("Title"),
                    Cell::from("Artists"),
                    Cell::from(Text::from("Listens").alignment(Alignment::Right)),
                    Cell::from("Duration"),
                ])
                .style(theme.workspace_table_header()),
            )
            .column_spacing(2)
            .row_highlight_style({
                if active {
                    theme.workspace_selection_active()
                } else {
                    theme.workspace_selection_inactive()
                }
            });
            (table, row_count)
        }
    }
}

fn context_track_table_state_mut(
    page: &mut PageState,
    pane: ContextTrackPane,
) -> Option<&mut TableState> {
    let PageState::Context {
        state: Some(state), ..
    } = page
    else {
        return None;
    };
    match state {
        ContextPageUIState::Artist {
            top_track_table,
            liked_track_table,
            ..
        } => Some(if pane == ContextTrackPane::ArtistLikedSongs {
            liked_track_table
        } else {
            top_track_table
        }),
        ContextPageUIState::Playlist { playlist_state } => Some(playlist_state.table_mut()),
        ContextPageUIState::Album { track_table, .. }
        | ContextPageUIState::Tracks { track_table, .. } => Some(track_table),
        ContextPageUIState::Show { .. } | ContextPageUIState::Failed { .. } => None,
    }
}

#[cfg(test)]
mod provider_table_tests {
    use super::{
        listenbrainz_album_fallback_rows, listenbrainz_album_fallback_table,
        listenbrainz_fallback_rows, listenbrainz_fallback_table, listenbrainz_row_style,
        release_date_visible, ListenBrainzAlbumFallbackBody, ListenBrainzFallbackBody,
        ListenBrainzRowVisual,
    };
    use crate::state::{
        listenbrainz_album_key, listenbrainz_recording_key, ListenBrainzAlbumResolution,
        ListenBrainzArtistEnrichment, ListenBrainzCollectionStatus, ListenBrainzPopularRecording,
        ListenBrainzRecordingResolution, ListenBrainzReleaseGroup, MemoryCaches,
        TTL_CACHE_DURATION,
    };
    use ratatui::{
        buffer::Buffer,
        layout::Rect,
        style::Modifier,
        widgets::{StatefulWidget, TableState},
    };

    #[test]
    fn secondary_date_columns_require_at_least_one_real_date() {
        assert!(!release_date_visible(["", "  "].into_iter()));
        assert!(release_date_visible(["", "2024-02-01"].into_iter()));
    }

    #[test]
    fn listenbrainz_rows_are_source_labelled_and_show_resolution_state() {
        let enrichment = ListenBrainzArtistEnrichment::Available {
            artist_mbid: "artist-mbid".to_owned(),
            artist_name: "Artist".to_owned(),
            recordings_status: ListenBrainzCollectionStatus::Available,
            recordings: vec![ListenBrainzPopularRecording {
                recording_mbid: "recording-mbid".to_owned(),
                name: "Track".to_owned(),
                total_listen_count: Some(12),
                total_user_count: Some(3),
            }],
            release_groups_status: ListenBrainzCollectionStatus::NotRequested,
            release_groups: Vec::new(),
        };
        let mut caches = MemoryCaches::new();

        let fallback =
            listenbrainz_fallback_rows(&enrichment, &caches.listenbrainz_recordings).unwrap();
        assert_eq!(fallback.title, "Popular on ListenBrainz");
        let ListenBrainzFallbackBody::Recordings(rows) = fallback.body else {
            panic!("available enrichment must project recording rows");
        };
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].title, "Track");
        assert_eq!(rows[0].artists, "Artist");
        assert_eq!(rows[0].listens, "12");
        assert_eq!(rows[0].duration, "");
        assert_eq!(rows[0].visual, ListenBrainzRowVisual::Unresolved);
        assert!(!rows[0].title.contains('|'));
        assert!(!rows[0].title.contains("Unresolved"));

        caches.listenbrainz_recordings.insert(
            listenbrainz_recording_key("recording-mbid"),
            ListenBrainzRecordingResolution::Resolving { request_id: 7 },
            *TTL_CACHE_DURATION,
        );
        let fallback =
            listenbrainz_fallback_rows(&enrichment, &caches.listenbrainz_recordings).unwrap();
        let ListenBrainzFallbackBody::Recordings(rows) = fallback.body else {
            panic!("available enrichment must project recording rows");
        };
        assert_eq!(rows[0].visual, ListenBrainzRowVisual::Resolving);
    }

    #[test]
    fn listenbrainz_empty_and_error_states_are_explicit() {
        let caches = MemoryCaches::new();
        for enrichment in [
            ListenBrainzArtistEnrichment::Pending,
            ListenBrainzArtistEnrichment::Loading { request_id: 9 },
        ] {
            let fallback =
                listenbrainz_fallback_rows(&enrichment, &caches.listenbrainz_recordings).unwrap();
            assert_eq!(
                fallback.body,
                ListenBrainzFallbackBody::Message("Loading ListenBrainz enrichment...".to_owned())
            );
        }
        assert_eq!(
            listenbrainz_fallback_rows(
                &ListenBrainzArtistEnrichment::NoArtistMatch,
                &caches.listenbrainz_recordings,
            )
            .unwrap()
            .body,
            ListenBrainzFallbackBody::Message("No MusicBrainz match for this artist".to_owned())
        );
        let empty_recordings = ListenBrainzArtistEnrichment::Available {
            artist_mbid: "artist-mbid".to_owned(),
            artist_name: "Artist".to_owned(),
            recordings_status: ListenBrainzCollectionStatus::Empty,
            recordings: Vec::new(),
            release_groups_status: ListenBrainzCollectionStatus::NotRequested,
            release_groups: Vec::new(),
        };
        assert_eq!(
            listenbrainz_fallback_rows(&empty_recordings, &caches.listenbrainz_recordings,)
                .unwrap()
                .body,
            ListenBrainzFallbackBody::Message(
                "No popular recordings found on ListenBrainz".to_owned()
            )
        );
        assert_eq!(
            listenbrainz_fallback_rows(
                &ListenBrainzArtistEnrichment::Unavailable,
                &caches.listenbrainz_recordings,
            )
            .unwrap()
            .body,
            ListenBrainzFallbackBody::Message("ListenBrainz enrichment is unavailable".to_owned())
        );
        assert!(listenbrainz_fallback_rows(
            &ListenBrainzArtistEnrichment::NotRequested,
            &caches.listenbrainz_recordings,
        )
        .is_none());
    }

    #[test]
    fn listenbrainz_resolution_state_uses_row_visuals_without_status_text() {
        let theme = crate::config::Theme::default();
        assert!(
            listenbrainz_row_style(&theme, ListenBrainzRowVisual::Unresolved)
                .add_modifier
                .contains(Modifier::DIM)
        );
        assert!(
            listenbrainz_row_style(&theme, ListenBrainzRowVisual::Resolving)
                .add_modifier
                .contains(Modifier::ITALIC)
        );
        assert_eq!(
            listenbrainz_row_style(&theme, ListenBrainzRowVisual::Resolved),
            ratatui::style::Style::default()
        );
        let unavailable =
            listenbrainz_row_style(&theme, ListenBrainzRowVisual::Unavailable).add_modifier;
        assert!(unavailable.contains(Modifier::DIM));
        assert!(unavailable.contains(Modifier::CROSSED_OUT));
    }

    #[test]
    fn listenbrainz_recordings_render_as_real_columns() {
        let enrichment = ListenBrainzArtistEnrichment::Available {
            artist_mbid: "artist-mbid".to_owned(),
            artist_name: "Artist".to_owned(),
            recordings_status: ListenBrainzCollectionStatus::Available,
            recordings: vec![ListenBrainzPopularRecording {
                recording_mbid: "recording-mbid".to_owned(),
                name: "Track".to_owned(),
                total_listen_count: Some(12),
                total_user_count: Some(3),
            }],
            release_groups_status: ListenBrainzCollectionStatus::NotRequested,
            release_groups: Vec::new(),
        };
        let caches = MemoryCaches::new();
        let fallback =
            listenbrainz_fallback_rows(&enrichment, &caches.listenbrainz_recordings).unwrap();
        let (table, len) =
            listenbrainz_fallback_table(fallback, &crate::config::Theme::default(), true);
        assert_eq!(len, 1);
        let area = Rect::new(0, 0, 72, 4);
        let mut buffer = Buffer::empty(area);
        let mut state = TableState::default().with_selected(0);

        StatefulWidget::render(table, area, &mut buffer, &mut state);

        let rendered = buffer
            .content
            .iter()
            .map(|cell| cell.symbol())
            .collect::<String>();
        for label in ["#", "Title", "Artists", "Listens", "Duration", "Track"] {
            assert!(rendered.contains(label), "missing rendered label {label}");
        }
        assert!(!rendered.contains('|'));
        assert!(!rendered.contains("Unresolved"));
    }

    #[test]
    fn listenbrainz_album_fallback_renders_metadata_and_resolution_state() {
        let enrichment = ListenBrainzArtistEnrichment::Available {
            artist_mbid: "artist-mbid".to_owned(),
            artist_name: "Artist".to_owned(),
            recordings_status: ListenBrainzCollectionStatus::NotRequested,
            recordings: Vec::new(),
            release_groups_status: ListenBrainzCollectionStatus::Available,
            release_groups: vec![ListenBrainzReleaseGroup {
                release_group_mbid: "release-group-mbid".to_owned(),
                name: "The Album".to_owned(),
                release_date: Some("1994-03-08".to_owned()),
                release_type: Some("Album".to_owned()),
                total_listen_count: Some(42),
                total_user_count: Some(7),
            }],
        };
        let mut caches = MemoryCaches::new();
        let fallback =
            listenbrainz_album_fallback_rows(&enrichment, &caches.listenbrainz_albums).unwrap();
        assert_eq!(fallback.title, "Albums on ListenBrainz");
        let ListenBrainzAlbumFallbackBody::ReleaseGroups(rows) = &fallback.body else {
            panic!("available release groups must project album rows");
        };
        assert_eq!(rows[0].name, "The Album");
        assert_eq!(rows[0].release_date, "1994-03-08");
        assert_eq!(rows[0].release_type, "Album");
        assert_eq!(rows[0].listens, "42");
        assert_eq!(rows[0].visual, ListenBrainzRowVisual::Unresolved);

        caches.listenbrainz_albums.insert(
            listenbrainz_album_key("release-group-mbid"),
            ListenBrainzAlbumResolution::Resolving { request_id: 7 },
            *TTL_CACHE_DURATION,
        );
        let resolving =
            listenbrainz_album_fallback_rows(&enrichment, &caches.listenbrainz_albums).unwrap();
        let ListenBrainzAlbumFallbackBody::ReleaseGroups(rows) = &resolving.body else {
            panic!("available release groups must project album rows");
        };
        assert_eq!(rows[0].visual, ListenBrainzRowVisual::Resolving);

        let (table, len) = listenbrainz_album_fallback_table(
            resolving,
            &crate::config::Theme::default(),
            false,
            true,
        );
        assert_eq!(len, 1);
        let area = Rect::new(0, 0, 72, 4);
        let mut buffer = Buffer::empty(area);
        let mut state = TableState::default();
        StatefulWidget::render(table, area, &mut buffer, &mut state);
        let rendered = buffer
            .content
            .iter()
            .map(|cell| cell.symbol())
            .collect::<String>();
        for label in ["Date", "Type", "Name", "Listens", "The Album"] {
            assert!(rendered.contains(label), "missing rendered label {label}");
        }
        assert!(!rendered.contains("spotify:"));
    }

    #[test]
    fn listenbrainz_album_fallback_has_explicit_lifecycle_messages() {
        let caches = MemoryCaches::new();
        for (enrichment, expected) in [
            (
                ListenBrainzArtistEnrichment::Pending,
                "Loading ListenBrainz albums...",
            ),
            (
                ListenBrainzArtistEnrichment::Available {
                    artist_mbid: "artist-mbid".to_owned(),
                    artist_name: "Artist".to_owned(),
                    recordings_status: ListenBrainzCollectionStatus::NotRequested,
                    recordings: Vec::new(),
                    release_groups_status: ListenBrainzCollectionStatus::Empty,
                    release_groups: Vec::new(),
                },
                "No release groups found on ListenBrainz",
            ),
            (
                ListenBrainzArtistEnrichment::Available {
                    artist_mbid: "artist-mbid".to_owned(),
                    artist_name: "Artist".to_owned(),
                    recordings_status: ListenBrainzCollectionStatus::NotRequested,
                    recordings: Vec::new(),
                    release_groups_status: ListenBrainzCollectionStatus::Unavailable,
                    release_groups: Vec::new(),
                },
                "ListenBrainz albums are unavailable",
            ),
        ] {
            let fallback =
                listenbrainz_album_fallback_rows(&enrichment, &caches.listenbrainz_albums).unwrap();
            assert_eq!(
                fallback.body,
                ListenBrainzAlbumFallbackBody::Message(expected.to_owned())
            );
        }
    }
}

#[cfg(test)]
mod settings_layout_tests {
    use super::*;
    use crate::ui::utils::to_bidi_string;

    #[test]
    fn settings_rows_project_dynamic_labels_and_values_for_bidi_text() {
        assert_eq!(
            settings_display_label("accounts.spotify.active"),
            to_bidi_string("Active Spotify account")
        );
        assert_eq!(
            settings_display_value("Mix שלום"),
            to_bidi_string("Mix שלום")
        );
    }

    #[test]
    fn settings_values_are_shown_without_toml_syntax() {
        assert_eq!(settings_display_value("\"speaker\""), "speaker");
        assert_eq!(settings_display_value("true"), "On");
        assert_eq!(settings_display_value("false"), "Off");
        assert_eq!(settings_display_value("320"), "320");
        assert_eq!(
            settings_display_value("[\"repeat\", \"shuffle\"]"),
            "repeat, shuffle"
        );
        assert_eq!(
            settings_display_value("\"\"\"\n{track}\n{album}\"\"\""),
            "{track} ⏎ {album}"
        );
    }

    #[test]
    fn settings_workspace_runtime_note_matches_restart_metadata() {
        assert_eq!(
            settings_workspace_runtime_note(true),
            "Saved preferences take effect after restart."
        );
        assert_eq!(
            settings_workspace_runtime_note(false),
            "Changes apply to the running UI."
        );
    }
}

#[cfg(test)]
mod context_marker_tests {
    use super::{
        journal_compact_columns, selected_marker, unified_spotify_queue_label, youtube_tracks_match,
    };
    use crate::config::CompactMetadataMode;
    use crate::state::{MediaKind, YouTubeTrack};

    #[test]
    fn marker_indices_follow_each_context_pane_visible_order() {
        let top_indices = vec![0, 2];
        let liked_indices = vec![1];
        assert!(selected_marker(&top_indices, 0));
        assert!(selected_marker(&top_indices, 2));
        assert!(!selected_marker(&top_indices, 1));
        assert!(selected_marker(&liked_indices, 1));
        assert!(!selected_marker(&liked_indices, 0));
    }

    #[test]
    fn journal_markers_follow_keyed_visible_order() {
        let selected_indices = vec![1, 3];
        assert!(!selected_marker(&selected_indices, 0));
        assert!(selected_marker(&selected_indices, 1));
        assert!(!selected_marker(&selected_indices, 2));
        assert!(selected_marker(&selected_indices, 3));
    }

    #[test]
    fn journal_compact_columns_follow_the_shared_metadata_profile() {
        assert_eq!(
            journal_compact_columns(true, CompactMetadataMode::Minimal),
            (false, false)
        );
        assert_eq!(
            journal_compact_columns(true, CompactMetadataMode::Balanced),
            (true, false)
        );
        assert_eq!(
            journal_compact_columns(true, CompactMetadataMode::Detailed),
            (true, true)
        );
        assert_eq!(
            journal_compact_columns(false, CompactMetadataMode::Detailed),
            (false, false)
        );
    }

    #[test]
    fn queue_and_unified_playlist_markers_follow_keyed_visible_order() {
        let queue_indices = vec![0, 2];
        let playlist_indices = vec![1];
        assert!(selected_marker(&queue_indices, 0));
        assert!(!selected_marker(&queue_indices, 1));
        assert!(selected_marker(&queue_indices, 2));
        assert!(!selected_marker(&playlist_indices, 0));
        assert!(selected_marker(&playlist_indices, 1));
    }

    #[test]
    fn unified_queue_uses_a_user_facing_fallback_for_spotify_media() {
        assert_eq!(
            unified_spotify_queue_label(MediaKind::Track),
            "Spotify track"
        );
        assert_eq!(
            unified_spotify_queue_label(MediaKind::Episode),
            "Spotify episode"
        );
        assert_eq!(
            unified_spotify_queue_label(MediaKind::Video),
            "Spotify video"
        );
    }

    #[test]
    fn youtube_playback_marker_requires_matching_media_kind() {
        let song = YouTubeTrack {
            id: "same".to_owned(),
            name: String::new(),
            artists: String::new(),
            album: None,
            duration: String::new(),
            explicit: false,
            thumbnail_url: None,
            is_video: false,
        };
        let video = YouTubeTrack {
            is_video: true,
            ..song.clone()
        };
        assert!(youtube_tracks_match(&song, &song));
        assert!(!youtube_tracks_match(&song, &video));
    }
}

#[cfg(test)]
mod unified_playlist_title_tests {
    use super::{
        unified_playlist_display_title, unified_playlist_library_label, unified_playlist_row_label,
        unified_playlist_source_label, unified_playlist_sync_pending,
        unified_playlist_sync_pending_for_scope, unified_playlist_title,
    };
    use crate::state::{
        MediaId, MediaKind, PlaylistLink, PlaylistSeedItem, Provider, UnifiedPlaylist,
    };
    use crate::ui::utils::to_bidi_string;

    #[test]
    fn linked_state_is_visible_without_breaking_narrow_titles() {
        assert_eq!(
            unified_playlist_title(
                "Mix",
                Some("Road trip"),
                Some(crate::state::PlaylistProjectionStatus::Clean),
                40,
            ),
            "Mix [YT: Road trip]"
        );
        let pending = unified_playlist_title(
            "Mix",
            Some("Road trip"),
            Some(crate::state::PlaylistProjectionStatus::Conflict),
            40,
        );
        assert_eq!(pending, "Mix [YT conflict: Road trip]");
        let title = unified_playlist_title(
            "A very long playlist name",
            Some("linked"),
            Some(crate::state::PlaylistProjectionStatus::Clean),
            16,
        );
        assert!(title.chars().count() <= 12);
        assert!(title.ends_with("..."));
    }

    #[test]
    fn row_labels_use_the_shared_bidi_projection() {
        let label = "Song שלום";
        assert_eq!(unified_playlist_row_label(label), to_bidi_string(label));
    }

    #[test]
    fn unresolved_rows_do_not_render_provider_ids_as_titles() {
        let item = PlaylistSeedItem {
            media_id: MediaId {
                provider: Provider::Spotify,
                kind: MediaKind::Track,
                raw_id: "recording-id".to_owned(),
            },
            title: "recording-id".to_owned(),
            artists: String::new(),
            album: None,
            duration_ms: None,
            explicit: None,
            provider_url: Some("spotify:track:recording-id".to_owned()),
            metadata_degraded: true,
            artwork_url: None,
            metadata_pending: false,
        };
        assert_eq!(
            unified_playlist_display_title(
                &item.title,
                &item.media_id.raw_id,
                item.provider_url.as_deref()
            ),
            "Unknown track"
        );
        assert_eq!(unified_playlist_source_label(&item), "Unresolved");
    }

    #[test]
    fn hydrated_rows_keep_provider_label_and_display_metadata() {
        let item = PlaylistSeedItem {
            media_id: MediaId {
                provider: Provider::Spotify,
                kind: MediaKind::Track,
                raw_id: "track-id".to_owned(),
            },
            title: "Song".to_owned(),
            artists: String::new(),
            album: None,
            duration_ms: None,
            explicit: None,
            provider_url: None,
            artwork_url: None,
            metadata_degraded: false,
            metadata_pending: false,
        };
        assert_eq!(
            unified_playlist_display_title(&item.title, &item.media_id.raw_id, None),
            "Song"
        );
        assert_eq!(unified_playlist_source_label(&item), "Spotify");
    }

    #[test]
    fn linked_sync_pending_state_never_treats_legacy_or_unknown_as_clean() {
        let playlist = UnifiedPlaylist {
            id: "local".to_owned(),
            name: "Mix".to_owned(),
            items: Vec::new(),
            updated_at: 0,
            next_entry_id: 1,
        };
        assert!(!unified_playlist_sync_pending(&playlist, None));

        let mut link = PlaylistLink {
            unified_playlist_id: playlist.id.clone(),
            youtube_playlist_id: Some("youtube".to_owned()),
            ..PlaylistLink::default()
        };
        assert!(unified_playlist_sync_pending(&playlist, Some(&link)));

        link.last_local_snapshot = Some(playlist.snapshot_hash());
        assert!(unified_playlist_sync_pending(&playlist, Some(&link)));

        link.last_local_snapshot = Some("stale".to_owned());
        assert!(unified_playlist_sync_pending(&playlist, Some(&link)));

        assert_eq!(
            unified_playlist_library_label(&playlist, Some(&link), "unknown", 0),
            "[Unified pending] Mix"
        );
        link.last_local_snapshot = Some(playlist.snapshot_hash());
        assert_eq!(
            unified_playlist_library_label(&playlist, Some(&link), "unknown", 0),
            "[Unified pending] Mix"
        );
    }

    #[test]
    fn account_scoped_clean_projection_can_render_clean() {
        let playlist = UnifiedPlaylist {
            id: "local".to_owned(),
            name: "Mix".to_owned(),
            items: Vec::new(),
            updated_at: 0,
            next_entry_id: 1,
        };
        let mut link = PlaylistLink {
            unified_playlist_id: playlist.id.clone(),
            youtube_playlist_id: Some("youtube".to_owned()),
            ..PlaylistLink::default()
        };
        link.upsert_projection(crate::state::PlaylistProjectionState {
            target: crate::state::PlaylistProjectionTarget {
                provider: crate::state::Provider::YouTubeMusic,
                account_id: "account-1".to_owned(),
                account_epoch: 1,
                playlist_id: "youtube".to_owned(),
            },
            local_revision: Some(playlist.snapshot_hash()),
            remote_revision: Some("remote".to_owned()),
            status: crate::state::PlaylistProjectionStatus::Clean,
            conflicts: Vec::new(),
            mappings: Vec::new(),
            acknowledged_operations: Vec::new(),
            acknowledged_intents: Vec::new(),
            recovery: None,
        });
        assert!(!unified_playlist_sync_pending_for_scope(
            &playlist,
            Some(&link),
            "account-1",
            1,
        ));
        assert_eq!(
            unified_playlist_library_label(&playlist, Some(&link), "account-1", 1),
            "[Unified] Mix"
        );
    }
}

#[cfg(test)]
mod context_title_tests {
    use super::wrapped_description_height;

    #[test]
    fn context_descriptions_allocate_rows_for_wrapped_copy() {
        assert_eq!(wrapped_description_height("short", 80), 1);
        assert!(
            wrapped_description_height(
                "A long playlist description should remain readable on a narrow terminal.",
                24
            ) > 1
        );
        assert!(wrapped_description_height(&"x".repeat(200), 24) <= 3);
    }
}

#[cfg(test)]
mod lyrics_title_tests {
    use super::{
        lyrics_line_text, lyrics_metadata_line, lyrics_page_title, wrapped_description_height,
    };
    use crate::ui::utils::to_bidi_string;

    #[test]
    fn track_identity_leads_lyrics_title_with_bounded_fallbacks() {
        assert_eq!(
            lyrics_page_title("  Blue  ", "  Nightcore  "),
            "Lyrics: Blue · Nightcore"
        );
        assert_eq!(lyrics_page_title("Blue", ""), "Lyrics: Blue");
        assert_eq!(lyrics_page_title("", "Nightcore"), "Lyrics");
    }

    #[test]
    fn lyrics_metadata_wraps_instead_of_clipping_the_source() {
        let metadata =
            lyrics_metadata_line("YouTube Music · a provider with a long display name", false);
        assert!(wrapped_description_height(&metadata, 32) > 1);
        assert!(wrapped_description_height(&metadata, 32) <= 3);
    }

    #[test]
    fn lyrics_metadata_names_the_source_and_follow_mode() {
        assert_eq!(
            lyrics_metadata_line("LRCLIB", true),
            "LRCLIB · following playback"
        );
        assert_eq!(
            lyrics_metadata_line("Lyrics.ovh", false),
            "Lyrics.ovh · scrolling manually"
        );
    }

    #[test]
    fn lyrics_title_uses_the_shared_bidi_projection() {
        assert_eq!(
            lyrics_page_title("Blue שלום", "Artist עולם"),
            format!(
                "Lyrics: {} · {}",
                to_bidi_string("Blue שלום"),
                to_bidi_string("Artist עולם")
            )
        );
    }

    #[test]
    fn lyrics_content_lines_use_the_shared_bidi_projection() {
        assert_eq!(lyrics_line_text("שלום world"), to_bidi_string("שלום world"));
        assert_eq!(lyrics_line_text("plain lyrics"), "plain lyrics");
    }
}

#[cfg(test)]
mod lyrics_render_tests {
    use super::{lyrics_playback_progress, project_visible_lyrics};
    use crate::{
        config::ActiveProvider,
        observability::UiDiagnosticEntry,
        state::{Lyrics, LyricsLines, SharedState, State, YouTubePlayback, YouTubeTrack},
    };
    use parking_lot::Mutex;
    use std::{collections::VecDeque, sync::Arc, time::Duration};

    #[test]
    fn synchronized_lyrics_use_the_page_provider_progress() {
        crate::ui::initialize_test_config();
        let ring = Arc::new(Mutex::new(VecDeque::<UiDiagnosticEntry>::new()));
        let (diagnostics, _runtime) = crate::observability::disabled(ring);
        let state: SharedState = Arc::new(State::new(false, diagnostics));
        state.player.write().playback = Some(rspotify::model::CurrentPlaybackContext {
            device: rspotify::model::Device {
                id: Some("device".to_owned()),
                is_active: true,
                is_private_session: false,
                is_restricted: false,
                name: "device".to_owned(),
                _type: rspotify::model::DeviceType::Computer,
                volume_percent: Some(70),
            },
            repeat_state: rspotify::model::RepeatState::Off,
            shuffle_state: false,
            context: None,
            timestamp: chrono::Utc::now(),
            progress: Some(chrono::Duration::seconds(40)),
            is_playing: false,
            item: None,
            currently_playing_type: rspotify::model::CurrentlyPlayingType::Track,
            actions: rspotify::model::Actions::default(),
        });
        state.player.write().youtube_playback = Some(YouTubePlayback {
            track: YouTubeTrack {
                id: "youtube-track".to_owned(),
                name: "YouTube track".to_owned(),
                artists: "Artist".to_owned(),
                album: None,
                duration: "3:00".to_owned(),
                explicit: false,
                thumbnail_url: None,
                is_video: false,
            },
            is_playing: false,
            progress: Duration::from_secs(2),
            volume: 70,
            mute_state: None,
            route: Default::default(),
        });

        assert_eq!(
            lyrics_playback_progress(&state, ActiveProvider::YouTubeMusic),
            Some(chrono::Duration::seconds(2))
        );
        assert_eq!(
            lyrics_playback_progress(&state, ActiveProvider::Spotify),
            Some(chrono::Duration::seconds(40))
        );
    }

    #[test]
    fn visible_lyrics_projection_bounds_plain_lines_to_the_viewport() {
        let lyrics = Lyrics {
            lines: LyricsLines::Plain((0..12).map(|index| format!("line {index}")).collect()),
            source: "test".to_owned(),
        };

        let projection =
            project_visible_lyrics(&lyrics, None, false, 7, 3, &crate::config::Theme::default())
                .expect("plain lyrics do not need playback progress");

        assert_eq!(projection.scroll_offset, 7);
        assert_eq!(projection.lines.len(), 3);
    }

    #[test]
    fn visible_lyrics_projection_follows_synced_playback_without_full_copy() {
        let lyrics = Lyrics {
            lines: LyricsLines::Synced(
                (0..12)
                    .map(|index| {
                        (
                            chrono::Duration::seconds(index as i64 * 10),
                            format!("line {index}"),
                        )
                    })
                    .collect(),
            ),
            source: "test".to_owned(),
        };

        let projection = project_visible_lyrics(
            &lyrics,
            Some(chrono::Duration::seconds(55)),
            true,
            0,
            3,
            &crate::config::Theme::default(),
        )
        .expect("synced lyrics have playback progress");

        assert_eq!(projection.scroll_offset, 5);
        assert_eq!(projection.lines.len(), 3);
    }
}

#[cfg(test)]
mod search_title_tests {
    use super::{
        workspace_search_group_from_items, workspace_search_group_is_visible,
        workspace_search_group_rects, workspace_search_item, workspace_search_loaded_count,
        workspace_search_visible_focus, WorkspaceSearchProjectionWindow,
    };
    use crate::ui::components::search::{workspace_search_group, WorkspaceSearchItem};

    use crate::{command::ProviderSearchPane, state::SearchFocusState};
    use ratatui::layout::Rect;

    #[test]
    fn loading_search_panel_shows_status_without_a_zero_loaded_count() {
        use ratatui::{backend::TestBackend, widgets::ListState, Terminal};
        let ui = crate::state::UIState::default();
        let group = workspace_search_group(
            SearchFocusState::Tracks,
            "Tracks",
            0,
            WorkspaceSearchProjectionWindow::default(),
            0,
            None,
            Vec::new(),
        );
        let mut terminal = Terminal::new(TestBackend::new(40, 8)).unwrap();
        terminal
            .draw(|frame| {
                super::render_workspace_search_panel(
                    frame,
                    &ui.theme,
                    frame.area(),
                    &group,
                    true,
                    crate::state::UiViewStatus::Loading,
                    &mut ListState::default(),
                    ui.presentation.focused_row_overflow,
                    0,
                    &mut Vec::new(),
                );
            })
            .unwrap();
        let text = terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .map(ratatui::buffer::Cell::symbol)
            .collect::<String>();
        assert!(text.contains("Searching…"));
        assert!(!text.contains("0 of 0 loaded"));
    }

    #[test]
    fn workspace_search_grid_matches_the_measured_six_panel_recipe() {
        assert_eq!(
            workspace_search_group_rects(Rect::new(29, 10, 149, 26), 6),
            vec![
                Rect::new(29, 10, 73, 8),
                Rect::new(105, 10, 73, 8),
                Rect::new(29, 19, 73, 8),
                Rect::new(105, 19, 73, 8),
                Rect::new(29, 28, 73, 8),
                Rect::new(105, 28, 73, 8),
            ]
        );
    }

    #[test]
    fn workspace_search_count_distinguishes_visible_rows_from_loaded_rows() {
        assert_eq!(workspace_search_loaded_count(10, 6), "6 of 10 loaded");
        assert_eq!(workspace_search_loaded_count(4, 6), "4 of 4 loaded");
    }

    #[test]
    fn workspace_search_projection_keeps_only_the_visible_compact_pane() {
        assert_eq!(
            workspace_search_visible_focus(
                false,
                Some(ProviderSearchPane::Albums),
                SearchFocusState::Tracks,
            ),
            Some(SearchFocusState::Albums)
        );
        assert_eq!(
            workspace_search_visible_focus(false, None, SearchFocusState::Input),
            Some(SearchFocusState::Tracks)
        );
        assert_eq!(
            workspace_search_visible_focus(true, None, SearchFocusState::Albums),
            None
        );
        assert!(!workspace_search_group_is_visible(
            Some(SearchFocusState::Albums),
            SearchFocusState::Tracks
        ));
        assert!(workspace_search_group_is_visible(
            Some(SearchFocusState::Albums),
            SearchFocusState::Albums
        ));
    }

    #[test]
    fn workspace_search_projection_keeps_global_row_identity() {
        let window = WorkspaceSearchProjectionWindow {
            offset: 4,
            viewport: 3,
            selected: Some(5),
        };
        let items = (4..7)
            .map(|index| WorkspaceSearchItem {
                title: format!("Item {index}"),
                metadata: String::new(),
            })
            .collect();
        let group = workspace_search_group(
            SearchFocusState::Tracks,
            "Tracks",
            25,
            window,
            8,
            Some(WorkspaceSearchItem {
                title: "Item 5".to_owned(),
                metadata: String::new(),
            }),
            items,
        );

        assert_eq!(group.item_offset, 4);
        assert_eq!(group.total_items, 8);
        assert_eq!(group.selected_index, Some(5));
        assert_eq!(
            group
                .items
                .iter()
                .map(|item| item.title.as_str())
                .collect::<Vec<_>>(),
            vec!["Item 4", "Item 5", "Item 6"]
        );
        assert_eq!(
            group.selected_item.as_ref().map(|item| item.title.as_str()),
            Some("Item 5")
        );
    }

    #[test]
    fn workspace_search_group_projects_selection_and_visible_rows_once() {
        let source = ["Item 0", "Item 1", "Item 2", "Item 3"];
        let group = workspace_search_group_from_items(
            SearchFocusState::Tracks,
            "Tracks",
            25,
            WorkspaceSearchProjectionWindow {
                offset: 1,
                viewport: 2,
                selected: Some(2),
            },
            Some(source.as_slice()),
            |item| workspace_search_item(*item, "metadata"),
        );

        assert_eq!(group.total_items, source.len());
        assert_eq!(group.item_offset, 1);
        assert_eq!(group.selected_index, Some(2));
        assert_eq!(
            group
                .items
                .iter()
                .map(|item| item.title.as_str())
                .collect::<Vec<_>>(),
            vec!["Item 1", "Item 2"]
        );
        assert_eq!(
            group.selected_item.as_ref().map(|item| item.title.as_str()),
            Some("Item 2")
        );
    }
}

#[cfg(test)]
mod library_projection_tests {
    use super::{
        append_workspace_row_window, render_workspace_collection_panel,
        render_workspace_collection_table, workspace_collection_visible_rows,
        workspace_filtered_rows, workspace_library_focus_is_visible, workspace_row_selection_style,
        workspace_row_window, CollectionTrackRow, WorkspaceCollectionRows, WorkspaceListRow,
    };
    use crate::state::{
        Album, Artist, LibraryFocusState, PopupState, Track, UIState, WorkspaceHit,
    };
    use ratatui::widgets::TableState;
    use ratatui::{backend::TestBackend, layout::Rect, widgets::ListState, Terminal};

    #[test]
    fn collection_selection_and_hits_exclude_the_scrollbar_in_every_theme_profile() {
        let configs = crate::ui::initialize_test_config();
        let rows = (0..100)
            .map(|index| CollectionTrackRow {
                title: format!("Track {index}"),
                artist: "Artist".to_owned(),
                duration: "3:21".to_owned(),
            })
            .collect::<Vec<_>>();
        for name in ["default", "carbonfox", "catppuccin", "catppuccin-light"] {
            let theme = configs
                .theme_config
                .find_theme(name)
                .expect("built-in theme");
            for (width, height) in [(80, 24), (120, 35), (180, 49), (140, 40)] {
                for full_profile in [false, true] {
                    let rect = Rect::new(0, 0, width, height);
                    let mut table = TableState::default().with_selected(Some(0));
                    let mut hits = Vec::new();
                    let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
                    terminal
                        .draw(|frame| {
                            frame.render_widget(
                                ratatui::widgets::Block::default().style(theme.workspace_base()),
                                rect,
                            );
                            render_workspace_collection_table(
                                frame,
                                &theme,
                                rect,
                                &rows,
                                0,
                                rows.len(),
                                Some(0),
                                true,
                                &[],
                                None,
                                &mut table,
                                "100 tracks shown",
                                theme.workspace_secondary_text(),
                                super::CollectionDetailColumn::ARTIST,
                                full_profile,
                                crate::config::FocusedRowOverflow::Truncate,
                                0,
                                &mut hits,
                            );
                        })
                        .unwrap();
                    let row = hits
                        .iter()
                        .find(|(_, hit)| *hit == WorkspaceHit::ContextRow(0))
                        .unwrap()
                        .0;
                    let gutter_x = rect.right() - 1;
                    assert_eq!(row.right(), gutter_x);
                    let buffer = terminal.backend().buffer();
                    assert_eq!(
                        buffer[(gutter_x - 1, row.y)].bg,
                        theme.workspace_selection_active().bg.unwrap()
                    );
                    assert_eq!(
                        buffer[(gutter_x, row.y)].bg,
                        theme
                            .workspace_base()
                            .bg
                            .unwrap_or(ratatui::style::Color::Reset),
                        "{name}/{width}x{height}"
                    );
                    assert!(matches!(buffer[(gutter_x, row.y)].symbol(), "│" | "┃"));
                }
            }
        }
    }

    #[test]
    fn settings_navigation_never_paints_outside_its_panel() {
        for (width, height) in [(80, 24), (120, 35), (180, 49), (140, 40)] {
            for nav_height in [7, 14, height - 10] {
                let nav = Rect::new(1, 1, 18, nav_height);
                let state = crate::state::TrackedMutex::new(UIState::default());
                let mut ui = state.lock();
                let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
                terminal
                    .draw(|frame| {
                        frame.render_widget(
                            ratatui::widgets::Paragraph::new(
                                (0..height)
                                    .map(|_| "#".repeat(usize::from(width)))
                                    .collect::<Vec<_>>()
                                    .join("\n"),
                            ),
                            frame.area(),
                        );
                        super::render_workspace_settings_rail(
                            frame,
                            &mut ui,
                            nav,
                            crate::state::page::SettingsCategory::Preferences,
                        );
                    })
                    .unwrap();
                let buffer = terminal.backend().buffer();
                for y in 0..height {
                    for x in 0..width {
                        if !nav.contains(ratatui::layout::Position::new(x, y)) {
                            assert_eq!(
                                buffer[(x, y)].symbol(),
                                "#",
                                "{width}x{height}, nav height {nav_height}: {x},{y}"
                            );
                        }
                    }
                }
            }
        }
    }

    #[test]
    fn compact_library_projection_keeps_only_the_focused_collection() {
        assert!(workspace_library_focus_is_visible(
            None,
            LibraryFocusState::Playlists
        ));
        assert!(!workspace_library_focus_is_visible(
            Some(LibraryFocusState::SavedAlbums),
            LibraryFocusState::Playlists
        ));
        assert!(workspace_library_focus_is_visible(
            Some(LibraryFocusState::SavedAlbums),
            LibraryFocusState::SavedAlbums
        ));
    }

    #[test]
    fn workspace_selection_helper_uses_semantic_active_and_inactive_roles() {
        let ui = UIState::default();
        assert_eq!(
            workspace_row_selection_style(&ui, true),
            ui.theme.workspace_selection_active()
        );
        assert_eq!(
            workspace_row_selection_style(&ui, false),
            ui.theme.workspace_selection_inactive()
        );
    }

    #[test]
    fn workspace_collection_projection_uses_the_canonical_visible_row_count() {
        assert_eq!(
            workspace_collection_visible_rows(Rect::new(35, 3, 110, 35), true),
            12
        );
        // Width no longer gates the table; the vertical profile decides the stride.
        assert_eq!(
            workspace_collection_visible_rows(Rect::new(0, 0, 60, 35), true),
            12
        );
        // Compact: heading row, table header, one row per track, status on the last row.
        assert_eq!(
            workspace_collection_visible_rows(Rect::new(0, 0, 60, 17), false),
            14
        );
        assert_eq!(
            workspace_collection_visible_rows(Rect::new(0, 0, 60, 2), false),
            0
        );
    }

    #[test]
    fn collection_columns_keep_the_canonical_table_at_110_cells() {
        let columns = super::CollectionTableColumns::new(Rect::new(35, 3, 110, 35), 12, 4);
        assert_eq!(columns.number, Some((40, 3)));
        assert_eq!(columns.title_x, 45);
        assert_eq!(columns.title_width, 57);
        assert_eq!(columns.artist, Some((104, 30)));
        assert_eq!(columns.time, Some((136, 5)));
    }

    #[test]
    fn collection_columns_follow_the_documented_shrink_order() {
        let layout = |width| super::CollectionTableColumns::new(Rect::new(0, 0, width, 20), 40, 4);

        // Wide panes give the title everything past the 30-cell artist column.
        let wide = layout(140);
        assert_eq!(wide.artist.map(|(_, width)| width), Some(30));
        assert_eq!(wide.title_width, 87);

        // Artist narrows before the title drops below 20 cells...
        let narrow = layout(60);
        assert_eq!(narrow.title_width, 20);
        assert_eq!(narrow.artist.map(|(_, width)| width), Some(17));

        // ...then disappears at 12, then the index goes, then time.
        assert_eq!(layout(55).artist.map(|(_, width)| width), Some(12));
        let no_artist = layout(54);
        assert_eq!(no_artist.artist, None);
        assert!(no_artist.number.is_some());
        assert_eq!(no_artist.title_width, 33);

        let no_index = layout(36);
        assert_eq!(no_index.number, None);
        assert!(no_index.time.is_some());
        assert_eq!(no_index.title_width, 20);

        let title_only = layout(24);
        assert_eq!(title_only.number, None);
        assert_eq!(title_only.time, None);
        assert_eq!(title_only.title_x, 5);
        assert_eq!(title_only.title_width, 15);
    }

    #[test]
    fn collection_columns_widen_for_long_durations_and_indices() {
        let columns = super::CollectionTableColumns::new(Rect::new(0, 0, 110, 20), 1200, 7);
        assert_eq!(columns.number, Some((5, 4)));
        assert_eq!(columns.time, Some((99, 7)));
        assert_eq!(columns.title_x, 11);
        assert_eq!(columns.artist.map(|(_, width)| width), Some(30));
        assert_eq!(columns.title_width, 54);
    }

    #[test]
    fn workspace_track_rows_share_the_provider_projection_shape() {
        let track = crate::state::YouTubeTrack {
            id: "track".to_owned(),
            name: "Title".to_owned(),
            artists: "Artist".to_owned(),
            album: Some("Album".to_owned()),
            duration: "3:21".to_owned(),
            explicit: false,
            thumbnail_url: None,
            is_video: false,
        };

        assert_eq!(
            CollectionTrackRow::from_youtube(&track),
            CollectionTrackRow {
                title: "Title".to_owned(),
                artist: "Artist".to_owned(),
                duration: "3:21".to_owned(),
            }
        );
    }

    #[test]
    fn spotify_track_rows_share_the_legacy_and_workspace_projection() {
        let track = Track {
            id: rspotify::model::TrackId::from_id("track0000000000000000000000000001")
                .unwrap()
                .into_static(),
            name: "Explicit title".to_owned(),
            artists: vec![Artist {
                id: rspotify::model::ArtistId::from_id("artist000000000000000000000000001")
                    .unwrap()
                    .into_static(),
                name: "Artist".to_owned(),
            }],
            album: Some(Album {
                id: rspotify::model::AlbumId::from_id("album0000000000000000000000000001")
                    .unwrap()
                    .into_static(),
                release_date: "2026".to_owned(),
                name: "Album".to_owned(),
                artists: Vec::new(),
                typ: None,
                added_at: 0,
            }),
            duration: std::time::Duration::from_secs(62),
            explicit: false,
            added_at: 0,
        };

        assert_eq!(
            CollectionTrackRow::from_spotify(&track),
            CollectionTrackRow {
                title: "Explicit title".to_owned(),
                artist: "Artist".to_owned(),
                duration: "1:02".to_owned(),
            }
        );
    }

    #[test]
    fn workspace_collection_table_keeps_source_identity_for_visible_rows() {
        let mut terminal = Terminal::new(TestBackend::new(120, 35)).unwrap();
        let theme = crate::config::Theme::default();
        let rows = vec![
            CollectionTrackRow {
                title: "Visible 08".to_owned(),
                artist: "Artist".to_owned(),
                duration: "1:00".to_owned(),
            },
            CollectionTrackRow {
                title: "Visible 09".to_owned(),
                artist: "Artist".to_owned(),
                duration: "2:00".to_owned(),
            },
        ];
        let mut table_state = TableState::default();
        table_state.select(Some(19));
        let mut hits = Vec::new();

        terminal
            .draw(|frame| {
                render_workspace_collection_table(
                    frame,
                    &theme,
                    Rect::new(0, 0, 120, 35),
                    &rows,
                    8,
                    20,
                    Some(19),
                    true,
                    &[],
                    None,
                    &mut table_state,
                    "20 tracks shown",
                    theme.workspace_secondary_text(),
                    super::CollectionDetailColumn::ARTIST,
                    true,
                    crate::config::FocusedRowOverflow::Truncate,
                    0,
                    &mut hits,
                );
            })
            .unwrap();

        assert_eq!(
            hits.iter().map(|(_, hit)| *hit).collect::<Vec<_>>(),
            vec![WorkspaceHit::ContextRow(8), WorkspaceHit::ContextRow(9)]
        );
    }

    #[test]
    fn workspace_library_filter_matches_typed_fields_without_reordering_rows() {
        let mut ui = UIState::default();
        ui.popup = Some(PopupState::Search {
            query: "blue sky".to_owned(),
        });
        let rows = vec![
            WorkspaceListRow {
                primary: "Blue".to_owned(),
                secondary: "Sky".to_owned(),
            },
            WorkspaceListRow {
                primary: "Blue".to_owned(),
                secondary: "Ocean".to_owned(),
            },
            WorkspaceListRow {
                primary: "Green".to_owned(),
                secondary: "Sky".to_owned(),
            },
        ];

        let filtered = workspace_filtered_rows(&ui, rows);
        assert_eq!(filtered.len(), 1);
        assert_eq!(filtered[0].primary, "Blue");
        assert_eq!(filtered[0].secondary, "Sky");
    }

    #[test]
    fn workspace_library_without_a_query_keeps_the_original_row_order() {
        let ui = UIState::default();
        let rows = vec![
            WorkspaceListRow {
                primary: "First".to_owned(),
                secondary: "A".to_owned(),
            },
            WorkspaceListRow {
                primary: "Second".to_owned(),
                secondary: "B".to_owned(),
            },
        ];

        let filtered = workspace_filtered_rows(&ui, rows);
        assert_eq!(
            filtered
                .iter()
                .map(|row| (row.primary.as_str(), row.secondary.as_str()))
                .collect::<Vec<_>>(),
            vec![("First", "A"), ("Second", "B")]
        );
    }

    #[test]
    fn workspace_library_window_keeps_global_indices_for_visible_rows() {
        let (start, end) = workspace_row_window(100, 21, Some(22), 3);
        assert_eq!((start, end), (21, 24));

        let mut rows = Vec::new();
        let mut skip = start;
        let mut remaining = end - start;
        append_workspace_row_window(&mut rows, 0..100, &mut skip, &mut remaining, |index| {
            WorkspaceListRow {
                primary: format!("Row {index}"),
                secondary: String::new(),
            }
        });
        let projection = super::WorkspaceCollectionRows {
            rows,
            total: 100,
            offset: start,
        };

        assert!(projection.row_at(20).is_none());
        assert_eq!(
            projection.row_at(21).map(|row| row.primary.as_str()),
            Some("Row 21")
        );
        assert_eq!(
            projection.row_at(23).map(|row| row.primary.as_str()),
            Some("Row 23")
        );
        assert!(projection.row_at(24).is_none());
    }

    #[test]
    fn library_window_follows_a_selection_that_moved_past_the_viewport() {
        // The stored offset (21) is from the previous frame; the selection has
        // since moved to 25, below the old three-row window.
        assert_eq!(workspace_row_window(100, 21, Some(25), 3), (23, 26));
        assert_eq!(workspace_row_window(100, 21, Some(4), 3), (4, 7));
        assert_eq!(workspace_row_window(100, 21, Some(99), 3), (97, 100));
    }

    #[test]
    fn library_panel_draws_a_newly_selected_row_in_the_same_frame() {
        let mut terminal = Terminal::new(TestBackend::new(40, 5)).unwrap();
        let theme = crate::config::Theme::default();
        let mut state = ListState::default().with_selected(Some(25));
        *state.offset_mut() = 21;
        let (start, end) = workspace_row_window(100, state.offset(), state.selected(), 3);
        let mut rows = Vec::new();
        let mut skip = start;
        let mut remaining = end - start;
        append_workspace_row_window(&mut rows, 0..100, &mut skip, &mut remaining, |index| {
            WorkspaceListRow {
                primary: format!("Row {index}"),
                secondary: String::new(),
            }
        });
        let projection = super::WorkspaceCollectionRows {
            rows,
            total: 100,
            offset: start,
        };

        terminal
            .draw(|frame| {
                render_workspace_collection_panel(
                    frame,
                    &theme,
                    Rect::new(0, 0, 40, 5),
                    "Playlists",
                    &projection,
                    true,
                    0,
                    &mut state,
                    crate::state::LibraryFocusState::Playlists,
                    crate::config::FocusedRowOverflow::Truncate,
                    0,
                    &mut Vec::new(),
                );
            })
            .unwrap();

        let text = terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .map(ratatui::buffer::Cell::symbol)
            .collect::<String>();
        for row in ["Row 23", "Row 24", "Row 25"] {
            assert!(text.contains(row), "{row} missing from the first frame");
        }
    }

    #[test]
    fn focused_library_row_scrolls_only_in_the_active_panel() {
        let theme = crate::config::Theme::default();
        let long = "A playlist name far too long for this narrow panel";
        let rows = (0..3)
            .map(|index| WorkspaceListRow {
                primary: format!("{index} {long}"),
                secondary: String::new(),
            })
            .collect::<Vec<_>>();
        let projection = WorkspaceCollectionRows::from_owned(rows);
        let draw = |active: bool| {
            let mut terminal = Terminal::new(TestBackend::new(30, 5)).unwrap();
            let mut state = ListState::default().with_selected(Some(1));
            terminal
                .draw(|frame| {
                    render_workspace_collection_panel(
                        frame,
                        &theme,
                        Rect::new(0, 0, 30, 5),
                        "Playlists",
                        &projection,
                        active,
                        0,
                        &mut state,
                        crate::state::LibraryFocusState::Playlists,
                        crate::config::FocusedRowOverflow::Marquee,
                        10,
                        &mut Vec::new(),
                    );
                })
                .unwrap();
            let buffer = terminal.backend().buffer().clone();
            (0..5)
                .map(|y| (0..30).map(|x| buffer[(x, y)].symbol()).collect::<String>())
                .collect::<Vec<_>>()
        };

        let active = draw(true);
        // Rows start below the heading and rule; row 1 is the selection.
        assert!(active[2].contains("0 A playlist"), "{active:?}");
        assert!(active[2].contains("..."), "unfocused rows truncate");
        assert!(
            !active[3].contains("1 A playlist"),
            "the focused row scrolled"
        );
        assert!(active[3].contains("name far"), "{active:?}");

        let inactive = draw(false);
        assert!(
            inactive[3].contains("1 A playlist"),
            "inactive panels do not scroll"
        );
    }

    #[test]
    fn manual_library_row_stays_still_until_scrolled_and_reports_its_extent() {
        let theme = crate::config::Theme::default();
        let projection = WorkspaceCollectionRows::from_owned(vec![WorkspaceListRow {
            primary: "A playlist name far too long for this narrow panel".to_owned(),
            secondary: String::new(),
        }]);
        let draw = |offset: usize| {
            let mut terminal = Terminal::new(TestBackend::new(30, 5)).unwrap();
            let mut state = ListState::default().with_selected(Some(0));
            let _ = crate::ui::utils::take_manual_scroll_extent();
            terminal
                .draw(|frame| {
                    render_workspace_collection_panel(
                        frame,
                        &theme,
                        Rect::new(0, 0, 30, 5),
                        "Playlists",
                        &projection,
                        true,
                        0,
                        &mut state,
                        crate::state::LibraryFocusState::Playlists,
                        crate::config::FocusedRowOverflow::Manual,
                        offset,
                        &mut Vec::new(),
                    );
                })
                .unwrap();
            let buffer = terminal.backend().buffer().clone();
            let row = (0..30).map(|x| buffer[(x, 2)].symbol()).collect::<String>();
            (row, crate::ui::utils::take_manual_scroll_extent())
        };

        let (still, extent) = draw(0);
        assert!(
            still.contains("A playlist") && still.contains("..."),
            "{still:?}"
        );
        assert!(extent > 0);
        let (scrolled, _) = draw(11);
        assert!(scrolled.contains("name far"), "{scrolled:?}");
        assert!(!scrolled.contains("A playlist"));
    }

    #[test]
    fn wide_library_rows_keep_the_reference_content_anchors() {
        let mut terminal = Terminal::new(TestBackend::new(180, 49)).unwrap();
        let theme = crate::config::Theme::default();
        let playlist_rows = vec![WorkspaceListRow {
            primary: "Playlist 01".to_owned(),
            secondary: "Listener".to_owned(),
        }];
        let album_rows = vec![WorkspaceListRow {
            primary: "Album 01".to_owned(),
            secondary: "Artist 01 · 2026".to_owned(),
        }];
        let artist_rows = vec![WorkspaceListRow {
            primary: "Artist 01".to_owned(),
            secondary: String::new(),
        }];
        let playlist_projection = WorkspaceCollectionRows::from_owned(playlist_rows);
        let album_projection = WorkspaceCollectionRows::from_owned(album_rows);
        let artist_projection = WorkspaceCollectionRows::from_owned(artist_rows);
        let mut playlist_state = ListState::default().with_selected(Some(0));
        let mut album_state = ListState::default().with_selected(Some(0));
        let mut artist_state = ListState::default().with_selected(Some(0));
        let mut hits = Vec::new();

        terminal
            .draw(|frame| {
                render_workspace_collection_panel(
                    frame,
                    &theme,
                    Rect::new(29, 7, 58, 29),
                    "Playlists",
                    &playlist_projection,
                    true,
                    14,
                    &mut playlist_state,
                    LibraryFocusState::Playlists,
                    crate::config::FocusedRowOverflow::Truncate,
                    0,
                    &mut hits,
                );
                render_workspace_collection_panel(
                    frame,
                    &theme,
                    Rect::new(88, 7, 57, 29),
                    "Albums",
                    &album_projection,
                    false,
                    23,
                    &mut album_state,
                    LibraryFocusState::SavedAlbums,
                    crate::config::FocusedRowOverflow::Truncate,
                    0,
                    &mut hits,
                );
                render_workspace_collection_panel(
                    frame,
                    &theme,
                    Rect::new(146, 7, 32, 29),
                    "Artists",
                    &artist_projection,
                    false,
                    0,
                    &mut artist_state,
                    LibraryFocusState::FollowedArtists,
                    crate::config::FocusedRowOverflow::Truncate,
                    0,
                    &mut hits,
                );
            })
            .unwrap();

        let buffer = terminal.backend().buffer();
        assert_eq!(buffer[(31, 9)].symbol(), "P");
        assert_eq!(buffer[(71, 9)].symbol(), "L");
        assert_eq!(buffer[(86, 9)].symbol(), " ");
        assert_eq!(buffer[(90, 9)].symbol(), "A");
        assert_eq!(buffer[(120, 9)].symbol(), "A");
        assert_eq!(buffer[(144, 9)].symbol(), " ");
        assert_eq!(buffer[(148, 9)].symbol(), "A");
        assert_eq!(buffer[(177, 9)].symbol(), " ");
        assert_eq!(
            buffer[(30, 9)].bg,
            ratatui::style::Color::Rgb(245, 177, 131)
        );
        assert_eq!(
            buffer[(85, 9)].bg,
            ratatui::style::Color::Rgb(245, 177, 131)
        );
        assert_eq!(hits.len(), 3);
        assert_eq!(hits[0].0, Rect::new(30, 9, 56, 1));

        let overflow_rows = (1..=41)
            .map(|index| WorkspaceListRow {
                primary: format!("Playlist {index:02}"),
                secondary: "Listener".to_owned(),
            })
            .collect::<Vec<_>>();
        let overflow_projection = WorkspaceCollectionRows::from_owned(overflow_rows);
        let mut overflow_state = ListState::default().with_selected(Some(0));
        let mut overflow_hits = Vec::new();
        let mut overflow_terminal = Terminal::new(TestBackend::new(180, 49)).unwrap();
        overflow_terminal
            .draw(|frame| {
                render_workspace_collection_panel(
                    frame,
                    &theme,
                    Rect::new(29, 7, 58, 29),
                    "Playlists",
                    &overflow_projection,
                    true,
                    14,
                    &mut overflow_state,
                    LibraryFocusState::Playlists,
                    crate::config::FocusedRowOverflow::Truncate,
                    0,
                    &mut overflow_hits,
                );
            })
            .unwrap();

        let overflow_buffer = overflow_terminal.backend().buffer();
        assert_eq!(overflow_buffer[(86, 9)].symbol(), "┃");
        assert_eq!(overflow_buffer[(86, 25)].symbol(), "┃");
        assert_eq!(overflow_buffer[(86, 26)].symbol(), "│");
        assert_eq!(
            overflow_buffer[(86, 9)].fg,
            ratatui::style::Color::Rgb(116, 116, 126)
        );
        assert_eq!(
            overflow_buffer[(86, 26)].fg,
            ratatui::style::Color::Rgb(48, 48, 52)
        );
    }
}

#[cfg(test)]
mod mutable_playlist_render_tests {
    use super::render_mutable_playlist_snapshot;
    use crate::{
        state::{
            ContextId, ContextPageType, ContextPageUIState, MediaId, MediaKind, PageState,
            PlaylistActionModel, PlaylistCapabilities, PlaylistEntryId, PlaylistEntrySnapshot,
            PlaylistRef, PlaylistSeedItem, PlaylistSnapshot, Provider, ProviderOccurrenceToken,
            UIState, UiViewStatus, YouTubeContextId, YouTubeContextPageUIState,
        },
        ui::LayoutPolicy,
    };
    use ratatui::{backend::TestBackend, Terminal};
    use rspotify::model::PlaylistId;

    fn pages() -> Vec<PageState> {
        vec![
            PageState::new_unified_playlist("unified"),
            PageState::YouTubeContext {
                id: YouTubeContextId::Playlist("youtube".to_owned()),
                context: None,
                state: YouTubeContextPageUIState::new(),
            },
            PageState::Context {
                id: Some(ContextId::Playlist(
                    PlaylistId::from_id("37i9dQZF1DXcBWIGoYBM5M").unwrap(),
                )),
                context_page_type: ContextPageType::Browsing(ContextId::Playlist(
                    PlaylistId::from_id("37i9dQZF1DXcBWIGoYBM5M").unwrap(),
                )),
                state: Some(ContextPageUIState::new_playlist()),
            },
        ]
    }

    fn snapshot() -> PlaylistSnapshot {
        PlaylistSnapshot {
            playlist: PlaylistRef::new(None, "playlist", 0),
            title: "Shared Playlist".to_owned(),
            revision: "revision".to_owned(),
            capabilities: PlaylistCapabilities::read_only("test"),
            actions: PlaylistActionModel::default(),
            entries: vec![PlaylistEntrySnapshot {
                occurrence: ProviderOccurrenceToken::Unified(PlaylistEntryId(1)),
                item: PlaylistSeedItem {
                    media_id: MediaId {
                        provider: Provider::Spotify,
                        kind: MediaKind::Track,
                        raw_id: "track".to_owned(),
                    },
                    title: "Shared Row".to_owned(),
                    artists: "Artist".to_owned(),
                    album: None,
                    duration_ms: Some(61_000),
                    explicit: None,
                    provider_url: None,
                    artwork_url: None,
                    metadata_degraded: false,
                    metadata_pending: false,
                },
                source_index: 0,
                item_actions: Vec::new(),
            }],
            status: UiViewStatus::Ready,
        }
    }

    #[test]
    fn three_playlist_pages_render_the_same_table_in_normal_and_narrow_layouts() {
        crate::ui::initialize_test_config();
        for (columns, rows) in [(80, 20), (30, 12)] {
            for page in pages() {
                let mut ui_state = UIState::default();
                ui_state.history.push(page);
                let mutex = crate::state::TrackedMutex::new(ui_state);
                let mut ui = mutex.lock();
                let policy = LayoutPolicy::from_size(columns, rows);
                ui.orientation = policy.orientation;
                ui.layout_mode = policy.mode;
                let mut terminal = Terminal::new(TestBackend::new(columns, rows)).unwrap();
                terminal
                    .draw(|frame| {
                        render_mutable_playlist_snapshot(
                            true,
                            frame,
                            &mut ui,
                            frame.area(),
                            None,
                            &snapshot(),
                        );
                    })
                    .unwrap();
                let rendered = (0..rows)
                    .map(|y| {
                        (0..columns)
                            .map(|x| terminal.backend().buffer()[(x, y)].symbol())
                            .collect::<String>()
                    })
                    .collect::<Vec<_>>()
                    .join("\n");
                assert!(rendered.contains("Shared"));
                assert_eq!(ui.current_page().selected_index(), Some(0));
            }
        }
    }

    #[test]
    fn three_playlist_pages_share_loading_empty_and_error_presentation() {
        crate::ui::initialize_test_config();
        let statuses = [
            UiViewStatus::Loading,
            UiViewStatus::Empty,
            UiViewStatus::Failed {
                code: "PLAYLIST_FAILED",
                message: "Playlist failed",
                next_action: "Retry",
            },
        ];
        for status in statuses {
            for page in pages() {
                let mut ui_state = UIState::default();
                ui_state.history.push(page);
                let mutex = crate::state::TrackedMutex::new(ui_state);
                let mut ui = mutex.lock();
                let mut terminal = Terminal::new(TestBackend::new(40, 10)).unwrap();
                let mut current = snapshot();
                current.entries.clear();
                current.status = status;
                terminal
                    .draw(|frame| {
                        render_mutable_playlist_snapshot(
                            true,
                            frame,
                            &mut ui,
                            frame.area(),
                            None,
                            &current,
                        );
                    })
                    .unwrap();
                let rendered = (0..10)
                    .map(|y| {
                        (0..40)
                            .map(|x| terminal.backend().buffer()[(x, y)].symbol())
                            .collect::<String>()
                    })
                    .collect::<Vec<_>>()
                    .join("\n");
                assert!(rendered.contains(status.display_message()));
            }
        }
    }
}
