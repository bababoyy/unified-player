use std::{
    collections::VecDeque,
    fs::{File, OpenOptions},
    io::{BufWriter, Write as _},
    path::{Path, PathBuf},
    sync::{
        atomic::{AtomicU64, AtomicU8, Ordering},
        Arc,
    },
    thread,
    time::{Duration, Instant, SystemTime},
};

use anyhow::{Context as _, Result};
use parking_lot::Mutex;
use tracing::Subscriber;
use tracing_subscriber::Layer;

use super::protocol::{
    Component, DiagnosticEvent, Severity, UiDiagnosticEntry, DIAGNOSTIC_SCHEMA_VERSION,
};

const DEFAULT_CHANNEL_CAPACITY: usize = 2_048;
const DEFAULT_UI_CAPACITY: usize = 1_000;
const DEFAULT_ROTATE_BYTES: u64 = 10 * 1024 * 1024;
const DEFAULT_RETAIN_BYTES: u64 = 50 * 1024 * 1024;
const DEFAULT_RETAIN_AGE: Duration = Duration::from_secs(7 * 24 * 60 * 60);
const FILE_PREFIX: &str = "unified-player-diagnostics-";

const fn severity_from_u8(value: u8) -> Severity {
    match value {
        0 => Severity::Trace,
        1 => Severity::Debug,
        2 => Severity::Info,
        3 => Severity::Warn,
        _ => Severity::Error,
    }
}

fn default_filter_level() -> Severity {
    let Ok(filter) = std::env::var("RUST_LOG") else {
        return Severity::Info;
    };
    filter
        .split(',')
        .filter_map(|directive| {
            let (target, level) = directive
                .split_once('=')
                .map_or(("", directive), |(target, level)| (target.trim(), level));
            if !target.is_empty() && !target.starts_with("unified-player") {
                return None;
            }
            match level.trim().to_ascii_lowercase().as_str() {
                "trace" => Some(Severity::Trace),
                "debug" => Some(Severity::Debug),
                "info" => Some(Severity::Info),
                "warn" => Some(Severity::Warn),
                "error" => Some(Severity::Error),
                _ => None,
            }
        })
        .min()
        .unwrap_or(Severity::Info)
}

pub(crate) type UiDiagnosticRing = Arc<Mutex<VecDeque<UiDiagnosticEntry>>>;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[repr(u8)]
pub(crate) enum WriterState {
    Starting = 0,
    Healthy = 1,
    Degraded = 2,
    Disabled = 3,
    Stopped = 4,
}

impl WriterState {
    fn from_u8(value: u8) -> Self {
        match value {
            0 => Self::Starting,
            1 => Self::Healthy,
            2 => Self::Degraded,
            3 => Self::Disabled,
            _ => Self::Stopped,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct WriterHealth {
    pub(crate) state: WriterState,
    pub(crate) dropped_events: u64,
    pub(crate) files_created: u64,
    pub(crate) bytes_written: u64,
}

#[derive(Clone, Debug)]
pub(crate) struct SinkPolicy {
    pub(crate) channel_capacity: usize,
    pub(crate) ui_capacity: usize,
    pub(crate) rotate_bytes: u64,
    pub(crate) retain_bytes: u64,
    pub(crate) retain_age: Duration,
    #[cfg(test)]
    pub(crate) write_delay: Duration,
}

impl Default for SinkPolicy {
    fn default() -> Self {
        Self {
            channel_capacity: DEFAULT_CHANNEL_CAPACITY,
            ui_capacity: DEFAULT_UI_CAPACITY,
            rotate_bytes: DEFAULT_ROTATE_BYTES,
            retain_bytes: DEFAULT_RETAIN_BYTES,
            retain_age: DEFAULT_RETAIN_AGE,
            #[cfg(test)]
            write_delay: Duration::ZERO,
        }
    }
}

struct HealthAtoms {
    state: AtomicU8,
    dropped_events: AtomicU64,
    files_created: AtomicU64,
    bytes_written: AtomicU64,
}

struct DynamicFilterState {
    default_level: AtomicU8,
    temporary_level: AtomicU8,
    expires_at_uptime_ms: AtomicU64,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct DynamicFilterSnapshot {
    pub(crate) level: Severity,
    pub(crate) temporary: bool,
    pub(crate) remaining_seconds: u64,
    pub(crate) available: bool,
}

impl DynamicFilterState {
    fn new(default_level: Severity) -> Self {
        Self {
            default_level: AtomicU8::new(default_level as u8),
            temporary_level: AtomicU8::new(default_level as u8),
            expires_at_uptime_ms: AtomicU64::new(0),
        }
    }

    fn snapshot(&self, uptime_ms: u64, available: bool) -> DynamicFilterSnapshot {
        let expires_at = self.expires_at_uptime_ms.load(Ordering::Acquire);
        let temporary = expires_at > uptime_ms;
        let level = if temporary {
            severity_from_u8(self.temporary_level.load(Ordering::Acquire))
        } else {
            severity_from_u8(self.default_level.load(Ordering::Acquire))
        };
        DynamicFilterSnapshot {
            level,
            temporary,
            remaining_seconds: expires_at.saturating_sub(uptime_ms).div_ceil(1_000),
            available,
        }
    }

    fn enable_verbose(&self, uptime_ms: u64, duration: Duration) {
        self.temporary_level
            .store(Severity::Trace as u8, Ordering::Release);
        self.expires_at_uptime_ms.store(
            uptime_ms.saturating_add(duration.as_millis().try_into().unwrap_or(u64::MAX)),
            Ordering::Release,
        );
    }

    fn stop_verbose(&self) {
        self.expires_at_uptime_ms.store(0, Ordering::Release);
    }
}

impl HealthAtoms {
    fn new(state: WriterState) -> Self {
        Self {
            state: AtomicU8::new(state as u8),
            dropped_events: AtomicU64::new(0),
            files_created: AtomicU64::new(0),
            bytes_written: AtomicU64::new(0),
        }
    }

    fn snapshot(&self) -> WriterHealth {
        WriterHealth {
            state: WriterState::from_u8(self.state.load(Ordering::Acquire)),
            dropped_events: self.dropped_events.load(Ordering::Acquire),
            files_created: self.files_created.load(Ordering::Acquire),
            bytes_written: self.bytes_written.load(Ordering::Acquire),
        }
    }
}

enum SinkCommand {
    Event(Box<DiagnosticEvent>),
    Shutdown,
}

#[derive(Default)]
struct SupportSession {
    directory: Option<PathBuf>,
    review: Option<super::BundleReview>,
}

#[derive(Clone)]
pub(crate) struct DiagnosticsHandle {
    run_id: Arc<str>,
    started_at: Instant,
    sender: flume::Sender<SinkCommand>,
    ui_ring: UiDiagnosticRing,
    health: Arc<HealthAtoms>,
    component_health: super::HealthRegistry,
    console: super::ConsoleRegistry,
    incidents: super::IncidentRecorder,
    filter: Arc<DynamicFilterState>,
    ui_capacity: usize,
    sink_enabled: bool,
    log_directory: Arc<PathBuf>,
    support: Arc<Mutex<SupportSession>>,
}

impl DiagnosticsHandle {
    pub(crate) fn run_id(&self) -> &str {
        &self.run_id
    }

    pub(crate) fn started_at(&self) -> Instant {
        self.started_at
    }

    pub(crate) fn health(&self) -> WriterHealth {
        self.health.snapshot()
    }

    pub(crate) fn health_snapshot(&self) -> super::HealthSnapshot {
        self.component_health.snapshot(self.health())
    }

    pub(crate) fn incidents(&self) -> Vec<super::IncidentSummary> {
        self.incidents.snapshot()
    }

    pub(crate) fn incident_states(&self) -> Vec<(super::IncidentSummary, bool)> {
        self.incidents.snapshot_with_acknowledgement()
    }

    pub(crate) fn acknowledge_incident(&self, reference: &str) -> bool {
        self.incidents.acknowledge(reference)
    }

    pub(crate) fn record_local_action_failure(&self) -> Option<super::IncidentSummary> {
        let mut event = DiagnosticEvent::from_tracing(
            self.run_id(),
            self.started_at(),
            "DIAGNOSTIC_ACTION_FAILED",
            Severity::Warn,
            Component::Support,
            "A local diagnostic action failed",
        );
        event.fields.error_type = Some("resource".to_owned());
        event.fields.outcome = Some(super::OperationOutcome::Error);
        self.record(event);
        self.incidents
            .snapshot()
            .into_iter()
            .rev()
            .find(|incident| incident.safe_event_code() == "DIAGNOSTIC_ACTION_FAILED")
    }

    pub(crate) fn timeline(&self, reference: &str) -> Option<super::OperationTimeline> {
        self.console.timeline(reference)
    }

    pub(crate) fn performance_entries(&self) -> Vec<super::PerformanceEntry> {
        self.console.performance()
    }

    pub(crate) fn recent_operations(&self, limit: usize) -> Vec<super::OperationTimeline> {
        self.console.recent_operations(limit)
    }

    pub(crate) fn component_history(&self, component: Component) -> Vec<super::HealthTransition> {
        self.component_health.component_history(component)
    }

    pub(crate) fn worker_history(&self, worker: &str) -> Vec<super::HealthTransition> {
        self.component_health.worker_history(worker)
    }

    pub(crate) fn filter_snapshot(&self) -> DynamicFilterSnapshot {
        self.filter.snapshot(self.uptime_ms(), self.sink_enabled)
    }

    pub(crate) fn enable_verbose(&self, duration: Duration) -> Option<Duration> {
        if !self.sink_enabled {
            return None;
        }
        let duration = duration.clamp(Duration::from_secs(1), Duration::from_secs(5 * 60));
        self.filter.enable_verbose(self.uptime_ms(), duration);
        let mut event = DiagnosticEvent::new(
            self.run_id(),
            self.started_at(),
            super::EventName::FILTER_CHANGED,
            super::EventCode::FILTER_CHANGED,
            Severity::Info,
            Component::Logging,
            "Temporary diagnostic verbosity enabled",
        );
        event.fields.state = Some("trace".to_owned());
        event.fields.duration_ms = duration.as_millis().try_into().ok();
        self.record(event);
        Some(duration)
    }

    pub(crate) fn stop_verbose(&self) -> bool {
        let was_temporary = self.filter_snapshot().temporary;
        self.filter.stop_verbose();
        if was_temporary {
            let mut event = DiagnosticEvent::new(
                self.run_id(),
                self.started_at(),
                super::EventName::FILTER_CHANGED,
                super::EventCode::FILTER_CHANGED,
                Severity::Info,
                Component::Logging,
                "Temporary diagnostic verbosity stopped",
            );
            event.fields.state = Some("default".to_owned());
            self.record(event);
        }
        was_temporary
    }

    pub(crate) fn has_support_bundle(&self) -> bool {
        self.support.lock().directory.is_some()
    }

    pub(crate) fn create_support_bundle(
        &self,
        focus_reference: Option<&str>,
    ) -> Result<super::BundleReview> {
        anyhow::ensure!(self.sink_enabled, "diagnostic evidence is unavailable");
        let output = self.log_directory.join(format!(
            "support-bundle-{}",
            super::protocol::random_hex::<8>()
        ));
        let review = super::create_focused_support_bundle(
            self.log_directory.as_path(),
            &output,
            focus_reference,
        )?;
        let mut support = self.support.lock();
        support.directory = Some(output);
        support.review = Some(review.clone());
        Ok(review)
    }

    pub(crate) fn review_support_bundle(&self) -> Result<super::BundleReview> {
        let directory = self
            .support
            .lock()
            .directory
            .clone()
            .context("support bundle has not been created")?;
        let review = super::review_support_bundle(&directory)?;
        self.support.lock().review = Some(review.clone());
        Ok(review)
    }

    pub(crate) fn latest_support_review(&self) -> Option<super::BundleReview> {
        self.support.lock().review.clone()
    }

    pub(crate) fn open_support_bundle_folder(&self) -> Result<()> {
        let directory = self
            .support
            .lock()
            .directory
            .clone()
            .context("support bundle has not been created")?;
        super::support_bundle::open_folder_with(&directory, |path| open::that(path))
    }

    pub(crate) fn local_trend(&self) -> Result<String> {
        super::render_local_trend(self.log_directory.as_path())
    }

    fn uptime_ms(&self) -> u64 {
        self.started_at
            .elapsed()
            .as_millis()
            .try_into()
            .unwrap_or(u64::MAX)
    }

    fn allows(&self, severity: Severity) -> bool {
        severity as u8 >= self.filter_snapshot().level as u8
    }

    pub(crate) fn set_component_health(
        &self,
        component: Component,
        status: super::HealthStatus,
        fact: &'static str,
    ) {
        self.component_health.set_component(
            component,
            status,
            fact,
            self.started_at
                .elapsed()
                .as_millis()
                .try_into()
                .unwrap_or(u64::MAX),
        );
    }

    pub(crate) fn update_ui_snapshot(&self, snapshot: &super::UiDiagnosticSnapshot) {
        if !self.component_health.update_ui(snapshot) {
            return;
        }
        let mut event = DiagnosticEvent::new(
            self.run_id(),
            self.started_at(),
            super::EventName::UI_TRANSITION,
            super::EventCode::UI_TRANSITION,
            Severity::Debug,
            Component::Ui,
            "UI state changed",
        );
        event.fields.state = Some(format!(
            "{}:{}:{}:{}:{}",
            snapshot.page,
            snapshot.popup,
            snapshot.meaningful_state,
            snapshot.provider,
            snapshot.lifecycle
        ));
        event.fields.generation = Some(snapshot.revision);
        event.fields.selection_index = snapshot.selection;
        self.record(event);
    }

    pub(crate) fn record(&self, event: DiagnosticEvent) {
        let incident = self.incidents.observe(&event);
        self.record_inner(event);
        if let Some(incident) = incident {
            let mut recorded = DiagnosticEvent::new(
                self.run_id(),
                self.started_at(),
                super::EventName::INCIDENT_RECORDED,
                super::EventCode::INCIDENT_RECORDED,
                Severity::Warn,
                incident.component,
                "Incident summary recorded",
            );
            recorded.fields.incident_reference = Some(incident.reference);
            recorded.fields.error_type = Some("bounded_incident".to_owned());
            recorded.fields.count = Some(u64::from(incident.occurrence_count));
            self.record_inner(recorded);
        }
    }

    fn record_inner(&self, event: DiagnosticEvent) {
        self.component_health.observe(&event);
        self.console.observe(&event);
        let entry = UiDiagnosticEntry::from_event(&event);
        {
            let mut ring = self.ui_ring.lock();
            ring.push_back(entry);
            while ring.len() > self.ui_capacity {
                ring.pop_front();
            }
        }
        if self.sink_enabled
            && self
                .sender
                .try_send(SinkCommand::Event(Box::new(event)))
                .is_err()
        {
            self.health.dropped_events.fetch_add(1, Ordering::AcqRel);
        }
    }

    pub(crate) fn record_panic_event(&self, event: DiagnosticEvent) {
        if self.sink_enabled
            && self
                .sender
                .try_send(SinkCommand::Event(Box::new(event)))
                .is_err()
        {
            self.health.dropped_events.fetch_add(1, Ordering::AcqRel);
        }
    }
}

pub(crate) struct DiagnosticsRuntime {
    handle: DiagnosticsHandle,
    worker: Option<thread::JoinHandle<()>>,
    completion: flume::Receiver<()>,
    shutdown_sent: bool,
}

impl DiagnosticsRuntime {
    pub(crate) fn shutdown(&mut self, timeout: Duration) -> Result<()> {
        if self.worker.is_none() {
            return Ok(());
        }
        let deadline = Instant::now() + timeout;
        if !self.shutdown_sent {
            self.handle
                .sender
                .send_timeout(SinkCommand::Shutdown, timeout)
                .context("request diagnostic writer shutdown")?;
            self.shutdown_sent = true;
        }
        let remaining = deadline.saturating_duration_since(Instant::now());
        self.completion
            .recv_timeout(remaining)
            .context("diagnostic writer exceeded shutdown deadline")?;
        let worker = self.worker.take().expect("diagnostic worker exists");
        anyhow::ensure!(worker.join().is_ok(), "diagnostic writer panicked");
        Ok(())
    }

    pub(crate) fn detach_after_timeout(&mut self) {
        self.handle
            .health
            .state
            .store(WriterState::Degraded as u8, Ordering::Release);
        self.worker.take();
    }
}

impl Drop for DiagnosticsRuntime {
    fn drop(&mut self) {
        let _ = self.shutdown(Duration::from_secs(2));
    }
}

pub(crate) fn start(
    directory: &Path,
    ui_ring: UiDiagnosticRing,
    policy: SinkPolicy,
) -> Result<(DiagnosticsHandle, DiagnosticsRuntime)> {
    std::fs::create_dir_all(directory).context("create diagnostic directory")?;
    enforce_retention(directory, &policy);

    let run_id: Arc<str> = super::protocol::random_hex::<16>().into();
    let started_at = Instant::now();
    let (sender, receiver) = flume::bounded(policy.channel_capacity);
    let (completion_tx, completion) = flume::bounded(1);
    let health = Arc::new(HealthAtoms::new(WriterState::Starting));
    let handle = DiagnosticsHandle {
        run_id,
        started_at,
        sender,
        ui_ring,
        health: health.clone(),
        component_health: super::HealthRegistry::default(),
        console: super::ConsoleRegistry::default(),
        incidents: super::IncidentRecorder::default(),
        filter: Arc::new(DynamicFilterState::new(default_filter_level())),
        ui_capacity: policy.ui_capacity,
        sink_enabled: true,
        log_directory: Arc::new(directory.to_path_buf()),
        support: Arc::new(Mutex::new(SupportSession::default())),
    };
    let worker_handle = handle.clone();
    let directory = directory.to_path_buf();
    let worker = thread::Builder::new()
        .name("diagnostic-writer".to_owned())
        .spawn(move || {
            writer_loop(&directory, &worker_handle, &policy, &receiver);
            let _ = completion_tx.send(());
        })
        .context("spawn diagnostic writer")?;
    let runtime = DiagnosticsRuntime {
        handle: handle.clone(),
        worker: Some(worker),
        completion,
        shutdown_sent: false,
    };
    Ok((handle, runtime))
}

pub(crate) fn disabled(ui_ring: UiDiagnosticRing) -> (DiagnosticsHandle, DiagnosticsRuntime) {
    let (sender, receiver) = flume::bounded(1);
    drop(receiver);
    let (_completion_tx, completion) = flume::bounded(1);
    let handle = DiagnosticsHandle {
        run_id: super::protocol::random_hex::<16>().into(),
        started_at: Instant::now(),
        sender,
        ui_ring,
        health: Arc::new(HealthAtoms::new(WriterState::Disabled)),
        component_health: super::HealthRegistry::default(),
        console: super::ConsoleRegistry::default(),
        incidents: super::IncidentRecorder::default(),
        filter: Arc::new(DynamicFilterState::new(Severity::Error)),
        ui_capacity: DEFAULT_UI_CAPACITY,
        sink_enabled: false,
        log_directory: Arc::new(PathBuf::new()),
        support: Arc::new(Mutex::new(SupportSession::default())),
    };
    let runtime = DiagnosticsRuntime {
        handle: handle.clone(),
        worker: None,
        completion,
        shutdown_sent: false,
    };
    (handle, runtime)
}

pub(crate) struct DiagnosticLayer {
    handle: DiagnosticsHandle,
}

impl DiagnosticLayer {
    pub(crate) fn new(handle: DiagnosticsHandle) -> Self {
        Self { handle }
    }
}

impl<S: Subscriber> Layer<S> for DiagnosticLayer {
    fn on_event(
        &self,
        event: &tracing::Event<'_>,
        _context: tracing_subscriber::layer::Context<'_, S>,
    ) {
        let metadata = event.metadata();
        let severity = Severity::from(metadata.level());
        if !self.handle.allows(severity) {
            return;
        }
        let mut visitor = SafeEventVisitor::default();
        event.record(&mut visitor);
        let code = visitor
            .diagnostic_code()
            .unwrap_or_else(|| "APPLICATION_EVENT".to_owned());
        let mut diagnostic = DiagnosticEvent::from_tracing(
            self.handle.run_id(),
            self.handle.started_at(),
            &code,
            severity,
            Component::from_target(metadata.target()),
            &visitor.message,
        );
        diagnostic.fields.attempt = visitor.attempt;
        diagnostic.fields.error_type = visitor.diagnostic_category();
        diagnostic.fields.duration_ms = visitor.duration_ms;
        diagnostic.fields.retry_after_ms = visitor.retry_after_ms;
        diagnostic.fields.queue_depth = visitor.queue_depth;
        diagnostic.fields.generation = visitor.generation;
        diagnostic.fields.count = visitor.count;
        diagnostic.fields.status_class = visitor.status_class;
        diagnostic.fields.phase = visitor.phase;
        diagnostic.fields.retryable = visitor.retryable;
        diagnostic.fields.worker = visitor.worker;
        diagnostic.fields.state = visitor.state;
        if let Some(context) = super::context::current_operation() {
            diagnostic.operation = Some(context.child());
        }
        self.handle.record(diagnostic);
    }
}

#[derive(Default)]
struct SafeEventVisitor {
    message: String,
    diagnostic: String,
    attempt: Option<u16>,
    duration_ms: Option<u64>,
    retry_after_ms: Option<u64>,
    queue_depth: Option<u16>,
    generation: Option<u64>,
    count: Option<u64>,
    status_class: Option<String>,
    phase: Option<String>,
    retryable: Option<bool>,
    worker: Option<String>,
    state: Option<String>,
}

impl SafeEventVisitor {
    fn diagnostic_code(&self) -> Option<String> {
        self.diagnostic
            .split_whitespace()
            .find_map(|part| part.strip_prefix("code="))
            .map(ToOwned::to_owned)
    }

    fn diagnostic_category(&self) -> Option<String> {
        self.diagnostic
            .split_whitespace()
            .find_map(|part| part.strip_prefix("category="))
            .map(ToOwned::to_owned)
    }
}

impl tracing::field::Visit for SafeEventVisitor {
    fn record_debug(&mut self, field: &tracing::field::Field, value: &dyn std::fmt::Debug) {
        let rendered = format!("{value:?}");
        match field.name() {
            "message" => self.message = bounded_text(&rendered, 240),
            "diagnostic" => self.diagnostic = bounded_text(&rendered, 160),
            "retry_after_ms" => self.retry_after_ms = rendered.parse().ok(),
            "status_class" => self.status_class = Some(bounded_token(&rendered)),
            "phase" => self.phase = Some(bounded_token(&rendered)),
            "worker" => self.worker = Some(bounded_token(&rendered)),
            "state" | "status" => self.state = Some(bounded_token(&rendered)),
            _ => {}
        }
    }

    fn record_bool(&mut self, field: &tracing::field::Field, value: bool) {
        if field.name() == "retryable" {
            self.retryable = Some(value);
        }
    }

    fn record_u64(&mut self, field: &tracing::field::Field, value: u64) {
        match field.name() {
            "attempt" => self.attempt = value.try_into().ok(),
            "duration_ms" => self.duration_ms = Some(value),
            "retry_after_ms" => self.retry_after_ms = Some(value),
            "queue_depth" => self.queue_depth = value.try_into().ok(),
            "generation" => self.generation = Some(value),
            "count" => self.count = Some(value),
            _ => {}
        }
    }
}

fn writer_loop(
    directory: &Path,
    handle: &DiagnosticsHandle,
    policy: &SinkPolicy,
    receiver: &flume::Receiver<SinkCommand>,
) {
    let mut file = open_writer(directory, handle.run_id(), 0, handle.health.as_ref()).ok();
    let mut sequence = 0_u32;
    let mut bytes_in_file = 0_u64;
    let mut day = chrono::Utc::now().date_naive();
    handle.health.state.store(
        if file.is_some() {
            WriterState::Healthy
        } else {
            WriterState::Degraded
        } as u8,
        Ordering::Release,
    );

    while let Ok(command) = receiver.recv() {
        match command {
            SinkCommand::Shutdown => break,
            SinkCommand::Event(event) => {
                #[cfg(test)]
                if !policy.write_delay.is_zero() {
                    thread::sleep(policy.write_delay);
                }
                let Ok(mut encoded) = serde_json::to_vec(&event) else {
                    handle.health.dropped_events.fetch_add(1, Ordering::AcqRel);
                    continue;
                };
                encoded.push(b'\n');
                let current_day = chrono::Utc::now().date_naive();
                if bytes_in_file.saturating_add(encoded.len() as u64) > policy.rotate_bytes
                    || current_day != day
                {
                    if let Some(writer) = file.as_mut() {
                        let _ = writer.flush();
                    }
                    sequence = sequence.saturating_add(1);
                    file =
                        open_writer(directory, handle.run_id(), sequence, handle.health.as_ref())
                            .ok();
                    bytes_in_file = 0;
                    day = current_day;
                    enforce_retention(directory, policy);
                }
                let Some(writer) = file.as_mut() else {
                    handle.health.dropped_events.fetch_add(1, Ordering::AcqRel);
                    continue;
                };
                if writer.write_all(&encoded).is_err() {
                    handle
                        .health
                        .state
                        .store(WriterState::Degraded as u8, Ordering::Release);
                    handle.health.dropped_events.fetch_add(1, Ordering::AcqRel);
                    file = None;
                    continue;
                }
                bytes_in_file = bytes_in_file.saturating_add(encoded.len() as u64);
                handle
                    .health
                    .bytes_written
                    .fetch_add(encoded.len() as u64, Ordering::AcqRel);
            }
        }
    }
    if let Some(writer) = file.as_mut() {
        if writer.flush().is_err() {
            handle
                .health
                .state
                .store(WriterState::Degraded as u8, Ordering::Release);
        }
    }
    handle
        .health
        .state
        .store(WriterState::Stopped as u8, Ordering::Release);
}

fn open_writer(
    directory: &Path,
    run_id: &str,
    sequence: u32,
    health: &HealthAtoms,
) -> Result<BufWriter<File>> {
    let timestamp = chrono::Utc::now().format("%Y%m%dT%H%M%S%.3fZ");
    let short_run = run_id.get(..8).unwrap_or(run_id);
    for collision in 0_u16..=u16::MAX {
        let path = directory.join(format!(
            "{FILE_PREFIX}{timestamp}-{}-{short_run}-{sequence:03}-{collision:03}.jsonl",
            std::process::id()
        ));
        let mut options = OpenOptions::new();
        options.create_new(true).write(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt as _;
            options.mode(0o600);
        }
        match options.open(path) {
            Ok(file) => {
                health.files_created.fetch_add(1, Ordering::AcqRel);
                return Ok(BufWriter::new(file));
            }
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {}
            Err(error) => return Err(error.into()),
        }
    }
    anyhow::bail!("diagnostic file namespace exhausted")
}

fn enforce_retention(directory: &Path, policy: &SinkPolicy) {
    let Ok(entries) = std::fs::read_dir(directory) else {
        return;
    };
    let now = SystemTime::now();
    let mut files = entries
        .filter_map(Result::ok)
        .filter_map(|entry| {
            let path = entry.path();
            let name = path.file_name()?.to_str()?;
            if !name.starts_with(FILE_PREFIX)
                || path.extension().and_then(|value| value.to_str()) != Some("jsonl")
            {
                return None;
            }
            let metadata = entry.metadata().ok()?;
            let modified = metadata.modified().unwrap_or(SystemTime::UNIX_EPOCH);
            Some((path, modified, metadata.len()))
        })
        .collect::<Vec<_>>();
    files.sort_by_key(|(_, modified, _)| *modified);

    for (path, modified, _) in &files {
        if now
            .duration_since(*modified)
            .is_ok_and(|age| age > policy.retain_age)
        {
            let _ = std::fs::remove_file(path);
        }
    }
    files.retain(|(path, _, _)| path.exists());
    let mut total = files.iter().map(|(_, _, length)| *length).sum::<u64>();
    for (path, _, length) in files {
        if total <= policy.retain_bytes {
            break;
        }
        if std::fs::remove_file(path).is_ok() {
            total = total.saturating_sub(length);
        }
    }
}

fn bounded_text(value: &str, limit: usize) -> String {
    value
        .trim_matches('"')
        .chars()
        .filter(|character| !character.is_control())
        .take(limit)
        .collect()
}

fn bounded_token(value: &str) -> String {
    value
        .trim_matches('"')
        .chars()
        .filter(|character| {
            character.is_ascii_alphanumeric() || matches!(character, '_' | '-' | '.')
        })
        .take(64)
        .collect()
}

pub(crate) fn process_start_event(handle: &DiagnosticsHandle) -> DiagnosticEvent {
    let revision = option_env!("UNIFIED_PLAYER_GIT_REVISION").unwrap_or("unknown");
    let dirty = option_env!("UNIFIED_PLAYER_GIT_DIRTY").unwrap_or("unknown");
    let mut event = DiagnosticEvent::new(
        handle.run_id(),
        handle.started_at(),
        super::protocol::EventName::PROCESS_STARTED,
        super::protocol::EventCode::PROCESS_STARTED,
        Severity::Info,
        Component::Application,
        "Application diagnostics started",
    );
    event = event
        .with_operation(super::protocol::OperationContext::new(
            "startup",
            super::protocol::OperationSource::Startup,
        ))
        .with_outcome(super::protocol::OperationOutcome::Success)
        .with_duration(handle.started_at().elapsed());
    event.privacy_class = super::protocol::PrivacyClass::PublicBuild;
    event.fields.state = Some(format!(
        "version-{}-revision-{revision}-dirty-{dirty}-schema-{DIAGNOSTIC_SCHEMA_VERSION}",
        env!("CARGO_PKG_VERSION")
    ));
    event
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_directory(label: &str) -> std::path::PathBuf {
        let path = std::env::temp_dir().join(format!(
            "unified-player-{label}-{}-{}",
            std::process::id(),
            super::super::protocol::random_hex::<6>()
        ));
        std::fs::create_dir_all(&path).unwrap();
        path
    }

    fn event(handle: &DiagnosticsHandle, message: &'static str) -> DiagnosticEvent {
        DiagnosticEvent::new(
            handle.run_id(),
            handle.started_at(),
            super::super::protocol::EventName::REQUEST_STARTED,
            super::super::protocol::EventCode::REQUEST_STARTED,
            Severity::Info,
            Component::Application,
            message,
        )
    }

    #[test]
    fn temporary_verbose_tracing_filter_expires_and_restores_its_default() {
        let filter = DynamicFilterState::new(Severity::Info);
        assert_eq!(filter.snapshot(10, true).level, Severity::Info);
        filter.enable_verbose(10, Duration::from_secs(2));
        let active = filter.snapshot(11, true);
        assert_eq!(active.level, Severity::Trace);
        assert!(active.temporary);
        assert_eq!(active.remaining_seconds, 2);
        let expired = filter.snapshot(2_010, true);
        assert_eq!(expired.level, Severity::Info);
        assert!(!expired.temporary);
        assert_eq!(expired.remaining_seconds, 0);
    }

    #[test]
    fn core_typed_events_remain_on_when_verbose_tracing_is_filtered() {
        let ring = Arc::new(Mutex::new(VecDeque::new()));
        let (handle, _runtime) = disabled(ring.clone());
        assert_eq!(handle.filter_snapshot().level, Severity::Error);
        assert!(!handle.allows(Severity::Debug));

        let mut typed = event(&handle, "Core causality event");
        typed.severity = Severity::Debug;
        handle.record(typed);

        let ring = ring.lock();
        assert_eq!(ring.len(), 1);
        assert_eq!(ring.front().unwrap().code, "REQUEST_STARTED");
    }

    #[test]
    fn disabled_diagnostics_reject_verbose_tracing_without_changing_filter_state() {
        let ring = Arc::new(Mutex::new(VecDeque::new()));
        let (handle, _runtime) = disabled(ring);

        assert_eq!(handle.enable_verbose(Duration::from_secs(30)), None);
        let snapshot = handle.filter_snapshot();
        assert!(!snapshot.available);
        assert!(!snapshot.temporary);
        assert_eq!(snapshot.level, Severity::Error);
    }

    #[test]
    fn local_action_failure_records_safe_incident_when_file_sink_is_disabled() {
        let ring = Arc::new(Mutex::new(VecDeque::new()));
        let (handle, _runtime) = disabled(ring);
        let incident = handle.record_local_action_failure().unwrap();
        assert_eq!(incident.safe_event_code(), "DIAGNOSTIC_ACTION_FAILED");
        assert_eq!(incident.component, Component::Support);
        assert!(incident.safe_reference().starts_with("I-"));
        assert_eq!(
            incident.safe_cause(),
            "A required local resource was unavailable"
        );
    }

    #[test]
    fn writer_lifecycle_is_idempotent_and_jsonl_is_valid() {
        let directory = temp_directory("diagnostic-lifecycle");
        let ring = Arc::new(Mutex::new(VecDeque::new()));
        let (handle, mut runtime) = start(&directory, ring, SinkPolicy::default()).unwrap();
        handle.record(event(&handle, "safe message"));
        runtime.shutdown(Duration::from_secs(1)).unwrap();
        runtime.shutdown(Duration::from_secs(1)).unwrap();

        let files = std::fs::read_dir(&directory)
            .unwrap()
            .filter_map(Result::ok)
            .map(|entry| entry.path())
            .collect::<Vec<_>>();
        assert_eq!(files.len(), 1);
        let contents = std::fs::read_to_string(&files[0]).unwrap();
        let decoded: DiagnosticEvent = serde_json::from_str(contents.trim()).unwrap();
        assert_eq!(decoded.event_code, "REQUEST_STARTED");
        assert_eq!(handle.health().state, WriterState::Stopped);
        std::fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn names_do_not_collide_and_small_files_rotate() {
        let directory = temp_directory("diagnostic-rotation");
        let policy = SinkPolicy {
            rotate_bytes: 1,
            ..SinkPolicy::default()
        };
        let ring = Arc::new(Mutex::new(VecDeque::new()));
        let (handle, mut runtime) = start(&directory, ring, policy).unwrap();
        handle.record(event(&handle, "first"));
        handle.record(event(&handle, "second"));
        runtime.shutdown(Duration::from_secs(1)).unwrap();
        let names = std::fs::read_dir(&directory)
            .unwrap()
            .filter_map(Result::ok)
            .map(|entry| entry.file_name())
            .collect::<std::collections::HashSet<_>>();
        assert!(names.len() >= 2);
        assert_eq!(names.len() as u64, handle.health().files_created);
        std::fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn retention_never_deletes_unrelated_files() {
        let directory = temp_directory("diagnostic-retention");
        let unrelated = directory.join("keep-me.log");
        std::fs::write(&unrelated, b"private legacy file").unwrap();
        for index in 0..3 {
            std::fs::write(
                directory.join(format!("{FILE_PREFIX}fixture-{index}.jsonl")),
                vec![b'x'; 8],
            )
            .unwrap();
        }
        enforce_retention(
            &directory,
            &SinkPolicy {
                retain_bytes: 8,
                ..SinkPolicy::default()
            },
        );
        assert!(unrelated.exists());
        let retained = std::fs::read_dir(&directory)
            .unwrap()
            .filter_map(Result::ok)
            .filter(|entry| entry.file_name().to_string_lossy().starts_with(FILE_PREFIX))
            .count();
        assert_eq!(retained, 1);
        std::fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn ui_ring_is_structured_and_bounded() {
        let directory = temp_directory("diagnostic-ring");
        let ring = Arc::new(Mutex::new(VecDeque::new()));
        let policy = SinkPolicy {
            ui_capacity: 2,
            ..SinkPolicy::default()
        };
        let (handle, mut runtime) = start(&directory, ring.clone(), policy).unwrap();
        for message in ["one", "two", "three"] {
            handle.record(event(&handle, message));
        }
        runtime.shutdown(Duration::from_secs(1)).unwrap();
        let ring = ring.lock();
        assert_eq!(ring.len(), 2);
        assert_eq!(ring.front().unwrap().message, "two");
        assert_eq!(ring.back().unwrap().code, "REQUEST_STARTED");
        drop(ring);
        std::fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn saturated_writer_reports_dropped_events_without_blocking_callers() {
        let directory = temp_directory("diagnostic-drops");
        let ring = Arc::new(Mutex::new(VecDeque::new()));
        let policy = SinkPolicy {
            channel_capacity: 1,
            write_delay: Duration::from_millis(20),
            ..SinkPolicy::default()
        };
        let (handle, mut runtime) = start(&directory, ring, policy).unwrap();
        let started = Instant::now();
        for _ in 0..100 {
            handle.record(event(&handle, "bounded"));
        }
        assert!(started.elapsed() < Duration::from_millis(100));
        assert!(handle.health().dropped_events > 0);
        runtime.shutdown(Duration::from_secs(2)).unwrap();
        std::fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn shutdown_deadline_is_bounded_and_can_be_retried() {
        let directory = temp_directory("diagnostic-deadline");
        let ring = Arc::new(Mutex::new(VecDeque::new()));
        let policy = SinkPolicy {
            write_delay: Duration::from_millis(100),
            ..SinkPolicy::default()
        };
        let (handle, mut runtime) = start(&directory, ring, policy).unwrap();
        handle.record(event(&handle, "slow"));
        let started = Instant::now();
        assert!(runtime.shutdown(Duration::from_millis(1)).is_err());
        assert!(started.elapsed() < Duration::from_millis(50));
        runtime.shutdown(Duration::from_secs(1)).unwrap();
        std::fs::remove_dir_all(directory).unwrap();
    }
}
