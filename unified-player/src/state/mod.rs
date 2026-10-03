mod constant;
mod context_history;
mod data;
mod home;
mod journal;
mod model;
mod player;
mod projection;
mod queue;
mod redraw;
mod session;
mod ui;
mod unified_matching;

use std::sync::{
    atomic::{AtomicBool, Ordering},
    Arc,
};

pub use constant::*;
pub use context_history::*;
pub use data::*;
pub use home::*;
pub use journal::*;
pub use model::*;
pub use player::*;
pub(crate) use player::{QueueDisplayItem, QueueDisplayItemRef};
#[allow(unused_imports)]
pub use projection::*;
#[allow(unused_imports)]
pub use queue::*;
pub use redraw::{RedrawSignal, TrackedMutex, TrackedMutexGuard, TrackedRwLock};
pub use session::*;
pub use ui::*;
pub use unified_matching::*;

use crate::runtime::{ShutdownPhase, ShutdownProgress};
use crate::{auth, config};
use tokio_util::sync::CancellationToken;

#[cfg(feature = "streaming")]
pub use parking_lot::Mutex;

/// Application's shared state
pub type SharedState = Arc<State>;

/// Application's state
pub struct State {
    pub ui: TrackedMutex<UIState>,
    pub player: TrackedRwLock<PlayerState>,
    pub data: TrackedRwLock<AppData>,
    /// Shared by the tracked locks above; the render loop waits on it.
    pub(crate) redraw: Arc<RedrawSignal>,

    pub is_daemon: bool,
    shutdown: CancellationToken,
    shutdown_progress: ShutdownProgress,
    playback_shutdown_complete: AtomicBool,

    /// Shared FFT frequency-band data written by the audio sink and read by the UI.
    /// `Some` only when `enable_audio_visualization` is `true`; avoids allocating
    /// the mutex/state entirely when the feature is not in use.
    #[cfg(feature = "streaming")]
    pub vis_bands: Option<Arc<Mutex<crate::ui::streaming::VisBands>>>,

    pub(crate) diagnostics: crate::observability::DiagnosticsHandle,

    #[cfg(feature = "private-capture")]
    private_capture_operator: RwLock<Option<crate::developer_capture::CaptureOperatorHandle>>,
}

impl State {
    /// Construct state from the immutable runtime configuration.
    pub fn new_with_configs(
        is_daemon: bool,
        diagnostics: crate::observability::DiagnosticsHandle,
        configs: &config::Configs,
    ) -> Self {
        let mut ui = UIState::default();
        ui.active_provider = configs.app_config.active_provider;
        ui.apply_presentation_config(&configs.app_config);
        if let Ok(registry) = config::AccountRegistry::load(&configs.config_folder) {
            ui.spotify_account_label = registry
                .active_label(config::ActiveProvider::Spotify)
                .map(str::to_owned);
            ui.youtube_account_label = registry
                .active_label(config::ActiveProvider::YouTubeMusic)
                .map(str::to_owned);
            ui.youtube_account_id = registry
                .active_id(config::ActiveProvider::YouTubeMusic)
                .map(str::to_owned);
            ui.spotify_account_id = registry
                .active_id(config::ActiveProvider::Spotify)
                .map(str::to_owned);
        }
        ui.spotify_auth_status = auth::cached_spotify_auth_snapshot(configs);
        ui.welcome_spotify_client_id
            .clone_from(&configs.app_config.client_id);
        ui.welcome_spotify_client_command = configs.app_config.client_id_command.is_some();
        ui.welcome_spotify_web_token_cached = auth::web_api_token_cached(configs);
        ui.youtube_auth_status = configs.youtube_music_auth_status();
        ui.welcome_youtube_browser =
            crate::client::resolve_browser_executable(&configs.config_folder, None).ok();
        ui.setup_state = configs.setup.clone();
        if ui.setup_state.status == config::SetupStatus::Ready {
            let missing_cached_auth = match ui.setup_state.startup_provider {
                config::ActiveProvider::Spotify if !ui.spotify_auth_status.session_ready => {
                    Some(config::SetupFailure::MissingSpotifySession)
                }
                config::ActiveProvider::YouTubeMusic
                    if !ui.setup_auth_snapshot().youtube.account_ready =>
                {
                    Some(config::SetupFailure::MissingYouTubeAccountAuth)
                }
                _ => None,
            };
            if let Some(failure) = missing_cached_auth {
                ui.setup_state.status = config::SetupStatus::Failed;
                ui.setup_state.failure = Some(failure);
            }
        }
        if ui.setup_state.requires_attention() {
            ui.history = vec![PageState::Welcome {
                state: WelcomePageUIState::new(),
                from_settings: false,
            }];
        }

        if let Some(theme) = configs.theme_config.find_theme(&configs.app_config.theme) {
            // update the UI's theme based on the `theme` config option
            ui.theme = theme;
        }

        let app_data = AppData::new(&configs.config_folder, &configs.cache_folder);

        let redraw = Arc::new(RedrawSignal::default());
        Self {
            ui: TrackedMutex::with_signal(ui, redraw.clone()),
            player: TrackedRwLock::with_signal(
                PlayerState::load_provider_sessions(&configs.config_folder),
                redraw.clone(),
            ),
            data: TrackedRwLock::with_signal(app_data, redraw.clone()),
            redraw,
            is_daemon,
            shutdown: CancellationToken::new(),
            shutdown_progress: ShutdownProgress::default(),
            playback_shutdown_complete: AtomicBool::new(false),
            #[cfg(feature = "streaming")]
            vis_bands: if configs.app_config.enable_audio_visualization {
                Some(Arc::new(Mutex::new(
                    crate::ui::streaming::VisBands::default(),
                )))
            } else {
                None
            },

            diagnostics,
            #[cfg(feature = "private-capture")]
            private_capture_operator: RwLock::new(None),
        }
    }

    /// Compatibility constructor for the existing test caller that uses the process-wide config.
    #[cfg(test)]
    pub fn new(is_daemon: bool, diagnostics: crate::observability::DiagnosticsHandle) -> Self {
        Self::new_with_configs(is_daemon, diagnostics, config::get_config())
    }

    #[cfg(feature = "private-capture")]
    pub(crate) fn install_private_capture_operator(
        &self,
        operator: crate::developer_capture::CaptureOperatorHandle,
    ) {
        *self.private_capture_operator.write() = Some(operator);
    }

    #[cfg(feature = "private-capture")]
    pub(crate) fn private_capture_operator(
        &self,
    ) -> Option<crate::developer_capture::CaptureOperatorHandle> {
        self.private_capture_operator.read().clone()
    }

    #[cfg(feature = "private-capture")]
    pub(crate) fn private_capture_operator_snapshot(
        &self,
    ) -> crate::developer_capture::SafeOperatorSnapshot {
        self.private_capture_operator()
            .map(|operator| operator.snapshot())
            .unwrap_or_default()
    }

    pub fn persist_provider_sessions(&self) {
        if let Err(err) = self
            .player
            .read()
            .save_provider_sessions(&config::get_config().config_folder)
        {
            crate::observability::log_safe_error!(
                warn,
                crate::observability::DiagnosticCode::PROVIDER_SESSION_PERSIST_FAILED,
                crate::observability::ErrorCategory::Storage,
                &err,
                "Unable to persist provider playback sessions"
            );
        }
    }

    pub(crate) fn shutdown_token(&self) -> CancellationToken {
        self.shutdown.clone()
    }

    pub(crate) fn shutdown_requested(&self) -> bool {
        self.shutdown.is_cancelled()
    }

    pub(crate) fn request_shutdown(&self) -> anyhow::Result<()> {
        self.shutdown_progress
            .advance(ShutdownPhase::InputStopped)?;
        self.shutdown.cancel();
        Ok(())
    }

    pub(crate) fn mark_shutdown_phase(&self, phase: ShutdownPhase) -> anyhow::Result<()> {
        self.shutdown_progress.advance(phase)
    }

    pub fn mark_playback_shutdown_complete(&self) {
        self.playback_shutdown_complete
            .store(true, Ordering::Release);
    }

    pub fn playback_shutdown_complete(&self) -> bool {
        self.playback_shutdown_complete.load(Ordering::Acquire)
    }

    #[cfg(feature = "streaming")]
    pub fn is_streaming_enabled(&self) -> bool {
        let configs = config::get_config();
        configs.app_config.enable_streaming == config::StreamingType::Always
            || (configs.app_config.enable_streaming == config::StreamingType::DaemonOnly
                && self.is_daemon)
    }

    /// Returns `true` when the custom queue system should be used for new playback.
    ///
    /// Requires streaming to be enabled and the `custom_queue` config option
    /// to be `true`.
    #[cfg(feature = "streaming")]
    #[allow(dead_code)]
    pub fn should_use_custom_queue(&self) -> bool {
        self.is_streaming_enabled() && config::get_config().app_config.custom_queue
    }

    /// Returns `true` when the local librespot player is actively streaming
    /// audio (i.e. a `Playing` event has been received and no `Paused` / `stop`
    /// has occurred since).  Used by the UI to decide whether to allocate and
    /// render the audio-visualization area.
    #[cfg(feature = "streaming")]
    pub fn is_local_streaming_active(&self) -> bool {
        self.vis_bands.as_ref().is_some_and(|b| b.lock().is_active)
    }
}
