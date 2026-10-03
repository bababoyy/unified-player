#![allow(clippy::items_after_test_module)]

use std::fmt::Write as _;

use super::bulk_action::{
    current_spotify_epoch, current_youtube_epoch, replan_spotify_menu,
    replan_spotify_menu_for_owner, replan_youtube_menu, replan_youtube_menu_for_owner,
};
use super::*;
use crate::client::{
    dispatch_legacy_playlist, playlist_request, PlaylistApplicationService, PlaylistRequest,
    PlaylistRequestKind,
};
use crate::observability::{DiagnosticAction as A, DiagnosticRowId as R};
use crate::state::MultiSelectActionMenu;
use crate::{
    command::construct_artist_actions, state::ConfirmableAction, utils::filtered_items_from_query,
};
use anyhow::Context;

fn handle_command_for_command_help_popup(command: Command, ui: &mut UIStateGuard) -> bool {
    let Some(PopupState::CommandHelp { scroll_offset }) = ui.popup.as_ref() else {
        return false;
    };
    let scroll_offset = *scroll_offset;

    if matches!(command, Command::ClosePopup | Command::PreviousPage) {
        ui.popup = None;
        return true;
    }
    if command == Command::Search {
        ui.new_search_popup();
        return true;
    }

    let bindings = config::get_config().keymap_config.resolved_bindings();
    let n_bindings = ui.search_filtered_item_count(&bindings);
    let count = ui.count_prefix;
    let mut page = PageState::CommandHelp { scroll_offset };
    let handled = crate::event::page::handle_navigation_command(
        command,
        &mut page,
        scroll_offset,
        n_bindings,
        count,
    );
    if handled {
        let PageState::CommandHelp { scroll_offset } = page else {
            unreachable!("temporary command-help page changed type");
        };
        if let Some(PopupState::CommandHelp {
            scroll_offset: popup_offset,
        }) = ui.popup.as_mut()
        {
            *popup_offset = scroll_offset;
        }
    }
    handled
}

pub fn handle_key_sequence_for_popup(
    key_sequence: &KeySequence,
    client_pub: &crate::client::ClientRequestSender,
    state: &SharedState,
    ui: &mut UIStateGuard,
) -> Result<bool> {
    #[cfg(feature = "private-capture")]
    if matches!(ui.popup, Some(PopupState::PrivateDerivativePreview { .. })) {
        return Ok(handle_diagnostic_popup(key_sequence, state, ui));
    }
    #[cfg(feature = "private-capture")]
    if matches!(
        ui.popup,
        Some(
            PopupState::PrivateCaptureSelector { .. }
                | PopupState::PrivateCapturePassphrase { .. }
                | PopupState::PrivateCaptureConfirm { .. }
        )
    ) {
        return Ok(handle_private_capture_popup(key_sequence, state, ui));
    }
    if matches!(
        ui.popup,
        Some(PopupState::DiagnosticActions { .. } | PopupState::DiagnosticDetail { .. })
    ) {
        return Ok(handle_diagnostic_popup(key_sequence, state, ui));
    }
    // handle popups that need reading the raw key sequence instead of the matched command
    match ui.popup.as_ref().context("empty popup")? {
        PopupState::Search { .. } => {
            return handle_key_sequence_for_search_popup(key_sequence, client_pub, state, ui);
        }
        PopupState::Volume { .. } => {
            return handle_key_sequence_for_volume_popup(key_sequence, client_pub, state, ui);
        }
        PopupState::PlaylistCreate { .. } => {
            return handle_key_sequence_for_create_playlist_popup(key_sequence, client_pub, ui);
        }
        PopupState::SessionHistoryCreate { .. } => {
            return handle_key_sequence_for_session_history_create_popup(
                key_sequence,
                client_pub,
                ui,
            );
        }
        PopupState::TrackNote { .. } => {
            return handle_key_sequence_for_track_note_popup(key_sequence, state, ui);
        }
        PopupState::JournalListName { .. } => {
            return handle_key_sequence_for_journal_list_name_popup(key_sequence, state, ui);
        }
        PopupState::PlaylistName { .. } => {
            return handle_key_sequence_for_playlist_name_popup(key_sequence, client_pub, ui);
        }
        PopupState::ListenBrainzPlaylists { .. } => {
            let command = config::get_config()
                .keymap_config
                .find_command_from_key_sequence(key_sequence);
            if let Some(command) = command {
                handle_listenbrainz_playlist_command(command, client_pub, ui)?;
            }
            return Ok(true);
        }
        PopupState::ListenBrainzToken { .. } => {
            return Ok(handle_listenbrainz_token_popup(
                key_sequence,
                client_pub,
                ui,
            ));
        }
        PopupState::ConfigEdit { .. } => {
            return Ok(handle_key_sequence_for_config_edit_popup(
                key_sequence,
                client_pub,
                ui,
            ));
        }
        PopupState::ConfigChoice { .. } => {
            return handle_key_sequence_for_config_choice_popup(key_sequence, client_pub, ui);
        }
        PopupState::ConfigMultiChoice { .. } => {
            return Ok(handle_key_sequence_for_config_multi_choice_popup(
                key_sequence,
                ui,
            ));
        }
        PopupState::WorkspaceScope { .. } => {
            return handle_key_sequence_for_workspace_scope_popup(
                key_sequence,
                client_pub,
                state,
                ui,
            );
        }
        PopupState::ActionList(item, ..) | PopupState::AnchoredActionList { item, .. } => {
            return handle_key_sequence_for_action_list_popup(
                item.n_actions(),
                key_sequence,
                client_pub,
                state,
                ui,
            );
        }
        PopupState::ListenBrainzWorkspace { .. } => {
            return handle_key_sequence_for_listenbrainz_workspace(
                key_sequence,
                client_pub,
                state,
                ui,
            );
        }
        PopupState::ListenBrainzResolve { .. } => {
            return handle_key_sequence_for_listenbrainz_resolve(key_sequence, state, ui);
        }
        // can't use match guard: the match holds an immutable borrow of ui
        #[allow(clippy::collapsible_match)]
        PopupState::UserPlaylistList(..) => {
            if handle_key_sequence_for_playlist_search_popup(key_sequence, ui) {
                return Ok(true);
            }
        }
        PopupState::YouTubePlaylistList(..) => {
            if handle_key_sequence_for_youtube_playlist_search_popup(key_sequence, ui) {
                return Ok(true);
            }
        }
        PopupState::ConfirmAction { action, .. } => {
            return handle_key_sequence_for_confirm_popup(
                key_sequence,
                client_pub,
                state,
                ui,
                action.clone(),
            );
        }
        _ => {}
    }

    let Some(command) = config::get_config()
        .keymap_config
        .find_command_from_key_sequence(key_sequence)
    else {
        return Ok(false);
    };

    match ui.popup.as_ref().context("empty popup")? {
        PopupState::DeferredAction { .. } => {
            if matches!(
                command,
                Command::ClosePopup | Command::PreviousPage | Command::ChooseSelected
            ) {
                ui.popup = None;
                return Ok(true);
            }
            Ok(false)
        }
        PopupState::CommandHelp { .. } => Ok(handle_command_for_command_help_popup(command, ui)),
        PopupState::SpotifyUserSearch { .. } => {
            if matches!(command, Command::ClosePopup | Command::PreviousPage) {
                ui.popup = None;
                return Ok(true);
            }
            Ok(false)
        }
        PopupState::SpotifyUserCandidates { candidates, .. } => {
            let candidates = candidates.clone();
            handle_command_for_list_popup(
                command,
                ui,
                candidates.len(),
                |_, _| {},
                |ui: &mut UIStateGuard, id: usize| -> Result<()> {
                    let Some(candidate) = candidates.get(id) else {
                        return Ok(());
                    };
                    ui.popup = Some(PopupState::SpotifyUserPlaylists {
                        profile: candidate.profile.clone(),
                        playlists: candidate.playlists.clone(),
                        state: ratatui::widgets::ListState::default(),
                    });
                    Ok(())
                },
                |ui: &mut UIStateGuard| {
                    ui.popup = None;
                },
            )
        }
        PopupState::SpotifyUserPlaylists { playlists, .. } => {
            let playlist_uris = playlists
                .iter()
                .map(|playlist| playlist.id.uri())
                .collect::<Vec<_>>();
            handle_command_for_context_browsing_list_popup(
                command,
                ui,
                &playlist_uris,
                &rspotify::model::Type::Playlist,
            )
        }
        PopupState::SpotifyUserProfile { profile } => {
            if matches!(command, Command::ClosePopup | Command::PreviousPage) {
                ui.popup = None;
                return Ok(true);
            }
            if command == Command::ChooseSelected {
                open::that(&profile.profile_url).context("open Spotify user profile")?;
                ui.popup = None;
                return Ok(true);
            }
            Ok(false)
        }
        PopupState::YouTubeArtistMenu { details, .. } => {
            let details = details.clone();
            let rows = youtube_artist_menu_rows(&details);
            handle_command_for_list_popup(
                command,
                ui,
                rows.len(),
                |_, _| {},
                |ui: &mut UIStateGuard, id: usize| -> Result<()> {
                    match youtube_artist_menu_selection(&details, id) {
                        YouTubeArtistMenuSelection::Subscribe => {
                            client_pub.send(if details.subscribed {
                                ClientRequest::UnsubscribeYouTubeArtist {
                                    channel_id: details.channel_id.clone(),
                                }
                            } else {
                                ClientRequest::SubscribeYouTubeArtist {
                                    channel_id: details.channel_id.clone(),
                                }
                            })?;
                            ui.popup = None;
                        }
                        YouTubeArtistMenuSelection::Radio(Some(radio_id)) => {
                            ui.new_page(PageState::YouTubeContext {
                                id: YouTubeContextId::Playlist(radio_id.clone()),
                                context: None,
                                state: YouTubeContextPageUIState::new(),
                            });
                            client_pub.send(ClientRequest::GetYouTubeContext(
                                YouTubeContextId::Playlist(radio_id),
                            ))?;
                        }
                        YouTubeArtistMenuSelection::Channel(channel_id) => {
                            open::that(format!("https://music.youtube.com/channel/{channel_id}"))?;
                            ui.popup = None;
                        }
                        YouTubeArtistMenuSelection::Album(album_id) => {
                            ui.new_page(PageState::YouTubeContext {
                                id: YouTubeContextId::Album(album_id.clone()),
                                context: None,
                                state: YouTubeContextPageUIState::new(),
                            });
                            client_pub.send(ClientRequest::GetYouTubeContext(
                                YouTubeContextId::Album(album_id),
                            ))?;
                        }
                        YouTubeArtistMenuSelection::Related(artist_id) => {
                            ui.new_page(PageState::YouTubeContext {
                                id: YouTubeContextId::Artist(artist_id.clone()),
                                context: None,
                                state: YouTubeContextPageUIState::new(),
                            });
                            client_pub.send(ClientRequest::GetYouTubeContext(
                                YouTubeContextId::Artist(artist_id),
                            ))?;
                        }
                        YouTubeArtistMenuSelection::Radio(None)
                        | YouTubeArtistMenuSelection::None => {}
                    }
                    Ok(())
                },
                |ui: &mut UIStateGuard| {
                    ui.popup = None;
                },
            )
        }
        PopupState::UnifiedPlaylistDestination { options, .. } => {
            let options = options.clone();
            handle_command_for_list_popup(
                command,
                ui,
                options.len() + 1,
                |_, _| {},
                |ui: &mut UIStateGuard, id: usize| -> Result<()> {
                    if id == options.len() {
                        let items = match &ui.popup {
                            Some(PopupState::UnifiedPlaylistDestination { items, .. }) => {
                                items.clone()
                            }
                            _ => Vec::new(),
                        };
                        ui.popup = Some(PopupState::PlaylistCreate {
                            target: crate::state::PlaylistCreateTarget::Unified,
                            public: false,
                            name: crate::ui::single_line_input::LineInput::default(),
                            desc: crate::ui::single_line_input::LineInput::default(),
                            current_field: crate::state::PlaylistCreateCurrentField::Name,
                            pending_items: Some(
                                items
                                    .iter()
                                    .map(crate::state::PlaylistSeedItem::from_unified_playlist_item)
                                    .collect(),
                            ),
                            source_provider: None,
                            source_epoch: None,
                        });
                        return Ok(());
                    }
                    let Some((_, _id)) = options.get(id) else {
                        return Ok(());
                    };
                    let items = match &ui.popup {
                        Some(PopupState::UnifiedPlaylistDestination { items, .. }) => items.clone(),
                        _ => Vec::new(),
                    };
                    let Some((_, playlist_id)) = options.get(id) else {
                        return Ok(());
                    };
                    dispatch_legacy_playlist(
                        client_pub,
                        ClientRequest::AddItemsToUnifiedPlaylist {
                            playlist_id: playlist_id.clone(),
                            items,
                            operation: None,
                        },
                    )?;
                    ui.popup = None;
                    super::window::clear_track_selection(ui);
                    Ok(())
                },
                |ui: &mut UIStateGuard| {
                    ui.popup = None;
                },
            )
        }
        PopupState::ConfirmAction { .. } => {
            anyhow::bail!("confirm action should be handled before")
        }
        PopupState::Search { .. } | PopupState::Volume { .. } => {
            anyhow::bail!("raw-key popups should be handled before")
        }
        PopupState::PlaylistCreate { .. } => {
            anyhow::bail!("create playlist popup should be handled before")
        }
        PopupState::SessionHistoryCreate { .. } => {
            anyhow::bail!("session history create popup should be handled before")
        }
        PopupState::ActionList(..) | PopupState::AnchoredActionList { .. } => {
            anyhow::bail!("action list popup should be handled before")
        }
        PopupState::TrackNote { .. } => anyhow::bail!("track note popup should be handled before"),
        PopupState::JournalListName { .. } => {
            anyhow::bail!("journal list name popup should be handled before")
        }
        PopupState::PlaylistName { .. } => {
            anyhow::bail!("playlist name popup should be handled before")
        }
        PopupState::ListenBrainzPlaylists { .. }
        | PopupState::ListenBrainzToken { .. }
        | PopupState::ConfigEdit { .. } => {
            anyhow::bail!("config edit popup should be handled before")
        }
        PopupState::ConfigChoice { .. } => {
            anyhow::bail!("config choice popup should be handled before")
        }
        PopupState::ConfigMultiChoice { .. } => {
            anyhow::bail!("config multi-choice popup should be handled before")
        }
        PopupState::WorkspaceScope { .. } => {
            anyhow::bail!("workspace scope popup should be handled before")
        }
        PopupState::DiagnosticActions { .. } | PopupState::DiagnosticDetail { .. } => {
            anyhow::bail!("diagnostic popup should be handled before")
        }
        PopupState::ListenBrainzSyncDetails { preview, .. } => {
            let row_count = preview.rows.len();
            handle_command_for_list_popup(
                command,
                ui,
                row_count,
                |_, _| {},
                |_, _| Ok(()),
                |ui: &mut UIStateGuard| {
                    ui.popup = None;
                },
            )
        }
        // The workspace intercepts key sequences before command dispatch, so
        // this arm is unreachable. It only satisfies exhaustiveness.
        PopupState::ListenBrainzWorkspace { .. } => Ok(false),
        // Same for the resolve popup: handled before command dispatch.
        PopupState::ListenBrainzResolve { .. } => Ok(false),
        #[cfg(feature = "private-capture")]
        PopupState::PrivateDerivativePreview { .. } => {
            anyhow::bail!("private derivative preview should be handled before")
        }
        #[cfg(feature = "private-capture")]
        PopupState::PrivateCaptureSelector { .. }
        | PopupState::PrivateCapturePassphrase { .. }
        | PopupState::PrivateCaptureConfirm { .. } => {
            anyhow::bail!("private-capture popup should be handled before")
        }
        PopupState::JournalListSelect(action, _) => match action {
            JournalListPopupAction::AddTracks { tracks } => {
                let tracks = tracks.clone();
                let n_items = state.data.read().journal.lists.len();
                handle_command_for_list_popup(
                    command,
                    ui,
                    n_items,
                    |_, _| {},
                    |ui: &mut UIStateGuard, id: usize| -> Result<()> {
                        let list_id = {
                            let data = state.data.read();
                            data.journal.lists[id].id.clone()
                        };
                        update_track_journal(state, |journal| {
                            journal.add_tracks_to_list(&list_id, tracks.clone());
                        })?;
                        ui.popup = None;
                        Ok(())
                    },
                    |ui: &mut UIStateGuard| {
                        ui.popup = None;
                    },
                )
            }
            JournalListPopupAction::AddYouTubeTracks { tracks } => {
                let tracks = tracks.clone();
                let n_items = state.data.read().journal.lists.len();
                handle_command_for_list_popup(
                    command,
                    ui,
                    n_items,
                    |_, _| {},
                    |ui: &mut UIStateGuard, id: usize| -> Result<()> {
                        let list_id = {
                            let data = state.data.read();
                            data.journal.lists[id].id.clone()
                        };
                        update_track_journal(state, |journal| {
                            journal.add_youtube_tracks_to_list(&list_id, tracks.clone());
                        })?;
                        ui.popup = None;
                        Ok(())
                    },
                    |ui: &mut UIStateGuard| {
                        ui.popup = None;
                    },
                )
            }
        },
        PopupState::ArtistList(_, artists, _) => {
            let n_items = artists.len();

            handle_command_for_list_popup(
                command,
                ui,
                n_items,
                |_, _| {},
                |ui: &mut UIStateGuard, id: usize| -> Result<()> {
                    let Some(PopupState::ArtistList(action, artists, _)) = &ui.popup else {
                        return Ok(());
                    };

                    match action {
                        ArtistPopupAction::Browse => {
                            let context_id = ContextId::Artist(artists[id].id.clone());
                            ui.new_page(PageState::Context {
                                id: None,
                                context_page_type: ContextPageType::Browsing(context_id),
                                state: None,
                            });
                        }
                        ArtistPopupAction::ShowActions => {
                            let actions = {
                                let data = state.data.read();
                                construct_artist_actions(&artists[id], &data)
                            };
                            ui.popup = Some(PopupState::ActionList(
                                Box::new(ActionListItem::Artist(artists[id].clone(), actions)),
                                ListState::default(),
                            ));
                        }
                    }

                    Ok(())
                },
                |ui: &mut UIStateGuard| {
                    ui.popup = None;
                },
            )
        }
        PopupState::UserPlaylistList(action, _) => match action {
            PlaylistPopupAction::Browse {
                folder_id,
                search_query,
            } => {
                let search_query = search_query.clone();
                let data = state.data.read();
                let items = data.user_data.folder_playlists_items(*folder_id);
                let filtered_items = filtered_items_from_query(&search_query, &items);

                handle_command_for_list_popup(
                    command,
                    ui,
                    filtered_items.len(),
                    |_, _| {},
                    |ui: &mut UIStateGuard, id: usize| -> Result<()> {
                        match filtered_items.get(id).expect("invalid index") {
                            PlaylistFolderItem::Folder(f) => {
                                ui.popup = Some(PopupState::UserPlaylistList(
                                    PlaylistPopupAction::Browse {
                                        folder_id: f.target_id,
                                        search_query: search_query.clone(),
                                    },
                                    ListState::default(),
                                ));
                            }
                            PlaylistFolderItem::Playlist(p) => {
                                let context_id = ContextId::Playlist(
                                    PlaylistId::from_uri(&crate::utils::parse_uri(&p.id.uri()))?
                                        .into_static(),
                                );
                                ui.new_page(PageState::Context {
                                    id: None,
                                    context_page_type: ContextPageType::Browsing(context_id),
                                    state: None,
                                });
                            }
                        }
                        Ok(())
                    },
                    |ui: &mut UIStateGuard| {
                        ui.popup = None;
                    },
                )
            }
            PlaylistPopupAction::AddTrack {
                folder_id,
                track,
                search_query,
            } => {
                let search_query = search_query.clone();
                let track = track.clone();
                let data = state.data.read();
                let items = data
                    .user_data
                    .modifiable_playlist_items(Some(*folder_id))
                    .into_iter()
                    .cloned()
                    .collect::<Vec<_>>();
                let filtered_items = filtered_items_from_query(&search_query, &items)
                    .into_iter()
                    .cloned()
                    .collect::<Vec<_>>();
                let query = search_query.to_lowercase();
                let unified_playlists = data
                    .unified_playlists
                    .iter()
                    .filter(|playlist| {
                        query.is_empty() || playlist.name.to_lowercase().contains(&query)
                    })
                    .map(|playlist| (playlist.id.clone(), playlist.name.clone()))
                    .collect::<Vec<_>>();
                drop(data);
                let native_count = filtered_items.len();
                let create_index = native_count + unified_playlists.len();

                handle_command_for_list_popup(
                    command,
                    ui,
                    create_index + 1,
                    |_, _| {},
                    |ui: &mut UIStateGuard, id: usize| -> Result<()> {
                        if id == create_index {
                            ui.popup = Some(PopupState::PlaylistCreate {
                                target: crate::state::PlaylistCreateTarget::Spotify,
                                public: false,
                                name: crate::ui::single_line_input::LineInput::default(),
                                desc: crate::ui::single_line_input::LineInput::default(),
                                current_field: crate::state::PlaylistCreateCurrentField::Name,
                                pending_items: Some(vec![
                                    crate::state::PlaylistSeedItem::from_spotify_track(&track),
                                ]),
                                source_provider: Some(crate::state::Provider::Spotify),
                                source_epoch: Some(current_spotify_epoch(ui).value()),
                            });
                            return Ok(());
                        }
                        if let Some((playlist_id, _)) = unified_playlists
                            .get(id.saturating_sub(native_count))
                            .filter(|_| id >= native_count)
                        {
                            let seed = crate::state::PlaylistSeedItem::from_spotify_track(&track);
                            let operation = new_unified_playlist_operation(
                                "append",
                                crate::state::Provider::Spotify,
                                current_spotify_epoch(ui).value(),
                                crate::state::PlaylistDestination::Existing {
                                    target: crate::state::PlaylistTargetKind::Unified,
                                    id: playlist_id.clone(),
                                },
                                crate::state::PlaylistIntent::Append {
                                    seed: vec![seed.clone()],
                                },
                                unified_playlist_revision(state, playlist_id),
                            );
                            dispatch_legacy_playlist(
                                client_pub,
                                ClientRequest::AddItemsToUnifiedPlaylist {
                                    playlist_id: playlist_id.clone(),
                                    items: vec![seed.into_unified_playlist_item()],
                                    operation: Some(operation),
                                },
                            )?;
                            ui.popup = None;
                            return Ok(());
                        }
                        ui.popup = match filtered_items.get(id).context("invalid index")? {
                            PlaylistFolderItem::Folder(f) => Some(PopupState::UserPlaylistList(
                                PlaylistPopupAction::AddTrack {
                                    folder_id: f.target_id,
                                    track,
                                    search_query: search_query.clone(),
                                },
                                ListState::default(),
                            )),
                            PlaylistFolderItem::Playlist(p) => {
                                dispatch_legacy_playlist(
                                    client_pub,
                                    ClientRequest::AddPlayableToPlaylist(
                                        p.id.clone(),
                                        track.id.clone().into(),
                                    ),
                                )?;
                                None
                            }
                        };
                        Ok(())
                    },
                    |ui: &mut UIStateGuard| {
                        ui.popup = None;
                    },
                )
            }
            PlaylistPopupAction::AddTracks {
                folder_id,
                tracks,
                search_query,
            } => {
                let search_query = search_query.clone();
                let menu = tracks.clone();
                let data = state.data.read();
                let items = data.user_data.modifiable_playlist_items(Some(*folder_id));
                let filtered_items = filtered_items_from_query(&search_query, &items);
                let query = search_query.to_lowercase();
                let unified_playlists = data
                    .unified_playlists
                    .iter()
                    .filter(|playlist| {
                        query.is_empty() || playlist.name.to_lowercase().contains(&query)
                    })
                    .map(|playlist| (playlist.id.clone(), playlist.name.clone()))
                    .collect::<Vec<_>>();
                let native_count = filtered_items.len();
                let create_index = native_count + unified_playlists.len();

                handle_command_for_list_popup(
                    command,
                    ui,
                    create_index + 1,
                    |_, _| {},
                    |ui: &mut UIStateGuard, id: usize| -> Result<()> {
                        if id == create_index {
                            ui.popup = Some(PopupState::PlaylistCreate {
                                target: crate::state::PlaylistCreateTarget::Spotify,
                                public: false,
                                name: crate::ui::single_line_input::LineInput::default(),
                                desc: crate::ui::single_line_input::LineInput::default(),
                                current_field: crate::state::PlaylistCreateCurrentField::Name,
                                pending_items: Some(
                                    menu.items()
                                        .iter()
                                        .map(crate::state::PlaylistSeedItem::from_spotify_track)
                                        .collect(),
                                ),
                                source_provider: Some(crate::state::Provider::Spotify),
                                source_epoch: Some(current_spotify_epoch(ui).value()),
                            });
                            return Ok(());
                        }
                        if let Some((playlist_id, _)) = unified_playlists
                            .get(id.saturating_sub(native_count))
                            .filter(|_| id >= native_count)
                        {
                            let plan = replan_spotify_menu_for_owner(
                                &menu,
                                current_spotify_epoch(ui),
                                Action::AddToPlaylist,
                                crate::command::BulkActionOwner::UnifiedPlaylist,
                            )
                            .map_err(|_| anyhow::anyhow!("playlist target is stale"))?;
                            let seeds = menu
                                .items()
                                .iter()
                                .map(crate::state::PlaylistSeedItem::from_spotify_track)
                                .collect::<Vec<_>>();
                            let operation = new_unified_playlist_operation(
                                "append-bulk",
                                crate::state::Provider::Spotify,
                                current_spotify_epoch(ui).value(),
                                crate::state::PlaylistDestination::Existing {
                                    target: crate::state::PlaylistTargetKind::Unified,
                                    id: playlist_id.clone(),
                                },
                                crate::state::PlaylistIntent::Append {
                                    seed: seeds.clone(),
                                },
                                unified_playlist_revision(state, playlist_id),
                            );
                            let assignments = vec![super::bulk_action::BulkRequestAssignment::new(
                                playlist_request(ClientRequest::AddItemsToUnifiedPlaylist {
                                    playlist_id: playlist_id.clone(),
                                    items: seeds
                                        .into_iter()
                                        .map(crate::state::PlaylistSeedItem::into_unified_playlist_item)
                                        .collect(),
                                    operation: Some(operation),
                                })?,
                                plan.operation_ids(),
                            )];
                            super::bulk_action::dispatch_bulk_requests(
                                ui,
                                client_pub,
                                &plan,
                                assignments,
                            )?;
                            window::clear_track_selection(ui);
                            ui.popup = None;
                            return Ok(());
                        }
                        ui.popup = match filtered_items.get(id).expect("invalid index") {
                            PlaylistFolderItem::Folder(f) => Some(PopupState::UserPlaylistList(
                                PlaylistPopupAction::AddTracks {
                                    folder_id: f.target_id,
                                    tracks: menu,
                                    search_query: search_query.clone(),
                                },
                                ListState::default(),
                            )),
                            PlaylistFolderItem::Playlist(p) => {
                                let plan = replan_spotify_menu_for_owner(
                                    &menu,
                                    current_spotify_epoch(ui),
                                    Action::AddToPlaylist,
                                    crate::command::BulkActionOwner::Provider(
                                        crate::state::Provider::Spotify,
                                    ),
                                )
                                .map_err(|_| anyhow::anyhow!("playlist target is stale"))?;
                                let assignments = plan
                                    .operation_ids()
                                    .into_iter()
                                    .zip(menu.items().iter())
                                    .map(|(operation_id, track)| {
                                        playlist_request(ClientRequest::AddPlayableToPlaylist(
                                            p.id.clone(),
                                            track.id.clone().into(),
                                        ))
                                        .map(|request| {
                                            super::bulk_action::BulkRequestAssignment::one(
                                                request,
                                                operation_id,
                                            )
                                        })
                                    })
                                    .collect::<Result<Vec<_>>>()?;
                                super::bulk_action::dispatch_bulk_requests(
                                    ui,
                                    client_pub,
                                    &plan,
                                    assignments,
                                )?;
                                window::clear_track_selection(ui);
                                None
                            }
                        };
                        Ok(())
                    },
                    |ui: &mut UIStateGuard| {
                        ui.popup = None;
                    },
                )
            }
            PlaylistPopupAction::AddEpisode {
                folder_id,
                episode_id,
                search_query,
            } => {
                let search_query = search_query.clone();
                let episode_id = episode_id.clone();
                let data = state.data.read();
                let items = data.user_data.modifiable_playlist_items(Some(*folder_id));
                let filtered_items = filtered_items_from_query(&search_query, &items);

                handle_command_for_list_popup(
                    command,
                    ui,
                    filtered_items.len(),
                    |_, _| {},
                    |ui: &mut UIStateGuard, id: usize| -> Result<()> {
                        ui.popup = match filtered_items.get(id).expect("invalid index") {
                            PlaylistFolderItem::Folder(f) => Some(PopupState::UserPlaylistList(
                                PlaylistPopupAction::AddEpisode {
                                    folder_id: f.target_id,
                                    episode_id,
                                    search_query: search_query.clone(),
                                },
                                ListState::default(),
                            )),
                            PlaylistFolderItem::Playlist(p) => {
                                dispatch_legacy_playlist(
                                    client_pub,
                                    ClientRequest::AddPlayableToPlaylist(
                                        p.id.clone(),
                                        episode_id.into(),
                                    ),
                                )?;
                                None
                            }
                        };
                        Ok(())
                    },
                    |ui: &mut UIStateGuard| {
                        ui.popup = None;
                    },
                )
            }
        },
        PopupState::YouTubePlaylistList(action, _) => {
            let (bulk_menu, tracks, search_query, linking_unified_id) = match action {
                YouTubePlaylistPopupAction::AddTrack {
                    track,
                    search_query,
                } => (None, vec![track.clone()], search_query, None),
                YouTubePlaylistPopupAction::AddTracks {
                    tracks,
                    search_query,
                } => (
                    Some(tracks.clone()),
                    tracks.items().to_vec(),
                    search_query,
                    None,
                ),
                YouTubePlaylistPopupAction::LinkUnified {
                    unified_playlist_id,
                    search_query,
                } => (
                    None,
                    Vec::new(),
                    search_query,
                    Some(unified_playlist_id.clone()),
                ),
            };
            let linking_unified = linking_unified_id.is_some();
            let query = search_query.to_lowercase();
            let data = state.data.read();
            let playlists = data
                .user_data
                .youtube_library
                .playlists
                .iter()
                .filter(|playlist| {
                    query.is_empty() || playlist.name.to_lowercase().contains(&query)
                })
                .cloned()
                .collect::<Vec<_>>();
            let unified_playlists = if linking_unified {
                Vec::new()
            } else {
                data.unified_playlists
                    .iter()
                    .filter(|playlist| {
                        query.is_empty() || playlist.name.to_lowercase().contains(&query)
                    })
                    .map(|playlist| playlist.id.clone())
                    .collect::<Vec<_>>()
            };
            drop(data);
            let native_count = playlists.len();
            let create_index = native_count + unified_playlists.len();
            handle_command_for_list_popup(
                command,
                ui,
                if linking_unified {
                    native_count
                } else {
                    create_index + 1
                },
                |_, _| {},
                |ui: &mut UIStateGuard, id: usize| -> Result<()> {
                    if let Some(unified_playlist_id) = linking_unified_id.as_ref() {
                        let playlist = playlists
                            .get(id)
                            .context("invalid YouTube playlist selection")?;
                        dispatch_legacy_playlist(
                            client_pub,
                            ClientRequest::LinkUnifiedPlaylistToYouTube {
                                unified_playlist_id: unified_playlist_id.clone(),
                                youtube_playlist_id: playlist.id.clone(),
                            },
                        )?;
                        ui.popup = None;
                        return Ok(());
                    }
                    if id == create_index {
                        ui.popup = Some(PopupState::PlaylistCreate {
                            target: crate::state::PlaylistCreateTarget::YouTubeMusic,
                            public: false,
                            name: crate::ui::single_line_input::LineInput::default(),
                            desc: crate::ui::single_line_input::LineInput::default(),
                            current_field: crate::state::PlaylistCreateCurrentField::Name,
                            pending_items: Some(
                                tracks
                                    .iter()
                                    .map(crate::state::PlaylistSeedItem::from_youtube_track)
                                    .collect(),
                            ),
                            source_provider: Some(crate::state::Provider::YouTubeMusic),
                            source_epoch: Some(current_youtube_epoch(ui).value()),
                        });
                        return Ok(());
                    }
                    if let Some(playlist_id) = unified_playlists
                        .get(id.saturating_sub(native_count))
                        .filter(|_| id >= native_count)
                    {
                        if let Some(menu) = bulk_menu.as_ref() {
                            let plan = replan_youtube_menu_for_owner(
                                menu,
                                current_youtube_epoch(ui),
                                Action::AddToPlaylist,
                                crate::command::BulkActionOwner::UnifiedPlaylist,
                            )
                            .map_err(|_| anyhow::anyhow!("playlist target is stale"))?;
                            let seeds = menu
                                .items()
                                .iter()
                                .map(crate::state::PlaylistSeedItem::from_youtube_track)
                                .collect::<Vec<_>>();
                            let operation = new_unified_playlist_operation(
                                "append-bulk",
                                crate::state::Provider::YouTubeMusic,
                                current_youtube_epoch(ui).value(),
                                crate::state::PlaylistDestination::Existing {
                                    target: crate::state::PlaylistTargetKind::Unified,
                                    id: playlist_id.clone(),
                                },
                                crate::state::PlaylistIntent::Append {
                                    seed: seeds.clone(),
                                },
                                unified_playlist_revision(state, playlist_id),
                            );
                            let assignments = vec![super::bulk_action::BulkRequestAssignment::new(
                                playlist_request(ClientRequest::AddItemsToUnifiedPlaylist {
                                    playlist_id: playlist_id.clone(),
                                    items: seeds
                                        .into_iter()
                                        .map(crate::state::PlaylistSeedItem::into_unified_playlist_item)
                                        .collect(),
                                    operation: Some(operation),
                                })?,
                                plan.operation_ids(),
                            )];
                            super::bulk_action::dispatch_bulk_requests(
                                ui,
                                client_pub,
                                &plan,
                                assignments,
                            )?;
                        } else {
                            for track in &tracks {
                                let seed =
                                    crate::state::PlaylistSeedItem::from_youtube_track(track);
                                let operation = new_unified_playlist_operation(
                                    "append",
                                    crate::state::Provider::YouTubeMusic,
                                    current_youtube_epoch(ui).value(),
                                    crate::state::PlaylistDestination::Existing {
                                        target: crate::state::PlaylistTargetKind::Unified,
                                        id: playlist_id.clone(),
                                    },
                                    crate::state::PlaylistIntent::Append {
                                        seed: vec![seed.clone()],
                                    },
                                    unified_playlist_revision(state, playlist_id),
                                );
                                dispatch_legacy_playlist(
                                    client_pub,
                                    ClientRequest::AddItemsToUnifiedPlaylist {
                                        playlist_id: playlist_id.clone(),
                                        items: vec![seed.into_unified_playlist_item()],
                                        operation: Some(operation),
                                    },
                                )?;
                            }
                        }
                    } else {
                        let playlist = playlists
                            .get(id)
                            .context("invalid YouTube playlist selection")?;
                        if let Some(menu) = bulk_menu.as_ref() {
                            let plan = replan_youtube_menu_for_owner(
                                menu,
                                current_youtube_epoch(ui),
                                Action::AddToPlaylist,
                                crate::command::BulkActionOwner::Provider(
                                    crate::state::Provider::YouTubeMusic,
                                ),
                            )
                            .map_err(|_| anyhow::anyhow!("playlist target is stale"))?;
                            let assignments = plan
                                .operation_ids()
                                .into_iter()
                                .zip(menu.items().iter())
                                .map(|(operation_id, track)| {
                                    playlist_request(ClientRequest::AddYouTubeTrackToPlaylist {
                                        operation_id: provider_operation_id(operation_id),
                                        playlist_id: playlist.id.clone(),
                                        track: track.clone(),
                                    })
                                    .map(|request| {
                                        super::bulk_action::BulkRequestAssignment::one(
                                            request,
                                            operation_id,
                                        )
                                    })
                                })
                                .collect::<Result<Vec<_>>>()?;
                            super::bulk_action::dispatch_bulk_requests(
                                ui,
                                client_pub,
                                &plan,
                                assignments,
                            )?;
                        } else {
                            for track in &tracks {
                                dispatch_legacy_playlist(
                                    client_pub,
                                    ClientRequest::AddYouTubeTrackToPlaylist {
                                        operation_id: crate::client::PlaylistMutationOperationId(
                                            rand::rng().random(),
                                        ),
                                        playlist_id: playlist.id.clone(),
                                        track: track.clone(),
                                    },
                                )?;
                            }
                        }
                    }
                    window::clear_track_selection(ui);
                    ui.popup = None;
                    Ok(())
                },
                |ui: &mut UIStateGuard| {
                    ui.popup = None;
                },
            )
        }
        PopupState::UserFollowedArtistList(_) => {
            let artist_uris = state
                .data
                .read()
                .user_data
                .followed_artists
                .iter()
                .map(|a| a.id.uri())
                .collect::<Vec<_>>();

            handle_command_for_context_browsing_list_popup(
                command,
                ui,
                &artist_uris,
                &rspotify::model::Type::Artist,
            )
        }
        PopupState::UserSavedAlbumList(_) => {
            let album_uris = state
                .data
                .read()
                .user_data
                .saved_albums
                .iter()
                .map(|a| a.id.uri())
                .collect::<Vec<_>>();

            handle_command_for_context_browsing_list_popup(
                command,
                ui,
                &album_uris,
                &rspotify::model::Type::Album,
            )
        }
        PopupState::ThemeList(themes, _) => {
            let n_items = themes.len();

            handle_command_for_list_popup(
                command,
                ui,
                n_items,
                |ui: &mut UIStateGuard, id: usize| {
                    ui.theme = match ui.popup {
                        Some(PopupState::ThemeList(ref themes, _)) => themes[id].clone(),
                        _ => return,
                    };
                },
                |ui: &mut UIStateGuard, id: usize| -> Result<()> {
                    let Some(PopupState::ThemeList(themes, _)) = ui.popup.take() else {
                        return Ok(());
                    };
                    let theme = themes[id].clone();
                    let name = theme.name.clone();
                    ui.theme = theme;
                    let selected = selected_settings_index(ui, "theme");
                    page::save_settings_value(ui, "theme", &name, selected);
                    Ok(())
                },
                |ui: &mut UIStateGuard| {
                    ui.theme = match ui.popup {
                        Some(PopupState::ThemeList(ref themes, _)) => themes[0].clone(),
                        _ => return,
                    };
                    ui.popup = None;
                },
            )
        }
        PopupState::DeviceList(_) => {
            let player = state.player.read();

            handle_command_for_list_popup(
                command,
                ui,
                player.devices.len(),
                |_, _| {},
                |ui: &mut UIStateGuard, id: usize| -> Result<()> {
                    let is_playing = player.playback.as_ref().is_some_and(|p| p.is_playing);
                    client_pub.send(ClientRequest::Player(PlayerRequest::TransferPlayback(
                        player.devices[id].id.clone(),
                        is_playing,
                    )))?;
                    ui.popup = None;
                    Ok(())
                },
                |ui: &mut UIStateGuard| {
                    ui.popup = None;
                },
            )
        }
        PopupState::TrackRating { track, .. } => {
            let track = track.clone();
            handle_command_for_list_popup(
                command,
                ui,
                11,
                |_, _| {},
                |ui: &mut UIStateGuard, id: usize| -> Result<()> {
                    let rating = if id < 10 { Some((id + 1) as u8) } else { None };
                    update_track_journal(state, |journal| {
                        journal.set_rating(track.clone(), rating);
                    })?;
                    ui.popup = None;
                    Ok(())
                },
                |ui: &mut UIStateGuard| {
                    ui.popup = None;
                },
            )
        }
    }
}

fn handle_diagnostic_popup(
    key_sequence: &KeySequence,
    state: &SharedState,
    ui: &mut UIStateGuard,
) -> bool {
    if let Some(PopupState::DiagnosticActions { actions, .. }) = ui.popup.as_ref() {
        if let Some(Key::None(crossterm::event::KeyCode::Char(c))) = key_sequence.keys.first() {
            if let Some(index) = crate::command::one_based_digit_index(*c, actions.len()) {
                return execute_diagnostic_action(index, state, ui);
            }
        }
    }
    let Some(command) = config::get_config()
        .keymap_config
        .find_command_from_key_sequence(key_sequence)
    else {
        return false;
    };
    let count_prefix = ui.count_prefix.unwrap_or(1);
    match ui.popup.as_mut() {
        Some(PopupState::DiagnosticDetail {
            lines,
            scroll_offset,
            ..
        }) => handle_detail_scroll(command, scroll_offset, lines.len(), count_prefix).apply(ui),
        #[cfg(feature = "private-capture")]
        Some(PopupState::PrivateDerivativePreview {
            scroll_offset,
            rendered_row_count,
            ..
        }) => handle_detail_scroll(command, scroll_offset, *rendered_row_count, count_prefix)
            .apply(ui),
        Some(PopupState::DiagnosticActions {
            actions,
            state: list,
            ..
        }) => {
            let len = actions.len();
            let selected = list.selected().unwrap_or_default();
            let count = count_prefix;
            match command {
                Command::SelectNextOrScrollDown => {
                    list.select(Some((selected + count).min(len.saturating_sub(1))));
                    true
                }
                Command::SelectPreviousOrScrollUp => {
                    list.select(Some(selected.saturating_sub(count)));
                    true
                }
                Command::PageSelectNextOrScrollDown => {
                    list.select(Some(
                        (selected + config::get_config().app_config.page_size_in_rows * count)
                            .min(len.saturating_sub(1)),
                    ));
                    true
                }
                Command::PageSelectPreviousOrScrollUp => {
                    list.select(Some(selected.saturating_sub(
                        config::get_config().app_config.page_size_in_rows * count,
                    )));
                    true
                }
                Command::SelectFirstOrScrollToTop => {
                    list.select(Some(0));
                    true
                }
                Command::SelectLastOrScrollToBottom => {
                    list.select(Some(len.saturating_sub(1)));
                    true
                }
                Command::ChooseSelected => execute_diagnostic_action(selected, state, ui),
                Command::ClosePopup | Command::PreviousPage => {
                    ui.popup = None;
                    true
                }
                _ => false,
            }
        }
        _ => false,
    }
}

#[derive(Clone, Copy)]
enum DetailScrollOutcome {
    Handled,
    Close,
    Ignored,
}

impl DetailScrollOutcome {
    fn apply(self, ui: &mut UIStateGuard) -> bool {
        match self {
            Self::Handled => true,
            Self::Close => {
                ui.popup = None;
                true
            }
            Self::Ignored => false,
        }
    }
}

fn handle_detail_scroll(
    command: Command,
    scroll_offset: &mut usize,
    line_count: usize,
    count_prefix: usize,
) -> DetailScrollOutcome {
    match command {
        Command::SelectNextOrScrollDown => {
            *scroll_offset = (*scroll_offset + count_prefix).min(line_count.saturating_sub(1));
            DetailScrollOutcome::Handled
        }
        Command::SelectPreviousOrScrollUp => {
            *scroll_offset = scroll_offset.saturating_sub(count_prefix);
            DetailScrollOutcome::Handled
        }
        Command::PageSelectNextOrScrollDown => {
            *scroll_offset = (*scroll_offset + config::get_config().app_config.page_size_in_rows)
                .min(line_count.saturating_sub(1));
            DetailScrollOutcome::Handled
        }
        Command::PageSelectPreviousOrScrollUp => {
            *scroll_offset =
                scroll_offset.saturating_sub(config::get_config().app_config.page_size_in_rows);
            DetailScrollOutcome::Handled
        }
        Command::SelectFirstOrScrollToTop => {
            *scroll_offset = 0;
            DetailScrollOutcome::Handled
        }
        Command::SelectLastOrScrollToBottom => {
            *scroll_offset = line_count.saturating_sub(1);
            DetailScrollOutcome::Handled
        }
        Command::ClosePopup | Command::PreviousPage => DetailScrollOutcome::Close,
        _ => DetailScrollOutcome::Ignored,
    }
}

fn execute_diagnostic_action(index: usize, state: &SharedState, ui: &mut UIStateGuard) -> bool {
    let target = match ui.popup.as_ref() {
        Some(PopupState::DiagnosticActions { target, .. }) => target.clone(),
        _ => return false,
    };
    let Some(crate::state::PopupActionPayload::Diagnostic(action)) = ui
        .popup
        .as_ref()
        .and_then(|popup| popup.action_payload(index))
    else {
        return false;
    };
    match action {
        A::ExplainState => {
            let (title, explanation) = match target {
                R::ComponentLoading => (
                    "Component Health Loading",
                    "Runtime health has not published its first bounded fact yet.",
                ),
                R::WorkersEmpty => (
                    "Worker History Empty",
                    "No worker lifecycle transition is retained for this run yet.",
                ),
                R::OperationsEmpty => (
                    "Operation History Empty",
                    "No queued, running, or completed operation is retained for this run yet.",
                ),
                R::IncidentsEmpty => (
                    "Incident History Empty",
                    "No significant failure incident is retained for this run.",
                ),
                R::PlaybackRoute => (
                    "YouTube Playback Route",
                    "The route records the client order, selected client, and JavaScript runtime used for the latest resolved YouTube source.",
                ),
                _ => return false,
            };
            diagnostic_detail(ui, title, vec![explanation.to_owned()]);
        }
        A::IncidentSummary | A::CopyIncidentSummary | A::IncidentRunbook => {
            let R::Incident(reference) = &target else {
                return false;
            };
            let Some(incident) = state
                .diagnostics
                .incidents()
                .into_iter()
                .find(|incident| incident.reference == *reference)
            else {
                diagnostic_detail(
                    ui,
                    "Incident expired",
                    vec!["This bounded incident is no longer retained.".to_owned()],
                );
                return true;
            };
            if action == A::CopyIncidentSummary {
                let text = crate::observability::safe_incident_text(&incident);
                if let Err(error) = super::execute_copy_command(text) {
                    diagnostic_action_failure(&error, state, ui, "Clipboard unavailable");
                } else {
                    diagnostic_detail(
                        ui,
                        "Safe summary copied",
                        vec![
                            "Only newly constructed allowlisted incident fields were written."
                                .to_owned(),
                        ],
                    );
                }
            } else if action == A::IncidentRunbook {
                diagnostic_detail(
                    ui,
                    "Recommended Action",
                    vec![
                        incident.safe_next_action().to_owned(),
                        format!(
                            "Runbook: {}",
                            crate::observability::component_runbook(incident.component)
                        ),
                        "The diagnostics console does not retry or control playback.".to_owned(),
                    ],
                );
            } else {
                diagnostic_detail(
                    ui,
                    "Safe Incident Summary",
                    incident.render_lines().into_iter().collect(),
                );
            }
        }
        A::IncidentTimeline => {
            let R::Incident(reference) = &target else {
                return false;
            };
            show_timeline(reference.trim_start_matches("I-"), state, ui);
        }
        A::AcknowledgeIncident => {
            let R::Incident(reference) = &target else {
                return false;
            };
            let acknowledged = state.diagnostics.acknowledge_incident(reference);
            diagnostic_detail(
                ui,
                "Incident Acknowledgement",
                vec![if acknowledged {
                    "Acknowledged for this run. Evidence remains retained.".to_owned()
                } else {
                    "Incident expired before acknowledgement; no evidence was changed.".to_owned()
                }],
            );
        }
        A::FocusSupportBundle => {
            let R::Incident(reference) = &target else {
                return false;
            };
            if let PageState::Logs { state: page } = ui.current_page_mut() {
                page.support_focus_reference = Some(reference.clone());
            }
            diagnostic_detail(
                ui,
                "Support Focus",
                vec![
                    format!("Incident {reference} will focus the next focused bundle."),
                    "No evidence was exported or deleted.".to_owned(),
                ],
            );
        }
        A::HealthSummary
        | A::CopyHealthSummary
        | A::HealthRunbook
        | A::HealthHistory
        | A::RelatedIncidents => {
            execute_health_action(action, &target, state, ui);
        }
        A::FollowOperation => {
            let R::Operation(reference) = &target else {
                return false;
            };
            if let PageState::Logs { state: page } = ui.current_page_mut() {
                page.follow_reference = Some(reference.clone());
            }
            let outcome = state
                .diagnostics
                .timeline(reference)
                .and_then(|timeline| timeline.outcome);
            diagnostic_detail(
                ui,
                "Operation Follow",
                vec![match outcome {
                    Some(value) => format!(
                        "Operation already ended: {}.",
                        crate::observability::outcome_label(value)
                    ),
                    None => "Following this operation until one terminal outcome is recorded."
                        .to_owned(),
                }],
            );
        }
        A::OperationTimeline => {
            let R::Operation(reference) = &target else {
                return false;
            };
            show_timeline(reference, state, ui);
        }
        A::CopyOperationReference => {
            let R::Operation(reference) = &target else {
                return false;
            };
            let Some(text) = crate::observability::safe_reference_text(reference) else {
                return false;
            };
            if let Err(error) = super::execute_copy_command(text) {
                diagnostic_action_failure(&error, state, ui, "Clipboard unavailable");
            } else {
                diagnostic_detail(
                    ui,
                    "Reference Copied",
                    vec!["The allowlisted short operation reference was written.".to_owned()],
                );
            }
        }
        A::EnableTrace15 | A::EnableTrace30 | A::EnableTrace60 => {
            let seconds = match action {
                A::EnableTrace15 => 15,
                A::EnableTrace30 => 30,
                _ => 60,
            };
            let enabled = state
                .diagnostics
                .enable_verbose(std::time::Duration::from_secs(seconds));
            diagnostic_detail(
                ui,
                "Temporary Verbose Tracing",
                if enabled.is_some() {
                    vec![
                        format!("Verbose tracing enabled for {seconds} seconds."),
                        "Core typed causality evidence remains on at every filter level."
                            .to_owned(),
                        "Playback behavior is unchanged.".to_owned(),
                    ]
                } else {
                    vec![
                        "Verbose tracing is unavailable because diagnostics are disabled."
                            .to_owned(),
                        "Core typed causality evidence remains available for this run.".to_owned(),
                    ]
                },
            );
        }
        A::StopTrace => {
            let stopped = state.diagnostics.stop_verbose();
            diagnostic_detail(
                ui,
                "Temporary Verbose Tracing",
                vec![
                    if stopped {
                        "Temporary verbose tracing stopped.".to_owned()
                    } else {
                        "Temporary verbose tracing was already inactive.".to_owned()
                    },
                    "Core typed causality evidence remains on.".to_owned(),
                ],
            );
        }
        A::ExplainWriterHealth | A::ExplainDroppedEvents => {
            let health = state.diagnostics.health();
            let lines = if action == A::ExplainWriterHealth {
                vec![format!("Writer state: {:?}", health.state).to_ascii_lowercase(), format!("Files created: {}", health.files_created), format!("Bytes written: {}", health.bytes_written), "A degraded writer may leave support evidence incomplete; playback is unaffected.".to_owned()]
            } else {
                vec![
                    format!("Dropped events: {}", health.dropped_events),
                    if health.dropped_events == 0 {
                        "No diagnostic events have been dropped in this run.".to_owned()
                    } else {
                        "The bounded writer queue overflowed; support evidence may be incomplete."
                            .to_owned()
                    },
                ]
            };
            diagnostic_detail(ui, "Logging Health", lines);
        }
        A::PreviewBundle => {
            let focused = matches!(ui.current_page(), PageState::Logs { state: page } if page.support_focus_reference.is_some());
            diagnostic_detail(
                ui,
                "Support Bundle Preview",
                crate::observability::preview_support_bundle(focused),
            );
        }
        A::CreateGeneralBundle | A::CreateFocusedBundle => {
            let focus = if action == A::CreateFocusedBundle {
                match ui.current_page() {
                    PageState::Logs { state: page } => page.support_focus_reference.clone(),
                    _ => None,
                }
            } else {
                None
            };
            match state.diagnostics.create_support_bundle(focus.as_deref()) {
                Ok(review) => diagnostic_detail(
                    ui,
                    "Support Bundle Created",
                    crate::observability::safe_bundle_review_text(&review)
                        .lines()
                        .map(ToOwned::to_owned)
                        .collect(),
                ),
                Err(error) => {
                    diagnostic_action_failure(&error, state, ui, "Support bundle creation failed");
                }
            }
        }
        A::ReviewBundle | A::VerifyChecksums | A::ScanForbiddenData => {
            match state.diagnostics.review_support_bundle() {
                Ok(review) => {
                    let title = match action {
                        A::ReviewBundle => "Support Bundle Review",
                        A::VerifyChecksums => "Checksum Verification",
                        _ => "Forbidden-Data Scan",
                    };
                    diagnostic_detail(
                        ui,
                        title,
                        crate::observability::safe_bundle_review_text(&review)
                            .lines()
                            .map(ToOwned::to_owned)
                            .collect(),
                    );
                }
                Err(error) => {
                    diagnostic_action_failure(&error, state, ui, "Support bundle review failed");
                }
            }
        }
        A::OpenBundleFolder => match state.diagnostics.open_support_bundle_folder() {
            Ok(()) => diagnostic_detail(
                ui,
                "Bundle Folder",
                vec!["The local bundle folder was opened. Its path was not recorded.".to_owned()],
            ),
            Err(error) => {
                diagnostic_action_failure(&error, state, ui, "Opening folders is unavailable");
            }
        },
        A::CopyBundleReview => {
            let Some(review) = state.diagnostics.latest_support_review() else {
                diagnostic_detail(
                    ui,
                    "Bundle Review Unavailable",
                    vec!["Create and review a support bundle first.".to_owned()],
                );
                return true;
            };
            if let Err(error) =
                super::execute_copy_command(crate::observability::safe_bundle_review_text(&review))
            {
                diagnostic_action_failure(&error, state, ui, "Clipboard unavailable");
            } else {
                diagnostic_detail(
                    ui,
                    "Bundle Review Copied",
                    vec!["Only the allowlisted review result was written.".to_owned()],
                );
            }
        }
        A::ShowExceededBudgets => {
            let entries = state.diagnostics.performance_entries();
            let mut lines = entries
                .into_iter()
                .rev()
                .take(12)
                .map(|entry| {
                    format!(
                        "{}: {}ms / {}ms budget",
                        crate::observability::component_label(entry.component),
                        entry.duration_ms,
                        entry.budget_ms
                    )
                })
                .collect::<Vec<_>>();
            if lines.is_empty() {
                lines.push("No exceeded local budgets are retained in this run.".to_owned());
            }
            diagnostic_detail(ui, "Exceeded Local Budgets", lines);
        }
        A::ShowTimingTrends | A::CopyTrendSummary => match state.diagnostics.local_trend() {
            Ok(trend) => {
                let safe = crate::observability::bounded_trend_text(&trend);
                if action == A::CopyTrendSummary {
                    if let Err(error) = super::execute_copy_command(safe) {
                        diagnostic_action_failure(&error, state, ui, "Clipboard unavailable");
                    } else {
                        diagnostic_detail(
                            ui,
                            "Trend Summary Copied",
                            vec![
                                "A bounded allowlisted local trend summary was written.".to_owned()
                            ],
                        );
                    }
                } else {
                    diagnostic_detail(
                        ui,
                        "Retained Local Timing Trends",
                        safe.lines().map(ToOwned::to_owned).collect(),
                    );
                }
            }
            Err(error) => diagnostic_action_failure(&error, state, ui, "Local trend unavailable"),
        },
        #[cfg(feature = "private-capture")]
        action @ (A::ExplainCaptureSensitivity
        | A::ArmPrivateCapture
        | A::CancelPrivateCaptureConsent
        | A::DisarmPrivateCapture
        | A::ViewPrivateCaptureStatus
        | A::ReviewPrivateCapture
        | A::SelectPrivateCapture
        | A::MarkCaptureWorking
        | A::MarkCaptureFailing
        | A::ComparePrivateCaptures
        | A::ReplayPrivateCaptureOffline
        | A::ReplayPrivateCaptureFresh
        | A::PreviewPrivateDerivative
        | A::ViewPrivateDerivativePreview
        | A::CopyPrivateDerivativeReview
        | A::CreatePrivateDerivative
        | A::ReviewPrivateDerivative
        | A::OpenPrivateCaptureFolder
        | A::DeletePrivateCapture
        | A::PurgeExpiredPrivateCaptures) => {
            if target != R::PrivateCapture {
                return false;
            }
            execute_private_capture_action(action, state, ui);
        }
    }
    true
}

#[cfg(feature = "private-capture")]
fn execute_private_capture_action(
    action: crate::observability::DiagnosticAction,
    state: &SharedState,
    ui: &mut UIStateGuard,
) {
    use crate::developer_capture::SafeArtifactLabel;

    if action == A::ExplainCaptureSensitivity {
        diagnostic_detail(
            ui,
            "Private Capture Sensitivity",
            vec![
                "Private captures can contain provider requests, responses, and authentication material.".to_owned(),
                "Captures stay encrypted in a separate local vault and never enter diagnostics, logs, clipboard text, or support bundles.".to_owned(),
                "Arming applies to one manual YouTube playback attempt and expires automatically.".to_owned(),
                "Offline replay sends no network request. Fresh replay requires two confirmations.".to_owned(),
            ],
        );
        return;
    }
    if action == A::ViewPrivateCaptureStatus {
        diagnostic_detail(
            ui,
            "Safe Private Capture Status",
            private_capture_status_lines(&state.private_capture_operator_snapshot()),
        );
        return;
    }
    if action == A::ViewPrivateDerivativePreview {
        let snapshot = state.private_capture_operator_snapshot();
        let Some(preview) = snapshot
            .derivative
            .as_ref()
            .and_then(crate::developer_capture::SafeDerivativeView::preview_shared)
        else {
            diagnostic_detail(
                ui,
                "Derivative Preview Unavailable",
                vec!["Prepare a safe derivative preview first.".to_owned()],
            );
            return;
        };
        ui.popup = Some(PopupState::PrivateDerivativePreview {
            preview,
            scroll_offset: 0,
            rendered_row_count: 1,
        });
        return;
    }
    if action == A::CopyPrivateDerivativeReview {
        let snapshot = state.private_capture_operator_snapshot();
        let Some(derivative) = snapshot.derivative.as_ref() else {
            diagnostic_detail(
                ui,
                "Derivative Review Unavailable",
                vec!["Prepare a safe derivative preview first.".to_owned()],
            );
            return;
        };
        if let Err(error) = super::execute_copy_command(derivative.safe_review_copy_text()) {
            diagnostic_action_failure(&error, state, ui, "Clipboard unavailable");
        } else {
            diagnostic_detail(
                ui,
                "Derivative Review Copied",
                vec![
                    "The bounded allowlisted derivative review was written to the clipboard."
                        .to_owned(),
                ],
            );
        }
        return;
    }

    let Some(operator) = state.private_capture_operator() else {
        private_capture_operator_unavailable(ui);
        return;
    };
    match action {
        A::ArmPrivateCapture => {
            if private_capture_submission_accepted(&operator.request_arm()) {
                ui.popup = Some(PopupState::PrivateCapturePassphrase {
                    action: crate::state::PrivateCapturePassphraseAction::Arm,
                    input: crate::developer_capture::CapturePassphraseInput::new(),
                });
            } else {
                private_capture_operator_busy(ui);
            }
        }
        A::CancelPrivateCaptureConsent => {
            private_capture_submit_with_feedback(
                operator.cancel_consent().is_ok(),
                ui,
                "Capture Consent",
                "Consent cancellation was accepted.",
            );
        }
        A::DisarmPrivateCapture => {
            private_capture_submit_with_feedback(
                operator.disarm().is_ok(),
                ui,
                "Private Capture",
                "Disarm was accepted. Playback remains unaffected.",
            );
        }
        A::SelectPrivateCapture => {
            let snapshot = operator.snapshot();
            if snapshot.artifacts.is_empty() {
                diagnostic_detail(
                    ui,
                    "Private Capture Selection",
                    vec!["No retained encrypted capture is available.".to_owned()],
                );
            } else {
                ui.popup = Some(PopupState::PrivateCaptureSelector {
                    state: crate::state::PrivateCaptureSelectorState::new(
                        &snapshot.artifacts,
                        snapshot.selected,
                    ),
                });
            }
        }
        A::MarkCaptureWorking | A::MarkCaptureFailing => {
            let label = if action == A::MarkCaptureWorking {
                SafeArtifactLabel::Working
            } else {
                SafeArtifactLabel::Failing
            };
            private_capture_submit_with_feedback(
                operator.label_selected(label).is_ok(),
                ui,
                "Capture Label",
                if label == SafeArtifactLabel::Working {
                    "The selected safe reference will be marked working."
                } else {
                    "The selected safe reference will be marked failing."
                },
            );
        }
        A::ReviewPrivateCapture => open_private_capture_passphrase(
            ui,
            crate::state::PrivateCapturePassphraseAction::ReviewCapture,
        ),
        A::ComparePrivateCaptures => open_private_capture_passphrase(
            ui,
            crate::state::PrivateCapturePassphraseAction::Compare,
        ),
        A::ReplayPrivateCaptureOffline => open_private_capture_passphrase(
            ui,
            crate::state::PrivateCapturePassphraseAction::ReplayOffline,
        ),
        A::ReplayPrivateCaptureFresh => {
            ui.popup = Some(PopupState::PrivateCaptureConfirm {
                action: crate::state::PrivateCaptureConfirmation::FreshReplayFirst,
            });
        }
        A::PreviewPrivateDerivative => open_private_capture_passphrase(
            ui,
            crate::state::PrivateCapturePassphraseAction::PreviewDerivative,
        ),
        A::CreatePrivateDerivative => open_private_capture_passphrase(
            ui,
            crate::state::PrivateCapturePassphraseAction::CreateDerivative,
        ),
        A::ReviewPrivateDerivative => {
            private_capture_submit_with_feedback(
                operator.review_derivative().is_ok(),
                ui,
                "Derivative Review",
                "Safe checksum and forbidden-data review was requested.",
            );
        }
        A::OpenPrivateCaptureFolder => {
            ui.popup = Some(PopupState::PrivateCaptureConfirm {
                action: crate::state::PrivateCaptureConfirmation::OpenEncryptedFolder,
            });
        }
        A::DeletePrivateCapture => {
            ui.popup = Some(PopupState::PrivateCaptureConfirm {
                action: crate::state::PrivateCaptureConfirmation::DeleteSelected,
            });
        }
        A::PurgeExpiredPrivateCaptures => {
            private_capture_submit_with_feedback(
                operator.purge_expired().is_ok(),
                ui,
                "Capture Retention",
                "Expired-capture maintenance was requested.",
            );
        }
        _ => {}
    }
}

#[cfg(feature = "private-capture")]
fn handle_private_capture_popup(
    key_sequence: &KeySequence,
    state: &SharedState,
    ui: &mut UIStateGuard,
) -> bool {
    match ui.popup {
        Some(PopupState::PrivateCapturePassphrase { .. }) => {
            handle_private_capture_passphrase(key_sequence, state, ui)
        }
        Some(PopupState::PrivateCaptureConfirm { .. }) => {
            handle_private_capture_confirmation(key_sequence, state, ui)
        }
        Some(PopupState::PrivateCaptureSelector { .. }) => {
            handle_private_capture_selector(key_sequence, state, ui)
        }
        _ => false,
    }
}

#[cfg(feature = "private-capture")]
fn handle_private_capture_passphrase(
    key_sequence: &KeySequence,
    state: &SharedState,
    ui: &mut UIStateGuard,
) -> bool {
    let [Key::None(key)] = key_sequence.keys.as_slice() else {
        return true;
    };
    match key {
        crossterm::event::KeyCode::Char(character) => {
            let Some(PopupState::PrivateCapturePassphrase { input, .. }) = ui.popup.as_mut() else {
                return false;
            };
            input.push(*character);
            true
        }
        crossterm::event::KeyCode::Backspace => {
            let Some(PopupState::PrivateCapturePassphrase { input, .. }) = ui.popup.as_mut() else {
                return false;
            };
            input.pop();
            true
        }
        crossterm::event::KeyCode::Esc => {
            ui.popup = None;
            true
        }
        crossterm::event::KeyCode::Enter => {
            let Some(PopupState::PrivateCapturePassphrase { action, input }) = ui.popup.take()
            else {
                return false;
            };
            let Ok(passphrase) = input.into_passphrase() else {
                diagnostic_detail(
                    ui,
                    "Passphrase Required",
                    vec!["Enter a non-empty private passphrase and try again.".to_owned()],
                );
                return true;
            };
            let Some(operator) = state.private_capture_operator() else {
                private_capture_operator_unavailable(ui);
                return true;
            };
            let accepted = match action {
                crate::state::PrivateCapturePassphraseAction::Arm => {
                    private_capture_submission_accepted(&operator.accept_consent(passphrase))
                }
                crate::state::PrivateCapturePassphraseAction::ReviewCapture => {
                    private_capture_submission_accepted(&operator.review_selected(passphrase))
                }
                crate::state::PrivateCapturePassphraseAction::Compare => {
                    private_capture_submission_accepted(&operator.compare_labeled(passphrase))
                }
                crate::state::PrivateCapturePassphraseAction::ReplayOffline => {
                    private_capture_submission_accepted(
                        &operator.replay_offline_selected(passphrase),
                    )
                }
                crate::state::PrivateCapturePassphraseAction::ReplayFresh => {
                    private_capture_submission_accepted(&operator.replay_fresh_selected(
                        passphrase,
                        crate::developer_capture::FreshReplayAcknowledgement::confirmed(),
                    ))
                }
                crate::state::PrivateCapturePassphraseAction::PreviewDerivative => {
                    private_capture_submission_accepted(
                        &operator.preview_derivative_selected(passphrase),
                    )
                }
                crate::state::PrivateCapturePassphraseAction::CreateDerivative => {
                    private_capture_submission_accepted(
                        &operator.create_derivative_selected_default(passphrase),
                    )
                }
            };
            if !accepted {
                private_capture_operator_busy(ui);
            } else if action == crate::state::PrivateCapturePassphraseAction::Arm {
                ui.popup = None;
                if ui.history.len() > 1 {
                    ui.history.pop();
                }
                ui.bump_diagnostic_revision();
            } else {
                diagnostic_detail(
                    ui,
                    action.title(),
                    vec![
                        "The bounded local action was accepted.".to_owned(),
                        "Its safe terminal result will appear in Diagnostics.".to_owned(),
                    ],
                );
            }
            true
        }
        _ => true,
    }
}

#[cfg(feature = "private-capture")]
fn handle_private_capture_confirmation(
    key_sequence: &KeySequence,
    state: &SharedState,
    ui: &mut UIStateGuard,
) -> bool {
    let Some(PopupState::PrivateCaptureConfirm { action }) = ui.popup.as_ref() else {
        return false;
    };
    let action = *action;
    match private_capture_confirmation_input(key_sequence) {
        PrivateCaptureConfirmationInput::Confirm => match action {
            crate::state::PrivateCaptureConfirmation::FreshReplayFirst => {
                ui.popup = Some(PopupState::PrivateCaptureConfirm {
                    action: crate::state::PrivateCaptureConfirmation::FreshReplaySecond,
                });
            }
            crate::state::PrivateCaptureConfirmation::FreshReplaySecond => {
                open_private_capture_passphrase(
                    ui,
                    crate::state::PrivateCapturePassphraseAction::ReplayFresh,
                );
            }
            crate::state::PrivateCaptureConfirmation::OpenEncryptedFolder => {
                let Some(operator) = state.private_capture_operator() else {
                    private_capture_operator_unavailable(ui);
                    return true;
                };
                private_capture_submit_with_feedback(
                    operator
                        .open_private_folder(
                            crate::developer_capture::SensitiveFolderAcknowledgement::confirmed(),
                        )
                        .is_ok(),
                    ui,
                    "Encrypted Capture Folder",
                    "The local encrypted-capture folder will be opened without recording its path.",
                );
            }
            crate::state::PrivateCaptureConfirmation::DeleteSelected => {
                let Some(operator) = state.private_capture_operator() else {
                    private_capture_operator_unavailable(ui);
                    return true;
                };
                private_capture_submit_with_feedback(
                    operator.delete_selected().is_ok(),
                    ui,
                    "Delete Private Capture",
                    "Deletion of the selected encrypted capture was accepted.",
                );
            }
        },
        PrivateCaptureConfirmationInput::Cancel => {
            ui.popup = None;
        }
        PrivateCaptureConfirmationInput::Ignore => {}
    }
    true
}

#[cfg(feature = "private-capture")]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum PrivateCaptureConfirmationInput {
    Confirm,
    Cancel,
    Ignore,
}

#[cfg(feature = "private-capture")]
fn private_capture_confirmation_input(
    key_sequence: &KeySequence,
) -> PrivateCaptureConfirmationInput {
    match key_sequence.keys.as_slice() {
        [Key::None(crossterm::event::KeyCode::Char('y'))] => {
            PrivateCaptureConfirmationInput::Confirm
        }
        [Key::None(crossterm::event::KeyCode::Char('n') | crossterm::event::KeyCode::Esc)] => {
            PrivateCaptureConfirmationInput::Cancel
        }
        _ => PrivateCaptureConfirmationInput::Ignore,
    }
}

#[cfg(feature = "private-capture")]
fn handle_private_capture_selector(
    key_sequence: &KeySequence,
    state: &SharedState,
    ui: &mut UIStateGuard,
) -> bool {
    let Some(command) = config::get_config()
        .keymap_config
        .find_command_from_key_sequence(key_sequence)
    else {
        return true;
    };
    let snapshot = state.private_capture_operator_snapshot();
    let count = ui.count_prefix.unwrap_or(1);
    let Some(PopupState::PrivateCaptureSelector { state: selector }) = ui.popup.as_mut() else {
        return false;
    };
    selector.synchronize(&snapshot.artifacts);
    let selected = selector.list.selected().unwrap_or_default();
    let last = snapshot.artifacts.len().saturating_sub(1);
    match command {
        Command::SelectNextOrScrollDown => {
            selector.select_index(
                &snapshot.artifacts,
                selected.saturating_add(count).min(last),
            );
        }
        Command::SelectPreviousOrScrollUp => {
            selector.select_index(&snapshot.artifacts, selected.saturating_sub(count));
        }
        Command::PageSelectNextOrScrollDown => {
            selector.select_index(
                &snapshot.artifacts,
                selected
                    .saturating_add(config::get_config().app_config.page_size_in_rows * count)
                    .min(last),
            );
        }
        Command::PageSelectPreviousOrScrollUp => {
            selector.select_index(
                &snapshot.artifacts,
                selected.saturating_sub(config::get_config().app_config.page_size_in_rows * count),
            );
        }
        Command::SelectFirstOrScrollToTop => selector.select_index(&snapshot.artifacts, 0),
        Command::SelectLastOrScrollToBottom => {
            selector.select_index(&snapshot.artifacts, last);
        }
        Command::ChooseSelected => {
            let capture_ref = selector.selected;
            let Some(operator) = state.private_capture_operator() else {
                private_capture_operator_unavailable(ui);
                return true;
            };
            private_capture_submit_with_feedback(
                operator.select(capture_ref).is_ok(),
                ui,
                "Private Capture Selection",
                "The selected safe reference was submitted.",
            );
        }
        Command::ClosePopup | Command::PreviousPage => ui.popup = None,
        _ => return false,
    }
    true
}

#[cfg(feature = "private-capture")]
fn open_private_capture_passphrase(
    ui: &mut UIStateGuard,
    action: crate::state::PrivateCapturePassphraseAction,
) {
    ui.popup = Some(PopupState::PrivateCapturePassphrase {
        action,
        input: crate::developer_capture::CapturePassphraseInput::new(),
    });
}

#[cfg(feature = "private-capture")]
fn private_capture_submission_accepted<T, E>(result: &std::result::Result<T, E>) -> bool {
    result.is_ok()
}

#[cfg(feature = "private-capture")]
fn private_capture_submit_with_feedback(
    accepted_result: bool,
    ui: &mut UIStateGuard,
    title: &str,
    accepted: &str,
) {
    if accepted_result {
        diagnostic_detail(
            ui,
            title,
            vec![
                accepted.to_owned(),
                "The safe terminal result will appear in Diagnostics.".to_owned(),
            ],
        );
    } else {
        private_capture_operator_busy(ui);
    }
}

#[cfg(feature = "private-capture")]
fn private_capture_operator_unavailable(ui: &mut UIStateGuard) {
    diagnostic_detail(
        ui,
        "Private Capture Unavailable",
        vec![
            "The bounded private-capture operator is not available.".to_owned(),
            "Playback and application state are unaffected.".to_owned(),
        ],
    );
}

#[cfg(feature = "private-capture")]
fn private_capture_operator_busy(ui: &mut UIStateGuard) {
    diagnostic_detail(
        ui,
        "Private Capture Action Rejected",
        vec![
            "The bounded private-capture operator is busy or unavailable.".to_owned(),
            "Wait for the active local action to finish, then retry.".to_owned(),
            "Playback and application state are unaffected.".to_owned(),
        ],
    );
}

#[cfg(feature = "private-capture")]
fn private_capture_status_lines(
    snapshot: &crate::developer_capture::SafeOperatorSnapshot,
) -> Vec<String> {
    let mut lines = vec![
        format!("Operator: {}", snapshot.phase.as_str()),
        format!(
            "Capture: {} / completeness={}",
            safe_capture_state_label(snapshot.capture.state),
            capture_completeness_label(snapshot.capture.completeness)
        ),
        format!(
            "Records: {} / dropped={} / size={}",
            snapshot.capture.record_count,
            snapshot.capture.dropped_records,
            capture_byte_bucket_label(snapshot.capture.byte_bucket)
        ),
        format!(
            "Retained: {}{}",
            snapshot.artifacts.len(),
            if snapshot.artifact_overflow {
                " / additional references omitted"
            } else {
                ""
            }
        ),
    ];
    if let Some(remaining) = snapshot.capture.remaining_seconds {
        lines.push(format!("Consent expiry: {remaining}s"));
    }
    if let Some(capture_ref) = snapshot.selected {
        lines.push(format!("Selected safe reference: {capture_ref}"));
    }
    if let Some(result) = snapshot.last_result {
        lines.push(format!(
            "Last action: {} / {}",
            result.action.as_str(),
            result.disposition_str()
        ));
    }
    if let Some(review) = snapshot.review {
        lines.extend([
            format!(
                "Review: {} / records={} / checksum={}",
                review.capture_ref,
                review.record_count,
                yes_no(review.checksum_valid)
            ),
            format!(
                "Review completeness: {} / terminal={}",
                capture_completeness_label(review.completeness),
                terminal_category_label(review.terminal_category)
            ),
        ]);
    }
    if let Some(comparison) = snapshot.comparison {
        lines.push(format!(
            "Comparison: {} finding(s) / incomplete={} / dropped={}",
            comparison.finding_count,
            yes_no(comparison.incomplete),
            comparison.dropped_findings
        ));
        let categories = comparison
            .category_labels()
            .into_iter()
            .flatten()
            .collect::<Vec<_>>()
            .join(", ");
        if !categories.is_empty() {
            lines.push(format!("Comparison categories: {categories}"));
        }
    }
    if let Some(replay) = snapshot.replay {
        lines.push(format!(
            "Replay: {} / {} / terminal={} / records={}",
            match replay.mode {
                crate::developer_capture::ReplayMode::Offline => "offline",
                crate::developer_capture::ReplayMode::Fresh => "fresh",
            },
            replay.outcome.as_str(),
            terminal_category_label(replay.terminal_category),
            replay.record_count
        ));
    }
    if let Some(derivative) = snapshot.derivative.as_ref() {
        lines.push(format!(
            "Derivative: {} / schema={} / checksum={} / forbidden-scan={} / durable={}",
            derivative.state_str(),
            derivative.schema_version,
            yes_no(derivative.checksum_valid),
            yes_no(derivative.forbidden_scan_passed),
            derivative
                .durability_confirmed
                .map_or("not applicable", yes_no)
        ));
        lines.push(format!(
            "Derivative completeness: {} / size={}",
            derivative.completeness_str(),
            derivative.byte_bucket_str()
        ));
    }
    lines
}

#[cfg(feature = "private-capture")]
const fn safe_capture_state_label(
    state: crate::developer_capture::SafeCaptureState,
) -> &'static str {
    use crate::developer_capture::SafeCaptureState as S;
    match state {
        S::Inactive => "inactive",
        S::ConsentRequired => "consent required",
        S::Armed => "armed",
        S::Claimed => "claimed",
        S::Capturing => "capturing",
        S::Finalizing => "finalizing",
        S::Ready => "ready",
        S::Incomplete => "incomplete",
        S::Expired => "expired",
        S::Failed => "failed",
    }
}

#[cfg(feature = "private-capture")]
const fn capture_completeness_label(
    completeness: crate::developer_capture::CaptureCompleteness,
) -> &'static str {
    use crate::developer_capture::CaptureCompleteness as C;
    match completeness {
        C::Pending => "pending",
        C::Complete => "complete",
        C::Incomplete => "incomplete",
    }
}

#[cfg(feature = "private-capture")]
const fn capture_byte_bucket_label(
    bucket: crate::developer_capture::CaptureByteBucket,
) -> &'static str {
    use crate::developer_capture::CaptureByteBucket as B;
    match bucket {
        B::Empty => "empty",
        B::Under64KiB => "under 64 KiB",
        B::Under1MiB => "under 1 MiB",
        B::Under4MiB => "under 4 MiB",
        B::Under16MiB => "under 16 MiB",
        B::AtOrOver16MiB => "at least 16 MiB",
    }
}

#[cfg(feature = "private-capture")]
const fn terminal_category_label(
    terminal: crate::developer_capture::SafeTerminalCategory,
) -> &'static str {
    use crate::developer_capture::SafeTerminalCategory as T;
    match terminal {
        T::Success => "success",
        T::Failed => "failed",
        T::Cancelled => "cancelled",
        T::Superseded => "superseded",
        T::TimedOut => "timed out",
        T::Panicked => "failed safely",
    }
}

#[cfg(feature = "private-capture")]
const fn yes_no(value: bool) -> &'static str {
    if value {
        "yes"
    } else {
        "no"
    }
}

fn show_timeline(reference: &str, state: &SharedState, ui: &mut UIStateGuard) {
    let lines = state.diagnostics.timeline(reference).map_or_else(
        || vec!["No retained timeline exists for this bounded reference.".to_owned()],
        |timeline| {
            let mut lines = vec![format!("{} / {}", timeline.reference, timeline.operation)];
            lines.extend(
                timeline
                    .entries
                    .iter()
                    .map(crate::observability::TimelineEntry::render),
            );
            lines.push(format!(
                "Terminal outcome: {}",
                timeline
                    .outcome
                    .map_or("still running", crate::observability::outcome_label)
            ));
            lines
        },
    );
    diagnostic_detail(ui, "Operation Timeline", lines);
}

fn execute_health_action(
    action: crate::observability::DiagnosticAction,
    target: &crate::observability::DiagnosticRowId,
    state: &SharedState,
    ui: &mut UIStateGuard,
) {
    let snapshot = state.diagnostics.health_snapshot();
    let now = state
        .diagnostics
        .started_at()
        .elapsed()
        .as_millis()
        .try_into()
        .unwrap_or(u64::MAX);
    let (label, status, fact, updated, component, history) = match target {
        R::Component(component) => {
            let Some(item) = snapshot
                .components
                .iter()
                .find(|item| item.component == *component)
            else {
                return;
            };
            (
                crate::observability::component_label(*component),
                item.status,
                item.fact.as_str(),
                item.updated_uptime_ms,
                *component,
                state.diagnostics.component_history(*component),
            )
        }
        R::Worker(worker) => {
            let Some(item) = snapshot.workers.iter().find(|item| item.worker == *worker) else {
                return;
            };
            let label = crate::observability::safe_worker_label(worker).to_owned();
            let history = state.diagnostics.worker_history(worker);
            if action == A::HealthHistory {
                let mut lines = history
                    .iter()
                    .rev()
                    .map(|item| crate::observability::safe_transition_line(item, now))
                    .collect::<Vec<_>>();
                if lines.is_empty() {
                    lines.push("No retained transitions for this worker.".to_owned());
                }
                diagnostic_detail(ui, "Health Transition History", lines);
                return;
            }
            let text = crate::observability::safe_health_text(
                &label,
                item.status,
                "lifecycle-transition",
                now.saturating_sub(item.updated_uptime_ms),
            );
            if action == A::CopyHealthSummary {
                if let Err(error) = super::execute_copy_command(text) {
                    diagnostic_action_failure(&error, state, ui, "Clipboard unavailable");
                    return;
                }
                diagnostic_detail(
                    ui,
                    "Health Summary Copied",
                    vec!["Only allowlisted health facts were written.".to_owned()],
                );
                return;
            }
            if action == A::RelatedIncidents {
                show_related_incidents(crate::observability::Component::Runtime, state, ui);
                return;
            }
            if action == A::HealthRunbook {
                diagnostic_detail(
                    ui,
                    "Worker Runbook",
                    vec![
                        "Runbook: runtime-lifecycle".to_owned(),
                        "Restart the application only if the worker remains degraded.".to_owned(),
                    ],
                );
                return;
            }
            diagnostic_detail(
                ui,
                "Current Worker Health",
                text.lines().map(ToOwned::to_owned).collect(),
            );
            return;
        }
        _ => return,
    };
    if action == A::HealthHistory {
        let mut lines = history
            .iter()
            .rev()
            .map(|item| crate::observability::safe_transition_line(item, now))
            .collect::<Vec<_>>();
        if lines.is_empty() {
            lines.push("No retained transitions for this component.".to_owned());
        }
        diagnostic_detail(ui, "Health Transition History", lines);
    } else if action == A::RelatedIncidents {
        show_related_incidents(component, state, ui);
    } else if action == A::HealthRunbook {
        diagnostic_detail(
            ui,
            "Component Runbook",
            vec![
                format!(
                    "Runbook: {}",
                    crate::observability::component_runbook(component)
                ),
                "Use the recommended incident action; diagnostics does not control the component."
                    .to_owned(),
            ],
        );
    } else {
        let text = crate::observability::safe_health_text(
            label,
            status,
            fact,
            now.saturating_sub(updated),
        );
        if action == A::CopyHealthSummary {
            if let Err(error) = super::execute_copy_command(text) {
                diagnostic_action_failure(&error, state, ui, "Clipboard unavailable");
            } else {
                diagnostic_detail(
                    ui,
                    "Health Summary Copied",
                    vec!["Only allowlisted health facts were written.".to_owned()],
                );
            }
        } else {
            diagnostic_detail(
                ui,
                "Current Component Health",
                text.lines().map(ToOwned::to_owned).collect(),
            );
        }
    }
}

fn show_related_incidents(
    component: crate::observability::Component,
    state: &SharedState,
    ui: &mut UIStateGuard,
) {
    let mut lines = state
        .diagnostics
        .incidents()
        .into_iter()
        .rev()
        .filter(|incident| incident.component == component)
        .take(8)
        .map(|incident| {
            format!(
                "{} / {} / {}",
                incident.safe_reference(),
                incident.safe_event_code(),
                incident.safe_operation()
            )
        })
        .collect::<Vec<_>>();
    if lines.is_empty() {
        lines.push("No related significant incidents are retained in this run.".to_owned());
    }
    diagnostic_detail(ui, "Related Incidents", lines);
}

fn diagnostic_detail(ui: &mut UIStateGuard, title: &str, lines: Vec<String>) {
    ui.popup = Some(PopupState::DiagnosticDetail {
        title: crate::observability::safe_detail_text(title),
        lines: lines
            .into_iter()
            .map(|line| crate::observability::safe_detail_text(&line))
            .collect(),
        scroll_offset: 0,
    });
}

fn diagnostic_action_failure(
    _error: &anyhow::Error,
    state: &SharedState,
    ui: &mut UIStateGuard,
    summary: &str,
) {
    let incident = state.diagnostics.record_local_action_failure();
    state.diagnostics.set_component_health(
        crate::observability::Component::Support,
        crate::observability::HealthStatus::Degraded,
        "recent-failure",
    );
    diagnostic_detail(
        ui,
        summary,
        diagnostic_action_failure_lines(incident.as_ref()),
    );
}

fn diagnostic_action_failure_lines(
    incident: Option<&crate::observability::IncidentSummary>,
) -> Vec<String> {
    let mut lines = vec![
        "The local diagnostic action did not complete.".to_owned(),
        "Playback and application state were not affected.".to_owned(),
    ];
    if let Some(incident) = incident {
        lines.extend(incident.render_lines());
    } else {
        lines.extend([
            "Cause: A required local resource was unavailable | Retryable: yes".to_owned(),
            "Next: Retry once; review Diagnostics if it repeats".to_owned(),
            "Event code: DIAGNOSTIC_ACTION_FAILED | Incident reference unavailable".to_owned(),
            "Support=degraded".to_owned(),
        ]);
    }
    lines
}

#[cfg(test)]
mod diagnostic_action_failure_tests {
    use super::diagnostic_action_failure_lines;
    use crate::command::one_based_digit_index;
    use crate::observability::{Component, IncidentSummary};

    #[test]
    fn handled_failure_summary_contains_safe_actionable_incident_evidence() {
        let incident = IncidentSummary {
            reference: "I-ab12cd34".to_owned(),
            event_code: "DIAGNOSTIC_ACTION_FAILED".to_owned(),
            operation: "Live diagnostics".to_owned(),
            provider: None,
            impact: "Playback is unaffected; diagnostic evidence may be incomplete".to_owned(),
            cause: "A required local resource was unavailable".to_owned(),
            retryable: true,
            next_action: "Check the local device or helper, then retry".to_owned(),
            component: Component::Support,
            component_health: "degraded".to_owned(),
            occurrence_count: 1,
        };
        let rendered = diagnostic_action_failure_lines(Some(&incident)).join("\n");
        for expected in [
            "local diagnostic action did not complete",
            "I-ab12cd34",
            "DIAGNOSTIC_ACTION_FAILED",
            "Cause:",
            "Retryable: yes",
            "Next:",
            "Support=degraded",
        ] {
            assert!(rendered.contains(expected));
        }
        for forbidden in ["C:\\private", "https://", "query=", "token="] {
            assert!(!rendered.contains(forbidden));
        }
    }

    #[test]
    fn diagnostic_numeric_shortcuts_match_one_based_display_labels() {
        assert_eq!(one_based_digit_index('1', 3), Some(0));
        assert_eq!(one_based_digit_index('3', 3), Some(2));
        assert_eq!(one_based_digit_index('0', 3), None);
        assert_eq!(one_based_digit_index('4', 3), None);
        assert_eq!(one_based_digit_index('x', 3), None);
    }

    #[cfg(feature = "private-capture")]
    #[test]
    fn numeric_shortcuts_cannot_confirm_private_actions() {
        use super::{private_capture_confirmation_input, PrivateCaptureConfirmationInput};
        use crate::key::{Key, KeySequence};
        use crossterm::event::KeyCode;

        for key in ['0', '1', '2', '9'] {
            assert_eq!(
                private_capture_confirmation_input(&KeySequence {
                    keys: vec![Key::None(KeyCode::Char(key))]
                }),
                PrivateCaptureConfirmationInput::Ignore
            );
        }
        assert_eq!(
            private_capture_confirmation_input(&KeySequence {
                keys: vec![Key::None(KeyCode::Char('y'))]
            }),
            PrivateCaptureConfirmationInput::Confirm
        );
        assert_eq!(
            private_capture_confirmation_input(&KeySequence {
                keys: vec![Key::None(KeyCode::Esc)]
            }),
            PrivateCaptureConfirmationInput::Cancel
        );
    }
}

fn popup_text_is_blank(text: &str) -> bool {
    text.trim().is_empty()
}

fn lyrics_plain_text(lyrics: &crate::state::Lyrics) -> String {
    match &lyrics.lines {
        crate::state::LyricsLines::Plain(lines) => lines.join("\n"),
        crate::state::LyricsLines::Synced(lines) => lines
            .iter()
            .map(|(_, line)| line.as_str())
            .collect::<Vec<_>>()
            .join("\n"),
        crate::state::LyricsLines::Rich(lines) => lines
            .iter()
            .map(|line| line.text.as_str())
            .collect::<Vec<_>>()
            .join("\n"),
    }
}

fn lyrics_timed_lrc(lyrics: &crate::state::Lyrics) -> Option<String> {
    let lines = match &lyrics.lines {
        crate::state::LyricsLines::Synced(lines) => lines
            .iter()
            .map(|(timestamp, line)| (*timestamp, line.as_str()))
            .collect::<Vec<_>>(),
        crate::state::LyricsLines::Rich(lines) => lines
            .iter()
            .map(|line| (line.start, line.text.as_str()))
            .collect::<Vec<_>>(),
        crate::state::LyricsLines::Plain(_) => return None,
    };
    let mut output = String::new();
    for (timestamp, line) in lines {
        let millis = timestamp.num_milliseconds().max(0);
        let minutes = millis / 60_000;
        let seconds = (millis % 60_000) / 1_000;
        let centiseconds = (millis % 1_000) / 10;
        writeln!(
            output,
            "[{minutes:02}:{seconds:02}.{centiseconds:02}]{line}"
        )
        .expect("writing lyrics to a String cannot fail");
    }
    (!output.is_empty()).then_some(output)
}

#[derive(Debug, PartialEq, Eq)]
enum YouTubeArtistMenuSelection {
    Subscribe,
    Radio(Option<String>),
    Channel(String),
    Album(String),
    Related(String),
    None,
}

fn youtube_artist_menu_rows(details: &crate::state::YouTubeArtistContext) -> Vec<String> {
    let mut rows = vec![
        if details.subscribed {
            "Unsubscribe".to_owned()
        } else {
            "Subscribe".to_owned()
        },
        if details.radio_id.is_some() {
            "Start radio".to_owned()
        } else {
            "Start radio (unavailable)".to_owned()
        },
        "Open channel".to_owned(),
        "Overview".to_owned(),
    ];
    if let Some(description) = details.description.as_deref() {
        if !description.trim().is_empty() {
            rows.push(format!("Description: {description}"));
        }
    }
    if let Some(views) = details.views.as_deref() {
        rows.push(format!("Views: {views}"));
    }
    if let Some(subscribers) = details.subscribers.as_deref() {
        rows.push(format!("Subscribers: {subscribers}"));
    }
    rows.push(format!("Albums ({})", details.albums.len()));
    rows.extend(details.albums.iter().map(|release| {
        format!(
            "  {}{} · {}",
            release.title,
            release
                .year
                .as_deref()
                .map(|year| format!(" ({year})"))
                .unwrap_or_default(),
            release.kind
        )
    }));
    rows.push(format!("Singles ({})", details.singles.len()));
    rows.extend(details.singles.iter().map(|release| {
        format!(
            "  {}{} · {}",
            release.title,
            release
                .year
                .as_deref()
                .map(|year| format!(" ({year})"))
                .unwrap_or_default(),
            release.kind
        )
    }));
    rows.push(format!("Related artists ({})", details.related.len()));
    rows.extend(
        details
            .related
            .iter()
            .map(|artist| format!("  {} · {}", artist.name, artist.subscribers)),
    );
    rows
}

fn youtube_artist_menu_selection(
    details: &crate::state::YouTubeArtistContext,
    index: usize,
) -> YouTubeArtistMenuSelection {
    match index {
        0 => YouTubeArtistMenuSelection::Subscribe,
        1 => YouTubeArtistMenuSelection::Radio(details.radio_id.clone()),
        2 => YouTubeArtistMenuSelection::Channel(details.channel_id.clone()),
        _ => {
            let mut cursor = 4;
            if details
                .description
                .as_deref()
                .is_some_and(|text| !text.trim().is_empty())
            {
                cursor += 1;
            }
            if details.views.is_some() {
                cursor += 1;
            }
            if details.subscribers.is_some() {
                cursor += 1;
            }
            cursor += 1;
            if index >= cursor && index < cursor + details.albums.len() {
                return YouTubeArtistMenuSelection::Album(
                    details.albums[index - cursor].id.clone(),
                );
            }
            cursor += details.albums.len() + 1;
            if index >= cursor && index < cursor + details.singles.len() {
                return YouTubeArtistMenuSelection::Album(
                    details.singles[index - cursor].id.clone(),
                );
            }
            cursor += details.singles.len() + 1;
            if index >= cursor && index < cursor + details.related.len() {
                return YouTubeArtistMenuSelection::Related(
                    details.related[index - cursor].id.clone(),
                );
            }
            YouTubeArtistMenuSelection::None
        }
    }
}

#[cfg(test)]
mod reset_confirmation_tests {
    use super::{handle_key_sequence_for_popup, PopupState};
    use crate::key::{Key, KeySequence};
    use crate::state::ConfirmableAction;

    #[test]
    fn reset_confirmation_dispatches_once_and_cancel_sends_nothing() {
        crate::ui::initialize_test_config();
        let ring = std::sync::Arc::new(parking_lot::Mutex::new(std::collections::VecDeque::new()));
        let (diagnostics, _runtime) = crate::observability::disabled(ring);
        let state = std::sync::Arc::new(crate::state::State::new(false, diagnostics));
        let (sender, receiver) = crate::client::client_request_channel();
        let confirm = KeySequence {
            keys: vec![Key::None(crossterm::event::KeyCode::Char('y'))],
        };
        {
            let mut ui = state.ui.lock();
            ui.popup = Some(PopupState::ConfirmAction {
                message: "reset".to_owned(),
                action: ConfirmableAction::ResetAllConfiguration,
            });
            assert!(handle_key_sequence_for_popup(&confirm, &sender, &state, &mut ui).unwrap());
            assert!(ui.popup.is_none());
        }
        // The confirmed arm sends exactly one ResetAllConfiguration request.
        receiver.try_recv().expect("reset dispatched");
        assert!(receiver.try_recv().is_err());
        // A non-confirm key closes the popup without dispatching.
        {
            let mut ui = state.ui.lock();
            ui.popup = Some(PopupState::ConfirmAction {
                message: "reset".to_owned(),
                action: ConfirmableAction::ResetAllConfiguration,
            });
            let cancel = KeySequence {
                keys: vec![Key::None(crossterm::event::KeyCode::Esc)],
            };
            assert!(handle_key_sequence_for_popup(&cancel, &sender, &state, &mut ui).unwrap());
            assert!(ui.popup.is_none());
        }
        assert!(receiver.try_recv().is_err());
    }
}

#[cfg(test)]
mod ui06_ui10_tests {
    use super::{
        handle_item_action, lyrics_plain_text, lyrics_timed_lrc, youtube_artist_menu_rows,
        youtube_artist_menu_selection, PopupState, YouTubeArtistMenuSelection,
    };
    use crate::client::client_request_channel;
    use crate::state::{
        ActionListItem, ListenBrainzSyncDetailAction, ListenBrainzSyncDetailRow,
        ListenBrainzSyncPreview, ListenBrainzSyncSide, Lyrics, LyricsLines, PageState,
        UiViewStatus, UnifiedPlaylistContextActionMenu, YouTubeArtistContext, YouTubeArtistRelease,
        YouTubeRelatedArtist, YouTubeTrack, TTL_CACHE_DURATION,
    };
    use ratatui::widgets::ListState;
    use std::{collections::VecDeque, sync::Arc};

    #[test]
    fn lyrics_exports_plain_and_timed_forms_without_losing_lines() {
        let lyrics = Lyrics {
            lines: LyricsLines::Synced(vec![
                (chrono::Duration::milliseconds(1_250), "first".to_owned()),
                (chrono::Duration::milliseconds(62_340), "second".to_owned()),
            ]),
            source: "test".to_owned(),
        };
        assert_eq!(lyrics_plain_text(&lyrics), "first\nsecond");
        assert_eq!(
            lyrics_timed_lrc(&lyrics).as_deref(),
            Some("[00:01.25]first\n[01:02.34]second\n")
        );
    }

    #[test]
    fn lyrics_provider_action_releases_data_read_lock_before_retry() -> anyhow::Result<()> {
        crate::ui::initialize_test_config();
        let ring = Arc::new(parking_lot::Mutex::new(VecDeque::new()));
        let (diagnostics, _runtime) = crate::observability::disabled(ring);
        let state = Arc::new(crate::state::State::new(false, diagnostics));
        let (client_pub, client_sub) = client_request_channel();
        let track_uri = "youtube:lyrics-deadlock-test".to_owned();
        let track = YouTubeTrack {
            id: "lyrics-deadlock-test".to_owned(),
            name: "Track".to_owned(),
            artists: "Artist".to_owned(),
            album: None,
            duration: "3:00".to_owned(),
            explicit: false,
            thumbnail_url: None,
            is_video: true,
        };
        state.data.write().caches.lyrics.insert(
            crate::state::LyricsCacheKey::new(&track_uri, None),
            Lyrics::from_plain("cached line", "test"),
            *TTL_CACHE_DURATION,
        );

        let mut ui = state.ui.lock();
        ui.new_page(PageState::Lyrics {
            provider: crate::config::ActiveProvider::YouTubeMusic,
            track_uri: track_uri.clone(),
            track: track.name.clone(),
            artists: track.artists.clone(),
            youtube_track: Some(track),
            lyrics_provider: None,
            scroll_offset: 0,
            follow_playback: true,
            status: UiViewStatus::Ready,
        });
        ui.popup = Some(PopupState::ActionList(
            Box::new(ActionListItem::Lyrics(crate::state::LyricsActionMenu::new(
                track_uri, true, false,
            ))),
            ListState::default(),
        ));

        // CycleLyricsSource is the third action when cached plain lyrics are
        // present. It must enqueue normally instead of self-deadlocking while
        // clearing the old cache entry.
        assert!(handle_item_action(2, &client_pub, &state, &mut ui)?);
        drop(ui);
        assert!(client_sub.try_recv().is_ok());
        Ok(())
    }

    #[test]
    fn listenbrainz_detail_action_keeps_the_captured_preview_target() -> anyhow::Result<()> {
        crate::ui::initialize_test_config();
        let ring = Arc::new(parking_lot::Mutex::new(VecDeque::new()));
        let (diagnostics, _runtime) = crate::observability::disabled(ring);
        let state = Arc::new(crate::state::State::new(false, diagnostics));
        let (client_pub, _client_sub) = client_request_channel();
        let preview = ListenBrainzSyncPreview {
            playlist_id: "playlist-a".to_owned(),
            operation_reference: "ui-0007".to_owned(),
            rows: vec![ListenBrainzSyncDetailRow {
                occurrence: Some(crate::state::PlaylistEntryId(1)),
                side: ListenBrainzSyncSide::Local,
                action: ListenBrainzSyncDetailAction::Added,
                title: "Title".to_owned(),
                artist: "Artist".to_owned(),
                provider: "Spotify".to_owned(),
                conflict: None,
            }],
            conflicts: Vec::new(),
        };
        let mut ui = state.ui.lock();
        ui.new_page(PageState::new_unified_playlist("playlist-a"));
        let PageState::UnifiedPlaylist {
            listenbrainz_preview,
            ..
        } = ui.current_page_mut()
        else {
            unreachable!()
        };
        *listenbrainz_preview = Some(preview);
        ui.popup = Some(PopupState::ActionList(
            Box::new(ActionListItem::UnifiedPlaylistContext(
                UnifiedPlaylistContextActionMenu::with_actions(
                    "playlist-a".to_owned(),
                    "Playlist".to_owned(),
                    [crate::command::Action::ReviewUnifiedPlaylistListenBrainzConflicts],
                ),
            )),
            ListState::default(),
        ));

        assert!(handle_item_action(0, &client_pub, &state, &mut ui)?);
        ui.current_page_mut().select(99);
        assert!(matches!(
            ui.popup.as_ref(),
            Some(PopupState::ListenBrainzSyncDetails { preview, .. })
                if preview.playlist_id == "playlist-a"
                    && preview.operation_reference == "ui-0007"
                    && preview.rows[0].occurrence == Some(crate::state::PlaylistEntryId(1))
        ));
        Ok(())
    }

    #[test]
    fn listenbrainz_workspace_opens_from_single_menu_entry_with_captured_identity(
    ) -> anyhow::Result<()> {
        crate::ui::initialize_test_config();
        let ring = Arc::new(parking_lot::Mutex::new(VecDeque::new()));
        let (diagnostics, _runtime) = crate::observability::disabled(ring);
        let state = Arc::new(crate::state::State::new(false, diagnostics));
        let (client_pub, _client_sub) = client_request_channel();
        let mut ui = state.ui.lock();
        ui.new_page(PageState::new_unified_playlist("playlist-a"));
        ui.popup = Some(PopupState::ActionList(
            Box::new(ActionListItem::UnifiedPlaylistContext(
                UnifiedPlaylistContextActionMenu::with_actions(
                    "playlist-a".to_owned(),
                    "Playlist".to_owned(),
                    [crate::command::Action::OpenUnifiedPlaylistListenBrainzSync],
                ),
            )),
            ListState::default(),
        ));

        assert!(handle_item_action(0, &client_pub, &state, &mut ui)?);
        let (playlist_id, selection) = match ui.popup.as_ref() {
            Some(PopupState::ListenBrainzWorkspace {
                playlist_id,
                state: list,
                ..
            }) => (playlist_id.clone(), list.selected()),
            other => anyhow::bail!("expected workspace, found {other:?}"),
        };
        assert_eq!(playlist_id, "playlist-a");
        assert_eq!(
            selection,
            Some(
                crate::state::LISTENBRAINZ_WORKSPACE_ACTIONS
                    .iter()
                    .position(|action| *action
                        == crate::command::Action::BackupUnifiedPlaylistToListenBrainz)
                    .unwrap_or(0)
            )
        );
        // Navigating the page behind the workspace must not retarget it.
        ui.current_page_mut().select(99);
        assert!(matches!(
            ui.popup.as_ref(),
            Some(PopupState::ListenBrainzWorkspace { playlist_id, .. })
                if playlist_id == "playlist-a"
        ));
        Ok(())
    }

    #[test]
    fn listenbrainz_workspace_review_keeps_captured_target_without_preview() -> anyhow::Result<()> {
        crate::ui::initialize_test_config();
        let ring = Arc::new(parking_lot::Mutex::new(VecDeque::new()));
        let (diagnostics, _runtime) = crate::observability::disabled(ring);
        let state = Arc::new(crate::state::State::new(false, diagnostics));
        let (client_pub, _client_sub) = client_request_channel();
        let mut ui = state.ui.lock();
        ui.new_page(PageState::new_unified_playlist("playlist-a"));
        let mut list = ListState::default();
        list.select(Some(7));
        ui.popup = Some(PopupState::ListenBrainzWorkspace {
            playlist_id: "playlist-a".to_owned(),
            playlist_name: "Playlist".to_owned(),
            state: list,
            changes: ratatui::widgets::TableState::default(),
        });

        // Review without a preview reports the gate and restores the workspace.
        assert!(super::activate_listenbrainz_workspace_row(
            7,
            &client_pub,
            &state,
            &mut ui
        )?);
        assert!(matches!(
            ui.popup.as_ref(),
            Some(PopupState::ListenBrainzWorkspace { playlist_id, .. })
                if playlist_id == "playlist-a"
        ));
        assert_eq!(
            ui.popup.as_ref().and_then(PopupState::list_selected),
            Some(7)
        );
        Ok(())
    }

    #[test]
    fn listenbrainz_workspace_init_reports_link_gate_and_restores_workspace() -> anyhow::Result<()>
    {
        crate::ui::initialize_test_config();
        let ring = Arc::new(parking_lot::Mutex::new(VecDeque::new()));
        let (diagnostics, _runtime) = crate::observability::disabled(ring);
        let state = Arc::new(crate::state::State::new(false, diagnostics));
        let (client_pub, _client_sub) = client_request_channel();
        let mut ui = state.ui.lock();
        ui.new_page(PageState::new_unified_playlist("playlist-a"));
        let mut list = ListState::default();
        list.select(Some(1));
        ui.popup = Some(PopupState::ListenBrainzWorkspace {
            playlist_id: "playlist-a".to_owned(),
            playlist_name: "Playlist".to_owned(),
            state: list,
            changes: ratatui::widgets::TableState::default(),
        });

        // No link exists, so init reports the gate and keeps the workspace.
        assert!(super::activate_listenbrainz_workspace_row(
            1,
            &client_pub,
            &state,
            &mut ui
        )?);
        assert!(matches!(
            ui.popup.as_ref(),
            Some(PopupState::ListenBrainzWorkspace { playlist_id, .. })
                if playlist_id == "playlist-a"
        ));
        assert_eq!(
            ui.popup.as_ref().and_then(PopupState::list_selected),
            Some(1)
        );
        assert_eq!(
            crate::state::LISTENBRAINZ_WORKSPACE_ACTIONS[1],
            crate::command::Action::InitializeUnifiedPlaylistListenBrainzBase
        );
        Ok(())
    }

    #[test]
    fn listenbrainz_workspace_apply_push_reports_gate_and_restores_workspace() -> anyhow::Result<()>
    {
        crate::ui::initialize_test_config();
        let ring = Arc::new(parking_lot::Mutex::new(VecDeque::new()));
        let (diagnostics, _runtime) = crate::observability::disabled(ring);
        let state = Arc::new(crate::state::State::new(false, diagnostics));
        let (client_pub, _client_sub) = client_request_channel();
        let mut ui = state.ui.lock();
        ui.new_page(PageState::new_unified_playlist("playlist-a"));
        let mut list = ListState::default();
        list.select(Some(10));
        ui.popup = Some(PopupState::ListenBrainzWorkspace {
            playlist_id: "playlist-a".to_owned(),
            playlist_name: "Playlist".to_owned(),
            state: list,
            changes: ratatui::widgets::TableState::default(),
        });

        // No fresh preview exists, so the apply reports the gate and keeps
        // the workspace instead of opening a confirmation.
        assert_eq!(
            crate::state::LISTENBRAINZ_WORKSPACE_ACTIONS[10],
            crate::command::Action::ApplyUnifiedPlaylistListenBrainzPush
        );
        assert!(super::activate_listenbrainz_workspace_row(
            10,
            &client_pub,
            &state,
            &mut ui
        )?);
        assert!(matches!(
            ui.popup.as_ref(),
            Some(PopupState::ListenBrainzWorkspace { playlist_id, .. })
                if playlist_id == "playlist-a"
        ));
        assert_eq!(
            ui.popup.as_ref().and_then(PopupState::list_selected),
            Some(10)
        );
        Ok(())
    }

    #[test]
    fn listenbrainz_resolve_machine_collects_policy_and_decisions_to_confirm() -> anyhow::Result<()>
    {
        crate::ui::initialize_test_config();
        let ring = Arc::new(parking_lot::Mutex::new(VecDeque::new()));
        let (diagnostics, _runtime) = crate::observability::disabled(ring);
        let state = Arc::new(crate::state::State::new(false, diagnostics));
        let (client_pub, _client_sub) = client_request_channel();
        let mut ui = state.ui.lock();
        ui.new_page(PageState::new_unified_playlist("playlist-a"));

        // Without a conflict preview the workspace row reports the gate.
        let mut list = ListState::default();
        list.select(Some(12));
        ui.popup = Some(PopupState::ListenBrainzWorkspace {
            playlist_id: "playlist-a".to_owned(),
            playlist_name: "Playlist".to_owned(),
            state: list,
            changes: ratatui::widgets::TableState::default(),
        });
        assert_eq!(
            crate::state::LISTENBRAINZ_WORKSPACE_ACTIONS[12],
            crate::command::Action::ApplyUnifiedPlaylistListenBrainzResolve
        );
        assert!(super::activate_listenbrainz_workspace_row(
            12,
            &client_pub,
            &state,
            &mut ui
        )?);
        assert!(matches!(
            ui.popup.as_ref(),
            Some(PopupState::ListenBrainzWorkspace { .. })
        ));

        // The resolve menu itself collects policy plus every decision and
        // opens one explicit confirmation carrying them.
        state
            .data
            .write()
            .upsert_playlist_link(crate::state::PlaylistLink {
                unified_playlist_id: "playlist-a".to_owned(),
                listenbrainz_playlist_id: Some("remote".to_owned()),
                ..crate::state::PlaylistLink::default()
            })?;
        let mut resolve = ListState::default();
        resolve.select(Some(0));
        ui.popup = Some(PopupState::ListenBrainzResolve {
            menu: crate::state::ListenBrainzResolveMenu::new(
                "playlist-a".to_owned(),
                "Playlist".to_owned(),
                vec![crate::state::ListenBrainzSyncConflictKind::Reorder],
            ),
            state: resolve,
        });
        // Policy first, then the conflict decision, then apply.
        assert!(super::choose_listenbrainz_resolve_row(0, &state, &mut ui)?);
        assert!(super::choose_listenbrainz_resolve_row(3, &state, &mut ui)?);
        assert!(super::choose_listenbrainz_resolve_row(4, &state, &mut ui)?);
        match ui.popup.as_ref() {
            Some(PopupState::ConfirmAction { message, action }) => {
                assert!(message.contains("Keep local"));
                assert!(matches!(
                    action,
                    crate::state::ConfirmableAction::ApplyUnifiedPlaylistListenBrainzResolve {
                        policy: crate::state::ResolutionPolicy::KeepLocal,
                        decisions,
                        ..
                    } if decisions.len() == 1
                ));
            }
            other => anyhow::bail!("expected resolve confirmation, found {other:?}"),
        }
        Ok(())
    }

    #[test]
    fn artist_menu_projects_sections_and_provider_actions() {
        let details = YouTubeArtistContext {
            channel_id: "channel".to_owned(),
            name: "Artist".to_owned(),
            description: Some("Overview".to_owned()),
            views: Some("10M views".to_owned()),
            subscribers: Some("100K subscribers".to_owned()),
            subscribed: false,
            radio_id: Some("radio".to_owned()),
            albums: vec![YouTubeArtistRelease {
                id: "album".to_owned(),
                title: "Album".to_owned(),
                year: Some("2026".to_owned()),
                kind: "Album".to_owned(),
            }],
            singles: Vec::new(),
            related: vec![YouTubeRelatedArtist {
                id: "related".to_owned(),
                name: "Related".to_owned(),
                subscribers: "1K".to_owned(),
            }],
        };
        let rows = youtube_artist_menu_rows(&details);
        assert!(rows.iter().any(|row| row == "Albums (1)"));
        assert!(rows.iter().any(|row| row == "Related artists (1)"));
        assert_eq!(
            youtube_artist_menu_selection(&details, 0),
            YouTubeArtistMenuSelection::Subscribe
        );
        assert_eq!(
            youtube_artist_menu_selection(&details, 1),
            YouTubeArtistMenuSelection::Radio(Some("radio".to_owned()))
        );
    }
}

fn handle_key_sequence_for_create_playlist_popup(
    key_sequence: &KeySequence,
    client_pub: &crate::client::ClientRequestSender,
    ui: &mut UIStateGuard,
) -> Result<bool> {
    let Some(PopupState::PlaylistCreate {
        target,
        public,
        name,
        desc,
        current_field,
        pending_items,
        source_provider,
        source_epoch,
    }) = &mut ui.popup
    else {
        return Ok(false);
    };
    if key_sequence.keys.len() == 1 {
        match &key_sequence.keys[0] {
            Key::None(crossterm::event::KeyCode::Enter) => {
                let playlist_name = name.get_text();
                if popup_text_is_blank(&playlist_name) {
                    ui.set_unsupported_operation(
                        "Playlist name is required.",
                        "Type a name, then press Enter.",
                    );
                    return Ok(true);
                }
                let pending_seeds = pending_items.clone().unwrap_or_default();
                if !pending_seeds.is_empty() && !target.accepts_source(*source_provider) {
                    ui.set_unsupported_operation(
                        "The selected destination cannot accept these items.",
                        "Choose Unified or the source provider as the destination.",
                    );
                    return Ok(true);
                }
                let has_pending_items = pending_items.is_some();
                pending_items.take();
                match target {
                    crate::state::PlaylistCreateTarget::Spotify => {
                        if has_pending_items {
                            let tracks = pending_seeds
                                .iter()
                                .filter_map(crate::state::PlaylistSeedItem::to_spotify_track)
                                .collect::<Vec<_>>();
                            dispatch_legacy_playlist(
                                client_pub,
                                ClientRequest::CreateSpotifyPlaylistWithTracks {
                                    playlist_name,
                                    public: *public,
                                    collab: false,
                                    desc: desc.get_text(),
                                    tracks,
                                },
                            )?;
                        } else {
                            dispatch_legacy_playlist(
                                client_pub,
                                ClientRequest::CreatePlaylist {
                                    playlist_name,
                                    public: *public,
                                    collab: false,
                                    desc: desc.get_text(),
                                },
                            )?;
                        }
                    }
                    crate::state::PlaylistCreateTarget::YouTubeMusic => {
                        if has_pending_items {
                            let tracks = pending_seeds
                                .iter()
                                .filter_map(crate::state::PlaylistSeedItem::to_youtube_track)
                                .collect::<Vec<_>>();
                            dispatch_legacy_playlist(
                                client_pub,
                                ClientRequest::CreateYouTubePlaylistWithTracks {
                                    playlist_name,
                                    public: *public,
                                    tracks,
                                },
                            )?;
                        } else {
                            dispatch_legacy_playlist(
                                client_pub,
                                ClientRequest::CreateYouTubePlaylist {
                                    playlist_name,
                                    public: *public,
                                },
                            )?;
                        }
                    }
                    crate::state::PlaylistCreateTarget::Unified => {
                        if pending_seeds.is_empty() {
                            dispatch_legacy_playlist(
                                client_pub,
                                ClientRequest::CreateUnifiedPlaylist { playlist_name },
                            )?;
                        } else {
                            let operation =
                                (*source_provider)
                                    .zip(*source_epoch)
                                    .map(|(provider, epoch)| {
                                        new_unified_playlist_operation(
                                            "create",
                                            provider,
                                            epoch,
                                            crate::state::PlaylistDestination::New {
                                                target: crate::state::PlaylistTargetKind::Unified,
                                            },
                                            crate::state::PlaylistIntent::Create {
                                                seed: pending_seeds.clone(),
                                            },
                                            None,
                                        )
                                    });
                            dispatch_legacy_playlist(client_pub, ClientRequest::CreateUnifiedPlaylistWithItems {
                                playlist_name,
                                items: pending_seeds
                                    .into_iter()
                                    .map(crate::state::PlaylistSeedItem::into_unified_playlist_item)
                                    .collect(),
                                operation,
                            })?;
                        }
                    }
                }
                ui.popup = None;
                return Ok(true);
            }
            Key::None(crossterm::event::KeyCode::Tab | crossterm::event::KeyCode::BackTab) => {
                let backwards = matches!(
                    key_sequence.keys[0],
                    Key::None(crossterm::event::KeyCode::BackTab)
                );
                *current_field = if backwards {
                    current_field.previous(*target)
                } else {
                    current_field.next(*target)
                };
                return Ok(true);
            }
            Key::None(crossterm::event::KeyCode::Char(' '))
                if *current_field == PlaylistCreateCurrentField::Target
                    && *target != crate::state::PlaylistCreateTarget::Unified =>
            {
                *public = !*public;
                return Ok(true);
            }
            Key::None(crossterm::event::KeyCode::Left)
                if *current_field == PlaylistCreateCurrentField::Target =>
            {
                *target = target.previous();
                return Ok(true);
            }
            Key::None(crossterm::event::KeyCode::Right)
                if *current_field == PlaylistCreateCurrentField::Target =>
            {
                *target = target.next();
                return Ok(true);
            }
            k => {
                let line_input = match current_field {
                    PlaylistCreateCurrentField::Name => name,
                    PlaylistCreateCurrentField::Desc => desc,
                    PlaylistCreateCurrentField::Target => return Ok(true),
                };
                if line_input.input(k).is_some() {
                    return Ok(true);
                }
            }
        }
    }
    Ok(false)
}

fn new_unified_playlist_operation(
    action: &str,
    source_provider: crate::state::Provider,
    source_epoch: u64,
    destination: crate::state::PlaylistDestination,
    intent: crate::state::PlaylistIntent,
    expected_target_revision: Option<String>,
) -> crate::state::PlaylistOperationEnvelope {
    let nonce = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.as_nanos())
        .unwrap_or_default();
    let operation_id = format!("unified-playlist-{action}-{source_epoch}-{nonce}");
    crate::state::PlaylistOperationEnvelope::new(
        operation_id.clone(),
        format!("{operation_id}:idem"),
        Some(source_provider),
        Some(source_epoch),
        destination,
        intent,
        expected_target_revision,
    )
}

fn unified_playlist_revision(state: &SharedState, playlist_id: &str) -> Option<String> {
    state
        .data
        .read()
        .unified_playlists
        .iter()
        .find(|playlist| playlist.id == playlist_id)
        .map(crate::state::UnifiedPlaylist::snapshot_hash)
}

fn handle_key_sequence_for_session_history_create_popup(
    key_sequence: &KeySequence,
    client_pub: &crate::client::ClientRequestSender,
    ui: &mut UIStateGuard,
) -> Result<bool> {
    if matches!(
        config::get_config()
            .keymap_config
            .find_command_from_key_sequence(key_sequence),
        Some(Command::ClosePopup | Command::PreviousPage)
    ) {
        ui.popup = None;
        return Ok(true);
    }
    if key_sequence.keys.len() != 1 {
        return Ok(false);
    }
    match &key_sequence.keys[0] {
        Key::None(crossterm::event::KeyCode::Enter) => {
            let Some(PopupState::SessionHistoryCreate { items, input }) = &ui.popup else {
                return Ok(false);
            };
            let playlist_name = input.get_text();
            if popup_text_is_blank(&playlist_name) {
                ui.set_unsupported_operation(
                    "Playlist name is required.",
                    "Type a name, then press Enter.",
                );
                return Ok(true);
            }
            dispatch_legacy_playlist(
                client_pub,
                ClientRequest::CreateUnifiedPlaylistFromHistory {
                    playlist_name,
                    items: items.clone(),
                },
            )?;
            ui.popup = None;
            Ok(true)
        }
        key => {
            let Some(PopupState::SessionHistoryCreate { input, .. }) = &mut ui.popup else {
                return Ok(false);
            };
            Ok(input.input(key).is_some())
        }
    }
}

fn handle_key_sequence_for_journal_list_name_popup(
    key_sequence: &KeySequence,
    state: &SharedState,
    ui: &mut UIStateGuard,
) -> Result<bool> {
    if matches!(
        config::get_config()
            .keymap_config
            .find_command_from_key_sequence(key_sequence),
        Some(Command::ClosePopup)
    ) {
        ui.popup = None;
        return Ok(true);
    }

    if key_sequence.keys.len() != 1 {
        return Ok(false);
    }

    match &key_sequence.keys[0] {
        Key::None(crossterm::event::KeyCode::Enter) => {
            let Some(PopupState::JournalListName { action, input }) = &ui.popup else {
                return Ok(false);
            };
            let action = action.clone();
            let name = input.get_text();
            let name = name.trim();
            if popup_text_is_blank(name) {
                ui.set_unsupported_operation(
                    "Journal list name is required.",
                    "Type a name, then press Enter.",
                );
                return Ok(true);
            }

            update_track_journal(state, |journal| match action {
                JournalListNameAction::Create => {
                    journal.create_list(name.to_string());
                }
                JournalListNameAction::CreateWithTracks { tracks } => {
                    let list_id = journal.create_list(name.to_string());
                    journal.add_tracks_to_list(&list_id, tracks);
                }
                JournalListNameAction::CreateWithYouTubeTracks { tracks } => {
                    let list_id = journal.create_list(name.to_string());
                    journal.add_youtube_tracks_to_list(&list_id, tracks);
                }
                JournalListNameAction::Rename { list_id } => {
                    journal.rename_list(&list_id, name.to_string());
                }
            })?;
            ui.popup = None;
            Ok(true)
        }
        key => {
            let Some(PopupState::JournalListName { input, .. }) = &mut ui.popup else {
                return Ok(false);
            };
            Ok(input.input(key).is_some())
        }
    }
}

fn handle_key_sequence_for_playlist_name_popup(
    key_sequence: &KeySequence,
    client_pub: &crate::client::ClientRequestSender,
    ui: &mut UIStateGuard,
) -> Result<bool> {
    if matches!(
        config::get_config()
            .keymap_config
            .find_command_from_key_sequence(key_sequence),
        Some(Command::ClosePopup)
    ) {
        ui.popup = None;
        return Ok(true);
    }
    if key_sequence.keys.len() != 1 {
        return Ok(false);
    }
    match &key_sequence.keys[0] {
        Key::None(crossterm::event::KeyCode::Enter) => {
            let Some(PopupState::PlaylistName { action, input }) = &ui.popup else {
                return Ok(false);
            };
            let action = action.clone();
            let name = input.get_text();
            let name = name.trim();
            if popup_text_is_blank(name) {
                ui.set_unsupported_operation(
                    "Playlist name is required.",
                    "Type a name, then press Enter.",
                );
                return Ok(true);
            }
            let request = match action {
                crate::state::PlaylistNameAction::Spotify { playlist_id } => {
                    ClientRequest::RenameSpotifyPlaylist {
                        playlist_id,
                        name: name.to_owned(),
                    }
                }
                crate::state::PlaylistNameAction::YouTubeMusic { playlist_id } => {
                    ClientRequest::RenameYouTubePlaylist {
                        playlist_id,
                        name: name.to_owned(),
                    }
                }
                crate::state::PlaylistNameAction::Unified { playlist_id } => {
                    ClientRequest::RenameUnifiedPlaylist {
                        playlist_id,
                        name: name.to_owned(),
                    }
                }
            };
            dispatch_legacy_playlist(client_pub, request)?;
            ui.popup = None;
            Ok(true)
        }
        key => {
            let Some(PopupState::PlaylistName { input, .. }) = &mut ui.popup else {
                return Ok(false);
            };
            Ok(input.input(key).is_some())
        }
    }
}

fn handle_key_sequence_for_track_note_popup(
    key_sequence: &KeySequence,
    state: &SharedState,
    ui: &mut UIStateGuard,
) -> Result<bool> {
    if matches!(
        config::get_config()
            .keymap_config
            .find_command_from_key_sequence(key_sequence),
        Some(Command::ClosePopup)
    ) {
        ui.popup = None;
        return Ok(true);
    }

    if key_sequence.keys.len() != 1 {
        return Ok(false);
    }

    match &key_sequence.keys[0] {
        Key::None(crossterm::event::KeyCode::Enter) => {
            let Some(PopupState::TrackNote { track, input }) = &ui.popup else {
                return Ok(false);
            };
            let track = track.clone();
            let note = input.get_text();
            update_track_journal(state, |journal| journal.set_note(track, note))?;
            ui.popup = None;
            Ok(true)
        }
        key => {
            let Some(PopupState::TrackNote { input, .. }) = &mut ui.popup else {
                return Ok(false);
            };
            Ok(input.input(key).is_some())
        }
    }
}

/// Keys for the volume popup: Left/Right step by one, Up/Down by
/// `volume_scroll_step`, digits and Enter set a typed percentage, and Esc
/// closes it. Other keys reach the global commands.
fn handle_key_sequence_for_volume_popup(
    key_sequence: &KeySequence,
    client_pub: &crate::client::ClientRequestSender,
    state: &SharedState,
    ui: &mut UIStateGuard,
) -> Result<bool> {
    if matches!(
        config::get_config()
            .keymap_config
            .find_command_from_key_sequence(key_sequence),
        Some(Command::ClosePopup)
    ) {
        ui.popup = None;
        return Ok(true);
    }
    let [key @ Key::None(code)] = key_sequence.keys.as_slice() else {
        return Ok(false);
    };
    let provider = state
        .player
        .read()
        .effective_playback_provider(ui.active_provider);
    let step = config::get_config().app_config.volume_scroll_step;
    let change = |change: &dyn Fn(u8) -> u8| {
        super::change_playback_volume(client_pub, state, provider, change)
    };
    match code {
        KeyCode::Left => change(&|volume| volume.saturating_sub(1))?,
        KeyCode::Right => change(&|volume| volume.saturating_add(1).min(100))?,
        KeyCode::Down => change(&|volume| volume.saturating_sub(step))?,
        KeyCode::Up => change(&|volume| volume.saturating_add(step).min(100))?,
        KeyCode::Enter => {
            let Some(PopupState::Volume { input, .. }) = ui.popup.as_mut() else {
                return Ok(false);
            };
            match input.get_text().parse::<u8>() {
                Ok(volume) if volume <= 100 => {
                    ui.popup = None;
                    change(&|_| volume)?;
                }
                _ => *input = crate::ui::single_line_input::LineInput::new(Vec::new()),
            }
        }
        KeyCode::Char(digit) if digit.is_ascii_digit() => {
            if let Some(PopupState::Volume { input, .. }) = ui.popup.as_mut() {
                if input.get_text().len() < 3 {
                    input.input(key);
                }
            }
        }
        KeyCode::Backspace => {
            if let Some(PopupState::Volume { input, .. }) = ui.popup.as_mut() {
                input.input(key);
            }
        }
        _ => return Ok(false),
    }
    Ok(true)
}

pub(super) fn handle_key_sequence_for_config_edit_popup(
    key_sequence: &KeySequence,
    client_pub: &crate::client::ClientRequestSender,
    ui: &mut UIStateGuard,
) -> bool {
    if matches!(
        config::get_config()
            .keymap_config
            .find_command_from_key_sequence(key_sequence),
        Some(Command::ClosePopup)
    ) {
        ui.popup = None;
        return true;
    }

    if key_sequence.keys.len() != 1 {
        return false;
    }

    match &key_sequence.keys[0] {
        Key::None(crossterm::event::KeyCode::Enter) => {
            let Some(PopupState::ConfigEdit { key, input }) = &ui.popup else {
                return false;
            };
            let key = key.clone();
            let value = input.get_text();
            if key.starts_with("welcome.youtube.")
                && matches!(ui.current_page(), PageState::Welcome { .. })
            {
                if ui.welcome_youtube_operation.is_busy() {
                    ui.welcome_youtube_notice =
                        Some("Wait for the current action to finish.".to_owned());
                    return true;
                }
                let path = std::path::PathBuf::from(value.trim());
                if key == "welcome.youtube.browser" {
                    match crate::client::save_browser_choice(&config::get_config().config_folder, path) {
                        Ok(path) => {
                            ui.finish_welcome_youtube_action(crate::state::WelcomeOperation::Idle);
                            ui.welcome_youtube_browser = Some(path);
                            ui.welcome_youtube_notice = Some("Browser selected. Choose Sign in to open it.".to_owned());
                            ui.popup = None;
                        }
                        Err(_) => ui.welcome_youtube_notice = Some("Cannot save this browser. Check the executable path and configuration permissions.".to_owned()),
                    }
                } else if path.is_file() {
                    ui.begin_welcome_youtube_action(crate::state::WelcomeOperation::SigningIn);
                    ui.welcome_youtube_login_active = true;
                    ui.welcome_youtube_notice =
                        Some("Reading cookies and verifying account access...".to_owned());
                    if client_pub
                        .send(crate::client::ClientRequest::ImportYouTubeCookies(path))
                        .is_ok()
                    {
                        ui.popup = None;
                    } else {
                        ui.finish_welcome_youtube_action(crate::state::WelcomeOperation::Failed);
                        ui.welcome_youtube_notice =
                            Some("Could not start import; retry.".to_owned());
                    }
                } else {
                    ui.welcome_youtube_notice =
                        Some("Choose an existing local cookie text file.".to_owned());
                }
                return true;
            }
            if key == "client_id" && matches!(ui.current_page(), PageState::Welcome { .. }) {
                if page::save_welcome_spotify_client(ui, &value) {
                    ui.popup = None;
                }
                return true;
            }
            let selected = match ui.current_page() {
                PageState::Settings { settings, .. } => settings
                    .iter()
                    .position(|setting| setting.key == key)
                    .unwrap_or_default(),
                _ => 0,
            };
            ui.popup = None;
            page::save_settings_value(ui, &key, &value, selected);
            true
        }
        key => {
            let Some(PopupState::ConfigEdit { input, .. }) = &mut ui.popup else {
                return false;
            };
            input.input(key).is_some()
        }
    }
}

fn handle_key_sequence_for_config_choice_popup(
    key_sequence: &KeySequence,
    client_pub: &crate::client::ClientRequestSender,
    ui: &mut UIStateGuard,
) -> Result<bool> {
    let Some(command) = config::get_config()
        .keymap_config
        .find_command_from_key_sequence(key_sequence)
    else {
        return Ok(false);
    };

    let n_options = match &ui.popup {
        Some(PopupState::ConfigChoice { options, .. }) => options.len(),
        _ => return Ok(false),
    };

    handle_command_for_list_popup(
        command,
        ui,
        n_options,
        |_, _| {},
        |ui: &mut UIStateGuard, id: usize| -> Result<()> {
            let Some(PopupState::ConfigChoice { key, options, .. }) = &ui.popup else {
                return Ok(());
            };
            let key = key.clone();
            let value = options[id].clone();
            let selected = selected_settings_index(ui, &key);
            ui.popup = None;
            if let Some(provider) = account_choice_provider(&key) {
                let configs = config::get_config();
                let registry = config::AccountRegistry::load(&configs.config_folder)?;
                let accounts = registry.summaries(
                    provider,
                    &configs.config_folder,
                    &configs.cache_folder,
                    &configs.youtube_music_cookie_path(),
                );
                let account = accounts.get(id).context("selected account disappeared")?;
                client_pub.send(ClientRequest::ManageAccount(
                    crate::client::AccountOperation::Switch {
                        provider,
                        account_id: account.id.clone(),
                    },
                ))?;
                page::set_settings_message(
                    ui,
                    &format!(
                        "Switching to {} account {}",
                        provider.title(),
                        account.label
                    ),
                );
            } else {
                page::save_settings_value(ui, &key, &value, selected);
            }
            Ok(())
        },
        |ui: &mut UIStateGuard| {
            ui.popup = None;
        },
    )
}

fn handle_key_sequence_for_workspace_scope_popup(
    key_sequence: &KeySequence,
    client_pub: &crate::client::ClientRequestSender,
    state: &SharedState,
    ui: &mut UIStateGuard,
) -> Result<bool> {
    let Some(command) = config::get_config()
        .keymap_config
        .find_command_from_key_sequence(key_sequence)
    else {
        return Ok(false);
    };

    let n_options = match &ui.popup {
        Some(PopupState::WorkspaceScope { options, .. }) => options.len(),
        _ => return Ok(false),
    };

    handle_command_for_list_popup(
        command,
        ui,
        n_options,
        |_, _| {},
        |ui: &mut UIStateGuard, id: usize| -> Result<()> {
            super::page::choose_workspace_scope(id, client_pub, state, ui)?;
            Ok(())
        },
        |ui: &mut UIStateGuard| {
            ui.popup = None;
        },
    )
}

fn account_choice_provider(key: &str) -> Option<config::ActiveProvider> {
    match key {
        "accounts.spotify.active" => Some(config::ActiveProvider::Spotify),
        "accounts.youtube_music.active" => Some(config::ActiveProvider::YouTubeMusic),
        _ => None,
    }
}

fn handle_key_sequence_for_config_multi_choice_popup(
    key_sequence: &KeySequence,
    ui: &mut UIStateGuard,
) -> bool {
    if matches!(
        config::get_config()
            .keymap_config
            .find_command_from_key_sequence(key_sequence),
        Some(Command::ClosePopup)
    ) {
        ui.popup = None;
        return true;
    }

    let Some(command) = config::get_config()
        .keymap_config
        .find_command_from_key_sequence(key_sequence)
    else {
        return false;
    };

    let offset = ui.count_prefix.unwrap_or(1);
    let Some(PopupState::ConfigMultiChoice {
        state, selected, ..
    }) = &mut ui.popup
    else {
        return false;
    };
    let current_id = state.selected().unwrap_or_default();
    let n_options = selected.len();
    if n_options == 0 {
        return false;
    }

    match command {
        Command::SelectPreviousOrScrollUp => {
            state.select(Some(current_id.saturating_sub(offset)));
        }
        Command::SelectNextOrScrollDown => {
            state.select(Some(std::cmp::min(current_id + offset, n_options - 1)));
        }
        Command::ChooseSelected | Command::ResumePause => {
            if current_id < n_options {
                selected[current_id] = !selected[current_id];
            }
        }
        Command::PreviousPage => {
            let Some(PopupState::ConfigMultiChoice {
                key,
                options,
                selected,
                ..
            }) = &ui.popup
            else {
                return false;
            };
            let key = key.clone();
            let values = options
                .iter()
                .zip(selected)
                .filter_map(|(option, selected)| selected.then_some(option.clone()))
                .collect::<Vec<_>>();
            let value = toml_array_string(&values);
            let selected = selected_settings_index(ui, &key);
            ui.popup = None;
            page::save_settings_value(ui, &key, &value, selected);
        }
        _ => return false,
    }

    true
}

fn selected_settings_index(ui: &UIStateGuard, key: &str) -> usize {
    match ui.current_page() {
        PageState::Settings { settings, .. } => settings
            .iter()
            .position(|setting| setting.key == key)
            .unwrap_or_default(),
        _ => 0,
    }
}

fn toml_array_string(values: &[String]) -> String {
    let values = values
        .iter()
        .map(|value| format!("\"{}\"", value.replace('"', "\\\"")))
        .collect::<Vec<_>>()
        .join(", ");
    format!("[{values}]")
}

fn close_collection_search(state: &SharedState, ui: &mut UIStateGuard) {
    fn source_index<T: std::fmt::Display>(
        ui: &UIStateGuard,
        items: &[T],
        cursor: usize,
    ) -> Option<usize> {
        let visible = ui.search_filtered_items(items);
        let item = visible.get(cursor)?;
        items
            .iter()
            .position(|candidate| std::ptr::eq(candidate, *item))
    }

    let cursor = ui.current_page().selected_index().unwrap_or_default();
    let source = if let PageState::Context {
        id: Some(id),
        state: Some(page_state),
        ..
    } = ui.current_page()
    {
        let data = state.data.read();
        match (data.caches.context.get(&id.uri()), page_state) {
            (
                Some(
                    crate::state::Context::Album { tracks, .. }
                    | crate::state::Context::Tracks { tracks, .. },
                ),
                _,
            ) => source_index(ui, tracks, cursor),
            (Some(crate::state::Context::Show { episodes, .. }), _) => {
                source_index(ui, episodes, cursor)
            }
            (
                Some(crate::state::Context::Artist {
                    artist,
                    top_tracks,
                    albums,
                    related_artists,
                    ..
                }),
                ContextPageUIState::Artist { focus, .. },
            ) => match focus {
                ArtistFocusState::TopTracks => source_index(ui, top_tracks, cursor),
                ArtistFocusState::LikedSongs => {
                    source_index(ui, &data.user_data.liked_tracks_by_artist(artist), cursor)
                }
                ArtistFocusState::Albums => source_index(ui, albums, cursor),
                ArtistFocusState::RelatedArtists => source_index(ui, related_artists, cursor),
            },
            _ => None,
        }
    } else if let PageState::YouTubeContext {
        id,
        context: Some(context),
        ..
    } = ui.current_page()
    {
        if matches!(id, YouTubeContextId::Playlist(_)) {
            None
        } else {
            source_index(ui, &context.tracks, cursor)
        }
    } else {
        None
    };
    ui.popup = None;
    if let Some(source) = source {
        ui.current_page_mut().select(source);
    }
}

fn handle_key_sequence_for_search_popup(
    key_sequence: &KeySequence,
    client_pub: &crate::client::ClientRequestSender,
    state: &SharedState,
    ui: &mut UIStateGuard,
) -> Result<bool> {
    if matches!(
        config::get_config()
            .keymap_config
            .find_command_from_key_sequence(key_sequence),
        Some(Command::ClosePopup)
    ) {
        close_collection_search(state, ui);
        return Ok(true);
    }
    // handle user's input that updates the search query
    let Some(PopupState::Search { ref mut query }) = &mut ui.popup else {
        return Ok(false);
    };
    if key_sequence.keys.len() == 1 {
        if let Key::None(c) = key_sequence.keys[0] {
            match c {
                crossterm::event::KeyCode::Char(c) => {
                    query.push(c);
                    ui.current_page_mut().select(0);
                    return Ok(true);
                }
                crossterm::event::KeyCode::Backspace => {
                    if query.is_empty() {
                        // close search popup when user presses backspace on empty search
                        close_collection_search(state, ui);
                    } else {
                        query.pop().unwrap();
                        ui.current_page_mut().select(0);
                    }
                    return Ok(true);
                }
                _ => {}
            }
        }
    }

    // key sequence not handle by the popup should be moved to the current page's event handler
    page::handle_key_sequence_for_page(key_sequence, client_pub, state, ui)
}

/// Handle a command for a context list popup in which each item represents a context
///
/// # Arguments
/// In addition to application's states and the key sequence,
/// the function requires to specify:
/// - `uris`: a list of context URIs
/// - `uri_type`: an enum represents the type of a context in the list (`playlist`, `artist`, etc)
fn handle_command_for_context_browsing_list_popup(
    command: Command,
    ui: &mut UIStateGuard,
    uris: &[String],
    context_type: &rspotify::model::Type,
) -> Result<bool> {
    handle_command_for_list_popup(
        command,
        ui,
        uris.len(),
        |_, _| {},
        |ui: &mut UIStateGuard, id: usize| -> Result<()> {
            let uri = crate::utils::parse_uri(&uris[id]);
            let context_id = match context_type {
                rspotify::model::Type::Playlist => {
                    ContextId::Playlist(PlaylistId::from_uri(&uri)?.into_static())
                }
                rspotify::model::Type::Artist => {
                    ContextId::Artist(ArtistId::from_uri(&uri)?.into_static())
                }
                rspotify::model::Type::Album => {
                    ContextId::Album(AlbumId::from_uri(&uri)?.into_static())
                }
                _ => {
                    return Ok(());
                }
            };

            ui.new_page(PageState::Context {
                id: None,
                context_page_type: ContextPageType::Browsing(context_id),
                state: None,
            });

            Ok(())
        },
        |ui: &mut UIStateGuard| {
            ui.popup = None;
        },
    )
}

pub(super) fn handle_session_history_command(
    command: Command,
    state: &SharedState,
    ui: &mut UIStateGuard,
) -> Result<bool> {
    if ui.popup.is_some() || !matches!(ui.current_page(), PageState::SessionHistory { .. }) {
        return Ok(false);
    }

    if command == Command::CreatePlaylistFromSessionHistory {
        let items = super::session_history_playlist_items(state);
        if items.is_empty() {
            ui.set_unsupported_operation(
                "Session history is empty.",
                "Play an item, then try creating a playlist from history again.",
            );
        } else {
            ui.popup = Some(PopupState::SessionHistoryCreate {
                items,
                input: crate::ui::single_line_input::LineInput::default(),
            });
        }
        return Ok(true);
    }

    if matches!(command, Command::ClosePopup | Command::PreviousPage) {
        if ui.history.len() > 1 {
            ui.history.pop();
            ui.sync_workspace_after_history_change();
        }
        ui.session_history_selection.clear();
        return Ok(true);
    }

    let n_items = state.data.read().session_history.entries.len();
    if matches!(command, Command::SelectAll | Command::InvertSelection) {
        let projection = (0..n_items).collect::<Vec<_>>();
        let result = if command == Command::SelectAll {
            ui.session_history_selection.select_all(&projection)
        } else {
            ui.session_history_selection.invert(&projection)
        };
        if let Err(error) = result {
            tracing::debug!(selection_error = ?error, "session history selection refused");
        }
        return Ok(true);
    }
    if matches!(
        command,
        Command::ExtendSelectionNext | Command::ExtendSelectionPrevious
    ) {
        if n_items == 0 {
            return Ok(true);
        }
        let projection = (0..n_items).collect::<Vec<_>>();
        let current = session_history_cursor(ui);
        let target = if command == Command::ExtendSelectionNext {
            current.saturating_add(1).min(n_items - 1)
        } else {
            current.saturating_sub(1)
        };
        if let Err(error) =
            ui.session_history_selection
                .extend_range(&projection, Some(current), Some(target))
        {
            tracing::debug!(selection_error = ?error, "session history range refused");
        }
        session_history_select(ui, target);
        return Ok(true);
    }
    if command == Command::ShowActionsOnSelectedItem {
        return open_session_history_actions(state, ui);
    }
    if matches!(
        command,
        Command::SelectPreviousOrScrollUp | Command::SelectNextOrScrollDown
    ) {
        // Match the shared list contract: moving without Shift starts a fresh
        // cursor selection instead of retaining a stale range.
        ui.session_history_selection.clear();
    }

    if n_items == 0 {
        return Ok(false);
    }
    let offset = ui.count_prefix.unwrap_or(1);
    let current = session_history_cursor(ui).min(n_items - 1);
    let target = match command {
        Command::SelectPreviousOrScrollUp => current.saturating_sub(offset),
        Command::SelectNextOrScrollDown => current
            .saturating_add(offset)
            .min(n_items.saturating_sub(1)),
        Command::ChooseSelected => current,
        _ => return Ok(false),
    };
    session_history_select(ui, target);
    Ok(true)
}

fn session_history_cursor(ui: &UIStateGuard) -> usize {
    ui.current_page().selected_index().unwrap_or_default()
}

fn session_history_select(ui: &mut UIStateGuard, index: usize) {
    if matches!(ui.current_page(), PageState::SessionHistory { .. }) {
        ui.current_page_mut().select(index);
    }
}

/// Handle a command for a generic list popup.
///
/// # Arguments
/// - `n_items`: the number of items in the list
/// - `on_select_func`: the callback when selecting an item
/// - `on_choose_func`: the callback when choosing an item
/// - `on_close_func`: the callback when closing the popup
fn handle_command_for_list_popup(
    command: Command,
    ui: &mut UIStateGuard,
    n_items: usize,
    on_select_func: impl FnOnce(&mut UIStateGuard, usize),
    on_choose_func: impl FnOnce(&mut UIStateGuard, usize) -> anyhow::Result<()>,
    on_close_func: impl FnOnce(&mut UIStateGuard),
) -> anyhow::Result<bool> {
    let offset = ui.count_prefix.unwrap_or(1);
    let popup = ui.popup.as_mut().with_context(|| "expect a popup")?;
    let current_id = popup.list_selected().unwrap_or_default();

    match command {
        Command::ClosePopup | Command::PreviousPage => {
            // Empty list popups must still be dismissible.
            on_close_func(ui);
        }
        _ if n_items == 0 => return Ok(false),
        Command::SelectPreviousOrScrollUp => {
            let next_id = current_id.saturating_sub(offset);
            popup.list_select(Some(next_id));
            on_select_func(ui, next_id);
        }
        Command::SelectNextOrScrollDown => {
            let next_id = std::cmp::min(current_id + offset, n_items - 1);
            popup.list_select(Some(next_id));
            on_select_func(ui, next_id);
        }
        Command::ChooseSelected => {
            if current_id < n_items {
                on_choose_func(ui, current_id)?;
            }
        }
        _ => return Ok(false),
    }
    Ok(true)
}

fn open_session_history_actions(state: &SharedState, ui: &mut UIStateGuard) -> Result<bool> {
    let entries = state
        .data
        .read()
        .session_history
        .newest_first()
        .cloned()
        .collect::<Vec<_>>();
    if entries.is_empty() {
        ui.set_unsupported_operation(
            "Session history is empty.",
            "Play an item, then open history actions again.",
        );
        return Ok(true);
    }
    let current = session_history_cursor(ui);
    let selected = ui.session_history_selection.selected_keys();
    let indices = if selected.is_empty() {
        vec![current.min(entries.len() - 1)]
    } else {
        selected
            .iter()
            .copied()
            .filter(|index| *index < entries.len())
            .collect::<Vec<_>>()
    };
    let items = indices
        .into_iter()
        .filter_map(|index| entries.get(index).cloned())
        .collect::<Vec<_>>();
    if items.is_empty() {
        return Ok(false);
    }
    ui.popup = Some(PopupState::ActionList(
        Box::new(ActionListItem::SessionHistory(
            crate::state::SessionHistoryActionMenu::new(items),
        )),
        ListState::default(),
    ));
    Ok(true)
}

fn handle_key_sequence_for_action_list_popup(
    n_actions: usize,
    key_sequence: &KeySequence,
    client_pub: &crate::client::ClientRequestSender,
    state: &SharedState,
    ui: &mut UIStateGuard,
) -> Result<bool> {
    if let Some(Key::None(crossterm::event::KeyCode::Char(c))) = key_sequence.keys.first() {
        if let Some(id) = crate::command::one_based_digit_index(*c, n_actions) {
            handle_item_action(id, client_pub, state, ui)?;
            return Ok(true);
        }
    }

    let Some(command) = config::get_config()
        .keymap_config
        .find_command_from_key_sequence(key_sequence)
    else {
        return Ok(false);
    };

    handle_command_for_list_popup(
        command,
        ui,
        n_actions,
        |_, _| {},
        |ui: &mut UIStateGuard, id: usize| -> Result<()> {
            handle_item_action(id, client_pub, state, ui)?;
            Ok(())
        },
        |ui: &mut UIStateGuard| {
            ui.popup = None;
        },
    )
}

fn handle_key_sequence_for_listenbrainz_workspace(
    key_sequence: &KeySequence,
    client_pub: &crate::client::ClientRequestSender,
    state: &SharedState,
    ui: &mut UIStateGuard,
) -> Result<bool> {
    let n_actions = crate::state::LISTENBRAINZ_WORKSPACE_ACTIONS.len();
    if let Some(Key::None(crossterm::event::KeyCode::Char(c))) = key_sequence.keys.first() {
        if let Some(id) = crate::command::one_based_digit_index(*c, n_actions) {
            activate_listenbrainz_workspace_row(id, client_pub, state, ui)?;
            return Ok(true);
        }
    }

    let Some(command) = config::get_config()
        .keymap_config
        .find_command_from_key_sequence(key_sequence)
    else {
        return Ok(false);
    };

    handle_command_for_list_popup(
        command,
        ui,
        n_actions,
        |_, _| {},
        |ui: &mut UIStateGuard, id: usize| -> Result<()> {
            activate_listenbrainz_workspace_row(id, client_pub, state, ui)?;
            Ok(())
        },
        |ui: &mut UIStateGuard| {
            ui.popup = None;
        },
    )
}

fn handle_key_sequence_for_listenbrainz_resolve(
    key_sequence: &KeySequence,
    state: &SharedState,
    ui: &mut UIStateGuard,
) -> Result<bool> {
    let row_count = match ui.popup.as_ref() {
        Some(PopupState::ListenBrainzResolve { menu, .. }) => menu.row_count(),
        _ => return Ok(false),
    };
    if let Some(Key::None(crossterm::event::KeyCode::Char(c))) = key_sequence.keys.first() {
        if let Some(id) = crate::command::one_based_digit_index(*c, row_count) {
            return choose_listenbrainz_resolve_row(id, state, ui);
        }
    }

    let Some(command) = config::get_config()
        .keymap_config
        .find_command_from_key_sequence(key_sequence)
    else {
        return Ok(false);
    };

    handle_command_for_list_popup(
        command,
        ui,
        row_count,
        |_, _| {},
        |ui: &mut UIStateGuard, id: usize| -> Result<()> {
            choose_listenbrainz_resolve_row(id, state, ui)?;
            Ok(())
        },
        |ui: &mut UIStateGuard| {
            ui.popup = None;
        },
    )
}

/// Advance the resolve menu: policy rows select, conflict rows cycle, and the
/// apply row opens an explicit confirmation carrying the chosen policy and
/// decisions. Nothing here writes; the confirmed apply re-validates
/// everything fail-closed in the client handler.
fn choose_listenbrainz_resolve_row(
    id: usize,
    state: &SharedState,
    ui: &mut UIStateGuard,
) -> Result<bool> {
    let Some(PopupState::ListenBrainzResolve { menu, .. }) = ui.popup.as_mut() else {
        return Ok(false);
    };
    if id < 3 {
        menu.select_policy(id);
        return Ok(true);
    }
    if id + 1 < menu.row_count() {
        menu.cycle_decision(id);
        return Ok(true);
    }
    if !menu.is_ready() {
        ui.set_unsupported_operation(
            "The resolution is incomplete.",
            "Choose a policy and keep every conflict on one side.",
        );
        return Ok(true);
    }
    if menu.has_unsafe_remote_choice() {
        ui.set_unsupported_operation(
            "Drifted or unlinked rows cannot resolve to ListenBrainz here.",
            "Keep them local, or map them with the CLI resolve --import-unlinked.",
        );
        return Ok(true);
    }
    let remote_linked = state.data.read().playlist_links.iter().any(|link| {
        link.unified_playlist_id == menu.playlist_id() && link.listenbrainz_playlist_id.is_some()
    });
    if !remote_linked {
        ui.set_unsupported_operation(
            "No ListenBrainz backup is linked yet.",
            "Back up this playlist before applying.",
        );
        return Ok(true);
    }
    let (playlist_id, playlist_name, policy, decisions, summary) = (
        menu.playlist_id().to_owned(),
        menu.playlist_name().to_owned(),
        menu.policy().expect("resolution is ready"),
        menu.decisions(),
        menu.apply_summary(),
    );
    ui.popup = Some(PopupState::ConfirmAction {
        message: format!("Apply {summary} to {playlist_name}?"),
        action: ConfirmableAction::ApplyUnifiedPlaylistListenBrainzResolve {
            unified_playlist_id: playlist_id,
            policy,
            decisions,
        },
    });
    Ok(true)
}

/// Run one workspace row through the existing Unified Playlist context
/// dispatch so guards, requests, and lifecycle stay identical.
///
/// The workspace captures playlist identity when it opens; the synthetic
/// single-action menu below only forwards that captured target. When dispatch
/// leaves the synthetic menu in place (a gate message with no follow-up
/// popup), the workspace is restored so the owner keeps full context.
fn activate_listenbrainz_workspace_row(
    id: usize,
    client_pub: &crate::client::ClientRequestSender,
    state: &SharedState,
    ui: &mut UIStateGuard,
) -> Result<bool> {
    let Some(action) = crate::state::LISTENBRAINZ_WORKSPACE_ACTIONS
        .get(id)
        .copied()
    else {
        return Ok(false);
    };
    let (playlist_id, playlist_name, changes) = match ui.popup.as_ref() {
        Some(PopupState::ListenBrainzWorkspace {
            playlist_id,
            playlist_name,
            changes,
            ..
        }) => (playlist_id.clone(), playlist_name.clone(), *changes),
        _ => return Ok(false),
    };
    ui.popup = Some(PopupState::ActionList(
        Box::new(ActionListItem::UnifiedPlaylistContext(
            crate::state::UnifiedPlaylistContextActionMenu::with_actions(
                playlist_id.clone(),
                playlist_name.clone(),
                [action],
            ),
        )),
        ListState::default(),
    ));
    handle_item_action(0, client_pub, state, ui)?;
    let synthetic_menu_left = matches!(
        ui.popup.as_ref(),
        Some(PopupState::ActionList(item, ..)) if item.n_actions() == 1
    );
    if synthetic_menu_left {
        let mut list = ListState::default();
        list.select(Some(
            id.min(
                crate::state::LISTENBRAINZ_WORKSPACE_ACTIONS
                    .len()
                    .saturating_sub(1),
            ),
        ));
        ui.popup = Some(PopupState::ListenBrainzWorkspace {
            playlist_id,
            playlist_name,
            state: list,
            changes,
        });
    }
    Ok(true)
}

fn journal_destructive_action_uses_page_projection(
    action: Action,
    page_type: crate::state::PageType,
) -> bool {
    matches!(
        action,
        Action::RemoveFromJournal | Action::RemoveFromJournalList
    ) && matches!(
        page_type,
        crate::state::PageType::Journal | crate::state::PageType::JournalList
    )
}

#[cfg(test)]
mod action_list_index_tests {
    use super::{journal_destructive_action_uses_page_projection, popup_text_is_blank};
    use crate::{command::one_based_digit_index, command::Action, state::PageType};

    #[test]
    fn visible_action_rows_are_one_based() {
        assert_eq!(one_based_digit_index('1', 3), Some(0));
        assert_eq!(one_based_digit_index('2', 3), Some(1));
        assert_eq!(one_based_digit_index('3', 3), Some(2));
        assert_eq!(one_based_digit_index('0', 3), None);
        assert_eq!(one_based_digit_index('4', 3), None);
        assert_eq!(one_based_digit_index('x', 3), None);
    }

    #[test]
    fn journal_destructive_popup_guard_is_limited_to_journal_track_pages() {
        assert!(journal_destructive_action_uses_page_projection(
            Action::RemoveFromJournal,
            PageType::Journal,
        ));
        assert!(journal_destructive_action_uses_page_projection(
            Action::RemoveFromJournalList,
            PageType::JournalList,
        ));
        assert!(!journal_destructive_action_uses_page_projection(
            Action::RemoveFromJournal,
            PageType::Context,
        ));
        assert!(!journal_destructive_action_uses_page_projection(
            Action::AddToQueue,
            PageType::Journal,
        ));
    }

    #[test]
    fn required_popup_text_rejects_blank_names_but_keeps_content() {
        assert!(popup_text_is_blank(""));
        assert!(popup_text_is_blank(" \t\n"));
        assert!(!popup_text_is_blank("Morning mix"));
    }
}

/// Handle the `n`-th action in an action list popup
pub fn handle_item_action(
    n: usize,
    client_pub: &crate::client::ClientRequestSender,
    state: &SharedState,
    ui: &mut UIStateGuard,
) -> Result<bool> {
    let Some(crate::state::PopupActionPayload::Ordinary(descriptor)) =
        ui.popup.as_ref().and_then(|popup| popup.action_payload(n))
    else {
        return Ok(false);
    };
    let action = descriptor.action;
    let item = match ui.popup {
        Some(
            PopupState::ActionList(ref item, ..) | PopupState::AnchoredActionList { ref item, .. },
        ) => *item.clone(),
        _ => return Ok(false),
    };

    let data = state.data.read();

    match item {
        ActionListItem::SessionHistory(menu) => {
            if !menu.supports_action(action) {
                return Ok(false);
            }
            match action {
                Action::CopyLink => {
                    let links = menu
                        .items()
                        .iter()
                        .map(|entry| super::media_id_link(&entry.media_id))
                        .collect::<Vec<_>>()
                        .join("\n");
                    super::execute_copy_command(links)?;
                    ui.popup = None;
                    ui.session_history_selection.clear();
                    Ok(true)
                }
                Action::AddToPlaylist => {
                    let items = menu
                        .items()
                        .iter()
                        .map(|entry| UnifiedPlaylistItem {
                            media_id: entry.media_id.clone(),
                            title: entry.title.clone(),
                            artists: entry.artists.clone(),
                            duration_ms: entry.duration_ms,
                            provider_url: None,
                            ..UnifiedPlaylistItem::default()
                        })
                        .collect::<Vec<_>>();
                    let options = data
                        .unified_playlists
                        .iter()
                        .map(|playlist| (playlist.name.clone(), playlist.id.clone()))
                        .collect::<Vec<_>>();
                    if options.is_empty() {
                        drop(data);
                        ui.popup = Some(PopupState::DeferredAction {
                            title: "Add to Unified Playlist".to_string(),
                            message: "Create a Unified Playlist before adding history entries."
                                .to_string(),
                        });
                    } else {
                        drop(data);
                        ui.popup = Some(PopupState::UnifiedPlaylistDestination {
                            item_count: items.len(),
                            items,
                            options,
                            state: ListState::default(),
                        });
                    }
                    ui.session_history_selection.clear();
                    Ok(true)
                }
                Action::ClearSessionHistory => {
                    drop(data);
                    ui.popup = Some(PopupState::ConfirmAction {
                        message: "Clear all local session history?".to_string(),
                        action: ConfirmableAction::ClearSessionHistory,
                    });
                    Ok(true)
                }
                _ => Ok(false),
            }
        }
        ActionListItem::Lyrics(menu) => {
            if !menu
                .actions()
                .iter()
                .any(|descriptor| descriptor.action == action)
            {
                return Ok(false);
            }
            match action {
                Action::CopyLyrics | Action::CopyTimedLyrics => {
                    let cache_key = match ui.current_page() {
                        PageState::Lyrics {
                            track_uri,
                            lyrics_provider,
                            ..
                        } if track_uri == menu.track_uri() => {
                            crate::state::LyricsCacheKey::new(track_uri, lyrics_provider.as_deref())
                        }
                        _ => crate::state::LyricsCacheKey::new(menu.track_uri(), None),
                    };
                    let Some(lyrics) = data
                        .caches
                        .lyrics
                        .get(&cache_key)
                        .and_then(|lyrics| lyrics.as_ref())
                    else {
                        drop(data);
                        ui.set_unsupported_operation(
                            "Lyrics are not loaded.",
                            "Retry lyrics, then try copying again.",
                        );
                        return Ok(true);
                    };
                    let text = if action == Action::CopyLyrics {
                        lyrics_plain_text(lyrics)
                    } else if let Some(text) = lyrics_timed_lrc(lyrics) {
                        text
                    } else {
                        drop(data);
                        ui.set_unsupported_operation(
                            "Timed lyrics are unavailable.",
                            "Copy plain lyrics or try another lyrics source.",
                        );
                        return Ok(true);
                    };
                    drop(data);
                    super::execute_copy_command(text)?;
                    ui.popup = None;
                    Ok(true)
                }
                Action::RetryLyrics | Action::CycleLyricsSource => {
                    let command = if action == Action::RetryLyrics {
                        Command::RetryLyrics
                    } else {
                        Command::CycleLyricsSource
                    };
                    // The page handler clears the current lyrics cache before
                    // enqueueing the replacement request. Release the read
                    // guard first so that write lock can be acquired.
                    drop(data);
                    let handled = super::page::handle_command_for_lyrics_page(
                        command, client_pub, state, ui,
                    )?;
                    if handled {
                        ui.popup = None;
                    }
                    Ok(handled)
                }
                _ => Ok(false),
            }
        }
        ActionListItem::YouTubePlaylistContext(menu) => {
            if !menu
                .actions()
                .iter()
                .any(|descriptor| descriptor.action == action)
            {
                return Ok(false);
            }
            let playlist_id = menu.id().to_owned();
            match action {
                Action::CopyLink => {
                    super::execute_copy_command(format!(
                        "https://music.youtube.com/playlist?list={playlist_id}"
                    ))?;
                    ui.popup = None;
                    Ok(true)
                }
                Action::RenamePlaylist => {
                    ui.popup = Some(PopupState::PlaylistName {
                        action: crate::state::PlaylistNameAction::YouTubeMusic { playlist_id },
                        input: crate::ui::single_line_input::LineInput::new(
                            menu.name().chars().collect(),
                        ),
                    });
                    Ok(true)
                }
                Action::DeletePlaylist => {
                    ui.popup = Some(PopupState::ConfirmAction {
                        message: format!("Delete YouTube Music playlist '{}'", menu.name()),
                        action: ConfirmableAction::DeleteYouTubePlaylist(playlist_id),
                    });
                    Ok(true)
                }
                _ => Ok(false),
            }
        }
        ActionListItem::UnifiedPlaylistContext(menu) => {
            if !menu
                .actions()
                .iter()
                .any(|descriptor| descriptor.action == action)
            {
                return Ok(false);
            }
            let playlist_id = menu.id().to_owned();
            match action {
                Action::OpenUnifiedPlaylistListenBrainzSync => {
                    let initial_selection = {
                        let data = state.data.read();
                        let playlist = data
                            .unified_playlists
                            .iter()
                            .find(|playlist| playlist.id == playlist_id);
                        let link = data
                            .playlist_links
                            .iter()
                            .find(|link| link.unified_playlist_id == playlist_id);
                        let lifecycle = match ui.current_page() {
                            crate::state::PageState::UnifiedPlaylist {
                                id,
                                listenbrainz_sync,
                                ..
                            } if id == &playlist_id => *listenbrainz_sync,
                            _ => crate::state::ListenBrainzSyncLifecycle::Idle,
                        };
                        let configs = config::get_config();
                        playlist.and_then(|playlist| {
                            let summary = crate::state::ListenBrainzSyncSummary::project(
                                configs.app_config.listenbrainz.enabled,
                                configs.app_config.listenbrainz.read_only_checking,
                                configs.listenbrainz_token().is_some(),
                                playlist,
                                link,
                                lifecycle,
                            );
                            summary.primary_command_action().and_then(|primary| {
                                crate::state::LISTENBRAINZ_WORKSPACE_ACTIONS
                                    .iter()
                                    .position(|candidate| *candidate == primary)
                            })
                        })
                    };
                    drop(data);
                    let playlist_name = state
                        .data
                        .read()
                        .unified_playlists
                        .iter()
                        .find(|playlist| playlist.id == playlist_id)
                        .map_or_else(|| menu.name().to_owned(), |playlist| playlist.name.clone());
                    let mut list = ListState::default();
                    list.select(Some(initial_selection.unwrap_or(0)));
                    ui.popup = Some(PopupState::ListenBrainzWorkspace {
                        playlist_id,
                        playlist_name,
                        state: list,
                        changes: ratatui::widgets::TableState::default(),
                    });
                    Ok(true)
                }
                Action::CopyLink => {
                    super::execute_copy_command(format!(
                        "unified-player://playlist/{playlist_id}"
                    ))?;
                    ui.popup = None;
                    Ok(true)
                }
                Action::BackupUnifiedPlaylistToListenBrainz => {
                    let configs = config::get_config();
                    let already_linked = data.playlist_links.iter().any(|link| {
                        link.unified_playlist_id == playlist_id
                            && link.listenbrainz_playlist_id.is_some()
                    });
                    drop(data);
                    if !configs.app_config.listenbrainz.enabled {
                        ui.set_unsupported_operation(
                            "ListenBrainz integration is disabled.",
                            "Enable listenbrainz.enabled, restart, then try again.",
                        );
                    } else if configs.listenbrainz_token().is_none() {
                        ui.set_unsupported_operation(
                            "A ListenBrainz user token is required for backups.",
                            "Run unified-player listenbrainz auth, then restart.",
                        );
                    } else if already_linked {
                        ui.set_unsupported_operation(
                            "This Unified playlist already has a ListenBrainz backup.",
                            "Open the ListenBrainz workspace to initialize the sync base.",
                        );
                    } else if let Some(operation_reference) =
                        ui.start_listenbrainz_backup(&playlist_id)
                    {
                        let request = ClientRequest::BackupUnifiedPlaylistToListenBrainz {
                            unified_playlist_id: playlist_id.clone(),
                            operation_reference: operation_reference.clone(),
                        };
                        if let Err(error) = client_pub.send(request) {
                            ui.finish_listenbrainz_backup_failed(
                                &playlist_id,
                                &operation_reference,
                            );
                            return Err(error.into());
                        }
                    }
                    ui.popup = None;
                    Ok(true)
                }
                Action::InitializeUnifiedPlaylistListenBrainzBase => {
                    let configs = config::get_config();
                    let link_state = data
                        .playlist_links
                        .iter()
                        .find(|link| link.unified_playlist_id == playlist_id)
                        .map(|link| {
                            (
                                link.listenbrainz_playlist_id.clone(),
                                link.listenbrainz_sync.is_some(),
                            )
                        });
                    drop(data);
                    if !configs.app_config.listenbrainz.enabled
                        || !configs.app_config.listenbrainz.read_only_checking
                        || configs.listenbrainz_token().is_none()
                    {
                        ui.set_unsupported_operation(
                            "ListenBrainz read-only checking is not ready.",
                            "Enable integration and read-only checking, then configure a token.",
                        );
                        return Ok(true);
                    }
                    match link_state {
                        None | Some((None, _)) => {
                            ui.set_unsupported_operation(
                                "No ListenBrainz backup is linked yet.",
                                "Back up this playlist before initializing the sync base.",
                            );
                            return Ok(true);
                        }
                        Some((Some(_), true)) => {
                            ui.set_unsupported_operation(
                                "The ListenBrainz sync base is already initialized.",
                                "Check for changes or preview a sync direction instead.",
                            );
                            return Ok(true);
                        }
                        Some((Some(_), false)) => {}
                    }
                    if let Some(operation_reference) = ui.start_listenbrainz_sync_check(
                        &playlist_id,
                        crate::state::ListenBrainzSyncLifecycle::Checking,
                    ) {
                        let request = ClientRequest::InitializeUnifiedPlaylistListenBrainzBase {
                            unified_playlist_id: playlist_id.clone(),
                            operation_reference: operation_reference.clone(),
                        };
                        if let Err(error) = client_pub.send(request) {
                            ui.finish_listenbrainz_sync_check_failed(
                                &playlist_id,
                                &operation_reference,
                            );
                            return Err(error.into());
                        }
                    } else {
                        ui.set_unsupported_operation(
                            "A ListenBrainz sync check is already running.",
                            "Wait for the current check to finish.",
                        );
                    }
                    ui.popup = None;
                    Ok(true)
                }
                Action::ApplyUnifiedPlaylistListenBrainzPush
                | Action::ApplyUnifiedPlaylistListenBrainzPull => {
                    let configs = config::get_config();
                    if !configs.app_config.listenbrainz.enabled
                        || !configs.app_config.listenbrainz.read_only_checking
                        || configs.listenbrainz_token().is_none()
                    {
                        drop(data);
                        ui.set_unsupported_operation(
                            "ListenBrainz read-only checking is not ready.",
                            "Enable integration and read-only checking, then configure a token.",
                        );
                        return Ok(true);
                    }
                    let is_push = action == Action::ApplyUnifiedPlaylistListenBrainzPush;
                    let fresh = match ui.current_page() {
                        crate::state::PageState::UnifiedPlaylist {
                            id,
                            listenbrainz_sync,
                            listenbrainz_preview,
                            ..
                        } if id == &playlist_id => match listenbrainz_sync {
                            crate::state::ListenBrainzSyncLifecycle::Ready {
                                remote_changed,
                                conflicts,
                                ..
                            } if *conflicts == 0 => {
                                let local_changed = data
                                    .unified_playlists
                                    .iter()
                                    .find(|playlist| playlist.id == playlist_id)
                                    .zip(
                                        data.playlist_links
                                            .iter()
                                            .find(|link| link.unified_playlist_id == playlist_id)
                                            .and_then(|link| link.listenbrainz_sync.as_ref()),
                                    )
                                    .is_some_and(|(playlist, sync)| {
                                        playlist.snapshot_hash() != sync.base.local_snapshot_hash
                                    });
                                (is_push == local_changed)
                                    && (is_push || *remote_changed)
                                    && listenbrainz_preview.is_some()
                            }
                            _ => false,
                        },
                        _ => false,
                    };
                    if !fresh {
                        drop(data);
                        ui.set_unsupported_operation(
                            if is_push {
                                "No fresh push preview is available."
                            } else {
                                "No fresh pull preview is available."
                            },
                            "Preview the direction first, then apply without changing the playlist.",
                        );
                        return Ok(true);
                    }
                    let linked = data.playlist_links.iter().any(|link| {
                        link.unified_playlist_id == playlist_id
                            && link.listenbrainz_playlist_id.is_some()
                    });
                    drop(data);
                    if !linked {
                        ui.set_unsupported_operation(
                            "No ListenBrainz backup is linked yet.",
                            "Back up this playlist before applying.",
                        );
                        return Ok(true);
                    }
                    let preview = match ui.current_page() {
                        crate::state::PageState::UnifiedPlaylist {
                            id,
                            listenbrainz_preview,
                            ..
                        } if id == &playlist_id => listenbrainz_preview.clone(),
                        _ => None,
                    };
                    let Some(preview) = preview else {
                        ui.set_unsupported_operation(
                            "The preview is no longer available.",
                            "Preview the direction first, then apply.",
                        );
                        return Ok(true);
                    };
                    let (added, removed) =
                        preview
                            .rows
                            .iter()
                            .fold((0usize, 0usize), |(added, removed), row| {
                                let mine = if is_push {
                                    row.side == crate::state::ListenBrainzSyncSide::Local
                                } else {
                                    row.side == crate::state::ListenBrainzSyncSide::ListenBrainz
                                };
                                match (&row.action, mine) {
                                    (crate::state::ListenBrainzSyncDetailAction::Added, true) => {
                                        (added + 1, removed)
                                    }
                                    (crate::state::ListenBrainzSyncDetailAction::Removed, true) => {
                                        (added, removed + 1)
                                    }
                                    _ => (added, removed),
                                }
                            });
                    let confirm_action = if is_push {
                        ConfirmableAction::ApplyUnifiedPlaylistListenBrainzPush {
                            unified_playlist_id: playlist_id,
                        }
                    } else {
                        ConfirmableAction::ApplyUnifiedPlaylistListenBrainzPull {
                            unified_playlist_id: playlist_id,
                        }
                    };
                    ui.popup = Some(PopupState::ConfirmAction {
                        message: if is_push {
                            format!("Push {added} added, {removed} removed rows to ListenBrainz?")
                        } else {
                            format!(
                                "Pull {added} added, {removed} removed rows locally? A rollback snapshot will be kept."
                            )
                        },
                        action: confirm_action,
                    });
                    Ok(true)
                }
                Action::ApplyUnifiedPlaylistListenBrainzResolve => {
                    let configs = config::get_config();
                    if !configs.app_config.listenbrainz.enabled
                        || !configs.app_config.listenbrainz.read_only_checking
                        || configs.listenbrainz_token().is_none()
                    {
                        drop(data);
                        ui.set_unsupported_operation(
                            "ListenBrainz read-only checking is not ready.",
                            "Enable integration and read-only checking, then configure a token.",
                        );
                        return Ok(true);
                    }
                    let preview = match ui.current_page() {
                        crate::state::PageState::UnifiedPlaylist {
                            id,
                            listenbrainz_preview,
                            ..
                        } if id == &playlist_id => listenbrainz_preview.clone(),
                        _ => None,
                    };
                    drop(data);
                    let Some(preview) = preview.filter(|preview| !preview.conflicts.is_empty())
                    else {
                        ui.set_unsupported_operation(
                            "No fresh conflict preview is available.",
                            "Preview or review conflicts first, then resolve.",
                        );
                        return Ok(true);
                    };
                    let playlist_name = state
                        .data
                        .read()
                        .unified_playlists
                        .iter()
                        .find(|playlist| playlist.id == preview.playlist_id)
                        .map_or_else(|| menu.name().to_owned(), |playlist| playlist.name.clone());
                    let mut list = ListState::default();
                    list.select(Some(0));
                    ui.popup = Some(PopupState::ListenBrainzResolve {
                        menu: crate::state::ListenBrainzResolveMenu::new(
                            preview.playlist_id,
                            playlist_name,
                            preview.conflicts,
                        ),
                        state: list,
                    });
                    Ok(true)
                }
                Action::CheckUnifiedPlaylistListenBrainzSync
                | Action::RefreshUnifiedPlaylistListenBrainzSync
                | Action::RetryUnifiedPlaylistListenBrainzSync
                | Action::PreviewUnifiedPlaylistListenBrainzPush
                | Action::PreviewUnifiedPlaylistListenBrainzPull
                | Action::RecoverUnifiedPlaylistListenBrainzSync => {
                    let existing_preview = match ui.current_page() {
                        crate::state::PageState::UnifiedPlaylist {
                            id,
                            listenbrainz_preview,
                            ..
                        } if id == &playlist_id => listenbrainz_preview.clone(),
                        _ => None,
                    };
                    if let Some(preview) = existing_preview.filter(|_| {
                        matches!(
                            action,
                            Action::PreviewUnifiedPlaylistListenBrainzPush
                                | Action::PreviewUnifiedPlaylistListenBrainzPull
                        )
                    }) {
                        drop(data);
                        let mut list = ListState::default();
                        list.select((!preview.rows.is_empty()).then_some(0));
                        ui.popup = Some(PopupState::ListenBrainzSyncDetails {
                            preview,
                            state: list,
                        });
                        return Ok(true);
                    }
                    let configs = config::get_config();
                    if !configs.app_config.listenbrainz.enabled
                        || !configs.app_config.listenbrainz.read_only_checking
                        || configs.listenbrainz_token().is_none()
                    {
                        drop(data);
                        ui.set_unsupported_operation(
                            "ListenBrainz read-only checking is not ready.",
                            "Enable integration and read-only checking, then configure a token.",
                        );
                        return Ok(true);
                    }
                    let sync_status = data
                        .playlist_links
                        .iter()
                        .find(|link| link.unified_playlist_id == playlist_id)
                        .and_then(|link| link.listenbrainz_sync.as_ref())
                        .map(|sync| sync.status);
                    if action == Action::RetryUnifiedPlaylistListenBrainzSync
                        && matches!(
                            sync_status,
                            Some(
                                crate::state::ListenBrainzSyncStatus::Pending
                                    | crate::state::ListenBrainzSyncStatus::OutcomeUnknown
                            )
                        )
                    {
                        drop(data);
                        ui.set_unsupported_operation(
                            "Retry is blocked while the remote outcome is unknown.",
                            "Refresh the read-back until the outcome is resolved.",
                        );
                        return Ok(true);
                    }
                    let lifecycle = match action {
                        Action::CheckUnifiedPlaylistListenBrainzSync
                        | Action::RefreshUnifiedPlaylistListenBrainzSync => {
                            crate::state::ListenBrainzSyncLifecycle::Checking
                        }
                        Action::RetryUnifiedPlaylistListenBrainzSync
                        | Action::PreviewUnifiedPlaylistListenBrainzPush
                        | Action::PreviewUnifiedPlaylistListenBrainzPull => {
                            crate::state::ListenBrainzSyncLifecycle::Planning
                        }
                        Action::RecoverUnifiedPlaylistListenBrainzSync => {
                            crate::state::ListenBrainzSyncLifecycle::Recovering
                        }
                        _ => unreachable!("sync action group is exhaustive"),
                    };
                    drop(data);
                    if let Some(operation_reference) =
                        ui.start_listenbrainz_sync_check(&playlist_id, lifecycle)
                    {
                        let request = ClientRequest::CheckUnifiedPlaylistListenBrainzSync {
                            unified_playlist_id: playlist_id.clone(),
                            operation_reference: operation_reference.clone(),
                            mode: if action == Action::RecoverUnifiedPlaylistListenBrainzSync {
                                crate::client::ListenBrainzSyncReadMode::Recovery
                            } else {
                                crate::client::ListenBrainzSyncReadMode::Preview
                            },
                        };
                        if let Err(error) = client_pub.send(request) {
                            ui.finish_listenbrainz_sync_check_failed(
                                &playlist_id,
                                &operation_reference,
                            );
                            return Err(error.into());
                        }
                    } else {
                        ui.set_unsupported_operation(
                            "A ListenBrainz sync check is already running.",
                            "Wait for the current check to finish.",
                        );
                    }
                    ui.popup = None;
                    Ok(true)
                }
                Action::ReviewUnifiedPlaylistListenBrainzConflicts => {
                    drop(data);
                    let preview = match ui.current_page() {
                        crate::state::PageState::UnifiedPlaylist {
                            id,
                            listenbrainz_preview,
                            ..
                        } if id == &playlist_id => listenbrainz_preview.clone(),
                        _ => None,
                    };
                    if let Some(preview) = preview {
                        let mut list = ListState::default();
                        list.select((!preview.rows.is_empty()).then_some(0));
                        ui.popup = Some(PopupState::ListenBrainzSyncDetails {
                            preview,
                            state: list,
                        });
                    } else {
                        ui.set_unsupported_operation(
                            "Conflict details are not available yet.",
                            "Refresh the read-only preview, then review conflicts.",
                        );
                    }
                    Ok(true)
                }
                Action::RollbackUnifiedPlaylistListenBrainzPull => {
                    let remote_playlist_id = data
                        .playlist_links
                        .iter()
                        .find(|link| link.unified_playlist_id == playlist_id)
                        .and_then(|link| {
                            link.listenbrainz_sync
                                .as_ref()
                                .and_then(|sync| sync.local_apply_snapshot.as_ref())
                                .and_then(|_| link.listenbrainz_playlist_id.clone())
                        });
                    drop(data);
                    if let Some(listenbrainz_playlist_id) = remote_playlist_id {
                        ui.popup = Some(PopupState::ConfirmAction {
                            message: "Roll back the last ListenBrainz pull?".to_owned(),
                            action: ConfirmableAction::RollbackUnifiedPlaylistListenBrainzPull {
                                unified_playlist_id: playlist_id,
                                listenbrainz_playlist_id,
                            },
                        });
                    } else {
                        ui.set_unsupported_operation(
                            "No ListenBrainz pull rollback is available.",
                            "Refresh the sync summary before trying again.",
                        );
                    }
                    Ok(true)
                }
                Action::LinkUnifiedPlaylistToYouTube => {
                    drop(data);
                    ui.youtube_auth_status = config::get_config().youtube_music_auth_status();
                    if ui.youtube_auth_status.missing_message().is_some() {
                        ui.set_unsupported_operation(
                            "YouTube Music is not ready for playlist linking.",
                            "Authenticate YouTube Music, then try again.",
                        );
                    } else {
                        client_pub.send(ClientRequest::GetYouTubeLibrary)?;
                        ui.popup = Some(PopupState::YouTubePlaylistList(
                            YouTubePlaylistPopupAction::LinkUnified {
                                unified_playlist_id: playlist_id,
                                search_query: String::new(),
                            },
                            ListState::default(),
                        ));
                    }
                    Ok(true)
                }
                Action::SyncUnifiedPlaylistToYouTube => {
                    let (account_id, account_epoch) = current_youtube_projection_scope(ui);
                    let projection_status = data.unified_playlist_projection_status(
                        &playlist_id,
                        &account_id,
                        account_epoch,
                    );
                    let projection_preview = data.unified_playlist_projection_preview(
                        &playlist_id,
                        &account_id,
                        account_epoch,
                    );
                    drop(data);
                    match projection_status {
                        None => ui.set_unsupported_operation(
                            "This Unified playlist has no YouTube link.",
                            "Link it to a YouTube playlist first.",
                        ),
                        Some(crate::state::PlaylistProjectionStatus::OutcomeUnknown) => ui
                            .set_unsupported_operation(
                                "The remote projection outcome is unknown.",
                                "Refresh the linked target or relink it; retry remains blocked until verified.",
                            ),
                        Some(status) if status.needs_preview_confirmation() => {
                            ui.popup = Some(PopupState::ConfirmAction {
                                message: format!(
                                    "Previewed {status:?} projection ({}); reconcile Unified playlist?",
                                    projection_preview
                                        .as_deref()
                                        .unwrap_or("conflict details unavailable")
                                ),
                                action: ConfirmableAction::ReconcileUnifiedPlaylistProjection(
                                    playlist_id,
                                ),
                            });
                        }
                        Some(_) => {
                            dispatch_legacy_playlist(
                                client_pub,
                                ClientRequest::SyncUnifiedPlaylistToYouTube {
                                    unified_playlist_id: playlist_id,
                                },
                            )?;
                            ui.popup = None;
                        }
                    }
                    Ok(true)
                }
                Action::UnlinkUnifiedPlaylistFromYouTube => {
                    let linked = data.playlist_links.iter().any(|link| {
                        link.unified_playlist_id == playlist_id
                            && link.youtube_playlist_id.is_some()
                    });
                    drop(data);
                    if linked {
                        dispatch_legacy_playlist(
                            client_pub,
                            ClientRequest::UnlinkUnifiedPlaylistFromYouTube {
                                unified_playlist_id: playlist_id,
                            },
                        )?;
                        ui.popup = None;
                    } else {
                        ui.set_unsupported_operation(
                            "This Unified playlist has no YouTube link.",
                            "Link it before trying to unlink it.",
                        );
                    }
                    Ok(true)
                }
                Action::RenamePlaylist => {
                    let name = menu.name().to_owned();
                    ui.popup = Some(PopupState::PlaylistName {
                        action: crate::state::PlaylistNameAction::Unified { playlist_id },
                        input: crate::ui::single_line_input::LineInput::new(name.chars().collect()),
                    });
                    Ok(true)
                }
                Action::DeletePlaylist => {
                    ui.popup = Some(PopupState::ConfirmAction {
                        message: format!("Delete Unified playlist '{}'", menu.name()),
                        action: ConfirmableAction::DeleteUnifiedPlaylist(playlist_id),
                    });
                    Ok(true)
                }
                _ => Ok(false),
            }
        }
        ActionListItem::Track(track, _actions) => {
            if journal_destructive_action_uses_page_projection(
                action,
                ui.current_page().page_type(),
            ) {
                drop(data);
                return super::page::handle_safe_journal_destructive_action(
                    action, track, state, ui,
                );
            }
            if is_track_journal_action(action) {
                drop(data);
                return handle_track_journal_action(action, track, state, ui);
            }
            handle_action_in_context(action, track.into(), client_pub, &data, ui)
        }
        ActionListItem::YouTubeTrack(track, _actions) => {
            handle_action_in_context(action, track.into(), client_pub, &data, ui)
        }
        ActionListItem::YouTubeTracks(menu) => {
            if !menu.supports_action(action) {
                return Ok(false);
            }
            if action != Action::AddToJournalList
                && replan_youtube_menu(&menu, current_youtube_epoch(ui), action).is_err()
            {
                return Ok(false);
            }
            handle_action_in_context(
                action,
                ActionContext::YouTubeTracks(menu.items().to_vec()),
                client_pub,
                &data,
                ui,
            )
        }
        ActionListItem::Tracks(menu) => {
            if !menu.supports_action(action) {
                return Ok(false);
            }
            if replan_spotify_menu(&menu, current_spotify_epoch(ui), action).is_err() {
                return Ok(false);
            }
            if action == Action::AddToListenLater {
                drop(data);
                handle_bulk_add_to_listen_later(menu.items().to_vec(), state, ui)?;
                window::clear_track_selection(ui);
                return Ok(true);
            }
            handle_action_in_context(
                action,
                ActionContext::Tracks(menu.items().to_vec()),
                client_pub,
                &data,
                ui,
            )
        }
        ActionListItem::Queue(menu) => {
            if !menu.supports_action(action) {
                return Ok(false);
            }
            if action == Action::CopyLink {
                let links = menu
                    .items()
                    .iter()
                    .map(|item| super::media_id_link(item.media_id()))
                    .collect::<Vec<_>>()
                    .join("\n");
                super::execute_copy_command(links)?;
                ui.popup = None;
                window::clear_track_selection(ui);
                return Ok(true);
            }
            drop(data);
            let Some((scope, rows)) = super::page::synchronize_queue_page_selection(ui, state)
            else {
                return Ok(false);
            };
            let Some(current_items) = super::page::queue_action_items_for_snapshot(&menu, &rows)
            else {
                return Ok(false);
            };
            let Ok(plan) = super::bulk_action::replan_queue_menu(&menu, &scope, &current_items)
            else {
                return Ok(false);
            };
            super::bulk_action::dispatch_queue_menu(&menu, &plan, ui, client_pub)
                .map_err(|error| anyhow::anyhow!(error.to_string()))?;
            ui.popup = None;
            window::clear_track_selection(ui);
            Ok(true)
        }
        ActionListItem::UnifiedPlaylist(menu) => {
            if !menu.supports_action(action) {
                return Ok(false);
            }
            if action == Action::CopyLink {
                let links = menu
                    .items()
                    .iter()
                    .map(|item| super::unified_playlist_item_link(item.item()))
                    .collect::<Vec<_>>()
                    .join("\n");
                super::execute_copy_command(links)?;
                ui.popup = None;
                window::clear_track_selection(ui);
                return Ok(true);
            }
            if action == Action::DeleteFromPlaylist {
                let (playlist_id, rows) = match ui.current_page() {
                    PageState::UnifiedPlaylist { id, .. } => {
                        let rows = data
                            .unified_playlists
                            .iter()
                            .find(|playlist| playlist.id == *id)
                            .map(|playlist| playlist.items.clone());
                        (id.clone(), rows)
                    }
                    _ => (String::new(), None),
                };
                let Some(rows) = rows else {
                    return Ok(false);
                };
                let scope = crate::state::UnifiedPlaylistSelectionScope::new(playlist_id);
                let Some(current_items) =
                    super::page::unified_playlist_action_items_for_snapshot(&menu, &rows)
                else {
                    return Ok(false);
                };
                if super::bulk_action::replan_unified_playlist_menu(
                    Action::DeleteFromPlaylist,
                    &menu,
                    &scope,
                    &current_items,
                )
                .is_err()
                {
                    return Ok(false);
                }
                let count = menu.items().len();
                drop(data);
                ui.popup = Some(PopupState::ConfirmAction {
                    message: format!(
                        "Remove {count} selected occurrence{} from this Unified playlist?",
                        if count == 1 { "" } else { "s" }
                    ),
                    action: ConfirmableAction::RemoveUnifiedPlaylistEntries(menu),
                });
                return Ok(true);
            }
            if action == Action::AddToPlaylist {
                let source_playlist_id = match ui.current_page() {
                    PageState::UnifiedPlaylist { id, .. } => id.clone(),
                    _ => return Ok(false),
                };
                let options = data
                    .unified_playlists
                    .iter()
                    .filter(|playlist| playlist.id != source_playlist_id)
                    .map(|playlist| (playlist.name.clone(), playlist.id.clone()))
                    .collect::<Vec<_>>();
                if options.is_empty() {
                    drop(data);
                    ui.popup = Some(PopupState::DeferredAction {
                        title: "Add to Unified Playlist".to_string(),
                        message: "No other Unified Playlist is available as a destination yet."
                            .to_string(),
                    });
                    return Ok(true);
                }
                ui.popup = Some(PopupState::UnifiedPlaylistDestination {
                    item_count: menu.items().len(),
                    items: menu
                        .items()
                        .iter()
                        .map(|item| item.item().clone())
                        .collect(),
                    options,
                    state: ListState::default(),
                });
                return Ok(true);
            }
            if action != Action::AddToQueue {
                return Ok(false);
            }
            let (playlist_id, rows) = match ui.current_page() {
                PageState::UnifiedPlaylist { id, .. } => {
                    let rows = data
                        .unified_playlists
                        .iter()
                        .find(|playlist| playlist.id == *id)
                        .map(|playlist| playlist.items.clone());
                    (id.clone(), rows)
                }
                _ => (String::new(), None),
            };
            let Some(rows) = rows else {
                return Ok(false);
            };
            drop(data);
            let scope = crate::state::UnifiedPlaylistSelectionScope::new(playlist_id);
            let Some(current_items) =
                super::page::unified_playlist_action_items_for_snapshot(&menu, &rows)
            else {
                return Ok(false);
            };
            let Ok(plan) = super::bulk_action::replan_unified_playlist_menu(
                Action::AddToQueue,
                &menu,
                &scope,
                &current_items,
            ) else {
                return Ok(false);
            };
            super::bulk_action::dispatch_unified_playlist_menu(&menu, &plan, ui, client_pub)
                .map_err(|error| anyhow::anyhow!(error.to_string()))?;
            ui.popup = None;
            window::clear_track_selection(ui);
            Ok(true)
        }
        ActionListItem::Album(album, _actions) => {
            handle_action_in_context(action, album.into(), client_pub, &data, ui)
        }
        ActionListItem::Artist(artist, _actions) => {
            handle_action_in_context(action, artist.into(), client_pub, &data, ui)
        }
        ActionListItem::Playlist(playlist, _actions) => {
            handle_action_in_context(action, playlist.into(), client_pub, &data, ui)
        }
        ActionListItem::Show(show, _actions) => {
            handle_action_in_context(action, show.into(), client_pub, &data, ui)
        }
        ActionListItem::Episode(episode, _actions) => {
            handle_action_in_context(action, episode.into(), client_pub, &data, ui)
        }
    }
}

/// Handle key sequence for playlist search popup (AddTrack/AddEpisode)
fn handle_key_sequence_for_playlist_search_popup(
    key_sequence: &KeySequence,
    ui: &mut UIStateGuard,
) -> bool {
    // Handle user's input that updates the search query
    let Some(PopupState::UserPlaylistList(ref mut action, _)) = &mut ui.popup else {
        return false;
    };

    let search_query = match action {
        PlaylistPopupAction::AddTrack { search_query, .. }
        | PlaylistPopupAction::AddTracks { search_query, .. }
        | PlaylistPopupAction::AddEpisode { search_query, .. }
        | PlaylistPopupAction::Browse { search_query, .. } => search_query,
    };

    if key_sequence.keys.len() == 1 {
        if let Key::None(c) = key_sequence.keys[0] {
            match c {
                crossterm::event::KeyCode::Char(c) => {
                    search_query.push(c);
                    // Reset selection to first item when search query changes
                    if let Some(popup) = &mut ui.popup {
                        popup.list_select(Some(0));
                    }
                    return true;
                }
                crossterm::event::KeyCode::Backspace => {
                    if search_query.is_empty() {
                        // Close playlist popup when user presses backspace on empty search
                        ui.popup = None;
                    } else {
                        search_query.pop();
                        // Reset selection to first item when search query changes
                        if let Some(popup) = &mut ui.popup {
                            popup.list_select(Some(0));
                        }
                    }
                    return true;
                }
                _ => {}
            }
        }
    }

    false
}

fn handle_key_sequence_for_youtube_playlist_search_popup(
    key_sequence: &KeySequence,
    ui: &mut UIStateGuard,
) -> bool {
    let Some(PopupState::YouTubePlaylistList(ref mut action, _)) = &mut ui.popup else {
        return false;
    };
    let search_query = match action {
        YouTubePlaylistPopupAction::AddTrack { search_query, .. }
        | YouTubePlaylistPopupAction::AddTracks { search_query, .. }
        | YouTubePlaylistPopupAction::LinkUnified { search_query, .. } => search_query,
    };
    if key_sequence.keys.len() == 1 {
        if let Key::None(c) = key_sequence.keys[0] {
            match c {
                crossterm::event::KeyCode::Char(c) => {
                    search_query.push(c);
                    if let Some(popup) = &mut ui.popup {
                        popup.list_select(Some(0));
                    }
                    return true;
                }
                crossterm::event::KeyCode::Backspace => {
                    if search_query.is_empty() {
                        ui.popup = None;
                    } else {
                        search_query.pop();
                        if let Some(popup) = &mut ui.popup {
                            popup.list_select(Some(0));
                        }
                    }
                    return true;
                }
                _ => {}
            }
        }
    }
    false
}

fn handle_key_sequence_for_confirm_popup(
    key_sequence: &KeySequence,
    client_pub: &crate::client::ClientRequestSender,
    state: &SharedState,
    ui: &mut UIStateGuard,
    action: ConfirmableAction,
) -> Result<bool> {
    if matches!(
        key_sequence.keys.as_slice(),
        [Key::None(crossterm::event::KeyCode::Char('y'))]
    ) {
        match action {
            ConfirmableAction::SpotifyPlaylistMutation(intent) => {
                dispatch_legacy_playlist(
                    client_pub,
                    ClientRequest::SpotifyPlaylistMutation(intent),
                )?;
            }
            ConfirmableAction::YouTubePlaylistMutation(intent) => {
                dispatch_legacy_playlist(
                    client_pub,
                    ClientRequest::YouTubePlaylistMutation(intent),
                )?;
            }
            ConfirmableAction::DeleteTracksFromPlaylist {
                playlist_id,
                tracks: menu,
            } => {
                let data = state.data.read();
                let track_ids = menu
                    .items()
                    .iter()
                    .map(|track| track.id.clone())
                    .collect::<Vec<_>>();
                if window::playlist_delete_is_safe(&data, &playlist_id, Some(&track_ids)) {
                    let current_page_matches = matches!(
                        ui.current_page(),
                        crate::state::PageState::Context {
                            id: Some(crate::state::ContextId::Playlist(current_id)),
                            ..
                        } if current_id == &playlist_id
                    );
                    if current_page_matches {
                        let snapshot_id = data
                            .caches
                            .context
                            .get(&playlist_id.uri())
                            .and_then(|context| match context {
                                crate::state::Context::Playlist { playlist, .. } => {
                                    Some(playlist.snapshot_id.clone())
                                }
                                _ => None,
                            })
                            .filter(|revision| !revision.is_empty())
                            .context("playlist deletion requires a current snapshot")?;
                        let plan = replan_spotify_menu_for_owner(
                            &menu,
                            current_spotify_epoch(ui),
                            Action::DeleteFromPlaylist,
                            crate::command::BulkActionOwner::Provider(
                                crate::state::Provider::Spotify,
                            ),
                        )
                        .map_err(|_| anyhow::anyhow!("playlist deletion target is stale"))?;
                        let operation_ids = plan.operation_ids();
                        let intent = spotify_remove_all_intent(
                            playlist_id.uri(),
                            menu.items().iter().map(|track| track.id.uri()).collect(),
                            snapshot_id,
                            *operation_ids
                                .first()
                                .context("playlist deletion plan has no operation identity")?,
                        );
                        let assignments = vec![super::bulk_action::BulkRequestAssignment::new(
                            playlist_request(ClientRequest::SpotifyPlaylistMutation(intent))?,
                            operation_ids,
                        )];
                        super::bulk_action::dispatch_bulk_requests(
                            ui,
                            client_pub,
                            &plan,
                            assignments,
                        )?;
                        window::clear_track_selection(ui);
                    } else {
                        tracing::warn!("Playlist deletion confirmation refused by page gate");
                    }
                } else {
                    tracing::warn!("Playlist deletion confirmation refused by safety gate");
                }
            }
            ConfirmableAction::DeleteFromLibrary(item_id) => {
                client_pub.send(ClientRequest::DeleteFromLibrary(item_id))?;
            }
            ConfirmableAction::DeleteYouTubePlaylist(playlist_id) => {
                dispatch_legacy_playlist(
                    client_pub,
                    ClientRequest::DeleteYouTubePlaylist { playlist_id },
                )?;
            }
            ConfirmableAction::DeleteUnifiedPlaylist(playlist_id) => {
                dispatch_legacy_playlist(
                    client_pub,
                    ClientRequest::DeleteUnifiedPlaylist { playlist_id },
                )?;
            }
            ConfirmableAction::ReconcileUnifiedPlaylistProjection(playlist_id) => {
                let (account_id, account_epoch) = current_youtube_projection_scope(ui);
                let status = state.data.read().unified_playlist_projection_status(
                    &playlist_id,
                    &account_id,
                    account_epoch,
                );
                match status {
                    Some(status)
                        if status.needs_preview_confirmation()
                            && status != crate::state::PlaylistProjectionStatus::OutcomeUnknown =>
                    {
                        dispatch_legacy_playlist(
                            client_pub,
                            ClientRequest::SyncUnifiedPlaylistToYouTube {
                                unified_playlist_id: playlist_id,
                            },
                        )?;
                    }
                    Some(crate::state::PlaylistProjectionStatus::OutcomeUnknown) => {
                        ui.set_unsupported_operation(
                            "The remote projection outcome is unknown.",
                            "Refresh the linked target or relink it; retry remains blocked until verified.",
                        );
                    }
                    _ => tracing::warn!("projection reconciliation confirmation became stale"),
                }
            }
            ConfirmableAction::RollbackUnifiedPlaylistListenBrainzPull {
                unified_playlist_id,
                listenbrainz_playlist_id,
            } => {
                state.data.write().rollback_listenbrainz_pull_apply(
                    &unified_playlist_id,
                    &listenbrainz_playlist_id,
                )?;
                ui.reset_listenbrainz_sync_lifecycle(&unified_playlist_id);
            }
            ConfirmableAction::ApplyUnifiedPlaylistListenBrainzPush {
                unified_playlist_id,
            } => {
                send_listenbrainz_apply(
                    client_pub,
                    ui,
                    &unified_playlist_id,
                    |unified, reference| ClientRequest::ApplyUnifiedPlaylistListenBrainzPush {
                        unified_playlist_id: unified,
                        operation_reference: reference.clone(),
                        operation_id: reference,
                    },
                )?;
            }
            ConfirmableAction::ApplyUnifiedPlaylistListenBrainzPull {
                unified_playlist_id,
            } => {
                send_listenbrainz_apply(
                    client_pub,
                    ui,
                    &unified_playlist_id,
                    |unified, reference| ClientRequest::ApplyUnifiedPlaylistListenBrainzPull {
                        unified_playlist_id: unified,
                        operation_reference: reference.clone(),
                        operation_id: reference,
                    },
                )?;
            }
            ConfirmableAction::ApplyUnifiedPlaylistListenBrainzResolve {
                unified_playlist_id,
                policy,
                decisions,
            } => {
                let Some(reference) = ui.start_listenbrainz_sync_check(
                    &unified_playlist_id,
                    crate::state::ListenBrainzSyncLifecycle::WritingListenBrainz,
                ) else {
                    ui.set_unsupported_operation(
                        "A ListenBrainz sync check is already running.",
                        "Wait for the current check to finish.",
                    );
                    return Ok(true);
                };
                let request = ClientRequest::ApplyUnifiedPlaylistListenBrainzResolve {
                    unified_playlist_id: unified_playlist_id.clone(),
                    operation_reference: reference.clone(),
                    operation_id: reference.clone(),
                    policy,
                    decisions,
                };
                if let Err(error) = client_pub.send(request) {
                    ui.finish_listenbrainz_sync_apply_failed(
                        &unified_playlist_id,
                        &reference,
                        crate::state::LISTENBRAINZ_SYNC_APPLY_FAILED_MESSAGE,
                        crate::state::LISTENBRAINZ_SYNC_APPLY_FAILED_NEXT_ACTION,
                    );
                    return Err(error.into());
                }
            }
            ConfirmableAction::RemoveUnifiedPlaylistEntries(menu) => {
                remove_confirmed_unified_playlist_entries(menu, state, ui)?;
            }
            ConfirmableAction::DeleteJournalList(list_id) => {
                update_track_journal(state, |journal| journal.delete_list(&list_id))?;
            }
            ConfirmableAction::ClearSessionHistory => {
                state.data.write().clear_session_history()?;
                ui.session_history_selection.clear();
            }
            ConfirmableAction::ClearHomeHistory => {
                state.data.write().clear_context_history()?;
            }
            ConfirmableAction::RemoveAccount {
                provider,
                account_id,
            } => {
                if account_id
                    == config::AccountRegistry::load(&config::get_config().config_folder)?
                        .active_id(provider)
                        .unwrap_or_default()
                {
                    client_pub.send(ClientRequest::ManageAccount(
                        crate::client::AccountOperation::Remove(provider),
                    ))?;
                }
            }
            ConfirmableAction::ResetAllConfiguration => {
                client_pub.send(ClientRequest::ResetAllConfiguration)?;
            }
        }
    }
    ui.popup = None;
    Ok(true)
}

/// Start a confirmed `ListenBrainz` apply: the busy lifecycle begins only after
/// the user confirms, and the confirmed operation id doubles as the
/// transaction identity persisted in the pending intent for recovery.
fn send_listenbrainz_apply(
    client_pub: &crate::client::ClientRequestSender,
    ui: &mut UIStateGuard,
    unified_playlist_id: &str,
    request: impl FnOnce(String, String) -> ClientRequest,
) -> Result<()> {
    let Some(reference) = ui.start_listenbrainz_sync_check(
        unified_playlist_id,
        crate::state::ListenBrainzSyncLifecycle::WritingListenBrainz,
    ) else {
        ui.set_unsupported_operation(
            "A ListenBrainz sync check is already running.",
            "Wait for the current check to finish.",
        );
        return Ok(());
    };
    let send = request(unified_playlist_id.to_owned(), reference.clone());
    if let Err(error) = client_pub.send(send) {
        ui.finish_listenbrainz_sync_apply_failed(
            unified_playlist_id,
            &reference,
            crate::state::LISTENBRAINZ_SYNC_APPLY_FAILED_MESSAGE,
            crate::state::LISTENBRAINZ_SYNC_APPLY_FAILED_NEXT_ACTION,
        );
        return Err(error.into());
    }
    Ok(())
}

fn spotify_remove_all_intent(
    playlist_id: String,
    media_uris: Vec<String>,
    snapshot_id: String,
    operation_id: crate::command::BulkOperationId,
) -> crate::client::SpotifyMutationIntent {
    let mut unique_media_uris = Vec::with_capacity(media_uris.len());
    for media_uri in media_uris {
        if !unique_media_uris.contains(&media_uri) {
            unique_media_uris.push(media_uri);
        }
    }
    crate::client::SpotifyMutationIntent::RemoveAllMedia {
        operation_id: provider_operation_id(operation_id),
        playlist_id,
        media_uris: unique_media_uris,
        snapshot_id,
    }
}

fn provider_operation_id(
    operation_id: crate::command::BulkOperationId,
) -> crate::client::PlaylistMutationOperationId {
    crate::client::PlaylistMutationOperationId(operation_id.index() as u64)
}

fn remove_confirmed_unified_playlist_entries(
    menu: crate::state::UnifiedPlaylistActionMenu,
    state: &SharedState,
    ui: &mut UIStateGuard,
) -> Result<()> {
    let playlist_id = menu.scope().playlist_id().to_owned();
    if !matches!(
        ui.current_page(),
        PageState::UnifiedPlaylist { id, .. } if id == &playlist_id
    ) {
        anyhow::bail!("Unified playlist removal scope is stale");
    }
    let rows = state
        .data
        .read()
        .unified_playlists
        .iter()
        .find(|playlist| playlist.id == playlist_id)
        .map(|playlist| playlist.items.clone())
        .ok_or_else(|| anyhow::anyhow!("Unified playlist removal target is missing"))?;
    let current_items = super::page::unified_playlist_action_items_for_snapshot(&menu, &rows)
        .ok_or_else(|| anyhow::anyhow!("Unified playlist removal snapshot is stale"))?;
    let plan = super::bulk_action::replan_unified_playlist_menu(
        Action::DeleteFromPlaylist,
        &menu,
        menu.scope(),
        &current_items,
    )
    .map_err(|_| anyhow::anyhow!("Unified playlist removal plan is stale"))?;
    let entry_ids = plan
        .partitions()
        .iter()
        .flat_map(|partition| partition.operations())
        .flat_map(|operation| operation.occurrence_keys())
        .map(|occurrence| occurrence.occurrence_token().copied())
        .collect::<Option<Vec<_>>>()
        .ok_or_else(|| anyhow::anyhow!("Unified playlist removal has no occurrence token"))?;
    let expected_order = rows.iter().map(|item| item.entry_id).collect::<Vec<_>>();
    let first_removed_index = rows
        .iter()
        .position(|item| entry_ids.contains(&item.entry_id))
        .ok_or_else(|| anyhow::anyhow!("Unified playlist removal entries are missing"))?;
    let operation_ids = plan.operation_ids();
    let operation_id = provider_operation_id(
        *operation_ids
            .first()
            .context("Unified playlist removal plan has no operation identity")?,
    );
    PlaylistApplicationService::plan(PlaylistRequest::new(
        operation_id,
        PlaylistRequestKind::RemoveUnifiedOccurrences {
            playlist_id: playlist_id.clone(),
            expected_order: expected_order.clone(),
            entry_ids: entry_ids.clone(),
        },
    ))?;
    let prior_cursor = ui.current_page().unified_playlist_cursor_entry_id();
    let handle = super::bulk_action::start_bulk_local(ui, &plan, &operation_ids)
        .map_err(|error| anyhow::anyhow!(error.to_string()))?;
    if let Err(error) = state.data.write().remove_unified_playlist_items_if_current(
        &playlist_id,
        &expected_order,
        &entry_ids,
    ) {
        super::bulk_action::complete_bulk_local(ui, &handle, &operation_ids, false)
            .map_err(|record_error| anyhow::anyhow!(record_error.to_string()))?;
        return Err(error);
    }
    super::bulk_action::complete_bulk_local(ui, &handle, &operation_ids, true)
        .map_err(|error| anyhow::anyhow!(error.to_string()))?;

    let fresh_items = state
        .data
        .read()
        .unified_playlists
        .iter()
        .find(|playlist| playlist.id == playlist_id)
        .map(|playlist| playlist.items.clone())
        .unwrap_or_default();
    let fresh_visible = ui
        .search_filtered_items(&fresh_items)
        .into_iter()
        .cloned()
        .collect::<Vec<_>>();
    if let Some(selection) = ui.current_page_mut().unified_playlist_selection_mut() {
        crate::state::synchronize_unified_playlist_entries(
            selection,
            playlist_id,
            fresh_items
                .iter()
                .map(|item| (item.media_id.clone(), item.entry_id)),
            fresh_visible
                .iter()
                .map(|item| (item.media_id.clone(), item.entry_id)),
        )
        .map_err(|error| anyhow::anyhow!("Unified playlist selection refresh failed: {error:?}"))?;
    }
    let cursor = super::page::unified_playlist_cursor_after_remove(
        &fresh_items,
        &fresh_visible,
        prior_cursor,
        first_removed_index,
    );
    if let Some((index, cursor_id)) = cursor {
        ui.current_page_mut().select(index);
        ui.current_page_mut()
            .set_unified_playlist_cursor_entry_id(Some(cursor_id));
    } else if let Some(playlist_state) = ui.current_page_mut().mutable_playlist_state_mut() {
        playlist_state.table_mut().select(None);
        playlist_state.set_cursor_occurrence(None);
    }
    Ok(())
}

#[cfg(test)]
mod native_playlist_mutation_tests {
    use super::{provider_operation_id, spotify_remove_all_intent};

    #[test]
    fn spotify_bulk_remove_all_keeps_one_snapshot_and_the_planner_identity() {
        let intent = spotify_remove_all_intent(
            "spotify:playlist:one".to_owned(),
            vec![
                "spotify:track:first".to_owned(),
                "spotify:track:first".to_owned(),
                "spotify:track:second".to_owned(),
            ],
            "base-snapshot".to_owned(),
            crate::command::BulkOperationId::from_index(7),
        );
        assert!(matches!(
            intent,
            crate::client::SpotifyMutationIntent::RemoveAllMedia {
                operation_id: crate::client::PlaylistMutationOperationId(7),
                media_uris,
                snapshot_id,
                ..
            } if media_uris.len() == 2 && snapshot_id == "base-snapshot"
        ));
    }

    #[test]
    fn provider_adapter_identity_is_the_exact_bulk_planner_operation_id() {
        assert_eq!(
            provider_operation_id(crate::command::BulkOperationId::from_index(11)),
            crate::client::PlaylistMutationOperationId(11)
        );
    }
}

pub(super) fn submit_listenbrainz_token(
    client_pub: &crate::client::ClientRequestSender,
    ui: &mut UIStateGuard,
    token: String,
    save: bool,
) {
    if ui.welcome_listenbrainz_pending.is_some() {
        return;
    }
    ui.welcome_listenbrainz_identity = None;
    ui.welcome_listenbrainz_attempt += 1;
    let attempt = ui.welcome_listenbrainz_attempt;
    ui.welcome_listenbrainz_pending = Some(attempt);
    ui.welcome_listenbrainz_notice = Some("Checking token…".to_owned());
    ui.welcome_listenbrainz_username = None;
    if client_pub
        .send(ClientRequest::ValidateListenBrainzToken {
            attempt,
            token: crate::client::listenbrainz::ListenBrainzToken::new(token),
            save,
        })
        .is_err()
    {
        ui.welcome_listenbrainz_pending = None;
        ui.welcome_listenbrainz_notice = Some("Could not start token check. Retry.".to_owned());
    }
}
pub(super) fn handle_listenbrainz_token_popup(
    keys: &KeySequence,
    client_pub: &crate::client::ClientRequestSender,
    ui: &mut UIStateGuard,
) -> bool {
    if matches!(
        config::get_config()
            .keymap_config
            .find_command_from_key_sequence(keys),
        Some(Command::ClosePopup)
    ) {
        ui.popup = None;
        ui.welcome_listenbrainz_pending = None;
        return true;
    }
    if keys.keys.len() != 1 {
        return true;
    }
    let key = &keys.keys[0];
    if matches!(key, Key::None(crossterm::event::KeyCode::Enter)) {
        let Some(PopupState::ListenBrainzToken { input }) = &ui.popup else {
            return false;
        };
        let token = input.get_text();
        if token.trim().is_empty() {
            ui.welcome_listenbrainz_notice = Some("Enter a user token or Cancel.".to_owned());
            return true;
        }
        ui.popup = None;
        submit_listenbrainz_token(client_pub, ui, token, true);
    } else if let Some(PopupState::ListenBrainzToken { input }) = &mut ui.popup {
        input.input(key);
    }
    true
}

#[cfg(test)]
mod listenbrainz_input_tests {
    use super::*;
    #[test]
    fn listenbrainz_busy_edit_cancel_and_retry_own_input() {
        crate::ui::initialize_test_config();
        let ring = std::sync::Arc::new(parking_lot::Mutex::new(std::collections::VecDeque::new()));
        let (diagnostics, _runtime) = crate::observability::disabled(ring);
        let state = std::sync::Arc::new(crate::state::State::new(false, diagnostics));
        let (sender, receiver) = crate::client::client_request_channel();
        let mut ui = state.ui.lock();
        let mut welcome = crate::state::WelcomePageUIState::new();
        welcome.show_step(crate::state::WelcomeStep::ListenBrainz);
        ui.history = vec![PageState::Welcome {
            state: welcome,
            from_settings: false,
        }];
        submit_listenbrainz_token(&sender, &mut ui, "one".to_owned(), true);
        let attempt = ui.welcome_listenbrainz_pending;
        submit_listenbrainz_token(&sender, &mut ui, "two".to_owned(), true);
        assert_eq!(ui.welcome_listenbrainz_pending, attempt);
        assert!(receiver.try_recv().is_ok());
        assert!(receiver.try_recv().is_err());
        super::super::page::handle_command_for_welcome_page(
            Command::ChooseSelected,
            &sender,
            &mut ui,
        )
        .unwrap();
        assert!(ui.welcome_listenbrainz_pending.is_none());
        assert!(matches!(
            ui.popup,
            Some(PopupState::ListenBrainzToken { .. })
        ));
        handle_listenbrainz_token_popup(
            &KeySequence {
                keys: vec![Key::None(crossterm::event::KeyCode::Char('x'))],
            },
            &sender,
            &mut ui,
        );
        handle_listenbrainz_token_popup(
            &KeySequence {
                keys: vec![Key::None(crossterm::event::KeyCode::Esc)],
            },
            &sender,
            &mut ui,
        );
        assert!(ui.popup.is_none());
        submit_listenbrainz_token(&sender, &mut ui, "three".to_owned(), true);
        assert!(ui.welcome_listenbrainz_pending.unwrap() > attempt.unwrap());
        assert!(receiver.try_recv().is_ok());
        ui.current_page_mut()
            .select(crate::state::WelcomeStep::ListenBrainz.row_count() - 1);
        super::super::page::handle_command_for_welcome_page(
            Command::ChooseSelected,
            &sender,
            &mut ui,
        )
        .unwrap();
        assert!(ui.welcome_listenbrainz_pending.is_none());
        assert!(
            matches!(ui.current_page(), PageState::Welcome { state, .. } if state.step == crate::state::WelcomeStep::Review)
        );
    }
}

pub(super) fn start_listenbrainz_playlist_list(
    client_pub: &crate::client::ClientRequestSender,
    ui: &mut UIStateGuard,
) {
    let Some(identity) = ui.welcome_listenbrainz_identity.clone().filter(|identity| {
        config::get_config().listenbrainz_token().as_deref() == Some(identity.token.expose())
    }) else {
        ui.welcome_listenbrainz_identity = None;
        ui.welcome_listenbrainz_username = None;
        ui.welcome_listenbrainz_notice =
            Some("Validate the current token before fetching playlists.".to_owned());
        ui.popup = None;
        return;
    };
    ui.welcome_listenbrainz_attempt += 1;
    let operation = ui.welcome_listenbrainz_attempt;
    ui.popup = Some(PopupState::ListenBrainzPlaylists {
        operation,
        identity: identity.clone(),
        rows: Vec::new(),
        state: ListState::default().with_selected(Some(0)),
        busy: true,
        notice: "Fetching playlists…".to_owned(),
    });
    if client_pub
        .send(ClientRequest::ListListenBrainzPlaylists {
            operation,
            identity,
        })
        .is_err()
    {
        if let Some(PopupState::ListenBrainzPlaylists { busy, notice, .. }) = &mut ui.popup {
            *busy = false;
            "Could not start playlist fetch. Choose Refresh to retry.".clone_into(notice);
        }
    }
}
pub(super) fn handle_listenbrainz_playlist_command(
    command: Command,
    client_pub: &crate::client::ClientRequestSender,
    ui: &mut UIStateGuard,
) -> Result<bool> {
    let Some(PopupState::ListenBrainzPlaylists { rows, .. }) = &ui.popup else {
        return Ok(false);
    };
    let count = rows.len();
    let result = handle_command_for_list_popup(
        command,
        ui,
        count + 2,
        |_, _| {},
        |ui, selected| {
            if selected == count {
                start_listenbrainz_playlist_list(client_pub, ui);
                return Ok(());
            }
            if selected == count + 1 {
                ui.popup = None;
                return Ok(());
            }
            let Some(PopupState::ListenBrainzPlaylists {
                identity,
                rows,
                busy,
                ..
            }) = &ui.popup
            else {
                return Ok(());
            };
            if *busy {
                return Ok(());
            }
            let identity = identity.clone();
            let playlist_id = rows[selected].id.clone();
            if config::get_config().listenbrainz_token().as_deref() != Some(identity.token.expose())
                || ui.welcome_listenbrainz_identity.as_ref() != Some(&identity)
            {
                ui.popup = None;
                ui.welcome_listenbrainz_identity = None;
                ui.welcome_listenbrainz_notice =
                    Some("Token changed. Validate it again.".to_owned());
                return Ok(());
            }
            ui.welcome_listenbrainz_attempt += 1;
            let next = ui.welcome_listenbrainz_attempt;
            if let Some(PopupState::ListenBrainzPlaylists {
                operation,
                busy,
                notice,
                ..
            }) = &mut ui.popup
            {
                *operation = next;
                *busy = true;
                "Importing playlist and resolving track metadata…".clone_into(notice);
            }
            if client_pub
                .send(ClientRequest::ImportListenBrainzPlaylist {
                    operation: next,
                    identity,
                    playlist_id,
                })
                .is_err()
            {
                if let Some(PopupState::ListenBrainzPlaylists { busy, notice, .. }) = &mut ui.popup
                {
                    *busy = false;
                    "Could not start import. Retry.".clone_into(notice);
                }
            }
            Ok(())
        },
        |ui| {
            ui.popup = None;
        },
    );
    // Rows may have moved; wait for the next frame's geometry.
    ui.workspace_popup_hits.clear();
    result
}
