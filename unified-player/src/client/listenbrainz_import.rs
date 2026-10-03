use anyhow::Result;

const UNKNOWN_TRACK_TITLE: &str = "Unknown track";

fn non_empty_text(value: Option<&serde_json::Value>) -> Option<&str> {
    value
        .and_then(serde_json::Value::as_str)
        .filter(|text| !text.trim().is_empty())
}

pub(crate) fn listenbrainz_recording_mbid(identifier: &str) -> Option<String> {
    let url = reqwest::Url::parse(identifier).ok()?;
    if url.scheme() != "https" || url.host_str() != Some("musicbrainz.org") {
        return None;
    }
    let mut segments = url.path_segments()?;
    if segments.next()? != "recording" {
        return None;
    }
    let mbid = segments.next()?;
    (segments.next().is_none() && crate::cli::listenbrainz_manifest::is_recording_mbid(mbid))
        .then_some(mbid.to_owned())
}

pub(crate) fn parse_listenbrainz_playlist(
    value: &serde_json::Value,
) -> Result<(String, Vec<crate::state::UnifiedPlaylistItem>, usize)> {
    let playlist = value.get("playlist").unwrap_or(value);
    let name = playlist
        .get("title")
        .and_then(serde_json::Value::as_str)
        .unwrap_or("ListenBrainz playlist")
        .to_owned();
    let tracks = playlist
        .get("track")
        .and_then(serde_json::Value::as_array)
        .cloned()
        .unwrap_or_default();
    let mut items = Vec::with_capacity(tracks.len());
    let mut unresolved = 0;
    for (index, track) in tracks.iter().cloned().enumerate() {
        let identifiers = track
            .get("identifier")
            .and_then(serde_json::Value::as_array);
        let identifier = identifiers.and_then(|ids| {
            ids.iter()
                .filter_map(serde_json::Value::as_str)
                .find(|id| listenbrainz_identifier_provider(id).is_some())
                .or_else(|| {
                    ids.iter()
                        .filter_map(serde_json::Value::as_str)
                        .find(|id| listenbrainz_recording_mbid(id).is_some())
                })
                .or_else(|| ids.iter().find_map(serde_json::Value::as_str))
        });
        let extension = track.get("extension");
        let provider_hint = extension
            .and_then(|extension| extension.get("unified-player:provider"))
            .and_then(serde_json::Value::as_str);
        let raw_id = extension
            .and_then(|extension| extension.get("unified-player:raw_id"))
            .and_then(serde_json::Value::as_str)
            .or_else(|| identifier.and_then(listenbrainz_identifier_raw_id));
        let provider_from_hint = provider_hint.and_then(|provider| match provider {
            "Spotify" => Some(crate::state::Provider::Spotify),
            "YouTubeMusic" => Some(crate::state::Provider::YouTubeMusic),
            _ => None,
        });
        let invalid_provider_hint = provider_hint.is_some() && provider_from_hint.is_none();
        let resolved_provider = if invalid_provider_hint {
            None
        } else {
            provider_from_hint.or_else(|| identifier.and_then(listenbrainz_identifier_provider))
        };
        let provider_was_resolved = resolved_provider.is_some();
        let provider = resolved_provider.unwrap_or(crate::state::Provider::Spotify);
        let Some(raw_id) = raw_id.or(identifier) else {
            unresolved += 1;
            // The stable index is only a local placeholder; the source row is
            // retained in metadata instead of being silently discarded.
            let raw_id = format!("unresolved:{index}");
            let title = non_empty_text(track.get("title"))
                .unwrap_or(UNKNOWN_TRACK_TITLE)
                .to_owned();
            let artists = non_empty_text(track.get("creator"))
                .unwrap_or_default()
                .to_owned();
            let metadata_pending = non_empty_text(track.get("title")).is_none()
                || non_empty_text(track.get("creator")).is_none()
                || track
                    .get("duration")
                    .and_then(serde_json::Value::as_u64)
                    .is_none();
            items.push(crate::state::UnifiedPlaylistItem {
                entry_id: extension
                    .and_then(|extension| extension.get("unified-player:entry_id"))
                    .and_then(serde_json::Value::as_u64)
                    .map(crate::state::PlaylistEntryId)
                    .unwrap_or_default(),
                media_id: crate::state::MediaId {
                    provider,
                    kind: crate::state::MediaKind::Track,
                    raw_id,
                },
                title,
                artists,
                duration_ms: None,
                provider_url: identifier.map(str::to_owned),
                metadata: crate::state::UnifiedPlaylistMetadata {
                    provenance: extension
                        .and_then(|extension| extension.get("unified-player:metadata_provenance"))
                        .and_then(serde_json::Value::as_str)
                        .map(str::to_owned)
                        .or_else(|| Some("unresolved-listenbrainz".to_owned())),
                    observed_at: extension
                        .and_then(|extension| extension.get("unified-player:metadata_observed_at"))
                        .and_then(serde_json::Value::as_u64),
                    source_identifier: identifier.map(str::to_owned),
                    source_entry_id: extension
                        .and_then(|extension| extension.get("unified-player:entry_id"))
                        .and_then(serde_json::Value::as_u64),
                    degraded: true,
                    metadata_pending,
                },
                ..crate::state::UnifiedPlaylistItem::default()
            });
            continue;
        };
        let title = non_empty_text(track.get("title"));
        let artists = non_empty_text(track.get("creator"));
        let duration_value = track.get("duration").and_then(serde_json::Value::as_u64);
        let duration_unit = extension
            .and_then(|extension| extension.get("unified-player:duration_unit"))
            .and_then(serde_json::Value::as_str)
            .unwrap_or("milliseconds");
        let duration_ms = duration_value.and_then(|value| match duration_unit {
            "seconds" => value.checked_mul(1_000),
            _ => Some(value),
        });
        let kind = extension
            .and_then(|extension| extension.get("unified-player:media_kind"))
            .and_then(serde_json::Value::as_str)
            .and_then(|kind| match kind {
                "Track" => Some(crate::state::MediaKind::Track),
                "Video" => Some(crate::state::MediaKind::Video),
                "Episode" => Some(crate::state::MediaKind::Episode),
                _ => None,
            })
            .unwrap_or_else(|| {
                if identifier.is_some_and(|id| {
                    id.starts_with("spotify:episode:")
                        || reqwest::Url::parse(id).is_ok_and(|url| {
                            url.host_str() == Some("open.spotify.com")
                                && url.path().starts_with("/episode/")
                        })
                }) {
                    crate::state::MediaKind::Episode
                } else {
                    crate::state::MediaKind::Track
                }
            });
        let degraded = !provider_was_resolved;
        let metadata_pending = title.is_none() || artists.is_none() || duration_ms.is_none();
        if degraded || metadata_pending {
            unresolved += 1;
        }
        items.push(crate::state::UnifiedPlaylistItem {
            entry_id: extension
                .and_then(|extension| extension.get("unified-player:entry_id"))
                .and_then(serde_json::Value::as_u64)
                .map(crate::state::PlaylistEntryId)
                .unwrap_or_default(),
            media_id: crate::state::MediaId {
                provider,
                kind,
                raw_id: raw_id.to_owned(),
            },
            title: title.unwrap_or(UNKNOWN_TRACK_TITLE).to_owned(),
            artists: artists.unwrap_or_default().to_owned(),
            duration_ms,
            provider_url: identifier.map(str::to_owned),
            duration_unit: crate::state::DurationUnit::Milliseconds,
            metadata: crate::state::UnifiedPlaylistMetadata {
                provenance: Some(if provider_was_resolved {
                    extension
                        .and_then(|extension| extension.get("unified-player:metadata_provenance"))
                        .and_then(serde_json::Value::as_str)
                        .unwrap_or("listenbrainz")
                        .to_owned()
                } else {
                    "unresolved-listenbrainz".to_owned()
                }),
                observed_at: extension
                    .and_then(|extension| extension.get("unified-player:metadata_observed_at"))
                    .and_then(serde_json::Value::as_u64),
                source_identifier: identifier.map(str::to_owned),
                source_entry_id: extension
                    .and_then(|extension| extension.get("unified-player:entry_id"))
                    .and_then(serde_json::Value::as_u64),
                degraded,
                metadata_pending,
            },
        });
    }
    let Some(description) = playlist
        .get("annotation")
        .and_then(serde_json::Value::as_str)
        .filter(|description| {
            description.starts_with(crate::cli::listenbrainz_manifest::DESCRIPTION_ENVELOPE_PREFIX)
        })
    else {
        return Ok((name, items, unresolved));
    };
    let manifest = crate::cli::listenbrainz_manifest::parse_description_manifest(description)?;
    let metadata_matches = match_manifest_metadata(&manifest, &items, &tracks);
    let manifest_items = manifest
        .entries
        .iter()
        .enumerate()
        .map(|(index, entry)| {
            let fallback = metadata_matches[index].and_then(|row| items.get(row));
            let provider = crate::cli::listenbrainz_manifest::parse_provider(&entry.provider)
                .expect("validated manifest provider");
            let kind = crate::cli::listenbrainz_manifest::parse_media_kind(&entry.kind)
                .expect("validated manifest media kind");
            crate::state::UnifiedPlaylistItem {
                entry_id: crate::state::PlaylistEntryId(entry.occurrence),
                media_id: crate::state::MediaId {
                    provider,
                    kind,
                    raw_id: entry.id.clone(),
                },
                title: fallback
                    .map(|item| item.title.clone())
                    .filter(|title| !title.trim().is_empty())
                    .unwrap_or_else(|| UNKNOWN_TRACK_TITLE.to_owned()),
                artists: fallback
                    .map(|item| item.artists.clone())
                    .unwrap_or_default(),
                duration_ms: fallback.and_then(|item| item.duration_ms),
                duration_unit: crate::state::DurationUnit::Milliseconds,
                provider_url: Some(listenbrainz_manifest_provider_url(
                    provider, kind, &entry.id,
                )),
                metadata: crate::state::UnifiedPlaylistMetadata {
                    provenance: Some("listenbrainz-description-manifest".to_owned()),
                    source_identifier: fallback
                        .and_then(|item| item.metadata.source_identifier.clone()),
                    source_entry_id: Some(entry.occurrence),
                    degraded: fallback.is_some_and(|item| item.metadata.degraded),
                    metadata_pending: fallback.is_none_or(|item| item.metadata.metadata_pending),
                    ..crate::state::UnifiedPlaylistMetadata::default()
                },
            }
        })
        .collect::<Vec<_>>();
    let restored = crate::state::UnifiedPlaylist {
        id: manifest.playlist_id,
        name: name.clone(),
        items: manifest_items.clone(),
        updated_at: 0,
        next_entry_id: 1,
    };
    anyhow::ensure!(
        restored.snapshot_hash() == manifest.snapshot_hash,
        "ListenBrainz description manifest does not match the remote playlist title or identity order"
    );
    let unresolved = manifest_items
        .iter()
        .filter(|item| item.metadata.degraded || item.metadata.metadata_pending)
        .count();
    Ok((name, manifest_items, unresolved))
}

fn listenbrainz_manifest_provider_url(
    provider: crate::state::Provider,
    kind: crate::state::MediaKind,
    raw_id: &str,
) -> String {
    match provider {
        crate::state::Provider::Spotify => match kind {
            crate::state::MediaKind::Episode => format!("spotify:episode:{raw_id}"),
            crate::state::MediaKind::Track | crate::state::MediaKind::Video => {
                format!("spotify:track:{raw_id}")
            }
        },
        crate::state::Provider::YouTubeMusic => {
            format!("https://music.youtube.com/watch?v={raw_id}")
        }
    }
}

fn listenbrainz_identifier_raw_id(identifier: &str) -> Option<&str> {
    for prefix in ["spotify:track:", "spotify:episode:"] {
        if let Some(id) = identifier.strip_prefix(prefix) {
            return (!id.is_empty() && !id.contains(['/', '?', ':', '#'])).then_some(id);
        }
    }
    let url = reqwest::Url::parse(identifier).ok()?;
    if !matches!(url.scheme(), "http" | "https") {
        return None;
    }
    match url.host_str()? {
        "open.spotify.com"
            if url.path().starts_with("/track/") || url.path().starts_with("/episode/") =>
        {
            let kind = if url.path().starts_with("/track/") {
                "/track/"
            } else {
                "/episode/"
            };
            let id = identifier
                .split_once(kind)?
                .1
                .split(['?', '#', '/'])
                .next()?;
            (!id.is_empty()).then_some(id)
        }
        "youtube.com" | "www.youtube.com" | "music.youtube.com" if url.path() == "/watch" => {
            identifier
                .split_once('?')?
                .1
                .split('#')
                .next()?
                .split('&')
                .find_map(|part| part.strip_prefix("v="))
                .filter(|id| !id.is_empty())
        }
        "youtu.be" | "www.youtu.be" => {
            let id = identifier
                .split_once("://")?
                .1
                .split_once('/')?
                .1
                .split(['?', '#', '/'])
                .next()?;
            (!id.is_empty()).then_some(id)
        }
        _ => None,
    }
}
fn listenbrainz_identifier_provider(identifier: &str) -> Option<crate::state::Provider> {
    listenbrainz_identifier_raw_id(identifier)?;
    if identifier.starts_with("spotify:") {
        return Some(crate::state::Provider::Spotify);
    }
    match reqwest::Url::parse(identifier).ok()?.host_str()? {
        "open.spotify.com" => Some(crate::state::Provider::Spotify),
        "youtube.com" | "www.youtube.com" | "music.youtube.com" | "youtu.be" | "www.youtu.be" => {
            Some(crate::state::Provider::YouTubeMusic)
        }
        _ => None,
    }
}

/// Live imports reject malformed wire data before the permissive legacy parser runs.
pub(crate) fn parse_live_playlist(
    value: &serde_json::Value,
) -> Result<(String, Vec<crate::state::UnifiedPlaylistItem>, usize)> {
    let playlist = value
        .get("playlist")
        .and_then(serde_json::Value::as_object)
        .ok_or_else(|| anyhow::anyhow!("ListenBrainz returned an invalid playlist."))?;
    anyhow::ensure!(
        playlist
            .get("title")
            .and_then(serde_json::Value::as_str)
            .is_some(),
        "ListenBrainz playlist title is missing."
    );
    anyhow::ensure!(
        playlist
            .get("annotation")
            .is_none_or(serde_json::Value::is_string)
            && playlist
                .get("extension")
                .is_none_or(serde_json::Value::is_object),
        "ListenBrainz returned invalid playlist metadata."
    );
    match playlist.get("track") {
        Some(serde_json::Value::Array(tracks)) => {
            anyhow::ensure!(
                tracks.iter().all(|track| track.is_object()
                    && ["title", "creator"]
                        .iter()
                        .all(|key| track.get(key).is_none_or(serde_json::Value::is_string))
                    && track
                        .get("duration")
                        .is_none_or(|value| value.is_null() || value.as_u64().is_some())
                    && track
                        .get("extension")
                        .is_none_or(serde_json::Value::is_object)
                    && track.get("identifier").is_none_or(|ids| ids
                        .as_array()
                        .is_some_and(|ids| ids.iter().all(serde_json::Value::is_string)))),
                "ListenBrainz returned invalid playlist tracks."
            );
        }
        None if playlist
            .get("annotation")
            .and_then(serde_json::Value::as_str)
            .is_some_and(|s| {
                s.starts_with(crate::cli::listenbrainz_manifest::DESCRIPTION_ENVELOPE_PREFIX)
            }) => {}
        _ => anyhow::bail!("ListenBrainz playlist tracks are missing or invalid."),
    }
    parse_listenbrainz_playlist(value)
}

fn match_manifest_metadata(
    manifest: &crate::cli::listenbrainz_manifest::DescriptionManifest,
    items: &[crate::state::UnifiedPlaylistItem],
    tracks: &[serde_json::Value],
) -> Vec<Option<usize>> {
    let mut matched = vec![None; manifest.entries.len()];
    let mut consumed = vec![false; items.len()];
    // Complete each priority across all entries before any weaker match can consume a row.
    for priority in 0..3 {
        for (index, entry) in manifest.entries.iter().enumerate() {
            if matched[index].is_some() {
                continue;
            }
            let provider = crate::cli::listenbrainz_manifest::parse_provider(&entry.provider)
                .expect("validated provider");
            let kind = crate::cli::listenbrainz_manifest::parse_media_kind(&entry.kind)
                .expect("validated kind");
            for (row, item) in items.iter().enumerate() {
                if consumed[row] {
                    continue;
                }
                let ext = tracks[row].get("extension");
                let occurrence = ext.and_then(|e| e.get("unified-player:entry_id"));
                if occurrence.is_some_and(|v| v.as_u64() != Some(entry.occurrence)) {
                    continue;
                }
                let contradictory = [
                    (
                        "unified-player:provider",
                        match provider {
                            crate::state::Provider::Spotify => "Spotify",
                            crate::state::Provider::YouTubeMusic => "YouTubeMusic",
                        },
                    ),
                    (
                        "unified-player:media_kind",
                        match kind {
                            crate::state::MediaKind::Track => "Track",
                            crate::state::MediaKind::Episode => "Episode",
                            crate::state::MediaKind::Video => "Video",
                        },
                    ),
                    ("unified-player:raw_id", entry.id.as_str()),
                ]
                .into_iter()
                .any(|(key, expected)| {
                    ext.and_then(|e| e.get(key))
                        .is_some_and(|v| v.as_str() != Some(expected))
                });
                if contradictory {
                    continue;
                }
                let same_identity = item.media_id.provider == provider
                    && item.media_id.kind == kind
                    && item.media_id.raw_id == entry.id;
                if !item.metadata.degraded && !same_identity {
                    continue;
                }
                let matches = match priority {
                    0 => occurrence.and_then(serde_json::Value::as_u64) == Some(entry.occurrence),
                    1 => !item.metadata.degraded && same_identity,
                    _ => entry.projection.as_ref().filter(|p| p.status == crate::cli::listenbrainz_manifest::DescriptionManifestProjectionStatus::Resolved)
                        .and_then(|p| p.recording_mbid.as_deref()).is_some_and(|mbid| tracks[row].get("identifier").and_then(serde_json::Value::as_array).is_some_and(|ids| ids.iter().filter_map(serde_json::Value::as_str).any(|id| id.strip_prefix("https://musicbrainz.org/recording/").or_else(|| id.strip_prefix("http://musicbrainz.org/recording/")) == Some(mbid)))),
                };
                if matches {
                    matched[index] = Some(row);
                    consumed[row] = true;
                    break;
                }
            }
        }
    }
    matched
}

/// Caller holds the data write lock through collision checks and rollback-safe persistence.
pub(crate) fn import_local_playlist(
    data: &mut crate::state::AppData,
    remote_id: &str,
    name: String,
    items: Vec<crate::state::UnifiedPlaylistItem>,
) -> Result<()> {
    let local_id = format!("listenbrainz-{remote_id}");
    anyhow::ensure!(
        !data
            .playlist_links
            .iter()
            .any(|link| link.listenbrainz_playlist_id.as_deref() == Some(remote_id)),
        "This ListenBrainz playlist is already imported. No local data changed."
    );
    anyhow::ensure!(
        !data
            .unified_playlists
            .iter()
            .any(|playlist| playlist.id == local_id)
            && !data
                .playlist_links
                .iter()
                .any(|link| link.unified_playlist_id == local_id),
        "A local playlist or link already uses this import ID. No local data changed."
    );
    let updated_at = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)?
        .as_secs();
    let playlist = crate::state::UnifiedPlaylist {
        id: local_id.clone(),
        name,
        items,
        updated_at,
        next_entry_id: 1,
    };
    let link = crate::state::PlaylistLink {
        unified_playlist_id: local_id,
        listenbrainz_playlist_id: Some(remote_id.to_owned()),
        updated_at,
        ..Default::default()
    };
    data.upsert_unified_playlist_with_link(playlist, link)
        .map_err(|_| {
            anyhow::anyhow!("Could not save imported playlist. Existing local data retained.")
        })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cli::listenbrainz_manifest::{
        description_envelope_from_manifest, description_manifest_with_recording_mbids,
    };
    use crate::state::{
        AppData, MediaId, MediaKind, PlaylistEntryId, PlaylistLink, Provider, UnifiedPlaylist,
        UnifiedPlaylistItem,
    };
    use serde_json::{json, Value};
    const MBID: &str = "12345678-1234-1234-1234-123456789abc";
    fn playlist() -> UnifiedPlaylist {
        UnifiedPlaylist {
            id: "original".to_owned(),
            name: "Saved".to_owned(),
            items: ["a", "b"]
                .into_iter()
                .enumerate()
                .map(|(index, id)| UnifiedPlaylistItem {
                    entry_id: PlaylistEntryId(index as u64 + 1),
                    media_id: MediaId {
                        provider: Provider::Spotify,
                        kind: MediaKind::Track,
                        raw_id: id.to_owned(),
                    },
                    title: format!("Title {id}"),
                    artists: "Artist".to_owned(),
                    ..Default::default()
                })
                .collect(),
            next_entry_id: 3,
            ..Default::default()
        }
    }
    fn wire(
        playlist: &UnifiedPlaylist,
        projections: &[(u64, &str)],
        tracks: Option<Vec<Value>>,
    ) -> Value {
        let mbids = projections
            .iter()
            .map(|(occurrence, mbid)| (PlaylistEntryId(*occurrence), mbid.to_string()))
            .collect();
        let manifest = description_manifest_with_recording_mbids(playlist, &mbids).unwrap();
        let mut value = json!({"playlist":{"title":playlist.name,"annotation":description_envelope_from_manifest(&manifest).unwrap()}});
        if let Some(tracks) = tracks {
            value["playlist"]["track"] = json!(tracks);
        }
        value
    }
    #[test]
    fn listenbrainz_import_sparse_projection_uses_identity_not_position() {
        let value = wire(
            &playlist(),
            &[(2, MBID)],
            Some(vec![
                json!({"identifier":["https://example.com/other",format!("https://musicbrainz.org/recording/{MBID}")],"title":"Second","creator":"Second artist","duration":123}),
            ]),
        );
        let (_, items, unresolved) = parse_live_playlist(&value).unwrap();
        assert_eq!(items[0].title, UNKNOWN_TRACK_TITLE);
        assert!(!items[0].metadata.degraded);
        assert!(items[0].metadata.metadata_pending);
        assert!(items[0].artists.is_empty());
        assert_eq!(items[0].duration_ms, None);
        assert_eq!(items[1].title, "Second");
        assert_eq!(items[1].duration_ms, Some(123));
        assert_eq!(
            items.iter().map(|i| i.entry_id.0).collect::<Vec<_>>(),
            [1, 2]
        );
        assert_eq!(unresolved, 2);
    }
    #[test]
    fn listenbrainz_import_stronger_occurrences_are_reserved_before_identity_matches() {
        let mut source = playlist();
        source.items[1].media_id = source.items[0].media_id.clone();
        let value = wire(
            &source,
            &[],
            Some(vec![
                json!({"identifier":["spotify:track:a"],"title":"Second","extension":{"unified-player:entry_id":2,"unified-player:provider":"Spotify"}}),
                json!({"identifier":["spotify:track:a"],"title":"First"}),
            ]),
        );
        let (_, items, _) = parse_live_playlist(&value).unwrap();
        assert_eq!(
            items.iter().map(|i| i.title.as_str()).collect::<Vec<_>>(),
            ["First", "Second"]
        );
        assert_ne!(items[0].entry_id, items[1].entry_id);
    }
    #[test]
    fn listenbrainz_import_duplicate_mbids_consume_rows_once_and_respect_conflicts() {
        let source = playlist();
        let value = wire(
            &source,
            &[(1, MBID), (2, MBID)],
            Some(vec![
                json!({"identifier":[format!("https://musicbrainz.org/recording/{MBID}")],"title":"First"}),
                json!({"identifier":[format!("https://musicbrainz.org/recording/{MBID}")],"title":"Second","extension":{"unified-player:entry_id":2}}),
            ]),
        );
        let (_, items, _) = parse_live_playlist(&value).unwrap();
        assert_eq!(items[0].title, "First");
        assert_eq!(items[1].title, "Second");
        let conflicting = wire(
            &source,
            &[(1, MBID)],
            Some(vec![
                json!({"identifier":[format!("https://musicbrainz.org/recording/{MBID}")],"title":"Wrong","extension":{"unified-player:entry_id":1,"unified-player:raw_id":"wrong"}}),
            ]),
        );
        assert_eq!(
            parse_live_playlist(&conflicting).unwrap().1[0].title,
            UNKNOWN_TRACK_TITLE
        );
    }
    #[test]
    fn listenbrainz_import_annotation_only_and_full_legacy_metadata_roundtrip() {
        let source = playlist();
        let (_, restored, unresolved) = parse_live_playlist(&wire(&source, &[], None)).unwrap();
        assert_eq!(restored.len(), 2);
        assert_eq!(unresolved, 2);
        assert!(restored
            .iter()
            .all(|item| item.title == UNKNOWN_TRACK_TITLE && item.metadata.metadata_pending));
        let mut value = source.to_jspf_value();
        value["playlist"]["annotation"] =
            wire(&source, &[], None)["playlist"]["annotation"].clone();
        let (_, restored, _) = parse_live_playlist(&value).unwrap();
        assert_eq!(restored[0].title, source.items[0].title);
        assert_eq!(restored[1].title, source.items[1].title);
    }
    #[test]
    fn listenbrainz_import_rejects_malformed_wire_and_retains_unresolved_rows() {
        for value in [
            json!({}),
            json!({"playlist":{}}),
            json!({"playlist":{"title":"x","track":{}}}),
            json!({"playlist":{"title":"x","track":[2]}}),
            json!({"playlist":{"title":"x","track":[],"annotation":3}}),
            json!({"playlist":{"title":"x","track":[{"title":3}]}}),
            json!({"playlist":{"title":"x","track":[{"identifier":"wrong"}]}}),
            json!({"playlist":{"title":"x","annotation":"unified-player-manifest:not-json"}}),
        ] {
            assert!(parse_live_playlist(&value).is_err());
        }
        assert!(
            parse_live_playlist(&json!({"playlist":{"title":"Empty","track":[]}}))
                .unwrap()
                .1
                .is_empty()
        );
        let (_, items, unresolved) = parse_live_playlist(&json!({"playlist":{"title":"Unknown","track":[{"title":"Missing ID"},{"identifier":["https://musicbrainz.org/recording/unknown"]}]}})).unwrap();
        assert_eq!(items.len(), 2);
        assert_eq!(unresolved, 2);
        assert!(items.iter().all(|i| i.metadata.degraded));
    }
    #[test]
    fn listenbrainz_import_rejects_lookalike_hosts_and_reads_all_identifiers() {
        for id in [
            "https://example.com/youtube.com?v=x",
            "https://youtube.com.evil.test/watch?v=x",
            "https://evil.test/?v=x",
            "https://youtube.com/not-watch?v=x",
        ] {
            assert!(listenbrainz_identifier_provider(id).is_none());
        }
        let (_, items, unresolved) = parse_listenbrainz_playlist(&json!({"playlist":{"title":"x","track":[{"identifier":["https://example.com/unknown","https://music.youtube.com/watch?v=good"]}]}})).unwrap();
        assert_eq!(unresolved, 1);
        assert_eq!(items[0].media_id.raw_id, "good");
        assert_eq!(items[0].media_id.provider, Provider::YouTubeMusic);
        assert_eq!(items[0].title, UNKNOWN_TRACK_TITLE);
        assert!(!items[0].metadata.degraded);
        assert!(items[0].metadata.metadata_pending);
    }

    #[test]
    fn sparse_rows_prefer_recording_identity_without_using_it_as_display_title() {
        let recording = "12345678-1234-1234-1234-123456789abc";
        let value = json!({"playlist":{"title":"Sparse","track":[{
            "identifier":[
                "https://example.com/opaque",
                format!("https://musicbrainz.org/recording/{recording}")
            ]
        }]}});
        let (_, items, unresolved) = parse_live_playlist(&value).unwrap();
        assert_eq!(unresolved, 1);
        assert_eq!(
            items[0].metadata.source_identifier.as_deref(),
            Some(format!("https://musicbrainz.org/recording/{recording}").as_str())
        );
        assert_eq!(
            items[0].media_id.raw_id,
            format!("https://musicbrainz.org/recording/{recording}")
        );
        assert_eq!(items[0].title, UNKNOWN_TRACK_TITLE);
        assert!(items[0].metadata.degraded);
    }
    #[test]
    fn listenbrainz_import_is_local_non_overwriting_and_collision_safe() {
        crate::ui::initialize_test_config();
        let dir = tempfile::tempdir().unwrap();
        let mut data = AppData::new(dir.path(), &dir.path().join("cache"));
        import_local_playlist(&mut data, MBID, "Imported".to_owned(), playlist().items).unwrap();
        assert_eq!(data.unified_playlists.len(), 1);
        assert_eq!(data.playlist_links.len(), 1);
        assert!(
            import_local_playlist(&mut data, MBID, "Overwrite".to_owned(), Vec::new()).is_err()
        );
        assert_eq!(data.unified_playlists[0].name, "Imported");
        data.playlist_links.clear();
        assert!(
            import_local_playlist(&mut data, MBID, "Overwrite".to_owned(), Vec::new()).is_err()
        );
        data.unified_playlists.clear();
        data.playlist_links.push(PlaylistLink {
            unified_playlist_id: format!("listenbrainz-{MBID}"),
            ..Default::default()
        });
        assert!(
            import_local_playlist(&mut data, MBID, "Overwrite".to_owned(), Vec::new()).is_err()
        );
        assert!(data.unified_playlists.is_empty());
    }

    #[test]
    fn hydrated_import_metadata_survives_restart() {
        crate::ui::initialize_test_config();
        let dir = tempfile::tempdir().unwrap();
        let item = UnifiedPlaylistItem {
            entry_id: PlaylistEntryId(1),
            media_id: MediaId {
                provider: Provider::Spotify,
                kind: MediaKind::Track,
                raw_id: "spotify-track".to_owned(),
            },
            title: "Hydrated title".to_owned(),
            artists: "Hydrated artist".to_owned(),
            duration_ms: Some(123_000),
            metadata: crate::state::UnifiedPlaylistMetadata {
                provenance: Some("listenbrainz-spotify-api".to_owned()),
                observed_at: Some(42),
                degraded: false,
                ..Default::default()
            },
            ..Default::default()
        };
        {
            let mut data = AppData::new(dir.path(), &dir.path().join("cache"));
            import_local_playlist(&mut data, MBID, "Hydrated".to_owned(), vec![item.clone()])
                .unwrap();
        }
        let data = AppData::new(dir.path(), &dir.path().join("cache"));
        let restored = &data.unified_playlists[0].items[0];
        assert_eq!(restored.title, item.title);
        assert_eq!(restored.artists, item.artists);
        assert_eq!(restored.duration_ms, item.duration_ms);
        assert_eq!(restored.metadata, item.metadata);
    }
    #[test]
    fn listenbrainz_import_persistence_failure_rolls_back_both_collections() {
        crate::ui::initialize_test_config();
        let dir = tempfile::tempdir().unwrap();
        let mut data = AppData::new(dir.path(), &dir.path().join("cache"));
        std::fs::create_dir(dir.path().join("unified_playlists.json")).unwrap();
        assert!(
            import_local_playlist(&mut data, MBID, "Imported".to_owned(), playlist().items)
                .is_err()
        );
        assert!(data.unified_playlists.is_empty());
        assert!(data.playlist_links.is_empty());
    }
}

#[cfg(test)]
mod episode_tests {
    #[test]
    fn listenbrainz_import_episode_identifier_preserves_media_kind() {
        for identifier in [
            "spotify:episode:episode",
            "https://open.spotify.com/episode/episode",
        ] {
            let (_, items, unresolved) = super::parse_live_playlist(&serde_json::json!({
                "playlist": {"title": "Episode", "track": [{"identifier": [identifier]}]}
            }))
            .unwrap();
            assert_eq!(unresolved, 1);
            assert_eq!(items[0].media_id.kind, crate::state::MediaKind::Episode);
            assert_eq!(items[0].media_id.raw_id, "episode");
            assert_eq!(items[0].title, "Unknown track");
            assert!(!items[0].metadata.degraded);
            assert!(items[0].metadata.metadata_pending);
        }
    }
}

#[cfg(test)]
mod native_wire_tests {
    #[test]
    fn listenbrainz_import_accepts_official_native_wire_without_optional_metadata() {
        // Upstream Playlist.serialize_jspf omits absent annotation/title/creator/duration.
        let value = serde_json::json!({"playlist":{
            "title":"Native playlist", "creator":"owner",
            "identifier":"https://listenbrainz.org/playlist/12345678-1234-1234-1234-123456789abc",
            "extension":{"https://musicbrainz.org/doc/jspf#playlist":{"public":true,"creator":"owner"}},
            "track":[{"identifier":["https://musicbrainz.org/recording/12345678-1234-1234-1234-123456789abc"],
                "extension":{"https://musicbrainz.org/doc/jspf#track":{"added_by":"owner","added_at":"2026-09-13T00:00:00+00:00"}}}]
        }});
        let (name, items, unresolved) = super::parse_live_playlist(&value).unwrap();
        assert_eq!(name, "Native playlist");
        assert_eq!(items.len(), 1);
        assert_eq!(unresolved, 1);
        assert!(items[0].artists.is_empty());
        assert_eq!(items[0].duration_ms, None);
    }
}
