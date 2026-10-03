use std::{future::Future, time::Duration};

use super::{
    record, Component, DiagnosticEvent, EventCode, EventName, OperationContext, OperationOutcome,
    OperationSource, Severity,
};

tokio::task_local! {
    static CURRENT_OPERATION: OperationContext;
}

pub(crate) fn current_operation() -> Option<OperationContext> {
    CURRENT_OPERATION.try_with(Clone::clone).ok()
}

pub(crate) async fn in_operation<F: Future>(context: OperationContext, future: F) -> F::Output {
    CURRENT_OPERATION.scope(context, future).await
}

pub(crate) fn child_or_new(operation: &'static str, source: OperationSource) -> OperationContext {
    current_operation().map_or_else(
        || OperationContext::new(operation, source),
        |context| context.child(),
    )
}

pub(crate) fn request_accepted(context: &OperationContext, queue_depth: usize) {
    let mut event = event(
        EventName::REQUEST_ACCEPTED,
        EventCode::REQUEST_ACCEPTED,
        Severity::Debug,
        "Request accepted",
        context,
    );
    event.fields.queue_depth = queue_depth.try_into().ok();
    record(event);
}

pub(crate) fn request_started(
    context: &OperationContext,
    queue_wait: Duration,
    queue_depth: usize,
) {
    let mut event = event(
        EventName::REQUEST_STARTED,
        EventCode::REQUEST_STARTED,
        Severity::Debug,
        "Request started",
        context,
    );
    event.fields.queue_wait_ms = queue_wait.as_millis().try_into().ok();
    event.fields.queue_depth = queue_depth.try_into().ok();
    record(event);
    enforce_performance_budget(
        Component::Scheduler,
        "request_queue_wait",
        queue_wait,
        Duration::from_secs(1),
    );
}

pub(crate) fn request_completed(
    context: &OperationContext,
    outcome: OperationOutcome,
    elapsed: Duration,
    cancellation_reason: Option<&'static str>,
) {
    let severity = match outcome {
        OperationOutcome::Success | OperationOutcome::Cancelled | OperationOutcome::Superseded => {
            Severity::Debug
        }
        OperationOutcome::Rejected => Severity::Warn,
        OperationOutcome::Error
        | OperationOutcome::Timeout
        | OperationOutcome::Panicked
        | OperationOutcome::Aborted => Severity::Error,
    };
    let mut event = event(
        EventName::REQUEST_COMPLETED,
        EventCode::REQUEST_COMPLETED,
        severity,
        "Request completed",
        context,
    )
    .with_outcome(outcome)
    .with_duration(elapsed);
    event.fields.cancellation_reason = cancellation_reason.map(str::to_owned);
    record(event);
    enforce_performance_budget(
        Component::Scheduler,
        "request_total",
        elapsed,
        Duration::from_secs(15),
    );
}

pub(crate) fn operation_stage(
    component: Component,
    stage: &'static str,
    elapsed: Option<Duration>,
    outcome: Option<OperationOutcome>,
) {
    let Some(context) = current_operation() else {
        return;
    };
    operation_stage_detail_for(
        &context, component, stage, elapsed, outcome, None, None, None, None,
    );
}

/// Record a stage with bounded, compile-time safe detail fields. Callers must
/// pass static tokens; provider payloads, URLs, and error text do not belong
/// in the operational diagnostics stream.
pub(crate) fn operation_stage_detail(
    component: Component,
    stage: &'static str,
    elapsed: Option<Duration>,
    outcome: Option<OperationOutcome>,
    phase: Option<&'static str>,
    status_class: Option<&'static str>,
    error_type: Option<&'static str>,
    selection_index: Option<u64>,
) {
    let Some(context) = current_operation() else {
        return;
    };
    operation_stage_detail_for(
        &context,
        component,
        stage,
        elapsed,
        outcome,
        phase,
        status_class,
        error_type,
        selection_index,
    );
}

pub(crate) fn operation_stage_detail_for(
    context: &OperationContext,
    component: Component,
    stage: &'static str,
    elapsed: Option<Duration>,
    outcome: Option<OperationOutcome>,
    phase: Option<&'static str>,
    status_class: Option<&'static str>,
    error_type: Option<&'static str>,
    selection_index: Option<u64>,
) {
    let Some(handle) = super::handle() else {
        return;
    };
    let mut event = DiagnosticEvent::new(
        handle.run_id(),
        handle.started_at(),
        EventName::OPERATION_STAGE,
        EventCode::OPERATION_STAGE,
        Severity::Debug,
        component,
        "Operation stage",
    )
    .with_operation(context.child());
    event.fields.state = Some(stage.to_owned());
    event.fields.outcome = outcome;
    event.fields.duration_ms = elapsed.and_then(|value| value.as_millis().try_into().ok());
    event.fields.phase = phase.map(str::to_owned);
    event.fields.status_class = status_class.map(str::to_owned);
    event.fields.error_type = error_type.map(str::to_owned);
    event.fields.selection_index = selection_index;
    record(event);
    if let (Some(elapsed), Some(budget)) = (elapsed, stage_budget(stage)) {
        enforce_performance_budget(component, stage, elapsed, budget);
    }
}

pub(crate) fn worker_transition(
    worker: &'static str,
    state: &'static str,
    outcome: Option<OperationOutcome>,
) {
    let Some(handle) = super::handle() else {
        return;
    };
    let mut event = DiagnosticEvent::new(
        handle.run_id(),
        handle.started_at(),
        EventName::WORKER_TRANSITION,
        EventCode::WORKER_TRANSITION,
        if matches!(
            outcome,
            Some(OperationOutcome::Error | OperationOutcome::Panicked)
        ) {
            Severity::Error
        } else {
            Severity::Debug
        },
        Component::Runtime,
        "Worker lifecycle changed",
    );
    event.fields.worker = Some(worker.to_owned());
    event.fields.state = Some(state.to_owned());
    event.fields.outcome = outcome;
    record(event);
}

pub(crate) fn shutdown_stage(context: &OperationContext, stage: &'static str, elapsed: Duration) {
    let Some(handle) = super::handle() else {
        return;
    };
    let mut event = DiagnosticEvent::new(
        handle.run_id(),
        handle.started_at(),
        EventName::OPERATION_STAGE,
        EventCode::OPERATION_STAGE,
        Severity::Debug,
        Component::Runtime,
        "Shutdown stage completed",
    )
    .with_operation(context.child());
    event.fields.state = Some(stage.to_owned());
    event.fields.duration_ms = elapsed.as_millis().try_into().ok();
    if stage == "workers_joined" {
        event.fields.outcome = Some(OperationOutcome::Success);
    }
    record(event);
    enforce_performance_budget(
        Component::Runtime,
        stage,
        elapsed,
        if stage == "browser_closed" {
            Duration::from_secs(5)
        } else {
            Duration::from_secs(2)
        },
    );
}

pub(crate) fn ui_render_sample(elapsed: Duration, revision: u64, slow: bool) {
    let Some(handle) = super::handle() else {
        return;
    };
    let mut event = DiagnosticEvent::new(
        handle.run_id(),
        handle.started_at(),
        EventName::UI_TRANSITION,
        EventCode::UI_TRANSITION,
        Severity::Debug,
        Component::Ui,
        if slow {
            "UI render exceeded its responsiveness budget"
        } else {
            "UI render timing sample"
        },
    );
    event.fields.state = Some("render".to_owned());
    event.fields.generation = Some(revision);
    event.fields.duration_ms = elapsed.as_millis().try_into().ok();
    record(event);
    enforce_performance_budget(
        Component::Ui,
        "ui_render",
        elapsed,
        Duration::from_millis(50),
    );
}

fn stage_budget(stage: &str) -> Option<Duration> {
    match stage {
        "auth_session" => Some(Duration::from_secs(15)),
        "provider_read" => Some(Duration::from_secs(8)),
        "playback_coordination" => Some(Duration::from_secs(5)),
        "playlist_mutation" => Some(Duration::from_secs(10)),
        "state_applied" | "youtube_queue_handoff" => Some(Duration::from_millis(100)),
        "youtube_audio_ready" => Some(Duration::from_millis(1_500)),
        "youtube_client_material" | "youtube_media_probe" | "youtube_source_resolve" => {
            Some(Duration::from_secs(1))
        }
        "youtube_media_decode" => Some(Duration::from_millis(500)),
        _ => None,
    }
}

fn enforce_performance_budget(
    component: Component,
    budget_name: &'static str,
    elapsed: Duration,
    budget: Duration,
) {
    if !budget_exceeded(elapsed, budget) {
        return;
    }
    let Some(handle) = super::handle() else {
        return;
    };
    let mut event = DiagnosticEvent::new(
        handle.run_id(),
        handle.started_at(),
        EventName::PERFORMANCE_BUDGET,
        EventCode::PERFORMANCE_BUDGET,
        Severity::Warn,
        component,
        "Local performance budget exceeded",
    );
    event.fields.state = Some(budget_name.to_owned());
    event.fields.duration_ms = elapsed.as_millis().try_into().ok();
    event.fields.budget_ms = budget.as_millis().try_into().ok();
    event.fields.outcome = Some(OperationOutcome::Timeout);
    if let Some(context) = current_operation() {
        event.operation = Some(context.child());
    }
    record(event);
}

fn budget_exceeded(elapsed: Duration, budget: Duration) -> bool {
    elapsed > budget
}

fn event(
    name: EventName,
    code: EventCode,
    severity: Severity,
    message: &'static str,
    context: &OperationContext,
) -> DiagnosticEvent {
    let Some(handle) = super::handle() else {
        return DiagnosticEvent::new(
            "uninitialized",
            std::time::Instant::now(),
            name,
            code,
            severity,
            Component::Scheduler,
            message,
        )
        .with_operation(context.clone());
    };
    DiagnosticEvent::new(
        handle.run_id(),
        handle.started_at(),
        name,
        code,
        severity,
        Component::Scheduler,
        message,
    )
    .with_operation(context.clone())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn child_context_preserves_trace_and_changes_span() {
        let parent = OperationContext::new("play", OperationSource::Terminal);
        let child = in_operation(parent.clone(), async {
            child_or_new("ignored", OperationSource::System)
        })
        .await;
        assert_eq!(child.trace_id, parent.trace_id);
        assert_eq!(child.operation, parent.operation);
        assert_eq!(child.source, parent.source);
        assert_ne!(child.span_id, parent.span_id);
    }

    #[test]
    fn performance_budgets_are_fixed_and_enforced_at_the_boundary() {
        assert_eq!(stage_budget("provider_read"), Some(Duration::from_secs(8)));
        assert_eq!(
            stage_budget("state_applied"),
            Some(Duration::from_millis(100))
        );
        assert_eq!(
            stage_budget("youtube_audio_ready"),
            Some(Duration::from_millis(1_500))
        );
        assert_eq!(
            stage_budget("youtube_media_decode"),
            Some(Duration::from_millis(500))
        );
        assert_eq!(
            stage_budget("youtube_queue_handoff"),
            Some(Duration::from_millis(100))
        );
        assert!(!budget_exceeded(
            Duration::from_millis(50),
            Duration::from_millis(50)
        ));
        assert!(budget_exceeded(
            Duration::from_millis(51),
            Duration::from_millis(50)
        ));
    }
}
