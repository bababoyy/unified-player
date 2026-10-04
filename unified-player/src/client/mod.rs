use std::sync::{Arc, Mutex as StdMutex};

use crate::{auth, auth::AuthConfig, config};
#[cfg(feature = "streaming")]
use parking_lot::Mutex;

mod auth_session;
mod handlers;
pub(crate) mod listenbrainz;
pub(crate) mod listenbrainz_import;
pub(crate) mod listenbrainz_projection;
pub(crate) mod listenbrainz_pull;
pub(crate) mod listenbrainz_push;
pub(crate) mod listenbrainz_resolution;
pub(crate) mod listenbrainz_sync;
mod playback_actions;
mod playback_coordinator;
mod playback_state;
mod playlist_application;
mod playlist_mutation;
mod playlist_provider_mutation;
mod provider_metadata;
mod provider_read;
mod provider_search;
mod request;
mod request_router;
mod request_scheduler;
mod spotify;
mod state_application;
mod youtube;
pub(crate) use youtube::browser_auth::{resolve_browser_executable, save_browser_choice};

pub(crate) use auth_session::startup_failure_metadata;
pub use auth_session::{
    begin_youtube_browser_login, begin_youtube_oauth_login, check_youtube_auth,
    check_youtube_playback_auth, check_youtube_playback_auth_for_video,
    check_youtube_playback_output_for_video, diagnose_youtube_media_transport_for_video,
    new_api_client, probe_youtube_playback_for_video,
};
#[cfg(feature = "private-capture")]
pub(crate) use auth_session::{
    capture_youtube_playback_for_video, inspect_youtube_for_video,
    inspect_youtube_for_video_with_configs, YouTubeDeveloperInspection,
};
pub use handlers::*;
pub(crate) use playlist_application::{
    dispatch_legacy_playlist, playlist_request, PlaylistApplicationService,
};
pub use playlist_mutation::{
    youtube_add_video_to_playlist, youtube_append_playlist_items, youtube_create_playlist,
    youtube_delete_playlist, youtube_playlist_tracks, youtube_rate_song,
    youtube_remove_video_from_playlist,
};
pub use playlist_provider_mutation::*;
pub use provider_search::youtube_search_for_projection;
pub use request::*;
pub(crate) use request_scheduler::{
    channel as client_request_channel, start_client_handler, BulkSendError, ClientRequestSender,
};
pub(crate) use youtube::playback::diagnostic_category_inventory as youtube_playback_diagnostic_category_inventory;
#[cfg(feature = "private-capture")]
pub(crate) use youtube::playback::MediaTransportDiagnostic;
#[cfg(feature = "private-capture")]
pub(crate) use youtube::playback::{YouTubeFreshReplayAdapter, YouTubeOfflineReplayAdapter};
pub use youtube::playback::{YouTubeProbeClient, YouTubeProbeDecoderChunkSize};

pub(crate) type SpotifyQueueCompletionSlot = Arc<
    StdMutex<
        Option<(
            rspotify::model::PlayableId<'static>,
            request::UnifiedQueueCompletion,
        )>,
    >,
>;

/// The application's Spotify client
#[derive(Clone)]
pub struct AppClient {
    http: reqwest::Client,
    /// The integrated Spotify client, mainly used for streaming and librespot integration
    spotify: Arc<spotify::Spotify>,
    auth_config: AuthConfig,
    /// The Spotify Web API client, used for interacting with Spotify Web APIs
    api_client: Arc<parking_lot::RwLock<auth::SpotifyWebApiClient>>,
    spotify_api_epoch: Arc<tokio::sync::RwLock<u64>>,
    /// Keeps client replacement and authentication completion in one operation.
    spotify_api_auth_lock: Arc<tokio::sync::Mutex<()>>,
    youtube: Arc<tokio::sync::Mutex<Option<youtube::YouTubeMusic>>>,
    youtube_browser_login: Arc<tokio::sync::Mutex<auth_session::YouTubeBrowserLoginControl>>,
    account_operation_lock: Arc<tokio::sync::Mutex<()>>,
    /// Serializes Spotify session establishment across Welcome check,
    /// re-authentication, and account-add flows so overlapping requests cannot
    /// repeatedly recreate the integrated playback session.
    spotify_auth_lock: Arc<tokio::sync::Mutex<()>>,
    youtube_player: Arc<tokio::sync::Mutex<youtube::YouTubeLocalPlayer>>,
    youtube_audio_resolver: Arc<dyn youtube::playback::AudioSourceResolver>,
    youtube_audio_quality: config::YouTubePlaybackQuality,
    youtube_audio_cache_size_bytes: usize,
    youtube_prefetch:
        Arc<tokio::sync::Mutex<Option<(String, youtube::playback::PreparedAudioSource)>>>,
    youtube_expiry_recovery: Arc<StdMutex<Option<String>>>,
    youtube_queue_completion: Arc<
        StdMutex<
            Option<(
                String,
                Arc<StdMutex<Option<request::UnifiedQueueCompletion>>>,
            )>,
        >,
    >,
    spotify_queue_completion: Arc<StdMutex<Option<SpotifyQueueCompletionSlot>>>,
    volume_requests: Arc<StdMutex<playback_state::VolumeRequestState>>,
    state_application: state_application::StateApplicationService,
    playback: playback_coordinator::PlaybackCoordinator,
    playback_completion_tx: flume::Sender<request::UnifiedQueueCompletion>,
    playback_completion_rx: flume::Receiver<request::UnifiedQueueCompletion>,
    spotify_rate_limit_until: Arc<StdMutex<Option<std::time::Instant>>>,
    #[cfg(feature = "private-capture")]
    developer_capture: Arc<StdMutex<Option<crate::developer_capture::CaptureHandle>>>,
    #[cfg(feature = "streaming")]
    stream_conn: Arc<Mutex<Option<librespot_connect::Spirc>>>,
}

#[cfg(any(feature = "streaming", test))]
fn integrated_device_for_connection(
    integrated_device_id: Option<&str>,
    has_streaming_connection: bool,
) -> Option<&str> {
    has_streaming_connection
        .then_some(integrated_device_id)
        .flatten()
}

#[cfg(feature = "streaming")]
fn integrated_spotify_load_request(track_uris: Vec<String>) -> librespot_connect::LoadRequest {
    librespot_connect::LoadRequest::from_tracks(
        track_uris,
        librespot_connect::LoadRequestOptions {
            start_playing: true,
            ..Default::default()
        },
    )
}

impl AppClient {
    /// Access the Spotify Web API client explicitly at provider boundaries.
    pub(crate) fn spotify_api(&self) -> auth::SpotifyWebApiClient {
        self.api_client.read().clone()
    }

    #[cfg(feature = "streaming")]
    pub(super) async fn connected_integrated_spotify_device_id(&self) -> Option<String> {
        let integrated_device_id = self
            .spotify
            .session_if_present()
            .await
            .map(|session| session.device_id().to_owned());
        integrated_device_for_connection(
            integrated_device_id.as_deref(),
            self.stream_conn.lock().is_some(),
        )
        .map(str::to_owned)
    }

    /// The integrated device id while it is connected and the active Spotify Connect
    /// device, i.e. while spirc commands take effect.
    pub(super) async fn active_integrated_spotify_device_id(
        &self,
        state: &crate::state::SharedState,
    ) -> Option<String> {
        let connected = self.connected_integrated_spotify_device_id().await?;
        let player = state.player.read();
        (player.active_integrated_device_id() == Some(connected.as_str())).then_some(connected)
    }

    #[cfg(feature = "streaming")]
    pub(super) fn start_integrated_spotify_tracks(
        &self,
        track_uris: Vec<String>,
    ) -> Option<anyhow::Result<()>> {
        let connection = self.stream_conn.lock();
        let connection = connection.as_ref()?;
        let result = (|| {
            connection.activate().map_err(anyhow::Error::from)?;
            connection
                .load(integrated_spotify_load_request(track_uris))
                .map_err(anyhow::Error::from)?;
            Ok(())
        })();
        Some(result)
    }

    // Stub keeps the async signature of the streaming implementation for shared callers.
    #[cfg(not(feature = "streaming"))]
    #[allow(clippy::unused_async)]
    pub(super) async fn connected_integrated_spotify_device_id(&self) -> Option<String> {
        None
    }

    #[cfg(not(feature = "streaming"))]
    pub(super) fn start_integrated_spotify_tracks(
        &self,
        _track_uris: Vec<String>,
    ) -> Option<anyhow::Result<()>> {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::integrated_device_for_connection;
    #[cfg(feature = "streaming")]
    use super::integrated_spotify_load_request;

    #[test]
    fn integrated_device_requires_a_live_streaming_connection() {
        assert_eq!(
            integrated_device_for_connection(Some("integrated"), true),
            Some("integrated")
        );
        assert_eq!(
            integrated_device_for_connection(Some("integrated"), false),
            None
        );
        assert_eq!(integrated_device_for_connection(None, true), None);
    }

    #[cfg(feature = "streaming")]
    #[test]
    fn integrated_spotify_load_starts_the_requested_tracks() {
        let request = integrated_spotify_load_request(vec!["spotify:track:test".to_owned()]);

        assert!(request.start_playing);
        assert_eq!(request.seek_to, 0);
        assert!(format!("{request:?}").contains("spotify:track:test"));
    }
}

#[cfg(feature = "private-capture")]
impl AppClient {
    pub(crate) fn install_developer_capture(
        &self,
        handle: crate::developer_capture::CaptureHandle,
    ) {
        *self
            .developer_capture
            .lock()
            .expect("developer capture handle mutex poisoned") = Some(handle);
    }

    #[allow(dead_code)]
    pub(crate) fn developer_capture(&self) -> Option<crate::developer_capture::CaptureHandle> {
        self.developer_capture
            .lock()
            .expect("developer capture handle mutex poisoned")
            .clone()
    }
}
