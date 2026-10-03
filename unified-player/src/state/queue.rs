use rand::seq::SliceRandom;
use serde::{Deserialize, Serialize};
use std::collections::{HashSet, VecDeque};
use std::hash::{Hash, Hasher};
use std::sync::Arc;
use std::time::Instant;

use rspotify::model::Id as _;

use super::model::{ContextId, PlayableMedia};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum QueueOrigin {
    UserAdded,
    Context,
    Recommendation,
}

impl QueueOrigin {
    pub fn label(self) -> &'static str {
        match self {
            Self::UserAdded => "Added by you",
            Self::Context => "From context",
            Self::Recommendation => "Recommendation",
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct QueuedItem {
    pub entry_id: u64,
    pub media: PlayableMedia,
    pub origin: QueueOrigin,
}

/// Opaque identity for one runtime instance of the unified queue.
///
/// The identity is intentionally backed by an allocation rather than a
/// process-global counter or persisted value. Cloning a queue clones this
/// handle, while constructing or restoring a queue allocates a fresh handle.
/// No account, media, or row information is encoded in the identity.
#[derive(Clone)]
pub struct UnifiedQueueInstanceId(Arc<()>);

impl UnifiedQueueInstanceId {
    fn fresh() -> Self {
        Self(Arc::new(()))
    }
}

impl std::fmt::Debug for UnifiedQueueInstanceId {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("UnifiedQueueInstanceId(..)")
    }
}

impl PartialEq for UnifiedQueueInstanceId {
    fn eq(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.0, &other.0)
    }
}

impl Eq for UnifiedQueueInstanceId {}

impl Hash for UnifiedQueueInstanceId {
    fn hash<H: Hasher>(&self, state: &mut H) {
        std::ptr::hash(Arc::as_ptr(&self.0), state);
    }
}

/// Identity of one concrete occurrence at the head of a runtime unified queue.
///
/// Media IDs are not sufficient here because the same track may appear in
/// adjacent queue entries. This token is process-local and is never persisted.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct UnifiedQueueCompletionToken {
    instance_id: UnifiedQueueInstanceId,
    entry_id: u64,
}

#[derive(Clone, Debug)]
pub(crate) struct UnifiedQueueParts {
    pub current: Option<QueuedItem>,
    pub history: Vec<QueuedItem>,
    pub user_queue: VecDeque<QueuedItem>,
    pub automatic_queue: VecDeque<QueuedItem>,
    pub automatic_original: Vec<QueuedItem>,
    pub repeat: rspotify::model::RepeatState,
    pub shuffle: bool,
}

/// Most Spotify queue labels kept; older ones fall back to a generic label.
const MAX_SPOTIFY_QUEUE_LABELS: usize = 512;

/// Display details for a Spotify item added to the unified queue.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SpotifyQueueLabel {
    pub title: String,
    pub artists: String,
    pub duration: std::time::Duration,
}

/// Labels remembered when Spotify items are added to the unified queue,
/// which itself carries only Spotify IDs. Bounded and session-local.
#[derive(Debug, Default)]
pub struct SpotifyQueueLabelCache {
    labels: std::collections::HashMap<String, SpotifyQueueLabel>,
    order: VecDeque<String>,
}

impl SpotifyQueueLabelCache {
    pub fn remember_track(&mut self, track: &super::Track) {
        self.insert(
            track.id.id().to_owned(),
            SpotifyQueueLabel {
                title: track.name.clone(),
                artists: track.artists_info(),
                duration: track.duration,
            },
        );
    }

    pub fn remember_episode(&mut self, episode: &super::Episode) {
        self.insert(
            episode.id.id().to_owned(),
            SpotifyQueueLabel {
                title: episode.name.clone(),
                artists: episode
                    .show
                    .as_ref()
                    .map(|show| show.name.clone())
                    .unwrap_or_default(),
                duration: episode.duration,
            },
        );
    }

    pub fn get(&self, raw_id: &str) -> Option<&SpotifyQueueLabel> {
        self.labels.get(raw_id)
    }

    fn insert(&mut self, raw_id: String, label: SpotifyQueueLabel) {
        if self.labels.insert(raw_id.clone(), label).is_none() {
            self.order.push_back(raw_id);
            while self.order.len() > MAX_SPOTIFY_QUEUE_LABELS {
                if let Some(oldest) = self.order.pop_front() {
                    self.labels.remove(&oldest);
                }
            }
        }
    }
}

/// The single authoritative provider-neutral playback scheduler.
///
/// Explicit user additions live in a high-priority FIFO lane. Context and
/// recommendation items share a lower-priority automatic lane. Provider
/// adapters consume the scheduler's decision instead of maintaining their own
/// playback cursor.
#[derive(Clone, Debug)]
pub struct UnifiedQueue {
    #[allow(dead_code)]
    instance_id: UnifiedQueueInstanceId,
    current: Option<QueuedItem>,
    history: Vec<QueuedItem>,
    user_queue: VecDeque<QueuedItem>,
    automatic_queue: VecDeque<QueuedItem>,
    automatic_original: Vec<QueuedItem>,
    repeat: rspotify::model::RepeatState,
    shuffle: bool,
    next_entry_id: u64,
}

impl UnifiedQueue {
    pub fn new(tracks: Vec<PlayableMedia>, start_position: usize) -> Self {
        if tracks.is_empty() {
            return Self::empty();
        }

        let items = tracks
            .into_iter()
            .enumerate()
            .map(|(index, media)| QueuedItem {
                entry_id: index as u64 + 1,
                media,
                origin: QueueOrigin::Context,
            })
            .collect::<Vec<_>>();
        let position = start_position.min(items.len() - 1);
        Self {
            instance_id: UnifiedQueueInstanceId::fresh(),
            current: Some(items[position].clone()),
            history: items[..position].to_vec(),
            user_queue: VecDeque::new(),
            automatic_queue: items[position + 1..].iter().cloned().collect(),
            automatic_original: items.clone(),
            repeat: rspotify::model::RepeatState::Off,
            shuffle: false,
            next_entry_id: items.len() as u64 + 1,
        }
    }

    pub fn empty() -> Self {
        Self {
            instance_id: UnifiedQueueInstanceId::fresh(),
            current: None,
            history: Vec::new(),
            user_queue: VecDeque::new(),
            automatic_queue: VecDeque::new(),
            automatic_original: Vec::new(),
            repeat: rspotify::model::RepeatState::Off,
            shuffle: false,
            next_entry_id: 1,
        }
    }

    pub fn current(&self) -> Option<&PlayableMedia> {
        self.current.as_ref().map(|item| &item.media)
    }

    /// Capture the current queue occurrence only when it belongs to `media`.
    pub fn completion_token_for(
        &self,
        media: &PlayableMedia,
    ) -> Option<UnifiedQueueCompletionToken> {
        let current = self
            .current
            .as_ref()
            .filter(|item| item.media.is_same_item(media))?;
        Some(UnifiedQueueCompletionToken {
            instance_id: self.instance_id.clone(),
            entry_id: current.entry_id,
        })
    }

    /// Whether a completion still names the current occurrence of this queue.
    pub fn completion_is_current(&self, token: &UnifiedQueueCompletionToken) -> bool {
        self.instance_id == token.instance_id
            && self
                .current
                .as_ref()
                .is_some_and(|item| item.entry_id == token.entry_id)
    }

    /// Advance only if `token` still owns the current queue occurrence.
    pub fn advance_after_completion(
        &mut self,
        token: &UnifiedQueueCompletionToken,
    ) -> Option<PlayableMedia> {
        self.completion_is_current(token)
            .then(|| self.next())
            .flatten()
    }

    /// Return this queue's opaque runtime instance identity.
    #[allow(dead_code)]
    pub fn instance_id(&self) -> UnifiedQueueInstanceId {
        self.instance_id.clone()
    }

    pub fn next_candidate(&self) -> Option<&PlayableMedia> {
        if self.repeat == rspotify::model::RepeatState::Track {
            return self.current();
        }
        self.user_queue
            .front()
            .or_else(|| self.automatic_queue.front())
            .map(|item| &item.media)
    }

    pub fn next(&mut self) -> Option<PlayableMedia> {
        if self.repeat == rspotify::model::RepeatState::Track {
            return self.current().cloned();
        }

        let mut next = self
            .user_queue
            .pop_front()
            .or_else(|| self.automatic_queue.pop_front());
        if next.is_none() && self.repeat == rspotify::model::RepeatState::Context {
            self.automatic_queue = self
                .automatic_original
                .iter()
                .filter(|item| item.origin == QueueOrigin::Context)
                .cloned()
                .collect();
            if self.shuffle {
                self.shuffle_automatic_queue();
            }
            next = self.automatic_queue.pop_front();
        }

        let next = next?;
        if let Some(current) = self.current.replace(next) {
            self.history.push(current);
        }
        self.current().cloned()
    }

    pub fn previous(&mut self) -> Option<PlayableMedia> {
        let previous = self.history.pop()?;
        if let Some(current) = self.current.replace(previous) {
            match current.origin {
                QueueOrigin::UserAdded => self.user_queue.push_front(current),
                QueueOrigin::Context | QueueOrigin::Recommendation => {
                    self.automatic_queue.push_front(current);
                }
            }
        }
        self.current().cloned()
    }

    pub fn enqueue_user<I>(&mut self, items: I)
    where
        I: IntoIterator<Item = PlayableMedia>,
    {
        for media in items {
            let item = self.new_item(media, QueueOrigin::UserAdded);
            self.user_queue.push_back(item);
        }
    }

    #[allow(dead_code)]
    pub fn append_recommendations<I>(&mut self, items: I)
    where
        I: IntoIterator<Item = PlayableMedia>,
    {
        for media in items {
            let item = self.new_item(media, QueueOrigin::Recommendation);
            self.automatic_original.push(item.clone());
            self.automatic_queue.push_back(item);
        }
    }

    pub fn position(&self) -> usize {
        self.history.len()
    }

    #[cfg(test)]
    pub fn user_queue(&self) -> &VecDeque<QueuedItem> {
        &self.user_queue
    }

    #[cfg(test)]
    pub fn automatic_queue(&self) -> &VecDeque<QueuedItem> {
        &self.automatic_queue
    }

    pub fn display_items(&self) -> Vec<&QueuedItem> {
        self.current
            .iter()
            .chain(self.user_queue.iter())
            .chain(self.automatic_queue.iter())
            .collect()
    }

    /// Number of rows exposed by the compact queue projection.
    pub fn display_item_count(&self) -> usize {
        usize::from(self.current.is_some()) + self.user_queue.len() + self.automatic_queue.len()
    }

    /// Return one compact queue row without materializing the full projection.
    pub fn display_item(&self, index: usize) -> Option<&QueuedItem> {
        let mut index = index;
        if let Some(current) = self.current.as_ref() {
            if index == 0 {
                return Some(current);
            }
            index = index.saturating_sub(1);
        }
        self.user_queue.get(index).or_else(|| {
            self.automatic_queue
                .get(index.saturating_sub(self.user_queue.len()))
        })
    }

    pub fn set_repeat(&mut self, repeat: rspotify::model::RepeatState) {
        self.repeat = repeat;
    }

    pub fn repeat(&self) -> rspotify::model::RepeatState {
        self.repeat
    }

    pub fn toggle_shuffle(&mut self) {
        self.shuffle = !self.shuffle;
        if self.shuffle {
            self.shuffle_automatic_queue();
        } else {
            let pending = self
                .automatic_queue
                .iter()
                .map(|item| item.entry_id)
                .collect::<HashSet<_>>();
            self.automatic_queue = self
                .automatic_original
                .iter()
                .filter(|item| pending.contains(&item.entry_id))
                .cloned()
                .collect();
        }
    }

    pub fn is_shuffled(&self) -> bool {
        self.shuffle
    }

    pub(crate) fn persistent_parts(&self) -> UnifiedQueueParts {
        UnifiedQueueParts {
            current: self.current.clone(),
            history: self.history.clone(),
            user_queue: self.user_queue.clone(),
            automatic_queue: self.automatic_queue.clone(),
            automatic_original: self.automatic_original.clone(),
            repeat: self.repeat,
            shuffle: self.shuffle,
        }
    }

    pub(crate) fn restore(parts: UnifiedQueueParts) -> Self {
        let next_entry_id = parts
            .current
            .iter()
            .chain(parts.history.iter())
            .chain(parts.user_queue.iter())
            .chain(parts.automatic_queue.iter())
            .chain(parts.automatic_original.iter())
            .map(|item| item.entry_id)
            .max()
            .unwrap_or_default()
            .saturating_add(1);
        Self {
            instance_id: UnifiedQueueInstanceId::fresh(),
            current: parts.current,
            history: parts.history,
            user_queue: parts.user_queue,
            automatic_queue: parts.automatic_queue,
            automatic_original: parts.automatic_original,
            repeat: parts.repeat,
            shuffle: parts.shuffle,
            next_entry_id,
        }
    }

    fn new_item(&mut self, media: PlayableMedia, origin: QueueOrigin) -> QueuedItem {
        let item = QueuedItem {
            entry_id: self.next_entry_id,
            media,
            origin,
        };
        self.next_entry_id = self.next_entry_id.saturating_add(1);
        item
    }

    fn shuffle_automatic_queue(&mut self) {
        let mut items = self.automatic_queue.drain(..).collect::<Vec<_>>();
        items.shuffle(&mut rand::rng());
        self.automatic_queue = items.into();
    }
}

/// Result of advancing the queue by one track.
#[derive(Clone, Debug, PartialEq, Eq)]
#[allow(dead_code)]
pub enum AdvanceResult {
    /// The next track is still within the current batch — librespot handles it.
    SameBatch,
    /// The current batch is exhausted; here is the next batch of track URIs to
    /// send via `StartPlayback`.
    NewBatch(Vec<PlayableMedia>),
    /// The queue has reached the end and `autoplay` is enabled — the caller
    /// should fetch radio tracks and append them before continuing.
    NeedsRadioTracks,
    /// The queue is fully exhausted and autoplay is not enabled.
    EndOfQueue,
}

/// Result of retreating the queue by one track.
#[derive(Clone, Debug, PartialEq, Eq)]
#[allow(dead_code)]
pub enum RetreatResult {
    /// The previous track is still within the current batch.
    SameBatch,
    /// Need to load the previous batch to reach the previous track.
    PreviousBatch(Vec<PlayableMedia>),
    /// Already at the very beginning of the queue.
    BeginningOfQueue,
}

/// Shuffle mode for the custom queue.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
#[allow(dead_code)]
pub enum ShuffleMode {
    #[default]
    Off,
    /// Standard shuffle — randomize the full track order.
    Shuffle,
    /// Smart shuffle — shuffle + interleave radio recommendations.
    /// Carries the radio tracks used for interleaving.
    SmartShuffle(Vec<PlayableMedia>),
}

/// App-managed provider-neutral playback queue that replaces spirc-managed
/// queueing when enabled.
///
/// The custom queue stores the **full** ordered list for a context and sends
/// provider-specific batches to the active playback adapter. It only
/// intervenes at batch boundaries — within a batch, the native adapter handles
/// next/previous natively.
#[derive(Clone, Debug)]
#[allow(dead_code)]
pub struct CustomQueue {
    /// Original ordered track list (from the context, respecting client-side sort).
    original_tracks: Vec<PlayableMedia>,
    /// The effective play order.
    /// When shuffle is off this is a clone of `original_tracks`; when on it's a
    /// permutation. When smart-shuffle is on, extra recommendation track IDs are
    /// interleaved.
    play_order: Vec<PlayableMedia>,
    /// Current position within `play_order`.
    position: usize,
    /// Start index (inclusive) of the current batch within `play_order`.
    batch_start: usize,
    /// End index (exclusive) of the current batch within `play_order`.
    /// Tracks `play_order[batch_start..batch_end]` are the current batch.
    /// Normally `batch_end = min(batch_start + max_batch_size, play_order.len())`,
    /// but `truncate_batch_to_current()` can shrink it to `position + 1`.
    batch_end: usize,
    /// Maximum number of tracks per Spotify API batch (= `tracks_playback_limit`).
    max_batch_size: usize,
    /// Original context (for "playing from" display and radio seed).
    source_context: Option<ContextId>,
    /// Local repeat state mirroring the player's repeat.
    repeat: rspotify::model::RepeatState,
    /// Current shuffle mode (Off / Shuffle / `SmartShuffle`).
    shuffle_mode: ShuffleMode,
    /// Whether to fetch and append radio tracks when the queue is exhausted.
    /// Sourced from `DeviceConfig.autoplay`.
    autoplay: bool,
    /// Timestamp of last batch transition, used for consistency-check cooldown.
    last_batch_transition: Option<Instant>,
}

#[allow(dead_code)]
impl CustomQueue {
    /// Create a new custom queue.
    ///
    /// - `tracks`: the full ordered track list (respecting any client-side sort).
    /// - `start_position`: index of the track the user selected to play first.
    /// - `max_batch_size`: maximum tracks per Spotify batch (typically `tracks_playback_limit`).
    /// - `source_context`: the originating context (playlist, album, etc.) for
    ///   "playing from" display and radio seed.
    /// - `autoplay`: whether to fetch radio tracks when the queue is exhausted
    ///   (sourced from device config).
    pub fn new(
        tracks: Vec<PlayableMedia>,
        start_position: usize,
        max_batch_size: usize,
        source_context: Option<ContextId>,
        autoplay: bool,
    ) -> Self {
        let play_order = tracks.clone();
        let batch_start = start_position;
        let batch_end = (batch_start + max_batch_size).min(play_order.len());

        Self {
            original_tracks: tracks,
            play_order,
            position: start_position,
            batch_start,
            batch_end,
            max_batch_size,
            source_context,
            repeat: rspotify::model::RepeatState::Off,
            shuffle_mode: ShuffleMode::Off,
            autoplay,
            last_batch_transition: None,
        }
    }

    // ── Accessors ──────────────────────────────────────────────────────

    /// The track URIs that make up the current batch sent to Spotify.
    pub fn current_batch(&self) -> &[PlayableMedia] {
        &self.play_order[self.batch_start..self.batch_end]
    }

    /// The currently playing track.
    pub fn current_track(&self) -> &PlayableMedia {
        &self.play_order[self.position]
    }

    /// All tracks after the current position (for queue UI display).
    pub fn remaining_tracks(&self) -> &[PlayableMedia] {
        if self.position + 1 >= self.play_order.len() {
            &[]
        } else {
            &self.play_order[self.position + 1..]
        }
    }

    /// The source context this queue was built from.
    pub fn source_context(&self) -> Option<&ContextId> {
        self.source_context.as_ref()
    }

    /// Current shuffle mode.
    pub fn shuffle_mode(&self) -> &ShuffleMode {
        &self.shuffle_mode
    }

    /// Current repeat state.
    pub fn repeat(&self) -> rspotify::model::RepeatState {
        self.repeat
    }

    /// Current position within the play order.
    pub fn position(&self) -> usize {
        self.position
    }

    /// Batch start index.
    pub fn batch_start(&self) -> usize {
        self.batch_start
    }

    /// Batch end index (exclusive).
    pub fn batch_end(&self) -> usize {
        self.batch_end
    }

    /// Total number of tracks in the queue.
    pub fn len(&self) -> usize {
        self.play_order.len()
    }

    /// Whether the queue is empty.
    pub fn is_empty(&self) -> bool {
        self.play_order.is_empty()
    }

    /// Timestamp of last batch transition (for consistency-check cooldown).
    pub fn last_batch_transition(&self) -> Option<Instant> {
        self.last_batch_transition
    }

    /// The expected next track in the play order (if any and within the batch).
    /// Used for queue consistency checking.
    pub fn expected_next_track(&self) -> Option<&PlayableMedia> {
        let next = self.position + 1;
        if next < self.batch_end {
            Some(&self.play_order[next])
        } else {
            None
        }
    }

    /// Whether the current track is the last in the current batch.
    pub fn is_at_batch_end(&self) -> bool {
        self.position + 1 >= self.batch_end
    }

    /// Whether the current track is the first in the current batch.
    pub fn is_at_batch_start(&self) -> bool {
        self.position == self.batch_start
    }

    // ── Mutations ──────────────────────────────────────────────────────

    /// Advance to the next track. Returns what action the caller should take.
    ///
    /// This is called from the `EndOfTrack` handler — it is the **sole**
    /// mechanism for advancing position.
    pub fn advance(&mut self) -> AdvanceResult {
        let next = self.position + 1;

        // RepeatState::Track — don't advance; librespot loops the track.
        if self.repeat == rspotify::model::RepeatState::Track {
            return AdvanceResult::SameBatch;
        }

        if next < self.batch_end {
            // Still within the current batch.
            self.position = next;
            AdvanceResult::SameBatch
        } else if next < self.play_order.len() {
            // Current batch exhausted but more tracks remain — start next batch.
            self.position = next;
            self.batch_start = next;
            self.batch_end = (self.batch_start + self.max_batch_size).min(self.play_order.len());
            self.mark_batch_transition();
            AdvanceResult::NewBatch(self.current_batch().to_vec())
        } else if self.repeat == rspotify::model::RepeatState::Context {
            // End of queue with repeat-context — wrap to beginning.
            self.position = 0;
            self.batch_start = 0;
            self.batch_end = self.max_batch_size.min(self.play_order.len());
            self.mark_batch_transition();
            AdvanceResult::NewBatch(self.current_batch().to_vec())
        } else if self.autoplay {
            // End of queue, no repeat — autoplay is enabled, ask caller to
            // fetch radio tracks and append them.
            AdvanceResult::NeedsRadioTracks
        } else {
            AdvanceResult::EndOfQueue
        }
    }

    /// Retreat to the previous track. Returns what action the caller should take.
    pub fn retreat(&mut self) -> RetreatResult {
        if self.position == 0 {
            if self.repeat == rspotify::model::RepeatState::Context {
                // Wrap to end of queue.
                self.position = self.play_order.len().saturating_sub(1);
                self.batch_end = self.play_order.len();
                self.batch_start = self.batch_end.saturating_sub(self.max_batch_size);
                self.mark_batch_transition();
                RetreatResult::PreviousBatch(self.current_batch().to_vec())
            } else {
                RetreatResult::BeginningOfQueue
            }
        } else {
            let prev = self.position - 1;
            if prev >= self.batch_start {
                self.position = prev;
                RetreatResult::SameBatch
            } else {
                // Need to load the previous batch.
                self.position = prev;
                self.batch_end = self.batch_start;
                self.batch_start = self.batch_end.saturating_sub(self.max_batch_size);
                self.mark_batch_transition();
                RetreatResult::PreviousBatch(self.current_batch().to_vec())
            }
        }
    }

    /// Truncate the current batch so that the current track is the last entry.
    ///
    /// After calling this, the next `EndOfTrack` event will trigger a batch
    /// transition with the new state (shuffle permutation, repeat mode, etc.)
    /// **without interrupting the currently playing song**.
    ///
    /// This is the key mechanism for non-interrupting shuffle/repeat changes.
    pub fn truncate_batch_to_current(&mut self) {
        self.batch_end = self.position + 1;
    }

    /// Update the repeat state.
    pub fn set_repeat(&mut self, repeat: rspotify::model::RepeatState) {
        self.repeat = repeat;
    }

    /// Change the shuffle mode.
    ///
    /// - `Off`: restore `play_order` to `original_tracks` order; find the
    ///   current track's position in the original order.
    /// - `Shuffle`: Fisher-Yates permutation of `play_order`, keeping the
    ///   current track at front (`position` 0).
    /// - `SmartShuffle(radio_tracks)`: shuffle + interleave the provided radio
    ///   recommendation tracks every N songs.
    ///
    /// After permuting, calls `truncate_batch_to_current()` so the change
    /// takes effect at the next batch boundary without restarting the current
    /// track.
    pub fn set_shuffle_mode(&mut self, mode: ShuffleMode) {
        let current_track = self.play_order[self.position].clone();

        match &mode {
            ShuffleMode::Off => {
                // Restore original order.
                self.play_order = self.original_tracks.clone();
                // Find where the current track sits in the original order.
                self.position = self
                    .play_order
                    .iter()
                    .position(|t| *t == current_track)
                    .unwrap_or(0);
            }
            ShuffleMode::Shuffle => {
                // Build a shuffled order with current track at front.
                let mut rng = rand::rng();
                let mut order: Vec<PlayableMedia> = self
                    .original_tracks
                    .iter()
                    .filter(|t| **t != current_track)
                    .cloned()
                    .collect();
                order.shuffle(&mut rng);
                order.insert(0, current_track);
                self.play_order = order;
                self.position = 0;
            }
            ShuffleMode::SmartShuffle(radio_tracks) => {
                // Shuffle first, then interleave radio tracks.
                let mut rng = rand::rng();
                let mut order: Vec<PlayableMedia> = self
                    .original_tracks
                    .iter()
                    .filter(|t| **t != current_track)
                    .cloned()
                    .collect();
                order.shuffle(&mut rng);
                order.insert(0, current_track);

                if radio_tracks.is_empty() {
                    self.play_order = order;
                } else {
                    // Interleave one radio track every 4 original tracks.
                    let mut interleaved = Vec::with_capacity(order.len() + radio_tracks.len());
                    let mut radio_iter = radio_tracks.iter();
                    for (i, track) in order.into_iter().enumerate() {
                        interleaved.push(track);
                        if i > 0 && i % 4 == 0 {
                            if let Some(rt) = radio_iter.next() {
                                interleaved.push(rt.clone());
                            }
                        }
                    }
                    // Append any remaining radio tracks.
                    interleaved.extend(radio_iter.cloned());
                    self.play_order = interleaved;
                }
                self.position = 0;
            }
        }

        self.shuffle_mode = mode;
        // Let the current song finish, then the next batch uses the new order.
        self.truncate_batch_to_current();
    }

    /// Append radio recommendation tracks for autoplay continuation.
    pub fn append_radio_tracks(&mut self, tracks: Vec<PlayableMedia>) {
        self.play_order.extend(tracks);
    }

    /// Compute and load the next batch. Returns the batch URIs to send to
    /// Spotify, or `None` if the queue is exhausted.
    pub fn next_batch(&mut self) -> Option<Vec<PlayableMedia>> {
        if self.batch_end >= self.play_order.len() {
            return None;
        }
        self.batch_start = self.batch_end;
        self.batch_end = (self.batch_start + self.max_batch_size).min(self.play_order.len());
        self.mark_batch_transition();
        Some(self.current_batch().to_vec())
    }

    /// Record that a batch transition just occurred (for consistency-check
    /// cooldown).
    pub fn mark_batch_transition(&mut self) {
        self.last_batch_transition = Some(Instant::now());
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::state::{PlayableId, YouTubeTrack};

    fn make_track_id(n: u32) -> PlayableMedia {
        PlayableMedia::Spotify(PlayableId::Track(
            rspotify::model::TrackId::from_id(format!("track{n:032}"))
                .unwrap()
                .into_static(),
        ))
    }

    fn make_tracks(count: u32) -> Vec<PlayableMedia> {
        (0..count).map(make_track_id).collect()
    }

    fn make_youtube_track(id: &str) -> YouTubeTrack {
        YouTubeTrack {
            id: id.to_string(),
            name: id.to_string(),
            artists: "artist".to_string(),
            album: None,
            duration: "1:00".to_string(),
            explicit: false,
            thumbnail_url: None,
            is_video: false,
        }
    }

    #[test]
    fn unified_queue_instance_identity_is_clone_stable_but_replacement_fresh() {
        let queue = UnifiedQueue::new(vec![make_youtube_track("a").into()], 0);
        let clone = queue.clone();
        let replacement = UnifiedQueue::new(vec![make_youtube_track("a").into()], 0);
        let restored = UnifiedQueue::restore(queue.persistent_parts());

        assert_eq!(queue.instance_id(), clone.instance_id());
        assert_ne!(queue.instance_id(), replacement.instance_id());
        assert_ne!(queue.instance_id(), restored.instance_id());
    }

    #[test]
    fn unified_queue_completion_advances_one_occurrence_only_once() {
        let repeated = PlayableMedia::YouTube(make_youtube_track("same"));
        let tail = PlayableMedia::YouTube(make_youtube_track("tail"));
        let mut queue = UnifiedQueue::new(vec![repeated.clone(), repeated.clone(), tail], 0);
        let first = queue.completion_token_for(&repeated).unwrap();

        assert_eq!(
            queue
                .advance_after_completion(&first)
                .unwrap()
                .media_id()
                .raw_id,
            "same"
        );
        assert!(queue.advance_after_completion(&first).is_none());

        let second = queue.completion_token_for(&repeated).unwrap();
        assert_ne!(first, second);
        assert_eq!(
            queue
                .advance_after_completion(&second)
                .unwrap()
                .media_id()
                .raw_id,
            "tail"
        );
    }

    #[test]
    fn spotify_queue_labels_are_bounded_oldest_first() {
        let mut cache = SpotifyQueueLabelCache::default();
        let label = |title: &str| SpotifyQueueLabel {
            title: title.to_owned(),
            artists: String::new(),
            duration: std::time::Duration::ZERO,
        };
        for index in 0..=MAX_SPOTIFY_QUEUE_LABELS {
            cache.insert(format!("id{index}"), label("song"));
        }
        // Re-adding a known item does not refresh or duplicate its slot.
        cache.insert("id1".to_owned(), label("renamed"));

        assert!(cache.get("id0").is_none());
        assert_eq!(cache.get("id1").map(|l| l.title.as_str()), Some("renamed"));
        assert!(cache
            .get(&format!("id{MAX_SPOTIFY_QUEUE_LABELS}"))
            .is_some());
        assert_eq!(cache.order.len(), MAX_SPOTIFY_QUEUE_LABELS);
    }

    #[test]
    fn youtube_completion_survives_refreshed_display_metadata() {
        let listed = make_youtube_track("same");
        let spotify_id = rspotify::model::TrackId::from_id("track0000000000000000000000000001")
            .unwrap()
            .into_static();
        let spotify = PlayableMedia::Spotify(rspotify::model::PlayableId::Track(spotify_id));
        let mut queue = UnifiedQueue::new(
            vec![PlayableMedia::YouTube(listed.clone()), spotify.clone()],
            0,
        );
        // Playback replaces the listed duration with the resolved stream's.
        let playing = PlayableMedia::YouTube(YouTubeTrack {
            duration: "9:59".to_owned(),
            thumbnail_url: Some("https://example.invalid/refreshed.jpg".to_owned()),
            ..listed
        });

        let completion = queue
            .completion_token_for(&playing)
            .expect("the same video must still own its queue occurrence");
        assert_eq!(queue.advance_after_completion(&completion), Some(spotify));
        assert!(queue
            .completion_token_for(&PlayableMedia::YouTube(make_youtube_track("other")))
            .is_none());
    }

    #[test]
    fn unified_queue_completion_from_replaced_queue_is_stale() {
        let media = PlayableMedia::YouTube(make_youtube_track("same"));
        let old_queue = UnifiedQueue::new(vec![media.clone()], 0);
        let completion = old_queue.completion_token_for(&media).unwrap();
        let mut replacement = UnifiedQueue::new(vec![media], 0);

        assert!(!replacement.completion_is_current(&completion));
        assert!(replacement.advance_after_completion(&completion).is_none());
    }

    #[test]
    fn unified_queue_preserves_provider_boundaries() {
        let spotify_id = rspotify::model::TrackId::from_id("track0000000000000000000000000001")
            .unwrap()
            .into_static();
        let mut queue = UnifiedQueue::new(
            vec![
                PlayableMedia::Spotify(PlayableId::Track(spotify_id)),
                PlayableMedia::YouTube(make_youtube_track("youtube")),
            ],
            0,
        );
        assert_eq!(
            queue.current().unwrap().provider(),
            super::super::model::Provider::Spotify
        );
        assert_eq!(
            queue.next().unwrap().provider(),
            super::super::model::Provider::YouTubeMusic
        );
    }

    #[test]
    fn unified_queue_advances_and_rewinds() {
        let mut queue = UnifiedQueue::new(
            vec![
                PlayableMedia::YouTube(make_youtube_track("a")),
                PlayableMedia::YouTube(make_youtube_track("b")),
            ],
            0,
        );
        assert_eq!(queue.current().unwrap().media_id().raw_id, "a");
        assert_eq!(queue.next().unwrap().media_id().raw_id, "b");
        assert!(queue.next().is_none());
        assert_eq!(queue.previous().unwrap().media_id().raw_id, "a");
    }

    #[test]
    fn user_queue_is_fifo_and_precedes_context() {
        let mut queue = UnifiedQueue::new(
            vec![
                PlayableMedia::YouTube(make_youtube_track("a")),
                PlayableMedia::YouTube(make_youtube_track("b")),
                PlayableMedia::YouTube(make_youtube_track("c")),
            ],
            0,
        );
        queue.enqueue_user(vec![
            PlayableMedia::YouTube(make_youtube_track("x")),
            PlayableMedia::YouTube(make_youtube_track("y")),
        ]);
        assert_eq!(queue.next().unwrap().media_id().raw_id, "x");
        assert_eq!(queue.next().unwrap().media_id().raw_id, "y");
        assert_eq!(queue.next().unwrap().media_id().raw_id, "b");
    }

    #[test]
    fn unified_queue_context_repeat_wraps() {
        let mut queue = UnifiedQueue::new(
            vec![
                PlayableMedia::YouTube(make_youtube_track("a")),
                PlayableMedia::YouTube(make_youtube_track("b")),
            ],
            1,
        );
        queue.set_repeat(rspotify::model::RepeatState::Context);
        assert_eq!(queue.next().unwrap().media_id().raw_id, "a");
        assert_eq!(queue.previous().unwrap().media_id().raw_id, "b");
    }

    #[test]
    fn shuffle_never_reorders_user_lane_and_can_restore_context_order() {
        let mut queue = UnifiedQueue::new(
            vec![
                PlayableMedia::YouTube(make_youtube_track("a")),
                PlayableMedia::YouTube(make_youtube_track("b")),
                PlayableMedia::YouTube(make_youtube_track("c")),
                PlayableMedia::YouTube(make_youtube_track("d")),
            ],
            0,
        );
        queue.enqueue_user(vec![
            PlayableMedia::YouTube(make_youtube_track("x")),
            PlayableMedia::YouTube(make_youtube_track("y")),
        ]);

        queue.toggle_shuffle();
        assert_eq!(
            queue
                .user_queue()
                .iter()
                .map(|item| item.media.media_id().raw_id)
                .collect::<Vec<_>>(),
            ["x", "y"]
        );
        queue.toggle_shuffle();
        assert_eq!(
            queue
                .automatic_queue()
                .iter()
                .map(|item| item.media.media_id().raw_id)
                .collect::<Vec<_>>(),
            ["b", "c", "d"]
        );
    }

    #[test]
    fn recommendations_remain_behind_user_and_context_items() {
        let mut queue = UnifiedQueue::new(
            vec![
                PlayableMedia::YouTube(make_youtube_track("a")),
                PlayableMedia::YouTube(make_youtube_track("b")),
            ],
            0,
        );
        queue.append_recommendations(vec![PlayableMedia::YouTube(make_youtube_track("r"))]);
        queue.enqueue_user(vec![PlayableMedia::YouTube(make_youtube_track("x"))]);

        assert_eq!(queue.next().unwrap().media_id().raw_id, "x");
        assert_eq!(queue.next().unwrap().media_id().raw_id, "b");
        assert_eq!(queue.next().unwrap().media_id().raw_id, "r");
    }

    #[test]
    fn repeat_context_does_not_promote_recommendations_into_the_context() {
        let mut queue = UnifiedQueue::new(vec![PlayableMedia::YouTube(make_youtube_track("a"))], 0);
        queue.append_recommendations(vec![PlayableMedia::YouTube(make_youtube_track("r"))]);
        queue.set_repeat(rspotify::model::RepeatState::Context);

        assert_eq!(queue.next().unwrap().media_id().raw_id, "r");
        assert_eq!(queue.next().unwrap().media_id().raw_id, "a");
        assert_eq!(queue.next().unwrap().media_id().raw_id, "a");
    }

    #[test]
    fn previous_follows_actual_cross_lane_history() {
        let mut queue = UnifiedQueue::new(
            vec![
                PlayableMedia::YouTube(make_youtube_track("a")),
                PlayableMedia::YouTube(make_youtube_track("b")),
            ],
            0,
        );
        queue.enqueue_user(vec![PlayableMedia::YouTube(make_youtube_track("x"))]);
        assert_eq!(queue.next().unwrap().media_id().raw_id, "x");
        assert_eq!(queue.next().unwrap().media_id().raw_id, "b");
        assert_eq!(queue.previous().unwrap().media_id().raw_id, "x");
        assert_eq!(queue.previous().unwrap().media_id().raw_id, "a");
    }

    #[test]
    fn new_queue_basic_properties() {
        let tracks = make_tracks(10);
        let q = CustomQueue::new(tracks.clone(), 0, 5, None, false);

        assert_eq!(q.len(), 10);
        assert_eq!(q.position(), 0);
        assert_eq!(q.batch_start(), 0);
        assert_eq!(q.batch_end(), 5);
        assert_eq!(q.current_batch().len(), 5);
        assert_eq!(*q.current_track(), tracks[0]);
    }

    #[test]
    fn new_queue_start_position_mid() {
        let tracks = make_tracks(10);
        let q = CustomQueue::new(tracks.clone(), 3, 5, None, false);

        assert_eq!(q.position(), 3);
        assert_eq!(q.batch_start(), 3);
        assert_eq!(q.batch_end(), 8);
        assert_eq!(*q.current_track(), tracks[3]);
    }

    #[test]
    fn new_queue_batch_end_clamped() {
        let tracks = make_tracks(3);
        let q = CustomQueue::new(tracks, 0, 10, None, false);

        assert_eq!(q.batch_end(), 3);
        assert_eq!(q.current_batch().len(), 3);
    }

    #[test]
    fn advance_within_batch() {
        let tracks = make_tracks(10);
        let mut q = CustomQueue::new(tracks.clone(), 0, 5, None, false);

        assert_eq!(q.advance(), AdvanceResult::SameBatch);
        assert_eq!(q.position(), 1);
        assert_eq!(*q.current_track(), tracks[1]);
    }

    #[test]
    fn advance_across_batch_boundary() {
        let tracks = make_tracks(10);
        let mut q = CustomQueue::new(tracks, 0, 5, None, false);

        // Advance to position 4 (last in batch [0..5)).
        for _ in 0..4 {
            assert_eq!(q.advance(), AdvanceResult::SameBatch);
        }
        assert_eq!(q.position(), 4);

        // Next advance should trigger a new batch.
        let result = q.advance();
        assert!(matches!(result, AdvanceResult::NewBatch(_)));
        assert_eq!(q.position(), 5);
        assert_eq!(q.batch_start(), 5);
        assert_eq!(q.batch_end(), 10);
    }

    #[test]
    fn advance_end_of_queue() {
        let tracks = make_tracks(3);
        let mut q = CustomQueue::new(tracks, 0, 10, None, false);

        for _ in 0..2 {
            q.advance();
        }
        assert_eq!(q.advance(), AdvanceResult::EndOfQueue);
    }

    #[test]
    fn advance_needs_radio_tracks() {
        let tracks = make_tracks(3);
        let mut q = CustomQueue::new(tracks, 0, 10, None, true);

        for _ in 0..2 {
            q.advance();
        }
        assert_eq!(q.advance(), AdvanceResult::NeedsRadioTracks);
    }

    #[test]
    fn advance_repeat_context_wraps() {
        let tracks = make_tracks(3);
        let mut q = CustomQueue::new(tracks.clone(), 0, 10, None, false);
        q.set_repeat(rspotify::model::RepeatState::Context);

        for _ in 0..2 {
            q.advance();
        }
        let result = q.advance();
        assert!(matches!(result, AdvanceResult::NewBatch(_)));
        assert_eq!(q.position(), 0);
        assert_eq!(*q.current_track(), tracks[0]);
    }

    #[test]
    fn advance_repeat_track_stays() {
        let tracks = make_tracks(3);
        let mut q = CustomQueue::new(tracks.clone(), 0, 10, None, false);
        q.set_repeat(rspotify::model::RepeatState::Track);

        assert_eq!(q.advance(), AdvanceResult::SameBatch);
        assert_eq!(q.position(), 0); // Didn't move.
        assert_eq!(*q.current_track(), tracks[0]);
    }

    #[test]
    fn retreat_within_batch() {
        let tracks = make_tracks(10);
        let mut q = CustomQueue::new(tracks.clone(), 0, 5, None, false);

        // Advance to position 2 (still within batch [0..5)).
        q.advance();
        q.advance();
        assert_eq!(q.position(), 2);

        assert_eq!(q.retreat(), RetreatResult::SameBatch);
        assert_eq!(q.position(), 1);
        assert_eq!(*q.current_track(), tracks[1]);
    }

    #[test]
    fn retreat_at_beginning() {
        let tracks = make_tracks(10);
        let mut q = CustomQueue::new(tracks, 0, 5, None, false);

        assert_eq!(q.retreat(), RetreatResult::BeginningOfQueue);
        assert_eq!(q.position(), 0);
    }

    #[test]
    fn retreat_across_batch_boundary() {
        let tracks = make_tracks(10);
        let mut q = CustomQueue::new(tracks, 0, 5, None, false);

        // Advance into the second batch.
        for _ in 0..4 {
            q.advance();
        }
        q.advance(); // Triggers new batch at position 5.

        // Now retreat back across the boundary.
        let result = q.retreat();
        assert!(matches!(result, RetreatResult::PreviousBatch(_)));
        assert_eq!(q.position(), 4);
    }

    #[test]
    fn truncate_batch_to_current() {
        let tracks = make_tracks(10);
        let mut q = CustomQueue::new(tracks, 0, 5, None, false);

        q.advance(); // position = 1
        q.advance(); // position = 2
        q.truncate_batch_to_current();

        assert_eq!(q.batch_end(), 3); // position + 1
        assert!(q.is_at_batch_end());
    }

    #[test]
    fn remaining_tracks_correct() {
        let tracks = make_tracks(5);
        let q = CustomQueue::new(tracks.clone(), 0, 10, None, false);

        assert_eq!(q.remaining_tracks().len(), 4);
        assert_eq!(q.remaining_tracks()[0], tracks[1]);
    }

    #[test]
    fn remaining_tracks_at_end() {
        let tracks = make_tracks(3);
        let mut q = CustomQueue::new(tracks, 0, 10, None, false);
        q.advance();
        q.advance();

        assert!(q.remaining_tracks().is_empty());
    }

    #[test]
    fn expected_next_track_within_batch() {
        let tracks = make_tracks(10);
        let q = CustomQueue::new(tracks.clone(), 0, 5, None, false);

        assert_eq!(q.expected_next_track(), Some(&tracks[1]));
    }

    #[test]
    fn expected_next_track_at_batch_end() {
        let tracks = make_tracks(10);
        let mut q = CustomQueue::new(tracks, 0, 5, None, false);

        // Advance to position 4 (last in batch).
        for _ in 0..4 {
            q.advance();
        }
        assert_eq!(q.expected_next_track(), None); // Next is outside batch.
    }

    #[test]
    fn append_radio_tracks() {
        let tracks = make_tracks(3);
        let mut q = CustomQueue::new(tracks, 0, 10, None, false);

        let radio = make_tracks(5);
        q.append_radio_tracks(radio);

        assert_eq!(q.len(), 8);
    }

    #[test]
    fn set_shuffle_mode_shuffle() {
        let tracks = make_tracks(10);
        let mut q = CustomQueue::new(tracks.clone(), 3, 5, None, false);

        q.set_shuffle_mode(ShuffleMode::Shuffle);

        // Current track should be at front.
        assert_eq!(*q.current_track(), tracks[3]);
        assert_eq!(q.position(), 0);
        // All original tracks should be present.
        assert_eq!(q.len(), 10);
        assert_eq!(*q.shuffle_mode(), ShuffleMode::Shuffle);
        // Batch should be truncated to current.
        assert_eq!(q.batch_end(), 1);
    }

    #[test]
    fn set_shuffle_mode_off_restores_order() {
        let tracks = make_tracks(10);
        let mut q = CustomQueue::new(tracks.clone(), 3, 5, None, false);

        q.set_shuffle_mode(ShuffleMode::Shuffle);
        q.set_shuffle_mode(ShuffleMode::Off);

        // Should be back in original order.
        assert_eq!(q.play_order, tracks);
        assert_eq!(*q.current_track(), tracks[3]);
        assert_eq!(q.position(), 3);
    }
}
