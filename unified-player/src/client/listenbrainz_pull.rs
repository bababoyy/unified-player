use std::collections::HashMap;

use serde::Serialize;

use crate::cli::listenbrainz_manifest::{
    parse_description_manifest, parse_media_kind, parse_provider,
    DescriptionManifestProjectionStatus,
};
use crate::state::{
    AppData, DurationUnit, MediaId, PlaylistEntryId, UnifiedPlaylist, UnifiedPlaylistItem,
    UnifiedPlaylistMetadata,
};

use super::listenbrainz_sync::{
    build_persisted_base_pull_preview, verified_sync_state_from_remote, ChangeClassification,
    ListenBrainzPullPreview, PlanStatus,
};

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub(crate) struct PullApplyResult {
    pub(crate) applied: bool,
    pub(crate) occurrences: usize,
    pub(crate) unresolved: usize,
    pub(crate) rollback_available: bool,
    pub(crate) writes_performed: bool,
}

pub(crate) fn execute_pull_apply(
    data: &mut AppData,
    remote_playlist_id: &str,
    unified_playlist_id: &str,
    remote_value: &serde_json::Value,
    operation_id: &str,
    observed_at: u64,
) -> anyhow::Result<(ListenBrainzPullPreview, PullApplyResult)> {
    let local = data
        .unified_playlists
        .iter()
        .find(|playlist| playlist.id == unified_playlist_id)
        .ok_or_else(|| anyhow::anyhow!("unified playlist not found"))?
        .clone();
    let link = data
        .playlist_links
        .iter()
        .find(|link| link.unified_playlist_id == unified_playlist_id)
        .ok_or_else(|| anyhow::anyhow!("unified playlist link not found"))?;
    anyhow::ensure!(
        link.listenbrainz_playlist_id.as_deref() == Some(remote_playlist_id),
        "ListenBrainz link targets a different remote playlist"
    );
    let base = &link
        .listenbrainz_sync
        .as_ref()
        .ok_or_else(|| anyhow::anyhow!("ListenBrainz sync base is not initialized"))?
        .base;
    let preview = build_persisted_base_pull_preview(remote_playlist_id, base, &local, remote_value);
    anyhow::ensure!(
        preview.plan.status == PlanStatus::Ready && preview.plan.conflicts.is_empty(),
        "ListenBrainz pull contains conflicts and requires an explicit resolution"
    );
    anyhow::ensure!(
        preview.plan.changes.iter().all(|change| matches!(
            change.classification,
            ChangeClassification::RemoteOnly | ChangeClassification::SameChange
        )),
        "ListenBrainz pull would overwrite local-only changes"
    );

    let replacement = remote_playlist_from_manifest(remote_value, &local, observed_at)?;
    let expected_remote_fingerprint =
        preview.plan.remote_fingerprint.as_deref().ok_or_else(|| {
            anyhow::anyhow!("ListenBrainz pull preview has no remote fingerprint")
        })?;
    let verified_base = verified_sync_state_from_remote(
        remote_playlist_id,
        &replacement,
        remote_value,
        Some(expected_remote_fingerprint),
        observed_at,
    )?
    .base;

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
        replacement,
        verified_base,
    )?;
    Ok((
        preview,
        PullApplyResult {
            applied: true,
            occurrences: data
                .unified_playlists
                .iter()
                .find(|playlist| playlist.id == unified_playlist_id)
                .map_or(0, |playlist| playlist.items.len()),
            unresolved: data
                .unified_playlists
                .iter()
                .find(|playlist| playlist.id == unified_playlist_id)
                .map_or(0, |playlist| {
                    playlist
                        .items
                        .iter()
                        .filter(|item| item.metadata.degraded || item.metadata.metadata_pending)
                        .count()
                }),
            rollback_available: true,
            writes_performed: true,
        },
    ))
}

pub(crate) fn remote_playlist_from_manifest(
    remote_value: &serde_json::Value,
    local: &UnifiedPlaylist,
    observed_at: u64,
) -> anyhow::Result<UnifiedPlaylist> {
    let playlist_value = remote_value.get("playlist").unwrap_or(remote_value);
    let name = playlist_value
        .get("title")
        .and_then(serde_json::Value::as_str)
        .unwrap_or("ListenBrainz playlist")
        .to_owned();
    let annotation = playlist_value
        .get("annotation")
        .and_then(serde_json::Value::as_str)
        .ok_or_else(|| anyhow::anyhow!("ListenBrainz playlist has no lossless manifest"))?;
    let manifest = parse_description_manifest(annotation)?;
    anyhow::ensure!(
        manifest.playlist_id == local.id,
        "ListenBrainz manifest belongs to another Unified playlist"
    );
    let current = local
        .items
        .iter()
        .map(|item| (item.entry_id, item))
        .collect::<HashMap<_, _>>();
    let mut items = Vec::with_capacity(manifest.entries.len());
    for entry in &manifest.entries {
        let media_id = MediaId {
            provider: parse_provider(&entry.provider)
                .ok_or_else(|| anyhow::anyhow!("unsupported provider in ListenBrainz manifest"))?,
            kind: parse_media_kind(&entry.kind).ok_or_else(|| {
                anyhow::anyhow!("unsupported media kind in ListenBrainz manifest")
            })?,
            raw_id: entry.id.clone(),
        };
        let unresolved = entry.projection.as_ref().is_none_or(|projection| {
            projection.status == DescriptionManifestProjectionStatus::Unresolved
        });
        let mut item = current
            .get(&PlaylistEntryId(entry.occurrence))
            .filter(|item| item.media_id == media_id)
            .map_or_else(
                || UnifiedPlaylistItem {
                    entry_id: PlaylistEntryId(entry.occurrence),
                    media_id: media_id.clone(),
                    title: String::new(),
                    artists: String::new(),
                    duration_ms: None,
                    duration_unit: DurationUnit::Milliseconds,
                    provider_url: None,
                    metadata: UnifiedPlaylistMetadata {
                        provenance: Some("listenbrainz-manifest".to_owned()),
                        observed_at: Some(observed_at),
                        ..UnifiedPlaylistMetadata::default()
                    },
                },
                |item| (*item).clone(),
            );
        item.entry_id = PlaylistEntryId(entry.occurrence);
        item.media_id = media_id;
        if unresolved {
            item.provider_url = None;
            item.metadata.provenance = Some("listenbrainz-manifest-unresolved".to_owned());
            item.metadata.observed_at = Some(observed_at);
            item.metadata.degraded = true;
        }
        items.push(item);
    }
    let max_remote_entry = items.iter().map(|item| item.entry_id.0).max().unwrap_or(0);
    let mut playlist = UnifiedPlaylist {
        id: manifest.playlist_id,
        name,
        items,
        updated_at: observed_at,
        next_entry_id: local.next_entry_id.max(max_remote_entry.saturating_add(1)),
    };
    playlist.normalize_entry_ids()?;
    anyhow::ensure!(
        playlist.snapshot_hash() == manifest.snapshot_hash,
        "ListenBrainz pull did not reproduce the manifest snapshot"
    );
    Ok(playlist)
}

#[cfg(test)]
mod tests {
    use super::{execute_pull_apply, remote_playlist_from_manifest};
    use crate::cli::listenbrainz_manifest::{
        description_envelope_from_manifest, DescriptionManifest, DescriptionManifestEntry,
        DescriptionManifestProjection, DescriptionManifestProjectionStatus,
    };
    use crate::state::{
        AppData, ListenBrainzSyncBase, ListenBrainzSyncState, ListenBrainzSyncStatus, MediaId,
        MediaKind, PlaylistEntryId, PlaylistLink, Provider, UnifiedPlaylist, UnifiedPlaylistItem,
    };

    fn item(occurrence: u64, raw_id: &str) -> UnifiedPlaylistItem {
        UnifiedPlaylistItem {
            entry_id: PlaylistEntryId(occurrence),
            media_id: MediaId {
                provider: Provider::Spotify,
                kind: MediaKind::Track,
                raw_id: raw_id.to_owned(),
            },
            title: format!("title-{occurrence}"),
            ..UnifiedPlaylistItem::default()
        }
    }

    #[test]
    fn remote_manifest_preserves_occurrence_order_duplicates_and_gates_unresolved_rows() {
        let local = UnifiedPlaylist {
            id: "local-1".to_owned(),
            name: "before".to_owned(),
            items: vec![item(7, "same"), item(8, "removed")],
            next_entry_id: 20,
            ..UnifiedPlaylist::default()
        };
        let expected = UnifiedPlaylist {
            id: "local-1".to_owned(),
            name: "after".to_owned(),
            items: vec![item(9, "same"), item(7, "same")],
            next_entry_id: 20,
            ..UnifiedPlaylist::default()
        };
        let manifest = DescriptionManifest {
            schema_version: 2,
            playlist_id: "local-1".to_owned(),
            snapshot_hash: expected.snapshot_hash(),
            entries: vec![
                DescriptionManifestEntry {
                    occurrence: 9,
                    provider: "spotify".to_owned(),
                    kind: "track".to_owned(),
                    id: "same".to_owned(),
                    projection: Some(DescriptionManifestProjection {
                        status: DescriptionManifestProjectionStatus::Unresolved,
                        recording_mbid: None,
                    }),
                },
                DescriptionManifestEntry {
                    occurrence: 7,
                    provider: "spotify".to_owned(),
                    kind: "track".to_owned(),
                    id: "same".to_owned(),
                    projection: Some(DescriptionManifestProjection {
                        status: DescriptionManifestProjectionStatus::Resolved,
                        recording_mbid: Some("12345678-1234-1234-1234-123456789abc".to_owned()),
                    }),
                },
            ],
        };
        let remote = serde_json::json!({
            "playlist": {
                "title": "after",
                "annotation": description_envelope_from_manifest(&manifest).unwrap()
            }
        });

        let pulled = remote_playlist_from_manifest(&remote, &local, 42).unwrap();

        assert_eq!(
            pulled
                .items
                .iter()
                .map(|item| item.entry_id.0)
                .collect::<Vec<_>>(),
            vec![9, 7]
        );
        assert_eq!(pulled.items[0].media_id, pulled.items[1].media_id);
        assert!(pulled.items[0].playable_media().is_none());
        assert!(pulled.items[1].playable_media().is_some());
        assert_eq!(pulled.items[1].title, "title-7");
        assert_eq!(pulled.next_entry_id, 20);
    }

    #[test]
    fn explicit_pull_apply_persists_remote_only_occurrence_and_verified_base() {
        let folder = std::env::temp_dir().join(format!(
            "unified-player-listenbrainz-pull-apply-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&folder).unwrap();
        let local = UnifiedPlaylist {
            id: "local-1".to_owned(),
            name: "before".to_owned(),
            next_entry_id: 8,
            ..UnifiedPlaylist::default()
        };
        let base = ListenBrainzSyncBase {
            manifest_schema_version: 2,
            unified_playlist_id: local.id.clone(),
            playlist_name: local.name.clone(),
            local_snapshot_hash: local.snapshot_hash(),
            canonical_manifest_hash: "2".repeat(64),
            remote_fingerprint: "3".repeat(64),
            entries: Vec::new(),
            verified_at: 10,
        };
        let expected = UnifiedPlaylist {
            id: "local-1".to_owned(),
            name: "after".to_owned(),
            items: vec![item(9, "remote-track")],
            next_entry_id: 10,
            ..UnifiedPlaylist::default()
        };
        let manifest = DescriptionManifest {
            schema_version: 2,
            playlist_id: "local-1".to_owned(),
            snapshot_hash: expected.snapshot_hash(),
            entries: vec![DescriptionManifestEntry {
                occurrence: 9,
                provider: "spotify".to_owned(),
                kind: "track".to_owned(),
                id: "remote-track".to_owned(),
                projection: Some(DescriptionManifestProjection {
                    status: DescriptionManifestProjectionStatus::Unresolved,
                    recording_mbid: None,
                }),
            }],
        };
        let remote = serde_json::json!({
            "playlist": {
                "title": "after",
                "annotation": description_envelope_from_manifest(&manifest).unwrap(),
                "track": []
            }
        });
        let mut data = AppData::new(&folder, &folder);
        data.upsert_unified_playlist(local).unwrap();
        data.upsert_playlist_link(PlaylistLink {
            unified_playlist_id: "local-1".to_owned(),
            listenbrainz_playlist_id: Some("remote-1".to_owned()),
            listenbrainz_sync: Some(ListenBrainzSyncState::verified(base).unwrap()),
            ..PlaylistLink::default()
        })
        .unwrap();

        let (_, result) =
            execute_pull_apply(&mut data, "remote-1", "local-1", &remote, "pull-1", 20).unwrap();

        assert!(result.applied);
        assert_eq!(result.occurrences, 1);
        assert_eq!(result.unresolved, 1);
        assert!(data.unified_playlists[0].items[0]
            .playable_media()
            .is_none());
        let sync = data.playlist_links[0].listenbrainz_sync.as_ref().unwrap();
        assert_eq!(sync.status, ListenBrainzSyncStatus::Clean);
        assert_eq!(sync.base.local_snapshot_hash, manifest.snapshot_hash);
        assert!(sync.local_apply_snapshot.is_some());
        let restarted = AppData::new(&folder, &folder);
        assert_eq!(restarted.unified_playlists[0].items[0].entry_id.0, 9);
        assert!(restarted.playlist_links[0]
            .listenbrainz_sync
            .as_ref()
            .unwrap()
            .local_apply_snapshot
            .is_some());
        std::fs::remove_dir_all(folder).unwrap();
    }
}
