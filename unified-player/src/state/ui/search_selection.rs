use crate::config::ActiveProvider;

use super::{
    DatasetGeneration, OccurrenceDescriptor, OccurrenceKey, SelectionChange, SelectionError,
    SelectionState, ValidatedOccurrenceProjection,
};

/// The Search result pane that supplies a row identity namespace.
///
/// Episodes intentionally do not appear here. Search episodes remain
/// cursor-only until a later packet gives them a stable occurrence contract.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub enum SearchPane {
    SpotifyTracks,
    YouTubeSongs,
    YouTubeVideos,
}

/// The exact lifecycle scope for one Search selection.
///
/// The submitted query is kept verbatim. A provider, provider epoch, or pane
/// change is a new scope even when the result IDs happen to be identical. The
/// epoch is deliberately opaque and carries no account identifier.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SearchScope {
    provider: ActiveProvider,
    provider_epoch: u64,
    submitted_query: String,
    pane: SearchPane,
}

impl SearchScope {
    /// Construct a Search selection scope.
    pub fn new(
        provider: ActiveProvider,
        provider_epoch: u64,
        submitted_query: impl Into<String>,
        pane: SearchPane,
    ) -> Self {
        Self {
            provider,
            provider_epoch,
            submitted_query: submitted_query.into(),
            pane,
        }
    }

    /// Return the result pane for this scope.
    pub const fn pane(&self) -> SearchPane {
        self.pane
    }
}

/// A provider media identity namespaced by its Search pane.
///
/// The raw provider ID is the only row identity input. Display metadata,
/// model values, and positional ordinals are deliberately absent.
#[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
struct SearchMediaIdentity {
    pane: SearchPane,
    raw_id: String,
}

impl SearchMediaIdentity {
    /// Construct an identity from its pane namespace and provider ID.
    fn new(pane: SearchPane, raw_id: impl Into<String>) -> Self {
        Self {
            pane,
            raw_id: raw_id.into(),
        }
    }

    /// Return the pane namespace carried by this identity.
    #[cfg(test)]
    const fn pane(&self) -> SearchPane {
        self.pane
    }

    /// Return the raw provider media ID.
    #[cfg(test)]
    fn raw_id(&self) -> &str {
        &self.raw_id
    }
}

/// The observable state of the current Search pane projection.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
enum SearchSelectionStatus {
    /// No Search scope has been synchronized yet.
    #[default]
    Unscoped,
    /// The current scope has no rows.
    Empty,
    /// The complete projection has unique semantic identities.
    Ready,
    /// A semantic identity repeats, so keyed multi-selection is unavailable
    /// for the complete pane projection.
    Ambiguous,
}

impl SearchSelectionStatus {
    /// Return whether the status permits keyed range selection.
    #[cfg(test)]
    const fn supports_keyed_selection(self) -> bool {
        matches!(self, Self::Ready | Self::Empty)
    }
}

/// A bounded error from Search selection operations.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SearchSelectionError {
    /// No Search scope has been synchronized.
    NoScope,
    /// The complete pane projection is ambiguous, so keyed range selection
    /// cannot be applied.
    AmbiguousProjection,
    /// A requested cursor or target is outside the current row projection.
    OutOfBounds { index: usize, len: usize },
    /// The generation counter cannot advance without wrapping.
    GenerationExhausted,
    /// A generic selection-kernel operation was rejected.
    Kernel(SelectionError),
}

/// The pure keyed selection adapter used by the supported Search panes.
///
/// This type owns one generic [`SelectionState`] and keeps the current scope,
/// complete ordered identity signature, and validated projection alongside
/// it. It never stores a raw row index as selection identity.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SearchSelection {
    selection: SelectionState<SearchKey>,
    scope: Option<SearchScope>,
    identity_signature: Vec<SearchMediaIdentity>,
    projection: Option<SearchProjection>,
    status: SearchSelectionStatus,
}

type SearchKey = OccurrenceKey<SearchMediaIdentity>;
type SearchProjection = ValidatedOccurrenceProjection<SearchMediaIdentity>;

impl Default for SearchSelection {
    fn default() -> Self {
        Self::with_generation(DatasetGeneration::INITIAL)
    }
}

impl SearchSelection {
    /// Construct an empty adapter at a caller-supplied generation.
    ///
    /// The generation constructor is useful to keep overflow behavior
    /// testable without performing an unbounded number of synchronizations.
    #[cfg(test)]
    pub fn new(generation: DatasetGeneration) -> Self {
        Self::with_generation(generation)
    }

    fn with_generation(generation: DatasetGeneration) -> Self {
        Self {
            selection: SelectionState::new(generation),
            scope: None,
            identity_signature: Vec::new(),
            projection: None,
            status: SearchSelectionStatus::Unscoped,
        }
    }

    /// Synchronize the complete, unfiltered result projection for `scope`.
    ///
    /// A new scope resets selected keys and the range anchor. A changed
    /// projection in the same scope advances the generation and reconciles
    /// matching keys, preserving selections across reorder. Repeated
    /// semantic IDs are retained as rows but make the whole pane ambiguous;
    /// keyed selection is cleared and callers can use cursor fallback.
    pub fn synchronize<I, S>(
        &mut self,
        scope: SearchScope,
        raw_ids: I,
    ) -> Result<(), SearchSelectionError>
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        let identity_signature = raw_ids
            .into_iter()
            .map(|raw_id| SearchMediaIdentity::new(scope.pane(), raw_id))
            .collect::<Vec<_>>();

        let same_scope = self.scope.as_ref() == Some(&scope);
        if same_scope && self.identity_signature == identity_signature {
            return Ok(());
        }

        let next_generation = self
            .selection
            .generation()
            .checked_next()
            .ok_or(SearchSelectionError::GenerationExhausted)?;

        let descriptors = identity_signature
            .iter()
            .cloned()
            .map(OccurrenceDescriptor::unique)
            .collect::<Vec<_>>();
        let validated = SearchProjection::try_new(descriptors);

        if same_scope {
            match &validated {
                Ok(projection) => {
                    self.selection
                        .reconcile(next_generation, projection.keys())
                        .map_err(SearchSelectionError::Kernel)?;
                }
                Err(_) => {
                    // An ambiguous complete projection cannot safely be
                    // reconciled by semantic key. Advance the generation and
                    // explicitly drop all selected keys and the anchor.
                    self.selection
                        .reconcile(next_generation, &[])
                        .map_err(SearchSelectionError::Kernel)?;
                }
            }
        } else {
            self.selection
                .reset_scope(next_generation)
                .map_err(SearchSelectionError::Kernel)?;
        }

        self.scope = Some(scope);
        self.identity_signature = identity_signature;
        self.projection = validated.ok();
        self.status = match &self.projection {
            None => SearchSelectionStatus::Ambiguous,
            Some(projection) if projection.is_empty() => SearchSelectionStatus::Empty,
            Some(_) => SearchSelectionStatus::Ready,
        };

        Ok(())
    }

    /// Return the current projection status.
    #[cfg(test)]
    const fn status(&self) -> SearchSelectionStatus {
        self.status
    }

    /// Return the accepted dataset generation.
    #[cfg(test)]
    const fn generation(&self) -> DatasetGeneration {
        self.selection.generation()
    }

    /// Return the number of rows in the complete current projection.
    #[cfg(test)]
    fn len(&self) -> usize {
        self.identity_signature.len()
    }

    /// Return whether keyed range selection is available for this projection.
    #[cfg(test)]
    const fn multi_selection_enabled(&self) -> bool {
        self.status.supports_keyed_selection()
    }

    /// Return whether no keys are selected.
    #[cfg(test)]
    fn selection_is_empty(&self) -> bool {
        self.selection.is_empty()
    }

    /// Extend the inclusive range from the stored anchor to `target`.
    pub fn extend_range(
        &mut self,
        cursor: Option<usize>,
        target: Option<usize>,
    ) -> Result<SelectionChange, SearchSelectionError> {
        if self.scope.is_none() {
            return Err(SearchSelectionError::NoScope);
        }
        let projection = self
            .projection
            .as_ref()
            .ok_or(SearchSelectionError::AmbiguousProjection)?;
        let cursor = cursor.ok_or(SearchSelectionError::Kernel(SelectionError::MissingCursor))?;
        let target = target.ok_or(SearchSelectionError::Kernel(SelectionError::MissingTarget))?;
        self.ensure_index(cursor)?;
        self.ensure_index(target)?;
        self.selection
            .extend_range(projection.keys(), Some(cursor), Some(target))
            .map_err(SearchSelectionError::Kernel)
    }

    /// Select every row in the current keyed Search projection.
    ///
    /// The operation is visible-only because Search currently exposes one
    /// unfiltered pane at a time. Ambiguous and unsynchronized projections
    /// fail before touching selection state.
    pub fn select_all_visible(&mut self) -> Result<SelectionChange, SearchSelectionError> {
        let keys = self.keyed_projection()?.keys().to_vec();
        self.selection
            .select_all(&keys)
            .map_err(SearchSelectionError::Kernel)
    }

    /// Invert every row in the current keyed Search projection.
    ///
    /// As with [`Self::select_all_visible`], this never falls back to the
    /// cursor and therefore refuses ambiguous or cursor-only panes.
    pub fn invert_visible(&mut self) -> Result<SelectionChange, SearchSelectionError> {
        let keys = self.keyed_projection()?.keys().to_vec();
        self.selection
            .invert(&keys)
            .map_err(SearchSelectionError::Kernel)
    }

    /// Clear selected keys and the range anchor without changing scope.
    pub fn clear_selection(&mut self) -> SelectionChange {
        self.selection.clear()
    }

    /// Return selected row indices in current projection order.
    pub fn selected_indices(&self) -> Vec<usize> {
        let Some(projection) = self.projection.as_ref() else {
            return Vec::new();
        };
        projection
            .keys()
            .iter()
            .enumerate()
            .filter_map(|(index, key)| self.selection.contains(key).then_some(index))
            .collect()
    }

    /// Resolve selected rows, or fall back to the supplied cursor when no
    /// keyed rows are selected. This keeps ambiguous panes cursor-operable.
    pub fn selected_or_cursor(
        &self,
        cursor: Option<usize>,
    ) -> Result<Vec<usize>, SearchSelectionError> {
        if self.scope.is_none() {
            return Err(SearchSelectionError::NoScope);
        }
        let selected = self.selected_indices();
        if !selected.is_empty() {
            return Ok(selected);
        }
        let cursor = cursor.ok_or(SearchSelectionError::Kernel(SelectionError::MissingCursor))?;
        self.ensure_index(cursor)?;
        Ok(vec![cursor])
    }

    /// Return whether the row at `index` is keyed-selected.
    #[cfg(test)]
    fn is_selected(&self, index: usize) -> Result<bool, SearchSelectionError> {
        if self.scope.is_none() {
            return Err(SearchSelectionError::NoScope);
        }
        self.ensure_index(index)?;
        let Some(projection) = self.projection.as_ref() else {
            return Ok(false);
        };
        Ok(self.selection.contains(&projection.keys()[index]))
    }

    /// Resolve the current range anchor to its row index, if it remains
    /// present after a refresh.
    #[cfg(test)]
    fn anchor_index(&self) -> Option<usize> {
        let anchor = self.selection.anchor()?;
        self.projection
            .as_ref()?
            .keys()
            .iter()
            .position(|key| key == anchor)
    }

    fn ensure_index(&self, index: usize) -> Result<(), SearchSelectionError> {
        let len = self.identity_signature.len();
        if index >= len {
            return Err(SearchSelectionError::OutOfBounds { index, len });
        }
        Ok(())
    }

    fn keyed_projection(&self) -> Result<&SearchProjection, SearchSelectionError> {
        if self.scope.is_none() {
            return Err(SearchSelectionError::NoScope);
        }
        self.projection
            .as_ref()
            .ok_or(SearchSelectionError::AmbiguousProjection)
    }
}

impl super::MultiSelectModel for SearchSelection {
    type Error = SearchSelectionError;

    fn select_all_visible(&mut self) -> Result<SelectionChange, Self::Error> {
        Self::select_all_visible(self)
    }

    fn invert_visible(&mut self) -> Result<SelectionChange, Self::Error> {
        Self::invert_visible(self)
    }

    fn extend_visible_range(
        &mut self,
        cursor: usize,
        target: usize,
    ) -> Result<SelectionChange, Self::Error> {
        Self::extend_range(self, Some(cursor), Some(target))
    }

    fn clear_selection(&mut self) -> SelectionChange {
        Self::clear_selection(self)
    }

    fn selected_visible_indices(&self) -> Vec<usize> {
        Self::selected_indices(self)
    }
}

impl From<SelectionError> for SearchSelectionError {
    fn from(error: SelectionError) -> Self {
        Self::Kernel(error)
    }
}

#[cfg(test)]
mod tests {
    use super::{
        SearchMediaIdentity, SearchPane, SearchScope, SearchSelection, SearchSelectionError,
        SearchSelectionStatus,
    };
    use crate::config::ActiveProvider;
    use crate::state::ui::DatasetGeneration;

    fn scope(provider: ActiveProvider, query: &str, pane: SearchPane) -> SearchScope {
        SearchScope::new(provider, 0, query, pane)
    }

    fn scope_at_epoch(
        provider: ActiveProvider,
        epoch: u64,
        query: &str,
        pane: SearchPane,
    ) -> SearchScope {
        SearchScope::new(provider, epoch, query, pane)
    }

    #[test]
    fn scope_distinguishes_provider_query_and_pane() {
        let base = scope(
            ActiveProvider::Spotify,
            "same query",
            SearchPane::SpotifyTracks,
        );
        assert_ne!(
            base,
            scope(
                ActiveProvider::YouTubeMusic,
                "same query",
                SearchPane::SpotifyTracks
            )
        );
        assert_ne!(
            base,
            scope(
                ActiveProvider::Spotify,
                "other query",
                SearchPane::SpotifyTracks
            )
        );
        assert_ne!(
            base,
            scope(
                ActiveProvider::Spotify,
                "same query",
                SearchPane::YouTubeSongs
            )
        );
    }

    #[test]
    fn new_scope_clears_selected_keys_and_anchor() {
        let first = scope(ActiveProvider::Spotify, "one", SearchPane::SpotifyTracks);
        let second = scope(ActiveProvider::Spotify, "two", SearchPane::SpotifyTracks);
        let mut selection = SearchSelection::default();
        selection
            .synchronize(first, ["a", "b", "c"])
            .expect("initial sync");
        selection
            .extend_range(Some(0), Some(1))
            .expect("range exists");
        assert_eq!(selection.selected_indices(), [0, 1]);
        assert_eq!(selection.anchor_index(), Some(0));

        selection
            .synchronize(second, ["b", "c"])
            .expect("new scope sync");
        assert!(selection.selection_is_empty());
        assert_eq!(selection.anchor_index(), None);
        assert_eq!(selection.status(), SearchSelectionStatus::Ready);
    }

    #[test]
    fn provider_epoch_change_clears_selection_for_identical_results() {
        let first = scope_at_epoch(
            ActiveProvider::Spotify,
            0,
            "same query",
            SearchPane::SpotifyTracks,
        );
        let second = scope_at_epoch(
            ActiveProvider::Spotify,
            1,
            "same query",
            SearchPane::SpotifyTracks,
        );
        let mut selection = SearchSelection::default();
        selection
            .synchronize(first, ["a", "b", "c"])
            .expect("initial sync");
        selection
            .extend_range(Some(0), Some(1))
            .expect("range exists");

        selection
            .synchronize(second, ["a", "b", "c"])
            .expect("epoch change sync");
        assert!(selection.selection_is_empty());
        assert_eq!(selection.anchor_index(), None);
        assert_eq!(selection.status(), SearchSelectionStatus::Ready);
    }

    #[test]
    fn unchanged_provider_epoch_preserves_same_scope_reconciliation() {
        let scope = scope_at_epoch(
            ActiveProvider::YouTubeMusic,
            7,
            "same query",
            SearchPane::YouTubeSongs,
        );
        let mut selection = SearchSelection::default();
        selection
            .synchronize(scope.clone(), ["a", "b"])
            .expect("initial sync");
        selection
            .extend_range(Some(0), Some(0))
            .expect("range exists");

        selection
            .synchronize(scope, ["b", "a"])
            .expect("same epoch refresh");
        assert_eq!(selection.selected_indices(), [1]);
        assert_eq!(selection.anchor_index(), Some(1));
    }

    #[test]
    fn exact_repeat_keeps_generation_stable() {
        let mut selection = SearchSelection::default();
        let scope = scope(ActiveProvider::Spotify, "repeat", SearchPane::SpotifyTracks);
        selection
            .synchronize(scope.clone(), ["a", "b"])
            .expect("initial sync");
        let generation = selection.generation();
        selection
            .synchronize(scope, ["a", "b"])
            .expect("repeat sync");
        assert_eq!(selection.generation(), generation);
    }

    #[test]
    fn explicit_clear_drops_keys_and_anchor_without_restoring_them_on_repeat_sync() {
        let mut selection = SearchSelection::default();
        let scope = scope(ActiveProvider::Spotify, "clear", SearchPane::SpotifyTracks);
        selection
            .synchronize(scope.clone(), ["a", "b"])
            .expect("initial sync");
        selection
            .extend_range(Some(0), Some(1))
            .expect("range exists");
        let generation = selection.generation();

        assert_eq!(selection.clear_selection(), super::SelectionChange::Changed);
        assert!(selection.selection_is_empty());
        assert_eq!(selection.anchor_index(), None);

        selection
            .synchronize(scope, ["a", "b"])
            .expect("unchanged projection sync");
        assert_eq!(selection.generation(), generation);
        assert!(selection.selection_is_empty());
        assert_eq!(selection.anchor_index(), None);
    }

    #[test]
    fn reorder_preserves_keys_and_changes_resolved_indices() {
        let mut selection = SearchSelection::default();
        let scope = scope(
            ActiveProvider::Spotify,
            "reorder",
            SearchPane::SpotifyTracks,
        );
        selection
            .synchronize(scope.clone(), ["a", "b", "c"])
            .expect("initial sync");
        selection
            .extend_range(Some(1), Some(1))
            .expect("select one row");
        assert_eq!(selection.selected_indices(), [1]);

        selection
            .synchronize(scope, ["c", "a", "b"])
            .expect("refresh sync");
        assert_eq!(selection.selected_indices(), [2]);
        assert_eq!(selection.anchor_index(), Some(2));
    }

    #[test]
    fn refresh_drops_missing_keys_and_retains_present_keys() {
        let mut selection = SearchSelection::default();
        let scope = scope(
            ActiveProvider::YouTubeMusic,
            "refresh",
            SearchPane::YouTubeVideos,
        );
        selection
            .synchronize(scope.clone(), ["a", "b", "c"])
            .expect("initial sync");
        selection
            .extend_range(Some(0), Some(2))
            .expect("select rows");
        selection
            .synchronize(scope, ["c", "x"])
            .expect("refresh sync");
        assert_eq!(selection.selected_indices(), [0]);
        assert_eq!(selection.anchor_index(), None);
    }

    #[test]
    fn duplicate_projection_disables_multi_selection_and_uses_cursor_fallback() {
        let mut selection = SearchSelection::default();
        selection
            .synchronize(
                scope(
                    ActiveProvider::YouTubeMusic,
                    "duplicates",
                    SearchPane::YouTubeSongs,
                ),
                ["same", "same"],
            )
            .expect("ambiguous rows are observable, not fatal");
        assert_eq!(selection.status(), SearchSelectionStatus::Ambiguous);
        assert!(!selection.multi_selection_enabled());
        assert_eq!(selection.selected_indices(), Vec::<usize>::new());
        assert_eq!(selection.selected_or_cursor(Some(1)), Ok(vec![1]));
        assert_eq!(
            selection.extend_range(Some(0), Some(1)),
            Err(SearchSelectionError::AmbiguousProjection)
        );
    }

    #[test]
    fn select_all_and_invert_are_visible_only_and_repeatable() {
        let mut selection = SearchSelection::default();
        let scope = scope(ActiveProvider::Spotify, "bulk", SearchPane::SpotifyTracks);
        selection
            .synchronize(scope, ["a", "b", "c"])
            .expect("projection sync");
        let generation = selection.generation();

        assert_eq!(
            selection.select_all_visible(),
            Ok(super::SelectionChange::Changed)
        );
        assert_eq!(selection.selected_indices(), [0, 1, 2]);
        assert_eq!(selection.generation(), generation);
        assert_eq!(
            selection.invert_visible(),
            Ok(super::SelectionChange::Changed)
        );
        assert!(selection.selection_is_empty());
        assert_eq!(
            selection.invert_visible(),
            Ok(super::SelectionChange::Changed)
        );
        assert_eq!(selection.selected_indices(), [0, 1, 2]);
    }

    #[test]
    fn select_all_and_invert_refuse_ambiguous_or_unsynchronized_projection() {
        let mut selection = SearchSelection::default();
        assert_eq!(
            selection.select_all_visible(),
            Err(SearchSelectionError::NoScope)
        );
        assert_eq!(
            selection.invert_visible(),
            Err(SearchSelectionError::NoScope)
        );

        selection
            .synchronize(
                scope(
                    ActiveProvider::YouTubeMusic,
                    "ambiguous",
                    SearchPane::YouTubeVideos,
                ),
                ["same", "same"],
            )
            .expect("ambiguous projection is bounded");
        let before_refusal = selection.clone();
        assert_eq!(
            selection.select_all_visible(),
            Err(SearchSelectionError::AmbiguousProjection)
        );
        assert_eq!(
            selection.invert_visible(),
            Err(SearchSelectionError::AmbiguousProjection)
        );
        assert_eq!(selection, before_refusal);
        assert!(selection.selection_is_empty());
    }

    #[test]
    fn select_all_and_invert_empty_projection_are_bounded() {
        let mut selection = SearchSelection::default();
        selection
            .synchronize(
                scope(ActiveProvider::Spotify, "empty", SearchPane::SpotifyTracks),
                std::iter::empty::<&str>(),
            )
            .expect("empty projection");
        assert_eq!(
            selection.select_all_visible(),
            Ok(super::SelectionChange::Unchanged)
        );
        assert_eq!(
            selection.invert_visible(),
            Ok(super::SelectionChange::Unchanged)
        );
        assert!(selection.selection_is_empty());
    }

    #[test]
    fn ambiguous_refresh_clears_previously_selected_keys_and_anchor() {
        let mut selection = SearchSelection::default();
        let scope = scope(
            ActiveProvider::YouTubeMusic,
            "becomes ambiguous",
            SearchPane::YouTubeVideos,
        );
        selection
            .synchronize(scope.clone(), ["a", "b"])
            .expect("initial sync");
        selection
            .extend_range(Some(0), Some(1))
            .expect("range exists");

        selection
            .synchronize(scope, ["a", "a"])
            .expect("ambiguity is a bounded status");

        assert_eq!(selection.status(), SearchSelectionStatus::Ambiguous);
        assert!(selection.selection_is_empty());
        assert_eq!(selection.anchor_index(), None);
        assert_eq!(selection.selected_or_cursor(Some(1)), Ok(vec![1]));
    }

    #[test]
    fn zero_one_many_and_out_of_bounds_are_bounded() {
        let mut selection = SearchSelection::default();
        let empty = scope(ActiveProvider::Spotify, "empty", SearchPane::SpotifyTracks);
        selection
            .synchronize(empty, std::iter::empty::<&str>())
            .expect("empty sync");
        assert_eq!(selection.status(), SearchSelectionStatus::Empty);
        assert_eq!(selection.len(), 0);
        assert_eq!(
            selection.selected_or_cursor(Some(0)),
            Err(SearchSelectionError::OutOfBounds { index: 0, len: 0 })
        );

        let one = scope(ActiveProvider::Spotify, "one", SearchPane::SpotifyTracks);
        selection.synchronize(one, ["only"]).expect("one sync");
        assert_eq!(
            selection.extend_range(Some(0), Some(0)),
            Ok(super::SelectionChange::Changed)
        );
        assert_eq!(selection.selected_indices(), [0]);
        assert_eq!(
            selection.is_selected(1),
            Err(SearchSelectionError::OutOfBounds { index: 1, len: 1 })
        );
    }

    #[test]
    fn selected_markers_follow_keys_after_reorder() {
        let mut selection = SearchSelection::default();
        let scope = scope(
            ActiveProvider::Spotify,
            "markers",
            SearchPane::SpotifyTracks,
        );
        selection
            .synchronize(scope.clone(), ["left", "middle", "right"])
            .expect("initial sync");
        selection
            .extend_range(Some(0), Some(0))
            .expect("select left");
        assert_eq!(selection.is_selected(0), Ok(true));
        assert_eq!(selection.is_selected(1), Ok(false));

        selection
            .synchronize(scope, ["middle", "right", "left"])
            .expect("reorder sync");
        assert_eq!(selection.is_selected(0), Ok(false));
        assert_eq!(selection.is_selected(2), Ok(true));
    }

    #[test]
    fn generation_exhaustion_is_bounded() {
        let mut selection = SearchSelection::new(DatasetGeneration::new(u64::MAX));
        let result = selection.synchronize(
            scope(
                ActiveProvider::Spotify,
                "overflow",
                SearchPane::SpotifyTracks,
            ),
            ["id"],
        );
        assert_eq!(result, Err(SearchSelectionError::GenerationExhausted));
        assert_eq!(selection.status(), SearchSelectionStatus::Unscoped);
    }

    #[test]
    fn identity_is_pane_namespaced_without_metadata() {
        let left = SearchMediaIdentity::new(SearchPane::SpotifyTracks, "same");
        let right = SearchMediaIdentity::new(SearchPane::YouTubeSongs, "same");
        assert_ne!(left, right);
        assert_eq!(left.raw_id(), "same");
        assert_eq!(left.pane(), SearchPane::SpotifyTracks);
    }
}
