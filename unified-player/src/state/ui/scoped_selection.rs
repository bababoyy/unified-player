use std::collections::BTreeMap;

use super::selection::{
    DatasetGeneration, OccurrenceDescriptor, OccurrenceKey, OccurrenceProjectionError,
    SelectionChange, SelectionError, SelectionState, ValidatedOccurrenceProjection,
};

/// The bounded state of one scoped selection projection.
///
/// `Ready` means that the complete projection has unique occurrence keys and
/// keyed operations are available. `Empty` is a valid, empty complete
/// projection. `Ambiguous` means that the complete projection could not be
/// keyed safely; callers may still use the visible row count for cursor
/// fallback, but no keyed selection is exposed.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum ScopedSelectionStatus {
    #[default]
    Unscoped,
    Empty,
    Ready,
    Ambiguous,
}

impl ScopedSelectionStatus {
    /// Return whether occurrence-keyed selection operations are available.
    pub const fn keyed_selection_available(self) -> bool {
        matches!(self, Self::Empty | Self::Ready)
    }
}

/// A privacy-safe category for a projection conflict.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ScopedProjectionConflict {
    DuplicateUnique,
    MixedUniqueAndTokenized,
    DuplicateToken,
}

/// A bounded failure from a scoped selection adapter operation.
///
/// The error deliberately contains only categories, indices, lengths, and
/// generations. It never exposes caller-owned scope, view, identity, token,
/// or row payload values.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ScopedSelectionError {
    /// No synchronized scope has been installed yet.
    Unscoped,
    /// A keyed operation was requested while the complete projection is
    /// ambiguous.
    AmbiguousProjection,
    /// The visible projection is malformed.
    InvalidVisibleProjection {
        conflict: ScopedProjectionConflict,
        first_index: usize,
        second_index: usize,
    },
    /// A visible occurrence does not exist in the validated complete
    /// projection.
    VisibleOccurrenceNotInComplete { visible_index: usize },
    /// A cursor or visible row index is outside the current visible bounds.
    OutOfBounds { index: usize, len: usize },
    /// The current state does not have a valid complete/visible mapping.
    NoValidProjection,
    /// The monotonic dataset generation cannot advance further.
    GenerationExhausted,
    /// A lower-level selection-kernel operation was rejected.
    Kernel(SelectionError),
}

impl From<SelectionError> for ScopedSelectionError {
    fn from(error: SelectionError) -> Self {
        Self::Kernel(error)
    }
}

/// A reusable selection adapter for one caller-owned scope and view.
///
/// The adapter owns only opaque identity descriptors and bounded projection
/// metadata. Callers retain row payloads, cursors, and all provider-specific
/// state. Every synchronization validates the complete projection first, then
/// validates the visible projection and its membership in the complete one.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ScopedSelection<S, V, M, O = ()> {
    selection: SelectionState<OccurrenceKey<M, O>>,
    scope: Option<S>,
    view: Option<V>,
    complete_signature: Vec<OccurrenceDescriptor<M, O>>,
    complete_projection: Option<ValidatedOccurrenceProjection<M, O>>,
    visible_signature: Vec<OccurrenceDescriptor<M, O>>,
    visible_keys: Vec<OccurrenceKey<M, O>>,
    visible_full_indices: Vec<usize>,
    cursor_only_visible_len: usize,
    cursor_only: bool,
    status: ScopedSelectionStatus,
}

impl<S, V, M: Clone + Ord, O: Clone + Ord> Default for ScopedSelection<S, V, M, O> {
    fn default() -> Self {
        Self::new(DatasetGeneration::INITIAL)
    }
}

impl<S, V, M: Clone + Ord, O: Clone + Ord> ScopedSelection<S, V, M, O> {
    /// Construct an unscoped adapter at `generation`.
    pub fn new(generation: DatasetGeneration) -> Self {
        Self {
            selection: SelectionState::new(generation),
            scope: None,
            view: None,
            complete_signature: Vec::new(),
            complete_projection: None,
            visible_signature: Vec::new(),
            visible_keys: Vec::new(),
            visible_full_indices: Vec::new(),
            cursor_only_visible_len: 0,
            cursor_only: false,
            status: ScopedSelectionStatus::Unscoped,
        }
    }

    /// Return the current accepted dataset generation.
    pub const fn generation(&self) -> DatasetGeneration {
        self.selection.generation()
    }

    /// Return the current bounded projection status.
    pub const fn status(&self) -> ScopedSelectionStatus {
        self.status
    }

    /// Return whether keyed selection operations are available.
    pub const fn keyed_selection_available(&self) -> bool {
        self.status.keyed_selection_available()
    }

    /// Return the number of rows in the complete projection.
    pub fn full_len(&self) -> usize {
        self.complete_signature.len()
    }

    /// Return the number of rows in the visible projection.
    pub fn visible_len(&self) -> usize {
        self.visible_signature
            .len()
            .max(self.cursor_only_visible_len)
    }

    /// Synchronize a projection that cannot be keyed safely.
    ///
    /// This is used when a row has no stable semantic identity (for example,
    /// an unknown native playable). The adapter retains only the bounded row
    /// count needed for cursor fallback; it does not manufacture a descriptor
    /// or occurrence key for the unkeyable rows.
    pub fn synchronize_cursor_only(
        &mut self,
        scope: S,
        view: V,
        visible_len: usize,
    ) -> Result<(), ScopedSelectionError>
    where
        S: Clone + Eq,
        V: Clone + Eq,
    {
        let exact_repeat = self.scope.as_ref() == Some(&scope)
            && self.view.as_ref() == Some(&view)
            && self.status == ScopedSelectionStatus::Ambiguous
            && self.complete_signature.is_empty()
            && self.visible_signature.is_empty()
            && self.cursor_only_visible_len == visible_len;
        if exact_repeat {
            return Ok(());
        }

        let next_generation = self
            .selection
            .generation()
            .checked_next()
            .ok_or(ScopedSelectionError::GenerationExhausted)?;
        let mut next_selection = self.selection.clone();
        next_selection.reset_scope(next_generation)?;

        self.selection = next_selection;
        self.scope = Some(scope);
        self.view = Some(view);
        self.complete_signature.clear();
        self.complete_projection = None;
        self.visible_signature.clear();
        self.visible_keys.clear();
        self.visible_full_indices.clear();
        self.cursor_only_visible_len = visible_len;
        self.cursor_only = true;
        self.status = ScopedSelectionStatus::Ambiguous;
        Ok(())
    }

    /// Synchronize a complete scope and its ordered visible projection.
    ///
    /// The complete and visible descriptor lists are retained as separate
    /// signatures. An exact repeat is a no-op. Every other accepted update
    /// advances the generation; scope/view changes clear selection, while
    /// same-scope refreshes reconcile selected keys with the new visible
    /// projection.
    pub fn synchronize<I, J>(
        &mut self,
        scope: S,
        complete: I,
        view: V,
        visible: J,
    ) -> Result<(), ScopedSelectionError>
    where
        I: IntoIterator<Item = OccurrenceDescriptor<M, O>>,
        J: IntoIterator<Item = OccurrenceDescriptor<M, O>>,
        S: Clone + Eq,
        V: Clone + Eq,
    {
        self.synchronize_with_policy(scope, complete, view, visible, false)
    }

    /// Synchronize while retaining selected keys that remain in the complete
    /// dataset but are temporarily hidden by the visible projection.
    ///
    /// This is intended for editable filtered lists: changing a filter must
    /// not silently discard selection ownership, while actions still resolve
    /// only the selected rows in the visible projection.
    pub fn synchronize_retaining_hidden<I, J>(
        &mut self,
        scope: S,
        complete: I,
        view: V,
        visible: J,
    ) -> Result<(), ScopedSelectionError>
    where
        I: IntoIterator<Item = OccurrenceDescriptor<M, O>>,
        J: IntoIterator<Item = OccurrenceDescriptor<M, O>>,
        S: Clone + Eq,
        V: Clone + Eq,
    {
        self.synchronize_with_policy(scope, complete, view, visible, true)
    }

    fn synchronize_with_policy<I, J>(
        &mut self,
        scope: S,
        complete: I,
        view: V,
        visible: J,
        retain_hidden: bool,
    ) -> Result<(), ScopedSelectionError>
    where
        I: IntoIterator<Item = OccurrenceDescriptor<M, O>>,
        J: IntoIterator<Item = OccurrenceDescriptor<M, O>>,
        S: Clone + Eq,
        V: Clone + Eq,
    {
        let complete_signature = complete.into_iter().collect::<Vec<_>>();
        let visible_signature = visible.into_iter().collect::<Vec<_>>();

        let exact_repeat = self.scope.as_ref() == Some(&scope)
            && self.view.as_ref() == Some(&view)
            && self.complete_signature == complete_signature
            && self.visible_signature == visible_signature
            && !self.cursor_only;
        if exact_repeat {
            return Ok(());
        }

        let complete_projection =
            ValidatedOccurrenceProjection::try_new(complete_signature.clone());
        let (complete_projection, status, visible_keys, visible_full_indices) =
            match complete_projection {
                Ok(projection) => {
                    let visible_projection =
                        ValidatedOccurrenceProjection::try_new(visible_signature.clone())
                            .map_err(map_visible_projection_error)?;
                    let source_indices_by_key = projection
                        .keys()
                        .iter()
                        .cloned()
                        .enumerate()
                        .map(|(index, key)| (key, index))
                        .collect::<BTreeMap<_, _>>();
                    let mut source_indices = Vec::with_capacity(visible_projection.len());
                    for (visible_index, key) in visible_projection.keys().iter().enumerate() {
                        let Some(source_index) = source_indices_by_key.get(key).copied() else {
                            return Err(ScopedSelectionError::VisibleOccurrenceNotInComplete {
                                visible_index,
                            });
                        };
                        source_indices.push(source_index);
                    }
                    let status = if projection.is_empty() {
                        ScopedSelectionStatus::Empty
                    } else {
                        ScopedSelectionStatus::Ready
                    };
                    (
                        Some(projection),
                        status,
                        visible_projection.keys().to_vec(),
                        source_indices,
                    )
                }
                Err(_) => (
                    None,
                    ScopedSelectionStatus::Ambiguous,
                    Vec::new(),
                    Vec::new(),
                ),
            };

        let next_generation = self
            .selection
            .generation()
            .checked_next()
            .ok_or(ScopedSelectionError::GenerationExhausted)?;

        let scope_changed = self.scope.as_ref() != Some(&scope);
        let view_changed = self.view.as_ref() != Some(&view);
        let mut next_selection = self.selection.clone();
        if scope_changed || view_changed || status == ScopedSelectionStatus::Ambiguous {
            next_selection.reset_scope(next_generation)?;
        } else {
            let reconciliation_keys = if retain_hidden {
                complete_projection
                    .as_ref()
                    .map_or(visible_keys.as_slice(), ValidatedOccurrenceProjection::keys)
            } else {
                &visible_keys
            };
            next_selection.reconcile(next_generation, reconciliation_keys)?;
        }

        self.selection = next_selection;
        self.scope = Some(scope);
        self.view = Some(view);
        self.complete_signature = complete_signature;
        self.complete_projection = complete_projection;
        self.visible_signature = visible_signature;
        self.visible_keys = visible_keys;
        self.visible_full_indices = visible_full_indices;
        self.cursor_only_visible_len = 0;
        self.cursor_only = false;
        self.status = status;
        Ok(())
    }

    /// Clear selected keys and the range anchor.
    pub fn clear(&mut self) -> SelectionChange {
        self.selection.clear()
    }

    /// Set the range anchor to a visible row.
    pub fn set_anchor(&mut self, cursor: usize) -> Result<SelectionChange, ScopedSelectionError> {
        self.require_keyed_projection()?;
        self.ensure_visible_index(cursor)?;
        self.selection
            .set_anchor_at(&self.visible_keys, Some(cursor))
            .map_err(Into::into)
    }

    /// Extend the current range using visible row order.
    pub fn extend_range(
        &mut self,
        cursor: usize,
        target: usize,
    ) -> Result<SelectionChange, ScopedSelectionError> {
        self.require_keyed_projection()?;
        self.ensure_visible_index(cursor)?;
        self.ensure_visible_index(target)?;
        self.selection
            .extend_range(&self.visible_keys, Some(cursor), Some(target))
            .map_err(Into::into)
    }

    /// Select every visible occurrence, dropping hidden selections.
    pub fn select_all_visible(&mut self) -> Result<SelectionChange, ScopedSelectionError> {
        self.require_keyed_projection()?;
        self.selection
            .select_all(&self.visible_keys)
            .map_err(Into::into)
    }

    /// Invert selection within the visible projection only.
    pub fn invert_visible(&mut self) -> Result<SelectionChange, ScopedSelectionError> {
        self.require_keyed_projection()?;
        self.selection
            .invert(&self.visible_keys)
            .map_err(Into::into)
    }

    /// Return selected visible row indices in visible order.
    pub fn selected_visible_indices(&self) -> Vec<usize> {
        self.visible_keys
            .iter()
            .enumerate()
            .filter_map(|(index, key)| self.selection.contains(key).then_some(index))
            .collect()
    }

    /// Return selected visible indices, or the supplied cursor as fallback.
    ///
    /// Ambiguous projections intentionally return the cursor because keyed
    /// selection is unavailable. The cursor is always bounds-checked.
    pub fn selected_or_cursor_visible_indices(
        &self,
        cursor: usize,
    ) -> Result<Vec<usize>, ScopedSelectionError> {
        if self.status == ScopedSelectionStatus::Unscoped {
            return Err(ScopedSelectionError::Unscoped);
        }
        self.ensure_visible_index(cursor)?;
        let selected = self.selected_visible_indices();
        if selected.is_empty() {
            Ok(vec![cursor])
        } else {
            Ok(selected)
        }
    }

    /// Return the source index in the complete projection for a visible row.
    pub fn visible_to_full_index(
        &self,
        visible_index: usize,
    ) -> Result<usize, ScopedSelectionError> {
        self.ensure_visible_index(visible_index)?;
        if self.complete_projection.is_none() {
            return Err(ScopedSelectionError::NoValidProjection);
        }
        self.visible_full_indices
            .get(visible_index)
            .copied()
            .ok_or(ScopedSelectionError::NoValidProjection)
    }

    /// Return the current anchor's visible index, if the anchor is retained.
    ///
    /// This is useful to render a range marker without exposing the opaque
    /// occurrence key itself.
    pub fn anchor_visible_index(&self) -> Option<usize> {
        let anchor = self.selection.anchor()?;
        self.visible_keys.iter().position(|key| key == anchor)
    }

    /// Return the number of currently selected occurrence keys.
    pub fn selected_len(&self) -> usize {
        self.selection.len()
    }

    fn require_keyed_projection(&self) -> Result<(), ScopedSelectionError> {
        match self.status {
            ScopedSelectionStatus::Unscoped => Err(ScopedSelectionError::Unscoped),
            ScopedSelectionStatus::Ambiguous => Err(ScopedSelectionError::AmbiguousProjection),
            ScopedSelectionStatus::Empty | ScopedSelectionStatus::Ready => Ok(()),
        }
    }

    fn ensure_visible_index(&self, index: usize) -> Result<(), ScopedSelectionError> {
        if index < self.visible_len() {
            Ok(())
        } else {
            Err(ScopedSelectionError::OutOfBounds {
                index,
                len: self.visible_len(),
            })
        }
    }
}

impl<S, V, M: Clone + Ord, O: Clone + Ord> super::MultiSelectModel for ScopedSelection<S, V, M, O> {
    type Error = ScopedSelectionError;

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
        Self::extend_range(self, cursor, target)
    }

    fn clear_selection(&mut self) -> SelectionChange {
        Self::clear(self)
    }

    fn selected_visible_indices(&self) -> Vec<usize> {
        Self::selected_visible_indices(self)
    }
}

fn map_visible_projection_error(error: OccurrenceProjectionError) -> ScopedSelectionError {
    let (conflict, first_index, second_index) = match error {
        OccurrenceProjectionError::DuplicateUnique {
            first_index,
            duplicate_index,
        } => (
            ScopedProjectionConflict::DuplicateUnique,
            first_index,
            duplicate_index,
        ),
        OccurrenceProjectionError::MixedUniqueAndTokenized {
            unique_index,
            token_index,
        } => (
            ScopedProjectionConflict::MixedUniqueAndTokenized,
            unique_index,
            token_index,
        ),
        OccurrenceProjectionError::DuplicateToken {
            first_index,
            duplicate_index,
        } => (
            ScopedProjectionConflict::DuplicateToken,
            first_index,
            duplicate_index,
        ),
    };
    ScopedSelectionError::InvalidVisibleProjection {
        conflict,
        first_index,
        second_index,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn unique(rows: &[&'static str]) -> Vec<OccurrenceDescriptor<&'static str>> {
        rows.iter()
            .copied()
            .map(OccurrenceDescriptor::unique)
            .collect()
    }

    fn tokenized(rows: &[(&'static str, u8)]) -> Vec<OccurrenceDescriptor<&'static str, u8>> {
        rows.iter()
            .copied()
            .map(|(identity, token)| OccurrenceDescriptor::with_token(identity, token))
            .collect()
    }

    #[test]
    fn new_scope_clears_selection_and_anchor() {
        let mut adapter = ScopedSelection::<_, _, &'static str>::default();
        adapter
            .synchronize(
                "library",
                unique(&["a", "b", "c"]),
                "all",
                unique(&["a", "b", "c"]),
            )
            .unwrap();
        adapter.set_anchor(1).unwrap();
        adapter.extend_range(1, 2).unwrap();
        assert_eq!(adapter.selected_visible_indices(), vec![1, 2]);

        adapter
            .synchronize("playlist", unique(&["a", "b"]), "all", unique(&["a", "b"]))
            .unwrap();
        assert_eq!(adapter.selected_visible_indices(), Vec::<usize>::new());
        assert_eq!(adapter.anchor_visible_index(), None);
    }

    #[test]
    fn view_changes_open_change_and_close_reset_selection() {
        let mut adapter = ScopedSelection::<_, _, &'static str>::default();
        adapter
            .synchronize(
                "library",
                unique(&["a", "b"]),
                "closed",
                unique(&["a", "b"]),
            )
            .unwrap();
        adapter.extend_range(0, 1).unwrap();
        for view in ["open", "changed", "closed"] {
            adapter
                .synchronize("library", unique(&["a", "b"]), view, unique(&["a", "b"]))
                .unwrap();
            assert!(adapter.selected_visible_indices().is_empty());
            assert_eq!(adapter.anchor_visible_index(), None);
            adapter.extend_range(0, 1).unwrap();
        }
    }

    #[test]
    fn exact_repeat_is_a_no_op() {
        let mut adapter = ScopedSelection::<_, _, &'static str>::default();
        let complete = unique(&["a", "b"]);
        let visible = unique(&["a", "b"]);
        adapter
            .synchronize("library", complete.clone(), "all", visible.clone())
            .unwrap();
        adapter.extend_range(0, 1).unwrap();
        let generation = adapter.generation();
        adapter
            .synchronize("library", complete, "all", visible)
            .unwrap();
        assert_eq!(adapter.generation(), generation);
        assert_eq!(adapter.selected_visible_indices(), vec![0, 1]);
    }

    #[test]
    fn visible_reorder_controls_range_marker_and_action_order() {
        let mut adapter = ScopedSelection::<_, _, &'static str>::default();
        adapter
            .synchronize(
                "library",
                unique(&["a", "b", "c"]),
                "fzf",
                unique(&["c", "a", "b"]),
            )
            .unwrap();
        adapter.extend_range(0, 1).unwrap();
        assert_eq!(adapter.selected_visible_indices(), vec![0, 1]);
        assert_eq!(adapter.anchor_visible_index(), Some(0));
        assert_eq!(
            adapter.selected_or_cursor_visible_indices(2).unwrap(),
            vec![0, 1]
        );
        assert_eq!(adapter.visible_to_full_index(0).unwrap(), 2);
        assert_eq!(adapter.visible_to_full_index(1).unwrap(), 0);
    }

    #[test]
    fn full_reorder_preserves_keys_and_updates_source_mapping() {
        let mut adapter = ScopedSelection::<_, _, &'static str>::default();
        adapter
            .synchronize(
                "library",
                unique(&["a", "b", "c"]),
                "all",
                unique(&["a", "b"]),
            )
            .unwrap();
        adapter.extend_range(0, 1).unwrap();
        adapter
            .synchronize(
                "library",
                unique(&["c", "b", "a"]),
                "all",
                unique(&["b", "a"]),
            )
            .unwrap();
        assert_eq!(adapter.selected_visible_indices(), vec![0, 1]);
        assert_eq!(adapter.visible_to_full_index(0).unwrap(), 1);
        assert_eq!(adapter.visible_to_full_index(1).unwrap(), 2);
    }

    #[test]
    fn refresh_drops_hidden_and_missing_selected_keys_and_anchor() {
        let mut adapter = ScopedSelection::<_, _, &'static str>::default();
        adapter
            .synchronize(
                "library",
                unique(&["a", "b", "c"]),
                "filtered",
                unique(&["a", "b", "c"]),
            )
            .unwrap();
        adapter.set_anchor(1).unwrap();
        adapter.extend_range(1, 2).unwrap();

        adapter
            .synchronize(
                "library",
                unique(&["a", "c"]),
                "filtered",
                unique(&["a", "c"]),
            )
            .unwrap();
        assert_eq!(adapter.selected_visible_indices(), vec![1]);
        assert_eq!(adapter.anchor_visible_index(), None);

        adapter
            .synchronize("library", unique(&["a", "c"]), "filtered", unique(&["a"]))
            .unwrap();
        assert!(adapter.selected_visible_indices().is_empty());
    }

    #[test]
    fn complete_ambiguity_disables_keyed_selection_but_cursor_fallback_survives() {
        let mut adapter = ScopedSelection::<_, _, &'static str>::default();
        adapter
            .synchronize("library", unique(&["a", "b"]), "all", unique(&["a", "b"]))
            .unwrap();
        adapter.extend_range(0, 1).unwrap();
        adapter
            .synchronize(
                "library",
                unique(&["a", "a", "b"]),
                "all",
                unique(&["a", "b"]),
            )
            .unwrap();
        assert_eq!(adapter.status(), ScopedSelectionStatus::Ambiguous);
        assert!(!adapter.keyed_selection_available());
        assert_eq!(adapter.full_len(), 3);
        assert_eq!(adapter.visible_len(), 2);
        assert_eq!(adapter.selected_len(), 0);
        assert_eq!(adapter.anchor_visible_index(), None);
        assert_eq!(
            adapter.selected_or_cursor_visible_indices(1).unwrap(),
            vec![1]
        );
        assert_eq!(
            adapter.extend_range(0, 1),
            Err(ScopedSelectionError::AmbiguousProjection)
        );
        assert_eq!(
            adapter.visible_to_full_index(0),
            Err(ScopedSelectionError::NoValidProjection)
        );
    }

    #[test]
    fn tokenized_duplicate_occurrences_remain_distinct() {
        let mut adapter = ScopedSelection::<_, _, &'static str, u8>::default();
        adapter
            .synchronize(
                "queue",
                tokenized(&[("same", 1), ("same", 2)]),
                "all",
                tokenized(&[("same", 2), ("same", 1)]),
            )
            .unwrap();
        adapter.extend_range(0, 1).unwrap();
        assert_eq!(adapter.selected_visible_indices(), vec![0, 1]);
        assert_eq!(adapter.visible_to_full_index(0).unwrap(), 1);
    }

    #[test]
    fn invalid_visible_projection_is_transactional() {
        let mut adapter = ScopedSelection::<_, _, &'static str>::default();
        adapter
            .synchronize("library", unique(&["a", "b"]), "all", unique(&["a", "b"]))
            .unwrap();
        adapter.extend_range(0, 1).unwrap();
        let before = adapter.clone();

        let error = adapter.synchronize("library", unique(&["a", "b"]), "all", unique(&["a", "a"]));
        assert_eq!(
            error,
            Err(ScopedSelectionError::InvalidVisibleProjection {
                conflict: ScopedProjectionConflict::DuplicateUnique,
                first_index: 0,
                second_index: 1,
            })
        );
        assert_eq!(adapter, before);
    }

    #[test]
    fn visible_mixed_and_outside_full_errors_are_transactional() {
        let mut adapter = ScopedSelection::<_, _, &'static str, u8>::default();
        adapter
            .synchronize(
                "library",
                tokenized(&[("a", 1), ("b", 1)]),
                "all",
                tokenized(&[("a", 1), ("b", 1)]),
            )
            .unwrap();
        let before = adapter.clone();

        assert_eq!(
            adapter.synchronize(
                "library",
                vec![
                    OccurrenceDescriptor::unique("a"),
                    OccurrenceDescriptor::with_token("b", 1),
                ],
                "all",
                vec![
                    OccurrenceDescriptor::unique("a"),
                    OccurrenceDescriptor::with_token("a", 1),
                ],
            ),
            Err(ScopedSelectionError::InvalidVisibleProjection {
                conflict: ScopedProjectionConflict::MixedUniqueAndTokenized,
                first_index: 0,
                second_index: 1,
            })
        );
        assert_eq!(adapter, before);

        assert_eq!(
            adapter.synchronize(
                "library",
                tokenized(&[("a", 1), ("b", 1)]),
                "all",
                tokenized(&[("missing", 1)]),
            ),
            Err(ScopedSelectionError::VisibleOccurrenceNotInComplete { visible_index: 0 })
        );
        assert_eq!(adapter, before);
    }

    #[test]
    fn zero_one_many_rows_and_bounds_are_observable() {
        let mut adapter = ScopedSelection::<_, _, &'static str>::default();
        adapter
            .synchronize("empty", unique(&[]), "all", unique(&[]))
            .unwrap();
        assert_eq!(adapter.status(), ScopedSelectionStatus::Empty);
        assert_eq!(adapter.full_len(), 0);
        assert_eq!(adapter.visible_len(), 0);
        assert_eq!(
            adapter.selected_or_cursor_visible_indices(0),
            Err(ScopedSelectionError::OutOfBounds { index: 0, len: 0 })
        );

        adapter
            .synchronize("one", unique(&["a"]), "all", unique(&["a"]))
            .unwrap();
        assert_eq!(
            adapter.extend_range(0, 0).unwrap(),
            SelectionChange::Changed
        );
        assert_eq!(adapter.selected_visible_indices(), vec![0]);
        assert_eq!(
            adapter.extend_range(1, 1),
            Err(ScopedSelectionError::OutOfBounds { index: 1, len: 1 })
        );
    }

    #[test]
    fn select_all_and_invert_are_visible_only() {
        let mut adapter = ScopedSelection::<_, _, &'static str>::default();
        adapter
            .synchronize(
                "library",
                unique(&["a", "b", "c"]),
                "filtered",
                unique(&["a", "c"]),
            )
            .unwrap();
        adapter.select_all_visible().unwrap();
        assert_eq!(adapter.selected_visible_indices(), vec![0, 1]);
        adapter.invert_visible().unwrap();
        assert!(adapter.selected_visible_indices().is_empty());

        adapter
            .synchronize(
                "library",
                unique(&["a", "b", "c"]),
                "filtered",
                unique(&["a", "b", "c"]),
            )
            .unwrap();
        assert!(adapter.selected_visible_indices().is_empty());
    }

    #[test]
    fn generation_exhaustion_is_transactional() {
        let mut adapter =
            ScopedSelection::<_, _, &'static str>::new(DatasetGeneration::new(u64::MAX));
        let before = adapter.clone();
        assert_eq!(
            adapter.synchronize("library", unique(&["a"]), "all", unique(&["a"])),
            Err(ScopedSelectionError::GenerationExhausted)
        );
        assert_eq!(adapter, before);
    }

    #[test]
    fn selected_state_never_exceeds_visible_projection() {
        let mut adapter = ScopedSelection::<_, _, &'static str>::default();
        adapter
            .synchronize(
                "library",
                unique(&["a", "b", "c"]),
                "all",
                unique(&["a", "b", "c"]),
            )
            .unwrap();
        adapter.select_all_visible().unwrap();
        adapter
            .synchronize(
                "library",
                unique(&["a", "b", "c"]),
                "filtered",
                unique(&["a", "c"]),
            )
            .unwrap();
        assert_eq!(adapter.selected_len(), 0);
        adapter.select_all_visible().unwrap();
        assert_eq!(adapter.selected_len(), adapter.visible_len());
    }
}
