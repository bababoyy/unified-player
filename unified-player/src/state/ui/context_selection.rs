use super::scoped_selection::{ScopedSelection, ScopedSelectionError};
use super::selection::OccurrenceDescriptor;

/// The Spotify Context track pane that owns a keyed selection adapter.
///
/// Related artists remain cursor-only. Shows are also intentionally cursor-only
/// and therefore do not have a variant here.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum ContextTrackPane {
    Playlist,
    Album,
    Tracks,
    ArtistTopTracks,
    ArtistLikedSongs,
}

/// A privacy-safe scope for one Spotify Context track pane.
///
/// The scope carries only the provider-local selection epoch, the semantic
/// Context URI, and the exact pane. It deliberately does not retain account
/// labels, credentials, metadata, duplicate ordinals, or local row IDs.
#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub struct ContextSelectionScope {
    provider_selection_epoch: u64,
    context_uri: String,
    pane: ContextTrackPane,
}

impl ContextSelectionScope {
    /// Construct a scope for one Spotify Context pane.
    pub fn new(
        provider_selection_epoch: u64,
        context_uri: impl Into<String>,
        pane: ContextTrackPane,
    ) -> Self {
        Self {
            provider_selection_epoch,
            context_uri: context_uri.into(),
            pane,
        }
    }

    /// Return the provider-local epoch carried by this scope.
    pub const fn provider_selection_epoch(&self) -> u64 {
        self.provider_selection_epoch
    }

    /// Return the semantic Context URI carried by this scope.
    pub fn context_uri(&self) -> &str {
        &self.context_uri
    }

    /// Return the exact track pane carried by this scope.
    pub const fn pane(&self) -> ContextTrackPane {
        self.pane
    }
}

/// The exact view key for a Spotify Context track pane.
///
/// `Filtered(String::new())` intentionally differs from `Unfiltered`, so
/// opening a filter and clearing its query still resets selection scope.
#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub enum ContextSelectionView {
    Unfiltered,
    Filtered(String),
}

impl ContextSelectionView {
    /// Return the unfiltered view key.
    pub const fn unfiltered() -> Self {
        Self::Unfiltered
    }

    /// Return a filtered view key, including an empty filtered query.
    pub fn filtered(query: impl Into<String>) -> Self {
        Self::Filtered(query.into())
    }

    /// Build the exact view key from the optional active filter query.
    pub fn from_query(query: Option<&str>) -> Self {
        query.map_or(Self::Unfiltered, |query| Self::Filtered(query.to_owned()))
    }

    /// Return whether this key represents an open filter.
    pub const fn is_filtered(&self) -> bool {
        matches!(self, Self::Filtered(_))
    }

    /// Return the filter query when the filter is open.
    pub fn query(&self) -> Option<&str> {
        match self {
            Self::Unfiltered => None,
            Self::Filtered(query) => Some(query),
        }
    }
}

/// Context selection state specialized to Spotify track URI identities.
///
/// This is an alias rather than a second selection implementation, so all
/// scoped/filtered operations, ambiguity status, reconciliation, and bounded
/// source-index mapping come directly from [`ScopedSelection`].
pub type ContextTrackSelection =
    ScopedSelection<ContextSelectionScope, ContextSelectionView, String>;

/// Synchronize a Context adapter from complete and visible Spotify track URI
/// projections without retaining track payloads in the adapter.
pub fn synchronize_context_track_uris<I, J>(
    selection: &mut ContextTrackSelection,
    provider_selection_epoch: u64,
    context_uri: impl Into<String>,
    pane: ContextTrackPane,
    filter_query: Option<&str>,
    complete_uris: I,
    visible_uris: J,
) -> Result<(), ScopedSelectionError>
where
    I: IntoIterator,
    I::Item: AsRef<str>,
    J: IntoIterator,
    J::Item: AsRef<str>,
{
    let complete = complete_uris
        .into_iter()
        .map(|uri| OccurrenceDescriptor::unique(uri.as_ref().to_owned()))
        .collect::<Vec<_>>();
    let visible = visible_uris
        .into_iter()
        .map(|uri| OccurrenceDescriptor::unique(uri.as_ref().to_owned()))
        .collect::<Vec<_>>();
    selection.synchronize(
        ContextSelectionScope::new(provider_selection_epoch, context_uri, pane),
        complete,
        ContextSelectionView::from_query(filter_query),
        visible,
    )
}

/// Resolve selected visible indices for a Context action. Existing keyed rows
/// remain actionable even when the positional cursor is temporarily outside
/// the refreshed visible bounds; cursor-only fallback still enforces bounds.
pub fn context_selected_or_cursor_indices(
    selection: &ContextTrackSelection,
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
    use crate::state::ui::OccurrenceDescriptor;

    fn descriptors(values: &[&str]) -> Vec<OccurrenceDescriptor<String>> {
        values
            .iter()
            .map(|value| OccurrenceDescriptor::unique((*value).to_owned()))
            .collect()
    }

    #[test]
    fn scope_identity_includes_epoch_context_and_exact_pane() {
        let base =
            ContextSelectionScope::new(7, "spotify:playlist:one", ContextTrackPane::Playlist);
        assert_ne!(
            base,
            ContextSelectionScope::new(8, "spotify:playlist:one", ContextTrackPane::Playlist,)
        );
        assert_ne!(
            base,
            ContextSelectionScope::new(7, "spotify:playlist:two", ContextTrackPane::Playlist,)
        );
        assert_ne!(
            base,
            ContextSelectionScope::new(7, "spotify:playlist:one", ContextTrackPane::Tracks,)
        );
        assert_eq!(base.provider_selection_epoch(), 7);
        assert_eq!(base.context_uri(), "spotify:playlist:one");
        assert_eq!(base.pane(), ContextTrackPane::Playlist);
    }

    #[test]
    fn filtered_empty_and_query_views_differ_from_unfiltered() {
        assert_ne!(
            ContextSelectionView::unfiltered(),
            ContextSelectionView::filtered("")
        );
        assert_ne!(
            ContextSelectionView::filtered(""),
            ContextSelectionView::filtered("rock")
        );
        assert!(!ContextSelectionView::unfiltered().is_filtered());
        assert!(ContextSelectionView::filtered("").is_filtered());
        assert_eq!(ContextSelectionView::filtered("rock").query(), Some("rock"));
    }

    #[test]
    fn context_track_selection_uses_real_uri_projection_and_keeps_d1_operations() {
        let scope =
            ContextSelectionScope::new(1, "spotify:playlist:one", ContextTrackPane::Playlist);
        let mut selection = ContextTrackSelection::default();
        selection
            .synchronize(
                scope,
                descriptors(&["spotify:track:a", "spotify:track:b"]),
                ContextSelectionView::unfiltered(),
                descriptors(&["spotify:track:a", "spotify:track:b"]),
            )
            .unwrap();
        selection.extend_range(0, 1).unwrap();
        assert_eq!(selection.selected_visible_indices(), vec![0, 1]);
        assert_eq!(selection.visible_to_full_index(1).unwrap(), 1);
        selection.clear();
        assert!(selection.selected_visible_indices().is_empty());
    }

    #[test]
    fn selected_keys_survive_an_out_of_bounds_cursor_but_cursor_fallback_does_not() {
        let scope =
            ContextSelectionScope::new(1, "spotify:playlist:one", ContextTrackPane::Playlist);
        let mut selection = ContextTrackSelection::default();
        selection
            .synchronize(
                scope.clone(),
                descriptors(&["spotify:track:a", "spotify:track:b"]),
                ContextSelectionView::unfiltered(),
                descriptors(&["spotify:track:a", "spotify:track:b"]),
            )
            .unwrap();
        selection.extend_range(0, 1).unwrap();
        assert_eq!(
            context_selected_or_cursor_indices(&selection, 99).unwrap(),
            vec![0, 1]
        );

        selection.clear();
        selection
            .synchronize(
                scope,
                descriptors(&["spotify:track:a", "spotify:track:a"]),
                ContextSelectionView::unfiltered(),
                descriptors(&["spotify:track:a"]),
            )
            .unwrap();
        assert_eq!(
            context_selected_or_cursor_indices(&selection, 99),
            Err(ScopedSelectionError::OutOfBounds { index: 99, len: 1 })
        );
    }
}
