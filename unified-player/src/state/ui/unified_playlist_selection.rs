use crate::state::{MediaId, PlaylistEntryId};

use super::{
    MutablePlaylistSelection, MutablePlaylistView, OccurrenceDescriptor, PlaylistRef,
    ProviderOccurrenceToken, ScopedSelectionError,
};

/// The stable local identity of one unified playlist page.
#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub struct UnifiedPlaylistSelectionScope {
    playlist_id: String,
}

impl UnifiedPlaylistSelectionScope {
    /// Construct a scope from the local playlist ID.
    pub fn new(playlist_id: impl Into<String>) -> Self {
        Self {
            playlist_id: playlist_id.into(),
        }
    }

    /// Return the stable local playlist ID carried by this scope.
    pub fn playlist_id(&self) -> &str {
        &self.playlist_id
    }
}

/// Unified filtering changes only the visible projection. Keeping one view
/// identity lets hidden selections remain owned by the page while the query
/// is edited or cleared.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum UnifiedPlaylistSelectionView {
    All,
}

/// Keyed selection for one `UnifiedPlaylist` page.
pub type UnifiedPlaylistSelection = MutablePlaylistSelection;

/// Synchronize complete and visible `UnifiedPlaylist` projections by durable
/// playlist-local occurrence identity.
///
/// Repeated media IDs remain independently selectable because every descriptor
/// carries its persistent `PlaylistEntryId`. Titles, artists, provider URLs,
/// and other row metadata never enter selection identity.
pub fn synchronize_unified_playlist_entries<I, J>(
    selection: &mut UnifiedPlaylistSelection,
    playlist_id: impl Into<String>,
    complete_entries: I,
    visible_entries: J,
) -> Result<(), ScopedSelectionError>
where
    I: IntoIterator<Item = (MediaId, PlaylistEntryId)>,
    J: IntoIterator<Item = (MediaId, PlaylistEntryId)>,
{
    let complete = complete_entries
        .into_iter()
        .map(|(media_id, entry_id)| {
            OccurrenceDescriptor::with_token(media_id, ProviderOccurrenceToken::Unified(entry_id))
        })
        .collect::<Vec<_>>();
    let visible = visible_entries
        .into_iter()
        .map(|(media_id, entry_id)| {
            OccurrenceDescriptor::with_token(media_id, ProviderOccurrenceToken::Unified(entry_id))
        })
        .collect::<Vec<_>>();
    selection.synchronize_retaining_hidden(
        PlaylistRef::new(None, playlist_id, 0),
        complete,
        MutablePlaylistView::All,
        visible,
    )
}

/// Resolve selected `UnifiedPlaylist` rows, falling back to the bounds-checked
/// cursor when the projection has no keyed rows.
pub fn unified_playlist_selected_or_cursor_indices(
    selection: &UnifiedPlaylistSelection,
    cursor: usize,
) -> Result<Vec<usize>, ScopedSelectionError> {
    let selected = selection.selected_visible_indices();
    if selected.is_empty() {
        selection.selected_or_cursor_visible_indices(cursor)
    } else {
        Ok(selected)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::state::ui::ScopedSelectionStatus;
    use crate::state::{MediaKind, PlaylistEntryId, Provider};

    fn media(provider: Provider, kind: MediaKind, raw_id: &str) -> MediaId {
        MediaId {
            provider,
            kind,
            raw_id: raw_id.to_owned(),
        }
    }

    fn entry(media_id: MediaId, entry_id: u64) -> (MediaId, PlaylistEntryId) {
        (media_id, PlaylistEntryId(entry_id))
    }

    #[test]
    fn playlist_scope_is_local_id_and_reorder_retains_occurrence_keys() {
        let first = media(Provider::Spotify, MediaKind::Track, "a");
        let second = media(Provider::YouTubeMusic, MediaKind::Track, "b");
        let mut selection = UnifiedPlaylistSelection::default();
        synchronize_unified_playlist_entries(
            &mut selection,
            "playlist-one",
            vec![entry(first.clone(), 1), entry(second.clone(), 2)],
            vec![entry(first.clone(), 1), entry(second.clone(), 2)],
        )
        .unwrap();
        selection.extend_range(0, 0).unwrap();
        synchronize_unified_playlist_entries(
            &mut selection,
            "playlist-one",
            vec![entry(second.clone(), 2), entry(first.clone(), 1)],
            vec![entry(second, 2), entry(first, 1)],
        )
        .unwrap();
        assert_eq!(selection.selected_visible_indices(), vec![1]);
    }

    #[test]
    fn changing_playlist_id_clears_selection_and_anchor() {
        let id = media(Provider::Spotify, MediaKind::Track, "a");
        let mut selection = UnifiedPlaylistSelection::default();
        synchronize_unified_playlist_entries(
            &mut selection,
            "one",
            vec![entry(id.clone(), 1)],
            vec![entry(id.clone(), 1)],
        )
        .unwrap();
        selection.extend_range(0, 0).unwrap();
        synchronize_unified_playlist_entries(
            &mut selection,
            "two",
            vec![entry(id.clone(), 1)],
            vec![entry(id, 1)],
        )
        .unwrap();
        assert!(selection.selected_visible_indices().is_empty());
        assert_eq!(selection.anchor_visible_index(), None);
    }

    #[test]
    fn duplicate_media_ids_with_distinct_entry_ids_remain_independent() {
        let id = media(Provider::Spotify, MediaKind::Track, "same");
        let mut selection = UnifiedPlaylistSelection::default();
        let entries = vec![entry(id.clone(), 10), entry(id, 11)];
        synchronize_unified_playlist_entries(&mut selection, "one", entries.clone(), entries)
            .unwrap();
        assert_eq!(selection.status(), ScopedSelectionStatus::Ready);
        selection.extend_range(0, 1).unwrap();
        assert_eq!(selection.selected_visible_indices(), vec![0, 1]);
    }

    #[test]
    fn filtering_retains_hidden_selection_and_exact_full_mapping() {
        let first = media(Provider::Spotify, MediaKind::Track, "same");
        let second = media(Provider::Spotify, MediaKind::Track, "same");
        let third = media(Provider::YouTubeMusic, MediaKind::Track, "third");
        let complete = vec![
            entry(first.clone(), 10),
            entry(second.clone(), 11),
            entry(third.clone(), 12),
        ];
        let mut selection = UnifiedPlaylistSelection::default();
        synchronize_unified_playlist_entries(
            &mut selection,
            "one",
            complete.clone(),
            complete.clone(),
        )
        .unwrap();
        selection.extend_range(0, 1).unwrap();

        synchronize_unified_playlist_entries(
            &mut selection,
            "one",
            complete,
            vec![entry(second, 11), entry(third, 12)],
        )
        .unwrap();
        assert_eq!(selection.selected_len(), 2);
        assert_eq!(selection.selected_visible_indices(), vec![0]);
        assert_eq!(selection.visible_to_full_index(0), Ok(1));
    }

    #[test]
    fn empty_projection_is_bounded_and_cursor_fallback_checks_length() {
        let mut selection = UnifiedPlaylistSelection::default();
        synchronize_unified_playlist_entries(
            &mut selection,
            "empty",
            Vec::<(MediaId, PlaylistEntryId)>::new(),
            Vec::<(MediaId, PlaylistEntryId)>::new(),
        )
        .unwrap();
        assert_eq!(selection.status(), ScopedSelectionStatus::Empty);
        assert_eq!(
            unified_playlist_selected_or_cursor_indices(&selection, 0),
            Err(ScopedSelectionError::OutOfBounds { index: 0, len: 0 })
        );
    }
}
