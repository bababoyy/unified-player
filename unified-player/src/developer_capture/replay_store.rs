//! Encrypted child-artifact orchestration for private replay.

use std::{fmt, path::Path, sync::Arc, time::Instant};

use tokio_util::sync::CancellationToken;

use super::{
    controller::{CaptureRefSource, RandomCaptureRefSource},
    model::{
        CaptureCompleteness, CaptureLimits, CapturePurpose, CaptureRecordKind, CaptureRecordV1,
        CaptureRef, EndpointRole, PrivateCaptureV1, ProviderClientKind, SafeCaptureRef,
        SafeOperationRef, SafeTerminalCategory, SensitiveBytes, TransportKind,
    },
    payload::{encode_fields, field, PrivateField, PrivatePayloadKind},
    replay::{
        run_fresh_replay, run_offline_replay, FreshReplayOutcome, FreshReplayPolicy,
        FreshSemanticReplayAdapter, OfflineReplayAdapter, OfflineReplayOutcome, ReplayClock,
        ReplayDecision, ReplayRecipeBuildError, ReplayRecipeV1, ReplayRunResult, ReplayStartError,
        ReplayTerminalOutcome, ReplayTimer, SystemReplayClock,
    },
    security::CapturePassphrase,
    store::{CaptureStore, MaintenanceReport, StoreError},
};

const REPLAY_RESULT_MAGIC: &[u8; 8] = b"SPREPV1\0";
const REPLAY_RESULT_SCHEMA_VERSION: u16 = 1;
const CHILD_REFERENCE_ATTEMPTS: usize = 8;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ReplayMode {
    Offline,
    Fresh,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct SafeReplayArtifact {
    pub(crate) capture_ref: SafeCaptureRef,
    pub(crate) mode: ReplayMode,
    pub(crate) outcome: ReplayTerminalOutcome,
    pub(crate) terminal_category: SafeTerminalCategory,
    pub(crate) record_count: u16,
}

pub(crate) struct ReplayArtifactService {
    store: CaptureStore,
    refs: Arc<dyn CaptureRefSource>,
    clock: Arc<dyn ReplayClock>,
}

impl fmt::Debug for ReplayArtifactService {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ReplayArtifactService")
            .field("store", &"[private]")
            .finish_non_exhaustive()
    }
}

impl ReplayArtifactService {
    pub(crate) fn open(
        root: impl AsRef<Path>,
        limits: CaptureLimits,
    ) -> Result<(Self, MaintenanceReport), ReplayFacadeError> {
        let (store, maintenance) = CaptureStore::open(root, limits)?;
        Ok((
            Self {
                store,
                refs: Arc::new(RandomCaptureRefSource),
                clock: Arc::new(SystemReplayClock),
            },
            maintenance,
        ))
    }

    pub(crate) fn with_sources(
        store: CaptureStore,
        refs: Arc<dyn CaptureRefSource>,
        clock: Arc<dyn ReplayClock>,
    ) -> Self {
        Self { store, refs, clock }
    }

    pub(crate) fn list_safe_refs(&self) -> Result<Vec<SafeCaptureRef>, ReplayFacadeError> {
        self.store.list_safe_refs().map_err(Into::into)
    }

    pub(crate) fn run_offline<A: OfflineReplayAdapter>(
        &self,
        source_ref: SafeCaptureRef,
        passphrase: &CapturePassphrase,
        operation_ref: SafeOperationRef,
        adapter: &A,
    ) -> Result<SafeReplayArtifact, ReplayFacadeError> {
        let source = self
            .store
            .read_private_by_safe_ref(source_ref, passphrase)?;
        let recipe = ReplayRecipeV1::from_private_capture(&source)?;
        let child_ref = self.allocate_child_ref(recipe.parent_capture_ref())?;
        let created_unix_ms = self.clock.unix_ms();
        let result = run_offline_replay(&recipe, child_ref, adapter)?;
        let completed_unix_ms = self.clock.unix_ms().max(created_unix_ms);
        self.persist_result(
            &result,
            operation_ref,
            created_unix_ms,
            completed_unix_ms,
            passphrase,
        )
    }

    #[allow(clippy::too_many_arguments)]
    pub(crate) async fn run_fresh<A, T>(
        &self,
        source_ref: SafeCaptureRef,
        passphrase: &CapturePassphrase,
        operation_ref: SafeOperationRef,
        adapter: &A,
        timer: &T,
        cancellation: &CancellationToken,
        policy: FreshReplayPolicy,
    ) -> Result<SafeReplayArtifact, ReplayFacadeError>
    where
        A: FreshSemanticReplayAdapter,
        T: ReplayTimer,
    {
        let source = self
            .store
            .read_private_by_safe_ref(source_ref, passphrase)?;
        let recipe = ReplayRecipeV1::from_private_capture(&source)?;
        let child_ref = self.allocate_child_ref(recipe.parent_capture_ref())?;
        let created_unix_ms = self.clock.unix_ms();
        let result = run_fresh_replay(
            &recipe,
            child_ref,
            adapter,
            self.clock.as_ref(),
            timer,
            cancellation,
            policy,
        )
        .await?;
        let completed_unix_ms = self.clock.unix_ms().max(created_unix_ms);
        self.persist_result(
            &result,
            operation_ref,
            created_unix_ms,
            completed_unix_ms,
            passphrase,
        )
    }

    fn allocate_child_ref(&self, parent_ref: CaptureRef) -> Result<CaptureRef, ReplayFacadeError> {
        for _ in 0..CHILD_REFERENCE_ATTEMPTS {
            let candidate = self.refs.next_ref();
            if candidate != parent_ref && !self.store.contains_safe_ref(candidate.safe())? {
                return Ok(candidate);
            }
        }
        Err(ReplayFacadeError::ChildReferenceCollision)
    }

    fn persist_result(
        &self,
        result: &ReplayRunResult,
        operation_ref: SafeOperationRef,
        created_unix_ms: u64,
        completed_unix_ms: u64,
        passphrase: &CapturePassphrase,
    ) -> Result<SafeReplayArtifact, ReplayFacadeError> {
        let capture =
            replay_child_capture(result, operation_ref, created_unix_ms, completed_unix_ms)?;
        let terminal = result.terminal();
        let outcome = terminal.outcome();
        let mode = replay_mode(outcome);
        let terminal_category = safe_terminal(outcome);
        let record_count = u16::try_from(capture.records().len()).unwrap_or(u16::MAX);
        let deadline = Instant::now()
            .checked_add(self.store.finalization_timeout())
            .ok_or(ReplayFacadeError::Persistence)?;
        let stored = self
            .store
            .write_before(&capture, passphrase, deadline)
            .map_err(|error| match error {
                StoreError::AlreadyExists | StoreError::AmbiguousSafeReference => {
                    ReplayFacadeError::ChildReferenceCollision
                }
                _ => ReplayFacadeError::Store(error),
            })?;
        Ok(SafeReplayArtifact {
            capture_ref: stored.capture_ref,
            mode,
            outcome,
            terminal_category,
            record_count,
        })
    }
}

#[derive(Debug)]
pub(crate) enum ReplayFacadeError {
    ChildReferenceCollision,
    Encoding,
    Persistence,
    Recipe(ReplayRecipeBuildError),
    ReplayStart(ReplayStartError),
    Store(StoreError),
}

impl fmt::Display for ReplayFacadeError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::ChildReferenceCollision => {
                "private replay child reference could not be allocated"
            }
            Self::Encoding => "private replay result could not be encoded",
            Self::Persistence => "private replay result could not be persisted",
            Self::Recipe(_) => "private replay recipe is unavailable",
            Self::ReplayStart(_) => "private replay could not be admitted",
            Self::Store(_) => "private replay vault operation failed",
        })
    }
}

impl std::error::Error for ReplayFacadeError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Recipe(error) => Some(error),
            Self::ReplayStart(error) => Some(error),
            Self::Store(error) => Some(error),
            _ => None,
        }
    }
}

impl From<ReplayRecipeBuildError> for ReplayFacadeError {
    fn from(error: ReplayRecipeBuildError) -> Self {
        Self::Recipe(error)
    }
}

impl From<ReplayStartError> for ReplayFacadeError {
    fn from(error: ReplayStartError) -> Self {
        Self::ReplayStart(error)
    }
}

impl From<StoreError> for ReplayFacadeError {
    fn from(error: StoreError) -> Self {
        Self::Store(error)
    }
}

fn replay_child_capture(
    result: &ReplayRunResult,
    operation_ref: SafeOperationRef,
    created_unix_ms: u64,
    completed_unix_ms: u64,
) -> Result<PrivateCaptureV1, ReplayFacadeError> {
    let terminal = result.terminal();
    let lineage = terminal.lineage();
    let outcome = terminal.outcome();
    let terminal_category = safe_terminal(outcome);
    let transport = match replay_mode(outcome) {
        ReplayMode::Offline => TransportKind::OfflineReplay,
        ReplayMode::Fresh => TransportKind::FreshReplay,
    };
    let elapsed_ms = completed_unix_ms.saturating_sub(created_unix_ms);
    let result_record = CaptureRecordV1::with_context(
        0,
        0,
        Some(lineage.source_exchange_ref()),
        EndpointRole::Operation,
        ProviderClientKind::Unknown,
        transport,
        0,
        CaptureRecordKind::OperationBoundary,
        encode_replay_result(result),
    );
    let terminal_payload = encode_fields(
        PrivatePayloadKind::TerminalOutcome,
        &[PrivateField::text(
            field::OUTCOME,
            terminal_category_name(terminal_category),
        )],
    )
    .map_err(|_| ReplayFacadeError::Encoding)?;
    let terminal_record = CaptureRecordV1::with_context(
        1,
        elapsed_ms,
        Some(lineage.source_exchange_ref()),
        EndpointRole::Operation,
        ProviderClientKind::Unknown,
        transport,
        0,
        CaptureRecordKind::TerminalOutcome,
        terminal_payload,
    );
    Ok(PrivateCaptureV1::new(
        lineage.child_capture_ref(),
        created_unix_ms,
        completed_unix_ms,
        CapturePurpose::Replay,
        operation_ref,
        vec![result_record, terminal_record],
        CaptureCompleteness::Complete,
        Vec::new(),
        0,
        terminal_category,
    ))
}

fn encode_replay_result(result: &ReplayRunResult) -> SensitiveBytes {
    let terminal = result.terminal();
    let lineage = terminal.lineage();
    let mut bytes = Vec::with_capacity(64);
    bytes.extend_from_slice(REPLAY_RESULT_MAGIC);
    bytes.extend_from_slice(&REPLAY_RESULT_SCHEMA_VERSION.to_be_bytes());
    bytes.push(match replay_mode(terminal.outcome()) {
        ReplayMode::Offline => 1,
        ReplayMode::Fresh => 2,
    });
    bytes.extend_from_slice(&lineage.parent_capture_ref().bytes());
    bytes.extend_from_slice(&lineage.source_exchange_ref().bytes());
    bytes.push(replay_outcome_tag(terminal.outcome()));
    if let Some(decision) = result.observed_decision() {
        bytes.push(1);
        encode_decision(&mut bytes, decision);
    } else {
        bytes.push(0);
    }
    SensitiveBytes::new(bytes)
}

fn encode_decision(bytes: &mut Vec<u8>, decision: ReplayDecision) {
    bytes.push(decision.parser as u8);
    bytes.push(decision.provider as u8);
    bytes.push(u8::from(decision.streaming_data_present));
    bytes.extend_from_slice(&decision.returned_formats.to_be_bytes());
    bytes.extend_from_slice(&decision.supported_formats.to_be_bytes());
    bytes.extend_from_slice(&decision.direct_formats.to_be_bytes());
    bytes.extend_from_slice(&decision.cipher_formats.to_be_bytes());
    let (selection, itag) = match decision.selection {
        super::replay::ReplaySelectionOutcome::NotAttempted => (0, 0),
        super::replay::ReplaySelectionOutcome::Selected { itag } => (1, itag),
        super::replay::ReplaySelectionOutcome::CipherOnly => (2, 0),
        super::replay::ReplaySelectionOutcome::UnsupportedFormat => (3, 0),
        super::replay::ReplaySelectionOutcome::NoDirectFormat => (4, 0),
    };
    bytes.push(selection);
    bytes.extend_from_slice(&itag.to_be_bytes());
}

const fn replay_mode(outcome: ReplayTerminalOutcome) -> ReplayMode {
    match outcome {
        ReplayTerminalOutcome::Offline(_) => ReplayMode::Offline,
        ReplayTerminalOutcome::Fresh(_) => ReplayMode::Fresh,
    }
}

const fn safe_terminal(outcome: ReplayTerminalOutcome) -> SafeTerminalCategory {
    match outcome {
        ReplayTerminalOutcome::Offline(
            OfflineReplayOutcome::Reproduced | OfflineReplayOutcome::Changed,
        )
        | ReplayTerminalOutcome::Fresh(
            FreshReplayOutcome::Reproduced | FreshReplayOutcome::ProviderChanged,
        ) => SafeTerminalCategory::Success,
        ReplayTerminalOutcome::Fresh(FreshReplayOutcome::Cancelled) => {
            SafeTerminalCategory::Cancelled
        }
        ReplayTerminalOutcome::Fresh(FreshReplayOutcome::TimedOut) => {
            SafeTerminalCategory::TimedOut
        }
        ReplayTerminalOutcome::Offline(OfflineReplayOutcome::Panicked)
        | ReplayTerminalOutcome::Fresh(FreshReplayOutcome::Panicked) => {
            SafeTerminalCategory::Panicked
        }
        ReplayTerminalOutcome::Offline(
            OfflineReplayOutcome::UnsupportedSchema | OfflineReplayOutcome::Incomplete,
        )
        | ReplayTerminalOutcome::Fresh(
            FreshReplayOutcome::AuthUnavailable
            | FreshReplayOutcome::BrowserContended
            | FreshReplayOutcome::ExpiredInput
            | FreshReplayOutcome::NetworkFailed
            | FreshReplayOutcome::UnsupportedSchema
            | FreshReplayOutcome::Inconclusive,
        ) => SafeTerminalCategory::Failed,
    }
}

const fn terminal_category_name(category: SafeTerminalCategory) -> &'static str {
    match category {
        SafeTerminalCategory::Success => "success",
        SafeTerminalCategory::Failed => "failed",
        SafeTerminalCategory::Cancelled => "cancelled",
        SafeTerminalCategory::Superseded => "superseded",
        SafeTerminalCategory::TimedOut => "timed_out",
        SafeTerminalCategory::Panicked => "panicked",
    }
}

const fn replay_outcome_tag(outcome: ReplayTerminalOutcome) -> u8 {
    match outcome {
        ReplayTerminalOutcome::Offline(OfflineReplayOutcome::Reproduced) => 1,
        ReplayTerminalOutcome::Offline(OfflineReplayOutcome::Changed) => 2,
        ReplayTerminalOutcome::Offline(OfflineReplayOutcome::UnsupportedSchema) => 3,
        ReplayTerminalOutcome::Offline(OfflineReplayOutcome::Incomplete) => 4,
        ReplayTerminalOutcome::Offline(OfflineReplayOutcome::Panicked) => 5,
        ReplayTerminalOutcome::Fresh(FreshReplayOutcome::Reproduced) => 16,
        ReplayTerminalOutcome::Fresh(FreshReplayOutcome::ProviderChanged) => 17,
        ReplayTerminalOutcome::Fresh(FreshReplayOutcome::AuthUnavailable) => 18,
        ReplayTerminalOutcome::Fresh(FreshReplayOutcome::BrowserContended) => 19,
        ReplayTerminalOutcome::Fresh(FreshReplayOutcome::ExpiredInput) => 20,
        ReplayTerminalOutcome::Fresh(FreshReplayOutcome::NetworkFailed) => 21,
        ReplayTerminalOutcome::Fresh(FreshReplayOutcome::Cancelled) => 22,
        ReplayTerminalOutcome::Fresh(FreshReplayOutcome::TimedOut) => 23,
        ReplayTerminalOutcome::Fresh(FreshReplayOutcome::Panicked) => 24,
        ReplayTerminalOutcome::Fresh(FreshReplayOutcome::UnsupportedSchema) => 25,
        ReplayTerminalOutcome::Fresh(FreshReplayOutcome::Inconclusive) => 26,
    }
}

#[cfg(test)]
mod tests {
    use std::{
        sync::{
            atomic::{AtomicUsize, Ordering},
            Mutex,
        },
        time::{Duration, SystemTime},
    };

    use super::*;
    use crate::developer_capture::{
        CaptureController, CaptureRefSource, OfflineAdapterResult, ReplayAuthKind,
        ReplayClientVersionPolicy, ReplayCredentialPolicy, ReplayParserOutcome, ReplayPlayerClient,
        ReplayProviderOutcome, ReplayQualityPolicy, ReplayRecipeParts, ReplaySelectionOutcome,
        StoreClock,
    };

    struct SequenceRefs(Mutex<Vec<CaptureRef>>);

    impl CaptureRefSource for SequenceRefs {
        fn next_ref(&self) -> CaptureRef {
            self.0.lock().unwrap().remove(0)
        }
    }

    struct CountingClock(AtomicUsize);

    impl ReplayClock for CountingClock {
        fn unix_ms(&self) -> u64 {
            1_100 + u64::try_from(self.0.fetch_add(1, Ordering::SeqCst)).unwrap_or(0)
        }
    }

    struct FixedStoreClock;

    impl StoreClock for FixedStoreClock {
        fn wall_now(&self) -> SystemTime {
            SystemTime::UNIX_EPOCH + Duration::from_millis(1_100)
        }
    }

    struct Offline;

    impl OfflineReplayAdapter for Offline {
        fn replay(
            &self,
            _input: super::super::replay::OfflineReplayInput<'_>,
        ) -> OfflineAdapterResult {
            OfflineAdapterResult::Decision(playable_decision())
        }
    }

    fn playable_decision() -> ReplayDecision {
        ReplayDecision {
            parser: ReplayParserOutcome::Parsed,
            provider: ReplayProviderOutcome::Playable,
            streaming_data_present: true,
            returned_formats: 1,
            supported_formats: 1,
            direct_formats: 1,
            cipher_formats: 0,
            selection: ReplaySelectionOutcome::Selected { itag: 140 },
        }
    }

    fn recipe() -> ReplayRecipeV1 {
        ReplayRecipeV1::new(ReplayRecipeParts {
            parent_capture_ref: CaptureRef::from_bytes([1; 16]),
            source_exchange_ref: super::super::model::ExchangeRef::from_bytes([2; 8]),
            source_purpose: CapturePurpose::InteractivePlayback,
            created_unix_ms: 1_000,
            expires_unix_ms: 2_000,
            media_id: super::super::model::SensitiveString::new("fixture-id".to_owned()),
            player_http_status: 200,
            player_response: SensitiveBytes::new(b"fixture".to_vec()),
            response_complete: true,
            client: ReplayPlayerClient::TvHtml5,
            auth: ReplayAuthKind::Browser,
            proof_token_present: false,
            client_version_policy: ReplayClientVersionPolicy::CurrentCompatible,
            credential_policy: ReplayCredentialPolicy::CurrentConfigured,
            quality: ReplayQualityPolicy::High,
            expected_decision: playable_decision(),
        })
        .unwrap()
    }

    #[test]
    fn offline_child_is_complete_encrypted_and_does_not_consume_an_arm() {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path().join("vault");
        let limits = CaptureLimits::default();
        let (store, _) =
            CaptureStore::with_clock(&root, limits, Arc::new(FixedStoreClock)).unwrap();
        let child_ref = CaptureRef::from_bytes([3; 16]);
        let service = ReplayArtifactService::with_sources(
            store,
            Arc::new(SequenceRefs(Mutex::new(vec![child_ref]))),
            Arc::new(CountingClock(AtomicUsize::new(0))),
        );
        let controller = CaptureController::new(limits).unwrap();
        controller.request_arm().unwrap();
        controller.accept_consent().unwrap();
        let armed = controller.snapshot();

        let result = run_offline_replay(&recipe(), child_ref, &Offline).unwrap();
        let passphrase = CapturePassphrase::new("replay child passphrase".to_owned()).unwrap();
        let review = service
            .persist_result(
                &result,
                SafeOperationRef::from_bytes([4; 4]),
                1_100,
                1_101,
                &passphrase,
            )
            .unwrap();

        assert_eq!(review.mode, ReplayMode::Offline);
        assert_eq!(review.record_count, 2);
        assert_eq!(review.terminal_category, SafeTerminalCategory::Success);
        let after = controller.snapshot();
        assert_eq!(after.state, armed.state);
        assert_eq!(after.capture_ref, armed.capture_ref);
        assert_eq!(after.record_count, armed.record_count);
        assert_eq!(after.byte_bucket, armed.byte_bucket);
        assert_eq!(after.completeness, armed.completeness);
        assert_eq!(after.terminal_category, armed.terminal_category);
        assert_eq!(after.dropped_records, armed.dropped_records);
        assert!(
            after.remaining_seconds <= armed.remaining_seconds,
            "replay must not extend or claim the interactive arm"
        );
        let capture = service.store.read_private(child_ref, &passphrase).unwrap();
        assert_eq!(capture.purpose(), CapturePurpose::Replay);
        assert_eq!(capture.completeness(), CaptureCompleteness::Complete);
        assert_eq!(capture.records().len(), 2);
        assert_eq!(
            capture.records()[0].transport_kind(),
            TransportKind::OfflineReplay
        );
        assert_eq!(
            capture.records()[1].kind(),
            CaptureRecordKind::TerminalOutcome
        );
    }

    #[test]
    fn safe_reference_collision_is_rejected_before_replay_runs() {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path().join("vault");
        let limits = CaptureLimits::default();
        let (store, _) =
            CaptureStore::with_clock(&root, limits, Arc::new(FixedStoreClock)).unwrap();
        let passphrase = CapturePassphrase::new("collision passphrase".to_owned()).unwrap();
        let existing = CaptureRef::from_bytes([7; 16]);
        let result = run_offline_replay(&recipe(), existing, &Offline).unwrap();
        let service = ReplayArtifactService::with_sources(
            store,
            Arc::new(SequenceRefs(Mutex::new(vec![
                CaptureRef::from_bytes([
                    7, 7, 7, 7, 8, 8, 8, 8, 8, 8, 8, 8, 8, 8, 8, 8,
                ]);
                CHILD_REFERENCE_ATTEMPTS
            ]))),
            Arc::new(CountingClock(AtomicUsize::new(0))),
        );
        service
            .persist_result(
                &result,
                SafeOperationRef::from_bytes([1; 4]),
                1_100,
                1_101,
                &passphrase,
            )
            .unwrap();
        assert!(matches!(
            service.allocate_child_ref(recipe().parent_capture_ref()),
            Err(ReplayFacadeError::ChildReferenceCollision)
        ));
    }
}
