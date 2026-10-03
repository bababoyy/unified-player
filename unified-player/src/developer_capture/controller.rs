use std::{
    fmt,
    sync::Arc,
    time::{Duration, Instant},
};

use parking_lot::Mutex;
use tokio::sync::watch;

use super::model::{
    CaptureByteBucket, CaptureCompleteness, CaptureLimits, CapturePurpose, CaptureRecordKind,
    CaptureRef, IncompleteReason, SafeCaptureSnapshot, SafeCaptureState, SafeOperationRef,
    SafeTerminalCategory, TERMINAL_RECORD_RESERVE_BYTES,
};

pub(crate) trait CaptureClock: Send + Sync {
    fn monotonic_now(&self) -> Duration;
}

pub(crate) trait CaptureRefSource: Send + Sync {
    fn next_ref(&self) -> CaptureRef;
}

#[derive(Debug)]
pub(crate) struct SystemCaptureClock {
    epoch: Instant,
}

impl Default for SystemCaptureClock {
    fn default() -> Self {
        Self {
            epoch: Instant::now(),
        }
    }
}

impl CaptureClock for SystemCaptureClock {
    fn monotonic_now(&self) -> Duration {
        self.epoch.elapsed()
    }
}

#[derive(Debug, Default)]
pub(crate) struct RandomCaptureRefSource;

impl CaptureRefSource for RandomCaptureRefSource {
    fn next_ref(&self) -> CaptureRef {
        CaptureRef::from_bytes(rand::random())
    }
}

#[derive(Clone)]
pub(crate) struct CaptureController {
    clock: Arc<dyn CaptureClock>,
    refs: Arc<dyn CaptureRefSource>,
    limits: CaptureLimits,
    inner: Arc<Mutex<ControllerInner>>,
    snapshot_tx: watch::Sender<SafeCaptureSnapshot>,
}

impl fmt::Debug for CaptureController {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("CaptureController")
            .field("snapshot", &self.snapshot())
            .finish_non_exhaustive()
    }
}

impl CaptureController {
    pub(crate) fn new(limits: CaptureLimits) -> Result<Self, ControllerError> {
        Self::with_sources(
            limits,
            Arc::new(SystemCaptureClock::default()),
            Arc::new(RandomCaptureRefSource),
        )
    }

    pub(crate) fn with_sources(
        limits: CaptureLimits,
        clock: Arc<dyn CaptureClock>,
        refs: Arc<dyn CaptureRefSource>,
    ) -> Result<Self, ControllerError> {
        let limits = limits
            .validate()
            .map_err(|_| ControllerError::InvalidLimits)?;
        let initial = SafeCaptureSnapshot::default();
        let (snapshot_tx, _) = watch::channel(initial);
        Ok(Self {
            clock,
            refs,
            limits,
            inner: Arc::new(Mutex::new(ControllerInner::default())),
            snapshot_tx,
        })
    }

    pub(crate) fn subscribe(&self) -> watch::Receiver<SafeCaptureSnapshot> {
        self.snapshot_tx.subscribe()
    }

    pub(crate) fn snapshot(&self) -> SafeCaptureSnapshot {
        let now = self.clock.monotonic_now();
        let mut inner = self.inner.lock();
        expire_if_needed(&mut inner, now);
        let snapshot = inner.safe_snapshot(now);
        self.snapshot_tx.send_replace(snapshot);
        snapshot
    }

    pub(crate) fn request_arm(&self) -> Result<(), ControllerError> {
        self.transition(|inner, _| {
            if !inner.state.may_request_arm() {
                return Err(ControllerError::Busy);
            }
            inner.reset_progress();
            inner.state = ControllerState::ConsentRequired;
            Ok(())
        })
    }

    pub(crate) fn accept_consent(&self) -> Result<super::model::SafeCaptureRef, ControllerError> {
        let capture_ref = self.refs.next_ref();
        self.transition(|inner, now| {
            if inner.state != ControllerState::ConsentRequired {
                return Err(ControllerError::InvalidState);
            }
            inner.state = ControllerState::Armed {
                capture_ref,
                deadline: now.saturating_add(self.limits.arm_deadline),
            };
            Ok(capture_ref.safe())
        })
    }

    pub(crate) fn cancel_consent(&self) -> TransitionOutcome {
        self.transition_infallible(|inner, _| {
            if inner.state == ControllerState::ConsentRequired {
                inner.reset();
                TransitionOutcome::Applied
            } else {
                TransitionOutcome::AlreadyApplied
            }
        })
    }

    pub(crate) fn disarm(&self) -> TransitionOutcome {
        self.transition_infallible(|inner, _| {
            if matches!(inner.state, ControllerState::Armed { .. }) {
                inner.reset();
                TransitionOutcome::Applied
            } else {
                TransitionOutcome::AlreadyApplied
            }
        })
    }

    pub(crate) fn claim(
        &self,
        purpose: CapturePurpose,
        operation_ref: SafeOperationRef,
    ) -> Result<Option<CapturePermit>, ControllerError> {
        self.transition(|inner, now| {
            expire_if_needed(inner, now);
            if !purpose.may_claim_interactive() {
                return Ok(None);
            }
            let ControllerState::Armed {
                capture_ref,
                deadline: _,
            } = inner.state
            else {
                return Ok(None);
            };
            inner.state = ControllerState::Claimed {
                capture_ref,
                operation_ref,
                deadline: now.saturating_add(self.limits.operation_deadline),
            };
            Ok(Some(CapturePermit {
                capture_ref,
                operation_ref,
                purpose,
                limits: self.limits,
            }))
        })
    }

    pub(crate) fn start_capturing(
        &self,
        permit: &CapturePermit,
    ) -> Result<TransitionOutcome, ControllerError> {
        self.transition(|inner, now| {
            expire_if_needed(inner, now);
            match inner.state {
                ControllerState::Claimed {
                    capture_ref,
                    operation_ref,
                    deadline,
                } if permit.matches(capture_ref, operation_ref) => {
                    inner.state = ControllerState::Capturing {
                        capture_ref,
                        operation_ref,
                        deadline,
                    };
                    Ok(TransitionOutcome::Applied)
                }
                ControllerState::Capturing {
                    capture_ref,
                    operation_ref,
                    ..
                } if permit.matches(capture_ref, operation_ref) => {
                    Ok(TransitionOutcome::AlreadyApplied)
                }
                _ if inner.state.capture_ref() == Some(permit.capture_ref) => {
                    Err(ControllerError::InvalidState)
                }
                _ => Err(ControllerError::StalePermit),
            }
        })
    }

    pub(crate) fn account_record(
        &self,
        permit: &CapturePermit,
        kind: CaptureRecordKind,
        bytes: u64,
    ) -> Result<RecordAdmission, ControllerError> {
        self.account_record_if_enqueued(permit, kind, bytes, || RecordEnqueue::Enqueued)
    }

    pub(crate) fn account_record_if_enqueued(
        &self,
        permit: &CapturePermit,
        kind: CaptureRecordKind,
        bytes: u64,
        enqueue: impl FnOnce() -> RecordEnqueue,
    ) -> Result<RecordAdmission, ControllerError> {
        self.transition(|inner, now| {
            expire_if_needed(inner, now);
            if matches!(inner.state, ControllerState::Claimed { .. }) {
                let _ = transition_claimed_to_capturing(inner, permit)?;
            }
            if !inner.state.matches_permit(permit) {
                return if inner.state.capture_ref() == Some(permit.capture_ref) {
                    Err(ControllerError::InvalidState)
                } else {
                    Err(ControllerError::StalePermit)
                };
            }

            let next_records = inner.record_count.checked_add(1);
            let next_exchanges = if kind.starts_provider_exchange() {
                inner.exchange_count.checked_add(1)
            } else {
                Some(inner.exchange_count)
            };
            let next_bytes = inner.plaintext_bytes.checked_add(bytes);
            let terminal_record = kind == CaptureRecordKind::TerminalOutcome;
            let record_limit = if terminal_record {
                self.limits.record_capacity.saturating_add(1)
            } else {
                self.limits.record_capacity
            };
            let plaintext_limit = if terminal_record {
                self.limits
                    .plaintext_bytes
                    .saturating_add(TERMINAL_RECORD_RESERVE_BYTES)
            } else {
                self.limits.plaintext_bytes
            };
            let reason = if bytes > self.limits.record_bytes(kind) {
                Some(IncompleteReason::RecordSize)
            } else if next_records.is_none_or(|count| count > record_limit) {
                Some(IncompleteReason::RecordCapacity)
            } else if next_exchanges.is_none_or(|count| count > self.limits.exchange_capacity) {
                Some(IncompleteReason::ExchangeCapacity)
            } else if next_bytes.is_none_or(|count| count > plaintext_limit) {
                Some(IncompleteReason::PlaintextBudget)
            } else {
                None
            };

            if let Some(reason) = reason {
                inner.mark_incomplete(reason);
                inner.dropped_records = inner
                    .dropped_records
                    .checked_add(1)
                    .ok_or(ControllerError::CounterOverflow)?;
                return Ok(RecordAdmission::Rejected(reason));
            }
            let enqueue_failure = match enqueue() {
                RecordEnqueue::Enqueued => None,
                RecordEnqueue::Full => Some(IncompleteReason::QueueCapacity),
                RecordEnqueue::Disconnected => Some(IncompleteReason::WriterUnavailable),
            };
            if let Some(reason) = enqueue_failure {
                inner.mark_incomplete(reason);
                inner.dropped_records = inner
                    .dropped_records
                    .checked_add(1)
                    .ok_or(ControllerError::CounterOverflow)?;
                if reason == IncompleteReason::WriterUnavailable {
                    inner.completeness = CaptureCompleteness::Incomplete;
                    inner.state = ControllerState::Failed {
                        capture_ref: permit.capture_ref,
                    };
                }
                return Ok(RecordAdmission::Rejected(reason));
            }
            inner.record_count = next_records.ok_or(ControllerError::CounterOverflow)?;
            inner.exchange_count = next_exchanges.ok_or(ControllerError::CounterOverflow)?;
            inner.plaintext_bytes = next_bytes.ok_or(ControllerError::CounterOverflow)?;
            Ok(RecordAdmission::Accepted)
        })
    }

    pub(crate) fn note_incomplete(
        &self,
        permit: &CapturePermit,
        reason: IncompleteReason,
        dropped_record: bool,
    ) -> Result<(), ControllerError> {
        self.transition(|inner, now| {
            expire_if_needed(inner, now);
            if !inner.state.matches_permit(permit) {
                return if inner.state.capture_ref() == Some(permit.capture_ref) {
                    Err(ControllerError::InvalidState)
                } else {
                    Err(ControllerError::StalePermit)
                };
            }
            inner.mark_incomplete(reason);
            if dropped_record {
                inner.dropped_records = inner
                    .dropped_records
                    .checked_add(1)
                    .ok_or(ControllerError::CounterOverflow)?;
            }
            Ok(())
        })
    }

    pub(crate) fn begin_finalization(
        &self,
        permit: &CapturePermit,
        terminal_category: SafeTerminalCategory,
    ) -> Result<TransitionOutcome, ControllerError> {
        self.transition(|inner, now| {
            expire_if_needed(inner, now);
            match inner.state {
                ControllerState::Claimed {
                    capture_ref,
                    operation_ref,
                    ..
                }
                | ControllerState::Capturing {
                    capture_ref,
                    operation_ref,
                    ..
                } if permit.matches(capture_ref, operation_ref) => {
                    inner.terminal_category = Some(terminal_category);
                    inner.state = ControllerState::Finalizing {
                        capture_ref,
                        operation_ref,
                        deadline: now.saturating_add(self.limits.writer_finalization),
                    };
                    Ok(TransitionOutcome::Applied)
                }
                ControllerState::Finalizing {
                    capture_ref,
                    operation_ref,
                    ..
                } if permit.matches(capture_ref, operation_ref)
                    && inner.terminal_category == Some(terminal_category) =>
                {
                    Ok(TransitionOutcome::AlreadyApplied)
                }
                _ if inner.state.is_terminal()
                    && inner.state.capture_ref() == Some(permit.capture_ref) =>
                {
                    Ok(TransitionOutcome::AlreadyApplied)
                }
                _ if inner.state.capture_ref() == Some(permit.capture_ref) => {
                    Err(ControllerError::InvalidState)
                }
                _ => Err(ControllerError::StalePermit),
            }
        })
    }

    pub(crate) fn finish_finalization(
        &self,
        permit: &CapturePermit,
        persisted: bool,
    ) -> Result<TransitionOutcome, ControllerError> {
        self.transition(|inner, now| {
            expire_if_needed(inner, now);
            match inner.state {
                ControllerState::Finalizing {
                    capture_ref,
                    operation_ref,
                    ..
                } if permit.matches(capture_ref, operation_ref) => {
                    if !persisted {
                        inner.mark_incomplete(IncompleteReason::Persistence);
                    }
                    if inner.exchange_count == 0 {
                        inner.mark_incomplete(IncompleteReason::MissingProviderExchange);
                    }
                    inner.state = if inner.incomplete_reasons.is_empty() {
                        inner.completeness = CaptureCompleteness::Complete;
                        ControllerState::Ready { capture_ref }
                    } else {
                        inner.completeness = CaptureCompleteness::Incomplete;
                        ControllerState::Incomplete { capture_ref }
                    };
                    Ok(TransitionOutcome::Applied)
                }
                _ if inner.state.is_terminal()
                    && inner.state.capture_ref() == Some(permit.capture_ref) =>
                {
                    Ok(TransitionOutcome::AlreadyApplied)
                }
                _ if inner.state.capture_ref() == Some(permit.capture_ref) => {
                    Err(ControllerError::InvalidState)
                }
                _ => Err(ControllerError::StalePermit),
            }
        })
    }

    pub(crate) fn fail(&self, permit: &CapturePermit) -> TransitionOutcome {
        self.transition_infallible(|inner, _| {
            if inner.state.matches_permit(permit) {
                inner.mark_incomplete(IncompleteReason::Persistence);
                inner.completeness = CaptureCompleteness::Incomplete;
                inner.state = ControllerState::Failed {
                    capture_ref: permit.capture_ref,
                };
                TransitionOutcome::Applied
            } else {
                TransitionOutcome::AlreadyApplied
            }
        })
    }

    pub(crate) fn writer_unavailable(&self) -> TransitionOutcome {
        self.transition_infallible(|inner, _| {
            let Some(capture_ref) = inner.state.capture_ref() else {
                return TransitionOutcome::AlreadyApplied;
            };
            if !matches!(
                inner.state,
                ControllerState::Armed { .. }
                    | ControllerState::Claimed { .. }
                    | ControllerState::Capturing { .. }
                    | ControllerState::Finalizing { .. }
            ) {
                return TransitionOutcome::AlreadyApplied;
            }
            inner.mark_incomplete(IncompleteReason::WriterUnavailable);
            inner.completeness = CaptureCompleteness::Incomplete;
            inner.state = ControllerState::Failed { capture_ref };
            TransitionOutcome::Applied
        })
    }

    pub(crate) fn poll_deadlines(&self) -> SafeCaptureSnapshot {
        self.snapshot()
    }

    fn transition<T>(
        &self,
        transition: impl FnOnce(&mut ControllerInner, Duration) -> Result<T, ControllerError>,
    ) -> Result<T, ControllerError> {
        let now = self.clock.monotonic_now();
        let mut inner = self.inner.lock();
        let result = transition(&mut inner, now);
        self.snapshot_tx.send_replace(inner.safe_snapshot(now));
        result
    }

    fn transition_infallible<T>(
        &self,
        transition: impl FnOnce(&mut ControllerInner, Duration) -> T,
    ) -> T {
        let now = self.clock.monotonic_now();
        let mut inner = self.inner.lock();
        let result = transition(&mut inner, now);
        self.snapshot_tx.send_replace(inner.safe_snapshot(now));
        result
    }
}

#[derive(Clone)]
pub(crate) struct CapturePermit {
    capture_ref: CaptureRef,
    operation_ref: SafeOperationRef,
    purpose: CapturePurpose,
    limits: CaptureLimits,
}

impl CapturePermit {
    pub(crate) const fn capture_ref(&self) -> CaptureRef {
        self.capture_ref
    }

    pub(crate) const fn operation_ref(&self) -> SafeOperationRef {
        self.operation_ref
    }

    pub(crate) const fn purpose(&self) -> CapturePurpose {
        self.purpose
    }

    pub(crate) const fn limits(&self) -> CaptureLimits {
        self.limits
    }

    fn matches(&self, capture_ref: CaptureRef, operation_ref: SafeOperationRef) -> bool {
        self.capture_ref == capture_ref && self.operation_ref == operation_ref
    }
}

impl fmt::Debug for CapturePermit {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("CapturePermit")
            .field("capture_ref", &self.capture_ref.safe())
            .field("operation_ref", &self.operation_ref)
            .field("purpose", &self.purpose)
            .finish_non_exhaustive()
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum TransitionOutcome {
    Applied,
    AlreadyApplied,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum RecordAdmission {
    Accepted,
    Rejected(IncompleteReason),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum RecordEnqueue {
    Enqueued,
    Full,
    Disconnected,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ControllerError {
    Busy,
    CounterOverflow,
    InvalidLimits,
    InvalidState,
    StalePermit,
}

impl fmt::Display for ControllerError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::Busy => "a private capture is already active",
            Self::CounterOverflow => "private capture accounting exceeded its representable limit",
            Self::InvalidLimits => "private capture limits are invalid",
            Self::InvalidState => "private capture is not in the required state",
            Self::StalePermit => "private capture permit is stale",
        })
    }
}

impl std::error::Error for ControllerError {}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ControllerState {
    Inactive,
    ConsentRequired,
    Armed {
        capture_ref: CaptureRef,
        deadline: Duration,
    },
    Claimed {
        capture_ref: CaptureRef,
        operation_ref: SafeOperationRef,
        deadline: Duration,
    },
    Capturing {
        capture_ref: CaptureRef,
        operation_ref: SafeOperationRef,
        deadline: Duration,
    },
    Finalizing {
        capture_ref: CaptureRef,
        operation_ref: SafeOperationRef,
        deadline: Duration,
    },
    Ready {
        capture_ref: CaptureRef,
    },
    Incomplete {
        capture_ref: CaptureRef,
    },
    Expired {
        capture_ref: CaptureRef,
    },
    Failed {
        capture_ref: CaptureRef,
    },
}

impl ControllerState {
    const fn safe(self) -> SafeCaptureState {
        match self {
            Self::Inactive => SafeCaptureState::Inactive,
            Self::ConsentRequired => SafeCaptureState::ConsentRequired,
            Self::Armed { .. } => SafeCaptureState::Armed,
            Self::Claimed { .. } => SafeCaptureState::Claimed,
            Self::Capturing { .. } => SafeCaptureState::Capturing,
            Self::Finalizing { .. } => SafeCaptureState::Finalizing,
            Self::Ready { .. } => SafeCaptureState::Ready,
            Self::Incomplete { .. } => SafeCaptureState::Incomplete,
            Self::Expired { .. } => SafeCaptureState::Expired,
            Self::Failed { .. } => SafeCaptureState::Failed,
        }
    }

    const fn capture_ref(self) -> Option<CaptureRef> {
        match self {
            Self::Inactive | Self::ConsentRequired => None,
            Self::Armed { capture_ref, .. }
            | Self::Claimed { capture_ref, .. }
            | Self::Capturing { capture_ref, .. }
            | Self::Finalizing { capture_ref, .. }
            | Self::Ready { capture_ref }
            | Self::Incomplete { capture_ref }
            | Self::Expired { capture_ref }
            | Self::Failed { capture_ref } => Some(capture_ref),
        }
    }

    const fn may_request_arm(self) -> bool {
        matches!(
            self,
            Self::Inactive
                | Self::Ready { .. }
                | Self::Incomplete { .. }
                | Self::Expired { .. }
                | Self::Failed { .. }
        )
    }

    fn matches_permit(self, permit: &CapturePermit) -> bool {
        match self {
            Self::Claimed {
                capture_ref,
                operation_ref,
                ..
            }
            | Self::Capturing {
                capture_ref,
                operation_ref,
                ..
            }
            | Self::Finalizing {
                capture_ref,
                operation_ref,
                ..
            } => permit.matches(capture_ref, operation_ref),
            _ => false,
        }
    }

    const fn is_terminal(self) -> bool {
        matches!(
            self,
            Self::Ready { .. }
                | Self::Incomplete { .. }
                | Self::Expired { .. }
                | Self::Failed { .. }
        )
    }
}

#[derive(Debug)]
struct ControllerInner {
    state: ControllerState,
    record_count: u16,
    exchange_count: u16,
    plaintext_bytes: u64,
    dropped_records: u16,
    completeness: CaptureCompleteness,
    incomplete_reasons: Vec<IncompleteReason>,
    terminal_category: Option<SafeTerminalCategory>,
}

impl Default for ControllerInner {
    fn default() -> Self {
        Self {
            state: ControllerState::Inactive,
            record_count: 0,
            exchange_count: 0,
            plaintext_bytes: 0,
            dropped_records: 0,
            completeness: CaptureCompleteness::Pending,
            incomplete_reasons: Vec::new(),
            terminal_category: None,
        }
    }
}

impl ControllerInner {
    fn reset_progress(&mut self) {
        self.record_count = 0;
        self.exchange_count = 0;
        self.plaintext_bytes = 0;
        self.dropped_records = 0;
        self.completeness = CaptureCompleteness::Pending;
        self.incomplete_reasons.clear();
        self.terminal_category = None;
    }

    fn reset(&mut self) {
        self.state = ControllerState::Inactive;
        self.reset_progress();
    }

    fn mark_incomplete(&mut self, reason: IncompleteReason) {
        self.completeness = CaptureCompleteness::Incomplete;
        if !self.incomplete_reasons.contains(&reason) {
            self.incomplete_reasons.push(reason);
        }
    }

    fn safe_snapshot(&self, now: Duration) -> SafeCaptureSnapshot {
        let remaining_seconds = match self.state {
            ControllerState::Armed { deadline, .. }
            | ControllerState::Claimed { deadline, .. }
            | ControllerState::Capturing { deadline, .. }
            | ControllerState::Finalizing { deadline, .. } => {
                Some(ceil_seconds(deadline.saturating_sub(now)))
            }
            _ => None,
        };
        SafeCaptureSnapshot {
            state: self.state.safe(),
            capture_ref: self.state.capture_ref().map(CaptureRef::safe),
            remaining_seconds,
            record_count: self.record_count,
            byte_bucket: CaptureByteBucket::from_bytes(self.plaintext_bytes),
            completeness: self.completeness,
            terminal_category: self.terminal_category,
            dropped_records: self.dropped_records,
        }
    }
}

fn transition_claimed_to_capturing(
    inner: &mut ControllerInner,
    permit: &CapturePermit,
) -> Result<TransitionOutcome, ControllerError> {
    let ControllerState::Claimed {
        capture_ref,
        operation_ref,
        deadline,
    } = inner.state
    else {
        return Err(ControllerError::InvalidState);
    };
    if !permit.matches(capture_ref, operation_ref) {
        return Err(ControllerError::StalePermit);
    }
    inner.state = ControllerState::Capturing {
        capture_ref,
        operation_ref,
        deadline,
    };
    Ok(TransitionOutcome::Applied)
}

fn expire_if_needed(inner: &mut ControllerInner, now: Duration) {
    match inner.state {
        ControllerState::Armed {
            capture_ref,
            deadline,
        } if now >= deadline => {
            inner.state = ControllerState::Expired { capture_ref };
        }
        ControllerState::Claimed {
            capture_ref,
            deadline,
            ..
        }
        | ControllerState::Capturing {
            capture_ref,
            deadline,
            ..
        } if now >= deadline => {
            inner.mark_incomplete(IncompleteReason::OperationDeadline);
            inner.terminal_category = Some(SafeTerminalCategory::TimedOut);
            inner.state = ControllerState::Incomplete { capture_ref };
        }
        ControllerState::Finalizing {
            capture_ref,
            deadline,
            ..
        } if now >= deadline => {
            inner.mark_incomplete(IncompleteReason::FinalizationDeadline);
            inner.state = ControllerState::Incomplete { capture_ref };
        }
        _ => {}
    }
}

fn ceil_seconds(duration: Duration) -> u64 {
    duration
        .as_secs()
        .saturating_add(u64::from(duration.subsec_nanos() > 0))
}

#[cfg(test)]
mod tests {
    use super::{
        CaptureClock, CaptureController, CaptureRefSource, ControllerError, RecordAdmission,
        TransitionOutcome,
    };
    use crate::developer_capture::model::{
        CaptureCompleteness, CaptureLimits, CapturePurpose, CaptureRecordKind, CaptureRef,
        IncompleteReason, SafeCaptureState, SafeOperationRef, SafeTerminalCategory,
    };
    use std::sync::{
        atomic::{AtomicU64, AtomicUsize, Ordering},
        Arc, Barrier,
    };
    use std::time::Duration;

    #[derive(Debug, Default)]
    struct FakeClock(AtomicU64);

    impl FakeClock {
        fn advance(&self, duration: Duration) {
            self.0.fetch_add(
                u64::try_from(duration.as_millis()).unwrap(),
                Ordering::SeqCst,
            );
        }
    }

    impl CaptureClock for FakeClock {
        fn monotonic_now(&self) -> Duration {
            Duration::from_millis(self.0.load(Ordering::SeqCst))
        }
    }

    #[derive(Debug, Default)]
    struct FakeRefs(AtomicUsize);

    impl CaptureRefSource for FakeRefs {
        fn next_ref(&self) -> CaptureRef {
            let next = self.0.fetch_add(1, Ordering::SeqCst) + 1;
            let mut bytes = [0_u8; 16];
            bytes[..8].copy_from_slice(&u64::try_from(next).unwrap().to_be_bytes());
            CaptureRef::from_bytes(bytes)
        }
    }

    fn fixture() -> (CaptureController, Arc<FakeClock>) {
        let clock = Arc::new(FakeClock::default());
        let controller = CaptureController::with_sources(
            CaptureLimits::default(),
            clock.clone(),
            Arc::new(FakeRefs::default()),
        )
        .unwrap();
        (controller, clock)
    }

    fn arm_and_claim(controller: &CaptureController) -> super::CapturePermit {
        controller.request_arm().unwrap();
        controller.accept_consent().unwrap();
        controller
            .claim(
                CapturePurpose::InteractivePlayback,
                SafeOperationRef::from_bytes([1, 2, 3, 4]),
            )
            .unwrap()
            .unwrap()
    }

    #[test]
    fn consent_arm_claim_capture_and_finalize_are_ordered() {
        let (controller, _) = fixture();
        let mut snapshots = controller.subscribe();
        controller.request_arm().unwrap();
        assert_eq!(
            snapshots.borrow_and_update().state,
            SafeCaptureState::ConsentRequired
        );
        controller.accept_consent().unwrap();
        assert_eq!(snapshots.borrow_and_update().state, SafeCaptureState::Armed);
        let permit = controller
            .claim(
                CapturePurpose::InteractivePlayback,
                SafeOperationRef::from_bytes([1, 2, 3, 4]),
            )
            .unwrap()
            .unwrap();
        assert_eq!(controller.snapshot().state, SafeCaptureState::Claimed);
        assert_eq!(
            controller.start_capturing(&permit).unwrap(),
            TransitionOutcome::Applied
        );
        assert_eq!(
            controller
                .account_record(&permit, CaptureRecordKind::HttpRequest, 1_024)
                .unwrap(),
            RecordAdmission::Accepted
        );
        controller
            .begin_finalization(&permit, SafeTerminalCategory::Success)
            .unwrap();
        controller.finish_finalization(&permit, true).unwrap();
        let snapshot = controller.snapshot();
        assert_eq!(snapshot.state, SafeCaptureState::Ready);
        assert_eq!(snapshot.completeness, CaptureCompleteness::Complete);
        assert_eq!(snapshot.record_count, 1);
        assert_eq!(
            snapshot.terminal_category,
            Some(SafeTerminalCategory::Success)
        );
    }

    #[test]
    fn cancellation_and_disarm_are_idempotent() {
        let (controller, _) = fixture();
        controller.request_arm().unwrap();
        assert_eq!(controller.cancel_consent(), TransitionOutcome::Applied);
        assert_eq!(
            controller.cancel_consent(),
            TransitionOutcome::AlreadyApplied
        );
        controller.request_arm().unwrap();
        controller.accept_consent().unwrap();
        assert_eq!(controller.disarm(), TransitionOutcome::Applied);
        assert_eq!(controller.disarm(), TransitionOutcome::AlreadyApplied);
        assert_eq!(controller.snapshot().state, SafeCaptureState::Inactive);
    }

    #[test]
    fn background_purposes_do_not_consume_the_one_shot_permit() {
        let (controller, _) = fixture();
        controller.request_arm().unwrap();
        controller.accept_consent().unwrap();
        for purpose in [
            CapturePurpose::Prefetch,
            CapturePurpose::Resume,
            CapturePurpose::Probe,
            CapturePurpose::Replay,
        ] {
            assert!(controller
                .claim(purpose, SafeOperationRef::from_bytes([0; 4]))
                .unwrap()
                .is_none());
            assert_eq!(controller.snapshot().state, SafeCaptureState::Armed);
        }
        assert!(controller
            .claim(
                CapturePurpose::InteractivePlayback,
                SafeOperationRef::from_bytes([1; 4]),
            )
            .unwrap()
            .is_some());
    }

    #[test]
    fn arm_and_operation_deadlines_are_deterministic() {
        let (controller, clock) = fixture();
        controller.request_arm().unwrap();
        controller.accept_consent().unwrap();
        clock.advance(Duration::from_secs(60));
        assert_eq!(controller.poll_deadlines().state, SafeCaptureState::Expired);

        let permit = arm_and_claim(&controller);
        clock.advance(Duration::from_secs(60));
        let snapshot = controller.poll_deadlines();
        assert_eq!(snapshot.state, SafeCaptureState::Incomplete);
        assert_eq!(snapshot.completeness, CaptureCompleteness::Incomplete);
        assert_eq!(
            snapshot.terminal_category,
            Some(SafeTerminalCategory::TimedOut)
        );
        assert_eq!(
            controller.start_capturing(&permit),
            Err(ControllerError::InvalidState)
        );
    }

    #[test]
    fn writer_finalization_deadline_is_enforced() {
        let (controller, clock) = fixture();
        let permit = arm_and_claim(&controller);
        assert_eq!(
            controller
                .account_record(&permit, CaptureRecordKind::HttpRequest, 1)
                .unwrap(),
            RecordAdmission::Accepted
        );
        controller
            .begin_finalization(&permit, SafeTerminalCategory::Success)
            .unwrap();
        assert_eq!(controller.snapshot().state, SafeCaptureState::Finalizing);
        clock.advance(CaptureLimits::default().writer_finalization);

        let snapshot = controller.poll_deadlines();
        assert_eq!(snapshot.state, SafeCaptureState::Incomplete);
        assert_eq!(snapshot.completeness, CaptureCompleteness::Incomplete);
        assert!(controller
            .inner
            .lock()
            .incomplete_reasons
            .contains(&IncompleteReason::FinalizationDeadline));
        assert_eq!(
            controller.finish_finalization(&permit, true).unwrap(),
            TransitionOutcome::AlreadyApplied
        );
    }

    #[test]
    fn non_provider_records_cannot_satisfy_complete_evidence() {
        for with_boundary in [false, true] {
            let (controller, _) = fixture();
            let permit = arm_and_claim(&controller);
            if with_boundary {
                assert_eq!(
                    controller
                        .account_record(&permit, CaptureRecordKind::OperationBoundary, 1)
                        .unwrap(),
                    RecordAdmission::Accepted
                );
            }
            controller
                .begin_finalization(&permit, SafeTerminalCategory::Failed)
                .unwrap();
            controller.finish_finalization(&permit, true).unwrap();

            let snapshot = controller.snapshot();
            assert_eq!(snapshot.state, SafeCaptureState::Incomplete);
            assert_eq!(snapshot.completeness, CaptureCompleteness::Incomplete);
            assert!(controller
                .inner
                .lock()
                .incomplete_reasons
                .contains(&IncompleteReason::MissingProviderExchange));
        }
    }

    #[test]
    fn typed_record_and_aggregate_boundaries_are_checked() {
        let limits = CaptureLimits {
            record_capacity: 3,
            exchange_capacity: 2,
            player_request_bytes: 4,
            player_response_bytes: 4,
            transport_error_bytes: 2,
            plaintext_bytes: 8,
            ..CaptureLimits::default()
        };
        let controller = CaptureController::with_sources(
            limits,
            Arc::new(FakeClock::default()),
            Arc::new(FakeRefs::default()),
        )
        .unwrap();
        let permit = arm_and_claim(&controller);
        assert_eq!(
            controller
                .account_record(&permit, CaptureRecordKind::HttpRequest, 4)
                .unwrap(),
            RecordAdmission::Accepted
        );
        assert_eq!(
            controller
                .account_record(&permit, CaptureRecordKind::HttpResponse, 4)
                .unwrap(),
            RecordAdmission::Accepted
        );
        assert_eq!(
            controller
                .account_record(&permit, CaptureRecordKind::NetworkFailure, 3)
                .unwrap(),
            RecordAdmission::Rejected(IncompleteReason::RecordSize)
        );

        controller.inner.lock().dropped_records = u16::MAX;
        assert_eq!(
            controller.account_record(&permit, CaptureRecordKind::HttpResponse, 1),
            Err(ControllerError::CounterOverflow)
        );
    }

    #[test]
    fn only_one_concurrent_operation_claims_the_capture() {
        let (controller, _) = fixture();
        controller.request_arm().unwrap();
        controller.accept_consent().unwrap();
        let barrier = Arc::new(Barrier::new(9));
        let winners = Arc::new(AtomicUsize::new(0));
        let mut threads = Vec::new();
        for index in 0..8_u8 {
            let controller = controller.clone();
            let barrier = barrier.clone();
            let winners = winners.clone();
            threads.push(std::thread::spawn(move || {
                barrier.wait();
                if controller
                    .claim(
                        CapturePurpose::InteractivePlayback,
                        SafeOperationRef::from_bytes([index; 4]),
                    )
                    .unwrap()
                    .is_some()
                {
                    winners.fetch_add(1, Ordering::SeqCst);
                }
            }));
        }
        barrier.wait();
        for thread in threads {
            thread.join().unwrap();
        }
        assert_eq!(winners.load(Ordering::SeqCst), 1);
        assert_eq!(controller.snapshot().state, SafeCaptureState::Claimed);
    }

    #[test]
    fn bounds_reject_records_without_blocking_terminal_finalization() {
        let limits = CaptureLimits {
            record_capacity: 1,
            exchange_capacity: 1,
            player_request_bytes: 4,
            player_response_bytes: 4,
            transport_error_bytes: 4,
            plaintext_bytes: 8,
            ..CaptureLimits::default()
        };
        let controller = CaptureController::with_sources(
            limits,
            Arc::new(FakeClock::default()),
            Arc::new(FakeRefs::default()),
        )
        .unwrap();
        let permit = arm_and_claim(&controller);
        assert_eq!(
            controller
                .account_record(&permit, CaptureRecordKind::HttpRequest, 4)
                .unwrap(),
            RecordAdmission::Accepted
        );
        assert_eq!(
            controller
                .account_record(&permit, CaptureRecordKind::OperationBoundary, 4)
                .unwrap(),
            RecordAdmission::Rejected(IncompleteReason::RecordCapacity)
        );
        controller
            .begin_finalization(&permit, SafeTerminalCategory::Failed)
            .unwrap();
        controller.finish_finalization(&permit, true).unwrap();
        let snapshot = controller.snapshot();
        assert_eq!(snapshot.state, SafeCaptureState::Incomplete);
        assert_eq!(snapshot.record_count, 1);
        assert_eq!(snapshot.dropped_records, 1);
    }

    #[test]
    fn terminal_calls_are_idempotent_and_stale_permits_are_rejected() {
        let (controller, _) = fixture();
        let permit = arm_and_claim(&controller);
        assert_eq!(
            controller
                .begin_finalization(&permit, SafeTerminalCategory::Cancelled)
                .unwrap(),
            TransitionOutcome::Applied
        );
        assert_eq!(
            controller
                .begin_finalization(&permit, SafeTerminalCategory::Cancelled)
                .unwrap(),
            TransitionOutcome::AlreadyApplied
        );
        assert_eq!(
            controller.finish_finalization(&permit, true).unwrap(),
            TransitionOutcome::Applied
        );
        assert_eq!(
            controller.finish_finalization(&permit, true).unwrap(),
            TransitionOutcome::AlreadyApplied
        );

        controller.request_arm().unwrap();
        controller.accept_consent().unwrap();
        assert_eq!(
            controller.start_capturing(&permit),
            Err(ControllerError::StalePermit)
        );
    }
}
