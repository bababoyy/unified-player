use std::{
    collections::{BTreeMap, VecDeque},
    fmt::Write as _,
    sync::Arc,
};

use parking_lot::RwLock;

use super::{
    Component, DiagnosticEvent, DynamicFilterSnapshot, HealthSnapshot, HealthStatus,
    IncidentSummary, OperationOutcome, Severity,
};

const MAX_OPERATIONS: usize = 64;
const MAX_TIMELINE_ENTRIES: usize = 32;
const MAX_PERFORMANCE_ENTRIES: usize = 32;

#[derive(Clone, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub(crate) enum DiagnosticRowId {
    ComponentLoading,
    WorkersEmpty,
    OperationsEmpty,
    IncidentsEmpty,
    Component(Component),
    Worker(String),
    Operation(String),
    Logging,
    PlaybackRoute,
    Filter,
    Support,
    Performance,
    #[cfg(feature = "private-capture")]
    PrivateCapture,
    Incident(String),
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct DiagnosticRow {
    pub(crate) id: DiagnosticRowId,
    pub(crate) label: String,
    pub(crate) summary: String,
    pub(crate) severity: Severity,
    pub(crate) acknowledged: bool,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum DiagnosticAction {
    ExplainState,
    IncidentSummary,
    IncidentTimeline,
    CopyIncidentSummary,
    IncidentRunbook,
    AcknowledgeIncident,
    FocusSupportBundle,
    HealthSummary,
    HealthHistory,
    RelatedIncidents,
    CopyHealthSummary,
    HealthRunbook,
    FollowOperation,
    OperationTimeline,
    CopyOperationReference,
    EnableTrace15,
    EnableTrace30,
    EnableTrace60,
    StopTrace,
    ExplainWriterHealth,
    ExplainDroppedEvents,
    PreviewBundle,
    CreateGeneralBundle,
    CreateFocusedBundle,
    ReviewBundle,
    VerifyChecksums,
    ScanForbiddenData,
    OpenBundleFolder,
    CopyBundleReview,
    ShowExceededBudgets,
    ShowTimingTrends,
    CopyTrendSummary,
    #[cfg(feature = "private-capture")]
    ExplainCaptureSensitivity,
    #[cfg(feature = "private-capture")]
    ArmPrivateCapture,
    #[cfg(feature = "private-capture")]
    CancelPrivateCaptureConsent,
    #[cfg(feature = "private-capture")]
    DisarmPrivateCapture,
    #[cfg(feature = "private-capture")]
    ViewPrivateCaptureStatus,
    #[cfg(feature = "private-capture")]
    ReviewPrivateCapture,
    #[cfg(feature = "private-capture")]
    SelectPrivateCapture,
    #[cfg(feature = "private-capture")]
    MarkCaptureWorking,
    #[cfg(feature = "private-capture")]
    MarkCaptureFailing,
    #[cfg(feature = "private-capture")]
    ComparePrivateCaptures,
    #[cfg(feature = "private-capture")]
    ReplayPrivateCaptureOffline,
    #[cfg(feature = "private-capture")]
    ReplayPrivateCaptureFresh,
    #[cfg(feature = "private-capture")]
    PreviewPrivateDerivative,
    #[cfg(feature = "private-capture")]
    ViewPrivateDerivativePreview,
    #[cfg(feature = "private-capture")]
    CopyPrivateDerivativeReview,
    #[cfg(feature = "private-capture")]
    CreatePrivateDerivative,
    #[cfg(feature = "private-capture")]
    ReviewPrivateDerivative,
    #[cfg(feature = "private-capture")]
    OpenPrivateCaptureFolder,
    #[cfg(feature = "private-capture")]
    DeletePrivateCapture,
    #[cfg(feature = "private-capture")]
    PurgeExpiredPrivateCaptures,
}

impl DiagnosticAction {
    pub(crate) const fn label(self) -> &'static str {
        match self {
            Self::ExplainState => "Explain this diagnostic state",
            Self::IncidentSummary => "View safe incident summary",
            Self::IncidentTimeline => "View operation timeline",
            Self::CopyIncidentSummary => "Copy safe incident summary",
            Self::IncidentRunbook => "Show recommended action and runbook",
            Self::AcknowledgeIncident => "Acknowledge for this run",
            Self::FocusSupportBundle => "Focus next support bundle",
            Self::HealthSummary => "View current health",
            Self::HealthHistory => "View transition history",
            Self::RelatedIncidents => "Show related incidents",
            Self::CopyHealthSummary => "Copy safe health summary",
            Self::HealthRunbook => "Show component runbook",
            Self::FollowOperation => "Follow until completion",
            Self::OperationTimeline => "View queue, stage, retry and outcome timings",
            Self::CopyOperationReference => "Copy short operation reference",
            Self::EnableTrace15 => "Enable verbose tracing for 15 seconds",
            Self::EnableTrace30 => "Enable verbose tracing for 30 seconds",
            Self::EnableTrace60 => "Enable verbose tracing for 60 seconds",
            Self::StopTrace => "Stop temporary verbose tracing",
            Self::ExplainWriterHealth => "Explain diagnostic writer health",
            Self::ExplainDroppedEvents => "Explain dropped-event state",
            Self::PreviewBundle => "Preview support bundle manifest",
            Self::CreateGeneralBundle => "Create general support bundle",
            Self::CreateFocusedBundle => "Create focused support bundle",
            Self::ReviewBundle => "Review latest bundle manifest",
            Self::VerifyChecksums => "Verify latest bundle checksums",
            Self::ScanForbiddenData => "Run forbidden-data scan",
            Self::OpenBundleFolder => "Open latest bundle folder",
            Self::CopyBundleReview => "Copy safe bundle review",
            Self::ShowExceededBudgets => "Show exceeded local budgets",
            Self::ShowTimingTrends => "Show retained local timing trends",
            Self::CopyTrendSummary => "Copy bounded trend summary",
            #[cfg(feature = "private-capture")]
            Self::ExplainCaptureSensitivity => "Explain private-capture sensitivity",
            #[cfg(feature = "private-capture")]
            Self::ArmPrivateCapture => "Arm the next manual YouTube attempt",
            #[cfg(feature = "private-capture")]
            Self::CancelPrivateCaptureConsent => "Cancel pending capture consent",
            #[cfg(feature = "private-capture")]
            Self::DisarmPrivateCapture => "Disarm private capture",
            #[cfg(feature = "private-capture")]
            Self::ViewPrivateCaptureStatus => "View safe status and completeness",
            #[cfg(feature = "private-capture")]
            Self::ReviewPrivateCapture => "Review selected capture completeness",
            #[cfg(feature = "private-capture")]
            Self::SelectPrivateCapture => "Select a retained capture",
            #[cfg(feature = "private-capture")]
            Self::MarkCaptureWorking => "Mark selected capture as working",
            #[cfg(feature = "private-capture")]
            Self::MarkCaptureFailing => "Mark selected capture as failing",
            #[cfg(feature = "private-capture")]
            Self::ComparePrivateCaptures => "Compare working and failing captures",
            #[cfg(feature = "private-capture")]
            Self::ReplayPrivateCaptureOffline => "Replay selected capture offline",
            #[cfg(feature = "private-capture")]
            Self::ReplayPrivateCaptureFresh => "Replay selected capture with current transport",
            #[cfg(feature = "private-capture")]
            Self::PreviewPrivateDerivative => "Prepare exact safe derivative preview",
            #[cfg(feature = "private-capture")]
            Self::ViewPrivateDerivativePreview => "View exact derivative files",
            #[cfg(feature = "private-capture")]
            Self::CopyPrivateDerivativeReview => "Copy safe derivative review",
            #[cfg(feature = "private-capture")]
            Self::CreatePrivateDerivative => "Create reviewed safe derivative",
            #[cfg(feature = "private-capture")]
            Self::ReviewPrivateDerivative => "Review latest derivative",
            #[cfg(feature = "private-capture")]
            Self::OpenPrivateCaptureFolder => "Open encrypted capture folder",
            #[cfg(feature = "private-capture")]
            Self::DeletePrivateCapture => "Delete selected encrypted capture",
            #[cfg(feature = "private-capture")]
            Self::PurgeExpiredPrivateCaptures => "Purge expired captures",
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct TimelineEntry {
    pub(crate) uptime_ms: u64,
    pub(crate) stage: &'static str,
    pub(crate) outcome: Option<OperationOutcome>,
    pub(crate) duration_ms: Option<u64>,
    pub(crate) queue_wait_ms: Option<u64>,
    pub(crate) attempt: u16,
}

impl TimelineEntry {
    pub(crate) fn render(&self) -> String {
        let mut line = format!(
            "+{}ms {} attempt={}",
            self.uptime_ms, self.stage, self.attempt
        );
        if let Some(wait) = self.queue_wait_ms {
            let _ = write!(line, " queue={wait}ms");
        }
        if let Some(duration) = self.duration_ms {
            let _ = write!(line, " duration={duration}ms");
        }
        if let Some(outcome) = self.outcome {
            let _ = write!(line, " outcome={}", outcome_label(outcome));
        }
        line
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct OperationTimeline {
    pub(crate) reference: String,
    pub(crate) operation: String,
    pub(crate) entries: Vec<TimelineEntry>,
    pub(crate) outcome: Option<OperationOutcome>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct PerformanceEntry {
    pub(crate) component: Component,
    pub(crate) duration_ms: u64,
    pub(crate) budget_ms: u64,
    pub(crate) uptime_ms: u64,
}

#[derive(Default)]
struct ConsoleState {
    timelines: BTreeMap<String, OperationTimeline>,
    order: VecDeque<String>,
    performance: VecDeque<PerformanceEntry>,
}

#[derive(Clone, Default)]
pub(crate) struct ConsoleRegistry {
    state: Arc<RwLock<ConsoleState>>,
}

impl ConsoleRegistry {
    pub(crate) fn observe(&self, event: &DiagnosticEvent) {
        let mut state = self.state.write();
        if event.event_name == "performance.budget" {
            if let (Some(duration_ms), Some(budget_ms)) =
                (event.fields.duration_ms, event.fields.budget_ms)
            {
                state.performance.push_back(PerformanceEntry {
                    component: event.component,
                    duration_ms,
                    budget_ms,
                    uptime_ms: event.uptime_ms,
                });
                while state.performance.len() > MAX_PERFORMANCE_ENTRIES {
                    state.performance.pop_front();
                }
            }
        }

        let Some(operation) = event.operation.as_ref() else {
            return;
        };
        let Some(stage) = timeline_stage(event) else {
            return;
        };
        let reference = safe_reference(operation.short_reference());
        if reference.is_empty() {
            return;
        }
        if !state.timelines.contains_key(&reference) {
            state.order.push_back(reference.clone());
            while state.order.len() > MAX_OPERATIONS {
                if let Some(oldest) = state.order.pop_front() {
                    state.timelines.remove(&oldest);
                }
            }
        }
        let timeline =
            state
                .timelines
                .entry(reference.clone())
                .or_insert_with(|| OperationTimeline {
                    reference,
                    operation: operation_label(&operation.operation).to_owned(),
                    entries: Vec::new(),
                    outcome: None,
                });
        if stage == "terminal" && timeline.outcome.is_some() {
            return;
        }
        let outcome = if stage == "terminal" {
            event.fields.outcome
        } else {
            None
        };
        timeline.entries.push(TimelineEntry {
            uptime_ms: event.uptime_ms,
            stage,
            outcome,
            duration_ms: event.fields.duration_ms,
            queue_wait_ms: event.fields.queue_wait_ms,
            attempt: event.fields.attempt.unwrap_or(operation.attempt),
        });
        if timeline.entries.len() > MAX_TIMELINE_ENTRIES {
            timeline.entries.remove(0);
        }
        if outcome.is_some() {
            timeline.outcome = outcome;
        }
    }

    pub(crate) fn timeline(&self, reference: &str) -> Option<OperationTimeline> {
        self.state.read().timelines.get(reference).cloned()
    }

    pub(crate) fn performance(&self) -> Vec<PerformanceEntry> {
        self.state.read().performance.iter().cloned().collect()
    }

    pub(crate) fn recent_operations(&self, limit: usize) -> Vec<OperationTimeline> {
        let state = self.state.read();
        state
            .order
            .iter()
            .rev()
            .filter_map(|reference| state.timelines.get(reference).cloned())
            .take(limit)
            .collect()
    }
}

pub(crate) fn rows(
    health: &HealthSnapshot,
    filter: DynamicFilterSnapshot,
    incidents: &[(IncidentSummary, bool)],
    operations: &[OperationTimeline],
) -> Vec<DiagnosticRow> {
    let mut rows = Vec::new();
    if health
        .components
        .iter()
        .all(|item| item.component == Component::Logging)
    {
        rows.push(DiagnosticRow {
            id: DiagnosticRowId::ComponentLoading,
            label: "Components / loading".to_owned(),
            summary: "awaiting first runtime health update".to_owned(),
            severity: Severity::Info,
            acknowledged: false,
        });
    }
    rows.extend(health.components.iter().map(|item| DiagnosticRow {
        id: DiagnosticRowId::Component(item.component),
        label: format!("Component / {}", component_label(item.component)),
        summary: format!("{} / {}", item.status.label(), allowlisted_fact(&item.fact)),
        severity: severity_for_health(item.status),
        acknowledged: false,
    }));
    rows.extend(health.workers.iter().map(|item| DiagnosticRow {
        id: DiagnosticRowId::Worker(item.worker.clone()),
        label: format!("Worker / {}", safe_worker_label(&item.worker)),
        summary: item.status.label().to_owned(),
        severity: severity_for_health(item.status),
        acknowledged: false,
    }));
    if health.workers.is_empty() {
        rows.push(DiagnosticRow {
            id: DiagnosticRowId::WorkersEmpty,
            label: "Workers / empty".to_owned(),
            summary: "no worker transitions retained".to_owned(),
            severity: Severity::Info,
            acknowledged: false,
        });
    }
    rows.push(DiagnosticRow {
        id: DiagnosticRowId::Logging,
        label: "Logging / writer".to_owned(),
        summary: format!(
            "{} / dropped={}",
            health.logging_status.label(),
            health.dropped_events
        ),
        severity: severity_for_health(health.logging_status),
        acknowledged: false,
    });
    if let Some(operation) = health.active_operation.as_ref() {
        rows.push(operation_row(operation, true));
    }
    if let Some(operation) = health.last_operation.as_ref() {
        let row = operation_row(operation, false);
        if !rows.iter().any(|existing| existing.id == row.id) {
            rows.push(row);
        }
    }
    for operation in operations {
        let row = timeline_row(operation);
        if !rows.iter().any(|existing| existing.id == row.id) {
            rows.push(row);
        }
    }
    if !rows
        .iter()
        .any(|row| matches!(row.id, DiagnosticRowId::Operation(_)))
    {
        rows.push(DiagnosticRow {
            id: DiagnosticRowId::OperationsEmpty,
            label: "Operations / idle".to_owned(),
            summary: "no operation evidence retained".to_owned(),
            severity: Severity::Info,
            acknowledged: false,
        });
    }
    if incidents.is_empty() {
        rows.push(DiagnosticRow {
            id: DiagnosticRowId::IncidentsEmpty,
            label: "Incidents / empty".to_owned(),
            summary: "no significant incidents in this run".to_owned(),
            severity: Severity::Info,
            acknowledged: false,
        });
    }
    rows.extend(
        incidents
            .iter()
            .rev()
            .map(|(incident, acknowledged)| DiagnosticRow {
                id: DiagnosticRowId::Incident(incident.safe_reference().to_owned()),
                label: format!(
                    "Incident / {} / {}",
                    incident.safe_reference(),
                    incident.safe_event_code()
                ),
                summary: if *acknowledged {
                    format!(
                        "acknowledged / {}{}",
                        incident.safe_operation(),
                        incident
                            .safe_provider()
                            .map_or_else(String::new, |provider| format!(" ({provider})"))
                    )
                } else {
                    format!(
                        "{}{} / {}",
                        incident.safe_operation(),
                        incident
                            .safe_provider()
                            .map_or_else(String::new, |provider| format!(" ({provider})")),
                        incident.safe_cause()
                    )
                },
                severity: Severity::Warn,
                acknowledged: *acknowledged,
            }),
    );
    rows.push(DiagnosticRow {
        id: DiagnosticRowId::Filter,
        label: "Logging / verbose tracing".to_owned(),
        summary: if !filter.available {
            "unavailable; core on".to_owned()
        } else if filter.temporary {
            format!("trace / {}s; core on", filter.remaining_seconds)
        } else {
            format!(
                "{} / persistent; core on",
                filter.level.label().to_ascii_lowercase()
            )
        },
        severity: Severity::Info,
        acknowledged: false,
    });
    rows.push(DiagnosticRow {
        id: DiagnosticRowId::Support,
        label: "Support / reviewable bundle".to_owned(),
        summary: "allowlisted local evidence".to_owned(),
        severity: Severity::Info,
        acknowledged: false,
    });
    rows.push(DiagnosticRow {
        id: DiagnosticRowId::Performance,
        label: "Performance / local budgets and trends".to_owned(),
        summary: "bounded retained evidence".to_owned(),
        severity: Severity::Info,
        acknowledged: false,
    });
    rows
}

/// Insert the current `YouTube` route into the diagnostics workspace without
/// retaining media URLs, credentials, or other provider-private material.
pub(crate) fn append_youtube_playback_route_row(
    rows: &mut Vec<DiagnosticRow>,
    route: Option<&crate::state::YouTubePlaybackRoute>,
) {
    let summary = route.map_or_else(
        || "idle / no resolved YouTube playback route".to_owned(),
        |route| {
            if route.order.is_empty() {
                return "idle / no resolved YouTube playback route".to_owned();
            }
            let learned = if route.learned_public_android_vr {
                " (learned)"
            } else {
                ""
            };
            let selected = route.selected.as_deref().unwrap_or("pending");
            let javascript_backend = route
                .javascript_backend
                .as_deref()
                .map_or_else(String::new, |backend| format!(" | js: {backend}"));
            format!(
                "route{learned}: {} | selected: {selected}{javascript_backend}",
                route.order.join(" -> ")
            )
        },
    );
    let row = DiagnosticRow {
        id: DiagnosticRowId::PlaybackRoute,
        label: "Playback / YouTube route".to_owned(),
        summary,
        severity: Severity::Info,
        acknowledged: false,
    };
    let insert_at = rows
        .iter()
        .position(|row| matches!(row.id, DiagnosticRowId::Filter))
        .unwrap_or(rows.len());
    rows.insert(insert_at, row);
}

pub(crate) fn actions_for(
    row: &DiagnosticRowId,
    filter_active: bool,
    has_bundle: bool,
    focused: bool,
) -> Vec<DiagnosticAction> {
    use DiagnosticAction as A;
    match row {
        DiagnosticRowId::ComponentLoading
        | DiagnosticRowId::WorkersEmpty
        | DiagnosticRowId::OperationsEmpty
        | DiagnosticRowId::IncidentsEmpty
        | DiagnosticRowId::PlaybackRoute => vec![A::ExplainState],
        DiagnosticRowId::Incident(_) => vec![
            A::IncidentSummary,
            A::IncidentTimeline,
            A::CopyIncidentSummary,
            A::IncidentRunbook,
            A::AcknowledgeIncident,
            A::FocusSupportBundle,
        ],
        DiagnosticRowId::Component(_) | DiagnosticRowId::Worker(_) => vec![
            A::HealthSummary,
            A::HealthHistory,
            A::RelatedIncidents,
            A::CopyHealthSummary,
            A::HealthRunbook,
        ],
        DiagnosticRowId::Operation(_) => vec![
            A::FollowOperation,
            A::OperationTimeline,
            A::CopyOperationReference,
        ],
        DiagnosticRowId::Logging => vec![A::ExplainWriterHealth, A::ExplainDroppedEvents],
        DiagnosticRowId::Filter => {
            let mut actions = vec![A::EnableTrace15, A::EnableTrace30, A::EnableTrace60];
            if filter_active {
                actions.push(A::StopTrace);
            }
            actions
        }
        DiagnosticRowId::Support => {
            let mut actions = vec![A::PreviewBundle, A::CreateGeneralBundle];
            if focused {
                actions.push(A::CreateFocusedBundle);
            }
            if has_bundle {
                actions.extend([
                    A::ReviewBundle,
                    A::VerifyChecksums,
                    A::ScanForbiddenData,
                    A::OpenBundleFolder,
                    A::CopyBundleReview,
                ]);
            }
            actions
        }
        DiagnosticRowId::Performance => vec![
            A::ShowExceededBudgets,
            A::ShowTimingTrends,
            A::CopyTrendSummary,
        ],
        #[cfg(feature = "private-capture")]
        DiagnosticRowId::PrivateCapture => {
            vec![A::ExplainCaptureSensitivity, A::ViewPrivateCaptureStatus]
        }
    }
}

#[cfg(feature = "private-capture")]
pub(crate) fn private_capture_row(
    snapshot: &crate::developer_capture::SafeOperatorSnapshot,
) -> DiagnosticRow {
    use crate::developer_capture::{SafeCaptureState as C, SafeOperatorPhase as P};

    let (state_label, summary) = match snapshot.phase {
        P::Loading => (
            "loading",
            "safe local operator inventory is loading".to_owned(),
        ),
        P::Stopped => ("inactive", "private-capture operator is stopped".to_owned()),
        P::Failed(failure) => (
            "failed",
            format!("{} / playback unaffected", failure.as_str()),
        ),
        P::Ready | P::Working(_) => match snapshot.capture.state {
            C::Inactive if snapshot.artifacts.is_empty() => (
                "empty",
                "no retained encrypted captures; playback unaffected".to_owned(),
            ),
            C::Inactive => (
                "inactive",
                format!("{} retained encrypted capture(s)", snapshot.artifacts.len()),
            ),
            C::ConsentRequired => (
                "consent required",
                "enter a private passphrase or cancel".to_owned(),
            ),
            C::Armed => (
                "armed",
                format!(
                    "next manual YouTube attempt / {}s remaining",
                    snapshot.capture.remaining_seconds.unwrap_or_default()
                ),
            ),
            C::Claimed => (
                "claimed",
                "one manual YouTube attempt has claimed consent".to_owned(),
            ),
            C::Capturing => (
                "capturing",
                format!("{} bounded record(s)", snapshot.capture.record_count),
            ),
            C::Finalizing => (
                "finalizing",
                format!("{} bounded record(s)", snapshot.capture.record_count),
            ),
            C::Ready => (
                "ready",
                format!("{} record(s) / complete", snapshot.capture.record_count),
            ),
            C::Incomplete => (
                "incomplete",
                format!(
                    "{} record(s) / dropped={}",
                    snapshot.capture.record_count, snapshot.capture.dropped_records
                ),
            ),
            C::Expired => (
                "expired",
                "capture consent expired without affecting playback".to_owned(),
            ),
            C::Failed => (
                "failed",
                "private capture failed; playback was not controlled".to_owned(),
            ),
        },
    };
    let summary = match snapshot.phase {
        P::Working(action) => format!("{} / {summary}", action.as_str()),
        _ => summary,
    };
    let severity = if matches!(snapshot.phase, P::Failed(_))
        || matches!(snapshot.capture.state, C::Failed | C::Incomplete)
    {
        Severity::Warn
    } else {
        Severity::Info
    };
    DiagnosticRow {
        id: DiagnosticRowId::PrivateCapture,
        label: format!("Private capture / {state_label}"),
        summary,
        severity,
        acknowledged: false,
    }
}

#[cfg(feature = "private-capture")]
pub(crate) fn private_capture_actions(
    snapshot: &crate::developer_capture::SafeOperatorSnapshot,
) -> Vec<DiagnosticAction> {
    use crate::developer_capture::{SafeCaptureState as C, SafeOperatorPhase as P};
    use DiagnosticAction as A;

    let mut actions = vec![A::ExplainCaptureSensitivity, A::ViewPrivateCaptureStatus];
    if matches!(snapshot.phase, P::Loading | P::Stopped | P::Working(_)) {
        return actions;
    }

    match snapshot.capture.state {
        C::ConsentRequired => actions.push(A::CancelPrivateCaptureConsent),
        C::Armed => actions.push(A::DisarmPrivateCapture),
        C::Claimed | C::Capturing | C::Finalizing => {}
        C::Inactive | C::Ready | C::Incomplete | C::Expired | C::Failed => {
            actions.push(A::ArmPrivateCapture);
        }
    }

    if !snapshot.artifacts.is_empty() {
        actions.push(A::SelectPrivateCapture);
        actions.push(A::OpenPrivateCaptureFolder);
        actions.push(A::PurgeExpiredPrivateCaptures);
    }
    if snapshot.selected.is_some() {
        actions.extend([
            A::MarkCaptureWorking,
            A::MarkCaptureFailing,
            A::ReviewPrivateCapture,
            A::ReplayPrivateCaptureOffline,
            A::ReplayPrivateCaptureFresh,
            A::PreviewPrivateDerivative,
            A::CreatePrivateDerivative,
            A::DeletePrivateCapture,
        ]);
    }
    if snapshot.working.is_some() && snapshot.failing.is_some() {
        actions.push(A::ComparePrivateCaptures);
    }
    if let Some(derivative) = snapshot.derivative.as_ref() {
        if derivative.preview().is_some() {
            actions.push(A::ViewPrivateDerivativePreview);
        }
        actions.push(A::CopyPrivateDerivativeReview);
        if matches!(
            derivative.state,
            crate::developer_capture::SafeDerivativeState::Created
                | crate::developer_capture::SafeDerivativeState::Reviewed
        ) {
            actions.push(A::ReviewPrivateDerivative);
        }
    }
    actions
}

pub(crate) fn safe_incident_text(incident: &IncidentSummary) -> String {
    incident.render_lines().join("\n")
}

pub(crate) fn safe_health_text(
    label: &str,
    status: HealthStatus,
    fact: &str,
    age_ms: u64,
) -> String {
    format!(
        "diagnostic health summary\ncomponent={}\nstatus={}\nfact={}\ntransition_age_ms={}",
        allowlisted_label(label),
        status.label(),
        allowlisted_fact(fact),
        age_ms
    )
}

pub(crate) fn safe_transition_line(
    transition: &super::HealthTransition,
    now_uptime_ms: u64,
) -> String {
    format!(
        "{} / {} / {}ms ago",
        transition.status.label(),
        allowlisted_fact(&transition.fact),
        now_uptime_ms.saturating_sub(transition.uptime_ms)
    )
}

pub(crate) fn safe_reference_text(reference: &str) -> Option<String> {
    let reference = reference.strip_prefix("I-").unwrap_or(reference);
    (reference.len() == 8 && reference.bytes().all(|byte| byte.is_ascii_hexdigit()))
        .then(|| reference.to_ascii_lowercase())
}

pub(crate) fn safe_bundle_review_text(review: &super::BundleReview) -> String {
    format!(
        "support bundle review\nmanifest_version={}\nschema_version={}\nbuild_revision={}\nbuild_dirty={}\nbacktraces=excluded\nremote_export=disabled\nfiles={}\nevents={}\nmanifest_checksums=verified\nchecksums=verified\nforbidden_findings={}\nreview_required=yes",
        review.manifest_version,
        review.schema_version,
        review.build_revision,
        review.build_dirty,
        review.files.join(","),
        review.event_count,
        review.forbidden_findings
    )
}

pub(crate) fn bounded_trend_text(text: &str) -> String {
    text.lines()
        .filter(|line| {
            line.starts_with("diagnostic local trend")
                || line.starts_with("trend.scope=")
                || line.starts_with("trend.samples=")
                || line.starts_with("trend.")
        })
        .take(12)
        .map(|line| line.chars().take(240).collect::<String>())
        .collect::<Vec<_>>()
        .join("\n")
}

pub(crate) fn safe_detail_text(value: &str) -> String {
    let normalized = value
        .chars()
        .filter(|character| !character.is_control())
        .take(240)
        .collect::<String>();
    let lower = normalized.to_ascii_lowercase();
    let contains_private_shape = lower.contains(":\\")
        || lower.contains(":/")
        || lower.contains("\\\\")
        || lower.contains("://")
        || lower.contains("www.")
        || lower.contains("youtube.com")
        || lower.contains("spotify:")
        || lower.contains("authorization")
        || lower.contains("cookie=")
        || lower.contains("token=")
        || lower.contains("query=")
        || lower.contains("title=")
        || lower.contains("artist=")
        || lower.contains("lyrics=");
    if contains_private_shape {
        "Private diagnostic detail omitted by policy".to_owned()
    } else {
        normalized
    }
}

fn allowlisted_label(value: &str) -> &'static str {
    match value {
        "application" => "application",
        "runtime" => "runtime",
        "input" => "input",
        "socket" => "socket",
        "scheduler" => "scheduler",
        "coordinator" => "coordinator",
        "spotify" => "spotify",
        "youtube music" => "youtube music",
        "browser" => "browser",
        "audio" => "audio",
        "media control" => "media control",
        "persistence" => "persistence",
        "state" => "state",
        "ui" => "ui",
        "logging" => "logging",
        "support" => "support",
        "client handler" => "client handler",
        "client socket" => "client socket",
        "player event watcher" => "player event watcher",
        "session watcher" => "session watcher",
        "terminal event handler" => "terminal event handler",
        _ => "application component",
    }
}

fn allowlisted_fact(value: &str) -> &'static str {
    match value {
        "request-active" => "request-active",
        "idle" => "idle",
        "last-operation-succeeded" => "last-operation-succeeded",
        "last-operation-failed" => "last-operation-failed",
        "recent-failure" => "recent-failure",
        "accepting-events" => "accepting-events",
        "events-dropped" => "events-dropped",
        "supervised" => "supervised",
        "session-closed" => "session-closed",
        "output-ready" => "output-ready",
        "lifecycle-transition" => "lifecycle-transition",
        "bundle-reviewed" => "bundle-reviewed",
        _ => "bounded-health-fact",
    }
}

fn operation_row(operation: &super::OperationHealth, active: bool) -> DiagnosticRow {
    let outcome = operation.outcome.map_or_else(
        || operation.state.clone(),
        |value| outcome_label(value).to_owned(),
    );
    DiagnosticRow {
        id: DiagnosticRowId::Operation(operation.reference.clone()),
        label: format!(
            "Operation / {} / {}",
            operation.reference, operation.operation
        ),
        summary: if active {
            format!("running / {outcome}")
        } else {
            outcome
        },
        severity: match operation.outcome {
            Some(
                OperationOutcome::Error
                | OperationOutcome::Timeout
                | OperationOutcome::Panicked
                | OperationOutcome::Aborted
                | OperationOutcome::Rejected,
            ) => Severity::Warn,
            _ => Severity::Info,
        },
        acknowledged: false,
    }
}

fn timeline_row(operation: &OperationTimeline) -> DiagnosticRow {
    let last_stage = operation
        .entries
        .last()
        .map_or("awaiting evidence", |entry| entry.stage);
    let summary = operation.outcome.map_or_else(
        || {
            if last_stage == "queued" {
                "queued".to_owned()
            } else {
                format!("running / {last_stage}")
            }
        },
        |outcome| outcome_label(outcome).to_owned(),
    );
    DiagnosticRow {
        id: DiagnosticRowId::Operation(operation.reference.clone()),
        label: format!(
            "Operation / {} / {}",
            operation.reference, operation.operation
        ),
        summary,
        severity: match operation.outcome {
            Some(
                OperationOutcome::Error
                | OperationOutcome::Timeout
                | OperationOutcome::Panicked
                | OperationOutcome::Aborted
                | OperationOutcome::Rejected,
            ) => Severity::Warn,
            _ => Severity::Info,
        },
        acknowledged: false,
    }
}

fn timeline_stage(event: &DiagnosticEvent) -> Option<&'static str> {
    match event.event_name.as_str() {
        "request.accepted" => Some("queued"),
        "request.started" => Some("started"),
        "operation.stage" => Some(match event.fields.outcome {
            Some(OperationOutcome::Error) => "stage-failed",
            Some(OperationOutcome::Success) => "stage-completed",
            _ => "stage",
        }),
        "request.completed" => Some("terminal"),
        _ => None,
    }
}

pub(crate) const fn outcome_label(outcome: OperationOutcome) -> &'static str {
    match outcome {
        OperationOutcome::Success => "completed",
        OperationOutcome::Error => "failed",
        OperationOutcome::Cancelled => "cancelled",
        OperationOutcome::Superseded => "superseded",
        OperationOutcome::Timeout => "timed-out",
        OperationOutcome::Rejected => "rejected",
        OperationOutcome::Panicked => "panicked",
        OperationOutcome::Aborted => "aborted",
    }
}

pub(crate) const fn component_label(component: Component) -> &'static str {
    match component {
        Component::Application => "application",
        Component::Runtime => "runtime",
        Component::Input => "input",
        Component::Socket => "socket",
        Component::Scheduler => "scheduler",
        Component::Coordinator => "coordinator",
        Component::Spotify => "spotify",
        Component::YoutubeMusic => "youtube music",
        Component::Browser => "browser",
        Component::Audio => "audio",
        Component::MediaControl => "media control",
        Component::Persistence => "persistence",
        Component::State => "state",
        Component::Ui => "ui",
        Component::Logging => "logging",
        Component::Support => "support",
    }
}

pub(crate) const fn component_runbook(component: Component) -> &'static str {
    match component {
        Component::Logging | Component::Support => "diagnostic-evidence",
        Component::Browser | Component::YoutubeMusic => "youtube-playback",
        Component::Spotify => "spotify-playback",
        Component::Audio | Component::MediaControl | Component::Coordinator => {
            "playback-coordination"
        }
        Component::Runtime | Component::Input | Component::Socket | Component::Scheduler => {
            "runtime-lifecycle"
        }
        Component::Persistence | Component::State => "session-persistence",
        Component::Application | Component::Ui => "application-ui",
    }
}

fn severity_for_health(status: HealthStatus) -> Severity {
    match status {
        HealthStatus::Degraded => Severity::Warn,
        _ => Severity::Info,
    }
}

fn safe_reference(value: &str) -> String {
    if value.len() == 8 && value.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        value.to_ascii_lowercase()
    } else {
        String::new()
    }
}

pub(crate) fn operation_label(value: &str) -> &'static str {
    match value {
        "search_spotify" | "search_youtube" => "search",
        "switch_provider" => "provider switch",
        "play_youtube_context" | "play_unified_items" | "spotify_play" | "playback" => {
            "start playback"
        }
        "spotify_seek" | "youtube_seek" => "seek playback",
        "spotify_pause" | "youtube_toggle_pause" | "spotify_toggle_pause" => {
            "pause or resume playback"
        }
        "authenticate_youtube_browser" | "test_youtube_auth" | "reauthenticate_spotify" => {
            "provider authentication"
        }
        "shutdown" | "shutdown_playback" => "application shutdown",
        "diagnostics" | "diagnostics_filter" => "diagnostics",
        "get" => "provider data request",
        value if value.starts_with("get_") => "provider data request",
        value
            if value.contains("playlist")
                || value.contains("library")
                || value.contains("queue") =>
        {
            "queue or playlist update"
        }
        value
            if value.starts_with("spotify_")
                || value.starts_with("youtube_")
                || value.starts_with("unified_") =>
        {
            "playback control"
        }
        _ => "application operation",
    }
}

pub(crate) fn safe_worker_label(value: &str) -> &'static str {
    match value {
        "client-handler" => "client handler",
        "client-socket" => "client socket",
        "browser-worker" => "browser worker",
        "media-control" => "media control",
        "player-event-watcher" => "player event watcher",
        "session-watcher" => "session watcher",
        "terminal-event-handler" => "terminal event handler",
        "ui" => "ui",
        _ => "application worker",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::observability::{
        EventCode, EventName, HealthRegistry, OperationContext, OperationSource, WriterHealth,
        WriterState,
    };
    use std::time::Instant;

    #[test]
    fn timeline_has_one_terminal_outcome_and_is_bounded() {
        let registry = ConsoleRegistry::default();
        let context = OperationContext::new("playback", OperationSource::Terminal);
        for index in 0..40 {
            let mut event = DiagnosticEvent::new(
                "run",
                Instant::now(),
                EventName::OPERATION_STAGE,
                EventCode::OPERATION_STAGE,
                Severity::Debug,
                Component::Coordinator,
                "Operation stage",
            )
            .with_operation(context.clone());
            event.uptime_ms = index;
            registry.observe(&event);
        }
        let mut terminal = DiagnosticEvent::new(
            "run",
            Instant::now(),
            EventName::REQUEST_COMPLETED,
            EventCode::REQUEST_COMPLETED,
            Severity::Debug,
            Component::Scheduler,
            "Request completed",
        )
        .with_operation(context.clone());
        terminal.fields.outcome = Some(OperationOutcome::Superseded);
        registry.observe(&terminal);
        registry.observe(&terminal);
        let timeline = registry.timeline(context.short_reference()).unwrap();
        assert_eq!(timeline.entries.len(), MAX_TIMELINE_ENTRIES);
        assert_eq!(
            timeline
                .entries
                .iter()
                .filter(|entry| entry.stage == "terminal")
                .count(),
            1
        );
        assert_eq!(timeline.outcome, Some(OperationOutcome::Superseded));
    }

    #[test]
    fn untrusted_operation_and_worker_values_are_not_rendered() {
        assert_eq!(
            operation_label("https://private/item?q=secret"),
            "application operation"
        );
        assert_eq!(
            safe_worker_label("C:\\private\\worker"),
            "application worker"
        );

        let health = HealthRegistry::default();
        let mut worker = DiagnosticEvent::new(
            "run",
            Instant::now(),
            EventName::WORKER_TRANSITION,
            EventCode::WORKER_TRANSITION,
            Severity::Info,
            Component::Runtime,
            "Worker lifecycle changed",
        );
        worker.fields.worker = Some("C:\\private\\worker".to_owned());
        worker.fields.state = Some("started".to_owned());
        health.observe(&worker);
        let mut context = OperationContext::new("playback", OperationSource::Terminal);
        context.operation = "https://private/item?q=secret".to_owned();
        let accepted = DiagnosticEvent::new(
            "run",
            Instant::now(),
            EventName::REQUEST_ACCEPTED,
            EventCode::REQUEST_ACCEPTED,
            Severity::Debug,
            Component::Scheduler,
            "Request accepted",
        )
        .with_operation(context);
        health.observe(&accepted);
        let snapshot = health.snapshot(WriterHealth {
            state: WriterState::Healthy,
            dropped_events: 0,
            files_created: 0,
            bytes_written: 0,
        });
        let model = format!("{snapshot:?}");
        assert!(model.contains("application-worker"));
        assert!(model.contains("application operation"));
        assert!(!model.contains("private"));
        assert!(!model.contains("https://"));
    }

    #[test]
    fn contextual_actions_are_capability_bounded() {
        use DiagnosticAction as A;
        let incident = actions_for(
            &DiagnosticRowId::Incident("I-ab12cd34".to_owned()),
            false,
            false,
            false,
        );
        assert!(incident.contains(&A::AcknowledgeIncident));
        assert!(incident.contains(&A::FocusSupportBundle));
        assert!(!incident.contains(&A::EnableTrace30));

        let support_before = actions_for(&DiagnosticRowId::Support, false, false, false);
        assert!(!support_before.contains(&A::ReviewBundle));
        assert!(!support_before.contains(&A::CreateFocusedBundle));
        let support_after = actions_for(&DiagnosticRowId::Support, false, true, true);
        assert!(support_after.contains(&A::ReviewBundle));
        assert!(support_after.contains(&A::CreateFocusedBundle));

        let filter = actions_for(&DiagnosticRowId::Filter, true, false, false);
        assert!(filter.contains(&A::StopTrace));

        for empty in [
            DiagnosticRowId::ComponentLoading,
            DiagnosticRowId::WorkersEmpty,
            DiagnosticRowId::OperationsEmpty,
            DiagnosticRowId::IncidentsEmpty,
        ] {
            assert_eq!(
                actions_for(&empty, false, false, false),
                vec![A::ExplainState]
            );
        }
    }

    #[test]
    fn copied_artifacts_are_new_allowlisted_bounded_text() {
        let incident = IncidentSummary {
            reference: "I-ab12cd34".to_owned(),
            event_code: "REQUEST_COMPLETED".to_owned(),
            operation: "Start playback".to_owned(),
            provider: None,
            impact: "Playback may be affected; the application remains available".to_owned(),
            cause: "The operation is currently unavailable".to_owned(),
            retryable: true,
            next_action: "Retry once, then review Diagnostics".to_owned(),
            component: Component::YoutubeMusic,
            component_health: "degraded".to_owned(),
            occurrence_count: 1,
        };
        let copied = safe_incident_text(&incident);
        assert!(copied.contains("I-ab12cd34"));
        for forbidden in ["C:\\Users", "https://", "?query=", "lyrics", "secret-token"] {
            assert!(!copied.contains(forbidden));
        }
        assert_eq!(safe_reference_text("AB12cd34").as_deref(), Some("ab12cd34"));
        assert!(safe_reference_text("video-id-private").is_none());

        let hostile = safe_health_text(
            "C:\\private\\component",
            HealthStatus::Degraded,
            "https://private/fact?q=secret",
            10,
        );
        assert!(hostile.contains("application component"));
        assert!(hostile.contains("bounded-health-fact"));
        assert!(!hostile.contains("private"));
    }

    #[test]
    fn performance_retention_is_bounded() {
        let registry = ConsoleRegistry::default();
        for index in 0..50 {
            let mut event = DiagnosticEvent::new(
                "run",
                Instant::now(),
                EventName::PERFORMANCE_BUDGET,
                EventCode::PERFORMANCE_BUDGET,
                Severity::Warn,
                Component::Ui,
                "Performance budget exceeded",
            );
            event.uptime_ms = index;
            event.fields.duration_ms = Some(20);
            event.fields.budget_ms = Some(16);
            registry.observe(&event);
        }
        assert_eq!(registry.performance().len(), MAX_PERFORMANCE_ENTRIES);
        assert_eq!(registry.performance().first().unwrap().uptime_ms, 18);
    }

    #[test]
    fn followed_operation_remains_visible_until_its_terminal_outcome() {
        let registry = ConsoleRegistry::default();
        let followed = OperationContext::new("playback", OperationSource::Terminal);
        let accepted = DiagnosticEvent::new(
            "run",
            Instant::now(),
            EventName::REQUEST_ACCEPTED,
            EventCode::REQUEST_ACCEPTED,
            Severity::Debug,
            Component::Scheduler,
            "Request accepted",
        )
        .with_operation(followed.clone());
        registry.observe(&accepted);

        for _ in 0..3 {
            let other = OperationContext::new("get", OperationSource::Terminal);
            let mut completed = DiagnosticEvent::new(
                "run",
                Instant::now(),
                EventName::REQUEST_COMPLETED,
                EventCode::REQUEST_COMPLETED,
                Severity::Info,
                Component::Scheduler,
                "Request completed",
            )
            .with_operation(other);
            completed.fields.outcome = Some(OperationOutcome::Success);
            registry.observe(&completed);
        }

        let before = registry.recent_operations(8);
        assert!(before.iter().any(|operation| {
            operation.reference == followed.short_reference() && operation.outcome.is_none()
        }));

        let mut terminal = accepted;
        terminal.event_name = "request.completed".to_owned();
        terminal.event_code = "REQUEST_COMPLETED".to_owned();
        terminal.fields.outcome = Some(OperationOutcome::Cancelled);
        registry.observe(&terminal);
        let after = registry.recent_operations(8);
        let followed = after
            .iter()
            .find(|operation| operation.reference == followed.short_reference())
            .unwrap();
        assert_eq!(followed.outcome, Some(OperationOutcome::Cancelled));
        assert_eq!(
            followed
                .entries
                .iter()
                .filter(|entry| entry.stage == "terminal")
                .count(),
            1
        );
    }

    #[test]
    fn hostile_incident_fields_are_excluded_from_rows_popups_and_copies() {
        let incident = IncidentSummary {
            reference: "C:\\private\\incident".to_owned(),
            event_code: "https://private.invalid/?query=secret".to_owned(),
            operation: "private title and artist".to_owned(),
            provider: None,
            impact: "lyrics=private".to_owned(),
            cause: "cookie=private".to_owned(),
            retryable: true,
            next_action: "token=private".to_owned(),
            component: Component::Application,
            component_health: "private".to_owned(),
            occurrence_count: 1,
        };
        let rendered = incident.render_lines().join("\n");
        let copied = safe_incident_text(&incident);
        let registry = HealthRegistry::default();
        let health = registry.snapshot(WriterHealth {
            state: WriterState::Healthy,
            dropped_events: 0,
            files_created: 0,
            bytes_written: 0,
        });
        let rows = rows(
            &health,
            DynamicFilterSnapshot {
                level: Severity::Info,
                temporary: false,
                remaining_seconds: 0,
                available: true,
            },
            &[(incident, false)],
            &[],
        );
        let row_text = rows
            .iter()
            .map(|row| format!("{} {}", row.label, row.summary))
            .collect::<Vec<_>>()
            .join("\n");
        for text in [rendered, copied, row_text] {
            for forbidden in [
                "C:\\private",
                "https://",
                "query=secret",
                "private title",
                "lyrics=private",
                "cookie=private",
                "token=private",
            ] {
                assert!(!text.contains(forbidden), "leaked {forbidden} in {text}");
            }
        }
        assert_eq!(
            safe_detail_text("C:\\private\\bundle\nhttps://private.invalid"),
            "Private diagnostic detail omitted by policy"
        );
        assert_eq!(safe_detail_text(&"x".repeat(500)).len(), 240);
    }

    #[test]
    fn rows_explain_degraded_recovery_and_filter_expiry_states() {
        let registry = HealthRegistry::default();
        let degraded = registry.snapshot(WriterHealth {
            state: WriterState::Degraded,
            dropped_events: 4,
            files_created: 1,
            bytes_written: 10,
        });
        let degraded_rows = rows(
            &degraded,
            DynamicFilterSnapshot {
                level: Severity::Trace,
                temporary: true,
                remaining_seconds: 15,
                available: true,
            },
            &[],
            &[],
        );
        assert!(degraded_rows.iter().any(|row| {
            row.id == DiagnosticRowId::Logging && row.summary == "degraded / dropped=4"
        }));
        assert!(degraded_rows.iter().any(|row| {
            row.id == DiagnosticRowId::Filter && row.summary == "trace / 15s; core on"
        }));

        let recovered = registry.snapshot(WriterHealth {
            state: WriterState::Healthy,
            dropped_events: 0,
            files_created: 1,
            bytes_written: 20,
        });
        let recovered_rows = rows(
            &recovered,
            DynamicFilterSnapshot {
                level: Severity::Info,
                temporary: false,
                remaining_seconds: 0,
                available: true,
            },
            &[],
            &[],
        );
        assert!(recovered_rows.iter().any(|row| {
            row.id == DiagnosticRowId::Logging && row.summary == "healthy / dropped=0"
        }));
        assert!(recovered_rows.iter().any(|row| {
            row.id == DiagnosticRowId::Filter && row.summary == "info / persistent; core on"
        }));

        let unavailable_rows = rows(
            &recovered,
            DynamicFilterSnapshot {
                level: Severity::Error,
                temporary: false,
                remaining_seconds: 0,
                available: false,
            },
            &[],
            &[],
        );
        assert!(unavailable_rows.iter().any(|row| {
            row.id == DiagnosticRowId::Filter && row.summary == "unavailable; core on"
        }));
    }

    #[test]
    fn youtube_playback_route_is_transferred_to_the_playback_section() {
        let registry = HealthRegistry::default();
        let health = registry.snapshot(WriterHealth {
            state: WriterState::Healthy,
            dropped_events: 0,
            files_created: 0,
            bytes_written: 0,
        });
        let mut diagnostic_rows = rows(
            &health,
            DynamicFilterSnapshot {
                level: Severity::Info,
                temporary: false,
                remaining_seconds: 0,
                available: true,
            },
            &[],
            &[],
        );
        let route = crate::state::YouTubePlaybackRoute {
            selected: Some("WEB_REMIX".to_owned()),
            order: vec![
                "TVHTML5".to_owned(),
                "WEB".to_owned(),
                "WEB_REMIX".to_owned(),
                "ANDROID_VR".to_owned(),
            ],
            learned_public_android_vr: false,
            javascript_backend: Some("Node".to_owned()),
        };
        append_youtube_playback_route_row(&mut diagnostic_rows, Some(&route));
        let route_row = diagnostic_rows
            .iter()
            .find(|row| row.id == DiagnosticRowId::PlaybackRoute)
            .expect("playback route row");
        assert_eq!(route_row.label, "Playback / YouTube route");
        assert_eq!(
            route_row.summary,
            "route: TVHTML5 -> WEB -> WEB_REMIX -> ANDROID_VR | selected: WEB_REMIX | js: Node"
        );
        assert!(
            diagnostic_rows
                .iter()
                .position(|row| row.id == DiagnosticRowId::PlaybackRoute)
                .unwrap()
                < diagnostic_rows
                    .iter()
                    .position(|row| row.id == DiagnosticRowId::Filter)
                    .unwrap()
        );
    }

    #[cfg(feature = "private-capture")]
    #[test]
    fn private_capture_row_covers_every_safe_lifecycle_state_and_recovery() {
        use crate::developer_capture::{
            SafeCaptureState, SafeOperatorFailure, SafeOperatorPhase, SafeOperatorSnapshot,
        };

        let mut snapshot = SafeOperatorSnapshot::default();
        assert!(private_capture_row(&snapshot).label.ends_with("/ loading"));

        snapshot.phase = SafeOperatorPhase::Ready;
        assert!(private_capture_row(&snapshot).label.ends_with("/ empty"));

        let cases = [
            (SafeCaptureState::ConsentRequired, "consent required"),
            (SafeCaptureState::Armed, "armed"),
            (SafeCaptureState::Claimed, "claimed"),
            (SafeCaptureState::Capturing, "capturing"),
            (SafeCaptureState::Finalizing, "finalizing"),
            (SafeCaptureState::Ready, "ready"),
            (SafeCaptureState::Incomplete, "incomplete"),
            (SafeCaptureState::Expired, "expired"),
            (SafeCaptureState::Failed, "failed"),
        ];
        for (state, label) in cases {
            snapshot.capture.state = state;
            snapshot.capture.remaining_seconds = (state == SafeCaptureState::Armed).then_some(15);
            snapshot.capture.record_count = 7;
            snapshot.capture.dropped_records = 2;
            let row = private_capture_row(&snapshot);
            assert_eq!(row.id, DiagnosticRowId::PrivateCapture);
            assert_eq!(row.label, format!("Private capture / {label}"));
            if state == SafeCaptureState::Armed {
                assert!(row.summary.contains("15s remaining"));
            }
            if state == SafeCaptureState::Capturing {
                assert!(row.summary.contains("7 bounded record(s)"));
            }
            if state == SafeCaptureState::Incomplete {
                assert!(row.summary.contains("dropped=2"));
            }
        }

        snapshot.phase = SafeOperatorPhase::Failed(SafeOperatorFailure::DerivativeUnavailable);
        assert!(private_capture_row(&snapshot).label.ends_with("/ failed"));
        snapshot.phase = SafeOperatorPhase::Ready;
        snapshot.capture.state = SafeCaptureState::Inactive;
        assert!(private_capture_row(&snapshot).label.ends_with("/ empty"));
    }

    #[cfg(feature = "private-capture")]
    #[test]
    fn private_capture_actions_are_contextual_and_exclude_lifecycle_controls() {
        use crate::developer_capture::{
            CaptureRef, SafeArtifactLabel, SafeCaptureState, SafeDerivativeState,
            SafeDerivativeView, SafeOperatorArtifact, SafeOperatorPhase, SafeOperatorSnapshot,
        };
        use DiagnosticAction as A;

        let mut snapshot = SafeOperatorSnapshot::default();
        assert_eq!(
            private_capture_actions(&snapshot),
            vec![A::ExplainCaptureSensitivity, A::ViewPrivateCaptureStatus]
        );

        snapshot.phase = SafeOperatorPhase::Ready;
        snapshot.capture.state = SafeCaptureState::ConsentRequired;
        let consent = private_capture_actions(&snapshot);
        assert!(consent.contains(&A::CancelPrivateCaptureConsent));
        assert!(!consent.contains(&A::ArmPrivateCapture));

        snapshot.capture.state = SafeCaptureState::Armed;
        let armed = private_capture_actions(&snapshot);
        assert!(armed.contains(&A::DisarmPrivateCapture));
        assert!(!armed.contains(&A::ReplayPrivateCaptureFresh));

        snapshot.phase =
            SafeOperatorPhase::Working(crate::developer_capture::SafeOperatorAction::Review);
        assert_eq!(
            private_capture_actions(&snapshot),
            vec![A::ExplainCaptureSensitivity, A::ViewPrivateCaptureStatus]
        );
        snapshot.phase = SafeOperatorPhase::Ready;

        let capture_ref = CaptureRef::from_bytes([1; 16]).safe();
        snapshot.capture.state = SafeCaptureState::Inactive;
        snapshot.artifacts.push(SafeOperatorArtifact {
            capture_ref,
            label: SafeArtifactLabel::Unlabeled,
        });
        snapshot.selected = Some(capture_ref);
        snapshot.working = Some(capture_ref);
        snapshot.failing = Some(capture_ref);
        let selected = private_capture_actions(&snapshot);
        for expected in [
            A::SelectPrivateCapture,
            A::ReviewPrivateCapture,
            A::ComparePrivateCaptures,
            A::ReplayPrivateCaptureOffline,
            A::ReplayPrivateCaptureFresh,
            A::PreviewPrivateDerivative,
            A::CreatePrivateDerivative,
            A::DeletePrivateCapture,
            A::OpenPrivateCaptureFolder,
            A::PurgeExpiredPrivateCaptures,
        ] {
            assert!(selected.contains(&expected), "missing {expected:?}");
        }
        for forbidden in [
            A::FollowOperation,
            A::StopTrace,
            A::OpenBundleFolder,
            A::AcknowledgeIncident,
        ] {
            assert!(!selected.contains(&forbidden), "unexpected {forbidden:?}");
        }

        snapshot.derivative = Some(SafeDerivativeView::new_for_test(
            SafeDerivativeState::Previewed,
            [
                "{\"safe\":true}\n",
                "abc  evidence.json\n",
                "{\"files\":3}\n",
            ],
            "unified-player provider diagnostic derivative review\nforbidden_scan=passed\n",
        ));
        let previewed = private_capture_actions(&snapshot);
        assert!(previewed.contains(&A::ViewPrivateDerivativePreview));
        assert!(previewed.contains(&A::CopyPrivateDerivativeReview));
        assert!(!previewed.contains(&A::ReviewPrivateDerivative));

        snapshot.derivative.as_mut().unwrap().state = SafeDerivativeState::Created;
        assert!(
            private_capture_actions(&snapshot).contains(&A::ReviewPrivateDerivative),
            "persisted review is available only after creation"
        );
    }
}
