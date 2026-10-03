use std::fmt;

mod console;
mod context;
mod health;
mod incident;
mod protocol;
mod sink;
mod support_bundle;

pub(crate) use console::{
    actions_for as diagnostic_actions_for, append_youtube_playback_route_row, bounded_trend_text,
    component_label, component_runbook, outcome_label, rows as diagnostic_rows,
    safe_bundle_review_text, safe_detail_text, safe_health_text, safe_incident_text,
    safe_reference_text, safe_transition_line, safe_worker_label, ConsoleRegistry,
    DiagnosticAction, DiagnosticRow, DiagnosticRowId, OperationTimeline, PerformanceEntry,
    TimelineEntry,
};
#[cfg(feature = "private-capture")]
pub(crate) use console::{private_capture_actions, private_capture_row};
#[cfg(feature = "private-capture")]
pub(crate) use context::current_operation;
pub(crate) use context::{
    child_or_new, in_operation, operation_stage, operation_stage_detail, request_accepted,
    request_completed, request_started, shutdown_stage, ui_render_sample, worker_transition,
};
pub(crate) use health::{
    HealthRegistry, HealthSnapshot, HealthStatus, HealthTransition, OperationHealth,
    UiDiagnosticSnapshot,
};
pub(crate) use incident::{IncidentRecorder, IncidentSummary};
#[allow(unused_imports)]
pub(crate) use protocol::{
    random_hex, Component, DiagnosticEvent, EventCode, EventName, OperationContext,
    OperationOutcome, OperationSource, PrivacyClass, ProviderKind, Severity, UiDiagnosticEntry,
    DIAGNOSTIC_SCHEMA_VERSION, EVENT_REGISTRY,
};
#[allow(unused_imports)]
pub(crate) use sink::{
    DiagnosticLayer, DiagnosticsHandle, DiagnosticsRuntime, DynamicFilterSnapshot, SinkPolicy,
    UiDiagnosticRing, WriterHealth, WriterState,
};
pub(crate) use support_bundle::{
    create as create_support_bundle, create_focused as create_focused_support_bundle,
    preview_manifest as preview_support_bundle, render_local_trend,
    review as review_support_bundle, BundleReview,
};

static DIAGNOSTICS: std::sync::OnceLock<DiagnosticsHandle> = std::sync::OnceLock::new();

pub(crate) fn start(
    directory: &std::path::Path,
    ui_ring: UiDiagnosticRing,
) -> anyhow::Result<(DiagnosticsHandle, DiagnosticsRuntime)> {
    sink::start(directory, ui_ring, SinkPolicy::default())
}

pub(crate) fn disabled(ui_ring: UiDiagnosticRing) -> (DiagnosticsHandle, DiagnosticsRuntime) {
    sink::disabled(ui_ring)
}

pub(crate) fn install(handle: DiagnosticsHandle) -> anyhow::Result<()> {
    DIAGNOSTICS
        .set(handle)
        .map_err(|_| anyhow::anyhow!("diagnostics already initialized"))
}

pub(crate) fn handle() -> Option<&'static DiagnosticsHandle> {
    DIAGNOSTICS.get()
}

pub(crate) fn record(event: DiagnosticEvent) {
    if let Some(handle) = handle() {
        handle.record(event);
    }
}

pub(crate) fn set_component_health(component: Component, status: HealthStatus, fact: &'static str) {
    if let Some(handle) = handle() {
        handle.set_component_health(component, status, fact);
    }
}

pub(crate) fn process_start_event(handle: &DiagnosticsHandle) -> DiagnosticEvent {
    sink::process_start_event(handle)
}

pub(crate) const SAFE_PANIC_REPORT: &str =
    "Application panic occurred; payload and backtrace omitted by diagnostic privacy policy";

pub(crate) fn panic_fingerprint(file: &str, line: u32, column: u32) -> String {
    use sha2::Digest as _;
    let mut hasher = sha2::Sha256::new();
    hasher.update(file.as_bytes());
    hasher.update(line.to_le_bytes());
    hasher.update(column.to_le_bytes());
    let digest = hasher.finalize();
    let mut output = String::with_capacity(12);
    for byte in &digest[..6] {
        let _ = fmt::Write::write_fmt(&mut output, format_args!("{byte:02x}"));
    }
    output
}

pub(crate) fn record_panic(location: Option<&std::panic::Location<'_>>) -> (String, String) {
    let fingerprint = location.map_or_else(
        || "unknown000000".to_owned(),
        |location| panic_fingerprint(location.file(), location.line(), location.column()),
    );
    let reference = format!("I-P{}", fingerprint.get(..6).unwrap_or(&fingerprint));
    if let Some(handle) = handle() {
        let mut event = DiagnosticEvent::new(
            handle.run_id(),
            handle.started_at(),
            EventName::PANIC_CAPTURED,
            EventCode::PANIC_CAPTURED,
            Severity::Error,
            Component::Runtime,
            "Application panic captured",
        );
        event.fields.incident_reference = Some(reference.clone());
        event.fields.fingerprint = Some(fingerprint.clone());
        event.fields.outcome = Some(OperationOutcome::Panicked);
        event.fields.error_type = Some("internal_panic".to_owned());
        handle.record_panic_event(event);
    }
    (reference, fingerprint)
}

pub(crate) fn is_application_event(metadata: &tracing::Metadata<'_>) -> bool {
    metadata.target() == "unified_player" || metadata.target().starts_with("unified_player::")
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum ErrorCategory {
    Authentication,
    Cancelled,
    ConsentAgeRegion,
    Contract,
    Decipher,
    Decode,
    #[cfg(feature = "streaming")]
    ExternalCommand,
    MediaForbidden,
    MediaRangeContract,
    Network,
    NetworkUnavailable,
    ProofToken,
    ProviderUnavailable,
    RateLimited,
    Resource,
    Storage,
    Unavailable,
    UnsupportedFormat,
}

impl ErrorCategory {
    pub(crate) const fn as_str(self) -> &'static str {
        match self {
            Self::Authentication => "authentication",
            Self::Cancelled => "cancelled",
            Self::ConsentAgeRegion => "consent_age_region",
            Self::Contract => "contract",
            Self::Decipher => "decipher",
            Self::Decode => "decode",
            #[cfg(feature = "streaming")]
            Self::ExternalCommand => "external_command",
            Self::MediaForbidden => "media_forbidden",
            Self::MediaRangeContract => "media_range_contract",
            Self::Network => "network",
            Self::NetworkUnavailable => "network_unavailable",
            Self::ProofToken => "proof_token",
            Self::ProviderUnavailable => "provider_unavailable",
            Self::RateLimited => "rate_limited",
            Self::Resource => "resource",
            Self::Storage => "storage",
            Self::Unavailable => "unavailable",
            Self::UnsupportedFormat => "unsupported_format",
        }
    }
}

/// A centrally registered diagnostic code. The private field prevents runtime
/// or user-controlled text from being mistaken for a safe event identifier.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct DiagnosticCode(&'static str);

impl DiagnosticCode {
    pub(crate) const CONFIG_PROXY_INVALID: Self = Self("CONFIG_PROXY_INVALID");
    pub(crate) const CONTEXT_HISTORY_DECODE_FAILED: Self = Self("CONTEXT_HISTORY_DECODE_FAILED");
    pub(crate) const CONTEXT_HISTORY_OPEN_FAILED: Self = Self("CONTEXT_HISTORY_OPEN_FAILED");
    pub(crate) const CONTEXT_HISTORY_SAVE_FAILED: Self = Self("CONTEXT_HISTORY_SAVE_FAILED");
    pub(crate) const DATA_CACHE_DECODE_FAILED: Self = Self("DATA_CACHE_DECODE_FAILED");
    #[cfg(feature = "streaming")]
    pub(crate) const EVENT_HOOK_FAILED: Self = Self("EVENT_HOOK_FAILED");
    pub(crate) const EXTERNAL_LYRICS_FAILED: Self = Self("EXTERNAL_LYRICS_FAILED");
    pub(crate) const HOME_FEED_FAILED: Self = Self("HOME_FEED_FAILED");
    #[cfg(feature = "image")]
    pub(crate) const IMAGE_ENCODE_FAILED: Self = Self("IMAGE_ENCODE_FAILED");
    #[cfg(feature = "image")]
    pub(crate) const IMAGE_CACHE_MISS: Self = Self("IMAGE_CACHE_MISS");
    #[cfg(feature = "image")]
    pub(crate) const IMAGE_RENDER_FAILED: Self = Self("IMAGE_RENDER_FAILED");
    pub(crate) const JOURNAL_DECODE_FAILED: Self = Self("JOURNAL_DECODE_FAILED");
    pub(crate) const JOURNAL_OPEN_FAILED: Self = Self("JOURNAL_OPEN_FAILED");
    pub(crate) const SESSION_HISTORY_DECODE_FAILED: Self = Self("SESSION_HISTORY_DECODE_FAILED");
    pub(crate) const SESSION_HISTORY_OPEN_FAILED: Self = Self("SESSION_HISTORY_OPEN_FAILED");
    pub(crate) const SESSION_HISTORY_SAVE_FAILED: Self = Self("SESSION_HISTORY_SAVE_FAILED");
    pub(crate) const KEYMAP_OPEN_FAILED: Self = Self("KEYMAP_OPEN_FAILED");
    pub(crate) const LISTENBRAINZ_ARTIST_ENRICHMENT_FAILED: Self =
        Self("LISTENBRAINZ_ARTIST_ENRICHMENT_FAILED");
    pub(crate) const LISTENBRAINZ_ALBUM_RESOLUTION_FAILED: Self =
        Self("LISTENBRAINZ_ALBUM_RESOLUTION_FAILED");
    pub(crate) const LISTENBRAINZ_RECORDING_RESOLUTION_FAILED: Self =
        Self("LISTENBRAINZ_RECORDING_RESOLUTION_FAILED");
    pub(crate) const OAUTH_CALLBACK_ACCEPT_FAILED: Self = Self("OAUTH_CALLBACK_ACCEPT_FAILED");
    pub(crate) const OAUTH_CALLBACK_READ_FAILED: Self = Self("OAUTH_CALLBACK_READ_FAILED");
    pub(crate) const OAUTH_CALLBACK_WRITE_FAILED: Self = Self("OAUTH_CALLBACK_WRITE_FAILED");
    #[cfg(feature = "streaming")]
    pub(crate) const PLAYER_EVENT_CONVERT_FAILED: Self = Self("PLAYER_EVENT_CONVERT_FAILED");
    pub(crate) const PLAYER_EVENT_HANDLE_FAILED: Self = Self("PLAYER_EVENT_HANDLE_FAILED");
    pub(crate) const PROVIDER_SESSION_PERSIST_FAILED: Self =
        Self("PROVIDER_SESSION_PERSIST_FAILED");
    pub(crate) const PROVIDER_SESSION_RESTORE_FAILED: Self =
        Self("PROVIDER_SESSION_RESTORE_FAILED");
    pub(crate) const PROVIDER_SWITCH_PREPARE_FAILED: Self = Self("PROVIDER_SWITCH_PREPARE_FAILED");
    pub(crate) const PROVIDER_SWITCH_RESUME_FAILED: Self = Self("PROVIDER_SWITCH_RESUME_FAILED");
    pub(crate) const REQUEST_HANDLE_FAILED: Self = Self("REQUEST_HANDLE_FAILED");
    pub(crate) const REQUEST_SHUTDOWN_FAILED: Self = Self("REQUEST_SHUTDOWN_FAILED");
    pub(crate) const SOCKET_BIND_FAILED: Self = Self("SOCKET_BIND_FAILED");
    pub(crate) const SOCKET_DECODE_FAILED: Self = Self("SOCKET_DECODE_FAILED");
    pub(crate) const SOCKET_RECEIVE_FAILED: Self = Self("SOCKET_RECEIVE_FAILED");
    pub(crate) const SPOTIFY_ARTIST_ALBUMS_FAILED: Self = Self("SPOTIFY_ARTIST_ALBUMS_FAILED");
    pub(crate) const SPOTIFY_ARTIST_TRACKS_FAILED: Self = Self("SPOTIFY_ARTIST_TRACKS_FAILED");
    #[cfg(feature = "streaming")]
    pub(crate) const SPOTIFY_AUDIO_OUTPUT_FAILED: Self = Self("SPOTIFY_AUDIO_OUTPUT_FAILED");
    pub(crate) const SPOTIFY_AUTH_FAILED: Self = Self("SPOTIFY_AUTH_FAILED");
    pub(crate) const SPOTIFY_STARTUP_DEGRADED: Self = Self("SPOTIFY_STARTUP_DEGRADED");
    pub(crate) const SPOTIFY_DEVICE_DISCOVERY_FAILED: Self =
        Self("SPOTIFY_DEVICE_DISCOVERY_FAILED");
    pub(crate) const SPOTIFY_DEVICE_TRANSFER_FAILED: Self = Self("SPOTIFY_DEVICE_TRANSFER_FAILED");
    pub(crate) const SPOTIFY_PLAYBACK_REFRESH_FAILED: Self =
        Self("SPOTIFY_PLAYBACK_REFRESH_FAILED");
    pub(crate) const SPOTIFY_PLAYBACK_RESUME_FAILED: Self = Self("SPOTIFY_PLAYBACK_RESUME_FAILED");
    #[cfg(feature = "streaming")]
    pub(crate) const SPOTIFY_STARTUP_PAUSE_FAILED: Self = Self("SPOTIFY_STARTUP_PAUSE_FAILED");
    #[cfg(feature = "streaming")]
    pub(crate) const SPOTIFY_STREAM_SHUTDOWN_FAILED: Self = Self("SPOTIFY_STREAM_SHUTDOWN_FAILED");
    pub(crate) const SPOTIFY_TOKEN_CACHE_READ_FAILED: Self =
        Self("SPOTIFY_TOKEN_CACHE_READ_FAILED");
    pub(crate) const SPOTIFY_TOKEN_REFRESH_FAILED: Self = Self("SPOTIFY_TOKEN_REFRESH_FAILED");
    pub(crate) const TERMINAL_EVENT_HANDLE_FAILED: Self = Self("TERMINAL_EVENT_HANDLE_FAILED");
    pub(crate) const TERMINAL_POLL_FAILED: Self = Self("TERMINAL_POLL_FAILED");
    pub(crate) const TERMINAL_READ_FAILED: Self = Self("TERMINAL_READ_FAILED");
    pub(crate) const TERMINAL_SIZE_FAILED: Self = Self("TERMINAL_SIZE_FAILED");
    pub(crate) const THEME_OPEN_FAILED: Self = Self("THEME_OPEN_FAILED");
    #[cfg(feature = "image")]
    pub(crate) const UI_PICKER_INIT_FAILED: Self = Self("UI_PICKER_INIT_FAILED");
    pub(crate) const UI_RENDER_FAILED: Self = Self("UI_RENDER_FAILED");
    pub(crate) const UNIFIED_PLAYLIST_LOAD_FAILED: Self = Self("UNIFIED_PLAYLIST_LOAD_FAILED");
    pub(crate) const UNIFIED_PLAYLIST_RECOVERY_USED: Self = Self("UNIFIED_PLAYLIST_RECOVERY_USED");
    pub(crate) const UNIFIED_QUEUE_ITEM_UNAVAILABLE: Self = Self("UNIFIED_QUEUE_ITEM_UNAVAILABLE");
    pub(crate) const YOUTUBE_AUTH_TEST_FAILED: Self = Self("YOUTUBE_AUTH_TEST_FAILED");
    pub(crate) const YOUTUBE_AUDIO_PREFETCH_FAILED: Self = Self("YOUTUBE_AUDIO_PREFETCH_FAILED");
    pub(crate) const YOUTUBE_BROWSER_LOGIN_FAILED: Self = Self("YOUTUBE_BROWSER_LOGIN_FAILED");
    pub(crate) const YOUTUBE_CONTEXT_LOAD_FAILED: Self = Self("YOUTUBE_CONTEXT_LOAD_FAILED");
    pub(crate) const YOUTUBE_DESCRIPTOR_PREFETCH_FAILED: Self =
        Self("YOUTUBE_DESCRIPTOR_PREFETCH_FAILED");
    pub(crate) const YOUTUBE_LIBRARY_FETCH_FAILED: Self = Self("YOUTUBE_LIBRARY_FETCH_FAILED");
    pub(crate) const YOUTUBE_LIBRARY_GRID_MISSING: Self = Self("YOUTUBE_LIBRARY_GRID_MISSING");
    pub(crate) const YOUTUBE_LYRICS_FAILED: Self = Self("YOUTUBE_LYRICS_FAILED");
    pub(crate) const YOUTUBE_PLAYBACK_VALIDATION_FAILED: Self =
        Self("YOUTUBE_PLAYBACK_VALIDATION_FAILED");
    pub(crate) const YOUTUBE_PLAYLIST_CONTEXT_FETCH_FAILED: Self =
        Self("YOUTUBE_PLAYLIST_CONTEXT_FETCH_FAILED");
    pub(crate) const YOUTUBE_PLAYLIST_MUTATION_FAILED: Self =
        Self("YOUTUBE_PLAYLIST_MUTATION_FAILED");
    pub(crate) const YOUTUBE_PLAYLIST_SYNC_PARTIAL: Self = Self("YOUTUBE_PLAYLIST_SYNC_PARTIAL");
    pub(crate) const YOUTUBE_SEARCH_FAILED: Self = Self("YOUTUBE_SEARCH_FAILED");
    pub(crate) const YOUTUBE_SOURCE_REFRESH_FAILED: Self = Self("YOUTUBE_SOURCE_REFRESH_FAILED");
    pub(crate) const YOUTUBE_THUMBNAIL_FAILED: Self = Self("YOUTUBE_THUMBNAIL_FAILED");
    pub(crate) const fn as_str(self) -> &'static str {
        self.0
    }
}

/// A diagnostic representation that deliberately does not retain or format the
/// source error. `source_type` is compile-time type metadata, not error text.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct SafeDiagnosticError {
    code: DiagnosticCode,
    category: ErrorCategory,
    source_type: &'static str,
}

impl SafeDiagnosticError {
    pub(crate) fn new<E: ?Sized>(
        code: DiagnosticCode,
        category: ErrorCategory,
        _source: &E,
    ) -> Self {
        Self {
            code,
            category,
            source_type: std::any::type_name::<E>(),
        }
    }
}

impl fmt::Display for SafeDiagnosticError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            formatter,
            "code={} category={} source_type={}",
            self.code.as_str(),
            self.category.as_str(),
            self.source_type
        )
    }
}

pub(crate) fn safe_error<E: ?Sized>(
    code: DiagnosticCode,
    category: ErrorCategory,
    source: &E,
) -> SafeDiagnosticError {
    SafeDiagnosticError::new(code, category, source)
}

/// Static, privacy-safe metadata carried through an `anyhow` context chain.
/// The provider error remains available to application code, but diagnostics
/// need only downcast this marker and never inspect or format that source.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct PreservedDiagnostic {
    code: DiagnosticCode,
    category: ErrorCategory,
}

impl fmt::Display for PreservedDiagnostic {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            formatter,
            "code={} category={}",
            self.code.as_str(),
            self.category.as_str()
        )
    }
}

pub(crate) fn preserve_error_diagnostic(
    error: anyhow::Error,
    code: DiagnosticCode,
    category: ErrorCategory,
) -> anyhow::Error {
    error.context(PreservedDiagnostic { code, category })
}

pub(crate) fn preserved_error_diagnostic(
    error: &anyhow::Error,
) -> Option<(DiagnosticCode, ErrorCategory)> {
    error
        .downcast_ref::<PreservedDiagnostic>()
        .map(|diagnostic| (diagnostic.code, diagnostic.category))
}

macro_rules! log_safe_error {
    (debug, $code:expr, $category:expr, $source:expr, $message:literal) => {
        tracing::debug!(
            diagnostic = %crate::observability::safe_error($code, $category, $source),
            $message
        )
    };
    (info, $code:expr, $category:expr, $source:expr, $message:literal) => {
        tracing::info!(
            diagnostic = %crate::observability::safe_error($code, $category, $source),
            $message
        )
    };
    (warn, $code:expr, $category:expr, $source:expr, $message:literal) => {
        tracing::warn!(
            diagnostic = %crate::observability::safe_error($code, $category, $source),
            $message
        )
    };
    (error, $code:expr, $category:expr, $source:expr, $message:literal) => {
        tracing::error!(
            diagnostic = %crate::observability::safe_error($code, $category, $source),
            $message
        )
    };
}

pub(crate) use log_safe_error;

#[cfg(test)]
mod tests {
    use std::{
        collections::VecDeque,
        path::{Path, PathBuf},
        sync::Arc,
        time::Duration,
    };

    use parking_lot::Mutex;
    use tracing_subscriber::{layer::SubscriberExt, Layer};

    use super::{
        preserve_error_diagnostic, preserved_error_diagnostic, safe_error, DiagnosticCode,
        ErrorCategory,
    };

    #[test]
    fn safe_error_never_formats_the_source_value_or_control_characters() {
        let secret = "token=phase6a-secret\r\nforged-event=true";
        let source = anyhow::anyhow!(secret);
        let safe = safe_error(
            DiagnosticCode::REQUEST_HANDLE_FAILED,
            ErrorCategory::Contract,
            &source,
        );
        let display = safe.to_string();
        let debug = format!("{safe:?}");

        for output in [&display, &debug] {
            assert!(!output.contains(secret));
            assert!(!output.contains("phase6a-secret"));
            assert!(!output.contains('\r'));
            assert!(!output.contains('\n'));
        }
        assert!(display.contains("REQUEST_HANDLE_FAILED"));
        assert!(display.contains("contract"));
    }

    #[test]
    fn listenbrainz_recording_failures_have_a_dedicated_safe_code() {
        assert_eq!(
            DiagnosticCode::LISTENBRAINZ_RECORDING_RESOLUTION_FAILED.as_str(),
            "LISTENBRAINZ_RECORDING_RESOLUTION_FAILED"
        );
        assert_ne!(
            DiagnosticCode::LISTENBRAINZ_RECORDING_RESOLUTION_FAILED,
            DiagnosticCode::LISTENBRAINZ_ARTIST_ENRICHMENT_FAILED
        );
    }

    #[test]
    fn listenbrainz_album_failures_have_a_dedicated_safe_code() {
        assert_eq!(
            DiagnosticCode::LISTENBRAINZ_ALBUM_RESOLUTION_FAILED.as_str(),
            "LISTENBRAINZ_ALBUM_RESOLUTION_FAILED"
        );
        assert_ne!(
            DiagnosticCode::LISTENBRAINZ_ALBUM_RESOLUTION_FAILED,
            DiagnosticCode::LISTENBRAINZ_RECORDING_RESOLUTION_FAILED
        );
    }

    #[test]
    fn spotify_artist_album_failures_have_a_dedicated_safe_code() {
        assert_eq!(
            DiagnosticCode::SPOTIFY_ARTIST_ALBUMS_FAILED.as_str(),
            "SPOTIFY_ARTIST_ALBUMS_FAILED"
        );
        assert_ne!(
            DiagnosticCode::SPOTIFY_ARTIST_ALBUMS_FAILED,
            DiagnosticCode::SPOTIFY_ARTIST_TRACKS_FAILED
        );
    }

    #[test]
    fn preserved_diagnostic_survives_generic_context_without_exposing_provider_text() {
        const PRIVATE_REASON: &str = "provider reason with video-id=private-id token=private-token";
        let error = preserve_error_diagnostic(
            anyhow::anyhow!(PRIVATE_REASON),
            DiagnosticCode::YOUTUBE_PLAYBACK_VALIDATION_FAILED,
            ErrorCategory::ProviderUnavailable,
        )
        .context("resolve native YouTube audio source");

        let (code, category) =
            preserved_error_diagnostic(&error).expect("safe metadata remains downcastable");
        assert_eq!(code, DiagnosticCode::YOUTUBE_PLAYBACK_VALIDATION_FAILED);
        assert_eq!(category, ErrorCategory::ProviderUnavailable);

        let rendered = safe_error(code, category, &error).to_string();
        assert!(rendered.contains("YOUTUBE_PLAYBACK_VALIDATION_FAILED"));
        assert!(rendered.contains("provider_unavailable"));
        assert!(!rendered.contains(PRIVATE_REASON));
        assert!(!rendered.contains("private-id"));
        assert!(!rendered.contains("private-token"));
    }

    #[test]
    fn maximum_application_verbosity_keeps_file_and_ui_sinks_secret_free() {
        let directory = std::env::temp_dir().join(format!(
            "unified-player-observability-test-{}-{}",
            std::process::id(),
            super::protocol::random_hex::<6>()
        ));
        std::fs::create_dir_all(&directory).unwrap();
        let ui_output = Arc::new(Mutex::new(VecDeque::new()));
        let (handle, mut runtime) =
            super::sink::start(&directory, ui_output.clone(), super::SinkPolicy::default())
                .unwrap();
        let safe = safe_error(
            DiagnosticCode::REQUEST_HANDLE_FAILED,
            ErrorCategory::Contract,
            &anyhow::anyhow!(
                "credential=phase6a-credential query=phase6a-query \
                 clipboard=phase6a-clipboard media=phase6a-media body=phase6a-body \
                 https://example.invalid/media?id=private\r\nforged=true"
            ),
        );
        let subscriber = tracing_subscriber::registry()
            .with(tracing_subscriber::filter::LevelFilter::TRACE)
            .with(super::DiagnosticLayer::new(handle).with_filter(
                tracing_subscriber::filter::filter_fn(super::is_application_event),
            ));

        tracing::subscriber::with_default(subscriber, || {
            tracing::trace!(diagnostic = %safe, "Synthetic trace failure");
            tracing::debug!(diagnostic = %safe, "Synthetic debug failure");
            tracing::info!(diagnostic = %safe, "Synthetic info failure");
            tracing::warn!(diagnostic = %safe, "Synthetic warning failure");
            tracing::error!(diagnostic = %safe, "Synthetic error failure");
            tracing::error!(
                target: "librespot_core",
                "third-party token=phase6a-external-secret"
            );
        });
        runtime.shutdown(Duration::from_secs(1)).unwrap();

        let file_text = std::fs::read_dir(&directory)
            .unwrap()
            .filter_map(Result::ok)
            .map(|entry| std::fs::read_to_string(entry.path()).unwrap())
            .collect::<Vec<_>>()
            .join("\n");
        let ui_text = ui_output
            .lock()
            .iter()
            .map(super::UiDiagnosticEntry::render_line)
            .collect::<Vec<_>>()
            .join("\n");

        for output in [file_text, ui_text] {
            assert!(!output.contains("phase6a-credential"));
            assert!(!output.contains("phase6a-query"));
            assert!(!output.contains("phase6a-clipboard"));
            assert!(!output.contains("phase6a-media"));
            assert!(!output.contains("phase6a-body"));
            assert!(!output.contains("example.invalid"));
            assert!(!output.contains("private"));
            assert!(!output.contains("forged=true"));
            assert!(!output.contains("phase6a-external-secret"));
            assert!(output.contains("Synthetic"));
            assert!(output.contains("REQUEST_HANDLE_FAILED"));
            assert!(output.contains("contract"));
        }
        std::fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn startup_diagnostic_fields_survive_jsonl_sanitization() {
        let directory = std::env::temp_dir().join(format!(
            "unified-player-startup-diagnostic-test-{}-{}",
            std::process::id(),
            super::protocol::random_hex::<6>()
        ));
        std::fs::create_dir_all(&directory).unwrap();
        let ui_output = Arc::new(Mutex::new(VecDeque::new()));
        let (handle, mut runtime) =
            super::sink::start(&directory, ui_output, super::SinkPolicy::default()).unwrap();
        let safe = safe_error(
            DiagnosticCode::SPOTIFY_STARTUP_DEGRADED,
            ErrorCategory::RateLimited,
            &anyhow::anyhow!("provider response omitted from diagnostics"),
        );
        let subscriber = tracing_subscriber::registry()
            .with(tracing_subscriber::filter::LevelFilter::TRACE)
            .with(super::DiagnosticLayer::new(handle).with_filter(
                tracing_subscriber::filter::filter_fn(super::is_application_event),
            ));

        tracing::subscriber::with_default(subscriber, || {
            tracing::warn!(
                diagnostic = %safe,
                phase = "current_user",
                status_class = "rate_limited",
                retryable = true,
                retry_after_ms = 12_000_u128,
                "Spotify startup degraded"
            );
        });
        runtime.shutdown(Duration::from_secs(1)).unwrap();

        let contents = std::fs::read_dir(&directory)
            .unwrap()
            .filter_map(Result::ok)
            .map(|entry| std::fs::read_to_string(entry.path()).unwrap())
            .collect::<Vec<_>>()
            .join("\n");
        let event: super::DiagnosticEvent = serde_json::from_str(contents.trim()).unwrap();
        assert_eq!(event.event_code, "SPOTIFY_STARTUP_DEGRADED");
        assert_eq!(event.fields.phase.as_deref(), Some("current_user"));
        assert_eq!(event.fields.status_class.as_deref(), Some("rate_limited"));
        assert_eq!(event.fields.retryable, Some(true));
        assert_eq!(event.fields.retry_after_ms, Some(12_000));
        std::fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn panic_report_is_static_and_omits_payload_and_backtrace() {
        assert!(!super::SAFE_PANIC_REPORT.contains("phase6a-secret"));
        assert!(!super::SAFE_PANIC_REPORT.contains('\r'));
        assert!(!super::SAFE_PANIC_REPORT.contains('\n'));
        assert!(super::SAFE_PANIC_REPORT.contains("payload and backtrace omitted"));
        let first = super::panic_fingerprint("private/source/location.rs", 42, 7);
        let second = super::panic_fingerprint("private/source/location.rs", 42, 7);
        assert_eq!(first, second);
        assert_eq!(first.len(), 12);
        assert!(!first.contains("private"));
        assert!(!first.contains("location"));
    }

    #[test]
    fn tracing_call_sites_do_not_format_forbidden_runtime_values() {
        let source_root = Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
        let mut files = Vec::new();
        collect_rust_files(&source_root, &mut files);
        let forbidden = [
            "{access_token}",
            "{content}",
            "{device_id",
            "{err",
            "{error",
            "{info",
            "{message}",
            "{playlist",
            "{query",
            "{request_target}",
            "{seed_uri}",
            "{text}",
            "{token",
            "{track_id}",
            "{uri",
            "{url}",
            "= ?config",
            "= ?connect_config",
            "= ?devices",
            "= ?event",
            "= ?item",
            "error = %error",
            "path.display()",
            "playlist.id",
            "playlist.name",
            "request = ?request",
            "response.headers()",
            "backtrace::Backtrace",
            concat!("tracing", "::info!(\"Configurations:"),
            concat!("tracing", "::info!(\"Got a new player event:"),
            concat!("tracing", "::info!(\"Get album context:"),
            concat!("tracing", "::info!(\"Get artist context:"),
            concat!("tracing", "::info!(\"Get playlist context:"),
            concat!("tracing", "::info!(\"Get show context:"),
        ];

        let mut violations = Vec::new();
        for path in files {
            let source = std::fs::read_to_string(&path).expect("read Rust source");
            if source.contains(concat!("log", "::")) {
                violations.push(format!(
                    "{}: direct log facade calls bypass the safe tracing policy",
                    path.display()
                ));
            }
            for invocation in tracing_invocations(&source) {
                for pattern in forbidden {
                    if invocation.contains(pattern) {
                        violations.push(format!("{}: {pattern}", path.display()));
                    }
                }
            }
        }

        assert!(
            violations.is_empty(),
            "unsafe tracing source patterns found:\n{}",
            violations.join("\n")
        );
    }

    fn collect_rust_files(directory: &Path, output: &mut Vec<PathBuf>) {
        for entry in std::fs::read_dir(directory).expect("read source directory") {
            let path = entry.expect("read source entry").path();
            if path.is_dir() {
                collect_rust_files(&path, output);
            } else if path.extension().is_some_and(|extension| extension == "rs") {
                output.push(path);
            }
        }
    }

    fn tracing_invocations(source: &str) -> impl Iterator<Item = &str> {
        const MARKER: &str = concat!("tracing", "::");
        source.match_indices(MARKER).map(|(start, _)| {
            let remainder = &source[start..];
            let end = remainder.find(");").map_or(remainder.len(), |end| end + 2);
            &remainder[..end]
        })
    }
}
