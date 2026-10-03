use std::{
    collections::BTreeMap,
    fmt::Write as _,
    fs::File,
    io::{BufRead as _, BufReader, Read as _},
    path::Path,
};

use anyhow::{Context as _, Result};
use serde::Serialize;
use sha2::{Digest as _, Sha256};

use super::{Component, DiagnosticEvent, OperationOutcome, Severity, DIAGNOSTIC_SCHEMA_VERSION};

const MANIFEST_VERSION: u16 = 1;
const MAX_SOURCE_BYTES: u64 = 8 * 1024 * 1024;
const MAX_EVENTS: usize = 500;
const DIAGNOSTIC_PREFIX: &str = "unified-player-diagnostics-";

#[derive(Serialize)]
struct BundleManifest {
    manifest_version: u16,
    schema_version: u16,
    created_at: String,
    build_revision: &'static str,
    build_dirty: &'static str,
    backtraces: &'static str,
    remote_export: &'static str,
    review_required: bool,
    files: Vec<ManifestFile>,
}

#[derive(Serialize)]
struct ManifestFile {
    name: &'static str,
    sha256: String,
}

#[derive(Serialize)]
struct BundleFacts {
    application_version: &'static str,
    platform_os: &'static str,
    platform_arch: &'static str,
    event_count: usize,
    privacy_boundary: &'static str,
}

#[derive(Serialize)]
struct BundleEvent {
    timestamp: String,
    event_code: String,
    severity: Severity,
    component: Component,
    outcome: Option<OperationOutcome>,
    duration_ms: Option<u64>,
    reference: Option<String>,
    cause: Option<String>,
    fingerprint: Option<String>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct BundleReview {
    pub(crate) manifest_version: u16,
    pub(crate) schema_version: u16,
    pub(crate) build_revision: String,
    pub(crate) build_dirty: String,
    pub(crate) event_count: usize,
    pub(crate) files: Vec<&'static str>,
    pub(crate) forbidden_findings: usize,
}

pub(crate) fn create(source_directory: &Path, output_directory: &Path) -> Result<BundleReview> {
    create_focused(source_directory, output_directory, None)
}

pub(crate) fn create_focused(
    source_directory: &Path,
    output_directory: &Path,
    focus_reference: Option<&str>,
) -> Result<BundleReview> {
    if output_directory.exists() {
        anyhow::ensure!(
            output_directory
                .read_dir()
                .context("inspect support bundle output")?
                .next()
                .is_none(),
            "support bundle output directory must be empty"
        );
    } else {
        std::fs::create_dir_all(output_directory).context("create support bundle output")?;
    }

    let mut events = read_safe_events(source_directory)?;
    if let Some(focus) = focus_reference.and_then(normalize_focus_reference) {
        events.retain(|event| event.reference.as_deref() == Some(focus));
    }
    let events_jsonl = events
        .iter()
        .map(serde_json::to_string)
        .collect::<std::result::Result<Vec<_>, _>>()?
        .join("\n");
    let facts = serde_json::to_string_pretty(&BundleFacts {
        application_version: env!("CARGO_PKG_VERSION"),
        platform_os: std::env::consts::OS,
        platform_arch: std::env::consts::ARCH,
        event_count: events.len(),
        privacy_boundary: "allowlisted-operational-facts-only",
    })?;
    let files = [
        ManifestFile {
            name: "diagnostics.json",
            sha256: checksum(facts.as_bytes()),
        },
        ManifestFile {
            name: "events.jsonl",
            sha256: checksum(events_jsonl.as_bytes()),
        },
    ];
    let manifest = serde_json::to_string_pretty(&BundleManifest {
        manifest_version: MANIFEST_VERSION,
        schema_version: DIAGNOSTIC_SCHEMA_VERSION,
        created_at: chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, true),
        build_revision: option_env!("UNIFIED_PLAYER_GIT_REVISION").unwrap_or("unknown"),
        build_dirty: option_env!("UNIFIED_PLAYER_GIT_DIRTY").unwrap_or("unknown"),
        backtraces: "excluded",
        remote_export: "disabled",
        review_required: true,
        files: files.into_iter().collect(),
    })?;
    let checksums = files_for_checksums(&facts, &events_jsonl);
    let generated = [
        ("manifest.json", manifest),
        ("diagnostics.json", facts),
        ("events.jsonl", events_jsonl),
        ("checksums.sha256", checksums),
    ];
    let forbidden_findings: usize = generated
        .iter()
        .map(|(_, content)| forbidden_data_findings(content))
        .sum();
    anyhow::ensure!(
        forbidden_findings == 0,
        "support bundle forbidden-data scan rejected generated output"
    );
    for (name, content) in &generated {
        std::fs::write(output_directory.join(name), content)
            .with_context(|| format!("write support bundle file {name}"))?;
    }

    let review = review(output_directory)?;
    if let Some(handle) = super::handle() {
        let mut event = DiagnosticEvent::new(
            handle.run_id(),
            handle.started_at(),
            super::EventName::SUPPORT_BUNDLE_CREATED,
            super::EventCode::SUPPORT_BUNDLE_CREATED,
            Severity::Info,
            Component::Support,
            "Reviewable support bundle created",
        );
        event.fields.manifest_version = Some(MANIFEST_VERSION);
        event.fields.file_count = review.files.len().try_into().ok();
        handle.record(event);
        handle.set_component_health(
            Component::Support,
            super::HealthStatus::Healthy,
            "bundle-reviewed",
        );
    }
    Ok(review)
}

pub(crate) fn preview_manifest(focused: bool) -> Vec<String> {
    vec![
        format!("manifest_version={MANIFEST_VERSION}"),
        format!("schema_version={DIAGNOSTIC_SCHEMA_VERSION}"),
        "created_at=<UTC generation time>".to_owned(),
        "build_revision=<embedded safe revision>".to_owned(),
        "build_dirty=<true|false|unknown>".to_owned(),
        "backtraces=excluded".to_owned(),
        "remote_export=disabled".to_owned(),
        "review_required=true".to_owned(),
        "files[0].name=diagnostics.json files[0].sha256=<generated>".to_owned(),
        "files[1].name=events.jsonl files[1].sha256=<generated>".to_owned(),
        "bundle_files=manifest.json,diagnostics.json,events.jsonl,checksums.sha256".to_owned(),
        "evidence=allowlisted significant operational events only".to_owned(),
        format!(
            "scope={}",
            if focused {
                "focused incident operation"
            } else {
                "general retained local evidence"
            }
        ),
    ]
}

pub(crate) fn open_folder_with(
    directory: &Path,
    opener: impl FnOnce(&Path) -> std::io::Result<()>,
) -> Result<()> {
    opener(directory).context("open support bundle folder")
}

fn normalize_focus_reference(reference: &str) -> Option<&str> {
    let reference = reference.strip_prefix("I-").unwrap_or(reference);
    (reference.len() == 8 && reference.bytes().all(|byte| byte.is_ascii_hexdigit()))
        .then_some(reference)
}

pub(crate) fn review(directory: &Path) -> Result<BundleReview> {
    let names = [
        "manifest.json",
        "diagnostics.json",
        "events.jsonl",
        "checksums.sha256",
    ];
    let contents = names
        .iter()
        .map(|name| {
            let bytes = std::fs::read(directory.join(name))
                .with_context(|| format!("read support bundle file {name}"))?;
            anyhow::ensure!(
                bytes.len() <= MAX_SOURCE_BYTES as usize,
                "support bundle file exceeds review limit"
            );
            String::from_utf8(bytes).context("support bundle file is not UTF-8")
        })
        .collect::<Result<Vec<_>>>()?;
    let manifest: serde_json::Value =
        serde_json::from_str(&contents[0]).context("parse support bundle manifest")?;
    let manifest_version = manifest
        .get("manifest_version")
        .and_then(serde_json::Value::as_u64)
        .and_then(|value| value.try_into().ok())
        .context("support bundle manifest version is invalid")?;
    let schema_version = manifest
        .get("schema_version")
        .and_then(serde_json::Value::as_u64)
        .and_then(|value| value.try_into().ok())
        .context("support bundle schema version is invalid")?;
    let build_revision = manifest
        .get("build_revision")
        .and_then(serde_json::Value::as_str)
        .filter(|value| safe_revision(value))
        .context("support bundle build revision is invalid")?
        .to_owned();
    let build_dirty = manifest
        .get("build_dirty")
        .and_then(serde_json::Value::as_str)
        .filter(|value| matches!(*value, "true" | "false" | "unknown"))
        .context("support bundle dirty marker is invalid")?
        .to_owned();
    anyhow::ensure!(
        manifest_version == MANIFEST_VERSION
            && schema_version == DIAGNOSTIC_SCHEMA_VERSION
            && manifest
                .get("backtraces")
                .and_then(serde_json::Value::as_str)
                == Some("excluded")
            && manifest
                .get("remote_export")
                .and_then(serde_json::Value::as_str)
                == Some("disabled")
            && manifest
                .get("review_required")
                .and_then(serde_json::Value::as_bool)
                == Some(true),
        "support bundle manifest policy mismatch"
    );
    verify_manifest_files(&manifest, &contents[1], &contents[2])?;
    anyhow::ensure!(
        contents[3] == files_for_checksums(&contents[1], &contents[2]),
        "support bundle checksum verification failed"
    );
    let forbidden_findings: usize = contents
        .iter()
        .map(|content| forbidden_data_findings(content))
        .sum();
    anyhow::ensure!(
        forbidden_findings == 0,
        "support bundle forbidden-data review failed"
    );
    Ok(BundleReview {
        manifest_version,
        schema_version,
        build_revision,
        build_dirty,
        event_count: contents[2].lines().filter(|line| !line.is_empty()).count(),
        files: names.to_vec(),
        forbidden_findings,
    })
}

fn verify_manifest_files(manifest: &serde_json::Value, facts: &str, events: &str) -> Result<()> {
    let files = manifest
        .get("files")
        .and_then(serde_json::Value::as_array)
        .context("support bundle manifest files are invalid")?;
    let expected = [
        ("diagnostics.json", checksum(facts.as_bytes())),
        ("events.jsonl", checksum(events.as_bytes())),
    ];
    anyhow::ensure!(
        files.len() == expected.len()
            && expected.iter().all(|(name, sha256)| {
                files.iter().any(|file| {
                    file.get("name").and_then(serde_json::Value::as_str) == Some(*name)
                        && file.get("sha256").and_then(serde_json::Value::as_str)
                            == Some(sha256.as_str())
                })
            }),
        "support bundle manifest checksum verification failed"
    );
    Ok(())
}

fn safe_revision(value: &str) -> bool {
    value == "unknown"
        || ((7..=40).contains(&value.len()) && value.bytes().all(|byte| byte.is_ascii_hexdigit()))
}

pub(crate) fn render_local_trend(directory: &Path) -> Result<String> {
    let mut events = read_safe_events(directory)?;
    events.sort_by(|left, right| left.timestamp.cmp(&right.timestamp));
    let mut groups = BTreeMap::<String, Vec<u64>>::new();
    for event in events {
        if let Some(duration) = event.duration_ms {
            groups.entry(event.event_code).or_default().push(duration);
        }
    }
    let mut output = String::from("diagnostic local trend\ntrend.scope=retained-local-only\n");
    if groups.is_empty() {
        output.push_str("trend.samples=none\n");
        return Ok(output);
    }
    for (code, durations) in groups {
        let split = durations.len().div_ceil(2);
        let previous = &durations[..durations.len().saturating_sub(split)];
        let current = &durations[durations.len().saturating_sub(split)..];
        let previous_p95 = percentile_95(previous);
        let current_p95 = percentile_95(current).unwrap_or(0);
        let delta = previous_p95.map_or(0_i64, |previous| {
            i64::try_from(current_p95).unwrap_or(i64::MAX)
                - i64::try_from(previous).unwrap_or(i64::MAX)
        });
        writeln!(
            output,
            "trend.{code}.current_count={} current_p95_ms={current_p95} previous_count={} previous_p95_ms={} delta_ms={delta}",
            current.len(),
            previous.len(),
            previous_p95.map_or_else(|| "none".to_owned(), |value| value.to_string())
        )?;
    }
    Ok(output)
}

fn percentile_95(values: &[u64]) -> Option<u64> {
    if values.is_empty() {
        return None;
    }
    let mut sorted = values.to_vec();
    sorted.sort_unstable();
    let index = (sorted.len() * 95).div_ceil(100).saturating_sub(1);
    sorted.get(index).copied()
}

fn read_safe_events(directory: &Path) -> Result<Vec<BundleEvent>> {
    let Ok(entries) = directory.read_dir() else {
        return Ok(Vec::new());
    };
    let mut files = entries
        .filter_map(std::result::Result::ok)
        .filter(|entry| {
            entry.file_name().to_str().is_some_and(|name| {
                name.starts_with(DIAGNOSTIC_PREFIX)
                    && Path::new(name)
                        .extension()
                        .is_some_and(|extension| extension.eq_ignore_ascii_case("jsonl"))
            })
        })
        .collect::<Vec<_>>();
    files.sort_by_key(|entry| {
        std::cmp::Reverse(
            entry
                .metadata()
                .and_then(|metadata| metadata.modified())
                .ok(),
        )
    });
    let mut remaining = MAX_SOURCE_BYTES;
    let mut events = Vec::new();
    for entry in files {
        if remaining == 0 || events.len() >= MAX_EVENTS {
            break;
        }
        let file = File::open(entry.path()).context("open diagnostic evidence")?;
        let metadata_len = file.metadata().map_or(0, |metadata| metadata.len());
        let allowed = remaining.min(metadata_len.max(1));
        let reader = BufReader::new(file.take(allowed));
        for line in reader.lines().map_while(std::result::Result::ok) {
            if events.len() >= MAX_EVENTS {
                break;
            }
            let Ok(event) = serde_json::from_str::<DiagnosticEvent>(&line) else {
                continue;
            };
            if let Some(event) = allowlist_event(event) {
                events.push(event);
            }
        }
        remaining = remaining.saturating_sub(allowed);
    }
    events.reverse();
    Ok(events)
}

fn allowlist_event(event: DiagnosticEvent) -> Option<BundleEvent> {
    let significant = matches!(event.severity, Severity::Warn | Severity::Error)
        || event.event_name == "request.completed"
        || event.event_name == "operation.stage"
        || event.event_name == "worker.transition";
    if !significant || !safe_code(&event.event_code) {
        return None;
    }
    let timestamp = chrono::DateTime::parse_from_rfc3339(&event.timestamp)
        .ok()?
        .with_timezone(&chrono::Utc)
        .to_rfc3339_opts(chrono::SecondsFormat::Millis, true);
    let reference = event
        .operation
        .as_ref()
        .and_then(|operation| {
            let reference = operation.short_reference();
            (reference.len() == 8 && reference.bytes().all(|byte| byte.is_ascii_hexdigit()))
                .then(|| reference.to_ascii_lowercase())
        })
        .or_else(|| {
            event
                .fields
                .incident_reference
                .as_ref()
                .and_then(|reference| {
                    (reference.len() <= 16
                        && reference.starts_with("I-")
                        && reference
                            .bytes()
                            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-'))
                    .then(|| reference.clone())
                })
        });
    let fingerprint = event.fields.fingerprint.as_ref().and_then(|fingerprint| {
        (event.event_code == "PANIC_CAPTURED"
            && fingerprint.len() == 12
            && fingerprint.bytes().all(|byte| byte.is_ascii_hexdigit()))
        .then(|| fingerprint.to_ascii_lowercase())
    });
    Some(BundleEvent {
        timestamp,
        event_code: event.event_code,
        severity: event.severity,
        component: event.component,
        outcome: event.fields.outcome,
        duration_ms: event.fields.duration_ms,
        reference,
        cause: safe_cause(event.fields.error_type.as_deref()).map(str::to_owned),
        fingerprint,
    })
}

fn safe_code(code: &str) -> bool {
    !code.is_empty()
        && code.len() <= 64
        && code
            .bytes()
            .all(|byte| byte.is_ascii_uppercase() || byte.is_ascii_digit() || byte == b'_')
}

fn safe_cause(cause: Option<&str>) -> Option<&'static str> {
    match cause {
        Some("authentication") => Some("authentication"),
        Some("contract") => Some("contract"),
        Some("decode") => Some("decode"),
        Some("external_command") => Some("external_command"),
        Some("network_unavailable") => Some("network_unavailable"),
        Some("resource") => Some("resource"),
        Some("storage") => Some("storage"),
        Some("unavailable") => Some("unavailable"),
        Some("internal_panic") => Some("internal_panic"),
        Some("bounded_incident") => Some("bounded_incident"),
        _ => None,
    }
}

fn checksum(content: &[u8]) -> String {
    let digest = Sha256::digest(content);
    let mut output = String::with_capacity(digest.len() * 2);
    for byte in digest {
        let _ = write!(output, "{byte:02x}");
    }
    output
}

fn files_for_checksums(facts: &str, events: &str) -> String {
    format!(
        "{}  diagnostics.json\n{}  events.jsonl\n",
        checksum(facts.as_bytes()),
        checksum(events.as_bytes())
    )
}

fn forbidden_data_findings(content: &str) -> usize {
    let lower = content.to_ascii_lowercase();
    [
        "\"url\"",
        "\"path\"",
        "\"query\"",
        "\"title\"",
        "\"lyrics\"",
        "\"cookie\"",
        "\"header\"",
        "\"payload\"",
        "\"body\"",
        "authorization:",
        "spotify:track:",
        "youtube.com/",
    ]
    .iter()
    .filter(|needle| lower.contains(*needle))
    .count()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::observability::{EventCode, EventName, OperationContext, OperationSource};
    use std::time::Instant;

    #[test]
    fn bundle_is_allowlisted_reviewable_checksummed_and_forbidden_data_free() {
        let root = std::env::temp_dir().join(format!(
            "support-bundle-test-{}",
            super::super::random_hex::<5>()
        ));
        let source = root.join("source");
        let output = root.join("output");
        std::fs::create_dir_all(&source).unwrap();
        let mut event = DiagnosticEvent::new(
            "run",
            Instant::now(),
            EventName::REQUEST_COMPLETED,
            EventCode::REQUEST_COMPLETED,
            Severity::Error,
            Component::YoutubeMusic,
            "private title https://example.invalid",
        )
        .with_operation(OperationContext::new(
            "search_youtube",
            OperationSource::Terminal,
        ));
        event.fields.error_type = Some("network_unavailable".to_owned());
        event.fields.outcome = Some(OperationOutcome::Error);
        let mut value = serde_json::to_value(&event).unwrap();
        value
            .as_object_mut()
            .unwrap()
            .insert("query".to_owned(), serde_json::json!("private-query"));
        std::fs::write(
            source.join(format!("{DIAGNOSTIC_PREFIX}fixture.jsonl")),
            serde_json::to_string(&value).unwrap(),
        )
        .unwrap();

        let created = create(&source, &output).unwrap();
        assert_eq!(created.event_count, 1);
        assert_eq!(created.forbidden_findings, 0);
        assert_eq!(created.files.len(), 4);
        let combined = created
            .files
            .iter()
            .map(|name| std::fs::read_to_string(output.join(name)).unwrap())
            .collect::<Vec<_>>()
            .join("\n");
        assert!(!combined.contains("private title"));
        assert!(!combined.contains("private-query"));
        assert!(!combined.contains("example.invalid"));
        assert!(combined.contains("backtraces"));
        assert!(combined.contains("excluded"));
        assert!(combined.contains("REQUEST_COMPLETED"));
        let reviewed = review(&output).unwrap();
        assert_eq!(reviewed, created);
        std::fs::write(output.join("events.jsonl"), "tampered").unwrap();
        assert!(review(&output).is_err());
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn local_trend_compares_previous_and_current_timing_samples() {
        let root = std::env::temp_dir().join(format!(
            "diagnostic-trend-test-{}",
            super::super::random_hex::<5>()
        ));
        std::fs::create_dir_all(&root).unwrap();
        let mut lines = Vec::new();
        for (index, duration) in [10_u64, 20, 30, 40].into_iter().enumerate() {
            let mut event = DiagnosticEvent::new(
                "run",
                Instant::now(),
                EventName::OPERATION_STAGE,
                EventCode::OPERATION_STAGE,
                Severity::Debug,
                Component::Coordinator,
                "Operation stage",
            );
            event.timestamp = format!("2026-07-29T00:00:0{index}.000Z");
            event.fields.duration_ms = Some(duration);
            lines.push(serde_json::to_string(&event).unwrap());
        }
        std::fs::write(
            root.join(format!("{DIAGNOSTIC_PREFIX}trend.jsonl")),
            lines.join("\n"),
        )
        .unwrap();
        let report = render_local_trend(&root).unwrap();
        assert!(report.contains("current_count=2"));
        assert!(report.contains("current_p95_ms=40"));
        assert!(report.contains("previous_p95_ms=20"));
        assert!(report.contains("delta_ms=20"));
        assert!(!report.contains("private"));
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn local_trend_has_an_explicit_empty_state() {
        let root = std::env::temp_dir().join(format!(
            "diagnostic-empty-trend-test-{}",
            super::super::random_hex::<5>()
        ));
        std::fs::create_dir_all(&root).unwrap();
        let report = render_local_trend(&root).unwrap();
        assert!(report.contains("trend.samples=none"));
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn manifest_preview_distinguishes_general_and_focused_bundles() {
        let general = preview_manifest(false).join("\n");
        let focused = preview_manifest(true).join("\n");
        assert!(general.contains("general retained local evidence"));
        assert!(focused.contains("focused incident operation"));
        for expected in [
            "manifest_version=1",
            "schema_version=1",
            "manifest.json",
            "events.jsonl",
            "files[0].sha256=<generated>",
            "backtraces=excluded",
            "remote_export=disabled",
        ] {
            assert!(general.contains(expected));
        }
    }

    #[test]
    fn review_rejects_manifest_checksum_tampering() {
        let root = std::env::temp_dir().join(format!(
            "manifest-checksum-test-{}",
            super::super::random_hex::<5>()
        ));
        let source = root.join("source");
        let output = root.join("output");
        std::fs::create_dir_all(&source).unwrap();
        create(&source, &output).unwrap();
        let mut manifest: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(output.join("manifest.json")).unwrap())
                .unwrap();
        manifest["files"][0]["sha256"] = serde_json::json!("0".repeat(64));
        std::fs::write(
            output.join("manifest.json"),
            serde_json::to_string_pretty(&manifest).unwrap(),
        )
        .unwrap();
        let error = review(&output).unwrap_err();
        assert!(error
            .to_string()
            .contains("manifest checksum verification failed"));
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn unsupported_folder_opening_returns_a_bounded_error() {
        let error = open_folder_with(Path::new("not-disclosed"), |_| {
            Err(std::io::Error::new(
                std::io::ErrorKind::Unsupported,
                "private source detail",
            ))
        })
        .unwrap_err();
        assert!(error.to_string().contains("open support bundle folder"));
        assert!(!error.to_string().contains("not-disclosed"));
    }

    #[test]
    fn focused_bundle_contains_only_the_correlated_operation() {
        let root = std::env::temp_dir().join(format!(
            "focused-support-test-{}",
            super::super::random_hex::<5>()
        ));
        let source = root.join("source");
        let output = root.join("output");
        std::fs::create_dir_all(&source).unwrap();
        let first = OperationContext::new("playback", OperationSource::Terminal);
        let second = OperationContext::new("get", OperationSource::Terminal);
        let events = [&first, &second]
            .into_iter()
            .map(|operation| {
                let mut event = DiagnosticEvent::new(
                    "run",
                    Instant::now(),
                    EventName::REQUEST_COMPLETED,
                    EventCode::REQUEST_COMPLETED,
                    Severity::Error,
                    Component::Scheduler,
                    "Request completed",
                )
                .with_operation(operation.clone());
                event.fields.outcome = Some(OperationOutcome::Error);
                serde_json::to_string(&event).unwrap()
            })
            .collect::<Vec<_>>();
        std::fs::write(
            source.join(format!("{DIAGNOSTIC_PREFIX}focused.jsonl")),
            events.join("\n"),
        )
        .unwrap();
        let review = create_focused(
            &source,
            &output,
            Some(&format!("I-{}", first.short_reference())),
        )
        .unwrap();
        assert_eq!(review.event_count, 1);
        let retained = std::fs::read_to_string(output.join("events.jsonl")).unwrap();
        assert!(retained.contains(first.short_reference()));
        assert!(!retained.contains(second.short_reference()));
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn review_rejects_forbidden_data_even_with_matching_checksums() {
        let root = std::env::temp_dir().join(format!(
            "forbidden-support-test-{}",
            super::super::random_hex::<5>()
        ));
        let source = root.join("source");
        let output = root.join("output");
        std::fs::create_dir_all(&source).unwrap();
        create(&source, &output).unwrap();
        let diagnostics = "{\"path\":\"private\"}";
        let events = std::fs::read_to_string(output.join("events.jsonl")).unwrap();
        std::fs::write(output.join("diagnostics.json"), diagnostics).unwrap();
        let mut manifest: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(output.join("manifest.json")).unwrap())
                .unwrap();
        manifest["files"][0]["sha256"] = serde_json::json!(checksum(diagnostics.as_bytes()));
        std::fs::write(
            output.join("manifest.json"),
            serde_json::to_string_pretty(&manifest).unwrap(),
        )
        .unwrap();
        std::fs::write(
            output.join("checksums.sha256"),
            files_for_checksums(diagnostics, &events),
        )
        .unwrap();
        let error = review(&output).unwrap_err();
        assert!(error.to_string().contains("forbidden-data review failed"));
        assert!(!error.to_string().contains("private"));
        std::fs::remove_dir_all(root).unwrap();
    }
}
