//! Deterministic private-capture replay primitives.
//!
//! Integration requirements:
//! - declare this module from `developer_capture/mod.rs` and re-export only the
//!   orchestration types needed by the `YouTube` adapter;
//! - build `ReplayRecipeParts` from validated private player records, never
//!   from Tier 1 diagnostics;
//! - implement `OfflineReplayAdapter` with the provider's pure parser/selector;
//! - implement `FreshSemanticReplayAdapter` by mapping
//!   `AllowlistedPlayerEndpoint::YouTubePlayer` to the existing private player
//!   endpoint and obtaining credentials only in `acquire_current`;
//! - persist the returned `ReplayLineage` and single terminal result in a child
//!   artifact without routing replay through the interactive capture claim.
//!
//! This module intentionally has no application client, player, queue, session,
//! library, browser, audio, or general HTTP client capability. Fresh replay can
//! acquire current provider material and issue one typed player POST only.

use std::{
    fmt,
    future::Future,
    panic::AssertUnwindSafe,
    pin::Pin,
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use futures::FutureExt as _;
use tokio_util::sync::CancellationToken;

use super::{
    model::{
        CapturePurpose, CaptureRecordKind, CaptureRef, EndpointRole, ExchangeRef, PrivateCaptureV1,
        ProviderClientKind, SensitiveBytes, SensitiveString, TransportKind,
    },
    payload::{decode_fields, field, DecodedPrivatePayload, PrivatePayloadKind},
};

pub(crate) const REPLAY_RECIPE_SCHEMA_VERSION: u16 = 1;
const MAX_MEDIA_ID_BYTES: usize = 128;
const MAX_PLAYER_RESPONSE_BYTES: usize = 2 * 1024 * 1024;
const MAX_RECIPE_LIFETIME: Duration = Duration::from_secs(24 * 60 * 60);
pub(crate) const MAX_FRESH_REPLAY_TIMEOUT: Duration = Duration::from_secs(30);
const FRESH_PLAYER_REQUEST_BUDGET: u8 = 1;

pub(crate) type ReplayFuture<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ReplayPlayerClient {
    AndroidVr,
    TvHtml5,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ReplayClientVersionPolicy {
    CurrentCompatible,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ReplayCredentialPolicy {
    CurrentConfigured,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ReplayAuthKind {
    None,
    Browser,
    OAuthBearer,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ReplayQualityPolicy {
    High,
    DataSaver,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ReplayParserOutcome {
    NotRun,
    Parsed,
    Malformed,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ReplayProviderOutcome {
    Playable,
    Authentication,
    ConsentAgeRegion,
    ProviderUnavailable,
    ProofToken,
    RateLimited,
    NetworkFailed,
    Contract,
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum ReplaySelectionOutcome {
    NotAttempted,
    Selected { itag: u64 },
    CipherOnly,
    UnsupportedFormat,
    NoDirectFormat,
}

impl fmt::Debug for ReplaySelectionOutcome {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NotAttempted => formatter.write_str("NotAttempted"),
            Self::Selected { .. } => formatter.write_str("Selected([private format])"),
            Self::CipherOnly => formatter.write_str("CipherOnly"),
            Self::UnsupportedFormat => formatter.write_str("UnsupportedFormat"),
            Self::NoDirectFormat => formatter.write_str("NoDirectFormat"),
        }
    }
}

/// Canonical parser and selector result. It contains no provider prose.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct ReplayDecision {
    pub(crate) parser: ReplayParserOutcome,
    pub(crate) provider: ReplayProviderOutcome,
    pub(crate) streaming_data_present: bool,
    pub(crate) returned_formats: u16,
    pub(crate) supported_formats: u16,
    pub(crate) direct_formats: u16,
    pub(crate) cipher_formats: u16,
    pub(crate) selection: ReplaySelectionOutcome,
}

impl ReplayDecision {
    fn validate(self) -> Result<(), ReplayRecipeValidationError> {
        if self.supported_formats > self.returned_formats
            || self.direct_formats > self.returned_formats
            || self.cipher_formats > self.returned_formats
        {
            return Err(ReplayRecipeValidationError::InvalidDecision);
        }

        match self.parser {
            ReplayParserOutcome::Malformed => {
                if self.provider != ReplayProviderOutcome::Contract
                    || self.streaming_data_present
                    || self.returned_formats != 0
                    || self.selection != ReplaySelectionOutcome::NotAttempted
                {
                    return Err(ReplayRecipeValidationError::InvalidDecision);
                }
            }
            ReplayParserOutcome::NotRun => {
                if self.streaming_data_present
                    || self.returned_formats != 0
                    || self.selection != ReplaySelectionOutcome::NotAttempted
                    || self.provider == ReplayProviderOutcome::Playable
                {
                    return Err(ReplayRecipeValidationError::InvalidDecision);
                }
            }
            ReplayParserOutcome::Parsed => {
                if self.provider == ReplayProviderOutcome::Playable {
                    if !self.streaming_data_present {
                        return Err(ReplayRecipeValidationError::InvalidDecision);
                    }
                    match self.selection {
                        ReplaySelectionOutcome::Selected { itag } => {
                            if itag == 0 || self.direct_formats == 0 {
                                return Err(ReplayRecipeValidationError::InvalidDecision);
                            }
                        }
                        ReplaySelectionOutcome::CipherOnly => {
                            if self.cipher_formats == 0 {
                                return Err(ReplayRecipeValidationError::InvalidDecision);
                            }
                        }
                        ReplaySelectionOutcome::UnsupportedFormat => {
                            if self.supported_formats != 0 {
                                return Err(ReplayRecipeValidationError::InvalidDecision);
                            }
                        }
                        ReplaySelectionOutcome::NoDirectFormat => {
                            if self.supported_formats == 0 {
                                return Err(ReplayRecipeValidationError::InvalidDecision);
                            }
                        }
                        ReplaySelectionOutcome::NotAttempted => {
                            return Err(ReplayRecipeValidationError::InvalidDecision);
                        }
                    }
                } else if self.selection != ReplaySelectionOutcome::NotAttempted {
                    return Err(ReplayRecipeValidationError::InvalidDecision);
                }
            }
        }
        Ok(())
    }
}

/// Version-independent inputs decoded from a private capture.
///
/// Deliberately absent: cookies, authorization, proof tokens, timestamps used
/// for signing, endpoint strings, HTTP methods, and arbitrary request bodies.
pub(crate) struct ReplayRecipeParts {
    pub(crate) parent_capture_ref: CaptureRef,
    pub(crate) source_exchange_ref: ExchangeRef,
    pub(crate) source_purpose: CapturePurpose,
    pub(crate) created_unix_ms: u64,
    pub(crate) expires_unix_ms: u64,
    pub(crate) media_id: SensitiveString,
    pub(crate) player_http_status: u16,
    pub(crate) player_response: SensitiveBytes,
    pub(crate) response_complete: bool,
    pub(crate) client: ReplayPlayerClient,
    pub(crate) auth: ReplayAuthKind,
    pub(crate) proof_token_present: bool,
    pub(crate) client_version_policy: ReplayClientVersionPolicy,
    pub(crate) credential_policy: ReplayCredentialPolicy,
    pub(crate) quality: ReplayQualityPolicy,
    pub(crate) expected_decision: ReplayDecision,
}

/// V1 replay recipe retained only inside private storage.
pub(crate) struct ReplayRecipeV1 {
    schema_version: u16,
    parts: ReplayRecipeParts,
}

impl ReplayRecipeV1 {
    pub(crate) fn new(parts: ReplayRecipeParts) -> Result<Self, ReplayRecipeValidationError> {
        let recipe = Self {
            schema_version: REPLAY_RECIPE_SCHEMA_VERSION,
            parts,
        };
        recipe.validate()?;
        Ok(recipe)
    }

    pub(crate) fn from_private_capture(
        capture: &PrivateCaptureV1,
    ) -> Result<Self, ReplayRecipeBuildError> {
        let parts = extract_recipe_parts(capture)?;
        Self::new(parts).map_err(ReplayRecipeBuildError::Validation)
    }

    /// Decoder entry point. Keep validation separate so an unknown version can
    /// produce the typed `unsupported_schema` replay outcome.
    pub(crate) const fn from_versioned_parts(
        schema_version: u16,
        parts: ReplayRecipeParts,
    ) -> Self {
        Self {
            schema_version,
            parts,
        }
    }

    pub(crate) fn validate(&self) -> Result<(), ReplayRecipeValidationError> {
        if self.schema_version != REPLAY_RECIPE_SCHEMA_VERSION {
            return Err(ReplayRecipeValidationError::UnsupportedSchema);
        }
        if self.parts.source_purpose != CapturePurpose::InteractivePlayback {
            return Err(ReplayRecipeValidationError::RecursiveReplay);
        }
        let media_id = self.parts.media_id.expose();
        if media_id.is_empty()
            || media_id.len() > MAX_MEDIA_ID_BYTES
            || !media_id
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_'))
        {
            return Err(ReplayRecipeValidationError::InvalidMediaId);
        }
        if !(100..=599).contains(&self.parts.player_http_status) {
            return Err(ReplayRecipeValidationError::InvalidHttpStatus);
        }
        if self.parts.player_response.len() > MAX_PLAYER_RESPONSE_BYTES {
            return Err(ReplayRecipeValidationError::ResponseTooLarge);
        }
        let lifetime_ms = self
            .parts
            .expires_unix_ms
            .checked_sub(self.parts.created_unix_ms)
            .ok_or(ReplayRecipeValidationError::InvalidLifetime)?;
        if lifetime_ms == 0
            || lifetime_ms > u64::try_from(MAX_RECIPE_LIFETIME.as_millis()).unwrap_or(u64::MAX)
        {
            return Err(ReplayRecipeValidationError::InvalidLifetime);
        }
        self.parts.expected_decision.validate()
    }

    pub(crate) const fn schema_version(&self) -> u16 {
        self.schema_version
    }

    pub(crate) const fn parent_capture_ref(&self) -> CaptureRef {
        self.parts.parent_capture_ref
    }

    pub(crate) const fn source_exchange_ref(&self) -> ExchangeRef {
        self.parts.source_exchange_ref
    }

    pub(crate) const fn expected_decision(&self) -> ReplayDecision {
        self.parts.expected_decision
    }

    pub(crate) const fn response_complete(&self) -> bool {
        self.parts.response_complete
    }

    fn expired_at(&self, unix_ms: u64) -> bool {
        unix_ms >= self.parts.expires_unix_ms
    }
}

impl fmt::Debug for ReplayRecipeV1 {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("ReplayRecipeV1([private])")
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ReplayRecipeValidationError {
    InvalidDecision,
    InvalidHttpStatus,
    InvalidLifetime,
    InvalidMediaId,
    RecursiveReplay,
    ResponseTooLarge,
    UnsupportedSchema,
}

impl fmt::Display for ReplayRecipeValidationError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("private replay recipe validation failed")
    }
}

impl std::error::Error for ReplayRecipeValidationError {}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ReplayRecipeIncomplete {
    MissingPlayerRequest,
    MissingPlayerResponse,
    MissingAuthSelection,
    MissingQuality,
    MissingParserDecision,
    MissingFormatInventory,
    MissingSelectionDecision,
    TruncatedPlayerRequest,
    TruncatedPlayerResponse,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ReplayRecipeBuildError {
    AmbiguousEvidence,
    Incomplete(ReplayRecipeIncomplete),
    InvalidEvidence,
    RecursiveReplay,
    Validation(ReplayRecipeValidationError),
}

impl fmt::Display for ReplayRecipeBuildError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("private replay recipe could not be constructed")
    }
}

impl std::error::Error for ReplayRecipeBuildError {}

fn extract_recipe_parts(
    capture: &PrivateCaptureV1,
) -> Result<ReplayRecipeParts, ReplayRecipeBuildError> {
    if capture.purpose() != CapturePurpose::InteractivePlayback {
        return Err(ReplayRecipeBuildError::RecursiveReplay);
    }

    let request = exactly_one_record(
        capture,
        |record| {
            record.kind() == CaptureRecordKind::HttpRequest
                && record.endpoint_role() == EndpointRole::PlayerApi
                && record.transport_kind() == TransportKind::NativeHttp
                && record.exchange_ref().is_some()
        },
        ReplayRecipeIncomplete::MissingPlayerRequest,
    )?;
    let exchange_ref = request
        .exchange_ref()
        .ok_or(ReplayRecipeBuildError::InvalidEvidence)?;
    let request_payload = typed_payload(request, PrivatePayloadKind::HttpRequest)?;
    validate_player_request(&request_payload)?;
    let request_body = complete_body(
        &request_payload,
        ReplayRecipeIncomplete::TruncatedPlayerRequest,
    )?;
    let media_id = media_id_from_request(request_body)?;
    let client = replay_client(request.client_kind())?;

    let response = exactly_one_record(
        capture,
        |record| {
            record.kind() == CaptureRecordKind::HttpResponse
                && record.endpoint_role() == EndpointRole::PlayerApi
                && record.transport_kind() == TransportKind::NativeHttp
                && record.exchange_ref() == Some(exchange_ref)
        },
        ReplayRecipeIncomplete::MissingPlayerResponse,
    )?;
    if response.client_kind() != request.client_kind() || response.attempt() != request.attempt() {
        return Err(ReplayRecipeBuildError::InvalidEvidence);
    }
    let response_payload = typed_payload(response, PrivatePayloadKind::HttpResponse)?;
    let status = response_payload
        .field_u64(field::STATUS)
        .and_then(|status| u16::try_from(status).ok())
        .filter(|status| (100..=599).contains(status))
        .ok_or(ReplayRecipeBuildError::InvalidEvidence)?;
    let response_body = complete_body(
        &response_payload,
        ReplayRecipeIncomplete::TruncatedPlayerResponse,
    )?;

    let auth_record = exactly_one_record(
        capture,
        |record| {
            record.kind() == CaptureRecordKind::AuthSelection
                && record.endpoint_role() == EndpointRole::PlayerApi
                && record.transport_kind() == TransportKind::NativeHttp
                && record.exchange_ref() == Some(exchange_ref)
        },
        ReplayRecipeIncomplete::MissingAuthSelection,
    )?;
    if auth_record.client_kind() != request.client_kind() {
        return Err(ReplayRecipeBuildError::InvalidEvidence);
    }
    let auth_payload = typed_payload(auth_record, PrivatePayloadKind::AuthSelection)?;
    let auth = match auth_payload.field_bytes(field::AUTH_KIND) {
        Some(b"none") => ReplayAuthKind::None,
        Some(b"browser") => ReplayAuthKind::Browser,
        Some(b"oauth_bearer") => ReplayAuthKind::OAuthBearer,
        _ => return Err(ReplayRecipeBuildError::InvalidEvidence),
    };
    let proof_token_present = auth_payload.field_bool(field::PROOF_TOKEN_PRESENT).ok_or(
        ReplayRecipeBuildError::Incomplete(ReplayRecipeIncomplete::MissingAuthSelection),
    )?;
    validate_client_fact(&auth_payload, client)?;
    let quality = extract_quality(capture, exchange_ref, &auth_payload)?;
    let expected_decision =
        extract_expected_decision(capture, exchange_ref, status, auth, proof_token_present)?;

    let lifetime_ms = u64::try_from(MAX_RECIPE_LIFETIME.as_millis()).unwrap_or(u64::MAX);
    let expires_unix_ms = capture
        .created_unix_ms()
        .checked_add(lifetime_ms)
        .ok_or(ReplayRecipeBuildError::InvalidEvidence)?;
    Ok(ReplayRecipeParts {
        parent_capture_ref: capture.capture_ref(),
        source_exchange_ref: exchange_ref,
        source_purpose: capture.purpose(),
        created_unix_ms: capture.created_unix_ms(),
        expires_unix_ms,
        media_id: SensitiveString::new(media_id),
        player_http_status: status,
        player_response: SensitiveBytes::new(response_body.to_vec()),
        response_complete: true,
        client,
        auth,
        proof_token_present,
        client_version_policy: ReplayClientVersionPolicy::CurrentCompatible,
        credential_policy: ReplayCredentialPolicy::CurrentConfigured,
        quality,
        expected_decision,
    })
}

fn exactly_one_record(
    capture: &PrivateCaptureV1,
    predicate: impl Fn(&super::model::CaptureRecordV1) -> bool,
    missing: ReplayRecipeIncomplete,
) -> Result<&super::model::CaptureRecordV1, ReplayRecipeBuildError> {
    let mut matching = capture.records().iter().filter(|record| predicate(record));
    let record = matching
        .next()
        .ok_or(ReplayRecipeBuildError::Incomplete(missing))?;
    if matching.next().is_some() {
        return Err(ReplayRecipeBuildError::AmbiguousEvidence);
    }
    Ok(record)
}

fn typed_payload(
    record: &super::model::CaptureRecordV1,
    expected: PrivatePayloadKind,
) -> Result<DecodedPrivatePayload, ReplayRecipeBuildError> {
    let payload =
        decode_fields(record.payload()).map_err(|_| ReplayRecipeBuildError::InvalidEvidence)?;
    if payload.kind() != expected {
        return Err(ReplayRecipeBuildError::InvalidEvidence);
    }
    Ok(payload)
}

fn validate_player_request(payload: &DecodedPrivatePayload) -> Result<(), ReplayRecipeBuildError> {
    if payload.field_bytes(field::METHOD) != Some(&b"POST"[..]) {
        return Err(ReplayRecipeBuildError::InvalidEvidence);
    }
    let url = payload
        .field_bytes(field::URL)
        .and_then(|url| std::str::from_utf8(url).ok())
        .and_then(|url| reqwest::Url::parse(url).ok())
        .ok_or(ReplayRecipeBuildError::InvalidEvidence)?;
    if url.scheme() != "https"
        || url.host_str() != Some("www.youtube.com")
        || url.port().is_some()
        || url.path() != "/youtubei/v1/player"
        || url.cannot_be_a_base()
        || !url.username().is_empty()
        || url.password().is_some()
        || url.fragment().is_some()
    {
        return Err(ReplayRecipeBuildError::InvalidEvidence);
    }
    Ok(())
}

fn complete_body(
    payload: &DecodedPrivatePayload,
    incomplete: ReplayRecipeIncomplete,
) -> Result<&[u8], ReplayRecipeBuildError> {
    let body = payload
        .field_bytes(field::BODY)
        .ok_or(ReplayRecipeBuildError::InvalidEvidence)?;
    let body_length = payload
        .field_u64(field::BODY_LENGTH)
        .ok_or(ReplayRecipeBuildError::InvalidEvidence)?;
    let retained_length = payload
        .field_u64(field::RETAINED_BODY_LENGTH)
        .ok_or(ReplayRecipeBuildError::InvalidEvidence)?;
    let complete = payload
        .field_bool(field::BODY_COMPLETE)
        .ok_or(ReplayRecipeBuildError::InvalidEvidence)?;
    if !complete
        || body_length != retained_length
        || retained_length != u64::try_from(body.len()).unwrap_or(u64::MAX)
    {
        return Err(ReplayRecipeBuildError::Incomplete(incomplete));
    }
    Ok(body)
}

fn media_id_from_request(body: &[u8]) -> Result<String, ReplayRecipeBuildError> {
    let value: serde_json::Value =
        serde_json::from_slice(body).map_err(|_| ReplayRecipeBuildError::InvalidEvidence)?;
    let media_id = value
        .as_object()
        .and_then(|body| body.get("videoId"))
        .and_then(serde_json::Value::as_str)
        .ok_or(ReplayRecipeBuildError::InvalidEvidence)?;
    if media_id.is_empty()
        || media_id.len() > MAX_MEDIA_ID_BYTES
        || !media_id
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_'))
    {
        return Err(ReplayRecipeBuildError::InvalidEvidence);
    }
    Ok(media_id.to_owned())
}

const fn replay_client(
    client: ProviderClientKind,
) -> Result<ReplayPlayerClient, ReplayRecipeBuildError> {
    match client {
        ProviderClientKind::Android => Ok(ReplayPlayerClient::AndroidVr),
        ProviderClientKind::TvHtml5 => Ok(ReplayPlayerClient::TvHtml5),
        _ => Err(ReplayRecipeBuildError::InvalidEvidence),
    }
}

fn validate_client_fact(
    payload: &DecodedPrivatePayload,
    client: ReplayPlayerClient,
) -> Result<(), ReplayRecipeBuildError> {
    let expected = match client {
        ReplayPlayerClient::AndroidVr => &b"android_vr"[..],
        ReplayPlayerClient::TvHtml5 => &b"tv_html5"[..],
    };
    if payload.field_bytes(field::CLIENT_KIND) != Some(expected) {
        return Err(ReplayRecipeBuildError::InvalidEvidence);
    }
    Ok(())
}

fn extract_quality(
    capture: &PrivateCaptureV1,
    exchange_ref: ExchangeRef,
    auth_payload: &DecodedPrivatePayload,
) -> Result<ReplayQualityPolicy, ReplayRecipeBuildError> {
    let mut quality = auth_payload
        .field_bytes(field::QUALITY)
        .map(parse_quality)
        .transpose()?;
    for record in capture.records().iter().filter(|record| {
        record.kind() == CaptureRecordKind::SelectionDecision
            && record.exchange_ref() == Some(exchange_ref)
            && record.endpoint_role() == EndpointRole::PlayerApi
    }) {
        let payload = typed_payload(record, PrivatePayloadKind::SelectionDecision)?;
        let Some(value) = payload.field_bytes(field::QUALITY) else {
            continue;
        };
        let candidate = parse_quality(value)?;
        if quality.is_some_and(|quality| quality != candidate) {
            return Err(ReplayRecipeBuildError::AmbiguousEvidence);
        }
        quality = Some(candidate);
    }
    quality.ok_or(ReplayRecipeBuildError::Incomplete(
        ReplayRecipeIncomplete::MissingQuality,
    ))
}

fn parse_quality(value: &[u8]) -> Result<ReplayQualityPolicy, ReplayRecipeBuildError> {
    match value {
        b"high" => Ok(ReplayQualityPolicy::High),
        b"data_saver" => Ok(ReplayQualityPolicy::DataSaver),
        _ => Err(ReplayRecipeBuildError::InvalidEvidence),
    }
}

fn extract_expected_decision(
    capture: &PrivateCaptureV1,
    exchange_ref: ExchangeRef,
    status: u16,
    auth: ReplayAuthKind,
    proof_token_present: bool,
) -> Result<ReplayDecision, ReplayRecipeBuildError> {
    if !(200..=299).contains(&status) {
        let provider = match status {
            429 => ReplayProviderOutcome::RateLimited,
            401 => match auth {
                ReplayAuthKind::None => ReplayProviderOutcome::ProviderUnavailable,
                ReplayAuthKind::Browser | ReplayAuthKind::OAuthBearer => {
                    ReplayProviderOutcome::Authentication
                }
            },
            403 if proof_token_present => ReplayProviderOutcome::ProofToken,
            403 => match auth {
                ReplayAuthKind::None => ReplayProviderOutcome::ProviderUnavailable,
                ReplayAuthKind::Browser | ReplayAuthKind::OAuthBearer => {
                    ReplayProviderOutcome::Authentication
                }
            },
            _ => ReplayProviderOutcome::NetworkFailed,
        };
        return Ok(ReplayDecision {
            parser: ReplayParserOutcome::NotRun,
            provider,
            streaming_data_present: false,
            returned_formats: 0,
            supported_formats: 0,
            direct_formats: 0,
            cipher_formats: 0,
            selection: ReplaySelectionOutcome::NotAttempted,
        });
    }

    let parse_record = exactly_one_record(
        capture,
        |record| {
            record.kind() == CaptureRecordKind::PlayerParse
                && record.exchange_ref() == Some(exchange_ref)
                && record.endpoint_role() == EndpointRole::PlayerApi
                && record.transport_kind() == TransportKind::NativeHttp
        },
        ReplayRecipeIncomplete::MissingParserDecision,
    )?;
    let parse_payload = typed_payload(parse_record, PrivatePayloadKind::PlayerParse)?;
    if parse_payload.field_bytes(field::PLAYABILITY_STATUS) == Some(&b"malformed"[..]) {
        return Ok(ReplayDecision {
            parser: ReplayParserOutcome::Malformed,
            provider: ReplayProviderOutcome::Contract,
            streaming_data_present: false,
            returned_formats: 0,
            supported_formats: 0,
            direct_formats: 0,
            cipher_formats: 0,
            selection: ReplaySelectionOutcome::NotAttempted,
        });
    }

    let streaming_data_present = parse_payload
        .field_bool(field::STREAMING_DATA_PRESENT)
        .ok_or(ReplayRecipeBuildError::InvalidEvidence)?;
    let mut provider = provider_outcome(
        parse_payload
            .field_bytes(field::CATEGORY)
            .ok_or(ReplayRecipeBuildError::InvalidEvidence)?,
    )?;
    let inventory_record = exactly_one_record(
        capture,
        |record| {
            record.kind() == CaptureRecordKind::FormatInventory
                && record.exchange_ref() == Some(exchange_ref)
                && record.endpoint_role() == EndpointRole::PlayerApi
                && record.transport_kind() == TransportKind::NativeHttp
        },
        ReplayRecipeIncomplete::MissingFormatInventory,
    )?;
    let inventory = typed_payload(inventory_record, PrivatePayloadKind::FormatInventory)?;
    let returned_formats = bounded_count(&inventory, field::RETURNED_FORMATS)?;
    let supported_formats = bounded_count(&inventory, field::SUPPORTED_FORMATS)?;
    let direct_formats = bounded_count(&inventory, field::DIRECT_FORMATS)?;
    let cipher_formats = bounded_count(&inventory, field::CIPHER_FORMATS)?;

    let selection_records = capture
        .records()
        .iter()
        .filter(|record| {
            record.kind() == CaptureRecordKind::SelectionDecision
                && record.exchange_ref() == Some(exchange_ref)
                && record.endpoint_role() == EndpointRole::PlayerApi
        })
        .collect::<Vec<_>>();
    if selection_records.is_empty() {
        return Err(ReplayRecipeBuildError::Incomplete(
            ReplayRecipeIncomplete::MissingSelectionDecision,
        ));
    }

    let selection = if provider != ReplayProviderOutcome::Playable {
        ReplaySelectionOutcome::NotAttempted
    } else if !streaming_data_present {
        provider = ReplayProviderOutcome::Contract;
        ReplaySelectionOutcome::NotAttempted
    } else {
        extract_selection(&selection_records, supported_formats)?
    };
    let decision = ReplayDecision {
        parser: ReplayParserOutcome::Parsed,
        provider,
        streaming_data_present,
        returned_formats,
        supported_formats,
        direct_formats,
        cipher_formats,
        selection,
    };
    decision
        .validate()
        .map_err(ReplayRecipeBuildError::Validation)?;
    Ok(decision)
}

fn bounded_count(payload: &DecodedPrivatePayload, tag: u16) -> Result<u16, ReplayRecipeBuildError> {
    payload
        .field_u64(tag)
        .and_then(|value| u16::try_from(value).ok())
        .ok_or(ReplayRecipeBuildError::InvalidEvidence)
}

fn provider_outcome(value: &[u8]) -> Result<ReplayProviderOutcome, ReplayRecipeBuildError> {
    match value {
        b"playable" => Ok(ReplayProviderOutcome::Playable),
        b"authentication" => Ok(ReplayProviderOutcome::Authentication),
        b"consent_age_region" => Ok(ReplayProviderOutcome::ConsentAgeRegion),
        b"provider_unavailable" => Ok(ReplayProviderOutcome::ProviderUnavailable),
        b"proof_token" => Ok(ReplayProviderOutcome::ProofToken),
        b"rate_limited" => Ok(ReplayProviderOutcome::RateLimited),
        b"network" => Ok(ReplayProviderOutcome::NetworkFailed),
        b"contract" => Ok(ReplayProviderOutcome::Contract),
        _ => Err(ReplayRecipeBuildError::InvalidEvidence),
    }
}

fn extract_selection(
    records: &[&super::model::CaptureRecordV1],
    supported_formats: u16,
) -> Result<ReplaySelectionOutcome, ReplayRecipeBuildError> {
    for record in records {
        let payload = typed_payload(record, PrivatePayloadKind::SelectionDecision)?;
        match payload.field_bytes(field::OUTCOME) {
            Some(b"native_selected") => {
                let itag = payload
                    .field_u64(field::SELECTED_ITAG)
                    .filter(|itag| *itag != 0)
                    .ok_or(ReplayRecipeBuildError::InvalidEvidence)?;
                return Ok(ReplaySelectionOutcome::Selected { itag });
            }
            Some(b"decipher") => return Ok(ReplaySelectionOutcome::CipherOnly),
            Some(b"unsupported_format") => return Ok(ReplaySelectionOutcome::UnsupportedFormat),
            Some(b"provider_unavailable") if supported_formats > 0 => {
                return Ok(ReplaySelectionOutcome::NoDirectFormat)
            }
            Some(b"contract") => return Ok(ReplaySelectionOutcome::NotAttempted),
            Some(
                b"browser_selected"
                | b"authentication"
                | b"consent_age_region"
                | b"proof_token"
                | b"network"
                | b"media_forbidden"
                | b"media_range_contract",
            ) => {}
            _ => return Err(ReplayRecipeBuildError::InvalidEvidence),
        }
    }
    Err(ReplayRecipeBuildError::Incomplete(
        ReplayRecipeIncomplete::MissingSelectionDecision,
    ))
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[allow(clippy::struct_field_names)]
pub(crate) struct ReplayLineage {
    parent_capture_ref: CaptureRef,
    child_capture_ref: CaptureRef,
    source_exchange_ref: ExchangeRef,
}

impl ReplayLineage {
    fn new(
        recipe: &ReplayRecipeV1,
        child_capture_ref: CaptureRef,
    ) -> Result<Self, ReplayStartError> {
        if recipe.parent_capture_ref() == child_capture_ref {
            return Err(ReplayStartError::RecursiveCapture);
        }
        Ok(Self {
            parent_capture_ref: recipe.parent_capture_ref(),
            child_capture_ref,
            source_exchange_ref: recipe.source_exchange_ref(),
        })
    }

    pub(crate) const fn parent_capture_ref(self) -> CaptureRef {
        self.parent_capture_ref
    }

    pub(crate) const fn child_capture_ref(self) -> CaptureRef {
        self.child_capture_ref
    }

    pub(crate) const fn source_exchange_ref(self) -> ExchangeRef {
        self.source_exchange_ref
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ReplayStartError {
    RecursiveCapture,
    RecursiveReplay,
}

impl fmt::Display for ReplayStartError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("private replay was rejected before admission")
    }
}

impl std::error::Error for ReplayStartError {}

pub(crate) struct OfflineReplayInput<'a> {
    player_http_status: u16,
    player_response: &'a [u8],
    client: ReplayPlayerClient,
    auth: ReplayAuthKind,
    proof_token_present: bool,
    quality: ReplayQualityPolicy,
}

impl OfflineReplayInput<'_> {
    pub(crate) const fn player_http_status(&self) -> u16 {
        self.player_http_status
    }

    pub(crate) const fn player_response(&self) -> &[u8] {
        self.player_response
    }

    pub(crate) const fn client(&self) -> ReplayPlayerClient {
        self.client
    }

    pub(crate) const fn auth(&self) -> ReplayAuthKind {
        self.auth
    }

    pub(crate) const fn proof_token_present(&self) -> bool {
        self.proof_token_present
    }

    pub(crate) const fn quality(&self) -> ReplayQualityPolicy {
        self.quality
    }
}

impl fmt::Debug for OfflineReplayInput<'_> {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("OfflineReplayInput")
            .field("player_http_status", &self.player_http_status)
            .field("player_response", &"[private]")
            .field("client", &self.client)
            .field("auth", &self.auth)
            .field("proof_token_present", &self.proof_token_present)
            .field("quality", &self.quality)
            .finish()
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum OfflineAdapterResult {
    Decision(ReplayDecision),
    Incomplete,
    UnsupportedSchema,
}

/// Pure replay hook. It receives response bytes and selection policy only.
pub(crate) trait OfflineReplayAdapter {
    fn replay(&self, input: OfflineReplayInput<'_>) -> OfflineAdapterResult;
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum OfflineReplayOutcome {
    Reproduced,
    Changed,
    UnsupportedSchema,
    Incomplete,
    Panicked,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum FreshReplayOutcome {
    Reproduced,
    ProviderChanged,
    AuthUnavailable,
    BrowserContended,
    ExpiredInput,
    NetworkFailed,
    Cancelled,
    TimedOut,
    Panicked,
    UnsupportedSchema,
    Inconclusive,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ReplayTerminalOutcome {
    Offline(OfflineReplayOutcome),
    Fresh(FreshReplayOutcome),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct ReplayTerminalResult {
    lineage: ReplayLineage,
    outcome: ReplayTerminalOutcome,
}

impl ReplayTerminalResult {
    pub(crate) const fn lineage(self) -> ReplayLineage {
        self.lineage
    }

    pub(crate) const fn outcome(self) -> ReplayTerminalOutcome {
        self.outcome
    }
}

/// An accepted replay has one terminal field, never a list or event stream.
#[derive(Debug)]
pub(crate) struct ReplayRunResult {
    terminal: ReplayTerminalResult,
    observed_decision: Option<ReplayDecision>,
}

impl ReplayRunResult {
    const fn new(
        lineage: ReplayLineage,
        outcome: ReplayTerminalOutcome,
        observed_decision: Option<ReplayDecision>,
    ) -> Self {
        Self {
            terminal: ReplayTerminalResult { lineage, outcome },
            observed_decision,
        }
    }

    #[allow(clippy::unused_self)]
    pub(crate) const fn terminal_count(&self) -> usize {
        1
    }

    pub(crate) const fn terminal(&self) -> ReplayTerminalResult {
        self.terminal
    }

    pub(crate) const fn observed_decision(&self) -> Option<ReplayDecision> {
        self.observed_decision
    }

    pub(crate) const fn into_terminal(self) -> ReplayTerminalResult {
        self.terminal
    }
}

pub(crate) fn run_offline_replay<A: OfflineReplayAdapter>(
    recipe: &ReplayRecipeV1,
    child_capture_ref: CaptureRef,
    adapter: &A,
) -> Result<ReplayRunResult, ReplayStartError> {
    let lineage = ReplayLineage::new(recipe, child_capture_ref)?;
    let validation = recipe.validate();
    if validation == Err(ReplayRecipeValidationError::RecursiveReplay) {
        return Err(ReplayStartError::RecursiveReplay);
    }
    if validation == Err(ReplayRecipeValidationError::UnsupportedSchema) {
        return Ok(ReplayRunResult::new(
            lineage,
            ReplayTerminalOutcome::Offline(OfflineReplayOutcome::UnsupportedSchema),
            None,
        ));
    }
    if validation.is_err() || !recipe.response_complete() {
        return Ok(ReplayRunResult::new(
            lineage,
            ReplayTerminalOutcome::Offline(OfflineReplayOutcome::Incomplete),
            None,
        ));
    }

    let input = OfflineReplayInput {
        player_http_status: recipe.parts.player_http_status,
        player_response: recipe.parts.player_response.expose(),
        client: recipe.parts.client,
        auth: recipe.parts.auth,
        proof_token_present: recipe.parts.proof_token_present,
        quality: recipe.parts.quality,
    };
    let replay = std::panic::catch_unwind(AssertUnwindSafe(|| adapter.replay(input)));
    let (outcome, observed) = match replay {
        Ok(OfflineAdapterResult::Decision(observed)) if observed.validate().is_ok() => {
            let outcome = if observed == recipe.expected_decision() {
                OfflineReplayOutcome::Reproduced
            } else {
                OfflineReplayOutcome::Changed
            };
            (outcome, Some(observed))
        }
        Ok(OfflineAdapterResult::UnsupportedSchema) => {
            (OfflineReplayOutcome::UnsupportedSchema, None)
        }
        Ok(OfflineAdapterResult::Incomplete | OfflineAdapterResult::Decision(_)) => {
            (OfflineReplayOutcome::Incomplete, None)
        }
        Err(_) => (OfflineReplayOutcome::Panicked, None),
    };
    Ok(ReplayRunResult::new(
        lineage,
        ReplayTerminalOutcome::Offline(outcome),
        observed,
    ))
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum AllowlistedPlayerEndpoint {
    YouTubePlayer,
}

impl AllowlistedPlayerEndpoint {
    pub(crate) const fn scheme(self) -> &'static str {
        match self {
            Self::YouTubePlayer => "https",
        }
    }

    pub(crate) const fn authority(self) -> &'static str {
        match self {
            Self::YouTubePlayer => "www.youtube.com",
        }
    }

    pub(crate) const fn path(self) -> &'static str {
        match self {
            Self::YouTubePlayer => "/youtubei/v1/player",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum AllowlistedReplayMethod {
    Post,
}

/// Unforgeable outside this module; authorizes one typed player POST.
#[derive(Debug)]
pub(crate) struct PlayerPostCapability {
    _private: (),
}

struct FreshRequestBudget {
    remaining: u8,
}

impl FreshRequestBudget {
    const fn new() -> Self {
        Self {
            remaining: FRESH_PLAYER_REQUEST_BUDGET,
        }
    }

    fn claim(&mut self) -> Option<PlayerPostCapability> {
        if self.remaining == 0 {
            return None;
        }
        self.remaining -= 1;
        Some(PlayerPostCapability { _private: () })
    }
}

#[derive(Clone, Copy, Debug)]
pub(crate) struct CurrentMaterialRequest {
    requested_at_unix_ms: u64,
    client: ReplayPlayerClient,
    version_policy: ReplayClientVersionPolicy,
    credential_policy: ReplayCredentialPolicy,
}

impl CurrentMaterialRequest {
    pub(crate) const fn requested_at_unix_ms(self) -> u64 {
        self.requested_at_unix_ms
    }

    pub(crate) const fn client(self) -> ReplayPlayerClient {
        self.client
    }

    pub(crate) const fn version_policy(self) -> ReplayClientVersionPolicy {
        self.version_policy
    }

    pub(crate) const fn credential_policy(self) -> ReplayCredentialPolicy {
        self.credential_policy
    }
}

#[derive(Clone, Copy)]
pub(crate) struct FreshPlayerRequest<'a> {
    lineage: ReplayLineage,
    endpoint: AllowlistedPlayerEndpoint,
    method: AllowlistedReplayMethod,
    media_id: &'a str,
    client: ReplayPlayerClient,
    quality: ReplayQualityPolicy,
    issued_at_unix_ms: u64,
}

impl FreshPlayerRequest<'_> {
    pub(crate) const fn lineage(self) -> ReplayLineage {
        self.lineage
    }

    pub(crate) const fn endpoint(self) -> AllowlistedPlayerEndpoint {
        self.endpoint
    }

    pub(crate) const fn method(self) -> AllowlistedReplayMethod {
        self.method
    }

    pub(crate) const fn media_id(&self) -> &str {
        self.media_id
    }

    pub(crate) const fn client(self) -> ReplayPlayerClient {
        self.client
    }

    pub(crate) const fn quality(self) -> ReplayQualityPolicy {
        self.quality
    }

    pub(crate) const fn issued_at_unix_ms(self) -> u64 {
        self.issued_at_unix_ms
    }
}

impl fmt::Debug for FreshPlayerRequest<'_> {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("FreshPlayerRequest")
            .field("lineage", &self.lineage)
            .field("endpoint", &self.endpoint)
            .field("method", &self.method)
            .field("media_id", &"[private]")
            .field("client", &self.client)
            .field("quality", &self.quality)
            .field("issued_at_unix_ms", &self.issued_at_unix_ms)
            .finish()
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum CurrentMaterialFailure {
    AuthUnavailable,
    BrowserContended,
    Inconclusive,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum FreshPlayerAdapterResult {
    Decision(ReplayDecision),
    NetworkFailed,
    Inconclusive,
}

/// Narrow provider boundary for one fresh semantic player request.
///
/// `CurrentMaterial` is produced inside this call and consumed by `post_player`;
/// no saved credential value exists in `ReplayRecipeV1` or enters the runner.
pub(crate) trait FreshSemanticReplayAdapter: Sync {
    type CurrentMaterial: Send;

    fn acquire_current(
        &self,
        request: CurrentMaterialRequest,
    ) -> ReplayFuture<'_, Result<Self::CurrentMaterial, CurrentMaterialFailure>>;

    fn post_player<'a>(
        &'a self,
        capability: PlayerPostCapability,
        request: FreshPlayerRequest<'a>,
        current: Self::CurrentMaterial,
    ) -> ReplayFuture<'a, FreshPlayerAdapterResult>;
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct FreshReplayPolicy {
    timeout: Duration,
}

impl FreshReplayPolicy {
    pub(crate) fn new(timeout: Duration) -> Result<Self, FreshReplayPolicyError> {
        if timeout.is_zero() || timeout > MAX_FRESH_REPLAY_TIMEOUT {
            return Err(FreshReplayPolicyError::InvalidTimeout);
        }
        Ok(Self { timeout })
    }

    pub(crate) const fn timeout(self) -> Duration {
        self.timeout
    }

    #[allow(clippy::unused_self)]
    pub(crate) const fn request_budget(self) -> u8 {
        FRESH_PLAYER_REQUEST_BUDGET
    }
}

impl Default for FreshReplayPolicy {
    fn default() -> Self {
        Self {
            timeout: MAX_FRESH_REPLAY_TIMEOUT,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum FreshReplayPolicyError {
    InvalidTimeout,
}

impl fmt::Display for FreshReplayPolicyError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("fresh replay policy is invalid")
    }
}

impl std::error::Error for FreshReplayPolicyError {}

pub(crate) trait ReplayClock: Send + Sync {
    fn unix_ms(&self) -> u64;
}

#[derive(Clone, Copy, Debug, Default)]
pub(crate) struct SystemReplayClock;

impl ReplayClock for SystemReplayClock {
    fn unix_ms(&self) -> u64 {
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_or(0, |duration| {
                u64::try_from(duration.as_millis()).unwrap_or(u64::MAX)
            })
    }
}

pub(crate) trait ReplayTimer: Sync {
    fn wait(&self, duration: Duration) -> ReplayFuture<'_, ()>;
}

#[derive(Clone, Copy, Debug, Default)]
pub(crate) struct TokioReplayTimer;

impl ReplayTimer for TokioReplayTimer {
    fn wait(&self, duration: Duration) -> ReplayFuture<'_, ()> {
        Box::pin(tokio::time::sleep(duration))
    }
}

pub(crate) async fn run_fresh_replay<A, C, T>(
    recipe: &ReplayRecipeV1,
    child_capture_ref: CaptureRef,
    adapter: &A,
    clock: &C,
    timer: &T,
    cancellation: &CancellationToken,
    policy: FreshReplayPolicy,
) -> Result<ReplayRunResult, ReplayStartError>
where
    A: FreshSemanticReplayAdapter,
    C: ReplayClock + ?Sized,
    T: ReplayTimer,
{
    let lineage = ReplayLineage::new(recipe, child_capture_ref)?;
    let validation = recipe.validate();
    if validation == Err(ReplayRecipeValidationError::RecursiveReplay) {
        return Err(ReplayStartError::RecursiveReplay);
    }
    if validation == Err(ReplayRecipeValidationError::UnsupportedSchema) {
        return Ok(ReplayRunResult::new(
            lineage,
            ReplayTerminalOutcome::Fresh(FreshReplayOutcome::UnsupportedSchema),
            None,
        ));
    }
    if validation.is_err() || !recipe.response_complete() {
        return Ok(ReplayRunResult::new(
            lineage,
            ReplayTerminalOutcome::Fresh(FreshReplayOutcome::Inconclusive),
            None,
        ));
    }

    let now = clock.unix_ms();
    if recipe.expired_at(now) {
        return Ok(ReplayRunResult::new(
            lineage,
            ReplayTerminalOutcome::Fresh(FreshReplayOutcome::ExpiredInput),
            None,
        ));
    }

    let work = async {
        let material_request = CurrentMaterialRequest {
            requested_at_unix_ms: now,
            client: recipe.parts.client,
            version_policy: ReplayClientVersionPolicy::CurrentCompatible,
            credential_policy: ReplayCredentialPolicy::CurrentConfigured,
        };
        let current = adapter
            .acquire_current(material_request)
            .await
            .map_err(map_current_material_failure)?;
        let issued_at_unix_ms = clock.unix_ms();
        let player_request = FreshPlayerRequest {
            lineage,
            endpoint: AllowlistedPlayerEndpoint::YouTubePlayer,
            method: AllowlistedReplayMethod::Post,
            media_id: recipe.parts.media_id.expose(),
            client: recipe.parts.client,
            quality: recipe.parts.quality,
            issued_at_unix_ms,
        };
        let mut request_budget = FreshRequestBudget::new();
        let capability = request_budget
            .claim()
            .ok_or(FreshReplayOutcome::Inconclusive)?;
        Ok::<FreshPlayerAdapterResult, FreshReplayOutcome>(
            adapter
                .post_player(capability, player_request, current)
                .await,
        )
    };
    let work = AssertUnwindSafe(work).catch_unwind();
    tokio::pin!(work);
    let timeout = timer.wait(policy.timeout());
    tokio::pin!(timeout);

    let (outcome, observed) = tokio::select! {
        biased;
        () = cancellation.cancelled() => (FreshReplayOutcome::Cancelled, None),
        result = &mut work => match result {
            Ok(result) => map_fresh_work_result(result, recipe.expected_decision()),
            Err(_) => (FreshReplayOutcome::Panicked, None),
        },
        () = &mut timeout => (FreshReplayOutcome::TimedOut, None),
    };
    Ok(ReplayRunResult::new(
        lineage,
        ReplayTerminalOutcome::Fresh(outcome),
        observed,
    ))
}

const fn map_current_material_failure(failure: CurrentMaterialFailure) -> FreshReplayOutcome {
    match failure {
        CurrentMaterialFailure::AuthUnavailable => FreshReplayOutcome::AuthUnavailable,
        CurrentMaterialFailure::BrowserContended => FreshReplayOutcome::BrowserContended,
        CurrentMaterialFailure::Inconclusive => FreshReplayOutcome::Inconclusive,
    }
}

fn map_fresh_work_result(
    result: Result<FreshPlayerAdapterResult, FreshReplayOutcome>,
    expected: ReplayDecision,
) -> (FreshReplayOutcome, Option<ReplayDecision>) {
    match result {
        Err(outcome) => (outcome, None),
        Ok(FreshPlayerAdapterResult::Decision(observed)) if observed.validate().is_ok() => {
            let outcome = if observed == expected {
                FreshReplayOutcome::Reproduced
            } else {
                FreshReplayOutcome::ProviderChanged
            };
            (outcome, Some(observed))
        }
        Ok(FreshPlayerAdapterResult::NetworkFailed) => (FreshReplayOutcome::NetworkFailed, None),
        Ok(FreshPlayerAdapterResult::Inconclusive | FreshPlayerAdapterResult::Decision(_)) => {
            (FreshReplayOutcome::Inconclusive, None)
        }
    }
}

#[cfg(test)]
mod tests {
    use std::{
        cell::Cell,
        sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering},
    };

    use static_assertions::assert_not_impl_any;

    use super::*;

    assert_not_impl_any!(ReplayRecipeV1: Clone, std::fmt::Display, serde::Serialize);
    assert_not_impl_any!(PlayerPostCapability: Clone, Copy);

    fn playable_decision() -> ReplayDecision {
        ReplayDecision {
            parser: ReplayParserOutcome::Parsed,
            provider: ReplayProviderOutcome::Playable,
            streaming_data_present: true,
            returned_formats: 2,
            supported_formats: 2,
            direct_formats: 1,
            cipher_formats: 1,
            selection: ReplaySelectionOutcome::Selected { itag: 140 },
        }
    }

    fn unavailable_decision() -> ReplayDecision {
        ReplayDecision {
            parser: ReplayParserOutcome::Parsed,
            provider: ReplayProviderOutcome::ProviderUnavailable,
            streaming_data_present: false,
            returned_formats: 0,
            supported_formats: 0,
            direct_formats: 0,
            cipher_formats: 0,
            selection: ReplaySelectionOutcome::NotAttempted,
        }
    }

    #[test]
    fn replay_debug_redacts_private_format_identifier() {
        let rendered = format!("{:?}", playable_decision());

        assert!(rendered.contains("Selected([private format])"));
        assert!(!rendered.contains("140"));
    }

    #[test]
    fn fresh_request_budget_issues_exactly_one_capability() {
        let mut budget = FreshRequestBudget::new();

        assert!(budget.claim().is_some());
        assert!(budget.claim().is_none());
    }

    fn recipe_parts() -> ReplayRecipeParts {
        ReplayRecipeParts {
            parent_capture_ref: CaptureRef::from_bytes([1; 16]),
            source_exchange_ref: ExchangeRef::from_bytes([2; 8]),
            source_purpose: CapturePurpose::InteractivePlayback,
            created_unix_ms: 1_000,
            expires_unix_ms: 2_000,
            media_id: SensitiveString::new("fixture_media-1".to_owned()),
            player_http_status: 200,
            player_response: SensitiveBytes::new(b"fixture private response".to_vec()),
            response_complete: true,
            client: ReplayPlayerClient::TvHtml5,
            auth: ReplayAuthKind::Browser,
            proof_token_present: true,
            client_version_policy: ReplayClientVersionPolicy::CurrentCompatible,
            credential_policy: ReplayCredentialPolicy::CurrentConfigured,
            quality: ReplayQualityPolicy::High,
            expected_decision: playable_decision(),
        }
    }

    fn recipe() -> ReplayRecipeV1 {
        ReplayRecipeV1::new(recipe_parts()).unwrap()
    }

    fn captured_player_fixture(include_auth_quality: bool) -> PrivateCaptureV1 {
        use super::super::{
            model::{
                CaptureCompleteness, CaptureRecordV1, ProviderClientKind, SafeOperationRef,
                SafeTerminalCategory,
            },
            payload::{encode_fields, encode_http_request, encode_http_response, PrivateField},
        };

        let exchange_ref = ExchangeRef::from_bytes([2; 8]);
        let mut auth_fields = vec![
            PrivateField::text(field::AUTH_KIND, "browser"),
            PrivateField::text(field::CLIENT_KIND, "tv_html5"),
            PrivateField::boolean(field::PROOF_TOKEN_PRESENT, false),
        ];
        if include_auth_quality {
            auth_fields.push(PrivateField::text(field::QUALITY, "high"));
        }
        let auth = encode_fields(PrivatePayloadKind::AuthSelection, &auth_fields).unwrap();
        let request = reqwest::Client::new()
            .post("https://www.youtube.com/youtubei/v1/player?key=public")
            .body(br#"{"videoId":"fixture_media-1"}"#.to_vec())
            .build()
            .unwrap();
        let request = encode_http_request(&request).unwrap();
        let response_body =
            br#"{"playabilityStatus":{"status":"OK"},"streamingData":{"adaptiveFormats":[]}}"#;
        let response = encode_http_response(
            reqwest::StatusCode::OK,
            &reqwest::Url::parse("https://www.youtube.com/youtubei/v1/player?key=public").unwrap(),
            &reqwest::header::HeaderMap::new(),
            response_body,
            Duration::from_millis(10),
            true,
        )
        .unwrap();
        let parse = encode_fields(
            PrivatePayloadKind::PlayerParse,
            &[
                PrivateField::text(field::PLAYABILITY_STATUS, "OK"),
                PrivateField::text(field::CATEGORY, "playable"),
                PrivateField::boolean(field::STREAMING_DATA_PRESENT, true),
            ],
        )
        .unwrap();
        let inventory = encode_fields(
            PrivatePayloadKind::FormatInventory,
            &[
                PrivateField::u64(field::RETURNED_FORMATS, 2),
                PrivateField::u64(field::SUPPORTED_FORMATS, 2),
                PrivateField::u64(field::DIRECT_FORMATS, 1),
                PrivateField::u64(field::CIPHER_FORMATS, 1),
            ],
        )
        .unwrap();
        let selection = encode_fields(
            PrivatePayloadKind::SelectionDecision,
            &[
                PrivateField::text(field::QUALITY, "high"),
                PrivateField::text(field::OUTCOME, "native_selected"),
                PrivateField::u64(field::SELECTED_ITAG, 140),
            ],
        )
        .unwrap();
        let context = |sequence, kind, payload| {
            CaptureRecordV1::with_context(
                sequence,
                u64::from(sequence),
                Some(exchange_ref),
                EndpointRole::PlayerApi,
                ProviderClientKind::TvHtml5,
                TransportKind::NativeHttp,
                if kind == CaptureRecordKind::AuthSelection {
                    0
                } else {
                    1
                },
                kind,
                payload,
            )
        };
        PrivateCaptureV1::new(
            CaptureRef::from_bytes([1; 16]),
            1_000,
            1_100,
            CapturePurpose::InteractivePlayback,
            SafeOperationRef::from_bytes([3; 4]),
            vec![
                context(0, CaptureRecordKind::AuthSelection, auth),
                context(1, CaptureRecordKind::HttpRequest, request),
                context(2, CaptureRecordKind::HttpResponse, response),
                context(3, CaptureRecordKind::PlayerParse, parse),
                context(4, CaptureRecordKind::FormatInventory, inventory),
                context(5, CaptureRecordKind::SelectionDecision, selection),
            ],
            CaptureCompleteness::Complete,
            Vec::new(),
            0,
            SafeTerminalCategory::Success,
        )
    }

    #[test]
    fn recipe_v1_validation_is_bounded_private_and_strict() {
        let recipe = recipe();
        assert_eq!(recipe.schema_version(), REPLAY_RECIPE_SCHEMA_VERSION);
        assert_eq!(format!("{recipe:?}"), "ReplayRecipeV1([private])");
        assert!(!format!("{recipe:?}").contains("fixture_media"));

        let unsupported = ReplayRecipeV1::from_versioned_parts(2, recipe_parts());
        assert_eq!(
            unsupported.validate(),
            Err(ReplayRecipeValidationError::UnsupportedSchema)
        );

        let mut invalid_media = recipe_parts();
        invalid_media.media_id = SensitiveString::new("not/a/video".to_owned());
        assert_eq!(
            ReplayRecipeV1::from_versioned_parts(1, invalid_media).validate(),
            Err(ReplayRecipeValidationError::InvalidMediaId)
        );

        let mut oversized = recipe_parts();
        oversized.player_response =
            SensitiveBytes::new(vec![0; MAX_PLAYER_RESPONSE_BYTES.saturating_add(1)]);
        assert_eq!(
            ReplayRecipeV1::from_versioned_parts(1, oversized).validate(),
            Err(ReplayRecipeValidationError::ResponseTooLarge)
        );

        let mut invalid_lifetime = recipe_parts();
        invalid_lifetime.expires_unix_ms = invalid_lifetime
            .created_unix_ms
            .saturating_add(u64::try_from(MAX_RECIPE_LIFETIME.as_millis()).unwrap())
            .saturating_add(1);
        assert_eq!(
            ReplayRecipeV1::from_versioned_parts(1, invalid_lifetime).validate(),
            Err(ReplayRecipeValidationError::InvalidLifetime)
        );

        let mut invalid_decision = recipe_parts();
        invalid_decision.expected_decision.supported_formats = 3;
        assert_eq!(
            ReplayRecipeV1::from_versioned_parts(1, invalid_decision).validate(),
            Err(ReplayRecipeValidationError::InvalidDecision)
        );
    }

    #[test]
    fn real_capture_records_extract_one_strict_private_recipe() {
        let capture = captured_player_fixture(true);
        let recipe = ReplayRecipeV1::from_private_capture(&capture).unwrap();
        assert_eq!(recipe.parts.media_id.expose(), "fixture_media-1");
        assert_eq!(recipe.parts.player_http_status, 200);
        assert_eq!(recipe.parts.auth, ReplayAuthKind::Browser);
        assert!(!recipe.parts.proof_token_present);
        assert_eq!(recipe.parts.quality, ReplayQualityPolicy::High);
        assert_eq!(recipe.parts.expected_decision, playable_decision());
        assert!(recipe.parts.response_complete);
        assert_eq!(
            recipe.parts.player_response.expose(),
            br#"{"playabilityStatus":{"status":"OK"},"streamingData":{"adaptiveFormats":[]}}"#
        );
    }

    #[test]
    fn quality_missing_before_selection_is_a_typed_incomplete_recipe() {
        let mut capture = captured_player_fixture(false);
        capture
            .records
            .retain(|record| record.kind() != CaptureRecordKind::SelectionDecision);
        assert_eq!(
            ReplayRecipeV1::from_private_capture(&capture).unwrap_err(),
            ReplayRecipeBuildError::Incomplete(ReplayRecipeIncomplete::MissingQuality)
        );
    }

    #[test]
    fn duplicate_player_requests_are_rejected_as_ambiguous() {
        let mut capture = captured_player_fixture(true);
        let duplicate_payload = reqwest::Client::new()
            .post("https://www.youtube.com/youtubei/v1/player?key=public")
            .body(br#"{"videoId":"fixture_media-1"}"#.to_vec())
            .build()
            .unwrap();
        let duplicate_payload =
            super::super::payload::encode_http_request(&duplicate_payload).unwrap();
        capture
            .records
            .push(super::super::model::CaptureRecordV1::with_context(
                6,
                6,
                Some(ExchangeRef::from_bytes([9; 8])),
                EndpointRole::PlayerApi,
                ProviderClientKind::TvHtml5,
                TransportKind::NativeHttp,
                1,
                CaptureRecordKind::HttpRequest,
                duplicate_payload,
            ));
        assert_eq!(
            ReplayRecipeV1::from_private_capture(&capture).unwrap_err(),
            ReplayRecipeBuildError::AmbiguousEvidence
        );
    }

    #[test]
    fn source_player_endpoint_is_hard_allowlisted() {
        for endpoint in [
            "http://www.youtube.com/youtubei/v1/player?key=public",
            "https://example.invalid/youtubei/v1/player?key=public",
            "https://www.youtube.com:444/youtubei/v1/player?key=public",
        ] {
            let mut capture = captured_player_fixture(true);
            let request = reqwest::Client::new()
                .post(endpoint)
                .body(br#"{"videoId":"fixture_media-1"}"#.to_vec())
                .build()
                .unwrap();
            let request = super::super::payload::encode_http_request(&request).unwrap();
            capture.records[1] = super::super::model::CaptureRecordV1::with_context(
                1,
                1,
                Some(ExchangeRef::from_bytes([2; 8])),
                EndpointRole::PlayerApi,
                ProviderClientKind::TvHtml5,
                TransportKind::NativeHttp,
                1,
                CaptureRecordKind::HttpRequest,
                request,
            );
            assert_eq!(
                ReplayRecipeV1::from_private_capture(&capture).unwrap_err(),
                ReplayRecipeBuildError::InvalidEvidence
            );
        }
    }

    #[test]
    fn pre_parse_403_uses_captured_auth_and_proof_classification() {
        let mut capture = captured_player_fixture(true);
        let response = super::super::payload::encode_http_response(
            reqwest::StatusCode::FORBIDDEN,
            &reqwest::Url::parse("https://www.youtube.com/youtubei/v1/player?key=public").unwrap(),
            &reqwest::header::HeaderMap::new(),
            b"forbidden",
            Duration::from_millis(10),
            true,
        )
        .unwrap();
        capture.records[2] = super::super::model::CaptureRecordV1::with_context(
            2,
            2,
            Some(ExchangeRef::from_bytes([2; 8])),
            EndpointRole::PlayerApi,
            ProviderClientKind::TvHtml5,
            TransportKind::NativeHttp,
            1,
            CaptureRecordKind::HttpResponse,
            response,
        );
        capture.records.truncate(3);

        let recipe = ReplayRecipeV1::from_private_capture(&capture).unwrap();
        assert_eq!(
            recipe.expected_decision(),
            ReplayDecision {
                parser: ReplayParserOutcome::NotRun,
                provider: ReplayProviderOutcome::Authentication,
                streaming_data_present: false,
                returned_formats: 0,
                supported_formats: 0,
                direct_formats: 0,
                cipher_formats: 0,
                selection: ReplaySelectionOutcome::NotAttempted,
            }
        );
    }

    struct FixtureOfflineAdapter {
        calls: Cell<usize>,
        result: OfflineAdapterResult,
    }

    impl OfflineReplayAdapter for FixtureOfflineAdapter {
        fn replay(&self, input: OfflineReplayInput<'_>) -> OfflineAdapterResult {
            self.calls.set(self.calls.get().saturating_add(1));
            assert_eq!(input.player_http_status(), 200);
            assert_eq!(input.player_response(), b"fixture private response");
            assert_eq!(input.client(), ReplayPlayerClient::TvHtml5);
            assert_eq!(input.auth(), ReplayAuthKind::Browser);
            assert!(input.proof_token_present());
            assert_eq!(input.quality(), ReplayQualityPolicy::High);
            self.result
        }
    }

    #[test]
    fn offline_replay_is_deterministic_network_free_and_terminal_once() {
        let recipe = recipe();
        let adapter = FixtureOfflineAdapter {
            calls: Cell::new(0),
            result: OfflineAdapterResult::Decision(playable_decision()),
        };
        for child in [[3; 16], [4; 16]] {
            let result = run_offline_replay(&recipe, CaptureRef::from_bytes(child), &adapter)
                .expect("offline replay starts");
            assert_eq!(result.terminal_count(), 1);
            assert_eq!(
                result.terminal().outcome(),
                ReplayTerminalOutcome::Offline(OfflineReplayOutcome::Reproduced)
            );
            assert_eq!(result.observed_decision(), Some(playable_decision()));
        }
        assert_eq!(adapter.calls.get(), 2);

        let changed = FixtureOfflineAdapter {
            calls: Cell::new(0),
            result: OfflineAdapterResult::Decision(unavailable_decision()),
        };
        let result = run_offline_replay(&recipe, CaptureRef::from_bytes([5; 16]), &changed)
            .expect("changed replay starts");
        assert_eq!(
            result.into_terminal().outcome(),
            ReplayTerminalOutcome::Offline(OfflineReplayOutcome::Changed)
        );
    }

    #[test]
    #[ignore = "local Phase 7 performance evidence"]
    fn maximum_player_response_offline_replay_stays_below_two_hundred_fifty_ms() {
        const PREFIX: &[u8] = br#"{"playabilityStatus":{"status":"OK"},"streamingData":{"adaptiveFormats":[]},"padding":""#;
        const SUFFIX: &[u8] = br#""}"#;

        let mut response = Vec::with_capacity(MAX_PLAYER_RESPONSE_BYTES);
        response.extend_from_slice(PREFIX);
        response.resize(MAX_PLAYER_RESPONSE_BYTES.saturating_sub(SUFFIX.len()), b'a');
        response.extend_from_slice(SUFFIX);
        assert_eq!(response.len(), MAX_PLAYER_RESPONSE_BYTES);
        let mut parts = recipe_parts();
        parts.player_response = SensitiveBytes::new(response);
        let recipe = ReplayRecipeV1::new(parts).unwrap();

        let started = std::time::Instant::now();
        let result = run_offline_replay(
            &recipe,
            CaptureRef::from_bytes([0x70; 16]),
            &crate::client::YouTubeOfflineReplayAdapter,
        )
        .unwrap();
        let elapsed = started.elapsed();
        eprintln!(
            "phase7.performance.offline_replay_ms={}",
            elapsed.as_millis()
        );
        assert_eq!(result.terminal_count(), 1);
        assert!(
            elapsed < Duration::from_millis(250),
            "maximum offline replay took {elapsed:?}"
        );
    }

    #[test]
    fn offline_adapter_panic_becomes_one_typed_terminal() {
        struct PanickingAdapter;

        impl OfflineReplayAdapter for PanickingAdapter {
            fn replay(&self, _input: OfflineReplayInput<'_>) -> OfflineAdapterResult {
                panic!("private fixture panic")
            }
        }

        let result = run_offline_replay(
            &recipe(),
            CaptureRef::from_bytes([41; 16]),
            &PanickingAdapter,
        )
        .unwrap();
        assert_eq!(result.terminal_count(), 1);
        assert_eq!(
            result.terminal().outcome(),
            ReplayTerminalOutcome::Offline(OfflineReplayOutcome::Panicked)
        );
    }

    #[test]
    fn offline_invalid_or_incomplete_input_never_reaches_the_adapter() {
        let adapter = FixtureOfflineAdapter {
            calls: Cell::new(0),
            result: OfflineAdapterResult::Decision(playable_decision()),
        };
        let unsupported = ReplayRecipeV1::from_versioned_parts(7, recipe_parts());
        let result =
            run_offline_replay(&unsupported, CaptureRef::from_bytes([6; 16]), &adapter).unwrap();
        assert_eq!(
            result.terminal().outcome(),
            ReplayTerminalOutcome::Offline(OfflineReplayOutcome::UnsupportedSchema)
        );

        let mut incomplete_parts = recipe_parts();
        incomplete_parts.response_complete = false;
        let incomplete = ReplayRecipeV1::from_versioned_parts(1, incomplete_parts);
        let result =
            run_offline_replay(&incomplete, CaptureRef::from_bytes([7; 16]), &adapter).unwrap();
        assert_eq!(
            result.terminal().outcome(),
            ReplayTerminalOutcome::Offline(OfflineReplayOutcome::Incomplete)
        );
        assert_eq!(adapter.calls.get(), 0);
    }

    #[derive(Clone, Copy)]
    enum AcquireBehavior {
        Ready(u64),
        Failure(CurrentMaterialFailure),
        Pending,
    }

    #[derive(Clone, Copy)]
    enum PostBehavior {
        Decision(ReplayDecision),
        NetworkFailed,
        Inconclusive,
        Pending,
        Panicked,
    }

    struct FixtureFreshAdapter {
        acquire_behavior: AcquireBehavior,
        post_behavior: PostBehavior,
        acquire_calls: AtomicUsize,
        post_calls: AtomicUsize,
        requested_at: AtomicU64,
        used_material: AtomicU64,
        saw_allowlist: AtomicBool,
    }

    impl FixtureFreshAdapter {
        fn new(acquire_behavior: AcquireBehavior, post_behavior: PostBehavior) -> Self {
            Self {
                acquire_behavior,
                post_behavior,
                acquire_calls: AtomicUsize::new(0),
                post_calls: AtomicUsize::new(0),
                requested_at: AtomicU64::new(0),
                used_material: AtomicU64::new(0),
                saw_allowlist: AtomicBool::new(false),
            }
        }
    }

    impl FreshSemanticReplayAdapter for FixtureFreshAdapter {
        type CurrentMaterial = u64;

        fn acquire_current<'a>(
            &'a self,
            request: CurrentMaterialRequest,
        ) -> ReplayFuture<'a, Result<Self::CurrentMaterial, CurrentMaterialFailure>> {
            self.acquire_calls.fetch_add(1, Ordering::SeqCst);
            self.requested_at
                .store(request.requested_at_unix_ms(), Ordering::SeqCst);
            assert_eq!(request.client(), ReplayPlayerClient::TvHtml5);
            assert_eq!(
                request.version_policy(),
                ReplayClientVersionPolicy::CurrentCompatible
            );
            assert_eq!(
                request.credential_policy(),
                ReplayCredentialPolicy::CurrentConfigured
            );
            let behavior = self.acquire_behavior;
            Box::pin(async move {
                match behavior {
                    AcquireBehavior::Ready(material) => Ok(material),
                    AcquireBehavior::Failure(failure) => Err(failure),
                    AcquireBehavior::Pending => {
                        std::future::pending::<Result<u64, CurrentMaterialFailure>>().await
                    }
                }
            })
        }

        fn post_player<'a>(
            &'a self,
            _capability: PlayerPostCapability,
            request: FreshPlayerRequest<'a>,
            current: Self::CurrentMaterial,
        ) -> ReplayFuture<'a, FreshPlayerAdapterResult> {
            self.post_calls.fetch_add(1, Ordering::SeqCst);
            self.used_material.store(current, Ordering::SeqCst);
            self.saw_allowlist.store(
                request.endpoint() == AllowlistedPlayerEndpoint::YouTubePlayer
                    && request.method() == AllowlistedReplayMethod::Post
                    && request.endpoint().scheme() == "https"
                    && request.endpoint().authority() == "www.youtube.com"
                    && request.endpoint().path() == "/youtubei/v1/player"
                    && request.client() == ReplayPlayerClient::TvHtml5
                    && request.quality() == ReplayQualityPolicy::High
                    && request.issued_at_unix_ms() == 1_500
                    && request.media_id() == "fixture_media-1"
                    && request.lineage().parent_capture_ref() == CaptureRef::from_bytes([1; 16]),
                Ordering::SeqCst,
            );
            let behavior = self.post_behavior;
            Box::pin(async move {
                match behavior {
                    PostBehavior::Decision(decision) => {
                        FreshPlayerAdapterResult::Decision(decision)
                    }
                    PostBehavior::NetworkFailed => FreshPlayerAdapterResult::NetworkFailed,
                    PostBehavior::Inconclusive => FreshPlayerAdapterResult::Inconclusive,
                    PostBehavior::Pending => {
                        std::future::pending::<FreshPlayerAdapterResult>().await
                    }
                    PostBehavior::Panicked => panic!("private fresh fixture panic"),
                }
            })
        }
    }

    struct FixedClock(u64);

    impl ReplayClock for FixedClock {
        fn unix_ms(&self) -> u64 {
            self.0
        }
    }

    struct PendingTimer;

    impl ReplayTimer for PendingTimer {
        fn wait<'a>(&'a self, _duration: Duration) -> ReplayFuture<'a, ()> {
            Box::pin(std::future::pending())
        }
    }

    struct ImmediateTimer;

    impl ReplayTimer for ImmediateTimer {
        fn wait<'a>(&'a self, _duration: Duration) -> ReplayFuture<'a, ()> {
            Box::pin(std::future::ready(()))
        }
    }

    #[tokio::test]
    async fn fresh_replay_uses_current_material_and_one_allowlisted_post() {
        let recipe = recipe();
        let adapter = FixtureFreshAdapter::new(
            AcquireBehavior::Ready(777),
            PostBehavior::Decision(playable_decision()),
        );
        let result = run_fresh_replay(
            &recipe,
            CaptureRef::from_bytes([8; 16]),
            &adapter,
            &FixedClock(1_500),
            &PendingTimer,
            &CancellationToken::new(),
            FreshReplayPolicy::default(),
        )
        .await
        .unwrap();

        assert_eq!(result.terminal_count(), 1);
        assert_eq!(
            result.terminal().outcome(),
            ReplayTerminalOutcome::Fresh(FreshReplayOutcome::Reproduced)
        );
        assert_eq!(adapter.acquire_calls.load(Ordering::SeqCst), 1);
        assert_eq!(adapter.post_calls.load(Ordering::SeqCst), 1);
        assert_eq!(adapter.requested_at.load(Ordering::SeqCst), 1_500);
        assert_eq!(adapter.used_material.load(Ordering::SeqCst), 777);
        assert!(adapter.saw_allowlist.load(Ordering::SeqCst));
        assert_eq!(FreshReplayPolicy::default().request_budget(), 1);
    }

    #[tokio::test]
    async fn fresh_failures_are_typed_and_never_retry() {
        let recipe = recipe();
        let cases = [
            (
                AcquireBehavior::Failure(CurrentMaterialFailure::AuthUnavailable),
                PostBehavior::Inconclusive,
                FreshReplayOutcome::AuthUnavailable,
                0,
            ),
            (
                AcquireBehavior::Failure(CurrentMaterialFailure::BrowserContended),
                PostBehavior::Inconclusive,
                FreshReplayOutcome::BrowserContended,
                0,
            ),
            (
                AcquireBehavior::Ready(1),
                PostBehavior::NetworkFailed,
                FreshReplayOutcome::NetworkFailed,
                1,
            ),
            (
                AcquireBehavior::Ready(1),
                PostBehavior::Decision(unavailable_decision()),
                FreshReplayOutcome::ProviderChanged,
                1,
            ),
        ];
        for (index, (acquire, post, expected, expected_posts)) in cases.into_iter().enumerate() {
            let adapter = FixtureFreshAdapter::new(acquire, post);
            let result = run_fresh_replay(
                &recipe,
                CaptureRef::from_bytes([u8::try_from(index).unwrap_or(0).saturating_add(9); 16]),
                &adapter,
                &FixedClock(1_500),
                &PendingTimer,
                &CancellationToken::new(),
                FreshReplayPolicy::default(),
            )
            .await
            .unwrap();
            assert_eq!(result.terminal_count(), 1);
            assert_eq!(
                result.terminal().outcome(),
                ReplayTerminalOutcome::Fresh(expected)
            );
            assert_eq!(adapter.acquire_calls.load(Ordering::SeqCst), 1);
            assert_eq!(adapter.post_calls.load(Ordering::SeqCst), expected_posts);
        }
    }

    #[tokio::test]
    async fn fresh_adapter_panic_becomes_one_typed_terminal() {
        let adapter = FixtureFreshAdapter::new(AcquireBehavior::Ready(1), PostBehavior::Panicked);
        let result = run_fresh_replay(
            &recipe(),
            CaptureRef::from_bytes([42; 16]),
            &adapter,
            &FixedClock(1_500),
            &PendingTimer,
            &CancellationToken::new(),
            FreshReplayPolicy::default(),
        )
        .await
        .unwrap();
        assert_eq!(result.terminal_count(), 1);
        assert_eq!(
            result.terminal().outcome(),
            ReplayTerminalOutcome::Fresh(FreshReplayOutcome::Panicked)
        );
        assert_eq!(adapter.post_calls.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn cancellation_timeout_expiry_and_incomplete_input_are_bounded() {
        let recipe = recipe();
        let pending = FixtureFreshAdapter::new(AcquireBehavior::Pending, PostBehavior::Pending);
        let cancelled = CancellationToken::new();
        cancelled.cancel();
        let result = run_fresh_replay(
            &recipe,
            CaptureRef::from_bytes([20; 16]),
            &pending,
            &FixedClock(1_500),
            &PendingTimer,
            &cancelled,
            FreshReplayPolicy::default(),
        )
        .await
        .unwrap();
        assert_eq!(
            result.terminal().outcome(),
            ReplayTerminalOutcome::Fresh(FreshReplayOutcome::Cancelled)
        );

        let timed_out = FixtureFreshAdapter::new(AcquireBehavior::Pending, PostBehavior::Pending);
        let result = run_fresh_replay(
            &recipe,
            CaptureRef::from_bytes([21; 16]),
            &timed_out,
            &FixedClock(1_500),
            &ImmediateTimer,
            &CancellationToken::new(),
            FreshReplayPolicy::new(Duration::from_millis(1)).unwrap(),
        )
        .await
        .unwrap();
        assert_eq!(
            result.terminal().outcome(),
            ReplayTerminalOutcome::Fresh(FreshReplayOutcome::TimedOut)
        );

        let untouched = FixtureFreshAdapter::new(
            AcquireBehavior::Ready(1),
            PostBehavior::Decision(playable_decision()),
        );
        let result = run_fresh_replay(
            &recipe,
            CaptureRef::from_bytes([22; 16]),
            &untouched,
            &FixedClock(2_001),
            &PendingTimer,
            &CancellationToken::new(),
            FreshReplayPolicy::default(),
        )
        .await
        .unwrap();
        assert_eq!(
            result.terminal().outcome(),
            ReplayTerminalOutcome::Fresh(FreshReplayOutcome::ExpiredInput)
        );
        assert_eq!(untouched.acquire_calls.load(Ordering::SeqCst), 0);

        let mut incomplete_parts = recipe_parts();
        incomplete_parts.response_complete = false;
        let incomplete = ReplayRecipeV1::from_versioned_parts(1, incomplete_parts);
        let result = run_fresh_replay(
            &incomplete,
            CaptureRef::from_bytes([23; 16]),
            &untouched,
            &FixedClock(1_500),
            &PendingTimer,
            &CancellationToken::new(),
            FreshReplayPolicy::default(),
        )
        .await
        .unwrap();
        assert_eq!(
            result.terminal().outcome(),
            ReplayTerminalOutcome::Fresh(FreshReplayOutcome::Inconclusive)
        );
        assert_eq!(untouched.acquire_calls.load(Ordering::SeqCst), 0);
    }

    #[test]
    fn policy_and_lineage_prevent_arbitrary_or_recursive_replay() {
        assert_eq!(
            FreshReplayPolicy::new(Duration::ZERO),
            Err(FreshReplayPolicyError::InvalidTimeout)
        );
        assert_eq!(
            FreshReplayPolicy::new(MAX_FRESH_REPLAY_TIMEOUT + Duration::from_millis(1)),
            Err(FreshReplayPolicyError::InvalidTimeout)
        );
        assert_eq!(AllowlistedPlayerEndpoint::YouTubePlayer.scheme(), "https");
        assert_eq!(
            AllowlistedPlayerEndpoint::YouTubePlayer.authority(),
            "www.youtube.com"
        );
        assert_eq!(
            AllowlistedPlayerEndpoint::YouTubePlayer.path(),
            "/youtubei/v1/player"
        );
        assert_eq!(AllowlistedReplayMethod::Post, AllowlistedReplayMethod::Post);
        assert!(!CapturePurpose::Replay.may_claim_interactive());

        let recipe = recipe();
        let adapter = FixtureOfflineAdapter {
            calls: Cell::new(0),
            result: OfflineAdapterResult::Decision(playable_decision()),
        };
        assert!(matches!(
            run_offline_replay(&recipe, recipe.parent_capture_ref(), &adapter),
            Err(ReplayStartError::RecursiveCapture)
        ));
        assert_eq!(adapter.calls.get(), 0);

        let mut replay_parts = recipe_parts();
        replay_parts.source_purpose = CapturePurpose::Replay;
        let recursive = ReplayRecipeV1::from_versioned_parts(1, replay_parts);
        assert!(matches!(
            run_offline_replay(&recursive, CaptureRef::from_bytes([30; 16]), &adapter),
            Err(ReplayStartError::RecursiveReplay)
        ));
        assert_eq!(adapter.calls.get(), 0);
    }
}
