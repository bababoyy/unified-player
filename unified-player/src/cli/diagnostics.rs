use std::fmt::Write as _;

use crate::{
    config::{ActiveProvider, Configs, StreamingType, YouTubeMusicAuthType},
    state::SharedState,
};

pub(super) fn render(configs: &Configs, state: Option<&SharedState>) -> String {
    let auth = configs.youtube_music_auth_status();
    let browser_session_ready = configs
        .config_folder
        .join("youtube")
        .join("browser-profile")
        .is_dir()
        && configs
            .config_folder
            .join("youtube")
            .join("browser-path.txt")
            .is_file();
    let (runtime_state, active_provider, playback_state) = runtime_facts(state);
    let mut output = String::new();

    writeln!(output, "unified-player diagnostic report").unwrap();
    writeln!(output, "version={}", env!("CARGO_PKG_VERSION")).unwrap();
    writeln!(
        output,
        "diagnostics.schema_version={}",
        crate::observability::DIAGNOSTIC_SCHEMA_VERSION
    )
    .unwrap();
    writeln!(
        output,
        "diagnostics.registered_events={}",
        crate::observability::EVENT_REGISTRY.len()
    )
    .unwrap();
    writeln!(
        output,
        "build.revision={}",
        option_env!("UNIFIED_PLAYER_GIT_REVISION").unwrap_or("unknown")
    )
    .unwrap();
    writeln!(
        output,
        "build.dirty={}",
        option_env!("UNIFIED_PLAYER_GIT_DIRTY").unwrap_or("unknown")
    )
    .unwrap();
    writeln!(output, "platform.os={}", std::env::consts::OS).unwrap();
    writeln!(output, "platform.arch={}", std::env::consts::ARCH).unwrap();
    writeln!(output, "runtime.state={runtime_state}").unwrap();
    writeln!(output, "runtime.active_provider={active_provider}").unwrap();
    writeln!(output, "runtime.playback={playback_state}").unwrap();
    writeln!(output, "runtime.lifecycle=supervised-staged-shutdown").unwrap();
    append_observability_facts(&mut output, state);
    writeln!(
        output,
        "config.startup_provider={}",
        configs.app_config.active_provider.title()
    )
    .unwrap();
    writeln!(
        output,
        "config.streaming={}",
        streaming_label(&configs.app_config.enable_streaming)
    )
    .unwrap();
    writeln!(
        output,
        "config.media_control={}",
        media_control_label(configs)
    )
    .unwrap();
    writeln!(
        output,
        "youtube.auth={}",
        youtube_auth_label(auth.auth_type, auth.ready)
    )
    .unwrap();
    writeln!(
        output,
        "youtube.auth.action={}",
        youtube_auth_action(auth.auth_type, auth.ready)
    )
    .unwrap();
    writeln!(
        output,
        "youtube.browser_session={}",
        ready_label(browser_session_ready)
    )
    .unwrap();
    writeln!(
        output,
        "youtube.playback_failure_categories={}",
        crate::client::youtube_playback_diagnostic_category_inventory()
    )
    .unwrap();
    writeln!(output, "build.streaming={}", cfg!(feature = "streaming")).unwrap();
    writeln!(
        output,
        "build.media_control={}",
        cfg!(feature = "media-control")
    )
    .unwrap();
    writeln!(output, "build.image={}", cfg!(feature = "image")).unwrap();
    writeln!(output, "build.notify={}", cfg!(feature = "notify")).unwrap();
    writeln!(output, "build.fzf={}", cfg!(feature = "fzf")).unwrap();
    writeln!(output, "build.daemon={}", cfg!(feature = "daemon")).unwrap();
    writeln!(output, "build.audio_backends={}", audio_backends()).unwrap();
    writeln!(output, "privacy=credential-values-and-locations-omitted").unwrap();

    output
}

fn append_observability_facts(output: &mut String, state: Option<&SharedState>) {
    let Some(state) = state else {
        writeln!(output, "diagnostics.runtime=not-connected").unwrap();
        return;
    };
    let health = state.diagnostics.health();
    writeln!(output, "diagnostics.run_id={}", state.diagnostics.run_id()).unwrap();
    writeln!(
        output,
        "diagnostics.uptime_ms={}",
        state.diagnostics.started_at().elapsed().as_millis()
    )
    .unwrap();
    writeln!(output, "diagnostics.writer={:?}", health.state).unwrap();
    writeln!(
        output,
        "diagnostics.dropped_events={}",
        health.dropped_events
    )
    .unwrap();
    writeln!(output, "diagnostics.files_created={}", health.files_created).unwrap();
    writeln!(output, "diagnostics.bytes_written={}", health.bytes_written).unwrap();
    let snapshot = state.diagnostics.health_snapshot();
    let filter = state.diagnostics.filter_snapshot();
    writeln!(
        output,
        "diagnostics.logging_health={}",
        snapshot.logging_status.label()
    )
    .unwrap();
    writeln!(
        output,
        "diagnostics.filter_level={}",
        filter.level.label().to_ascii_lowercase()
    )
    .unwrap();
    writeln!(output, "diagnostics.filter_temporary={}", filter.temporary).unwrap();
    writeln!(
        output,
        "diagnostics.filter_remaining_seconds={}",
        filter.remaining_seconds
    )
    .unwrap();
    writeln!(output, "diagnostics.filter_scope=verbose-tracing-only").unwrap();
    writeln!(
        output,
        "diagnostics.verbose_tracing_available={}",
        filter.available
    )
    .unwrap();
    writeln!(output, "diagnostics.core_causality=always-on").unwrap();
    writeln!(
        output,
        "diagnostics.active_operation={}",
        snapshot
            .active_operation
            .as_ref()
            .map_or("none", |operation| operation.operation.as_str())
    )
    .unwrap();
    writeln!(
        output,
        "diagnostics.active_reference={}",
        snapshot
            .active_operation
            .as_ref()
            .map_or("none", |operation| operation.reference.as_str())
    )
    .unwrap();
    append_component_worker_facts(output, &snapshot);
    if let Some(ui) = snapshot.ui.as_ref() {
        writeln!(output, "diagnostics.ui_page={}", ui.page).unwrap();
        writeln!(output, "diagnostics.ui_popup={}", ui.popup).unwrap();
        writeln!(output, "diagnostics.ui_revision={}", ui.revision).unwrap();
    }
}

fn append_component_worker_facts(
    output: &mut String,
    snapshot: &crate::observability::HealthSnapshot,
) {
    for component in &snapshot.components {
        writeln!(
            output,
            "diagnostics.component.{}={}:{}",
            format!("{:?}", component.component).to_ascii_lowercase(),
            component.status.label(),
            component.fact
        )
        .unwrap();
    }
    writeln!(
        output,
        "diagnostics.worker_count={}",
        snapshot.workers.len()
    )
    .unwrap();
    for (index, worker) in snapshot.workers.iter().enumerate() {
        writeln!(
            output,
            "diagnostics.worker.{index}={}:{}",
            worker.worker,
            worker.status.label()
        )
        .unwrap();
    }
}

fn runtime_facts(state: Option<&SharedState>) -> (&'static str, &'static str, &'static str) {
    let Some(state) = state else {
        return ("not-connected", "not-inspected", "not-inspected");
    };
    let provider = state.ui.lock().active_provider;
    let player = state.player.read();
    runtime_labels(
        provider,
        state.shutdown_requested(),
        player.playback.as_ref().map(|playback| playback.is_playing),
        player
            .youtube_playback
            .as_ref()
            .map(|playback| playback.is_playing),
    )
}

fn runtime_labels(
    provider: ActiveProvider,
    shutdown_requested: bool,
    spotify_is_playing: Option<bool>,
    youtube_is_playing: Option<bool>,
) -> (&'static str, &'static str, &'static str) {
    let runtime = if shutdown_requested {
        "shutdown-requested"
    } else {
        "running"
    };
    (
        runtime,
        provider.title(),
        playback_label(provider, spotify_is_playing, youtube_is_playing),
    )
}

fn youtube_auth_label(auth_type: YouTubeMusicAuthType, ready: bool) -> &'static str {
    match (auth_type, ready) {
        (YouTubeMusicAuthType::Browser, true) => "browser-ready",
        (YouTubeMusicAuthType::Browser, false) => "browser-missing",
        (YouTubeMusicAuthType::OAuth, true) => "oauth-ready",
        (YouTubeMusicAuthType::OAuth, false) => "oauth-missing",
        (YouTubeMusicAuthType::Unauthenticated, _) => "disabled",
    }
}

fn youtube_auth_action(auth_type: YouTubeMusicAuthType, ready: bool) -> &'static str {
    match (auth_type, ready) {
        (_, true) | (YouTubeMusicAuthType::Unauthenticated, _) => "none",
        (YouTubeMusicAuthType::Browser, false) => "run-youtube-sign-in",
        (YouTubeMusicAuthType::OAuth, false) => "run-youtube-oauth-login",
    }
}

fn playback_label(
    provider: ActiveProvider,
    spotify_is_playing: Option<bool>,
    youtube_is_playing: Option<bool>,
) -> &'static str {
    let is_playing = match provider {
        ActiveProvider::Spotify => spotify_is_playing,
        ActiveProvider::YouTubeMusic => youtube_is_playing,
    };
    match is_playing {
        Some(true) => "playing",
        Some(false) => "paused",
        None => "none",
    }
}

fn streaming_label(streaming: &StreamingType) -> &'static str {
    match streaming {
        StreamingType::Always => "always",
        StreamingType::DaemonOnly => "daemon-only",
        StreamingType::Never => "never",
    }
}

#[cfg(feature = "media-control")]
fn media_control_label(configs: &Configs) -> &'static str {
    if configs.app_config.enable_media_control {
        "enabled"
    } else {
        "disabled"
    }
}

#[cfg(not(feature = "media-control"))]
fn media_control_label(_configs: &Configs) -> &'static str {
    "not-compiled"
}

fn ready_label(ready: bool) -> &'static str {
    if ready {
        "ready"
    } else {
        "missing"
    }
}

fn audio_backends() -> String {
    let backends = [
        ("alsa", cfg!(feature = "alsa-backend")),
        ("gstreamer", cfg!(feature = "gstreamer-backend")),
        ("jackaudio", cfg!(feature = "jackaudio-backend")),
        ("portaudio", cfg!(feature = "portaudio-backend")),
        ("pulseaudio", cfg!(feature = "pulseaudio-backend")),
        ("rodio", cfg!(feature = "rodio-backend")),
        ("rodiojack", cfg!(feature = "rodiojack-backend")),
        ("sdl", cfg!(feature = "sdl-backend")),
    ]
    .into_iter()
    .filter_map(|(name, enabled)| enabled.then_some(name))
    .collect::<Vec<_>>()
    .join(",");
    if backends.is_empty() {
        "none".to_string()
    } else {
        backends
    }
}

#[cfg(test)]
mod tests {
    use super::{
        append_component_worker_facts, audio_backends, playback_label, ready_label, render,
        runtime_labels, streaming_label, youtube_auth_action, youtube_auth_label,
    };
    use crate::config::{ActiveProvider, Configs, StreamingType, YouTubeMusicAuthType};
    use crate::observability::{
        Component, DiagnosticEvent, EventCode, EventName, HealthRegistry, HealthStatus, Severity,
        WriterHealth, WriterState,
    };
    use std::time::Instant;

    #[test]
    fn diagnostic_labels_are_bounded_and_non_sensitive() {
        assert_eq!(streaming_label(&StreamingType::Always), "always");
        assert_eq!(streaming_label(&StreamingType::DaemonOnly), "daemon-only");
        assert_eq!(streaming_label(&StreamingType::Never), "never");
        assert_eq!(ready_label(true), "ready");
        assert_eq!(ready_label(false), "missing");
        assert_eq!(
            youtube_auth_label(YouTubeMusicAuthType::Browser, false),
            "browser-missing"
        );
        assert_eq!(
            youtube_auth_action(YouTubeMusicAuthType::Browser, false),
            "run-youtube-sign-in"
        );
        assert_eq!(
            youtube_auth_action(YouTubeMusicAuthType::Unauthenticated, false),
            "none"
        );

        let backends = audio_backends();
        assert!(!backends.contains('\\'));
        assert!(!backends.contains('/'));
        assert!(!backends.contains("http"));
    }

    #[test]
    fn live_playback_label_uses_only_the_active_provider() {
        assert_eq!(
            playback_label(ActiveProvider::Spotify, Some(false), Some(true)),
            "paused"
        );
        assert_eq!(
            playback_label(ActiveProvider::YouTubeMusic, Some(false), Some(true)),
            "playing"
        );
        assert_eq!(
            playback_label(ActiveProvider::YouTubeMusic, Some(true), None),
            "none"
        );
    }

    #[test]
    fn live_runtime_labels_include_shutdown_and_active_provider() {
        assert_eq!(
            runtime_labels(ActiveProvider::YouTubeMusic, true, Some(false), Some(true)),
            ("shutdown-requested", "YouTube Music", "playing")
        );
        assert_eq!(
            runtime_labels(ActiveProvider::Spotify, false, Some(false), Some(true)),
            ("running", "Spotify", "paused")
        );
    }

    #[test]
    fn live_report_projects_component_and_worker_health_without_activity() {
        let registry = HealthRegistry::default();
        registry.set_component(Component::Browser, HealthStatus::Healthy, "page-ready", 4);
        let mut worker = DiagnosticEvent::new(
            "run",
            Instant::now(),
            EventName::WORKER_TRANSITION,
            EventCode::WORKER_TRANSITION,
            Severity::Info,
            Component::Runtime,
            "Worker lifecycle changed",
        );
        worker.fields.worker = Some("browser-worker".to_owned());
        worker.fields.state = Some("started".to_owned());
        registry.observe(&worker);
        let snapshot = registry.snapshot(WriterHealth {
            state: WriterState::Healthy,
            dropped_events: 0,
            files_created: 0,
            bytes_written: 0,
        });

        let mut output = String::new();
        append_component_worker_facts(&mut output, &snapshot);
        assert!(output.contains("diagnostics.component.browser=healthy:page-ready"));
        assert!(output.contains("diagnostics.worker_count=1"));
        assert!(output.contains("diagnostics.worker.0=browser-worker:healthy"));
        for forbidden in ["title", "lyrics", "https://", "cookie", "video-id"] {
            assert!(!output.contains(forbidden));
        }
    }

    #[test]
    fn report_omits_config_and_credential_locations() {
        let private_location = std::env::temp_dir().join(format!(
            "diagnostic-fixture-secret-location-{}",
            std::process::id()
        ));
        std::fs::create_dir_all(&private_location).unwrap();
        let configs = Configs::new(&private_location, &private_location).unwrap();

        let report = render(&configs, None);

        std::fs::remove_dir_all(&private_location).unwrap();

        assert!(report.contains("runtime.state=not-connected"));
        assert!(report.contains("runtime.active_provider=not-inspected"));
        assert!(report.contains("privacy=credential-values-and-locations-omitted"));
        assert!(!report.contains(private_location.to_string_lossy().as_ref()));
        assert!(!report.contains("cookie.txt"));
        assert!(!report.contains("oauth.json"));
    }
}
