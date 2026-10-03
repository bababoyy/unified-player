//! Production projection from private capture evidence into the standalone,
//! typed diagnostic derivative.
//!
//! This is the only bridge between private semantic analysis and the public-safe
//! derivative vocabulary. It deliberately accepts no strings, paths, URLs, raw
//! payloads, or provider identifiers.

use std::{
    collections::{BTreeMap, BTreeSet},
    fmt,
};

use super::{
    analysis::{compare_captures, extract_derivative_semantics, CaptureComparisonV1},
    diff::{
        AuthenticationKind, BitrateBucket, CancellationState, ClientVersionPolicy, CodecKind,
        ComparisonKind, ContainerKind, ContentRangeClass, CountBucket, DiffCategory, DiffSeverity,
        FailureCategory, FallbackOutcome, FixedExplanation, HttpStatusClass, PlayabilityClass,
        PlayerClientKind, RedirectClass, RegisteredField, ResponseClassification, SafeDiffFinding,
        SafeValue, SemanticFact, TerminalOutcome, TimingBucket, TransportSource, ValueClass,
    },
    model::{
        CaptureCompleteness, CapturePurpose, PrivateCaptureV1, SafeTerminalCategory, TransportKind,
    },
    sanitize::{
        create_derivative_with_v1, prepare_derivative_v1, review_derivative_with_v1,
        DerivativeAuthV1, DerivativeBitrateBucketV1, DerivativeBrowserComparisonV1,
        DerivativeClientV1, DerivativeClientVersionPolicyV1, DerivativeCodecV1,
        DerivativeCompletenessV1, DerivativeContainerV1, DerivativeContentRangeV1,
        DerivativeCreateOutcomeV1, DerivativeError, DerivativeExplanationV1, DerivativeFactV1,
        DerivativeFailureCategoryV1, DerivativeFallbackV1, DerivativeFieldV1,
        DerivativeFindingCategoryV1, DerivativeFindingSeverityV1, DerivativeFindingV1,
        DerivativeHttpStatusClassV1, DerivativeMediaResultV1, DerivativeOperationV1,
        DerivativePlayabilityV1, DerivativePreviewV1, DerivativeRegisteredFieldV1,
        DerivativeReplayResultV1, DerivativeResponseClassV1, DerivativeReviewV1, DerivativeStageV1,
        DerivativeStoreV1, DerivativeTerminalV1, DerivativeTimingBucketV1, DerivativeTimingV1,
        DerivativeTransportV1, DerivativeValueClassV1, PreparedDerivativeV1,
        ProviderDiagnosticDerivativeV1, SeededForbiddenScannerV1, MAX_COUNT,
    },
};

const MAX_RETRY_COUNT: u16 = 16;
const MAX_DERIVATIVE_FINDINGS: usize = 64;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum DerivativeBuildError {
    IncompatibleCapture,
    IncompatibleComparison,
    ProjectionRejected,
    ScannerRejected,
}

impl fmt::Display for DerivativeBuildError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::IncompatibleCapture => {
                "private capture is not compatible with diagnostic projection"
            }
            Self::IncompatibleComparison => {
                "private comparison is not compatible with diagnostic projection"
            }
            Self::ProjectionRejected => "safe diagnostic projection was rejected",
            Self::ScannerRejected => "safe diagnostic projection failed privacy review",
        })
    }
}

impl std::error::Error for DerivativeBuildError {}

/// Keeps the private canary scanner beside its prepared allowlisted output.
///
/// It intentionally has no formatting, cloning, or serialization implementation:
/// moving this value into UI, clipboard, logging, or support-bundle state would
/// also move private canaries across the privacy boundary.
pub(crate) struct PreparedPrivateDerivativeV1 {
    prepared: PreparedDerivativeV1,
    scanner: SeededForbiddenScannerV1,
}

impl PreparedPrivateDerivativeV1 {
    pub(crate) fn preview(&self) -> DerivativePreviewV1 {
        self.prepared.preview()
    }

    pub(crate) const fn review(&self) -> &DerivativeReviewV1 {
        self.prepared.review()
    }

    pub(crate) fn create(
        &self,
        store: &mut impl DerivativeStoreV1,
    ) -> Result<DerivativeCreateOutcomeV1, DerivativeBuildError> {
        create_derivative_with_v1(&self.prepared, store, &self.scanner)
            .map_err(classify_derivative_error)
    }

    pub(crate) fn review_created(
        &self,
        store: &impl DerivativeStoreV1,
    ) -> Result<DerivativeReviewV1, DerivativeBuildError> {
        review_derivative_with_v1(store, &self.scanner).map_err(classify_derivative_error)
    }
}

pub(crate) fn prepare_derivative_from_capture(
    capture: &PrivateCaptureV1,
) -> Result<PreparedPrivateDerivativeV1, DerivativeBuildError> {
    let derivative = build_derivative_from_capture(capture)?;
    let scanner = SeededForbiddenScannerV1::from_private_capture(capture)
        .map_err(|_| DerivativeBuildError::ScannerRejected)?;
    prepare_private_derivative(derivative, scanner)
}

pub(crate) fn prepare_derivative_from_comparison(
    comparison: &CaptureComparisonV1,
    left: &PrivateCaptureV1,
    right: &PrivateCaptureV1,
) -> Result<PreparedPrivateDerivativeV1, DerivativeBuildError> {
    validate_comparison_sources(comparison, left, right)?;
    if !comparison.matches_sources(left, right) {
        return Err(DerivativeBuildError::IncompatibleComparison);
    }
    let expected = compare_captures(comparison.safe_summary().kind(), left, right);
    if expected.safe_summary() != comparison.safe_summary()
        || expected.private_report() != comparison.private_report()
    {
        return Err(DerivativeBuildError::IncompatibleComparison);
    }

    let derivative = build_derivative_from_comparison(comparison)?;
    let scanner = SeededForbiddenScannerV1::from_private_captures(&[left, right])
        .map_err(|_| DerivativeBuildError::ScannerRejected)?;
    prepare_private_derivative(derivative, scanner)
}

fn prepare_private_derivative(
    derivative: ProviderDiagnosticDerivativeV1,
    scanner: SeededForbiddenScannerV1,
) -> Result<PreparedPrivateDerivativeV1, DerivativeBuildError> {
    let prepared =
        prepare_derivative_v1(derivative, &scanner).map_err(classify_derivative_error)?;
    Ok(PreparedPrivateDerivativeV1 { prepared, scanner })
}

fn classify_derivative_error(error: DerivativeError) -> DerivativeBuildError {
    match error {
        DerivativeError::ForbiddenDataFound
        | DerivativeError::InvalidScannerConfiguration
        | DerivativeError::ScannerFailed => DerivativeBuildError::ScannerRejected,
        _ => DerivativeBuildError::ProjectionRejected,
    }
}

pub(crate) fn build_derivative_from_capture(
    capture: &PrivateCaptureV1,
) -> Result<ProviderDiagnosticDerivativeV1, DerivativeBuildError> {
    let (operation, mut transports) = capture_operation_and_transports(capture)?;
    let semantics = extract_derivative_semantics(capture);
    let mut incomplete = capture.completeness != CaptureCompleteness::Complete
        || capture.dropped_records != 0
        || semantics.incomplete()
        || semantics.malformed_records() != 0
        || semantics.has_unknown_private();

    let mut facts = FactAccumulator::default();
    let mut timings = TimingAccumulator::default();
    let mut failure = Unique::default();
    let mut fallback_eligible = Unique::default();
    let mut fallback_outcome = Unique::default();
    let mut native_response = Unique::default();
    let mut browser_response = Unique::default();
    let mut media_status = Unique::default();
    let mut content_range = Unique::default();
    let mut terminal_outcome = Unique::default();
    let has_media_transport = transports.contains(&DerivativeTransportV1::MediaRange);

    for fact in semantics.registered_facts() {
        match fact {
            SemanticFact::PlayerHttpStatus(status) => {
                incomplete |= facts.insert(DerivativeFactV1::PlayerHttpStatusClass(
                    map_http_status(status.class()),
                ));
            }
            SemanticFact::PlayerRedirectClass(redirect) => {
                incomplete |= facts.insert(DerivativeFactV1::Redirected(!matches!(
                    redirect,
                    RedirectClass::None
                )));
            }
            SemanticFact::PlayerClientKind(client) => {
                incomplete |= facts.insert(DerivativeFactV1::Client(map_client(*client)));
            }
            SemanticFact::ClientVersionPolicy(policy) => {
                incomplete |= facts.insert(DerivativeFactV1::ClientVersionPolicy(
                    map_version_policy(*policy),
                ));
            }
            SemanticFact::AuthenticationKind(authentication) => {
                incomplete |= facts.insert(DerivativeFactV1::Authentication(map_authentication(
                    *authentication,
                )));
            }
            SemanticFact::ProofTokenPresent(present) => {
                incomplete |= facts.insert(DerivativeFactV1::ProofTokenPresent(*present));
            }
            SemanticFact::PlayabilityStatus(playability) => {
                incomplete |=
                    facts.insert(DerivativeFactV1::Playability(map_playability(*playability)));
            }
            SemanticFact::SafeFailureCategory(category) => failure.observe(*category),
            SemanticFact::StreamingDataPresent(present) => {
                incomplete |= facts.insert(DerivativeFactV1::StreamingDataPresent(*present));
            }
            SemanticFact::ReturnedFormatCount(count) => {
                let (count, clamped) = bounded_count(count.value());
                incomplete |= clamped || facts.insert(DerivativeFactV1::ReturnedFormats(count));
            }
            SemanticFact::SupportedFormatCount(count) => {
                let (count, clamped) = bounded_count(count.value());
                incomplete |= clamped || facts.insert(DerivativeFactV1::SupportedFormats(count));
            }
            SemanticFact::DirectFormatCount(count) => {
                let (count, clamped) = bounded_count(count.value());
                incomplete |= clamped || facts.insert(DerivativeFactV1::DirectFormats(count));
            }
            SemanticFact::CipherFormatCount(count) => {
                let (count, clamped) = bounded_count(count.value());
                incomplete |= clamped || facts.insert(DerivativeFactV1::CipherFormats(count));
            }
            SemanticFact::FormatIdentifier(_) => {
                // Exact provider identifiers never cross into the derivative.
            }
            SemanticFact::Container(container) => {
                incomplete |= facts.insert(DerivativeFactV1::SelectedContainer(map_container(
                    *container,
                )));
            }
            SemanticFact::Codec(codec) => {
                incomplete |= facts.insert(DerivativeFactV1::SelectedCodec(map_codec(*codec)));
            }
            SemanticFact::BitrateBucket(bitrate) => {
                incomplete |=
                    facts.insert(DerivativeFactV1::SelectedBitrate(map_bitrate(*bitrate)));
            }
            SemanticFact::SelectedFormat(selected) => {
                incomplete |=
                    facts.insert(DerivativeFactV1::SelectedFormatPresent(selected.is_some()));
            }
            SemanticFact::BrowserFallbackEligible(eligible) => {
                fallback_eligible.observe(*eligible);
            }
            SemanticFact::BrowserFallbackOutcome(outcome) => fallback_outcome.observe(*outcome),
            SemanticFact::NativeResponseClass(response) => native_response.observe(*response),
            SemanticFact::BrowserResponseClass(response) => browser_response.observe(*response),
            SemanticFact::MediaHttpStatus(status) => {
                media_status.observe(status.class());
                incomplete |= facts.insert(DerivativeFactV1::MediaHttpStatusClass(
                    map_http_status(status.class()),
                ));
            }
            SemanticFact::ContentRangeClass(range) => content_range.observe(*range),
            SemanticFact::TransportSource(source) => {
                if let Some(transport) = map_transport_source(*source) {
                    transports.insert(transport);
                }
            }
            SemanticFact::ParserTiming(bucket) => {
                incomplete |= timings.insert(DerivativeStageV1::Parser, map_timing_bucket(*bucket));
            }
            SemanticFact::SelectorTiming(bucket) => {
                incomplete |=
                    timings.insert(DerivativeStageV1::Selector, map_timing_bucket(*bucket));
            }
            SemanticFact::TransportTiming(bucket) => {
                let stage = if has_media_transport {
                    DerivativeStageV1::MediaTransport
                } else {
                    DerivativeStageV1::PlayerResponse
                };
                incomplete |= timings.insert(stage, map_timing_bucket(*bucket));
            }
            SemanticFact::DecoderTiming(bucket) => {
                incomplete |=
                    timings.insert(DerivativeStageV1::Decoder, map_timing_bucket(*bucket));
            }
            SemanticFact::TerminalTiming(bucket) => {
                incomplete |=
                    timings.insert(DerivativeStageV1::Terminal, map_timing_bucket(*bucket));
            }
            SemanticFact::CancellationState(state) => {
                let (cancelled, superseded) = match state {
                    CancellationState::NotCancelled => (false, false),
                    CancellationState::Cancelled | CancellationState::Shutdown => (true, false),
                    CancellationState::Superseded => (true, true),
                };
                incomplete |= facts.insert(DerivativeFactV1::Cancelled(cancelled));
                incomplete |= facts.insert(DerivativeFactV1::Superseded(superseded));
            }
            SemanticFact::RetryCount(count) => {
                let clamped = count.value().min(MAX_RETRY_COUNT);
                incomplete |= count.value() > MAX_RETRY_COUNT
                    || facts.insert(DerivativeFactV1::RetryCount(
                        u8::try_from(clamped).expect("retry count is clamped to u8"),
                    ));
            }
            SemanticFact::TerminalOutcome(outcome) => terminal_outcome.observe(*outcome),
        }
    }

    incomplete |= failure.conflicted
        || fallback_eligible.conflicted
        || fallback_outcome.conflicted
        || native_response.conflicted
        || browser_response.conflicted
        || media_status.conflicted
        || content_range.conflicted
        || terminal_outcome.conflicted;

    if let Some(category) = select_failure(failure) {
        incomplete |= facts.insert(DerivativeFactV1::FailureCategory(map_failure(category)));
    }

    if let Some(fallback) = project_fallback(fallback_eligible, fallback_outcome) {
        incomplete |= facts.insert(DerivativeFactV1::Fallback(fallback));
    }

    if let Some(response) = native_response.value {
        incomplete |= facts.insert(DerivativeFactV1::NativeResponse(map_response(response)));
    }
    if let Some(response) = browser_response.value {
        incomplete |= facts.insert(DerivativeFactV1::BrowserResponse(map_response(response)));
    }
    incomplete |= facts.insert(DerivativeFactV1::BrowserComparison(
        compare_browser_response(native_response.value, browser_response.value),
    ));

    if let Some(range) = content_range.value {
        incomplete |= facts.insert(DerivativeFactV1::ContentRange(map_content_range(range)));
    }

    match project_media_result(
        has_media_transport,
        media_status.value,
        content_range.value,
        failure.value,
    ) {
        Some(result) => {
            incomplete |= facts.insert(DerivativeFactV1::MediaResult(result));
        }
        None => incomplete = true,
    }

    let terminal = map_terminal(capture.terminal_category);
    if terminal_outcome
        .value
        .is_some_and(|semantic| map_terminal_outcome(semantic) != terminal)
    {
        incomplete = true;
    }

    if operation == DerivativeOperationV1::InteractivePlayback {
        incomplete |= facts.insert(DerivativeFactV1::ReplayResult(
            DerivativeReplayResultV1::NotRun,
        ));
    } else {
        incomplete |= facts.insert(DerivativeFactV1::ReplayResult(project_replay_result(
            capture.terminal_category,
            failure.value,
        )));
    }

    if facts
        .get(DerivativeFieldV1::SelectedFormatPresent)
        .is_some_and(|fact| matches!(fact, DerivativeFactV1::SelectedFormatPresent(false)))
    {
        facts.remove(DerivativeFieldV1::SelectedContainer);
        facts.remove(DerivativeFieldV1::SelectedCodec);
        facts.remove(DerivativeFieldV1::SelectedBitrate);
    }

    ProviderDiagnosticDerivativeV1::new(
        operation,
        completeness(incomplete),
        terminal,
        transports.into_iter().collect(),
        facts.into_values(),
        timings.into_values(),
        Vec::new(),
    )
    .map_err(|_| DerivativeBuildError::ProjectionRejected)
}

pub(crate) fn build_derivative_from_comparison(
    comparison: &CaptureComparisonV1,
) -> Result<ProviderDiagnosticDerivativeV1, DerivativeBuildError> {
    let report = comparison.private_report();
    let safe = comparison.safe_summary();
    if report.schema_version() != safe.schema_version()
        || report.kind() != safe.kind()
        || report.safe_projection() != safe.findings()
        || report.dropped_findings() != safe.dropped_findings()
    {
        return Err(DerivativeBuildError::IncompatibleComparison);
    }

    let (findings, projection_incomplete) = project_findings(safe.findings())?;
    let private_findings_omitted = report.findings().len() != report.safe_projection().len();
    let incomplete = safe.incomplete()
        || report.incomplete()
        || safe.dropped_findings() != 0
        || safe.malformed_records() != 0
        || private_findings_omitted
        || projection_incomplete;

    ProviderDiagnosticDerivativeV1::new(
        DerivativeOperationV1::Comparison,
        completeness(incomplete),
        if incomplete {
            DerivativeTerminalV1::Inconclusive
        } else {
            DerivativeTerminalV1::Success
        },
        Vec::new(),
        Vec::new(),
        Vec::new(),
        findings,
    )
    .map_err(|_| DerivativeBuildError::ProjectionRejected)
}

fn validate_comparison_sources(
    comparison: &CaptureComparisonV1,
    left: &PrivateCaptureV1,
    right: &PrivateCaptureV1,
) -> Result<(), DerivativeBuildError> {
    let compatible = match comparison.safe_summary().kind() {
        ComparisonKind::WorkingVsFailing => {
            left.purpose() == CapturePurpose::InteractivePlayback
                && right.purpose() == CapturePurpose::InteractivePlayback
        }
        ComparisonKind::OriginalVsReplay => {
            left.purpose() == CapturePurpose::InteractivePlayback
                && right.purpose() == CapturePurpose::Replay
        }
    };
    if compatible {
        Ok(())
    } else {
        Err(DerivativeBuildError::IncompatibleComparison)
    }
}

fn capture_operation_and_transports(
    capture: &PrivateCaptureV1,
) -> Result<(DerivativeOperationV1, BTreeSet<DerivativeTransportV1>), DerivativeBuildError> {
    let mut transports = BTreeSet::new();
    let mut replay_transport = None;
    for record in capture.records() {
        let mapped = match record.transport_kind() {
            TransportKind::Unknown => None,
            TransportKind::NativeHttp => Some(DerivativeTransportV1::NativeHttp),
            TransportKind::BrowserCdp => Some(DerivativeTransportV1::BrowserCdp),
            TransportKind::MediaRange => Some(DerivativeTransportV1::MediaRange),
            TransportKind::OfflineReplay => {
                observe_replay_transport(
                    &mut replay_transport,
                    DerivativeTransportV1::OfflineReplay,
                )?;
                Some(DerivativeTransportV1::OfflineReplay)
            }
            TransportKind::FreshReplay => {
                observe_replay_transport(
                    &mut replay_transport,
                    DerivativeTransportV1::FreshReplay,
                )?;
                Some(DerivativeTransportV1::FreshReplay)
            }
        };
        if let Some(mapped) = mapped {
            transports.insert(mapped);
        }
    }

    let operation = match capture.purpose() {
        CapturePurpose::InteractivePlayback if replay_transport.is_none() => {
            if transports.is_empty() {
                transports.insert(DerivativeTransportV1::NotReached);
            }
            DerivativeOperationV1::InteractivePlayback
        }
        CapturePurpose::Replay => match replay_transport {
            Some(DerivativeTransportV1::OfflineReplay)
                if transports.len() == 1
                    && transports.contains(&DerivativeTransportV1::OfflineReplay) =>
            {
                DerivativeOperationV1::OfflineReplay
            }
            Some(DerivativeTransportV1::FreshReplay)
                if transports.len() == 1
                    && transports.contains(&DerivativeTransportV1::FreshReplay) =>
            {
                DerivativeOperationV1::FreshReplay
            }
            _ => return Err(DerivativeBuildError::IncompatibleCapture),
        },
        CapturePurpose::InteractivePlayback
        | CapturePurpose::Prefetch
        | CapturePurpose::Resume
        | CapturePurpose::Probe
        | CapturePurpose::Comparison => {
            return Err(DerivativeBuildError::IncompatibleCapture);
        }
    };
    Ok((operation, transports))
}

fn observe_replay_transport(
    current: &mut Option<DerivativeTransportV1>,
    next: DerivativeTransportV1,
) -> Result<(), DerivativeBuildError> {
    if current.is_some_and(|current| current != next) {
        return Err(DerivativeBuildError::IncompatibleCapture);
    }
    *current = Some(next);
    Ok(())
}

#[derive(Default)]
struct FactAccumulator {
    facts: BTreeMap<DerivativeFieldV1, DerivativeFactV1>,
    conflicted: BTreeSet<DerivativeFieldV1>,
}

impl FactAccumulator {
    /// Returns true only when the new value creates or repeats a conflict.
    fn insert(&mut self, fact: DerivativeFactV1) -> bool {
        let field = fact.field();
        if self.conflicted.contains(&field) {
            return true;
        }
        match self.facts.get(&field) {
            Some(existing) if *existing == fact => false,
            Some(_) => {
                self.facts.remove(&field);
                self.conflicted.insert(field);
                true
            }
            None => {
                self.facts.insert(field, fact);
                false
            }
        }
    }

    fn get(&self, field: DerivativeFieldV1) -> Option<DerivativeFactV1> {
        self.facts.get(&field).copied()
    }

    fn remove(&mut self, field: DerivativeFieldV1) {
        self.facts.remove(&field);
    }

    fn into_values(self) -> Vec<DerivativeFactV1> {
        self.facts.into_values().collect()
    }
}

#[derive(Default)]
struct TimingAccumulator {
    timings: BTreeMap<DerivativeStageV1, DerivativeTimingBucketV1>,
    conflicted: BTreeSet<DerivativeStageV1>,
}

impl TimingAccumulator {
    fn insert(&mut self, stage: DerivativeStageV1, bucket: DerivativeTimingBucketV1) -> bool {
        if self.conflicted.contains(&stage) {
            return true;
        }
        match self.timings.get(&stage) {
            Some(existing) if *existing == bucket => false,
            Some(_) => {
                self.timings.remove(&stage);
                self.conflicted.insert(stage);
                true
            }
            None => {
                self.timings.insert(stage, bucket);
                false
            }
        }
    }

    fn into_values(self) -> Vec<DerivativeTimingV1> {
        self.timings
            .into_iter()
            .map(|(stage, bucket)| DerivativeTimingV1::from_bucket(stage, bucket))
            .collect()
    }
}

#[derive(Clone, Copy)]
struct Unique<T> {
    value: Option<T>,
    conflicted: bool,
}

impl<T> Default for Unique<T> {
    fn default() -> Self {
        Self {
            value: None,
            conflicted: false,
        }
    }
}

impl<T: Copy + Eq> Unique<T> {
    fn observe(&mut self, next: T) {
        if self.value.is_some_and(|current| current != next) {
            self.conflicted = true;
        } else if self.value.is_none() {
            self.value = Some(next);
        }
    }
}

fn select_failure(failure: Unique<FailureCategory>) -> Option<FailureCategory> {
    if failure.conflicted {
        Some(FailureCategory::Unknown)
    } else {
        failure.value
    }
}

fn project_fallback(
    eligible: Unique<bool>,
    outcome: Unique<FallbackOutcome>,
) -> Option<DerivativeFallbackV1> {
    match (eligible.value, outcome.value) {
        (Some(false), _) | (_, Some(FallbackOutcome::NotEligible)) => {
            Some(DerivativeFallbackV1::NotEligible)
        }
        (Some(true) | None, Some(FallbackOutcome::Eligible)) | (Some(true), None) => {
            Some(DerivativeFallbackV1::EligibleNotAttempted)
        }
        (_, Some(FallbackOutcome::Attempted)) => Some(DerivativeFallbackV1::Attempted),
        (_, Some(FallbackOutcome::Succeeded)) => Some(DerivativeFallbackV1::AttemptedSucceeded),
        (_, Some(FallbackOutcome::Failed)) => Some(DerivativeFallbackV1::AttemptedFailed),
        (None, None) => None,
    }
}

const fn compare_browser_response(
    native: Option<ResponseClassification>,
    browser: Option<ResponseClassification>,
) -> DerivativeBrowserComparisonV1 {
    match (native, browser) {
        (_, None) => DerivativeBrowserComparisonV1::NotRequested,
        (
            _,
            Some(
                ResponseClassification::Malformed
                | ResponseClassification::TransportFailure
                | ResponseClassification::Cancelled,
            ),
        ) => DerivativeBrowserComparisonV1::Failed,
        (Some(native), Some(browser)) if response_discriminant(native, browser) => {
            DerivativeBrowserComparisonV1::MatchedNative
        }
        (Some(_), Some(_)) => DerivativeBrowserComparisonV1::DifferedFromNative,
        (None, Some(_)) => DerivativeBrowserComparisonV1::Inconclusive,
    }
}

const fn response_discriminant(
    left: ResponseClassification,
    right: ResponseClassification,
) -> bool {
    matches!(
        (left, right),
        (
            ResponseClassification::Playable,
            ResponseClassification::Playable
        ) | (
            ResponseClassification::Refused,
            ResponseClassification::Refused
        ) | (
            ResponseClassification::Malformed,
            ResponseClassification::Malformed
        ) | (
            ResponseClassification::TransportFailure,
            ResponseClassification::TransportFailure
        ) | (
            ResponseClassification::Cancelled,
            ResponseClassification::Cancelled
        ) | (
            ResponseClassification::Unknown,
            ResponseClassification::Unknown
        )
    )
}

const fn project_media_result(
    reached: bool,
    status: Option<HttpStatusClass>,
    range: Option<ContentRangeClass>,
    failure: Option<FailureCategory>,
) -> Option<DerivativeMediaResultV1> {
    if !reached {
        return Some(DerivativeMediaResultV1::NotReached);
    }
    match failure {
        Some(FailureCategory::MediaForbidden) => {
            return Some(DerivativeMediaResultV1::Forbidden);
        }
        Some(FailureCategory::MediaRangeContract) => {
            return Some(DerivativeMediaResultV1::RangeContractFailure);
        }
        Some(FailureCategory::Network) => {
            return Some(DerivativeMediaResultV1::NetworkFailure);
        }
        Some(FailureCategory::Cancelled) => {
            return Some(DerivativeMediaResultV1::Cancelled);
        }
        _ => {}
    }
    match (status, range) {
        (Some(HttpStatusClass::Forbidden), _) => Some(DerivativeMediaResultV1::Forbidden),
        (
            _,
            Some(
                ContentRangeClass::Missing
                | ContentRangeClass::WrongStart
                | ContentRangeClass::Invalid,
            ),
        ) => Some(DerivativeMediaResultV1::RangeContractFailure),
        (Some(HttpStatusClass::Success), Some(ContentRangeClass::Valid)) => {
            Some(DerivativeMediaResultV1::PartialContent)
        }
        _ => None,
    }
}

const fn project_replay_result(
    terminal: SafeTerminalCategory,
    failure: Option<FailureCategory>,
) -> DerivativeReplayResultV1 {
    match terminal {
        SafeTerminalCategory::Success => DerivativeReplayResultV1::Reproduced,
        SafeTerminalCategory::Cancelled | SafeTerminalCategory::Superseded => {
            DerivativeReplayResultV1::Cancelled
        }
        SafeTerminalCategory::TimedOut => DerivativeReplayResultV1::TimedOut,
        SafeTerminalCategory::Failed => match failure {
            Some(FailureCategory::Authentication) => {
                DerivativeReplayResultV1::AuthenticationUnavailable
            }
            Some(FailureCategory::Network) => DerivativeReplayResultV1::NetworkFailed,
            _ => DerivativeReplayResultV1::Inconclusive,
        },
        SafeTerminalCategory::Panicked => DerivativeReplayResultV1::Inconclusive,
    }
}

fn project_findings(
    source: &[SafeDiffFinding],
) -> Result<(Vec<DerivativeFindingV1>, bool), DerivativeBuildError> {
    let mut findings = Vec::with_capacity(source.len().min(MAX_DERIVATIVE_FINDINGS));
    let incomplete = source.len() > MAX_DERIVATIVE_FINDINGS;
    for finding in source.iter().take(MAX_DERIVATIVE_FINDINGS) {
        if safe_value_class(finding.left) != finding.left_class
            || safe_value_class(finding.right) != finding.right_class
            || finding.category == DiffCategory::PrivateUnknown
            || finding.explanation == FixedExplanation::UnknownPrivateFactChanged
        {
            return Err(DerivativeBuildError::IncompatibleComparison);
        }
        let field = map_registered_field(finding.field);
        findings.push(DerivativeFindingV1::new(
            map_finding_category(finding.category, finding.field),
            map_severity(finding.severity),
            field,
            map_safe_value(finding.left),
            map_safe_value(finding.right),
            map_explanation(finding.explanation),
        ));
    }
    Ok((findings, incomplete))
}

const fn safe_value_class(value: SafeValue) -> ValueClass {
    match value {
        SafeValue::Missing => ValueClass::Missing,
        SafeValue::Multiple => ValueClass::Multiple,
        SafeValue::Boolean(_) => ValueClass::Boolean,
        SafeValue::HttpStatus(_) => ValueClass::HttpStatus,
        SafeValue::Redirect(_) => ValueClass::Redirect,
        SafeValue::Client(_) => ValueClass::Client,
        SafeValue::VersionPolicy(_) => ValueClass::VersionPolicy,
        SafeValue::Authentication(_) => ValueClass::Authentication,
        SafeValue::Playability(_) => ValueClass::Playability,
        SafeValue::FailureCategory(_) => ValueClass::FailureCategory,
        SafeValue::Count(_) => ValueClass::Count,
        SafeValue::PrivateValuePresent => ValueClass::PrivateValue,
        SafeValue::Container(_) => ValueClass::Container,
        SafeValue::Codec(_) => ValueClass::Codec,
        SafeValue::Bitrate(_) => ValueClass::Bitrate,
        SafeValue::Fallback(_) => ValueClass::Fallback,
        SafeValue::Response(_) => ValueClass::Response,
        SafeValue::ContentRange(_) => ValueClass::ContentRange,
        SafeValue::Transport(_) => ValueClass::Transport,
        SafeValue::Timing(_) => ValueClass::Timing,
        SafeValue::Cancellation(_) => ValueClass::Cancellation,
        SafeValue::Terminal(_) => ValueClass::Terminal,
    }
}

const fn map_registered_field(field: RegisteredField) -> DerivativeRegisteredFieldV1 {
    match field {
        RegisteredField::PlayerHttpStatus | RegisteredField::MediaHttpStatus => {
            DerivativeRegisteredFieldV1::HttpStatusClass
        }
        RegisteredField::PlayerRedirectClass
        | RegisteredField::PlayerClientKind
        | RegisteredField::ClientVersionPolicy => DerivativeRegisteredFieldV1::ClientPolicy,
        RegisteredField::AuthenticationKind => DerivativeRegisteredFieldV1::AuthenticationKind,
        RegisteredField::ProofTokenPresent => DerivativeRegisteredFieldV1::ProofTokenPresence,
        RegisteredField::PlayabilityStatus | RegisteredField::SafeFailureCategory => {
            DerivativeRegisteredFieldV1::PlayabilityCategory
        }
        RegisteredField::StreamingDataPresent => DerivativeRegisteredFieldV1::StreamingDataPresence,
        RegisteredField::ReturnedFormatCount
        | RegisteredField::SupportedFormatCount
        | RegisteredField::DirectFormatCount
        | RegisteredField::CipherFormatCount
        | RegisteredField::FormatIdentifier
        | RegisteredField::Container
        | RegisteredField::Codec
        | RegisteredField::BitrateBucket => DerivativeRegisteredFieldV1::FormatCounts,
        RegisteredField::SelectedFormat => DerivativeRegisteredFieldV1::SelectedFormat,
        RegisteredField::BrowserFallbackEligible | RegisteredField::BrowserFallbackOutcome => {
            DerivativeRegisteredFieldV1::FallbackOutcome
        }
        RegisteredField::NativeResponseClass | RegisteredField::BrowserResponseClass => {
            DerivativeRegisteredFieldV1::BrowserClassification
        }
        RegisteredField::ContentRangeClass | RegisteredField::TransportSource => {
            DerivativeRegisteredFieldV1::MediaClassification
        }
        RegisteredField::ParserTiming
        | RegisteredField::SelectorTiming
        | RegisteredField::TransportTiming
        | RegisteredField::DecoderTiming
        | RegisteredField::TerminalTiming => DerivativeRegisteredFieldV1::StageTiming,
        RegisteredField::CancellationState
        | RegisteredField::RetryCount
        | RegisteredField::TerminalOutcome => DerivativeRegisteredFieldV1::TerminalOutcome,
    }
}

const fn map_finding_category(
    category: DiffCategory,
    field: RegisteredField,
) -> DerivativeFindingCategoryV1 {
    match category {
        DiffCategory::ProviderResponse => DerivativeFindingCategoryV1::PlayerResponse,
        DiffCategory::Authentication => DerivativeFindingCategoryV1::Authentication,
        DiffCategory::FormatSelection if matches!(field, RegisteredField::SelectedFormat) => {
            DerivativeFindingCategoryV1::Selection
        }
        DiffCategory::FormatSelection => DerivativeFindingCategoryV1::FormatInventory,
        DiffCategory::Fallback => DerivativeFindingCategoryV1::BrowserFallback,
        DiffCategory::MediaTransport => DerivativeFindingCategoryV1::MediaTransport,
        DiffCategory::Performance => DerivativeFindingCategoryV1::Timing,
        DiffCategory::Lifecycle | DiffCategory::PrivateUnknown => {
            DerivativeFindingCategoryV1::Terminal
        }
    }
}

const fn map_severity(severity: DiffSeverity) -> DerivativeFindingSeverityV1 {
    match severity {
        DiffSeverity::Critical => DerivativeFindingSeverityV1::Critical,
        DiffSeverity::High | DiffSeverity::Medium => DerivativeFindingSeverityV1::Material,
        DiffSeverity::Low | DiffSeverity::Informational => {
            DerivativeFindingSeverityV1::Informational
        }
    }
}

const fn map_explanation(explanation: FixedExplanation) -> DerivativeExplanationV1 {
    match explanation {
        FixedExplanation::PlayerClientChanged
        | FixedExplanation::ClientVersionPolicyChanged
        | FixedExplanation::AuthenticationChanged
        | FixedExplanation::ProofTokenPresenceChanged => {
            DerivativeExplanationV1::AuthenticationPolicyChanged
        }
        FixedExplanation::PlayerHttpStatusChanged
        | FixedExplanation::RedirectBehaviorChanged
        | FixedExplanation::FailureCategoryChanged => {
            DerivativeExplanationV1::ProviderStatusChanged
        }
        FixedExplanation::PlayabilityChanged => DerivativeExplanationV1::PlayabilityChanged,
        FixedExplanation::StreamingDataPresenceChanged => {
            DerivativeExplanationV1::StreamingDataChanged
        }
        FixedExplanation::ReturnedFormatCountChanged
        | FixedExplanation::SupportedFormatCountChanged
        | FixedExplanation::DirectFormatCountChanged
        | FixedExplanation::CipherFormatCountChanged
        | FixedExplanation::FormatInventoryChanged
        | FixedExplanation::ContainerChanged
        | FixedExplanation::CodecChanged
        | FixedExplanation::BitrateClassChanged => {
            DerivativeExplanationV1::FormatAvailabilityChanged
        }
        FixedExplanation::SelectedFormatChanged => DerivativeExplanationV1::SelectedFormatChanged,
        FixedExplanation::FallbackEligibilityChanged
        | FixedExplanation::FallbackOutcomeChanged
        | FixedExplanation::NativeResponseChanged
        | FixedExplanation::BrowserResponseChanged => {
            DerivativeExplanationV1::BrowserAndNativeDiffered
        }
        FixedExplanation::MediaHttpStatusChanged
        | FixedExplanation::ContentRangeChanged
        | FixedExplanation::TransportSourceChanged => {
            DerivativeExplanationV1::MediaTransportChanged
        }
        FixedExplanation::DecoderTimingChanged => DerivativeExplanationV1::DecoderOutcomeChanged,
        FixedExplanation::ParserTimingChanged
        | FixedExplanation::SelectorTimingChanged
        | FixedExplanation::TransportTimingChanged
        | FixedExplanation::TerminalTimingChanged => {
            DerivativeExplanationV1::TimingMateriallyChanged
        }
        FixedExplanation::CancellationChanged
        | FixedExplanation::RetryCountChanged
        | FixedExplanation::TerminalOutcomeChanged
        | FixedExplanation::UnknownPrivateFactChanged => {
            DerivativeExplanationV1::TerminalOutcomeChanged
        }
    }
}

#[allow(clippy::match_same_arms)] // Keep the security projection exhaustive and auditable by input.
const fn map_safe_value(value: SafeValue) -> DerivativeValueClassV1 {
    match value {
        SafeValue::Missing => DerivativeValueClassV1::Absent,
        SafeValue::Multiple => DerivativeValueClassV1::Many,
        SafeValue::Boolean(false) => DerivativeValueClassV1::False,
        SafeValue::Boolean(true) => DerivativeValueClassV1::True,
        SafeValue::HttpStatus(status) => match status {
            HttpStatusClass::Success => DerivativeValueClassV1::Success,
            HttpStatusClass::Unauthorized => DerivativeValueClassV1::Unauthorized,
            HttpStatusClass::Forbidden => DerivativeValueClassV1::Forbidden,
            HttpStatusClass::RateLimited => DerivativeValueClassV1::RateLimited,
            HttpStatusClass::OtherClientError => DerivativeValueClassV1::ClientError,
            HttpStatusClass::ServerError => DerivativeValueClassV1::ServerError,
            HttpStatusClass::Other => DerivativeValueClassV1::Unknown,
        },
        SafeValue::Redirect(RedirectClass::None) => DerivativeValueClassV1::False,
        SafeValue::Redirect(_) => DerivativeValueClassV1::Redirect,
        SafeValue::Client(PlayerClientKind::Unknown)
        | SafeValue::VersionPolicy(ClientVersionPolicy::Unknown) => DerivativeValueClassV1::Unknown,
        SafeValue::Client(_) | SafeValue::VersionPolicy(_) => DerivativeValueClassV1::Present,
        SafeValue::Authentication(AuthenticationKind::None) => DerivativeValueClassV1::Absent,
        SafeValue::Authentication(AuthenticationKind::Unavailable) => {
            DerivativeValueClassV1::Unknown
        }
        SafeValue::Authentication(_) => DerivativeValueClassV1::Present,
        SafeValue::Playability(PlayabilityClass::Playable) => DerivativeValueClassV1::Playable,
        SafeValue::Playability(_) => DerivativeValueClassV1::Unavailable,
        SafeValue::FailureCategory(FailureCategory::None) => DerivativeValueClassV1::Success,
        SafeValue::FailureCategory(FailureCategory::Cancelled) => DerivativeValueClassV1::Cancelled,
        SafeValue::FailureCategory(FailureCategory::Unknown) => DerivativeValueClassV1::Unknown,
        SafeValue::FailureCategory(_) => DerivativeValueClassV1::Failed,
        SafeValue::Count(CountBucket::None) => DerivativeValueClassV1::Zero,
        SafeValue::Count(CountBucket::One) => DerivativeValueClassV1::One,
        SafeValue::Count(CountBucket::Several | CountBucket::Many) => DerivativeValueClassV1::Many,
        SafeValue::PrivateValuePresent => DerivativeValueClassV1::Present,
        SafeValue::Container(_) | SafeValue::Codec(_) => DerivativeValueClassV1::Present,
        SafeValue::Bitrate(BitrateBucket::Unknown) => DerivativeValueClassV1::Unknown,
        SafeValue::Bitrate(_) => DerivativeValueClassV1::Present,
        SafeValue::Fallback(FallbackOutcome::NotEligible) => DerivativeValueClassV1::Unsupported,
        SafeValue::Fallback(FallbackOutcome::Eligible | FallbackOutcome::Attempted) => {
            DerivativeValueClassV1::Supported
        }
        SafeValue::Fallback(FallbackOutcome::Succeeded) => DerivativeValueClassV1::Success,
        SafeValue::Fallback(FallbackOutcome::Failed) => DerivativeValueClassV1::Failed,
        SafeValue::Response(ResponseClassification::Playable) => DerivativeValueClassV1::Success,
        SafeValue::Response(ResponseClassification::Cancelled) => DerivativeValueClassV1::Cancelled,
        SafeValue::Response(ResponseClassification::Unknown) => DerivativeValueClassV1::Unknown,
        SafeValue::Response(_) => DerivativeValueClassV1::Failed,
        SafeValue::ContentRange(ContentRangeClass::Valid) => DerivativeValueClassV1::Success,
        SafeValue::ContentRange(ContentRangeClass::Missing) => DerivativeValueClassV1::Absent,
        SafeValue::ContentRange(ContentRangeClass::NotApplicable) => {
            DerivativeValueClassV1::Unknown
        }
        SafeValue::ContentRange(ContentRangeClass::WrongStart | ContentRangeClass::Invalid) => {
            DerivativeValueClassV1::Failed
        }
        SafeValue::Transport(TransportSource::Unknown) => DerivativeValueClassV1::Unknown,
        SafeValue::Transport(_) => DerivativeValueClassV1::Present,
        SafeValue::Timing(TimingBucket::Immediate | TimingBucket::Fast) => {
            DerivativeValueClassV1::Faster
        }
        SafeValue::Timing(TimingBucket::Moderate) => DerivativeValueClassV1::Similar,
        SafeValue::Timing(TimingBucket::Slow | TimingBucket::VerySlow | TimingBucket::TimedOut) => {
            DerivativeValueClassV1::Slower
        }
        SafeValue::Timing(TimingBucket::Unavailable) => DerivativeValueClassV1::Unknown,
        SafeValue::Cancellation(CancellationState::NotCancelled) => DerivativeValueClassV1::False,
        SafeValue::Cancellation(CancellationState::Cancelled | CancellationState::Shutdown) => {
            DerivativeValueClassV1::Cancelled
        }
        SafeValue::Cancellation(CancellationState::Superseded) => {
            DerivativeValueClassV1::Superseded
        }
        SafeValue::Terminal(TerminalOutcome::Success) => DerivativeValueClassV1::Success,
        SafeValue::Terminal(TerminalOutcome::Cancelled) => DerivativeValueClassV1::Cancelled,
        SafeValue::Terminal(TerminalOutcome::Superseded) => DerivativeValueClassV1::Superseded,
        SafeValue::Terminal(
            TerminalOutcome::Failed | TerminalOutcome::TimedOut | TerminalOutcome::Panicked,
        ) => DerivativeValueClassV1::Failed,
    }
}

const fn completeness(incomplete: bool) -> DerivativeCompletenessV1 {
    if incomplete {
        DerivativeCompletenessV1::Incomplete
    } else {
        DerivativeCompletenessV1::Complete
    }
}

fn bounded_count(value: u16) -> (u16, bool) {
    (value.min(MAX_COUNT), value > MAX_COUNT)
}

const fn map_terminal(terminal: SafeTerminalCategory) -> DerivativeTerminalV1 {
    match terminal {
        SafeTerminalCategory::Success => DerivativeTerminalV1::Success,
        SafeTerminalCategory::Failed => DerivativeTerminalV1::Failed,
        SafeTerminalCategory::Cancelled => DerivativeTerminalV1::Cancelled,
        SafeTerminalCategory::Superseded => DerivativeTerminalV1::Superseded,
        SafeTerminalCategory::TimedOut => DerivativeTerminalV1::TimedOut,
        SafeTerminalCategory::Panicked => DerivativeTerminalV1::Panicked,
    }
}

const fn map_terminal_outcome(terminal: TerminalOutcome) -> DerivativeTerminalV1 {
    match terminal {
        TerminalOutcome::Success => DerivativeTerminalV1::Success,
        TerminalOutcome::Failed => DerivativeTerminalV1::Failed,
        TerminalOutcome::Cancelled => DerivativeTerminalV1::Cancelled,
        TerminalOutcome::Superseded => DerivativeTerminalV1::Superseded,
        TerminalOutcome::TimedOut => DerivativeTerminalV1::TimedOut,
        TerminalOutcome::Panicked => DerivativeTerminalV1::Panicked,
    }
}

const fn map_http_status(status: HttpStatusClass) -> DerivativeHttpStatusClassV1 {
    match status {
        HttpStatusClass::Success => DerivativeHttpStatusClassV1::Success,
        HttpStatusClass::Unauthorized => DerivativeHttpStatusClassV1::Unauthorized,
        HttpStatusClass::Forbidden => DerivativeHttpStatusClassV1::Forbidden,
        HttpStatusClass::RateLimited => DerivativeHttpStatusClassV1::RateLimited,
        HttpStatusClass::OtherClientError => DerivativeHttpStatusClassV1::ClientError,
        HttpStatusClass::ServerError => DerivativeHttpStatusClassV1::ServerError,
        HttpStatusClass::Other => DerivativeHttpStatusClassV1::Other,
    }
}

const fn map_client(client: PlayerClientKind) -> DerivativeClientV1 {
    match client {
        PlayerClientKind::Web => DerivativeClientV1::Web,
        PlayerClientKind::WebRemix => DerivativeClientV1::WebRemix,
        PlayerClientKind::Android => DerivativeClientV1::Android,
        PlayerClientKind::Ios => DerivativeClientV1::Ios,
        PlayerClientKind::TvHtml5 => DerivativeClientV1::TvHtml5,
        PlayerClientKind::Unknown => DerivativeClientV1::Unknown,
    }
}

const fn map_version_policy(policy: ClientVersionPolicy) -> DerivativeClientVersionPolicyV1 {
    match policy {
        ClientVersionPolicy::Static => DerivativeClientVersionPolicyV1::Static,
        ClientVersionPolicy::Cached => DerivativeClientVersionPolicyV1::Cached,
        ClientVersionPolicy::Discovered => DerivativeClientVersionPolicyV1::Discovered,
        ClientVersionPolicy::Fallback => DerivativeClientVersionPolicyV1::Fallback,
        ClientVersionPolicy::Unknown => DerivativeClientVersionPolicyV1::Unknown,
    }
}

const fn map_authentication(authentication: AuthenticationKind) -> DerivativeAuthV1 {
    match authentication {
        AuthenticationKind::None => DerivativeAuthV1::None,
        AuthenticationKind::Browser => DerivativeAuthV1::Browser,
        AuthenticationKind::OAuthBearer => DerivativeAuthV1::Bearer,
        AuthenticationKind::Unavailable => DerivativeAuthV1::Unknown,
    }
}

const fn map_playability(playability: PlayabilityClass) -> DerivativePlayabilityV1 {
    match playability {
        PlayabilityClass::Playable => DerivativePlayabilityV1::Playable,
        PlayabilityClass::LoginRequired => DerivativePlayabilityV1::AuthenticationRequired,
        PlayabilityClass::AgeConsentOrRegion => DerivativePlayabilityV1::ConsentAgeOrRegion,
        PlayabilityClass::Unavailable => DerivativePlayabilityV1::ProviderUnavailable,
        PlayabilityClass::Other => DerivativePlayabilityV1::Unknown,
    }
}

const fn map_failure(failure: FailureCategory) -> DerivativeFailureCategoryV1 {
    match failure {
        FailureCategory::None => DerivativeFailureCategoryV1::None,
        FailureCategory::Authentication => DerivativeFailureCategoryV1::Authentication,
        FailureCategory::ConsentAgeOrRegion => DerivativeFailureCategoryV1::ConsentAgeOrRegion,
        FailureCategory::ProviderUnavailable => DerivativeFailureCategoryV1::ProviderUnavailable,
        FailureCategory::ProofToken => DerivativeFailureCategoryV1::ProofToken,
        FailureCategory::Decipher => DerivativeFailureCategoryV1::Decipher,
        FailureCategory::RateLimited => DerivativeFailureCategoryV1::RateLimited,
        FailureCategory::Network => DerivativeFailureCategoryV1::Network,
        FailureCategory::Contract => DerivativeFailureCategoryV1::Contract,
        FailureCategory::UnsupportedFormat => DerivativeFailureCategoryV1::UnsupportedFormat,
        FailureCategory::MediaForbidden => DerivativeFailureCategoryV1::MediaForbidden,
        FailureCategory::MediaRangeContract => DerivativeFailureCategoryV1::MediaRangeContract,
        FailureCategory::Decode => DerivativeFailureCategoryV1::Decode,
        FailureCategory::Cancelled => DerivativeFailureCategoryV1::Cancelled,
        FailureCategory::Unknown => DerivativeFailureCategoryV1::Unknown,
    }
}

const fn map_container(container: ContainerKind) -> DerivativeContainerV1 {
    match container {
        ContainerKind::Mp4 => DerivativeContainerV1::Mp4,
        ContainerKind::WebM => DerivativeContainerV1::WebM,
        ContainerKind::Other => DerivativeContainerV1::OtherKnown,
    }
}

const fn map_codec(codec: CodecKind) -> DerivativeCodecV1 {
    match codec {
        CodecKind::Aac => DerivativeCodecV1::Aac,
        CodecKind::Opus => DerivativeCodecV1::Opus,
        CodecKind::Vorbis | CodecKind::Other => DerivativeCodecV1::OtherKnown,
    }
}

const fn map_bitrate(bitrate: BitrateBucket) -> DerivativeBitrateBucketV1 {
    match bitrate {
        BitrateBucket::Low => DerivativeBitrateBucketV1::Low,
        BitrateBucket::Medium => DerivativeBitrateBucketV1::Medium,
        BitrateBucket::High => DerivativeBitrateBucketV1::High,
        BitrateBucket::Unknown => DerivativeBitrateBucketV1::Unknown,
    }
}

const fn map_response(response: ResponseClassification) -> DerivativeResponseClassV1 {
    match response {
        ResponseClassification::Playable => DerivativeResponseClassV1::Playable,
        ResponseClassification::Refused => DerivativeResponseClassV1::Refused,
        ResponseClassification::Malformed => DerivativeResponseClassV1::Malformed,
        ResponseClassification::TransportFailure => DerivativeResponseClassV1::TransportFailure,
        ResponseClassification::Cancelled => DerivativeResponseClassV1::Cancelled,
        ResponseClassification::Unknown => DerivativeResponseClassV1::Unknown,
    }
}

const fn map_content_range(range: ContentRangeClass) -> DerivativeContentRangeV1 {
    match range {
        ContentRangeClass::Valid => DerivativeContentRangeV1::Valid,
        ContentRangeClass::Missing => DerivativeContentRangeV1::Missing,
        ContentRangeClass::WrongStart => DerivativeContentRangeV1::WrongStart,
        ContentRangeClass::Invalid => DerivativeContentRangeV1::Invalid,
        ContentRangeClass::NotApplicable => DerivativeContentRangeV1::NotApplicable,
    }
}

const fn map_transport_source(source: TransportSource) -> Option<DerivativeTransportV1> {
    match source {
        TransportSource::NativeHttp => Some(DerivativeTransportV1::NativeHttp),
        TransportSource::Browser
        | TransportSource::BrowserCache
        | TransportSource::ServiceWorker => Some(DerivativeTransportV1::BrowserCdp),
        TransportSource::OfflineReplay => Some(DerivativeTransportV1::OfflineReplay),
        TransportSource::FreshReplay => Some(DerivativeTransportV1::FreshReplay),
        TransportSource::Unknown => None,
    }
}

const fn map_timing_bucket(bucket: TimingBucket) -> DerivativeTimingBucketV1 {
    match bucket {
        TimingBucket::Immediate => DerivativeTimingBucketV1::Immediate,
        TimingBucket::Fast => DerivativeTimingBucketV1::Fast,
        TimingBucket::Moderate => DerivativeTimingBucketV1::Moderate,
        TimingBucket::Slow => DerivativeTimingBucketV1::Slow,
        TimingBucket::VerySlow => DerivativeTimingBucketV1::VerySlow,
        TimingBucket::TimedOut => DerivativeTimingBucketV1::TimedOut,
        TimingBucket::Unavailable => DerivativeTimingBucketV1::Unavailable,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::developer_capture::{
        analysis::compare_captures,
        diff::{ComparisonKind, MAX_DIFF_FINDINGS},
        model::{
            CaptureRecordKind, CaptureRecordV1, CaptureRef, EndpointRole, ExchangeRef,
            IncompleteReason, ProviderClientKind, SafeOperationRef, SensitiveBytes,
        },
        payload::{encode_fields, field, PrivateField, PrivatePayloadKind},
        sanitize::{
            DerivativeFileNameV1, DerivativeFileViewV1, DerivativeForbiddenScannerV1,
            DerivativePreviewFileV1, ForbiddenScanVerdictV1,
        },
    };
    use static_assertions::assert_not_impl_any;

    assert_not_impl_any!(
        PreparedPrivateDerivativeV1:
            Clone,
            std::fmt::Debug,
            std::fmt::Display,
            serde::Serialize
    );

    #[allow(clippy::too_many_arguments)]
    fn context_record(
        sequence: u16,
        exchange: Option<ExchangeRef>,
        endpoint: EndpointRole,
        client: ProviderClientKind,
        transport: TransportKind,
        attempt: u8,
        kind: CaptureRecordKind,
        payload_kind: PrivatePayloadKind,
        fields: &[PrivateField<'_>],
    ) -> CaptureRecordV1 {
        CaptureRecordV1::with_context(
            sequence,
            u64::from(sequence) * 10,
            exchange,
            endpoint,
            client,
            transport,
            attempt,
            kind,
            encode_fields(payload_kind, fields).unwrap(),
        )
    }

    fn format_blob() -> Vec<u8> {
        let mime = b"audio/mp4; codecs=mp4a.40.2";
        let mut output = Vec::new();
        output.extend_from_slice(&140_u64.to_be_bytes());
        output.extend_from_slice(&128_000_u64.to_be_bytes());
        output.push(1);
        output.push(0);
        output.extend_from_slice(&u16::try_from(mime.len()).unwrap().to_be_bytes());
        output.extend_from_slice(mime);
        output.extend_from_slice(&0_u16.to_be_bytes());
        output.extend_from_slice(&0_u16.to_be_bytes());
        output
    }

    fn terminal_record(sequence: u16, terminal: SafeTerminalCategory) -> CaptureRecordV1 {
        let outcome = match terminal {
            SafeTerminalCategory::Success => "success",
            SafeTerminalCategory::Failed => "failed",
            SafeTerminalCategory::Cancelled => "cancelled",
            SafeTerminalCategory::Superseded => "superseded",
            SafeTerminalCategory::TimedOut => "timed_out",
            SafeTerminalCategory::Panicked => "panicked",
        };
        context_record(
            sequence,
            None,
            EndpointRole::Operation,
            ProviderClientKind::Unknown,
            TransportKind::Unknown,
            0,
            CaptureRecordKind::TerminalOutcome,
            PrivatePayloadKind::TerminalOutcome,
            &[PrivateField::text(field::OUTCOME, outcome)],
        )
    }

    fn player_records(
        exchange: ExchangeRef,
        status: u64,
        playability: &str,
        category: &str,
        selected: bool,
        private_url: &str,
    ) -> Vec<CaptureRecordV1> {
        let formats = format_blob();
        let selection = if selected {
            "native_selected"
        } else {
            "provider_unavailable"
        };
        let selected_itag = u64::from(selected) * 140;
        vec![
            context_record(
                0,
                Some(exchange),
                EndpointRole::PlayerApi,
                ProviderClientKind::Android,
                TransportKind::NativeHttp,
                1,
                CaptureRecordKind::HttpRequest,
                PrivatePayloadKind::HttpRequest,
                &[
                    PrivateField::text(field::METHOD, "POST"),
                    PrivateField::text(field::URL, private_url),
                ],
            ),
            context_record(
                1,
                Some(exchange),
                EndpointRole::PlayerApi,
                ProviderClientKind::Android,
                TransportKind::NativeHttp,
                0,
                CaptureRecordKind::AuthSelection,
                PrivatePayloadKind::AuthSelection,
                &[
                    PrivateField::text(field::AUTH_KIND, "browser"),
                    PrivateField::text(field::VERSION_SOURCE, "discovered"),
                    PrivateField::boolean(field::PROOF_TOKEN_PRESENT, true),
                ],
            ),
            context_record(
                2,
                Some(exchange),
                EndpointRole::PlayerApi,
                ProviderClientKind::Android,
                TransportKind::NativeHttp,
                1,
                CaptureRecordKind::HttpResponse,
                PrivatePayloadKind::HttpResponse,
                &[
                    PrivateField::u64(field::STATUS, status),
                    PrivateField::boolean(field::REDIRECTED, false),
                    PrivateField::u64(field::ELAPSED_MS, 35),
                ],
            ),
            context_record(
                3,
                Some(exchange),
                EndpointRole::PlayerApi,
                ProviderClientKind::Android,
                TransportKind::NativeHttp,
                1,
                CaptureRecordKind::PlayerParse,
                PrivatePayloadKind::PlayerParse,
                &[
                    PrivateField::text(field::PLAYABILITY_STATUS, playability),
                    PrivateField::text(field::CATEGORY, category),
                    PrivateField::boolean(field::STREAMING_DATA_PRESENT, selected),
                    PrivateField::u64(field::ELAPSED_MS, 2),
                ],
            ),
            context_record(
                4,
                Some(exchange),
                EndpointRole::PlayerApi,
                ProviderClientKind::Android,
                TransportKind::NativeHttp,
                1,
                CaptureRecordKind::FormatInventory,
                PrivatePayloadKind::FormatInventory,
                &[
                    PrivateField::u64(field::RETURNED_FORMATS, u64::from(selected)),
                    PrivateField::u64(field::SUPPORTED_FORMATS, u64::from(selected)),
                    PrivateField::u64(field::DIRECT_FORMATS, u64::from(selected)),
                    PrivateField::u64(field::CIPHER_FORMATS, 0),
                    PrivateField::bytes(
                        field::FORMAT_FACTS,
                        if selected { formats.as_slice() } else { &[] },
                    ),
                ],
            ),
            context_record(
                5,
                Some(exchange),
                EndpointRole::PlayerApi,
                ProviderClientKind::Android,
                TransportKind::NativeHttp,
                1,
                CaptureRecordKind::SelectionDecision,
                PrivatePayloadKind::SelectionDecision,
                &[
                    PrivateField::text(field::OUTCOME, selection),
                    PrivateField::boolean(field::FALLBACK_ELIGIBLE, false),
                    PrivateField::boolean(field::FALLBACK_ATTEMPTED, false),
                    PrivateField::u64(field::SELECTED_ITAG, selected_itag),
                    PrivateField::u64(field::ELAPSED_MS, 3),
                ],
            ),
        ]
    }

    fn capture(
        capture_byte: u8,
        purpose: CapturePurpose,
        mut records: Vec<CaptureRecordV1>,
        terminal: SafeTerminalCategory,
        complete: bool,
    ) -> PrivateCaptureV1 {
        if purpose == CapturePurpose::InteractivePlayback {
            records.push(terminal_record(
                u16::try_from(records.len()).unwrap(),
                terminal,
            ));
        }
        PrivateCaptureV1::new(
            CaptureRef::from_bytes([capture_byte; 16]),
            1,
            2,
            purpose,
            SafeOperationRef::from_bytes([capture_byte; 4]),
            records,
            if complete {
                CaptureCompleteness::Complete
            } else {
                CaptureCompleteness::Incomplete
            },
            if complete {
                Vec::new()
            } else {
                vec![IncompleteReason::Truncated]
            },
            0,
            terminal,
        )
    }

    fn working_capture(capture_byte: u8, private_url: &str) -> PrivateCaptureV1 {
        capture(
            capture_byte,
            CapturePurpose::InteractivePlayback,
            player_records(
                ExchangeRef::from_bytes([capture_byte; 8]),
                200,
                "OK",
                "playable",
                true,
                private_url,
            ),
            SafeTerminalCategory::Success,
            true,
        )
    }

    #[test]
    fn real_capture_projects_only_registered_finite_vocabulary() {
        let capture = working_capture(0x11, "https://private.invalid/player?id=secret");
        let derivative = build_derivative_from_capture(&capture).unwrap();

        assert_eq!(
            derivative.operation(),
            DerivativeOperationV1::InteractivePlayback
        );
        assert_eq!(derivative.terminal(), DerivativeTerminalV1::Success);
        assert_eq!(
            derivative.completeness(),
            DerivativeCompletenessV1::Complete
        );
        assert_eq!(
            derivative.transports(),
            &[DerivativeTransportV1::NativeHttp]
        );
        for expected in [
            DerivativeFactV1::Client(DerivativeClientV1::Android),
            DerivativeFactV1::Authentication(DerivativeAuthV1::Browser),
            DerivativeFactV1::Playability(DerivativePlayabilityV1::Playable),
            DerivativeFactV1::SelectedFormatPresent(true),
            DerivativeFactV1::SelectedContainer(DerivativeContainerV1::Mp4),
            DerivativeFactV1::SelectedCodec(DerivativeCodecV1::Aac),
            DerivativeFactV1::MediaResult(DerivativeMediaResultV1::NotReached),
            DerivativeFactV1::ReplayResult(DerivativeReplayResultV1::NotRun),
        ] {
            assert!(
                derivative.facts().contains(&expected),
                "missing {expected:?}"
            );
        }
        assert_eq!(derivative.timings().len(), 4);
        assert!(derivative.findings().is_empty());
    }

    #[test]
    fn prepared_capture_is_deterministic_and_excludes_private_values() {
        const PRIVATE_URL: &str = "https://private.invalid/player?id=seeded-canary";
        let capture = working_capture(0x22, PRIVATE_URL);
        let first = prepare_derivative_from_capture(&capture).unwrap();
        let second = prepare_derivative_from_capture(&capture).unwrap();
        assert_eq!(first.preview(), second.preview());
        assert!(first.review().checksum_valid());
        assert!(first.review().forbidden_scan_passed());

        let output = first
            .preview()
            .files()
            .iter()
            .map(DerivativePreviewFileV1::contents)
            .collect::<String>();
        assert!(!output.contains(PRIVATE_URL));
        assert!(!output.contains("seeded-canary"));

        let injected = DerivativeFileViewV1::new_for_store_test(
            DerivativeFileNameV1::Evidence,
            PRIVATE_URL.as_bytes(),
        );
        assert!(matches!(
            first.scanner.scan(&[injected]).unwrap(),
            ForbiddenScanVerdictV1::Forbidden { .. }
        ));
    }

    #[test]
    fn malformed_or_partial_capture_is_bounded_and_marked_incomplete() {
        let record = CaptureRecordV1::with_context(
            7,
            0,
            None,
            EndpointRole::Operation,
            ProviderClientKind::Unknown,
            TransportKind::Unknown,
            0,
            CaptureRecordKind::OperationBoundary,
            SensitiveBytes::new(b"malformed-private-record".to_vec()),
        );
        let capture = capture(
            0x33,
            CapturePurpose::InteractivePlayback,
            vec![record],
            SafeTerminalCategory::Failed,
            false,
        );
        let derivative = build_derivative_from_capture(&capture).unwrap();
        assert_eq!(
            derivative.completeness(),
            DerivativeCompletenessV1::Incomplete
        );
        assert!(derivative.fact_count() <= 48);
        assert!(derivative.timings().len() <= 16);
    }

    #[test]
    fn incompatible_capture_purposes_and_mixed_replay_transports_are_rejected() {
        let prefetch = capture(
            0x44,
            CapturePurpose::Prefetch,
            Vec::new(),
            SafeTerminalCategory::Failed,
            false,
        );
        assert_eq!(
            build_derivative_from_capture(&prefetch).unwrap_err(),
            DerivativeBuildError::IncompatibleCapture
        );

        let mixed = capture(
            0x45,
            CapturePurpose::Replay,
            vec![
                CaptureRecordV1::with_context(
                    0,
                    0,
                    None,
                    EndpointRole::Operation,
                    ProviderClientKind::Unknown,
                    TransportKind::OfflineReplay,
                    0,
                    CaptureRecordKind::OperationBoundary,
                    SensitiveBytes::new(b"private-offline".to_vec()),
                ),
                CaptureRecordV1::with_context(
                    1,
                    1,
                    None,
                    EndpointRole::Operation,
                    ProviderClientKind::Unknown,
                    TransportKind::FreshReplay,
                    0,
                    CaptureRecordKind::TerminalOutcome,
                    SensitiveBytes::new(b"private-fresh".to_vec()),
                ),
            ],
            SafeTerminalCategory::Failed,
            false,
        );
        assert_eq!(
            build_derivative_from_capture(&mixed).unwrap_err(),
            DerivativeBuildError::IncompatibleCapture
        );
    }

    #[test]
    fn real_comparison_projects_registered_findings_without_private_sources() {
        const LEFT_URL: &str = "https://private.invalid/working?secret=left";
        const RIGHT_URL: &str = "https://private.invalid/failing?secret=right";
        let left = working_capture(0x55, LEFT_URL);
        let right = capture(
            0x66,
            CapturePurpose::InteractivePlayback,
            player_records(
                ExchangeRef::from_bytes([0x66; 8]),
                403,
                "UNPLAYABLE",
                "provider_unavailable",
                false,
                RIGHT_URL,
            ),
            SafeTerminalCategory::Failed,
            true,
        );
        let comparison = compare_captures(ComparisonKind::WorkingVsFailing, &left, &right);
        let derivative = build_derivative_from_comparison(&comparison).unwrap();
        assert_eq!(derivative.operation(), DerivativeOperationV1::Comparison);
        assert!(derivative.finding_count() > 0);
        assert!(derivative.finding_count() <= MAX_DIFF_FINDINGS);

        let prepared = prepare_derivative_from_comparison(&comparison, &left, &right).unwrap();
        let output = prepared
            .preview()
            .files()
            .iter()
            .map(DerivativePreviewFileV1::contents)
            .collect::<String>();
        for private in [LEFT_URL, RIGHT_URL, "secret=left", "secret=right"] {
            assert!(!output.contains(private));
        }
    }

    #[test]
    fn comparison_sources_are_bound_to_the_supplied_report() {
        let left = working_capture(0x71, "https://private.invalid/left");
        let right = working_capture(0x72, "https://private.invalid/right");
        let unrelated = working_capture(0x73, "https://private.invalid/unrelated");
        let comparison = compare_captures(ComparisonKind::WorkingVsFailing, &left, &right);
        assert!(matches!(
            prepare_derivative_from_comparison(&comparison, &left, &unrelated),
            Err(DerivativeBuildError::IncompatibleComparison)
        ));
    }

    #[test]
    fn comparison_projection_retains_at_most_sixty_four_findings() {
        let template = SafeDiffFinding {
            category: DiffCategory::ProviderResponse,
            severity: DiffSeverity::Critical,
            field: RegisteredField::PlayerHttpStatus,
            left_class: ValueClass::HttpStatus,
            right_class: ValueClass::HttpStatus,
            left: SafeValue::HttpStatus(HttpStatusClass::Success),
            right: SafeValue::HttpStatus(HttpStatusClass::Forbidden),
            explanation: FixedExplanation::PlayerHttpStatusChanged,
        };
        let source = vec![template; MAX_DERIVATIVE_FINDINGS + 7];
        let (projected, incomplete) = project_findings(&source).unwrap();
        assert_eq!(projected.len(), MAX_DERIVATIVE_FINDINGS);
        assert!(incomplete);
    }
}
