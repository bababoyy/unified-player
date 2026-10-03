//! Bounded semantic extraction for encrypted provider captures.
//!
//! This module is the only Phase 7E bridge from private record payloads to the
//! comparison vocabulary. Raw strings and bytes are consumed here and are never
//! retained by the safe comparison projection.

use std::collections::BTreeSet;

use super::{
    diff::{
        compare, AuthenticationKind, BitrateBucket, BoundedCount, CancellationState,
        ClientVersionPolicy, CodecKind, ComparisonFact, ComparisonKind, ComparisonReportV1,
        ContainerKind, ContentRangeClass, DiffCategory, FailureCategory, FallbackOutcome,
        HttpStatusCode, PlayabilityClass, PlayerClientKind, PrivateFormatId, RedirectClass,
        RegisteredField, ResponseClassification, SafeDiffFinding, SemanticFact, SemanticSnapshot,
        TerminalOutcome, TimingBucket, TransportSource, UnknownPrivateFact, MAX_DIFF_FINDINGS,
    },
    model::{
        CaptureCompleteness, CapturePurpose, CaptureRecordKind, CaptureRecordV1, CaptureRef,
        EndpointRole, ExchangeRef, IncompleteReason, PrivateCaptureV1, ProviderClientKind,
        SafeTerminalCategory, SensitiveBytes, TransportKind,
    },
    payload::field,
    replay::{
        ReplayDecision, ReplayParserOutcome, ReplayProviderOutcome, ReplayRecipeV1,
        ReplaySelectionOutcome,
    },
};

const PAYLOAD_MAGIC: &[u8; 8] = b"SPPEV1\0\0";
const PAYLOAD_SCHEMA_VERSION: u16 = 1;
const PAYLOAD_HEADER_BYTES: usize = 13;
const MAX_PAYLOAD_FIELDS: usize = 128;
const MAX_PLAYER_FLOWS: usize = 32;
const MAX_PARSED_FORMATS: usize = 128;
const MAX_UNKNOWN_FACTS: usize = 64;
const REPLAY_RESULT_MAGIC: &[u8; 8] = b"SPREPV1\0";
const REPLAY_RESULT_SCHEMA_VERSION: u16 = 1;

/// Private semantic state derived from one validated vault artifact.
///
/// Deliberately has no general formatting or serialization implementation.
pub(crate) struct CaptureSemantics {
    snapshot: SemanticSnapshot,
    incomplete: bool,
    malformed_records: u16,
}

impl CaptureSemantics {
    pub(crate) const fn incomplete(&self) -> bool {
        self.incomplete
    }

    pub(crate) const fn malformed_records(&self) -> u16 {
        self.malformed_records
    }

    pub(super) fn registered_facts(&self) -> impl Iterator<Item = &SemanticFact> {
        self.snapshot.registered_facts()
    }

    pub(super) fn has_unknown_private(&self) -> bool {
        self.snapshot.has_unknown_private()
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct SafeComparisonSummaryV1 {
    schema_version: u16,
    kind: ComparisonKind,
    findings: Vec<SafeDiffFinding>,
    categories: Vec<DiffCategory>,
    incomplete: bool,
    dropped_findings: u16,
    malformed_records: u16,
}

impl SafeComparisonSummaryV1 {
    pub(crate) const fn schema_version(&self) -> u16 {
        self.schema_version
    }

    pub(crate) const fn kind(&self) -> ComparisonKind {
        self.kind
    }

    pub(crate) fn findings(&self) -> &[SafeDiffFinding] {
        &self.findings
    }

    pub(crate) fn categories(&self) -> &[DiffCategory] {
        &self.categories
    }

    pub(crate) const fn incomplete(&self) -> bool {
        self.incomplete
    }

    pub(crate) const fn dropped_findings(&self) -> u16 {
        self.dropped_findings
    }

    pub(crate) const fn malformed_records(&self) -> u16 {
        self.malformed_records
    }
}

/// A private report paired with the only value that may cross into diagnostics.
///
/// Deliberately has no `Debug`, `Display`, or serialization implementation.
pub(crate) struct CaptureComparisonV1 {
    report: ComparisonReportV1,
    safe: SafeComparisonSummaryV1,
    source_capture_refs: [CaptureRef; 2],
}

impl CaptureComparisonV1 {
    pub(super) const fn private_report(&self) -> &ComparisonReportV1 {
        &self.report
    }

    pub(crate) const fn safe_summary(&self) -> &SafeComparisonSummaryV1 {
        &self.safe
    }

    pub(super) fn matches_sources(
        &self,
        left: &PrivateCaptureV1,
        right: &PrivateCaptureV1,
    ) -> bool {
        self.source_capture_refs == [left.capture_ref(), right.capture_ref()]
    }
}

pub(crate) fn compare_captures(
    kind: ComparisonKind,
    left: &PrivateCaptureV1,
    right: &PrivateCaptureV1,
) -> CaptureComparisonV1 {
    let left_capture_ref = left.capture_ref();
    let right_capture_ref = right.capture_ref();
    let (left, right) = match kind {
        ComparisonKind::WorkingVsFailing => (
            extract_capture_semantics(left),
            extract_capture_semantics(right),
        ),
        ComparisonKind::OriginalVsReplay => (
            extract_replay_source_semantics(left),
            extract_replay_child_semantics(right),
        ),
    };
    let report = compare(kind, &left.snapshot, &right.snapshot);
    let findings = report
        .safe_projection()
        .iter()
        .take(MAX_DIFF_FINDINGS)
        .cloned()
        .collect::<Vec<_>>();
    let categories = findings
        .iter()
        .map(|finding| finding.category)
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect();
    let safe = SafeComparisonSummaryV1 {
        schema_version: report.schema_version(),
        kind,
        findings,
        categories,
        incomplete: left.incomplete || right.incomplete || report.incomplete(),
        dropped_findings: report.dropped_findings(),
        malformed_records: left
            .malformed_records
            .saturating_add(right.malformed_records),
    };
    CaptureComparisonV1 {
        report,
        safe,
        source_capture_refs: [left_capture_ref, right_capture_ref],
    }
}

pub(super) fn extract_derivative_semantics(capture: &PrivateCaptureV1) -> CaptureSemantics {
    if capture.purpose() == CapturePurpose::Replay {
        extract_replay_child_semantics(capture)
    } else {
        extract_capture_semantics(capture)
    }
}

fn extract_replay_source_semantics(capture: &PrivateCaptureV1) -> CaptureSemantics {
    let incomplete =
        capture.completeness != CaptureCompleteness::Complete || capture.dropped_records != 0;
    if capture.purpose == CapturePurpose::Replay {
        return CaptureSemantics {
            snapshot: SemanticSnapshot::from_registered([]),
            incomplete: true,
            malformed_records: 1,
        };
    }

    match ReplayRecipeV1::from_private_capture(capture) {
        Ok(recipe) => CaptureSemantics {
            snapshot: replay_decision_snapshot(recipe.expected_decision()),
            incomplete,
            malformed_records: 0,
        },
        Err(_) => CaptureSemantics {
            snapshot: SemanticSnapshot::from_registered([]),
            incomplete: true,
            malformed_records: 0,
        },
    }
}

fn extract_replay_child_semantics(capture: &PrivateCaptureV1) -> CaptureSemantics {
    let mut incomplete =
        capture.completeness != CaptureCompleteness::Complete || capture.dropped_records != 0;
    let mut malformed_records = 0_u16;
    if capture.purpose != CapturePurpose::Replay {
        malformed_records = malformed_records.saturating_add(1);
        incomplete = true;
    }

    let mut result = None;
    let mut terminal = None;
    let mut previous_offset = None;
    for (index, record) in capture.records.iter().enumerate() {
        if usize::from(record.sequence) != index
            || previous_offset.is_some_and(|offset| record.monotonic_offset_ms < offset)
        {
            malformed_records = malformed_records.saturating_add(1);
            incomplete = true;
        }
        previous_offset = Some(record.monotonic_offset_ms);

        match record.kind {
            CaptureRecordKind::OperationBoundary
                if record.endpoint_role == EndpointRole::Operation && result.is_none() =>
            {
                match decode_replay_result(record.payload.expose()) {
                    Ok(decoded)
                        if record.exchange_ref.map(ExchangeRef::bytes)
                            == Some(decoded.source_exchange_ref)
                            && replay_transport(decoded.mode) == record.transport_kind =>
                    {
                        result = Some(decoded);
                    }
                    Ok(_) | Err(_) => {
                        malformed_records = malformed_records.saturating_add(1);
                        incomplete = true;
                    }
                }
            }
            CaptureRecordKind::TerminalOutcome
                if record.endpoint_role == EndpointRole::Operation && terminal.is_none() =>
            {
                let decoded = PayloadView::decode(record.payload.expose()).and_then(|view| {
                    if view.kind != expected_payload_kind(CaptureRecordKind::TerminalOutcome) {
                        return Err(ExtractionError);
                    }
                    terminal_from_bytes(view.required_bytes(field::OUTCOME, ValueKind::Text)?)
                });
                if let Ok(outcome) = decoded {
                    terminal = Some((outcome, record.transport_kind));
                } else {
                    malformed_records = malformed_records.saturating_add(1);
                    incomplete = true;
                }
            }
            _ => {
                malformed_records = malformed_records.saturating_add(1);
                incomplete = true;
            }
        }
    }

    let decision = match (result, terminal) {
        (Some(result), Some((terminal, terminal_transport)))
            if terminal == terminal_outcome(capture.terminal_category)
                && terminal == terminal_outcome(result.terminal_category)
                && terminal_transport == replay_transport(result.mode) =>
        {
            result.decision
        }
        _ => {
            malformed_records = malformed_records.saturating_add(1);
            incomplete = true;
            None
        }
    };
    if decision.is_none() {
        incomplete = true;
    }

    CaptureSemantics {
        snapshot: decision.map_or_else(
            || SemanticSnapshot::from_registered([]),
            replay_decision_snapshot,
        ),
        incomplete,
        malformed_records,
    }
}

fn replay_decision_snapshot(decision: ReplayDecision) -> SemanticSnapshot {
    let mut facts = Vec::new();
    let (playability, failure, response) = replay_provider_classification(decision.provider);
    match decision.parser {
        ReplayParserOutcome::NotRun => {
            facts.push(SemanticFact::SafeFailureCategory(failure));
            facts.push(SemanticFact::NativeResponseClass(response));
        }
        ReplayParserOutcome::Malformed => {
            facts.push(SemanticFact::PlayabilityStatus(PlayabilityClass::Other));
            facts.push(SemanticFact::SafeFailureCategory(FailureCategory::Contract));
            facts.push(SemanticFact::StreamingDataPresent(false));
            facts.push(SemanticFact::NativeResponseClass(
                ResponseClassification::Malformed,
            ));
        }
        ReplayParserOutcome::Parsed => {
            facts.push(SemanticFact::PlayabilityStatus(playability));
            facts.push(SemanticFact::SafeFailureCategory(failure));
            facts.push(SemanticFact::StreamingDataPresent(
                decision.streaming_data_present,
            ));
            facts.push(SemanticFact::NativeResponseClass(response));
            facts.push(SemanticFact::ReturnedFormatCount(BoundedCount::new(
                usize::from(decision.returned_formats),
            )));
            facts.push(SemanticFact::SupportedFormatCount(BoundedCount::new(
                usize::from(decision.supported_formats),
            )));
            facts.push(SemanticFact::DirectFormatCount(BoundedCount::new(
                usize::from(decision.direct_formats),
            )));
            facts.push(SemanticFact::CipherFormatCount(BoundedCount::new(
                usize::from(decision.cipher_formats),
            )));
            match decision.selection {
                ReplaySelectionOutcome::NotAttempted => {}
                ReplaySelectionOutcome::Selected { itag } => {
                    facts.push(SemanticFact::SelectedFormat(Some(PrivateFormatId::new(
                        itag,
                    ))));
                }
                ReplaySelectionOutcome::CipherOnly => {
                    facts.push(SemanticFact::SelectedFormat(None));
                    facts.push(SemanticFact::SafeFailureCategory(FailureCategory::Decipher));
                }
                ReplaySelectionOutcome::UnsupportedFormat => {
                    facts.push(SemanticFact::SelectedFormat(None));
                    facts.push(SemanticFact::SafeFailureCategory(
                        FailureCategory::UnsupportedFormat,
                    ));
                }
                ReplaySelectionOutcome::NoDirectFormat => {
                    facts.push(SemanticFact::SelectedFormat(None));
                    facts.push(SemanticFact::SafeFailureCategory(
                        FailureCategory::ProviderUnavailable,
                    ));
                }
            }
        }
    }
    SemanticSnapshot::from_registered(facts)
}

const fn replay_provider_classification(
    provider: ReplayProviderOutcome,
) -> (PlayabilityClass, FailureCategory, ResponseClassification) {
    match provider {
        ReplayProviderOutcome::Playable => (
            PlayabilityClass::Playable,
            FailureCategory::None,
            ResponseClassification::Playable,
        ),
        ReplayProviderOutcome::Authentication => (
            PlayabilityClass::LoginRequired,
            FailureCategory::Authentication,
            ResponseClassification::Refused,
        ),
        ReplayProviderOutcome::ConsentAgeRegion => (
            PlayabilityClass::AgeConsentOrRegion,
            FailureCategory::ConsentAgeOrRegion,
            ResponseClassification::Refused,
        ),
        ReplayProviderOutcome::ProviderUnavailable => (
            PlayabilityClass::Unavailable,
            FailureCategory::ProviderUnavailable,
            ResponseClassification::Refused,
        ),
        ReplayProviderOutcome::ProofToken => (
            PlayabilityClass::Other,
            FailureCategory::ProofToken,
            ResponseClassification::Refused,
        ),
        ReplayProviderOutcome::RateLimited => (
            PlayabilityClass::Other,
            FailureCategory::RateLimited,
            ResponseClassification::Refused,
        ),
        ReplayProviderOutcome::NetworkFailed => (
            PlayabilityClass::Other,
            FailureCategory::Network,
            ResponseClassification::TransportFailure,
        ),
        ReplayProviderOutcome::Contract => (
            PlayabilityClass::Other,
            FailureCategory::Contract,
            ResponseClassification::Malformed,
        ),
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum ReplayResultMode {
    Offline,
    Fresh,
}

struct ReplayResultEvidence {
    mode: ReplayResultMode,
    source_exchange_ref: [u8; 8],
    terminal_category: SafeTerminalCategory,
    decision: Option<ReplayDecision>,
}

fn decode_replay_result(bytes: &[u8]) -> Result<ReplayResultEvidence, ExtractionError> {
    let mut cursor = 0_usize;
    if take(bytes, &mut cursor, REPLAY_RESULT_MAGIC.len())? != REPLAY_RESULT_MAGIC
        || take_u16(bytes, &mut cursor)? != REPLAY_RESULT_SCHEMA_VERSION
    {
        return Err(ExtractionError);
    }
    let mode = match take(bytes, &mut cursor, 1)? {
        [1] => ReplayResultMode::Offline,
        [2] => ReplayResultMode::Fresh,
        _ => return Err(ExtractionError),
    };
    take(bytes, &mut cursor, 16)?;
    let source_exchange_ref = take(bytes, &mut cursor, 8)?
        .try_into()
        .map_err(|_| ExtractionError)?;
    let outcome = take(bytes, &mut cursor, 1)?[0];
    let decision_present = match take(bytes, &mut cursor, 1)? {
        [0] => false,
        [1] => true,
        _ => return Err(ExtractionError),
    };
    let (terminal_category, outcome_has_decision) = replay_outcome(mode, outcome)?;
    if decision_present != outcome_has_decision {
        return Err(ExtractionError);
    }
    let decision = decision_present
        .then(|| decode_replay_decision(bytes, &mut cursor))
        .transpose()?;
    if cursor != bytes.len() {
        return Err(ExtractionError);
    }
    Ok(ReplayResultEvidence {
        mode,
        source_exchange_ref,
        terminal_category,
        decision,
    })
}

fn decode_replay_decision(
    bytes: &[u8],
    cursor: &mut usize,
) -> Result<ReplayDecision, ExtractionError> {
    let parser = match take(bytes, cursor, 1)? {
        [0] => ReplayParserOutcome::NotRun,
        [1] => ReplayParserOutcome::Parsed,
        [2] => ReplayParserOutcome::Malformed,
        _ => return Err(ExtractionError),
    };
    let provider = match take(bytes, cursor, 1)? {
        [0] => ReplayProviderOutcome::Playable,
        [1] => ReplayProviderOutcome::Authentication,
        [2] => ReplayProviderOutcome::ConsentAgeRegion,
        [3] => ReplayProviderOutcome::ProviderUnavailable,
        [4] => ReplayProviderOutcome::ProofToken,
        [5] => ReplayProviderOutcome::RateLimited,
        [6] => ReplayProviderOutcome::NetworkFailed,
        [7] => ReplayProviderOutcome::Contract,
        _ => return Err(ExtractionError),
    };
    let streaming_data_present = match take(bytes, cursor, 1)? {
        [0] => false,
        [1] => true,
        _ => return Err(ExtractionError),
    };
    let returned_formats = take_u16(bytes, cursor)?;
    let supported_formats = take_u16(bytes, cursor)?;
    let direct_formats = take_u16(bytes, cursor)?;
    let cipher_formats = take_u16(bytes, cursor)?;
    let selection_tag = take(bytes, cursor, 1)?[0];
    let itag = take_u64(bytes, cursor)?;
    let selection = match (selection_tag, itag) {
        (0, 0) => ReplaySelectionOutcome::NotAttempted,
        (1, itag) if itag != 0 => ReplaySelectionOutcome::Selected { itag },
        (2, 0) => ReplaySelectionOutcome::CipherOnly,
        (3, 0) => ReplaySelectionOutcome::UnsupportedFormat,
        (4, 0) => ReplaySelectionOutcome::NoDirectFormat,
        _ => return Err(ExtractionError),
    };
    let decision = ReplayDecision {
        parser,
        provider,
        streaming_data_present,
        returned_formats,
        supported_formats,
        direct_formats,
        cipher_formats,
        selection,
    };
    validate_replay_decision(decision)?;
    Ok(decision)
}

fn validate_replay_decision(decision: ReplayDecision) -> Result<(), ExtractionError> {
    if decision.supported_formats > decision.returned_formats
        || decision.direct_formats > decision.returned_formats
        || decision.cipher_formats > decision.returned_formats
    {
        return Err(ExtractionError);
    }
    match decision.parser {
        ReplayParserOutcome::Malformed => {
            if decision.provider != ReplayProviderOutcome::Contract
                || decision.streaming_data_present
                || decision.returned_formats != 0
                || decision.selection != ReplaySelectionOutcome::NotAttempted
            {
                return Err(ExtractionError);
            }
        }
        ReplayParserOutcome::NotRun => {
            if decision.streaming_data_present
                || decision.returned_formats != 0
                || decision.selection != ReplaySelectionOutcome::NotAttempted
                || decision.provider == ReplayProviderOutcome::Playable
            {
                return Err(ExtractionError);
            }
        }
        ReplayParserOutcome::Parsed => {
            if decision.provider == ReplayProviderOutcome::Playable {
                match decision.selection {
                    ReplaySelectionOutcome::Selected { itag }
                        if itag != 0 && decision.direct_formats != 0 => {}
                    ReplaySelectionOutcome::CipherOnly if decision.cipher_formats != 0 => {}
                    ReplaySelectionOutcome::UnsupportedFormat
                        if decision.supported_formats == 0 => {}
                    ReplaySelectionOutcome::NoDirectFormat if decision.supported_formats != 0 => {}
                    _ => return Err(ExtractionError),
                }
                if !decision.streaming_data_present {
                    return Err(ExtractionError);
                }
            } else if decision.selection != ReplaySelectionOutcome::NotAttempted {
                return Err(ExtractionError);
            }
        }
    }
    Ok(())
}

const fn replay_outcome(
    mode: ReplayResultMode,
    outcome: u8,
) -> Result<(SafeTerminalCategory, bool), ExtractionError> {
    match (mode, outcome) {
        (ReplayResultMode::Offline, 1 | 2) | (ReplayResultMode::Fresh, 16 | 17) => {
            Ok((SafeTerminalCategory::Success, true))
        }
        (ReplayResultMode::Offline, 3 | 4) | (ReplayResultMode::Fresh, 18..=21 | 25 | 26) => {
            Ok((SafeTerminalCategory::Failed, false))
        }
        (ReplayResultMode::Offline, 5) | (ReplayResultMode::Fresh, 24) => {
            Ok((SafeTerminalCategory::Panicked, false))
        }
        (ReplayResultMode::Fresh, 22) => Ok((SafeTerminalCategory::Cancelled, false)),
        (ReplayResultMode::Fresh, 23) => Ok((SafeTerminalCategory::TimedOut, false)),
        _ => Err(ExtractionError),
    }
}

const fn replay_transport(mode: ReplayResultMode) -> TransportKind {
    match mode {
        ReplayResultMode::Offline => TransportKind::OfflineReplay,
        ReplayResultMode::Fresh => TransportKind::FreshReplay,
    }
}

pub(crate) fn extract_capture_semantics(capture: &PrivateCaptureV1) -> CaptureSemantics {
    let mut state = ExtractionState {
        incomplete: capture.completeness != CaptureCompleteness::Complete
            || capture.dropped_records != 0,
        ..ExtractionState::default()
    };

    let mut previous_offset = None;
    for (index, record) in capture.records.iter().enumerate() {
        if usize::from(record.sequence) != index
            || previous_offset.is_some_and(|offset| record.monotonic_offset_ms < offset)
        {
            state.incomplete = true;
        }
        previous_offset = Some(record.monotonic_offset_ms);
        state.observe_record(record);
    }
    let terminal_records = capture
        .records
        .iter()
        .filter(|record| record.kind == CaptureRecordKind::TerminalOutcome)
        .collect::<Vec<_>>();
    let terminal_valid = match terminal_records.as_slice() {
        [record] => {
            PayloadView::decode(record.payload.expose()).and_then(|view| {
                terminal_from_bytes(view.required_bytes(field::OUTCOME, ValueKind::Text)?)
            }) == Ok(terminal_outcome(capture.terminal_category))
        }
        _ => false,
    };
    if !terminal_valid {
        state.note_malformed();
    }

    let mut facts = Vec::<ComparisonFact>::new();
    let (selected_exchange, browser_selected) =
        if let Some(flow) = authoritative_flow(&state.native_player) {
            flow.append_native_facts(&mut facts, &mut state.incomplete);
            (
                Some(flow.exchange_ref),
                flow.selection
                    .as_ref()
                    .is_some_and(|selection| selection.value.browser_selected),
            )
        } else {
            state.incomplete = true;
            (None, false)
        };
    let browser_flow = selected_exchange
        .and_then(|exchange_ref| flow_for_exchange(&state.browser_player, exchange_ref))
        .or_else(|| authoritative_flow(&state.browser_player));
    if let Some(flow) = browser_flow {
        flow.append_browser_facts(&mut facts, browser_selected);
    } else if browser_selected {
        state.incomplete = true;
    }
    let native_media = selected_exchange
        .and_then(|exchange_ref| media_for_exchange(&state.native_media, exchange_ref));
    let browser_media = selected_exchange
        .and_then(|exchange_ref| media_for_exchange(&state.browser_media, exchange_ref));
    let media = if browser_selected {
        browser_media.or(native_media)
    } else {
        native_media.or(browser_media)
    };
    if let Some(media) = media {
        media.append_facts(&mut facts);
    }
    if let Some(decoder) = selected_exchange.and_then(|exchange_ref| {
        decoder_for_exchange(&state.decoder, exchange_ref).and_then(|flow| flow.evidence.as_ref())
    }) {
        decoder.append_facts(&mut facts);
    }

    facts.extend(
        state
            .transport_sources
            .into_iter()
            .map(|source| ComparisonFact::Registered(SemanticFact::TransportSource(source))),
    );
    facts.extend(state.unknown_private);

    let terminal = terminal_outcome(capture.terminal_category);
    facts.push(ComparisonFact::Registered(SemanticFact::TerminalOutcome(
        terminal,
    )));
    facts.push(ComparisonFact::Registered(SemanticFact::CancellationState(
        cancellation_state(capture),
    )));
    let terminal_offset = capture
        .records
        .iter()
        .rev()
        .find(|record| record.kind == CaptureRecordKind::TerminalOutcome)
        .map_or(0, |record| record.monotonic_offset_ms);
    facts.push(ComparisonFact::Registered(SemanticFact::TerminalTiming(
        timing_bucket(terminal_offset),
    )));

    let snapshot = SemanticSnapshot::from_facts(facts);
    let incomplete = state.incomplete || snapshot.input_incomplete();
    CaptureSemantics {
        snapshot,
        incomplete,
        malformed_records: state.malformed_records,
    }
}

#[derive(Default)]
struct ExtractionState {
    native_player: Vec<PlayerFlow>,
    browser_player: Vec<PlayerFlow>,
    native_media: Vec<MediaEvidence>,
    browser_media: Vec<MediaEvidence>,
    decoder: Vec<DecoderFlow>,
    transport_sources: BTreeSet<TransportSource>,
    unknown_private: Vec<ComparisonFact>,
    malformed_records: u16,
    incomplete: bool,
}

impl ExtractionState {
    fn observe_record(&mut self, record: &CaptureRecordV1) {
        let view = match PayloadView::decode(record.payload.expose()) {
            Ok(view) if view.kind == expected_payload_kind(record.kind) => view,
            _ => {
                self.note_malformed();
                return;
            }
        };

        for private_field in view
            .fields
            .iter()
            .filter(|field| field.tag > field::REDIRECT_COUNT)
        {
            if self.unknown_private.len() < MAX_UNKNOWN_FACTS {
                self.unknown_private
                    .push(ComparisonFact::UnknownPrivate(UnknownPrivateFact::new(
                        private_field.tag,
                        private_field.value,
                    )));
            } else {
                self.incomplete = true;
            }
        }

        let result = match record.endpoint_role {
            EndpointRole::PlayerApi => {
                let Some(exchange_ref) = record.exchange_ref else {
                    self.note_malformed();
                    return;
                };
                let Some(index) =
                    flow_index(&mut self.native_player, exchange_ref, &mut self.incomplete)
                else {
                    return;
                };
                self.native_player[index].observe(record, &view, false)
            }
            EndpointRole::BrowserPlayer => {
                let Some(exchange_ref) = record.exchange_ref else {
                    self.note_malformed();
                    return;
                };
                let Some(index) =
                    flow_index(&mut self.browser_player, exchange_ref, &mut self.incomplete)
                else {
                    return;
                };
                self.browser_player[index].observe(record, &view, true)
            }
            EndpointRole::Media => self.observe_media_record(record, &view, false),
            EndpointRole::BrowserMedia => self.observe_media_record(record, &view, true),
            EndpointRole::Decoder => self.observe_decoder_record(record, &view),
            EndpointRole::Operation | EndpointRole::Unknown => {
                validate_global_record(record, &view)
            }
        };
        if result.is_err() {
            self.note_malformed();
            return;
        }
        self.note_transport(record.transport_kind);

        if record.kind == CaptureRecordKind::BrowserExchange {
            match (
                view.boolean(field::FROM_DISK_CACHE),
                view.boolean(field::FROM_SERVICE_WORKER),
            ) {
                (Ok(Some(true)), _) => {
                    self.transport_sources.insert(TransportSource::BrowserCache);
                }
                (_, Ok(Some(true))) => {
                    self.transport_sources
                        .insert(TransportSource::ServiceWorker);
                }
                (Ok(_), Ok(_)) => {}
                _ => self.note_malformed(),
            }
        }
    }

    fn note_transport(&mut self, transport: TransportKind) {
        let source = match transport {
            TransportKind::NativeHttp | TransportKind::MediaRange => TransportSource::NativeHttp,
            TransportKind::BrowserCdp => TransportSource::Browser,
            TransportKind::OfflineReplay => TransportSource::OfflineReplay,
            TransportKind::FreshReplay => TransportSource::FreshReplay,
            TransportKind::Unknown => return,
        };
        self.transport_sources.insert(source);
    }

    fn note_malformed(&mut self) {
        self.malformed_records = self.malformed_records.saturating_add(1);
        self.incomplete = true;
    }

    fn observe_media_record(
        &mut self,
        record: &CaptureRecordV1,
        view: &PayloadView<'_>,
        browser: bool,
    ) -> Result<(), ExtractionError> {
        let exchange_ref = record.exchange_ref.ok_or(ExtractionError)?;
        let destination = if browser {
            &mut self.browser_media
        } else {
            &mut self.native_media
        };
        let index =
            media_index(destination, exchange_ref, &mut self.incomplete).ok_or(ExtractionError)?;
        observe_media(&mut destination[index], record, view, browser)
    }

    fn observe_decoder_record(
        &mut self,
        record: &CaptureRecordV1,
        view: &PayloadView<'_>,
    ) -> Result<(), ExtractionError> {
        let exchange_ref = record.exchange_ref.ok_or(ExtractionError)?;
        let index = decoder_index(&mut self.decoder, exchange_ref, &mut self.incomplete)
            .ok_or(ExtractionError)?;
        observe_decoder(&mut self.decoder[index].evidence, record, view)
    }
}

fn flow_index(
    flows: &mut Vec<PlayerFlow>,
    exchange_ref: ExchangeRef,
    incomplete: &mut bool,
) -> Option<usize> {
    if let Some(index) = flows
        .iter()
        .position(|flow| flow.exchange_ref == exchange_ref)
    {
        return Some(index);
    }
    if flows.len() >= MAX_PLAYER_FLOWS {
        *incomplete = true;
        return None;
    }
    flows.push(PlayerFlow::new(exchange_ref));
    Some(flows.len().saturating_sub(1))
}

fn authoritative_flow(flows: &[PlayerFlow]) -> Option<&PlayerFlow> {
    flows.iter().max_by_key(|flow| {
        (
            flow.last_sequence,
            u8::from(flow.selection.is_some()),
            u8::from(flow.parse.is_some()),
            u8::from(flow.http.is_some()),
        )
    })
}

fn flow_for_exchange(flows: &[PlayerFlow], exchange_ref: ExchangeRef) -> Option<&PlayerFlow> {
    flows.iter().find(|flow| flow.exchange_ref == exchange_ref)
}

fn media_index(
    flows: &mut Vec<MediaEvidence>,
    exchange_ref: ExchangeRef,
    incomplete: &mut bool,
) -> Option<usize> {
    if let Some(index) = flows
        .iter()
        .position(|flow| flow.exchange_ref == exchange_ref)
    {
        return Some(index);
    }
    if flows.len() >= MAX_PLAYER_FLOWS {
        *incomplete = true;
        return None;
    }
    flows.push(MediaEvidence::new(exchange_ref));
    Some(flows.len().saturating_sub(1))
}

fn media_for_exchange(
    flows: &[MediaEvidence],
    exchange_ref: ExchangeRef,
) -> Option<&MediaEvidence> {
    flows.iter().find(|flow| flow.exchange_ref == exchange_ref)
}

fn decoder_index(
    flows: &mut Vec<DecoderFlow>,
    exchange_ref: ExchangeRef,
    incomplete: &mut bool,
) -> Option<usize> {
    if let Some(index) = flows
        .iter()
        .position(|flow| flow.exchange_ref == exchange_ref)
    {
        return Some(index);
    }
    if flows.len() >= MAX_PLAYER_FLOWS {
        *incomplete = true;
        return None;
    }
    flows.push(DecoderFlow {
        exchange_ref,
        evidence: None,
    });
    Some(flows.len().saturating_sub(1))
}

fn decoder_for_exchange(flows: &[DecoderFlow], exchange_ref: ExchangeRef) -> Option<&DecoderFlow> {
    flows.iter().find(|flow| flow.exchange_ref == exchange_ref)
}

struct PlayerFlow {
    exchange_ref: ExchangeRef,
    last_sequence: u16,
    maximum_attempt: u8,
    auth: Option<Sequenced<AuthEvidence>>,
    http: Option<Sequenced<HttpEvidence>>,
    parse: Option<Sequenced<ParseEvidence>>,
    inventory: Option<Sequenced<InventoryEvidence>>,
    selection: Option<Sequenced<SelectionEvidence>>,
    failure: Option<Sequenced<FailureCategory>>,
    browser_cache: bool,
    service_worker: bool,
}

impl PlayerFlow {
    const fn new(exchange_ref: ExchangeRef) -> Self {
        Self {
            exchange_ref,
            last_sequence: 0,
            maximum_attempt: 0,
            auth: None,
            http: None,
            parse: None,
            inventory: None,
            selection: None,
            failure: None,
            browser_cache: false,
            service_worker: false,
        }
    }

    fn observe(
        &mut self,
        record: &CaptureRecordV1,
        view: &PayloadView<'_>,
        browser: bool,
    ) -> Result<(), ExtractionError> {
        self.last_sequence = self.last_sequence.max(record.sequence);
        self.maximum_attempt = self.maximum_attempt.max(record.attempt);
        match record.kind {
            CaptureRecordKind::AuthSelection => {
                set_latest(&mut self.auth, record.sequence, parse_auth(record, view)?);
            }
            CaptureRecordKind::HttpRequest => validate_http_request(view)?,
            CaptureRecordKind::HttpResponse => {
                set_latest(&mut self.http, record.sequence, parse_http(view)?);
            }
            CaptureRecordKind::NetworkFailure => {
                set_latest(&mut self.failure, record.sequence, parse_failure(view)?);
            }
            CaptureRecordKind::PlayerParse if !browser => {
                set_latest(&mut self.parse, record.sequence, parse_player(view)?);
            }
            CaptureRecordKind::FormatInventory if !browser => {
                set_latest(&mut self.inventory, record.sequence, parse_inventory(view)?);
            }
            CaptureRecordKind::SelectionDecision if !browser => {
                set_latest(&mut self.selection, record.sequence, parse_selection(view)?);
            }
            CaptureRecordKind::BrowserExchange if browser => {
                view.required_u64(field::REQUEST_ORDINAL)?;
                self.browser_cache = view.boolean(field::FROM_DISK_CACHE)?.unwrap_or(false);
                self.service_worker = view.boolean(field::FROM_SERVICE_WORKER)?.unwrap_or(false);
            }
            _ => return Err(ExtractionError),
        }
        Ok(())
    }

    fn append_native_facts(&self, facts: &mut Vec<ComparisonFact>, incomplete: &mut bool) {
        if let Some(auth) = &self.auth {
            push(facts, SemanticFact::AuthenticationKind(auth.value.auth));
            push(facts, SemanticFact::PlayerClientKind(auth.value.client));
            push(
                facts,
                SemanticFact::ClientVersionPolicy(auth.value.version_policy),
            );
            push(
                facts,
                SemanticFact::ProofTokenPresent(auth.value.proof_token_present),
            );
        }
        if let Some(http) = &self.http {
            push(
                facts,
                SemanticFact::PlayerHttpStatus(HttpStatusCode::new(http.value.status)),
            );
            push(
                facts,
                SemanticFact::PlayerRedirectClass(if http.value.redirected {
                    RedirectClass::Redirected
                } else {
                    RedirectClass::None
                }),
            );
            push(
                facts,
                SemanticFact::TransportTiming(timing_bucket(http.value.elapsed_ms)),
            );
        }
        if let Some(parse) = &self.parse {
            if parse.value.unrecognized_status {
                *incomplete = true;
            }
            push(
                facts,
                SemanticFact::PlayabilityStatus(parse.value.playability),
            );
            push(
                facts,
                SemanticFact::SafeFailureCategory(parse.value.failure),
            );
            push(
                facts,
                SemanticFact::StreamingDataPresent(parse.value.streaming_data),
            );
            push(
                facts,
                SemanticFact::ParserTiming(timing_bucket(parse.value.elapsed_ms)),
            );
        }
        if let Some(failure) = &self.failure {
            push(facts, SemanticFact::SafeFailureCategory(failure.value));
        }
        if self.failure.as_ref().is_some_and(|failure| {
            self.parse
                .as_ref()
                .is_none_or(|parse| failure.sequence > parse.sequence)
        }) {
            let failure = self.failure.as_ref().expect("failure checked above");
            push(
                facts,
                SemanticFact::NativeResponseClass(response_class_for_failure(failure.value)),
            );
        } else if let Some(parse) = &self.parse {
            push(
                facts,
                SemanticFact::NativeResponseClass(parse.value.response_class),
            );
        } else if let Some(http) = &self.http {
            push(
                facts,
                SemanticFact::NativeResponseClass(response_class_without_parse(http.value.status)),
            );
        }
        if let Some(inventory) = &self.inventory {
            let inventory = &inventory.value;
            push(
                facts,
                SemanticFact::ReturnedFormatCount(BoundedCount::new(inventory.returned)),
            );
            push(
                facts,
                SemanticFact::SupportedFormatCount(BoundedCount::new(inventory.supported)),
            );
            push(
                facts,
                SemanticFact::DirectFormatCount(BoundedCount::new(inventory.direct)),
            );
            push(
                facts,
                SemanticFact::CipherFormatCount(BoundedCount::new(inventory.cipher)),
            );
            for format in inventory.formats.iter().take(16) {
                push(
                    facts,
                    SemanticFact::FormatIdentifier(PrivateFormatId::new(format.itag)),
                );
            }
            if inventory.formats.len() > 16 {
                *incomplete = true;
            }
        }
        if let Some(selection) = &self.selection {
            let selection = &selection.value;
            push(
                facts,
                SemanticFact::SelectedFormat(selection.selected_itag.map(PrivateFormatId::new)),
            );
            push(
                facts,
                SemanticFact::BrowserFallbackEligible(selection.fallback_eligible),
            );
            push(
                facts,
                SemanticFact::BrowserFallbackOutcome(selection.fallback_outcome),
            );
            push(
                facts,
                SemanticFact::SelectorTiming(timing_bucket(selection.elapsed_ms)),
            );
            if selection.failure != FailureCategory::None {
                push(facts, SemanticFact::SafeFailureCategory(selection.failure));
            }
            if let (Some(selected), Some(inventory)) =
                (selection.selected_itag, self.inventory.as_ref())
            {
                if let Some(format) = inventory
                    .value
                    .formats
                    .iter()
                    .find(|format| format.itag == selected)
                {
                    push(facts, SemanticFact::Container(format.container));
                    push(facts, SemanticFact::Codec(format.codec));
                    push(facts, SemanticFact::BitrateBucket(format.bitrate));
                } else {
                    *incomplete = true;
                }
            }
        }
        push(
            facts,
            SemanticFact::RetryCount(BoundedCount::new(usize::from(
                self.maximum_attempt.saturating_sub(1),
            ))),
        );
    }

    fn append_browser_facts(&self, facts: &mut Vec<ComparisonFact>, browser_selected: bool) {
        let response = if self.failure.as_ref().is_some_and(|failure| {
            self.http
                .as_ref()
                .is_none_or(|http| failure.sequence > http.sequence)
        }) {
            let failure = self.failure.as_ref().expect("failure checked above");
            response_class_for_failure(failure.value)
        } else if let Some(http) = &self.http {
            if browser_selected && (200..=299).contains(&http.value.status) {
                ResponseClassification::Playable
            } else {
                response_class_without_parse(http.value.status)
            }
        } else {
            ResponseClassification::Unknown
        };
        push(facts, SemanticFact::BrowserResponseClass(response));
        if self.browser_cache {
            push(
                facts,
                SemanticFact::TransportSource(TransportSource::BrowserCache),
            );
        }
        if self.service_worker {
            push(
                facts,
                SemanticFact::TransportSource(TransportSource::ServiceWorker),
            );
        }
    }
}

struct Sequenced<T> {
    sequence: u16,
    value: T,
}

impl<T> Sequenced<T> {
    const fn new(sequence: u16, value: T) -> Self {
        Self { sequence, value }
    }
}

fn set_latest<T>(destination: &mut Option<Sequenced<T>>, sequence: u16, value: T) {
    if destination
        .as_ref()
        .is_none_or(|current| sequence >= current.sequence)
    {
        *destination = Some(Sequenced::new(sequence, value));
    }
}

#[derive(Clone, Copy)]
struct AuthEvidence {
    auth: AuthenticationKind,
    client: PlayerClientKind,
    version_policy: ClientVersionPolicy,
    proof_token_present: bool,
}

#[derive(Clone, Copy)]
struct HttpEvidence {
    status: u16,
    redirected: bool,
    elapsed_ms: u64,
}

#[derive(Clone, Copy)]
struct ParseEvidence {
    playability: PlayabilityClass,
    failure: FailureCategory,
    streaming_data: bool,
    elapsed_ms: u64,
    response_class: ResponseClassification,
    unrecognized_status: bool,
}

struct InventoryEvidence {
    returned: usize,
    supported: usize,
    direct: usize,
    cipher: usize,
    formats: Vec<FormatEvidence>,
}

#[derive(Clone, Copy)]
struct FormatEvidence {
    itag: u64,
    container: ContainerKind,
    codec: CodecKind,
    bitrate: BitrateBucket,
}

#[derive(Clone, Copy)]
struct SelectionEvidence {
    selected_itag: Option<u64>,
    fallback_eligible: bool,
    fallback_outcome: FallbackOutcome,
    browser_selected: bool,
    failure: FailureCategory,
    elapsed_ms: u64,
}

struct MediaEvidence {
    exchange_ref: ExchangeRef,
    sequence: u16,
    status: Option<u16>,
    content_range: ContentRangeClass,
    elapsed_ms: Option<u64>,
    failure: Option<FailureCategory>,
    source: TransportSource,
}

impl MediaEvidence {
    const fn new(exchange_ref: ExchangeRef) -> Self {
        Self {
            exchange_ref,
            sequence: 0,
            status: None,
            content_range: ContentRangeClass::NotApplicable,
            elapsed_ms: None,
            failure: None,
            source: TransportSource::Unknown,
        }
    }

    fn append_facts(&self, facts: &mut Vec<ComparisonFact>) {
        if let Some(status) = self.status {
            push(
                facts,
                SemanticFact::MediaHttpStatus(HttpStatusCode::new(status)),
            );
        }
        push(facts, SemanticFact::ContentRangeClass(self.content_range));
        push(facts, SemanticFact::TransportSource(self.source));
        if let Some(elapsed_ms) = self.elapsed_ms {
            push(
                facts,
                SemanticFact::TransportTiming(timing_bucket(elapsed_ms)),
            );
        }
        if let Some(failure) = self.failure {
            push(facts, SemanticFact::SafeFailureCategory(failure));
        }
    }
}

struct DecoderFlow {
    exchange_ref: ExchangeRef,
    evidence: Option<DecoderEvidence>,
}

struct DecoderEvidence {
    sequence: u16,
    elapsed_ms: u64,
    failure: Option<FailureCategory>,
}

impl DecoderEvidence {
    fn append_facts(&self, facts: &mut Vec<ComparisonFact>) {
        push(
            facts,
            SemanticFact::DecoderTiming(timing_bucket(self.elapsed_ms)),
        );
        if let Some(failure) = self.failure {
            push(facts, SemanticFact::SafeFailureCategory(failure));
        }
    }
}

fn observe_media(
    current: &mut MediaEvidence,
    record: &CaptureRecordV1,
    view: &PayloadView<'_>,
    browser: bool,
) -> Result<(), ExtractionError> {
    let source = if browser {
        TransportSource::Browser
    } else {
        TransportSource::NativeHttp
    };
    match record.kind {
        CaptureRecordKind::MediaProbe => {
            let status = optional_status(view)?;
            let range_present = view.boolean(field::CONTENT_RANGE_PRESENT)?;
            let content_range = match (status, range_present, browser) {
                (Some(206), Some(true), _) => ContentRangeClass::Valid,
                (Some(206), Some(false), _) => ContentRangeClass::Missing,
                (Some(206), None, false) => return Err(ExtractionError),
                _ => ContentRangeClass::NotApplicable,
            };
            let elapsed_ms = view.u64(field::ELAPSED_MS)?;
            let source = if view.boolean(field::FROM_DISK_CACHE)?.unwrap_or(false) {
                TransportSource::BrowserCache
            } else if view.boolean(field::FROM_SERVICE_WORKER)?.unwrap_or(false) {
                TransportSource::ServiceWorker
            } else {
                source
            };
            if record.sequence >= current.sequence {
                current.sequence = record.sequence;
                current.status = status;
                current.elapsed_ms = elapsed_ms;
                current.content_range = content_range;
                current.source = source;
                current.failure = if status == Some(403) {
                    Some(FailureCategory::MediaForbidden)
                } else {
                    None
                };
            }
        }
        CaptureRecordKind::NetworkFailure => {
            let failure = parse_failure(view)?;
            if record.sequence >= current.sequence {
                current.sequence = record.sequence;
                current.failure = Some(failure);
                current.source = source;
                if failure == FailureCategory::MediaRangeContract {
                    current.content_range = ContentRangeClass::Invalid;
                }
            }
        }
        _ => return Err(ExtractionError),
    }
    Ok(())
}

fn observe_decoder(
    destination: &mut Option<DecoderEvidence>,
    record: &CaptureRecordV1,
    view: &PayloadView<'_>,
) -> Result<(), ExtractionError> {
    if record.kind != CaptureRecordKind::DecodeStage {
        return Err(ExtractionError);
    }
    let stage = view.required_bytes(field::STAGE, ValueKind::Text)?;
    let outcome = view.required_bytes(field::OUTCOME, ValueKind::Text)?;
    let elapsed_ms = view.required_u64(field::ELAPSED_MS)?;
    if stage != b"decoder_initialize" {
        return Ok(());
    }
    let failure = match outcome {
        b"completed" | b"started" => None,
        b"failed" => Some(FailureCategory::Decode),
        b"cancelled" => Some(FailureCategory::Cancelled),
        _ => return Err(ExtractionError),
    };
    let next = DecoderEvidence {
        sequence: record.sequence,
        elapsed_ms,
        failure,
    };
    if destination
        .as_ref()
        .is_none_or(|current| next.sequence >= current.sequence)
    {
        *destination = Some(next);
    }
    Ok(())
}

fn validate_global_record(
    record: &CaptureRecordV1,
    view: &PayloadView<'_>,
) -> Result<(), ExtractionError> {
    match record.kind {
        CaptureRecordKind::OperationBoundary => {
            view.required_bytes(field::STAGE, ValueKind::Text)?;
        }
        CaptureRecordKind::TerminalOutcome => {
            terminal_from_bytes(view.required_bytes(field::OUTCOME, ValueKind::Text)?)?;
        }
        _ => return Err(ExtractionError),
    }
    Ok(())
}

fn parse_auth(
    record: &CaptureRecordV1,
    view: &PayloadView<'_>,
) -> Result<AuthEvidence, ExtractionError> {
    let auth = match view.required_bytes(field::AUTH_KIND, ValueKind::Text)? {
        b"none" => AuthenticationKind::None,
        b"browser" => AuthenticationKind::Browser,
        b"oauth_bearer" => AuthenticationKind::OAuthBearer,
        b"unavailable" => AuthenticationKind::Unavailable,
        _ => return Err(ExtractionError),
    };
    let client = match record.client_kind {
        ProviderClientKind::Web => PlayerClientKind::Web,
        ProviderClientKind::WebRemix => PlayerClientKind::WebRemix,
        ProviderClientKind::Android => PlayerClientKind::Android,
        ProviderClientKind::Ios => PlayerClientKind::Ios,
        ProviderClientKind::TvHtml5 => PlayerClientKind::TvHtml5,
        ProviderClientKind::Unknown | ProviderClientKind::Spotify => PlayerClientKind::Unknown,
    };
    let version_policy = match view.bytes(field::VERSION_SOURCE, ValueKind::Text)? {
        Some(b"static") => ClientVersionPolicy::Static,
        Some(b"cached") => ClientVersionPolicy::Cached,
        Some(b"discovered") => ClientVersionPolicy::Discovered,
        Some(b"fallback") => ClientVersionPolicy::Fallback,
        Some(_) => return Err(ExtractionError),
        None => ClientVersionPolicy::Unknown,
    };
    Ok(AuthEvidence {
        auth,
        client,
        version_policy,
        proof_token_present: view.boolean(field::PROOF_TOKEN_PRESENT)?.unwrap_or(false),
    })
}

fn parse_http(view: &PayloadView<'_>) -> Result<HttpEvidence, ExtractionError> {
    Ok(HttpEvidence {
        status: required_status(view)?,
        redirected: view.required_bool(field::REDIRECTED)?,
        elapsed_ms: view.required_u64(field::ELAPSED_MS)?,
    })
}

fn validate_http_request(view: &PayloadView<'_>) -> Result<(), ExtractionError> {
    view.required_bytes(field::METHOD, ValueKind::Text)?;
    view.required_bytes(field::URL, ValueKind::Text)?;
    Ok(())
}

fn parse_player(view: &PayloadView<'_>) -> Result<ParseEvidence, ExtractionError> {
    let status = view.required_bytes(field::PLAYABILITY_STATUS, ValueKind::Text)?;
    let (playability, unrecognized_status) = match status {
        b"OK" => (PlayabilityClass::Playable, false),
        b"LOGIN_REQUIRED" => (PlayabilityClass::LoginRequired, false),
        b"AGE_CHECK_REQUIRED" | b"CONTENT_CHECK_REQUIRED" => {
            (PlayabilityClass::AgeConsentOrRegion, false)
        }
        b"UNPLAYABLE" | b"ERROR" | b"LIVE_STREAM_OFFLINE" => (PlayabilityClass::Unavailable, false),
        b"malformed" => (PlayabilityClass::Other, false),
        _ => (PlayabilityClass::Other, true),
    };
    let failure = failure_from_bytes(view.required_bytes(field::CATEGORY, ValueKind::Text)?)?;
    let response_class = if playability == PlayabilityClass::Playable {
        ResponseClassification::Playable
    } else if failure == FailureCategory::Contract {
        ResponseClassification::Malformed
    } else {
        response_class_for_failure(failure)
    };
    Ok(ParseEvidence {
        playability,
        failure,
        streaming_data: view.required_bool(field::STREAMING_DATA_PRESENT)?,
        elapsed_ms: view.required_u64(field::ELAPSED_MS)?,
        response_class,
        unrecognized_status,
    })
}

fn parse_inventory(view: &PayloadView<'_>) -> Result<InventoryEvidence, ExtractionError> {
    let returned = bounded_usize(view.required_u64(field::RETURNED_FORMATS)?)?;
    let supported = bounded_usize(view.required_u64(field::SUPPORTED_FORMATS)?)?;
    let direct = bounded_usize(view.required_u64(field::DIRECT_FORMATS)?)?;
    let cipher = bounded_usize(view.required_u64(field::CIPHER_FORMATS)?)?;
    if supported > returned || direct > returned || cipher > returned {
        return Err(ExtractionError);
    }
    let formats = parse_format_facts(view.required_bytes(field::FORMAT_FACTS, ValueKind::Bytes)?)?;
    Ok(InventoryEvidence {
        returned,
        supported,
        direct,
        cipher,
        formats,
    })
}

fn parse_selection(view: &PayloadView<'_>) -> Result<SelectionEvidence, ExtractionError> {
    let outcome = view.required_bytes(field::OUTCOME, ValueKind::Text)?;
    let fallback_eligible = view.required_bool(field::FALLBACK_ELIGIBLE)?;
    let fallback_attempted = view.required_bool(field::FALLBACK_ATTEMPTED)?;
    let browser_selected = outcome == b"browser_selected";
    let failure = if matches!(outcome, b"native_selected" | b"browser_selected") {
        FailureCategory::None
    } else {
        failure_from_bytes(outcome)?
    };
    let fallback_outcome = if browser_selected {
        FallbackOutcome::Succeeded
    } else if fallback_attempted {
        FallbackOutcome::Failed
    } else if fallback_eligible {
        FallbackOutcome::Eligible
    } else {
        FallbackOutcome::NotEligible
    };
    Ok(SelectionEvidence {
        selected_itag: view.u64(field::SELECTED_ITAG)?,
        fallback_eligible,
        fallback_outcome,
        browser_selected,
        failure,
        elapsed_ms: view.required_u64(field::ELAPSED_MS)?,
    })
}

fn parse_failure(view: &PayloadView<'_>) -> Result<FailureCategory, ExtractionError> {
    failure_from_bytes(view.required_bytes(field::CATEGORY, ValueKind::Text)?)
}

fn parse_format_facts(bytes: &[u8]) -> Result<Vec<FormatEvidence>, ExtractionError> {
    let mut cursor = 0_usize;
    let mut formats = Vec::new();
    while cursor < bytes.len() {
        if formats.len() >= MAX_PARSED_FORMATS {
            return Err(ExtractionError);
        }
        let itag = take_u64(bytes, &mut cursor)?;
        let bitrate = take_u64(bytes, &mut cursor)?;
        take(bytes, &mut cursor, 2)?;
        let mime_length = usize::from(take_u16(bytes, &mut cursor)?);
        let mime = take(bytes, &mut cursor, mime_length)?;
        for _ in 0..2 {
            let length = usize::from(take_u16(bytes, &mut cursor)?);
            take(bytes, &mut cursor, length)?;
        }
        formats.push(FormatEvidence {
            itag,
            container: if mime.starts_with(b"audio/mp4") {
                ContainerKind::Mp4
            } else if mime.starts_with(b"audio/webm") {
                ContainerKind::WebM
            } else {
                ContainerKind::Other
            },
            codec: if contains_ascii_case_insensitive(mime, b"mp4a") {
                CodecKind::Aac
            } else if contains_ascii_case_insensitive(mime, b"opus") {
                CodecKind::Opus
            } else if contains_ascii_case_insensitive(mime, b"vorbis") {
                CodecKind::Vorbis
            } else {
                CodecKind::Other
            },
            bitrate: match bitrate {
                0 => BitrateBucket::Unknown,
                1..=127_999 => BitrateBucket::Low,
                128_000..=255_999 => BitrateBucket::Medium,
                _ => BitrateBucket::High,
            },
        });
    }
    Ok(formats)
}

fn contains_ascii_case_insensitive(haystack: &[u8], needle: &[u8]) -> bool {
    haystack.windows(needle.len()).any(|window| {
        window
            .iter()
            .zip(needle)
            .all(|(left, right)| left.eq_ignore_ascii_case(right))
    })
}

fn take<'a>(
    bytes: &'a [u8],
    cursor: &mut usize,
    length: usize,
) -> Result<&'a [u8], ExtractionError> {
    let end = cursor.checked_add(length).ok_or(ExtractionError)?;
    let value = bytes.get(*cursor..end).ok_or(ExtractionError)?;
    *cursor = end;
    Ok(value)
}

fn take_u16(bytes: &[u8], cursor: &mut usize) -> Result<u16, ExtractionError> {
    let bytes = take(bytes, cursor, 2)?;
    Ok(u16::from_be_bytes([bytes[0], bytes[1]]))
}

fn take_u64(bytes: &[u8], cursor: &mut usize) -> Result<u64, ExtractionError> {
    let bytes = take(bytes, cursor, 8)?;
    let mut value = [0_u8; 8];
    value.copy_from_slice(bytes);
    Ok(u64::from_be_bytes(value))
}

fn bounded_usize(value: u64) -> Result<usize, ExtractionError> {
    u16::try_from(value)
        .map(usize::from)
        .map_err(|_| ExtractionError)
}

fn failure_from_bytes(bytes: &[u8]) -> Result<FailureCategory, ExtractionError> {
    Ok(match bytes {
        b"playable" | b"success" | b"native_selected" | b"browser_selected" => {
            FailureCategory::None
        }
        b"authentication" => FailureCategory::Authentication,
        b"consent_age_region" => FailureCategory::ConsentAgeOrRegion,
        b"provider_unavailable" | b"unavailable" => FailureCategory::ProviderUnavailable,
        b"proof_token" => FailureCategory::ProofToken,
        b"decipher" => FailureCategory::Decipher,
        b"rate_limited" => FailureCategory::RateLimited,
        b"network" | b"loading_failed" => FailureCategory::Network,
        b"contract" => FailureCategory::Contract,
        b"unsupported_format" => FailureCategory::UnsupportedFormat,
        b"media_forbidden" => FailureCategory::MediaForbidden,
        b"media_range_contract" => FailureCategory::MediaRangeContract,
        b"decode" => FailureCategory::Decode,
        b"cancelled" | b"capture_ended" => FailureCategory::Cancelled,
        _ => return Err(ExtractionError),
    })
}

fn response_class_for_failure(failure: FailureCategory) -> ResponseClassification {
    match failure {
        FailureCategory::None => ResponseClassification::Playable,
        FailureCategory::Network => ResponseClassification::TransportFailure,
        FailureCategory::Cancelled => ResponseClassification::Cancelled,
        FailureCategory::Contract => ResponseClassification::Malformed,
        _ => ResponseClassification::Refused,
    }
}

fn response_class_without_parse(status: u16) -> ResponseClassification {
    match status {
        400..=599 => ResponseClassification::Refused,
        _ => ResponseClassification::Unknown,
    }
}

fn required_status(view: &PayloadView<'_>) -> Result<u16, ExtractionError> {
    let status = view.required_u64(field::STATUS)?;
    u16::try_from(status)
        .ok()
        .filter(|status| (100..=599).contains(status))
        .ok_or(ExtractionError)
}

fn optional_status(view: &PayloadView<'_>) -> Result<Option<u16>, ExtractionError> {
    view.u64(field::STATUS)?
        .map(|status| {
            u16::try_from(status)
                .ok()
                .filter(|status| (100..=599).contains(status))
                .ok_or(ExtractionError)
        })
        .transpose()
}

fn terminal_outcome(terminal: SafeTerminalCategory) -> TerminalOutcome {
    match terminal {
        SafeTerminalCategory::Success => TerminalOutcome::Success,
        SafeTerminalCategory::Failed => TerminalOutcome::Failed,
        SafeTerminalCategory::Cancelled => TerminalOutcome::Cancelled,
        SafeTerminalCategory::Superseded => TerminalOutcome::Superseded,
        SafeTerminalCategory::TimedOut => TerminalOutcome::TimedOut,
        SafeTerminalCategory::Panicked => TerminalOutcome::Panicked,
    }
}

fn terminal_from_bytes(bytes: &[u8]) -> Result<TerminalOutcome, ExtractionError> {
    Ok(match bytes {
        b"success" => TerminalOutcome::Success,
        b"failed" => TerminalOutcome::Failed,
        b"cancelled" => TerminalOutcome::Cancelled,
        b"superseded" => TerminalOutcome::Superseded,
        b"timed_out" => TerminalOutcome::TimedOut,
        b"panicked" => TerminalOutcome::Panicked,
        _ => return Err(ExtractionError),
    })
}

fn cancellation_state(capture: &PrivateCaptureV1) -> CancellationState {
    if capture
        .incomplete_reasons
        .contains(&IncompleteReason::Shutdown)
    {
        return CancellationState::Shutdown;
    }
    match capture.terminal_category {
        SafeTerminalCategory::Cancelled => CancellationState::Cancelled,
        SafeTerminalCategory::Superseded => CancellationState::Superseded,
        _ => CancellationState::NotCancelled,
    }
}

fn timing_bucket(elapsed_ms: u64) -> TimingBucket {
    match elapsed_ms {
        0 => TimingBucket::Immediate,
        1..=50 => TimingBucket::Fast,
        51..=250 => TimingBucket::Moderate,
        251..=2_000 => TimingBucket::Slow,
        2_001..=60_000 => TimingBucket::VerySlow,
        _ => TimingBucket::TimedOut,
    }
}

fn push(facts: &mut Vec<ComparisonFact>, fact: SemanticFact) {
    facts.push(ComparisonFact::Registered(fact));
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct ExtractionError;

#[derive(Clone, Copy, PartialEq, Eq)]
enum ValueKind {
    Bytes = 1,
    Text = 2,
    U64 = 3,
    Bool = 4,
}

impl ValueKind {
    fn from_tag(tag: u8) -> Result<Self, ExtractionError> {
        match tag {
            1 => Ok(Self::Bytes),
            2 => Ok(Self::Text),
            3 => Ok(Self::U64),
            4 => Ok(Self::Bool),
            _ => Err(ExtractionError),
        }
    }
}

struct PayloadField<'a> {
    tag: u16,
    kind: ValueKind,
    value: &'a [u8],
}

struct PayloadView<'a> {
    kind: u8,
    fields: Vec<PayloadField<'a>>,
}

impl<'a> PayloadView<'a> {
    fn decode(bytes: &'a [u8]) -> Result<Self, ExtractionError> {
        if bytes.len() < PAYLOAD_HEADER_BYTES || &bytes[..8] != PAYLOAD_MAGIC {
            return Err(ExtractionError);
        }
        if u16::from_be_bytes([bytes[8], bytes[9]]) != PAYLOAD_SCHEMA_VERSION {
            return Err(ExtractionError);
        }
        let kind = bytes[10];
        if !(1..=12).contains(&kind) {
            return Err(ExtractionError);
        }
        let count = usize::from(u16::from_be_bytes([bytes[11], bytes[12]]));
        if count > MAX_PAYLOAD_FIELDS {
            return Err(ExtractionError);
        }
        let mut cursor = PAYLOAD_HEADER_BYTES;
        let mut fields = Vec::with_capacity(count);
        for _ in 0..count {
            let header = take(bytes, &mut cursor, 7)?;
            let tag = u16::from_be_bytes([header[0], header[1]]);
            if fields
                .iter()
                .any(|field: &PayloadField<'_>| field.tag == tag)
            {
                return Err(ExtractionError);
            }
            let kind = ValueKind::from_tag(header[2])?;
            let length = usize::try_from(u32::from_be_bytes([
                header[3], header[4], header[5], header[6],
            ]))
            .map_err(|_| ExtractionError)?;
            let value = take(bytes, &mut cursor, length)?;
            match kind {
                ValueKind::Text => {
                    std::str::from_utf8(value).map_err(|_| ExtractionError)?;
                }
                ValueKind::U64 if value.len() != 8 => return Err(ExtractionError),
                ValueKind::Bool if !matches!(value, [0 | 1]) => {
                    return Err(ExtractionError);
                }
                ValueKind::Bytes | ValueKind::U64 | ValueKind::Bool => {}
            }
            if expected_field_kind(tag).is_some_and(|expected| expected != kind) {
                return Err(ExtractionError);
            }
            fields.push(PayloadField { tag, kind, value });
        }
        if cursor != bytes.len() {
            return Err(ExtractionError);
        }
        Ok(Self { kind, fields })
    }

    fn bytes(&self, tag: u16, kind: ValueKind) -> Result<Option<&'a [u8]>, ExtractionError> {
        let Some(field) = self.fields.iter().find(|field| field.tag == tag) else {
            return Ok(None);
        };
        if field.kind != kind {
            return Err(ExtractionError);
        }
        Ok(Some(field.value))
    }

    fn required_bytes(&self, tag: u16, kind: ValueKind) -> Result<&'a [u8], ExtractionError> {
        self.bytes(tag, kind)?.ok_or(ExtractionError)
    }

    fn u64(&self, tag: u16) -> Result<Option<u64>, ExtractionError> {
        self.bytes(tag, ValueKind::U64)?
            .map(|bytes| {
                let bytes: [u8; 8] = bytes.try_into().map_err(|_| ExtractionError)?;
                Ok(u64::from_be_bytes(bytes))
            })
            .transpose()
    }

    fn required_u64(&self, tag: u16) -> Result<u64, ExtractionError> {
        self.u64(tag)?.ok_or(ExtractionError)
    }

    fn boolean(&self, tag: u16) -> Result<Option<bool>, ExtractionError> {
        self.bytes(tag, ValueKind::Bool)?
            .map(|bytes| match bytes {
                [0] => Ok(false),
                [1] => Ok(true),
                _ => Err(ExtractionError),
            })
            .transpose()
    }

    fn required_bool(&self, tag: u16) -> Result<bool, ExtractionError> {
        self.boolean(tag)?.ok_or(ExtractionError)
    }
}

const fn expected_payload_kind(kind: CaptureRecordKind) -> u8 {
    match kind {
        CaptureRecordKind::OperationBoundary => 1,
        CaptureRecordKind::AuthSelection => 2,
        CaptureRecordKind::HttpRequest => 3,
        CaptureRecordKind::HttpResponse => 4,
        CaptureRecordKind::NetworkFailure => 5,
        CaptureRecordKind::PlayerParse => 6,
        CaptureRecordKind::FormatInventory => 7,
        CaptureRecordKind::SelectionDecision => 8,
        CaptureRecordKind::BrowserExchange => 9,
        CaptureRecordKind::MediaProbe => 10,
        CaptureRecordKind::DecodeStage => 11,
        CaptureRecordKind::TerminalOutcome => 12,
        CaptureRecordKind::SyntheticFixture => 0,
    }
}

const fn expected_field_kind(tag: u16) -> Option<ValueKind> {
    match tag {
        field::HEADERS | field::BODY | field::FORMAT_FACTS => Some(ValueKind::Bytes),
        field::METHOD
        | field::URL
        | field::STAGE
        | field::CATEGORY
        | field::AUTH_KIND
        | field::CLIENT_KIND
        | field::CLIENT_VERSION
        | field::VERSION_SOURCE
        | field::USER_AGENT_PROFILE
        | field::PLAYABILITY_STATUS
        | field::PLAYABILITY_REASON
        | field::QUALITY
        | field::OUTCOME => Some(ValueKind::Text),
        field::STATUS
        | field::ELAPSED_MS
        | field::BODY_LENGTH
        | field::RETAINED_BODY_LENGTH
        | field::RETURNED_FORMATS
        | field::SUPPORTED_FORMATS
        | field::DIRECT_FORMATS
        | field::CIPHER_FORMATS
        | field::SELECTED_ITAG
        | field::RANGE_START
        | field::RANGE_END
        | field::RESPONSE_LENGTH
        | field::REQUEST_ORDINAL
        | field::DROPPED_COUNT
        | field::REDIRECT_COUNT => Some(ValueKind::U64),
        field::BODY_COMPLETE
        | field::PROOF_TOKEN_PRESENT
        | field::STREAMING_DATA_PRESENT
        | field::FALLBACK_ELIGIBLE
        | field::FALLBACK_ATTEMPTED
        | field::ERROR_IS_TIMEOUT
        | field::ERROR_IS_CONNECT
        | field::ERROR_IS_REQUEST
        | field::ERROR_IS_BODY
        | field::ERROR_IS_DECODE
        | field::REDIRECTED
        | field::CONTENT_RANGE_PRESENT
        | field::FROM_DISK_CACHE
        | field::FROM_SERVICE_WORKER => Some(ValueKind::Bool),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use std::time::{Duration, Instant};

    use super::*;
    use crate::developer_capture::{
        model::{CapturePurpose, CaptureRef, IncompleteReason, SafeOperationRef},
        payload::{encode_fields, PrivateField, PrivatePayloadKind},
        security::CapturePassphrase,
        store::CaptureStore,
    };
    use static_assertions::assert_not_impl_any;

    assert_not_impl_any!(CaptureSemantics: std::fmt::Debug, std::fmt::Display, serde::Serialize);
    assert_not_impl_any!(CaptureComparisonV1: std::fmt::Debug, std::fmt::Display, serde::Serialize);
    assert_not_impl_any!(ReplayResultEvidence: std::fmt::Debug, std::fmt::Display, serde::Serialize);

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

    fn format_blob(formats: &[(u64, u64, bool, bool, &str)]) -> Vec<u8> {
        let mut output = Vec::new();
        for (itag, bitrate, direct, cipher, mime) in formats {
            output.extend_from_slice(&itag.to_be_bytes());
            output.extend_from_slice(&bitrate.to_be_bytes());
            output.push(u8::from(*direct));
            output.push(u8::from(*cipher));
            output.extend_from_slice(&u16::try_from(mime.len()).unwrap().to_be_bytes());
            output.extend_from_slice(mime.as_bytes());
            output.extend_from_slice(&0_u16.to_be_bytes());
            output.extend_from_slice(&0_u16.to_be_bytes());
        }
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

    fn capture(
        capture_byte: u8,
        records: Vec<CaptureRecordV1>,
        terminal: SafeTerminalCategory,
        complete: bool,
    ) -> PrivateCaptureV1 {
        let now = u64::try_from(
            std::time::SystemTime::now()
                .duration_since(std::time::SystemTime::UNIX_EPOCH)
                .unwrap()
                .as_millis(),
        )
        .unwrap();
        PrivateCaptureV1::new(
            CaptureRef::from_bytes([capture_byte; 16]),
            now,
            now.saturating_add(1),
            CapturePurpose::InteractivePlayback,
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

    fn player_records(exchange: ExchangeRef, start: u16) -> Vec<CaptureRecordV1> {
        let formats = format_blob(&[(140, 128_000, true, false, "audio/mp4; codecs=mp4a.40.2")]);
        vec![
            context_record(
                start,
                Some(exchange),
                EndpointRole::PlayerApi,
                ProviderClientKind::Android,
                TransportKind::NativeHttp,
                1,
                CaptureRecordKind::HttpRequest,
                PrivatePayloadKind::HttpRequest,
                &[
                    PrivateField::text(field::METHOD, "POST"),
                    PrivateField::text(field::URL, "https://private.invalid/player"),
                ],
            ),
            context_record(
                start + 1,
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
                start + 2,
                Some(exchange),
                EndpointRole::PlayerApi,
                ProviderClientKind::Android,
                TransportKind::NativeHttp,
                1,
                CaptureRecordKind::HttpResponse,
                PrivatePayloadKind::HttpResponse,
                &[
                    PrivateField::u64(field::STATUS, 200),
                    PrivateField::boolean(field::REDIRECTED, false),
                    PrivateField::u64(field::ELAPSED_MS, 35),
                ],
            ),
            context_record(
                start + 3,
                Some(exchange),
                EndpointRole::PlayerApi,
                ProviderClientKind::Android,
                TransportKind::NativeHttp,
                1,
                CaptureRecordKind::PlayerParse,
                PrivatePayloadKind::PlayerParse,
                &[
                    PrivateField::text(field::PLAYABILITY_STATUS, "OK"),
                    PrivateField::text(field::CATEGORY, "playable"),
                    PrivateField::boolean(field::STREAMING_DATA_PRESENT, true),
                    PrivateField::u64(field::ELAPSED_MS, 2),
                ],
            ),
            context_record(
                start + 4,
                Some(exchange),
                EndpointRole::PlayerApi,
                ProviderClientKind::Android,
                TransportKind::NativeHttp,
                1,
                CaptureRecordKind::FormatInventory,
                PrivatePayloadKind::FormatInventory,
                &[
                    PrivateField::u64(field::RETURNED_FORMATS, 1),
                    PrivateField::u64(field::SUPPORTED_FORMATS, 1),
                    PrivateField::u64(field::DIRECT_FORMATS, 1),
                    PrivateField::u64(field::CIPHER_FORMATS, 0),
                    PrivateField::bytes(field::FORMAT_FACTS, &formats),
                ],
            ),
            context_record(
                start + 5,
                Some(exchange),
                EndpointRole::PlayerApi,
                ProviderClientKind::Android,
                TransportKind::NativeHttp,
                1,
                CaptureRecordKind::SelectionDecision,
                PrivatePayloadKind::SelectionDecision,
                &[
                    PrivateField::text(field::OUTCOME, "native_selected"),
                    PrivateField::boolean(field::FALLBACK_ELIGIBLE, false),
                    PrivateField::boolean(field::FALLBACK_ATTEMPTED, false),
                    PrivateField::u64(field::SELECTED_ITAG, 140),
                    PrivateField::u64(field::ELAPSED_MS, 3),
                ],
            ),
        ]
    }

    fn replayable_player_records(
        exchange: ExchangeRef,
        decision: ReplayDecision,
    ) -> Vec<CaptureRecordV1> {
        let request_body = br#"{"videoId":"private-fixture-id"}"#;
        let response_body = br#"{"private":"provider-response"}"#;
        let formats = format_blob(&[(140, 128_000, true, false, "audio/mp4; codecs=mp4a.40.2")]);
        let provider_category = match decision.provider {
            ReplayProviderOutcome::Playable => "playable",
            ReplayProviderOutcome::Authentication => "authentication",
            ReplayProviderOutcome::ConsentAgeRegion => "consent_age_region",
            ReplayProviderOutcome::ProviderUnavailable => "provider_unavailable",
            ReplayProviderOutcome::ProofToken => "proof_token",
            ReplayProviderOutcome::RateLimited => "rate_limited",
            ReplayProviderOutcome::NetworkFailed => "network",
            ReplayProviderOutcome::Contract => "contract",
        };
        let playability = match decision.provider {
            ReplayProviderOutcome::Playable => "OK",
            ReplayProviderOutcome::Authentication => "LOGIN_REQUIRED",
            ReplayProviderOutcome::ConsentAgeRegion => "AGE_CHECK_REQUIRED",
            ReplayProviderOutcome::ProviderUnavailable => "UNPLAYABLE",
            _ => "ERROR",
        };
        let selection = match decision.selection {
            ReplaySelectionOutcome::NotAttempted => "contract",
            ReplaySelectionOutcome::Selected { .. } => "native_selected",
            ReplaySelectionOutcome::CipherOnly => "decipher",
            ReplaySelectionOutcome::UnsupportedFormat => "unsupported_format",
            ReplaySelectionOutcome::NoDirectFormat => "provider_unavailable",
        };

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
                    PrivateField::text(field::URL, "https://www.youtube.com/youtubei/v1/player"),
                    PrivateField::bytes(field::BODY, request_body),
                    PrivateField::u64(
                        field::BODY_LENGTH,
                        u64::try_from(request_body.len()).unwrap(),
                    ),
                    PrivateField::u64(
                        field::RETAINED_BODY_LENGTH,
                        u64::try_from(request_body.len()).unwrap(),
                    ),
                    PrivateField::boolean(field::BODY_COMPLETE, true),
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
                    PrivateField::text(field::CLIENT_KIND, "android_vr"),
                    PrivateField::boolean(field::PROOF_TOKEN_PRESENT, false),
                    PrivateField::text(field::QUALITY, "high"),
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
                    PrivateField::u64(field::STATUS, 200),
                    PrivateField::bytes(field::BODY, response_body),
                    PrivateField::u64(
                        field::BODY_LENGTH,
                        u64::try_from(response_body.len()).unwrap(),
                    ),
                    PrivateField::u64(
                        field::RETAINED_BODY_LENGTH,
                        u64::try_from(response_body.len()).unwrap(),
                    ),
                    PrivateField::boolean(field::BODY_COMPLETE, true),
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
                    PrivateField::text(field::CATEGORY, provider_category),
                    PrivateField::boolean(
                        field::STREAMING_DATA_PRESENT,
                        decision.streaming_data_present,
                    ),
                    PrivateField::u64(field::ELAPSED_MS, 1),
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
                    PrivateField::u64(
                        field::RETURNED_FORMATS,
                        u64::from(decision.returned_formats),
                    ),
                    PrivateField::u64(
                        field::SUPPORTED_FORMATS,
                        u64::from(decision.supported_formats),
                    ),
                    PrivateField::u64(field::DIRECT_FORMATS, u64::from(decision.direct_formats)),
                    PrivateField::u64(field::CIPHER_FORMATS, u64::from(decision.cipher_formats)),
                    PrivateField::bytes(field::FORMAT_FACTS, &formats),
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
                    PrivateField::text(field::QUALITY, "high"),
                    PrivateField::text(field::OUTCOME, selection),
                    PrivateField::boolean(field::FALLBACK_ELIGIBLE, false),
                    PrivateField::boolean(field::FALLBACK_ATTEMPTED, false),
                    PrivateField::u64(
                        field::SELECTED_ITAG,
                        match decision.selection {
                            ReplaySelectionOutcome::Selected { itag } => itag,
                            _ => 0,
                        },
                    ),
                    PrivateField::u64(field::ELAPSED_MS, 1),
                ],
            ),
            terminal_record(6, SafeTerminalCategory::Success),
        ]
    }

    fn replay_result_payload(
        parent: [u8; 16],
        exchange: ExchangeRef,
        outcome: u8,
        decision: ReplayDecision,
    ) -> SensitiveBytes {
        let mut bytes = Vec::new();
        bytes.extend_from_slice(REPLAY_RESULT_MAGIC);
        bytes.extend_from_slice(&REPLAY_RESULT_SCHEMA_VERSION.to_be_bytes());
        bytes.push(1);
        bytes.extend_from_slice(&parent);
        bytes.extend_from_slice(&exchange.bytes());
        bytes.push(outcome);
        bytes.push(1);
        bytes.push(match decision.parser {
            ReplayParserOutcome::NotRun => 0,
            ReplayParserOutcome::Parsed => 1,
            ReplayParserOutcome::Malformed => 2,
        });
        bytes.push(match decision.provider {
            ReplayProviderOutcome::Playable => 0,
            ReplayProviderOutcome::Authentication => 1,
            ReplayProviderOutcome::ConsentAgeRegion => 2,
            ReplayProviderOutcome::ProviderUnavailable => 3,
            ReplayProviderOutcome::ProofToken => 4,
            ReplayProviderOutcome::RateLimited => 5,
            ReplayProviderOutcome::NetworkFailed => 6,
            ReplayProviderOutcome::Contract => 7,
        });
        bytes.push(u8::from(decision.streaming_data_present));
        bytes.extend_from_slice(&decision.returned_formats.to_be_bytes());
        bytes.extend_from_slice(&decision.supported_formats.to_be_bytes());
        bytes.extend_from_slice(&decision.direct_formats.to_be_bytes());
        bytes.extend_from_slice(&decision.cipher_formats.to_be_bytes());
        let (selection, itag) = match decision.selection {
            ReplaySelectionOutcome::NotAttempted => (0, 0),
            ReplaySelectionOutcome::Selected { itag } => (1, itag),
            ReplaySelectionOutcome::CipherOnly => (2, 0),
            ReplaySelectionOutcome::UnsupportedFormat => (3, 0),
            ReplaySelectionOutcome::NoDirectFormat => (4, 0),
        };
        bytes.push(selection);
        bytes.extend_from_slice(&itag.to_be_bytes());
        SensitiveBytes::new(bytes)
    }

    fn replay_child(
        capture_byte: u8,
        parent: [u8; 16],
        exchange: ExchangeRef,
        outcome: u8,
        decision: ReplayDecision,
    ) -> PrivateCaptureV1 {
        let result = CaptureRecordV1::with_context(
            0,
            0,
            Some(exchange),
            EndpointRole::Operation,
            ProviderClientKind::Unknown,
            TransportKind::OfflineReplay,
            0,
            CaptureRecordKind::OperationBoundary,
            replay_result_payload(parent, exchange, outcome, decision),
        );
        let terminal = context_record(
            1,
            Some(exchange),
            EndpointRole::Operation,
            ProviderClientKind::Unknown,
            TransportKind::OfflineReplay,
            0,
            CaptureRecordKind::TerminalOutcome,
            PrivatePayloadKind::TerminalOutcome,
            &[PrivateField::text(field::OUTCOME, "success")],
        );
        let now = u64::try_from(
            std::time::SystemTime::now()
                .duration_since(std::time::SystemTime::UNIX_EPOCH)
                .unwrap()
                .as_millis(),
        )
        .unwrap();
        PrivateCaptureV1::new(
            CaptureRef::from_bytes([capture_byte; 16]),
            now,
            now.saturating_add(1),
            CapturePurpose::Replay,
            SafeOperationRef::from_bytes([capture_byte; 4]),
            vec![result, terminal],
            CaptureCompleteness::Complete,
            Vec::new(),
            0,
            SafeTerminalCategory::Success,
        )
    }

    #[test]
    fn real_records_extract_and_compare_after_encrypted_vault_round_trip() {
        let exchange = ExchangeRef::from_bytes([1; 8]);
        let mut records = player_records(exchange, 0);
        records.push(context_record(
            6,
            Some(exchange),
            EndpointRole::Media,
            ProviderClientKind::Unknown,
            TransportKind::MediaRange,
            1,
            CaptureRecordKind::MediaProbe,
            PrivatePayloadKind::MediaProbe,
            &[
                PrivateField::u64(field::STATUS, 206),
                PrivateField::boolean(field::CONTENT_RANGE_PRESENT, true),
                PrivateField::u64(field::ELAPSED_MS, 12),
            ],
        ));
        records.push(context_record(
            7,
            Some(exchange),
            EndpointRole::Decoder,
            ProviderClientKind::Unknown,
            TransportKind::MediaRange,
            0,
            CaptureRecordKind::DecodeStage,
            PrivatePayloadKind::DecodeStage,
            &[
                PrivateField::text(field::STAGE, "decoder_initialize"),
                PrivateField::text(field::OUTCOME, "completed"),
                PrivateField::u64(field::ELAPSED_MS, 8),
            ],
        ));
        records.push(terminal_record(8, SafeTerminalCategory::Success));
        let original = capture(1, records, SafeTerminalCategory::Success, true);

        let directory = tempfile::tempdir().unwrap();
        let root = directory.path().join("captures");
        let (store, _) =
            CaptureStore::open(&root, super::super::model::CaptureLimits::default()).unwrap();
        store
            .write(
                &original,
                &CapturePassphrase::new("comparison fixture passphrase".to_owned()).unwrap(),
            )
            .unwrap();
        let restored = store
            .read_private(
                original.capture_ref(),
                &CapturePassphrase::new("comparison fixture passphrase".to_owned()).unwrap(),
            )
            .unwrap();

        let comparison = compare_captures(ComparisonKind::WorkingVsFailing, &original, &restored);
        assert!(comparison.private_report().findings().is_empty());
        assert!(comparison.safe_summary().findings().is_empty());
        assert!(!comparison.safe_summary().incomplete());
        assert_eq!(comparison.safe_summary().malformed_records(), 0);
    }

    #[test]
    fn original_and_replay_compare_the_same_parser_selector_decision() {
        let exchange = ExchangeRef::from_bytes([0x31; 8]);
        let decision = ReplayDecision {
            parser: ReplayParserOutcome::Parsed,
            provider: ReplayProviderOutcome::Playable,
            streaming_data_present: true,
            returned_formats: 1,
            supported_formats: 1,
            direct_formats: 1,
            cipher_formats: 0,
            selection: ReplaySelectionOutcome::Selected { itag: 140 },
        };
        let original = capture(
            0x41,
            replayable_player_records(exchange, decision),
            SafeTerminalCategory::Success,
            true,
        );
        let replay = replay_child(0x42, [0x41; 16], exchange, 1, decision);

        let comparison = compare_captures(ComparisonKind::OriginalVsReplay, &original, &replay);

        assert!(comparison.private_report().findings().is_empty());
        assert!(comparison.safe_summary().findings().is_empty());
        assert!(!comparison.safe_summary().incomplete());
        assert_eq!(comparison.safe_summary().malformed_records(), 0);
    }

    #[test]
    fn changed_replay_decision_surfaces_semantics_without_replay_lifecycle_noise() {
        let exchange = ExchangeRef::from_bytes([0x32; 8]);
        let original_decision = ReplayDecision {
            parser: ReplayParserOutcome::Parsed,
            provider: ReplayProviderOutcome::Playable,
            streaming_data_present: true,
            returned_formats: 1,
            supported_formats: 1,
            direct_formats: 1,
            cipher_formats: 0,
            selection: ReplaySelectionOutcome::Selected { itag: 140 },
        };
        let replay_decision = ReplayDecision {
            parser: ReplayParserOutcome::Parsed,
            provider: ReplayProviderOutcome::ProviderUnavailable,
            streaming_data_present: false,
            returned_formats: 0,
            supported_formats: 0,
            direct_formats: 0,
            cipher_formats: 0,
            selection: ReplaySelectionOutcome::NotAttempted,
        };
        let original = capture(
            0x43,
            replayable_player_records(exchange, original_decision),
            SafeTerminalCategory::Failed,
            true,
        );
        let replay = replay_child(0x44, [0x43; 16], exchange, 2, replay_decision);

        let comparison = compare_captures(ComparisonKind::OriginalVsReplay, &original, &replay);
        let fields = comparison
            .safe_summary()
            .findings()
            .iter()
            .map(|finding| finding.field)
            .collect::<BTreeSet<_>>();

        for expected in [
            RegisteredField::PlayabilityStatus,
            RegisteredField::SafeFailureCategory,
            RegisteredField::StreamingDataPresent,
            RegisteredField::DirectFormatCount,
            RegisteredField::SelectedFormat,
        ] {
            assert!(fields.contains(&expected), "missing {expected:?}");
        }
        for lifecycle_noise in [
            RegisteredField::TerminalOutcome,
            RegisteredField::CancellationState,
            RegisteredField::TerminalTiming,
            RegisteredField::TransportSource,
        ] {
            assert!(!fields.contains(&lifecycle_noise));
        }
        assert!(!comparison.safe_summary().incomplete());
    }

    #[test]
    fn malformed_replay_result_is_incomplete_and_never_projects_private_bytes() {
        const PRIVATE_CANARY: &[u8] = b"private-replay-result-canary";
        let exchange = ExchangeRef::from_bytes([0x33; 8]);
        let decision = ReplayDecision {
            parser: ReplayParserOutcome::Parsed,
            provider: ReplayProviderOutcome::Playable,
            streaming_data_present: true,
            returned_formats: 1,
            supported_formats: 1,
            direct_formats: 1,
            cipher_formats: 0,
            selection: ReplaySelectionOutcome::Selected { itag: 140 },
        };
        let original = capture(
            0x45,
            replayable_player_records(exchange, decision),
            SafeTerminalCategory::Success,
            true,
        );
        let mut replay = replay_child(0x46, [0x45; 16], exchange, 1, decision);
        let mut malformed = replay.records[0].payload.expose().to_vec();
        malformed.extend_from_slice(PRIVATE_CANARY);
        replay.records[0].payload = SensitiveBytes::new(malformed);

        let comparison = compare_captures(ComparisonKind::OriginalVsReplay, &original, &replay);
        let rendered = format!("{:?}", comparison.safe_summary());

        assert!(comparison.safe_summary().incomplete());
        assert_ne!(comparison.safe_summary().malformed_records(), 0);
        assert!(!rendered.contains(std::str::from_utf8(PRIVATE_CANARY).unwrap()));
    }

    #[test]
    fn authoritative_retry_browser_media_and_terminal_are_correlated() {
        let failed = ExchangeRef::from_bytes([2; 8]);
        let selected = ExchangeRef::from_bytes([3; 8]);
        let mut records = player_records(failed, 1);
        records.push(context_record(
            7,
            Some(failed),
            EndpointRole::PlayerApi,
            ProviderClientKind::Android,
            TransportKind::NativeHttp,
            1,
            CaptureRecordKind::SelectionDecision,
            PrivatePayloadKind::SelectionDecision,
            &[
                PrivateField::text(field::OUTCOME, "provider_unavailable"),
                PrivateField::boolean(field::FALLBACK_ELIGIBLE, false),
                PrivateField::boolean(field::FALLBACK_ATTEMPTED, false),
                PrivateField::u64(field::ELAPSED_MS, 4),
            ],
        ));
        records.extend(player_records(selected, 8));
        records.push(context_record(
            14,
            Some(selected),
            EndpointRole::PlayerApi,
            ProviderClientKind::Android,
            TransportKind::NativeHttp,
            2,
            CaptureRecordKind::SelectionDecision,
            PrivatePayloadKind::SelectionDecision,
            &[
                PrivateField::text(field::OUTCOME, "browser_selected"),
                PrivateField::boolean(field::FALLBACK_ELIGIBLE, true),
                PrivateField::boolean(field::FALLBACK_ATTEMPTED, true),
                PrivateField::u64(field::SELECTED_ITAG, 140),
                PrivateField::u64(field::ELAPSED_MS, 20),
            ],
        ));
        records.push(context_record(
            15,
            Some(selected),
            EndpointRole::BrowserPlayer,
            ProviderClientKind::WebRemix,
            TransportKind::BrowserCdp,
            1,
            CaptureRecordKind::HttpResponse,
            PrivatePayloadKind::HttpResponse,
            &[
                PrivateField::u64(field::STATUS, 200),
                PrivateField::boolean(field::REDIRECTED, false),
                PrivateField::u64(field::ELAPSED_MS, 40),
            ],
        ));
        records.push(context_record(
            16,
            Some(selected),
            EndpointRole::BrowserPlayer,
            ProviderClientKind::WebRemix,
            TransportKind::BrowserCdp,
            1,
            CaptureRecordKind::BrowserExchange,
            PrivatePayloadKind::BrowserExchange,
            &[
                PrivateField::u64(field::REQUEST_ORDINAL, 1),
                PrivateField::boolean(field::FROM_DISK_CACHE, true),
                PrivateField::boolean(field::FROM_SERVICE_WORKER, false),
            ],
        ));
        records.push(context_record(
            17,
            Some(selected),
            EndpointRole::Media,
            ProviderClientKind::Unknown,
            TransportKind::MediaRange,
            1,
            CaptureRecordKind::MediaProbe,
            PrivatePayloadKind::MediaProbe,
            &[PrivateField::u64(field::STATUS, 403)],
        ));
        records.push(context_record(
            18,
            Some(selected),
            EndpointRole::BrowserMedia,
            ProviderClientKind::WebRemix,
            TransportKind::BrowserCdp,
            1,
            CaptureRecordKind::MediaProbe,
            PrivatePayloadKind::MediaProbe,
            &[
                PrivateField::u64(field::STATUS, 200),
                PrivateField::boolean(field::FROM_DISK_CACHE, true),
                PrivateField::boolean(field::FROM_SERVICE_WORKER, false),
            ],
        ));
        records.push(context_record(
            19,
            Some(failed),
            EndpointRole::BrowserPlayer,
            ProviderClientKind::WebRemix,
            TransportKind::BrowserCdp,
            2,
            CaptureRecordKind::NetworkFailure,
            PrivatePayloadKind::NetworkFailure,
            &[
                PrivateField::text(field::STAGE, "browser_player"),
                PrivateField::text(field::CATEGORY, "network"),
            ],
        ));
        records.push(context_record(
            20,
            Some(failed),
            EndpointRole::BrowserMedia,
            ProviderClientKind::WebRemix,
            TransportKind::BrowserCdp,
            2,
            CaptureRecordKind::MediaProbe,
            PrivatePayloadKind::MediaProbe,
            &[PrivateField::u64(field::STATUS, 403)],
        ));
        records.push(terminal_record(21, SafeTerminalCategory::Success));
        let capture = capture(2, records, SafeTerminalCategory::Success, true);
        let semantics = extract_capture_semantics(&capture);
        let expected = SemanticSnapshot::from_registered([
            SemanticFact::PlayabilityStatus(PlayabilityClass::Playable),
            SemanticFact::BrowserFallbackOutcome(FallbackOutcome::Succeeded),
            SemanticFact::BrowserResponseClass(ResponseClassification::Playable),
            SemanticFact::MediaHttpStatus(HttpStatusCode::new(200)),
            SemanticFact::TerminalOutcome(TerminalOutcome::Success),
        ]);
        let report = compare(
            ComparisonKind::OriginalVsReplay,
            &semantics.snapshot,
            &expected,
        );
        let fields = report
            .safe_projection()
            .iter()
            .map(|finding| finding.field)
            .collect::<BTreeSet<_>>();
        for essential in [
            RegisteredField::PlayabilityStatus,
            RegisteredField::BrowserFallbackOutcome,
            RegisteredField::BrowserResponseClass,
            RegisteredField::MediaHttpStatus,
            RegisteredField::TerminalOutcome,
        ] {
            assert!(
                !fields.contains(&essential),
                "authoritative fact differed for {essential:?}"
            );
        }
    }

    #[test]
    fn final_failed_retry_remains_authoritative_over_an_earlier_selection() {
        let earlier = ExchangeRef::from_bytes([0x51; 8]);
        let final_retry = ExchangeRef::from_bytes([0x52; 8]);
        let mut records = player_records(earlier, 1);
        records.extend([
            context_record(
                7,
                Some(final_retry),
                EndpointRole::PlayerApi,
                ProviderClientKind::Android,
                TransportKind::NativeHttp,
                2,
                CaptureRecordKind::HttpRequest,
                PrivatePayloadKind::HttpRequest,
                &[
                    PrivateField::text(field::METHOD, "POST"),
                    PrivateField::text(field::URL, "https://private.invalid/player"),
                ],
            ),
            context_record(
                8,
                Some(final_retry),
                EndpointRole::PlayerApi,
                ProviderClientKind::Android,
                TransportKind::NativeHttp,
                2,
                CaptureRecordKind::AuthSelection,
                PrivatePayloadKind::AuthSelection,
                &[
                    PrivateField::text(field::AUTH_KIND, "browser"),
                    PrivateField::text(field::VERSION_SOURCE, "discovered"),
                    PrivateField::boolean(field::PROOF_TOKEN_PRESENT, false),
                ],
            ),
            context_record(
                9,
                Some(final_retry),
                EndpointRole::PlayerApi,
                ProviderClientKind::Android,
                TransportKind::NativeHttp,
                2,
                CaptureRecordKind::HttpResponse,
                PrivatePayloadKind::HttpResponse,
                &[
                    PrivateField::u64(field::STATUS, 403),
                    PrivateField::boolean(field::REDIRECTED, false),
                    PrivateField::u64(field::ELAPSED_MS, 20),
                ],
            ),
            context_record(
                10,
                Some(final_retry),
                EndpointRole::PlayerApi,
                ProviderClientKind::Android,
                TransportKind::NativeHttp,
                2,
                CaptureRecordKind::NetworkFailure,
                PrivatePayloadKind::NetworkFailure,
                &[
                    PrivateField::text(field::STAGE, "http_status"),
                    PrivateField::text(field::CATEGORY, "authentication"),
                ],
            ),
            terminal_record(11, SafeTerminalCategory::Failed),
        ]);
        let capture = capture(0x53, records, SafeTerminalCategory::Failed, true);
        let semantics = extract_capture_semantics(&capture);
        let expected = SemanticSnapshot::from_registered([
            SemanticFact::PlayerHttpStatus(HttpStatusCode::new(403)),
            SemanticFact::SafeFailureCategory(FailureCategory::Authentication),
            SemanticFact::NativeResponseClass(ResponseClassification::Refused),
            SemanticFact::TerminalOutcome(TerminalOutcome::Failed),
        ]);

        let report = compare(
            ComparisonKind::WorkingVsFailing,
            &semantics.snapshot,
            &expected,
        );
        let fields = report
            .safe_projection()
            .iter()
            .map(|finding| finding.field)
            .collect::<BTreeSet<_>>();

        for authoritative in [
            RegisteredField::PlayerHttpStatus,
            RegisteredField::SafeFailureCategory,
            RegisteredField::NativeResponseClass,
            RegisteredField::TerminalOutcome,
        ] {
            assert!(!fields.contains(&authoritative));
        }
        assert!(
            !fields.contains(&RegisteredField::SelectedFormat),
            "the final failed retry inherited an earlier selected format"
        );
    }

    #[test]
    fn malformed_and_unknown_private_fields_never_reach_safe_summary() {
        const PRIVATE_CANARY: &[u8] = b"private-provider-canary-never-render";
        let exchange = ExchangeRef::from_bytes([4; 8]);
        let mut left_records = player_records(exchange, 1);
        left_records.push(context_record(
            7,
            Some(exchange),
            EndpointRole::PlayerApi,
            ProviderClientKind::Android,
            TransportKind::NativeHttp,
            1,
            CaptureRecordKind::NetworkFailure,
            PrivatePayloadKind::NetworkFailure,
            &[
                PrivateField::text(field::STAGE, "execute"),
                PrivateField::text(field::CATEGORY, "network"),
                PrivateField::bytes(65_000, PRIVATE_CANARY),
            ],
        ));
        left_records.push(terminal_record(8, SafeTerminalCategory::Failed));
        let mut right_records = player_records(exchange, 1);
        right_records.push(CaptureRecordV1::with_context(
            7,
            70,
            Some(exchange),
            EndpointRole::PlayerApi,
            ProviderClientKind::Android,
            TransportKind::NativeHttp,
            1,
            CaptureRecordKind::NetworkFailure,
            SensitiveBytes::new(PRIVATE_CANARY.to_vec()),
        ));
        right_records.push(terminal_record(8, SafeTerminalCategory::Failed));
        let comparison = compare_captures(
            ComparisonKind::WorkingVsFailing,
            &capture(3, left_records, SafeTerminalCategory::Failed, true),
            &capture(4, right_records, SafeTerminalCategory::Failed, true),
        );
        let rendered = format!("{:?}", comparison.safe_summary());
        assert!(!rendered.contains(std::str::from_utf8(PRIVATE_CANARY).unwrap()));
        assert!(comparison.safe_summary().incomplete());
        assert_eq!(comparison.safe_summary().malformed_records(), 1);
        assert!(comparison.safe_summary().findings().len() <= MAX_DIFF_FINDINGS);
    }

    #[test]
    fn superseded_terminal_is_preserved_even_when_late_evidence_is_absent() {
        let exchange = ExchangeRef::from_bytes([7; 8]);
        let mut records = player_records(exchange, 1);
        records.push(terminal_record(7, SafeTerminalCategory::Superseded));
        let capture = capture(7, records, SafeTerminalCategory::Superseded, true);
        let semantics = extract_capture_semantics(&capture);
        let expected = SemanticSnapshot::from_registered([
            SemanticFact::TerminalOutcome(TerminalOutcome::Superseded),
            SemanticFact::CancellationState(CancellationState::Superseded),
        ]);
        let report = compare(
            ComparisonKind::OriginalVsReplay,
            &semantics.snapshot,
            &expected,
        );
        let fields = report
            .safe_projection()
            .iter()
            .map(|finding| finding.field)
            .collect::<BTreeSet<_>>();

        assert!(!fields.contains(&RegisteredField::TerminalOutcome));
        assert!(!fields.contains(&RegisteredField::CancellationState));
    }

    #[test]
    fn extraction_and_comparison_stay_within_the_local_budget() {
        let exchange = ExchangeRef::from_bytes([5; 8]);
        let mut records = player_records(exchange, 1);
        for sequence in 7..=250 {
            records.push(context_record(
                sequence,
                Some(exchange),
                EndpointRole::Media,
                ProviderClientKind::Unknown,
                TransportKind::MediaRange,
                u8::try_from(sequence % 8).unwrap(),
                CaptureRecordKind::MediaProbe,
                PrivatePayloadKind::MediaProbe,
                &[
                    PrivateField::u64(field::STATUS, 206),
                    PrivateField::boolean(field::CONTENT_RANGE_PRESENT, true),
                    PrivateField::u64(field::ELAPSED_MS, 1),
                ],
            ));
        }
        records.push(terminal_record(251, SafeTerminalCategory::Success));
        let left = capture(5, records, SafeTerminalCategory::Success, true);
        let right = capture(
            6,
            {
                let mut records = player_records(exchange, 1);
                records.push(terminal_record(7, SafeTerminalCategory::Success));
                records
            },
            SafeTerminalCategory::Success,
            true,
        );
        let started = Instant::now();
        let comparison = compare_captures(ComparisonKind::WorkingVsFailing, &left, &right);
        let elapsed = started.elapsed();
        eprintln!(
            "phase7.performance.typed_comparison_ms={}",
            elapsed.as_millis()
        );
        assert!(elapsed < Duration::from_millis(250));
        assert!(comparison.safe_summary().findings().len() <= MAX_DIFF_FINDINGS);
    }
}
