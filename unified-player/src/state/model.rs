use crate::config;
use crate::ui::utils::to_bidi_string;
use crate::utils::map_join;
use html_escape::decode_html_entities;
pub use rspotify::model::{
    AlbumId, ArtistId, EpisodeId, Id, PlayableId, PlaylistId, ShowId, TrackId, UserId,
};
use serde::{Deserialize, Serialize};
use sha2::{Digest as _, Sha256};
use std::borrow::Cow;
use std::collections::HashSet;
use std::fmt::{Display, Write};

fn hex_digest(bytes: &[u8]) -> String {
    bytes.iter().fold(
        String::with_capacity(bytes.len() * 2),
        |mut output, byte| {
            write!(output, "{byte:02x}").expect("writing to a String cannot fail");
            output
        },
    )
}

/// A trait similar to Display but with bidirectional text support
pub trait BidiDisplay: Display {
    fn to_bidi_string(&self) -> String {
        let disp_str = self.to_string();
        to_bidi_string(&disp_str)
    }
}

#[derive(Serialize, Clone, Debug)]
#[serde(untagged)]
/// A Spotify context (playlist, album, artist)
pub enum Context {
    Playlist {
        playlist: Playlist,
        tracks: Vec<Track>,
    },
    Album {
        album: Album,
        tracks: Vec<Track>,
    },
    Artist {
        artist: Artist,
        top_tracks: Vec<Track>,
        listenbrainz: ListenBrainzArtistEnrichment,
        albums: Vec<Album>,
        related_artists: Vec<Artist>,
    },
    Tracks {
        tracks: Vec<Track>,
        desc: String,
    },
    Show {
        show: Show,
        episodes: Vec<Episode>,
    },
}

#[derive(Serialize, Clone, Debug, Default, PartialEq, Eq)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum ListenBrainzArtistEnrichment {
    #[default]
    NotRequested,
    Pending,
    Loading {
        request_id: u64,
    },
    Available {
        artist_mbid: String,
        artist_name: String,
        recordings_status: ListenBrainzCollectionStatus,
        recordings: Vec<ListenBrainzPopularRecording>,
        release_groups_status: ListenBrainzCollectionStatus,
        release_groups: Vec<ListenBrainzReleaseGroup>,
    },
    NoArtistMatch,
    Unavailable,
}

#[derive(Serialize, Clone, Copy, Debug, Default, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ListenBrainzCollectionStatus {
    #[default]
    NotRequested,
    Available,
    Empty,
    Unavailable,
}

#[derive(Serialize, Clone, Debug, PartialEq, Eq)]
pub struct ListenBrainzPopularRecording {
    pub recording_mbid: String,
    pub name: String,
    pub total_listen_count: Option<u64>,
    pub total_user_count: Option<u64>,
}

#[derive(Serialize, Clone, Debug, PartialEq, Eq)]
pub struct ListenBrainzReleaseGroup {
    pub release_group_mbid: String,
    pub name: String,
    pub release_date: Option<String>,
    pub release_type: Option<String>,
    pub total_listen_count: Option<u64>,
    pub total_user_count: Option<u64>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TracksId {
    pub uri: String,
    pub kind: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
/// A context Id
pub enum ContextId {
    Playlist(PlaylistId<'static>),
    Album(AlbumId<'static>),
    Artist(ArtistId<'static>),
    Tracks(TracksId),
    Show(ShowId<'static>),
}

/// Data used to start a new playback.
/// There are two ways to start a new playback:
/// - Specify the playing context ID with an offset
/// - Specify the list of track IDs with an offset
///
/// An offset can be either a track's URI or its absolute offset in the context
#[derive(Clone, Debug)]
pub enum Playback {
    Context(ContextId, Option<rspotify::model::Offset>),
    URIs(Vec<PlayableId<'static>>, Option<rspotify::model::Offset>),
}

#[derive(Default, Clone, Debug, Deserialize, Serialize)]
/// Data returned when searching a query using Spotify APIs.
pub struct SearchResults {
    pub tracks: Vec<Track>,
    pub artists: Vec<Artist>,
    pub albums: Vec<Album>,
    pub playlists: Vec<Playlist>,
    pub shows: Vec<Show>,
    pub episodes: Vec<Episode>,
}

/// Provider identity used by app-owned queues and future unified playlists.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Ord, PartialOrd, Serialize, Deserialize)]
#[allow(dead_code)]
pub enum Provider {
    Spotify,
    YouTubeMusic,
}

/// A destination kind used by the provider-neutral playlist workflow.
///
/// The local Unified store is intentionally represented alongside the remote
/// providers here.  The request layer still owns provider-specific execution;
/// this type only captures the user's destination intent.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum PlaylistTargetKind {
    Spotify,
    YouTubeMusic,
    Unified,
}

/// A provider-neutral playlist destination.  `Existing` carries the concrete
/// target identity while `New` carries only the target kind; naming and other
/// attributes remain part of the operation payload.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum PlaylistDestination {
    Existing {
        target: PlaylistTargetKind,
        id: String,
    },
    New {
        target: PlaylistTargetKind,
    },
}

/// The P1 mutation intents.  Remove, move, rename, and delete are deliberately
/// left for later playlist phases.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum PlaylistIntent {
    Create { seed: Vec<PlaylistSeedItem> },
    Append { seed: Vec<PlaylistSeedItem> },
}

/// Correlation and freshness metadata carried by the shared local playlist
/// workflow.  Provider adapters may retain their existing request variants,
/// while this envelope lets the local application service reject stale work
/// before changing memory or disk.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct PlaylistOperationEnvelope {
    pub operation_id: String,
    pub idempotency_key: String,
    /// Application-service correlation retained alongside the human-readable
    /// operation identity. Older persisted/request envelopes may omit it.
    pub application_operation_id: Option<u64>,
    pub source_provider: Option<Provider>,
    pub source_generation: Option<u64>,
    pub source_account_epoch: Option<u64>,
    pub target_account_epoch: Option<u64>,
    pub expected_target_revision: Option<String>,
    pub destination: PlaylistDestination,
    pub intent: PlaylistIntent,
}

impl PlaylistOperationEnvelope {
    pub fn new(
        operation_id: impl Into<String>,
        idempotency_key: impl Into<String>,
        source_provider: Option<Provider>,
        source_generation: Option<u64>,
        destination: PlaylistDestination,
        intent: PlaylistIntent,
        expected_target_revision: Option<String>,
    ) -> Self {
        Self {
            operation_id: operation_id.into(),
            idempotency_key: idempotency_key.into(),
            application_operation_id: None,
            source_account_epoch: source_generation,
            source_provider,
            source_generation,
            target_account_epoch: None,
            expected_target_revision,
            destination,
            intent,
        }
    }

    /// Check the envelope's local invariants before it reaches a mutation
    /// adapter.  The current P1 route accepts only Unified create/append.
    pub fn validate_unified(&self) -> anyhow::Result<()> {
        anyhow::ensure!(
            !self.operation_id.trim().is_empty(),
            "playlist operation has no id"
        );
        anyhow::ensure!(
            !self.idempotency_key.trim().is_empty(),
            "playlist operation has no idempotency key"
        );
        match (&self.destination, &self.intent) {
            (
                PlaylistDestination::New {
                    target: PlaylistTargetKind::Unified,
                },
                PlaylistIntent::Create { seed },
            )
            | (
                PlaylistDestination::Existing {
                    target: PlaylistTargetKind::Unified,
                    ..
                },
                PlaylistIntent::Append { seed },
            ) => anyhow::ensure!(!seed.is_empty(), "playlist operation has no seed items"),
            _ => anyhow::bail!("unsupported Unified playlist operation"),
        }
        Ok(())
    }
}

/// Whether a provider can currently perform a normalized playback control.
///
/// `Unsupported` means the provider does not expose that concept (for
/// example, `YouTube` Music has no remote device selector for local output),
/// while `Unavailable` means the concept exists but there is no active media
/// or queue to operate on yet.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PlaybackSupport {
    Supported,
    Unavailable,
    Unsupported,
}

impl PlaybackSupport {
    pub const fn label(self) -> &'static str {
        match self {
            Self::Supported => "supported",
            Self::Unavailable => "unavailable",
            Self::Unsupported => "unsupported",
        }
    }
}

/// Device/output semantics exposed to the shared playback UI.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PlaybackDeviceMode {
    Selectable,
    FixedLocal,
}

/// Provider-neutral playback control capabilities for the current snapshot.
///
/// This is intentionally a value object rather than a second playback owner:
/// the existing coordinator still performs every operation, while the TUI and
/// platform adapters can make the same honest support decision.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PlaybackCapabilities {
    pub provider: Provider,
    pub play_pause: PlaybackSupport,
    pub seek: PlaybackSupport,
    pub next: PlaybackSupport,
    pub previous: PlaybackSupport,
    pub volume: PlaybackSupport,
    pub mute: PlaybackSupport,
    pub repeat: PlaybackSupport,
    pub shuffle: PlaybackSupport,
    pub device: PlaybackDeviceMode,
}

impl PlaybackCapabilities {
    pub const fn for_provider(provider: Provider, has_playback: bool, has_queue: bool) -> Self {
        let active = if has_playback {
            PlaybackSupport::Supported
        } else {
            PlaybackSupport::Unavailable
        };
        match provider {
            Provider::Spotify => Self {
                provider,
                play_pause: active,
                seek: active,
                next: active,
                previous: active,
                volume: active,
                mute: active,
                repeat: active,
                shuffle: active,
                // Spotify can select/open a remote device before a track is
                // active, so device control is not tied to media presence.
                device: PlaybackDeviceMode::Selectable,
            },
            Provider::YouTubeMusic => {
                let queue_control = if has_playback && has_queue {
                    PlaybackSupport::Supported
                } else if has_playback {
                    PlaybackSupport::Unsupported
                } else {
                    PlaybackSupport::Unavailable
                };
                Self {
                    provider,
                    play_pause: active,
                    seek: active,
                    next: active,
                    previous: active,
                    volume: active,
                    mute: active,
                    repeat: queue_control,
                    shuffle: queue_control,
                    device: PlaybackDeviceMode::FixedLocal,
                }
            }
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Ord, PartialOrd, Serialize, Deserialize)]
#[allow(dead_code)]
pub enum MediaKind {
    Track,
    Video,
    Episode,
}

#[derive(Clone, Debug, PartialEq, Eq, Hash, Ord, PartialOrd, Serialize, Deserialize)]
#[allow(dead_code)]
pub struct MediaId {
    pub provider: Provider,
    pub kind: MediaKind,
    pub raw_id: String,
}

/// A denormalized, provider-neutral snapshot captured from a playlist-capable
/// source row.  It is suitable for local display and matching, but it is not a
/// provider mutation token.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct PlaylistSeedItem {
    pub media_id: MediaId,
    pub title: String,
    pub artists: String,
    pub album: Option<String>,
    pub duration_ms: Option<u64>,
    pub explicit: Option<bool>,
    pub provider_url: Option<String>,
    pub artwork_url: Option<String>,
    /// Whether the source row is not currently playable because its provider
    /// identity has not been resolved. This keeps unresolved Unified metadata
    /// visible after snapshot projection without blocking known-provider rows
    /// whose display fields still need hydration.
    #[serde(default)]
    pub metadata_degraded: bool,
    /// Whether display metadata still needs hydration even when provider
    /// identity is already usable for playback.
    #[serde(default)]
    pub metadata_pending: bool,
}

impl PlaylistSeedItem {
    pub fn from_spotify_track(track: &Track) -> Self {
        Self {
            media_id: MediaId {
                provider: Provider::Spotify,
                kind: MediaKind::Track,
                raw_id: track.id.id().to_string(),
            },
            title: track.name.clone(),
            artists: track.artists_info(),
            album: Some(track.album_info()).filter(|album| !album.is_empty()),
            duration_ms: Some(track.duration.as_millis() as u64),
            explicit: Some(track.explicit),
            provider_url: Some(track.id.uri()),
            artwork_url: None,
            metadata_degraded: false,
            metadata_pending: false,
        }
    }

    pub fn from_youtube_track(track: &YouTubeTrack) -> Self {
        Self {
            media_id: MediaId {
                provider: Provider::YouTubeMusic,
                kind: if track.is_video {
                    MediaKind::Video
                } else {
                    MediaKind::Track
                },
                raw_id: track.id.clone(),
            },
            title: track.name.clone(),
            artists: track.artists.clone(),
            album: track.album.clone(),
            duration_ms: parse_playlist_duration_ms(&track.duration),
            explicit: Some(track.explicit),
            provider_url: Some(format!("https://music.youtube.com/watch?v={}", track.id)),
            artwork_url: track.thumbnail_url.clone(),
            metadata_degraded: false,
            metadata_pending: false,
        }
    }

    /// Compatibility projection for the current Unified storage shape.  P1
    /// keeps the existing schema; the richer seed fields remain available to
    /// the workflow without changing persisted rows ahead of schema v2.
    pub fn into_unified_playlist_item(self) -> UnifiedPlaylistItem {
        let provenance = match self.media_id.provider {
            Provider::Spotify => "spotify",
            Provider::YouTubeMusic => "youtube-music",
        };
        let source_identifier = self.provider_url.clone();
        UnifiedPlaylistItem {
            entry_id: PlaylistEntryId::default(),
            media_id: self.media_id,
            title: self.title,
            artists: self.artists,
            duration_ms: self.duration_ms,
            duration_unit: DurationUnit::Milliseconds,
            provider_url: self.provider_url,
            metadata: UnifiedPlaylistMetadata {
                provenance: Some(provenance.to_owned()),
                source_identifier,
                degraded: self.metadata_degraded,
                metadata_pending: self.metadata_pending,
                ..UnifiedPlaylistMetadata::default()
            },
        }
    }

    pub fn from_unified_playlist_item(item: &UnifiedPlaylistItem) -> Self {
        Self {
            media_id: item.media_id.clone(),
            title: item.title.clone(),
            artists: item.artists.clone(),
            album: None,
            duration_ms: item.duration_ms,
            explicit: None,
            provider_url: item.provider_url.clone(),
            artwork_url: None,
            metadata_degraded: item.metadata.degraded,
            metadata_pending: item.metadata.metadata_pending,
        }
    }

    /// Reconstruct the small Spotify row shape needed by the existing native
    /// create adapter.  The adapter uses only the typed track ID; denormalized
    /// fields are retained where they can be represented safely.
    pub fn to_spotify_track(&self) -> Option<Track> {
        if self.media_id.provider != Provider::Spotify || self.media_id.kind != MediaKind::Track {
            return None;
        }
        Some(Track {
            id: TrackId::from_id(self.media_id.raw_id.clone())
                .ok()?
                .into_static(),
            name: self.title.clone(),
            artists: Vec::new(),
            album: None,
            duration: std::time::Duration::from_millis(self.duration_ms.unwrap_or_default()),
            explicit: self.explicit.unwrap_or(false),
            added_at: 0,
        })
    }

    /// Reconstruct the existing `YouTube` request row shape.  The native adapter
    /// currently needs the provider ID and keeps the captured display fields.
    pub fn to_youtube_track(&self) -> Option<YouTubeTrack> {
        if self.media_id.provider != Provider::YouTubeMusic {
            return None;
        }
        Some(YouTubeTrack {
            id: self.media_id.raw_id.clone(),
            name: self.title.clone(),
            artists: self.artists.clone(),
            album: self.album.clone(),
            duration: format_playlist_duration(self.duration_ms),
            explicit: self.explicit.unwrap_or(false),
            thumbnail_url: self.artwork_url.clone(),
            is_video: self.media_id.kind == MediaKind::Video,
        })
    }
}

fn parse_playlist_duration_ms(value: &str) -> Option<u64> {
    let parts = value
        .split(':')
        .map(str::trim)
        .map(str::parse::<u64>)
        .collect::<Result<Vec<_>, _>>()
        .ok()?;
    if parts.is_empty() || parts.len() > 3 {
        return None;
    }
    let mut seconds = 0_u64;
    for part in parts {
        seconds = seconds.checked_mul(60)?.checked_add(part)?;
    }
    seconds.checked_mul(1_000)
}

fn format_playlist_duration(duration_ms: Option<u64>) -> String {
    let seconds = duration_ms.unwrap_or_default() / 1_000;
    let minutes = seconds / 60;
    let seconds = seconds % 60;
    format!("{minutes}:{seconds:02}")
}

/// A provider-tagged item stored in a unified playlist sidecar. Metadata is
/// denormalized intentionally so the playlist remains useful when a provider
/// is temporarily unavailable.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct UnifiedPlaylistItem {
    /// Playlist-local occurrence identity. Zero is reserved for legacy rows
    /// until the schema migration assigns a durable value.
    #[serde(default)]
    pub entry_id: PlaylistEntryId,
    pub media_id: MediaId,
    pub title: String,
    pub artists: String,
    #[serde(default)]
    pub duration_ms: Option<u64>,
    #[serde(default)]
    pub duration_unit: DurationUnit,
    pub provider_url: Option<String>,
    #[serde(default)]
    pub metadata: UnifiedPlaylistMetadata,
}

impl Default for UnifiedPlaylistItem {
    fn default() -> Self {
        Self {
            entry_id: PlaylistEntryId::default(),
            media_id: MediaId {
                provider: Provider::Spotify,
                kind: MediaKind::Track,
                raw_id: String::new(),
            },
            title: String::new(),
            artists: String::new(),
            duration_ms: None,
            duration_unit: DurationUnit::Milliseconds,
            provider_url: None,
            metadata: UnifiedPlaylistMetadata::default(),
        }
    }
}

impl Display for UnifiedPlaylistItem {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "{} {} {:?} {}",
            self.title, self.artists, self.media_id.provider, self.media_id.raw_id
        )
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct UnifiedPlaylist {
    pub id: String,
    pub name: String,
    pub items: Vec<UnifiedPlaylistItem>,
    pub updated_at: u64,
    /// The next value is persisted per playlist and is never decremented.
    #[serde(default = "default_next_entry_id")]
    pub next_entry_id: u64,
}

/// A playlist-local occurrence identity. It is intentionally distinct from a
/// provider media ID and is never reused after removal.
#[derive(
    Clone, Copy, Debug, Default, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize,
)]
#[serde(transparent)]
pub struct PlaylistEntryId(pub u64);

impl PlaylistEntryId {
    #[allow(dead_code)]
    pub const fn new(value: u64) -> Self {
        Self(value)
    }
}

/// The canonical unit persisted for playlist duration values.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum DurationUnit {
    #[default]
    Milliseconds,
    /// Accepted while reading older/imported data; migration normalizes it to
    /// milliseconds before the row is persisted in schema v2.
    Seconds,
}

/// Provenance and freshness describe the denormalized display snapshot. They
/// are deliberately absent from `snapshot_hash`, which represents membership
/// and ordering only.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct UnifiedPlaylistMetadata {
    #[serde(default)]
    pub provenance: Option<String>,
    #[serde(default)]
    pub observed_at: Option<u64>,
    #[serde(default)]
    pub source_identifier: Option<String>,
    /// The source occurrence ID is retained for portable import provenance;
    /// destination imports still allocate a fresh local `entry_id`.
    #[serde(default)]
    pub source_entry_id: Option<u64>,
    #[serde(default)]
    pub degraded: bool,
    #[serde(default)]
    pub metadata_pending: bool,
}

const fn default_next_entry_id() -> u64 {
    1
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct PlaylistLink {
    pub unified_playlist_id: String,
    pub listenbrainz_playlist_id: Option<String>,
    pub spotify_playlist_id: Option<String>,
    pub youtube_playlist_id: Option<String>,
    #[serde(default)]
    pub last_local_snapshot: Option<String>,
    #[serde(default)]
    pub last_youtube_snapshot: Option<String>,
    /// Account-scoped projection records replace the legacy snapshot pair.
    /// The old fields remain readable so older stores can be migrated without
    /// dropping `ListenBrainz` or provider link identity.
    #[serde(default)]
    pub projections: Vec<PlaylistProjectionState>,
    /// `ListenBrainz` owns a manifest-backed sync protocol rather than a
    /// provider playlist projection, so its durable base is kept separate
    /// from Spotify and `YouTube` projection state.
    #[serde(default)]
    pub listenbrainz_sync: Option<ListenBrainzSyncState>,
    pub updated_at: u64,
}

pub const LISTENBRAINZ_SYNC_STATE_SCHEMA_VERSION: u8 = 1;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ListenBrainzSyncStatus {
    Pending,
    #[default]
    Clean,
    Drifted,
    Conflict,
    Partial,
    OutcomeUnknown,
    Detached,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ListenBrainzProjectionStatus {
    Ineligible,
    Unresolved,
    Resolved,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ListenBrainzSyncBaseEntry {
    pub occurrence: PlaylistEntryId,
    pub media_id: MediaId,
    pub projection_status: ListenBrainzProjectionStatus,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub recording_mbid: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ListenBrainzSyncBase {
    pub manifest_schema_version: u8,
    pub unified_playlist_id: String,
    pub playlist_name: String,
    pub local_snapshot_hash: String,
    pub canonical_manifest_hash: String,
    pub remote_fingerprint: String,
    pub entries: Vec<ListenBrainzSyncBaseEntry>,
    pub verified_at: u64,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ListenBrainzSyncIntent {
    pub operation_id: String,
    pub expected_remote_fingerprint: String,
    pub target_manifest_hash: String,
    pub target_local_snapshot_hash: String,
    pub started_at: u64,
}

impl ListenBrainzSyncIntent {
    pub fn validate(&self) -> anyhow::Result<()> {
        anyhow::ensure!(
            !self.operation_id.trim().is_empty(),
            "ListenBrainz pending intent has no operation ID"
        );
        anyhow::ensure!(
            is_sha256(&self.expected_remote_fingerprint)
                && is_sha256(&self.target_manifest_hash)
                && is_sha256(&self.target_local_snapshot_hash),
            "ListenBrainz pending intent contains an invalid fingerprint"
        );
        Ok(())
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ListenBrainzRecoveryDisposition {
    AlreadyApplied,
    Conflict,
    OutcomeUnknown,
    Rejected,
    Partial,
    VerificationFailed,
    NoPendingIntent,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ListenBrainzSyncRecovery {
    pub operation_id: String,
    pub disposition: ListenBrainzRecoveryDisposition,
    pub observed_at: u64,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ListenBrainzLocalApplySnapshot {
    pub operation_id: String,
    pub playlist: UnifiedPlaylist,
    pub snapshot_hash: String,
    pub captured_at: u64,
}

impl ListenBrainzLocalApplySnapshot {
    pub fn validate(&self) -> anyhow::Result<()> {
        anyhow::ensure!(
            !self.operation_id.trim().is_empty(),
            "ListenBrainz local apply snapshot has no operation ID"
        );
        anyhow::ensure!(
            self.playlist.snapshot_hash() == self.snapshot_hash,
            "ListenBrainz local apply snapshot fingerprint is inconsistent"
        );
        Ok(())
    }
}

impl ListenBrainzSyncBase {
    pub fn validate(&self) -> anyhow::Result<()> {
        anyhow::ensure!(
            matches!(self.manifest_schema_version, 1 | 2),
            "unsupported ListenBrainz sync-base manifest schema"
        );
        anyhow::ensure!(
            !self.unified_playlist_id.trim().is_empty(),
            "ListenBrainz sync base has no Unified playlist ID"
        );
        anyhow::ensure!(
            is_sha256(&self.local_snapshot_hash)
                && is_sha256(&self.canonical_manifest_hash)
                && is_sha256(&self.remote_fingerprint),
            "ListenBrainz sync base contains an invalid fingerprint"
        );
        let mut occurrences = std::collections::HashSet::with_capacity(self.entries.len());
        for entry in &self.entries {
            anyhow::ensure!(
                entry.occurrence.0 > 0 && occurrences.insert(entry.occurrence),
                "ListenBrainz sync base contains an invalid or duplicate occurrence"
            );
            match entry.projection_status {
                ListenBrainzProjectionStatus::Resolved => anyhow::ensure!(
                    entry.recording_mbid.as_deref().is_some_and(is_uuid),
                    "resolved ListenBrainz sync-base entry has no valid recording MBID"
                ),
                ListenBrainzProjectionStatus::Ineligible
                | ListenBrainzProjectionStatus::Unresolved => anyhow::ensure!(
                    entry.recording_mbid.is_none(),
                    "unresolved ListenBrainz sync-base entry contains a recording MBID"
                ),
            }
        }
        Ok(())
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ListenBrainzSyncState {
    pub schema_version: u8,
    pub status: ListenBrainzSyncStatus,
    pub base: ListenBrainzSyncBase,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pending_intent: Option<ListenBrainzSyncIntent>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub recovery: Option<ListenBrainzSyncRecovery>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub local_apply_snapshot: Option<ListenBrainzLocalApplySnapshot>,
}

impl ListenBrainzSyncState {
    pub fn verified(base: ListenBrainzSyncBase) -> anyhow::Result<Self> {
        base.validate()?;
        Ok(Self {
            schema_version: LISTENBRAINZ_SYNC_STATE_SCHEMA_VERSION,
            status: ListenBrainzSyncStatus::Clean,
            base,
            pending_intent: None,
            recovery: None,
            local_apply_snapshot: None,
        })
    }

    pub fn validate(&self) -> anyhow::Result<()> {
        anyhow::ensure!(
            self.schema_version == LISTENBRAINZ_SYNC_STATE_SCHEMA_VERSION,
            "unsupported ListenBrainz sync-state schema"
        );
        self.base.validate()?;
        if let Some(intent) = &self.pending_intent {
            intent.validate()?;
        }
        if let Some(snapshot) = &self.local_apply_snapshot {
            snapshot.validate()?;
            anyhow::ensure!(
                snapshot.playlist.id == self.base.unified_playlist_id,
                "ListenBrainz local apply snapshot targets another Unified playlist"
            );
        }
        Ok(())
    }

    pub fn begin_intent(&mut self, intent: ListenBrainzSyncIntent) -> anyhow::Result<bool> {
        self.validate()?;
        intent.validate()?;
        anyhow::ensure!(
            intent.expected_remote_fingerprint == self.base.remote_fingerprint,
            "ListenBrainz pending intent does not match the verified remote base"
        );
        if self.pending_intent.as_ref() == Some(&intent) {
            return Ok(false);
        }
        anyhow::ensure!(
            self.pending_intent.is_none(),
            "a different ListenBrainz operation is already pending"
        );
        self.pending_intent = Some(intent);
        self.recovery = None;
        self.status = ListenBrainzSyncStatus::Pending;
        Ok(true)
    }

    pub fn begin_resolution_intent(
        &mut self,
        intent: ListenBrainzSyncIntent,
        verified_remote_fingerprint: &str,
    ) -> anyhow::Result<bool> {
        self.validate()?;
        intent.validate()?;
        anyhow::ensure!(
            intent.expected_remote_fingerprint == verified_remote_fingerprint,
            "ListenBrainz resolution intent does not match the previewed remote state"
        );
        if self.pending_intent.as_ref() == Some(&intent) {
            return Ok(false);
        }
        anyhow::ensure!(
            self.pending_intent.is_none(),
            "a different ListenBrainz operation is already pending"
        );
        self.pending_intent = Some(intent);
        self.recovery = None;
        self.status = ListenBrainzSyncStatus::Pending;
        Ok(true)
    }

    pub fn recover_pending_intent(
        &mut self,
        verified_remote: Option<ListenBrainzSyncBase>,
        observed_at: u64,
    ) -> anyhow::Result<ListenBrainzRecoveryDisposition> {
        self.validate()?;
        let Some(intent) = self.pending_intent.clone() else {
            return Ok(ListenBrainzRecoveryDisposition::NoPendingIntent);
        };
        let disposition = if let Some(remote) = verified_remote {
            remote.validate()?;
            anyhow::ensure!(
                remote.unified_playlist_id == self.base.unified_playlist_id,
                "ListenBrainz recovery read-back targets another Unified playlist"
            );
            if remote.canonical_manifest_hash == intent.target_manifest_hash {
                self.base = remote;
                self.pending_intent = None;
                self.status = ListenBrainzSyncStatus::Clean;
                ListenBrainzRecoveryDisposition::AlreadyApplied
            } else if remote.remote_fingerprint == intent.expected_remote_fingerprint {
                self.status = ListenBrainzSyncStatus::OutcomeUnknown;
                ListenBrainzRecoveryDisposition::OutcomeUnknown
            } else {
                self.status = ListenBrainzSyncStatus::Conflict;
                ListenBrainzRecoveryDisposition::Conflict
            }
        } else {
            self.status = ListenBrainzSyncStatus::OutcomeUnknown;
            ListenBrainzRecoveryDisposition::OutcomeUnknown
        };
        self.recovery = Some(ListenBrainzSyncRecovery {
            operation_id: intent.operation_id,
            disposition,
            observed_at,
        });
        Ok(disposition)
    }

    pub fn record_pending_outcome(
        &mut self,
        status: ListenBrainzSyncStatus,
        disposition: ListenBrainzRecoveryDisposition,
        observed_at: u64,
    ) -> anyhow::Result<()> {
        self.validate()?;
        anyhow::ensure!(
            matches!(
                status,
                ListenBrainzSyncStatus::Partial
                    | ListenBrainzSyncStatus::OutcomeUnknown
                    | ListenBrainzSyncStatus::Conflict
            ),
            "invalid failed ListenBrainz transaction status"
        );
        anyhow::ensure!(
            !matches!(
                disposition,
                ListenBrainzRecoveryDisposition::AlreadyApplied
                    | ListenBrainzRecoveryDisposition::NoPendingIntent
            ),
            "invalid failed ListenBrainz recovery disposition"
        );
        let intent = self
            .pending_intent
            .as_ref()
            .ok_or_else(|| anyhow::anyhow!("no ListenBrainz operation is pending"))?;
        self.status = status;
        self.recovery = Some(ListenBrainzSyncRecovery {
            operation_id: intent.operation_id.clone(),
            disposition,
            observed_at,
        });
        Ok(())
    }
}

fn is_sha256(value: &str) -> bool {
    value.len() == 64 && value.bytes().all(|byte| byte.is_ascii_hexdigit())
}

fn is_uuid(value: &str) -> bool {
    value.len() == 36
        && value.bytes().enumerate().all(|(index, byte)| {
            if matches!(index, 8 | 13 | 18 | 23) {
                byte == b'-'
            } else {
                byte.is_ascii_digit() || matches!(byte, b'a'..=b'f')
            }
        })
}

/// The concrete remote playlist that owns a projection.  Account and epoch
/// are both required: an account switch must never reuse a target merely
/// because its provider playlist ID happens to match.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct PlaylistProjectionTarget {
    pub provider: Provider,
    pub account_id: String,
    pub account_epoch: u64,
    pub playlist_id: String,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum PlaylistProjectionStatus {
    #[default]
    Pending,
    Clean,
    Drifted,
    Conflict,
    Partial,
    OutcomeUnknown,
    Detached,
}

impl PlaylistProjectionStatus {
    pub const fn is_clean(self) -> bool {
        matches!(self, Self::Clean)
    }

    pub const fn needs_preview_confirmation(self) -> bool {
        matches!(
            self,
            Self::Drifted | Self::Conflict | Self::Partial | Self::OutcomeUnknown
        )
    }

    /// `OutcomeUnknown` is deliberately not retryable: the caller must first
    /// inspect and acknowledge the remote result to avoid duplicate writes.
    pub const fn permits_retry(self) -> bool {
        matches!(
            self,
            Self::Pending | Self::Clean | Self::Drifted | Self::Conflict | Self::Partial
        )
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Ord, PartialOrd, Serialize, Deserialize)]
pub enum PlaylistProjectionConflictKind {
    Membership,
    Order,
    Mapping,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct PlaylistProjectionConflict {
    pub kind: PlaylistProjectionConflictKind,
    pub message: String,
    #[serde(default)]
    pub local_entry_ids: Vec<PlaylistEntryId>,
    #[serde(default)]
    pub remote_positions: Vec<usize>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct PlaylistProjectionMapping {
    pub local_entry_id: PlaylistEntryId,
    pub remote_media_id: MediaId,
    /// Provider occurrence tokens (for example `YouTube` `SetVideoID`) are
    /// optional because read-only provider contracts may not expose them.
    #[serde(default)]
    pub remote_occurrence_token: Option<String>,
    pub accepted_at: u64,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct PlaylistProjectionRecovery {
    pub reason: String,
    #[serde(default)]
    pub unresolved_items: Vec<String>,
    #[serde(default)]
    pub failed_appends: usize,
    pub next_action: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct PlaylistProjectionState {
    pub target: PlaylistProjectionTarget,
    #[serde(default)]
    pub local_revision: Option<String>,
    #[serde(default)]
    pub remote_revision: Option<String>,
    #[serde(default)]
    pub status: PlaylistProjectionStatus,
    #[serde(default)]
    pub conflicts: Vec<PlaylistProjectionConflict>,
    #[serde(default)]
    pub mappings: Vec<PlaylistProjectionMapping>,
    #[serde(default)]
    pub acknowledged_operations: Vec<String>,
    #[serde(default)]
    pub acknowledged_intents: Vec<String>,
    #[serde(default)]
    pub recovery: Option<PlaylistProjectionRecovery>,
}

impl PlaylistProjectionState {
    pub fn pending(target: PlaylistProjectionTarget) -> Self {
        Self {
            target,
            local_revision: None,
            remote_revision: None,
            status: PlaylistProjectionStatus::Pending,
            conflicts: Vec::new(),
            mappings: Vec::new(),
            acknowledged_operations: Vec::new(),
            acknowledged_intents: Vec::new(),
            recovery: None,
        }
    }

    pub fn is_clean_for(&self, local_revision: &str) -> bool {
        self.status.is_clean()
            && self.conflicts.is_empty()
            && self.local_revision.as_deref() == Some(local_revision)
    }

    pub fn can_retry(&self, operation_id: &str) -> bool {
        !self
            .acknowledged_operations
            .iter()
            .any(|acknowledged| acknowledged == operation_id)
            && self.status.permits_retry()
    }

    pub fn can_retry_intent(&self, intent_key: &str) -> bool {
        self.status.permits_retry()
            && (!self
                .acknowledged_intents
                .iter()
                .any(|acknowledged| acknowledged == intent_key)
                // A clean projection retry is a read-back probe.  It may
                // discover external drift and only appends rows that the
                // verified remote state is missing.
                || self.status == PlaylistProjectionStatus::Clean)
    }

    pub fn acknowledge_operation(&mut self, operation_id: impl Into<String>) -> bool {
        let operation_id = operation_id.into();
        if self
            .acknowledged_operations
            .iter()
            .any(|acknowledged| acknowledged == &operation_id)
        {
            return false;
        }
        self.acknowledged_operations.push(operation_id);
        true
    }

    pub fn acknowledge_intent(&mut self, intent_key: impl Into<String>) -> bool {
        let intent_key = intent_key.into();
        if self
            .acknowledged_intents
            .iter()
            .any(|acknowledged| acknowledged == &intent_key)
        {
            return false;
        }
        self.acknowledged_intents.push(intent_key);
        true
    }

    pub fn preview_summary(&self) -> String {
        let recovery = self.recovery.as_ref().map(|recovery| {
            format!(
                "recovery: {}; next: {}",
                recovery.reason, recovery.next_action
            )
        });
        if self.conflicts.is_empty() {
            return recovery
                .unwrap_or_else(|| "no membership, order, or mapping conflicts".to_owned());
        }
        let conflicts = self
            .conflicts
            .iter()
            .map(|conflict| format!("{:?}: {}", conflict.kind, conflict.message))
            .collect::<Vec<_>>()
            .join("; ");
        match recovery {
            Some(recovery) => format!("{conflicts}; {recovery}"),
            None => conflicts,
        }
    }
}

impl PlaylistLink {
    pub fn projection_for(
        &self,
        provider: Provider,
        account_id: &str,
        account_epoch: u64,
        playlist_id: &str,
    ) -> Option<&PlaylistProjectionState> {
        self.projections.iter().find(|projection| {
            let target = &projection.target;
            target.provider == provider
                && target.account_id == account_id
                && target.account_epoch == account_epoch
                && target.playlist_id == playlist_id
        })
    }

    pub fn projection_for_mut(
        &mut self,
        provider: Provider,
        account_id: &str,
        account_epoch: u64,
        playlist_id: &str,
    ) -> Option<&mut PlaylistProjectionState> {
        self.projections.iter_mut().find(|projection| {
            let target = &projection.target;
            target.provider == provider
                && target.account_id == account_id
                && target.account_epoch == account_epoch
                && target.playlist_id == playlist_id
        })
    }

    pub fn upsert_projection(&mut self, projection: PlaylistProjectionState) {
        if let Some(existing) = self
            .projections
            .iter_mut()
            .find(|existing| existing.target == projection.target)
        {
            *existing = projection;
        } else {
            self.projections.push(projection);
        }
    }

    #[allow(dead_code)]
    pub fn detach_incompatible(
        &mut self,
        provider: Provider,
        account_id: &str,
        account_epoch: u64,
    ) {
        for projection in &mut self.projections {
            let target = &projection.target;
            if target.provider == provider
                && (target.account_id != account_id || target.account_epoch != account_epoch)
            {
                projection.status = PlaylistProjectionStatus::Detached;
                projection.conflicts.clear();
            }
        }
    }

    #[allow(dead_code)]
    pub fn projection_status_for_local(
        &self,
        provider: Provider,
        playlist_id: &str,
        local_revision: &str,
    ) -> Option<PlaylistProjectionStatus> {
        self.projections
            .iter()
            .find(|projection| {
                projection.target.provider == provider
                    && projection.target.playlist_id == playlist_id
            })
            .map(|projection| {
                if projection.is_clean_for(local_revision) {
                    PlaylistProjectionStatus::Clean
                } else if projection.status == PlaylistProjectionStatus::Clean {
                    PlaylistProjectionStatus::Drifted
                } else {
                    projection.status
                }
            })
    }
}

impl UnifiedPlaylist {
    pub fn snapshot_hash(&self) -> String {
        // Denormalized display metadata and freshness must not turn a provider
        // hydration refresh into a membership/order conflict.
        let items = self
            .items
            .iter()
            .map(|item| {
                serde_json::json!({
                    "entry_id": item.entry_id,
                    "provider": item.media_id.provider,
                    "kind": item.media_id.kind,
                    "raw_id": item.media_id.raw_id,
                })
            })
            .collect::<Vec<_>>();
        let value = serde_json::json!({
            "name": self.name,
            "items": items,
        });
        hex_digest(&Sha256::digest(
            serde_json::to_vec(&value).unwrap_or_default(),
        ))
    }

    /// Normalize rows loaded from legacy/v1 storage and repair deterministic
    /// collisions before the playlist is exposed to callers.
    pub fn normalize_entry_ids(&mut self) -> anyhow::Result<()> {
        let mut used = HashSet::new();
        let has_invalid_ids = self
            .items
            .iter()
            .any(|item| item.entry_id.0 == 0 || !used.insert(item.entry_id.0));
        if has_invalid_ids {
            // A mixed high/missing or colliding source cannot preserve both
            // its explicit values and source-order monotonicity. Deterministic
            // repair therefore allocates the whole occurrence list after the
            // highest observed ID.
            let mut next = self
                .items
                .iter()
                .map(|item| item.entry_id.0)
                .max()
                .unwrap_or(0)
                .checked_add(1)
                .ok_or_else(|| anyhow::anyhow!("Unified playlist entry IDs are exhausted"))?;
            for item in &mut self.items {
                item.entry_id = PlaylistEntryId(next);
                next = next
                    .checked_add(1)
                    .ok_or_else(|| anyhow::anyhow!("Unified playlist entry IDs are exhausted"))?;
            }
            self.next_entry_id = next;
        } else {
            let max_id = self.items.iter().map(|item| item.entry_id.0).max();
            self.next_entry_id = self
                .next_entry_id
                .max(1)
                .max(max_id.map_or(1, |id| id.saturating_add(1)));
        }
        for item in &mut self.items {
            if item.duration_unit == DurationUnit::Seconds {
                item.duration_ms = item
                    .duration_ms
                    .and_then(|duration| duration.checked_mul(1_000));
            }
            item.duration_unit = DurationUnit::Milliseconds;
        }
        Ok(())
    }

    /// Allocate a fresh occurrence ID. Removed IDs are intentionally not
    /// returned to the allocator.
    pub fn allocate_entry_id(&mut self) -> anyhow::Result<PlaylistEntryId> {
        let value = self.next_entry_id.max(1);
        anyhow::ensure!(
            value < u64::MAX,
            "Unified playlist entry ID allocator is exhausted"
        );
        let id = PlaylistEntryId(value);
        self.next_entry_id = value
            .checked_add(1)
            .ok_or_else(|| anyhow::anyhow!("Unified playlist entry ID allocator overflowed"))?;
        Ok(id)
    }

    pub fn allocate_item(
        &mut self,
        mut item: UnifiedPlaylistItem,
    ) -> anyhow::Result<UnifiedPlaylistItem> {
        item.entry_id = self.allocate_entry_id()?;
        item.duration_unit = DurationUnit::Milliseconds;
        Ok(item)
    }

    pub fn youtube_tracks_snapshot_hash(tracks: &[YouTubeTrack]) -> String {
        let value = serde_json::json!({ "tracks": tracks });
        hex_digest(&Sha256::digest(
            serde_json::to_vec(&value).unwrap_or_default(),
        ))
    }

    pub fn to_jspf_value(&self) -> serde_json::Value {
        let tracks = self
            .items
            .iter()
            .map(|item| {
                let identifier =
                    item.provider_url
                        .clone()
                        .unwrap_or_else(|| match item.media_id.provider {
                            Provider::Spotify => {
                                format!("spotify:track:{}", item.media_id.raw_id)
                            }
                            Provider::YouTubeMusic => {
                                format!(
                                    "https://music.youtube.com/watch?v={}",
                                    item.media_id.raw_id
                                )
                            }
                        });
                serde_json::json!({
                    "title": item.title,
                    "creator": item.artists,
                    "duration": item.duration_ms,
                    "identifier": [identifier],
                    "extension": {
                        "unified-player:provider": format!("{:?}", item.media_id.provider),
                        "unified-player:raw_id": item.media_id.raw_id,
                        "unified-player:entry_id": item.entry_id.0,
                        "unified-player:media_kind": format!("{:?}", item.media_id.kind),
                        "unified-player:duration_unit": "milliseconds",
                        "unified-player:metadata_provenance": item
                            .metadata
                            .provenance,
                        "unified-player:metadata_observed_at": item.metadata.observed_at,
                        "unified-player:source_identifier": item.metadata.source_identifier,
                    }
                })
            })
            .collect::<Vec<_>>();
        serde_json::json!({
            "playlist": {
                "title": self.name,
                "track": tracks,
            }
        })
    }
}

impl UnifiedPlaylistItem {
    #[allow(dead_code)]
    pub fn from_spotify_track(track: &Track) -> Self {
        Self {
            entry_id: PlaylistEntryId::default(),
            media_id: MediaId {
                provider: Provider::Spotify,
                kind: MediaKind::Track,
                raw_id: track.id.id().to_string(),
            },
            title: track.name.clone(),
            artists: track.artists_info(),
            duration_ms: Some(track.duration.as_millis() as u64),
            duration_unit: DurationUnit::Milliseconds,
            provider_url: Some(track.id.uri()),
            metadata: UnifiedPlaylistMetadata {
                provenance: Some("spotify".to_owned()),
                ..UnifiedPlaylistMetadata::default()
            },
        }
    }

    #[allow(dead_code)]
    pub fn from_youtube_track(track: &YouTubeTrack) -> Self {
        Self {
            entry_id: PlaylistEntryId::default(),
            media_id: MediaId {
                provider: Provider::YouTubeMusic,
                kind: if track.is_video {
                    MediaKind::Video
                } else {
                    MediaKind::Track
                },
                raw_id: track.id.clone(),
            },
            title: track.name.clone(),
            artists: track.artists.clone(),
            duration_ms: parse_playlist_duration_ms(&track.duration),
            duration_unit: DurationUnit::Milliseconds,
            provider_url: Some(format!("https://music.youtube.com/watch?v={}", track.id)),
            metadata: UnifiedPlaylistMetadata {
                provenance: Some("youtube-music".to_owned()),
                ..UnifiedPlaylistMetadata::default()
            },
        }
    }

    pub fn playable_media(&self) -> Option<PlayableMedia> {
        if self.metadata.degraded {
            return None;
        }
        match self.media_id.provider {
            Provider::YouTubeMusic => Some(PlayableMedia::YouTube(YouTubeTrack {
                id: self.media_id.raw_id.clone(),
                name: self.title.clone(),
                artists: self.artists.clone(),
                album: None,
                duration: self
                    .duration_ms
                    .map(|duration| format!("{}:{:02}", duration / 60_000, (duration / 1_000) % 60))
                    .unwrap_or_default(),
                explicit: false,
                thumbnail_url: Some(youtube_video_thumbnail_url(&self.media_id.raw_id)),
                is_video: self.media_id.kind == MediaKind::Video,
            })),
            Provider::Spotify => match self.media_id.kind {
                MediaKind::Track => Some(PlayableMedia::Spotify(PlayableId::Track(
                    TrackId::from_id(self.media_id.raw_id.clone())
                        .ok()?
                        .into_static(),
                ))),
                MediaKind::Episode => Some(PlayableMedia::Spotify(PlayableId::Episode(
                    EpisodeId::from_id(self.media_id.raw_id.clone())
                        .ok()?
                        .into_static(),
                ))),
                MediaKind::Video => None,
            },
        }
    }
}

fn youtube_video_thumbnail_url(video_id: &str) -> String {
    format!("https://i.ytimg.com/vi/{video_id}/hqdefault.jpg")
}

/// A playable item that can be handed to a provider-specific playback engine.
#[derive(Clone, Debug, PartialEq, Eq)]
#[allow(dead_code)]
pub enum PlayableMedia {
    Spotify(PlayableId<'static>),
    YouTube(YouTubeTrack),
}

#[allow(dead_code)]
impl PlayableMedia {
    pub fn provider(&self) -> Provider {
        match self {
            Self::Spotify(_) => Provider::Spotify,
            Self::YouTube(_) => Provider::YouTubeMusic,
        }
    }

    /// Whether both name the same provider item. Display metadata such as a
    /// `YouTube` duration label can be refreshed during playback, so it does
    /// not take part.
    pub fn is_same_item(&self, other: &Self) -> bool {
        match (self, other) {
            (Self::Spotify(left), Self::Spotify(right)) => left == right,
            (Self::YouTube(left), Self::YouTube(right)) => left.id == right.id,
            _ => false,
        }
    }

    pub fn media_id(&self) -> MediaId {
        match self {
            Self::Spotify(PlayableId::Track(id)) => MediaId {
                provider: Provider::Spotify,
                kind: MediaKind::Track,
                raw_id: id.id().to_string(),
            },
            Self::Spotify(PlayableId::Episode(id)) => MediaId {
                provider: Provider::Spotify,
                kind: MediaKind::Episode,
                raw_id: id.id().to_string(),
            },
            Self::YouTube(track) => MediaId {
                provider: Provider::YouTubeMusic,
                kind: if track.is_video {
                    MediaKind::Video
                } else {
                    MediaKind::Track
                },
                raw_id: track.id.clone(),
            },
        }
    }
}

impl From<PlayableId<'static>> for PlayableMedia {
    fn from(value: PlayableId<'static>) -> Self {
        Self::Spotify(value)
    }
}

impl From<YouTubeTrack> for PlayableMedia {
    fn from(value: YouTubeTrack) -> Self {
        Self::YouTube(value)
    }
}

#[derive(Default, Clone, Debug, Deserialize, Serialize)]
pub struct YouTubeSearchResults {
    pub songs: Vec<YouTubeTrack>,
    pub videos: Vec<YouTubeTrack>,
    pub albums: Vec<YouTubeLibraryAlbum>,
    pub artists: Vec<YouTubeLibraryArtist>,
    pub playlists: Vec<YouTubeLibraryPlaylist>,
    pub podcasts: Vec<YouTubePodcast>,
    pub episodes: Vec<YouTubeEpisode>,
}

#[derive(Deserialize, Serialize, Debug, Clone, PartialEq, Eq)]
pub struct YouTubePodcast {
    pub id: String,
    pub name: String,
    pub publisher: String,
    pub thumbnail_url: Option<String>,
}

#[derive(Deserialize, Serialize, Debug, Clone, PartialEq, Eq)]
pub struct YouTubeEpisode {
    pub track: YouTubeTrack,
    pub date: String,
}

#[derive(Deserialize, Serialize, Debug, Clone, PartialEq, Eq)]
pub struct YouTubeTrack {
    pub id: String,
    pub name: String,
    pub artists: String,
    pub album: Option<String>,
    pub duration: String,
    pub explicit: bool,
    pub thumbnail_url: Option<String>,
    pub is_video: bool,
}

#[derive(Deserialize, Serialize, Debug, Clone, PartialEq, Eq, Default)]
pub struct YouTubePlaybackRoute {
    pub selected: Option<String>,
    pub order: Vec<String>,
    pub learned_public_android_vr: bool,
    /// The runtime that most recently solved a player challenge for this
    /// playback request, if deciphering was needed.
    #[serde(default)]
    pub javascript_backend: Option<String>,
}

#[derive(Deserialize, Serialize, Debug, Clone, PartialEq, Eq)]
pub struct YouTubePlayback {
    pub track: YouTubeTrack,
    pub is_playing: bool,
    pub progress: std::time::Duration,
    pub volume: u8,
    pub mute_state: Option<u8>,
    #[serde(default)]
    pub route: YouTubePlaybackRoute,
}

#[derive(Default, Clone, Debug, Deserialize, Serialize)]
pub struct YouTubeLibrary {
    /// Distinguishes an empty authenticated library from the initial state
    /// before the provider response has arrived.
    #[serde(default)]
    pub loaded: bool,
    pub playlists: Vec<YouTubeLibraryPlaylist>,
    pub albums: Vec<YouTubeLibraryAlbum>,
    pub artists: Vec<YouTubeLibraryArtist>,
    pub errors: Vec<String>,
}

#[derive(Deserialize, Serialize, Debug, Clone, PartialEq, Eq)]
pub struct YouTubeLibraryPlaylist {
    pub id: String,
    pub name: String,
    pub author: String,
    pub tracks: String,
    pub thumbnail_url: Option<String>,
}

#[derive(Deserialize, Serialize, Debug, Clone, PartialEq, Eq)]
pub struct YouTubeLibraryAlbum {
    pub id: String,
    pub name: String,
    pub artist: String,
    pub year: String,
    pub album_type: String,
    pub thumbnail_url: Option<String>,
}

#[derive(Deserialize, Serialize, Debug, Clone, PartialEq, Eq)]
pub struct YouTubeLibraryArtist {
    pub id: String,
    pub name: String,
    pub byline: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum YouTubeContextId {
    LikedTracks,
    Playlist(String),
    Album(String),
    Artist(String),
    Podcast(String),
}

impl YouTubeContextId {
    pub fn title(&self) -> &'static str {
        match self {
            // Match the loaded context title so the page header does not
            // change shape when the liked-media request completes.
            Self::LikedTracks => "Liked Music",
            Self::Playlist(_) => "YouTube Playlist",
            Self::Album(_) => "YouTube Album",
            Self::Artist(_) => "YouTube Artist",
            Self::Podcast(_) => "YouTube Podcast",
        }
    }
}

#[derive(Default, Clone, Debug, PartialEq, Deserialize, Serialize)]
pub struct YouTubeContext {
    pub title: String,
    pub description: Option<String>,
    pub tracks: Vec<YouTubeTrack>,
    /// Provider mutation identity aligned by playlist row. Ordinary reads from
    /// provider clients that omit `SetVideoID` leave entries as `None` so exact
    /// removal fails closed instead of inventing a token.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub playlist_set_video_ids: Vec<Option<String>>,
    #[serde(default)]
    pub artist: Option<YouTubeArtistContext>,
}

#[derive(Clone, Debug, PartialEq, Eq, Deserialize, Serialize)]
pub struct YouTubeArtistContext {
    pub channel_id: String,
    pub name: String,
    pub description: Option<String>,
    pub views: Option<String>,
    pub subscribers: Option<String>,
    pub subscribed: bool,
    pub radio_id: Option<String>,
    pub albums: Vec<YouTubeArtistRelease>,
    pub singles: Vec<YouTubeArtistRelease>,
    pub related: Vec<YouTubeRelatedArtist>,
}

#[derive(Clone, Debug, PartialEq, Eq, Deserialize, Serialize)]
pub struct YouTubeArtistRelease {
    pub id: String,
    pub title: String,
    pub year: Option<String>,
    pub kind: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Deserialize, Serialize)]
pub struct YouTubeRelatedArtist {
    pub id: String,
    pub name: String,
    pub subscribers: String,
}

#[derive(Debug)]
/// A track order
pub enum TrackOrder {
    AddedAt,
    TrackName,
    Album,
    Artists,
    Duration,
}

#[derive(Debug, Clone)]
/// A Spotify item (track, album, artist, playlist)
pub enum Item {
    Track(Track),
    Album(Album),
    Artist(Artist),
    Playlist(Playlist),
    Show(Show),
}

#[derive(Debug, Clone)]
pub enum ItemId {
    Track(TrackId<'static>),
    Album(AlbumId<'static>),
    Artist(ArtistId<'static>),
    Playlist(PlaylistId<'static>),
    Show(ShowId<'static>),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PlaybackMetadata {
    pub device_name: String,
    pub device_id: Option<String>,
    pub volume: Option<u32>,
    pub is_playing: bool,
    pub repeat_state: rspotify::model::RepeatState,
    pub shuffle_state: bool,
    pub mute_state: Option<u32>,
}

#[derive(Debug, Clone)]
/// A Spotify device
pub struct Device {
    pub id: String,
    pub name: String,
    /// Whether this device is the integrated librespot player of *this* running instance.
    ///
    /// Used to distinguish the current app's integrated device from other `unified-player`
    /// instances (which may share the same device name) running elsewhere.
    pub is_integrated: bool,
}

#[derive(Deserialize, Serialize, Debug, Clone)]
/// A Spotify track
pub struct Track {
    pub id: TrackId<'static>,
    pub name: String,
    pub artists: Vec<Artist>,
    pub album: Option<Album>,
    pub duration: std::time::Duration,
    pub explicit: bool,
    #[serde(skip)]
    pub added_at: u64,
}

#[derive(Deserialize, Serialize, Debug, Clone)]
/// A Spotify album
pub struct Album {
    pub id: AlbumId<'static>,
    pub release_date: String,
    pub name: String,
    pub artists: Vec<Artist>,
    pub typ: Option<rspotify::model::AlbumType>,
    pub added_at: u64,
}

#[derive(Deserialize, Serialize, Debug, Clone)]
/// A Spotify artist
pub struct Artist {
    pub id: ArtistId<'static>,
    pub name: String,
}

#[derive(Deserialize, Serialize, Debug, Clone)]
/// A Spotify playlist
pub struct Playlist {
    pub id: PlaylistId<'static>,
    pub collaborative: bool,
    pub name: String,
    pub owner: (String, UserId<'static>),
    pub desc: String,
    /// which folder id the playlist refers to
    #[serde(default)]
    pub current_folder_id: usize,
    pub snapshot_id: String,
}

#[derive(Deserialize, Serialize, Debug, Clone)]
/// A Spotify show (podcast)
pub struct Show {
    pub id: ShowId<'static>,
    pub name: String,
}

#[derive(Deserialize, Serialize, Debug, Clone)]
/// A Spotify episode (podcast episode)
pub struct Episode {
    pub id: EpisodeId<'static>,
    pub name: String,
    pub description: String,
    pub duration: std::time::Duration,
    pub show: Option<Show>,
    pub release_date: String,
}

#[derive(Deserialize, Serialize, Debug, Clone)]
/// A playlist folder, not related to Spotify API yet
pub struct PlaylistFolder {
    pub name: String,
    /// current folder id in the folders tree
    pub current_id: usize,
    /// target folder id it refers to
    pub target_id: usize,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
/// A playlist folder item
pub enum PlaylistFolderItem {
    Playlist(Playlist),
    Folder(PlaylistFolder),
}

#[derive(Deserialize, Debug, Clone)]
/// A reference node retrieved by running <https://github.com/mikez/spotify-folders>
/// Helps building a playlist folder hierarchy
pub struct PlaylistFolderNode {
    pub name: Option<String>,
    #[serde(rename = "type")]
    pub node_type: String,
    #[serde(default)]
    pub uri: String,
    #[serde(default = "Vec::new")]
    pub children: Vec<PlaylistFolderNode>,
}

#[derive(Clone, Debug, PartialEq)]
/// A Spotify category
pub struct Category {
    pub id: String,
    pub name: String,
}

impl Context {}

impl ContextId {
    pub fn uri(&self) -> String {
        match self {
            Self::Album(id) => id.uri(),
            Self::Artist(id) => id.uri(),
            Self::Playlist(id) => id.uri(),
            Self::Tracks(id) => id.uri.clone(),
            Self::Show(id) => id.uri(),
        }
    }
}

impl TrackOrder {
    pub fn compare(&self, x: &Track, y: &Track) -> std::cmp::Ordering {
        match *self {
            Self::AddedAt => x.added_at.cmp(&y.added_at),
            Self::TrackName => x.name.cmp(&y.name),
            Self::Album => x.album_info().cmp(&y.album_info()),
            Self::Duration => x.duration.cmp(&y.duration),
            Self::Artists => x.artists_info().cmp(&y.artists_info()),
        }
    }
}

impl Device {
    /// tries to convert from a `rspotify::model::Device` into `Device`
    pub fn try_from_device(device: rspotify::model::Device) -> Option<Self> {
        Some(Self {
            id: device.id?,
            name: device.name,
            is_integrated: false,
        })
    }
}

impl Track {
    /// gets the track's artists information
    pub fn artists_info(&self) -> String {
        map_join(&self.artists, |a| &a.name, ", ")
    }

    /// gets the track's album information
    pub fn album_info(&self) -> String {
        self.album
            .as_ref()
            .map(|a| a.name.clone())
            .unwrap_or_default()
    }

    /// gets the track's name, including an explicit label
    pub fn display_name(&self) -> Cow<'_, str> {
        if self.explicit {
            Cow::Owned(format!(
                "{} {}",
                self.name,
                config::get_config().app_config.explicit_icon
            ))
        } else {
            Cow::Borrowed(self.name.as_str())
        }
    }

    /// tries to convert from a `rspotify::model::SimplifiedTrack` into `Track`
    pub fn try_from_simplified_track(track: rspotify::model::SimplifiedTrack) -> Option<Self> {
        if track.is_playable.unwrap_or(true) {
            let id = track.id?;
            Some(Self {
                id,
                name: track.name,
                artists: from_simplified_artists_to_artists(track.artists),
                album: None,
                duration: track.duration.to_std().expect("valid chrono duration"),
                explicit: track.explicit,
                added_at: 0,
            })
        } else {
            None
        }
    }

    /// tries to convert from a `rspotify::model::FullTrack` into `Track` with a optional `added_at` date
    fn try_from_full_track_with_date(
        track: rspotify::model::FullTrack,
        added_at: Option<chrono::DateTime<chrono::Utc>>,
    ) -> Option<Self> {
        if track.is_playable.unwrap_or(true) {
            let id = track.id?;
            Some(Self {
                id,
                name: track.name,
                artists: from_simplified_artists_to_artists(track.artists),
                album: Album::try_from_simplified_album(track.album),
                duration: track.duration.to_std().expect("valid chrono duration"),
                explicit: track.explicit,
                added_at: added_at.map(|t| t.timestamp() as u64).unwrap_or_default(),
            })
        } else {
            None
        }
    }

    /// tries to convert from a `rspotify::model::FullTrack` into `Track`
    pub fn try_from_full_track(track: rspotify::model::FullTrack) -> Option<Self> {
        Track::try_from_full_track_with_date(track, None)
    }

    /// tries to convert from a `rspotify::model::PlaylistItem` into `Track`
    pub fn try_from_playlist_item(item: rspotify::model::PlaylistItem) -> Option<Self> {
        let rspotify::model::PlayableItem::Track(track) = item.item? else {
            return None;
        };

        Track::try_from_full_track_with_date(track, item.added_at)
    }
}

impl std::fmt::Display for Track {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "{} • {} ▎ {}",
            self.display_name(),
            self.artists_info(),
            self.album_info(),
        )
    }
}

impl BidiDisplay for Track {}

impl std::fmt::Display for YouTubeTrack {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let kind = if self.is_video { "video" } else { "song" };
        match &self.album {
            Some(album) if !album.is_empty() => {
                write!(f, "{} • {} ▎ {} • {}", self.name, self.artists, album, kind)
            }
            _ => write!(f, "{} • {} • {}", self.name, self.artists, kind),
        }
    }
}

impl BidiDisplay for YouTubeTrack {}

impl std::fmt::Display for YouTubeLibraryPlaylist {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match (self.tracks.is_empty(), self.author.is_empty()) {
            (false, false) => write!(f, "{} • {} • {}", self.name, self.tracks, self.author),
            (false, true) => write!(f, "{} • {}", self.name, self.tracks),
            (true, false) => write!(f, "{} • {}", self.name, self.author),
            (true, true) => write!(f, "{}", self.name),
        }
    }
}

impl BidiDisplay for YouTubeLibraryPlaylist {}

impl std::fmt::Display for YouTubeLibraryAlbum {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let mut parts = vec![self.name.clone(), self.artist.clone()];
        if !self.year.is_empty() {
            parts.push(self.year.clone());
        }
        if !self.album_type.is_empty() {
            parts.push(self.album_type.clone());
        }
        write!(f, "{}", parts.join(" • "))
    }
}

impl BidiDisplay for YouTubeLibraryAlbum {}

impl std::fmt::Display for YouTubeLibraryArtist {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        if self.byline.is_empty() {
            write!(f, "{}", self.name)
        } else {
            write!(f, "{} • {}", self.name, self.byline)
        }
    }
}

impl BidiDisplay for YouTubeLibraryArtist {}

impl std::fmt::Display for YouTubePodcast {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        if self.publisher.is_empty() {
            write!(f, "{}", self.name)
        } else {
            write!(f, "{} • {}", self.name, self.publisher)
        }
    }
}

impl BidiDisplay for YouTubePodcast {}

impl std::fmt::Display for YouTubeEpisode {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        if self.date.is_empty() {
            write!(f, "{}", self.track)
        } else {
            write!(f, "{} • {}", self.track, self.date)
        }
    }
}

impl BidiDisplay for YouTubeEpisode {}

impl Album {
    /// tries to convert from a `rspotify::model::SimplifiedAlbum` into `Album`
    pub fn try_from_simplified_album(album: rspotify::model::SimplifiedAlbum) -> Option<Self> {
        Some(Self {
            id: album.id?,
            name: album.name,
            release_date: album.release_date.unwrap_or_default(),
            artists: from_simplified_artists_to_artists(album.artists),
            typ: album
                .album_type
                .and_then(|t| match t.to_ascii_lowercase().as_str() {
                    "album" => Some(rspotify::model::AlbumType::Album),
                    "single" => Some(rspotify::model::AlbumType::Single),
                    "appears_on" => Some(rspotify::model::AlbumType::AppearsOn),
                    "compilation" => Some(rspotify::model::AlbumType::Compilation),
                    _ => None,
                }),
            added_at: 0,
        })
    }

    /// gets the album's release year
    pub fn year(&self) -> String {
        self.release_date
            .split('-')
            .next()
            .unwrap_or("")
            .to_string()
    }

    /// gets the album type
    pub fn album_type(&self) -> String {
        match self.typ {
            Some(t) => <&str>::from(t).to_string(),
            _ => String::new(),
        }
    }
}

impl From<rspotify::model::FullAlbum> for Album {
    fn from(album: rspotify::model::FullAlbum) -> Self {
        Self {
            name: album.name,
            id: album.id,
            release_date: album.release_date,
            artists: from_simplified_artists_to_artists(album.artists),
            typ: Some(album.album_type),
            added_at: 0,
        }
    }
}

impl From<rspotify::model::SavedAlbum> for Album {
    fn from(saved_album: rspotify::model::SavedAlbum) -> Self {
        let mut album: Album = saved_album.album.into();
        album.added_at = saved_album.added_at.timestamp() as u64;
        album
    }
}

impl std::fmt::Display for Album {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "{} • {} ({})",
            self.name,
            map_join(&self.artists, |a| &a.name, ", "),
            self.year()
        )
    }
}

impl BidiDisplay for Album {}

impl Artist {
    /// tries to convert from a `rspotify::model::SimplifiedArtist` into `Artist`
    pub fn try_from_simplified_artist(artist: rspotify::model::SimplifiedArtist) -> Option<Self> {
        Some(Self {
            id: artist.id?,
            name: artist.name,
        })
    }
}

impl From<rspotify::model::FullArtist> for Artist {
    fn from(artist: rspotify::model::FullArtist) -> Self {
        Self {
            name: artist.name,
            id: artist.id,
        }
    }
}

impl std::fmt::Display for Artist {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.name)
    }
}

/// a helper function to convert a vector of `rspotify::model::SimplifiedArtist`
/// into a vector of `Artist`.
fn from_simplified_artists_to_artists(
    artists: Vec<rspotify::model::SimplifiedArtist>,
) -> Vec<Artist> {
    artists
        .into_iter()
        .filter_map(Artist::try_from_simplified_artist)
        .collect()
}

impl BidiDisplay for Artist {}

impl From<rspotify::model::SimplifiedPlaylist> for Playlist {
    fn from(playlist: rspotify::model::SimplifiedPlaylist) -> Self {
        Self {
            id: playlist.id,
            name: playlist.name,
            collaborative: playlist.collaborative,
            owner: (
                playlist.owner.display_name.unwrap_or_default(),
                playlist.owner.id,
            ),
            desc: String::new(),
            current_folder_id: 0,
            snapshot_id: playlist.snapshot_id,
        }
    }
}

impl From<rspotify::model::FullPlaylist> for Playlist {
    fn from(playlist: rspotify::model::FullPlaylist) -> Self {
        // remove HTML tags from the description
        let re = regex::Regex::new("(<.*?>|</.*?>)").expect("valid regex");
        let desc = playlist.description.unwrap_or_default();
        let desc = decode_html_entities(&re.replace_all(&desc, "")).to_string();

        Self {
            id: playlist.id,
            name: playlist.name,
            collaborative: playlist.collaborative,
            owner: (
                playlist.owner.display_name.unwrap_or_default(),
                playlist.owner.id,
            ),
            desc,
            current_folder_id: 0,
            snapshot_id: playlist.snapshot_id,
        }
    }
}

impl std::fmt::Display for Playlist {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{} • {}", self.name, self.owner.0)
    }
}

impl BidiDisplay for Playlist {}

impl From<rspotify::model::SimplifiedShow> for Show {
    fn from(show: rspotify::model::SimplifiedShow) -> Self {
        Self {
            id: show.id,
            name: show.name,
        }
    }
}

impl From<rspotify::model::FullShow> for Show {
    fn from(show: rspotify::model::FullShow) -> Self {
        Self {
            id: show.id,
            name: show.name,
        }
    }
}

impl std::fmt::Display for Show {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.name)
    }
}

impl BidiDisplay for Show {}

impl From<rspotify::model::SimplifiedEpisode> for Episode {
    fn from(episode: rspotify::model::SimplifiedEpisode) -> Self {
        Self {
            id: episode.id,
            name: episode.name,
            description: episode.description,
            duration: episode.duration.to_std().expect("valid chrono duration"),
            show: None,
            release_date: episode.release_date,
        }
    }
}

impl From<rspotify::model::FullEpisode> for Episode {
    fn from(episode: rspotify::model::FullEpisode) -> Self {
        Self {
            id: episode.id,
            name: episode.name,
            description: episode.description,
            duration: episode.duration.to_std().expect("valid chrono duration"),
            show: Some(episode.show.into()),
            release_date: episode.release_date,
        }
    }
}

impl std::fmt::Display for Episode {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        if let Some(s) = &self.show {
            write!(f, "{} • {}", self.name, s.name)
        } else {
            write!(f, "{}", self.name)
        }
    }
}

impl std::fmt::Display for PlaylistFolder {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}/", self.name)
    }
}

impl BidiDisplay for PlaylistFolder {}

impl std::fmt::Display for PlaylistFolderItem {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            PlaylistFolderItem::Playlist(playlist) => playlist.fmt(f),
            PlaylistFolderItem::Folder(folder) => folder.fmt(f),
        }
    }
}

impl BidiDisplay for PlaylistFolderItem {}

impl From<rspotify::model::category::Category> for Category {
    fn from(c: rspotify::model::category::Category) -> Self {
        Self {
            name: c.name,
            id: c.id,
        }
    }
}

impl std::fmt::Display for Category {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.name)
    }
}

impl TracksId {
    pub fn new<U, K>(uri: U, kind: K) -> Self
    where
        U: Into<String>,
        K: Into<String>,
    {
        Self {
            uri: uri.into(),
            kind: kind.into(),
        }
    }
}

impl Playback {
    /// creates new playback with a specified offset based on the current playback
    pub fn uri_offset(&self, uri: String, limit: usize) -> Self {
        match self {
            Playback::Context(id, _) => {
                Playback::Context(id.clone(), Some(rspotify::model::Offset::Uri(uri)))
            }
            Playback::URIs(ids, _) => {
                let ids = if ids.len() < limit {
                    ids.clone()
                } else {
                    let pos = ids
                        .iter()
                        .position(|id| id.uri() == uri)
                        .unwrap_or_default();
                    let l = pos.saturating_sub(limit / 2);
                    let r = std::cmp::min(l + limit, ids.len());
                    // For a list with too many tracks, to avoid payload limit when making the `start_playback`
                    // API request, we restrict the range of tracks to be played, which is based on the
                    // playing track's position (if any) and the application's limit (`app_config.tracks_playback_limit`).
                    // Related issue: https://github.com/aome510/spotify-player/issues/78
                    ids[l..r].to_vec()
                };

                Playback::URIs(ids, Some(rspotify::model::Offset::Uri(uri)))
            }
        }
    }
}

impl PlaybackMetadata {
    pub fn from_playback(p: &rspotify::model::CurrentPlaybackContext) -> Self {
        Self {
            device_name: p.device.name.clone(),
            device_id: p.device.id.clone(),
            is_playing: p.is_playing,
            volume: p.device.volume_percent,
            repeat_state: p.repeat_state,
            shuffle_state: p.shuffle_state,
            mute_state: None,
        }
    }
}

#[derive(Debug)]
pub struct Lyrics {
    pub lines: LyricsLines,
    pub source: String,
}

#[derive(Debug)]
pub enum LyricsLines {
    Synced(Vec<(chrono::Duration, String)>),
    Rich(Vec<LyricsRichLine>),
    Plain(Vec<String>),
}

#[derive(Clone, Debug)]
pub struct LyricsRichLine {
    pub start: chrono::Duration,
    pub end: Option<chrono::Duration>,
    pub text: String,
    pub parts: Vec<LyricsPart>,
}

#[derive(Clone, Debug)]
pub struct LyricsPart {
    pub start: chrono::Duration,
    pub end: Option<chrono::Duration>,
    pub text: String,
}

impl From<librespot_metadata::lyrics::Lyrics> for Lyrics {
    fn from(value: librespot_metadata::lyrics::Lyrics) -> Self {
        let sync_type = value.lyrics.sync_type;
        let mut lines = value
            .lyrics
            .lines
            .into_iter()
            .map(|l| {
                let t = chrono::Duration::milliseconds(
                    l.start_time_ms.parse::<i64>().expect("invalid number"),
                );

                (t, to_bidi_string(&l.words))
            })
            .collect::<Vec<_>>();
        lines.sort_by_key(|l| l.0);
        let lines = match sync_type {
            librespot_metadata::lyrics::SyncType::LineSynced => LyricsLines::Synced(lines),
            librespot_metadata::lyrics::SyncType::Unsynced => {
                LyricsLines::Plain(lines.into_iter().map(|(_, line)| line).collect())
            }
        };
        Self {
            lines,
            source: "Spotify".to_string(),
        }
    }
}

impl Lyrics {
    /// Parse `SimpMusic`'s rich-sync LRC variant.
    ///
    /// Each line carries a line timestamp followed by word timestamps, for
    /// example `[00:16.62]<00:16.62>word <00:16.90>word`.
    pub fn from_simp_music_rich_sync(rich_sync: &str, source: impl Into<String>) -> Option<Self> {
        let word_timestamp = regex::Regex::new(r"<\d{1,3}:\d{2}\.\d{2,3}>").ok()?;
        let mut lines = Vec::new();

        for raw_line in rich_sync.lines() {
            let raw_line = raw_line.trim();
            let Some((line_timestamp, content)) = raw_line.split_once(']') else {
                continue;
            };
            let Some(line_timestamp) = line_timestamp.strip_prefix('[') else {
                continue;
            };
            let Some(line_start) = parse_simp_music_timestamp(line_timestamp) else {
                continue;
            };

            let timestamps = word_timestamp.find_iter(content).collect::<Vec<_>>();
            if timestamps.is_empty() {
                continue;
            }

            let mut parts = Vec::new();
            for (index, timestamp) in timestamps.iter().enumerate() {
                let next_start = timestamps
                    .get(index + 1)
                    .map_or(content.len(), |next| next.start());
                let raw_text = &content[timestamp.end()..next_start];
                let mut text = html_escape::decode_html_entities(raw_text.trim()).to_string();
                if index == 0 {
                    if let Some(stripped) = text
                        .strip_prefix("v1:")
                        .or_else(|| text.strip_prefix("v2:"))
                    {
                        text = stripped.trim_start().to_string();
                    }
                }
                if text.is_empty() {
                    continue;
                }

                let timestamp = timestamp.as_str();
                let timestamp = &timestamp[1..timestamp.len() - 1];
                let Some(start) = parse_simp_music_timestamp(timestamp) else {
                    continue;
                };
                let separator = if index > 0 { " " } else { "" };
                parts.push(LyricsPart {
                    start,
                    end: None,
                    text: format!("{separator}{text}"),
                });
            }

            if parts.is_empty() {
                continue;
            }

            let text = parts
                .iter()
                .map(|part| part.text.as_str())
                .collect::<String>();
            lines.push(LyricsRichLine {
                start: line_start,
                end: None,
                text,
                parts,
            });
        }

        if lines.is_empty() {
            return None;
        }

        lines.sort_by_key(|line| line.start);
        for index in 0..lines.len() {
            if lines[index].end.is_none() {
                lines[index].end = lines.get(index + 1).map(|next| next.start);
            }
            for part_index in 0..lines[index].parts.len() {
                if lines[index].parts[part_index].end.is_none() {
                    lines[index].parts[part_index].end = lines[index]
                        .parts
                        .get(part_index + 1)
                        .map(|next| next.start)
                        .or(lines[index].end);
                }
            }
        }

        Some(Self {
            lines: LyricsLines::Rich(lines),
            source: source.into(),
        })
    }

    pub fn from_lrc(lrc: &str, source: impl Into<String>) -> Option<Self> {
        let mut lines = lrc
            .lines()
            .filter_map(|line| {
                let (timestamp, words) = line.split_once(']')?;
                let timestamp = timestamp.strip_prefix('[')?;
                let (minutes, seconds) = timestamp.split_once(':')?;
                let minutes = minutes.parse::<i64>().ok()?;
                let seconds = seconds.parse::<f64>().ok()?;
                let millis = minutes
                    .checked_mul(60_000)?
                    .checked_add((seconds * 1_000.0) as i64)?;
                let words = words.trim();
                if words.is_empty() {
                    return None;
                }
                Some((chrono::Duration::milliseconds(millis), words.to_string()))
            })
            .collect::<Vec<_>>();
        lines.sort_by_key(|line| line.0);
        (!lines.is_empty()).then(|| Self {
            lines: LyricsLines::Synced(lines),
            source: source.into(),
        })
    }

    pub fn from_plain(plain: &str, source: impl Into<String>) -> Option<Self> {
        let lines = plain
            .lines()
            .map(str::trim)
            .filter(|line| !line.is_empty())
            .map(str::to_string)
            .collect::<Vec<_>>();
        (!lines.is_empty()).then(|| Self {
            lines: LyricsLines::Plain(lines),
            source: source.into(),
        })
    }
}

fn parse_simp_music_timestamp(value: &str) -> Option<chrono::Duration> {
    let (minutes, seconds) = value.trim().split_once(':')?;
    let minutes = minutes.parse::<i64>().ok()?;
    let seconds = seconds.parse::<f64>().ok()?;
    let millis = minutes
        .checked_mul(60_000)?
        .checked_add((seconds * 1_000.0).round() as i64)?;
    Some(chrono::Duration::milliseconds(millis))
}

#[cfg(test)]
mod playback_capability_tests {
    use super::{PlaybackCapabilities, PlaybackDeviceMode, PlaybackSupport, Provider};

    #[test]
    fn spotify_exposes_selectable_remote_controls_when_playing() {
        let capabilities = PlaybackCapabilities::for_provider(Provider::Spotify, true, false);
        assert_eq!(capabilities.play_pause, PlaybackSupport::Supported);
        assert_eq!(capabilities.repeat, PlaybackSupport::Supported);
        assert_eq!(capabilities.device, PlaybackDeviceMode::Selectable);
        assert_eq!(
            PlaybackCapabilities::for_provider(Provider::Spotify, false, false).device,
            PlaybackDeviceMode::Selectable
        );
    }

    #[test]
    fn youtube_marks_local_output_and_queue_controls_honestly() {
        let idle = PlaybackCapabilities::for_provider(Provider::YouTubeMusic, false, false);
        assert_eq!(idle.volume, PlaybackSupport::Unavailable);
        assert_eq!(idle.repeat, PlaybackSupport::Unavailable);
        let no_queue = PlaybackCapabilities::for_provider(Provider::YouTubeMusic, true, false);
        assert_eq!(no_queue.repeat, PlaybackSupport::Unsupported);
        assert_eq!(no_queue.shuffle, PlaybackSupport::Unsupported);
        assert_eq!(idle.device, PlaybackDeviceMode::FixedLocal);

        let playing = PlaybackCapabilities::for_provider(Provider::YouTubeMusic, true, true);
        assert_eq!(playing.repeat, PlaybackSupport::Supported);
        assert_eq!(playing.shuffle, PlaybackSupport::Supported);
        assert_eq!(playing.device, PlaybackDeviceMode::FixedLocal);
    }
}

#[cfg(test)]
mod youtube_context_title_tests {
    use super::YouTubeContextId;

    #[test]
    fn liked_context_title_matches_loaded_context_identity() {
        assert_eq!(YouTubeContextId::LikedTracks.title(), "Liked Music");
    }
}

#[cfg(test)]
mod lyrics_tests {
    use super::{Lyrics, LyricsLines};

    #[test]
    fn parses_lrc_lines_in_time_order() {
        let lyrics = Lyrics::from_lrc("[00:10.50]later\n[00:02.25]first", "test").unwrap();
        assert_eq!(lyrics.source, "test");
        let LyricsLines::Synced(lines) = lyrics.lines else {
            panic!("expected synchronized lyrics");
        };
        assert_eq!(lines[0].0.num_milliseconds(), 2_250);
        assert_eq!(lines[0].1, "first");
    }

    #[test]
    fn parses_plain_lyrics_without_empty_lines() {
        let lyrics = Lyrics::from_plain("one\n\ntwo", "test").unwrap();
        assert!(matches!(lyrics.lines, LyricsLines::Plain(lines) if lines == ["one", "two"]));
        assert!(Lyrics::from_plain("\n", "test").is_none());
    }

    #[test]
    fn parses_simp_music_rich_sync_words() {
        let rich_sync = "[00:01.00]<00:01.00>hello <00:01.50>world\n[00:02.00]<00:02.00>again";
        let lyrics = Lyrics::from_simp_music_rich_sync(rich_sync, "SimpMusic").unwrap();
        assert_eq!(lyrics.source, "SimpMusic");
        let LyricsLines::Rich(lines) = lyrics.lines else {
            panic!("expected rich synchronized lyrics");
        };
        assert_eq!(lines.len(), 2);
        assert_eq!(lines[0].start.num_milliseconds(), 1_000);
        assert_eq!(lines[0].end.unwrap().num_milliseconds(), 2_000);
        assert_eq!(lines[0].text, "hello world");
        assert_eq!(lines[0].parts.len(), 2);
        assert_eq!(lines[0].parts[0].text, "hello");
        assert_eq!(lines[0].parts[0].end.unwrap().num_milliseconds(), 1_500);
        assert_eq!(lines[0].parts[1].text, " world");
        assert_eq!(lines[0].parts[1].end.unwrap().num_milliseconds(), 2_000);
    }

    #[test]
    fn skips_invalid_simp_music_rich_sync_lines() {
        let rich_sync = "[offset:0]\nnot a timed line\n[00:01.00]plain";
        assert!(Lyrics::from_simp_music_rich_sync(rich_sync, "SimpMusic").is_none());
    }
}

#[cfg(test)]
mod provider_tests {
    use super::{
        MediaId, MediaKind, PlayableId, PlayableMedia, PlaylistEntryId, Provider, UnifiedPlaylist,
        UnifiedPlaylistItem, UnifiedPlaylistMetadata,
    };

    #[test]
    fn spotify_playable_media_exposes_provider_neutral_identity() {
        let id = rspotify::model::TrackId::from_id("track0000000000000000000000000001")
            .unwrap()
            .into_static();
        let media = PlayableMedia::from(PlayableId::Track(id));
        assert_eq!(media.provider(), Provider::Spotify);
        assert_eq!(media.media_id().kind, MediaKind::Track);
    }

    #[test]
    fn unified_playlist_sidecar_round_trips_provider_identity() {
        let playlist = UnifiedPlaylist {
            id: "mixed-1".to_string(),
            name: "Mixed".to_string(),
            items: vec![UnifiedPlaylistItem {
                media_id: MediaId {
                    provider: Provider::YouTubeMusic,
                    kind: MediaKind::Video,
                    raw_id: "video-1".to_string(),
                },
                title: "Video".to_string(),
                artists: "Artist".to_string(),
                duration_ms: Some(1_000),
                provider_url: None,
                ..UnifiedPlaylistItem::default()
            }],
            updated_at: 1,
            next_entry_id: 1,
        };
        let encoded = serde_json::to_string(&playlist).unwrap();
        assert_eq!(
            serde_json::from_str::<UnifiedPlaylist>(&encoded).unwrap(),
            playlist
        );
    }

    #[test]
    fn unified_youtube_item_restores_a_playable_thumbnail_url() {
        let item = UnifiedPlaylistItem {
            media_id: MediaId {
                provider: Provider::YouTubeMusic,
                kind: MediaKind::Track,
                raw_id: "video-1".to_string(),
            },
            title: "Song".to_string(),
            artists: "Artist".to_string(),
            duration_ms: None,
            provider_url: None,
            ..UnifiedPlaylistItem::default()
        };

        let PlayableMedia::YouTube(track) = item.playable_media().unwrap() else {
            panic!("expected YouTube playable media");
        };
        assert_eq!(
            track.thumbnail_url.as_deref(),
            Some("https://i.ytimg.com/vi/video-1/hqdefault.jpg")
        );
    }

    #[test]
    fn unified_playlist_snapshot_changes_with_identity_or_order_but_not_metadata() {
        let mut playlist = UnifiedPlaylist {
            id: "snapshot".to_string(),
            name: "Snapshot".to_string(),
            items: vec![UnifiedPlaylistItem {
                media_id: MediaId {
                    provider: Provider::YouTubeMusic,
                    kind: MediaKind::Track,
                    raw_id: "video-1".to_string(),
                },
                title: "One".to_string(),
                artists: "Artist".to_string(),
                duration_ms: None,
                provider_url: None,
                ..UnifiedPlaylistItem::default()
            }],
            updated_at: 1,
            next_entry_id: 1,
        };
        let original = playlist.snapshot_hash();
        playlist.items[0].title = "Changed".to_string();
        assert_eq!(original, playlist.snapshot_hash());
        playlist.items[0].entry_id = PlaylistEntryId(1);
        assert_ne!(original, playlist.snapshot_hash());
    }

    #[test]
    fn unified_playlist_snapshot_excludes_metadata_freshness() {
        let mut playlist = UnifiedPlaylist {
            id: "metadata".to_string(),
            name: "Metadata".to_string(),
            items: vec![UnifiedPlaylistItem {
                media_id: MediaId {
                    provider: Provider::Spotify,
                    kind: MediaKind::Track,
                    raw_id: "track".to_string(),
                },
                title: "Track".to_string(),
                artists: "Artist".to_string(),
                duration_ms: Some(1_000),
                provider_url: None,
                metadata: UnifiedPlaylistMetadata {
                    provenance: Some("provider".to_string()),
                    observed_at: Some(1),
                    source_identifier: Some("source".to_string()),
                    ..UnifiedPlaylistMetadata::default()
                },
                ..UnifiedPlaylistItem::default()
            }],
            updated_at: 1,
            next_entry_id: 2,
        };
        let original = playlist.snapshot_hash();
        playlist.items[0].metadata.observed_at = Some(2);
        assert_eq!(original, playlist.snapshot_hash());
        playlist.items[0].title = "Changed".to_string();
        assert_eq!(original, playlist.snapshot_hash());
        playlist.items[0].entry_id = PlaylistEntryId(9);
        assert_ne!(original, playlist.snapshot_hash());
    }

    #[test]
    fn jspf_extensions_round_trip_occurrence_contract() {
        let playlist = UnifiedPlaylist {
            id: "jspf".to_string(),
            name: "Portable".to_string(),
            items: vec![UnifiedPlaylistItem {
                entry_id: PlaylistEntryId(7),
                media_id: MediaId {
                    provider: Provider::YouTubeMusic,
                    kind: MediaKind::Video,
                    raw_id: "video".to_string(),
                },
                title: "Video".to_string(),
                artists: "Artist".to_string(),
                duration_ms: Some(62_000),
                provider_url: None,
                metadata: UnifiedPlaylistMetadata {
                    provenance: Some("import".to_string()),
                    observed_at: Some(42),
                    source_identifier: Some("https://example/video".to_string()),
                    ..UnifiedPlaylistMetadata::default()
                },
                ..UnifiedPlaylistItem::default()
            }],
            updated_at: 1,
            next_entry_id: 8,
        };
        let track = &playlist.to_jspf_value()["playlist"]["track"][0];
        let extension = &track["extension"];
        assert_eq!(track["duration"], 62_000);
        assert_eq!(extension["unified-player:entry_id"], 7);
        assert_eq!(extension["unified-player:media_kind"], "Video");
        assert_eq!(extension["unified-player:duration_unit"], "milliseconds");
        assert_eq!(extension["unified-player:metadata_provenance"], "import");
    }

    #[test]
    fn mixed_high_and_missing_ids_repair_in_source_order() {
        let item = |entry_id| UnifiedPlaylistItem {
            entry_id: PlaylistEntryId(entry_id),
            media_id: MediaId {
                provider: Provider::Spotify,
                kind: MediaKind::Track,
                raw_id: entry_id.to_string(),
            },
            title: entry_id.to_string(),
            artists: "Artist".to_string(),
            ..UnifiedPlaylistItem::default()
        };
        let mut playlist = UnifiedPlaylist {
            id: "mixed-ids".to_string(),
            name: "Mixed IDs".to_string(),
            items: vec![item(0), item(100)],
            updated_at: 0,
            next_entry_id: 1,
        };
        playlist.normalize_entry_ids().unwrap();
        assert_eq!(
            playlist
                .items
                .iter()
                .map(|item| item.entry_id.0)
                .collect::<Vec<_>>(),
            vec![101, 102]
        );
        assert_eq!(playlist.next_entry_id, 103);
    }

    #[test]
    fn allocator_fails_closed_at_u64_max() {
        let mut playlist = UnifiedPlaylist {
            id: "exhausted".to_string(),
            name: "Exhausted".to_string(),
            items: Vec::new(),
            updated_at: 0,
            next_entry_id: u64::MAX,
        };
        assert!(playlist.allocate_entry_id().is_err());
    }
}

#[cfg(test)]
mod playlist_seed_tests {
    use super::{
        Album, Artist, PlaylistDestination, PlaylistIntent, PlaylistOperationEnvelope,
        PlaylistSeedItem, PlaylistTargetKind, Provider, Track, YouTubeTrack,
    };

    #[test]
    fn spotify_seed_keeps_typed_track_identity_and_native_projection() {
        let track = Track {
            id: rspotify::model::TrackId::from_id("track0000000000000000000000000001")
                .unwrap()
                .into_static(),
            name: "Example".to_string(),
            artists: vec![Artist {
                id: rspotify::model::ArtistId::from_id("artist000000000000000000000000001")
                    .unwrap()
                    .into_static(),
                name: "Artist".to_string(),
            }],
            album: Some(Album {
                id: rspotify::model::AlbumId::from_id("album0000000000000000000000000001")
                    .unwrap()
                    .into_static(),
                release_date: "2026".to_string(),
                name: "Album".to_string(),
                artists: Vec::new(),
                typ: None,
                added_at: 0,
            }),
            duration: std::time::Duration::from_secs(62),
            explicit: false,
            added_at: 0,
        };
        let seed = PlaylistSeedItem::from_spotify_track(&track);
        assert_eq!(seed.media_id.provider, Provider::Spotify);
        assert_eq!(seed.duration_ms, Some(62_000));
        assert_eq!(seed.album.as_deref(), Some("Album"));
        assert_eq!(seed.to_spotify_track().expect("Spotify seed").id, track.id);
    }

    #[test]
    fn youtube_seed_keeps_provider_identity_duration_and_artwork() {
        let seed = PlaylistSeedItem::from_youtube_track(&YouTubeTrack {
            id: "video-1".to_string(),
            name: "Example".to_string(),
            artists: "Artist".to_string(),
            album: Some("Album".to_string()),
            duration: "1:02:03".to_string(),
            explicit: true,
            thumbnail_url: Some("https://img.example/1.jpg".to_string()),
            is_video: true,
        });
        assert_eq!(seed.media_id.provider, Provider::YouTubeMusic);
        assert_eq!(seed.duration_ms, Some(3_723_000));
        assert_eq!(
            seed.artwork_url.as_deref(),
            Some("https://img.example/1.jpg")
        );
        assert_eq!(seed.to_youtube_track().expect("YouTube seed").id, "video-1");
    }

    #[test]
    fn unified_operation_requires_matching_destination_and_intent() {
        let seed = PlaylistSeedItem {
            media_id: super::MediaId {
                provider: Provider::Spotify,
                kind: super::MediaKind::Track,
                raw_id: "track-1".to_string(),
            },
            title: "Track".to_string(),
            artists: "Artist".to_string(),
            album: None,
            duration_ms: None,
            explicit: None,
            provider_url: None,
            artwork_url: None,
            metadata_degraded: false,
            metadata_pending: false,
        };
        let envelope = PlaylistOperationEnvelope::new(
            "op-1",
            "idem-1",
            Some(Provider::Spotify),
            Some(7),
            PlaylistDestination::New {
                target: PlaylistTargetKind::Unified,
            },
            PlaylistIntent::Create { seed: vec![seed] },
            None,
        );
        assert!(envelope.validate_unified().is_ok());
    }
}

#[cfg(test)]
mod playlist_projection_tests {
    use super::*;

    fn projection() -> PlaylistProjectionState {
        PlaylistProjectionState::pending(PlaylistProjectionTarget {
            provider: Provider::YouTubeMusic,
            account_id: "account-a".to_owned(),
            account_epoch: 3,
            playlist_id: "remote".to_owned(),
        })
    }

    #[test]
    fn account_switch_detaches_only_incompatible_targets() {
        let mut link = PlaylistLink::default();
        let current = projection();
        let mut other = projection();
        other.target.account_id = "account-b".to_owned();
        link.projections = vec![current, other];

        link.detach_incompatible(Provider::YouTubeMusic, "account-a", 3);
        assert_eq!(
            link.projections[0].status,
            PlaylistProjectionStatus::Pending
        );
        assert_eq!(
            link.projections[1].status,
            PlaylistProjectionStatus::Detached
        );
    }

    #[test]
    fn acknowledged_and_unknown_operations_cannot_retry() {
        let mut projection = projection();
        assert!(projection.can_retry("operation-1"));
        assert!(projection.acknowledge_operation("operation-1"));
        assert!(!projection.acknowledge_operation("operation-1"));
        assert!(!projection.can_retry("operation-1"));

        projection.status = PlaylistProjectionStatus::OutcomeUnknown;
        assert!(!projection.can_retry("operation-2"));
    }

    #[test]
    fn acknowledged_intent_blocks_unknown_but_allows_clean_readback_probe() {
        let mut projection = projection();
        assert!(projection.can_retry_intent("sync:playlist:target:revision"));
        assert!(projection.acknowledge_intent("sync:playlist:target:revision"));
        assert!(!projection.can_retry_intent("sync:playlist:target:revision"));
        assert!(projection.can_retry_intent("sync:playlist:target:new-revision"));
        projection.status = PlaylistProjectionStatus::Clean;
        assert!(projection.can_retry_intent("sync:playlist:target:revision"));
        projection.status = PlaylistProjectionStatus::OutcomeUnknown;
        assert!(!projection.can_retry_intent("sync:playlist:target:revision"));
    }

    #[test]
    fn status_requires_preview_for_drift_and_partial_but_not_clean() {
        assert!(!PlaylistProjectionStatus::Clean.needs_preview_confirmation());
        assert!(PlaylistProjectionStatus::Drifted.needs_preview_confirmation());
        assert!(PlaylistProjectionStatus::Partial.needs_preview_confirmation());
        assert!(PlaylistProjectionStatus::OutcomeUnknown.needs_preview_confirmation());
    }
}
