#[cfg(any(feature = "streaming", test))]
use super::model::PlayableId;
use super::model::{
    AlbumId, ArtistId, ContextId, Device, MediaId, MediaKind, PlaybackMetadata, PlaylistId,
    Provider, ShowId, TracksId, YouTubePlayback,
};
use super::queue::{CustomQueue, QueueOrigin, QueuedItem, UnifiedQueue, UnifiedQueueParts};
use rspotify::model::{Id as _, PlayableItem};
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, VecDeque};
use std::io::Write as _;
use std::path::Path;

const PROVIDER_SESSIONS_FILE: &str = "playback-sessions.json";

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ProviderPlaybackSession {
    pub provider: super::Provider,
    pub media_id: Option<super::MediaId>,
    pub queue_index: Option<usize>,
    pub progress: std::time::Duration,
    pub is_playing: bool,
    pub resume_on_activate: bool,
    pub repeat: rspotify::model::RepeatState,
    pub shuffle: bool,
    pub volume: u8,
}

/// A playback update emitted by the integrated librespot player.
///
/// These events carry enough information to update the cached snapshot for
/// the track already known by the Web API. Events that can change the item
/// identity still need one REST reconciliation to obtain the full snapshot.
/// Volume changes are complete provider state updates and do not need a
/// track identity.
#[cfg(feature = "streaming")]
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SpotifyPlaybackEvent {
    VolumeChanged {
        volume: u8,
    },
    Changed {
        playable_id: PlayableId<'static>,
    },
    Playing {
        playable_id: PlayableId<'static>,
        position_ms: u32,
    },
    Paused {
        playable_id: PlayableId<'static>,
        position_ms: u32,
    },
    EndOfTrack {
        playable_id: PlayableId<'static>,
    },
}

#[derive(Clone, Debug)]
pub(crate) enum QueueDisplayItem {
    Spotify {
        item: Box<PlayableItem>,
        is_current: bool,
    },
    Unified {
        item: QueuedItem,
        is_current: bool,
    },
}

/// Borrowed queue row projection used by the UI when it only needs one row.
///
/// Keeping the provider payload borrowed avoids cloning a complete queue just
/// to render the current viewport or synchronize selection identities.
pub(crate) enum QueueDisplayItemRef<'a> {
    Spotify {
        item: &'a PlayableItem,
        is_current: bool,
    },
    Unified {
        item: &'a QueuedItem,
        is_current: bool,
    },
}

impl QueueDisplayItemRef<'_> {
    pub(crate) fn is_current(&self) -> bool {
        match self {
            Self::Spotify { is_current, .. } | Self::Unified { is_current, .. } => *is_current,
        }
    }

    pub(crate) fn media_id(&self) -> Option<MediaId> {
        match self {
            Self::Unified { item, .. } => Some(item.media.media_id()),
            Self::Spotify { item, .. } => spotify_queue_media_id(item),
        }
    }

    pub(crate) fn unified_entry_id(&self) -> Option<u64> {
        match self {
            Self::Unified { item, .. } => Some(item.entry_id),
            Self::Spotify { .. } => None,
        }
    }
}

fn spotify_queue_media_id(item: &PlayableItem) -> Option<MediaId> {
    match item {
        PlayableItem::Track(track) => track.id.as_ref().map(|id| MediaId {
            provider: Provider::Spotify,
            kind: MediaKind::Track,
            raw_id: id.id().to_owned(),
        }),
        PlayableItem::Episode(episode) => Some(MediaId {
            provider: Provider::Spotify,
            kind: MediaKind::Episode,
            raw_id: episode.id.id().to_owned(),
        }),
        PlayableItem::Unknown(_) => None,
    }
}

/// Read-only projection of the queue source that currently owns progression.
///
/// This is deliberately derived from existing playback and queue facts. It is
/// not persisted, and it does not decide control or mutation routing.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum QueueAuthority {
    LocalUnified,
    LocalProviderSession,
    SpotifyNative,
    Unavailable,
}

/// Non-secret identity carried by a Spotify native-queue read.
///
/// Results are publishable only while the same playback identity still owns
/// the Spotify-native queue surface.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct NativeQueueRefreshGuard {
    playback_uri: Option<String>,
    device_id: Option<String>,
}

impl NativeQueueRefreshGuard {
    pub(crate) fn new(playback_uri: Option<&str>, device_id: Option<&str>) -> Self {
        Self {
            playback_uri: playback_uri.map(str::to_owned),
            device_id: device_id.map(str::to_owned),
        }
    }
}

impl QueueDisplayItem {
    pub(crate) fn is_current(&self) -> bool {
        match self {
            Self::Spotify { is_current, .. } | Self::Unified { is_current, .. } => *is_current,
        }
    }

    /// Return the provider-neutral media identity when this row has one.
    /// Native local tracks and unknown playables intentionally return `None`.
    pub(crate) fn media_id(&self) -> Option<MediaId> {
        match self {
            Self::Unified { item, .. } => Some(item.media.media_id()),
            Self::Spotify { item, .. } => spotify_queue_media_id(item),
        }
    }

    /// Return the stable unified queue occurrence token for this row.
    pub(crate) fn unified_entry_id(&self) -> Option<u64> {
        match self {
            Self::Unified { item, .. } => Some(item.entry_id),
            Self::Spotify { .. } => None,
        }
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub enum YouTubePlaybackPhase {
    #[default]
    Idle,
    Resolving,
    Buffering,
    Playing,
    Paused,
    Failed(String),
}

impl YouTubePlaybackPhase {
    pub fn label(&self) -> &str {
        match self {
            Self::Idle => "Idle",
            Self::Resolving => "Resolving audio",
            Self::Buffering => "Buffering audio",
            Self::Playing => "Playing",
            Self::Paused => "Paused",
            Self::Failed(_) => "Playback failed",
        }
    }
}

#[derive(Debug, Serialize, Deserialize)]
struct PersistedProviderPlayback {
    version: u8,
    sessions: Vec<ProviderPlaybackSession>,
    youtube_playback: Option<super::YouTubePlayback>,
    #[serde(default)]
    playback_queue: Option<PersistedPlaybackQueue>,
    #[serde(default)]
    youtube_queue: Option<PersistedYouTubeQueue>,
}

#[derive(Debug, Serialize, Deserialize)]
struct PersistedYouTubeQueue {
    tracks: Vec<super::YouTubeTrack>,
    position: usize,
    repeat: rspotify::model::RepeatState,
    shuffle: bool,
}

#[derive(Debug, Serialize, Deserialize)]
struct PersistedPlaybackQueue {
    current: Option<PersistedQueuedItem>,
    history: Vec<PersistedQueuedItem>,
    user_queue: Vec<PersistedQueuedItem>,
    automatic_queue: Vec<PersistedQueuedItem>,
    automatic_original: Vec<PersistedQueuedItem>,
    repeat: rspotify::model::RepeatState,
    shuffle: bool,
}

#[derive(Debug, Serialize, Deserialize)]
struct PersistedQueuedItem {
    entry_id: u64,
    origin: QueueOrigin,
    media: PersistedPlayableMedia,
}

#[derive(Debug, Serialize, Deserialize)]
enum PersistedPlayableMedia {
    Spotify(super::MediaId),
    YouTube(super::YouTubeTrack),
}

impl From<&QueuedItem> for PersistedQueuedItem {
    fn from(item: &QueuedItem) -> Self {
        let media = match &item.media {
            super::PlayableMedia::Spotify(_) => {
                PersistedPlayableMedia::Spotify(item.media.media_id())
            }
            super::PlayableMedia::YouTube(track) => PersistedPlayableMedia::YouTube(track.clone()),
        };
        Self {
            entry_id: item.entry_id,
            origin: item.origin,
            media,
        }
    }
}

impl PersistedQueuedItem {
    fn into_queued_item(self) -> Option<QueuedItem> {
        let media = match self.media {
            PersistedPlayableMedia::YouTube(track) => super::PlayableMedia::YouTube(track),
            PersistedPlayableMedia::Spotify(media_id) => super::UnifiedPlaylistItem {
                media_id,
                title: String::new(),
                artists: String::new(),
                duration_ms: None,
                provider_url: None,
                ..super::UnifiedPlaylistItem::default()
            }
            .playable_media()?,
        };
        Some(QueuedItem {
            entry_id: self.entry_id,
            media,
            origin: self.origin,
        })
    }
}

impl From<&UnifiedQueue> for PersistedPlaybackQueue {
    fn from(queue: &UnifiedQueue) -> Self {
        let parts = queue.persistent_parts();
        Self {
            current: parts.current.as_ref().map(PersistedQueuedItem::from),
            history: parts
                .history
                .iter()
                .map(PersistedQueuedItem::from)
                .collect(),
            user_queue: parts
                .user_queue
                .iter()
                .map(PersistedQueuedItem::from)
                .collect(),
            automatic_queue: parts
                .automatic_queue
                .iter()
                .map(PersistedQueuedItem::from)
                .collect(),
            automatic_original: parts
                .automatic_original
                .iter()
                .map(PersistedQueuedItem::from)
                .collect(),
            repeat: parts.repeat,
            shuffle: parts.shuffle,
        }
    }
}

impl PersistedPlaybackQueue {
    fn into_queue(self) -> UnifiedQueue {
        UnifiedQueue::restore(UnifiedQueueParts {
            current: self.current.and_then(PersistedQueuedItem::into_queued_item),
            history: self
                .history
                .into_iter()
                .filter_map(PersistedQueuedItem::into_queued_item)
                .collect(),
            user_queue: self
                .user_queue
                .into_iter()
                .filter_map(PersistedQueuedItem::into_queued_item)
                .collect::<VecDeque<_>>(),
            automatic_queue: self
                .automatic_queue
                .into_iter()
                .filter_map(PersistedQueuedItem::into_queued_item)
                .collect::<VecDeque<_>>(),
            automatic_original: self
                .automatic_original
                .into_iter()
                .filter_map(PersistedQueuedItem::into_queued_item)
                .collect(),
            repeat: self.repeat,
            shuffle: self.shuffle,
        })
    }
}

impl ProviderPlaybackSession {
    fn new(provider: super::Provider) -> Self {
        Self {
            provider,
            media_id: None,
            queue_index: None,
            progress: std::time::Duration::ZERO,
            is_playing: false,
            resume_on_activate: false,
            repeat: rspotify::model::RepeatState::Off,
            shuffle: false,
            volume: 100,
        }
    }
}

/// Player state
#[derive(Default, Debug)]
pub struct PlayerState {
    pub devices: Vec<Device>,

    pub playback: Option<rspotify::model::CurrentPlaybackContext>,
    pub playback_last_updated_time: Option<std::time::Instant>,
    /// A buffered state to speedup the feedback of playback metadata update to user
    // Related issue: https://github.com/aome510/spotify-player/issues/109
    pub buffered_playback: Option<PlaybackMetadata>,

    pub queue: Option<rspotify::model::CurrentUserQueue>,

    /// The currently playing Tracks context (for contexts not tracked by Spotify's playback, e.g. liked/top tracks)
    pub currently_playing_tracks_id: Option<TracksId>,

    /// App-managed custom queue for full playlist/album playback.
    /// Active when the integrated librespot player is streaming and the user
    /// started playback from a track-table context.
    pub custom_queue: Option<CustomQueue>,

    pub youtube_playback: Option<YouTubePlayback>,
    pub youtube_playback_phase: YouTubePlaybackPhase,

    /// Provider currently owned by the playback coordinator. Retained provider
    /// snapshots are resumable sessions, not competing playback owners.
    pub active_playback_provider: Option<crate::config::ActiveProvider>,

    /// Non-secret account label associated with the current playback owner.
    /// The provider is stored beside it so a retained snapshot cannot borrow
    /// the label for another provider or a newly selected account.
    pub active_playback_account_provider: Option<crate::config::ActiveProvider>,
    pub active_playback_account_label: Option<String>,

    /// Single provider-neutral scheduler for context, user, and recommendation
    /// queue lanes.
    pub unified_queue: Option<UnifiedQueue>,

    /// Provider-local state retained while another playback engine is active.
    pub provider_sessions: HashMap<super::Provider, ProviderPlaybackSession>,

    /// Device id of this instance's integrated Spotify player, set when its
    /// streaming connection starts. It is not cleared on shutdown: Spotify
    /// stops reporting playback on a device once its connection is gone.
    pub integrated_device_id: Option<String>,

    /// Streaming connection whose integrated player currently holds the Spotify
    /// Connect active role, as reported by its own session events. Spotify
    /// ignores spirc commands sent to an inactive device.
    pub integrated_device_active_connection: Option<u64>,
}

impl PlayerState {
    /// Device id of the integrated player while it is the active Spotify Connect device.
    pub fn active_integrated_device_id(&self) -> Option<&str> {
        self.integrated_device_active_connection
            .and(self.integrated_device_id.as_deref())
    }

    /// Record a session event from the integrated player of `connection`.
    #[cfg(any(feature = "streaming", test))]
    pub fn apply_integrated_session_event(&mut self, connection: u64, active: bool) {
        if active {
            self.integrated_device_active_connection = Some(connection);
        } else if self.integrated_device_active_connection == Some(connection) {
            // A replaced connection can report its disconnect after the new one activated.
            self.integrated_device_active_connection = None;
        }
    }

    /// Return the provider that owns the current playback surface, independent
    /// of the provider currently selected for browsing.
    pub fn playing_provider(&self) -> Option<crate::config::ActiveProvider> {
        if let Some(provider) = self.active_playback_provider {
            return Some(provider);
        }
        // A YouTube activation claims playback before its native snapshot is
        // published. Keep controls with that pending owner instead of falling
        // through to a stale Spotify snapshot during resolution or buffering.
        if matches!(
            self.youtube_playback_phase,
            YouTubePlaybackPhase::Resolving | YouTubePlaybackPhase::Buffering
        ) {
            return Some(crate::config::ActiveProvider::YouTubeMusic);
        }
        if self
            .youtube_playback
            .as_ref()
            .is_some_and(|playback| playback.is_playing)
        {
            return Some(crate::config::ActiveProvider::YouTubeMusic);
        }
        if self
            .current_playback()
            .as_ref()
            .is_some_and(|playback| playback.is_playing)
        {
            return Some(crate::config::ActiveProvider::Spotify);
        }
        if self.youtube_playback.is_some() {
            return Some(crate::config::ActiveProvider::YouTubeMusic);
        }
        self.current_playback()
            .is_some()
            .then_some(crate::config::ActiveProvider::Spotify)
    }

    /// The 0-100 volume of `provider`'s playback, if it reports one.
    pub fn playback_volume(&self, provider: crate::config::ActiveProvider) -> Option<u8> {
        let volume = match provider {
            crate::config::ActiveProvider::Spotify => self.buffered_playback.as_ref()?.volume?,
            crate::config::ActiveProvider::YouTubeMusic => {
                u32::from(self.youtube_playback.as_ref()?.volume)
            }
        };
        Some(u8::try_from(volume.min(100)).unwrap_or(100))
    }

    /// Use the browsing provider only when no playback owner is known.
    pub fn effective_playback_provider(
        &self,
        browsing_provider: crate::config::ActiveProvider,
    ) -> crate::config::ActiveProvider {
        self.playing_provider().unwrap_or(browsing_provider)
    }

    /// Return the normalized control snapshot for one provider without
    /// creating a second playback owner or inferring support in the UI.
    pub fn playback_capabilities(&self, provider: super::Provider) -> super::PlaybackCapabilities {
        let has_playback = match provider {
            super::Provider::Spotify => self.current_playback().is_some(),
            super::Provider::YouTubeMusic => self.youtube_playback.is_some(),
        };
        super::PlaybackCapabilities::for_provider(
            provider,
            has_playback,
            self.unified_queue
                .as_ref()
                .is_some_and(|queue| queue.current().is_some()),
        )
    }

    pub(crate) fn queue_authority(&self) -> QueueAuthority {
        let Some(playing_provider) = self.playing_provider() else {
            return QueueAuthority::Unavailable;
        };
        let unified_matches_playback = self
            .unified_queue
            .as_ref()
            .and_then(UnifiedQueue::current)
            .is_some_and(|current| {
                matches!(
                    (current.provider(), playing_provider),
                    (Provider::Spotify, crate::config::ActiveProvider::Spotify)
                        | (
                            Provider::YouTubeMusic,
                            crate::config::ActiveProvider::YouTubeMusic
                        )
                )
            });
        if unified_matches_playback {
            return QueueAuthority::LocalUnified;
        }

        match playing_provider {
            crate::config::ActiveProvider::Spotify => QueueAuthority::SpotifyNative,
            crate::config::ActiveProvider::YouTubeMusic => QueueAuthority::LocalProviderSession,
        }
    }

    pub(crate) fn native_queue_refresh_guard(&self) -> Option<NativeQueueRefreshGuard> {
        (self.queue_authority() == QueueAuthority::SpotifyNative).then(|| {
            NativeQueueRefreshGuard::new(
                self.currently_playing()
                    .and_then(PlayableItem::id)
                    .map(|id| id.uri())
                    .as_deref(),
                self.buffered_playback
                    .as_ref()
                    .and_then(|playback| playback.device_id.clone())
                    .or_else(|| {
                        self.playback
                            .as_ref()
                            .and_then(|playback| playback.device.id.clone())
                    })
                    .as_deref(),
            )
        })
    }

    pub(crate) fn automatic_native_queue_refresh_guard(&self) -> Option<NativeQueueRefreshGuard> {
        let guard = self.native_queue_refresh_guard()?;
        let Some(queue) = self.queue.as_ref() else {
            return Some(guard);
        };
        let queue_uri = queue
            .currently_playing
            .as_ref()
            .and_then(PlayableItem::id)
            .map(|id| id.uri());
        match (guard.playback_uri.as_deref(), queue_uri.as_deref()) {
            (Some(playback_uri), Some(queue_uri)) if playback_uri != queue_uri => Some(guard),
            _ => None,
        }
    }

    pub(crate) fn native_queue_refresh_guard_is_current(
        &self,
        guard: &NativeQueueRefreshGuard,
    ) -> bool {
        self.native_queue_refresh_guard().as_ref() == Some(guard)
    }

    pub(crate) fn authoritative_unified_queue_instance_id(
        &self,
    ) -> Option<super::UnifiedQueueInstanceId> {
        (self.queue_authority() == QueueAuthority::LocalUnified)
            .then(|| self.unified_queue.as_ref().map(UnifiedQueue::instance_id))
            .flatten()
    }

    /// Release app-owned queue state before Spotify's provider-native context
    /// becomes the source of playback progression.
    pub(crate) fn clear_local_queue_for_native_playback(&mut self) {
        self.unified_queue = None;
        self.queue = None;
    }

    pub(crate) fn queue_display_items(&self) -> Vec<QueueDisplayItem> {
        match self.queue_authority() {
            QueueAuthority::LocalUnified => {
                return self
                    .unified_queue
                    .as_ref()
                    .into_iter()
                    .flat_map(UnifiedQueue::display_items)
                    .enumerate()
                    .map(|(index, item)| QueueDisplayItem::Unified {
                        item: item.clone(),
                        is_current: index == 0,
                    })
                    .collect();
            }
            QueueAuthority::LocalProviderSession | QueueAuthority::Unavailable => {
                return Vec::new();
            }
            QueueAuthority::SpotifyNative => {}
        }

        let mut items = Vec::new();
        let current = self.currently_playing().cloned().or_else(|| {
            self.queue
                .as_ref()
                .and_then(|queue| queue.currently_playing.clone())
        });
        if let Some(item) = current {
            items.push(QueueDisplayItem::Spotify {
                item: Box::new(item),
                is_current: true,
            });
        }
        if let Some(queue) = self.queue.as_ref() {
            items.extend(
                queue
                    .queue
                    .iter()
                    .cloned()
                    .map(|item| QueueDisplayItem::Spotify {
                        item: Box::new(item),
                        is_current: false,
                    }),
            );
        }
        items
    }

    /// Number of rows exposed by the queue surfaces without cloning them.
    pub(crate) fn queue_display_item_count(&self) -> usize {
        match self.queue_authority() {
            QueueAuthority::LocalUnified => self
                .unified_queue
                .as_ref()
                .map_or(0, UnifiedQueue::display_item_count),
            QueueAuthority::LocalProviderSession | QueueAuthority::Unavailable => 0,
            QueueAuthority::SpotifyNative => {
                let current = self.currently_playing().is_some()
                    || self
                        .queue
                        .as_ref()
                        .and_then(|queue| queue.currently_playing.as_ref())
                        .is_some();
                usize::from(current) + self.queue.as_ref().map_or(0, |queue| queue.queue.len())
            }
        }
    }

    /// Return one borrowed queue row without materializing or cloning its
    /// provider payload.
    pub(crate) fn queue_display_item_ref(&self, index: usize) -> Option<QueueDisplayItemRef<'_>> {
        match self.queue_authority() {
            QueueAuthority::LocalUnified => {
                self.unified_queue
                    .as_ref()?
                    .display_item(index)
                    .map(|item| QueueDisplayItemRef::Unified {
                        item,
                        is_current: index == 0,
                    })
            }
            QueueAuthority::LocalProviderSession | QueueAuthority::Unavailable => None,
            QueueAuthority::SpotifyNative => {
                let current = self.currently_playing().or_else(|| {
                    self.queue
                        .as_ref()
                        .and_then(|queue| queue.currently_playing.as_ref())
                });
                let queue_index = if current.is_some() {
                    if index == 0 {
                        return current.map(|item| QueueDisplayItemRef::Spotify {
                            item,
                            is_current: true,
                        });
                    }
                    index.saturating_sub(1)
                } else {
                    index
                };
                self.queue.as_ref()?.queue.get(queue_index).map(|item| {
                    QueueDisplayItemRef::Spotify {
                        item,
                        is_current: false,
                    }
                })
            }
        }
    }

    pub(crate) fn enqueue_unified_user_items<I>(
        &mut self,
        current: Option<super::PlayableMedia>,
        items: I,
    ) where
        I: IntoIterator<Item = super::PlayableMedia>,
    {
        let queue = self.unified_queue.get_or_insert_with(|| {
            current.map_or_else(UnifiedQueue::empty, |current| {
                UnifiedQueue::new(vec![current], 0)
            })
        });
        queue.enqueue_user(items);
    }

    /// Apply an integrated Spotify player event to the cached Web API
    /// snapshot. Returns `true` when the event is incomplete, refers to a
    /// different item, or ends a track and therefore needs one REST
    /// reconciliation.
    #[cfg(feature = "streaming")]
    pub fn apply_spotify_playback_event(&mut self, event: &SpotifyPlaybackEvent) -> bool {
        if let SpotifyPlaybackEvent::VolumeChanged { volume } = event {
            let volume = (*volume).min(100);
            if let Some(playback) = self.playback.as_mut() {
                playback.device.volume_percent = Some(u32::from(volume));
            }
            let had_buffered_playback = self.buffered_playback.is_some();
            if let Some(playback) = self.buffered_playback.as_mut() {
                if volume == 0 {
                    playback.volume = Some(playback.mute_state.unwrap_or_default());
                } else {
                    playback.volume = Some(u32::from(volume));
                    playback.mute_state = None;
                }
            }
            self.refresh_spotify_session();
            if !had_buffered_playback {
                self.provider_sessions
                    .entry(Provider::Spotify)
                    .or_insert_with(|| ProviderPlaybackSession::new(Provider::Spotify))
                    .volume = volume;
            }
            return false;
        }

        let playable_id = match event {
            SpotifyPlaybackEvent::VolumeChanged { .. } => unreachable!("handled above"),
            SpotifyPlaybackEvent::Changed { .. } => return true,
            SpotifyPlaybackEvent::Playing { playable_id, .. }
            | SpotifyPlaybackEvent::Paused { playable_id, .. }
            | SpotifyPlaybackEvent::EndOfTrack { playable_id } => playable_id,
        };

        let current_matches = self
            .currently_playing()
            .and_then(PlayableItem::id)
            .is_some_and(|current_id| current_id.uri() == playable_id.uri());
        if !current_matches {
            return true;
        }

        let now = chrono::Utc::now();
        let is_playing = match event {
            SpotifyPlaybackEvent::Playing { position_ms, .. } => {
                if let Some(playback) = self.playback.as_mut() {
                    playback.progress =
                        Some(chrono::Duration::milliseconds(i64::from(*position_ms)));
                    playback.is_playing = true;
                    playback.timestamp = now;
                } else {
                    return true;
                }
                true
            }
            SpotifyPlaybackEvent::Paused { position_ms, .. } => {
                if let Some(playback) = self.playback.as_mut() {
                    playback.progress =
                        Some(chrono::Duration::milliseconds(i64::from(*position_ms)));
                    playback.is_playing = false;
                    playback.timestamp = now;
                } else {
                    return true;
                }
                false
            }
            SpotifyPlaybackEvent::EndOfTrack { .. } => {
                if let Some(playback) = self.playback.as_mut() {
                    playback.is_playing = false;
                    playback.timestamp = now;
                } else {
                    return true;
                }
                false
            }
            SpotifyPlaybackEvent::VolumeChanged { .. } | SpotifyPlaybackEvent::Changed { .. } => {
                unreachable!("handled above")
            }
        };

        self.playback_last_updated_time = Some(std::time::Instant::now());
        if let Some(playback) = self.buffered_playback.as_mut() {
            playback.is_playing = is_playing;
        }
        self.refresh_spotify_session();

        matches!(event, SpotifyPlaybackEvent::EndOfTrack { .. })
    }

    /// Publish an acknowledged Spotify seek without waiting for REST
    /// reconciliation. The instant is reset so the normal progress estimator
    /// continues from the requested position on the next frame.
    pub fn apply_spotify_seek(&mut self, position: chrono::Duration) -> bool {
        if position < chrono::Duration::zero() {
            return false;
        }
        let Some(playback) = self.playback.as_mut() else {
            return false;
        };
        playback.progress = Some(position);
        playback.timestamp = chrono::Utc::now();
        self.playback_last_updated_time = Some(std::time::Instant::now());
        self.refresh_spotify_session();
        true
    }

    pub fn refresh_spotify_session(&mut self) {
        let playback = self.current_playback();
        let session = self
            .provider_sessions
            .entry(super::Provider::Spotify)
            .or_insert_with(|| ProviderPlaybackSession::new(super::Provider::Spotify));
        debug_assert_eq!(session.provider, super::Provider::Spotify);
        session.media_id = playback
            .as_ref()
            .and_then(|playback| match playback.item.as_ref()? {
                rspotify::model::PlayableItem::Track(track) => {
                    track.id.as_ref().map(|id| super::MediaId {
                        provider: super::Provider::Spotify,
                        kind: super::MediaKind::Track,
                        raw_id: id.uri(),
                    })
                }
                rspotify::model::PlayableItem::Episode(episode) => Some(super::MediaId {
                    provider: super::Provider::Spotify,
                    kind: super::MediaKind::Episode,
                    raw_id: episode.id.uri(),
                }),
                rspotify::model::PlayableItem::Unknown(_) => None,
            });
        session.progress = playback
            .as_ref()
            .and_then(|playback| playback.progress)
            .and_then(|progress| progress.to_std().ok())
            .unwrap_or_default();
        session.is_playing = playback
            .as_ref()
            .is_some_and(|playback| playback.is_playing);
        session.repeat = playback
            .as_ref()
            .map_or(rspotify::model::RepeatState::Off, |playback| {
                playback.repeat_state
            });
        session.shuffle = playback
            .as_ref()
            .is_some_and(|playback| playback.shuffle_state);
        session.volume = playback
            .as_ref()
            .and_then(|playback| playback.device.volume_percent)
            .map_or(session.volume, |volume| volume.min(100) as u8);
    }

    pub fn remember_spotify_activation_state(&mut self) {
        self.refresh_spotify_session();
        if let Some(session) = self.provider_sessions.get_mut(&super::Provider::Spotify) {
            session.resume_on_activate = session.is_playing;
        }
    }

    pub fn load_provider_sessions(config_folder: &Path) -> Self {
        let path = config_folder.join(PROVIDER_SESSIONS_FILE);
        let Ok(data) = std::fs::read(&path) else {
            return Self::default();
        };
        let persisted: PersistedProviderPlayback = match serde_json::from_slice(&data) {
            Ok(persisted) => persisted,
            Err(err) => {
                crate::observability::log_safe_error!(
                    warn,
                    crate::observability::DiagnosticCode::PROVIDER_SESSION_RESTORE_FAILED,
                    crate::observability::ErrorCategory::Storage,
                    &err,
                    "Unable to restore provider playback sessions"
                );
                return Self::default();
            }
        };
        if !matches!(persisted.version, 1 | 2) {
            tracing::warn!(
                "Ignoring unsupported provider playback session version {}",
                persisted.version
            );
            return Self::default();
        }

        let mut sessions = persisted
            .sessions
            .into_iter()
            .map(|mut session| {
                session.is_playing = false;
                session.resume_on_activate = false;
                (session.provider, session)
            })
            .collect::<HashMap<_, _>>();
        for provider in [super::Provider::Spotify, super::Provider::YouTubeMusic] {
            sessions
                .entry(provider)
                .or_insert_with(|| ProviderPlaybackSession::new(provider));
        }
        // YouTube playback snapshots describe a browser/player transport that
        // belonged to the previous process. Restoring one here creates a
        // phantom playback owner because that transport is not alive anymore;
        // the next YouTube track must establish a fresh playable session.
        let youtube_playback_phase = YouTubePlaybackPhase::Idle;
        let unified_queue = persisted
            .playback_queue
            .map(PersistedPlaybackQueue::into_queue)
            .or_else(|| {
                persisted.youtube_queue.map(|queue| {
                    let mut scheduler = UnifiedQueue::new(
                        queue
                            .tracks
                            .into_iter()
                            .map(super::PlayableMedia::YouTube)
                            .collect(),
                        queue.position,
                    );
                    let mut parts = scheduler.persistent_parts();
                    parts.repeat = queue.repeat;
                    parts.shuffle = queue.shuffle;
                    scheduler = UnifiedQueue::restore(parts);
                    scheduler
                })
            });
        Self {
            provider_sessions: sessions,
            youtube_playback: None,
            youtube_playback_phase,
            unified_queue,
            ..Self::default()
        }
    }

    pub fn save_provider_sessions(&self, config_folder: &Path) -> anyhow::Result<()> {
        let mut youtube_playback = self.youtube_playback.clone();
        if let Some(playback) = &mut youtube_playback {
            playback.is_playing = false;
        }
        let persisted = PersistedProviderPlayback {
            version: 2,
            sessions: self.provider_sessions.values().cloned().collect(),
            youtube_playback,
            playback_queue: self
                .unified_queue
                .as_ref()
                .map(PersistedPlaybackQueue::from),
            youtube_queue: None,
        };
        let data = serde_json::to_vec_pretty(&persisted)?;
        std::fs::create_dir_all(config_folder)?;
        let path = config_folder.join(PROVIDER_SESSIONS_FILE);
        atomicwrites::AtomicFile::new(path, atomicwrites::AllowOverwrite)
            .write(|file| file.write_all(&data))?;
        Ok(())
    }

    pub fn refresh_youtube_session(&mut self) {
        let playback = self.youtube_playback.as_ref();
        let queue = self.unified_queue.as_ref();
        let session = self
            .provider_sessions
            .entry(super::Provider::YouTubeMusic)
            .or_insert_with(|| ProviderPlaybackSession::new(super::Provider::YouTubeMusic));
        debug_assert_eq!(session.provider, super::Provider::YouTubeMusic);
        session.media_id = playback.map(|playback| super::MediaId {
            provider: super::Provider::YouTubeMusic,
            kind: if playback.track.is_video {
                super::MediaKind::Video
            } else {
                super::MediaKind::Track
            },
            raw_id: playback.track.id.clone(),
        });
        session.queue_index = queue.map(UnifiedQueue::position);
        session.progress = playback
            .map(|playback| playback.progress)
            .unwrap_or_default();
        session.is_playing = playback.is_some_and(|playback| playback.is_playing);
        session.repeat = queue.map_or(rspotify::model::RepeatState::Off, UnifiedQueue::repeat);
        session.shuffle = queue.is_some_and(UnifiedQueue::is_shuffled);
        session.volume = playback.map_or(session.volume, |playback| playback.volume);
    }

    pub fn remember_youtube_activation_state(&mut self) -> bool {
        self.refresh_youtube_session();
        let session = self
            .provider_sessions
            .get_mut(&super::Provider::YouTubeMusic)
            .expect("refresh creates YouTube session");
        session.resume_on_activate = session.is_playing;
        session.resume_on_activate
    }

    pub fn youtube_should_resume_on_activate(&self) -> bool {
        self.provider_sessions
            .get(&super::Provider::YouTubeMusic)
            .is_some_and(|session| session.resume_on_activate)
    }

    /// Get the current playback
    ///
    /// # Note
    /// Because playback metadata stored inside the player state is buffered,
    /// the returned playback is estimated based on the available data.
    pub fn current_playback(&self) -> Option<rspotify::model::CurrentPlaybackContext> {
        let mut playback = self.playback.clone()?;

        // update the playback's progress based on the `playback_last_updated_time`
        playback.progress = playback.progress.map(|d| {
            d + if playback.is_playing {
                chrono::Duration::from_std(self.playback_last_updated_time.unwrap().elapsed())
                    .unwrap()
            } else {
                chrono::Duration::zero()
            }
        });

        // update the playback's metadata based on the `buffered_playback` metadata
        if let Some(ref p) = self.buffered_playback {
            playback.device.name.clone_from(&p.device_name);
            playback.device.id.clone_from(&p.device_id);
            playback.is_playing = p.is_playing;
            playback.device.volume_percent = p.volume;
            playback.repeat_state = p.repeat_state;
            playback.shuffle_state = p.shuffle_state;
        }

        Some(playback)
    }

    pub fn currently_playing(&self) -> Option<&rspotify::model::PlayableItem> {
        self.playback.as_ref().and_then(|p| p.item.as_ref())
    }

    pub fn playback_progress(&self) -> Option<chrono::Duration> {
        match self.playback {
            None => None,
            Some(ref playback) => {
                let progress = playback.progress.unwrap()
                    + if playback.is_playing {
                        chrono::Duration::from_std(
                            self.playback_last_updated_time.unwrap().elapsed(),
                        )
                        .ok()?
                    } else {
                        chrono::Duration::zero()
                    };
                Some(progress)
            }
        }
    }

    pub fn playing_context_id(&self) -> Option<ContextId> {
        match self.playback {
            Some(ref playback) => match playback.context {
                Some(ref context) => {
                    let uri = crate::utils::parse_uri(&context.uri);
                    match context._type {
                        rspotify::model::Type::Playlist => Some(ContextId::Playlist(
                            PlaylistId::from_uri(&uri).ok()?.into_static(),
                        )),
                        rspotify::model::Type::Album => Some(ContextId::Album(
                            AlbumId::from_uri(&uri).ok()?.into_static(),
                        )),
                        rspotify::model::Type::Artist => Some(ContextId::Artist(
                            ArtistId::from_uri(&uri).ok()?.into_static(),
                        )),
                        rspotify::model::Type::Show => {
                            Some(ContextId::Show(ShowId::from_uri(&uri).ok()?.into_static()))
                        }
                        _ => None,
                    }
                }
                None => self
                    .custom_queue
                    .as_ref()
                    .and_then(|q| q.source_context().cloned())
                    .or_else(|| {
                        self.currently_playing_tracks_id
                            .clone()
                            .map(ContextId::Tracks)
                    }),
            },
            None => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn spotify_playback(
        track_id: &str,
        is_playing: bool,
    ) -> rspotify::model::CurrentPlaybackContext {
        rspotify::model::CurrentPlaybackContext {
            device: rspotify::model::Device {
                id: Some("integrated-device".to_owned()),
                is_active: true,
                is_private_session: false,
                is_restricted: false,
                name: "unified-player".to_owned(),
                _type: rspotify::model::DeviceType::Computer,
                volume_percent: Some(75),
            },
            repeat_state: rspotify::model::RepeatState::Off,
            shuffle_state: false,
            context: None,
            timestamp: chrono::Utc::now(),
            progress: Some(chrono::Duration::seconds(1)),
            is_playing,
            item: Some(rspotify::model::PlayableItem::Unknown(serde_json::json!({
                "id": track_id,
                "type": "track",
            }))),
            currently_playing_type: rspotify::model::CurrentlyPlayingType::Track,
            actions: rspotify::model::Actions::default(),
        }
    }

    fn spotify_playable_id(track_id: &str) -> PlayableId<'static> {
        PlayableId::Track(
            rspotify::model::TrackId::from_id(track_id)
                .unwrap()
                .into_static(),
        )
    }

    #[test]
    #[cfg(feature = "streaming")]
    fn matching_spotify_events_update_cached_playback_without_refresh() {
        let track_id = "4iV5W9uYEdYUVa79Axb7Rh";
        let mut state = PlayerState {
            playback: Some(spotify_playback(track_id, true)),
            buffered_playback: Some(PlaybackMetadata {
                device_name: "unified-player".to_owned(),
                device_id: Some("integrated-device".to_owned()),
                volume: Some(75),
                is_playing: true,
                repeat_state: rspotify::model::RepeatState::Off,
                shuffle_state: false,
                mute_state: None,
            }),
            ..PlayerState::default()
        };

        let event = SpotifyPlaybackEvent::Paused {
            playable_id: spotify_playable_id(track_id),
            position_ms: 4_200,
        };

        assert!(!state.apply_spotify_playback_event(&event));
        let playback = state.playback.as_ref().unwrap();
        assert_eq!(
            playback.progress,
            Some(chrono::Duration::milliseconds(4_200))
        );
        assert!(!playback.is_playing);
        assert!(!state.buffered_playback.as_ref().unwrap().is_playing);
        assert_eq!(
            state
                .provider_sessions
                .get(&Provider::Spotify)
                .unwrap()
                .progress,
            std::time::Duration::from_millis(4_200)
        );

        assert!(!state
            .apply_spotify_playback_event(&SpotifyPlaybackEvent::VolumeChanged { volume: 42 }));
        assert_eq!(
            state.playback.as_ref().unwrap().device.volume_percent,
            Some(42)
        );
        assert_eq!(state.buffered_playback.as_ref().unwrap().volume, Some(42));
        assert_eq!(
            state
                .provider_sessions
                .get(&Provider::Spotify)
                .unwrap()
                .volume,
            42
        );

        assert!(
            !state.apply_spotify_playback_event(&SpotifyPlaybackEvent::Playing {
                playable_id: spotify_playable_id(track_id),
                position_ms: 6_300,
            })
        );
        assert_eq!(
            state.playback.as_ref().unwrap().progress,
            Some(chrono::Duration::milliseconds(6_300))
        );
        assert!(state.playback.as_ref().unwrap().is_playing);
        assert!(state.buffered_playback.as_ref().unwrap().is_playing);
    }

    #[test]
    #[cfg(feature = "streaming")]
    fn matching_spotify_volume_zero_preserves_local_mute_overlay() {
        let track_id = "4iV5W9uYEdYUVa79Axb7Rh";
        let mut state = PlayerState {
            playback: Some(spotify_playback(track_id, true)),
            buffered_playback: Some(PlaybackMetadata {
                device_name: "unified-player".to_owned(),
                device_id: Some("integrated-device".to_owned()),
                volume: Some(75),
                is_playing: true,
                repeat_state: rspotify::model::RepeatState::Off,
                shuffle_state: false,
                mute_state: Some(75),
            }),
            playback_last_updated_time: Some(std::time::Instant::now()),
            ..PlayerState::default()
        };

        assert!(
            !state.apply_spotify_playback_event(&SpotifyPlaybackEvent::VolumeChanged { volume: 0 })
        );
        assert_eq!(state.buffered_playback.as_ref().unwrap().volume, Some(75));
        assert_eq!(
            state.buffered_playback.as_ref().unwrap().mute_state,
            Some(75)
        );
        assert_eq!(
            state.playback.as_ref().unwrap().device.volume_percent,
            Some(0)
        );

        assert!(!state
            .apply_spotify_playback_event(&SpotifyPlaybackEvent::VolumeChanged { volume: 50 }));
        assert_eq!(state.buffered_playback.as_ref().unwrap().volume, Some(50));
        assert_eq!(state.buffered_playback.as_ref().unwrap().mute_state, None);
    }

    #[test]
    fn integrated_device_is_active_only_between_its_session_events() {
        let mut player = PlayerState {
            integrated_device_id: Some("integrated".to_owned()),
            ..PlayerState::default()
        };
        assert_eq!(player.active_integrated_device_id(), None);

        player.apply_integrated_session_event(1, true);
        assert_eq!(player.active_integrated_device_id(), Some("integrated"));

        player.apply_integrated_session_event(1, false);
        assert_eq!(player.active_integrated_device_id(), None);
    }

    #[test]
    fn replaced_connection_disconnect_keeps_its_successor_active() {
        let mut player = PlayerState {
            integrated_device_id: Some("integrated".to_owned()),
            ..PlayerState::default()
        };
        player.apply_integrated_session_event(1, true);
        player.apply_integrated_session_event(2, true);
        player.apply_integrated_session_event(1, false);
        assert_eq!(player.active_integrated_device_id(), Some("integrated"));
    }

    #[test]
    fn current_playback_projects_an_external_device_from_buffered_state() {
        let mut state = PlayerState {
            playback: Some(spotify_playback("track", true)),
            buffered_playback: Some(PlaybackMetadata {
                device_name: "Phone".to_owned(),
                device_id: Some("phone-device".to_owned()),
                volume: Some(60),
                is_playing: true,
                repeat_state: rspotify::model::RepeatState::Off,
                shuffle_state: false,
                mute_state: None,
            }),
            playback_last_updated_time: Some(std::time::Instant::now()),
            ..PlayerState::default()
        };

        let playback = state.current_playback().expect("playback snapshot");
        assert_eq!(playback.device.name, "Phone");
        assert_eq!(playback.device.id.as_deref(), Some("phone-device"));
        assert_eq!(playback.device.volume_percent, Some(60));

        state.buffered_playback = None;
        let raw_playback = state.current_playback().expect("raw playback snapshot");
        assert_eq!(raw_playback.device.name, "unified-player");
        assert_eq!(raw_playback.device.id.as_deref(), Some("integrated-device"));
    }

    #[test]
    fn acknowledged_spotify_seek_resets_local_progress_timebase() {
        let target = chrono::Duration::seconds(42);
        let mut state = PlayerState {
            playback: Some(spotify_playback("4iV5W9uYEdYUVa79Axb7Rh", true)),
            ..PlayerState::default()
        };

        assert!(state.apply_spotify_seek(target));
        let projected = state.playback_progress().unwrap();
        assert!(projected >= target);
        assert!(projected < target + chrono::Duration::seconds(1));
        assert_eq!(
            state
                .provider_sessions
                .get(&Provider::Spotify)
                .unwrap()
                .progress
                .as_secs(),
            42
        );

        assert!(!state.apply_spotify_seek(chrono::Duration::seconds(-1)));
        assert!(!PlayerState::default().apply_spotify_seek(chrono::Duration::zero()));
    }

    #[test]
    #[cfg(feature = "streaming")]
    fn changed_or_unknown_spotify_events_request_one_reconciliation() {
        let track_id = "4iV5W9uYEdYUVa79Axb7Rh";
        let mut state = PlayerState {
            playback: Some(spotify_playback(track_id, true)),
            ..PlayerState::default()
        };
        let same_id = spotify_playable_id(track_id);
        let other_id = spotify_playable_id("5iKndSu1XI74U2OZePzP8L");

        assert!(
            state.apply_spotify_playback_event(&SpotifyPlaybackEvent::Changed {
                playable_id: same_id.clone(),
            })
        );
        assert!(
            state.apply_spotify_playback_event(&SpotifyPlaybackEvent::Playing {
                playable_id: other_id,
                position_ms: 2_000,
            })
        );
    }

    #[test]
    #[cfg(feature = "streaming")]
    fn matching_end_of_track_updates_pause_state_and_requests_next_snapshot() {
        let track_id = "4iV5W9uYEdYUVa79Axb7Rh";
        let mut state = PlayerState {
            playback: Some(spotify_playback(track_id, true)),
            ..PlayerState::default()
        };

        assert!(
            state.apply_spotify_playback_event(&SpotifyPlaybackEvent::EndOfTrack {
                playable_id: spotify_playable_id(track_id),
            })
        );
        assert!(!state.playback.as_ref().unwrap().is_playing);
    }

    #[test]
    fn effective_playback_provider_follows_playback_not_browsing_selection() {
        let mut state = PlayerState::default();
        assert_eq!(
            state.effective_playback_provider(crate::config::ActiveProvider::YouTubeMusic),
            crate::config::ActiveProvider::YouTubeMusic
        );

        state.youtube_playback = Some(YouTubePlayback {
            track: super::super::model::YouTubeTrack {
                id: "video".to_owned(),
                name: "title".to_owned(),
                artists: "artist".to_owned(),
                album: None,
                duration: "1:00".to_owned(),
                explicit: false,
                thumbnail_url: None,
                is_video: false,
            },
            is_playing: false,
            progress: std::time::Duration::ZERO,
            volume: 100,
            mute_state: None,
            route: Default::default(),
        });
        assert_eq!(
            state.effective_playback_provider(crate::config::ActiveProvider::Spotify),
            crate::config::ActiveProvider::YouTubeMusic
        );
    }

    #[test]
    fn coordinator_owner_wins_when_spotify_and_youtube_snapshots_are_paused() {
        let state = PlayerState {
            playback: Some(spotify_playback("spotify-track", false)),
            youtube_playback: Some(YouTubePlayback {
                track: super::super::model::YouTubeTrack {
                    id: "youtube-track".to_owned(),
                    name: "title".to_owned(),
                    artists: "artist".to_owned(),
                    album: None,
                    duration: "1:00".to_owned(),
                    explicit: false,
                    thumbnail_url: None,
                    is_video: false,
                },
                is_playing: false,
                progress: std::time::Duration::ZERO,
                volume: 100,
                mute_state: None,
                route: Default::default(),
            }),
            active_playback_provider: Some(crate::config::ActiveProvider::Spotify),
            ..PlayerState::default()
        };

        assert_eq!(
            state.effective_playback_provider(crate::config::ActiveProvider::YouTubeMusic),
            crate::config::ActiveProvider::Spotify
        );
    }

    #[test]
    fn pending_youtube_activation_owns_controls_before_snapshot() {
        let mut state = PlayerState {
            playback: Some(spotify_playback("spotify-track", true)),
            youtube_playback_phase: YouTubePlaybackPhase::Resolving,
            ..PlayerState::default()
        };

        assert_eq!(
            state.playing_provider(),
            Some(crate::config::ActiveProvider::YouTubeMusic)
        );
        assert_eq!(
            state.effective_playback_provider(crate::config::ActiveProvider::Spotify),
            crate::config::ActiveProvider::YouTubeMusic
        );

        state.youtube_playback_phase = YouTubePlaybackPhase::Buffering;
        assert_eq!(
            state.playing_provider(),
            Some(crate::config::ActiveProvider::YouTubeMusic)
        );
    }

    fn temporary_folder(name: &str) -> std::path::PathBuf {
        std::env::temp_dir().join(format!(
            "unified-player-{name}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ))
    }

    fn track_with_id(id: &str) -> super::super::YouTubeTrack {
        super::super::YouTubeTrack {
            id: id.to_string(),
            name: id.to_string(),
            artists: "Artist".to_string(),
            album: Some("Album".to_string()),
            duration: "3:20".to_string(),
            explicit: false,
            thumbnail_url: None,
            is_video: false,
        }
    }

    fn track() -> super::super::YouTubeTrack {
        track_with_id("video-id")
    }

    fn youtube_playback(id: &str, is_playing: bool) -> YouTubePlayback {
        YouTubePlayback {
            track: track_with_id(id),
            is_playing,
            progress: std::time::Duration::ZERO,
            volume: 100,
            mute_state: None,
            route: Default::default(),
        }
    }

    #[test]
    fn persisted_youtube_playback_is_not_restored_on_startup() {
        let folder = temporary_folder("provider-sessions");
        let mut state = PlayerState {
            youtube_playback: Some(super::super::YouTubePlayback {
                track: track(),
                is_playing: true,
                progress: std::time::Duration::from_secs(42),
                volume: 73,
                mute_state: None,
                route: super::super::YouTubePlaybackRoute::default(),
            }),
            unified_queue: Some(UnifiedQueue::new(
                vec![super::super::PlayableMedia::YouTube(track())],
                0,
            )),
            ..PlayerState::default()
        };
        state.refresh_youtube_session();
        state
            .provider_sessions
            .get_mut(&super::super::Provider::YouTubeMusic)
            .unwrap()
            .resume_on_activate = true;

        state.save_provider_sessions(&folder).unwrap();
        state.save_provider_sessions(&folder).unwrap();
        let raw = std::fs::read_to_string(folder.join(PROVIDER_SESSIONS_FILE)).unwrap();
        for forbidden in [
            "googlevideo",
            "access_token",
            "refresh_token",
            "authorization",
            "cookie",
            "sapisid",
            "po_token",
            "signed_url",
            "required_headers",
        ] {
            assert!(
                !raw.to_ascii_lowercase().contains(forbidden),
                "provider session persisted forbidden field category {forbidden}"
            );
        }

        let restored = PlayerState::load_provider_sessions(&folder);
        assert!(restored.youtube_playback.is_none());
        assert_eq!(restored.youtube_playback_phase, YouTubePlaybackPhase::Idle);
        assert!(!restored.youtube_should_resume_on_activate());
        assert_eq!(
            restored.effective_playback_provider(crate::config::ActiveProvider::Spotify),
            crate::config::ActiveProvider::Spotify
        );
        assert_eq!(restored.unified_queue.as_ref().unwrap().position(), 0);

        std::fs::remove_dir_all(folder).unwrap();
    }

    #[test]
    fn recording_activation_state_preserves_the_other_provider_session() {
        let mut youtube = ProviderPlaybackSession::new(super::super::Provider::YouTubeMusic);
        youtube.media_id = Some(super::super::MediaId {
            provider: super::super::Provider::YouTubeMusic,
            kind: super::super::MediaKind::Track,
            raw_id: "youtube-session".to_string(),
        });
        youtube.progress = std::time::Duration::from_secs(41);
        youtube.repeat = rspotify::model::RepeatState::Context;
        youtube.shuffle = true;
        youtube.volume = 61;

        let mut state = PlayerState::default();
        state
            .provider_sessions
            .insert(super::super::Provider::YouTubeMusic, youtube);
        let youtube_before = serde_json::to_value(
            state
                .provider_sessions
                .get(&super::super::Provider::YouTubeMusic)
                .unwrap(),
        )
        .unwrap();

        state.remember_spotify_activation_state();

        assert_eq!(
            serde_json::to_value(
                state
                    .provider_sessions
                    .get(&super::super::Provider::YouTubeMusic)
                    .unwrap()
            )
            .unwrap(),
            youtube_before
        );

        let mut spotify = ProviderPlaybackSession::new(super::super::Provider::Spotify);
        spotify.media_id = Some(super::super::MediaId {
            provider: super::super::Provider::Spotify,
            kind: super::super::MediaKind::Track,
            raw_id: "spotify-session".to_string(),
        });
        spotify.progress = std::time::Duration::from_secs(29);
        spotify.repeat = rspotify::model::RepeatState::Track;
        spotify.shuffle = true;
        spotify.volume = 47;
        state
            .provider_sessions
            .insert(super::super::Provider::Spotify, spotify);
        let spotify_before = serde_json::to_value(
            state
                .provider_sessions
                .get(&super::super::Provider::Spotify)
                .unwrap(),
        )
        .unwrap();

        state.remember_youtube_activation_state();

        assert_eq!(
            serde_json::to_value(
                state
                    .provider_sessions
                    .get(&super::super::Provider::Spotify)
                    .unwrap()
            )
            .unwrap(),
            spotify_before
        );
    }

    #[test]
    fn shuffled_scheduler_round_trip_preserves_original_and_user_order() {
        let folder = temporary_folder("provider-queue");
        let mut queue = UnifiedQueue::new(
            ["a", "b", "c", "d"]
                .into_iter()
                .map(|id| super::super::PlayableMedia::YouTube(track_with_id(id)))
                .collect(),
            0,
        );
        queue.enqueue_user(
            ["x", "y"]
                .into_iter()
                .map(|id| super::super::PlayableMedia::YouTube(track_with_id(id))),
        );
        queue.toggle_shuffle();
        let state = PlayerState {
            unified_queue: Some(queue),
            ..PlayerState::default()
        };

        state.save_provider_sessions(&folder).unwrap();
        let mut restored = PlayerState::load_provider_sessions(&folder)
            .unified_queue
            .unwrap();
        assert!(restored.is_shuffled());
        assert_eq!(
            restored
                .user_queue()
                .iter()
                .map(|item| item.media.media_id().raw_id)
                .collect::<Vec<_>>(),
            ["x", "y"]
        );

        restored.toggle_shuffle();
        assert_eq!(
            restored
                .automatic_queue()
                .iter()
                .map(|item| item.media.media_id().raw_id)
                .collect::<Vec<_>>(),
            ["b", "c", "d"]
        );

        std::fs::remove_dir_all(folder).unwrap();
    }

    #[test]
    fn unified_queue_display_items_keep_current_before_pending_items() {
        let mut queue = UnifiedQueue::new(
            ["current", "context-next"]
                .into_iter()
                .map(|id| super::super::PlayableMedia::YouTube(track_with_id(id)))
                .collect(),
            0,
        );
        queue.enqueue_user([super::super::PlayableMedia::YouTube(track_with_id(
            "user-next",
        ))]);

        let player = PlayerState {
            youtube_playback: Some(youtube_playback("current", true)),
            unified_queue: Some(queue),
            ..PlayerState::default()
        };
        let items = player.queue_display_items();

        assert_eq!(items.len(), 3);
        assert_eq!(player.queue_display_item_count(), items.len());
        for (index, item) in items.iter().enumerate() {
            let borrowed = player
                .queue_display_item_ref(index)
                .expect("borrowed queue row");
            assert_eq!(borrowed.media_id(), item.media_id());
            assert_eq!(borrowed.is_current(), item.is_current());
            assert_eq!(borrowed.unified_entry_id(), item.unified_entry_id());
        }
        assert!(items[0].is_current());
        assert!(!items[1].is_current());
        assert!(!items[2].is_current());
        assert!(matches!(
            &items[0],
            QueueDisplayItem::Unified {
                item: QueuedItem {
                    media: super::super::PlayableMedia::YouTube(track),
                    ..
                },
                ..
            } if track.id == "current"
        ));
    }

    #[test]
    fn indexed_queue_projection_reads_large_queues_without_materializing_rows() {
        let queue = UnifiedQueue::new(
            (0..128)
                .map(|index| {
                    super::super::PlayableMedia::YouTube(track_with_id(&format!("track-{index}")))
                })
                .collect(),
            0,
        );
        let player = PlayerState {
            youtube_playback: Some(youtube_playback("track-0", true)),
            unified_queue: Some(queue),
            ..PlayerState::default()
        };

        assert_eq!(player.queue_display_item_count(), 128);
        for index in [0, 63, 127] {
            let item = player
                .queue_display_item_ref(index)
                .expect("indexed queue row");
            let expected = format!("track-{index}");
            assert_eq!(
                item.media_id().map(|media_id| media_id.raw_id),
                Some(expected)
            );
        }
        assert!(player.queue_display_item_ref(128).is_none());
    }

    #[test]
    fn queue_authority_uses_playback_and_queue_facts_instead_of_browsing_state() {
        let spotify_track_id = "4iV5W9uYEdYUVa79Axb7Rh";
        let spotify_without_unified = PlayerState {
            playback: Some(spotify_playback(spotify_track_id, true)),
            playback_last_updated_time: Some(std::time::Instant::now()),
            ..PlayerState::default()
        };
        assert_eq!(
            spotify_without_unified.queue_authority(),
            QueueAuthority::SpotifyNative
        );
        assert!(spotify_without_unified
            .automatic_native_queue_refresh_guard()
            .is_some());

        let spotify_with_unified = PlayerState {
            playback: Some(spotify_playback(spotify_track_id, true)),
            playback_last_updated_time: Some(std::time::Instant::now()),
            unified_queue: Some(UnifiedQueue::new(
                vec![super::super::PlayableMedia::Spotify(spotify_playable_id(
                    spotify_track_id,
                ))],
                0,
            )),
            ..PlayerState::default()
        };
        assert_eq!(
            spotify_with_unified.queue_authority(),
            QueueAuthority::LocalUnified
        );
        assert!(spotify_with_unified
            .automatic_native_queue_refresh_guard()
            .is_none());

        let youtube_without_unified = PlayerState {
            youtube_playback: Some(youtube_playback("youtube-track", true)),
            ..PlayerState::default()
        };
        assert_eq!(
            youtube_without_unified.queue_authority(),
            QueueAuthority::LocalProviderSession
        );
        assert!(youtube_without_unified
            .automatic_native_queue_refresh_guard()
            .is_none());
        assert!(youtube_without_unified.queue_display_items().is_empty());

        let youtube_with_unified = PlayerState {
            youtube_playback: Some(youtube_playback("youtube-track", true)),
            unified_queue: Some(UnifiedQueue::new(
                vec![super::super::PlayableMedia::YouTube(track_with_id(
                    "youtube-track",
                ))],
                0,
            )),
            ..PlayerState::default()
        };
        assert_eq!(
            youtube_with_unified.queue_authority(),
            QueueAuthority::LocalUnified
        );
        assert!(matches!(
            youtube_with_unified.queue_display_items().as_slice(),
            [QueueDisplayItem::Unified { .. }]
        ));

        let stale_cross_provider_queue = PlayerState {
            playback: Some(spotify_playback("spotify-track", true)),
            playback_last_updated_time: Some(std::time::Instant::now()),
            unified_queue: Some(UnifiedQueue::new(
                vec![super::super::PlayableMedia::YouTube(track_with_id(
                    "stale-youtube",
                ))],
                0,
            )),
            ..PlayerState::default()
        };
        assert_eq!(
            stale_cross_provider_queue.queue_authority(),
            QueueAuthority::SpotifyNative
        );
        assert!(matches!(
            stale_cross_provider_queue.queue_display_items().as_slice(),
            [QueueDisplayItem::Spotify { .. }]
        ));
    }

    #[test]
    fn native_context_start_releases_stale_local_queue_before_projection() {
        let mut player = PlayerState {
            playback: Some(spotify_playback("current", true)),
            active_playback_provider: Some(crate::config::ActiveProvider::Spotify),
            unified_queue: Some(UnifiedQueue::new(
                vec![super::super::PlayableMedia::Spotify(spotify_playable_id(
                    "current",
                ))],
                0,
            )),
            queue: Some(rspotify::model::CurrentUserQueue {
                currently_playing: Some(rspotify::model::PlayableItem::Unknown(
                    serde_json::json!({ "id": "current", "type": "track" }),
                )),
                queue: vec![rspotify::model::PlayableItem::Unknown(
                    serde_json::json!({ "id": "context-next", "type": "track" }),
                )],
            }),
            ..PlayerState::default()
        };

        assert_eq!(player.queue_authority(), QueueAuthority::LocalUnified);
        assert_eq!(player.queue_display_items().len(), 1);

        let provider_queue = player.queue.take().unwrap();
        player.clear_local_queue_for_native_playback();
        assert!(player.queue.is_none());
        player.queue = Some(provider_queue);

        assert_eq!(player.queue_authority(), QueueAuthority::SpotifyNative);
        assert_eq!(player.queue_display_items().len(), 2);
        assert!(player.unified_queue.is_none());
        assert!(player.queue.is_some());
    }

    #[test]
    fn native_queue_result_guard_rejects_authority_and_identity_changes() {
        let mut player = PlayerState {
            playback: Some(spotify_playback("spotify-track", true)),
            playback_last_updated_time: Some(std::time::Instant::now()),
            buffered_playback: Some(PlaybackMetadata {
                device_name: "unified-player".to_owned(),
                device_id: Some("device-a".to_owned()),
                volume: Some(75),
                is_playing: true,
                repeat_state: rspotify::model::RepeatState::Off,
                shuffle_state: false,
                mute_state: None,
            }),
            ..PlayerState::default()
        };
        let guard = player.native_queue_refresh_guard().unwrap();
        assert!(player.native_queue_refresh_guard_is_current(&guard));

        player.buffered_playback.as_mut().unwrap().device_id = Some("device-b".to_owned());
        assert!(!player.native_queue_refresh_guard_is_current(&guard));

        player.buffered_playback.as_mut().unwrap().device_id = Some("device-a".to_owned());
        player.youtube_playback_phase = YouTubePlaybackPhase::Resolving;
        assert!(!player.native_queue_refresh_guard_is_current(&guard));
    }

    #[test]
    fn mixed_user_additions_share_one_queue_after_the_current_item() {
        let current = spotify_playable_id("4iV5W9uYEdYUVa79Axb7Rh");
        let spotify_next = spotify_playable_id("5iKndSu1XI74U2OZePzP8L");
        let youtube_next = track_with_id("youtube-next");
        let mut player = PlayerState::default();

        player.enqueue_unified_user_items(
            Some(super::super::PlayableMedia::Spotify(current.clone())),
            [super::super::PlayableMedia::Spotify(spotify_next.clone())],
        );
        player.enqueue_unified_user_items(
            Some(super::super::PlayableMedia::Spotify(current)),
            [super::super::PlayableMedia::YouTube(youtube_next)],
        );

        let ids = player
            .unified_queue
            .as_ref()
            .unwrap()
            .display_items()
            .into_iter()
            .map(|item| item.media.media_id().raw_id)
            .collect::<Vec<_>>();
        assert_eq!(
            ids,
            [
                "4iV5W9uYEdYUVa79Axb7Rh",
                "5iKndSu1XI74U2OZePzP8L",
                "youtube-next",
            ]
        );
    }
}
