use std::{
    collections::HashMap,
    path::PathBuf,
    sync::{
        atomic::{AtomicBool, AtomicU8, Ordering},
        Arc, Mutex, OnceLock,
    },
    time::{Duration, Instant},
};

use reqwest::{header, Url};
use serde::Serialize;
use tokio_util::sync::CancellationToken;

use crate::config::{Configs, YouTubeMusicAuthType, YouTubePlaybackQuality};

use super::{
    cipher::{parse_player_script_url, CipherEngine, EjsSolutionCacheKey},
    format::{
        apply_gvs_po_token, browser_source_metadata, cipher_contains_media_url,
        playable_formats_for_attempt, select_audio_format, select_browser_audio_target,
        BrowserAudioTarget,
    },
    player::{
        fallback_tv_client_version, fallback_web_client_version, fallback_web_music_client_version,
        load_optional_po_tokens, load_request_auth, parse_innertube_client_version,
        parse_signature_timestamp, parse_tv_client_version, parse_visitor_data, PlayerClient,
        PlayerClientAttempt, PlayerClientKind, PlayerVersionSource, PoTokenMaterial, RequestAuth,
        BROWSER_SESSION_SOURCE_NAME, PLAYER_ENDPOINT, TV_PAGE, TV_USER_AGENT,
        WEB_DEFAULT_CLIENT_VERSION_PREFIX, WEB_MUSIC_DEFAULT_CLIENT_VERSION_PREFIX, WEB_MUSIC_PAGE,
        WEB_PAGE, WEB_USER_AGENT,
    },
    source::{AudioSourceError, AudioSourceErrorKind, AudioSourceResolver, ResolvedAudioSource},
    transport::{
        diagnose_browser_media_target, prepare_resolved_source, shared_media_http_client,
        verify_media_continuation, MediaTransportDiagnostic,
    },
};

#[cfg(feature = "private-capture")]
use super::{
    forensics::{
        bounded_inspection_text, capture_auth_failure, capture_auth_selection,
        capture_format_inventory, capture_network_failure, capture_selection_decision,
        inspect_player_attempt, inspect_player_attempt_error, inspect_player_auth_failure,
        YouTubePlaybackInspection,
    },
    player::request_auth_kind,
};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum PlayerClientPlan {
    Playback,
    #[cfg_attr(not(feature = "private-capture"), allow(dead_code))]
    Inspection,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum YouTubeProbeClient {
    #[default]
    Auto,
    Tv,
    Web,
    WebRemix,
    AndroidVr,
    VisionOs,
}

impl YouTubeProbeClient {
    pub fn from_cli(value: &str) -> Self {
        match value {
            "tv" => Self::Tv,
            "web" => Self::Web,
            "web-remix" => Self::WebRemix,
            "android-vr" => Self::AndroidVr,
            "visionos" => Self::VisionOs,
            _ => Self::Auto,
        }
    }

    pub const fn label(self) -> &'static str {
        match self {
            Self::Auto => "auto",
            Self::Tv => "tv",
            Self::Web => "web",
            Self::WebRemix => "web-remix",
            Self::AndroidVr => "android-vr",
            Self::VisionOs => "visionos",
        }
    }
}

#[derive(Clone, Debug, Serialize)]
pub struct YouTubeProbeAttempt {
    pub(crate) stage: &'static str,
    pub(crate) client: &'static str,
    pub(crate) result: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) status: Option<&'static str>,
    pub(crate) error_category: Option<&'static str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) attempt: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) target: Option<&'static str>,
    pub(crate) duration_ms: u64,
}

impl YouTubeProbeAttempt {
    pub(crate) fn decoder(
        client: &'static str,
        result: &'static str,
        error_category: Option<&'static str>,
        duration: Duration,
    ) -> Self {
        Self {
            stage: "decoder",
            client,
            result,
            status: None,
            error_category,
            attempt: None,
            target: None,
            duration_ms: duration.as_millis() as u64,
        }
    }

    pub(crate) fn decoder_media(
        client: &'static str,
        attempt: u64,
        target: &'static str,
        result: &'static str,
        status: &'static str,
        error_category: Option<&'static str>,
        duration: Duration,
    ) -> Self {
        Self {
            stage: "decoder_media_attempt",
            client,
            result,
            status: Some(status),
            error_category,
            attempt: Some(attempt),
            target: Some(target),
            duration_ms: duration.as_millis() as u64,
        }
    }
}

#[derive(Clone, Default)]
pub(super) struct YouTubeProbeRecorder {
    attempts: Arc<Mutex<Vec<YouTubeProbeAttempt>>>,
}

impl YouTubeProbeRecorder {
    fn record(
        &self,
        stage: &'static str,
        client: &'static str,
        result: &'static str,
        error: Option<&AudioSourceError>,
        elapsed: Duration,
    ) {
        self.attempts
            .lock()
            .expect("YouTube probe recorder mutex poisoned")
            .push(YouTubeProbeAttempt {
                stage,
                client,
                result,
                status: None,
                error_category: error.map(|error| error.kind.diagnostic_category().as_str()),
                attempt: None,
                target: None,
                duration_ms: elapsed.as_millis() as u64,
            });
    }

    pub(super) fn record_transport(
        &self,
        client: &'static str,
        attempt: usize,
        target: &'static str,
        result: &'static str,
        status: &'static str,
        error_category: Option<&'static str>,
        elapsed: Duration,
    ) {
        self.attempts
            .lock()
            .expect("YouTube probe recorder mutex poisoned")
            .push(YouTubeProbeAttempt {
                stage: "media_probe_attempt",
                client,
                result,
                status: Some(status),
                error_category,
                attempt: Some(attempt.saturating_add(1) as u64),
                target: Some(target),
                duration_ms: elapsed.as_millis() as u64,
            });
    }

    pub(super) fn record_decoder_transport(
        &self,
        client: &'static str,
        attempt: u64,
        target: &'static str,
        result: &'static str,
        status: &'static str,
        error_category: Option<&'static str>,
        elapsed: Duration,
    ) {
        self.attempts
            .lock()
            .expect("YouTube probe recorder mutex poisoned")
            .push(YouTubeProbeAttempt::decoder_media(
                client,
                attempt,
                target,
                result,
                status,
                error_category,
                elapsed,
            ));
    }

    pub(super) fn finish(&self) -> Vec<YouTubeProbeAttempt> {
        self.attempts
            .lock()
            .expect("YouTube probe recorder mutex poisoned")
            .clone()
    }
}

fn player_client_stage(kind: &PlayerClientKind) -> &'static str {
    match kind {
        PlayerClientKind::AndroidVr => "youtube_player_android_vr",
        PlayerClientKind::VisionOs => "youtube_player_visionos",
        PlayerClientKind::Tv => "youtube_player_tv",
        PlayerClientKind::Web => "youtube_player_web",
        PlayerClientKind::WebRemix => "youtube_player_web_remix",
        #[cfg(feature = "private-capture")]
        PlayerClientKind::WebSafari => "youtube_player_web_safari",
    }
}

fn source_error_outcome(error: &AudioSourceError) -> crate::observability::OperationOutcome {
    if error.kind == AudioSourceErrorKind::Cancelled {
        crate::observability::OperationOutcome::Cancelled
    } else {
        crate::observability::OperationOutcome::Error
    }
}

fn source_selection_status(error: Option<&AudioSourceError>) -> &'static str {
    match error.map(|error| error.kind) {
        None => "native_available",
        Some(AudioSourceErrorKind::Decipher) => "decipher_required",
        Some(AudioSourceErrorKind::Cancelled) => "cancelled",
        Some(_) => "unavailable",
    }
}

fn source_route_status(
    selection_succeeded: bool,
    can_use_js_decipher: bool,
    can_use_browser_fallback: bool,
) -> &'static str {
    if selection_succeeded {
        "native"
    } else if can_use_js_decipher {
        "ejs"
    } else if can_use_browser_fallback {
        "browser_fallback"
    } else {
        "next_client"
    }
}

pub(super) fn automatic_player_client_enabled(
    client: &PlayerClient,
    po_tokens: &PoTokenMaterial,
) -> bool {
    !matches!(client.kind, PlayerClientKind::AndroidVr)
        || po_tokens.has_scoped_playback_token(client)
}

pub(super) fn automatic_player_clients(
    mut clients: Vec<PlayerClient>,
    po_tokens: &PoTokenMaterial,
    prefer_public_android_vr: bool,
) -> Vec<PlayerClient> {
    clients.retain(|client| automatic_player_client_enabled(client, po_tokens));
    if prefer_public_android_vr {
        if let Some(index) = clients
            .iter()
            .position(|client| matches!(client.kind, PlayerClientKind::AndroidVr))
        {
            let android_vr = clients.remove(index);
            let insert_index = clients
                .iter()
                .position(|client| matches!(client.kind, PlayerClientKind::VisionOs))
                .map_or(0, |index| index + 1)
                .min(clients.len());
            clients.insert(insert_index, android_vr);
        }
    }
    clients
}

const WARMUP_IDLE: u8 = 0;
const WARMUP_RUNNING: u8 = 1;
const WARMUP_COMPLETE: u8 = 2;
const WARMUP_TIMEOUT: Duration = Duration::from_secs(10);

#[derive(Clone)]
pub struct InnertubeAudioResolver {
    client: reqwest::Client,
    media_client: reqwest::Client,
    player_endpoint: Url,
    auth_type: YouTubeMusicAuthType,
    config_folder: PathBuf,
    cookie_path: PathBuf,
    oauth_path: PathBuf,
    po_token_path: Option<PathBuf>,
    tv_client_version: Arc<OnceLock<String>>,
    tv_signature_timestamp: Arc<OnceLock<Option<u32>>>,
    web_client_material: Arc<OnceLock<(String, PlayerVersionSource)>>,
    web_music_client_material: Arc<OnceLock<(String, PlayerVersionSource)>>,
    guest_visitor_data: Arc<OnceLock<String>>,
    prefer_public_android_vr: Arc<AtomicBool>,
    warmup_state: Arc<AtomicU8>,
    player_script_cache: Arc<tokio::sync::Mutex<HashMap<String, String>>>,
    ejs_solution_cache: Arc<tokio::sync::Mutex<HashMap<EjsSolutionCacheKey, String>>>,
    javascript_solver: Arc<super::super::javascript::Solver>,
    #[cfg(test)]
    test_response_headers_observer: Option<Arc<tokio::sync::Notify>>,
    #[cfg(test)]
    #[allow(dead_code)]
    test_inspection_clients: Option<Vec<PlayerClient>>,
    #[cfg(test)]
    test_playback_clients: Option<Vec<PlayerClient>>,
}

impl InnertubeAudioResolver {
    pub fn new(configs: &Configs, client: reqwest::Client) -> Self {
        Self {
            client,
            media_client: shared_media_http_client(),
            player_endpoint: Url::parse(PLAYER_ENDPOINT)
                .expect("static YouTube player endpoint is valid"),
            auth_type: configs.app_config.youtube.auth_type,
            config_folder: configs.config_folder.clone(),
            cookie_path: configs.youtube_music_cookie_path(),
            oauth_path: configs.youtube_music_oauth_path(),
            po_token_path: configs.app_config.youtube.po_token_file.clone(),
            tv_client_version: Arc::new(OnceLock::new()),
            tv_signature_timestamp: Arc::new(OnceLock::new()),
            web_client_material: Arc::new(OnceLock::new()),
            web_music_client_material: Arc::new(OnceLock::new()),
            guest_visitor_data: Arc::new(OnceLock::new()),
            prefer_public_android_vr: Arc::new(AtomicBool::new(false)),
            warmup_state: Arc::new(AtomicU8::new(WARMUP_IDLE)),
            player_script_cache: Arc::new(tokio::sync::Mutex::new(HashMap::new())),
            ejs_solution_cache: Arc::new(tokio::sync::Mutex::new(HashMap::new())),
            javascript_solver: Arc::new(super::super::javascript::Solver::configured_for(
                configs.app_config.youtube.javascript_runtime,
            )),
            #[cfg(test)]
            test_response_headers_observer: None,
            #[cfg(test)]
            test_inspection_clients: None,
            #[cfg(test)]
            test_playback_clients: None,
        }
    }

    fn cipher_engine(&self) -> CipherEngine<'_> {
        CipherEngine::new(
            &self.client,
            &self.player_script_cache,
            &self.ejs_solution_cache,
            &self.javascript_solver,
        )
    }

    async fn request_auth(&self) -> Result<RequestAuth, AudioSourceError> {
        load_request_auth(self.auth_type, &self.cookie_path, &self.oauth_path, None).await
    }

    /// Guest visitor data from the watch page of `video_id`, or from the home
    /// page when warming up before any video is known.
    async fn guest_visitor_data(
        &self,
        video_id: Option<&str>,
        cancellation: &CancellationToken,
    ) -> Result<Option<String>, AudioSourceError> {
        if let Some(visitor_data) = self.guest_visitor_data.get() {
            return Ok(Some(visitor_data.clone()));
        }
        if !matches!(
            self.player_endpoint.host_str(),
            Some("www.youtube.com" | "music.youtube.com")
        ) {
            return Ok(None);
        }

        let mut page_url = Url::parse(WEB_PAGE).expect("static YouTube page URL is valid");
        if let Some(video_id) = video_id {
            page_url.set_path("watch");
            page_url.query_pairs_mut().append_pair("v", video_id);
        }
        let response = tokio::select! {
            biased;
            () = cancellation.cancelled() => {
                return Err(AudioSourceError::new(
                    AudioSourceErrorKind::Cancelled,
                    "YouTube guest session bootstrap was cancelled",
                ));
            }
            response = self
                .client
                .get(page_url)
                .header(header::USER_AGENT, WEB_USER_AGENT)
                .header(header::ACCEPT_LANGUAGE, "en-US,en;q=0.9")
                .send() => response,
        };
        let Ok(response) = response.and_then(reqwest::Response::error_for_status) else {
            return Ok(None);
        };
        let page = tokio::select! {
            biased;
            () = cancellation.cancelled() => {
                return Err(AudioSourceError::new(
                    AudioSourceErrorKind::Cancelled,
                    "YouTube guest session bootstrap was cancelled",
                ));
            }
            page = response.text() => page.ok(),
        };
        let visitor_data = page.as_deref().and_then(parse_visitor_data);
        if let Some(visitor_data) = visitor_data {
            let _ = self.guest_visitor_data.set(visitor_data);
        }
        Ok(self.guest_visitor_data.get().cloned())
    }

    async fn tv_player_client(&self) -> PlayerClient {
        if let Some(version) = self.tv_client_version.get() {
            return PlayerClient::tv(version.clone(), PlayerVersionSource::Cached)
                .with_signature_timestamp(self.tv_signature_timestamp.get().copied().flatten());
        }
        let (version, signature_timestamp) = self.discover_tv_client_material().await;
        let (version, source) = match version {
            Some(version) => (version, PlayerVersionSource::Discovered),
            None => (fallback_tv_client_version(), PlayerVersionSource::Fallback),
        };
        let _ = self.tv_client_version.set(version.clone());
        let _ = self.tv_signature_timestamp.set(signature_timestamp);
        PlayerClient::tv(version, source).with_signature_timestamp(signature_timestamp)
    }

    async fn player_client(&self) -> PlayerClient {
        if self.auth_type != YouTubeMusicAuthType::Browser {
            return PlayerClient::android_vr();
        }
        self.tv_player_client().await
    }

    async fn probe_client_candidates(&self, client: YouTubeProbeClient) -> Vec<PlayerClient> {
        match client {
            YouTubeProbeClient::Auto => {
                self.player_client_candidates(PlayerClientPlan::Playback)
                    .await
            }
            YouTubeProbeClient::Tv => vec![self.tv_player_client().await],
            YouTubeProbeClient::Web => {
                let primary = self.tv_player_client().await;
                let (version, source) = self.web_client_material().await;
                vec![PlayerClient::web(version, source)
                    .with_signature_timestamp(primary.signature_timestamp)]
            }
            YouTubeProbeClient::WebRemix => {
                let primary = self.tv_player_client().await;
                let (version, source) = self.web_music_client_material().await;
                vec![PlayerClient::web_music(version, source)
                    .with_signature_timestamp(primary.signature_timestamp)]
            }
            YouTubeProbeClient::AndroidVr => vec![PlayerClient::android_vr()],
            YouTubeProbeClient::VisionOs => vec![PlayerClient::visionos()],
        }
    }

    async fn discover_tv_client_material(&self) -> (Option<String>, Option<u32>) {
        let Ok(response) = self
            .client
            .get(TV_PAGE)
            .header(header::USER_AGENT, TV_USER_AGENT)
            .send()
            .await
        else {
            return (None, None);
        };
        let Ok(response) = response.error_for_status() else {
            return (None, None);
        };
        let Ok(page) = response.text().await else {
            return (None, None);
        };
        (
            parse_tv_client_version(&page),
            parse_signature_timestamp(&page),
        )
    }

    async fn player_client_candidates(&self, plan: PlayerClientPlan) -> Vec<PlayerClient> {
        let include_safari = plan == PlayerClientPlan::Inspection;
        #[cfg(not(feature = "private-capture"))]
        let _ = include_safari;
        #[cfg(test)]
        match plan {
            PlayerClientPlan::Playback => {
                if let Some(clients) = &self.test_playback_clients {
                    return clients.clone();
                }
            }
            PlayerClientPlan::Inspection => {
                if let Some(clients) = &self.test_inspection_clients {
                    return clients.clone();
                }
            }
        }

        if self.auth_type != YouTubeMusicAuthType::Browser {
            return vec![PlayerClient::visionos(), self.player_client().await];
        }

        let discovery_started = Instant::now();
        let (primary, (web_version, web_source), (web_music_version, web_music_source)) = tokio::join!(
            self.player_client(),
            self.web_client_material(),
            self.web_music_client_material(),
        );
        crate::observability::operation_stage(
            crate::observability::Component::YoutubeMusic,
            "youtube_client_material",
            Some(discovery_started.elapsed()),
            Some(crate::observability::OperationOutcome::Success),
        );
        let signature_timestamp = primary.signature_timestamp;
        let web = PlayerClient::web(web_version, web_source)
            .with_signature_timestamp(signature_timestamp);
        let web_music = PlayerClient::web_music(web_music_version, web_music_source)
            .with_signature_timestamp(signature_timestamp);
        let mut clients = vec![PlayerClient::visionos(), primary];
        #[cfg(feature = "private-capture")]
        if include_safari {
            clients.push(PlayerClient::web_safari(
                web.version.clone(),
                web.version_source,
            ));
        }
        clients.extend([
            web,
            web_music,
            // Android VR is a public fallback. It must be sent without the
            // browser session because this client does not support account
            // cookies and must not inherit account-scoped proof material.
            PlayerClient::android_vr(),
        ]);
        clients
    }

    async fn web_client_material(&self) -> (String, PlayerVersionSource) {
        if let Some(material) = self.web_client_material.get() {
            return material.clone();
        }
        let material = self
            .discover_client_material(
                WEB_PAGE,
                WEB_DEFAULT_CLIENT_VERSION_PREFIX,
                fallback_web_client_version,
            )
            .await;
        let _ = self.web_client_material.set(material.clone());
        material
    }

    async fn web_music_client_material(&self) -> (String, PlayerVersionSource) {
        if let Some(material) = self.web_music_client_material.get() {
            return material.clone();
        }
        let material = self
            .discover_client_material(
                WEB_MUSIC_PAGE,
                WEB_MUSIC_DEFAULT_CLIENT_VERSION_PREFIX,
                fallback_web_music_client_version,
            )
            .await;
        let _ = self.web_music_client_material.set(material.clone());
        material
    }

    fn player_endpoint_for_client(&self, player_client: &PlayerClient) -> Url {
        let mut endpoint = self.player_endpoint.clone();
        if matches!(
            endpoint.host_str(),
            Some("www.youtube.com" | "music.youtube.com")
        ) {
            let _ = endpoint.set_host(Some(player_client.endpoint_host));
        }
        endpoint
    }

    async fn po_tokens(&self) -> PoTokenMaterial {
        load_optional_po_tokens(self.po_token_path.as_deref()).await
    }

    async fn warm_up_inner(
        &self,
        cancellation: &CancellationToken,
    ) -> Result<(), AudioSourceError> {
        // Client-material discovery is public and does not send account
        // cookies or proof tokens. Prefer the public Android route's player
        // script because it is the common cold path for native playback.
        let clients = self
            .player_client_candidates(PlayerClientPlan::Playback)
            .await;
        let player_client = clients
            .iter()
            .find(|client| matches!(client.kind, PlayerClientKind::AndroidVr))
            .or_else(|| clients.first())
            .cloned()
            .ok_or_else(|| {
                AudioSourceError::new(
                    AudioSourceErrorKind::Unavailable,
                    "YouTube returned no playback client for warm-up",
                )
            })?;
        let cipher = self.cipher_engine();
        let player_script_url = cipher
            .discover_player_script_url(&player_client, cancellation)
            .await
            .ok_or_else(|| {
                AudioSourceError::new(
                    AudioSourceErrorKind::Network,
                    "YouTube player script discovery did not complete",
                )
            })?;
        let player_script = cipher
            .fetch_player_script(&player_script_url, &player_client, cancellation)
            .await?;
        cipher
            .warm_up_javascript(&player_script, cancellation)
            .await
    }

    pub(super) async fn warm_up(
        &self,
        cancellation: &CancellationToken,
    ) -> Result<(), AudioSourceError> {
        if self
            .warmup_state
            .compare_exchange(
                WARMUP_IDLE,
                WARMUP_RUNNING,
                Ordering::AcqRel,
                Ordering::Acquire,
            )
            .is_err()
        {
            return Ok(());
        }

        let result = tokio::time::timeout(WARMUP_TIMEOUT, self.warm_up_inner(cancellation))
            .await
            .map_err(|_| {
                AudioSourceError::new(
                    AudioSourceErrorKind::Network,
                    "YouTube playback warm-up timed out",
                )
            })
            .and_then(std::convert::identity);
        self.warmup_state.store(
            if result.is_ok() {
                WARMUP_COMPLETE
            } else {
                WARMUP_IDLE
            },
            Ordering::Release,
        );
        result
    }

    // Every playback-facing path uses this boundary for client scope, proof
    // token selection, capture timing, and the player request itself.
    #[allow(clippy::too_many_arguments)]
    async fn attempt_player_client(
        &self,
        video_id: &str,
        auth: &RequestAuth,
        po_tokens: &PoTokenMaterial,
        player_client: PlayerClient,
        quality: YouTubePlaybackQuality,
        cancellation: &CancellationToken,
        auth_started: Instant,
        probe_recorder: Option<&YouTubeProbeRecorder>,
        #[cfg(feature = "private-capture")] capture: Option<
            &crate::developer_capture::CaptureSession,
        >,
        #[cfg(feature = "private-capture")] exchange_ref: crate::developer_capture::ExchangeRef,
    ) -> PlayerClientAttempt {
        #[cfg(not(feature = "private-capture"))]
        let _ = (quality, auth_started);
        let request_auth = player_client.auth_for_request(auth);
        let client_po_token = player_client.player_po_token(po_tokens.player_for(&player_client));
        let auth_scope = player_client.auth_scope(auth);
        let request_started = Instant::now();
        #[cfg(feature = "private-capture")]
        if let Some(capture) = capture {
            capture_auth_selection(
                capture,
                exchange_ref,
                &request_auth,
                &player_client,
                client_po_token.is_some(),
                quality,
                auth_started.elapsed(),
            );
        }
        #[cfg(feature = "private-capture")]
        let proof_token_present = client_po_token.is_some();
        let visitor_data = if matches!(
            player_client.kind,
            PlayerClientKind::AndroidVr | PlayerClientKind::VisionOs
        ) && auth_scope == super::format::PlayerAttemptAuthScope::Public
        {
            match self.guest_visitor_data(Some(video_id), cancellation).await {
                Ok(visitor_data) => visitor_data,
                Err(error) => {
                    let outcome = if error.kind == AudioSourceErrorKind::Cancelled {
                        crate::observability::OperationOutcome::Cancelled
                    } else {
                        crate::observability::OperationOutcome::Error
                    };
                    crate::observability::operation_stage(
                        crate::observability::Component::YoutubeMusic,
                        player_client_stage(&player_client.kind),
                        Some(request_started.elapsed()),
                        Some(outcome),
                    );
                    if let Some(recorder) = probe_recorder {
                        recorder.record(
                            "player",
                            player_client.source_name,
                            if error.kind == AudioSourceErrorKind::Cancelled {
                                "cancelled"
                            } else {
                                "error"
                            },
                            Some(&error),
                            request_started.elapsed(),
                        );
                    }
                    return PlayerClientAttempt {
                        client: player_client,
                        auth_scope,
                        #[cfg(feature = "private-capture")]
                        proof_token_present,
                        response: Err(error),
                    };
                }
            }
        } else {
            None
        };
        let player_endpoint = self.player_endpoint_for_client(&player_client);
        let response = super::player::request_player_response(
            &self.client,
            player_endpoint,
            #[cfg(test)]
            self.test_response_headers_observer.as_ref(),
            video_id,
            &request_auth,
            &player_client,
            client_po_token,
            visitor_data.as_deref(),
            cancellation,
            #[cfg(feature = "private-capture")]
            capture,
            #[cfg(feature = "private-capture")]
            exchange_ref,
        )
        .await;
        let outcome = match &response {
            Ok(_) => crate::observability::OperationOutcome::Success,
            Err(error) if error.kind == AudioSourceErrorKind::Cancelled => {
                crate::observability::OperationOutcome::Cancelled
            }
            Err(_) => crate::observability::OperationOutcome::Error,
        };
        crate::observability::operation_stage(
            crate::observability::Component::YoutubeMusic,
            player_client_stage(&player_client.kind),
            Some(request_started.elapsed()),
            Some(outcome),
        );
        if let Some(recorder) = probe_recorder {
            let error = response.as_ref().err();
            recorder.record(
                "player",
                player_client.source_name,
                match error.map(|error| error.kind) {
                    None => "success",
                    Some(AudioSourceErrorKind::Cancelled) => "cancelled",
                    Some(_) => "error",
                },
                error,
                request_started.elapsed(),
            );
        }
        PlayerClientAttempt {
            client: player_client,
            auth_scope,
            #[cfg(feature = "private-capture")]
            proof_token_present,
            response,
        }
    }

    #[cfg(feature = "private-capture")]
    pub(crate) async fn inspect_player_response(
        &self,
        video_id: &str,
        quality: YouTubePlaybackQuality,
        cancellation: CancellationToken,
    ) -> Result<YouTubePlaybackInspection, AudioSourceError> {
        let clients = self
            .player_client_candidates(PlayerClientPlan::Inspection)
            .await;
        let auth = match self.request_auth().await {
            Ok(auth) => auth,
            Err(error) if error.kind != AudioSourceErrorKind::Cancelled => {
                return Ok(inspect_player_auth_failure(
                    video_id,
                    self.auth_type,
                    &clients,
                    &error,
                ));
            }
            Err(error) => return Err(error),
        };
        let po_tokens = self.po_tokens().await;
        let mut client_matrix = Vec::with_capacity(clients.len());

        for player_client in clients {
            let attempt = self
                .attempt_player_client(
                    video_id,
                    &auth,
                    &po_tokens,
                    player_client,
                    quality,
                    &cancellation,
                    Instant::now(),
                    None,
                    #[cfg(feature = "private-capture")]
                    None,
                    #[cfg(feature = "private-capture")]
                    crate::developer_capture::ExchangeRef::from_bytes(rand::random()),
                )
                .await;
            let PlayerClientAttempt {
                client: player_client,
                auth_scope,
                proof_token_present,
                response,
            } = attempt;
            match response {
                Ok(response) => client_matrix.push(inspect_player_attempt(
                    &player_client,
                    auth_scope,
                    proof_token_present,
                    &response,
                    quality,
                )),
                Err(error) if error.kind == AudioSourceErrorKind::Cancelled => {
                    return Err(error);
                }
                Err(error) => client_matrix.push(inspect_player_attempt_error(
                    &player_client,
                    auth_scope,
                    proof_token_present,
                    &error,
                )),
            }
        }

        let primary = client_matrix
            .first()
            .cloned()
            .expect("YouTube client matrix always includes the primary client");
        Ok(YouTubePlaybackInspection {
            video_id: bounded_inspection_text(video_id),
            auth_kind: request_auth_kind(&auth),
            client: primary.client.clone(),
            proof_token_present: primary.proof_token_present,
            signature_timestamp_present: primary.signature_timestamp_present,
            http_status: primary.http_status,
            playability_status: primary.playability_status.clone(),
            playability_reason: primary.playability_reason.clone(),
            streaming_data_present: primary.streaming_data_present,
            adaptive_format_count: primary.adaptive_format_count,
            direct_audio_count: primary.direct_audio_count,
            cipher_audio_count: primary.cipher_audio_count,
            selected_audio_itag: primary.selected_audio_itag,
            selection: primary.selection.clone(),
            client_matrix,
        })
    }

    async fn discover_client_material(
        &self,
        page_url: &str,
        fallback_prefix: &str,
        fallback: fn() -> String,
    ) -> (String, PlayerVersionSource) {
        let version = match self
            .client
            .get(page_url)
            .header(header::USER_AGENT, WEB_USER_AGENT)
            .send()
            .await
            .ok()
            .and_then(|response| response.error_for_status().ok())
        {
            Some(response) => response
                .text()
                .await
                .ok()
                .and_then(|page| parse_innertube_client_version(&page, fallback_prefix)),
            None => None,
        };
        version.map_or_else(
            || (fallback(), PlayerVersionSource::Fallback),
            |version| (version, PlayerVersionSource::Discovered),
        )
    }

    async fn resolve_browser_session_source(
        &self,
        video_id: &str,
        target: BrowserAudioTarget,
        cancellation: &CancellationToken,
        close_browser_after_capture: bool,
        #[cfg(feature = "private-capture")] capture: Option<
            &crate::developer_capture::CaptureSession,
        >,
        #[cfg(feature = "private-capture")] exchange_ref: crate::developer_capture::ExchangeRef,
    ) -> Result<ResolvedAudioSource, AudioSourceError> {
        #[cfg(feature = "private-capture")]
        let browser_result = match capture {
            Some(capture) => {
                super::super::browser_auth::capture_playback_url_with_private_evidence(
                    &self.config_folder,
                    video_id,
                    target.itag,
                    target.content_length,
                    cancellation,
                    close_browser_after_capture,
                    capture.clone(),
                    exchange_ref,
                )
                .await
            }
            None => {
                super::super::browser_auth::capture_playback_url(
                    &self.config_folder,
                    video_id,
                    target.itag,
                    target.content_length,
                    cancellation,
                    close_browser_after_capture,
                )
                .await
            }
        };
        #[cfg(not(feature = "private-capture"))]
        let browser_result = super::super::browser_auth::capture_playback_url(
            &self.config_folder,
            video_id,
            target.itag,
            target.content_length,
            cancellation,
            close_browser_after_capture,
        )
        .await;
        let url = browser_result.map_err(|_| {
            AudioSourceError::new(
                AudioSourceErrorKind::Authentication,
                "the dedicated signed-in browser did not produce a playable audio request",
            )
        })?;
        let (captured_itag, mime_type, content_length) = browser_source_metadata(&url, &target);
        #[cfg(not(feature = "private-capture"))]
        let _ = captured_itag;
        let expires_at_unix = url
            .query_pairs()
            .find(|(key, _)| key == "expire")
            .and_then(|(_, value)| value.parse::<u64>().ok())
            .map(|expiry| expiry.saturating_sub(60));
        Ok(ResolvedAudioSource {
            media_id: video_id.to_string(),
            #[cfg(feature = "private-capture")]
            itag: captured_itag,
            url,
            required_headers: header::HeaderMap::new(),
            mime_type,
            bitrate: target.bitrate,
            content_length,
            duration: target.duration,
            expires_at_unix,
            source_client: BROWSER_SESSION_SOURCE_NAME,
        })
    }

    pub(crate) async fn diagnose_browser_media_transport(
        &self,
        video_id: &str,
        quality: YouTubePlaybackQuality,
        cancellation: CancellationToken,
    ) -> Result<MediaTransportDiagnostic, AudioSourceError> {
        let auth = self.request_auth().await?;
        let po_tokens = self.po_tokens().await;
        let mut selected = None;
        let mut last_error = None;
        for player_client in self
            .player_client_candidates(PlayerClientPlan::Playback)
            .await
        {
            let attempt = self
                .attempt_player_client(
                    video_id,
                    &auth,
                    &po_tokens,
                    player_client,
                    quality,
                    &cancellation,
                    Instant::now(),
                    None,
                    #[cfg(feature = "private-capture")]
                    None,
                    #[cfg(feature = "private-capture")]
                    crate::developer_capture::ExchangeRef::from_bytes(rand::random()),
                )
                .await;
            let PlayerClientAttempt {
                client: player_client,
                auth_scope,
                response,
                ..
            } = attempt;
            let response = match response {
                Ok(response) => response,
                Err(error) if should_try_next_player_client(&error) => {
                    last_error = Some(error);
                    continue;
                }
                Err(error) => return Err(error),
            };
            let formats = match playable_formats_for_attempt(response, auth_scope) {
                Ok(formats) => formats,
                Err(error) if should_try_next_player_client(&error) => {
                    last_error = Some(error);
                    continue;
                }
                Err(error) => return Err(error),
            };
            let player_required_browser =
                match select_audio_format(formats.clone(), quality, player_client.source_name) {
                    Err(err) if should_use_browser_session_transport(&auth, &err) => true,
                    Err(err) => {
                        last_error = Some(err);
                        continue;
                    }
                    Ok(_) => false,
                };
            let target = match select_browser_audio_target(&formats, quality) {
                Ok(target) => target,
                Err(error) if should_try_next_player_client(&error) => {
                    last_error = Some(error);
                    continue;
                }
                Err(error) => return Err(error),
            };
            selected = Some((player_client, formats, player_required_browser, target));
            break;
        }
        let (_, _, player_required_browser, target) = selected.ok_or_else(|| {
            last_error.unwrap_or_else(|| {
                AudioSourceError::new(
                    AudioSourceErrorKind::Unavailable,
                    "YouTube returned no usable playback client for transport diagnostics",
                )
            })
        })?;
        diagnose_browser_media_target(
            &self.config_folder,
            video_id,
            target,
            player_required_browser,
            &cancellation,
        )
        .await
    }

    async fn verify_media_continuation(
        &self,
        source: &mut ResolvedAudioSource,
        cancellation: &CancellationToken,
        probe_recorder: Option<&YouTubeProbeRecorder>,
        #[cfg(feature = "private-capture")] capture: Option<
            &crate::developer_capture::CaptureSession,
        >,
        #[cfg(feature = "private-capture")] exchange_ref: crate::developer_capture::ExchangeRef,
    ) -> Result<(), AudioSourceError> {
        let started = Instant::now();
        let result = verify_media_continuation(
            &self.media_client,
            source,
            cancellation,
            probe_recorder,
            #[cfg(feature = "private-capture")]
            capture,
            #[cfg(feature = "private-capture")]
            exchange_ref,
        )
        .await;
        let outcome = match &result {
            Ok(()) => crate::observability::OperationOutcome::Success,
            Err(error) if error.kind == AudioSourceErrorKind::Cancelled => {
                crate::observability::OperationOutcome::Cancelled
            }
            Err(_) => crate::observability::OperationOutcome::Error,
        };
        crate::observability::operation_stage(
            crate::observability::Component::YoutubeMusic,
            "youtube_media_probe",
            Some(started.elapsed()),
            Some(outcome),
        );
        if let Some(recorder) = probe_recorder {
            let error = result.as_ref().err();
            recorder.record(
                "media_probe",
                source.source_client,
                match error.map(|error| error.kind) {
                    None => "success",
                    Some(AudioSourceErrorKind::Cancelled) => "cancelled",
                    Some(_) => "error",
                },
                error,
                started.elapsed(),
            );
        }
        result
    }

    #[allow(clippy::too_many_arguments)]
    async fn try_public_android_vr_source(
        &self,
        video_id: &str,
        quality: YouTubePlaybackQuality,
        auth: &RequestAuth,
        po_tokens: &PoTokenMaterial,
        cancellation: &CancellationToken,
        auth_started: Instant,
        probe_recorder: Option<&YouTubeProbeRecorder>,
        #[cfg(feature = "private-capture")] capture: Option<
            &crate::developer_capture::CaptureSession,
        >,
        #[cfg(feature = "private-capture")] exchange_ref: crate::developer_capture::ExchangeRef,
    ) -> Result<Option<ResolvedAudioSource>, AudioSourceError> {
        let player_client = PlayerClient::android_vr();
        if !po_tokens.has_scoped_playback_token(&player_client) {
            tracing::debug!(
                "Skipping automatic Android VR playback without a client-scoped PO token"
            );
            return Ok(None);
        }
        let attempt = self
            .attempt_player_client(
                video_id,
                auth,
                po_tokens,
                player_client,
                quality,
                cancellation,
                auth_started,
                probe_recorder,
                #[cfg(feature = "private-capture")]
                capture,
                #[cfg(feature = "private-capture")]
                exchange_ref,
            )
            .await;
        let PlayerClientAttempt {
            client: player_client,
            auth_scope,
            response,
            ..
        } = attempt;
        let response = match response {
            Ok(response) => response,
            Err(error) if error.kind == AudioSourceErrorKind::Cancelled => return Err(error),
            Err(_) => return Ok(None),
        };
        #[cfg(feature = "private-capture")]
        capture_format_inventory(
            capture,
            exchange_ref,
            &player_client,
            response
                .streaming_data
                .as_ref()
                .map_or(&[][..], |streaming| streaming.adaptive_formats.as_slice()),
        );
        let formats = match playable_formats_for_attempt(response, auth_scope) {
            Ok(formats) => formats,
            Err(error) if error.kind == AudioSourceErrorKind::Cancelled => return Err(error),
            Err(_) => return Ok(None),
        };
        #[cfg(feature = "private-capture")]
        let selection_started = Instant::now();
        let mut source = match select_audio_format(formats, quality, player_client.source_name) {
            Ok(source) => source,
            Err(error) if error.kind == AudioSourceErrorKind::Cancelled => return Err(error),
            Err(_) => return Ok(None),
        };
        apply_gvs_po_token(&mut source.url, po_tokens.gvs_for(&player_client));
        #[cfg(feature = "private-capture")]
        capture_selection_decision(
            capture,
            exchange_ref,
            &player_client,
            quality,
            Some(source.itag),
            "native_selected",
            true,
            true,
            selection_started.elapsed(),
        );
        prepare_resolved_source(&mut source, video_id);
        match self
            .verify_media_continuation(
                &mut source,
                cancellation,
                probe_recorder,
                #[cfg(feature = "private-capture")]
                capture,
                #[cfg(feature = "private-capture")]
                exchange_ref,
            )
            .await
        {
            Ok(()) => Ok(Some(source)),
            Err(error) if error.kind == AudioSourceErrorKind::Cancelled => Err(error),
            Err(_) => Ok(None),
        }
    }

    async fn resolve_source(
        &self,
        video_id: &str,
        quality: YouTubePlaybackQuality,
        cancellation: CancellationToken,
        allow_browser_fallback: bool,
        browser_fallback_attempted: Option<&AtomicBool>,
        close_browser_after_capture: bool,
        probe_client: YouTubeProbeClient,
        probe_recorder: Option<&YouTubeProbeRecorder>,
        #[cfg(feature = "private-capture")] capture: Option<
            &crate::developer_capture::CaptureSession,
        >,
    ) -> Result<ResolvedAudioSource, AudioSourceError> {
        self.javascript_solver.reset_last_backend();
        #[cfg(feature = "private-capture")]
        let exchange_ref = crate::developer_capture::ExchangeRef::from_bytes(rand::random());
        let auth_started = std::time::Instant::now();
        let public_auth = RequestAuth::None;
        let mut configured_auth = None;
        let po_tokens = self.po_tokens().await;
        let mut selected_client = None;
        let mut selected_formats = None;
        let mut selected_player_script_url = None;
        let mut selected_client_index = None;
        let mut last_client_error = None;
        let mut player_clients = self.probe_client_candidates(probe_client).await;
        if probe_client == YouTubeProbeClient::Auto {
            player_clients = automatic_player_clients(
                player_clients,
                &po_tokens,
                self.prefer_public_android_vr.load(Ordering::Acquire),
            );
        }
        for (selection_index, player_client) in player_clients.into_iter().enumerate() {
            let attempt_auth = if player_client.requires_configured_auth(self.auth_type) {
                if configured_auth.is_none() {
                    #[cfg(feature = "private-capture")]
                    let auth_load_started = Instant::now();
                    configured_auth = Some(match self.request_auth().await {
                        Ok(auth) => auth,
                        Err(error) => {
                            #[cfg(feature = "private-capture")]
                            capture_auth_failure(
                                capture,
                                exchange_ref,
                                quality,
                                error.kind.diagnostic_category().as_str(),
                                auth_load_started.elapsed(),
                            );
                            #[cfg(feature = "private-capture")]
                            capture_network_failure(
                                capture,
                                Some(exchange_ref),
                                "auth_selection",
                                error.kind.diagnostic_category().as_str(),
                                None,
                            );
                            return Err(error);
                        }
                    });
                }
                configured_auth
                    .as_ref()
                    .expect("configured YouTube auth was loaded")
            } else {
                &public_auth
            };
            let attempt = self
                .attempt_player_client(
                    video_id,
                    attempt_auth,
                    &po_tokens,
                    player_client,
                    quality,
                    &cancellation,
                    auth_started,
                    probe_recorder,
                    #[cfg(feature = "private-capture")]
                    capture,
                    #[cfg(feature = "private-capture")]
                    exchange_ref,
                )
                .await;
            let PlayerClientAttempt {
                client: player_client,
                auth_scope,
                response,
                ..
            } = attempt;
            let response = match response {
                Ok(response) => response,
                Err(error) if should_try_next_player_client(&error) => {
                    last_client_error = Some(error);
                    continue;
                }
                Err(error) => return Err(error),
            };
            let mut player_script_url = response
                .assets
                .as_ref()
                .and_then(|assets| assets.js.as_deref())
                .and_then(parse_player_script_url);
            #[cfg(feature = "private-capture")]
            capture_format_inventory(
                capture,
                exchange_ref,
                &player_client,
                response
                    .streaming_data
                    .as_ref()
                    .map_or(&[][..], |streaming| streaming.adaptive_formats.as_slice()),
            );
            let candidate_formats = match playable_formats_for_attempt(response, auth_scope) {
                Ok(formats) => formats,
                Err(error) if should_try_next_player_client(&error) => {
                    last_client_error = Some(error);
                    continue;
                }
                Err(error) => return Err(error),
            };
            if player_script_url.is_none()
                && candidate_formats.iter().any(cipher_contains_media_url)
            {
                player_script_url = self
                    .cipher_engine()
                    .discover_player_script_url(&player_client, &cancellation)
                    .await;
            }
            let selection_started = Instant::now();
            let selection = select_audio_format(
                candidate_formats.clone(),
                quality,
                player_client.source_name,
            );
            let selection_error = selection.as_ref().err();
            if let Some(recorder) = probe_recorder {
                recorder.record(
                    "selection",
                    player_client.source_name,
                    match selection_error.map(|error| error.kind) {
                        None => "success",
                        Some(AudioSourceErrorKind::Cancelled) => "cancelled",
                        Some(_) => "error",
                    },
                    selection_error,
                    selection_started.elapsed(),
                );
            }
            let selection_outcome = selection_error.map_or(
                Some(crate::observability::OperationOutcome::Success),
                |error| {
                    (error.kind == AudioSourceErrorKind::Cancelled)
                        .then_some(crate::observability::OperationOutcome::Cancelled)
                },
            );
            crate::observability::operation_stage_detail(
                crate::observability::Component::YoutubeMusic,
                "youtube_source_selection",
                Some(selection_started.elapsed()),
                selection_outcome,
                Some(player_client_stage(&player_client.kind)),
                Some(source_selection_status(selection_error)),
                selection_error.map(|error| error.kind.diagnostic_category().as_str()),
                Some(selection_index as u64),
            );
            let can_use_browser_fallback = allow_browser_fallback
                && selection.as_ref().is_err_and(|error| {
                    should_use_browser_session_transport_for_client(
                        attempt_auth,
                        &player_client,
                        error,
                    )
                })
                && select_browser_audio_target(&candidate_formats, quality).is_ok();
            let can_use_js_decipher = selection
                .as_ref()
                .is_err_and(|error| error.kind == AudioSourceErrorKind::Decipher)
                && player_script_url.is_some();
            let route_status = source_route_status(
                selection.is_ok(),
                can_use_js_decipher,
                can_use_browser_fallback,
            );
            let route_outcome = if selection_error
                .is_some_and(|error| error.kind == AudioSourceErrorKind::Cancelled)
            {
                Some(crate::observability::OperationOutcome::Cancelled)
            } else if route_status == "next_client" {
                None
            } else {
                Some(crate::observability::OperationOutcome::Success)
            };
            crate::observability::operation_stage_detail(
                crate::observability::Component::YoutubeMusic,
                "youtube_source_route",
                Some(selection_started.elapsed()),
                route_outcome,
                Some(player_client_stage(&player_client.kind)),
                Some(route_status),
                selection_error.map(|error| error.kind.diagnostic_category().as_str()),
                Some(selection_index as u64),
            );
            if selection.is_ok() || can_use_js_decipher || can_use_browser_fallback {
                selected_client = Some(player_client);
                selected_formats = Some(candidate_formats);
                selected_player_script_url = player_script_url;
                selected_client_index = Some(selection_index as u64);
                break;
            }
            let error = selection.expect_err("a non-selected YouTube client has an error");
            if should_try_next_player_client(&error) {
                last_client_error = Some(error);
                continue;
            }
            return Err(error);
        }
        let Some((player_client, formats)) = selected_client.zip(selected_formats) else {
            return Err(last_client_error.unwrap_or_else(|| {
                AudioSourceError::new(
                    AudioSourceErrorKind::Unavailable,
                    "YouTube returned no usable playback client",
                )
            }));
        };
        let auth = configured_auth.unwrap_or(RequestAuth::None);
        let player_script_url = selected_player_script_url;
        let selected_client_index = selected_client_index.unwrap_or_default();
        #[cfg(feature = "private-capture")]
        let selection_started = std::time::Instant::now();
        let browser_target = select_browser_audio_target(&formats, quality);
        match select_audio_format(formats.clone(), quality, player_client.source_name) {
            Ok(mut source) => {
                apply_gvs_po_token(&mut source.url, po_tokens.gvs_for(&player_client));
                #[cfg(feature = "private-capture")]
                capture_selection_decision(
                    capture,
                    exchange_ref,
                    &player_client,
                    quality,
                    Some(source.itag),
                    "native_selected",
                    false,
                    false,
                    selection_started.elapsed(),
                );
                prepare_resolved_source(&mut source, video_id);
                match self
                    .verify_media_continuation(
                        &mut source,
                        &cancellation,
                        probe_recorder,
                        #[cfg(feature = "private-capture")]
                        capture,
                        #[cfg(feature = "private-capture")]
                        exchange_ref,
                    )
                    .await
                {
                    Ok(()) => {
                        if matches!(&player_client.kind, PlayerClientKind::AndroidVr) {
                            self.prefer_public_android_vr.store(true, Ordering::Release);
                        }
                        return Ok(source);
                    }
                    Err(error)
                        if should_retry_browser_session_transport_for_client(
                            &auth,
                            &player_client,
                            &error,
                        ) =>
                    {
                        if probe_client == YouTubeProbeClient::Auto {
                            tracing::info!(
                                "YouTube rejected the native media URL; retrying through the public Android VR client"
                            );
                            if let Some(source) = self
                                .try_public_android_vr_source(
                                    video_id,
                                    quality,
                                    &auth,
                                    &po_tokens,
                                    &cancellation,
                                    auth_started,
                                    probe_recorder,
                                    #[cfg(feature = "private-capture")]
                                    capture,
                                    #[cfg(feature = "private-capture")]
                                    exchange_ref,
                                )
                                .await?
                            {
                                self.prefer_public_android_vr.store(true, Ordering::Release);
                                return Ok(source);
                            }
                        }
                        if !allow_browser_fallback {
                            return Err(error);
                        }
                        tracing::info!(
                            "Public Android VR playback was unavailable; retrying once through the dedicated browser session"
                        );
                    }
                    Err(error) => return Err(error),
                }
            }
            Err(err) => {
                if err.kind == AudioSourceErrorKind::Decipher {
                    if let Some(player_script_url) = player_script_url.as_ref() {
                        let ejs_started = Instant::now();
                        crate::observability::operation_stage_detail(
                            crate::observability::Component::YoutubeMusic,
                            "youtube_ejs_decipher",
                            Some(Duration::ZERO),
                            None,
                            Some(player_client_stage(&player_client.kind)),
                            Some("started"),
                            None,
                            Some(selected_client_index),
                        );
                        let ejs_result = self
                            .cipher_engine()
                            .resolve_ciphered_audio_format(
                                &formats,
                                quality,
                                &player_client,
                                player_script_url,
                                &cancellation,
                            )
                            .await;
                        let ejs_error = ejs_result.as_ref().err();
                        if let Some(recorder) = probe_recorder {
                            recorder.record(
                                "ejs",
                                player_client.source_name,
                                match ejs_error.map(|error| error.kind) {
                                    None => "success",
                                    Some(AudioSourceErrorKind::Cancelled) => "cancelled",
                                    Some(_) => "error",
                                },
                                ejs_error,
                                ejs_started.elapsed(),
                            );
                        }
                        crate::observability::operation_stage_detail(
                            crate::observability::Component::YoutubeMusic,
                            "youtube_ejs_decipher",
                            Some(ejs_started.elapsed()),
                            Some(ejs_error.map_or(
                                crate::observability::OperationOutcome::Success,
                                source_error_outcome,
                            )),
                            Some(player_client_stage(&player_client.kind)),
                            Some(if ejs_error.is_some() {
                                "failed"
                            } else {
                                "resolved"
                            }),
                            ejs_error.map(|error| error.kind.diagnostic_category().as_str()),
                            Some(selected_client_index),
                        );
                        let ejs_backend =
                            match self.javascript_solver.last_backend_diagnostic_label() {
                                "not_recorded" => {
                                    self.javascript_solver.configured_backend_diagnostic_label()
                                }
                                backend => backend,
                            };
                        crate::observability::operation_stage_detail(
                            crate::observability::Component::YoutubeMusic,
                            "youtube_ejs_runtime",
                            Some(ejs_started.elapsed()),
                            Some(ejs_error.map_or(
                                crate::observability::OperationOutcome::Success,
                                source_error_outcome,
                            )),
                            Some(ejs_backend),
                            Some(super::super::javascript::EJS_BUNDLE_PROVENANCE),
                            ejs_error.map(|error| error.kind.diagnostic_category().as_str()),
                            Some(selected_client_index),
                        );
                        match ejs_result {
                            Ok(mut source) => {
                                apply_gvs_po_token(
                                    &mut source.url,
                                    po_tokens.gvs_for(&player_client),
                                );
                                #[cfg(feature = "private-capture")]
                                capture_selection_decision(
                                    capture,
                                    exchange_ref,
                                    &player_client,
                                    quality,
                                    Some(source.itag),
                                    "native_js_deciphered",
                                    false,
                                    false,
                                    selection_started.elapsed(),
                                );
                                prepare_resolved_source(&mut source, video_id);
                                match self
                                    .verify_media_continuation(
                                        &mut source,
                                        &cancellation,
                                        probe_recorder,
                                        #[cfg(feature = "private-capture")]
                                        capture,
                                        #[cfg(feature = "private-capture")]
                                        exchange_ref,
                                    )
                                    .await
                                {
                                    Ok(()) => {
                                        if matches!(
                                            &player_client.kind,
                                            PlayerClientKind::AndroidVr
                                        ) {
                                            self.prefer_public_android_vr
                                                .store(true, Ordering::Release);
                                        }
                                        return Ok(source);
                                    }
                                    Err(error)
                                        if should_retry_browser_session_transport_for_client(
                                            &auth,
                                            &player_client,
                                            &error,
                                        ) =>
                                    {
                                        if probe_client == YouTubeProbeClient::Auto {
                                            tracing::info!(
                                                "YouTube rejected the JavaScript-deciphered media URL; retrying through the public Android VR client"
                                            );
                                            if let Some(source) = self
                                                .try_public_android_vr_source(
                                                    video_id,
                                                    quality,
                                                    &auth,
                                                    &po_tokens,
                                                    &cancellation,
                                                    auth_started,
                                                    probe_recorder,
                                                    #[cfg(feature = "private-capture")]
                                                    capture,
                                                    #[cfg(feature = "private-capture")]
                                                    exchange_ref,
                                                )
                                                .await?
                                            {
                                                self.prefer_public_android_vr
                                                    .store(true, Ordering::Release);
                                                return Ok(source);
                                            }
                                        }
                                        if !allow_browser_fallback {
                                            return Err(error);
                                        }
                                        tracing::info!(
                                            "Public Android VR playback was unavailable; retrying once through the dedicated browser session"
                                        );
                                    }
                                    Err(error) => return Err(error),
                                }
                            }
                            Err(error) => {
                                let _ = error;
                                tracing::debug!(
                                    "YouTube EJS challenge solving did not produce a media URL"
                                );
                                if !should_use_browser_session_transport_for_client(
                                    &auth,
                                    &player_client,
                                    &err,
                                ) {
                                    return Err(err);
                                }
                            }
                        }
                    } else {
                        crate::observability::operation_stage_detail(
                            crate::observability::Component::YoutubeMusic,
                            "youtube_ejs_decipher",
                            Some(Duration::ZERO),
                            None,
                            Some(player_client_stage(&player_client.kind)),
                            Some("skipped_no_script"),
                            Some(err.kind.diagnostic_category().as_str()),
                            Some(selected_client_index),
                        );
                    }
                }
                if allow_browser_fallback
                    && should_use_browser_session_transport_for_client(&auth, &player_client, &err)
                {
                    crate::observability::operation_stage_detail(
                        crate::observability::Component::YoutubeMusic,
                        "youtube_source_route",
                        Some(Duration::ZERO),
                        Some(crate::observability::OperationOutcome::Success),
                        Some(player_client_stage(&player_client.kind)),
                        Some("browser_fallback_selected"),
                        Some(err.kind.diagnostic_category().as_str()),
                        Some(selected_client_index),
                    );
                    #[cfg(feature = "private-capture")]
                    capture_selection_decision(
                        capture,
                        exchange_ref,
                        &player_client,
                        quality,
                        None,
                        err.kind.diagnostic_category().as_str(),
                        true,
                        true,
                        selection_started.elapsed(),
                    );
                    tracing::info!(
                        "Authenticated YouTube player returned cipher-only audio; authorizing the same format through the dedicated browser session"
                    );
                } else {
                    #[cfg(feature = "private-capture")]
                    capture_selection_decision(
                        capture,
                        exchange_ref,
                        &player_client,
                        quality,
                        None,
                        err.kind.diagnostic_category().as_str(),
                        false,
                        false,
                        selection_started.elapsed(),
                    );
                    return Err(err);
                }
            }
        }

        let target = match browser_target {
            Ok(target) => {
                crate::observability::operation_stage_detail(
                    crate::observability::Component::YoutubeMusic,
                    "youtube_browser_target",
                    Some(Duration::ZERO),
                    Some(crate::observability::OperationOutcome::Success),
                    Some(player_client_stage(&player_client.kind)),
                    Some("selected"),
                    None,
                    Some(selected_client_index),
                );
                target
            }
            Err(target_error) => {
                crate::observability::operation_stage_detail(
                    crate::observability::Component::YoutubeMusic,
                    "youtube_browser_target",
                    Some(Duration::ZERO),
                    Some(source_error_outcome(&target_error)),
                    Some(player_client_stage(&player_client.kind)),
                    Some("unavailable"),
                    Some(target_error.kind.diagnostic_category().as_str()),
                    Some(selected_client_index),
                );
                #[cfg(feature = "private-capture")]
                capture_selection_decision(
                    capture,
                    exchange_ref,
                    &player_client,
                    quality,
                    None,
                    target_error.kind.diagnostic_category().as_str(),
                    true,
                    true,
                    selection_started.elapsed(),
                );
                return Err(AudioSourceError::new(
                    target_error.kind,
                    "the authenticated account authorized this media, but no browser-playable target was available",
                ));
            }
        };
        #[cfg(feature = "private-capture")]
        let itag = target.itag;
        if let Some(attempted) = browser_fallback_attempted {
            attempted.store(true, Ordering::Release);
        }
        let mut source = match self
            .resolve_browser_session_source(
                video_id,
                target,
                &cancellation,
                close_browser_after_capture,
                #[cfg(feature = "private-capture")]
                capture,
                #[cfg(feature = "private-capture")]
                exchange_ref,
            )
            .await
        {
            Ok(source) => source,
            Err(error) => {
                #[cfg(feature = "private-capture")]
                capture_selection_decision(
                    capture,
                    exchange_ref,
                    &player_client,
                    quality,
                    Some(itag),
                    error.kind.diagnostic_category().as_str(),
                    true,
                    true,
                    selection_started.elapsed(),
                );
                return Err(error);
            }
        };
        #[cfg(feature = "private-capture")]
        capture_selection_decision(
            capture,
            exchange_ref,
            &player_client,
            quality,
            Some(itag),
            "browser_selected",
            true,
            true,
            selection_started.elapsed(),
        );
        prepare_resolved_source(&mut source, video_id);
        self.verify_media_continuation(
            &mut source,
            &cancellation,
            probe_recorder,
            #[cfg(feature = "private-capture")]
            capture,
            #[cfg(feature = "private-capture")]
            exchange_ref,
        )
        .await?;
        Ok(source)
    }

    pub(crate) async fn resolve_for_probe(
        &self,
        video_id: &str,
        quality: YouTubePlaybackQuality,
        cancellation: CancellationToken,
        allow_browser_fallback: bool,
        probe_client: YouTubeProbeClient,
    ) -> (
        Result<ResolvedAudioSource, AudioSourceError>,
        bool,
        Option<&'static str>,
        &'static str,
        Vec<YouTubeProbeAttempt>,
    ) {
        let browser_fallback_attempted = AtomicBool::new(false);
        let probe_recorder = YouTubeProbeRecorder::default();
        let result = self
            .resolve_source(
                video_id,
                quality,
                cancellation,
                allow_browser_fallback,
                Some(&browser_fallback_attempted),
                true,
                probe_client,
                Some(&probe_recorder),
                #[cfg(feature = "private-capture")]
                None,
            )
            .await;
        (
            result,
            browser_fallback_attempted.load(Ordering::Acquire),
            self.javascript_solver.last_backend(),
            self.javascript_solver.configured_backend_diagnostic_label(),
            probe_recorder.finish(),
        )
    }
}

#[cfg(test)]
impl InnertubeAudioResolver {
    pub(super) fn new_for_test(player_endpoint: Url, auth_type: YouTubeMusicAuthType) -> Self {
        Self {
            client: reqwest::Client::new(),
            media_client: shared_media_http_client(),
            player_endpoint,
            auth_type,
            config_folder: PathBuf::new(),
            cookie_path: PathBuf::new(),
            oauth_path: PathBuf::new(),
            po_token_path: None,
            tv_client_version: Arc::new(OnceLock::new()),
            tv_signature_timestamp: Arc::new(OnceLock::new()),
            web_client_material: Arc::new(OnceLock::new()),
            web_music_client_material: Arc::new(OnceLock::new()),
            guest_visitor_data: Arc::new(OnceLock::new()),
            prefer_public_android_vr: Arc::new(AtomicBool::new(false)),
            warmup_state: Arc::new(AtomicU8::new(WARMUP_IDLE)),
            player_script_cache: Arc::new(tokio::sync::Mutex::new(HashMap::new())),
            ejs_solution_cache: Arc::new(tokio::sync::Mutex::new(HashMap::new())),
            javascript_solver: Arc::new(super::super::javascript::Solver::configured()),
            test_response_headers_observer: None,
            test_inspection_clients: None,
            test_playback_clients: None,
        }
    }

    pub(super) fn seed_client_versions_for_test(
        &self,
        tv_version: String,
        web_material: (String, PlayerVersionSource),
        web_music_material: (String, PlayerVersionSource),
    ) {
        self.tv_client_version
            .set(tv_version)
            .expect("test TV client version is set once");
        self.web_client_material
            .set(web_material)
            .expect("test web client material is set once");
        self.web_music_client_material
            .set(web_music_material)
            .expect("test web music client material is set once");
    }

    #[cfg(feature = "private-capture")]
    pub(super) fn seed_guest_visitor_data_for_test(&self, visitor_data: String) {
        self.guest_visitor_data
            .set(visitor_data)
            .expect("test guest visitor data is set once");
    }

    pub(super) async fn player_client_candidates_for_test(
        &self,
        plan: PlayerClientPlan,
    ) -> Vec<PlayerClient> {
        self.player_client_candidates(plan).await
    }

    pub(super) async fn probe_client_labels_for_test(
        &self,
        client: YouTubeProbeClient,
    ) -> Vec<&'static str> {
        self.probe_client_candidates(client)
            .await
            .into_iter()
            .map(|client| client.source_name)
            .collect()
    }

    pub(super) async fn solve_javascript_challenges_for_test(
        &self,
        player_script_url: &Url,
        player_script: &str,
        signature_challenges: &[String],
        n_challenges: &[String],
        cancellation: &CancellationToken,
    ) -> anyhow::Result<super::super::javascript::Solutions> {
        self.cipher_engine()
            .solve_javascript_challenges(
                player_script_url,
                player_script,
                signature_challenges,
                n_challenges,
                cancellation,
            )
            .await
    }

    pub(super) async fn ejs_solution_cache_len_for_test(&self) -> usize {
        self.ejs_solution_cache.lock().await.len()
    }

    #[cfg(feature = "private-capture")]
    pub(super) fn set_auth_paths_for_test(&mut self, config_folder: PathBuf, cookie_path: PathBuf) {
        self.config_folder = config_folder;
        self.cookie_path = cookie_path;
    }

    #[cfg(feature = "private-capture")]
    pub(super) fn set_playback_clients_for_test(&mut self, clients: Vec<PlayerClient>) {
        self.test_playback_clients = Some(clients);
    }

    #[cfg(feature = "private-capture")]
    pub(super) fn set_inspection_clients_for_test(&mut self, clients: Vec<PlayerClient>) {
        self.test_inspection_clients = Some(clients);
    }

    #[cfg(feature = "private-capture")]
    pub(super) fn set_http_client_for_test(&mut self, client: reqwest::Client) {
        self.client = client.clone();
        self.media_client = client;
    }

    #[cfg(feature = "private-capture")]
    pub(super) fn set_response_headers_observer_for_test(
        &mut self,
        observer: Arc<tokio::sync::Notify>,
    ) {
        self.test_response_headers_observer = Some(observer);
    }

    #[cfg(feature = "private-capture")]
    pub(super) fn set_player_endpoint_for_test(&mut self, endpoint: Url) {
        self.player_endpoint = endpoint;
    }

    #[cfg(feature = "private-capture")]
    pub(super) fn player_endpoint_for_client_for_test(&self, player_client: &PlayerClient) -> Url {
        self.player_endpoint_for_client(player_client)
    }

    #[cfg(feature = "private-capture")]
    pub(super) async fn verify_media_continuation_for_test(
        &self,
        source: &ResolvedAudioSource,
        cancellation: &CancellationToken,
        capture: Option<&crate::developer_capture::CaptureSession>,
        exchange_ref: crate::developer_capture::ExchangeRef,
    ) -> Result<(), AudioSourceError> {
        let mut source = source.clone();
        self.verify_media_continuation(&mut source, cancellation, None, capture, exchange_ref)
            .await
    }
}

#[async_trait::async_trait]
impl AudioSourceResolver for InnertubeAudioResolver {
    async fn warm_up(&self, cancellation: &CancellationToken) -> Result<(), AudioSourceError> {
        InnertubeAudioResolver::warm_up(self, cancellation).await
    }

    async fn warm_up_session(&self, cancellation: &CancellationToken) {
        // Client versions and guest visitor data cost a first playback about
        // a second when fetched on demand.
        let _ = tokio::join!(
            self.player_client_candidates(PlayerClientPlan::Playback),
            self.guest_visitor_data(None, cancellation),
        );
    }

    async fn resolve(
        &self,
        video_id: &str,
        quality: YouTubePlaybackQuality,
        cancellation: CancellationToken,
    ) -> Result<ResolvedAudioSource, AudioSourceError> {
        self.resolve_source(
            video_id,
            quality,
            cancellation,
            true,
            None,
            false,
            YouTubeProbeClient::Auto,
            None,
            #[cfg(feature = "private-capture")]
            None,
        )
        .await
    }

    #[cfg(feature = "private-capture")]
    async fn resolve_with_capture(
        &self,
        video_id: &str,
        quality: YouTubePlaybackQuality,
        cancellation: CancellationToken,
        capture: crate::developer_capture::CaptureSession,
    ) -> Result<ResolvedAudioSource, AudioSourceError> {
        self.resolve_source(
            video_id,
            quality,
            cancellation,
            true,
            None,
            false,
            YouTubeProbeClient::Auto,
            None,
            Some(&capture),
        )
        .await
    }

    async fn resolve_for_prefetch(
        &self,
        video_id: &str,
        quality: YouTubePlaybackQuality,
        cancellation: CancellationToken,
    ) -> Result<ResolvedAudioSource, AudioSourceError> {
        self.resolve_source(
            video_id,
            quality,
            cancellation,
            true,
            None,
            true,
            YouTubeProbeClient::Auto,
            None,
            #[cfg(feature = "private-capture")]
            None,
        )
        .await
    }

    fn playback_route(&self) -> crate::state::YouTubePlaybackRoute {
        let learned_public_android_vr = self.auth_type == YouTubeMusicAuthType::Browser
            && self.prefer_public_android_vr.load(Ordering::Acquire);
        let mut order = if self.auth_type == YouTubeMusicAuthType::Browser {
            vec![
                "VISIONOS".to_string(),
                "TVHTML5".to_string(),
                "WEB".to_string(),
                "WEB_REMIX".to_string(),
                "ANDROID_VR".to_string(),
                "WEB_MUSIC_BROWSER_SESSION".to_string(),
            ]
        } else {
            vec!["VISIONOS".to_string(), "ANDROID_VR".to_string()]
        };
        if learned_public_android_vr {
            if let Some(index) = order.iter().position(|name| name == "ANDROID_VR") {
                let android_vr = order.remove(index);
                let insert_index = order
                    .iter()
                    .position(|name| name == "VISIONOS")
                    .map_or(0, |index| index + 1)
                    .min(order.len());
                order.insert(insert_index, android_vr);
            }
        }
        crate::state::YouTubePlaybackRoute {
            selected: None,
            order,
            learned_public_android_vr,
            javascript_backend: self.javascript_solver.last_backend().map(str::to_owned),
        }
    }

    fn backend_name(&self) -> &'static str {
        "Native Innertube"
    }
}

pub(super) fn should_use_browser_session_transport(
    auth: &RequestAuth,
    error: &AudioSourceError,
) -> bool {
    error.kind == AudioSourceErrorKind::Decipher && matches!(auth, RequestAuth::Browser(_))
}

fn should_use_browser_session_transport_for_client(
    auth: &RequestAuth,
    client: &PlayerClient,
    error: &AudioSourceError,
) -> bool {
    !matches!(
        &client.kind,
        PlayerClientKind::AndroidVr | PlayerClientKind::VisionOs
    ) && should_use_browser_session_transport(auth, error)
}

fn should_try_next_player_client(error: &AudioSourceError) -> bool {
    matches!(
        error.kind,
        AudioSourceErrorKind::Unavailable | AudioSourceErrorKind::UnsupportedFormat
    ) || (error.kind == AudioSourceErrorKind::Contract
        && error.to_string() == "YouTube returned no streamingData")
}

pub(super) fn should_retry_browser_session_transport(
    auth: &RequestAuth,
    error: &AudioSourceError,
) -> bool {
    error.kind == AudioSourceErrorKind::MediaForbidden && matches!(auth, RequestAuth::Browser(_))
}

fn should_retry_browser_session_transport_for_client(
    auth: &RequestAuth,
    client: &PlayerClient,
    error: &AudioSourceError,
) -> bool {
    !matches!(
        &client.kind,
        PlayerClientKind::AndroidVr | PlayerClientKind::VisionOs
    ) && should_retry_browser_session_transport(auth, error)
}

#[cfg(test)]
mod tests {
    use super::{source_error_outcome, source_route_status, source_selection_status};
    use crate::client::youtube::playback::{AudioSourceError, AudioSourceErrorKind};

    #[test]
    fn source_selection_diagnostics_distinguish_native_decipher_and_cancelled() {
        let decipher =
            AudioSourceError::new(AudioSourceErrorKind::Decipher, "fixture decipher error");
        let cancelled =
            AudioSourceError::new(AudioSourceErrorKind::Cancelled, "fixture cancellation");

        assert_eq!(source_selection_status(None), "native_available");
        assert_eq!(
            source_selection_status(Some(&decipher)),
            "decipher_required"
        );
        assert_eq!(source_selection_status(Some(&cancelled)), "cancelled");
        assert_eq!(
            source_error_outcome(&decipher),
            crate::observability::OperationOutcome::Error
        );
        assert_eq!(
            source_error_outcome(&cancelled),
            crate::observability::OperationOutcome::Cancelled
        );
    }

    #[test]
    fn source_route_diagnostics_distinguish_native_ejs_and_browser_fallback() {
        assert_eq!(source_route_status(true, false, false), "native");
        assert_eq!(source_route_status(false, true, true), "ejs");
        assert_eq!(source_route_status(false, false, true), "browser_fallback");
        assert_eq!(source_route_status(false, false, false), "next_client");
    }
}
