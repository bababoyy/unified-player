use std::{
    fmt,
    path::Path,
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc,
    },
    time::{Duration, Instant, SystemTime},
};

use parking_lot::Mutex;
use tokio::sync::watch;
use tokio_util::sync::CancellationToken;

use super::{
    controller::{
        CaptureController, CapturePermit, ControllerError, RecordAdmission, RecordEnqueue,
        TransitionOutcome,
    },
    model::{
        CaptureCompleteness, CaptureLimits, CapturePurpose, CaptureRecordKind, CaptureRecordV1,
        CaptureRef, EndpointRole, ExchangeRef, IncompleteReason, PrivateCaptureV1,
        ProviderClientKind, SafeCaptureRef, SafeCaptureSnapshot, SafeCaptureState,
        SafeOperationRef, SafeTerminalCategory, SensitiveBytes, TransportKind,
    },
    security::CapturePassphrase,
    store::{CaptureStore, MaintenanceReport, PendingCaptureWrite, StoreError},
};

const WORKER_POLL_INTERVAL: Duration = Duration::from_millis(25);

pub(crate) fn prepare_runtime(
    root: impl AsRef<Path>,
    limits: CaptureLimits,
) -> Result<(CaptureHandle, CaptureWorker, MaintenanceReport), CaptureRuntimeError> {
    prepare_runtime_with_capacity(
        root,
        limits,
        usize::from(limits.record_capacity).saturating_add(2),
    )
}

fn prepare_runtime_with_capacity(
    root: impl AsRef<Path>,
    limits: CaptureLimits,
    queue_capacity: usize,
) -> Result<(CaptureHandle, CaptureWorker, MaintenanceReport), CaptureRuntimeError> {
    let limits = limits
        .validate()
        .map_err(|_| CaptureRuntimeError::InvalidLimits)?;
    if queue_capacity == 0 {
        return Err(CaptureRuntimeError::InvalidLimits);
    }
    let controller = CaptureController::new(limits).map_err(CaptureRuntimeError::Controller)?;
    let pending_passphrase = Arc::new(Mutex::new(None));
    let (sender, receiver) = flume::bounded(queue_capacity);
    let (store, maintenance) =
        CaptureStore::open(root, limits).map_err(CaptureRuntimeError::Store)?;
    let handle = CaptureHandle {
        controller: controller.clone(),
        sender,
        pending_passphrase: pending_passphrase.clone(),
    };
    let worker = CaptureWorker {
        controller,
        receiver,
        pending_passphrase,
        store,
        limits,
    };
    Ok((handle, worker, maintenance))
}

#[derive(Clone)]
pub(crate) struct CaptureHandle {
    controller: CaptureController,
    sender: flume::Sender<WriterCommand>,
    pending_passphrase: Arc<Mutex<Option<CapturePassphrase>>>,
}

impl fmt::Debug for CaptureHandle {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("CaptureHandle")
            .field("snapshot", &self.snapshot())
            .finish_non_exhaustive()
    }
}

impl CaptureHandle {
    pub(crate) fn subscribe(&self) -> watch::Receiver<SafeCaptureSnapshot> {
        self.controller.subscribe()
    }

    pub(crate) fn snapshot(&self) -> SafeCaptureSnapshot {
        self.controller.snapshot()
    }

    pub(crate) fn request_arm(&self) -> Result<(), CaptureRuntimeError> {
        self.controller
            .request_arm()
            .map_err(CaptureRuntimeError::Controller)
    }

    pub(crate) fn accept_consent(
        &self,
        passphrase: CapturePassphrase,
    ) -> Result<SafeCaptureRef, CaptureRuntimeError> {
        let mut pending = self.pending_passphrase.lock();
        if pending.is_some() {
            return Err(CaptureRuntimeError::Busy);
        }
        let capture_ref = self
            .controller
            .accept_consent()
            .map_err(CaptureRuntimeError::Controller)?;
        *pending = Some(passphrase);
        Ok(capture_ref)
    }

    pub(crate) fn cancel_consent(&self) -> TransitionOutcome {
        self.pending_passphrase.lock().take();
        self.controller.cancel_consent()
    }

    pub(crate) fn disarm(&self) -> TransitionOutcome {
        self.pending_passphrase.lock().take();
        self.controller.disarm()
    }

    pub(crate) fn claim(
        &self,
        purpose: CapturePurpose,
        operation_ref: SafeOperationRef,
    ) -> Result<Option<CaptureSession>, CaptureRuntimeError> {
        let mut pending = self.pending_passphrase.lock();
        let Some(permit) = self
            .controller
            .claim(purpose, operation_ref)
            .map_err(CaptureRuntimeError::Controller)?
        else {
            return Ok(None);
        };
        let Some(passphrase) = pending.take() else {
            self.controller.fail(&permit);
            return Err(CaptureRuntimeError::WriterUnavailable);
        };
        let accounting = Arc::new(Mutex::new(SessionAccounting::default()));
        let terminal_sent = Arc::new(AtomicBool::new(false));
        let started_at = Instant::now();
        let command = WriterCommand::Begin {
            permit: permit.clone(),
            passphrase,
            created_unix_ms: unix_ms(SystemTime::now()),
            accounting: accounting.clone(),
            terminal_sent: terminal_sent.clone(),
            started_at,
        };
        if self.sender.try_send(command).is_err() {
            self.controller.fail(&permit);
            return Err(CaptureRuntimeError::WriterUnavailable);
        }
        Ok(Some(CaptureSession {
            inner: Arc::new(CaptureSessionInner {
                permit,
                controller: self.controller.clone(),
                sender: self.sender.clone(),
                started_at,
                accounting,
                terminal_sent,
                send_gate: Mutex::new(()),
            }),
        }))
    }
}

#[derive(Clone)]
pub(crate) struct CaptureSession {
    inner: Arc<CaptureSessionInner>,
}

impl fmt::Debug for CaptureSession {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("CaptureSession")
            .field("capture_ref", &self.inner.permit.capture_ref().safe())
            .field("operation_ref", &self.inner.permit.operation_ref())
            .finish_non_exhaustive()
    }
}

impl CaptureSession {
    pub(crate) fn body_limit(&self, kind: CaptureRecordKind) -> usize {
        let limits = self.inner.permit.limits();
        let bytes = match kind {
            CaptureRecordKind::HttpRequest => limits.player_request_bytes,
            CaptureRecordKind::HttpResponse => limits.player_response_bytes,
            _ => limits.transport_error_bytes,
        };
        usize::try_from(bytes).unwrap_or(usize::MAX)
    }

    pub(crate) fn record(&self, kind: CaptureRecordKind, payload: SensitiveBytes) -> RecordOutcome {
        self.inner.record(
            None,
            EndpointRole::Unknown,
            ProviderClientKind::Unknown,
            TransportKind::Unknown,
            0,
            kind,
            payload,
        )
    }

    #[allow(clippy::too_many_arguments)]
    pub(crate) fn record_with_context(
        &self,
        exchange_ref: Option<ExchangeRef>,
        endpoint_role: EndpointRole,
        client_kind: ProviderClientKind,
        transport_kind: TransportKind,
        attempt: u8,
        kind: CaptureRecordKind,
        payload: SensitiveBytes,
    ) -> RecordOutcome {
        self.inner.record(
            exchange_ref,
            endpoint_role,
            client_kind,
            transport_kind,
            attempt,
            kind,
            payload,
        )
    }

    pub(crate) fn finish(&self, terminal: SafeTerminalCategory) -> TransitionOutcome {
        self.inner.finalize(None, terminal)
    }

    pub(crate) fn mark_credential_values_present(&self) {
        self.inner.accounting.lock().credential_values_present = true;
    }

    pub(crate) fn note_incomplete(&self, reason: IncompleteReason) -> TransitionOutcome {
        if self.inner.terminal_sent.load(Ordering::Acquire) {
            return TransitionOutcome::AlreadyApplied;
        }
        self.inner.accounting.lock().note_incomplete(reason, false);
        if self
            .inner
            .controller
            .note_incomplete(&self.inner.permit, reason, false)
            .is_ok()
        {
            TransitionOutcome::Applied
        } else {
            TransitionOutcome::AlreadyApplied
        }
    }

    #[cfg(test)]
    pub(crate) fn capture_ref(&self) -> CaptureRef {
        self.inner.permit.capture_ref()
    }
}

struct CaptureSessionInner {
    permit: CapturePermit,
    controller: CaptureController,
    sender: flume::Sender<WriterCommand>,
    started_at: Instant,
    accounting: Arc<Mutex<SessionAccounting>>,
    terminal_sent: Arc<AtomicBool>,
    send_gate: Mutex<()>,
}

impl CaptureSessionInner {
    #[allow(clippy::too_many_arguments)]
    fn record(
        &self,
        exchange_ref: Option<ExchangeRef>,
        endpoint_role: EndpointRole,
        client_kind: ProviderClientKind,
        transport_kind: TransportKind,
        attempt: u8,
        kind: CaptureRecordKind,
        payload: SensitiveBytes,
    ) -> RecordOutcome {
        let _send_guard = self.send_gate.lock();
        if self.terminal_sent.load(Ordering::Acquire) {
            return RecordOutcome::Inactive;
        }
        self.record_unchecked(
            exchange_ref,
            endpoint_role,
            client_kind,
            transport_kind,
            attempt,
            kind,
            payload,
        )
    }

    #[allow(clippy::too_many_arguments)]
    fn record_unchecked(
        &self,
        exchange_ref: Option<ExchangeRef>,
        endpoint_role: EndpointRole,
        client_kind: ProviderClientKind,
        transport_kind: TransportKind,
        attempt: u8,
        kind: CaptureRecordKind,
        payload: SensitiveBytes,
    ) -> RecordOutcome {
        let bytes = u64::try_from(payload.len()).unwrap_or(u64::MAX);
        let mut accounting = self.accounting.lock();
        let sequence = accounting.next_sequence;
        let record = CaptureRecordV1::with_context(
            sequence,
            u64::try_from(self.started_at.elapsed().as_millis()).unwrap_or(u64::MAX),
            exchange_ref,
            endpoint_role,
            client_kind,
            transport_kind,
            attempt,
            kind,
            payload,
        );
        let command = WriterCommand::Record {
            capture_ref: self.permit.capture_ref(),
            record,
        };
        match self
            .controller
            .account_record_if_enqueued(&self.permit, kind, bytes, || {
                match self.sender.try_send(command) {
                    Ok(()) => RecordEnqueue::Enqueued,
                    Err(flume::TrySendError::Full(_)) => RecordEnqueue::Full,
                    Err(flume::TrySendError::Disconnected(_)) => RecordEnqueue::Disconnected,
                }
            }) {
            Ok(RecordAdmission::Accepted) => {
                accounting.next_sequence = accounting.next_sequence.saturating_add(1);
                if kind.starts_provider_exchange() {
                    accounting.provider_exchange_observed = true;
                }
                RecordOutcome::Accepted
            }
            Ok(RecordAdmission::Rejected(reason)) => {
                accounting.note_incomplete(reason, true);
                RecordOutcome::Dropped(reason)
            }
            Err(_) => RecordOutcome::Inactive,
        }
    }

    fn finalize(
        &self,
        reason: Option<IncompleteReason>,
        terminal: SafeTerminalCategory,
    ) -> TransitionOutcome {
        let _send_guard = self.send_gate.lock();
        if self.terminal_sent.swap(true, Ordering::AcqRel) {
            return TransitionOutcome::AlreadyApplied;
        }
        if let Some(reason) = reason {
            self.accounting.lock().note_incomplete(reason, false);
            let _ = self.controller.note_incomplete(&self.permit, reason, false);
        }
        if !self.accounting.lock().provider_exchange_observed {
            self.accounting
                .lock()
                .note_incomplete(IncompleteReason::MissingProviderExchange, false);
            let _ = self.controller.note_incomplete(
                &self.permit,
                IncompleteReason::MissingProviderExchange,
                false,
            );
        }
        let terminal_record = make_terminal_record(
            self.accounting.lock().next_sequence,
            u64::try_from(self.started_at.elapsed().as_millis()).unwrap_or(u64::MAX),
            terminal,
        );
        let terminal_bytes = u64::try_from(terminal_record.payload().len()).unwrap_or(u64::MAX);
        match self.controller.account_record(
            &self.permit,
            CaptureRecordKind::TerminalOutcome,
            terminal_bytes,
        ) {
            Ok(RecordAdmission::Accepted) | Err(_) => {}
            Ok(RecordAdmission::Rejected(reason)) => {
                self.accounting.lock().note_incomplete(reason, false);
            }
        }
        {
            let mut accounting = self.accounting.lock();
            accounting.next_sequence = accounting.next_sequence.saturating_add(1);
            accounting.pending_terminal = Some((terminal, terminal_record));
        }
        let Ok(outcome) = self.controller.begin_finalization(&self.permit, terminal) else {
            self.controller.fail(&self.permit);
            return TransitionOutcome::Applied;
        };
        if outcome == TransitionOutcome::AlreadyApplied {
            return outcome;
        }
        let command = WriterCommand::Finalize {
            capture_ref: self.permit.capture_ref(),
            terminal,
        };
        if self
            .sender
            .send_timeout(command, WORKER_POLL_INTERVAL)
            .is_err()
        {
            self.controller.fail(&self.permit);
        }
        outcome
    }
}

impl Drop for CaptureSessionInner {
    fn drop(&mut self) {
        let _ = self.finalize(
            Some(IncompleteReason::Abandoned),
            SafeTerminalCategory::Cancelled,
        );
    }
}

fn make_terminal_record(
    sequence: u16,
    monotonic_offset_ms: u64,
    terminal: SafeTerminalCategory,
) -> CaptureRecordV1 {
    let outcome = match terminal {
        SafeTerminalCategory::Success => "success",
        SafeTerminalCategory::Failed => "failed",
        SafeTerminalCategory::Cancelled => "cancelled",
        SafeTerminalCategory::Superseded => "superseded",
        SafeTerminalCategory::TimedOut => "timed_out",
        SafeTerminalCategory::Panicked => "panicked",
    };
    let payload = super::payload::encode_fields(
        super::payload::PrivatePayloadKind::TerminalOutcome,
        &[super::payload::PrivateField::text(
            super::payload::field::OUTCOME,
            outcome,
        )],
    )
    .expect("fixed terminal evidence must fit the private payload schema");
    CaptureRecordV1::with_context(
        sequence,
        monotonic_offset_ms,
        None,
        EndpointRole::Operation,
        ProviderClientKind::Unknown,
        TransportKind::Unknown,
        0,
        CaptureRecordKind::TerminalOutcome,
        payload,
    )
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum RecordOutcome {
    Accepted,
    Dropped(IncompleteReason),
    Inactive,
}

#[derive(Debug, Default)]
struct SessionAccounting {
    next_sequence: u16,
    provider_exchange_observed: bool,
    incomplete_reasons: Vec<IncompleteReason>,
    dropped_records: u16,
    credential_values_present: bool,
    pending_terminal: Option<(SafeTerminalCategory, CaptureRecordV1)>,
}

impl SessionAccounting {
    fn note_incomplete(&mut self, reason: IncompleteReason, dropped_record: bool) {
        if !self.incomplete_reasons.contains(&reason) {
            self.incomplete_reasons.push(reason);
        }
        if dropped_record {
            self.dropped_records = self.dropped_records.saturating_add(1);
        }
    }
}

enum WriterCommand {
    Begin {
        permit: CapturePermit,
        passphrase: CapturePassphrase,
        created_unix_ms: u64,
        accounting: Arc<Mutex<SessionAccounting>>,
        terminal_sent: Arc<AtomicBool>,
        started_at: Instant,
    },
    Record {
        capture_ref: CaptureRef,
        record: CaptureRecordV1,
    },
    Finalize {
        capture_ref: CaptureRef,
        terminal: SafeTerminalCategory,
    },
}

impl fmt::Debug for WriterCommand {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::Begin { .. } => "WriterCommand::Begin([private])",
            Self::Record { .. } => "WriterCommand::Record([private])",
            Self::Finalize { .. } => "WriterCommand::Finalize([private])",
        })
    }
}

pub(crate) struct CaptureWorker {
    controller: CaptureController,
    receiver: flume::Receiver<WriterCommand>,
    pending_passphrase: Arc<Mutex<Option<CapturePassphrase>>>,
    store: CaptureStore,
    limits: CaptureLimits,
}

impl fmt::Debug for CaptureWorker {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("CaptureWorker")
            .field("snapshot", &self.controller.snapshot())
            .finish_non_exhaustive()
    }
}

struct CaptureWorkerFailureGuard<'a> {
    controller: &'a CaptureController,
    pending_passphrase: &'a Mutex<Option<CapturePassphrase>>,
    armed: bool,
}

impl<'a> CaptureWorkerFailureGuard<'a> {
    fn new(
        controller: &'a CaptureController,
        pending_passphrase: &'a Mutex<Option<CapturePassphrase>>,
    ) -> Self {
        Self {
            controller,
            pending_passphrase,
            armed: true,
        }
    }

    fn disarm(&mut self) {
        self.armed = false;
    }
}

impl Drop for CaptureWorkerFailureGuard<'_> {
    fn drop(&mut self) {
        if self.armed {
            self.pending_passphrase.lock().take();
            self.controller.writer_unavailable();
        }
    }
}

impl CaptureWorker {
    pub(crate) fn run(self, shutdown: &CancellationToken) -> Result<(), CaptureWorkerError> {
        let mut failure_guard =
            CaptureWorkerFailureGuard::new(&self.controller, &self.pending_passphrase);
        let result = self.run_loop(shutdown);
        if result.is_ok() {
            failure_guard.disarm();
        }
        result
    }

    fn run_loop(&self, shutdown: &CancellationToken) -> Result<(), CaptureWorkerError> {
        let mut active = None;
        let maintenance_interval = self.store.periodic_maintenance_interval();
        let mut next_maintenance = Instant::now() + maintenance_interval;
        loop {
            if shutdown.is_cancelled() {
                while let Ok(command) = self.receiver.try_recv() {
                    self.apply_command(command, &mut active)?;
                }
                if let Some(active_capture) = active.take() {
                    self.persist_active(
                        active_capture,
                        SafeTerminalCategory::Cancelled,
                        Some(IncompleteReason::Shutdown),
                        Instant::now() + self.limits.writer_finalization,
                    );
                }
                self.pending_passphrase.lock().take();
                return Ok(());
            }

            match self.receiver.recv_timeout(WORKER_POLL_INTERVAL) {
                Ok(command) => {
                    self.apply_command(command, &mut active)?;
                    for _ in 0..usize::from(self.limits.record_capacity).saturating_add(1) {
                        let Ok(command) = self.receiver.try_recv() else {
                            break;
                        };
                        self.apply_command(command, &mut active)?;
                    }
                }
                Err(flume::RecvTimeoutError::Timeout) => {}
                Err(flume::RecvTimeoutError::Disconnected) => {
                    if let Some(active_capture) = active.take() {
                        self.persist_active(
                            active_capture,
                            SafeTerminalCategory::Cancelled,
                            Some(IncompleteReason::WriterUnavailable),
                            Instant::now() + self.limits.writer_finalization,
                        );
                    }
                    return Err(CaptureWorkerError::CommandChannelClosed);
                }
            }

            let snapshot = self.controller.poll_deadlines();
            if snapshot.state == SafeCaptureState::Incomplete
                && snapshot.terminal_category == Some(SafeTerminalCategory::TimedOut)
            {
                if let Some(active_capture) = active.take() {
                    self.persist_active(
                        active_capture,
                        SafeTerminalCategory::TimedOut,
                        Some(IncompleteReason::OperationDeadline),
                        Instant::now() + self.limits.writer_finalization,
                    );
                }
            }
            if matches!(
                snapshot.state,
                SafeCaptureState::Inactive
                    | SafeCaptureState::Expired
                    | SafeCaptureState::Incomplete
                    | SafeCaptureState::Failed
            ) {
                self.pending_passphrase.lock().take();
            }
            if Instant::now() >= next_maintenance {
                let _ = self.store.try_maintain();
                next_maintenance = Instant::now() + maintenance_interval;
            }
        }
    }

    fn apply_command(
        &self,
        command: WriterCommand,
        active: &mut Option<ActiveCapture>,
    ) -> Result<(), CaptureWorkerError> {
        match command {
            WriterCommand::Begin {
                permit,
                passphrase,
                created_unix_ms,
                accounting,
                terminal_sent,
                started_at,
            } => {
                if active.is_some() {
                    self.controller.fail(&permit);
                    return Err(CaptureWorkerError::ProtocolViolation);
                }
                let writer = self
                    .store
                    .begin_stream(permit.capture_ref(), &passphrase)
                    .ok();
                if writer.is_none() {
                    accounting
                        .lock()
                        .note_incomplete(IncompleteReason::Persistence, false);
                    let _ = self.controller.note_incomplete(
                        &permit,
                        IncompleteReason::Persistence,
                        false,
                    );
                }
                *active = Some(ActiveCapture {
                    permit,
                    writer,
                    created_unix_ms,
                    accounting,
                    terminal_sent,
                    started_at,
                });
            }
            WriterCommand::Record {
                capture_ref,
                record,
            } => {
                let Some(active_capture) = active.as_mut() else {
                    return Err(CaptureWorkerError::ProtocolViolation);
                };
                if active_capture.permit.capture_ref() != capture_ref {
                    return Err(CaptureWorkerError::ProtocolViolation);
                }
                if active_capture
                    .writer
                    .as_mut()
                    .is_none_or(|writer| writer.write_record(&record).is_err())
                {
                    active_capture.writer.take();
                    active_capture
                        .accounting
                        .lock()
                        .note_incomplete(IncompleteReason::Persistence, true);
                    let _ = self.controller.note_incomplete(
                        &active_capture.permit,
                        IncompleteReason::Persistence,
                        true,
                    );
                }
            }
            WriterCommand::Finalize {
                capture_ref,
                terminal,
            } => {
                let Some(active_capture) = active.take() else {
                    return Err(CaptureWorkerError::ProtocolViolation);
                };
                if active_capture.permit.capture_ref() != capture_ref {
                    self.controller.fail(&active_capture.permit);
                    return Err(CaptureWorkerError::ProtocolViolation);
                }
                self.persist_active(
                    active_capture,
                    terminal,
                    None,
                    Instant::now() + self.limits.writer_finalization,
                );
            }
        }
        Ok(())
    }

    fn persist_active(
        &self,
        mut active: ActiveCapture,
        terminal: SafeTerminalCategory,
        forced_reason: Option<IncompleteReason>,
        deadline: Instant,
    ) {
        self.write_terminal(&mut active, terminal);
        if let Some(reason) = forced_reason {
            active.accounting.lock().note_incomplete(reason, false);
            let _ = self
                .controller
                .note_incomplete(&active.permit, reason, false);
            let _ = self.controller.begin_finalization(&active.permit, terminal);
        }
        let accounting = active.accounting.lock();
        let completeness =
            if accounting.incomplete_reasons.is_empty() && accounting.dropped_records == 0 {
                CaptureCompleteness::Complete
            } else {
                CaptureCompleteness::Incomplete
            };
        let mut capture = PrivateCaptureV1::new(
            active.permit.capture_ref(),
            active.created_unix_ms,
            unix_ms(SystemTime::now()),
            active.permit.purpose(),
            active.permit.operation_ref(),
            Vec::new(),
            completeness,
            accounting.incomplete_reasons.clone(),
            accounting.dropped_records,
            terminal,
        );
        if accounting.credential_values_present {
            capture.mark_credential_values_present();
        }
        drop(accounting);
        let persisted = active.writer.is_some_and(|writer| {
            match self.store.finish_stream(writer, &capture, Some(deadline)) {
                Ok(_) => true,
                Err(error) => error.committed_artifact().is_some(),
            }
        });
        let _ = self
            .controller
            .finish_finalization(&active.permit, persisted);
    }

    fn write_terminal(&self, active: &mut ActiveCapture, terminal: SafeTerminalCategory) {
        active.terminal_sent.store(true, Ordering::Release);
        let record = {
            let mut accounting = active.accounting.lock();
            match accounting.pending_terminal.take() {
                Some((recorded_terminal, record)) if recorded_terminal == terminal => record,
                Some((_, record)) => {
                    make_terminal_record(record.sequence(), record.monotonic_offset_ms(), terminal)
                }
                None => {
                    let record = make_terminal_record(
                        accounting.next_sequence,
                        u64::try_from(active.started_at.elapsed().as_millis()).unwrap_or(u64::MAX),
                        terminal,
                    );
                    accounting.next_sequence = accounting.next_sequence.saturating_add(1);
                    record
                }
            }
        };
        if active
            .writer
            .as_mut()
            .is_none_or(|writer| writer.write_record(&record).is_err())
        {
            active.writer.take();
            active
                .accounting
                .lock()
                .note_incomplete(IncompleteReason::Persistence, true);
            let _ = self.controller.note_incomplete(
                &active.permit,
                IncompleteReason::Persistence,
                true,
            );
        }
    }
}

struct ActiveCapture {
    permit: CapturePermit,
    writer: Option<PendingCaptureWrite>,
    created_unix_ms: u64,
    accounting: Arc<Mutex<SessionAccounting>>,
    terminal_sent: Arc<AtomicBool>,
    started_at: Instant,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum CaptureWorkerError {
    CommandChannelClosed,
    ProtocolViolation,
}

impl fmt::Display for CaptureWorkerError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::CommandChannelClosed => "private capture command channel closed unexpectedly",
            Self::ProtocolViolation => "private capture writer protocol was violated",
        })
    }
}

impl std::error::Error for CaptureWorkerError {}

#[derive(Debug)]
pub(crate) enum CaptureRuntimeError {
    Busy,
    Controller(ControllerError),
    InvalidLimits,
    Store(StoreError),
    WriterUnavailable,
}

impl fmt::Display for CaptureRuntimeError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::Busy => "private capture is already armed",
            Self::Controller(_) => "private capture state transition failed",
            Self::InvalidLimits => "private capture limits are invalid",
            Self::Store(_) => "private capture vault is unavailable",
            Self::WriterUnavailable => "private capture writer is unavailable",
        })
    }
}

impl std::error::Error for CaptureRuntimeError {}

fn unix_ms(time: SystemTime) -> u64 {
    u64::try_from(
        time.duration_since(SystemTime::UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis(),
    )
    .unwrap_or(u64::MAX)
}

#[cfg(test)]
mod tests {
    use super::{
        prepare_runtime, prepare_runtime_with_capacity, CaptureWorkerError, RecordOutcome,
        WriterCommand,
    };
    use crate::developer_capture::{
        model::{
            CaptureByteBucket, CaptureCompleteness, CaptureLimits, CapturePurpose,
            CaptureRecordKind, CaptureRecordV1, CaptureRef, IncompleteReason, SafeCaptureState,
            SafeOperationRef, SafeTerminalCategory, SensitiveBytes,
        },
        security::CapturePassphrase,
        store::CaptureStore,
        TransitionOutcome,
    };
    use std::time::{Duration, Instant};
    use tokio_util::sync::CancellationToken;

    fn wait_for_state(
        handle: &super::CaptureHandle,
        expected: SafeCaptureState,
    ) -> super::SafeCaptureSnapshot {
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            let snapshot = handle.snapshot();
            if snapshot.state == expected {
                return snapshot;
            }
            assert!(Instant::now() < deadline, "capture state did not settle");
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    #[test]
    fn synthetic_capture_is_owned_persisted_and_reviewable() {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path().join("vault");
        let limits = CaptureLimits::default();
        let (handle, worker, _) = prepare_runtime(&root, limits).unwrap();
        let shutdown = CancellationToken::new();
        let worker_shutdown = shutdown.clone();
        let thread = std::thread::spawn(move || worker.run(&worker_shutdown));

        handle.request_arm().unwrap();
        handle
            .accept_consent(CapturePassphrase::new("runtime test passphrase".to_owned()).unwrap())
            .unwrap();
        let session = handle
            .claim(
                CapturePurpose::InteractivePlayback,
                SafeOperationRef::from_bytes([1; 4]),
            )
            .unwrap()
            .unwrap();
        let capture_ref = session.capture_ref();
        assert_eq!(
            session.record(
                CaptureRecordKind::HttpRequest,
                SensitiveBytes::new(b"seeded-private-runtime-payload".to_vec()),
            ),
            RecordOutcome::Accepted
        );
        session.finish(SafeTerminalCategory::Failed);
        let snapshot = wait_for_state(&handle, SafeCaptureState::Ready);
        assert_eq!(snapshot.completeness, CaptureCompleteness::Complete);

        shutdown.cancel();
        thread.join().unwrap().unwrap();
        let (store, _) = CaptureStore::open(&root, limits).unwrap();
        let review = store
            .review(
                capture_ref,
                &CapturePassphrase::new("runtime test passphrase".to_owned()).unwrap(),
            )
            .unwrap();
        assert!(review.checksum_valid);
        assert_eq!(review.record_count, 2);
    }

    #[test]
    fn concurrent_recording_cannot_append_after_the_single_terminal_record() {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path().join("vault");
        let limits = CaptureLimits::default();
        let (handle, worker, _) = prepare_runtime(&root, limits).unwrap();
        let shutdown = CancellationToken::new();
        let worker_shutdown = shutdown.clone();
        let worker_thread = std::thread::spawn(move || worker.run(&worker_shutdown));
        handle.request_arm().unwrap();
        handle
            .accept_consent(
                CapturePassphrase::new("concurrent terminal passphrase".to_owned()).unwrap(),
            )
            .unwrap();
        let session = handle
            .claim(
                CapturePurpose::InteractivePlayback,
                SafeOperationRef::from_bytes([11; 4]),
            )
            .unwrap()
            .unwrap();
        let capture_ref = session.capture_ref();
        let barrier = std::sync::Arc::new(std::sync::Barrier::new(9));
        let mut threads = Vec::new();
        for value in 0..8_u8 {
            let session = session.clone();
            let barrier = barrier.clone();
            threads.push(std::thread::spawn(move || {
                barrier.wait();
                if value == 0 {
                    session.finish(SafeTerminalCategory::Failed);
                } else {
                    let _ = session.record(
                        CaptureRecordKind::HttpRequest,
                        SensitiveBytes::new(vec![value]),
                    );
                }
            }));
        }
        barrier.wait();
        for thread in threads {
            thread.join().unwrap();
        }
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            if matches!(
                handle.snapshot().state,
                SafeCaptureState::Ready | SafeCaptureState::Incomplete
            ) {
                break;
            }
            assert!(
                Instant::now() < deadline,
                "concurrent capture did not settle"
            );
            std::thread::sleep(Duration::from_millis(10));
        }
        shutdown.cancel();
        worker_thread.join().unwrap().unwrap();
        let (store, _) = CaptureStore::open(&root, limits).unwrap();
        let capture = store
            .read_private(
                capture_ref,
                &CapturePassphrase::new("concurrent terminal passphrase".to_owned()).unwrap(),
            )
            .unwrap();
        assert_eq!(
            capture
                .records()
                .iter()
                .filter(|record| record.kind() == CaptureRecordKind::TerminalOutcome)
                .count(),
            1
        );
        assert_eq!(
            capture.records().last().map(CaptureRecordV1::kind),
            Some(CaptureRecordKind::TerminalOutcome)
        );
    }

    #[test]
    fn shutdown_during_active_capture_persists_incomplete_evidence() {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path().join("vault");
        let limits = CaptureLimits::default();
        let (handle, worker, _) = prepare_runtime(&root, limits).unwrap();
        let shutdown = CancellationToken::new();
        let worker_shutdown = shutdown.clone();
        let thread = std::thread::spawn(move || worker.run(&worker_shutdown));
        handle.request_arm().unwrap();
        handle
            .accept_consent(CapturePassphrase::new("shutdown test passphrase".to_owned()).unwrap())
            .unwrap();
        let session = handle
            .claim(
                CapturePurpose::InteractivePlayback,
                SafeOperationRef::from_bytes([2; 4]),
            )
            .unwrap()
            .unwrap();
        let capture_ref = session.capture_ref();
        assert_eq!(
            session.record(
                CaptureRecordKind::HttpRequest,
                SensitiveBytes::new(b"seeded-private-shutdown-payload".to_vec()),
            ),
            RecordOutcome::Accepted
        );
        let finalization_started = Instant::now();
        shutdown.cancel();
        thread.join().unwrap().unwrap();
        let finalization_elapsed = finalization_started.elapsed();
        eprintln!(
            "phase7.performance.writer_finalization_ms={}",
            finalization_elapsed.as_millis()
        );
        assert!(
            finalization_elapsed < limits.writer_finalization,
            "active writer finalization took {finalization_elapsed:?}"
        );

        let (store, _) = CaptureStore::open(&root, limits).unwrap();
        let capture = store
            .read_private(
                capture_ref,
                &CapturePassphrase::new("shutdown test passphrase".to_owned()).unwrap(),
            )
            .unwrap();
        assert_eq!(capture.completeness, CaptureCompleteness::Incomplete);
        assert!(capture
            .incomplete_reasons
            .contains(&IncompleteReason::Shutdown));
        assert_eq!(
            capture
                .records()
                .iter()
                .filter(|record| record.kind() == CaptureRecordKind::TerminalOutcome)
                .count(),
            1
        );
        assert_eq!(
            capture.records().last().map(CaptureRecordV1::kind),
            Some(CaptureRecordKind::TerminalOutcome)
        );
    }

    #[test]
    fn queue_saturation_is_nonblocking_and_visible() {
        let directory = tempfile::tempdir().unwrap();
        let (handle, _worker, _) = prepare_runtime_with_capacity(
            directory.path().join("vault"),
            CaptureLimits::default(),
            1,
        )
        .unwrap();
        handle.request_arm().unwrap();
        handle
            .accept_consent(CapturePassphrase::new("queue test passphrase".to_owned()).unwrap())
            .unwrap();
        let session = handle
            .claim(
                CapturePurpose::InteractivePlayback,
                SafeOperationRef::from_bytes([3; 4]),
            )
            .unwrap()
            .unwrap();
        assert_eq!(
            session.record(CaptureRecordKind::HttpRequest, SensitiveBytes::new(vec![1]),),
            RecordOutcome::Dropped(IncompleteReason::QueueCapacity)
        );
        let snapshot = handle.snapshot();
        assert_eq!(snapshot.record_count, 0);
        assert_eq!(snapshot.byte_bucket, CaptureByteBucket::Empty);
        assert_eq!(snapshot.dropped_records, 1);
        assert_eq!(snapshot.completeness, CaptureCompleteness::Incomplete);
    }

    #[test]
    #[ignore = "local Phase 7 performance evidence"]
    fn active_nonblocking_enqueue_stays_below_one_hundred_microseconds_p95() {
        const SAMPLE_COUNT: usize = 200;

        let directory = tempfile::tempdir().unwrap();
        let (handle, _worker, _) = prepare_runtime_with_capacity(
            directory.path().join("vault"),
            CaptureLimits::default(),
            512,
        )
        .unwrap();
        handle.request_arm().unwrap();
        handle
            .accept_consent(
                CapturePassphrase::new("enqueue performance passphrase".to_owned()).unwrap(),
            )
            .unwrap();
        let session = handle
            .claim(
                CapturePurpose::InteractivePlayback,
                SafeOperationRef::from_bytes([0x70; 4]),
            )
            .unwrap()
            .unwrap();

        let mut samples = Vec::with_capacity(SAMPLE_COUNT);
        for _ in 0..SAMPLE_COUNT {
            let started = Instant::now();
            assert_eq!(
                session.record(CaptureRecordKind::DecodeStage, SensitiveBytes::new(vec![1]),),
                RecordOutcome::Accepted
            );
            samples.push(started.elapsed());
        }
        samples.sort_unstable();
        let p95 = samples[(SAMPLE_COUNT * 95 / 100).saturating_sub(1)];
        eprintln!(
            "phase7.performance.active_enqueue_p95_ns={}",
            p95.as_nanos()
        );
        assert!(
            p95 < Duration::from_micros(100),
            "active private capture enqueue p95 was {p95:?}"
        );
        session.finish(SafeTerminalCategory::Cancelled);
    }

    #[test]
    fn disconnected_writer_is_distinct_from_queue_saturation() {
        let directory = tempfile::tempdir().unwrap();
        let (handle, worker, _) =
            prepare_runtime(directory.path().join("vault"), CaptureLimits::default()).unwrap();
        handle.request_arm().unwrap();
        handle
            .accept_consent(CapturePassphrase::new("writer loss passphrase".to_owned()).unwrap())
            .unwrap();
        let session = handle
            .claim(
                CapturePurpose::InteractivePlayback,
                SafeOperationRef::from_bytes([0x31; 4]),
            )
            .unwrap()
            .unwrap();
        drop(worker);

        assert_eq!(
            session.record(CaptureRecordKind::HttpRequest, SensitiveBytes::new(vec![1]),),
            RecordOutcome::Dropped(IncompleteReason::WriterUnavailable)
        );
        let snapshot = handle.snapshot();
        assert_eq!(snapshot.state, SafeCaptureState::Failed);
        assert_eq!(snapshot.record_count, 0);
        assert_eq!(snapshot.dropped_records, 1);
        assert_eq!(snapshot.completeness, CaptureCompleteness::Incomplete);
    }

    #[test]
    fn unexpected_worker_error_transitions_capture_out_of_active_state() {
        let directory = tempfile::tempdir().unwrap();
        let (handle, worker, _) =
            prepare_runtime(directory.path().join("vault"), CaptureLimits::default()).unwrap();
        handle.request_arm().unwrap();
        handle
            .accept_consent(CapturePassphrase::new("worker failure passphrase".to_owned()).unwrap())
            .unwrap();
        let _session = handle
            .claim(
                CapturePurpose::InteractivePlayback,
                SafeOperationRef::from_bytes([0x32; 4]),
            )
            .unwrap()
            .unwrap();
        handle
            .sender
            .send(WriterCommand::Record {
                capture_ref: CaptureRef::from_bytes([0xee; 16]),
                record: CaptureRecordV1::new(
                    0,
                    0,
                    CaptureRecordKind::HttpRequest,
                    SensitiveBytes::new(vec![1]),
                ),
            })
            .unwrap();

        assert_eq!(
            worker.run(&CancellationToken::new()),
            Err(CaptureWorkerError::ProtocolViolation)
        );
        let snapshot = handle.snapshot();
        assert_eq!(snapshot.state, SafeCaptureState::Failed);
        assert_eq!(snapshot.completeness, CaptureCompleteness::Incomplete);
    }

    #[test]
    fn operation_deadline_finalizes_without_waiting_for_the_request_owner() {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path().join("vault");
        let limits = CaptureLimits {
            operation_deadline: Duration::from_millis(50),
            writer_finalization: Duration::from_secs(5),
            ..CaptureLimits::default()
        };
        let (handle, worker, _) = prepare_runtime(&root, limits).unwrap();
        let shutdown = CancellationToken::new();
        let worker_shutdown = shutdown.clone();
        let thread = std::thread::spawn(move || worker.run(&worker_shutdown));
        handle.request_arm().unwrap();
        handle
            .accept_consent(CapturePassphrase::new("timeout test passphrase".to_owned()).unwrap())
            .unwrap();
        let session = handle
            .claim(
                CapturePurpose::InteractivePlayback,
                SafeOperationRef::from_bytes([4; 4]),
            )
            .unwrap()
            .unwrap();
        let capture_ref = session.capture_ref();
        assert_eq!(
            session.record(
                CaptureRecordKind::HttpRequest,
                SensitiveBytes::new(b"seeded-private-timeout-payload".to_vec()),
            ),
            RecordOutcome::Accepted
        );

        let snapshot = wait_for_state(&handle, SafeCaptureState::Incomplete);
        assert_eq!(
            snapshot.terminal_category,
            Some(SafeTerminalCategory::TimedOut)
        );
        let artifact_deadline = Instant::now() + Duration::from_secs(10);
        while !root
            .read_dir()
            .unwrap()
            .filter_map(Result::ok)
            .any(|entry| entry.path().extension().is_some_and(|ext| ext == "age"))
        {
            assert!(
                Instant::now() < artifact_deadline,
                "timed-out capture was not persisted"
            );
            std::thread::sleep(Duration::from_millis(10));
        }
        assert_eq!(
            session.finish(SafeTerminalCategory::Success),
            TransitionOutcome::AlreadyApplied
        );
        shutdown.cancel();
        thread.join().unwrap().unwrap();

        let (store, _) = CaptureStore::open(&root, limits).unwrap();
        let capture = store
            .read_private(
                capture_ref,
                &CapturePassphrase::new("timeout test passphrase".to_owned()).unwrap(),
            )
            .unwrap();
        assert!(capture
            .incomplete_reasons
            .contains(&IncompleteReason::OperationDeadline));
        assert_eq!(capture.terminal_category, SafeTerminalCategory::TimedOut);
        assert_eq!(
            capture
                .records()
                .iter()
                .filter(|record| record.kind() == CaptureRecordKind::TerminalOutcome)
                .count(),
            1
        );
        assert_eq!(
            capture.records().last().map(CaptureRecordV1::kind),
            Some(CaptureRecordKind::TerminalOutcome)
        );
    }

    #[test]
    fn idle_worker_shutdown_is_bounded() {
        let directory = tempfile::tempdir().unwrap();
        let (_handle, worker, _) =
            prepare_runtime(directory.path().join("vault"), CaptureLimits::default()).unwrap();
        let shutdown = CancellationToken::new();
        shutdown.cancel();
        let started = Instant::now();
        worker.run(&shutdown).unwrap();
        assert!(started.elapsed() < Duration::from_secs(1));
    }
}
