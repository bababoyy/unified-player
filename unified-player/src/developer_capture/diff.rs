use std::collections::{BTreeMap, BTreeSet};

use sha2::{Digest as _, Sha256};

pub(crate) const COMPARISON_SCHEMA_VERSION: u16 = 1;
pub(crate) const MAX_DIFF_FINDINGS: usize = 64;
const MAX_INPUT_FACTS: usize = 512;
const MAX_VALUES_PER_FIELD: usize = 16;
const PRIVATE_VALUE_DOMAIN: &[u8] = b"unified-player-private-diff-v1";

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ComparisonKind {
    WorkingVsFailing,
    OriginalVsReplay,
}

impl ComparisonKind {
    pub(crate) const fn left_role(self) -> ComparisonRole {
        match self {
            Self::WorkingVsFailing => ComparisonRole::Working,
            Self::OriginalVsReplay => ComparisonRole::Original,
        }
    }

    pub(crate) const fn right_role(self) -> ComparisonRole {
        match self {
            Self::WorkingVsFailing => ComparisonRole::Failing,
            Self::OriginalVsReplay => ComparisonRole::Replay,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ComparisonRole {
    Working,
    Failing,
    Original,
    Replay,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum VolatilityPolicy {
    Compare,
    NormalizeOrder,
    Ignore,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub(crate) enum RegisteredField {
    PlayerHttpStatus,
    PlayerRedirectClass,
    PlayerClientKind,
    ClientVersionPolicy,
    AuthenticationKind,
    ProofTokenPresent,
    PlayabilityStatus,
    SafeFailureCategory,
    StreamingDataPresent,
    ReturnedFormatCount,
    SupportedFormatCount,
    DirectFormatCount,
    CipherFormatCount,
    FormatIdentifier,
    Container,
    Codec,
    BitrateBucket,
    SelectedFormat,
    BrowserFallbackEligible,
    BrowserFallbackOutcome,
    NativeResponseClass,
    BrowserResponseClass,
    MediaHttpStatus,
    ContentRangeClass,
    TransportSource,
    ParserTiming,
    SelectorTiming,
    TransportTiming,
    DecoderTiming,
    TerminalTiming,
    CancellationState,
    RetryCount,
    TerminalOutcome,
}

impl RegisteredField {
    pub(crate) const fn volatility_policy(self) -> VolatilityPolicy {
        match self {
            Self::FormatIdentifier | Self::Container | Self::Codec | Self::BitrateBucket => {
                VolatilityPolicy::NormalizeOrder
            }
            _ => VolatilityPolicy::Compare,
        }
    }

    const fn explanatory_rank(self) -> u16 {
        match self {
            Self::PlayabilityStatus => 10,
            Self::SafeFailureCategory => 11,
            Self::StreamingDataPresent => 12,
            Self::PlayerHttpStatus => 20,
            Self::MediaHttpStatus => 21,
            Self::ContentRangeClass => 22,
            Self::DirectFormatCount => 30,
            Self::CipherFormatCount => 31,
            Self::SupportedFormatCount => 32,
            Self::ReturnedFormatCount => 33,
            Self::SelectedFormat => 34,
            Self::FormatIdentifier => 35,
            Self::Container => 36,
            Self::Codec => 37,
            Self::BitrateBucket => 38,
            Self::BrowserFallbackOutcome => 40,
            Self::BrowserFallbackEligible => 41,
            Self::NativeResponseClass => 42,
            Self::BrowserResponseClass => 43,
            Self::AuthenticationKind => 50,
            Self::ProofTokenPresent => 51,
            Self::PlayerClientKind => 52,
            Self::ClientVersionPolicy => 53,
            Self::PlayerRedirectClass => 54,
            Self::TransportSource => 60,
            Self::CancellationState => 70,
            Self::TerminalOutcome => 71,
            Self::RetryCount => 72,
            Self::ParserTiming => 80,
            Self::SelectorTiming => 81,
            Self::TransportTiming => 82,
            Self::DecoderTiming => 83,
            Self::TerminalTiming => 84,
        }
    }

    const fn retention_rank(self) -> u8 {
        match self {
            Self::TerminalOutcome => 0,
            Self::CancellationState => 1,
            Self::SafeFailureCategory => 2,
            Self::PlayabilityStatus => 3,
            Self::StreamingDataPresent => 4,
            Self::PlayerHttpStatus | Self::MediaHttpStatus | Self::ContentRangeClass => 5,
            Self::SelectedFormat
            | Self::BrowserFallbackEligible
            | Self::BrowserFallbackOutcome
            | Self::NativeResponseClass
            | Self::BrowserResponseClass => 6,
            Self::AuthenticationKind
            | Self::ProofTokenPresent
            | Self::PlayerClientKind
            | Self::ClientVersionPolicy => 7,
            Self::ReturnedFormatCount
            | Self::SupportedFormatCount
            | Self::DirectFormatCount
            | Self::CipherFormatCount => 8,
            Self::TransportSource | Self::RetryCount | Self::PlayerRedirectClass => 9,
            Self::ParserTiming
            | Self::SelectorTiming
            | Self::TransportTiming
            | Self::DecoderTiming
            | Self::TerminalTiming => 10,
            Self::FormatIdentifier | Self::Container | Self::Codec | Self::BitrateBucket => 11,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum VolatileField {
    CaptureTimestamp,
    WallClock,
    GeneratedRequestId,
    AuthorizationValue,
    CookieValue,
    ProofTokenValue,
    VisitorValue,
    ClientVersionValue,
    SignedMediaSignature,
    SignedMediaExpiration,
    SignedMediaHost,
    QueryOrdering,
    RangeCounter,
    RequestNumber,
    BufferingHint,
    ProviderTrackingValue,
    ProviderExperimentValue,
}

impl VolatileField {
    pub(crate) const fn volatility_policy(self) -> VolatilityPolicy {
        let _ = self;
        VolatilityPolicy::Ignore
    }
}

#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub(crate) struct HttpStatusCode(u16);

impl HttpStatusCode {
    pub(crate) const fn new(value: u16) -> Self {
        Self(value)
    }

    pub(super) const fn class(self) -> HttpStatusClass {
        match self.0 {
            200..=299 => HttpStatusClass::Success,
            401 => HttpStatusClass::Unauthorized,
            403 => HttpStatusClass::Forbidden,
            429 => HttpStatusClass::RateLimited,
            400..=499 => HttpStatusClass::OtherClientError,
            500..=599 => HttpStatusClass::ServerError,
            _ => HttpStatusClass::Other,
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub(crate) struct PrivateFormatId(u64);

impl PrivateFormatId {
    pub(crate) const fn new(value: u64) -> Self {
        Self(value)
    }
}

#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub(crate) struct BoundedCount(u16);

impl BoundedCount {
    pub(crate) fn new(value: usize) -> Self {
        Self(u16::try_from(value).unwrap_or(u16::MAX))
    }

    pub(super) const fn value(self) -> u16 {
        self.0
    }

    const fn bucket(self) -> CountBucket {
        match self.0 {
            0 => CountBucket::None,
            1 => CountBucket::One,
            2..=4 => CountBucket::Several,
            _ => CountBucket::Many,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub(crate) enum RedirectClass {
    None,
    Redirected,
    SameOrigin,
    CrossOrigin,
    Rejected,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub(crate) enum PlayerClientKind {
    Web,
    WebRemix,
    Android,
    Ios,
    TvHtml5,
    Unknown,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub(crate) enum ClientVersionPolicy {
    Static,
    Cached,
    Discovered,
    Fallback,
    Unknown,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub(crate) enum AuthenticationKind {
    None,
    Browser,
    OAuthBearer,
    Unavailable,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub(crate) enum PlayabilityClass {
    Playable,
    LoginRequired,
    AgeConsentOrRegion,
    Unavailable,
    Other,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub(crate) enum FailureCategory {
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

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub(crate) enum ContainerKind {
    Mp4,
    WebM,
    Other,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub(crate) enum CodecKind {
    Aac,
    Opus,
    Vorbis,
    Other,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub(crate) enum BitrateBucket {
    Low,
    Medium,
    High,
    Unknown,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub(crate) enum FallbackOutcome {
    NotEligible,
    Eligible,
    Attempted,
    Succeeded,
    Failed,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub(crate) enum ResponseClassification {
    Playable,
    Refused,
    Malformed,
    TransportFailure,
    Cancelled,
    Unknown,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub(crate) enum ContentRangeClass {
    Valid,
    Missing,
    WrongStart,
    Invalid,
    NotApplicable,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub(crate) enum TransportSource {
    NativeHttp,
    Browser,
    BrowserCache,
    ServiceWorker,
    OfflineReplay,
    FreshReplay,
    Unknown,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub(crate) enum TimingBucket {
    Immediate,
    Fast,
    Moderate,
    Slow,
    VerySlow,
    TimedOut,
    Unavailable,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub(crate) enum CancellationState {
    NotCancelled,
    Cancelled,
    Superseded,
    Shutdown,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub(crate) enum TerminalOutcome {
    Success,
    Failed,
    Cancelled,
    Superseded,
    TimedOut,
    Panicked,
}

#[derive(Clone, PartialEq, Eq)]
pub(crate) enum SemanticFact {
    PlayerHttpStatus(HttpStatusCode),
    PlayerRedirectClass(RedirectClass),
    PlayerClientKind(PlayerClientKind),
    ClientVersionPolicy(ClientVersionPolicy),
    AuthenticationKind(AuthenticationKind),
    ProofTokenPresent(bool),
    PlayabilityStatus(PlayabilityClass),
    SafeFailureCategory(FailureCategory),
    StreamingDataPresent(bool),
    ReturnedFormatCount(BoundedCount),
    SupportedFormatCount(BoundedCount),
    DirectFormatCount(BoundedCount),
    CipherFormatCount(BoundedCount),
    FormatIdentifier(PrivateFormatId),
    Container(ContainerKind),
    Codec(CodecKind),
    BitrateBucket(BitrateBucket),
    SelectedFormat(Option<PrivateFormatId>),
    BrowserFallbackEligible(bool),
    BrowserFallbackOutcome(FallbackOutcome),
    NativeResponseClass(ResponseClassification),
    BrowserResponseClass(ResponseClassification),
    MediaHttpStatus(HttpStatusCode),
    ContentRangeClass(ContentRangeClass),
    TransportSource(TransportSource),
    ParserTiming(TimingBucket),
    SelectorTiming(TimingBucket),
    TransportTiming(TimingBucket),
    DecoderTiming(TimingBucket),
    TerminalTiming(TimingBucket),
    CancellationState(CancellationState),
    RetryCount(BoundedCount),
    TerminalOutcome(TerminalOutcome),
}

impl SemanticFact {
    pub(crate) const fn field(&self) -> RegisteredField {
        match self {
            Self::PlayerHttpStatus(_) => RegisteredField::PlayerHttpStatus,
            Self::PlayerRedirectClass(_) => RegisteredField::PlayerRedirectClass,
            Self::PlayerClientKind(_) => RegisteredField::PlayerClientKind,
            Self::ClientVersionPolicy(_) => RegisteredField::ClientVersionPolicy,
            Self::AuthenticationKind(_) => RegisteredField::AuthenticationKind,
            Self::ProofTokenPresent(_) => RegisteredField::ProofTokenPresent,
            Self::PlayabilityStatus(_) => RegisteredField::PlayabilityStatus,
            Self::SafeFailureCategory(_) => RegisteredField::SafeFailureCategory,
            Self::StreamingDataPresent(_) => RegisteredField::StreamingDataPresent,
            Self::ReturnedFormatCount(_) => RegisteredField::ReturnedFormatCount,
            Self::SupportedFormatCount(_) => RegisteredField::SupportedFormatCount,
            Self::DirectFormatCount(_) => RegisteredField::DirectFormatCount,
            Self::CipherFormatCount(_) => RegisteredField::CipherFormatCount,
            Self::FormatIdentifier(_) => RegisteredField::FormatIdentifier,
            Self::Container(_) => RegisteredField::Container,
            Self::Codec(_) => RegisteredField::Codec,
            Self::BitrateBucket(_) => RegisteredField::BitrateBucket,
            Self::SelectedFormat(_) => RegisteredField::SelectedFormat,
            Self::BrowserFallbackEligible(_) => RegisteredField::BrowserFallbackEligible,
            Self::BrowserFallbackOutcome(_) => RegisteredField::BrowserFallbackOutcome,
            Self::NativeResponseClass(_) => RegisteredField::NativeResponseClass,
            Self::BrowserResponseClass(_) => RegisteredField::BrowserResponseClass,
            Self::MediaHttpStatus(_) => RegisteredField::MediaHttpStatus,
            Self::ContentRangeClass(_) => RegisteredField::ContentRangeClass,
            Self::TransportSource(_) => RegisteredField::TransportSource,
            Self::ParserTiming(_) => RegisteredField::ParserTiming,
            Self::SelectorTiming(_) => RegisteredField::SelectorTiming,
            Self::TransportTiming(_) => RegisteredField::TransportTiming,
            Self::DecoderTiming(_) => RegisteredField::DecoderTiming,
            Self::TerminalTiming(_) => RegisteredField::TerminalTiming,
            Self::CancellationState(_) => RegisteredField::CancellationState,
            Self::RetryCount(_) => RegisteredField::RetryCount,
            Self::TerminalOutcome(_) => RegisteredField::TerminalOutcome,
        }
    }

    fn canonical_value(&self) -> CanonicalValue {
        match self {
            Self::PlayerHttpStatus(value) | Self::MediaHttpStatus(value) => {
                CanonicalValue::HttpStatus(value.0)
            }
            Self::PlayerRedirectClass(value) => CanonicalValue::Redirect(*value),
            Self::PlayerClientKind(value) => CanonicalValue::Client(*value),
            Self::ClientVersionPolicy(value) => CanonicalValue::VersionPolicy(*value),
            Self::AuthenticationKind(value) => CanonicalValue::Authentication(*value),
            Self::ProofTokenPresent(value)
            | Self::StreamingDataPresent(value)
            | Self::BrowserFallbackEligible(value) => CanonicalValue::Boolean(*value),
            Self::PlayabilityStatus(value) => CanonicalValue::Playability(*value),
            Self::SafeFailureCategory(value) => CanonicalValue::Failure(*value),
            Self::ReturnedFormatCount(value)
            | Self::SupportedFormatCount(value)
            | Self::DirectFormatCount(value)
            | Self::CipherFormatCount(value)
            | Self::RetryCount(value) => CanonicalValue::Count(value.0),
            Self::FormatIdentifier(value) => CanonicalValue::PrivateFormat(value.0),
            Self::Container(value) => CanonicalValue::Container(*value),
            Self::Codec(value) => CanonicalValue::Codec(*value),
            Self::BitrateBucket(value) => CanonicalValue::Bitrate(*value),
            Self::SelectedFormat(value) => {
                CanonicalValue::SelectedPrivateFormat(value.map(|value| value.0))
            }
            Self::BrowserFallbackOutcome(value) => CanonicalValue::Fallback(*value),
            Self::NativeResponseClass(value) | Self::BrowserResponseClass(value) => {
                CanonicalValue::Response(*value)
            }
            Self::ContentRangeClass(value) => CanonicalValue::ContentRange(*value),
            Self::TransportSource(value) => CanonicalValue::Transport(*value),
            Self::ParserTiming(value)
            | Self::SelectorTiming(value)
            | Self::TransportTiming(value)
            | Self::DecoderTiming(value)
            | Self::TerminalTiming(value) => CanonicalValue::Timing(*value),
            Self::CancellationState(value) => CanonicalValue::Cancellation(*value),
            Self::TerminalOutcome(value) => CanonicalValue::Terminal(*value),
        }
    }
}

#[derive(Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub(crate) struct PrivateOpaqueValue([u8; 32]);

impl PrivateOpaqueValue {
    pub(crate) fn from_private_bytes(value: &[u8]) -> Self {
        let mut hasher = Sha256::new();
        hasher.update(PRIVATE_VALUE_DOMAIN);
        hasher.update(u64::try_from(value.len()).unwrap_or(u64::MAX).to_be_bytes());
        hasher.update(value);
        Self(hasher.finalize().into())
    }
}

#[derive(Clone, PartialEq, Eq)]
pub(crate) struct UnknownPrivateFact {
    slot: u16,
    value: PrivateOpaqueValue,
}

impl UnknownPrivateFact {
    pub(crate) fn new(slot: u16, private_value: &[u8]) -> Self {
        Self {
            slot,
            value: PrivateOpaqueValue::from_private_bytes(private_value),
        }
    }
}

#[derive(Clone, PartialEq, Eq)]
pub(crate) enum VolatileFact {
    CaptureTimestamp(u64),
    WallClock(u64),
    GeneratedRequestId(PrivateOpaqueValue),
    AuthorizationValue(PrivateOpaqueValue),
    CookieValue(PrivateOpaqueValue),
    ProofTokenValue(PrivateOpaqueValue),
    VisitorValue(PrivateOpaqueValue),
    ClientVersionValue(PrivateOpaqueValue),
    SignedMediaSignature(PrivateOpaqueValue),
    SignedMediaExpiration(u64),
    SignedMediaHost(PrivateOpaqueValue),
    QueryOrdering(PrivateOpaqueValue),
    RangeCounter(u64),
    RequestNumber(u64),
    BufferingHint(u64),
    ProviderTrackingValue(PrivateOpaqueValue),
    ProviderExperimentValue(PrivateOpaqueValue),
}

impl VolatileFact {
    pub(crate) const fn field(&self) -> VolatileField {
        match self {
            Self::CaptureTimestamp(_) => VolatileField::CaptureTimestamp,
            Self::WallClock(_) => VolatileField::WallClock,
            Self::GeneratedRequestId(_) => VolatileField::GeneratedRequestId,
            Self::AuthorizationValue(_) => VolatileField::AuthorizationValue,
            Self::CookieValue(_) => VolatileField::CookieValue,
            Self::ProofTokenValue(_) => VolatileField::ProofTokenValue,
            Self::VisitorValue(_) => VolatileField::VisitorValue,
            Self::ClientVersionValue(_) => VolatileField::ClientVersionValue,
            Self::SignedMediaSignature(_) => VolatileField::SignedMediaSignature,
            Self::SignedMediaExpiration(_) => VolatileField::SignedMediaExpiration,
            Self::SignedMediaHost(_) => VolatileField::SignedMediaHost,
            Self::QueryOrdering(_) => VolatileField::QueryOrdering,
            Self::RangeCounter(_) => VolatileField::RangeCounter,
            Self::RequestNumber(_) => VolatileField::RequestNumber,
            Self::BufferingHint(_) => VolatileField::BufferingHint,
            Self::ProviderTrackingValue(_) => VolatileField::ProviderTrackingValue,
            Self::ProviderExperimentValue(_) => VolatileField::ProviderExperimentValue,
        }
    }
}

#[derive(Clone, PartialEq, Eq)]
pub(crate) enum ComparisonFact {
    Registered(SemanticFact),
    Volatile(VolatileFact),
    UnknownPrivate(UnknownPrivateFact),
}

impl From<SemanticFact> for ComparisonFact {
    fn from(value: SemanticFact) -> Self {
        Self::Registered(value)
    }
}

#[derive(Clone, PartialEq, Eq)]
pub(crate) struct SemanticSnapshot {
    facts: Vec<ComparisonFact>,
    input_incomplete: bool,
}

impl SemanticSnapshot {
    pub(crate) fn from_facts(facts: impl IntoIterator<Item = ComparisonFact>) -> Self {
        let mut registered = BTreeMap::<RegisteredField, Vec<ComparisonFact>>::new();
        let mut unknown = BTreeMap::<u16, Vec<ComparisonFact>>::new();
        let mut input_incomplete = false;
        for fact in facts {
            match fact {
                ComparisonFact::Registered(fact) => {
                    let values = registered.entry(fact.field()).or_default();
                    let fact = ComparisonFact::Registered(fact);
                    if values.contains(&fact) {
                        continue;
                    }
                    if values.len() < MAX_VALUES_PER_FIELD {
                        values.push(fact);
                    } else {
                        input_incomplete = true;
                    }
                }
                ComparisonFact::Volatile(_) => {
                    // Volatile facts never participate in comparison, so retaining them
                    // could only crowd meaningful terminal evidence out of the bound.
                }
                ComparisonFact::UnknownPrivate(fact) => {
                    let values = unknown.entry(fact.slot).or_default();
                    let fact = ComparisonFact::UnknownPrivate(fact);
                    if values.contains(&fact) {
                        continue;
                    }
                    if values.len() < MAX_VALUES_PER_FIELD {
                        values.push(fact);
                    } else {
                        input_incomplete = true;
                    }
                }
            }
        }

        let mut registered = registered.into_iter().collect::<Vec<_>>();
        registered.sort_by_key(|(field, _)| (field.retention_rank(), *field));
        let mut retained = Vec::with_capacity(MAX_INPUT_FACTS);
        for (_, facts) in registered {
            retain_bounded(&mut retained, facts, &mut input_incomplete);
        }
        for (_, facts) in unknown {
            retain_bounded(&mut retained, facts, &mut input_incomplete);
        }
        Self {
            facts: retained,
            input_incomplete,
        }
    }

    pub(crate) fn from_registered(facts: impl IntoIterator<Item = SemanticFact>) -> Self {
        Self::from_facts(facts.into_iter().map(ComparisonFact::Registered))
    }

    pub(crate) const fn input_incomplete(&self) -> bool {
        self.input_incomplete
    }

    pub(super) fn registered_facts(&self) -> impl Iterator<Item = &SemanticFact> {
        self.facts.iter().filter_map(|fact| match fact {
            ComparisonFact::Registered(fact) => Some(fact),
            ComparisonFact::Volatile(_) | ComparisonFact::UnknownPrivate(_) => None,
        })
    }

    pub(super) fn has_unknown_private(&self) -> bool {
        self.facts
            .iter()
            .any(|fact| matches!(fact, ComparisonFact::UnknownPrivate(_)))
    }
}

fn retain_bounded(
    retained: &mut Vec<ComparisonFact>,
    facts: Vec<ComparisonFact>,
    incomplete: &mut bool,
) {
    let available = MAX_INPUT_FACTS.saturating_sub(retained.len());
    if facts.len() > available {
        *incomplete = true;
    }
    retained.extend(facts.into_iter().take(available));
}

#[derive(Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub(super) enum CanonicalValue {
    Boolean(bool),
    HttpStatus(u16),
    Redirect(RedirectClass),
    Client(PlayerClientKind),
    VersionPolicy(ClientVersionPolicy),
    Authentication(AuthenticationKind),
    Playability(PlayabilityClass),
    Failure(FailureCategory),
    Count(u16),
    PrivateFormat(u64),
    SelectedPrivateFormat(Option<u64>),
    PrivateOpaqueCount(u16),
    Container(ContainerKind),
    Codec(CodecKind),
    Bitrate(BitrateBucket),
    Fallback(FallbackOutcome),
    Response(ResponseClassification),
    ContentRange(ContentRangeClass),
    Transport(TransportSource),
    Timing(TimingBucket),
    Cancellation(CancellationState),
    Terminal(TerminalOutcome),
}

impl CanonicalValue {
    const fn value_class(&self) -> ValueClass {
        match self {
            Self::Boolean(_) => ValueClass::Boolean,
            Self::HttpStatus(_) => ValueClass::HttpStatus,
            Self::Redirect(_) => ValueClass::Redirect,
            Self::Client(_) => ValueClass::Client,
            Self::VersionPolicy(_) => ValueClass::VersionPolicy,
            Self::Authentication(_) => ValueClass::Authentication,
            Self::Playability(_) => ValueClass::Playability,
            Self::Failure(_) => ValueClass::FailureCategory,
            Self::Count(_) => ValueClass::Count,
            Self::PrivateFormat(_)
            | Self::SelectedPrivateFormat(_)
            | Self::PrivateOpaqueCount(_) => ValueClass::PrivateValue,
            Self::Container(_) => ValueClass::Container,
            Self::Codec(_) => ValueClass::Codec,
            Self::Bitrate(_) => ValueClass::Bitrate,
            Self::Fallback(_) => ValueClass::Fallback,
            Self::Response(_) => ValueClass::Response,
            Self::ContentRange(_) => ValueClass::ContentRange,
            Self::Transport(_) => ValueClass::Transport,
            Self::Timing(_) => ValueClass::Timing,
            Self::Cancellation(_) => ValueClass::Cancellation,
            Self::Terminal(_) => ValueClass::Terminal,
        }
    }

    fn safe_value(&self) -> SafeValue {
        match self {
            Self::Boolean(value) => SafeValue::Boolean(*value),
            Self::HttpStatus(value) => SafeValue::HttpStatus(HttpStatusCode(*value).class()),
            Self::Redirect(value) => SafeValue::Redirect(*value),
            Self::Client(value) => SafeValue::Client(*value),
            Self::VersionPolicy(value) => SafeValue::VersionPolicy(*value),
            Self::Authentication(value) => SafeValue::Authentication(*value),
            Self::Playability(value) => SafeValue::Playability(*value),
            Self::Failure(value) => SafeValue::FailureCategory(*value),
            Self::Count(value) => SafeValue::Count(BoundedCount(*value).bucket()),
            Self::PrivateFormat(_)
            | Self::SelectedPrivateFormat(Some(_))
            | Self::PrivateOpaqueCount(1..) => SafeValue::PrivateValuePresent,
            Self::SelectedPrivateFormat(None) | Self::PrivateOpaqueCount(0) => SafeValue::Missing,
            Self::Container(value) => SafeValue::Container(*value),
            Self::Codec(value) => SafeValue::Codec(*value),
            Self::Bitrate(value) => SafeValue::Bitrate(*value),
            Self::Fallback(value) => SafeValue::Fallback(*value),
            Self::Response(value) => SafeValue::Response(*value),
            Self::ContentRange(value) => SafeValue::ContentRange(*value),
            Self::Transport(value) => SafeValue::Transport(*value),
            Self::Timing(value) => SafeValue::Timing(*value),
            Self::Cancellation(value) => SafeValue::Cancellation(*value),
            Self::Terminal(value) => SafeValue::Terminal(*value),
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub(crate) enum HttpStatusClass {
    Success,
    Unauthorized,
    Forbidden,
    RateLimited,
    OtherClientError,
    ServerError,
    Other,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub(crate) enum CountBucket {
    None,
    One,
    Several,
    Many,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ValueClass {
    Missing,
    Multiple,
    Boolean,
    HttpStatus,
    Redirect,
    Client,
    VersionPolicy,
    Authentication,
    Playability,
    FailureCategory,
    Count,
    PrivateValue,
    Container,
    Codec,
    Bitrate,
    Fallback,
    Response,
    ContentRange,
    Transport,
    Timing,
    Cancellation,
    Terminal,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum SafeValue {
    Missing,
    Multiple,
    Boolean(bool),
    HttpStatus(HttpStatusClass),
    Redirect(RedirectClass),
    Client(PlayerClientKind),
    VersionPolicy(ClientVersionPolicy),
    Authentication(AuthenticationKind),
    Playability(PlayabilityClass),
    FailureCategory(FailureCategory),
    Count(CountBucket),
    PrivateValuePresent,
    Container(ContainerKind),
    Codec(CodecKind),
    Bitrate(BitrateBucket),
    Fallback(FallbackOutcome),
    Response(ResponseClassification),
    ContentRange(ContentRangeClass),
    Transport(TransportSource),
    Timing(TimingBucket),
    Cancellation(CancellationState),
    Terminal(TerminalOutcome),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) enum DiffSeverity {
    Critical,
    High,
    Medium,
    Low,
    Informational,
}

impl DiffSeverity {
    const fn rank(self) -> u8 {
        match self {
            Self::Critical => 0,
            Self::High => 1,
            Self::Medium => 2,
            Self::Low => 3,
            Self::Informational => 4,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) enum DiffCategory {
    ProviderResponse,
    Authentication,
    FormatSelection,
    Fallback,
    MediaTransport,
    Performance,
    Lifecycle,
    PrivateUnknown,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
#[allow(clippy::enum_variant_names)]
pub(crate) enum FixedExplanation {
    PlayerHttpStatusChanged,
    RedirectBehaviorChanged,
    PlayerClientChanged,
    ClientVersionPolicyChanged,
    AuthenticationChanged,
    ProofTokenPresenceChanged,
    PlayabilityChanged,
    FailureCategoryChanged,
    StreamingDataPresenceChanged,
    ReturnedFormatCountChanged,
    SupportedFormatCountChanged,
    DirectFormatCountChanged,
    CipherFormatCountChanged,
    FormatInventoryChanged,
    ContainerChanged,
    CodecChanged,
    BitrateClassChanged,
    SelectedFormatChanged,
    FallbackEligibilityChanged,
    FallbackOutcomeChanged,
    NativeResponseChanged,
    BrowserResponseChanged,
    MediaHttpStatusChanged,
    ContentRangeChanged,
    TransportSourceChanged,
    ParserTimingChanged,
    SelectorTimingChanged,
    TransportTimingChanged,
    DecoderTimingChanged,
    TerminalTimingChanged,
    CancellationChanged,
    RetryCountChanged,
    TerminalOutcomeChanged,
    UnknownPrivateFactChanged,
}

#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum PrivateLocation {
    Registered(RegisteredField),
    Unknown(u16),
}

#[derive(Clone, PartialEq, Eq)]
pub(crate) struct PrivateDiffFinding {
    category: DiffCategory,
    severity: DiffSeverity,
    location: PrivateLocation,
    left: Vec<CanonicalValue>,
    right: Vec<CanonicalValue>,
    explanation: FixedExplanation,
}

impl PrivateDiffFinding {
    pub(crate) const fn category(&self) -> DiffCategory {
        self.category
    }

    pub(crate) const fn severity(&self) -> DiffSeverity {
        self.severity
    }

    pub(crate) const fn registered_field(&self) -> Option<RegisteredField> {
        match self.location {
            PrivateLocation::Registered(field) => Some(field),
            PrivateLocation::Unknown(_) => None,
        }
    }

    pub(super) const fn unknown_slot(&self) -> Option<u16> {
        match self.location {
            PrivateLocation::Registered(_) => None,
            PrivateLocation::Unknown(slot) => Some(slot),
        }
    }

    pub(crate) const fn explanation(&self) -> FixedExplanation {
        self.explanation
    }

    pub(crate) fn left_class(&self) -> ValueClass {
        value_class(&self.left)
    }

    pub(crate) fn right_class(&self) -> ValueClass {
        value_class(&self.right)
    }

    pub(super) fn private_values(&self) -> (&[CanonicalValue], &[CanonicalValue]) {
        (&self.left, &self.right)
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct SafeDiffFinding {
    pub(crate) category: DiffCategory,
    pub(crate) severity: DiffSeverity,
    pub(crate) field: RegisteredField,
    pub(crate) left_class: ValueClass,
    pub(crate) right_class: ValueClass,
    pub(crate) left: SafeValue,
    pub(crate) right: SafeValue,
    pub(crate) explanation: FixedExplanation,
}

#[derive(Clone, PartialEq, Eq)]
pub(crate) struct ComparisonReportV1 {
    schema_version: u16,
    kind: ComparisonKind,
    findings: Vec<PrivateDiffFinding>,
    safe_projection: Vec<SafeDiffFinding>,
    incomplete: bool,
    dropped_findings: u16,
}

impl ComparisonReportV1 {
    pub(crate) const fn schema_version(&self) -> u16 {
        self.schema_version
    }

    pub(crate) const fn kind(&self) -> ComparisonKind {
        self.kind
    }

    pub(crate) const fn left_role(&self) -> ComparisonRole {
        self.kind.left_role()
    }

    pub(crate) const fn right_role(&self) -> ComparisonRole {
        self.kind.right_role()
    }

    pub(crate) fn findings(&self) -> &[PrivateDiffFinding] {
        &self.findings
    }

    pub(crate) fn safe_projection(&self) -> &[SafeDiffFinding] {
        &self.safe_projection
    }

    pub(crate) const fn incomplete(&self) -> bool {
        self.incomplete
    }

    pub(crate) const fn dropped_findings(&self) -> u16 {
        self.dropped_findings
    }
}

#[derive(Default)]
struct CanonicalSnapshot {
    registered: BTreeMap<RegisteredField, Vec<CanonicalValue>>,
    unknown: BTreeMap<u16, Vec<PrivateOpaqueValue>>,
    incomplete: bool,
}

pub(crate) fn compare(
    kind: ComparisonKind,
    left: &SemanticSnapshot,
    right: &SemanticSnapshot,
) -> ComparisonReportV1 {
    let left = canonicalize(left);
    let right = canonicalize(right);
    let mut findings = Vec::new();

    let registered_fields = left
        .registered
        .keys()
        .chain(right.registered.keys())
        .copied()
        .collect::<BTreeSet<_>>();
    for field in registered_fields {
        let left_values = left.registered.get(&field).cloned().unwrap_or_default();
        let right_values = right.registered.get(&field).cloned().unwrap_or_default();
        if left_values != right_values {
            findings.push(private_registered_finding(field, left_values, right_values));
        }
    }

    let unknown_slots = left
        .unknown
        .keys()
        .chain(right.unknown.keys())
        .copied()
        .collect::<BTreeSet<_>>();
    for slot in unknown_slots {
        let left_values = left.unknown.get(&slot).cloned().unwrap_or_default();
        let right_values = right.unknown.get(&slot).cloned().unwrap_or_default();
        if left_values != right_values {
            findings.push(PrivateDiffFinding {
                category: DiffCategory::PrivateUnknown,
                severity: DiffSeverity::Informational,
                location: PrivateLocation::Unknown(slot),
                left: opaque_classes(left_values.len()),
                right: opaque_classes(right_values.len()),
                explanation: FixedExplanation::UnknownPrivateFactChanged,
            });
        }
    }

    findings.sort_by_key(finding_sort_key);
    findings.dedup_by(|left, right| {
        left.location == right.location
            && left.left == right.left
            && left.right == right.right
            && left.explanation == right.explanation
    });

    let dropped = findings.len().saturating_sub(MAX_DIFF_FINDINGS);
    findings.truncate(MAX_DIFF_FINDINGS);
    let safe_projection = findings.iter().filter_map(safe_finding).collect::<Vec<_>>();
    ComparisonReportV1 {
        schema_version: COMPARISON_SCHEMA_VERSION,
        kind,
        safe_projection,
        incomplete: left.incomplete || right.incomplete || dropped > 0,
        dropped_findings: u16::try_from(dropped).unwrap_or(u16::MAX),
        findings,
    }
}

fn canonicalize(snapshot: &SemanticSnapshot) -> CanonicalSnapshot {
    let mut output = CanonicalSnapshot {
        incomplete: snapshot.input_incomplete,
        ..CanonicalSnapshot::default()
    };
    for fact in &snapshot.facts {
        match fact {
            ComparisonFact::Registered(fact) => {
                let field = fact.field();
                if field.volatility_policy() != VolatilityPolicy::Ignore {
                    output
                        .registered
                        .entry(field)
                        .or_default()
                        .push(fact.canonical_value());
                }
            }
            ComparisonFact::Volatile(fact) => {
                debug_assert_eq!(fact.field().volatility_policy(), VolatilityPolicy::Ignore);
            }
            ComparisonFact::UnknownPrivate(fact) => {
                output
                    .unknown
                    .entry(fact.slot)
                    .or_default()
                    .push(fact.value.clone());
            }
        }
    }
    for values in output.registered.values_mut() {
        canonicalize_values(values, &mut output.incomplete);
    }
    for values in output.unknown.values_mut() {
        values.sort_unstable();
        values.dedup();
        if values.len() > MAX_VALUES_PER_FIELD {
            values.truncate(MAX_VALUES_PER_FIELD);
            output.incomplete = true;
        }
    }
    output
}

fn canonicalize_values(values: &mut Vec<CanonicalValue>, incomplete: &mut bool) {
    values.sort_unstable();
    values.dedup();
    if values.len() > MAX_VALUES_PER_FIELD {
        values.truncate(MAX_VALUES_PER_FIELD);
        *incomplete = true;
    }
}

fn private_registered_finding(
    field: RegisteredField,
    left: Vec<CanonicalValue>,
    right: Vec<CanonicalValue>,
) -> PrivateDiffFinding {
    let (category, severity, explanation) = field_policy(field);
    PrivateDiffFinding {
        category,
        severity,
        location: PrivateLocation::Registered(field),
        left,
        right,
        explanation,
    }
}

const fn field_policy(field: RegisteredField) -> (DiffCategory, DiffSeverity, FixedExplanation) {
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

fn finding_sort_key(
    finding: &PrivateDiffFinding,
) -> (u8, u16, DiffCategory, PrivateLocation, FixedExplanation) {
    let explanatory_rank = match finding.location {
        PrivateLocation::Registered(field) => field.explanatory_rank(),
        PrivateLocation::Unknown(_) => u16::MAX,
    };
    (
        finding.severity.rank(),
        explanatory_rank,
        finding.category,
        finding.location,
        finding.explanation,
    )
}

fn value_class(values: &[CanonicalValue]) -> ValueClass {
    match values {
        [] => ValueClass::Missing,
        [value] => value.value_class(),
        _ => ValueClass::Multiple,
    }
}

fn safe_value(values: &[CanonicalValue]) -> SafeValue {
    match values {
        [] => SafeValue::Missing,
        [value] => value.safe_value(),
        _ => SafeValue::Multiple,
    }
}

fn safe_finding(finding: &PrivateDiffFinding) -> Option<SafeDiffFinding> {
    let PrivateLocation::Registered(field) = finding.location else {
        return None;
    };
    Some(SafeDiffFinding {
        category: finding.category,
        severity: finding.severity,
        field,
        left_class: value_class(&finding.left),
        right_class: value_class(&finding.right),
        left: safe_value(&finding.left),
        right: safe_value(&finding.right),
        explanation: finding.explanation,
    })
}

fn opaque_classes(count: usize) -> Vec<CanonicalValue> {
    (count != 0)
        .then(|| CanonicalValue::PrivateOpaqueCount(BoundedCount::new(count).0))
        .into_iter()
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn registered(facts: impl IntoIterator<Item = SemanticFact>) -> SemanticSnapshot {
        SemanticSnapshot::from_registered(facts)
    }

    fn fields(report: &ComparisonReportV1) -> BTreeSet<RegisteredField> {
        report
            .safe_projection()
            .iter()
            .map(|finding| finding.field)
            .collect()
    }

    #[test]
    fn identical_semantic_captures_have_no_findings() {
        let left = registered([
            SemanticFact::PlayerHttpStatus(HttpStatusCode::new(200)),
            SemanticFact::PlayabilityStatus(PlayabilityClass::Playable),
            SemanticFact::StreamingDataPresent(true),
            SemanticFact::FormatIdentifier(PrivateFormatId::new(251)),
            SemanticFact::FormatIdentifier(PrivateFormatId::new(140)),
            SemanticFact::Container(ContainerKind::WebM),
            SemanticFact::Container(ContainerKind::Mp4),
        ]);
        let right = registered([
            SemanticFact::Container(ContainerKind::Mp4),
            SemanticFact::FormatIdentifier(PrivateFormatId::new(140)),
            SemanticFact::StreamingDataPresent(true),
            SemanticFact::PlayabilityStatus(PlayabilityClass::Playable),
            SemanticFact::FormatIdentifier(PrivateFormatId::new(251)),
            SemanticFact::PlayerHttpStatus(HttpStatusCode::new(200)),
            SemanticFact::Container(ContainerKind::WebM),
            SemanticFact::Container(ContainerKind::Mp4),
        ]);

        let report = compare(ComparisonKind::WorkingVsFailing, &left, &right);

        assert_eq!(report.schema_version(), COMPARISON_SCHEMA_VERSION);
        assert!(report.findings().is_empty());
        assert!(report.safe_projection().is_empty());
        assert!(!report.incomplete());
    }

    #[test]
    fn meaningful_provider_and_media_differences_are_ranked_and_projected() {
        let working = registered([
            SemanticFact::PlayerHttpStatus(HttpStatusCode::new(200)),
            SemanticFact::PlayabilityStatus(PlayabilityClass::Playable),
            SemanticFact::SafeFailureCategory(FailureCategory::None),
            SemanticFact::StreamingDataPresent(true),
            SemanticFact::ReturnedFormatCount(BoundedCount::new(4)),
            SemanticFact::SupportedFormatCount(BoundedCount::new(3)),
            SemanticFact::DirectFormatCount(BoundedCount::new(3)),
            SemanticFact::CipherFormatCount(BoundedCount::new(1)),
            SemanticFact::FormatIdentifier(PrivateFormatId::new(140)),
            SemanticFact::Container(ContainerKind::Mp4),
            SemanticFact::Codec(CodecKind::Aac),
            SemanticFact::SelectedFormat(Some(PrivateFormatId::new(140))),
            SemanticFact::MediaHttpStatus(HttpStatusCode::new(206)),
            SemanticFact::ContentRangeClass(ContentRangeClass::Valid),
        ]);
        let failing = registered([
            SemanticFact::PlayerHttpStatus(HttpStatusCode::new(200)),
            SemanticFact::PlayabilityStatus(PlayabilityClass::Unavailable),
            SemanticFact::SafeFailureCategory(FailureCategory::ProviderUnavailable),
            SemanticFact::StreamingDataPresent(false),
            SemanticFact::ReturnedFormatCount(BoundedCount::new(3)),
            SemanticFact::SupportedFormatCount(BoundedCount::new(3)),
            SemanticFact::DirectFormatCount(BoundedCount::new(0)),
            SemanticFact::CipherFormatCount(BoundedCount::new(3)),
            SemanticFact::FormatIdentifier(PrivateFormatId::new(251)),
            SemanticFact::Container(ContainerKind::WebM),
            SemanticFact::Codec(CodecKind::Opus),
            SemanticFact::SelectedFormat(None),
            SemanticFact::MediaHttpStatus(HttpStatusCode::new(403)),
            SemanticFact::ContentRangeClass(ContentRangeClass::Missing),
        ]);

        let report = compare(ComparisonKind::WorkingVsFailing, &working, &failing);
        let fields = fields(&report);

        for expected in [
            RegisteredField::PlayabilityStatus,
            RegisteredField::StreamingDataPresent,
            RegisteredField::DirectFormatCount,
            RegisteredField::CipherFormatCount,
            RegisteredField::FormatIdentifier,
            RegisteredField::Container,
            RegisteredField::Codec,
            RegisteredField::SelectedFormat,
            RegisteredField::MediaHttpStatus,
            RegisteredField::ContentRangeClass,
        ] {
            assert!(fields.contains(&expected), "missing {expected:?}");
        }
        assert_eq!(report.safe_projection()[0].severity, DiffSeverity::Critical);
        let media = report
            .safe_projection()
            .iter()
            .find(|finding| finding.field == RegisteredField::MediaHttpStatus)
            .unwrap();
        assert_eq!(media.left, SafeValue::HttpStatus(HttpStatusClass::Success));
        assert_eq!(
            media.right,
            SafeValue::HttpStatus(HttpStatusClass::Forbidden)
        );
    }

    #[test]
    fn volatile_values_are_ignored_by_policy() {
        let private = |value: &[u8]| PrivateOpaqueValue::from_private_bytes(value);
        let left = SemanticSnapshot::from_facts([
            ComparisonFact::Registered(SemanticFact::PlayabilityStatus(PlayabilityClass::Playable)),
            ComparisonFact::Volatile(VolatileFact::CaptureTimestamp(1)),
            ComparisonFact::Volatile(VolatileFact::GeneratedRequestId(private(b"left-id"))),
            ComparisonFact::Volatile(VolatileFact::AuthorizationValue(private(b"left-auth"))),
            ComparisonFact::Volatile(VolatileFact::CookieValue(private(b"left-cookie"))),
            ComparisonFact::Volatile(VolatileFact::ProofTokenValue(private(b"left-proof"))),
            ComparisonFact::Volatile(VolatileFact::SignedMediaSignature(private(
                b"left-signature",
            ))),
            ComparisonFact::Volatile(VolatileFact::SignedMediaExpiration(1)),
            ComparisonFact::Volatile(VolatileFact::SignedMediaHost(private(b"left-host"))),
            ComparisonFact::Volatile(VolatileFact::QueryOrdering(private(b"a=1&b=2"))),
            ComparisonFact::Volatile(VolatileFact::RangeCounter(1)),
            ComparisonFact::Volatile(VolatileFact::ProviderExperimentValue(private(
                b"left-experiment",
            ))),
        ]);
        let right = SemanticSnapshot::from_facts([
            ComparisonFact::Registered(SemanticFact::PlayabilityStatus(PlayabilityClass::Playable)),
            ComparisonFact::Volatile(VolatileFact::CaptureTimestamp(u64::MAX)),
            ComparisonFact::Volatile(VolatileFact::GeneratedRequestId(private(b"right-id"))),
            ComparisonFact::Volatile(VolatileFact::AuthorizationValue(private(b"right-auth"))),
            ComparisonFact::Volatile(VolatileFact::CookieValue(private(b"right-cookie"))),
            ComparisonFact::Volatile(VolatileFact::ProofTokenValue(private(b"right-proof"))),
            ComparisonFact::Volatile(VolatileFact::SignedMediaSignature(private(
                b"right-signature",
            ))),
            ComparisonFact::Volatile(VolatileFact::SignedMediaExpiration(u64::MAX)),
            ComparisonFact::Volatile(VolatileFact::SignedMediaHost(private(b"right-host"))),
            ComparisonFact::Volatile(VolatileFact::QueryOrdering(private(b"b=2&a=1"))),
            ComparisonFact::Volatile(VolatileFact::RangeCounter(u64::MAX)),
            ComparisonFact::Volatile(VolatileFact::ProviderExperimentValue(private(
                b"right-experiment",
            ))),
        ]);

        let report = compare(ComparisonKind::OriginalVsReplay, &left, &right);

        assert!(report.findings().is_empty());
        assert!(report.safe_projection().is_empty());
    }

    #[test]
    fn unknown_private_facts_never_enter_the_safe_projection() {
        let left = SemanticSnapshot::from_facts([
            ComparisonFact::UnknownPrivate(UnknownPrivateFact::new(7, b"left-private-value")),
            ComparisonFact::Registered(SemanticFact::TerminalOutcome(TerminalOutcome::Success)),
        ]);
        let right = SemanticSnapshot::from_facts([
            ComparisonFact::UnknownPrivate(UnknownPrivateFact::new(7, b"right-private-value")),
            ComparisonFact::Registered(SemanticFact::TerminalOutcome(TerminalOutcome::Success)),
        ]);

        let report = compare(ComparisonKind::OriginalVsReplay, &left, &right);

        assert_eq!(report.findings().len(), 1);
        assert_eq!(report.findings()[0].registered_field(), None);
        assert_eq!(
            report.findings()[0].explanation(),
            FixedExplanation::UnknownPrivateFactChanged
        );
        assert!(report.safe_projection().is_empty());
    }

    #[test]
    fn finding_order_deduplication_and_bounds_are_deterministic() {
        let mut left = vec![ComparisonFact::Registered(SemanticFact::PlayabilityStatus(
            PlayabilityClass::Playable,
        ))];
        let mut right = vec![ComparisonFact::Registered(SemanticFact::PlayabilityStatus(
            PlayabilityClass::Unavailable,
        ))];
        for slot in 0..100_u16 {
            left.push(ComparisonFact::UnknownPrivate(UnknownPrivateFact::new(
                slot,
                &[u8::try_from(slot).unwrap_or(u8::MAX)],
            )));
            right.push(ComparisonFact::UnknownPrivate(UnknownPrivateFact::new(
                slot,
                &[u8::try_from(slot).unwrap_or(u8::MAX).wrapping_add(1)],
            )));
        }
        left.push(ComparisonFact::Registered(SemanticFact::PlayabilityStatus(
            PlayabilityClass::Playable,
        )));
        right.push(ComparisonFact::Registered(SemanticFact::PlayabilityStatus(
            PlayabilityClass::Unavailable,
        )));
        let left = SemanticSnapshot::from_facts(left);
        let right = SemanticSnapshot::from_facts(right);

        let first = compare(ComparisonKind::WorkingVsFailing, &left, &right);
        let second = compare(ComparisonKind::WorkingVsFailing, &left, &right);

        assert!(first == second);
        assert_eq!(first.findings().len(), MAX_DIFF_FINDINGS);
        assert!(first.incomplete());
        assert_eq!(first.dropped_findings(), 37);
        assert_eq!(first.safe_projection().len(), 1);
        assert_eq!(
            first.safe_projection()[0].field,
            RegisteredField::PlayabilityStatus
        );
        assert_eq!(
            first.findings()[0].registered_field(),
            Some(RegisteredField::PlayabilityStatus)
        );
    }

    #[test]
    fn comparison_kind_exposes_only_fixed_side_roles() {
        let empty = registered([]);
        let report = compare(ComparisonKind::OriginalVsReplay, &empty, &empty);

        assert_eq!(report.kind(), ComparisonKind::OriginalVsReplay);
        assert_eq!(report.left_role(), ComparisonRole::Original);
        assert_eq!(report.right_role(), ComparisonRole::Replay);
    }

    #[test]
    fn high_volume_inventory_cannot_evict_essential_terminal_facts() {
        let mut facts = (0..600_u64)
            .map(|itag| {
                ComparisonFact::Registered(SemanticFact::FormatIdentifier(PrivateFormatId::new(
                    itag,
                )))
            })
            .collect::<Vec<_>>();
        facts.extend([
            ComparisonFact::Registered(SemanticFact::PlayabilityStatus(
                PlayabilityClass::Unavailable,
            )),
            ComparisonFact::Registered(SemanticFact::SafeFailureCategory(
                FailureCategory::Decipher,
            )),
            ComparisonFact::Registered(SemanticFact::TerminalOutcome(TerminalOutcome::Failed)),
        ]);
        let snapshot = SemanticSnapshot::from_facts(facts);
        let expected = registered([
            SemanticFact::PlayabilityStatus(PlayabilityClass::Unavailable),
            SemanticFact::SafeFailureCategory(FailureCategory::Decipher),
            SemanticFact::TerminalOutcome(TerminalOutcome::Failed),
        ]);

        let report = compare(ComparisonKind::OriginalVsReplay, &snapshot, &expected);
        let differing = fields(&report);

        assert!(snapshot.input_incomplete());
        assert!(!differing.contains(&RegisteredField::PlayabilityStatus));
        assert!(!differing.contains(&RegisteredField::SafeFailureCategory));
        assert!(!differing.contains(&RegisteredField::TerminalOutcome));
    }
}
