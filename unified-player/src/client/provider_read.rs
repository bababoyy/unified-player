use super::provider_metadata::SPOTIFY_API_ENDPOINT;
use std::collections::HashMap;
use std::sync::atomic::{AtomicU8, Ordering};

use anyhow::{Context as _, Result};
use librespot_core::SpotifyUri;
use rspotify::{http::Query, prelude::*};
use serde::Deserialize;
use url::Url;

use crate::{
    config,
    state::{
        listenbrainz_album_key, listenbrainz_recording_key, store_data_into_file_cache, Album,
        Artist, ArtistFocusState, ArtistId, Category, Context, ContextId, ContextPageType,
        ContextPageUIState, Device, FileCacheKey, ListenBrainzAlbumIntent,
        ListenBrainzAlbumResolution, ListenBrainzArtistEnrichment, ListenBrainzCollectionStatus,
        ListenBrainzRecordingIntent, ListenBrainzRecordingResolution, Lyrics, PageState, Playback,
        Playlist, PlaylistFolderItem, PopupState, SharedState, Show, ShowId, Track, TrackId,
        LISTENBRAINZ_RESOLUTION_FAILURE_TTL, LISTENBRAINZ_RESOLUTION_NEGATIVE_TTL,
        TTL_CACHE_DURATION, USER_LIKED_TRACKS_URI, USER_RECENTLY_PLAYED_TRACKS_URI,
        USER_TOP_TRACKS_URI,
    },
};

use super::{playback_coordinator::ActivationPermit, AppClient, ClientRequest, PlayerRequest};

const LRCLIB_USER_AGENT: &str = concat!(
    "unified-player/",
    env!("CARGO_PKG_VERSION"),
    " (https://github.com/bababoyy/unified-player)"
);
/// Keep a stalled third-party lyrics service from pinning the active lyrics request forever.
const LYRICS_HTTP_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(5);
/// Keep a stalled `YouTube` Music library request from leaving the TUI in Loading forever.
const YOUTUBE_LIBRARY_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(20);
/// Current Web API maximum for `GET /artists/{id}/albums`.
const SPOTIFY_ARTIST_ALBUMS_PAGE_LIMIT: usize = 10;
const LISTENBRAINZ_ARTIST_FALLBACK_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(4);
const LISTENBRAINZ_RECORDING_RESOLUTION_TIMEOUT: std::time::Duration =
    std::time::Duration::from_secs(6);
const LISTENBRAINZ_ALBUM_RESOLUTION_TIMEOUT: std::time::Duration =
    std::time::Duration::from_secs(8);

fn provider_label(provider: crate::state::Provider) -> String {
    match provider {
        crate::state::Provider::Spotify => "Spotify".to_owned(),
        crate::state::Provider::YouTubeMusic => "YouTube Music".to_owned(),
    }
}

fn listenbrainz_remote_detail_entries(
    remote: &serde_json::Value,
) -> HashMap<u64, (String, String, String)> {
    use crate::cli::listenbrainz_manifest::DescriptionManifestProjectionStatus;

    let remote_playlist = remote.get("playlist").unwrap_or(remote);
    remote_playlist
        .get("annotation")
        .and_then(serde_json::Value::as_str)
        .and_then(|annotation| {
            crate::cli::listenbrainz_manifest::parse_description_manifest(annotation).ok()
        })
        .map(|manifest| {
            let tracks = remote_playlist
                .get("track")
                .and_then(serde_json::Value::as_array);
            let mut projection_index = 0;
            manifest
                .entries
                .into_iter()
                .map(|entry| {
                    let track = (entry.projection.as_ref().is_some_and(|projection| {
                        projection.status == DescriptionManifestProjectionStatus::Resolved
                    }))
                    .then(|| {
                        let track = tracks.and_then(|tracks| tracks.get(projection_index));
                        projection_index += 1;
                        track
                    })
                    .flatten();
                    (
                        entry.occurrence,
                        (
                            track
                                .and_then(|track| track.get("title"))
                                .and_then(serde_json::Value::as_str)
                                .unwrap_or_default()
                                .to_owned(),
                            track
                                .and_then(|track| track.get("creator"))
                                .and_then(serde_json::Value::as_str)
                                .unwrap_or_default()
                                .to_owned(),
                            entry.provider,
                        ),
                    )
                })
                .collect()
        })
        .unwrap_or_default()
}

fn listenbrainz_sync_preview_model(
    operation_reference: &str,
    playlist: &crate::state::UnifiedPlaylist,
    base: &crate::state::ListenBrainzSyncBase,
    remote: &serde_json::Value,
    preview: &super::listenbrainz_sync::ListenBrainzPullPreview,
) -> crate::state::ListenBrainzSyncPreview {
    use super::listenbrainz_sync::{ChangeClassification, ChangeKind, ConflictKind};
    use crate::state::{
        ListenBrainzSyncConflictKind as UiConflict, ListenBrainzSyncDetailAction as UiAction,
        ListenBrainzSyncDetailRow as UiRow, ListenBrainzSyncSide as UiSide, PlaylistEntryId,
    };

    let remote_entries = listenbrainz_remote_detail_entries(remote);

    let conflict_kind = |kind| match kind {
        ConflictKind::AddAdd => UiConflict::AddAdd,
        ConflictKind::DeleteEdit => UiConflict::DeleteEdit,
        ConflictKind::Mapping => UiConflict::Mapping,
        ConflictKind::Reorder => UiConflict::Reorder,
        ConflictKind::ManifestProjectionDrift => UiConflict::ManifestProjectionDrift,
        ConflictKind::UnlinkedRemoteRow => UiConflict::UnlinkedRemoteRow,
        ConflictKind::StaleBase => UiConflict::StaleBase,
        ConflictKind::DuplicateAmbiguity => UiConflict::DuplicateAmbiguity,
        ConflictKind::Schema => UiConflict::Schema,
    };
    let details = |occurrence: Option<u64>, prefer_remote: bool| {
        let local = occurrence.and_then(|occurrence| {
            playlist
                .items
                .iter()
                .find(|item| item.entry_id == PlaylistEntryId(occurrence))
        });
        let remote = occurrence.and_then(|occurrence| remote_entries.get(&occurrence));
        let base_provider = occurrence
            .and_then(|occurrence| {
                base.entries
                    .iter()
                    .find(|entry| entry.occurrence == PlaylistEntryId(occurrence))
            })
            .map(|entry| entry.media_id.provider);
        let local_provider = local.map(|item| item.media_id.provider).or(base_provider);
        let provider = if prefer_remote {
            remote
                .map(|entry| entry.2.clone())
                .or_else(|| local_provider.map(provider_label))
                .unwrap_or_else(|| "Unknown".to_owned())
        } else {
            local_provider
                .map(provider_label)
                .or_else(|| remote.map(|entry| entry.2.clone()))
                .unwrap_or_else(|| "Unknown".to_owned())
        };
        let remote_title = || {
            remote
                .map(|entry| entry.0.clone())
                .filter(|value| !value.is_empty())
        };
        let remote_artist = || {
            remote
                .map(|entry| entry.1.clone())
                .filter(|value| !value.is_empty())
        };
        let local_title = || {
            local
                .map(|item| item.title.clone())
                .filter(|value| !value.is_empty())
        };
        let local_artist = || {
            local
                .map(|item| item.artists.clone())
                .filter(|value| !value.is_empty())
        };
        (
            if prefer_remote {
                remote_title().or_else(local_title)
            } else {
                local_title().or_else(remote_title)
            }
            .unwrap_or_else(|| "Unavailable".to_owned()),
            if prefer_remote {
                remote_artist().or_else(local_artist)
            } else {
                local_artist().or_else(remote_artist)
            }
            .unwrap_or_default(),
            provider,
        )
    };

    let mut rows = preview
        .plan
        .changes
        .iter()
        .map(|change| {
            let (title, artist, provider) = details(
                change.occurrence,
                change.classification != ChangeClassification::LocalOnly,
            );
            let conflict = preview
                .plan
                .conflicts
                .iter()
                .find(|conflict| conflict.occurrence == change.occurrence)
                .map(|conflict| conflict_kind(conflict.kind));
            UiRow {
                occurrence: change.occurrence.map(PlaylistEntryId),
                side: match change.classification {
                    ChangeClassification::LocalOnly => UiSide::Local,
                    ChangeClassification::RemoteOnly => UiSide::ListenBrainz,
                    ChangeClassification::SameChange => UiSide::Both,
                },
                action: match change.kind {
                    ChangeKind::Added => UiAction::Added,
                    ChangeKind::Removed => UiAction::Removed,
                    ChangeKind::IdentityChanged => UiAction::IdentityChanged,
                    ChangeKind::PlaylistRenamed => UiAction::PlaylistRenamed,
                    ChangeKind::Reordered => UiAction::Reordered,
                },
                title,
                artist,
                provider,
                conflict,
            }
        })
        .collect::<Vec<_>>();
    rows.extend(
        preview
            .plan
            .conflicts
            .iter()
            .filter(|conflict| {
                !preview
                    .plan
                    .changes
                    .iter()
                    .any(|change| change.occurrence == conflict.occurrence)
            })
            .map(|conflict| {
                let (title, artist, provider) = details(conflict.occurrence, false);
                UiRow {
                    occurrence: conflict.occurrence.map(PlaylistEntryId),
                    side: UiSide::Both,
                    action: UiAction::Conflict,
                    title,
                    artist,
                    provider,
                    conflict: Some(conflict_kind(conflict.kind)),
                }
            }),
    );
    crate::state::ListenBrainzSyncPreview {
        playlist_id: playlist.id.clone(),
        operation_reference: operation_reference.to_owned(),
        rows,
        conflicts: preview
            .plan
            .conflicts
            .iter()
            .map(|conflict| conflict_kind(conflict.kind))
            .collect(),
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[repr(u8)]
enum ListenBrainzResolutionPhase {
    ArtistMapping = 0,
    ArtistPopularity = 1,
    ReleaseGroupPopularity = 2,
    RecordingRelation = 3,
    SpotifyTrackFetch = 4,
    ReleaseRelation = 5,
    SpotifyAlbumFetch = 6,
}

impl ListenBrainzResolutionPhase {
    const ALL: [Self; 7] = [
        Self::ArtistMapping,
        Self::ArtistPopularity,
        Self::ReleaseGroupPopularity,
        Self::RecordingRelation,
        Self::SpotifyTrackFetch,
        Self::ReleaseRelation,
        Self::SpotifyAlbumFetch,
    ];

    const fn as_str(self) -> &'static str {
        match self {
            Self::ArtistMapping => "artist_mapping",
            Self::ArtistPopularity => "artist_popularity",
            Self::ReleaseGroupPopularity => "release_group_popularity",
            Self::RecordingRelation => "recording_relation",
            Self::SpotifyTrackFetch => "spotify_track_fetch",
            Self::ReleaseRelation => "release_relation",
            Self::SpotifyAlbumFetch => "spotify_album_fetch",
        }
    }

    fn from_marker(marker: u8) -> Self {
        Self::ALL
            .into_iter()
            .find(|phase| *phase as u8 == marker)
            .unwrap_or(Self::ArtistMapping)
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum ListenBrainzResolutionStatus {
    Matched,
    NoMatch,
    Available,
    Empty,
    NoRelation,
    Resolved,
    Unavailable,
    Timeout,
    Cancelled,
}

impl ListenBrainzResolutionStatus {
    #[cfg(test)]
    const ALL: [Self; 9] = [
        Self::Matched,
        Self::NoMatch,
        Self::Available,
        Self::Empty,
        Self::NoRelation,
        Self::Resolved,
        Self::Unavailable,
        Self::Timeout,
        Self::Cancelled,
    ];

    const fn as_str(self) -> &'static str {
        match self {
            Self::Matched => "matched",
            Self::NoMatch => "no_match",
            Self::Available => "available",
            Self::Empty => "empty",
            Self::NoRelation => "no_relation",
            Self::Resolved => "resolved",
            Self::Unavailable => "unavailable",
            Self::Timeout => "timeout",
            Self::Cancelled => "cancelled",
        }
    }
}

type ListenBrainzPhaseError = (
    ListenBrainzResolutionPhase,
    std::time::Duration,
    anyhow::Error,
);

fn record_listenbrainz_resolution_stage(
    phase: ListenBrainzResolutionPhase,
    status: ListenBrainzResolutionStatus,
    outcome: crate::observability::OperationOutcome,
    elapsed: Option<std::time::Duration>,
    error_category: Option<crate::observability::ErrorCategory>,
) {
    crate::observability::operation_stage_detail(
        crate::observability::Component::Spotify,
        "listenbrainz_resolution",
        elapsed,
        Some(outcome),
        Some(phase.as_str()),
        Some(status.as_str()),
        error_category.map(crate::observability::ErrorCategory::as_str),
        None,
    );
}

fn lyrics_request(request: reqwest::RequestBuilder) -> reqwest::RequestBuilder {
    request.timeout(LYRICS_HTTP_TIMEOUT)
}

#[derive(Debug, Deserialize)]
struct LrcLibLyricsResponse {
    #[serde(rename = "plainLyrics")]
    plain_lyrics: Option<String>,
    #[serde(rename = "syncedLyrics")]
    synced_lyrics: Option<String>,
}

#[derive(Debug, Deserialize)]
struct LyricsOvhResponse {
    lyrics: Option<String>,
}

fn parse_youtube_duration_seconds(duration: &str) -> Option<u64> {
    duration
        .split(':')
        .try_fold(0_u64, |total, part| {
            total
                .checked_mul(60)?
                .checked_add(part.parse::<u64>().ok()?)
        })
        .filter(|seconds| *seconds > 0)
}

fn lyrics_from_lrclib(response: LrcLibLyricsResponse) -> Option<Lyrics> {
    response
        .synced_lyrics
        .as_deref()
        .filter(|lyrics| !lyrics.trim().is_empty())
        .and_then(|lyrics| Lyrics::from_lrc(lyrics, "LRCLIB"))
        .or_else(|| {
            response
                .plain_lyrics
                .as_deref()
                .filter(|lyrics| !lyrics.trim().is_empty())
                .and_then(|lyrics| Lyrics::from_plain(lyrics, "LRCLIB"))
        })
}

fn lyrics_from_lyrics_ovh(response: LyricsOvhResponse) -> Option<Lyrics> {
    response
        .lyrics
        .as_deref()
        .filter(|lyrics| !lyrics.trim().is_empty())
        .and_then(|lyrics| Lyrics::from_plain(lyrics, "Lyrics.ovh"))
}

fn lyrics_ovh_url(title: &str, artist: &str) -> Result<Url> {
    let mut url = Url::parse("https://api.lyrics.ovh/v1")?;
    url.path_segments_mut()
        .map_err(|()| anyhow::anyhow!("lyrics.ovh URL cannot accept path segments"))?
        .push(artist)
        .push(title);
    Ok(url)
}

/// Accept Spotify identifiers as well as a pasted profile URL.
/// Spotify does not provide display-name user search, so this remains an
/// identifier lookup rather than a free-text search.
fn spotify_user_lookup_id(query: &str) -> String {
    let trimmed = query.trim();
    let candidate = trimmed
        .strip_prefix("https://open.spotify.com/user/")
        .or_else(|| trimmed.strip_prefix("http://open.spotify.com/user/"))
        .or_else(|| trimmed.strip_prefix("open.spotify.com/user/"))
        .unwrap_or(trimmed);
    let candidate = candidate
        .split(['?', '#'])
        .next()
        .unwrap_or_default()
        .trim_end_matches('/');
    if candidate.is_empty() {
        trimmed.to_owned()
    } else {
        candidate.to_owned()
    }
}

fn is_explicit_spotify_user_identifier(query: &str) -> bool {
    let trimmed = query.trim();
    trimmed.eq_ignore_ascii_case("me")
        || trimmed.starts_with("spotify:user:")
        || trimmed.starts_with("https://open.spotify.com/user/")
        || trimmed.starts_with("http://open.spotify.com/user/")
        || trimmed.starts_with("open.spotify.com/user/")
}

fn replace_listenbrainz_resolution_if_current(
    state: &SharedState,
    recording_mbid: &str,
    request_id: u64,
    replacement: Option<ListenBrainzRecordingResolution>,
) -> bool {
    let key = listenbrainz_recording_key(recording_mbid);
    let mut data = state.data.write();
    let is_current = matches!(
        data.caches.listenbrainz_recordings.get(&key),
        Some(ListenBrainzRecordingResolution::Resolving {
            request_id: current,
        }) if *current == request_id
    );
    if !is_current {
        return false;
    }
    if let Some(replacement) = replacement {
        let ttl = match &replacement {
            ListenBrainzRecordingResolution::NoSpotifyRelation => {
                LISTENBRAINZ_RESOLUTION_NEGATIVE_TTL
            }
            ListenBrainzRecordingResolution::Unavailable => LISTENBRAINZ_RESOLUTION_FAILURE_TTL,
            ListenBrainzRecordingResolution::Resolved(_)
            | ListenBrainzRecordingResolution::Resolving { .. } => *TTL_CACHE_DURATION,
        };
        data.caches
            .listenbrainz_recordings
            .insert(key, replacement, ttl);
    } else {
        data.caches.listenbrainz_recordings.remove(&key);
    }
    true
}

fn replace_listenbrainz_album_resolution_if_current(
    state: &SharedState,
    release_group_mbid: &str,
    request_id: u64,
    replacement: Option<ListenBrainzAlbumResolution>,
) -> bool {
    let key = listenbrainz_album_key(release_group_mbid);
    let mut data = state.data.write();
    let is_current = matches!(
        data.caches.listenbrainz_albums.get(&key),
        Some(ListenBrainzAlbumResolution::Resolving {
            request_id: current,
        }) if *current == request_id
    );
    if !is_current {
        return false;
    }
    if let Some(replacement) = replacement {
        let ttl = match &replacement {
            ListenBrainzAlbumResolution::NoSpotifyRelation => LISTENBRAINZ_RESOLUTION_NEGATIVE_TTL,
            ListenBrainzAlbumResolution::Unavailable => LISTENBRAINZ_RESOLUTION_FAILURE_TTL,
            ListenBrainzAlbumResolution::Resolved(_)
            | ListenBrainzAlbumResolution::Resolving { .. } => *TTL_CACHE_DURATION,
        };
        data.caches
            .listenbrainz_albums
            .insert(key, replacement, ttl);
    } else {
        data.caches.listenbrainz_albums.remove(&key);
    }
    true
}

fn listenbrainz_release_group_ids_for_context(
    state: &SharedState,
    context_uri: &str,
) -> Vec<String> {
    let data = state.data.read();
    let Some(Context::Artist {
        albums,
        listenbrainz:
            ListenBrainzArtistEnrichment::Available {
                release_groups_status: ListenBrainzCollectionStatus::Available,
                release_groups,
                ..
            },
        ..
    }) = data.caches.context.get(context_uri)
    else {
        return Vec::new();
    };
    if !albums.is_empty() {
        return Vec::new();
    }
    release_groups
        .iter()
        .map(|release| release.release_group_mbid.clone())
        .collect()
}

fn consume_current_listenbrainz_album_intent(
    state: &SharedState,
    request_id: u64,
    context_uri: &str,
    release_group_mbid: &str,
    expected_intent: ListenBrainzAlbumIntent,
    on_current: impl FnOnce(&mut crate::state::UIState),
) -> bool {
    let release_group_ids = listenbrainz_release_group_ids_for_context(state, context_uri);
    let mut ui = state.ui.lock();
    let PageState::Context {
        id: Some(context_id),
        state:
            Some(ContextPageUIState::Artist {
                album_table,
                listenbrainz_album_pending,
                focus: ArtistFocusState::Albums,
                ..
            }),
        ..
    } = ui.current_page_mut()
    else {
        return false;
    };
    let Some(pending) = listenbrainz_album_pending.as_ref() else {
        return false;
    };
    let selected_release_group = album_table
        .selected()
        .and_then(|index| release_group_ids.get(index))
        .map(String::as_str);
    if !listenbrainz_album_pending_matches(
        pending,
        request_id,
        context_uri,
        release_group_mbid,
        expected_intent,
        &context_id.uri(),
        selected_release_group,
    ) {
        return false;
    }
    *listenbrainz_album_pending = None;
    on_current(&mut ui);
    true
}

fn listenbrainz_album_pending_matches(
    pending: &crate::state::ListenBrainzAlbumPendingIntent,
    request_id: u64,
    context_uri: &str,
    release_group_mbid: &str,
    expected_intent: ListenBrainzAlbumIntent,
    current_context_uri: &str,
    selected_release_group_mbid: Option<&str>,
) -> bool {
    pending.request_id == request_id
        && pending.context_uri == context_uri
        && pending.release_group_mbid == release_group_mbid
        && pending.intent == expected_intent
        && current_context_uri == context_uri
        && selected_release_group_mbid == Some(release_group_mbid)
}

fn clear_current_listenbrainz_album_intent(
    state: &SharedState,
    context_uri: &str,
    request_id: u64,
) {
    let mut ui = state.ui.lock();
    let PageState::Context {
        state:
            Some(ContextPageUIState::Artist {
                listenbrainz_album_pending,
                ..
            }),
        ..
    } = ui.current_page_mut()
    else {
        return;
    };
    if listenbrainz_album_pending.as_ref().is_some_and(|pending| {
        pending.request_id == request_id && pending.context_uri == context_uri
    }) {
        *listenbrainz_album_pending = None;
    }
}

pub(super) fn terminalize_listenbrainz_album_resolution_if_current(
    state: &SharedState,
    context_uri: &str,
    release_group_mbid: &str,
    request_id: u64,
) {
    replace_listenbrainz_album_resolution_if_current(
        state,
        release_group_mbid,
        request_id,
        Some(ListenBrainzAlbumResolution::Unavailable),
    );
    clear_current_listenbrainz_album_intent(state, context_uri, request_id);
}

fn direct_spotify_album_id(
    mut relations: Vec<super::listenbrainz::SpotifyAlbumRelation>,
) -> Option<String> {
    relations.sort_by(|left, right| {
        left.spotify_album_id
            .cmp(&right.spotify_album_id)
            .then_with(|| left.release_mbid.cmp(&right.release_mbid))
    });
    relations
        .into_iter()
        .next()
        .map(|relation| relation.spotify_album_id)
}

fn complete_listenbrainz_artist_enrichment_if_current(
    context: &mut Context,
    request_id: u64,
    replacement: ListenBrainzArtistEnrichment,
) -> bool {
    let Context::Artist { listenbrainz, .. } = context else {
        return false;
    };
    if !matches!(
        listenbrainz,
        ListenBrainzArtistEnrichment::Loading {
            request_id: current,
        } if *current == request_id
    ) {
        return false;
    }
    *listenbrainz = replacement;
    true
}

fn replace_cached_listenbrainz_artist_enrichment_if_current(
    state: &SharedState,
    context_uri: &str,
    request_id: u64,
    replacement: ListenBrainzArtistEnrichment,
) -> bool {
    let mut data = state.data.write();
    data.caches
        .context
        .get_mut(context_uri)
        .is_some_and(|context| {
            complete_listenbrainz_artist_enrichment_if_current(context, request_id, replacement)
        })
}

pub(super) fn terminalize_listenbrainz_artist_enrichment_if_current(
    state: &SharedState,
    context_uri: &str,
    request_id: u64,
) {
    replace_cached_listenbrainz_artist_enrichment_if_current(
        state,
        context_uri,
        request_id,
        ListenBrainzArtistEnrichment::Unavailable,
    );
}

fn listenbrainz_recording_ids_for_context(state: &SharedState, context_uri: &str) -> Vec<String> {
    let data = state.data.read();
    let Some(Context::Artist {
        listenbrainz:
            crate::state::ListenBrainzArtistEnrichment::Available {
                recordings_status: ListenBrainzCollectionStatus::Available,
                recordings,
                ..
            },
        ..
    }) = data.caches.context.get(context_uri)
    else {
        return Vec::new();
    };
    recordings
        .iter()
        .map(|recording| recording.recording_mbid.clone())
        .collect()
}

fn consume_current_listenbrainz_intent(
    state: &SharedState,
    request_id: u64,
    context_uri: &str,
    recording_mbid: &str,
    expected_intent: ListenBrainzRecordingIntent,
    on_current: impl FnOnce(&mut crate::state::UIState),
) -> bool {
    let recording_ids = listenbrainz_recording_ids_for_context(state, context_uri);
    let mut ui = state.ui.lock();
    let PageState::Context {
        id: Some(context_id),
        state:
            Some(ContextPageUIState::Artist {
                top_track_table,
                listenbrainz_pending,
                focus: ArtistFocusState::TopTracks,
                ..
            }),
        ..
    } = ui.current_page_mut()
    else {
        return false;
    };
    let Some(pending) = listenbrainz_pending.as_ref() else {
        return false;
    };
    let selected_recording = top_track_table
        .selected()
        .and_then(|index| recording_ids.get(index))
        .map(String::as_str);
    if !listenbrainz_pending_matches(
        pending,
        request_id,
        context_uri,
        recording_mbid,
        expected_intent,
        &context_id.uri(),
        selected_recording,
    ) {
        return false;
    }
    *listenbrainz_pending = None;
    on_current(&mut ui);
    true
}

fn listenbrainz_pending_matches(
    pending: &crate::state::ListenBrainzPendingIntent,
    request_id: u64,
    context_uri: &str,
    recording_mbid: &str,
    expected_intent: ListenBrainzRecordingIntent,
    current_context_uri: &str,
    selected_recording_mbid: Option<&str>,
) -> bool {
    pending.request_id == request_id
        && pending.context_uri == context_uri
        && pending.recording_mbid == recording_mbid
        && pending.intent == expected_intent
        && current_context_uri == context_uri
        && selected_recording_mbid == Some(recording_mbid)
}

fn clear_current_listenbrainz_intent(state: &SharedState, request_id: u64) {
    let mut ui = state.ui.lock();
    let PageState::Context {
        state:
            Some(ContextPageUIState::Artist {
                listenbrainz_pending,
                ..
            }),
        ..
    } = ui.current_page_mut()
    else {
        return;
    };
    if listenbrainz_pending
        .as_ref()
        .is_some_and(|pending| pending.request_id == request_id)
    {
        *listenbrainz_pending = None;
    }
}

/// Classify a failed Home read and record it with an actionable category.
fn home_feed_failure(error: &anyhow::Error) -> crate::state::HomeFeedFailure {
    use crate::observability::ErrorCategory;
    use crate::state::HomeFeedFailure;
    let status = super::provider_metadata::spotify_error_status(error);
    let (failure, category) = match status.map(|status| status.as_u16()) {
        Some(429) => (HomeFeedFailure::RequestLimit, ErrorCategory::RateLimited),
        Some(401 | 403) => (HomeFeedFailure::AccessDenied, ErrorCategory::Authentication),
        _ => (HomeFeedFailure::Unavailable, ErrorCategory::Unavailable),
    };
    crate::observability::log_safe_error!(
        warn,
        crate::observability::DiagnosticCode::HOME_FEED_FAILED,
        category,
        error,
        "A Spotify Home shelf could not be loaded"
    );
    failure
}

impl AppClient {
    /// Get lyrics of a given track, return None if no lyrics is available
    pub async fn lyrics(&self, track_id: TrackId<'static>) -> Result<Option<Lyrics>> {
        let session = self.spotify.session().await;
        let uri = SpotifyUri::from_uri(&track_id.uri())?;
        match uri {
            SpotifyUri::Track { id } => {
                match librespot_metadata::Lyrics::get(&session, &id).await {
                    Ok(lyrics) => Ok(Some(lyrics.into())),
                    Err(err) => {
                        if err.to_string().to_lowercase().contains("not found") {
                            Ok(None)
                        } else {
                            Err(err.into())
                        }
                    }
                }
            }
            _ => Ok(None),
        }
    }

    pub async fn external_lyrics(
        &self,
        title: &str,
        artist: &str,
        album: Option<&str>,
        duration_seconds: Option<u64>,
        video_id: Option<&str>,
    ) -> Result<Option<Lyrics>> {
        self.external_lyrics_with_provider(title, artist, album, duration_seconds, video_id, None)
            .await
    }

    pub async fn external_lyrics_from_provider(
        &self,
        title: &str,
        artist: &str,
        album: Option<&str>,
        duration_seconds: Option<u64>,
        video_id: Option<&str>,
        provider: &str,
    ) -> Result<Option<Lyrics>> {
        self.external_lyrics_with_provider(
            title,
            artist,
            album,
            duration_seconds,
            video_id,
            Some(provider),
        )
        .await
    }

    async fn external_lyrics_with_provider(
        &self,
        title: &str,
        artist: &str,
        album: Option<&str>,
        duration_seconds: Option<u64>,
        video_id: Option<&str>,
        provider: Option<&str>,
    ) -> Result<Option<Lyrics>> {
        let providers = config::get_config().app_config.lyrics.clone();
        if let Some(provider) = provider {
            if !providers.provider_enabled(provider)
                || !matches!(
                    provider,
                    "simpmusic" | "lrclib" | "lyricsovh" | "musixmatch"
                )
            {
                return Ok(None);
            }
        }
        if provider.is_none_or(|provider| provider == "simpmusic")
            && providers.provider_enabled("simpmusic")
        {
            if let Some(video_id) = video_id {
                let response = lyrics_request(
                    self.http
                        .get(format!("https://api-lyrics.simpmusic.org/v1/{video_id}")),
                )
                .send()
                .await?;
                if response.status().is_success() {
                    let body: serde_json::Value = response.json().await?;
                    if let Some(item) = body
                        .get("data")
                        .and_then(|data| data.as_array())
                        .and_then(|items| items.first())
                    {
                        if let Some(rich_sync) =
                            item.get("richSyncLyrics").and_then(|value| value.as_str())
                        {
                            if let Some(lyrics) =
                                Lyrics::from_simp_music_rich_sync(rich_sync, "SimpMusic")
                            {
                                return Ok(Some(lyrics));
                            }
                        }
                        if let Some(synced) =
                            item.get("syncedLyrics").and_then(|value| value.as_str())
                        {
                            if let Some(lyrics) = Lyrics::from_lrc(synced, "SimpMusic") {
                                return Ok(Some(lyrics));
                            }
                        }
                        if let Some(plain) = item.get("plainLyric").and_then(|value| value.as_str())
                        {
                            if let Some(lyrics) = Lyrics::from_plain(plain, "SimpMusic") {
                                return Ok(Some(lyrics));
                            }
                        }
                    }
                }
            }
        }
        if provider == Some("simpmusic") {
            return Ok(None);
        }

        if provider.is_none_or(|provider| provider == "lrclib")
            && providers.provider_enabled("lrclib")
        {
            if let Ok(Some(lyrics)) = self
                .lrclib_lyrics(title, artist, album, duration_seconds)
                .await
            {
                return Ok(Some(lyrics));
            }
        }
        if provider == Some("lrclib") {
            return Ok(None);
        }

        if provider.is_none_or(|provider| provider == "lyricsovh")
            && providers.provider_enabled("lyricsovh")
        {
            if let Ok(Some(lyrics)) = self.lyrics_ovh_lyrics(title, artist).await {
                return Ok(Some(lyrics));
            }
        }
        if provider == Some("lyricsovh") {
            return Ok(None);
        }

        if !providers.provider_enabled("musixmatch") {
            return Ok(None);
        }

        let token_response = lyrics_request(
            self.http
                .get("https://apic-desktop.musixmatch.com/ws/1.1/token.get")
                .query(&[("app_id", "web-desktop-app-v1.0"), ("guid", "default")])
                .header("User-Agent", "Mozilla/5.0")
                .header("Cookie", "x-mxm-token-guid="),
        )
        .send()
        .await?;
        let token_body: serde_json::Value = token_response.json().await?;
        let Some(token) = token_body
            .pointer("/message/body/user_token")
            .and_then(|value| value.as_str())
        else {
            return Ok(None);
        };
        let search_body: serde_json::Value = lyrics_request(
            self.http
                .get("https://apic-desktop.musixmatch.com/ws/1.1/track.search")
                .query(&[
                    ("app_id", "web-desktop-app-v1.0"),
                    ("usertoken", token),
                    ("q_track", title),
                    ("q_artist", artist),
                    ("s_track_rating", "desc"),
                    ("page_size", "5"),
                ])
                .header("User-Agent", "Mozilla/5.0")
                .header("Cookie", "x-mxm-token-guid="),
        )
        .send()
        .await?
        .json()
        .await?;
        let Some(track_id) = search_body
            .pointer("/message/body/track_list/0/track/track_id")
            .and_then(serde_json::Value::as_i64)
        else {
            return Ok(None);
        };
        let subtitle_body: serde_json::Value = lyrics_request(
            self.http
                .get("https://apic-desktop.musixmatch.com/ws/1.1/track.subtitle.get")
                .query(&[
                    ("app_id", "web-desktop-app-v1.0"),
                    ("usertoken", token),
                    ("track_id", &track_id.to_string()),
                    ("subtitle_format", "lrc"),
                ])
                .header("User-Agent", "Mozilla/5.0")
                .header("Cookie", "x-mxm-token-guid="),
        )
        .send()
        .await?
        .json()
        .await?;
        if let Some(lrc) = subtitle_body
            .pointer("/message/body/subtitle/subtitle_body")
            .and_then(|value| value.as_str())
        {
            return Ok(Lyrics::from_lrc(lrc, "Musixmatch"));
        }

        Ok(None)
    }

    async fn lrclib_lyrics(
        &self,
        title: &str,
        artist: &str,
        album: Option<&str>,
        duration_seconds: Option<u64>,
    ) -> Result<Option<Lyrics>> {
        let (Some(album), Some(duration_seconds)) = (
            album.filter(|album| !album.trim().is_empty()),
            duration_seconds,
        ) else {
            return Ok(None);
        };
        let duration = duration_seconds.to_string();
        let response = lyrics_request(
            self.http
                .get("https://lrclib.net/api/get")
                .query(&[
                    ("track_name", title),
                    ("artist_name", artist),
                    ("album_name", album),
                    ("duration", duration.as_str()),
                ])
                .header("User-Agent", LRCLIB_USER_AGENT),
        )
        .send()
        .await?;
        if !response.status().is_success() {
            return Ok(None);
        }
        let lyrics = response.json::<LrcLibLyricsResponse>().await?;
        Ok(lyrics_from_lrclib(lyrics))
    }

    async fn lyrics_ovh_lyrics(&self, title: &str, artist: &str) -> Result<Option<Lyrics>> {
        let response = lyrics_request(self.http.get(lyrics_ovh_url(title, artist)?))
            .send()
            .await?;
        if !response.status().is_success() {
            return Ok(None);
        }
        let lyrics = response.json::<LyricsOvhResponse>().await?;
        Ok(lyrics_from_lyrics_ovh(lyrics))
    }

    /// Get user available devices
    pub async fn available_devices(&self) -> Result<Vec<rspotify::model::Device>> {
        Ok(self.spotify_api().device().await?)
    }

    pub fn update_playback(&self, state: &SharedState) {
        // Spotify playback changes are eventually consistent. Keep one
        // delayed reconciliation for commands and incomplete player events;
        // matching Playing/Paused events are applied locally without a REST
        // request by the integrated event path.
        let client = self.clone();
        let state = state.clone();
        let generation = self.playback.begin_spotify_update();
        tokio::task::spawn(async move {
            let delay = std::time::Duration::from_secs(1);
            tokio::time::sleep(delay).await;
            if !client.playback.spotify_update_is_current(generation)
                || client.playback.stable_active_provider()
                    == Some(config::ActiveProvider::YouTubeMusic)
            {
                return;
            }
            if let Err(err) = client.retrieve_current_playback(&state, false).await {
                crate::observability::log_safe_error!(
                    error,
                    crate::observability::DiagnosticCode::SPOTIFY_PLAYBACK_REFRESH_FAILED,
                    crate::observability::ErrorCategory::Unavailable,
                    &err,
                    "Failed to update Spotify playback state"
                );
            }
        });
    }

    #[cfg(feature = "streaming")]
    pub(crate) fn cancel_spotify_playback_update(&self) {
        self.playback.cancel_spotify_update();
    }

    /// Get Spotify's available browse categories
    pub fn browse_categories() -> Vec<Category> {
        tracing::warn!("Spotify removed browse category endpoints in February 2026.");
        Vec::new()
    }

    /// Get Spotify's available browse playlists of a given category
    pub fn browse_category_playlists(_category_id: &str) -> Vec<Playlist> {
        tracing::warn!("Spotify removed browse category playlist endpoints in February 2026");
        Vec::new()
    }

    /// Find an available device. If found, return the device's ID.
    pub(super) async fn find_available_device(&self) -> Result<Option<String>> {
        let devices = self.available_devices().await?;

        // if there is an active device, return it
        if let Some(d) = devices.iter().find(|d| d.is_active) {
            return Ok(d.id.clone());
        }

        #[allow(unused_mut)]
        let mut devices = devices
            .into_iter()
            .filter_map(Device::try_from_device)
            .collect::<Vec<_>>();

        #[cfg(feature = "streaming")]
        self.ensure_integrated_device(&mut devices).await;

        tracing::info!(
            available_device_count = devices.len(),
            "No active Spotify device found"
        );

        if devices.is_empty() {
            return Ok(None);
        }

        // Prioritize the integrated device; otherwise, use the first available device.
        let id = devices
            .iter()
            .position(|d| d.is_integrated)
            .unwrap_or_default();

        Ok(Some(devices.remove(id).id))
    }

    /// Advertise this instance's integrated device only while its streaming connection exists.
    ///
    /// The integrated device may not show up in the device list returned by the Spotify API because
    /// 1. The device is just initialized and hasn't been registered in Spotify server.
    ///    Related issue/discussion: <https://github.com/aome510/spotify-player/issues/79>
    /// 2. The device list is empty. This might be because user doesn't specify their own client ID.
    ///    By default, the application uses Spotify web app's client ID, which doesn't have
    ///    access to user's active devices.
    #[cfg(feature = "streaming")]
    pub(super) async fn ensure_integrated_device(&self, devices: &mut Vec<Device>) {
        let Some(session_device_id) = self.connected_integrated_spotify_device_id().await else {
            return;
        };

        // Mark the integrated device if it's already in the list; otherwise, add it, so it's
        // always present without duplicating an entry the API already returned.
        match devices.iter_mut().find(|d| d.id == session_device_id) {
            Some(device) => device.is_integrated = true,
            None => devices.insert(
                0,
                Device {
                    id: session_device_id,
                    name: config::get_config().app_config.device.name.clone(),
                    is_integrated: true,
                },
            ),
        }
    }

    /// Get the saved (liked) tracks of the current user
    pub async fn current_user_saved_tracks(&self) -> Result<Vec<Track>> {
        let tracks = self
            .all_paging_items::<rspotify::model::SavedTrack>(
                &format!("{SPOTIFY_API_ENDPOINT}/me/tracks"),
                0, // we don't know the total number of saved tracks beforehand
            )
            .await?;

        Ok(tracks
            .into_iter()
            .filter_map(|t| Track::try_from_full_track(t.track))
            .collect())
    }

    /// Get the recently played tracks of the current user. Without
    /// `all_pages`, only the latest 50 plays are read, in one request.
    pub async fn current_user_recently_played_tracks(&self, all_pages: bool) -> Result<Vec<Track>> {
        let first_page = self
            .spotify_api()
            .current_user_recently_played(Some(50), None)
            .await?;

        let play_histories = if all_pages {
            self.all_cursor_based_paging_items(first_page).await?
        } else {
            first_page.items
        };

        // de-duplicate the tracks returned from the recently-played API
        let mut tracks = Vec::<Track>::new();
        for history in play_histories {
            if !tracks.iter().any(|t| t.name == history.track.name) {
                if let Some(track) = Track::try_from_full_track(history.track) {
                    tracks.push(track);
                }
            }
        }
        Ok(tracks)
    }

    /// Get the top tracks of the current user
    pub async fn current_user_top_tracks(&self) -> Result<Vec<Track>> {
        let tracks = self
            .all_paging_items::<rspotify::model::FullTrack>(
                &format!("{SPOTIFY_API_ENDPOINT}/me/top/tracks"),
                0, // we don't know the total number of top tracks beforehand
            )
            .await?;

        Ok(tracks
            .into_iter()
            .filter_map(Track::try_from_full_track)
            .collect())
    }

    /// Get the current user's first `limit` top tracks in one request.
    pub async fn current_user_top_tracks_page(&self, limit: usize) -> Result<Vec<Track>> {
        let limit = limit.to_string();
        let page = self
            .http_get::<rspotify::model::Page<rspotify::model::FullTrack>>(
                &format!("{SPOTIFY_API_ENDPOINT}/me/top/tracks"),
                &Query::from([("limit", limit.as_str())]),
            )
            .await?;

        Ok(page
            .items
            .into_iter()
            .filter_map(Track::try_from_full_track)
            .collect())
    }

    /// Get all playlists of the current user
    pub async fn current_user_playlists(&self) -> Result<Vec<Playlist>> {
        let playlists = self
            .all_paging_items::<rspotify::model::SimplifiedPlaylist>(
                &format!("{SPOTIFY_API_ENDPOINT}/me/playlists"),
                0, // we don't know the total number of playlists beforehand
            )
            .await?;

        Ok(playlists
            .into_iter()
            .map(std::convert::Into::into)
            .collect())
    }

    /// Get all followed artists of the current user
    pub async fn current_user_followed_artists(&self) -> Result<Vec<Artist>> {
        let first_page = self
            .spotify_api()
            .current_user_followed_artists(None, None)
            .await?;

        // followed artists pagination is handled different from
        // other paginations. The endpoint uses cursor-based pagination.
        let mut artists = first_page.items;
        let mut maybe_next = first_page.next;
        while let Some(url) = maybe_next {
            let mut next_page = self
                .http_get::<rspotify::model::CursorPageFullArtists>(&url, &Query::new())
                .await?
                .artists;
            artists.append(&mut next_page.items);
            maybe_next = next_page.next;
        }

        // converts `rspotify::model::FullArtist` into `state::Artist`
        Ok(artists.into_iter().map(std::convert::Into::into).collect())
    }

    /// Get all saved albums of the current user
    pub async fn current_user_saved_albums(&self) -> Result<Vec<Album>> {
        let albums = self
            .all_paging_items::<rspotify::model::SavedAlbum>(
                &format!("{SPOTIFY_API_ENDPOINT}/me/albums"),
                0, // we don't know the total number of saved albums beforehand
            )
            .await?;

        // Converts `rspotify::model::SavedAlbum` into `state::Album`
        Ok(albums.into_iter().map(Album::from).collect())
    }

    /// Get all saved shows of the current user
    pub async fn current_user_saved_shows(&self) -> Result<Vec<Show>> {
        #[derive(Debug, Deserialize)]
        struct SavedShow {
            show: SavedShowItem,
        }

        #[derive(Debug, Deserialize)]
        struct SavedShowItem {
            id: ShowId<'static>,
            name: String,
        }

        let shows = self
            .all_paging_items::<SavedShow>(
                &format!("{SPOTIFY_API_ENDPOINT}/me/shows"),
                0, // we don't know the total number of saved shows beforehand
            )
            .await?;

        Ok(shows
            .into_iter()
            .map(|s| Show {
                id: s.show.id,
                name: s.show.name,
            })
            .collect())
    }

    /// Get all albums of an artist
    pub async fn artist_albums(&self, artist_id: ArtistId<'_>) -> Result<Vec<Album>> {
        let albums = self
            .all_paging_items_with_limit::<rspotify::model::SimplifiedAlbum>(
                &format!(
                    "{SPOTIFY_API_ENDPOINT}/artists/{}/albums?include_groups=album,single",
                    artist_id.id()
                ),
                0, // we don't know the total number of artist albums beforehand
                SPOTIFY_ARTIST_ALBUMS_PAGE_LIMIT,
            )
            .await?
            .into_iter()
            .filter_map(Album::try_from_simplified_album)
            .collect();

        Ok(AppClient::process_artist_albums(albums))
    }

    pub(super) async fn artist_genres(
        &self,
        artist_id: ArtistId<'_>,
    ) -> Result<(String, Vec<String>)> {
        #[derive(Deserialize)]
        struct ArtistGenres {
            name: String,
            #[serde(default)]
            genres: Vec<String>,
        }

        let artist = self
            .http_get::<ArtistGenres>(
                &format!("{SPOTIFY_API_ENDPOINT}/artists/{}", artist_id.id()),
                &Query::new(),
            )
            .await?;

        Ok((artist.name, artist.genres))
    }
}

#[cfg(test)]
mod tests {
    use super::{
        complete_listenbrainz_artist_enrichment_if_current, direct_spotify_album_id,
        is_explicit_spotify_user_identifier, listenbrainz_album_pending_matches,
        listenbrainz_pending_matches, listenbrainz_remote_detail_entries, lyrics_from_lrclib,
        lyrics_from_lyrics_ovh, lyrics_ovh_url, lyrics_request, parse_youtube_duration_seconds,
        spotify_user_lookup_id, terminalize_youtube_library_if_unloaded,
        youtube_library_failure_state, ListenBrainzResolutionPhase, ListenBrainzResolutionStatus,
        LrcLibLyricsResponse, LyricsOvhResponse, LYRICS_HTTP_TIMEOUT,
    };
    use crate::state::{
        Artist, Context, ListenBrainzAlbumIntent, ListenBrainzAlbumPendingIntent,
        ListenBrainzArtistEnrichment, ListenBrainzPendingIntent, ListenBrainzRecordingIntent,
        LyricsLines, MediaId, MediaKind, PlaylistEntryId, Provider, UnifiedPlaylist,
        UnifiedPlaylistItem, YOUTUBE_LIBRARY_ERROR_MESSAGE,
    };
    use std::collections::BTreeMap;

    fn artist_context(enrichment: ListenBrainzArtistEnrichment) -> Context {
        Context::Artist {
            artist: Artist {
                id: rspotify::model::ArtistId::from_id("artist").unwrap(),
                name: "Artist".to_owned(),
            },
            top_tracks: Vec::new(),
            listenbrainz: enrichment,
            albums: Vec::new(),
            related_artists: Vec::new(),
        }
    }

    #[test]
    fn listenbrainz_artist_enrichment_completion_requires_the_current_request_identity() {
        let mut context = artist_context(ListenBrainzArtistEnrichment::Loading { request_id: 7 });

        assert!(!complete_listenbrainz_artist_enrichment_if_current(
            &mut context,
            6,
            ListenBrainzArtistEnrichment::Unavailable,
        ));
        assert!(matches!(
            context,
            Context::Artist {
                listenbrainz: ListenBrainzArtistEnrichment::Loading { request_id: 7 },
                ..
            }
        ));

        assert!(complete_listenbrainz_artist_enrichment_if_current(
            &mut context,
            7,
            ListenBrainzArtistEnrichment::NoArtistMatch,
        ));
        assert!(matches!(
            context,
            Context::Artist {
                listenbrainz: ListenBrainzArtistEnrichment::NoArtistMatch,
                ..
            }
        ));
    }

    #[test]
    fn listenbrainz_diagnostic_vocabulary_is_bounded_and_secret_free() {
        let phases = ListenBrainzResolutionPhase::ALL.map(|phase| phase.as_str());
        let statuses = ListenBrainzResolutionStatus::ALL.map(|status| status.as_str());

        assert_eq!(
            phases,
            [
                "artist_mapping",
                "artist_popularity",
                "release_group_popularity",
                "recording_relation",
                "spotify_track_fetch",
                "release_relation",
                "spotify_album_fetch",
            ]
        );
        assert_eq!(
            statuses,
            [
                "matched",
                "no_match",
                "available",
                "empty",
                "no_relation",
                "resolved",
                "unavailable",
                "timeout",
                "cancelled",
            ]
        );
        for phase in ListenBrainzResolutionPhase::ALL {
            assert_eq!(ListenBrainzResolutionPhase::from_marker(phase as u8), phase);
        }
        assert_eq!(
            ListenBrainzResolutionPhase::from_marker(u8::MAX),
            ListenBrainzResolutionPhase::ArtistMapping
        );

        let vocabulary = phases.into_iter().chain(statuses).collect::<Vec<_>>();
        for token in vocabulary {
            assert!(
                token
                    .bytes()
                    .all(|byte| byte.is_ascii_lowercase() || byte == b'_'),
                "diagnostic tokens must remain static identifiers"
            );
            assert!(!token.contains("https"));
            assert!(!token.contains("token"));
            assert!(!token.contains("cookie"));
            assert!(!token.contains("mbid"));
        }
    }

    #[test]
    fn listenbrainz_detail_metadata_follows_only_resolved_projection_rows() {
        let playlist = UnifiedPlaylist {
            id: "local".to_owned(),
            name: "Playlist".to_owned(),
            items: vec![
                UnifiedPlaylistItem {
                    entry_id: PlaylistEntryId(1),
                    media_id: MediaId {
                        provider: Provider::Spotify,
                        kind: MediaKind::Track,
                        raw_id: "unresolved".to_owned(),
                    },
                    ..UnifiedPlaylistItem::default()
                },
                UnifiedPlaylistItem {
                    entry_id: PlaylistEntryId(2),
                    media_id: MediaId {
                        provider: Provider::Spotify,
                        kind: MediaKind::Track,
                        raw_id: "resolved".to_owned(),
                    },
                    ..UnifiedPlaylistItem::default()
                },
            ],
            ..UnifiedPlaylist::default()
        };
        let recording_mbids = BTreeMap::from([(
            PlaylistEntryId(2),
            "12345678-1234-1234-1234-123456789abc".to_owned(),
        )]);
        let manifest =
            crate::cli::listenbrainz_manifest::description_manifest_with_recording_mbids(
                &playlist,
                &recording_mbids,
            )
            .unwrap();
        let annotation =
            crate::cli::listenbrainz_manifest::description_envelope_from_manifest(&manifest)
                .unwrap();
        let remote = serde_json::json!({
            "playlist": {
                "annotation": annotation,
                "track": [{"title": "Resolved title", "creator": "Resolved artist"}]
            }
        });

        let details = listenbrainz_remote_detail_entries(&remote);
        assert_eq!(details.get(&1).map(|value| value.0.as_str()), Some(""));
        assert_eq!(
            details
                .get(&2)
                .map(|value| (value.0.as_str(), value.1.as_str())),
            Some(("Resolved title", "Resolved artist"))
        );
    }

    #[test]
    fn listenbrainz_intent_requires_the_same_request_context_and_selected_mbid() {
        let pending = ListenBrainzPendingIntent {
            request_id: 7,
            context_uri: "spotify:artist:artist".to_owned(),
            recording_mbid: "recording".to_owned(),
            intent: ListenBrainzRecordingIntent::Play,
        };

        assert!(listenbrainz_pending_matches(
            &pending,
            7,
            "spotify:artist:artist",
            "recording",
            ListenBrainzRecordingIntent::Play,
            "spotify:artist:artist",
            Some("recording"),
        ));
        assert!(!listenbrainz_pending_matches(
            &pending,
            8,
            "spotify:artist:artist",
            "recording",
            ListenBrainzRecordingIntent::Play,
            "spotify:artist:artist",
            Some("recording"),
        ));
        assert!(!listenbrainz_pending_matches(
            &pending,
            7,
            "spotify:artist:artist",
            "recording",
            ListenBrainzRecordingIntent::Play,
            "spotify:artist:other",
            Some("recording"),
        ));
        assert!(!listenbrainz_pending_matches(
            &pending,
            7,
            "spotify:artist:artist",
            "recording",
            ListenBrainzRecordingIntent::OpenMenu,
            "spotify:artist:artist",
            Some("recording"),
        ));
        assert!(!listenbrainz_pending_matches(
            &pending,
            7,
            "spotify:artist:artist",
            "recording",
            ListenBrainzRecordingIntent::Play,
            "spotify:artist:artist",
            Some("other-recording"),
        ));
    }

    #[test]
    fn listenbrainz_album_intent_requires_current_context_row_and_intent() {
        let pending = ListenBrainzAlbumPendingIntent {
            request_id: 9,
            context_uri: "spotify:artist:artist".to_owned(),
            release_group_mbid: "release-group".to_owned(),
            intent: ListenBrainzAlbumIntent::OpenPage,
        };

        assert!(listenbrainz_album_pending_matches(
            &pending,
            9,
            "spotify:artist:artist",
            "release-group",
            ListenBrainzAlbumIntent::OpenPage,
            "spotify:artist:artist",
            Some("release-group"),
        ));
        for (request_id, current_context, selected, intent) in [
            (
                10,
                "spotify:artist:artist",
                Some("release-group"),
                ListenBrainzAlbumIntent::OpenPage,
            ),
            (
                9,
                "spotify:artist:other",
                Some("release-group"),
                ListenBrainzAlbumIntent::OpenPage,
            ),
            (
                9,
                "spotify:artist:artist",
                Some("other-release"),
                ListenBrainzAlbumIntent::OpenPage,
            ),
            (
                9,
                "spotify:artist:artist",
                Some("release-group"),
                ListenBrainzAlbumIntent::OpenMenu,
            ),
        ] {
            assert!(!listenbrainz_album_pending_matches(
                &pending,
                request_id,
                "spotify:artist:artist",
                "release-group",
                intent,
                current_context,
                selected,
            ));
        }
    }

    #[test]
    fn direct_album_resolution_is_deterministic_without_title_guessing() {
        let relations = vec![
            super::super::listenbrainz::SpotifyAlbumRelation {
                release_mbid: "release-b".to_owned(),
                spotify_album_id: "album-z".to_owned(),
            },
            super::super::listenbrainz::SpotifyAlbumRelation {
                release_mbid: "release-a".to_owned(),
                spotify_album_id: "album-a".to_owned(),
            },
        ];

        assert_eq!(
            direct_spotify_album_id(relations).as_deref(),
            Some("album-a")
        );
        assert_eq!(direct_spotify_album_id(Vec::new()), None);
    }

    #[test]
    fn user_lookup_accepts_spotify_profile_urls() {
        assert_eq!(
            spotify_user_lookup_id("https://open.spotify.com/user/alice?si=ignored"),
            "alice"
        );
        assert_eq!(
            spotify_user_lookup_id("spotify:user:alice"),
            "spotify:user:alice"
        );
    }

    #[test]
    fn user_lookup_preserves_me_alias() {
        assert_eq!(spotify_user_lookup_id("  me  "), "me");
    }

    #[test]
    fn user_name_queries_are_not_treated_as_exact_ids() {
        assert!(!is_explicit_spotify_user_identifier("Alice"));
        assert!(is_explicit_spotify_user_identifier("spotify:user:alice"));
        assert!(is_explicit_spotify_user_identifier(
            "https://open.spotify.com/user/alice"
        ));
    }

    #[test]
    fn youtube_duration_is_projected_for_lyrics_matching() {
        assert_eq!(parse_youtube_duration_seconds("4:02"), Some(242));
        assert_eq!(parse_youtube_duration_seconds("1:02:03"), Some(3_723));
        assert_eq!(parse_youtube_duration_seconds("not-a-duration"), None);
        assert_eq!(parse_youtube_duration_seconds("0:00"), None);
    }

    #[test]
    fn lyrics_requests_have_a_bounded_timeout() {
        let request = lyrics_request(reqwest::Client::new().get("https://example.com"))
            .build()
            .unwrap();
        assert_eq!(request.timeout(), Some(&LYRICS_HTTP_TIMEOUT));
    }

    #[test]
    fn lrclib_prefers_synced_lyrics_and_falls_back_to_plain() {
        let synced = lyrics_from_lrclib(LrcLibLyricsResponse {
            plain_lyrics: Some("plain".to_owned()),
            synced_lyrics: Some("[00:01.00]synced".to_owned()),
        })
        .unwrap();
        assert_eq!(synced.source, "LRCLIB");
        assert!(matches!(synced.lines, LyricsLines::Synced(_)));

        let plain = lyrics_from_lrclib(LrcLibLyricsResponse {
            plain_lyrics: Some("plain".to_owned()),
            synced_lyrics: None,
        })
        .unwrap();
        assert!(matches!(plain.lines, LyricsLines::Plain(_)));
    }

    #[test]
    fn lrclib_rejects_empty_lyrics() {
        assert!(lyrics_from_lrclib(LrcLibLyricsResponse {
            plain_lyrics: Some("  ".to_owned()),
            synced_lyrics: Some("  ".to_owned()),
        })
        .is_none());
    }

    #[test]
    fn lyrics_ovh_projects_plain_lyrics_and_rejects_empty_payloads() {
        let lyrics = lyrics_from_lyrics_ovh(LyricsOvhResponse {
            lyrics: Some("first line\nsecond line".to_owned()),
        })
        .unwrap();
        assert_eq!(lyrics.source, "Lyrics.ovh");
        assert!(matches!(lyrics.lines, LyricsLines::Plain(_)));
        assert!(lyrics_from_lyrics_ovh(LyricsOvhResponse {
            lyrics: Some("  ".to_owned()),
        })
        .is_none());
    }

    #[test]
    fn lyrics_ovh_url_encodes_metadata_as_path_segments() {
        let url = lyrics_ovh_url("Song / Title", "AC/DC").unwrap();
        assert_eq!(
            url.as_str(),
            "https://api.lyrics.ovh/v1/AC%2FDC/Song%20%2F%20Title"
        );
    }

    #[test]
    fn youtube_library_failure_state_is_loaded_but_safe() {
        let library = youtube_library_failure_state();
        assert!(library.loaded);
        assert!(library.playlists.is_empty());
        assert!(library.albums.is_empty());
        assert!(library.artists.is_empty());
        assert_eq!(
            library.errors,
            vec![YOUTUBE_LIBRARY_ERROR_MESSAGE.to_owned()]
        );
    }

    #[test]
    fn unresolved_youtube_library_is_terminalized_without_clobbering_loaded_data() {
        let mut unresolved = crate::state::YouTubeLibrary::default();
        terminalize_youtube_library_if_unloaded(&mut unresolved);
        assert!(unresolved.loaded);
        assert_eq!(
            unresolved.errors,
            vec![YOUTUBE_LIBRARY_ERROR_MESSAGE.to_owned()]
        );

        let mut loaded = crate::state::YouTubeLibrary {
            loaded: true,
            errors: vec!["partial but usable".to_owned()],
            ..crate::state::YouTubeLibrary::default()
        };
        terminalize_youtube_library_if_unloaded(&mut loaded);
        assert_eq!(loaded.errors, vec!["partial but usable"]);
    }
}

fn youtube_library_failure_state() -> crate::state::YouTubeLibrary {
    crate::state::YouTubeLibrary {
        loaded: true,
        errors: vec![crate::state::YOUTUBE_LIBRARY_ERROR_MESSAGE.to_owned()],
        ..crate::state::YouTubeLibrary::default()
    }
}

/// A scheduler outcome must never leave the visible library at its initial
/// `loaded = false` state. Preserve a real response, but turn an unresolved
/// request into the same privacy-safe terminal state used by the provider
/// adapter's own error and timeout paths.
pub(crate) fn terminalize_youtube_library_if_unloaded(library: &mut crate::state::YouTubeLibrary) {
    if !library.loaded {
        *library = youtube_library_failure_state();
    }
}

impl AppClient {
    pub(super) async fn refresh_youtube_library(&self, state: &SharedState) -> Result<()> {
        let library =
            match tokio::time::timeout(YOUTUBE_LIBRARY_TIMEOUT, self.youtube_library()).await {
                Ok(Ok(library)) => library,
                Ok(Err(error)) => {
                    state.data.write().user_data.youtube_library = youtube_library_failure_state();
                    return Err(error);
                }
                Err(_) => {
                    state.data.write().user_data.youtube_library = youtube_library_failure_state();
                    anyhow::bail!(
                        "YouTube Music library request timed out after {} seconds",
                        YOUTUBE_LIBRARY_TIMEOUT.as_secs()
                    );
                }
            };
        state.data.write().user_data.youtube_library = library;
        Ok(())
    }

    pub(super) async fn handle_provider_read_request(
        &self,
        state: &SharedState,
        request: ClientRequest,
        activation: Option<&ActivationPermit>,
    ) -> Result<()> {
        match request {
            ClientRequest::CheckUnifiedPlaylistListenBrainzSync {
                unified_playlist_id,
                operation_reference,
                mode,
            } => {
                let preview_result = async {
                    let (mut playlist, remote_playlist_id, mut base) = {
                        let data = state.data.read();
                        let playlist = data
                            .unified_playlists
                            .iter()
                            .find(|playlist| playlist.id == unified_playlist_id)
                            .cloned()
                            .context("unified playlist not found")?;
                        let link = data
                            .playlist_links
                            .iter()
                            .find(|link| link.unified_playlist_id == unified_playlist_id)
                            .context("unified playlist link not found")?;
                        let remote_playlist_id = link
                            .listenbrainz_playlist_id
                            .clone()
                            .context("ListenBrainz playlist link not found")?;
                        let base = link
                            .listenbrainz_sync
                            .as_ref()
                            .map(|sync| sync.base.clone())
                            .context("ListenBrainz sync base is not initialized")?;
                        (playlist, remote_playlist_id, base)
                    };
                    let configs = config::get_config();
                    anyhow::ensure!(
                        configs.app_config.listenbrainz.enabled,
                        "ListenBrainz integration is disabled"
                    );
                    anyhow::ensure!(
                        configs.app_config.listenbrainz.read_only_checking,
                        "ListenBrainz read-only checking is disabled"
                    );
                    let token = configs
                        .listenbrainz_token()
                        .context("ListenBrainz token is not configured")?;
                    let adapter = super::listenbrainz_push::ListenBrainzMutationAdapter::new(
                        self.http.clone(),
                    );
                    let remote = adapter.fetch(&token, &remote_playlist_id).await?;
                    if mode == super::ListenBrainzSyncReadMode::Recovery {
                        let observed_at = std::time::SystemTime::now()
                            .duration_since(std::time::UNIX_EPOCH)
                            .context("read system clock")?
                            .as_secs();
                        let verified = super::listenbrainz_push::verified_base_from_readback(
                            &remote_playlist_id,
                            &playlist,
                            &remote,
                            None,
                            observed_at,
                        )
                        .ok();
                        state.data.write().recover_listenbrainz_sync_intent(
                            &unified_playlist_id,
                            &remote_playlist_id,
                            verified,
                            observed_at,
                        )?;
                        let data = state.data.read();
                        playlist = data
                            .unified_playlists
                            .iter()
                            .find(|playlist| playlist.id == unified_playlist_id)
                            .cloned()
                            .context("unified playlist not found after recovery")?;
                        base = data
                            .playlist_links
                            .iter()
                            .find(|link| link.unified_playlist_id == unified_playlist_id)
                            .and_then(|link| link.listenbrainz_sync.as_ref())
                            .map(|sync| sync.base.clone())
                            .context("ListenBrainz sync base is unavailable after recovery")?;
                    }
                    let preview = super::listenbrainz_sync::build_persisted_base_pull_preview(
                        &remote_playlist_id,
                        &base,
                        &playlist,
                        &remote,
                    );
                    let ui_preview = listenbrainz_sync_preview_model(
                        &operation_reference,
                        &playlist,
                        &base,
                        &remote,
                        &preview,
                    );
                    Ok::<_, anyhow::Error>((preview, ui_preview))
                }
                .await;

                match preview_result {
                    Ok((preview, ui_preview)) => {
                        use super::listenbrainz_sync::{ChangeClassification, PlanStatus};
                        let lifecycle = if preview.plan.status == PlanStatus::CannotPlan {
                            crate::state::ListenBrainzSyncLifecycle::CannotPlan {
                                next_action:
                                    "Refresh the link or repair its manifest before applying.",
                            }
                        } else {
                            crate::state::ListenBrainzSyncLifecycle::Ready {
                                remote_changed: preview.plan.changes.iter().any(|change| {
                                    matches!(
                                        change.classification,
                                        ChangeClassification::RemoteOnly
                                            | ChangeClassification::SameChange
                                    )
                                }),
                                both_same: !preview.plan.changes.is_empty()
                                    && preview.plan.changes.iter().all(|change| {
                                        change.classification == ChangeClassification::SameChange
                                    }),
                                conflicts: preview.plan.conflicts.len(),
                                manifest_only: preview.remote_unresolved,
                            }
                        };
                        state.ui.lock().finish_listenbrainz_sync_check(
                            &unified_playlist_id,
                            &operation_reference,
                            lifecycle,
                            ui_preview,
                        );
                    }
                    Err(error) => {
                        state.ui.lock().finish_listenbrainz_sync_check_failed(
                            &unified_playlist_id,
                            &operation_reference,
                        );
                        return Err(error);
                    }
                }
            }
            ClientRequest::InitializeUnifiedPlaylistListenBrainzBase {
                unified_playlist_id,
                operation_reference,
            } => {
                let init_result = async {
                    let (playlist, remote_playlist_id) = {
                        let data = state.data.read();
                        let playlist = data
                            .unified_playlists
                            .iter()
                            .find(|playlist| playlist.id == unified_playlist_id)
                            .cloned()
                            .context("unified playlist not found")?;
                        let link = data
                            .playlist_links
                            .iter()
                            .find(|link| link.unified_playlist_id == unified_playlist_id)
                            .context("unified playlist link not found")?;
                        let remote_playlist_id = link
                            .listenbrainz_playlist_id
                            .clone()
                            .context("ListenBrainz playlist link not found")?;
                        anyhow::ensure!(
                            link.listenbrainz_sync.is_none(),
                            "ListenBrainz sync base is already initialized"
                        );
                        (playlist, remote_playlist_id)
                    };
                    let configs = config::get_config();
                    anyhow::ensure!(
                        configs.app_config.listenbrainz.enabled,
                        "ListenBrainz integration is disabled"
                    );
                    anyhow::ensure!(
                        configs.app_config.listenbrainz.read_only_checking,
                        "ListenBrainz read-only checking is disabled"
                    );
                    let token = configs
                        .listenbrainz_token()
                        .context("ListenBrainz token is not configured")?;
                    let adapter = super::listenbrainz_push::ListenBrainzMutationAdapter::new(
                        self.http.clone(),
                    );
                    let remote = adapter.fetch(&token, &remote_playlist_id).await?;
                    let plan = super::listenbrainz_sync::build_remote_anchored_plan(
                        &remote_playlist_id,
                        &playlist,
                        &remote,
                        None,
                    );
                    anyhow::ensure!(
                        plan.status == super::listenbrainz_sync::PlanStatus::Ready
                            && plan.conflicts.is_empty(),
                        "cannot initialize a ListenBrainz base from an unsafe plan"
                    );
                    let observed_at = std::time::SystemTime::now()
                        .duration_since(std::time::UNIX_EPOCH)
                        .context("read system clock")?
                        .as_secs();
                    let base = super::listenbrainz_push::verified_base_from_readback(
                        &remote_playlist_id,
                        &playlist,
                        &remote,
                        None,
                        observed_at,
                    )?;
                    state.data.write().store_verified_listenbrainz_base(
                        &unified_playlist_id,
                        &remote_playlist_id,
                        crate::state::ListenBrainzSyncState::verified(base)?,
                    )?;
                    let (playlist, base) = {
                        let data = state.data.read();
                        let playlist = data
                            .unified_playlists
                            .iter()
                            .find(|playlist| playlist.id == unified_playlist_id)
                            .cloned()
                            .context("unified playlist not found after init")?;
                        let base = data
                            .playlist_links
                            .iter()
                            .find(|link| link.unified_playlist_id == unified_playlist_id)
                            .and_then(|link| link.listenbrainz_sync.as_ref())
                            .map(|sync| sync.base.clone())
                            .context("ListenBrainz sync base is unavailable after init")?;
                        (playlist, base)
                    };
                    let preview = super::listenbrainz_sync::build_persisted_base_pull_preview(
                        &remote_playlist_id,
                        &base,
                        &playlist,
                        &remote,
                    );
                    let ui_preview = listenbrainz_sync_preview_model(
                        &operation_reference,
                        &playlist,
                        &base,
                        &remote,
                        &preview,
                    );
                    Ok::<_, anyhow::Error>((preview, ui_preview))
                }
                .await;

                match init_result {
                    Ok((preview, ui_preview)) => {
                        use super::listenbrainz_sync::{ChangeClassification, PlanStatus};
                        let lifecycle = if preview.plan.status == PlanStatus::CannotPlan {
                            crate::state::ListenBrainzSyncLifecycle::CannotPlan {
                                next_action:
                                    "Refresh the link or repair its manifest before applying.",
                            }
                        } else {
                            crate::state::ListenBrainzSyncLifecycle::Ready {
                                remote_changed: preview.plan.changes.iter().any(|change| {
                                    matches!(
                                        change.classification,
                                        ChangeClassification::RemoteOnly
                                            | ChangeClassification::SameChange
                                    )
                                }),
                                both_same: !preview.plan.changes.is_empty()
                                    && preview.plan.changes.iter().all(|change| {
                                        change.classification == ChangeClassification::SameChange
                                    }),
                                conflicts: preview.plan.conflicts.len(),
                                manifest_only: preview.remote_unresolved,
                            }
                        };
                        state.ui.lock().finish_listenbrainz_sync_check(
                            &unified_playlist_id,
                            &operation_reference,
                            lifecycle,
                            ui_preview,
                        );
                    }
                    Err(error) => {
                        state.ui.lock().finish_listenbrainz_sync_check_failed(
                            &unified_playlist_id,
                            &operation_reference,
                        );
                        return Err(error);
                    }
                }
            }
            ClientRequest::GetBrowseCategories => {
                let categories = Self::browse_categories();
                self.state_application
                    .apply_browse_categories(&mut state.data.write().browse, categories);
            }
            ClientRequest::GetBrowseCategoryPlaylists(category) => {
                let playlists = Self::browse_category_playlists(&category.id);
                self.state_application.apply_browse_category_playlists(
                    &mut state.data.write().browse,
                    category.id,
                    playlists,
                );
            }
            ClientRequest::GetLyrics { track_id } => {
                let uri = track_id.uri();
                let cache_key = crate::state::LyricsCacheKey::new(&uri, None);
                if !state.data.read().caches.lyrics.contains_key(&cache_key) {
                    let lyrics = if let Some(lyrics) = self.lyrics(track_id.clone()).await? {
                        Some(lyrics)
                    } else {
                        let track = self.track(track_id).await?;
                        let artists = track
                            .artists
                            .iter()
                            .map(|artist| artist.name.as_str())
                            .collect::<Vec<_>>()
                            .join(", ");
                        match self
                            .external_lyrics(
                                &track.name,
                                &artists,
                                track.album.as_ref().map(|album| album.name.as_str()),
                                Some(track.duration.as_secs()),
                                None,
                            )
                            .await
                        {
                            Ok(lyrics) => lyrics,
                            Err(err) => {
                                crate::observability::log_safe_error!(
                                    warn,
                                    crate::observability::DiagnosticCode::EXTERNAL_LYRICS_FAILED,
                                    crate::observability::ErrorCategory::Unavailable,
                                    &err,
                                    "External lyrics lookup failed"
                                );
                                None
                            }
                        }
                    };
                    state
                        .data
                        .write()
                        .caches
                        .lyrics
                        .insert(cache_key, lyrics, *TTL_CACHE_DURATION);
                }
            }
            ClientRequest::GetYouTubeLyrics(track) => {
                let uri = format!("youtube:{}", track.id);
                let cache_key = crate::state::LyricsCacheKey::new(&uri, None);
                if !state.data.read().caches.lyrics.contains_key(&cache_key) {
                    let lyrics = match self
                        .external_lyrics(
                            &track.name,
                            &track.artists,
                            track.album.as_deref(),
                            parse_youtube_duration_seconds(&track.duration),
                            Some(&track.id),
                        )
                        .await
                    {
                        Ok(lyrics) => lyrics,
                        Err(err) => {
                            crate::observability::log_safe_error!(
                                warn,
                                crate::observability::DiagnosticCode::YOUTUBE_LYRICS_FAILED,
                                crate::observability::ErrorCategory::Unavailable,
                                &err,
                                "YouTube Music lyrics lookup failed"
                            );
                            None
                        }
                    };
                    state
                        .data
                        .write()
                        .caches
                        .lyrics
                        .insert(cache_key, lyrics, *TTL_CACHE_DURATION);
                }
            }
            ClientRequest::GetLyricsFromProvider { track_id, provider } => {
                let uri = track_id.uri();
                let cache_key = crate::state::LyricsCacheKey::new(&uri, Some(&provider));
                if !state.data.read().caches.lyrics.contains_key(&cache_key) {
                    let track = self.track(track_id).await?;
                    let artists = track
                        .artists
                        .iter()
                        .map(|artist| artist.name.as_str())
                        .collect::<Vec<_>>()
                        .join(", ");
                    let lyrics = match self
                        .external_lyrics_from_provider(
                            &track.name,
                            &artists,
                            track.album.as_ref().map(|album| album.name.as_str()),
                            Some(track.duration.as_secs()),
                            None,
                            &provider,
                        )
                        .await
                    {
                        Ok(lyrics) => lyrics,
                        Err(err) => {
                            crate::observability::log_safe_error!(
                                warn,
                                crate::observability::DiagnosticCode::EXTERNAL_LYRICS_FAILED,
                                crate::observability::ErrorCategory::Unavailable,
                                &err,
                                "Selected external lyrics lookup failed"
                            );
                            None
                        }
                    };
                    state
                        .data
                        .write()
                        .caches
                        .lyrics
                        .insert(cache_key, lyrics, *TTL_CACHE_DURATION);
                }
            }
            ClientRequest::GetYouTubeLyricsFromProvider { track, provider } => {
                let uri = format!("youtube:{}", track.id);
                let cache_key = crate::state::LyricsCacheKey::new(&uri, Some(&provider));
                if !state.data.read().caches.lyrics.contains_key(&cache_key) {
                    let lyrics = match self
                        .external_lyrics_from_provider(
                            &track.name,
                            &track.artists,
                            track.album.as_deref(),
                            parse_youtube_duration_seconds(&track.duration),
                            Some(&track.id),
                            &provider,
                        )
                        .await
                    {
                        Ok(lyrics) => lyrics,
                        Err(err) => {
                            crate::observability::log_safe_error!(
                                warn,
                                crate::observability::DiagnosticCode::YOUTUBE_LYRICS_FAILED,
                                crate::observability::ErrorCategory::Unavailable,
                                &err,
                                "Selected YouTube lyrics lookup failed"
                            );
                            None
                        }
                    };
                    state
                        .data
                        .write()
                        .caches
                        .lyrics
                        .insert(cache_key, lyrics, *TTL_CACHE_DURATION);
                }
            }
            ClientRequest::GetCurrentUser => {
                let user = self.spotify_api().current_user().await?;
                state.data.write().user_data.user = Some(user);
            }
            ClientRequest::GetSpotifyUser(query) => {
                let query = query.trim().to_owned();
                let lookup_id = spotify_user_lookup_id(&query);
                let popup_result: Result<crate::state::PopupState> = async {
                    let popup = if is_explicit_spotify_user_identifier(&query) {
                    if lookup_id.eq_ignore_ascii_case("me") {
                        let user = self.spotify_api().current_user().await?;
                        let profile = crate::state::SpotifyUserProfile {
                            id: user.id.id().to_owned(),
                            display_name: user.display_name,
                            profile_url: user.id.url(),
                            lookup_note: Some(
                                "Your public playlists are shown below. Spotify does not expose a user follower graph."
                                    .to_owned(),
                            ),
                        };
                        let playlists = self.current_user_playlists().await?;
                        crate::state::PopupState::SpotifyUserPlaylists {
                            profile,
                            playlists,
                            state: ratatui::widgets::ListState::default(),
                        }
                    } else {
                        let user_id = rspotify::model::UserId::from_id_or_uri(&lookup_id)
                            .map_err(|error| anyhow::anyhow!(error))?;
                        crate::state::PopupState::SpotifyUserProfile {
                            profile: crate::state::SpotifyUserProfile {
                                id: user_id.id().to_owned(),
                                display_name: None,
                                profile_url: user_id.url(),
                                lookup_note: Some(
                                    "Paste a name to discover playlist owners, or use `me` for your playlists."
                                        .to_owned(),
                                ),
                            },
                        }
                    }
                } else {
                    let results = self.search(&query).await?;
                    let normalized_query = query.to_lowercase();
                    let mut candidates =
                        HashMap::<String, crate::state::SpotifyUserCandidate>::new();
                    for playlist in results.playlists {
                        let (display_name, user_id) = &playlist.owner;
                        if !display_name.to_lowercase().contains(&normalized_query) {
                            continue;
                        }
                        let id = user_id.id().to_owned();
                        let entry = candidates.entry(id.clone()).or_insert_with(|| {
                            crate::state::SpotifyUserCandidate {
                                profile: crate::state::SpotifyUserProfile {
                                    id,
                                    display_name: Some(display_name.clone()),
                                    profile_url: user_id.url(),
                                    lookup_note: Some(
                                        "Candidate discovered from matching public playlist owners."
                                            .to_owned(),
                                    ),
                                },
                                playlists: Vec::new(),
                            }
                        });
                        entry.playlists.push(playlist);
                    }
                    let mut candidates = candidates.into_values().collect::<Vec<_>>();
                    candidates.sort_by(|left, right| {
                        left.profile
                            .display_name
                            .cmp(&right.profile.display_name)
                            .then_with(|| left.profile.id.cmp(&right.profile.id))
                    });
                    if candidates.is_empty() {
                        crate::state::PopupState::DeferredAction {
                            title: "Spotify User Search".to_string(),
                            message: "No matching public playlist owners were found. Spotify's Web API does not provide display-name user search; paste a profile URL or use `me`.".to_string(),
                        }
                    } else {
                        crate::state::PopupState::SpotifyUserCandidates {
                            query: query.clone(),
                            candidates,
                            state: ratatui::widgets::ListState::default(),
                        }
                    }
                    };
                    Ok(popup)
                }
                .await;
                let mut ui = state.ui.lock();
                let is_current_lookup = matches!(
                    &ui.popup,
                    Some(crate::state::PopupState::SpotifyUserSearch { query: current })
                        if current == &query
                );
                if !is_current_lookup {
                    return Ok(());
                }
                match popup_result {
                    Ok(popup) => ui.popup = Some(popup),
                    Err(error) => {
                        crate::observability::log_safe_error!(
                            warn,
                            crate::observability::DiagnosticCode::REQUEST_HANDLE_FAILED,
                            crate::observability::ErrorCategory::Unavailable,
                            &error,
                            "Spotify user discovery failed"
                        );
                        ui.popup = Some(crate::state::PopupState::DeferredAction {
                            title: "Spotify User Search".to_string(),
                            message: "Spotify user discovery is unavailable. Try again, or paste a Spotify profile URL.".to_string(),
                        });
                    }
                }
            }
            ClientRequest::GetDevices => {
                #[allow(unused_mut)]
                let mut devices: Vec<Device> = self
                    .available_devices()
                    .await?
                    .into_iter()
                    .filter_map(Device::try_from_device)
                    .collect();

                #[cfg(feature = "streaming")]
                self.ensure_integrated_device(&mut devices).await;

                state.player.write().devices = devices;
            }
            ClientRequest::GetUserPlaylists => {
                let playlists = self.current_user_playlists().await?;
                let node = state.data.read().user_data.playlist_folder_node.clone();
                let playlists = if let Some(node) = node.filter(|n| !n.children.is_empty()) {
                    crate::playlist_folders::structurize(playlists, &node.children)
                } else {
                    playlists
                        .into_iter()
                        .map(PlaylistFolderItem::Playlist)
                        .collect()
                };
                store_data_into_file_cache(
                    FileCacheKey::Playlists,
                    &config::get_config().cache_folder,
                    &playlists,
                )
                .context("store user's playlists into the cache folder")?;
                state.data.write().user_data.playlists = playlists;
            }
            ClientRequest::GetUserFollowedArtists => {
                let artists = self.current_user_followed_artists().await?;
                store_data_into_file_cache(
                    FileCacheKey::FollowedArtists,
                    &config::get_config().cache_folder,
                    &artists,
                )
                .context("store user's followed artists into the cache folder")?;
                state.data.write().user_data.followed_artists = artists;
            }
            ClientRequest::GetUserSavedAlbums => {
                let albums = self.current_user_saved_albums().await?;
                store_data_into_file_cache(
                    FileCacheKey::SavedAlbums,
                    &config::get_config().cache_folder,
                    &albums,
                )
                .context("store user's saved albums into the cache folder")?;
                state.data.write().user_data.saved_albums = albums;
            }
            ClientRequest::GetYouTubeLibrary => {
                self.refresh_youtube_library(state).await?;
            }
            ClientRequest::GetYouTubeContext(id) => {
                let context_id = id.clone();
                let result = self.youtube_context(&context_id).await;
                let mut ui = state.ui.lock();
                if let crate::state::PageState::YouTubeContext {
                    id,
                    context: page_context,
                    state: page_state,
                    ..
                } = ui.current_page_mut()
                {
                    if *id == context_id {
                        match result {
                            Ok(context) => {
                                page_state.status = if context.tracks.is_empty() {
                                    crate::state::UiViewStatus::Empty
                                } else {
                                    crate::state::UiViewStatus::Ready
                                };
                                *page_context = Some(context);
                            }
                            Err(err) => {
                                let (diagnostic_code, diagnostic_category) =
                                    crate::observability::preserved_error_diagnostic(&err)
                                        .unwrap_or((
                                            crate::observability::DiagnosticCode::YOUTUBE_CONTEXT_LOAD_FAILED,
                                            crate::observability::ErrorCategory::Unavailable,
                                        ));
                                crate::observability::log_safe_error!(
                                    error,
                                    diagnostic_code,
                                    diagnostic_category,
                                    &err,
                                    "Unable to load the YouTube Music context"
                                );
                                page_state.status = crate::state::UiViewStatus::Failed {
                                    code: crate::state::YOUTUBE_CONTEXT_ERROR_CODE,
                                    message: crate::state::YOUTUBE_CONTEXT_ERROR_MESSAGE,
                                    next_action: crate::state::YOUTUBE_CONTEXT_ERROR_NEXT_ACTION,
                                };
                            }
                        }
                    }
                }
            }
            ClientRequest::GetUserSavedShows => {
                let shows = self.current_user_saved_shows().await?;
                store_data_into_file_cache(
                    FileCacheKey::SavedShows,
                    &config::get_config().cache_folder,
                    &shows,
                )
                .context("store user's saved shows into the cache folder")?;
                state.data.write().user_data.saved_shows = shows;
            }
            ClientRequest::GetContext(context) => {
                let uri = context.uri();
                // Liked tracks must always be refreshed to keep user_data.saved_tracks in sync.
                let cache_miss = uri != USER_LIKED_TRACKS_URI
                    && !state.data.read().caches.context.contains_key(&uri);
                let is_liked = uri == USER_LIKED_TRACKS_URI;
                if cache_miss || is_liked {
                    let ctx_result = async {
                        Ok(match context {
                            ContextId::Playlist(playlist_id) => {
                                self.playlist_context(playlist_id).await?
                            }
                            ContextId::Album(album_id) => self.album_context(album_id).await?,
                            ContextId::Artist(artist_id) => self.artist_context(artist_id).await?,
                            ContextId::Tracks(tracks_id) => match tracks_id.uri.as_str() {
                                USER_TOP_TRACKS_URI => Context::Tracks {
                                    tracks: self.current_user_top_tracks().await?,
                                    desc: "User's top tracks".to_string(),
                                },
                                USER_RECENTLY_PLAYED_TRACKS_URI => Context::Tracks {
                                    tracks: self.current_user_recently_played_tracks(true).await?,
                                    desc: "User's recently played tracks".to_string(),
                                },
                                USER_LIKED_TRACKS_URI => {
                                    let tracks = self.current_user_saved_tracks().await?;
                                    let tracks_hm = tracks
                                        .iter()
                                        .map(|t| (t.id.uri(), t.clone()))
                                        .collect::<HashMap<_, _>>();
                                    store_data_into_file_cache(
                                        FileCacheKey::SavedTracks,
                                        &config::get_config().cache_folder,
                                        &tracks_hm,
                                    )
                                    .context("store user's saved tracks into the cache folder")?;
                                    state.data.write().user_data.saved_tracks = tracks_hm;
                                    Context::Tracks {
                                        tracks,
                                        desc: "User's liked tracks".to_string(),
                                    }
                                }
                                u if u.starts_with("radio:") => Context::Tracks {
                                    tracks: self
                                        .radio_tracks(u["radio:".len()..].to_string())
                                        .await?,
                                    desc: tracks_id.kind.clone(),
                                },
                                uri => anyhow::bail!("unsupported Tracks context: {uri}"),
                            },
                            ContextId::Show(show_id) => self.show_context(show_id).await?,
                        })
                    }
                    .await;
                    match ctx_result {
                        Ok(ctx) => {
                            state
                                .data
                                .write()
                                .caches
                                .context
                                .insert(uri, ctx, *TTL_CACHE_DURATION);
                        }
                        Err(error) => {
                            let mut ui = state.ui.lock();
                            if let crate::state::PageState::Context {
                                id: Some(current_id),
                                state: Some(page_state),
                                ..
                            } = ui.current_page_mut()
                            {
                                if current_id.uri() == uri {
                                    *page_state = crate::state::ContextPageUIState::Failed {
                                        status: crate::state::UiViewStatus::Failed {
                                            code: crate::state::CONTEXT_ERROR_CODE,
                                            message: crate::state::CONTEXT_ERROR_MESSAGE,
                                            next_action: crate::state::CONTEXT_ERROR_NEXT_ACTION,
                                        },
                                    };
                                }
                            }
                            return Err(error);
                        }
                    }
                }
            }
            ClientRequest::EnrichListenBrainzArtist {
                request_id,
                context_uri,
                spotify_artist_id,
                include_recordings,
                include_release_groups,
            } => {
                let listenbrainz_enabled = {
                    let listenbrainz = &config::get_config().app_config.listenbrainz;
                    listenbrainz.enabled && listenbrainz.artist_enrichment
                };
                let enrichment = if listenbrainz_enabled {
                    let token = config::get_config().listenbrainz_token();
                    let resolve = async {
                        let phase = ListenBrainzResolutionPhase::ArtistMapping;
                        let started = std::time::Instant::now();
                        let artist = super::listenbrainz::artist_from_spotify_id(
                            &self.http,
                            &spotify_artist_id,
                        )
                        .await
                        .map_err(|error| (phase, started.elapsed(), error))?;
                        let Some(artist) = artist else {
                            record_listenbrainz_resolution_stage(
                                phase,
                                ListenBrainzResolutionStatus::NoMatch,
                                crate::observability::OperationOutcome::Success,
                                Some(started.elapsed()),
                                None,
                            );
                            return Ok::<_, ListenBrainzPhaseError>(
                                ListenBrainzArtistEnrichment::NoArtistMatch,
                            );
                        };
                        record_listenbrainz_resolution_stage(
                            phase,
                            ListenBrainzResolutionStatus::Matched,
                            crate::observability::OperationOutcome::Success,
                            Some(started.elapsed()),
                            None,
                        );

                        let recordings = async {
                            if !include_recordings {
                                return (ListenBrainzCollectionStatus::NotRequested, Vec::new());
                            }
                            let phase = ListenBrainzResolutionPhase::ArtistPopularity;
                            let started = std::time::Instant::now();
                            match super::listenbrainz::top_recordings_for_artist(
                                &self.http,
                                &artist.id,
                                token.as_deref(),
                            )
                            .await
                            {
                                Ok(recordings) => {
                                    let status = if recordings.is_empty() {
                                        ListenBrainzCollectionStatus::Empty
                                    } else {
                                        ListenBrainzCollectionStatus::Available
                                    };
                                    record_listenbrainz_resolution_stage(
                                        phase,
                                        if recordings.is_empty() {
                                            ListenBrainzResolutionStatus::Empty
                                        } else {
                                            ListenBrainzResolutionStatus::Available
                                        },
                                        crate::observability::OperationOutcome::Success,
                                        Some(started.elapsed()),
                                        None,
                                    );
                                    (status, recordings)
                                }
                                Err(error) => {
                                    record_listenbrainz_resolution_stage(
                                        phase,
                                        ListenBrainzResolutionStatus::Unavailable,
                                        crate::observability::OperationOutcome::Error,
                                        Some(started.elapsed()),
                                        Some(crate::observability::ErrorCategory::Unavailable),
                                    );
                                    crate::observability::log_safe_error!(
                                        warn,
                                        crate::observability::DiagnosticCode::LISTENBRAINZ_ARTIST_ENRICHMENT_FAILED,
                                        crate::observability::ErrorCategory::Unavailable,
                                        &error,
                                        "ListenBrainz artist recordings are unavailable"
                                    );
                                    (ListenBrainzCollectionStatus::Unavailable, Vec::new())
                                }
                            }
                        };
                        let release_groups = async {
                            if !include_release_groups {
                                return (ListenBrainzCollectionStatus::NotRequested, Vec::new());
                            }
                            let phase = ListenBrainzResolutionPhase::ReleaseGroupPopularity;
                            let started = std::time::Instant::now();
                            match super::listenbrainz::top_release_groups_for_artist(
                                &self.http,
                                &artist.id,
                                token.as_deref(),
                            )
                            .await
                            {
                                Ok(release_groups) => {
                                    let status = if release_groups.is_empty() {
                                        ListenBrainzCollectionStatus::Empty
                                    } else {
                                        ListenBrainzCollectionStatus::Available
                                    };
                                    record_listenbrainz_resolution_stage(
                                        phase,
                                        if release_groups.is_empty() {
                                            ListenBrainzResolutionStatus::Empty
                                        } else {
                                            ListenBrainzResolutionStatus::Available
                                        },
                                        crate::observability::OperationOutcome::Success,
                                        Some(started.elapsed()),
                                        None,
                                    );
                                    (status, release_groups)
                                }
                                Err(error) => {
                                    record_listenbrainz_resolution_stage(
                                        phase,
                                        ListenBrainzResolutionStatus::Unavailable,
                                        crate::observability::OperationOutcome::Error,
                                        Some(started.elapsed()),
                                        Some(crate::observability::ErrorCategory::Unavailable),
                                    );
                                    crate::observability::log_safe_error!(
                                        warn,
                                        crate::observability::DiagnosticCode::LISTENBRAINZ_ARTIST_ENRICHMENT_FAILED,
                                        crate::observability::ErrorCategory::Unavailable,
                                        &error,
                                        "ListenBrainz artist release groups are unavailable"
                                    );
                                    (ListenBrainzCollectionStatus::Unavailable, Vec::new())
                                }
                            }
                        };
                        let (
                            (recordings_status, recordings),
                            (release_groups_status, release_groups),
                        ) = tokio::join!(recordings, release_groups);
                        Ok(super::listenbrainz::artist_enrichment_from_sources(
                            artist,
                            recordings_status,
                            recordings,
                            release_groups_status,
                            release_groups,
                        ))
                    };
                    match tokio::time::timeout(LISTENBRAINZ_ARTIST_FALLBACK_TIMEOUT, resolve).await
                    {
                        Ok(Ok(enrichment)) => enrichment,
                        Ok(Err((phase, elapsed, error))) => {
                            record_listenbrainz_resolution_stage(
                                phase,
                                ListenBrainzResolutionStatus::Unavailable,
                                crate::observability::OperationOutcome::Error,
                                Some(elapsed),
                                Some(crate::observability::ErrorCategory::Unavailable),
                            );
                            crate::observability::log_safe_error!(
                                warn,
                                crate::observability::DiagnosticCode::LISTENBRAINZ_ARTIST_ENRICHMENT_FAILED,
                                crate::observability::ErrorCategory::Unavailable,
                                &error,
                                "ListenBrainz artist enrichment is unavailable"
                            );
                            ListenBrainzArtistEnrichment::Unavailable
                        }
                        Err(error) => {
                            for (included, phase) in [
                                (
                                    include_recordings,
                                    ListenBrainzResolutionPhase::ArtistPopularity,
                                ),
                                (
                                    include_release_groups,
                                    ListenBrainzResolutionPhase::ReleaseGroupPopularity,
                                ),
                            ] {
                                if included {
                                    record_listenbrainz_resolution_stage(
                                        phase,
                                        ListenBrainzResolutionStatus::Timeout,
                                        crate::observability::OperationOutcome::Timeout,
                                        None,
                                        Some(
                                            crate::observability::ErrorCategory::NetworkUnavailable,
                                        ),
                                    );
                                }
                            }
                            crate::observability::log_safe_error!(
                                warn,
                                crate::observability::DiagnosticCode::LISTENBRAINZ_ARTIST_ENRICHMENT_FAILED,
                                crate::observability::ErrorCategory::NetworkUnavailable,
                                &error,
                                "ListenBrainz artist enrichment exceeded its time budget"
                            );
                            ListenBrainzArtistEnrichment::Unavailable
                        }
                    }
                } else {
                    ListenBrainzArtistEnrichment::NotRequested
                };

                let listenbrainz = &config::get_config().app_config.listenbrainz;
                let enrichment = if listenbrainz.enabled && listenbrainz.artist_enrichment {
                    enrichment
                } else {
                    ListenBrainzArtistEnrichment::NotRequested
                };
                replace_cached_listenbrainz_artist_enrichment_if_current(
                    state,
                    &context_uri,
                    request_id,
                    enrichment,
                );
            }
            ClientRequest::ResolveListenBrainzAlbum {
                request_id,
                context_uri,
                release_group_mbid,
                intent: requested_intent,
            } => {
                let listenbrainz = &config::get_config().app_config.listenbrainz;
                if !listenbrainz.enabled || !listenbrainz.artist_enrichment {
                    replace_listenbrainz_album_resolution_if_current(
                        state,
                        &release_group_mbid,
                        request_id,
                        Some(ListenBrainzAlbumResolution::Unavailable),
                    );
                    clear_current_listenbrainz_album_intent(state, &context_uri, request_id);
                    return Ok(());
                }

                let current_phase =
                    AtomicU8::new(ListenBrainzResolutionPhase::ReleaseRelation as u8);
                let resolve = async {
                    let phase = ListenBrainzResolutionPhase::ReleaseRelation;
                    let started = std::time::Instant::now();
                    let relations = super::listenbrainz::spotify_album_relations_for_release_group(
                        &self.http,
                        &release_group_mbid,
                    )
                    .await
                    .map_err(|error| (phase, started.elapsed(), error))?;
                    let Some(spotify_id) = direct_spotify_album_id(relations) else {
                        record_listenbrainz_resolution_stage(
                            phase,
                            ListenBrainzResolutionStatus::NoRelation,
                            crate::observability::OperationOutcome::Success,
                            Some(started.elapsed()),
                            None,
                        );
                        return Ok::<_, ListenBrainzPhaseError>(None);
                    };
                    let album_id = rspotify::model::AlbumId::from_id(&spotify_id)
                        .map_err(|error| (phase, started.elapsed(), anyhow::anyhow!(error)))?;
                    record_listenbrainz_resolution_stage(
                        phase,
                        ListenBrainzResolutionStatus::Matched,
                        crate::observability::OperationOutcome::Success,
                        Some(started.elapsed()),
                        None,
                    );

                    let phase = ListenBrainzResolutionPhase::SpotifyAlbumFetch;
                    current_phase.store(phase as u8, Ordering::Relaxed);
                    let started = std::time::Instant::now();
                    let album = self
                        .spotify_api()
                        .album(album_id, Some(rspotify::model::Market::FromToken))
                        .await
                        .map(Album::from)
                        .map_err(|error| (phase, started.elapsed(), error.into()))?;
                    record_listenbrainz_resolution_stage(
                        phase,
                        ListenBrainzResolutionStatus::Resolved,
                        crate::observability::OperationOutcome::Success,
                        Some(started.elapsed()),
                        None,
                    );
                    Ok(Some(album))
                };
                let album = match tokio::time::timeout(
                    LISTENBRAINZ_ALBUM_RESOLUTION_TIMEOUT,
                    resolve,
                )
                .await
                {
                    Ok(Ok(Some(album))) => album,
                    Ok(Ok(None)) => {
                        replace_listenbrainz_album_resolution_if_current(
                            state,
                            &release_group_mbid,
                            request_id,
                            Some(ListenBrainzAlbumResolution::NoSpotifyRelation),
                        );
                        clear_current_listenbrainz_album_intent(state, &context_uri, request_id);
                        return Ok(());
                    }
                    Ok(Err((phase, elapsed, error))) => {
                        record_listenbrainz_resolution_stage(
                            phase,
                            ListenBrainzResolutionStatus::Unavailable,
                            crate::observability::OperationOutcome::Error,
                            Some(elapsed),
                            Some(crate::observability::ErrorCategory::Unavailable),
                        );
                        crate::observability::log_safe_error!(
                            warn,
                            crate::observability::DiagnosticCode::LISTENBRAINZ_ALBUM_RESOLUTION_FAILED,
                            crate::observability::ErrorCategory::Unavailable,
                            &error,
                            "ListenBrainz album resolution is unavailable"
                        );
                        replace_listenbrainz_album_resolution_if_current(
                            state,
                            &release_group_mbid,
                            request_id,
                            Some(ListenBrainzAlbumResolution::Unavailable),
                        );
                        clear_current_listenbrainz_album_intent(state, &context_uri, request_id);
                        return Ok(());
                    }
                    Err(error) => {
                        record_listenbrainz_resolution_stage(
                            ListenBrainzResolutionPhase::from_marker(
                                current_phase.load(Ordering::Relaxed),
                            ),
                            ListenBrainzResolutionStatus::Timeout,
                            crate::observability::OperationOutcome::Timeout,
                            None,
                            Some(crate::observability::ErrorCategory::NetworkUnavailable),
                        );
                        crate::observability::log_safe_error!(
                            warn,
                            crate::observability::DiagnosticCode::LISTENBRAINZ_ALBUM_RESOLUTION_FAILED,
                            crate::observability::ErrorCategory::NetworkUnavailable,
                            &error,
                            "ListenBrainz album resolution exceeded its time budget"
                        );
                        replace_listenbrainz_album_resolution_if_current(
                            state,
                            &release_group_mbid,
                            request_id,
                            Some(ListenBrainzAlbumResolution::Unavailable),
                        );
                        clear_current_listenbrainz_album_intent(state, &context_uri, request_id);
                        return Ok(());
                    }
                };

                let listenbrainz = &config::get_config().app_config.listenbrainz;
                if !listenbrainz.enabled || !listenbrainz.artist_enrichment {
                    replace_listenbrainz_album_resolution_if_current(
                        state,
                        &release_group_mbid,
                        request_id,
                        Some(ListenBrainzAlbumResolution::Unavailable),
                    );
                    clear_current_listenbrainz_album_intent(state, &context_uri, request_id);
                    return Ok(());
                }
                if !replace_listenbrainz_album_resolution_if_current(
                    state,
                    &release_group_mbid,
                    request_id,
                    Some(ListenBrainzAlbumResolution::Resolved(album.clone())),
                ) {
                    return Ok(());
                }
                match requested_intent {
                    ListenBrainzAlbumIntent::OpenPage => {
                        let album_id = album.id.clone();
                        consume_current_listenbrainz_album_intent(
                            state,
                            request_id,
                            &context_uri,
                            &release_group_mbid,
                            ListenBrainzAlbumIntent::OpenPage,
                            |ui| {
                                ui.new_page(PageState::Context {
                                    id: None,
                                    context_page_type: ContextPageType::Browsing(ContextId::Album(
                                        album_id,
                                    )),
                                    state: None,
                                });
                            },
                        );
                    }
                    ListenBrainzAlbumIntent::OpenMenu => {
                        let actions = {
                            let data = state.data.read();
                            crate::command::construct_album_actions(&album, &data)
                        };
                        consume_current_listenbrainz_album_intent(
                            state,
                            request_id,
                            &context_uri,
                            &release_group_mbid,
                            ListenBrainzAlbumIntent::OpenMenu,
                            |ui| {
                                ui.popup = Some(PopupState::ActionList(
                                    Box::new(crate::state::ActionListItem::Album(album, actions)),
                                    ratatui::widgets::ListState::default(),
                                ));
                            },
                        );
                    }
                }
            }
            ClientRequest::ResolveListenBrainzRecording {
                request_id,
                context_uri,
                recording_mbid,
                intent: requested_intent,
            } => {
                let listenbrainz = &config::get_config().app_config.listenbrainz;
                if !listenbrainz.enabled || !listenbrainz.artist_enrichment {
                    replace_listenbrainz_resolution_if_current(
                        state,
                        &recording_mbid,
                        request_id,
                        Some(ListenBrainzRecordingResolution::Unavailable),
                    );
                    clear_current_listenbrainz_intent(state, request_id);
                    return Ok(());
                }

                let current_phase =
                    AtomicU8::new(ListenBrainzResolutionPhase::RecordingRelation as u8);
                let resolve = async {
                    let phase = ListenBrainzResolutionPhase::RecordingRelation;
                    let started = std::time::Instant::now();
                    let spotify_id = super::listenbrainz::spotify_track_id_for_recording(
                        &self.http,
                        &recording_mbid,
                    )
                    .await
                    .map_err(|error| (phase, started.elapsed(), error))?;
                    let Some(spotify_id) = spotify_id else {
                        record_listenbrainz_resolution_stage(
                            phase,
                            ListenBrainzResolutionStatus::NoRelation,
                            crate::observability::OperationOutcome::Success,
                            Some(started.elapsed()),
                            None,
                        );
                        return Ok::<_, ListenBrainzPhaseError>(None);
                    };
                    let track_id = TrackId::from_id(&spotify_id)
                        .map_err(|error| (phase, started.elapsed(), anyhow::anyhow!(error)))?;
                    record_listenbrainz_resolution_stage(
                        phase,
                        ListenBrainzResolutionStatus::Matched,
                        crate::observability::OperationOutcome::Success,
                        Some(started.elapsed()),
                        None,
                    );

                    let phase = ListenBrainzResolutionPhase::SpotifyTrackFetch;
                    current_phase.store(phase as u8, Ordering::Relaxed);
                    let started = std::time::Instant::now();
                    let track = self
                        .track(track_id)
                        .await
                        .map_err(|error| (phase, started.elapsed(), error))?;
                    record_listenbrainz_resolution_stage(
                        phase,
                        ListenBrainzResolutionStatus::Resolved,
                        crate::observability::OperationOutcome::Success,
                        Some(started.elapsed()),
                        None,
                    );
                    Ok(Some(track))
                };
                let timed =
                    tokio::time::timeout(LISTENBRAINZ_RECORDING_RESOLUTION_TIMEOUT, resolve);
                let result = if let Some(activation) = activation {
                    let cancellation = activation.cancellation();
                    tokio::select! {
                        biased;
                        () = cancellation.cancelled() => {
                            record_listenbrainz_resolution_stage(
                                ListenBrainzResolutionPhase::from_marker(
                                    current_phase.load(Ordering::Relaxed),
                                ),
                                ListenBrainzResolutionStatus::Cancelled,
                                crate::observability::OperationOutcome::Cancelled,
                                None,
                                Some(crate::observability::ErrorCategory::Cancelled),
                            );
                            replace_listenbrainz_resolution_if_current(
                                state,
                                &recording_mbid,
                                request_id,
                                None,
                            );
                            clear_current_listenbrainz_intent(state, request_id);
                            return Ok(());
                        }
                        result = timed => result,
                    }
                } else {
                    timed.await
                };

                let track = match result {
                    Ok(Ok(Some(track))) => track,
                    Ok(Ok(None)) => {
                        replace_listenbrainz_resolution_if_current(
                            state,
                            &recording_mbid,
                            request_id,
                            Some(ListenBrainzRecordingResolution::NoSpotifyRelation),
                        );
                        clear_current_listenbrainz_intent(state, request_id);
                        return Ok(());
                    }
                    Ok(Err((phase, elapsed, error))) => {
                        record_listenbrainz_resolution_stage(
                            phase,
                            ListenBrainzResolutionStatus::Unavailable,
                            crate::observability::OperationOutcome::Error,
                            Some(elapsed),
                            Some(crate::observability::ErrorCategory::Unavailable),
                        );
                        crate::observability::log_safe_error!(
                            warn,
                            crate::observability::DiagnosticCode::LISTENBRAINZ_RECORDING_RESOLUTION_FAILED,
                            crate::observability::ErrorCategory::Unavailable,
                            &error,
                            "ListenBrainz recording resolution is unavailable"
                        );
                        replace_listenbrainz_resolution_if_current(
                            state,
                            &recording_mbid,
                            request_id,
                            Some(ListenBrainzRecordingResolution::Unavailable),
                        );
                        clear_current_listenbrainz_intent(state, request_id);
                        return Ok(());
                    }
                    Err(error) => {
                        record_listenbrainz_resolution_stage(
                            ListenBrainzResolutionPhase::from_marker(
                                current_phase.load(Ordering::Relaxed),
                            ),
                            ListenBrainzResolutionStatus::Timeout,
                            crate::observability::OperationOutcome::Timeout,
                            None,
                            Some(crate::observability::ErrorCategory::NetworkUnavailable),
                        );
                        crate::observability::log_safe_error!(
                            warn,
                            crate::observability::DiagnosticCode::LISTENBRAINZ_RECORDING_RESOLUTION_FAILED,
                            crate::observability::ErrorCategory::NetworkUnavailable,
                            &error,
                            "ListenBrainz recording resolution exceeded its time budget"
                        );
                        replace_listenbrainz_resolution_if_current(
                            state,
                            &recording_mbid,
                            request_id,
                            Some(ListenBrainzRecordingResolution::Unavailable),
                        );
                        clear_current_listenbrainz_intent(state, request_id);
                        return Ok(());
                    }
                };

                let listenbrainz = &config::get_config().app_config.listenbrainz;
                if !listenbrainz.enabled || !listenbrainz.artist_enrichment {
                    replace_listenbrainz_resolution_if_current(
                        state,
                        &recording_mbid,
                        request_id,
                        Some(ListenBrainzRecordingResolution::Unavailable),
                    );
                    clear_current_listenbrainz_intent(state, request_id);
                    return Ok(());
                }

                if !replace_listenbrainz_resolution_if_current(
                    state,
                    &recording_mbid,
                    request_id,
                    Some(ListenBrainzRecordingResolution::Resolved(track.clone())),
                ) {
                    return Ok(());
                }
                match requested_intent {
                    ListenBrainzRecordingIntent::Play => {
                        if !consume_current_listenbrainz_intent(
                            state,
                            request_id,
                            &context_uri,
                            &recording_mbid,
                            ListenBrainzRecordingIntent::Play,
                            |_| {},
                        ) {
                            return Ok(());
                        }
                        let request = ClientRequest::Player(PlayerRequest::StartPlayback(
                            Playback::URIs(vec![track.id.clone().into()], None),
                            None,
                        ));
                        let _ = Box::pin(self.handle_playback_request(state, request, activation))
                            .await?;
                    }
                    ListenBrainzRecordingIntent::OpenMenu => {
                        let actions = {
                            let data = state.data.read();
                            crate::command::construct_track_actions(&track, &data)
                        };
                        consume_current_listenbrainz_intent(
                            state,
                            request_id,
                            &context_uri,
                            &recording_mbid,
                            ListenBrainzRecordingIntent::OpenMenu,
                            |ui| {
                                ui.popup = Some(PopupState::ActionList(
                                    Box::new(crate::state::ActionListItem::Track(track, actions)),
                                    ratatui::widgets::ListState::default(),
                                ));
                            },
                        );
                    }
                }
            }
            ClientRequest::Search {
                query,
                lifecycle_reference,
            } => {
                if !state.data.read().caches.search.contains_key(&query) {
                    let results = self.search(&query).await?;
                    self.state_application.cache_spotify_search(
                        &mut state.data.write().caches,
                        query.clone(),
                        results,
                    );
                }
                let result_count = {
                    let data = state.data.write();
                    data.caches.search.get(&query).map(|results| {
                        results.tracks.len()
                            + results.artists.len()
                            + results.albums.len()
                            + results.playlists.len()
                            + results.shows.len()
                            + results.episodes.len()
                    })
                };
                if let Some(result_count) = result_count {
                    state.ui.lock().finish_search_success(
                        crate::config::ActiveProvider::Spotify,
                        &query,
                        &lifecycle_reference,
                        result_count,
                    );
                } else {
                    // A successful provider call must still leave the search
                    // lifecycle terminal if its cache entry disappeared
                    // before the UI projection ran.
                    state.ui.lock().finish_search_failure(
                        crate::config::ActiveProvider::Spotify,
                        &query,
                        &lifecycle_reference,
                    );
                }
            }
            ClientRequest::SearchYouTube {
                query,
                lifecycle_reference,
            } => {
                if !state.data.read().caches.youtube_search.contains_key(&query) {
                    let results = self.youtube_search(&query).await?;
                    self.state_application.cache_youtube_search(
                        &mut state.data.write().caches,
                        query.clone(),
                        results,
                    );
                }
                let result_count = {
                    let data = state.data.write();
                    data.caches.youtube_search.get(&query).map(|results| {
                        results.songs.len()
                            + results.videos.len()
                            + results.albums.len()
                            + results.artists.len()
                            + results.playlists.len()
                            + results.podcasts.len()
                            + results.episodes.len()
                    })
                };
                if let Some(result_count) = result_count {
                    state.ui.lock().finish_search_success(
                        crate::config::ActiveProvider::YouTubeMusic,
                        &query,
                        &lifecycle_reference,
                        result_count,
                    );
                } else {
                    // Keep cache and lifecycle state from diverging when the
                    // cache cannot be read back after a successful request.
                    state.ui.lock().finish_search_failure(
                        crate::config::ActiveProvider::YouTubeMusic,
                        &query,
                        &lifecycle_reference,
                    );
                }
            }
            ClientRequest::GetHomeFeed {
                account,
                generation,
            } => {
                // Shelves show at most a shelf's worth of cards, so one
                // request each; "Show all" reads the full lists.
                let (recent, top) = tokio::join!(
                    self.current_user_recently_played_tracks(false),
                    self.current_user_top_tracks_page(crate::state::MAX_HOME_SHELF_CARDS)
                );
                let recent = recent.map_err(|error| home_feed_failure(&error));
                let top = top.map_err(|error| home_feed_failure(&error));
                let now = std::time::Instant::now();
                let mut data = state.data.write();
                let applied = data.home_feed.apply(
                    generation,
                    crate::state::HomeFeedSource::RecentlyPlayed,
                    recent,
                    now,
                ) | data.home_feed.apply(
                    generation,
                    crate::state::HomeFeedSource::TopTracks,
                    top,
                    now,
                );
                if !applied {
                    tracing::debug!(
                        state = "stale_home_feed_result",
                        has_account = account.is_some(),
                        "Discarding a Spotify Home response for an older fetch"
                    );
                }
            }
            ClientRequest::GetCurrentUserQueue(guard) => {
                let queue = self.current_user_queue().await?;
                let mut player = state.player.write();
                if player.native_queue_refresh_guard_is_current(&guard) {
                    player.queue = Some(queue);
                } else {
                    tracing::debug!(
                        state = "stale_native_queue_result",
                        "Discarding a Spotify native queue response after authority changed"
                    );
                }
            }
            _ => unreachable!("request routed to the wrong provider-read handler"),
        }
        Ok(())
    }
}

#[cfg(all(test, feature = "streaming"))]
mod upstream_device_discovery_tests {
    #[tokio::test]
    async fn device_discovery_without_integrated_session_preserves_external_devices() {
        crate::ui::initialize_test_config();
        let client = super::AppClient::new_without_auth().unwrap();
        let mut devices = vec![crate::state::Device {
            id: "external-device".to_owned(),
            name: "External player".to_owned(),
            is_integrated: false,
        }];
        client.ensure_integrated_device(&mut devices).await;
        assert_eq!(devices.len(), 1);
        assert_eq!(devices[0].id, "external-device");
        assert!(!devices[0].is_integrated);
        let mut empty = Vec::new();
        client.ensure_integrated_device(&mut empty).await;
        assert!(empty.is_empty());
    }
}
