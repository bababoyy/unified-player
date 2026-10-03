//! Encrypted comparison-child orchestration and its private binary format.

use std::{
    fmt,
    path::Path,
    sync::Arc,
    time::{Instant, SystemTime},
};

use super::{
    analysis::{compare_captures, CaptureComparisonV1, SafeComparisonSummaryV1},
    controller::{CaptureRefSource, RandomCaptureRefSource},
    diff::{
        AuthenticationKind, BitrateBucket, CancellationState, CanonicalValue, ClientVersionPolicy,
        CodecKind, ComparisonKind, ContainerKind, ContentRangeClass, DiffCategory, DiffSeverity,
        FailureCategory, FallbackOutcome, FixedExplanation, PlayabilityClass, PlayerClientKind,
        RedirectClass, RegisteredField, ResponseClassification, TerminalOutcome, TimingBucket,
        TransportSource, MAX_DIFF_FINDINGS,
    },
    model::{
        CaptureCompleteness, CaptureLimits, CapturePurpose, CaptureRecordKind, CaptureRecordV1,
        CaptureRef, EndpointRole, PrivateCaptureV1, ProviderClientKind, SafeCaptureRef,
        SafeCaptureState, SafeOperationRef, SafeTerminalCategory, SensitiveBytes, TransportKind,
    },
    payload::{encode_fields, field, PrivateField, PrivatePayloadKind},
    security::CapturePassphrase,
    store::{CaptureStore, MaintenanceReport, StoreError},
};

const COMPARISON_RESULT_MAGIC: &[u8; 8] = b"SPCMPV1\0";
const COMPARISON_RESULT_SCHEMA_VERSION: u16 = 1;
const MAX_VALUES_PER_SIDE: usize = 16;
const MAX_COMPARISON_RESULT_BYTES: usize = 64 * 1024;
const CHILD_REFERENCE_ATTEMPTS: usize = 8;

pub(crate) trait ComparisonClock: Send + Sync {
    fn unix_ms(&self) -> u64;
}

#[derive(Debug, Default)]
pub(crate) struct SystemComparisonClock;

impl ComparisonClock for SystemComparisonClock {
    fn unix_ms(&self) -> u64 {
        SystemTime::now()
            .duration_since(SystemTime::UNIX_EPOCH)
            .map_or(0, |duration| {
                u64::try_from(duration.as_millis()).unwrap_or(u64::MAX)
            })
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct SafeComparisonArtifact {
    pub(crate) capture_ref: SafeCaptureRef,
    pub(crate) state: SafeCaptureState,
    pub(crate) summary: SafeComparisonSummaryV1,
}

pub(crate) struct ComparisonArtifactService {
    store: CaptureStore,
    refs: Arc<dyn CaptureRefSource>,
    clock: Arc<dyn ComparisonClock>,
}

impl fmt::Debug for ComparisonArtifactService {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ComparisonArtifactService")
            .field("store", &"[private]")
            .finish_non_exhaustive()
    }
}

impl ComparisonArtifactService {
    pub(crate) fn open(
        root: impl AsRef<Path>,
        limits: CaptureLimits,
    ) -> Result<(Self, MaintenanceReport), ComparisonFacadeError> {
        let (store, maintenance) = CaptureStore::open(root, limits)?;
        Ok((
            Self {
                store,
                refs: Arc::new(RandomCaptureRefSource),
                clock: Arc::new(SystemComparisonClock),
            },
            maintenance,
        ))
    }

    pub(crate) fn with_sources(
        store: CaptureStore,
        refs: Arc<dyn CaptureRefSource>,
        clock: Arc<dyn ComparisonClock>,
    ) -> Self {
        Self { store, refs, clock }
    }

    pub(crate) fn compare_and_persist(
        &self,
        kind: ComparisonKind,
        left_ref: SafeCaptureRef,
        right_ref: SafeCaptureRef,
        passphrase: &CapturePassphrase,
        operation_ref: SafeOperationRef,
    ) -> Result<SafeComparisonArtifact, ComparisonFacadeError> {
        if left_ref == right_ref {
            return Err(ComparisonFacadeError::SameReference);
        }
        let left = self.store.read_private_by_safe_ref(left_ref, passphrase)?;
        let right = self.store.read_private_by_safe_ref(right_ref, passphrase)?;
        if left.capture_ref() == right.capture_ref() {
            return Err(ComparisonFacadeError::SameReference);
        }

        let comparison = compare_captures(kind, &left, &right);
        let detailed = DetailedComparisonV1::from_comparison(
            left.capture_ref(),
            right.capture_ref(),
            &comparison,
        )?;
        let encoded = encode_comparison_result(&detailed)?;
        let decoded = decode_comparison_result(encoded.expose())?;
        if decoded != detailed {
            return Err(ComparisonFacadeError::Encoding);
        }

        let child_ref = self.allocate_child_ref(left.capture_ref(), right.capture_ref())?;
        let created_unix_ms = self.clock.unix_ms();
        let completed_unix_ms = self.clock.unix_ms().max(created_unix_ms);
        let capture = comparison_child_capture(
            child_ref,
            operation_ref,
            created_unix_ms,
            completed_unix_ms,
            encoded,
        )?;
        let deadline = Instant::now()
            .checked_add(self.store.finalization_timeout())
            .ok_or(ComparisonFacadeError::Persistence)?;
        let stored = self
            .store
            .write_before(&capture, passphrase, deadline)
            .map_err(|error| match error {
                StoreError::AlreadyExists | StoreError::AmbiguousSafeReference => {
                    ComparisonFacadeError::ChildReferenceCollision
                }
                _ => ComparisonFacadeError::Store(error),
            })?;

        Ok(SafeComparisonArtifact {
            capture_ref: stored.capture_ref,
            state: SafeCaptureState::Ready,
            summary: comparison.safe_summary().clone(),
        })
    }

    fn allocate_child_ref(
        &self,
        left_ref: CaptureRef,
        right_ref: CaptureRef,
    ) -> Result<CaptureRef, ComparisonFacadeError> {
        for _ in 0..CHILD_REFERENCE_ATTEMPTS {
            let candidate = self.refs.next_ref();
            if candidate != left_ref
                && candidate != right_ref
                && !self.store.contains_safe_ref(candidate.safe())?
            {
                return Ok(candidate);
            }
        }
        Err(ComparisonFacadeError::ChildReferenceCollision)
    }
}

#[derive(Debug)]
pub(crate) enum ComparisonFacadeError {
    SameReference,
    ChildReferenceCollision,
    Encoding,
    Persistence,
    Store(StoreError),
}

impl fmt::Display for ComparisonFacadeError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::SameReference => "private comparison requires two distinct artifacts",
            Self::ChildReferenceCollision => {
                "private comparison child reference could not be allocated"
            }
            Self::Encoding => "private comparison result could not be encoded",
            Self::Persistence => "private comparison result could not be persisted",
            Self::Store(_) => "private comparison vault operation failed",
        })
    }
}

impl std::error::Error for ComparisonFacadeError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Store(error) => Some(error),
            _ => None,
        }
    }
}

impl From<StoreError> for ComparisonFacadeError {
    fn from(error: StoreError) -> Self {
        Self::Store(error)
    }
}

impl From<ComparisonCodecError> for ComparisonFacadeError {
    fn from(_: ComparisonCodecError) -> Self {
        Self::Encoding
    }
}

fn comparison_child_capture(
    child_ref: CaptureRef,
    operation_ref: SafeOperationRef,
    created_unix_ms: u64,
    completed_unix_ms: u64,
    result: SensitiveBytes,
) -> Result<PrivateCaptureV1, ComparisonFacadeError> {
    let terminal_payload = encode_fields(
        PrivatePayloadKind::TerminalOutcome,
        &[PrivateField::text(field::OUTCOME, "success")],
    )
    .map_err(|_| ComparisonFacadeError::Encoding)?;
    Ok(PrivateCaptureV1::new(
        child_ref,
        created_unix_ms,
        completed_unix_ms,
        CapturePurpose::Comparison,
        operation_ref,
        vec![
            CaptureRecordV1::with_context(
                0,
                0,
                None,
                EndpointRole::Operation,
                ProviderClientKind::Unknown,
                TransportKind::Unknown,
                0,
                CaptureRecordKind::OperationBoundary,
                result,
            ),
            CaptureRecordV1::with_context(
                1,
                completed_unix_ms.saturating_sub(created_unix_ms),
                None,
                EndpointRole::Operation,
                ProviderClientKind::Unknown,
                TransportKind::Unknown,
                0,
                CaptureRecordKind::TerminalOutcome,
                terminal_payload,
            ),
        ],
        CaptureCompleteness::Complete,
        Vec::new(),
        0,
        SafeTerminalCategory::Success,
    ))
}

#[derive(Clone, PartialEq, Eq)]
struct DetailedFindingV1 {
    field: Option<RegisteredField>,
    unknown_slot: Option<u16>,
    category: DiffCategory,
    severity: DiffSeverity,
    explanation: FixedExplanation,
    left: Vec<CanonicalValue>,
    right: Vec<CanonicalValue>,
}

#[derive(PartialEq, Eq)]
struct DetailedComparisonV1 {
    schema_version: u16,
    kind: ComparisonKind,
    left_ref: CaptureRef,
    right_ref: CaptureRef,
    findings: Vec<DetailedFindingV1>,
    incomplete: bool,
    dropped_findings: u16,
    malformed_records: u16,
}

impl DetailedComparisonV1 {
    fn from_comparison(
        left_ref: CaptureRef,
        right_ref: CaptureRef,
        comparison: &CaptureComparisonV1,
    ) -> Result<Self, ComparisonCodecError> {
        if left_ref == right_ref || left_ref.safe() == right_ref.safe() {
            return Err(ComparisonCodecError::InvalidData);
        }
        let report = comparison.private_report();
        let safe = comparison.safe_summary();
        if report.schema_version() != COMPARISON_RESULT_SCHEMA_VERSION
            || safe.schema_version() != report.schema_version()
            || safe.kind() != report.kind()
            || safe.findings() != report.safe_projection()
            || safe.dropped_findings() != report.dropped_findings()
            || report.findings().len() > MAX_DIFF_FINDINGS
            || (report.incomplete() && !safe.incomplete())
            || (report.dropped_findings() != 0 && !report.incomplete())
        {
            return Err(ComparisonCodecError::InvalidData);
        }
        let findings = report
            .findings()
            .iter()
            .map(|finding| {
                let (category, severity, explanation) = finding
                    .registered_field()
                    .map_or(unknown_metadata(), field_metadata);
                if finding.category() != category
                    || finding.severity() != severity
                    || finding.explanation() != explanation
                {
                    return Err(ComparisonCodecError::InvalidData);
                }
                let (left, right) = finding.private_values();
                let finding = DetailedFindingV1 {
                    field: finding.registered_field(),
                    unknown_slot: finding.unknown_slot(),
                    category,
                    severity,
                    explanation,
                    left: left.to_vec(),
                    right: right.to_vec(),
                };
                validate_finding(&finding)?;
                Ok(finding)
            })
            .collect::<Result<Vec<_>, _>>()?;
        Ok(Self {
            schema_version: COMPARISON_RESULT_SCHEMA_VERSION,
            kind: report.kind(),
            left_ref,
            right_ref,
            findings,
            incomplete: safe.incomplete(),
            dropped_findings: safe.dropped_findings(),
            malformed_records: safe.malformed_records(),
        })
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum ComparisonCodecError {
    InvalidData,
    TooLarge,
    UnsupportedSchema,
}

impl fmt::Display for ComparisonCodecError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::InvalidData => "private comparison result is malformed",
            Self::TooLarge => "private comparison result exceeds its bound",
            Self::UnsupportedSchema => "private comparison result schema is unsupported",
        })
    }
}

impl std::error::Error for ComparisonCodecError {}

pub(super) fn validate_comparison_result_payload(
    payload: &[u8],
) -> Result<(), ComparisonCodecError> {
    decode_comparison_result(payload).map(|_| ())
}

fn encode_comparison_result(
    comparison: &DetailedComparisonV1,
) -> Result<SensitiveBytes, ComparisonCodecError> {
    validate_comparison(comparison)?;
    let mut bytes = Vec::with_capacity(256);
    bytes.extend_from_slice(COMPARISON_RESULT_MAGIC);
    bytes.extend_from_slice(&comparison.schema_version.to_be_bytes());
    bytes.push(comparison_kind_tag(comparison.kind));
    bytes.push(u8::from(comparison.incomplete));
    bytes.extend_from_slice(&comparison.left_ref.bytes());
    bytes.extend_from_slice(&comparison.right_ref.bytes());
    bytes.extend_from_slice(&comparison.dropped_findings.to_be_bytes());
    bytes.extend_from_slice(&comparison.malformed_records.to_be_bytes());
    bytes.extend_from_slice(
        &u16::try_from(comparison.findings.len())
            .map_err(|_| ComparisonCodecError::TooLarge)?
            .to_be_bytes(),
    );
    for finding in &comparison.findings {
        bytes.push(finding.field.map_or(0, registered_field_tag));
        if let Some(slot) = finding.unknown_slot {
            bytes.extend_from_slice(&slot.to_be_bytes());
        }
        bytes.push(u8::try_from(finding.left.len()).map_err(|_| ComparisonCodecError::TooLarge)?);
        bytes.push(u8::try_from(finding.right.len()).map_err(|_| ComparisonCodecError::TooLarge)?);
        for value in finding.left.iter().chain(&finding.right) {
            encode_canonical_value(&mut bytes, value);
        }
        if bytes.len() > MAX_COMPARISON_RESULT_BYTES {
            return Err(ComparisonCodecError::TooLarge);
        }
    }
    Ok(SensitiveBytes::new(bytes))
}

fn decode_comparison_result(payload: &[u8]) -> Result<DetailedComparisonV1, ComparisonCodecError> {
    if payload.len() > MAX_COMPARISON_RESULT_BYTES {
        return Err(ComparisonCodecError::TooLarge);
    }
    let mut reader = ComparisonReader::new(payload);
    if reader.read_exact(COMPARISON_RESULT_MAGIC.len())? != COMPARISON_RESULT_MAGIC {
        return Err(ComparisonCodecError::InvalidData);
    }
    let schema_version = reader.read_u16()?;
    if schema_version != COMPARISON_RESULT_SCHEMA_VERSION {
        return Err(ComparisonCodecError::UnsupportedSchema);
    }
    let kind = comparison_kind_from_tag(reader.read_u8()?)?;
    let incomplete = match reader.read_u8()? {
        0 => false,
        1 => true,
        _ => return Err(ComparisonCodecError::InvalidData),
    };
    let left_ref = CaptureRef::from_bytes(reader.read_array::<16>()?);
    let right_ref = CaptureRef::from_bytes(reader.read_array::<16>()?);
    let dropped_findings = reader.read_u16()?;
    let malformed_records = reader.read_u16()?;
    let finding_count = usize::from(reader.read_u16()?);
    if finding_count > MAX_DIFF_FINDINGS {
        return Err(ComparisonCodecError::TooLarge);
    }
    let mut findings = Vec::with_capacity(finding_count);
    for _ in 0..finding_count {
        let field = registered_field_from_tag(reader.read_u8()?)?;
        let unknown_slot = if field.is_none() {
            Some(reader.read_u16()?)
        } else {
            None
        };
        let left_count = usize::from(reader.read_u8()?);
        let right_count = usize::from(reader.read_u8()?);
        if left_count > MAX_VALUES_PER_SIDE || right_count > MAX_VALUES_PER_SIDE {
            return Err(ComparisonCodecError::TooLarge);
        }
        let mut left = Vec::with_capacity(left_count);
        let mut right = Vec::with_capacity(right_count);
        for _ in 0..left_count {
            left.push(decode_canonical_value(&mut reader)?);
        }
        for _ in 0..right_count {
            right.push(decode_canonical_value(&mut reader)?);
        }
        let (category, severity, explanation) = field.map_or(unknown_metadata(), field_metadata);
        let finding = DetailedFindingV1 {
            field,
            unknown_slot,
            category,
            severity,
            explanation,
            left,
            right,
        };
        validate_finding(&finding)?;
        findings.push(finding);
    }
    if !reader.is_empty() {
        return Err(ComparisonCodecError::InvalidData);
    }
    let comparison = DetailedComparisonV1 {
        schema_version,
        kind,
        left_ref,
        right_ref,
        findings,
        incomplete,
        dropped_findings,
        malformed_records,
    };
    validate_comparison(&comparison)?;
    Ok(comparison)
}

fn validate_comparison(comparison: &DetailedComparisonV1) -> Result<(), ComparisonCodecError> {
    if comparison.schema_version != COMPARISON_RESULT_SCHEMA_VERSION {
        return Err(ComparisonCodecError::UnsupportedSchema);
    }
    if comparison.left_ref == comparison.right_ref
        || comparison.left_ref.safe() == comparison.right_ref.safe()
        || comparison.findings.len() > MAX_DIFF_FINDINGS
        || ((comparison.dropped_findings != 0 || comparison.malformed_records != 0)
            && !comparison.incomplete)
    {
        return Err(ComparisonCodecError::InvalidData);
    }
    for (index, finding) in comparison.findings.iter().enumerate() {
        validate_finding(finding)?;
        if comparison.findings[..index].iter().any(|previous| {
            previous.field == finding.field && previous.unknown_slot == finding.unknown_slot
        }) {
            return Err(ComparisonCodecError::InvalidData);
        }
    }
    Ok(())
}

fn validate_finding(finding: &DetailedFindingV1) -> Result<(), ComparisonCodecError> {
    if finding.left.len() > MAX_VALUES_PER_SIDE
        || finding.right.len() > MAX_VALUES_PER_SIDE
        || finding.left == finding.right
        || !strictly_increasing(&finding.left)
        || !strictly_increasing(&finding.right)
    {
        return Err(ComparisonCodecError::InvalidData);
    }
    let (category, severity, explanation) =
        finding.field.map_or(unknown_metadata(), field_metadata);
    if finding.category != category
        || finding.severity != severity
        || finding.explanation != explanation
    {
        return Err(ComparisonCodecError::InvalidData);
    }
    match finding.field {
        Some(field) => {
            if finding.unknown_slot.is_some()
                || finding
                    .left
                    .iter()
                    .chain(&finding.right)
                    .any(|value| !value_matches_field(field, value))
            {
                return Err(ComparisonCodecError::InvalidData);
            }
        }
        None => {
            if finding.unknown_slot.is_none()
                || finding.left.len() > 1
                || finding.right.len() > 1
                || finding
                    .left
                    .iter()
                    .chain(&finding.right)
                    .any(|value| !matches!(value, CanonicalValue::PrivateOpaqueCount(_)))
            {
                return Err(ComparisonCodecError::InvalidData);
            }
        }
    }
    Ok(())
}

fn strictly_increasing(values: &[CanonicalValue]) -> bool {
    values.windows(2).all(|pair| pair[0] < pair[1])
}

fn value_matches_field(field: RegisteredField, value: &CanonicalValue) -> bool {
    match field {
        RegisteredField::PlayerHttpStatus | RegisteredField::MediaHttpStatus => {
            matches!(value, CanonicalValue::HttpStatus(_))
        }
        RegisteredField::PlayerRedirectClass => matches!(value, CanonicalValue::Redirect(_)),
        RegisteredField::PlayerClientKind => matches!(value, CanonicalValue::Client(_)),
        RegisteredField::ClientVersionPolicy => {
            matches!(value, CanonicalValue::VersionPolicy(_))
        }
        RegisteredField::AuthenticationKind => {
            matches!(value, CanonicalValue::Authentication(_))
        }
        RegisteredField::ProofTokenPresent
        | RegisteredField::StreamingDataPresent
        | RegisteredField::BrowserFallbackEligible => {
            matches!(value, CanonicalValue::Boolean(_))
        }
        RegisteredField::PlayabilityStatus => matches!(value, CanonicalValue::Playability(_)),
        RegisteredField::SafeFailureCategory => matches!(value, CanonicalValue::Failure(_)),
        RegisteredField::ReturnedFormatCount
        | RegisteredField::SupportedFormatCount
        | RegisteredField::DirectFormatCount
        | RegisteredField::CipherFormatCount
        | RegisteredField::RetryCount => matches!(value, CanonicalValue::Count(_)),
        RegisteredField::FormatIdentifier => {
            matches!(value, CanonicalValue::PrivateFormat(_))
        }
        RegisteredField::Container => matches!(value, CanonicalValue::Container(_)),
        RegisteredField::Codec => matches!(value, CanonicalValue::Codec(_)),
        RegisteredField::BitrateBucket => matches!(value, CanonicalValue::Bitrate(_)),
        RegisteredField::SelectedFormat => {
            matches!(value, CanonicalValue::SelectedPrivateFormat(_))
        }
        RegisteredField::BrowserFallbackOutcome => {
            matches!(value, CanonicalValue::Fallback(_))
        }
        RegisteredField::NativeResponseClass | RegisteredField::BrowserResponseClass => {
            matches!(value, CanonicalValue::Response(_))
        }
        RegisteredField::ContentRangeClass => {
            matches!(value, CanonicalValue::ContentRange(_))
        }
        RegisteredField::TransportSource => matches!(value, CanonicalValue::Transport(_)),
        RegisteredField::ParserTiming
        | RegisteredField::SelectorTiming
        | RegisteredField::TransportTiming
        | RegisteredField::DecoderTiming
        | RegisteredField::TerminalTiming => matches!(value, CanonicalValue::Timing(_)),
        RegisteredField::CancellationState => {
            matches!(value, CanonicalValue::Cancellation(_))
        }
        RegisteredField::TerminalOutcome => matches!(value, CanonicalValue::Terminal(_)),
    }
}

const fn unknown_metadata() -> (DiffCategory, DiffSeverity, FixedExplanation) {
    (
        DiffCategory::PrivateUnknown,
        DiffSeverity::Informational,
        FixedExplanation::UnknownPrivateFactChanged,
    )
}

const fn field_metadata(field: RegisteredField) -> (DiffCategory, DiffSeverity, FixedExplanation) {
    match field {
        RegisteredField::PlayerHttpStatus => (
            DiffCategory::ProviderResponse,
            DiffSeverity::Critical,
            FixedExplanation::PlayerHttpStatusChanged,
        ),
        RegisteredField::PlayerRedirectClass => (
            DiffCategory::ProviderResponse,
            DiffSeverity::Low,
            FixedExplanation::RedirectBehaviorChanged,
        ),
        RegisteredField::PlayerClientKind => (
            DiffCategory::Authentication,
            DiffSeverity::Medium,
            FixedExplanation::PlayerClientChanged,
        ),
        RegisteredField::ClientVersionPolicy => (
            DiffCategory::Authentication,
            DiffSeverity::Low,
            FixedExplanation::ClientVersionPolicyChanged,
        ),
        RegisteredField::AuthenticationKind => (
            DiffCategory::Authentication,
            DiffSeverity::High,
            FixedExplanation::AuthenticationChanged,
        ),
        RegisteredField::ProofTokenPresent => (
            DiffCategory::Authentication,
            DiffSeverity::High,
            FixedExplanation::ProofTokenPresenceChanged,
        ),
        RegisteredField::PlayabilityStatus => (
            DiffCategory::ProviderResponse,
            DiffSeverity::Critical,
            FixedExplanation::PlayabilityChanged,
        ),
        RegisteredField::SafeFailureCategory => (
            DiffCategory::ProviderResponse,
            DiffSeverity::Critical,
            FixedExplanation::FailureCategoryChanged,
        ),
        RegisteredField::StreamingDataPresent => (
            DiffCategory::ProviderResponse,
            DiffSeverity::Critical,
            FixedExplanation::StreamingDataPresenceChanged,
        ),
        RegisteredField::ReturnedFormatCount => (
            DiffCategory::FormatSelection,
            DiffSeverity::Medium,
            FixedExplanation::ReturnedFormatCountChanged,
        ),
        RegisteredField::SupportedFormatCount => (
            DiffCategory::FormatSelection,
            DiffSeverity::High,
            FixedExplanation::SupportedFormatCountChanged,
        ),
        RegisteredField::DirectFormatCount => (
            DiffCategory::FormatSelection,
            DiffSeverity::High,
            FixedExplanation::DirectFormatCountChanged,
        ),
        RegisteredField::CipherFormatCount => (
            DiffCategory::FormatSelection,
            DiffSeverity::High,
            FixedExplanation::CipherFormatCountChanged,
        ),
        RegisteredField::FormatIdentifier => (
            DiffCategory::FormatSelection,
            DiffSeverity::Medium,
            FixedExplanation::FormatInventoryChanged,
        ),
        RegisteredField::Container => (
            DiffCategory::FormatSelection,
            DiffSeverity::Medium,
            FixedExplanation::ContainerChanged,
        ),
        RegisteredField::Codec => (
            DiffCategory::FormatSelection,
            DiffSeverity::Medium,
            FixedExplanation::CodecChanged,
        ),
        RegisteredField::BitrateBucket => (
            DiffCategory::FormatSelection,
            DiffSeverity::Low,
            FixedExplanation::BitrateClassChanged,
        ),
        RegisteredField::SelectedFormat => (
            DiffCategory::FormatSelection,
            DiffSeverity::High,
            FixedExplanation::SelectedFormatChanged,
        ),
        RegisteredField::BrowserFallbackEligible => (
            DiffCategory::Fallback,
            DiffSeverity::Medium,
            FixedExplanation::FallbackEligibilityChanged,
        ),
        RegisteredField::BrowserFallbackOutcome => (
            DiffCategory::Fallback,
            DiffSeverity::High,
            FixedExplanation::FallbackOutcomeChanged,
        ),
        RegisteredField::NativeResponseClass => (
            DiffCategory::ProviderResponse,
            DiffSeverity::High,
            FixedExplanation::NativeResponseChanged,
        ),
        RegisteredField::BrowserResponseClass => (
            DiffCategory::ProviderResponse,
            DiffSeverity::High,
            FixedExplanation::BrowserResponseChanged,
        ),
        RegisteredField::MediaHttpStatus => (
            DiffCategory::MediaTransport,
            DiffSeverity::Critical,
            FixedExplanation::MediaHttpStatusChanged,
        ),
        RegisteredField::ContentRangeClass => (
            DiffCategory::MediaTransport,
            DiffSeverity::High,
            FixedExplanation::ContentRangeChanged,
        ),
        RegisteredField::TransportSource => (
            DiffCategory::MediaTransport,
            DiffSeverity::Medium,
            FixedExplanation::TransportSourceChanged,
        ),
        RegisteredField::ParserTiming => (
            DiffCategory::Performance,
            DiffSeverity::Low,
            FixedExplanation::ParserTimingChanged,
        ),
        RegisteredField::SelectorTiming => (
            DiffCategory::Performance,
            DiffSeverity::Low,
            FixedExplanation::SelectorTimingChanged,
        ),
        RegisteredField::TransportTiming => (
            DiffCategory::Performance,
            DiffSeverity::Medium,
            FixedExplanation::TransportTimingChanged,
        ),
        RegisteredField::DecoderTiming => (
            DiffCategory::Performance,
            DiffSeverity::Medium,
            FixedExplanation::DecoderTimingChanged,
        ),
        RegisteredField::TerminalTiming => (
            DiffCategory::Performance,
            DiffSeverity::Low,
            FixedExplanation::TerminalTimingChanged,
        ),
        RegisteredField::CancellationState => (
            DiffCategory::Lifecycle,
            DiffSeverity::High,
            FixedExplanation::CancellationChanged,
        ),
        RegisteredField::RetryCount => (
            DiffCategory::Lifecycle,
            DiffSeverity::Medium,
            FixedExplanation::RetryCountChanged,
        ),
        RegisteredField::TerminalOutcome => (
            DiffCategory::Lifecycle,
            DiffSeverity::High,
            FixedExplanation::TerminalOutcomeChanged,
        ),
    }
}

const fn comparison_kind_tag(kind: ComparisonKind) -> u8 {
    match kind {
        ComparisonKind::WorkingVsFailing => 1,
        ComparisonKind::OriginalVsReplay => 2,
    }
}

fn comparison_kind_from_tag(tag: u8) -> Result<ComparisonKind, ComparisonCodecError> {
    match tag {
        1 => Ok(ComparisonKind::WorkingVsFailing),
        2 => Ok(ComparisonKind::OriginalVsReplay),
        _ => Err(ComparisonCodecError::InvalidData),
    }
}

const fn registered_field_tag(field: RegisteredField) -> u8 {
    match field {
        RegisteredField::PlayerHttpStatus => 1,
        RegisteredField::PlayerRedirectClass => 2,
        RegisteredField::PlayerClientKind => 3,
        RegisteredField::ClientVersionPolicy => 4,
        RegisteredField::AuthenticationKind => 5,
        RegisteredField::ProofTokenPresent => 6,
        RegisteredField::PlayabilityStatus => 7,
        RegisteredField::SafeFailureCategory => 8,
        RegisteredField::StreamingDataPresent => 9,
        RegisteredField::ReturnedFormatCount => 10,
        RegisteredField::SupportedFormatCount => 11,
        RegisteredField::DirectFormatCount => 12,
        RegisteredField::CipherFormatCount => 13,
        RegisteredField::FormatIdentifier => 14,
        RegisteredField::Container => 15,
        RegisteredField::Codec => 16,
        RegisteredField::BitrateBucket => 17,
        RegisteredField::SelectedFormat => 18,
        RegisteredField::BrowserFallbackEligible => 19,
        RegisteredField::BrowserFallbackOutcome => 20,
        RegisteredField::NativeResponseClass => 21,
        RegisteredField::BrowserResponseClass => 22,
        RegisteredField::MediaHttpStatus => 23,
        RegisteredField::ContentRangeClass => 24,
        RegisteredField::TransportSource => 25,
        RegisteredField::ParserTiming => 26,
        RegisteredField::SelectorTiming => 27,
        RegisteredField::TransportTiming => 28,
        RegisteredField::DecoderTiming => 29,
        RegisteredField::TerminalTiming => 30,
        RegisteredField::CancellationState => 31,
        RegisteredField::RetryCount => 32,
        RegisteredField::TerminalOutcome => 33,
    }
}

fn registered_field_from_tag(tag: u8) -> Result<Option<RegisteredField>, ComparisonCodecError> {
    let field = match tag {
        0 => return Ok(None),
        1 => RegisteredField::PlayerHttpStatus,
        2 => RegisteredField::PlayerRedirectClass,
        3 => RegisteredField::PlayerClientKind,
        4 => RegisteredField::ClientVersionPolicy,
        5 => RegisteredField::AuthenticationKind,
        6 => RegisteredField::ProofTokenPresent,
        7 => RegisteredField::PlayabilityStatus,
        8 => RegisteredField::SafeFailureCategory,
        9 => RegisteredField::StreamingDataPresent,
        10 => RegisteredField::ReturnedFormatCount,
        11 => RegisteredField::SupportedFormatCount,
        12 => RegisteredField::DirectFormatCount,
        13 => RegisteredField::CipherFormatCount,
        14 => RegisteredField::FormatIdentifier,
        15 => RegisteredField::Container,
        16 => RegisteredField::Codec,
        17 => RegisteredField::BitrateBucket,
        18 => RegisteredField::SelectedFormat,
        19 => RegisteredField::BrowserFallbackEligible,
        20 => RegisteredField::BrowserFallbackOutcome,
        21 => RegisteredField::NativeResponseClass,
        22 => RegisteredField::BrowserResponseClass,
        23 => RegisteredField::MediaHttpStatus,
        24 => RegisteredField::ContentRangeClass,
        25 => RegisteredField::TransportSource,
        26 => RegisteredField::ParserTiming,
        27 => RegisteredField::SelectorTiming,
        28 => RegisteredField::TransportTiming,
        29 => RegisteredField::DecoderTiming,
        30 => RegisteredField::TerminalTiming,
        31 => RegisteredField::CancellationState,
        32 => RegisteredField::RetryCount,
        33 => RegisteredField::TerminalOutcome,
        _ => return Err(ComparisonCodecError::InvalidData),
    };
    Ok(Some(field))
}

macro_rules! tagged_enum {
    ($to:ident, $from:ident, $ty:ty, { $($variant:path => $tag:literal),+ $(,)? }) => {
        const fn $to(value: $ty) -> u8 {
            match value {
                $($variant => $tag),+
            }
        }

        fn $from(tag: u8) -> Result<$ty, ComparisonCodecError> {
            match tag {
                $($tag => Ok($variant)),+,
                _ => Err(ComparisonCodecError::InvalidData),
            }
        }
    };
}

tagged_enum!(redirect_tag, redirect_from_tag, RedirectClass, {
    RedirectClass::None => 1,
    RedirectClass::Redirected => 2,
    RedirectClass::SameOrigin => 3,
    RedirectClass::CrossOrigin => 4,
    RedirectClass::Rejected => 5,
});
tagged_enum!(client_tag, client_from_tag, PlayerClientKind, {
    PlayerClientKind::Web => 1,
    PlayerClientKind::WebRemix => 2,
    PlayerClientKind::Android => 3,
    PlayerClientKind::Ios => 4,
    PlayerClientKind::TvHtml5 => 5,
    PlayerClientKind::Unknown => 6,
});
tagged_enum!(version_policy_tag, version_policy_from_tag, ClientVersionPolicy, {
    ClientVersionPolicy::Static => 1,
    ClientVersionPolicy::Cached => 2,
    ClientVersionPolicy::Discovered => 3,
    ClientVersionPolicy::Fallback => 4,
    ClientVersionPolicy::Unknown => 5,
});
tagged_enum!(authentication_tag, authentication_from_tag, AuthenticationKind, {
    AuthenticationKind::None => 1,
    AuthenticationKind::Browser => 2,
    AuthenticationKind::OAuthBearer => 3,
    AuthenticationKind::Unavailable => 4,
});
tagged_enum!(playability_tag, playability_from_tag, PlayabilityClass, {
    PlayabilityClass::Playable => 1,
    PlayabilityClass::LoginRequired => 2,
    PlayabilityClass::AgeConsentOrRegion => 3,
    PlayabilityClass::Unavailable => 4,
    PlayabilityClass::Other => 5,
});
tagged_enum!(failure_tag, failure_from_tag, FailureCategory, {
    FailureCategory::None => 1,
    FailureCategory::Authentication => 2,
    FailureCategory::ConsentAgeOrRegion => 3,
    FailureCategory::ProviderUnavailable => 4,
    FailureCategory::ProofToken => 5,
    FailureCategory::Decipher => 6,
    FailureCategory::RateLimited => 7,
    FailureCategory::Network => 8,
    FailureCategory::Contract => 9,
    FailureCategory::UnsupportedFormat => 10,
    FailureCategory::MediaForbidden => 11,
    FailureCategory::MediaRangeContract => 12,
    FailureCategory::Decode => 13,
    FailureCategory::Cancelled => 14,
    FailureCategory::Unknown => 15,
});
tagged_enum!(container_tag, container_from_tag, ContainerKind, {
    ContainerKind::Mp4 => 1,
    ContainerKind::WebM => 2,
    ContainerKind::Other => 3,
});
tagged_enum!(codec_tag, codec_from_tag, CodecKind, {
    CodecKind::Aac => 1,
    CodecKind::Opus => 2,
    CodecKind::Vorbis => 3,
    CodecKind::Other => 4,
});
tagged_enum!(bitrate_tag, bitrate_from_tag, BitrateBucket, {
    BitrateBucket::Low => 1,
    BitrateBucket::Medium => 2,
    BitrateBucket::High => 3,
    BitrateBucket::Unknown => 4,
});
tagged_enum!(fallback_tag, fallback_from_tag, FallbackOutcome, {
    FallbackOutcome::NotEligible => 1,
    FallbackOutcome::Eligible => 2,
    FallbackOutcome::Attempted => 3,
    FallbackOutcome::Succeeded => 4,
    FallbackOutcome::Failed => 5,
});
tagged_enum!(response_tag, response_from_tag, ResponseClassification, {
    ResponseClassification::Playable => 1,
    ResponseClassification::Refused => 2,
    ResponseClassification::Malformed => 3,
    ResponseClassification::TransportFailure => 4,
    ResponseClassification::Cancelled => 5,
    ResponseClassification::Unknown => 6,
});
tagged_enum!(content_range_tag, content_range_from_tag, ContentRangeClass, {
    ContentRangeClass::Valid => 1,
    ContentRangeClass::Missing => 2,
    ContentRangeClass::WrongStart => 3,
    ContentRangeClass::Invalid => 4,
    ContentRangeClass::NotApplicable => 5,
});
tagged_enum!(transport_tag, transport_from_tag, TransportSource, {
    TransportSource::NativeHttp => 1,
    TransportSource::Browser => 2,
    TransportSource::BrowserCache => 3,
    TransportSource::ServiceWorker => 4,
    TransportSource::OfflineReplay => 5,
    TransportSource::FreshReplay => 6,
    TransportSource::Unknown => 7,
});
tagged_enum!(timing_tag, timing_from_tag, TimingBucket, {
    TimingBucket::Immediate => 1,
    TimingBucket::Fast => 2,
    TimingBucket::Moderate => 3,
    TimingBucket::Slow => 4,
    TimingBucket::VerySlow => 5,
    TimingBucket::TimedOut => 6,
    TimingBucket::Unavailable => 7,
});
tagged_enum!(cancellation_tag, cancellation_from_tag, CancellationState, {
    CancellationState::NotCancelled => 1,
    CancellationState::Cancelled => 2,
    CancellationState::Superseded => 3,
    CancellationState::Shutdown => 4,
});
tagged_enum!(terminal_tag, terminal_from_tag, TerminalOutcome, {
    TerminalOutcome::Success => 1,
    TerminalOutcome::Failed => 2,
    TerminalOutcome::Cancelled => 3,
    TerminalOutcome::Superseded => 4,
    TerminalOutcome::TimedOut => 5,
    TerminalOutcome::Panicked => 6,
});

fn encode_canonical_value(bytes: &mut Vec<u8>, value: &CanonicalValue) {
    match value {
        CanonicalValue::Boolean(value) => {
            bytes.extend_from_slice(&[1, u8::from(*value)]);
        }
        CanonicalValue::HttpStatus(value) => {
            bytes.push(2);
            bytes.extend_from_slice(&value.to_be_bytes());
        }
        CanonicalValue::Redirect(value) => bytes.extend_from_slice(&[3, redirect_tag(*value)]),
        CanonicalValue::Client(value) => bytes.extend_from_slice(&[4, client_tag(*value)]),
        CanonicalValue::VersionPolicy(value) => {
            bytes.extend_from_slice(&[5, version_policy_tag(*value)]);
        }
        CanonicalValue::Authentication(value) => {
            bytes.extend_from_slice(&[6, authentication_tag(*value)]);
        }
        CanonicalValue::Playability(value) => {
            bytes.extend_from_slice(&[7, playability_tag(*value)]);
        }
        CanonicalValue::Failure(value) => bytes.extend_from_slice(&[8, failure_tag(*value)]),
        CanonicalValue::Count(value) => {
            bytes.push(9);
            bytes.extend_from_slice(&value.to_be_bytes());
        }
        CanonicalValue::PrivateFormat(value) => {
            bytes.push(10);
            bytes.extend_from_slice(&value.to_be_bytes());
        }
        CanonicalValue::SelectedPrivateFormat(value) => {
            bytes.push(11);
            match value {
                Some(value) => {
                    bytes.push(1);
                    bytes.extend_from_slice(&value.to_be_bytes());
                }
                None => bytes.push(0),
            }
        }
        CanonicalValue::PrivateOpaqueCount(value) => {
            bytes.push(12);
            bytes.extend_from_slice(&value.to_be_bytes());
        }
        CanonicalValue::Container(value) => bytes.extend_from_slice(&[13, container_tag(*value)]),
        CanonicalValue::Codec(value) => bytes.extend_from_slice(&[14, codec_tag(*value)]),
        CanonicalValue::Bitrate(value) => bytes.extend_from_slice(&[15, bitrate_tag(*value)]),
        CanonicalValue::Fallback(value) => bytes.extend_from_slice(&[16, fallback_tag(*value)]),
        CanonicalValue::Response(value) => bytes.extend_from_slice(&[17, response_tag(*value)]),
        CanonicalValue::ContentRange(value) => {
            bytes.extend_from_slice(&[18, content_range_tag(*value)]);
        }
        CanonicalValue::Transport(value) => bytes.extend_from_slice(&[19, transport_tag(*value)]),
        CanonicalValue::Timing(value) => bytes.extend_from_slice(&[20, timing_tag(*value)]),
        CanonicalValue::Cancellation(value) => {
            bytes.extend_from_slice(&[21, cancellation_tag(*value)]);
        }
        CanonicalValue::Terminal(value) => bytes.extend_from_slice(&[22, terminal_tag(*value)]),
    }
}

fn decode_canonical_value(
    reader: &mut ComparisonReader<'_>,
) -> Result<CanonicalValue, ComparisonCodecError> {
    match reader.read_u8()? {
        1 => match reader.read_u8()? {
            0 => Ok(CanonicalValue::Boolean(false)),
            1 => Ok(CanonicalValue::Boolean(true)),
            _ => Err(ComparisonCodecError::InvalidData),
        },
        2 => Ok(CanonicalValue::HttpStatus(reader.read_u16()?)),
        3 => Ok(CanonicalValue::Redirect(redirect_from_tag(
            reader.read_u8()?,
        )?)),
        4 => Ok(CanonicalValue::Client(client_from_tag(reader.read_u8()?)?)),
        5 => Ok(CanonicalValue::VersionPolicy(version_policy_from_tag(
            reader.read_u8()?,
        )?)),
        6 => Ok(CanonicalValue::Authentication(authentication_from_tag(
            reader.read_u8()?,
        )?)),
        7 => Ok(CanonicalValue::Playability(playability_from_tag(
            reader.read_u8()?,
        )?)),
        8 => Ok(CanonicalValue::Failure(failure_from_tag(
            reader.read_u8()?,
        )?)),
        9 => Ok(CanonicalValue::Count(reader.read_u16()?)),
        10 => Ok(CanonicalValue::PrivateFormat(reader.read_u64()?)),
        11 => match reader.read_u8()? {
            0 => Ok(CanonicalValue::SelectedPrivateFormat(None)),
            1 => Ok(CanonicalValue::SelectedPrivateFormat(Some(
                reader.read_u64()?,
            ))),
            _ => Err(ComparisonCodecError::InvalidData),
        },
        12 => Ok(CanonicalValue::PrivateOpaqueCount(reader.read_u16()?)),
        13 => Ok(CanonicalValue::Container(container_from_tag(
            reader.read_u8()?,
        )?)),
        14 => Ok(CanonicalValue::Codec(codec_from_tag(reader.read_u8()?)?)),
        15 => Ok(CanonicalValue::Bitrate(bitrate_from_tag(
            reader.read_u8()?,
        )?)),
        16 => Ok(CanonicalValue::Fallback(fallback_from_tag(
            reader.read_u8()?,
        )?)),
        17 => Ok(CanonicalValue::Response(response_from_tag(
            reader.read_u8()?,
        )?)),
        18 => Ok(CanonicalValue::ContentRange(content_range_from_tag(
            reader.read_u8()?,
        )?)),
        19 => Ok(CanonicalValue::Transport(transport_from_tag(
            reader.read_u8()?,
        )?)),
        20 => Ok(CanonicalValue::Timing(timing_from_tag(reader.read_u8()?)?)),
        21 => Ok(CanonicalValue::Cancellation(cancellation_from_tag(
            reader.read_u8()?,
        )?)),
        22 => Ok(CanonicalValue::Terminal(terminal_from_tag(
            reader.read_u8()?,
        )?)),
        _ => Err(ComparisonCodecError::InvalidData),
    }
}

struct ComparisonReader<'a> {
    bytes: &'a [u8],
    position: usize,
}

impl<'a> ComparisonReader<'a> {
    const fn new(bytes: &'a [u8]) -> Self {
        Self { bytes, position: 0 }
    }

    fn read_exact(&mut self, length: usize) -> Result<&'a [u8], ComparisonCodecError> {
        let end = self
            .position
            .checked_add(length)
            .ok_or(ComparisonCodecError::TooLarge)?;
        let value = self
            .bytes
            .get(self.position..end)
            .ok_or(ComparisonCodecError::InvalidData)?;
        self.position = end;
        Ok(value)
    }

    fn read_array<const N: usize>(&mut self) -> Result<[u8; N], ComparisonCodecError> {
        self.read_exact(N)?
            .try_into()
            .map_err(|_| ComparisonCodecError::InvalidData)
    }

    fn read_u8(&mut self) -> Result<u8, ComparisonCodecError> {
        Ok(self.read_exact(1)?[0])
    }

    fn read_u16(&mut self) -> Result<u16, ComparisonCodecError> {
        Ok(u16::from_be_bytes(self.read_array()?))
    }

    fn read_u64(&mut self) -> Result<u64, ComparisonCodecError> {
        Ok(u64::from_be_bytes(self.read_array()?))
    }

    const fn is_empty(&self) -> bool {
        self.position == self.bytes.len()
    }
}

#[cfg(test)]
mod tests {
    use std::{
        io::Write as _,
        sync::{
            atomic::{AtomicU64, Ordering},
            Mutex,
        },
        time::{Duration, SystemTime},
    };

    use static_assertions::assert_not_impl_any;

    use super::*;
    use crate::developer_capture::{
        controller::CaptureController, security::secure_new_file, store::StoreClock,
        writer::VaultFormatError,
    };

    assert_not_impl_any!(DetailedComparisonV1: std::fmt::Debug, std::fmt::Display, serde::Serialize);
    assert_not_impl_any!(DetailedFindingV1: std::fmt::Debug, std::fmt::Display, serde::Serialize);

    struct SequenceRefs(Mutex<Vec<CaptureRef>>);

    impl CaptureRefSource for SequenceRefs {
        fn next_ref(&self) -> CaptureRef {
            self.0.lock().unwrap().remove(0)
        }
    }

    struct IncrementingClock(AtomicU64);

    impl ComparisonClock for IncrementingClock {
        fn unix_ms(&self) -> u64 {
            self.0.fetch_add(1, Ordering::SeqCst)
        }
    }

    struct FixedStoreClock;

    impl StoreClock for FixedStoreClock {
        fn wall_now(&self) -> SystemTime {
            SystemTime::UNIX_EPOCH + Duration::from_secs(1)
        }
    }

    fn open_test_store(root: &Path) -> CaptureStore {
        CaptureStore::with_clock(root, CaptureLimits::default(), Arc::new(FixedStoreClock))
            .unwrap()
            .0
    }

    fn passphrase() -> CapturePassphrase {
        CapturePassphrase::new("comparison persistence test passphrase".to_owned()).unwrap()
    }

    fn terminal_payload(category: SafeTerminalCategory) -> SensitiveBytes {
        let name = match category {
            SafeTerminalCategory::Success => "success",
            SafeTerminalCategory::Failed => "failed",
            SafeTerminalCategory::Cancelled => "cancelled",
            SafeTerminalCategory::Superseded => "superseded",
            SafeTerminalCategory::TimedOut => "timed_out",
            SafeTerminalCategory::Panicked => "panicked",
        };
        encode_fields(
            PrivatePayloadKind::TerminalOutcome,
            &[PrivateField::text(field::OUTCOME, name)],
        )
        .unwrap()
    }

    fn source_capture(capture_ref: CaptureRef, terminal: SafeTerminalCategory) -> PrivateCaptureV1 {
        PrivateCaptureV1::new(
            capture_ref,
            10,
            20,
            CapturePurpose::InteractivePlayback,
            SafeOperationRef::from_bytes([8; 4]),
            vec![
                CaptureRecordV1::with_context(
                    0,
                    1,
                    None,
                    EndpointRole::PlayerApi,
                    ProviderClientKind::Web,
                    TransportKind::NativeHttp,
                    0,
                    CaptureRecordKind::HttpRequest,
                    SensitiveBytes::new(b"private-provider-request".to_vec()),
                ),
                CaptureRecordV1::with_context(
                    1,
                    2,
                    None,
                    EndpointRole::Operation,
                    ProviderClientKind::Unknown,
                    TransportKind::Unknown,
                    0,
                    CaptureRecordKind::TerminalOutcome,
                    terminal_payload(terminal),
                ),
            ],
            CaptureCompleteness::Complete,
            Vec::new(),
            0,
            terminal,
        )
    }

    fn replay_capture(capture_ref: CaptureRef) -> PrivateCaptureV1 {
        PrivateCaptureV1::new(
            capture_ref,
            10,
            20,
            CapturePurpose::Replay,
            SafeOperationRef::from_bytes([9; 4]),
            vec![
                CaptureRecordV1::with_context(
                    0,
                    1,
                    None,
                    EndpointRole::Operation,
                    ProviderClientKind::Unknown,
                    TransportKind::OfflineReplay,
                    0,
                    CaptureRecordKind::OperationBoundary,
                    SensitiveBytes::new(b"bounded-replay-result-fixture".to_vec()),
                ),
                CaptureRecordV1::with_context(
                    1,
                    2,
                    None,
                    EndpointRole::Operation,
                    ProviderClientKind::Unknown,
                    TransportKind::OfflineReplay,
                    0,
                    CaptureRecordKind::TerminalOutcome,
                    terminal_payload(SafeTerminalCategory::Success),
                ),
            ],
            CaptureCompleteness::Complete,
            Vec::new(),
            0,
            SafeTerminalCategory::Success,
        )
    }

    fn seeded_store(
        root: &Path,
        left_ref: CaptureRef,
        right_ref: CaptureRef,
    ) -> (CaptureStore, PrivateCaptureV1, PrivateCaptureV1) {
        let passphrase = passphrase();
        let store = open_test_store(root);
        let left = source_capture(left_ref, SafeTerminalCategory::Success);
        let right = source_capture(right_ref, SafeTerminalCategory::Failed);
        store.write(&left, &passphrase).unwrap();
        store.write(&right, &passphrase).unwrap();
        (store, left, right)
    }

    fn service_with_child(store: CaptureStore, child_ref: CaptureRef) -> ComparisonArtifactService {
        ComparisonArtifactService::with_sources(
            store,
            Arc::new(SequenceRefs(Mutex::new(vec![child_ref]))),
            Arc::new(IncrementingClock(AtomicU64::new(100))),
        )
    }

    #[test]
    fn encrypted_comparison_round_trip_preserves_safe_summary_and_one_terminal() {
        let directory = tempfile::tempdir().unwrap();
        let left_ref = CaptureRef::from_bytes([1; 16]);
        let right_ref = CaptureRef::from_bytes([2; 16]);
        let child_ref = CaptureRef::from_bytes([3; 16]);
        let (store, left, right) = seeded_store(directory.path(), left_ref, right_ref);
        let expected = compare_captures(ComparisonKind::WorkingVsFailing, &left, &right)
            .safe_summary()
            .clone();
        let service = service_with_child(store, child_ref);

        let safe = service
            .compare_and_persist(
                ComparisonKind::WorkingVsFailing,
                left_ref.safe(),
                right_ref.safe(),
                &passphrase(),
                SafeOperationRef::from_bytes([4; 4]),
            )
            .unwrap();
        assert_eq!(safe.capture_ref, child_ref.safe());
        assert_eq!(safe.state, SafeCaptureState::Ready);
        assert_eq!(safe.summary, expected);

        let reopened = open_test_store(directory.path());
        let child = reopened
            .read_private_by_safe_ref(child_ref.safe(), &passphrase())
            .unwrap();
        assert_eq!(child.purpose(), CapturePurpose::Comparison);
        assert_eq!(child.records().len(), 2);
        assert_eq!(
            child
                .records()
                .iter()
                .filter(|record| record.kind() == CaptureRecordKind::TerminalOutcome)
                .count(),
            1
        );
        let detailed = decode_comparison_result(child.records()[0].payload().expose()).unwrap();
        assert_eq!(detailed.kind, ComparisonKind::WorkingVsFailing);
        assert!(detailed.left_ref == left_ref);
        assert!(detailed.right_ref == right_ref);

        for entry in std::fs::read_dir(directory.path()).unwrap() {
            let path = entry.unwrap().path();
            if path.extension().and_then(|value| value.to_str()) == Some("age") {
                let ciphertext = std::fs::read(path).unwrap();
                assert!(!ciphertext
                    .windows(COMPARISON_RESULT_MAGIC.len())
                    .any(|window| window == COMPARISON_RESULT_MAGIC));
            }
        }
    }

    #[test]
    fn original_to_replay_comparison_uses_the_same_encrypted_child_contract() {
        let directory = tempfile::tempdir().unwrap();
        let original_ref = CaptureRef::from_bytes([81; 16]);
        let replay_ref = CaptureRef::from_bytes([82; 16]);
        let child_ref = CaptureRef::from_bytes([83; 16]);
        let original = source_capture(original_ref, SafeTerminalCategory::Success);
        let replay = replay_capture(replay_ref);
        let expected = compare_captures(ComparisonKind::OriginalVsReplay, &original, &replay)
            .safe_summary()
            .clone();
        let store = open_test_store(directory.path());
        store.write(&original, &passphrase()).unwrap();
        store.write(&replay, &passphrase()).unwrap();
        let service = service_with_child(store, child_ref);

        let safe = service
            .compare_and_persist(
                ComparisonKind::OriginalVsReplay,
                original_ref.safe(),
                replay_ref.safe(),
                &passphrase(),
                SafeOperationRef::from_bytes([7; 4]),
            )
            .unwrap();
        assert_eq!(safe.summary, expected);
        let store = open_test_store(directory.path());
        let child = store
            .read_private_by_safe_ref(child_ref.safe(), &passphrase())
            .unwrap();
        let detailed = decode_comparison_result(child.records()[0].payload().expose()).unwrap();
        assert_eq!(detailed.kind, ComparisonKind::OriginalVsReplay);
    }

    #[test]
    fn same_reference_wrong_passphrase_and_ambiguous_reference_fail_closed() {
        let directory = tempfile::tempdir().unwrap();
        let left_ref = CaptureRef::from_bytes([11; 16]);
        let right_ref = CaptureRef::from_bytes([12; 16]);
        let (store, _, _) = seeded_store(directory.path(), left_ref, right_ref);
        let service = service_with_child(store, CaptureRef::from_bytes([13; 16]));
        let operation_ref = SafeOperationRef::from_bytes([1; 4]);
        assert!(matches!(
            service.compare_and_persist(
                ComparisonKind::WorkingVsFailing,
                left_ref.safe(),
                left_ref.safe(),
                &passphrase(),
                operation_ref,
            ),
            Err(ComparisonFacadeError::SameReference)
        ));
        let wrong = CapturePassphrase::new("a different comparison passphrase".to_owned()).unwrap();
        assert!(matches!(
            service.compare_and_persist(
                ComparisonKind::WorkingVsFailing,
                left_ref.safe(),
                right_ref.safe(),
                &wrong,
                operation_ref,
            ),
            Err(ComparisonFacadeError::Store(StoreError::Format(
                VaultFormatError::DecryptionFailed
            )))
        ));

        drop(service);
        let colliding_ref = CaptureRef::from_bytes([
            11, 11, 11, 11, 99, 99, 99, 99, 99, 99, 99, 99, 99, 99, 99, 99,
        ]);
        let source = directory
            .path()
            .join(format!("{}.age", left_ref.file_stem()));
        let duplicate = directory
            .path()
            .join(format!("{}.age", colliding_ref.file_stem()));
        let bytes = std::fs::read(source).unwrap();
        let mut duplicate_file = secure_new_file(&duplicate).unwrap();
        duplicate_file.write_all(&bytes).unwrap();
        duplicate_file.sync_all().unwrap();
        drop(duplicate_file);
        let store = open_test_store(directory.path());
        let service = service_with_child(store, CaptureRef::from_bytes([14; 16]));
        assert!(matches!(
            service.compare_and_persist(
                ComparisonKind::WorkingVsFailing,
                left_ref.safe(),
                right_ref.safe(),
                &passphrase(),
                operation_ref,
            ),
            Err(ComparisonFacadeError::Store(
                StoreError::AmbiguousSafeReference
            ))
        ));
    }

    #[test]
    fn child_reference_collision_is_bounded_and_does_not_claim_an_arm() {
        let directory = tempfile::tempdir().unwrap();
        let left_ref = CaptureRef::from_bytes([21; 16]);
        let right_ref = CaptureRef::from_bytes([22; 16]);
        let (store, _, _) = seeded_store(directory.path(), left_ref, right_ref);
        let colliding =
            CaptureRef::from_bytes([21, 21, 21, 21, 1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12]);
        let service = ComparisonArtifactService::with_sources(
            store,
            Arc::new(SequenceRefs(Mutex::new(vec![
                colliding;
                CHILD_REFERENCE_ATTEMPTS
            ]))),
            Arc::new(IncrementingClock(AtomicU64::new(200))),
        );
        let controller = CaptureController::new(CaptureLimits::default()).unwrap();
        controller.request_arm().unwrap();
        controller.accept_consent().unwrap();
        assert_eq!(controller.snapshot().state, SafeCaptureState::Armed);

        assert!(matches!(
            service.compare_and_persist(
                ComparisonKind::WorkingVsFailing,
                left_ref.safe(),
                right_ref.safe(),
                &passphrase(),
                SafeOperationRef::from_bytes([2; 4]),
            ),
            Err(ComparisonFacadeError::ChildReferenceCollision)
        ));
        assert_eq!(controller.snapshot().state, SafeCaptureState::Armed);
    }

    #[test]
    fn codec_round_trips_every_registered_value_type_and_unknown_findings() {
        let fields = [
            RegisteredField::PlayerHttpStatus,
            RegisteredField::PlayerRedirectClass,
            RegisteredField::PlayerClientKind,
            RegisteredField::ClientVersionPolicy,
            RegisteredField::AuthenticationKind,
            RegisteredField::ProofTokenPresent,
            RegisteredField::PlayabilityStatus,
            RegisteredField::SafeFailureCategory,
            RegisteredField::StreamingDataPresent,
            RegisteredField::ReturnedFormatCount,
            RegisteredField::SupportedFormatCount,
            RegisteredField::DirectFormatCount,
            RegisteredField::CipherFormatCount,
            RegisteredField::FormatIdentifier,
            RegisteredField::Container,
            RegisteredField::Codec,
            RegisteredField::BitrateBucket,
            RegisteredField::SelectedFormat,
            RegisteredField::BrowserFallbackEligible,
            RegisteredField::BrowserFallbackOutcome,
            RegisteredField::NativeResponseClass,
            RegisteredField::BrowserResponseClass,
            RegisteredField::MediaHttpStatus,
            RegisteredField::ContentRangeClass,
            RegisteredField::TransportSource,
            RegisteredField::ParserTiming,
            RegisteredField::SelectorTiming,
            RegisteredField::TransportTiming,
            RegisteredField::DecoderTiming,
            RegisteredField::TerminalTiming,
            RegisteredField::CancellationState,
            RegisteredField::RetryCount,
            RegisteredField::TerminalOutcome,
        ];
        let mut findings = fields
            .into_iter()
            .map(|field| {
                let (left, right) = value_pair(field);
                let (category, severity, explanation) = field_metadata(field);
                DetailedFindingV1 {
                    field: Some(field),
                    unknown_slot: None,
                    category,
                    severity,
                    explanation,
                    left: vec![left],
                    right: vec![right],
                }
            })
            .collect::<Vec<_>>();
        let (category, severity, explanation) = unknown_metadata();
        findings.push(DetailedFindingV1 {
            field: None,
            unknown_slot: Some(7),
            category,
            severity,
            explanation,
            left: vec![CanonicalValue::PrivateOpaqueCount(1)],
            right: vec![CanonicalValue::PrivateOpaqueCount(2)],
        });
        let detailed = DetailedComparisonV1 {
            schema_version: COMPARISON_RESULT_SCHEMA_VERSION,
            kind: ComparisonKind::OriginalVsReplay,
            left_ref: CaptureRef::from_bytes([31; 16]),
            right_ref: CaptureRef::from_bytes([32; 16]),
            findings,
            incomplete: false,
            dropped_findings: 0,
            malformed_records: 0,
        };
        let encoded = encode_comparison_result(&detailed).unwrap();
        let decoded = decode_comparison_result(encoded.expose()).unwrap();
        assert!(decoded == detailed);
    }

    #[test]
    fn codec_preserves_the_sixty_four_finding_bound_and_incomplete_evidence() {
        let (category, severity, explanation) = unknown_metadata();
        let detailed = DetailedComparisonV1 {
            schema_version: COMPARISON_RESULT_SCHEMA_VERSION,
            kind: ComparisonKind::WorkingVsFailing,
            left_ref: CaptureRef::from_bytes([41; 16]),
            right_ref: CaptureRef::from_bytes([42; 16]),
            findings: (0..MAX_DIFF_FINDINGS)
                .map(|index| DetailedFindingV1 {
                    field: None,
                    unknown_slot: Some(u16::try_from(index).unwrap()),
                    category,
                    severity,
                    explanation,
                    left: vec![CanonicalValue::PrivateOpaqueCount(
                        u16::try_from(index).unwrap(),
                    )],
                    right: vec![CanonicalValue::PrivateOpaqueCount(
                        u16::try_from(index + 1).unwrap(),
                    )],
                })
                .collect(),
            incomplete: true,
            dropped_findings: 17,
            malformed_records: 3,
        };
        let encoded = encode_comparison_result(&detailed).unwrap();
        let decoded = decode_comparison_result(encoded.expose()).unwrap();
        assert_eq!(decoded.findings.len(), MAX_DIFF_FINDINGS);
        assert!(decoded.incomplete);
        assert_eq!(decoded.dropped_findings, 17);
        assert_eq!(decoded.malformed_records, 3);
    }

    #[test]
    fn unknown_schema_trailing_bytes_and_malformed_results_are_rejected_by_writer() {
        let directory = tempfile::tempdir().unwrap();
        let store = open_test_store(directory.path());
        let base = minimal_detailed();
        let valid = encode_comparison_result(&base).unwrap();
        let mut unknown_schema = valid.expose().to_vec();
        unknown_schema[8..10].copy_from_slice(&2_u16.to_be_bytes());
        let mut trailing = valid.expose().to_vec();
        trailing.push(0);
        let mut malformed = valid.expose().to_vec();
        malformed[10] = 0xff;

        for (index, bytes) in [unknown_schema, trailing, malformed]
            .into_iter()
            .enumerate()
        {
            let capture = comparison_child_capture(
                CaptureRef::from_bytes([u8::try_from(index).unwrap().saturating_add(50); 16]),
                SafeOperationRef::from_bytes([5; 4]),
                1,
                2,
                SensitiveBytes::new(bytes),
            )
            .unwrap();
            assert!(matches!(
                store.write(&capture, &passphrase()),
                Err(StoreError::Format(VaultFormatError::InvalidRecordStream))
            ));
        }
    }

    #[test]
    fn codec_rejects_finding_value_and_payload_overflow_before_allocation() {
        let valid = encode_comparison_result(&minimal_detailed()).unwrap();
        let mut finding_overflow = valid.expose().to_vec();
        finding_overflow[48..50]
            .copy_from_slice(&u16::try_from(MAX_DIFF_FINDINGS + 1).unwrap().to_be_bytes());
        assert_eq!(
            decode_comparison_result(&finding_overflow).err(),
            Some(ComparisonCodecError::TooLarge)
        );

        let mut value_overflow = valid.expose().to_vec();
        value_overflow[51] = u8::try_from(MAX_VALUES_PER_SIDE + 1).unwrap();
        assert_eq!(
            decode_comparison_result(&value_overflow).err(),
            Some(ComparisonCodecError::TooLarge)
        );
        assert_eq!(
            decode_comparison_result(&vec![0; MAX_COMPARISON_RESULT_BYTES + 1]).err(),
            Some(ComparisonCodecError::TooLarge)
        );
    }

    #[test]
    fn comparison_writer_rejects_missing_or_duplicate_terminal_evidence() {
        let directory = tempfile::tempdir().unwrap();
        let store = open_test_store(directory.path());
        let encoded = encode_comparison_result(&minimal_detailed()).unwrap();
        let mut missing = comparison_child_capture(
            CaptureRef::from_bytes([60; 16]),
            SafeOperationRef::from_bytes([6; 4]),
            1,
            2,
            SensitiveBytes::new(encoded.expose().to_vec()),
        )
        .unwrap();
        missing.records.pop();
        assert!(matches!(
            store.write(&missing, &passphrase()),
            Err(StoreError::Format(
                VaultFormatError::MissingRequiredEvidence | VaultFormatError::InvalidRecordStream
            ))
        ));

        let mut duplicate = comparison_child_capture(
            CaptureRef::from_bytes([61; 16]),
            SafeOperationRef::from_bytes([6; 4]),
            1,
            2,
            encoded,
        )
        .unwrap();
        duplicate.records.push(CaptureRecordV1::with_context(
            2,
            2,
            None,
            EndpointRole::Operation,
            ProviderClientKind::Unknown,
            TransportKind::Unknown,
            0,
            CaptureRecordKind::TerminalOutcome,
            terminal_payload(SafeTerminalCategory::Success),
        ));
        assert!(matches!(
            store.write(&duplicate, &passphrase()),
            Err(StoreError::Format(
                VaultFormatError::MissingRequiredEvidence | VaultFormatError::InvalidRecordStream
            ))
        ));
    }

    fn minimal_detailed() -> DetailedComparisonV1 {
        let field = RegisteredField::TerminalOutcome;
        let (category, severity, explanation) = field_metadata(field);
        DetailedComparisonV1 {
            schema_version: COMPARISON_RESULT_SCHEMA_VERSION,
            kind: ComparisonKind::WorkingVsFailing,
            left_ref: CaptureRef::from_bytes([71; 16]),
            right_ref: CaptureRef::from_bytes([72; 16]),
            findings: vec![DetailedFindingV1 {
                field: Some(field),
                unknown_slot: None,
                category,
                severity,
                explanation,
                left: vec![CanonicalValue::Terminal(TerminalOutcome::Success)],
                right: vec![CanonicalValue::Terminal(TerminalOutcome::Failed)],
            }],
            incomplete: false,
            dropped_findings: 0,
            malformed_records: 0,
        }
    }

    fn value_pair(field: RegisteredField) -> (CanonicalValue, CanonicalValue) {
        match field {
            RegisteredField::PlayerHttpStatus | RegisteredField::MediaHttpStatus => (
                CanonicalValue::HttpStatus(200),
                CanonicalValue::HttpStatus(403),
            ),
            RegisteredField::PlayerRedirectClass => (
                CanonicalValue::Redirect(RedirectClass::None),
                CanonicalValue::Redirect(RedirectClass::Rejected),
            ),
            RegisteredField::PlayerClientKind => (
                CanonicalValue::Client(PlayerClientKind::Web),
                CanonicalValue::Client(PlayerClientKind::TvHtml5),
            ),
            RegisteredField::ClientVersionPolicy => (
                CanonicalValue::VersionPolicy(ClientVersionPolicy::Cached),
                CanonicalValue::VersionPolicy(ClientVersionPolicy::Fallback),
            ),
            RegisteredField::AuthenticationKind => (
                CanonicalValue::Authentication(AuthenticationKind::Browser),
                CanonicalValue::Authentication(AuthenticationKind::Unavailable),
            ),
            RegisteredField::ProofTokenPresent
            | RegisteredField::StreamingDataPresent
            | RegisteredField::BrowserFallbackEligible => (
                CanonicalValue::Boolean(false),
                CanonicalValue::Boolean(true),
            ),
            RegisteredField::PlayabilityStatus => (
                CanonicalValue::Playability(PlayabilityClass::Playable),
                CanonicalValue::Playability(PlayabilityClass::Unavailable),
            ),
            RegisteredField::SafeFailureCategory => (
                CanonicalValue::Failure(FailureCategory::None),
                CanonicalValue::Failure(FailureCategory::ProofToken),
            ),
            RegisteredField::ReturnedFormatCount
            | RegisteredField::SupportedFormatCount
            | RegisteredField::DirectFormatCount
            | RegisteredField::CipherFormatCount
            | RegisteredField::RetryCount => (CanonicalValue::Count(1), CanonicalValue::Count(7)),
            RegisteredField::FormatIdentifier => (
                CanonicalValue::PrivateFormat(140),
                CanonicalValue::PrivateFormat(251),
            ),
            RegisteredField::Container => (
                CanonicalValue::Container(ContainerKind::Mp4),
                CanonicalValue::Container(ContainerKind::WebM),
            ),
            RegisteredField::Codec => (
                CanonicalValue::Codec(CodecKind::Aac),
                CanonicalValue::Codec(CodecKind::Opus),
            ),
            RegisteredField::BitrateBucket => (
                CanonicalValue::Bitrate(BitrateBucket::Low),
                CanonicalValue::Bitrate(BitrateBucket::High),
            ),
            RegisteredField::SelectedFormat => (
                CanonicalValue::SelectedPrivateFormat(None),
                CanonicalValue::SelectedPrivateFormat(Some(140)),
            ),
            RegisteredField::BrowserFallbackOutcome => (
                CanonicalValue::Fallback(FallbackOutcome::NotEligible),
                CanonicalValue::Fallback(FallbackOutcome::Failed),
            ),
            RegisteredField::NativeResponseClass | RegisteredField::BrowserResponseClass => (
                CanonicalValue::Response(ResponseClassification::Playable),
                CanonicalValue::Response(ResponseClassification::Refused),
            ),
            RegisteredField::ContentRangeClass => (
                CanonicalValue::ContentRange(ContentRangeClass::Valid),
                CanonicalValue::ContentRange(ContentRangeClass::WrongStart),
            ),
            RegisteredField::TransportSource => (
                CanonicalValue::Transport(TransportSource::NativeHttp),
                CanonicalValue::Transport(TransportSource::Browser),
            ),
            RegisteredField::ParserTiming
            | RegisteredField::SelectorTiming
            | RegisteredField::TransportTiming
            | RegisteredField::DecoderTiming
            | RegisteredField::TerminalTiming => (
                CanonicalValue::Timing(TimingBucket::Fast),
                CanonicalValue::Timing(TimingBucket::Slow),
            ),
            RegisteredField::CancellationState => (
                CanonicalValue::Cancellation(CancellationState::NotCancelled),
                CanonicalValue::Cancellation(CancellationState::Superseded),
            ),
            RegisteredField::TerminalOutcome => (
                CanonicalValue::Terminal(TerminalOutcome::Success),
                CanonicalValue::Terminal(TerminalOutcome::Failed),
            ),
        }
    }
}
