use std::{
    collections::{HashMap, HashSet},
    hash::Hash,
};

use crate::state::{MediaId, Provider};

use super::{Action, ActionAvailability, ActionDescriptor};

/// The subsystem that owns one planned bulk effect.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum BulkActionOwner {
    Provider(Provider),
    UnifiedQueue,
    UnifiedPlaylist,
    Journal,
    Clipboard,
}

/// Whether equal media identities represent one mutation or distinct effects.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum BulkActionIdentity {
    Occurrence,
    MediaId,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct BulkActionSemantics {
    identity: BulkActionIdentity,
}

impl BulkActionSemantics {
    pub const fn identity(self) -> BulkActionIdentity {
        self.identity
    }
}

impl Action {
    /// Semantics for actions already exposed by multi-track action contexts.
    pub const fn bulk_semantics(self) -> Option<BulkActionSemantics> {
        let identity = match self {
            Self::CopyLink | Self::AddToPlaylist | Self::AddToQueue => {
                BulkActionIdentity::Occurrence
            }
            Self::DeleteFromPlaylist => BulkActionIdentity::Occurrence,
            Self::AddToLiked | Self::AddToListenLater | Self::DeleteFromLiked => {
                BulkActionIdentity::MediaId
            }
            _ => return None,
        };
        Some(BulkActionSemantics { identity })
    }
}

/// One scope-bound occurrence handle supplied to the structural move planner.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct StructuralMoveHandle<S, K> {
    scope: S,
    occurrence_key: K,
}

impl<S, K> StructuralMoveHandle<S, K> {
    pub fn new(scope: S, occurrence_key: K) -> Self {
        Self {
            scope,
            occurrence_key,
        }
    }

    pub const fn scope(&self) -> &S {
        &self.scope
    }

    pub const fn occurrence_key(&self) -> &K {
        &self.occurrence_key
    }
}

/// Stable structural edit produced against one immutable pre-mutation order.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct StructuralBlockMovePlan<K> {
    ordered_occurrence_keys: Vec<K>,
    source_indices: Vec<usize>,
    destination_boundary: usize,
    insertion_index: usize,
    no_op: bool,
}

impl<K> StructuralBlockMovePlan<K> {
    pub fn ordered_occurrence_keys(&self) -> &[K] {
        &self.ordered_occurrence_keys
    }

    pub fn source_indices(&self) -> &[usize] {
        &self.source_indices
    }

    pub const fn destination_boundary(&self) -> usize {
        self.destination_boundary
    }

    /// Post-removal insertion index consumed by the local repository.
    pub const fn insertion_index(&self) -> usize {
        self.insertion_index
    }

    pub const fn is_no_op(&self) -> bool {
        self.no_op
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum StructuralMovePlanError {
    EmptySelection,
    DestinationOutOfBounds { boundary: usize, len: usize },
    DuplicateSnapshotKey { item_index: usize },
    ForeignHandle { item_index: usize },
    DuplicateHandle { item_index: usize },
    MissingHandle { item_index: usize },
}

/// Plan a stable block move against an immutable full-list snapshot.
///
/// Selected rows are ordered by the snapshot, compacted into one block, and
/// inserted at a pre-mutation entry boundary. Non-contiguous input order never
/// changes playlist order. Dropping anywhere inside the selected span is an
/// explicit no-op. Any stale, foreign, duplicate, or ambiguous handle rejects
/// the complete plan before mutation.
pub fn plan_structural_block_move<S, K>(
    expected_scope: &S,
    snapshot: &[K],
    handles: &[StructuralMoveHandle<S, K>],
    destination_boundary: usize,
) -> Result<StructuralBlockMovePlan<K>, StructuralMovePlanError>
where
    S: Eq,
    K: Clone + Eq + Hash,
{
    if handles.is_empty() {
        return Err(StructuralMovePlanError::EmptySelection);
    }
    if destination_boundary > snapshot.len() {
        return Err(StructuralMovePlanError::DestinationOutOfBounds {
            boundary: destination_boundary,
            len: snapshot.len(),
        });
    }

    let mut snapshot_indices = HashMap::with_capacity(snapshot.len());
    for (item_index, key) in snapshot.iter().enumerate() {
        if snapshot_indices.insert(key.clone(), item_index).is_some() {
            return Err(StructuralMovePlanError::DuplicateSnapshotKey { item_index });
        }
    }

    let mut selected = HashSet::with_capacity(handles.len());
    for (item_index, handle) in handles.iter().enumerate() {
        if handle.scope() != expected_scope {
            return Err(StructuralMovePlanError::ForeignHandle { item_index });
        }
        if !selected.insert(handle.occurrence_key().clone()) {
            return Err(StructuralMovePlanError::DuplicateHandle { item_index });
        }
        if !snapshot_indices.contains_key(handle.occurrence_key()) {
            return Err(StructuralMovePlanError::MissingHandle { item_index });
        }
    }

    let source_indices = snapshot
        .iter()
        .enumerate()
        .filter_map(|(index, key)| selected.contains(key).then_some(index))
        .collect::<Vec<_>>();
    let ordered_occurrence_keys = source_indices
        .iter()
        .map(|index| snapshot[*index].clone())
        .collect::<Vec<_>>();
    let span_start = source_indices[0];
    let span_end = *source_indices.last().expect("selection is non-empty");
    let no_op = (span_start..=span_end.saturating_add(1)).contains(&destination_boundary);
    let insertion_index = if no_op {
        span_start
    } else {
        destination_boundary.saturating_sub(
            source_indices
                .iter()
                .filter(|index| **index < destination_boundary)
                .count(),
        )
    };

    Ok(StructuralBlockMovePlan {
        ordered_occurrence_keys,
        source_indices,
        destination_boundary,
        insertion_index,
        no_op,
    })
}

/// One exact route by which an item can perform an action.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct BulkActionCapability {
    action: Action,
    owner: BulkActionOwner,
}

impl BulkActionCapability {
    pub const fn new(action: Action, owner: BulkActionOwner) -> Self {
        Self { action, owner }
    }

    pub const fn action(self) -> Action {
        self.action
    }

    pub const fn owner(self) -> BulkActionOwner {
        self.owner
    }
}

/// Ordered candidate actions for a multi-select surface.
///
/// A surface starts with the actions recommended for its row type, then can
/// add or remove actions without reimplementing capability validation.  The
/// final list is always projected through the selected rows' capabilities.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct BulkActionCandidates {
    actions: Vec<Action>,
}

impl BulkActionCandidates {
    pub fn new(actions: impl IntoIterator<Item = Action>) -> Self {
        let mut candidates = Self::default();
        candidates.extend(actions);
        candidates
    }

    pub fn add(&mut self, action: Action) {
        if !self.actions.contains(&action) {
            self.actions.push(action);
        }
    }

    pub fn remove(&mut self, action: Action) {
        self.actions.retain(|candidate| *candidate != action);
    }

    pub fn contains(&self, action: Action) -> bool {
        self.actions.contains(&action)
    }

    pub fn as_slice(&self) -> &[Action] {
        &self.actions
    }

    pub fn available_descriptors<K>(&self, items: &[BulkActionItem<K>]) -> Vec<ActionDescriptor>
    where
        K: Clone + Eq + Hash,
    {
        available_bulk_action_descriptors(&self.actions, items)
    }
}

impl Extend<Action> for BulkActionCandidates {
    fn extend<T: IntoIterator<Item = Action>>(&mut self, actions: T) {
        for action in actions {
            self.add(action);
        }
    }
}

/// Planner input combining stable occurrence identity with existing media identity.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct BulkActionItem<K> {
    occurrence_key: K,
    media_id: MediaId,
    capabilities: Vec<BulkActionCapability>,
}

impl<K> BulkActionItem<K> {
    pub fn new(
        occurrence_key: K,
        media_id: MediaId,
        capabilities: impl IntoIterator<Item = BulkActionCapability>,
    ) -> Self {
        Self {
            occurrence_key,
            media_id,
            capabilities: capabilities.into_iter().collect(),
        }
    }

    pub const fn occurrence_key(&self) -> &K {
        &self.occurrence_key
    }

    pub const fn media_id(&self) -> &MediaId {
        &self.media_id
    }

    pub fn capabilities(&self) -> &[BulkActionCapability] {
        &self.capabilities
    }
}

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub struct BulkOperationId(usize);

impl BulkOperationId {
    pub(crate) const fn from_index(index: usize) -> Self {
        Self(index)
    }

    pub const fn index(self) -> usize {
        self.0
    }
}

/// One provider/local mutation in a validated bulk plan.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct BulkOperationPlan<K> {
    id: BulkOperationId,
    media_id: MediaId,
    occurrence_keys: Vec<K>,
}

impl<K> BulkOperationPlan<K> {
    pub const fn id(&self) -> BulkOperationId {
        self.id
    }

    pub const fn media_id(&self) -> &MediaId {
        &self.media_id
    }

    pub fn occurrence_keys(&self) -> &[K] {
        &self.occurrence_keys
    }
}

/// Operations sharing one exact effect owner.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct BulkOperationPartition<K> {
    owner: BulkActionOwner,
    operations: Vec<BulkOperationPlan<K>>,
}

impl<K> BulkOperationPartition<K> {
    pub const fn owner(&self) -> BulkActionOwner {
        self.owner
    }

    pub fn operations(&self) -> &[BulkOperationPlan<K>] {
        &self.operations
    }
}

/// Immutable, deterministic output of bulk-action validation and partitioning.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct BulkActionPlan<K> {
    action: Action,
    semantics: BulkActionSemantics,
    selected_occurrence_count: usize,
    operation_count: usize,
    partitions: Vec<BulkOperationPartition<K>>,
}

impl<K> BulkActionPlan<K> {
    pub const fn action(&self) -> Action {
        self.action
    }

    pub const fn semantics(&self) -> BulkActionSemantics {
        self.semantics
    }

    pub const fn selected_occurrence_count(&self) -> usize {
        self.selected_occurrence_count
    }

    pub const fn operation_count(&self) -> usize {
        self.operation_count
    }

    pub fn partitions(&self) -> &[BulkOperationPartition<K>] {
        &self.partitions
    }

    pub fn operation_ids(&self) -> Vec<BulkOperationId> {
        let mut ids = self
            .partitions
            .iter()
            .flat_map(|partition| partition.operations.iter().map(BulkOperationPlan::id))
            .collect::<Vec<_>>();
        ids.sort_by_key(|operation_id| operation_id.index());
        ids
    }
}

/// Privacy-safe validation failures. Item indices refer only to planner input order.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum BulkActionPlanError {
    EmptySelection,
    ActionNotBulk { action: Action },
    UnsupportedItem { item_index: usize },
    AmbiguousCapability { item_index: usize },
    DuplicateOccurrenceKey { item_index: usize },
    ProviderOwnerMismatch { item_index: usize },
    ConflictingIdentityOwner { item_index: usize },
}

/// Describe candidates in caller order and mark only completely valid plans available.
pub fn describe_bulk_actions<K>(
    candidates: &[Action],
    items: &[BulkActionItem<K>],
) -> Vec<ActionDescriptor>
where
    K: Clone + Eq + Hash,
{
    let mut seen = Vec::with_capacity(candidates.len());
    candidates
        .iter()
        .copied()
        .filter_map(|action| {
            if seen.contains(&action) {
                return None;
            }
            seen.push(action);
            let mut descriptor = action.descriptor();
            descriptor.availability = if plan_bulk_action(action, items).is_ok() {
                ActionAvailability::Available
            } else {
                ActionAvailability::Unavailable
            };
            Some(descriptor)
        })
        .collect()
}

/// Return the capability intersection as action descriptors in caller order.
pub fn available_bulk_action_descriptors<K>(
    candidates: &[Action],
    items: &[BulkActionItem<K>],
) -> Vec<ActionDescriptor>
where
    K: Clone + Eq + Hash,
{
    describe_bulk_actions(candidates, items)
        .into_iter()
        .filter(|descriptor| descriptor.availability == ActionAvailability::Available)
        .collect()
}

/// Validate, coalesce, and partition an action without dispatching any effect.
pub fn plan_bulk_action<K>(
    action: Action,
    items: &[BulkActionItem<K>],
) -> Result<BulkActionPlan<K>, BulkActionPlanError>
where
    K: Clone + Eq + Hash,
{
    if items.is_empty() {
        return Err(BulkActionPlanError::EmptySelection);
    }
    let Some(semantics) = action.bulk_semantics() else {
        return Err(BulkActionPlanError::ActionNotBulk { action });
    };

    let mut occurrence_keys = HashSet::with_capacity(items.len());
    let mut partitions = Vec::<BulkOperationPartition<K>>::new();
    let mut identity_operations = HashMap::<MediaId, (BulkActionOwner, usize, usize)>::new();
    let mut operation_count = 0;

    for (item_index, item) in items.iter().enumerate() {
        if !occurrence_keys.insert(item.occurrence_key.clone()) {
            return Err(BulkActionPlanError::DuplicateOccurrenceKey { item_index });
        }

        let mut routes = item
            .capabilities
            .iter()
            .filter(|capability| capability.action == action);
        let Some(capability) = routes.next() else {
            return Err(BulkActionPlanError::UnsupportedItem { item_index });
        };
        if routes.next().is_some() {
            return Err(BulkActionPlanError::AmbiguousCapability { item_index });
        }
        let owner = capability.owner;
        if matches!(owner, BulkActionOwner::Provider(provider) if provider != item.media_id.provider)
        {
            return Err(BulkActionPlanError::ProviderOwnerMismatch { item_index });
        }

        if semantics.identity == BulkActionIdentity::MediaId {
            if let Some((prior_owner, partition_index, operation_index)) =
                identity_operations.get(&item.media_id).copied()
            {
                if prior_owner != owner {
                    return Err(BulkActionPlanError::ConflictingIdentityOwner { item_index });
                }
                partitions[partition_index].operations[operation_index]
                    .occurrence_keys
                    .push(item.occurrence_key.clone());
                continue;
            }
        }

        let partition_index = if let Some(index) = partitions
            .iter()
            .position(|partition| partition.owner == owner)
        {
            index
        } else {
            partitions.push(BulkOperationPartition {
                owner,
                operations: Vec::new(),
            });
            partitions.len() - 1
        };
        let operation_index = partitions[partition_index].operations.len();
        partitions[partition_index]
            .operations
            .push(BulkOperationPlan {
                id: BulkOperationId(operation_count),
                media_id: item.media_id.clone(),
                occurrence_keys: vec![item.occurrence_key.clone()],
            });
        if semantics.identity == BulkActionIdentity::MediaId {
            identity_operations.insert(
                item.media_id.clone(),
                (owner, partition_index, operation_index),
            );
        }
        operation_count += 1;
    }

    Ok(BulkActionPlan {
        action,
        semantics,
        selected_occurrence_count: items.len(),
        operation_count,
        partitions,
    })
}

/// Privacy-safe failure categories for one planned mutation.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum BulkOperationFailure {
    ProviderUnavailable,
    Rejected,
    RequestFailed,
    Internal,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum BulkOperationTerminal {
    Succeeded,
    Failed(BulkOperationFailure),
    Cancelled,
    Superseded,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum BulkOperationState {
    Pending,
    Succeeded,
    Failed(BulkOperationFailure),
    Cancelled,
    Superseded,
}

impl From<BulkOperationTerminal> for BulkOperationState {
    fn from(terminal: BulkOperationTerminal) -> Self {
        match terminal {
            BulkOperationTerminal::Succeeded => Self::Succeeded,
            BulkOperationTerminal::Failed(failure) => Self::Failed(failure),
            BulkOperationTerminal::Cancelled => Self::Cancelled,
            BulkOperationTerminal::Superseded => Self::Superseded,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum BulkOutcomeTransitionError {
    UnknownOperation { operation_id: BulkOperationId },
    AlreadyTerminal { operation_id: BulkOperationId },
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct BulkOutcomeCount {
    operations: usize,
    occurrences: usize,
}

impl BulkOutcomeCount {
    pub const fn operations(self) -> usize {
        self.operations
    }

    pub const fn occurrences(self) -> usize {
        self.occurrences
    }

    fn add_operation(&mut self, covered_occurrences: usize) {
        self.operations += 1;
        self.occurrences += covered_occurrences;
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum BulkActionOverall {
    InProgress,
    Succeeded,
    Failed,
    Cancelled,
    Superseded,
    Partial,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct BulkActionSummary {
    selected_occurrences: usize,
    planned_operations: usize,
    pending: BulkOutcomeCount,
    succeeded: BulkOutcomeCount,
    failed: BulkOutcomeCount,
    cancelled: BulkOutcomeCount,
    superseded: BulkOutcomeCount,
    overall: BulkActionOverall,
}

impl BulkActionSummary {
    pub const fn selected_occurrences(self) -> usize {
        self.selected_occurrences
    }

    pub const fn planned_operations(self) -> usize {
        self.planned_operations
    }

    pub const fn pending(self) -> BulkOutcomeCount {
        self.pending
    }

    pub const fn succeeded(self) -> BulkOutcomeCount {
        self.succeeded
    }

    pub const fn failed(self) -> BulkOutcomeCount {
        self.failed
    }

    pub const fn cancelled(self) -> BulkOutcomeCount {
        self.cancelled
    }

    pub const fn superseded(self) -> BulkOutcomeCount {
        self.superseded
    }

    pub const fn overall(self) -> BulkActionOverall {
        self.overall
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct BulkActionOwnerSummary {
    owner: BulkActionOwner,
    summary: BulkActionSummary,
}

impl BulkActionOwnerSummary {
    pub const fn owner(self) -> BulkActionOwner {
        self.owner
    }

    pub const fn summary(self) -> BulkActionSummary {
        self.summary
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct BulkOutcomeOperation {
    id: BulkOperationId,
    owner: BulkActionOwner,
    covered_occurrences: usize,
    state: BulkOperationState,
}

/// Fixed-size terminal state for one immutable bulk-action plan.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct BulkActionOutcome {
    action: Action,
    selected_occurrence_count: usize,
    operations: Vec<BulkOutcomeOperation>,
}

impl BulkActionOutcome {
    pub fn new<K>(plan: &BulkActionPlan<K>) -> Self {
        let mut operations = plan
            .partitions
            .iter()
            .flat_map(|partition| {
                partition
                    .operations
                    .iter()
                    .map(|operation| BulkOutcomeOperation {
                        id: operation.id,
                        owner: partition.owner,
                        covered_occurrences: operation.occurrence_keys.len(),
                        state: BulkOperationState::Pending,
                    })
            })
            .collect::<Vec<_>>();
        operations.sort_by_key(|operation| operation.id.index());
        Self {
            action: plan.action,
            selected_occurrence_count: plan.selected_occurrence_count,
            operations,
        }
    }

    pub const fn action(&self) -> Action {
        self.action
    }

    pub const fn selected_occurrence_count(&self) -> usize {
        self.selected_occurrence_count
    }

    pub fn operation_count(&self) -> usize {
        self.operations.len()
    }

    pub fn state(&self, operation_id: BulkOperationId) -> Option<BulkOperationState> {
        self.operations
            .iter()
            .find(|operation| operation.id == operation_id)
            .map(|operation| operation.state)
    }

    pub fn record(
        &mut self,
        operation_id: BulkOperationId,
        terminal: BulkOperationTerminal,
    ) -> Result<(), BulkOutcomeTransitionError> {
        let Some(operation) = self
            .operations
            .iter_mut()
            .find(|operation| operation.id == operation_id)
        else {
            return Err(BulkOutcomeTransitionError::UnknownOperation { operation_id });
        };
        if operation.state != BulkOperationState::Pending {
            return Err(BulkOutcomeTransitionError::AlreadyTerminal { operation_id });
        }
        operation.state = terminal.into();
        Ok(())
    }

    pub fn cancel_pending(&mut self) -> usize {
        self.finish_pending(BulkOperationState::Cancelled)
    }

    pub fn supersede_pending(&mut self) -> usize {
        self.finish_pending(BulkOperationState::Superseded)
    }

    pub fn summary(&self) -> BulkActionSummary {
        summarize_operations(self.selected_occurrence_count, self.operations.iter())
    }

    pub fn owner_summaries(&self) -> Vec<BulkActionOwnerSummary> {
        let mut owners = Vec::new();
        for operation in &self.operations {
            if !owners.contains(&operation.owner) {
                owners.push(operation.owner);
            }
        }
        owners
            .into_iter()
            .map(|owner| {
                let selected_occurrences = self
                    .operations
                    .iter()
                    .filter(|operation| operation.owner == owner)
                    .map(|operation| operation.covered_occurrences)
                    .sum();
                BulkActionOwnerSummary {
                    owner,
                    summary: summarize_operations(
                        selected_occurrences,
                        self.operations
                            .iter()
                            .filter(|operation| operation.owner == owner),
                    ),
                }
            })
            .collect()
    }

    fn finish_pending(&mut self, terminal: BulkOperationState) -> usize {
        let mut changed = 0;
        for operation in &mut self.operations {
            if operation.state == BulkOperationState::Pending {
                operation.state = terminal;
                changed += 1;
            }
        }
        changed
    }
}

fn summarize_operations<'a>(
    selected_occurrences: usize,
    operations: impl IntoIterator<Item = &'a BulkOutcomeOperation>,
) -> BulkActionSummary {
    let mut pending = BulkOutcomeCount::default();
    let mut succeeded = BulkOutcomeCount::default();
    let mut failed = BulkOutcomeCount::default();
    let mut cancelled = BulkOutcomeCount::default();
    let mut superseded = BulkOutcomeCount::default();
    let mut planned_operations = 0;

    for operation in operations {
        planned_operations += 1;
        let count = match operation.state {
            BulkOperationState::Pending => &mut pending,
            BulkOperationState::Succeeded => &mut succeeded,
            BulkOperationState::Failed(_) => &mut failed,
            BulkOperationState::Cancelled => &mut cancelled,
            BulkOperationState::Superseded => &mut superseded,
        };
        count.add_operation(operation.covered_occurrences);
    }

    let overall = if pending.operations > 0 {
        BulkActionOverall::InProgress
    } else if succeeded.operations == planned_operations {
        BulkActionOverall::Succeeded
    } else if failed.operations == planned_operations {
        BulkActionOverall::Failed
    } else if cancelled.operations == planned_operations {
        BulkActionOverall::Cancelled
    } else if superseded.operations == planned_operations {
        BulkActionOverall::Superseded
    } else {
        BulkActionOverall::Partial
    };

    BulkActionSummary {
        selected_occurrences,
        planned_operations,
        pending,
        succeeded,
        failed,
        cancelled,
        superseded,
        overall,
    }
}

#[cfg(test)]
mod tests {
    use crate::{
        command::{ActionConfirmation, BulkActionPlanError},
        state::{MediaKind, Provider},
    };

    use super::*;

    fn media(provider: Provider, raw_id: &str) -> MediaId {
        MediaId {
            provider,
            kind: MediaKind::Track,
            raw_id: raw_id.to_owned(),
        }
    }

    fn route(action: Action, owner: BulkActionOwner) -> BulkActionCapability {
        BulkActionCapability::new(action, owner)
    }

    #[test]
    fn candidates_are_ordered_unique_and_editable() {
        let mut candidates =
            BulkActionCandidates::new([Action::CopyLink, Action::AddToQueue, Action::CopyLink]);
        assert_eq!(
            candidates.as_slice(),
            &[Action::CopyLink, Action::AddToQueue]
        );
        candidates.remove(Action::CopyLink);
        candidates.add(Action::AddToLiked);
        assert_eq!(
            candidates.as_slice(),
            &[Action::AddToQueue, Action::AddToLiked]
        );
    }

    fn item(
        key: u64,
        provider: Provider,
        raw_id: &str,
        capabilities: Vec<BulkActionCapability>,
    ) -> BulkActionItem<u64> {
        BulkActionItem::new(key, media(provider, raw_id), capabilities)
    }

    #[test]
    fn empty_selection_has_no_available_action() {
        let descriptors = describe_bulk_actions::<u64>(&[Action::AddToQueue], &[]);
        assert_eq!(descriptors[0].availability, ActionAvailability::Unavailable);
        assert_eq!(
            plan_bulk_action::<u64>(Action::AddToQueue, &[]),
            Err(BulkActionPlanError::EmptySelection)
        );
    }

    #[test]
    fn descriptors_preserve_candidates_deduplicate_and_intersect_capabilities() {
        let spotify = item(
            1,
            Provider::Spotify,
            "s",
            vec![
                route(Action::CopyLink, BulkActionOwner::Clipboard),
                route(Action::AddToQueue, BulkActionOwner::UnifiedQueue),
                route(
                    Action::AddToLiked,
                    BulkActionOwner::Provider(Provider::Spotify),
                ),
            ],
        );
        let youtube = item(
            2,
            Provider::YouTubeMusic,
            "y",
            vec![
                route(Action::AddToQueue, BulkActionOwner::UnifiedQueue),
                route(
                    Action::AddToLiked,
                    BulkActionOwner::Provider(Provider::YouTubeMusic),
                ),
            ],
        );
        let candidates = [
            Action::CopyLink,
            Action::AddToQueue,
            Action::AddToQueue,
            Action::AddToLiked,
            Action::GoToArtist,
        ];
        let descriptors = describe_bulk_actions(&candidates, &[spotify, youtube]);
        assert_eq!(descriptors.len(), 4);
        assert_eq!(descriptors[0].action, Action::CopyLink);
        assert_eq!(descriptors[0].availability, ActionAvailability::Unavailable);
        assert_eq!(descriptors[1].action, Action::AddToQueue);
        assert_eq!(descriptors[1].availability, ActionAvailability::Available);
        assert_eq!(descriptors[2].action, Action::AddToLiked);
        assert_eq!(descriptors[2].availability, ActionAvailability::Available);
        assert_eq!(descriptors[3].action, Action::GoToArtist);
        assert_eq!(descriptors[3].availability, ActionAvailability::Unavailable);
    }

    #[test]
    fn available_descriptors_keep_only_fully_plannable_actions() {
        let items = [item(
            1,
            Provider::Spotify,
            "s",
            vec![route(Action::AddToQueue, BulkActionOwner::UnifiedQueue)],
        )];
        let descriptors =
            available_bulk_action_descriptors(&[Action::GoToArtist, Action::AddToQueue], &items);
        assert_eq!(
            descriptors
                .iter()
                .map(|item| item.action)
                .collect::<Vec<_>>(),
            [Action::AddToQueue]
        );
    }

    #[test]
    fn unsupported_and_ambiguous_capabilities_fail_closed() {
        let unsupported = [item(1, Provider::Spotify, "s", vec![])];
        assert_eq!(
            plan_bulk_action(Action::AddToQueue, &unsupported),
            Err(BulkActionPlanError::UnsupportedItem { item_index: 0 })
        );

        let ambiguous = [item(
            1,
            Provider::Spotify,
            "s",
            vec![
                route(Action::AddToQueue, BulkActionOwner::UnifiedQueue),
                route(
                    Action::AddToQueue,
                    BulkActionOwner::Provider(Provider::Spotify),
                ),
            ],
        )];
        assert_eq!(
            plan_bulk_action(Action::AddToQueue, &ambiguous),
            Err(BulkActionPlanError::AmbiguousCapability { item_index: 0 })
        );
    }

    #[test]
    fn actions_without_bulk_semantics_are_rejected() {
        let items = [item(
            1,
            Provider::Spotify,
            "s",
            vec![route(Action::GoToArtist, BulkActionOwner::Clipboard)],
        )];
        assert_eq!(
            plan_bulk_action(Action::GoToArtist, &items),
            Err(BulkActionPlanError::ActionNotBulk {
                action: Action::GoToArtist
            })
        );
    }

    #[test]
    fn duplicate_occurrence_keys_are_rejected() {
        let items = [
            item(
                1,
                Provider::Spotify,
                "one",
                vec![route(Action::AddToQueue, BulkActionOwner::UnifiedQueue)],
            ),
            item(
                1,
                Provider::Spotify,
                "two",
                vec![route(Action::AddToQueue, BulkActionOwner::UnifiedQueue)],
            ),
        ];
        assert_eq!(
            plan_bulk_action(Action::AddToQueue, &items),
            Err(BulkActionPlanError::DuplicateOccurrenceKey { item_index: 1 })
        );
    }

    #[test]
    fn provider_owned_capability_must_match_media_provider() {
        let items = [item(
            1,
            Provider::YouTubeMusic,
            "y",
            vec![route(
                Action::AddToLiked,
                BulkActionOwner::Provider(Provider::Spotify),
            )],
        )];
        assert_eq!(
            plan_bulk_action(Action::AddToLiked, &items),
            Err(BulkActionPlanError::ProviderOwnerMismatch { item_index: 0 })
        );
    }

    #[test]
    fn occurrence_sensitive_actions_preserve_duplicate_media_rows() {
        let items = [
            item(
                10,
                Provider::Spotify,
                "same",
                vec![route(Action::AddToQueue, BulkActionOwner::UnifiedQueue)],
            ),
            item(
                20,
                Provider::Spotify,
                "same",
                vec![route(Action::AddToQueue, BulkActionOwner::UnifiedQueue)],
            ),
        ];
        let plan = plan_bulk_action(Action::AddToQueue, &items).unwrap();
        assert_eq!(plan.semantics().identity(), BulkActionIdentity::Occurrence);
        assert_eq!(plan.selected_occurrence_count(), 2);
        assert_eq!(plan.operation_count(), 2);
        assert_eq!(plan.partitions()[0].operations()[0].occurrence_keys(), [10]);
        assert_eq!(plan.partitions()[0].operations()[1].occurrence_keys(), [20]);
    }

    #[test]
    fn playlist_delete_is_occurrence_sensitive_for_duplicate_media_rows() {
        let owner = BulkActionOwner::UnifiedPlaylist;
        let items = [
            item(
                10,
                Provider::Spotify,
                "same",
                vec![route(Action::DeleteFromPlaylist, owner)],
            ),
            item(
                20,
                Provider::Spotify,
                "same",
                vec![route(Action::DeleteFromPlaylist, owner)],
            ),
        ];
        let plan = plan_bulk_action(Action::DeleteFromPlaylist, &items).unwrap();
        assert_eq!(plan.semantics().identity(), BulkActionIdentity::Occurrence);
        assert_eq!(plan.operation_count(), 2);
        assert_eq!(plan.partitions()[0].operations()[0].occurrence_keys(), [10]);
        assert_eq!(plan.partitions()[0].operations()[1].occurrence_keys(), [20]);
    }

    #[test]
    fn structural_move_compacts_non_contiguous_rows_in_playlist_order() {
        let scope = "playlist";
        let snapshot = [10, 20, 30, 40, 50];
        let handles = [
            StructuralMoveHandle::new(scope, 40),
            StructuralMoveHandle::new(scope, 20),
        ];
        let plan = plan_structural_block_move(&scope, &snapshot, &handles, 5).unwrap();
        assert_eq!(plan.ordered_occurrence_keys(), [20, 40]);
        assert_eq!(plan.source_indices(), [1, 3]);
        assert_eq!(plan.destination_boundary(), 5);
        assert_eq!(plan.insertion_index(), 3);
        assert!(!plan.is_no_op());
    }

    #[test]
    fn structural_move_adjusts_pre_mutation_boundary_and_detects_span_noop() {
        let scope = "playlist";
        let snapshot = [10, 20, 30, 40, 50];
        let selected = [
            StructuralMoveHandle::new(scope, 20),
            StructuralMoveHandle::new(scope, 40),
        ];
        let before = plan_structural_block_move(&scope, &snapshot, &selected, 0).unwrap();
        assert_eq!(before.insertion_index(), 0);
        assert!(!before.is_no_op());

        for boundary in 1..=4 {
            let inside =
                plan_structural_block_move(&scope, &snapshot, &selected, boundary).unwrap();
            assert!(inside.is_no_op(), "boundary {boundary} must be a no-op");
        }
    }

    #[test]
    fn structural_move_rejects_stale_foreign_overlapping_and_ambiguous_handles() {
        let scope = "playlist";
        let snapshot = [10, 20, 30];
        assert_eq!(
            plan_structural_block_move(
                &scope,
                &snapshot,
                &[StructuralMoveHandle::new("other", 20)],
                0,
            ),
            Err(StructuralMovePlanError::ForeignHandle { item_index: 0 })
        );
        assert_eq!(
            plan_structural_block_move(
                &scope,
                &snapshot,
                &[StructuralMoveHandle::new(scope, 99)],
                0,
            ),
            Err(StructuralMovePlanError::MissingHandle { item_index: 0 })
        );
        assert_eq!(
            plan_structural_block_move(
                &scope,
                &snapshot,
                &[
                    StructuralMoveHandle::new(scope, 20),
                    StructuralMoveHandle::new(scope, 20),
                ],
                0,
            ),
            Err(StructuralMovePlanError::DuplicateHandle { item_index: 1 })
        );
        assert_eq!(
            plan_structural_block_move(
                &scope,
                &[10, 20, 20],
                &[StructuralMoveHandle::new(scope, 20)],
                0,
            ),
            Err(StructuralMovePlanError::DuplicateSnapshotKey { item_index: 2 })
        );
        assert_eq!(
            plan_structural_block_move(
                &scope,
                &snapshot,
                &[StructuralMoveHandle::new(scope, 20)],
                4,
            ),
            Err(StructuralMovePlanError::DestinationOutOfBounds {
                boundary: 4,
                len: 3,
            })
        );
    }

    #[test]
    fn media_id_sensitive_actions_coalesce_duplicate_media_rows() {
        let owner = BulkActionOwner::Provider(Provider::Spotify);
        let items = [
            item(
                10,
                Provider::Spotify,
                "same",
                vec![route(Action::AddToLiked, owner)],
            ),
            item(
                20,
                Provider::Spotify,
                "same",
                vec![route(Action::AddToLiked, owner)],
            ),
        ];
        let plan = plan_bulk_action(Action::AddToLiked, &items).unwrap();
        assert_eq!(plan.semantics().identity(), BulkActionIdentity::MediaId);
        assert_eq!(plan.selected_occurrence_count(), 2);
        assert_eq!(plan.operation_count(), 1);
        assert_eq!(
            plan.partitions()[0].operations()[0].occurrence_keys(),
            [10, 20]
        );
    }

    #[test]
    fn coalesced_identity_with_conflicting_owners_is_rejected() {
        let items = [
            item(
                1,
                Provider::Spotify,
                "same",
                vec![route(
                    Action::AddToLiked,
                    BulkActionOwner::Provider(Provider::Spotify),
                )],
            ),
            item(
                2,
                Provider::Spotify,
                "same",
                vec![route(Action::AddToLiked, BulkActionOwner::Journal)],
            ),
        ];
        assert_eq!(
            plan_bulk_action(Action::AddToLiked, &items),
            Err(BulkActionPlanError::ConflictingIdentityOwner { item_index: 1 })
        );
    }

    #[test]
    fn provider_owned_actions_partition_without_crossing_payloads() {
        let items = [
            item(
                1,
                Provider::Spotify,
                "s",
                vec![route(
                    Action::AddToLiked,
                    BulkActionOwner::Provider(Provider::Spotify),
                )],
            ),
            item(
                2,
                Provider::YouTubeMusic,
                "y",
                vec![route(
                    Action::AddToLiked,
                    BulkActionOwner::Provider(Provider::YouTubeMusic),
                )],
            ),
        ];
        let plan = plan_bulk_action(Action::AddToLiked, &items).unwrap();
        assert_eq!(plan.partitions().len(), 2);
        assert_eq!(
            plan.partitions()[0].owner(),
            BulkActionOwner::Provider(Provider::Spotify)
        );
        assert_eq!(
            plan.partitions()[1].owner(),
            BulkActionOwner::Provider(Provider::YouTubeMusic)
        );
        assert_eq!(
            plan.partitions()[0].operations()[0].media_id().provider,
            Provider::Spotify
        );
        assert_eq!(
            plan.partitions()[1].operations()[0].media_id().provider,
            Provider::YouTubeMusic
        );
    }

    #[test]
    fn shared_owner_keeps_mixed_provider_queue_in_one_partition() {
        let items = [
            item(
                1,
                Provider::Spotify,
                "s",
                vec![route(Action::AddToQueue, BulkActionOwner::UnifiedQueue)],
            ),
            item(
                2,
                Provider::YouTubeMusic,
                "y",
                vec![route(Action::AddToQueue, BulkActionOwner::UnifiedQueue)],
            ),
        ];
        let plan = plan_bulk_action(Action::AddToQueue, &items).unwrap();
        assert_eq!(plan.partitions().len(), 1);
        assert_eq!(plan.partitions()[0].owner(), BulkActionOwner::UnifiedQueue);
        assert_eq!(plan.partitions()[0].operations().len(), 2);
    }

    #[test]
    fn partition_and_operation_order_are_first_seen_and_stable() {
        let youtube_owner = BulkActionOwner::Provider(Provider::YouTubeMusic);
        let spotify_owner = BulkActionOwner::Provider(Provider::Spotify);
        let items = [
            item(
                1,
                Provider::YouTubeMusic,
                "y1",
                vec![route(Action::AddToLiked, youtube_owner)],
            ),
            item(
                2,
                Provider::Spotify,
                "s1",
                vec![route(Action::AddToLiked, spotify_owner)],
            ),
            item(
                3,
                Provider::YouTubeMusic,
                "y2",
                vec![route(Action::AddToLiked, youtube_owner)],
            ),
        ];
        let plan = plan_bulk_action(Action::AddToLiked, &items).unwrap();
        assert_eq!(plan.partitions()[0].owner(), youtube_owner);
        assert_eq!(plan.partitions()[1].owner(), spotify_owner);
        let youtube_ids = plan.partitions()[0]
            .operations()
            .iter()
            .map(|operation| operation.id().index())
            .collect::<Vec<_>>();
        assert_eq!(youtube_ids, [0, 2]);
        assert_eq!(plan.partitions()[1].operations()[0].id().index(), 1);
    }

    #[test]
    fn available_descriptor_preserves_confirmation_metadata() {
        let items = [item(
            1,
            Provider::Spotify,
            "s",
            vec![route(
                Action::DeleteFromLiked,
                BulkActionOwner::Provider(Provider::Spotify),
            )],
        )];
        let descriptors = describe_bulk_actions(&[Action::DeleteFromLiked], &items);
        assert_eq!(descriptors[0].availability, ActionAvailability::Available);
        assert_eq!(descriptors[0].confirmation, ActionConfirmation::Required);
    }

    fn two_provider_like_plan() -> BulkActionPlan<u64> {
        plan_bulk_action(
            Action::AddToLiked,
            &[
                item(
                    1,
                    Provider::Spotify,
                    "s",
                    vec![route(
                        Action::AddToLiked,
                        BulkActionOwner::Provider(Provider::Spotify),
                    )],
                ),
                item(
                    2,
                    Provider::YouTubeMusic,
                    "y",
                    vec![route(
                        Action::AddToLiked,
                        BulkActionOwner::Provider(Provider::YouTubeMusic),
                    )],
                ),
            ],
        )
        .unwrap()
    }

    #[test]
    fn outcome_starts_with_one_fixed_pending_slot_per_operation() {
        let plan = two_provider_like_plan();
        let outcome = BulkActionOutcome::new(&plan);
        assert_eq!(outcome.action(), Action::AddToLiked);
        assert_eq!(outcome.selected_occurrence_count(), 2);
        assert_eq!(outcome.operation_count(), 2);
        assert_eq!(
            outcome.state(BulkOperationId(0)),
            Some(BulkOperationState::Pending)
        );
        assert_eq!(
            outcome.state(BulkOperationId(1)),
            Some(BulkOperationState::Pending)
        );
        let summary = outcome.summary();
        assert_eq!(summary.selected_occurrences(), 2);
        assert_eq!(summary.planned_operations(), 2);
        assert_eq!(summary.pending().operations(), 2);
        assert_eq!(summary.pending().occurrences(), 2);
        assert_eq!(summary.overall(), BulkActionOverall::InProgress);
    }

    #[test]
    fn mixed_success_and_failure_produce_partial_global_and_owner_summaries() {
        let plan = two_provider_like_plan();
        let mut outcome = BulkActionOutcome::new(&plan);
        outcome
            .record(BulkOperationId(0), BulkOperationTerminal::Succeeded)
            .unwrap();
        outcome
            .record(
                BulkOperationId(1),
                BulkOperationTerminal::Failed(BulkOperationFailure::ProviderUnavailable),
            )
            .unwrap();

        let summary = outcome.summary();
        assert_eq!(summary.overall(), BulkActionOverall::Partial);
        assert_eq!(summary.succeeded().operations(), 1);
        assert_eq!(summary.failed().operations(), 1);
        let owners = outcome.owner_summaries();
        assert_eq!(owners.len(), 2);
        assert_eq!(
            owners[0].owner(),
            BulkActionOwner::Provider(Provider::Spotify)
        );
        assert_eq!(owners[0].summary().overall(), BulkActionOverall::Succeeded);
        assert_eq!(owners[0].summary().selected_occurrences(), 1);
        assert_eq!(owners[0].summary().planned_operations(), 1);
        assert_eq!(owners[0].summary().succeeded().occurrences(), 1);
        assert_eq!(
            owners[1].owner(),
            BulkActionOwner::Provider(Provider::YouTubeMusic)
        );
        assert_eq!(owners[1].summary().overall(), BulkActionOverall::Failed);
        assert_eq!(owners[1].summary().selected_occurrences(), 1);
        assert_eq!(owners[1].summary().planned_operations(), 1);
        assert_eq!(owners[1].summary().failed().occurrences(), 1);
    }

    #[test]
    fn coalesced_operation_summary_counts_every_covered_occurrence() {
        let owner = BulkActionOwner::Provider(Provider::Spotify);
        let plan = plan_bulk_action(
            Action::AddToLiked,
            &[
                item(
                    10,
                    Provider::Spotify,
                    "same",
                    vec![route(Action::AddToLiked, owner)],
                ),
                item(
                    20,
                    Provider::Spotify,
                    "same",
                    vec![route(Action::AddToLiked, owner)],
                ),
            ],
        )
        .unwrap();
        let mut outcome = BulkActionOutcome::new(&plan);
        outcome
            .record(BulkOperationId(0), BulkOperationTerminal::Succeeded)
            .unwrap();
        let summary = outcome.summary();
        assert_eq!(summary.planned_operations(), 1);
        assert_eq!(summary.selected_occurrences(), 2);
        assert_eq!(summary.succeeded().operations(), 1);
        assert_eq!(summary.succeeded().occurrences(), 2);
        assert_eq!(summary.overall(), BulkActionOverall::Succeeded);
        let owners = outcome.owner_summaries();
        assert_eq!(owners.len(), 1);
        assert_eq!(owners[0].summary().selected_occurrences(), 2);
        assert_eq!(owners[0].summary().planned_operations(), 1);
        assert_eq!(owners[0].summary().succeeded().occurrences(), 2);
    }

    #[test]
    fn homogeneous_terminal_results_have_exact_overall_state() {
        let plan = two_provider_like_plan();

        let mut succeeded = BulkActionOutcome::new(&plan);
        succeeded
            .record(BulkOperationId(0), BulkOperationTerminal::Succeeded)
            .unwrap();
        succeeded
            .record(BulkOperationId(1), BulkOperationTerminal::Succeeded)
            .unwrap();
        assert_eq!(succeeded.summary().overall(), BulkActionOverall::Succeeded);

        let mut failed = BulkActionOutcome::new(&plan);
        for id in [BulkOperationId(0), BulkOperationId(1)] {
            failed
                .record(
                    id,
                    BulkOperationTerminal::Failed(BulkOperationFailure::RequestFailed),
                )
                .unwrap();
        }
        assert_eq!(failed.summary().overall(), BulkActionOverall::Failed);

        let mut cancelled = BulkActionOutcome::new(&plan);
        assert_eq!(cancelled.cancel_pending(), 2);
        assert_eq!(cancelled.summary().overall(), BulkActionOverall::Cancelled);

        let mut superseded = BulkActionOutcome::new(&plan);
        assert_eq!(superseded.supersede_pending(), 2);
        assert_eq!(
            superseded.summary().overall(),
            BulkActionOverall::Superseded
        );
    }

    #[test]
    fn cancelling_pending_operations_preserves_completed_success() {
        let plan = two_provider_like_plan();
        let mut outcome = BulkActionOutcome::new(&plan);
        outcome
            .record(BulkOperationId(0), BulkOperationTerminal::Succeeded)
            .unwrap();
        assert_eq!(outcome.cancel_pending(), 1);
        assert_eq!(outcome.cancel_pending(), 0);
        assert_eq!(
            outcome.state(BulkOperationId(0)),
            Some(BulkOperationState::Succeeded)
        );
        assert_eq!(
            outcome.state(BulkOperationId(1)),
            Some(BulkOperationState::Cancelled)
        );
        let summary = outcome.summary();
        assert_eq!(summary.succeeded().operations(), 1);
        assert_eq!(summary.cancelled().operations(), 1);
        assert_eq!(summary.overall(), BulkActionOverall::Partial);
    }

    #[test]
    fn superseding_pending_operations_preserves_completed_failure() {
        let plan = two_provider_like_plan();
        let mut outcome = BulkActionOutcome::new(&plan);
        outcome
            .record(
                BulkOperationId(0),
                BulkOperationTerminal::Failed(BulkOperationFailure::Rejected),
            )
            .unwrap();
        assert_eq!(outcome.supersede_pending(), 1);
        assert_eq!(
            outcome.state(BulkOperationId(0)),
            Some(BulkOperationState::Failed(BulkOperationFailure::Rejected))
        );
        assert_eq!(
            outcome.state(BulkOperationId(1)),
            Some(BulkOperationState::Superseded)
        );
        assert_eq!(outcome.summary().overall(), BulkActionOverall::Partial);
    }

    #[test]
    fn unknown_operation_is_rejected_without_growing_outcome_state() {
        let plan = two_provider_like_plan();
        let mut outcome = BulkActionOutcome::new(&plan);
        assert_eq!(
            outcome.record(BulkOperationId(99), BulkOperationTerminal::Succeeded),
            Err(BulkOutcomeTransitionError::UnknownOperation {
                operation_id: BulkOperationId(99)
            })
        );
        assert_eq!(outcome.operation_count(), 2);
        assert_eq!(outcome.summary().pending().operations(), 2);
    }

    #[test]
    fn no_terminal_operation_state_can_be_overwritten() {
        let plan = two_provider_like_plan();
        let terminals = [
            (
                BulkOperationTerminal::Succeeded,
                BulkOperationState::Succeeded,
            ),
            (
                BulkOperationTerminal::Failed(BulkOperationFailure::Internal),
                BulkOperationState::Failed(BulkOperationFailure::Internal),
            ),
            (
                BulkOperationTerminal::Cancelled,
                BulkOperationState::Cancelled,
            ),
            (
                BulkOperationTerminal::Superseded,
                BulkOperationState::Superseded,
            ),
        ];
        for (terminal, expected_state) in terminals {
            let mut outcome = BulkActionOutcome::new(&plan);
            outcome.record(BulkOperationId(0), terminal).unwrap();
            assert_eq!(
                outcome.record(BulkOperationId(0), BulkOperationTerminal::Succeeded),
                Err(BulkOutcomeTransitionError::AlreadyTerminal {
                    operation_id: BulkOperationId(0)
                })
            );
            assert_eq!(outcome.state(BulkOperationId(0)), Some(expected_state));
        }
    }

    #[test]
    fn any_pending_operation_keeps_the_outcome_in_progress() {
        let plan = two_provider_like_plan();
        let mut outcome = BulkActionOutcome::new(&plan);
        outcome
            .record(BulkOperationId(0), BulkOperationTerminal::Succeeded)
            .unwrap();
        let summary = outcome.summary();
        assert_eq!(summary.succeeded().operations(), 1);
        assert_eq!(summary.pending().operations(), 1);
        assert_eq!(summary.overall(), BulkActionOverall::InProgress);
    }
}
