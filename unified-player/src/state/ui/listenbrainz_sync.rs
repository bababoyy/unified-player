use crate::state::{
    ListenBrainzProjectionStatus, ListenBrainzSyncStatus, PlaylistEntryId, PlaylistLink,
    UnifiedPlaylist,
};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ListenBrainzSyncSide {
    Local,
    ListenBrainz,
    Both,
}

impl ListenBrainzSyncSide {
    pub const fn label(self) -> &'static str {
        match self {
            Self::Local => "Local",
            Self::ListenBrainz => "ListenBrainz",
            Self::Both => "Both",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ListenBrainzSyncDetailAction {
    Added,
    Removed,
    IdentityChanged,
    PlaylistRenamed,
    Reordered,
    Conflict,
}

impl ListenBrainzSyncDetailAction {
    pub const fn label(self) -> &'static str {
        match self {
            Self::Added => "Added",
            Self::Removed => "Removed",
            Self::IdentityChanged => "Changed",
            Self::PlaylistRenamed => "Renamed",
            Self::Reordered => "Reordered",
            Self::Conflict => "Conflict",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ListenBrainzSyncConflictKind {
    AddAdd,
    DeleteEdit,
    Mapping,
    Reorder,
    ManifestProjectionDrift,
    UnlinkedRemoteRow,
    StaleBase,
    DuplicateAmbiguity,
    Schema,
}

impl ListenBrainzSyncConflictKind {
    pub const fn label(self) -> &'static str {
        match self {
            Self::AddAdd => "add/add",
            Self::DeleteEdit => "delete/edit",
            Self::Mapping => "mapping",
            Self::Reorder => "reorder",
            Self::ManifestProjectionDrift => "projection drift",
            Self::UnlinkedRemoteRow => "unlinked row",
            Self::StaleBase => "stale base",
            Self::DuplicateAmbiguity => "duplicate",
            Self::Schema => "schema",
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ListenBrainzSyncDetailRow {
    pub occurrence: Option<PlaylistEntryId>,
    pub side: ListenBrainzSyncSide,
    pub action: ListenBrainzSyncDetailAction,
    pub title: String,
    pub artist: String,
    pub provider: String,
    pub conflict: Option<ListenBrainzSyncConflictKind>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ListenBrainzSyncPreview {
    pub playlist_id: String,
    pub operation_reference: String,
    pub rows: Vec<ListenBrainzSyncDetailRow>,
    /// Conflict kinds in sync-plan order. UI rows are grouped by change, so
    /// positions among conflicted rows do not match plan indices; conflict
    /// decisions must use this list to address the plan correctly.
    pub conflicts: Vec<ListenBrainzSyncConflictKind>,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
#[allow(dead_code)] // Write/verify stages are part of the apply lifecycle gated by this preview UI.
pub enum ListenBrainzSyncLifecycle {
    #[default]
    Idle,
    Checking,
    Planning,
    WritingListenBrainz,
    Verifying,
    Recovering,
    Ready {
        remote_changed: bool,
        both_same: bool,
        conflicts: usize,
        manifest_only: usize,
    },
    CannotPlan {
        next_action: &'static str,
    },
    Failed {
        message: &'static str,
        next_action: &'static str,
    },
}

impl ListenBrainzSyncLifecycle {
    pub const fn loading_label(self) -> Option<&'static str> {
        match self {
            Self::Checking => Some("Checking remote"),
            Self::Planning => Some("Planning"),
            Self::WritingListenBrainz => Some("Writing ListenBrainz"),
            Self::Verifying => Some("Verifying"),
            Self::Recovering => Some("Recovering"),
            _ => None,
        }
    }

    pub const fn is_busy(self) -> bool {
        self.loading_label().is_some()
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ListenBrainzSyncMeaning {
    Disabled,
    ReadOnlyDisabled,
    NeedsAuth,
    Unlinked,
    Uninitialized,
    Checking,
    Clean,
    LocalChanges,
    RemoteChanges,
    BothSame,
    BothChanged,
    Conflict,
    CannotPlan,
    Partial,
    OutcomeUnknown,
    Detached,
    Failed,
}

impl ListenBrainzSyncMeaning {
    pub const fn label(self) -> &'static str {
        match self {
            Self::Disabled => "disabled",
            Self::ReadOnlyDisabled => "checking disabled",
            Self::NeedsAuth => "needs auth",
            Self::Unlinked => "unlinked",
            Self::Uninitialized => "needs init",
            Self::Checking => "checking",
            Self::Clean => "clean",
            Self::LocalChanges => "local changes",
            Self::RemoteChanges => "remote changes",
            Self::BothSame => "both changed alike",
            Self::BothChanged => "both changed",
            Self::Conflict => "conflict",
            Self::CannotPlan => "cannot plan",
            Self::Partial => "partial",
            Self::OutcomeUnknown => "outcome unknown",
            Self::Detached => "detached",
            Self::Failed => "failed",
        }
    }

    /// Version-control aid color for this status. Styles resolve through the
    /// theme so every state stays configurable; defaults keep clean green,
    /// changed yellow, attention-demanding red, and inactive gray.
    pub fn status_style(self, theme: &crate::config::Theme) -> ratatui::style::Style {
        match self {
            Self::Clean | Self::BothSame => theme.sync_clean(),
            Self::LocalChanges | Self::RemoteChanges | Self::BothChanged => theme.sync_changed(),
            Self::Conflict
            | Self::CannotPlan
            | Self::Partial
            | Self::OutcomeUnknown
            | Self::Failed => theme.sync_conflict(),
            Self::Disabled
            | Self::ReadOnlyDisabled
            | Self::NeedsAuth
            | Self::Unlinked
            | Self::Uninitialized
            | Self::Checking
            | Self::Detached => theme.sync_neutral(),
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ListenBrainzSyncUiAction {
    EnableInSettings,
    EnableReadOnlyChecking,
    ConfigureToken,
    BackUp,
    InitializeBase,
    Check,
    Refresh,
    Retry,
    PreviewPush,
    PreviewPull,
    ReviewConflicts,
    InspectRecovery,
    VerifyRemote,
}

impl ListenBrainzSyncUiAction {
    pub const fn label(self) -> &'static str {
        match self {
            Self::EnableInSettings => "Enable in Settings",
            Self::EnableReadOnlyChecking => "Enable read-only checking",
            Self::ConfigureToken => "Configure token",
            Self::BackUp => "Back up",
            Self::InitializeBase => "Start sync tracking",
            Self::Check => "Check for changes",
            Self::Refresh => "Refresh the preview",
            Self::Retry => "Retry the preview",
            Self::PreviewPush => "Review outgoing changes",
            Self::PreviewPull => "Review incoming changes",
            Self::ReviewConflicts => "Review conflicts",
            Self::InspectRecovery => "Check an unfinished operation",
            Self::VerifyRemote => "Verify remote",
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ListenBrainzSyncSummary {
    pub meaning: ListenBrainzSyncMeaning,
    pub last_verified_at: Option<u64>,
    pub local_changed: bool,
    pub remote_changed: Option<bool>,
    pub conflicts: usize,
    pub manifest_only: usize,
    pub primary_action: ListenBrainzSyncUiAction,
    pub apply_enabled: bool,
    pub disabled_reason: Option<&'static str>,
    pub loading_label: Option<&'static str>,
    pub status_message: Option<&'static str>,
}

impl ListenBrainzSyncSummary {
    pub fn project(
        enabled: bool,
        read_only_checking: bool,
        has_auth: bool,
        playlist: &UnifiedPlaylist,
        link: Option<&PlaylistLink>,
        lifecycle: ListenBrainzSyncLifecycle,
    ) -> Self {
        let sync = link.and_then(|link| link.listenbrainz_sync.as_ref());
        let linked = link.is_some_and(|link| link.listenbrainz_playlist_id.is_some());
        let local_changed =
            sync.is_some_and(|sync| playlist.snapshot_hash() != sync.base.local_snapshot_hash);
        let (remote_changed, both_same, conflicts, manifest_only) = match lifecycle {
            ListenBrainzSyncLifecycle::Ready {
                remote_changed,
                both_same,
                conflicts,
                manifest_only,
            } => (Some(remote_changed), both_same, conflicts, manifest_only),
            _ => (
                None,
                false,
                0,
                sync.map_or(0, |sync| {
                    sync.base
                        .entries
                        .iter()
                        .filter(|entry| {
                            entry.projection_status != ListenBrainzProjectionStatus::Resolved
                        })
                        .count()
                }),
            ),
        };
        let meaning = if !enabled {
            ListenBrainzSyncMeaning::Disabled
        } else if !read_only_checking {
            ListenBrainzSyncMeaning::ReadOnlyDisabled
        } else if !has_auth {
            ListenBrainzSyncMeaning::NeedsAuth
        } else if !linked {
            ListenBrainzSyncMeaning::Unlinked
        } else if sync.is_none() {
            ListenBrainzSyncMeaning::Uninitialized
        } else if lifecycle.is_busy() {
            ListenBrainzSyncMeaning::Checking
        } else if matches!(lifecycle, ListenBrainzSyncLifecycle::CannotPlan { .. }) {
            ListenBrainzSyncMeaning::CannotPlan
        } else if matches!(lifecycle, ListenBrainzSyncLifecycle::Failed { .. }) {
            ListenBrainzSyncMeaning::Failed
        } else if conflicts > 0 {
            ListenBrainzSyncMeaning::Conflict
        } else {
            let Some(sync) = sync else {
                unreachable!("linked sync state checked above");
            };
            match sync.status {
                ListenBrainzSyncStatus::Conflict => ListenBrainzSyncMeaning::Conflict,
                ListenBrainzSyncStatus::Partial => ListenBrainzSyncMeaning::Partial,
                ListenBrainzSyncStatus::OutcomeUnknown | ListenBrainzSyncStatus::Pending => {
                    ListenBrainzSyncMeaning::OutcomeUnknown
                }
                ListenBrainzSyncStatus::Detached => ListenBrainzSyncMeaning::Detached,
                ListenBrainzSyncStatus::Drifted => ListenBrainzSyncMeaning::LocalChanges,
                ListenBrainzSyncStatus::Clean => match (local_changed, remote_changed) {
                    (false, Some(false) | None) => ListenBrainzSyncMeaning::Clean,
                    (true, Some(true)) if both_same => ListenBrainzSyncMeaning::BothSame,
                    (true, Some(true)) => ListenBrainzSyncMeaning::BothChanged,
                    (true, _) => ListenBrainzSyncMeaning::LocalChanges,
                    (false, Some(true)) => ListenBrainzSyncMeaning::RemoteChanges,
                },
            }
        };
        let primary_action = match meaning {
            ListenBrainzSyncMeaning::Disabled => ListenBrainzSyncUiAction::EnableInSettings,
            ListenBrainzSyncMeaning::ReadOnlyDisabled => {
                ListenBrainzSyncUiAction::EnableReadOnlyChecking
            }
            ListenBrainzSyncMeaning::NeedsAuth => ListenBrainzSyncUiAction::ConfigureToken,
            ListenBrainzSyncMeaning::Unlinked | ListenBrainzSyncMeaning::Detached => {
                ListenBrainzSyncUiAction::BackUp
            }
            ListenBrainzSyncMeaning::Uninitialized => ListenBrainzSyncUiAction::InitializeBase,
            ListenBrainzSyncMeaning::Clean
                if matches!(lifecycle, ListenBrainzSyncLifecycle::Ready { .. }) =>
            {
                ListenBrainzSyncUiAction::Refresh
            }
            ListenBrainzSyncMeaning::Clean | ListenBrainzSyncMeaning::Checking => {
                ListenBrainzSyncUiAction::Check
            }
            ListenBrainzSyncMeaning::Failed => ListenBrainzSyncUiAction::Retry,
            ListenBrainzSyncMeaning::LocalChanges | ListenBrainzSyncMeaning::BothChanged => {
                ListenBrainzSyncUiAction::PreviewPush
            }
            ListenBrainzSyncMeaning::RemoteChanges => ListenBrainzSyncUiAction::PreviewPull,
            ListenBrainzSyncMeaning::BothSame | ListenBrainzSyncMeaning::OutcomeUnknown => {
                ListenBrainzSyncUiAction::VerifyRemote
            }
            ListenBrainzSyncMeaning::Conflict => ListenBrainzSyncUiAction::ReviewConflicts,
            ListenBrainzSyncMeaning::CannotPlan | ListenBrainzSyncMeaning::Partial => {
                ListenBrainzSyncUiAction::InspectRecovery
            }
        };
        let apply_enabled = enabled
            && read_only_checking
            && has_auth
            && linked
            && !lifecycle.is_busy()
            && !matches!(
                meaning,
                ListenBrainzSyncMeaning::Conflict
                    | ListenBrainzSyncMeaning::CannotPlan
                    | ListenBrainzSyncMeaning::Failed
                    | ListenBrainzSyncMeaning::Partial
                    | ListenBrainzSyncMeaning::OutcomeUnknown
                    | ListenBrainzSyncMeaning::Uninitialized
            );
        let disabled_reason = (!apply_enabled).then_some(match meaning {
            ListenBrainzSyncMeaning::Disabled => "Enable ListenBrainz integration in Settings.",
            ListenBrainzSyncMeaning::ReadOnlyDisabled => {
                "Enable ListenBrainz read-only checking in Settings."
            }
            ListenBrainzSyncMeaning::NeedsAuth => "Configure a ListenBrainz token first.",
            ListenBrainzSyncMeaning::Unlinked => "Create or attach a private backup first.",
            ListenBrainzSyncMeaning::Uninitialized => "Start sync tracking before previewing.",
            ListenBrainzSyncMeaning::Checking => "Wait for the current plan to finish.",
            ListenBrainzSyncMeaning::Conflict => "Review every conflict before applying.",
            ListenBrainzSyncMeaning::Partial => "Inspect the partial operation before retrying.",
            ListenBrainzSyncMeaning::OutcomeUnknown => {
                "Verify the remote state; automatic retry is disabled."
            }
            ListenBrainzSyncMeaning::Failed => "Retry or refresh the preview before applying.",
            _ => "Refresh the sync state before applying.",
        });
        let status_message = match lifecycle {
            ListenBrainzSyncLifecycle::CannotPlan { next_action } => Some(next_action),
            ListenBrainzSyncLifecycle::Failed {
                message,
                next_action,
            } => Some(if message.is_empty() {
                next_action
            } else {
                message
            }),
            _ => None,
        };
        Self {
            meaning,
            last_verified_at: sync.map(|sync| sync.base.verified_at),
            local_changed,
            remote_changed,
            conflicts,
            manifest_only,
            primary_action,
            apply_enabled,
            disabled_reason,
            loading_label: lifecycle.loading_label(),
            status_message,
        }
    }

    pub fn compact_line(&self) -> String {
        let verified = self.verified_label();
        let remote = self.remote_label();
        let mut line = format!(
            "ListenBrainz: {} | verified {} | local {} | remote {} | conflicts {} | manifest-only {} | {}",
            self.meaning.label(),
            verified,
            if self.local_changed { "changed" } else { "clean" },
            remote,
            self.conflicts,
            self.manifest_only,
            self.loading_label.unwrap_or_else(|| self.primary_action.label()),
        );
        if let Some(reason) = self.disabled_reason {
            line.push_str(" | ");
            line.push_str(reason);
        }
        if let Some(message) = self.status_message {
            line.push_str(" | ");
            line.push_str(message);
        }
        line
    }

    pub fn display_text(&self, width: u16) -> String {
        if width >= 90 {
            return self.compact_line();
        }
        format!(
            "ListenBrainz: {} | {}\nverified {} | local {}\nremote {} | conflicts {} | manifest-only {}",
            self.meaning.label(),
            self.loading_label.unwrap_or_else(|| self.primary_action.label()),
            self.verified_label(),
            if self.local_changed { "changed" } else { "clean" },
            self.remote_label(),
            self.conflicts,
            self.manifest_only,
        )
    }

    fn verified_label(&self) -> String {
        self.last_verified_at.map_or_else(
            || "never".to_owned(),
            |value| {
                chrono::DateTime::from_timestamp(value as i64, 0).map_or_else(
                    || "unknown".to_owned(),
                    |time| time.format("%Y-%m-%d %H:%MZ").to_string(),
                )
            },
        )
    }

    fn remote_label(&self) -> &'static str {
        self.remote_changed.map_or(
            "unknown",
            |changed| {
                if changed {
                    "changed"
                } else {
                    "clean"
                }
            },
        )
    }

    pub const fn primary_command_action(&self) -> Option<crate::command::Action> {
        use crate::command::Action;
        match self.primary_action {
            ListenBrainzSyncUiAction::EnableInSettings
            | ListenBrainzSyncUiAction::EnableReadOnlyChecking
            | ListenBrainzSyncUiAction::ConfigureToken => None,
            ListenBrainzSyncUiAction::BackUp => Some(Action::BackupUnifiedPlaylistToListenBrainz),
            ListenBrainzSyncUiAction::InitializeBase => {
                Some(Action::InitializeUnifiedPlaylistListenBrainzBase)
            }
            ListenBrainzSyncUiAction::Check => Some(Action::CheckUnifiedPlaylistListenBrainzSync),
            ListenBrainzSyncUiAction::Refresh => {
                Some(Action::RefreshUnifiedPlaylistListenBrainzSync)
            }
            ListenBrainzSyncUiAction::Retry => Some(Action::RetryUnifiedPlaylistListenBrainzSync),
            ListenBrainzSyncUiAction::PreviewPush => {
                Some(Action::PreviewUnifiedPlaylistListenBrainzPush)
            }
            ListenBrainzSyncUiAction::PreviewPull => {
                Some(Action::PreviewUnifiedPlaylistListenBrainzPull)
            }
            ListenBrainzSyncUiAction::ReviewConflicts => {
                Some(Action::ReviewUnifiedPlaylistListenBrainzConflicts)
            }
            ListenBrainzSyncUiAction::InspectRecovery | ListenBrainzSyncUiAction::VerifyRemote => {
                Some(Action::RecoverUnifiedPlaylistListenBrainzSync)
            }
        }
    }
}

/// Every operation owned by the `ListenBrainz` workspace window.
///
/// The workspace is the only TUI surface that lists these actions together.
/// The Unified Playlist context menu exposes a single `ListenBrainz` entry so
/// the playlist itself stays uncrowded.
pub const LISTENBRAINZ_WORKSPACE_ACTIONS: [crate::command::Action; 13] = [
    crate::command::Action::BackupUnifiedPlaylistToListenBrainz,
    crate::command::Action::InitializeUnifiedPlaylistListenBrainzBase,
    crate::command::Action::CheckUnifiedPlaylistListenBrainzSync,
    crate::command::Action::RefreshUnifiedPlaylistListenBrainzSync,
    crate::command::Action::RetryUnifiedPlaylistListenBrainzSync,
    crate::command::Action::PreviewUnifiedPlaylistListenBrainzPush,
    crate::command::Action::PreviewUnifiedPlaylistListenBrainzPull,
    crate::command::Action::ReviewUnifiedPlaylistListenBrainzConflicts,
    crate::command::Action::RecoverUnifiedPlaylistListenBrainzSync,
    crate::command::Action::RollbackUnifiedPlaylistListenBrainzPull,
    crate::command::Action::ApplyUnifiedPlaylistListenBrainzPush,
    crate::command::Action::ApplyUnifiedPlaylistListenBrainzPull,
    crate::command::Action::ApplyUnifiedPlaylistListenBrainzResolve,
];

#[cfg(test)]
mod tests {
    use super::{
        ListenBrainzSyncLifecycle, ListenBrainzSyncMeaning, ListenBrainzSyncSummary,
        ListenBrainzSyncUiAction,
    };
    use crate::state::{
        ListenBrainzSyncBase, ListenBrainzSyncState, ListenBrainzSyncStatus, PlaylistLink,
        UnifiedPlaylist,
    };

    fn linked(playlist: &UnifiedPlaylist, status: ListenBrainzSyncStatus) -> PlaylistLink {
        let mut sync = ListenBrainzSyncState::verified(ListenBrainzSyncBase {
            manifest_schema_version: 2,
            unified_playlist_id: playlist.id.clone(),
            playlist_name: playlist.name.clone(),
            local_snapshot_hash: playlist.snapshot_hash(),
            canonical_manifest_hash: "2".repeat(64),
            remote_fingerprint: "3".repeat(64),
            entries: Vec::new(),
            verified_at: 42,
        })
        .unwrap();
        sync.status = status;
        PlaylistLink {
            unified_playlist_id: playlist.id.clone(),
            listenbrainz_playlist_id: Some("remote".to_owned()),
            listenbrainz_sync: Some(sync),
            ..PlaylistLink::default()
        }
    }

    #[test]
    fn summary_matrix_gates_disabled_auth_loading_conflict_and_unknown_states() {
        let playlist = UnifiedPlaylist {
            id: "local".to_owned(),
            name: "Mix".to_owned(),
            ..UnifiedPlaylist::default()
        };
        let link = linked(&playlist, ListenBrainzSyncStatus::Clean);
        for (enabled, auth, lifecycle, expected) in [
            (
                false,
                true,
                ListenBrainzSyncLifecycle::Idle,
                ListenBrainzSyncMeaning::Disabled,
            ),
            (
                true,
                false,
                ListenBrainzSyncLifecycle::Idle,
                ListenBrainzSyncMeaning::NeedsAuth,
            ),
            (
                true,
                true,
                ListenBrainzSyncLifecycle::Checking,
                ListenBrainzSyncMeaning::Checking,
            ),
        ] {
            let summary = ListenBrainzSyncSummary::project(
                enabled,
                true,
                auth,
                &playlist,
                Some(&link),
                lifecycle,
            );
            assert_eq!(summary.meaning, expected);
            assert!(!summary.apply_enabled);
            assert!(summary.disabled_reason.is_some());
        }
        let checking_disabled = ListenBrainzSyncSummary::project(
            true,
            false,
            true,
            &playlist,
            Some(&link),
            ListenBrainzSyncLifecycle::Idle,
        );
        assert_eq!(
            checking_disabled.meaning,
            ListenBrainzSyncMeaning::ReadOnlyDisabled
        );
        assert!(checking_disabled
            .disabled_reason
            .is_some_and(|reason| reason.contains("read-only")));

        let mut outcome_unknown = link.clone();
        outcome_unknown
            .listenbrainz_sync
            .as_mut()
            .expect("sync state")
            .status = ListenBrainzSyncStatus::OutcomeUnknown;
        let unknown = ListenBrainzSyncSummary::project(
            true,
            true,
            true,
            &playlist,
            Some(&outcome_unknown),
            ListenBrainzSyncLifecycle::Idle,
        );
        assert_eq!(unknown.meaning, ListenBrainzSyncMeaning::OutcomeUnknown);
        assert_eq!(
            unknown.primary_action,
            ListenBrainzSyncUiAction::VerifyRemote
        );
        assert_ne!(unknown.primary_action, ListenBrainzSyncUiAction::Retry);
        let conflict = linked(&playlist, ListenBrainzSyncStatus::Conflict);
        let summary = ListenBrainzSyncSummary::project(
            true,
            true,
            true,
            &playlist,
            Some(&conflict),
            ListenBrainzSyncLifecycle::Idle,
        );
        assert_eq!(
            summary.primary_action,
            ListenBrainzSyncUiAction::ReviewConflicts
        );
        assert!(!summary.apply_enabled);

        let cannot_plan = ListenBrainzSyncSummary::project(
            true,
            true,
            true,
            &playlist,
            Some(&link),
            ListenBrainzSyncLifecycle::CannotPlan {
                next_action: "Repair the manifest.",
            },
        );
        assert_eq!(cannot_plan.meaning, ListenBrainzSyncMeaning::CannotPlan);
        let failed = ListenBrainzSyncSummary::project(
            true,
            true,
            true,
            &playlist,
            Some(&link),
            ListenBrainzSyncLifecycle::Failed {
                message: "Check failed.",
                next_action: "Try again.",
            },
        );
        assert_eq!(failed.meaning, ListenBrainzSyncMeaning::Failed);
        assert_eq!(failed.primary_action, ListenBrainzSyncUiAction::Retry);
        assert_eq!(
            failed.primary_command_action(),
            Some(crate::command::Action::RetryUnifiedPlaylistListenBrainzSync)
        );

        let refreshed = ListenBrainzSyncSummary::project(
            true,
            true,
            true,
            &playlist,
            Some(&link),
            ListenBrainzSyncLifecycle::Ready {
                remote_changed: false,
                both_same: false,
                conflicts: 0,
                manifest_only: 0,
            },
        );
        assert_eq!(refreshed.meaning, ListenBrainzSyncMeaning::Clean);
        assert_eq!(refreshed.primary_action, ListenBrainzSyncUiAction::Refresh);
        assert_eq!(
            refreshed.primary_command_action(),
            Some(crate::command::Action::RefreshUnifiedPlaylistListenBrainzSync)
        );

        let planned_conflict = ListenBrainzSyncSummary::project(
            true,
            true,
            true,
            &playlist,
            Some(&link),
            ListenBrainzSyncLifecycle::Ready {
                remote_changed: true,
                both_same: false,
                conflicts: 1,
                manifest_only: 0,
            },
        );
        assert_eq!(planned_conflict.meaning, ListenBrainzSyncMeaning::Conflict);
        assert!(!planned_conflict.apply_enabled);

        let mut same_local = playlist.clone();
        same_local.name = "Changed alike".to_owned();
        let same = ListenBrainzSyncSummary::project(
            true,
            true,
            true,
            &same_local,
            Some(&link),
            ListenBrainzSyncLifecycle::Ready {
                remote_changed: true,
                both_same: true,
                conflicts: 0,
                manifest_only: 0,
            },
        );
        assert_eq!(same.meaning, ListenBrainzSyncMeaning::BothSame);
        assert_eq!(same.primary_action, ListenBrainzSyncUiAction::VerifyRemote);
    }

    #[test]
    fn compact_summary_contains_counts_but_no_playlist_identity_or_content() {
        let playlist = UnifiedPlaylist {
            id: "private-id".to_owned(),
            name: "private name".to_owned(),
            ..UnifiedPlaylist::default()
        };
        let summary = ListenBrainzSyncSummary::project(
            true,
            true,
            true,
            &playlist,
            Some(&linked(&playlist, ListenBrainzSyncStatus::Clean)),
            ListenBrainzSyncLifecycle::Ready {
                remote_changed: true,
                both_same: false,
                conflicts: 2,
                manifest_only: 3,
            },
        );
        let line = summary.compact_line();
        assert!(line.contains("conflicts 2"));
        assert!(line.contains("manifest-only 3"));
        assert!(!line.contains("private-id"));
        assert!(!line.contains("private name"));
        let narrow = summary.display_text(60);
        assert_eq!(narrow.lines().count(), 3);
        assert!(narrow.contains("remote changed"));
        assert!(narrow.contains("conflicts 2"));
    }

    #[test]
    fn backup_only_link_offers_initialize_as_primary_action() {
        use super::{ListenBrainzSyncLifecycle, ListenBrainzSyncMeaning, ListenBrainzSyncSummary};
        let playlist = UnifiedPlaylist {
            id: "local".to_owned(),
            name: "Mix".to_owned(),
            ..UnifiedPlaylist::default()
        };
        let link = PlaylistLink {
            unified_playlist_id: playlist.id.clone(),
            listenbrainz_playlist_id: Some("remote".to_owned()),
            listenbrainz_sync: None,
            ..PlaylistLink::default()
        };
        let summary = ListenBrainzSyncSummary::project(
            true,
            true,
            true,
            &playlist,
            Some(&link),
            ListenBrainzSyncLifecycle::Idle,
        );
        assert_eq!(summary.meaning, ListenBrainzSyncMeaning::Uninitialized);
        assert_eq!(
            summary.primary_action,
            super::ListenBrainzSyncUiAction::InitializeBase
        );
        assert_eq!(
            summary.primary_command_action(),
            Some(crate::command::Action::InitializeUnifiedPlaylistListenBrainzBase)
        );
        assert!(!summary.apply_enabled);
        assert!(summary
            .disabled_reason
            .is_some_and(|reason| reason.contains("Start sync tracking")));
    }

    #[test]
    fn sync_meanings_map_to_distinct_configurable_state_styles() {
        use super::ListenBrainzSyncMeaning;
        let theme = crate::config::Theme::default();
        assert_eq!(
            ListenBrainzSyncMeaning::Clean.status_style(&theme),
            theme.sync_clean()
        );
        assert_eq!(
            ListenBrainzSyncMeaning::BothSame.status_style(&theme),
            theme.sync_clean()
        );
        assert_eq!(
            ListenBrainzSyncMeaning::LocalChanges.status_style(&theme),
            theme.sync_changed()
        );
        assert_eq!(
            ListenBrainzSyncMeaning::RemoteChanges.status_style(&theme),
            theme.sync_changed()
        );
        assert_eq!(
            ListenBrainzSyncMeaning::BothChanged.status_style(&theme),
            theme.sync_changed()
        );
        assert_eq!(
            ListenBrainzSyncMeaning::Conflict.status_style(&theme),
            theme.sync_conflict()
        );
        assert_eq!(
            ListenBrainzSyncMeaning::CannotPlan.status_style(&theme),
            theme.sync_conflict()
        );
        assert_eq!(
            ListenBrainzSyncMeaning::OutcomeUnknown.status_style(&theme),
            theme.sync_conflict()
        );
        assert_eq!(
            ListenBrainzSyncMeaning::Failed.status_style(&theme),
            theme.sync_conflict()
        );
        assert_eq!(
            ListenBrainzSyncMeaning::Disabled.status_style(&theme),
            theme.sync_neutral()
        );
        assert_eq!(
            ListenBrainzSyncMeaning::Uninitialized.status_style(&theme),
            theme.sync_neutral()
        );
        assert_eq!(
            ListenBrainzSyncMeaning::Checking.status_style(&theme),
            theme.sync_neutral()
        );
    }

    #[test]
    fn every_busy_stage_has_the_required_explicit_label() {
        assert_eq!(
            ListenBrainzSyncLifecycle::Checking.loading_label(),
            Some("Checking remote")
        );
        assert_eq!(
            ListenBrainzSyncLifecycle::Planning.loading_label(),
            Some("Planning")
        );
        assert_eq!(
            ListenBrainzSyncLifecycle::WritingListenBrainz.loading_label(),
            Some("Writing ListenBrainz")
        );
        assert_eq!(
            ListenBrainzSyncLifecycle::Verifying.loading_label(),
            Some("Verifying")
        );
        assert_eq!(
            ListenBrainzSyncLifecycle::Recovering.loading_label(),
            Some("Recovering")
        );
    }

    #[test]
    fn workspace_lists_every_sync_operation_exactly_once() {
        use crate::command::Action;
        assert_eq!(super::LISTENBRAINZ_WORKSPACE_ACTIONS.len(), 13);
        for action in [
            Action::BackupUnifiedPlaylistToListenBrainz,
            Action::InitializeUnifiedPlaylistListenBrainzBase,
            Action::CheckUnifiedPlaylistListenBrainzSync,
            Action::RefreshUnifiedPlaylistListenBrainzSync,
            Action::RetryUnifiedPlaylistListenBrainzSync,
            Action::PreviewUnifiedPlaylistListenBrainzPush,
            Action::PreviewUnifiedPlaylistListenBrainzPull,
            Action::ReviewUnifiedPlaylistListenBrainzConflicts,
            Action::RecoverUnifiedPlaylistListenBrainzSync,
            Action::RollbackUnifiedPlaylistListenBrainzPull,
            Action::ApplyUnifiedPlaylistListenBrainzPush,
            Action::ApplyUnifiedPlaylistListenBrainzPull,
            Action::ApplyUnifiedPlaylistListenBrainzResolve,
        ] {
            assert_eq!(
                super::LISTENBRAINZ_WORKSPACE_ACTIONS
                    .iter()
                    .filter(|candidate| **candidate == action)
                    .count(),
                1,
                "workspace must list {action:?} exactly once"
            );
        }
        assert!(!super::LISTENBRAINZ_WORKSPACE_ACTIONS
            .contains(&Action::OpenUnifiedPlaylistListenBrainzSync));
    }
}
