use std::collections::HashSet;

use anyhow::{Context as _, Result};
use rand::RngExt;
use rspotify::{
    model::{ItemPositions, LibraryId},
    prelude::*,
};
use ytmapi_rs::common::YoutubeID;

use crate::{
    config,
    state::{
        Context, Item, ItemId, PlayableId, PlayableMedia, PlayerState, Playlist,
        PlaylistFolderItem, PlaylistId, SharedState, Track, TrackId, UserId, USER_LIKED_TRACKS_URI,
    },
};

use super::state_application::StateApplicationService;
use super::{
    youtube, AppClient, ClientRequest, MutationFailureCategory, PlaylistApplicationService,
    PlaylistMutationEffect, PlaylistMutationResult, ProviderMutationDelivery, Retryability,
    SpotifyMutationIntent, SpotifyPlaylistAdapter, YouTubeAppendReadBack, YouTubeMutationIntent,
    YouTubePlaylistAdapter,
};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum UserQueueRoute {
    LocalUnified(config::ActiveProvider),
    NativeSpotify,
}

fn user_queue_route(
    active_provider: Option<config::ActiveProvider>,
    spotify_targets_integrated_device: bool,
) -> UserQueueRoute {
    match active_provider {
        Some(config::ActiveProvider::YouTubeMusic) => {
            UserQueueRoute::LocalUnified(config::ActiveProvider::YouTubeMusic)
        }
        Some(config::ActiveProvider::Spotify) if spotify_targets_integrated_device => {
            UserQueueRoute::LocalUnified(config::ActiveProvider::Spotify)
        }
        Some(config::ActiveProvider::Spotify) | None => UserQueueRoute::NativeSpotify,
    }
}

fn queue_anchor(player: &PlayerState, provider: config::ActiveProvider) -> Option<PlayableMedia> {
    match provider {
        config::ActiveProvider::Spotify => match player.currently_playing()? {
            rspotify::model::PlayableItem::Track(track) => track
                .id
                .as_ref()
                .map(|id| PlayableMedia::Spotify(PlayableId::Track(id.clone().into_static()))),
            rspotify::model::PlayableItem::Episode(episode) => Some(PlayableMedia::Spotify(
                PlayableId::Episode(episode.id.clone().into_static()),
            )),
            rspotify::model::PlayableItem::Unknown(_) => None,
        },
        config::ActiveProvider::YouTubeMusic => player
            .youtube_playback
            .as_ref()
            .map(|playback| PlayableMedia::YouTube(playback.track.clone())),
    }
}

impl AppClient {
    async fn user_queue_route(&self, state: &SharedState) -> UserQueueRoute {
        let active_provider = self.playback.stable_active_provider();
        let spotify_targets_integrated_device =
            if active_provider == Some(config::ActiveProvider::Spotify) {
                super::playback_coordinator::SpotifyEngineAdapter::new(self, state)
                    .targets_integrated_device()
                    .await
            } else {
                false
            };
        let route = user_queue_route(active_provider, spotify_targets_integrated_device);
        crate::observability::operation_stage_detail(
            crate::observability::Component::Coordinator,
            "user_queue_route",
            None,
            Some(crate::observability::OperationOutcome::Success),
            None,
            Some(match route {
                UserQueueRoute::LocalUnified(_) => "local_unified",
                UserQueueRoute::NativeSpotify => "native_spotify",
            }),
            None,
            None,
        );
        route
    }

    fn enqueue_local_user_items<I>(
        &self,
        state: &SharedState,
        anchor_provider: Option<config::ActiveProvider>,
        items: I,
    ) where
        I: IntoIterator<Item = PlayableMedia>,
    {
        let current = {
            let mut player = state.player.write();
            let anchor = anchor_provider.and_then(|provider| queue_anchor(&player, provider));
            player.enqueue_unified_user_items(anchor, items);
            player
                .unified_queue
                .as_ref()
                .and_then(|queue| queue.current().cloned())
        };
        self.refresh_and_persist_session(
            state,
            anchor_provider.unwrap_or(config::ActiveProvider::YouTubeMusic),
        );
        match current {
            Some(PlayableMedia::Spotify(playable_id)) => {
                self.refresh_spotify_queue_completion(state, &playable_id);
            }
            Some(PlayableMedia::YouTube(_)) => self.refresh_youtube_queue_completion(state),
            None => {}
        }
        self.schedule_youtube_prefetch(state);
    }

    pub async fn add_item_to_playlist(
        &self,
        state: &SharedState,
        playlist_id: PlaylistId<'_>,
        playable_id: PlayableId<'_>,
    ) -> Result<()> {
        // remove all the occurrences of the track to ensure no duplication in the playlist
        self.spotify_api()
            .playlist_remove_all_occurrences_of_items(
                playlist_id.as_ref(),
                [playable_id.as_ref()],
                None,
            )
            .await?;

        self.spotify_api()
            .playlist_add_items(playlist_id.as_ref(), [playable_id.as_ref()], None)
            .await?;

        // After adding a new track to a playlist, remove the cache of that playlist to force refetching new data
        state.data.write().caches.context.remove(&playlist_id.uri());

        Ok(())
    }

    /// Add a Spotify item to current user's library.
    pub(super) async fn add_to_library(&self, state: &SharedState, item: Item) -> Result<()> {
        // Before adding new item, checks if that item already exists in the library to avoid adding a duplicated item.
        match item {
            Item::Track(track) => {
                let contains = self
                    .spotify_api()
                    .library_contains([LibraryId::Track(track.id.as_ref())])
                    .await?;
                if !contains[0] {
                    self.spotify_api()
                        .library_add([LibraryId::Track(track.id.as_ref())])
                        .await?;
                    // update the in-memory `user_data`
                    state
                        .data
                        .write()
                        .user_data
                        .saved_tracks
                        .insert(track.id.uri(), track);
                    state
                        .data
                        .write()
                        .caches
                        .context
                        .remove(USER_LIKED_TRACKS_URI);
                }
            }
            Item::Album(album) => {
                let contains = self
                    .spotify_api()
                    .library_contains([LibraryId::Album(album.id.as_ref())])
                    .await?;
                if !contains[0] {
                    self.spotify_api()
                        .library_add([LibraryId::Album(album.id.as_ref())])
                        .await?;
                    // update the in-memory `user_data`
                    state.data.write().user_data.saved_albums.insert(0, album);
                }
            }
            Item::Artist(artist) => {
                let follows = self
                    .spotify_api()
                    .library_contains([LibraryId::Artist(artist.id.as_ref())])
                    .await?;
                if !follows[0] {
                    self.spotify_api()
                        .library_add([LibraryId::Artist(artist.id.as_ref())])
                        .await?;
                    // update the in-memory `user_data`
                    state
                        .data
                        .write()
                        .user_data
                        .followed_artists
                        .insert(0, artist);
                }
            }
            Item::Playlist(playlist) => {
                let follows = self
                    .spotify_api()
                    .library_contains([LibraryId::Playlist(playlist.id.as_ref())])
                    .await?;
                if !follows[0] {
                    self.spotify_api()
                        .library_add([LibraryId::Playlist(playlist.id.as_ref())])
                        .await?;
                    // update the in-memory `user_data`
                    state
                        .data
                        .write()
                        .user_data
                        .playlists
                        .insert(0, PlaylistFolderItem::Playlist(playlist));
                }
            }
            Item::Show(show) => {
                let follows = self
                    .spotify_api()
                    .library_contains([LibraryId::Show(show.id.as_ref())])
                    .await?;
                if !follows[0] {
                    self.spotify_api()
                        .library_add([LibraryId::Show(show.id.as_ref())])
                        .await?;
                    // update the in-memory `user_data`
                    state.data.write().user_data.saved_shows.insert(0, show);
                }
            }
        }
        Ok(())
    }

    /// Add Spotify tracks to current user's library.
    pub(super) async fn add_tracks_to_library(
        &self,
        state: &SharedState,
        tracks: Vec<Track>,
    ) -> Result<()> {
        for tracks_chunk in tracks.chunks(50) {
            self.spotify_api()
                .library_add(
                    tracks_chunk
                        .iter()
                        .map(|track| LibraryId::Track(track.id.as_ref())),
                )
                .await?;
        }

        let mut data = state.data.write();
        for track in tracks {
            data.user_data.saved_tracks.insert(track.id.uri(), track);
        }
        data.caches.context.remove(USER_LIKED_TRACKS_URI);
        Ok(())
    }

    /// Delete Spotify tracks from current user's library.
    pub(super) async fn delete_tracks_from_library(
        &self,
        state: &SharedState,
        track_ids: Vec<TrackId<'static>>,
    ) -> Result<()> {
        for track_ids_chunk in track_ids.chunks(50) {
            self.spotify_api()
                .library_remove(
                    track_ids_chunk
                        .iter()
                        .map(|track_id| LibraryId::Track(track_id.as_ref())),
                )
                .await?;
        }

        let removed_track_uris = track_ids
            .iter()
            .map(rspotify::prelude::Id::uri)
            .collect::<HashSet<_>>();

        let mut data = state.data.write();
        for track_id in track_ids {
            data.user_data.saved_tracks.remove(&track_id.uri());
        }
        if let Some(Context::Tracks { tracks, .. }) =
            data.caches.context.get_mut(USER_LIKED_TRACKS_URI)
        {
            tracks.retain(|track| !removed_track_uris.contains(&track.id.uri()));
        }
        Ok(())
    }

    // Delete a Spotify item from user's library
    pub(super) async fn delete_from_library(&self, state: &SharedState, id: ItemId) -> Result<()> {
        match id {
            ItemId::Track(id) => {
                let uri = id.uri();
                self.spotify_api()
                    .library_remove([LibraryId::Track(id)])
                    .await?;
                let mut data = state.data.write();
                data.user_data.saved_tracks.remove(&uri);
                if let Some(Context::Tracks { tracks, .. }) =
                    data.caches.context.get_mut(USER_LIKED_TRACKS_URI)
                {
                    tracks.retain(|track| track.id.uri() != uri);
                }
            }
            ItemId::Album(id) => {
                state
                    .data
                    .write()
                    .user_data
                    .saved_albums
                    .retain(|a| a.id != id);
                self.spotify_api()
                    .library_remove([LibraryId::Album(id)])
                    .await?;
            }
            ItemId::Artist(id) => {
                state
                    .data
                    .write()
                    .user_data
                    .followed_artists
                    .retain(|a| a.id != id);
                self.spotify_api()
                    .library_remove([LibraryId::Artist(id)])
                    .await?;
            }
            ItemId::Playlist(id) => {
                state
                    .data
                    .write()
                    .user_data
                    .playlists
                    .retain(|item| match item {
                        PlaylistFolderItem::Playlist(p) => p.id != id,
                        PlaylistFolderItem::Folder(_) => true,
                    });
                self.spotify_api()
                    .library_remove([LibraryId::Playlist(id)])
                    .await?;
            }
            ItemId::Show(id) => {
                state
                    .data
                    .write()
                    .user_data
                    .saved_shows
                    .retain(|s| s.id != id);
                self.spotify_api()
                    .library_remove([LibraryId::Show(id)])
                    .await?;
            }
        }
        Ok(())
    }
}

impl AppClient {
    /// Create a new playlist
    pub(super) async fn create_new_playlist(
        &self,
        state: &SharedState,
        user_id: UserId<'static>,
        playlist_name: &str,
        public: bool,
        collab: bool,
        desc: &str,
    ) -> Result<PlaylistId<'static>> {
        let playlist: Playlist = self
            .spotify_api()
            .user_playlist_create(
                user_id,
                playlist_name,
                Some(public),
                Some(collab),
                Some(desc),
            )
            .await?
            .into();
        tracing::info!("A new Spotify playlist was successfully created");
        let playlist_id = playlist.id.clone_static();
        state
            .data
            .write()
            .user_data
            .playlists
            .insert(0, PlaylistFolderItem::Playlist(playlist));
        Ok(playlist_id)
    }

    async fn rename_spotify_playlist(
        &self,
        state: &SharedState,
        playlist_id: PlaylistId<'static>,
        name: &str,
    ) -> Result<()> {
        self.spotify_api()
            .playlist_change_detail(playlist_id.as_ref(), Some(name), None, None, None)
            .await?;
        let uri = playlist_id.uri();
        let mut data = state.data.write();
        for item in &mut data.user_data.playlists {
            if let PlaylistFolderItem::Playlist(playlist) = item {
                if playlist.id == playlist_id {
                    name.clone_into(&mut playlist.name);
                }
            }
        }
        if let Some(Context::Playlist { playlist, .. }) = data.caches.context.get_mut(&uri) {
            name.clone_into(&mut playlist.name);
        }
        Ok(())
    }
}

fn spotify_playlist_owner_id(
    user: Option<&rspotify::model::PrivateUser>,
) -> Result<UserId<'static>> {
    user.map(|user| user.id.clone())
        .context("Spotify profile is unavailable; refresh Spotify data before creating a playlist")
}

fn invalid_spotify_mutation() -> PlaylistMutationResult {
    PlaylistMutationResult::Invalidated {
        reason: "Spotify playlist mutation contains an invalid provider identifier or position.",
    }
}

fn classify_spotify_status(
    operation_id: super::PlaylistMutationOperationId,
    status: u16,
) -> PlaylistMutationResult {
    match status {
        400 | 404 | 422 => PlaylistMutationResult::Invalidated {
            reason: "Spotify rejected a stale or invalid playlist mutation.",
        },
        401 => PlaylistMutationResult::Failed {
            category: MutationFailureCategory::Authentication,
            retryability: Retryability::AfterRefresh,
        },
        403 => PlaylistMutationResult::Failed {
            category: MutationFailureCategory::InvalidRequest,
            retryability: Retryability::Never,
        },
        409 => PlaylistMutationResult::Conflict {
            current_revision: None,
        },
        // Includes 429: a rate-limited response does not prove the write was skipped.
        _ => PlaylistMutationResult::OutcomeUnknown { operation_id },
    }
}

fn classify_spotify_error(
    intent: &SpotifyMutationIntent,
    error: rspotify::ClientError,
) -> PlaylistMutationResult {
    use rspotify::{http::HttpError, ClientError};

    match error {
        ClientError::Http(error) => match *error {
            HttpError::StatusCode(response) => {
                classify_spotify_status(intent.operation_id(), response.status().as_u16())
            }
            HttpError::Client(error) if error.is_connect() => PlaylistMutationResult::Failed {
                category: MutationFailureCategory::Transport,
                retryability: Retryability::AfterRefresh,
            },
            HttpError::Client(error) if error.is_builder() || error.is_redirect() => {
                PlaylistMutationResult::Invalidated {
                    reason: "Spotify playlist mutation could not be constructed safely.",
                }
            }
            HttpError::Client(_) => PlaylistMutationResult::OutcomeUnknown {
                operation_id: intent.operation_id(),
            },
        },
        ClientError::InvalidToken => PlaylistMutationResult::Failed {
            category: MutationFailureCategory::Authentication,
            retryability: Retryability::AfterRefresh,
        },
        ClientError::ParseUrl(_) | ClientError::Model(_) => PlaylistMutationResult::Invalidated {
            reason: "Spotify playlist mutation contains an invalid provider identifier.",
        },
        ClientError::ParseJson(_) | ClientError::Io(_) => PlaylistMutationResult::OutcomeUnknown {
            operation_id: intent.operation_id(),
        },
        _ => PlaylistMutationResult::Failed {
            category: MutationFailureCategory::InvalidRequest,
            retryability: Retryability::Never,
        },
    }
}

impl AppClient {
    async fn execute_spotify_playlist_mutation(
        &self,
        intent: &SpotifyMutationIntent,
    ) -> PlaylistMutationResult {
        if let Err(result) = SpotifyPlaylistAdapter::validate(intent) {
            return result;
        }
        let delivery = match intent {
            SpotifyMutationIntent::RemoveOccurrence {
                playlist_id,
                media_uri,
                occurrence,
                ..
            } => {
                let Ok(playlist_id) =
                    PlaylistId::from_uri(playlist_id).or_else(|_| PlaylistId::from_id(playlist_id))
                else {
                    return invalid_spotify_mutation();
                };
                let Ok(track_id) = TrackId::from_uri(media_uri) else {
                    return invalid_spotify_mutation();
                };
                let media_id = PlayableId::Track(track_id);
                let Ok(position) = u32::try_from(occurrence.position) else {
                    return invalid_spotify_mutation();
                };
                let positions = [position];
                let item = ItemPositions {
                    id: media_id,
                    positions: &positions,
                };
                match self
                    .spotify_api()
                    .playlist_remove_specific_occurrences_of_items(
                        playlist_id,
                        [item],
                        Some(occurrence.snapshot_id.as_str()),
                    )
                    .await
                {
                    Ok(result) => ProviderMutationDelivery::Applied {
                        revision: Some(result.snapshot_id),
                        occurrence_token: None,
                    },
                    Err(error) => return classify_spotify_error(intent, error),
                }
            }
            SpotifyMutationIntent::RemoveAllMedia {
                playlist_id,
                media_uris,
                snapshot_id,
                ..
            } => {
                let Ok(playlist_id) =
                    PlaylistId::from_uri(playlist_id).or_else(|_| PlaylistId::from_id(playlist_id))
                else {
                    return invalid_spotify_mutation();
                };
                let media_ids = media_uris
                    .iter()
                    .map(|media_uri| {
                        TrackId::from_uri(media_uri)
                            .map(PlayableId::Track)
                            .map_err(|_| ())
                    })
                    .collect::<Result<Vec<_>, _>>();
                let Ok(media_ids) = media_ids else {
                    return invalid_spotify_mutation();
                };
                match self
                    .spotify_api()
                    .playlist_remove_all_occurrences_of_items(
                        playlist_id,
                        media_ids,
                        Some(snapshot_id.as_str()),
                    )
                    .await
                {
                    Ok(result) => ProviderMutationDelivery::Applied {
                        revision: Some(result.snapshot_id),
                        occurrence_token: None,
                    },
                    Err(error) => return classify_spotify_error(intent, error),
                }
            }
            SpotifyMutationIntent::Reorder {
                playlist_id,
                range_start,
                insert_before,
                range_length,
                snapshot_id,
                ..
            } => {
                let Ok(playlist_id) =
                    PlaylistId::from_uri(playlist_id).or_else(|_| PlaylistId::from_id(playlist_id))
                else {
                    return invalid_spotify_mutation();
                };
                let (Ok(range_start), Ok(insert_before), Ok(range_length)) = (
                    i32::try_from(*range_start),
                    i32::try_from(*insert_before),
                    u32::try_from(*range_length),
                ) else {
                    return invalid_spotify_mutation();
                };
                match self
                    .spotify_api()
                    .playlist_reorder_items(
                        playlist_id,
                        Some(range_start),
                        Some(insert_before),
                        Some(range_length),
                        Some(snapshot_id.as_str()),
                    )
                    .await
                {
                    Ok(result) => ProviderMutationDelivery::Applied {
                        revision: Some(result.snapshot_id),
                        occurrence_token: None,
                    },
                    Err(error) => return classify_spotify_error(intent, error),
                }
            }
        };
        SpotifyPlaylistAdapter::complete(intent, delivery)
    }

    async fn execute_youtube_playlist_mutation(
        &self,
        intent: &YouTubeMutationIntent,
    ) -> PlaylistMutationResult {
        if let Err(result) = YouTubePlaylistAdapter::validate(intent) {
            return result;
        }
        let Ok(youtube) = youtube::YouTubeMusic::new(config::get_config()).await else {
            return PlaylistMutationResult::Failed {
                category: MutationFailureCategory::Authentication,
                retryability: Retryability::AfterRefresh,
            };
        };
        execute_youtube_playlist_mutation_with(&youtube, intent).await
    }
}

async fn execute_youtube_playlist_mutation_with(
    youtube: &youtube::YouTubeMusic,
    intent: &YouTubeMutationIntent,
) -> PlaylistMutationResult {
    if let Err(result) = YouTubePlaylistAdapter::validate(intent) {
        return result;
    }
    let mut read_back = None;
    let delivery = match intent {
        YouTubeMutationIntent::RemoveOccurrence {
            playlist_id,
            set_video_id: Some(set_video_id),
            ..
        } => match youtube
            .remove_video_from_playlist(playlist_id, set_video_id)
            .await
        {
            Ok(()) => ProviderMutationDelivery::Applied {
                revision: None,
                occurrence_token: None,
            },
            Err(_) => ProviderMutationDelivery::Ambiguous,
        },
        YouTubeMutationIntent::Append {
            playlist_id,
            video_id,
            ..
        } => {
            if let Ok(occurrence_token) = youtube
                .add_video_to_playlist_with_token(playlist_id, video_id)
                .await
            {
                ProviderMutationDelivery::Applied {
                    revision: None,
                    occurrence_token,
                }
            } else {
                read_back = youtube
                    .context(&crate::state::YouTubeContextId::Playlist(
                        playlist_id.clone(),
                    ))
                    .await
                    .ok()
                    .map(|context| YouTubeAppendReadBack {
                        total: context.tracks.len(),
                        media_occurrences: context
                            .tracks
                            .iter()
                            .filter(|track| track.id == *video_id)
                            .count(),
                        last_media_position: context
                            .tracks
                            .iter()
                            .rposition(|track| track.id == *video_id),
                    });
                ProviderMutationDelivery::Ambiguous
            }
        }
        YouTubeMutationIntent::Reorder { .. }
        | YouTubeMutationIntent::RemoveOccurrence {
            set_video_id: None, ..
        } => ProviderMutationDelivery::Failed {
            category: MutationFailureCategory::InvalidRequest,
            retryability: Retryability::Never,
        },
    };
    YouTubePlaylistAdapter::complete(intent, delivery, read_back)
}

fn apply_native_playlist_effect(
    state: &SharedState,
    result: PlaylistMutationResult,
) -> Result<super::state_application::PlaylistEffectApplication> {
    let effect = playlist_mutation_effect(result)?;
    Ok(StateApplicationService.apply_playlist_mutation_effect(state, effect))
}

async fn apply_playlist_refresh_effect(
    client: &AppClient,
    state: &SharedState,
    effect: &super::state_application::PlaylistRefreshEffect,
) {
    if let super::state_application::PlaylistRefreshEffect::YouTubePlaylist { playlist_id } = effect
    {
        refresh_youtube_library_after_mutation(client, state).await;
        refresh_visible_youtube_playlist_after_mutation(state, playlist_id).await;
    }
}

fn playlist_mutation_effect(result: PlaylistMutationResult) -> Result<PlaylistMutationEffect> {
    match result {
        PlaylistMutationResult::Applied { receipt } => Ok(receipt.effect),
        PlaylistMutationResult::Conflict { current_revision } => {
            Err(anyhow::anyhow!(if current_revision
                .as_deref()
                .is_some_and(|revision| !revision.is_empty())
            {
                "playlist mutation conflicted with a newer provider revision"
            } else {
                "playlist mutation conflicted; refresh before retrying"
            }))
        }
        PlaylistMutationResult::Invalidated { reason }
        | PlaylistMutationResult::Unsupported { reason } => Err(anyhow::anyhow!(reason)),
        PlaylistMutationResult::Failed {
            category,
            retryability,
        } => Err(anyhow::anyhow!(match (category, retryability) {
            (MutationFailureCategory::Authentication, Retryability::AfterRefresh) =>
                "playlist mutation requires refreshed provider authentication",
            (MutationFailureCategory::InvalidRequest, Retryability::Never) =>
                "playlist mutation was rejected as invalid",
            (MutationFailureCategory::Transport, Retryability::AfterRefresh) =>
                "playlist mutation was not sent; refresh before retrying",
            (MutationFailureCategory::Transport, Retryability::ManualAfterVerification) =>
                "playlist mutation requires provider verification before retrying",
            _ => "playlist mutation failed before it could be verified",
        })),
        PlaylistMutationResult::OutcomeUnknown { .. } => Err(anyhow::anyhow!(
            "playlist mutation outcome is unknown; refresh before retrying"
        )),
    }
}

fn retain_youtube_append_token_on_visible_page(
    state: &SharedState,
    playlist_id: &str,
    video_id: &str,
    previous_media_occurrences: usize,
    set_video_id: String,
) {
    let mut ui = state.ui.lock();
    let crate::state::PageState::YouTubeContext {
        id: crate::state::YouTubeContextId::Playlist(current_id),
        context: Some(context),
        ..
    } = ui.current_page_mut()
    else {
        return;
    };
    if !crate::state::youtube_playlist_ids_match(current_id, playlist_id) {
        return;
    }
    retain_youtube_append_token(context, video_id, previous_media_occurrences, set_video_id);
}

fn retain_youtube_append_token(
    context: &mut crate::state::YouTubeContext,
    video_id: &str,
    previous_media_occurrences: usize,
    set_video_id: String,
) -> bool {
    let Some(position) = context
        .tracks
        .iter()
        .enumerate()
        .filter(|(_, track)| track.id == video_id)
        .nth(previous_media_occurrences)
        .map(|(position, _)| position)
    else {
        return false;
    };
    context
        .playlist_set_video_ids
        .resize(context.tracks.len(), None);
    context.playlist_set_video_ids[position] = Some(set_video_id);
    true
}

fn remove_visible_youtube_occurrence_token(
    state: &SharedState,
    playlist_id: &str,
    set_video_id: &str,
) {
    let mut ui = state.ui.lock();
    let crate::state::PageState::YouTubeContext {
        id: crate::state::YouTubeContextId::Playlist(current_id),
        context: Some(context),
        ..
    } = ui.current_page_mut()
    else {
        return;
    };
    if !crate::state::youtube_playlist_ids_match(current_id, playlist_id) {
        return;
    }
    let Some(position) = context
        .playlist_set_video_ids
        .iter()
        .position(|token| token.as_deref() == Some(set_video_id))
    else {
        return;
    };
    if position >= context.tracks.len() {
        return;
    }
    context.tracks.remove(position);
    context.playlist_set_video_ids.remove(position);
}

impl AppClient {
    pub(super) async fn handle_playlist_mutation_request(
        &self,
        state: &SharedState,
        request: ClientRequest,
    ) -> Result<()> {
        match request {
            ClientRequest::Playlist(request) => {
                // Validate the already-planned request at the application
                // boundary, then hand only the adapter-specific execution
                // payload to the existing provider handlers.
                let plan = PlaylistApplicationService::plan(request.clone())?;
                let projection_sync = match &request.operation {
                    crate::client::PlaylistRequestKind::SyncUnifiedPlaylistToYouTube {
                        unified_playlist_id,
                    } => Some(unified_playlist_id.clone()),
                    _ => None,
                };
                let projection_intent_key = projection_sync
                    .as_deref()
                    .map(|playlist_id| projection_intent_key(state, playlist_id))
                    .transpose()?;
                if let Some(playlist_id) = projection_sync.as_deref() {
                    ensure_projection_retry_allowed(
                        state,
                        playlist_id,
                        plan.operation_id.0,
                        projection_intent_key.as_deref().unwrap_or_default(),
                    )?;
                }
                let result =
                    Box::pin(self.handle_playlist_mutation_request(state, request.into_legacy()))
                        .await;
                if result.is_ok() {
                    if let Some(playlist_id) = projection_sync.as_deref() {
                        acknowledge_projection_operation(
                            state,
                            playlist_id,
                            plan.operation_id.0,
                            projection_intent_key.as_deref().unwrap_or_default(),
                        )?;
                    }
                }
                let _published = PlaylistApplicationService::publish(plan.operation_id, &result);
                result?;
                debug_assert_eq!(plan.operation_id, plan.request.operation_id);
            }
            ClientRequest::SpotifyPlaylistMutation(intent) => {
                let result = self.execute_spotify_playlist_mutation(&intent).await;
                let effect = apply_native_playlist_effect(state, result)?;
                apply_playlist_refresh_effect(self, state, &effect.refresh).await;
            }
            ClientRequest::YouTubePlaylistMutation(intent) => {
                let result = self.execute_youtube_playlist_mutation(&intent).await;
                let applied = matches!(result, PlaylistMutationResult::Applied { .. });
                let effect = apply_native_playlist_effect(state, result)?;
                if applied {
                    if let YouTubeMutationIntent::RemoveOccurrence {
                        playlist_id,
                        set_video_id: Some(set_video_id),
                        ..
                    } = &intent
                    {
                        remove_visible_youtube_occurrence_token(state, playlist_id, set_video_id);
                    }
                }
                apply_playlist_refresh_effect(self, state, &effect.refresh).await;
            }
            ClientRequest::RateYouTubeTrack { track, liked } => {
                let youtube = youtube::YouTubeMusic::new(config::get_config()).await?;
                youtube.rate_song(&track.id, liked).await?;
                if !liked && youtube_liked_context_is_visible(state) {
                    match youtube
                        .context(&crate::state::YouTubeContextId::LikedTracks)
                        .await
                    {
                        Ok(context) => {
                            let mut ui = state.ui.lock();
                            if let crate::state::PageState::YouTubeContext {
                                id,
                                context: page_context,
                                state: page_state,
                                ..
                            } = ui.current_page_mut()
                            {
                                if *id == crate::state::YouTubeContextId::LikedTracks {
                                    page_state.status = if context.tracks.is_empty() {
                                        crate::state::UiViewStatus::Empty
                                    } else {
                                        crate::state::UiViewStatus::Ready
                                    };
                                    *page_context = Some(context);
                                }
                            }
                        }
                        Err(error) => {
                            crate::observability::log_safe_error!(
                                warn,
                                crate::observability::DiagnosticCode::YOUTUBE_CONTEXT_LOAD_FAILED,
                                crate::observability::ErrorCategory::Unavailable,
                                &error,
                                "YouTube liked state changed, but the visible context could not be refreshed"
                            );
                            let mut ui = state.ui.lock();
                            if let crate::state::PageState::YouTubeContext {
                                id,
                                state: page_state,
                                ..
                            } = ui.current_page_mut()
                            {
                                if *id == crate::state::YouTubeContextId::LikedTracks {
                                    page_state.status = crate::state::UiViewStatus::Partial {
                                        code: crate::state::YOUTUBE_CONTEXT_REFRESH_PARTIAL_CODE,
                                        message: crate::state::YOUTUBE_CONTEXT_REFRESH_PARTIAL_MESSAGE,
                                        next_action:
                                            crate::state::YOUTUBE_CONTEXT_REFRESH_PARTIAL_NEXT_ACTION,
                                    };
                                }
                            }
                        }
                    }
                }
            }
            ClientRequest::SubscribeYouTubeArtist { channel_id } => {
                let youtube = youtube::YouTubeMusic::new(config::get_config()).await?;
                youtube
                    .subscribe_artist(ytmapi_rs::common::ArtistChannelID::from_raw(channel_id))
                    .await?;
            }
            ClientRequest::UnsubscribeYouTubeArtist { channel_id } => {
                let youtube = youtube::YouTubeMusic::new(config::get_config()).await?;
                youtube
                    .unsubscribe_artists([ytmapi_rs::common::ArtistChannelID::from_raw(channel_id)])
                    .await?;
            }
            ClientRequest::AddItemsToUserQueue(items) => {
                if items.is_empty() {
                    return Ok(());
                }
                let anchor_provider = match self.user_queue_route(state).await {
                    UserQueueRoute::LocalUnified(provider) => Some(provider),
                    UserQueueRoute::NativeSpotify => None,
                };
                self.enqueue_local_user_items(state, anchor_provider, items);
            }
            ClientRequest::AddYouTubeTrackToPlaylist {
                operation_id,
                playlist_id,
                track,
            } => {
                let baseline = youtube_append_baseline(&playlist_id, &track.id).await;
                let intent = YouTubeMutationIntent::Append {
                    operation_id,
                    playlist_id: playlist_id.clone(),
                    video_id: track.id.clone(),
                    baseline,
                };
                let result = self.execute_youtube_playlist_mutation(&intent).await;
                let effect = apply_native_playlist_effect(state, result)?;
                let appended_token = effect.appended_occurrence_token;
                apply_playlist_refresh_effect(self, state, &effect.refresh).await;
                if let (Some(set_video_id), Some(baseline)) = (appended_token, baseline) {
                    retain_youtube_append_token_on_visible_page(
                        state,
                        &playlist_id,
                        &track.id,
                        baseline.media_occurrences,
                        set_video_id,
                    );
                }
            }
            ClientRequest::AddItemsToUnifiedPlaylist {
                playlist_id,
                items,
                operation,
            } => {
                if let Some(operation) = operation.as_ref() {
                    validate_unified_playlist_operation(
                        state,
                        operation,
                        &playlist_id,
                        false,
                        &items,
                    )?;
                }
                let linked_youtube_playlist = state
                    .data
                    .read()
                    .playlist_links
                    .iter()
                    .find(|link| link.unified_playlist_id == playlist_id)
                    .and_then(|link| link.youtube_playlist_id.clone());
                append_local_unified_items(state, &playlist_id, items.clone())?;
                if let Some(target_playlist_id) = linked_youtube_playlist {
                    let (library, remote_snapshot, remote_context) =
                        append_unified_items_to_youtube(&target_playlist_id, &items).await?;
                    let remote_items = remote_context
                        .as_ref()
                        .map(|context| context.tracks.clone());
                    state.data.write().user_data.youtube_library = library;
                    if let Some(context) = remote_context {
                        apply_youtube_context_to_visible_page(
                            state,
                            &crate::state::YouTubeContextId::Playlist(target_playlist_id),
                            context,
                        );
                    }
                    record_linked_projection_snapshots(
                        state,
                        &playlist_id,
                        remote_snapshot,
                        remote_items.as_deref(),
                        &[],
                        None,
                    )?;
                }
            }
            ClientRequest::AddPlayableToQueue(playable_id) => {
                match self.user_queue_route(state).await {
                    UserQueueRoute::LocalUnified(provider) => {
                        self.enqueue_local_user_items(
                            state,
                            Some(provider),
                            [PlayableMedia::Spotify(playable_id)],
                        );
                    }
                    UserQueueRoute::NativeSpotify => {
                        self.spotify_api()
                            .add_item_to_queue(playable_id, None)
                            .await?;
                    }
                }
            }
            ClientRequest::AddPlayableToPlaylist(playlist_id, playable_id) => {
                self.add_item_to_playlist(state, playlist_id, playable_id)
                    .await?;
            }
            ClientRequest::AddAlbumToQueue(album_id) => {
                let album_context = self.album_context(album_id).await?;

                if let Context::Album { album: _, tracks } = album_context {
                    for track in tracks {
                        self.spotify_api()
                            .add_item_to_queue(PlayableId::Track(track.id), None)
                            .await?;
                    }
                }
            }
            ClientRequest::AddToLibrary(item) => {
                self.add_to_library(state, item).await?;
            }
            ClientRequest::AddTracksToLibrary(tracks) => {
                self.add_tracks_to_library(state, tracks).await?;
            }
            ClientRequest::DeleteTracksFromLibrary(track_ids) => {
                self.delete_tracks_from_library(state, track_ids).await?;
            }
            ClientRequest::DeleteFromLibrary(id) => {
                self.delete_from_library(state, id).await?;
            }
            ClientRequest::CreatePlaylist {
                playlist_name,
                public,
                collab,
                desc,
            } => {
                let user_id = spotify_playlist_owner_id(state.data.read().user_data.user.as_ref())?;
                self.create_new_playlist(
                    state,
                    user_id,
                    playlist_name.as_str(),
                    public,
                    collab,
                    desc.as_str(),
                )
                .await?;
            }
            ClientRequest::CreateSpotifyPlaylistWithTracks {
                playlist_name,
                public,
                collab,
                desc,
                tracks,
            } => {
                let user_id = spotify_playlist_owner_id(state.data.read().user_data.user.as_ref())?;
                let playlist_id = self
                    .create_new_playlist(
                        state,
                        user_id,
                        playlist_name.as_str(),
                        public,
                        collab,
                        desc.as_str(),
                    )
                    .await?;
                for track in tracks {
                    self.add_item_to_playlist(state, playlist_id.clone_static(), track.id.into())
                        .await?;
                }
            }
            ClientRequest::CreateYouTubePlaylist {
                playlist_name,
                public,
            } => {
                youtube_create_playlist_and_refresh(state, playlist_name.as_str(), public).await?;
            }
            ClientRequest::CreateYouTubePlaylistWithTracks {
                playlist_name,
                public,
                tracks,
            } => {
                let playlist_id =
                    youtube_create_playlist_and_refresh(state, playlist_name.as_str(), public)
                        .await?;
                let mut failures = Vec::new();
                for track in tracks {
                    if let Err(error) = youtube_add_video_to_playlist(&playlist_id, &track.id).await
                    {
                        failures.push(error.to_string());
                    }
                }
                if !failures.is_empty() {
                    return Err(anyhow::anyhow!(
                        "Playlist created, but {} item(s) could not be added",
                        failures.len()
                    ));
                }
            }
            ClientRequest::CreateUnifiedPlaylist { playlist_name } => {
                let now = std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .context("read system clock")?;
                let id = format!("local-{}", now.as_nanos());
                state
                    .data
                    .write()
                    .upsert_unified_playlist(crate::state::UnifiedPlaylist {
                        id,
                        name: playlist_name,
                        items: Vec::new(),
                        updated_at: now.as_secs(),
                        next_entry_id: 1,
                    })?;
            }
            ClientRequest::CreateUnifiedPlaylistWithItems {
                playlist_name,
                items,
                operation,
            } => {
                if let Some(operation) = operation.as_ref() {
                    validate_unified_playlist_operation(state, operation, "", true, &items)?;
                }
                let now = std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .context("read system clock")?;
                let id = format!("local-{}", now.as_nanos());
                state
                    .data
                    .write()
                    .upsert_unified_playlist(crate::state::UnifiedPlaylist {
                        id,
                        name: playlist_name,
                        items,
                        updated_at: now.as_secs(),
                        next_entry_id: 1,
                    })?;
            }
            ClientRequest::CreateUnifiedPlaylistFromHistory {
                playlist_name,
                items,
            } => {
                let now = std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .context("read system clock")?;
                let id = format!("local-{}", now.as_nanos());
                state
                    .data
                    .write()
                    .upsert_unified_playlist(crate::state::UnifiedPlaylist {
                        id,
                        name: playlist_name,
                        items,
                        updated_at: now.as_secs(),
                        next_entry_id: 1,
                    })?;
            }
            ClientRequest::RenameSpotifyPlaylist { playlist_id, name } => {
                self.rename_spotify_playlist(state, playlist_id, name.as_str())
                    .await?;
            }
            ClientRequest::RenameYouTubePlaylist { playlist_id, name } => {
                youtube_rename_playlist_and_refresh(state, playlist_id.as_str(), name.as_str())
                    .await?;
            }
            ClientRequest::RenameUnifiedPlaylist { playlist_id, name } => {
                state
                    .data
                    .write()
                    .rename_unified_playlist(playlist_id.as_str(), name)?;
            }
            ClientRequest::DeleteYouTubePlaylist { playlist_id } => {
                youtube_delete_playlist(&playlist_id).await?;
                let library = youtube::YouTubeMusic::new(config::get_config())
                    .await?
                    .library()
                    .await?;
                state.data.write().user_data.youtube_library = library;
            }
            ClientRequest::DeleteUnifiedPlaylist { playlist_id } => {
                state
                    .data
                    .write()
                    .delete_unified_playlist(playlist_id.as_str())?;
            }
            ClientRequest::BackupUnifiedPlaylistToListenBrainz {
                unified_playlist_id,
                operation_reference,
            } => {
                backup_unified_playlist_to_listenbrainz(
                    self,
                    state,
                    &unified_playlist_id,
                    &operation_reference,
                )
                .await?;
            }
            ClientRequest::ApplyUnifiedPlaylistListenBrainzPush {
                unified_playlist_id,
                operation_reference,
                operation_id,
            } => {
                apply_listenbrainz_push(
                    state,
                    &unified_playlist_id,
                    &operation_reference,
                    &operation_id,
                )
                .await?;
            }
            ClientRequest::ApplyUnifiedPlaylistListenBrainzPull {
                unified_playlist_id,
                operation_reference,
                operation_id,
            } => {
                apply_listenbrainz_pull(
                    state,
                    &unified_playlist_id,
                    &operation_reference,
                    &operation_id,
                )
                .await?;
            }
            ClientRequest::ApplyUnifiedPlaylistListenBrainzResolve {
                unified_playlist_id,
                operation_reference,
                operation_id,
                policy,
                decisions,
            } => {
                apply_listenbrainz_resolve(
                    state,
                    &unified_playlist_id,
                    &operation_reference,
                    &operation_id,
                    policy,
                    &decisions,
                )
                .await?;
            }
            ClientRequest::LinkUnifiedPlaylistToYouTube {
                unified_playlist_id,
                youtube_playlist_id,
            } => {
                let mut data = state.data.write();
                anyhow::ensure!(
                    data.unified_playlists
                        .iter()
                        .any(|playlist| playlist.id == unified_playlist_id),
                    "unified playlist not found"
                );
                let mut link = data
                    .playlist_links
                    .iter()
                    .find(|link| link.unified_playlist_id == unified_playlist_id)
                    .cloned()
                    .unwrap_or_else(|| crate::state::PlaylistLink {
                        unified_playlist_id: unified_playlist_id.clone(),
                        ..crate::state::PlaylistLink::default()
                    });
                link.youtube_playlist_id = Some(youtube_playlist_id.clone());
                link.last_local_snapshot = None;
                link.last_youtube_snapshot = None;
                let account_id = config::AccountRegistry::load(&config::get_config().config_folder)
                    .ok()
                    .and_then(|registry| {
                        registry
                            .active_id(config::ActiveProvider::YouTubeMusic)
                            .map(str::to_owned)
                    })
                    .unwrap_or_else(|| "unknown".to_owned());
                let account_epoch = state
                    .ui
                    .lock()
                    .provider_selection_epoch(config::ActiveProvider::YouTubeMusic);
                link.upsert_projection(crate::state::PlaylistProjectionState::pending(
                    crate::state::PlaylistProjectionTarget {
                        provider: crate::state::Provider::YouTubeMusic,
                        account_id,
                        account_epoch,
                        playlist_id: youtube_playlist_id,
                    },
                ));
                link.updated_at = std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .context("read system clock")?
                    .as_secs();
                data.upsert_playlist_link(link)?;
            }
            ClientRequest::SyncUnifiedPlaylistToYouTube {
                unified_playlist_id,
            } => {
                project_unified_playlist_to_youtube(state, &unified_playlist_id).await?;
            }
            ClientRequest::UnlinkUnifiedPlaylistFromYouTube {
                unified_playlist_id,
            } => {
                let mut data = state.data.write();
                let Some(mut link) = data
                    .playlist_links
                    .iter()
                    .find(|link| link.unified_playlist_id == unified_playlist_id)
                    .cloned()
                else {
                    anyhow::bail!("unified playlist is not linked to YouTube");
                };
                anyhow::ensure!(
                    link.youtube_playlist_id.is_some(),
                    "unified playlist is not linked to YouTube"
                );
                link.youtube_playlist_id = None;
                link.last_local_snapshot = None;
                link.last_youtube_snapshot = None;
                for projection in &mut link.projections {
                    if projection.target.provider == crate::state::Provider::YouTubeMusic {
                        projection.status = crate::state::PlaylistProjectionStatus::Detached;
                    }
                }
                link.updated_at = std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .context("read system clock")?
                    .as_secs();
                data.upsert_playlist_link(link)?;
            }
            _ => unreachable!("request routed to the wrong playlist-mutation handler"),
        }
        Ok(())
    }
}

async fn backup_unified_playlist_to_listenbrainz(
    client: &AppClient,
    state: &SharedState,
    playlist_id: &str,
    operation_reference: &str,
) -> Result<()> {
    let configs = config::get_config();
    if !configs.app_config.listenbrainz.enabled {
        state
            .ui
            .lock()
            .finish_listenbrainz_backup_failed(playlist_id, operation_reference);
        anyhow::bail!("ListenBrainz integration is disabled");
    }
    let Some(token) = configs.listenbrainz_token() else {
        state
            .ui
            .lock()
            .finish_listenbrainz_backup_failed(playlist_id, operation_reference);
        anyhow::bail!("ListenBrainz token is missing");
    };
    let (playlist_name, description, snapshot_hash) = {
        let data = state.data.read();
        let playlist = data
            .unified_playlists
            .iter()
            .find(|playlist| playlist.id == playlist_id)
            .context("unified playlist not found")?;
        anyhow::ensure!(
            !data.playlist_links.iter().any(|link| {
                link.unified_playlist_id == playlist_id && link.listenbrainz_playlist_id.is_some()
            }),
            "unified playlist already has a ListenBrainz backup"
        );
        (
            playlist.name.clone(),
            crate::cli::listenbrainz_manifest::description_envelope_with_budget(
                playlist,
                crate::cli::listenbrainz_manifest::DESCRIPTION_CHARACTER_BUDGET,
            )?,
            playlist.snapshot_hash(),
        )
    };

    let playlist_mbid = match super::listenbrainz::create_description_backup(
        &client.http,
        &token,
        &playlist_name,
        &description,
    )
    .await
    {
        Ok(playlist_mbid) => playlist_mbid,
        Err(error) => {
            if let Some(playlist_mbid) = error.partial_playlist_mbid() {
                state.ui.lock().finish_listenbrainz_backup_partial(
                    playlist_id,
                    operation_reference,
                    playlist_mbid,
                );
            } else {
                state
                    .ui
                    .lock()
                    .finish_listenbrainz_backup_failed(playlist_id, operation_reference);
            }
            return Err(error.into());
        }
    };

    let updated_at = match std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH) {
        Ok(duration) => duration.as_secs(),
        Err(error) => {
            state.ui.lock().finish_listenbrainz_backup_partial(
                playlist_id,
                operation_reference,
                &playlist_mbid,
            );
            return Err(error).context("read system clock");
        }
    };
    let persistence = {
        let mut data = state.data.write();
        let playlist_is_current = data
            .unified_playlists
            .iter()
            .find(|playlist| playlist.id == playlist_id)
            .is_some_and(|playlist| playlist.snapshot_hash() == snapshot_hash);
        let backup_is_still_unlinked = !data.playlist_links.iter().any(|link| {
            link.unified_playlist_id == playlist_id && link.listenbrainz_playlist_id.is_some()
        });
        if !playlist_is_current || !backup_is_still_unlinked {
            Err(anyhow::anyhow!(
                "unified playlist or its ListenBrainz link changed while the backup was being created"
            ))
        } else {
            let mut link = data
                .playlist_links
                .iter()
                .find(|link| link.unified_playlist_id == playlist_id)
                .cloned()
                .unwrap_or_else(|| crate::state::PlaylistLink {
                    unified_playlist_id: playlist_id.to_owned(),
                    ..crate::state::PlaylistLink::default()
                });
            link.listenbrainz_playlist_id = Some(playlist_mbid.clone());
            link.updated_at = updated_at;
            data.upsert_playlist_link(link)
        }
    };
    if let Err(error) = persistence {
        state.ui.lock().finish_listenbrainz_backup_partial(
            playlist_id,
            operation_reference,
            &playlist_mbid,
        );
        return Err(error.context(format!(
            "ListenBrainz backup {playlist_mbid} succeeded, but its local link could not be saved"
        )));
    }
    state.ui.lock().finish_listenbrainz_backup_completed(
        playlist_id,
        operation_reference,
        &playlist_mbid,
    );
    Ok(())
}

fn apply_listenbrainz_settings() -> anyhow::Result<String> {
    let configs = config::get_config();
    anyhow::ensure!(
        configs.app_config.listenbrainz.enabled,
        "ListenBrainz integration is disabled"
    );
    anyhow::ensure!(
        configs.app_config.listenbrainz.read_only_checking,
        "ListenBrainz read-only checking is disabled"
    );
    configs
        .listenbrainz_token()
        .context("ListenBrainz token is missing")
}

fn apply_listenbrainz_cancelled(
    state: &SharedState,
    playlist_id: &str,
    operation_reference: &str,
) -> bool {
    !state
        .ui
        .lock()
        .listenbrainz_sync_check_is_active(playlist_id, operation_reference)
}

/// Completion of a `ListenBrainz` apply inside the blocking worker, carrying
/// the disk state the transaction persisted so shared memory can be
/// refreshed from it.
enum ListenBrainzApplyCompletion {
    Pushed(String),
    Pulled(String),
    Resolved(String),
}

struct ListenBrainzApplyFinished {
    playlists: Vec<crate::state::UnifiedPlaylist>,
    links: Vec<crate::state::PlaylistLink>,
    completion: ListenBrainzApplyCompletion,
}

async fn apply_listenbrainz_push(
    state: &SharedState,
    playlist_id: &str,
    operation_reference: &str,
    operation_id: &str,
) -> Result<()> {
    use crate::state::{
        LISTENBRAINZ_SYNC_APPLY_FAILED_MESSAGE, LISTENBRAINZ_SYNC_APPLY_FAILED_NEXT_ACTION,
        LISTENBRAINZ_SYNC_APPLY_PUSH_COMPLETED_MESSAGE, LISTENBRAINZ_SYNC_APPLY_UNKNOWN_MESSAGE,
        LISTENBRAINZ_SYNC_APPLY_UNKNOWN_NEXT_ACTION,
    };
    let fail = |message: &'static str, next_action: &'static str| {
        state.ui.lock().finish_listenbrainz_sync_apply_failed(
            playlist_id,
            operation_reference,
            message,
            next_action,
        );
    };
    let token = match apply_listenbrainz_settings() {
        Ok(token) => token,
        Err(error) => {
            fail(
                LISTENBRAINZ_SYNC_APPLY_FAILED_MESSAGE,
                LISTENBRAINZ_SYNC_APPLY_FAILED_NEXT_ACTION,
            );
            return Err(error);
        }
    };
    if apply_listenbrainz_cancelled(state, playlist_id, operation_reference) {
        return Ok(());
    }
    // Client tasks must not hold the data lock across awaits, so the
    // transaction runs in a blocking worker on fresh disk state exactly like
    // the CLI flow, then the shared memory is refreshed from what the
    // transaction persisted. The UI busy guard prevents a second sync op
    // meanwhile.
    let configs = config::get_config();
    let (config_folder, cache_folder) =
        (configs.config_folder.clone(), configs.cache_folder.clone());
    let (playlist_id_owned, operation_id_owned) = (playlist_id.to_owned(), operation_id.to_owned());
    let finished = tokio::task::spawn_blocking(move || {
        let mut data = crate::state::AppData::new(&config_folder, &cache_folder);
        let playlist = data
            .unified_playlists
            .iter()
            .find(|playlist| playlist.id == playlist_id_owned)
            .cloned()
            .context("unified playlist not found")?;
        let link = data
            .playlist_links
            .iter()
            .find(|link| link.unified_playlist_id == playlist_id_owned)
            .context("unified playlist link not found")?;
        let remote_playlist_id = link
            .listenbrainz_playlist_id
            .clone()
            .context("ListenBrainz playlist link not found")?;
        anyhow::ensure!(
            link.listenbrainz_sync.as_ref().is_some_and(|sync| matches!(
                sync.status,
                crate::state::ListenBrainzSyncStatus::Clean
                    | crate::state::ListenBrainzSyncStatus::Drifted
            )),
            "ListenBrainz push needs a clean or locally-changed link"
        );
        let (projection, projection_preview) =
            super::listenbrainz_projection::preview_native_projection(
                &playlist,
                &[],
                crate::cli::listenbrainz_manifest::DESCRIPTION_CHARACTER_BUDGET,
            )?;
        anyhow::ensure!(
            projection_preview.is_ready(),
            "ListenBrainz projection exceeds the manifest budget"
        );
        let adapter = super::listenbrainz_push::ListenBrainzMutationAdapter::new(
            reqwest::Client::new(),
        );
        let started_at = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .context("read system clock")?
            .as_secs();
        let result = tokio::runtime::Runtime::new()?.block_on(
            super::listenbrainz_push::execute_push_transaction(
                &adapter,
                &mut data,
                &token,
                &remote_playlist_id,
                &playlist,
                &projection,
                &operation_id_owned,
                started_at,
            ),
        )?;
        match result.status {
            super::listenbrainz_push::PushTransactionStatus::Verified => {
                Ok::<_, anyhow::Error>(ListenBrainzApplyFinished {
                    playlists: data.unified_playlists.clone(),
                    links: data.playlist_links.clone(),
                    completion: ListenBrainzApplyCompletion::Pushed(format!(
                        "Pushed {} native rows ({} manifest-only) of {} occurrences; remote verified.",
                        projection_preview.native_rows,
                        projection_preview.manifest_only,
                        projection_preview.total_occurrences,
                    )),
                })
            }
            super::listenbrainz_push::PushTransactionStatus::OutcomeUnknown => {
                anyhow::bail!("ListenBrainz push outcome is unknown; verify the remote state")
            }
            _ => anyhow::bail!("ListenBrainz push was rejected; local data is preserved"),
        }
    })
    .await
    .context("ListenBrainz push worker was cancelled");
    let finished = match finished {
        Ok(Ok(finished)) => finished,
        Ok(Err(error)) => {
            let unknown = error.to_string().contains("outcome is unknown");
            fail(
                if unknown {
                    LISTENBRAINZ_SYNC_APPLY_UNKNOWN_MESSAGE
                } else {
                    LISTENBRAINZ_SYNC_APPLY_FAILED_MESSAGE
                },
                if unknown {
                    LISTENBRAINZ_SYNC_APPLY_UNKNOWN_NEXT_ACTION
                } else {
                    LISTENBRAINZ_SYNC_APPLY_FAILED_NEXT_ACTION
                },
            );
            return Err(error);
        }
        Err(error) => {
            fail(
                LISTENBRAINZ_SYNC_APPLY_FAILED_MESSAGE,
                LISTENBRAINZ_SYNC_APPLY_FAILED_NEXT_ACTION,
            );
            return Err(error);
        }
    };
    let (message, details) = match finished.completion {
        ListenBrainzApplyCompletion::Pushed(details) => {
            (LISTENBRAINZ_SYNC_APPLY_PUSH_COMPLETED_MESSAGE, details)
        }
        ListenBrainzApplyCompletion::Pulled(details) => (
            crate::state::LISTENBRAINZ_SYNC_APPLY_PULL_COMPLETED_MESSAGE,
            details,
        ),
        ListenBrainzApplyCompletion::Resolved(details) => (
            crate::state::LISTENBRAINZ_SYNC_APPLY_RESOLVE_COMPLETED_MESSAGE,
            details,
        ),
    };
    {
        let mut data = state.data.write();
        data.unified_playlists = finished.playlists;
        data.playlist_links = finished.links;
    }
    state.ui.lock().finish_listenbrainz_sync_applied(
        playlist_id,
        operation_reference,
        message,
        details,
    );
    Ok(())
}

async fn apply_listenbrainz_pull(
    state: &SharedState,
    playlist_id: &str,
    operation_reference: &str,
    operation_id: &str,
) -> Result<()> {
    use crate::state::{
        LISTENBRAINZ_SYNC_APPLY_FAILED_MESSAGE, LISTENBRAINZ_SYNC_APPLY_FAILED_NEXT_ACTION,
    };
    let fail = |message: &'static str, next_action: &'static str| {
        state.ui.lock().finish_listenbrainz_sync_apply_failed(
            playlist_id,
            operation_reference,
            message,
            next_action,
        );
    };
    let token = match apply_listenbrainz_settings() {
        Ok(token) => token,
        Err(error) => {
            fail(
                LISTENBRAINZ_SYNC_APPLY_FAILED_MESSAGE,
                LISTENBRAINZ_SYNC_APPLY_FAILED_NEXT_ACTION,
            );
            return Err(error);
        }
    };
    if apply_listenbrainz_cancelled(state, playlist_id, operation_reference) {
        return Ok(());
    }
    let configs = config::get_config();
    let (config_folder, cache_folder) =
        (configs.config_folder.clone(), configs.cache_folder.clone());
    let (playlist_id_owned, operation_id_owned) = (playlist_id.to_owned(), operation_id.to_owned());
    let finished = tokio::task::spawn_blocking(move || {
        let mut data = crate::state::AppData::new(&config_folder, &cache_folder);
        let remote_playlist_id = data
            .playlist_links
            .iter()
            .find(|link| link.unified_playlist_id == playlist_id_owned)
            .and_then(|link| link.listenbrainz_playlist_id.clone())
            .context("ListenBrainz playlist link not found")?;
        let adapter =
            super::listenbrainz_push::ListenBrainzMutationAdapter::new(reqwest::Client::new());
        let runtime = tokio::runtime::Runtime::new()?;
        let remote_value = runtime.block_on(adapter.fetch(&token, &remote_playlist_id))?;
        let observed_at = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .context("read system clock")?
            .as_secs();
        let (_, result) = super::listenbrainz_pull::execute_pull_apply(
            &mut data,
            &remote_playlist_id,
            &playlist_id_owned,
            &remote_value,
            &operation_id_owned,
            observed_at,
        )?;
        Ok::<_, anyhow::Error>(ListenBrainzApplyFinished {
            playlists: data.unified_playlists.clone(),
            links: data.playlist_links.clone(),
            completion: ListenBrainzApplyCompletion::Pulled(format!(
                "Applied {} occurrences ({} unresolved); rollback {}.",
                result.occurrences,
                result.unresolved,
                if result.rollback_available {
                    "available"
                } else {
                    "unavailable"
                },
            )),
        })
    })
    .await
    .context("ListenBrainz pull worker was cancelled");
    let finished = match finished {
        Ok(Ok(finished)) => finished,
        Ok(Err(error)) | Err(error) => {
            fail(
                LISTENBRAINZ_SYNC_APPLY_FAILED_MESSAGE,
                LISTENBRAINZ_SYNC_APPLY_FAILED_NEXT_ACTION,
            );
            return Err(error);
        }
    };
    let ListenBrainzApplyCompletion::Pulled(details) = finished.completion else {
        unreachable!("pull worker reports pull completions")
    };
    {
        let mut data = state.data.write();
        data.unified_playlists = finished.playlists;
        data.playlist_links = finished.links;
    }
    state.ui.lock().finish_listenbrainz_sync_applied(
        playlist_id,
        operation_reference,
        crate::state::LISTENBRAINZ_SYNC_APPLY_PULL_COMPLETED_MESSAGE,
        details,
    );
    Ok(())
}

#[allow(clippy::too_many_arguments)]
async fn apply_listenbrainz_resolve(
    state: &SharedState,
    playlist_id: &str,
    operation_reference: &str,
    operation_id: &str,
    policy: super::listenbrainz_resolution::ResolutionPolicy,
    decisions: &[super::listenbrainz_resolution::ConflictDecision],
) -> Result<()> {
    use crate::state::{
        LISTENBRAINZ_SYNC_APPLY_FAILED_MESSAGE, LISTENBRAINZ_SYNC_APPLY_FAILED_NEXT_ACTION,
    };
    let fail = |message: &'static str, next_action: &'static str| {
        state.ui.lock().finish_listenbrainz_sync_apply_failed(
            playlist_id,
            operation_reference,
            message,
            next_action,
        );
    };
    let token = match apply_listenbrainz_settings() {
        Ok(token) => token,
        Err(error) => {
            fail(
                LISTENBRAINZ_SYNC_APPLY_FAILED_MESSAGE,
                LISTENBRAINZ_SYNC_APPLY_FAILED_NEXT_ACTION,
            );
            return Err(error);
        }
    };
    let remote_playlist_id = {
        let data = state.data.read();
        data.playlist_links
            .iter()
            .find(|link| link.unified_playlist_id == playlist_id)
            .and_then(|link| link.listenbrainz_playlist_id.clone())
            .context("ListenBrainz playlist link not found")
    };
    let remote_playlist_id = match remote_playlist_id {
        Ok(remote_playlist_id) => remote_playlist_id,
        Err(error) => {
            fail(
                LISTENBRAINZ_SYNC_APPLY_FAILED_MESSAGE,
                LISTENBRAINZ_SYNC_APPLY_FAILED_NEXT_ACTION,
            );
            return Err(error);
        }
    };
    if apply_listenbrainz_cancelled(state, playlist_id, operation_reference) {
        return Ok(());
    }
    let configs = config::get_config();
    let (config_folder, cache_folder) =
        (configs.config_folder.clone(), configs.cache_folder.clone());
    let (playlist_id_owned, operation_id_owned, decisions_owned) = (
        playlist_id.to_owned(),
        operation_id.to_owned(),
        decisions.to_vec(),
    );
    let finished = tokio::task::spawn_blocking(move || {
        let mut data = crate::state::AppData::new(&config_folder, &cache_folder);
        let adapter =
            super::listenbrainz_push::ListenBrainzMutationAdapter::new(reqwest::Client::new());
        let runtime = tokio::runtime::Runtime::new()?;
        let remote_value = runtime.block_on(adapter.fetch(&token, &remote_playlist_id))?;
        let observed_at = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .context("read system clock")?
            .as_secs();
        let (plan, _, result) =
            runtime.block_on(super::listenbrainz_resolution::execute_resolution(
                &adapter,
                &mut data,
                &token,
                &remote_playlist_id,
                &playlist_id_owned,
                &remote_value,
                policy,
                &decisions_owned,
                &[],
                &[],
                &operation_id_owned,
                observed_at,
            ))?;
        anyhow::ensure!(result.completed, "ListenBrainz resolution did not complete");
        Ok::<_, anyhow::Error>(ListenBrainzApplyFinished {
            playlists: data.unified_playlists.clone(),
            links: data.playlist_links.clone(),
            completion: ListenBrainzApplyCompletion::Resolved(format!(
                "Resolved {} additions, {} removals; local {}, remote {}.",
                plan.preview.additions,
                plan.preview.removals,
                if plan.preview.local_write_required {
                    "written"
                } else {
                    "unchanged"
                },
                if plan.preview.remote_write_required {
                    "written"
                } else {
                    "unchanged"
                },
            )),
        })
    })
    .await
    .context("ListenBrainz resolve worker was cancelled");
    let finished = match finished {
        Ok(Ok(finished)) => finished,
        Ok(Err(error)) | Err(error) => {
            fail(
                LISTENBRAINZ_SYNC_APPLY_FAILED_MESSAGE,
                LISTENBRAINZ_SYNC_APPLY_FAILED_NEXT_ACTION,
            );
            return Err(error);
        }
    };
    let ListenBrainzApplyCompletion::Resolved(details) = finished.completion else {
        unreachable!("resolve worker reports resolve completions")
    };
    {
        let mut data = state.data.write();
        data.unified_playlists = finished.playlists;
        data.playlist_links = finished.links;
    }
    state.ui.lock().finish_listenbrainz_sync_applied(
        playlist_id,
        operation_reference,
        crate::state::LISTENBRAINZ_SYNC_APPLY_RESOLVE_COMPLETED_MESSAGE,
        details,
    );
    Ok(())
}

async fn youtube_create_playlist_and_refresh(
    state: &SharedState,
    name: &str,
    public: bool,
) -> Result<String> {
    let playlist_id = youtube_create_playlist(name, public).await?;
    let library = youtube::YouTubeMusic::new(config::get_config())
        .await?
        .library()
        .await?;
    state.data.write().user_data.youtube_library = library;
    Ok(playlist_id)
}

async fn youtube_rename_playlist_and_refresh(
    state: &SharedState,
    playlist_id: &str,
    name: &str,
) -> Result<()> {
    let youtube = youtube::YouTubeMusic::new(config::get_config()).await?;
    youtube.rename_playlist(playlist_id, name).await?;
    state.data.write().user_data.youtube_library = youtube.library().await?;
    Ok(())
}

fn youtube_liked_context_is_visible(state: &SharedState) -> bool {
    matches!(
        state.ui.lock().current_page(),
        crate::state::PageState::YouTubeContext {
            id: crate::state::YouTubeContextId::LikedTracks,
            ..
        }
    )
}

async fn refresh_youtube_library_after_mutation(client: &AppClient, state: &SharedState) {
    if let Err(error) = client.refresh_youtube_library(state).await {
        crate::observability::log_safe_error!(
            warn,
            crate::observability::DiagnosticCode::YOUTUBE_LIBRARY_FETCH_FAILED,
            crate::observability::ErrorCategory::Unavailable,
            &error,
            "YouTube playlist changed, but its library projection could not be refreshed"
        );
    }
}

async fn youtube_append_baseline(
    playlist_id: &str,
    video_id: &str,
) -> Option<YouTubeAppendReadBack> {
    let youtube = youtube::YouTubeMusic::new(config::get_config())
        .await
        .ok()?;
    youtube
        .context(&crate::state::YouTubeContextId::Playlist(
            playlist_id.to_owned(),
        ))
        .await
        .ok()
        .map(|context| YouTubeAppendReadBack {
            total: context.tracks.len(),
            media_occurrences: context
                .tracks
                .iter()
                .filter(|row| row.id == video_id)
                .count(),
            last_media_position: context.tracks.iter().rposition(|row| row.id == video_id),
        })
}

async fn verified_youtube_append_with(
    youtube: &youtube::YouTubeMusic,
    playlist_id: &str,
    video_id: &str,
) -> Result<Option<String>> {
    let baseline = youtube
        .context(&crate::state::YouTubeContextId::Playlist(
            playlist_id.to_owned(),
        ))
        .await
        .ok()
        .map(|context| YouTubeAppendReadBack {
            total: context.tracks.len(),
            media_occurrences: context
                .tracks
                .iter()
                .filter(|row| row.id == video_id)
                .count(),
            last_media_position: context.tracks.iter().rposition(|row| row.id == video_id),
        });
    let intent = YouTubeMutationIntent::Append {
        operation_id: super::PlaylistMutationOperationId(rand::rng().random()),
        playlist_id: playlist_id.to_owned(),
        video_id: video_id.to_owned(),
        baseline,
    };
    let PlaylistMutationEffect::InvalidatePlaylist {
        appended_occurrence_token,
        ..
    } = playlist_mutation_effect(execute_youtube_playlist_mutation_with(youtube, &intent).await)?;
    Ok(appended_occurrence_token)
}

async fn refresh_visible_youtube_playlist_after_mutation(state: &SharedState, playlist_id: &str) {
    match youtube::YouTubeMusic::new(config::get_config()).await {
        Ok(youtube) => refresh_visible_youtube_playlist(state, &youtube, playlist_id).await,
        Err(error) => {
            crate::observability::log_safe_error!(
                warn,
                crate::observability::DiagnosticCode::YOUTUBE_CONTEXT_LOAD_FAILED,
                crate::observability::ErrorCategory::Unavailable,
                &error,
                "YouTube playlist changed, but its visible projection could not be refreshed"
            );
        }
    }
}

/// Refresh an already-open `YouTube` playlist after a successful mutation.
///
/// The library and context pages have separate state owners: refreshing the
/// library alone leaves an open playlist page displaying its old snapshot.
/// A follow-up context failure must not turn a successful remote mutation into
/// a false mutation failure, so the refresh is deliberately best effort and
/// records only a bounded diagnostic on failure.
async fn refresh_visible_youtube_playlist(
    state: &SharedState,
    youtube: &youtube::YouTubeMusic,
    playlist_id: &str,
) {
    let context_id = crate::state::YouTubeContextId::Playlist(playlist_id.to_owned());
    let visible = {
        let ui = state.ui.lock();
        matches!(
            ui.current_page(),
            crate::state::PageState::YouTubeContext { id, .. }
                if youtube_context_ids_match(id, &context_id)
        )
    };
    if !visible {
        return;
    }

    match youtube.context(&context_id).await {
        Ok(context) => {
            apply_youtube_context_to_visible_page(state, &context_id, context);
        }
        Err(error) => {
            crate::observability::log_safe_error!(
                warn,
                crate::observability::DiagnosticCode::YOUTUBE_CONTEXT_LOAD_FAILED,
                crate::observability::ErrorCategory::Unavailable,
                &error,
                "YouTube playlist changed, but its open page could not be refreshed"
            );
        }
    }
}

fn apply_youtube_context_to_visible_page(
    state: &SharedState,
    context_id: &crate::state::YouTubeContextId,
    context: crate::state::YouTubeContext,
) -> bool {
    let mut ui = state.ui.lock();
    apply_youtube_context_to_page(ui.current_page_mut(), context_id, context)
}

fn apply_youtube_context_to_page(
    page: &mut crate::state::PageState,
    context_id: &crate::state::YouTubeContextId,
    mut context: crate::state::YouTubeContext,
) -> bool {
    let crate::state::PageState::YouTubeContext {
        id,
        context: page_context,
        state: page_state,
        ..
    } = page
    else {
        return false;
    };
    if !youtube_context_ids_match(id, context_id) {
        return false;
    }
    if let Some(previous) = page_context.as_ref() {
        preserve_youtube_tokens_for_unchanged_prefix(previous, &mut context);
    }
    page_state.status = if context.tracks.is_empty() {
        crate::state::UiViewStatus::Empty
    } else {
        crate::state::UiViewStatus::Ready
    };
    *page_context = Some(context);
    true
}

fn preserve_youtube_tokens_for_unchanged_prefix(
    previous: &crate::state::YouTubeContext,
    refreshed: &mut crate::state::YouTubeContext,
) {
    if previous.tracks.len() > refreshed.tracks.len()
        || previous
            .tracks
            .iter()
            .zip(&refreshed.tracks)
            .any(|(old, new)| old.id != new.id)
    {
        return;
    }
    refreshed
        .playlist_set_video_ids
        .resize(refreshed.tracks.len(), None);
    for (position, token) in previous.playlist_set_video_ids.iter().enumerate() {
        if position >= previous.tracks.len() {
            break;
        }
        if token
            .as_deref()
            .is_some_and(|token| !token.trim().is_empty())
        {
            refreshed.playlist_set_video_ids[position].clone_from(token);
        }
    }
}

fn youtube_context_ids_match(
    left: &crate::state::YouTubeContextId,
    right: &crate::state::YouTubeContextId,
) -> bool {
    match (left, right) {
        (
            crate::state::YouTubeContextId::Playlist(left),
            crate::state::YouTubeContextId::Playlist(right),
        ) => crate::state::youtube_playlist_ids_match(left, right),
        _ => left == right,
    }
}

fn append_local_unified_items(
    state: &SharedState,
    playlist_id: &str,
    items: Vec<crate::state::UnifiedPlaylistItem>,
) -> Result<()> {
    state
        .data
        .write()
        .append_unified_playlist_items(playlist_id, items)?;
    Ok(())
}

/// Append local unified items to a linked `YouTube` playlist. YouTube-backed
/// rows already have an authoritative video ID; Spotify rows require an
/// explicit high-confidence metadata match and fail closed otherwise.
async fn append_unified_items_to_youtube(
    target_playlist_id: &str,
    items: &[crate::state::UnifiedPlaylistItem],
) -> Result<(
    crate::state::YouTubeLibrary,
    Option<String>,
    Option<crate::state::YouTubeContext>,
)> {
    let youtube = youtube::YouTubeMusic::new(config::get_config()).await?;
    let mut video_ids = Vec::with_capacity(items.len());
    for item in items {
        let video_id = match item.media_id.provider {
            crate::state::Provider::YouTubeMusic => item.media_id.raw_id.clone(),
            crate::state::Provider::Spotify => {
                let query = format!("{} {}", item.title, item.artists);
                let results = youtube.search(&query).await?;
                let candidates = results
                    .songs
                    .into_iter()
                    .chain(results.videos)
                    .collect::<Vec<_>>();
                resolve_unified_item_youtube_id(item, &candidates)?
            }
        };
        anyhow::ensure!(
            !video_id.trim().is_empty(),
            "linked YouTube playlist item has no video identifier"
        );
        video_ids.push(video_id);
    }
    for video_id in video_ids {
        let _ = verified_youtube_append_with(&youtube, target_playlist_id, &video_id).await?;
    }
    let library = youtube.library().await?;
    let remote_context = youtube
        .context(&crate::state::YouTubeContextId::Playlist(
            target_playlist_id.to_owned(),
        ))
        .await
        .ok();
    let remote_snapshot = remote_context.as_ref().map(|context| {
        crate::state::UnifiedPlaylist::youtube_tracks_snapshot_hash(&context.tracks)
    });
    Ok((library, remote_snapshot, remote_context))
}

/// Perform an explicit initial/repair sync for a linked Unified playlist.
/// Existing remote occurrences are preserved; only missing occurrences in
/// local order are appended, so a retry cannot duplicate already-synced rows.
async fn project_unified_playlist_to_youtube(state: &SharedState, playlist_id: &str) -> Result<()> {
    let (playlist, target_playlist_id, accepted_mappings) = {
        let (account_id, account_epoch) = current_youtube_projection_scope(state);
        let data = state.data.read();
        let playlist = data
            .unified_playlists
            .iter()
            .find(|playlist| playlist.id == playlist_id)
            .cloned()
            .context("unified playlist not found")?;
        let target = data
            .playlist_links
            .iter()
            .find(|link| link.unified_playlist_id == playlist_id)
            .and_then(|link| link.youtube_playlist_id.clone())
            .context("unified playlist is not linked to YouTube")?;
        let mappings = data
            .playlist_links
            .iter()
            .find(|link| link.unified_playlist_id == playlist_id)
            .and_then(|link| {
                link.projection_for(
                    crate::state::Provider::YouTubeMusic,
                    &account_id,
                    account_epoch,
                    &target,
                )
            })
            .map(|projection| projection.mappings.clone())
            .unwrap_or_default();
        (playlist, target, mappings)
    };

    let youtube = youtube::YouTubeMusic::new(config::get_config()).await?;
    let remote_tracks = match youtube
        .context(&crate::state::YouTubeContextId::Playlist(
            target_playlist_id.clone(),
        ))
        .await
    {
        Ok(context) => context.tracks,
        Err(error) => {
            persist_unknown_projection_recovery(
                state,
                playlist_id,
                "linked target read-back failed before reconciliation",
            )?;
            return Err(crate::observability::preserve_error_diagnostic(
                error.context("load linked YouTube playlist context"),
                crate::observability::DiagnosticCode::YOUTUBE_PLAYLIST_CONTEXT_FETCH_FAILED,
                crate::observability::ErrorCategory::Contract,
            ));
        }
    };
    let existing_ids = remote_tracks
        .iter()
        .map(|track| track.id.clone())
        .collect::<Vec<_>>();
    let mut desired_ids = Vec::with_capacity(playlist.items.len());
    let mut unresolved = Vec::new();
    let mut resolved_mappings = Vec::new();
    let accepted_at = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.as_secs())
        .unwrap_or_default();
    for item in &playlist.items {
        let video_id = if let Some(mapping) = accepted_mappings
            .iter()
            .find(|mapping| mapping.local_entry_id == item.entry_id)
            .filter(|mapping| {
                mapping.remote_media_id.provider == crate::state::Provider::YouTubeMusic
            }) {
            mapping.remote_media_id.raw_id.clone()
        } else {
            match item.media_id.provider {
                crate::state::Provider::YouTubeMusic => item.media_id.raw_id.clone(),
                crate::state::Provider::Spotify => {
                    // Prefer an already-present remote row before searching. Search
                    // result IDs can vary between requests; re-resolving first would
                    // append another copy of the same song on every repair attempt.
                    if let Ok(video_id) = resolve_unified_item_youtube_id(item, &remote_tracks) {
                        video_id
                    } else {
                        let results = youtube
                            .search(&format!("{} {}", item.title, item.artists))
                            .await?;
                        let candidates = results
                            .songs
                            .into_iter()
                            .chain(results.videos)
                            .collect::<Vec<_>>();
                        if let Ok(video_id) = resolve_unified_item_youtube_id(item, &candidates) {
                            video_id
                        } else {
                            unresolved.push(format!("{} / {}", item.title, item.artists));
                            continue;
                        }
                    }
                }
            }
        };
        anyhow::ensure!(
            !video_id.trim().is_empty(),
            "unified playlist item has no video identifier"
        );
        desired_ids.push(video_id);
        if item.media_id.provider == crate::state::Provider::Spotify {
            let media_kind = remote_tracks
                .iter()
                .find(|track| track.id == desired_ids.last().cloned().unwrap_or_default())
                .map_or(crate::state::MediaKind::Track, |track| {
                    if track.is_video {
                        crate::state::MediaKind::Video
                    } else {
                        crate::state::MediaKind::Track
                    }
                });
            resolved_mappings.push(crate::state::PlaylistProjectionMapping {
                local_entry_id: item.entry_id,
                remote_media_id: crate::state::MediaId {
                    provider: crate::state::Provider::YouTubeMusic,
                    kind: media_kind,
                    raw_id: desired_ids.last().cloned().unwrap_or_default(),
                },
                remote_occurrence_token: None,
                accepted_at,
            });
        }
    }
    let mut append_failures = 0usize;
    for (index, video_id) in missing_youtube_ids(&desired_ids, &existing_ids)
        .into_iter()
        .enumerate()
    {
        if let Err(err) =
            verified_youtube_append_with(&youtube, &target_playlist_id, &video_id).await
        {
            append_failures = append_failures.saturating_add(1);
            tracing::warn!(
                diagnostic = %crate::observability::safe_error(
                    crate::observability::DiagnosticCode::YOUTUBE_PLAYLIST_MUTATION_FAILED,
                    crate::observability::ErrorCategory::Unavailable,
                    &err,
                ),
                "Unable to append item {} to the linked YouTube playlist",
                index + 1
            );
        }
    }

    let library = match youtube.library().await {
        Ok(library) => library,
        Err(error) => {
            persist_unknown_projection_recovery(
                state,
                playlist_id,
                "remote mutation completed but library refresh failed",
            )?;
            return Err(crate::observability::preserve_error_diagnostic(
                error.context("refresh YouTube library after projection"),
                crate::observability::DiagnosticCode::YOUTUBE_LIBRARY_FETCH_FAILED,
                crate::observability::ErrorCategory::Contract,
            ));
        }
    };
    let remote_context = youtube
        .context(&crate::state::YouTubeContextId::Playlist(
            target_playlist_id.clone(),
        ))
        .await
        .ok();
    let remote_snapshot = remote_context.as_ref().map(|context| {
        crate::state::UnifiedPlaylist::youtube_tracks_snapshot_hash(&context.tracks)
    });
    let remote_outcome_known = remote_snapshot.is_some();
    let recovery = if !remote_outcome_known {
        Some(crate::state::PlaylistProjectionRecovery {
            reason: "remote read-back was unavailable after projection".to_owned(),
            unresolved_items: unresolved.clone(),
            failed_appends: append_failures,
            next_action: "refresh the linked target before retrying".to_owned(),
        })
    } else if !unresolved.is_empty() || append_failures != 0 {
        Some(crate::state::PlaylistProjectionRecovery {
            reason: "projection completed partially".to_owned(),
            unresolved_items: unresolved.clone(),
            failed_appends: append_failures,
            next_action: "review the dry-run and retry only unresolved occurrences".to_owned(),
        })
    } else {
        None
    };
    state.data.write().user_data.youtube_library = library;
    record_linked_projection_snapshots(
        state,
        playlist_id,
        remote_snapshot,
        remote_context
            .as_ref()
            .map(|context| context.tracks.as_slice()),
        &resolved_mappings,
        recovery.as_ref(),
    )?;
    if recovery.is_none() {
        let (account_id, account_epoch) = current_youtube_projection_scope(state);
        let status = state.data.read().unified_playlist_projection_status(
            playlist_id,
            &account_id,
            account_epoch,
        );
        if matches!(
            status,
            Some(
                crate::state::PlaylistProjectionStatus::Conflict
                    | crate::state::PlaylistProjectionStatus::Drifted
            )
        ) {
            Err(crate::observability::preserve_error_diagnostic(
                anyhow::anyhow!("remote projection conflicts require preview confirmation"),
                crate::observability::DiagnosticCode::YOUTUBE_PLAYLIST_SYNC_PARTIAL,
                crate::observability::ErrorCategory::Contract,
            ))
        } else {
            Ok(())
        }
    } else {
        Err(crate::observability::preserve_error_diagnostic(
            anyhow::anyhow!(
                "YouTube sync partially completed; {} unresolved item(s), {} append failure(s)",
                unresolved.len(),
                append_failures
            ),
            crate::observability::DiagnosticCode::YOUTUBE_PLAYLIST_SYNC_PARTIAL,
            crate::observability::ErrorCategory::Contract,
        ))
    }
}

fn missing_youtube_ids(desired: &[String], existing: &[String]) -> Vec<String> {
    let mut available = std::collections::HashMap::<&str, usize>::new();
    for id in existing {
        *available.entry(id.as_str()).or_default() += 1;
    }
    desired
        .iter()
        .filter(|id| match available.get_mut(id.as_str()) {
            Some(count) if *count > 0 => {
                *count -= 1;
                false
            }
            _ => true,
        })
        .cloned()
        .collect()
}

fn validate_unified_playlist_operation(
    state: &SharedState,
    operation: &crate::state::PlaylistOperationEnvelope,
    playlist_id: &str,
    creating: bool,
    items: &[crate::state::UnifiedPlaylistItem],
) -> Result<()> {
    operation.validate_unified()?;
    let seeds = match &operation.intent {
        crate::state::PlaylistIntent::Create { seed }
        | crate::state::PlaylistIntent::Append { seed } => seed,
    };
    anyhow::ensure!(
        seeds.len() == items.len()
            && seeds.iter().zip(items).all(|(seed, item)| {
                seed.media_id == item.media_id
                    && seed.title == item.title
                    && seed.artists == item.artists
                    && seed.duration_ms == item.duration_ms
                    && seed.provider_url == item.provider_url
            }),
        "playlist operation seed payload does not match request"
    );
    match (&operation.destination, creating) {
        (
            crate::state::PlaylistDestination::New {
                target: crate::state::PlaylistTargetKind::Unified,
            },
            true,
        ) => {}
        (
            crate::state::PlaylistDestination::Existing {
                target: crate::state::PlaylistTargetKind::Unified,
                id,
            },
            false,
        ) => anyhow::ensure!(id == playlist_id, "playlist operation target is stale"),
        _ => anyhow::bail!("playlist operation destination does not match request"),
    }

    if let Some(provider) = operation.source_provider {
        let active_provider = match provider {
            crate::state::Provider::Spotify => config::ActiveProvider::Spotify,
            crate::state::Provider::YouTubeMusic => config::ActiveProvider::YouTubeMusic,
        };
        let current_epoch = state.ui.lock().provider_selection_epoch(active_provider);
        anyhow::ensure!(
            operation.source_account_epoch == Some(current_epoch)
                && operation.source_generation == Some(current_epoch),
            "playlist source selection is stale"
        );
    }

    if !creating {
        if let Some(expected_revision) = operation.expected_target_revision.as_deref() {
            let current_revision = state
                .data
                .read()
                .unified_playlists
                .iter()
                .find(|playlist| playlist.id == playlist_id)
                .map(crate::state::UnifiedPlaylist::snapshot_hash)
                .context("unified playlist not found")?;
            anyhow::ensure!(
                expected_revision == current_revision,
                "unified playlist changed before append"
            );
        }
    }
    Ok(())
}

fn record_linked_projection_snapshots(
    state: &SharedState,
    playlist_id: &str,
    remote_snapshot: Option<String>,
    remote_tracks: Option<&[crate::state::YouTubeTrack]>,
    accepted_mappings: &[crate::state::PlaylistProjectionMapping],
    recovery: Option<&crate::state::PlaylistProjectionRecovery>,
) -> Result<()> {
    let mut data = state.data.write();
    let local_playlist = data
        .unified_playlists
        .iter()
        .find(|playlist| playlist.id == playlist_id)
        .cloned()
        .context("unified playlist not found after mutation")?;
    let local_snapshot = local_playlist.snapshot_hash();
    let Some(mut link) = data
        .playlist_links
        .iter()
        .find(|link| link.unified_playlist_id == playlist_id)
        .cloned()
    else {
        return Ok(());
    };
    // The legacy snapshot pair is intentionally left untouched. New writes
    // belong to the account-scoped projection below; old fields remain only
    // for backward-compatible reads and migration diagnostics.
    let remote_revision = remote_snapshot;
    let account_id = config::AccountRegistry::load(&config::get_config().config_folder)
        .ok()
        .and_then(|registry| {
            registry
                .active_id(config::ActiveProvider::YouTubeMusic)
                .map(str::to_owned)
        })
        .unwrap_or_else(|| "unknown".to_owned());
    let account_epoch = state
        .ui
        .lock()
        .provider_selection_epoch(config::ActiveProvider::YouTubeMusic);
    let target_id = link.youtube_playlist_id.clone();
    if let Some(target_id) = target_id {
        if let Some(projection) = link.projection_for_mut(
            crate::state::Provider::YouTubeMusic,
            &account_id,
            account_epoch,
            &target_id,
        ) {
            projection.local_revision = Some(local_snapshot);
            projection.remote_revision = remote_revision;
            for mapping in accepted_mappings {
                if let Some(existing) = projection
                    .mappings
                    .iter_mut()
                    .find(|existing| existing.local_entry_id == mapping.local_entry_id)
                {
                    *existing = mapping.clone();
                } else {
                    projection.mappings.push(mapping.clone());
                }
            }
            if let Some(remote_tracks) = remote_tracks {
                let remote_items = remote_tracks
                    .iter()
                    .map(crate::state::UnifiedPlaylistItem::from_youtube_track)
                    .collect::<Vec<_>>();
                let plan = crate::state::dry_run_projection(
                    &local_playlist.items,
                    &remote_items,
                    &projection.mappings,
                );
                projection.conflicts = plan.conflicts;
                projection.status = if recovery.is_some() {
                    if projection.remote_revision.is_some() {
                        crate::state::PlaylistProjectionStatus::Partial
                    } else {
                        crate::state::PlaylistProjectionStatus::OutcomeUnknown
                    }
                } else {
                    plan.status
                };
                if projection.status != crate::state::PlaylistProjectionStatus::Clean {
                    projection.acknowledged_intents.clear();
                }
            } else {
                projection.status = crate::state::PlaylistProjectionStatus::OutcomeUnknown;
                projection.conflicts.clear();
                projection.acknowledged_intents.clear();
            }
            projection.recovery = recovery.cloned().or_else(|| {
                remote_tracks
                    .is_none()
                    .then(|| crate::state::PlaylistProjectionRecovery {
                        reason: "remote read-back was unavailable after mutation".to_owned(),
                        unresolved_items: Vec::new(),
                        failed_appends: 0,
                        next_action: "refresh the linked target before retrying".to_owned(),
                    })
            });
        }
    }
    link.updated_at = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .context("read system clock")?
        .as_secs();
    data.upsert_playlist_link(link)
}

fn current_youtube_projection_scope(state: &SharedState) -> (String, u64) {
    let account_id = config::AccountRegistry::load(&config::get_config().config_folder)
        .ok()
        .and_then(|registry| {
            registry
                .active_id(config::ActiveProvider::YouTubeMusic)
                .map(str::to_owned)
        })
        .unwrap_or_else(|| "unknown".to_owned());
    let epoch = state
        .ui
        .lock()
        .provider_selection_epoch(config::ActiveProvider::YouTubeMusic);
    (account_id, epoch)
}

fn persist_unknown_projection_recovery(
    state: &SharedState,
    playlist_id: &str,
    reason: &str,
) -> Result<()> {
    let (account_id, account_epoch) = current_youtube_projection_scope(state);
    let target_id = state
        .data
        .read()
        .playlist_links
        .iter()
        .find(|link| link.unified_playlist_id == playlist_id)
        .and_then(|link| link.youtube_playlist_id.clone())
        .context("unified playlist link not found")?;
    state.data.write().set_projection_recovery(
        playlist_id,
        crate::state::Provider::YouTubeMusic,
        &account_id,
        account_epoch,
        &target_id,
        crate::state::PlaylistProjectionStatus::OutcomeUnknown,
        crate::state::PlaylistProjectionRecovery {
            reason: reason.to_owned(),
            unresolved_items: Vec::new(),
            failed_appends: 0,
            next_action: "refresh the linked target before retrying".to_owned(),
        },
    )?;
    Ok(())
}

fn ensure_projection_retry_allowed(
    state: &SharedState,
    playlist_id: &str,
    operation_id: u64,
    intent_key: &str,
) -> Result<()> {
    let (account_id, account_epoch) = current_youtube_projection_scope(state);
    let data = state.data.read();
    let link = data
        .playlist_links
        .iter()
        .find(|link| link.unified_playlist_id == playlist_id)
        .context("unified playlist link not found")?;
    let target_id = link
        .youtube_playlist_id
        .as_deref()
        .context("unified playlist is not linked to YouTube")?;
    let projection = link
        .projection_for(
            crate::state::Provider::YouTubeMusic,
            &account_id,
            account_epoch,
            target_id,
        )
        .context("YouTube projection is detached or not yet scoped")?;
    anyhow::ensure!(
        projection.can_retry(&operation_id.to_string()) && projection.can_retry_intent(intent_key),
        "projection operation is acknowledged or outcome is unknown"
    );
    Ok(())
}

fn acknowledge_projection_operation(
    state: &SharedState,
    playlist_id: &str,
    operation_id: u64,
    intent_key: &str,
) -> Result<()> {
    let (account_id, account_epoch) = current_youtube_projection_scope(state);
    let target_id = state
        .data
        .read()
        .playlist_links
        .iter()
        .find(|link| link.unified_playlist_id == playlist_id)
        .and_then(|link| link.youtube_playlist_id.clone())
        .context("unified playlist link not found")?;
    let mut data = state.data.write();
    data.acknowledge_projection_operation(
        playlist_id,
        crate::state::Provider::YouTubeMusic,
        &account_id,
        account_epoch,
        &target_id,
        &operation_id.to_string(),
    )?;
    data.acknowledge_projection_intent(
        playlist_id,
        crate::state::Provider::YouTubeMusic,
        &account_id,
        account_epoch,
        &target_id,
        intent_key,
    )?;
    Ok(())
}

fn projection_intent_key(state: &SharedState, playlist_id: &str) -> Result<String> {
    let data = state.data.read();
    let playlist = data
        .unified_playlists
        .iter()
        .find(|playlist| playlist.id == playlist_id)
        .context("unified playlist not found")?;
    let target_id = data
        .playlist_links
        .iter()
        .find(|link| link.unified_playlist_id == playlist_id)
        .and_then(|link| link.youtube_playlist_id.as_deref())
        .context("unified playlist is not linked to YouTube")?;
    Ok(format!(
        "sync:{playlist_id}:{target_id}:{}",
        playlist.snapshot_hash()
    ))
}

fn resolve_unified_item_youtube_id(
    item: &crate::state::UnifiedPlaylistItem,
    candidates: &[crate::state::YouTubeTrack],
) -> Result<String> {
    crate::state::best_youtube_match(item, candidates)
        .map(|(track, _)| track.id.clone())
        .context("no confident YouTube match for unified playlist item")
}

pub async fn youtube_rate_song(video_id: &str, liked: bool) -> Result<()> {
    let youtube = youtube::YouTubeMusic::new(config::get_config()).await?;
    youtube.rate_song(video_id, liked).await
}

pub async fn youtube_add_video_to_playlist(playlist_id: &str, video_id: &str) -> Result<()> {
    let youtube = youtube::YouTubeMusic::new(config::get_config()).await?;
    let _ = verified_youtube_append_with(&youtube, playlist_id, video_id).await?;
    Ok(())
}

pub async fn youtube_append_playlist_items(
    playlist_id: &str,
    video_ids: &[String],
) -> Result<Vec<String>> {
    let youtube = youtube::YouTubeMusic::new(config::get_config()).await?;
    let mut failures = Vec::new();
    for video_id in video_ids {
        if let Err(err) = verified_youtube_append_with(&youtube, playlist_id, video_id).await {
            failures.push(format!("{video_id}: {err}"));
        }
    }
    Ok(failures)
}

pub async fn youtube_playlist_tracks(playlist_id: &str) -> Result<Vec<crate::state::YouTubeTrack>> {
    let youtube = youtube::YouTubeMusic::new(config::get_config()).await?;
    youtube
        .context(&crate::state::YouTubeContextId::Playlist(
            playlist_id.to_owned(),
        ))
        .await
        .map(|context| context.tracks)
}

pub async fn youtube_remove_video_from_playlist(
    playlist_id: &str,
    set_video_id: &str,
) -> Result<()> {
    let youtube = youtube::YouTubeMusic::new(config::get_config()).await?;
    let intent = YouTubeMutationIntent::RemoveOccurrence {
        operation_id: super::PlaylistMutationOperationId(rand::rng().random()),
        playlist_id: playlist_id.to_owned(),
        video_id: None,
        set_video_id: Some(set_video_id.to_owned()),
    };
    let _ =
        playlist_mutation_effect(execute_youtube_playlist_mutation_with(&youtube, &intent).await)?;
    Ok(())
}

pub async fn youtube_create_playlist(name: &str, public: bool) -> Result<String> {
    let youtube = youtube::YouTubeMusic::new(config::get_config()).await?;
    let privacy = if public {
        ytmapi_rs::query::playlist::PrivacyStatus::Public
    } else {
        ytmapi_rs::query::playlist::PrivacyStatus::Private
    };
    youtube.create_playlist(name, privacy).await
}

pub async fn youtube_delete_playlist(playlist_id: &str) -> Result<()> {
    let youtube = youtube::YouTubeMusic::new(config::get_config()).await?;
    youtube.delete_playlist(playlist_id).await
}

#[cfg(test)]
mod tests {
    use super::{
        apply_youtube_context_to_page, classify_spotify_status, missing_youtube_ids,
        preserve_youtube_tokens_for_unchanged_prefix, resolve_unified_item_youtube_id,
        retain_youtube_append_token, spotify_playlist_owner_id, user_queue_route, UserQueueRoute,
    };
    use crate::state::{
        MediaId, MediaKind, PageState, Provider, UnifiedPlaylistItem, YouTubeContext,
        YouTubeContextId, YouTubeContextPageUIState, YouTubeTrack,
    };

    fn spotify_item(title: &str, artists: &str) -> UnifiedPlaylistItem {
        UnifiedPlaylistItem {
            media_id: MediaId {
                provider: Provider::Spotify,
                kind: MediaKind::Track,
                raw_id: "spotify-id".to_owned(),
            },
            title: title.to_owned(),
            artists: artists.to_owned(),
            duration_ms: Some(229_000),
            provider_url: None,
            ..UnifiedPlaylistItem::default()
        }
    }

    #[test]
    fn app_owned_playback_uses_the_local_unified_queue() {
        assert_eq!(
            user_queue_route(Some(crate::config::ActiveProvider::Spotify), true),
            UserQueueRoute::LocalUnified(crate::config::ActiveProvider::Spotify)
        );
        assert_eq!(
            user_queue_route(Some(crate::config::ActiveProvider::YouTubeMusic), false),
            UserQueueRoute::LocalUnified(crate::config::ActiveProvider::YouTubeMusic)
        );
        assert_eq!(
            user_queue_route(Some(crate::config::ActiveProvider::Spotify), false),
            UserQueueRoute::NativeSpotify
        );
        assert_eq!(user_queue_route(None, true), UserQueueRoute::NativeSpotify);
    }

    fn youtube_track(id: &str, title: &str, artists: &str) -> YouTubeTrack {
        YouTubeTrack {
            id: id.to_owned(),
            name: title.to_owned(),
            artists: artists.to_owned(),
            album: None,
            duration: "3:49".to_owned(),
            explicit: false,
            thumbnail_url: None,
            is_video: false,
        }
    }

    #[test]
    fn appended_set_video_id_is_attached_to_the_exact_new_duplicate_occurrence() {
        let mut context = YouTubeContext {
            title: "Playlist".to_owned(),
            description: None,
            tracks: vec![
                youtube_track("same", "First", "Artist"),
                youtube_track("other", "Other", "Artist"),
                youtube_track("same", "Second", "Artist"),
            ],
            playlist_set_video_ids: vec![Some("old-token".to_owned())],
            artist: None,
        };
        assert!(retain_youtube_append_token(
            &mut context,
            "same",
            1,
            "new-token".to_owned(),
        ));
        assert_eq!(
            context.playlist_set_video_ids,
            [
                Some("old-token".to_owned()),
                None,
                Some("new-token".to_owned())
            ]
        );
    }

    #[test]
    fn youtube_refresh_preserves_prior_append_tokens_only_for_an_unchanged_prefix() {
        let previous = YouTubeContext {
            title: "Playlist".to_owned(),
            description: None,
            tracks: vec![
                youtube_track("first", "First", "Artist"),
                youtube_track("second", "Second", "Artist"),
            ],
            playlist_set_video_ids: vec![
                Some("first-token".to_owned()),
                Some("second-token".to_owned()),
            ],
            artist: None,
        };
        let mut appended = YouTubeContext {
            title: "Playlist".to_owned(),
            description: None,
            tracks: vec![
                youtube_track("first", "First", "Artist"),
                youtube_track("second", "Second", "Artist"),
                youtube_track("third", "Third", "Artist"),
            ],
            playlist_set_video_ids: vec![None; 3],
            artist: None,
        };
        preserve_youtube_tokens_for_unchanged_prefix(&previous, &mut appended);
        assert_eq!(
            appended.playlist_set_video_ids,
            [
                Some("first-token".to_owned()),
                Some("second-token".to_owned()),
                None
            ]
        );

        let mut reordered = appended.clone();
        reordered.tracks.swap(0, 1);
        reordered.playlist_set_video_ids.fill(None);
        preserve_youtube_tokens_for_unchanged_prefix(&previous, &mut reordered);
        assert_eq!(reordered.playlist_set_video_ids, [None, None, None]);
    }

    #[test]
    fn spotify_status_classification_uses_definitive_provider_evidence() {
        let operation_id = crate::client::PlaylistMutationOperationId(9);
        assert!(matches!(
            classify_spotify_status(operation_id, 409),
            crate::client::PlaylistMutationResult::Conflict {
                current_revision: None
            }
        ));
        assert!(matches!(
            classify_spotify_status(operation_id, 400),
            crate::client::PlaylistMutationResult::Invalidated { .. }
        ));
        assert!(matches!(
            classify_spotify_status(operation_id, 401),
            crate::client::PlaylistMutationResult::Failed {
                category: crate::client::MutationFailureCategory::Authentication,
                ..
            }
        ));
        assert_eq!(
            classify_spotify_status(operation_id, 500),
            crate::client::PlaylistMutationResult::OutcomeUnknown { operation_id }
        );
        assert_eq!(
            classify_spotify_status(operation_id, 429),
            crate::client::PlaylistMutationResult::OutcomeUnknown { operation_id }
        );
    }

    #[test]
    fn linked_projection_accepts_only_a_confident_match() {
        let item = spotify_item("Want Some More", "Nicki Minaj");
        let candidates = vec![
            youtube_track("official", "Want Some More", "Nicki Minaj"),
            youtube_track("other", "Want Some More", "Another Artist"),
        ];
        assert_eq!(
            resolve_unified_item_youtube_id(&item, &candidates).unwrap(),
            "official"
        );
        assert!(resolve_unified_item_youtube_id(
            &item,
            &[youtube_track("wrong", "Want Some More", "Another Artist")]
        )
        .is_err());
    }

    #[test]
    fn linked_projection_appends_only_missing_occurrences_in_order() {
        let desired = vec![
            "a".to_owned(),
            "b".to_owned(),
            "a".to_owned(),
            "c".to_owned(),
        ];
        let existing = vec!["a".to_owned(), "a".to_owned()];
        assert_eq!(
            missing_youtube_ids(&desired, &existing),
            vec!["b".to_owned(), "c".to_owned()]
        );
    }

    #[test]
    fn spotify_playlist_creation_requires_a_loaded_profile() {
        let error = spotify_playlist_owner_id(None).expect_err("missing profile must fail");
        assert!(error.to_string().contains("Spotify profile is unavailable"));
    }

    #[test]
    fn visible_youtube_playlist_alias_is_replaced_after_mutation() {
        let visible_id = YouTubeContextId::Playlist("VLPLplaylist".to_owned());
        let context_id = YouTubeContextId::Playlist("PLplaylist".to_owned());
        let mut page = PageState::YouTubeContext {
            id: visible_id,
            context: Some(YouTubeContext {
                title: "YouTube Playlist".to_owned(),
                description: None,
                tracks: vec![],
                playlist_set_video_ids: Vec::new(),
                artist: None,
            }),
            state: YouTubeContextPageUIState::new(),
        };
        let refreshed = YouTubeContext {
            title: "YouTube Playlist".to_owned(),
            description: None,
            tracks: vec![YouTubeTrack {
                id: "video".to_owned(),
                name: "Added".to_owned(),
                artists: "Artist".to_owned(),
                album: None,
                duration: "3:00".to_owned(),
                explicit: false,
                thumbnail_url: None,
                is_video: false,
            }],
            playlist_set_video_ids: vec![Some("set-video".to_owned())],
            artist: None,
        };

        assert!(apply_youtube_context_to_page(
            &mut page,
            &context_id,
            refreshed
        ));
        assert!(matches!(
            page,
            PageState::YouTubeContext {
                context: Some(YouTubeContext { tracks, .. }),
                state: YouTubeContextPageUIState { status: crate::state::UiViewStatus::Ready, .. },
                ..
            } if tracks.len() == 1
        ));
    }

    #[test]
    fn stale_youtube_context_is_not_replaced_on_another_page() {
        let visible_id = YouTubeContextId::Playlist("visible".to_owned());
        let requested_id = YouTubeContextId::Playlist("other".to_owned());
        let mut page = PageState::YouTubeContext {
            id: visible_id.clone(),
            context: None,
            state: YouTubeContextPageUIState::new(),
        };

        assert!(!apply_youtube_context_to_page(
            &mut page,
            &requested_id,
            YouTubeContext::default()
        ));
        assert!(matches!(
            page,
            PageState::YouTubeContext { context: None, .. }
        ));
    }
}
