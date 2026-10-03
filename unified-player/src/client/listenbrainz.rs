use anyhow::Result;
use reqwest::{header, StatusCode};
use serde::Deserialize;
use std::collections::{HashMap, HashSet};

use crate::state::{
    ListenBrainzArtistEnrichment, ListenBrainzCollectionStatus, ListenBrainzPopularRecording,
    ListenBrainzReleaseGroup,
};

const LISTENBRAINZ_API_ROOT: &str = "https://api.listenbrainz.org/1";
const MUSICBRAINZ_API_ROOT: &str = "https://musicbrainz.org/ws/2";
const MUSICBRAINZ_USER_AGENT: &str = concat!(
    "unified-player/",
    env!("CARGO_PKG_VERSION"),
    " (https://github.com/bababoyy/unified-player)"
);
const ARTIST_RECORDING_LIMIT: usize = 10;

#[derive(Debug, PartialEq, Eq)]
pub(crate) enum PlaylistBackupError {
    CreateRequest,
    CreateRejected(StatusCode),
    CreateResponse,
    InvalidPlaylistMbid,
    DescriptionRequest {
        playlist_mbid: String,
    },
    DescriptionRejected {
        playlist_mbid: String,
        status: StatusCode,
    },
}

impl PlaylistBackupError {
    pub(crate) fn partial_playlist_mbid(&self) -> Option<&str> {
        match self {
            Self::DescriptionRequest { playlist_mbid }
            | Self::DescriptionRejected { playlist_mbid, .. } => Some(playlist_mbid),
            Self::CreateRequest
            | Self::CreateRejected(_)
            | Self::CreateResponse
            | Self::InvalidPlaylistMbid => None,
        }
    }
}

impl std::fmt::Display for PlaylistBackupError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::CreateRequest => formatter.write_str(
                "send ListenBrainz playlist create request; no local link was recorded",
            ),
            Self::CreateRejected(status) => write!(
                formatter,
                "ListenBrainz backup create failed ({status}); no local link was recorded"
            ),
            Self::CreateResponse => formatter.write_str(
                "parse ListenBrainz playlist create response; no local link was recorded",
            ),
            Self::InvalidPlaylistMbid => formatter.write_str(
                "ListenBrainz response did not include a valid playlist MBID; no local link was recorded",
            ),
            Self::DescriptionRequest { playlist_mbid } => write!(
                formatter,
                "ListenBrainz playlist {playlist_mbid} was created, but its identity description could not be saved; delete or repair that remote playlist before retrying; no local link was recorded"
            ),
            Self::DescriptionRejected {
                playlist_mbid,
                status,
            } => write!(
                formatter,
                "ListenBrainz playlist {playlist_mbid} was created, but its identity description could not be saved ({status}); delete or repair that remote playlist before retrying; no local link was recorded"
            ),
        }
    }
}

impl std::error::Error for PlaylistBackupError {}

pub(crate) async fn create_description_backup(
    http: &reqwest::Client,
    token: &str,
    playlist_name: &str,
    description: &str,
) -> std::result::Result<String, PlaylistBackupError> {
    let create_response = http
        .post(format!("{LISTENBRAINZ_API_ROOT}/playlist/create"))
        .header(header::AUTHORIZATION, format!("Token {token}"))
        .json(&playlist_backup_create_payload(playlist_name))
        .send()
        .await
        .map_err(|_| PlaylistBackupError::CreateRequest)?;
    let create_status = create_response.status();
    if !create_status.is_success() {
        return Err(PlaylistBackupError::CreateRejected(create_status));
    }
    let create_body = create_response
        .json::<serde_json::Value>()
        .await
        .map_err(|_| PlaylistBackupError::CreateResponse)?;
    let playlist_mbid = parse_created_playlist_mbid(&create_body)?;

    let edit_response = http
        .post(format!(
            "{LISTENBRAINZ_API_ROOT}/playlist/edit/{playlist_mbid}"
        ))
        .header(header::AUTHORIZATION, format!("Token {token}"))
        .json(&playlist_backup_description_payload(description))
        .send()
        .await
        .map_err(|_| PlaylistBackupError::DescriptionRequest {
            playlist_mbid: playlist_mbid.clone(),
        })?;
    let status = edit_response.status();
    if !status.is_success() {
        return Err(PlaylistBackupError::DescriptionRejected {
            playlist_mbid,
            status,
        });
    }
    Ok(playlist_mbid)
}

pub(crate) fn playlist_backup_create_payload(playlist_name: &str) -> serde_json::Value {
    serde_json::json!({
        "playlist": {
            "title": playlist_name,
            "extension": {
                "https://musicbrainz.org/doc/jspf#playlist": {
                    "public": false
                }
            }
        }
    })
}

pub(crate) fn playlist_backup_description_payload(description: &str) -> serde_json::Value {
    serde_json::json!({
        "playlist": {
            "annotation": description
        }
    })
}

fn parse_created_playlist_mbid(
    body: &serde_json::Value,
) -> std::result::Result<String, PlaylistBackupError> {
    let playlist_mbid = body
        .get("playlist_mbid")
        .and_then(serde_json::Value::as_str)
        .or_else(|| {
            body.get("playlist")
                .and_then(|playlist| playlist.get("mbid"))
                .and_then(serde_json::Value::as_str)
        })
        .ok_or(PlaylistBackupError::InvalidPlaylistMbid)?;
    if !is_playlist_mbid(playlist_mbid) {
        return Err(PlaylistBackupError::InvalidPlaylistMbid);
    }
    Ok(playlist_mbid.to_owned())
}

fn is_playlist_mbid(value: &str) -> bool {
    value.len() == 36
        && value.bytes().enumerate().all(|(index, byte)| {
            if matches!(index, 8 | 13 | 18 | 23) {
                byte == b'-'
            } else {
                byte.is_ascii_hexdigit()
            }
        })
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct MusicBrainzArtist {
    pub(crate) id: String,
    pub(crate) name: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct PopularRecording {
    pub(crate) recording_mbid: String,
    pub(crate) recording_name: String,
    pub(crate) total_listen_count: Option<u64>,
    pub(crate) total_user_count: Option<u64>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct PopularReleaseGroup {
    pub(crate) release_group_mbid: String,
    pub(crate) release_name: String,
    pub(crate) release_date: Option<String>,
    pub(crate) release_type: Option<String>,
    pub(crate) total_listen_count: Option<u64>,
    pub(crate) total_user_count: Option<u64>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct SpotifyAlbumRelation {
    pub(crate) release_mbid: String,
    pub(crate) spotify_album_id: String,
}

#[derive(Debug, Deserialize)]
struct MusicBrainzUrlResponse {
    #[serde(default)]
    relations: Vec<MusicBrainzRelation>,
}

#[derive(Debug, Deserialize)]
struct MusicBrainzRelation {
    artist: Option<MusicBrainzArtistResponse>,
    url: Option<MusicBrainzUrlTarget>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct MusicBrainzRecordingMetadata {
    pub(crate) title: String,
    pub(crate) artists: String,
    pub(crate) duration_ms: Option<u64>,
    pub(crate) spotify_track_id: Option<String>,
}

#[derive(Debug, Deserialize)]
struct MusicBrainzRecordingResponse {
    #[serde(default)]
    title: String,
    #[serde(default)]
    length: Option<u64>,
    #[serde(rename = "artist-credit", default)]
    artist_credit: Vec<MusicBrainzArtistCredit>,
    #[serde(default)]
    relations: Vec<MusicBrainzRelation>,
}

#[derive(Debug, Deserialize)]
struct MusicBrainzArtistCredit {
    artist: MusicBrainzArtistResponse,
    #[serde(default)]
    joinphrase: String,
}

#[derive(Debug, Deserialize)]
struct MusicBrainzArtistResponse {
    id: String,
    name: String,
}

#[derive(Debug, Deserialize)]
struct MusicBrainzUrlTarget {
    resource: String,
}

#[derive(Debug, Deserialize)]
struct MusicBrainzReleaseBrowseResponse {
    #[serde(default)]
    releases: Vec<MusicBrainzReleaseResponse>,
}

#[derive(Debug, Deserialize)]
struct MusicBrainzReleaseResponse {
    id: String,
    #[serde(default)]
    relations: Vec<MusicBrainzRelation>,
}

#[derive(Debug, Deserialize)]
struct ListenBrainzTopRecordingResponse {
    recording_mbid: String,
    recording_name: String,
    total_listen_count: Option<u64>,
    total_user_count: Option<u64>,
}

#[derive(Debug, Deserialize)]
struct ListenBrainzTopReleaseGroupResponse {
    release_group_mbid: String,
    release_group: ListenBrainzReleaseGroupMetadata,
    total_listen_count: Option<u64>,
    total_user_count: Option<u64>,
}

#[derive(Debug, Deserialize)]
struct ListenBrainzReleaseGroupMetadata {
    name: String,
    date: Option<String>,
    #[serde(rename = "type")]
    release_type: Option<String>,
}

pub(crate) async fn artist_from_spotify_id(
    http: &reqwest::Client,
    spotify_artist_id: &str,
) -> Result<Option<MusicBrainzArtist>> {
    let spotify_url = format!("https://open.spotify.com/artist/{spotify_artist_id}");
    let response = http
        .get(format!("{MUSICBRAINZ_API_ROOT}/url"))
        .header(header::USER_AGENT, MUSICBRAINZ_USER_AGENT)
        .query(&[
            ("resource", spotify_url.as_str()),
            ("inc", "artist-rels"),
            ("fmt", "json"),
        ])
        .send()
        .await?;
    if response.status() == StatusCode::NOT_FOUND {
        return Ok(None);
    }
    let response = response
        .error_for_status()?
        .json::<MusicBrainzUrlResponse>()
        .await?;
    Ok(response
        .relations
        .into_iter()
        .find_map(|relation| relation.artist)
        .map(|artist| MusicBrainzArtist {
            id: artist.id,
            name: artist.name,
        }))
}

pub(crate) async fn top_recordings_for_artist(
    http: &reqwest::Client,
    artist_mbid: &str,
    token: Option<&str>,
) -> Result<Vec<PopularRecording>> {
    let mut request = http.get(format!(
        "{LISTENBRAINZ_API_ROOT}/popularity/top-recordings-for-artist/{artist_mbid}"
    ));
    if let Some(token) = token.filter(|token| !token.trim().is_empty()) {
        request = request.header(header::AUTHORIZATION, format!("Token {}", token.trim()));
    }
    Ok(request
        .send()
        .await?
        .error_for_status()?
        .json::<Vec<ListenBrainzTopRecordingResponse>>()
        .await?
        .into_iter()
        .map(|recording| PopularRecording {
            recording_mbid: recording.recording_mbid,
            recording_name: recording.recording_name,
            total_listen_count: recording.total_listen_count,
            total_user_count: recording.total_user_count,
        })
        .collect())
}

pub(crate) async fn top_release_groups_for_artist(
    http: &reqwest::Client,
    artist_mbid: &str,
    token: Option<&str>,
) -> Result<Vec<PopularReleaseGroup>> {
    let mut request = http.get(format!(
        "{LISTENBRAINZ_API_ROOT}/popularity/top-release-groups-for-artist/{artist_mbid}"
    ));
    if let Some(token) = token.filter(|token| !token.trim().is_empty()) {
        request = request.header(header::AUTHORIZATION, format!("Token {}", token.trim()));
    }
    Ok(request
        .send()
        .await?
        .error_for_status()?
        .json::<Vec<ListenBrainzTopReleaseGroupResponse>>()
        .await?
        .into_iter()
        .map(|release| PopularReleaseGroup {
            release_group_mbid: release.release_group_mbid,
            release_name: release.release_group.name,
            release_date: release.release_group.date,
            release_type: release.release_group.release_type,
            total_listen_count: release.total_listen_count,
            total_user_count: release.total_user_count,
        })
        .collect())
}

fn spotify_track_id_from_resource(resource: &str) -> Option<String> {
    if let Some(id) = resource.strip_prefix("spotify:track:") {
        return (!id.is_empty()).then(|| id.to_owned());
    }
    let url = reqwest::Url::parse(resource).ok()?;
    if url.scheme() != "https" || url.host_str() != Some("open.spotify.com") {
        return None;
    }
    let mut segments = url.path_segments()?;
    if segments.next()? != "track" {
        return None;
    }
    let id = segments.next()?;
    (segments.next().is_none() && !id.is_empty()).then(|| id.to_owned())
}

fn spotify_album_id_from_resource(resource: &str) -> Option<String> {
    if let Some(id) = resource.strip_prefix("spotify:album:") {
        return (!id.is_empty()).then(|| id.to_owned());
    }
    let url = reqwest::Url::parse(resource).ok()?;
    if url.scheme() != "https" || url.host_str() != Some("open.spotify.com") {
        return None;
    }
    let mut segments = url.path_segments()?;
    if segments.next()? != "album" {
        return None;
    }
    let id = segments.next()?;
    (segments.next().is_none() && !id.is_empty()).then(|| id.to_owned())
}

pub(crate) async fn spotify_album_relations_for_release_group(
    http: &reqwest::Client,
    release_group_mbid: &str,
) -> Result<Vec<SpotifyAlbumRelation>> {
    let response = http
        .get(format!("{MUSICBRAINZ_API_ROOT}/release"))
        .header(header::USER_AGENT, MUSICBRAINZ_USER_AGENT)
        .query(&[
            ("release-group", release_group_mbid),
            ("inc", "url-rels"),
            ("fmt", "json"),
            ("limit", "100"),
        ])
        .send()
        .await?
        .error_for_status()?
        .json::<MusicBrainzReleaseBrowseResponse>()
        .await?;
    let mut seen = HashSet::new();
    Ok(response
        .releases
        .into_iter()
        .flat_map(|release| {
            release.relations.into_iter().filter_map(move |relation| {
                relation.url.and_then(|url| {
                    spotify_album_id_from_resource(&url.resource).map(|spotify_album_id| {
                        SpotifyAlbumRelation {
                            release_mbid: release.id.clone(),
                            spotify_album_id,
                        }
                    })
                })
            })
        })
        .filter(|relation| {
            seen.insert((
                relation.release_mbid.clone(),
                relation.spotify_album_id.clone(),
            ))
        })
        .collect())
}

pub(crate) async fn spotify_track_id_for_recording(
    http: &reqwest::Client,
    recording_mbid: &str,
) -> Result<Option<String>> {
    let mut url = reqwest::Url::parse(MUSICBRAINZ_API_ROOT)?;
    url.path_segments_mut()
        .map_err(|()| anyhow::anyhow!("MusicBrainz URL cannot accept path segments"))?
        .push("recording")
        .push(recording_mbid);
    let response = http
        .get(url)
        .header(header::USER_AGENT, MUSICBRAINZ_USER_AGENT)
        .query(&[("inc", "url-rels"), ("fmt", "json")])
        .send()
        .await?;
    if response.status() == StatusCode::NOT_FOUND {
        return Ok(None);
    }
    let response = response
        .error_for_status()?
        .json::<MusicBrainzUrlResponse>()
        .await?;
    Ok(response.relations.into_iter().find_map(|relation| {
        relation
            .url
            .and_then(|url| spotify_track_id_from_resource(&url.resource))
    }))
}

async fn recording_metadata(
    http: &reqwest::Client,
    recording_mbid: &str,
) -> Result<Option<MusicBrainzRecordingMetadata>> {
    let mut url = reqwest::Url::parse(MUSICBRAINZ_API_ROOT)?;
    url.path_segments_mut()
        .map_err(|()| anyhow::anyhow!("MusicBrainz URL cannot accept path segments"))?
        .push("recording")
        .push(recording_mbid);
    let response = http
        .get(url)
        .header(header::USER_AGENT, MUSICBRAINZ_USER_AGENT)
        .query(&[("inc", "artist-credits+url-rels"), ("fmt", "json")])
        .send()
        .await?;
    if response.status() == StatusCode::NOT_FOUND {
        return Ok(None);
    }
    let response = response
        .error_for_status()?
        .json::<MusicBrainzRecordingResponse>()
        .await?;
    let artists = response
        .artist_credit
        .iter()
        .fold(String::new(), |mut artists, credit| {
            artists.push_str(&credit.artist.name);
            artists.push_str(&credit.joinphrase);
            artists
        });
    Ok(Some(MusicBrainzRecordingMetadata {
        title: response.title,
        artists,
        duration_ms: response.length,
        spotify_track_id: response.relations.into_iter().find_map(|relation| {
            relation
                .url
                .and_then(|url| spotify_track_id_from_resource(&url.resource))
        }),
    }))
}

pub(crate) fn apply_recording_metadata(
    item: &mut crate::state::UnifiedPlaylistItem,
    metadata: &MusicBrainzRecordingMetadata,
    observed_at: u64,
) {
    if (item.title.trim().is_empty() || item.title == "Unknown track")
        && !metadata.title.trim().is_empty()
    {
        item.title.clone_from(&metadata.title);
    }
    if item.artists.trim().is_empty() && !metadata.artists.trim().is_empty() {
        item.artists.clone_from(&metadata.artists);
    }
    if item.duration_ms.is_none() {
        item.duration_ms = metadata.duration_ms;
    }
    if let Some(spotify_track_id) = metadata.spotify_track_id.as_deref() {
        item.media_id = crate::state::MediaId {
            provider: crate::state::Provider::Spotify,
            kind: crate::state::MediaKind::Track,
            raw_id: spotify_track_id.to_owned(),
        };
        item.provider_url = Some(format!("spotify:track:{spotify_track_id}"));
    }
    item.metadata.provenance = Some(if metadata.spotify_track_id.is_some() {
        "listenbrainz-musicbrainz-spotify".to_owned()
    } else {
        "listenbrainz-musicbrainz".to_owned()
    });
    item.metadata.observed_at = Some(observed_at);
    item.metadata.degraded = metadata.spotify_track_id.is_none();
    item.metadata.metadata_pending = item.title.trim().is_empty()
        || item.title == "Unknown track"
        || item.artists.trim().is_empty()
        || item.duration_ms.is_none();
}

pub(crate) async fn hydrate_recording_metadata(
    http: &reqwest::Client,
    items: &mut [crate::state::UnifiedPlaylistItem],
) -> usize {
    let observed_at = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |duration| duration.as_secs());
    let mut cache = HashMap::<String, Option<MusicBrainzRecordingMetadata>>::new();
    for item in items.iter_mut().filter(|item| {
        item.metadata.degraded
            && item.metadata.provenance.as_deref() == Some("unresolved-listenbrainz")
    }) {
        let Some(identifier) = item
            .metadata
            .source_identifier
            .as_deref()
            .or(item.provider_url.as_deref())
        else {
            continue;
        };
        let Some(recording_mbid) =
            super::listenbrainz_import::listenbrainz_recording_mbid(identifier)
        else {
            continue;
        };
        let metadata = if let Some(metadata) = cache.get(&recording_mbid) {
            metadata.clone()
        } else {
            let metadata = recording_metadata(http, &recording_mbid)
                .await
                .ok()
                .flatten();
            cache.insert(recording_mbid, metadata.clone());
            metadata
        };
        if let Some(metadata) = metadata {
            apply_recording_metadata(item, &metadata, observed_at);
        }
    }
    items
        .iter()
        .filter(|item| item.metadata.degraded || item.metadata.metadata_pending)
        .count()
}

fn unique_popular_recordings(recordings: Vec<PopularRecording>) -> Vec<PopularRecording> {
    let mut seen = HashSet::new();
    recordings
        .into_iter()
        .filter(|recording| seen.insert(recording.recording_mbid.clone()))
        .take(ARTIST_RECORDING_LIMIT)
        .collect()
}

fn unique_popular_release_groups(
    release_groups: Vec<PopularReleaseGroup>,
) -> Vec<PopularReleaseGroup> {
    let mut seen = HashSet::new();
    release_groups
        .into_iter()
        .filter(|release| seen.insert(release.release_group_mbid.clone()))
        .take(ARTIST_RECORDING_LIMIT)
        .collect()
}

pub(crate) fn artist_enrichment_from_sources(
    artist: MusicBrainzArtist,
    recordings_status: ListenBrainzCollectionStatus,
    recordings: Vec<PopularRecording>,
    release_groups_status: ListenBrainzCollectionStatus,
    release_groups: Vec<PopularReleaseGroup>,
) -> ListenBrainzArtistEnrichment {
    ListenBrainzArtistEnrichment::Available {
        artist_mbid: artist.id,
        artist_name: artist.name,
        recordings_status,
        recordings: unique_popular_recordings(recordings)
            .into_iter()
            .map(|recording| ListenBrainzPopularRecording {
                recording_mbid: recording.recording_mbid,
                name: recording.recording_name,
                total_listen_count: recording.total_listen_count,
                total_user_count: recording.total_user_count,
            })
            .collect(),
        release_groups_status,
        release_groups: unique_popular_release_groups(release_groups)
            .into_iter()
            .map(|release| ListenBrainzReleaseGroup {
                release_group_mbid: release.release_group_mbid,
                name: release.release_name,
                release_date: release.release_date,
                release_type: release.release_type,
                total_listen_count: release.total_listen_count,
                total_user_count: release.total_user_count,
            })
            .collect(),
    }
}

#[cfg(test)]
mod tests {
    use super::{
        artist_enrichment_from_sources, spotify_album_id_from_resource,
        spotify_track_id_from_resource, ListenBrainzTopRecordingResponse,
        ListenBrainzTopReleaseGroupResponse, MusicBrainzArtist, MusicBrainzReleaseBrowseResponse,
        MusicBrainzUrlResponse, PlaylistBackupError, PopularRecording, PopularReleaseGroup,
    };
    use crate::state::{ListenBrainzArtistEnrichment, ListenBrainzCollectionStatus};

    #[test]
    fn public_artist_responses_parse_without_credentials() {
        let musicbrainz: MusicBrainzUrlResponse = serde_json::from_str(
            r#"{"relations":[{"artist":{"id":"artist-mbid","name":"Artist"}}]}"#,
        )
        .unwrap();
        let listenbrainz: Vec<ListenBrainzTopRecordingResponse> = serde_json::from_str(
            r#"[{"recording_mbid":"recording-mbid","recording_name":"Track","total_listen_count":12,"total_user_count":3}]"#,
        )
        .unwrap();

        assert_eq!(
            musicbrainz.relations[0].artist.as_ref().unwrap().id,
            "artist-mbid"
        );
        assert_eq!(listenbrainz[0].recording_name, "Track");
        assert_eq!(listenbrainz[0].total_listen_count, Some(12));
    }

    #[test]
    fn playlist_backup_payloads_create_private_shell_then_set_annotation() {
        let create = super::playlist_backup_create_payload("Portable");
        assert_eq!(create["playlist"]["title"], "Portable");
        assert_eq!(
            create["playlist"]["extension"]["https://musicbrainz.org/doc/jspf#playlist"]["public"],
            false
        );
        assert!(create["playlist"].get("track").is_none());

        assert_eq!(
            super::playlist_backup_description_payload("manifest"),
            serde_json::json!({"playlist": {"annotation": "manifest"}})
        );
    }

    #[test]
    fn playlist_backup_edit_failure_retains_safe_partial_identity() {
        let playlist_mbid = "9ef9f54c-1d4b-4ac4-8b6e-a127c77021f1";
        let error = PlaylistBackupError::DescriptionRejected {
            playlist_mbid: playlist_mbid.to_owned(),
            status: reqwest::StatusCode::BAD_GATEWAY,
        };

        assert_eq!(error.partial_playlist_mbid(), Some(playlist_mbid));
        let display = error.to_string();
        assert!(display.contains(playlist_mbid));
        assert!(display.contains("502 Bad Gateway"));
        assert!(display.contains("no local link was recorded"));
        assert!(!display.contains("Token"));
    }

    #[test]
    fn top_release_groups_keep_musicbrainz_identity_and_album_metadata() {
        let response: Vec<ListenBrainzTopReleaseGroupResponse> = serde_json::from_str(
            r#"[{"release_group_mbid":"release-group-mbid","release_group":{"date":"1994-03-08","name":"The Album","rels":[],"type":"Album"},"total_listen_count":42,"total_user_count":7}]"#,
        )
        .unwrap();
        let release = &response[0];

        assert_eq!(release.release_group_mbid, "release-group-mbid");
        assert_eq!(release.release_group.name, "The Album");
        assert_eq!(release.release_group.date.as_deref(), Some("1994-03-08"));
        assert_eq!(release.release_group.release_type.as_deref(), Some("Album"));
        assert_eq!(release.total_listen_count, Some(42));
        assert_eq!(release.total_user_count, Some(7));
    }

    #[test]
    fn artist_enrichment_preserves_independent_collection_status_and_release_identity() {
        let release = PopularReleaseGroup {
            release_group_mbid: "release-group-mbid".to_owned(),
            release_name: "The Album".to_owned(),
            release_date: Some("1994-03-08".to_owned()),
            release_type: Some("Album".to_owned()),
            total_listen_count: Some(42),
            total_user_count: Some(7),
        };
        let enrichment = artist_enrichment_from_sources(
            MusicBrainzArtist {
                id: "artist-mbid".to_owned(),
                name: "Artist".to_owned(),
            },
            ListenBrainzCollectionStatus::Unavailable,
            Vec::new(),
            ListenBrainzCollectionStatus::Available,
            vec![release.clone(), release],
        );

        let ListenBrainzArtistEnrichment::Available {
            recordings_status,
            recordings,
            release_groups_status,
            release_groups,
            ..
        } = enrichment
        else {
            panic!("mapped artist should retain partial ListenBrainz results")
        };
        assert_eq!(recordings_status, ListenBrainzCollectionStatus::Unavailable);
        assert!(recordings.is_empty());
        assert_eq!(
            release_groups_status,
            ListenBrainzCollectionStatus::Available
        );
        assert_eq!(release_groups.len(), 1);
        assert_eq!(release_groups[0].release_group_mbid, "release-group-mbid");
    }

    #[test]
    fn recording_relations_accept_only_direct_spotify_track_targets() {
        assert_eq!(
            spotify_track_id_from_resource("https://open.spotify.com/track/track-id?si=ignored")
                .as_deref(),
            Some("track-id")
        );
        assert_eq!(
            spotify_track_id_from_resource("spotify:track:track-id").as_deref(),
            Some("track-id")
        );
        assert_eq!(
            spotify_track_id_from_resource("https://open.spotify.com/album/album-id"),
            None
        );
        assert_eq!(
            spotify_track_id_from_resource("https://example.com/track/track-id"),
            None
        );
    }

    #[test]
    fn recording_relation_response_parses_spotify_url_without_tokens() {
        let response: MusicBrainzUrlResponse = serde_json::from_str(
            r#"{"relations":[{"url":{"resource":"https://open.spotify.com/track/track-id"}}]}"#,
        )
        .unwrap();

        let id = response.relations.into_iter().find_map(|relation| {
            relation
                .url
                .and_then(|url| spotify_track_id_from_resource(&url.resource))
        });
        assert_eq!(id.as_deref(), Some("track-id"));
    }

    #[test]
    fn album_relations_accept_only_direct_spotify_album_targets() {
        assert_eq!(
            spotify_album_id_from_resource("https://open.spotify.com/album/album-id?si=ignored")
                .as_deref(),
            Some("album-id")
        );
        assert_eq!(
            spotify_album_id_from_resource("spotify:album:album-id").as_deref(),
            Some("album-id")
        );
        assert_eq!(
            spotify_album_id_from_resource("https://open.spotify.com/track/track-id"),
            None
        );
        assert_eq!(
            spotify_album_id_from_resource("https://example.com/album/album-id"),
            None
        );
    }

    #[test]
    fn release_browse_response_keeps_release_scoped_album_relations() {
        let response: MusicBrainzReleaseBrowseResponse = serde_json::from_str(
            r#"{"releases":[{"id":"release-mbid","relations":[{"url":{"resource":"https://open.spotify.com/album/album-id"}}]}]}"#,
        )
        .unwrap();
        let relation = &response.releases[0].relations[0];

        assert_eq!(response.releases[0].id, "release-mbid");
        assert_eq!(
            spotify_album_id_from_resource(&relation.url.as_ref().unwrap().resource).as_deref(),
            Some("album-id")
        );
    }

    #[test]
    fn artist_fallback_keeps_one_row_instance_per_recording_mbid() {
        let recordings = super::unique_popular_recordings(vec![
            PopularRecording {
                recording_mbid: "same".to_owned(),
                recording_name: "First".to_owned(),
                total_listen_count: Some(3),
                total_user_count: Some(2),
            },
            PopularRecording {
                recording_mbid: "same".to_owned(),
                recording_name: "Duplicate".to_owned(),
                total_listen_count: Some(2),
                total_user_count: Some(1),
            },
        ]);

        assert_eq!(recordings.len(), 1);
        assert_eq!(recordings[0].recording_name, "First");
    }
}

/// A credential may travel through Debug-derived request envelopes safely.
#[derive(Clone, PartialEq, Eq)]
pub struct ListenBrainzToken(String);
impl std::fmt::Debug for ListenBrainzToken {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("ListenBrainzToken([REDACTED])")
    }
}
impl ListenBrainzToken {
    pub(crate) fn new(value: String) -> Self {
        Self(value.trim().to_owned())
    }
    pub(crate) fn expose(&self) -> &str {
        &self.0
    }
}
#[derive(Deserialize)]
struct TokenValidation {
    valid: bool,
    user_name: Option<String>,
}
fn validated_username(value: TokenValidation) -> Result<String> {
    anyhow::ensure!(value.valid, "ListenBrainz token is invalid.");
    value
        .user_name
        .filter(|name| !name.trim().is_empty())
        .ok_or_else(|| anyhow::anyhow!("ListenBrainz returned an incomplete token check."))
}
pub(crate) async fn validate_token(token: &ListenBrainzToken) -> Result<String> {
    anyhow::ensure!(
        !token.expose().is_empty(),
        "Enter a ListenBrainz user token."
    );
    let http = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(20))
        .build()
        .map_err(|_| anyhow::anyhow!("Could not prepare ListenBrainz token check."))?;
    let response = http
        .get(format!("{LISTENBRAINZ_API_ROOT}/validate-token"))
        .header(header::USER_AGENT, MUSICBRAINZ_USER_AGENT)
        .header(header::AUTHORIZATION, format!("Token {}", token.expose()))
        .send()
        .await
        .map_err(|_| anyhow::anyhow!("ListenBrainz token check could not connect. Retry."))?;
    anyhow::ensure!(
        response.status().is_success(),
        "ListenBrainz token check was rejected. Retry later."
    );
    let value = response
        .json::<TokenValidation>()
        .await
        .map_err(|_| anyhow::anyhow!("ListenBrainz returned an invalid token check."))?;
    validated_username(value)
}

#[cfg(test)]
mod token_tests {
    use super::*;
    #[test]
    fn listenbrainz_token_response_requires_valid_and_username() {
        for body in [
            r#"{"valid":false,"user_name":"name"}"#,
            r#"{"valid":true}"#,
            r#"{"valid":true,"user_name":" "}"#,
        ] {
            assert!(validated_username(serde_json::from_str(body).unwrap()).is_err());
        }
        assert!(serde_json::from_str::<TokenValidation>(r#"{"message":"secret"}"#).is_err());
        assert_eq!(
            validated_username(
                serde_json::from_str(r#"{"valid":true,"user_name":"listener"}"#).unwrap()
            )
            .unwrap(),
            "listener"
        );
    }
    #[test]
    fn listenbrainz_token_request_debug_redacts_credential() {
        let request = crate::client::ClientRequest::ValidateListenBrainzToken {
            attempt: 1,
            token: ListenBrainzToken::new("example-sensitive-token".to_owned()),
            save: true,
        };
        let debug = format!("{request:?}");
        assert!(!debug.contains("example-sensitive-token"));
        assert!(debug.contains("REDACTED"));
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ValidatedListenBrainzIdentity {
    pub(crate) username: String,
    pub(crate) token: ListenBrainzToken,
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ListenBrainzPlaylistSummary {
    pub(crate) id: String,
    pub(crate) title: String,
    pub(crate) item_count: Option<usize>,
    pub(crate) imported: bool,
}
#[derive(Deserialize)]
struct PlaylistMetadataEnvelope {
    playlist: PlaylistMetadata,
}
#[derive(Deserialize)]
struct PlaylistMetadata {
    identifier: String,
    title: String,
    annotation: Option<String>,
}
#[derive(Deserialize)]
struct PlaylistPage {
    playlists: Vec<PlaylistMetadataEnvelope>,
    playlist_count: usize,
    offset: usize,
}
fn playlist_id_from_identifier(identifier: &str) -> Result<String> {
    let id = identifier
        .strip_prefix("https://listenbrainz.org/playlist/")
        .or_else(|| identifier.strip_prefix("http://listenbrainz.org/playlist/"))
        .ok_or_else(|| anyhow::anyhow!("ListenBrainz returned an invalid playlist identifier."))?;
    anyhow::ensure!(
        is_playlist_mbid(id),
        "ListenBrainz returned an invalid playlist identifier."
    );
    Ok(id.to_ascii_lowercase())
}
fn playlist_list_url(root: &str, username: &str, offset: usize) -> Result<reqwest::Url> {
    let mut url = reqwest::Url::parse(root)?;
    url.path_segments_mut()
        .map_err(|()| anyhow::anyhow!("Invalid ListenBrainz API URL."))?
        .pop_if_empty()
        .extend(["user", username, "playlists"]);
    url.query_pairs_mut()
        .append_pair("count", "100")
        .append_pair("offset", &offset.to_string());
    Ok(url)
}
fn append_playlist_page(
    page: PlaylistPage,
    offset: usize,
    rows: &mut Vec<ListenBrainzPlaylistSummary>,
    seen: &mut HashSet<String>,
) -> Result<Option<usize>> {
    anyhow::ensure!(
        page.offset == offset && page.playlists.len() <= 100,
        "ListenBrainz returned invalid playlist pagination."
    );
    let next = offset + page.playlists.len();
    anyhow::ensure!(
        next >= page.playlist_count || next > offset,
        "ListenBrainz returned incomplete playlist pagination."
    );
    for envelope in page.playlists {
        let id = playlist_id_from_identifier(&envelope.playlist.identifier)?;
        if !seen.insert(id.clone()) {
            continue;
        }
        let item_count = envelope
            .playlist
            .annotation
            .as_deref()
            .and_then(|text| {
                crate::cli::listenbrainz_manifest::parse_description_manifest(text).ok()
            })
            .map(|manifest| manifest.entries.len());
        rows.push(ListenBrainzPlaylistSummary {
            id,
            title: envelope.playlist.title,
            item_count,
            imported: false,
        });
    }
    Ok((next < page.playlist_count).then_some(next))
}
fn playlist_read_client() -> Result<reqwest::Client> {
    reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(20))
        .user_agent(MUSICBRAINZ_USER_AGENT)
        .build()
        .map_err(|_| anyhow::anyhow!("Could not prepare ListenBrainz playlist read."))
}
pub(crate) async fn user_playlists(
    identity: &ValidatedListenBrainzIdentity,
) -> Result<Vec<ListenBrainzPlaylistSummary>> {
    let http = playlist_read_client()?;
    let mut offset = 0;
    let mut rows = Vec::new();
    let mut seen = HashSet::new();
    for _ in 0..20 {
        let url = playlist_list_url(LISTENBRAINZ_API_ROOT, &identity.username, offset)?;
        let response = http
            .get(url)
            .header(
                header::AUTHORIZATION,
                format!("Token {}", identity.token.expose()),
            )
            .send()
            .await
            .map_err(|_| anyhow::anyhow!("ListenBrainz playlist list could not connect. Retry."))?;
        anyhow::ensure!(
            response.status().is_success(),
            "ListenBrainz playlist list was rejected. Check the token and retry."
        );
        let page = response
            .json::<PlaylistPage>()
            .await
            .map_err(|_| anyhow::anyhow!("ListenBrainz returned an invalid playlist list."))?;
        match append_playlist_page(page, offset, &mut rows, &mut seen)? {
            Some(next) => offset = next,
            None => return Ok(rows),
        }
    }
    anyhow::bail!("ListenBrainz playlist list exceeds the 2000-entry read limit.")
}
pub(crate) async fn read_playlist(
    identity: &ValidatedListenBrainzIdentity,
    playlist_id: &str,
) -> Result<(String, Vec<crate::state::UnifiedPlaylistItem>, usize)> {
    anyhow::ensure!(
        is_playlist_mbid(playlist_id),
        "Invalid ListenBrainz playlist identifier."
    );
    let value = super::listenbrainz_push::ListenBrainzMutationAdapter::new(playlist_read_client()?)
        .fetch(identity.token.expose(), playlist_id)
        .await
        .map_err(|_| anyhow::anyhow!("ListenBrainz playlist could not be read. Retry."))?;
    let returned_id = value
        .get("playlist")
        .and_then(|p| p.get("identifier"))
        .and_then(serde_json::Value::as_str)
        .ok_or_else(|| anyhow::anyhow!("ListenBrainz playlist identity is missing."))?;
    anyhow::ensure!(
        playlist_id_from_identifier(returned_id)? == playlist_id,
        "ListenBrainz returned a different playlist."
    );
    let (name, mut items, _) = super::listenbrainz_import::parse_live_playlist(&value)
        .map_err(|_| anyhow::anyhow!("ListenBrainz playlist contents could not be imported."))?;
    let unresolved = hydrate_recording_metadata(&playlist_read_client()?, &mut items).await;
    Ok((name, items, unresolved))
}

#[cfg(test)]
mod catalog_tests {
    use super::*;
    use serde_json::json;
    fn page(ids: &[&str], total: usize, offset: usize) -> PlaylistPage {
        serde_json::from_value(json!({"playlists":ids.iter().map(|id| json!({"playlist":{"identifier":format!("https://listenbrainz.org/playlist/{id}"),"title":"Native","track":[]}})).collect::<Vec<_>>(),"playlist_count":total,"offset":offset,"count":100})).unwrap()
    }
    #[test]
    fn listenbrainz_catalog_pagination_deduplicates_and_keeps_native_counts_unknown() {
        let a = "12345678-1234-1234-1234-123456789abc";
        let b = "12345678-1234-1234-1234-123456789abd";
        let mut rows = Vec::new();
        let mut seen = HashSet::new();
        assert_eq!(
            append_playlist_page(page(&[a, a], 3, 0), 0, &mut rows, &mut seen).unwrap(),
            Some(2)
        );
        assert_eq!(
            append_playlist_page(page(&[b], 3, 2), 2, &mut rows, &mut seen).unwrap(),
            None
        );
        assert_eq!(rows.len(), 2);
        assert!(rows.iter().all(|r| r.item_count.is_none()));
        assert!(append_playlist_page(page(&[], 2, 0), 0, &mut rows, &mut seen).is_err());
        assert!(append_playlist_page(page(&[], 0, 1), 0, &mut rows, &mut seen).is_err());
        assert!(append_playlist_page(page(&["invalid"], 1, 0), 0, &mut rows, &mut seen).is_err());
        assert_eq!(
            append_playlist_page(page(&[], 0, 0), 0, &mut rows, &mut seen).unwrap(),
            None
        );
    }
    #[test]
    fn listenbrainz_catalog_url_encodes_username_and_rejects_malformed_envelope() {
        let url =
            playlist_list_url(LISTENBRAINZ_API_ROOT, "name/with?query#fragment", 100).unwrap();
        assert_eq!(url.host_str(), Some("api.listenbrainz.org"));
        assert!(url.path().contains("name%2Fwith%3Fquery%23fragment"));
        assert_eq!(url.query(), Some("count=100&offset=100"));
        assert!(url.fragment().is_none());
        assert!(
            serde_json::from_value::<PlaylistPage>(json!({"payload":{"playlists":[]}})).is_err()
        );
        assert!(serde_json::from_value::<PlaylistPage>(
            json!({"playlists":[{}],"playlist_count":1,"offset":0})
        )
        .is_err());
    }
}

#[cfg(test)]
mod recording_metadata_tests {
    use super::{apply_recording_metadata, MusicBrainzRecordingMetadata};
    use crate::state::{MediaId, MediaKind, Provider, UnifiedPlaylistItem};

    fn unresolved_item() -> UnifiedPlaylistItem {
        UnifiedPlaylistItem {
            media_id: MediaId {
                provider: Provider::Spotify,
                kind: MediaKind::Track,
                raw_id: "https://musicbrainz.org/recording/12345678-1234-1234-1234-123456789abc"
                    .to_owned(),
            },
            title: "Unknown track".to_owned(),
            provider_url: Some(
                "https://musicbrainz.org/recording/12345678-1234-1234-1234-123456789abc".to_owned(),
            ),
            metadata: crate::state::UnifiedPlaylistMetadata {
                provenance: Some("unresolved-listenbrainz".to_owned()),
                degraded: true,
                ..Default::default()
            },
            ..Default::default()
        }
    }

    #[test]
    fn recording_metadata_promotes_a_direct_spotify_relation() {
        let mut item = unresolved_item();
        apply_recording_metadata(
            &mut item,
            &MusicBrainzRecordingMetadata {
                title: "Song".to_owned(),
                artists: "Artist".to_owned(),
                duration_ms: Some(123_000),
                spotify_track_id: Some("spotify-id".to_owned()),
            },
            42,
        );
        assert_eq!(item.title, "Song");
        assert_eq!(item.artists, "Artist");
        assert_eq!(item.duration_ms, Some(123_000));
        assert_eq!(item.media_id.raw_id, "spotify-id");
        assert_eq!(
            item.provider_url.as_deref(),
            Some("spotify:track:spotify-id")
        );
        assert_eq!(
            item.metadata.provenance.as_deref(),
            Some("listenbrainz-musicbrainz-spotify")
        );
        assert_eq!(item.metadata.observed_at, Some(42));
        assert!(!item.metadata.degraded);
        assert!(!item.metadata.metadata_pending);
    }

    #[test]
    fn recording_metadata_keeps_rows_unresolved_without_a_provider_relation() {
        let mut item = unresolved_item();
        apply_recording_metadata(
            &mut item,
            &MusicBrainzRecordingMetadata {
                title: "Song".to_owned(),
                artists: "Artist".to_owned(),
                duration_ms: Some(123_000),
                spotify_track_id: None,
            },
            42,
        );
        assert_eq!(item.title, "Song");
        assert_eq!(item.artists, "Artist");
        assert_eq!(item.duration_ms, Some(123_000));
        assert!(item.metadata.degraded);
        assert!(!item.metadata.metadata_pending);
        assert_eq!(
            item.metadata.provenance.as_deref(),
            Some("listenbrainz-musicbrainz")
        );
        assert_eq!(
            item.media_id.raw_id,
            "https://musicbrainz.org/recording/12345678-1234-1234-1234-123456789abc"
        );
    }
}
