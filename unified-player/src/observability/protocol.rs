use std::fmt;
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};

pub(crate) const DIAGNOSTIC_SCHEMA_VERSION: u16 = 1;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct EventName(&'static str);

impl EventName {
    pub(crate) const PROCESS_STARTED: Self = Self("process.started");
    pub(crate) const REQUEST_ACCEPTED: Self = Self("request.accepted");
    pub(crate) const REQUEST_STARTED: Self = Self("request.started");
    pub(crate) const REQUEST_COMPLETED: Self = Self("request.completed");
    pub(crate) const OPERATION_STAGE: Self = Self("operation.stage");
    pub(crate) const WORKER_TRANSITION: Self = Self("worker.transition");
    pub(crate) const HEALTH_CHANGED: Self = Self("health.changed");
    pub(crate) const UI_TRANSITION: Self = Self("ui.transition");
    pub(crate) const INCIDENT_RECORDED: Self = Self("incident.recorded");
    pub(crate) const PANIC_CAPTURED: Self = Self("panic.captured");
    pub(crate) const SUPPORT_BUNDLE_CREATED: Self = Self("support.bundle_created");
    pub(crate) const FILTER_CHANGED: Self = Self("diagnostics.filter_changed");
    pub(crate) const PERFORMANCE_BUDGET: Self = Self("performance.budget");

    const fn as_str(self) -> &'static str {
        self.0
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct EventCode(&'static str);

impl EventCode {
    pub(crate) const PROCESS_STARTED: Self = Self("PROCESS_STARTED");
    pub(crate) const REQUEST_ACCEPTED: Self = Self("REQUEST_ACCEPTED");
    pub(crate) const REQUEST_STARTED: Self = Self("REQUEST_STARTED");
    pub(crate) const REQUEST_COMPLETED: Self = Self("REQUEST_COMPLETED");
    pub(crate) const OPERATION_STAGE: Self = Self("OPERATION_STAGE");
    pub(crate) const WORKER_TRANSITION: Self = Self("WORKER_TRANSITION");
    pub(crate) const HEALTH_CHANGED: Self = Self("HEALTH_CHANGED");
    pub(crate) const UI_TRANSITION: Self = Self("UI_TRANSITION");
    pub(crate) const INCIDENT_RECORDED: Self = Self("INCIDENT_RECORDED");
    pub(crate) const PANIC_CAPTURED: Self = Self("PANIC_CAPTURED");
    pub(crate) const SUPPORT_BUNDLE_CREATED: Self = Self("SUPPORT_BUNDLE_CREATED");
    pub(crate) const FILTER_CHANGED: Self = Self("FILTER_CHANGED");
    pub(crate) const PERFORMANCE_BUDGET: Self = Self("PERFORMANCE_BUDGET");

    pub(crate) const fn as_str(self) -> &'static str {
        self.0
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum Severity {
    Trace,
    Debug,
    Info,
    Warn,
    Error,
}

impl Severity {
    pub(crate) const fn label(self) -> &'static str {
        match self {
            Self::Trace => "TRACE",
            Self::Debug => "DEBUG",
            Self::Info => "INFO",
            Self::Warn => "WARN",
            Self::Error => "ERROR",
        }
    }
}

impl From<&tracing::Level> for Severity {
    fn from(level: &tracing::Level) -> Self {
        match *level {
            tracing::Level::TRACE => Self::Trace,
            tracing::Level::DEBUG => Self::Debug,
            tracing::Level::INFO => Self::Info,
            tracing::Level::WARN => Self::Warn,
            tracing::Level::ERROR => Self::Error,
        }
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum Component {
    Application,
    Runtime,
    Input,
    Socket,
    Scheduler,
    Coordinator,
    Spotify,
    YoutubeMusic,
    Browser,
    Audio,
    MediaControl,
    Persistence,
    State,
    Ui,
    Logging,
    Support,
}

impl Component {
    pub(crate) fn from_target(target: &str) -> Self {
        if target.contains("request_scheduler") {
            Self::Scheduler
        } else if target.contains("playback_coordinator") {
            Self::Coordinator
        } else if target.contains("browser") {
            Self::Browser
        } else if target.contains("youtube") {
            Self::YoutubeMusic
        } else if target.contains("spotify") || target.contains("streaming") {
            Self::Spotify
        } else if target.contains("media_control") {
            Self::MediaControl
        } else if target.contains("runtime") {
            Self::Runtime
        } else if target.contains("event") {
            Self::Input
        } else if target.contains("ui") {
            Self::Ui
        } else if target.contains("state_application") {
            Self::State
        } else if target.contains("state") || target.contains("journal") {
            Self::Persistence
        } else if target.contains("cli") {
            Self::Socket
        } else {
            Self::Application
        }
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum PrivacyClass {
    PublicBuild,
    SafeOperational,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum OperationOutcome {
    Success,
    Error,
    Cancelled,
    Superseded,
    Timeout,
    Rejected,
    Panicked,
    Aborted,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum ProviderKind {
    Spotify,
    YoutubeMusic,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum OperationSource {
    Startup,
    Terminal,
    Mouse,
    Socket,
    MediaControl,
    Scheduler,
    Runtime,
    System,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct EventDefinition {
    pub(crate) name: EventName,
    pub(crate) code: EventCode,
    pub(crate) component: Component,
    pub(crate) default_severity: Severity,
    pub(crate) required_fields: &'static [&'static str],
    pub(crate) privacy_class: PrivacyClass,
    pub(crate) runbook: &'static str,
}

pub(crate) const EVENT_REGISTRY: &[EventDefinition] = &[
    EventDefinition {
        name: EventName::PROCESS_STARTED,
        code: EventCode::PROCESS_STARTED,
        component: Component::Application,
        default_severity: Severity::Info,
        required_fields: &["run_id", "build_revision", "schema_version"],
        privacy_class: PrivacyClass::PublicBuild,
        runbook: "process-start",
    },
    EventDefinition {
        name: EventName::REQUEST_ACCEPTED,
        code: EventCode::REQUEST_ACCEPTED,
        component: Component::Scheduler,
        default_severity: Severity::Debug,
        required_fields: &["trace_id", "operation", "source"],
        privacy_class: PrivacyClass::SafeOperational,
        runbook: "request-lifecycle",
    },
    EventDefinition {
        name: EventName::REQUEST_STARTED,
        code: EventCode::REQUEST_STARTED,
        component: Component::Scheduler,
        default_severity: Severity::Debug,
        required_fields: &["trace_id", "span_id", "queue_wait_ms"],
        privacy_class: PrivacyClass::SafeOperational,
        runbook: "request-lifecycle",
    },
    EventDefinition {
        name: EventName::REQUEST_COMPLETED,
        code: EventCode::REQUEST_COMPLETED,
        component: Component::Scheduler,
        default_severity: Severity::Info,
        required_fields: &["trace_id", "outcome", "duration_ms"],
        privacy_class: PrivacyClass::SafeOperational,
        runbook: "request-lifecycle",
    },
    EventDefinition {
        name: EventName::OPERATION_STAGE,
        code: EventCode::OPERATION_STAGE,
        component: Component::Application,
        default_severity: Severity::Debug,
        required_fields: &["trace_id", "state"],
        privacy_class: PrivacyClass::SafeOperational,
        runbook: "request-lifecycle",
    },
    EventDefinition {
        name: EventName::WORKER_TRANSITION,
        code: EventCode::WORKER_TRANSITION,
        component: Component::Runtime,
        default_severity: Severity::Info,
        required_fields: &["worker", "state"],
        privacy_class: PrivacyClass::SafeOperational,
        runbook: "worker-lifecycle",
    },
    EventDefinition {
        name: EventName::HEALTH_CHANGED,
        code: EventCode::HEALTH_CHANGED,
        component: Component::Application,
        default_severity: Severity::Info,
        required_fields: &["component", "state"],
        privacy_class: PrivacyClass::SafeOperational,
        runbook: "component-health",
    },
    EventDefinition {
        name: EventName::UI_TRANSITION,
        code: EventCode::UI_TRANSITION,
        component: Component::Ui,
        default_severity: Severity::Debug,
        required_fields: &["state", "generation"],
        privacy_class: PrivacyClass::SafeOperational,
        runbook: "ui-state",
    },
    EventDefinition {
        name: EventName::INCIDENT_RECORDED,
        code: EventCode::INCIDENT_RECORDED,
        component: Component::Application,
        default_severity: Severity::Warn,
        required_fields: &["incident_reference", "error_type"],
        privacy_class: PrivacyClass::SafeOperational,
        runbook: "incident-review",
    },
    EventDefinition {
        name: EventName::PANIC_CAPTURED,
        code: EventCode::PANIC_CAPTURED,
        component: Component::Runtime,
        default_severity: Severity::Error,
        required_fields: &["incident_reference", "fingerprint"],
        privacy_class: PrivacyClass::SafeOperational,
        runbook: "panic-captured",
    },
    EventDefinition {
        name: EventName::SUPPORT_BUNDLE_CREATED,
        code: EventCode::SUPPORT_BUNDLE_CREATED,
        component: Component::Support,
        default_severity: Severity::Info,
        required_fields: &["manifest_version", "file_count"],
        privacy_class: PrivacyClass::SafeOperational,
        runbook: "support-bundle",
    },
    EventDefinition {
        name: EventName::FILTER_CHANGED,
        code: EventCode::FILTER_CHANGED,
        component: Component::Logging,
        default_severity: Severity::Info,
        required_fields: &["state", "duration_ms"],
        privacy_class: PrivacyClass::SafeOperational,
        runbook: "dynamic-filter",
    },
    EventDefinition {
        name: EventName::PERFORMANCE_BUDGET,
        code: EventCode::PERFORMANCE_BUDGET,
        component: Component::Application,
        default_severity: Severity::Warn,
        required_fields: &["component", "duration_ms", "outcome"],
        privacy_class: PrivacyClass::SafeOperational,
        runbook: "performance-budget",
    },
];

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub(crate) struct OperationContext {
    pub(crate) trace_id: String,
    pub(crate) span_id: String,
    pub(crate) operation: String,
    pub(crate) source: OperationSource,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) provider: Option<ProviderKind>,
    pub(crate) attempt: u16,
}

impl OperationContext {
    pub(crate) fn new(operation: impl Into<String>, source: OperationSource) -> Self {
        let operation = operation.into();
        Self {
            trace_id: random_hex::<16>(),
            span_id: random_hex::<8>(),
            operation: bounded_identifier(&operation),
            source,
            provider: None,
            attempt: 1,
        }
    }

    #[allow(dead_code)] // Phase 6C propagates child spans through request ownership boundaries.
    pub(crate) fn child(&self) -> Self {
        let mut child = self.clone();
        child.span_id = random_hex::<8>();
        child
    }

    pub(crate) fn short_reference(&self) -> &str {
        self.trace_id.get(..8).unwrap_or(&self.trace_id)
    }
}

#[derive(Clone, Debug, Default, Deserialize, PartialEq, Serialize)]
pub(crate) struct EventFields {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) outcome: Option<OperationOutcome>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) error_type: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) retryable: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) attempt: Option<u16>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) duration_ms: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) retry_after_ms: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) queue_wait_ms: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) queue_depth: Option<u16>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) generation: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) selection_index: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) cancellation_reason: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) worker: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) state: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) phase: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) count: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) status_class: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) incident_reference: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) fingerprint: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) manifest_version: Option<u16>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) file_count: Option<u16>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) budget_ms: Option<u64>,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub(crate) struct DiagnosticEvent {
    pub(crate) schema_version: u16,
    pub(crate) timestamp: String,
    pub(crate) uptime_ms: u64,
    pub(crate) run_id: String,
    pub(crate) event_name: String,
    pub(crate) event_code: String,
    pub(crate) severity: Severity,
    pub(crate) component: Component,
    pub(crate) message: String,
    pub(crate) privacy_class: PrivacyClass,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) operation: Option<OperationContext>,
    #[serde(flatten)]
    pub(crate) fields: EventFields,
}

impl DiagnosticEvent {
    pub(crate) fn new(
        run_id: &str,
        started_at: Instant,
        event_name: EventName,
        event_code: EventCode,
        severity: Severity,
        component: Component,
        message: &'static str,
    ) -> Self {
        Self::new_internal(
            run_id,
            started_at,
            event_name.as_str(),
            event_code.as_str(),
            severity,
            component,
            message,
        )
    }

    pub(super) fn from_tracing(
        run_id: &str,
        started_at: Instant,
        event_code: &str,
        severity: Severity,
        component: Component,
        message: &str,
    ) -> Self {
        Self::new_internal(
            run_id,
            started_at,
            "application.message",
            event_code,
            severity,
            component,
            message,
        )
    }

    fn new_internal(
        run_id: &str,
        started_at: Instant,
        event_name: &str,
        event_code: &str,
        severity: Severity,
        component: Component,
        message: &str,
    ) -> Self {
        Self {
            schema_version: DIAGNOSTIC_SCHEMA_VERSION,
            timestamp: chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Millis, true),
            uptime_ms: duration_ms(started_at.elapsed()),
            run_id: run_id.to_owned(),
            event_name: bounded_identifier(event_name),
            event_code: bounded_identifier(event_code),
            severity,
            component,
            message: bounded_message(message),
            privacy_class: PrivacyClass::SafeOperational,
            operation: None,
            fields: EventFields::default(),
        }
    }

    pub(crate) fn with_operation(mut self, context: OperationContext) -> Self {
        self.operation = Some(context);
        self
    }

    pub(crate) fn with_outcome(mut self, outcome: OperationOutcome) -> Self {
        self.fields.outcome = Some(outcome);
        self
    }

    pub(crate) fn with_duration(mut self, duration: Duration) -> Self {
        self.fields.duration_ms = Some(duration_ms(duration));
        self
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct UiDiagnosticEntry {
    pub(crate) timestamp: String,
    pub(crate) severity: Severity,
    pub(crate) component: Component,
    pub(crate) code: String,
    pub(crate) message: String,
    pub(crate) reference: Option<String>,
    pub(crate) outcome: Option<OperationOutcome>,
    pub(crate) error_type: Option<String>,
}

impl UiDiagnosticEntry {
    pub(crate) fn from_event(event: &DiagnosticEvent) -> Self {
        Self {
            timestamp: event
                .timestamp
                .get(11..19)
                .unwrap_or(&event.timestamp)
                .to_owned(),
            severity: event.severity,
            component: event.component,
            code: event.event_code.clone(),
            message: event.message.clone(),
            reference: event
                .operation
                .as_ref()
                .map(|context| context.short_reference().to_owned()),
            outcome: event.fields.outcome,
            error_type: event.fields.error_type.clone(),
        }
    }

    pub(crate) fn render_line(&self) -> String {
        let reference = self
            .reference
            .as_ref()
            .map_or_else(String::new, |reference| format!(" ref={reference}"));
        let outcome = self
            .outcome
            .map_or_else(String::new, |outcome| format!(" outcome={outcome:?}"));
        let error_type = self
            .error_type
            .as_ref()
            .map_or_else(String::new, |error_type| format!(" cause={error_type}"));
        format!(
            "{} {:>5} {:?} [{}] {}{}{}{}",
            self.timestamp,
            self.severity.label(),
            self.component,
            self.code,
            self.message,
            reference,
            outcome,
            error_type
        )
    }
}

impl fmt::Display for UiDiagnosticEntry {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.render_line())
    }
}

pub(crate) fn random_hex<const N: usize>() -> String {
    let mut bytes = [0_u8; N];
    rand::fill(&mut bytes);
    let mut output = String::with_capacity(N * 2);
    for byte in bytes {
        use fmt::Write as _;
        let _ = write!(output, "{byte:02x}");
    }
    output
}

fn bounded_identifier(value: &str) -> String {
    value
        .chars()
        .filter(|character| {
            character.is_ascii_alphanumeric() || matches!(character, '_' | '-' | '.')
        })
        .take(64)
        .collect()
}

fn bounded_message(value: &str) -> String {
    value
        .chars()
        .filter(|character| !character.is_control())
        .take(240)
        .collect()
}

fn duration_ms(duration: Duration) -> u64 {
    duration.as_millis().try_into().unwrap_or(u64::MAX)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn event_schema_has_a_stable_golden_shape() {
        let event = DiagnosticEvent {
            schema_version: DIAGNOSTIC_SCHEMA_VERSION,
            timestamp: "2026-07-29T12:00:00.000Z".to_owned(),
            uptime_ms: 42,
            run_id: "00112233445566778899aabbccddeeff".to_owned(),
            event_name: "request.completed".to_owned(),
            event_code: "REQUEST_COMPLETED".to_owned(),
            severity: Severity::Info,
            component: Component::Scheduler,
            message: "Request completed".to_owned(),
            privacy_class: PrivacyClass::SafeOperational,
            operation: Some(OperationContext {
                trace_id: "ffeeddccbbaa99887766554433221100".to_owned(),
                span_id: "0123456789abcdef".to_owned(),
                operation: "play".to_owned(),
                source: OperationSource::Terminal,
                provider: Some(ProviderKind::YoutubeMusic),
                attempt: 1,
            }),
            fields: EventFields {
                outcome: Some(OperationOutcome::Success),
                duration_ms: Some(40),
                queue_wait_ms: Some(2),
                ..EventFields::default()
            },
        };

        let actual = serde_json::to_value(event).unwrap();
        let expected = serde_json::json!({
            "schema_version": 1,
            "timestamp": "2026-07-29T12:00:00.000Z",
            "uptime_ms": 42,
            "run_id": "00112233445566778899aabbccddeeff",
            "event_name": "request.completed",
            "event_code": "REQUEST_COMPLETED",
            "severity": "info",
            "component": "scheduler",
            "message": "Request completed",
            "privacy_class": "safe_operational",
            "operation": {
                "trace_id": "ffeeddccbbaa99887766554433221100",
                "span_id": "0123456789abcdef",
                "operation": "play",
                "source": "terminal",
                "provider": "youtube_music",
                "attempt": 1
            },
            "outcome": "success",
            "duration_ms": 40,
            "queue_wait_ms": 2
        });
        assert_eq!(actual, expected);
    }

    #[test]
    fn identifiers_and_messages_reject_control_injection() {
        let event = DiagnosticEvent::new(
            "run",
            Instant::now(),
            EventName("request\r\nforged"),
            EventCode("CODE\tFORGED"),
            Severity::Warn,
            Component::Application,
            "safe\r\nforged-event=true",
        );
        let encoded = serde_json::to_string(&event).unwrap();
        assert!(!event.event_name.contains(['\r', '\n']));
        assert!(!event.event_code.contains('\t'));
        assert!(!event.message.contains(['\r', '\n']));
        assert!(!encoded.contains("forged-event=true\\r"));
    }

    #[test]
    fn retry_after_is_serialized_as_bounded_operational_metadata() {
        let mut event = DiagnosticEvent::new(
            "run",
            Instant::now(),
            EventName("application.message"),
            EventCode("SPOTIFY_STARTUP_DEGRADED"),
            Severity::Warn,
            Component::Spotify,
            "Spotify Web API is rate-limited",
        );
        event.fields.retry_after_ms = Some(12_000);

        let encoded = serde_json::to_value(event).unwrap();
        assert_eq!(encoded["retry_after_ms"], 12_000);
    }

    #[test]
    fn event_registry_is_unique_bounded_and_documentable() {
        let mut names = std::collections::HashSet::new();
        let mut codes = std::collections::HashSet::new();
        for definition in EVENT_REGISTRY {
            assert!(names.insert(definition.name.as_str()));
            assert!(codes.insert(definition.code.as_str()));
            assert!(!definition.required_fields.is_empty());
            assert!(!definition.runbook.is_empty());
            assert!(definition
                .code
                .as_str()
                .chars()
                .all(|character| character.is_ascii_uppercase() || character == '_'));
            assert!(matches!(
                definition.privacy_class,
                PrivacyClass::PublicBuild | PrivacyClass::SafeOperational
            ));
            let _ = definition.component;
            let _ = definition.default_severity;
        }
    }
}
