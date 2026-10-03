use std::collections::{BTreeMap, BTreeSet};

use serde::Serialize;

use crate::state::{
    ListenBrainzSyncBase, MediaId, PlaylistEntryId, UnifiedPlaylist, UnifiedPlaylistItem,
    UnifiedPlaylistMetadata,
};

use super::listenbrainz_projection::{ExplicitRecordingRelation, NativeProjectionPreview};
use super::listenbrainz_pull::remote_playlist_from_manifest;
use super::listenbrainz_push::{
    execute_push_transaction_with_guard, verified_base_from_readback, ListenBrainzMutationAdapter,
    PushTransactionStatus,
};
use super::listenbrainz_sync::{
    build_persisted_base_pull_preview, ChangeClassification, ChangeKind, ConflictKind,
    ListenBrainzPullPreview, PlanStatus,
};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum ResolutionPolicy {
    KeepLocal,
    KeepListenBrainz,
    MergeNonConflicting,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum ResolutionSide {
    Local,
    ListenBrainz,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct ConflictDecision {
    pub(crate) conflict_index: usize,
    pub(crate) side: ResolutionSide,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct UnlinkedImport {
    pub(crate) conflict_index: usize,
    pub(crate) media_id: MediaId,
    pub(crate) recording_mbid: String,
}

// The flags are independent fields of the serialized preview report.
#[allow(clippy::struct_excessive_bools)]
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub(crate) struct ResolutionPreview {
    pub(crate) schema_version: u8,
    pub(crate) policy: ResolutionPolicy,
    pub(crate) ready: bool,
    pub(crate) additions: usize,
    pub(crate) removals: usize,
    pub(crate) reorder: bool,
    pub(crate) rename: bool,
    pub(crate) conflicts: usize,
    pub(crate) decisions_required: usize,
    pub(crate) decisions_supplied: usize,
    pub(crate) affected_occurrences: Vec<u64>,
    pub(crate) local_write_required: bool,
    pub(crate) remote_write_required: bool,
    pub(crate) writes_performed: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct ResolutionPlan {
    pub(crate) pull: ListenBrainzPullPreview,
    pub(crate) preview: ResolutionPreview,
    pub(crate) target: Option<UnifiedPlaylist>,
    pub(crate) expected_remote_fingerprint: Option<String>,
    pub(crate) imported_relationships: Vec<ExplicitRecordingRelation>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub(crate) struct ResolutionApplyResult {
    pub(crate) remote_status: Option<PushTransactionStatus>,
    pub(crate) local_applied: bool,
    pub(crate) rollback_available: bool,
    pub(crate) completed: bool,
}

#[allow(clippy::too_many_arguments)]
pub(crate) async fn execute_resolution(
    adapter: &ListenBrainzMutationAdapter,
    data: &mut crate::state::AppData,
    token: &str,
    remote_playlist_id: &str,
    unified_playlist_id: &str,
    remote_value: &serde_json::Value,
    policy: ResolutionPolicy,
    decisions: &[ConflictDecision],
    unlinked_imports: &[UnlinkedImport],
    relationships: &[ExplicitRecordingRelation],
    operation_id: &str,
    observed_at: u64,
) -> anyhow::Result<(
    ResolutionPlan,
    NativeProjectionPreview,
    ResolutionApplyResult,
)> {
    let local = data
        .unified_playlists
        .iter()
        .find(|playlist| playlist.id == unified_playlist_id)
        .ok_or_else(|| anyhow::anyhow!("unified playlist not found"))?
        .clone();
    let base = data
        .playlist_links
        .iter()
        .find(|link| link.unified_playlist_id == unified_playlist_id)
        .and_then(|link| link.listenbrainz_sync.as_ref())
        .ok_or_else(|| anyhow::anyhow!("ListenBrainz sync base is not initialized"))?
        .base
        .clone();
    let plan = build_resolution_plan(
        remote_playlist_id,
        &base,
        &local,
        remote_value,
        policy,
        decisions,
        unlinked_imports,
        observed_at,
    )?;
    anyhow::ensure!(
        plan.preview.ready,
        "ListenBrainz resolution is incomplete or selects an unsafe remote row"
    );
    let target = plan.target.as_ref().expect("ready plans have a target");
    let inherited = inherited_remote_relationships(remote_value, target)?;
    let mut effective_relationships = inherited;
    effective_relationships.extend(plan.imported_relationships.clone());
    effective_relationships.extend_from_slice(relationships);
    let (projection, projection_preview) =
        super::listenbrainz_projection::preview_native_projection(
            target,
            &effective_relationships,
            crate::cli::listenbrainz_manifest::DESCRIPTION_CHARACTER_BUDGET,
        )?;
    anyhow::ensure!(
        projection_preview.is_ready(),
        "ListenBrainz resolution exceeds the annotation budget"
    );

    let mut remote_status = None;
    if plan.preview.remote_write_required {
        let result = execute_push_transaction_with_guard(
            adapter,
            data,
            token,
            remote_playlist_id,
            target,
            &projection,
            operation_id,
            observed_at,
            plan.expected_remote_fingerprint.as_deref(),
        )
        .await?;
        remote_status = Some(result.status);
        if result.status != PushTransactionStatus::Verified {
            return Ok((
                plan,
                projection_preview,
                ResolutionApplyResult {
                    remote_status,
                    local_applied: false,
                    rollback_available: false,
                    completed: false,
                },
            ));
        }
    }

    let mut local_applied = false;
    if plan.preview.local_write_required {
        let verified_base = if plan.preview.remote_write_required {
            data.playlist_links
                .iter()
                .find(|link| link.unified_playlist_id == unified_playlist_id)
                .and_then(|link| link.listenbrainz_sync.as_ref())
                .expect("verified push retains sync state")
                .base
                .clone()
        } else {
            verified_base_from_readback(
                remote_playlist_id,
                target,
                remote_value,
                plan.expected_remote_fingerprint.as_deref(),
                observed_at,
            )?
        };
        data.begin_listenbrainz_pull_apply(
            unified_playlist_id,
            remote_playlist_id,
            operation_id,
            observed_at,
        )?;
        data.commit_listenbrainz_pull_apply(
            unified_playlist_id,
            remote_playlist_id,
            operation_id,
            target.clone(),
            verified_base,
        )?;
        local_applied = true;
    }
    Ok((
        plan,
        projection_preview,
        ResolutionApplyResult {
            remote_status,
            local_applied,
            rollback_available: local_applied,
            completed: true,
        },
    ))
}

fn inherited_remote_relationships(
    remote_value: &serde_json::Value,
    target: &UnifiedPlaylist,
) -> anyhow::Result<Vec<ExplicitRecordingRelation>> {
    let playlist = remote_value.get("playlist").unwrap_or(remote_value);
    let annotation = playlist
        .get("annotation")
        .and_then(serde_json::Value::as_str)
        .ok_or_else(|| anyhow::anyhow!("ListenBrainz playlist has no lossless manifest"))?;
    let manifest = crate::cli::listenbrainz_manifest::parse_description_manifest(annotation)?;
    let target_by_occurrence = target
        .items
        .iter()
        .map(|item| (item.entry_id.0, &item.media_id))
        .collect::<BTreeMap<_, _>>();
    Ok(manifest
        .entries
        .iter()
        .filter_map(|entry| {
            let recording_mbid = entry.projection.as_ref()?.recording_mbid.as_ref()?;
            let media_id = target_by_occurrence.get(&entry.occurrence)?;
            Some(ExplicitRecordingRelation {
                media_id: (*media_id).clone(),
                recording_mbid: recording_mbid.clone(),
            })
        })
        .collect())
}

pub(crate) fn build_resolution_plan(
    remote_playlist_id: &str,
    base: &ListenBrainzSyncBase,
    local: &UnifiedPlaylist,
    remote_value: &serde_json::Value,
    policy: ResolutionPolicy,
    decisions: &[ConflictDecision],
    unlinked_imports: &[UnlinkedImport],
    observed_at: u64,
) -> anyhow::Result<ResolutionPlan> {
    let pull = build_persisted_base_pull_preview(remote_playlist_id, base, local, remote_value);
    let remote = remote_playlist_from_manifest(remote_value, local, observed_at)?;
    let decisions_by_index = decisions
        .iter()
        .map(|decision| (decision.conflict_index, decision.side))
        .collect::<BTreeMap<_, _>>();
    anyhow::ensure!(
        decisions_by_index.len() == decisions.len()
            && decisions_by_index
                .keys()
                .all(|index| *index < pull.plan.conflicts.len()),
        "ListenBrainz conflict decisions contain a duplicate or invalid index"
    );
    let imports_by_index = unlinked_imports
        .iter()
        .map(|import| (import.conflict_index, import))
        .collect::<BTreeMap<_, _>>();
    let available_unlinked_mbids = unlinked_recording_mbids(remote_value)?;
    let mut selected_unlinked_mbids = BTreeSet::new();
    anyhow::ensure!(
        imports_by_index.len() == unlinked_imports.len()
            && imports_by_index.iter().all(|(index, import)| {
                policy == ResolutionPolicy::MergeNonConflicting
                    && decisions_by_index.get(index) == Some(&ResolutionSide::ListenBrainz)
                    && pull
                        .plan
                        .conflicts
                        .get(*index)
                        .is_some_and(|conflict| conflict.kind == ConflictKind::UnlinkedRemoteRow)
                    && crate::cli::listenbrainz_manifest::is_recording_mbid(&import.recording_mbid)
                    && available_unlinked_mbids
                        .contains(&import.recording_mbid.to_ascii_lowercase())
                    && selected_unlinked_mbids.insert(import.recording_mbid.to_ascii_lowercase())
            }),
        "ListenBrainz unlinked import does not match one unique unlinked remote recording"
    );

    let unsafe_remote_choice = pull
        .plan
        .conflicts
        .iter()
        .enumerate()
        .any(|(index, conflict)| {
            matches!(
                conflict.kind,
                ConflictKind::ManifestProjectionDrift
                    | ConflictKind::UnlinkedRemoteRow
                    | ConflictKind::DuplicateAmbiguity
                    | ConflictKind::Schema
            ) && (policy == ResolutionPolicy::KeepListenBrainz
                || decisions_by_index.get(&index) == Some(&ResolutionSide::ListenBrainz))
                && !(conflict.kind == ConflictKind::UnlinkedRemoteRow
                    && policy == ResolutionPolicy::MergeNonConflicting
                    && imports_by_index.contains_key(&index))
        });
    let decisions_required = pull.plan.conflicts.len();
    let decisions_complete =
        (0..pull.plan.conflicts.len()).all(|index| decisions_by_index.contains_key(&index));
    let policy_consistent = decisions_by_index.values().all(|side| match policy {
        ResolutionPolicy::KeepLocal => *side == ResolutionSide::Local,
        ResolutionPolicy::KeepListenBrainz => *side == ResolutionSide::ListenBrainz,
        ResolutionPolicy::MergeNonConflicting => true,
    });
    let structurally_ready =
        pull.plan.status == PlanStatus::Ready && !unsafe_remote_choice && policy_consistent;

    let mut target = if !structurally_ready || !decisions_complete {
        None
    } else {
        Some(match policy {
            ResolutionPolicy::KeepLocal => local.clone(),
            ResolutionPolicy::KeepListenBrainz => remote.clone(),
            ResolutionPolicy::MergeNonConflicting => merge_playlists(
                base,
                local,
                &remote,
                &pull,
                &decisions_by_index,
                observed_at,
            )?,
        })
    };
    let mut imported_relationships = Vec::new();
    if let Some(target) = target.as_mut() {
        for import in unlinked_imports {
            let item = target.allocate_item(UnifiedPlaylistItem {
                media_id: import.media_id.clone(),
                metadata: UnifiedPlaylistMetadata {
                    provenance: Some("listenbrainz-explicit-unlinked-import".to_owned()),
                    observed_at: Some(observed_at),
                    ..UnifiedPlaylistMetadata::default()
                },
                ..UnifiedPlaylistItem::default()
            })?;
            target.items.push(item);
            imported_relationships.push(ExplicitRecordingRelation {
                media_id: import.media_id.clone(),
                recording_mbid: import.recording_mbid.to_ascii_lowercase(),
            });
        }
    }
    let expected_remote_fingerprint = pull.plan.remote_fingerprint.clone();
    let (additions, removals, reorder, rename, local_write_required, remote_write_required) =
        target
            .as_ref()
            .map_or((0, 0, false, false, false, false), |target| {
                let local_ids = identity_by_occurrence(local);
                let target_ids = identity_by_occurrence(target);
                let additions = target_ids
                    .iter()
                    .filter(|(occurrence, identity)| local_ids.get(occurrence) != Some(identity))
                    .count();
                let removals = local_ids
                    .iter()
                    .filter(|(occurrence, identity)| target_ids.get(occurrence) != Some(identity))
                    .count();
                let reorder = occurrence_order(local) != occurrence_order(target);
                let rename = local.name != target.name;
                let local_write_required = local.snapshot_hash() != target.snapshot_hash();
                let remote_write_required = remote.snapshot_hash() != target.snapshot_hash();
                (
                    additions,
                    removals,
                    reorder,
                    rename,
                    local_write_required,
                    remote_write_required,
                )
            });
    let affected_occurrences = target
        .as_ref()
        .map(|target| {
            let local_ids = identity_by_occurrence(local);
            let target_ids = identity_by_occurrence(target);
            local_ids
                .keys()
                .chain(target_ids.keys())
                .copied()
                .collect::<BTreeSet<_>>()
                .into_iter()
                .filter(|occurrence| local_ids.get(occurrence) != target_ids.get(occurrence))
                .map(|occurrence| occurrence.0)
                .collect()
        })
        .unwrap_or_default();
    Ok(ResolutionPlan {
        preview: ResolutionPreview {
            schema_version: 1,
            policy,
            ready: target.is_some() && expected_remote_fingerprint.is_some(),
            additions,
            removals,
            reorder,
            rename,
            conflicts: pull.plan.conflicts.len(),
            decisions_required,
            decisions_supplied: decisions_by_index.len(),
            affected_occurrences,
            local_write_required,
            remote_write_required,
            writes_performed: false,
        },
        pull,
        target,
        expected_remote_fingerprint,
        imported_relationships,
    })
}

fn unlinked_recording_mbids(remote_value: &serde_json::Value) -> anyhow::Result<BTreeSet<String>> {
    const PREFIX: &str = "https://musicbrainz.org/recording/";
    let playlist = remote_value.get("playlist").unwrap_or(remote_value);
    let annotation = playlist
        .get("annotation")
        .and_then(serde_json::Value::as_str)
        .ok_or_else(|| anyhow::anyhow!("ListenBrainz playlist has no lossless manifest"))?;
    let manifest = crate::cli::listenbrainz_manifest::parse_description_manifest(annotation)?;
    let mut expected = manifest
        .entries
        .iter()
        .filter_map(|entry| entry.projection.as_ref()?.recording_mbid.as_deref())
        .fold(BTreeMap::<String, usize>::new(), |mut counts, mbid| {
            *counts.entry(mbid.to_ascii_lowercase()).or_default() += 1;
            counts
        });
    let mut unlinked = BTreeSet::new();
    for row in playlist
        .get("track")
        .and_then(serde_json::Value::as_array)
        .into_iter()
        .flatten()
    {
        let mbid = row
            .get("identifier")
            .and_then(serde_json::Value::as_array)
            .into_iter()
            .flatten()
            .filter_map(serde_json::Value::as_str)
            .find_map(|identifier| identifier.strip_prefix(PREFIX))
            .map(str::to_ascii_lowercase);
        let Some(mbid) = mbid else {
            continue;
        };
        if expected.get_mut(&mbid).is_some_and(|count| {
            if *count == 0 {
                false
            } else {
                *count -= 1;
                true
            }
        }) {
            continue;
        }
        unlinked.insert(mbid);
    }
    Ok(unlinked)
}

fn merge_playlists(
    base: &ListenBrainzSyncBase,
    local: &UnifiedPlaylist,
    remote: &UnifiedPlaylist,
    pull: &ListenBrainzPullPreview,
    decisions: &BTreeMap<usize, ResolutionSide>,
    observed_at: u64,
) -> anyhow::Result<UnifiedPlaylist> {
    let base_items = base
        .entries
        .iter()
        .map(|entry| (entry.occurrence, entry.media_id.clone()))
        .collect::<BTreeMap<_, _>>();
    let local_items = local
        .items
        .iter()
        .map(|item| (item.entry_id, item))
        .collect::<BTreeMap<_, _>>();
    let remote_items = remote
        .items
        .iter()
        .map(|item| (item.entry_id, item))
        .collect::<BTreeMap<_, _>>();
    let occurrences = base_items
        .keys()
        .chain(local_items.keys())
        .chain(remote_items.keys())
        .copied()
        .collect::<BTreeSet<_>>();
    let mut chosen = BTreeMap::<PlaylistEntryId, UnifiedPlaylistItem>::new();
    for occurrence in occurrences {
        let base_identity = base_items.get(&occurrence);
        let local_item = local_items.get(&occurrence).copied();
        let remote_item = remote_items.get(&occurrence).copied();
        let local_identity = local_item.map(|item| &item.media_id);
        let remote_identity = remote_item.map(|item| &item.media_id);
        let selected = if local_identity == base_identity {
            remote_item
        } else if remote_identity == base_identity || local_identity == remote_identity {
            local_item
        } else {
            let conflict_index = pull
                .plan
                .conflicts
                .iter()
                .position(|conflict| conflict.occurrence == Some(occurrence.0))
                .ok_or_else(|| {
                    anyhow::anyhow!("ListenBrainz merge conflict has no decision slot")
                })?;
            match decisions.get(&conflict_index) {
                Some(ResolutionSide::Local) => local_item,
                Some(ResolutionSide::ListenBrainz) => remote_item,
                None => anyhow::bail!("ListenBrainz merge conflict decision is missing"),
            }
        };
        if let Some(item) = selected {
            chosen.insert(occurrence, item.clone());
        }
    }

    let reorder_conflict_index = pull
        .plan
        .conflicts
        .iter()
        .position(|conflict| conflict.kind == ConflictKind::Reorder);
    let remote_reordered = pull.plan.changes.iter().any(|change| {
        change.kind == ChangeKind::Reordered
            && change.classification == ChangeClassification::RemoteOnly
    });
    let use_remote_order = reorder_conflict_index
        .and_then(|index| decisions.get(&index))
        .is_some_and(|side| *side == ResolutionSide::ListenBrainz)
        || (reorder_conflict_index.is_none() && remote_reordered);
    let primary = if use_remote_order { remote } else { local };
    let secondary = if use_remote_order { local } else { remote };
    let mut order = primary
        .items
        .iter()
        .chain(secondary.items.iter())
        .map(|item| item.entry_id)
        .filter(|occurrence| chosen.contains_key(occurrence))
        .collect::<Vec<_>>();
    let mut seen = BTreeSet::new();
    order.retain(|occurrence| seen.insert(*occurrence));

    let rename_conflict_index = pull.plan.conflicts.iter().position(|conflict| {
        conflict.kind == ConflictKind::Mapping && conflict.occurrence.is_none()
    });
    let remote_renamed = pull.plan.changes.iter().any(|change| {
        change.kind == ChangeKind::PlaylistRenamed
            && change.classification == ChangeClassification::RemoteOnly
    });
    let use_remote_name = rename_conflict_index
        .and_then(|index| decisions.get(&index))
        .is_some_and(|side| *side == ResolutionSide::ListenBrainz)
        || (rename_conflict_index.is_none() && remote_renamed);
    let mut merged = UnifiedPlaylist {
        id: local.id.clone(),
        name: if use_remote_name {
            remote.name.clone()
        } else {
            local.name.clone()
        },
        items: order
            .into_iter()
            .filter_map(|occurrence| chosen.remove(&occurrence))
            .collect(),
        updated_at: observed_at,
        next_entry_id: local.next_entry_id.max(remote.next_entry_id),
    };
    merged.normalize_entry_ids()?;
    Ok(merged)
}

fn identity_by_occurrence(playlist: &UnifiedPlaylist) -> BTreeMap<PlaylistEntryId, MediaId> {
    playlist
        .items
        .iter()
        .map(|item| (item.entry_id, item.media_id.clone()))
        .collect()
}

fn occurrence_order(playlist: &UnifiedPlaylist) -> Vec<PlaylistEntryId> {
    playlist.items.iter().map(|item| item.entry_id).collect()
}

#[cfg(test)]
mod tests {
    use super::{
        build_resolution_plan, execute_resolution, ConflictDecision, ResolutionPolicy,
        ResolutionSide, UnlinkedImport,
    };
    use crate::cli::listenbrainz_manifest::description_envelope;
    use crate::client::listenbrainz_push::ListenBrainzMutationAdapter;
    use crate::state::{
        AppData, ListenBrainzProjectionStatus, ListenBrainzSyncBase, ListenBrainzSyncBaseEntry,
        ListenBrainzSyncState, MediaId, MediaKind, PlaylistEntryId, PlaylistLink, Provider,
        UnifiedPlaylist, UnifiedPlaylistItem,
    };

    fn item(occurrence: u64, id: &str) -> UnifiedPlaylistItem {
        UnifiedPlaylistItem {
            entry_id: PlaylistEntryId(occurrence),
            media_id: MediaId {
                provider: Provider::Spotify,
                kind: MediaKind::Track,
                raw_id: id.to_owned(),
            },
            title: id.to_owned(),
            ..UnifiedPlaylistItem::default()
        }
    }

    fn playlist(name: &str, entries: &[(u64, &str)]) -> UnifiedPlaylist {
        UnifiedPlaylist {
            id: "local-1".to_owned(),
            name: name.to_owned(),
            items: entries.iter().map(|(id, raw)| item(*id, raw)).collect(),
            next_entry_id: 20,
            ..UnifiedPlaylist::default()
        }
    }

    fn base(playlist: &UnifiedPlaylist) -> ListenBrainzSyncBase {
        ListenBrainzSyncBase {
            manifest_schema_version: 2,
            unified_playlist_id: playlist.id.clone(),
            playlist_name: playlist.name.clone(),
            local_snapshot_hash: playlist.snapshot_hash(),
            canonical_manifest_hash: "2".repeat(64),
            remote_fingerprint: "3".repeat(64),
            entries: playlist
                .items
                .iter()
                .map(|item| ListenBrainzSyncBaseEntry {
                    occurrence: item.entry_id,
                    media_id: item.media_id.clone(),
                    projection_status: ListenBrainzProjectionStatus::Unresolved,
                    recording_mbid: None,
                })
                .collect(),
            verified_at: 10,
        }
    }

    fn remote(playlist: &UnifiedPlaylist) -> serde_json::Value {
        serde_json::json!({
            "playlist": {
                "identifier": "https://listenbrainz.org/playlist/remote-1",
                "title": playlist.name,
                "annotation": description_envelope(playlist).unwrap(),
                "track": []
            }
        })
    }

    #[test]
    fn merge_combines_non_conflicting_occurrence_changes_and_reports_consequences() {
        let original = playlist("Mix", &[(1, "a"), (2, "b")]);
        let local = playlist("Mix", &[(1, "a"), (2, "b"), (3, "local")]);
        let listenbrainz = playlist("Remote Mix", &[(1, "a"), (2, "remote")]);

        let plan = build_resolution_plan(
            "remote-1",
            &base(&original),
            &local,
            &remote(&listenbrainz),
            ResolutionPolicy::MergeNonConflicting,
            &[],
            &[],
            20,
        )
        .unwrap();

        assert!(plan.preview.ready);
        assert!(plan.preview.local_write_required);
        assert!(plan.preview.remote_write_required);
        assert_eq!(plan.preview.additions, 1);
        assert_eq!(plan.preview.removals, 1);
        assert_eq!(
            plan.target
                .unwrap()
                .items
                .iter()
                .map(|item| (item.entry_id.0, item.media_id.raw_id.as_str()))
                .collect::<Vec<_>>(),
            vec![(1, "a"), (2, "remote"), (3, "local")]
        );
    }

    #[test]
    fn merge_requires_each_ambiguous_conflict_decision_without_a_default() {
        let original = playlist("Mix", &[]);
        let local = playlist("Mix", &[(7, "local")]);
        let listenbrainz = playlist("Mix", &[(7, "remote")]);
        let remote_value = remote(&listenbrainz);
        let unresolved = build_resolution_plan(
            "remote-1",
            &base(&original),
            &local,
            &remote_value,
            ResolutionPolicy::MergeNonConflicting,
            &[],
            &[],
            20,
        )
        .unwrap();
        assert!(!unresolved.preview.ready);
        assert_eq!(unresolved.preview.decisions_required, 1);
        assert_eq!(unresolved.preview.decisions_supplied, 0);

        let resolved = build_resolution_plan(
            "remote-1",
            &base(&original),
            &local,
            &remote_value,
            ResolutionPolicy::MergeNonConflicting,
            &[ConflictDecision {
                conflict_index: 0,
                side: ResolutionSide::ListenBrainz,
            }],
            &[],
            20,
        )
        .unwrap();
        assert!(resolved.preview.ready);
        assert_eq!(resolved.target.unwrap().items[0].media_id.raw_id, "remote");
    }

    #[test]
    fn keep_policies_select_only_the_named_authority() {
        let original = playlist("Mix", &[(1, "base")]);
        let local = playlist("Local", &[(1, "local")]);
        let listenbrainz = playlist("Remote", &[(1, "remote")]);
        for (policy, expected) in [
            (ResolutionPolicy::KeepLocal, "local"),
            (ResolutionPolicy::KeepListenBrainz, "remote"),
        ] {
            let side = if policy == ResolutionPolicy::KeepLocal {
                ResolutionSide::Local
            } else {
                ResolutionSide::ListenBrainz
            };
            let plan = build_resolution_plan(
                "remote-1",
                &base(&original),
                &local,
                &remote(&listenbrainz),
                policy,
                &[
                    ConflictDecision {
                        conflict_index: 0,
                        side,
                    },
                    ConflictDecision {
                        conflict_index: 1,
                        side,
                    },
                ],
                &[],
                20,
            )
            .unwrap();
            assert!(plan.preview.ready);
            assert_eq!(plan.target.unwrap().items[0].media_id.raw_id, expected);
        }
    }

    #[test]
    fn explicit_unlinked_import_allocates_a_fresh_occurrence_without_guessing_identity() {
        let original = playlist("Mix", &[]);
        let mbid = "12345678-1234-1234-1234-123456789abc";
        let mut remote_value = remote(&original);
        remote_value["playlist"]["track"] = serde_json::json!([{
            "identifier": [format!("https://musicbrainz.org/recording/{mbid}")]
        }]);
        let decisions = [
            ConflictDecision {
                conflict_index: 0,
                side: ResolutionSide::Local,
            },
            ConflictDecision {
                conflict_index: 1,
                side: ResolutionSide::ListenBrainz,
            },
        ];
        let imports = [UnlinkedImport {
            conflict_index: 1,
            media_id: MediaId {
                provider: Provider::Spotify,
                kind: MediaKind::Track,
                raw_id: "explicit-track".to_owned(),
            },
            recording_mbid: mbid.to_owned(),
        }];

        let plan = build_resolution_plan(
            "remote-1",
            &base(&original),
            &original,
            &remote_value,
            ResolutionPolicy::MergeNonConflicting,
            &decisions,
            &imports,
            20,
        )
        .unwrap();

        assert!(plan.preview.ready);
        let target = plan.target.unwrap();
        assert_eq!(target.items.len(), 1);
        assert_eq!(target.items[0].entry_id, PlaylistEntryId(20));
        assert_eq!(target.items[0].media_id.raw_id, "explicit-track");
        assert_eq!(plan.imported_relationships[0].recording_mbid, mbid);
    }

    #[tokio::test]
    async fn keep_listenbrainz_applies_locally_without_a_remote_write() {
        let original = playlist("Mix", &[(1, "base")]);
        let local = playlist("Local", &[(1, "local")]);
        let listenbrainz = playlist("Remote", &[(1, "remote")]);
        let remote_value = remote(&listenbrainz);
        let folder = std::env::temp_dir().join(format!(
            "unified-player-listenbrainz-keep-remote-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&folder).unwrap();
        let mut data = AppData::new(&folder, &folder);
        data.upsert_unified_playlist(local).unwrap();
        data.upsert_playlist_link(PlaylistLink {
            unified_playlist_id: "local-1".to_owned(),
            listenbrainz_playlist_id: Some("remote-1".to_owned()),
            listenbrainz_sync: Some(ListenBrainzSyncState::verified(base(&original)).unwrap()),
            ..PlaylistLink::default()
        })
        .unwrap();
        let adapter = ListenBrainzMutationAdapter::new(reqwest::Client::new());

        let (_, _, result) = execute_resolution(
            &adapter,
            &mut data,
            "unused",
            "remote-1",
            "local-1",
            &remote_value,
            ResolutionPolicy::KeepListenBrainz,
            &[
                ConflictDecision {
                    conflict_index: 0,
                    side: ResolutionSide::ListenBrainz,
                },
                ConflictDecision {
                    conflict_index: 1,
                    side: ResolutionSide::ListenBrainz,
                },
            ],
            &[],
            &[],
            "resolve-1",
            20,
        )
        .await
        .unwrap();

        assert!(result.completed);
        assert!(result.local_applied);
        assert_eq!(result.remote_status, None);
        assert_eq!(data.unified_playlists[0].name, "Remote");
        assert_eq!(data.unified_playlists[0].items[0].media_id.raw_id, "remote");
        assert!(data.playlist_links[0]
            .listenbrainz_sync
            .as_ref()
            .unwrap()
            .local_apply_snapshot
            .is_some());
        std::fs::remove_dir_all(folder).unwrap();
    }
}
