use std::collections::{BTreeMap, BTreeSet};

use serde::Serialize;

use crate::cli::listenbrainz_manifest::{
    description_envelope_from_manifest, description_manifest_with_recording_mbids,
    is_recording_mbid, DescriptionManifest,
};
use crate::state::{MediaId, MediaKind, PlaylistEntryId, UnifiedPlaylist};

const MUSICBRAINZ_RECORDING_URI_PREFIX: &str = "https://musicbrainz.org/recording/";

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct ExplicitRecordingRelation {
    pub(crate) media_id: MediaId,
    pub(crate) recording_mbid: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub(crate) struct NativeJspfTrack {
    pub(crate) identifier: Vec<String>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct NativeProjection {
    pub(crate) manifest: DescriptionManifest,
    pub(crate) tracks: Vec<NativeJspfTrack>,
    pub(crate) resolved_occurrences: BTreeMap<PlaylistEntryId, String>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum NativeProjectionPreviewStatus {
    Ready,
    CannotProject,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum NativeProjectionPreviewReason {
    ManifestBudgetExceeded,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub(crate) struct NativeProjectionPreview {
    pub(crate) schema_version: u8,
    pub(crate) status: NativeProjectionPreviewStatus,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) cannot_project_reason: Option<NativeProjectionPreviewReason>,
    pub(crate) total_occurrences: usize,
    pub(crate) native_rows: usize,
    pub(crate) manifest_only: usize,
    pub(crate) unresolved: usize,
    pub(crate) ineligible: usize,
    pub(crate) duplicate_native_rows: usize,
    pub(crate) annotation_characters: usize,
    pub(crate) annotation_budget: usize,
    pub(crate) writes_performed: bool,
}

impl NativeProjectionPreview {
    pub(crate) const fn is_ready(&self) -> bool {
        matches!(self.status, NativeProjectionPreviewStatus::Ready)
    }
}

pub(crate) fn build_native_projection(
    playlist: &UnifiedPlaylist,
    relationships: &[ExplicitRecordingRelation],
) -> anyhow::Result<NativeProjection> {
    let playlist_media = playlist
        .items
        .iter()
        .map(|item| item.media_id.clone())
        .collect::<BTreeSet<_>>();
    let mut by_media = BTreeMap::<MediaId, String>::new();
    for relationship in relationships {
        anyhow::ensure!(
            playlist_media.contains(&relationship.media_id),
            "MusicBrainz relationship targets media outside the Unified playlist"
        );
        anyhow::ensure!(
            relationship.media_id.kind != MediaKind::Episode,
            "MusicBrainz recording relationship cannot target an episode"
        );
        let recording_mbid = relationship.recording_mbid.to_ascii_lowercase();
        anyhow::ensure!(
            is_recording_mbid(&recording_mbid),
            "MusicBrainz relationship has an invalid recording MBID"
        );
        if let Some(existing) = by_media.get(&relationship.media_id) {
            anyhow::ensure!(
                existing == &recording_mbid,
                "MusicBrainz relationships are ambiguous for one provider media identity"
            );
        } else {
            by_media.insert(relationship.media_id.clone(), recording_mbid);
        }
    }

    let resolved_occurrences = playlist
        .items
        .iter()
        .filter_map(|item| {
            by_media
                .get(&item.media_id)
                .map(|mbid| (item.entry_id, mbid.clone()))
        })
        .collect::<BTreeMap<_, _>>();
    let manifest = description_manifest_with_recording_mbids(playlist, &resolved_occurrences)?;
    let tracks = playlist
        .items
        .iter()
        .filter_map(|item| resolved_occurrences.get(&item.entry_id))
        .map(|recording_mbid| NativeJspfTrack {
            identifier: vec![format!(
                "{MUSICBRAINZ_RECORDING_URI_PREFIX}{recording_mbid}"
            )],
        })
        .collect();
    Ok(NativeProjection {
        manifest,
        tracks,
        resolved_occurrences,
    })
}

pub(crate) fn preview_native_projection(
    playlist: &UnifiedPlaylist,
    relationships: &[ExplicitRecordingRelation],
    annotation_budget: usize,
) -> anyhow::Result<(NativeProjection, NativeProjectionPreview)> {
    let projection = build_native_projection(playlist, relationships)?;
    let annotation_characters = description_envelope_from_manifest(&projection.manifest)?
        .chars()
        .count();
    let mut recording_counts = BTreeMap::<&str, usize>::new();
    for recording_mbid in projection.resolved_occurrences.values() {
        *recording_counts.entry(recording_mbid).or_default() += 1;
    }
    let duplicate_native_rows = recording_counts
        .values()
        .map(|count| count.saturating_sub(1))
        .sum();
    let ineligible = playlist
        .items
        .iter()
        .filter(|item| item.media_id.kind == MediaKind::Episode)
        .count();
    let unresolved = playlist
        .items
        .len()
        .saturating_sub(projection.tracks.len())
        .saturating_sub(ineligible);
    let ready = annotation_characters <= annotation_budget;
    let report = NativeProjectionPreview {
        schema_version: 1,
        status: if ready {
            NativeProjectionPreviewStatus::Ready
        } else {
            NativeProjectionPreviewStatus::CannotProject
        },
        cannot_project_reason: (!ready)
            .then_some(NativeProjectionPreviewReason::ManifestBudgetExceeded),
        total_occurrences: playlist.items.len(),
        native_rows: projection.tracks.len(),
        manifest_only: playlist.items.len().saturating_sub(projection.tracks.len()),
        unresolved,
        ineligible,
        duplicate_native_rows,
        annotation_characters,
        annotation_budget,
        writes_performed: false,
    };
    Ok((projection, report))
}

#[cfg(test)]
mod tests {
    use super::{
        build_native_projection, preview_native_projection, ExplicitRecordingRelation,
        NativeProjectionPreviewReason, NativeProjectionPreviewStatus,
    };
    use crate::cli::listenbrainz_manifest::DescriptionManifestProjectionStatus;
    use crate::state::{
        MediaId, MediaKind, PlaylistEntryId, Provider, UnifiedPlaylist, UnifiedPlaylistItem,
    };

    fn media(provider: Provider, kind: MediaKind, raw_id: &str) -> MediaId {
        MediaId {
            provider,
            kind,
            raw_id: raw_id.to_owned(),
        }
    }

    fn item(entry_id: u64, media_id: MediaId) -> UnifiedPlaylistItem {
        UnifiedPlaylistItem {
            entry_id: PlaylistEntryId(entry_id),
            media_id,
            ..UnifiedPlaylistItem::default()
        }
    }

    fn playlist() -> UnifiedPlaylist {
        let spotify = media(Provider::Spotify, MediaKind::Track, "spotify-1");
        UnifiedPlaylist {
            id: "local".to_owned(),
            name: "Mixed".to_owned(),
            items: vec![
                item(1, spotify.clone()),
                item(
                    2,
                    media(Provider::YouTubeMusic, MediaKind::Video, "youtube-1"),
                ),
                item(3, spotify),
                item(4, media(Provider::Spotify, MediaKind::Episode, "episode-1")),
            ],
            next_entry_id: 5,
            ..UnifiedPlaylist::default()
        }
    }

    #[test]
    fn explicit_relationships_preserve_duplicate_occurrences_and_relative_order() {
        let mbid = "12345678-1234-1234-1234-123456789abc";
        let projection = build_native_projection(
            &playlist(),
            &[ExplicitRecordingRelation {
                media_id: media(Provider::Spotify, MediaKind::Track, "spotify-1"),
                recording_mbid: mbid.to_uppercase(),
            }],
        )
        .unwrap();

        assert_eq!(projection.manifest.entries.len(), 4);
        assert_eq!(projection.tracks.len(), 2);
        assert_eq!(
            projection
                .tracks
                .iter()
                .map(|track| track.identifier[0].as_str())
                .collect::<Vec<_>>(),
            vec![
                "https://musicbrainz.org/recording/12345678-1234-1234-1234-123456789abc",
                "https://musicbrainz.org/recording/12345678-1234-1234-1234-123456789abc",
            ]
        );
        assert_eq!(
            projection
                .resolved_occurrences
                .keys()
                .copied()
                .collect::<Vec<_>>(),
            vec![PlaylistEntryId(1), PlaylistEntryId(3)]
        );
        assert_eq!(
            projection.manifest.entries[1]
                .projection
                .as_ref()
                .unwrap()
                .status,
            DescriptionManifestProjectionStatus::Unresolved
        );
        assert_eq!(
            projection.manifest.entries[3]
                .projection
                .as_ref()
                .unwrap()
                .status,
            DescriptionManifestProjectionStatus::Ineligible
        );
    }

    #[test]
    fn jspf_rows_do_not_claim_occurrence_identity_or_additional_metadata() {
        let projection = build_native_projection(
            &playlist(),
            &[ExplicitRecordingRelation {
                media_id: media(Provider::Spotify, MediaKind::Track, "spotify-1"),
                recording_mbid: "12345678-1234-1234-1234-123456789abc".to_owned(),
            }],
        )
        .unwrap();
        let value = serde_json::to_value(&projection.tracks).unwrap();
        let json = serde_json::to_string(&value).unwrap();

        assert!(!json.contains("occurrence"));
        assert!(!json.contains("additional_metadata"));
        assert!(!json.contains("spotify-1"));
    }

    #[test]
    fn ambiguous_invalid_episode_and_external_relationships_fail_closed() {
        let spotify = media(Provider::Spotify, MediaKind::Track, "spotify-1");
        let first = "12345678-1234-1234-1234-123456789abc";
        let second = "abcdefab-cdef-abcd-efab-cdefabcdefab";
        assert!(build_native_projection(
            &playlist(),
            &[
                ExplicitRecordingRelation {
                    media_id: spotify.clone(),
                    recording_mbid: first.to_owned(),
                },
                ExplicitRecordingRelation {
                    media_id: spotify,
                    recording_mbid: second.to_owned(),
                },
            ],
        )
        .unwrap_err()
        .to_string()
        .contains("ambiguous"));
        assert!(build_native_projection(
            &playlist(),
            &[ExplicitRecordingRelation {
                media_id: media(Provider::Spotify, MediaKind::Episode, "episode-1"),
                recording_mbid: first.to_owned(),
            }],
        )
        .is_err());
        assert!(build_native_projection(
            &playlist(),
            &[ExplicitRecordingRelation {
                media_id: media(Provider::Spotify, MediaKind::Track, "outside"),
                recording_mbid: first.to_owned(),
            }],
        )
        .is_err());
        assert!(build_native_projection(
            &playlist(),
            &[ExplicitRecordingRelation {
                media_id: media(Provider::Spotify, MediaKind::Track, "spotify-1"),
                recording_mbid: "not-an-mbid".to_owned(),
            }],
        )
        .is_err());
    }

    #[test]
    fn preview_reports_redacted_counts_and_fails_closed_over_budget() {
        let relationship = ExplicitRecordingRelation {
            media_id: media(Provider::Spotify, MediaKind::Track, "spotify-1"),
            recording_mbid: "12345678-1234-1234-1234-123456789abc".to_owned(),
        };
        let (_, ready) =
            preview_native_projection(&playlist(), &[relationship.clone()], 9_000).unwrap();
        assert_eq!(ready.status, NativeProjectionPreviewStatus::Ready);
        assert_eq!(ready.total_occurrences, 4);
        assert_eq!(ready.native_rows, 2);
        assert_eq!(ready.manifest_only, 2);
        assert_eq!(ready.unresolved, 1);
        assert_eq!(ready.ineligible, 1);
        assert_eq!(ready.duplicate_native_rows, 1);
        assert!(!ready.writes_performed);

        let (_, blocked) = preview_native_projection(&playlist(), &[relationship], 10).unwrap();
        assert_eq!(blocked.status, NativeProjectionPreviewStatus::CannotProject);
        assert_eq!(
            blocked.cannot_project_reason,
            Some(NativeProjectionPreviewReason::ManifestBudgetExceeded)
        );
        let json = serde_json::to_string(&blocked).unwrap();
        assert!(!json.contains("spotify-1"));
        assert!(!json.contains("12345678"));
        assert!(!json.contains("annotation\""));
    }
}
