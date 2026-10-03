use crate::state::{MediaId, MediaKind, Provider, UnifiedQueueInstanceId};

use super::{OccurrenceDescriptor, ScopedSelection, ScopedSelectionError};

/// The display mode represented by a Queue page projection.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum QueueDisplayMode {
    Unified,
    NativeSpotify,
}

/// The exact runtime scope for one Queue page projection.
///
/// Unified rows are scoped to the queue instance as well as their entry IDs;
/// native Spotify rows are scoped to the provider-local selection epoch. The
/// enum variant itself is the exact display mode, so switching modes cannot
/// reuse keyed state accidentally.
#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub enum QueueSelectionScope {
    Unified { instance_id: UnifiedQueueInstanceId },
    NativeSpotify { provider_selection_epoch: u64 },
}

impl QueueSelectionScope {
    /// Construct a unified queue scope from its runtime instance identity.
    pub fn unified(instance_id: UnifiedQueueInstanceId) -> Self {
        Self::Unified { instance_id }
    }

    /// Construct the native Spotify queue scope for a provider epoch.
    pub const fn native_spotify(provider_selection_epoch: u64) -> Self {
        Self::NativeSpotify {
            provider_selection_epoch,
        }
    }

    /// Return the exact queue display mode carried by this scope.
    pub const fn display_mode(&self) -> QueueDisplayMode {
        match self {
            Self::Unified { .. } => QueueDisplayMode::Unified,
            Self::NativeSpotify { .. } => QueueDisplayMode::NativeSpotify,
        }
    }

    /// Return the provider-local epoch for a native scope.
    pub const fn provider_selection_epoch(&self) -> Option<u64> {
        match self {
            Self::Unified { .. } => None,
            Self::NativeSpotify {
                provider_selection_epoch,
            } => Some(*provider_selection_epoch),
        }
    }

    /// Return the unified queue instance identity for a unified scope.
    pub fn instance_id(&self) -> Option<&UnifiedQueueInstanceId> {
        match self {
            Self::Unified { instance_id } => Some(instance_id),
            Self::NativeSpotify { .. } => None,
        }
    }
}

/// Queue has one unfiltered visible projection, represented explicitly so a
/// future view cannot silently share selection state with the current one.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum QueueSelectionView {
    All,
}

/// Keyed selection for unified and native Queue projections.
pub type QueueSelection = ScopedSelection<QueueSelectionScope, QueueSelectionView, MediaId, u64>;

/// A native Spotify queue row that may or may not have a safe semantic ID.
///
/// `Unkeyable` covers local rows without an ID and unknown playables. Such a
/// row makes the complete native projection cursor-only rather than receiving
/// a fabricated placeholder identity.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum NativeQueueRow {
    Media(MediaId),
    Unkeyable,
}

impl From<MediaId> for NativeQueueRow {
    fn from(media_id: MediaId) -> Self {
        Self::Media(media_id)
    }
}

impl From<Option<MediaId>> for NativeQueueRow {
    fn from(media_id: Option<MediaId>) -> Self {
        media_id.map_or(Self::Unkeyable, Self::Media)
    }
}

/// Synchronize a unified queue projection using each item's real entry ID as
/// its occurrence token.
pub fn synchronize_unified_queue_items<I>(
    selection: &mut QueueSelection,
    instance_id: UnifiedQueueInstanceId,
    items: I,
) -> Result<(), ScopedSelectionError>
where
    I: IntoIterator<Item = (MediaId, u64)>,
{
    let descriptors = items
        .into_iter()
        .map(|(media_id, entry_id)| OccurrenceDescriptor::with_token(media_id, entry_id))
        .collect::<Vec<_>>();
    selection.synchronize(
        QueueSelectionScope::unified(instance_id),
        descriptors.clone(),
        QueueSelectionView::All,
        descriptors,
    )
}

/// Synchronize a native Spotify queue projection.
///
/// Track and episode IDs are asserted unique within the complete projection.
/// Any unsupported provider/kind, empty ID, duplicate, or explicitly
/// unkeyable row makes the entire projection cursor-only while retaining its
/// bounded row count for navigation fallback.
pub fn synchronize_native_spotify_queue<I, R>(
    selection: &mut QueueSelection,
    provider_selection_epoch: u64,
    rows: I,
) -> Result<(), ScopedSelectionError>
where
    I: IntoIterator<Item = R>,
    R: Into<NativeQueueRow>,
{
    let rows = rows.into_iter().map(Into::into).collect::<Vec<_>>();
    let Some(descriptors) = rows
        .iter()
        .map(native_descriptor)
        .collect::<Option<Vec<_>>>()
    else {
        return selection.synchronize_cursor_only(
            QueueSelectionScope::native_spotify(provider_selection_epoch),
            QueueSelectionView::All,
            rows.len(),
        );
    };

    selection.synchronize(
        QueueSelectionScope::native_spotify(provider_selection_epoch),
        descriptors.clone(),
        QueueSelectionView::All,
        descriptors,
    )
}

/// Resolve selected Queue rows, falling back to the bounds-checked cursor if
/// no keyed rows are available.
pub fn queue_selected_or_cursor_indices(
    selection: &QueueSelection,
    cursor: usize,
) -> Result<Vec<usize>, ScopedSelectionError> {
    let selected = selection.selected_visible_indices();
    if selected.is_empty() {
        selection.selected_or_cursor_visible_indices(cursor)
    } else {
        Ok(selected)
    }
}

/// Return whether a native row can safely participate in keyed selection.
fn native_descriptor(row: &NativeQueueRow) -> Option<OccurrenceDescriptor<MediaId, u64>> {
    let NativeQueueRow::Media(media_id) = row else {
        return None;
    };
    let valid_kind = matches!(media_id.kind, MediaKind::Track | MediaKind::Episode);
    (media_id.provider == Provider::Spotify && valid_kind && !media_id.raw_id.is_empty())
        .then(|| OccurrenceDescriptor::<MediaId, u64>::unique(media_id.clone()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::state::ui::ScopedSelectionStatus;
    use crate::state::{MediaKind, Provider};

    fn media(raw_id: &str, kind: MediaKind) -> MediaId {
        MediaId {
            provider: Provider::Spotify,
            kind,
            raw_id: raw_id.to_owned(),
        }
    }

    fn youtube_media(raw_id: &str) -> MediaId {
        MediaId {
            provider: Provider::YouTubeMusic,
            kind: MediaKind::Track,
            raw_id: raw_id.to_owned(),
        }
    }

    #[test]
    fn unified_scope_keeps_duplicate_media_occurrences_distinct() {
        let instance_id = crate::state::UnifiedQueue::empty().instance_id();
        let mut selection = QueueSelection::default();
        synchronize_unified_queue_items(
            &mut selection,
            instance_id.clone(),
            vec![(youtube_media("same"), 11), (youtube_media("same"), 12)],
        )
        .unwrap();
        selection.extend_range(0, 1).unwrap();
        assert_eq!(selection.selected_visible_indices(), vec![0, 1]);

        synchronize_unified_queue_items(
            &mut selection,
            instance_id,
            vec![(youtube_media("same"), 12), (youtube_media("same"), 11)],
        )
        .unwrap();
        assert_eq!(selection.selected_visible_indices(), vec![0, 1]);
    }

    #[test]
    fn unified_scope_replacement_clears_and_refresh_drops_missing_entries() {
        let mut selection = QueueSelection::default();
        let first = crate::state::UnifiedQueue::empty().instance_id();
        synchronize_unified_queue_items(
            &mut selection,
            first.clone(),
            vec![(youtube_media("a"), 1), (youtube_media("b"), 2)],
        )
        .unwrap();
        selection.extend_range(0, 1).unwrap();
        synchronize_unified_queue_items(
            &mut selection,
            first,
            vec![(youtube_media("b"), 2), (youtube_media("c"), 3)],
        )
        .unwrap();
        assert_eq!(selection.selected_visible_indices(), vec![0]);

        synchronize_unified_queue_items(
            &mut selection,
            crate::state::UnifiedQueue::empty().instance_id(),
            vec![(youtube_media("b"), 2)],
        )
        .unwrap();
        assert!(selection.selected_visible_indices().is_empty());
        assert_eq!(selection.anchor_visible_index(), None);
    }

    #[test]
    fn unified_scope_same_instance_reconciles_append_reorder_and_missing_rows() {
        let instance_id = crate::state::UnifiedQueue::empty().instance_id();
        let mut selection = QueueSelection::default();
        synchronize_unified_queue_items(
            &mut selection,
            instance_id.clone(),
            vec![(youtube_media("a"), 1), (youtube_media("b"), 2)],
        )
        .unwrap();
        selection.extend_range(0, 1).unwrap();

        synchronize_unified_queue_items(
            &mut selection,
            instance_id.clone(),
            vec![
                (youtube_media("b"), 2),
                (youtube_media("a"), 1),
                (youtube_media("c"), 3),
            ],
        )
        .unwrap();
        assert_eq!(selection.selected_visible_indices(), vec![0, 1]);
        assert_eq!(selection.visible_len(), 3);

        synchronize_unified_queue_items(
            &mut selection,
            instance_id,
            vec![(youtube_media("c"), 3), (youtube_media("d"), 4)],
        )
        .unwrap();
        assert!(selection.selected_visible_indices().is_empty());
        assert_eq!(selection.anchor_visible_index(), None);
        assert_eq!(selection.visible_len(), 2);
    }

    #[test]
    fn native_duplicate_or_unkeyable_rows_are_cursor_only_and_bounded() {
        let duplicate = media("same", MediaKind::Track);
        let mut selection = QueueSelection::default();
        synchronize_native_spotify_queue(&mut selection, 4, vec![duplicate.clone(), duplicate])
            .unwrap();
        assert_eq!(selection.status(), ScopedSelectionStatus::Ambiguous);
        assert_eq!(queue_selected_or_cursor_indices(&selection, 1), Ok(vec![1]));
        assert!(selection.extend_range(0, 1).is_err());

        synchronize_native_spotify_queue(
            &mut selection,
            4,
            vec![Some(media("track", MediaKind::Track)), None],
        )
        .unwrap();
        assert_eq!(selection.status(), ScopedSelectionStatus::Ambiguous);
        assert_eq!(queue_selected_or_cursor_indices(&selection, 1), Ok(vec![1]));
        assert_eq!(
            queue_selected_or_cursor_indices(&selection, 2),
            Err(ScopedSelectionError::OutOfBounds { index: 2, len: 2 })
        );
    }

    #[test]
    fn native_track_and_episode_ids_are_keyed_and_epoch_scoped() {
        let mut selection = QueueSelection::default();
        synchronize_native_spotify_queue(
            &mut selection,
            1,
            vec![
                media("track", MediaKind::Track),
                media("episode", MediaKind::Episode),
            ],
        )
        .unwrap();
        selection.extend_range(0, 1).unwrap();
        assert_eq!(selection.selected_visible_indices(), vec![0, 1]);

        synchronize_native_spotify_queue(&mut selection, 2, vec![media("track", MediaKind::Track)])
            .unwrap();
        assert!(selection.selected_visible_indices().is_empty());
    }

    #[test]
    fn unsupported_native_provider_or_kind_is_cursor_only() {
        let mut selection = QueueSelection::default();
        synchronize_native_spotify_queue(
            &mut selection,
            1,
            vec![youtube_media("video"), media("video", MediaKind::Video)],
        )
        .unwrap();
        assert_eq!(selection.status(), ScopedSelectionStatus::Ambiguous);
    }
}
