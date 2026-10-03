use std::{
    collections::{HashMap, HashSet, VecDeque},
    panic::AssertUnwindSafe,
    time::Instant,
};

use futures::FutureExt as _;
use rspotify::prelude::Id as _;
use tokio::task::{Id, JoinSet};

use crate::command::BulkOperationId;
use crate::state::SharedState;

use super::request_router::RequestDisposition;
use super::{
    playback_coordinator::ActivationPermit, ClientRequest, RequestDelivery, RequestDomain,
    RequestKey,
};

const CLIENT_REQUEST_CAPACITY: usize = 64;
const CLIENT_PENDING_CAPACITY: usize = 64;
const CLIENT_MAX_IN_FLIGHT: usize = 8;

#[derive(Clone)]
pub(crate) struct ClientRequestSender {
    inner: flume::Sender<ContextualRequest>,
    source: crate::observability::OperationSource,
}

/// Correlation metadata carried only by an Ordered mutation envelope.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct BulkRequestCorrelation {
    reference: String,
    operation_ids: Vec<BulkOperationId>,
}

impl BulkRequestCorrelation {
    pub(crate) fn new(
        reference: &str,
        operation_ids: &[BulkOperationId],
    ) -> Result<Self, BulkCorrelationError> {
        if reference.is_empty()
            || reference.len() > 64
            || !reference
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_'))
        {
            return Err(BulkCorrelationError::InvalidReference);
        }
        if operation_ids.is_empty() {
            return Err(BulkCorrelationError::EmptyOperationIds);
        }
        let mut unique = Vec::with_capacity(operation_ids.len());
        for operation_id in operation_ids {
            if unique.contains(operation_id) {
                return Err(BulkCorrelationError::DuplicateOperationId {
                    operation_id: *operation_id,
                });
            }
            unique.push(*operation_id);
        }
        Ok(Self {
            reference: reference.to_owned(),
            operation_ids: unique,
        })
    }

    pub(crate) fn reference(&self) -> &str {
        &self.reference
    }

    pub(crate) fn operation_ids(&self) -> &[BulkOperationId] {
        &self.operation_ids
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum BulkCorrelationError {
    InvalidReference,
    EmptyOperationIds,
    DuplicateOperationId { operation_id: BulkOperationId },
}

#[allow(dead_code)] // Recovery payloads are consumed by bulk callers on send failure.
#[derive(Debug)]
pub(crate) enum BulkSendError {
    Invalid {
        request: ClientRequest,
        reason: BulkCorrelationError,
    },
    NotOrdered {
        request: ClientRequest,
    },
    NotMutation {
        request: ClientRequest,
    },
    Disconnected {
        request: ClientRequest,
    },
}

impl std::fmt::Display for BulkSendError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let message = match self {
            Self::Invalid { .. } => "invalid bulk request correlation",
            Self::NotOrdered { .. } => "bulk request is not ordered",
            Self::NotMutation { .. } => "bulk request is not a mutation",
            Self::Disconnected { .. } => "bulk request channel is disconnected",
        };
        formatter.write_str(message)
    }
}

impl std::error::Error for BulkSendError {}

impl ClientRequestSender {
    pub(crate) fn with_source(&self, source: crate::observability::OperationSource) -> Self {
        Self {
            inner: self.inner.clone(),
            source,
        }
    }

    #[allow(clippy::result_large_err)] // Preserve flume's unsent-request recovery contract.
    pub(crate) fn send(
        &self,
        request: ClientRequest,
    ) -> Result<(), flume::SendError<ClientRequest>> {
        self.inner
            .send(ContextualRequest::new(request, self.source))
            .map_err(|error| flume::SendError(error.0.request))
    }

    /// Send an Ordered request carrying plan-local bulk correlation.  The
    /// provider request payload and delivery policy remain unchanged.
    pub(crate) fn send_bulk(
        &self,
        request: ClientRequest,
        reference: &str,
        operation_ids: &[BulkOperationId],
    ) -> Result<(), BulkSendError> {
        if request.delivery_policy() != RequestDelivery::Ordered {
            return Err(BulkSendError::NotOrdered { request });
        }
        if request.domain() != RequestDomain::PlaylistMutation {
            return Err(BulkSendError::NotMutation { request });
        }
        let correlation = match BulkRequestCorrelation::new(reference, operation_ids) {
            Ok(correlation) => correlation,
            Err(reason) => return Err(BulkSendError::Invalid { request, reason }),
        };
        self.inner
            .send(ContextualRequest::with_bulk(
                request,
                self.source,
                correlation,
            ))
            .map_err(|error| BulkSendError::Disconnected {
                request: error.0.request,
            })
    }

    pub(crate) async fn send_async(
        &self,
        request: ClientRequest,
    ) -> Result<(), flume::SendError<ClientRequest>> {
        self.inner
            .send_async(ContextualRequest::new(request, self.source))
            .await
            .map_err(|error| flume::SendError(error.0.request))
    }

    #[cfg(test)]
    fn try_send(&self, request: ClientRequest) -> Result<(), flume::TrySendError<ClientRequest>> {
        self.inner
            .try_send(ContextualRequest::new(request, self.source))
            .map_err(|error| match error {
                flume::TrySendError::Full(envelope) => flume::TrySendError::Full(envelope.request),
                flume::TrySendError::Disconnected(envelope) => {
                    flume::TrySendError::Disconnected(envelope.request)
                }
            })
    }
}

pub(crate) struct ContextualRequest {
    request: ClientRequest,
    context: crate::observability::OperationContext,
    enqueued_at: Instant,
    bulk: Option<BulkRequestCorrelation>,
}

impl ContextualRequest {
    fn new(request: ClientRequest, source: crate::observability::OperationSource) -> Self {
        let mut context = crate::observability::child_or_new(request.operation_name(), source);
        // A provider switch names its target explicitly; do not let a parent
        // operation's provider label make the switch look like the old one.
        if matches!(&request, ClientRequest::SwitchProvider(_)) || context.provider.is_none() {
            context.provider = request.provider();
        }
        Self {
            request,
            context,
            enqueued_at: Instant::now(),
            bulk: None,
        }
    }

    fn with_bulk(
        request: ClientRequest,
        source: crate::observability::OperationSource,
        bulk: BulkRequestCorrelation,
    ) -> Self {
        let mut contextual = Self::new(request, source);
        contextual.bulk = Some(bulk);
        contextual
    }

    #[allow(dead_code)]
    pub(crate) fn bulk(&self) -> Option<&BulkRequestCorrelation> {
        self.bulk.as_ref()
    }

    pub(crate) fn request(&self) -> &ClientRequest {
        &self.request
    }
}

impl From<ClientRequest> for ContextualRequest {
    fn from(request: ClientRequest) -> Self {
        Self::new(request, crate::observability::OperationSource::System)
    }
}

pub(crate) fn channel() -> (ClientRequestSender, flume::Receiver<ContextualRequest>) {
    let (sender, receiver) = flume::bounded(CLIENT_REQUEST_CAPACITY);
    (
        ClientRequestSender {
            inner: sender,
            source: crate::observability::OperationSource::System,
        },
        receiver,
    )
}

struct TaskMetadata {
    delivery: RequestDelivery,
    context: crate::observability::OperationContext,
    started_at: Instant,
    ui_request: Option<UiRequestDescriptor>,
    bulk: Option<BulkRequestCorrelation>,
}

#[derive(Clone, Debug)]
enum UiRequestDescriptor {
    Search {
        provider: crate::config::ActiveProvider,
        query: String,
        lifecycle_reference: String,
    },
    YouTubeLibrary,
    Lyrics {
        track_uri: String,
        source: Option<String>,
    },
    ListenBrainzArtistEnrichment {
        context_uri: String,
        request_id: u64,
    },
    ListenBrainzAlbumResolution {
        context_uri: String,
        release_group_mbid: String,
        request_id: u64,
    },
    ListenBrainzPlaylistBackup {
        playlist_id: String,
        operation_reference: String,
    },
    ActivePlaybackControl,
    UnifiedQueueNext,
    Playback,
    Mutation,
    MutationTracked {
        reference: String,
    },
}

fn ui_request_descriptor(request: &ClientRequest) -> Option<UiRequestDescriptor> {
    if let ClientRequest::BackupUnifiedPlaylistToListenBrainz {
        unified_playlist_id,
        operation_reference,
    } = request
    {
        return Some(UiRequestDescriptor::ListenBrainzPlaylistBackup {
            playlist_id: unified_playlist_id.clone(),
            operation_reference: operation_reference.clone(),
        });
    }
    if request.domain() == RequestDomain::PlaylistMutation {
        return Some(UiRequestDescriptor::Mutation);
    }
    match request {
        ClientRequest::GetYouTubeLibrary => Some(UiRequestDescriptor::YouTubeLibrary),
        ClientRequest::EnrichListenBrainzArtist {
            context_uri,
            request_id,
            ..
        } => Some(UiRequestDescriptor::ListenBrainzArtistEnrichment {
            context_uri: context_uri.clone(),
            request_id: *request_id,
        }),
        ClientRequest::ResolveListenBrainzAlbum {
            context_uri,
            release_group_mbid,
            request_id,
            ..
        } => Some(UiRequestDescriptor::ListenBrainzAlbumResolution {
            context_uri: context_uri.clone(),
            release_group_mbid: release_group_mbid.clone(),
            request_id: *request_id,
        }),
        ClientRequest::GetLyrics { track_id } => Some(UiRequestDescriptor::Lyrics {
            track_uri: track_id.uri(),
            source: None,
        }),
        ClientRequest::GetLyricsFromProvider { track_id, provider } => {
            Some(UiRequestDescriptor::Lyrics {
                track_uri: track_id.uri(),
                source: Some(provider.clone()),
            })
        }
        ClientRequest::GetYouTubeLyrics(track) => Some(UiRequestDescriptor::Lyrics {
            track_uri: format!("youtube:{}", track.id),
            source: None,
        }),
        ClientRequest::GetYouTubeLyricsFromProvider { track, provider } => {
            Some(UiRequestDescriptor::Lyrics {
                track_uri: format!("youtube:{}", track.id),
                source: Some(provider.clone()),
            })
        }
        ClientRequest::Search {
            query,
            lifecycle_reference,
        } => Some(UiRequestDescriptor::Search {
            provider: crate::config::ActiveProvider::Spotify,
            query: query.clone(),
            lifecycle_reference: lifecycle_reference.clone(),
        }),
        ClientRequest::SearchYouTube {
            query,
            lifecycle_reference,
        } => Some(UiRequestDescriptor::Search {
            provider: crate::config::ActiveProvider::YouTubeMusic,
            query: query.clone(),
            lifecycle_reference: lifecycle_reference.clone(),
        }),
        ClientRequest::ActivePlaybackControl(_) | ClientRequest::ActivePlaybackSeek(_) => {
            Some(UiRequestDescriptor::ActivePlaybackControl)
        }
        ClientRequest::UnifiedNext => Some(UiRequestDescriptor::UnifiedQueueNext),
        ClientRequest::SwitchProvider(_)
        | ClientRequest::PlayYouTubeContext { .. }
        | ClientRequest::PlayUnifiedItems { .. }
        | ClientRequest::UnifiedPrevious
        | ClientRequest::Player(_)
        | ClientRequest::YouTubePlayer(_) => Some(UiRequestDescriptor::Playback),
        _ => None,
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum RequestTaskOutcome {
    Success,
    Rejected(Option<super::request::PlaybackControlRejection>),
    Failure,
    Cancelled,
    Superseded,
    Panicked,
}

fn bulk_terminal_for_outcome(outcome: RequestTaskOutcome) -> crate::command::BulkOperationTerminal {
    match outcome {
        RequestTaskOutcome::Success => crate::command::BulkOperationTerminal::Succeeded,
        // Bulk requests are playlist mutations and cannot currently return a
        // playback no-op. Keep this terminal mapping defensive if that changes.
        RequestTaskOutcome::Rejected(_) | RequestTaskOutcome::Failure => {
            crate::command::BulkOperationTerminal::Failed(
                crate::command::BulkOperationFailure::RequestFailed,
            )
        }
        RequestTaskOutcome::Cancelled => crate::command::BulkOperationTerminal::Cancelled,
        RequestTaskOutcome::Superseded => crate::command::BulkOperationTerminal::Superseded,
        RequestTaskOutcome::Panicked => crate::command::BulkOperationTerminal::Failed(
            crate::command::BulkOperationFailure::Internal,
        ),
    }
}

fn request_failure_diagnostic(
    error: &anyhow::Error,
) -> (
    crate::observability::DiagnosticCode,
    crate::observability::ErrorCategory,
) {
    crate::observability::preserved_error_diagnostic(error).unwrap_or((
        crate::observability::DiagnosticCode::REQUEST_HANDLE_FAILED,
        crate::observability::ErrorCategory::Unavailable,
    ))
}

fn record_request_failure(error: &anyhow::Error) {
    let (code, category) = request_failure_diagnostic(error);
    crate::observability::log_safe_error!(
        error,
        code,
        category,
        error,
        "Failed to handle a client request"
    );
}

fn classify_request_result(
    result: Result<RequestDisposition, anyhow::Error>,
) -> RequestTaskOutcome {
    match result {
        Ok(RequestDisposition::Applied) => RequestTaskOutcome::Success,
        Ok(RequestDisposition::NoOp) => RequestTaskOutcome::Rejected(None),
        Ok(RequestDisposition::Rejected(reason)) => RequestTaskOutcome::Rejected(Some(reason)),
        Ok(RequestDisposition::Superseded) => RequestTaskOutcome::Superseded,
        Err(error) => {
            record_request_failure(&error);
            RequestTaskOutcome::Failure
        }
    }
}

fn unified_queue_completion_is_current(
    activation_generation: u64,
    queue: Option<&crate::state::UnifiedQueue>,
    completion: &super::request::UnifiedQueueCompletion,
) -> bool {
    activation_generation == completion.activation_generation
        && queue.is_some_and(|queue| queue.completion_is_current(&completion.queue))
}

fn should_reserve_playback_activation(
    request: &ClientRequest,
    switch_target_is_active: bool,
) -> bool {
    request.reserves_playback_activation()
        && !(matches!(request, ClientRequest::SwitchProvider(_)) && switch_target_is_active)
}

fn enqueue_received_request(
    state: &SharedState,
    client: &super::AppClient,
    scheduler: &mut RequestScheduler,
    envelope: ContextualRequest,
) {
    crate::observability::request_accepted(&envelope.context, scheduler.pending.len());
    let switch_target_is_active = match &envelope.request {
        ClientRequest::SwitchProvider(provider) => client.playback.accepts_control(*provider),
        _ => false,
    };
    let activation =
        if should_reserve_playback_activation(&envelope.request, switch_target_is_active) {
            let stale_completion =
                envelope
                    .request
                    .unified_queue_completion()
                    .is_some_and(|completion| {
                        let player = state.player.read();
                        !unified_queue_completion_is_current(
                            client.playback.activation_generation(),
                            player.unified_queue.as_ref(),
                            completion,
                        )
                    });
            if stale_completion {
                crate::observability::request_completed(
                    &envelope.context,
                    crate::observability::OperationOutcome::Rejected,
                    envelope.enqueued_at.elapsed(),
                    Some("stale_playback_completion"),
                );
                return;
            }
            Some(client.reserve_playback_activation())
        } else {
            None
        };
    let outcome = enqueue_with_search_supersession(state, scheduler, envelope, activation);
    if outcome != EnqueueOutcome::Accepted {
        tracing::trace!(?outcome, "Applied client request ingress policy");
    }
}

fn enqueue_with_search_supersession(
    state: &SharedState,
    scheduler: &mut RequestScheduler,
    envelope: ContextualRequest,
    activation: Option<ActivationPermit>,
) -> EnqueueOutcome {
    let displaced_search = scheduler.pending_search_replacement(&envelope.request);
    let outcome = scheduler.enqueue(envelope, activation);
    if outcome == EnqueueOutcome::Replaced {
        if let Some(request) = displaced_search.as_ref() {
            apply_ui_request_outcome(
                state,
                Some(request),
                crate::observability::OperationOutcome::Superseded,
                None,
            );
        }
    }
    outcome
}

pub(crate) async fn start_client_handler(
    state: &SharedState,
    client: &super::AppClient,
    client_sub: flume::Receiver<ContextualRequest>,
    shutdown: tokio_util::sync::CancellationToken,
) {
    let mut scheduler = RequestScheduler::default();
    let mut tasks = JoinSet::new();
    let mut task_metadata = HashMap::<Id, TaskMetadata>::new();
    let mut input_closed = false;
    let playback_completion_sub = client.playback_completion_rx.clone();

    loop {
        while let Some(ScheduledRequest {
            request,
            context,
            enqueued_at,
            activation,
            delivery,
            bulk,
        }) = scheduler.pop_ready()
        {
            crate::observability::request_started(
                &context,
                enqueued_at.elapsed(),
                scheduler.pending.len(),
            );
            if request.is_shutdown() {
                let started_at = Instant::now();
                let result = Box::pin(crate::observability::in_operation(context.clone(), async {
                    let result = client.handle_request(state, request, activation).await;
                    if let Err(err) = &result {
                        crate::observability::log_safe_error!(
                            error,
                            crate::observability::DiagnosticCode::REQUEST_SHUTDOWN_FAILED,
                            crate::observability::ErrorCategory::Unavailable,
                            err,
                            "Failed to handle the client shutdown request"
                        );
                    }
                    result
                }))
                .await;
                scheduler.finish(delivery);
                let outcome = match result {
                    Ok(RequestDisposition::Applied) => {
                        crate::observability::OperationOutcome::Success
                    }
                    Ok(RequestDisposition::NoOp | RequestDisposition::Rejected(_)) => {
                        crate::observability::OperationOutcome::Rejected
                    }
                    Ok(RequestDisposition::Superseded) => {
                        crate::observability::OperationOutcome::Superseded
                    }
                    Err(_) => crate::observability::OperationOutcome::Error,
                };
                crate::observability::request_completed(
                    &context,
                    outcome,
                    started_at.elapsed(),
                    match outcome {
                        crate::observability::OperationOutcome::Rejected => Some("no_op"),
                        crate::observability::OperationOutcome::Superseded => {
                            Some("activation_superseded")
                        }
                        _ => None,
                    },
                );
                while let Some(result) = tasks.join_next_with_id().await {
                    complete_task(result, &mut task_metadata, &mut scheduler, Some(state));
                }
                return;
            }

            let state = state.clone();
            let client = client.clone();
            let shutdown = shutdown.clone();
            let operation = context.clone();
            let activation_probe = activation.clone();
            let ui_request = if bulk.is_some() {
                // Bulk operations own their lifecycle and count summary. A
                // generic mutation footer must not overwrite that tracker.
                None
            } else {
                match ui_request_descriptor(&request) {
                    Some(UiRequestDescriptor::Mutation) => {
                        let reference = {
                            let mut ui = state.ui.lock();
                            ui.start_operation(
                                crate::state::UiOperationKind::ProviderCommand,
                                crate::state::MUTATION_RUNNING_CODE,
                                crate::state::MUTATION_RUNNING_MESSAGE,
                            )
                        };
                        Some(UiRequestDescriptor::MutationTracked { reference })
                    }
                    descriptor => descriptor,
                }
            };
            let abort = tasks.spawn(async move {
                let future = async {
                    let handle = Box::pin(client.handle_request(&state, request, activation));
                    let result = match delivery {
                        RequestDelivery::LatestWins(_) | RequestDelivery::Coalescible(_) => {
                            tokio::select! {
                                biased;
                                () = shutdown.cancelled() => return RequestTaskOutcome::Cancelled,
                                result = handle => result,
                            }
                        }
                        RequestDelivery::MustDeliver | RequestDelivery::Ordered => handle.await,
                    };
                    if activation_probe
                        .as_ref()
                        .is_some_and(|permit| !client.playback.activation_permit_is_current(permit))
                    {
                        return RequestTaskOutcome::Superseded;
                    }
                    classify_request_result(result)
                };
                match AssertUnwindSafe(crate::observability::in_operation(operation, future))
                    .catch_unwind()
                    .await
                {
                    Ok(outcome) => outcome,
                    Err(_) => RequestTaskOutcome::Panicked,
                }
            });
            task_metadata.insert(
                abort.id(),
                TaskMetadata {
                    delivery,
                    context,
                    started_at: Instant::now(),
                    ui_request,
                    bulk,
                },
            );
        }

        if input_closed && scheduler.is_idle() && tasks.is_empty() {
            return;
        }

        tokio::select! {
            completion = tasks.join_next_with_id(), if !tasks.is_empty() => {
                complete_task(
                    completion.expect("request task set is non-empty"),
                    &mut task_metadata,
                    &mut scheduler,
                    Some(state),
                );
            }
            received = client_sub.recv_async(), if !input_closed && scheduler.can_receive() => {
                match received {
                    Ok(envelope) => enqueue_received_request(state, client, &mut scheduler, envelope),
                    Err(_) => input_closed = true,
                }
            }
            received = playback_completion_sub.recv_async(), if !input_closed && scheduler.can_receive() => {
                if let Ok(completion) = received {
                    enqueue_received_request(
                        state,
                        client,
                        &mut scheduler,
                        ContextualRequest::new(
                            ClientRequest::ContinueUnifiedQueue(completion),
                            crate::observability::OperationSource::Runtime,
                        ),
                    );
                }
            }
        }
    }
}

fn complete_task(
    result: Result<(Id, RequestTaskOutcome), tokio::task::JoinError>,
    metadata: &mut HashMap<Id, TaskMetadata>,
    scheduler: &mut RequestScheduler,
    state: Option<&SharedState>,
) {
    let id = match &result {
        Ok((id, _)) => *id,
        Err(error) => error.id(),
    };
    let task = metadata
        .remove(&id)
        .expect("owned request task has lifecycle metadata");
    scheduler.finish(task.delivery);
    let (outcome, bulk_terminal, rejection) = match result {
        Ok((_, RequestTaskOutcome::Success)) => (
            crate::observability::OperationOutcome::Success,
            bulk_terminal_for_outcome(RequestTaskOutcome::Success),
            None,
        ),
        Ok((_, RequestTaskOutcome::Rejected(reason))) => (
            crate::observability::OperationOutcome::Rejected,
            bulk_terminal_for_outcome(RequestTaskOutcome::Rejected(reason)),
            reason,
        ),
        Ok((_, RequestTaskOutcome::Failure)) => (
            crate::observability::OperationOutcome::Error,
            bulk_terminal_for_outcome(RequestTaskOutcome::Failure),
            None,
        ),
        Ok((_, RequestTaskOutcome::Cancelled)) => (
            crate::observability::OperationOutcome::Cancelled,
            bulk_terminal_for_outcome(RequestTaskOutcome::Cancelled),
            None,
        ),
        Ok((_, RequestTaskOutcome::Superseded)) => (
            crate::observability::OperationOutcome::Superseded,
            bulk_terminal_for_outcome(RequestTaskOutcome::Superseded),
            None,
        ),
        Err(error) if error.is_cancelled() => (
            crate::observability::OperationOutcome::Aborted,
            bulk_terminal_for_outcome(RequestTaskOutcome::Cancelled),
            None,
        ),
        Ok((_, RequestTaskOutcome::Panicked)) | Err(_) => (
            crate::observability::OperationOutcome::Panicked,
            bulk_terminal_for_outcome(RequestTaskOutcome::Panicked),
            None,
        ),
    };
    let reason = rejection
        .map(|reason| reason.diagnostic_reason())
        .or(match outcome {
            crate::observability::OperationOutcome::Cancelled => Some("shutdown"),
            crate::observability::OperationOutcome::Superseded => Some("activation_superseded"),
            crate::observability::OperationOutcome::Aborted => Some("task_abort"),
            _ => None,
        });
    if let Some(state) = state {
        apply_ui_request_outcome(state, task.ui_request.as_ref(), outcome, rejection);
        if let Some(bulk) = task.bulk.as_ref() {
            let mut ui = state.ui.lock();
            if let Err(error) =
                ui.record_bulk_terminal(bulk.reference(), bulk.operation_ids(), bulk_terminal)
            {
                tracing::debug!(?error, "Rejected bulk request terminal");
            }
        }
    }
    crate::observability::request_completed(
        &task.context,
        outcome,
        task.started_at.elapsed(),
        reason,
    );
}

fn apply_ui_request_outcome(
    state: &SharedState,
    request: Option<&UiRequestDescriptor>,
    outcome: crate::observability::OperationOutcome,
    rejection: Option<super::request::PlaybackControlRejection>,
) {
    let Some(request) = request else {
        return;
    };
    if let UiRequestDescriptor::ListenBrainzAlbumResolution {
        context_uri,
        release_group_mbid,
        request_id,
    } = request
    {
        if outcome != crate::observability::OperationOutcome::Success {
            super::provider_read::terminalize_listenbrainz_album_resolution_if_current(
                state,
                context_uri,
                release_group_mbid,
                *request_id,
            );
        }
        return;
    }
    if let UiRequestDescriptor::ListenBrainzArtistEnrichment {
        context_uri,
        request_id,
    } = request
    {
        if outcome != crate::observability::OperationOutcome::Success {
            super::provider_read::terminalize_listenbrainz_artist_enrichment_if_current(
                state,
                context_uri,
                *request_id,
            );
        }
        return;
    }
    if let UiRequestDescriptor::ListenBrainzPlaylistBackup {
        playlist_id,
        operation_reference,
    } = request
    {
        if outcome == crate::observability::OperationOutcome::Cancelled
            || outcome == crate::observability::OperationOutcome::Aborted
        {
            state
                .ui
                .lock()
                .finish_listenbrainz_backup_cancelled(playlist_id, operation_reference);
        } else if outcome != crate::observability::OperationOutcome::Success {
            state
                .ui
                .lock()
                .finish_listenbrainz_backup_failed(playlist_id, operation_reference);
        }
        return;
    }
    if matches!(request, UiRequestDescriptor::YouTubeLibrary) {
        // The provider adapter normally writes a loaded response (or its
        // failure state) itself. This fallback closes the lifecycle when a
        // task exits before that write, including cancellation, a panic, or
        // an activation supersession during the Spotify -> YouTube handoff.
        super::provider_read::terminalize_youtube_library_if_unloaded(
            &mut state.data.write().user_data.youtube_library,
        );
        return;
    }
    if let UiRequestDescriptor::Lyrics { track_uri, source } = request {
        let status = match outcome {
            crate::observability::OperationOutcome::Success => {
                let data = state.data.read();
                let cache_key = crate::state::LyricsCacheKey::new(track_uri, source.as_deref());
                match data.caches.lyrics.get(&cache_key) {
                    Some(Some(_)) => crate::state::UiViewStatus::Ready,
                    Some(None) | None => crate::state::UiViewStatus::Empty,
                }
            }
            crate::observability::OperationOutcome::Superseded => {
                crate::state::UiViewStatus::Superseded {
                    code: crate::state::LYRICS_SUPERSEDED_CODE,
                    message: crate::state::LYRICS_SUPERSEDED_MESSAGE,
                    next_action: crate::state::LYRICS_SUPERSEDED_NEXT_ACTION,
                }
            }
            crate::observability::OperationOutcome::Error
            | crate::observability::OperationOutcome::Panicked
            | crate::observability::OperationOutcome::Cancelled
            | crate::observability::OperationOutcome::Aborted => {
                crate::state::UiViewStatus::Failed {
                    code: crate::state::LYRICS_FAILURE_CODE,
                    message: crate::state::LYRICS_FAILURE_MESSAGE,
                    next_action: crate::state::LYRICS_FAILURE_NEXT_ACTION,
                }
            }
            _ => return,
        };
        state
            .ui
            .lock()
            .set_lyrics_status(track_uri, source.as_deref(), status);
        return;
    }
    let mut ui = state.ui.lock();
    apply_ui_request_outcome_to_ui(&mut ui, request, outcome, rejection);
}

fn apply_ui_request_outcome_to_ui(
    ui: &mut crate::state::UIState,
    request: &UiRequestDescriptor,
    outcome: crate::observability::OperationOutcome,
    rejection: Option<super::request::PlaybackControlRejection>,
) {
    use super::request::PlaybackControlRejection;

    match (request, outcome) {
        (
            UiRequestDescriptor::ActivePlaybackControl,
            crate::observability::OperationOutcome::Rejected,
        ) => match rejection {
            Some(PlaybackControlRejection::NoActivePlayback) => ui.set_operation_failure(
                crate::state::UiOperationKind::Playback,
                "PLAYBACK_CONTROL_NO_ACTIVE_PLAYBACK",
                "No active playback.",
                "Start a track, then try the control again.",
            ),
            Some(PlaybackControlRejection::TransitionInProgress) => ui.set_operation_failure(
                crate::state::UiOperationKind::Playback,
                "PLAYBACK_CONTROL_TRANSITION_IN_PROGRESS",
                "Playback is switching providers.",
                "Wait for the switch to finish, then try again.",
            ),
            Some(PlaybackControlRejection::SeekUnavailable) => ui.set_operation_failure(
                crate::state::UiOperationKind::Playback,
                "PLAYBACK_SEEK_UNAVAILABLE",
                "Seeking is not available for the active track right now.",
                "Wait for playback to become ready, then try again.",
            ),
            Some(PlaybackControlRejection::ShuttingDown) => ui.set_operation_failure(
                crate::state::UiOperationKind::Playback,
                "PLAYBACK_CONTROL_SHUTTING_DOWN",
                "Playback is shutting down.",
                "Start the application again before using playback controls.",
            ),
            None => {}
        },
        (
            UiRequestDescriptor::Search {
                provider,
                query,
                lifecycle_reference,
            },
            crate::observability::OperationOutcome::Error
            | crate::observability::OperationOutcome::Panicked
            | crate::observability::OperationOutcome::Cancelled
            | crate::observability::OperationOutcome::Aborted,
        ) => ui.finish_search_failure(*provider, query, lifecycle_reference),
        (
            UiRequestDescriptor::Search {
                provider,
                query,
                lifecycle_reference,
            },
            crate::observability::OperationOutcome::Superseded,
        ) => ui.finish_search_superseded(*provider, query, lifecycle_reference),
        (
            UiRequestDescriptor::Playback,
            crate::observability::OperationOutcome::Error
            | crate::observability::OperationOutcome::Panicked,
        ) => {
            ui.set_playback_failure();
        }
        (
            UiRequestDescriptor::UnifiedQueueNext,
            crate::observability::OperationOutcome::Rejected,
        ) => {
            let reference = ui.start_operation(
                crate::state::UiOperationKind::Playback,
                crate::state::UNIFIED_QUEUE_END_CODE,
                crate::state::UNIFIED_QUEUE_END_MESSAGE,
            );
            ui.complete_operation(
                &reference,
                crate::state::UiOperationState::Completed,
                crate::state::UNIFIED_QUEUE_END_CODE,
                crate::state::UNIFIED_QUEUE_END_MESSAGE,
                Some(crate::state::UNIFIED_QUEUE_END_NEXT_ACTION),
            );
        }
        (
            UiRequestDescriptor::MutationTracked { reference },
            crate::observability::OperationOutcome::Success,
        ) => ui.complete_operation(
            reference,
            crate::state::UiOperationState::Completed,
            crate::state::MUTATION_COMPLETED_CODE,
            crate::state::MUTATION_COMPLETED_MESSAGE,
            None,
        ),
        (
            UiRequestDescriptor::MutationTracked { reference },
            crate::observability::OperationOutcome::Error
            | crate::observability::OperationOutcome::Panicked,
        ) => ui.complete_operation(
            reference,
            crate::state::UiOperationState::Failed,
            crate::state::MUTATION_FAILURE_CODE,
            crate::state::MUTATION_FAILURE_MESSAGE,
            Some(crate::state::MUTATION_FAILURE_NEXT_ACTION),
        ),
        (
            UiRequestDescriptor::MutationTracked { reference },
            crate::observability::OperationOutcome::Cancelled
            | crate::observability::OperationOutcome::Aborted,
        ) => ui.complete_operation(
            reference,
            crate::state::UiOperationState::Cancelled,
            crate::state::MUTATION_CANCELLED_CODE,
            crate::state::MUTATION_CANCELLED_MESSAGE,
            None,
        ),
        (
            UiRequestDescriptor::MutationTracked { reference },
            crate::observability::OperationOutcome::Superseded,
        ) => ui.complete_operation(
            reference,
            crate::state::UiOperationState::Superseded,
            crate::state::MUTATION_SUPERSEDED_CODE,
            crate::state::MUTATION_SUPERSEDED_MESSAGE,
            None,
        ),
        _ => {}
    }
}

struct ScheduledRequest {
    request: ClientRequest,
    context: crate::observability::OperationContext,
    enqueued_at: Instant,
    activation: Option<ActivationPermit>,
    delivery: RequestDelivery,
    bulk: Option<BulkRequestCorrelation>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum EnqueueOutcome {
    Accepted,
    Replaced,
    Coalesced,
}

struct RequestScheduler {
    pending: VecDeque<ScheduledRequest>,
    active_count: usize,
    ordered_active: bool,
    account_change_active: bool,
    active_keys: HashSet<RequestKey>,
    shutdown_received: bool,
    pending_capacity: usize,
    max_in_flight: usize,
}

impl Default for RequestScheduler {
    fn default() -> Self {
        Self::new(CLIENT_PENDING_CAPACITY, CLIENT_MAX_IN_FLIGHT)
    }
}

impl RequestScheduler {
    fn new(pending_capacity: usize, max_in_flight: usize) -> Self {
        assert!(pending_capacity > 0);
        assert!(max_in_flight > 0);
        Self {
            pending: VecDeque::with_capacity(pending_capacity),
            active_count: 0,
            ordered_active: false,
            account_change_active: false,
            active_keys: HashSet::new(),
            shutdown_received: false,
            pending_capacity,
            max_in_flight,
        }
    }

    fn can_receive(&self) -> bool {
        !self.shutdown_received && self.pending.len() < self.pending_capacity
    }

    fn is_idle(&self) -> bool {
        self.active_count == 0 && self.pending.is_empty()
    }

    fn pending_search_replacement(&self, request: &ClientRequest) -> Option<UiRequestDescriptor> {
        let key = request.delivery_policy().key()?;
        self.pending
            .iter()
            .find(|pending| pending.delivery.key() == Some(key))
            .and_then(|pending| ui_request_descriptor(&pending.request))
            .filter(|descriptor| matches!(descriptor, UiRequestDescriptor::Search { .. }))
    }

    fn enqueue<R: Into<ContextualRequest>>(
        &mut self,
        request: R,
        activation: Option<ActivationPermit>,
    ) -> EnqueueOutcome {
        let ContextualRequest {
            request,
            context,
            enqueued_at,
            bulk,
        } = request.into();
        let delivery = request.delivery_policy();
        if request.is_shutdown() {
            self.shutdown_received = true;
            // Shutdown is terminal. Preserve accepted ordered mutations and
            // controls, but do not let replaceable, duplicate, or independent
            // pending work delay process cleanup.
            let mut preserved = VecDeque::with_capacity(self.pending.len());
            while let Some(pending) = self.pending.pop_front() {
                if pending.delivery == RequestDelivery::Ordered {
                    preserved.push_back(pending);
                } else {
                    debug_assert!(
                        pending.bulk.is_none(),
                        "bulk correlation must never use a replaceable request"
                    );
                    crate::observability::request_completed(
                        &pending.context,
                        crate::observability::OperationOutcome::Cancelled,
                        pending.enqueued_at.elapsed(),
                        Some("shutdown"),
                    );
                }
            }
            self.pending = preserved;
            self.pending.push_back(ScheduledRequest {
                request,
                context,
                enqueued_at,
                activation,
                delivery,
                bulk,
            });
            return EnqueueOutcome::Accepted;
        }

        match delivery {
            RequestDelivery::Coalescible(key)
                if self.active_keys.contains(&key)
                    || self
                        .pending
                        .iter()
                        .any(|pending| pending.delivery.key() == Some(key)) =>
            {
                debug_assert!(
                    bulk.is_none(),
                    "bulk correlation must never use a coalescible request"
                );
                crate::observability::request_completed(
                    &context,
                    crate::observability::OperationOutcome::Superseded,
                    enqueued_at.elapsed(),
                    Some("duplicate_request"),
                );
                EnqueueOutcome::Coalesced
            }
            RequestDelivery::LatestWins(key) => {
                let outcome = self
                    .pending
                    .iter()
                    .position(|pending| pending.delivery.key() == Some(key))
                    .map_or(EnqueueOutcome::Accepted, |position| {
                        let displaced = self
                            .pending
                            .remove(position)
                            .expect("located pending request exists");
                        debug_assert!(
                            displaced.bulk.is_none() && bulk.is_none(),
                            "bulk correlation must never use a replaceable request"
                        );
                        crate::observability::request_completed(
                            &displaced.context,
                            crate::observability::OperationOutcome::Superseded,
                            displaced.enqueued_at.elapsed(),
                            Some("newer_request"),
                        );
                        EnqueueOutcome::Replaced
                    });
                self.pending.push_back(ScheduledRequest {
                    request,
                    context,
                    enqueued_at,
                    activation,
                    delivery,
                    bulk,
                });
                outcome
            }
            RequestDelivery::MustDeliver | RequestDelivery::Coalescible(_) => {
                debug_assert!(
                    bulk.is_none(),
                    "bulk correlation must only use Ordered delivery"
                );
                self.pending.push_back(ScheduledRequest {
                    request,
                    context,
                    enqueued_at,
                    activation,
                    delivery,
                    bulk,
                });
                EnqueueOutcome::Accepted
            }
            RequestDelivery::Ordered => {
                self.pending.push_back(ScheduledRequest {
                    request,
                    context,
                    enqueued_at,
                    activation,
                    delivery,
                    bulk,
                });
                EnqueueOutcome::Accepted
            }
        }
    }

    fn pop_ready(&mut self) -> Option<ScheduledRequest> {
        if self.account_change_active {
            return None;
        }
        if self
            .pending
            .front()
            .is_some_and(|pending| pending.request.is_shutdown())
        {
            if self.ordered_active {
                return None;
            }
            let request = self.pending.pop_front()?;
            self.active_count += 1;
            return Some(request);
        }
        if self.active_count >= self.max_in_flight {
            return None;
        }

        let account_change_index = self
            .pending
            .iter()
            .position(|pending| pending.request.is_account_change());
        if account_change_index.is_some_and(|_| self.active_count > 0) {
            return None;
        }

        let mut ready_index = None;
        for (index, pending) in self.pending.iter().enumerate() {
            if account_change_index.is_some_and(|account_index| index > account_index) {
                break;
            }
            if pending.request.is_shutdown() {
                break;
            }
            let ready = match pending.delivery {
                RequestDelivery::MustDeliver => true,
                RequestDelivery::Ordered => !self.ordered_active,
                RequestDelivery::LatestWins(key) | RequestDelivery::Coalescible(key) => {
                    !self.active_keys.contains(&key)
                }
            };
            if ready {
                ready_index = Some(index);
                break;
            }
        }

        let request = self.pending.remove(ready_index?)?;
        self.active_count += 1;
        if request.request.is_account_change() {
            self.account_change_active = true;
        }
        match request.delivery {
            RequestDelivery::Ordered => self.ordered_active = true,
            RequestDelivery::LatestWins(key) | RequestDelivery::Coalescible(key) => {
                self.active_keys.insert(key);
            }
            RequestDelivery::MustDeliver => {}
        }
        Some(request)
    }

    fn finish(&mut self, delivery: RequestDelivery) {
        self.active_count = self
            .active_count
            .checked_sub(1)
            .expect("completed client request was not active");
        match delivery {
            RequestDelivery::Ordered => self.ordered_active = false,
            RequestDelivery::LatestWins(key) | RequestDelivery::Coalescible(key) => {
                assert!(self.active_keys.remove(&key));
            }
            RequestDelivery::MustDeliver => {}
        }
        if self.account_change_active && delivery == RequestDelivery::Ordered {
            assert!(self.account_change_active);
            self.account_change_active = false;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        client::{
            playback_coordinator::PlaybackCoordinator, request::UnifiedQueueCompletion,
            AccountOperation, PlayerRequest, YouTubePlayerRequest,
        },
        config::ActiveProvider,
        state::{PlayableMedia, UnifiedQueue, YouTubeTrack},
    };

    fn scheduler() -> RequestScheduler {
        RequestScheduler::new(4, 2)
    }

    fn spotify_search(query: &str) -> ClientRequest {
        spotify_search_with_reference(query, "test-search")
    }

    fn spotify_search_with_reference(query: &str, lifecycle_reference: &str) -> ClientRequest {
        ClientRequest::Search {
            query: query.to_owned(),
            lifecycle_reference: lifecycle_reference.to_owned(),
        }
    }

    fn youtube_media(id: &str) -> PlayableMedia {
        PlayableMedia::YouTube(YouTubeTrack {
            id: id.to_owned(),
            name: id.to_owned(),
            artists: "artist".to_owned(),
            album: None,
            duration: "1:00".to_owned(),
            explicit: false,
            thumbnail_url: None,
            is_video: false,
        })
    }

    fn listenbrainz_resolution(request_id: u64) -> ClientRequest {
        ClientRequest::ResolveListenBrainzRecording {
            request_id,
            context_uri: "spotify:artist:artist".to_owned(),
            recording_mbid: format!("recording-{request_id}"),
            intent: crate::state::ListenBrainzRecordingIntent::OpenMenu,
        }
    }

    fn listenbrainz_album_resolution(request_id: u64) -> ClientRequest {
        ClientRequest::ResolveListenBrainzAlbum {
            request_id,
            context_uri: "spotify:artist:artist".to_owned(),
            release_group_mbid: format!("release-group-{request_id}"),
            intent: crate::state::ListenBrainzAlbumIntent::OpenPage,
        }
    }

    #[test]
    fn listenbrainz_resolution_keeps_only_the_latest_pending_intent() {
        let mut scheduler = scheduler();
        let first = ContextualRequest::new(
            listenbrainz_resolution(1),
            crate::observability::OperationSource::Terminal,
        );
        assert_eq!(scheduler.enqueue(first, None), EnqueueOutcome::Accepted);
        let active = scheduler.pop_ready().unwrap();

        let second = ContextualRequest::new(
            listenbrainz_resolution(2),
            crate::observability::OperationSource::Terminal,
        );
        let third = ContextualRequest::new(
            listenbrainz_resolution(3),
            crate::observability::OperationSource::Terminal,
        );
        assert_eq!(scheduler.enqueue(second, None), EnqueueOutcome::Accepted);
        assert_eq!(scheduler.enqueue(third, None), EnqueueOutcome::Replaced);
        assert!(scheduler.pop_ready().is_none());

        scheduler.finish(active.delivery);
        let latest = scheduler.pop_ready().unwrap();
        assert!(matches!(
            latest.request,
            ClientRequest::ResolveListenBrainzRecording { request_id: 3, .. }
        ));
    }

    #[test]
    fn album_and_recording_resolution_have_independent_latest_wins_keys() {
        let mut scheduler = scheduler();
        for request in [listenbrainz_resolution(1), listenbrainz_album_resolution(2)] {
            assert_eq!(
                scheduler.enqueue(
                    ContextualRequest::new(
                        request,
                        crate::observability::OperationSource::Terminal,
                    ),
                    None,
                ),
                EnqueueOutcome::Accepted
            );
        }

        let first = scheduler.pop_ready().expect("recording resolution");
        let second = scheduler.pop_ready().expect("album resolution");
        assert_ne!(first.delivery.key(), second.delivery.key());
    }

    #[test]
    fn completion_must_match_activation_and_queue_occurrence_before_reservation() {
        let media = youtube_media("same");
        let mut queue = UnifiedQueue::new(vec![media.clone(), media.clone()], 0);
        let completion = UnifiedQueueCompletion {
            source: ActiveProvider::YouTubeMusic,
            queue: queue.completion_token_for(&media).unwrap(),
            activation_generation: 7,
        };
        let envelope = ContextualRequest::new(
            ClientRequest::ContinueUnifiedQueue(completion.clone()),
            crate::observability::OperationSource::Runtime,
        );

        assert_eq!(envelope.context.operation, "unified_queue_continue");
        assert_eq!(
            envelope.context.source,
            crate::observability::OperationSource::Runtime
        );
        assert_eq!(envelope.context.provider, None);
        assert_eq!(
            envelope.request.delivery_policy(),
            RequestDelivery::LatestWins(RequestKey::PlaybackSelection)
        );
        assert!(envelope.request.reserves_playback_activation());

        assert!(unified_queue_completion_is_current(
            7,
            Some(&queue),
            &completion
        ));
        assert!(!unified_queue_completion_is_current(
            8,
            Some(&queue),
            &completion
        ));

        queue.advance_after_completion(&completion.queue).unwrap();
        assert!(!unified_queue_completion_is_current(
            7,
            Some(&queue),
            &completion
        ));
        assert!(!unified_queue_completion_is_current(7, None, &completion));
    }

    #[test]
    fn switching_to_the_active_provider_does_not_invalidate_completion_evidence() {
        let switch = ClientRequest::SwitchProvider(ActiveProvider::YouTubeMusic);

        assert!(!should_reserve_playback_activation(&switch, true));
        assert!(should_reserve_playback_activation(&switch, false));
        assert!(should_reserve_playback_activation(
            &ClientRequest::UnifiedNext,
            true
        ));
    }

    #[tokio::test]
    async fn provider_switch_context_labels_the_target_provider() {
        let mut parent = crate::observability::OperationContext::new(
            "parent_operation",
            crate::observability::OperationSource::Terminal,
        );
        parent.provider = Some(crate::observability::ProviderKind::Spotify);

        crate::observability::in_operation(parent, async {
            let request = ContextualRequest::new(
                ClientRequest::SwitchProvider(ActiveProvider::YouTubeMusic),
                crate::observability::OperationSource::Terminal,
            );
            assert_eq!(
                request.context.provider,
                Some(crate::observability::ProviderKind::YoutubeMusic)
            );
        })
        .await;
    }

    #[test]
    fn bulk_terminal_mapping_is_lossless_and_privacy_safe() {
        assert_eq!(
            bulk_terminal_for_outcome(RequestTaskOutcome::Success),
            crate::command::BulkOperationTerminal::Succeeded
        );
        assert_eq!(
            bulk_terminal_for_outcome(RequestTaskOutcome::Rejected(None)),
            crate::command::BulkOperationTerminal::Failed(
                crate::command::BulkOperationFailure::RequestFailed
            )
        );
        assert_eq!(
            bulk_terminal_for_outcome(RequestTaskOutcome::Failure),
            crate::command::BulkOperationTerminal::Failed(
                crate::command::BulkOperationFailure::RequestFailed
            )
        );
        assert_eq!(
            bulk_terminal_for_outcome(RequestTaskOutcome::Cancelled),
            crate::command::BulkOperationTerminal::Cancelled
        );
        assert_eq!(
            bulk_terminal_for_outcome(RequestTaskOutcome::Superseded),
            crate::command::BulkOperationTerminal::Superseded
        );
        assert_eq!(
            bulk_terminal_for_outcome(RequestTaskOutcome::Panicked),
            crate::command::BulkOperationTerminal::Failed(
                crate::command::BulkOperationFailure::Internal
            )
        );
    }

    #[test]
    fn no_op_request_is_rejected_instead_of_reported_successfully() {
        assert!(matches!(
            classify_request_result(Ok(RequestDisposition::NoOp)),
            RequestTaskOutcome::Rejected(None)
        ));
        assert!(matches!(
            classify_request_result(Ok(RequestDisposition::Applied)),
            RequestTaskOutcome::Success
        ));
        assert!(matches!(
            classify_request_result(Ok(RequestDisposition::Superseded)),
            RequestTaskOutcome::Superseded
        ));
        assert_eq!(
            classify_request_result(Ok(RequestDisposition::Rejected(
                crate::client::PlaybackControlRejection::NoActivePlayback,
            ))),
            RequestTaskOutcome::Rejected(Some(
                crate::client::PlaybackControlRejection::NoActivePlayback
            ))
        );
    }

    #[test]
    fn bulk_correlation_requires_safe_reference_and_unique_ids() {
        let id = BulkOperationId::from_index(1);
        assert_eq!(
            BulkRequestCorrelation::new("", &[id]),
            Err(BulkCorrelationError::InvalidReference)
        );
        assert_eq!(
            BulkRequestCorrelation::new("ui-0001", &[]),
            Err(BulkCorrelationError::EmptyOperationIds)
        );
        assert_eq!(
            BulkRequestCorrelation::new("ui-0001", &[id, id]),
            Err(BulkCorrelationError::DuplicateOperationId { operation_id: id })
        );
        let correlation = BulkRequestCorrelation::new("ui-0001", &[id]).expect("correlation");
        assert_eq!(correlation.reference(), "ui-0001");
        assert_eq!(correlation.operation_ids(), &[id]);
    }

    #[test]
    fn send_bulk_rejects_non_ordered_and_preserves_ordered_metadata() {
        let (sender, receiver) = channel();
        let id = BulkOperationId::from_index(2);
        assert!(matches!(
            sender.send_bulk(spotify_search("private-query"), "ui-0001", &[id],),
            Err(BulkSendError::NotOrdered { .. })
        ));
        assert!(matches!(
            sender.send_bulk(
                ClientRequest::Player(PlayerRequest::Pause),
                "ui-0001",
                &[id],
            ),
            Err(BulkSendError::NotMutation { .. })
        ));
        sender
            .send_bulk(
                ClientRequest::AddItemsToUserQueue(Vec::new()),
                "ui-0001",
                &[id],
            )
            .expect("ordered bulk send");
        let envelope = receiver.recv().expect("envelope");
        assert_eq!(envelope.request.delivery_policy(), RequestDelivery::Ordered);
        assert_eq!(
            envelope.bulk().expect("bulk metadata").reference(),
            "ui-0001"
        );
        assert_eq!(
            envelope.bulk().expect("bulk metadata").operation_ids(),
            &[id]
        );
    }

    fn enqueue(scheduler: &mut RequestScheduler, request: ClientRequest) -> EnqueueOutcome {
        scheduler.enqueue(request, None)
    }

    #[test]
    fn request_failure_uses_preserved_provider_diagnostic_before_generic_fallback() {
        let provider_error = crate::observability::preserve_error_diagnostic(
            anyhow::anyhow!("private provider response text"),
            crate::observability::DiagnosticCode::YOUTUBE_PLAYBACK_VALIDATION_FAILED,
            crate::observability::ErrorCategory::ProofToken,
        )
        .context("generic playback coordinator context");
        assert_eq!(
            request_failure_diagnostic(&provider_error),
            (
                crate::observability::DiagnosticCode::YOUTUBE_PLAYBACK_VALIDATION_FAILED,
                crate::observability::ErrorCategory::ProofToken,
            )
        );

        assert_eq!(
            request_failure_diagnostic(&anyhow::anyhow!("ordinary request failure")),
            (
                crate::observability::DiagnosticCode::REQUEST_HANDLE_FAILED,
                crate::observability::ErrorCategory::Unavailable,
            )
        );
    }

    #[test]
    fn mutation_requests_receive_ui_failure_correlation() {
        assert!(matches!(
            ui_request_descriptor(&ClientRequest::AddItemsToUserQueue(Vec::new())),
            Some(UiRequestDescriptor::Mutation)
        ));
        assert!(matches!(
            ui_request_descriptor(&spotify_search("query")),
            Some(UiRequestDescriptor::Search { .. })
        ));
        assert!(matches!(
            ui_request_descriptor(&ClientRequest::GetYouTubeLibrary),
            Some(UiRequestDescriptor::YouTubeLibrary)
        ));
        assert!(matches!(
            ui_request_descriptor(&ClientRequest::Player(PlayerRequest::Pause)),
            Some(UiRequestDescriptor::Playback)
        ));
        assert!(matches!(
            ui_request_descriptor(&ClientRequest::UnifiedNext),
            Some(UiRequestDescriptor::UnifiedQueueNext)
        ));
    }

    #[test]
    fn lyrics_completion_requires_the_visible_source_identity() {
        crate::ui::initialize_test_config();
        let ring = std::sync::Arc::new(parking_lot::Mutex::new(std::collections::VecDeque::new()));
        let (diagnostics, _runtime) = crate::observability::disabled(ring);
        let state = std::sync::Arc::new(crate::state::State::new(false, diagnostics));
        let track_id = rspotify::model::TrackId::from_id("4iV5W9uYEdYUVa79Axb7Rh")
            .unwrap()
            .into_static();
        let track_uri = track_id.uri();
        {
            let mut ui = state.ui.lock();
            ui.new_page(crate::state::PageState::Lyrics {
                provider: ActiveProvider::Spotify,
                track_uri: track_uri.clone(),
                track: "Track".to_owned(),
                artists: "Artist".to_owned(),
                youtube_track: None,
                lyrics_provider: Some("lyricsovh".to_owned()),
                scroll_offset: 0,
                follow_playback: true,
                status: crate::state::UiViewStatus::Loading,
            });
        }

        let old_request = ClientRequest::GetLyricsFromProvider {
            track_id: track_id.clone_static(),
            provider: "lrclib".to_owned(),
        };
        let old_descriptor = ui_request_descriptor(&old_request).unwrap();
        state.data.write().caches.lyrics.insert(
            crate::state::LyricsCacheKey::new(&track_uri, Some("lrclib")),
            crate::state::Lyrics::from_plain("old source", "LRCLIB"),
            *crate::state::TTL_CACHE_DURATION,
        );
        apply_ui_request_outcome(
            &state,
            Some(&old_descriptor),
            crate::observability::OperationOutcome::Success,
            None,
        );
        assert!(matches!(
            state.ui.lock().current_page(),
            crate::state::PageState::Lyrics {
                status: crate::state::UiViewStatus::Loading,
                ..
            }
        ));

        let current_request = ClientRequest::GetLyricsFromProvider {
            track_id,
            provider: "lyricsovh".to_owned(),
        };
        let current_descriptor = ui_request_descriptor(&current_request).unwrap();
        state.data.write().caches.lyrics.insert(
            crate::state::LyricsCacheKey::new(&track_uri, Some("lyricsovh")),
            crate::state::Lyrics::from_plain("current source", "Lyrics.ovh"),
            *crate::state::TTL_CACHE_DURATION,
        );
        apply_ui_request_outcome(
            &state,
            Some(&current_descriptor),
            crate::observability::OperationOutcome::Success,
            None,
        );
        assert!(matches!(
            state.ui.lock().current_page(),
            crate::state::PageState::Lyrics {
                status: crate::state::UiViewStatus::Ready,
                ..
            }
        ));
    }

    #[test]
    fn listenbrainz_artist_request_failure_closes_only_its_loading_state() {
        let configs = crate::ui::initialize_test_config();
        let ring = std::sync::Arc::new(parking_lot::Mutex::new(std::collections::VecDeque::new()));
        let (diagnostics, _runtime) = crate::observability::disabled(ring);
        let state = std::sync::Arc::new(crate::state::State::new_with_configs(
            false,
            diagnostics,
            configs,
        ));
        let context_uri = "spotify:artist:artist";
        state.data.write().caches.context.insert(
            context_uri.to_owned(),
            crate::state::Context::Artist {
                artist: crate::state::Artist {
                    id: rspotify::model::ArtistId::from_id("artist").unwrap(),
                    name: "Artist".to_owned(),
                },
                top_tracks: Vec::new(),
                listenbrainz: crate::state::ListenBrainzArtistEnrichment::Loading { request_id: 9 },
                albums: Vec::new(),
                related_artists: Vec::new(),
            },
            *crate::state::TTL_CACHE_DURATION,
        );
        let descriptor = ui_request_descriptor(&ClientRequest::EnrichListenBrainzArtist {
            request_id: 9,
            context_uri: context_uri.to_owned(),
            spotify_artist_id: "artist".to_owned(),
            include_recordings: true,
            include_release_groups: true,
        })
        .expect("artist enrichment should own lifecycle cleanup");

        apply_ui_request_outcome(
            &state,
            Some(&descriptor),
            crate::observability::OperationOutcome::Cancelled,
            None,
        );

        assert!(matches!(
            state.data.read().caches.context.get(context_uri),
            Some(crate::state::Context::Artist {
                listenbrainz: crate::state::ListenBrainzArtistEnrichment::Unavailable,
                ..
            })
        ));
    }

    #[test]
    fn listenbrainz_album_request_cancellation_closes_cache_and_pending_intent() {
        let configs = crate::ui::initialize_test_config();
        let ring = std::sync::Arc::new(parking_lot::Mutex::new(std::collections::VecDeque::new()));
        let (diagnostics, _runtime) = crate::observability::disabled(ring);
        let state = std::sync::Arc::new(crate::state::State::new_with_configs(
            false,
            diagnostics,
            configs,
        ));
        let context_uri = "spotify:artist:artist";
        let request_id = 12;
        state.data.write().caches.listenbrainz_albums.insert(
            crate::state::listenbrainz_album_key("release-group"),
            crate::state::ListenBrainzAlbumResolution::Resolving { request_id },
            *crate::state::TTL_CACHE_DURATION,
        );
        let mut page_state = crate::state::ContextPageUIState::new_artist();
        let crate::state::ContextPageUIState::Artist {
            focus,
            album_table,
            listenbrainz_album_pending,
            ..
        } = &mut page_state
        else {
            unreachable!()
        };
        *focus = crate::state::ArtistFocusState::Albums;
        album_table.select(Some(0));
        *listenbrainz_album_pending = Some(crate::state::ListenBrainzAlbumPendingIntent {
            request_id,
            context_uri: context_uri.to_owned(),
            release_group_mbid: "release-group".to_owned(),
            intent: crate::state::ListenBrainzAlbumIntent::OpenPage,
        });
        state
            .ui
            .lock()
            .history
            .push(crate::state::PageState::Context {
                id: Some(crate::state::ContextId::Artist(
                    rspotify::model::ArtistId::from_id("artist").unwrap(),
                )),
                context_page_type: crate::state::ContextPageType::Browsing(
                    crate::state::ContextId::Artist(
                        rspotify::model::ArtistId::from_id("artist").unwrap(),
                    ),
                ),
                state: Some(page_state),
            });
        let descriptor = ui_request_descriptor(&ClientRequest::ResolveListenBrainzAlbum {
            request_id,
            context_uri: context_uri.to_owned(),
            release_group_mbid: "release-group".to_owned(),
            intent: crate::state::ListenBrainzAlbumIntent::OpenPage,
        })
        .expect("album resolution should own lifecycle cleanup");

        apply_ui_request_outcome(
            &state,
            Some(&descriptor),
            crate::observability::OperationOutcome::Cancelled,
            None,
        );

        assert!(matches!(
            state
                .data
                .read()
                .caches
                .listenbrainz_albums
                .get(&crate::state::listenbrainz_album_key("release-group")),
            Some(crate::state::ListenBrainzAlbumResolution::Unavailable)
        ));
        let ui = state.ui.lock();
        assert!(matches!(
            ui.current_page(),
            crate::state::PageState::Context {
                state: Some(crate::state::ContextPageUIState::Artist {
                    listenbrainz_album_pending: None,
                    ..
                }),
                ..
            }
        ));
    }

    #[test]
    fn youtube_library_request_outcomes_close_unresolved_loading_state() {
        let configs = crate::ui::initialize_test_config();
        let ring = std::sync::Arc::new(parking_lot::Mutex::new(std::collections::VecDeque::new()));
        let (diagnostics, _runtime) = crate::observability::disabled(ring);
        let state = std::sync::Arc::new(crate::state::State::new_with_configs(
            false,
            diagnostics,
            configs,
        ));
        let descriptor = UiRequestDescriptor::YouTubeLibrary;

        for outcome in [
            crate::observability::OperationOutcome::Success,
            crate::observability::OperationOutcome::Error,
            crate::observability::OperationOutcome::Cancelled,
            crate::observability::OperationOutcome::Superseded,
            crate::observability::OperationOutcome::Panicked,
            crate::observability::OperationOutcome::Aborted,
        ] {
            state.data.write().user_data.youtube_library = Default::default();
            apply_ui_request_outcome(&state, Some(&descriptor), outcome, None);

            let library = state.data.read().user_data.youtube_library.clone();
            assert!(library.loaded, "{outcome:?} left the library loading");
            assert_eq!(
                library.errors,
                vec![crate::state::YOUTUBE_LIBRARY_ERROR_MESSAGE.to_owned()]
            );
        }

        state.data.write().user_data.youtube_library = crate::state::YouTubeLibrary {
            loaded: true,
            errors: vec!["partial but usable".to_owned()],
            ..Default::default()
        };
        apply_ui_request_outcome(
            &state,
            Some(&descriptor),
            crate::observability::OperationOutcome::Error,
            None,
        );
        assert_eq!(
            state.data.read().user_data.youtube_library.errors,
            vec!["partial but usable"]
        );
    }

    #[test]
    fn mutation_request_outcomes_complete_the_same_ui_reference() {
        let mut ui = crate::state::UIState::default();
        let reference = ui.start_operation(
            crate::state::UiOperationKind::ProviderCommand,
            crate::state::MUTATION_RUNNING_CODE,
            crate::state::MUTATION_RUNNING_MESSAGE,
        );
        let descriptor = UiRequestDescriptor::MutationTracked {
            reference: reference.clone(),
        };

        apply_ui_request_outcome_to_ui(
            &mut ui,
            &descriptor,
            crate::observability::OperationOutcome::Success,
            None,
        );

        let status = ui.operation_status.as_ref().expect("mutation status");
        assert_eq!(status.reference, reference);
        assert_eq!(status.state, crate::state::UiOperationState::Completed);
        assert_eq!(status.code, crate::state::MUTATION_COMPLETED_CODE);

        let failure_reference = ui.start_operation(
            crate::state::UiOperationKind::ProviderCommand,
            crate::state::MUTATION_RUNNING_CODE,
            crate::state::MUTATION_RUNNING_MESSAGE,
        );
        let failure_descriptor = UiRequestDescriptor::MutationTracked {
            reference: failure_reference.clone(),
        };
        apply_ui_request_outcome_to_ui(
            &mut ui,
            &failure_descriptor,
            crate::observability::OperationOutcome::Error,
            None,
        );
        let status = ui
            .operation_status
            .as_ref()
            .expect("failed mutation status");
        assert_eq!(status.reference, failure_reference);
        assert_eq!(status.state, crate::state::UiOperationState::Failed);
        assert_eq!(status.code, crate::state::MUTATION_FAILURE_CODE);
        assert!(status.next_action.is_some());
    }

    #[test]
    fn listenbrainz_backup_cancellation_closes_lifecycle_without_overwriting_partial_result() {
        let configs = crate::ui::initialize_test_config();
        let ring = std::sync::Arc::new(parking_lot::Mutex::new(std::collections::VecDeque::new()));
        let (diagnostics, _runtime) = crate::observability::disabled(ring);
        let state = std::sync::Arc::new(crate::state::State::new_with_configs(
            false,
            diagnostics,
            configs,
        ));

        let reference = state
            .ui
            .lock()
            .start_listenbrainz_backup("playlist")
            .expect("backup starts");
        let descriptor =
            ui_request_descriptor(&ClientRequest::BackupUnifiedPlaylistToListenBrainz {
                unified_playlist_id: "playlist".to_owned(),
                operation_reference: reference.clone(),
            })
            .expect("backup owns a UI descriptor");
        apply_ui_request_outcome(
            &state,
            Some(&descriptor),
            crate::observability::OperationOutcome::Cancelled,
            None,
        );
        assert_eq!(
            state
                .ui
                .lock()
                .operation_status
                .as_ref()
                .map(|status| status.state),
            Some(crate::state::UiOperationState::Cancelled)
        );

        let reference = state
            .ui
            .lock()
            .start_listenbrainz_backup("playlist")
            .expect("retry starts");
        state.ui.lock().finish_listenbrainz_backup_partial(
            "playlist",
            &reference,
            "9ef9f54c-1d4b-4ac4-8b6e-a127c77021f1",
        );
        let descriptor = UiRequestDescriptor::ListenBrainzPlaylistBackup {
            playlist_id: "playlist".to_owned(),
            operation_reference: reference,
        };
        apply_ui_request_outcome(
            &state,
            Some(&descriptor),
            crate::observability::OperationOutcome::Error,
            None,
        );
        assert_eq!(
            state
                .ui
                .lock()
                .operation_status
                .as_ref()
                .map(|status| status.state),
            Some(crate::state::UiOperationState::Partial)
        );
    }

    #[test]
    fn active_playback_control_rejections_are_visible_and_specific() {
        let cases = [
            (
                crate::client::PlaybackControlRejection::NoActivePlayback,
                "PLAYBACK_CONTROL_NO_ACTIVE_PLAYBACK",
            ),
            (
                crate::client::PlaybackControlRejection::TransitionInProgress,
                "PLAYBACK_CONTROL_TRANSITION_IN_PROGRESS",
            ),
            (
                crate::client::PlaybackControlRejection::SeekUnavailable,
                "PLAYBACK_SEEK_UNAVAILABLE",
            ),
            (
                crate::client::PlaybackControlRejection::ShuttingDown,
                "PLAYBACK_CONTROL_SHUTTING_DOWN",
            ),
        ];

        for (rejection, code) in cases {
            let mut ui = crate::state::UIState::default();
            apply_ui_request_outcome_to_ui(
                &mut ui,
                &UiRequestDescriptor::ActivePlaybackControl,
                crate::observability::OperationOutcome::Rejected,
                Some(rejection),
            );

            let status = ui.operation_status.as_ref().expect("control status");
            assert_eq!(status.kind, crate::state::UiOperationKind::Playback);
            assert_eq!(status.state, crate::state::UiOperationState::Failed);
            assert_eq!(status.code, code);
            assert!(status.next_action.is_some());
        }
    }

    #[test]
    fn unified_queue_end_noop_is_visible_without_becoming_a_failure() {
        let mut ui = crate::state::UIState::default();

        apply_ui_request_outcome_to_ui(
            &mut ui,
            &UiRequestDescriptor::UnifiedQueueNext,
            crate::observability::OperationOutcome::Rejected,
            None,
        );

        let status = ui.operation_status.as_ref().expect("queue boundary status");
        assert_eq!(status.kind, crate::state::UiOperationKind::Playback);
        assert_eq!(status.state, crate::state::UiOperationState::Completed);
        assert_eq!(status.code, crate::state::UNIFIED_QUEUE_END_CODE);
        assert_eq!(status.message, "End of queue reached.");
        assert!(status.next_action.is_some());
        assert!(status.expires_at.is_some());
        assert!(status.ordinary_display_line().contains("End of queue"));
    }

    #[test]
    fn cancelled_search_outcomes_close_the_loading_lifecycle() {
        for outcome in [
            crate::observability::OperationOutcome::Cancelled,
            crate::observability::OperationOutcome::Aborted,
        ] {
            let mut ui = crate::state::UIState::default();
            ui.new_page(crate::state::PageState::Search {
                line_input: crate::ui::single_line_input::LineInput::default(),
                current_query: String::new(),
                state: crate::state::SearchPageUIState::new(),
            });
            let lifecycle_reference =
                ui.begin_search(crate::config::ActiveProvider::Spotify, "private query");
            let descriptor = UiRequestDescriptor::Search {
                provider: crate::config::ActiveProvider::Spotify,
                query: "private query".to_owned(),
                lifecycle_reference,
            };

            apply_ui_request_outcome_to_ui(&mut ui, &descriptor, outcome, None);

            assert!(matches!(
                ui.current_page(),
                crate::state::PageState::Search {
                    state: crate::state::SearchPageUIState {
                        search_lifecycle: crate::state::SearchLifecycle::Failed { .. },
                        ..
                    },
                    ..
                }
            ));
        }
    }

    #[tokio::test]
    async fn provider_failure_event_is_correlated_and_omits_raw_provider_text() {
        use tracing_subscriber::layer::SubscriberExt as _;

        const PRIVATE_PROVIDER_TEXT: &str = "provider body video-id=private-id token=private-token";
        let ring = std::sync::Arc::new(parking_lot::Mutex::new(VecDeque::new()));
        let (handle, _runtime) = crate::observability::disabled(ring.clone());
        let subscriber =
            tracing_subscriber::registry().with(crate::observability::DiagnosticLayer::new(handle));
        let context = crate::observability::OperationContext::new(
            "play_youtube_context",
            crate::observability::OperationSource::Terminal,
        );
        let reference = context.short_reference().to_owned();
        let provider_error = crate::observability::preserve_error_diagnostic(
            anyhow::anyhow!(PRIVATE_PROVIDER_TEXT),
            crate::observability::DiagnosticCode::YOUTUBE_PLAYBACK_VALIDATION_FAILED,
            crate::observability::ErrorCategory::ProviderUnavailable,
        )
        .context("generic playback coordinator context");

        crate::observability::in_operation(context, async {
            tracing::subscriber::with_default(subscriber, || {
                record_request_failure(&provider_error);
            });
        })
        .await;

        let entries = ring.lock().iter().cloned().collect::<Vec<_>>();
        let failures = entries
            .iter()
            .filter(|entry| entry.code == "YOUTUBE_PLAYBACK_VALIDATION_FAILED")
            .collect::<Vec<_>>();
        assert_eq!(failures.len(), 1);
        assert_eq!(failures[0].reference.as_deref(), Some(reference.as_str()));
        assert_eq!(
            failures[0].error_type.as_deref(),
            Some("provider_unavailable")
        );
        let rendered = failures[0].render_line();
        assert!(!rendered.contains(PRIVATE_PROVIDER_TEXT));
        assert!(!rendered.contains("private-id"));
        assert!(!rendered.contains("private-token"));
    }

    #[tokio::test]
    async fn channel_preserves_causality_and_tags_new_ingress() {
        let (sender, receiver) = channel();
        let parent = crate::observability::OperationContext::new(
            "global_control",
            crate::observability::OperationSource::MediaControl,
        );
        crate::observability::in_operation(parent.clone(), async {
            sender
                .with_source(crate::observability::OperationSource::Terminal)
                .send_async(spotify_search("private-query"))
                .await
                .unwrap();
        })
        .await;

        let envelope = receiver.recv_async().await.unwrap();
        assert_eq!(envelope.context.trace_id, parent.trace_id);
        assert_ne!(envelope.context.span_id, parent.span_id);
        assert_eq!(
            envelope.context.source,
            crate::observability::OperationSource::MediaControl
        );
        assert_eq!(envelope.context.operation, "global_control");

        sender
            .with_source(crate::observability::OperationSource::Terminal)
            .send_async(spotify_search("another-private-query"))
            .await
            .unwrap();
        let envelope = receiver.recv_async().await.unwrap();
        assert_eq!(
            envelope.context.source,
            crate::observability::OperationSource::Terminal
        );
        assert_eq!(envelope.context.operation, "search_spotify");
    }

    #[test]
    fn latest_request_replaces_pending_work_at_the_newest_position() {
        let mut scheduler = scheduler();
        assert_eq!(
            enqueue(&mut scheduler, spotify_search("old")),
            EnqueueOutcome::Accepted
        );
        enqueue(&mut scheduler, ClientRequest::Player(PlayerRequest::Pause));
        assert_eq!(
            enqueue(&mut scheduler, spotify_search("new")),
            EnqueueOutcome::Replaced
        );

        let first = scheduler.pop_ready().unwrap();
        assert!(matches!(
            first.request,
            ClientRequest::Player(PlayerRequest::Pause)
        ));
        scheduler.finish(first.delivery);
        let second = scheduler.pop_ready().unwrap();
        assert!(
            matches!(second.request, ClientRequest::Search { ref query, .. } if query == "new")
        );
    }

    #[test]
    fn latest_class_serializes_active_work_and_keeps_one_newest_successor() {
        let mut scheduler = scheduler();
        enqueue(&mut scheduler, spotify_search("active"));
        let active = scheduler.pop_ready().unwrap();

        assert_eq!(
            enqueue(&mut scheduler, spotify_search("pending")),
            EnqueueOutcome::Accepted
        );
        assert_eq!(
            enqueue(&mut scheduler, spotify_search("newest")),
            EnqueueOutcome::Replaced
        );
        assert!(scheduler.pop_ready().is_none());

        scheduler.finish(active.delivery);
        let newest = scheduler.pop_ready().unwrap();
        assert!(
            matches!(newest.request, ClientRequest::Search { ref query, .. } if query == "newest")
        );
    }

    #[test]
    fn latest_search_successor_completes_after_the_active_result_populates_cache() {
        let mut ui = crate::state::UIState::default();
        ui.new_page(crate::state::PageState::Search {
            line_input: crate::ui::single_line_input::LineInput::default(),
            current_query: String::new(),
            state: crate::state::SearchPageUIState::new(),
        });
        let first_foo = ui.begin_search(ActiveProvider::Spotify, "foo");

        let mut scheduler = scheduler();
        enqueue(
            &mut scheduler,
            spotify_search_with_reference("foo", &first_foo),
        );
        let active = scheduler.pop_ready().unwrap();

        let bar = ui.begin_search(ActiveProvider::Spotify, "bar");
        assert_eq!(
            enqueue(&mut scheduler, spotify_search_with_reference("bar", &bar)),
            EnqueueOutcome::Accepted
        );
        let latest_foo = ui.begin_search(ActiveProvider::Spotify, "foo");
        assert_eq!(
            enqueue(
                &mut scheduler,
                spotify_search_with_reference("foo", &latest_foo),
            ),
            EnqueueOutcome::Replaced
        );

        // The active foo request populates the cache, but cannot finish the
        // later foo invocation. The queued latest request then takes the
        // provider handler's cache-hit path and finishes its own lifecycle.
        ui.finish_search_success(ActiveProvider::Spotify, "foo", &first_foo, 2);
        assert!(matches!(
            ui.current_page(),
            crate::state::PageState::Search {
                state: crate::state::SearchPageUIState {
                    search_lifecycle: crate::state::SearchLifecycle::Loading { reference, .. },
                    ..
                },
                ..
            } if reference == &latest_foo
        ));

        scheduler.finish(active.delivery);
        let newest = scheduler.pop_ready().unwrap();
        let ClientRequest::Search {
            query,
            lifecycle_reference,
        } = newest.request
        else {
            panic!("latest Search successor is scheduled");
        };
        assert_eq!(query, "foo");
        assert_eq!(lifecycle_reference, latest_foo);
        ui.finish_search_success(ActiveProvider::Spotify, &query, &lifecycle_reference, 2);
        assert!(matches!(
            ui.current_page(),
            crate::state::PageState::Search {
                state: crate::state::SearchPageUIState {
                    search_lifecycle: crate::state::SearchLifecycle::Ready { result_count: 2 },
                    ..
                },
                ..
            }
        ));
    }

    #[test]
    fn replacing_pending_search_terminalizes_its_retained_page() {
        let configs = crate::ui::initialize_test_config();
        let ring = std::sync::Arc::new(parking_lot::Mutex::new(std::collections::VecDeque::new()));
        let (diagnostics, _runtime) = crate::observability::disabled(ring);
        let state = std::sync::Arc::new(crate::state::State::new_with_configs(
            false,
            diagnostics,
            configs,
        ));
        let mut scheduler = scheduler();

        enqueue(
            &mut scheduler,
            spotify_search_with_reference("active", "active-ref"),
        );
        let active = scheduler.pop_ready().expect("active search request");

        let old_reference = {
            let mut ui = state.ui.lock();
            ui.new_page(crate::state::PageState::Search {
                line_input: crate::ui::single_line_input::LineInput::default(),
                current_query: String::new(),
                state: crate::state::SearchPageUIState::new(),
            });
            ui.begin_search(ActiveProvider::Spotify, "old pending query")
        };
        assert_eq!(
            enqueue(
                &mut scheduler,
                spotify_search_with_reference("old pending query", &old_reference),
            ),
            EnqueueOutcome::Accepted
        );

        let latest_reference = {
            let mut ui = state.ui.lock();
            ui.new_page(crate::state::PageState::Search {
                line_input: crate::ui::single_line_input::LineInput::default(),
                current_query: String::new(),
                state: crate::state::SearchPageUIState::new(),
            });
            ui.begin_search(ActiveProvider::Spotify, "latest query")
        };
        let outcome = enqueue_with_search_supersession(
            &state,
            &mut scheduler,
            ContextualRequest::new(
                spotify_search_with_reference("latest query", &latest_reference),
                crate::observability::OperationSource::System,
            ),
            None,
        );

        assert_eq!(outcome, EnqueueOutcome::Replaced);
        let ui = state.ui.lock();
        let old_page = ui.history.iter().find_map(|page| match page {
            crate::state::PageState::Search {
                current_query,
                state,
                ..
            } if current_query == "old pending query" => Some(state),
            _ => None,
        });
        assert!(matches!(
            old_page.map(|page| &page.search_lifecycle),
            Some(crate::state::SearchLifecycle::Superseded { reference })
                if reference == &old_reference
        ));
        assert!(matches!(
            ui.current_page(),
            crate::state::PageState::Search {
                state: crate::state::SearchPageUIState {
                    search_lifecycle: crate::state::SearchLifecycle::Loading { reference, .. },
                    ..
                },
                ..
            } if reference == &latest_reference
        ));

        scheduler.finish(active.delivery);
    }

    #[test]
    fn active_controls_keep_only_the_newest_pending_successor() {
        let mut scheduler = scheduler();
        enqueue(
            &mut scheduler,
            ClientRequest::ActivePlaybackControl(crate::client::ActivePlaybackControl::Toggle),
        );
        let active = scheduler.pop_ready().unwrap();

        assert_eq!(
            enqueue(
                &mut scheduler,
                ClientRequest::ActivePlaybackControl(crate::client::ActivePlaybackControl::Pause,),
            ),
            EnqueueOutcome::Accepted
        );
        assert_eq!(
            enqueue(
                &mut scheduler,
                ClientRequest::ActivePlaybackControl(crate::client::ActivePlaybackControl::Play,),
            ),
            EnqueueOutcome::Replaced
        );
        assert!(scheduler.pop_ready().is_none());

        scheduler.finish(active.delivery);
        assert!(matches!(
            scheduler.pop_ready().unwrap().request,
            ClientRequest::ActivePlaybackControl(crate::client::ActivePlaybackControl::Play)
        ));
    }

    #[test]
    fn coalescible_request_is_deduplicated_while_pending_and_active() {
        let mut scheduler = scheduler();
        assert_eq!(
            enqueue(&mut scheduler, ClientRequest::GetCurrentPlayback),
            EnqueueOutcome::Accepted
        );
        assert_eq!(
            enqueue(&mut scheduler, ClientRequest::GetCurrentPlayback),
            EnqueueOutcome::Coalesced
        );
        let active = scheduler.pop_ready().unwrap();
        assert_eq!(
            enqueue(&mut scheduler, ClientRequest::GetCurrentPlayback),
            EnqueueOutcome::Coalesced
        );
        scheduler.finish(active.delivery);
        assert!(scheduler.is_idle());
    }

    #[test]
    fn must_deliver_requests_are_neither_replaced_nor_coalesced() {
        let mut scheduler = scheduler();
        assert_eq!(
            enqueue(&mut scheduler, ClientRequest::TestYouTubeAuth),
            EnqueueOutcome::Accepted
        );
        assert_eq!(
            enqueue(&mut scheduler, ClientRequest::TestYouTubeAuth),
            EnqueueOutcome::Accepted
        );

        let first = scheduler.pop_ready().unwrap();
        let second = scheduler.pop_ready().unwrap();
        assert_eq!(first.delivery, RequestDelivery::MustDeliver);
        assert_eq!(second.delivery, RequestDelivery::MustDeliver);
    }

    #[test]
    fn ordered_requests_never_run_concurrently() {
        let mut scheduler = scheduler();
        enqueue(&mut scheduler, ClientRequest::Player(PlayerRequest::Pause));
        enqueue(
            &mut scheduler,
            ClientRequest::YouTubePlayer(YouTubePlayerRequest::Repeat),
        );

        let first = scheduler.pop_ready().unwrap();
        assert_eq!(first.delivery, RequestDelivery::Ordered);
        assert!(scheduler.pop_ready().is_none());
        scheduler.finish(first.delivery);
        assert_eq!(
            scheduler.pop_ready().unwrap().delivery,
            RequestDelivery::Ordered
        );
    }

    #[test]
    fn account_change_waits_for_active_work_and_blocks_successors() {
        let mut scheduler = scheduler();
        enqueue(&mut scheduler, spotify_search("active"));
        let active = scheduler.pop_ready().unwrap();
        enqueue(
            &mut scheduler,
            ClientRequest::ManageAccount(AccountOperation::Validate(ActiveProvider::Spotify)),
        );
        enqueue(&mut scheduler, spotify_search("after"));

        assert!(scheduler.pop_ready().is_none());
        scheduler.finish(active.delivery);

        let account = scheduler.pop_ready().unwrap();
        assert!(matches!(account.request, ClientRequest::ManageAccount(_)));
        assert!(scheduler.pop_ready().is_none());
        scheduler.finish(account.delivery);

        assert!(matches!(
            scheduler.pop_ready().unwrap().request,
            ClientRequest::Search { ref query, .. } if query == "after"
        ));
    }

    #[test]
    fn account_change_is_not_overlapped_by_playback_or_shutdown() {
        let mut scheduler = scheduler();
        enqueue(
            &mut scheduler,
            ClientRequest::ManageAccount(AccountOperation::Remove(ActiveProvider::YouTubeMusic)),
        );
        enqueue(
            &mut scheduler,
            ClientRequest::SwitchProvider(ActiveProvider::Spotify),
        );
        let account = scheduler.pop_ready().unwrap();
        assert!(scheduler.pop_ready().is_none());
        scheduler.finish(account.delivery);
        assert!(matches!(
            scheduler.pop_ready().unwrap().request,
            ClientRequest::SwitchProvider(ActiveProvider::Spotify)
        ));
    }

    #[test]
    fn in_flight_limit_stops_dispatch_until_completion() {
        let mut scheduler = scheduler();
        enqueue(&mut scheduler, ClientRequest::TestYouTubeAuth);
        enqueue(&mut scheduler, ClientRequest::ReauthenticateSpotify);
        enqueue(&mut scheduler, ClientRequest::AuthenticateYouTubeBrowser);

        let first = scheduler.pop_ready().unwrap();
        let second = scheduler.pop_ready().unwrap();
        assert!(scheduler.pop_ready().is_none());
        scheduler.finish(first.delivery);
        assert!(scheduler.pop_ready().is_some());
        scheduler.finish(second.delivery);
    }

    #[test]
    fn pending_capacity_pauses_and_reopens_receiver_ingress() {
        let mut scheduler = RequestScheduler::new(2, 1);
        enqueue(&mut scheduler, ClientRequest::Player(PlayerRequest::Pause));
        enqueue(&mut scheduler, ClientRequest::Player(PlayerRequest::Resume));

        assert!(!scheduler.can_receive());
        let active = scheduler.pop_ready().unwrap();
        assert!(scheduler.can_receive());
        assert!(scheduler.pop_ready().is_none());
        scheduler.finish(active.delivery);
        assert!(scheduler.pop_ready().is_some());
    }

    #[test]
    fn received_latest_playback_request_cancels_active_work_before_successor_dispatch() {
        let coordinator = PlaybackCoordinator::new(ActiveProvider::Spotify);
        let older = coordinator.reserve_activation();
        let older_probe = older.clone();
        let mut scheduler = scheduler();

        scheduler.enqueue(
            ClientRequest::SwitchProvider(ActiveProvider::YouTubeMusic),
            Some(older),
        );
        let active = scheduler.pop_ready().unwrap();
        let newer = coordinator.reserve_activation();
        let newer_probe = newer.clone();
        assert_eq!(
            scheduler.enqueue(
                ClientRequest::SwitchProvider(ActiveProvider::Spotify),
                Some(newer),
            ),
            EnqueueOutcome::Accepted
        );

        assert!(!coordinator.activation_permit_is_current(&older_probe));
        assert!(coordinator.activation_permit_is_current(&newer_probe));
        assert!(scheduler.pop_ready().is_none());
        scheduler.finish(active.delivery);
        let scheduled = scheduler.pop_ready().unwrap();
        assert!(matches!(
            scheduled.request,
            ClientRequest::SwitchProvider(ActiveProvider::Spotify)
        ));
        assert!(coordinator.activation_permit_is_current(scheduled.activation.as_ref().unwrap()));
    }

    #[test]
    fn shutdown_waits_for_ordered_work_and_stops_further_ingress() {
        let mut scheduler = scheduler();
        enqueue(&mut scheduler, ClientRequest::Player(PlayerRequest::Pause));
        let active = scheduler.pop_ready().unwrap();
        enqueue(&mut scheduler, ClientRequest::Player(PlayerRequest::Resume));
        enqueue(&mut scheduler, ClientRequest::ShutdownPlayback);

        assert!(!scheduler.can_receive());
        assert!(scheduler.pop_ready().is_none());
        scheduler.finish(active.delivery);
        let pending_ordered = scheduler.pop_ready().unwrap();
        assert!(matches!(
            pending_ordered.request,
            ClientRequest::Player(PlayerRequest::Resume)
        ));
        assert!(scheduler.pop_ready().is_none());
        scheduler.finish(pending_ordered.delivery);
        assert!(scheduler.pop_ready().unwrap().request.is_shutdown());
    }

    #[test]
    fn shutdown_is_not_starved_by_in_flight_independent_work() {
        let mut scheduler = RequestScheduler::new(4, 1);
        enqueue(&mut scheduler, spotify_search("active"));
        let active = scheduler.pop_ready().unwrap();
        enqueue(&mut scheduler, spotify_search("pending"));
        enqueue(&mut scheduler, ClientRequest::GetCurrentPlayback);
        enqueue(&mut scheduler, ClientRequest::ShutdownPlayback);

        assert_eq!(scheduler.pending.len(), 1);
        let shutdown = scheduler.pop_ready().unwrap();
        assert!(shutdown.request.is_shutdown());
        scheduler.finish(shutdown.delivery);
        scheduler.finish(active.delivery);
    }

    #[test]
    fn bounded_channel_applies_backpressure_and_still_delivers_shutdown() {
        let (sender, receiver) = channel();
        for _ in 0..CLIENT_REQUEST_CAPACITY {
            sender.send(ClientRequest::GetCurrentPlayback).unwrap();
        }
        assert!(matches!(
            sender.try_send(ClientRequest::ShutdownPlayback),
            Err(flume::TrySendError::Full(ClientRequest::ShutdownPlayback))
        ));

        let blocked_sender = sender.clone();
        let thread = std::thread::spawn(move || {
            blocked_sender
                .send(ClientRequest::ShutdownPlayback)
                .unwrap();
        });
        assert!(matches!(
            receiver.recv().unwrap().request,
            ClientRequest::GetCurrentPlayback
        ));
        thread.join().unwrap();
        for _ in 1..CLIENT_REQUEST_CAPACITY {
            assert!(matches!(
                receiver.recv().unwrap().request,
                ClientRequest::GetCurrentPlayback
            ));
        }
        assert!(receiver.recv().unwrap().request.is_shutdown());
    }

    #[tokio::test]
    async fn correlated_timeline_classifies_supersession_panic_and_shutdown_once() {
        let directory = std::env::temp_dir().join(format!(
            "unified-player-causality-{}-{}",
            std::process::id(),
            crate::observability::random_hex::<6>()
        ));
        std::fs::create_dir_all(&directory).unwrap();
        let ring = std::sync::Arc::new(parking_lot::Mutex::new(VecDeque::new()));
        let (handle, mut runtime) = crate::observability::start(&directory, ring.clone()).unwrap();
        crate::observability::install(handle).unwrap();

        let mut scheduler = scheduler();
        let first = ContextualRequest::new(
            spotify_search("private-a"),
            crate::observability::OperationSource::Terminal,
        );
        let first_ref = first.context.short_reference().to_owned();
        crate::observability::request_accepted(&first.context, 0);
        scheduler.enqueue(first, None);

        let second = ContextualRequest::new(
            spotify_search("private-b"),
            crate::observability::OperationSource::Terminal,
        );
        let second_ref = second.context.short_reference().to_owned();
        crate::observability::request_accepted(&second.context, 1);
        assert_eq!(scheduler.enqueue(second, None), EnqueueOutcome::Replaced);

        let third = ContextualRequest::new(
            spotify_search("private-c"),
            crate::observability::OperationSource::Terminal,
        );
        let third_context = third.context.clone();
        let third_ref = third.context.short_reference().to_owned();
        crate::observability::request_accepted(&third.context, 1);
        assert_eq!(scheduler.enqueue(third, None), EnqueueOutcome::Replaced);
        let ready = scheduler.pop_ready().unwrap();
        crate::observability::request_started(
            &ready.context,
            ready.enqueued_at.elapsed(),
            scheduler.pending.len(),
        );
        crate::observability::in_operation(third_context.clone(), async {
            crate::observability::operation_stage(
                crate::observability::Component::Application,
                "retry_attempt_2",
                Some(std::time::Duration::from_millis(7)),
                Some(crate::observability::OperationOutcome::Success),
            );
        })
        .await;
        scheduler.finish(ready.delivery);
        crate::observability::request_completed(
            &third_context,
            crate::observability::OperationOutcome::Success,
            std::time::Duration::from_millis(9),
            None,
        );

        let panicking = ContextualRequest::new(
            ClientRequest::TestYouTubeAuth,
            crate::observability::OperationSource::System,
        );
        let panic_context = panicking.context.clone();
        let panic_ref = panic_context.short_reference().to_owned();
        crate::observability::request_accepted(&panic_context, 0);
        scheduler.enqueue(panicking, None);
        let ready = scheduler.pop_ready().unwrap();
        let mut tasks = JoinSet::new();
        let abort = tasks.spawn(async {
            panic!("seeded task panic payload");
            #[allow(unreachable_code)]
            RequestTaskOutcome::Success
        });
        let mut metadata = HashMap::from([(
            abort.id(),
            TaskMetadata {
                delivery: ready.delivery,
                context: ready.context,
                started_at: Instant::now(),
                ui_request: None,
                bulk: None,
            },
        )]);
        complete_task(
            tasks.join_next_with_id().await.unwrap(),
            &mut metadata,
            &mut scheduler,
            None,
        );

        let failing = ContextualRequest::new(
            ClientRequest::TestYouTubeAuth,
            crate::observability::OperationSource::Terminal,
        );
        let failure_context = failing.context.clone();
        let failure_ref = failure_context.short_reference().to_owned();
        crate::observability::request_accepted(&failure_context, 0);
        scheduler.enqueue(failing, None);
        let ready = scheduler.pop_ready().unwrap();
        let mut tasks = JoinSet::new();
        let abort = tasks.spawn(async { RequestTaskOutcome::Failure });
        let mut metadata = HashMap::from([(
            abort.id(),
            TaskMetadata {
                delivery: ready.delivery,
                context: ready.context,
                started_at: Instant::now(),
                ui_request: None,
                bulk: None,
            },
        )]);
        complete_task(
            tasks.join_next_with_id().await.unwrap(),
            &mut metadata,
            &mut scheduler,
            None,
        );

        let pending = ContextualRequest::new(
            spotify_search("private-shutdown"),
            crate::observability::OperationSource::System,
        );
        let shutdown_ref = pending.context.short_reference().to_owned();
        crate::observability::request_accepted(&pending.context, 0);
        scheduler.enqueue(pending, None);
        scheduler.enqueue(ClientRequest::ShutdownPlayback, None);

        runtime.shutdown(std::time::Duration::from_secs(1)).unwrap();
        let entries = ring.lock().iter().cloned().collect::<Vec<_>>();
        for (reference, expected) in [
            (
                first_ref,
                crate::observability::OperationOutcome::Superseded,
            ),
            (
                second_ref,
                crate::observability::OperationOutcome::Superseded,
            ),
            (
                third_ref.clone(),
                crate::observability::OperationOutcome::Success,
            ),
            (panic_ref, crate::observability::OperationOutcome::Panicked),
            (failure_ref, crate::observability::OperationOutcome::Error),
            (
                shutdown_ref,
                crate::observability::OperationOutcome::Cancelled,
            ),
        ] {
            let terminal = entries
                .iter()
                .filter(|entry| {
                    entry.code == "REQUEST_COMPLETED"
                        && entry.reference.as_deref() == Some(reference.as_str())
                })
                .collect::<Vec<_>>();
            assert_eq!(terminal.len(), 1, "one terminal outcome for {reference}");
            assert_eq!(terminal[0].outcome, Some(expected));
        }
        assert!(entries.iter().any(|entry| {
            entry.code == "OPERATION_STAGE"
                && entry.reference.as_deref() == Some(third_ref.as_str())
        }));
        let rendered = entries
            .iter()
            .map(crate::observability::UiDiagnosticEntry::render_line)
            .collect::<Vec<_>>()
            .join("\n");
        assert!(!rendered.contains("private-a"));
        assert!(!rendered.contains("private-b"));
        assert!(!rendered.contains("private-c"));
        assert!(!rendered.contains("seeded task panic payload"));
        std::fs::remove_dir_all(directory).unwrap();
    }
}
