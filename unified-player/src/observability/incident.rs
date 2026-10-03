use std::{
    collections::{BTreeMap, BTreeSet, VecDeque},
    sync::Arc,
};

use parking_lot::Mutex;
use serde::{Deserialize, Serialize};

use super::{Component, DiagnosticEvent, OperationOutcome, ProviderKind, Severity};

const INCIDENT_CAPACITY: usize = 64;
const PERFORMANCE_REPEAT_THRESHOLD: u16 = 3;
const PERFORMANCE_REPEAT_WINDOW_MS: u64 = 5 * 60 * 1_000;
const PERFORMANCE_BREACH_CAPACITY: usize = 32;
const PERFORMANCE_MATERIAL_MULTIPLIER: u64 = 4;
const PERFORMANCE_MATERIAL_FLOOR_MS: u64 = 5_000;

#[derive(Clone, Copy, Debug)]
struct PerformanceBreachState {
    window_started_uptime_ms: u64,
    count: u16,
    promoted: bool,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub(crate) struct IncidentSummary {
    pub(crate) reference: String,
    pub(crate) event_code: String,
    pub(crate) operation: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) provider: Option<String>,
    pub(crate) impact: String,
    pub(crate) cause: String,
    pub(crate) retryable: bool,
    pub(crate) next_action: String,
    pub(crate) component: Component,
    pub(crate) component_health: String,
    pub(crate) occurrence_count: u16,
}

impl IncidentSummary {
    pub(crate) fn safe_reference(&self) -> &str {
        if self.reference.starts_with("I-")
            && (4..=16).contains(&self.reference.len())
            && self
                .reference
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-')
        {
            &self.reference
        } else {
            "I-unavailable"
        }
    }

    pub(crate) fn safe_event_code(&self) -> &str {
        if !self.event_code.is_empty()
            && self.event_code.len() <= 64
            && self
                .event_code
                .bytes()
                .all(|byte| byte.is_ascii_uppercase() || byte.is_ascii_digit() || byte == b'_')
        {
            &self.event_code
        } else {
            "UNCLASSIFIED_INCIDENT"
        }
    }

    pub(crate) fn safe_operation(&self) -> &str {
        match self.operation.as_str() {
            "Search"
            | "Provider switch"
            | "Start playback"
            | "Seek playback"
            | "Pause or resume playback"
            | "Provider authentication"
            | "Application shutdown"
            | "Live diagnostics"
            | "Playback command"
            | "Provider data request"
            | "Application operation" => &self.operation,
            _ => "Application operation",
        }
    }

    pub(crate) fn safe_provider(&self) -> Option<&str> {
        match self.provider.as_deref() {
            Some("Spotify") => Some("Spotify"),
            Some("YouTube Music") => Some("YouTube Music"),
            _ => None,
        }
    }

    pub(crate) fn safe_cause(&self) -> &str {
        match self.cause.as_str() {
            "Authentication needs attention"
            | "A network service was unavailable"
            | "Local storage could not complete the request"
            | "A required local resource was unavailable"
            | "A response could not be understood"
            | "The operation is currently unavailable"
            | "Provider access requirements prevented playback"
            | "Provider media verification could not complete"
            | "Provider media details could not be processed"
            | "The provider refused the media request"
            | "The provider returned an invalid media range"
            | "The provider temporarily limited requests"
            | "No supported audio format was available"
            | "The operation exceeded its local time budget"
            | "An internal worker stopped unexpectedly"
            | "The operation ended unexpectedly" => &self.cause,
            _ => "The operation ended unexpectedly",
        }
    }

    fn safe_impact(&self) -> &str {
        let expected = impact(self.component);
        if self.impact == expected {
            &self.impact
        } else {
            expected
        }
    }

    pub(crate) fn safe_next_action(&self) -> &str {
        match self.next_action.as_str() {
            "Reauthenticate the affected provider, then retry"
            | "Check connectivity, then retry the operation"
            | "Check available disk space and retry"
            | "Check the local device or helper, then retry"
            | "Retry once; if it repeats, review Diagnostics"
            | "Retry the operation; use its incident reference if it repeats"
            | "Review provider access requirements or choose another item"
            | "Retry once; reauthenticate the provider if it repeats"
            | "Try another item; update the application if it repeats"
            | "Try another item; if this repeats, review provider diagnostics"
            | "Retry once; if it repeats, review media transport diagnostics"
            | "Wait briefly, then retry the operation"
            | "Choose another item or playback provider"
            | "Retry after current work settles"
            | "Restart the application and review the incident if it repeats"
            | "Retry once; review Diagnostics if it repeats" => &self.next_action,
            _ => "Retry once; review Diagnostics if it repeats",
        }
    }

    pub(crate) fn render_lines(&self) -> [String; 4] {
        let provider = self
            .safe_provider()
            .map_or_else(String::new, |provider| format!(" ({provider})"));
        [
            format!(
                "{} [{}] {}{} failed",
                self.safe_reference(),
                self.safe_event_code(),
                self.safe_operation(),
                provider,
            ),
            format!("Impact: {}", self.safe_impact()),
            format!(
                "Cause: {} | Retryable: {}",
                self.safe_cause(),
                if self.retryable { "yes" } else { "no" }
            ),
            format!(
                "Next: {} | {:?}={}",
                self.safe_next_action(),
                self.component,
                if self.component_health == "healthy" {
                    "healthy"
                } else {
                    "degraded"
                }
            ),
        ]
    }
}

#[derive(Clone, Default)]
pub(crate) struct IncidentRecorder {
    incidents: Arc<Mutex<VecDeque<IncidentSummary>>>,
    acknowledged: Arc<Mutex<BTreeSet<String>>>,
    performance_breaches: Arc<Mutex<BTreeMap<(Component, String), PerformanceBreachState>>>,
}

impl IncidentRecorder {
    pub(crate) fn observe(&self, event: &DiagnosticEvent) -> Option<IncidentSummary> {
        let occurrence_count = if event.event_name == "performance.budget" {
            self.admit_performance_budget(event)?
        } else {
            is_incident(event).then_some(1)?
        };
        let operation = event
            .operation
            .as_ref()
            .map_or("Application operation", |context| {
                operation_phrase(&context.operation)
            })
            .to_owned();
        let (cause, retryable, next_action) =
            guidance(event.fields.error_type.as_deref(), event.fields.outcome);
        let trace_reference = event
            .operation
            .as_ref()
            .map(|context| context.short_reference().to_owned())
            .or_else(|| event.fields.incident_reference.clone());
        let reference = trace_reference
            .and_then(|reference| normalize_reference(&reference))
            .unwrap_or_else(|| format!("I-{}", super::random_hex::<4>()));
        let incident = IncidentSummary {
            reference,
            event_code: normalize_event_code(&event.event_code).to_owned(),
            operation,
            provider: event
                .operation
                .as_ref()
                .and_then(|context| context.provider)
                .map(provider_label)
                .map(str::to_owned),
            impact: impact(event.component).to_owned(),
            cause: cause.to_owned(),
            retryable,
            next_action: next_action.to_owned(),
            component: event.component,
            component_health: "degraded".to_owned(),
            occurrence_count,
        };
        let mut incidents = self.incidents.lock();
        if let Some(index) = incidents
            .iter()
            .rposition(|existing| existing.reference == incident.reference)
        {
            let existing = &mut incidents[index];
            if existing.event_code == incident.event_code {
                existing.occurrence_count = existing.occurrence_count.saturating_add(1);
                return None;
            }
            if is_generic_terminal_code(&incident.event_code) {
                return None;
            }
            *existing = incident.clone();
            return Some(incident);
        }
        incidents.push_back(incident.clone());
        while incidents.len() > INCIDENT_CAPACITY {
            if let Some(removed) = incidents.pop_front() {
                self.acknowledged.lock().remove(&removed.reference);
            }
        }
        Some(incident)
    }

    fn admit_performance_budget(&self, event: &DiagnosticEvent) -> Option<u16> {
        let (duration_ms, budget_ms) = (event.fields.duration_ms?, event.fields.budget_ms?);
        if budget_ms == 0 {
            return None;
        }
        let key = (
            event.component,
            event
                .fields
                .state
                .as_deref()
                .unwrap_or("unclassified_budget")
                .to_owned(),
        );
        let mut breaches = self.performance_breaches.lock();
        if !breaches.contains_key(&key) && breaches.len() >= PERFORMANCE_BREACH_CAPACITY {
            let oldest = breaches
                .iter()
                .min_by_key(|(_, breach)| breach.window_started_uptime_ms)
                .map(|(key, _)| key.clone());
            if let Some(oldest) = oldest {
                breaches.remove(&oldest);
            }
        }
        let breach = breaches.entry(key).or_insert(PerformanceBreachState {
            window_started_uptime_ms: event.uptime_ms,
            count: 0,
            promoted: false,
        });
        if event
            .uptime_ms
            .saturating_sub(breach.window_started_uptime_ms)
            > PERFORMANCE_REPEAT_WINDOW_MS
        {
            *breach = PerformanceBreachState {
                window_started_uptime_ms: event.uptime_ms,
                count: 0,
                promoted: false,
            };
        }
        breach.count = breach.count.saturating_add(1);
        let material_threshold = budget_ms
            .saturating_mul(PERFORMANCE_MATERIAL_MULTIPLIER)
            .max(PERFORMANCE_MATERIAL_FLOOR_MS);
        let should_promote = !breach.promoted
            && (breach.count >= PERFORMANCE_REPEAT_THRESHOLD || duration_ms >= material_threshold);
        if !should_promote {
            return None;
        }
        breach.promoted = true;
        Some(breach.count)
    }

    pub(crate) fn snapshot(&self) -> Vec<IncidentSummary> {
        self.incidents.lock().iter().cloned().collect()
    }

    pub(crate) fn snapshot_with_acknowledgement(&self) -> Vec<(IncidentSummary, bool)> {
        let acknowledged = self.acknowledged.lock();
        self.incidents
            .lock()
            .iter()
            .cloned()
            .map(|incident| {
                let is_acknowledged = acknowledged.contains(&incident.reference);
                (incident, is_acknowledged)
            })
            .collect()
    }

    pub(crate) fn acknowledge(&self, reference: &str) -> bool {
        if !self
            .incidents
            .lock()
            .iter()
            .any(|incident| incident.reference == reference)
        {
            return false;
        }
        self.acknowledged.lock().insert(reference.to_owned())
    }
}

fn provider_label(provider: ProviderKind) -> &'static str {
    match provider {
        ProviderKind::Spotify => "Spotify",
        ProviderKind::YoutubeMusic => "YouTube Music",
    }
}

fn normalize_reference(reference: &str) -> Option<String> {
    if reference.starts_with("I-")
        && (4..=16).contains(&reference.len())
        && reference
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-')
    {
        return Some(reference.to_owned());
    }
    let reference = reference.strip_prefix("I-").unwrap_or(reference);
    (reference.len() == 8 && reference.bytes().all(|byte| byte.is_ascii_hexdigit()))
        .then(|| format!("I-{}", reference.to_ascii_lowercase()))
}

fn normalize_event_code(code: &str) -> &str {
    if !code.is_empty()
        && code.len() <= 64
        && code
            .bytes()
            .all(|byte| byte.is_ascii_uppercase() || byte.is_ascii_digit() || byte == b'_')
    {
        code
    } else {
        "UNCLASSIFIED_INCIDENT"
    }
}

fn is_generic_terminal_code(code: &str) -> bool {
    matches!(code, "REQUEST_COMPLETED" | "REQUEST_HANDLE_FAILED")
}

fn is_incident(event: &DiagnosticEvent) -> bool {
    if event.fields.error_type.as_deref() == Some("cancelled")
        || matches!(
            event.fields.outcome,
            Some(
                OperationOutcome::Success
                    | OperationOutcome::Cancelled
                    | OperationOutcome::Superseded
            )
        )
    {
        return false;
    }
    event.severity == Severity::Error
        || (event.severity == Severity::Warn
            && matches!(
                event.fields.outcome,
                Some(
                    OperationOutcome::Error
                        | OperationOutcome::Timeout
                        | OperationOutcome::Panicked
                        | OperationOutcome::Aborted
                        | OperationOutcome::Rejected
                )
            ))
}

fn operation_phrase(operation: &str) -> &'static str {
    match operation {
        "search_spotify" | "search_youtube" => "Search",
        "switch_provider" => "Provider switch",
        "play_youtube_context" | "play_unified_items" | "spotify_play" => "Start playback",
        "spotify_seek" | "youtube_seek" => "Seek playback",
        "spotify_pause" | "youtube_toggle_pause" | "spotify_toggle_pause" => {
            "Pause or resume playback"
        }
        "authenticate_youtube_browser" | "test_youtube_auth" | "reauthenticate_spotify" => {
            "Provider authentication"
        }
        "shutdown" | "shutdown_playback" => "Application shutdown",
        "diagnostics" => "Live diagnostics",
        "playback" => "Playback command",
        "get" => "Provider data request",
        _ => "Application operation",
    }
}

fn impact(component: Component) -> &'static str {
    match component {
        Component::Spotify
        | Component::YoutubeMusic
        | Component::Coordinator
        | Component::Audio
        | Component::MediaControl => "Playback may be affected; the application remains available",
        Component::Browser => "YouTube playback or sign-in may be affected",
        Component::Persistence | Component::State => "Recent application state may not be saved",
        Component::Logging | Component::Support => {
            "Playback is unaffected; diagnostic evidence may be incomplete"
        }
        Component::Ui | Component::Input => {
            "Playback can continue; this view or control may be affected"
        }
        Component::Runtime => "Application stability may be affected",
        Component::Socket | Component::Scheduler | Component::Application => {
            "The requested operation did not complete; the application remains available"
        }
    }
}

fn guidance(
    error_type: Option<&str>,
    outcome: Option<OperationOutcome>,
) -> (&'static str, bool, &'static str) {
    match error_type {
        Some("authentication") => (
            "Authentication needs attention",
            true,
            "Reauthenticate the affected provider, then retry",
        ),
        Some("network" | "network_unavailable") => (
            "A network service was unavailable",
            true,
            "Check connectivity, then retry the operation",
        ),
        Some("storage") => (
            "Local storage could not complete the request",
            true,
            "Check available disk space and retry",
        ),
        Some("resource" | "external_command") => (
            "A required local resource was unavailable",
            true,
            "Check the local device or helper, then retry",
        ),
        Some("decode" | "contract") => (
            "A response could not be understood",
            true,
            "Retry once; if it repeats, review Diagnostics",
        ),
        Some("unavailable" | "provider_unavailable") => (
            "The operation is currently unavailable",
            true,
            "Retry the operation; use its incident reference if it repeats",
        ),
        Some("consent_age_region") => (
            "Provider access requirements prevented playback",
            false,
            "Review provider access requirements or choose another item",
        ),
        Some("proof_token") => (
            "Provider media verification could not complete",
            true,
            "Retry once; reauthenticate the provider if it repeats",
        ),
        Some("decipher") => (
            "Provider media details could not be processed",
            false,
            "Try another item; update the application if it repeats",
        ),
        Some("media_forbidden") => (
            "The provider refused the media request",
            true,
            "Try another item; if this repeats, review provider diagnostics",
        ),
        Some("media_range_contract") => (
            "The provider returned an invalid media range",
            true,
            "Retry once; if it repeats, review media transport diagnostics",
        ),
        Some("rate_limited") => (
            "The provider temporarily limited requests",
            true,
            "Wait briefly, then retry the operation",
        ),
        Some("unsupported_format") => (
            "No supported audio format was available",
            false,
            "Choose another item or playback provider",
        ),
        _ if outcome == Some(OperationOutcome::Timeout) => (
            "The operation exceeded its local time budget",
            true,
            "Retry after current work settles",
        ),
        _ if outcome == Some(OperationOutcome::Panicked) => (
            "An internal worker stopped unexpectedly",
            false,
            "Restart the application and review the incident if it repeats",
        ),
        _ => (
            "The operation ended unexpectedly",
            true,
            "Retry once; review Diagnostics if it repeats",
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::observability::{
        EventCode, EventName, OperationContext, OperationSource, PrivacyClass,
    };
    use std::time::Instant;

    #[test]
    fn common_incidents_are_actionable_and_exclude_private_source_values() {
        let recorder = IncidentRecorder::default();
        let mut event = DiagnosticEvent::new(
            "run",
            Instant::now(),
            EventName::REQUEST_COMPLETED,
            EventCode::REQUEST_COMPLETED,
            Severity::Error,
            Component::YoutubeMusic,
            "Request completed",
        )
        .with_operation(OperationContext::new(
            "search_youtube",
            OperationSource::Terminal,
        ));
        event.fields.error_type = Some("network_unavailable".to_owned());
        event.fields.outcome = Some(OperationOutcome::Error);
        let incident = recorder.observe(&event).unwrap();
        let rendered = incident.render_lines().join("\n");
        assert!(rendered.contains("Search failed"));
        assert!(rendered.contains("Playback may be affected"));
        assert!(rendered.contains("Retryable: yes"));
        assert!(rendered.contains("Check connectivity"));
        assert!(rendered.contains("REQUEST_COMPLETED"));
        assert!(!rendered.contains("private query"));
        assert!(!rendered.contains("https://"));
        assert_eq!(event.privacy_class, PrivacyClass::SafeOperational);
    }

    #[test]
    fn routine_success_cancellation_and_supersession_stay_unobtrusive() {
        let recorder = IncidentRecorder::default();
        for outcome in [
            OperationOutcome::Success,
            OperationOutcome::Cancelled,
            OperationOutcome::Superseded,
        ] {
            let mut event = DiagnosticEvent::new(
                "run",
                Instant::now(),
                EventName::REQUEST_COMPLETED,
                EventCode::REQUEST_COMPLETED,
                Severity::Debug,
                Component::Scheduler,
                "Request completed",
            );
            event.fields.outcome = Some(outcome);
            assert!(recorder.observe(&event).is_none());
        }
        assert!(recorder.snapshot().is_empty());
    }

    #[test]
    fn acknowledgement_is_run_local_and_never_deletes_incident_evidence() {
        let recorder = IncidentRecorder::default();
        let mut event = DiagnosticEvent::new(
            "run",
            Instant::now(),
            EventName::OPERATION_STAGE,
            EventCode::OPERATION_STAGE,
            Severity::Error,
            Component::Scheduler,
            "Request completed",
        )
        .with_operation(OperationContext::new("get", OperationSource::Terminal));
        event.fields.outcome = Some(OperationOutcome::Error);
        let reference = recorder.observe(&event).unwrap().reference;
        assert!(recorder.acknowledge(&reference));
        assert_eq!(recorder.snapshot().len(), 1);
        assert!(recorder.snapshot_with_acknowledgement()[0].1);
        assert!(!recorder.acknowledge("I-expired0"));
    }

    #[test]
    fn one_trace_keeps_one_specific_incident_before_its_terminal_outcome() {
        let recorder = IncidentRecorder::default();
        let mut context = OperationContext::new("play_youtube_context", OperationSource::Terminal);
        context.provider = Some(ProviderKind::YoutubeMusic);
        let mut specific = DiagnosticEvent::new(
            "run",
            Instant::now(),
            EventName::REQUEST_COMPLETED,
            EventCode::REQUEST_COMPLETED,
            Severity::Error,
            Component::YoutubeMusic,
            "Playback request failed",
        )
        .with_operation(context.clone());
        specific.event_code = "YOUTUBE_PLAYBACK_VALIDATION_FAILED".to_owned();
        specific.fields.error_type = Some("unavailable".to_owned());
        specific.fields.outcome = Some(OperationOutcome::Error);
        assert_eq!(
            recorder.observe(&specific).unwrap().event_code,
            "YOUTUBE_PLAYBACK_VALIDATION_FAILED"
        );

        let mut fallback = specific.clone();
        fallback.event_code = "REQUEST_HANDLE_FAILED".to_owned();
        fallback.component = Component::Scheduler;
        assert!(recorder.observe(&fallback).is_none());

        let mut terminal = specific;
        terminal.event_code = "REQUEST_COMPLETED".to_owned();
        terminal.fields.error_type = None;
        assert!(recorder.observe(&terminal).is_none());

        let incidents = recorder.snapshot();
        assert_eq!(incidents.len(), 1);
        assert_eq!(
            incidents[0].event_code,
            "YOUTUBE_PLAYBACK_VALIDATION_FAILED"
        );
        assert_eq!(incidents[0].operation, "Start playback");
        assert_eq!(
            incidents[0].reference,
            format!("I-{}", context.short_reference())
        );
        assert_eq!(incidents[0].provider.as_deref(), Some("YouTube Music"));
        assert!(incidents[0].render_lines()[0].contains("(YouTube Music)"));
    }

    #[test]
    fn correlated_fallback_incident_remains_actionable_without_a_specific_code() {
        let recorder = IncidentRecorder::default();
        let context = OperationContext::new("spotify_seek", OperationSource::MediaControl);
        let mut fallback = DiagnosticEvent::new(
            "run",
            Instant::now(),
            EventName::REQUEST_COMPLETED,
            EventCode::REQUEST_COMPLETED,
            Severity::Error,
            Component::Scheduler,
            "Request failed",
        )
        .with_operation(context.clone());
        fallback.event_code = "REQUEST_HANDLE_FAILED".to_owned();
        fallback.fields.error_type = Some("unavailable".to_owned());
        fallback.fields.outcome = Some(OperationOutcome::Error);
        let incident = recorder.observe(&fallback).unwrap();
        assert_eq!(incident.operation, "Seek playback");
        assert_eq!(incident.event_code, "REQUEST_HANDLE_FAILED");
        assert!(incident.retryable);
        assert_eq!(
            incident.reference,
            format!("I-{}", context.short_reference())
        );

        let mut terminal = fallback;
        terminal.event_code = "REQUEST_COMPLETED".to_owned();
        terminal.fields.error_type = None;
        assert!(recorder.observe(&terminal).is_none());
        assert_eq!(recorder.snapshot().len(), 1);
    }

    #[test]
    fn authentication_timeout_and_panic_fixtures_have_distinct_guidance() {
        let fixtures = [
            (
                Some("authentication"),
                Some(OperationOutcome::Error),
                "Reauthenticate the affected provider",
                "Retryable: yes",
            ),
            (
                None,
                Some(OperationOutcome::Timeout),
                "Retry after current work settles",
                "Retryable: yes",
            ),
            (
                None,
                Some(OperationOutcome::Panicked),
                "Restart the application",
                "Retryable: no",
            ),
        ];
        for (category, outcome, action, retryability) in fixtures {
            let recorder = IncidentRecorder::default();
            let mut event = DiagnosticEvent::new(
                "run",
                Instant::now(),
                EventName::REQUEST_COMPLETED,
                EventCode::REQUEST_COMPLETED,
                Severity::Error,
                Component::Runtime,
                "Request completed",
            );
            event.fields.error_type = category.map(str::to_owned);
            event.fields.outcome = outcome;
            let rendered = recorder.observe(&event).unwrap().render_lines().join("\n");
            assert!(rendered.contains(action));
            assert!(rendered.contains(retryability));
            assert!(rendered.contains("Runtime=degraded"));
        }
    }

    #[test]
    fn provider_categories_render_static_specific_guidance() {
        let fixtures = [
            ("network", "network service", "Check connectivity"),
            (
                "consent_age_region",
                "access requirements",
                "choose another item",
            ),
            ("proof_token", "verification", "reauthenticate"),
            ("decipher", "media details", "update the application"),
            (
                "media_forbidden",
                "refused the media request",
                "provider diagnostics",
            ),
            (
                "media_range_contract",
                "invalid media range",
                "media transport diagnostics",
            ),
            ("rate_limited", "limited requests", "Wait briefly"),
            (
                "provider_unavailable",
                "currently unavailable",
                "incident reference",
            ),
            (
                "unsupported_format",
                "supported audio format",
                "playback provider",
            ),
        ];
        for (category, expected_cause, expected_action) in fixtures {
            let recorder = IncidentRecorder::default();
            let mut event = DiagnosticEvent::new(
                "run",
                Instant::now(),
                EventName::REQUEST_COMPLETED,
                EventCode::REQUEST_COMPLETED,
                Severity::Error,
                Component::YoutubeMusic,
                "Playback request failed",
            );
            event.fields.error_type = Some(category.to_owned());
            event.fields.outcome = Some(OperationOutcome::Error);
            let rendered = recorder.observe(&event).unwrap().render_lines().join("\n");
            assert!(rendered.contains(expected_cause), "{category}: {rendered}");
            assert!(rendered.contains(expected_action), "{category}: {rendered}");
            assert!(!rendered.contains("provider reason"));
        }
    }

    #[test]
    fn cancelled_provider_category_never_creates_an_incident() {
        let recorder = IncidentRecorder::default();
        let mut event = DiagnosticEvent::new(
            "run",
            Instant::now(),
            EventName::REQUEST_COMPLETED,
            EventCode::REQUEST_COMPLETED,
            Severity::Error,
            Component::YoutubeMusic,
            "Playback request ended",
        );
        event.fields.error_type = Some("cancelled".to_owned());
        event.fields.outcome = Some(OperationOutcome::Cancelled);
        assert!(recorder.observe(&event).is_none());
        assert!(recorder.snapshot().is_empty());
    }

    #[test]
    fn handled_warning_without_terminal_failure_stays_out_of_incidents() {
        let recorder = IncidentRecorder::default();
        let mut recovered_warning = DiagnosticEvent::new(
            "run",
            Instant::now(),
            EventName::OPERATION_STAGE,
            EventCode::OPERATION_STAGE,
            Severity::Warn,
            Component::YoutubeMusic,
            "A queue candidate was skipped before playback recovered",
        );
        recovered_warning.fields.error_type = Some("provider_unavailable".to_owned());

        assert!(recorder.observe(&recovered_warning).is_none());
        assert!(recorder.snapshot().is_empty());
    }

    fn performance_event(uptime_ms: u64, duration_ms: u64, budget_ms: u64) -> DiagnosticEvent {
        let mut event = DiagnosticEvent::new(
            "run",
            Instant::now(),
            EventName::PERFORMANCE_BUDGET,
            EventCode::PERFORMANCE_BUDGET,
            Severity::Warn,
            Component::Ui,
            "Local performance budget exceeded",
        );
        event.uptime_ms = uptime_ms;
        event.fields.state = Some("ui_render".to_owned());
        event.fields.duration_ms = Some(duration_ms);
        event.fields.budget_ms = Some(budget_ms);
        event.fields.outcome = Some(OperationOutcome::Timeout);
        event
    }

    #[test]
    fn isolated_slow_render_stays_in_performance_evidence() {
        let recorder = IncidentRecorder::default();
        let slow_render = performance_event(1_000, 925, 50);

        assert!(recorder.observe(&slow_render).is_none());
        assert!(recorder.snapshot().is_empty());
    }

    #[test]
    fn repeated_budget_breaches_promote_once_per_bounded_window() {
        let recorder = IncidentRecorder::default();
        assert!(recorder
            .observe(&performance_event(1_000, 60, 50))
            .is_none());
        assert!(recorder
            .observe(&performance_event(2_000, 70, 50))
            .is_none());
        let incident = recorder
            .observe(&performance_event(3_000, 80, 50))
            .expect("third breach is promoted");
        assert_eq!(incident.occurrence_count, PERFORMANCE_REPEAT_THRESHOLD);
        assert_eq!(incident.safe_operation(), "Application operation");
        assert!(recorder
            .observe(&performance_event(4_000, 90, 50))
            .is_none());
        assert_eq!(recorder.snapshot().len(), 1);
    }

    #[test]
    fn material_budget_breach_promotes_without_repetition() {
        let recorder = IncidentRecorder::default();
        let incident = recorder
            .observe(&performance_event(1_000, 5_000, 50))
            .expect("five-second UI stall is material");
        assert_eq!(incident.safe_event_code(), "PERFORMANCE_BUDGET");
        assert_eq!(incident.occurrence_count, 1);
    }

    #[test]
    fn performance_admission_state_is_bounded() {
        let recorder = IncidentRecorder::default();
        for index in 0..(PERFORMANCE_BREACH_CAPACITY + 8) {
            let mut event = performance_event(index as u64, 60, 50);
            event.fields.state = Some(format!("fixture_budget_{index}"));
            assert!(recorder.observe(&event).is_none());
        }
        assert_eq!(
            recorder.performance_breaches.lock().len(),
            PERFORMANCE_BREACH_CAPACITY
        );
    }
}
