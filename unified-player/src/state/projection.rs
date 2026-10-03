#![allow(dead_code)]

use std::collections::HashMap;

use super::{
    MediaId, PlaylistEntryId, PlaylistProjectionConflict, PlaylistProjectionConflictKind,
    PlaylistProjectionMapping, PlaylistProjectionStatus, UnifiedPlaylistItem,
};

/// Pure dry-run output used by both the TUI and deterministic provider
/// contract tests.  It intentionally carries no credentials or mutation
/// handles.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ProjectionConflictPlan {
    pub status: PlaylistProjectionStatus,
    pub conflicts: Vec<PlaylistProjectionConflict>,
}

impl ProjectionConflictPlan {
    pub const fn clean() -> Self {
        Self {
            status: PlaylistProjectionStatus::Clean,
            conflicts: Vec::new(),
        }
    }

    pub fn requires_preview_confirmation(&self) -> bool {
        self.status.needs_preview_confirmation()
    }
}

/// Compare ordered occurrence lists after applying accepted cross-provider
/// mappings.  Membership is a multiset comparison, so duplicate occurrences
/// remain visible instead of being collapsed into a set.
pub fn dry_run_projection(
    local: &[UnifiedPlaylistItem],
    remote: &[UnifiedPlaylistItem],
    mappings: &[PlaylistProjectionMapping],
) -> ProjectionConflictPlan {
    let mut mapping_counts = HashMap::<_, usize>::new();
    for mapping in mappings {
        *mapping_counts.entry(mapping.local_entry_id).or_default() += 1;
    }
    let duplicate_local_entries = mapping_counts
        .iter()
        .filter_map(|(entry_id, count)| (*count > 1).then_some(*entry_id))
        .collect::<Vec<_>>();
    let mut occurrence_tokens = HashMap::<&str, Vec<PlaylistEntryId>>::new();
    for mapping in mappings {
        if let Some(token) = mapping.remote_occurrence_token.as_deref() {
            occurrence_tokens
                .entry(token)
                .or_default()
                .push(mapping.local_entry_id);
        }
    }
    let mapping_by_entry = mappings
        .iter()
        .filter(|mapping| mapping_counts[&mapping.local_entry_id] == 1)
        .map(|mapping| (mapping.local_entry_id, &mapping.remote_media_id))
        .collect::<HashMap<_, _>>();
    let expected = local
        .iter()
        .map(|item| {
            mapping_by_entry
                .get(&item.entry_id)
                .copied()
                .unwrap_or(&item.media_id)
        })
        .collect::<Vec<_>>();
    let actual = remote.iter().map(|item| &item.media_id).collect::<Vec<_>>();
    let mut conflicts = Vec::new();

    for entry_id in duplicate_local_entries {
        conflicts.push(PlaylistProjectionConflict {
            kind: PlaylistProjectionConflictKind::Mapping,
            message: format!(
                "multiple accepted mappings reference local occurrence {}",
                entry_id.0
            ),
            local_entry_ids: vec![entry_id],
            remote_positions: Vec::new(),
        });
    }
    for (token, mut entry_ids) in occurrence_tokens {
        if entry_ids.len() > 1 {
            entry_ids.sort_unstable();
            conflicts.push(PlaylistProjectionConflict {
                kind: PlaylistProjectionConflictKind::Mapping,
                message: format!(
                    "remote occurrence token '{token}' is assigned to multiple local occurrences"
                ),
                local_entry_ids: entry_ids,
                remote_positions: Vec::new(),
            });
        }
    }

    let expected_counts = counts(&expected);
    let actual_counts = counts(&actual);
    if expected_counts != actual_counts {
        let local_entry_ids = local
            .iter()
            .zip(&expected)
            .filter(|(_, expected_id)| {
                let expected_count = expected_counts.get(*expected_id).copied().unwrap_or(0);
                let actual_count = actual_counts.get(*expected_id).copied().unwrap_or(0);
                expected_count > actual_count
            })
            .map(|(item, _)| item.entry_id)
            .collect();
        conflicts.push(PlaylistProjectionConflict {
            kind: PlaylistProjectionConflictKind::Membership,
            message: "remote membership differs from the projected occurrence list".to_owned(),
            local_entry_ids,
            remote_positions: (0..remote.len()).collect(),
        });
    }

    if expected_counts == actual_counts && expected != actual {
        conflicts.push(PlaylistProjectionConflict {
            kind: PlaylistProjectionConflictKind::Order,
            message: "remote occurrence order differs from the projected order".to_owned(),
            local_entry_ids: local.iter().map(|item| item.entry_id).collect(),
            remote_positions: (0..remote.len()).collect(),
        });
    }

    for mapping in mappings {
        let Some(local_item) = local
            .iter()
            .find(|item| item.entry_id == mapping.local_entry_id)
        else {
            conflicts.push(PlaylistProjectionConflict {
                kind: PlaylistProjectionConflictKind::Mapping,
                message: format!(
                    "accepted mapping references missing local occurrence {}",
                    mapping.local_entry_id.0
                ),
                local_entry_ids: vec![mapping.local_entry_id],
                remote_positions: Vec::new(),
            });
            continue;
        };
        if local_item.media_id == mapping.remote_media_id {
            continue;
        }
        if !remote
            .iter()
            .any(|item| item.media_id == mapping.remote_media_id)
        {
            conflicts.push(PlaylistProjectionConflict {
                kind: PlaylistProjectionConflictKind::Mapping,
                message: format!(
                    "accepted mapping for occurrence {} is absent remotely",
                    mapping.local_entry_id.0
                ),
                local_entry_ids: vec![mapping.local_entry_id],
                remote_positions: Vec::new(),
            });
        }
    }

    conflicts.sort_by(|left, right| {
        left.kind
            .cmp(&right.kind)
            .then_with(|| left.message.cmp(&right.message))
    });
    if conflicts.is_empty() {
        ProjectionConflictPlan::clean()
    } else {
        ProjectionConflictPlan {
            status: PlaylistProjectionStatus::Conflict,
            conflicts,
        }
    }
}

fn counts(values: &[&MediaId]) -> HashMap<MediaId, usize> {
    let mut counts = HashMap::new();
    for value in values {
        *counts.entry((*value).clone()).or_default() += 1;
    }
    counts
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::state::{
        DurationUnit, MediaKind, PlaylistEntryId, Provider, UnifiedPlaylistMetadata,
    };

    fn item(entry_id: u64, provider: Provider, id: &str) -> UnifiedPlaylistItem {
        UnifiedPlaylistItem {
            entry_id: PlaylistEntryId(entry_id),
            media_id: MediaId {
                provider,
                kind: MediaKind::Track,
                raw_id: id.to_owned(),
            },
            title: id.to_owned(),
            artists: String::new(),
            duration_ms: None,
            duration_unit: DurationUnit::Milliseconds,
            provider_url: None,
            metadata: UnifiedPlaylistMetadata::default(),
        }
    }

    #[test]
    fn membership_order_and_mapping_conflicts_are_deterministic() {
        let local = vec![
            item(1, Provider::Spotify, "a"),
            item(2, Provider::Spotify, "b"),
        ];
        let remote = vec![
            item(7, Provider::YouTubeMusic, "b"),
            item(8, Provider::YouTubeMusic, "a"),
        ];
        let mappings = vec![
            PlaylistProjectionMapping {
                local_entry_id: PlaylistEntryId(1),
                remote_media_id: MediaId {
                    provider: Provider::YouTubeMusic,
                    kind: MediaKind::Track,
                    raw_id: "a".to_owned(),
                },
                remote_occurrence_token: None,
                accepted_at: 1,
            },
            PlaylistProjectionMapping {
                local_entry_id: PlaylistEntryId(2),
                remote_media_id: MediaId {
                    provider: Provider::YouTubeMusic,
                    kind: MediaKind::Track,
                    raw_id: "b".to_owned(),
                },
                remote_occurrence_token: None,
                accepted_at: 1,
            },
            PlaylistProjectionMapping {
                local_entry_id: PlaylistEntryId(99),
                remote_media_id: MediaId {
                    provider: Provider::YouTubeMusic,
                    kind: MediaKind::Track,
                    raw_id: "missing".to_owned(),
                },
                remote_occurrence_token: None,
                accepted_at: 1,
            },
        ];
        let plan = dry_run_projection(&local, &remote, &mappings);
        assert_eq!(plan.status, PlaylistProjectionStatus::Conflict);
        assert_eq!(
            plan.conflicts[0].kind,
            PlaylistProjectionConflictKind::Order
        );
        assert!(plan
            .conflicts
            .iter()
            .any(|conflict| conflict.kind == PlaylistProjectionConflictKind::Mapping));
    }

    #[test]
    fn identical_mapped_occurrences_are_clean() {
        let local = vec![item(1, Provider::Spotify, "a")];
        let remote = vec![item(8, Provider::YouTubeMusic, "a")];
        let mappings = vec![PlaylistProjectionMapping {
            local_entry_id: PlaylistEntryId(1),
            remote_media_id: remote[0].media_id.clone(),
            remote_occurrence_token: None,
            accepted_at: 1,
        }];
        assert_eq!(
            dry_run_projection(&local, &remote, &mappings),
            ProjectionConflictPlan::clean()
        );
    }

    #[test]
    fn duplicate_mapping_rows_are_a_deterministic_conflict() {
        let local = vec![item(1, Provider::Spotify, "a")];
        let mappings = vec![
            PlaylistProjectionMapping {
                local_entry_id: PlaylistEntryId(1),
                remote_media_id: MediaId {
                    provider: Provider::YouTubeMusic,
                    kind: MediaKind::Track,
                    raw_id: "a".to_owned(),
                },
                remote_occurrence_token: Some("occ-1".to_owned()),
                accepted_at: 1,
            },
            PlaylistProjectionMapping {
                local_entry_id: PlaylistEntryId(1),
                remote_media_id: MediaId {
                    provider: Provider::YouTubeMusic,
                    kind: MediaKind::Track,
                    raw_id: "b".to_owned(),
                },
                remote_occurrence_token: Some("occ-1".to_owned()),
                accepted_at: 2,
            },
        ];
        let plan = dry_run_projection(&local, &[], &mappings);
        assert_eq!(plan.status, PlaylistProjectionStatus::Conflict);
        assert!(plan
            .conflicts
            .iter()
            .any(|conflict| conflict.message.contains("multiple accepted mappings")));
        assert!(plan
            .conflicts
            .iter()
            .any(|conflict| conflict.message.contains("occ-1")));
    }
}
