//! Typed Phase 7F sanitizer for the standalone provider diagnostic derivative.
//!
//! Integration should declare this as `mod sanitize` under the existing
//! `private-capture` module. Operator code needs only the typed input enums,
//! `ProviderDiagnosticDerivativeV1`, the seeded scanner, prepare/create/review
//! functions, and the safe preview/review types. Filesystem code implements the
//! atomic writer and exact reader traits; this module never imports the ordinary
//! support-bundle subsystem or accepts a capture reference or output path.

use std::{collections::HashMap, fmt};

use base64::{
    engine::general_purpose::{STANDARD, STANDARD_NO_PAD, URL_SAFE, URL_SAFE_NO_PAD},
    Engine as _,
};
use serde::{de::DeserializeOwned, Deserialize, Serialize};
use sha2::{Digest as _, Sha256};
use zeroize::{Zeroize as _, Zeroizing};

use super::{
    model::{CaptureRecordKind, PrivateCaptureV1, SensitiveBytes},
    payload::{decode_fields, field as private_field, MAX_HEADER_COUNT as MAX_PRIVATE_HEADERS},
};

pub(crate) const DERIVATIVE_SCHEMA_VERSION: u16 = 1;
const DERIVATIVE_MANIFEST_VERSION: u16 = 1;
const MAX_DERIVATIVE_BYTES: usize = 512 * 1024;
const MAX_FACTS: usize = 48;
const MAX_FINDINGS: usize = 64;
const MAX_TIMINGS: usize = 16;
const MAX_TRANSPORTS: usize = 8;
pub(super) const MAX_COUNT: u16 = 4096;
const MAX_TIMING_MS: u32 = 120_000;
const MIN_ACTIONABLE_CANARY_BYTES: usize = 4;
const MAX_CANARIES: usize = 8192;
// A valid HTTP evidence record can contain a 2 MiB provider payload plus its
// bounded capture envelope. The scanner must accept that source evidence even
// when the much smaller typed derivative could not possibly contain it.
const MAX_CANARY_BYTES: usize = 2 * 1024 * 1024 + 128 * 1024;
const MAX_TOTAL_CANARY_BYTES: usize = 4 * 1024 * 1024;
const MAX_SCANNER_SOURCES: usize = 4;
const MAX_SCANNER_SEARCH_BYTES: usize = 512 * 1024 * 1024;
const CANARY_REPRESENTATIONS_PER_VALUE: usize = 8;

const MANIFEST_FILE_NAME: &str = "manifest.json";
const EVIDENCE_FILE_NAME: &str = "evidence.json";
const CHECKSUM_FILE_NAME: &str = "checksums.sha256";
const PRIVACY_BOUNDARY: &str = "typed-allowlist-only";
const BUNDLE_RELATIONSHIP: &str = "separate-from-support-bundles";

const FORBIDDEN_FIELD_NAMES: &[&[u8]] = &[
    br#""capture_ref""#,
    br#""parent_capture_ref""#,
    br#""exchange_ref""#,
    br#""operation_ref""#,
    br#""path""#,
    br#""filesystem_path""#,
    br#""url""#,
    br#""query""#,
    br#""media_id""#,
    br#""video_id""#,
    br#""identifier""#,
    br#""title""#,
    br#""artist""#,
    br#""lyrics""#,
    br#""headers""#,
    br#""body""#,
    br#""cookie""#,
    br#""authorization""#,
    br#""credential""#,
    br#""passphrase""#,
    br#""proof_token""#,
    br#""proxy""#,
    br#""request""#,
    br#""response""#,
    br#""payload""#,
    br#""raw""#,
    br#""token""#,
    br#""user_agent""#,
];

#[derive(Clone, Copy, Debug, Deserialize, Eq, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum DerivativeProviderV1 {
    YouTubeMusic,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum DerivativeOperationV1 {
    InteractivePlayback,
    OfflineReplay,
    FreshReplay,
    Comparison,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum DerivativeCompletenessV1 {
    Complete,
    Incomplete,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum DerivativeTerminalV1 {
    Success,
    Failed,
    Cancelled,
    Superseded,
    TimedOut,
    Panicked,
    Inconclusive,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum DerivativeTransportV1 {
    NotReached,
    NativeHttp,
    BrowserCdp,
    MediaRange,
    OfflineReplay,
    FreshReplay,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum DerivativeClientV1 {
    AndroidVr,
    Android,
    Ios,
    Web,
    TvHtml5,
    WebRemix,
    Unknown,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum DerivativeAuthV1 {
    None,
    Browser,
    Bearer,
    Unknown,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum DerivativeClientVersionPolicyV1 {
    Static,
    Cached,
    Discovered,
    Fallback,
    Unknown,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum DerivativeHttpStatusClassV1 {
    Informational,
    Success,
    Redirect,
    Unauthorized,
    Forbidden,
    RateLimited,
    ClientError,
    ServerError,
    Other,
    NoResponse,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum DerivativePlayabilityV1 {
    Playable,
    AuthenticationRequired,
    ConsentAgeOrRegion,
    ProviderUnavailable,
    RateLimited,
    ContractFailure,
    Unknown,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum DerivativeFailureCategoryV1 {
    None,
    Authentication,
    ConsentAgeOrRegion,
    ProviderUnavailable,
    ProofToken,
    Decipher,
    RateLimited,
    Network,
    Contract,
    UnsupportedFormat,
    MediaForbidden,
    MediaRangeContract,
    Decode,
    Cancelled,
    Unknown,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum DerivativeContainerV1 {
    Mp4,
    WebM,
    OtherKnown,
    Unknown,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum DerivativeCodecV1 {
    Aac,
    Opus,
    OtherKnown,
    Unknown,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum DerivativeBitrateBucketV1 {
    Low,
    Medium,
    High,
    Unknown,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum DerivativeFallbackV1 {
    NotEligible,
    EligibleNotAttempted,
    Attempted,
    AttemptedSucceeded,
    AttemptedFailed,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum DerivativeBrowserComparisonV1 {
    NotRequested,
    MatchedNative,
    DifferedFromNative,
    Failed,
    Inconclusive,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum DerivativeResponseClassV1 {
    Playable,
    Refused,
    Malformed,
    TransportFailure,
    Cancelled,
    Unknown,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum DerivativeContentRangeV1 {
    Valid,
    Missing,
    WrongStart,
    Invalid,
    NotApplicable,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum DerivativeMediaResultV1 {
    NotReached,
    PartialContent,
    Forbidden,
    RangeContractFailure,
    NetworkFailure,
    Cancelled,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum DerivativeReplayResultV1 {
    NotRun,
    Reproduced,
    ProviderChanged,
    AuthenticationUnavailable,
    ExpiredInput,
    NetworkFailed,
    Cancelled,
    TimedOut,
    BrowserContended,
    UnsupportedSchema,
    Inconclusive,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(tag = "field", content = "value", rename_all = "snake_case")]
pub(crate) enum DerivativeFactV1 {
    Client(DerivativeClientV1),
    ClientVersionPolicy(DerivativeClientVersionPolicyV1),
    PlayerHttpStatusClass(DerivativeHttpStatusClassV1),
    MediaHttpStatusClass(DerivativeHttpStatusClassV1),
    Redirected(bool),
    Authentication(DerivativeAuthV1),
    ProofTokenPresent(bool),
    Playability(DerivativePlayabilityV1),
    FailureCategory(DerivativeFailureCategoryV1),
    StreamingDataPresent(bool),
    ReturnedFormats(u16),
    SupportedFormats(u16),
    DirectFormats(u16),
    CipherFormats(u16),
    SelectedContainer(DerivativeContainerV1),
    SelectedCodec(DerivativeCodecV1),
    SelectedBitrate(DerivativeBitrateBucketV1),
    SelectedFormatPresent(bool),
    Fallback(DerivativeFallbackV1),
    NativeResponse(DerivativeResponseClassV1),
    BrowserResponse(DerivativeResponseClassV1),
    BrowserComparison(DerivativeBrowserComparisonV1),
    ContentRange(DerivativeContentRangeV1),
    MediaResult(DerivativeMediaResultV1),
    RetryCount(u8),
    Cancelled(bool),
    Superseded(bool),
    ReplayResult(DerivativeReplayResultV1),
}

impl DerivativeFactV1 {
    pub(super) const fn field(self) -> DerivativeFieldV1 {
        match self {
            Self::Client(_) => DerivativeFieldV1::Client,
            Self::ClientVersionPolicy(_) => DerivativeFieldV1::ClientVersionPolicy,
            Self::PlayerHttpStatusClass(_) => DerivativeFieldV1::PlayerHttpStatusClass,
            Self::MediaHttpStatusClass(_) => DerivativeFieldV1::MediaHttpStatusClass,
            Self::Redirected(_) => DerivativeFieldV1::Redirected,
            Self::Authentication(_) => DerivativeFieldV1::Authentication,
            Self::ProofTokenPresent(_) => DerivativeFieldV1::ProofTokenPresent,
            Self::Playability(_) => DerivativeFieldV1::Playability,
            Self::FailureCategory(_) => DerivativeFieldV1::FailureCategory,
            Self::StreamingDataPresent(_) => DerivativeFieldV1::StreamingDataPresent,
            Self::ReturnedFormats(_) => DerivativeFieldV1::ReturnedFormats,
            Self::SupportedFormats(_) => DerivativeFieldV1::SupportedFormats,
            Self::DirectFormats(_) => DerivativeFieldV1::DirectFormats,
            Self::CipherFormats(_) => DerivativeFieldV1::CipherFormats,
            Self::SelectedContainer(_) => DerivativeFieldV1::SelectedContainer,
            Self::SelectedCodec(_) => DerivativeFieldV1::SelectedCodec,
            Self::SelectedBitrate(_) => DerivativeFieldV1::SelectedBitrate,
            Self::SelectedFormatPresent(_) => DerivativeFieldV1::SelectedFormatPresent,
            Self::Fallback(_) => DerivativeFieldV1::Fallback,
            Self::NativeResponse(_) => DerivativeFieldV1::NativeResponse,
            Self::BrowserResponse(_) => DerivativeFieldV1::BrowserResponse,
            Self::BrowserComparison(_) => DerivativeFieldV1::BrowserComparison,
            Self::ContentRange(_) => DerivativeFieldV1::ContentRange,
            Self::MediaResult(_) => DerivativeFieldV1::MediaResult,
            Self::RetryCount(_) => DerivativeFieldV1::RetryCount,
            Self::Cancelled(_) => DerivativeFieldV1::Cancelled,
            Self::Superseded(_) => DerivativeFieldV1::Superseded,
            Self::ReplayResult(_) => DerivativeFieldV1::ReplayResult,
        }
    }

    const fn values_are_bounded(self) -> bool {
        match self {
            Self::ReturnedFormats(value)
            | Self::SupportedFormats(value)
            | Self::DirectFormats(value)
            | Self::CipherFormats(value) => value <= MAX_COUNT,
            Self::RetryCount(value) => value <= 16,
            _ => true,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub(super) enum DerivativeFieldV1 {
    Client,
    ClientVersionPolicy,
    PlayerHttpStatusClass,
    MediaHttpStatusClass,
    Redirected,
    Authentication,
    ProofTokenPresent,
    Playability,
    FailureCategory,
    StreamingDataPresent,
    ReturnedFormats,
    SupportedFormats,
    DirectFormats,
    CipherFormats,
    SelectedContainer,
    SelectedCodec,
    SelectedBitrate,
    SelectedFormatPresent,
    Fallback,
    NativeResponse,
    BrowserResponse,
    BrowserComparison,
    ContentRange,
    MediaResult,
    RetryCount,
    Cancelled,
    Superseded,
    ReplayResult,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum DerivativeStageV1 {
    PlayerRequest,
    PlayerResponse,
    Parser,
    Selector,
    Browser,
    MediaTransport,
    Decoder,
    Seek,
    AudioOutput,
    Terminal,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum DerivativeTimingBucketV1 {
    Immediate,
    Fast,
    Moderate,
    Slow,
    VerySlow,
    TimedOut,
    Unavailable,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct DerivativeTimingV1 {
    stage: DerivativeStageV1,
    bucket: DerivativeTimingBucketV1,
}

impl DerivativeTimingV1 {
    pub(crate) fn new(stage: DerivativeStageV1, elapsed_ms: u32) -> Result<Self, DerivativeError> {
        if elapsed_ms > MAX_TIMING_MS {
            return Err(DerivativeError::InvalidTypedInput);
        }
        let bucket = match elapsed_ms {
            0 => DerivativeTimingBucketV1::Immediate,
            1..=50 => DerivativeTimingBucketV1::Fast,
            51..=250 => DerivativeTimingBucketV1::Moderate,
            251..=2_000 => DerivativeTimingBucketV1::Slow,
            2_001..=60_000 => DerivativeTimingBucketV1::VerySlow,
            _ => DerivativeTimingBucketV1::TimedOut,
        };
        Ok(Self { stage, bucket })
    }

    pub(super) const fn from_bucket(
        stage: DerivativeStageV1,
        bucket: DerivativeTimingBucketV1,
    ) -> Self {
        Self { stage, bucket }
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum DerivativeFindingCategoryV1 {
    Authentication,
    PlayerResponse,
    Playability,
    FormatInventory,
    Selection,
    BrowserFallback,
    MediaTransport,
    Decoder,
    Terminal,
    Timing,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum DerivativeFindingSeverityV1 {
    Informational,
    Material,
    Critical,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum DerivativeRegisteredFieldV1 {
    HttpStatusClass,
    ClientPolicy,
    AuthenticationKind,
    ProofTokenPresence,
    PlayabilityCategory,
    StreamingDataPresence,
    FormatCounts,
    SelectedFormat,
    FallbackOutcome,
    BrowserClassification,
    MediaClassification,
    StageTiming,
    TerminalOutcome,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum DerivativeValueClassV1 {
    Absent,
    Present,
    False,
    True,
    Zero,
    One,
    Many,
    Success,
    Redirect,
    Unauthorized,
    Forbidden,
    RateLimited,
    ClientError,
    ServerError,
    Playable,
    Unavailable,
    Supported,
    Unsupported,
    Direct,
    CipherOnly,
    Failed,
    Cancelled,
    Superseded,
    Faster,
    Similar,
    Slower,
    Unknown,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum DerivativeExplanationV1 {
    AuthenticationPolicyChanged,
    ProviderStatusChanged,
    PlayabilityChanged,
    StreamingDataChanged,
    FormatAvailabilityChanged,
    SelectedFormatChanged,
    BrowserAndNativeDiffered,
    MediaTransportChanged,
    DecoderOutcomeChanged,
    TerminalOutcomeChanged,
    TimingMateriallyChanged,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct DerivativeFindingV1 {
    category: DerivativeFindingCategoryV1,
    severity: DerivativeFindingSeverityV1,
    field: DerivativeRegisteredFieldV1,
    left: DerivativeValueClassV1,
    right: DerivativeValueClassV1,
    explanation: DerivativeExplanationV1,
}

impl DerivativeFindingV1 {
    pub(crate) const fn new(
        category: DerivativeFindingCategoryV1,
        severity: DerivativeFindingSeverityV1,
        field: DerivativeRegisteredFieldV1,
        left: DerivativeValueClassV1,
        right: DerivativeValueClassV1,
        explanation: DerivativeExplanationV1,
    ) -> Self {
        Self {
            category,
            severity,
            field,
            left,
            right,
            explanation,
        }
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct ProviderDiagnosticDerivativeV1 {
    schema_version: u16,
    provider: DerivativeProviderV1,
    operation: DerivativeOperationV1,
    completeness: DerivativeCompletenessV1,
    terminal: DerivativeTerminalV1,
    transports: Vec<DerivativeTransportV1>,
    facts: Vec<DerivativeFactV1>,
    timings: Vec<DerivativeTimingV1>,
    findings: Vec<DerivativeFindingV1>,
}

impl ProviderDiagnosticDerivativeV1 {
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn new(
        operation: DerivativeOperationV1,
        completeness: DerivativeCompletenessV1,
        terminal: DerivativeTerminalV1,
        transports: Vec<DerivativeTransportV1>,
        facts: Vec<DerivativeFactV1>,
        timings: Vec<DerivativeTimingV1>,
        findings: Vec<DerivativeFindingV1>,
    ) -> Result<Self, DerivativeError> {
        Self {
            schema_version: DERIVATIVE_SCHEMA_VERSION,
            provider: DerivativeProviderV1::YouTubeMusic,
            operation,
            completeness,
            terminal,
            transports,
            facts,
            timings,
            findings,
        }
        .normalize_and_validate()
    }

    fn normalize_and_validate(mut self) -> Result<Self, DerivativeError> {
        if self.schema_version != DERIVATIVE_SCHEMA_VERSION {
            return Err(DerivativeError::UnsupportedSchema);
        }
        if (self.transports.is_empty() && self.operation != DerivativeOperationV1::Comparison)
            || self.transports.len() > MAX_TRANSPORTS
            || self.facts.len() > MAX_FACTS
            || self.timings.len() > MAX_TIMINGS
            || self.findings.len() > MAX_FINDINGS
        {
            return Err(DerivativeError::InputLimitExceeded);
        }

        self.transports.sort_unstable();
        self.transports.dedup();
        self.facts.sort_unstable();
        if self.facts.iter().any(|fact| !fact.values_are_bounded()) {
            return Err(DerivativeError::InvalidTypedInput);
        }
        if self
            .facts
            .windows(2)
            .any(|pair| pair[0].field() == pair[1].field())
        {
            return Err(DerivativeError::DuplicateFact);
        }
        self.timings.sort_unstable();
        if self
            .timings
            .windows(2)
            .any(|pair| pair[0].stage == pair[1].stage)
        {
            return Err(DerivativeError::DuplicateOrInvalidTiming);
        }
        self.findings.sort_unstable();
        self.findings.dedup();
        Ok(self)
    }

    pub(crate) const fn completeness(&self) -> DerivativeCompletenessV1 {
        self.completeness
    }

    pub(super) const fn operation(&self) -> DerivativeOperationV1 {
        self.operation
    }

    pub(super) const fn terminal(&self) -> DerivativeTerminalV1 {
        self.terminal
    }

    pub(super) fn transports(&self) -> &[DerivativeTransportV1] {
        &self.transports
    }

    pub(super) fn facts(&self) -> &[DerivativeFactV1] {
        &self.facts
    }

    pub(super) fn timings(&self) -> &[DerivativeTimingV1] {
        &self.timings
    }

    pub(super) fn findings(&self) -> &[DerivativeFindingV1] {
        &self.findings
    }

    pub(crate) fn fact_count(&self) -> usize {
        self.facts.len()
    }

    pub(crate) fn finding_count(&self) -> usize {
        self.findings.len()
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
enum DerivativeDirtyStateV1 {
    True,
    False,
    Unknown,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
enum DerivativeScanStateV1 {
    Passed,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
struct DerivativeManifestEvidenceV1 {
    name: String,
    bytes: u32,
    sha256: String,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
struct DerivativeManifestV1 {
    manifest_version: u16,
    derivative_schema_version: u16,
    application_version: String,
    source_revision: String,
    source_dirty: DerivativeDirtyStateV1,
    completeness: DerivativeCompletenessV1,
    forbidden_scan: DerivativeScanStateV1,
    privacy_boundary: String,
    support_bundle_relationship: String,
    file_count: u8,
    evidence: DerivativeManifestEvidenceV1,
}

#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub(crate) enum DerivativeFileNameV1 {
    Evidence,
    Checksums,
    Manifest,
}

impl DerivativeFileNameV1 {
    pub(crate) const fn as_str(self) -> &'static str {
        match self {
            Self::Evidence => EVIDENCE_FILE_NAME,
            Self::Checksums => CHECKSUM_FILE_NAME,
            Self::Manifest => MANIFEST_FILE_NAME,
        }
    }
}

#[derive(Clone, Copy)]
pub(crate) struct DerivativeFileViewV1<'a> {
    name: DerivativeFileNameV1,
    contents: &'a [u8],
}

impl<'a> DerivativeFileViewV1<'a> {
    #[cfg(test)]
    pub(super) const fn new_for_store_test(name: DerivativeFileNameV1, contents: &'a [u8]) -> Self {
        Self { name, contents }
    }

    pub(crate) const fn name(self) -> DerivativeFileNameV1 {
        self.name
    }

    pub(crate) const fn contents(self) -> &'a [u8] {
        self.contents
    }
}

impl fmt::Debug for DerivativeFileViewV1<'_> {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("DerivativeFileViewV1")
            .field("name", &self.name)
            .field("bytes", &self.contents.len())
            .finish()
    }
}

#[derive(Clone, Eq, PartialEq)]
pub(crate) struct DerivativePreviewFileV1 {
    name: DerivativeFileNameV1,
    contents: String,
}

impl DerivativePreviewFileV1 {
    pub(crate) const fn name(&self) -> DerivativeFileNameV1 {
        self.name
    }

    pub(crate) fn contents(&self) -> &str {
        &self.contents
    }
}

impl fmt::Debug for DerivativePreviewFileV1 {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("DerivativePreviewFileV1")
            .field("name", &self.name)
            .field("bytes", &self.contents.len())
            .finish()
    }
}

#[derive(Clone, Eq, PartialEq)]
pub(crate) struct DerivativePreviewV1 {
    files: Vec<DerivativePreviewFileV1>,
    total_bytes: usize,
}

impl fmt::Debug for DerivativePreviewV1 {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("DerivativePreviewV1")
            .field("file_count", &self.files.len())
            .field("total_bytes", &self.total_bytes)
            .finish()
    }
}

impl DerivativePreviewV1 {
    pub(crate) fn files(&self) -> &[DerivativePreviewFileV1] {
        &self.files
    }

    pub(crate) const fn total_bytes(&self) -> usize {
        self.total_bytes
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct DerivativeReviewV1 {
    schema_version: u16,
    completeness: DerivativeCompletenessV1,
    file_count: u8,
    fact_count: u16,
    finding_count: u16,
    total_bytes: u32,
    checksum_valid: bool,
    forbidden_scan_passed: bool,
}

impl DerivativeReviewV1 {
    pub(crate) const fn schema_version(&self) -> u16 {
        self.schema_version
    }

    pub(crate) const fn completeness(&self) -> DerivativeCompletenessV1 {
        self.completeness
    }

    pub(crate) const fn checksum_valid(&self) -> bool {
        self.checksum_valid
    }

    pub(crate) const fn forbidden_scan_passed(&self) -> bool {
        self.forbidden_scan_passed
    }

    pub(crate) fn safe_copy_text(&self) -> String {
        format!(
            "unified-player provider diagnostic derivative review\n\
             schema_version={}\n\
             completeness={}\n\
             files={}\n\
             facts={}\n\
             findings={}\n\
             total_bytes={}\n\
             checksum={}\n\
             forbidden_scan={}\n\
             privacy=typed-allowlist-only\n\
             support_bundle=separate\n",
            self.schema_version,
            match self.completeness {
                DerivativeCompletenessV1::Complete => "complete",
                DerivativeCompletenessV1::Incomplete => "incomplete",
            },
            self.file_count,
            self.fact_count,
            self.finding_count,
            self.total_bytes,
            if self.checksum_valid {
                "passed"
            } else {
                "failed"
            },
            if self.forbidden_scan_passed {
                "passed"
            } else {
                "failed"
            }
        )
    }
}

pub(crate) struct PreparedDerivativeV1 {
    evidence: Vec<u8>,
    checksums: Vec<u8>,
    manifest: Vec<u8>,
    review: DerivativeReviewV1,
}

impl fmt::Debug for PreparedDerivativeV1 {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("PreparedDerivativeV1")
            .field("total_bytes", &self.total_bytes())
            .field("review", &self.review)
            .finish_non_exhaustive()
    }
}

impl PreparedDerivativeV1 {
    pub(crate) fn files(&self) -> [DerivativeFileViewV1<'_>; 3] {
        [
            DerivativeFileViewV1 {
                name: DerivativeFileNameV1::Evidence,
                contents: &self.evidence,
            },
            DerivativeFileViewV1 {
                name: DerivativeFileNameV1::Checksums,
                contents: &self.checksums,
            },
            DerivativeFileViewV1 {
                name: DerivativeFileNameV1::Manifest,
                contents: &self.manifest,
            },
        ]
    }

    pub(crate) fn preview(&self) -> DerivativePreviewV1 {
        DerivativePreviewV1 {
            files: self
                .files()
                .into_iter()
                .map(|file| DerivativePreviewFileV1 {
                    name: file.name,
                    contents: String::from_utf8(file.contents.to_vec())
                        .expect("generated derivative files are UTF-8"),
                })
                .collect(),
            total_bytes: self.total_bytes(),
        }
    }

    pub(crate) const fn review(&self) -> &DerivativeReviewV1 {
        &self.review
    }

    fn total_bytes(&self) -> usize {
        self.evidence
            .len()
            .saturating_add(self.checksums.len())
            .saturating_add(self.manifest.len())
    }
}

pub(crate) struct ForbiddenCanaryV1(SensitiveBytes);

impl ForbiddenCanaryV1 {
    pub(crate) fn new(mut bytes: Vec<u8>) -> Result<Self, DerivativeError> {
        if bytes.is_empty() || bytes.len() > MAX_CANARY_BYTES {
            bytes.zeroize();
            return Err(DerivativeError::InvalidScannerConfiguration);
        }
        Ok(Self(SensitiveBytes::new(bytes)))
    }
}

impl fmt::Debug for ForbiddenCanaryV1 {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("ForbiddenCanaryV1([private])")
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum ForbiddenScanVerdictV1 {
    Passed,
    Forbidden { findings: u16 },
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct DerivativeScannerError;

pub(crate) trait DerivativeForbiddenScannerV1 {
    fn scan(
        &self,
        files: &[DerivativeFileViewV1<'_>],
    ) -> Result<ForbiddenScanVerdictV1, DerivativeScannerError>;
}

pub(crate) struct SeededForbiddenScannerV1 {
    canaries: Vec<ForbiddenCanaryV1>,
    non_actionable_value_count: usize,
}

impl fmt::Debug for SeededForbiddenScannerV1 {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("SeededForbiddenScannerV1")
            .field("canary_count", &self.canaries.len())
            .field(
                "non_actionable_value_count",
                &self.non_actionable_value_count,
            )
            .field("representation_strategy", &"on-demand")
            .finish()
    }
}

impl SeededForbiddenScannerV1 {
    pub(crate) fn new(canaries: Vec<ForbiddenCanaryV1>) -> Result<Self, DerivativeError> {
        if canaries.is_empty() {
            return Err(DerivativeError::InvalidScannerConfiguration);
        }
        let mut seeds = CanaryAccumulator::default();
        for canary in canaries {
            seeds.push_canary(canary)?;
        }
        Self::from_accumulator(seeds)
    }

    /// Seeds the defense-in-depth scanner while the decrypted artifact remains
    /// inside the private boundary. The scanner itself must never be moved into
    /// UI state, logs, clipboard models, or ordinary support artifacts.
    pub(crate) fn from_private_capture(
        capture: &PrivateCaptureV1,
    ) -> Result<Self, DerivativeError> {
        let mut seeds = CanaryAccumulator::default();
        seed_capture_canaries(capture, &mut seeds)?;
        Self::from_accumulator(seeds)
    }

    pub(crate) fn from_private_captures(
        captures: &[&PrivateCaptureV1],
    ) -> Result<Self, DerivativeError> {
        if captures.is_empty() || captures.len() > MAX_SCANNER_SOURCES {
            return Err(DerivativeError::InvalidScannerConfiguration);
        }
        let mut merged = CanaryAccumulator::default();
        for capture in captures {
            seed_capture_canaries(capture, &mut merged)?;
        }
        Self::from_accumulator(merged)
    }

    fn from_accumulator(seeds: CanaryAccumulator) -> Result<Self, DerivativeError> {
        let (canaries, non_actionable_value_count) = seeds.into_parts();
        if canaries.is_empty() {
            return Err(DerivativeError::InvalidScannerConfiguration);
        }
        Ok(Self {
            canaries,
            non_actionable_value_count,
        })
    }
}

fn seed_capture_canaries(
    capture: &PrivateCaptureV1,
    seeds: &mut CanaryAccumulator,
) -> Result<(), DerivativeError> {
    let capture_ref = private_hex(&capture.capture_ref().bytes());
    seeds.push(capture_ref.as_bytes())?;
    let operation_ref = Zeroizing::new(capture.operation_ref().to_string());
    seeds.push(operation_ref.as_bytes())?;

    for record in capture.records() {
        if let Some(exchange_ref) = record.exchange_ref() {
            let exchange_ref = private_hex(&exchange_ref.bytes());
            seeds.push(exchange_ref.as_bytes())?;
        }
        if record.kind() == CaptureRecordKind::SyntheticFixture {
            seeds.push(record.payload().expose())?;
            continue;
        }
        let payload = match decode_fields(record.payload()) {
            Ok(payload) => payload,
            // Replay operation boundaries use their own strict private binary
            // schema. The complete payload remains a canary, while malformed
            // field payloads elsewhere still fail closed.
            Err(_) if record.kind() == CaptureRecordKind::OperationBoundary => {
                seeds.push(record.payload().expose())?;
                continue;
            }
            Err(_) => return Err(DerivativeError::InvalidScannerConfiguration),
        };
        for (tag, value) in payload.field_values() {
            match tag {
                private_field::HEADERS => seed_encoded_header_values(value, seeds)?,
                private_field::BODY => seed_body_values(value, seeds)?,
                _ if field_may_contain_private_value(tag) => seeds.push(value)?,
                _ => {}
            }
        }
    }

    Ok(())
}

const fn field_may_contain_private_value(tag: u16) -> bool {
    matches!(
        tag,
        private_field::URL
            | private_field::HEADERS
            | private_field::BODY
            | private_field::CLIENT_VERSION
            | private_field::USER_AGENT_PROFILE
            | private_field::PLAYABILITY_REASON
            | private_field::FORMAT_FACTS
            | private_field::SELECTED_ITAG
    ) || tag > private_field::REDIRECT_COUNT
}

#[derive(Default)]
struct CanaryAccumulator {
    canaries: Vec<ForbiddenCanaryV1>,
    digest_indexes: HashMap<[u8; 32], Vec<usize>>,
    total_bytes: usize,
    non_actionable_value_count: usize,
}

impl CanaryAccumulator {
    fn push(&mut self, value: &[u8]) -> Result<(), DerivativeError> {
        if value.is_empty() {
            self.note_non_actionable()?;
            return Ok(());
        }
        if value.len() < MIN_ACTIONABLE_CANARY_BYTES {
            self.note_non_actionable()?;
            return Ok(());
        }
        if value.len() > MAX_CANARY_BYTES {
            return Err(DerivativeError::InvalidScannerConfiguration);
        }
        let digest: [u8; 32] = Sha256::digest(value).into();
        if self.digest_indexes.get(&digest).is_some_and(|indexes| {
            indexes
                .iter()
                .any(|index| self.canaries[*index].0.expose() == value)
        }) {
            return Ok(());
        }
        if self.canaries.len() >= MAX_CANARIES {
            return Err(DerivativeError::InvalidScannerConfiguration);
        }
        let next_total = self
            .total_bytes
            .checked_add(value.len())
            .filter(|total| *total <= MAX_TOTAL_CANARY_BYTES)
            .ok_or(DerivativeError::InvalidScannerConfiguration)?;
        self.canaries.push(ForbiddenCanaryV1::new(value.to_vec())?);
        self.digest_indexes
            .entry(digest)
            .or_default()
            .push(self.canaries.len() - 1);
        self.total_bytes = next_total;
        Ok(())
    }

    fn push_canary(&mut self, canary: ForbiddenCanaryV1) -> Result<(), DerivativeError> {
        if canary.0.len() < MIN_ACTIONABLE_CANARY_BYTES {
            self.note_non_actionable()?;
            return Ok(());
        }
        let digest: [u8; 32] = Sha256::digest(canary.0.expose()).into();
        if self.digest_indexes.get(&digest).is_some_and(|indexes| {
            indexes
                .iter()
                .any(|index| self.canaries[*index].0.expose() == canary.0.expose())
        }) {
            return Ok(());
        }
        if self.canaries.len() >= MAX_CANARIES {
            return Err(DerivativeError::InvalidScannerConfiguration);
        }
        let next_total = self
            .total_bytes
            .checked_add(canary.0.len())
            .filter(|total| *total <= MAX_TOTAL_CANARY_BYTES)
            .ok_or(DerivativeError::InvalidScannerConfiguration)?;
        self.canaries.push(canary);
        self.digest_indexes
            .entry(digest)
            .or_default()
            .push(self.canaries.len() - 1);
        self.total_bytes = next_total;
        Ok(())
    }

    fn note_non_actionable(&mut self) -> Result<(), DerivativeError> {
        self.non_actionable_value_count = self
            .non_actionable_value_count
            .checked_add(1)
            .ok_or(DerivativeError::InvalidScannerConfiguration)?;
        Ok(())
    }

    fn into_parts(self) -> (Vec<ForbiddenCanaryV1>, usize) {
        (self.canaries, self.non_actionable_value_count)
    }
}

fn percent_encode(value: &[u8]) -> Result<Zeroizing<Vec<u8>>, DerivativeError> {
    percent_encode_with(value, b"0123456789ABCDEF")
}

fn percent_encode_lower(value: &[u8]) -> Result<Zeroizing<Vec<u8>>, DerivativeError> {
    percent_encode_with(value, b"0123456789abcdef")
}

fn percent_encode_with(
    value: &[u8],
    hex: &[u8; 16],
) -> Result<Zeroizing<Vec<u8>>, DerivativeError> {
    let capacity = value
        .len()
        .checked_mul(3)
        .ok_or(DerivativeError::InvalidScannerConfiguration)?;
    let mut encoded = Zeroizing::new(Vec::with_capacity(capacity));
    for byte in value {
        if byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'.' | b'_' | b'~') {
            encoded.push(*byte);
        } else {
            encoded.push(b'%');
            encoded.push(hex[usize::from(byte >> 4)]);
            encoded.push(hex[usize::from(byte & 0x0f)]);
        }
    }
    Ok(encoded)
}

fn seed_json_values(value: &[u8], seeds: &mut CanaryAccumulator) -> Result<(), DerivativeError> {
    // `IgnoredAny` validates the complete JSON grammar through serde_json's
    // string-skipping path without materializing private string values. Do not
    // interpret quote-like HTML as structured data and create noisy canaries.
    if serde_json::from_slice::<serde::de::IgnoredAny>(value).is_err() {
        return Ok(());
    }
    let mut cursor = 0_usize;
    while cursor < value.len() {
        if value[cursor] != b'"' {
            cursor += 1;
            continue;
        }
        let start = cursor + 1;
        let end =
            json_string_end(value, start).ok_or(DerivativeError::InvalidScannerConfiguration)?;
        cursor = end + 1;

        let next = value[cursor..]
            .iter()
            .copied()
            .find(|byte| !byte.is_ascii_whitespace());
        if next == Some(b':') {
            continue;
        }

        let encoded = &value[start..end];
        seeds.push(encoded)?;
        if encoded.contains(&b'\\') {
            let decoded =
                decode_json_string(encoded).ok_or(DerivativeError::InvalidScannerConfiguration)?;
            seeds.push(&decoded)?;
        }
    }
    Ok(())
}

fn seed_body_values(value: &[u8], seeds: &mut CanaryAccumulator) -> Result<(), DerivativeError> {
    if serde_json::from_slice::<serde::de::IgnoredAny>(value).is_ok() {
        seed_json_values(value, seeds)
    } else {
        seeds.push(value)
    }
}

fn json_string_end(input: &[u8], start: usize) -> Option<usize> {
    let mut cursor = start;
    while cursor < input.len() {
        match input[cursor] {
            b'"' => return Some(cursor),
            b'\\' => {
                cursor = cursor.checked_add(2)?;
            }
            byte if byte < 0x20 => return None,
            _ => cursor += 1,
        }
    }
    None
}

fn decode_json_string(encoded: &[u8]) -> Option<Zeroizing<Vec<u8>>> {
    let mut decoded = Zeroizing::new(Vec::with_capacity(encoded.len()));
    let mut cursor = 0_usize;
    while cursor < encoded.len() {
        if encoded[cursor] != b'\\' {
            let start = cursor;
            while cursor < encoded.len() && encoded[cursor] != b'\\' {
                if encoded[cursor] < 0x20 {
                    return None;
                }
                cursor += 1;
            }
            decoded.extend_from_slice(&encoded[start..cursor]);
            continue;
        }

        cursor += 1;
        let escaped = *encoded.get(cursor)?;
        cursor += 1;
        match escaped {
            b'"' | b'\\' | b'/' => decoded.push(escaped),
            b'b' => decoded.push(0x08),
            b'f' => decoded.push(0x0c),
            b'n' => decoded.push(b'\n'),
            b'r' => decoded.push(b'\r'),
            b't' => decoded.push(b'\t'),
            b'u' => {
                let high = take_json_hex_quad(encoded, &mut cursor)?;
                let scalar = if (0xd800..=0xdbff).contains(&high) {
                    if encoded.get(cursor..cursor.saturating_add(2)) != Some(br"\u") {
                        return None;
                    }
                    cursor += 2;
                    let low = take_json_hex_quad(encoded, &mut cursor)?;
                    if !(0xdc00..=0xdfff).contains(&low) {
                        return None;
                    }
                    0x1_0000 + ((u32::from(high) - 0xd800) << 10) + (u32::from(low) - 0xdc00)
                } else if (0xdc00..=0xdfff).contains(&high) {
                    return None;
                } else {
                    u32::from(high)
                };
                let character = char::from_u32(scalar)?;
                let mut utf8 = [0_u8; 4];
                decoded.extend_from_slice(character.encode_utf8(&mut utf8).as_bytes());
                utf8.zeroize();
            }
            _ => return None,
        }
    }
    std::str::from_utf8(&decoded).ok()?;
    Some(decoded)
}

fn take_json_hex_quad(input: &[u8], cursor: &mut usize) -> Option<u16> {
    let mut value = 0_u16;
    for byte in input.get(*cursor..cursor.checked_add(4)?)? {
        value = value.checked_mul(16)?.checked_add(u16::from(match byte {
            b'0'..=b'9' => byte - b'0',
            b'a'..=b'f' => byte - b'a' + 10,
            b'A'..=b'F' => byte - b'A' + 10,
            _ => return None,
        }))?;
    }
    *cursor += 4;
    Some(value)
}

fn seed_encoded_header_values(
    encoded: &[u8],
    seeds: &mut CanaryAccumulator,
) -> Result<(), DerivativeError> {
    let mut cursor = 0_usize;
    let count =
        take_u16(encoded, &mut cursor).ok_or(DerivativeError::InvalidScannerConfiguration)?;
    if usize::from(count) > MAX_PRIVATE_HEADERS {
        return Err(DerivativeError::InvalidScannerConfiguration);
    }
    for _ in 0..count {
        let name_len = usize::from(
            take_u16(encoded, &mut cursor).ok_or(DerivativeError::InvalidScannerConfiguration)?,
        );
        take_slice(encoded, &mut cursor, name_len)
            .ok_or(DerivativeError::InvalidScannerConfiguration)?;
        let value_len = usize::try_from(
            take_u32(encoded, &mut cursor).ok_or(DerivativeError::InvalidScannerConfiguration)?,
        )
        .map_err(|_| DerivativeError::InvalidScannerConfiguration)?;
        let value = take_slice(encoded, &mut cursor, value_len)
            .ok_or(DerivativeError::InvalidScannerConfiguration)?;
        seeds.push(value)?;
    }
    if cursor != encoded.len() {
        return Err(DerivativeError::InvalidScannerConfiguration);
    }
    Ok(())
}

fn take_u16(input: &[u8], cursor: &mut usize) -> Option<u16> {
    let bytes: [u8; 2] = take_slice(input, cursor, 2)?.try_into().ok()?;
    Some(u16::from_be_bytes(bytes))
}

fn take_u32(input: &[u8], cursor: &mut usize) -> Option<u32> {
    let bytes: [u8; 4] = take_slice(input, cursor, 4)?.try_into().ok()?;
    Some(u32::from_be_bytes(bytes))
}

fn take_slice<'a>(input: &'a [u8], cursor: &mut usize, len: usize) -> Option<&'a [u8]> {
    let end = cursor.checked_add(len)?;
    let value = input.get(*cursor..end)?;
    *cursor = end;
    Some(value)
}

fn private_hex(bytes: &[u8]) -> Zeroizing<String> {
    use fmt::Write as _;
    let mut output = Zeroizing::new(String::with_capacity(bytes.len().saturating_mul(2)));
    for byte in bytes {
        write!(output, "{byte:02x}").expect("write private reference canary");
    }
    output
}

fn files_contain(files: &[DerivativeFileViewV1<'_>], needle: &[u8]) -> bool {
    files
        .iter()
        .any(|file| contains_bytes(file.contents, needle))
}

fn scan_canary_representations(
    files: &[DerivativeFileViewV1<'_>],
    raw: &[u8],
) -> Result<bool, DerivativeScannerError> {
    if files.iter().all(|file| file.contents.len() < raw.len()) {
        return Ok(false);
    }
    if files_contain(files, raw) {
        return Ok(true);
    }

    if let Ok(text) = std::str::from_utf8(raw) {
        let capacity = text
            .len()
            .checked_mul(6)
            .and_then(|value| value.checked_add(2))
            .ok_or(DerivativeScannerError)?;
        let mut json = Zeroizing::new(Vec::with_capacity(capacity));
        serde_json::to_writer(&mut *json, text).map_err(|_| DerivativeScannerError)?;
        let interior = json
            .get(1..json.len().saturating_sub(1))
            .ok_or(DerivativeScannerError)?;
        if files_contain(files, interior) {
            return Ok(true);
        }
    }

    let percent = percent_encode(raw).map_err(|_| DerivativeScannerError)?;
    if files_contain(files, &percent) {
        return Ok(true);
    }
    let percent_lower = percent_encode_lower(raw).map_err(|_| DerivativeScannerError)?;
    if files_contain(files, &percent_lower) {
        return Ok(true);
    }

    for engine in [&STANDARD, &STANDARD_NO_PAD, &URL_SAFE, &URL_SAFE_NO_PAD] {
        let representation = Zeroizing::new(engine.encode(raw));
        if files_contain(files, representation.as_bytes()) {
            return Ok(true);
        }
    }
    Ok(false)
}

impl DerivativeForbiddenScannerV1 for SeededForbiddenScannerV1 {
    fn scan(
        &self,
        files: &[DerivativeFileViewV1<'_>],
    ) -> Result<ForbiddenScanVerdictV1, DerivativeScannerError> {
        let maximum_file_bytes = files
            .iter()
            .map(|file| file.contents.len())
            .max()
            .unwrap_or(0);
        let output_bytes = files.iter().try_fold(0_usize, |total, file| {
            total.checked_add(file.contents.len())
        });
        let searchable_canaries = self
            .canaries
            .iter()
            .filter(|canary| canary.0.expose().len() <= maximum_file_bytes)
            .count();
        let search_count = searchable_canaries
            .checked_mul(CANARY_REPRESENTATIONS_PER_VALUE)
            .and_then(|count| count.checked_add(FORBIDDEN_FIELD_NAMES.len()));
        if output_bytes.is_none_or(|bytes| bytes > MAX_DERIVATIVE_BYTES)
            || output_bytes
                .zip(search_count)
                .and_then(|(bytes, searches)| bytes.checked_mul(searches))
                .is_none_or(|work| work > MAX_SCANNER_SEARCH_BYTES)
        {
            return Err(DerivativeScannerError);
        }

        for forbidden in FORBIDDEN_FIELD_NAMES {
            if files_contain(files, forbidden) {
                return Ok(ForbiddenScanVerdictV1::Forbidden { findings: 1 });
            }
        }
        for canary in &self.canaries {
            if canary.0.expose().len() > maximum_file_bytes {
                continue;
            }
            if scan_canary_representations(files, canary.0.expose())? {
                return Ok(ForbiddenScanVerdictV1::Forbidden { findings: 1 });
            }
        }
        Ok(ForbiddenScanVerdictV1::Passed)
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct DerivativeIoError;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum DerivativeCommitStateV1 {
    Committed,
    CommittedDurabilityUncertain,
}

pub(crate) trait DerivativeAtomicWriterV1 {
    fn write_atomically(
        &mut self,
        files: &[DerivativeFileViewV1<'_>],
    ) -> Result<DerivativeCommitStateV1, DerivativeIoError>;
}

pub(crate) trait DerivativeReaderV1 {
    fn read_exact(&self, maximum_bytes: usize) -> Result<DerivativeFileSetV1, DerivativeIoError>;
}

pub(crate) trait DerivativeStoreV1: DerivativeAtomicWriterV1 + DerivativeReaderV1 {}

impl<T> DerivativeStoreV1 for T where T: DerivativeAtomicWriterV1 + DerivativeReaderV1 {}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct DerivativeCreateOutcomeV1 {
    review: DerivativeReviewV1,
    durability_confirmed: bool,
}

impl DerivativeCreateOutcomeV1 {
    pub(crate) const fn review(&self) -> &DerivativeReviewV1 {
        &self.review
    }

    pub(crate) const fn durability_confirmed(&self) -> bool {
        self.durability_confirmed
    }
}

#[derive(Clone)]
pub(crate) struct DerivativeFileSetV1 {
    evidence: Vec<u8>,
    checksums: Vec<u8>,
    manifest: Vec<u8>,
    unexpected_entries: u16,
}

impl fmt::Debug for DerivativeFileSetV1 {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("DerivativeFileSetV1")
            .field("evidence_bytes", &self.evidence.len())
            .field("checksum_bytes", &self.checksums.len())
            .field("manifest_bytes", &self.manifest.len())
            .field("unexpected_entries", &self.unexpected_entries)
            .finish()
    }
}

impl DerivativeFileSetV1 {
    pub(crate) fn new(
        evidence: Vec<u8>,
        checksums: Vec<u8>,
        manifest: Vec<u8>,
        unexpected_entries: u16,
    ) -> Self {
        Self {
            evidence,
            checksums,
            manifest,
            unexpected_entries,
        }
    }

    pub(super) fn views(&self) -> [DerivativeFileViewV1<'_>; 3] {
        [
            DerivativeFileViewV1 {
                name: DerivativeFileNameV1::Evidence,
                contents: &self.evidence,
            },
            DerivativeFileViewV1 {
                name: DerivativeFileNameV1::Checksums,
                contents: &self.checksums,
            },
            DerivativeFileViewV1 {
                name: DerivativeFileNameV1::Manifest,
                contents: &self.manifest,
            },
        ]
    }

    #[cfg(test)]
    pub(super) const fn unexpected_entries(&self) -> u16 {
        self.unexpected_entries
    }

    fn total_bytes(&self) -> Option<usize> {
        self.evidence
            .len()
            .checked_add(self.checksums.len())?
            .checked_add(self.manifest.len())
    }
}

pub(crate) fn prepare_derivative_v1(
    derivative: ProviderDiagnosticDerivativeV1,
    scanner: &impl DerivativeForbiddenScannerV1,
) -> Result<PreparedDerivativeV1, DerivativeError> {
    let derivative = derivative.normalize_and_validate()?;
    let evidence = canonical_json(&derivative)?;
    let evidence_checksum = checksum(&evidence);
    let manifest = DerivativeManifestV1 {
        manifest_version: DERIVATIVE_MANIFEST_VERSION,
        derivative_schema_version: DERIVATIVE_SCHEMA_VERSION,
        application_version: embedded_application_version(),
        source_revision: embedded_source_revision(),
        source_dirty: embedded_dirty_state(),
        completeness: derivative.completeness(),
        forbidden_scan: DerivativeScanStateV1::Passed,
        privacy_boundary: PRIVACY_BOUNDARY.to_owned(),
        support_bundle_relationship: BUNDLE_RELATIONSHIP.to_owned(),
        file_count: 3,
        evidence: DerivativeManifestEvidenceV1 {
            name: DerivativeFileNameV1::Evidence.as_str().to_owned(),
            bytes: u32::try_from(evidence.len()).map_err(|_| DerivativeError::SizeLimitExceeded)?,
            sha256: evidence_checksum.clone(),
        },
    };
    let manifest = canonical_json(&manifest)?;
    let checksums = checksum_file(&evidence_checksum, &checksum(&manifest)).into_bytes();
    let files = DerivativeFileSetV1::new(evidence, checksums, manifest, 0);
    let review = review_file_set_v1(&files, scanner)?;
    Ok(PreparedDerivativeV1 {
        evidence: files.evidence,
        checksums: files.checksums,
        manifest: files.manifest,
        review,
    })
}

pub(crate) fn create_derivative_with_v1(
    prepared: &PreparedDerivativeV1,
    store: &mut impl DerivativeStoreV1,
    scanner: &impl DerivativeForbiddenScannerV1,
) -> Result<DerivativeCreateOutcomeV1, DerivativeError> {
    match scanner
        .scan(&prepared.files())
        .map_err(|_| DerivativeError::ScannerFailed)?
    {
        ForbiddenScanVerdictV1::Passed => {}
        ForbiddenScanVerdictV1::Forbidden { .. } => {
            return Err(DerivativeError::ForbiddenDataFound);
        }
    }
    let commit = store
        .write_atomically(&prepared.files())
        .map_err(|_| DerivativeError::WriteFailed)?;
    Ok(DerivativeCreateOutcomeV1 {
        review: prepared.review.clone(),
        durability_confirmed: commit == DerivativeCommitStateV1::Committed,
    })
}

pub(crate) fn review_derivative_with_v1(
    reader: &impl DerivativeReaderV1,
    scanner: &impl DerivativeForbiddenScannerV1,
) -> Result<DerivativeReviewV1, DerivativeError> {
    let files = reader
        .read_exact(MAX_DERIVATIVE_BYTES)
        .map_err(|_| DerivativeError::ReadFailed)?;
    review_file_set_v1(&files, scanner)
}

fn review_file_set_v1(
    files: &DerivativeFileSetV1,
    scanner: &impl DerivativeForbiddenScannerV1,
) -> Result<DerivativeReviewV1, DerivativeError> {
    if files.unexpected_entries != 0 {
        return Err(DerivativeError::UnexpectedEntry);
    }
    let total_bytes = files
        .total_bytes()
        .filter(|total| *total <= MAX_DERIVATIVE_BYTES)
        .ok_or(DerivativeError::SizeLimitExceeded)?;
    let manifest: DerivativeManifestV1 = decode_canonical_json(&files.manifest)?;
    if manifest.manifest_version != DERIVATIVE_MANIFEST_VERSION
        || manifest.derivative_schema_version != DERIVATIVE_SCHEMA_VERSION
    {
        return Err(DerivativeError::UnsupportedSchema);
    }
    validate_manifest(&manifest, &files.evidence)?;
    let expected_checksums = checksum_file(&checksum(&files.evidence), &checksum(&files.manifest));
    if files.checksums != expected_checksums.as_bytes() {
        return Err(DerivativeError::ChecksumMismatch);
    }
    let derivative: ProviderDiagnosticDerivativeV1 = decode_canonical_json(&files.evidence)?;
    if derivative.schema_version != DERIVATIVE_SCHEMA_VERSION {
        return Err(DerivativeError::UnsupportedSchema);
    }
    let normalized = derivative.clone().normalize_and_validate()?;
    if normalized != derivative || derivative.completeness != manifest.completeness {
        return Err(DerivativeError::NonCanonicalEncoding);
    }
    match scanner
        .scan(&files.views())
        .map_err(|_| DerivativeError::ScannerFailed)?
    {
        ForbiddenScanVerdictV1::Passed => {}
        ForbiddenScanVerdictV1::Forbidden { .. } => {
            return Err(DerivativeError::ForbiddenDataFound);
        }
    }
    Ok(DerivativeReviewV1 {
        schema_version: derivative.schema_version,
        completeness: derivative.completeness,
        file_count: 3,
        fact_count: u16::try_from(derivative.fact_count()).unwrap_or(u16::MAX),
        finding_count: u16::try_from(derivative.finding_count()).unwrap_or(u16::MAX),
        total_bytes: u32::try_from(total_bytes).map_err(|_| DerivativeError::SizeLimitExceeded)?,
        checksum_valid: true,
        forbidden_scan_passed: true,
    })
}

fn validate_manifest(
    manifest: &DerivativeManifestV1,
    evidence: &[u8],
) -> Result<(), DerivativeError> {
    if manifest.application_version.is_empty()
        || manifest.application_version.len() > 32
        || !manifest
            .application_version
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'-' | b'+'))
        || !safe_revision(&manifest.source_revision)
        || manifest.forbidden_scan != DerivativeScanStateV1::Passed
        || manifest.privacy_boundary != PRIVACY_BOUNDARY
        || manifest.support_bundle_relationship != BUNDLE_RELATIONSHIP
        || manifest.file_count != 3
        || manifest.evidence.name != DerivativeFileNameV1::Evidence.as_str()
    {
        return Err(DerivativeError::ManifestPolicyMismatch);
    }
    if usize::try_from(manifest.evidence.bytes).ok() != Some(evidence.len())
        || manifest.evidence.sha256 != checksum(evidence)
    {
        return Err(DerivativeError::ChecksumMismatch);
    }
    Ok(())
}

fn embedded_application_version() -> String {
    env!("CARGO_PKG_VERSION").to_owned()
}

fn embedded_source_revision() -> String {
    option_env!("UNIFIED_PLAYER_GIT_REVISION")
        .filter(|revision| safe_revision(revision))
        .unwrap_or("unknown")
        .to_owned()
}

fn embedded_dirty_state() -> DerivativeDirtyStateV1 {
    match option_env!("UNIFIED_PLAYER_GIT_DIRTY") {
        Some("true") => DerivativeDirtyStateV1::True,
        Some("false") => DerivativeDirtyStateV1::False,
        _ => DerivativeDirtyStateV1::Unknown,
    }
}

fn safe_revision(revision: &str) -> bool {
    revision == "unknown"
        || ((7..=40).contains(&revision.len())
            && revision.bytes().all(|byte| {
                byte.is_ascii_hexdigit()
                    && (!byte.is_ascii_alphabetic() || byte.is_ascii_lowercase())
            }))
}

fn canonical_json(value: &impl Serialize) -> Result<Vec<u8>, DerivativeError> {
    let mut output = serde_json::to_vec(value).map_err(|_| DerivativeError::InvalidEncoding)?;
    output.push(b'\n');
    if output.len() > MAX_DERIVATIVE_BYTES {
        return Err(DerivativeError::SizeLimitExceeded);
    }
    Ok(output)
}

fn decode_canonical_json<T>(bytes: &[u8]) -> Result<T, DerivativeError>
where
    T: DeserializeOwned + Serialize,
{
    if bytes.len() > MAX_DERIVATIVE_BYTES {
        return Err(DerivativeError::SizeLimitExceeded);
    }
    let value = serde_json::from_slice::<T>(bytes).map_err(|_| DerivativeError::InvalidEncoding)?;
    if canonical_json(&value)? != bytes {
        return Err(DerivativeError::NonCanonicalEncoding);
    }
    Ok(value)
}

fn checksum(bytes: &[u8]) -> String {
    let digest = Sha256::digest(bytes);
    let mut output = String::with_capacity(64);
    for byte in digest {
        use fmt::Write as _;
        write!(output, "{byte:02x}").expect("write SHA-256 to String");
    }
    output
}

fn checksum_file(evidence_checksum: &str, manifest_checksum: &str) -> String {
    let evidence_name = DerivativeFileNameV1::Evidence.as_str();
    let manifest_name = DerivativeFileNameV1::Manifest.as_str();
    format!("{evidence_checksum}  {evidence_name}\n{manifest_checksum}  {manifest_name}\n")
}

fn contains_bytes(haystack: &[u8], needle: &[u8]) -> bool {
    !needle.is_empty()
        && haystack
            .windows(needle.len())
            .any(|window| window == needle)
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum DerivativeError {
    ChecksumMismatch,
    DuplicateFact,
    DuplicateOrInvalidTiming,
    ForbiddenDataFound,
    InputLimitExceeded,
    InvalidEncoding,
    InvalidScannerConfiguration,
    InvalidTypedInput,
    ManifestPolicyMismatch,
    NonCanonicalEncoding,
    ReadFailed,
    ScannerFailed,
    SizeLimitExceeded,
    UnexpectedEntry,
    UnsupportedSchema,
    WriteFailed,
}

impl fmt::Display for DerivativeError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::ChecksumMismatch => "provider diagnostic derivative checksum verification failed",
            Self::DuplicateFact => "provider diagnostic derivative contains a duplicate fact",
            Self::DuplicateOrInvalidTiming => {
                "provider diagnostic derivative contains an invalid timing"
            }
            Self::ForbiddenDataFound => {
                "provider diagnostic derivative forbidden-data scan rejected output"
            }
            Self::InputLimitExceeded => "provider diagnostic derivative input exceeds its limit",
            Self::InvalidEncoding => "provider diagnostic derivative encoding is invalid",
            Self::InvalidScannerConfiguration => {
                "provider diagnostic derivative scanner configuration is invalid"
            }
            Self::InvalidTypedInput => "provider diagnostic derivative typed input is invalid",
            Self::ManifestPolicyMismatch => {
                "provider diagnostic derivative manifest policy is invalid"
            }
            Self::NonCanonicalEncoding => {
                "provider diagnostic derivative encoding is not canonical"
            }
            Self::ReadFailed => "provider diagnostic derivative could not be read",
            Self::ScannerFailed => "provider diagnostic derivative scan did not complete",
            Self::SizeLimitExceeded => "provider diagnostic derivative exceeds its size limit",
            Self::UnexpectedEntry => "provider diagnostic derivative contains an unexpected entry",
            Self::UnsupportedSchema => "provider diagnostic derivative schema is unsupported",
            Self::WriteFailed => "provider diagnostic derivative could not be created",
        })
    }
}

impl std::error::Error for DerivativeError {}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use super::*;
    use crate::developer_capture::{
        CaptureCompleteness, CapturePurpose, CaptureRecordKind, CaptureRecordV1, CaptureRef,
        SafeOperationRef, SafeTerminalCategory, SensitiveBytes,
    };
    use static_assertions::assert_not_impl_any;

    assert_not_impl_any!(ForbiddenCanaryV1: Clone, std::fmt::Display, serde::Serialize);
    assert_not_impl_any!(SeededForbiddenScannerV1: Clone, std::fmt::Display, serde::Serialize);

    const PRIVATE_CAPTURE_REF: &str = "9f4d6a7b8c1e2f30415263748596a0bd";
    const PRIVATE_PATH: &str = "C:\\private\\capture\\fixture.age";
    const PRIVATE_MEDIA_ID: &str = "fixture-private-media-id";
    const PRIVATE_URL: &str = "https://private.invalid/watch?secret=query";
    const PRIVATE_COOKIE: &str = "Cookie: fixture-private-cookie";
    const PRIVATE_AUTH: &str = "Authorization: Bearer fixture-private-token";
    const PRIVATE_TITLE: &str = "fixture-private-title";
    const PRIVATE_ARTIST: &str = "fixture-private-artist";
    const PRIVATE_LYRIC: &str = "fixture-private-lyric";
    const PRIVATE_BODY: &str = "fixture-private-provider-body";

    fn finding() -> DerivativeFindingV1 {
        DerivativeFindingV1::new(
            DerivativeFindingCategoryV1::Playability,
            DerivativeFindingSeverityV1::Material,
            DerivativeRegisteredFieldV1::PlayabilityCategory,
            DerivativeValueClassV1::Playable,
            DerivativeValueClassV1::Unavailable,
            DerivativeExplanationV1::PlayabilityChanged,
        )
    }

    fn derivative(reverse: bool) -> ProviderDiagnosticDerivativeV1 {
        let mut transports = vec![
            DerivativeTransportV1::MediaRange,
            DerivativeTransportV1::NativeHttp,
        ];
        let mut facts = vec![
            DerivativeFactV1::Playability(DerivativePlayabilityV1::ProviderUnavailable),
            DerivativeFactV1::PlayerHttpStatusClass(DerivativeHttpStatusClassV1::Success),
            DerivativeFactV1::StreamingDataPresent(false),
            DerivativeFactV1::SupportedFormats(0),
            DerivativeFactV1::BrowserComparison(DerivativeBrowserComparisonV1::NotRequested),
            DerivativeFactV1::MediaResult(DerivativeMediaResultV1::NotReached),
        ];
        let mut timings = vec![
            DerivativeTimingV1::new(DerivativeStageV1::PlayerResponse, 37).unwrap(),
            DerivativeTimingV1::new(DerivativeStageV1::Parser, 2).unwrap(),
        ];
        if reverse {
            transports.reverse();
            facts.reverse();
            timings.reverse();
        }
        ProviderDiagnosticDerivativeV1::new(
            DerivativeOperationV1::Comparison,
            DerivativeCompletenessV1::Complete,
            DerivativeTerminalV1::Failed,
            transports,
            facts,
            timings,
            vec![finding()],
        )
        .unwrap()
    }

    fn canary_values() -> Vec<&'static str> {
        vec![
            PRIVATE_CAPTURE_REF,
            PRIVATE_PATH,
            PRIVATE_MEDIA_ID,
            PRIVATE_URL,
            PRIVATE_COOKIE,
            PRIVATE_AUTH,
            PRIVATE_TITLE,
            PRIVATE_ARTIST,
            PRIVATE_LYRIC,
            PRIVATE_BODY,
        ]
    }

    fn synthetic_capture(capture_byte: u8, payloads: Vec<Vec<u8>>) -> PrivateCaptureV1 {
        let records = payloads
            .into_iter()
            .enumerate()
            .map(|(sequence, payload)| {
                CaptureRecordV1::new(
                    u16::try_from(sequence).unwrap(),
                    u64::try_from(sequence).unwrap(),
                    CaptureRecordKind::SyntheticFixture,
                    SensitiveBytes::new(payload),
                )
            })
            .collect();
        PrivateCaptureV1::new(
            CaptureRef::from_bytes([capture_byte; 16]),
            1,
            2,
            CapturePurpose::InteractivePlayback,
            SafeOperationRef::from_bytes([capture_byte; 4]),
            records,
            CaptureCompleteness::Complete,
            Vec::new(),
            0,
            SafeTerminalCategory::Success,
        )
    }

    fn scanner() -> SeededForbiddenScannerV1 {
        SeededForbiddenScannerV1::new(
            canary_values()
                .into_iter()
                .map(|value| ForbiddenCanaryV1::new(value.as_bytes().to_vec()).unwrap())
                .collect(),
        )
        .unwrap()
    }

    fn file_set(prepared: &PreparedDerivativeV1) -> DerivativeFileSetV1 {
        DerivativeFileSetV1::new(
            prepared.evidence.clone(),
            prepared.checksums.clone(),
            prepared.manifest.clone(),
            0,
        )
    }

    fn refresh_checksums(files: &mut DerivativeFileSetV1) {
        files.checksums =
            checksum_file(&checksum(&files.evidence), &checksum(&files.manifest)).into_bytes();
    }

    #[derive(Default)]
    struct MemoryStore {
        files: BTreeMap<DerivativeFileNameV1, Vec<u8>>,
        unexpected_entries: u16,
        fail_write: bool,
        fail_read: bool,
        durability_uncertain: bool,
    }

    impl DerivativeAtomicWriterV1 for MemoryStore {
        fn write_atomically(
            &mut self,
            files: &[DerivativeFileViewV1<'_>],
        ) -> Result<DerivativeCommitStateV1, DerivativeIoError> {
            if self.fail_write {
                return Err(DerivativeIoError);
            }
            let next = files
                .iter()
                .map(|file| (file.name(), file.contents().to_vec()))
                .collect::<BTreeMap<_, _>>();
            self.files = next;
            Ok(if self.durability_uncertain {
                DerivativeCommitStateV1::CommittedDurabilityUncertain
            } else {
                DerivativeCommitStateV1::Committed
            })
        }
    }

    impl DerivativeReaderV1 for MemoryStore {
        fn read_exact(
            &self,
            _maximum_bytes: usize,
        ) -> Result<DerivativeFileSetV1, DerivativeIoError> {
            if self.fail_read {
                return Err(DerivativeIoError);
            }
            Ok(DerivativeFileSetV1::new(
                self.files
                    .get(&DerivativeFileNameV1::Evidence)
                    .cloned()
                    .ok_or(DerivativeIoError)?,
                self.files
                    .get(&DerivativeFileNameV1::Checksums)
                    .cloned()
                    .ok_or(DerivativeIoError)?,
                self.files
                    .get(&DerivativeFileNameV1::Manifest)
                    .cloned()
                    .ok_or(DerivativeIoError)?,
                self.unexpected_entries,
            ))
        }
    }

    struct FailingScanner;

    impl DerivativeForbiddenScannerV1 for FailingScanner {
        fn scan(
            &self,
            _files: &[DerivativeFileViewV1<'_>],
        ) -> Result<ForbiddenScanVerdictV1, DerivativeScannerError> {
            Err(DerivativeScannerError)
        }
    }

    #[test]
    fn typed_allowlist_output_is_deterministic_canonical_and_bounded() {
        let first = prepare_derivative_v1(derivative(false), &scanner()).unwrap();
        let second = prepare_derivative_v1(derivative(true), &scanner()).unwrap();

        assert_eq!(first.evidence, second.evidence);
        assert_eq!(first.manifest, second.manifest);
        assert_eq!(first.checksums, second.checksums);
        assert!(first.total_bytes() <= MAX_DERIVATIVE_BYTES);
        assert_eq!(first.files().len(), 3);
        assert!(first.review().checksum_valid());
        assert!(first.review().forbidden_scan_passed());
    }

    #[test]
    fn private_fixture_canaries_never_enter_the_derivative() {
        let private_fixture = canary_values().join("\n");
        let scanner = scanner();
        assert!(matches!(
            scanner
                .scan(&[DerivativeFileViewV1 {
                    name: DerivativeFileNameV1::Evidence,
                    contents: private_fixture.as_bytes(),
                }])
                .unwrap(),
            ForbiddenScanVerdictV1::Forbidden { .. }
        ));

        let prepared = prepare_derivative_v1(derivative(false), &scanner).unwrap();
        for file in prepared.files() {
            for canary in canary_values() {
                assert!(!contains_bytes(file.contents(), canary.as_bytes()));
            }
        }
    }

    #[test]
    fn decrypted_capture_seeds_stay_private_and_reject_a_leak() {
        let capture_ref = CaptureRef::from_bytes([0x5a; 16]);
        let capture = PrivateCaptureV1::new(
            capture_ref,
            1,
            2,
            CapturePurpose::InteractivePlayback,
            SafeOperationRef::from_bytes([1, 2, 3, 4]),
            vec![CaptureRecordV1::new(
                0,
                0,
                CaptureRecordKind::SyntheticFixture,
                SensitiveBytes::new(PRIVATE_BODY.as_bytes().to_vec()),
            )],
            CaptureCompleteness::Complete,
            Vec::new(),
            0,
            SafeTerminalCategory::Success,
        );
        let scanner = SeededForbiddenScannerV1::from_private_capture(&capture).unwrap();
        let prepared = prepare_derivative_v1(derivative(false), &scanner).unwrap();
        assert!(prepared
            .files()
            .iter()
            .all(|file| !contains_bytes(file.contents(), PRIVATE_BODY.as_bytes())));
        assert!(matches!(
            scanner
                .scan(&[DerivativeFileViewV1 {
                    name: DerivativeFileNameV1::Evidence,
                    contents: PRIVATE_BODY.as_bytes(),
                }])
                .unwrap(),
            ForbiddenScanVerdictV1::Forbidden { .. }
        ));
    }

    #[test]
    fn structured_json_keys_do_not_false_positive_but_escaped_values_do() {
        use crate::developer_capture::payload::{encode_fields, PrivateField, PrivatePayloadKind};

        let payload = encode_fields(
            PrivatePayloadKind::HttpRequest,
            &[PrivateField::bytes(
                private_field::BODY,
                br#"{"playability":"private-\u0131-identifier"}"#,
            )],
        )
        .unwrap();
        let capture = PrivateCaptureV1::new(
            CaptureRef::from_bytes([0x44; 16]),
            1,
            2,
            CapturePurpose::InteractivePlayback,
            SafeOperationRef::from_bytes([4, 3, 2, 1]),
            vec![CaptureRecordV1::new(
                0,
                0,
                CaptureRecordKind::HttpRequest,
                payload,
            )],
            CaptureCompleteness::Complete,
            Vec::new(),
            0,
            SafeTerminalCategory::Success,
        );
        let scanner = SeededForbiddenScannerV1::from_private_capture(&capture).unwrap();

        // "playability" is a legitimate allowlisted derivative key, not a
        // private value and therefore must not make every derivative fail.
        prepare_derivative_v1(derivative(false), &scanner).unwrap();

        let leaked =
            serde_json::to_vec(&serde_json::json!({ "safe": "private-ı-identifier" })).unwrap();
        assert!(matches!(
            scanner
                .scan(&[DerivativeFileViewV1 {
                    name: DerivativeFileNameV1::Evidence,
                    contents: &leaked,
                }])
                .unwrap(),
            ForbiddenScanVerdictV1::Forbidden { .. }
        ));
    }

    #[test]
    fn replay_binary_operation_boundaries_are_seeded_without_field_decoding() {
        let private_replay = b"SPPREPLAY-private-replay-binary-value";
        let capture = PrivateCaptureV1::new(
            CaptureRef::from_bytes([0x55; 16]),
            1,
            2,
            CapturePurpose::Replay,
            SafeOperationRef::from_bytes([5, 5, 5, 5]),
            vec![CaptureRecordV1::new(
                0,
                0,
                CaptureRecordKind::OperationBoundary,
                SensitiveBytes::new(private_replay.to_vec()),
            )],
            CaptureCompleteness::Complete,
            Vec::new(),
            0,
            SafeTerminalCategory::Success,
        );
        let scanner = SeededForbiddenScannerV1::from_private_capture(&capture).unwrap();
        prepare_derivative_v1(derivative(false), &scanner).unwrap();
        assert!(matches!(
            scanner
                .scan(&[DerivativeFileViewV1 {
                    name: DerivativeFileNameV1::Evidence,
                    contents: private_replay,
                }])
                .unwrap(),
            ForbiddenScanVerdictV1::Forbidden { .. }
        ));
    }

    #[test]
    fn preview_contains_the_exact_bytes_that_creation_writes() {
        let prepared = prepare_derivative_v1(derivative(false), &scanner()).unwrap();
        let preview = prepared.preview();
        let mut store = MemoryStore::default();
        let created = create_derivative_with_v1(&prepared, &mut store, &scanner()).unwrap();

        assert_eq!(preview.total_bytes(), prepared.total_bytes());
        for preview_file in preview.files() {
            assert_eq!(
                store.files[&preview_file.name()],
                preview_file.contents().as_bytes()
            );
        }
        assert_eq!(created.review(), prepared.review());
        assert!(created.durability_confirmed());
    }

    #[test]
    fn create_and_review_are_separate_from_filesystem_and_support_bundle_code() {
        let prepared = prepare_derivative_v1(derivative(false), &scanner()).unwrap();
        let mut store = MemoryStore::default();
        create_derivative_with_v1(&prepared, &mut store, &scanner()).unwrap();
        let review = review_derivative_with_v1(&store, &scanner()).unwrap();

        assert_eq!(review.schema_version(), DERIVATIVE_SCHEMA_VERSION);
        assert_eq!(review.completeness(), DerivativeCompletenessV1::Complete);
        assert!(review.checksum_valid());
        assert!(review.forbidden_scan_passed());
    }

    #[test]
    fn committed_publication_never_becomes_an_ordinary_postwrite_failure() {
        let prepared = prepare_derivative_v1(derivative(false), &scanner()).unwrap();
        let mut store = MemoryStore {
            fail_read: true,
            durability_uncertain: true,
            ..MemoryStore::default()
        };
        let outcome = create_derivative_with_v1(&prepared, &mut store, &scanner()).unwrap();

        assert!(!outcome.durability_confirmed());
        assert_eq!(outcome.review(), prepared.review());
        assert_eq!(store.files.len(), 3);
    }

    #[test]
    fn evidence_and_checksum_tampering_are_rejected() {
        let prepared = prepare_derivative_v1(derivative(false), &scanner()).unwrap();
        let mut evidence_tamper = file_set(&prepared);
        evidence_tamper.evidence[20] ^= 1;
        assert_eq!(
            review_file_set_v1(&evidence_tamper, &scanner()),
            Err(DerivativeError::ChecksumMismatch)
        );

        let mut checksum_tamper = file_set(&prepared);
        checksum_tamper.checksums[0] = if checksum_tamper.checksums[0] == b'0' {
            b'1'
        } else {
            b'0'
        };
        assert_eq!(
            review_file_set_v1(&checksum_tamper, &scanner()),
            Err(DerivativeError::ChecksumMismatch)
        );
    }

    #[test]
    fn unsupported_schema_and_unknown_fields_are_rejected() {
        let prepared = prepare_derivative_v1(derivative(false), &scanner()).unwrap();
        let mut schema_tamper = file_set(&prepared);
        let mut manifest: DerivativeManifestV1 =
            decode_canonical_json(&schema_tamper.manifest).unwrap();
        manifest.derivative_schema_version = 2;
        schema_tamper.manifest = canonical_json(&manifest).unwrap();
        refresh_checksums(&mut schema_tamper);
        assert_eq!(
            review_file_set_v1(&schema_tamper, &scanner()),
            Err(DerivativeError::UnsupportedSchema)
        );

        let mut evidence_schema_tamper = file_set(&prepared);
        let mut evidence: ProviderDiagnosticDerivativeV1 =
            decode_canonical_json(&evidence_schema_tamper.evidence).unwrap();
        evidence.schema_version = 2;
        evidence_schema_tamper.evidence = canonical_json(&evidence).unwrap();
        let mut manifest: DerivativeManifestV1 =
            decode_canonical_json(&evidence_schema_tamper.manifest).unwrap();
        manifest.evidence.bytes = u32::try_from(evidence_schema_tamper.evidence.len()).unwrap();
        manifest.evidence.sha256 = checksum(&evidence_schema_tamper.evidence);
        evidence_schema_tamper.manifest = canonical_json(&manifest).unwrap();
        refresh_checksums(&mut evidence_schema_tamper);
        assert_eq!(
            review_file_set_v1(&evidence_schema_tamper, &scanner()),
            Err(DerivativeError::UnsupportedSchema)
        );

        let mut unknown_field = file_set(&prepared);
        let mut evidence: serde_json::Value =
            serde_json::from_slice(&unknown_field.evidence).unwrap();
        evidence["capture_ref"] = serde_json::json!(PRIVATE_CAPTURE_REF);
        unknown_field.evidence = canonical_json(&evidence).unwrap();
        let mut manifest: DerivativeManifestV1 =
            decode_canonical_json(&unknown_field.manifest).unwrap();
        manifest.evidence.bytes = u32::try_from(unknown_field.evidence.len()).unwrap();
        manifest.evidence.sha256 = checksum(&unknown_field.evidence);
        unknown_field.manifest = canonical_json(&manifest).unwrap();
        refresh_checksums(&mut unknown_field);
        assert_eq!(
            review_file_set_v1(&unknown_field, &scanner()),
            Err(DerivativeError::InvalidEncoding)
        );
    }

    #[test]
    fn a_seeded_private_ref_is_rejected_even_after_checksums_are_recomputed() {
        let prepared = prepare_derivative_v1(derivative(false), &scanner()).unwrap();
        let mut files = file_set(&prepared);
        let mut manifest: DerivativeManifestV1 = decode_canonical_json(&files.manifest).unwrap();
        manifest.source_revision = PRIVATE_CAPTURE_REF.to_owned();
        files.manifest = canonical_json(&manifest).unwrap();
        refresh_checksums(&mut files);

        assert_eq!(
            review_file_set_v1(&files, &scanner()),
            Err(DerivativeError::ForbiddenDataFound)
        );
    }

    #[test]
    fn traversal_paths_refs_and_forbidden_keys_have_no_typed_output_channel() {
        let prepared = prepare_derivative_v1(derivative(false), &scanner()).unwrap();
        let output = prepared
            .files()
            .into_iter()
            .flat_map(|file| file.contents().iter().copied())
            .collect::<Vec<_>>();

        for private in [
            PRIVATE_CAPTURE_REF,
            PRIVATE_PATH,
            "../escape",
            "/absolute/path",
        ] {
            assert!(!contains_bytes(&output, private.as_bytes()));
        }
        for forbidden in FORBIDDEN_FIELD_NAMES {
            assert!(!contains_bytes(&output, forbidden));
        }

        let mut traversal_manifest = file_set(&prepared);
        let mut manifest: DerivativeManifestV1 =
            decode_canonical_json(&traversal_manifest.manifest).unwrap();
        manifest.evidence.name = "../evidence.json".to_owned();
        traversal_manifest.manifest = canonical_json(&manifest).unwrap();
        refresh_checksums(&mut traversal_manifest);
        assert_eq!(
            review_file_set_v1(&traversal_manifest, &scanner()),
            Err(DerivativeError::ManifestPolicyMismatch)
        );
    }

    #[test]
    fn scanner_failure_writer_failure_reader_failure_and_extra_entries_fail_closed() {
        assert_eq!(
            prepare_derivative_v1(derivative(false), &FailingScanner).unwrap_err(),
            DerivativeError::ScannerFailed
        );

        let prepared = prepare_derivative_v1(derivative(false), &scanner()).unwrap();
        assert_eq!(
            review_file_set_v1(&file_set(&prepared), &FailingScanner),
            Err(DerivativeError::ScannerFailed)
        );
        let mut failing_writer = MemoryStore {
            fail_write: true,
            ..MemoryStore::default()
        };
        assert_eq!(
            create_derivative_with_v1(&prepared, &mut failing_writer, &scanner()),
            Err(DerivativeError::WriteFailed)
        );

        let failing_reader = MemoryStore {
            fail_read: true,
            ..MemoryStore::default()
        };
        assert_eq!(
            review_derivative_with_v1(&failing_reader, &scanner()),
            Err(DerivativeError::ReadFailed)
        );

        let mut extra_entry = file_set(&prepared);
        extra_entry.unexpected_entries = 1;
        assert_eq!(
            review_file_set_v1(&extra_entry, &scanner()),
            Err(DerivativeError::UnexpectedEntry)
        );
    }

    #[test]
    fn malformed_noncanonical_and_over_limit_inputs_are_rejected() {
        let duplicate = ProviderDiagnosticDerivativeV1::new(
            DerivativeOperationV1::Comparison,
            DerivativeCompletenessV1::Complete,
            DerivativeTerminalV1::Success,
            vec![DerivativeTransportV1::NativeHttp],
            vec![
                DerivativeFactV1::StreamingDataPresent(true),
                DerivativeFactV1::StreamingDataPresent(false),
            ],
            Vec::new(),
            Vec::new(),
        );
        assert_eq!(duplicate, Err(DerivativeError::DuplicateFact));

        let findings = vec![finding(); MAX_FINDINGS + 1];
        assert_eq!(
            ProviderDiagnosticDerivativeV1::new(
                DerivativeOperationV1::Comparison,
                DerivativeCompletenessV1::Incomplete,
                DerivativeTerminalV1::Inconclusive,
                vec![DerivativeTransportV1::OfflineReplay],
                Vec::new(),
                Vec::new(),
                findings,
            ),
            Err(DerivativeError::InputLimitExceeded)
        );

        let prepared = prepare_derivative_v1(derivative(false), &scanner()).unwrap();
        let mut whitespace = file_set(&prepared);
        whitespace.evidence.insert(0, b' ');
        let mut manifest: DerivativeManifestV1 =
            decode_canonical_json(&whitespace.manifest).unwrap();
        manifest.evidence.bytes = u32::try_from(whitespace.evidence.len()).unwrap();
        manifest.evidence.sha256 = checksum(&whitespace.evidence);
        whitespace.manifest = canonical_json(&manifest).unwrap();
        refresh_checksums(&mut whitespace);
        assert_eq!(
            review_file_set_v1(&whitespace, &scanner()),
            Err(DerivativeError::NonCanonicalEncoding)
        );

        let mut oversized = file_set(&prepared);
        oversized.evidence = vec![b'x'; MAX_DERIVATIVE_BYTES];
        assert_eq!(
            review_file_set_v1(&oversized, &scanner()),
            Err(DerivativeError::SizeLimitExceeded)
        );
    }

    #[test]
    fn scanner_configuration_is_bounded_and_private_in_debug_output() {
        assert_eq!(
            SeededForbiddenScannerV1::new(Vec::new()).unwrap_err(),
            DerivativeError::InvalidScannerConfiguration
        );
        assert_eq!(
            ForbiddenCanaryV1::new(Vec::new()).unwrap_err(),
            DerivativeError::InvalidScannerConfiguration
        );
        let canary = ForbiddenCanaryV1::new(PRIVATE_AUTH.as_bytes().to_vec()).unwrap();
        assert_eq!(format!("{canary:?}"), "ForbiddenCanaryV1([private])");
        let too_many = (0..=MAX_CANARIES)
            .map(|index| ForbiddenCanaryV1::new(format!("canary-{index:04}").into_bytes()).unwrap())
            .collect();
        assert_eq!(
            SeededForbiddenScannerV1::new(too_many).unwrap_err(),
            DerivativeError::InvalidScannerConfiguration
        );
    }

    #[test]
    fn scanner_accepts_large_bounded_sources_without_ignoring_possible_leaks() {
        let private = vec![b'x'; 48 * 1024];
        let scanner =
            SeededForbiddenScannerV1::new(vec![ForbiddenCanaryV1::new(private.clone()).unwrap()])
                .unwrap();
        let prepared = prepare_derivative_v1(derivative(false), &scanner).unwrap();
        assert!(prepared.review().forbidden_scan_passed());

        assert!(matches!(
            scanner
                .scan(&[DerivativeFileViewV1 {
                    name: DerivativeFileNameV1::Evidence,
                    contents: &private,
                }])
                .unwrap(),
            ForbiddenScanVerdictV1::Forbidden { .. }
        ));
    }

    #[test]
    fn scanner_rejects_work_above_its_deterministic_search_budget() {
        let scanner = SeededForbiddenScannerV1::new(
            (0..128)
                .map(|index| {
                    ForbiddenCanaryV1::new(format!("bounded-canary-{index:03}").into_bytes())
                        .unwrap()
                })
                .collect(),
        )
        .unwrap();
        let output = vec![b'z'; MAX_DERIVATIVE_BYTES];

        assert_eq!(
            scanner.scan(&[DerivativeFileViewV1 {
                name: DerivativeFileNameV1::Evidence,
                contents: &output,
            }]),
            Err(DerivativeScannerError)
        );
    }

    #[test]
    fn scanner_rejects_json_escaped_private_canaries() {
        let private = "secret-\"quoted\"-line\npath\\value";
        let scanner = SeededForbiddenScannerV1::new(vec![ForbiddenCanaryV1::new(
            private.as_bytes().to_vec(),
        )
        .unwrap()])
        .unwrap();
        let encoded = serde_json::to_vec(&serde_json::json!({ "safe": private })).unwrap();

        assert!(matches!(
            scanner
                .scan(&[DerivativeFileViewV1 {
                    name: DerivativeFileNameV1::Evidence,
                    contents: &encoded,
                }])
                .unwrap(),
            ForbiddenScanVerdictV1::Forbidden { .. }
        ));
    }

    #[test]
    fn scanner_detects_a_private_json_value_in_the_payload_middle() {
        use crate::developer_capture::payload::{encode_fields, PrivateField, PrivatePayloadKind};

        const MIDDLE: &str = "middle-private-canary";
        let body = format!(
            "{{\"first\":\"{}\",\"middle\":\"{MIDDLE}\",\"last\":\"{}\"}}",
            "a".repeat(512),
            "z".repeat(512)
        );
        let payload = encode_fields(
            PrivatePayloadKind::HttpRequest,
            &[PrivateField::bytes(private_field::BODY, body.as_bytes())],
        )
        .unwrap();
        let capture = PrivateCaptureV1::new(
            CaptureRef::from_bytes([0x81; 16]),
            1,
            2,
            CapturePurpose::InteractivePlayback,
            SafeOperationRef::from_bytes([0x81; 4]),
            vec![CaptureRecordV1::new(
                0,
                0,
                CaptureRecordKind::HttpRequest,
                payload,
            )],
            CaptureCompleteness::Complete,
            Vec::new(),
            0,
            SafeTerminalCategory::Success,
        );
        let scanner = SeededForbiddenScannerV1::from_private_capture(&capture).unwrap();

        assert!(matches!(
            scanner
                .scan(&[DerivativeFileViewV1 {
                    name: DerivativeFileNameV1::Evidence,
                    contents: MIDDLE.as_bytes(),
                }])
                .unwrap(),
            ForbiddenScanVerdictV1::Forbidden { .. }
        ));
    }

    #[test]
    fn realistic_large_structured_player_body_prepares_with_complete_leaf_coverage() {
        use crate::developer_capture::payload::{encode_fields, PrivateField, PrivatePayloadKind};
        use std::collections::HashSet;

        const MIDDLE: &str = "large-body-middle-private-canary";
        let mut values = (b'a'..=b'z')
            .map(|value| char::from(value).to_string())
            .chain((0..100).map(|index| format!("{index:02}")))
            .chain((0..100).map(|index| format!("{index:03}")))
            .collect::<Vec<_>>();
        while values.len() < 1_201 {
            let index = values.len();
            values.push(format!(
                "player-value-{index:04}-{}",
                "x".repeat(48 + index % 32)
            ));
        }
        values.insert(values.len() / 2, MIDDLE.to_owned());
        assert!(values.len() > 1_100);
        assert_eq!(
            values.iter().collect::<HashSet<_>>().len(),
            values.len(),
            "the fixture must exercise unique leaves rather than deduplication"
        );
        let body = serde_json::to_vec(&serde_json::json!({ "items": values })).unwrap();
        assert!(
            body.len() > 66_784,
            "the fixture should be at least as large as a typical player response"
        );
        let payload = encode_fields(
            PrivatePayloadKind::HttpResponse,
            &[PrivateField::bytes(private_field::BODY, &body)],
        )
        .unwrap();
        let capture = PrivateCaptureV1::new(
            CaptureRef::from_bytes([0x87; 16]),
            1,
            2,
            CapturePurpose::InteractivePlayback,
            SafeOperationRef::from_bytes([0x87; 4]),
            vec![CaptureRecordV1::new(
                0,
                0,
                CaptureRecordKind::HttpResponse,
                payload,
            )],
            CaptureCompleteness::Complete,
            Vec::new(),
            0,
            SafeTerminalCategory::Success,
        );
        let scanner = SeededForbiddenScannerV1::from_private_capture(&capture).unwrap();

        let started = std::time::Instant::now();
        prepare_derivative_v1(derivative(false), &scanner).unwrap();
        let elapsed = started.elapsed();
        eprintln!(
            "phase7.performance.derivative_build_scan_ms={}",
            elapsed.as_millis()
        );
        assert!(
            elapsed < std::time::Duration::from_secs(2),
            "large derivative construction and scan took {elapsed:?}"
        );
        assert!(matches!(
            scanner
                .scan(&[DerivativeFileViewV1 {
                    name: DerivativeFileNameV1::Evidence,
                    contents: MIDDLE.as_bytes(),
                }])
                .unwrap(),
            ForbiddenScanVerdictV1::Forbidden { .. }
        ));
    }

    #[test]
    fn scanner_keeps_complete_coverage_after_record_sixty_four() {
        let payloads = (0..65)
            .map(|index| format!("record-{index:03}-private").into_bytes())
            .collect();
        let capture = synthetic_capture(0x82, payloads);
        let scanner = SeededForbiddenScannerV1::from_private_capture(&capture).unwrap();

        assert!(matches!(
            scanner
                .scan(&[DerivativeFileViewV1 {
                    name: DerivativeFileNameV1::Evidence,
                    contents: b"record-064-private",
                }])
                .unwrap(),
            ForbiddenScanVerdictV1::Forbidden { .. }
        ));
    }

    #[test]
    fn scanner_visits_json_values_after_the_old_five_hundred_twelve_limit() {
        use crate::developer_capture::payload::{encode_fields, PrivateField, PrivatePayloadKind};

        const LATE: &str = "late-private-canary";
        let repeated = std::iter::repeat_n("\"same\"", 513)
            .collect::<Vec<_>>()
            .join(",");
        let body = format!("[{repeated},\"{LATE}\"]");
        assert!(body.len() <= MAX_CANARY_BYTES);
        let payload = encode_fields(
            PrivatePayloadKind::HttpRequest,
            &[PrivateField::bytes(private_field::BODY, body.as_bytes())],
        )
        .unwrap();
        assert!(payload.len() <= MAX_CANARY_BYTES);
        let capture = PrivateCaptureV1::new(
            CaptureRef::from_bytes([0x83; 16]),
            1,
            2,
            CapturePurpose::InteractivePlayback,
            SafeOperationRef::from_bytes([0x83; 4]),
            vec![CaptureRecordV1::new(
                0,
                0,
                CaptureRecordKind::HttpRequest,
                payload,
            )],
            CaptureCompleteness::Complete,
            Vec::new(),
            0,
            SafeTerminalCategory::Success,
        );
        let scanner = SeededForbiddenScannerV1::from_private_capture(&capture).unwrap();
        assert!(matches!(
            scanner
                .scan(&[DerivativeFileViewV1 {
                    name: DerivativeFileNameV1::Evidence,
                    contents: LATE.as_bytes(),
                }])
                .unwrap(),
            ForbiddenScanVerdictV1::Forbidden { .. }
        ));
    }

    #[test]
    fn scanner_rejects_opaque_oversize_and_generates_large_encodings_on_demand() {
        let long_capture =
            synthetic_capture(0x84, vec![vec![b'x'; MAX_CANARY_BYTES.saturating_add(1)]]);
        assert!(matches!(
            SeededForbiddenScannerV1::from_private_capture(&long_capture),
            Err(DerivativeError::InvalidScannerConfiguration)
        ));

        let expands_past_old_bound = ForbiddenCanaryV1::new(vec![b'"'; 3_000]).unwrap();
        let scanner = SeededForbiddenScannerV1::new(vec![expands_past_old_bound]).unwrap();
        let percent = percent_encode(&vec![b'"'; 3_000]).unwrap();
        assert!(matches!(
            scanner
                .scan(&[DerivativeFileViewV1 {
                    name: DerivativeFileNameV1::Evidence,
                    contents: &percent,
                }])
                .unwrap(),
            ForbiddenScanVerdictV1::Forbidden { .. }
        ));
        assert!(format!("{scanner:?}").contains("on-demand"));
    }

    #[test]
    fn scanner_detects_raw_json_percent_and_base64_representations() {
        let private = "private-\"quoted\" value/+?";
        let scanner = SeededForbiddenScannerV1::new(vec![ForbiddenCanaryV1::new(
            private.as_bytes().to_vec(),
        )
        .unwrap()])
        .unwrap();
        let raw = private.as_bytes().to_vec();
        let json = serde_json::to_vec(private).unwrap();
        let percent = percent_encode(private.as_bytes()).unwrap().to_vec();
        let percent_lower = percent_encode_lower(private.as_bytes()).unwrap().to_vec();
        let standard = STANDARD.encode(private).into_bytes();
        let standard_no_pad = STANDARD_NO_PAD.encode(private).into_bytes();
        let url = URL_SAFE.encode(private).into_bytes();
        let url_no_pad = URL_SAFE_NO_PAD.encode(private).into_bytes();

        for representation in [
            raw,
            json,
            percent,
            percent_lower,
            standard,
            standard_no_pad,
            url,
            url_no_pad,
        ] {
            assert!(matches!(
                scanner
                    .scan(&[DerivativeFileViewV1 {
                        name: DerivativeFileNameV1::Evidence,
                        contents: &representation,
                    }])
                    .unwrap(),
                ForbiddenScanVerdictV1::Forbidden { .. }
            ));
        }
    }

    #[test]
    fn multi_source_scanner_rejects_combined_coverage_overflow() {
        let left = synthetic_capture(
            0x85,
            (0..MAX_CANARIES / 2)
                .map(|index| format!("left-{index:03}-private").into_bytes())
                .collect(),
        );
        let right = synthetic_capture(
            0x86,
            (0..MAX_CANARIES / 2)
                .map(|index| format!("right-{index:03}-private").into_bytes())
                .collect(),
        );

        assert!(matches!(
            SeededForbiddenScannerV1::from_private_captures(&[&left, &right]),
            Err(DerivativeError::InvalidScannerConfiguration)
        ));
    }

    #[test]
    fn copied_review_text_contains_statuses_only() {
        let prepared = prepare_derivative_v1(derivative(false), &scanner()).unwrap();
        let copied = prepared.review().safe_copy_text();

        for private in canary_values() {
            assert!(!copied.contains(private));
        }
        assert!(!copied.contains("manifest.json"));
        assert!(!copied.contains("sha256"));
        assert!(copied.contains("checksum=passed"));
        assert!(copied.contains("forbidden_scan=passed"));
        assert!(copied.contains("support_bundle=separate"));
    }
}
