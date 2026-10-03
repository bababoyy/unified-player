use std::collections::{BTreeMap, HashSet};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use sha2::{Digest as _, Sha256};

use crate::state::{MediaKind, PlaylistEntryId, Provider, UnifiedPlaylist};

pub(crate) const DESCRIPTION_CHARACTER_BUDGET: usize = 9_000;
// The envelope version is independent from the manifest schema carried inside.
pub(crate) const DESCRIPTION_ENVELOPE_PREFIX: &str = "unified-player-playlist:v1\n";
const CURRENT_MANIFEST_SCHEMA_VERSION: u8 = 2;

pub(crate) const fn is_supported_manifest_schema_version(version: u64) -> bool {
    version == 1 || version == CURRENT_MANIFEST_SCHEMA_VERSION as u64
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct DescriptionManifest {
    pub(crate) schema_version: u8,
    pub(crate) playlist_id: String,
    pub(crate) snapshot_hash: String,
    pub(crate) entries: Vec<DescriptionManifestEntry>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct DescriptionManifestEntry {
    pub(crate) occurrence: u64,
    pub(crate) provider: String,
    pub(crate) kind: String,
    pub(crate) id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) projection: Option<DescriptionManifestProjection>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct DescriptionManifestProjection {
    pub(crate) status: DescriptionManifestProjectionStatus,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) recording_mbid: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum DescriptionManifestProjectionStatus {
    Ineligible,
    Unresolved,
    Resolved,
}

pub(crate) fn description_manifest(playlist: &UnifiedPlaylist) -> Result<DescriptionManifest> {
    description_manifest_with_recording_mbids(playlist, &BTreeMap::new())
}

pub(crate) fn description_manifest_with_recording_mbids(
    playlist: &UnifiedPlaylist,
    recording_mbids: &BTreeMap<PlaylistEntryId, String>,
) -> Result<DescriptionManifest> {
    let occurrences = playlist
        .items
        .iter()
        .map(|item| item.entry_id)
        .collect::<HashSet<_>>();
    anyhow::ensure!(
        recording_mbids
            .keys()
            .all(|occurrence| occurrences.contains(occurrence)),
        "MusicBrainz projection references an occurrence absent from the playlist"
    );
    let entries = playlist
        .items
        .iter()
        .map(|item| {
            let recording_mbid = recording_mbids
                .get(&item.entry_id)
                .map(|value| value.to_ascii_lowercase());
            anyhow::ensure!(
                recording_mbid.as_deref().is_none_or(is_recording_mbid),
                "MusicBrainz projection has an invalid recording MBID"
            );
            let status = match (item.media_id.kind, recording_mbid.is_some()) {
                (MediaKind::Episode, true) => anyhow::bail!(
                    "MusicBrainz recording projection cannot be attached to an episode"
                ),
                (MediaKind::Episode, false) => DescriptionManifestProjectionStatus::Ineligible,
                (MediaKind::Track | MediaKind::Video, true) => {
                    DescriptionManifestProjectionStatus::Resolved
                }
                (MediaKind::Track | MediaKind::Video, false) => {
                    DescriptionManifestProjectionStatus::Unresolved
                }
            };
            Ok(DescriptionManifestEntry {
                occurrence: item.entry_id.0,
                provider: provider_slug(item.media_id.provider).to_owned(),
                kind: media_kind_slug(item.media_id.kind).to_owned(),
                id: item.media_id.raw_id.clone(),
                projection: Some(DescriptionManifestProjection {
                    status,
                    recording_mbid,
                }),
            })
        })
        .collect::<Result<Vec<_>>>()?;
    let manifest = DescriptionManifest {
        schema_version: CURRENT_MANIFEST_SCHEMA_VERSION,
        playlist_id: playlist.id.clone(),
        snapshot_hash: playlist.snapshot_hash(),
        entries,
    };
    validate_manifest(&manifest)?;
    Ok(manifest)
}

pub(crate) fn description_manifest_fingerprint(manifest: &DescriptionManifest) -> Result<String> {
    use std::fmt::Write as _;

    let canonical_json = serde_json::to_vec(manifest)?;
    Ok(Sha256::digest(canonical_json)
        .iter()
        .fold(String::new(), |mut hex, byte| {
            write!(hex, "{byte:02x}").expect("writing to a String cannot fail");
            hex
        }))
}

pub(crate) fn description_envelope(playlist: &UnifiedPlaylist) -> Result<String> {
    description_envelope_from_manifest(&description_manifest(playlist)?)
}

pub(crate) fn description_envelope_from_manifest(manifest: &DescriptionManifest) -> Result<String> {
    validate_manifest(manifest)?;
    let json = serde_json::to_string(manifest)?;
    Ok(format!(
        "{DESCRIPTION_ENVELOPE_PREFIX}{}",
        escape_html_significant_json(&json)
    ))
}

pub(crate) fn description_envelope_with_budget(
    playlist: &UnifiedPlaylist,
    budget_characters: usize,
) -> Result<String> {
    let envelope = description_envelope(playlist)?;
    let characters = envelope.chars().count();
    anyhow::ensure!(
        characters <= budget_characters,
        "ListenBrainz identity description needs {characters} characters but the configured budget is {budget_characters}; no remote playlist was created"
    );
    Ok(envelope)
}

pub(crate) fn parse_description_manifest(envelope: &str) -> Result<DescriptionManifest> {
    let json = envelope
        .strip_prefix(DESCRIPTION_ENVELOPE_PREFIX)
        .context("ListenBrainz description manifest prefix is missing")?;
    let manifest = serde_json::from_str::<DescriptionManifest>(json)
        .context("parse ListenBrainz description manifest")?;
    validate_manifest(&manifest)?;
    Ok(manifest)
}

pub(crate) fn parse_provider(value: &str) -> Option<Provider> {
    match value {
        "spotify" => Some(Provider::Spotify),
        "youtube-music" => Some(Provider::YouTubeMusic),
        _ => None,
    }
}

pub(crate) fn parse_media_kind(value: &str) -> Option<MediaKind> {
    match value {
        "track" => Some(MediaKind::Track),
        "episode" => Some(MediaKind::Episode),
        "video" => Some(MediaKind::Video),
        _ => None,
    }
}

fn validate_manifest(manifest: &DescriptionManifest) -> Result<()> {
    anyhow::ensure!(
        is_supported_manifest_schema_version(u64::from(manifest.schema_version)),
        "Unsupported ListenBrainz description manifest version"
    );
    anyhow::ensure!(
        !manifest.playlist_id.trim().is_empty(),
        "ListenBrainz description manifest has no playlist ID"
    );
    anyhow::ensure!(
        manifest.snapshot_hash.len() == 64
            && manifest
                .snapshot_hash
                .bytes()
                .all(|byte| byte.is_ascii_hexdigit()),
        "ListenBrainz description manifest has an invalid snapshot hash"
    );
    let mut occurrences = HashSet::with_capacity(manifest.entries.len());
    for entry in &manifest.entries {
        anyhow::ensure!(
            entry.occurrence > 0 && occurrences.insert(entry.occurrence),
            "ListenBrainz description manifest has an invalid or duplicate occurrence"
        );
        anyhow::ensure!(
            parse_provider(&entry.provider).is_some(),
            "ListenBrainz description manifest has an unsupported provider"
        );
        anyhow::ensure!(
            parse_media_kind(&entry.kind).is_some(),
            "ListenBrainz description manifest has an unsupported media kind"
        );
        anyhow::ensure!(
            !entry.id.trim().is_empty(),
            "ListenBrainz description manifest has an empty media ID"
        );
        match (manifest.schema_version, entry.projection.as_ref()) {
            (1, None) => {}
            (1, Some(_)) => anyhow::bail!(
                "ListenBrainz description manifest v1 contains v2 projection metadata"
            ),
            (CURRENT_MANIFEST_SCHEMA_VERSION, Some(projection)) => {
                validate_projection(entry, projection)?;
            }
            (CURRENT_MANIFEST_SCHEMA_VERSION, None) => {
                anyhow::bail!("ListenBrainz description manifest v2 entry has no projection state")
            }
            _ => unreachable!("schema version was validated above"),
        }
    }
    Ok(())
}

fn validate_projection(
    entry: &DescriptionManifestEntry,
    projection: &DescriptionManifestProjection,
) -> Result<()> {
    match projection.status {
        DescriptionManifestProjectionStatus::Ineligible => {
            anyhow::ensure!(
                parse_media_kind(&entry.kind) == Some(MediaKind::Episode)
                    && projection.recording_mbid.is_none(),
                "Ineligible manifest projection must be an episode without a recording MBID"
            );
        }
        DescriptionManifestProjectionStatus::Unresolved => {
            anyhow::ensure!(
                parse_media_kind(&entry.kind) != Some(MediaKind::Episode)
                    && projection.recording_mbid.is_none(),
                "Unresolved manifest projection cannot contain a recording MBID"
            );
        }
        DescriptionManifestProjectionStatus::Resolved => {
            anyhow::ensure!(
                parse_media_kind(&entry.kind) != Some(MediaKind::Episode)
                    && projection
                        .recording_mbid
                        .as_deref()
                        .is_some_and(is_recording_mbid),
                "Resolved manifest projection needs a valid recording MBID"
            );
        }
    }
    Ok(())
}

pub(crate) fn is_recording_mbid(value: &str) -> bool {
    value.len() == 36
        && value.bytes().enumerate().all(|(index, byte)| {
            if matches!(index, 8 | 13 | 18 | 23) {
                byte == b'-'
            } else {
                byte.is_ascii_digit() || matches!(byte, b'a'..=b'f')
            }
        })
}

pub(crate) const fn provider_slug(provider: Provider) -> &'static str {
    match provider {
        Provider::Spotify => "spotify",
        Provider::YouTubeMusic => "youtube-music",
    }
}

const fn media_kind_slug(kind: MediaKind) -> &'static str {
    match kind {
        MediaKind::Track => "track",
        MediaKind::Episode => "episode",
        MediaKind::Video => "video",
    }
}

fn escape_html_significant_json(json: &str) -> String {
    let mut escaped = String::with_capacity(json.len());
    for character in json.chars() {
        match character {
            '<' => escaped.push_str("\\u003c"),
            '>' => escaped.push_str("\\u003e"),
            '&' => escaped.push_str("\\u0026"),
            _ => escaped.push(character),
        }
    }
    escaped
}

#[cfg(test)]
mod tests {
    use super::{
        description_envelope, description_envelope_with_budget, description_manifest,
        description_manifest_fingerprint, description_manifest_with_recording_mbids,
        parse_description_manifest, DescriptionManifestProjectionStatus,
        DESCRIPTION_ENVELOPE_PREFIX,
    };
    use crate::state::{
        MediaId, MediaKind, PlaylistEntryId, Provider, UnifiedPlaylist, UnifiedPlaylistItem,
    };
    use std::collections::BTreeMap;

    fn playlist() -> UnifiedPlaylist {
        UnifiedPlaylist {
            id: "mix<&>".to_owned(),
            name: "Mix".to_owned(),
            items: vec![
                UnifiedPlaylistItem {
                    entry_id: PlaylistEntryId(4),
                    media_id: MediaId {
                        provider: Provider::Spotify,
                        kind: MediaKind::Track,
                        raw_id: "spotify-track".to_owned(),
                    },
                    ..UnifiedPlaylistItem::default()
                },
                UnifiedPlaylistItem {
                    entry_id: PlaylistEntryId(9),
                    media_id: MediaId {
                        provider: Provider::YouTubeMusic,
                        kind: MediaKind::Video,
                        raw_id: "youtube-video".to_owned(),
                    },
                    ..UnifiedPlaylistItem::default()
                },
            ],
            next_entry_id: 10,
            ..UnifiedPlaylist::default()
        }
    }

    #[test]
    fn description_round_trip_preserves_identity_and_escapes_html_significant_text() {
        let playlist = playlist();
        let manifest = description_manifest(&playlist).unwrap();
        let envelope = description_envelope(&playlist).unwrap();
        let parsed = parse_description_manifest(&envelope).unwrap();

        assert!(!envelope.contains('<'));
        assert!(!envelope.contains('>'));
        assert!(!envelope.contains('&'));
        assert_eq!(parsed, manifest);
        assert_eq!(manifest.schema_version, 2);
        assert!(manifest.entries.iter().all(|entry| {
            entry.projection.as_ref().is_some_and(|projection| {
                projection.status == DescriptionManifestProjectionStatus::Unresolved
                    && projection.recording_mbid.is_none()
            })
        }));
        assert_eq!(
            description_manifest_fingerprint(&parsed).unwrap(),
            description_manifest_fingerprint(&manifest).unwrap()
        );
    }

    #[test]
    fn manifest_fingerprint_is_canonical_and_transport_escape_independent() {
        let manifest = description_manifest(&playlist()).unwrap();
        let fingerprint = description_manifest_fingerprint(&manifest).unwrap();
        let envelope = description_envelope(&playlist()).unwrap();

        assert_eq!(fingerprint.len(), 64);
        assert_eq!(
            description_manifest_fingerprint(&parse_description_manifest(&envelope).unwrap())
                .unwrap(),
            fingerprint
        );
        assert!(!fingerprint.contains("mix"));
    }

    #[test]
    fn description_budget_fails_before_returning_a_payload() {
        let error = description_envelope_with_budget(&playlist(), 10)
            .unwrap_err()
            .to_string();

        assert!(error.contains("no remote playlist was created"));
    }

    #[test]
    fn invalid_local_occurrence_fails_before_envelope_creation() {
        let mut playlist = playlist();
        playlist.items[0].entry_id = PlaylistEntryId(0);

        assert!(description_envelope(&playlist)
            .unwrap_err()
            .to_string()
            .contains("invalid or duplicate occurrence"));
    }

    #[test]
    fn v1_manifest_remains_readable_without_projection_metadata() {
        let manifest = description_manifest(&playlist()).unwrap();
        let mut value = serde_json::to_value(&manifest).unwrap();
        value["schema_version"] = serde_json::json!(1);
        for entry in value["entries"].as_array_mut().unwrap() {
            entry.as_object_mut().unwrap().remove("projection");
        }
        let envelope = format!(
            "{DESCRIPTION_ENVELOPE_PREFIX}{}",
            serde_json::to_string(&value).unwrap()
        );

        let parsed = parse_description_manifest(&envelope).unwrap();
        assert_eq!(parsed.schema_version, 1);
        assert!(parsed
            .entries
            .iter()
            .all(|entry| entry.projection.is_none()));
    }

    #[test]
    fn v2_projection_requires_explicit_valid_occurrence_mapping() {
        let mut playlist = playlist();
        playlist.items.push(UnifiedPlaylistItem {
            entry_id: PlaylistEntryId(10),
            media_id: MediaId {
                provider: Provider::Spotify,
                kind: MediaKind::Episode,
                raw_id: "spotify-episode".to_owned(),
            },
            ..UnifiedPlaylistItem::default()
        });
        let mbid = "12345678-1234-1234-1234-123456789abc";
        let manifest = description_manifest_with_recording_mbids(
            &playlist,
            &BTreeMap::from([(PlaylistEntryId(4), mbid.to_uppercase())]),
        )
        .unwrap();

        assert_eq!(
            manifest.entries[0].projection.as_ref().unwrap().status,
            DescriptionManifestProjectionStatus::Resolved
        );
        assert_eq!(
            manifest.entries[0]
                .projection
                .as_ref()
                .unwrap()
                .recording_mbid
                .as_deref(),
            Some(mbid)
        );
        assert_eq!(
            manifest.entries[2].projection.as_ref().unwrap().status,
            DescriptionManifestProjectionStatus::Ineligible
        );
        assert!(manifest.entries[2]
            .projection
            .as_ref()
            .unwrap()
            .recording_mbid
            .is_none());
        assert!(description_manifest_with_recording_mbids(
            &playlist,
            &BTreeMap::from([(PlaylistEntryId(99), mbid.to_owned())])
        )
        .unwrap_err()
        .to_string()
        .contains("occurrence absent"));
        assert!(description_manifest_with_recording_mbids(
            &playlist,
            &BTreeMap::from([(PlaylistEntryId(4), "not-an-mbid".to_owned())])
        )
        .unwrap_err()
        .to_string()
        .contains("invalid recording MBID"));
        assert!(description_manifest_with_recording_mbids(
            &playlist,
            &BTreeMap::from([(PlaylistEntryId(10), mbid.to_owned())])
        )
        .unwrap_err()
        .to_string()
        .contains("cannot be attached to an episode"));
    }

    #[test]
    fn v2_rejects_missing_or_inconsistent_projection_state() {
        let manifest = description_manifest(&playlist()).unwrap();
        let mut missing = serde_json::to_value(&manifest).unwrap();
        missing["entries"][0]
            .as_object_mut()
            .unwrap()
            .remove("projection");
        let missing_envelope = format!(
            "{DESCRIPTION_ENVELOPE_PREFIX}{}",
            serde_json::to_string(&missing).unwrap()
        );
        assert!(parse_description_manifest(&missing_envelope)
            .unwrap_err()
            .to_string()
            .contains("no projection state"));

        let mut inconsistent = serde_json::to_value(&manifest).unwrap();
        inconsistent["entries"][0]["projection"] = serde_json::json!({
            "status": "resolved"
        });
        let inconsistent_envelope = format!(
            "{DESCRIPTION_ENVELOPE_PREFIX}{}",
            serde_json::to_string(&inconsistent).unwrap()
        );
        assert!(parse_description_manifest(&inconsistent_envelope)
            .unwrap_err()
            .to_string()
            .contains("valid recording MBID"));
    }
}
