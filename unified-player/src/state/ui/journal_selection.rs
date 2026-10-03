use super::scoped_selection::{ScopedSelection, ScopedSelectionError};
use super::selection::OccurrenceDescriptor;

/// The page-specific scope of one Journal selection.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum JournalSelectionScope {
    Journal(u64),
    JournalList(u64, String),
}

impl JournalSelectionScope {
    pub const fn journal(provider_selection_epoch: u64) -> Self {
        Self::Journal(provider_selection_epoch)
    }

    pub fn journal_list(provider_selection_epoch: u64, list_id: impl Into<String>) -> Self {
        Self::JournalList(provider_selection_epoch, list_id.into())
    }

    pub const fn provider_selection_epoch(&self) -> u64 {
        match self {
            Self::Journal(epoch) | Self::JournalList(epoch, _) => *epoch,
        }
    }

    pub fn list_id(&self) -> Option<&str> {
        match self {
            Self::Journal(_) => None,
            Self::JournalList(_, list_id) => Some(list_id),
        }
    }
}

/// The exact Journal filter lifecycle. An open empty filter is distinct from
/// the unfiltered page, so opening/closing the popup cannot retain keys.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum JournalSelectionView {
    Unfiltered,
    Filtered(String),
}

impl JournalSelectionView {
    pub const fn unfiltered() -> Self {
        Self::Unfiltered
    }

    pub fn filtered(query: impl Into<String>) -> Self {
        Self::Filtered(query.into())
    }

    pub fn from_query(query: Option<&str>) -> Self {
        query.map_or(Self::Unfiltered, |query| Self::Filtered(query.to_owned()))
    }

    #[allow(dead_code)]
    pub const fn is_filtered(&self) -> bool {
        matches!(self, Self::Filtered(_))
    }

    #[allow(dead_code)]
    pub fn query(&self) -> Option<&str> {
        match self {
            Self::Unfiltered => None,
            Self::Filtered(query) => Some(query),
        }
    }
}

/// Keyed selection for Journal and `JournalList` rows, identified by Spotify URI.
pub type JournalSelection = ScopedSelection<JournalSelectionScope, JournalSelectionView, String>;

/// Synchronize a Journal projection. `complete_uris` may include rows whose
/// payload is missing; the caller supplies only renderable rows as visible URIs.
pub fn synchronize_journal_uris<I, J>(
    selection: &mut JournalSelection,
    scope: JournalSelectionScope,
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
        scope,
        complete,
        JournalSelectionView::from_query(filter_query),
        visible,
    )
}

/// Resolve keyed rows in visible order, falling back to a bounds-checked
/// cursor when no keyed rows are available.
pub fn journal_selected_or_cursor_indices(
    selection: &JournalSelection,
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

    #[test]
    fn scope_distinguishes_epoch_and_page_list() {
        assert_ne!(
            JournalSelectionScope::journal(1),
            JournalSelectionScope::journal(2)
        );
        assert_ne!(
            JournalSelectionScope::journal(1),
            JournalSelectionScope::journal_list(1, "list")
        );
        assert_ne!(
            JournalSelectionScope::journal_list(1, "one"),
            JournalSelectionScope::journal_list(1, "two")
        );
        assert_eq!(
            JournalSelectionScope::journal(3).provider_selection_epoch(),
            3
        );
        assert_eq!(JournalSelectionScope::journal(3).list_id(), None);
        assert_eq!(
            JournalSelectionScope::journal_list(3, "list").list_id(),
            Some("list")
        );
    }

    #[test]
    fn filter_open_change_and_close_are_distinct_views() {
        assert_ne!(
            JournalSelectionView::unfiltered(),
            JournalSelectionView::filtered("")
        );
        assert_ne!(
            JournalSelectionView::filtered(""),
            JournalSelectionView::filtered("rock")
        );

        let mut selection = JournalSelection::default();
        let scope = JournalSelectionScope::journal(1);
        synchronize_journal_uris(&mut selection, scope.clone(), None, ["a", "b"], ["a", "b"])
            .unwrap();
        selection.extend_range(0, 1).unwrap();
        assert_eq!(selection.selected_visible_indices(), vec![0, 1]);

        synchronize_journal_uris(
            &mut selection,
            scope.clone(),
            Some(""),
            ["a", "b"],
            ["a", "b"],
        )
        .unwrap();
        assert!(selection.selected_visible_indices().is_empty());
        selection.extend_range(0, 1).unwrap();
        synchronize_journal_uris(
            &mut selection,
            scope.clone(),
            Some("rock"),
            ["a", "b"],
            ["a"],
        )
        .unwrap();
        assert!(selection.selected_visible_indices().is_empty());
        selection.extend_range(0, 0).unwrap();
        synchronize_journal_uris(&mut selection, scope, None, ["a", "b"], ["a", "b"]).unwrap();
        assert!(selection.selected_visible_indices().is_empty());
    }

    #[test]
    fn visible_mapping_can_skip_missing_full_payloads() {
        let mut selection = JournalSelection::default();
        synchronize_journal_uris(
            &mut selection,
            JournalSelectionScope::journal_list(1, "list"),
            Some("query"),
            ["a", "missing", "b"],
            ["b", "a"],
        )
        .unwrap();
        assert_eq!(selection.visible_to_full_index(0), Ok(2));
        assert_eq!(selection.visible_to_full_index(1), Ok(0));
    }

    #[test]
    fn duplicate_uri_projection_is_ambiguous_and_cursor_fallback_is_bounded() {
        let mut selection = JournalSelection::default();
        synchronize_journal_uris(
            &mut selection,
            JournalSelectionScope::journal_list(1, "list"),
            None,
            ["a", "a"],
            ["a", "a"],
        )
        .unwrap();
        assert_eq!(
            selection.status(),
            super::super::scoped_selection::ScopedSelectionStatus::Ambiguous
        );
        assert_eq!(
            journal_selected_or_cursor_indices(&selection, 1),
            Ok(vec![1])
        );
        assert!(matches!(
            selection.extend_range(0, 1),
            Err(ScopedSelectionError::AmbiguousProjection)
        ));
    }

    #[test]
    fn visible_uri_missing_from_complete_projection_is_transactional() {
        let mut selection = JournalSelection::default();
        synchronize_journal_uris(
            &mut selection,
            JournalSelectionScope::journal(1),
            None,
            ["a"],
            ["a"],
        )
        .unwrap();
        let before = selection.clone();
        assert_eq!(
            synchronize_journal_uris(
                &mut selection,
                JournalSelectionScope::journal(1),
                None,
                ["a"],
                ["missing"],
            ),
            Err(ScopedSelectionError::VisibleOccurrenceNotInComplete { visible_index: 0 })
        );
        assert_eq!(selection, before);
    }

    #[test]
    fn same_scope_reorder_retains_selected_uri() {
        let mut selection = JournalSelection::default();
        let scope = JournalSelectionScope::journal(1);
        synchronize_journal_uris(&mut selection, scope.clone(), None, ["a", "b"], ["a", "b"])
            .unwrap();
        selection.extend_range(0, 0).unwrap();

        synchronize_journal_uris(&mut selection, scope, None, ["b", "a"], ["b", "a"]).unwrap();

        assert_eq!(selection.selected_visible_indices(), vec![1]);
    }
}
