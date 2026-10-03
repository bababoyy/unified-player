use std::collections::{BTreeMap, VecDeque};

use parking_lot::RwLock;
use serde::Serialize;

use super::{Component, DiagnosticEvent, OperationOutcome, Severity, WriterHealth, WriterState};

const MAX_ACTIVE_OPERATIONS: usize = 64;
const MAX_WORKERS: usize = 32;
const MAX_TRANSITIONS_PER_TARGET: usize = 16;

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum HealthStatus {
    Starting,
    Healthy,
    Busy,
    Degraded,
    Disabled,
    Stopped,
    Unknown,
}

impl HealthStatus {
    pub(crate) const fn label(self) -> &'static str {
        match self {
            Self::Starting => "starting",
            Self::Healthy => "healthy",
            Self::Busy => "busy",
            Self::Degraded => "degraded",
            Self::Disabled => "disabled",
            Self::Stopped => "stopped",
            Self::Unknown => "unknown",
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub(crate) struct ComponentHealth {
    pub(crate) component: Component,
    pub(crate) status: HealthStatus,
    pub(crate) fact: String,
    pub(crate) updated_uptime_ms: u64,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub(crate) struct WorkerHealth {
    pub(crate) worker: String,
    pub(crate) status: HealthStatus,
    pub(crate) updated_uptime_ms: u64,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub(crate) struct HealthTransition {
    pub(crate) status: HealthStatus,
    pub(crate) fact: String,
    pub(crate) uptime_ms: u64,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub(crate) struct OperationHealth {
    pub(crate) operation: String,
    pub(crate) reference: String,
    pub(crate) state: String,
    pub(crate) outcome: Option<OperationOutcome>,
    pub(crate) updated_uptime_ms: u64,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub(crate) struct HealthSnapshot {
    pub(crate) components: Vec<ComponentHealth>,
    pub(crate) workers: Vec<WorkerHealth>,
    pub(crate) active_operation: Option<OperationHealth>,
    pub(crate) last_operation: Option<OperationHealth>,
    pub(crate) dropped_events: u64,
    pub(crate) logging_status: HealthStatus,
    pub(crate) ui: Option<UiDiagnosticSnapshot>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub(crate) struct UiDiagnosticSnapshot {
    pub(crate) revision: u64,
    pub(crate) page: String,
    pub(crate) popup: String,
    pub(crate) selection: Option<u64>,
    pub(crate) loading: bool,
    pub(crate) meaningful_state: String,
    pub(crate) provider: String,
    pub(crate) lifecycle: String,
}

impl UiDiagnosticSnapshot {
    fn semantically_eq(&self, other: &Self) -> bool {
        self.page == other.page
            && self.popup == other.popup
            && self.selection == other.selection
            && self.loading == other.loading
            && self.meaningful_state == other.meaningful_state
            && self.provider == other.provider
            && self.lifecycle == other.lifecycle
    }
}

#[derive(Default)]
struct HealthState {
    components: BTreeMap<Component, ComponentHealth>,
    workers: BTreeMap<String, WorkerHealth>,
    active: BTreeMap<String, OperationHealth>,
    last_operation: Option<OperationHealth>,
    ui: Option<UiDiagnosticSnapshot>,
    component_history: BTreeMap<Component, VecDeque<HealthTransition>>,
    worker_history: BTreeMap<String, VecDeque<HealthTransition>>,
}

#[derive(Clone, Default)]
pub(crate) struct HealthRegistry {
    state: std::sync::Arc<RwLock<HealthState>>,
}

impl HealthRegistry {
    pub(crate) fn set_component(
        &self,
        component: Component,
        status: HealthStatus,
        fact: &'static str,
        uptime_ms: u64,
    ) {
        let mut state = self.state.write();
        let next = ComponentHealth {
            component,
            status,
            fact: fact.to_owned(),
            updated_uptime_ms: uptime_ms,
        };
        if state
            .components
            .get(&component)
            .is_none_or(|current| current.status != next.status || current.fact != next.fact)
        {
            push_transition(
                state.component_history.entry(component).or_default(),
                HealthTransition {
                    status,
                    fact: fact.to_owned(),
                    uptime_ms,
                },
            );
        }
        state.components.insert(component, next);
    }

    pub(crate) fn observe(&self, event: &DiagnosticEvent) {
        let mut state = self.state.write();
        let previous_components = state.components.clone();
        let previous_workers = state.workers.clone();
        match event.event_name.as_str() {
            "request.accepted" | "request.started" => {
                if let Some(operation) = event.operation.as_ref() {
                    let state_label = if event.event_name == "request.accepted" {
                        "queued"
                    } else {
                        "running"
                    };
                    if state.active.len() >= MAX_ACTIVE_OPERATIONS
                        && !state.active.contains_key(&operation.trace_id)
                    {
                        if let Some(oldest) = state.active.keys().next().cloned() {
                            state.active.remove(&oldest);
                        }
                    }
                    state.active.insert(
                        operation.trace_id.clone(),
                        OperationHealth {
                            operation: super::console::operation_label(&operation.operation)
                                .to_owned(),
                            reference: safe_short_reference(operation.short_reference()),
                            state: state_label.to_owned(),
                            outcome: None,
                            updated_uptime_ms: event.uptime_ms,
                        },
                    );
                }
                state.components.insert(
                    Component::Scheduler,
                    ComponentHealth {
                        component: Component::Scheduler,
                        status: HealthStatus::Busy,
                        fact: "request-active".to_owned(),
                        updated_uptime_ms: event.uptime_ms,
                    },
                );
            }
            "request.completed" => {
                if let Some(operation) = event.operation.as_ref() {
                    state.active.remove(&operation.trace_id);
                    state.last_operation = Some(OperationHealth {
                        operation: super::console::operation_label(&operation.operation).to_owned(),
                        reference: safe_short_reference(operation.short_reference()),
                        state: "completed".to_owned(),
                        outcome: event.fields.outcome,
                        updated_uptime_ms: event.uptime_ms,
                    });
                    if state.active.is_empty() {
                        state.components.insert(
                            Component::Scheduler,
                            ComponentHealth {
                                component: Component::Scheduler,
                                status: HealthStatus::Healthy,
                                fact: "idle".to_owned(),
                                updated_uptime_ms: event.uptime_ms,
                            },
                        );
                    }
                }
            }
            "worker.transition" => {
                if let (Some(worker), Some(worker_state)) =
                    (event.fields.worker.as_ref(), event.fields.state.as_deref())
                {
                    let worker = safe_worker_name(worker).to_owned();
                    if state.workers.len() < MAX_WORKERS || state.workers.contains_key(&worker) {
                        state.workers.insert(
                            worker.clone(),
                            WorkerHealth {
                                worker,
                                status: status_from_worker_state(worker_state),
                                updated_uptime_ms: event.uptime_ms,
                            },
                        );
                    }
                }
            }
            _ => {}
        }

        if event.event_name == "operation.stage" {
            if event.fields.outcome == Some(OperationOutcome::Success) {
                state.components.insert(
                    event.component,
                    ComponentHealth {
                        component: event.component,
                        status: HealthStatus::Healthy,
                        fact: "last-operation-succeeded".to_owned(),
                        updated_uptime_ms: event.uptime_ms,
                    },
                );
            } else if event.fields.outcome == Some(OperationOutcome::Error) {
                state.components.insert(
                    event.component,
                    ComponentHealth {
                        component: event.component,
                        status: HealthStatus::Degraded,
                        fact: "last-operation-failed".to_owned(),
                        updated_uptime_ms: event.uptime_ms,
                    },
                );
            }
        }

        if event.severity == Severity::Error {
            state.components.insert(
                event.component,
                ComponentHealth {
                    component: event.component,
                    status: HealthStatus::Degraded,
                    fact: "recent-failure".to_owned(),
                    updated_uptime_ms: event.uptime_ms,
                },
            );
        }

        let changed_components = state
            .components
            .iter()
            .filter(|(component, current)| {
                previous_components.get(component).is_none_or(|previous| {
                    previous.status != current.status || previous.fact != current.fact
                })
            })
            .map(|(component, current)| (*component, current.clone()))
            .collect::<Vec<_>>();
        for (component, current) in changed_components {
            push_transition(
                state.component_history.entry(component).or_default(),
                HealthTransition {
                    status: current.status,
                    fact: current.fact,
                    uptime_ms: current.updated_uptime_ms,
                },
            );
        }
        let changed_workers = state
            .workers
            .iter()
            .filter(|(worker, current)| {
                previous_workers
                    .get(*worker)
                    .is_none_or(|previous| previous.status != current.status)
            })
            .map(|(worker, current)| (worker.clone(), current.clone()))
            .collect::<Vec<_>>();
        for (worker, current) in changed_workers {
            push_transition(
                state.worker_history.entry(worker).or_default(),
                HealthTransition {
                    status: current.status,
                    fact: "lifecycle-transition".to_owned(),
                    uptime_ms: current.updated_uptime_ms,
                },
            );
        }
    }

    pub(crate) fn update_ui(&self, snapshot: &UiDiagnosticSnapshot) -> bool {
        let mut state = self.state.write();
        let changed = state
            .ui
            .as_ref()
            .is_none_or(|current| !current.semantically_eq(snapshot));
        if changed {
            state.ui = Some(snapshot.clone());
        } else if let Some(current) = state.ui.as_mut() {
            current.revision = current.revision.max(snapshot.revision);
        }
        changed
    }

    pub(crate) fn snapshot(&self, writer: WriterHealth) -> HealthSnapshot {
        let state = self.state.read();
        let mut components = state.components.values().cloned().collect::<Vec<_>>();
        components.retain(|component| component.component != Component::Logging);
        components.push(ComponentHealth {
            component: Component::Logging,
            status: writer_status(writer.state),
            fact: if writer.dropped_events == 0 {
                "accepting-events".to_owned()
            } else {
                "events-dropped".to_owned()
            },
            updated_uptime_ms: 0,
        });
        HealthSnapshot {
            components,
            workers: state.workers.values().cloned().collect(),
            active_operation: state.active.values().next_back().cloned(),
            last_operation: state.last_operation.clone(),
            dropped_events: writer.dropped_events,
            logging_status: writer_status(writer.state),
            ui: state.ui.clone(),
        }
    }

    pub(crate) fn component_history(&self, component: Component) -> Vec<HealthTransition> {
        self.state
            .read()
            .component_history
            .get(&component)
            .map_or_else(Vec::new, |history| history.iter().cloned().collect())
    }

    pub(crate) fn worker_history(&self, worker: &str) -> Vec<HealthTransition> {
        self.state
            .read()
            .worker_history
            .get(worker)
            .map_or_else(Vec::new, |history| history.iter().cloned().collect())
    }
}

fn push_transition(history: &mut VecDeque<HealthTransition>, transition: HealthTransition) {
    if history.back().is_some_and(|previous| {
        previous.status == transition.status && previous.fact == transition.fact
    }) {
        return;
    }
    history.push_back(transition);
    while history.len() > MAX_TRANSITIONS_PER_TARGET {
        history.pop_front();
    }
}

fn status_from_worker_state(state: &str) -> HealthStatus {
    match state {
        "started" => HealthStatus::Healthy,
        "stopped" => HealthStatus::Stopped,
        "failed" | "panicked" => HealthStatus::Degraded,
        _ => HealthStatus::Unknown,
    }
}

fn safe_worker_name(worker: &str) -> &'static str {
    match worker {
        "client-handler" => "client-handler",
        "client-socket" => "client-socket",
        "browser-worker" => "browser-worker",
        "media-control" => "media-control",
        "player-event-watcher" => "player-event-watcher",
        "session-watcher" => "session-watcher",
        "terminal-event-handler" => "terminal-event-handler",
        "ui" => "ui",
        _ => "application-worker",
    }
}

fn safe_short_reference(reference: &str) -> String {
    if reference.len() == 8 && reference.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        reference.to_ascii_lowercase()
    } else {
        "00000000".to_owned()
    }
}

const fn writer_status(state: WriterState) -> HealthStatus {
    match state {
        WriterState::Starting => HealthStatus::Starting,
        WriterState::Healthy => HealthStatus::Healthy,
        WriterState::Degraded => HealthStatus::Degraded,
        WriterState::Disabled => HealthStatus::Disabled,
        WriterState::Stopped => HealthStatus::Stopped,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::observability::{
        EventCode, EventName, OperationContext, OperationSource, PrivacyClass,
    };
    use std::time::Instant;

    fn event(name: EventName, code: EventCode, message: &'static str) -> DiagnosticEvent {
        DiagnosticEvent::new(
            "run",
            Instant::now(),
            name,
            code,
            Severity::Debug,
            Component::Scheduler,
            message,
        )
        .with_operation(OperationContext::new("search", OperationSource::Terminal))
    }

    #[test]
    fn request_health_distinguishes_running_superseded_and_completed() {
        let registry = HealthRegistry::default();
        let accepted = event(
            EventName::REQUEST_ACCEPTED,
            EventCode::REQUEST_ACCEPTED,
            "Request accepted",
        );
        registry.observe(&accepted);
        let snapshot = registry.snapshot(WriterHealth {
            state: WriterState::Healthy,
            dropped_events: 0,
            files_created: 1,
            bytes_written: 1,
        });
        assert_eq!(snapshot.active_operation.unwrap().state, "queued");

        let mut completed = accepted.clone();
        completed.event_name = "request.completed".to_owned();
        completed.event_code = "REQUEST_COMPLETED".to_owned();
        completed.fields.outcome = Some(OperationOutcome::Superseded);
        registry.observe(&completed);
        let snapshot = registry.snapshot(WriterHealth {
            state: WriterState::Degraded,
            dropped_events: 2,
            files_created: 1,
            bytes_written: 1,
        });
        assert!(snapshot.active_operation.is_none());
        assert_eq!(
            snapshot.last_operation.unwrap().outcome,
            Some(OperationOutcome::Superseded)
        );
        assert_eq!(snapshot.logging_status, HealthStatus::Degraded);
        assert_eq!(snapshot.dropped_events, 2);
        assert!(snapshot.ui.is_none());
        assert_eq!(completed.privacy_class, PrivacyClass::SafeOperational);
    }

    #[test]
    fn snapshots_cover_worker_ui_degradation_and_recovery_without_activity_data() {
        let registry = HealthRegistry::default();
        let mut worker = event(
            EventName::WORKER_TRANSITION,
            EventCode::WORKER_TRANSITION,
            "Worker lifecycle changed",
        );
        worker.operation = None;
        worker.fields.worker = Some("client-handler".to_owned());
        worker.fields.state = Some("started".to_owned());
        registry.observe(&worker);

        let mut failed = event(
            EventName::OPERATION_STAGE,
            EventCode::OPERATION_STAGE,
            "Operation stage",
        );
        failed.component = Component::Browser;
        failed.fields.outcome = Some(OperationOutcome::Error);
        registry.observe(&failed);
        assert!(registry.update_ui(&UiDiagnosticSnapshot {
            revision: 7,
            page: "search".to_owned(),
            popup: "none".to_owned(),
            selection: Some(2),
            loading: false,
            meaningful_state: "ready".to_owned(),
            provider: "youtube_music".to_owned(),
            lifecycle: "running".to_owned(),
        }));

        let mut recovered = failed;
        recovered.fields.outcome = Some(OperationOutcome::Success);
        registry.observe(&recovered);
        let snapshot = registry.snapshot(WriterHealth {
            state: WriterState::Healthy,
            dropped_events: 0,
            files_created: 1,
            bytes_written: 1,
        });
        assert_eq!(snapshot.workers[0].status, HealthStatus::Healthy);
        assert!(snapshot.components.iter().any(|component| {
            component.component == Component::Browser && component.status == HealthStatus::Healthy
        }));
        let serialized = serde_json::to_string(&snapshot).unwrap();
        assert!(!serialized.contains("private-query"));
        assert!(!serialized.contains("https://"));
        assert!(!serialized.contains("cookie"));
    }

    #[test]
    fn revision_only_ui_refreshes_update_revision_without_transition_flooding() {
        let registry = HealthRegistry::default();
        let mut transitions = 0;
        for revision in 0..10_000 {
            transitions += usize::from(registry.update_ui(&UiDiagnosticSnapshot {
                revision,
                page: "diagnostics".to_owned(),
                popup: "none".to_owned(),
                selection: Some(4),
                loading: false,
                meaningful_state: "ready".to_owned(),
                provider: "youtube_music".to_owned(),
                lifecycle: "running".to_owned(),
            }));
        }

        assert_eq!(transitions, 1);
        let snapshot = registry.snapshot(WriterHealth {
            state: WriterState::Healthy,
            dropped_events: 0,
            files_created: 1,
            bytes_written: 1,
        });
        assert_eq!(snapshot.ui.unwrap().revision, 9_999);
    }

    #[test]
    fn meaningful_ui_state_changes_still_record_transitions() {
        let registry = HealthRegistry::default();
        let base = UiDiagnosticSnapshot {
            revision: 1,
            page: "youtube_context".to_owned(),
            popup: "none".to_owned(),
            selection: None,
            loading: true,
            meaningful_state: "loading".to_owned(),
            provider: "youtube_music".to_owned(),
            lifecycle: "running".to_owned(),
        };
        assert!(registry.update_ui(&base));
        assert!(registry.update_ui(&UiDiagnosticSnapshot {
            revision: 2,
            selection: Some(0),
            loading: false,
            meaningful_state: "ready".to_owned(),
            ..base
        }));
    }

    #[test]
    fn transition_histories_are_typed_deduplicated_and_bounded() {
        let registry = HealthRegistry::default();
        for index in 0..40 {
            registry.set_component(
                Component::Runtime,
                if index % 2 == 0 {
                    HealthStatus::Healthy
                } else {
                    HealthStatus::Degraded
                },
                if index % 2 == 0 {
                    "supervised"
                } else {
                    "recent-failure"
                },
                index,
            );
        }
        let history = registry.component_history(Component::Runtime);
        assert_eq!(history.len(), MAX_TRANSITIONS_PER_TARGET);
        assert_eq!(history.last().unwrap().uptime_ms, 39);
        registry.set_component(
            Component::Runtime,
            HealthStatus::Degraded,
            "recent-failure",
            40,
        );
        assert_eq!(
            registry.component_history(Component::Runtime).len(),
            MAX_TRANSITIONS_PER_TARGET
        );
    }
}
