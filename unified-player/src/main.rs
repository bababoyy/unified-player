mod auth;
mod cli;
mod client;
mod command;
mod config;
#[cfg(feature = "private-capture")]
#[allow(dead_code)]
mod developer_capture;
mod event;
mod key;
#[cfg(feature = "media-control")]
mod media_control;
mod observability;
mod playlist_folders;
mod runtime;
mod state;
#[cfg(feature = "streaming")]
mod streaming;
mod token;
mod ui;
mod utils;

use anyhow::{Context, Result};
use parking_lot::Mutex;
use std::{collections::VecDeque, io::Write, sync::Arc};
use tracing_subscriber::layer::SubscriberExt;
use tracing_subscriber::util::SubscriberInitExt;
use tracing_subscriber::Layer;

use crate::config::apply_config_override;

#[cfg(feature = "private-capture")]
fn private_capture_root() -> Result<std::path::PathBuf> {
    let configs = config::get_config();
    let parent = configs
        .config_folder
        .parent()
        .context("private capture root requires a config parent")?;
    let root = parent.join(".unified-player-private-captures");
    let mut ordinary_roots = vec![configs.config_folder.clone(), configs.cache_folder.clone()];
    if let Some(log_folder) = &configs.app_config.log_folder {
        ordinary_roots.push(log_folder.clone());
    }
    crate::developer_capture::ensure_private_root_separate(&root, &ordinary_roots)
        .context("private capture root is not separate from ordinary application data")?;
    Ok(root)
}

fn init_logging(
    log_folder: &std::path::Path,
    log_buffer: observability::UiDiagnosticRing,
) -> Result<(
    observability::DiagnosticsHandle,
    observability::DiagnosticsRuntime,
)> {
    if std::env::var_os("RUST_LOG").is_some_and(|x| x == "off") {
        let (handle, runtime) = observability::disabled(log_buffer);
        observability::install(handle.clone())?;
        return Ok((handle, runtime));
    }

    // initialize the application's logging
    if std::env::var("RUST_LOG").is_err() {
        std::env::set_var("RUST_LOG", "unified_player=info");
    }
    let (handle, runtime) = observability::start(log_folder, log_buffer)?;
    let diagnostic_layer = observability::DiagnosticLayer::new(handle.clone()).with_filter(
        tracing_subscriber::filter::filter_fn(crate::observability::is_application_event),
    );

    tracing_subscriber::registry()
        .with(tracing_subscriber::EnvFilter::new("unified_player=trace"))
        .with(diagnostic_layer)
        .init();
    observability::install(handle.clone())?;
    observability::record(observability::process_start_event(&handle));
    handle.set_component_health(
        observability::Component::Runtime,
        observability::HealthStatus::Healthy,
        "supervised",
    );
    handle.set_component_health(
        observability::Component::Scheduler,
        observability::HealthStatus::Starting,
        "awaiting-ingress",
    );
    handle.set_component_health(
        observability::Component::Browser,
        observability::HealthStatus::Unknown,
        "idle-or-unobserved",
    );
    handle.set_component_health(
        observability::Component::Audio,
        if cfg!(feature = "streaming") {
            observability::HealthStatus::Unknown
        } else {
            observability::HealthStatus::Disabled
        },
        if cfg!(feature = "streaming") {
            "idle-or-unobserved"
        } else {
            "not-compiled"
        },
    );

    // Keep panic evidence credential-safe until the bounded incident recorder is introduced.
    let backtrace_file = std::fs::File::create(log_folder.join(format!(
        "unified-player-panic-{}.txt",
        handle.run_id().get(..8).unwrap_or(handle.run_id())
    )))
    .context("failed to create panic report file")?;
    let backtrace_file = std::sync::Mutex::new(backtrace_file);
    std::panic::set_hook(Box::new(move |info| {
        let (reference, fingerprint) = crate::observability::record_panic(info.location());
        let Ok(mut file) = backtrace_file.lock() else {
            return;
        };
        let _ = writeln!(
            &mut file,
            "{}; reference={reference}; fingerprint={fingerprint}",
            crate::observability::SAFE_PANIC_REPORT
        );
    }));

    Ok((handle, runtime))
}

#[tokio::main]
async fn start_app(
    state: &state::SharedState,
    diagnostics: observability::DiagnosticsRuntime,
) -> Result<()> {
    const SHUTDOWN_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(10);

    // client channels
    let (client_pub, client_sub) = client::client_request_channel();
    let shutdown = state.shutdown_token();
    let mut runtime = runtime::AppRuntime::new(shutdown.clone());
    runtime.attach_diagnostics(diagnostics);
    let ingress_shutdown = runtime.ingress_token();
    let work_shutdown = runtime.work_token();

    #[cfg(feature = "private-capture")]
    let private_capture_shutdown = tokio_util::sync::CancellationToken::new();
    #[cfg(feature = "private-capture")]
    let prepared_private_capture: Result<
        (
            crate::developer_capture::CaptureHandle,
            crate::developer_capture::CaptureWorker,
            crate::developer_capture::CaptureOperatorHandle,
            crate::developer_capture::CaptureOperatorWorker,
        ),
        crate::developer_capture::SafeOperatorFailure,
    > = (|| {
        let limits = crate::developer_capture::CaptureLimits::default();
        let root = private_capture_root()
            .map_err(|_| crate::developer_capture::SafeOperatorFailure::VaultUnavailable)?;
        let (capture, writer, _maintenance) =
            crate::developer_capture::prepare_runtime(&root, limits)
                .map_err(|_| crate::developer_capture::SafeOperatorFailure::VaultUnavailable)?;
        let fresh = client::YouTubeFreshReplayAdapter::from_configs(config::get_config())
            .map_err(|_| crate::developer_capture::SafeOperatorFailure::ReplayUnavailable)?;
        let configs = config::get_config();
        let mut forbidden_derivative_roots =
            vec![configs.config_folder.clone(), configs.cache_folder.clone()];
        if let Some(log_folder) = &configs.app_config.log_folder {
            forbidden_derivative_roots.push(log_folder.clone());
        }
        let (backend, _maintenance) = crate::developer_capture::VaultOperatorBackend::open(
            &root,
            limits,
            dirs_next::home_dir(),
            forbidden_derivative_roots,
            client::YouTubeOfflineReplayAdapter,
            fresh,
            crate::developer_capture::TokioReplayTimer,
            crate::developer_capture::FreshReplayPolicy::default(),
            crate::developer_capture::SystemPrivateFolderOpener,
        )?;
        let (operator, operator_worker) =
            crate::developer_capture::prepare_operator_default(capture.clone(), Box::new(backend));
        Ok((capture, writer, operator, operator_worker))
    })();
    #[cfg(feature = "private-capture")]
    let private_capture_handle = match prepared_private_capture {
        Ok((capture, writer, operator, operator_worker)) => {
            let writer_shutdown = private_capture_shutdown.clone();
            if runtime
                .spawn_thread("private-capture-writer", false, move || {
                    writer.run(&writer_shutdown).map_err(Into::into)
                })
                .is_err()
            {
                private_capture_shutdown.cancel();
                state.install_private_capture_operator(
                    crate::developer_capture::unavailable_operator_handle(
                        crate::developer_capture::SafeOperatorFailure::WorkerStopped,
                    ),
                );
                tracing::warn!("Private capture writer could not be started");
                None
            } else {
                state.install_private_capture_operator(operator);
                let operator_shutdown = work_shutdown.clone();
                runtime.spawn_async("private-capture-operator", false, async move {
                    operator_worker
                        .run(&operator_shutdown)
                        .await
                        .map_err(Into::into)
                });
                Some(capture)
            }
        }
        Err(failure) => {
            state.install_private_capture_operator(
                crate::developer_capture::unavailable_operator_handle(failure),
            );
            tracing::warn!("Private capture operator is unavailable");
            None
        }
    };

    #[cfg(feature = "pulseaudio-backend")]
    {
        // set environment variables for PulseAudio
        if std::env::var("PULSE_PROP_application.name").is_err() {
            std::env::set_var("PULSE_PROP_application.name", "unified-player");
        }
        if std::env::var("PULSE_PROP_application.icon_name").is_err() {
            std::env::set_var("PULSE_PROP_application.icon_name", "spotify");
        }
        if std::env::var("PULSE_PROP_stream.description").is_err() {
            let configs = config::get_config();
            std::env::set_var(
                "PULSE_PROP_stream.description",
                format!(
                    "Spotify Connect endpoint ({})",
                    configs.app_config.device.name
                ),
            );
        }
        if std::env::var("PULSE_PROP_media.software").is_err() {
            std::env::set_var("PULSE_PROP_media.software", "Spotify");
        }
        if std::env::var("PULSE_PROP_media.role").is_err() {
            std::env::set_var("PULSE_PROP_media.role", "music");
        }
    }

    // create a Spotify API client
    let client = client::AppClient::new_without_auth().context("construct app client")?;
    #[cfg(feature = "private-capture")]
    if let Some(handle) = private_capture_handle {
        client.install_developer_capture(handle);
    }
    let should_initialize_existing = {
        let ui = state.ui.lock();
        !ui.setup_state.requires_attention() && ui.spotify_auth_status.session_ready
    };
    let startup_session_ready = if should_initialize_existing {
        match client.initialize_existing_session(state).await {
            Ok(()) => {
                let mut ui = state.ui.lock();
                if ui.setup_state.status == config::SetupStatus::Failed {
                    if !state.is_daemon {
                        ui.open_setup_page(false);
                    }
                    false
                } else {
                    ui.spotify_auth_status.ready()
                }
            }
            Err(err) => {
                let failure = client::startup_failure_metadata(&err);
                let code = if failure.requires_setup() {
                    observability::DiagnosticCode::SPOTIFY_AUTH_FAILED
                } else {
                    observability::DiagnosticCode::SPOTIFY_STARTUP_DEGRADED
                };
                tracing::warn!(
                    diagnostic = %observability::safe_error(code, failure.category(), &err),
                    phase = failure.phase.as_str(),
                    status_class = failure.status_class.as_str(),
                    retryable = failure.retryable,
                    "Configured Spotify session could not be initialized"
                );
                if failure.requires_setup() {
                    let mut ui = state.ui.lock();
                    let setup_failure = ui
                        .setup_state
                        .failure
                        .unwrap_or(config::SetupFailure::AuthenticationFailed);
                    ui.mark_setup_failed(setup_failure);
                    if !state.is_daemon {
                        ui.open_setup_page(false);
                    }
                }
                false
            }
        }
    } else {
        false
    };

    // request user data
    let startup_client_pub = client_pub.with_source(observability::OperationSource::Startup);
    if startup_session_ready {
        startup_client_pub.send(client::ClientRequest::GetCurrentUser)?;
        startup_client_pub.send(client::ClientRequest::GetUserPlaylists)?;
        startup_client_pub.send(client::ClientRequest::GetUserFollowedArtists)?;
        startup_client_pub.send(client::ClientRequest::GetUserSavedAlbums)?;
        startup_client_pub.send(client::ClientRequest::GetContext(state::ContextId::Tracks(
            state::USER_LIKED_TRACKS_ID.to_owned(),
        )))?;
        startup_client_pub.send(client::ClientRequest::GetUserSavedShows)?;
    }
    if config::get_config().app_config.active_provider == config::ActiveProvider::YouTubeMusic
        && config::get_config().youtube_music_auth_status().is_ready()
    {
        startup_client_pub.send(client::ClientRequest::GetYouTubeLibrary)?;
    }

    // client socket task (for handling CLI commands)
    runtime.spawn_async("client-socket", false, {
        let client = client.clone();
        let state = state.clone();
        let shutdown = ingress_shutdown;
        async move {
            Box::pin(cli::start_socket_until(
                &client,
                Some(&state),
                None,
                shutdown,
            ))
            .await;
            Ok(())
        }
    });

    // client event handler task
    runtime.spawn_async("client-handler", true, {
        let state = state.clone();
        let client = client.clone();
        let shutdown = work_shutdown.clone();
        async move {
            Box::pin(client::start_client_handler(
                &state, &client, client_sub, shutdown,
            ))
            .await;
            Ok(())
        }
    });

    // background task that detects an invalidated session and reconnects,
    // independent of any incoming client request
    runtime.spawn_async("session-watcher", true, {
        let state = state.clone();
        let client = client.clone();
        let shutdown = work_shutdown.clone();
        async move {
            client::start_session_watcher(state, client, shutdown).await;
            Ok(())
        }
    });

    // player event watcher task
    runtime.spawn_thread("player-event-watcher", true, {
        let state = state.clone();
        let client_pub = client_pub.with_source(observability::OperationSource::Runtime);
        let shutdown = work_shutdown.clone();
        move || {
            client::start_player_event_watcher(&state, &client_pub, &shutdown);
            Ok(())
        }
    })?;

    if !state.is_daemon {
        let terminal = ui::init_terminal().context("initialize terminal")?;
        #[cfg(feature = "image")]
        let terminal = match ui::init_image_picker(state) {
            Ok(()) => terminal,
            Err(err) => {
                if let Err(cleanup_err) = ui::clean_up(terminal) {
                    tracing::error!(
                        error = %cleanup_err,
                        "Failed to restore terminal after image picker initialization"
                    );
                }
                return Err(err).context("initialize image picker");
            }
        };

        // terminal event handler task
        runtime.spawn_thread("terminal-event-handler", true, {
            let client_pub = client_pub.with_source(observability::OperationSource::Terminal);
            let state = state.clone();
            move || {
                event::start_event_handler(&state, &client_pub);
                Ok(())
            }
        })?;

        // application UI task
        runtime.spawn_thread("ui", true, {
            let state = state.clone();
            move || ui::run(&state, terminal)
        })?;
    }

    #[cfg(feature = "media-control")]
    if config::get_config().app_config.enable_media_control {
        // media control task
        runtime.spawn_thread("media-control", false, {
            let state = state.clone();
            let client_pub = client_pub.with_source(observability::OperationSource::MediaControl);
            let shutdown = work_shutdown;
            move || {
                media_control::start_event_watcher(&state, client_pub, &shutdown)
                    .map_err(Into::into)
            }
        })?;

        // the winit's event loop must be run in the main thread
        #[cfg(any(target_os = "macos", target_os = "windows"))]
        {
            // Start an event loop that listens to OS window events.
            //
            // MacOS and Windows require an open window to be able to listen to media
            // control events. The below code will create an invisible window on startup
            // to listen to such events.
            let event_loop = winit::event_loop::EventLoop::new()?;
            let state = state.clone();
            #[allow(deprecated)]
            event_loop.run(move |_, event_loop| {
                event_loop.set_control_flow(winit::event_loop::ControlFlow::WaitUntil(
                    std::time::Instant::now() + std::time::Duration::from_millis(100),
                ));
                if state.shutdown_requested() {
                    event_loop.exit();
                }
            })?;
        }
    }

    shutdown.cancelled().await;
    state.request_shutdown()?;
    state.ui.lock().is_running = false;
    runtime.stop_ingress();
    state.mark_shutdown_phase(runtime::ShutdownPhase::IngressStopped)?;
    runtime.cancel_work();
    client.cancel_pending_playback_work();
    state.mark_shutdown_phase(runtime::ShutdownPhase::WorkCancelled)?;

    let shutdown_request = tokio::time::timeout(
        SHUTDOWN_TIMEOUT,
        client_pub.send_async(client::ClientRequest::ShutdownPlayback),
    )
    .await;
    drop(client_pub);
    if !matches!(shutdown_request, Ok(Ok(()))) {
        tracing::error!("Client request ingress stopped before playback shutdown was accepted");
        client
            .handle_request(state, client::ClientRequest::ShutdownPlayback, None)
            .await
            .context("perform fallback playback shutdown")?;
    }

    #[cfg(feature = "private-capture")]
    private_capture_shutdown.cancel();

    if state.is_daemon {
        state.mark_shutdown_phase(runtime::ShutdownPhase::TerminalRestored)?;
    }
    runtime
        .join(SHUTDOWN_TIMEOUT, || {
            state.mark_shutdown_phase(runtime::ShutdownPhase::WorkersJoined)
        })
        .await?;
    Ok(())
}

fn main() -> Result<()> {
    // librespot depends on hyper-rustls which requires a crypto provider to be set up.
    // TODO: see if this can be fixed upstream
    rustls::crypto::ring::default_provider()
        .install_default()
        .unwrap();

    // parse command line arguments
    let args = cli::init_cli()?.get_matches();

    // Select isolated roots before any config bootstrap or writer can run.
    let preview_root = if args.subcommand_name() == Some("demo") {
        Some(tempfile::tempdir().context("create isolated demo folders")?)
    } else {
        None
    };

    // initialize the application's cache and config folders
    let config_folder: std::path::PathBuf = preview_root.as_ref().map_or_else(
        || {
            args.get_one::<String>("config-folder")
                .expect("config-folder should have default value")
                .into()
        },
        |root| root.path().join("config"),
    );
    if !config_folder.exists() {
        std::fs::create_dir_all(&config_folder)?;
    }
    let youtube_config_folder = config_folder.join("youtube");
    if !youtube_config_folder.exists() {
        std::fs::create_dir_all(&youtube_config_folder)?;
    }

    let cache_folder: std::path::PathBuf = preview_root.as_ref().map_or_else(
        || {
            args.get_one::<String>("cache-folder")
                .expect("cache-folder should have a default value")
                .into()
        },
        |root| root.path().join("cache"),
    );
    let cache_audio_folder = cache_folder.join("audio");
    if !cache_audio_folder.exists() {
        std::fs::create_dir_all(&cache_audio_folder)?;
    }
    let cache_image_folder = cache_folder.join("image");
    if !cache_image_folder.exists() {
        std::fs::create_dir_all(&cache_image_folder)?;
    }

    // initialize the application configs
    {
        let mut configs = config::Configs::new(&config_folder, &cache_folder)?;
        if configs.app_config.log_folder.is_none() {
            // set the log folder to be the cache folder if it is not set
            configs.app_config.log_folder = Some(cache_folder);
        }
        if let Some(overrides) = args.get_many::<String>("config-override") {
            for override_str in overrides {
                let (key, value) = override_str.split_once('=').context(format!(
                    "Invalid override format: '{override_str}'. Expected KEY=VALUE"
                ))?;

                apply_config_override(&mut configs.app_config, key, value)?;
            }
        }
        if let Some(provider) = args.get_one::<String>("provider") {
            configs.app_config.active_provider = match provider.as_str() {
                "spotify" => config::ActiveProvider::Spotify,
                "youtube" | "youtube-music" => config::ActiveProvider::YouTubeMusic,
                _ => unreachable!("clap value_parser restricts provider values"),
            };
        }
        config::set_config(configs);
    }

    match args.subcommand() {
        None => {
            // initialize the application's log
            let log_folder = config::get_config()
                .app_config
                .log_folder
                .as_deref()
                .expect("log_folder is set");

            let log_buffer: observability::UiDiagnosticRing =
                Arc::new(Mutex::new(VecDeque::with_capacity(1000)));

            let (diagnostic_handle, diagnostics) = init_logging(log_folder, log_buffer)
                .context("failed to initialize application's logging")?;

            tracing::info!("Application configuration loaded");

            let is_daemon;

            #[cfg(feature = "daemon")]
            {
                is_daemon = args.get_flag("daemon");
                if is_daemon {
                    if cfg!(any(target_os = "macos", target_os = "windows"))
                        && cfg!(feature = "media-control")
                    {
                        eprintln!("Running the application as a daemon on windows/macos with `media-control` feature enabled is not supported!");
                        std::process::exit(1);
                    }

                    tracing::info!("Starting the application as a daemon...");
                    let daemonize = daemonize::Daemonize::new();
                    daemonize.start()?;
                }
            }

            #[cfg(not(feature = "daemon"))]
            {
                is_daemon = false;
            }

            let state = std::sync::Arc::new(state::State::new_with_configs(
                is_daemon,
                diagnostic_handle,
                config::get_config(),
            ));
            start_app(&state, diagnostics)
        }
        Some((cmd, args)) => cli::handle_cli_subcommand(cmd, args),
    }
}
