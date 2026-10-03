use crate::state::{Provider, ProviderOccurrenceToken};

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub struct PlaylistMutationOperationId(pub u64);

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum MutationFailureCategory {
    Authentication,
    InvalidRequest,
    Transport,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Retryability {
    Never,
    AfterRefresh,
    ManualAfterVerification,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum PlaylistMutationEffect {
    InvalidatePlaylist {
        provider: Provider,
        playlist_id: String,
        observed_revision: Option<String>,
        appended_occurrence_token: Option<String>,
    },
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct MutationReceipt {
    pub operation_id: PlaylistMutationOperationId,
    pub effect: PlaylistMutationEffect,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum PlaylistMutationResult {
    Applied {
        receipt: MutationReceipt,
    },
    Conflict {
        current_revision: Option<String>,
    },
    Invalidated {
        reason: &'static str,
    },
    Unsupported {
        reason: &'static str,
    },
    Failed {
        category: MutationFailureCategory,
        retryability: Retryability,
    },
    OutcomeUnknown {
        operation_id: PlaylistMutationOperationId,
    },
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ProviderMutationDelivery {
    Applied {
        revision: Option<String>,
        occurrence_token: Option<String>,
    },
    #[allow(dead_code)] // The provider SDK does not currently expose a stable conflict status.
    Conflict {
        current_revision: Option<String>,
    },
    Failed {
        category: MutationFailureCategory,
        retryability: Retryability,
    },
    Ambiguous,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SpotifyExactOccurrence {
    pub position: usize,
    pub snapshot_id: String,
}

impl SpotifyExactOccurrence {
    pub fn from_token(token: &ProviderOccurrenceToken) -> Option<Self> {
        let ProviderOccurrenceToken::Spotify {
            position,
            snapshot_id,
        } = token
        else {
            return None;
        };
        Some(Self {
            position: *position,
            snapshot_id: snapshot_id.clone(),
        })
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum SpotifyMutationIntent {
    RemoveOccurrence {
        operation_id: PlaylistMutationOperationId,
        playlist_id: String,
        media_uri: String,
        occurrence: SpotifyExactOccurrence,
    },
    RemoveAllMedia {
        operation_id: PlaylistMutationOperationId,
        playlist_id: String,
        media_uris: Vec<String>,
        snapshot_id: String,
    },
    Reorder {
        operation_id: PlaylistMutationOperationId,
        playlist_id: String,
        range_start: usize,
        insert_before: usize,
        range_length: usize,
        snapshot_id: String,
    },
}

impl SpotifyMutationIntent {
    pub(crate) fn operation_id(&self) -> PlaylistMutationOperationId {
        match self {
            Self::RemoveOccurrence { operation_id, .. }
            | Self::RemoveAllMedia { operation_id, .. }
            | Self::Reorder { operation_id, .. } => *operation_id,
        }
    }

    pub(crate) fn playlist_id(&self) -> &str {
        match self {
            Self::RemoveOccurrence { playlist_id, .. }
            | Self::RemoveAllMedia { playlist_id, .. }
            | Self::Reorder { playlist_id, .. } => playlist_id,
        }
    }

    pub(crate) fn base_revision(&self) -> &str {
        match self {
            Self::RemoveOccurrence { occurrence, .. } => &occurrence.snapshot_id,
            Self::RemoveAllMedia { snapshot_id, .. } | Self::Reorder { snapshot_id, .. } => {
                snapshot_id
            }
        }
    }
}

#[derive(Clone, Copy, Debug, Default)]
pub struct SpotifyPlaylistAdapter;

impl SpotifyPlaylistAdapter {
    pub fn validate(intent: &SpotifyMutationIntent) -> Result<(), PlaylistMutationResult> {
        if intent.playlist_id().trim().is_empty() || intent.base_revision().trim().is_empty() {
            return Err(PlaylistMutationResult::Invalidated {
                reason: "Spotify occurrence and reorder mutations require a base snapshot.",
            });
        }
        if matches!(
            intent,
            SpotifyMutationIntent::RemoveOccurrence { media_uri, .. }
                if media_uri.trim().is_empty()
        ) {
            return Err(PlaylistMutationResult::Invalidated {
                reason: "Spotify removal requires a concrete media URI.",
            });
        }
        if matches!(
            intent,
            SpotifyMutationIntent::RemoveAllMedia { media_uris, .. }
                if media_uris.is_empty()
                    || media_uris.len() > 100
                    || media_uris.iter().any(|uri| uri.trim().is_empty())
        ) {
            return Err(PlaylistMutationResult::Invalidated {
                reason: "Spotify remove-all requires one to 100 concrete media URIs.",
            });
        }
        if matches!(
            intent,
            SpotifyMutationIntent::Reorder {
                range_start,
                range_length,
                ..
            } if *range_length == 0 || range_start.checked_add(*range_length).is_none()
        ) {
            return Err(PlaylistMutationResult::Invalidated {
                reason: "Spotify reorder requires a nonempty bounded occurrence range.",
            });
        }
        Ok(())
    }

    pub fn complete(
        intent: &SpotifyMutationIntent,
        delivery: ProviderMutationDelivery,
    ) -> PlaylistMutationResult {
        if let Err(result) = Self::validate(intent) {
            return result;
        }
        complete_delivery(
            Provider::Spotify,
            intent.operation_id(),
            intent.playlist_id(),
            delivery,
        )
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum YouTubeMutationIntent {
    RemoveOccurrence {
        operation_id: PlaylistMutationOperationId,
        playlist_id: String,
        video_id: Option<String>,
        set_video_id: Option<String>,
    },
    Append {
        operation_id: PlaylistMutationOperationId,
        playlist_id: String,
        video_id: String,
        baseline: Option<YouTubeAppendReadBack>,
    },
    #[allow(dead_code)] // Kept as an explicit fail-closed contract, never dispatched.
    Reorder {
        operation_id: PlaylistMutationOperationId,
        playlist_id: String,
    },
}

impl YouTubeMutationIntent {
    pub(crate) fn operation_id(&self) -> PlaylistMutationOperationId {
        match self {
            Self::RemoveOccurrence { operation_id, .. }
            | Self::Append { operation_id, .. }
            | Self::Reorder { operation_id, .. } => *operation_id,
        }
    }

    pub(crate) fn playlist_id(&self) -> &str {
        match self {
            Self::RemoveOccurrence { playlist_id, .. }
            | Self::Append { playlist_id, .. }
            | Self::Reorder { playlist_id, .. } => playlist_id,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct YouTubeAppendReadBack {
    pub total: usize,
    pub media_occurrences: usize,
    pub last_media_position: Option<usize>,
}

#[derive(Clone, Copy, Debug, Default)]
pub struct YouTubePlaylistAdapter;

impl YouTubePlaylistAdapter {
    pub fn validate(intent: &YouTubeMutationIntent) -> Result<(), PlaylistMutationResult> {
        if intent.playlist_id().trim().is_empty() {
            return Err(PlaylistMutationResult::Invalidated {
                reason: "YouTube playlist mutation requires a concrete playlist identity.",
            });
        }
        match intent {
            YouTubeMutationIntent::Reorder { .. } => Err(PlaylistMutationResult::Unsupported {
                reason: "YouTube Music reorder has no verified provider contract.",
            }),
            YouTubeMutationIntent::RemoveOccurrence {
                video_id,
                set_video_id,
                ..
            } if video_id
                .as_deref()
                .is_some_and(|video_id| video_id.trim().is_empty())
                || set_video_id
                    .as_deref()
                    .is_none_or(|token| token.trim().is_empty()) =>
            {
                Err(PlaylistMutationResult::Invalidated {
                    reason: "Exact YouTube removal requires the row's SetVideoID.",
                })
            }
            YouTubeMutationIntent::Append { video_id, .. } if video_id.trim().is_empty() => {
                Err(PlaylistMutationResult::Invalidated {
                    reason: "YouTube append requires a concrete video identity.",
                })
            }
            _ => Ok(()),
        }
    }

    pub fn complete(
        intent: &YouTubeMutationIntent,
        delivery: ProviderMutationDelivery,
        read_back: Option<YouTubeAppendReadBack>,
    ) -> PlaylistMutationResult {
        if let Err(result) = Self::validate(intent) {
            return result;
        }
        match intent {
            YouTubeMutationIntent::Append {
                video_id, baseline, ..
            } if matches!(delivery, ProviderMutationDelivery::Ambiguous) => {
                let Some(baseline) = baseline else {
                    return PlaylistMutationResult::OutcomeUnknown {
                        operation_id: intent.operation_id(),
                    };
                };
                let Some(read_back) = read_back else {
                    return PlaylistMutationResult::OutcomeUnknown {
                        operation_id: intent.operation_id(),
                    };
                };
                if read_back.total == baseline.total.saturating_add(1)
                    && read_back.media_occurrences == baseline.media_occurrences.saturating_add(1)
                    && read_back.last_media_position == read_back.total.checked_sub(1)
                {
                    applied_receipt(
                        Provider::YouTubeMusic,
                        intent.operation_id(),
                        intent.playlist_id(),
                        None,
                        None,
                    )
                } else if read_back.total == baseline.total
                    && read_back.media_occurrences == baseline.media_occurrences
                    && read_back.last_media_position == baseline.last_media_position
                    && !video_id.trim().is_empty()
                {
                    PlaylistMutationResult::Failed {
                        category: MutationFailureCategory::Transport,
                        retryability: Retryability::ManualAfterVerification,
                    }
                } else {
                    PlaylistMutationResult::OutcomeUnknown {
                        operation_id: intent.operation_id(),
                    }
                }
            }
            _ => complete_delivery(
                Provider::YouTubeMusic,
                intent.operation_id(),
                intent.playlist_id(),
                delivery,
            ),
        }
    }
}

fn complete_delivery(
    provider: Provider,
    operation_id: PlaylistMutationOperationId,
    playlist_id: &str,
    delivery: ProviderMutationDelivery,
) -> PlaylistMutationResult {
    match delivery {
        ProviderMutationDelivery::Applied {
            revision,
            occurrence_token,
        } => applied_receipt(
            provider,
            operation_id,
            playlist_id,
            revision,
            occurrence_token,
        ),
        ProviderMutationDelivery::Conflict { current_revision } => {
            PlaylistMutationResult::Conflict { current_revision }
        }
        ProviderMutationDelivery::Failed {
            category,
            retryability,
        } => PlaylistMutationResult::Failed {
            category,
            retryability,
        },
        ProviderMutationDelivery::Ambiguous => {
            PlaylistMutationResult::OutcomeUnknown { operation_id }
        }
    }
}

fn applied_receipt(
    provider: Provider,
    operation_id: PlaylistMutationOperationId,
    playlist_id: &str,
    observed_revision: Option<String>,
    appended_occurrence_token: Option<String>,
) -> PlaylistMutationResult {
    PlaylistMutationResult::Applied {
        receipt: MutationReceipt {
            operation_id,
            effect: PlaylistMutationEffect::InvalidatePlaylist {
                provider,
                playlist_id: playlist_id.to_owned(),
                observed_revision,
                appended_occurrence_token,
            },
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn assert_common_contract(
        applied: PlaylistMutationResult,
        conflict: PlaylistMutationResult,
        ambiguous: PlaylistMutationResult,
        provider: Provider,
        operation_id: PlaylistMutationOperationId,
    ) {
        assert!(matches!(
            applied,
            PlaylistMutationResult::Applied {
                receipt: MutationReceipt {
                    effect: PlaylistMutationEffect::InvalidatePlaylist {
                        provider: actual_provider,
                        ..
                    },
                    ..
                }
            } if actual_provider == provider
        ));
        assert_eq!(
            conflict,
            PlaylistMutationResult::Conflict {
                current_revision: Some("new-revision".to_owned())
            }
        );
        assert_eq!(
            ambiguous,
            PlaylistMutationResult::OutcomeUnknown { operation_id }
        );
    }

    #[test]
    fn both_adapters_pass_the_same_result_contract_harness() {
        let operation_id = PlaylistMutationOperationId(7);
        let spotify = SpotifyMutationIntent::Reorder {
            operation_id,
            playlist_id: "spotify-playlist".to_owned(),
            range_start: 2,
            insert_before: 5,
            range_length: 1,
            snapshot_id: "base".to_owned(),
        };
        assert_common_contract(
            SpotifyPlaylistAdapter::complete(
                &spotify,
                ProviderMutationDelivery::Applied {
                    revision: Some("next".to_owned()),
                    occurrence_token: None,
                },
            ),
            SpotifyPlaylistAdapter::complete(
                &spotify,
                ProviderMutationDelivery::Conflict {
                    current_revision: Some("new-revision".to_owned()),
                },
            ),
            SpotifyPlaylistAdapter::complete(&spotify, ProviderMutationDelivery::Ambiguous),
            Provider::Spotify,
            operation_id,
        );

        let youtube = YouTubeMutationIntent::RemoveOccurrence {
            operation_id,
            playlist_id: "youtube-playlist".to_owned(),
            video_id: Some("video".to_owned()),
            set_video_id: Some("set-video".to_owned()),
        };
        assert_common_contract(
            YouTubePlaylistAdapter::complete(
                &youtube,
                ProviderMutationDelivery::Applied {
                    revision: None,
                    occurrence_token: None,
                },
                None,
            ),
            YouTubePlaylistAdapter::complete(
                &youtube,
                ProviderMutationDelivery::Conflict {
                    current_revision: Some("new-revision".to_owned()),
                },
                None,
            ),
            YouTubePlaylistAdapter::complete(&youtube, ProviderMutationDelivery::Ambiguous, None),
            Provider::YouTubeMusic,
            operation_id,
        );
    }

    #[test]
    fn spotify_exact_and_remove_all_intents_are_distinct_and_require_revisions() {
        let operation_id = PlaylistMutationOperationId(1);
        let exact = SpotifyMutationIntent::RemoveOccurrence {
            operation_id,
            playlist_id: "playlist".to_owned(),
            media_uri: "spotify:track:one".to_owned(),
            occurrence: SpotifyExactOccurrence {
                position: 3,
                snapshot_id: "base".to_owned(),
            },
        };
        let all = SpotifyMutationIntent::RemoveAllMedia {
            operation_id,
            playlist_id: "playlist".to_owned(),
            media_uris: vec!["spotify:track:one".to_owned()],
            snapshot_id: "base".to_owned(),
        };
        assert_ne!(exact, all);
        let oversized = SpotifyMutationIntent::RemoveAllMedia {
            operation_id,
            playlist_id: "playlist".to_owned(),
            media_uris: vec!["spotify:track:one".to_owned(); 101],
            snapshot_id: "base".to_owned(),
        };
        assert!(matches!(
            SpotifyPlaylistAdapter::validate(&oversized),
            Err(PlaylistMutationResult::Invalidated { .. })
        ));
        let stale = SpotifyMutationIntent::Reorder {
            operation_id,
            playlist_id: "playlist".to_owned(),
            range_start: 0,
            insert_before: 1,
            range_length: 1,
            snapshot_id: String::new(),
        };
        assert!(matches!(
            SpotifyPlaylistAdapter::complete(
                &stale,
                ProviderMutationDelivery::Applied {
                    revision: None,
                    occurrence_token: None,
                }
            ),
            PlaylistMutationResult::Invalidated { .. }
        ));
        let empty_range = SpotifyMutationIntent::Reorder {
            operation_id,
            playlist_id: "playlist".to_owned(),
            range_start: 0,
            insert_before: 1,
            range_length: 0,
            snapshot_id: "base".to_owned(),
        };
        assert!(matches!(
            SpotifyPlaylistAdapter::validate(&empty_range),
            Err(PlaylistMutationResult::Invalidated { .. })
        ));
    }

    #[test]
    fn youtube_rows_retain_set_video_id_and_exact_removal_fails_closed_without_it() {
        let intent = YouTubeMutationIntent::RemoveOccurrence {
            operation_id: PlaylistMutationOperationId(2),
            playlist_id: "playlist".to_owned(),
            video_id: Some("video".to_owned()),
            set_video_id: None,
        };
        assert!(matches!(
            YouTubePlaylistAdapter::complete(
                &intent,
                ProviderMutationDelivery::Applied {
                    revision: None,
                    occurrence_token: None,
                },
                None
            ),
            PlaylistMutationResult::Invalidated { .. }
        ));
    }

    #[test]
    fn youtube_ambiguous_append_is_read_back_and_never_blindly_retried() {
        let operation_id = PlaylistMutationOperationId(9);
        let intent = YouTubeMutationIntent::Append {
            operation_id,
            playlist_id: "playlist".to_owned(),
            video_id: "video".to_owned(),
            baseline: Some(YouTubeAppendReadBack {
                total: 4,
                media_occurrences: 1,
                last_media_position: Some(1),
            }),
        };
        assert!(matches!(
            YouTubePlaylistAdapter::complete(
                &intent,
                ProviderMutationDelivery::Ambiguous,
                Some(YouTubeAppendReadBack {
                    total: 5,
                    media_occurrences: 2,
                    last_media_position: Some(4),
                })
            ),
            PlaylistMutationResult::Applied { .. }
        ));

        assert!(matches!(
            YouTubePlaylistAdapter::complete(
                &intent,
                ProviderMutationDelivery::Applied {
                    revision: None,
                    occurrence_token: Some("set-video".to_owned()),
                },
                None,
            ),
            PlaylistMutationResult::Applied {
                receipt: MutationReceipt {
                    effect: PlaylistMutationEffect::InvalidatePlaylist {
                        appended_occurrence_token: Some(token),
                        ..
                    },
                    ..
                }
            } if token == "set-video"
        ));
        assert_eq!(
            YouTubePlaylistAdapter::complete(&intent, ProviderMutationDelivery::Ambiguous, None),
            PlaylistMutationResult::OutcomeUnknown { operation_id }
        );
        assert!(matches!(
            YouTubePlaylistAdapter::complete(
                &intent,
                ProviderMutationDelivery::Ambiguous,
                Some(YouTubeAppendReadBack {
                    total: 4,
                    media_occurrences: 1,
                    last_media_position: Some(1),
                })
            ),
            PlaylistMutationResult::Failed {
                retryability: Retryability::ManualAfterVerification,
                ..
            }
        ));
        assert_eq!(
            YouTubePlaylistAdapter::complete(
                &intent,
                ProviderMutationDelivery::Ambiguous,
                Some(YouTubeAppendReadBack {
                    total: 5,
                    media_occurrences: 2,
                    last_media_position: Some(2),
                })
            ),
            PlaylistMutationResult::OutcomeUnknown { operation_id }
        );
    }

    #[test]
    fn youtube_reorder_is_stably_unsupported() {
        let intent = YouTubeMutationIntent::Reorder {
            operation_id: PlaylistMutationOperationId(3),
            playlist_id: "playlist".to_owned(),
        };
        assert!(matches!(
            YouTubePlaylistAdapter::complete(
                &intent,
                ProviderMutationDelivery::Applied {
                    revision: None,
                    occurrence_token: None,
                },
                None
            ),
            PlaylistMutationResult::Unsupported { .. }
        ));
    }
}
