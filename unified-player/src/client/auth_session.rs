use std::{
    borrow::Cow,
    collections::HashSet,
    fmt,
    sync::{Arc, Mutex as StdMutex},
    time::Duration,
};

use anyhow::{Context as _, Result};
#[cfg(feature = "streaming")]
use parking_lot::Mutex;
use rspotify::prelude::*;
use serde::Serialize;
use tokio_util::sync::CancellationToken;

use crate::{
    auth::{self, AuthConfig},
    config,
    state::{MemoryCaches, SharedState},
};

use super::{
    playback_coordinator, playback_state, spotify, state_application, youtube, AccountOperation,
    AppClient, ClientRequest,
};

const EXISTING_SESSION_ATTEMPTS: usize = 2;
const EXISTING_SESSION_RETRY_DELAY: Duration = Duration::from_millis(250);
#[cfg(feature = "streaming")]
const SPOTIFY_STREAM_START_TIMEOUT: Duration = Duration::from_secs(30);

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum StartupFailurePhase {
    Unknown,
    WebApiToken,
    IntegratedSession,
    CurrentUser,
    AccountSnapshot,
    PlaybackProbe,
}

impl StartupFailurePhase {
    pub(crate) const fn as_str(self) -> &'static str {
        match self {
            Self::Unknown => "unknown",
            Self::WebApiToken => "web_api_token",
            Self::IntegratedSession => "integrated_session",
            Self::CurrentUser => "current_user",
            Self::AccountSnapshot => "account_snapshot",
            Self::PlaybackProbe => "playback_probe",
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum StartupStatusClass {
    Unknown,
    Unauthorized,
    Forbidden,
    RateLimited,
    ProviderError,
    ServerError,
    Network,
    Storage,
}

impl StartupStatusClass {
    pub(crate) const fn as_str(self) -> &'static str {
        match self {
            Self::Unknown => "unknown",
            Self::Unauthorized => "unauthorized",
            Self::Forbidden => "forbidden",
            Self::RateLimited => "rate_limited",
            Self::ProviderError => "provider_error",
            Self::ServerError => "server_error",
            Self::Network => "network",
            Self::Storage => "storage",
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct StartupFailureMetadata {
    pub(crate) phase: StartupFailurePhase,
    pub(crate) status_class: StartupStatusClass,
    pub(crate) retryable: bool,
}

impl StartupFailureMetadata {
    pub(crate) const fn category(self) -> crate::observability::ErrorCategory {
        match self.status_class {
            StartupStatusClass::Unauthorized | StartupStatusClass::Forbidden => {
                crate::observability::ErrorCategory::Authentication
            }
            StartupStatusClass::RateLimited => crate::observability::ErrorCategory::RateLimited,
            StartupStatusClass::Network => crate::observability::ErrorCategory::NetworkUnavailable,
            StartupStatusClass::Storage => crate::observability::ErrorCategory::Storage,
            StartupStatusClass::ServerError => {
                crate::observability::ErrorCategory::ProviderUnavailable
            }
            StartupStatusClass::ProviderError | StartupStatusClass::Unknown => {
                crate::observability::ErrorCategory::Unavailable
            }
        }
    }

    pub(crate) const fn requires_setup(self) -> bool {
        !self.retryable
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct StartupFailureMarker {
    metadata: StartupFailureMetadata,
}

impl fmt::Display for StartupFailureMarker {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            formatter,
            "startup failure phase={} status_class={} retryable={}",
            self.metadata.phase.as_str(),
            self.metadata.status_class.as_str(),
            self.metadata.retryable
        )
    }
}

impl std::error::Error for StartupFailureMarker {}

fn startup_status_class_for_http_status(status: u16) -> StartupStatusClass {
    match status {
        401 => StartupStatusClass::Unauthorized,
        403 => StartupStatusClass::Forbidden,
        429 => StartupStatusClass::RateLimited,
        500..=599 => StartupStatusClass::ServerError,
        400..=499 => StartupStatusClass::ProviderError,
        _ => StartupStatusClass::Unknown,
    }
}

fn startup_status_class_for_error(
    error: &anyhow::Error,
    phase: StartupFailurePhase,
) -> StartupStatusClass {
    if let Some(status) = super::provider_metadata::spotify_api_status_code(error) {
        return startup_status_class_for_http_status(status);
    }

    if let Some(error) = error.downcast_ref::<rspotify::ClientError>() {
        return startup_status_class_for_rspotify_error(error);
    }

    for cause in error.chain() {
        if cause.downcast_ref::<reqwest::Error>().is_some() {
            return StartupStatusClass::Network;
        }
        if let Some(error) = cause.downcast_ref::<rspotify::ClientError>() {
            return startup_status_class_for_rspotify_error(error);
        }
        if matches!(
            phase,
            StartupFailurePhase::WebApiToken | StartupFailurePhase::AccountSnapshot
        ) && cause.downcast_ref::<std::io::Error>().is_some()
        {
            return StartupStatusClass::Storage;
        }
    }

    StartupStatusClass::Unknown
}

fn startup_status_class_for_rspotify_error(error: &rspotify::ClientError) -> StartupStatusClass {
    match error {
        rspotify::ClientError::Http(error) => match error.as_ref() {
            rspotify::http::HttpError::StatusCode(response) => {
                startup_status_class_for_http_status(response.status().as_u16())
            }
            rspotify::http::HttpError::Client(_) => StartupStatusClass::Network,
        },
        rspotify::ClientError::InvalidToken => StartupStatusClass::Unauthorized,
        rspotify::ClientError::CacheFile(_) | rspotify::ClientError::Io(_) => {
            StartupStatusClass::Storage
        }
        _ => StartupStatusClass::Unknown,
    }
}

fn startup_failure_metadata_for(
    phase: StartupFailurePhase,
    status_class: StartupStatusClass,
) -> StartupFailureMetadata {
    let retryable = match status_class {
        StartupStatusClass::Unauthorized | StartupStatusClass::Forbidden => false,
        StartupStatusClass::Unknown if matches!(phase, StartupFailurePhase::IntegratedSession) => {
            false
        }
        _ => true,
    };
    StartupFailureMetadata {
        phase,
        status_class,
        retryable,
    }
}

fn annotate_startup_error(error: anyhow::Error, phase: StartupFailurePhase) -> anyhow::Error {
    let status_class = startup_status_class_for_error(&error, phase);
    error.context(StartupFailureMarker {
        metadata: startup_failure_metadata_for(phase, status_class),
    })
}

pub(crate) fn startup_failure_metadata(error: &anyhow::Error) -> StartupFailureMetadata {
    if let Some(marker) = error.downcast_ref::<StartupFailureMarker>() {
        return marker.metadata;
    }
    error
        .chain()
        .find_map(|cause| {
            cause
                .downcast_ref::<StartupFailureMarker>()
                .map(|marker| marker.metadata)
        })
        .unwrap_or(StartupFailureMetadata {
            phase: StartupFailurePhase::Unknown,
            status_class: StartupStatusClass::Unknown,
            retryable: false,
        })
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum SpotifyAuthenticationOutcome {
    Ready,
    WebApiRateLimited { retry_after: Duration },
}

impl SpotifyAuthenticationOutcome {
    fn completion_message(self, success: String) -> String {
        match self {
            Self::Ready => success,
            Self::WebApiRateLimited { retry_after } => format!(
                "Sign-in saved; Spotify Web API limited (429). Wait {}s, then Check existing session.",
                retry_after.as_secs().max(1)
            ),
        }
    }
}

fn spotify_failure_notice(error: &anyhow::Error) -> String {
    let failure = startup_failure_metadata(error);
    let phase = match failure.phase {
        StartupFailurePhase::WebApiToken => "Spotify browser authorization",
        StartupFailurePhase::IntegratedSession => "Spotify playback connection",
        StartupFailurePhase::CurrentUser => "Spotify library/profile check",
        StartupFailurePhase::AccountSnapshot => "Saving Spotify sign-in",
        StartupFailurePhase::PlaybackProbe => "Spotify playback check",
        StartupFailurePhase::Unknown => "Spotify sign-in/check",
    };
    let next = match failure.status_class {
        StartupStatusClass::Unauthorized => "Authorization was rejected (401). Sign in again.",
        StartupStatusClass::Forbidden => "Access denied (403). Check app account access, or Apply & sign in with the bundled client.",
        StartupStatusClass::RateLimited => "Spotify is rate-limiting requests (429). Wait before using Check existing session.",
        StartupStatusClass::Network | StartupStatusClass::ServerError => "Check connectivity and retry shortly.",
        StartupStatusClass::Storage => "Check configuration/cache permissions and retry.",
        _ => match failure.phase {
            StartupFailurePhase::WebApiToken => "Retry approval; check the configured redirect URI matches your app.",
            StartupFailurePhase::IntegratedSession => "Web API approved; retry playback approval with a Premium account.",
            _ => "Retry or open Diagnostics for details.",
        },
    };
    format!("{phase} failed. {next}")
}

fn spotify_auth_bootstrap_can_degrade(failure: StartupFailureMetadata) -> bool {
    failure.status_class == StartupStatusClass::RateLimited
        && matches!(
            failure.phase,
            StartupFailurePhase::CurrentUser | StartupFailurePhase::PlaybackProbe
        )
}

fn merge_cached_spotify_auth_snapshot(
    mut cached: config::SpotifyAuthSnapshot,
    live: config::SpotifyAuthSnapshot,
) -> config::SpotifyAuthSnapshot {
    if cached.session_ready
        && live.session_ready
        && matches!(cached.premium, config::SpotifyPremiumStatus::Unknown)
    {
        cached.premium = live.premium;
    }
    cached
}

fn log_spotify_auth_bootstrap_degraded(
    error: &anyhow::Error,
    failure: StartupFailureMetadata,
    retry_after: Duration,
) {
    tracing::warn!(
        diagnostic = %crate::observability::safe_error(
            crate::observability::DiagnosticCode::SPOTIFY_STARTUP_DEGRADED,
            failure.category(),
            error,
        ),
        phase = failure.phase.as_str(),
        status_class = failure.status_class.as_str(),
        retryable = failure.retryable,
        retry_after_ms = retry_after.as_millis(),
        "Spotify authentication succeeded, but Web API bootstrap was rate-limited"
    );
}

#[derive(Default)]
pub(super) struct YouTubeBrowserLoginControl {
    generation: u64,
    active: Option<(u64, CancellationToken)>,
}

impl YouTubeBrowserLoginControl {
    fn begin(&mut self) -> Option<(u64, CancellationToken)> {
        if self.active.is_some() {
            self.cancel_active();
            return None;
        }
        self.generation = self.generation.wrapping_add(1);
        let generation = self.generation;
        let cancellation = CancellationToken::new();
        self.active = Some((generation, cancellation.clone()));
        Some((generation, cancellation))
    }

    fn cancel_active(&self) -> bool {
        let Some((_, cancellation)) = &self.active else {
            return false;
        };
        cancellation.cancel();
        true
    }

    fn finish(&mut self, generation: u64) {
        if self
            .active
            .as_ref()
            .is_some_and(|(active_generation, _)| *active_generation == generation)
        {
            self.active = None;
        }
    }
}

impl AppClient {
    async fn handle_account_operation(
        &self,
        state: &SharedState,
        operation: AccountOperation,
    ) -> Result<()> {
        let provider = operation.provider();
        let status_key = account_status_key(provider);
        let _operation = self.account_operation_lock.lock().await;
        let result = match operation {
            AccountOperation::Add(provider) => self.add_account(state, provider).await,
            AccountOperation::Switch {
                provider,
                account_id,
            } => self.switch_account(state, provider, &account_id).await,
            AccountOperation::Validate(provider) => self.validate_account(state, provider).await,
            AccountOperation::Remove(provider) => self.remove_account(state, provider).await,
        };

        match result {
            Ok(message) => {
                self.refresh_account_ui_status(state, provider);
                self.state_application.update_settings_action_status(
                    state,
                    status_key,
                    Ok(message),
                );
            }
            Err(error) => {
                crate::observability::log_safe_error!(
                    error,
                    match provider {
                        config::ActiveProvider::Spotify => {
                            crate::observability::DiagnosticCode::SPOTIFY_AUTH_FAILED
                        }
                        config::ActiveProvider::YouTubeMusic => {
                            crate::observability::DiagnosticCode::YOUTUBE_AUTH_TEST_FAILED
                        }
                    },
                    crate::observability::ErrorCategory::Authentication,
                    &error,
                    "Account operation failed"
                );
                self.state_application.update_settings_action_status(
                    state,
                    status_key,
                    Err(&error),
                );
            }
        }
        Ok(())
    }

    async fn add_account(
        &self,
        state: &SharedState,
        provider: config::ActiveProvider,
    ) -> Result<String> {
        let (config_folder, cache_folder, cookie_path) = account_paths();
        let previous = config::AccountRegistry::load(&config_folder)
            .ok()
            .and_then(|registry| registry.active_id(provider).map(str::to_owned));
        self.prepare_account_change(state, provider).await?;
        let result = match provider {
            config::ActiveProvider::Spotify => {
                let _spotify_auth_guard = self.spotify_auth_lock.try_lock().map_err(|_| {
                    anyhow::anyhow!(
                        "Spotify authentication already in progress; wait for the current check to finish"
                    )
                })?;
                let authentication = self.authenticate_spotify(state).await?;
                let label = state.data.read().user_data.user.as_ref().map(|user| {
                    user.display_name
                        .clone()
                        .unwrap_or_else(|| user.id.id().to_string())
                });
                let mut registry = config::AccountRegistry::load(&config_folder)?;
                let record = registry.register_current(
                    provider,
                    label.as_deref(),
                    &config_folder,
                    &cache_folder,
                    &cookie_path,
                )?;
                Ok(authentication
                    .completion_message(format!("Added Spotify account {}", record.label)))
            }
            config::ActiveProvider::YouTubeMusic => self.add_youtube_account(state).await,
        };
        if result.is_err() {
            if let Some(previous) = previous {
                if let Ok(mut registry) = config::AccountRegistry::load(&config_folder) {
                    let _ = registry.activate(
                        provider,
                        &previous,
                        &config_folder,
                        &cache_folder,
                        &cookie_path,
                    );
                }
            }
        }
        result
    }

    async fn add_youtube_account(&self, state: &SharedState) -> Result<String> {
        let (generation, cancellation) = {
            let mut control = self.youtube_browser_login.lock().await;
            if control.active.is_some() {
                control.cancel_active();
                return Ok("YouTube Music account sign-in cancellation requested".to_string());
            }
            control
                .begin()
                .expect("account sign-in control was checked as inactive")
        };

        let result: Result<Option<String>> = async {
            let configs = config::get_config();
            let config_folder = configs.config_folder.clone();
            let mut session = youtube::browser_auth::begin_login_for_new_account(
                &configs.config_folder,
                None,
                false,
            )
            .await
            .context("open dedicated YouTube sign-in browser")?;
            self.state_application.update_settings_action_status(
                state,
                "accounts.youtube_music.status",
                Ok("Waiting for Google sign-in; activate the sign-in action again to cancel".to_string()),
            );
            let cookie_path = configs.youtube_music_cookie_path();
            let cookie_count = session
                .wait_for_sign_in_and_save(&cookie_path, true, &cancellation)
                .await?;
            let Some(cookie_count) = cookie_count else {
                return Ok(None);
            };
            session.promote_to_active_profile(&config_folder)?;
            drop(session);
            let had_youtube_account = {
                let registry = config::AccountRegistry::load(&configs.config_folder)?;
                !registry.youtube_music.is_empty()
            };
            config::save_app_config_override(
                &configs.config_folder,
                "youtube.auth_type",
                "Browser",
            )
            .context("select YouTube browser authentication")?;
            let active_configs = config::Configs::new_without_account_bootstrap(
                &configs.config_folder,
                &configs.cache_folder,
            )
            .context("reload YouTube browser authentication")?;
            *self.youtube.lock().await = None;
            state.ui.lock().youtube_auth_status = active_configs.youtube_music_auth_status();
            self.state_application.update_settings_action_status(
                state,
                "accounts.youtube_music.status",
                Ok("Browser sign-in saved; validating YouTube Music account access".to_string()),
            );
            let library = check_youtube_auth_with_configs(&active_configs)
                .await
                .context("validate YouTube Music account access")?;
            state.data.write().user_data.youtube_library = library;
            let mut registry = config::AccountRegistry::load(&active_configs.config_folder)?;
            let record = if had_youtube_account {
                registry.register_current(
                    config::ActiveProvider::YouTubeMusic,
                    None,
                    &active_configs.config_folder,
                    &active_configs.cache_folder,
                    &active_configs.youtube_music_cookie_path(),
                )?
            } else {
                registry.refresh_or_register_current(
                    config::ActiveProvider::YouTubeMusic,
                    None,
                    &active_configs.config_folder,
                    &active_configs.cache_folder,
                    &active_configs.youtube_music_cookie_path(),
                )?
            };
            Ok(Some(format!(
                "Added YouTube Music account {} ({cookie_count} cookies; browser account access ready)",
                record.label
            )))
        }
        .await;

        {
            let mut control = self.youtube_browser_login.lock().await;
            control.finish(generation);
        }

        match result? {
            Some(message) => Ok(message),
            None => Ok("YouTube Music account sign-in cancelled".to_string()),
        }
    }

    async fn validate_account(
        &self,
        state: &SharedState,
        provider: config::ActiveProvider,
    ) -> Result<String> {
        self.prepare_account_change(state, provider).await?;
        let (config_folder, cache_folder, cookie_path) = account_paths();
        let registry = config::AccountRegistry::load(&config_folder)?;
        let active = require_active_account(
            &registry,
            provider,
            &config_folder,
            &cache_folder,
            &cookie_path,
        )?;
        self.validate_active_account(state, provider).await?;
        Ok(format!(
            "Validated {} account {}",
            provider.title(),
            active.label
        ))
    }

    async fn switch_account(
        &self,
        state: &SharedState,
        provider: config::ActiveProvider,
        account_id: &str,
    ) -> Result<String> {
        let (config_folder, cache_folder, cookie_path) = account_paths();
        let mut registry = config::AccountRegistry::load(&config_folder)?;
        let selected = registry
            .summaries(provider, &config_folder, &cache_folder, &cookie_path)
            .into_iter()
            .find(|account| account.id == account_id)
            .context("the requested account is not registered")?;
        anyhow::ensure!(
            selected.ready,
            "the selected account has no complete saved session"
        );
        let previous = registry.active_id(provider).map(str::to_owned);
        self.prepare_account_change(state, provider).await?;
        registry.activate(
            provider,
            account_id,
            &config_folder,
            &cache_folder,
            &cookie_path,
        )?;
        let result = self.validate_active_account(state, provider).await;
        if let Err(error) = result {
            if let Some(previous) = previous.filter(|previous| previous != account_id) {
                let restored = registry.activate(
                    provider,
                    &previous,
                    &config_folder,
                    &cache_folder,
                    &cookie_path,
                );
                if restored.is_ok() {
                    let _ = self.validate_active_account(state, provider).await;
                }
            }
            return Err(error).context("validate switched account");
        }
        Ok(format!(
            "Switched to {} account {}",
            provider.title(),
            selected.label
        ))
    }

    async fn remove_account(
        &self,
        state: &SharedState,
        provider: config::ActiveProvider,
    ) -> Result<String> {
        let (config_folder, cache_folder, cookie_path) = account_paths();
        let mut registry = config::AccountRegistry::load(&config_folder)?;
        let active = require_active_account(
            &registry,
            provider,
            &config_folder,
            &cache_folder,
            &cookie_path,
        )?;
        self.prepare_account_change(state, provider).await?;
        registry.remove(
            provider,
            &active.id,
            &config_folder,
            &cache_folder,
            &cookie_path,
        )?;
        let next = registry.active_id(provider).map(str::to_owned);
        if next.is_some() {
            self.clear_account_data(state, provider);
            self.validate_active_account(state, provider).await?;
            Ok(format!(
                "Removed {}; switched to the next {} account",
                active.label,
                provider.title()
            ))
        } else {
            self.clear_account_data(state, provider);
            self.refresh_account_ui_status(state, provider);
            Ok(format!(
                "Removed {} account {}",
                provider.title(),
                active.label
            ))
        }
    }

    async fn validate_active_account(
        &self,
        state: &SharedState,
        provider: config::ActiveProvider,
    ) -> Result<()> {
        self.clear_account_data(state, provider);
        match provider {
            config::ActiveProvider::Spotify => {
                self.initialize_existing_session(state).await?;
            }
            config::ActiveProvider::YouTubeMusic => {
                *self.youtube.lock().await = None;
                let configs = config::get_config();
                let active_configs =
                    config::Configs::new(&configs.config_folder, &configs.cache_folder)
                        .context("reload YouTube account configuration")?;
                let library = check_youtube_auth_with_configs(&active_configs).await?;
                state.data.write().user_data.youtube_library = library;
                state.ui.lock().youtube_auth_status = active_configs.youtube_music_auth_status();
            }
        }
        Ok(())
    }

    async fn prepare_account_change(
        &self,
        state: &SharedState,
        provider: config::ActiveProvider,
    ) -> Result<()> {
        // Invalidate keyed selections before any provider data is cleared. A
        // failed switch still leaves the old selection scope unusable.
        let selection_invalidated = state.ui.lock().bump_provider_selection_epoch(provider);
        anyhow::ensure!(
            selection_invalidated,
            "provider selection epoch is exhausted"
        );

        let spotify = playback_coordinator::SpotifyEngineAdapter::new(self, state);
        let youtube = playback_coordinator::YouTubeEngineAdapter::new(self, state);
        let sessions = playback_coordinator::AppPlaybackSessions::new(state);
        self.playback
            .stop_for_account_change(&spotify, &youtube, &sessions)
            .await?;
        match provider {
            config::ActiveProvider::Spotify => {
                #[cfg(feature = "streaming")]
                self.shutdown_streaming_connection()?;
            }
            config::ActiveProvider::YouTubeMusic => {
                youtube::browser_auth::shutdown_playback_browser().await;
                *self.youtube.lock().await = None;
            }
        }
        self.clear_account_data(state, provider);
        Ok(())
    }

    fn clear_account_data(&self, state: &SharedState, provider: config::ActiveProvider) {
        // Search caches are cleared together, so invalidate every retained
        // Search projection before removing their data. The query and page
        // stay in history for a scoped reload when the page is active again.
        state.ui.lock().invalidate_search_lifecycles();
        let mut data = state.data.write();
        let projection_provider = match provider {
            config::ActiveProvider::Spotify => crate::state::Provider::Spotify,
            config::ActiveProvider::YouTubeMusic => crate::state::Provider::YouTubeMusic,
        };
        if let Err(error) = data.detach_provider_projections(projection_provider) {
            tracing::warn!(
                ?error,
                ?provider,
                "unable to persist detached playlist projections"
            );
        }
        data.caches = MemoryCaches::new();
        match provider {
            config::ActiveProvider::Spotify => {
                data.user_data.user = None;
                data.user_data.playlists.clear();
                data.user_data.playlist_folder_node = None;
                data.user_data.followed_artists.clear();
                data.user_data.saved_shows.clear();
                data.user_data.saved_albums.clear();
                data.user_data.saved_tracks.clear();
            }
            config::ActiveProvider::YouTubeMusic => {
                data.user_data.youtube_library = crate::state::YouTubeLibrary::default();
            }
        }
    }

    /// Delete every saved preference and account file, then reopen first-use
    /// setup from fresh in-memory state. Files are removed before playback is
    /// stopped and memory is cleared so a deletion failure leaves the running
    /// session consistent and reportable from Settings.
    async fn reset_all_configuration(&self, state: &SharedState) -> Result<()> {
        let (config_folder, cache_folder, cookie_path) = account_paths();
        config::reset_all_configuration(&config_folder, &cache_folder, &cookie_path)
            .context("delete saved configuration and account files")?;
        #[cfg(feature = "streaming")]
        self.shutdown_streaming_connection()?;
        youtube::browser_auth::shutdown_playback_browser().await;
        self.clear_account_data(state, config::ActiveProvider::Spotify);
        self.clear_account_data(state, config::ActiveProvider::YouTubeMusic);
        {
            let mut data = state.data.write();
            data.journal = crate::state::TrackJournal::default();
            data.session_history = crate::state::SessionHistory::default();
            data.context_history = crate::state::ContextHistory::default();
        }
        {
            let mut player = state.player.write();
            player.buffered_playback = None;
            player.youtube_playback = None;
            player.youtube_playback_phase = crate::state::YouTubePlaybackPhase::Paused;
            player.active_playback_account_provider = None;
            player.active_playback_account_label = None;
        }
        {
            let mut ui = state.ui.lock();
            ui.setup_state = config::SetupState::default();
            ui.spotify_auth_status = config::SpotifyAuthSnapshot::default();
            ui.youtube_auth_status = config::YouTubeMusicAuthStatus::default();
            ui.spotify_account_label = None;
            ui.youtube_account_label = None;
            ui.youtube_account_id = None;
            ui.spotify_account_id = None;
            auth::NCSPOT_CLIENT_ID.clone_into(&mut ui.welcome_spotify_client_id);
            ui.welcome_spotify_client_command = false;
            ui.welcome_spotify_client_pending = false;
            ui.welcome_spotify_notice = None;
            ui.welcome_spotify_web_token_cached = false;
            ui.welcome_spotify_library_tested = None;
            ui.welcome_spotify_playback_tested = None;
            ui.welcome_spotify_auth_in_flight = false;
            ui.welcome_spotify_operation = crate::state::WelcomeOperation::Idle;
            ui.welcome_youtube_notice = None;
            ui.welcome_youtube_operation = crate::state::WelcomeOperation::Idle;
            ui.welcome_youtube_account_tested = None;
            ui.welcome_youtube_playback_tested = None;
            ui.session_history_selection.clear();
            ui.popup = None;
            ui.open_setup_page(true);
            let reference = ui.start_operation(
                crate::state::UiOperationKind::ProviderCommand,
                crate::state::SETTINGS_ACTION_COMPLETED_CODE,
                "Configuration reset; first-use setup is open.",
            );
            ui.complete_operation(
                &reference,
                crate::state::UiOperationState::Completed,
                crate::state::SETTINGS_ACTION_COMPLETED_CODE,
                "All saved preferences, accounts, journal, and history were deleted.",
                None,
            );
        }
        Ok(())
    }

    fn refresh_account_ui_status(&self, state: &SharedState, provider: config::ActiveProvider) {
        let configs = config::get_config();
        let refreshed_settings = config::app_config_settings(&configs.config_folder).ok();
        let mut ui = state.ui.lock();
        match provider {
            config::ActiveProvider::Spotify => {
                ui.spotify_auth_status = merge_cached_spotify_auth_snapshot(
                    auth::cached_spotify_auth_snapshot(configs),
                    ui.spotify_auth_status,
                );
                let registry = config::AccountRegistry::load(&configs.config_folder).ok();
                ui.spotify_account_label = registry.as_ref().and_then(|registry| {
                    registry
                        .active_label(config::ActiveProvider::Spotify)
                        .map(str::to_owned)
                });
                ui.spotify_account_id = registry.as_ref().and_then(|registry| {
                    registry
                        .active_id(config::ActiveProvider::Spotify)
                        .map(str::to_owned)
                });
            }
            config::ActiveProvider::YouTubeMusic => {
                let mut status = configs.youtube_music_auth_status();
                let registry = config::AccountRegistry::load(&configs.config_folder).ok();
                if let Some(registry) = registry.as_ref() {
                    let ready = registry
                        .summaries(
                            config::ActiveProvider::YouTubeMusic,
                            &configs.config_folder,
                            &configs.cache_folder,
                            &configs.youtube_music_cookie_path(),
                        )
                        .into_iter()
                        .any(|account| account.active && account.ready);
                    if ready {
                        status.auth_type = config::YouTubeMusicAuthType::Browser;
                        status.credential_path = Some(configs.youtube_music_cookie_path());
                        status.ready = true;
                    }
                }
                ui.youtube_auth_status = status;
                ui.youtube_account_id = registry.as_ref().and_then(|registry| {
                    registry
                        .active_id(config::ActiveProvider::YouTubeMusic)
                        .map(str::to_owned)
                });
                ui.youtube_account_label = registry.as_ref().and_then(|registry| {
                    registry
                        .active_label(config::ActiveProvider::YouTubeMusic)
                        .map(str::to_owned)
                });
            }
        }
        if let Some(settings) = refreshed_settings {
            let selected_key = match ui.current_page() {
                crate::state::PageState::Settings { settings, list, .. } => list
                    .selected()
                    .and_then(|index| settings.get(index))
                    .map(|setting| setting.key.clone()),
                _ => None,
            };
            if let crate::state::PageState::Settings {
                settings: current,
                list,
                ..
            } = ui.current_page_mut()
            {
                *current = settings;
                let index = selected_key
                    .and_then(|key| current.iter().position(|setting| setting.key == key))
                    .unwrap_or_else(|| {
                        list.selected()
                            .unwrap_or_default()
                            .min(current.len().saturating_sub(1))
                    });
                list.select((!current.is_empty()).then_some(index));
            }
        }
    }

    fn refresh_current_account_snapshot(
        &self,
        state: &SharedState,
        provider: config::ActiveProvider,
    ) -> Result<config::AccountRecord> {
        let (config_folder, cache_folder, cookie_path) = account_paths();
        let label = account_snapshot_label(state, provider);
        let mut registry = config::AccountRegistry::load(&config_folder)?;
        registry.refresh_or_register_current(
            provider,
            label.as_deref(),
            &config_folder,
            &cache_folder,
            &cookie_path,
        )
    }
}

fn account_snapshot_label(state: &SharedState, provider: config::ActiveProvider) -> Option<String> {
    // The shared Spotify user projection is not a YouTube identity. The
    // YouTube API path currently has no profile-name projection, so let the
    // account registry create its provider-specific fallback instead of
    // leaking Spotify's display name into a YouTube account slot.
    match provider {
        config::ActiveProvider::Spotify => state.data.read().user_data.user.as_ref().map(|user| {
            user.display_name
                .clone()
                .unwrap_or_else(|| user.id.id().to_string())
        }),
        config::ActiveProvider::YouTubeMusic => None,
    }
}

fn account_paths() -> (std::path::PathBuf, std::path::PathBuf, std::path::PathBuf) {
    let configs = config::get_config();
    (
        configs.config_folder.clone(),
        configs.cache_folder.clone(),
        configs.youtube_music_cookie_path(),
    )
}

fn account_status_key(provider: config::ActiveProvider) -> &'static str {
    match provider {
        config::ActiveProvider::Spotify => "accounts.spotify.status",
        config::ActiveProvider::YouTubeMusic => "accounts.youtube_music.status",
    }
}

fn require_active_account(
    registry: &config::AccountRegistry,
    provider: config::ActiveProvider,
    config_folder: &std::path::Path,
    cache_folder: &std::path::Path,
    cookie_path: &std::path::Path,
) -> Result<config::AccountRecord> {
    let active_id = registry
        .active_id(provider)
        .context("no active account is selected")?;
    let active = registry
        .summaries(provider, config_folder, cache_folder, cookie_path)
        .into_iter()
        .find(|account| account.id == active_id)
        .context("the active account is not registered")?;
    anyhow::ensure!(
        active.ready,
        "the active account has no complete saved session"
    );
    Ok(config::AccountRecord {
        id: active.id,
        label: active.label,
    })
}

impl AppClient {
    /// Construct a new client
    pub async fn new() -> Result<Self> {
        let client = Self::new_without_auth()?;
        let mut api_client = client.spotify_api();
        auth::prompt_for_user_token(&mut api_client, false)
            .await
            .context("authenticate Spotify Web API client")?;
        Ok(client)
    }

    /// Construct the client without opening an authentication flow.
    ///
    /// The interactive TUI uses this path so first-use setup can explain the
    /// provider requirements before it asks the user to authenticate.
    pub fn new_without_auth() -> Result<Self> {
        let configs = config::get_config();
        let auth_config = AuthConfig::new(configs)?;
        let http = reqwest::Client::builder()
            .build()
            .context("build application HTTP client")?;
        let youtube_audio_resolver = Arc::new(youtube::playback::InnertubeAudioResolver::new(
            configs,
            http.clone(),
        ));

        let spotify_api_epoch = Arc::new(tokio::sync::RwLock::new(0));
        let api_client = Arc::new(parking_lot::RwLock::new(
            new_api_client()?.with_cache_owner(spotify_api_epoch.clone(), 0),
        ));
        let (playback_completion_tx, playback_completion_rx) = flume::unbounded();

        Ok(Self {
            spotify: Arc::new(spotify::Spotify::new()),
            youtube: Arc::new(tokio::sync::Mutex::new(None)),
            youtube_browser_login: Arc::new(tokio::sync::Mutex::new(
                YouTubeBrowserLoginControl::default(),
            )),
            account_operation_lock: Arc::new(tokio::sync::Mutex::new(())),
            spotify_auth_lock: Arc::new(tokio::sync::Mutex::new(())),
            youtube_player: Arc::new(tokio::sync::Mutex::new(youtube::YouTubeLocalPlayer::new(
                configs.app_config.device.volume,
            ))),
            youtube_audio_resolver,
            youtube_audio_quality: configs.app_config.youtube.playback_quality,
            youtube_audio_cache_size_bytes: configs
                .app_config
                .youtube
                .native_audio_cache_size_mb
                .clamp(4, 128)
                * 1024
                * 1024,
            youtube_prefetch: Arc::new(tokio::sync::Mutex::new(None)),
            youtube_expiry_recovery: Arc::new(StdMutex::new(None)),
            youtube_queue_completion: Arc::new(StdMutex::new(None)),
            spotify_queue_completion: Arc::new(StdMutex::new(None)),
            http,
            auth_config,
            api_client,
            spotify_api_epoch,
            spotify_api_auth_lock: Arc::new(tokio::sync::Mutex::new(())),
            volume_requests: Arc::new(StdMutex::new(playback_state::VolumeRequestState::default())),
            state_application: state_application::StateApplicationService,
            playback: playback_coordinator::PlaybackCoordinator::new(
                configs.app_config.active_provider,
            ),
            playback_completion_tx,
            playback_completion_rx,
            spotify_rate_limit_until: Arc::new(StdMutex::new(None)),
            #[cfg(feature = "private-capture")]
            developer_capture: Arc::new(StdMutex::new(None)),

            #[cfg(feature = "streaming")]
            stream_conn: Arc::new(Mutex::new(None)),
        })
    }

    async fn replace_spotify_api(&self, replacement: auth::SpotifyWebApiClient) {
        let mut epoch = self.spotify_api_epoch.write().await;
        *epoch += 1;
        *self.api_client.write() =
            replacement.with_cache_owner(self.spotify_api_epoch.clone(), *epoch);
        *self.spotify_rate_limit_until.lock().unwrap() = None;
    }

    async fn apply_saved_spotify_client(
        &self,
        config_folder: &std::path::Path,
    ) -> Result<auth::SpotifyWebApiClient> {
        let saved = config::AppConfig::new(config_folder)?;
        let id = saved.get_client_id()?;
        let mut setup = config::SetupState::load(config_folder)?;
        if self.spotify_api().get_creds().id != id || setup.spotify_reauthentication_required {
            setup.spotify_reauthentication_required = true;
            setup.save(config_folder)?;
            let replacement = new_api_client_for_id(id)?;
            self.replace_spotify_api(replacement).await;
        }
        Ok(self.spotify_api())
    }

    pub async fn initialize_existing_session(&self, state: &SharedState) -> Result<()> {
        let _auth_guard = self.spotify_api_auth_lock.lock().await;
        let mut api_client = self
            .apply_saved_spotify_client(&config::get_config().config_folder)
            .await?;
        anyhow::ensure!(
            !config::SetupState::load(&config::get_config().config_folder)?
                .spotify_reauthentication_required,
            "Client changed. Use Apply & sign in in Welcome."
        );
        auth::prompt_for_user_token_with_interaction(
            &mut api_client,
            false,
            auth::AuthInteraction::NonInteractive,
        )
        .await
        .map_err(|error| annotate_startup_error(error, StartupFailurePhase::WebApiToken))
        .context("reuse Spotify Web API session")?;
        let mut session_error = None;
        for attempt in 0..EXISTING_SESSION_ATTEMPTS {
            match self.new_session(Some(state), false).await {
                Ok(()) => {
                    session_error = None;
                    break;
                }
                Err(error) => {
                    session_error = Some(annotate_startup_error(
                        error,
                        StartupFailurePhase::IntegratedSession,
                    ));
                    #[cfg(feature = "streaming")]
                    let _ = self.shutdown_streaming_connection();
                    if attempt + 1 < EXISTING_SESSION_ATTEMPTS {
                        tracing::warn!(
                            attempt = attempt + 1,
                            "Existing Spotify session startup failed; retrying once"
                        );
                        tokio::time::sleep(EXISTING_SESSION_RETRY_DELAY).await;
                    }
                }
            }
        }
        if let Some(error) = session_error {
            state
                .ui
                .lock()
                .mark_setup_failed(config::SetupFailure::SpotifySessionUnavailable);
            return Err(error).context("reuse integrated Spotify session");
        }
        let user = self.spotify_api().current_user().await.map_err(|error| {
            annotate_startup_error(error.into(), StartupFailurePhase::CurrentUser)
        })?;
        state.data.write().user_data.user = Some(user);
        // Account bootstrap restores the active slot into the canonical cache on
        // every launch. Keep that slot in sync after reusing, refreshing, or
        // reauthorizing the Web API token, otherwise the next launch can restore
        // the stale token and prompt for OAuth again.
        self.refresh_current_account_snapshot(state, config::ActiveProvider::Spotify)
            .map_err(|error| annotate_startup_error(error, StartupFailurePhase::AccountSnapshot))
            .context("persist active Spotify account session")?;
        self.state_application
            .update_spotify_auth_status(state, Some(config::SpotifyPremiumStatus::Premium));
        self.retrieve_current_playback(state, true)
            .await
            .map_err(|error| annotate_startup_error(error, StartupFailurePhase::PlaybackProbe))?;
        Ok(())
    }

    async fn authenticate_spotify(
        &self,
        state: &SharedState,
    ) -> Result<SpotifyAuthenticationOutcome> {
        let _auth_guard = self.spotify_api_auth_lock.lock().await;
        let mut api_client = self
            .apply_saved_spotify_client(&config::get_config().config_folder)
            .await?;
        auth::prompt_for_user_token_with_interaction(
            &mut api_client,
            true,
            auth::AuthInteraction::NonInteractive,
        )
        .await
        .map_err(|error| annotate_startup_error(error, StartupFailurePhase::WebApiToken))
        .context("authenticate Spotify Web API client")?;
        {
            let mut ui = state.ui.lock();
            ui.setup_state.spotify_reauthentication_required = false;
            ui.welcome_spotify_client_pending = false;
            ui.welcome_spotify_client_id
                .clone_from(&api_client.get_creds().id);
            ui.welcome_spotify_web_token_cached = true;
            ui.welcome_spotify_notice = Some(
                "Web API authorized; connecting the integrated playback session...".to_owned(),
            );
        }
        if let Err(error) = self.new_session(Some(state), true).await {
            state
                .ui
                .lock()
                .mark_setup_failed(config::SetupFailure::SpotifySessionUnavailable);
            return Err(annotate_startup_error(
                error,
                StartupFailurePhase::IntegratedSession,
            ))
            .context("authenticate integrated Spotify session");
        }

        let mut outcome = SpotifyAuthenticationOutcome::Ready;
        match self.spotify_api().current_user().await {
            Ok(user) => state.data.write().user_data.user = Some(user),
            Err(error) => {
                let retry_after = self.defer_spotify_rate_limit_error(&error);
                let error = annotate_startup_error(error.into(), StartupFailurePhase::CurrentUser);
                let failure = startup_failure_metadata(&error);
                if !spotify_auth_bootstrap_can_degrade(failure) {
                    return Err(error).context("load authenticated Spotify profile");
                }
                let retry_after =
                    retry_after.unwrap_or(super::provider_metadata::DEFAULT_RATE_LIMIT_DELAY);
                log_spotify_auth_bootstrap_degraded(&error, failure, retry_after);
                outcome = SpotifyAuthenticationOutcome::WebApiRateLimited { retry_after };
            }
        }
        self.state_application
            .update_spotify_auth_status(state, Some(config::SpotifyPremiumStatus::Premium));
        if matches!(outcome, SpotifyAuthenticationOutcome::Ready) {
            if let Err(error) = self.retrieve_current_playback(state, true).await {
                let error = annotate_startup_error(error, StartupFailurePhase::PlaybackProbe);
                let failure = startup_failure_metadata(&error);
                if !spotify_auth_bootstrap_can_degrade(failure) {
                    return Err(error).context("load authenticated Spotify playback");
                }
                let retry_after = self
                    .spotify_rate_limit_remaining()
                    .unwrap_or(super::provider_metadata::DEFAULT_RATE_LIMIT_DELAY);
                log_spotify_auth_bootstrap_degraded(&error, failure, retry_after);
                outcome = SpotifyAuthenticationOutcome::WebApiRateLimited { retry_after };
            }
        }
        Ok(outcome)
    }

    pub(super) async fn token(&self) -> Result<String> {
        let api_client = self.spotify_api();
        api_client.auto_reauth().await?;
        Ok(api_client
            .get_token()
            .lock()
            .await
            .unwrap()
            .as_ref()
            .context("no access token")?
            .access_token
            .clone())
    }

    /// Initialize the application's playback upon creating a new session or during startup.
    ///
    /// `resume` controls whether playback should be (re)started on the device we connect to.
    pub fn initialize_playback(&self, state: &SharedState, resume: bool) {
        tokio::task::spawn({
            let client = self.clone();
            let state = state.clone();
            async move {
                // The main playback initialization logic is simple:
                // if there is no playback, connect to an available device
                //
                // However, because it takes time for Spotify server to show up new changes,
                // a retry logic is implemented to ensure the application's state is properly initialized
                let delay = std::time::Duration::from_secs(1);

                for _ in 0..5 {
                    tokio::time::sleep(delay).await;

                    if let Err(err) = client.retrieve_current_playback(&state, false).await {
                        crate::observability::log_safe_error!(
                            error,
                            crate::observability::DiagnosticCode::SPOTIFY_PLAYBACK_REFRESH_FAILED,
                            crate::observability::ErrorCategory::Unavailable,
                            &err,
                            "Failed to retrieve current Spotify playback"
                        );
                        return;
                    }

                    // if playback exists, don't connect to a new device
                    if state.player.read().playback.is_some() {
                        continue;
                    }

                    let id = match client.find_available_device().await {
                        Ok(Some(id)) => Some(Cow::Owned(id)),
                        Ok(None) => None,
                        Err(err) => {
                            crate::observability::log_safe_error!(
                                error,
                                crate::observability::DiagnosticCode::SPOTIFY_DEVICE_DISCOVERY_FAILED,
                                crate::observability::ErrorCategory::Unavailable,
                                &err,
                                "Failed to find an available Spotify device"
                            );
                            None
                        }
                    };

                    if let Some(id) = id {
                        tracing::info!(resume, "Trying to connect to a Spotify device");
                        if let Err(err) = client
                            .spotify_api()
                            .transfer_playback(&id, Some(false))
                            .await
                        {
                            crate::observability::log_safe_error!(
                                warn,
                                crate::observability::DiagnosticCode::SPOTIFY_DEVICE_TRANSFER_FAILED,
                                crate::observability::ErrorCategory::Unavailable,
                                &err,
                                "Spotify device connection failed"
                            );
                        } else {
                            tracing::info!("Spotify device connection succeeded");
                            if resume {
                                if let Err(err) = client
                                    .spotify_api()
                                    .resume_playback(Some(id.as_ref()), None)
                                    .await
                                {
                                    crate::observability::log_safe_error!(
                                        warn,
                                        crate::observability::DiagnosticCode::SPOTIFY_PLAYBACK_RESUME_FAILED,
                                        crate::observability::ErrorCategory::Unavailable,
                                        &err,
                                        "Failed to resume playback after reconnect"
                                    );
                                }
                            }
                            // upon new connection, reset the buffered playback
                            state.player.write().buffered_playback = None;
                            client.update_playback(&state);
                            break;
                        }
                    }
                }
            }
        });
    }

    /// Create a new client session
    pub async fn new_session(&self, state: Option<&SharedState>, reauth: bool) -> Result<()> {
        // Capture whether playback was active *before* tearing down any existing streaming
        // connection. Shutting down the old `librespot` spirc pauses playback Spotify-side
        // (and a broken session leaves it paused too), so we use this to resume on the new
        // device rather than reconnecting in a paused state.
        let was_playing = state.is_some_and(|state| {
            state
                .player
                .read()
                .buffered_playback
                .as_ref()
                .is_some_and(|p| p.is_playing)
        });

        let session = self.auth_config.session();
        let creds = auth::get_creds_with_interaction(
            &self.auth_config,
            reauth,
            true,
            auth::AuthInteraction::NonInteractive,
        )
        .context("get credentials")?;
        self.spotify.set_session(session.clone()).await;

        #[allow(unused_mut)]
        let mut connected = false;

        #[cfg(feature = "streaming")]
        if let Some(state) = state {
            if state.is_streaming_enabled() {
                tokio::time::timeout(
                    SPOTIFY_STREAM_START_TIMEOUT,
                    self.new_streaming_connection(state.clone(), session.clone(), creds.clone()),
                )
                .await
                .context("new streaming connection timed out")?
                .context("new streaming connection")?;
                connected = true;
            }
        }

        if !connected {
            // if session is not connected (triggered by `new_streaming_connection`), connect to the session
            session
                .connect(creds, true)
                .await
                .context("connect to a session")?;
        }

        tracing::info!("Used a new session for Spotify client.");

        if let Some(state) = state {
            // reset the application's caches
            state.data.write().caches = MemoryCaches::new();
            self.initialize_playback(state, was_playing);
        }

        Ok(())
    }

    /// Check if the current session is valid and if invalid, create a new session
    pub async fn check_valid_session(&self, state: &SharedState) -> Result<()> {
        let Some(session) = self.spotify.session_if_present().await else {
            // First-use setup intentionally constructs the client before a Spotify
            // session exists. The watcher remains alive so it can observe a later
            // Welcome-page authentication without turning that expected state into
            // a critical worker panic.
            return Ok(());
        };

        if session.is_invalid() {
            tracing::info!("Client's current session is invalid, creating a new session...");
            self.new_session(Some(state), false)
                .await
                .context("create new client session")?;
        }
        Ok(())
    }

    /// Create a new streaming connection
    #[cfg(feature = "streaming")]
    pub async fn new_streaming_connection(
        &self,
        state: SharedState,
        session: librespot_core::Session,
        creds: librespot_core::authentication::Credentials,
    ) -> Result<()> {
        let device_id = session.device_id().to_owned();
        let new_conn =
            crate::streaming::new_connection(self.clone(), state.clone(), session, creds).await?;
        self.shutdown_streaming_connection()?;
        *self.stream_conn.lock() = Some(new_conn);
        state.player.write().integrated_device_id = Some(device_id);
        Ok(())
    }

    /// Pause the integrated streaming client, if a connection exists.
    ///
    /// Returns `true` if a streaming connection was present and the pause
    /// command was issued. Used to suppress Spotify's auto-resume of the
    /// previous session on startup when `pause_on_startup` is enabled.
    #[cfg(feature = "streaming")]
    pub fn pause_streaming_on_startup(&self) -> bool {
        match self.stream_conn.lock().as_ref() {
            Some(spirc) => {
                if let Err(err) = spirc.pause() {
                    crate::observability::log_safe_error!(
                        warn,
                        crate::observability::DiagnosticCode::SPOTIFY_STARTUP_PAUSE_FAILED,
                        crate::observability::ErrorCategory::Unavailable,
                        &err,
                        "Failed to pause the integrated Spotify client on startup"
                    );
                }
                true
            }
            None => false,
        }
    }

    #[cfg(feature = "streaming")]
    pub(super) fn shutdown_streaming_connection(&self) -> Result<()> {
        if let Some(connection) = self.stream_conn.lock().take() {
            if let Err(err) = connection.shutdown() {
                crate::observability::log_safe_error!(
                    warn,
                    crate::observability::DiagnosticCode::SPOTIFY_STREAM_SHUTDOWN_FAILED,
                    crate::observability::ErrorCategory::Resource,
                    &err,
                    "Integrated Spotify connection was already stopped; continuing shutdown"
                );
            }
        }
        Ok(())
    }
}

impl AppClient {
    async fn hydrate_listenbrainz_spotify_items(
        &self,
        items: &mut [crate::state::UnifiedPlaylistItem],
    ) -> usize {
        let observed_at = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |duration| duration.as_secs());
        for item in items.iter_mut().filter(|item| {
            item.media_id.provider == crate::state::Provider::Spotify
                && item.media_id.kind == crate::state::MediaKind::Track
                && (item.metadata.degraded
                    || item.metadata.metadata_pending
                    || item.duration_ms.is_none())
        }) {
            let Ok(track_id) = rspotify::model::TrackId::from_id(&item.media_id.raw_id) else {
                continue;
            };
            match self.track(track_id).await {
                Ok(track) => {
                    let artists = track.artists_info();
                    let duration_ms = track.duration.as_millis() as u64;
                    let provider_url = track.id.uri();
                    item.title = track.name;
                    item.artists = artists;
                    item.duration_ms = Some(duration_ms);
                    item.provider_url = Some(provider_url);
                    item.metadata.provenance = Some("listenbrainz-spotify-api".to_owned());
                    item.metadata.observed_at = Some(observed_at);
                    item.metadata.degraded = false;
                    item.metadata.metadata_pending = false;
                }
                Err(error) => {
                    tracing::debug!(
                        error = ?error,
                        raw_id = %item.media_id.raw_id,
                        "ListenBrainz Spotify metadata hydration failed"
                    );
                }
            }
        }
        items
            .iter()
            .filter(|item| item.metadata.degraded || item.metadata.metadata_pending)
            .count()
    }

    pub(super) async fn handle_auth_session_request(
        &self,
        state: &SharedState,
        request: ClientRequest,
    ) -> Result<()> {
        match request {
            ClientRequest::ListListenBrainzPlaylists {
                operation,
                identity,
            } => {
                if !finish_listenbrainz_catalog_ownership(
                    &mut state.ui.lock(),
                    config::get_config(),
                    operation,
                    &identity,
                ) {
                    return Ok(());
                }
                let result = super::listenbrainz::user_playlists(&identity).await;
                let mut ui = state.ui.lock();
                if !finish_listenbrainz_catalog_ownership(
                    &mut ui,
                    config::get_config(),
                    operation,
                    &identity,
                ) {
                    return Ok(());
                }
                let result = result.map(|mut rows| {
                    let data = state.data.read();
                    for row in &mut rows {
                        row.imported = data
                            .playlist_links
                            .iter()
                            .any(|link| link.listenbrainz_playlist_id.as_deref() == Some(&row.id));
                    }
                    rows
                });
                // The rows change under the picker; drop last frame's row geometry.
                ui.workspace_popup_hits.clear();
                if let Some(crate::state::PopupState::ListenBrainzPlaylists {
                    rows,
                    busy,
                    notice,
                    state: list,
                    ..
                }) = &mut ui.popup
                {
                    *busy = false;
                    match result {
                        Ok(fetched) => {
                            *rows = fetched;
                            list.select(Some(0));
                            let message = if rows.is_empty() {
                                "No playlists found. Refresh or Cancel."
                            } else {
                                "Choose a playlist to import locally. No remote changes."
                            };
                            message.clone_into(notice);
                        }
                        Err(error) => {
                            *notice = error.to_string();
                        }
                    }
                }
            }
            ClientRequest::ImportListenBrainzPlaylist {
                operation,
                identity,
                playlist_id,
            } => {
                if !finish_listenbrainz_catalog_ownership(
                    &mut state.ui.lock(),
                    config::get_config(),
                    operation,
                    &identity,
                ) {
                    return Ok(());
                }
                let read_result = super::listenbrainz::read_playlist(&identity, &playlist_id).await;
                let result = match read_result {
                    Err(error) => Err(error),
                    Ok((name, mut items, _)) => {
                        {
                            let mut ui = state.ui.lock();
                            if !finish_listenbrainz_catalog_ownership(
                                &mut ui,
                                config::get_config(),
                                operation,
                                &identity,
                            ) {
                                return Ok(());
                            }
                        }
                        let unresolved = self.hydrate_listenbrainz_spotify_items(&mut items).await;
                        let count = items.len();
                        let import_result = {
                            let mut data = state.data.write();
                            anyhow::ensure!(
                                config::get_config().listenbrainz_token().as_deref()
                                    == Some(identity.token.expose()),
                                "Token changed. Validate it again before importing."
                            );
                            super::listenbrainz_import::import_local_playlist(
                                &mut data,
                                &playlist_id,
                                name,
                                items,
                            )
                        };
                        import_result.map(|()| (count, unresolved))
                    }
                };
                let mut ui = state.ui.lock();
                if !finish_listenbrainz_catalog_ownership(
                    &mut ui,
                    config::get_config(),
                    operation,
                    &identity,
                ) {
                    return Ok(());
                }
                // The rows change under the picker; drop last frame's row geometry.
                ui.workspace_popup_hits.clear();
                if let Some(crate::state::PopupState::ListenBrainzPlaylists {
                    rows,
                    busy,
                    notice,
                    ..
                }) = &mut ui.popup
                {
                    *busy = false;
                    match result {
                        Ok((count, unresolved)) => {
                            if let Some(row) = rows.iter_mut().find(|row| row.id == playlist_id) {
                                row.imported = true;
                                row.item_count = Some(count);
                            }
                            *notice =
                                format!("Imported {count} items locally; {unresolved} unresolved.");
                        }
                        Err(error) => {
                            *notice = error.to_string();
                        }
                    }
                }
            }
            ClientRequest::ValidateListenBrainzToken {
                attempt,
                token,
                save,
            } => {
                if state.ui.lock().welcome_listenbrainz_pending != Some(attempt) {
                    return Ok(());
                }
                let result = super::listenbrainz::validate_token(&token).await;
                let mut ui = state.ui.lock();
                finish_listenbrainz_token(
                    &mut ui,
                    config::get_config(),
                    attempt,
                    &token,
                    save,
                    result,
                );
            }
            ClientRequest::InitializeSpotifySession => {
                let Ok(_spotify_auth_guard) = self.spotify_auth_lock.try_lock() else {
                    self.state_application.update_settings_action_status(
                        state,
                        "spotify.auth",
                        Ok(crate::state::WELCOME_SPOTIFY_AUTH_IN_FLIGHT_NOTICE.to_owned()),
                    );
                    // This request ends here without running; release the
                    // dispatch slot so a later retry can proceed.
                    state
                        .ui
                        .lock()
                        .finish_welcome_spotify_action(crate::state::WelcomeOperation::Waiting);
                    crate::observability::log_safe_error!(
                        warn,
                        crate::observability::DiagnosticCode::REQUEST_HANDLE_FAILED,
                        crate::observability::ErrorCategory::Unavailable,
                        &anyhow::anyhow!(
                            "Spotify session request overlapped an in-flight authentication"
                        ),
                        "Skipped overlapping Spotify session request"
                    );
                    return Ok(());
                };
                match self.initialize_existing_session(state).await {
                    Ok(()) => {
                        {
                            let mut ui = state.ui.lock();
                            ui.finish_welcome_spotify_action(
                                crate::state::WelcomeOperation::Succeeded,
                            );
                            ui.welcome_spotify_web_token_cached = true;
                            ui.welcome_spotify_library_tested = Some(true);
                            ui.welcome_spotify_playback_tested = Some(true);
                            ui.setup_state.spotify_reauthentication_required = false;
                            ui.welcome_spotify_client_pending = false;
                            ui.welcome_spotify_notice = Some(
                                "Existing credentials checked; Spotify playback connected."
                                    .to_owned(),
                            );
                        }
                        self.state_application.update_setup_success(state);
                    }
                    Err(err) => {
                        let failure = startup_failure_metadata(&err);
                        if spotify_auth_bootstrap_can_degrade(failure) {
                            let retry_after = self
                                .spotify_rate_limit_remaining()
                                .unwrap_or(super::provider_metadata::DEFAULT_RATE_LIMIT_DELAY);
                            log_spotify_auth_bootstrap_degraded(&err, failure, retry_after);
                            {
                                let mut ui = state.ui.lock();
                                ui.finish_welcome_spotify_action(
                                    crate::state::WelcomeOperation::RateLimited,
                                );
                                ui.welcome_spotify_web_token_cached = true;
                                ui.welcome_spotify_library_tested = None;
                                ui.welcome_spotify_playback_tested = None;
                                ui.welcome_spotify_notice = Some(format!(
                                    "Spotify playback session connected, but the Web API is rate-limited (retry after about {}s). Wait a little, then use Check again.",
                                    retry_after.as_secs().max(1)
                                ));
                            }
                            self.state_application.update_spotify_auth_status(
                                state,
                                Some(config::SpotifyPremiumStatus::Premium),
                            );
                            self.state_application.update_setup_success(state);
                        } else {
                            {
                                let mut ui = state.ui.lock();
                                ui.finish_welcome_spotify_action(
                                    crate::state::WelcomeOperation::Failed,
                                );
                                ui.welcome_spotify_library_tested = Some(false);
                                ui.welcome_spotify_playback_tested = Some(false);
                                ui.welcome_spotify_notice = Some(spotify_failure_notice(&err));
                            }
                            crate::observability::log_safe_error!(
                                error,
                                crate::observability::DiagnosticCode::SPOTIFY_AUTH_FAILED,
                                crate::observability::ErrorCategory::Authentication,
                                &err,
                                "Existing Spotify session could not be initialized"
                            );
                            let failure = state
                                .ui
                                .lock()
                                .setup_state
                                .failure
                                .unwrap_or(config::SetupFailure::AuthenticationFailed);
                            self.state_application.update_setup_failure(state, failure);
                        }
                    }
                }
            }
            ClientRequest::ManageAccount(operation) => {
                self.handle_account_operation(state, operation).await?;
            }
            #[cfg(feature = "streaming")]
            ClientRequest::RestartIntegratedClient => {
                self.new_session(Some(state), false).await?;
            }
            ClientRequest::TestYouTubeAuth => match async {
                let library = self.youtube_library().await?;
                let configs = config::get_config();
                let active = config::Configs::new_without_account_bootstrap(
                    &configs.config_folder,
                    &configs.cache_folder,
                )?;
                let playback = check_youtube_playback_for_video_with_configs(
                    &active,
                    YOUTUBE_PLAYBACK_AUTH_PROBE_VIDEO_ID,
                    false,
                )
                .await;
                anyhow::Ok((library, playback))
            }
            .await
            {
                Ok((library, playback)) => {
                    let library_summary = format!(
                        "{} playlists, {} albums, {} artists",
                        library.playlists.len(),
                        library.albums.len(),
                        library.artists.len(),
                    );
                    state.data.write().user_data.youtube_library = library;
                    {
                        let mut ui = state.ui.lock();
                        ui.welcome_youtube_account_tested = Some(true);
                        ui.welcome_youtube_playback_tested = Some(playback.is_ok());
                        ui.finish_welcome_youtube_action(crate::state::WelcomeOperation::Succeeded);
                    }
                    match playback {
                        Ok(playback) => self.state_application.update_settings_action_status(
                            state,
                            "youtube.auth.test",
                            Ok(format!(
                                "YouTube Music account authentication succeeded: {library_summary}; native playback validation also succeeded: {playback}"
                            )),
                        ),
                        Err(err) => {
                            let err = anyhow::anyhow!(
                                "YouTube Music account authentication succeeded ({library_summary}), but native playback validation failed: {err:#}"
                            );
                            crate::observability::log_safe_error!(
                                error,
                                crate::observability::DiagnosticCode::YOUTUBE_PLAYBACK_VALIDATION_FAILED,
                                crate::observability::ErrorCategory::Unavailable,
                                &err,
                                "YouTube Music playback validation failed after authentication"
                            );
                            // Authentication and account access succeeded; a native playback
                            // probe is a separate availability result, not a save failure.
                            self.state_application.update_settings_action_status(
                                state,
                                "youtube.auth.test",
                                Ok(format!(
                                    "YouTube Music authentication succeeded ({library_summary}); native playback validation failed. See Diagnostics."
                                )),
                            );
                        }
                    }
                    // The setup requirement is account authentication. Keep a
                    // transport-probe failure visible without misclassifying it
                    // as an authentication failure.
                    self.state_application.update_setup_success(state);
                }
                Err(err) => {
                    {
                        let mut ui = state.ui.lock();
                        ui.welcome_youtube_account_tested = Some(false);
                        ui.welcome_youtube_playback_tested = None;
                        ui.finish_welcome_youtube_action(crate::state::WelcomeOperation::Failed);
                    }
                    crate::observability::log_safe_error!(
                        error,
                        crate::observability::DiagnosticCode::YOUTUBE_AUTH_TEST_FAILED,
                        crate::observability::ErrorCategory::Authentication,
                        &err,
                        "YouTube Music authentication test failed"
                    );
                    self.state_application.update_settings_action_status(
                        state,
                        "youtube.auth.test",
                        Err(&err),
                    );
                    self.state_application
                        .update_setup_failure(state, config::SetupFailure::AuthenticationFailed);
                }
            },
            ClientRequest::CancelYouTubeAuthentication => {
                self.youtube_browser_login.lock().await.cancel_active();
            }
            ClientRequest::ImportYouTubeCookies(path) => {
                let (generation, cancellation) = {
                    let mut control = self.youtube_browser_login.lock().await;
                    if control.active.is_some() {
                        state.ui.lock().welcome_youtube_notice =
                            Some("Another sign-in is running; cancel it or wait.".to_owned());
                        return Ok(());
                    }
                    control.begin().expect("login slot checked")
                };
                let mut failure_notice = "Cannot read cookie file.".to_owned();
                let prepared = async {
                    let cookie =
                        youtube::cookie_import::read_cookie_file(&path).inspect_err(|error| {
                            failure_notice = error.to_string();
                        })?;
                    "Could not verify imported cookies. Check your connection or export a fresh signed-in session.".clone_into(&mut failure_notice);
                    let youtube = youtube::YouTubeMusic::from_cookie(cookie.clone()).await?;
                    let library = youtube.library().await?;
                    anyhow::Ok((cookie, youtube, library))
                };
                let result = tokio::select! {
                    biased;
                    () = cancellation.cancelled() => Ok(None),
                    result = tokio::time::timeout(std::time::Duration::from_secs(90), prepared) => {
                        result.context("Cookie account check timed out").and_then(|result| result).map(Some)
                    }
                };
                let result = match result {
                    Ok(Some((cookie, youtube, library))) if !cancellation.is_cancelled() => {
                        "Account verified, but saving credentials failed. Check configuration permissions and retry import.".clone_into(&mut failure_notice);
                        let configs = config::get_config();
                        let saved = (|| -> Result<()> {
                            youtube::cookie_import::persist_cookie(
                                &configs.youtube_music_cookie_path(),
                                &cookie,
                            )?;
                            config::save_app_config_override(
                                &configs.config_folder,
                                "youtube.auth_type",
                                "Browser",
                            )?;
                            Ok(())
                        })();
                        match saved {
                            Ok(()) => {
                                *self.youtube.lock().await = Some(youtube);
                                state.data.write().user_data.youtube_library = library;
                                {
                                    let mut ui = state.ui.lock();
                                    ui.youtube_auth_status = config::YouTubeMusicAuthStatus {
                                        auth_type: config::YouTubeMusicAuthType::Browser,
                                        credential_path: Some(configs.youtube_music_cookie_path()),
                                        ready: true,
                                    };
                                    ui.welcome_youtube_account_tested = Some(true);
                                    ui.welcome_youtube_playback_tested = None;
                                }
                                if self
                                    .refresh_current_account_snapshot(
                                        state,
                                        config::ActiveProvider::YouTubeMusic,
                                    )
                                    .is_err()
                                {
                                    "Sign-in saved and verified; account snapshot could not be saved. Check configuration permissions and retry.".clone_into(&mut failure_notice);
                                    Err(anyhow::anyhow!("Could not save YouTube account snapshot"))
                                } else {
                                    Ok(Some(()))
                                }
                            }
                            Err(error) => Err(error),
                        }
                    }
                    Ok(_) => Ok(None),
                    Err(error) => Err(error),
                };
                self.youtube_browser_login.lock().await.finish(generation);
                let mut ui = state.ui.lock();
                match result {
                    Ok(Some(())) => {
                        ui.finish_welcome_youtube_action(crate::state::WelcomeOperation::Succeeded);
                        ui.welcome_youtube_notice = Some(
                            "Cookies saved; account verified. Playback has not been tested."
                                .to_owned(),
                        );
                        ui.mark_setup_ready_if_possible();
                    }
                    Ok(None) => {
                        ui.finish_welcome_youtube_action(crate::state::WelcomeOperation::Cancelled);
                        ui.welcome_youtube_notice = Some(
                            "Import cancelled. Saved credentials were not replaced.".to_owned(),
                        );
                    }
                    Err(_) => {
                        ui.finish_welcome_youtube_action(crate::state::WelcomeOperation::Failed);
                        ui.welcome_youtube_notice = Some(failure_notice.clone());
                        ui.welcome_youtube_account_tested = Some(false);
                        ui.mark_setup_failed(config::SetupFailure::AuthenticationFailed);
                    }
                }
            }
            ClientRequest::AuthenticateYouTubeBrowser => {
                let (generation, cancellation) = {
                    let mut control = self.youtube_browser_login.lock().await;
                    if let Some((_, cancellation)) = &control.active {
                        if cancellation.is_cancelled() {
                            self.state_application.update_settings_action_status(
                                state,
                                "youtube.auth.browser_login",
                                Ok("Cancelling YouTube browser sign-in...".to_string()),
                            );
                        } else {
                            cancellation.cancel();
                            self.state_application.update_settings_action_status(
                                state,
                                "youtube.auth.browser_login",
                                Ok("YouTube browser sign-in cancellation requested".to_string()),
                            );
                        }
                        return Ok(());
                    }
                    control.generation = control.generation.wrapping_add(1);
                    let generation = control.generation;
                    let cancellation = CancellationToken::new();
                    control.active = Some((generation, cancellation.clone()));
                    (generation, cancellation)
                };
                self.state_application.update_settings_action_status(
                    state,
                    "youtube.auth.browser_login",
                    Ok("Opening the dedicated YouTube Music sign-in browser...".to_string()),
                );

                let mut failure_notice =
                    "Browser could not start. Choose a browser path or Import cookies.";
                let result = async {
                    let configs = config::get_config();
                    let config_folder = configs.config_folder.clone();
                    youtube::browser_auth::resolve_browser_executable(&config_folder, None)?;
                    let mut session = youtube::browser_auth::begin_login_for_new_account(
                        &configs.config_folder,
                        None,
                        false,
                    )
                    .await
                    .context("open dedicated YouTube sign-in browser")?;
                    self.state_application.update_settings_action_status(
                        state,
                        "youtube.auth.browser_login",
                        Ok(
                            "Waiting for Google sign-in; activate this row again to cancel"
                            .to_string(),
                        ),
                    );
                    failure_notice = "Browser sign-in did not finish. Retry, choose another browser, or Import cookies.";
                    let cookie_path = configs.youtube_music_cookie_path();
                    let cookie_count = session
                        .wait_for_sign_in_and_save(&cookie_path, true, &cancellation)
                        .await?;
                    let Some(cookie_count) = cookie_count else {
                        return anyhow::Ok(None);
                    };
                    failure_notice = "Could not save browser sign-in. Check configuration permissions and retry.";
                    session.promote_to_active_profile(&config_folder)?;
                    drop(session);
                    self.state_application.update_settings_action_status(
                        state,
                        "youtube.auth.browser_login",
                        Ok("Saving the signed-in YouTube Music browser session...".to_string()),
                    );
                    config::save_app_config_override(
                        &configs.config_folder,
                        "youtube.auth_type",
                        "Browser",
                    )
                    .context("select YouTube browser authentication")?;
                    let active_configs = config::Configs::new_without_account_bootstrap(
                        &configs.config_folder,
                        &configs.cache_folder,
                    )
                    .context("reload YouTube browser authentication")?;
                    *self.youtube.lock().await = None;
                    state.ui.lock().youtube_auth_status =
                        active_configs.youtube_music_auth_status();
                    self.state_application.update_settings_action_status(
                        state,
                        "youtube.auth.browser_login",
                        Ok("Validating YouTube Music account access...".to_string()),
                    );
                    failure_notice = "Sign-in saved, but account access could not be verified. Check your connection and retry Check account & playback.";
                    let youtube = youtube::YouTubeMusic::new(&active_configs).await?;
                    let library = youtube.library()
                        .await
                        .context("validate YouTube Music account access")?;
                    self.state_application.update_settings_action_status(
                        state,
                        "youtube.auth.browser_login",
                        Ok("Account access succeeded; native playback validation is separate and deferred".to_string()),
                    );
                    *self.youtube.lock().await = Some(youtube);
                    anyhow::Ok(Some((cookie_count, library)))
                }
                .await;

                {
                    let mut control = self.youtube_browser_login.lock().await;
                    if control
                        .active
                        .as_ref()
                        .is_some_and(|(active_generation, _)| *active_generation == generation)
                    {
                        control.active = None;
                    }
                }

                let cancelled = matches!(&result, Ok(None));
                match result {
                    Ok(Some((cookie_count, library))) => {
                        let library_summary = format!(
                            "{} playlists, {} albums, {} artists",
                            library.playlists.len(),
                            library.albums.len(),
                            library.artists.len()
                        );
                        state.data.write().user_data.youtube_library = library;
                        if let Err(err) = self.refresh_current_account_snapshot(
                            state,
                            config::ActiveProvider::YouTubeMusic,
                        ) {
                            self.state_application.update_settings_action_status(
                                state,
                                "youtube.auth.browser_login",
                                Err(&err),
                            );
                            let mut ui = state.ui.lock();
                            ui.finish_welcome_youtube_action(
                                crate::state::WelcomeOperation::Failed,
                            );
                            ui.welcome_youtube_account_tested = Some(false);
                            ui.welcome_youtube_notice = Some("Sign-in saved, but account snapshot failed. Check configuration permissions and retry.".to_owned());
                            ui.mark_setup_failed(config::SetupFailure::PersistenceFailed);
                            return Ok(());
                        }
                        self.state_application.update_settings_action_status(state, "youtube.auth.browser_login",
                            Ok(format!("Sign-in saved ({cookie_count} cookies); account verified ({library_summary}). Playback has not been tested.")));
                        self.refresh_account_ui_status(state, config::ActiveProvider::YouTubeMusic);
                        {
                            let mut ui = state.ui.lock();
                            ui.welcome_youtube_account_tested = Some(true);
                            ui.welcome_youtube_playback_tested = None;
                            ui.welcome_youtube_notice = Some(
                                "Sign-in saved; account verified. Playback has not been tested."
                                    .to_owned(),
                            );
                            ui.finish_welcome_youtube_action(
                                crate::state::WelcomeOperation::Succeeded,
                            );
                        }
                        // Browser authentication and account access completed;
                        // native transport is a separate availability concern.
                        self.state_application.update_setup_success(state);
                    }
                    Ok(None) => {
                        state.ui.lock().finish_welcome_youtube_action(
                            crate::state::WelcomeOperation::Cancelled,
                        );
                        self.state_application.update_settings_action_status(
                            state,
                            "youtube.auth.browser_login",
                            Ok("YouTube browser sign-in cancelled".to_string()),
                        );
                    }
                    Err(err) => {
                        {
                            let mut ui = state.ui.lock();
                            ui.welcome_youtube_account_tested = Some(false);
                            ui.welcome_youtube_playback_tested = None;
                            ui.finish_welcome_youtube_action(
                                crate::state::WelcomeOperation::Failed,
                            );
                        }
                        crate::observability::log_safe_error!(
                            error,
                            crate::observability::DiagnosticCode::YOUTUBE_BROWSER_LOGIN_FAILED,
                            crate::observability::ErrorCategory::Authentication,
                            &err,
                            "YouTube browser sign-in failed"
                        );
                        self.state_application.update_settings_action_status(
                            state,
                            "youtube.auth.browser_login",
                            Err(&err),
                        );
                        state.ui.lock().welcome_youtube_notice = Some(failure_notice.to_owned());
                        self.state_application.update_setup_failure(
                            state,
                            config::SetupFailure::AuthenticationFailed,
                        );
                    }
                }
                if cancelled {
                    self.state_application.update_setup_cancelled(state);
                }
            }
            ClientRequest::ReauthenticateSpotify => {
                let Ok(_spotify_auth_guard) = self.spotify_auth_lock.try_lock() else {
                    self.state_application.update_settings_action_status(
                        state,
                        "spotify.auth",
                        Ok(crate::state::WELCOME_SPOTIFY_AUTH_IN_FLIGHT_NOTICE.to_owned()),
                    );
                    // This request ends here without running; release the
                    // dispatch slot so a later retry can proceed.
                    state
                        .ui
                        .lock()
                        .finish_welcome_spotify_action(crate::state::WelcomeOperation::Waiting);
                    crate::observability::log_safe_error!(
                        warn,
                        crate::observability::DiagnosticCode::REQUEST_HANDLE_FAILED,
                        crate::observability::ErrorCategory::Unavailable,
                        &anyhow::anyhow!(
                            "Spotify re-authentication overlapped an in-flight session request"
                        ),
                        "Skipped overlapping Spotify authentication request"
                    );
                    return Ok(());
                };
                let result = self.authenticate_spotify(state).await;
                match result {
                    Ok(authentication) => {
                        let operation = match authentication {
                            SpotifyAuthenticationOutcome::Ready => {
                                crate::state::WelcomeOperation::Succeeded
                            }
                            SpotifyAuthenticationOutcome::WebApiRateLimited { .. } => {
                                crate::state::WelcomeOperation::RateLimited
                            }
                        };
                        {
                            let mut ui = state.ui.lock();
                            ui.finish_welcome_spotify_action(operation);
                            let live_checks_succeeded =
                                matches!(authentication, SpotifyAuthenticationOutcome::Ready);
                            ui.welcome_spotify_library_tested =
                                live_checks_succeeded.then_some(true);
                            ui.welcome_spotify_playback_tested =
                                live_checks_succeeded.then_some(true);
                        }
                        match self.refresh_current_account_snapshot(
                            state,
                            config::ActiveProvider::Spotify,
                        ) {
                            Ok(account) => {
                                self.state_application.update_settings_action_status(
                                    state,
                                    "spotify.auth",
                                    Ok(authentication.completion_message(format!(
                                        "Spotify authentication succeeded for {}",
                                        account.label
                                    ))),
                                );
                                // Readiness was masked while the dispatch slot was busy.
                                // Recompute after completion so a successful sign-in does
                                // not retain the temporary missing-session warning.
                                self.state_application.update_setup_success(state);
                            }
                            Err(err) => {
                                let mut ui = state.ui.lock();
                                ui.finish_welcome_spotify_action(
                                    crate::state::WelcomeOperation::Failed,
                                );
                                ui.welcome_spotify_notice = Some("Sign-in succeeded, but saving the account failed. Check configuration/cache permissions, then Check existing session to retry saving.".to_owned());
                                ui.welcome_spotify_library_tested = Some(false);
                                ui.mark_setup_failed(config::SetupFailure::PersistenceFailed);
                                crate::observability::log_safe_error!(
                                    error,
                                    crate::observability::DiagnosticCode::SPOTIFY_AUTH_FAILED,
                                    crate::observability::ErrorCategory::Storage,
                                    &err,
                                    "Could not save authenticated Spotify account"
                                );
                            }
                        }
                        self.refresh_account_ui_status(state, config::ActiveProvider::Spotify);
                    }
                    Err(err) => {
                        {
                            let mut ui = state.ui.lock();
                            ui.finish_welcome_spotify_action(
                                crate::state::WelcomeOperation::Failed,
                            );
                            ui.welcome_spotify_library_tested = Some(false);
                            ui.welcome_spotify_playback_tested = Some(false);
                        }
                        crate::observability::log_safe_error!(
                            error,
                            crate::observability::DiagnosticCode::SPOTIFY_AUTH_FAILED,
                            crate::observability::ErrorCategory::Authentication,
                            &err,
                            "Spotify authentication failed"
                        );
                        self.state_application.update_settings_action_status(
                            state,
                            "spotify.auth",
                            Err(&err),
                        );
                        state.ui.lock().welcome_spotify_notice = Some(spotify_failure_notice(&err));
                        self.state_application.update_setup_failure(
                            state,
                            config::SetupFailure::AuthenticationFailed,
                        );
                    }
                }
            }
            ClientRequest::ResetAllConfiguration => {
                if let Err(err) = self.reset_all_configuration(state).await {
                    crate::observability::log_safe_error!(
                        error,
                        crate::observability::DiagnosticCode::REQUEST_HANDLE_FAILED,
                        crate::observability::ErrorCategory::Storage,
                        &err,
                        "Saved configuration could not be reset"
                    );
                    self.state_application.update_settings_action_status(
                        state,
                        "diagnostics.reset_all",
                        Err(&err),
                    );
                }
            }
            _ => unreachable!("request routed to the wrong auth/session handler"),
        }
        Ok(())
    }
}

pub async fn check_youtube_auth() -> Result<crate::state::YouTubeLibrary> {
    check_youtube_auth_with_configs(config::get_config()).await
}

// The field names are the serialized report keys.
#[allow(clippy::struct_field_names)]
#[derive(Debug, Serialize)]
pub struct YouTubePlaybackProbeTimings {
    resolve_ms: u64,
    media_probe_ms: u64,
    total_ms: u64,
}

#[derive(Debug, Serialize)]
pub struct YouTubePlaybackProbeReport {
    schema_version: u8,
    success: bool,
    auth_type: &'static str,
    requested_client: &'static str,
    browser_fallback_allowed: bool,
    decoder_chunk_bytes: u64,
    route: Option<&'static str>,
    client: Option<&'static str>,
    native_result: &'static str,
    browser_result: &'static str,
    mime_type: Option<String>,
    bitrate_bps: Option<u64>,
    duration_ms: Option<u64>,
    content_length: Option<u64>,
    media_probe_result: &'static str,
    error_category: Option<&'static str>,
    ejs_backend: Option<&'static str>,
    ejs_backend_configured: &'static str,
    attempts: Vec<youtube::playback::YouTubeProbeAttempt>,
    timings: YouTubePlaybackProbeTimings,
}

impl YouTubePlaybackProbeReport {
    pub fn is_success(&self) -> bool {
        self.success
    }

    pub fn error_category(&self) -> Option<&'static str> {
        self.error_category
    }
}

impl fmt::Display for YouTubePlaybackProbeReport {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        writeln!(formatter, "schema_version={}", self.schema_version)?;
        writeln!(formatter, "success={}", self.success)?;
        writeln!(formatter, "auth_type={}", self.auth_type)?;
        writeln!(formatter, "requested_client={}", self.requested_client)?;
        writeln!(
            formatter,
            "browser_fallback_allowed={}",
            self.browser_fallback_allowed
        )?;
        writeln!(
            formatter,
            "decoder_chunk_bytes={}",
            self.decoder_chunk_bytes
        )?;
        writeln!(formatter, "route={}", self.route.unwrap_or("none"))?;
        writeln!(formatter, "client={}", self.client.unwrap_or("none"))?;
        writeln!(formatter, "native_result={}", self.native_result)?;
        writeln!(formatter, "browser_result={}", self.browser_result)?;
        writeln!(
            formatter,
            "mime_type={}",
            self.mime_type.as_deref().unwrap_or("none")
        )?;
        writeln!(
            formatter,
            "bitrate_bps={}",
            self.bitrate_bps
                .map_or_else(|| "none".to_owned(), |value| value.to_string())
        )?;
        writeln!(
            formatter,
            "duration_ms={}",
            self.duration_ms
                .map_or_else(|| "none".to_owned(), |value| value.to_string())
        )?;
        writeln!(
            formatter,
            "content_length={}",
            self.content_length
                .map_or_else(|| "none".to_owned(), |value| value.to_string())
        )?;
        writeln!(formatter, "media_probe_result={}", self.media_probe_result)?;
        writeln!(
            formatter,
            "error_category={}",
            self.error_category.unwrap_or("none")
        )?;
        writeln!(
            formatter,
            "ejs_backend={}",
            self.ejs_backend.unwrap_or("not_used")
        )?;
        writeln!(
            formatter,
            "ejs_backend_configured={}",
            self.ejs_backend_configured
        )?;
        writeln!(formatter, "attempt_count={}", self.attempts.len())?;
        for (index, attempt) in self.attempts.iter().enumerate() {
            writeln!(
                formatter,
                "attempt_{index}=stage:{},client:{},result:{},status:{},error_category:{},attempt:{},target:{},duration_ms:{}",
                attempt.stage,
                attempt.client,
                attempt.result,
                attempt.status.unwrap_or("none"),
                attempt.error_category.unwrap_or("none"),
                attempt.attempt.map_or_else(|| "none".to_owned(), |value| value.to_string()),
                attempt.target.unwrap_or("none"),
                attempt.duration_ms
            )?;
        }
        writeln!(formatter, "resolve_ms={}", self.timings.resolve_ms)?;
        writeln!(formatter, "media_probe_ms={}", self.timings.media_probe_ms)?;
        write!(formatter, "total_ms={}", self.timings.total_ms)
    }
}

fn is_browser_probe_source(source_client: &str) -> bool {
    source_client.contains("BROWSER_SESSION")
}

fn youtube_probe_decoder_error_category(error: &anyhow::Error, cancelled: bool) -> &'static str {
    if cancelled {
        return "cancelled";
    }
    if let Some(error) = error
        .chain()
        .find_map(|cause| cause.downcast_ref::<rodio::decoder::DecoderError>())
    {
        return match error {
            rodio::decoder::DecoderError::UnrecognizedFormat => "decoder_unrecognized_format",
            rodio::decoder::DecoderError::IoError(_) => "decoder_io",
            rodio::decoder::DecoderError::DecodeError(_) => "decoder_malformed",
            rodio::decoder::DecoderError::LimitError(_) => "decoder_limit",
            rodio::decoder::DecoderError::ResetRequired => "decoder_reset_required",
            rodio::decoder::DecoderError::NoStreams => "decoder_no_streams",
        };
    }
    match error.to_string().as_str() {
        "open native YouTube media transport" => "transport_open",
        "initialize native YouTube stream" => "stream_initialize",
        "decode native YouTube audio stream" => "decoder_initialize",
        "join native YouTube decoder initialization" => "decoder_task",
        _ => "unknown",
    }
}

#[allow(clippy::too_many_arguments)]
fn youtube_probe_report_from_resolution(
    auth_type: config::YouTubeMusicAuthType,
    probe_client: youtube::playback::YouTubeProbeClient,
    decoder_chunk_size: youtube::playback::YouTubeProbeDecoderChunkSize,
    allow_browser_fallback: bool,
    browser_fallback_attempted: bool,
    ejs_backend: Option<&'static str>,
    ejs_backend_configured: &'static str,
    resolution: std::result::Result<
        youtube::playback::ResolvedAudioSource,
        youtube::playback::AudioSourceErrorKind,
    >,
    media_probe_succeeded: Option<bool>,
    attempts: Vec<youtube::playback::YouTubeProbeAttempt>,
    timings: YouTubePlaybackProbeTimings,
) -> YouTubePlaybackProbeReport {
    let source = resolution.as_ref().ok();
    let browser_route = source.is_some_and(|source| is_browser_probe_source(source.source_client));
    let media_probe_result = match media_probe_succeeded {
        Some(true) => "success",
        Some(false) => "failed",
        None => "not_run",
    };
    let route_result = match media_probe_succeeded {
        Some(true) => "success",
        Some(false) => "media_probe_failed",
        None => "failure",
    };
    let native_result = if browser_route {
        "failed"
    } else {
        route_result
    };
    let browser_result = if browser_route {
        route_result
    } else if browser_fallback_attempted {
        "failed"
    } else if allow_browser_fallback {
        "not_attempted"
    } else {
        "not_allowed"
    };
    let error_category = resolution
        .as_ref()
        .err()
        .map(|kind| kind.diagnostic_category().as_str())
        .or_else(|| (media_probe_succeeded == Some(false)).then_some("media_probe"));

    YouTubePlaybackProbeReport {
        schema_version: 5,
        success: source.is_some() && media_probe_succeeded == Some(true),
        auth_type: youtube_auth_type_label(auth_type),
        requested_client: probe_client.label(),
        browser_fallback_allowed: allow_browser_fallback,
        decoder_chunk_bytes: decoder_chunk_size.bytes(),
        route: source.map(|_| if browser_route { "browser" } else { "native" }),
        client: source.map(|source| source.source_client),
        native_result,
        browser_result,
        mime_type: source.map(|source| source.mime_type.clone()),
        bitrate_bps: source.map(|source| source.bitrate),
        duration_ms: source.and_then(|source| {
            source
                .duration
                .and_then(|duration| u64::try_from(duration.as_millis()).ok())
        }),
        content_length: source.and_then(|source| source.content_length),
        media_probe_result,
        error_category,
        ejs_backend,
        ejs_backend_configured,
        attempts,
        timings,
    }
}

pub async fn probe_youtube_playback_for_video(
    configs: &config::Configs,
    video_id: &str,
    allow_browser_fallback: bool,
    probe_client: youtube::playback::YouTubeProbeClient,
    decoder_chunk_size: youtube::playback::YouTubeProbeDecoderChunkSize,
) -> YouTubePlaybackProbeReport {
    let total_started = std::time::Instant::now();
    let resolver = youtube::playback::InnertubeAudioResolver::new(configs, reqwest::Client::new());
    let cancellation = CancellationToken::new();
    let resolve_started = std::time::Instant::now();
    let (resolution, browser_fallback_attempted, ejs_backend, ejs_backend_configured, mut attempts) =
        resolver
            .resolve_for_probe(
                video_id,
                configs.app_config.youtube.playback_quality,
                cancellation.clone(),
                allow_browser_fallback,
                probe_client,
            )
            .await;
    let resolve_ms = resolve_started.elapsed().as_millis() as u64;
    let media_probe_started = std::time::Instant::now();
    let media_probe_succeeded = match resolution.as_ref() {
        Ok(source) => {
            let decoder_cancellation = cancellation.clone();
            let (result, decoder_media_attempts) =
                youtube::playback::open_decoded_source_for_probe(
                    source,
                    Duration::ZERO,
                    4 * 1024 * 1024,
                    cancellation,
                    decoder_chunk_size,
                )
                .await;
            attempts.extend(decoder_media_attempts);
            let (attempt_result, error_category) = match result.as_ref() {
                Ok(_) => ("success", None),
                Err(error) => {
                    let category = youtube_probe_decoder_error_category(
                        error,
                        decoder_cancellation.is_cancelled(),
                    );
                    (
                        if category == "cancelled" {
                            "cancelled"
                        } else {
                            "error"
                        },
                        Some(category),
                    )
                }
            };
            attempts.push(youtube::playback::YouTubeProbeAttempt::decoder(
                source.source_client,
                attempt_result,
                error_category,
                media_probe_started.elapsed(),
            ));
            Some(result.is_ok())
        }
        Err(_) => None,
    };
    let media_probe_ms = media_probe_started.elapsed().as_millis() as u64;
    let resolution = resolution.map_err(|error| error.kind);
    youtube::browser_auth::shutdown_playback_browser().await;

    youtube_probe_report_from_resolution(
        configs.app_config.youtube.auth_type,
        probe_client,
        decoder_chunk_size,
        allow_browser_fallback,
        browser_fallback_attempted,
        ejs_backend,
        ejs_backend_configured,
        resolution,
        media_probe_succeeded,
        attempts,
        YouTubePlaybackProbeTimings {
            resolve_ms,
            media_probe_ms,
            total_ms: total_started.elapsed().as_millis() as u64,
        },
    )
}

pub(crate) async fn check_youtube_auth_with_configs(
    configs: &config::Configs,
) -> Result<crate::state::YouTubeLibrary> {
    let youtube = youtube::YouTubeMusic::new(configs).await?;
    youtube.library().await
}

#[cfg(feature = "private-capture")]
#[derive(Debug, Serialize)]
pub(crate) struct YouTubeLibraryInspection {
    pub(crate) playlists: usize,
    pub(crate) albums: usize,
    pub(crate) artists: usize,
    pub(crate) warning_count: usize,
}

#[cfg(feature = "private-capture")]
#[derive(Debug, Serialize)]
pub(crate) struct YouTubeDeveloperInspection {
    pub(crate) account_id: Option<String>,
    pub(crate) account_label: Option<String>,
    pub(crate) auth_type: &'static str,
    pub(crate) library: YouTubeLibraryInspection,
    pub(crate) library_status: &'static str,
    pub(crate) player: youtube::playback::YouTubePlaybackInspection,
    pub(crate) transport: Option<youtube::playback::MediaTransportDiagnostic>,
    pub(crate) transport_error_category: Option<String>,
}

#[cfg(feature = "private-capture")]
pub(crate) async fn inspect_youtube_for_video(
    video_id: &str,
    include_transport: bool,
) -> Result<YouTubeDeveloperInspection> {
    inspect_youtube_for_video_with_configs(config::get_config(), video_id, include_transport).await
}

#[cfg(feature = "private-capture")]
pub(crate) async fn inspect_youtube_for_video_with_configs(
    configs: &config::Configs,
    video_id: &str,
    include_transport: bool,
) -> Result<YouTubeDeveloperInspection> {
    let result = async {
        let resolver = youtube::playback::InnertubeAudioResolver::new(
            configs,
            reqwest::Client::builder().build()?,
        );
        let player = resolver
            .inspect_player_response(
                video_id,
                configs.app_config.youtube.playback_quality,
                CancellationToken::new(),
            )
            .await
            .context("inspect YouTube player response")?;
        let (library, library_status) = match check_youtube_auth_with_configs(configs).await {
            Ok(library) => (Some(library), "ready"),
            Err(_) => (None, "unavailable"),
        };
        let (transport, transport_error_category) = if include_transport {
            match resolver
                .diagnose_browser_media_transport(
                    video_id,
                    configs.app_config.youtube.playback_quality,
                    CancellationToken::new(),
                )
                .await
            {
                Ok(diagnostic) => (Some(diagnostic), None),
                Err(error) => (
                    None,
                    Some(error.kind.diagnostic_category().as_str().to_owned()),
                ),
            }
        } else {
            (None, None)
        };
        let registry = config::AccountRegistry::load(&configs.config_folder).ok();
        let account_id = registry.as_ref().and_then(|registry| {
            registry
                .active_id(config::ActiveProvider::YouTubeMusic)
                .map(str::to_owned)
        });
        let account_label = registry.as_ref().and_then(|registry| {
            registry
                .active_label(config::ActiveProvider::YouTubeMusic)
                .map(str::to_owned)
        });
        Ok(YouTubeDeveloperInspection {
            account_id,
            account_label,
            auth_type: youtube_auth_type_label(configs.app_config.youtube.auth_type),
            library: YouTubeLibraryInspection {
                playlists: library
                    .as_ref()
                    .map_or(0, |library| library.playlists.len()),
                albums: library.as_ref().map_or(0, |library| library.albums.len()),
                artists: library.as_ref().map_or(0, |library| library.artists.len()),
                warning_count: library.as_ref().map_or(0, |library| library.errors.len()),
            },
            library_status,
            player,
            transport,
            transport_error_category,
        })
    }
    .await;
    youtube::browser_auth::shutdown_playback_browser().await;
    result
}

#[cfg(feature = "private-capture")]
pub(crate) async fn capture_youtube_playback_for_video(
    configs: &config::Configs,
    video_id: &str,
    capture: crate::developer_capture::CaptureSession,
) -> Result<(), youtube::playback::AudioSourceError> {
    use youtube::playback::AudioSourceResolver as _;

    let resolver = youtube::playback::InnertubeAudioResolver::new(configs, reqwest::Client::new());
    let result = resolver
        .resolve_with_capture(
            video_id,
            configs.app_config.youtube.playback_quality,
            CancellationToken::new(),
            capture,
        )
        .await
        .map(|_| ());
    youtube::browser_auth::shutdown_playback_browser().await;
    result
}

const fn youtube_auth_type_label(auth_type: config::YouTubeMusicAuthType) -> &'static str {
    match auth_type {
        config::YouTubeMusicAuthType::Browser => "browser",
        config::YouTubeMusicAuthType::OAuth => "oauth",
        config::YouTubeMusicAuthType::Unauthenticated => "unauthenticated",
    }
}

pub async fn begin_youtube_oauth_login(client_id: String) -> Result<youtube::YouTubeOAuthLogin> {
    youtube::begin_oauth_login(client_id).await
}

pub async fn begin_youtube_browser_login(
    browser: Option<std::path::PathBuf>,
) -> Result<youtube::browser_auth::BrowserLoginSession> {
    youtube::browser_auth::begin_login_for_new_account(
        &config::get_config().config_folder,
        browser,
        false,
    )
    .await
}

const YOUTUBE_PLAYBACK_AUTH_PROBE_VIDEO_ID: &str = "jNQXAC9IVRw";

pub async fn check_youtube_playback_auth() -> Result<String> {
    check_youtube_playback_auth_for_video(YOUTUBE_PLAYBACK_AUTH_PROBE_VIDEO_ID).await
}

pub async fn check_youtube_playback_auth_for_video(video_id: &str) -> Result<String> {
    check_youtube_playback_for_video(video_id, false).await
}

pub async fn diagnose_youtube_media_transport_for_video(video_id: &str) -> Result<String> {
    let configs = config::get_config();
    let cancellation = tokio_util::sync::CancellationToken::new();
    let resolver = youtube::playback::InnertubeAudioResolver::new(
        configs,
        reqwest::Client::builder().build()?,
    );
    let result = resolver
        .diagnose_browser_media_transport(
            video_id,
            configs.app_config.youtube.playback_quality,
            cancellation,
        )
        .await
        .context("diagnose YouTube browser media transport")
        .map(|diagnostic| diagnostic.to_string());
    youtube::browser_auth::shutdown_playback_browser().await;
    result
}

pub async fn check_youtube_playback_output_for_video(video_id: Option<&str>) -> Result<String> {
    check_youtube_playback_for_video(
        video_id.unwrap_or(YOUTUBE_PLAYBACK_AUTH_PROBE_VIDEO_ID),
        true,
    )
    .await
}

async fn check_youtube_playback_for_video(
    video_id: &str,
    check_audio_output: bool,
) -> Result<String> {
    check_youtube_playback_for_video_with_configs(
        config::get_config(),
        video_id,
        check_audio_output,
    )
    .await
}

async fn check_youtube_playback_for_video_with_configs(
    configs: &config::Configs,
    video_id: &str,
    check_audio_output: bool,
) -> Result<String> {
    let result =
        check_youtube_playback_for_video_inner(configs, video_id, check_audio_output).await;
    youtube::browser_auth::shutdown_playback_browser().await;
    result
}

async fn check_youtube_playback_for_video_inner(
    configs: &config::Configs,
    video_id: &str,
    check_audio_output: bool,
) -> Result<String> {
    use youtube::playback::AudioSourceResolver as _;

    let cancellation = tokio_util::sync::CancellationToken::new();
    let resolver = youtube::playback::InnertubeAudioResolver::new(
        configs,
        reqwest::Client::builder().build()?,
    );
    let source = resolver
        .resolve(
            video_id,
            configs.app_config.youtube.playback_quality,
            cancellation.clone(),
        )
        .await
        .context("verify YouTube playback access")?;
    let decoded = youtube::playback::open_decoded_source(
        &source,
        Duration::ZERO,
        4 * 1024 * 1024,
        cancellation,
    )
    .await
    .context("verify YouTube playback transport and decoder")?;
    let readiness = if check_audio_output {
        verify_youtube_audio_output(decoded, source.duration).await?;
        "transport, decoder, audio output, and backward seek ready"
    } else {
        drop(decoded);
        "transport and decoder ready"
    };
    Ok(format!(
        "{} {} kbps via {} ({readiness})",
        source.mime_type,
        source.bitrate / 1_000,
        source.source_client
    ))
}

async fn verify_youtube_audio_output(
    decoded: Box<dyn rodio::Source<Item = f32> + Send>,
    duration: Option<Duration>,
) -> Result<()> {
    tokio::task::spawn_blocking(move || -> Result<()> {
        const CHECK_DURATION: Duration = Duration::from_secs(3);
        const MINIMUM_RENDERED: Duration = Duration::from_millis(2_500);

        let mut stream = rodio::OutputStreamBuilder::open_default_stream()
            .context("open the default audio output stream")?;
        stream.log_on_drop(false);
        let sink = rodio::Sink::connect_new(stream.mixer());
        sink.set_volume(0.0);
        sink.append(decoded);
        sink.play();
        std::thread::sleep(CHECK_DURATION);
        anyhow::ensure!(
            sink.get_pos() >= MINIMUM_RENDERED,
            "the YouTube decoder stopped after {:?}, before the silent output check completed",
            sink.get_pos()
        );
        if let Some(duration) = duration.filter(|duration| *duration >= Duration::from_secs(8)) {
            let forward = duration.mul_f32(0.6).min(Duration::from_secs(50));
            let backward = duration.mul_f32(0.15).min(Duration::from_secs(10));
            sink.try_seek(forward).map_err(|err| {
                anyhow::anyhow!("seek forward in the YouTube audio output check: {err:?}")
            })?;
            std::thread::sleep(Duration::from_millis(600));
            anyhow::ensure!(
                sink.get_pos() >= forward,
                "the YouTube output did not reach the forward seek position"
            );
            sink.try_seek(backward).map_err(|err| {
                anyhow::anyhow!("seek backward in the YouTube audio output check: {err:?}")
            })?;
            std::thread::sleep(Duration::from_millis(600));
            let actual = sink.get_pos();
            anyhow::ensure!(
                actual >= backward && actual <= backward + Duration::from_secs(2),
                "the YouTube backward seek landed at {actual:?} instead of near {backward:?}"
            );
        }
        sink.stop();
        Ok(())
    })
    .await
    .context("join the YouTube audio output check")??;
    Ok(())
}

/// Build the Spotify Web API client from the configured client ID.
///
/// The returned client is unauthenticated; call [`auth::prompt_for_user_token`] to obtain an
/// access token.
pub fn new_api_client() -> Result<auth::SpotifyWebApiClient> {
    new_api_client_for_id(config::get_config().app_config.get_client_id()?)
}

fn new_api_client_for_id(id: String) -> Result<auth::SpotifyWebApiClient> {
    let configs = config::get_config();
    // The bundled default (ncspot's client ID) is registered with extended quota mode and
    // predates Spotify's 2024 Web API changes, so it is far less likely to hit rate limits
    // than a freshly-registered client. Warn users who override it that they may run into
    // `429 Too Many Requests` / `403 Forbidden` errors.
    //
    // See https://github.com/aome510/spotify-player/issues/890 for details.
    if id != auth::NCSPOT_CLIENT_ID {
        tracing::warn!(
            "A custom `client_id` is configured. Newly-registered Spotify clients \
             use the restricted default quota mode and may hit rate-limit (429) or \
             forbidden (403) errors. Unless you specifically need your own client, \
             consider removing `client_id`/`client_id_command` to use the bundled default. \
             See https://github.com/aome510/spotify-player/issues/890 for details."
        );
    }

    let creds = rspotify::Credentials { id, secret: None };
    let mut scopes = auth::OAUTH_SCOPES
        .iter()
        .map(ToString::to_string)
        .collect::<HashSet<_>>();
    // `user-personalized` scope is not supported by the Web API client and only available to the official Spotify client
    scopes.remove("user-personalized");
    let oauth = rspotify::OAuth {
        redirect_uri: configs.app_config.login_redirect_uri.clone(),
        scopes,
        ..Default::default()
    };
    let config = rspotify::Config {
        token_cached: true,
        cache_path: configs.cache_folder.join("user_client_token.json"),
        ..Default::default()
    };
    Ok(auth::SpotifyWebApiClient::new(
        rspotify::AuthCodePkceSpotify::with_config(creds, oauth, config),
    ))
}

#[cfg(test)]
mod tests {
    use std::{fs, time::Duration};

    use super::{
        account_snapshot_label, annotate_startup_error, merge_cached_spotify_auth_snapshot,
        require_active_account, spotify_auth_bootstrap_can_degrade, startup_failure_metadata,
        startup_failure_metadata_for, startup_status_class_for_error,
        startup_status_class_for_http_status, startup_status_class_for_rspotify_error,
        youtube_probe_decoder_error_category, youtube_probe_report_from_resolution,
        StartupFailurePhase, StartupStatusClass, YouTubeBrowserLoginControl,
        YouTubePlaybackProbeTimings,
    };

    #[test]
    fn youtube_account_snapshot_label_does_not_use_spotify_profile() {
        let configs = crate::ui::initialize_test_config();
        let ring = std::sync::Arc::new(parking_lot::Mutex::new(std::collections::VecDeque::new()));
        let (diagnostics, _runtime) = crate::observability::disabled(ring);
        let state = crate::state::State::new_with_configs(false, diagnostics, configs);
        state.data.write().user_data.user = Some(
            serde_json::from_value(serde_json::json!({
                "country": null,
                "display_name": "Spotify Profile",
                "email": null,
                "external_urls": {},
                "explicit_content": null,
                "followers": null,
                "href": "https://open.spotify.com/user/profile",
                "id": "0123456789012345678901",
                "images": null,
                "product": null
            }))
            .unwrap(),
        );
        let state = std::sync::Arc::new(state);

        assert_eq!(
            account_snapshot_label(&state, crate::config::ActiveProvider::Spotify).as_deref(),
            Some("Spotify Profile")
        );
        assert_eq!(
            account_snapshot_label(&state, crate::config::ActiveProvider::YouTubeMusic),
            None
        );
    }

    #[tokio::test]
    async fn clearing_account_data_invalidates_retained_search_state_and_cache() {
        let configs = crate::ui::initialize_test_config();
        let ring = std::sync::Arc::new(parking_lot::Mutex::new(std::collections::VecDeque::new()));
        let (diagnostics, _runtime) = crate::observability::disabled(ring);
        let state = std::sync::Arc::new(crate::state::State::new_with_configs(
            false,
            diagnostics,
            configs,
        ));
        let client = super::AppClient::new_without_auth().unwrap();
        let query = "account scoped query";
        {
            let mut ui = state.ui.lock();
            ui.history.clear();
            ui.history.push(crate::state::PageState::Search {
                line_input: crate::ui::single_line_input::LineInput::default(),
                current_query: String::new(),
                state: crate::state::SearchPageUIState::new(),
            });
            ui.spotify_account_label = Some("Account A".to_owned());
            if let crate::state::PageState::Search { state, .. } = ui.current_page_mut() {
                state.provider = Some(crate::config::ActiveProvider::Spotify);
            }
            let reference = ui.begin_search(crate::config::ActiveProvider::Spotify, query);
            state.data.write().caches.search.insert(
                query.to_owned(),
                std::sync::Arc::new(crate::state::SearchResults::default()),
                Duration::from_secs(60),
            );
            ui.finish_search_success(crate::config::ActiveProvider::Spotify, query, &reference, 2);
            ui.new_page(crate::state::PageState::CommandHelp { scroll_offset: 0 });
        }

        client.clear_account_data(&state, crate::config::ActiveProvider::Spotify);

        {
            let mut ui = state.ui.lock();
            ui.spotify_account_label = Some("Account B".to_owned());
            let retained = ui
                .history
                .iter()
                .find_map(|page| match page {
                    crate::state::PageState::Search {
                        current_query,
                        state,
                        ..
                    } if current_query == query => Some(state),
                    _ => None,
                })
                .expect("Search page remains in history");
            assert_eq!(
                retained.search_lifecycle,
                crate::state::SearchLifecycle::Idle
            );
            assert!(retained.search_selection.selected_indices().is_empty());
        }
        assert!(state.data.read().caches.search.get(query).is_none());
    }

    #[tokio::test]
    async fn spotify_client_replacement_is_shared_and_retires_previous_cache_owner() {
        use rspotify::clients::BaseClient;
        crate::ui::initialize_test_config();
        let client = super::AppClient::new_without_auth().unwrap();
        let clone = client.clone();
        let previous = client.spotify_api();
        let id = "0123456789abcdef0123456789abcdef";
        client
            .replace_spotify_api(super::new_api_client_for_id(id.to_owned()).unwrap())
            .await;
        assert_eq!(clone.spotify_api().get_creds().id, id);
        assert!(clone
            .spotify_api()
            .get_token()
            .lock()
            .await
            .unwrap()
            .is_none());
        assert!(matches!(
            previous.write_token_cache().await,
            Err(rspotify::ClientError::InvalidToken)
        ));
        // Changing back must also retire the intermediate client.
        let intermediate = clone.spotify_api();
        client
            .replace_spotify_api(
                super::new_api_client_for_id(previous.get_creds().id.clone()).unwrap(),
            )
            .await;
        assert_eq!(clone.spotify_api().get_creds().id, previous.get_creds().id);
        assert!(intermediate.write_token_cache().await.is_err());
    }

    #[tokio::test]
    async fn spotify_saved_choice_applies_without_restart_including_change_back() {
        use rspotify::clients::BaseClient;
        crate::ui::initialize_test_config();
        let client = super::AppClient::new_without_auth().unwrap();
        let folder = tempfile::tempdir().unwrap();
        let original = client.spotify_api().get_creds().id.clone();
        let selected = "0123456789abcdef0123456789abcdef";
        crate::config::save_welcome_spotify_client(folder.path(), selected).unwrap();
        let current = client
            .apply_saved_spotify_client(folder.path())
            .await
            .unwrap();
        assert_eq!(current.get_creds().id, selected);
        assert!(
            crate::config::SetupState::load(folder.path())
                .unwrap()
                .spotify_reauthentication_required
        );
        crate::config::save_welcome_spotify_client(folder.path(), &original).unwrap();
        assert_eq!(
            client
                .apply_saved_spotify_client(folder.path())
                .await
                .unwrap()
                .get_creds()
                .id,
            original
        );
        assert!(current.write_token_cache().await.is_err());
        // Re-selecting the runtime ID still retires its token when fresh auth is pending.
        let previous = client.spotify_api();
        client
            .apply_saved_spotify_client(folder.path())
            .await
            .unwrap();
        assert!(previous.write_token_cache().await.is_err());
    }

    #[test]
    fn spotify_failure_copy_is_phase_specific_and_omits_raw_payload() {
        let error = annotate_startup_error(
            anyhow::anyhow!("secret-payload"),
            StartupFailurePhase::IntegratedSession,
        );
        let notice = super::spotify_failure_notice(&error);
        assert!(notice.contains("playback connection"));
        assert!(notice.contains("Premium"));
        assert!(!notice.contains("secret-payload"));
    }

    #[tokio::test]
    async fn welcome_invalid_cookie_import_preserves_saved_credentials_and_releases_slot() {
        let configs = crate::ui::initialize_test_config();
        let saved_before = std::fs::read(configs.youtube_music_cookie_path()).ok();
        let ring = std::sync::Arc::new(parking_lot::Mutex::new(std::collections::VecDeque::new()));
        let (diagnostics, _runtime) = crate::observability::disabled(ring);
        let state = std::sync::Arc::new(crate::state::State::new(false, diagnostics));
        let client = super::AppClient::new_without_auth().unwrap();
        let folder = tempfile::tempdir().unwrap();
        let file = folder.path().join("malformed-cookie.txt");
        std::fs::write(&file, "not a signed-in cookie header").unwrap();
        {
            let mut ui = state.ui.lock();
            ui.welcome_youtube_login_active = true;
            ui.welcome_youtube_operation = crate::state::WelcomeOperation::SigningIn;
        }
        client
            .handle_auth_session_request(
                &state,
                crate::client::ClientRequest::ImportYouTubeCookies(file),
            )
            .await
            .unwrap();
        assert_eq!(
            std::fs::read(configs.youtube_music_cookie_path()).ok(),
            saved_before
        );
        assert!(client.youtube_browser_login.lock().await.active.is_none());
        let ui = state.ui.lock();
        assert!(!ui.welcome_youtube_login_active);
        assert_eq!(
            ui.welcome_youtube_operation,
            crate::state::WelcomeOperation::Failed
        );
        assert_eq!(ui.welcome_youtube_account_tested, Some(false));
        assert!(ui
            .welcome_youtube_notice
            .as_ref()
            .unwrap()
            .contains("Cookie header"));
    }

    fn probe_timings() -> YouTubePlaybackProbeTimings {
        YouTubePlaybackProbeTimings {
            resolve_ms: 12,
            media_probe_ms: 3,
            total_ms: 15,
        }
    }

    fn native_probe_source(
        source_client: &'static str,
    ) -> crate::client::youtube::playback::ResolvedAudioSource {
        let mut required_headers = reqwest::header::HeaderMap::new();
        required_headers.insert(
            reqwest::header::AUTHORIZATION,
            reqwest::header::HeaderValue::from_static("Bearer oauth-secret"),
        );
        crate::client::youtube::playback::ResolvedAudioSource {
            media_id: "private-video-id".to_owned(),
            #[cfg(feature = "private-capture")]
            itag: 251,
            url: reqwest::Url::parse(
                "https://media.example/audio?sig=signed-url-secret&pot=proof-token-secret",
            )
            .unwrap(),
            required_headers,
            mime_type: "audio/webm; codecs=opus".to_owned(),
            bitrate: 128_000,
            content_length: Some(4096),
            duration: Some(Duration::from_millis(9_876)),
            expires_at_unix: Some(4_000_000_000),
            source_client,
        }
    }

    #[test]
    fn native_probe_success_serializes_only_safe_summary_fields() {
        let report = youtube_probe_report_from_resolution(
            crate::config::YouTubeMusicAuthType::Browser,
            crate::client::youtube::playback::YouTubeProbeClient::AndroidVr,
            crate::client::youtube::playback::YouTubeProbeDecoderChunkSize::TenMib,
            false,
            false,
            Some("QuickJS"),
            "QuickJS",
            Ok(native_probe_source("ANDROID_VR")),
            Some(true),
            vec![
                crate::client::youtube::playback::YouTubeProbeAttempt::decoder(
                    "ANDROID_VR",
                    "success",
                    None,
                    Duration::from_millis(3),
                ),
            ],
            probe_timings(),
        );
        let json = serde_json::to_string(&report).unwrap();

        assert!(report.is_success());
        assert!(json.contains("\"route\":\"native\""));
        assert!(json.contains("\"requested_client\":\"android-vr\""));
        assert!(json.contains("\"decoder_chunk_bytes\":10485760"));
        assert!(json.contains("\"native_result\":\"success\""));
        assert!(json.contains("\"stage\":\"decoder\""));
        for secret in [
            "private-video-id",
            "signed-url-secret",
            "oauth-secret",
            "proof-token-secret",
            "https://media.example",
        ] {
            assert!(!json.contains(secret), "probe JSON leaked {secret}");
        }
    }

    #[test]
    fn visionos_probe_success_keeps_report_machine_readable_and_redacted() {
        let report = youtube_probe_report_from_resolution(
            crate::config::YouTubeMusicAuthType::OAuth,
            crate::client::youtube::playback::YouTubeProbeClient::VisionOs,
            crate::client::youtube::playback::YouTubeProbeDecoderChunkSize::OneMib,
            false,
            false,
            None,
            "Node",
            Ok(native_probe_source("VISIONOS")),
            Some(true),
            vec![
                crate::client::youtube::playback::YouTubeProbeAttempt::decoder(
                    "VISIONOS",
                    "success",
                    None,
                    Duration::from_millis(4),
                ),
            ],
            probe_timings(),
        );
        let json = serde_json::to_string(&report).unwrap();

        assert!(report.is_success());
        assert!(json.contains("\"requested_client\":\"visionos\""));
        assert!(json.contains("\"client\":\"VISIONOS\""));
        assert!(json.contains("\"route\":\"native\""));
        for secret in [
            "private-video-id",
            "signed-url-secret",
            "oauth-secret",
            "proof-token-secret",
            "https://media.example",
        ] {
            assert!(!json.contains(secret), "probe JSON leaked {secret}");
        }
    }

    #[test]
    fn native_probe_failure_is_machine_readable_and_unsuccessful() {
        let report = youtube_probe_report_from_resolution(
            crate::config::YouTubeMusicAuthType::OAuth,
            crate::client::youtube::playback::YouTubeProbeClient::WebRemix,
            crate::client::youtube::playback::YouTubeProbeDecoderChunkSize::OneMib,
            false,
            false,
            None,
            "Node",
            Err(crate::client::youtube::playback::AudioSourceErrorKind::Network),
            None,
            Vec::new(),
            probe_timings(),
        );
        let json = serde_json::to_value(&report).unwrap();

        assert!(!report.is_success());
        assert_eq!(report.error_category(), Some("network"));
        assert_eq!(json["route"], serde_json::Value::Null);
        assert_eq!(json["requested_client"], "web-remix");
        assert_eq!(json["decoder_chunk_bytes"], 1_048_576);
        assert_eq!(json["native_result"], "failure");
        assert_eq!(json["browser_result"], "not_allowed");
        assert_eq!(json["media_probe_result"], "not_run");
    }

    #[test]
    fn decoder_probe_errors_map_only_to_safe_sub_stages() {
        for (message, expected) in [
            ("open native YouTube media transport", "transport_open"),
            ("initialize native YouTube stream", "stream_initialize"),
            ("decode native YouTube audio stream", "decoder_initialize"),
            ("join native YouTube decoder initialization", "decoder_task"),
            ("provider detail that must not be exposed", "unknown"),
        ] {
            let error = anyhow::anyhow!(message);
            assert_eq!(
                youtube_probe_decoder_error_category(&error, false),
                expected
            );
        }
        assert_eq!(
            youtube_probe_decoder_error_category(&anyhow::anyhow!("secret"), true),
            "cancelled"
        );
    }

    #[test]
    fn decoder_probe_attempt_serializes_only_the_safe_category() {
        let attempt = crate::client::youtube::playback::YouTubeProbeAttempt::decoder(
            "ANDROID_VR",
            "error",
            Some("decoder_initialize"),
            Duration::from_millis(347),
        );
        let json = serde_json::to_string(&attempt).unwrap();

        assert_eq!(
            json,
            r#"{"stage":"decoder","client":"ANDROID_VR","result":"error","error_category":"decoder_initialize","duration_ms":347}"#
        );
    }

    #[test]
    fn media_probe_attempt_serializes_role_without_transport_identity() {
        let attempt = crate::client::youtube::playback::YouTubeProbeAttempt {
            stage: "media_probe_attempt",
            client: "ANDROID_VR",
            result: "error",
            status: Some("no_response_timeout"),
            error_category: Some("network"),
            attempt: Some(1),
            target: Some("primary"),
            duration_ms: 3_001,
        };
        let json = serde_json::to_string(&attempt).unwrap();

        assert!(json.contains("\"attempt\":1"));
        assert!(json.contains("\"target\":\"primary\""));
        assert!(json.contains("\"status\":\"no_response_timeout\""));
        for forbidden in ["googlevideo", "videoplayback", "?", "cookie", "token"] {
            assert!(!json.contains(forbidden));
        }
    }

    #[test]
    fn decoder_media_attempt_serializes_phase_without_request_identity() {
        let attempt = crate::client::youtube::playback::YouTubeProbeAttempt::decoder_media(
            "ANDROID_VR",
            2,
            "followup_range",
            "error",
            "http_403_forbidden",
            Some("media_forbidden"),
            Duration::from_millis(219),
        );
        let json = serde_json::to_string(&attempt).unwrap();

        assert_eq!(
            json,
            r#"{"stage":"decoder_media_attempt","client":"ANDROID_VR","result":"error","status":"http_403_forbidden","error_category":"media_forbidden","attempt":2,"target":"followup_range","duration_ms":219}"#
        );
        for forbidden in [
            "googlevideo",
            "videoplayback",
            "signed-url",
            "cookie",
            "oauth",
            "proof-token",
        ] {
            assert!(!json.contains(forbidden));
        }
    }

    #[test]
    fn typed_decoder_probe_errors_discard_embedded_messages() {
        for (error, expected) in [
            (
                rodio::decoder::DecoderError::UnrecognizedFormat,
                "decoder_unrecognized_format",
            ),
            (
                rodio::decoder::DecoderError::IoError("signed URL detail".to_owned()),
                "decoder_io",
            ),
            (
                rodio::decoder::DecoderError::DecodeError("provider payload detail"),
                "decoder_malformed",
            ),
            (
                rodio::decoder::DecoderError::LimitError("limit detail"),
                "decoder_limit",
            ),
            (
                rodio::decoder::DecoderError::ResetRequired,
                "decoder_reset_required",
            ),
            (
                rodio::decoder::DecoderError::NoStreams,
                "decoder_no_streams",
            ),
        ] {
            let error = anyhow::Error::new(error).context("decode native YouTube audio stream");
            assert_eq!(
                youtube_probe_decoder_error_category(&error, false),
                expected
            );
        }
    }

    #[test]
    fn browser_login_cancellation_is_generation_safe() {
        let mut control = YouTubeBrowserLoginControl::default();
        let (first_generation, first_token) = control.begin().expect("first login starts");
        assert!(control.begin().is_none());
        assert!(first_token.is_cancelled());

        control.finish(first_generation);
        let (second_generation, second_token) = control.begin().expect("second login starts");
        control.finish(first_generation);
        assert!(!second_token.is_cancelled());
        control.finish(second_generation);
        assert!(control.active.is_none());
    }

    #[test]
    fn incomplete_saved_account_fails_before_provider_auth_is_used() {
        let root = tempfile::tempdir().unwrap();
        let config = root.path().join("config");
        let cache = root.path().join("cache");
        let cookie = config.join("youtube").join("cookie.txt");
        fs::create_dir_all(&cache).unwrap();
        let mut registry = crate::config::AccountRegistry::default();
        registry.add_metadata(crate::config::ActiveProvider::Spotify, Some("Stale"));
        registry.save(&config).unwrap();

        let error = require_active_account(
            &registry,
            crate::config::ActiveProvider::Spotify,
            &config,
            &cache,
            &cookie,
        )
        .unwrap_err();
        assert!(error.to_string().contains("complete saved session"));
    }

    #[test]
    fn startup_http_statuses_keep_safe_classes() {
        assert_eq!(
            startup_status_class_for_http_status(401),
            StartupStatusClass::Unauthorized
        );
        assert_eq!(
            startup_status_class_for_http_status(403),
            StartupStatusClass::Forbidden
        );
        assert_eq!(
            startup_status_class_for_http_status(429),
            StartupStatusClass::RateLimited
        );
        assert_eq!(
            startup_status_class_for_http_status(503),
            StartupStatusClass::ServerError
        );
    }

    #[test]
    fn startup_metadata_only_requires_welcome_for_definitive_auth_failures() {
        let raw = anyhow::Error::new(rspotify::ClientError::InvalidToken);
        let client_error = raw
            .chain()
            .find_map(|cause| cause.downcast_ref::<rspotify::ClientError>())
            .expect("rspotify error retained");
        assert_eq!(
            startup_status_class_for_rspotify_error(client_error),
            StartupStatusClass::Unauthorized
        );
        assert_eq!(
            startup_status_class_for_error(&raw, StartupFailurePhase::WebApiToken),
            StartupStatusClass::Unauthorized
        );
        let error = annotate_startup_error(raw, StartupFailurePhase::WebApiToken);
        let authentication = startup_failure_metadata(&error);
        assert_eq!(
            authentication.status_class,
            StartupStatusClass::Unauthorized
        );
        assert!(authentication.requires_setup());
        let wrapped = error.context("outer startup context");
        assert_eq!(
            startup_failure_metadata(&wrapped),
            authentication,
            "startup metadata must survive later anyhow context"
        );

        let rate_limited = startup_failure_metadata_for(
            StartupFailurePhase::CurrentUser,
            StartupStatusClass::RateLimited,
        );
        assert!(!rate_limited.requires_setup());
        assert!(rate_limited.retryable);

        let integrated = startup_failure_metadata_for(
            StartupFailurePhase::IntegratedSession,
            StartupStatusClass::Unknown,
        );
        assert!(integrated.requires_setup());
    }

    #[test]
    fn interactive_auth_only_degrades_for_post_auth_rate_limits() {
        let rate_limited_profile = startup_failure_metadata_for(
            StartupFailurePhase::CurrentUser,
            StartupStatusClass::RateLimited,
        );
        assert!(spotify_auth_bootstrap_can_degrade(rate_limited_profile));

        let rate_limited_playback = startup_failure_metadata_for(
            StartupFailurePhase::PlaybackProbe,
            StartupStatusClass::RateLimited,
        );
        assert!(spotify_auth_bootstrap_can_degrade(rate_limited_playback));

        let token_rate_limit = startup_failure_metadata_for(
            StartupFailurePhase::WebApiToken,
            StartupStatusClass::RateLimited,
        );
        assert!(!spotify_auth_bootstrap_can_degrade(token_rate_limit));

        let unauthorized_profile = startup_failure_metadata_for(
            StartupFailurePhase::CurrentUser,
            StartupStatusClass::Unauthorized,
        );
        assert!(!spotify_auth_bootstrap_can_degrade(unauthorized_profile));
    }

    #[test]
    fn cached_auth_refresh_preserves_live_premium_evidence() {
        let cached = crate::config::SpotifyAuthSnapshot {
            session_ready: true,
            premium: crate::config::SpotifyPremiumStatus::Unknown,
        };
        let live = crate::config::SpotifyAuthSnapshot {
            session_ready: true,
            premium: crate::config::SpotifyPremiumStatus::Premium,
        };

        assert_eq!(merge_cached_spotify_auth_snapshot(cached, live), live);

        let unavailable_cache = crate::config::SpotifyAuthSnapshot {
            session_ready: false,
            premium: crate::config::SpotifyPremiumStatus::Unknown,
        };
        assert_eq!(
            merge_cached_spotify_auth_snapshot(unavailable_cache, live),
            unavailable_cache,
            "live evidence must not make an incomplete cached session ready"
        );
    }
}

fn finish_listenbrainz_token(
    ui: &mut crate::state::UIState,
    configs: &config::Configs,
    attempt: u64,
    token: &super::listenbrainz::ListenBrainzToken,
    save: bool,
    result: Result<String>,
) {
    if ui.welcome_listenbrainz_pending != Some(attempt) {
        return;
    }
    if !matches!(ui.current_page(), crate::state::PageState::Welcome { state, .. } if state.step == crate::state::WelcomeStep::ListenBrainz)
    {
        ui.cancel_welcome_listenbrainz_check();
        return;
    }
    let result = result.and_then(|username| {
        if save {
            configs
                .save_listenbrainz_token(token.expose())
                .map_err(|_| {
                    anyhow::anyhow!("Could not save ListenBrainz token. Previous token retained.")
                })?;
        }
        Ok(username)
    });
    ui.welcome_listenbrainz_pending = None;
    match result {
        Ok(username) => {
            ui.welcome_listenbrainz_notice = Some(
                if save {
                    "Token validated and saved."
                } else {
                    "Token validated."
                }
                .to_owned(),
            );
            ui.welcome_listenbrainz_identity =
                Some(super::listenbrainz::ValidatedListenBrainzIdentity {
                    username: username.clone(),
                    token: token.clone(),
                });
            ui.welcome_listenbrainz_username = Some(username);
        }
        Err(error) => {
            ui.welcome_listenbrainz_notice = Some(error.to_string());
            ui.welcome_listenbrainz_username = None;
            ui.welcome_listenbrainz_identity = None;
        }
    }
}

#[cfg(test)]
mod listenbrainz_token_tests {
    use super::*;
    #[test]
    fn listenbrainz_completion_preserves_token_on_failure_or_stale_attempt() {
        crate::ui::initialize_test_config();
        let dir = tempfile::tempdir().unwrap();
        let configs = config::Configs::new(dir.path(), &dir.path().join("cache")).unwrap();
        configs.save_listenbrainz_token("previous").unwrap();
        let token = super::super::listenbrainz::ListenBrainzToken::new("replacement".to_owned());
        let mut ui = crate::state::UIState::default();
        let mut welcome = crate::state::WelcomePageUIState::new();
        welcome.show_step(crate::state::WelcomeStep::ListenBrainz);
        ui.history = vec![crate::state::PageState::Welcome {
            state: welcome,
            from_settings: false,
        }];
        ui.welcome_listenbrainz_pending = Some(2);
        finish_listenbrainz_token(
            &mut ui,
            &configs,
            1,
            &token,
            true,
            Ok("listener".to_owned()),
        );
        assert_eq!(configs.listenbrainz_token().as_deref(), Some("previous"));
        assert_eq!(ui.welcome_listenbrainz_pending, Some(2));
        finish_listenbrainz_token(
            &mut ui,
            &configs,
            2,
            &token,
            true,
            Err(anyhow::anyhow!("Invalid token")),
        );
        assert_eq!(configs.listenbrainz_token().as_deref(), Some("previous"));
        assert!(ui.welcome_listenbrainz_pending.is_none());
        ui.welcome_listenbrainz_pending = Some(3);
        finish_listenbrainz_token(
            &mut ui,
            &configs,
            3,
            &token,
            true,
            Ok("listener".to_owned()),
        );
        assert_eq!(configs.listenbrainz_token().as_deref(), Some("replacement"));
        assert_eq!(
            ui.welcome_listenbrainz_username.as_deref(),
            Some("listener")
        );
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(
                std::fs::metadata(configs.listenbrainz_token_path())
                    .unwrap()
                    .permissions()
                    .mode()
                    & 0o777,
                0o600
            );
        }
        assert!(!configs.app_config.listenbrainz.enabled);
        ui.welcome_listenbrainz_pending = Some(4);
        ui.history.pop();
        // Leaving the Welcome page must reject late success even without a newer request.
        ui.history.push(crate::state::PageState::Welcome {
            state: crate::state::WelcomePageUIState::new(),
            from_settings: false,
        });
        finish_listenbrainz_token(
            &mut ui,
            &configs,
            4,
            &super::super::listenbrainz::ListenBrainzToken::new("late-token".to_owned()),
            true,
            Ok("late".to_owned()),
        );
        assert_eq!(configs.listenbrainz_token().as_deref(), Some("replacement"));
        assert!(ui.welcome_listenbrainz_pending.is_none());
        ui.welcome_listenbrainz_pending = Some(5);
        ui.history = vec![crate::state::PageState::new_unified_playlist("local")];
        finish_listenbrainz_token(&mut ui, &configs, 5, &token, true, Ok("late".to_owned()));
        assert!(ui.welcome_listenbrainz_pending.is_none());
        assert_eq!(
            ui.welcome_listenbrainz_username.as_deref(),
            Some("listener")
        );
    }
    #[test]
    fn listenbrainz_save_failure_is_safe_and_does_not_claim_connection() {
        crate::ui::initialize_test_config();
        let dir = tempfile::tempdir().unwrap();
        let configs = config::Configs::new(dir.path(), &dir.path().join("cache")).unwrap();
        std::fs::write(dir.path().join("listenbrainz"), "blocked").unwrap();
        let mut ui = crate::state::UIState::default();
        let mut welcome = crate::state::WelcomePageUIState::new();
        welcome.show_step(crate::state::WelcomeStep::ListenBrainz);
        ui.history = vec![crate::state::PageState::Welcome {
            state: welcome,
            from_settings: false,
        }];
        ui.welcome_listenbrainz_pending = Some(1);
        let token =
            super::super::listenbrainz::ListenBrainzToken::new("sensitive-token".to_owned());
        finish_listenbrainz_token(
            &mut ui,
            &configs,
            1,
            &token,
            true,
            Ok("listener".to_owned()),
        );
        assert!(ui.welcome_listenbrainz_pending.is_none());
        assert!(ui.welcome_listenbrainz_username.is_none());
        assert!(!ui
            .welcome_listenbrainz_notice
            .unwrap()
            .contains("sensitive-token"));
    }
    #[test]
    fn listenbrainz_failure_never_changes_setup_readiness() {
        crate::ui::initialize_test_config();
        let mut ui = crate::state::UIState::default();
        let mut welcome = crate::state::WelcomePageUIState::new();
        welcome.show_step(crate::state::WelcomeStep::ListenBrainz);
        ui.history = vec![crate::state::PageState::Welcome {
            state: welcome,
            from_settings: false,
        }];
        let before = ui.setup_state.failure_for(ui.setup_auth_snapshot());
        ui.welcome_listenbrainz_notice = Some("Token check failed".to_owned());
        ui.welcome_listenbrainz_pending = Some(1);
        assert_eq!(ui.setup_state.failure_for(ui.setup_auth_snapshot()), before);
    }
}

fn listenbrainz_catalog_owned(
    ui: &crate::state::UIState,
    configs: &config::Configs,
    operation: u64,
    identity: &super::listenbrainz::ValidatedListenBrainzIdentity,
) -> bool {
    matches!(ui.current_page(), crate::state::PageState::Welcome { state, .. } if state.step == crate::state::WelcomeStep::ListenBrainz)
        && matches!(&ui.popup, Some(crate::state::PopupState::ListenBrainzPlaylists { operation: active, identity: owner, .. }) if *active == operation && owner == identity)
        && ui.welcome_listenbrainz_identity.as_ref() == Some(identity)
        && configs.listenbrainz_token().as_deref() == Some(identity.token.expose())
}
fn finish_listenbrainz_catalog_ownership(
    ui: &mut crate::state::UIState,
    configs: &config::Configs,
    operation: u64,
    identity: &super::listenbrainz::ValidatedListenBrainzIdentity,
) -> bool {
    if listenbrainz_catalog_owned(ui, configs, operation, identity) {
        return true;
    }
    if matches!(&ui.popup, Some(crate::state::PopupState::ListenBrainzPlaylists { operation: active, .. }) if *active == operation)
    {
        ui.popup = None;
        ui.welcome_listenbrainz_identity = None;
        ui.welcome_listenbrainz_username = None;
        ui.welcome_listenbrainz_notice =
            Some("Playlist request cancelled. Validate the current token to retry.".to_owned());
    }
    false
}

#[cfg(test)]
mod listenbrainz_catalog_tests {
    use super::*;
    #[test]
    fn listenbrainz_catalog_rejects_cancel_newer_operation_and_changed_token() {
        crate::ui::initialize_test_config();
        let dir = tempfile::tempdir().unwrap();
        let configs = config::Configs::new(dir.path(), &dir.path().join("cache")).unwrap();
        configs.save_listenbrainz_token("old").unwrap();
        let identity = super::super::listenbrainz::ValidatedListenBrainzIdentity {
            username: "owner".to_owned(),
            token: super::super::listenbrainz::ListenBrainzToken::new("old".to_owned()),
        };
        let mut ui = crate::state::UIState::default();
        let mut welcome = crate::state::WelcomePageUIState::new();
        welcome.show_step(crate::state::WelcomeStep::ListenBrainz);
        ui.history = vec![crate::state::PageState::Welcome {
            state: welcome,
            from_settings: false,
        }];
        ui.welcome_listenbrainz_identity = Some(identity.clone());
        let popup = || crate::state::PopupState::ListenBrainzPlaylists {
            operation: 2,
            identity: identity.clone(),
            rows: Vec::new(),
            state: Default::default(),
            busy: true,
            notice: String::new(),
        };
        ui.popup = Some(popup());
        assert!(listenbrainz_catalog_owned(&ui, &configs, 2, &identity));
        assert!(!finish_listenbrainz_catalog_ownership(
            &mut ui, &configs, 1, &identity
        ));
        assert!(ui.popup.is_some());
        ui.popup = None;
        assert!(!finish_listenbrainz_catalog_ownership(
            &mut ui, &configs, 2, &identity
        ));
        ui.popup = Some(popup());
        configs.save_listenbrainz_token("changed").unwrap();
        assert!(!finish_listenbrainz_catalog_ownership(
            &mut ui, &configs, 2, &identity
        ));
        assert!(ui.popup.is_none());
        assert!(ui.welcome_listenbrainz_identity.is_none());
        assert_eq!(configs.listenbrainz_token().as_deref(), Some("changed"));
    }
}
