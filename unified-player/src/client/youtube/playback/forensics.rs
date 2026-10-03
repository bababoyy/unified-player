use std::{
    sync::{
        atomic::{AtomicBool, AtomicU8, Ordering},
        Arc, Mutex,
    },
    time::Duration,
};

use reqwest::{header, Url};
use serde::Serialize;
use tokio_util::sync::CancellationToken;

use crate::config::{YouTubeMusicAuthType, YouTubePlaybackQuality};

use super::{
    format::{
        cipher_for_format, playability_error_kind, select_audio_format, AdaptiveFormat,
        PlayerAttemptAuthScope, PlayerResponse,
    },
    player::{
        configured_request_auth_kind, player_client_kind_name, player_version_source_name,
        PlayerClient, PlayerClientKind, PlayerVersionSource, RequestAuth, PLAYER_LANGUAGE,
        PLAYER_REGION,
    },
    source::{AudioSourceError, AudioSourceErrorKind},
};

#[cfg(feature = "private-capture")]
pub(super) const PRIVATE_CAPTURE_URL_LIMIT: usize = 64 * 1024;
#[cfg(feature = "private-capture")]
pub(super) const PRIVATE_CAPTURE_HEADER_COUNT_LIMIT: usize = 256;
#[cfg(feature = "private-capture")]
pub(super) const PRIVATE_CAPTURE_HEADER_BYTES_LIMIT: usize = 256 * 1024;

#[cfg(feature = "private-capture")]
#[derive(Clone, Debug, Serialize)]
pub(crate) struct YouTubePlayerClientInspection {
    pub(crate) context_name: &'static str,
    pub(crate) version: String,
    pub(crate) kind: &'static str,
    pub(crate) version_source: &'static str,
    pub(crate) signature_timestamp_present: bool,
    pub(crate) language: &'static str,
    pub(crate) region: &'static str,
}

#[cfg(feature = "private-capture")]
#[derive(Clone, Debug, Serialize)]
pub(crate) struct YouTubePlaybackAttemptInspection {
    pub(crate) client: YouTubePlayerClientInspection,
    pub(crate) proof_token_present: bool,
    pub(crate) signature_timestamp_present: bool,
    pub(crate) http_status: u16,
    pub(crate) playability_status: String,
    pub(crate) playability_reason: Option<String>,
    pub(crate) streaming_data_present: bool,
    pub(crate) adaptive_format_count: usize,
    pub(crate) direct_audio_count: usize,
    pub(crate) cipher_audio_count: usize,
    pub(crate) selected_audio_itag: Option<u64>,
    pub(crate) selection: String,
    pub(crate) error_category: Option<String>,
}

#[cfg(feature = "private-capture")]
#[derive(Debug, Serialize)]
pub(crate) struct YouTubePlaybackInspection {
    pub(crate) video_id: String,
    pub(crate) auth_kind: &'static str,
    pub(crate) client: YouTubePlayerClientInspection,
    pub(crate) proof_token_present: bool,
    pub(crate) signature_timestamp_present: bool,
    pub(crate) http_status: u16,
    pub(crate) playability_status: String,
    pub(crate) playability_reason: Option<String>,
    pub(crate) streaming_data_present: bool,
    pub(crate) adaptive_format_count: usize,
    pub(crate) direct_audio_count: usize,
    pub(crate) cipher_audio_count: usize,
    pub(crate) selected_audio_itag: Option<u64>,
    pub(crate) selection: String,
    pub(crate) client_matrix: Vec<YouTubePlaybackAttemptInspection>,
}

#[cfg(feature = "private-capture")]
pub(super) fn bounded_inspection_text(value: &str) -> String {
    value
        .chars()
        .map(|character| {
            if character.is_control() {
                ' '
            } else {
                character
            }
        })
        .take(512)
        .collect()
}

#[cfg(feature = "private-capture")]
pub(super) fn inspection_error_kind(
    auth_scope: PlayerAttemptAuthScope,
    kind: AudioSourceErrorKind,
) -> AudioSourceErrorKind {
    if auth_scope == PlayerAttemptAuthScope::Public && kind == AudioSourceErrorKind::Authentication
    {
        AudioSourceErrorKind::Unavailable
    } else {
        kind
    }
}

#[cfg(feature = "private-capture")]
pub(super) fn inspect_player_attempt(
    player_client: &PlayerClient,
    auth_scope: PlayerAttemptAuthScope,
    proof_token_present: bool,
    response: &PlayerResponse,
    quality: YouTubePlaybackQuality,
) -> YouTubePlaybackAttemptInspection {
    let streaming_data_present = response.streaming_data.is_some();
    let formats = response
        .streaming_data
        .as_ref()
        .map_or(&[][..], |streaming| streaming.adaptive_formats.as_slice());
    let audio_formats = formats
        .iter()
        .filter(|format| format.mime_type.starts_with("audio/"))
        .collect::<Vec<_>>();
    let direct_audio_count = audio_formats
        .iter()
        .filter(|format| format.url.is_some())
        .count();
    let cipher_audio_count = audio_formats
        .iter()
        .filter(|format| cipher_for_format(format).is_some())
        .count();
    let playability_status = response.playability_status.status.clone();
    let playability_reason = response
        .playability_status
        .reason
        .as_deref()
        .map(bounded_inspection_text);
    let (selected_audio_itag, selection) = if playability_status == "OK" {
        match select_audio_format(formats.to_vec(), quality, player_client.source_name) {
            Ok(source) => (Some(source.itag), "native_selected".to_owned()),
            Err(error) => (None, error.kind.diagnostic_category().as_str().to_owned()),
        }
    } else {
        (
            None,
            inspection_error_kind(
                auth_scope,
                playability_error_kind(&playability_status)
                    .unwrap_or(AudioSourceErrorKind::Unavailable),
            )
            .diagnostic_category()
            .as_str()
            .to_owned(),
        )
    };

    YouTubePlaybackAttemptInspection {
        client: inspect_player_client(player_client),
        proof_token_present,
        signature_timestamp_present: player_client.signature_timestamp.is_some(),
        http_status: response.http_status,
        playability_status: bounded_inspection_text(&playability_status),
        playability_reason,
        streaming_data_present,
        adaptive_format_count: formats.len(),
        direct_audio_count,
        cipher_audio_count,
        selected_audio_itag,
        selection,
        error_category: None,
    }
}

#[cfg(feature = "private-capture")]
pub(super) fn inspect_player_attempt_error(
    player_client: &PlayerClient,
    auth_scope: PlayerAttemptAuthScope,
    proof_token_present: bool,
    error: &AudioSourceError,
) -> YouTubePlaybackAttemptInspection {
    YouTubePlaybackAttemptInspection {
        client: inspect_player_client(player_client),
        proof_token_present,
        signature_timestamp_present: player_client.signature_timestamp.is_some(),
        http_status: 0,
        playability_status: "request_error".to_owned(),
        playability_reason: None,
        streaming_data_present: false,
        adaptive_format_count: 0,
        direct_audio_count: 0,
        cipher_audio_count: 0,
        selected_audio_itag: None,
        selection: inspection_error_kind(auth_scope, error.kind)
            .diagnostic_category()
            .as_str()
            .to_owned(),
        error_category: Some(
            inspection_error_kind(auth_scope, error.kind)
                .diagnostic_category()
                .as_str()
                .to_owned(),
        ),
    }
}

#[cfg(feature = "private-capture")]
pub(super) fn inspect_player_auth_failure(
    video_id: &str,
    auth_type: YouTubeMusicAuthType,
    clients: &[PlayerClient],
    error: &AudioSourceError,
) -> YouTubePlaybackInspection {
    let error_category = error.kind.diagnostic_category().as_str().to_owned();
    let client_matrix = clients
        .iter()
        .map(|client| YouTubePlaybackAttemptInspection {
            client: inspect_player_client(client),
            proof_token_present: false,
            signature_timestamp_present: client.signature_timestamp.is_some(),
            http_status: 0,
            playability_status: "request_error".to_owned(),
            playability_reason: None,
            streaming_data_present: false,
            adaptive_format_count: 0,
            direct_audio_count: 0,
            cipher_audio_count: 0,
            selected_audio_itag: None,
            selection: error_category.clone(),
            error_category: Some(error_category.clone()),
        })
        .collect::<Vec<_>>();
    let primary = client_matrix
        .first()
        .cloned()
        .expect("YouTube client matrix always includes the primary client");
    YouTubePlaybackInspection {
        video_id: bounded_inspection_text(video_id),
        auth_kind: configured_request_auth_kind(auth_type),
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
    }
}

#[cfg(feature = "private-capture")]
pub(super) fn inspect_player_client(player_client: &PlayerClient) -> YouTubePlayerClientInspection {
    YouTubePlayerClientInspection {
        context_name: player_client.context_name,
        version: bounded_inspection_text(&player_client.version),
        kind: player_client_kind_name(&player_client.kind),
        version_source: player_version_source_name(player_client.version_source),
        signature_timestamp_present: player_client.signature_timestamp.is_some(),
        language: PLAYER_LANGUAGE,
        region: PLAYER_REGION,
    }
}

#[cfg(feature = "private-capture")]
pub(super) fn capture_player_client_kind(
    client: &PlayerClient,
) -> crate::developer_capture::ProviderClientKind {
    match client.kind {
        PlayerClientKind::AndroidVr => crate::developer_capture::ProviderClientKind::Android,
        PlayerClientKind::VisionOs => crate::developer_capture::ProviderClientKind::Ios,
        PlayerClientKind::Tv => crate::developer_capture::ProviderClientKind::TvHtml5,
        PlayerClientKind::Web | PlayerClientKind::WebSafari => {
            crate::developer_capture::ProviderClientKind::Web
        }
        PlayerClientKind::WebRemix => crate::developer_capture::ProviderClientKind::WebRemix,
    }
}

#[cfg(feature = "private-capture")]
pub(super) fn bounded_private_header_map(headers: &header::HeaderMap) -> Option<header::HeaderMap> {
    private_headers_within_limits(headers).then(|| headers.clone())
}

#[cfg(feature = "private-capture")]
pub(super) fn private_headers_within_limits(headers: &header::HeaderMap) -> bool {
    if headers.len() > PRIVATE_CAPTURE_HEADER_COUNT_LIMIT {
        return false;
    }
    let mut total_bytes = 0_usize;
    for (name, value) in headers {
        let Some(next) = total_bytes
            .checked_add(name.as_str().len())
            .and_then(|bytes| bytes.checked_add(value.as_bytes().len()))
        else {
            return false;
        };
        total_bytes = next;
        if total_bytes > PRIVATE_CAPTURE_HEADER_BYTES_LIMIT {
            return false;
        }
    }
    true
}

#[cfg(feature = "private-capture")]
#[allow(clippy::too_many_arguments)]
pub(super) fn capture_player_http_response(
    capture: Option<&crate::developer_capture::CaptureSession>,
    exchange_ref: crate::developer_capture::ExchangeRef,
    player_client: &PlayerClient,
    status: reqwest::StatusCode,
    final_url: Option<&Url>,
    headers: &header::HeaderMap,
    body: &[u8],
    elapsed: Duration,
    body_complete: bool,
    redirected: bool,
) {
    let Some(capture) = capture else {
        return;
    };
    let Some(final_url) = final_url else {
        capture.note_incomplete(crate::developer_capture::IncompleteReason::Truncated);
        return;
    };
    capture.mark_credential_values_present();
    match crate::developer_capture::encode_private_http_response_bounded(
        status,
        final_url,
        headers,
        body,
        elapsed,
        body_complete,
        redirected,
        capture.body_limit(crate::developer_capture::CaptureRecordKind::HttpResponse),
    ) {
        Ok((payload, complete)) => {
            let _ = capture.record_with_context(
                Some(exchange_ref),
                crate::developer_capture::EndpointRole::PlayerApi,
                capture_player_client_kind(player_client),
                crate::developer_capture::TransportKind::NativeHttp,
                1,
                crate::developer_capture::CaptureRecordKind::HttpResponse,
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
pub(super) fn capture_auth_selection(
    capture: &crate::developer_capture::CaptureSession,
    exchange_ref: crate::developer_capture::ExchangeRef,
    auth: &RequestAuth,
    client: &PlayerClient,
    proof_token_present: bool,
    quality: YouTubePlaybackQuality,
    elapsed: Duration,
) {
    let auth_kind = match auth {
        RequestAuth::None => "none",
        RequestAuth::Browser(_) => "browser",
        RequestAuth::Bearer(_) => "oauth_bearer",
    };
    let client_kind = match client.kind {
        PlayerClientKind::AndroidVr => "android_vr",
        PlayerClientKind::VisionOs => "visionos",
        PlayerClientKind::Tv => "tv_html5",
        PlayerClientKind::Web | PlayerClientKind::WebSafari => "web",
        PlayerClientKind::WebRemix => "web_remix",
    };
    let version_source = match client.version_source {
        PlayerVersionSource::Static => "static",
        PlayerVersionSource::Cached => "cached",
        PlayerVersionSource::Discovered => "discovered",
        PlayerVersionSource::Fallback => "fallback",
    };
    if !matches!(auth, RequestAuth::None) || proof_token_present {
        capture.mark_credential_values_present();
    }
    let fields = [
        crate::developer_capture::PrivateField::text(
            crate::developer_capture::private_field::AUTH_KIND,
            auth_kind,
        ),
        crate::developer_capture::PrivateField::text(
            crate::developer_capture::private_field::CLIENT_KIND,
            client_kind,
        ),
        crate::developer_capture::PrivateField::text(
            crate::developer_capture::private_field::CLIENT_VERSION,
            &client.version,
        ),
        crate::developer_capture::PrivateField::text(
            crate::developer_capture::private_field::VERSION_SOURCE,
            version_source,
        ),
        crate::developer_capture::PrivateField::text(
            crate::developer_capture::private_field::USER_AGENT_PROFILE,
            client.source_name,
        ),
        crate::developer_capture::PrivateField::boolean(
            crate::developer_capture::private_field::PROOF_TOKEN_PRESENT,
            proof_token_present,
        ),
        crate::developer_capture::PrivateField::text(
            crate::developer_capture::private_field::QUALITY,
            capture_quality(quality),
        ),
        crate::developer_capture::PrivateField::u64(
            crate::developer_capture::private_field::ELAPSED_MS,
            u64::try_from(elapsed.as_millis()).unwrap_or(u64::MAX),
        ),
    ];
    match crate::developer_capture::encode_private_fields(
        crate::developer_capture::PrivatePayloadKind::AuthSelection,
        &fields,
    ) {
        Ok(payload) => {
            let _ = capture.record_with_context(
                Some(exchange_ref),
                crate::developer_capture::EndpointRole::PlayerApi,
                capture_player_client_kind(client),
                crate::developer_capture::TransportKind::NativeHttp,
                0,
                crate::developer_capture::CaptureRecordKind::AuthSelection,
                payload,
            );
        }
        Err(_) => {
            capture.note_incomplete(crate::developer_capture::IncompleteReason::Truncated);
        }
    }
}

#[cfg(feature = "private-capture")]
pub(super) fn capture_auth_failure(
    capture: Option<&crate::developer_capture::CaptureSession>,
    exchange_ref: crate::developer_capture::ExchangeRef,
    quality: YouTubePlaybackQuality,
    category: &'static str,
    elapsed: Duration,
) {
    let Some(capture) = capture else {
        return;
    };
    let fields = [
        crate::developer_capture::PrivateField::text(
            crate::developer_capture::private_field::AUTH_KIND,
            "unavailable",
        ),
        crate::developer_capture::PrivateField::text(
            crate::developer_capture::private_field::CATEGORY,
            category,
        ),
        crate::developer_capture::PrivateField::text(
            crate::developer_capture::private_field::QUALITY,
            capture_quality(quality),
        ),
        crate::developer_capture::PrivateField::u64(
            crate::developer_capture::private_field::ELAPSED_MS,
            u64::try_from(elapsed.as_millis()).unwrap_or(u64::MAX),
        ),
    ];
    record_private_fields(
        capture,
        exchange_ref,
        crate::developer_capture::ProviderClientKind::Unknown,
        crate::developer_capture::PrivatePayloadKind::AuthSelection,
        crate::developer_capture::CaptureRecordKind::AuthSelection,
        &fields,
    );
}

#[cfg(feature = "private-capture")]
pub(super) fn capture_network_failure(
    capture: Option<&crate::developer_capture::CaptureSession>,
    exchange_ref: Option<crate::developer_capture::ExchangeRef>,
    stage: &'static str,
    category: &'static str,
    error: Option<&reqwest::Error>,
) {
    let Some(capture) = capture else {
        return;
    };
    let fields = [
        crate::developer_capture::PrivateField::text(
            crate::developer_capture::private_field::STAGE,
            stage,
        ),
        crate::developer_capture::PrivateField::text(
            crate::developer_capture::private_field::CATEGORY,
            category,
        ),
        crate::developer_capture::PrivateField::boolean(
            crate::developer_capture::private_field::ERROR_IS_TIMEOUT,
            error.is_some_and(reqwest::Error::is_timeout),
        ),
        crate::developer_capture::PrivateField::boolean(
            crate::developer_capture::private_field::ERROR_IS_CONNECT,
            error.is_some_and(reqwest::Error::is_connect),
        ),
        crate::developer_capture::PrivateField::boolean(
            crate::developer_capture::private_field::ERROR_IS_REQUEST,
            error.is_some_and(reqwest::Error::is_request),
        ),
        crate::developer_capture::PrivateField::boolean(
            crate::developer_capture::private_field::ERROR_IS_BODY,
            error.is_some_and(reqwest::Error::is_body),
        ),
        crate::developer_capture::PrivateField::boolean(
            crate::developer_capture::private_field::ERROR_IS_DECODE,
            error.is_some_and(reqwest::Error::is_decode),
        ),
    ];
    match crate::developer_capture::encode_private_fields(
        crate::developer_capture::PrivatePayloadKind::NetworkFailure,
        &fields,
    ) {
        Ok(payload) => {
            let _ = capture.record_with_context(
                exchange_ref,
                crate::developer_capture::EndpointRole::PlayerApi,
                crate::developer_capture::ProviderClientKind::Unknown,
                crate::developer_capture::TransportKind::NativeHttp,
                1,
                crate::developer_capture::CaptureRecordKind::NetworkFailure,
                payload,
            );
        }
        Err(_) => {
            capture.note_incomplete(crate::developer_capture::IncompleteReason::Truncated);
        }
    }
}

#[cfg(feature = "private-capture")]
pub(super) fn capture_player_parse(
    capture: Option<&crate::developer_capture::CaptureSession>,
    exchange_ref: crate::developer_capture::ExchangeRef,
    player_client: &PlayerClient,
    response: &PlayerResponse,
    elapsed: Duration,
) {
    let Some(capture) = capture else {
        return;
    };
    let mut fields = vec![
        crate::developer_capture::PrivateField::text(
            crate::developer_capture::private_field::PLAYABILITY_STATUS,
            &response.playability_status.status,
        ),
        crate::developer_capture::PrivateField::boolean(
            crate::developer_capture::private_field::STREAMING_DATA_PRESENT,
            response.streaming_data.is_some(),
        ),
    ];
    if let Some(reason) = response.playability_status.reason.as_deref() {
        fields.push(crate::developer_capture::PrivateField::text(
            crate::developer_capture::private_field::PLAYABILITY_REASON,
            reason,
        ));
    }
    let category = playability_error_kind(&response.playability_status.status)
        .map_or("playable", |kind| kind.diagnostic_category().as_str());
    fields.push(crate::developer_capture::PrivateField::text(
        crate::developer_capture::private_field::CATEGORY,
        category,
    ));
    fields.push(crate::developer_capture::PrivateField::u64(
        crate::developer_capture::private_field::ELAPSED_MS,
        u64::try_from(elapsed.as_millis()).unwrap_or(u64::MAX),
    ));
    record_private_fields(
        capture,
        exchange_ref,
        capture_player_client_kind(player_client),
        crate::developer_capture::PrivatePayloadKind::PlayerParse,
        crate::developer_capture::CaptureRecordKind::PlayerParse,
        &fields,
    );
}

#[cfg(feature = "private-capture")]
pub(super) fn capture_player_parse_failure(
    capture: Option<&crate::developer_capture::CaptureSession>,
    exchange_ref: crate::developer_capture::ExchangeRef,
    player_client: &PlayerClient,
    elapsed: Duration,
) {
    let Some(capture) = capture else {
        return;
    };
    let fields = [
        crate::developer_capture::PrivateField::text(
            crate::developer_capture::private_field::PLAYABILITY_STATUS,
            "malformed",
        ),
        crate::developer_capture::PrivateField::text(
            crate::developer_capture::private_field::CATEGORY,
            "contract",
        ),
        crate::developer_capture::PrivateField::boolean(
            crate::developer_capture::private_field::STREAMING_DATA_PRESENT,
            false,
        ),
        crate::developer_capture::PrivateField::u64(
            crate::developer_capture::private_field::ELAPSED_MS,
            u64::try_from(elapsed.as_millis()).unwrap_or(u64::MAX),
        ),
    ];
    record_private_fields(
        capture,
        exchange_ref,
        capture_player_client_kind(player_client),
        crate::developer_capture::PrivatePayloadKind::PlayerParse,
        crate::developer_capture::CaptureRecordKind::PlayerParse,
        &fields,
    );
}

#[cfg(feature = "private-capture")]
pub(super) fn capture_format_inventory(
    capture: Option<&crate::developer_capture::CaptureSession>,
    exchange_ref: crate::developer_capture::ExchangeRef,
    player_client: &PlayerClient,
    formats: &[AdaptiveFormat],
) {
    const MAX_RECORDED_FORMATS: usize = 128;
    const MAX_FORMAT_FACT_BYTES: usize = 48 * 1024;

    let Some(capture) = capture else {
        return;
    };
    if formats.len() > MAX_RECORDED_FORMATS {
        capture.note_incomplete(crate::developer_capture::IncompleteReason::Truncated);
    }
    let mut facts = zeroize::Zeroizing::new(Vec::new());
    for format in formats.iter().take(MAX_RECORDED_FORMATS) {
        let before = facts.len();
        facts.extend_from_slice(&format.itag.to_be_bytes());
        facts.extend_from_slice(&format.bitrate.to_be_bytes());
        facts.push(u8::from(format.url.is_some()));
        facts.push(u8::from(cipher_for_format(format).is_some()));
        let mime = format.mime_type.as_bytes();
        let mime_len = u16::try_from(mime.len()).unwrap_or(u16::MAX);
        facts.extend_from_slice(&mime_len.to_be_bytes());
        facts.extend_from_slice(&mime[..usize::from(mime_len).min(mime.len())]);
        for value in [
            format.content_length.as_deref(),
            format.approx_duration_ms.as_deref(),
        ] {
            let value = value.unwrap_or_default().as_bytes();
            let value_len = u16::try_from(value.len()).unwrap_or(u16::MAX);
            facts.extend_from_slice(&value_len.to_be_bytes());
            facts.extend_from_slice(&value[..usize::from(value_len).min(value.len())]);
        }
        if facts.len() > MAX_FORMAT_FACT_BYTES {
            facts.truncate(before);
            capture.note_incomplete(crate::developer_capture::IncompleteReason::Truncated);
            break;
        }
    }
    let supported = formats
        .iter()
        .filter(|format| format.mime_type.starts_with("audio/mp4"))
        .count();
    let direct = formats.iter().filter(|format| format.url.is_some()).count();
    let cipher = formats
        .iter()
        .filter(|format| cipher_for_format(format).is_some())
        .count();
    let fields = [
        crate::developer_capture::PrivateField::u64(
            crate::developer_capture::private_field::RETURNED_FORMATS,
            u64::try_from(formats.len()).unwrap_or(u64::MAX),
        ),
        crate::developer_capture::PrivateField::u64(
            crate::developer_capture::private_field::SUPPORTED_FORMATS,
            u64::try_from(supported).unwrap_or(u64::MAX),
        ),
        crate::developer_capture::PrivateField::u64(
            crate::developer_capture::private_field::DIRECT_FORMATS,
            u64::try_from(direct).unwrap_or(u64::MAX),
        ),
        crate::developer_capture::PrivateField::u64(
            crate::developer_capture::private_field::CIPHER_FORMATS,
            u64::try_from(cipher).unwrap_or(u64::MAX),
        ),
        crate::developer_capture::PrivateField::bytes(
            crate::developer_capture::private_field::FORMAT_FACTS,
            &facts,
        ),
    ];
    record_private_fields(
        capture,
        exchange_ref,
        capture_player_client_kind(player_client),
        crate::developer_capture::PrivatePayloadKind::FormatInventory,
        crate::developer_capture::CaptureRecordKind::FormatInventory,
        &fields,
    );
}

#[cfg(feature = "private-capture")]
#[allow(clippy::too_many_arguments)]
pub(super) fn capture_selection_decision(
    capture: Option<&crate::developer_capture::CaptureSession>,
    exchange_ref: crate::developer_capture::ExchangeRef,
    player_client: &PlayerClient,
    quality: YouTubePlaybackQuality,
    selected_itag: Option<u64>,
    outcome: &'static str,
    fallback_eligible: bool,
    fallback_attempted: bool,
    elapsed: Duration,
) {
    let Some(capture) = capture else {
        return;
    };
    let mut fields = vec![
        crate::developer_capture::PrivateField::text(
            crate::developer_capture::private_field::QUALITY,
            capture_quality(quality),
        ),
        crate::developer_capture::PrivateField::text(
            crate::developer_capture::private_field::OUTCOME,
            outcome,
        ),
        crate::developer_capture::PrivateField::boolean(
            crate::developer_capture::private_field::FALLBACK_ELIGIBLE,
            fallback_eligible,
        ),
        crate::developer_capture::PrivateField::boolean(
            crate::developer_capture::private_field::FALLBACK_ATTEMPTED,
            fallback_attempted,
        ),
        crate::developer_capture::PrivateField::u64(
            crate::developer_capture::private_field::ELAPSED_MS,
            u64::try_from(elapsed.as_millis()).unwrap_or(u64::MAX),
        ),
    ];
    if let Some(itag) = selected_itag {
        fields.push(crate::developer_capture::PrivateField::u64(
            crate::developer_capture::private_field::SELECTED_ITAG,
            itag,
        ));
    }
    record_private_fields(
        capture,
        exchange_ref,
        capture_player_client_kind(player_client),
        crate::developer_capture::PrivatePayloadKind::SelectionDecision,
        crate::developer_capture::CaptureRecordKind::SelectionDecision,
        &fields,
    );
}

#[cfg(feature = "private-capture")]
pub(super) const fn capture_quality(quality: YouTubePlaybackQuality) -> &'static str {
    match quality {
        YouTubePlaybackQuality::High => "high",
        YouTubePlaybackQuality::DataSaver => "data_saver",
    }
}

#[cfg(feature = "private-capture")]
pub(super) fn record_private_fields(
    capture: &crate::developer_capture::CaptureSession,
    exchange_ref: crate::developer_capture::ExchangeRef,
    client_kind: crate::developer_capture::ProviderClientKind,
    payload_kind: crate::developer_capture::PrivatePayloadKind,
    record_kind: crate::developer_capture::CaptureRecordKind,
    fields: &[crate::developer_capture::PrivateField<'_>],
) {
    match crate::developer_capture::encode_private_fields(payload_kind, fields) {
        Ok(payload) => {
            let _ = capture.record_with_context(
                Some(exchange_ref),
                crate::developer_capture::EndpointRole::PlayerApi,
                client_kind,
                crate::developer_capture::TransportKind::NativeHttp,
                1,
                record_kind,
                payload,
            );
        }
        Err(_) => {
            capture.note_incomplete(crate::developer_capture::IncompleteReason::Truncated);
        }
    }
}

#[cfg(feature = "private-capture")]
const RETAINED_MEDIA_REQUESTS: u8 = 8;

#[cfg(feature = "private-capture")]
#[derive(Clone)]
pub(super) struct MediaPrivateEvidence {
    capture: crate::developer_capture::CaptureSession,
    exchange_ref: crate::developer_capture::ExchangeRef,
    sampler: Arc<Mutex<MediaEvidenceSampler>>,
}

#[cfg(feature = "private-capture")]
#[derive(Default)]
struct MediaEvidenceSampler {
    observed: u64,
    retained: u8,
    dropped: u64,
    first_byte_recorded: bool,
    summary_recorded: bool,
    unrecovered_failure: bool,
}

#[cfg(feature = "private-capture")]
impl MediaPrivateEvidence {
    pub(super) fn new(
        capture: crate::developer_capture::CaptureSession,
        exchange_ref: crate::developer_capture::ExchangeRef,
    ) -> Self {
        Self {
            capture,
            exchange_ref,
            sampler: Arc::new(Mutex::new(MediaEvidenceSampler::default())),
        }
    }

    pub(super) fn admit_request(&self) -> (u8, bool, bool) {
        let mut sampler = self
            .sampler
            .lock()
            .expect("media evidence sampler mutex poisoned");
        sampler.observed = sampler.observed.saturating_add(1);
        let attempt = u8::try_from(sampler.observed).unwrap_or(u8::MAX);
        if sampler.retained < RETAINED_MEDIA_REQUESTS {
            sampler.retained = sampler.retained.saturating_add(1);
            (attempt, true, false)
        } else {
            sampler.dropped = sampler.dropped.saturating_add(1);
            (attempt, false, sampler.dropped.is_power_of_two())
        }
    }

    pub(super) fn note_first_byte(&self, attempt: u8) {
        let should_record = {
            let mut sampler = self
                .sampler
                .lock()
                .expect("media evidence sampler mutex poisoned");
            sampler.unrecovered_failure = false;
            if sampler.first_byte_recorded {
                false
            } else {
                sampler.first_byte_recorded = true;
                true
            }
        };
        if should_record {
            record_decode_stage(
                self,
                "first_media_byte",
                "completed",
                attempt,
                Duration::ZERO,
            );
        }
    }

    pub(super) fn dropped(&self) -> u64 {
        self.sampler
            .lock()
            .expect("media evidence sampler mutex poisoned")
            .dropped
    }

    pub(super) fn note_transport_progress(&self) {
        self.sampler
            .lock()
            .expect("media evidence sampler mutex poisoned")
            .unrecovered_failure = false;
    }

    pub(super) fn note_transport_failure(&self) {
        self.sampler
            .lock()
            .expect("media evidence sampler mutex poisoned")
            .unrecovered_failure = true;
    }

    pub(super) fn has_unrecovered_failure(&self) -> bool {
        self.sampler
            .lock()
            .expect("media evidence sampler mutex poisoned")
            .unrecovered_failure
    }

    pub(super) fn record_sampling_summary(&self, outcome: &'static str) {
        let should_record = {
            let mut sampler = self
                .sampler
                .lock()
                .expect("media evidence sampler mutex poisoned");
            if sampler.summary_recorded {
                false
            } else {
                sampler.summary_recorded = true;
                true
            }
        };
        if should_record {
            record_decode_stage(self, "media_range_retention", outcome, 0, Duration::ZERO);
        }
    }
}

#[cfg(feature = "private-capture")]
const SOURCE_ACTIVE: u8 = 0;
#[cfg(feature = "private-capture")]
const SOURCE_COMPLETED: u8 = 1;
#[cfg(feature = "private-capture")]
const SOURCE_CANCELLED: u8 = 2;
#[cfg(feature = "private-capture")]
const SOURCE_FAILED: u8 = 3;

#[cfg(feature = "private-capture")]
pub(super) struct MediaCaptureLifecycle {
    evidence: MediaPrivateEvidence,
    cancellation: CancellationToken,
    committed: AtomicBool,
    source_outcome: AtomicU8,
}

#[cfg(feature = "private-capture")]
impl MediaCaptureLifecycle {
    pub(super) fn new(
        evidence: MediaPrivateEvidence,
        cancellation: CancellationToken,
    ) -> Arc<Self> {
        Arc::new(Self {
            evidence,
            cancellation,
            committed: AtomicBool::new(false),
            source_outcome: AtomicU8::new(SOURCE_ACTIVE),
        })
    }

    pub(super) fn commit(&self) {
        self.committed.store(true, Ordering::Release);
        self.finish_if_ready();
    }

    pub(super) fn source_finished(&self, completed: bool) {
        let outcome = if self.cancellation.is_cancelled() {
            SOURCE_CANCELLED
        } else if self.evidence.has_unrecovered_failure() {
            SOURCE_FAILED
        } else if completed {
            SOURCE_COMPLETED
        } else {
            SOURCE_CANCELLED
        };
        if self
            .source_outcome
            .compare_exchange(SOURCE_ACTIVE, outcome, Ordering::AcqRel, Ordering::Acquire)
            .is_ok()
        {
            self.evidence
                .record_sampling_summary(if outcome == SOURCE_COMPLETED {
                    "completed"
                } else if outcome == SOURCE_FAILED {
                    "failed"
                } else {
                    "cancelled"
                });
        }
        self.finish_if_ready();
    }

    pub(super) fn finish_if_ready(&self) {
        if !self.committed.load(Ordering::Acquire) {
            return;
        }
        match self.source_outcome.load(Ordering::Acquire) {
            SOURCE_COMPLETED => {
                self.evidence
                    .capture
                    .finish(crate::developer_capture::SafeTerminalCategory::Success);
            }
            SOURCE_CANCELLED => {
                self.evidence
                    .capture
                    .finish(crate::developer_capture::SafeTerminalCategory::Cancelled);
            }
            SOURCE_FAILED => {
                self.evidence
                    .capture
                    .finish(crate::developer_capture::SafeTerminalCategory::Failed);
            }
            _ => {}
        }
    }
}

#[cfg(feature = "private-capture")]
pub(crate) struct MediaCaptureHandle {
    lifecycle: Arc<MediaCaptureLifecycle>,
}

#[cfg(feature = "private-capture")]
impl MediaCaptureHandle {
    pub(super) fn new(lifecycle: Arc<MediaCaptureLifecycle>) -> Self {
        Self { lifecycle }
    }

    pub(crate) fn record_audio_output(&self, outcome: &'static str, elapsed: Duration) {
        record_decode_stage(
            &self.lifecycle.evidence,
            "audio_output",
            outcome,
            0,
            elapsed,
        );
    }

    pub(crate) fn commit(&self) {
        self.lifecycle.commit();
    }
}

#[cfg(feature = "private-capture")]
pub(super) struct CapturedMediaSource {
    source: Box<dyn rodio::Source<Item = f32> + Send>,
    lifecycle: Arc<MediaCaptureLifecycle>,
    ended: bool,
}

#[cfg(feature = "private-capture")]
impl CapturedMediaSource {
    pub(super) fn new(
        source: Box<dyn rodio::Source<Item = f32> + Send>,
        lifecycle: Arc<MediaCaptureLifecycle>,
    ) -> Self {
        Self {
            source,
            lifecycle,
            ended: false,
        }
    }
}

#[cfg(feature = "private-capture")]
impl Iterator for CapturedMediaSource {
    type Item = f32;

    fn next(&mut self) -> Option<Self::Item> {
        let sample = self.source.next();
        if sample.is_none() && !self.ended {
            self.ended = true;
            self.lifecycle.source_finished(true);
        }
        sample
    }
}

#[cfg(feature = "private-capture")]
impl rodio::Source for CapturedMediaSource {
    fn current_span_len(&self) -> Option<usize> {
        self.source.current_span_len()
    }

    fn channels(&self) -> rodio::ChannelCount {
        self.source.channels()
    }

    fn sample_rate(&self) -> rodio::SampleRate {
        self.source.sample_rate()
    }

    fn total_duration(&self) -> Option<Duration> {
        self.source.total_duration()
    }

    fn try_seek(&mut self, position: Duration) -> Result<(), rodio::source::SeekError> {
        self.source.try_seek(position)
    }
}

#[cfg(feature = "private-capture")]
impl Drop for CapturedMediaSource {
    fn drop(&mut self) {
        self.lifecycle.source_finished(self.ended);
    }
}

#[cfg(feature = "private-capture")]
pub(super) fn record_decode_stage(
    evidence: &MediaPrivateEvidence,
    stage: &'static str,
    outcome: &'static str,
    attempt: u8,
    elapsed: Duration,
) {
    let fields = [
        crate::developer_capture::PrivateField::text(
            crate::developer_capture::private_field::STAGE,
            stage,
        ),
        crate::developer_capture::PrivateField::text(
            crate::developer_capture::private_field::OUTCOME,
            outcome,
        ),
        crate::developer_capture::PrivateField::u64(
            crate::developer_capture::private_field::ELAPSED_MS,
            u64::try_from(elapsed.as_millis()).unwrap_or(u64::MAX),
        ),
        crate::developer_capture::PrivateField::u64(
            crate::developer_capture::private_field::DROPPED_COUNT,
            evidence.dropped(),
        ),
    ];
    match crate::developer_capture::encode_private_fields(
        crate::developer_capture::PrivatePayloadKind::DecodeStage,
        &fields,
    ) {
        Ok(payload) => {
            let _ = evidence.capture.record_with_context(
                Some(evidence.exchange_ref),
                crate::developer_capture::EndpointRole::Decoder,
                crate::developer_capture::ProviderClientKind::Unknown,
                crate::developer_capture::TransportKind::MediaRange,
                attempt,
                crate::developer_capture::CaptureRecordKind::DecodeStage,
                payload,
            );
        }
        Err(_) => {
            evidence
                .capture
                .note_incomplete(crate::developer_capture::IncompleteReason::Truncated);
        }
    }
}

#[cfg(feature = "private-capture")]
#[allow(clippy::too_many_arguments)]
pub(super) fn record_media_probe(
    evidence: &MediaPrivateEvidence,
    attempt: u8,
    stage: &'static str,
    request: &reqwest::Request,
    range_start: u64,
    range_end: u64,
    response: Option<&reqwest::Response>,
    elapsed: Duration,
) {
    if request.url().as_str().len() > PRIVATE_CAPTURE_URL_LIMIT
        || !private_headers_within_limits(request.headers())
    {
        evidence
            .capture
            .note_incomplete(crate::developer_capture::IncompleteReason::Truncated);
        return;
    }
    let Ok(request_headers) = crate::developer_capture::encode_private_headers(request.headers())
    else {
        evidence
            .capture
            .note_incomplete(crate::developer_capture::IncompleteReason::Truncated);
        return;
    };
    let mut fields = vec![
        crate::developer_capture::PrivateField::text(
            crate::developer_capture::private_field::STAGE,
            stage,
        ),
        crate::developer_capture::PrivateField::text(
            crate::developer_capture::private_field::METHOD,
            request.method().as_str(),
        ),
        crate::developer_capture::PrivateField::text(
            crate::developer_capture::private_field::URL,
            request.url().as_str(),
        ),
        crate::developer_capture::PrivateField::bytes(
            crate::developer_capture::private_field::HEADERS,
            &request_headers,
        ),
        crate::developer_capture::PrivateField::u64(
            crate::developer_capture::private_field::RANGE_START,
            range_start,
        ),
        crate::developer_capture::PrivateField::u64(
            crate::developer_capture::private_field::RANGE_END,
            range_end,
        ),
        crate::developer_capture::PrivateField::u64(
            crate::developer_capture::private_field::REQUEST_ORDINAL,
            u64::from(attempt),
        ),
        crate::developer_capture::PrivateField::u64(
            crate::developer_capture::private_field::ELAPSED_MS,
            u64::try_from(elapsed.as_millis()).unwrap_or(u64::MAX),
        ),
    ];
    if let Some(response) = response {
        fields.push(crate::developer_capture::PrivateField::u64(
            crate::developer_capture::private_field::STATUS,
            u64::from(response.status().as_u16()),
        ));
        fields.push(crate::developer_capture::PrivateField::boolean(
            crate::developer_capture::private_field::CONTENT_RANGE_PRESENT,
            response.headers().contains_key(header::CONTENT_RANGE),
        ));
        if let Some(length) = response.content_length() {
            fields.push(crate::developer_capture::PrivateField::u64(
                crate::developer_capture::private_field::RESPONSE_LENGTH,
                length,
            ));
        }
        fields.push(crate::developer_capture::PrivateField::boolean(
            crate::developer_capture::private_field::REDIRECTED,
            response.url() != request.url(),
        ));
        if response.url().as_str().len() > PRIVATE_CAPTURE_URL_LIMIT {
            evidence
                .capture
                .note_incomplete(crate::developer_capture::IncompleteReason::Truncated);
            return;
        }
    }
    match crate::developer_capture::encode_private_fields(
        crate::developer_capture::PrivatePayloadKind::MediaProbe,
        &fields,
    ) {
        Ok(payload) => {
            evidence.capture.mark_credential_values_present();
            let _ = evidence.capture.record_with_context(
                Some(evidence.exchange_ref),
                crate::developer_capture::EndpointRole::Media,
                crate::developer_capture::ProviderClientKind::Unknown,
                crate::developer_capture::TransportKind::MediaRange,
                attempt,
                crate::developer_capture::CaptureRecordKind::MediaProbe,
                payload,
            );
        }
        Err(_) => {
            evidence
                .capture
                .note_incomplete(crate::developer_capture::IncompleteReason::Truncated);
        }
    }
}

#[cfg(feature = "private-capture")]
pub(super) fn record_media_failure(
    evidence: &MediaPrivateEvidence,
    attempt: u8,
    stage: &'static str,
    category: &'static str,
) {
    evidence.note_transport_failure();
    let fields = [
        crate::developer_capture::PrivateField::text(
            crate::developer_capture::private_field::STAGE,
            stage,
        ),
        crate::developer_capture::PrivateField::text(
            crate::developer_capture::private_field::CATEGORY,
            category,
        ),
        crate::developer_capture::PrivateField::u64(
            crate::developer_capture::private_field::REQUEST_ORDINAL,
            u64::from(attempt),
        ),
    ];
    if let Ok(payload) = crate::developer_capture::encode_private_fields(
        crate::developer_capture::PrivatePayloadKind::NetworkFailure,
        &fields,
    ) {
        let _ = evidence.capture.record_with_context(
            Some(evidence.exchange_ref),
            crate::developer_capture::EndpointRole::Media,
            crate::developer_capture::ProviderClientKind::Unknown,
            crate::developer_capture::TransportKind::MediaRange,
            attempt,
            crate::developer_capture::CaptureRecordKind::NetworkFailure,
            payload,
        );
    }
}
