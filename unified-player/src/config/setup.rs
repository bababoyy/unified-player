use std::path::Path;

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

use super::ActiveProvider;

const SETUP_CONFIG_FILE: &str = "setup.toml";

#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
pub enum SetupStatus {
    #[default]
    Pending,
    Ready,
    Skipped,
    Failed,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub enum SetupFailure {
    MissingSpotifySession,
    MissingSpotifyPremium,
    #[serde(alias = "MissingYouTubeBrowserAuth")]
    MissingYouTubeAccountAuth,
    AuthenticationFailed,
    SpotifySessionUnavailable,
    PersistenceFailed,
}

impl SetupFailure {
    pub const fn message(self) -> &'static str {
        match self {
            Self::MissingSpotifySession => {
                "Spotify sign-in is required before the first playback."
            }
            Self::MissingSpotifyPremium => {
                "Spotify playback requires a Premium account; this account was not confirmed as Premium."
            }
            Self::MissingYouTubeAccountAuth => {
                "YouTube Music account and library access requires Browser or OAuth authentication."
            }
            Self::AuthenticationFailed => {
                "Authentication did not finish. Retry the provider sign-in or use an existing session."
            }
            Self::SpotifySessionUnavailable => {
                "Spotify authentication is ready, but the integrated session could not be started. Retry the connection or open Diagnostics."
            }
            Self::PersistenceFailed => {
                "Setup could not be saved. Check the application configuration and retry."
            }
        }
    }
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum SpotifyPremiumStatus {
    #[default]
    Unknown,
    Premium,
    NotPremium,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct SpotifyAuthSnapshot {
    pub session_ready: bool,
    pub premium: SpotifyPremiumStatus,
}

impl SpotifyAuthSnapshot {
    pub const fn ready(self) -> bool {
        self.session_ready && matches!(self.premium, SpotifyPremiumStatus::Premium)
    }
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct YouTubeAuthSnapshot {
    pub account_ready: bool,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct SetupAuthSnapshot {
    pub spotify: SpotifyAuthSnapshot,
    pub youtube: YouTubeAuthSnapshot,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct SetupState {
    /// A changed Web API application must not reuse the previous token cache.
    #[serde(default)]
    pub spotify_reauthentication_required: bool,
    #[serde(default)]
    pub status: SetupStatus,
    #[serde(default = "default_startup_provider")]
    pub startup_provider: ActiveProvider,
    #[serde(default)]
    pub pause_on_startup: bool,
    #[serde(default)]
    pub failure: Option<SetupFailure>,
}

const fn default_startup_provider() -> ActiveProvider {
    ActiveProvider::Spotify
}

impl Default for SetupState {
    fn default() -> Self {
        Self {
            spotify_reauthentication_required: false,
            status: SetupStatus::Pending,
            startup_provider: ActiveProvider::Spotify,
            pause_on_startup: false,
            failure: None,
        }
    }
}

impl SetupState {
    pub fn load(config_folder: &Path) -> Result<Self> {
        let path = config_folder.join(SETUP_CONFIG_FILE);
        match std::fs::read_to_string(path) {
            Ok(content) => toml::from_str(&content).context("parse first-use setup state"),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(Self::default()),
            Err(error) => Err(error).context("read first-use setup state"),
        }
    }

    pub fn save(&self, config_folder: &Path) -> Result<()> {
        std::fs::create_dir_all(config_folder).context("create setup configuration directory")?;
        let content = toml::to_string_pretty(self).context("serialize first-use setup state")?;
        std::fs::write(config_folder.join(SETUP_CONFIG_FILE), content)
            .context("persist first-use setup state")
    }

    pub const fn requires_attention(&self) -> bool {
        (self.spotify_reauthentication_required
            && matches!(self.startup_provider, ActiveProvider::Spotify))
            || matches!(self.status, SetupStatus::Pending | SetupStatus::Failed)
            || self.failure.is_some()
    }

    pub const fn failure_for(&self, auth: SetupAuthSnapshot) -> Option<SetupFailure> {
        match self.startup_provider {
            ActiveProvider::Spotify => {
                if self.spotify_reauthentication_required {
                    return Some(SetupFailure::MissingSpotifySession);
                }
                if !auth.spotify.session_ready {
                    return Some(SetupFailure::MissingSpotifySession);
                }
                if !matches!(auth.spotify.premium, SpotifyPremiumStatus::Premium) {
                    return Some(SetupFailure::MissingSpotifyPremium);
                }
            }
            ActiveProvider::YouTubeMusic if !auth.youtube.account_ready => {
                return Some(SetupFailure::MissingYouTubeAccountAuth);
            }
            ActiveProvider::YouTubeMusic => {}
        }
        None
    }

    pub fn ready_for(&self, auth: SetupAuthSnapshot) -> bool {
        self.failure_for(auth).is_none()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Clone, Copy, Default)]
    struct FakeAuth {
        spotify_session: bool,
        spotify_premium: SpotifyPremiumStatus,
        youtube_account: bool,
    }

    impl FakeAuth {
        const fn snapshot(self) -> SetupAuthSnapshot {
            SetupAuthSnapshot {
                spotify: SpotifyAuthSnapshot {
                    session_ready: self.spotify_session,
                    premium: self.spotify_premium,
                },
                youtube: YouTubeAuthSnapshot {
                    account_ready: self.youtube_account,
                },
            }
        }
    }

    #[derive(Clone, Copy)]
    struct FakeProvider {
        provider: ActiveProvider,
    }

    impl FakeProvider {
        const fn spotify() -> Self {
            Self {
                provider: ActiveProvider::Spotify,
            }
        }

        const fn youtube() -> Self {
            Self {
                provider: ActiveProvider::YouTubeMusic,
            }
        }

        fn setup(self) -> SetupState {
            SetupState {
                startup_provider: self.provider,
                ..SetupState::default()
            }
        }
    }

    #[test]
    fn fresh_setup_is_pending_and_needs_spotify_auth() {
        let setup = SetupState::default();
        assert!(setup.requires_attention());
        assert_eq!(
            setup.failure_for(FakeAuth::default().snapshot()),
            Some(SetupFailure::MissingSpotifySession)
        );
    }

    #[test]
    fn pending_spotify_client_does_not_block_completed_youtube_setup() {
        let setup = SetupState {
            status: SetupStatus::Ready,
            startup_provider: ActiveProvider::YouTubeMusic,
            spotify_reauthentication_required: true,
            ..Default::default()
        };
        assert!(!setup.requires_attention());
        assert!(setup.ready_for(SetupAuthSnapshot {
            youtube: YouTubeAuthSnapshot {
                account_ready: true
            },
            ..Default::default()
        }));
    }

    #[test]
    fn existing_session_is_not_first_run_forever_after_ready_persistence() {
        let mut setup = FakeProvider::spotify().setup();
        setup.status = SetupStatus::Ready;
        assert!(!setup.requires_attention());
        assert!(setup.ready_for(
            FakeAuth {
                spotify_session: true,
                spotify_premium: SpotifyPremiumStatus::Premium,
                ..FakeAuth::default()
            }
            .snapshot()
        ));
    }

    #[test]
    fn partially_completed_setup_keeps_the_missing_provider_boundary_visible() {
        let setup = FakeProvider::youtube().setup();
        let auth = FakeAuth {
            spotify_session: true,
            spotify_premium: SpotifyPremiumStatus::Premium,
            ..FakeAuth::default()
        };
        assert!(setup.requires_attention());
        assert_eq!(
            setup.failure_for(auth.snapshot()),
            Some(SetupFailure::MissingYouTubeAccountAuth)
        );
    }

    #[test]
    fn unknown_premium_is_not_silently_assumed() {
        let auth = FakeAuth {
            spotify_session: true,
            spotify_premium: SpotifyPremiumStatus::Unknown,
            ..FakeAuth::default()
        };
        assert_eq!(
            FakeProvider::spotify().setup().failure_for(auth.snapshot()),
            Some(SetupFailure::MissingSpotifyPremium)
        );
    }

    #[test]
    fn unavailable_spotify_session_explains_the_integrated_boundary() {
        assert!(SetupFailure::SpotifySessionUnavailable
            .message()
            .contains("integrated session"));
    }

    #[test]
    fn youtube_requires_account_auth_in_addition_to_spotify() {
        let mut setup = FakeProvider::youtube().setup();
        let spotify_auth = FakeAuth {
            spotify_session: true,
            spotify_premium: SpotifyPremiumStatus::Premium,
            ..FakeAuth::default()
        };
        assert_eq!(
            setup.failure_for(spotify_auth.snapshot()),
            Some(SetupFailure::MissingYouTubeAccountAuth)
        );

        setup.status = SetupStatus::Ready;
        assert!(setup.ready_for(
            FakeAuth {
                youtube_account: true,
                ..spotify_auth
            }
            .snapshot()
        ));
    }

    #[test]
    fn youtube_setup_does_not_require_an_unrelated_spotify_account() {
        let setup = FakeProvider::youtube().setup();
        assert!(setup.ready_for(
            FakeAuth {
                youtube_account: true,
                ..FakeAuth::default()
            }
            .snapshot()
        ));
    }

    #[test]
    fn cancellation_returns_setup_to_pending_without_auth_data() {
        let mut setup = FakeProvider::spotify().setup();
        setup.status = SetupStatus::Failed;
        setup.failure = Some(SetupFailure::AuthenticationFailed);

        setup.status = SetupStatus::Pending;
        setup.failure = None;

        assert!(setup.requires_attention());
        assert_eq!(setup.failure, None);
    }

    #[test]
    fn restart_preserves_partial_setup_and_ready_does_not_reopen_it() {
        let folder = tempfile::tempdir().unwrap();
        let partial = SetupState {
            startup_provider: ActiveProvider::YouTubeMusic,
            pause_on_startup: true,
            ..SetupState::default()
        };
        partial.save(folder.path()).unwrap();
        let restored = SetupState::load(folder.path()).unwrap();
        assert_eq!(restored.status, SetupStatus::Pending);
        assert_eq!(restored.startup_provider, ActiveProvider::YouTubeMusic);
        assert!(restored.pause_on_startup);

        let mut ready = restored;
        ready.status = SetupStatus::Ready;
        ready.failure = None;
        ready.save(folder.path()).unwrap();
        let restarted = SetupState::load(folder.path()).unwrap();
        assert!(!restarted.requires_attention());
    }

    #[test]
    fn setup_state_round_trips_without_secrets() {
        let folder = tempfile::tempdir().unwrap();
        let state = SetupState {
            spotify_reauthentication_required: false,
            status: SetupStatus::Skipped,
            startup_provider: ActiveProvider::YouTubeMusic,
            pause_on_startup: true,
            failure: None,
        };
        state.save(folder.path()).unwrap();
        let restored = SetupState::load(folder.path()).unwrap();
        assert_eq!(restored, state);
        let content = std::fs::read_to_string(folder.path().join(SETUP_CONFIG_FILE)).unwrap();
        assert!(!content.contains("token"));
        assert!(!content.contains("cookie"));
    }

    #[test]
    fn old_browser_specific_failure_name_still_loads() {
        let restored: SetupState = toml::from_str(
            "status = 'Failed'\nstartup_provider = 'YouTubeMusic'\npause_on_startup = false\nfailure = 'MissingYouTubeBrowserAuth'\n",
        )
        .unwrap();
        assert_eq!(
            restored.failure,
            Some(SetupFailure::MissingYouTubeAccountAuth)
        );
    }
}
