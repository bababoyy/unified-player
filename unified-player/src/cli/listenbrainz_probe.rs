use std::{
    collections::{BTreeMap, BTreeSet},
    future::Future,
    time::{Duration, Instant},
};

use anyhow::{Context, Result};
use clap::ArgMatches;
use serde::{Deserialize, Serialize};

use crate::{
    client::listenbrainz::{
        artist_from_spotify_id, spotify_album_relations_for_release_group,
        top_recordings_for_artist, top_release_groups_for_artist, PopularReleaseGroup,
    },
    config::Configs,
    state::{AppData, UnifiedPlaylist},
};

use super::listenbrainz_manifest::{
    description_envelope, description_manifest, parse_description_manifest, provider_slug,
};

const LISTENBRAINZ_API_ROOT: &str = "https://api.listenbrainz.org/1";
const MUSICBRAINZ_USER_AGENT: &str = concat!(
    "unified-player/",
    env!("CARGO_PKG_VERSION"),
    " (https://github.com/bababoyy/unified-player)"
);
const ARTIST_SAMPLE_LIMIT: usize = 10;
const ALBUM_RESOLUTION_SAMPLE_LIMIT: usize = 5;
const MUSICBRAINZ_MINIMUM_INTERVAL: Duration = Duration::from_secs(1);
const MUSICBRAINZ_MAX_503_RETRIES: usize = 1;

#[derive(Debug, Serialize)]
struct ListenBrainzProbeReport {
    schema_version: u8,
    remote_writes_performed: bool,
    auth: AuthReport,
    artist_enrichment: Option<ArtistEnrichmentReport>,
    identity_coverage: Option<IdentityCoverageReport>,
    description_manifest: Option<DescriptionManifestReport>,
    scrobble: ScrobbleReadinessReport,
    total_duration_ms: u128,
}

#[derive(Debug, Serialize)]
struct AuthReport {
    configured: bool,
    valid: bool,
    user_name: Option<String>,
    duration_ms: u128,
}

#[derive(Debug, Serialize)]
struct ArtistEnrichmentReport {
    spotify_artist_id: String,
    artist_mbid: Option<String>,
    artist_name: Option<String>,
    top_recordings_available: usize,
    top_recordings_sample: Vec<TopRecordingSummary>,
    album_fallback: AlbumFallbackReport,
    musicbrainz_duration_ms: u128,
    listenbrainz_duration_ms: u128,
}

#[derive(Debug, Serialize, Deserialize)]
struct TopRecordingSummary {
    recording_mbid: String,
    recording_name: String,
    total_listen_count: Option<u64>,
    total_user_count: Option<u64>,
}

#[derive(Debug, Serialize, Deserialize)]
struct AlbumFallbackReport {
    source: &'static str,
    release_groups_available: usize,
    release_groups_sample: Vec<TopReleaseGroupSummary>,
    spotify_album_resolution: AlbumResolutionReport,
    duration_ms: u128,
}

#[derive(Debug, Serialize, Deserialize)]
struct AlbumResolutionReport {
    method: &'static str,
    attempted_release_groups: usize,
    matched_release_groups: usize,
    unmatched_release_groups: usize,
    unavailable_release_groups: usize,
    spotify_album_ids_found: usize,
    sample: Vec<ReleaseGroupResolutionSummary>,
    musicbrainz_requests: usize,
    musicbrainz_503_retries: usize,
    minimum_request_interval_ms: u128,
    duration_ms: u128,
}

#[derive(Debug, Serialize, Deserialize)]
struct ReleaseGroupResolutionSummary {
    release_group_mbid: String,
    status: &'static str,
    spotify_album_ids: Vec<String>,
}

#[derive(Debug, Serialize, Deserialize)]
struct TopReleaseGroupSummary {
    release_group_mbid: String,
    release_name: String,
    release_date: Option<String>,
    release_type: Option<String>,
    total_listen_count: Option<u64>,
    total_user_count: Option<u64>,
}

#[derive(Debug, Serialize)]
struct IdentityCoverageReport {
    playlist_id: String,
    lookup_status: &'static str,
    total_items: usize,
    sampled_items: usize,
    attempted_items: usize,
    resolved_recording_mbids: usize,
    coverage_percent: f64,
    by_provider: BTreeMap<String, ProviderCoverage>,
    duration_ms: u128,
}

#[derive(Debug, Default, Serialize)]
struct ProviderCoverage {
    attempted: usize,
    resolved: usize,
}

#[derive(Debug, Serialize)]
struct DescriptionManifestReport {
    playlist_id: String,
    schema_version: u8,
    entry_count: usize,
    characters: usize,
    bytes: usize,
    budget_characters: usize,
    fits_budget: bool,
    round_trip_exact: bool,
    snapshot_hash: String,
}

#[derive(Debug, Serialize)]
struct ScrobbleReadinessReport {
    token_ready: bool,
    submission_endpoint: &'static str,
    real_submission_performed: bool,
    remaining_invariants: [&'static str; 4],
}

#[derive(Debug, Deserialize)]
struct TokenValidationResponse {
    #[serde(default)]
    valid: bool,
    user_name: Option<String>,
}

struct MusicBrainzRequestPolicy {
    last_request_started: Option<Instant>,
    request_count: usize,
    retry_count: usize,
    minimum_interval: Duration,
}

impl Default for MusicBrainzRequestPolicy {
    fn default() -> Self {
        Self {
            last_request_started: None,
            request_count: 0,
            retry_count: 0,
            minimum_interval: MUSICBRAINZ_MINIMUM_INTERVAL,
        }
    }
}

impl MusicBrainzRequestPolicy {
    async fn execute<T, F, Fut>(&mut self, mut request: F) -> Result<T>
    where
        F: FnMut() -> Fut,
        Fut: Future<Output = Result<T>>,
    {
        for attempt in 0..=MUSICBRAINZ_MAX_503_RETRIES {
            if let Some(last_request_started) = self.last_request_started {
                let elapsed = last_request_started.elapsed();
                if elapsed < self.minimum_interval {
                    tokio::time::sleep(self.minimum_interval.saturating_sub(elapsed)).await;
                }
            }
            self.last_request_started = Some(Instant::now());
            self.request_count += 1;

            match request().await {
                Ok(value) => return Ok(value),
                Err(error) if should_retry_musicbrainz(&error, attempt) => {
                    self.retry_count += 1;
                }
                Err(error) => return Err(error),
            }
        }

        unreachable!("MusicBrainz retry loop always returns")
    }
}

fn should_retry_musicbrainz(error: &anyhow::Error, attempt: usize) -> bool {
    should_retry_musicbrainz_status(
        error.chain().find_map(|cause| {
            cause
                .downcast_ref::<reqwest::Error>()
                .and_then(reqwest::Error::status)
        }),
        attempt,
    )
}

const fn should_retry_musicbrainz_status(
    status: Option<reqwest::StatusCode>,
    attempt: usize,
) -> bool {
    attempt < MUSICBRAINZ_MAX_503_RETRIES
        && matches!(status, Some(reqwest::StatusCode::SERVICE_UNAVAILABLE))
}

fn validated_enrichment_token(token: Option<&str>, valid: bool) -> Option<&str> {
    valid.then_some(token).flatten()
}

pub(super) fn run(args: &ArgMatches, configs: &Configs) -> Result<()> {
    let spotify_artist_id = args.get_one::<String>("spotify_artist_id").cloned();
    let unified_id = args.get_one::<String>("unified_id").cloned();
    let sample_limit = *args
        .get_one::<usize>("sample_limit")
        .expect("sample-limit has a default");
    let description_budget = *args
        .get_one::<usize>("description_budget")
        .expect("description-budget has a default");
    anyhow::ensure!(sample_limit > 0, "sample-limit must be positive");
    anyhow::ensure!(sample_limit <= 1_000, "sample-limit must not exceed 1000");
    anyhow::ensure!(
        description_budget > 0,
        "description-budget must be positive"
    );

    let token = configs
        .listenbrainz_token()
        .map(|token| token.trim().to_owned())
        .filter(|token| !token.is_empty());
    let runtime = tokio::runtime::Runtime::new()?;
    let report = runtime.block_on(build_report(
        configs,
        token.as_deref(),
        spotify_artist_id.as_deref(),
        unified_id.as_deref(),
        sample_limit,
        description_budget,
    ))?;

    if args.get_flag("json") {
        println!("{}", serde_json::to_string_pretty(&report)?);
    } else {
        print_text_report(&report);
    }
    Ok(())
}

async fn build_report(
    configs: &Configs,
    token: Option<&str>,
    spotify_artist_id: Option<&str>,
    unified_id: Option<&str>,
    sample_limit: usize,
    description_budget: usize,
) -> Result<ListenBrainzProbeReport> {
    let started = Instant::now();
    let http = reqwest::Client::builder()
        .user_agent(MUSICBRAINZ_USER_AGENT)
        .timeout(Duration::from_secs(20))
        .build()?;
    let auth = validate_token(&http, token).await?;

    let artist_enrichment = match spotify_artist_id {
        Some(artist_id) => Some(
            probe_artist_enrichment(
                &http,
                validated_enrichment_token(token, auth.valid),
                artist_id,
            )
            .await?,
        ),
        None => None,
    };

    let playlist = unified_id
        .map(|playlist_id| load_unified_playlist(configs, playlist_id))
        .transpose()?;
    let description_manifest = playlist
        .as_ref()
        .map(|playlist| assess_description_manifest(playlist, description_budget))
        .transpose()?;
    let identity_coverage = match (playlist.as_ref(), token, auth.valid) {
        (Some(playlist), Some(token), true) => {
            Some(probe_identity_coverage(&http, token, playlist, sample_limit).await?)
        }
        (Some(playlist), _, _) => Some(empty_identity_coverage(playlist, sample_limit)),
        (None, _, _) => None,
    };

    Ok(ListenBrainzProbeReport {
        schema_version: 3,
        remote_writes_performed: false,
        scrobble: scrobble_readiness(auth.valid),
        auth,
        artist_enrichment,
        identity_coverage,
        description_manifest,
        total_duration_ms: started.elapsed().as_millis(),
    })
}

async fn validate_token(http: &reqwest::Client, token: Option<&str>) -> Result<AuthReport> {
    let started = Instant::now();
    let Some(token) = token else {
        return Ok(AuthReport {
            configured: false,
            valid: false,
            user_name: None,
            duration_ms: started.elapsed().as_millis(),
        });
    };
    let response = http
        .get(format!("{LISTENBRAINZ_API_ROOT}/validate-token"))
        .header(reqwest::header::AUTHORIZATION, format!("Token {token}"))
        .send()
        .await?
        .error_for_status()?
        .json::<TokenValidationResponse>()
        .await?;
    Ok(AuthReport {
        configured: true,
        valid: response.valid,
        user_name: response.user_name,
        duration_ms: started.elapsed().as_millis(),
    })
}

async fn probe_album_resolution(
    http: &reqwest::Client,
    policy: &mut MusicBrainzRequestPolicy,
    release_groups: &[PopularReleaseGroup],
) -> AlbumResolutionReport {
    let started = Instant::now();
    let requests_before = policy.request_count;
    let retries_before = policy.retry_count;
    let mut matched_release_groups = 0;
    let mut unmatched_release_groups = 0;
    let mut unavailable_release_groups = 0;
    let mut album_ids = BTreeSet::new();
    let mut sample = Vec::new();

    for release_group in release_groups.iter().take(ALBUM_RESOLUTION_SAMPLE_LIMIT) {
        let result = policy
            .execute(|| {
                spotify_album_relations_for_release_group(http, &release_group.release_group_mbid)
            })
            .await;
        let (status, mut spotify_album_ids) = if let Ok(relations) = result {
            let mut ids = relations
                .into_iter()
                .map(|relation| relation.spotify_album_id)
                .collect::<Vec<_>>();
            ids.sort();
            ids.dedup();
            if ids.is_empty() {
                unmatched_release_groups += 1;
                ("unmatched", ids)
            } else {
                matched_release_groups += 1;
                album_ids.extend(ids.iter().cloned());
                ("matched", ids)
            }
        } else {
            unavailable_release_groups += 1;
            ("unavailable", Vec::new())
        };
        spotify_album_ids.truncate(ARTIST_SAMPLE_LIMIT);
        sample.push(ReleaseGroupResolutionSummary {
            release_group_mbid: release_group.release_group_mbid.clone(),
            status,
            spotify_album_ids,
        });
    }

    AlbumResolutionReport {
        method: "musicbrainz_release_url_relations",
        attempted_release_groups: sample.len(),
        matched_release_groups,
        unmatched_release_groups,
        unavailable_release_groups,
        spotify_album_ids_found: album_ids.len(),
        sample,
        musicbrainz_requests: policy.request_count.saturating_sub(requests_before),
        musicbrainz_503_retries: policy.retry_count.saturating_sub(retries_before),
        minimum_request_interval_ms: policy.minimum_interval.as_millis(),
        duration_ms: started.elapsed().as_millis(),
    }
}

async fn probe_artist_enrichment(
    http: &reqwest::Client,
    token: Option<&str>,
    spotify_artist_id: &str,
) -> Result<ArtistEnrichmentReport> {
    let mut musicbrainz_policy = MusicBrainzRequestPolicy::default();
    let musicbrainz_started = Instant::now();
    let artist = musicbrainz_policy
        .execute(|| artist_from_spotify_id(http, spotify_artist_id))
        .await?;
    let musicbrainz_duration_ms = musicbrainz_started.elapsed().as_millis();

    let (top_recordings, listenbrainz_duration_ms, top_release_groups, release_groups_duration_ms) =
        match artist.as_ref() {
            Some(artist) => {
                let recordings = async {
                    let started = Instant::now();
                    let recordings = top_recordings_for_artist(http, &artist.id, token).await?;
                    Ok::<_, anyhow::Error>((recordings, started.elapsed().as_millis()))
                };
                let release_groups = async {
                    let started = Instant::now();
                    let release_groups =
                        top_release_groups_for_artist(http, &artist.id, token).await?;
                    Ok::<_, anyhow::Error>((release_groups, started.elapsed().as_millis()))
                };
                let ((recordings, recordings_ms), (release_groups, release_groups_ms)) =
                    tokio::try_join!(recordings, release_groups)?;
                (recordings, recordings_ms, release_groups, release_groups_ms)
            }
            None => (Vec::new(), 0, Vec::new(), 0),
        };
    let spotify_album_resolution =
        probe_album_resolution(http, &mut musicbrainz_policy, &top_release_groups).await;
    let release_groups_available = top_release_groups.len();
    let release_groups_sample = top_release_groups
        .into_iter()
        .take(ARTIST_SAMPLE_LIMIT)
        .map(|release| TopReleaseGroupSummary {
            release_group_mbid: release.release_group_mbid,
            release_name: release.release_name,
            release_date: release.release_date,
            release_type: release.release_type,
            total_listen_count: release.total_listen_count,
            total_user_count: release.total_user_count,
        })
        .collect();
    let album_fallback = AlbumFallbackReport {
        source: "listenbrainz_top_release_groups",
        release_groups_available,
        release_groups_sample,
        spotify_album_resolution,
        duration_ms: release_groups_duration_ms,
    };
    let top_recordings_available = top_recordings.len();
    let top_recordings_sample = top_recordings
        .into_iter()
        .take(ARTIST_SAMPLE_LIMIT)
        .map(|recording| TopRecordingSummary {
            recording_mbid: recording.recording_mbid,
            recording_name: recording.recording_name,
            total_listen_count: recording.total_listen_count,
            total_user_count: recording.total_user_count,
        })
        .collect();

    Ok(ArtistEnrichmentReport {
        spotify_artist_id: spotify_artist_id.to_owned(),
        artist_mbid: artist.as_ref().map(|artist| artist.id.clone()),
        artist_name: artist.map(|artist| artist.name),
        top_recordings_available,
        top_recordings_sample,
        album_fallback,
        musicbrainz_duration_ms,
        listenbrainz_duration_ms,
    })
}

fn load_unified_playlist(configs: &Configs, playlist_id: &str) -> Result<UnifiedPlaylist> {
    AppData::new(&configs.config_folder, &configs.cache_folder)
        .unified_playlists
        .into_iter()
        .find(|playlist| playlist.id == playlist_id)
        .with_context(|| format!("Unified playlist not found: {playlist_id}"))
}

async fn probe_identity_coverage(
    http: &reqwest::Client,
    token: &str,
    playlist: &UnifiedPlaylist,
    sample_limit: usize,
) -> Result<IdentityCoverageReport> {
    let started = Instant::now();
    let sampled = playlist.items.iter().take(sample_limit).collect::<Vec<_>>();
    let recordings = sampled
        .iter()
        .map(|item| {
            serde_json::json!({
                "recording_name": item.title,
                "artist_name": item.artists,
            })
        })
        .collect::<Vec<_>>();
    let response = if recordings.is_empty() {
        Vec::new()
    } else {
        http.post(format!("{LISTENBRAINZ_API_ROOT}/metadata/lookup/"))
            .header(reqwest::header::AUTHORIZATION, format!("Token {token}"))
            .json(&serde_json::json!({ "recordings": recordings }))
            .send()
            .await?
            .error_for_status()?
            .json::<Vec<serde_json::Value>>()
            .await?
    };
    let mut resolved = vec![false; sampled.len()];
    for item in response {
        let Some(index) = item.get("index").and_then(serde_json::Value::as_u64) else {
            continue;
        };
        if item
            .get("recording_mbid")
            .and_then(serde_json::Value::as_str)
            .is_some()
        {
            if let Some(slot) = resolved.get_mut(index as usize) {
                *slot = true;
            }
        }
    }
    Ok(identity_coverage_report(
        playlist,
        &sampled,
        &resolved,
        started.elapsed().as_millis(),
    ))
}

fn empty_identity_coverage(
    playlist: &UnifiedPlaylist,
    sample_limit: usize,
) -> IdentityCoverageReport {
    IdentityCoverageReport {
        playlist_id: playlist.id.clone(),
        lookup_status: "skipped_auth_unavailable",
        total_items: playlist.items.len(),
        sampled_items: playlist.items.len().min(sample_limit),
        attempted_items: 0,
        resolved_recording_mbids: 0,
        coverage_percent: 0.0,
        by_provider: BTreeMap::new(),
        duration_ms: 0,
    }
}

fn identity_coverage_report(
    playlist: &UnifiedPlaylist,
    sampled: &[&crate::state::UnifiedPlaylistItem],
    resolved: &[bool],
    duration_ms: u128,
) -> IdentityCoverageReport {
    let mut by_provider = BTreeMap::new();
    for (item, is_resolved) in sampled.iter().zip(resolved.iter().copied()) {
        let coverage = by_provider
            .entry(provider_slug(item.media_id.provider).to_owned())
            .or_insert_with(ProviderCoverage::default);
        coverage.attempted += 1;
        coverage.resolved += usize::from(is_resolved);
    }
    let resolved_recording_mbids = resolved.iter().filter(|value| **value).count();
    let coverage_percent = if sampled.is_empty() {
        0.0
    } else {
        resolved_recording_mbids as f64 * 100.0 / sampled.len() as f64
    };
    IdentityCoverageReport {
        playlist_id: playlist.id.clone(),
        lookup_status: "completed",
        total_items: playlist.items.len(),
        sampled_items: sampled.len(),
        attempted_items: sampled.len(),
        resolved_recording_mbids,
        coverage_percent,
        by_provider,
        duration_ms,
    }
}

fn assess_description_manifest(
    playlist: &UnifiedPlaylist,
    budget_characters: usize,
) -> Result<DescriptionManifestReport> {
    let manifest = description_manifest(playlist)?;
    let envelope = description_envelope(playlist)?;
    let parsed = parse_description_manifest(&envelope)?;
    Ok(DescriptionManifestReport {
        playlist_id: playlist.id.clone(),
        schema_version: manifest.schema_version,
        entry_count: manifest.entries.len(),
        characters: envelope.chars().count(),
        bytes: envelope.len(),
        budget_characters,
        fits_budget: envelope.chars().count() <= budget_characters,
        round_trip_exact: parsed == manifest,
        snapshot_hash: manifest.snapshot_hash,
    })
}

const fn scrobble_readiness(token_ready: bool) -> ScrobbleReadinessReport {
    ScrobbleReadinessReport {
        token_ready,
        submission_endpoint: "/1/submit-listens",
        real_submission_performed: false,
        remaining_invariants: [
            "half-track-or-four-minute threshold accounting",
            "provider-neutral playback identity and metadata snapshot",
            "idempotent offline retry and duplicate suppression",
            "explicit privacy and playing-now lifecycle",
        ],
    }
}

fn print_text_report(report: &ListenBrainzProbeReport) {
    println!("ListenBrainz capability probe");
    println!(
        "auth: configured={} valid={} user={}",
        report.auth.configured,
        report.auth.valid,
        report.auth.user_name.as_deref().unwrap_or("unavailable")
    );
    if let Some(artist) = &report.artist_enrichment {
        let album_resolution = &artist.album_fallback.spotify_album_resolution;
        println!(
            "artist: mbid={} top_recordings={} release_groups={} spotify_album_matches={}/{}",
            artist.artist_mbid.as_deref().unwrap_or("unresolved"),
            artist.top_recordings_available,
            artist.album_fallback.release_groups_available,
            album_resolution.matched_release_groups,
            album_resolution.attempted_release_groups,
        );
    }
    if let Some(coverage) = &report.identity_coverage {
        println!(
            "identity: resolved={}/{} coverage={:.1}%",
            coverage.resolved_recording_mbids, coverage.attempted_items, coverage.coverage_percent
        );
    }
    if let Some(manifest) = &report.description_manifest {
        println!(
            "description: characters={}/{} fits={} round_trip={}",
            manifest.characters,
            manifest.budget_characters,
            manifest.fits_budget,
            manifest.round_trip_exact
        );
    }
    println!(
        "scrobble: token_ready={} real_submission_performed=false",
        report.scrobble.token_ready
    );
    println!("remote_writes_performed=false");
}

#[cfg(test)]
mod tests {
    use super::{
        assess_description_manifest, empty_identity_coverage, identity_coverage_report,
        should_retry_musicbrainz_status, validated_enrichment_token, AlbumFallbackReport,
        AlbumResolutionReport, AuthReport, ListenBrainzProbeReport, ReleaseGroupResolutionSummary,
        ScrobbleReadinessReport, TopReleaseGroupSummary, MUSICBRAINZ_MAX_503_RETRIES,
        MUSICBRAINZ_MINIMUM_INTERVAL,
    };
    use crate::cli::listenbrainz_manifest::{
        description_envelope, description_manifest, parse_description_manifest,
    };
    use crate::state::{
        MediaId, MediaKind, PlaylistEntryId, Provider, UnifiedPlaylist, UnifiedPlaylistItem,
    };

    fn playlist() -> UnifiedPlaylist {
        UnifiedPlaylist {
            id: "mix".to_owned(),
            name: "Mix".to_owned(),
            items: vec![
                UnifiedPlaylistItem {
                    entry_id: PlaylistEntryId(1),
                    media_id: MediaId {
                        provider: Provider::Spotify,
                        kind: MediaKind::Track,
                        raw_id: "spotify-track".to_owned(),
                    },
                    title: "First".to_owned(),
                    artists: "Artist".to_owned(),
                    ..UnifiedPlaylistItem::default()
                },
                UnifiedPlaylistItem {
                    entry_id: PlaylistEntryId(2),
                    media_id: MediaId {
                        provider: Provider::YouTubeMusic,
                        kind: MediaKind::Video,
                        raw_id: "youtube-video".to_owned(),
                    },
                    title: "Second".to_owned(),
                    artists: "Artist".to_owned(),
                    ..UnifiedPlaylistItem::default()
                },
            ],
            next_entry_id: 3,
            ..UnifiedPlaylist::default()
        }
    }

    #[test]
    fn description_manifest_round_trips_provider_identity() {
        let playlist = playlist();
        let manifest = description_manifest(&playlist).unwrap();
        let envelope = description_envelope(&playlist).unwrap();

        assert_eq!(parse_description_manifest(&envelope).unwrap(), manifest);
        let report = assess_description_manifest(&playlist, 9_000).unwrap();
        assert!(report.fits_budget);
        assert!(report.round_trip_exact);
        assert_eq!(report.schema_version, 2);
        assert_eq!(report.entry_count, 2);
    }

    #[test]
    fn coverage_is_counted_per_provider() {
        let playlist = playlist();
        let sampled = playlist.items.iter().collect::<Vec<_>>();
        let report = identity_coverage_report(&playlist, &sampled, &[true, false], 12);

        assert_eq!(report.resolved_recording_mbids, 1);
        assert_eq!(report.coverage_percent, 50.0);
        assert_eq!(report.by_provider["spotify"].resolved, 1);
        assert_eq!(report.by_provider["youtube-music"].resolved, 0);
    }

    #[test]
    fn missing_auth_does_not_claim_an_identity_lookup_attempt() {
        let report = empty_identity_coverage(&playlist(), 100);

        assert_eq!(report.lookup_status, "skipped_auth_unavailable");
        assert_eq!(report.sampled_items, 2);
        assert_eq!(report.attempted_items, 0);
        assert!(report.by_provider.is_empty());
    }

    #[test]
    fn json_report_never_contains_token_material() {
        let report = ListenBrainzProbeReport {
            schema_version: 3,
            remote_writes_performed: false,
            auth: AuthReport {
                configured: true,
                valid: true,
                user_name: Some("listener".to_owned()),
                duration_ms: 1,
            },
            artist_enrichment: None,
            identity_coverage: None,
            description_manifest: None,
            scrobble: ScrobbleReadinessReport {
                token_ready: true,
                submission_endpoint: "/1/submit-listens",
                real_submission_performed: false,
                remaining_invariants: ["one", "two", "three", "four"],
            },
            total_duration_ms: 1,
        };
        let json = serde_json::to_string(&report).unwrap();

        assert!(!json.contains("secret-token"));
        assert!(!json.contains("access_token"));
        assert!(!json.contains("authorization"));
        assert!(json.contains("remote_writes_performed"));
    }

    #[test]
    fn album_fallback_report_is_bounded_and_reports_only_safe_direct_identity() {
        let report = AlbumFallbackReport {
            source: "listenbrainz_top_release_groups",
            release_groups_available: 12,
            release_groups_sample: vec![TopReleaseGroupSummary {
                release_group_mbid: "release-group-mbid".to_owned(),
                release_name: "The Album".to_owned(),
                release_date: Some("1994-03-08".to_owned()),
                release_type: Some("Album".to_owned()),
                total_listen_count: Some(42),
                total_user_count: Some(7),
            }],
            spotify_album_resolution: AlbumResolutionReport {
                method: "musicbrainz_release_url_relations",
                attempted_release_groups: 1,
                matched_release_groups: 1,
                unmatched_release_groups: 0,
                unavailable_release_groups: 0,
                spotify_album_ids_found: 1,
                sample: vec![ReleaseGroupResolutionSummary {
                    release_group_mbid: "release-group-mbid".to_owned(),
                    status: "matched",
                    spotify_album_ids: vec!["spotify-album-id".to_owned()],
                }],
                musicbrainz_requests: 1,
                musicbrainz_503_retries: 0,
                minimum_request_interval_ms: 1_000,
                duration_ms: 2,
            },
            duration_ms: 3,
        };
        let json = serde_json::to_string(&report).unwrap();

        assert!(json.contains("listenbrainz_top_release_groups"));
        assert!(json.contains("release-group-mbid"));
        assert!(json.contains("musicbrainz_release_url_relations"));
        assert!(json.contains("spotify-album-id"));
        assert!(!json.contains("https://"));
        assert!(!json.contains("authorization"));
        assert!(!json.contains("token"));
    }

    #[test]
    fn enrichment_uses_only_a_validated_configured_token() {
        assert_eq!(
            validated_enrichment_token(Some("configured-token"), true),
            Some("configured-token")
        );
        assert_eq!(
            validated_enrichment_token(Some("configured-token"), false),
            None
        );
        assert_eq!(validated_enrichment_token(None, true), None);
    }

    #[test]
    fn musicbrainz_policy_retries_only_the_first_503_after_one_second() {
        assert_eq!(MUSICBRAINZ_MINIMUM_INTERVAL.as_millis(), 1_000);
        assert_eq!(MUSICBRAINZ_MAX_503_RETRIES, 1);
        assert!(should_retry_musicbrainz_status(
            Some(reqwest::StatusCode::SERVICE_UNAVAILABLE),
            0
        ));
        assert!(!should_retry_musicbrainz_status(
            Some(reqwest::StatusCode::SERVICE_UNAVAILABLE),
            1
        ));
        assert!(!should_retry_musicbrainz_status(
            Some(reqwest::StatusCode::TOO_MANY_REQUESTS),
            0
        ));
    }
}
