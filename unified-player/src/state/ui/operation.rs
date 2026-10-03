use std::time::{Duration, Instant};

use crate::command::{BulkActionOverall, BulkActionSummary, BulkOperationId};

/// The small set of operations that deserve ordinary UI feedback.  This is
/// intentionally separate from the diagnostics operation model: routine UI
/// feedback must stay understandable and must never become a second request
/// or playback controller.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum UiOperationKind {
    Search,
    Playback,
    ProviderCommand,
    BulkAction,
}

impl UiOperationKind {
    pub const fn label(self) -> &'static str {
        match self {
            Self::Search => "Search",
            Self::Playback => "Playback",
            Self::ProviderCommand => "Command",
            Self::BulkAction => "Bulk",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum UiOperationState {
    Running,
    Completed,
    Superseded,
    Unsupported,
    Failed,
    Partial,
    Cancelled,
}

impl UiOperationState {
    pub const fn label(self) -> &'static str {
        match self {
            Self::Running => "running",
            Self::Completed => "completed",
            Self::Superseded => "superseded",
            Self::Unsupported => "unavailable",
            Self::Failed => "failed",
            Self::Partial => "partial",
            Self::Cancelled => "cancelled",
        }
    }
}

/// Shared lifecycle state for ordinary pages. The message fields are static
/// and privacy-safe; technical failures belong in Diagnostics.
#[allow(dead_code)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum UiViewStatus {
    #[default]
    Idle,
    Loading,
    Ready,
    Empty,
    Partial {
        code: &'static str,
        message: &'static str,
        next_action: &'static str,
    },
    Failed {
        code: &'static str,
        message: &'static str,
        next_action: &'static str,
    },
    Unsupported {
        code: &'static str,
        message: &'static str,
        next_action: &'static str,
    },
    Superseded {
        code: &'static str,
        message: &'static str,
        next_action: &'static str,
    },
}

impl UiViewStatus {
    pub const fn label(self) -> &'static str {
        match self {
            Self::Idle => "idle",
            Self::Loading => "loading",
            Self::Ready => "ready",
            Self::Empty => "empty",
            Self::Partial { .. } => "partial",
            Self::Failed { .. } => "failed",
            Self::Unsupported { .. } => "unavailable",
            Self::Superseded { .. } => "superseded",
        }
    }

    /// Return the bounded ordinary-UI copy for this lifecycle state. Dynamic
    /// provider details stay in Diagnostics; page renderers share this
    /// vocabulary instead of inventing loading/empty/error text.
    pub const fn display_message(self) -> &'static str {
        match self {
            Self::Idle => "Waiting for data.",
            Self::Loading => "Loading...",
            Self::Ready => "Ready.",
            Self::Empty => "No items were found.",
            Self::Partial { message, .. }
            | Self::Failed { message, .. }
            | Self::Unsupported { message, .. }
            | Self::Superseded { message, .. } => message,
        }
    }

    pub const fn next_action(self) -> Option<&'static str> {
        match self {
            Self::Failed { next_action, .. }
            | Self::Partial { next_action, .. }
            | Self::Unsupported { next_action, .. }
            | Self::Superseded { next_action, .. } => Some(next_action),
            Self::Idle | Self::Loading | Self::Ready | Self::Empty => None,
        }
    }
}

/// Count-only summary for a bulk operation.  It intentionally contains no
/// titles, provider IDs, request payloads, or raw error text.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct BulkActionUiSummary {
    pub selected_occurrences: usize,
    pub planned_operations: usize,
    pub pending: usize,
    pub succeeded: usize,
    pub failed: usize,
    pub cancelled: usize,
    pub superseded: usize,
}

impl From<BulkActionSummary> for BulkActionUiSummary {
    fn from(summary: BulkActionSummary) -> Self {
        Self {
            selected_occurrences: summary.selected_occurrences(),
            planned_operations: summary.planned_operations(),
            pending: summary.pending().operations(),
            succeeded: summary.succeeded().operations(),
            failed: summary.failed().operations(),
            cancelled: summary.cancelled().operations(),
            superseded: summary.superseded().operations(),
        }
    }
}

impl BulkActionUiSummary {
    pub const fn state(self) -> UiOperationState {
        match self.overall() {
            BulkActionOverall::InProgress => UiOperationState::Running,
            BulkActionOverall::Succeeded => UiOperationState::Completed,
            BulkActionOverall::Failed => UiOperationState::Failed,
            BulkActionOverall::Cancelled => UiOperationState::Cancelled,
            BulkActionOverall::Superseded => UiOperationState::Superseded,
            BulkActionOverall::Partial => UiOperationState::Partial,
        }
    }

    pub const fn overall(self) -> BulkActionOverall {
        if self.pending > 0 {
            return BulkActionOverall::InProgress;
        }
        if self.succeeded == self.planned_operations {
            return BulkActionOverall::Succeeded;
        }
        if self.failed == self.planned_operations {
            return BulkActionOverall::Failed;
        }
        if self.cancelled == self.planned_operations {
            return BulkActionOverall::Cancelled;
        }
        if self.superseded == self.planned_operations {
            return BulkActionOverall::Superseded;
        }
        BulkActionOverall::Partial
    }
}

/// The plan-local handle returned when the UI accepts a bulk operation.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BulkOperationHandle {
    reference: String,
    operation_ids: Vec<BulkOperationId>,
}

impl BulkOperationHandle {
    pub(crate) fn new(reference: String, operation_ids: Vec<BulkOperationId>) -> Self {
        Self {
            reference,
            operation_ids,
        }
    }

    pub(crate) fn reference(&self) -> &str {
        &self.reference
    }

    #[allow(dead_code)]
    pub(crate) fn operation_ids(&self) -> &[BulkOperationId] {
        &self.operation_ids
    }
}

/// A bounded active tracker retained while one or more plan operations are
/// pending.  The tracker is removed once every operation reaches a terminal
/// state; the ordinary status footer retains only the count summary.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct ActiveBulkOperation {
    pub(crate) handle: BulkOperationHandle,
    pub(crate) outcome: crate::command::BulkActionOutcome,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum BulkOperationRuntimeError {
    ActiveOperation,
    EmptyPlan,
    EmptyOperationIds,
    UnknownReference,
    UnknownOperation { operation_id: BulkOperationId },
    AlreadyTerminal { operation_id: BulkOperationId },
    DuplicateOperationId { operation_id: BulkOperationId },
}

/// A privacy-safe, bounded status that can be rendered in the ordinary UI.
/// All human text is supplied by the caller from static safe messages.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct UiOperationStatus {
    pub reference: String,
    pub kind: UiOperationKind,
    pub state: UiOperationState,
    pub code: &'static str,
    pub message: &'static str,
    /// Optional safe result detail such as a remote playlist MBID. Secrets,
    /// URLs, and provider response bodies must never be stored here.
    pub details: Option<String>,
    pub next_action: Option<&'static str>,
    pub expires_at: Option<Instant>,
    pub bulk_summary: Option<BulkActionUiSummary>,
}

impl UiOperationStatus {
    pub(crate) fn is_expired(&self, now: Instant) -> bool {
        self.expires_at.is_some_and(|expires_at| expires_at <= now)
    }

    #[allow(dead_code)] // Technical projection retained for diagnostics and state-contract tests.
    pub fn display_line(&self) -> String {
        let next = self
            .next_action
            .map_or_else(String::new, |action| format!(" Next: {action}"));
        let summary = self.bulk_summary.map_or_else(String::new, |summary| {
            format!(
                " selected={} planned={} pending={} succeeded={} failed={} cancelled={} superseded={}",
                summary.selected_occurrences,
                summary.planned_operations,
                summary.pending,
                summary.succeeded,
                summary.failed,
                summary.cancelled,
                summary.superseded,
            )
        });
        format!(
            "{} {} [{} / {}] {}{}{summary}{next}",
            self.kind.label(),
            self.state.label(),
            self.code,
            self.reference,
            self.message,
            self.details
                .as_deref()
                .map_or_else(String::new, |details| format!(" {details}")),
        )
    }

    /// Project an operation into the ordinary footer without exposing the
    /// diagnostic code or correlation reference. Those identifiers remain
    /// available to the Diagnostics surface and support tooling.
    pub fn ordinary_display_line(&self) -> String {
        let next = self
            .next_action
            .map_or_else(String::new, |action| format!(" Next: {action}"));
        let summary = self.bulk_summary.map_or_else(String::new, |summary| {
            format!(
                " Selected: {}; planned: {}; pending: {}; succeeded: {}; failed: {}; cancelled: {}; superseded: {}.",
                summary.selected_occurrences,
                summary.planned_operations,
                summary.pending,
                summary.succeeded,
                summary.failed,
                summary.cancelled,
                summary.superseded,
            )
        });
        format!(
            "{} {}: {}{}{}{}",
            self.kind.label(),
            self.state.label(),
            self.message,
            self.details
                .as_deref()
                .map_or_else(String::new, |details| format!(" {details}")),
            summary,
            next
        )
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct ActiveListenBrainzBackup {
    pub(crate) reference: String,
    pub(crate) playlist_id: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct ActiveListenBrainzSyncCheck {
    pub(crate) reference: String,
    pub(crate) playlist_id: String,
}

/// The search page keeps a typed state so an empty result is not confused with
/// a request that is still loading or one that failed.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub enum SearchLifecycle {
    #[default]
    Idle,
    Loading {
        reference: String,
        superseded: bool,
    },
    Ready {
        result_count: usize,
    },
    Empty,
    Failed {
        reference: String,
        code: &'static str,
        message: &'static str,
        next_action: &'static str,
    },
    Superseded {
        reference: String,
    },
}

impl SearchLifecycle {
    /// Project the search-specific payload into the shared page lifecycle
    /// vocabulary without discarding references, result counts, or query
    /// metadata kept by the search state itself.
    pub(crate) fn view_status(&self) -> UiViewStatus {
        match self {
            Self::Idle => UiViewStatus::Idle,
            Self::Loading { .. } => UiViewStatus::Loading,
            Self::Ready { .. } => UiViewStatus::Ready,
            Self::Empty => UiViewStatus::Empty,
            Self::Failed {
                code,
                message,
                next_action,
                ..
            } if *code == SEARCH_UNAVAILABLE_CODE => UiViewStatus::Unsupported {
                code,
                message,
                next_action,
            },
            Self::Failed {
                code,
                message,
                next_action,
                ..
            } => UiViewStatus::Failed {
                code,
                message,
                next_action,
            },
            Self::Superseded { .. } => UiViewStatus::Superseded {
                code: SEARCH_SUPERSEDED_CODE,
                message: SEARCH_SUPERSEDED_MESSAGE,
                next_action: SEARCH_SUPERSEDED_NEXT_ACTION,
            },
        }
    }
}

pub(crate) const SEARCH_FAILURE_CODE: &str = "SEARCH_FAILED";
pub(crate) const SEARCH_FAILURE_MESSAGE: &str = "The search could not be completed.";
pub(crate) const SEARCH_FAILURE_NEXT_ACTION: &str = "Check the connection and try again.";
pub(crate) const SEARCH_EMPTY_MESSAGE: &str = "No items were found.";
pub(crate) const SEARCH_EMPTY_NEXT_ACTION: &str = "Try a different search.";
pub(crate) const SEARCH_REPLACED_MESSAGE: &str =
    "A newer search replaced the previous request; searching...";
pub(crate) const SEARCH_UNAVAILABLE_CODE: &str = "SEARCH_PROVIDER_UNAVAILABLE";
pub(crate) const SEARCH_UNAVAILABLE_MESSAGE: &str = "This search provider is not ready.";
pub(crate) const SEARCH_UNAVAILABLE_NEXT_ACTION: &str =
    "Complete sign-in, then try the search again.";
pub(crate) const SEARCH_SUPERSEDED_CODE: &str = "SEARCH_SUPERSEDED";
pub(crate) const SEARCH_SUPERSEDED_MESSAGE: &str = "A newer search replaced this request.";
pub(crate) const SEARCH_SUPERSEDED_NEXT_ACTION: &str = "Wait for the newer search to finish.";
pub(crate) const PLAYBACK_FAILURE_CODE: &str = "PLAYBACK_FAILED";
pub(crate) const PLAYBACK_FAILURE_MESSAGE: &str = "This item could not be started.";
pub(crate) const PLAYBACK_FAILURE_NEXT_ACTION: &str = "Try again or choose another item.";
pub(crate) const UNIFIED_QUEUE_END_CODE: &str = "UNIFIED_QUEUE_END";
pub(crate) const UNIFIED_QUEUE_END_MESSAGE: &str = "End of queue reached.";
pub(crate) const UNIFIED_QUEUE_END_NEXT_ACTION: &str =
    "Add an item to the queue or start another context.";
pub(crate) const PROVIDER_UNAVAILABLE_CODE: &str = "PROVIDER_UNAVAILABLE";
pub(crate) const MUTATION_RUNNING_CODE: &str = "MUTATION_RUNNING";
pub(crate) const MUTATION_RUNNING_MESSAGE: &str = "Applying the requested change.";
pub(crate) const MUTATION_COMPLETED_CODE: &str = "MUTATION_COMPLETED";
pub(crate) const MUTATION_COMPLETED_MESSAGE: &str = "The requested change completed.";
pub(crate) const MUTATION_FAILURE_CODE: &str = "MUTATION_FAILED";
pub(crate) const MUTATION_FAILURE_MESSAGE: &str = "The requested change could not be completed.";
pub(crate) const MUTATION_FAILURE_NEXT_ACTION: &str =
    "Try again or open Diagnostics for technical detail.";
pub(crate) const MUTATION_CANCELLED_CODE: &str = "MUTATION_CANCELLED";
pub(crate) const MUTATION_CANCELLED_MESSAGE: &str = "The requested change was cancelled.";
pub(crate) const MUTATION_SUPERSEDED_CODE: &str = "MUTATION_SUPERSEDED";
pub(crate) const MUTATION_SUPERSEDED_MESSAGE: &str =
    "A newer change replaced the previous request.";
pub(crate) const LISTENBRAINZ_BACKUP_RUNNING_CODE: &str = "LISTENBRAINZ_BACKUP_RUNNING";
pub(crate) const LISTENBRAINZ_BACKUP_RUNNING_MESSAGE: &str =
    "Creating a private ListenBrainz backup.";
pub(crate) const LISTENBRAINZ_BACKUP_COMPLETED_CODE: &str = "LISTENBRAINZ_BACKUP_COMPLETED";
pub(crate) const LISTENBRAINZ_BACKUP_COMPLETED_MESSAGE: &str =
    "The ListenBrainz backup was created.";
pub(crate) const LISTENBRAINZ_BACKUP_PARTIAL_CODE: &str = "LISTENBRAINZ_BACKUP_PARTIAL";
pub(crate) const LISTENBRAINZ_BACKUP_PARTIAL_MESSAGE: &str =
    "The remote playlist was created, but the backup is incomplete.";
pub(crate) const LISTENBRAINZ_BACKUP_PARTIAL_NEXT_ACTION: &str =
    "Repair or delete the remote playlist before retrying.";
pub(crate) const LISTENBRAINZ_BACKUP_FAILED_CODE: &str = "LISTENBRAINZ_BACKUP_FAILED";
pub(crate) const LISTENBRAINZ_BACKUP_FAILED_MESSAGE: &str =
    "The ListenBrainz backup could not be created.";
pub(crate) const LISTENBRAINZ_BACKUP_FAILED_NEXT_ACTION: &str =
    "Check ListenBrainz setup and Diagnostics, then try again.";
pub(crate) const LISTENBRAINZ_BACKUP_CANCELLED_CODE: &str = "LISTENBRAINZ_BACKUP_CANCELLED";
pub(crate) const LISTENBRAINZ_BACKUP_CANCELLED_MESSAGE: &str =
    "The ListenBrainz backup was cancelled.";
pub(crate) const LISTENBRAINZ_SYNC_CHECK_RUNNING_CODE: &str = "LISTENBRAINZ_SYNC_CHECK_RUNNING";
pub(crate) const LISTENBRAINZ_SYNC_CHECK_RUNNING_MESSAGE: &str =
    "Checking the linked ListenBrainz playlist.";
pub(crate) const LISTENBRAINZ_SYNC_CHECK_COMPLETED_CODE: &str = "LISTENBRAINZ_SYNC_CHECK_COMPLETED";
pub(crate) const LISTENBRAINZ_SYNC_CHECK_COMPLETED_MESSAGE: &str =
    "The ListenBrainz sync preview is ready.";
pub(crate) const LISTENBRAINZ_SYNC_CHECK_FAILED_CODE: &str = "LISTENBRAINZ_SYNC_CHECK_FAILED";
pub(crate) const LISTENBRAINZ_SYNC_CHECK_FAILED_MESSAGE: &str =
    "The ListenBrainz sync preview could not be completed.";
pub(crate) const LISTENBRAINZ_SYNC_CHECK_FAILED_NEXT_ACTION: &str =
    "Check ListenBrainz setup and Diagnostics, then refresh the preview.";
pub(crate) const LISTENBRAINZ_SYNC_CHECK_CANCELLED_CODE: &str = "LISTENBRAINZ_SYNC_CHECK_CANCELLED";
pub(crate) const LISTENBRAINZ_SYNC_CHECK_CANCELLED_MESSAGE: &str =
    "The ListenBrainz sync preview was cancelled before any write.";
pub(crate) const LISTENBRAINZ_SYNC_APPLY_COMPLETED_CODE: &str = "LISTENBRAINZ_SYNC_APPLY_COMPLETED";
pub(crate) const LISTENBRAINZ_SYNC_APPLY_PUSH_COMPLETED_MESSAGE: &str =
    "The ListenBrainz push was verified.";
pub(crate) const LISTENBRAINZ_SYNC_APPLY_PULL_COMPLETED_MESSAGE: &str =
    "The ListenBrainz pull was applied locally.";
pub(crate) const LISTENBRAINZ_SYNC_APPLY_RESOLVE_COMPLETED_MESSAGE: &str =
    "The ListenBrainz resolution was applied.";
pub(crate) const LISTENBRAINZ_SYNC_APPLY_FAILED_CODE: &str = "LISTENBRAINZ_SYNC_APPLY_FAILED";
pub(crate) const LISTENBRAINZ_SYNC_APPLY_FAILED_MESSAGE: &str =
    "The ListenBrainz apply could not be completed.";
pub(crate) const LISTENBRAINZ_SYNC_APPLY_FAILED_NEXT_ACTION: &str =
    "Refresh the preview and try again.";
pub(crate) const LISTENBRAINZ_SYNC_APPLY_UNKNOWN_MESSAGE: &str =
    "The ListenBrainz write outcome is unknown.";
pub(crate) const LISTENBRAINZ_SYNC_APPLY_UNKNOWN_NEXT_ACTION: &str =
    "Verify the remote state before retrying.";
pub(crate) const LYRICS_FAILURE_CODE: &str = "LYRICS_FAILED";
pub(crate) const LYRICS_FAILURE_MESSAGE: &str = "Lyrics could not be loaded.";
pub(crate) const LYRICS_FAILURE_NEXT_ACTION: &str = "Try again or choose another track.";
pub(crate) const LYRICS_SUPERSEDED_CODE: &str = "LYRICS_SUPERSEDED";
pub(crate) const LYRICS_SUPERSEDED_MESSAGE: &str = "A newer lyrics request replaced this one.";
pub(crate) const LYRICS_SUPERSEDED_NEXT_ACTION: &str =
    "Wait for the newer lyrics request to finish.";
pub(crate) const SETTINGS_ACTION_RUNNING_CODE: &str = "SETTINGS_ACTION_RUNNING";
pub(crate) const SETTINGS_ACTION_COMPLETED_CODE: &str = "SETTINGS_ACTION_COMPLETED";
pub(crate) const SETTINGS_ACTION_FAILED_CODE: &str = "SETTINGS_ACTION_FAILED";
pub(crate) const SETTINGS_ACTION_NEXT_ACTION: &str =
    "Try again or open Diagnostics for technical detail.";
pub(crate) const SETTINGS_ACTION_RUNNING_MESSAGE: &str = "Applying the settings action.";
pub(crate) const SETTINGS_ACTION_COMPLETED_MESSAGE: &str = "The settings action completed.";
pub(crate) const SETTINGS_ACTION_FAILED_MESSAGE: &str =
    "The settings action could not be completed.";
pub(crate) const BULK_ACTION_RUNNING_CODE: &str = "BULK_ACTION_RUNNING";
pub(crate) const BULK_ACTION_COMPLETED_CODE: &str = "BULK_ACTION_COMPLETED";
pub(crate) const BULK_ACTION_PARTIAL_CODE: &str = "BULK_ACTION_PARTIAL";
pub(crate) const BULK_ACTION_FAILED_CODE: &str = "BULK_ACTION_FAILED";
pub(crate) const BULK_ACTION_CANCELLED_CODE: &str = "BULK_ACTION_CANCELLED";
pub(crate) const BULK_ACTION_SUPERSEDED_CODE: &str = "BULK_ACTION_SUPERSEDED";
pub(crate) const TERMINAL_STATUS_TTL: Duration = Duration::from_secs(8);

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn terminal_status_expires_but_running_status_does_not() {
        let now = Instant::now();
        let terminal = UiOperationStatus {
            reference: "ui-0001".to_owned(),
            kind: UiOperationKind::Search,
            state: UiOperationState::Failed,
            code: SEARCH_FAILURE_CODE,
            message: SEARCH_FAILURE_MESSAGE,
            details: None,
            next_action: Some(SEARCH_FAILURE_NEXT_ACTION),
            expires_at: Some(now + Duration::from_secs(1)),
            bulk_summary: None,
        };
        assert!(!terminal.is_expired(now));
        assert!(terminal.is_expired(now + Duration::from_secs(1)));
    }

    #[test]
    fn display_line_contains_only_allowlisted_fields() {
        let status = UiOperationStatus {
            reference: "ui-0002".to_owned(),
            kind: UiOperationKind::Playback,
            state: UiOperationState::Failed,
            code: PLAYBACK_FAILURE_CODE,
            message: PLAYBACK_FAILURE_MESSAGE,
            details: None,
            next_action: Some(PLAYBACK_FAILURE_NEXT_ACTION),
            expires_at: None,
            bulk_summary: None,
        };
        let line = status.display_line();
        assert!(line.contains("PLAYBACK_FAILED"));
        assert!(line.contains("ui-0002"));
        assert!(!line.contains("http"));
        assert!(!line.contains("error chain"));
    }

    #[test]
    fn bulk_summary_is_count_only_and_maps_honest_terminal_states() {
        let summary = BulkActionUiSummary {
            selected_occurrences: 3,
            planned_operations: 2,
            pending: 0,
            succeeded: 1,
            failed: 1,
            cancelled: 0,
            superseded: 0,
        };
        assert_eq!(summary.state(), UiOperationState::Partial);
        let status = UiOperationStatus {
            reference: "ui-0003".to_owned(),
            kind: UiOperationKind::BulkAction,
            state: summary.state(),
            code: BULK_ACTION_PARTIAL_CODE,
            message: "Bulk action partially completed.",
            details: None,
            next_action: None,
            expires_at: None,
            bulk_summary: Some(summary),
        };
        let line = status.display_line();
        assert!(line.contains("selected=3 planned=2 pending=0 succeeded=1 failed=1"));
        assert!(!line.contains("title"));
        assert!(!line.contains("raw-id"));
    }

    #[test]
    fn ordinary_display_line_hides_diagnostic_identifiers() {
        let status = UiOperationStatus {
            reference: "ui-0004".to_owned(),
            kind: UiOperationKind::Playback,
            state: UiOperationState::Failed,
            code: PLAYBACK_FAILURE_CODE,
            message: PLAYBACK_FAILURE_MESSAGE,
            details: None,
            next_action: Some(PLAYBACK_FAILURE_NEXT_ACTION),
            expires_at: None,
            bulk_summary: None,
        };
        let line = status.ordinary_display_line();
        assert!(line.starts_with("Playback failed: This item could not be started."));
        assert!(line.contains("Next: Try again or choose another item."));
        assert!(!line.contains(PLAYBACK_FAILURE_CODE));
        assert!(!line.contains("ui-0004"));
    }

    #[test]
    fn view_status_labels_keep_lifecycle_states_bounded() {
        assert_eq!(UiViewStatus::Idle.label(), "idle");
        assert_eq!(UiViewStatus::Loading.label(), "loading");
        assert_eq!(UiViewStatus::Ready.label(), "ready");
        assert_eq!(UiViewStatus::Empty.label(), "empty");
        let partial = UiViewStatus::Partial {
            code: "PARTIAL_RESULTS",
            message: "Some results could not be loaded.",
            next_action: "Open Diagnostics or retry the request.",
        };
        assert_eq!(partial.label(), "partial");
        assert_eq!(
            partial.display_message(),
            "Some results could not be loaded."
        );
        assert_eq!(
            partial.next_action(),
            Some("Open Diagnostics or retry the request.")
        );
        let failed = UiViewStatus::Failed {
            code: "VIEW_FAILED",
            message: "The view could not be loaded.",
            next_action: "Try again.",
        };
        assert_eq!(failed.label(), "failed");
        assert_eq!(
            UiViewStatus::Unsupported {
                code: "VIEW_UNAVAILABLE",
                message: "This view is unavailable.",
                next_action: "Choose another view.",
            }
            .label(),
            "unavailable"
        );
        assert_eq!(
            UiViewStatus::Superseded {
                code: "VIEW_REPLACED",
                message: "A newer view replaced this one.",
                next_action: "Wait for the newer view.",
            }
            .label(),
            "superseded"
        );
        assert_eq!(UiViewStatus::Loading.display_message(), "Loading...");
        assert_eq!(
            UiViewStatus::Empty.display_message(),
            "No items were found."
        );
        assert_eq!(UiViewStatus::Ready.next_action(), None);
        assert_eq!(failed.next_action(), Some("Try again."));
    }

    #[test]
    fn search_lifecycle_projects_to_shared_view_status() {
        assert_eq!(SearchLifecycle::Idle.view_status(), UiViewStatus::Idle);
        assert_eq!(
            SearchLifecycle::Loading {
                reference: "ui-0004".to_owned(),
                superseded: false,
            }
            .view_status(),
            UiViewStatus::Loading
        );
        assert_eq!(
            SearchLifecycle::Ready { result_count: 2 }.view_status(),
            UiViewStatus::Ready
        );
        assert_eq!(SearchLifecycle::Empty.view_status(), UiViewStatus::Empty);
        assert_eq!(
            SearchLifecycle::Failed {
                reference: "ui-0005".to_owned(),
                code: SEARCH_UNAVAILABLE_CODE,
                message: SEARCH_UNAVAILABLE_MESSAGE,
                next_action: SEARCH_UNAVAILABLE_NEXT_ACTION,
            }
            .view_status()
            .label(),
            "unavailable"
        );
        assert_eq!(
            SearchLifecycle::Superseded {
                reference: "ui-0006".to_owned(),
            }
            .view_status()
            .label(),
            "superseded"
        );
    }
}
