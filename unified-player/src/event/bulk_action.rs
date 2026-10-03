//! Event-layer adapters for the pure bulk-action planner.
//!
//! The adapter owns mapping from existing row payloads to provider identities,
//! exact operation owners, and the small runtime bridge that starts one
//! correlated UI operation before dispatching validated mutation envelopes.

use std::collections::HashSet;

use anyhow::Result;

use crate::{
    client::{BulkSendError, ClientRequest, ClientRequestSender, RequestDelivery, RequestDomain},
    command::{
        plan_bulk_action, Action, BulkActionCandidates, BulkActionCapability, BulkActionItem,
        BulkActionOwner, BulkActionPlan, BulkActionPlanError, BulkOperationFailure,
        BulkOperationId, BulkOperationTerminal,
    },
    config::ActiveProvider,
    state::{
        BulkActionSelectionEpoch, BulkOperationHandle, BulkOperationRuntimeError, MediaId,
        MediaKind, PlaylistEntryId, Provider, QueueActionItem, QueueActionMenu, QueueActionPayload,
        QueueSelectionScope, Track, TracksActionMenu, UnifiedPlaylistActionItem,
        UnifiedPlaylistActionMenu, UnifiedPlaylistSelectionScope, YouTubeTrack,
        YouTubeTracksActionMenu,
    },
};
use rspotify::prelude::Id;

/// A menu snapshot can be used only while its provider selection epoch still
/// matches.  Errors carry no payload or provider identifiers beyond typed
/// enums and bounded indices.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum BulkActionMenuError {
    DuplicateOccurrenceKey {
        item_index: usize,
    },
    EpochMismatch {
        provider: Provider,
        expected: u64,
        actual: u64,
    },
    ScopeMismatch,
    OccurrenceMismatch,
    ActionUnavailable {
        action: Action,
    },
    Plan(BulkActionPlanError),
}

type QueueOccurrence = crate::state::OccurrenceDescriptor<MediaId, u64>;
type UnifiedPlaylistOccurrence = crate::state::OccurrenceDescriptor<MediaId, PlaylistEntryId>;

/// Build a Queue `AddToQueue` plan only when the immutable menu scope and every
/// occurrence descriptor still match the current projection.
pub(crate) fn replan_queue_menu(
    menu: &QueueActionMenu,
    current_scope: &QueueSelectionScope,
    current_items: &[QueueActionItem],
) -> Result<BulkActionPlan<QueueOccurrence>, BulkActionMenuError> {
    if menu.scope() != current_scope {
        return Err(BulkActionMenuError::ScopeMismatch);
    }
    if menu.items() != current_items {
        return Err(BulkActionMenuError::OccurrenceMismatch);
    }
    let owner = match menu.scope().display_mode() {
        crate::state::QueueDisplayMode::Unified => BulkActionOwner::UnifiedQueue,
        crate::state::QueueDisplayMode::NativeSpotify => {
            BulkActionOwner::Provider(Provider::Spotify)
        }
    };
    let items = menu
        .items()
        .iter()
        .map(|item| {
            BulkActionItem::new(
                item.occurrence().clone(),
                item.media_id().clone(),
                [BulkActionCapability::new(Action::AddToQueue, owner)],
            )
        })
        .collect::<Vec<_>>();
    plan_bulk_action(Action::AddToQueue, &items).map_err(BulkActionMenuError::Plan)
}

/// Build a `UnifiedPlaylist` `AddToQueue` plan only when the immutable playlist
/// scope and exact occurrence descriptors still match the current rows.
pub(crate) fn replan_unified_playlist_menu(
    action: Action,
    menu: &UnifiedPlaylistActionMenu,
    current_scope: &UnifiedPlaylistSelectionScope,
    current_items: &[UnifiedPlaylistActionItem],
) -> Result<BulkActionPlan<UnifiedPlaylistOccurrence>, BulkActionMenuError> {
    if menu.scope() != current_scope {
        return Err(BulkActionMenuError::ScopeMismatch);
    }
    if menu.items().len() != current_items.len()
        || menu
            .items()
            .iter()
            .zip(current_items)
            .any(|(captured, current)| captured.occurrence() != current.occurrence())
    {
        return Err(BulkActionMenuError::OccurrenceMismatch);
    }
    if !menu
        .actions()
        .iter()
        .any(|descriptor| descriptor.action == action)
    {
        return Err(BulkActionMenuError::ActionUnavailable { action });
    }
    let owner = match action {
        Action::CopyLink => BulkActionOwner::Clipboard,
        Action::AddToPlaylist | Action::DeleteFromPlaylist => BulkActionOwner::UnifiedPlaylist,
        Action::AddToQueue => BulkActionOwner::UnifiedQueue,
        _ => return Err(BulkActionMenuError::ActionUnavailable { action }),
    };
    let items = current_items
        .iter()
        .map(|item| {
            BulkActionItem::new(
                item.occurrence().clone(),
                item.item().media_id.clone(),
                [BulkActionCapability::new(action, owner)],
            )
        })
        .collect::<Vec<_>>();
    plan_bulk_action(action, &items).map_err(BulkActionMenuError::Plan)
}

/// Dispatch one Queue menu plan through its exact owner. Unified rows use one
/// provider-neutral batch; native Spotify rows retain one correlated request
/// per playable occurrence.
pub(crate) fn dispatch_queue_menu(
    menu: &QueueActionMenu,
    plan: &BulkActionPlan<QueueOccurrence>,
    ui: &mut crate::state::UIState,
    client_pub: &ClientRequestSender,
) -> Result<(), BulkDispatchError> {
    match menu.scope().display_mode() {
        crate::state::QueueDisplayMode::Unified => {
            let items = menu
                .items()
                .iter()
                .map(|item| match item.payload() {
                    QueueActionPayload::Unified(media) => Ok(media.clone()),
                    QueueActionPayload::Native(_) => Err(BulkDispatchError::InvalidAssignments(
                        BulkAssignmentError::NotMutation {
                            assignment_index: 0,
                        },
                    )),
                })
                .collect::<Result<Vec<_>, _>>()?;
            let assignment = BulkRequestAssignment::new(
                ClientRequest::AddItemsToUserQueue(items),
                plan.operation_ids(),
            );
            dispatch_bulk_requests(ui, client_pub, plan, vec![assignment]).map(|_| ())
        }
        crate::state::QueueDisplayMode::NativeSpotify => {
            let assignments = plan
                .operation_ids()
                .into_iter()
                .zip(menu.items())
                .map(|(operation_id, item)| match item.payload() {
                    QueueActionPayload::Native(playable) => Ok(BulkRequestAssignment::one(
                        ClientRequest::AddPlayableToQueue(playable.clone()),
                        operation_id,
                    )),
                    QueueActionPayload::Unified(_) => Err(BulkDispatchError::InvalidAssignments(
                        BulkAssignmentError::NotMutation {
                            assignment_index: 0,
                        },
                    )),
                })
                .collect::<Result<Vec<_>, _>>()?;
            dispatch_bulk_requests(ui, client_pub, plan, assignments).map(|_| ())
        }
    }
}

/// Dispatch a `UnifiedPlaylist` `AddToQueue` plan as one mixed-provider unified
/// queue batch.
pub(crate) fn dispatch_unified_playlist_menu(
    menu: &UnifiedPlaylistActionMenu,
    plan: &BulkActionPlan<UnifiedPlaylistOccurrence>,
    ui: &mut crate::state::UIState,
    client_pub: &ClientRequestSender,
) -> Result<(), BulkDispatchError> {
    let items = menu
        .items()
        .iter()
        .map(|item| {
            item.item()
                .playable_media()
                .ok_or(BulkDispatchError::InvalidAssignments(
                    BulkAssignmentError::NotMutation {
                        assignment_index: 0,
                    },
                ))
        })
        .collect::<Result<Vec<_>, _>>()?;
    let assignment = BulkRequestAssignment::new(
        ClientRequest::AddItemsToUserQueue(items),
        plan.operation_ids(),
    );
    dispatch_bulk_requests(ui, client_pub, plan, vec![assignment]).map(|_| ())
}

pub(crate) fn spotify_bulk_action_menu(
    tracks: &[Track],
    candidates: &[Action],
    provider_epoch: u64,
) -> Result<TracksActionMenu, BulkActionMenuError> {
    let items = tracks
        .iter()
        .map(spotify_bulk_action_item)
        .collect::<Vec<_>>();
    ensure_unique_occurrence_keys(&items)?;
    let candidates = BulkActionCandidates::new(candidates.iter().copied());
    let actions = candidates.available_descriptors(&items);
    Ok(TracksActionMenu::new(
        tracks.to_vec(),
        actions,
        BulkActionSelectionEpoch::new(Provider::Spotify, provider_epoch),
    ))
}

pub(crate) fn youtube_bulk_action_menu(
    tracks: &[YouTubeTrack],
    candidates: &[Action],
    provider_epoch: u64,
) -> Result<YouTubeTracksActionMenu, BulkActionMenuError> {
    let items = tracks
        .iter()
        .map(youtube_bulk_action_item)
        .collect::<Vec<_>>();
    ensure_unique_occurrence_keys(&items)?;
    let candidates = BulkActionCandidates::new(candidates.iter().copied());
    let actions = candidates.available_descriptors(&items);
    Ok(YouTubeTracksActionMenu::new(
        tracks.to_vec(),
        actions,
        BulkActionSelectionEpoch::new(Provider::YouTubeMusic, provider_epoch),
    ))
}

pub(crate) fn replan_spotify_menu(
    menu: &TracksActionMenu,
    current_epoch: BulkActionSelectionEpoch,
    action: Action,
) -> Result<BulkActionPlan<MediaId>, BulkActionMenuError> {
    replan_menu(
        menu,
        current_epoch,
        action,
        menu.items().iter().map(spotify_bulk_action_item),
    )
}

/// Re-plan a Spotify menu after a concrete target is chosen.  The target
/// owner is explicit so a cross-provider queue route cannot accidentally
/// inherit the native Spotify owner.
pub(crate) fn replan_spotify_menu_for_owner(
    menu: &TracksActionMenu,
    current_epoch: BulkActionSelectionEpoch,
    action: Action,
    owner: BulkActionOwner,
) -> Result<BulkActionPlan<MediaId>, BulkActionMenuError> {
    replan_menu(
        menu,
        current_epoch,
        action,
        menu.items()
            .iter()
            .map(|track| spotify_bulk_action_item_for_owner(track, action, owner)),
    )
}

pub(crate) fn replan_youtube_menu(
    menu: &YouTubeTracksActionMenu,
    current_epoch: BulkActionSelectionEpoch,
    action: Action,
) -> Result<BulkActionPlan<MediaId>, BulkActionMenuError> {
    replan_menu(
        menu,
        current_epoch,
        action,
        menu.items().iter().map(youtube_bulk_action_item),
    )
}

/// Re-plan a `YouTube` menu after a concrete native or unified target is
/// chosen.  Only the requested action's owner is overridden.
pub(crate) fn replan_youtube_menu_for_owner(
    menu: &YouTubeTracksActionMenu,
    current_epoch: BulkActionSelectionEpoch,
    action: Action,
    owner: BulkActionOwner,
) -> Result<BulkActionPlan<MediaId>, BulkActionMenuError> {
    replan_menu(
        menu,
        current_epoch,
        action,
        menu.items()
            .iter()
            .map(|track| youtube_bulk_action_item_for_owner(track, action, owner)),
    )
}

pub(crate) fn plan_spotify_tracks(
    tracks: &[Track],
    current_epoch: BulkActionSelectionEpoch,
    action: Action,
) -> Result<BulkActionPlan<MediaId>, BulkActionMenuError> {
    let menu = spotify_bulk_action_menu(tracks, &[action], current_epoch.value())?;
    replan_spotify_menu(&menu, current_epoch, action)
}

pub(crate) fn plan_spotify_tracks_for_owner(
    tracks: &[Track],
    current_epoch: BulkActionSelectionEpoch,
    action: Action,
    owner: BulkActionOwner,
) -> Result<BulkActionPlan<MediaId>, BulkActionMenuError> {
    let menu = spotify_bulk_action_menu(tracks, &[action], current_epoch.value())?;
    replan_spotify_menu_for_owner(&menu, current_epoch, action, owner)
}

pub(crate) fn plan_youtube_tracks(
    tracks: &[YouTubeTrack],
    current_epoch: BulkActionSelectionEpoch,
    action: Action,
) -> Result<BulkActionPlan<MediaId>, BulkActionMenuError> {
    let menu = youtube_bulk_action_menu(tracks, &[action], current_epoch.value())?;
    replan_youtube_menu(&menu, current_epoch, action)
}

pub(crate) fn plan_youtube_tracks_for_owner(
    tracks: &[YouTubeTrack],
    current_epoch: BulkActionSelectionEpoch,
    action: Action,
    owner: BulkActionOwner,
) -> Result<BulkActionPlan<MediaId>, BulkActionMenuError> {
    let menu = youtube_bulk_action_menu(tracks, &[action], current_epoch.value())?;
    replan_youtube_menu_for_owner(&menu, current_epoch, action, owner)
}

pub(crate) fn current_spotify_epoch(ui: &crate::state::UIStateGuard) -> BulkActionSelectionEpoch {
    BulkActionSelectionEpoch::new(
        Provider::Spotify,
        ui.provider_selection_epoch(ActiveProvider::Spotify),
    )
}

pub(crate) fn current_youtube_epoch(ui: &crate::state::UIStateGuard) -> BulkActionSelectionEpoch {
    BulkActionSelectionEpoch::new(
        Provider::YouTubeMusic,
        ui.provider_selection_epoch(ActiveProvider::YouTubeMusic),
    )
}

fn replan_menu<T>(
    menu: &crate::state::BulkActionMenu<T>,
    current_epoch: BulkActionSelectionEpoch,
    action: Action,
    items: impl Iterator<Item = BulkActionItem<MediaId>>,
) -> Result<BulkActionPlan<MediaId>, BulkActionMenuError> {
    if menu.epoch() != current_epoch {
        return Err(BulkActionMenuError::EpochMismatch {
            provider: menu.epoch().provider(),
            expected: menu.epoch().value(),
            actual: current_epoch.value(),
        });
    }
    if !menu
        .actions()
        .iter()
        .any(|descriptor| descriptor.action == action)
    {
        return Err(BulkActionMenuError::ActionUnavailable { action });
    }
    plan_bulk_action(action, &items.collect::<Vec<_>>()).map_err(BulkActionMenuError::Plan)
}

/// One immutable request-to-operation assignment.  A provider batch may
/// carry several IDs, but each plan ID must belong to exactly one assignment.
#[derive(Debug)]
pub(crate) struct BulkRequestAssignment {
    operation_ids: Vec<BulkOperationId>,
    request: ClientRequest,
}

impl BulkRequestAssignment {
    pub(crate) fn new(
        request: ClientRequest,
        operation_ids: impl IntoIterator<Item = BulkOperationId>,
    ) -> Self {
        Self {
            operation_ids: operation_ids.into_iter().collect(),
            request,
        }
    }

    pub(crate) fn one(request: ClientRequest, operation_id: BulkOperationId) -> Self {
        Self::new(request, [operation_id])
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum BulkAssignmentError {
    EmptyAssignment,
    NotOrdered { assignment_index: usize },
    NotMutation { assignment_index: usize },
    UnknownOperation { operation_id: BulkOperationId },
    DuplicateOperation { operation_id: BulkOperationId },
    MissingOperation { operation_id: BulkOperationId },
}

#[allow(dead_code)] // Typed variants preserve fail-closed diagnostics for callers.
#[derive(Debug)]
pub(crate) enum BulkDispatchError {
    InvalidAssignments(BulkAssignmentError),
    Start(BulkOperationRuntimeError),
    Send(BulkSendError),
    Record(BulkOperationRuntimeError),
}

impl std::fmt::Display for BulkDispatchError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let message = match self {
            Self::InvalidAssignments(_) => "invalid bulk request assignments",
            Self::Start(_) => "bulk operation was refused",
            Self::Send(error) => return std::fmt::Display::fmt(error, formatter),
            Self::Record(_) => "bulk operation terminal was refused",
        };
        formatter.write_str(message)
    }
}

impl std::error::Error for BulkDispatchError {}

/// Validate exact coverage before UI state is changed or any request is sent.
pub(crate) fn validate_bulk_assignments<K>(
    plan: &BulkActionPlan<K>,
    assignments: &[BulkRequestAssignment],
) -> Result<(), BulkAssignmentError> {
    let expected = plan.operation_ids();
    let mut seen = HashSet::with_capacity(expected.len());
    for (assignment_index, assignment) in assignments.iter().enumerate() {
        if assignment.operation_ids.is_empty() {
            return Err(BulkAssignmentError::EmptyAssignment);
        }
        if assignment.request.delivery_policy() != RequestDelivery::Ordered {
            return Err(BulkAssignmentError::NotOrdered { assignment_index });
        }
        if assignment.request.domain() != RequestDomain::PlaylistMutation {
            return Err(BulkAssignmentError::NotMutation { assignment_index });
        }
        for operation_id in &assignment.operation_ids {
            if !expected.contains(operation_id) {
                return Err(BulkAssignmentError::UnknownOperation {
                    operation_id: *operation_id,
                });
            }
            if !seen.insert(*operation_id) {
                return Err(BulkAssignmentError::DuplicateOperation {
                    operation_id: *operation_id,
                });
            }
        }
    }
    for operation_id in expected {
        if !seen.contains(&operation_id) {
            return Err(BulkAssignmentError::MissingOperation { operation_id });
        }
    }
    Ok(())
}

pub(crate) fn validate_bulk_operation_ids<K>(
    plan: &BulkActionPlan<K>,
    operation_ids: &[BulkOperationId],
) -> Result<(), BulkAssignmentError> {
    let expected = plan.operation_ids();
    let mut seen = HashSet::with_capacity(operation_ids.len());
    for operation_id in operation_ids {
        if !expected.contains(operation_id) {
            return Err(BulkAssignmentError::UnknownOperation {
                operation_id: *operation_id,
            });
        }
        if !seen.insert(*operation_id) {
            return Err(BulkAssignmentError::DuplicateOperation {
                operation_id: *operation_id,
            });
        }
    }
    for operation_id in expected {
        if !seen.contains(&operation_id) {
            return Err(BulkAssignmentError::MissingOperation { operation_id });
        }
    }
    Ok(())
}

/// Start and send a provider-backed bulk effect.  Channel success deliberately
/// leaves every sent operation Pending; the scheduler records the real
/// provider terminal later.  A failed send rejects that request and every
/// request that was not attempted, before returning the original error.
pub(crate) fn dispatch_bulk_requests<K: Clone>(
    ui: &mut crate::state::UIState,
    client_pub: &ClientRequestSender,
    plan: &BulkActionPlan<K>,
    assignments: Vec<BulkRequestAssignment>,
) -> Result<BulkOperationHandle, BulkDispatchError> {
    validate_bulk_assignments(plan, &assignments).map_err(BulkDispatchError::InvalidAssignments)?;
    let handle = ui
        .start_bulk_operation(plan)
        .map_err(BulkDispatchError::Start)?;
    let ids_by_assignment = assignments
        .iter()
        .map(|assignment| assignment.operation_ids.clone())
        .collect::<Vec<_>>();
    for (assignment_index, assignment) in assignments.into_iter().enumerate() {
        let operation_ids = assignment.operation_ids;
        let request = assignment.request;
        if let Err(error) = client_pub.send_bulk(request, handle.reference(), &operation_ids) {
            let rejected = ids_by_assignment[assignment_index..]
                .iter()
                .flatten()
                .copied()
                .collect::<Vec<_>>();
            ui.record_bulk_terminal(
                handle.reference(),
                &rejected,
                BulkOperationTerminal::Failed(BulkOperationFailure::Rejected),
            )
            .map_err(BulkDispatchError::Record)?;
            return Err(BulkDispatchError::Send(error));
        }
    }
    Ok(handle)
}

pub(crate) fn start_bulk_local<K: Clone>(
    ui: &mut crate::state::UIState,
    plan: &BulkActionPlan<K>,
    operation_ids: &[BulkOperationId],
) -> Result<BulkOperationHandle, BulkDispatchError> {
    validate_bulk_operation_ids(plan, operation_ids)
        .map_err(BulkDispatchError::InvalidAssignments)?;
    ui.start_bulk_operation(plan)
        .map_err(BulkDispatchError::Start)
}

pub(crate) fn complete_bulk_local(
    ui: &mut crate::state::UIState,
    handle: &BulkOperationHandle,
    operation_ids: &[BulkOperationId],
    success: bool,
) -> Result<(), BulkDispatchError> {
    let terminal = if success {
        BulkOperationTerminal::Succeeded
    } else {
        BulkOperationTerminal::Failed(BulkOperationFailure::Internal)
    };
    ui.record_bulk_terminal(handle.reference(), operation_ids, terminal)
        .map_err(BulkDispatchError::Record)
}

pub(crate) fn dispatch_spotify_native_queue_tracks(
    ui: &mut crate::state::UIState,
    client_pub: &ClientRequestSender,
    tracks: Vec<Track>,
    epoch: BulkActionSelectionEpoch,
) -> Result<()> {
    let plan = plan_spotify_tracks_for_owner(
        &tracks,
        epoch,
        Action::AddToQueue,
        BulkActionOwner::Provider(Provider::Spotify),
    )
    .map_err(|_| anyhow::anyhow!("bulk queue plan is unavailable"))?;
    let assignments = plan
        .operation_ids()
        .into_iter()
        .zip(tracks)
        .map(|(operation_id, track)| {
            ui.spotify_queue_labels.remember_track(&track);
            BulkRequestAssignment::one(
                ClientRequest::AddPlayableToQueue(track.id.into()),
                operation_id,
            )
        })
        .collect();
    dispatch_bulk_requests(ui, client_pub, &plan, assignments)?;
    Ok(())
}

pub(crate) fn dispatch_youtube_queue_tracks(
    ui: &mut crate::state::UIState,
    client_pub: &ClientRequestSender,
    tracks: Vec<YouTubeTrack>,
    epoch: BulkActionSelectionEpoch,
) -> Result<()> {
    let plan = plan_youtube_tracks_for_owner(
        &tracks,
        epoch,
        Action::AddToQueue,
        BulkActionOwner::UnifiedQueue,
    )
    .map_err(|_| anyhow::anyhow!("bulk queue plan is unavailable"))?;
    let assignment = BulkRequestAssignment::new(
        ClientRequest::AddItemsToUserQueue(
            tracks
                .into_iter()
                .map(crate::state::PlayableMedia::YouTube)
                .collect(),
        ),
        plan.operation_ids(),
    );
    dispatch_bulk_requests(ui, client_pub, &plan, vec![assignment])?;
    Ok(())
}

fn ensure_unique_occurrence_keys<K>(items: &[BulkActionItem<K>]) -> Result<(), BulkActionMenuError>
where
    K: Eq + std::hash::Hash,
{
    let mut seen = HashSet::with_capacity(items.len());
    for (item_index, item) in items.iter().enumerate() {
        if !seen.insert(item.occurrence_key()) {
            return Err(BulkActionMenuError::DuplicateOccurrenceKey { item_index });
        }
    }
    Ok(())
}

fn spotify_bulk_action_item(track: &Track) -> BulkActionItem<MediaId> {
    spotify_bulk_action_item_for_owner(track, Action::CopyLink, BulkActionOwner::Clipboard)
}

fn spotify_bulk_action_item_for_owner(
    track: &Track,
    override_action: Action,
    override_owner: BulkActionOwner,
) -> BulkActionItem<MediaId> {
    let media_id = spotify_media_id(track);
    let capabilities = [
        capability(Action::CopyLink, BulkActionOwner::Clipboard),
        capability(
            Action::AddToPlaylist,
            BulkActionOwner::Provider(Provider::Spotify),
        ),
        capability(
            Action::AddToQueue,
            BulkActionOwner::Provider(Provider::Spotify),
        ),
        capability(
            Action::AddToLiked,
            BulkActionOwner::Provider(Provider::Spotify),
        ),
        capability(
            Action::DeleteFromLiked,
            BulkActionOwner::Provider(Provider::Spotify),
        ),
        capability(
            Action::DeleteFromPlaylist,
            BulkActionOwner::Provider(Provider::Spotify),
        ),
        capability(Action::AddToListenLater, BulkActionOwner::Journal),
    ]
    .into_iter()
    .map(|capability| {
        if capability.action() == override_action {
            BulkActionCapability::new(override_action, override_owner)
        } else {
            capability
        }
    })
    .collect::<Vec<_>>();
    BulkActionItem::new(media_id.clone(), media_id, capabilities)
}

fn youtube_bulk_action_item(track: &YouTubeTrack) -> BulkActionItem<MediaId> {
    youtube_bulk_action_item_for_owner(track, Action::AddToQueue, BulkActionOwner::UnifiedQueue)
}

fn youtube_bulk_action_item_for_owner(
    track: &YouTubeTrack,
    override_action: Action,
    override_owner: BulkActionOwner,
) -> BulkActionItem<MediaId> {
    let media_id = youtube_media_id(track);
    let capabilities = [
        capability(Action::CopyLink, BulkActionOwner::Clipboard),
        capability(
            Action::AddToPlaylist,
            BulkActionOwner::Provider(Provider::YouTubeMusic),
        ),
        capability(Action::AddToQueue, BulkActionOwner::UnifiedQueue),
        capability(
            Action::AddToLiked,
            BulkActionOwner::Provider(Provider::YouTubeMusic),
        ),
        capability(
            Action::DeleteFromLiked,
            BulkActionOwner::Provider(Provider::YouTubeMusic),
        ),
    ]
    .into_iter()
    .map(|capability| {
        if capability.action() == override_action {
            BulkActionCapability::new(override_action, override_owner)
        } else {
            capability
        }
    })
    .collect::<Vec<_>>();
    BulkActionItem::new(media_id.clone(), media_id, capabilities)
}

fn capability(action: Action, owner: BulkActionOwner) -> BulkActionCapability {
    BulkActionCapability::new(action, owner)
}

fn spotify_media_id(track: &Track) -> MediaId {
    MediaId {
        provider: Provider::Spotify,
        kind: MediaKind::Track,
        raw_id: track.id.id().to_owned(),
    }
}

fn youtube_media_id(track: &YouTubeTrack) -> MediaId {
    MediaId {
        provider: Provider::YouTubeMusic,
        kind: if track.is_video {
            MediaKind::Video
        } else {
            MediaKind::Track
        },
        raw_id: track.id.clone(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::state::{
        Artist, PlayableMedia, QueueSelectionScope, TrackId, UnifiedPlaylistItem,
        UnifiedPlaylistSelectionScope, UnifiedQueue,
    };
    use std::time::Duration;

    fn track(id: &str) -> Track {
        Track {
            id: TrackId::from_id(id).expect("test track id").into_static(),
            name: id.to_owned(),
            artists: vec![Artist {
                id: crate::state::ArtistId::from_id("artist")
                    .expect("test artist id")
                    .into_static(),
                name: "artist".to_owned(),
            }],
            album: None,
            duration: Duration::from_secs(1),
            explicit: false,
            added_at: 0,
        }
    }

    fn youtube(id: &str) -> YouTubeTrack {
        YouTubeTrack {
            id: id.to_owned(),
            name: id.to_owned(),
            artists: "artist".to_owned(),
            album: None,
            duration: "0:01".to_owned(),
            explicit: false,
            thumbnail_url: None,
            is_video: false,
        }
    }

    fn unified_queue_item(id: &str, entry_id: u64) -> QueueActionItem {
        let track = youtube(id);
        let media = PlayableMedia::YouTube(track);
        let media_id = media.media_id();
        QueueActionItem::new(
            crate::state::OccurrenceDescriptor::with_token(media_id.clone(), entry_id),
            media_id,
            QueueActionPayload::Unified(media),
        )
    }

    fn native_queue_item(id: &str) -> QueueActionItem {
        let track_id = TrackId::from_id(id).expect("test track id").into_static();
        let media_id = MediaId {
            provider: Provider::Spotify,
            kind: MediaKind::Track,
            raw_id: id.to_owned(),
        };
        QueueActionItem::new(
            crate::state::OccurrenceDescriptor::unique(media_id.clone()),
            media_id,
            QueueActionPayload::Native(rspotify::model::PlayableId::Track(track_id)),
        )
    }

    fn unified_playlist_item(id: &str, provider: Provider) -> UnifiedPlaylistItem {
        UnifiedPlaylistItem {
            entry_id: crate::state::PlaylistEntryId(match provider {
                Provider::Spotify => 1,
                Provider::YouTubeMusic => 2,
            }),
            media_id: MediaId {
                provider,
                kind: MediaKind::Track,
                raw_id: id.to_owned(),
            },
            title: id.to_owned(),
            artists: "artist".to_owned(),
            duration_ms: Some(1_000),
            provider_url: None,
            ..UnifiedPlaylistItem::default()
        }
    }

    #[test]
    fn queue_menu_replans_exact_scope_and_owners() {
        let unified_scope = QueueSelectionScope::unified(UnifiedQueue::empty().instance_id());
        let unified_items = vec![unified_queue_item("one", 11), unified_queue_item("two", 12)];
        let unified_menu = QueueActionMenu::new(unified_scope.clone(), unified_items.clone());
        assert_eq!(
            unified_menu
                .actions()
                .iter()
                .map(|descriptor| descriptor.action)
                .collect::<Vec<_>>(),
            vec![Action::CopyLink, Action::AddToQueue]
        );
        let unified_plan = replan_queue_menu(&unified_menu, &unified_scope, &unified_items)
            .expect("unified queue plan");
        assert_eq!(unified_plan.operation_count(), 2);
        assert_eq!(
            unified_plan.partitions()[0].owner(),
            BulkActionOwner::UnifiedQueue
        );

        let native_scope = QueueSelectionScope::native_spotify(7);
        let native_items = vec![native_queue_item("one"), native_queue_item("two")];
        let native_menu = QueueActionMenu::new(native_scope.clone(), native_items.clone());
        let native_plan =
            replan_queue_menu(&native_menu, &native_scope, &native_items).expect("native plan");
        assert_eq!(native_plan.operation_count(), 2);
        assert_eq!(
            native_plan.partitions()[0].owner(),
            BulkActionOwner::Provider(Provider::Spotify)
        );

        assert_eq!(
            replan_queue_menu(
                &unified_menu,
                &QueueSelectionScope::unified(UnifiedQueue::empty().instance_id()),
                &unified_items,
            ),
            Err(BulkActionMenuError::ScopeMismatch)
        );
        let mut changed_items = unified_items.clone();
        changed_items[0] = unified_queue_item("changed", 11);
        assert_eq!(
            replan_queue_menu(&unified_menu, &unified_scope, &changed_items),
            Err(BulkActionMenuError::OccurrenceMismatch)
        );
    }

    #[test]
    fn unified_playlist_menu_replans_mixed_provider_batch_and_rejects_stale_scope() {
        let first = unified_playlist_item("spotify", Provider::Spotify);
        let second = unified_playlist_item("youtube", Provider::YouTubeMusic);
        let items = vec![
            UnifiedPlaylistActionItem::new(
                crate::state::OccurrenceDescriptor::with_token(
                    first.media_id.clone(),
                    first.entry_id,
                ),
                first.clone(),
            ),
            UnifiedPlaylistActionItem::new(
                crate::state::OccurrenceDescriptor::with_token(
                    second.media_id.clone(),
                    second.entry_id,
                ),
                second.clone(),
            ),
        ];
        let scope = UnifiedPlaylistSelectionScope::new("playlist".to_owned());
        let menu = UnifiedPlaylistActionMenu::new(scope.clone(), items.clone());
        assert_eq!(
            menu.actions()
                .iter()
                .map(|descriptor| descriptor.action)
                .collect::<Vec<_>>(),
            vec![
                Action::CopyLink,
                Action::AddToPlaylist,
                Action::AddToQueue,
                Action::DeleteFromPlaylist,
            ]
        );
        let plan = replan_unified_playlist_menu(Action::AddToQueue, &menu, &scope, &items)
            .expect("playlist plan");
        assert_eq!(plan.operation_count(), 2);
        assert_eq!(plan.partitions()[0].owner(), BulkActionOwner::UnifiedQueue);
        let delete_plan =
            replan_unified_playlist_menu(Action::DeleteFromPlaylist, &menu, &scope, &items)
                .expect("occurrence delete plan");
        assert_eq!(delete_plan.operation_count(), 2);
        assert_eq!(
            delete_plan.partitions()[0].owner(),
            BulkActionOwner::UnifiedPlaylist
        );
        assert_eq!(
            delete_plan
                .partitions()
                .iter()
                .flat_map(|partition| partition.operations())
                .flat_map(|operation| operation.occurrence_keys())
                .cloned()
                .collect::<Vec<_>>(),
            vec![items[0].occurrence().clone(), items[1].occurrence().clone()]
        );

        let mut hydrated_items = items.clone();
        let mut hydrated_first = first.clone();
        hydrated_first.title = "hydrated title".to_owned();
        hydrated_items[0] =
            UnifiedPlaylistActionItem::new(items[0].occurrence().clone(), hydrated_first);
        assert!(replan_unified_playlist_menu(
            Action::DeleteFromPlaylist,
            &menu,
            &scope,
            &hydrated_items,
        )
        .is_ok());

        assert_eq!(
            replan_unified_playlist_menu(
                Action::AddToQueue,
                &menu,
                &UnifiedPlaylistSelectionScope::new("other".to_owned()),
                &items,
            ),
            Err(BulkActionMenuError::ScopeMismatch)
        );
        let mut changed_items = items.clone();
        changed_items[0] = UnifiedPlaylistActionItem::new(
            crate::state::OccurrenceDescriptor::with_token(
                first.media_id.clone(),
                crate::state::PlaylistEntryId(99),
            ),
            unified_playlist_item("changed", Provider::Spotify),
        );
        assert_eq!(
            replan_unified_playlist_menu(Action::AddToQueue, &menu, &scope, &changed_items),
            Err(BulkActionMenuError::OccurrenceMismatch)
        );
    }

    #[test]
    fn spotify_routes_use_exact_owners() {
        let menu = spotify_bulk_action_menu(
            &[track("one")],
            &[
                Action::CopyLink,
                Action::AddToPlaylist,
                Action::AddToQueue,
                Action::AddToLiked,
                Action::AddToListenLater,
            ],
            3,
        )
        .expect("menu");
        let plan = replan_spotify_menu(
            &menu,
            BulkActionSelectionEpoch::new(Provider::Spotify, 3),
            Action::AddToListenLater,
        )
        .expect("plan");
        assert_eq!(plan.partitions()[0].owner(), BulkActionOwner::Journal);
        let queue_plan = replan_spotify_menu(
            &menu,
            BulkActionSelectionEpoch::new(Provider::Spotify, 3),
            Action::AddToQueue,
        )
        .expect("queue plan");
        assert_eq!(
            queue_plan.partitions()[0].owner(),
            BulkActionOwner::Provider(Provider::Spotify)
        );
    }

    #[test]
    fn youtube_routes_queue_to_shared_owner() {
        let menu =
            youtube_bulk_action_menu(&[youtube("one")], &[Action::AddToQueue], 4).expect("menu");
        let plan = replan_youtube_menu(
            &menu,
            BulkActionSelectionEpoch::new(Provider::YouTubeMusic, 4),
            Action::AddToQueue,
        )
        .expect("plan");
        assert_eq!(plan.partitions()[0].owner(), BulkActionOwner::UnifiedQueue);
    }

    #[test]
    fn youtube_liked_menu_can_plan_unlike() {
        let menu = youtube_bulk_action_menu(
            &[youtube("one"), youtube("two")],
            &[Action::DeleteFromLiked],
            4,
        )
        .expect("menu");
        assert!(menu
            .actions()
            .iter()
            .any(|descriptor| descriptor.action == Action::DeleteFromLiked));
        let plan = replan_youtube_menu(
            &menu,
            BulkActionSelectionEpoch::new(Provider::YouTubeMusic, 4),
            Action::DeleteFromLiked,
        )
        .expect("plan");
        assert_eq!(
            plan.partitions()[0].owner(),
            BulkActionOwner::Provider(Provider::YouTubeMusic)
        );
        assert_eq!(plan.partitions()[0].operations().len(), 2);
    }

    #[test]
    fn concrete_target_replans_override_only_the_requested_owner() {
        let spotify_menu = spotify_bulk_action_menu(
            &[track("one")],
            &[Action::AddToQueue, Action::AddToPlaylist],
            3,
        )
        .expect("menu");
        let queue_plan = replan_spotify_menu_for_owner(
            &spotify_menu,
            BulkActionSelectionEpoch::new(Provider::Spotify, 3),
            Action::AddToQueue,
            BulkActionOwner::UnifiedQueue,
        )
        .expect("queue plan");
        assert_eq!(
            queue_plan.partitions()[0].owner(),
            BulkActionOwner::UnifiedQueue
        );
        let playlist_plan = replan_spotify_menu_for_owner(
            &spotify_menu,
            BulkActionSelectionEpoch::new(Provider::Spotify, 3),
            Action::AddToPlaylist,
            BulkActionOwner::Provider(Provider::Spotify),
        )
        .expect("playlist plan");
        assert_eq!(
            playlist_plan.partitions()[0].owner(),
            BulkActionOwner::Provider(Provider::Spotify)
        );
        let unified_spotify_plan = replan_spotify_menu_for_owner(
            &spotify_menu,
            BulkActionSelectionEpoch::new(Provider::Spotify, 3),
            Action::AddToPlaylist,
            BulkActionOwner::UnifiedPlaylist,
        )
        .expect("Spotify tracks can target a unified playlist");
        assert_eq!(
            unified_spotify_plan.partitions()[0].owner(),
            BulkActionOwner::UnifiedPlaylist
        );

        let youtube_menu = youtube_bulk_action_menu(&[youtube("one")], &[Action::AddToPlaylist], 4)
            .expect("youtube menu");
        let unified_plan = replan_youtube_menu_for_owner(
            &youtube_menu,
            BulkActionSelectionEpoch::new(Provider::YouTubeMusic, 4),
            Action::AddToPlaylist,
            BulkActionOwner::UnifiedPlaylist,
        )
        .expect("unified playlist plan");
        assert_eq!(
            unified_plan.partitions()[0].owner(),
            BulkActionOwner::UnifiedPlaylist
        );
        let native_plan = replan_youtube_menu_for_owner(
            &youtube_menu,
            BulkActionSelectionEpoch::new(Provider::YouTubeMusic, 4),
            Action::AddToPlaylist,
            BulkActionOwner::Provider(Provider::YouTubeMusic),
        )
        .expect("native playlist plan");
        assert_eq!(
            native_plan.partitions()[0].owner(),
            BulkActionOwner::Provider(Provider::YouTubeMusic)
        );
    }

    #[test]
    fn bulk_assignments_require_exact_plan_coverage() {
        let plan = plan_spotify_tracks(
            &[track("one"), track("two")],
            BulkActionSelectionEpoch::new(Provider::Spotify, 3),
            Action::AddToQueue,
        )
        .expect("plan");
        let ids = plan.operation_ids();
        let request = ClientRequest::AddItemsToUserQueue(Vec::new());
        assert!(matches!(
            validate_bulk_assignments(&plan, &[]),
            Err(BulkAssignmentError::MissingOperation { .. })
        ));
        assert!(validate_bulk_assignments(
            &plan,
            &[BulkRequestAssignment::one(request.clone(), ids[0])]
        )
        .is_err());
        assert!(matches!(
            validate_bulk_assignments(
                &plan,
                &[
                    BulkRequestAssignment::one(request.clone(), ids[0]),
                    BulkRequestAssignment::one(request.clone(), ids[0]),
                ]
            ),
            Err(BulkAssignmentError::DuplicateOperation { .. })
        ));
        assert!(validate_bulk_assignments(
            &plan,
            &[
                BulkRequestAssignment::one(request.clone(), ids[0]),
                BulkRequestAssignment::one(request, ids[1]),
            ]
        )
        .is_ok());
    }

    #[test]
    fn bulk_assignments_reject_unsupported_request_contracts_before_dispatch() {
        let plan = plan_spotify_tracks(
            &[track("one")],
            BulkActionSelectionEpoch::new(Provider::Spotify, 3),
            Action::AddToQueue,
        )
        .expect("plan");
        let operation_id = plan.operation_ids()[0];

        assert_eq!(
            validate_bulk_assignments(
                &plan,
                &[BulkRequestAssignment::one(
                    ClientRequest::GetCurrentUser,
                    operation_id,
                )],
            ),
            Err(BulkAssignmentError::NotOrdered {
                assignment_index: 0,
            })
        );
        assert_eq!(
            validate_bulk_assignments(
                &plan,
                &[BulkRequestAssignment::one(
                    ClientRequest::ManageAccount(crate::client::AccountOperation::Validate(
                        ActiveProvider::Spotify,
                    )),
                    operation_id,
                )],
            ),
            Err(BulkAssignmentError::NotMutation {
                assignment_index: 0,
            })
        );
    }

    #[test]
    fn local_bulk_terminal_is_recorded_without_a_request() {
        let plan = plan_spotify_tracks(
            &[track("one"), track("two")],
            BulkActionSelectionEpoch::new(Provider::Spotify, 3),
            Action::CopyLink,
        )
        .expect("plan");
        let ids = plan.operation_ids();
        let mut ui = crate::state::UIState::default();
        let handle = start_bulk_local(&mut ui, &plan, &ids).expect("start");
        complete_bulk_local(&mut ui, &handle, &ids, true).expect("complete");
        assert!(ui.active_bulk_operation().is_none());
        let status = ui.operation_status.expect("status");
        assert_eq!(status.bulk_summary.expect("summary").succeeded, 2);
    }

    #[test]
    fn disconnected_bulk_send_rejects_every_unsent_operation() {
        let tracks = [track("one"), track("two")];
        let plan = plan_spotify_tracks(
            &tracks,
            BulkActionSelectionEpoch::new(Provider::Spotify, 3),
            Action::AddToQueue,
        )
        .expect("plan");
        let ids = plan.operation_ids();
        let assignments = ids
            .iter()
            .zip(tracks.iter())
            .map(|(operation_id, track)| {
                BulkRequestAssignment::one(
                    ClientRequest::AddPlayableToQueue(track.id.clone().into()),
                    *operation_id,
                )
            })
            .collect();
        let (sender, receiver) = crate::client::client_request_channel();
        drop(receiver);
        let mut ui = crate::state::UIState::default();
        assert!(dispatch_bulk_requests(&mut ui, &sender, &plan, assignments).is_err());
        assert!(ui.active_bulk_operation().is_none());
        let summary = ui
            .operation_status
            .expect("terminal status")
            .bulk_summary
            .expect("summary");
        assert_eq!(summary.failed, 2);
        assert_eq!(summary.pending, 0);
    }

    #[test]
    fn disconnected_queue_dispatch_preserves_page_selection() {
        let instance_id = UnifiedQueue::empty().instance_id();
        let scope = QueueSelectionScope::unified(instance_id.clone());
        let items = vec![unified_queue_item("one", 11), unified_queue_item("two", 12)];
        let menu = QueueActionMenu::new(scope.clone(), items.clone());
        let plan = replan_queue_menu(&menu, &scope, &items).expect("queue plan");

        let mut ui = crate::state::UIState::default();
        ui.history = vec![crate::state::PageState::new_queue()];
        let selection = ui
            .current_page_mut()
            .queue_selection_mut()
            .expect("queue selection");
        crate::state::synchronize_unified_queue_items(
            selection,
            instance_id,
            [
                (items[0].media_id().clone(), 11),
                (items[1].media_id().clone(), 12),
            ],
        )
        .expect("selection projection");
        selection.select_all_visible().expect("select rows");

        let before = selection.selected_visible_indices();
        let (sender, receiver) = crate::client::client_request_channel();
        drop(receiver);
        assert!(dispatch_queue_menu(&menu, &plan, &mut ui, &sender).is_err());
        assert_eq!(
            ui.current_page()
                .queue_selection()
                .expect("queue selection")
                .selected_visible_indices(),
            before
        );
    }

    #[test]
    fn youtube_copy_link_is_available_for_bulk_selection() {
        let menu =
            youtube_bulk_action_menu(&[youtube("one")], &[Action::CopyLink], 4).expect("menu");
        assert_eq!(menu.actions().len(), 1);
        assert_eq!(menu.actions()[0].action, Action::CopyLink);
        let plan = replan_youtube_menu(
            &menu,
            BulkActionSelectionEpoch::new(Provider::YouTubeMusic, 4),
            Action::CopyLink,
        )
        .expect("copy-link plan");
        assert_eq!(plan.operation_count(), 1);
    }

    #[test]
    fn duplicate_media_keys_are_rejected_without_an_ordinal_fallback() {
        let result =
            spotify_bulk_action_menu(&[track("same"), track("same")], &[Action::CopyLink], 3);
        assert!(matches!(
            result,
            Err(BulkActionMenuError::DuplicateOccurrenceKey { item_index: 1 })
        ));
    }

    #[test]
    fn stale_epoch_invalidates_the_snapshot() {
        let menu = spotify_bulk_action_menu(&[track("one")], &[Action::CopyLink], 3).expect("menu");
        assert!(matches!(
            replan_spotify_menu(
                &menu,
                BulkActionSelectionEpoch::new(Provider::Spotify, 4),
                Action::CopyLink,
            ),
            Err(BulkActionMenuError::EpochMismatch { .. })
        ));
    }
}
