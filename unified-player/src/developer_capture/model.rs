use std::{fmt, time::Duration};

use zeroize::Zeroizing;

pub(crate) const CAPTURE_SCHEMA_VERSION: u16 = 1;
pub(crate) const CAPTURE_CONSENT_VERSION: u16 = 1;

pub(super) const MAX_CONTAINER_OVERHEAD_BYTES: u64 = 128 * 1024;
pub(super) const MAX_HTTP_EVIDENCE_OVERHEAD_BYTES: u64 = 128 * 1024;
pub(super) const TERMINAL_RECORD_RESERVE_BYTES: u64 = 256;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct CaptureLimits {
    pub(crate) arm_deadline: Duration,
    pub(crate) operation_deadline: Duration,
    pub(crate) record_capacity: u16,
    pub(crate) exchange_capacity: u16,
    pub(crate) player_request_bytes: u64,
    pub(crate) player_response_bytes: u64,
    pub(crate) transport_error_bytes: u64,
    pub(crate) plaintext_bytes: u64,
    pub(crate) encrypted_artifact_bytes: u64,
    pub(crate) retained_artifacts: u16,
    pub(crate) total_storage_bytes: u64,
    pub(crate) retention: Duration,
    pub(crate) writer_finalization: Duration,
}

impl CaptureLimits {
    #[allow(clippy::duration_suboptimal_units)]
    pub(crate) const DEFAULT: Self = Self {
        arm_deadline: Duration::from_secs(60),
        operation_deadline: Duration::from_secs(60),
        record_capacity: 256,
        exchange_capacity: 32,
        player_request_bytes: 2 * 1024 * 1024,
        player_response_bytes: 2 * 1024 * 1024,
        transport_error_bytes: 64 * 1024,
        plaintext_bytes: 16 * 1024 * 1024,
        encrypted_artifact_bytes: 20 * 1024 * 1024,
        retained_artifacts: 5,
        total_storage_bytes: 100 * 1024 * 1024,
        retention: Duration::from_secs(24 * 60 * 60),
        writer_finalization: Duration::from_secs(5),
    };

    pub(crate) fn validate(self) -> Result<Self, CaptureModelError> {
        if self.arm_deadline.is_zero()
            || self.operation_deadline.is_zero()
            || self.record_capacity == 0
            || self.exchange_capacity == 0
            || self.player_request_bytes == 0
            || self.player_response_bytes == 0
            || self.transport_error_bytes == 0
            || self.plaintext_bytes == 0
            || self.encrypted_artifact_bytes == 0
            || self.retained_artifacts == 0
            || self.total_storage_bytes == 0
            || self.retention.is_zero()
            || self.writer_finalization.is_zero()
        {
            return Err(CaptureModelError::InvalidLimits);
        }
        if self.player_request_bytes > self.plaintext_bytes
            || self.player_response_bytes > self.plaintext_bytes
            || self.transport_error_bytes > self.plaintext_bytes
            || self.exchange_capacity > self.record_capacity
            || self.encrypted_artifact_bytes
                < self
                    .plaintext_bytes
                    .checked_add(MAX_CONTAINER_OVERHEAD_BYTES)
                    .ok_or(CaptureModelError::InvalidLimits)?
            || self.encrypted_artifact_bytes > self.total_storage_bytes
        {
            return Err(CaptureModelError::InvalidLimits);
        }
        Ok(self)
    }

    pub(crate) const fn record_bytes(self, kind: CaptureRecordKind) -> u64 {
        match kind {
            CaptureRecordKind::HttpRequest => self
                .player_request_bytes
                .saturating_add(MAX_HTTP_EVIDENCE_OVERHEAD_BYTES),
            CaptureRecordKind::HttpResponse => self
                .player_response_bytes
                .saturating_add(MAX_HTTP_EVIDENCE_OVERHEAD_BYTES),
            CaptureRecordKind::OperationBoundary
            | CaptureRecordKind::AuthSelection
            | CaptureRecordKind::NetworkFailure
            | CaptureRecordKind::PlayerParse
            | CaptureRecordKind::FormatInventory
            | CaptureRecordKind::SelectionDecision
            | CaptureRecordKind::BrowserExchange
            | CaptureRecordKind::MediaProbe
            | CaptureRecordKind::DecodeStage
            | CaptureRecordKind::TerminalOutcome
            | CaptureRecordKind::SyntheticFixture => self.transport_error_bytes,
        }
    }
}

impl Default for CaptureLimits {
    fn default() -> Self {
        Self::DEFAULT
    }
}

#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub(crate) struct CaptureRef([u8; 16]);

impl CaptureRef {
    pub(crate) const fn from_bytes(bytes: [u8; 16]) -> Self {
        Self(bytes)
    }

    pub(super) const fn bytes(self) -> [u8; 16] {
        self.0
    }

    pub(super) fn file_stem(self) -> String {
        encode_hex(&self.0)
    }

    pub(super) fn from_file_stem(value: &str) -> Result<Self, CaptureModelError> {
        if value.len() != 32 || !value.bytes().all(|byte| byte.is_ascii_hexdigit()) {
            return Err(CaptureModelError::InvalidCaptureRef);
        }
        if value.bytes().any(|byte| byte.is_ascii_uppercase()) {
            return Err(CaptureModelError::InvalidCaptureRef);
        }
        let mut bytes = [0_u8; 16];
        for (index, pair) in value.as_bytes().chunks_exact(2).enumerate() {
            bytes[index] = decode_hex_pair(pair)?;
        }
        Ok(Self(bytes))
    }

    pub(crate) fn safe(self) -> SafeCaptureRef {
        let mut bytes = [0_u8; 4];
        bytes.copy_from_slice(&self.0[..4]);
        SafeCaptureRef(bytes)
    }
}

impl fmt::Debug for CaptureRef {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("CaptureRef([private])")
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub(crate) struct SafeCaptureRef([u8; 4]);

impl SafeCaptureRef {
    pub(crate) fn from_hex(value: &str) -> Result<Self, CaptureModelError> {
        if value.len() != 8
            || value.bytes().any(|byte| byte.is_ascii_uppercase())
            || !value.bytes().all(|byte| byte.is_ascii_hexdigit())
        {
            return Err(CaptureModelError::InvalidCaptureRef);
        }
        let mut bytes = [0_u8; 4];
        for (index, pair) in value.as_bytes().chunks_exact(2).enumerate() {
            bytes[index] = decode_hex_pair(pair)?;
        }
        Ok(Self(bytes))
    }
}

impl fmt::Display for SafeCaptureRef {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&encode_hex(&self.0))
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub(crate) struct SafeOperationRef([u8; 4]);

impl SafeOperationRef {
    pub(crate) const fn from_bytes(bytes: [u8; 4]) -> Self {
        Self(bytes)
    }

    pub(crate) fn from_hex(value: &str) -> Result<Self, CaptureModelError> {
        if value.len() != 8
            || value.bytes().any(|byte| byte.is_ascii_uppercase())
            || !value.bytes().all(|byte| byte.is_ascii_hexdigit())
        {
            return Err(CaptureModelError::InvalidCaptureRef);
        }
        let mut bytes = [0_u8; 4];
        for (index, pair) in value.as_bytes().chunks_exact(2).enumerate() {
            bytes[index] = decode_hex_pair(pair)?;
        }
        Ok(Self(bytes))
    }
}

impl fmt::Display for SafeOperationRef {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&encode_hex(&self.0))
    }
}

pub(crate) struct SensitiveString(Zeroizing<String>);

impl SensitiveString {
    pub(crate) fn new(value: String) -> Self {
        Self(Zeroizing::new(value))
    }

    pub(super) fn expose(&self) -> &str {
        self.0.as_str()
    }
}

impl fmt::Debug for SensitiveString {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("SensitiveString([private])")
    }
}

pub(crate) struct SensitiveBytes(Zeroizing<Vec<u8>>);

impl SensitiveBytes {
    pub(crate) fn new(value: Vec<u8>) -> Self {
        Self(Zeroizing::new(value))
    }

    pub(super) fn expose(&self) -> &[u8] {
        self.0.as_slice()
    }

    pub(super) fn len(&self) -> usize {
        self.0.len()
    }

    pub(super) fn with_capacity(capacity: usize) -> Self {
        Self(Zeroizing::new(Vec::with_capacity(capacity)))
    }

    pub(super) fn expose_mut(&mut self) -> &mut Vec<u8> {
        &mut self.0
    }
}

impl fmt::Debug for SensitiveBytes {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("SensitiveBytes([private])")
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum CapturePurpose {
    InteractivePlayback,
    Prefetch,
    Resume,
    Probe,
    Replay,
    Comparison,
}

impl CapturePurpose {
    pub(crate) const fn may_claim_interactive(self) -> bool {
        matches!(self, Self::InteractivePlayback)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum SafeCaptureState {
    Inactive,
    ConsentRequired,
    Armed,
    Claimed,
    Capturing,
    Finalizing,
    Ready,
    Incomplete,
    Expired,
    Failed,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum CaptureCompleteness {
    Pending,
    Complete,
    Incomplete,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum IncompleteReason {
    RecordCapacity,
    ExchangeCapacity,
    RecordSize,
    QueueCapacity,
    PlaintextBudget,
    OperationDeadline,
    FinalizationDeadline,
    MissingProviderExchange,
    WriterUnavailable,
    Abandoned,
    Shutdown,
    Persistence,
    Truncated,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum SafeTerminalCategory {
    Success,
    Failed,
    Cancelled,
    Superseded,
    TimedOut,
    Panicked,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum CaptureByteBucket {
    Empty,
    Under64KiB,
    Under1MiB,
    Under4MiB,
    Under16MiB,
    AtOrOver16MiB,
}

impl CaptureByteBucket {
    pub(crate) const fn from_bytes(bytes: u64) -> Self {
        match bytes {
            0 => Self::Empty,
            1..=65_535 => Self::Under64KiB,
            65_536..=1_048_575 => Self::Under1MiB,
            1_048_576..=4_194_303 => Self::Under4MiB,
            4_194_304..=16_777_215 => Self::Under16MiB,
            _ => Self::AtOrOver16MiB,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct SafeCaptureSnapshot {
    pub(crate) state: SafeCaptureState,
    pub(crate) capture_ref: Option<SafeCaptureRef>,
    pub(crate) remaining_seconds: Option<u64>,
    pub(crate) record_count: u16,
    pub(crate) byte_bucket: CaptureByteBucket,
    pub(crate) completeness: CaptureCompleteness,
    pub(crate) terminal_category: Option<SafeTerminalCategory>,
    pub(crate) dropped_records: u16,
}

impl Default for SafeCaptureSnapshot {
    fn default() -> Self {
        Self {
            state: SafeCaptureState::Inactive,
            capture_ref: None,
            remaining_seconds: None,
            record_count: 0,
            byte_bucket: CaptureByteBucket::Empty,
            completeness: CaptureCompleteness::Pending,
            terminal_category: None,
            dropped_records: 0,
        }
    }
}

pub(crate) struct CaptureRecordV1 {
    pub(super) sequence: u16,
    pub(super) monotonic_offset_ms: u64,
    pub(super) exchange_ref: Option<ExchangeRef>,
    pub(super) endpoint_role: EndpointRole,
    pub(super) client_kind: ProviderClientKind,
    pub(super) transport_kind: TransportKind,
    pub(super) attempt: u8,
    pub(super) kind: CaptureRecordKind,
    pub(super) payload: SensitiveBytes,
}

impl CaptureRecordV1 {
    pub(crate) fn new(
        sequence: u16,
        monotonic_offset_ms: u64,
        kind: CaptureRecordKind,
        payload: SensitiveBytes,
    ) -> Self {
        Self {
            sequence,
            monotonic_offset_ms,
            exchange_ref: None,
            endpoint_role: EndpointRole::Unknown,
            client_kind: ProviderClientKind::Unknown,
            transport_kind: TransportKind::Unknown,
            attempt: 0,
            kind,
            payload,
        }
    }

    #[allow(clippy::too_many_arguments)]
    pub(crate) fn with_context(
        sequence: u16,
        monotonic_offset_ms: u64,
        exchange_ref: Option<ExchangeRef>,
        endpoint_role: EndpointRole,
        client_kind: ProviderClientKind,
        transport_kind: TransportKind,
        attempt: u8,
        kind: CaptureRecordKind,
        payload: SensitiveBytes,
    ) -> Self {
        Self {
            sequence,
            monotonic_offset_ms,
            exchange_ref,
            endpoint_role,
            client_kind,
            transport_kind,
            attempt,
            kind,
            payload,
        }
    }

    pub(crate) const fn sequence(&self) -> u16 {
        self.sequence
    }

    pub(crate) const fn monotonic_offset_ms(&self) -> u64 {
        self.monotonic_offset_ms
    }

    pub(crate) const fn exchange_ref(&self) -> Option<ExchangeRef> {
        self.exchange_ref
    }

    pub(crate) const fn client_kind(&self) -> ProviderClientKind {
        self.client_kind
    }

    pub(crate) const fn endpoint_role(&self) -> EndpointRole {
        self.endpoint_role
    }

    pub(crate) const fn transport_kind(&self) -> TransportKind {
        self.transport_kind
    }

    pub(crate) const fn attempt(&self) -> u8 {
        self.attempt
    }

    pub(crate) const fn kind(&self) -> CaptureRecordKind {
        self.kind
    }

    pub(crate) const fn payload(&self) -> &SensitiveBytes {
        &self.payload
    }
}

impl fmt::Debug for CaptureRecordV1 {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("CaptureRecordV1([private])")
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum CaptureRecordKind {
    OperationBoundary,
    AuthSelection,
    HttpRequest,
    HttpResponse,
    NetworkFailure,
    PlayerParse,
    FormatInventory,
    SelectionDecision,
    BrowserExchange,
    MediaProbe,
    DecodeStage,
    TerminalOutcome,
    SyntheticFixture,
}

#[derive(Clone, Copy, PartialEq, Eq, Hash)]
pub(crate) struct ExchangeRef([u8; 8]);

impl ExchangeRef {
    pub(crate) const fn from_bytes(bytes: [u8; 8]) -> Self {
        Self(bytes)
    }

    pub(super) const fn bytes(self) -> [u8; 8] {
        self.0
    }
}

impl fmt::Debug for ExchangeRef {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("ExchangeRef([private])")
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum EndpointRole {
    Unknown,
    Operation,
    PlayerApi,
    BrowserPlayer,
    Media,
    BrowserMedia,
    Decoder,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ProviderClientKind {
    Unknown,
    Web,
    WebRemix,
    Android,
    Ios,
    TvHtml5,
    Spotify,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum TransportKind {
    Unknown,
    NativeHttp,
    BrowserCdp,
    MediaRange,
    OfflineReplay,
    FreshReplay,
}

impl CaptureRecordKind {
    pub(crate) const fn starts_provider_exchange(self) -> bool {
        matches!(self, Self::HttpRequest)
    }
}

pub(crate) struct PrivateCaptureV1 {
    pub(super) capture_ref: CaptureRef,
    pub(super) created_unix_ms: u64,
    pub(super) completed_unix_ms: u64,
    pub(super) purpose: CapturePurpose,
    pub(super) operation_ref: SafeOperationRef,
    pub(super) records: Vec<CaptureRecordV1>,
    pub(super) completeness: CaptureCompleteness,
    pub(super) incomplete_reasons: Vec<IncompleteReason>,
    pub(super) dropped_records: u16,
    pub(super) terminal_category: SafeTerminalCategory,
    pub(super) credential_values_present: bool,
}

impl PrivateCaptureV1 {
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn new(
        capture_ref: CaptureRef,
        created_unix_ms: u64,
        completed_unix_ms: u64,
        purpose: CapturePurpose,
        operation_ref: SafeOperationRef,
        records: Vec<CaptureRecordV1>,
        completeness: CaptureCompleteness,
        incomplete_reasons: Vec<IncompleteReason>,
        dropped_records: u16,
        terminal_category: SafeTerminalCategory,
    ) -> Self {
        Self {
            capture_ref,
            created_unix_ms,
            completed_unix_ms,
            purpose,
            operation_ref,
            records,
            completeness,
            incomplete_reasons,
            dropped_records,
            terminal_category,
            credential_values_present: false,
        }
    }

    pub(crate) const fn capture_ref(&self) -> CaptureRef {
        self.capture_ref
    }

    pub(crate) const fn created_unix_ms(&self) -> u64 {
        self.created_unix_ms
    }

    pub(crate) const fn purpose(&self) -> CapturePurpose {
        self.purpose
    }

    pub(crate) const fn operation_ref(&self) -> SafeOperationRef {
        self.operation_ref
    }

    pub(crate) fn mark_credential_values_present(&mut self) {
        self.credential_values_present = true;
    }

    pub(crate) fn records(&self) -> &[CaptureRecordV1] {
        &self.records
    }

    pub(crate) const fn credential_values_present(&self) -> bool {
        self.credential_values_present
    }

    pub(crate) const fn completeness(&self) -> CaptureCompleteness {
        self.completeness
    }
}

impl fmt::Debug for PrivateCaptureV1 {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("PrivateCaptureV1([private])")
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct SafeArtifactReview {
    pub(crate) schema_version: u16,
    pub(crate) capture_ref: SafeCaptureRef,
    pub(crate) record_count: u16,
    pub(crate) byte_bucket: CaptureByteBucket,
    pub(crate) completeness: CaptureCompleteness,
    pub(crate) terminal_category: SafeTerminalCategory,
    pub(crate) checksum_valid: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum CaptureModelError {
    InvalidCaptureRef,
    InvalidLimits,
}

impl fmt::Display for CaptureModelError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::InvalidCaptureRef => "invalid private capture reference",
            Self::InvalidLimits => "invalid private capture limits",
        })
    }
}

impl std::error::Error for CaptureModelError {}

fn encode_hex(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut output = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        output.push(char::from(HEX[usize::from(byte >> 4)]));
        output.push(char::from(HEX[usize::from(byte & 0x0f)]));
    }
    output
}

fn decode_hex_pair(pair: &[u8]) -> Result<u8, CaptureModelError> {
    let high = decode_hex_digit(pair[0])?;
    let low = decode_hex_digit(pair[1])?;
    Ok((high << 4) | low)
}

fn decode_hex_digit(value: u8) -> Result<u8, CaptureModelError> {
    match value {
        b'0'..=b'9' => Ok(value - b'0'),
        b'a'..=b'f' => Ok(value - b'a' + 10),
        _ => Err(CaptureModelError::InvalidCaptureRef),
    }
}

#[cfg(test)]
mod tests {
    use super::{
        CaptureLimits, CaptureRef, ExchangeRef, PrivateCaptureV1, SafeCaptureRef, SensitiveBytes,
        SensitiveString,
    };
    use static_assertions::assert_not_impl_any;

    assert_not_impl_any!(SensitiveBytes: std::fmt::Display, serde::Serialize);
    assert_not_impl_any!(SensitiveString: std::fmt::Display, serde::Serialize);
    assert_not_impl_any!(CaptureRef: std::fmt::Display, serde::Serialize);
    assert_not_impl_any!(ExchangeRef: std::fmt::Display, serde::Serialize);

    #[test]
    fn safe_capture_reference_accepts_only_strict_lowercase_hex() {
        assert_eq!(
            SafeCaptureRef::from_hex("0123abcd").unwrap().to_string(),
            "0123abcd"
        );
        for rejected in [
            "",
            "0123abc",
            "0123abcde",
            "0123ABCD",
            "0123abcz",
            " 123abcd",
        ] {
            assert!(SafeCaptureRef::from_hex(rejected).is_err(), "{rejected:?}");
        }
    }
    assert_not_impl_any!(PrivateCaptureV1: std::fmt::Display, serde::Serialize);

    #[test]
    fn private_debug_output_is_redacted() {
        let text = SensitiveString::new("seeded-private-canary".to_owned());
        let bytes = SensitiveBytes::new(b"seeded-private-canary".to_vec());
        assert_eq!(format!("{text:?}"), "SensitiveString([private])");
        assert_eq!(format!("{bytes:?}"), "SensitiveBytes([private])");
    }

    #[test]
    fn capture_refs_accept_only_canonical_fixed_hex() {
        let capture_ref = CaptureRef::from_bytes([0xab; 16]);
        let stem = capture_ref.file_stem();
        assert_eq!(CaptureRef::from_file_stem(&stem).unwrap(), capture_ref);
        for invalid in [
            "../aaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
            "AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA",
            "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
            "gggggggggggggggggggggggggggggggg",
        ] {
            assert!(CaptureRef::from_file_stem(invalid).is_err());
        }
    }

    #[test]
    fn default_limits_are_valid_and_match_the_plan() {
        let limits = CaptureLimits::default().validate().unwrap();
        assert_eq!(limits.arm_deadline.as_secs(), 60);
        assert_eq!(limits.operation_deadline.as_secs(), 60);
        assert_eq!(limits.record_capacity, 256);
        assert_eq!(limits.exchange_capacity, 32);
        assert_eq!(limits.plaintext_bytes, 16 * 1024 * 1024);
        assert_eq!(limits.encrypted_artifact_bytes, 20 * 1024 * 1024);
        assert_eq!(limits.retained_artifacts, 5);
        assert_eq!(limits.retention.as_secs(), 24 * 60 * 60);
    }

    #[test]
    fn limits_reserve_space_for_binary_container_overhead() {
        let limits = CaptureLimits {
            encrypted_artifact_bytes: CaptureLimits::default()
                .plaintext_bytes
                .saturating_add(super::MAX_CONTAINER_OVERHEAD_BYTES - 1),
            ..CaptureLimits::default()
        };
        assert!(limits.validate().is_err());
    }
}
