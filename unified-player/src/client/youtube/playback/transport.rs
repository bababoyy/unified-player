use std::{
    fmt,
    path::Path,
    sync::{
        atomic::{AtomicU64, Ordering},
        Arc, OnceLock,
    },
    time::{Duration, Instant},
};

use futures::StreamExt as _;
use reqwest::{header, Url};
use serde::Serialize;
use tokio_util::sync::CancellationToken;

#[cfg(feature = "private-capture")]
use super::forensics::{
    record_decode_stage, record_media_failure, record_media_probe, MediaPrivateEvidence,
};
use super::innertube::YouTubeProbeRecorder;
use super::{
    format::{validate_media_url, BrowserAudioTarget},
    player::{
        ANDROID_VR_USER_AGENT, BROWSER_SESSION_SOURCE_NAME, TV_CONTEXT_NAME, TV_USER_AGENT,
        VISIONOS_USER_AGENT, WEB_CONTEXT_NAME, WEB_MUSIC_CONTEXT_NAME, WEB_USER_AGENT,
    },
    source::{AudioSourceError, AudioSourceErrorKind, ResolvedAudioSource},
};

pub(super) const MEDIA_RANGE_CHUNK_BYTES: u64 = 1024 * 1024;
const MEDIA_PROBE_TIMEOUT: Duration = Duration::from_secs(3);
const MEDIA_REDIRECT_BODY_LIMIT: usize = 64 * 1024;
const MEDIA_REDIRECT_BODY_TIMEOUT: Duration = Duration::from_secs(1);

// The HTTP form is accepted only by localhost fixtures; production validation
// still requires an HTTPS Google Video URL.
const MEDIA_REDIRECT_PREFIXES: [&[u8]; 2] = [b"https://", b"http://"];

/// Return a bounded, URL-free classification for a media probe response.
///
/// The status class intentionally preserves the two statuses that matter to
/// the range contract (`200` and `206`) and the auth-specific `403`, while
/// keeping all other response codes in coarse classes.
pub(super) fn media_probe_response_class(
    status: reqwest::StatusCode,
    embedded_redirect: bool,
    valid_range: bool,
) -> &'static str {
    match status.as_u16() {
        200 if embedded_redirect => "http_200_embedded_redirect",
        200 => "http_200_range_contract",
        206 if valid_range => "http_206_valid_range",
        206 => "http_206_range_contract",
        403 => "http_403_forbidden",
        400..=499 => "http_4xx",
        500..=599 => "http_5xx",
        _ => "http_other",
    }
}

pub(super) fn decoder_media_probe_outcome(
    status: reqwest::StatusCode,
    valid_range: bool,
) -> (&'static str, Option<&'static str>) {
    if valid_range {
        ("success", None)
    } else if status == reqwest::StatusCode::FORBIDDEN {
        ("error", Some("media_forbidden"))
    } else {
        ("error", Some("media_range_contract"))
    }
}

fn record_media_probe_attempt(
    source_client: &'static str,
    elapsed: Duration,
    outcome: Option<crate::observability::OperationOutcome>,
    status_class: &'static str,
    error_type: Option<&'static str>,
) {
    crate::observability::operation_stage_detail(
        crate::observability::Component::YoutubeMusic,
        "youtube_media_probe_attempt",
        Some(elapsed),
        outcome,
        Some(source_client),
        Some(status_class),
        error_type,
        None,
    );
}

pub(super) fn media_url_failover_candidates(url: &Url) -> Vec<Url> {
    let Some(host) = url.host_str() else {
        return Vec::new();
    };
    let Some((route_prefix, _)) = host.split_once("---") else {
        return Vec::new();
    };
    let Some(media_nodes) = url
        .query_pairs()
        .find_map(|(key, value)| (key == "mn").then_some(value.into_owned()))
    else {
        return Vec::new();
    };

    let mut candidates = Vec::new();
    for node in media_nodes.split(',').filter(|node| !node.is_empty()) {
        let node_host = if node.ends_with(".googlevideo.com") {
            node.to_owned()
        } else {
            format!("{node}.googlevideo.com")
        };
        let candidate_host = if node_host.contains("---") {
            node_host
        } else {
            format!("{route_prefix}---{node_host}")
        };
        if candidate_host == host
            || candidates
                .iter()
                .any(|candidate: &Url| candidate.host_str() == Some(candidate_host.as_str()))
        {
            continue;
        }

        let mut candidate = url.clone();
        if candidate.set_host(Some(&candidate_host)).is_ok()
            && validate_media_url(&candidate).is_ok()
        {
            candidates.push(candidate);
        }
    }
    candidates
}

pub(super) fn media_probe_target(
    attempt_index: usize,
    advertised_url_count: usize,
) -> &'static str {
    if attempt_index == 0 {
        "primary"
    } else if attempt_index < advertised_url_count {
        "advertised_failover"
    } else {
        "embedded_redirect"
    }
}

pub(super) fn media_redirect_candidates(body: &[u8]) -> Vec<Url> {
    let mut candidates = Vec::new();
    let mut cursor = 0;
    while cursor < body.len() {
        let Some((offset, prefix)) = MEDIA_REDIRECT_PREFIXES
            .iter()
            .filter_map(|prefix| {
                body[cursor..]
                    .windows(prefix.len())
                    .position(|window| window == *prefix)
                    .map(|offset| (offset, *prefix))
            })
            .min_by_key(|(offset, _)| *offset)
        else {
            break;
        };
        let start = cursor + offset;
        let end = body[start..]
            .iter()
            .position(|byte| !is_media_url_byte(*byte))
            .map_or(body.len(), |offset| start + offset);
        if let Some(candidate) = std::str::from_utf8(&body[start..end])
            .ok()
            .and_then(|candidate| Url::parse(candidate).ok())
        {
            if candidate.path() == "/videoplayback"
                && validate_media_url(&candidate).is_ok()
                && !candidates.iter().any(|known: &Url| known == &candidate)
            {
                candidates.push(candidate);
            }
        }
        cursor = start + prefix.len();
    }
    candidates
}

fn is_media_url_byte(byte: u8) -> bool {
    byte.is_ascii_alphanumeric()
        || matches!(
            byte,
            b'-' | b'.'
                | b'_'
                | b'~'
                | b':'
                | b'/'
                | b'?'
                | b'#'
                | b'['
                | b']'
                | b'@'
                | b'!'
                | b'$'
                | b'&'
                | b'\''
                | b'('
                | b')'
                | b'*'
                | b'+'
                | b','
                | b';'
                | b'='
                | b'%'
        )
}

async fn read_media_redirect_body(
    response: reqwest::Response,
    cancellation: &CancellationToken,
) -> Result<Option<Vec<u8>>, AudioSourceError> {
    if response
        .content_length()
        .is_some_and(|length| length > MEDIA_REDIRECT_BODY_LIMIT as u64)
    {
        return Ok(None);
    }

    let mut stream = response.bytes_stream();
    let read_body = async {
        let mut body = Vec::new();
        while let Some(chunk) = stream.next().await {
            let chunk = chunk.map_err(|_| {
                AudioSourceError::new(
                    AudioSourceErrorKind::Network,
                    "read YouTube media redirect payload",
                )
            })?;
            if body.len().saturating_add(chunk.len()) > MEDIA_REDIRECT_BODY_LIMIT {
                return Ok(None);
            }
            body.extend_from_slice(&chunk);
        }
        Ok(Some(body))
    };

    tokio::select! {
        () = cancellation.cancelled() => Err(AudioSourceError::new(
            AudioSourceErrorKind::Cancelled,
            "YouTube media redirect payload read was cancelled",
        )),
        result = tokio::time::timeout(MEDIA_REDIRECT_BODY_TIMEOUT, read_body) => {
            result.map_err(|_| {
                AudioSourceError::new(
                    AudioSourceErrorKind::Network,
                    "YouTube media redirect payload read timed out",
                )
            })?
        }
    }
}

#[derive(Debug, Serialize)]
pub(crate) struct MediaTransportDiagnostic {
    request_only_current_status: u16,
    browser_status: u16,
    exact_replay_status: u16,
    raw_url_current_headers_status: u16,
    sanitized_url_browser_headers_status: u16,
    current_replay_status: u16,
    captured_query_shape: CapturedQueryShape,
    player_required_browser: bool,
}

#[derive(Debug, Serialize)]
struct CapturedQueryShape {
    ump: bool,
    srfvp: bool,
    range: bool,
}

impl CapturedQueryShape {
    fn from_keys(keys: &std::collections::BTreeSet<String>) -> Self {
        Self {
            ump: keys.contains("ump"),
            srfvp: keys.contains("srfvp"),
            range: keys.contains("range"),
        }
    }
}

impl fmt::Display for MediaTransportDiagnostic {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "request-only-current={} browser={} exact={} raw-current-headers={} sanitized-browser-headers={} current={} captured-query={{ump:{},srfvp:{},range:{}}} player-required-browser={}",
            self.request_only_current_status,
            self.browser_status,
            self.exact_replay_status,
            self.raw_url_current_headers_status,
            self.sanitized_url_browser_headers_status,
            self.current_replay_status,
            self.captured_query_shape.ump,
            self.captured_query_shape.srfvp,
            self.captured_query_shape.range,
            self.player_required_browser,
        )
    }
}

#[cfg(test)]
impl MediaTransportDiagnostic {
    pub(super) fn new_for_test(
        statuses: [u16; 6],
        captured_query_shape: [bool; 3],
        player_required_browser: bool,
    ) -> Self {
        Self {
            request_only_current_status: statuses[0],
            browser_status: statuses[1],
            exact_replay_status: statuses[2],
            raw_url_current_headers_status: statuses[3],
            sanitized_url_browser_headers_status: statuses[4],
            current_replay_status: statuses[5],
            captured_query_shape: CapturedQueryShape {
                ump: captured_query_shape[0],
                srfvp: captured_query_shape[1],
                range: captured_query_shape[2],
            },
            player_required_browser,
        }
    }
}

pub(super) async fn diagnose_browser_media_target(
    config_folder: &Path,
    video_id: &str,
    target: BrowserAudioTarget,
    player_required_browser: bool,
    cancellation: &CancellationToken,
) -> Result<MediaTransportDiagnostic, AudioSourceError> {
    let length = target.content_length.ok_or_else(|| {
        AudioSourceError::new(
            AudioSourceErrorKind::Contract,
            "YouTube returned diagnostic audio without a content length",
        )
    })?;
    let start = MEDIA_RANGE_CHUNK_BYTES.min(length / 2);
    let end = start
        .saturating_add(64 * 1024 - 1)
        .min(length.saturating_sub(1));
    let mut current_base_headers = header::HeaderMap::new();
    current_base_headers.insert(
        header::USER_AGENT,
        header::HeaderValue::from_static(super::super::browser_auth::BROWSER_MEDIA_USER_AGENT),
    );
    let diagnostic_client = reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::custom(|attempt| {
            if validate_media_url(attempt.url()).is_ok() {
                attempt.follow()
            } else {
                attempt.error("diagnostic media redirect left the HTTPS Google Video allowlist")
            }
        }))
        .build()
        .map_err(|_| {
            AudioSourceError::new(
                AudioSourceErrorKind::Network,
                "build YouTube media transport diagnostic",
            )
        })?;
    let request_only_url = super::super::browser_auth::capture_playback_url(
        config_folder,
        video_id,
        target.itag,
        target.content_length,
        cancellation,
        false,
    )
    .await
    .map_err(|_| {
        AudioSourceError::new(
            AudioSourceErrorKind::Authentication,
            "the dedicated browser could not capture request-only diagnostic media",
        )
    })?;
    let request_only_current_status = diagnostic_request_status(
        &diagnostic_client,
        request_only_url,
        media_continuation_headers(&current_base_headers, start, end)?,
        cancellation,
    )
    .await?;
    let capture = super::super::browser_auth::capture_playback_diagnostic(
        config_folder,
        video_id,
        target.itag,
        target.content_length,
        cancellation,
    )
    .await
    .map_err(|_| {
        AudioSourceError::new(
            AudioSourceErrorKind::Authentication,
            "the dedicated browser could not capture diagnostic media",
        )
    })?;

    let query_keys = capture
        .original_url
        .query_pairs()
        .map(|(key, _)| key.into_owned())
        .collect::<std::collections::BTreeSet<_>>();
    let browser_status = capture.response_status.ok_or_else(|| {
        AudioSourceError::new(
            AudioSourceErrorKind::Contract,
            "the dedicated browser returned no diagnostic media status",
        )
    })?;
    let mut raw_current_headers = current_base_headers.clone();
    raw_current_headers.insert(
        header::ACCEPT_ENCODING,
        header::HeaderValue::from_static("identity"),
    );
    let browser_replay_headers = replayable_browser_headers(capture.request_headers);
    let sanitized_browser_headers =
        media_continuation_headers(&browser_replay_headers, start, end)?;
    let current_headers = media_continuation_headers(&current_base_headers, start, end)?;
    let exact_replay_status = diagnostic_request_status(
        &diagnostic_client,
        capture.original_url.clone(),
        browser_replay_headers,
        cancellation,
    )
    .await?;
    let raw_url_current_headers_status = diagnostic_request_status(
        &diagnostic_client,
        capture.original_url,
        raw_current_headers,
        cancellation,
    )
    .await?;
    let sanitized_url_browser_headers_status = diagnostic_request_status(
        &diagnostic_client,
        capture.sanitized_url.clone(),
        sanitized_browser_headers,
        cancellation,
    )
    .await?;
    let current_replay_status = diagnostic_request_status(
        &diagnostic_client,
        capture.sanitized_url,
        current_headers,
        cancellation,
    )
    .await?;

    Ok(MediaTransportDiagnostic {
        request_only_current_status,
        browser_status,
        exact_replay_status,
        raw_url_current_headers_status,
        sanitized_url_browser_headers_status,
        current_replay_status,
        captured_query_shape: CapturedQueryShape::from_keys(&query_keys),
        player_required_browser,
    })
}

pub(super) async fn verify_media_continuation(
    client: &reqwest::Client,
    source: &mut ResolvedAudioSource,
    cancellation: &CancellationToken,
    probe_recorder: Option<&YouTubeProbeRecorder>,
    #[cfg(feature = "private-capture")] capture: Option<&crate::developer_capture::CaptureSession>,
    #[cfg(feature = "private-capture")] exchange_ref: crate::developer_capture::ExchangeRef,
) -> Result<(), AudioSourceError> {
    let length = source.content_length.ok_or_else(|| {
        AudioSourceError::new(
            AudioSourceErrorKind::Contract,
            "YouTube returned audio without a content length",
        )
    })?;
    if length < 2 {
        return Err(AudioSourceError::new(
            AudioSourceErrorKind::Contract,
            "YouTube returned an empty audio source",
        ));
    }
    let start = MEDIA_RANGE_CHUNK_BYTES.min(length / 2);
    let end = start
        .saturating_add(64 * 1024 - 1)
        .min(length.saturating_sub(1));
    let headers = media_continuation_headers(&source.required_headers, start, end)?;
    let mut urls = std::iter::once(source.url.clone())
        .chain(media_url_failover_candidates(&source.url))
        .collect::<Vec<_>>();
    let advertised_url_count = urls.len();
    let mut last_network_error = None;
    let mut next_url_index = 0;

    while let Some(url) = urls.get(next_url_index).cloned() {
        let attempt_index = next_url_index;
        next_url_index += 1;
        let target = media_probe_target(attempt_index, advertised_url_count);
        let has_more_urls = next_url_index < urls.len();
        let request = client
            .get(url.clone())
            .headers(headers.clone())
            .build()
            .map_err(|_| {
                AudioSourceError::new(
                    AudioSourceErrorKind::Contract,
                    "build YouTube media continuation request",
                )
            })?;
        #[cfg(feature = "private-capture")]
        let evidence = capture
            .cloned()
            .map(|capture| MediaPrivateEvidence::new(capture, exchange_ref));
        #[cfg(feature = "private-capture")]
        let evidence_request = request.try_clone();
        let request_started = std::time::Instant::now();
        let record_attempt = |elapsed, outcome, status_class, error_type| {
            record_media_probe_attempt(
                source.source_client,
                elapsed,
                outcome,
                status_class,
                error_type,
            );
            if let Some(recorder) = probe_recorder {
                let result = match outcome {
                    Some(crate::observability::OperationOutcome::Success) => "success",
                    Some(crate::observability::OperationOutcome::Cancelled) => "cancelled",
                    Some(_) => "error",
                    None => "continue",
                };
                recorder.record_transport(
                    source.source_client,
                    attempt_index,
                    target,
                    result,
                    status_class,
                    error_type,
                    elapsed,
                );
            }
        };
        #[cfg(feature = "private-capture")]
        let attempt = (attempt_index.saturating_add(1)).min(u8::MAX as usize) as u8;
        let response = tokio::select! {
            () = cancellation.cancelled() => {
                record_attempt(
                    request_started.elapsed(),
                    Some(crate::observability::OperationOutcome::Cancelled),
                    "no_response_cancelled",
                    Some("cancelled"),
                );
                #[cfg(feature = "private-capture")]
                if let Some(evidence) = &evidence {
                    record_media_failure(evidence, attempt, "continuation_probe", "cancelled");
                }
                return Err(AudioSourceError::new(
                    AudioSourceErrorKind::Cancelled,
                    "YouTube media continuation check was cancelled",
                ));
            }
            response = tokio::time::timeout(MEDIA_PROBE_TIMEOUT, client.execute(request)) => {
                if let Ok(response) = response {
                    response.map_err(|_| {
                        record_attempt(
                            request_started.elapsed(),
                            Some(crate::observability::OperationOutcome::Error),
                            "no_response_network",
                            Some("network"),
                        );
                        #[cfg(feature = "private-capture")]
                        if let Some(evidence) = &evidence {
                            record_media_failure(evidence, attempt, "continuation_probe", "network");
                        }
                        AudioSourceError::new(
                            AudioSourceErrorKind::Network,
                            "verify YouTube media continuation",
                        )
                    })
                } else {
                    record_attempt(
                        request_started.elapsed(),
                        Some(crate::observability::OperationOutcome::Error),
                        "no_response_timeout",
                        Some("network"),
                    );
                    #[cfg(feature = "private-capture")]
                    if let Some(evidence) = &evidence {
                        record_media_failure(evidence, attempt, "continuation_probe", "timeout");
                    }
                    Err(AudioSourceError::new(
                        AudioSourceErrorKind::Network,
                        "YouTube media continuation probe timed out",
                    ))
                }
            }
        };
        let response = match response {
            Ok(response) => response,
            Err(error) if error.kind == AudioSourceErrorKind::Network && has_more_urls => {
                last_network_error = Some(error);
                continue;
            }
            Err(error) => return Err(error),
        };
        #[cfg(feature = "private-capture")]
        if let (Some(evidence), Some(request)) = (&evidence, evidence_request.as_ref()) {
            record_media_probe(
                evidence,
                attempt,
                "continuation_probe",
                request,
                start,
                end,
                Some(&response),
                request_started.elapsed(),
            );
        }
        if response.status() == reqwest::StatusCode::FORBIDDEN {
            record_attempt(
                request_started.elapsed(),
                Some(crate::observability::OperationOutcome::Error),
                media_probe_response_class(response.status(), false, false),
                Some("media_forbidden"),
            );
            #[cfg(feature = "private-capture")]
            if let Some(evidence) = &evidence {
                record_media_failure(evidence, attempt, "continuation_probe", "media_forbidden");
            }
            return Err(AudioSourceError::new(
                AudioSourceErrorKind::MediaForbidden,
                "YouTube rejected the media continuation token; run `unified-player youtube browser-login` if the dedicated profile is no longer signed in",
            ));
        }
        let response_status = response.status();
        if response_status == reqwest::StatusCode::OK {
            let redirect_body = match read_media_redirect_body(response, cancellation).await {
                Ok(body) => body,
                Err(error) => {
                    record_attempt(
                        request_started.elapsed(),
                        Some(if error.kind == AudioSourceErrorKind::Cancelled {
                            crate::observability::OperationOutcome::Cancelled
                        } else {
                            crate::observability::OperationOutcome::Error
                        }),
                        if error.kind == AudioSourceErrorKind::Cancelled {
                            "http_200_redirect_cancelled"
                        } else {
                            "http_200_redirect_read_error"
                        },
                        Some(error.kind.diagnostic_category().as_str()),
                    );
                    return Err(error);
                }
            };
            let redirect_urls = redirect_body
                .as_deref()
                .map(media_redirect_candidates)
                .unwrap_or_default();
            let mut queued_redirect = false;
            for redirect_url in redirect_urls {
                if !urls.iter().any(|known| known == &redirect_url) {
                    urls.push(redirect_url);
                    queued_redirect = true;
                }
            }
            if queued_redirect {
                record_attempt(
                    request_started.elapsed(),
                    None,
                    media_probe_response_class(response_status, true, false),
                    None,
                );
                continue;
            }
            record_attempt(
                request_started.elapsed(),
                Some(crate::observability::OperationOutcome::Error),
                media_probe_response_class(response_status, false, false),
                Some("media_range_contract"),
            );
            #[cfg(feature = "private-capture")]
            if let Some(evidence) = &evidence {
                record_media_failure(evidence, attempt, "continuation_probe", "range_contract");
            }
            return Err(AudioSourceError::new(
                AudioSourceErrorKind::MediaRangeContract,
                "YouTube media continuation did not return a byte range",
            ));
        }
        if response_status != reqwest::StatusCode::PARTIAL_CONTENT {
            record_attempt(
                request_started.elapsed(),
                Some(crate::observability::OperationOutcome::Error),
                media_probe_response_class(response_status, false, false),
                Some("media_range_contract"),
            );
            #[cfg(feature = "private-capture")]
            if let Some(evidence) = &evidence {
                record_media_failure(evidence, attempt, "continuation_probe", "range_contract");
            }
            return Err(AudioSourceError::new(
                AudioSourceErrorKind::MediaRangeContract,
                "YouTube media continuation did not return a byte range",
            ));
        }
        let range_starts_correctly = response
            .headers()
            .get(header::CONTENT_RANGE)
            .and_then(|value| value.to_str().ok())
            .is_some_and(|value| value.starts_with(&format!("bytes {start}-")));
        if !range_starts_correctly {
            record_attempt(
                request_started.elapsed(),
                Some(crate::observability::OperationOutcome::Error),
                media_probe_response_class(response_status, false, false),
                Some("media_range_contract"),
            );
            #[cfg(feature = "private-capture")]
            if let Some(evidence) = &evidence {
                record_media_failure(evidence, attempt, "continuation_probe", "range_contract");
            }
            return Err(AudioSourceError::new(
                AudioSourceErrorKind::MediaRangeContract,
                "YouTube media continuation returned an unexpected content range",
            ));
        }
        record_attempt(
            request_started.elapsed(),
            Some(crate::observability::OperationOutcome::Success),
            media_probe_response_class(response_status, false, true),
            None,
        );
        source.url = url;
        return Ok(());
    }

    Err(last_network_error.unwrap_or_else(|| {
        AudioSourceError::new(
            AudioSourceErrorKind::Network,
            "verify YouTube media continuation",
        )
    }))
}

#[derive(Clone)]
pub(super) struct MediaHttpClient {
    client: reqwest::Client,
    default_headers: header::HeaderMap,
    range_chunk_bytes: u64,
    probe: Option<DecoderMediaProbe>,
    #[cfg(feature = "private-capture")]
    evidence: Option<MediaPrivateEvidence>,
}

#[derive(Clone)]
struct DecoderMediaProbe {
    recorder: YouTubeProbeRecorder,
    client: &'static str,
    next_attempt: Arc<AtomicU64>,
}

impl DecoderMediaProbe {
    fn begin(&self) -> (u64, Instant) {
        (
            self.next_attempt.fetch_add(1, Ordering::Relaxed) + 1,
            Instant::now(),
        )
    }

    fn record_error(
        &self,
        attempt: u64,
        target: &'static str,
        status: &'static str,
        error_category: &'static str,
        started: Instant,
    ) {
        self.recorder.record_decoder_transport(
            self.client,
            attempt,
            target,
            "error",
            status,
            Some(error_category),
            started.elapsed(),
        );
    }

    fn record_response(
        &self,
        attempt: u64,
        target: &'static str,
        response: &reqwest::Response,
        range_start: u64,
        started: Instant,
    ) {
        let valid_range = response.status() == reqwest::StatusCode::PARTIAL_CONTENT
            && response
                .headers()
                .get(header::CONTENT_RANGE)
                .and_then(|value| value.to_str().ok())
                .is_some_and(|value| value.starts_with(&format!("bytes {range_start}-")));
        let status = media_probe_response_class(response.status(), false, valid_range);
        let (result, error_category) = decoder_media_probe_outcome(response.status(), valid_range);
        self.recorder.record_decoder_transport(
            self.client,
            attempt,
            target,
            result,
            status,
            error_category,
            started.elapsed(),
        );
    }
}

#[derive(Debug)]
pub(super) struct MediaHttpError(String);

impl fmt::Display for MediaHttpError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for MediaHttpError {}

pub(super) struct MediaResponse {
    response: reqwest::Response,
    #[cfg(feature = "private-capture")]
    evidence: Option<MediaPrivateEvidence>,
    #[cfg(feature = "private-capture")]
    attempt: u8,
}

#[derive(Debug)]
pub(super) struct MediaResponseError(reqwest::StatusCode);

impl fmt::Display for MediaResponseError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "media server returned HTTP {}", self.0.as_u16())
    }
}

impl std::error::Error for MediaResponseError {}

impl stream_download::source::DecodeError for MediaResponseError {
    async fn decode_error(self) -> String {
        self.to_string()
    }
}

impl stream_download::http::ClientResponse for MediaResponse {
    type ResponseError = MediaResponseError;
    type StreamError = MediaHttpError;
    type Headers = header::HeaderMap;

    fn content_length(&self) -> Option<u64> {
        media_response_length(self.response.headers()).or_else(|| self.response.content_length())
    }

    fn content_type(&self) -> Option<&str> {
        self.response
            .headers()
            .get(header::CONTENT_TYPE)
            .and_then(|value| value.to_str().ok())
    }

    fn headers(&self) -> Self::Headers {
        self.response.headers().clone()
    }

    fn into_result(self) -> Result<Self, Self::ResponseError> {
        if self.response.status().is_success() {
            Ok(self)
        } else {
            Err(MediaResponseError(self.response.status()))
        }
    }

    fn stream(
        self,
    ) -> Box<
        dyn futures::Stream<Item = Result<bytes::Bytes, Self::StreamError>> + Unpin + Send + Sync,
    > {
        #[cfg(feature = "private-capture")]
        let evidence = self.evidence;
        #[cfg(feature = "private-capture")]
        let attempt = self.attempt;
        Box::new(self.response.bytes_stream().map(move |result| {
            #[cfg(feature = "private-capture")]
            if let Some(evidence) = &evidence {
                match &result {
                    Ok(bytes) if !bytes.is_empty() => evidence.note_first_byte(attempt),
                    Err(_) => record_media_failure(evidence, attempt, "media_body", "network"),
                    Ok(_) => {}
                }
            }
            result.map_err(|_| MediaHttpError("media stream read failed".to_string()))
        }))
    }
}

impl MediaHttpClient {
    pub(super) fn new(
        mut default_headers: header::HeaderMap,
        #[cfg(feature = "private-capture")] evidence: Option<MediaPrivateEvidence>,
    ) -> anyhow::Result<Self> {
        default_headers
            .entry(header::ACCEPT_ENCODING)
            .or_insert(header::HeaderValue::from_static("identity"));
        Ok(Self {
            client: shared_media_http_client(),
            default_headers,
            range_chunk_bytes: MEDIA_RANGE_CHUNK_BYTES,
            probe: None,
            #[cfg(feature = "private-capture")]
            evidence,
        })
    }

    pub(super) fn with_probe(
        mut self,
        source_client: &'static str,
        recorder: Option<YouTubeProbeRecorder>,
    ) -> Self {
        self.probe = recorder.map(|recorder| DecoderMediaProbe {
            recorder,
            client: source_client,
            next_attempt: Arc::new(AtomicU64::new(0)),
        });
        self
    }

    pub(super) fn with_range_chunk_bytes(mut self, range_chunk_bytes: Option<u64>) -> Self {
        if let Some(range_chunk_bytes) = range_chunk_bytes {
            self.range_chunk_bytes = range_chunk_bytes.max(1);
        }
        self
    }
}

pub(super) fn shared_media_http_client() -> reqwest::Client {
    static CLIENT: OnceLock<reqwest::Client> = OnceLock::new();
    CLIENT
        .get_or_init(|| {
            reqwest::Client::builder()
                .redirect(reqwest::redirect::Policy::custom(|attempt| {
                    if validate_media_url(attempt.url()).is_ok() {
                        attempt.follow()
                    } else {
                        attempt.error("media redirect left the HTTPS Google Video allowlist")
                    }
                }))
                .build()
                .expect("static native YouTube media client configuration")
        })
        .clone()
}

pub(super) fn media_response_length(headers: &header::HeaderMap) -> Option<u64> {
    let content_range = headers.get(header::CONTENT_RANGE)?.to_str().ok()?;
    let total = content_range.rsplit_once('/')?.1;
    (total != "*").then(|| total.parse().ok()).flatten()
}

#[cfg(test)]
pub(super) fn bounded_media_range_end(start: u64, requested_end: Option<u64>) -> u64 {
    bounded_media_range_end_for_chunk(start, requested_end, MEDIA_RANGE_CHUNK_BYTES)
}

pub(super) fn bounded_media_range_end_for_chunk(
    start: u64,
    requested_end: Option<u64>,
    range_chunk_bytes: u64,
) -> u64 {
    let max_end = start.saturating_add(range_chunk_bytes.max(1) - 1);
    requested_end.unwrap_or(max_end).min(max_end)
}

pub(super) fn media_range_header(start: u64, end: u64) -> String {
    format!("bytes={start}-{end}")
}

pub(super) async fn diagnostic_request_status(
    client: &reqwest::Client,
    url: Url,
    headers: header::HeaderMap,
    cancellation: &CancellationToken,
) -> Result<u16, AudioSourceError> {
    let response = tokio::select! {
        () = cancellation.cancelled() => {
            return Err(AudioSourceError::new(
                AudioSourceErrorKind::Cancelled,
                "YouTube media transport diagnostic was cancelled",
            ));
        }
        response = client.get(url).headers(headers).send() => {
            response.map_err(|_| AudioSourceError::new(
                AudioSourceErrorKind::Network,
                "run YouTube media transport diagnostic",
            ))?
        }
    };
    Ok(response.status().as_u16())
}

pub(super) fn replayable_browser_headers(mut headers: header::HeaderMap) -> header::HeaderMap {
    for name in [
        header::HOST,
        header::CONTENT_LENGTH,
        header::CONNECTION,
        header::TRANSFER_ENCODING,
    ] {
        headers.remove(name);
    }
    headers
}

pub(super) fn media_continuation_headers(
    base: &header::HeaderMap,
    start: u64,
    end: u64,
) -> Result<header::HeaderMap, AudioSourceError> {
    let mut headers = base.clone();
    headers.insert(
        header::ACCEPT_ENCODING,
        header::HeaderValue::from_static("identity"),
    );
    headers.insert(
        header::RANGE,
        header::HeaderValue::from_str(&media_range_header(start, end)).map_err(|_| {
            AudioSourceError::new(
                AudioSourceErrorKind::Contract,
                "build YouTube media continuation range",
            )
        })?,
    );
    Ok(headers)
}

#[derive(Clone)]
pub(super) struct RedactedMediaUrl(pub(super) Url);

impl fmt::Display for RedactedMediaUrl {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("<redacted Google Video URL>")
    }
}

impl stream_download::http::Client for MediaHttpClient {
    type Url = RedactedMediaUrl;
    type Response = MediaResponse;
    type Error = MediaHttpError;
    type Headers = reqwest::header::HeaderMap;

    fn create() -> Self {
        Self::new(
            header::HeaderMap::new(),
            #[cfg(feature = "private-capture")]
            None,
        )
        .expect("static native YouTube media client configuration")
    }

    async fn get(&self, url: &Self::Url) -> Result<Self::Response, Self::Error> {
        let initial_end = self.range_chunk_bytes - 1;
        let probe_attempt = self.probe.as_ref().map(DecoderMediaProbe::begin);
        let request = self
            .client
            .get(url.0.clone())
            .headers(self.default_headers.clone())
            .header(reqwest::header::RANGE, media_range_header(0, initial_end))
            .build()
            .map_err(|_| {
                if let (Some(probe), Some((attempt, started))) = (&self.probe, probe_attempt) {
                    probe.record_error(
                        attempt,
                        "initial_range",
                        "request_build_failed",
                        "contract",
                        started,
                    );
                }
                MediaHttpError("media request construction failed".to_string())
            })?;
        #[cfg(feature = "private-capture")]
        let (attempt, retained, evidence_request) =
            self.evidence.as_ref().map_or((0, false, None), |evidence| {
                let (attempt, retained, drop_checkpoint) = evidence.admit_request();
                if drop_checkpoint {
                    record_decode_stage(
                        evidence,
                        "media_range_retention",
                        "overflow",
                        attempt,
                        Duration::ZERO,
                    );
                }
                (attempt, retained, request.try_clone())
            });
        #[cfg(feature = "private-capture")]
        let started = std::time::Instant::now();
        let response = self.client.execute(request).await.map_err(|_| {
            if let (Some(probe), Some((attempt, started))) = (&self.probe, probe_attempt) {
                probe.record_error(
                    attempt,
                    "initial_range",
                    "no_response_network",
                    "network",
                    started,
                );
            }
            #[cfg(feature = "private-capture")]
            if let Some(evidence) = &self.evidence {
                record_media_failure(evidence, attempt, "initial_range", "network");
            }
            MediaHttpError("media request failed".to_string())
        })?;
        if let (Some(probe), Some((attempt, started))) = (&self.probe, probe_attempt) {
            probe.record_response(attempt, "initial_range", &response, 0, started);
        }
        #[cfg(feature = "private-capture")]
        if let Some(evidence) = &self.evidence {
            if response.status().is_success() {
                evidence.note_transport_progress();
            } else {
                record_media_failure(
                    evidence,
                    attempt,
                    "initial_range",
                    if response.status() == reqwest::StatusCode::FORBIDDEN {
                        "media_forbidden"
                    } else {
                        "http_status"
                    },
                );
            }
        }
        #[cfg(feature = "private-capture")]
        if retained {
            if let (Some(evidence), Some(request)) = (&self.evidence, evidence_request.as_ref()) {
                record_media_probe(
                    evidence,
                    attempt,
                    "initial_range",
                    request,
                    0,
                    initial_end,
                    Some(&response),
                    started.elapsed(),
                );
            }
        }
        let has_content_range = response.headers().contains_key(header::CONTENT_RANGE);
        tracing::debug!(
            start = 0,
            end = initial_end,
            status = %response.status(),
            has_content_range,
            response_content_length = ?response.content_length(),
            "YouTube initial media range response"
        );
        Ok(MediaResponse {
            response,
            #[cfg(feature = "private-capture")]
            evidence: self.evidence.clone(),
            #[cfg(feature = "private-capture")]
            attempt,
        })
    }

    async fn get_range(
        &self,
        url: &Self::Url,
        start: u64,
        end: Option<u64>,
    ) -> Result<Self::Response, Self::Error> {
        let bounded_end = bounded_media_range_end_for_chunk(start, end, self.range_chunk_bytes);
        let probe_attempt = self.probe.as_ref().map(DecoderMediaProbe::begin);
        let request = self
            .client
            .get(url.0.clone())
            .headers(self.default_headers.clone())
            .header(
                reqwest::header::RANGE,
                media_range_header(start, bounded_end),
            )
            .build()
            .map_err(|_| {
                if let (Some(probe), Some((attempt, started))) = (&self.probe, probe_attempt) {
                    probe.record_error(
                        attempt,
                        "followup_range",
                        "request_build_failed",
                        "contract",
                        started,
                    );
                }
                MediaHttpError("media range construction failed".to_string())
            })?;
        #[cfg(feature = "private-capture")]
        let (attempt, retained, evidence_request) =
            self.evidence.as_ref().map_or((0, false, None), |evidence| {
                let (attempt, retained, drop_checkpoint) = evidence.admit_request();
                if drop_checkpoint {
                    record_decode_stage(
                        evidence,
                        "media_range_retention",
                        "overflow",
                        attempt,
                        Duration::ZERO,
                    );
                }
                (attempt, retained, request.try_clone())
            });
        #[cfg(feature = "private-capture")]
        let started = std::time::Instant::now();
        let response = self.client.execute(request).await.map_err(|_| {
            if let (Some(probe), Some((attempt, started))) = (&self.probe, probe_attempt) {
                probe.record_error(
                    attempt,
                    "followup_range",
                    "no_response_network",
                    "network",
                    started,
                );
            }
            #[cfg(feature = "private-capture")]
            if let Some(evidence) = &self.evidence {
                record_media_failure(evidence, attempt, "followup_range", "network");
            }
            MediaHttpError("media range request failed".to_string())
        })?;
        if let (Some(probe), Some((attempt, started))) = (&self.probe, probe_attempt) {
            probe.record_response(attempt, "followup_range", &response, start, started);
        }
        #[cfg(feature = "private-capture")]
        if let Some(evidence) = &self.evidence {
            if response.status().is_success() {
                evidence.note_transport_progress();
            } else {
                record_media_failure(
                    evidence,
                    attempt,
                    "followup_range",
                    if response.status() == reqwest::StatusCode::FORBIDDEN {
                        "media_forbidden"
                    } else {
                        "http_status"
                    },
                );
            }
        }
        #[cfg(feature = "private-capture")]
        if retained {
            if let (Some(evidence), Some(request)) = (&self.evidence, evidence_request.as_ref()) {
                record_media_probe(
                    evidence,
                    attempt,
                    "followup_range",
                    request,
                    start,
                    bounded_end,
                    Some(&response),
                    started.elapsed(),
                );
            }
        }
        let has_content_range = response.headers().contains_key(header::CONTENT_RANGE);
        tracing::debug!(
            start,
            end = bounded_end,
            status = %response.status(),
            has_content_range,
            response_content_length = ?response.content_length(),
            "YouTube follow-up media range response"
        );
        Ok(MediaResponse {
            response,
            #[cfg(feature = "private-capture")]
            evidence: self.evidence.clone(),
            #[cfg(feature = "private-capture")]
            attempt,
        })
    }
}

pub(super) fn prepare_resolved_source(source: &mut ResolvedAudioSource, video_id: &str) {
    if let Some(user_agent) = media_user_agent(source.source_client) {
        source.required_headers.insert(
            header::USER_AGENT,
            header::HeaderValue::from_static(user_agent),
        );
    }
    source.media_id = video_id.to_string();
}

fn media_user_agent(source_client: &str) -> Option<&'static str> {
    match source_client {
        "ANDROID_VR" => Some(ANDROID_VR_USER_AGENT),
        "VISIONOS" => Some(VISIONOS_USER_AGENT),
        TV_CONTEXT_NAME => Some(TV_USER_AGENT),
        WEB_CONTEXT_NAME | WEB_MUSIC_CONTEXT_NAME => Some(WEB_USER_AGENT),
        BROWSER_SESSION_SOURCE_NAME => Some(super::super::browser_auth::BROWSER_MEDIA_USER_AGENT),
        _ => None,
    }
}
