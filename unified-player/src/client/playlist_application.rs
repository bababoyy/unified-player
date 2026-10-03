//! Application boundary for playlist intent planning and dispatch.
//!
//! Page and popup code should describe a playlist operation here rather than
//! choosing provider request variants itself.  The adapter remains the only
//! owner of provider execution; this module owns correlation and ingress.

use anyhow::{ensure, Result};
use rand::RngExt;

use super::{ClientRequest, ClientRequestSender, PlaylistMutationOperationId, PlaylistRequest};

#[derive(Clone, Debug)]
pub(crate) struct PlaylistOperationPlan {
    pub operation_id: PlaylistMutationOperationId,
    pub request: PlaylistRequest,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum PlaylistApplicationResult {
    Applied {
        operation_id: PlaylistMutationOperationId,
    },
    Failed {
        operation_id: PlaylistMutationOperationId,
    },
}

#[derive(Clone, Copy, Debug, Default)]
pub(crate) struct PlaylistApplicationService;

impl PlaylistApplicationService {
    pub(crate) fn publish(
        operation_id: PlaylistMutationOperationId,
        result: &Result<()>,
    ) -> PlaylistApplicationResult {
        let outcome = if result.is_ok() {
            PlaylistApplicationResult::Applied { operation_id }
        } else {
            PlaylistApplicationResult::Failed { operation_id }
        };
        crate::observability::operation_stage(
            crate::observability::Component::Application,
            "playlist_result_published",
            None,
            Some(if result.is_ok() {
                crate::observability::OperationOutcome::Success
            } else {
                crate::observability::OperationOutcome::Error
            }),
        );
        outcome
    }

    pub(crate) fn from_legacy(request: ClientRequest) -> Result<PlaylistRequest> {
        let operation_id = || {
            let mut value = rand::rng().random();
            while value == 0 {
                value = rand::rng().random();
            }
            PlaylistMutationOperationId(value)
        };
        let request = match request {
            ClientRequest::Playlist(request) => return Ok(request),
            ClientRequest::AddYouTubeTrackToPlaylist {
                operation_id,
                playlist_id,
                track,
            } => PlaylistRequest::new(
                operation_id,
                super::PlaylistRequestKind::AddYouTubeTrack { playlist_id, track },
            ),
            ClientRequest::AddItemsToUnifiedPlaylist {
                playlist_id,
                items,
                operation,
            } => PlaylistRequest::new(
                operation
                    .as_ref()
                    .and_then(|operation| operation.application_operation_id)
                    .map_or_else(operation_id, PlaylistMutationOperationId),
                super::PlaylistRequestKind::AddItemsToUnified {
                    playlist_id,
                    items,
                    operation,
                },
            ),
            ClientRequest::AddPlayableToPlaylist(playlist_id, playable_id) => PlaylistRequest::new(
                operation_id(),
                super::PlaylistRequestKind::AddPlayableToPlaylist {
                    playlist_id,
                    playable_id,
                },
            ),
            ClientRequest::SpotifyPlaylistMutation(intent) => {
                let operation_id = intent.operation_id();
                PlaylistRequest::new(
                    operation_id,
                    super::PlaylistRequestKind::SpotifyMutation(intent),
                )
            }
            ClientRequest::YouTubePlaylistMutation(intent) => {
                let operation_id = intent.operation_id();
                PlaylistRequest::new(
                    operation_id,
                    super::PlaylistRequestKind::YouTubeMutation(intent),
                )
            }
            ClientRequest::CreatePlaylist {
                playlist_name,
                public,
                collab,
                desc,
            } => PlaylistRequest::new(
                operation_id(),
                super::PlaylistRequestKind::CreatePlaylist {
                    playlist_name,
                    public,
                    collab,
                    desc,
                },
            ),
            ClientRequest::CreateSpotifyPlaylistWithTracks {
                playlist_name,
                public,
                collab,
                desc,
                tracks,
            } => PlaylistRequest::new(
                operation_id(),
                super::PlaylistRequestKind::CreateSpotifyPlaylistWithTracks {
                    playlist_name,
                    public,
                    collab,
                    desc,
                    tracks,
                },
            ),
            ClientRequest::CreateYouTubePlaylist {
                playlist_name,
                public,
            } => PlaylistRequest::new(
                operation_id(),
                super::PlaylistRequestKind::CreateYouTubePlaylist {
                    playlist_name,
                    public,
                },
            ),
            ClientRequest::CreateYouTubePlaylistWithTracks {
                playlist_name,
                public,
                tracks,
            } => PlaylistRequest::new(
                operation_id(),
                super::PlaylistRequestKind::CreateYouTubePlaylistWithTracks {
                    playlist_name,
                    public,
                    tracks,
                },
            ),
            ClientRequest::CreateUnifiedPlaylist { playlist_name } => PlaylistRequest::new(
                operation_id(),
                super::PlaylistRequestKind::CreateUnifiedPlaylist { playlist_name },
            ),
            ClientRequest::CreateUnifiedPlaylistWithItems {
                playlist_name,
                items,
                operation,
            } => PlaylistRequest::new(
                operation
                    .as_ref()
                    .and_then(|operation| operation.application_operation_id)
                    .map_or_else(operation_id, PlaylistMutationOperationId),
                super::PlaylistRequestKind::CreateUnifiedPlaylistWithItems {
                    playlist_name,
                    items,
                    operation,
                },
            ),
            ClientRequest::CreateUnifiedPlaylistFromHistory {
                playlist_name,
                items,
            } => PlaylistRequest::new(
                operation_id(),
                super::PlaylistRequestKind::CreateUnifiedPlaylistFromHistory {
                    playlist_name,
                    items,
                },
            ),
            ClientRequest::RenameSpotifyPlaylist { playlist_id, name } => PlaylistRequest::new(
                operation_id(),
                super::PlaylistRequestKind::RenameSpotifyPlaylist { playlist_id, name },
            ),
            ClientRequest::RenameYouTubePlaylist { playlist_id, name } => PlaylistRequest::new(
                operation_id(),
                super::PlaylistRequestKind::RenameYouTubePlaylist { playlist_id, name },
            ),
            ClientRequest::RenameUnifiedPlaylist { playlist_id, name } => PlaylistRequest::new(
                operation_id(),
                super::PlaylistRequestKind::RenameUnifiedPlaylist { playlist_id, name },
            ),
            ClientRequest::DeleteYouTubePlaylist { playlist_id } => PlaylistRequest::new(
                operation_id(),
                super::PlaylistRequestKind::DeleteYouTubePlaylist { playlist_id },
            ),
            ClientRequest::DeleteUnifiedPlaylist { playlist_id } => PlaylistRequest::new(
                operation_id(),
                super::PlaylistRequestKind::DeleteUnifiedPlaylist { playlist_id },
            ),
            ClientRequest::LinkUnifiedPlaylistToYouTube {
                unified_playlist_id,
                youtube_playlist_id,
            } => PlaylistRequest::new(
                operation_id(),
                super::PlaylistRequestKind::LinkUnifiedPlaylistToYouTube {
                    unified_playlist_id,
                    youtube_playlist_id,
                },
            ),
            ClientRequest::SyncUnifiedPlaylistToYouTube {
                unified_playlist_id,
            } => PlaylistRequest::new(
                operation_id(),
                super::PlaylistRequestKind::SyncUnifiedPlaylistToYouTube {
                    unified_playlist_id,
                },
            ),
            ClientRequest::UnlinkUnifiedPlaylistFromYouTube {
                unified_playlist_id,
            } => PlaylistRequest::new(
                operation_id(),
                super::PlaylistRequestKind::UnlinkUnifiedPlaylistFromYouTube {
                    unified_playlist_id,
                },
            ),
            _ => anyhow::bail!("request is not a playlist operation"),
        };
        Ok(request)
    }

    pub(crate) fn plan(request: PlaylistRequest) -> Result<PlaylistOperationPlan> {
        let operation_id = request.operation_id;
        match &request.operation {
            super::PlaylistRequestKind::SpotifyMutation(intent) => ensure!(
                intent.operation_id() == operation_id,
                "playlist request and Spotify intent correlation ids differ"
            ),
            super::PlaylistRequestKind::YouTubeMutation(intent) => ensure!(
                intent.operation_id() == operation_id,
                "playlist request and YouTube intent correlation ids differ"
            ),
            super::PlaylistRequestKind::AddItemsToUnified {
                operation: Some(operation),
                ..
            } => ensure!(
                !operation.operation_id.trim().is_empty()
                    && operation
                        .application_operation_id
                        .is_none_or(|id| id == operation_id.0),
                "Unified playlist request has no operation identity"
            ),
            super::PlaylistRequestKind::CreateUnifiedPlaylistWithItems {
                operation: Some(operation),
                ..
            } => ensure!(
                !operation.operation_id.trim().is_empty()
                    && operation
                        .application_operation_id
                        .is_none_or(|id| id == operation_id.0),
                "Unified playlist request has no operation identity"
            ),
            super::PlaylistRequestKind::RemoveUnifiedOccurrences {
                playlist_id,
                expected_order,
                entry_ids,
            } => {
                ensure!(
                    !playlist_id.trim().is_empty(),
                    "Unified playlist has no identity"
                );
                ensure!(
                    !expected_order.is_empty(),
                    "Unified removal has no expected order"
                );
                ensure!(!entry_ids.is_empty(), "Unified removal has no occurrences");
                ensure!(
                    entry_ids
                        .iter()
                        .all(|entry_id| expected_order.contains(entry_id)),
                    "Unified removal contains an occurrence outside the expected order"
                );
            }
            _ => {}
        }

        Ok(PlaylistOperationPlan {
            operation_id,
            request,
        })
    }

    pub(crate) fn dispatch(
        sender: &ClientRequestSender,
        request: PlaylistRequest,
    ) -> Result<PlaylistOperationPlan> {
        let plan = Self::plan(request)?;
        sender.send(ClientRequest::Playlist(plan.request.clone()))?;
        Ok(plan)
    }
}

/// Dispatch helper intentionally exposed to event handlers as the sole
/// playlist ingress.  It keeps request construction and operation correlation
/// in one place without making the UI aware of provider adapters.
pub(crate) fn dispatch_playlist(
    sender: &ClientRequestSender,
    request: PlaylistRequest,
) -> Result<PlaylistOperationPlan> {
    PlaylistApplicationService::dispatch(sender, request)
}

pub(crate) fn playlist_request(request: ClientRequest) -> Result<ClientRequest> {
    let request = PlaylistApplicationService::from_legacy(request)?;
    let plan = PlaylistApplicationService::plan(request)?;
    Ok(ClientRequest::Playlist(plan.request))
}

pub(crate) fn dispatch_legacy_playlist(
    sender: &ClientRequestSender,
    request: ClientRequest,
) -> Result<PlaylistOperationPlan> {
    dispatch_playlist(sender, PlaylistApplicationService::from_legacy(request)?)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn planner_accepts_zero_bulk_index_as_a_valid_correlation_id() {
        let request = PlaylistRequest::new(
            PlaylistMutationOperationId(0),
            super::super::PlaylistRequestKind::CreateUnifiedPlaylist {
                playlist_name: "local".to_owned(),
            },
        );
        let plan = PlaylistApplicationService::plan(request).expect("zero is a valid bulk index");
        assert_eq!(plan.operation_id, PlaylistMutationOperationId(0));
    }

    #[test]
    fn planner_preserves_one_operation_identity() {
        let operation_id = PlaylistMutationOperationId(41);
        let request = PlaylistRequest::new(
            operation_id,
            super::super::PlaylistRequestKind::CreateUnifiedPlaylist {
                playlist_name: "local".to_owned(),
            },
        );
        let plan = PlaylistApplicationService::plan(request).expect("valid plan");
        assert_eq!(plan.operation_id, operation_id);
        assert_eq!(plan.request.operation_id, operation_id);
    }
}
