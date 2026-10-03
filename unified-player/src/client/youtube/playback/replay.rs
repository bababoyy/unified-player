use std::{
    fmt,
    path::PathBuf,
    time::{Duration, UNIX_EPOCH},
};

use futures::StreamExt as _;
use reqwest::Url;

use crate::config::{Configs, YouTubeMusicAuthType, YouTubePlaybackQuality};
use crate::developer_capture::{
    AllowlistedPlayerEndpoint, AllowlistedReplayMethod, CurrentMaterialFailure,
    CurrentMaterialRequest, FreshPlayerAdapterResult, FreshPlayerRequest,
    FreshSemanticReplayAdapter, OfflineAdapterResult, OfflineReplayAdapter, OfflineReplayInput,
    PlayerPostCapability, ReplayAuthKind, ReplayDecision, ReplayFuture, ReplayParserOutcome,
    ReplayPlayerClient, ReplayProviderOutcome, ReplayQualityPolicy, ReplaySelectionOutcome,
};

use super::{
    format::{cipher_for_format, playability_error_kind, select_audio_format, PlayerResponse},
    player::{
        append_player_response_chunk, build_player_request, load_optional_po_tokens,
        load_request_auth, PlayerClient, PlayerVersionSource, RequestAuth, PLAYER_ENDPOINT,
        TV_CONTEXT_NAME,
    },
    source::AudioSourceErrorKind,
};

#[cfg(feature = "private-capture")]
#[derive(Clone, Copy, Debug, Default)]
pub(crate) struct YouTubeOfflineReplayAdapter;

#[cfg(feature = "private-capture")]
impl OfflineReplayAdapter for YouTubeOfflineReplayAdapter {
    fn replay(&self, input: OfflineReplayInput<'_>) -> OfflineAdapterResult {
        OfflineAdapterResult::Decision(replay_player_decision(
            input.player_http_status(),
            input.player_response(),
            input.auth(),
            input.proof_token_present(),
            input.client(),
            input.quality(),
        ))
    }
}

#[cfg(feature = "private-capture")]
#[allow(dead_code)]
pub(crate) struct YouTubeFreshReplayAdapter {
    http: reqwest::Client,
    endpoint: Url,
    auth_type: YouTubeMusicAuthType,
    cookie_path: PathBuf,
    oauth_path: PathBuf,
    po_token_path: Option<PathBuf>,
    cached_tv_client_version: Option<String>,
}

#[cfg(feature = "private-capture")]
impl fmt::Debug for YouTubeFreshReplayAdapter {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("YouTubeFreshReplayAdapter([private current provider material])")
    }
}

#[cfg(feature = "private-capture")]
impl YouTubeFreshReplayAdapter {
    pub(crate) fn from_configs(configs: &Configs) -> Result<Self, CurrentMaterialFailure> {
        let http = reqwest::Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .map_err(|_| CurrentMaterialFailure::Inconclusive)?;
        Ok(Self {
            http,
            endpoint: Url::parse(PLAYER_ENDPOINT).expect("static YouTube player endpoint is valid"),
            auth_type: configs.app_config.youtube.auth_type,
            cookie_path: configs.youtube_music_cookie_path(),
            oauth_path: configs.youtube_music_oauth_path(),
            po_token_path: configs.app_config.youtube.po_token_file.clone(),
            cached_tv_client_version: None,
        })
    }

    #[cfg(test)]
    pub(super) fn new_for_test(
        endpoint: Url,
        auth_type: YouTubeMusicAuthType,
        cookie_path: PathBuf,
        po_token_path: Option<PathBuf>,
    ) -> Self {
        Self {
            http: reqwest::Client::builder()
                .redirect(reqwest::redirect::Policy::none())
                .build()
                .expect("test replay HTTP client is valid"),
            endpoint,
            auth_type,
            cookie_path,
            oauth_path: PathBuf::new(),
            po_token_path,
            cached_tv_client_version: None,
        }
    }
}

#[cfg(feature = "private-capture")]
pub(crate) struct YouTubeFreshReplayMaterial {
    browser_profile_guard: super::super::browser_auth::FreshReplayBrowserProfileGuard,
    auth: RequestAuth,
    player_client: PlayerClient,
    replay_client: ReplayPlayerClient,
    po_token: Option<String>,
}

#[cfg(feature = "private-capture")]
impl FreshSemanticReplayAdapter for YouTubeFreshReplayAdapter {
    type CurrentMaterial = YouTubeFreshReplayMaterial;

    fn acquire_current(
        &self,
        request: CurrentMaterialRequest,
    ) -> ReplayFuture<'_, Result<Self::CurrentMaterial, CurrentMaterialFailure>> {
        Box::pin(async move {
            let browser_profile_guard =
                super::super::browser_auth::try_lock_browser_profile_for_fresh_replay()
                    .ok_or(CurrentMaterialFailure::BrowserContended)?;
            let auth = load_request_auth(
                self.auth_type,
                &self.cookie_path,
                &self.oauth_path,
                Some(request.requested_at_unix_ms() / 1_000),
            )
            .await
            .map_err(|_| CurrentMaterialFailure::AuthUnavailable)?;
            let player_client = replay_current_player_client(
                request.client(),
                request.requested_at_unix_ms(),
                self.cached_tv_client_version.as_deref(),
            )?;
            let po_tokens = load_optional_po_tokens(self.po_token_path.as_deref()).await;
            let po_token = player_client
                .player_po_token(po_tokens.player_for(&player_client))
                .map(ToOwned::to_owned);
            Ok(YouTubeFreshReplayMaterial {
                browser_profile_guard,
                auth,
                player_client,
                replay_client: request.client(),
                po_token,
            })
        })
    }

    fn post_player<'a>(
        &'a self,
        _capability: PlayerPostCapability,
        request: FreshPlayerRequest<'a>,
        current: Self::CurrentMaterial,
    ) -> ReplayFuture<'a, FreshPlayerAdapterResult> {
        Box::pin(async move {
            let _browser_profile_guard = current.browser_profile_guard;
            if request.endpoint() != AllowlistedPlayerEndpoint::YouTubePlayer
                || request.method() != AllowlistedReplayMethod::Post
                || request.client() != current.replay_client
            {
                return FreshPlayerAdapterResult::Inconclusive;
            }
            let request_auth = current.player_client.auth_for_request(&current.auth);
            let po_token = current
                .player_client
                .player_po_token(current.po_token.as_deref());
            let auth_kind = replay_auth_kind(&request_auth);
            let proof_token_present = po_token.is_some();
            let Ok(player_request) = build_player_request(
                &self.http,
                self.endpoint.clone(),
                request.media_id(),
                &request_auth,
                &current.player_client,
                po_token,
                None,
            ) else {
                return FreshPlayerAdapterResult::Inconclusive;
            };
            let Ok(response) = self.http.execute(player_request).await else {
                return FreshPlayerAdapterResult::NetworkFailed;
            };
            let status = response.status().as_u16();
            let mut stream = response.bytes_stream();
            let mut body = zeroize::Zeroizing::new(Vec::new());
            while let Some(chunk) = stream.next().await {
                let Ok(chunk) = chunk else {
                    return FreshPlayerAdapterResult::NetworkFailed;
                };
                if !append_player_response_chunk(&mut body, &chunk) {
                    return FreshPlayerAdapterResult::Inconclusive;
                }
            }
            FreshPlayerAdapterResult::Decision(replay_player_decision(
                status,
                &body,
                auth_kind,
                proof_token_present,
                request.client(),
                request.quality(),
            ))
        })
    }
}

#[cfg(feature = "private-capture")]
fn replay_current_player_client(
    client: ReplayPlayerClient,
    requested_at_unix_ms: u64,
    cached_tv_version: Option<&str>,
) -> Result<PlayerClient, CurrentMaterialFailure> {
    match client {
        ReplayPlayerClient::AndroidVr => Ok(PlayerClient::android_vr()),
        ReplayPlayerClient::TvHtml5 => {
            let (version, source) = if let Some(version) = cached_tv_version {
                (version.to_owned(), PlayerVersionSource::Cached)
            } else {
                let requested_at = UNIX_EPOCH
                    .checked_add(Duration::from_millis(requested_at_unix_ms))
                    .ok_or(CurrentMaterialFailure::Inconclusive)?;
                (
                    format!(
                        "5.{}",
                        chrono::DateTime::<chrono::Utc>::from(requested_at).format("%Y%m%d")
                    ),
                    PlayerVersionSource::Fallback,
                )
            };
            Ok(PlayerClient::tv(version, source))
        }
    }
}

#[cfg(feature = "private-capture")]
const fn replay_auth_kind(auth: &RequestAuth) -> ReplayAuthKind {
    match auth {
        RequestAuth::None => ReplayAuthKind::None,
        RequestAuth::Browser(_) => ReplayAuthKind::Browser,
        RequestAuth::Bearer(_) => ReplayAuthKind::OAuthBearer,
    }
}

#[cfg(feature = "private-capture")]
pub(crate) fn replay_player_decision(
    player_http_status: u16,
    player_response: &[u8],
    auth: ReplayAuthKind,
    proof_token_present: bool,
    client: ReplayPlayerClient,
    quality: ReplayQualityPolicy,
) -> ReplayDecision {
    let provider = match player_http_status {
        429 => Some(ReplayProviderOutcome::RateLimited),
        401 | 403 if player_http_status == 403 && proof_token_present => {
            Some(ReplayProviderOutcome::ProofToken)
        }
        401 | 403 if auth == ReplayAuthKind::None => {
            Some(ReplayProviderOutcome::ProviderUnavailable)
        }
        401 | 403 => Some(ReplayProviderOutcome::Authentication),
        200..=299 => None,
        _ => Some(ReplayProviderOutcome::NetworkFailed),
    };
    if let Some(provider) = provider {
        return ReplayDecision {
            parser: ReplayParserOutcome::NotRun,
            provider,
            streaming_data_present: false,
            returned_formats: 0,
            supported_formats: 0,
            direct_formats: 0,
            cipher_formats: 0,
            selection: ReplaySelectionOutcome::NotAttempted,
        };
    }

    let response: PlayerResponse = match serde_json::from_slice(player_response) {
        Ok(response) => response,
        Err(_) => {
            return ReplayDecision {
                parser: ReplayParserOutcome::Malformed,
                provider: ReplayProviderOutcome::Contract,
                streaming_data_present: false,
                returned_formats: 0,
                supported_formats: 0,
                direct_formats: 0,
                cipher_formats: 0,
                selection: ReplaySelectionOutcome::NotAttempted,
            };
        }
    };
    let streaming_data_present = response.streaming_data.is_some();
    let formats = response
        .streaming_data
        .map_or_else(Vec::new, |streaming| streaming.adaptive_formats);
    let returned_formats = replay_count(formats.len());
    let supported_formats = replay_count(
        formats
            .iter()
            .filter(|format| format.mime_type.starts_with("audio/mp4"))
            .count(),
    );
    let direct_formats = replay_count(formats.iter().filter(|format| format.url.is_some()).count());
    let cipher_formats = replay_count(
        formats
            .iter()
            .filter(|format| cipher_for_format(format).is_some())
            .count(),
    );
    if response.playability_status.status != "OK" {
        return ReplayDecision {
            parser: ReplayParserOutcome::Parsed,
            provider: match playability_error_kind(&response.playability_status.status) {
                Some(AudioSourceErrorKind::Authentication) => ReplayProviderOutcome::Authentication,
                Some(AudioSourceErrorKind::ConsentAgeOrRegion) => {
                    ReplayProviderOutcome::ConsentAgeRegion
                }
                _ => ReplayProviderOutcome::ProviderUnavailable,
            },
            streaming_data_present,
            returned_formats,
            supported_formats,
            direct_formats,
            cipher_formats,
            selection: ReplaySelectionOutcome::NotAttempted,
        };
    }
    if !streaming_data_present {
        return ReplayDecision {
            parser: ReplayParserOutcome::Parsed,
            provider: ReplayProviderOutcome::Contract,
            streaming_data_present: false,
            returned_formats,
            supported_formats,
            direct_formats,
            cipher_formats,
            selection: ReplaySelectionOutcome::NotAttempted,
        };
    }

    let playback_quality = match quality {
        ReplayQualityPolicy::High => YouTubePlaybackQuality::High,
        ReplayQualityPolicy::DataSaver => YouTubePlaybackQuality::DataSaver,
    };
    let source_name = match client {
        ReplayPlayerClient::AndroidVr => "ANDROID_VR",
        ReplayPlayerClient::TvHtml5 => TV_CONTEXT_NAME,
    };
    let selection = match select_audio_format(formats, playback_quality, source_name) {
        Ok(source) => ReplaySelectionOutcome::Selected { itag: source.itag },
        Err(error) if error.kind == AudioSourceErrorKind::Decipher => {
            ReplaySelectionOutcome::CipherOnly
        }
        Err(error) if error.kind == AudioSourceErrorKind::UnsupportedFormat => {
            ReplaySelectionOutcome::UnsupportedFormat
        }
        Err(error) if error.kind == AudioSourceErrorKind::Unavailable => {
            ReplaySelectionOutcome::NoDirectFormat
        }
        Err(_) => {
            return ReplayDecision {
                parser: ReplayParserOutcome::Parsed,
                provider: ReplayProviderOutcome::Contract,
                streaming_data_present,
                returned_formats,
                supported_formats,
                direct_formats,
                cipher_formats,
                selection: ReplaySelectionOutcome::NotAttempted,
            };
        }
    };
    ReplayDecision {
        parser: ReplayParserOutcome::Parsed,
        provider: ReplayProviderOutcome::Playable,
        streaming_data_present,
        returned_formats,
        supported_formats,
        direct_formats,
        cipher_formats,
        selection,
    }
}

#[cfg(feature = "private-capture")]
fn replay_count(value: usize) -> u16 {
    u16::try_from(value).unwrap_or(u16::MAX)
}
