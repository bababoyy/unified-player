#![allow(unused_imports)]

mod analysis;
mod comparison_store;
mod controller;
mod derivative_builder;
mod derivative_store;
mod diff;
mod model;
mod operator;
mod payload;
mod private_view;
mod recorder;
mod replay;
mod replay_store;
mod sanitize;
mod security;
mod store;
mod writer;

pub(crate) use comparison_store::{
    ComparisonArtifactService, ComparisonFacadeError, SafeComparisonArtifact,
};
pub(crate) use controller::{
    CaptureClock, CaptureController, CapturePermit, CaptureRefSource, ControllerError,
    RandomCaptureRefSource, RecordAdmission, SystemCaptureClock, TransitionOutcome,
};
pub(crate) use model::{
    CaptureByteBucket, CaptureCompleteness, CaptureLimits, CapturePurpose, CaptureRecordKind,
    CaptureRecordV1, CaptureRef, EndpointRole, ExchangeRef, IncompleteReason, PrivateCaptureV1,
    ProviderClientKind, SafeArtifactReview, SafeCaptureRef, SafeCaptureSnapshot, SafeCaptureState,
    SafeOperationRef, SafeTerminalCategory, SensitiveBytes, SensitiveString, TransportKind,
};
pub(crate) use operator::{
    prepare_operator, prepare_operator_default, unavailable_operator_handle,
    CaptureOperatorBackend, CaptureOperatorHandle, CaptureOperatorWorker, FolderOpenError,
    FreshReplayAcknowledgement, OperatorCompletion, OperatorPrepareError, OperatorSubmitError,
    OperatorWorkerError, PrivateDerivativeDestination, PrivateFolderOpener, SafeArtifactLabel,
    SafeComparisonView, SafeDerivativePreview, SafeDerivativeState, SafeDerivativeView,
    SafeOperatorAction, SafeOperatorArtifact, SafeOperatorDisposition, SafeOperatorFailure,
    SafeOperatorPhase, SafeOperatorResult, SafeOperatorSnapshot, SafeReplayOutcome, SafeReplayView,
    SensitiveFolderAcknowledgement, SystemPrivateFolderOpener, VaultOperatorBackend,
    MAX_SAFE_OPERATOR_ARTIFACTS,
};
pub(crate) use payload::{
    decode_fields as decode_private_fields, encode_fields as encode_private_fields,
    encode_headers as encode_private_headers, encode_http_request as encode_private_http_request,
    encode_http_request_bounded as encode_private_http_request_bounded,
    encode_http_response as encode_private_http_response,
    encode_http_response_bounded as encode_private_http_response_bounded, field as private_field,
    DecodedPrivatePayload, PrivateField, PrivatePayloadError, PrivatePayloadKind,
    PRIVATE_PAYLOAD_SCHEMA_VERSION,
};
pub(crate) use private_view::PrivateViewSelection;
pub(crate) use recorder::{
    prepare_runtime, CaptureHandle, CaptureRuntimeError, CaptureSession, CaptureWorker,
    CaptureWorkerError, RecordOutcome,
};
pub(crate) use replay::{
    run_fresh_replay, run_offline_replay, AllowlistedPlayerEndpoint, AllowlistedReplayMethod,
    CurrentMaterialFailure, CurrentMaterialRequest, FreshPlayerAdapterResult, FreshPlayerRequest,
    FreshReplayOutcome, FreshReplayPolicy, FreshSemanticReplayAdapter, OfflineAdapterResult,
    OfflineReplayAdapter, OfflineReplayInput, OfflineReplayOutcome, PlayerPostCapability,
    ReplayAuthKind, ReplayClientVersionPolicy, ReplayClock, ReplayCredentialPolicy, ReplayDecision,
    ReplayFuture, ReplayParserOutcome, ReplayPlayerClient, ReplayProviderOutcome,
    ReplayQualityPolicy, ReplayRecipeBuildError, ReplayRecipeIncomplete, ReplayRecipeParts,
    ReplayRecipeV1, ReplaySelectionOutcome, ReplayTerminalOutcome, ReplayTimer, TokioReplayTimer,
};
pub(crate) use replay_store::{
    ReplayArtifactService, ReplayFacadeError, ReplayMode, SafeReplayArtifact,
};
pub(crate) use security::{
    ensure_private_root_separate, CapturePassphrase, CapturePassphraseInput, SecurityError,
};
pub(crate) use store::{
    CaptureStore, MaintenanceReport, StoreClock, StoreError, StoredArtifact, SystemStoreClock,
};
pub(crate) use writer::VaultFormatError;

#[cfg(test)]
mod boundary_tests {
    #[test]
    fn private_capture_has_no_tier_one_dependency() {
        let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("src")
            .join("developer_capture");
        for entry in std::fs::read_dir(root).unwrap() {
            let path = entry.unwrap().path();
            if path.extension().and_then(std::ffi::OsStr::to_str) != Some("rs")
                || path.file_name().and_then(std::ffi::OsStr::to_str) == Some("mod.rs")
            {
                continue;
            }
            let source = std::fs::read_to_string(path).unwrap();
            assert!(!source.contains("observability"));
        }
    }

    #[test]
    fn tier_one_sinks_cannot_read_or_format_private_evidence() {
        let source_root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
        let tier_one_roots = [
            source_root.join("observability"),
            source_root.join("ui"),
            source_root.join("event"),
            source_root.join("state"),
            source_root.join("cli"),
            source_root.join("main.rs"),
            source_root.join("command.rs"),
            source_root.join("runtime.rs"),
        ];
        let forbidden = [
            "decode_private_fields",
            "read_private(",
            "PrivateCaptureV1",
            "CaptureRecordV1",
            "DecodedPrivatePayload",
            "PrivateField",
            "ProviderDiagnosticDerivativeV1",
            "SeededForbiddenScannerV1",
            "DerivativeDirectoryStoreV1",
            "SensitiveBytes",
            "SensitiveString",
        ];
        for root in tier_one_roots {
            let mut pending = vec![root];
            while let Some(path) = pending.pop() {
                if path.is_dir() {
                    pending.extend(
                        std::fs::read_dir(path)
                            .unwrap()
                            .map(|entry| entry.unwrap().path()),
                    );
                    continue;
                }
                if path.extension().and_then(std::ffi::OsStr::to_str) != Some("rs") {
                    continue;
                }
                let source = std::fs::read_to_string(&path).unwrap();
                for identifier in forbidden {
                    assert!(
                        !source.contains(identifier),
                        "Tier 1 source {} can access {identifier}",
                        path.display()
                    );
                }
            }
        }
    }

    #[test]
    fn masked_private_inspection_has_one_terminal_only_cli_boundary() {
        let source_root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
        let private_view = source_root
            .join("developer_capture")
            .join("private_view.rs");
        let operator = source_root.join("developer_capture").join("operator.rs");
        let boundary_test = source_root.join("developer_capture").join("mod.rs");
        let cli = source_root.join("cli").join("private_capture.rs");
        let allowed = [&private_view, &operator, &boundary_test, &cli];
        let boundary_identifiers = [
            "authorize_private_terminal_output",
            "write_masked_private_view_to_terminal",
            "inspect_masked_private_evidence_to_terminal",
        ];

        let mut pending = vec![source_root.clone()];
        while let Some(path) = pending.pop() {
            if path.is_dir() {
                pending.extend(
                    std::fs::read_dir(path)
                        .unwrap()
                        .map(|entry| entry.unwrap().path()),
                );
                continue;
            }
            if path.extension().and_then(std::ffi::OsStr::to_str) != Some("rs")
                || allowed.contains(&&path)
            {
                continue;
            }
            let source = std::fs::read_to_string(&path).unwrap();
            for identifier in boundary_identifiers {
                assert!(
                    !source.contains(identifier),
                    "Private terminal inspection escaped into {} through {identifier}",
                    path.display()
                );
            }
        }

        let operator_source = std::fs::read_to_string(operator).unwrap();
        assert!(!operator_source.contains("write_masked_private_view("));
        assert!(!operator_source.contains("impl std::io::Write"));
        assert_eq!(
            operator_source
                .matches("inspect_masked_private_evidence_to_terminal")
                .count(),
            1
        );
        assert!(
            operator_source
                .find("authorize_private_terminal_output")
                .unwrap()
                < operator_source
                    .find("read_private_artifact_by_safe_ref")
                    .unwrap(),
            "terminal authorization must happen before private artifact decryption"
        );
        let cli_source = std::fs::read_to_string(cli).unwrap();
        assert_eq!(
            cli_source
                .matches("inspect_masked_private_evidence_to_terminal")
                .count(),
            1
        );
    }

    #[test]
    fn provider_production_code_can_write_but_cannot_read_private_evidence() {
        fn collect_production_rust_sources(
            root: &std::path::Path,
            sources: &mut Vec<std::path::PathBuf>,
        ) {
            for entry in std::fs::read_dir(root).unwrap() {
                let path = entry.unwrap().path();
                if path.is_dir() {
                    if path.file_name().is_some_and(|name| name == "tests") {
                        continue;
                    }
                    collect_production_rust_sources(&path, sources);
                } else if path.extension().is_some_and(|extension| extension == "rs")
                    && path.file_name().is_none_or(|name| name != "tests.rs")
                {
                    sources.push(path);
                }
            }
        }

        let source_root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
        let youtube_root = source_root.join("client").join("youtube");
        let playback_root = youtube_root.join("playback");
        assert!(
            playback_root.is_dir(),
            "YouTube playback source directory is missing"
        );
        let mut provider_sources = vec![youtube_root.join("browser_auth.rs")];
        collect_production_rust_sources(&playback_root, &mut provider_sources);
        provider_sources.sort();
        assert!(
            provider_sources
                .iter()
                .any(|path| path == &playback_root.join("mod.rs")),
            "YouTube playback module root is missing from the private-source boundary scan"
        );
        let forbidden = [
            "decode_private_fields",
            ".read_private(",
            "PrivateCaptureV1",
            "CaptureRecordV1",
            "SensitiveBytes",
            "SensitiveString",
        ];
        for path in provider_sources {
            let source = std::fs::read_to_string(&path).unwrap();
            let production = source
                .split_once("\n#[cfg(test)]\nmod tests")
                .map_or(source.as_str(), |(production, _)| production);
            for identifier in forbidden {
                assert!(
                    !production.contains(identifier),
                    "Provider source {} can read {identifier} outside tests",
                    path.display()
                );
            }
        }
    }
}
