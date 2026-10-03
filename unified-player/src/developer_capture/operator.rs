//! Bounded operator boundary for encrypted provider captures.
//!
//! The worker is the only owner of passphrases, decrypted artifacts, replay
//! adapters, derivative destinations, and private filesystem paths. Consumers
//! receive only the finite safe snapshot types in this module.

use std::{
    fmt,
    panic::AssertUnwindSafe,
    path::{Path, PathBuf},
    sync::Arc,
};

use futures::FutureExt as _;
use tokio::sync::{oneshot, watch};
use tokio_util::sync::CancellationToken;

use super::{
    analysis::SafeComparisonSummaryV1,
    comparison_store::{ComparisonArtifactService, ComparisonFacadeError},
    derivative_builder::{
        prepare_derivative_from_capture, DerivativeBuildError, PreparedPrivateDerivativeV1,
    },
    derivative_store::DerivativeDirectoryStoreV1,
    diff::{ComparisonKind, DiffCategory},
    model::{
        CaptureByteBucket, CaptureCompleteness, CaptureLimits, SafeArtifactReview, SafeCaptureRef,
        SafeCaptureSnapshot, SafeOperationRef, SafeTerminalCategory,
    },
    recorder::{CaptureHandle, CaptureRuntimeError},
    replay::{
        FreshReplayOutcome, FreshReplayPolicy, FreshSemanticReplayAdapter, OfflineReplayAdapter,
        OfflineReplayOutcome, ReplayTerminalOutcome, ReplayTimer,
    },
    replay_store::{ReplayArtifactService, ReplayFacadeError, ReplayMode, SafeReplayArtifact},
    sanitize::{
        DerivativeCompletenessV1, DerivativeFileNameV1, DerivativePreviewV1, DerivativeReviewV1,
    },
    security::CapturePassphrase,
    store::{CaptureStore, MaintenanceReport, StoreError},
    writer::VaultFormatError,
};

pub(crate) const MAX_SAFE_OPERATOR_ARTIFACTS: usize = 5;
const DEFAULT_OPERATOR_QUEUE_CAPACITY: usize = 8;
const MAX_SAFE_DERIVATIVE_PREVIEW_BYTES: usize = 512 * 1024;
const MAX_SAFE_DERIVATIVE_REVIEW_COPY_BYTES: usize = 1024;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) enum SafeArtifactLabel {
    #[default]
    Unlabeled,
    Working,
    Failing,
}

impl SafeArtifactLabel {
    pub(crate) const fn as_str(self) -> &'static str {
        match self {
            Self::Unlabeled => "unlabeled",
            Self::Working => "working",
            Self::Failing => "failing",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct SafeOperatorArtifact {
    pub(crate) capture_ref: SafeCaptureRef,
    pub(crate) label: SafeArtifactLabel,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum SafeOperatorAction {
    Refresh,
    Select,
    Label,
    RequestArm,
    AcceptConsent,
    CancelConsent,
    Disarm,
    Review,
    Compare,
    ReplayOffline,
    ReplayFresh,
    PreviewDerivative,
    CreateDerivative,
    ReviewDerivative,
    OpenPrivateFolder,
    Delete,
    PurgeExpired,
}

impl SafeOperatorAction {
    pub(crate) const fn as_str(self) -> &'static str {
        match self {
            Self::Refresh => "refresh private captures",
            Self::Select => "select private capture",
            Self::Label => "label private capture",
            Self::RequestArm => "request private capture",
            Self::AcceptConsent => "arm private capture",
            Self::CancelConsent => "cancel private capture consent",
            Self::Disarm => "disarm private capture",
            Self::Review => "review private capture safely",
            Self::Compare => "compare working and failing captures",
            Self::ReplayOffline => "run offline replay",
            Self::ReplayFresh => "run fresh replay",
            Self::PreviewDerivative => "preview diagnostic derivative",
            Self::CreateDerivative => "create diagnostic derivative",
            Self::ReviewDerivative => "review diagnostic derivative",
            Self::OpenPrivateFolder => "open private capture folder",
            Self::Delete => "delete private capture",
            Self::PurgeExpired => "purge expired private captures",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum SafeOperatorFailure {
    InvalidState,
    NothingSelected,
    LabelsIncomplete,
    ArtifactNotFound,
    AmbiguousReference,
    WrongPassphrase,
    InvalidArtifact,
    QueueFull,
    VaultUnavailable,
    ReplayUnavailable,
    ComparisonUnavailable,
    DerivativeUnavailable,
    InspectionUnavailable,
    DestinationUnavailable,
    FolderOpeningUnsupported,
    ConsentRequired,
    NetworkConsentRequired,
    Cancelled,
    WorkerStopped,
}

impl SafeOperatorFailure {
    pub(crate) const fn as_str(self) -> &'static str {
        match self {
            Self::InvalidState => "the action is not available in the current state",
            Self::NothingSelected => "no private capture is selected",
            Self::LabelsIncomplete => "mark one working and one failing capture first",
            Self::ArtifactNotFound => "the selected private capture is no longer retained",
            Self::AmbiguousReference => "the short private capture reference is ambiguous",
            Self::WrongPassphrase => "the private capture passphrase was not accepted",
            Self::InvalidArtifact => "the encrypted private capture did not pass validation",
            Self::QueueFull => "the private capture operator is busy",
            Self::VaultUnavailable => "the private capture vault is unavailable",
            Self::ReplayUnavailable => "private replay could not be completed",
            Self::ComparisonUnavailable => "private comparison could not be completed",
            Self::DerivativeUnavailable => "the diagnostic derivative could not be completed",
            Self::InspectionUnavailable => {
                "masked private capture inspection could not be completed"
            }
            Self::DestinationUnavailable => "no safe derivative destination is available",
            Self::FolderOpeningUnsupported => {
                "opening the private capture folder is unsupported here"
            }
            Self::ConsentRequired => "private capture consent has not been completed",
            Self::NetworkConsentRequired => "fresh replay network consent is required",
            Self::Cancelled => "the private capture action was cancelled",
            Self::WorkerStopped => "the private capture operator is unavailable",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum SafeOperatorDisposition {
    Completed,
    Rejected(SafeOperatorFailure),
    Cancelled,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct SafeOperatorResult {
    pub(crate) action: SafeOperatorAction,
    pub(crate) disposition: SafeOperatorDisposition,
}

impl SafeOperatorResult {
    const fn completed(action: SafeOperatorAction) -> Self {
        Self {
            action,
            disposition: SafeOperatorDisposition::Completed,
        }
    }

    const fn rejected(action: SafeOperatorAction, failure: SafeOperatorFailure) -> Self {
        Self {
            action,
            disposition: SafeOperatorDisposition::Rejected(failure),
        }
    }

    const fn cancelled(action: SafeOperatorAction) -> Self {
        Self {
            action,
            disposition: SafeOperatorDisposition::Cancelled,
        }
    }

    pub(crate) const fn disposition_str(self) -> &'static str {
        match self.disposition {
            SafeOperatorDisposition::Completed => "completed",
            SafeOperatorDisposition::Rejected(_) => "failed",
            SafeOperatorDisposition::Cancelled => "cancelled",
        }
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) enum SafeOperatorPhase {
    #[default]
    Loading,
    Ready,
    Working(SafeOperatorAction),
    Failed(SafeOperatorFailure),
    Stopped,
}

impl SafeOperatorPhase {
    pub(crate) const fn as_str(self) -> &'static str {
        match self {
            Self::Loading => "loading",
            Self::Ready => "ready",
            Self::Working(_) => "working",
            Self::Failed(_) => "degraded",
            Self::Stopped => "stopped",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct SafeComparisonView {
    pub(crate) capture_ref: SafeCaptureRef,
    pub(crate) finding_count: u16,
    pub(crate) categories: [Option<DiffCategory>; 8],
    pub(crate) incomplete: bool,
    pub(crate) dropped_findings: u16,
}

impl SafeComparisonView {
    fn new(capture_ref: SafeCaptureRef, summary: &SafeComparisonSummaryV1) -> Self {
        let mut categories = [None; 8];
        for (slot, category) in categories.iter_mut().zip(summary.categories()) {
            *slot = Some(*category);
        }
        Self {
            capture_ref,
            finding_count: u16::try_from(summary.findings().len()).unwrap_or(u16::MAX),
            categories,
            incomplete: summary.incomplete(),
            dropped_findings: summary.dropped_findings(),
        }
    }

    pub(crate) fn category_labels(self) -> [Option<&'static str>; 8] {
        self.categories.map(|category| {
            category.map(|category| match category {
                DiffCategory::ProviderResponse => "provider response",
                DiffCategory::Authentication => "authentication",
                DiffCategory::FormatSelection => "format selection",
                DiffCategory::Fallback => "fallback",
                DiffCategory::MediaTransport => "media transport",
                DiffCategory::Performance => "performance",
                DiffCategory::Lifecycle => "lifecycle",
                DiffCategory::PrivateUnknown => "private unknown",
            })
        })
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum SafeReplayOutcome {
    Reproduced,
    Changed,
    AuthenticationUnavailable,
    BrowserContended,
    ExpiredInput,
    NetworkFailed,
    Cancelled,
    TimedOut,
    Unsupported,
    Inconclusive,
    Panicked,
}

impl SafeReplayOutcome {
    pub(crate) const fn as_str(self) -> &'static str {
        match self {
            Self::Reproduced => "reproduced",
            Self::Changed => "changed",
            Self::AuthenticationUnavailable => "authentication unavailable",
            Self::BrowserContended => "playback browser busy",
            Self::ExpiredInput => "input expired",
            Self::NetworkFailed => "network failed",
            Self::Cancelled => "cancelled",
            Self::TimedOut => "timed out",
            Self::Unsupported => "unsupported",
            Self::Inconclusive => "inconclusive",
            Self::Panicked => "replay failed safely",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct SafeReplayView {
    pub(crate) capture_ref: SafeCaptureRef,
    pub(crate) mode: ReplayMode,
    pub(crate) outcome: SafeReplayOutcome,
    pub(crate) terminal_category: SafeTerminalCategory,
    pub(crate) record_count: u16,
}

impl From<SafeReplayArtifact> for SafeReplayView {
    fn from(value: SafeReplayArtifact) -> Self {
        Self {
            capture_ref: value.capture_ref,
            mode: value.mode,
            outcome: safe_replay_outcome(value.outcome),
            terminal_category: value.terminal_category,
            record_count: value.record_count,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum SafeDerivativeState {
    Previewed,
    Created,
    Reviewed,
}

#[derive(Clone, Eq, PartialEq)]
struct SafeDerivativePreviewFile {
    name: DerivativeFileNameV1,
    contents: String,
}

#[derive(Clone, Eq, PartialEq)]
pub(crate) struct SafeDerivativePreview {
    files: Vec<SafeDerivativePreviewFile>,
    total_bytes: usize,
    rendered: Arc<str>,
    rendered_line_count: usize,
}

impl fmt::Debug for SafeDerivativePreview {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("SafeDerivativePreview")
            .field("file_count", &self.files.len())
            .field("total_bytes", &self.total_bytes)
            .field("rendered_line_count", &self.rendered_line_count)
            .finish_non_exhaustive()
    }
}

impl SafeDerivativePreview {
    fn from_scanned(preview: &DerivativePreviewV1) -> Result<Self, SafeOperatorFailure> {
        let expected = [
            DerivativeFileNameV1::Evidence,
            DerivativeFileNameV1::Checksums,
            DerivativeFileNameV1::Manifest,
        ];
        if preview.files().len() != expected.len()
            || !preview
                .files()
                .iter()
                .zip(expected)
                .all(|(file, expected)| file.name() == expected)
        {
            return Err(SafeOperatorFailure::DerivativeUnavailable);
        }
        let total_bytes = preview.files().iter().try_fold(0_usize, |total, file| {
            total.checked_add(file.contents().len())
        });
        if total_bytes != Some(preview.total_bytes())
            || preview.total_bytes() > MAX_SAFE_DERIVATIVE_PREVIEW_BYTES
        {
            return Err(SafeOperatorFailure::DerivativeUnavailable);
        }
        let files = preview
            .files()
            .iter()
            .map(|file| SafeDerivativePreviewFile {
                name: file.name(),
                contents: file.contents().to_owned(),
            })
            .collect::<Vec<_>>();
        Ok(Self::from_files(files, preview.total_bytes()))
    }

    pub(crate) fn render_text(&self) -> String {
        self.rendered.to_string()
    }

    pub(crate) fn rendered_text(&self) -> &str {
        &self.rendered
    }

    fn render_files(files: &[SafeDerivativePreviewFile], total_bytes: usize) -> String {
        let mut output = String::with_capacity(total_bytes.saturating_add(96));
        for (index, file) in files.iter().enumerate() {
            if index != 0 {
                output.push('\n');
            }
            output.push_str("===== ");
            output.push_str(file.name.as_str());
            output.push_str(" =====\n");
            output.push_str(&file.contents);
            if !file.contents.ends_with('\n') {
                output.push('\n');
            }
        }
        output
    }

    fn from_files(files: Vec<SafeDerivativePreviewFile>, total_bytes: usize) -> Self {
        let rendered_line_count = files
            .iter()
            .map(|file| 1_usize.saturating_add(file.contents.lines().count().max(1)))
            .sum::<usize>()
            .saturating_add(files.len().saturating_sub(1));
        let rendered = Arc::from(Self::render_files(&files, total_bytes));
        Self {
            files,
            total_bytes,
            rendered,
            rendered_line_count,
        }
    }

    pub(crate) const fn rendered_line_count(&self) -> usize {
        self.rendered_line_count
    }

    pub(crate) const fn total_bytes(&self) -> usize {
        self.total_bytes
    }

    #[cfg(test)]
    fn from_test_contents(contents: [&str; 3]) -> Self {
        let names = [
            DerivativeFileNameV1::Evidence,
            DerivativeFileNameV1::Checksums,
            DerivativeFileNameV1::Manifest,
        ];
        let files = names
            .into_iter()
            .zip(contents)
            .map(|(name, contents)| SafeDerivativePreviewFile {
                name,
                contents: contents.to_owned(),
            })
            .collect::<Vec<_>>();
        let total_bytes = files.iter().map(|file| file.contents.len()).sum();
        Self::from_files(files, total_bytes)
    }
}

#[derive(Clone, Eq, PartialEq)]
struct SafeDerivativeReviewCopy(Arc<str>);

impl fmt::Debug for SafeDerivativeReviewCopy {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("SafeDerivativeReviewCopy")
            .field("bytes", &self.0.len())
            .finish()
    }
}

impl SafeDerivativeReviewCopy {
    fn from_review(review: &DerivativeReviewV1) -> Result<Self, SafeOperatorFailure> {
        let text = review.safe_copy_text();
        if text.len() > MAX_SAFE_DERIVATIVE_REVIEW_COPY_BYTES {
            return Err(SafeOperatorFailure::DerivativeUnavailable);
        }
        Ok(Self(Arc::from(text)))
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct SafeDerivativeView {
    pub(crate) state: SafeDerivativeState,
    pub(crate) schema_version: u16,
    pub(crate) completeness: DerivativeCompletenessV1,
    pub(crate) byte_bucket: CaptureByteBucket,
    pub(crate) checksum_valid: bool,
    pub(crate) forbidden_scan_passed: bool,
    pub(crate) durability_confirmed: Option<bool>,
    preview: Option<Arc<SafeDerivativePreview>>,
    review_copy: SafeDerivativeReviewCopy,
}

impl SafeDerivativeView {
    fn from_review(
        state: SafeDerivativeState,
        review: &DerivativeReviewV1,
        byte_bucket: CaptureByteBucket,
        durability_confirmed: Option<bool>,
        preview: Option<&DerivativePreviewV1>,
    ) -> Result<Self, SafeOperatorFailure> {
        let preview = preview
            .map(SafeDerivativePreview::from_scanned)
            .transpose()?
            .map(Arc::new);
        let review_copy = SafeDerivativeReviewCopy::from_review(review)?;
        Ok(Self {
            state,
            schema_version: review.schema_version(),
            completeness: review.completeness(),
            byte_bucket,
            checksum_valid: review.checksum_valid(),
            forbidden_scan_passed: review.forbidden_scan_passed(),
            durability_confirmed,
            preview,
            review_copy,
        })
    }

    pub(crate) const fn state_str(&self) -> &'static str {
        match self.state {
            SafeDerivativeState::Previewed => "previewed",
            SafeDerivativeState::Created => "created",
            SafeDerivativeState::Reviewed => "reviewed",
        }
    }

    pub(crate) const fn completeness_str(&self) -> &'static str {
        match self.completeness {
            DerivativeCompletenessV1::Complete => "complete",
            DerivativeCompletenessV1::Incomplete => "incomplete",
        }
    }

    pub(crate) const fn byte_bucket_str(&self) -> &'static str {
        match self.byte_bucket {
            CaptureByteBucket::Empty => "empty",
            CaptureByteBucket::Under64KiB => "under 64 KiB",
            CaptureByteBucket::Under1MiB => "under 1 MiB",
            CaptureByteBucket::Under4MiB => "under 4 MiB",
            CaptureByteBucket::Under16MiB => "under 16 MiB",
            CaptureByteBucket::AtOrOver16MiB => "at least 16 MiB",
        }
    }

    pub(crate) fn preview(&self) -> Option<&SafeDerivativePreview> {
        self.preview.as_deref()
    }

    pub(crate) fn preview_shared(&self) -> Option<Arc<SafeDerivativePreview>> {
        self.preview.clone()
    }

    pub(crate) fn safe_review_copy_text(&self) -> String {
        self.review_copy.0.to_string()
    }

    #[cfg(test)]
    pub(crate) fn new_for_test(
        state: SafeDerivativeState,
        contents: [&str; 3],
        review_copy: &str,
    ) -> Self {
        assert!(review_copy.len() <= MAX_SAFE_DERIVATIVE_REVIEW_COPY_BYTES);
        Self {
            state,
            schema_version: 1,
            completeness: DerivativeCompletenessV1::Complete,
            byte_bucket: CaptureByteBucket::Under64KiB,
            checksum_valid: true,
            forbidden_scan_passed: true,
            durability_confirmed: (state == SafeDerivativeState::Created).then_some(true),
            preview: Some(Arc::new(SafeDerivativePreview::from_test_contents(
                contents,
            ))),
            review_copy: SafeDerivativeReviewCopy(Arc::from(review_copy)),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct SafeOperatorSnapshot {
    pub(crate) phase: SafeOperatorPhase,
    pub(crate) capture: SafeCaptureSnapshot,
    pub(crate) artifacts: Vec<SafeOperatorArtifact>,
    pub(crate) artifact_overflow: bool,
    pub(crate) selected: Option<SafeCaptureRef>,
    pub(crate) working: Option<SafeCaptureRef>,
    pub(crate) failing: Option<SafeCaptureRef>,
    pub(crate) last_result: Option<SafeOperatorResult>,
    pub(crate) review: Option<SafeArtifactReview>,
    pub(crate) comparison: Option<SafeComparisonView>,
    pub(crate) replay: Option<SafeReplayView>,
    pub(crate) derivative: Option<SafeDerivativeView>,
}

impl Default for SafeOperatorSnapshot {
    fn default() -> Self {
        Self {
            phase: SafeOperatorPhase::Loading,
            capture: SafeCaptureSnapshot::default(),
            artifacts: Vec::new(),
            artifact_overflow: false,
            selected: None,
            working: None,
            failing: None,
            last_result: None,
            review: None,
            comparison: None,
            replay: None,
            derivative: None,
        }
    }
}

pub(crate) struct PrivateDerivativeDestination(PathBuf);

impl PrivateDerivativeDestination {
    pub(crate) fn new(path: PathBuf) -> Self {
        Self(path)
    }

    fn into_inner(self) -> PathBuf {
        self.0
    }
}

impl fmt::Debug for PrivateDerivativeDestination {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("PrivateDerivativeDestination([private])")
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct FreshReplayAcknowledgement(());

impl FreshReplayAcknowledgement {
    pub(crate) const fn confirmed() -> Self {
        Self(())
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct SensitiveFolderAcknowledgement(());

impl SensitiveFolderAcknowledgement {
    pub(crate) const fn confirmed() -> Self {
        Self(())
    }
}

#[derive(Clone)]
pub(crate) struct CaptureOperatorHandle {
    sender: flume::Sender<OperatorEnvelope>,
    snapshot_rx: watch::Receiver<SafeOperatorSnapshot>,
}

impl fmt::Debug for CaptureOperatorHandle {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("CaptureOperatorHandle")
            .field("snapshot", &self.snapshot())
            .finish_non_exhaustive()
    }
}

impl CaptureOperatorHandle {
    pub(crate) fn subscribe(&self) -> watch::Receiver<SafeOperatorSnapshot> {
        self.snapshot_rx.clone()
    }

    pub(crate) fn snapshot(&self) -> SafeOperatorSnapshot {
        self.snapshot_rx.borrow().clone()
    }

    pub(crate) fn refresh(&self) -> Result<OperatorCompletion, OperatorSubmitError> {
        self.submit(OperatorRequest::Refresh)
    }

    pub(crate) fn select(
        &self,
        capture_ref: Option<SafeCaptureRef>,
    ) -> Result<OperatorCompletion, OperatorSubmitError> {
        self.submit(OperatorRequest::Select(capture_ref))
    }

    pub(crate) fn label_selected(
        &self,
        label: SafeArtifactLabel,
    ) -> Result<OperatorCompletion, OperatorSubmitError> {
        self.submit(OperatorRequest::LabelSelected(label))
    }

    pub(crate) fn request_arm(&self) -> Result<OperatorCompletion, OperatorSubmitError> {
        self.submit(OperatorRequest::RequestArm)
    }

    pub(crate) fn accept_consent(
        &self,
        passphrase: CapturePassphrase,
    ) -> Result<OperatorCompletion, OperatorSubmitError> {
        self.submit(OperatorRequest::AcceptConsent(passphrase))
    }

    pub(crate) fn cancel_consent(&self) -> Result<OperatorCompletion, OperatorSubmitError> {
        self.submit(OperatorRequest::CancelConsent)
    }

    pub(crate) fn disarm(&self) -> Result<OperatorCompletion, OperatorSubmitError> {
        self.submit(OperatorRequest::Disarm)
    }

    pub(crate) fn review_selected(
        &self,
        passphrase: CapturePassphrase,
    ) -> Result<OperatorCompletion, OperatorSubmitError> {
        self.submit(OperatorRequest::ReviewSelected(passphrase))
    }

    pub(crate) fn compare_labeled(
        &self,
        passphrase: CapturePassphrase,
    ) -> Result<OperatorCompletion, OperatorSubmitError> {
        self.submit(OperatorRequest::CompareLabeled(passphrase))
    }

    pub(crate) fn replay_offline_selected(
        &self,
        passphrase: CapturePassphrase,
    ) -> Result<OperatorCompletion, OperatorSubmitError> {
        self.submit(OperatorRequest::ReplayOfflineSelected(passphrase))
    }

    pub(crate) fn replay_fresh_selected(
        &self,
        passphrase: CapturePassphrase,
        _: FreshReplayAcknowledgement,
    ) -> Result<OperatorCompletion, OperatorSubmitError> {
        self.submit(OperatorRequest::ReplayFreshSelected(passphrase))
    }

    pub(crate) fn preview_derivative_selected(
        &self,
        passphrase: CapturePassphrase,
    ) -> Result<OperatorCompletion, OperatorSubmitError> {
        self.submit(OperatorRequest::PreviewDerivativeSelected(passphrase))
    }

    pub(crate) fn create_derivative_selected_default(
        &self,
        passphrase: CapturePassphrase,
    ) -> Result<OperatorCompletion, OperatorSubmitError> {
        self.submit(OperatorRequest::CreateDerivativeSelected {
            passphrase,
            destination: None,
        })
    }

    pub(crate) fn create_derivative_selected_at(
        &self,
        passphrase: CapturePassphrase,
        destination: PrivateDerivativeDestination,
    ) -> Result<OperatorCompletion, OperatorSubmitError> {
        self.submit(OperatorRequest::CreateDerivativeSelected {
            passphrase,
            destination: Some(destination),
        })
    }

    pub(crate) fn review_derivative(&self) -> Result<OperatorCompletion, OperatorSubmitError> {
        self.submit(OperatorRequest::ReviewDerivative)
    }

    pub(crate) fn open_private_folder(
        &self,
        _: SensitiveFolderAcknowledgement,
    ) -> Result<OperatorCompletion, OperatorSubmitError> {
        self.submit(OperatorRequest::OpenPrivateFolder)
    }

    pub(crate) fn delete_selected(&self) -> Result<OperatorCompletion, OperatorSubmitError> {
        self.submit(OperatorRequest::DeleteSelected)
    }

    pub(crate) fn purge_expired(&self) -> Result<OperatorCompletion, OperatorSubmitError> {
        self.submit(OperatorRequest::PurgeExpired)
    }

    fn submit(&self, request: OperatorRequest) -> Result<OperatorCompletion, OperatorSubmitError> {
        let action = request.action();
        let (reply, completion) = oneshot::channel();
        let envelope = OperatorEnvelope { request, reply };
        match self.sender.try_send(envelope) {
            Ok(()) => Ok(OperatorCompletion { action, completion }),
            Err(flume::TrySendError::Full(_)) => Err(OperatorSubmitError::QueueFull),
            Err(flume::TrySendError::Disconnected(_)) => Err(OperatorSubmitError::WorkerStopped),
        }
    }
}

pub(crate) struct OperatorCompletion {
    action: SafeOperatorAction,
    completion: oneshot::Receiver<SafeOperatorResult>,
}

impl fmt::Debug for OperatorCompletion {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("OperatorCompletion")
            .field("action", &self.action)
            .finish_non_exhaustive()
    }
}

impl OperatorCompletion {
    pub(crate) async fn wait(self) -> SafeOperatorResult {
        self.completion.await.unwrap_or_else(|_| {
            SafeOperatorResult::rejected(self.action, SafeOperatorFailure::WorkerStopped)
        })
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum OperatorSubmitError {
    QueueFull,
    WorkerStopped,
}

impl fmt::Display for OperatorSubmitError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::QueueFull => "private capture operator queue is full",
            Self::WorkerStopped => "private capture operator is unavailable",
        })
    }
}

impl std::error::Error for OperatorSubmitError {}

struct OperatorEnvelope {
    request: OperatorRequest,
    reply: oneshot::Sender<SafeOperatorResult>,
}

enum OperatorRequest {
    Refresh,
    Select(Option<SafeCaptureRef>),
    LabelSelected(SafeArtifactLabel),
    RequestArm,
    AcceptConsent(CapturePassphrase),
    CancelConsent,
    Disarm,
    ReviewSelected(CapturePassphrase),
    CompareLabeled(CapturePassphrase),
    ReplayOfflineSelected(CapturePassphrase),
    ReplayFreshSelected(CapturePassphrase),
    PreviewDerivativeSelected(CapturePassphrase),
    CreateDerivativeSelected {
        passphrase: CapturePassphrase,
        destination: Option<PrivateDerivativeDestination>,
    },
    ReviewDerivative,
    OpenPrivateFolder,
    DeleteSelected,
    PurgeExpired,
}

impl OperatorRequest {
    const fn action(&self) -> SafeOperatorAction {
        match self {
            Self::Refresh => SafeOperatorAction::Refresh,
            Self::Select(_) => SafeOperatorAction::Select,
            Self::LabelSelected(_) => SafeOperatorAction::Label,
            Self::RequestArm => SafeOperatorAction::RequestArm,
            Self::AcceptConsent(_) => SafeOperatorAction::AcceptConsent,
            Self::CancelConsent => SafeOperatorAction::CancelConsent,
            Self::Disarm => SafeOperatorAction::Disarm,
            Self::ReviewSelected(_) => SafeOperatorAction::Review,
            Self::CompareLabeled(_) => SafeOperatorAction::Compare,
            Self::ReplayOfflineSelected(_) => SafeOperatorAction::ReplayOffline,
            Self::ReplayFreshSelected(_) => SafeOperatorAction::ReplayFresh,
            Self::PreviewDerivativeSelected(_) => SafeOperatorAction::PreviewDerivative,
            Self::CreateDerivativeSelected { .. } => SafeOperatorAction::CreateDerivative,
            Self::ReviewDerivative => SafeOperatorAction::ReviewDerivative,
            Self::OpenPrivateFolder => SafeOperatorAction::OpenPrivateFolder,
            Self::DeleteSelected => SafeOperatorAction::Delete,
            Self::PurgeExpired => SafeOperatorAction::PurgeExpired,
        }
    }
}

#[async_trait::async_trait]
pub(crate) trait CaptureOperatorBackend: Send {
    fn list_safe_refs(&mut self) -> Result<Vec<SafeCaptureRef>, SafeOperatorFailure>;

    fn review(
        &mut self,
        capture_ref: SafeCaptureRef,
        passphrase: &CapturePassphrase,
    ) -> Result<SafeArtifactReview, SafeOperatorFailure>;

    fn compare(
        &mut self,
        working: SafeCaptureRef,
        failing: SafeCaptureRef,
        passphrase: &CapturePassphrase,
    ) -> Result<SafeComparisonView, SafeOperatorFailure>;

    fn replay_offline(
        &mut self,
        capture_ref: SafeCaptureRef,
        passphrase: &CapturePassphrase,
    ) -> Result<SafeReplayView, SafeOperatorFailure>;

    async fn replay_fresh(
        &mut self,
        capture_ref: SafeCaptureRef,
        passphrase: &CapturePassphrase,
        cancellation: &CancellationToken,
    ) -> Result<SafeReplayView, SafeOperatorFailure>;

    fn preview_derivative(
        &mut self,
        capture_ref: SafeCaptureRef,
        passphrase: &CapturePassphrase,
    ) -> Result<SafeDerivativeView, SafeOperatorFailure>;

    fn create_derivative(
        &mut self,
        capture_ref: SafeCaptureRef,
        passphrase: &CapturePassphrase,
        destination: Option<PrivateDerivativeDestination>,
    ) -> Result<SafeDerivativeView, SafeOperatorFailure>;

    fn review_derivative(&mut self) -> Result<SafeDerivativeView, SafeOperatorFailure>;

    fn open_private_folder(&mut self, selected: SafeCaptureRef) -> Result<(), SafeOperatorFailure>;

    fn delete(&mut self, capture_ref: SafeCaptureRef) -> Result<bool, SafeOperatorFailure>;

    fn purge_expired(&mut self) -> Result<MaintenanceReport, SafeOperatorFailure>;
}

pub(crate) fn prepare_operator(
    capture: CaptureHandle,
    backend: Box<dyn CaptureOperatorBackend>,
    queue_capacity: usize,
) -> Result<(CaptureOperatorHandle, CaptureOperatorWorker), OperatorPrepareError> {
    if queue_capacity == 0 {
        return Err(OperatorPrepareError::InvalidQueueCapacity);
    }
    let initial = SafeOperatorSnapshot {
        capture: capture.snapshot(),
        ..SafeOperatorSnapshot::default()
    };
    let (snapshot_tx, snapshot_rx) = watch::channel(initial.clone());
    let (sender, receiver) = flume::bounded(queue_capacity);
    Ok((
        CaptureOperatorHandle {
            sender,
            snapshot_rx,
        },
        CaptureOperatorWorker {
            capture_rx: capture.subscribe(),
            capture,
            backend,
            receiver,
            snapshot_tx,
            snapshot: initial,
        },
    ))
}

pub(crate) fn prepare_operator_default(
    capture: CaptureHandle,
    backend: Box<dyn CaptureOperatorBackend>,
) -> (CaptureOperatorHandle, CaptureOperatorWorker) {
    prepare_operator(capture, backend, DEFAULT_OPERATOR_QUEUE_CAPACITY)
        .expect("the fixed private operator queue capacity is valid")
}

pub(crate) fn unavailable_operator_handle(failure: SafeOperatorFailure) -> CaptureOperatorHandle {
    let (sender, receiver) = flume::bounded(1);
    drop(receiver);
    let (snapshot_tx, snapshot_rx) = watch::channel(SafeOperatorSnapshot {
        phase: SafeOperatorPhase::Failed(failure),
        last_result: Some(SafeOperatorResult::rejected(
            SafeOperatorAction::Refresh,
            failure,
        )),
        ..SafeOperatorSnapshot::default()
    });
    drop(snapshot_tx);
    CaptureOperatorHandle {
        sender,
        snapshot_rx,
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum OperatorPrepareError {
    InvalidQueueCapacity,
}

impl fmt::Display for OperatorPrepareError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("private capture operator queue capacity is invalid")
    }
}

impl std::error::Error for OperatorPrepareError {}

pub(crate) struct CaptureOperatorWorker {
    capture: CaptureHandle,
    capture_rx: watch::Receiver<SafeCaptureSnapshot>,
    backend: Box<dyn CaptureOperatorBackend>,
    receiver: flume::Receiver<OperatorEnvelope>,
    snapshot_tx: watch::Sender<SafeOperatorSnapshot>,
    snapshot: SafeOperatorSnapshot,
}

impl fmt::Debug for CaptureOperatorWorker {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("CaptureOperatorWorker")
            .field("snapshot", &self.snapshot)
            .field("backend", &"[private]")
            .finish_non_exhaustive()
    }
}

impl CaptureOperatorWorker {
    pub(crate) async fn run(
        mut self,
        shutdown: &CancellationToken,
    ) -> Result<(), OperatorWorkerError> {
        if std::panic::catch_unwind(AssertUnwindSafe(|| self.refresh_inventory())).is_err() {
            self.fail_phase(SafeOperatorFailure::WorkerStopped);
            return Err(OperatorWorkerError::BackendPanicked);
        }
        loop {
            tokio::select! {
                biased;
                () = shutdown.cancelled() => {
                    self.stop_and_cancel_pending();
                    return Ok(());
                }
                command = self.receiver.recv_async() => {
                    let command = command.map_err(|_| OperatorWorkerError::CommandChannelClosed)?;
                    if AssertUnwindSafe(self.execute(command, shutdown))
                        .catch_unwind()
                        .await
                        .is_err()
                    {
                        self.fail_phase(SafeOperatorFailure::WorkerStopped);
                        return Err(OperatorWorkerError::BackendPanicked);
                    }
                }
                changed = self.capture_rx.changed() => {
                    if changed.is_err() {
                        self.fail_phase(SafeOperatorFailure::WorkerStopped);
                        return Err(OperatorWorkerError::CaptureSnapshotClosed);
                    }
                    let capture = *self.capture_rx.borrow_and_update();
                    if capture == self.snapshot.capture {
                        continue;
                    }
                    self.snapshot.capture = capture;
                    if capture_inventory_may_have_changed(capture) {
                        self.refresh_inventory();
                    } else {
                        self.publish();
                    }
                }
            }
        }
    }

    async fn execute(&mut self, envelope: OperatorEnvelope, shutdown: &CancellationToken) {
        let OperatorEnvelope { request, reply } = envelope;
        let action = request.action();
        if shutdown.is_cancelled() {
            let result = SafeOperatorResult::cancelled(action);
            let _ = reply.send(result);
            return;
        }
        self.snapshot.phase = SafeOperatorPhase::Working(action);
        self.publish();

        let mut outcome = match request {
            OperatorRequest::Refresh => self.refresh_inventory_result(),
            OperatorRequest::Select(capture_ref) => self.select(capture_ref),
            OperatorRequest::LabelSelected(label) => self.label_selected(label),
            OperatorRequest::RequestArm => self
                .capture
                .request_arm()
                .map_err(map_capture_runtime_error),
            OperatorRequest::AcceptConsent(passphrase) => self
                .capture
                .accept_consent(passphrase)
                .map(|_| ())
                .map_err(map_capture_runtime_error),
            OperatorRequest::CancelConsent => {
                self.capture.cancel_consent();
                Ok(())
            }
            OperatorRequest::Disarm => {
                self.capture.disarm();
                Ok(())
            }
            OperatorRequest::ReviewSelected(passphrase) => self
                .with_selected(|backend, selected| backend.review(selected, &passphrase))
                .map(|review| self.snapshot.review = Some(review)),
            OperatorRequest::CompareLabeled(passphrase) => {
                match (self.snapshot.working, self.snapshot.failing) {
                    (Some(working), Some(failing)) if working != failing => self
                        .backend
                        .compare(working, failing, &passphrase)
                        .map(|comparison| self.snapshot.comparison = Some(comparison)),
                    _ => Err(SafeOperatorFailure::LabelsIncomplete),
                }
            }
            OperatorRequest::ReplayOfflineSelected(passphrase) => self
                .with_selected(|backend, selected| backend.replay_offline(selected, &passphrase))
                .map(|replay| self.snapshot.replay = Some(replay)),
            OperatorRequest::ReplayFreshSelected(passphrase) => {
                let selected = self.selected();
                match selected {
                    Ok(selected) => self
                        .backend
                        .replay_fresh(selected, &passphrase, shutdown)
                        .await
                        .map(|replay| self.snapshot.replay = Some(replay)),
                    Err(error) => Err(error),
                }
            }
            OperatorRequest::PreviewDerivativeSelected(passphrase) => self
                .with_selected(|backend, selected| {
                    backend.preview_derivative(selected, &passphrase)
                })
                .map(|derivative| self.snapshot.derivative = Some(derivative)),
            OperatorRequest::CreateDerivativeSelected {
                passphrase,
                destination,
            } => self
                .with_selected(|backend, selected| {
                    backend.create_derivative(selected, &passphrase, destination)
                })
                .map(|derivative| self.snapshot.derivative = Some(derivative)),
            OperatorRequest::ReviewDerivative => self
                .backend
                .review_derivative()
                .map(|derivative| self.snapshot.derivative = Some(derivative)),
            OperatorRequest::OpenPrivateFolder => self
                .selected()
                .and_then(|selected| self.backend.open_private_folder(selected)),
            OperatorRequest::DeleteSelected => self.delete_selected(),
            OperatorRequest::PurgeExpired => self
                .backend
                .purge_expired()
                .map(|_| ())
                .and_then(|()| self.refresh_inventory_result()),
        };
        if outcome.is_ok()
            && matches!(
                action,
                SafeOperatorAction::Compare
                    | SafeOperatorAction::ReplayOffline
                    | SafeOperatorAction::ReplayFresh
            )
        {
            outcome = self.refresh_inventory_result();
        }

        self.snapshot.capture = self.capture.snapshot();
        if matches!(
            action,
            SafeOperatorAction::AcceptConsent
                | SafeOperatorAction::CancelConsent
                | SafeOperatorAction::Disarm
                | SafeOperatorAction::RequestArm
        ) {
            self.publish();
        }
        let result = match outcome {
            Ok(()) if shutdown.is_cancelled() => SafeOperatorResult::cancelled(action),
            Ok(()) => SafeOperatorResult::completed(action),
            Err(SafeOperatorFailure::Cancelled) => SafeOperatorResult::cancelled(action),
            Err(error) => SafeOperatorResult::rejected(action, error),
        };
        self.snapshot.last_result = Some(result);
        self.snapshot.phase = match result.disposition {
            SafeOperatorDisposition::Completed => SafeOperatorPhase::Ready,
            SafeOperatorDisposition::Rejected(error) => SafeOperatorPhase::Failed(error),
            SafeOperatorDisposition::Cancelled => {
                SafeOperatorPhase::Failed(SafeOperatorFailure::Cancelled)
            }
        };
        self.publish();
        let _ = reply.send(result);
    }

    fn selected(&self) -> Result<SafeCaptureRef, SafeOperatorFailure> {
        self.snapshot
            .selected
            .ok_or(SafeOperatorFailure::NothingSelected)
    }

    fn with_selected<T>(
        &mut self,
        operation: impl FnOnce(
            &mut dyn CaptureOperatorBackend,
            SafeCaptureRef,
        ) -> Result<T, SafeOperatorFailure>,
    ) -> Result<T, SafeOperatorFailure> {
        let selected = self.selected()?;
        operation(self.backend.as_mut(), selected)
    }

    fn select(&mut self, capture_ref: Option<SafeCaptureRef>) -> Result<(), SafeOperatorFailure> {
        if let Some(capture_ref) = capture_ref {
            if !self
                .snapshot
                .artifacts
                .iter()
                .any(|artifact| artifact.capture_ref == capture_ref)
            {
                return Err(SafeOperatorFailure::ArtifactNotFound);
            }
        }
        self.snapshot.selected = capture_ref;
        self.snapshot.review = None;
        Ok(())
    }

    fn label_selected(&mut self, label: SafeArtifactLabel) -> Result<(), SafeOperatorFailure> {
        let selected = self.selected()?;
        if label != SafeArtifactLabel::Unlabeled {
            for artifact in &mut self.snapshot.artifacts {
                if artifact.label == label {
                    artifact.label = SafeArtifactLabel::Unlabeled;
                }
            }
        }
        let artifact = self
            .snapshot
            .artifacts
            .iter_mut()
            .find(|artifact| artifact.capture_ref == selected)
            .ok_or(SafeOperatorFailure::ArtifactNotFound)?;
        artifact.label = label;
        self.update_labels();
        Ok(())
    }

    fn delete_selected(&mut self) -> Result<(), SafeOperatorFailure> {
        let selected = self.selected()?;
        if !self.backend.delete(selected)? {
            return Err(SafeOperatorFailure::ArtifactNotFound);
        }
        self.refresh_inventory_result()?;
        Ok(())
    }

    fn refresh_inventory(&mut self) {
        match self.refresh_inventory_result() {
            Ok(()) => self.snapshot.phase = SafeOperatorPhase::Ready,
            Err(error) => self.snapshot.phase = SafeOperatorPhase::Failed(error),
        }
        self.publish();
    }

    fn refresh_inventory_result(&mut self) -> Result<(), SafeOperatorFailure> {
        let mut references = self.backend.list_safe_refs()?;
        references.sort_unstable();
        references.dedup();
        self.snapshot.artifact_overflow = references.len() > MAX_SAFE_OPERATOR_ARTIFACTS;
        references.truncate(MAX_SAFE_OPERATOR_ARTIFACTS);

        let previous = std::mem::take(&mut self.snapshot.artifacts);
        self.snapshot.artifacts = references
            .into_iter()
            .map(|capture_ref| SafeOperatorArtifact {
                capture_ref,
                label: previous
                    .iter()
                    .find(|artifact| artifact.capture_ref == capture_ref)
                    .map_or(SafeArtifactLabel::Unlabeled, |artifact| artifact.label),
            })
            .collect();
        if self.snapshot.selected.is_none_or(|selected| {
            !self
                .snapshot
                .artifacts
                .iter()
                .any(|artifact| artifact.capture_ref == selected)
        }) {
            self.snapshot.selected = self
                .snapshot
                .artifacts
                .first()
                .map(|artifact| artifact.capture_ref);
            self.snapshot.review = None;
        }
        self.update_labels();
        Ok(())
    }

    fn update_labels(&mut self) {
        self.snapshot.working = self
            .snapshot
            .artifacts
            .iter()
            .find(|artifact| artifact.label == SafeArtifactLabel::Working)
            .map(|artifact| artifact.capture_ref);
        self.snapshot.failing = self
            .snapshot
            .artifacts
            .iter()
            .find(|artifact| artifact.label == SafeArtifactLabel::Failing)
            .map(|artifact| artifact.capture_ref);
    }

    fn fail_phase(&mut self, error: SafeOperatorFailure) {
        self.snapshot.phase = SafeOperatorPhase::Failed(error);
        self.publish();
    }

    fn stop_and_cancel_pending(&mut self) {
        self.snapshot.phase = SafeOperatorPhase::Stopped;
        self.publish();
        while let Ok(envelope) = self.receiver.try_recv() {
            let action = envelope.request.action();
            let _ = envelope.reply.send(SafeOperatorResult::cancelled(action));
        }
    }

    fn publish(&self) {
        self.snapshot_tx.send_replace(self.snapshot.clone());
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum OperatorWorkerError {
    BackendPanicked,
    CommandChannelClosed,
    CaptureSnapshotClosed,
}

impl fmt::Display for OperatorWorkerError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::BackendPanicked => "private capture operator failed safely",
            Self::CommandChannelClosed => {
                "private capture operator command channel closed unexpectedly"
            }
            Self::CaptureSnapshotClosed => {
                "private capture operator status channel closed unexpectedly"
            }
        })
    }
}

impl std::error::Error for OperatorWorkerError {}

pub(crate) trait PrivateFolderOpener: Send {
    fn open(&mut self, path: &Path) -> Result<(), FolderOpenError>;
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct FolderOpenError;

#[derive(Debug, Default)]
pub(crate) struct SystemPrivateFolderOpener;

impl PrivateFolderOpener for SystemPrivateFolderOpener {
    fn open(&mut self, path: &Path) -> Result<(), FolderOpenError> {
        open::that(path).map_err(|_| FolderOpenError)
    }
}

/// Production vault backend. All generic collaborators are moved into the
/// operator worker and cannot be reached from application state.
pub(crate) struct VaultOperatorBackend<O, F, T, P> {
    root: PathBuf,
    default_derivative_parent: Option<PathBuf>,
    forbidden_derivative_roots: Vec<PathBuf>,
    store: CaptureStore,
    replay: ReplayArtifactService,
    comparison: ComparisonArtifactService,
    offline_adapter: O,
    fresh_adapter: F,
    replay_timer: T,
    fresh_policy: FreshReplayPolicy,
    folder_opener: P,
    prepared_derivative: Option<(SafeCaptureRef, PreparedPrivateDerivativeV1)>,
    created_derivative: Option<(
        SafeCaptureRef,
        DerivativeDirectoryStoreV1,
        CaptureByteBucket,
    )>,
}

impl<O, F, T, P> fmt::Debug for VaultOperatorBackend<O, F, T, P> {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("VaultOperatorBackend")
            .field("root", &"[private]")
            .field("default_derivative_parent", &"[private]")
            .field("forbidden_derivative_roots", &"[private]")
            .finish_non_exhaustive()
    }
}

impl<O, F, T, P> VaultOperatorBackend<O, F, T, P> {
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn open(
        root: impl AsRef<Path>,
        limits: CaptureLimits,
        default_derivative_parent: Option<PathBuf>,
        mut forbidden_derivative_roots: Vec<PathBuf>,
        offline_adapter: O,
        fresh_adapter: F,
        replay_timer: T,
        fresh_policy: FreshReplayPolicy,
        folder_opener: P,
    ) -> Result<(Self, MaintenanceReport), SafeOperatorFailure> {
        let root_input = root.as_ref();
        let (store, maintenance) =
            CaptureStore::open(root_input, limits).map_err(map_store_error)?;
        let root =
            std::fs::canonicalize(root_input).map_err(|_| SafeOperatorFailure::VaultUnavailable)?;
        if !forbidden_derivative_roots.contains(&root) {
            forbidden_derivative_roots.push(root.clone());
        }
        let (replay, _) = ReplayArtifactService::open(&root, limits).map_err(map_replay_error)?;
        let (comparison, _) =
            ComparisonArtifactService::open(&root, limits).map_err(map_comparison_error)?;
        Ok((
            Self {
                root,
                default_derivative_parent,
                forbidden_derivative_roots,
                store,
                replay,
                comparison,
                offline_adapter,
                fresh_adapter,
                replay_timer,
                fresh_policy,
                folder_opener,
                prepared_derivative: None,
                created_derivative: None,
            },
            maintenance,
        ))
    }

    fn operation_ref() -> SafeOperationRef {
        SafeOperationRef::from_bytes(rand::random())
    }

    fn prepare_derivative(
        &mut self,
        capture_ref: SafeCaptureRef,
        passphrase: &CapturePassphrase,
    ) -> Result<SafeDerivativeView, SafeOperatorFailure> {
        let capture = self
            .store
            .read_private_by_safe_ref(capture_ref, passphrase)
            .map_err(map_store_error)?;
        let prepared =
            prepare_derivative_from_capture(&capture).map_err(map_derivative_build_error)?;
        let preview = prepared.preview();
        let view = SafeDerivativeView::from_review(
            SafeDerivativeState::Previewed,
            prepared.review(),
            CaptureByteBucket::from_bytes(u64::try_from(preview.total_bytes()).unwrap_or(u64::MAX)),
            None,
            Some(&preview),
        )?;
        self.prepared_derivative = Some((capture_ref, prepared));
        self.created_derivative = None;
        Ok(view)
    }

    fn derivative_destination(
        &self,
        destination: Option<PrivateDerivativeDestination>,
    ) -> Result<PathBuf, SafeOperatorFailure> {
        if let Some(destination) = destination {
            return Ok(destination.into_inner());
        }
        let parent = self
            .default_derivative_parent
            .as_ref()
            .ok_or(SafeOperatorFailure::DestinationUnavailable)?;
        Ok(parent.join(format!(
            "unified-player-provider-diagnostic-{:016x}",
            rand::random::<u64>()
        )))
    }

    pub(crate) fn inspect_masked_private_evidence_to_terminal(
        &mut self,
        capture_ref: SafeCaptureRef,
        passphrase: &CapturePassphrase,
        selection: super::private_view::PrivateViewSelection,
    ) -> Result<(), SafeOperatorFailure> {
        let authorization = super::private_view::authorize_private_terminal_output()
            .map_err(|_| SafeOperatorFailure::InspectionUnavailable)?;
        let artifact = self
            .store
            .read_private_artifact_by_safe_ref(capture_ref, passphrase)
            .map_err(map_store_error)?;
        super::private_view::write_masked_private_view_to_terminal(
            &artifact,
            selection,
            authorization,
        )
        .map_err(|_| SafeOperatorFailure::InspectionUnavailable)
    }
}

#[async_trait::async_trait]
impl<O, F, T, P> CaptureOperatorBackend for VaultOperatorBackend<O, F, T, P>
where
    O: OfflineReplayAdapter + Send,
    F: FreshSemanticReplayAdapter + Send + Sync,
    T: ReplayTimer + Send + Sync,
    P: PrivateFolderOpener + Send,
{
    fn list_safe_refs(&mut self) -> Result<Vec<SafeCaptureRef>, SafeOperatorFailure> {
        self.store.list_safe_refs().map_err(map_store_error)
    }

    fn review(
        &mut self,
        capture_ref: SafeCaptureRef,
        passphrase: &CapturePassphrase,
    ) -> Result<SafeArtifactReview, SafeOperatorFailure> {
        self.store
            .review_by_safe_ref(capture_ref, passphrase)
            .map_err(map_store_error)
    }

    fn compare(
        &mut self,
        working: SafeCaptureRef,
        failing: SafeCaptureRef,
        passphrase: &CapturePassphrase,
    ) -> Result<SafeComparisonView, SafeOperatorFailure> {
        let artifact = self
            .comparison
            .compare_and_persist(
                ComparisonKind::WorkingVsFailing,
                working,
                failing,
                passphrase,
                Self::operation_ref(),
            )
            .map_err(map_comparison_error)?;
        Ok(SafeComparisonView::new(
            artifact.capture_ref,
            &artifact.summary,
        ))
    }

    fn replay_offline(
        &mut self,
        capture_ref: SafeCaptureRef,
        passphrase: &CapturePassphrase,
    ) -> Result<SafeReplayView, SafeOperatorFailure> {
        self.replay
            .run_offline(
                capture_ref,
                passphrase,
                Self::operation_ref(),
                &self.offline_adapter,
            )
            .map(Into::into)
            .map_err(map_replay_error)
    }

    async fn replay_fresh(
        &mut self,
        capture_ref: SafeCaptureRef,
        passphrase: &CapturePassphrase,
        cancellation: &CancellationToken,
    ) -> Result<SafeReplayView, SafeOperatorFailure> {
        self.replay
            .run_fresh(
                capture_ref,
                passphrase,
                Self::operation_ref(),
                &self.fresh_adapter,
                &self.replay_timer,
                cancellation,
                self.fresh_policy,
            )
            .await
            .map(Into::into)
            .map_err(map_replay_error)
    }

    fn preview_derivative(
        &mut self,
        capture_ref: SafeCaptureRef,
        passphrase: &CapturePassphrase,
    ) -> Result<SafeDerivativeView, SafeOperatorFailure> {
        self.prepare_derivative(capture_ref, passphrase)
    }

    fn create_derivative(
        &mut self,
        capture_ref: SafeCaptureRef,
        passphrase: &CapturePassphrase,
        destination: Option<PrivateDerivativeDestination>,
    ) -> Result<SafeDerivativeView, SafeOperatorFailure> {
        let preview = self.prepare_derivative(capture_ref, passphrase)?;
        let target = self.derivative_destination(destination)?;
        let mut derivative_store =
            DerivativeDirectoryStoreV1::new_separate(target, &self.forbidden_derivative_roots)
                .map_err(|_| SafeOperatorFailure::DestinationUnavailable)?;
        let (_, prepared) = self
            .prepared_derivative
            .as_ref()
            .ok_or(SafeOperatorFailure::DerivativeUnavailable)?;
        let outcome = prepared
            .create(&mut derivative_store)
            .map_err(map_derivative_build_error)?;
        let exact_preview = prepared.preview();
        let view = SafeDerivativeView::from_review(
            SafeDerivativeState::Created,
            outcome.review(),
            preview.byte_bucket,
            Some(outcome.durability_confirmed()),
            Some(&exact_preview),
        )?;
        self.created_derivative = Some((capture_ref, derivative_store, preview.byte_bucket));
        Ok(view)
    }

    fn review_derivative(&mut self) -> Result<SafeDerivativeView, SafeOperatorFailure> {
        let (prepared_ref, prepared) = self
            .prepared_derivative
            .as_ref()
            .ok_or(SafeOperatorFailure::DerivativeUnavailable)?;
        let (created_ref, store, byte_bucket) = self
            .created_derivative
            .as_ref()
            .ok_or(SafeOperatorFailure::DerivativeUnavailable)?;
        if prepared_ref != created_ref {
            return Err(SafeOperatorFailure::DerivativeUnavailable);
        }
        let review = prepared
            .review_created(store)
            .map_err(map_derivative_build_error)?;
        let exact_preview = prepared.preview();
        SafeDerivativeView::from_review(
            SafeDerivativeState::Reviewed,
            &review,
            *byte_bucket,
            None,
            Some(&exact_preview),
        )
    }

    fn open_private_folder(&mut self, selected: SafeCaptureRef) -> Result<(), SafeOperatorFailure> {
        if !self
            .store
            .contains_safe_ref(selected)
            .map_err(map_store_error)?
        {
            return Err(SafeOperatorFailure::ArtifactNotFound);
        }
        self.folder_opener
            .open(&self.root)
            .map_err(|_| SafeOperatorFailure::FolderOpeningUnsupported)
    }

    fn delete(&mut self, capture_ref: SafeCaptureRef) -> Result<bool, SafeOperatorFailure> {
        let deleted = self
            .store
            .delete_by_safe_ref(capture_ref)
            .map_err(map_store_error)?;
        if deleted
            && self
                .prepared_derivative
                .as_ref()
                .is_some_and(|(reference, _)| *reference == capture_ref)
        {
            self.prepared_derivative = None;
            self.created_derivative = None;
        }
        Ok(deleted)
    }

    fn purge_expired(&mut self) -> Result<MaintenanceReport, SafeOperatorFailure> {
        self.store.maintain().map_err(map_store_error)
    }
}

fn capture_inventory_may_have_changed(snapshot: SafeCaptureSnapshot) -> bool {
    use super::model::SafeCaptureState;
    matches!(
        snapshot.state,
        SafeCaptureState::Ready
            | SafeCaptureState::Incomplete
            | SafeCaptureState::Expired
            | SafeCaptureState::Failed
            | SafeCaptureState::Inactive
    )
}

// Owning the error here guarantees private source errors are dropped inside the operator boundary.
#[allow(clippy::needless_pass_by_value)]
fn map_capture_runtime_error(error: CaptureRuntimeError) -> SafeOperatorFailure {
    match error {
        CaptureRuntimeError::Busy | CaptureRuntimeError::Controller(_) => {
            SafeOperatorFailure::InvalidState
        }
        CaptureRuntimeError::InvalidLimits
        | CaptureRuntimeError::Store(_)
        | CaptureRuntimeError::WriterUnavailable => SafeOperatorFailure::VaultUnavailable,
    }
}

// Owning the error here guarantees private source errors are dropped inside the operator boundary.
#[allow(clippy::needless_pass_by_value)]
fn map_store_error(error: StoreError) -> SafeOperatorFailure {
    match error {
        StoreError::ArtifactNotFound => SafeOperatorFailure::ArtifactNotFound,
        StoreError::AmbiguousSafeReference => SafeOperatorFailure::AmbiguousReference,
        StoreError::Format(VaultFormatError::DecryptionFailed) => {
            SafeOperatorFailure::WrongPassphrase
        }
        StoreError::Format(_) => SafeOperatorFailure::InvalidArtifact,
        StoreError::AlreadyExists
        | StoreError::Committed(_)
        | StoreError::DirectoryEntryLimit
        | StoreError::FinalizationDeadline
        | StoreError::InvalidLimits
        | StoreError::Io
        | StoreError::QuotaExceeded
        | StoreError::Security(_)
        | StoreError::UnsafePath => SafeOperatorFailure::VaultUnavailable,
    }
}

fn map_replay_error(error: ReplayFacadeError) -> SafeOperatorFailure {
    match error {
        ReplayFacadeError::Store(error) => map_store_error(error),
        ReplayFacadeError::ChildReferenceCollision
        | ReplayFacadeError::Encoding
        | ReplayFacadeError::Persistence
        | ReplayFacadeError::Recipe(_)
        | ReplayFacadeError::ReplayStart(_) => SafeOperatorFailure::ReplayUnavailable,
    }
}

fn map_comparison_error(error: ComparisonFacadeError) -> SafeOperatorFailure {
    match error {
        ComparisonFacadeError::Store(error) => map_store_error(error),
        ComparisonFacadeError::SameReference => SafeOperatorFailure::LabelsIncomplete,
        ComparisonFacadeError::ChildReferenceCollision
        | ComparisonFacadeError::Encoding
        | ComparisonFacadeError::Persistence => SafeOperatorFailure::ComparisonUnavailable,
    }
}

const fn map_derivative_build_error(_: DerivativeBuildError) -> SafeOperatorFailure {
    SafeOperatorFailure::DerivativeUnavailable
}

const fn safe_replay_outcome(outcome: ReplayTerminalOutcome) -> SafeReplayOutcome {
    match outcome {
        ReplayTerminalOutcome::Offline(OfflineReplayOutcome::Reproduced)
        | ReplayTerminalOutcome::Fresh(FreshReplayOutcome::Reproduced) => {
            SafeReplayOutcome::Reproduced
        }
        ReplayTerminalOutcome::Offline(OfflineReplayOutcome::Changed)
        | ReplayTerminalOutcome::Fresh(FreshReplayOutcome::ProviderChanged) => {
            SafeReplayOutcome::Changed
        }
        ReplayTerminalOutcome::Fresh(FreshReplayOutcome::AuthUnavailable) => {
            SafeReplayOutcome::AuthenticationUnavailable
        }
        ReplayTerminalOutcome::Fresh(FreshReplayOutcome::BrowserContended) => {
            SafeReplayOutcome::BrowserContended
        }
        ReplayTerminalOutcome::Fresh(FreshReplayOutcome::ExpiredInput) => {
            SafeReplayOutcome::ExpiredInput
        }
        ReplayTerminalOutcome::Fresh(FreshReplayOutcome::NetworkFailed) => {
            SafeReplayOutcome::NetworkFailed
        }
        ReplayTerminalOutcome::Fresh(FreshReplayOutcome::Cancelled) => SafeReplayOutcome::Cancelled,
        ReplayTerminalOutcome::Fresh(FreshReplayOutcome::TimedOut) => SafeReplayOutcome::TimedOut,
        ReplayTerminalOutcome::Offline(OfflineReplayOutcome::UnsupportedSchema)
        | ReplayTerminalOutcome::Fresh(FreshReplayOutcome::UnsupportedSchema) => {
            SafeReplayOutcome::Unsupported
        }
        ReplayTerminalOutcome::Offline(OfflineReplayOutcome::Incomplete)
        | ReplayTerminalOutcome::Fresh(FreshReplayOutcome::Inconclusive) => {
            SafeReplayOutcome::Inconclusive
        }
        ReplayTerminalOutcome::Offline(OfflineReplayOutcome::Panicked)
        | ReplayTerminalOutcome::Fresh(FreshReplayOutcome::Panicked) => SafeReplayOutcome::Panicked,
    }
}

#[cfg(test)]
mod tests {
    use std::{
        sync::{Arc, Mutex},
        time::Duration,
    };

    use static_assertions::assert_not_impl_any;
    use tokio_util::sync::CancellationToken;

    use super::*;
    use crate::developer_capture::{
        prepare_runtime, CaptureCompleteness, CapturePassphrase, SafeCaptureState,
    };

    assert_not_impl_any!(OperatorRequest: Clone, std::fmt::Debug, std::fmt::Display, serde::Serialize);
    assert_not_impl_any!(OperatorEnvelope: Clone, std::fmt::Debug, std::fmt::Display, serde::Serialize);

    #[derive(Default)]
    struct FakeBackendState {
        refs: Vec<SafeCaptureRef>,
        fail_next: Option<SafeOperatorFailure>,
        opened: bool,
        purge_count: u16,
        fresh_waits_for_cancellation: bool,
        panic_next: bool,
    }

    struct FakeBackend {
        state: Arc<Mutex<FakeBackendState>>,
    }

    impl FakeBackend {
        fn failure(&self) -> Result<(), SafeOperatorFailure> {
            let mut state = self.state.lock().unwrap();
            assert!(
                !std::mem::take(&mut state.panic_next),
                "fixed operator backend fixture panic"
            );
            match state.fail_next.take() {
                Some(error) => Err(error),
                None => Ok(()),
            }
        }
    }

    #[async_trait::async_trait]
    impl CaptureOperatorBackend for FakeBackend {
        fn list_safe_refs(&mut self) -> Result<Vec<SafeCaptureRef>, SafeOperatorFailure> {
            self.failure()?;
            Ok(self.state.lock().unwrap().refs.clone())
        }

        fn review(
            &mut self,
            capture_ref: SafeCaptureRef,
            _: &CapturePassphrase,
        ) -> Result<SafeArtifactReview, SafeOperatorFailure> {
            self.failure()?;
            Ok(SafeArtifactReview {
                schema_version: 1,
                capture_ref,
                record_count: 3,
                byte_bucket: CaptureByteBucket::Under64KiB,
                completeness: CaptureCompleteness::Complete,
                terminal_category: SafeTerminalCategory::Failed,
                checksum_valid: true,
            })
        }

        fn compare(
            &mut self,
            _: SafeCaptureRef,
            _: SafeCaptureRef,
            _: &CapturePassphrase,
        ) -> Result<SafeComparisonView, SafeOperatorFailure> {
            self.failure()?;
            Ok(SafeComparisonView {
                capture_ref: safe_ref("cccccccc"),
                finding_count: 2,
                categories: [
                    Some(DiffCategory::ProviderResponse),
                    Some(DiffCategory::FormatSelection),
                    None,
                    None,
                    None,
                    None,
                    None,
                    None,
                ],
                incomplete: false,
                dropped_findings: 0,
            })
        }

        fn replay_offline(
            &mut self,
            _: SafeCaptureRef,
            _: &CapturePassphrase,
        ) -> Result<SafeReplayView, SafeOperatorFailure> {
            self.failure()?;
            Ok(replay_view(
                ReplayMode::Offline,
                SafeReplayOutcome::Reproduced,
            ))
        }

        async fn replay_fresh(
            &mut self,
            _: SafeCaptureRef,
            _: &CapturePassphrase,
            cancellation: &CancellationToken,
        ) -> Result<SafeReplayView, SafeOperatorFailure> {
            self.failure()?;
            let waits = self.state.lock().unwrap().fresh_waits_for_cancellation;
            if waits {
                cancellation.cancelled().await;
                return Err(SafeOperatorFailure::Cancelled);
            }
            tokio::time::sleep(Duration::from_millis(1)).await;
            Ok(replay_view(ReplayMode::Fresh, SafeReplayOutcome::Changed))
        }

        fn preview_derivative(
            &mut self,
            _: SafeCaptureRef,
            _: &CapturePassphrase,
        ) -> Result<SafeDerivativeView, SafeOperatorFailure> {
            self.failure()?;
            Ok(derivative_view(SafeDerivativeState::Previewed))
        }

        fn create_derivative(
            &mut self,
            _: SafeCaptureRef,
            _: &CapturePassphrase,
            _: Option<PrivateDerivativeDestination>,
        ) -> Result<SafeDerivativeView, SafeOperatorFailure> {
            self.failure()?;
            Ok(derivative_view(SafeDerivativeState::Created))
        }

        fn review_derivative(&mut self) -> Result<SafeDerivativeView, SafeOperatorFailure> {
            self.failure()?;
            Ok(derivative_view(SafeDerivativeState::Reviewed))
        }

        fn open_private_folder(&mut self, _: SafeCaptureRef) -> Result<(), SafeOperatorFailure> {
            self.failure()?;
            self.state.lock().unwrap().opened = true;
            Ok(())
        }

        fn delete(&mut self, capture_ref: SafeCaptureRef) -> Result<bool, SafeOperatorFailure> {
            self.failure()?;
            let mut state = self.state.lock().unwrap();
            let before = state.refs.len();
            state.refs.retain(|candidate| *candidate != capture_ref);
            Ok(state.refs.len() != before)
        }

        fn purge_expired(&mut self) -> Result<MaintenanceReport, SafeOperatorFailure> {
            self.failure()?;
            let mut state = self.state.lock().unwrap();
            state.purge_count = state.purge_count.saturating_add(1);
            Ok(MaintenanceReport {
                quota_satisfied: true,
                retained_artifacts: u16::try_from(state.refs.len()).unwrap_or(u16::MAX),
                ..MaintenanceReport::default()
            })
        }
    }

    fn safe_ref(value: &str) -> SafeCaptureRef {
        SafeCaptureRef::from_hex(value).unwrap()
    }

    fn passphrase() -> CapturePassphrase {
        CapturePassphrase::new("operator-test-passphrase".to_owned()).unwrap()
    }

    fn replay_view(mode: ReplayMode, outcome: SafeReplayOutcome) -> SafeReplayView {
        SafeReplayView {
            capture_ref: safe_ref("dddddddd"),
            mode,
            outcome,
            terminal_category: SafeTerminalCategory::Success,
            record_count: 2,
        }
    }

    fn derivative_view(state: SafeDerivativeState) -> SafeDerivativeView {
        SafeDerivativeView::new_for_test(
            state,
            [
                "{\"provider\":\"youtube_music\"}\n",
                "0123456789abcdef  evidence.json\n",
                "{\"privacy_boundary\":\"typed-allowlist-only\"}\n",
            ],
            "unified-player provider diagnostic derivative review\n\
             schema_version=1\n\
             completeness=complete\n\
             files=3\n\
             facts=1\n\
             findings=0\n\
             total_bytes=120\n\
             checksum=passed\n\
             forbidden_scan=passed\n\
             privacy=typed-allowlist-only\n\
             support_bundle=separate\n",
        )
    }

    fn harness(
        references: Vec<SafeCaptureRef>,
        queue_capacity: usize,
    ) -> (
        CaptureOperatorHandle,
        CaptureOperatorWorker,
        Arc<Mutex<FakeBackendState>>,
    ) {
        let root = tempfile::tempdir().unwrap().keep();
        let (capture, _writer, _) = prepare_runtime(&root, CaptureLimits::default()).unwrap();
        let state = Arc::new(Mutex::new(FakeBackendState {
            refs: references,
            ..FakeBackendState::default()
        }));
        let backend = FakeBackend {
            state: state.clone(),
        };
        let (handle, worker) =
            prepare_operator(capture, Box::new(backend), queue_capacity).unwrap();
        (handle, worker, state)
    }

    async fn start_worker(
        worker: CaptureOperatorWorker,
    ) -> (
        CancellationToken,
        tokio::task::JoinHandle<Result<(), OperatorWorkerError>>,
    ) {
        let shutdown = CancellationToken::new();
        let task_shutdown = shutdown.clone();
        let task = tokio::spawn(async move { worker.run(&task_shutdown).await });
        tokio::task::yield_now().await;
        (shutdown, task)
    }

    async fn wait_ready(handle: &CaptureOperatorHandle) {
        let mut receiver = handle.subscribe();
        tokio::time::timeout(Duration::from_secs(2), async {
            loop {
                if receiver.borrow().phase == SafeOperatorPhase::Ready {
                    return;
                }
                receiver.changed().await.unwrap();
            }
        })
        .await
        .unwrap();
    }

    async fn stop_worker(
        shutdown: CancellationToken,
        task: tokio::task::JoinHandle<Result<(), OperatorWorkerError>>,
    ) {
        shutdown.cancel();
        assert_eq!(
            tokio::time::timeout(Duration::from_secs(2), task)
                .await
                .unwrap()
                .unwrap(),
            Ok(())
        );
    }

    #[tokio::test]
    async fn inventory_is_bounded_and_selection_survives_live_refresh() {
        let references = [
            "11111111", "22222222", "33333333", "44444444", "55555555", "66666666",
        ]
        .map(safe_ref)
        .to_vec();
        let (handle, worker, state) = harness(references, 4);
        let (shutdown, task) = start_worker(worker).await;
        wait_ready(&handle).await;
        assert_eq!(
            handle.snapshot().artifacts.len(),
            MAX_SAFE_OPERATOR_ARTIFACTS
        );
        assert!(handle.snapshot().artifact_overflow);

        let selected = safe_ref("33333333");
        assert_eq!(
            handle
                .select(Some(selected))
                .unwrap()
                .wait()
                .await
                .disposition,
            SafeOperatorDisposition::Completed
        );
        state.lock().unwrap().refs = vec![
            safe_ref("00000000"),
            safe_ref("22222222"),
            selected,
            safe_ref("77777777"),
        ];
        assert_eq!(
            handle.refresh().unwrap().wait().await.disposition,
            SafeOperatorDisposition::Completed
        );
        assert_eq!(handle.snapshot().selected, Some(selected));
        assert!(!handle.snapshot().artifact_overflow);
        stop_worker(shutdown, task).await;
    }

    #[tokio::test]
    async fn labels_comparison_replays_and_derivative_states_are_typed() {
        let working = safe_ref("11111111");
        let failing = safe_ref("22222222");
        let (handle, worker, _) = harness(vec![working, failing], 8);
        let (shutdown, task) = start_worker(worker).await;
        wait_ready(&handle).await;

        handle.select(Some(working)).unwrap().wait().await;
        handle
            .label_selected(SafeArtifactLabel::Working)
            .unwrap()
            .wait()
            .await;
        handle.select(Some(failing)).unwrap().wait().await;
        handle
            .label_selected(SafeArtifactLabel::Failing)
            .unwrap()
            .wait()
            .await;
        handle.compare_labeled(passphrase()).unwrap().wait().await;
        assert_eq!(handle.snapshot().comparison.unwrap().finding_count, 2);

        handle
            .replay_offline_selected(passphrase())
            .unwrap()
            .wait()
            .await;
        assert_eq!(
            handle.snapshot().replay.unwrap().outcome,
            SafeReplayOutcome::Reproduced
        );
        handle
            .replay_fresh_selected(passphrase(), FreshReplayAcknowledgement::confirmed())
            .unwrap()
            .wait()
            .await;
        assert_eq!(
            handle.snapshot().replay.unwrap().outcome,
            SafeReplayOutcome::Changed
        );

        handle
            .preview_derivative_selected(passphrase())
            .unwrap()
            .wait()
            .await;
        let previewed = handle.snapshot().derivative.unwrap();
        assert_eq!(previewed.state, SafeDerivativeState::Previewed);
        let preview = previewed.preview().unwrap();
        let rendered = preview.render_text();
        for expected in [
            "===== evidence.json =====",
            "{\"provider\":\"youtube_music\"}",
            "===== checksums.sha256 =====",
            "0123456789abcdef  evidence.json",
            "===== manifest.json =====",
            "\"privacy_boundary\":\"typed-allowlist-only\"",
        ] {
            assert!(rendered.contains(expected));
        }
        let copied = previewed.safe_review_copy_text();
        assert!(copied.starts_with("unified-player provider diagnostic derivative review\n"));
        assert!(copied.contains("forbidden_scan=passed"));
        assert!(!copied.contains("youtube_music"));
        let debug = format!("{previewed:?}");
        assert!(!debug.contains("youtube_music"));
        assert!(!debug.contains("0123456789abcdef"));
        handle
            .create_derivative_selected_default(passphrase())
            .unwrap()
            .wait()
            .await;
        assert_eq!(
            handle.snapshot().derivative.unwrap().durability_confirmed,
            Some(true)
        );
        handle.review_derivative().unwrap().wait().await;
        assert_eq!(
            handle.snapshot().derivative.unwrap().state,
            SafeDerivativeState::Reviewed
        );
        stop_worker(shutdown, task).await;
    }

    #[tokio::test]
    async fn failure_recovery_delete_purge_and_folder_open_are_safe() {
        let selected = safe_ref("11111111");
        let (handle, worker, state) = harness(vec![selected], 8);
        let (shutdown, task) = start_worker(worker).await;
        wait_ready(&handle).await;

        state.lock().unwrap().fail_next = Some(SafeOperatorFailure::WrongPassphrase);
        let result = handle.review_selected(passphrase()).unwrap().wait().await;
        assert_eq!(
            result.disposition,
            SafeOperatorDisposition::Rejected(SafeOperatorFailure::WrongPassphrase)
        );
        assert_eq!(
            handle.snapshot().phase,
            SafeOperatorPhase::Failed(SafeOperatorFailure::WrongPassphrase)
        );
        assert_eq!(
            handle.refresh().unwrap().wait().await.disposition,
            SafeOperatorDisposition::Completed
        );
        assert_eq!(handle.snapshot().phase, SafeOperatorPhase::Ready);

        handle
            .open_private_folder(SensitiveFolderAcknowledgement::confirmed())
            .unwrap()
            .wait()
            .await;
        assert!(state.lock().unwrap().opened);
        handle.purge_expired().unwrap().wait().await;
        assert_eq!(state.lock().unwrap().purge_count, 1);
        handle.delete_selected().unwrap().wait().await;
        assert!(handle.snapshot().artifacts.is_empty());
        assert_eq!(handle.snapshot().selected, None);
        stop_worker(shutdown, task).await;
    }

    #[tokio::test]
    async fn arm_consent_cancel_and_disarm_update_the_safe_capture_snapshot() {
        let (handle, worker, _) = harness(Vec::new(), 8);
        let (shutdown, task) = start_worker(worker).await;
        wait_ready(&handle).await;
        assert_eq!(
            handle.request_arm().unwrap().wait().await.disposition,
            SafeOperatorDisposition::Completed
        );
        assert_eq!(
            handle.snapshot().capture.state,
            SafeCaptureState::ConsentRequired
        );
        handle.cancel_consent().unwrap().wait().await;
        assert_eq!(handle.snapshot().capture.state, SafeCaptureState::Inactive);

        handle.request_arm().unwrap().wait().await;
        handle.accept_consent(passphrase()).unwrap().wait().await;
        assert_eq!(handle.snapshot().capture.state, SafeCaptureState::Armed);
        handle.disarm().unwrap().wait().await;
        assert_eq!(handle.snapshot().capture.state, SafeCaptureState::Inactive);
        stop_worker(shutdown, task).await;
    }

    #[tokio::test]
    async fn bounded_queue_rejects_without_cloning_secret_commands() {
        let (handle, _worker, _) = harness(vec![safe_ref("11111111")], 1);
        let _queued = handle.review_selected(passphrase()).unwrap();
        assert_eq!(
            handle.review_selected(passphrase()).unwrap_err(),
            OperatorSubmitError::QueueFull
        );
    }

    #[test]
    fn unavailable_handle_is_immediately_terminal_and_rejects_commands() {
        let handle = unavailable_operator_handle(SafeOperatorFailure::VaultUnavailable);
        assert_eq!(
            handle.snapshot().phase,
            SafeOperatorPhase::Failed(SafeOperatorFailure::VaultUnavailable)
        );
        assert_eq!(
            handle.refresh().unwrap_err(),
            OperatorSubmitError::WorkerStopped
        );
    }

    #[tokio::test]
    async fn cancellation_completes_active_and_queued_work_within_bound() {
        let selected = safe_ref("11111111");
        let (handle, worker, state) = harness(vec![selected], 4);
        state.lock().unwrap().fresh_waits_for_cancellation = true;
        let (shutdown, task) = start_worker(worker).await;
        wait_ready(&handle).await;
        let fresh = handle
            .replay_fresh_selected(passphrase(), FreshReplayAcknowledgement::confirmed())
            .unwrap();
        let queued = handle.refresh().unwrap();
        tokio::task::yield_now().await;
        shutdown.cancel();
        let fresh = tokio::time::timeout(Duration::from_secs(2), fresh.wait())
            .await
            .unwrap();
        let queued = tokio::time::timeout(Duration::from_secs(2), queued.wait())
            .await
            .unwrap();
        assert!(matches!(
            fresh.disposition,
            SafeOperatorDisposition::Cancelled
        ));
        assert!(matches!(
            queued.disposition,
            SafeOperatorDisposition::Cancelled
        ));
        assert_eq!(
            tokio::time::timeout(Duration::from_secs(2), task)
                .await
                .unwrap()
                .unwrap(),
            Ok(())
        );
        assert_eq!(handle.snapshot().phase, SafeOperatorPhase::Stopped);
    }

    #[tokio::test]
    async fn folder_open_failure_and_sensitive_debug_are_bounded() {
        let selected = safe_ref("11111111");
        let (handle, worker, state) = harness(vec![selected], 4);
        let (shutdown, task) = start_worker(worker).await;
        wait_ready(&handle).await;
        state.lock().unwrap().fail_next = Some(SafeOperatorFailure::FolderOpeningUnsupported);
        let result = handle
            .open_private_folder(SensitiveFolderAcknowledgement::confirmed())
            .unwrap()
            .wait()
            .await;
        assert_eq!(
            result.disposition,
            SafeOperatorDisposition::Rejected(SafeOperatorFailure::FolderOpeningUnsupported)
        );
        let private_destination =
            PrivateDerivativeDestination::new(PathBuf::from("private-canary-path"));
        assert_eq!(
            format!("{private_destination:?}"),
            "PrivateDerivativeDestination([private])"
        );
        assert!(!format!("{handle:?}").contains("private-canary-path"));
        stop_worker(shutdown, task).await;
    }

    #[tokio::test]
    async fn unexpected_backend_panic_surfaces_only_a_finite_worker_failure() {
        let selected = safe_ref("11111111");
        let (handle, worker, state) = harness(vec![selected], 4);
        let (shutdown, task) = start_worker(worker).await;
        wait_ready(&handle).await;
        state.lock().unwrap().panic_next = true;
        let completion = handle.review_selected(passphrase()).unwrap();
        assert_eq!(
            completion.wait().await.disposition,
            SafeOperatorDisposition::Rejected(SafeOperatorFailure::WorkerStopped)
        );
        assert_eq!(
            task.await.unwrap(),
            Err(OperatorWorkerError::BackendPanicked)
        );
        assert_eq!(
            handle.snapshot().phase,
            SafeOperatorPhase::Failed(SafeOperatorFailure::WorkerStopped)
        );
        shutdown.cancel();
    }
}
