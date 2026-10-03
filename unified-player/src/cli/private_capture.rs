//! Provider-scoped CLI for the encrypted private capture vault.
//!
//! This module talks directly to the typed vault operator backend and does not
//! use the interactive application's request channel. Normal commands render
//! bounded Tier 1 facts or a scanned Tier 3 derivative. The separately
//! acknowledged `inspect` command renders masked but still-private Tier 2
//! evidence only to an interactive terminal.

use std::{
    fmt,
    io::{IsTerminal as _, Write},
    path::PathBuf,
};

use clap::ArgMatches;
use crossterm::{
    event::{self, Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers},
    terminal,
};
use tokio_util::sync::CancellationToken;

use crate::{
    cli::config,
    client,
    client::{YouTubeFreshReplayAdapter, YouTubeOfflineReplayAdapter},
    developer_capture::{
        ensure_private_root_separate, CaptureByteBucket, CaptureCompleteness, CaptureLimits,
        CaptureOperatorBackend, CapturePassphrase, CapturePassphraseInput, CapturePurpose,
        CaptureRuntimeError, PrivateDerivativeDestination, PrivateViewSelection,
        SafeArtifactReview, SafeCaptureRef, SafeCaptureState, SafeComparisonView,
        SafeDerivativeView, SafeOperationRef, SafeOperatorFailure, SafeReplayView,
        SafeTerminalCategory, SystemPrivateFolderOpener, TokioReplayTimer, VaultOperatorBackend,
    },
};

type CliBackend = VaultOperatorBackend<
    YouTubeOfflineReplayAdapter,
    YouTubeFreshReplayAdapter,
    TokioReplayTimer,
    SystemPrivateFolderOpener,
>;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum PrivateCaptureCliError {
    VaultUnavailable,
    OperatorUnavailable,
    PassphraseTerminalRequired,
    PassphraseInputUnavailable,
    PassphraseInvalid,
    PassphraseCancelled,
    PrivateOutputTerminalRequired,
    InvalidCommand,
    Action(SafeOperatorFailure),
}

impl fmt::Display for PrivateCaptureCliError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::VaultUnavailable => "the private capture vault is unavailable",
            Self::OperatorUnavailable => "the private capture operator is unavailable",
            Self::PassphraseTerminalRequired => {
                "private capture passphrases require an interactive terminal"
            }
            Self::PassphraseInputUnavailable => {
                "private capture passphrase input could not be completed"
            }
            Self::PassphraseInvalid => "the private capture passphrase is invalid",
            Self::PassphraseCancelled => "private capture passphrase entry was cancelled",
            Self::PrivateOutputTerminalRequired => {
                "private evidence inspection requires an interactive output terminal"
            }
            Self::InvalidCommand => "the private capture command is invalid",
            Self::Action(failure) => failure.as_str(),
        })
    }
}

impl std::error::Error for PrivateCaptureCliError {}

impl From<SafeOperatorFailure> for PrivateCaptureCliError {
    fn from(value: SafeOperatorFailure) -> Self {
        Self::Action(value)
    }
}

pub(super) fn handle(args: &ArgMatches, configs: &config::Configs) -> anyhow::Result<()> {
    run(args, configs).map_err(anyhow::Error::new)
}

fn run(args: &ArgMatches, configs: &config::Configs) -> Result<(), PrivateCaptureCliError> {
    let (command, command_args) = args
        .subcommand()
        .ok_or(PrivateCaptureCliError::InvalidCommand)?;

    let output = match command {
        "status" => {
            let mut backend = open_backend(configs)?;
            render_status(backend.list_safe_refs()?.len())
        }
        "list" => {
            let mut backend = open_backend(configs)?;
            render_list(&backend.list_safe_refs()?)
        }
        "review" => {
            let capture_ref = required_ref(command_args, "capture_ref")?;
            let mut backend = open_backend(configs)?;
            let passphrase = prompt_for_passphrase()?;
            render_review(backend.review(capture_ref, &passphrase)?)
        }
        "replay" => {
            let capture_ref = required_ref(command_args, "capture_ref")?;
            let offline = command_args.get_flag("offline");
            let fresh =
                command_args.get_flag("fresh") && command_args.get_flag("acknowledge_network");
            if offline == fresh {
                return Err(PrivateCaptureCliError::InvalidCommand);
            }
            let mut backend = open_backend(configs)?;
            let passphrase = prompt_for_passphrase()?;
            let replay = if offline {
                backend.replay_offline(capture_ref, &passphrase)?
            } else {
                let runtime = tokio::runtime::Runtime::new()
                    .map_err(|_| PrivateCaptureCliError::OperatorUnavailable)?;
                runtime.block_on(backend.replay_fresh(
                    capture_ref,
                    &passphrase,
                    &CancellationToken::new(),
                ))?
            };
            render_replay(replay)
        }
        "compare" => {
            let working = required_ref(command_args, "working_ref")?;
            let failing = required_ref(command_args, "failing_ref")?;
            if working == failing {
                return Err(PrivateCaptureCliError::InvalidCommand);
            }
            let mut backend = open_backend(configs)?;
            let passphrase = prompt_for_passphrase()?;
            render_comparison(backend.compare(working, failing, &passphrase)?)
        }
        "sanitize-preview" => {
            let capture_ref = required_ref(command_args, "capture_ref")?;
            let mut backend = open_backend(configs)?;
            let passphrase = prompt_for_passphrase()?;
            let derivative = backend.preview_derivative(capture_ref, &passphrase)?;
            render_derivative_preview(&derivative)?
        }
        "inspect" => return inspect_capture(command_args, configs),
        "live" => live_capture(command_args, configs)?,
        "sanitize" => {
            let capture_ref = required_ref(command_args, "capture_ref")?;
            let output = command_args
                .get_one::<PathBuf>("output")
                .cloned()
                .ok_or(PrivateCaptureCliError::InvalidCommand)?;
            let mut backend = open_backend(configs)?;
            let passphrase = prompt_for_passphrase()?;
            backend.create_derivative(
                capture_ref,
                &passphrase,
                Some(PrivateDerivativeDestination::new(output)),
            )?;
            render_derivative(&backend.review_derivative()?)
        }
        "open" => {
            if !command_args.get_flag("acknowledge_sensitive") {
                return Err(PrivateCaptureCliError::InvalidCommand);
            }
            let capture_ref = required_ref(command_args, "capture_ref")?;
            let mut backend = open_backend(configs)?;
            backend.open_private_folder(capture_ref)?;
            "private capture folder opened\nsensitivity=private evidence\n".to_owned()
        }
        "delete" => {
            if !command_args.get_flag("acknowledge_delete") {
                return Err(PrivateCaptureCliError::InvalidCommand);
            }
            let capture_ref = required_ref(command_args, "capture_ref")?;
            let mut backend = open_backend(configs)?;
            render_delete(capture_ref, backend.delete(capture_ref)?)
        }
        "purge-expired" => {
            let mut backend = open_backend(configs)?;
            render_maintenance(backend.purge_expired()?)
        }
        _ => return Err(PrivateCaptureCliError::InvalidCommand),
    };

    print!("{output}");
    Ok(())
}

fn inspect_capture(
    args: &ArgMatches,
    configs: &config::Configs,
) -> Result<(), PrivateCaptureCliError> {
    if !args.get_flag("acknowledge_sensitive") {
        return Err(PrivateCaptureCliError::InvalidCommand);
    }
    private_output_terminal_policy(std::io::stdout().is_terminal())?;
    let capture_ref = required_ref(args, "capture_ref")?;
    let selection = args
        .get_one::<u16>("record")
        .copied()
        .map_or(PrivateViewSelection::Catalog, PrivateViewSelection::Record);
    let mut backend = open_backend(configs)?;
    let passphrase = prompt_for_passphrase()?;
    backend.inspect_masked_private_evidence_to_terminal(capture_ref, &passphrase, selection)?;
    Ok(())
}

fn live_capture(
    args: &ArgMatches,
    configs: &config::Configs,
) -> Result<String, PrivateCaptureCliError> {
    if !args.get_flag("acknowledge_sensitive") {
        return Err(PrivateCaptureCliError::InvalidCommand);
    }
    private_output_terminal_policy(std::io::stdout().is_terminal())?;
    let video_id = args
        .get_one::<String>("video_id")
        .ok_or(PrivateCaptureCliError::InvalidCommand)?;
    let passphrase = prompt_for_passphrase()?;
    let parent = configs
        .config_folder
        .parent()
        .ok_or(PrivateCaptureCliError::VaultUnavailable)?;
    let root = parent.join(".unified-player-private-captures");
    let mut ordinary_roots = vec![configs.config_folder.clone(), configs.cache_folder.clone()];
    if let Some(log_folder) = &configs.app_config.log_folder {
        ordinary_roots.push(log_folder.clone());
    }
    ensure_private_root_separate(&root, &ordinary_roots)
        .map_err(|_| PrivateCaptureCliError::VaultUnavailable)?;
    let limits = CaptureLimits::default();
    let (capture, worker, _) = crate::developer_capture::prepare_runtime(&root, limits)
        .map_err(|_| PrivateCaptureCliError::VaultUnavailable)?;
    let shutdown = CancellationToken::new();
    let worker_shutdown = shutdown.clone();
    let worker_thread = std::thread::spawn(move || worker.run(&worker_shutdown));

    let result = (|| {
        capture.request_arm().map_err(map_capture_runtime_error)?;
        let capture_ref = capture
            .accept_consent(passphrase)
            .map_err(map_capture_runtime_error)?;
        let session = capture
            .claim(
                CapturePurpose::InteractivePlayback,
                SafeOperationRef::from_bytes(rand::random()),
            )
            .map_err(map_capture_runtime_error)?
            .ok_or(PrivateCaptureCliError::OperatorUnavailable)?;
        let runtime = tokio::runtime::Runtime::new()
            .map_err(|_| PrivateCaptureCliError::OperatorUnavailable)?;
        let playback_result = runtime.block_on(client::capture_youtube_playback_for_video(
            configs,
            video_id,
            session.clone(),
        ));
        let terminal = if playback_result.is_ok() {
            SafeTerminalCategory::Success
        } else {
            SafeTerminalCategory::Failed
        };
        session.finish(terminal);
        wait_for_live_capture_terminal(&capture)?;
        let outcome = playback_result
            .as_ref()
            .map(|_| "success")
            .unwrap_or("failed");
        let category = playback_result
            .as_ref()
            .err()
            .map(|error| error.kind.diagnostic_category().as_str());
        Ok(format!(
            "private live capture\nsensitivity=private\nreference={capture_ref}\nvideo_id={}\noutcome={outcome}\ncategory={}\nnext=run `unified-player youtube debug-capture inspect {capture_ref} --acknowledge-sensitive` in an interactive terminal\n",
            private_single_line(video_id),
            category.unwrap_or("none")
        ))
    })();

    shutdown.cancel();
    let _ = worker_thread.join();
    result
}

fn map_capture_runtime_error(_: CaptureRuntimeError) -> PrivateCaptureCliError {
    PrivateCaptureCliError::OperatorUnavailable
}

fn wait_for_live_capture_terminal(
    handle: &crate::developer_capture::CaptureHandle,
) -> Result<(), PrivateCaptureCliError> {
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    loop {
        match handle.snapshot().state {
            SafeCaptureState::Ready | SafeCaptureState::Incomplete | SafeCaptureState::Failed => {
                return Ok(())
            }
            _ if std::time::Instant::now() >= deadline => {
                return Err(PrivateCaptureCliError::OperatorUnavailable)
            }
            _ => std::thread::sleep(std::time::Duration::from_millis(10)),
        }
    }
}

fn open_backend(configs: &config::Configs) -> Result<CliBackend, PrivateCaptureCliError> {
    let parent = configs
        .config_folder
        .parent()
        .ok_or(PrivateCaptureCliError::VaultUnavailable)?;
    let root = parent.join(".unified-player-private-captures");
    let mut ordinary_roots = vec![configs.config_folder.clone(), configs.cache_folder.clone()];
    if let Some(log_folder) = &configs.app_config.log_folder {
        ordinary_roots.push(log_folder.clone());
    }
    ensure_private_root_separate(&root, &ordinary_roots)
        .map_err(|_| PrivateCaptureCliError::VaultUnavailable)?;
    let fresh = YouTubeFreshReplayAdapter::from_configs(configs)
        .map_err(|_| PrivateCaptureCliError::OperatorUnavailable)?;
    VaultOperatorBackend::open(
        root,
        CaptureLimits::default(),
        None,
        ordinary_roots,
        YouTubeOfflineReplayAdapter,
        fresh,
        TokioReplayTimer,
        crate::developer_capture::FreshReplayPolicy::default(),
        SystemPrivateFolderOpener,
    )
    .map(|(backend, _)| backend)
    .map_err(Into::into)
}

fn required_ref(args: &ArgMatches, name: &str) -> Result<SafeCaptureRef, PrivateCaptureCliError> {
    args.get_one::<String>(name)
        .and_then(|value| SafeCaptureRef::from_hex(value).ok())
        .ok_or(PrivateCaptureCliError::InvalidCommand)
}

fn private_single_line(value: &str) -> String {
    value
        .chars()
        .map(|character| {
            if character.is_control() {
                ' '
            } else {
                character
            }
        })
        .take(512)
        .collect()
}

fn render_status(retained: usize) -> String {
    format!("private capture status\nfeature=enabled\nvault=available\nretained={retained}\n")
}

fn render_list(references: &[SafeCaptureRef]) -> String {
    let mut output = format!("private capture list\nretained={}\n", references.len());
    for capture_ref in references {
        output.push_str("reference=");
        output.push_str(&capture_ref.to_string());
        output.push('\n');
    }
    output
}

fn render_review(review: SafeArtifactReview) -> String {
    format!(
        "private capture review\nreference={}\nschema={}\nrecords={}\nbytes={}\n\
         completeness={}\nterminal={}\nchecksum={}\n",
        review.capture_ref,
        review.schema_version,
        review.record_count,
        byte_bucket(review.byte_bucket),
        completeness(review.completeness),
        terminal(review.terminal_category),
        valid_invalid(review.checksum_valid),
    )
}

fn render_replay(replay: SafeReplayView) -> String {
    format!(
        "private capture replay\nreference={}\nmode={}\noutcome={}\nterminal={}\nrecords={}\n",
        replay.capture_ref,
        match replay.mode {
            crate::developer_capture::ReplayMode::Offline => "offline",
            crate::developer_capture::ReplayMode::Fresh => "fresh",
        },
        replay.outcome.as_str(),
        terminal(replay.terminal_category),
        replay.record_count,
    )
}

fn render_comparison(comparison: SafeComparisonView) -> String {
    let categories = comparison
        .category_labels()
        .into_iter()
        .flatten()
        .collect::<Vec<_>>()
        .join(",");
    format!(
        "private capture comparison\nreference={}\nfindings={}\ncategories={categories}\n\
         completeness={}\ndropped_findings={}\n",
        comparison.capture_ref,
        comparison.finding_count,
        if comparison.incomplete {
            "incomplete"
        } else {
            "complete"
        },
        comparison.dropped_findings,
    )
}

fn render_derivative(derivative: &SafeDerivativeView) -> String {
    format!(
        "private diagnostic derivative\nstate={}\nschema={}\ncompleteness={}\nbytes={}\n\
         checksum={}\nforbidden_data_scan={}\ndurability={}\n",
        derivative.state_str(),
        derivative.schema_version,
        derivative.completeness_str(),
        derivative.byte_bucket_str(),
        valid_invalid(derivative.checksum_valid),
        passed_failed(derivative.forbidden_scan_passed),
        match derivative.durability_confirmed {
            Some(true) => "confirmed",
            Some(false) => "uncertain",
            None => "not reported",
        },
    )
}

fn render_derivative_preview(
    derivative: &SafeDerivativeView,
) -> Result<String, PrivateCaptureCliError> {
    derivative
        .preview()
        .map(crate::developer_capture::SafeDerivativePreview::render_text)
        .ok_or(PrivateCaptureCliError::Action(
            SafeOperatorFailure::DerivativeUnavailable,
        ))
}

fn render_delete(capture_ref: SafeCaptureRef, deleted: bool) -> String {
    format!(
        "private capture deletion\nreference={capture_ref}\nresult={}\n",
        if deleted { "deleted" } else { "not found" }
    )
}

fn render_maintenance(report: crate::developer_capture::MaintenanceReport) -> String {
    format!(
        "private capture maintenance\nexpired_removed={}\nquota_removed={}\n\
         partials_removed={}\nfailed_removals={}\nretained={}\nquota={}\n",
        report.expired_removed,
        report.quota_removed,
        report.partials_removed,
        report.failed_removals,
        report.retained_artifacts,
        if report.quota_satisfied {
            "satisfied"
        } else {
            "not satisfied"
        },
    )
}

const fn completeness(value: CaptureCompleteness) -> &'static str {
    match value {
        CaptureCompleteness::Pending => "pending",
        CaptureCompleteness::Complete => "complete",
        CaptureCompleteness::Incomplete => "incomplete",
    }
}

const fn terminal(value: SafeTerminalCategory) -> &'static str {
    match value {
        SafeTerminalCategory::Success => "success",
        SafeTerminalCategory::Failed => "failed",
        SafeTerminalCategory::Cancelled => "cancelled",
        SafeTerminalCategory::Superseded => "superseded",
        SafeTerminalCategory::TimedOut => "timed out",
        SafeTerminalCategory::Panicked => "panicked",
    }
}

const fn byte_bucket(value: CaptureByteBucket) -> &'static str {
    match value {
        CaptureByteBucket::Empty => "empty",
        CaptureByteBucket::Under64KiB => "under 64 KiB",
        CaptureByteBucket::Under1MiB => "under 1 MiB",
        CaptureByteBucket::Under4MiB => "under 4 MiB",
        CaptureByteBucket::Under16MiB => "under 16 MiB",
        CaptureByteBucket::AtOrOver16MiB => "at least 16 MiB",
    }
}

const fn valid_invalid(value: bool) -> &'static str {
    if value {
        "valid"
    } else {
        "invalid"
    }
}

const fn passed_failed(value: bool) -> &'static str {
    if value {
        "passed"
    } else {
        "failed"
    }
}

fn passphrase_terminal_policy(
    stdin_is_terminal: bool,
    prompt_is_terminal: bool,
) -> Result<(), PrivateCaptureCliError> {
    if stdin_is_terminal && prompt_is_terminal {
        Ok(())
    } else {
        Err(PrivateCaptureCliError::PassphraseTerminalRequired)
    }
}

fn private_output_terminal_policy(stdout_is_terminal: bool) -> Result<(), PrivateCaptureCliError> {
    if stdout_is_terminal {
        Ok(())
    } else {
        Err(PrivateCaptureCliError::PrivateOutputTerminalRequired)
    }
}

struct RawModeGuard;

impl RawModeGuard {
    fn enable() -> Result<Self, PrivateCaptureCliError> {
        terminal::enable_raw_mode()
            .map_err(|_| PrivateCaptureCliError::PassphraseInputUnavailable)?;
        Ok(Self)
    }
}

impl Drop for RawModeGuard {
    fn drop(&mut self) {
        let _ = terminal::disable_raw_mode();
    }
}

enum PassphraseKeyOutcome {
    Continue,
    Complete,
    Cancel,
}

fn apply_passphrase_key(
    input: &mut CapturePassphraseInput,
    key: KeyEvent,
    prompt: &mut impl Write,
) -> Result<PassphraseKeyOutcome, PrivateCaptureCliError> {
    if !matches!(key.kind, KeyEventKind::Press | KeyEventKind::Repeat) {
        return Ok(PassphraseKeyOutcome::Continue);
    }
    match key.code {
        KeyCode::Enter => Ok(PassphraseKeyOutcome::Complete),
        KeyCode::Esc => Ok(PassphraseKeyOutcome::Cancel),
        KeyCode::Char('c') if key.modifiers.contains(KeyModifiers::CONTROL) => {
            Ok(PassphraseKeyOutcome::Cancel)
        }
        KeyCode::Backspace => {
            if input.pop() {
                prompt
                    .write_all(b"\x08 \x08")
                    .map_err(|_| PrivateCaptureCliError::PassphraseInputUnavailable)?;
                prompt
                    .flush()
                    .map_err(|_| PrivateCaptureCliError::PassphraseInputUnavailable)?;
            }
            Ok(PassphraseKeyOutcome::Continue)
        }
        KeyCode::Char(character)
            if !key
                .modifiers
                .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT) =>
        {
            if input.push(character) {
                prompt
                    .write_all(b"*")
                    .map_err(|_| PrivateCaptureCliError::PassphraseInputUnavailable)?;
                prompt
                    .flush()
                    .map_err(|_| PrivateCaptureCliError::PassphraseInputUnavailable)?;
            }
            Ok(PassphraseKeyOutcome::Continue)
        }
        _ => Ok(PassphraseKeyOutcome::Continue),
    }
}

fn prompt_for_passphrase() -> Result<CapturePassphrase, PrivateCaptureCliError> {
    passphrase_terminal_policy(
        std::io::stdin().is_terminal(),
        std::io::stderr().is_terminal(),
    )?;
    let mut prompt = std::io::stderr().lock();
    prompt
        .write_all(b"Private capture passphrase: ")
        .map_err(|_| PrivateCaptureCliError::PassphraseInputUnavailable)?;
    prompt
        .flush()
        .map_err(|_| PrivateCaptureCliError::PassphraseInputUnavailable)?;
    let raw_mode = RawModeGuard::enable()?;
    let mut input = CapturePassphraseInput::new();
    loop {
        let event =
            event::read().map_err(|_| PrivateCaptureCliError::PassphraseInputUnavailable)?;
        let Event::Key(key) = event else {
            continue;
        };
        match apply_passphrase_key(&mut input, key, &mut prompt)? {
            PassphraseKeyOutcome::Continue => {}
            PassphraseKeyOutcome::Complete => break,
            PassphraseKeyOutcome::Cancel => {
                drop(raw_mode);
                let _ = prompt.write_all(b"\r\n");
                return Err(PrivateCaptureCliError::PassphraseCancelled);
            }
        }
    }
    drop(raw_mode);
    prompt
        .write_all(b"\r\n")
        .map_err(|_| PrivateCaptureCliError::PassphraseInputUnavailable)?;
    input
        .into_passphrase()
        .map_err(|_| PrivateCaptureCliError::PassphraseInvalid)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crossterm::event::KeyEvent;

    #[test]
    fn passphrase_requires_terminal_input_and_prompt_streams() {
        assert!(passphrase_terminal_policy(true, true).is_ok());
        for (stdin, prompt) in [(false, true), (true, false), (false, false)] {
            assert_eq!(
                passphrase_terminal_policy(stdin, prompt),
                Err(PrivateCaptureCliError::PassphraseTerminalRequired)
            );
        }
    }

    #[test]
    fn private_evidence_output_rejects_redirection() {
        assert!(private_output_terminal_policy(true).is_ok());
        assert_eq!(
            private_output_terminal_policy(false),
            Err(PrivateCaptureCliError::PrivateOutputTerminalRequired)
        );
    }

    #[test]
    fn capture_references_are_strict_lowercase_fixed_hex() {
        for accepted in ["0123abcd", "00000000", "ffffffff"] {
            assert!(SafeCaptureRef::from_hex(accepted).is_ok());
        }
        for rejected in [
            "0123ABCd",
            "0123abc",
            "0123abcde",
            "0123abcg",
            "../0123abcd",
        ] {
            assert!(SafeCaptureRef::from_hex(rejected).is_err(), "{rejected:?}");
        }
    }

    #[test]
    fn masked_input_writes_only_masks_and_supports_backspace() {
        let mut input = CapturePassphraseInput::new();
        let mut output = Vec::new();
        for key in [
            KeyEvent::new(KeyCode::Char('s'), KeyModifiers::NONE),
            KeyEvent::new(KeyCode::Char('e'), KeyModifiers::NONE),
            KeyEvent::new(KeyCode::Backspace, KeyModifiers::NONE),
            KeyEvent::new(KeyCode::Char('x'), KeyModifiers::NONE),
        ] {
            assert!(matches!(
                apply_passphrase_key(&mut input, key, &mut output).unwrap(),
                PassphraseKeyOutcome::Continue
            ));
        }
        assert_eq!(input.character_count(), 2);
        assert_eq!(output, b"**\x08 \x08*");
        assert!(!String::from_utf8(output).unwrap().contains("sex"));
    }

    #[test]
    fn control_c_cancels_without_accepting_input() {
        let mut input = CapturePassphraseInput::new();
        let mut output = Vec::new();
        let result = apply_passphrase_key(
            &mut input,
            KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL),
            &mut output,
        )
        .unwrap();
        assert!(matches!(result, PassphraseKeyOutcome::Cancel));
        assert_eq!(input.character_count(), 0);
        assert!(output.is_empty());
    }

    #[test]
    fn safe_renderers_never_contain_private_canaries() {
        let canaries = [
            "Cookie: private-cookie-canary",
            "https://private.invalid/watch?v=secret",
            "private-title-canary",
            "private-artist-canary",
            "private-lyric-canary",
            "C:\\private\\capture",
        ];
        let output = [
            render_status(2),
            render_list(&[
                SafeCaptureRef::from_hex("00112233").unwrap(),
                SafeCaptureRef::from_hex("44556677").unwrap(),
            ]),
            render_delete(SafeCaptureRef::from_hex("00112233").unwrap(), true),
        ]
        .join("");
        for canary in canaries {
            assert!(!output.contains(canary));
        }
        assert!(output.contains("00112233"));
    }

    #[test]
    fn derivative_preview_renderer_prints_only_the_three_scanned_files() {
        let derivative = SafeDerivativeView::new_for_test(
            crate::developer_capture::SafeDerivativeState::Previewed,
            [
                "{\"provider\":\"youtube_music\",\"safe\":true}\n",
                "abc123  evidence.json\n",
                "{\"privacy_boundary\":\"typed-allowlist-only\"}\n",
            ],
            "unified-player provider diagnostic derivative review\nforbidden_scan=passed\n",
        );
        let output = render_derivative_preview(&derivative).unwrap();
        assert_eq!(
            output,
            "===== evidence.json =====\n\
             {\"provider\":\"youtube_music\",\"safe\":true}\n\
             \n\
             ===== checksums.sha256 =====\n\
             abc123  evidence.json\n\
             \n\
             ===== manifest.json =====\n\
             {\"privacy_boundary\":\"typed-allowlist-only\"}\n"
        );
        for private in [
            "Cookie: private-cookie-canary",
            "https://private.invalid/watch?v=secret",
            "C:\\private\\capture",
        ] {
            assert!(!output.contains(private));
        }
        assert!(!output.contains("provider diagnostic derivative review"));
    }

    #[test]
    fn every_error_message_is_finite_and_path_free() {
        let mut errors = vec![
            PrivateCaptureCliError::VaultUnavailable,
            PrivateCaptureCliError::OperatorUnavailable,
            PrivateCaptureCliError::PassphraseTerminalRequired,
            PrivateCaptureCliError::PassphraseInputUnavailable,
            PrivateCaptureCliError::PassphraseInvalid,
            PrivateCaptureCliError::PassphraseCancelled,
            PrivateCaptureCliError::PrivateOutputTerminalRequired,
            PrivateCaptureCliError::InvalidCommand,
        ];
        errors.extend(
            [
                SafeOperatorFailure::InvalidState,
                SafeOperatorFailure::NothingSelected,
                SafeOperatorFailure::LabelsIncomplete,
                SafeOperatorFailure::ArtifactNotFound,
                SafeOperatorFailure::AmbiguousReference,
                SafeOperatorFailure::WrongPassphrase,
                SafeOperatorFailure::InvalidArtifact,
                SafeOperatorFailure::QueueFull,
                SafeOperatorFailure::VaultUnavailable,
                SafeOperatorFailure::ReplayUnavailable,
                SafeOperatorFailure::ComparisonUnavailable,
                SafeOperatorFailure::DerivativeUnavailable,
                SafeOperatorFailure::InspectionUnavailable,
                SafeOperatorFailure::DestinationUnavailable,
                SafeOperatorFailure::FolderOpeningUnsupported,
                SafeOperatorFailure::ConsentRequired,
                SafeOperatorFailure::NetworkConsentRequired,
                SafeOperatorFailure::Cancelled,
                SafeOperatorFailure::WorkerStopped,
            ]
            .map(PrivateCaptureCliError::Action),
        );
        for error in errors {
            let rendered = error.to_string();
            assert!(rendered.len() <= 96);
            assert!(!rendered.contains('\\'));
            assert!(!rendered.contains("://"));
            assert!(!rendered.contains("private-canary"));
        }
    }
}
