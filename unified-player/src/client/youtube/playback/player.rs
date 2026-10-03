use std::{
    collections::HashMap,
    fmt::Write as _,
    path::Path,
    time::{SystemTime, UNIX_EPOCH},
};

use futures::StreamExt as _;
use reqwest::{header, Url};
use sha1::{Digest as _, Sha1};
use tokio_util::sync::CancellationToken;

use crate::config::YouTubeMusicAuthType;

#[cfg(feature = "private-capture")]
use super::forensics::{
    bounded_private_header_map, capture_network_failure, capture_player_client_kind,
    capture_player_http_response, capture_player_parse, capture_player_parse_failure,
    PRIVATE_CAPTURE_URL_LIMIT,
};
use super::{
    format::{PlayerAttemptAuthScope, PlayerResponse},
    source::{AudioSourceError, AudioSourceErrorKind},
};

pub(super) const PLAYER_ENDPOINT: &str =
    "https://www.youtube.com/youtubei/v1/player?key=AIzaSyAO_FJ2SlqU8Q4STEHLGCilw_Y9_11qcW8"; // trufflehog:ignore gitleaks:allow - public YouTube client key
const ANDROID_VR_CLIENT_NAME: &str = "28";
const ANDROID_VR_CLIENT_VERSION: &str = "1.65.10";
pub(super) const ANDROID_VR_USER_AGENT: &str =
    "com.google.android.apps.youtube.vr.oculus/1.65.10 (Linux; U; Android 12L; eureka-user Build/SQ3A.220605.009.A1) gzip";
const VISIONOS_CLIENT_NAME: &str = "101";
const VISIONOS_CLIENT_VERSION: &str = "1.02";
pub(super) const VISIONOS_USER_AGENT: &str =
    "Mozilla/5.0 (Macintosh; Intel Mac OS X 15_7_3) AppleWebKit/605.1.15 (KHTML, like Gecko) Version/26.0 Safari/605.1.15";
const YOUTUBE_ORIGIN: &str = "https://www.youtube.com";
pub(super) const YOUTUBE_MUSIC_ORIGIN: &str = "https://music.youtube.com";
pub(super) const TV_PAGE: &str = "https://www.youtube.com/tv";
pub(super) const WEB_PAGE: &str = "https://www.youtube.com";
pub(super) const WEB_MUSIC_PAGE: &str = "https://music.youtube.com";
const TV_CLIENT_NAME: &str = "7";
pub(super) const TV_CONTEXT_NAME: &str = "TVHTML5";
pub(super) const TV_USER_AGENT: &str = "Mozilla/5.0 (ChromiumStylePlatform) Cobalt/Version";
const WEB_CLIENT_NAME: &str = "1";
pub(super) const WEB_CONTEXT_NAME: &str = "WEB";
pub(super) const WEB_DEFAULT_CLIENT_VERSION_PREFIX: &str = "2.";
const WEB_MUSIC_CLIENT_NAME: &str = "67";
pub(super) const WEB_MUSIC_CONTEXT_NAME: &str = "WEB_REMIX";
pub(super) const WEB_MUSIC_DEFAULT_CLIENT_VERSION_PREFIX: &str = "1.";
pub(super) const WEB_USER_AGENT: &str =
    "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/131.0.0.0 Safari/537.36";
#[cfg(feature = "private-capture")]
const WEB_SAFARI_USER_AGENT: &str =
    "Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15_7) AppleWebKit/605.1.15 (KHTML, like Gecko) Version/15.5 Safari/605.1.15,gzip(gfe)";
pub(super) const BROWSER_SESSION_SOURCE_NAME: &str = "WEB_MUSIC_BROWSER_SESSION";
pub(super) const PLAYER_LANGUAGE: &str = "en";
pub(super) const PLAYER_REGION: &str = "US";
pub(super) const PLAYER_RESPONSE_COPY_LIMIT: usize = 2 * 1024 * 1024;

#[cfg(feature = "private-capture")]
pub(super) type PlayerResponseBody = zeroize::Zeroizing<Vec<u8>>;
#[cfg(not(feature = "private-capture"))]
pub(super) type PlayerResponseBody = Vec<u8>;

pub(super) fn empty_player_response_body() -> PlayerResponseBody {
    #[cfg(feature = "private-capture")]
    {
        zeroize::Zeroizing::new(Vec::new())
    }
    #[cfg(not(feature = "private-capture"))]
    {
        Vec::new()
    }
}

pub(super) enum PlayerResponseBodyError {
    Network(reqwest::Error),
    TooLarge(PlayerResponseBody),
}

pub(super) async fn read_player_response_body(
    response: reqwest::Response,
) -> Result<PlayerResponseBody, PlayerResponseBodyError> {
    let mut body = empty_player_response_body();
    let mut stream = response.bytes_stream();
    while let Some(chunk) = stream.next().await {
        let chunk = chunk.map_err(PlayerResponseBodyError::Network)?;
        if !append_player_response_chunk(&mut body, &chunk) {
            return Err(PlayerResponseBodyError::TooLarge(body));
        }
    }
    Ok(body)
}

pub(super) fn append_player_response_chunk(body: &mut Vec<u8>, chunk: &[u8]) -> bool {
    let remaining = PLAYER_RESPONSE_COPY_LIMIT.saturating_sub(body.len());
    if chunk.len() > remaining {
        body.extend_from_slice(&chunk[..remaining]);
        return false;
    }
    body.extend_from_slice(chunk);
    true
}

#[derive(Clone)]
pub(super) enum RequestAuth {
    None,
    Browser(header::HeaderMap),
    Bearer(String),
}

#[derive(Clone, Debug, Default)]
pub(super) struct PoTokenMaterial {
    pub(super) legacy_player: Option<String>,
    pub(super) player: Option<String>,
    pub(super) gvs: Option<String>,
    pub(super) clients: HashMap<String, PoTokenClientMaterial>,
}

#[derive(Clone, Debug, Default)]
pub(super) struct PoTokenClientMaterial {
    player: Option<String>,
    gvs: Option<String>,
}

impl PoTokenMaterial {
    pub(super) fn player_for<'a>(&'a self, client: &PlayerClient) -> Option<&'a str> {
        if matches!(client.kind, PlayerClientKind::VisionOs) {
            return None;
        }
        if matches!(client.kind, PlayerClientKind::AndroidVr) {
            return self
                .client_scope(client)
                .and_then(|scope| scope.player.as_deref());
        }
        self.client_scope(client)
            .and_then(|scope| scope.player.as_deref())
            .or(self.player.as_deref())
            .or(self.legacy_player.as_deref())
    }

    pub(super) fn gvs_for<'a>(&'a self, client: &PlayerClient) -> Option<&'a str> {
        if matches!(client.kind, PlayerClientKind::VisionOs) {
            return None;
        }
        if matches!(client.kind, PlayerClientKind::AndroidVr) {
            return self
                .client_scope(client)
                .and_then(|scope| scope.gvs.as_deref());
        }
        self.client_scope(client)
            .and_then(|scope| scope.gvs.as_deref())
            .or(self.gvs.as_deref())
    }

    pub(super) fn client_scope(&self, client: &PlayerClient) -> Option<&PoTokenClientMaterial> {
        self.clients
            .get(client.source_name)
            .or_else(|| self.clients.get(client.context_name))
    }

    pub(super) fn has_scoped_playback_token(&self, client: &PlayerClient) -> bool {
        self.client_scope(client)
            .is_some_and(|scope| scope.player.is_some() || scope.gvs.is_some())
    }

    pub(super) fn from_text(text: &str) -> Self {
        let trimmed = text.trim();
        if trimmed.is_empty() {
            return Self::default();
        }
        let Ok(value) = serde_json::from_str::<serde_json::Value>(trimmed) else {
            return Self {
                legacy_player: Some(trimmed.to_owned()),
                ..Self::default()
            };
        };
        let Some(object) = value.as_object() else {
            return Self {
                legacy_player: Some(trimmed.to_owned()),
                ..Self::default()
            };
        };

        let mut material = Self {
            player: json_token(object, &["player", "player_token", "token"]),
            gvs: json_token(object, &["gvs", "gvs_token"]),
            ..Self::default()
        };
        if let Some(clients) = object.get("clients").and_then(serde_json::Value::as_object) {
            for (name, value) in clients {
                material
                    .clients
                    .insert(name.clone(), json_client_scope(value));
            }
        }
        if material.player.is_none() && material.gvs.is_none() && material.clients.is_empty() {
            material.legacy_player = Some(trimmed.to_owned());
        }
        material
    }
}

pub(super) fn json_token(
    object: &serde_json::Map<String, serde_json::Value>,
    names: &[&str],
) -> Option<String> {
    names.iter().find_map(|name| {
        object
            .get(*name)
            .and_then(serde_json::Value::as_str)
            .and_then(normalize_po_token)
    })
}

pub(super) fn json_client_scope(value: &serde_json::Value) -> PoTokenClientMaterial {
    let Some(object) = value.as_object() else {
        return PoTokenClientMaterial::default();
    };
    PoTokenClientMaterial {
        player: json_token(object, &["player", "player_token", "token"]),
        gvs: json_token(object, &["gvs", "gvs_token"]),
    }
}

pub(super) fn normalize_po_token(token: &str) -> Option<String> {
    let token = token.trim();
    (!token.is_empty() && !token.contains(['\r', '\n'])).then(|| token.to_owned())
}

pub(super) fn build_player_request(
    http: &reqwest::Client,
    endpoint: Url,
    video_id: &str,
    auth: &RequestAuth,
    player_client: &PlayerClient,
    po_token: Option<&str>,
    visitor_data: Option<&str>,
) -> Result<reqwest::Request, AudioSourceError> {
    let auth = player_client.auth_for_request(auth);
    let po_token = player_client.player_po_token(po_token);
    let mut body = serde_json::json!({
        "context": {
            "client": {
                "clientName": player_client.context_name,
                "clientVersion": player_client.version,
                "hl": PLAYER_LANGUAGE,
                "gl": PLAYER_REGION,
                "userAgent": player_client.user_agent,
                "timeZone": "UTC",
                "utcOffsetMinutes": 0
            }
        },
        "playbackContext": {
            "contentPlaybackContext": {
                "html5Preference": "HTML5_PREF_WANTS"
            }
        },
        "videoId": video_id,
        "contentCheckOk": true,
        "racyCheckOk": true
    });
    player_client.extend_context(&mut body);
    if let Some(signature_timestamp) = player_client.signature_timestamp {
        body["playbackContext"]["contentPlaybackContext"]["signatureTimestamp"] =
            serde_json::json!(signature_timestamp);
    }
    if let Some(po_token) = po_token {
        body["serviceIntegrityDimensions"] = serde_json::json!({ "poToken": po_token });
    }

    let body = serde_json::to_vec(&body).map_err(|_| {
        AudioSourceError::new(
            AudioSourceErrorKind::Contract,
            "encode YouTube playback source request",
        )
    })?;
    let mut request = http
        .post(endpoint)
        .header("X-YouTube-Client-Name", player_client.header_name)
        .header("X-YouTube-Client-Version", &player_client.version)
        .header(header::USER_AGENT, player_client.user_agent)
        .header(header::CONTENT_TYPE, "application/json")
        .body(body);
    if let Some(visitor_data) = visitor_data {
        let visitor_data = header::HeaderValue::from_str(visitor_data).map_err(|_| {
            AudioSourceError::new(
                AudioSourceErrorKind::Contract,
                "YouTube guest visitor data contained an invalid header value",
            )
        })?;
        request = request.header(
            header::HeaderName::from_static("x-goog-visitor-id"),
            visitor_data,
        );
    }
    request = match &auth {
        RequestAuth::None => request,
        RequestAuth::Browser(headers) => {
            let origin = if player_client.endpoint_host == "music.youtube.com" {
                YOUTUBE_MUSIC_ORIGIN
            } else {
                YOUTUBE_ORIGIN
            };
            let headers = browser_request_headers_for_origin(headers, origin)?;
            request.headers(headers)
        }
        RequestAuth::Bearer(token) => request.bearer_auth(token),
    };
    request.build().map_err(|_| {
        AudioSourceError::new(
            AudioSourceErrorKind::Contract,
            "build YouTube playback source request",
        )
    })
}

#[derive(Clone, Debug)]
pub(super) struct PlayerClient {
    pub(super) header_name: &'static str,
    pub(super) context_name: &'static str,
    pub(super) version: String,
    pub(super) user_agent: &'static str,
    pub(super) endpoint_host: &'static str,
    pub(super) source_name: &'static str,
    pub(super) kind: PlayerClientKind,
    pub(super) signature_timestamp: Option<u32>,
    #[cfg(feature = "private-capture")]
    pub(super) version_source: PlayerVersionSource,
}

pub(super) struct PlayerClientAttempt {
    pub(super) client: PlayerClient,
    pub(super) auth_scope: PlayerAttemptAuthScope,
    #[cfg(feature = "private-capture")]
    pub(super) proof_token_present: bool,
    pub(super) response: Result<PlayerResponse, AudioSourceError>,
}

#[cfg(feature = "private-capture")]
pub(super) fn request_auth_kind(auth: &RequestAuth) -> &'static str {
    match auth {
        RequestAuth::None => "none",
        RequestAuth::Browser(_) => "browser",
        RequestAuth::Bearer(_) => "oauth_bearer",
    }
}

#[cfg(feature = "private-capture")]
pub(super) fn configured_request_auth_kind(auth_type: YouTubeMusicAuthType) -> &'static str {
    match auth_type {
        YouTubeMusicAuthType::Browser => "browser",
        YouTubeMusicAuthType::OAuth => "oauth_bearer",
        YouTubeMusicAuthType::Unauthenticated => "none",
    }
}

#[cfg(feature = "private-capture")]
pub(super) fn player_client_kind_name(kind: &PlayerClientKind) -> &'static str {
    match kind {
        PlayerClientKind::AndroidVr => "android_vr",
        PlayerClientKind::VisionOs => "visionos",
        PlayerClientKind::Tv => "tv_html5",
        PlayerClientKind::Web => "web",
        PlayerClientKind::WebSafari => "web_safari",
        PlayerClientKind::WebRemix => "web_remix",
    }
}

#[cfg(feature = "private-capture")]
pub(super) fn player_version_source_name(source: PlayerVersionSource) -> &'static str {
    match source {
        PlayerVersionSource::Static => "static",
        PlayerVersionSource::Cached => "cached",
        PlayerVersionSource::Discovered => "discovered",
        PlayerVersionSource::Fallback => "fallback",
    }
}

#[derive(Clone, Debug)]
pub(super) enum PlayerClientKind {
    AndroidVr,
    VisionOs,
    Tv,
    Web,
    WebRemix,
    #[cfg(feature = "private-capture")]
    WebSafari,
}

#[derive(Clone, Copy, Debug)]
#[cfg_attr(not(feature = "private-capture"), allow(dead_code))]
pub(super) enum PlayerVersionSource {
    Static,
    Cached,
    Discovered,
    Fallback,
}

impl PlayerClient {
    pub(super) fn android_vr() -> Self {
        Self {
            header_name: ANDROID_VR_CLIENT_NAME,
            context_name: "ANDROID_VR",
            version: ANDROID_VR_CLIENT_VERSION.to_string(),
            user_agent: ANDROID_VR_USER_AGENT,
            endpoint_host: "www.youtube.com",
            source_name: "ANDROID_VR",
            kind: PlayerClientKind::AndroidVr,
            signature_timestamp: None,
            #[cfg(feature = "private-capture")]
            version_source: PlayerVersionSource::Static,
        }
    }

    pub(super) fn visionos() -> Self {
        Self {
            header_name: VISIONOS_CLIENT_NAME,
            context_name: "VISIONOS",
            version: VISIONOS_CLIENT_VERSION.to_string(),
            user_agent: VISIONOS_USER_AGENT,
            endpoint_host: "www.youtube.com",
            source_name: "VISIONOS",
            kind: PlayerClientKind::VisionOs,
            signature_timestamp: None,
            #[cfg(feature = "private-capture")]
            version_source: PlayerVersionSource::Static,
        }
    }

    pub(super) fn tv(version: String, version_source: PlayerVersionSource) -> Self {
        #[cfg(not(feature = "private-capture"))]
        let _ = version_source;
        Self {
            header_name: TV_CLIENT_NAME,
            context_name: TV_CONTEXT_NAME,
            version,
            user_agent: TV_USER_AGENT,
            endpoint_host: "www.youtube.com",
            source_name: TV_CONTEXT_NAME,
            kind: PlayerClientKind::Tv,
            signature_timestamp: None,
            #[cfg(feature = "private-capture")]
            version_source,
        }
    }

    pub(super) fn web(version: String, version_source: PlayerVersionSource) -> Self {
        #[cfg(not(feature = "private-capture"))]
        let _ = version_source;
        Self {
            header_name: WEB_CLIENT_NAME,
            context_name: WEB_CONTEXT_NAME,
            version,
            user_agent: WEB_USER_AGENT,
            endpoint_host: "www.youtube.com",
            source_name: WEB_CONTEXT_NAME,
            kind: PlayerClientKind::Web,
            signature_timestamp: None,
            #[cfg(feature = "private-capture")]
            version_source,
        }
    }

    #[cfg(feature = "private-capture")]
    pub(super) fn web_safari(version: String, version_source: PlayerVersionSource) -> Self {
        Self {
            header_name: WEB_CLIENT_NAME,
            context_name: WEB_CONTEXT_NAME,
            version,
            user_agent: WEB_SAFARI_USER_AGENT,
            endpoint_host: "www.youtube.com",
            source_name: "WEB_SAFARI",
            kind: PlayerClientKind::WebSafari,
            signature_timestamp: None,
            version_source,
        }
    }

    pub(super) fn web_music(version: String, version_source: PlayerVersionSource) -> Self {
        #[cfg(not(feature = "private-capture"))]
        let _ = version_source;
        Self {
            header_name: WEB_MUSIC_CLIENT_NAME,
            context_name: WEB_MUSIC_CONTEXT_NAME,
            version,
            user_agent: WEB_USER_AGENT,
            endpoint_host: "music.youtube.com",
            source_name: WEB_MUSIC_CONTEXT_NAME,
            kind: PlayerClientKind::WebRemix,
            signature_timestamp: None,
            #[cfg(feature = "private-capture")]
            version_source,
        }
    }

    pub(super) fn with_signature_timestamp(mut self, signature_timestamp: Option<u32>) -> Self {
        self.signature_timestamp = signature_timestamp;
        self
    }

    pub(super) fn auth_scope(&self, auth: &RequestAuth) -> PlayerAttemptAuthScope {
        if matches!(self.kind, PlayerClientKind::VisionOs)
            || matches!(auth, RequestAuth::None)
            || (matches!(&self.kind, PlayerClientKind::AndroidVr)
                && matches!(auth, RequestAuth::Browser(_)))
        {
            PlayerAttemptAuthScope::Public
        } else {
            PlayerAttemptAuthScope::Account
        }
    }

    pub(super) fn requires_configured_auth(&self, auth_type: YouTubeMusicAuthType) -> bool {
        !matches!(
            (&self.kind, auth_type),
            (_, YouTubeMusicAuthType::Unauthenticated)
                | (PlayerClientKind::VisionOs, _)
                | (PlayerClientKind::AndroidVr, YouTubeMusicAuthType::Browser)
        )
    }

    pub(super) fn auth_for_request(&self, auth: &RequestAuth) -> RequestAuth {
        if matches!(self.kind, PlayerClientKind::VisionOs)
            || (matches!(self.kind, PlayerClientKind::AndroidVr)
                && matches!(auth, RequestAuth::Browser(_)))
        {
            RequestAuth::None
        } else {
            auth.clone()
        }
    }

    pub(super) fn player_po_token<'a>(&self, po_token: Option<&'a str>) -> Option<&'a str> {
        if matches!(self.kind, PlayerClientKind::VisionOs) {
            None
        } else {
            po_token
        }
    }

    pub(super) fn extend_context(&self, body: &mut serde_json::Value) {
        match self.kind {
            PlayerClientKind::AndroidVr => {
                body["context"]["client"]["deviceMake"] = serde_json::json!("Oculus");
                body["context"]["client"]["deviceModel"] = serde_json::json!("Quest 3");
                body["context"]["client"]["androidSdkVersion"] = serde_json::json!(32);
                body["context"]["client"]["osName"] = serde_json::json!("Android");
                body["context"]["client"]["osVersion"] = serde_json::json!("12L");
            }
            PlayerClientKind::VisionOs => {
                body["context"]["client"]["deviceMake"] = serde_json::json!("Apple");
                body["context"]["client"]["deviceModel"] = serde_json::json!("RealityDevice17,1");
                body["context"]["client"]["osName"] = serde_json::json!("visionOS");
                body["context"]["client"]["osVersion"] = serde_json::json!("26.5.23O471");
            }
            PlayerClientKind::Tv => {}
            PlayerClientKind::Web | PlayerClientKind::WebRemix => {}
            #[cfg(feature = "private-capture")]
            PlayerClientKind::WebSafari => {}
        }
    }
}

pub(super) async fn load_request_auth(
    auth_type: YouTubeMusicAuthType,
    cookie_path: &Path,
    oauth_path: &Path,
    browser_timestamp_unix_secs: Option<u64>,
) -> Result<RequestAuth, AudioSourceError> {
    match auth_type {
        YouTubeMusicAuthType::Browser => {
            let cookie = tokio::fs::read_to_string(cookie_path).await.map_err(|_| {
                AudioSourceError::new(
                    AudioSourceErrorKind::Authentication,
                    "read YouTube Music browser credentials",
                )
            })?;
            match browser_timestamp_unix_secs {
                Some(timestamp) => browser_request_auth_at(cookie.trim(), timestamp),
                None => browser_request_auth(cookie.trim()),
            }
        }
        YouTubeMusicAuthType::OAuth => {
            let token = tokio::fs::read_to_string(oauth_path).await.map_err(|_| {
                AudioSourceError::new(
                    AudioSourceErrorKind::Authentication,
                    "read YouTube Music OAuth credentials",
                )
            })?;
            let value: serde_json::Value = serde_json::from_str(&token).map_err(|_| {
                AudioSourceError::new(
                    AudioSourceErrorKind::Authentication,
                    "parse YouTube Music OAuth credentials",
                )
            })?;
            let access_token = value
                .get("access_token")
                .and_then(serde_json::Value::as_str)
                .ok_or_else(|| {
                    AudioSourceError::new(
                        AudioSourceErrorKind::Authentication,
                        "YouTube Music OAuth credentials contain no access_token",
                    )
                })?;
            Ok(RequestAuth::Bearer(access_token.to_string()))
        }
        YouTubeMusicAuthType::Unauthenticated => Ok(RequestAuth::None),
    }
}

pub(super) async fn load_optional_po_tokens(path: Option<&Path>) -> PoTokenMaterial {
    let Some(path) = path else {
        return PoTokenMaterial::default();
    };
    let Ok(token_file) = tokio::fs::read_to_string(path).await else {
        return PoTokenMaterial::default();
    };
    PoTokenMaterial::from_text(&token_file)
}

pub(super) fn browser_request_auth(cookie: &str) -> Result<RequestAuth, AudioSourceError> {
    let timestamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(|_| {
            AudioSourceError::new(
                AudioSourceErrorKind::Authentication,
                "system clock is earlier than the Unix epoch",
            )
        })?
        .as_secs();
    browser_request_auth_at(cookie, timestamp)
}

pub(super) fn browser_request_auth_at(
    cookie: &str,
    timestamp: u64,
) -> Result<RequestAuth, AudioSourceError> {
    if cookie.is_empty() || cookie.contains(['\r', '\n']) {
        return Err(AudioSourceError::new(
            AudioSourceErrorKind::Authentication,
            "YouTube Music browser credentials are empty or contain an invalid line break",
        ));
    }
    let authorization = browser_authorization(cookie, timestamp, YOUTUBE_ORIGIN)?;

    let mut headers = header::HeaderMap::new();
    insert_auth_header(&mut headers, header::COOKIE, cookie)?;
    insert_auth_header(&mut headers, header::AUTHORIZATION, &authorization)?;
    insert_auth_header(&mut headers, header::ORIGIN, YOUTUBE_ORIGIN)?;
    insert_auth_header(
        &mut headers,
        header::HeaderName::from_static("x-origin"),
        YOUTUBE_ORIGIN,
    )?;
    insert_auth_header(
        &mut headers,
        header::HeaderName::from_static("x-goog-authuser"),
        "0",
    )?;
    if cookie_value(cookie, "LOGIN_INFO").is_some() {
        insert_auth_header(
            &mut headers,
            header::HeaderName::from_static("x-youtube-bootstrap-logged-in"),
            "true",
        )?;
    }
    if let Some(visitor_id) = cookie_value(cookie, "VISITOR_INFO1_LIVE") {
        insert_auth_header(
            &mut headers,
            header::HeaderName::from_static("x-goog-visitor-id"),
            visitor_id,
        )?;
    }
    Ok(RequestAuth::Browser(headers))
}

pub(super) fn browser_request_headers_for_origin(
    source: &header::HeaderMap,
    origin: &'static str,
) -> Result<header::HeaderMap, AudioSourceError> {
    let mut headers = source.clone();
    if origin != YOUTUBE_ORIGIN {
        let cookie = headers
            .get(header::COOKIE)
            .and_then(|value| value.to_str().ok())
            .ok_or_else(|| {
                AudioSourceError::new(
                    AudioSourceErrorKind::Authentication,
                    "YouTube Music browser credentials contain no cookie header",
                )
            })?;
        let timestamp = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_err(|_| {
                AudioSourceError::new(
                    AudioSourceErrorKind::Authentication,
                    "system clock is earlier than the Unix epoch",
                )
            })?
            .as_secs();
        let authorization = browser_authorization(cookie, timestamp, origin)?;
        insert_auth_header(&mut headers, header::AUTHORIZATION, &authorization)?;
    }
    let origin_header = header::HeaderValue::from_static(origin);
    headers.insert(header::ORIGIN, origin_header.clone());
    headers.insert(header::HeaderName::from_static("x-origin"), origin_header);
    Ok(headers)
}

pub(super) fn browser_authorization(
    cookie: &str,
    timestamp: u64,
    origin: &'static str,
) -> Result<String, AudioSourceError> {
    let mut authorizations = Vec::with_capacity(3);
    let primary_sapisid = cookie_value(cookie, "SAPISID")
        .or_else(|| cookie_value(cookie, "__Secure-3PAPISID"))
        .or_else(|| cookie_value(cookie, "__Secure-1PAPISID"));
    if let Some(sapisid) = primary_sapisid {
        authorizations.push(sapisid_authorization_for_origin(
            "SAPISIDHASH",
            sapisid,
            timestamp,
            origin,
        ));
    }
    if let Some(sapisid) = cookie_value(cookie, "__Secure-1PAPISID") {
        authorizations.push(sapisid_authorization_for_origin(
            "SAPISID1PHASH",
            sapisid,
            timestamp,
            origin,
        ));
    }
    if let Some(sapisid) = cookie_value(cookie, "__Secure-3PAPISID") {
        authorizations.push(sapisid_authorization_for_origin(
            "SAPISID3PHASH",
            sapisid,
            timestamp,
            origin,
        ));
    }
    if authorizations.is_empty() {
        return Err(AudioSourceError::new(
            AudioSourceErrorKind::Authentication,
            "YouTube Music browser credentials contain no SAPISID session cookie",
        ));
    }
    Ok(authorizations.join(" "))
}

pub(super) fn insert_auth_header(
    headers: &mut header::HeaderMap,
    name: header::HeaderName,
    value: &str,
) -> Result<(), AudioSourceError> {
    let value = header::HeaderValue::from_str(value).map_err(|_| {
        AudioSourceError::new(
            AudioSourceErrorKind::Authentication,
            "YouTube Music browser credentials contain an invalid header value",
        )
    })?;
    headers.insert(name, value);
    Ok(())
}

pub(super) fn cookie_value<'a>(cookie: &'a str, name: &str) -> Option<&'a str> {
    cookie.split(';').find_map(|part| {
        let (key, value) = part.trim().split_once('=')?;
        (key == name && !value.is_empty()).then_some(value)
    })
}

#[cfg(test)]
pub(super) fn sapisid_authorization(sapisid: &str, timestamp: u64) -> String {
    sapisid_authorization_for_origin("SAPISIDHASH", sapisid, timestamp, YOUTUBE_ORIGIN)
}

pub(super) fn sapisid_authorization_for_origin(
    scheme: &str,
    sapisid: &str,
    timestamp: u64,
    origin: &str,
) -> String {
    let mut hasher = Sha1::new();
    hasher.update(format!("{timestamp} {sapisid} {origin}"));
    let hash = hasher
        .finalize()
        .iter()
        .fold(String::with_capacity(40), |mut output, byte| {
            write!(output, "{byte:02x}").expect("writing to a String cannot fail");
            output
        });
    format!("{scheme} {timestamp}_{hash}")
}

pub(super) fn parse_tv_client_version(page: &str) -> Option<String> {
    let current = page
        .split_once("\"INNERTUBE_CONTEXT_CLIENT_VERSION\":\"")?
        .1
        .split_once('"')?
        .0;
    let date = current.split('.').nth(1)?;
    (date.len() == 8 && date.bytes().all(|byte| byte.is_ascii_digit())).then(|| format!("5.{date}"))
}

pub(super) fn fallback_tv_client_version() -> String {
    format!("5.{}", chrono::Utc::now().format("%Y%m%d"))
}

pub(super) fn parse_signature_timestamp(page: &str) -> Option<u32> {
    [
        "\"STS\":",
        "\"STS\" :",
        "\"signatureTimestamp\":",
        "\"signatureTimestamp\" :",
    ]
    .iter()
    .find_map(|marker| {
        let value = page.split_once(marker)?.1.trim_start();
        let digits = value
            .bytes()
            .take_while(u8::is_ascii_digit)
            .collect::<Vec<_>>();
        if digits.len() < 5 {
            return None;
        }
        std::str::from_utf8(&digits).ok()?.parse().ok()
    })
}

pub(super) fn parse_innertube_client_version(page: &str, expected_prefix: &str) -> Option<String> {
    let version = page
        .split_once("\"INNERTUBE_CONTEXT_CLIENT_VERSION\":\"")?
        .1
        .split_once('"')?
        .0;
    version
        .starts_with(expected_prefix)
        .then(|| version.to_owned())
}

pub(super) fn parse_visitor_data(page: &str) -> Option<String> {
    ["VISITOR_DATA", "visitorData"].iter().find_map(|key| {
        let marker = format!("\"{key}\"");
        let value = page.split_once(&marker)?.1.trim_start();
        let value = value.strip_prefix(':')?.trim_start();
        parse_json_string_prefix(value).filter(|value| {
            !value.is_empty()
                && value.len() <= 4 * 1024
                && !value.bytes().any(|byte| byte.is_ascii_control())
        })
    })
}

fn parse_json_string_prefix(value: &str) -> Option<String> {
    if !value.starts_with('"') {
        return None;
    }
    let mut escaped = false;
    for (index, character) in value.char_indices().skip(1) {
        if escaped {
            escaped = false;
            continue;
        }
        match character {
            '\\' => escaped = true,
            '"' => return serde_json::from_str(&value[..=index]).ok(),
            _ => {}
        }
    }
    None
}

pub(super) fn fallback_web_client_version() -> String {
    format!(
        "{}{}.00.00",
        WEB_DEFAULT_CLIENT_VERSION_PREFIX,
        chrono::Utc::now().format("%Y%m%d")
    )
}

pub(super) fn fallback_web_music_client_version() -> String {
    format!(
        "{}{}.12.00",
        WEB_MUSIC_DEFAULT_CLIENT_VERSION_PREFIX,
        chrono::Utc::now().format("%Y%m%d")
    )
}

#[allow(clippy::too_many_arguments)]
pub(super) async fn request_player_response(
    http: &reqwest::Client,
    player_endpoint: Url,
    #[cfg(test)] response_headers_observer: Option<&std::sync::Arc<tokio::sync::Notify>>,
    video_id: &str,
    auth: &RequestAuth,
    player_client: &PlayerClient,
    po_token: Option<&str>,
    visitor_data: Option<&str>,
    cancellation: &CancellationToken,
    #[cfg(feature = "private-capture")] capture: Option<&crate::developer_capture::CaptureSession>,
    #[cfg(feature = "private-capture")] exchange_ref: crate::developer_capture::ExchangeRef,
) -> Result<PlayerResponse, AudioSourceError> {
    let request_auth = player_client.auth_for_request(auth);
    let po_token = player_client.player_po_token(po_token);
    let request = build_player_request(
        http,
        player_endpoint.clone(),
        video_id,
        &request_auth,
        player_client,
        po_token,
        visitor_data,
    )?;

    #[cfg(feature = "private-capture")]
    if let Some(capture) = capture {
        if !matches!(&request_auth, RequestAuth::None)
            || po_token.is_some()
            || visitor_data.is_some()
        {
            capture.mark_credential_values_present();
        }
        match crate::developer_capture::encode_private_http_request_bounded(
            &request,
            capture.body_limit(crate::developer_capture::CaptureRecordKind::HttpRequest),
        ) {
            Ok((payload, complete)) => {
                let _ = capture.record_with_context(
                    Some(exchange_ref),
                    crate::developer_capture::EndpointRole::PlayerApi,
                    capture_player_client_kind(player_client),
                    crate::developer_capture::TransportKind::NativeHttp,
                    1,
                    crate::developer_capture::CaptureRecordKind::HttpRequest,
                    payload,
                );
                if !complete {
                    capture.note_incomplete(crate::developer_capture::IncompleteReason::Truncated);
                }
            }
            Err(_) => {
                capture.note_incomplete(crate::developer_capture::IncompleteReason::Truncated);
            }
        }
    }

    #[cfg(feature = "private-capture")]
    let request_started = std::time::Instant::now();
    let response = tokio::select! {
        biased;
        response = http.execute(request) => match response {
            Ok(response) => response,
            Err(error) => {
                #[cfg(feature = "private-capture")]
                capture_network_failure(
                    capture,
                    Some(exchange_ref),
                    "execute",
                    "network",
                    Some(&error),
                );
                #[cfg(not(feature = "private-capture"))]
                let _ = error;
                return Err(AudioSourceError::new(
                    AudioSourceErrorKind::Network,
                    "request YouTube playback source",
                ));
            }
        },
        () = cancellation.cancelled() => {
            #[cfg(feature = "private-capture")]
            capture_network_failure(capture, Some(exchange_ref), "execute", "cancelled", None);
            return Err(AudioSourceError::new(
                AudioSourceErrorKind::Cancelled,
                "YouTube playback source request was cancelled",
            ));
        }
    };
    #[cfg(test)]
    if let Some(observer) = response_headers_observer {
        observer.notify_one();
    }
    let status = response.status();
    #[cfg(feature = "private-capture")]
    let redirected = response.url() != &player_endpoint;
    #[cfg(feature = "private-capture")]
    let final_url = (response.url().as_str().len() <= PRIVATE_CAPTURE_URL_LIMIT)
        .then(|| response.url().clone());
    #[cfg(feature = "private-capture")]
    let (headers, headers_complete) =
        if let Some(headers) = bounded_private_header_map(response.headers()) {
            (headers, true)
        } else {
            if let Some(capture) = capture {
                capture.note_incomplete(crate::developer_capture::IncompleteReason::Truncated);
            }
            (header::HeaderMap::new(), false)
        };
    let response_body = tokio::select! {
        () = cancellation.cancelled() => {
            #[cfg(feature = "private-capture")]
            capture_player_http_response(
                capture,
                exchange_ref,
                player_client,
                status,
                final_url.as_ref(),
                &headers,
                &[],
                request_started.elapsed(),
                false,
                redirected,
            );
            #[cfg(feature = "private-capture")]
            capture_network_failure(
                capture,
                Some(exchange_ref),
                "response_body",
                "cancelled",
                None,
            );
            return Err(AudioSourceError::new(
                AudioSourceErrorKind::Cancelled,
                "YouTube playback source response was cancelled",
            ));
        }
        body = read_player_response_body(response) => match body {
            Ok(body) => body,
            Err(PlayerResponseBodyError::Network(error)) => {
                #[cfg(feature = "private-capture")]
                capture_player_http_response(
                    capture,
                    exchange_ref,
                    player_client,
                    status,
                    final_url.as_ref(),
                    &headers,
                    &[],
                    request_started.elapsed(),
                    false,
                    redirected,
                );
                #[cfg(feature = "private-capture")]
                capture_network_failure(
                    capture,
                    Some(exchange_ref),
                    "response_body",
                    "network",
                    Some(&error),
                );
                #[cfg(not(feature = "private-capture"))]
                let _ = error;
                return Err(AudioSourceError::new(
                    AudioSourceErrorKind::Network,
                    "read YouTube playback source response",
                ));
            }
            Err(PlayerResponseBodyError::TooLarge(body)) => {
                #[cfg(feature = "private-capture")]
                capture_player_http_response(
                    capture,
                    exchange_ref,
                    player_client,
                    status,
                    final_url.as_ref(),
                    &headers,
                    &body,
                    request_started.elapsed(),
                    false,
                    redirected,
                );
                #[cfg(not(feature = "private-capture"))]
                let _ = body;
                #[cfg(feature = "private-capture")]
                capture_network_failure(
                    capture,
                    Some(exchange_ref),
                    "response_body",
                    "contract",
                    None,
                );
                return Err(AudioSourceError::new(
                    AudioSourceErrorKind::Contract,
                    "YouTube playback source response exceeded the local size limit",
                ));
            }
        },
    };
    #[cfg(feature = "private-capture")]
    capture_player_http_response(
        capture,
        exchange_ref,
        player_client,
        status,
        final_url.as_ref(),
        &headers,
        &response_body,
        request_started.elapsed(),
        headers_complete,
        redirected,
    );

    if status == reqwest::StatusCode::TOO_MANY_REQUESTS {
        #[cfg(feature = "private-capture")]
        capture_network_failure(
            capture,
            Some(exchange_ref),
            "http_status",
            "rate_limited",
            None,
        );
        return Err(AudioSourceError::new(
            AudioSourceErrorKind::RateLimited,
            "YouTube playback source request was rate limited",
        ));
    }
    if status == reqwest::StatusCode::UNAUTHORIZED || status == reqwest::StatusCode::FORBIDDEN {
        let kind = if po_token.is_some() && status == reqwest::StatusCode::FORBIDDEN {
            AudioSourceErrorKind::ProofToken
        } else if matches!(&request_auth, RequestAuth::None) {
            AudioSourceErrorKind::Unavailable
        } else {
            AudioSourceErrorKind::Authentication
        };
        #[cfg(feature = "private-capture")]
        capture_network_failure(
            capture,
            Some(exchange_ref),
            "http_status",
            kind.diagnostic_category().as_str(),
            None,
        );
        return Err(AudioSourceError::new(
            kind,
            "YouTube rejected the playback source request",
        ));
    }
    if !status.is_success() {
        #[cfg(feature = "private-capture")]
        capture_network_failure(capture, Some(exchange_ref), "http_status", "network", None);
        return Err(AudioSourceError::new(
            AudioSourceErrorKind::Network,
            "YouTube playback source response failed",
        ));
    }
    #[cfg(feature = "private-capture")]
    let parse_started = std::time::Instant::now();
    if let Ok(response) = serde_json::from_slice::<PlayerResponse>(&response_body) {
        #[cfg(feature = "private-capture")]
        let response = {
            let mut response = response;
            response.http_status = status.as_u16();
            response
        };
        #[cfg(feature = "private-capture")]
        capture_player_parse(
            capture,
            exchange_ref,
            player_client,
            &response,
            parse_started.elapsed(),
        );
        Ok(response)
    } else {
        #[cfg(feature = "private-capture")]
        capture_player_parse_failure(
            capture,
            exchange_ref,
            player_client,
            parse_started.elapsed(),
        );
        #[cfg(feature = "private-capture")]
        capture_network_failure(capture, Some(exchange_ref), "deserialize", "contract", None);
        Err(AudioSourceError::new(
            AudioSourceErrorKind::Contract,
            "parse YouTube playback source response",
        ))
    }
}
