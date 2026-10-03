use reqwest::{header, StatusCode};
use serde::Serialize;

use crate::cli::listenbrainz_manifest::{
    description_envelope_from_manifest, description_manifest_fingerprint,
    DESCRIPTION_CHARACTER_BUDGET,
};
use crate::state::{
    AppData, ListenBrainzRecoveryDisposition, ListenBrainzSyncBase, ListenBrainzSyncIntent,
    ListenBrainzSyncStatus, UnifiedPlaylist,
};

use super::listenbrainz_projection::{NativeJspfTrack, NativeProjection};
use super::listenbrainz_sync::{
    build_remote_anchored_plan, verified_sync_state_from_remote, PlanStatus,
};

const LISTENBRAINZ_API_ROOT: &str = "https://api.listenbrainz.org/1";
const MAX_RECORDINGS_PER_ADD: usize = 100;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum PushMutationStage {
    DeleteRecordings,
    AddRecordings,
    UpdateAnnotation,
    ReadBack,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum PushTransactionStatus {
    Verified,
    GuardRejected,
    Rejected,
    Partial,
    OutcomeUnknown,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub(crate) struct PushTransactionResult {
    pub(crate) status: PushTransactionStatus,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) stage: Option<PushMutationStage>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) http_status: Option<u16>,
    pub(crate) writes_started: bool,
}

impl PushTransactionResult {
    pub(crate) const fn next_action(&self) -> &'static str {
        match self.status {
            PushTransactionStatus::Verified => "none",
            PushTransactionStatus::GuardRejected => {
                "refresh the plan and verify the remote fingerprint before applying"
            }
            PushTransactionStatus::Rejected => {
                "verify the remote playlist before deciding whether to clear or recover the intent"
            }
            PushTransactionStatus::Partial => {
                "run listenbrainz recover to verify the remote target"
            }
            PushTransactionStatus::OutcomeUnknown => {
                "run listenbrainz recover; do not retry the push"
            }
        }
    }
}

#[derive(Debug)]
enum AdapterFailure {
    Rejected {
        stage: PushMutationStage,
        status: StatusCode,
        changed: bool,
    },
    OutcomeUnknown {
        stage: PushMutationStage,
    },
}

pub(crate) struct ListenBrainzMutationAdapter {
    http: reqwest::Client,
    api_root: String,
}

impl ListenBrainzMutationAdapter {
    pub(crate) fn new(http: reqwest::Client) -> Self {
        Self::with_api_root(http, LISTENBRAINZ_API_ROOT)
    }

    pub(crate) fn with_api_root(http: reqwest::Client, api_root: impl Into<String>) -> Self {
        Self {
            http,
            api_root: api_root.into().trim_end_matches('/').to_owned(),
        }
    }

    pub(crate) async fn fetch(
        &self,
        token: &str,
        playlist_id: &str,
    ) -> anyhow::Result<serde_json::Value> {
        let response = self
            .http
            .get(format!("{}/playlist/{playlist_id}", self.api_root))
            .header(header::AUTHORIZATION, format!("Token {token}"))
            .send()
            .await?;
        anyhow::ensure!(
            response.status().is_success(),
            "ListenBrainz playlist read-back was rejected"
        );
        Ok(response.json().await?)
    }

    async fn replace_target(
        &self,
        token: &str,
        playlist_id: &str,
        existing_count: usize,
        annotation: &str,
        tracks: &[NativeJspfTrack],
    ) -> Result<(), AdapterFailure> {
        let mut changed = false;
        if existing_count > 0 {
            self.post(
                token,
                format!("{}/playlist/{playlist_id}/item/delete", self.api_root),
                &serde_json::json!({"index": 0, "count": existing_count}),
                PushMutationStage::DeleteRecordings,
                changed,
            )
            .await?;
            changed = true;
        }
        for chunk in tracks.chunks(MAX_RECORDINGS_PER_ADD) {
            self.post(
                token,
                format!("{}/playlist/{playlist_id}/item/add", self.api_root),
                &serde_json::json!({"playlist": {"track": chunk}}),
                PushMutationStage::AddRecordings,
                changed,
            )
            .await?;
            changed = true;
        }
        self.post(
            token,
            format!("{}/playlist/edit/{playlist_id}", self.api_root),
            &serde_json::json!({"playlist": {"annotation": annotation}}),
            PushMutationStage::UpdateAnnotation,
            changed,
        )
        .await?;
        Ok(())
    }

    async fn post(
        &self,
        token: &str,
        url: String,
        body: &serde_json::Value,
        stage: PushMutationStage,
        changed: bool,
    ) -> Result<(), AdapterFailure> {
        let response = self
            .http
            .post(url)
            .header(header::AUTHORIZATION, format!("Token {token}"))
            .json(body)
            .send()
            .await
            .map_err(|_| AdapterFailure::OutcomeUnknown { stage })?;
        if !response.status().is_success() {
            return Err(AdapterFailure::Rejected {
                stage,
                status: response.status(),
                changed,
            });
        }
        Ok(())
    }
}

#[allow(clippy::too_many_arguments)]
pub(crate) async fn execute_push_transaction(
    adapter: &ListenBrainzMutationAdapter,
    data: &mut AppData,
    token: &str,
    remote_playlist_id: &str,
    local_playlist: &UnifiedPlaylist,
    projection: &NativeProjection,
    operation_id: &str,
    started_at: u64,
) -> anyhow::Result<PushTransactionResult> {
    execute_push_transaction_with_guard(
        adapter,
        data,
        token,
        remote_playlist_id,
        local_playlist,
        projection,
        operation_id,
        started_at,
        None,
    )
    .await
}

#[allow(clippy::too_many_arguments)]
pub(crate) async fn execute_push_transaction_with_guard(
    adapter: &ListenBrainzMutationAdapter,
    data: &mut AppData,
    token: &str,
    remote_playlist_id: &str,
    local_playlist: &UnifiedPlaylist,
    projection: &NativeProjection,
    operation_id: &str,
    started_at: u64,
    resolved_remote_fingerprint: Option<&str>,
) -> anyhow::Result<PushTransactionResult> {
    anyhow::ensure!(
        projection.manifest.playlist_id == local_playlist.id
            && projection.manifest.snapshot_hash == local_playlist.snapshot_hash(),
        "ListenBrainz push target is stale for the local playlist"
    );
    let annotation = description_envelope_from_manifest(&projection.manifest)?;
    anyhow::ensure!(
        annotation.chars().count() <= DESCRIPTION_CHARACTER_BUDGET,
        "ListenBrainz push target exceeds the annotation budget"
    );
    let sync = data
        .playlist_links
        .iter()
        .find(|link| link.unified_playlist_id == local_playlist.id)
        .and_then(|link| link.listenbrainz_sync.as_ref())
        .ok_or_else(|| anyhow::anyhow!("ListenBrainz sync base is not initialized"))?;
    anyhow::ensure!(
        sync.pending_intent.is_none(),
        "a ListenBrainz operation is already pending"
    );
    let expected_remote_fingerprint = resolved_remote_fingerprint
        .unwrap_or(&sync.base.remote_fingerprint)
        .to_owned();

    let Ok(current_remote) = adapter.fetch(token, remote_playlist_id).await else {
        return Ok(PushTransactionResult {
            status: PushTransactionStatus::GuardRejected,
            stage: None,
            http_status: None,
            writes_started: false,
        });
    };
    let guard_matches = if resolved_remote_fingerprint.is_some() {
        remote_playlist_identity_matches(&current_remote, remote_playlist_id)
            && build_remote_anchored_plan(
                remote_playlist_id,
                local_playlist,
                &current_remote,
                Some(&expected_remote_fingerprint),
            )
            .status
                == PlanStatus::Ready
    } else {
        verified_base_from_readback(
            remote_playlist_id,
            local_playlist,
            &current_remote,
            Some(&expected_remote_fingerprint),
            started_at,
        )
        .is_ok()
    };
    if !guard_matches {
        return Ok(PushTransactionResult {
            status: PushTransactionStatus::GuardRejected,
            stage: None,
            http_status: None,
            writes_started: false,
        });
    }
    let target_manifest_hash = description_manifest_fingerprint(&projection.manifest)?;
    let intent = ListenBrainzSyncIntent {
        operation_id: operation_id.to_owned(),
        expected_remote_fingerprint: expected_remote_fingerprint.clone(),
        target_manifest_hash: target_manifest_hash.clone(),
        target_local_snapshot_hash: local_playlist.snapshot_hash(),
        started_at,
    };
    if resolved_remote_fingerprint.is_some() {
        data.begin_listenbrainz_resolution_intent(
            &local_playlist.id,
            remote_playlist_id,
            intent,
            &expected_remote_fingerprint,
        )?;
    } else {
        data.begin_listenbrainz_sync_intent(&local_playlist.id, remote_playlist_id, intent)?;
    }

    let existing_count = remote_tracks(&current_remote).len();
    if let Err(failure) = adapter
        .replace_target(
            token,
            remote_playlist_id,
            existing_count,
            &annotation,
            &projection.tracks,
        )
        .await
    {
        return record_adapter_failure(
            data,
            &local_playlist.id,
            remote_playlist_id,
            failure,
            started_at,
        );
    }

    let Ok(read_back) = adapter.fetch(token, remote_playlist_id).await else {
        data.record_listenbrainz_sync_outcome(
            &local_playlist.id,
            remote_playlist_id,
            ListenBrainzSyncStatus::OutcomeUnknown,
            ListenBrainzRecoveryDisposition::OutcomeUnknown,
            started_at,
        )?;
        return Ok(PushTransactionResult {
            status: PushTransactionStatus::OutcomeUnknown,
            stage: Some(PushMutationStage::ReadBack),
            http_status: None,
            writes_started: true,
        });
    };
    let verified = match verified_base_from_readback(
        remote_playlist_id,
        local_playlist,
        &read_back,
        None,
        started_at,
    ) {
        Ok(verified) if verified.canonical_manifest_hash == target_manifest_hash => verified,
        Ok(_) | Err(_) => {
            return record_verification_failure(
                data,
                local_playlist,
                remote_playlist_id,
                started_at,
            )
        }
    };
    let disposition = data.recover_listenbrainz_sync_intent(
        &local_playlist.id,
        remote_playlist_id,
        Some(verified),
        started_at,
    )?;
    anyhow::ensure!(
        disposition == ListenBrainzRecoveryDisposition::AlreadyApplied,
        "verified ListenBrainz read-back did not finalize the pending target"
    );
    Ok(PushTransactionResult {
        status: PushTransactionStatus::Verified,
        stage: Some(PushMutationStage::ReadBack),
        http_status: None,
        writes_started: true,
    })
}

pub(crate) fn verified_base_from_readback(
    remote_playlist_id: &str,
    local_playlist: &UnifiedPlaylist,
    remote: &serde_json::Value,
    expected_remote_fingerprint: Option<&str>,
    verified_at: u64,
) -> anyhow::Result<ListenBrainzSyncBase> {
    anyhow::ensure!(
        remote_playlist_identity_matches(remote, remote_playlist_id),
        "ListenBrainz read-back playlist identity does not match"
    );
    Ok(verified_sync_state_from_remote(
        remote_playlist_id,
        local_playlist,
        remote,
        expected_remote_fingerprint,
        verified_at,
    )?
    .base)
}

fn record_adapter_failure(
    data: &mut AppData,
    unified_playlist_id: &str,
    remote_playlist_id: &str,
    failure: AdapterFailure,
    observed_at: u64,
) -> anyhow::Result<PushTransactionResult> {
    let (status, stage, http_status, disposition, sync_status) = match failure {
        AdapterFailure::Rejected {
            stage,
            status,
            changed,
        } if !changed => (
            PushTransactionStatus::Rejected,
            stage,
            Some(status.as_u16()),
            ListenBrainzRecoveryDisposition::Rejected,
            ListenBrainzSyncStatus::Partial,
        ),
        AdapterFailure::Rejected { stage, status, .. } => (
            PushTransactionStatus::Partial,
            stage,
            Some(status.as_u16()),
            ListenBrainzRecoveryDisposition::Partial,
            ListenBrainzSyncStatus::Partial,
        ),
        AdapterFailure::OutcomeUnknown { stage } => (
            PushTransactionStatus::OutcomeUnknown,
            stage,
            None,
            ListenBrainzRecoveryDisposition::OutcomeUnknown,
            ListenBrainzSyncStatus::OutcomeUnknown,
        ),
    };
    data.record_listenbrainz_sync_outcome(
        unified_playlist_id,
        remote_playlist_id,
        sync_status,
        disposition,
        observed_at,
    )?;
    Ok(PushTransactionResult {
        status,
        stage: Some(stage),
        http_status,
        writes_started: true,
    })
}

fn record_verification_failure(
    data: &mut AppData,
    local_playlist: &UnifiedPlaylist,
    remote_playlist_id: &str,
    observed_at: u64,
) -> anyhow::Result<PushTransactionResult> {
    data.record_listenbrainz_sync_outcome(
        &local_playlist.id,
        remote_playlist_id,
        ListenBrainzSyncStatus::Partial,
        ListenBrainzRecoveryDisposition::VerificationFailed,
        observed_at,
    )?;
    Ok(PushTransactionResult {
        status: PushTransactionStatus::Partial,
        stage: Some(PushMutationStage::ReadBack),
        http_status: None,
        writes_started: true,
    })
}

fn remote_tracks(remote: &serde_json::Value) -> &[serde_json::Value] {
    remote
        .get("playlist")
        .unwrap_or(remote)
        .get("track")
        .and_then(serde_json::Value::as_array)
        .map(Vec::as_slice)
        .unwrap_or_default()
}

fn remote_playlist_identity_matches(remote: &serde_json::Value, playlist_id: &str) -> bool {
    let identifier = remote.get("playlist").unwrap_or(remote).get("identifier");
    match identifier {
        Some(serde_json::Value::String(value)) => value.rsplit('/').next() == Some(playlist_id),
        Some(serde_json::Value::Array(values)) => values.iter().any(|value| {
            value.as_str().and_then(|value| value.rsplit('/').next()) == Some(playlist_id)
        }),
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use std::io::{Read as _, Write as _};
    use std::path::PathBuf;
    use std::time::Duration;

    use super::{
        execute_push_transaction, execute_push_transaction_with_guard, ListenBrainzMutationAdapter,
        PushTransactionStatus,
    };
    use crate::cli::listenbrainz_manifest::description_envelope_from_manifest;
    use crate::client::listenbrainz_projection::{
        build_native_projection, ExplicitRecordingRelation, NativeProjection,
    };
    use crate::client::listenbrainz_sync::verified_sync_state_from_remote;
    use crate::state::{
        AppData, ListenBrainzRecoveryDisposition, ListenBrainzSyncStatus, MediaId, MediaKind,
        PlaylistEntryId, PlaylistLink, Provider, UnifiedPlaylist, UnifiedPlaylistItem,
    };

    fn playlist() -> UnifiedPlaylist {
        let media_id = MediaId {
            provider: Provider::Spotify,
            kind: MediaKind::Track,
            raw_id: "same-track".to_owned(),
        };
        UnifiedPlaylist {
            id: "local".to_owned(),
            name: "Mix".to_owned(),
            items: vec![
                UnifiedPlaylistItem {
                    entry_id: PlaylistEntryId(1),
                    media_id: media_id.clone(),
                    ..UnifiedPlaylistItem::default()
                },
                UnifiedPlaylistItem {
                    entry_id: PlaylistEntryId(2),
                    media_id,
                    ..UnifiedPlaylistItem::default()
                },
            ],
            next_entry_id: 3,
            ..UnifiedPlaylist::default()
        }
    }

    fn target_projection(playlist: &UnifiedPlaylist) -> NativeProjection {
        build_native_projection(
            playlist,
            &[ExplicitRecordingRelation {
                media_id: playlist.items[0].media_id.clone(),
                recording_mbid: "12345678-1234-1234-1234-123456789abc".to_owned(),
            }],
        )
        .unwrap()
    }

    fn remote_value(projection: &NativeProjection, last_modified_at: &str) -> serde_json::Value {
        serde_json::json!({
            "playlist": {
                "identifier": "https://listenbrainz.org/playlist/remote",
                "title": "Mix",
                "annotation": description_envelope_from_manifest(&projection.manifest).unwrap(),
                "track": projection.tracks,
                "extension": {
                    "https://musicbrainz.org/doc/jspf#playlist": {
                        "last_modified_at": last_modified_at
                    }
                }
            }
        })
    }

    fn temporary_folder(name: &str) -> PathBuf {
        std::env::temp_dir().join(format!(
            "unified-player-{name}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ))
    }

    fn data_with_base(
        folder: &PathBuf,
        playlist: &UnifiedPlaylist,
        remote: &serde_json::Value,
    ) -> AppData {
        std::fs::create_dir_all(folder).unwrap();
        let mut data = AppData::new(folder, folder);
        data.upsert_unified_playlist(playlist.clone()).unwrap();
        let sync = verified_sync_state_from_remote("remote", playlist, remote, None, 1).unwrap();
        data.upsert_playlist_link(PlaylistLink {
            unified_playlist_id: playlist.id.clone(),
            listenbrainz_playlist_id: Some("remote".to_owned()),
            listenbrainz_sync: Some(sync),
            ..PlaylistLink::default()
        })
        .unwrap();
        data
    }

    fn serve_sequence(
        responses: Vec<Option<(u16, String)>>,
    ) -> (
        String,
        std::sync::mpsc::Receiver<String>,
        std::thread::JoinHandle<()>,
    ) {
        serve_sequence_with_intent_check(responses, None)
    }

    fn serve_sequence_with_intent_check(
        responses: Vec<Option<(u16, String)>>,
        intent_store: Option<PathBuf>,
    ) -> (
        String,
        std::sync::mpsc::Receiver<String>,
        std::thread::JoinHandle<()>,
    ) {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let (sender, receiver) = std::sync::mpsc::channel();
        let thread = std::thread::spawn(move || {
            let mut intent_checked = false;
            for response in responses {
                let (mut socket, _) = listener.accept().unwrap();
                socket
                    .set_read_timeout(Some(Duration::from_secs(5)))
                    .unwrap();
                let request = read_request(&mut socket);
                if !intent_checked && request.starts_with("POST ") {
                    if let Some(folder) = &intent_store {
                        let store: serde_json::Value = serde_json::from_slice(
                            &std::fs::read(folder.join("unified_playlists.json")).unwrap(),
                        )
                        .unwrap();
                        assert_eq!(
                            store["links"][0]["listenbrainz_sync"]["pending_intent"]
                                ["operation_id"],
                            "operation-1"
                        );
                    }
                    intent_checked = true;
                }
                sender.send(request).unwrap();
                let Some((status, body)) = response else {
                    continue;
                };
                let reason = if status == 200 { "OK" } else { "Rejected" };
                write!(
                    socket,
                    "HTTP/1.1 {status} {reason}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                    body.len()
                )
                .unwrap();
                socket.write_all(body.as_bytes()).unwrap();
                socket.flush().unwrap();
            }
        });
        (format!("http://{address}/1"), receiver, thread)
    }

    fn read_request(socket: &mut std::net::TcpStream) -> String {
        let mut request = Vec::new();
        let mut buffer = [0_u8; 4096];
        let header_end = loop {
            let read = socket.read(&mut buffer).unwrap();
            assert!(read > 0);
            request.extend_from_slice(&buffer[..read]);
            if let Some(index) = request.windows(4).position(|window| window == b"\r\n\r\n") {
                break index + 4;
            }
        };
        let headers = std::str::from_utf8(&request[..header_end]).unwrap();
        let content_length = headers
            .lines()
            .find_map(|line| {
                let (name, value) = line.split_once(':')?;
                name.eq_ignore_ascii_case("content-length")
                    .then(|| value.trim().parse::<usize>().unwrap())
            })
            .unwrap_or_default();
        while request.len() < header_end + content_length {
            let read = socket.read(&mut buffer).unwrap();
            assert!(read > 0);
            request.extend_from_slice(&buffer[..read]);
        }
        String::from_utf8(request).unwrap()
    }

    #[tokio::test]
    async fn mock_transaction_persists_intent_before_write_and_advances_after_readback() {
        let playlist = playlist();
        let base_projection = build_native_projection(&playlist, &[]).unwrap();
        let target = target_projection(&playlist);
        let base_remote = remote_value(&base_projection, "1");
        let target_remote = remote_value(&target, "2");
        let folder = temporary_folder("listenbrainz-push-verified");
        let mut data = data_with_base(&folder, &playlist, &base_remote);
        let (root, requests, server) = serve_sequence_with_intent_check(
            vec![
                Some((200, base_remote.to_string())),
                Some((200, "{}".to_owned())),
                Some((200, "{}".to_owned())),
                Some((200, target_remote.to_string())),
            ],
            Some(folder.clone()),
        );
        let adapter = ListenBrainzMutationAdapter::with_api_root(reqwest::Client::new(), root);

        let result = execute_push_transaction(
            &adapter,
            &mut data,
            "secret",
            "remote",
            &playlist,
            &target,
            "operation-1",
            10,
        )
        .await
        .unwrap();

        assert_eq!(result.status, PushTransactionStatus::Verified);
        let captured = (0..4)
            .map(|_| requests.recv_timeout(Duration::from_secs(5)).unwrap())
            .collect::<Vec<_>>();
        server.join().unwrap();
        assert!(captured[0].starts_with("GET /1/playlist/remote "));
        assert!(captured[1].starts_with("POST /1/playlist/remote/item/add "));
        assert!(captured[2].starts_with("POST /1/playlist/edit/remote "));
        assert!(captured[3].starts_with("GET /1/playlist/remote "));
        assert_eq!(captured[1].matches("musicbrainz.org/recording").count(), 2);
        let sync = data.playlist_links[0].listenbrainz_sync.as_ref().unwrap();
        assert_eq!(sync.status, ListenBrainzSyncStatus::Clean);
        assert!(sync.pending_intent.is_none());
        assert_eq!(
            sync.base.canonical_manifest_hash,
            crate::cli::listenbrainz_manifest::description_manifest_fingerprint(&target.manifest)
                .unwrap()
        );
        std::fs::remove_dir_all(folder).unwrap();
    }

    #[tokio::test]
    async fn stale_mock_readback_rejects_before_intent_or_mutation() {
        let playlist = playlist();
        let base_projection = build_native_projection(&playlist, &[]).unwrap();
        let target = target_projection(&playlist);
        let base_remote = remote_value(&base_projection, "1");
        let stale_remote = remote_value(&base_projection, "2");
        let folder = temporary_folder("listenbrainz-push-stale");
        let mut data = data_with_base(&folder, &playlist, &base_remote);
        let (root, requests, server) = serve_sequence(vec![Some((200, stale_remote.to_string()))]);
        let adapter = ListenBrainzMutationAdapter::with_api_root(reqwest::Client::new(), root);

        let result = execute_push_transaction(
            &adapter,
            &mut data,
            "secret",
            "remote",
            &playlist,
            &target,
            "operation-1",
            10,
        )
        .await
        .unwrap();

        assert_eq!(result.status, PushTransactionStatus::GuardRejected);
        assert!(!result.writes_started);
        assert!(requests
            .recv_timeout(Duration::from_secs(5))
            .unwrap()
            .starts_with("GET /1/playlist/remote "));
        server.join().unwrap();
        assert!(data.playlist_links[0]
            .listenbrainz_sync
            .as_ref()
            .unwrap()
            .pending_intent
            .is_none());
        std::fs::remove_dir_all(folder).unwrap();
    }

    #[tokio::test]
    async fn explicit_resolution_guard_can_write_only_the_previewed_changed_remote() {
        let playlist = playlist();
        let base_projection = build_native_projection(&playlist, &[]).unwrap();
        let target = target_projection(&playlist);
        let base_remote = remote_value(&base_projection, "1");
        let changed_remote = remote_value(&base_projection, "2");
        let target_remote = remote_value(&target, "3");
        let expected =
            verified_sync_state_from_remote("remote", &playlist, &changed_remote, None, 9)
                .unwrap()
                .base
                .remote_fingerprint;
        let folder = temporary_folder("listenbrainz-resolution-guard");
        let mut data = data_with_base(&folder, &playlist, &base_remote);
        let (root, requests, server) = serve_sequence(vec![
            Some((200, changed_remote.to_string())),
            Some((200, "{}".to_owned())),
            Some((200, "{}".to_owned())),
            Some((200, target_remote.to_string())),
        ]);
        let adapter = ListenBrainzMutationAdapter::with_api_root(reqwest::Client::new(), root);

        let result = execute_push_transaction_with_guard(
            &adapter,
            &mut data,
            "secret",
            "remote",
            &playlist,
            &target,
            "operation-1",
            10,
            Some(&expected),
        )
        .await
        .unwrap();

        assert_eq!(result.status, PushTransactionStatus::Verified);
        assert_eq!(
            (0..4)
                .map(|_| requests.recv_timeout(Duration::from_secs(5)).unwrap())
                .count(),
            4
        );
        server.join().unwrap();
        let sync = data.playlist_links[0].listenbrainz_sync.as_ref().unwrap();
        assert_eq!(sync.status, ListenBrainzSyncStatus::Clean);
        assert!(sync.pending_intent.is_none());
        std::fs::remove_dir_all(folder).unwrap();
    }

    #[tokio::test]
    async fn mock_verification_mismatch_is_partial_and_keeps_base_and_intent() {
        let playlist = playlist();
        let base_projection = build_native_projection(&playlist, &[]).unwrap();
        let target = target_projection(&playlist);
        let base_remote = remote_value(&base_projection, "1");
        let folder = temporary_folder("listenbrainz-push-partial");
        let mut data = data_with_base(&folder, &playlist, &base_remote);
        let original_base = data.playlist_links[0]
            .listenbrainz_sync
            .as_ref()
            .unwrap()
            .base
            .clone();
        let (root, _requests, server) = serve_sequence(vec![
            Some((200, base_remote.to_string())),
            Some((200, "{}".to_owned())),
            Some((200, "{}".to_owned())),
            Some((200, base_remote.to_string())),
        ]);
        let adapter = ListenBrainzMutationAdapter::with_api_root(reqwest::Client::new(), root);

        let result = execute_push_transaction(
            &adapter,
            &mut data,
            "secret",
            "remote",
            &playlist,
            &target,
            "operation-1",
            10,
        )
        .await
        .unwrap();
        server.join().unwrap();

        assert_eq!(result.status, PushTransactionStatus::Partial);
        let sync = data.playlist_links[0].listenbrainz_sync.as_ref().unwrap();
        assert_eq!(sync.status, ListenBrainzSyncStatus::Partial);
        assert!(sync.pending_intent.is_some());
        assert_eq!(sync.base, original_base);
        assert_eq!(
            sync.recovery.as_ref().unwrap().disposition,
            ListenBrainzRecoveryDisposition::VerificationFailed
        );
        std::fs::remove_dir_all(folder).unwrap();
    }

    #[tokio::test]
    async fn ambiguous_mock_mutation_is_outcome_unknown_and_is_not_retried() {
        let playlist = playlist();
        let base_projection = build_native_projection(&playlist, &[]).unwrap();
        let target = target_projection(&playlist);
        let base_remote = remote_value(&base_projection, "1");
        let folder = temporary_folder("listenbrainz-push-unknown");
        let mut data = data_with_base(&folder, &playlist, &base_remote);
        let (root, requests, server) =
            serve_sequence(vec![Some((200, base_remote.to_string())), None]);
        let adapter = ListenBrainzMutationAdapter::with_api_root(reqwest::Client::new(), root);

        let result = execute_push_transaction(
            &adapter,
            &mut data,
            "secret",
            "remote",
            &playlist,
            &target,
            "operation-1",
            10,
        )
        .await
        .unwrap();
        let captured = (0..2)
            .map(|_| requests.recv_timeout(Duration::from_secs(5)).unwrap())
            .collect::<Vec<_>>();
        server.join().unwrap();

        assert_eq!(result.status, PushTransactionStatus::OutcomeUnknown);
        assert_eq!(captured.len(), 2);
        let sync = data.playlist_links[0].listenbrainz_sync.as_ref().unwrap();
        assert_eq!(sync.status, ListenBrainzSyncStatus::OutcomeUnknown);
        assert!(sync.pending_intent.is_some());
        assert_eq!(
            sync.recovery.as_ref().unwrap().disposition,
            ListenBrainzRecoveryDisposition::OutcomeUnknown
        );
        std::fs::remove_dir_all(folder).unwrap();
    }

    #[tokio::test]
    async fn known_mock_rejection_is_not_reported_as_success() {
        let playlist = playlist();
        let base_projection = build_native_projection(&playlist, &[]).unwrap();
        let target = target_projection(&playlist);
        let base_remote = remote_value(&base_projection, "1");
        let folder = temporary_folder("listenbrainz-push-rejected");
        let mut data = data_with_base(&folder, &playlist, &base_remote);
        let (root, _requests, server) = serve_sequence(vec![
            Some((200, base_remote.to_string())),
            Some((403, "{}".to_owned())),
        ]);
        let adapter = ListenBrainzMutationAdapter::with_api_root(reqwest::Client::new(), root);

        let result = execute_push_transaction(
            &adapter,
            &mut data,
            "secret",
            "remote",
            &playlist,
            &target,
            "operation-1",
            10,
        )
        .await
        .unwrap();
        server.join().unwrap();

        assert_eq!(result.status, PushTransactionStatus::Rejected);
        assert_eq!(result.http_status, Some(403));
        let sync = data.playlist_links[0].listenbrainz_sync.as_ref().unwrap();
        assert_eq!(sync.status, ListenBrainzSyncStatus::Partial);
        assert_eq!(
            sync.recovery.as_ref().unwrap().disposition,
            ListenBrainzRecoveryDisposition::Rejected
        );
        std::fs::remove_dir_all(folder).unwrap();
    }

    #[test]
    fn lifecycle_results_expose_one_safe_next_action_without_payload_data() {
        use super::{PushMutationStage, PushTransactionResult};

        let result = PushTransactionResult {
            status: PushTransactionStatus::OutcomeUnknown,
            stage: Some(PushMutationStage::AddRecordings),
            http_status: None,
            writes_started: true,
        };
        assert_eq!(
            result.next_action(),
            "run listenbrainz recover; do not retry the push"
        );
        let json = serde_json::to_string(&result).unwrap();
        assert!(!json.contains("token"));
        assert!(!json.contains("annotation"));
        assert!(!json.contains("identifier"));
        assert!(!json.contains("12345678"));
    }
}
