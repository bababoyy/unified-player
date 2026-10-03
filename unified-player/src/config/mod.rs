mod accounts;
mod keymap;
mod setup;
mod theme;

const DEFAULT_CONFIG_FOLDER: &str = ".config/unified-player";
const DEFAULT_CACHE_FOLDER: &str = ".cache/unified-player";
const APP_CONFIG_FILE: &str = "app.toml";
const THEME_CONFIG_FILE: &str = "theme.toml";
const KEYMAP_CONFIG_FILE: &str = "keymap.toml";

use anyhow::{anyhow, Result};
use config_parser2::{config_parser_impl, ConfigParse, ConfigParser};
use librespot_core::config::SessionConfig;
use reqwest::Url;
use serde::{Deserialize, Serialize};
use std::{
    path::{Path, PathBuf},
    sync::OnceLock,
};

use anyhow::Context;
use theme::ThemeConfig;

pub use accounts::{AccountRecord, AccountRegistry, AccountSummary};
pub(crate) use keymap::{KeymapConfig, ResolvedBinding};
pub use setup::{
    SetupAuthSnapshot, SetupFailure, SetupState, SetupStatus, SpotifyAuthSnapshot,
    SpotifyPremiumStatus, YouTubeAuthSnapshot,
};
pub use theme::Theme;

use crate::auth::{NCSPOT_CLIENT_ID, SPOTIFY_CLIENT_ID};

static CONFIGS: OnceLock<Configs> = OnceLock::new();

#[derive(Debug)]
pub struct Configs {
    pub app_config: AppConfig,
    pub setup: SetupState,
    pub keymap_config: KeymapConfig,
    pub theme_config: ThemeConfig,
    pub config_folder: std::path::PathBuf,
    pub cache_folder: std::path::PathBuf,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum AppConfigValueKind {
    Bool,
    Choice(Vec<String>),
    MultiChoice(Vec<String>),
    Value,
    Status,
    Action(AppConfigAction),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AppConfigAction {
    OpenWelcomeSetup,
    AuthenticateSpotify,
    AuthenticateYouTubeBrowser,
    AddSpotifyAccount,
    AddYouTubeAccount,
    ValidateSpotifyAccount,
    ValidateYouTubeAccount,
    RemoveSpotifyAccount,
    RemoveYouTubeAccount,
    ImportYouTubeAuth,
    TestYouTubeAuth,
    OpenLogs,
    ClearHomeHistory,
    ResetAllConfiguration,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord)]
pub enum AppConfigSection {
    #[default]
    Accounts,
    Spotify,
    YouTubeMusic,
    Playback,
    SharedUi,
    Services,
    Diagnostics,
}

impl AppConfigSection {
    pub fn title(self) -> &'static str {
        match self {
            Self::Accounts => "Accounts",
            Self::Spotify => "Spotify",
            Self::YouTubeMusic => "YouTube Music",
            Self::Playback => "Playback",
            Self::SharedUi => "Shared UI",
            Self::Services => "Services",
            Self::Diagnostics => "Diagnostics",
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AppConfigSetting {
    pub section: AppConfigSection,
    pub key: String,
    pub value: String,
    pub kind: AppConfigValueKind,
    pub restart_required: bool,
}

/// Human-facing label for the settings UI. The persisted key remains the
/// source of truth for editing and CLI overrides; this projection keeps the
/// ordinary UI focused on user concepts instead of implementation paths.
pub fn setting_label(key: &str) -> String {
    let label = match key {
        "client_id" => "Spotify Web API client ID",
        "active_provider" => "Active provider",
        "presentation.profile" => "Presentation profile",
        "presentation.layout_preset" => "Layout preset",
        "presentation.compact_metadata" => "Compact metadata detail",
        "presentation.journal_indicators" => "Journal indicators",
        "presentation.focused_row_overflow" => "Focused row overflow",
        "lyrics.providers" => "Lyrics providers",
        "session_history.enabled" => "Session history",
        "session_history.max_entries" => "Session history limit",
        "listenbrainz.enabled" => "ListenBrainz integration",
        "listenbrainz.read_only_checking" => "ListenBrainz read-only checking",
        "listenbrainz.artist_enrichment" => "ListenBrainz artist enrichment",
        "layout.library.playlist_percent" => "Library playlist width",
        "layout.library.album_percent" => "Library album width",
        "layout.playback_window_position" => "Playback window position",
        "layout.playback_window_height" => "Playback window height",
        "app_refresh_duration_in_ms" => "UI frame interval",
        "terminal_title" => "Terminal title",
        "terminal_title_idle" => "Terminal title when idle",
        "enable_relative_line_number" => "Relative line numbers",
        "enable_mouse_scroll_volume" => "Mouse volume scrolling",
        "enable_mouse_navigation" => "Mouse list navigation",
        "custom_queue" => "App-managed queue",
        "playback_metadata_fields" => "Playback metadata",
        "accounts.spotify.active" => "Active Spotify account",
        "accounts.youtube_music.active" => "Active YouTube Music account",
        "accounts.spotify.status" => "Spotify account status",
        "accounts.youtube_music.status" => "YouTube Music account status",
        "accounts.spotify.add" => "Add Spotify account",
        "accounts.youtube_music.add" => "Add YouTube Music account",
        "accounts.spotify.validate" => "Validate Spotify account",
        "accounts.youtube_music.validate" => "Validate YouTube Music account",
        "accounts.spotify.remove" => "Remove Spotify account",
        "accounts.youtube_music.remove" => "Remove YouTube Music account",
        "setup.welcome" => "First-use setup",
        "spotify.auth" => "Spotify authentication",
        "youtube.auth.browser_login" => "YouTube browser sign-in",
        "youtube.auth.import" => "Import YouTube credentials",
        "youtube.auth.browser_session" => "Dedicated browser session",
        "youtube.auth.test" => "Test YouTube access",
        "youtube.playback_backend" => "YouTube playback backend",
        "youtube.javascript_runtime.status" => "Compiled JavaScript runtimes",
        "listenbrainz.auth" => "ListenBrainz token",
        "home.history.clear" => "Clear Home history",
        "diagnostics.view" => "Live diagnostics",
        "diagnostics.reset_all" => "Reset all configuration",
        _ => key.rsplit('.').next().unwrap_or(key),
    };

    if matches!(key, "active_provider" | "presentation.compact_metadata") || label.contains(' ') {
        label.to_string()
    } else {
        label
            .split('_')
            .filter(|part| !part.is_empty())
            .enumerate()
            .map(|(index, part)| {
                if index == 0 {
                    let mut chars = part.chars();
                    chars
                        .next()
                        .map(|first| first.to_uppercase().collect::<String>() + chars.as_str())
                        .unwrap_or_default()
                } else {
                    part.to_string()
                }
            })
            .collect::<Vec<_>>()
            .join(" ")
    }
}

/// Short help copy for the settings surface. Keep this separate from the
/// persisted key so the UI can explain the user-facing choice without
/// exposing implementation details.
pub fn setting_description(key: &str) -> &'static str {
    match key {
        "client_id" => "Spotify Web API application ID, not a secret. Use Apply & sign in in Welcome to authorize changes immediately.",
        "active_provider" => "Choose which provider supplies searches and playback.",
        "presentation.layout_preset" => "Switch live between current borders and borderless windows, preserving spacing and page state.",
        "presentation.profile" => {
            "Choose a preset for compact metadata and journal rows, or keep explicit custom values."
        }
        "presentation.compact_metadata" => {
            "Choose how much secondary metadata remains visible in compact rows."
        }
        "presentation.journal_indicators" => {
            "Choose and order journal states; an empty list hides the compact journal column."
        }
        "presentation.focused_row_overflow" => {
            "Choose whether overflowing focused-row text is truncated, scrolls on its own, or scrolls with Left/Right."
        }
        "lyrics.providers" => {
            "Choose which external lyrics providers may be queried after native lyrics."
        }
        "session_history.enabled" => {
            "Keep a local, bounded record of tracks started in this application."
        }
        "session_history.max_entries" => {
            "Set the maximum number of local session entries retained on disk."
        }
        "listenbrainz.enabled" => "Allow explicit ListenBrainz features to make provider requests.",
        "listenbrainz.read_only_checking" => {
            "Allow explicit sync previews to read linked ListenBrainz playlists without writing."
        }
        "listenbrainz.artist_enrichment" => {
            "Show ListenBrainz recordings or albums when Spotify artist sections are unavailable."
        }
        "theme" => "Select the color and component style used throughout the terminal UI.",
        "border_type" => "Choose the border treatment used around pages and popups.",
        "progress_bar_type" => "Choose a line or rectangle playback progress indicator.",
        "progress_bar_position" => {
            "Place the playback progress indicator below or beside playback."
        }
        "page_size_in_rows" => "Set how many rows page navigation advances at a time.",
        "enable_relative_line_number" => "Show Vim-style relative numbers beside list rows.",
        "enable_mouse_scroll_volume" => "Use the mouse wheel over playback to adjust volume.",
        "enable_mouse_navigation" => {
            "Allow the mouse to navigate page lists; playback volume remains a separate setting."
        }
        "playback_metadata_fields" => "Choose which playback controls appear in the status area.",
        "layout.library.playlist_percent" => "Set the library playlist pane width as a percentage.",
        "layout.library.album_percent" => "Set the library album pane width as a percentage.",
        "layout.playback_window_position" => "Place the playback window at the top or bottom.",
        "layout.playback_window_height" => "Set the playback window height in terminal rows.",
        "app_refresh_duration_in_ms" => {
            "Minimum ms between redraws while the screen changes. 16 is about 60 FPS; the floor is 8."
        }
        "custom_queue" => "Use the app-managed queue for full playlist playback.",
        "tracks_playback_limit" => "Limit the number of tracks sent to a playback request.",
        "seek_duration_secs" => "Set the number of seconds used by seek commands.",
        "volume_scroll_step" => "Set the volume change applied by one mouse-wheel step.",
        "playback_format" => "Set the format string used for the playback line.",
        "terminal_title" => {
            "Terminal title while something plays; leave empty to keep the terminal's own title."
        }
        "terminal_title_idle" => "Terminal title while nothing is playing.",
        "youtube.auth_type" => "Choose how YouTube Music credentials are supplied.",
        "youtube.playback_quality" => "Choose higher quality or lower data usage for playback.",
        "youtube.javascript_runtime" => "Choose the runtime used to solve player challenges.",
        "youtube.native_audio_cache_size_mb" => "Set the temporary native audio cache size.",
        "youtube.cookie_file" => "Set the path to the browser cookie export.",
        "youtube.oauth_file" => "Set the path to the YouTube OAuth credentials.",
        "youtube.po_token_file" => "Set the optional proof-of-origin token file path.",
        "youtube.auth.browser_login" => "Open or cancel the dedicated browser sign-in flow.",
        "youtube.auth.import" => "Import credentials from the configured browser session.",
        "youtube.auth.test" => "Check metadata and playback access for the active account.",
        "accounts.spotify.active" => "Choose the Spotify account used by this installation.",
        "accounts.youtube_music.active" => {
            "Choose the YouTube Music account used by this installation."
        }
        "accounts.spotify.status" => "Review safe readiness information for Spotify accounts.",
        "accounts.youtube_music.status" => {
            "Review safe readiness information for YouTube Music accounts."
        }
        "accounts.spotify.add" => "Authenticate and save another Spotify account.",
        "accounts.youtube_music.add" => "Authenticate and save another YouTube Music account.",
        "accounts.spotify.validate" => "Validate the active Spotify account session.",
        "accounts.youtube_music.validate" => "Validate the active YouTube Music account session.",
        "accounts.spotify.remove" => "Remove the active Spotify account from this installation.",
        "accounts.youtube_music.remove" => {
            "Remove the active YouTube Music account from this installation."
        }
        "setup.welcome" => "Reopen first-use setup without changing the current page history.",
        "spotify.auth" => "Start or refresh Spotify authentication in the browser.",
        "listenbrainz.auth" => "Review whether a ListenBrainz token is available.",
        "home.history.clear" => {
            "Forget the collections listed under Continue on Home for every account."
        }
        "diagnostics.view" => "Open live diagnostics for safe, actionable runtime status.",
        "diagnostics.reset_all" => {
            "Delete all saved preferences, accounts, journal, and history, then reopen first-use setup."
        }
        "enable_streaming" => "Choose whether this build owns integrated audio streaming.",
        "pause_on_startup" => "Start with playback paused instead of resuming automatically.",
        _ => "Application setting. Enter to edit or choose a value.",
    }
}

impl Configs {
    pub fn new(config_folder: &std::path::Path, cache_folder: &std::path::Path) -> Result<Self> {
        Self::new_with_account_bootstrap(config_folder, cache_folder, true)
    }

    /// Load configuration after a newly captured provider session has been
    /// written. Account bootstrap is intentionally skipped so the pending
    /// session is not replaced by the previously active slot before it is
    /// validated and registered.
    pub fn new_without_account_bootstrap(
        config_folder: &std::path::Path,
        cache_folder: &std::path::Path,
    ) -> Result<Self> {
        Self::new_with_account_bootstrap(config_folder, cache_folder, false)
    }

    fn new_with_account_bootstrap(
        config_folder: &std::path::Path,
        cache_folder: &std::path::Path,
        bootstrap_accounts: bool,
    ) -> Result<Self> {
        let app_config = AppConfig::new(config_folder)?;
        if bootstrap_accounts {
            let youtube_cookie_path = app_config
                .youtube
                .cookie_file
                .clone()
                .unwrap_or_else(|| config_folder.join("youtube").join("cookie.txt"));
            AccountRegistry::bootstrap(config_folder, cache_folder, &youtube_cookie_path)?;
        }
        let setup_file_exists = config_folder.join("setup.toml").is_file();
        let mut setup = SetupState::load(config_folder)?;
        if !setup_file_exists {
            setup.startup_provider = app_config.active_provider;
        }
        Ok(Self {
            app_config,
            setup,
            keymap_config: KeymapConfig::new(config_folder)?,
            theme_config: ThemeConfig::new(config_folder)?,
            config_folder: config_folder.to_path_buf(),
            cache_folder: cache_folder.to_path_buf(),
        })
    }

    pub fn youtube_music_cookie_path(&self) -> PathBuf {
        self.app_config
            .youtube
            .cookie_file
            .clone()
            .unwrap_or_else(|| self.config_folder.join("youtube").join("cookie.txt"))
    }

    pub fn youtube_music_oauth_path(&self) -> PathBuf {
        self.app_config
            .youtube
            .oauth_file
            .clone()
            .unwrap_or_else(|| self.config_folder.join("youtube").join("oauth.json"))
    }

    pub fn listenbrainz_token_path(&self) -> PathBuf {
        self.config_folder.join("listenbrainz").join("token.txt")
    }

    /// Replace only a token already validated by the caller; failed writes retain the old file.
    pub(crate) fn save_listenbrainz_token(&self, token: &str) -> anyhow::Result<()> {
        use std::io::Write;
        let token = token.trim();
        anyhow::ensure!(!token.is_empty(), "ListenBrainz token is empty.");
        let path = self.listenbrainz_token_path();
        std::fs::create_dir_all(path.parent().expect("token parent"))?;
        atomicwrites::AtomicFile::new(&path, atomicwrites::AllowOverwrite).write(|file| {
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                file.set_permissions(std::fs::Permissions::from_mode(0o600))?;
            }
            file.write_all(token.as_bytes())?;
            file.sync_all()
        })?;
        Ok(())
    }

    pub fn listenbrainz_token(&self) -> Option<String> {
        std::fs::read_to_string(self.listenbrainz_token_path())
            .ok()
            .map(|token| token.trim().to_string())
            .filter(|token| !token.is_empty())
            .or_else(|| std::env::var("LISTENBRAINZ_TOKEN").ok())
    }

    pub fn youtube_music_auth_status(&self) -> YouTubeMusicAuthStatus {
        let auth_type = self.app_config.youtube.auth_type;
        let credential_path = match auth_type {
            YouTubeMusicAuthType::Browser => Some(self.youtube_music_cookie_path()),
            YouTubeMusicAuthType::OAuth => Some(self.youtube_music_oauth_path()),
            YouTubeMusicAuthType::Unauthenticated => None,
        };
        let ready = credential_path.as_ref().is_some_and(|path| path.is_file())
            && auth_type != YouTubeMusicAuthType::Unauthenticated;

        YouTubeMusicAuthStatus {
            auth_type,
            credential_path,
            ready,
        }
    }
}

#[derive(Debug, Deserialize, Serialize, ConfigParse)]
#[allow(clippy::struct_excessive_bools)]
/// Application configurations
pub struct AppConfig {
    pub active_provider: ActiveProvider,
    pub youtube: YouTubeMusicConfig,
    pub presentation: PresentationConfig,
    pub lyrics: LyricsConfig,
    pub session_history: SessionHistoryConfig,
    pub listenbrainz: ListenBrainzConfig,

    pub theme: String,
    pub client_id: String,
    pub client_id_command: Option<Command>,

    pub client_port: u16,

    pub login_redirect_uri: String,

    pub log_folder: Option<PathBuf>,

    pub player_event_hook_command: Option<Command>,

    pub playback_format: String,
    pub terminal_title: String,
    pub terminal_title_idle: String,
    pub playback_metadata_fields: Vec<String>,
    #[cfg(feature = "notify")]
    pub notify_format: NotifyFormat,
    #[cfg(feature = "notify")]
    pub notify_timeout_in_secs: u64,
    #[cfg(feature = "notify")]
    #[cfg(all(unix, not(target_os = "macos")))]
    pub notify_transient: bool,

    pub tracks_playback_limit: usize,

    // session configs
    pub proxy: Option<String>,
    pub ap_port: Option<u16>,

    // duration configs
    pub app_refresh_duration_in_ms: u64,
    pub playback_refresh_duration_in_ms: u64,

    pub page_size_in_rows: usize,

    // icon configs
    pub play_icon: String,
    pub pause_icon: String,
    pub liked_icon: String,
    pub explicit_icon: String,
    pub volume_icon: String,

    // layout configs
    pub border_type: BorderType,
    pub progress_bar_type: ProgressBarType,
    pub progress_bar_position: ProgressBarPosition,

    pub layout: LayoutConfig,

    pub genre_num: u8,

    #[cfg(feature = "image")]
    pub cover_img_length: usize,
    #[cfg(feature = "image")]
    pub cover_img_width: usize,
    #[cfg(feature = "pixelate")]
    pub cover_img_pixels: u32,

    #[cfg(feature = "media-control")]
    pub enable_media_control: bool,

    pub enable_streaming: StreamingType,

    #[cfg(feature = "streaming")]
    pub enable_audio_visualization: bool,

    #[cfg(feature = "notify")]
    pub enable_notify: bool,

    pub enable_cover_image_cache: bool,

    pub device: DeviceConfig,

    #[cfg(all(feature = "streaming", feature = "notify"))]
    pub notify_streaming_only: bool,

    pub seek_duration_secs: u16,

    pub sort_artist_albums_by_type: bool,

    pub volume_scroll_step: u8,
    pub enable_mouse_scroll_volume: bool,
    pub enable_mouse_navigation: bool,

    /// Enable app-managed queue for full playlist playback.
    /// Requires streaming. When disabled, playback uses Spotify-native queue
    /// management.
    pub custom_queue: bool,

    pub enable_relative_line_number: bool,

    /// Start the application with playback paused instead of resuming the
    /// previous session. Requires streaming. When the integrated client
    /// connects on startup, Spotify may restore and auto-resume the last
    /// playing track; enabling this pauses that auto-started playback once.
    #[cfg(feature = "streaming")]
    pub pause_on_startup: bool,
}

#[derive(Debug, Deserialize, Serialize, Clone, Copy, PartialEq, Eq)]
pub enum ActiveProvider {
    Spotify,
    YouTubeMusic,
}
config_parser_impl!(ActiveProvider);

/// Controls how much secondary metadata is shown when the terminal is in
/// compact mode.  Normal and wide layouts keep their semantic columns.
#[derive(Debug, Deserialize, Serialize, Clone, Copy, PartialEq, Eq)]
pub enum CompactMetadataMode {
    Minimal,
    Balanced,
    Detailed,
}
config_parser_impl!(CompactMetadataMode);

impl CompactMetadataMode {
    pub const fn shows_artists(self) -> bool {
        !matches!(self, Self::Minimal)
    }

    pub const fn shows_album(self) -> bool {
        !matches!(self, Self::Minimal)
    }

    pub const fn shows_added_at(self) -> bool {
        matches!(self, Self::Detailed)
    }
}

/// Named presets for the presentation controls. `Custom` keeps the explicit
/// metadata and journal fields authoritative, preserving older configurations
/// that did not have a profile field.
#[derive(Debug, Deserialize, Serialize, Clone, Copy, PartialEq, Eq)]
pub enum PresentationProfile {
    Minimal,
    Balanced,
    Detailed,
    Custom,
}
config_parser_impl!(PresentationProfile);

/// Controls how overflowing text in the focused row is presented.
#[derive(Debug, Deserialize, Serialize, Clone, Copy, PartialEq, Eq)]
pub enum FocusedRowOverflow {
    Truncate,
    /// Scroll overflowing text continuously.
    Marquee,
    /// Keep overflowing text still; Left/Right scroll it one character.
    Manual,
}
config_parser_impl!(FocusedRowOverflow);

impl FocusedRowOverflow {
    /// Whether the focused row shows more than its truncated text.
    pub const fn scrolls(self) -> bool {
        !matches!(self, Self::Truncate)
    }
}

#[derive(Debug, Deserialize, Serialize, ConfigParse, Clone, PartialEq, Eq)]
pub struct PresentationConfig {
    #[serde(default)]
    pub layout_preset: LayoutPreset,
    pub profile: PresentationProfile,
    pub compact_metadata: CompactMetadataMode,
    /// Ordered journal state tokens: listened, `listen_later`, rating, note.
    pub journal_indicators: Vec<String>,
    pub focused_row_overflow: FocusedRowOverflow,
}

/// Frame chrome can change independently from metadata density and theme colors.
#[derive(Debug, Default, Deserialize, Serialize, Clone, Copy, PartialEq, Eq)]
pub enum LayoutPreset {
    #[default]
    Current,
    Borderless,
}
config_parser_impl!(LayoutPreset);

/// Only application-frame preferences are projected into the live UI.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FrameLayoutConfig {
    pub playback_window_position: Position,
    pub playback_window_height: usize,
    pub border_type: BorderType,
}

impl From<&AppConfig> for FrameLayoutConfig {
    fn from(config: &AppConfig) -> Self {
        Self {
            playback_window_position: config.layout.playback_window_position,
            playback_window_height: config.layout.playback_window_height,
            border_type: config.border_type.clone(),
        }
    }
}

impl Default for FrameLayoutConfig {
    fn default() -> Self {
        let layout = LayoutConfig::default();
        Self {
            playback_window_position: layout.playback_window_position,
            playback_window_height: layout.playback_window_height,
            border_type: BorderType::Plain,
        }
    }
}

/// External lyrics providers used after provider-native lyrics are unavailable.
/// The fixed fallback order is `SimpMusic`, LRCLIB, Lyrics.ovh, then Musixmatch; this list
/// only controls which optional providers are allowed to participate.
#[derive(Debug, Deserialize, Serialize, ConfigParse, Clone, PartialEq, Eq)]
pub struct LyricsConfig {
    pub providers: Vec<String>,
}

/// Local-only listening history used for future generated playlists and
/// personal review. This is deliberately bounded and never sent to providers.
#[derive(Debug, Deserialize, Serialize, ConfigParse, Clone, PartialEq, Eq)]
pub struct SessionHistoryConfig {
    pub enabled: bool,
    pub max_entries: usize,
}

/// Optional `ListenBrainz` integrations. The master switch is privacy-first and
/// must be enabled before an in-app provider request is made.
#[derive(Debug, Deserialize, Serialize, ConfigParse, Clone, PartialEq, Eq)]
pub struct ListenBrainzConfig {
    pub enabled: bool,
    pub read_only_checking: bool,
    pub artist_enrichment: bool,
}

impl Default for ListenBrainzConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            read_only_checking: true,
            artist_enrichment: true,
        }
    }
}

impl Default for SessionHistoryConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            max_entries: 200,
        }
    }
}

pub const LYRICS_PROVIDER_ORDER: [&str; 4] = ["simpmusic", "lrclib", "lyricsovh", "musixmatch"];

impl Default for LyricsConfig {
    fn default() -> Self {
        Self {
            providers: vec![
                "simpmusic".to_string(),
                "lrclib".to_string(),
                "musixmatch".to_string(),
            ],
        }
    }
}

impl LyricsConfig {
    pub fn provider_enabled(&self, provider: &str) -> bool {
        self.providers.iter().any(|candidate| candidate == provider)
    }

    pub fn enabled_provider_order(&self) -> Vec<&'static str> {
        LYRICS_PROVIDER_ORDER
            .into_iter()
            .filter(|provider| self.provider_enabled(provider))
            .collect()
    }
}

impl Default for PresentationConfig {
    fn default() -> Self {
        Self {
            layout_preset: LayoutPreset::Current,
            profile: PresentationProfile::Custom,
            compact_metadata: CompactMetadataMode::Minimal,
            journal_indicators: vec![
                "listened".to_string(),
                "listen_later".to_string(),
                "rating".to_string(),
                "note".to_string(),
            ],
            focused_row_overflow: FocusedRowOverflow::Truncate,
        }
    }
}

impl PresentationConfig {
    /// Resolve a named preset into the concrete preferences consumed by UI
    /// renderers. Keeping this policy in one place prevents individual pages
    /// from inventing their own meaning for a profile.
    pub fn effective(&self) -> Self {
        let mut effective = self.clone();
        match self.profile {
            PresentationProfile::Minimal => {
                effective.compact_metadata = CompactMetadataMode::Minimal;
                effective.journal_indicators = vec!["listened".to_string()];
            }
            PresentationProfile::Balanced => {
                effective.compact_metadata = CompactMetadataMode::Balanced;
                effective.journal_indicators = vec![
                    "listened".to_string(),
                    "listen_later".to_string(),
                    "rating".to_string(),
                ];
            }
            PresentationProfile::Detailed => {
                effective.compact_metadata = CompactMetadataMode::Detailed;
                effective.journal_indicators = vec![
                    "listened".to_string(),
                    "listen_later".to_string(),
                    "rating".to_string(),
                    "note".to_string(),
                ];
            }
            PresentationProfile::Custom => {}
        }
        effective
    }
}

impl ActiveProvider {
    pub fn title(self) -> &'static str {
        match self {
            Self::Spotify => "Spotify",
            Self::YouTubeMusic => "YouTube Music",
        }
    }

    pub fn toggled(self) -> Self {
        match self {
            Self::Spotify => Self::YouTubeMusic,
            Self::YouTubeMusic => Self::Spotify,
        }
    }
}

#[derive(Debug, Deserialize, Serialize, Clone, Copy, PartialEq, Eq)]
pub enum YouTubeMusicAuthType {
    Browser,
    OAuth,
    Unauthenticated,
}
config_parser_impl!(YouTubeMusicAuthType);

#[derive(Debug, Deserialize, Serialize, Clone, Copy, PartialEq, Eq)]
pub enum YouTubePlaybackQuality {
    High,
    DataSaver,
}
config_parser_impl!(YouTubePlaybackQuality);

/// JavaScript runtime used for `YouTube` player challenge solving.
///
/// The selected runtime is initialized when the `YouTube` resolver starts, so
/// changing this setting requires an application restart. `QuickJS` falls
/// back to Node when it fails or was not compiled in.
#[derive(Debug, Deserialize, Serialize, Clone, Copy, PartialEq, Eq)]
pub enum YouTubeJavaScriptRuntime {
    Auto,
    Node,
    QuickJs,
}
config_parser_impl!(YouTubeJavaScriptRuntime);

#[derive(Debug, Deserialize, Serialize, ConfigParse, Clone)]
pub struct YouTubeMusicConfig {
    pub auth_type: YouTubeMusicAuthType,
    pub cookie_file: Option<PathBuf>,
    pub oauth_file: Option<PathBuf>,
    pub po_token_file: Option<PathBuf>,
    pub playback_quality: YouTubePlaybackQuality,
    pub native_audio_cache_size_mb: usize,
    pub javascript_runtime: YouTubeJavaScriptRuntime,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct YouTubeMusicAuthStatus {
    pub auth_type: YouTubeMusicAuthType,
    pub credential_path: Option<PathBuf>,
    pub ready: bool,
}

impl YouTubeMusicAuthStatus {
    pub fn is_ready(&self) -> bool {
        self.ready
    }

    pub fn label(&self) -> String {
        match (self.auth_type, self.ready) {
            (YouTubeMusicAuthType::Browser, true) => "Browser auth ready".to_string(),
            (YouTubeMusicAuthType::Browser, false) => "Browser auth missing".to_string(),
            (YouTubeMusicAuthType::OAuth, true) => "OAuth ready".to_string(),
            (YouTubeMusicAuthType::OAuth, false) => "OAuth missing".to_string(),
            (YouTubeMusicAuthType::Unauthenticated, _) => "Unauthenticated disabled".to_string(),
        }
    }

    pub fn missing_message(&self) -> Option<String> {
        if self.ready {
            return None;
        }

        match self.auth_type {
            YouTubeMusicAuthType::Browser => self.credential_path.as_ref().map(|path| {
                format!(
                    "YouTube Music mode requires browser auth. Save your YouTube Music Cookie header to {}",
                    path.display()
                )
            }),
            YouTubeMusicAuthType::OAuth => self.credential_path.as_ref().map(|path| {
                format!(
                    "YouTube Music mode requires OAuth auth. Save the OAuth token JSON to {}",
                    path.display()
                )
            }),
            YouTubeMusicAuthType::Unauthenticated => Some(
                "YouTube Music mode requires login; unauthenticated mode is disabled".to_string(),
            ),
        }
    }
}

impl Default for YouTubeMusicAuthStatus {
    fn default() -> Self {
        Self {
            auth_type: YouTubeMusicAuthType::Browser,
            credential_path: None,
            ready: false,
        }
    }
}

#[derive(Debug, Deserialize, Serialize, Clone, Copy, PartialEq, Eq)]
pub enum Position {
    Top,
    Bottom,
}
config_parser_impl!(Position);

#[derive(Debug, Deserialize, Serialize, Clone, PartialEq, Eq)]
pub enum BorderType {
    Hidden,
    Plain,
    Rounded,
    Double,
    Thick,
}
config_parser_impl!(BorderType);

#[derive(Debug, Deserialize, Serialize, Clone)]
pub enum ProgressBarType {
    Line,
    Rectangle,
}
config_parser_impl!(ProgressBarType);

#[derive(Debug, Deserialize, Serialize, Clone)]
pub enum ProgressBarPosition {
    Bottom,
    Right,
}
config_parser_impl!(ProgressBarPosition);

#[derive(Debug, Deserialize, Serialize, ConfigParse, Clone)]
pub struct Command {
    pub command: String,
    #[serde(default)]
    pub args: Vec<String>,
}

impl Command {
    /// Execute a command, returning stdout if succeeded or stderr if failed
    pub fn execute(&self, extra_args: Option<Vec<String>>) -> anyhow::Result<String> {
        let mut args = self.args.clone();
        args.extend(extra_args.unwrap_or_default());

        let output = std::process::Command::new(&self.command)
            .args(&args)
            .output()?;

        if !output.status.success() {
            let stderr = std::str::from_utf8(&output.stderr)?.to_string();
            anyhow::bail!(stderr);
        }

        let stdout = std::str::from_utf8(&output.stdout)?.to_string();
        Ok(stdout)
    }
}

#[derive(Debug, Deserialize, Serialize, ConfigParse, Clone)]
/// Application device configurations
pub struct DeviceConfig {
    pub name: String,
    pub device_type: String,
    pub volume: u8,
    pub bitrate: u16,
    pub audio_cache: bool,
    pub normalization: bool,
    pub autoplay: bool,
}

#[derive(Debug, Deserialize, Serialize, ConfigParse, Clone)]
#[cfg(feature = "notify")]
pub struct NotifyFormat {
    pub summary: String,
    pub body: String,
}

#[derive(Debug, Deserialize, Serialize, ConfigParse, Clone)]
// Application layout configurations
pub struct LayoutConfig {
    pub library: LibraryLayoutConfig,
    pub playback_window_position: Position,
    pub playback_window_height: usize,
}

#[derive(Debug, Deserialize, Serialize, ConfigParse, Clone)]
pub struct LibraryLayoutConfig {
    pub playlist_percent: u16,
    pub album_percent: u16,
}

#[allow(dead_code)]
#[derive(Debug, Deserialize, Serialize, Clone, PartialEq)]
#[serde(from = "StreamingTypeOrBool")]
pub enum StreamingType {
    Always,
    DaemonOnly,
    Never,
}
config_parser_impl!(StreamingType);

// For backward compatibility, to accept booleans for enable_streaming
#[derive(Deserialize)]
enum RawStreamingType {
    Always,
    DaemonOnly,
    Never,
}

#[allow(dead_code)]
#[derive(Deserialize)]
#[serde(untagged)]
enum StreamingTypeOrBool {
    Bool(bool),
    Type(RawStreamingType),
}

impl From<StreamingTypeOrBool> for StreamingType {
    fn from(v: StreamingTypeOrBool) -> Self {
        match v {
            StreamingTypeOrBool::Bool(true)
            | StreamingTypeOrBool::Type(RawStreamingType::Always) => StreamingType::Always,
            StreamingTypeOrBool::Bool(false)
            | StreamingTypeOrBool::Type(RawStreamingType::Never) => StreamingType::Never,
            StreamingTypeOrBool::Type(RawStreamingType::DaemonOnly) => StreamingType::DaemonOnly,
        }
    }
}

impl Command {
    pub fn new<C, A>(command: C, args: &[A]) -> Self
    where
        C: std::fmt::Display,
        A: std::fmt::Display,
    {
        Self {
            command: command.to_string(),
            args: args.iter().map(std::string::ToString::to_string).collect(),
        }
    }
}

impl Default for AppConfig {
    fn default() -> Self {
        Self {
            active_provider: ActiveProvider::Spotify,
            youtube: YouTubeMusicConfig::default(),
            presentation: PresentationConfig::default(),
            lyrics: LyricsConfig::default(),
            session_history: SessionHistoryConfig::default(),
            listenbrainz: ListenBrainzConfig::default(),

            theme: "default".to_owned(),
            // Use ncspot's client ID as a fallback for user-provided client ID
            //
            // Most of the time, using ncspot's client ID is better than user-provided one
            // because it is registered with [extended quota mode] and predates [spotify API changes]
            //
            // [extended quota mode]: https://developer.spotify.com/documentation/web-api/concepts/quota-modes
            // [spotify API changes]: https://developer.spotify.com/blog/2024-11-27-changes-to-the-web-api
            client_id: NCSPOT_CLIENT_ID.to_string(),
            client_id_command: None,

            client_port: 8080,

            login_redirect_uri: "http://127.0.0.1:8989/login".to_string(),

            log_folder: None,

            tracks_playback_limit: 50,

            playback_format: String::from(
                "{status} {track} • {artists} {liked}\n{album} • {genres}\n{metadata}",
            ),
            terminal_title: String::from("{status} {track} · {artists} — Unified Player"),
            terminal_title_idle: String::from("{page} — Unified Player"),
            playback_metadata_fields: vec![
                "repeat".to_string(),
                "shuffle".to_string(),
                "volume".to_string(),
                "device".to_string(),
            ],
            #[cfg(feature = "notify")]
            notify_format: NotifyFormat {
                summary: String::from("{track} • {artists}"),
                body: String::from("{album}"),
            },
            #[cfg(feature = "notify")]
            notify_timeout_in_secs: 0,
            #[cfg(feature = "notify")]
            #[cfg(all(unix, not(target_os = "macos")))]
            notify_transient: false,

            player_event_hook_command: None,

            proxy: None,
            ap_port: None,
            app_refresh_duration_in_ms: 32,
            // Keep the playback/device projection fresh when playback changes
            // from another Spotify client, including a phone. Users may still
            // explicitly set this to zero to restore event/command-only refresh.
            playback_refresh_duration_in_ms: 4_000,

            page_size_in_rows: 20,

            pause_icon: "▌▌".to_string(),
            play_icon: "▶".to_string(),
            liked_icon: "♥".to_string(),
            explicit_icon: "(E)".to_string(),
            volume_icon: "🔊".to_string(),

            border_type: BorderType::Plain,
            progress_bar_type: ProgressBarType::Rectangle,
            progress_bar_position: ProgressBarPosition::Bottom,

            layout: LayoutConfig::default(),

            genre_num: 2,

            // `0` means "auto": derive the cover's column count from the terminal's cell aspect ratio
            #[cfg(feature = "image")]
            cover_img_length: 0,
            #[cfg(feature = "image")]
            cover_img_width: 5,
            #[cfg(feature = "pixelate")]
            cover_img_pixels: 16,

            // Because of the "creating new window and stealing focus" behaviour
            // when running the media control event loop on startup,
            // media control support is disabled by default for Windows and MacOS.
            // Users will need to explicitly enable this option in their configuration files.
            #[cfg(feature = "media-control")]
            #[cfg(any(target_os = "macos", target_os = "windows"))]
            enable_media_control: false,
            #[cfg(feature = "media-control")]
            #[cfg(all(unix, not(target_os = "macos")))]
            enable_media_control: true,

            enable_streaming: StreamingType::Always,

            #[cfg(feature = "streaming")]
            enable_audio_visualization: false,

            #[cfg(feature = "notify")]
            enable_notify: true,

            enable_cover_image_cache: true,

            device: DeviceConfig::default(),

            #[cfg(all(feature = "streaming", feature = "notify"))]
            notify_streaming_only: false,

            seek_duration_secs: 5,

            sort_artist_albums_by_type: false,

            volume_scroll_step: 5,
            enable_mouse_scroll_volume: true,
            enable_mouse_navigation: true,

            custom_queue: true,

            enable_relative_line_number: false,

            #[cfg(feature = "streaming")]
            pause_on_startup: false,
        }
    }
}

impl Default for YouTubeMusicConfig {
    fn default() -> Self {
        Self {
            auth_type: YouTubeMusicAuthType::Browser,
            cookie_file: None,
            oauth_file: None,
            po_token_file: None,
            playback_quality: YouTubePlaybackQuality::High,
            native_audio_cache_size_mb: 16,
            javascript_runtime: YouTubeJavaScriptRuntime::Auto,
        }
    }
}

impl Default for DeviceConfig {
    fn default() -> Self {
        Self {
            name: "unified-player".to_string(),
            device_type: "speaker".to_string(),
            volume: 70,
            bitrate: 320,
            audio_cache: false,
            normalization: false,
            autoplay: false,
        }
    }
}

impl Default for LayoutConfig {
    fn default() -> Self {
        Self {
            library: LibraryLayoutConfig {
                playlist_percent: 40,
                album_percent: 40,
            },
            playback_window_position: Position::Top,
            playback_window_height: 6,
        }
    }
}

impl LayoutConfig {
    fn check_values(&self) -> anyhow::Result<()> {
        if self.library.album_percent + self.library.playlist_percent > 99 {
            anyhow::bail!("Invalid library layout: summation of album_percent and playlist_percent cannot be greater than 99!");
        }
        Ok(())
    }
}

/// Shortest UI frame interval, about 120 FPS. Content that animates on every
/// frame (the audio visualizer) would otherwise redraw in a tight loop.
const MIN_UI_FRAME_INTERVAL: std::time::Duration = std::time::Duration::from_millis(8);

impl AppConfig {
    /// Minimum time between UI redraws, from `app_refresh_duration_in_ms`.
    pub fn ui_frame_interval(&self) -> std::time::Duration {
        std::time::Duration::from_millis(self.app_refresh_duration_in_ms).max(MIN_UI_FRAME_INTERVAL)
    }

    pub fn new(path: &Path) -> Result<Self> {
        let mut config = Self::default();
        if !config.parse_config_file(path)? {
            config.write_config_file(path)?;
        }

        config.layout.check_values()?;
        Ok(config)
    }

    // parses configurations from an application config file in `path` folder,
    // then updates the current configurations accordingly.
    // returns false if no config file found and true otherwise
    fn parse_config_file(&mut self, path: &Path) -> Result<bool> {
        let file_path = path.join(APP_CONFIG_FILE);
        match std::fs::read_to_string(file_path) {
            Ok(content) => {
                let mut value = toml::from_str::<toml::Value>(&content)?;
                let had_legacy_youtube_command = value
                    .get("youtube")
                    .and_then(|youtube| youtube.get("yt_dlp_command"))
                    .is_some();
                if had_legacy_youtube_command {
                    tracing::warn!(
                        "`youtube.yt_dlp_command` is deprecated and ignored; YouTube playback is fully in-process"
                    );
                    if let Some(youtube) =
                        value.get_mut("youtube").and_then(toml::Value::as_table_mut)
                    {
                        youtube.remove("yt_dlp_command");
                    }
                }
                self.parse(value).map(|()| true)
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(false),
            Err(error) => Err(error.into()),
        }
    }

    fn write_config_file(&self, path: &Path) -> Result<()> {
        toml::to_string_pretty(&self)
            .map_err(From::from)
            .and_then(|content| {
                std::fs::write(path.join(APP_CONFIG_FILE), content).map_err(From::from)
            })
    }

    pub fn session_config(&self) -> SessionConfig {
        let proxy = self
            .proxy
            .as_ref()
            .and_then(|proxy| match Url::parse(proxy) {
                Err(err) => {
                    tracing::warn!(
                        diagnostic = %crate::observability::safe_error(
                            crate::observability::DiagnosticCode::CONFIG_PROXY_INVALID,
                            crate::observability::ErrorCategory::Contract,
                            &err,
                        ),
                        "Configured proxy URL is invalid; proxying is disabled"
                    );
                    None
                }
                Ok(url) => Some(url),
            });
        SessionConfig {
            proxy,
            ap_port: self.ap_port,
            client_id: SPOTIFY_CLIENT_ID.to_string(),
            autoplay: Some(self.device.autoplay),
            ..Default::default()
        }
    }

    /// Returns stdout of `client_id_command` if set, otherwise the value of `client_id`.
    pub fn get_client_id(&self) -> Result<String> {
        match self.client_id_command {
            Some(ref cmd) => cmd.execute(None).map(|out| out.trim().to_string()),
            None => Ok(self.client_id.clone()),
        }
    }
}

/// gets the application's configuration folder path
pub fn get_config_folder_path() -> Result<PathBuf> {
    match dirs_next::home_dir() {
        Some(home) => Ok(home.join(DEFAULT_CONFIG_FOLDER)),
        None => Err(anyhow!("cannot find the $HOME folder")),
    }
}

/// gets the application's cache folder path
pub fn get_cache_folder_path() -> Result<PathBuf> {
    match dirs_next::home_dir() {
        Some(home) => Ok(home.join(DEFAULT_CACHE_FOLDER)),
        None => Err(anyhow!("cannot find the $HOME folder")),
    }
}

pub fn get_config() -> &'static Configs {
    CONFIGS.get().expect("configs is already initialized")
}
pub fn set_config(configs: Configs) {
    CONFIGS
        .set(configs)
        .expect("configs should be initialized only once");
}

// Apply a CLI config override to the application config.
// Serializes the config to TOML, navigates to the key via dot-notation,
// overrides the value, and deserializes back into AppConfig.
// Returns an error if the key path is invalid or the value type mismatches.
pub fn apply_config_override(config: &mut AppConfig, key: &str, value: &str) -> anyhow::Result<()> {
    let mut config_value = toml::Value::try_from(&*config)?;

    let parts: Vec<&str> = key.split('.').collect();
    let mut current = &mut config_value;

    for (i, part) in parts.iter().enumerate() {
        if i == parts.len() - 1 {
            let table = current
                .as_table_mut()
                .context(format!("'{key}' is not a valid config path"))?;

            let parsed_value: toml::Value = value
                .parse()
                .unwrap_or_else(|_| toml::Value::String(value.to_string()));

            table.insert(part.to_string(), parsed_value);
        } else {
            current = current
                .get_mut(part)
                .context(format!("Config key '{part}' not found in path '{key}'"))?;
        }
    }

    *config = config_value.try_into()?;

    Ok(())
}

static SETTINGS_READ_ONLY: std::sync::atomic::AtomicBool =
    std::sync::atomic::AtomicBool::new(false);

/// Stops this process from saving settings, for offline previews that must not
/// change the user's `app.toml`. Changes still apply in memory.
pub(crate) fn make_settings_read_only() {
    SETTINGS_READ_ONLY.store(true, std::sync::atomic::Ordering::Relaxed);
}

pub fn save_app_config_override(
    config_folder: &Path,
    key: &str,
    value: &str,
) -> anyhow::Result<()> {
    if SETTINGS_READ_ONLY.load(std::sync::atomic::Ordering::Relaxed) {
        anyhow::bail!("settings are not saved in a preview");
    }
    if key == "client_id" {
        return save_welcome_spotify_client(config_folder, value);
    }
    let mut config = AppConfig::new(config_folder)?;
    apply_config_override(&mut config, key, value)?;
    config.write_config_file(config_folder)?;
    Ok(())
}

/// Persist an explicit Welcome choice, replacing command-based ID resolution.
/// Mark reauthorization first so a partial write cannot reuse an incompatible token.
pub fn save_welcome_spotify_client(config_folder: &Path, client_id: &str) -> Result<()> {
    let client_id = client_id.trim();
    anyhow::ensure!(
        client_id.len() == 32 && client_id.bytes().all(|byte| byte.is_ascii_hexdigit()),
        "Enter a 32-character hexadecimal Spotify client ID, not a client secret."
    );
    let mut app = AppConfig::new(config_folder)?;
    if app.client_id == client_id && app.client_id_command.is_none() {
        return Ok(());
    }
    let mut setup = SetupState::load(config_folder)?;
    setup.spotify_reauthentication_required = true;
    setup.status = SetupStatus::Pending;
    setup.failure = None;
    setup.save(config_folder)?;
    client_id.clone_into(&mut app.client_id);
    app.client_id_command = None;
    app.write_config_file(config_folder)
}

/// Delete every saved preference and account file so the Welcome wizard can
/// start fresh. The journal and session history live in the config folder and
/// are included. Missing paths are not errors.
pub fn reset_all_configuration(
    config_folder: &Path,
    cache_folder: &Path,
    youtube_cookie_path: &Path,
) -> Result<()> {
    remove_all_entries(config_folder).context("reset configuration folder")?;
    for provider in [ActiveProvider::Spotify, ActiveProvider::YouTubeMusic] {
        accounts::remove_canonical_files(provider, cache_folder, youtube_cookie_path)
            .context("reset cached account session")?;
    }
    Ok(())
}

fn remove_all_entries(folder: &Path) -> Result<()> {
    let entries = match std::fs::read_dir(folder) {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(error).context("read configuration folder"),
    };
    for entry in entries {
        let path = entry.context("read configuration entry")?.path();
        if path.is_dir() {
            std::fs::remove_dir_all(&path).context("remove configuration directory")?;
        } else {
            match std::fs::remove_file(&path) {
                Ok(()) => {}
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                Err(error) => return Err(error).context("remove configuration file"),
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod welcome_spotify_client_tests {
    use super::*;

    #[test]
    fn welcome_client_change_persists_and_requires_fresh_auth() {
        let folder = tempfile::tempdir().unwrap();
        let mut app = AppConfig::default();
        app.client_id_command = Some(Command {
            command: "unused".to_owned(),
            args: vec![],
        });
        app.write_config_file(folder.path()).unwrap();
        let custom = "0123456789abcdef0123456789abcdef";
        save_welcome_spotify_client(folder.path(), custom).unwrap();
        let restored = AppConfig::new(folder.path()).unwrap();
        assert_eq!(restored.client_id, custom);
        assert!(restored.client_id_command.is_none());
        let setup = SetupState::load(folder.path()).unwrap();
        assert!(setup.requires_attention());
        assert!(setup.spotify_reauthentication_required);
        assert!(!setup.ready_for(SetupAuthSnapshot {
            spotify: SpotifyAuthSnapshot {
                session_ready: true,
                premium: SpotifyPremiumStatus::Premium
            },
            ..Default::default()
        }));
        save_welcome_spotify_client(folder.path(), NCSPOT_CLIENT_ID).unwrap();
        assert_eq!(
            AppConfig::new(folder.path()).unwrap().client_id,
            NCSPOT_CLIENT_ID
        );
    }

    #[test]
    fn welcome_invalid_client_does_not_write_configuration() {
        let folder = tempfile::tempdir().unwrap();
        for value in [
            "",
            "secret",
            "https://example.com",
            "zzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzz",
        ] {
            assert!(save_welcome_spotify_client(folder.path(), value).is_err());
        }
        assert!(!folder.path().join(APP_CONFIG_FILE).exists());
        assert!(!folder.path().join("setup.toml").exists());
    }

    #[test]
    fn welcome_unchanged_bundled_client_preserves_setup() {
        let folder = tempfile::tempdir().unwrap();
        save_welcome_spotify_client(folder.path(), NCSPOT_CLIENT_ID).unwrap();
        assert!(
            !SetupState::load(folder.path())
                .unwrap()
                .spotify_reauthentication_required
        );
    }
}

#[cfg(test)]
mod reset_all_configuration_tests {
    use super::*;

    #[test]
    fn reset_removes_saved_files_and_tolerates_missing_paths() {
        let root = tempfile::tempdir().unwrap();
        let config = root.path().join("config");
        let cache = root.path().join("cache");
        let cookie = config.join("youtube").join("cookie.txt");
        for dir in [&config, &cache, cookie.parent().unwrap()] {
            std::fs::create_dir_all(dir).unwrap();
        }
        for file in [
            config.join(APP_CONFIG_FILE),
            config.join(THEME_CONFIG_FILE),
            config.join(KEYMAP_CONFIG_FILE),
            config.join("setup.toml"),
            config.join("accounts.toml"),
            config.join("journal.json"),
            config.join("session-history.json"),
            cookie.clone(),
            cache.join("user_client_token.json"),
            cache.join("credentials.json"),
        ] {
            std::fs::write(&file, b"saved").unwrap();
        }
        let profile = config.join("youtube").join("browser-profile");
        std::fs::create_dir_all(&profile).unwrap();
        std::fs::write(profile.join("data.bin"), b"saved").unwrap();
        let slot = config.join("accounts").join("spotify").join("one");
        std::fs::create_dir_all(&slot).unwrap();
        std::fs::write(slot.join("session.json"), b"saved").unwrap();

        reset_all_configuration(&config, &cache, &cookie).unwrap();
        assert!(std::fs::read_dir(&config).unwrap().next().is_none());
        assert!(!cache.join("user_client_token.json").exists());
        assert!(!cache.join("credentials.json").exists());

        // Missing paths are not errors.
        reset_all_configuration(&config, &cache, &cookie).unwrap();
        let missing = root.path().join("missing");
        reset_all_configuration(&missing, &missing, &missing.join("cookie.txt")).unwrap();
    }
}

pub fn app_config_settings(config_folder: &Path) -> anyhow::Result<Vec<AppConfigSetting>> {
    let config = AppConfig::new(config_folder)?;
    let value = toml::Value::try_from(&config)?;
    let mut settings = Vec::new();
    flatten_app_config_settings(None, &value, &mut settings);
    project_effective_presentation_settings(&mut settings, &config.presentation);
    let youtube_credential_path = match config.youtube.auth_type {
        YouTubeMusicAuthType::Browser => Some(
            config
                .youtube
                .cookie_file
                .clone()
                .unwrap_or_else(|| config_folder.join("youtube").join("cookie.txt")),
        ),
        YouTubeMusicAuthType::OAuth => Some(
            config
                .youtube
                .oauth_file
                .clone()
                .unwrap_or_else(|| config_folder.join("youtube").join("oauth.json")),
        ),
        YouTubeMusicAuthType::Unauthenticated => None,
    };
    let youtube_auth_status = YouTubeMusicAuthStatus {
        auth_type: config.youtube.auth_type,
        ready: youtube_credential_path
            .as_ref()
            .is_some_and(|path| path.is_file())
            && config.youtube.auth_type != YouTubeMusicAuthType::Unauthenticated,
        credential_path: youtube_credential_path,
    };
    let registry = AccountRegistry::load(config_folder)?;
    let cache_folder = CONFIGS.get().map_or_else(
        || config_folder.to_path_buf(),
        |configs| configs.cache_folder.clone(),
    );
    let youtube_cookie_path = config
        .youtube
        .cookie_file
        .clone()
        .unwrap_or_else(|| config_folder.join("youtube").join("cookie.txt"));
    let spotify_accounts = registry.summaries(
        ActiveProvider::Spotify,
        config_folder,
        &cache_folder,
        &youtube_cookie_path,
    );
    let youtube_accounts = registry.summaries(
        ActiveProvider::YouTubeMusic,
        config_folder,
        &cache_folder,
        &youtube_cookie_path,
    );
    settings.extend([
        AppConfigSetting {
            section: AppConfigSection::Accounts,
            key: "accounts.spotify.active".to_string(),
            value: registry
                .active_label(ActiveProvider::Spotify)
                .unwrap_or("No account selected")
                .to_string(),
            kind: account_choice_kind(&registry, ActiveProvider::Spotify),
            restart_required: false,
        },
        AppConfigSetting {
            section: AppConfigSection::Accounts,
            key: "accounts.spotify.status".to_string(),
            value: account_status_value(&spotify_accounts),
            kind: AppConfigValueKind::Status,
            restart_required: false,
        },
        AppConfigSetting {
            section: AppConfigSection::Accounts,
            key: "accounts.spotify.add".to_string(),
            value: "Add Spotify account".to_string(),
            kind: AppConfigValueKind::Action(AppConfigAction::AddSpotifyAccount),
            restart_required: false,
        },
        AppConfigSetting {
            section: AppConfigSection::Accounts,
            key: "accounts.spotify.validate".to_string(),
            value: "Validate active Spotify session".to_string(),
            kind: AppConfigValueKind::Action(AppConfigAction::ValidateSpotifyAccount),
            restart_required: false,
        },
        AppConfigSetting {
            section: AppConfigSection::Accounts,
            key: "accounts.spotify.remove".to_string(),
            value: "Remove active Spotify account".to_string(),
            kind: AppConfigValueKind::Action(AppConfigAction::RemoveSpotifyAccount),
            restart_required: false,
        },
        AppConfigSetting {
            section: AppConfigSection::Accounts,
            key: "accounts.youtube_music.active".to_string(),
            value: registry
                .active_label(ActiveProvider::YouTubeMusic)
                .unwrap_or("No account selected")
                .to_string(),
            kind: account_choice_kind(&registry, ActiveProvider::YouTubeMusic),
            restart_required: false,
        },
        AppConfigSetting {
            section: AppConfigSection::Accounts,
            key: "accounts.youtube_music.status".to_string(),
            value: account_status_value(&youtube_accounts),
            kind: AppConfigValueKind::Status,
            restart_required: false,
        },
        AppConfigSetting {
            section: AppConfigSection::Accounts,
            key: "accounts.youtube_music.add".to_string(),
            value: "Add YouTube Music account".to_string(),
            kind: AppConfigValueKind::Action(AppConfigAction::AddYouTubeAccount),
            restart_required: false,
        },
        AppConfigSetting {
            section: AppConfigSection::Accounts,
            key: "accounts.youtube_music.validate".to_string(),
            value: "Validate active YouTube Music session".to_string(),
            kind: AppConfigValueKind::Action(AppConfigAction::ValidateYouTubeAccount),
            restart_required: false,
        },
        AppConfigSetting {
            section: AppConfigSection::Accounts,
            key: "accounts.youtube_music.remove".to_string(),
            value: "Remove active YouTube Music account".to_string(),
            kind: AppConfigValueKind::Action(AppConfigAction::RemoveYouTubeAccount),
            restart_required: false,
        },
        AppConfigSetting {
            section: AppConfigSection::SharedUi,
            key: "setup.welcome".to_string(),
            value: "Review welcome and first-use setup".to_string(),
            kind: AppConfigValueKind::Action(AppConfigAction::OpenWelcomeSetup),
            restart_required: false,
        },
        AppConfigSetting {
            section: AppConfigSection::Spotify,
            key: "spotify.auth".to_string(),
            value: "Authenticate or refresh session".to_string(),
            kind: AppConfigValueKind::Action(AppConfigAction::AuthenticateSpotify),
            restart_required: false,
        },
        AppConfigSetting {
            section: AppConfigSection::YouTubeMusic,
            key: "youtube.auth.browser_login".to_string(),
            value: "Start or cancel dedicated browser sign-in".to_string(),
            kind: AppConfigValueKind::Action(AppConfigAction::AuthenticateYouTubeBrowser),
            restart_required: false,
        },
        AppConfigSetting {
            section: AppConfigSection::YouTubeMusic,
            key: "youtube.auth.import".to_string(),
            value: youtube_auth_status.label(),
            kind: AppConfigValueKind::Action(AppConfigAction::ImportYouTubeAuth),
            restart_required: false,
        },
        AppConfigSetting {
            section: AppConfigSection::YouTubeMusic,
            key: "youtube.auth.browser_session".to_string(),
            value: if config_folder
                .join("youtube")
                .join("browser-profile")
                .is_dir()
                && config_folder
                    .join("youtube")
                    .join("browser-path.txt")
                    .is_file()
            {
                "Dedicated browser session ready".to_string()
            } else {
                "Dedicated browser session not configured".to_string()
            },
            kind: AppConfigValueKind::Status,
            restart_required: false,
        },
        AppConfigSetting {
            section: AppConfigSection::YouTubeMusic,
            key: "youtube.auth.test".to_string(),
            value: "Test metadata and playback access".to_string(),
            kind: AppConfigValueKind::Action(AppConfigAction::TestYouTubeAuth),
            restart_required: false,
        },
        AppConfigSetting {
            section: AppConfigSection::YouTubeMusic,
            key: "youtube.playback_backend".to_string(),
            value: "Native in-process HTTPS + rodio".to_string(),
            kind: AppConfigValueKind::Status,
            restart_required: false,
        },
        AppConfigSetting {
            section: AppConfigSection::YouTubeMusic,
            key: "youtube.javascript_runtime.status".to_string(),
            value: compiled_youtube_javascript_runtime_label().to_string(),
            kind: AppConfigValueKind::Status,
            restart_required: false,
        },
        AppConfigSetting {
            section: AppConfigSection::Services,
            key: "listenbrainz.auth".to_string(),
            value: if std::fs::read_to_string(config_folder.join("listenbrainz").join("token.txt"))
                .ok()
                .is_some_and(|token| !token.trim().is_empty())
                || std::env::var("LISTENBRAINZ_TOKEN").is_ok()
            {
                "Token ready".to_string()
            } else {
                "Token missing".to_string()
            },
            kind: AppConfigValueKind::Status,
            restart_required: false,
        },
        AppConfigSetting {
            section: AppConfigSection::Services,
            key: "home.history.clear".to_string(),
            value: "Clear opened collections".to_string(),
            kind: AppConfigValueKind::Action(AppConfigAction::ClearHomeHistory),
            restart_required: false,
        },
        AppConfigSetting {
            section: AppConfigSection::Diagnostics,
            key: "diagnostics.view".to_string(),
            value: "Open live diagnostics".to_string(),
            kind: AppConfigValueKind::Action(AppConfigAction::OpenLogs),
            restart_required: false,
        },
        AppConfigSetting {
            section: AppConfigSection::Diagnostics,
            key: "diagnostics.reset_all".to_string(),
            value: "Reset preferences and accounts".to_string(),
            kind: AppConfigValueKind::Action(AppConfigAction::ResetAllConfiguration),
            restart_required: false,
        },
    ]);
    settings.sort_by(|a, b| (a.section, &a.key).cmp(&(b.section, &b.key)));
    Ok(settings)
}

fn flatten_app_config_settings(
    prefix: Option<&str>,
    value: &toml::Value,
    settings: &mut Vec<AppConfigSetting>,
) {
    match value {
        toml::Value::Table(table) => {
            for (key, value) in table {
                let full_key = match prefix {
                    Some(prefix) => format!("{prefix}.{key}"),
                    None => key.clone(),
                };
                flatten_app_config_settings(Some(&full_key), value, settings);
            }
        }
        toml::Value::Boolean(value) => {
            let key = prefix.unwrap_or_default();
            settings.push(AppConfigSetting {
                section: app_config_section(key),
                key: key.to_string(),
                value: value.to_string(),
                kind: app_config_value_kind(key).unwrap_or(AppConfigValueKind::Bool),
                restart_required: app_config_key_requires_restart(key),
            });
        }
        _ => {
            let key = prefix.unwrap_or_default();
            settings.push(AppConfigSetting {
                section: app_config_section(key),
                key: key.to_string(),
                value: value.to_string().replace('\n', "\\n"),
                kind: app_config_value_kind(key).unwrap_or(AppConfigValueKind::Value),
                restart_required: app_config_key_requires_restart(key),
            });
        }
    }
}

fn project_effective_presentation_settings(
    settings: &mut [AppConfigSetting],
    presentation: &PresentationConfig,
) {
    let effective = presentation.effective();
    for setting in settings {
        match setting.key.as_str() {
            "presentation.compact_metadata" => {
                setting.value =
                    toml::Value::String(format!("{:?}", effective.compact_metadata)).to_string();
            }
            "presentation.journal_indicators" => {
                setting.value = toml::Value::Array(
                    effective
                        .journal_indicators
                        .iter()
                        .cloned()
                        .map(toml::Value::String)
                        .collect(),
                )
                .to_string();
            }
            _ => {}
        }
    }
}

fn account_choice_kind(registry: &AccountRegistry, provider: ActiveProvider) -> AppConfigValueKind {
    let labels = registry.labels(provider);
    if labels.is_empty() {
        AppConfigValueKind::Status
    } else {
        AppConfigValueKind::Choice(labels)
    }
}

fn account_status_value(accounts: &[AccountSummary]) -> String {
    match accounts {
        [] => "No saved account".to_string(),
        accounts => {
            let ready = accounts.iter().filter(|account| account.ready).count();
            let active = accounts.iter().find(|account| account.active);
            match active {
                Some(account) if account.ready => {
                    format!(
                        "{} saved; active session ready ({})",
                        accounts.len(),
                        account.label
                    )
                }
                Some(account) => format!(
                    "{} saved; active session unavailable ({})",
                    accounts.len(),
                    account.label
                ),
                None => format!(
                    "{} saved; no active account ({ready} ready)",
                    accounts.len()
                ),
            }
        }
    }
}

#[cfg(feature = "youtube-quickjs")]
fn compiled_youtube_javascript_runtime_label() -> &'static str {
    "Embedded QuickJS + Node fallback compiled"
}

#[cfg(not(feature = "youtube-quickjs"))]
fn compiled_youtube_javascript_runtime_label() -> &'static str {
    "Node only; QuickJS is not compiled"
}

fn app_config_section(key: &str) -> AppConfigSection {
    if key.starts_with("accounts.") {
        return AppConfigSection::Accounts;
    }
    if key.starts_with("youtube.") {
        return AppConfigSection::YouTubeMusic;
    }
    if matches!(
        key,
        "client_id"
            | "client_id_command"
            | "client_port"
            | "login_redirect_uri"
            | "enable_streaming"
            | "pause_on_startup"
    ) || key.starts_with("device.")
    {
        return AppConfigSection::Spotify;
    }
    if key.starts_with("playback_")
        || matches!(
            key,
            "tracks_playback_limit"
                | "seek_duration_secs"
                | "volume_scroll_step"
                | "enable_mouse_scroll_volume"
                | "enable_mouse_navigation"
                | "custom_queue"
        )
    {
        return AppConfigSection::Playback;
    }
    if key.starts_with("lyrics.") || key.starts_with("listenbrainz.") {
        return AppConfigSection::Services;
    }
    if key.starts_with("session_history.") {
        return AppConfigSection::Services;
    }
    if key.starts_with("layout.")
        || key.starts_with("presentation.")
        || matches!(
            key,
            "theme"
                | "border_type"
                | "progress_bar_type"
                | "progress_bar_position"
                | "page_size_in_rows"
                | "play_icon"
                | "pause_icon"
                | "liked_icon"
                | "explicit_icon"
                | "volume_icon"
                | "terminal_title"
                | "terminal_title_idle"
                | "enable_relative_line_number"
        )
    {
        return AppConfigSection::SharedUi;
    }
    AppConfigSection::Diagnostics
}

fn app_config_value_kind(key: &str) -> Option<AppConfigValueKind> {
    match key {
        "active_provider" => Some(choice_values(["Spotify", "YouTubeMusic"])),
        "presentation.layout_preset" => Some(choice_values(["Current", "Borderless"])),
        "presentation.profile" => {
            Some(choice_values(["Minimal", "Balanced", "Detailed", "Custom"]))
        }
        "youtube.auth_type" => Some(choice_values(["Browser", "OAuth", "Unauthenticated"])),
        "youtube.playback_quality" => Some(choice_values(["High", "DataSaver"])),
        "youtube.javascript_runtime" => Some(choice_values(["Auto", "Node", "QuickJs"])),
        "border_type" => Some(choice_values([
            "Hidden", "Plain", "Rounded", "Double", "Thick",
        ])),
        "progress_bar_type" => Some(choice_values(["Rectangle", "Line"])),
        "progress_bar_position" => Some(choice_values(["Bottom", "Right"])),
        "presentation.compact_metadata" => Some(choice_values(["Minimal", "Balanced", "Detailed"])),
        "presentation.focused_row_overflow" => {
            Some(choice_values(["Truncate", "Marquee", "Manual"]))
        }
        "presentation.journal_indicators" => Some(AppConfigValueKind::MultiChoice(
            ["listened", "listen_later", "rating", "note"]
                .into_iter()
                .map(String::from)
                .collect(),
        )),
        "lyrics.providers" => Some(AppConfigValueKind::MultiChoice(
            ["simpmusic", "lrclib", "lyricsovh", "musixmatch"]
                .into_iter()
                .map(String::from)
                .collect(),
        )),
        "layout.playback_window_position" => Some(choice_values(["Top", "Bottom"])),
        "enable_streaming" => Some(choice_values(["Always", "DaemonOnly", "Never"])),
        "device.bitrate" => Some(choice_values(["96", "160", "320"])),
        "playback_metadata_fields" => Some(AppConfigValueKind::MultiChoice(
            ["repeat", "shuffle", "volume", "device"]
                .into_iter()
                .map(String::from)
                .collect(),
        )),
        "theme" => CONFIGS.get().map(|configs| {
            AppConfigValueKind::Choice(
                configs
                    .theme_config
                    .themes
                    .iter()
                    .map(|theme| theme.name.clone())
                    .collect(),
            )
        }),
        _ => None,
    }
}

fn choice_values(values: impl IntoIterator<Item = &'static str>) -> AppConfigValueKind {
    AppConfigValueKind::Choice(values.into_iter().map(String::from).collect())
}

fn app_config_key_requires_restart(key: &str) -> bool {
    // `save_settings_value` reloads only these UI-owned values into the
    // running session. The remaining AppConfig fields are read from the
    // startup-only `Configs` snapshot and therefore take effect on restart.
    !matches!(
        key,
        "presentation.layout_preset"
            | "presentation.profile"
            | "presentation.compact_metadata"
            | "presentation.journal_indicators"
            | "presentation.focused_row_overflow"
            | "border_type"
            | "theme"
            | "layout.playback_window_position"
            | "layout.playback_window_height"
            | "app_refresh_duration_in_ms"
            | "terminal_title"
            | "terminal_title_idle"
    )
}

#[cfg(test)]
mod settings_tests {
    use super::*;

    #[test]
    fn default_playback_refresh_keeps_external_device_changes_visible() {
        assert_eq!(AppConfig::default().playback_refresh_duration_in_ms, 4_000);
    }

    #[test]
    fn ui_frame_interval_applies_live_and_has_a_floor() {
        let mut config = AppConfig::default();
        assert_eq!(
            config.ui_frame_interval(),
            std::time::Duration::from_millis(32)
        );
        config.app_refresh_duration_in_ms = 0;
        assert_eq!(
            config.ui_frame_interval(),
            std::time::Duration::from_millis(8)
        );
        assert!(!app_config_key_requires_restart(
            "app_refresh_duration_in_ms"
        ));
        assert_eq!(
            setting_label("app_refresh_duration_in_ms"),
            "UI frame interval"
        );
    }

    #[test]
    fn layout_preset_defaults_persists_and_exposes_live_choices() {
        let folder = tempfile::tempdir().unwrap();
        std::fs::write(
            folder.path().join("app.toml"),
            r#"[layout]
playback_window_height = 9
playback_window_position = "Bottom"
"#,
        )
        .unwrap();
        let original = AppConfig::new(folder.path()).unwrap();
        assert_eq!(original.presentation.layout_preset, LayoutPreset::Current);
        assert_eq!(original.layout.playback_window_height, 9);
        assert_eq!(original.layout.playback_window_position, Position::Bottom);
        save_app_config_override(folder.path(), "presentation.layout_preset", "Borderless")
            .unwrap();
        let saved = AppConfig::new(folder.path()).unwrap();
        assert_eq!(
            saved.presentation.effective().layout_preset,
            LayoutPreset::Borderless
        );
        assert_eq!(saved.layout.playback_window_height, 9);
        let settings = app_config_settings(folder.path()).unwrap();
        let setting = settings
            .iter()
            .find(|setting| setting.key == "presentation.layout_preset")
            .unwrap();
        assert_eq!(setting.value, "\"Borderless\"");
        assert_eq!(
            setting.kind,
            AppConfigValueKind::Choice(vec!["Current".into(), "Borderless".into()])
        );
        assert!(!setting.restart_required);
        assert_eq!(setting_label(&setting.key), "Layout preset");
    }

    #[test]
    fn settings_include_provider_auth_actions_in_their_sections() {
        let folder = std::env::temp_dir().join(format!(
            "unified-player-settings-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&folder).unwrap();
        let settings = app_config_settings(&folder).unwrap();

        assert!(settings.iter().any(|setting| {
            setting.key == "presentation.profile"
                && setting.section == AppConfigSection::SharedUi
                && setting.kind
                    == AppConfigValueKind::Choice(vec![
                        "Minimal".to_string(),
                        "Balanced".to_string(),
                        "Detailed".to_string(),
                        "Custom".to_string(),
                    ])
                && !setting.restart_required
        }));
        assert!(settings.iter().any(|setting| {
            setting.key == "lyrics.providers"
                && setting.section == AppConfigSection::Services
                && setting.kind
                    == AppConfigValueKind::MultiChoice(vec![
                        "simpmusic".to_string(),
                        "lrclib".to_string(),
                        "lyricsovh".to_string(),
                        "musixmatch".to_string(),
                    ])
                && setting.restart_required
        }));
        assert!(settings.iter().any(|setting| {
            setting.section == AppConfigSection::SharedUi
                && setting.kind == AppConfigValueKind::Action(AppConfigAction::OpenWelcomeSetup)
        }));
        assert!(settings.iter().any(|setting| {
            setting.section == AppConfigSection::Spotify
                && setting.kind == AppConfigValueKind::Action(AppConfigAction::AuthenticateSpotify)
        }));
        assert!(settings.iter().any(|setting| {
            setting.section == AppConfigSection::YouTubeMusic
                && setting.kind
                    == AppConfigValueKind::Action(AppConfigAction::AuthenticateYouTubeBrowser)
        }));
        assert!(settings.iter().any(|setting| {
            setting.section == AppConfigSection::YouTubeMusic
                && setting.kind == AppConfigValueKind::Action(AppConfigAction::ImportYouTubeAuth)
        }));
        assert!(settings.iter().any(|setting| {
            setting.section == AppConfigSection::YouTubeMusic
                && setting.kind == AppConfigValueKind::Action(AppConfigAction::TestYouTubeAuth)
        }));
        assert!(settings.iter().any(|setting| {
            setting.key == "accounts.spotify.add"
                && setting.section == AppConfigSection::Accounts
                && setting.kind == AppConfigValueKind::Action(AppConfigAction::AddSpotifyAccount)
        }));
        assert!(settings.iter().any(|setting| {
            setting.key == "accounts.youtube_music.validate"
                && setting.section == AppConfigSection::Accounts
                && setting.kind
                    == AppConfigValueKind::Action(AppConfigAction::ValidateYouTubeAccount)
        }));
        assert!(settings.iter().any(|setting| {
            setting.key == "diagnostics.reset_all"
                && setting.section == AppConfigSection::Diagnostics
                && setting.kind
                    == AppConfigValueKind::Action(AppConfigAction::ResetAllConfiguration)
                && !setting.restart_required
        }));
        assert!(settings
            .iter()
            .any(|setting| setting.key == "accounts.spotify.status"));
        assert!(settings.iter().any(|setting| {
            setting.key == "youtube.javascript_runtime.status"
                && setting.kind == AppConfigValueKind::Status
                && !setting.value.is_empty()
        }));
        assert!(settings.iter().any(|setting| {
            setting.key == "youtube.javascript_runtime"
                && setting.kind
                    == AppConfigValueKind::Choice(vec![
                        "Auto".to_string(),
                        "Node".to_string(),
                        "QuickJs".to_string(),
                    ])
                && setting.restart_required
        }));
        assert!(settings.iter().any(|setting| {
            setting.key == "enable_mouse_navigation"
                && setting.section == AppConfigSection::Playback
                && setting.kind == AppConfigValueKind::Bool
                && setting.restart_required
        }));
        assert!(settings
            .iter()
            .all(|setting| !setting.value.contains("access_token")));

        std::fs::remove_dir_all(folder).unwrap();
    }

    #[test]
    fn legacy_youtube_downloader_option_is_accepted_but_ignored() {
        let folder = std::env::temp_dir().join(format!(
            "unified-player-legacy-youtube-config-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&folder).unwrap();
        std::fs::write(
            folder.join(APP_CONFIG_FILE),
            "[youtube]\nyt_dlp_command = \"unused\"\n",
        )
        .unwrap();

        let config = AppConfig::new(&folder).unwrap();
        assert_eq!(
            config.youtube.playback_quality,
            YouTubePlaybackQuality::High
        );

        std::fs::remove_dir_all(folder).unwrap();
    }

    #[test]
    fn presentation_settings_are_grouped_and_session_history_requires_restart() {
        let folder = std::env::temp_dir().join(format!(
            "unified-player-presentation-settings-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&folder).unwrap();
        let settings = app_config_settings(&folder).unwrap();

        assert!(settings.iter().any(|setting| {
            setting.key == "session_history.enabled"
                && setting.section == AppConfigSection::Services
                && setting.kind == AppConfigValueKind::Bool
                && setting.restart_required
        }));
        assert!(settings.iter().any(|setting| {
            setting.key == "session_history.max_entries"
                && setting.section == AppConfigSection::Services
                && setting.value == "200"
                && setting.restart_required
        }));

        assert!(settings.iter().any(|setting| {
            setting.key == "presentation.compact_metadata"
                && setting.section == AppConfigSection::SharedUi
                && setting.kind
                    == AppConfigValueKind::Choice(vec![
                        "Minimal".to_string(),
                        "Balanced".to_string(),
                        "Detailed".to_string(),
                    ])
                && !setting.restart_required
        }));
        assert!(settings.iter().any(|setting| {
            setting.key == "presentation.journal_indicators"
                && setting.section == AppConfigSection::SharedUi
                && setting.kind
                    == AppConfigValueKind::MultiChoice(vec![
                        "listened".to_string(),
                        "listen_later".to_string(),
                        "rating".to_string(),
                        "note".to_string(),
                    ])
                && !setting.restart_required
        }));
        assert!(settings.iter().any(|setting| {
            setting.key == "presentation.focused_row_overflow"
                && setting.section == AppConfigSection::SharedUi
                && setting.kind
                    == AppConfigValueKind::Choice(vec![
                        "Truncate".to_string(),
                        "Marquee".to_string(),
                        "Manual".to_string(),
                    ])
                && !setting.restart_required
        }));

        std::fs::remove_dir_all(folder).unwrap();
    }

    #[test]
    fn restart_metadata_matches_the_runtime_configuration_boundary() {
        let folder = tempfile::tempdir().unwrap();
        let settings = app_config_settings(folder.path()).unwrap();
        let live_keys = [
            "presentation.layout_preset",
            "presentation.profile",
            "presentation.compact_metadata",
            "presentation.journal_indicators",
            "presentation.focused_row_overflow",
            "border_type",
            "theme",
            "layout.playback_window_position",
            "layout.playback_window_height",
            "app_refresh_duration_in_ms",
            "terminal_title",
            "terminal_title_idle",
            "accounts.spotify.active",
            "accounts.youtube_music.active",
        ];

        for setting in &settings {
            let is_persisted_value = !matches!(
                setting.kind,
                AppConfigValueKind::Status | AppConfigValueKind::Action(_)
            );
            if is_persisted_value {
                assert_eq!(
                    !setting.restart_required,
                    live_keys.contains(&setting.key.as_str()),
                    "unexpected runtime effect for {}",
                    setting.key
                );
            }
        }
    }

    #[test]
    fn setting_labels_prefer_user_facing_concepts_and_keep_fallbacks_readable() {
        assert_eq!(
            setting_label("presentation.compact_metadata"),
            "Compact metadata detail"
        );
        assert_eq!(
            setting_label("accounts.spotify.active"),
            "Active Spotify account"
        );
        assert_eq!(
            setting_label("accounts.youtube_music.validate"),
            "Validate YouTube Music account"
        );
        assert_eq!(
            setting_label("youtube.auth.import"),
            "Import YouTube credentials"
        );
        assert_eq!(setting_label("youtube.auth_type"), "Auth type");
        assert_eq!(setting_label("page_size_in_rows"), "Page size in rows");
        assert_eq!(
            setting_label("enable_mouse_navigation"),
            "Mouse list navigation"
        );
        assert_eq!(
            setting_label("listenbrainz.enabled"),
            "ListenBrainz integration"
        );
        assert_eq!(
            setting_label("listenbrainz.read_only_checking"),
            "ListenBrainz read-only checking"
        );
    }

    #[test]
    fn setting_descriptions_are_stable_for_user_facing_and_unknown_keys() {
        assert!(setting_description("presentation.profile").contains("preset"));
        assert_eq!(
            setting_description("presentation.journal_indicators"),
            "Choose and order journal states; an empty list hides the compact journal column."
        );
        assert_eq!(
            setting_description("accounts.spotify.active"),
            "Choose the Spotify account used by this installation."
        );
        assert!(setting_description("listenbrainz.enabled").contains("explicit"));
        assert!(setting_description("listenbrainz.read_only_checking").contains("without writing"));
        assert!(setting_description("enable_mouse_navigation").contains("page lists"));
        assert!(setting_description("future.setting").starts_with("Application setting."));
    }

    #[test]
    fn youtube_javascript_runtime_round_trips_through_toml() {
        let folder = std::env::temp_dir().join(format!(
            "unified-player-youtube-runtime-config-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&folder).unwrap();
        std::fs::write(
            folder.join(APP_CONFIG_FILE),
            "[youtube]\njavascript_runtime = \"QuickJs\"\n",
        )
        .unwrap();

        let config = AppConfig::new(&folder).unwrap();
        assert_eq!(
            config.youtube.javascript_runtime,
            YouTubeJavaScriptRuntime::QuickJs
        );

        std::fs::remove_dir_all(folder).unwrap();
    }

    #[test]
    fn presentation_preferences_round_trip_through_toml() {
        let folder = std::env::temp_dir().join(format!(
            "unified-player-presentation-config-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&folder).unwrap();
        std::fs::write(
            folder.join(APP_CONFIG_FILE),
            "[presentation]\ncompact_metadata = \"Detailed\"\njournal_indicators = [\"rating\", \"note\"]\n",
        )
        .unwrap();

        let config = AppConfig::new(&folder).unwrap();
        assert_eq!(config.presentation.profile, PresentationProfile::Custom);
        assert_eq!(
            config.presentation.compact_metadata,
            CompactMetadataMode::Detailed
        );
        assert_eq!(
            config.presentation.journal_indicators,
            vec!["rating".to_string(), "note".to_string()]
        );

        std::fs::remove_dir_all(folder).unwrap();
    }

    #[test]
    fn lyrics_provider_allowlist_round_trips_through_toml() {
        let folder = std::env::temp_dir().join(format!(
            "unified-player-lyrics-config-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&folder).unwrap();
        std::fs::write(
            folder.join(APP_CONFIG_FILE),
            "[lyrics]\nproviders = [\"lrclib\"]\n",
        )
        .unwrap();

        let config = AppConfig::new(&folder).unwrap();
        assert_eq!(config.lyrics.providers, vec!["lrclib"]);
        assert!(config.lyrics.provider_enabled("lrclib"));
        assert!(!config.lyrics.provider_enabled("musixmatch"));

        std::fs::remove_dir_all(folder).unwrap();
    }

    #[test]
    fn session_history_config_round_trips_through_toml() {
        let folder = std::env::temp_dir().join(format!(
            "unified-player-session-history-config-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&folder).unwrap();
        std::fs::write(
            folder.join(APP_CONFIG_FILE),
            "[session_history]\nenabled = false\nmax_entries = 12\n",
        )
        .unwrap();

        let config = AppConfig::new(&folder).unwrap();
        assert!(!config.session_history.enabled);
        assert_eq!(config.session_history.max_entries, 12);

        std::fs::remove_dir_all(folder).unwrap();
    }

    #[test]
    fn listenbrainz_integrations_are_opt_in_and_independently_configurable() {
        let defaults = ListenBrainzConfig::default();
        assert!(!defaults.enabled);
        assert!(defaults.read_only_checking);
        assert!(defaults.artist_enrichment);

        let folder = std::env::temp_dir().join(format!(
            "unified-player-listenbrainz-config-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&folder).unwrap();
        std::fs::write(
            folder.join(APP_CONFIG_FILE),
            "[listenbrainz]\nenabled = true\nartist_enrichment = false\n",
        )
        .unwrap();

        let config = AppConfig::new(&folder).unwrap();
        assert!(config.listenbrainz.enabled);
        assert!(config.listenbrainz.read_only_checking);
        assert!(!config.listenbrainz.artist_enrichment);

        let settings = app_config_settings(&folder).unwrap();
        assert!(settings.iter().any(|setting| {
            setting.key == "listenbrainz.enabled"
                && setting.section == AppConfigSection::Services
                && setting.kind == AppConfigValueKind::Bool
                && setting.restart_required
        }));
        assert!(settings.iter().any(|setting| {
            setting.key == "listenbrainz.read_only_checking"
                && setting.section == AppConfigSection::Services
                && setting.kind == AppConfigValueKind::Bool
                && setting.restart_required
        }));
        assert!(settings.iter().any(|setting| {
            setting.key == "listenbrainz.artist_enrichment"
                && setting.section == AppConfigSection::Services
                && setting.kind == AppConfigValueKind::Bool
                && setting.restart_required
        }));

        std::fs::remove_dir_all(folder).unwrap();
    }

    #[test]
    fn presentation_profiles_resolve_without_mutating_custom_config() {
        let custom = PresentationConfig {
            layout_preset: LayoutPreset::Current,
            profile: PresentationProfile::Custom,
            compact_metadata: CompactMetadataMode::Detailed,
            journal_indicators: vec!["note".to_string()],
            focused_row_overflow: FocusedRowOverflow::Marquee,
        };
        assert_eq!(custom.effective(), custom);

        let mut preset = custom.clone();
        preset.profile = PresentationProfile::Minimal;
        let effective = preset.effective();
        assert_eq!(effective.compact_metadata, CompactMetadataMode::Minimal);
        assert_eq!(effective.journal_indicators, vec!["listened"]);
        assert_eq!(effective.profile, PresentationProfile::Minimal);
        assert_eq!(preset.compact_metadata, CompactMetadataMode::Detailed);
        assert_eq!(preset.journal_indicators, vec!["note"]);
    }

    #[test]
    fn compact_metadata_modes_have_stable_secondary_column_policy() {
        assert!(!CompactMetadataMode::Minimal.shows_artists());
        assert!(!CompactMetadataMode::Minimal.shows_album());
        assert!(!CompactMetadataMode::Minimal.shows_added_at());
        assert!(CompactMetadataMode::Balanced.shows_artists());
        assert!(CompactMetadataMode::Balanced.shows_album());
        assert!(!CompactMetadataMode::Balanced.shows_added_at());
        assert!(CompactMetadataMode::Detailed.shows_artists());
        assert!(CompactMetadataMode::Detailed.shows_album());
        assert!(CompactMetadataMode::Detailed.shows_added_at());
    }

    #[test]
    fn settings_project_effective_values_for_named_presentation_profiles() {
        let folder = std::env::temp_dir().join(format!(
            "unified-player-presentation-profile-settings-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&folder).unwrap();
        std::fs::write(
            folder.join(APP_CONFIG_FILE),
            "[presentation]\nprofile = \"Balanced\"\ncompact_metadata = \"Detailed\"\njournal_indicators = [\"note\"]\n",
        )
        .unwrap();

        let settings = app_config_settings(&folder).unwrap();
        let metadata = settings
            .iter()
            .find(|setting| setting.key == "presentation.compact_metadata")
            .unwrap();
        let indicators = settings
            .iter()
            .find(|setting| setting.key == "presentation.journal_indicators")
            .unwrap();
        assert_eq!(metadata.value, "\"Balanced\"");
        assert_eq!(
            indicators.value,
            "[\"listened\", \"listen_later\", \"rating\"]"
        );

        std::fs::remove_dir_all(folder).unwrap();
    }

    #[test]
    fn pending_youtube_session_reload_does_not_reactivate_the_previous_slot() {
        let root = tempfile::tempdir().unwrap();
        let config_folder = root.path().join("config");
        let cache_folder = root.path().join("cache");
        let cookie_path = config_folder.join("youtube/cookie.txt");
        std::fs::create_dir_all(cookie_path.parent().unwrap()).unwrap();
        std::fs::create_dir_all(&cache_folder).unwrap();

        let mut registry = AccountRegistry::default();
        let record = registry.add_metadata(ActiveProvider::YouTubeMusic, Some("old"));
        std::fs::write(&cookie_path, "old-cookie").unwrap();
        std::fs::create_dir_all(config_folder.join("youtube/browser-profile")).unwrap();
        registry
            .snapshot_current(
                ActiveProvider::YouTubeMusic,
                &record.id,
                &config_folder,
                &cache_folder,
                &cookie_path,
            )
            .unwrap();
        registry.save(&config_folder).unwrap();

        std::fs::write(&cookie_path, "pending-cookie").unwrap();
        let _configs =
            Configs::new_without_account_bootstrap(&config_folder, &cache_folder).unwrap();

        assert_eq!(
            std::fs::read_to_string(cookie_path).unwrap(),
            "pending-cookie"
        );
    }
}
