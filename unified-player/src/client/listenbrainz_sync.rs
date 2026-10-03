use std::collections::{BTreeMap, BTreeSet};

use serde::Serialize;
use sha2::{Digest as _, Sha256};

use crate::cli::listenbrainz_manifest::{
    description_manifest_fingerprint, is_recording_mbid, is_supported_manifest_schema_version,
    parse_description_manifest, parse_media_kind, parse_provider, provider_slug,
    DescriptionManifest, DescriptionManifestProjectionStatus, DESCRIPTION_ENVELOPE_PREFIX,
};
use crate::state::{
    ListenBrainzProjectionStatus, ListenBrainzSyncBase, ListenBrainzSyncBaseEntry,
    ListenBrainzSyncState, MediaId, MediaKind, PlaylistEntryId, UnifiedPlaylist,
    UnifiedPlaylistItem,
};

const PLAN_SCHEMA_VERSION: u8 = 1;
const MUSICBRAINZ_RECORDING_URI_PREFIX: &str = "https://musicbrainz.org/recording/";

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub(crate) struct SyncEntry {
    pub(crate) occurrence: u64,
    pub(crate) provider: String,
    pub(crate) kind: String,
    pub(crate) raw_id: String,
}

impl SyncEntry {
    fn from_item(item: &UnifiedPlaylistItem) -> Self {
        Self {
            occurrence: item.entry_id.0,
            provider: provider_slug(item.media_id.provider).to_owned(),
            kind: media_kind_slug(item.media_id.kind).to_owned(),
            raw_id: item.media_id.raw_id.clone(),
        }
    }

    fn from_manifest(entry: &crate::cli::listenbrainz_manifest::DescriptionManifestEntry) -> Self {
        Self {
            occurrence: entry.occurrence,
            provider: entry.provider.clone(),
            kind: entry.kind.clone(),
            raw_id: entry.id.clone(),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct SyncSnapshot {
    playlist_id: String,
    name: String,
    entries: Vec<SyncEntry>,
    fingerprint: String,
}

impl SyncSnapshot {
    pub(crate) fn from_playlist(playlist: &UnifiedPlaylist) -> Self {
        Self {
            playlist_id: playlist.id.clone(),
            name: playlist.name.clone(),
            entries: playlist.items.iter().map(SyncEntry::from_item).collect(),
            fingerprint: playlist.snapshot_hash(),
        }
    }

    fn from_manifest(manifest: &DescriptionManifest, remote_name: &str) -> Result<Self, ()> {
        let entries = manifest
            .entries
            .iter()
            .map(SyncEntry::from_manifest)
            .collect::<Vec<_>>();
        let playlist = UnifiedPlaylist {
            id: manifest.playlist_id.clone(),
            name: remote_name.to_owned(),
            items: entries
                .iter()
                .map(|entry| {
                    Ok(UnifiedPlaylistItem {
                        entry_id: PlaylistEntryId(entry.occurrence),
                        media_id: MediaId {
                            provider: parse_provider(&entry.provider).ok_or(())?,
                            kind: parse_media_kind(&entry.kind).ok_or(())?,
                            raw_id: entry.raw_id.clone(),
                        },
                        ..UnifiedPlaylistItem::default()
                    })
                })
                .collect::<Result<Vec<_>, ()>>()?,
            ..UnifiedPlaylist::default()
        };
        let fingerprint = playlist.snapshot_hash();
        if fingerprint != manifest.snapshot_hash {
            return Err(());
        }
        Ok(Self {
            playlist_id: manifest.playlist_id.clone(),
            name: remote_name.to_owned(),
            entries,
            fingerprint,
        })
    }

    fn from_sync_base(base: &ListenBrainzSyncBase) -> Result<Self, ()> {
        base.validate().map_err(|_| ())?;
        let playlist = UnifiedPlaylist {
            id: base.unified_playlist_id.clone(),
            name: base.playlist_name.clone(),
            items: base
                .entries
                .iter()
                .map(|entry| UnifiedPlaylistItem {
                    entry_id: entry.occurrence,
                    media_id: entry.media_id.clone(),
                    ..UnifiedPlaylistItem::default()
                })
                .collect(),
            ..UnifiedPlaylist::default()
        };
        (playlist.snapshot_hash() == base.local_snapshot_hash)
            .then(|| Self::from_playlist(&playlist))
            .ok_or(())
    }

    #[cfg(test)]
    fn testing(playlist_id: &str, name: &str, entries: Vec<SyncEntry>) -> Self {
        let items = entries
            .iter()
            .map(|entry| UnifiedPlaylistItem {
                entry_id: PlaylistEntryId(entry.occurrence),
                media_id: MediaId {
                    provider: parse_provider(&entry.provider).unwrap(),
                    kind: parse_media_kind(&entry.kind).unwrap(),
                    raw_id: entry.raw_id.clone(),
                },
                ..UnifiedPlaylistItem::default()
            })
            .collect();
        Self::from_playlist(&UnifiedPlaylist {
            id: playlist_id.to_owned(),
            name: name.to_owned(),
            items,
            ..UnifiedPlaylist::default()
        })
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum PlanStatus {
    Ready,
    CannotPlan,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum BaseSource {
    ExplicitSnapshot,
    RemoteManifestAnchor,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum CannotPlanReason {
    LosslessRemoteManifestMissing,
    InvalidRemoteManifest,
    UnsupportedManifestSchema,
    ManifestSnapshotMismatch,
    PlaylistIdentityMismatch,
    StaleRemoteFingerprint,
    StaleBase,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum ChangeClassification {
    LocalOnly,
    RemoteOnly,
    SameChange,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum ChangeKind {
    Added,
    Removed,
    IdentityChanged,
    PlaylistRenamed,
    Reordered,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub(crate) struct SyncChange {
    pub(crate) classification: ChangeClassification,
    pub(crate) kind: ChangeKind,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) occurrence: Option<u64>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum ConflictKind {
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

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum PullPreviewClassification {
    NoChange,
    LocalOnly,
    RemoteOnly,
    SameChange,
    BothNonConflicting,
    Conflict,
    UnresolvedEntry,
    UnlinkedRemoteRow,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub(crate) struct ListenBrainzPullPreview {
    pub(crate) schema_version: u8,
    pub(crate) classification: PullPreviewClassification,
    pub(crate) base_occurrences: usize,
    pub(crate) local_occurrences: usize,
    pub(crate) remote_occurrences: usize,
    pub(crate) remote_native_rows: usize,
    pub(crate) remote_unresolved: usize,
    pub(crate) affected_occurrences: Vec<u64>,
    pub(crate) plan: ListenBrainzSyncPlan,
    pub(crate) writes_performed: bool,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub(crate) struct SyncConflict {
    pub(crate) kind: ConflictKind,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) occurrence: Option<u64>,
    pub(crate) message: String,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum PlanWarningKind {
    RemoteManifestUsedAsInitialBase,
    ProjectionCoverageUnverifiable,
    ClientTrackMetadataUnsupported,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum OccurrenceIdentityContract {
    ManifestOnly,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub(crate) struct PlanWarning {
    pub(crate) kind: PlanWarningKind,
    pub(crate) message: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub(crate) struct ListenBrainzSyncPlan {
    pub(crate) schema_version: u8,
    pub(crate) status: PlanStatus,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) cannot_plan_reason: Option<CannotPlanReason>,
    pub(crate) remote_playlist_id: String,
    pub(crate) unified_playlist_id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) base_source: Option<BaseSource>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) base_fingerprint: Option<String>,
    pub(crate) local_fingerprint: String,
    pub(crate) occurrence_identity_contract: OccurrenceIdentityContract,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) remote_manifest_fingerprint: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) remote_fingerprint: Option<String>,
    pub(crate) changes: Vec<SyncChange>,
    pub(crate) conflicts: Vec<SyncConflict>,
    pub(crate) warnings: Vec<PlanWarning>,
    pub(crate) writes_performed: bool,
}

impl ListenBrainzSyncPlan {
    fn cannot_plan(
        remote_playlist_id: &str,
        local: &SyncSnapshot,
        reason: CannotPlanReason,
        remote_fingerprint: Option<String>,
        conflict: Option<SyncConflict>,
    ) -> Self {
        Self {
            schema_version: PLAN_SCHEMA_VERSION,
            status: PlanStatus::CannotPlan,
            cannot_plan_reason: Some(reason),
            remote_playlist_id: remote_playlist_id.to_owned(),
            unified_playlist_id: local.playlist_id.clone(),
            base_source: None,
            base_fingerprint: None,
            local_fingerprint: local.fingerprint.clone(),
            occurrence_identity_contract: OccurrenceIdentityContract::ManifestOnly,
            remote_manifest_fingerprint: None,
            remote_fingerprint,
            changes: Vec::new(),
            conflicts: conflict.into_iter().collect(),
            warnings: Vec::new(),
            writes_performed: false,
        }
    }
}

#[derive(Debug, Serialize)]
struct RemoteFingerprint<'a> {
    playlist_id: &'a str,
    #[serde(skip_serializing_if = "Option::is_none")]
    last_modified_at: Option<&'a str>,
    manifest_fingerprint: &'a str,
    tracks: &'a [ProjectionRow],
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
struct ProjectionRow {
    position: usize,
    title: Option<String>,
    creator: Option<String>,
    duration: Option<u64>,
    identifiers: Vec<String>,
}

pub(crate) fn build_remote_anchored_plan(
    remote_playlist_id: &str,
    local_playlist: &UnifiedPlaylist,
    remote_value: &serde_json::Value,
    expected_remote_fingerprint: Option<&str>,
) -> ListenBrainzSyncPlan {
    let local = SyncSnapshot::from_playlist(local_playlist);
    let playlist = remote_value.get("playlist").unwrap_or(remote_value);
    let remote_name = playlist
        .get("title")
        .and_then(serde_json::Value::as_str)
        .unwrap_or("ListenBrainz playlist");
    let Some(annotation) = playlist
        .get("annotation")
        .and_then(serde_json::Value::as_str)
    else {
        return ListenBrainzSyncPlan::cannot_plan(
            remote_playlist_id,
            &local,
            CannotPlanReason::LosslessRemoteManifestMissing,
            None,
            None,
        );
    };
    let Some(manifest_json) = annotation.strip_prefix(DESCRIPTION_ENVELOPE_PREFIX) else {
        return ListenBrainzSyncPlan::cannot_plan(
            remote_playlist_id,
            &local,
            CannotPlanReason::LosslessRemoteManifestMissing,
            None,
            None,
        );
    };
    let Ok(raw_manifest) = serde_json::from_str::<serde_json::Value>(manifest_json) else {
        return ListenBrainzSyncPlan::cannot_plan(
            remote_playlist_id,
            &local,
            CannotPlanReason::InvalidRemoteManifest,
            None,
            None,
        );
    };
    if !raw_manifest
        .get("schema_version")
        .and_then(serde_json::Value::as_u64)
        .is_some_and(is_supported_manifest_schema_version)
    {
        return ListenBrainzSyncPlan::cannot_plan(
            remote_playlist_id,
            &local,
            CannotPlanReason::UnsupportedManifestSchema,
            None,
            None,
        );
    }
    let Ok(manifest) = parse_description_manifest(annotation) else {
        return ListenBrainzSyncPlan::cannot_plan(
            remote_playlist_id,
            &local,
            CannotPlanReason::InvalidRemoteManifest,
            None,
            None,
        );
    };
    let Ok(base) = SyncSnapshot::from_manifest(&manifest, remote_name) else {
        return ListenBrainzSyncPlan::cannot_plan(
            remote_playlist_id,
            &local,
            CannotPlanReason::ManifestSnapshotMismatch,
            None,
            None,
        );
    };
    let manifest_fingerprint = description_manifest_fingerprint(&manifest)
        .expect("a validated manifest always serializes");
    if base.playlist_id != local.playlist_id {
        return ListenBrainzSyncPlan::cannot_plan(
            remote_playlist_id,
            &local,
            CannotPlanReason::PlaylistIdentityMismatch,
            None,
            None,
        );
    }

    let rows = normalize_projection_rows(playlist);
    let last_modified_at = playlist
        .pointer("/extension/https:~1~1musicbrainz.org~1doc~1jspf#playlist/last_modified_at")
        .and_then(serde_json::Value::as_str);
    let remote_fingerprint = sha256_json(&RemoteFingerprint {
        playlist_id: remote_playlist_id,
        last_modified_at,
        manifest_fingerprint: &manifest_fingerprint,
        tracks: &rows,
    });
    if expected_remote_fingerprint.is_some_and(|expected| expected != remote_fingerprint) {
        return ListenBrainzSyncPlan::cannot_plan(
            remote_playlist_id,
            &local,
            CannotPlanReason::StaleRemoteFingerprint,
            Some(remote_fingerprint),
            Some(SyncConflict {
                kind: ConflictKind::StaleBase,
                occurrence: None,
                message: "remote fingerprint differs from the caller's expected base".to_owned(),
            }),
        );
    }

    let (projection_conflicts, projection_warnings) = inspect_projection(&manifest, &rows);
    let mut plan = plan_snapshots(
        remote_playlist_id,
        &base,
        &local,
        &base,
        projection_conflicts,
    );
    plan.base_source = Some(BaseSource::RemoteManifestAnchor);
    plan.remote_manifest_fingerprint = Some(manifest_fingerprint);
    plan.remote_fingerprint = Some(remote_fingerprint);
    plan.warnings.push(PlanWarning {
        kind: PlanWarningKind::RemoteManifestUsedAsInitialBase,
        message: "the unchanged remote manifest is the initial base anchor; apply support must persist a separate last-synced base before updating it".to_owned(),
    });
    plan.warnings.extend(projection_warnings);
    plan
}

pub(crate) fn verified_sync_state_from_remote(
    remote_playlist_id: &str,
    local_playlist: &UnifiedPlaylist,
    remote_value: &serde_json::Value,
    expected_remote_fingerprint: Option<&str>,
    verified_at: u64,
) -> anyhow::Result<ListenBrainzSyncState> {
    let plan = build_remote_anchored_plan(
        remote_playlist_id,
        local_playlist,
        remote_value,
        expected_remote_fingerprint,
    );
    anyhow::ensure!(
        plan.status == PlanStatus::Ready && plan.conflicts.is_empty(),
        "remote ListenBrainz state is not a verified sync-base candidate"
    );
    let playlist = remote_value.get("playlist").unwrap_or(remote_value);
    let annotation = playlist
        .get("annotation")
        .and_then(serde_json::Value::as_str)
        .ok_or_else(|| anyhow::anyhow!("verified remote playlist has no manifest"))?;
    let manifest = parse_description_manifest(annotation)?;
    let entries = manifest
        .entries
        .iter()
        .map(|entry| {
            let projection_status = match entry.projection.as_ref().map(|value| value.status) {
                Some(DescriptionManifestProjectionStatus::Ineligible) => {
                    ListenBrainzProjectionStatus::Ineligible
                }
                Some(DescriptionManifestProjectionStatus::Resolved) => {
                    ListenBrainzProjectionStatus::Resolved
                }
                Some(DescriptionManifestProjectionStatus::Unresolved) | None => {
                    ListenBrainzProjectionStatus::Unresolved
                }
            };
            Ok(ListenBrainzSyncBaseEntry {
                occurrence: PlaylistEntryId(entry.occurrence),
                media_id: MediaId {
                    provider: parse_provider(&entry.provider).ok_or_else(|| {
                        anyhow::anyhow!("verified manifest has an unsupported provider")
                    })?,
                    kind: parse_media_kind(&entry.kind).ok_or_else(|| {
                        anyhow::anyhow!("verified manifest has an unsupported media kind")
                    })?,
                    raw_id: entry.id.clone(),
                },
                projection_status,
                recording_mbid: entry
                    .projection
                    .as_ref()
                    .and_then(|projection| projection.recording_mbid.clone()),
            })
        })
        .collect::<anyhow::Result<Vec<_>>>()?;
    ListenBrainzSyncState::verified(ListenBrainzSyncBase {
        manifest_schema_version: manifest.schema_version,
        unified_playlist_id: manifest.playlist_id,
        playlist_name: playlist
            .get("title")
            .and_then(serde_json::Value::as_str)
            .unwrap_or("ListenBrainz playlist")
            .to_owned(),
        local_snapshot_hash: manifest.snapshot_hash,
        canonical_manifest_hash: plan
            .remote_manifest_fingerprint
            .ok_or_else(|| anyhow::anyhow!("verified plan has no manifest fingerprint"))?,
        remote_fingerprint: plan
            .remote_fingerprint
            .ok_or_else(|| anyhow::anyhow!("verified plan has no remote fingerprint"))?,
        entries,
        verified_at,
    })
}

pub(crate) fn build_persisted_base_pull_preview(
    remote_playlist_id: &str,
    base: &ListenBrainzSyncBase,
    local_playlist: &UnifiedPlaylist,
    remote_value: &serde_json::Value,
) -> ListenBrainzPullPreview {
    let local = SyncSnapshot::from_playlist(local_playlist);
    let cannot_preview = |reason, conflict| {
        pull_preview_from_plan(
            ListenBrainzSyncPlan::cannot_plan(remote_playlist_id, &local, reason, None, conflict),
            base.entries.len(),
            local.entries.len(),
            0,
            0,
            0,
        )
    };
    let Ok(base_snapshot) = SyncSnapshot::from_sync_base(base) else {
        return cannot_preview(
            CannotPlanReason::StaleBase,
            Some(SyncConflict {
                kind: ConflictKind::StaleBase,
                occurrence: None,
                message: "persisted sync base does not match its declared snapshot".to_owned(),
            }),
        );
    };
    if base_snapshot.playlist_id != local.playlist_id {
        return cannot_preview(
            CannotPlanReason::PlaylistIdentityMismatch,
            Some(SyncConflict {
                kind: ConflictKind::StaleBase,
                occurrence: None,
                message: "persisted sync base belongs to another Unified playlist".to_owned(),
            }),
        );
    }
    let playlist = remote_value.get("playlist").unwrap_or(remote_value);
    let Some(annotation) = playlist
        .get("annotation")
        .and_then(serde_json::Value::as_str)
    else {
        return cannot_preview(
            CannotPlanReason::LosslessRemoteManifestMissing,
            Some(SyncConflict {
                kind: ConflictKind::Schema,
                occurrence: None,
                message: "remote playlist has no lossless manifest".to_owned(),
            }),
        );
    };
    let Some(manifest_json) = annotation.strip_prefix(DESCRIPTION_ENVELOPE_PREFIX) else {
        return cannot_preview(
            CannotPlanReason::LosslessRemoteManifestMissing,
            Some(SyncConflict {
                kind: ConflictKind::Schema,
                occurrence: None,
                message: "remote playlist has no compatible manifest envelope".to_owned(),
            }),
        );
    };
    let Ok(raw_manifest) = serde_json::from_str::<serde_json::Value>(manifest_json) else {
        return cannot_preview(
            CannotPlanReason::InvalidRemoteManifest,
            Some(SyncConflict {
                kind: ConflictKind::Schema,
                occurrence: None,
                message: "remote manifest is invalid".to_owned(),
            }),
        );
    };
    if !raw_manifest
        .get("schema_version")
        .and_then(serde_json::Value::as_u64)
        .is_some_and(is_supported_manifest_schema_version)
    {
        return cannot_preview(
            CannotPlanReason::UnsupportedManifestSchema,
            Some(SyncConflict {
                kind: ConflictKind::Schema,
                occurrence: None,
                message: "remote manifest schema is unsupported".to_owned(),
            }),
        );
    }
    let Ok(manifest) = parse_description_manifest(annotation) else {
        return cannot_preview(
            CannotPlanReason::InvalidRemoteManifest,
            Some(SyncConflict {
                kind: ConflictKind::Schema,
                occurrence: None,
                message: "remote manifest failed validation".to_owned(),
            }),
        );
    };
    let remote_name = playlist
        .get("title")
        .and_then(serde_json::Value::as_str)
        .unwrap_or("ListenBrainz playlist");
    let Ok(remote) = SyncSnapshot::from_manifest(&manifest, remote_name) else {
        return cannot_preview(
            CannotPlanReason::ManifestSnapshotMismatch,
            Some(SyncConflict {
                kind: ConflictKind::StaleBase,
                occurrence: None,
                message: "remote manifest snapshot hash is inconsistent".to_owned(),
            }),
        );
    };
    if remote.playlist_id != local.playlist_id {
        return cannot_preview(
            CannotPlanReason::PlaylistIdentityMismatch,
            Some(SyncConflict {
                kind: ConflictKind::StaleBase,
                occurrence: None,
                message: "remote manifest belongs to another Unified playlist".to_owned(),
            }),
        );
    }

    let rows = normalize_projection_rows(playlist);
    let (projection_conflicts, projection_warnings) = inspect_projection(&manifest, &rows);
    let unresolved = manifest
        .entries
        .iter()
        .filter(|entry| {
            entry.projection.as_ref().is_some_and(|projection| {
                projection.status == DescriptionManifestProjectionStatus::Unresolved
            })
        })
        .count();
    let mut plan = plan_snapshots(
        remote_playlist_id,
        &base_snapshot,
        &local,
        &remote,
        projection_conflicts,
    );
    let manifest_fingerprint = description_manifest_fingerprint(&manifest)
        .expect("a validated manifest always serializes");
    let last_modified_at = playlist
        .pointer("/extension/https:~1~1musicbrainz.org~1doc~1jspf#playlist/last_modified_at")
        .and_then(serde_json::Value::as_str);
    plan.remote_manifest_fingerprint = Some(manifest_fingerprint.clone());
    plan.remote_fingerprint = Some(sha256_json(&RemoteFingerprint {
        playlist_id: remote_playlist_id,
        last_modified_at,
        manifest_fingerprint: &manifest_fingerprint,
        tracks: &rows,
    }));
    plan.warnings.extend(projection_warnings);
    pull_preview_from_plan(
        plan,
        base_snapshot.entries.len(),
        local.entries.len(),
        remote.entries.len(),
        rows.len(),
        unresolved,
    )
}

fn pull_preview_from_plan(
    plan: ListenBrainzSyncPlan,
    base_occurrences: usize,
    local_occurrences: usize,
    remote_occurrences: usize,
    remote_native_rows: usize,
    remote_unresolved: usize,
) -> ListenBrainzPullPreview {
    let mut affected_occurrences = plan
        .changes
        .iter()
        .filter_map(|change| change.occurrence)
        .chain(
            plan.conflicts
                .iter()
                .filter_map(|conflict| conflict.occurrence),
        )
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect::<Vec<_>>();
    affected_occurrences.sort_unstable();
    let classification = if plan
        .conflicts
        .iter()
        .any(|conflict| conflict.kind == ConflictKind::UnlinkedRemoteRow)
    {
        PullPreviewClassification::UnlinkedRemoteRow
    } else if plan.status == PlanStatus::CannotPlan || !plan.conflicts.is_empty() {
        PullPreviewClassification::Conflict
    } else if remote_unresolved > 0 {
        PullPreviewClassification::UnresolvedEntry
    } else if plan.changes.is_empty() {
        PullPreviewClassification::NoChange
    } else {
        let local = plan
            .changes
            .iter()
            .any(|change| change.classification == ChangeClassification::LocalOnly);
        let remote = plan
            .changes
            .iter()
            .any(|change| change.classification == ChangeClassification::RemoteOnly);
        let same = plan
            .changes
            .iter()
            .any(|change| change.classification == ChangeClassification::SameChange);
        match (local, remote, same) {
            (true, false, false) => PullPreviewClassification::LocalOnly,
            (false, true, false) => PullPreviewClassification::RemoteOnly,
            (false, false, true) => PullPreviewClassification::SameChange,
            _ => PullPreviewClassification::BothNonConflicting,
        }
    };
    ListenBrainzPullPreview {
        schema_version: 1,
        classification,
        base_occurrences,
        local_occurrences,
        remote_occurrences,
        remote_native_rows,
        remote_unresolved,
        affected_occurrences,
        plan,
        writes_performed: false,
    }
}

fn plan_snapshots(
    remote_playlist_id: &str,
    base: &SyncSnapshot,
    local: &SyncSnapshot,
    remote: &SyncSnapshot,
    mut conflicts: Vec<SyncConflict>,
) -> ListenBrainzSyncPlan {
    let mut changes = Vec::new();
    let base_by_id = entries_by_occurrence(&base.entries);
    let local_by_id = entries_by_occurrence(&local.entries);
    let remote_by_id = entries_by_occurrence(&remote.entries);
    let occurrences = base_by_id
        .keys()
        .chain(local_by_id.keys())
        .chain(remote_by_id.keys())
        .copied()
        .collect::<BTreeSet<_>>();

    for occurrence in occurrences {
        let base_entry = base_by_id.get(&occurrence).copied();
        let local_entry = local_by_id.get(&occurrence).copied();
        let remote_entry = remote_by_id.get(&occurrence).copied();
        let local_changed = local_entry != base_entry;
        let remote_changed = remote_entry != base_entry;
        match (local_changed, remote_changed) {
            (false, false) => {}
            (true, false) => changes.push(entry_change(
                ChangeClassification::LocalOnly,
                occurrence,
                base_entry,
                local_entry,
            )),
            (false, true) => changes.push(entry_change(
                ChangeClassification::RemoteOnly,
                occurrence,
                base_entry,
                remote_entry,
            )),
            (true, true) if local_entry == remote_entry => changes.push(entry_change(
                ChangeClassification::SameChange,
                occurrence,
                base_entry,
                local_entry,
            )),
            (true, true) => conflicts.push(SyncConflict {
                kind: entry_conflict_kind(base_entry, local_entry, remote_entry),
                occurrence: Some(occurrence),
                message: format!(
                    "occurrence {occurrence} changed incompatibly in local and remote state"
                ),
            }),
        }
    }

    classify_scalar_change(
        base.name != local.name,
        base.name != remote.name,
        local.name == remote.name,
        ChangeKind::PlaylistRenamed,
        &mut changes,
        &mut conflicts,
    );

    let local_order = retained_base_order(base, local);
    let remote_order = retained_base_order(base, remote);
    let base_for_local = base_order_for_side(base, local);
    let base_for_remote = base_order_for_side(base, remote);
    let local_reordered = local_order != base_for_local;
    let remote_reordered = remote_order != base_for_remote;
    match (local_reordered, remote_reordered) {
        (false, false) => {}
        (true, false) => changes.push(SyncChange {
            classification: ChangeClassification::LocalOnly,
            kind: ChangeKind::Reordered,
            occurrence: None,
        }),
        (false, true) => changes.push(SyncChange {
            classification: ChangeClassification::RemoteOnly,
            kind: ChangeKind::Reordered,
            occurrence: None,
        }),
        (true, true) if local_order == remote_order => changes.push(SyncChange {
            classification: ChangeClassification::SameChange,
            kind: ChangeKind::Reordered,
            occurrence: None,
        }),
        (true, true) => conflicts.push(SyncConflict {
            kind: ConflictKind::Reorder,
            occurrence: None,
            message: "local and remote occurrence order changed differently".to_owned(),
        }),
    }

    ListenBrainzSyncPlan {
        schema_version: PLAN_SCHEMA_VERSION,
        status: PlanStatus::Ready,
        cannot_plan_reason: None,
        remote_playlist_id: remote_playlist_id.to_owned(),
        unified_playlist_id: local.playlist_id.clone(),
        base_source: Some(BaseSource::ExplicitSnapshot),
        base_fingerprint: Some(base.fingerprint.clone()),
        local_fingerprint: local.fingerprint.clone(),
        occurrence_identity_contract: OccurrenceIdentityContract::ManifestOnly,
        remote_manifest_fingerprint: None,
        remote_fingerprint: None,
        changes,
        conflicts,
        warnings: Vec::new(),
        writes_performed: false,
    }
}

fn classify_scalar_change(
    local_changed: bool,
    remote_changed: bool,
    sides_match: bool,
    kind: ChangeKind,
    changes: &mut Vec<SyncChange>,
    conflicts: &mut Vec<SyncConflict>,
) {
    let classification = match (local_changed, remote_changed) {
        (false, false) => return,
        (true, false) => ChangeClassification::LocalOnly,
        (false, true) => ChangeClassification::RemoteOnly,
        (true, true) if sides_match => ChangeClassification::SameChange,
        (true, true) => {
            conflicts.push(SyncConflict {
                kind: ConflictKind::Mapping,
                occurrence: None,
                message: "playlist name changed differently in local and remote state".to_owned(),
            });
            return;
        }
    };
    changes.push(SyncChange {
        classification,
        kind,
        occurrence: None,
    });
}

fn entry_change(
    classification: ChangeClassification,
    occurrence: u64,
    base: Option<&SyncEntry>,
    side: Option<&SyncEntry>,
) -> SyncChange {
    let kind = match (base, side) {
        (None, Some(_)) => ChangeKind::Added,
        (Some(_), None) => ChangeKind::Removed,
        (Some(_), Some(_)) => ChangeKind::IdentityChanged,
        (None, None) => unreachable!("unchanged missing occurrence cannot produce a change"),
    };
    SyncChange {
        classification,
        kind,
        occurrence: Some(occurrence),
    }
}

fn entry_conflict_kind(
    base: Option<&SyncEntry>,
    local: Option<&SyncEntry>,
    remote: Option<&SyncEntry>,
) -> ConflictKind {
    match (base, local, remote) {
        (None, Some(_), Some(_)) => ConflictKind::AddAdd,
        (Some(_), None, Some(_)) | (Some(_), Some(_), None) => ConflictKind::DeleteEdit,
        _ => ConflictKind::Mapping,
    }
}

fn entries_by_occurrence(entries: &[SyncEntry]) -> BTreeMap<u64, &SyncEntry> {
    entries
        .iter()
        .map(|entry| (entry.occurrence, entry))
        .collect()
}

fn retained_base_order(base: &SyncSnapshot, side: &SyncSnapshot) -> Vec<u64> {
    let base_ids = base
        .entries
        .iter()
        .map(|entry| entry.occurrence)
        .collect::<BTreeSet<_>>();
    side.entries
        .iter()
        .map(|entry| entry.occurrence)
        .filter(|occurrence| base_ids.contains(occurrence))
        .collect()
}

fn base_order_for_side(base: &SyncSnapshot, side: &SyncSnapshot) -> Vec<u64> {
    let side_ids = side
        .entries
        .iter()
        .map(|entry| entry.occurrence)
        .collect::<BTreeSet<_>>();
    base.entries
        .iter()
        .map(|entry| entry.occurrence)
        .filter(|occurrence| side_ids.contains(occurrence))
        .collect()
}

fn normalize_projection_rows(playlist: &serde_json::Value) -> Vec<ProjectionRow> {
    playlist
        .get("track")
        .and_then(serde_json::Value::as_array)
        .into_iter()
        .flatten()
        .enumerate()
        .map(|(position, track)| ProjectionRow {
            position,
            title: track
                .get("title")
                .and_then(serde_json::Value::as_str)
                .map(str::to_owned),
            creator: track
                .get("creator")
                .and_then(serde_json::Value::as_str)
                .map(str::to_owned),
            duration: track.get("duration").and_then(serde_json::Value::as_u64),
            identifiers: track
                .get("identifier")
                .and_then(serde_json::Value::as_array)
                .into_iter()
                .flatten()
                .filter_map(serde_json::Value::as_str)
                .map(str::to_owned)
                .collect(),
        })
        .collect()
}

fn inspect_projection(
    manifest: &DescriptionManifest,
    rows: &[ProjectionRow],
) -> (Vec<SyncConflict>, Vec<PlanWarning>) {
    if manifest.schema_version == 2 {
        return inspect_v2_projection(manifest, rows);
    }
    let coverage_warning = PlanWarning {
        kind: PlanWarningKind::ProjectionCoverageUnverifiable,
        message: "manifest schema v1 does not record MusicBrainz projection eligibility; absent JSPF rows cannot be classified as deletions".to_owned(),
    };
    if rows.is_empty() {
        return (Vec::new(), vec![coverage_warning]);
    }
    let conflicts = rows
        .iter()
        .map(|row| SyncConflict {
            kind: ConflictKind::UnlinkedRemoteRow,
            occurrence: None,
            message: format!(
                "remote JSPF row {} has no trusted unified-player occurrence identity",
                row.position
            ),
        })
        .collect();
    let metadata_warning = PlanWarning {
        kind: PlanWarningKind::ClientTrackMetadataUnsupported,
        message: "ListenBrainz user playlist mutations do not preserve client track additional_metadata; occurrence identity remains authoritative only in the annotation manifest".to_owned(),
    };
    (conflicts, vec![coverage_warning, metadata_warning])
}

fn inspect_v2_projection(
    manifest: &DescriptionManifest,
    rows: &[ProjectionRow],
) -> (Vec<SyncConflict>, Vec<PlanWarning>) {
    let expected = manifest
        .entries
        .iter()
        .filter_map(|entry| {
            let projection = entry.projection.as_ref()?;
            (projection.status == DescriptionManifestProjectionStatus::Resolved)
                .then(|| projection.recording_mbid.clone())
                .flatten()
        })
        .collect::<Vec<_>>();
    let actual = rows
        .iter()
        .map(projection_row_recording_mbid)
        .collect::<Vec<_>>();
    if actual.iter().all(Option::is_some)
        && actual
            .iter()
            .map(|value| value.as_deref().unwrap_or_default())
            .eq(expected.iter().map(String::as_str))
    {
        return (Vec::new(), Vec::new());
    }

    let mut remaining = expected.iter().fold(BTreeMap::new(), |mut counts, mbid| {
        *counts.entry(mbid.as_str()).or_insert(0_usize) += 1;
        counts
    });
    let mut conflicts = vec![SyncConflict {
        kind: ConflictKind::ManifestProjectionDrift,
        occurrence: None,
        message: "remote JSPF recording sequence differs from manifest v2 projection".to_owned(),
    }];
    let expected_counts = expected.iter().fold(BTreeMap::new(), |mut counts, mbid| {
        *counts.entry(mbid.as_str()).or_insert(0_usize) += 1;
        counts
    });
    let actual_counts = actual
        .iter()
        .flatten()
        .fold(BTreeMap::new(), |mut counts, mbid| {
            *counts.entry(mbid.as_str()).or_insert(0_usize) += 1;
            counts
        });
    if expected_counts
        .keys()
        .chain(actual_counts.keys())
        .copied()
        .collect::<BTreeSet<_>>()
        .into_iter()
        .any(|mbid| {
            let expected = expected_counts.get(mbid).copied().unwrap_or_default();
            let actual = actual_counts.get(mbid).copied().unwrap_or_default();
            expected != actual && (expected > 1 || actual > 1)
        })
    {
        conflicts.push(SyncConflict {
            kind: ConflictKind::DuplicateAmbiguity,
            occurrence: None,
            message: "remote JSPF duplicate counts differ from the manifest projection".to_owned(),
        });
    }
    for (position, mbid) in actual.iter().enumerate() {
        let linked = mbid.as_deref().is_some_and(|mbid| {
            remaining.get_mut(mbid).is_some_and(|count| {
                if *count == 0 {
                    false
                } else {
                    *count -= 1;
                    true
                }
            })
        });
        if !linked {
            conflicts.push(SyncConflict {
                kind: ConflictKind::UnlinkedRemoteRow,
                occurrence: None,
                message: format!(
                    "remote JSPF row {position} is absent from the manifest v2 projection"
                ),
            });
        }
    }
    (conflicts, Vec::new())
}

fn projection_row_recording_mbid(row: &ProjectionRow) -> Option<String> {
    row.identifiers.iter().find_map(|identifier| {
        let mbid = identifier.strip_prefix(MUSICBRAINZ_RECORDING_URI_PREFIX)?;
        let normalized = mbid.to_ascii_lowercase();
        is_recording_mbid(&normalized).then_some(normalized)
    })
}

const fn media_kind_slug(kind: MediaKind) -> &'static str {
    match kind {
        MediaKind::Track => "track",
        MediaKind::Episode => "episode",
        MediaKind::Video => "video",
    }
}

fn sha256_json(value: &impl Serialize) -> String {
    use std::fmt::Write as _;

    Sha256::digest(serde_json::to_vec(value).unwrap_or_default())
        .iter()
        .fold(String::new(), |mut hex, byte| {
            write!(hex, "{byte:02x}").expect("writing to a String cannot fail");
            hex
        })
}

#[cfg(test)]
mod tests {
    use super::{
        build_persisted_base_pull_preview, build_remote_anchored_plan, plan_snapshots,
        verified_sync_state_from_remote, ChangeClassification, ChangeKind, ConflictKind,
        PlanStatus, PullPreviewClassification, SyncEntry, SyncSnapshot,
        MUSICBRAINZ_RECORDING_URI_PREFIX,
    };
    use crate::cli::listenbrainz_manifest::{
        description_envelope, description_manifest, description_manifest_fingerprint,
        description_manifest_with_recording_mbids, DESCRIPTION_ENVELOPE_PREFIX,
    };
    use crate::state::{
        MediaId, MediaKind, PlaylistEntryId, Provider, UnifiedPlaylist, UnifiedPlaylistItem,
    };
    use std::collections::BTreeMap;

    fn entry(occurrence: u64, raw_id: &str) -> SyncEntry {
        SyncEntry {
            occurrence,
            provider: "spotify".to_owned(),
            kind: "track".to_owned(),
            raw_id: raw_id.to_owned(),
        }
    }

    fn playlist(entries: &[(u64, &str)]) -> UnifiedPlaylist {
        UnifiedPlaylist {
            id: "local".to_owned(),
            name: "Mix".to_owned(),
            items: entries
                .iter()
                .map(|(occurrence, raw_id)| UnifiedPlaylistItem {
                    entry_id: PlaylistEntryId(*occurrence),
                    media_id: MediaId {
                        provider: Provider::Spotify,
                        kind: MediaKind::Track,
                        raw_id: (*raw_id).to_owned(),
                    },
                    ..UnifiedPlaylistItem::default()
                })
                .collect(),
            ..UnifiedPlaylist::default()
        }
    }

    fn remote_value(playlist: &UnifiedPlaylist, tracks: serde_json::Value) -> serde_json::Value {
        serde_json::json!({
            "playlist": {
                "title": playlist.name,
                "annotation": description_envelope(playlist).unwrap(),
                "track": tracks,
            }
        })
    }

    fn v1_remote_value(playlist: &UnifiedPlaylist, tracks: serde_json::Value) -> serde_json::Value {
        let mut manifest = serde_json::to_value(description_manifest(playlist).unwrap()).unwrap();
        manifest["schema_version"] = serde_json::json!(1);
        for entry in manifest["entries"].as_array_mut().unwrap() {
            entry.as_object_mut().unwrap().remove("projection");
        }
        serde_json::json!({
            "playlist": {
                "title": playlist.name,
                "annotation": format!(
                    "{DESCRIPTION_ENVELOPE_PREFIX}{}",
                    serde_json::to_string(&manifest).unwrap()
                ),
                "track": tracks,
            }
        })
    }

    fn remote_value_with_recording_mbids(
        playlist: &UnifiedPlaylist,
        recording_mbids: &BTreeMap<PlaylistEntryId, String>,
        tracks: serde_json::Value,
    ) -> serde_json::Value {
        let manifest =
            description_manifest_with_recording_mbids(playlist, recording_mbids).unwrap();
        serde_json::json!({
            "playlist": {
                "title": playlist.name,
                "annotation": format!(
                    "{DESCRIPTION_ENVELOPE_PREFIX}{}",
                    serde_json::to_string(&manifest).unwrap()
                ),
                "track": tracks,
            }
        })
    }

    #[test]
    fn remote_manifest_is_initial_base_and_duplicate_occurrences_remain_distinct() {
        let base_playlist = playlist(&[(1, "same"), (2, "same")]);
        let mut local = base_playlist.clone();
        local.items.remove(0);
        let plan = build_remote_anchored_plan(
            "remote",
            &local,
            &remote_value(&base_playlist, serde_json::json!([])),
            None,
        );

        assert_eq!(plan.status, PlanStatus::Ready);
        assert_eq!(plan.changes.len(), 1);
        assert_eq!(
            plan.changes[0].classification,
            ChangeClassification::LocalOnly
        );
        assert_eq!(plan.changes[0].kind, ChangeKind::Removed);
        assert_eq!(plan.changes[0].occurrence, Some(1));
        assert!(plan.conflicts.is_empty());
    }

    #[test]
    fn verified_remote_manifest_becomes_a_restart_safe_base_candidate() {
        let local = playlist(&[(1, "same"), (2, "same")]);
        let mbid = "12345678-1234-1234-1234-123456789abc";
        let mappings = BTreeMap::from([
            (PlaylistEntryId(1), mbid.to_owned()),
            (PlaylistEntryId(2), mbid.to_owned()),
        ]);
        let tracks = serde_json::json!([
            {"identifier": [format!("{MUSICBRAINZ_RECORDING_URI_PREFIX}{mbid}")]},
            {"identifier": [format!("{MUSICBRAINZ_RECORDING_URI_PREFIX}{mbid}")]}
        ]);
        let state = verified_sync_state_from_remote(
            "remote",
            &local,
            &remote_value_with_recording_mbids(&local, &mappings, tracks),
            None,
            42,
        )
        .unwrap();

        assert_eq!(state.base.unified_playlist_id, "local");
        assert_eq!(state.base.entries.len(), 2);
        assert_eq!(state.base.entries[0].occurrence, PlaylistEntryId(1));
        assert_eq!(state.base.entries[1].occurrence, PlaylistEntryId(2));
        assert_eq!(state.base.entries[0].recording_mbid.as_deref(), Some(mbid));
        assert_eq!(state.base.verified_at, 42);
        assert_eq!(state.base.canonical_manifest_hash.len(), 64);
        assert_eq!(state.base.remote_fingerprint.len(), 64);
    }

    #[test]
    fn invalid_or_drifting_remote_state_cannot_initialize_a_base() {
        let local = playlist(&[(1, "a")]);
        let missing = serde_json::json!({"playlist": {"title": "Mix", "track": []}});
        assert!(verified_sync_state_from_remote("remote", &local, &missing, None, 1).is_err());

        let drifted = remote_value(
            &local,
            serde_json::json!([{"identifier": [
                "https://musicbrainz.org/recording/12345678-1234-1234-1234-123456789abc"
            ]}]),
        );
        assert!(verified_sync_state_from_remote("remote", &local, &drifted, None, 1).is_err());
    }

    fn resolved_remote_and_base(
        playlist: &UnifiedPlaylist,
    ) -> (serde_json::Value, crate::state::ListenBrainzSyncBase) {
        let mappings = playlist
            .items
            .iter()
            .filter(|item| item.media_id.kind != MediaKind::Episode)
            .map(|item| {
                (
                    item.entry_id,
                    "12345678-1234-1234-1234-123456789abc".to_owned(),
                )
            })
            .collect::<BTreeMap<_, _>>();
        let tracks = playlist
            .items
            .iter()
            .filter(|item| item.media_id.kind != MediaKind::Episode)
            .map(|_| {
                serde_json::json!({
                    "identifier": [format!(
                        "{MUSICBRAINZ_RECORDING_URI_PREFIX}12345678-1234-1234-1234-123456789abc"
                    )]
                })
            })
            .collect::<Vec<_>>();
        let remote = remote_value_with_recording_mbids(
            playlist,
            &mappings,
            serde_json::Value::Array(tracks),
        );
        let base = verified_sync_state_from_remote("remote", playlist, &remote, None, 1)
            .unwrap()
            .base;
        (remote, base)
    }

    #[test]
    fn persisted_base_preview_classifies_directional_and_same_changes() {
        let original = playlist(&[(1, "a"), (2, "b")]);
        let (base_remote, base) = resolved_remote_and_base(&original);
        let no_change = build_persisted_base_pull_preview("remote", &base, &original, &base_remote);
        assert_eq!(
            no_change.classification,
            PullPreviewClassification::NoChange
        );

        let mut local = original.clone();
        local.items[0].media_id.raw_id = "local-a".to_owned();
        let local_only = build_persisted_base_pull_preview("remote", &base, &local, &base_remote);
        assert_eq!(
            local_only.classification,
            PullPreviewClassification::LocalOnly
        );

        let mut remote_playlist = original.clone();
        remote_playlist.items[1].media_id.raw_id = "remote-b".to_owned();
        let (remote_changed, _) = resolved_remote_and_base(&remote_playlist);
        let remote_only =
            build_persisted_base_pull_preview("remote", &base, &original, &remote_changed);
        assert_eq!(
            remote_only.classification,
            PullPreviewClassification::RemoteOnly
        );

        let same_change =
            build_persisted_base_pull_preview("remote", &base, &remote_playlist, &remote_changed);
        assert_eq!(
            same_change.classification,
            PullPreviewClassification::SameChange
        );
    }

    #[test]
    fn persisted_base_preview_types_conflict_reorder_and_dual_changes() {
        let original = playlist(&[(1, "a"), (2, "b"), (3, "c")]);
        let (_, base) = resolved_remote_and_base(&original);
        let mut local = original.clone();
        local.items[0].media_id.raw_id = "local-a".to_owned();
        let mut remote_playlist = original.clone();
        remote_playlist.items[1].media_id.raw_id = "remote-b".to_owned();
        let (remote_changed, _) = resolved_remote_and_base(&remote_playlist);
        let dual = build_persisted_base_pull_preview("remote", &base, &local, &remote_changed);
        assert_eq!(
            dual.classification,
            PullPreviewClassification::BothNonConflicting
        );
        assert_eq!(dual.affected_occurrences, vec![1, 2]);

        remote_playlist.items[0].media_id.raw_id = "remote-a".to_owned();
        let (remote_conflict, _) = resolved_remote_and_base(&remote_playlist);
        let conflict = build_persisted_base_pull_preview("remote", &base, &local, &remote_conflict);
        assert_eq!(conflict.classification, PullPreviewClassification::Conflict);
        assert!(conflict
            .plan
            .conflicts
            .iter()
            .any(|conflict| conflict.kind == ConflictKind::Mapping));

        let mut local_reorder = original.clone();
        local_reorder.items.swap(0, 1);
        let mut remote_reorder = original.clone();
        remote_reorder.items.swap(1, 2);
        let (remote_reordered, _) = resolved_remote_and_base(&remote_reorder);
        let reorder =
            build_persisted_base_pull_preview("remote", &base, &local_reorder, &remote_reordered);
        assert!(reorder
            .plan
            .conflicts
            .iter()
            .any(|conflict| conflict.kind == ConflictKind::Reorder));
    }

    #[test]
    fn persisted_base_preview_flags_unresolved_unlinked_duplicate_and_schema_states() {
        let original = playlist(&[(1, "same"), (2, "same")]);
        let (resolved_remote, base) = resolved_remote_and_base(&original);
        let unresolved_remote = remote_value(&original, serde_json::json!([]));
        let unresolved =
            build_persisted_base_pull_preview("remote", &base, &original, &unresolved_remote);
        assert_eq!(
            unresolved.classification,
            PullPreviewClassification::UnresolvedEntry
        );
        assert_eq!(unresolved.remote_unresolved, 2);

        let mut extra = resolved_remote.clone();
        extra["playlist"]["track"]
            .as_array_mut()
            .unwrap()
            .push(serde_json::json!({
                "identifier": [format!(
                    "{MUSICBRAINZ_RECORDING_URI_PREFIX}12345678-1234-1234-1234-123456789abc"
                )]
            }));
        let unlinked = build_persisted_base_pull_preview("remote", &base, &original, &extra);
        assert_eq!(
            unlinked.classification,
            PullPreviewClassification::UnlinkedRemoteRow
        );
        assert!(unlinked
            .plan
            .conflicts
            .iter()
            .any(|conflict| conflict.kind == ConflictKind::DuplicateAmbiguity));

        let schema = build_persisted_base_pull_preview(
            "remote",
            &base,
            &original,
            &serde_json::json!({"playlist": {"title": "Mix", "track": []}}),
        );
        assert_eq!(schema.classification, PullPreviewClassification::Conflict);
        assert!(schema
            .plan
            .conflicts
            .iter()
            .any(|conflict| conflict.kind == ConflictKind::Schema));
        let json = serde_json::to_string(&schema).unwrap();
        assert!(!json.contains("same"));
        assert!(!json.contains("annotation"));
        assert!(!json.contains("musicbrainz.org"));
    }

    #[test]
    fn planner_classifies_remote_only_and_same_change_by_occurrence() {
        let base = SyncSnapshot::testing("local", "Mix", vec![entry(1, "a")]);
        let local = SyncSnapshot::testing("local", "Mix", vec![entry(1, "b")]);
        let remote = SyncSnapshot::testing("local", "Mix", vec![entry(1, "b"), entry(2, "c")]);
        let plan = plan_snapshots("remote", &base, &local, &remote, Vec::new());

        assert!(plan.changes.iter().any(|change| {
            change.occurrence == Some(1)
                && change.classification == ChangeClassification::SameChange
                && change.kind == ChangeKind::IdentityChanged
        }));
        assert!(plan.changes.iter().any(|change| {
            change.occurrence == Some(2)
                && change.classification == ChangeClassification::RemoteOnly
                && change.kind == ChangeKind::Added
        }));
    }

    #[test]
    fn concurrent_incompatible_reorder_fails_closed() {
        let base = SyncSnapshot::testing(
            "local",
            "Mix",
            vec![entry(1, "a"), entry(2, "b"), entry(3, "c")],
        );
        let local = SyncSnapshot::testing(
            "local",
            "Mix",
            vec![entry(2, "b"), entry(1, "a"), entry(3, "c")],
        );
        let remote = SyncSnapshot::testing(
            "local",
            "Mix",
            vec![entry(1, "a"), entry(3, "c"), entry(2, "b")],
        );
        let plan = plan_snapshots("remote", &base, &local, &remote, Vec::new());

        assert!(plan
            .conflicts
            .iter()
            .any(|conflict| conflict.kind == ConflictKind::Reorder));
    }

    #[test]
    fn concurrent_add_and_delete_edit_have_typed_conflicts() {
        let base = SyncSnapshot::testing("local", "Mix", vec![entry(1, "a")]);
        let local =
            SyncSnapshot::testing("local", "Mix", vec![entry(1, "a"), entry(2, "local-add")]);
        let remote = SyncSnapshot::testing(
            "local",
            "Mix",
            vec![entry(1, "remote-edit"), entry(2, "remote-add")],
        );
        let add_add = plan_snapshots("remote", &base, &local, &remote, Vec::new());
        assert!(add_add
            .conflicts
            .iter()
            .any(|conflict| conflict.kind == ConflictKind::AddAdd));

        let deleted = SyncSnapshot::testing("local", "Mix", Vec::new());
        let delete_edit = plan_snapshots("remote", &base, &deleted, &remote, Vec::new());
        assert!(delete_edit
            .conflicts
            .iter()
            .any(|conflict| conflict.kind == ConflictKind::DeleteEdit));
    }

    #[test]
    fn missing_or_foreign_manifest_cannot_be_planned_losslessly() {
        let local = playlist(&[(1, "a")]);
        let missing = build_remote_anchored_plan(
            "remote",
            &local,
            &serde_json::json!({"playlist": {"title": "Mix", "track": []}}),
            None,
        );
        assert_eq!(missing.status, PlanStatus::CannotPlan);
        assert_eq!(
            serde_json::to_value(&missing).unwrap()["cannot_plan_reason"],
            "lossless_remote_manifest_missing"
        );

        let mut foreign = playlist(&[(1, "a")]);
        foreign.id = "other".to_owned();
        let mismatch = build_remote_anchored_plan(
            "remote",
            &local,
            &remote_value(&foreign, serde_json::json!([])),
            None,
        );
        assert_eq!(mismatch.status, PlanStatus::CannotPlan);
        assert_eq!(
            serde_json::to_value(&mismatch).unwrap()["cannot_plan_reason"],
            "playlist_identity_mismatch"
        );
    }

    #[test]
    fn legacy_client_track_metadata_is_not_trusted_as_occurrence_identity() {
        let base = playlist(&[(1, "a"), (2, "b")]);
        let tracks = serde_json::json!([
            {"identifier": ["https://musicbrainz.org/recording/one"]},
            {
                "identifier": ["https://musicbrainz.org/recording/two"],
                "extension": {
                    "unified-player:entry_id": 2,
                    "unified-player:provider": "Spotify",
                    "unified-player:media_kind": "Track",
                    "unified-player:raw_id": "wrong"
                }
            }
        ]);
        let plan =
            build_remote_anchored_plan("remote", &base, &v1_remote_value(&base, tracks), None);

        assert_eq!(
            plan.conflicts
                .iter()
                .filter(|conflict| conflict.kind == ConflictKind::UnlinkedRemoteRow)
                .count(),
            2
        );
    }

    #[test]
    fn client_track_metadata_is_not_trusted_as_occurrence_identity() {
        let base = playlist(&[(1, "a"), (2, "b")]);
        let tracks = serde_json::json!([
            {
                "identifier": ["https://musicbrainz.org/recording/two"],
                "extension": {
                    "https://musicbrainz.org/doc/jspf#track": {
                        "additional_metadata": {
                            "unified-player": {"occurrence": 2}
                        }
                    }
                }
            },
            {
                "identifier": ["https://musicbrainz.org/recording/one"],
                "extension": {
                    "https://musicbrainz.org/doc/jspf#track": {
                        "additional_metadata": {
                            "unified-player": {"occurrence": 1}
                        }
                    }
                }
            }
        ]);
        let plan =
            build_remote_anchored_plan("remote", &base, &v1_remote_value(&base, tracks), None);

        assert_eq!(
            plan.conflicts
                .iter()
                .filter(|conflict| conflict.kind == ConflictKind::UnlinkedRemoteRow)
                .count(),
            2
        );
        assert_eq!(
            serde_json::to_value(&plan).unwrap()["occurrence_identity_contract"],
            "manifest_only"
        );
        assert!(serde_json::to_string(&plan)
            .unwrap()
            .contains("client_track_metadata_unsupported"));
    }

    #[test]
    fn v2_manifest_verifies_exact_projection_sequence_with_duplicates() {
        let base = playlist(&[(1, "a"), (2, "a")]);
        let mbid = "12345678-1234-1234-1234-123456789abc";
        let mappings = BTreeMap::from([
            (PlaylistEntryId(1), mbid.to_owned()),
            (PlaylistEntryId(2), mbid.to_owned()),
        ]);
        let tracks = serde_json::json!([
            {"identifier": [format!("{MUSICBRAINZ_RECORDING_URI_PREFIX}{mbid}")]},
            {"identifier": [format!("{MUSICBRAINZ_RECORDING_URI_PREFIX}{mbid}")]}
        ]);
        let remote = remote_value_with_recording_mbids(&base, &mappings, tracks);
        let plan = build_remote_anchored_plan("remote", &base, &remote, None);

        assert!(plan.conflicts.is_empty());
        assert!(!serde_json::to_string(&plan)
            .unwrap()
            .contains("projection_coverage_unverifiable"));
    }

    #[test]
    fn v2_manifest_detects_projection_order_drift_without_position_identity() {
        let base = playlist(&[(1, "a"), (2, "b")]);
        let first = "12345678-1234-1234-1234-123456789abc";
        let second = "abcdefab-cdef-abcd-efab-cdefabcdefab";
        let mappings = BTreeMap::from([
            (PlaylistEntryId(1), first.to_owned()),
            (PlaylistEntryId(2), second.to_owned()),
        ]);
        let tracks = serde_json::json!([
            {"identifier": [format!("{MUSICBRAINZ_RECORDING_URI_PREFIX}{second}")]},
            {"identifier": [format!("{MUSICBRAINZ_RECORDING_URI_PREFIX}{first}")]}
        ]);
        let remote = remote_value_with_recording_mbids(&base, &mappings, tracks);
        let plan = build_remote_anchored_plan("remote", &base, &remote, None);

        assert!(plan
            .conflicts
            .iter()
            .any(|conflict| conflict.kind == ConflictKind::ManifestProjectionDrift));
        assert!(!plan
            .conflicts
            .iter()
            .any(|conflict| conflict.kind == ConflictKind::UnlinkedRemoteRow));
    }

    #[test]
    fn stale_remote_fingerprint_is_deterministic_and_fail_closed() {
        let local = playlist(&[(1, "a")]);
        let remote = remote_value(&local, serde_json::json!([]));
        let first = build_remote_anchored_plan("remote", &local, &remote, None);
        let second = build_remote_anchored_plan("remote", &local, &remote, None);
        assert_eq!(first.remote_fingerprint, second.remote_fingerprint);
        let actual = first.remote_fingerprint.unwrap();

        let accepted = build_remote_anchored_plan("remote", &local, &remote, Some(&actual));
        assert_eq!(accepted.status, PlanStatus::Ready);
        let stale = build_remote_anchored_plan("remote", &local, &remote, Some(&"0".repeat(64)));
        assert_eq!(stale.status, PlanStatus::CannotPlan);
        assert!(stale
            .conflicts
            .iter()
            .any(|conflict| conflict.kind == ConflictKind::StaleBase));
        assert!(!serde_json::to_string(&stale)
            .unwrap()
            .contains("annotation"));
    }

    #[test]
    fn unsupported_or_snapshot_mismatched_manifests_cannot_be_planned() {
        let local = playlist(&[(1, "a")]);
        let invalid = serde_json::json!({
            "playlist": {
                "title": "Mix",
                "annotation": "unified-player-playlist:v1\n{not-json"
            }
        });
        let invalid_plan = build_remote_anchored_plan("remote", &local, &invalid, None);
        assert_eq!(invalid_plan.status, PlanStatus::CannotPlan);
        assert_eq!(
            serde_json::to_value(&invalid_plan).unwrap()["cannot_plan_reason"],
            "invalid_remote_manifest"
        );

        let mut unsupported = remote_value(&local, serde_json::json!([]));
        let annotation = unsupported["playlist"]["annotation"]
            .as_str()
            .unwrap()
            .replace("\"schema_version\":2", "\"schema_version\":3");
        unsupported["playlist"]["annotation"] = serde_json::Value::String(annotation);
        let unsupported_plan = build_remote_anchored_plan("remote", &local, &unsupported, None);
        assert_eq!(unsupported_plan.status, PlanStatus::CannotPlan);
        assert_eq!(
            serde_json::to_value(&unsupported_plan).unwrap()["cannot_plan_reason"],
            "unsupported_manifest_schema"
        );

        let mut mismatched = remote_value(&local, serde_json::json!([]));
        let annotation = mismatched["playlist"]["annotation"]
            .as_str()
            .unwrap()
            .replace(&local.snapshot_hash(), &"f".repeat(64));
        mismatched["playlist"]["annotation"] = serde_json::Value::String(annotation);
        let mismatch_plan = build_remote_anchored_plan("remote", &local, &mismatched, None);
        assert_eq!(mismatch_plan.status, PlanStatus::CannotPlan);
        assert_eq!(
            serde_json::to_value(&mismatch_plan).unwrap()["cannot_plan_reason"],
            "manifest_snapshot_mismatch"
        );
    }

    #[test]
    fn plan_json_is_stable_and_contains_no_remote_payload() {
        let local = playlist(&[(1, "a")]);
        let remote = remote_value(&local, serde_json::json!([]));
        let plan = build_remote_anchored_plan("remote", &local, &remote, None);
        let first = serde_json::to_string_pretty(&plan).unwrap();
        let second = serde_json::to_string_pretty(&build_remote_anchored_plan(
            "remote", &local, &remote, None,
        ))
        .unwrap();

        assert_eq!(first, second);
        assert_eq!(
            plan.remote_manifest_fingerprint.as_deref(),
            Some(
                description_manifest_fingerprint(&description_manifest(&local).unwrap())
                    .unwrap()
                    .as_str()
            )
        );
        assert!(first.contains("\"base_source\": \"remote_manifest_anchor\""));
        assert!(first.contains("\"writes_performed\": false"));
        assert!(!first.contains("unified-player-playlist"));
        assert!(!first.contains("annotation"));
    }
}
