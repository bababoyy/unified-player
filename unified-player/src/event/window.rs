use super::bulk_action::{
    current_spotify_epoch, dispatch_spotify_native_queue_tracks, spotify_bulk_action_menu,
};
use super::page::handle_navigation_command;
use super::*;
use crate::{
    client::dispatch_legacy_playlist,
    command::{
        construct_album_actions, construct_artist_actions, construct_playlist_actions,
        construct_show_actions,
    },
    state::{
        context_selected_or_cursor_indices, journal_selected_or_cursor_indices,
        playlist_seed_matches_filter, synchronize_context_track_uris, AppData, ContextTrackPane,
        ContextTrackSelection, Episode, ListenBrainzAlbumIntent, ListenBrainzAlbumPendingIntent,
        ListenBrainzAlbumResolution, ListenBrainzArtistEnrichment, ListenBrainzCollectionStatus,
        ListenBrainzPendingIntent, ListenBrainzPopularRecording, ListenBrainzRecordingIntent,
        ListenBrainzRecordingResolution, ListenBrainzReleaseGroup, MutablePlaylistController,
        MutableWindowState, PlaylistActionModel, PlaylistCapabilities, PlaylistSeedItem,
        PlaylistSnapshot, ScopedSelectionError, ScopedSelectionStatus, Show, UIStateGuard,
        LISTENBRAINZ_RESOLUTION_PENDING_TTL,
    },
};
use command::Action;
use rand::RngExt;
use std::collections::BTreeSet;

fn context_filter_query(ui: &UIStateGuard) -> Option<String> {
    match ui.popup.as_ref() {
        Some(PopupState::Search { query }) => Some(query.clone()),
        _ => None,
    }
}

fn spotify_playlist_visible_tracks<'a>(ui: &UIStateGuard, tracks: &'a [Track]) -> Vec<&'a Track> {
    let filter = context_filter_query(ui);
    tracks
        .iter()
        .filter(|track| {
            filter.as_deref().is_none_or(|query| {
                playlist_seed_matches_filter(&PlaylistSeedItem::from_spotify_track(track), query)
            })
        })
        .collect()
}

fn focused_context_track_pane(ui: &UIStateGuard) -> Option<ContextTrackPane> {
    match ui.current_page() {
        PageState::Context {
            state: Some(ContextPageUIState::Playlist { .. }),
            ..
        } => Some(ContextTrackPane::Playlist),
        PageState::Context {
            state: Some(ContextPageUIState::Album { .. }),
            ..
        } => Some(ContextTrackPane::Album),
        PageState::Context {
            state: Some(ContextPageUIState::Tracks { .. }),
            ..
        } => Some(ContextTrackPane::Tracks),
        PageState::Context {
            state:
                Some(ContextPageUIState::Artist {
                    focus: ArtistFocusState::TopTracks,
                    ..
                }),
            ..
        } => Some(ContextTrackPane::ArtistTopTracks),
        PageState::Context {
            state:
                Some(ContextPageUIState::Artist {
                    focus: ArtistFocusState::LikedSongs,
                    ..
                }),
            ..
        } => Some(ContextTrackPane::ArtistLikedSongs),
        _ => None,
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum CachedContextKind {
    Playlist,
    Album,
    Tracks,
    Artist,
    Show,
}

fn cached_context_kind(context: &Context) -> CachedContextKind {
    match context {
        Context::Playlist { .. } => CachedContextKind::Playlist,
        Context::Album { .. } => CachedContextKind::Album,
        Context::Tracks { .. } => CachedContextKind::Tracks,
        Context::Artist { .. } => CachedContextKind::Artist,
        Context::Show { .. } => CachedContextKind::Show,
    }
}

fn sortable_pane_matches_context(pane: ContextTrackPane, context_kind: CachedContextKind) -> bool {
    matches!(
        (pane, context_kind),
        (ContextTrackPane::Playlist, CachedContextKind::Playlist)
            | (ContextTrackPane::Album, CachedContextKind::Album)
            | (ContextTrackPane::Tracks, CachedContextKind::Tracks)
            | (ContextTrackPane::ArtistTopTracks, CachedContextKind::Artist)
    )
}

fn sortable_context_tracks_mut<'a>(
    data: &'a mut AppData,
    context_id: &ContextId,
    pane: ContextTrackPane,
) -> Option<&'a mut Vec<Track>> {
    let context = data.caches.context.get_mut(&context_id.uri())?;
    if !sortable_pane_matches_context(pane, cached_context_kind(context)) {
        return None;
    }
    match (pane, context) {
        (ContextTrackPane::Playlist, Context::Playlist { tracks, .. })
        | (ContextTrackPane::Album, Context::Album { tracks, .. })
        | (ContextTrackPane::Tracks, Context::Tracks { tracks, .. })
        | (
            ContextTrackPane::ArtistTopTracks,
            Context::Artist {
                top_tracks: tracks, ..
            },
        ) => Some(tracks),
        _ => None,
    }
}

fn borrowed_row_source_index<T>(row: &T, full_rows: &[T]) -> Option<usize> {
    full_rows
        .iter()
        .position(|candidate| std::ptr::eq(candidate, row))
}

fn context_source_index_from_selection(
    selection: &ContextTrackSelection,
    visible_index: usize,
    visible_len: usize,
    visible_row: &Track,
    full_rows: &[Track],
) -> Option<usize> {
    if visible_index >= visible_len {
        return None;
    }

    match selection.visible_to_full_index(visible_index) {
        Ok(source_index) if source_index < full_rows.len() => Some(source_index),
        Err(ScopedSelectionError::NoValidProjection)
            if selection.status() == ScopedSelectionStatus::Ambiguous =>
        {
            borrowed_row_source_index(visible_row, full_rows)
        }
        _ => None,
    }
}

fn context_source_index(
    ui: &UIStateGuard,
    pane: ContextTrackPane,
    visible_index: usize,
    visible_len: usize,
    visible_row: &Track,
    full_rows: &[Track],
) -> Option<usize> {
    if pane == ContextTrackPane::Playlist {
        return ui
            .current_page()
            .mutable_playlist_state()
            .and_then(|state| state.selection().visible_to_full_index(visible_index).ok())
            .filter(|index| *index < full_rows.len());
    }
    let selection = ui.current_page().context_track_selection(pane)?;
    context_source_index_from_selection(
        selection,
        visible_index,
        visible_len,
        visible_row,
        full_rows,
    )
}

fn playlist_reorder_indices(
    source_index: usize,
    full_len: usize,
    move_down: bool,
) -> Option<(usize, usize)> {
    if move_down {
        source_index
            .checked_add(1)
            .filter(|insert_index| *insert_index < full_len)
            .map(|insert_index| (insert_index, source_index))
    } else {
        source_index
            .checked_sub(1)
            .map(|insert_index| (insert_index, source_index))
    }
}

fn playlist_cursor_after_move(visible_index: usize, insert_index: usize, filtered: bool) -> usize {
    if filtered {
        visible_index
    } else {
        insert_index
    }
}

/// Refuse playlist deletion unless the current cache proves unique, current rows.
pub(super) fn playlist_delete_is_safe(
    data: &DataReadGuard,
    playlist_id: &PlaylistId<'static>,
    target_ids: Option<&[TrackId<'static>]>,
) -> bool {
    let Some(Context::Playlist { playlist, tracks }) = data.caches.context.get(&playlist_id.uri())
    else {
        return false;
    };
    if playlist.id.uri() != playlist_id.uri() {
        return false;
    }

    playlist_rows_are_safe_for_delete(tracks, target_ids)
}

fn playlist_rows_are_safe_for_delete(
    tracks: &[Track],
    target_ids: Option<&[TrackId<'static>]>,
) -> bool {
    let mut seen = BTreeSet::new();
    if tracks.iter().any(|track| !seen.insert(track.id.uri())) {
        return false;
    }
    target_ids.is_none_or(|ids| {
        ids.iter()
            .all(|target| tracks.iter().any(|track| track.id.uri() == target.uri()))
    })
}

fn spotify_playlist_snapshot(
    data: &DataReadGuard,
    context_uri: &str,
    tracks: &[Track],
    provider_epoch: u64,
) -> Option<PlaylistSnapshot> {
    let Context::Playlist { playlist, .. } = data.caches.context.get(context_uri)? else {
        return None;
    };
    let modifiable = crate::state::spotify_playlist_is_modifiable(
        playlist,
        data.user_data.user.as_ref().map(|user| &user.id),
    );
    Some(PlaylistSnapshot::from_spotify_playlist(
        playlist,
        tracks,
        provider_epoch,
        PlaylistCapabilities::spotify(modifiable),
        PlaylistActionModel::new(construct_playlist_actions(playlist, data)),
        |track| {
            let mut actions = command::construct_track_actions(track, data);
            if modifiable {
                actions.push(Action::DeleteFromPlaylist);
            }
            actions
        },
    ))
}

fn synchronize_spotify_playlist_projection(
    ui: &mut UIStateGuard,
    context_uri: &str,
    tracks: &[Track],
    data: &DataReadGuard,
) -> Option<(PlaylistSnapshot, crate::state::MutablePlaylistProjection)> {
    let provider_epoch = ui.provider_selection_epoch(config::ActiveProvider::Spotify);
    let snapshot = spotify_playlist_snapshot(data, context_uri, tracks, provider_epoch)?;
    let filter_query = context_filter_query(ui);
    let projection = ui
        .current_page_mut()
        .mutable_playlist_state_mut()
        .and_then(|state| {
            MutablePlaylistController::synchronize(state, &snapshot, filter_query.as_deref()).ok()
        })?;
    Some((snapshot, projection))
}

fn synchronize_context_track_pane(
    ui: &mut UIStateGuard,
    pane: ContextTrackPane,
    context_uri: &str,
    complete_tracks: &[Track],
    visible_tracks: &[&Track],
    data: &DataReadGuard,
) -> bool {
    let provider_epoch = ui.provider_selection_epoch(config::ActiveProvider::Spotify);
    let filter_query = context_filter_query(ui);
    if pane == ContextTrackPane::Playlist {
        return synchronize_spotify_playlist_projection(ui, context_uri, complete_tracks, data)
            .is_some();
    }
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
                visible_tracks.iter().map(|track| track.id.uri()),
            )
        });

    match result {
        Some(Ok(())) => true,
        Some(Err(error)) => {
            tracing::warn!(
                selection_error = ?error,
                "Context selection projection could not be synchronized"
            );
            false
        }
        None => false,
    }
}

fn selected_context_tracks_for_action(
    ui: &UIStateGuard,
    pane: ContextTrackPane,
    visible_tracks: &[&Track],
) -> Vec<Track> {
    let cursor = ui.current_page().diagnostic_selection().unwrap_or_default();
    if pane == ContextTrackPane::Playlist {
        let indices = ui
            .current_page()
            .mutable_playlist_state()
            .map(|state| {
                let selected = state.selection().selected_visible_indices();
                if selected.is_empty() {
                    state
                        .selection()
                        .selected_or_cursor_visible_indices(cursor)
                        .unwrap_or_default()
                } else {
                    selected
                }
            })
            .unwrap_or_default();
        return indices
            .into_iter()
            .filter_map(|index| visible_tracks.get(index).map(|track| (*track).clone()))
            .collect();
    }
    let indices = ui
        .current_page()
        .context_track_selection(pane)
        .and_then(|selection| context_selected_or_cursor_indices(selection, cursor).ok())
        .unwrap_or_default();
    indices
        .into_iter()
        .filter_map(|index| visible_tracks.get(index).map(|track| (*track).clone()))
        .collect()
}

fn handle_action_for_context_tracks(
    action: Action,
    complete_tracks: &[Track],
    pane: ContextTrackPane,
    context_uri: &str,
    data: &DataReadGuard,
    ui: &mut UIStateGuard,
    client_pub: &crate::client::ClientRequestSender,
    state: &SharedState,
) -> Result<bool> {
    let visible_tracks = if pane == ContextTrackPane::Playlist {
        spotify_playlist_visible_tracks(ui, complete_tracks)
    } else {
        ui.search_filtered_items(complete_tracks)
    };
    if !synchronize_context_track_pane(
        ui,
        pane,
        context_uri,
        complete_tracks,
        &visible_tracks,
        data,
    ) {
        return Ok(false);
    }
    let selected_tracks = selected_context_tracks_for_action(ui, pane, &visible_tracks);
    let context = match selected_tracks.as_slice() {
        [] => return Ok(false),
        [track] => ActionContext::Track(track.clone()),
        _ => ActionContext::Tracks(selected_tracks),
    };
    if action == Action::AddToListenLater {
        return match context {
            ActionContext::Tracks(tracks) => {
                super::handle_bulk_add_to_listen_later(tracks, state, ui)
            }
            other => handle_action_in_context(action, other, client_pub, data, ui),
        };
    }
    handle_action_in_context(action, context, client_pub, data, ui)
}

pub fn handle_action_for_focused_context_page(
    action: Action,
    client_pub: &crate::client::ClientRequestSender,
    ui: &mut UIStateGuard,
    state: &SharedState,
) -> Result<bool> {
    let PageState::Context { id: Some(id), .. } = ui.current_page() else {
        return Ok(false);
    };
    let context_uri = id.uri();

    if action == Action::AddToListenLater {
        if let Some(result) = handle_bulk_listen_later_for_context(state, ui, &context_uri)? {
            return Ok(result);
        }
    }

    let data = state.data.read();
    match data.caches.context.get(&context_uri) {
        Some(Context::Artist {
            artist,
            top_tracks,
            albums,
            related_artists,
            ..
        }) => {
            let PageState::Context {
                state: Some(ContextPageUIState::Artist { focus, .. }),
                ..
            } = ui.current_page()
            else {
                return Ok(false);
            };
            let focus = *focus;

            match focus {
                ArtistFocusState::Albums => handle_action_for_selected_item(
                    action,
                    &ui.search_filtered_items(albums),
                    &data,
                    ui,
                    client_pub,
                ),
                ArtistFocusState::RelatedArtists => handle_action_for_selected_item(
                    action,
                    &ui.search_filtered_items(related_artists),
                    &data,
                    ui,
                    client_pub,
                ),
                ArtistFocusState::TopTracks => handle_action_for_context_tracks(
                    action,
                    top_tracks,
                    ContextTrackPane::ArtistTopTracks,
                    &context_uri,
                    &data,
                    ui,
                    client_pub,
                    state,
                ),
                ArtistFocusState::LikedSongs => {
                    let liked = data.user_data.liked_tracks_by_artist(artist);
                    handle_action_for_context_tracks(
                        action,
                        &liked,
                        ContextTrackPane::ArtistLikedSongs,
                        &context_uri,
                        &data,
                        ui,
                        client_pub,
                        state,
                    )
                }
            }
        }
        Some(Context::Album { tracks, .. }) => handle_action_for_context_tracks(
            action,
            tracks,
            ContextTrackPane::Album,
            &context_uri,
            &data,
            ui,
            client_pub,
            state,
        ),
        Some(Context::Tracks { tracks, .. }) => handle_action_for_context_tracks(
            action,
            tracks,
            ContextTrackPane::Tracks,
            &context_uri,
            &data,
            ui,
            client_pub,
            state,
        ),
        Some(Context::Playlist { tracks, .. }) => handle_action_for_context_tracks(
            action,
            tracks,
            ContextTrackPane::Playlist,
            &context_uri,
            &data,
            ui,
            client_pub,
            state,
        ),
        Some(Context::Show { episodes, .. }) => handle_action_for_selected_item(
            action,
            &ui.search_filtered_items(episodes),
            &data,
            ui,
            client_pub,
        ),
        None => Ok(false),
    }
}

fn handle_bulk_listen_later_for_context(
    state: &SharedState,
    ui: &mut UIStateGuard,
    context_uri: &str,
) -> Result<Option<bool>> {
    let data = state.data.read();
    let Some(context) = data.caches.context.get(context_uri) else {
        return Ok(Some(false));
    };
    let (tracks, pane) = match context {
        Context::Artist {
            artist, top_tracks, ..
        } => {
            let pane = match ui.current_page() {
                PageState::Context {
                    state: Some(ContextPageUIState::Artist { focus, .. }),
                    ..
                } => match focus {
                    ArtistFocusState::TopTracks => ContextTrackPane::ArtistTopTracks,
                    ArtistFocusState::LikedSongs => ContextTrackPane::ArtistLikedSongs,
                    _ => return Ok(None),
                },
                _ => return Ok(None),
            };
            let tracks = if pane == ContextTrackPane::ArtistTopTracks {
                top_tracks.clone()
            } else {
                data.user_data.liked_tracks_by_artist(artist)
            };
            (tracks, pane)
        }
        Context::Album { tracks, .. } => (tracks.clone(), ContextTrackPane::Album),
        Context::Tracks { tracks, .. } => (tracks.clone(), ContextTrackPane::Tracks),
        Context::Playlist { tracks, .. } => (tracks.clone(), ContextTrackPane::Playlist),
        Context::Show { .. } => return Ok(None),
    };
    let visible_tracks = if pane == ContextTrackPane::Playlist {
        spotify_playlist_visible_tracks(ui, &tracks)
    } else {
        ui.search_filtered_items(&tracks)
    };
    if !synchronize_context_track_pane(ui, pane, context_uri, &tracks, &visible_tracks, &data) {
        return Ok(Some(false));
    }
    let selected_tracks = selected_context_tracks_for_action(ui, pane, &visible_tracks);
    drop(data);
    if selected_tracks.len() > 1 {
        return Ok(Some(super::handle_bulk_add_to_listen_later(
            selected_tracks,
            state,
            ui,
        )?));
    }
    Ok(None)
}

pub fn handle_action_for_selected_item<T: Into<ActionContext> + Clone>(
    action: Action,
    items: &[&T],
    data: &DataReadGuard,
    ui: &mut UIStateGuard,
    client_pub: &crate::client::ClientRequestSender,
) -> Result<bool> {
    let id = ui.current_page().selected_index().unwrap_or_default();
    if id >= items.len() {
        return Ok(false);
    }

    handle_action_in_context(action, items[id].clone().into(), client_pub, data, ui)
}

/// Handle a command for the currently focused context window
///
/// The function will need to determine the focused window then
/// assign the handling job to the window's command handler
pub fn handle_command_for_focused_context_window(
    command: Command,
    client_pub: &crate::client::ClientRequestSender,
    ui: &mut UIStateGuard,
    state: &SharedState,
) -> Result<bool> {
    let context_id = match ui.current_page() {
        PageState::Context { id, .. } => match id {
            None => return Ok(false),
            Some(id) => id,
        },
        _ => anyhow::bail!("expect a context page"),
    };
    let context_uri = context_id.uri();

    // handle commands that require access to data's mutable state
    {
        let order = match command {
            Command::SortTrackByTitle => Some(TrackOrder::TrackName),
            Command::SortTrackByAlbum => Some(TrackOrder::Album),
            Command::SortTrackByArtists => Some(TrackOrder::Artists),
            Command::SortTrackByAddedDate => Some(TrackOrder::AddedAt),
            Command::SortTrackByDuration => Some(TrackOrder::Duration),
            _ => None,
        };

        // sort ordering commands
        if let Some(order) = order {
            let Some(pane) = focused_context_track_pane(ui) else {
                return Ok(false);
            };
            let mut data = state.data.write();
            let Some(tracks) = sortable_context_tracks_mut(&mut data, context_id, pane) else {
                return Ok(false);
            };
            tracks.sort_by(|x, y| order.compare(x, y));
            return Ok(true);
        }
        // reverse ordering command
        if command == Command::ReverseTrackOrder {
            let Some(pane) = focused_context_track_pane(ui) else {
                return Ok(false);
            };
            let mut data = state.data.write();
            let Some(tracks) = sortable_context_tracks_mut(&mut data, context_id, pane) else {
                return Ok(false);
            };
            tracks.reverse();
            return Ok(true);
        }
    }

    let listenbrainz_recordings = {
        let data = state.data.read();
        match data.caches.context.get(&context_uri) {
            Some(Context::Artist {
                top_tracks,
                listenbrainz:
                    ListenBrainzArtistEnrichment::Available {
                        recordings_status: ListenBrainzCollectionStatus::Available,
                        recordings,
                        ..
                    },
                ..
            }) if top_tracks.is_empty() => Some(recordings.clone()),
            _ => None,
        }
    };
    let listenbrainz_focused = matches!(
        ui.current_page(),
        PageState::Context {
            state: Some(ContextPageUIState::Artist {
                focus: ArtistFocusState::TopTracks,
                ..
            }),
            ..
        }
    );
    if listenbrainz_focused {
        if let Some(recordings) = listenbrainz_recordings {
            return handle_command_for_listenbrainz_recordings(
                command,
                client_pub,
                &context_uri,
                &recordings,
                ui,
                state,
            );
        }
    }

    let listenbrainz_release_groups = {
        let data = state.data.read();
        match data.caches.context.get(&context_uri) {
            Some(Context::Artist {
                albums,
                listenbrainz:
                    ListenBrainzArtistEnrichment::Available {
                        release_groups_status: ListenBrainzCollectionStatus::Available,
                        release_groups,
                        ..
                    },
                ..
            }) if albums.is_empty() => Some(release_groups.clone()),
            _ => None,
        }
    };
    let listenbrainz_albums_focused = matches!(
        ui.current_page(),
        PageState::Context {
            state: Some(ContextPageUIState::Artist {
                focus: ArtistFocusState::Albums,
                ..
            }),
            ..
        }
    );
    if listenbrainz_albums_focused {
        if let Some(release_groups) = listenbrainz_release_groups {
            return handle_command_for_listenbrainz_albums(
                command,
                client_pub,
                &context_uri,
                &release_groups,
                ui,
                state,
            );
        }
    }

    let data = state.data.read();

    match data.caches.context.get(&context_uri) {
        Some(context) => match context {
            Context::Artist {
                artist,
                top_tracks,
                albums,
                related_artists,
                ..
            } => {
                let PageState::Context {
                    state: Some(ContextPageUIState::Artist { focus, .. }),
                    ..
                } = ui.current_page()
                else {
                    anyhow::bail!("expect an arist context page with a state")
                };
                let focus = *focus;

                match focus {
                    ArtistFocusState::Albums => handle_command_for_album_list_window(
                        command,
                        &ui.search_filtered_items(albums),
                        &data,
                        ui,
                        client_pub,
                    ),
                    ArtistFocusState::RelatedArtists => Ok(handle_command_for_artist_list_window(
                        command,
                        &ui.search_filtered_items(related_artists),
                        &data,
                        ui,
                    )),
                    ArtistFocusState::TopTracks => handle_command_for_track_table_window(
                        command,
                        client_pub,
                        None,
                        Some(ContextTrackPane::ArtistTopTracks),
                        Some(context_uri.clone()),
                        top_tracks,
                        &data,
                        ui,
                        state,
                    ),
                    ArtistFocusState::LikedSongs => {
                        let liked = data.user_data.liked_tracks_by_artist(artist);
                        handle_command_for_track_table_window(
                            command,
                            client_pub,
                            None,
                            Some(ContextTrackPane::ArtistLikedSongs),
                            Some(context_uri.clone()),
                            &liked,
                            &data,
                            ui,
                            state,
                        )
                    }
                }
            }
            Context::Album { tracks, .. } => handle_command_for_track_table_window(
                command,
                client_pub,
                Some(context_id.clone()),
                Some(ContextTrackPane::Album),
                Some(context_uri.clone()),
                tracks,
                &data,
                ui,
                state,
            ),
            Context::Playlist { tracks, .. } => handle_command_for_track_table_window(
                command,
                client_pub,
                Some(context_id.clone()),
                Some(ContextTrackPane::Playlist),
                Some(context_uri.clone()),
                tracks,
                &data,
                ui,
                state,
            ),
            Context::Tracks { tracks, .. } => handle_command_for_track_table_window(
                command,
                client_pub,
                Some(context_id.clone()),
                Some(ContextTrackPane::Tracks),
                Some(context_uri.clone()),
                tracks,
                &data,
                ui,
                state,
            ),
            Context::Show { show, episodes } => handle_command_for_episode_table_window(
                command,
                client_pub,
                &show.id,
                &ui.search_filtered_items(episodes),
                &data,
                ui,
                state,
            ),
        },
        None => Ok(false),
    }
}

fn handle_command_for_listenbrainz_albums(
    command: Command,
    client_pub: &crate::client::ClientRequestSender,
    context_uri: &str,
    release_groups: &[ListenBrainzReleaseGroup],
    ui: &mut UIStateGuard,
    state: &SharedState,
) -> Result<bool> {
    if !config::get_config().app_config.listenbrainz.enabled
        || !config::get_config()
            .app_config
            .listenbrainz
            .artist_enrichment
        || release_groups.is_empty()
    {
        return Ok(false);
    }

    let id = ui
        .current_page()
        .selected_index()
        .unwrap_or_default()
        .min(release_groups.len() - 1);
    let count = ui.count_prefix;
    if navigate_and_clear_selection(command, ui, id, release_groups.len(), count) {
        if let PageState::Context {
            state:
                Some(ContextPageUIState::Artist {
                    listenbrainz_album_pending,
                    ..
                }),
            ..
        } = ui.current_page_mut()
        {
            *listenbrainz_album_pending = None;
        }
        return Ok(true);
    }

    let Some(intent) = listenbrainz_album_intent(command) else {
        return Ok(false);
    };
    let release_group = &release_groups[id];
    let key = crate::state::listenbrainz_album_key(&release_group.release_group_mbid);
    let resolution = state
        .data
        .read()
        .caches
        .listenbrainz_albums
        .get(&key)
        .cloned();
    match resolution {
        Some(ListenBrainzAlbumResolution::Resolved(album)) => {
            match intent {
                ListenBrainzAlbumIntent::OpenPage => {
                    ui.new_page(PageState::Context {
                        id: None,
                        context_page_type: ContextPageType::Browsing(ContextId::Album(album.id)),
                        state: None,
                    });
                }
                ListenBrainzAlbumIntent::OpenMenu => {
                    let actions = {
                        let data = state.data.read();
                        command::construct_album_actions(&album, &data)
                    };
                    ui.popup = Some(PopupState::ActionList(
                        Box::new(ActionListItem::Album(album, actions)),
                        ListState::default(),
                    ));
                }
            }
            Ok(true)
        }
        Some(
            ListenBrainzAlbumResolution::Resolving { .. }
            | ListenBrainzAlbumResolution::NoSpotifyRelation,
        ) => Ok(true),
        Some(ListenBrainzAlbumResolution::Unavailable) | None => {
            let request_id = rand::rng().random::<u64>();
            state.data.write().caches.listenbrainz_albums.insert(
                key.clone(),
                ListenBrainzAlbumResolution::Resolving { request_id },
                LISTENBRAINZ_RESOLUTION_PENDING_TTL,
            );
            if let PageState::Context {
                state:
                    Some(ContextPageUIState::Artist {
                        listenbrainz_album_pending,
                        ..
                    }),
                ..
            } = ui.current_page_mut()
            {
                *listenbrainz_album_pending = Some(ListenBrainzAlbumPendingIntent {
                    request_id,
                    context_uri: context_uri.to_owned(),
                    release_group_mbid: release_group.release_group_mbid.clone(),
                    intent,
                });
            }
            let request = ClientRequest::ResolveListenBrainzAlbum {
                request_id,
                context_uri: context_uri.to_owned(),
                release_group_mbid: release_group.release_group_mbid.clone(),
                intent,
            };
            if let Err(error) = client_pub.send(request) {
                state.data.write().caches.listenbrainz_albums.remove(&key);
                if let PageState::Context {
                    state:
                        Some(ContextPageUIState::Artist {
                            listenbrainz_album_pending,
                            ..
                        }),
                    ..
                } = ui.current_page_mut()
                {
                    *listenbrainz_album_pending = None;
                }
                return Err(error.into());
            }
            Ok(true)
        }
    }
}

fn listenbrainz_album_intent(command: Command) -> Option<ListenBrainzAlbumIntent> {
    match command {
        Command::ChooseSelected => Some(ListenBrainzAlbumIntent::OpenPage),
        Command::ShowActionsOnSelectedItem => Some(ListenBrainzAlbumIntent::OpenMenu),
        _ => None,
    }
}

fn handle_command_for_listenbrainz_recordings(
    command: Command,
    client_pub: &crate::client::ClientRequestSender,
    context_uri: &str,
    recordings: &[ListenBrainzPopularRecording],
    ui: &mut UIStateGuard,
    state: &SharedState,
) -> Result<bool> {
    if !config::get_config().app_config.listenbrainz.enabled
        || !config::get_config()
            .app_config
            .listenbrainz
            .artist_enrichment
    {
        return Ok(false);
    }
    if recordings.is_empty() {
        return Ok(false);
    }

    let mut id = ui
        .current_page()
        .selected_index()
        .unwrap_or_default()
        .min(recordings.len() - 1);
    let count = ui.count_prefix;
    if navigate_and_clear_selection(command, ui, id, recordings.len(), count) {
        if let PageState::Context {
            state:
                Some(ContextPageUIState::Artist {
                    listenbrainz_pending,
                    ..
                }),
            ..
        } = ui.current_page_mut()
        {
            *listenbrainz_pending = None;
        }
        return Ok(true);
    }

    let intent = match command {
        Command::ChooseSelected => ListenBrainzRecordingIntent::Play,
        Command::ShowActionsOnSelectedItem => ListenBrainzRecordingIntent::OpenMenu,
        Command::PlayRandom => {
            id = rand::rng().random_range(0..recordings.len());
            ui.current_page_mut().select(id);
            ListenBrainzRecordingIntent::Play
        }
        _ => return Ok(false),
    };
    let recording = &recordings[id];
    let key = crate::state::listenbrainz_recording_key(&recording.recording_mbid);
    let resolution = state
        .data
        .read()
        .caches
        .listenbrainz_recordings
        .get(&key)
        .cloned();
    match resolution {
        Some(ListenBrainzRecordingResolution::Resolved(track)) => {
            match intent {
                ListenBrainzRecordingIntent::Play => {
                    client_pub.send(ClientRequest::Player(PlayerRequest::StartPlayback(
                        Playback::URIs(vec![track.id.into()], None),
                        None,
                    )))?;
                }
                ListenBrainzRecordingIntent::OpenMenu => {
                    let actions = {
                        let data = state.data.read();
                        command::construct_track_actions(&track, &data)
                    };
                    ui.popup = Some(PopupState::ActionList(
                        Box::new(ActionListItem::Track(track, actions)),
                        ListState::default(),
                    ));
                }
            }
            Ok(true)
        }
        Some(
            ListenBrainzRecordingResolution::Resolving { .. }
            | ListenBrainzRecordingResolution::NoSpotifyRelation,
        ) => Ok(true),
        Some(ListenBrainzRecordingResolution::Unavailable) | None => {
            let request_id = rand::rng().random::<u64>();
            state.data.write().caches.listenbrainz_recordings.insert(
                key.clone(),
                ListenBrainzRecordingResolution::Resolving { request_id },
                LISTENBRAINZ_RESOLUTION_PENDING_TTL,
            );
            if let PageState::Context {
                state:
                    Some(ContextPageUIState::Artist {
                        listenbrainz_pending,
                        ..
                    }),
                ..
            } = ui.current_page_mut()
            {
                *listenbrainz_pending = Some(ListenBrainzPendingIntent {
                    request_id,
                    context_uri: context_uri.to_owned(),
                    recording_mbid: recording.recording_mbid.clone(),
                    intent,
                });
            }
            let request = ClientRequest::ResolveListenBrainzRecording {
                request_id,
                context_uri: context_uri.to_owned(),
                recording_mbid: recording.recording_mbid.clone(),
                intent,
            };
            if let Err(error) = client_pub.send(request) {
                state
                    .data
                    .write()
                    .caches
                    .listenbrainz_recordings
                    .remove(&key);
                if let PageState::Context {
                    state:
                        Some(ContextPageUIState::Artist {
                            listenbrainz_pending,
                            ..
                        }),
                    ..
                } = ui.current_page_mut()
                {
                    *listenbrainz_pending = None;
                }
                return Err(error.into());
            }
            Ok(true)
        }
    }
}

/// Handle commands that may modify a playlist
fn handle_playlist_modify_command(
    id: usize,
    playlist_id: &PlaylistId<'static>,
    command: Command,
    client_pub: &crate::client::ClientRequestSender,
    full_tracks: &[Track],
    visible_tracks: &[&Track],
    snapshot_id: Option<&str>,
    ui: &mut UIStateGuard,
) -> Result<bool> {
    match command {
        Command::MovePlaylistItemUp => {
            let Some(visible_row) = visible_tracks.get(id) else {
                return Ok(false);
            };
            let Some(source_index) = context_source_index(
                ui,
                ContextTrackPane::Playlist,
                id,
                visible_tracks.len(),
                visible_row,
                full_tracks,
            ) else {
                return Ok(false);
            };
            if let Some((insert_index, range_start)) =
                playlist_reorder_indices(source_index, full_tracks.len(), false)
            {
                let Some(request) = spotify_reorder_mutation_request(
                    playlist_id,
                    insert_index,
                    range_start,
                    snapshot_id,
                    crate::client::PlaylistMutationOperationId(rand::rng().random()),
                ) else {
                    return Ok(false);
                };
                dispatch_legacy_playlist(client_pub, request)?;
                let filtered = context_filter_query(ui).is_some();
                ui.current_page_mut().select(playlist_cursor_after_move(
                    id,
                    insert_index,
                    filtered,
                ));
            }
            return Ok(true);
        }
        Command::MovePlaylistItemDown => {
            let Some(visible_row) = visible_tracks.get(id) else {
                return Ok(false);
            };
            let Some(source_index) = context_source_index(
                ui,
                ContextTrackPane::Playlist,
                id,
                visible_tracks.len(),
                visible_row,
                full_tracks,
            ) else {
                return Ok(false);
            };
            if let Some((insert_index, range_start)) =
                playlist_reorder_indices(source_index, full_tracks.len(), true)
            {
                let Some(request) = spotify_reorder_mutation_request(
                    playlist_id,
                    insert_index,
                    range_start,
                    snapshot_id,
                    crate::client::PlaylistMutationOperationId(rand::rng().random()),
                ) else {
                    return Ok(false);
                };
                dispatch_legacy_playlist(client_pub, request)?;
                let filtered = context_filter_query(ui).is_some();
                ui.current_page_mut().select(playlist_cursor_after_move(
                    id,
                    insert_index,
                    filtered,
                ));
            }
            return Ok(true);
        }
        _ => {}
    }

    Ok(false)
}

fn spotify_reorder_mutation_request(
    playlist_id: &PlaylistId<'static>,
    insert_index: usize,
    range_start: usize,
    snapshot_id: Option<&str>,
    operation_id: crate::client::PlaylistMutationOperationId,
) -> Option<ClientRequest> {
    let snapshot_id = snapshot_id.filter(|revision| !revision.is_empty())?;
    let insert_before = if insert_index > range_start {
        insert_index.checked_add(1)?
    } else {
        insert_index
    };
    Some(ClientRequest::SpotifyPlaylistMutation(
        crate::client::SpotifyMutationIntent::Reorder {
            operation_id,
            playlist_id: playlist_id.uri(),
            range_start,
            insert_before,
            range_length: 1,
            snapshot_id: snapshot_id.to_owned(),
        },
    ))
}

fn handle_command_for_track_table_window(
    command: Command,
    client_pub: &crate::client::ClientRequestSender,
    context_id: Option<ContextId>,
    selection_pane: Option<ContextTrackPane>,
    selection_context_uri: Option<String>,
    tracks: &[Track],
    data: &DataReadGuard,
    ui: &mut UIStateGuard,
    state: &SharedState,
) -> Result<bool> {
    let id = ui.current_page().selected_index().unwrap_or_default();
    let filtered_tracks = if selection_pane == Some(ContextTrackPane::Playlist) {
        spotify_playlist_visible_tracks(ui, tracks)
    } else {
        ui.search_filtered_items(tracks)
    };
    let playlist_projection = match (selection_pane, selection_context_uri.as_deref()) {
        (Some(ContextTrackPane::Playlist), Some(context_uri)) => {
            synchronize_spotify_playlist_projection(ui, context_uri, tracks, data)
        }
        _ => None,
    };
    let context_selection_ready = match (selection_pane, selection_context_uri.as_deref()) {
        (Some(ContextTrackPane::Playlist), Some(_)) => playlist_projection.is_some(),
        (Some(pane), Some(context_uri)) => {
            synchronize_context_track_pane(ui, pane, context_uri, tracks, &filtered_tracks, data)
        }
        _ => false,
    };
    if selection_pane.is_some() && !context_selection_ready {
        return Ok(false);
    }
    if is_selection_command(command) {
        return Ok(handle_page_selection_command(command, ui, selection_pane));
    }
    let keyed_action = matches!(
        command,
        Command::ShowActionsOnSelectedItem | Command::AddSelectedItemToQueue
    );
    let has_keyed_selection = if selection_pane == Some(ContextTrackPane::Playlist) {
        ui.current_page()
            .mutable_playlist_state()
            .is_some_and(|state| state.selection().selected_len() > 0)
    } else {
        selection_pane
            .and_then(|pane| ui.current_page().context_track_selection(pane))
            .is_some_and(|selection| selection.selected_len() > 0)
    };
    if id >= filtered_tracks.len()
        && !(context_selection_ready && keyed_action && has_keyed_selection)
    {
        return Ok(false);
    }

    match command {
        Command::ExtendSelectionNext => {
            let offset = ui.count_prefix.unwrap_or(1);
            return Ok(extend_track_selection(
                ui,
                id,
                filtered_tracks.len(),
                offset,
                1,
            ));
        }
        Command::ExtendSelectionPrevious => {
            let offset = ui.count_prefix.unwrap_or(1);
            return Ok(extend_track_selection(
                ui,
                id,
                filtered_tracks.len(),
                offset,
                -1,
            ));
        }
        _ => {}
    }

    if let Some(ContextId::Playlist(ref playlist_id)) = context_id {
        let modifiable =
            data.user_data.modifiable_playlist_items(None).iter().any(
                |item| matches!(item, PlaylistFolderItem::Playlist(p) if p.id.eq(playlist_id)),
            );
        if modifiable
            && handle_playlist_modify_command(
                id,
                playlist_id,
                command,
                client_pub,
                tracks,
                &filtered_tracks,
                playlist_projection
                    .as_ref()
                    .map(|(snapshot, _)| snapshot.revision.as_str()),
                ui,
            )?
        {
            return Ok(true);
        }
    }

    let count = ui.count_prefix;
    if navigate_and_clear_selection(command, ui, id, filtered_tracks.len(), count) {
        return Ok(true);
    }

    match command {
        Command::PlayRandom | Command::ChooseSelected => {
            let uri = if command == Command::PlayRandom {
                tracks[rand::rng().random_range(0..tracks.len())].id.uri()
            } else {
                filtered_tracks[id].id.uri()
            };

            // Update currently_playing_tracks_id based on the context
            match context_id {
                Some(ContextId::Tracks(ref tracks_id)) => {
                    state.player.write().currently_playing_tracks_id = Some(tracks_id.clone());
                }
                _ => {
                    state.player.write().currently_playing_tracks_id = None;
                }
            }

            let base_playback = match context_id {
                None | Some(ContextId::Tracks(_)) => {
                    Playback::URIs(tracks.iter().map(|t| t.id.clone().into()).collect(), None)
                }
                Some(ContextId::Show(_)) => unreachable!(
                    "show context should be handled by handle_command_for_episode_table_window"
                ),
                Some(context_id) => Playback::Context(context_id, None),
            };

            client_pub.send(ClientRequest::Player(PlayerRequest::StartPlayback(
                base_playback
                    .uri_offset(uri, config::get_config().app_config.tracks_playback_limit),
                None,
            )))?;
        }
        Command::ShowActionsOnSelectedItem => {
            let menu_selection = playlist_projection
                .as_ref()
                .and_then(|(snapshot, projection)| {
                    ui.current_page()
                        .mutable_playlist_state()
                        .and_then(|playlist_state| {
                            MutablePlaylistController::menu_selection(
                                playlist_state,
                                snapshot,
                                projection,
                            )
                        })
                });
            let selected_tracks = menu_selection.as_ref().map_or_else(
                || selected_tracks_for_action(ui, &filtered_tracks),
                |selection| {
                    selection
                        .source_indices
                        .iter()
                        .filter_map(|index| tracks.get(*index).cloned())
                        .collect()
                },
            );
            let playlist_actions = menu_selection.map(|selection| selection.actions);
            if selected_tracks.len() > 1 {
                let actions = playlist_actions.unwrap_or_else(|| {
                    selection_pane.map_or_else(
                        || construct_tracks_actions(ui),
                        |_| construct_tracks_actions_for_context(ui, data, Some(&selected_tracks)),
                    )
                });
                let Ok(menu) = spotify_bulk_action_menu(
                    &selected_tracks,
                    &actions,
                    current_spotify_epoch(ui).value(),
                ) else {
                    return Ok(false);
                };
                ui.popup = Some(PopupState::ActionList(
                    Box::new(ActionListItem::Tracks(menu)),
                    ListState::default(),
                ));
            } else {
                let Some(selected_track) = selected_tracks
                    .first()
                    .cloned()
                    .or_else(|| filtered_tracks.get(id).map(|track| (*track).clone()))
                else {
                    return Ok(false);
                };
                let actions = playlist_actions
                    .unwrap_or_else(|| command::construct_track_actions(&selected_track, data));
                ui.popup = Some(PopupState::ActionList(
                    Box::new(ActionListItem::Track(selected_track, actions)),
                    ListState::default(),
                ));
            }
        }
        Command::AddSelectedItemToQueue => {
            let selected_tracks = selected_tracks_for_action(ui, &filtered_tracks);
            if selected_tracks.len() > 1 {
                let epoch = current_spotify_epoch(ui);
                dispatch_spotify_native_queue_tracks(ui, client_pub, selected_tracks, epoch)?;
                clear_track_selection(ui);
            } else {
                let Some(selected_track) = selected_tracks
                    .first()
                    .cloned()
                    .or_else(|| filtered_tracks.get(id).map(|track| (*track).clone()))
                else {
                    return Ok(false);
                };
                ui.spotify_queue_labels.remember_track(&selected_track);
                client_pub.send(ClientRequest::AddPlayableToQueue(selected_track.id.into()))?;
            }
        }
        Command::JumpToHighlightTrackInContext => {
            let Some(selected_track) = filtered_tracks.get(id) else {
                return Ok(false);
            };
            let Some(pane) = selection_pane else {
                return Ok(false);
            };
            let Some(location) =
                context_source_index(ui, pane, id, filtered_tracks.len(), selected_track, tracks)
            else {
                return Ok(false);
            };

            // Move selection and change the offset so selection is at the top
            let Some(MutableWindowState::Table(table)) =
                ui.current_page_mut().focus_window_state_mut()
            else {
                return Ok(false);
            };
            *table.offset_mut() = location;
            ui.popup = None;
            ui.current_page_mut().select(location);
        }
        _ => return Ok(false),
    }
    Ok(true)
}

pub(super) fn extend_track_selection(
    ui: &mut UIStateGuard,
    id: usize,
    len: usize,
    offset: usize,
    direction: isize,
) -> bool {
    if len == 0 {
        return false;
    }

    let next_id = if direction.is_negative() {
        id.saturating_sub(offset)
    } else {
        std::cmp::min(id.saturating_add(offset), len - 1)
    };

    let context_pane = focused_context_track_pane(ui);
    let changed = ui
        .current_page_mut()
        .selection_adapter_mut(context_pane)
        .is_some_and(|mut selection| selection.extend_visible_range(id, next_id).is_ok());
    if changed {
        ui.current_page_mut().select(next_id);
    }
    changed
}

pub(super) fn clear_track_selection(ui: &mut UIStateGuard) {
    let context_pane = focused_context_track_pane(ui);
    if let Some(mut selection) = ui.current_page_mut().selection_adapter_mut(context_pane) {
        clear_multi_selection(&mut selection);
    }
}

/// Apply ordinary cursor navigation and consistently leave multi-selection
/// mode. Shift-based extension commands bypass this helper and preserve the
/// selected keys while moving the anchor.
pub(super) fn navigate_and_clear_selection(
    command: Command,
    ui: &mut UIStateGuard,
    id: usize,
    len: usize,
    count: Option<usize>,
) -> bool {
    let handled = handle_navigation_command(command, ui.current_page_mut(), id, len, count);
    if handled && navigation_clears_selection(command) {
        clear_track_selection(ui);
    }
    handled
}

pub(super) const fn navigation_clears_selection(command: Command) -> bool {
    matches!(
        command,
        Command::SelectNextOrScrollDown
            | Command::SelectPreviousOrScrollUp
            | Command::PageSelectNextOrScrollDown
            | Command::PageSelectPreviousOrScrollUp
            | Command::SelectLastOrScrollToBottom
            | Command::SelectFirstOrScrollToTop
    )
}

fn clear_multi_selection<M: MultiSelectModel>(selection: &mut M) {
    let _ = selection.clear_selection();
}

pub(super) fn selected_tracks_for_action(ui: &UIStateGuard, tracks: &[&Track]) -> Vec<Track> {
    if is_search_keyed_pane(ui) {
        let cursor = ui.current_page().diagnostic_selection().unwrap_or_default();
        let indices = match ui.current_page() {
            PageState::Search { state, .. } => {
                let selected = state.search_selection.selected_visible_indices();
                if selected.is_empty() {
                    state
                        .search_selection
                        .selected_or_cursor(Some(cursor))
                        .unwrap_or_default()
                } else {
                    selected
                }
            }
            _ => Vec::new(),
        };
        return indices
            .into_iter()
            .filter_map(|index| tracks.get(index).map(|track| (*track).clone()))
            .collect();
    }

    if let Some(pane) = focused_context_track_pane(ui) {
        return selected_context_tracks_for_action(ui, pane, tracks);
    }

    if matches!(
        ui.current_page(),
        PageState::Journal { .. } | PageState::JournalList { .. }
    ) {
        let cursor = ui.current_page().diagnostic_selection().unwrap_or_default();
        let indices = match ui.current_page() {
            PageState::Journal {
                journal_selection, ..
            }
            | PageState::JournalList {
                journal_selection, ..
            } => {
                let selected = journal_selection.selected_visible_indices();
                if selected.is_empty() {
                    journal_selected_or_cursor_indices(journal_selection, cursor)
                        .unwrap_or_default()
                } else {
                    selected
                }
            }
            _ => Vec::new(),
        };
        return indices
            .into_iter()
            .filter_map(|index| tracks.get(index).map(|track| (*track).clone()))
            .collect();
    }

    Vec::new()
}

pub(super) fn handle_action_for_search_track_list(
    action: Action,
    tracks: &[&Track],
    data: &DataReadGuard,
    ui: &mut UIStateGuard,
    client_pub: &crate::client::ClientRequestSender,
    state: &SharedState,
) -> Result<bool> {
    let selected_tracks = selected_tracks_for_action(ui, tracks);
    let context = match selected_tracks.as_slice() {
        [] => return Ok(false),
        [track] => ActionContext::Track(track.clone()),
        _ => ActionContext::Tracks(selected_tracks),
    };
    if action == Action::AddToListenLater {
        return match context {
            ActionContext::Tracks(tracks) => {
                super::handle_bulk_add_to_listen_later(tracks, state, ui)
            }
            other => handle_action_in_context(action, other, client_pub, data, ui),
        };
    }
    handle_action_in_context(action, context, client_pub, data, ui)
}

fn is_search_keyed_pane(ui: &UIStateGuard) -> bool {
    matches!(
        ui.current_page(),
        PageState::Search {
            state: SearchPageUIState {
                focus: SearchFocusState::Tracks | SearchFocusState::Videos,
                ..
            },
            ..
        }
    )
}

pub(super) fn construct_tracks_actions(ui: &UIStateGuard) -> Vec<Action> {
    let mut actions = command::construct_tracks_actions();
    if is_liked_tracks_context(ui) {
        if let Some(action) = actions
            .iter_mut()
            .find(|action| **action == Action::AddToLiked)
        {
            *action = Action::DeleteFromLiked;
        }
    }
    actions
}

fn construct_tracks_actions_for_context(
    ui: &UIStateGuard,
    data: &DataReadGuard,
    selected_tracks: Option<&[Track]>,
) -> Vec<Action> {
    let mut actions = command::construct_tracks_actions();
    if is_liked_tracks_context(ui) {
        if let Some(action) = actions
            .iter_mut()
            .find(|action| **action == Action::AddToLiked)
        {
            *action = Action::DeleteFromLiked;
        }
    }
    if let PageState::Context {
        id: Some(ContextId::Playlist(playlist_id)),
        ..
    } = ui.current_page()
    {
        let selected_ids = selected_tracks.map(|tracks| {
            tracks
                .iter()
                .map(|track| track.id.clone())
                .collect::<Vec<_>>()
        });
        if playlist_delete_is_safe(data, playlist_id, selected_ids.as_deref()) {
            actions.push(Action::DeleteFromPlaylist);
        }
    }
    actions
}

fn is_liked_tracks_context(ui: &UIStateGuard) -> bool {
    matches!(
        ui.current_page(),
        PageState::Context {
            id: Some(ContextId::Tracks(tracks_id)),
            ..
        } if tracks_id == &*USER_LIKED_TRACKS_ID
    )
}

pub fn handle_command_for_track_list_window(
    command: Command,
    client_pub: &crate::client::ClientRequestSender,
    tracks: &[&Track],
    data: &DataReadGuard,
    ui: &mut UIStateGuard,
    state: &SharedState,
) -> Result<bool> {
    let id = ui.current_page().selected_index().unwrap_or_default();
    let search_keyed = is_search_keyed_pane(ui);
    let has_keyed_selection = matches!(
        ui.current_page(),
        PageState::Search { state, .. } if !state.search_selection.selected_indices().is_empty()
    );
    let keyed_action = matches!(
        command,
        Command::ShowActionsOnSelectedItem | Command::AddSelectedItemToQueue
    );
    if id >= tracks.len() && !(search_keyed && keyed_action && has_keyed_selection) {
        return Ok(false);
    }

    match command {
        Command::ExtendSelectionNext => {
            let offset = ui.count_prefix.unwrap_or(1);
            return Ok(extend_track_selection(ui, id, tracks.len(), offset, 1));
        }
        Command::ExtendSelectionPrevious => {
            let offset = ui.count_prefix.unwrap_or(1);
            return Ok(extend_track_selection(ui, id, tracks.len(), offset, -1));
        }
        _ => {}
    }

    let count = ui.count_prefix;
    if navigate_and_clear_selection(command, ui, id, tracks.len(), count) {
        return Ok(true);
    }
    match command {
        Command::ChooseSelected => {
            // for a track list, `ChooseSelected` on a track
            // will start a `URIs` playback containing only that track.
            // This is different from the track table, which handles
            // `ChooseSelected` by starting a `URIs` playback
            // containing all the tracks in the table.

            // Track lists are used for search results, so clear the Tracks context
            state.player.write().currently_playing_tracks_id = None;

            client_pub.send(ClientRequest::Player(PlayerRequest::StartPlayback(
                Playback::URIs(vec![tracks[id].id.clone().into()], None),
                None,
            )))?;
        }
        Command::ShowActionsOnSelectedItem => {
            let selected_tracks = selected_tracks_for_action(ui, tracks);
            if selected_tracks.len() > 1 {
                let actions = construct_tracks_actions(ui);
                let Ok(menu) = spotify_bulk_action_menu(
                    &selected_tracks,
                    &actions,
                    current_spotify_epoch(ui).value(),
                ) else {
                    return Ok(false);
                };
                ui.popup = Some(PopupState::ActionList(
                    Box::new(ActionListItem::Tracks(menu)),
                    ListState::default(),
                ));
            } else {
                let selected_track = if search_keyed {
                    selected_tracks
                        .first()
                        .cloned()
                        .unwrap_or_else(|| tracks[id].clone())
                } else {
                    tracks[id].clone()
                };
                let actions = command::construct_track_actions(&selected_track, data);
                ui.popup = Some(PopupState::ActionList(
                    Box::new(ActionListItem::Track(selected_track, actions)),
                    ListState::default(),
                ));
            }
        }
        Command::AddSelectedItemToQueue => {
            let selected_tracks = selected_tracks_for_action(ui, tracks);
            if selected_tracks.len() > 1 {
                let epoch = current_spotify_epoch(ui);
                dispatch_spotify_native_queue_tracks(ui, client_pub, selected_tracks, epoch)?;
                clear_track_selection(ui);
            } else {
                let selected_track = if search_keyed {
                    selected_tracks
                        .first()
                        .cloned()
                        .unwrap_or_else(|| tracks[id].clone())
                } else {
                    tracks[id].clone()
                };
                ui.spotify_queue_labels.remember_track(&selected_track);
                client_pub.send(ClientRequest::AddPlayableToQueue(selected_track.id.into()))?;
            }
        }
        _ => return Ok(false),
    }
    Ok(true)
}

pub fn handle_command_for_artist_list_window(
    command: Command,
    artists: &[&Artist],
    data: &DataReadGuard,
    ui: &mut UIStateGuard,
) -> bool {
    let id = ui.current_page().selected_index().unwrap_or_default();
    if id >= artists.len() {
        return false;
    }

    let count = ui.count_prefix;
    if handle_navigation_command(command, ui.current_page_mut(), id, artists.len(), count) {
        return true;
    }
    match command {
        Command::ChooseSelected => {
            let context_id = ContextId::Artist(artists[id].id.clone());
            ui.new_page(PageState::Context {
                id: None,
                context_page_type: ContextPageType::Browsing(context_id),
                state: None,
            });
        }
        Command::ShowActionsOnSelectedItem => {
            let actions = construct_artist_actions(artists[id], data);
            ui.popup = Some(PopupState::ActionList(
                Box::new(ActionListItem::Artist(artists[id].clone(), actions)),
                ListState::default(),
            ));
        }
        _ => return false,
    }
    true
}

pub fn handle_command_for_album_list_window(
    command: Command,
    albums: &[&Album],
    data: &DataReadGuard,
    ui: &mut UIStateGuard,
    client_pub: &crate::client::ClientRequestSender,
) -> Result<bool> {
    let id = ui.current_page().selected_index().unwrap_or_default();
    if id >= albums.len() {
        return Ok(false);
    }

    let count = ui.count_prefix;
    if handle_navigation_command(command, ui.current_page_mut(), id, albums.len(), count) {
        return Ok(true);
    }
    match command {
        Command::ChooseSelected => {
            let context_id = ContextId::Album(albums[id].id.clone());
            ui.new_page(PageState::Context {
                id: None,
                context_page_type: ContextPageType::Browsing(context_id),
                state: None,
            });
        }
        Command::ShowActionsOnSelectedItem => {
            let actions = construct_album_actions(albums[id], data);
            ui.popup = Some(PopupState::ActionList(
                Box::new(ActionListItem::Album(albums[id].clone(), actions)),
                ListState::default(),
            ));
        }
        Command::AddSelectedItemToQueue => {
            client_pub.send(ClientRequest::AddAlbumToQueue(albums[id].id.clone()))?;
        }
        _ => return Ok(false),
    }
    Ok(true)
}

pub fn handle_command_for_playlist_list_window(
    command: Command,
    playlists: &[&PlaylistFolderItem],
    data: &DataReadGuard,
    ui: &mut UIStateGuard,
) -> bool {
    let id = ui.current_page().selected_index().unwrap_or_default();
    if id >= playlists.len() {
        return false;
    }

    let count = ui.count_prefix;
    if handle_navigation_command(command, ui.current_page_mut(), id, playlists.len(), count) {
        return true;
    }
    match command {
        Command::ChooseSelected => {
            let playlist = playlists[id];
            match playlist {
                PlaylistFolderItem::Folder(f) => {
                    // currently folders are only supported in the library page
                    match ui.current_page_mut() {
                        PageState::Library { state } => {
                            state.playlist_list.select(Some(0));
                            state.focus = LibraryFocusState::Playlists;
                            state.playlist_folder_id = f.target_id;
                        }
                        _ => return false,
                    }
                }
                PlaylistFolderItem::Playlist(p) => {
                    let context_id = ContextId::Playlist(p.id.clone());
                    ui.new_page(PageState::Context {
                        id: None,
                        context_page_type: ContextPageType::Browsing(context_id),
                        state: None,
                    });
                }
            }
        }
        Command::ShowActionsOnSelectedItem => match playlists[id] {
            PlaylistFolderItem::Playlist(p) => {
                super::page::open_spotify_playlist_context_actions(p, data, ui);
            }
            PlaylistFolderItem::Folder(_) => {
                ui.set_unsupported_operation(
                    "Playlist folder actions are unavailable.",
                    "Press Enter to open the folder and browse its playlists.",
                );
            }
        },
        _ => return false,
    }
    true
}

pub fn handle_command_for_show_list_window(
    command: Command,
    shows: &[&Show],
    data: &DataReadGuard,
    ui: &mut UIStateGuard,
) -> bool {
    let id = ui.current_page().selected_index().unwrap_or_default();
    if id >= shows.len() {
        return false;
    }

    let count = ui.count_prefix;
    if handle_navigation_command(command, ui.current_page_mut(), id, shows.len(), count) {
        return true;
    }
    match command {
        Command::ChooseSelected => {
            let context_id = ContextId::Show(shows[id].id.clone());
            ui.new_page(PageState::Context {
                id: None,
                context_page_type: ContextPageType::Browsing(context_id),
                state: None,
            });
        }
        Command::ShowActionsOnSelectedItem => {
            let actions = construct_show_actions(shows[id], data);
            ui.popup = Some(PopupState::ActionList(
                Box::new(ActionListItem::Show(shows[id].clone(), actions)),
                ListState::default(),
            ));
        }
        _ => return false,
    }
    true
}

pub fn handle_command_for_episode_list_window(
    command: Command,
    client_pub: &crate::client::ClientRequestSender,
    episodes: &[&Episode],
    data: &DataReadGuard,
    ui: &mut UIStateGuard,
    state: &SharedState,
) -> Result<bool> {
    let id = ui.current_page().selected_index().unwrap_or_default();
    if id >= episodes.len() {
        return Ok(false);
    }

    let count = ui.count_prefix;
    if handle_navigation_command(command, ui.current_page_mut(), id, episodes.len(), count) {
        return Ok(true);
    }
    match command {
        Command::ChooseSelected => {
            // Episodes don't have a Tracks context, so clear it
            state.player.write().currently_playing_tracks_id = None;

            client_pub.send(ClientRequest::Player(PlayerRequest::StartPlayback(
                Playback::URIs(vec![episodes[id].id.clone().into()], None),
                None,
            )))?;
        }
        Command::ShowActionsOnSelectedItem => {
            let actions = command::construct_episode_actions(episodes[id], data);
            ui.popup = Some(PopupState::ActionList(
                Box::new(ActionListItem::Episode(episodes[id].clone(), actions)),
                ListState::default(),
            ));
        }
        Command::AddSelectedItemToQueue => {
            ui.spotify_queue_labels.remember_episode(episodes[id]);
            client_pub.send(ClientRequest::AddPlayableToQueue(
                episodes[id].id.clone().into(),
            ))?;
        }
        _ => return Ok(false),
    }
    Ok(true)
}

fn handle_command_for_episode_table_window(
    command: Command,
    client_pub: &crate::client::ClientRequestSender,
    show_id: &ShowId,
    episodes: &[&Episode],
    data: &DataReadGuard,
    ui: &mut UIStateGuard,
    state: &SharedState,
) -> Result<bool> {
    let id = ui.current_page().selected_index().unwrap_or_default();
    if id >= episodes.len() {
        return Ok(false);
    }

    let count = ui.count_prefix;
    if handle_navigation_command(command, ui.current_page_mut(), id, episodes.len(), count) {
        return Ok(true);
    }
    match command {
        Command::ChooseSelected => {
            let uri = episodes[id].id.uri();

            // Show context doesn't have a Tracks context, so clear it
            state.player.write().currently_playing_tracks_id = None;

            client_pub.send(ClientRequest::Player(PlayerRequest::StartPlayback(
                Playback::Context(
                    ContextId::Show(show_id.clone_static()),
                    Some(rspotify::model::Offset::Uri(uri)),
                ),
                None,
            )))?;
        }
        Command::ShowActionsOnSelectedItem => {
            let actions = command::construct_episode_actions(episodes[id], data);
            ui.popup = Some(PopupState::ActionList(
                Box::new(ActionListItem::Episode(episodes[id].clone(), actions)),
                ListState::default(),
            ));
        }
        Command::AddSelectedItemToQueue => {
            ui.spotify_queue_labels.remember_episode(episodes[id]);
            client_pub.send(ClientRequest::AddPlayableToQueue(
                episodes[id].id.clone().into(),
            ))?;
        }
        _ => return Ok(false),
    }
    Ok(true)
}

#[cfg(test)]
mod tests {
    use super::{
        borrowed_row_source_index, clear_track_selection, context_source_index_from_selection,
        extend_track_selection, listenbrainz_album_intent, navigate_and_clear_selection,
        navigation_clears_selection, playlist_cursor_after_move, playlist_reorder_indices,
        playlist_rows_are_safe_for_delete, sortable_pane_matches_context,
        spotify_reorder_mutation_request, CachedContextKind,
    };
    use crate::command::Command;
    use crate::state::{
        synchronize_context_track_uris, synchronize_journal_uris, synchronize_native_spotify_queue,
        synchronize_unified_playlist_entries, synchronize_youtube_context_tracks, ContextTrackPane,
        ContextTrackSelection, JournalSelectionScope, ListenBrainzAlbumIntent, MediaId, MediaKind,
        MultiSelectModel, NativeQueueRow, PageState, PlaylistEntryId, Provider, SearchPageUIState,
        SearchPane, SearchScope, Track, UIState, YouTubeContextId, YouTubeContextPageUIState,
    };
    use rspotify::{model::TrackId, prelude::Id as _};

    fn track(id: &'static str) -> Track {
        Track {
            id: TrackId::from_id(id).unwrap().into_static(),
            name: String::new(),
            artists: Vec::new(),
            album: None,
            duration: std::time::Duration::ZERO,
            explicit: false,
            added_at: 0,
        }
    }

    fn media_id(provider: Provider, raw_id: &str) -> MediaId {
        MediaId {
            provider,
            kind: MediaKind::Track,
            raw_id: raw_id.to_owned(),
        }
    }

    #[test]
    fn borrowed_row_mapping_is_pointer_only() {
        let rows = vec![String::from("same"), String::from("same")];
        assert_eq!(borrowed_row_source_index(&rows[1], &rows), Some(1));
        let detached = String::from("same");
        assert_eq!(borrowed_row_source_index(&detached, &rows), None);
    }

    #[test]
    fn listenbrainz_album_rows_resolve_only_for_open_commands() {
        assert_eq!(
            listenbrainz_album_intent(Command::ChooseSelected),
            Some(ListenBrainzAlbumIntent::OpenPage)
        );
        assert_eq!(
            listenbrainz_album_intent(Command::ShowActionsOnSelectedItem),
            Some(ListenBrainzAlbumIntent::OpenMenu)
        );
        assert_eq!(
            listenbrainz_album_intent(Command::AddSelectedItemToQueue),
            None
        );
    }

    #[test]
    fn ready_adapter_maps_detached_rows_through_reordered_projection() {
        let mut selection = ContextTrackSelection::default();
        let full_rows = vec![
            track("3n3Ppam7vgaVa1iaRUc9Lp"),
            track("4uLU6hMCjMI75M1A2tKUQC"),
            track("1301WleyT98MSxVHPZCA6M"),
        ];
        let complete_uris = full_rows.iter().map(|row| row.id.uri()).collect::<Vec<_>>();
        let visible_uris = vec![full_rows[2].id.uri(), full_rows[0].id.uri()];
        synchronize_context_track_uris(
            &mut selection,
            1,
            "spotify:playlist:one",
            ContextTrackPane::Playlist,
            Some("query"),
            complete_uris,
            visible_uris,
        )
        .unwrap();
        let detached_visible = full_rows[2].clone();
        assert_eq!(
            context_source_index_from_selection(&selection, 0, 2, &detached_visible, &full_rows,),
            Some(2)
        );
        assert_eq!(
            context_source_index_from_selection(&selection, 1, 2, &detached_visible, &full_rows,),
            Some(0)
        );
    }

    #[test]
    fn ambiguous_adapter_falls_back_to_exact_borrowed_occurrence() {
        let mut selection = ContextTrackSelection::default();
        synchronize_context_track_uris(
            &mut selection,
            1,
            "spotify:playlist:one",
            ContextTrackPane::Playlist,
            None,
            ["a", "a"],
            ["a", "a"],
        )
        .unwrap();
        let full_rows = vec![
            track("3n3Ppam7vgaVa1iaRUc9Lp"),
            track("3n3Ppam7vgaVa1iaRUc9Lp"),
        ];
        assert_eq!(
            context_source_index_from_selection(&selection, 1, 2, &full_rows[1], &full_rows),
            Some(1)
        );
    }

    #[test]
    fn unscoped_adapter_never_pointer_falls_back() {
        let selection = ContextTrackSelection::default();
        let full_rows = vec![
            track("3n3Ppam7vgaVa1iaRUc9Lp"),
            track("3n3Ppam7vgaVa1iaRUc9Lp"),
        ];
        assert_eq!(
            context_source_index_from_selection(&selection, 1, 2, &full_rows[1], &full_rows),
            None
        );
    }

    #[test]
    fn sortable_panes_require_the_matching_cached_variant() {
        assert!(sortable_pane_matches_context(
            ContextTrackPane::Playlist,
            CachedContextKind::Playlist
        ));
        assert!(sortable_pane_matches_context(
            ContextTrackPane::ArtistTopTracks,
            CachedContextKind::Artist
        ));
        assert!(!sortable_pane_matches_context(
            ContextTrackPane::ArtistLikedSongs,
            CachedContextKind::Artist
        ));
        assert!(!sortable_pane_matches_context(
            ContextTrackPane::Playlist,
            CachedContextKind::Album
        ));
        assert!(!sortable_pane_matches_context(
            ContextTrackPane::Album,
            CachedContextKind::Show
        ));
    }

    #[test]
    fn playlist_reorder_uses_full_coordinates_and_keeps_filtered_cursor() {
        assert_eq!(playlist_reorder_indices(2, 4, false), Some((1, 2)));
        assert_eq!(playlist_reorder_indices(2, 4, true), Some((3, 2)));
        assert_eq!(playlist_reorder_indices(0, 4, false), None);
        assert_eq!(playlist_reorder_indices(3, 4, true), None);
        assert_eq!(playlist_cursor_after_move(1, 3, true), 1);
        assert_eq!(playlist_cursor_after_move(1, 3, false), 3);
    }

    #[test]
    fn spotify_reorder_request_requires_and_sends_the_base_snapshot() {
        let playlist_id = crate::state::PlaylistId::from_id("playlist").unwrap();
        assert!(spotify_reorder_mutation_request(
            &playlist_id,
            1,
            2,
            None,
            crate::client::PlaylistMutationOperationId(1),
        )
        .is_none());
        let request = spotify_reorder_mutation_request(
            &playlist_id,
            3,
            2,
            Some("base-snapshot"),
            crate::client::PlaylistMutationOperationId(2),
        )
        .unwrap();
        assert!(matches!(
            request,
            crate::client::ClientRequest::SpotifyPlaylistMutation(
                crate::client::SpotifyMutationIntent::Reorder {
                    range_start: 2,
                    insert_before: 4,
                    range_length: 1,
                    snapshot_id,
                    ..
                }
            ) if snapshot_id == "base-snapshot"
        ));
    }

    #[test]
    fn spotify_legacy_delete_is_hidden_for_duplicate_occurrences_until_p5_exact_removal() {
        let first = track("3n3Ppam7vgaVa1iaRUc9Lp");
        let second = track("4uLU6hMCjMI75M1A2tKUQC");
        assert!(playlist_rows_are_safe_for_delete(
            &[first.clone(), second],
            None
        ));
        assert!(!playlist_rows_are_safe_for_delete(
            &[first.clone(), first.clone()],
            None
        ));
        assert!(playlist_rows_are_safe_for_delete(
            std::slice::from_ref(&first),
            Some(std::slice::from_ref(&first.id))
        ));
        let missing = TrackId::from_id("1301WleyT98MSxVHPZCA6M")
            .unwrap()
            .into_static();
        assert!(!playlist_rows_are_safe_for_delete(
            std::slice::from_ref(&first),
            Some(std::slice::from_ref(&missing))
        ));
    }

    #[test]
    fn youtube_normal_cursor_move_clears_selection() {
        let tracks = vec![
            crate::state::YouTubeTrack {
                id: "one".to_owned(),
                name: String::new(),
                artists: String::new(),
                album: None,
                duration: String::new(),
                explicit: false,
                thumbnail_url: None,
                is_video: false,
            },
            crate::state::YouTubeTrack {
                id: "two".to_owned(),
                name: String::new(),
                artists: String::new(),
                album: None,
                duration: String::new(),
                explicit: false,
                thumbnail_url: None,
                is_video: false,
            },
        ];
        let mut ui_state = UIState::default();
        ui_state.history.push(PageState::YouTubeContext {
            id: YouTubeContextId::LikedTracks,
            context: Some(crate::state::YouTubeContext {
                title: String::new(),
                description: None,
                tracks: tracks.clone(),
                playlist_set_video_ids: Vec::new(),
                artist: None,
            }),
            state: YouTubeContextPageUIState::new(),
        });
        let mutex = crate::state::TrackedMutex::new(ui_state);
        let mut ui = mutex.lock();
        synchronize_youtube_context_tracks(
            ui.current_page_mut()
                .youtube_context_track_selection_mut()
                .expect("YouTube context selection"),
            1,
            &YouTubeContextId::LikedTracks,
            &tracks,
        )
        .unwrap();
        ui.current_page_mut()
            .youtube_context_track_selection_mut()
            .expect("YouTube context selection")
            .extend_range(0, 1)
            .unwrap();

        ui.current_page_mut().select(1);
        assert_eq!(
            ui.current_page()
                .youtube_context_track_selection()
                .expect("YouTube context selection")
                .selected_visible_indices(),
            vec![0, 1]
        );

        assert!(navigation_clears_selection(
            Command::SelectPreviousOrScrollUp
        ));
        assert!(!navigation_clears_selection(Command::ExtendSelectionNext));
        assert!(navigate_and_clear_selection(
            Command::SelectPreviousOrScrollUp,
            &mut ui,
            1,
            2,
            None,
        ));
        assert_eq!(ui.current_page().selected_index(), Some(0));
        assert!(ui
            .current_page()
            .youtube_context_track_selection()
            .expect("YouTube context selection")
            .selected_visible_indices()
            .is_empty());
    }

    #[test]
    fn spotify_search_cursor_move_clears_selection_through_the_shared_path() {
        let mut ui_state = UIState::default();
        ui_state.history.push(PageState::Search {
            line_input: Default::default(),
            current_query: "query".to_owned(),
            state: SearchPageUIState::new(),
        });
        let mutex = crate::state::TrackedMutex::new(ui_state);
        let mut ui = mutex.lock();
        let PageState::Search { state, .. } = ui.current_page_mut() else {
            unreachable!("test page should be search")
        };
        state.focus = crate::state::SearchFocusState::Tracks;
        state
            .search_selection
            .synchronize(
                SearchScope::new(
                    crate::config::ActiveProvider::Spotify,
                    1,
                    "query",
                    SearchPane::SpotifyTracks,
                ),
                ["one", "two"],
            )
            .unwrap();
        state.search_selection.extend_visible_range(0, 1).unwrap();
        ui.current_page_mut().select(1);

        let selected = match ui.current_page() {
            PageState::Search { state, .. } => state.search_selection.selected_visible_indices(),
            _ => unreachable!("test page should be search"),
        };
        assert_eq!(selected, vec![0, 1]);
        assert!(navigate_and_clear_selection(
            Command::SelectPreviousOrScrollUp,
            &mut ui,
            1,
            2,
            None,
        ));
        assert_eq!(ui.current_page().selected_index(), Some(0));
        let selected = match ui.current_page() {
            PageState::Search { state, .. } => state.search_selection.selected_visible_indices(),
            _ => unreachable!("test page should be search"),
        };
        assert!(selected.is_empty());
    }

    #[test]
    fn journal_cursor_navigation_clears_selection() {
        let mut table = ratatui::widgets::TableState::default();
        table.select(Some(0));
        let mut ui_state = UIState::default();
        ui_state.history.push(PageState::Journal {
            table,
            journal_selection: Default::default(),
        });
        let mutex = crate::state::TrackedMutex::new(ui_state);
        let mut ui = mutex.lock();
        synchronize_journal_uris(
            ui.current_page_mut()
                .journal_selection_mut()
                .expect("Journal selection"),
            JournalSelectionScope::journal(1),
            None,
            ["one", "two"],
            ["one", "two"],
        )
        .unwrap();
        assert!(extend_track_selection(&mut ui, 0, 2, 1, 1));
        assert_eq!(
            ui.current_page()
                .journal_selection()
                .expect("Journal selection")
                .selected_visible_indices(),
            vec![0, 1]
        );

        assert!(navigate_and_clear_selection(
            Command::SelectNextOrScrollDown,
            &mut ui,
            0,
            2,
            None,
        ));
        assert_eq!(ui.current_page().selected_index(), Some(1));
        assert!(ui
            .current_page()
            .journal_selection()
            .expect("Journal selection")
            .selected_visible_indices()
            .is_empty());
    }

    #[test]
    fn queue_selection_clears_through_the_shared_adapter() {
        let first = media_id(Provider::Spotify, "one");
        let second = media_id(Provider::Spotify, "two");
        let mut queue_selection = Default::default();
        synchronize_native_spotify_queue(
            &mut queue_selection,
            1,
            vec![NativeQueueRow::Media(first), NativeQueueRow::Media(second)],
        )
        .unwrap();
        queue_selection.extend_range(0, 1).unwrap();

        let mut ui_state = UIState::default();
        ui_state.history.push(PageState::Queue {
            table: ratatui::widgets::TableState::default(),
            queue_selection,
        });
        let mutex = crate::state::TrackedMutex::new(ui_state);
        let mut ui = mutex.lock();
        assert_eq!(
            ui.current_page()
                .queue_selection()
                .expect("Queue selection")
                .selected_visible_indices(),
            vec![0, 1]
        );

        clear_track_selection(&mut ui);

        assert!(ui
            .current_page()
            .queue_selection()
            .expect("Queue selection")
            .selected_visible_indices()
            .is_empty());
    }

    #[test]
    fn unified_playlist_selection_clears_through_the_shared_adapter() {
        let first = media_id(Provider::Spotify, "one");
        let second = media_id(Provider::YouTubeMusic, "two");
        let mut ui_state = UIState::default();
        ui_state
            .history
            .push(PageState::new_unified_playlist("playlist"));
        let selection = ui_state
            .history
            .last_mut()
            .and_then(PageState::unified_playlist_selection_mut)
            .expect("Unified playlist selection");
        synchronize_unified_playlist_entries(
            selection,
            "playlist",
            vec![
                (first.clone(), PlaylistEntryId(1)),
                (second.clone(), PlaylistEntryId(2)),
            ],
            vec![(first, PlaylistEntryId(1)), (second, PlaylistEntryId(2))],
        )
        .unwrap();
        selection.extend_range(0, 1).unwrap();
        let mutex = crate::state::TrackedMutex::new(ui_state);
        let mut ui = mutex.lock();
        assert_eq!(
            ui.current_page()
                .unified_playlist_selection()
                .expect("Unified playlist selection")
                .selected_visible_indices(),
            vec![0, 1]
        );

        clear_track_selection(&mut ui);

        assert!(ui
            .current_page()
            .unified_playlist_selection()
            .expect("Unified playlist selection")
            .selected_visible_indices()
            .is_empty());
    }
}
