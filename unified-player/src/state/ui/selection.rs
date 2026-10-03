use std::collections::{BTreeMap, BTreeSet};

/// A monotonically increasing generation for one selection dataset.
///
/// A generation is intentionally separate from the row keys. Callers choose
/// when a dataset has advanced and use [`SelectionState::reset_scope`] when
/// the dataset is a new scope rather than a refresh of the same scope.
#[derive(Clone, Copy, Debug, Default, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct DatasetGeneration(u64);

impl DatasetGeneration {
    /// The initial generation used by [`SelectionState::default`].
    pub const INITIAL: Self = Self(0);

    /// Construct a generation from a caller-owned monotonic counter.
    pub const fn new(value: u64) -> Self {
        Self(value)
    }

    /// Return the numeric generation value.
    pub const fn value(self) -> u64 {
        self.0
    }

    /// Return the next generation, or `None` if the counter is exhausted.
    pub const fn checked_next(self) -> Option<Self> {
        match self.0.checked_add(1) {
            Some(value) => Some(Self(value)),
            None => None,
        }
    }
}

/// A bounded reason why a selection operation could not be applied.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SelectionError {
    /// A row projection contained the same key more than once.
    DuplicateKey {
        first_index: usize,
        duplicate_index: usize,
    },
    /// The cursor was not supplied or did not identify a row in the
    /// projection.
    MissingCursor,
    /// The range target was not supplied or did not identify a row in the
    /// projection.
    MissingTarget,
    /// The stored anchor is not present in the supplied projection.
    MissingAnchor,
    /// The update belongs to an older (or already applied) generation.
    StaleGeneration {
        current: DatasetGeneration,
        received: DatasetGeneration,
    },
}

/// Describes whether an operation changed the selection state.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SelectionChange {
    Changed,
    Unchanged,
}

/// The bounded result of reconciling selected keys with a refreshed
/// projection.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ReconcileOutcome {
    pub generation: DatasetGeneration,
    pub retained_keys: usize,
    pub dropped_keys: usize,
    pub anchor_retained: bool,
}

/// Provider-neutral selection state over caller-owned opaque row keys.
///
/// The state deliberately does not contain a cursor, row payload, provider
/// identity, or media identity. A caller supplies an ordered projection for
/// operations that depend on row order. The projection must contain unique
/// occurrence keys; two occurrences of the same media remain distinct when
/// their keys differ.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SelectionState<K> {
    generation: DatasetGeneration,
    anchor: Option<K>,
    selected_keys: BTreeSet<K>,
}

impl<K: Ord + Clone> Default for SelectionState<K> {
    fn default() -> Self {
        Self::new(DatasetGeneration::INITIAL)
    }
}

impl<K: Ord + Clone> SelectionState<K> {
    /// Create empty selection state at `generation`.
    pub fn new(generation: DatasetGeneration) -> Self {
        Self {
            generation,
            anchor: None,
            selected_keys: BTreeSet::new(),
        }
    }

    /// Return the currently accepted dataset generation.
    pub const fn generation(&self) -> DatasetGeneration {
        self.generation
    }

    /// Return the selected occurrence keys in deterministic key order.
    pub fn selected_keys(&self) -> &BTreeSet<K> {
        &self.selected_keys
    }

    /// Return the current range anchor, if one is set.
    pub fn anchor(&self) -> Option<&K> {
        self.anchor.as_ref()
    }

    /// Return whether `key` is currently selected.
    pub fn contains(&self, key: &K) -> bool {
        self.selected_keys.contains(key)
    }

    /// Return the number of selected occurrence keys.
    pub fn len(&self) -> usize {
        self.selected_keys.len()
    }

    /// Return whether no occurrence keys are selected.
    pub fn is_empty(&self) -> bool {
        self.selected_keys.is_empty()
    }

    /// Set or clear the range anchor without changing selected keys.
    pub fn set_anchor(&mut self, anchor: Option<K>) -> SelectionChange {
        if self.anchor == anchor {
            return SelectionChange::Unchanged;
        }
        self.anchor = anchor;
        SelectionChange::Changed
    }

    /// Clear selected keys and the range anchor.
    pub fn clear(&mut self) -> SelectionChange {
        if self.selected_keys.is_empty() && self.anchor.is_none() {
            return SelectionChange::Unchanged;
        }
        self.selected_keys.clear();
        self.anchor = None;
        SelectionChange::Changed
    }

    /// Set the anchor to a cursor position in an ordered projection.
    pub fn set_anchor_at(
        &mut self,
        projection: &[K],
        cursor: Option<usize>,
    ) -> Result<SelectionChange, SelectionError> {
        validate_projection(projection)?;
        let cursor = cursor.ok_or(SelectionError::MissingCursor)?;
        let key = projection
            .get(cursor)
            .ok_or(SelectionError::MissingCursor)?
            .clone();
        Ok(self.set_anchor(Some(key)))
    }

    /// Replace the selected set with the inclusive range from the stored
    /// anchor to `target`.
    ///
    /// When no anchor exists yet, the supplied cursor becomes the anchor. This
    /// mirrors the existing first range-extension gesture while keeping the
    /// cursor outside this type. A caller can use [`Self::set_anchor`] first
    /// when a missing anchor should be reported instead.
    pub fn extend_range(
        &mut self,
        projection: &[K],
        cursor: Option<usize>,
        target: Option<usize>,
    ) -> Result<SelectionChange, SelectionError> {
        validate_projection(projection)?;
        let cursor = cursor.ok_or(SelectionError::MissingCursor)?;
        let target = target.ok_or(SelectionError::MissingTarget)?;
        let cursor_key = projection
            .get(cursor)
            .ok_or(SelectionError::MissingCursor)?;
        projection
            .get(target)
            .ok_or(SelectionError::MissingTarget)?;

        let anchor_key = self.anchor.as_ref().unwrap_or(cursor_key);
        let anchor_index = projection
            .iter()
            .position(|key| key == anchor_key)
            .ok_or(SelectionError::MissingAnchor)?;
        let start = anchor_index.min(target);
        let end = anchor_index.max(target);

        let next_selected = projection[start..=end]
            .iter()
            .cloned()
            .collect::<BTreeSet<_>>();
        let next_anchor = if self.anchor.is_some() {
            self.anchor.clone()
        } else {
            Some(cursor_key.clone())
        };

        let changed = self.selected_keys != next_selected || self.anchor != next_anchor;
        self.selected_keys = next_selected;
        self.anchor = next_anchor;

        Ok(if changed {
            SelectionChange::Changed
        } else {
            SelectionChange::Unchanged
        })
    }

    /// Select every key in an ordered projection.
    ///
    /// This only changes selected keys. The anchor is independent and remains
    /// unchanged even when it is outside this operation's projection.
    pub fn select_all(&mut self, projection: &[K]) -> Result<SelectionChange, SelectionError> {
        validate_projection(projection)?;
        let next_selected = projection.iter().cloned().collect::<BTreeSet<_>>();
        let changed = self.selected_keys != next_selected;
        self.selected_keys = next_selected;
        Ok(if changed {
            SelectionChange::Changed
        } else {
            SelectionChange::Unchanged
        })
    }

    /// Replace the selected set with the complement within an ordered
    /// projection. The independent anchor is left unchanged.
    pub fn invert(&mut self, projection: &[K]) -> Result<SelectionChange, SelectionError> {
        validate_projection(projection)?;
        let next_selected = projection
            .iter()
            .filter(|key| !self.selected_keys.contains(*key))
            .cloned()
            .collect::<BTreeSet<_>>();
        let changed = self.selected_keys != next_selected;
        self.selected_keys = next_selected;
        Ok(if changed {
            SelectionChange::Changed
        } else {
            SelectionChange::Unchanged
        })
    }

    /// Reset selection for a new scope and accept a strictly newer
    /// generation.
    pub fn reset_scope(
        &mut self,
        generation: DatasetGeneration,
    ) -> Result<SelectionChange, SelectionError> {
        self.ensure_new_generation(generation)?;
        self.generation = generation;
        self.selected_keys.clear();
        self.anchor = None;
        Ok(SelectionChange::Changed)
    }

    /// Reconcile a refreshed projection in the same scope.
    ///
    /// Matching keys survive reorder and refresh. Missing selected keys and a
    /// missing anchor are dropped. A stale generation is rejected before any
    /// state is changed.
    pub fn reconcile(
        &mut self,
        generation: DatasetGeneration,
        projection: &[K],
    ) -> Result<ReconcileOutcome, SelectionError> {
        self.ensure_new_generation(generation)?;
        validate_projection(projection)?;

        let projected = projection.iter().cloned().collect::<BTreeSet<_>>();
        let before = self.selected_keys.len();
        self.selected_keys.retain(|key| projected.contains(key));
        let retained_keys = self.selected_keys.len();
        let dropped_keys = before.saturating_sub(retained_keys);
        let anchor_retained = self
            .anchor
            .as_ref()
            .is_some_and(|anchor| projected.contains(anchor));
        if !anchor_retained {
            self.anchor = None;
        }
        self.generation = generation;

        Ok(ReconcileOutcome {
            generation,
            retained_keys,
            dropped_keys,
            anchor_retained,
        })
    }

    fn ensure_new_generation(&self, received: DatasetGeneration) -> Result<(), SelectionError> {
        if received <= self.generation {
            return Err(SelectionError::StaleGeneration {
                current: self.generation,
                received,
            });
        }
        Ok(())
    }
}

/// An occurrence descriptor supplied by a caller that owns row identity.
///
/// A descriptor is either an asserted-unique semantic identity or that
/// identity plus a caller-supplied stable occurrence token. Descriptors become
/// [`OccurrenceKey`] values only after a complete projection passes validation.
/// This type intentionally has no metadata field: labels and other row payload
/// cannot participate in reconciliation identity.
#[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct OccurrenceDescriptor<M, O = ()> {
    semantic_identity: M,
    occurrence_token: Option<O>,
}

impl<M, O> OccurrenceDescriptor<M, O> {
    /// Construct a descriptor for a row whose semantic identity is asserted
    /// unique within the full projection.
    pub fn unique(semantic_identity: M) -> Self {
        Self {
            semantic_identity,
            occurrence_token: None,
        }
    }

    /// Construct a descriptor for a distinguishable occurrence using a real,
    /// caller-supplied stable token.
    pub fn with_token(semantic_identity: M, occurrence_token: O) -> Self {
        Self {
            semantic_identity,
            occurrence_token: Some(occurrence_token),
        }
    }

    /// Return the semantic media identity carried by this descriptor.
    pub fn semantic_identity(&self) -> &M {
        &self.semantic_identity
    }

    /// Return the stable occurrence token, if this descriptor was tokenized.
    pub fn occurrence_token(&self) -> Option<&O> {
        self.occurrence_token.as_ref()
    }

    /// Return whether this descriptor relies on an asserted-unique identity.
    pub fn is_asserted_unique(&self) -> bool {
        self.occurrence_token.is_none()
    }
}

/// An opaque occurrence key produced only by a validated projection.
///
/// The private conversion boundary prevents an unvalidated descriptor from
/// being passed directly to [`SelectionState`]. Keys contain only semantic
/// identity and an optional stable occurrence token, and therefore derive the
/// ordering and cloning traits required by the selection kernel.
#[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct OccurrenceKey<M, O = ()> {
    semantic_identity: M,
    occurrence_token: Option<O>,
}

impl<M, O> OccurrenceKey<M, O> {
    fn from_descriptor(descriptor: OccurrenceDescriptor<M, O>) -> Self {
        Self {
            semantic_identity: descriptor.semantic_identity,
            occurrence_token: descriptor.occurrence_token,
        }
    }

    /// Return the semantic media identity carried by this key.
    pub fn semantic_identity(&self) -> &M {
        &self.semantic_identity
    }

    /// Return the stable occurrence token, if this row was tokenized.
    pub fn occurrence_token(&self) -> Option<&O> {
        self.occurrence_token.as_ref()
    }

    /// Return whether this key relies on an asserted-unique semantic identity.
    pub fn is_asserted_unique(&self) -> bool {
        self.occurrence_token.is_none()
    }
}

/// A bounded validation failure while constructing an occurrence projection.
///
/// Indices refer to the caller's input order. The validator stops at the
/// first observable conflict and never returns a partially validated wrapper.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum OccurrenceProjectionError {
    /// The same semantic identity was asserted unique more than once.
    DuplicateUnique {
        first_index: usize,
        duplicate_index: usize,
    },
    /// A semantic identity was used once as unique and once with a token.
    MixedUniqueAndTokenized {
        unique_index: usize,
        token_index: usize,
    },
    /// The same stable token was supplied twice for one semantic identity.
    DuplicateToken {
        first_index: usize,
        duplicate_index: usize,
    },
}

/// An immutable, ordered projection whose occurrence keys have been validated.
///
/// The wrapper preserves caller order and exposes only read-only views, so a
/// projection cannot become invalid after construction. Its [`keys`] slice can
/// be passed directly to [`SelectionState`] operations.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ValidatedOccurrenceProjection<M, O = ()> {
    keys: Vec<OccurrenceKey<M, O>>,
}

struct SemanticOccurrenceGroup<O> {
    unique_index: Option<usize>,
    first_token_index: Option<usize>,
    token_indices: BTreeMap<O, usize>,
}

impl<M: Clone + Ord, O: Clone + Ord> ValidatedOccurrenceProjection<M, O> {
    /// Validate descriptors and preserve their caller-supplied order as keys.
    pub fn try_new<I>(keys: I) -> Result<Self, OccurrenceProjectionError>
    where
        I: IntoIterator<Item = OccurrenceDescriptor<M, O>>,
    {
        let descriptors = keys.into_iter().collect::<Vec<_>>();
        let mut groups = BTreeMap::<M, SemanticOccurrenceGroup<O>>::new();

        for (index, descriptor) in descriptors.iter().enumerate() {
            let group = groups
                .entry(descriptor.semantic_identity.clone())
                .or_insert_with(|| SemanticOccurrenceGroup {
                    unique_index: None,
                    first_token_index: None,
                    token_indices: BTreeMap::new(),
                });

            match descriptor.occurrence_token.as_ref() {
                None => {
                    if let Some(token_index) = group.first_token_index {
                        return Err(OccurrenceProjectionError::MixedUniqueAndTokenized {
                            unique_index: index,
                            token_index,
                        });
                    }
                    if let Some(first_index) = group.unique_index {
                        return Err(OccurrenceProjectionError::DuplicateUnique {
                            first_index,
                            duplicate_index: index,
                        });
                    }
                    group.unique_index = Some(index);
                }
                Some(token) => {
                    if let Some(unique_index) = group.unique_index {
                        return Err(OccurrenceProjectionError::MixedUniqueAndTokenized {
                            unique_index,
                            token_index: index,
                        });
                    }
                    if let Some(first_index) = group.token_indices.get(token).copied() {
                        return Err(OccurrenceProjectionError::DuplicateToken {
                            first_index,
                            duplicate_index: index,
                        });
                    }
                    if group.first_token_index.is_none() {
                        group.first_token_index = Some(index);
                    }
                    group.token_indices.insert(token.clone(), index);
                }
            }
        }

        let keys = descriptors
            .into_iter()
            .map(OccurrenceKey::from_descriptor)
            .collect();
        Ok(Self { keys })
    }

    /// Borrow the validated keys in the exact order supplied by the caller.
    pub fn keys(&self) -> &[OccurrenceKey<M, O>] {
        &self.keys
    }

    /// Return the number of validated rows.
    pub fn len(&self) -> usize {
        self.keys.len()
    }

    /// Return whether the projection contains no rows.
    pub fn is_empty(&self) -> bool {
        self.keys.is_empty()
    }
}

fn validate_projection<K: Ord + Clone>(projection: &[K]) -> Result<(), SelectionError> {
    let mut seen = BTreeMap::new();
    for (index, key) in projection.iter().enumerate() {
        if let Some(&first_index) = seen.get(key) {
            return Err(SelectionError::DuplicateKey {
                first_index,
                duplicate_index: index,
            });
        }
        seen.insert(key.clone(), index);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{
        DatasetGeneration, OccurrenceDescriptor, OccurrenceProjectionError, SelectionChange,
        SelectionError, SelectionState, ValidatedOccurrenceProjection,
    };

    fn rows(values: &[u8]) -> Vec<u8> {
        values.to_vec()
    }

    fn state() -> SelectionState<u8> {
        SelectionState::new(DatasetGeneration::new(1))
    }

    #[test]
    fn distinct_occurrence_keys_for_duplicate_media_remain_distinct() {
        let projection = rows(&[10, 11]);
        let mut selection = state();
        selection
            .set_anchor_at(&projection, Some(0))
            .expect("cursor exists");
        selection
            .extend_range(&projection, Some(0), Some(1))
            .expect("range exists");

        assert_eq!(
            selection
                .selected_keys()
                .iter()
                .copied()
                .collect::<Vec<_>>(),
            [10, 11]
        );
        assert!(selection.contains(&10));
        assert!(selection.contains(&11));
    }

    #[test]
    fn duplicate_identical_keys_are_rejected_without_mutation() {
        let projection = rows(&[1, 2]);
        let mut selection = state();
        selection
            .set_anchor_at(&projection, Some(0))
            .expect("cursor exists");
        selection
            .extend_range(&projection, Some(0), Some(1))
            .expect("range exists");
        let before = selection.clone();

        let error = selection.select_all(&rows(&[1, 1])).unwrap_err();
        assert_eq!(
            error,
            SelectionError::DuplicateKey {
                first_index: 0,
                duplicate_index: 1,
            }
        );
        assert_eq!(selection, before);
    }

    #[test]
    fn forward_and_backward_ranges_reuse_the_anchor() {
        let projection = rows(&[1, 2, 3, 4, 5]);
        let mut selection = state();

        selection
            .extend_range(&projection, Some(1), Some(3))
            .expect("forward range exists");
        assert_eq!(selection.anchor(), Some(&2));
        assert_eq!(
            selection
                .selected_keys()
                .iter()
                .copied()
                .collect::<Vec<_>>(),
            [2, 3, 4]
        );

        selection
            .extend_range(&projection, Some(4), Some(0))
            .expect("backward range exists");
        assert_eq!(selection.anchor(), Some(&2));
        assert_eq!(
            selection
                .selected_keys()
                .iter()
                .copied()
                .collect::<Vec<_>>(),
            [1, 2]
        );
    }

    #[test]
    fn missing_inputs_are_bounded_errors() {
        let projection = rows(&[1, 2, 3]);
        let mut selection = state();

        assert_eq!(
            selection.extend_range(&projection, None, Some(1)),
            Err(SelectionError::MissingCursor)
        );
        assert_eq!(
            selection.extend_range(&projection, Some(0), None),
            Err(SelectionError::MissingTarget)
        );
        selection.set_anchor(Some(99));
        assert_eq!(
            selection.extend_range(&projection, Some(0), Some(1)),
            Err(SelectionError::MissingAnchor)
        );
        assert_eq!(selection.selected_keys().len(), 0);
    }

    #[test]
    fn zero_one_many_rows_and_clear_are_safe() {
        let mut selection = state();
        assert_eq!(selection.select_all(&[]), Ok(SelectionChange::Unchanged));
        assert_eq!(
            selection.extend_range(&[], Some(0), Some(0)),
            Err(SelectionError::MissingCursor)
        );

        selection.select_all(&rows(&[7])).expect("one row is valid");
        assert_eq!(
            selection
                .selected_keys()
                .iter()
                .copied()
                .collect::<Vec<_>>(),
            [7]
        );
        assert_eq!(selection.clear(), SelectionChange::Changed);
        assert!(selection.is_empty());
        assert_eq!(selection.anchor(), None);

        selection
            .select_all(&rows(&[1, 2, 3]))
            .expect("many rows are valid");
        assert_eq!(selection.len(), 3);
    }

    #[test]
    fn select_all_invert_and_clear_stay_within_projection() {
        let projection = rows(&[1, 2, 3, 4]);
        let mut selection = state();
        selection
            .select_all(&projection)
            .expect("projection is unique");
        assert_eq!(selection.len(), 4);

        selection
            .invert(&rows(&[2, 3, 4, 5]))
            .expect("projection is unique");
        assert_eq!(
            selection
                .selected_keys()
                .iter()
                .copied()
                .collect::<Vec<_>>(),
            [5]
        );

        selection.clear();
        assert!(selection.selected_keys().is_empty());
    }

    #[test]
    fn select_all_and_invert_preserve_an_anchor_outside_the_projection() {
        let mut selection = state();
        selection.set_anchor(Some(99));

        selection
            .select_all(&rows(&[1, 2, 3]))
            .expect("projection is unique");
        assert_eq!(selection.anchor(), Some(&99));

        selection
            .invert(&rows(&[2, 3, 4]))
            .expect("projection is unique");
        assert_eq!(selection.anchor(), Some(&99));
    }

    #[test]
    fn reconcile_preserves_reordered_keys_and_drops_only_missing_keys() {
        let mut selection = state();
        selection
            .select_all(&rows(&[1, 2, 3]))
            .expect("projection is unique");
        selection.set_anchor(Some(1));

        let outcome = selection
            .reconcile(DatasetGeneration::new(2), &rows(&[3, 1, 4]))
            .expect("new generation is accepted");
        assert_eq!(outcome.retained_keys, 2);
        assert_eq!(outcome.dropped_keys, 1);
        assert!(outcome.anchor_retained);
        assert_eq!(
            selection
                .selected_keys()
                .iter()
                .copied()
                .collect::<Vec<_>>(),
            [1, 3]
        );

        let outcome = selection
            .reconcile(DatasetGeneration::new(3), &rows(&[4]))
            .expect("new generation is accepted");
        assert_eq!(outcome.retained_keys, 0);
        assert_eq!(outcome.dropped_keys, 2);
        assert!(!outcome.anchor_retained);
        assert!(selection.is_empty());
        assert_eq!(selection.anchor(), None);
    }

    #[test]
    fn reset_scope_clears_keys_and_anchor() {
        let mut selection = state();
        selection
            .select_all(&rows(&[1, 2]))
            .expect("projection is unique");
        selection.set_anchor(Some(1));

        assert_eq!(
            selection.reset_scope(DatasetGeneration::new(2)),
            Ok(SelectionChange::Changed)
        );
        assert!(selection.is_empty());
        assert_eq!(selection.anchor(), None);
        assert_eq!(selection.generation(), DatasetGeneration::new(2));
    }

    #[test]
    fn stale_generation_leaves_newer_state_unchanged() {
        let mut selection = state();
        selection
            .select_all(&rows(&[1, 2]))
            .expect("projection is unique");
        selection.set_anchor(Some(1));
        selection
            .reconcile(DatasetGeneration::new(3), &rows(&[1, 2, 3]))
            .expect("new generation is accepted");
        let before = selection.clone();

        assert_eq!(
            selection.reconcile(DatasetGeneration::new(2), &rows(&[9])),
            Err(SelectionError::StaleGeneration {
                current: DatasetGeneration::new(3),
                received: DatasetGeneration::new(2),
            })
        );
        assert_eq!(selection, before);
    }

    #[test]
    fn selection_cannot_grow_beyond_reconciled_dataset() {
        let mut selection = state();
        selection
            .select_all(&rows(&[1, 2, 3]))
            .expect("projection is unique");
        selection
            .reconcile(DatasetGeneration::new(2), &rows(&[2]))
            .expect("new generation is accepted");
        assert_eq!(
            selection
                .selected_keys()
                .iter()
                .copied()
                .collect::<Vec<_>>(),
            [2]
        );
        assert_eq!(selection.len(), 1);
    }

    #[test]
    fn generation_checked_next_is_monotonic_and_bounded() {
        assert_eq!(
            DatasetGeneration::new(4).checked_next(),
            Some(DatasetGeneration::new(5))
        );
        assert_eq!(DatasetGeneration::new(u64::MAX).checked_next(), None);
    }

    type Occurrence = OccurrenceDescriptor<&'static str, u8>;
    type Projection = ValidatedOccurrenceProjection<&'static str, u8>;

    fn unique(media: &'static str) -> Occurrence {
        OccurrenceDescriptor::unique(media)
    }

    fn tokenized(media: &'static str, token: u8) -> Occurrence {
        OccurrenceDescriptor::with_token(media, token)
    }

    fn projection(keys: Vec<Occurrence>) -> Projection {
        Projection::try_new(keys).expect("occurrence projection is valid")
    }

    #[test]
    fn asserted_unique_identity_is_accepted_and_exposed() {
        let projection = projection(vec![unique("track-a")]);
        let key = &projection.keys()[0];

        assert_eq!(projection.len(), 1);
        assert!(!projection.is_empty());
        assert_eq!(key.semantic_identity(), &"track-a");
        assert_eq!(key.occurrence_token(), None);
        assert!(key.is_asserted_unique());
    }

    #[test]
    fn distinct_unique_identities_preserve_caller_order() {
        let projection = projection(vec![unique("track-b"), unique("track-a")]);

        assert_eq!(
            projection
                .keys()
                .iter()
                .map(|key| *key.semantic_identity())
                .collect::<Vec<_>>(),
            ["track-b", "track-a"]
        );
    }

    #[test]
    fn tokenized_duplicate_media_remains_distinct_through_reorder() {
        let first = projection(vec![tokenized("track-a", 10), tokenized("track-a", 11)]);
        let reordered = projection(vec![tokenized("track-a", 11), tokenized("track-a", 10)]);

        assert_ne!(first.keys()[0], first.keys()[1]);
        assert_eq!(first.keys()[0], reordered.keys()[1]);
        assert_eq!(first.keys()[1], reordered.keys()[0]);
        assert_eq!(first.keys()[0].occurrence_token(), Some(&10));
        assert_eq!(first.keys()[1].occurrence_token(), Some(&11));
    }

    #[test]
    fn duplicate_unique_identity_is_rejected_transactionally() {
        let error = Projection::try_new(vec![unique("track-a"), unique("track-a")])
            .expect_err("duplicate asserted-unique rows must fail");

        assert_eq!(
            error,
            OccurrenceProjectionError::DuplicateUnique {
                first_index: 0,
                duplicate_index: 1,
            }
        );
    }

    #[test]
    fn unique_and_tokenized_identity_is_rejected() {
        let error = Projection::try_new(vec![unique("track-a"), tokenized("track-a", 7)])
            .expect_err("mixed identity forms must fail");
        assert_eq!(
            error,
            OccurrenceProjectionError::MixedUniqueAndTokenized {
                unique_index: 0,
                token_index: 1,
            }
        );

        let reverse_error = Projection::try_new(vec![
            tokenized("track-a", 7),
            tokenized("track-a", 1),
            unique("track-a"),
        ])
        .expect_err("mixed identity forms must fail in either order");
        assert_eq!(
            reverse_error,
            OccurrenceProjectionError::MixedUniqueAndTokenized {
                unique_index: 2,
                token_index: 0,
            }
        );
    }

    #[test]
    fn duplicate_token_for_one_media_is_rejected() {
        let error = Projection::try_new(vec![tokenized("track-a", 7), tokenized("track-a", 7)])
            .expect_err("duplicate stable token must fail");
        assert_eq!(
            error,
            OccurrenceProjectionError::DuplicateToken {
                first_index: 0,
                duplicate_index: 1,
            }
        );
    }

    #[test]
    fn one_token_value_may_be_reused_for_different_media() {
        let projection = projection(vec![tokenized("track-a", 7), tokenized("track-b", 7)]);

        assert_eq!(projection.len(), 2);
        assert_ne!(projection.keys()[0], projection.keys()[1]);
    }

    #[test]
    fn empty_and_many_row_projections_are_bounded() {
        let empty = Projection::try_new(Vec::<Occurrence>::new()).expect("empty is valid");
        assert!(empty.is_empty());
        assert_eq!(empty.keys(), &[]);

        let many = projection((0..32).map(|index| tokenized("track-a", index)).collect());
        assert_eq!(many.len(), 32);
    }

    #[test]
    fn key_contract_contains_identity_and_token_but_no_metadata() {
        let projection = projection(vec![tokenized("track-a", 7_u8)]);
        let left = &projection.keys()[0];
        let right = &projection.keys()[0];

        assert_eq!(left, right);
        assert_eq!(left.semantic_identity(), &"track-a");
        assert_eq!(left.occurrence_token(), Some(&7));
    }

    #[test]
    fn validated_keys_compose_with_selection_operations_and_reconcile() {
        let first = projection(vec![
            tokenized("track-a", 10),
            tokenized("track-a", 11),
            unique("track-b"),
        ]);
        let reordered = projection(vec![
            unique("track-b"),
            tokenized("track-a", 11),
            tokenized("track-a", 10),
        ]);
        let mut selection = SelectionState::new(DatasetGeneration::new(1));

        selection
            .set_anchor_at(first.keys(), Some(0))
            .expect("validated projection has a cursor");
        selection
            .extend_range(first.keys(), Some(0), Some(2))
            .expect("validated projection supports ranges");
        assert_eq!(selection.len(), 3);

        selection
            .select_all(&first.keys()[..2])
            .expect("validated prefix is a unique projection");
        selection
            .invert(first.keys())
            .expect("validated projection supports invert");
        assert_eq!(selection.len(), 1);
        assert!(selection.contains(&first.keys()[2]));

        selection
            .select_all(first.keys())
            .expect("validated projection supports select-all");
        let outcome = selection
            .reconcile(DatasetGeneration::new(2), reordered.keys())
            .expect("new generation accepts a reordered projection");
        assert_eq!(outcome.retained_keys, 3);
        assert_eq!(outcome.dropped_keys, 0);
        assert!(outcome.anchor_retained);
        assert_eq!(selection.selected_keys().len(), 3);
        assert!(selection.contains(&reordered.keys()[0]));
        assert!(selection.contains(&reordered.keys()[1]));
        assert!(selection.contains(&reordered.keys()[2]));
    }
}
