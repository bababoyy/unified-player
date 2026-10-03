use std::{
    future::Future,
    panic::{self, AssertUnwindSafe},
    sync::{Arc, Mutex},
    thread,
    time::{Duration, Instant},
};

use anyhow::{Context as _, Result};
use futures::FutureExt as _;
use tokio::task::JoinSet;
use tokio_util::sync::CancellationToken;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u8)]
pub(crate) enum ShutdownPhase {
    Running = 0,
    InputStopped,
    IngressStopped,
    WorkCancelled,
    PlaybackStopped,
    SessionsPersisted,
    BrowserClosed,
    TerminalRestored,
    WorkersJoined,
}

impl ShutdownPhase {
    const fn label(self) -> &'static str {
        match self {
            Self::Running => "running",
            Self::InputStopped => "input_stopped",
            Self::IngressStopped => "ingress_stopped",
            Self::WorkCancelled => "work_cancelled",
            Self::PlaybackStopped => "playback_stopped",
            Self::SessionsPersisted => "sessions_persisted",
            Self::BrowserClosed => "browser_closed",
            Self::TerminalRestored => "terminal_restored",
            Self::WorkersJoined => "workers_joined",
        }
    }
}

#[derive(Clone, Debug)]
pub(crate) struct ShutdownProgress {
    phases: Arc<Mutex<Vec<ShutdownPhase>>>,
    timing: Arc<Mutex<ShutdownTiming>>,
}

#[derive(Debug, Default)]
struct ShutdownTiming {
    stage_started_at: Option<Instant>,
    context: Option<crate::observability::OperationContext>,
}

impl Default for ShutdownProgress {
    fn default() -> Self {
        Self {
            phases: Arc::new(Mutex::new(vec![ShutdownPhase::Running])),
            timing: Arc::new(Mutex::new(ShutdownTiming::default())),
        }
    }
}

impl ShutdownProgress {
    pub(crate) fn advance(&self, phase: ShutdownPhase) -> Result<()> {
        self.advance_at(phase, Instant::now()).map(|_| ())
    }

    fn advance_at(&self, phase: ShutdownPhase, now: Instant) -> Result<Duration> {
        let mut phases = self
            .phases
            .lock()
            .expect("shutdown progress mutex poisoned");
        let current = *phases.last().expect("shutdown progress is non-empty");
        if phase as u8 <= current as u8 {
            return Ok(Duration::ZERO);
        }
        anyhow::ensure!(
            phase as u8 == current as u8 + 1,
            "invalid shutdown phase transition"
        );
        phases.push(phase);
        drop(phases);
        let mut timing = self.timing.lock().expect("shutdown timing mutex poisoned");
        let elapsed = timing
            .stage_started_at
            .map_or(Duration::ZERO, |started_at| {
                now.saturating_duration_since(started_at)
            });
        let context = timing
            .context
            .get_or_insert_with(|| {
                crate::observability::OperationContext::new(
                    "shutdown",
                    crate::observability::OperationSource::Runtime,
                )
            })
            .clone();
        timing.stage_started_at = Some(now);
        drop(timing);
        crate::observability::shutdown_stage(&context, phase.label(), elapsed);
        Ok(elapsed)
    }

    #[cfg(test)]
    fn phases(&self) -> Vec<ShutdownPhase> {
        self.phases
            .lock()
            .expect("shutdown progress mutex poisoned")
            .clone()
    }
}

pub(crate) struct AppRuntime {
    shutdown_requested: CancellationToken,
    ingress_shutdown: CancellationToken,
    work_shutdown: CancellationToken,
    async_workers: JoinSet<()>,
    thread_workers: Vec<thread::JoinHandle<()>>,
    thread_completion_tx: flume::Sender<()>,
    thread_completion_rx: flume::Receiver<()>,
    diagnostics: Option<crate::observability::DiagnosticsRuntime>,
}

impl AppRuntime {
    pub(crate) fn new(shutdown_requested: CancellationToken) -> Self {
        let (thread_completion_tx, thread_completion_rx) = flume::bounded(16);
        Self {
            shutdown_requested,
            ingress_shutdown: CancellationToken::new(),
            work_shutdown: CancellationToken::new(),
            async_workers: JoinSet::new(),
            thread_workers: Vec::new(),
            thread_completion_tx,
            thread_completion_rx,
            diagnostics: None,
        }
    }

    pub(crate) fn attach_diagnostics(
        &mut self,
        diagnostics: crate::observability::DiagnosticsRuntime,
    ) {
        self.diagnostics = Some(diagnostics);
    }

    pub(crate) fn ingress_token(&self) -> CancellationToken {
        self.ingress_shutdown.clone()
    }

    pub(crate) fn work_token(&self) -> CancellationToken {
        self.work_shutdown.clone()
    }

    pub(crate) fn stop_ingress(&self) {
        self.ingress_shutdown.cancel();
    }

    pub(crate) fn cancel_work(&self) {
        self.work_shutdown.cancel();
    }

    pub(crate) fn spawn_async<F>(&mut self, name: &'static str, critical: bool, worker: F)
    where
        F: Future<Output = Result<()>> + Send + 'static,
    {
        crate::observability::worker_transition(name, "started", None);
        let shutdown = self.shutdown_requested.clone();
        self.async_workers.spawn(async move {
            let outcome = AssertUnwindSafe(worker).catch_unwind().await;
            report_worker_exit(name, critical, &shutdown, &outcome);
        });
    }

    pub(crate) fn spawn_thread<F>(
        &mut self,
        name: &'static str,
        critical: bool,
        worker: F,
    ) -> Result<()>
    where
        F: FnOnce() -> Result<()> + Send + 'static,
    {
        let shutdown = self.shutdown_requested.clone();
        let completion = self.thread_completion_tx.clone();
        crate::observability::worker_transition(name, "started", None);
        let handle = thread::Builder::new()
            .name(name.to_string())
            .spawn(move || {
                let outcome = panic::catch_unwind(AssertUnwindSafe(worker));
                report_worker_exit(name, critical, &shutdown, &outcome);
                let _ = completion.send(());
            })
            .with_context(|| format!("spawn runtime worker {name}"))?;
        self.thread_workers.push(handle);
        Ok(())
    }

    pub(crate) async fn join<F>(
        mut self,
        timeout: Duration,
        before_diagnostics_shutdown: F,
    ) -> Result<()>
    where
        F: FnOnce() -> Result<()>,
    {
        let deadline = tokio::time::Instant::now() + timeout;
        while !self.async_workers.is_empty() {
            let result = tokio::time::timeout_at(deadline, self.async_workers.join_next())
                .await
                .context("runtime workers exceeded the shutdown deadline")?
                .expect("runtime worker set is non-empty");
            anyhow::ensure!(result.is_ok(), "a runtime worker could not be joined");
        }

        for _ in 0..self.thread_workers.len() {
            tokio::time::timeout_at(deadline, self.thread_completion_rx.recv_async())
                .await
                .context("runtime threads exceeded the shutdown deadline")?
                .context("runtime thread completion channel closed")?;
        }
        for handle in self.thread_workers {
            anyhow::ensure!(
                handle.join().is_ok(),
                "a runtime thread could not be joined"
            );
        }
        before_diagnostics_shutdown()?;
        if let Some(mut diagnostics) = self.diagnostics {
            let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
            if diagnostics.shutdown(remaining).is_err() {
                diagnostics.detach_after_timeout();
            }
        }
        Ok(())
    }
}

fn report_worker_exit<T>(
    name: &'static str,
    critical: bool,
    shutdown: &CancellationToken,
    outcome: &std::thread::Result<Result<T>>,
) {
    let (status, succeeded) = match outcome {
        Ok(Ok(_)) => ("stopped", true),
        Ok(Err(_)) => ("failed", false),
        Err(_) => ("panicked", false),
    };
    let diagnostic_outcome = match outcome {
        Ok(Ok(_)) => Some(crate::observability::OperationOutcome::Success),
        Ok(Err(_)) => Some(crate::observability::OperationOutcome::Error),
        Err(_) => Some(crate::observability::OperationOutcome::Panicked),
    };
    crate::observability::worker_transition(name, status, diagnostic_outcome);
    if shutdown.is_cancelled() {
        if !succeeded {
            tracing::error!(
                worker = name,
                status,
                "Runtime worker failed during shutdown"
            );
        }
        return;
    }
    if critical {
        tracing::error!(
            worker = name,
            status,
            "Critical runtime worker exited unexpectedly"
        );
        shutdown.cancel();
    } else {
        tracing::warn!(
            worker = name,
            status,
            "Auxiliary runtime worker exited unexpectedly"
        );
    }
}

#[cfg(test)]
mod tests {
    use super::{AppRuntime, ShutdownPhase, ShutdownProgress};
    use std::{
        collections::VecDeque,
        sync::Arc,
        time::{Duration, Instant},
    };
    use tokio_util::sync::CancellationToken;

    #[test]
    fn shutdown_progress_is_ordered_and_idempotent() {
        let progress = ShutdownProgress::default();
        for phase in [
            ShutdownPhase::InputStopped,
            ShutdownPhase::IngressStopped,
            ShutdownPhase::WorkCancelled,
            ShutdownPhase::PlaybackStopped,
            ShutdownPhase::SessionsPersisted,
            ShutdownPhase::BrowserClosed,
            ShutdownPhase::TerminalRestored,
            ShutdownPhase::WorkersJoined,
        ] {
            progress.advance(phase).unwrap();
            progress.advance(phase).unwrap();
        }
        progress.advance(ShutdownPhase::InputStopped).unwrap();
        assert_eq!(progress.phases().len(), 9);
    }

    #[test]
    fn shutdown_progress_rejects_out_of_order_completion() {
        let progress = ShutdownProgress::default();
        assert!(progress.advance(ShutdownPhase::WorkCancelled).is_err());
        assert_eq!(progress.phases(), vec![ShutdownPhase::Running]);
    }

    #[test]
    fn shutdown_timing_starts_on_request_after_an_idle_runtime() {
        let progress = ShutdownProgress::default();
        let clock = Instant::now() + Duration::from_secs(24 * 60 * 60);

        assert_eq!(
            progress
                .advance_at(ShutdownPhase::InputStopped, clock)
                .unwrap(),
            Duration::ZERO
        );
        assert_eq!(
            progress
                .advance_at(
                    ShutdownPhase::IngressStopped,
                    clock + Duration::from_millis(17),
                )
                .unwrap(),
            Duration::from_millis(17)
        );
    }

    #[tokio::test]
    async fn normal_shutdown_cancels_and_joins_active_workers() {
        let shutdown_requested = CancellationToken::new();
        let mut runtime = AppRuntime::new(shutdown_requested.clone());
        let work_shutdown = runtime.work_token();
        let stopped = Arc::new(std::sync::atomic::AtomicBool::new(false));
        runtime.spawn_async("active-test-worker", true, {
            let stopped = stopped.clone();
            async move {
                work_shutdown.cancelled().await;
                stopped.store(true, std::sync::atomic::Ordering::Release);
                Ok(())
            }
        });

        shutdown_requested.cancel();
        runtime.cancel_work();
        runtime
            .join(Duration::from_secs(1), || Ok(()))
            .await
            .unwrap();
        assert!(stopped.load(std::sync::atomic::Ordering::Acquire));
    }

    #[tokio::test]
    async fn normal_shutdown_joins_native_threads() {
        let shutdown_requested = CancellationToken::new();
        let mut runtime = AppRuntime::new(shutdown_requested.clone());
        let work_shutdown = runtime.work_token();
        let stopped = Arc::new(std::sync::atomic::AtomicBool::new(false));
        runtime
            .spawn_thread("native-test-worker", true, {
                let stopped = stopped.clone();
                move || {
                    while !work_shutdown.is_cancelled() {
                        std::thread::sleep(Duration::from_millis(1));
                    }
                    stopped.store(true, std::sync::atomic::Ordering::Release);
                    Ok(())
                }
            })
            .unwrap();

        shutdown_requested.cancel();
        runtime.cancel_work();
        runtime
            .join(Duration::from_secs(1), || Ok(()))
            .await
            .unwrap();
        assert!(stopped.load(std::sync::atomic::Ordering::Acquire));
    }

    #[cfg(feature = "private-capture")]
    #[tokio::test]
    async fn private_capture_writer_is_owned_and_has_bounded_shutdown() {
        let directory = tempfile::tempdir().unwrap();
        let (handle, worker, _) = crate::developer_capture::prepare_runtime(
            directory.path().join("vault"),
            crate::developer_capture::CaptureLimits::default(),
        )
        .unwrap();
        let shutdown_requested = CancellationToken::new();
        let capture_shutdown = CancellationToken::new();
        let mut runtime = AppRuntime::new(shutdown_requested.clone());
        let worker_shutdown = capture_shutdown.clone();
        runtime
            .spawn_thread("private-capture-test-writer", false, move || {
                worker.run(&worker_shutdown).map_err(Into::into)
            })
            .unwrap();

        assert_eq!(
            handle.snapshot().state,
            crate::developer_capture::SafeCaptureState::Inactive
        );
        shutdown_requested.cancel();
        capture_shutdown.cancel();
        runtime
            .join(Duration::from_secs(2), || Ok(()))
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn final_worker_stage_is_persisted_before_diagnostic_writer_shutdown() {
        let directory = tempfile::tempdir().unwrap();
        let ring = Arc::new(parking_lot::Mutex::new(VecDeque::new()));
        let (handle, diagnostics) = crate::observability::start(directory.path(), ring).unwrap();
        let mut runtime = AppRuntime::new(CancellationToken::new());
        runtime.attach_diagnostics(diagnostics);

        runtime
            .join(Duration::from_secs(1), || {
                let mut event = crate::observability::DiagnosticEvent::new(
                    handle.run_id(),
                    handle.started_at(),
                    crate::observability::EventName::OPERATION_STAGE,
                    crate::observability::EventCode::OPERATION_STAGE,
                    crate::observability::Severity::Info,
                    crate::observability::Component::Runtime,
                    "Runtime workers joined",
                );
                event.fields.state = Some("workers_joined".to_owned());
                handle.record(event);
                Ok(())
            })
            .await
            .unwrap();

        assert_eq!(handle.health().dropped_events, 0);
        let persisted = std::fs::read_dir(directory.path())
            .unwrap()
            .filter_map(Result::ok)
            .filter(|entry| {
                entry
                    .path()
                    .extension()
                    .is_some_and(|value| value == "jsonl")
            })
            .map(|entry| std::fs::read_to_string(entry.path()).unwrap())
            .collect::<String>();
        assert!(persisted.contains("workers_joined"));
    }

    #[tokio::test]
    async fn critical_worker_failure_requests_shutdown_without_error_details() {
        let shutdown = CancellationToken::new();
        let mut runtime = AppRuntime::new(shutdown.clone());
        runtime.spawn_async("failing-test-worker", true, async {
            anyhow::bail!("sensitive provider detail")
        });

        tokio::time::timeout(Duration::from_secs(1), shutdown.cancelled())
            .await
            .unwrap();
        runtime
            .join(Duration::from_secs(1), || Ok(()))
            .await
            .unwrap();
    }

    #[test]
    fn ingress_and_work_shutdown_are_independent_and_orderable() {
        let runtime = AppRuntime::new(CancellationToken::new());
        let ingress = runtime.ingress_token();
        let work = runtime.work_token();

        runtime.stop_ingress();
        assert!(ingress.is_cancelled());
        assert!(!work.is_cancelled());

        runtime.cancel_work();
        assert!(work.is_cancelled());
    }

    #[tokio::test]
    async fn join_has_a_bounded_deadline() {
        let shutdown = CancellationToken::new();
        let mut runtime = AppRuntime::new(shutdown.clone());
        runtime.spawn_async("stuck-test-worker", true, async {
            std::future::pending::<()>().await;
            Ok(())
        });
        shutdown.cancel();

        assert!(runtime
            .join(Duration::from_millis(20), || Ok(()))
            .await
            .is_err());
    }
}
