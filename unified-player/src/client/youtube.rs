pub(crate) mod cookie_import;
use std::{future::Future, path::Path, sync::Arc};

use anyhow::{Context, Result};
use futures::{StreamExt, TryStreamExt};
use rodio::{OutputStreamBuilder, Sink, Source};
use ytmapi_rs::{
    auth::{AuthToken, BrowserToken, LoggedIn, OAuthToken},
    common::{
        AlbumID, ArtistChannelID, LikeStatus, PlaylistID, PodcastID, SetVideoID, Thumbnail,
        VideoID, YoutubeID,
    },
    parse::{
        EpisodeDate, LibraryArtist, LibraryPlaylist, PlaylistItem, SearchResultAlbum,
        SearchResultArtist, SearchResultEpisode, SearchResultPlaylist, SearchResultPodcast,
        SearchResultSong, SearchResultVideo,
    },
    query::playlist::{DuplicateHandlingMode, EditPlaylistQuery, PrivacyStatus},
    query::{
        search::{
            AlbumsFilter, ArtistsFilter, BasicSearch, EpisodesFilter, PlaylistsFilter,
            PodcastsFilter, SongsFilter, SpellingMode, VideosFilter,
        },
        CreatePlaylistQuery, GetAlbumQuery, GetArtistQuery, GetLibraryAlbumsQuery,
        GetLibraryArtistsQuery, GetLibraryPlaylistsQuery, GetPlaylistDetailsQuery,
        GetPlaylistTracksQuery, GetPodcastQuery, SearchQuery,
    },
    YtMusic, YtMusicBuilder,
};

use crate::config::{self, YouTubeMusicAuthType};
use crate::state::{
    YouTubeArtistContext, YouTubeArtistRelease, YouTubeContext, YouTubeContextId, YouTubeEpisode,
    YouTubeLibrary, YouTubeLibraryAlbum, YouTubeLibraryArtist, YouTubeLibraryPlaylist,
    YouTubePodcast, YouTubeRelatedArtist, YouTubeSearchResults, YouTubeTrack,
};

pub mod browser_auth;
pub(crate) mod javascript;
pub mod playback;

#[derive(Debug)]
pub enum YouTubeMusic {
    Browser(YtMusic<BrowserToken>),
    OAuth(YtMusic<OAuthToken>),
}

// A library response is continuation-backed. Keep a large library bounded so
// the TUI reaches a terminal state instead of waiting for an unbounded stream.
const MAX_LIBRARY_PAGES: usize = 20;
const YOUTUBE_CONTEXT_RESPONSE_MAX_ATTEMPTS: usize = 2;

fn context_response_retry_reason(error: &ytmapi_rs::error::ErrorKind) -> Option<&'static str> {
    match error {
        ytmapi_rs::error::ErrorKind::JsonParsing(_) => Some("json_parsing"),
        ytmapi_rs::error::ErrorKind::InvalidResponse { .. } => Some("invalid_response"),
        _ => None,
    }
}

async fn with_context_response_retry<T, F, Fut>(
    context_kind: &'static str,
    mut request: F,
) -> ytmapi_rs::Result<T>
where
    F: FnMut() -> Fut,
    Fut: Future<Output = ytmapi_rs::Result<T>>,
{
    for attempt in 1..=YOUTUBE_CONTEXT_RESPONSE_MAX_ATTEMPTS {
        match request().await {
            Ok(output) => return Ok(output),
            Err(error) => {
                let error_kind = error.into_kind();
                let Some(retry_reason) = context_response_retry_reason(&error_kind) else {
                    return Err(error_kind.into());
                };
                if attempt == YOUTUBE_CONTEXT_RESPONSE_MAX_ATTEMPTS {
                    return Err(error_kind.into());
                }

                tracing::warn!(
                    context_kind,
                    retry_reason,
                    next_attempt = attempt + 1,
                    "YouTube Music context response was incompatible; retrying once"
                );
                tokio::task::yield_now().await;
            }
        }
    }

    unreachable!("context response retry loop always returns")
}

async fn collect_library_pages<S, T>(stream: S) -> ytmapi_rs::Result<(Vec<T>, bool)>
where
    S: futures::Stream<Item = ytmapi_rs::Result<Vec<T>>>,
{
    let mut pages = stream
        .take(MAX_LIBRARY_PAGES.saturating_add(1))
        .try_collect::<Vec<_>>()
        .await?;
    let truncated = pages.len() > MAX_LIBRARY_PAGES;
    pages.truncate(MAX_LIBRARY_PAGES);
    Ok((pages.into_iter().flatten().collect(), truncated))
}

fn playlist_context_title(title: Option<String>) -> String {
    title
        .filter(|title| !title.trim().is_empty())
        .unwrap_or_else(|| "YouTube Playlist".to_string())
}

pub struct YouTubeOAuthLogin {
    client: ytmapi_rs::Client,
    device_code: ytmapi_rs::auth::oauth::OAuthDeviceCode,
    client_id: String,
    verification_url: String,
}

impl YouTubeOAuthLogin {
    pub fn verification_url(&self) -> &str {
        &self.verification_url
    }

    pub async fn finish(self, client_secret: String, token_path: &Path) -> Result<()> {
        let token = ytmapi_rs::generate_oauth_token(
            &self.client,
            self.device_code,
            self.client_id,
            client_secret,
        )
        .await
        .context("finish YouTube Music Google sign-in")?;
        persist_oauth_token(token_path, &token)
    }
}

pub async fn begin_oauth_login(client_id: String) -> Result<YouTubeOAuthLogin> {
    let client =
        ytmapi_rs::Client::new_rustls_tls().context("create YouTube Music OAuth HTTP client")?;
    let (device_code, verification_url) =
        ytmapi_rs::generate_oauth_code_and_url(&client, &client_id)
            .await
            .context("start YouTube Music Google sign-in")?;
    Ok(YouTubeOAuthLogin {
        client,
        device_code,
        client_id,
        verification_url,
    })
}

pub struct YouTubeLocalPlayer {
    audio_output: Option<YouTubeAudioOutput>,
    playback: Option<YouTubeLocalPlayback>,
    /// Restart point kept after `suspend` released the audio output, so a
    /// later resume can reopen the output at the same track and position.
    suspended: Option<YouTubePlaybackRestart>,
    volume: u8,
    mute_state: Option<u8>,
}

struct YouTubeAudioOutput {
    mixer: rodio::mixer::Mixer,
    shutdown_tx: Option<std::sync::mpsc::Sender<()>>,
    worker: Option<std::thread::JoinHandle<()>>,
}

impl YouTubeAudioOutput {
    fn open() -> Result<Self> {
        let (ready_tx, ready_rx) = std::sync::mpsc::sync_channel(1);
        let (shutdown_tx, shutdown_rx) = std::sync::mpsc::channel();
        let worker = std::thread::Builder::new()
            .name("youtube-audio-output".to_string())
            .spawn(move || match OutputStreamBuilder::open_default_stream() {
                Ok(mut stream) => {
                    stream.log_on_drop(false);
                    let mixer = stream.mixer().clone();
                    if ready_tx.send(Ok(mixer)).is_ok() {
                        let _ = shutdown_rx.recv();
                    }
                }
                Err(err) => {
                    let _ = ready_tx.send(Err(format!("{err:#}")));
                }
            })
            .context("spawn YouTube audio output worker")?;

        let mixer = match ready_rx.recv() {
            Ok(Ok(mixer)) => mixer,
            Ok(Err(err)) => {
                let _ = worker.join();
                crate::observability::set_component_health(
                    crate::observability::Component::Audio,
                    crate::observability::HealthStatus::Degraded,
                    "output-unavailable",
                );
                anyhow::bail!("open default audio output stream: {err}");
            }
            Err(_) => {
                let _ = worker.join();
                crate::observability::set_component_health(
                    crate::observability::Component::Audio,
                    crate::observability::HealthStatus::Degraded,
                    "worker-ended",
                );
                anyhow::bail!("YouTube audio output worker ended during initialization");
            }
        };

        crate::observability::set_component_health(
            crate::observability::Component::Audio,
            crate::observability::HealthStatus::Healthy,
            "output-ready",
        );

        Ok(Self {
            mixer,
            shutdown_tx: Some(shutdown_tx),
            worker: Some(worker),
        })
    }

    fn mixer(&self) -> &rodio::mixer::Mixer {
        &self.mixer
    }

    fn stop_and_join(&mut self) -> Result<()> {
        let signalled = self
            .shutdown_tx
            .take()
            .is_none_or(|shutdown_tx| shutdown_tx.send(()).is_ok());
        let joined = self
            .worker
            .take()
            .is_none_or(|worker| worker.join().is_ok());
        if signalled && joined {
            crate::observability::set_component_health(
                crate::observability::Component::Audio,
                crate::observability::HealthStatus::Stopped,
                "worker-joined",
            );
            Ok(())
        } else {
            crate::observability::set_component_health(
                crate::observability::Component::Audio,
                crate::observability::HealthStatus::Degraded,
                "worker-exit-failed",
            );
            anyhow::bail!("YouTube audio output worker terminated unexpectedly")
        }
    }
}

impl Drop for YouTubeAudioOutput {
    fn drop(&mut self) {
        if self.stop_and_join().is_err() {
            tracing::warn!("YouTube audio output worker terminated unexpectedly");
        }
    }
}

struct YouTubeLocalPlayback {
    sink: Arc<Sink>,
    track_id: String,
    track: YouTubeTrack,
    source: playback::ResolvedAudioSource,
    base_position: std::time::Duration,
}

#[derive(Debug, Clone)]
pub struct YouTubePlaybackSnapshot {
    pub track: YouTubeTrack,
    pub is_playing: bool,
    pub progress: std::time::Duration,
    pub volume: u8,
    pub mute_state: Option<u8>,
    pub route: crate::state::YouTubePlaybackRoute,
}

#[derive(Clone, Debug)]
pub(crate) struct YouTubePlaybackRestart {
    pub(crate) track: YouTubeTrack,
    pub(crate) source: playback::ResolvedAudioSource,
    pub(crate) progress: std::time::Duration,
    pub(crate) ended: bool,
    pub(crate) was_playing: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum SeekAttempt {
    Applied,
    RestartRequired,
}

trait SeekSink {
    fn is_empty(&self) -> bool;
    fn try_seek_to(&self, position: std::time::Duration) -> std::result::Result<(), String>;
}

impl SeekSink for Sink {
    fn is_empty(&self) -> bool {
        self.empty()
    }

    fn try_seek_to(&self, position: std::time::Duration) -> std::result::Result<(), String> {
        self.try_seek(position).map_err(|err| format!("{err:?}"))
    }
}

fn try_seek_active_sink(
    sink: &impl SeekSink,
    position: std::time::Duration,
) -> Result<SeekAttempt> {
    if sink.is_empty() {
        return Ok(SeekAttempt::RestartRequired);
    }
    sink.try_seek_to(position)
        .map_err(|err| anyhow::anyhow!("seek native YouTube audio: {err}"))?;
    Ok(if sink.is_empty() {
        SeekAttempt::RestartRequired
    } else {
        SeekAttempt::Applied
    })
}

#[derive(Debug, Clone)]
pub struct YouTubePlaybackControlSnapshot {
    pub is_playing: bool,
    pub progress: std::time::Duration,
    pub volume: u8,
    pub mute_state: Option<u8>,
}

impl YouTubeLocalPlayer {
    pub fn new(volume: u8) -> Self {
        Self {
            audio_output: None,
            playback: None,
            suspended: None,
            volume: volume.min(100),
            mute_state: None,
        }
    }

    pub(crate) fn play_native(
        &mut self,
        track: YouTubeTrack,
        resolved_source: playback::ResolvedAudioSource,
        source: Box<dyn Source<Item = f32> + Send>,
        position: std::time::Duration,
        is_playing: bool,
    ) -> Result<YouTubePlaybackSnapshot> {
        self.stop();
        if self.audio_output.is_none() {
            self.audio_output = Some(YouTubeAudioOutput::open()?);
        }
        let sink = Arc::new(Sink::connect_new(
            self.audio_output
                .as_ref()
                .expect("audio output initialized before creating sink")
                .mixer(),
        ));
        sink.append(source);
        sink.set_volume(self.active_volume_factor());
        if is_playing {
            sink.play();
        } else {
            sink.pause();
        }

        self.playback = Some(YouTubeLocalPlayback {
            sink,
            track_id: track.id.clone(),
            track: track.clone(),
            source: resolved_source,
            base_position: position,
        });

        Ok(YouTubePlaybackSnapshot {
            track,
            is_playing,
            progress: position,
            volume: self.volume,
            mute_state: self.mute_state,
            route: crate::state::YouTubePlaybackRoute::default(),
        })
    }

    pub fn stop(&mut self) {
        self.suspended = None;
        if let Some(playback) = self.playback.take() {
            playback.sink.stop();
        }
    }

    pub fn shutdown(&mut self) -> Result<()> {
        self.stop();
        self.release_output()
    }

    /// Pause playback and close the audio output while remembering where to
    /// restart. An open output keeps the audio device awake streaming silence,
    /// so it is released whenever `YouTube` playback is not audible.
    pub(crate) fn suspend(&mut self) -> Result<Option<YouTubePlaybackControlSnapshot>> {
        let snapshot = self.pause();
        if let Some(mut restart) = self.restart_info() {
            restart.was_playing = false;
            if let Some(playback) = self.playback.take() {
                playback.sink.stop();
            }
            self.suspended = Some(restart);
        }
        self.release_output()?;
        Ok(snapshot.map(|snapshot| YouTubePlaybackControlSnapshot {
            is_playing: false,
            ..snapshot
        }))
    }

    /// Suspend only if `expected_sink` is still the active sink and paused, so
    /// a delayed idle release cannot interrupt playback that resumed or changed.
    pub(crate) fn suspend_if_idle(&mut self, expected_sink: &Arc<Sink>) -> Result<bool> {
        let idle = self.playback.as_ref().is_some_and(|playback| {
            Arc::ptr_eq(&playback.sink, expected_sink) && playback.sink.is_paused()
        });
        if idle {
            self.suspend()?;
        }
        Ok(idle)
    }

    /// Close the audio output when no playback is using it.
    pub(crate) fn release_output_if_unused(&mut self) -> Result<bool> {
        let unused = self.playback.is_none() && self.audio_output.is_some();
        if unused {
            self.release_output()?;
        }
        Ok(unused)
    }

    pub(crate) fn active_sink(&self) -> Option<Arc<Sink>> {
        self.playback.as_ref().map(|playback| playback.sink.clone())
    }

    fn release_output(&mut self) -> Result<()> {
        if let Some(mut audio_output) = self.audio_output.take() {
            audio_output.stop_and_join()?;
        }
        Ok(())
    }

    pub fn restore_controls(&mut self, volume: u8, mute_state: Option<u8>) {
        self.volume = volume.min(100);
        self.mute_state = mute_state.map(|volume| volume.min(100));
    }

    pub fn pause(&mut self) -> Option<YouTubePlaybackControlSnapshot> {
        let playback = self.playback.as_ref()?;
        if !playback.sink.is_paused() {
            playback.sink.pause();
        }
        Some(self.control_snapshot(playback))
    }

    pub fn resume(&mut self) -> Option<YouTubePlaybackControlSnapshot> {
        let playback = self.playback.as_ref()?;
        let ended = playback.sink.empty();
        if ended || self.source_is_expired() {
            return None;
        }
        let playback = self
            .playback
            .as_ref()
            .expect("playback remains available after ended check");
        if playback.sink.is_paused() {
            playback.sink.play();
        }
        Some(self.control_snapshot(playback))
    }

    pub fn set_volume(&mut self, volume: u8) -> Option<YouTubePlaybackControlSnapshot> {
        self.volume = volume.min(100);
        self.mute_state = None;
        let playback = self.playback.as_ref()?;
        playback.sink.set_volume(self.active_volume_factor());
        Some(self.control_snapshot(playback))
    }

    pub fn toggle_mute(&mut self) -> Option<YouTubePlaybackControlSnapshot> {
        self.mute_state = match self.mute_state {
            Some(volume) => {
                self.volume = volume.min(100);
                None
            }
            None => Some(self.volume),
        };

        let playback = self.playback.as_ref()?;
        playback.sink.set_volume(self.active_volume_factor());
        Some(self.control_snapshot(playback))
    }

    pub fn seek(
        &mut self,
        position: std::time::Duration,
    ) -> Result<Option<YouTubePlaybackControlSnapshot>> {
        let Some(playback) = self.playback.as_ref() else {
            return Ok(None);
        };
        if try_seek_active_sink(playback.sink.as_ref(), position)? == SeekAttempt::RestartRequired {
            // Remove the exhausted sink before releasing the player lock. The
            // monitor then observes no active sink and cannot mistake a seek
            // fallback for a natural end-of-track transition.
            self.stop();
            return Ok(None);
        }
        let playback = self
            .playback
            .as_mut()
            .expect("active sink remains installed after successful seek");
        playback.base_position = std::time::Duration::ZERO;
        Ok(Some(YouTubePlaybackControlSnapshot {
            is_playing: !playback.sink.is_paused(),
            progress: position,
            volume: self.volume,
            mute_state: self.mute_state,
        }))
    }

    pub fn snapshot(&self) -> Option<(String, YouTubePlaybackControlSnapshot, bool, bool)> {
        let playback = self.playback.as_ref()?;
        Some(self.snapshot_for_playback(playback))
    }

    pub(crate) fn snapshot_for_sink(
        &self,
        expected_sink: &Arc<Sink>,
    ) -> Option<(String, YouTubePlaybackControlSnapshot, bool, bool)> {
        let playback = self.playback.as_ref()?;
        if !Arc::ptr_eq(&playback.sink, expected_sink) {
            return None;
        }
        Some(self.snapshot_for_playback(playback))
    }

    fn snapshot_for_playback(
        &self,
        playback: &YouTubeLocalPlayback,
    ) -> (String, YouTubePlaybackControlSnapshot, bool, bool) {
        let ended = playback.sink.empty();
        (
            playback.track_id.clone(),
            self.control_snapshot(playback),
            ended,
            self.source_is_expired(),
        )
    }

    pub(crate) fn restart_info(&self) -> Option<YouTubePlaybackRestart> {
        let Some(playback) = self.playback.as_ref() else {
            return self.suspended.clone();
        };
        let ended = playback.sink.empty();
        Some(YouTubePlaybackRestart {
            track: playback.track.clone(),
            source: playback.source.clone(),
            progress: playback.base_position + playback.sink.get_pos(),
            ended,
            was_playing: !playback.sink.is_paused() && !ended,
        })
    }

    pub(crate) fn completion_sink(&self, track_id: &str) -> Option<Arc<Sink>> {
        self.playback
            .as_ref()
            .filter(|playback| playback.track_id == track_id)
            .map(|playback| playback.sink.clone())
    }

    fn source_is_expired(&self) -> bool {
        self.playback
            .as_ref()
            .is_some_and(|playback| playback.source.is_expired())
    }

    fn active_volume_factor(&self) -> f32 {
        if self.mute_state.is_some() {
            0.0
        } else {
            f32::from(self.volume) / 100.0
        }
    }

    fn control_snapshot(&self, playback: &YouTubeLocalPlayback) -> YouTubePlaybackControlSnapshot {
        let ended = playback.sink.empty();
        YouTubePlaybackControlSnapshot {
            is_playing: !playback.sink.is_paused() && !ended,
            progress: playback.base_position + playback.sink.get_pos(),
            volume: self.volume,
            mute_state: self.mute_state,
        }
    }
}

impl Drop for YouTubeLocalPlayer {
    fn drop(&mut self) {
        if self.shutdown().is_err() {
            tracing::warn!("YouTube audio output worker terminated unexpectedly");
        }
    }
}

impl YouTubeMusic {
    pub(crate) async fn from_cookie(cookie: String) -> Result<Self> {
        let api = YtMusicBuilder::new_rustls_tls()
            .with_browser_token_cookie(format!("{};", cookie.trim_end_matches(';')))
            .build()
            .await?;
        Ok(Self::Browser(api))
    }

    pub async fn new(configs: &config::Configs) -> Result<Self> {
        let status = configs.youtube_music_auth_status();
        if !status.is_ready() {
            anyhow::bail!(
                "{}",
                status
                    .missing_message()
                    .unwrap_or_else(|| "YouTube Music authentication is not ready".to_string())
            );
        }

        match configs.app_config.youtube.auth_type {
            YouTubeMusicAuthType::Browser => {
                let cookie_path = configs.youtube_music_cookie_path();
                let api = YtMusicBuilder::new_rustls_tls()
                    .with_browser_token_cookie_file(cookie_path)
                    .build()
                    .await
                    .context(
                        "load the saved YouTube Music browser session; run `unified-player youtube browser-login` to replace it",
                    )?;
                Ok(Self::Browser(api))
            }
            YouTubeMusicAuthType::OAuth => {
                let oauth_path = configs.youtube_music_oauth_path();
                let token = read_oauth_token(&oauth_path).await?;
                let mut api = YtMusicBuilder::new_rustls_tls()
                    .with_auth_token(token)
                    .build()?;
                let refreshed = api.refresh_token().await?;
                persist_oauth_token(&oauth_path, &refreshed)?;
                Ok(Self::OAuth(api))
            }
            YouTubeMusicAuthType::Unauthenticated => {
                anyhow::bail!("YouTube Music mode requires login; unauthenticated mode is disabled")
            }
        }
    }

    pub async fn search(&self, query: &str) -> Result<YouTubeSearchResults> {
        macro_rules! search_all {
            ($api:expr) => {{
                let song_query = SearchQuery::new_filtered(query, SongsFilter)
                    .with_spelling_mode(SpellingMode::ExactMatch);
                let video_query = SearchQuery::new_filtered(query, VideosFilter)
                    .with_spelling_mode(SpellingMode::ExactMatch);
                let album_query = SearchQuery::new_filtered(query, AlbumsFilter)
                    .with_spelling_mode(SpellingMode::ExactMatch);
                let artist_query = SearchQuery::new_filtered(query, ArtistsFilter)
                    .with_spelling_mode(SpellingMode::ExactMatch);
                let playlist_query = SearchQuery::new_filtered(query, PlaylistsFilter)
                    .with_spelling_mode(SpellingMode::ExactMatch);
                let podcast_query = SearchQuery::new_filtered(query, PodcastsFilter)
                    .with_spelling_mode(SpellingMode::ExactMatch);
                let episode_query = SearchQuery::new_filtered(query, EpisodesFilter)
                    .with_spelling_mode(SpellingMode::ExactMatch);
                tokio::join!(
                    $api.query(song_query),
                    $api.query(video_query),
                    $api.query(album_query),
                    $api.query(artist_query),
                    $api.query(playlist_query),
                    $api.query(podcast_query),
                    $api.query(episode_query),
                )
            }};
        }

        let (songs, videos, albums, artists, playlists, podcasts, episodes) = match self {
            Self::Browser(api) => search_all!(api),
            Self::OAuth(api) => search_all!(api),
        };
        let filtered_search_empty =
            songs.as_ref().is_ok_and(Vec::is_empty) && videos.as_ref().is_ok_and(Vec::is_empty);
        let (songs, videos) = match (songs, videos) {
            (Ok(songs), Ok(videos)) if !filtered_search_empty => (songs, videos),
            (songs, videos) => {
                if let Err(error) = songs.as_ref() {
                    log_search_section_error("songs", error);
                }
                if let Err(error) = videos.as_ref() {
                    log_search_section_error("videos", error);
                }
                // The filtered endpoint is occasionally unavailable for one or
                // both media kinds, or returns an empty result despite a basic
                // search having matches. A basic search still exposes
                // songs/videos and is sufficient for playlist projection matching.
                let fallback = match self {
                    Self::Browser(api) => api.query(SearchQuery::<BasicSearch>::from(query)).await,
                    Self::OAuth(api) => api.query(SearchQuery::<BasicSearch>::from(query)).await,
                };
                match fallback {
                    Ok(results) => {
                        let songs = songs
                            .ok()
                            .filter(|items| !items.is_empty())
                            .unwrap_or(results.songs);
                        let videos = videos
                            .ok()
                            .filter(|items| !items.is_empty())
                            .unwrap_or(results.videos);
                        (songs, videos)
                    }
                    Err(error) => {
                        let source = songs.err().or_else(|| videos.err());
                        if let Some(source) = source {
                            crate::observability::log_safe_error!(
                                warn,
                                crate::observability::DiagnosticCode::YOUTUBE_SEARCH_FAILED,
                                crate::observability::ErrorCategory::Unavailable,
                                &source,
                                "YouTube Music filtered search was unavailable; basic search fallback was attempted"
                            );
                        }
                        crate::observability::log_safe_error!(
                            warn,
                            crate::observability::DiagnosticCode::YOUTUBE_SEARCH_FAILED,
                            crate::observability::ErrorCategory::Unavailable,
                            &error,
                            "YouTube Music basic search fallback was unavailable"
                        );
                        (Vec::new(), Vec::new())
                    }
                }
            }
        };
        let albums = search_section_or_empty("albums", albums);
        let artists = search_section_or_empty("artists", artists);
        let playlists = search_section_or_empty("playlists", playlists);
        let podcasts = search_section_or_empty("podcasts", podcasts);
        let episodes = search_section_or_empty("episodes", episodes);

        tracing::debug!(
            "YouTube Music search produced {} song result(s) and {} video result(s)",
            songs.len(),
            videos.len()
        );

        Ok(YouTubeSearchResults {
            songs: songs.into_iter().map(track_from_search_result).collect(),
            videos: videos.into_iter().map(video_from_search_result).collect(),
            albums: albums.into_iter().map(library_album).collect(),
            artists: artists.into_iter().map(search_artist).collect(),
            playlists: playlists.into_iter().filter_map(search_playlist).collect(),
            podcasts: podcasts.into_iter().map(search_podcast).collect(),
            episodes: episodes.into_iter().map(search_episode).collect(),
        })
    }

    pub async fn library(&self) -> Result<YouTubeLibrary> {
        let mut errors = Vec::new();
        let (playlists, albums, artists): (
            Vec<LibraryPlaylist>,
            Vec<SearchResultAlbum>,
            Vec<LibraryArtist>,
        ) =
            match self {
                Self::Browser(api) => {
                    let playlists =
                        match collect_library_pages(api.stream(&GetLibraryPlaylistsQuery)).await {
                            Ok((playlists, truncated)) => {
                                if truncated {
                                    errors.push(library_truncated_message("playlists"));
                                }
                                playlists
                            }
                            Err(err) => {
                                log_library_fetch_error("playlists", &err);
                                errors.push(library_error_message(
                                    "playlists",
                                    is_missing_library_grid_error(&err),
                                ));
                                Vec::new()
                            }
                        };
                    let albums =
                        match collect_library_pages(api.stream(&GetLibraryAlbumsQuery::default()))
                            .await
                        {
                            Ok((albums, truncated)) => {
                                if truncated {
                                    errors.push(library_truncated_message("albums"));
                                }
                                albums
                            }
                            Err(err) => {
                                log_library_fetch_error("albums", &err);
                                errors.push(library_error_message(
                                    "albums",
                                    is_missing_library_grid_error(&err),
                                ));
                                Vec::new()
                            }
                        };
                    let artists =
                        match collect_library_pages(api.stream(&GetLibraryArtistsQuery::default()))
                            .await
                        {
                            Ok((artists, truncated)) => {
                                if truncated {
                                    errors.push(library_truncated_message("artists"));
                                }
                                artists
                            }
                            Err(err) => {
                                log_library_fetch_error("artists", &err);
                                errors.push(library_error_message(
                                    "artists",
                                    is_missing_library_grid_error(&err),
                                ));
                                Vec::new()
                            }
                        };
                    (playlists, albums, artists)
                }
                Self::OAuth(api) => {
                    let playlists =
                        match collect_library_pages(api.stream(&GetLibraryPlaylistsQuery)).await {
                            Ok((playlists, truncated)) => {
                                if truncated {
                                    errors.push(library_truncated_message("playlists"));
                                }
                                playlists
                            }
                            Err(err) => {
                                log_library_fetch_error("playlists", &err);
                                errors.push(library_error_message(
                                    "playlists",
                                    is_missing_library_grid_error(&err),
                                ));
                                Vec::new()
                            }
                        };
                    let albums =
                        match collect_library_pages(api.stream(&GetLibraryAlbumsQuery::default()))
                            .await
                        {
                            Ok((albums, truncated)) => {
                                if truncated {
                                    errors.push(library_truncated_message("albums"));
                                }
                                albums
                            }
                            Err(err) => {
                                log_library_fetch_error("albums", &err);
                                errors.push(library_error_message(
                                    "albums",
                                    is_missing_library_grid_error(&err),
                                ));
                                Vec::new()
                            }
                        };
                    let artists =
                        match collect_library_pages(api.stream(&GetLibraryArtistsQuery::default()))
                            .await
                        {
                            Ok((artists, truncated)) => {
                                if truncated {
                                    errors.push(library_truncated_message("artists"));
                                }
                                artists
                            }
                            Err(err) => {
                                log_library_fetch_error("artists", &err);
                                errors.push(library_error_message(
                                    "artists",
                                    is_missing_library_grid_error(&err),
                                ));
                                Vec::new()
                            }
                        };
                    (playlists, albums, artists)
                }
            };

        Ok(YouTubeLibrary {
            loaded: true,
            playlists: playlists.into_iter().map(library_playlist).collect(),
            albums: albums.into_iter().map(library_album).collect(),
            artists: artists.into_iter().map(library_artist).collect(),
            errors,
        })
    }

    pub async fn context(&self, id: &YouTubeContextId) -> Result<YouTubeContext> {
        match id {
            YouTubeContextId::LikedTracks => {
                let pages = match self {
                    Self::Browser(api) => fetch_liked_playlist_tracks(api).await?,
                    Self::OAuth(api) => fetch_liked_playlist_tracks(api).await?,
                };
                let tracks = pages
                    .into_iter()
                    .flatten()
                    .filter_map(playlist_item_to_track)
                    .collect::<Vec<_>>();
                Ok(YouTubeContext {
                    title: "Liked Music".to_string(),
                    description: Some(format!("{} liked songs and videos", tracks.len())),
                    tracks,
                    playlist_set_video_ids: Vec::new(),
                    artist: None,
                })
            }
            YouTubeContextId::Playlist(id) => {
                let playlist_id = PlaylistID::from_raw(playlist_browse_id(id));
                // The tracks endpoint does not carry the playlist identity. Fetch the
                // lightweight details payload as a best-effort enrichment so the page
                // hierarchy can lead with the actual playlist name instead of a generic
                // provider label. A details failure must not hide otherwise usable rows.
                let (title, pages) = match self {
                    Self::Browser(api) => {
                        let details = api.query(GetPlaylistDetailsQuery::new(playlist_id.clone()));
                        let tracks_query = GetPlaylistTracksQuery::new(playlist_id);
                        let tracks = api.stream(&tracks_query).try_collect::<Vec<_>>();
                        let (details, pages) = tokio::join!(details, tracks);
                        (details.ok().map(|details| details.title), pages?)
                    }
                    Self::OAuth(api) => {
                        let details = api.query(GetPlaylistDetailsQuery::new(playlist_id.clone()));
                        let tracks_query = GetPlaylistTracksQuery::new(playlist_id);
                        let tracks = api.stream(&tracks_query).try_collect::<Vec<_>>();
                        let (details, pages) = tokio::join!(details, tracks);
                        (details.ok().map(|details| details.title), pages?)
                    }
                };
                let items: Vec<PlaylistItem> = pages.into_iter().flatten().collect();
                let tracks: Vec<YouTubeTrack> = items
                    .into_iter()
                    .filter_map(playlist_item_to_track)
                    .collect();
                Ok(YouTubeContext {
                    title: playlist_context_title(title),
                    description: None,
                    playlist_set_video_ids: vec![None; tracks.len()],
                    tracks,
                    artist: None,
                })
            }
            YouTubeContextId::Album(id) => {
                let album_id = AlbumID::from_raw(id.clone());
                let album = match self {
                    Self::Browser(api) => api.query(GetAlbumQuery::new(album_id)).await?,
                    Self::OAuth(api) => api.query(GetAlbumQuery::new(album_id)).await?,
                };
                let artists = album
                    .artists
                    .iter()
                    .map(|artist| artist.name.as_str())
                    .collect::<Vec<_>>()
                    .join(", ");
                let tracks = album
                    .tracks
                    .into_iter()
                    .map(|track| YouTubeTrack {
                        id: track.video_id.get_raw().to_string(),
                        name: track.title,
                        artists: artists.clone(),
                        album: Some(album.title.clone()),
                        duration: track.duration,
                        explicit: matches!(track.explicit, ytmapi_rs::common::Explicit::IsExplicit),
                        thumbnail_url: None,
                        is_video: false,
                    })
                    .collect();
                Ok(YouTubeContext {
                    title: album.title,
                    description: Some(format!("{} • {}", artists, album.year)),
                    tracks,
                    playlist_set_video_ids: Vec::new(),
                    artist: None,
                })
            }
            YouTubeContextId::Artist(id) => {
                let artist_id = ArtistChannelID::from_raw(id.clone());
                let query = GetArtistQuery::new(artist_id);
                let artist = match self {
                    Self::Browser(api) => {
                        with_context_response_retry("artist", || api.query(query.clone())).await?
                    }
                    Self::OAuth(api) => {
                        with_context_response_retry("artist", || api.query(query.clone())).await?
                    }
                };
                let artist_name = artist.name.clone();
                let artist_details = YouTubeArtistContext {
                    channel_id: artist.channel_id.get_raw().to_owned(),
                    name: artist.name.clone(),
                    description: artist.description.clone(),
                    views: artist.views.clone(),
                    subscribers: artist.subscribers.clone(),
                    subscribed: artist.subscribed,
                    radio_id: artist.radio_id.clone(),
                    albums: artist
                        .top_releases
                        .albums
                        .as_ref()
                        .map(|albums| {
                            albums
                                .results
                                .iter()
                                .map(|album| YouTubeArtistRelease {
                                    id: album.album_id.get_raw().to_owned(),
                                    title: album.title.clone(),
                                    year: (!album.year.trim().is_empty())
                                        .then(|| album.year.clone()),
                                    kind: "Album".to_owned(),
                                })
                                .collect()
                        })
                        .unwrap_or_default(),
                    singles: artist
                        .top_releases
                        .singles
                        .as_ref()
                        .map(|singles| {
                            singles
                                .results
                                .iter()
                                .map(|album| YouTubeArtistRelease {
                                    id: album.album_id.get_raw().to_owned(),
                                    title: album.title.clone(),
                                    year: (!album.year.trim().is_empty())
                                        .then(|| album.year.clone()),
                                    kind: "Single".to_owned(),
                                })
                                .collect()
                        })
                        .unwrap_or_default(),
                    related: artist
                        .top_releases
                        .related
                        .as_ref()
                        .map(|related| {
                            related
                                .results
                                .iter()
                                .map(|artist| YouTubeRelatedArtist {
                                    id: artist.browse_id.get_raw().to_owned(),
                                    name: artist.title.clone(),
                                    subscribers: artist.subscribers.clone(),
                                })
                                .collect()
                        })
                        .unwrap_or_default(),
                };
                let mut tracks: Vec<YouTubeTrack> = artist
                    .top_releases
                    .songs
                    .map(|songs| {
                        songs
                            .results
                            .into_iter()
                            .map(artist_song_to_track)
                            .collect()
                    })
                    .unwrap_or_default();
                if tracks.is_empty() {
                    #[allow(deprecated)]
                    let query = SearchQuery::new(artist_name.as_str())
                        .with_filter(SongsFilter)
                        .with_spelling_mode(SpellingMode::ExactMatch);
                    let search_results = match self {
                        Self::Browser(api) => api.query(query).await?,
                        Self::OAuth(api) => api.query(query).await?,
                    };
                    tracks = search_results
                        .into_iter()
                        .map(track_from_search_result)
                        .filter(|track| {
                            track
                                .artists
                                .to_lowercase()
                                .contains(&artist_name.to_lowercase())
                        })
                        .collect();
                }
                Ok(YouTubeContext {
                    title: format!("Artist: {artist_name}"),
                    description: artist.subscribers,
                    tracks,
                    playlist_set_video_ids: Vec::new(),
                    artist: Some(artist_details),
                })
            }
            YouTubeContextId::Podcast(id) => {
                let podcast_id = PodcastID::from_raw(id.clone());
                let podcast = match self {
                    Self::Browser(api) => api.query(GetPodcastQuery::new(podcast_id)).await?,
                    Self::OAuth(api) => api.query(GetPodcastQuery::new(podcast_id)).await?,
                };
                let tracks = podcast
                    .episodes
                    .into_iter()
                    .map(|episode| YouTubeTrack {
                        id: episode.episode_id.get_raw().to_string(),
                        name: episode.title,
                        artists: podcast
                            .channels
                            .iter()
                            .map(|channel| channel.name.as_str())
                            .collect::<Vec<_>>()
                            .join(", "),
                        album: Some(podcast.title.clone()),
                        duration: episode.total_duration,
                        explicit: false,
                        thumbnail_url: thumbnail_url(episode.thumbnails),
                        is_video: true,
                    })
                    .collect();
                Ok(YouTubeContext {
                    title: podcast.title,
                    description: Some(podcast.description),
                    tracks,
                    playlist_set_video_ids: Vec::new(),
                    artist: None,
                })
            }
        }
    }

    pub async fn rate_song(&self, video_id: &str, liked: bool) -> Result<()> {
        let status = if liked {
            LikeStatus::Liked
        } else {
            LikeStatus::Indifferent
        };
        match self {
            Self::Browser(api) => {
                api.rate_song(VideoID::from_raw(video_id.to_owned()), status)
                    .await?;
                Ok(())
            }
            Self::OAuth(api) => {
                api.rate_song(VideoID::from_raw(video_id.to_owned()), status)
                    .await?;
                Ok(())
            }
        }
    }

    pub async fn subscribe_artist(&self, channel_id: ArtistChannelID<'_>) -> Result<()> {
        match self {
            Self::Browser(api) => api
                .subscribe_artist(channel_id)
                .await
                .map_err(|error| anyhow::anyhow!(error.to_string())),
            Self::OAuth(api) => api
                .subscribe_artist(channel_id)
                .await
                .map_err(|error| anyhow::anyhow!(error.to_string())),
        }
    }

    pub async fn unsubscribe_artists(
        &self,
        channel_ids: impl IntoIterator<Item = ArtistChannelID<'static>>,
    ) -> Result<()> {
        let channel_ids = channel_ids.into_iter().collect::<Vec<_>>();
        match self {
            Self::Browser(api) => api
                .unsubscribe_artists(channel_ids.clone())
                .await
                .map_err(|error| anyhow::anyhow!(error.to_string())),
            Self::OAuth(api) => api
                .unsubscribe_artists(channel_ids)
                .await
                .map_err(|error| anyhow::anyhow!(error.to_string())),
        }
    }

    pub async fn add_video_to_playlist_with_token(
        &self,
        playlist_id: &str,
        video_id: &str,
    ) -> Result<Option<String>> {
        let playlist_id = playlist_mutation_id(playlist_id);
        let add_query = || {
            ytmapi_rs::query::playlist::AddPlaylistItemsQuery::new_from_videos(
                PlaylistID::from_raw(playlist_id.clone()),
                [VideoID::from_raw(video_id.to_owned())],
                DuplicateHandlingMode::Unhandled,
            )
        };
        match self {
            Self::Browser(api) => api.query(add_query()).await,
            Self::OAuth(api) => api.query(add_query()).await,
        }
        .map(|items| {
            items
                .into_iter()
                .next()
                .map(|item| item.set_video_id.get_raw().to_owned())
                .filter(|token| !token.trim().is_empty())
        })
        .map_err(Into::into)
    }

    pub async fn remove_video_from_playlist(
        &self,
        playlist_id: &str,
        set_video_id: &str,
    ) -> Result<()> {
        let playlist_id = playlist_mutation_id(playlist_id);
        match self {
            Self::Browser(api) => {
                api.remove_playlist_items(
                    PlaylistID::from_raw(playlist_id.clone()),
                    [SetVideoID::from_raw(set_video_id.to_owned())],
                )
                .await?;
                Ok(())
            }
            Self::OAuth(api) => {
                api.remove_playlist_items(
                    PlaylistID::from_raw(playlist_id),
                    [SetVideoID::from_raw(set_video_id.to_owned())],
                )
                .await?;
                Ok(())
            }
        }
    }

    pub async fn create_playlist(&self, name: &str, privacy: PrivacyStatus) -> Result<String> {
        let id = match self {
            Self::Browser(api) => {
                api.create_playlist(CreatePlaylistQuery::new(name, None, privacy.clone()))
                    .await?
            }
            Self::OAuth(api) => {
                api.create_playlist(CreatePlaylistQuery::new(name, None, privacy))
                    .await?
            }
        };
        Ok(id.get_raw().to_owned())
    }

    pub async fn delete_playlist(&self, playlist_id: &str) -> Result<()> {
        let playlist_id = playlist_mutation_id(playlist_id);
        match self {
            Self::Browser(api) => {
                api.delete_playlist(PlaylistID::from_raw(playlist_id.clone()))
                    .await?;
                Ok(())
            }
            Self::OAuth(api) => {
                api.delete_playlist(PlaylistID::from_raw(playlist_id))
                    .await?;
                Ok(())
            }
        }
    }

    pub async fn rename_playlist(&self, playlist_id: &str, name: &str) -> Result<()> {
        let playlist_id = playlist_mutation_id(playlist_id);
        let query = EditPlaylistQuery::new_title(PlaylistID::from_raw(playlist_id), name);
        match self {
            Self::Browser(api) => {
                let _ = api.edit_playlist(query).await?;
            }
            Self::OAuth(api) => {
                let _ = api.edit_playlist(query).await?;
            }
        }
        Ok(())
    }
}

fn playlist_mutation_id(playlist_id: &str) -> String {
    playlist_id
        .strip_prefix("VL")
        .unwrap_or(playlist_id)
        .to_owned()
}

fn playlist_browse_id(playlist_id: &str) -> String {
    if playlist_id.starts_with("VL") {
        playlist_id.to_owned()
    } else {
        format!("VL{playlist_id}")
    }
}

fn search_section_or_empty<T: Default>(kind: &str, result: ytmapi_rs::Result<T>) -> T {
    match result {
        Ok(items) => items,
        Err(err) => {
            log_search_section_error(kind, &err);
            T::default()
        }
    }
}

fn log_search_section_error(kind: &str, error: &ytmapi_rs::Error) {
    tracing::warn!(
        section = kind,
        diagnostic = %crate::observability::safe_error(
            crate::observability::DiagnosticCode::YOUTUBE_SEARCH_FAILED,
            crate::observability::ErrorCategory::Unavailable,
            error,
        ),
        "Unable to fetch YouTube Music search results"
    );
}

async fn read_oauth_token(path: &Path) -> Result<OAuthToken> {
    let file = tokio::fs::read_to_string(path)
        .await
        .with_context(|| format!("read YouTube Music OAuth token from {}", path.display()))?;
    serde_json::from_str(&file)
        .with_context(|| format!("parse YouTube Music OAuth token from {}", path.display()))
}

fn persist_oauth_token(path: &Path, token: &OAuthToken) -> Result<()> {
    let data = serde_json::to_vec_pretty(token)
        .context("serialize refreshed YouTube Music OAuth token")?;
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("create YouTube Music token folder {}", parent.display()))?;
    }
    atomicwrites::AtomicFile::new(path, atomicwrites::AllowOverwrite)
        .write(|file| std::io::Write::write_all(file, &data))
        .with_context(|| format!("persist YouTube Music OAuth token to {}", path.display()))?;
    restrict_token_permissions(path)?;
    Ok(())
}

#[cfg(unix)]
fn restrict_token_permissions(path: &Path) -> Result<()> {
    use std::os::unix::fs::PermissionsExt as _;

    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600)).with_context(|| {
        format!(
            "restrict YouTube Music token permissions on {}",
            path.display()
        )
    })
}

#[cfg(not(unix))]
#[allow(clippy::unnecessary_wraps)] // Keep the platform implementations interchangeable.
fn restrict_token_permissions(_path: &Path) -> Result<()> {
    Ok(())
}

fn track_from_search_result(result: SearchResultSong) -> YouTubeTrack {
    YouTubeTrack {
        id: result.video_id.get_raw().to_string(),
        name: result.title,
        artists: result.artist,
        album: result.album.map(|album| album.name),
        duration: result.duration,
        explicit: matches!(result.explicit, ytmapi_rs::common::Explicit::IsExplicit),
        thumbnail_url: result
            .thumbnails
            .into_iter()
            .last()
            .map(|thumbnail| thumbnail.url),
        is_video: false,
    }
}

/// `YouTube` Music exposes Liked Music as the account's special `LM` playlist.
/// The playlist browse endpoint expects the internal `VL` prefix.
async fn fetch_liked_playlist_tracks<A: AuthToken + LoggedIn>(
    api: &YtMusic<A>,
) -> Result<Vec<Vec<PlaylistItem>>> {
    let query = GetPlaylistTracksQuery::new(PlaylistID::from_raw("VLLM"));
    api.stream(&query)
        .try_collect::<Vec<_>>()
        .await
        .map_err(|error| {
            crate::observability::preserve_error_diagnostic(
                anyhow::Error::new(error).context("fetch YouTube liked music playlist"),
                crate::observability::DiagnosticCode::YOUTUBE_PLAYLIST_CONTEXT_FETCH_FAILED,
                crate::observability::ErrorCategory::Unavailable,
            )
        })
}

fn video_from_search_result(result: SearchResultVideo) -> YouTubeTrack {
    match result {
        SearchResultVideo::Video {
            title,
            channel_name,
            video_id,
            length,
            thumbnails,
            ..
        } => YouTubeTrack {
            id: video_id.get_raw().to_string(),
            name: title,
            artists: channel_name,
            album: None,
            duration: length,
            explicit: false,
            thumbnail_url: thumbnail_url(thumbnails),
            is_video: true,
        },
        SearchResultVideo::VideoEpisode {
            title,
            date: _,
            channel_name,
            episode_id,
            thumbnails,
            ..
        } => YouTubeTrack {
            id: episode_id.get_raw().to_string(),
            name: title,
            artists: channel_name,
            album: None,
            duration: String::new(),
            explicit: false,
            thumbnail_url: thumbnail_url(thumbnails),
            is_video: true,
        },
    }
}

fn search_artist(artist: SearchResultArtist) -> YouTubeLibraryArtist {
    YouTubeLibraryArtist {
        id: artist.browse_id.get_raw().to_string(),
        name: artist.artist,
        byline: artist.subscribers.unwrap_or_default(),
    }
}

fn search_playlist(playlist: SearchResultPlaylist) -> Option<YouTubeLibraryPlaylist> {
    match playlist {
        SearchResultPlaylist::Featured(playlist) => Some(YouTubeLibraryPlaylist {
            id: playlist.playlist_id.get_raw().to_string(),
            name: playlist.title,
            author: playlist.author,
            tracks: playlist.songs,
            thumbnail_url: thumbnail_url(playlist.thumbnails),
        }),
        SearchResultPlaylist::Community(playlist) => Some(YouTubeLibraryPlaylist {
            id: playlist.playlist_id.get_raw().to_string(),
            name: playlist.title,
            author: playlist.author,
            tracks: playlist.views,
            thumbnail_url: thumbnail_url(playlist.thumbnails),
        }),
        _ => None,
    }
}

fn search_podcast(podcast: SearchResultPodcast) -> YouTubePodcast {
    YouTubePodcast {
        id: podcast.podcast_id.get_raw().to_string(),
        name: podcast.title,
        publisher: podcast.publisher,
        thumbnail_url: thumbnail_url(podcast.thumbnails),
    }
}

fn search_episode(episode: SearchResultEpisode) -> YouTubeEpisode {
    YouTubeEpisode {
        track: YouTubeTrack {
            id: episode.episode_id.get_raw().to_string(),
            name: episode.title,
            artists: episode.channel_name,
            album: None,
            duration: String::new(),
            explicit: false,
            thumbnail_url: thumbnail_url(episode.thumbnails),
            is_video: true,
        },
        date: match episode.date {
            EpisodeDate::Live => "Live".to_string(),
            EpisodeDate::Recorded { date } => date,
        },
    }
}

fn playlist_item_to_track(item: PlaylistItem) -> Option<YouTubeTrack> {
    match item {
        PlaylistItem::Song(song) => Some(YouTubeTrack {
            id: song.video_id.get_raw().to_string(),
            name: song.title,
            artists: song
                .artists
                .iter()
                .map(|artist| artist.name.as_str())
                .collect::<Vec<_>>()
                .join(", "),
            album: Some(song.album.name),
            duration: song.duration,
            explicit: matches!(song.explicit, ytmapi_rs::common::Explicit::IsExplicit),
            thumbnail_url: thumbnail_url(song.thumbnails),
            is_video: false,
        }),
        PlaylistItem::Video(video) => Some(YouTubeTrack {
            id: video.video_id.get_raw().to_string(),
            name: video.title,
            artists: video.channel_name,
            album: None,
            duration: video.duration,
            explicit: false,
            thumbnail_url: thumbnail_url(video.thumbnails),
            is_video: true,
        }),
        PlaylistItem::UploadSong(song) => Some(YouTubeTrack {
            id: song.video_id.get_raw().to_string(),
            name: song.title,
            artists: song
                .artists
                .iter()
                .map(|artist| artist.name.as_str())
                .collect::<Vec<_>>()
                .join(", "),
            album: song.album.map(|album| album.name),
            duration: song.duration,
            explicit: false,
            thumbnail_url: thumbnail_url(song.thumbnails),
            is_video: false,
        }),
        PlaylistItem::Episode(_) => None,
    }
}

fn artist_song_to_track(song: ytmapi_rs::parse::ArtistSong) -> YouTubeTrack {
    YouTubeTrack {
        id: song.video_id.get_raw().to_string(),
        name: song.title,
        artists: song
            .artists
            .iter()
            .map(|artist| artist.name.as_str())
            .collect::<Vec<_>>()
            .join(", "),
        album: Some(song.album.name),
        duration: String::new(),
        explicit: matches!(song.explicit, ytmapi_rs::common::Explicit::IsExplicit),
        thumbnail_url: None,
        is_video: false,
    }
}

fn library_playlist(playlist: LibraryPlaylist) -> YouTubeLibraryPlaylist {
    YouTubeLibraryPlaylist {
        id: playlist.playlist_id.get_raw().to_string(),
        name: playlist.title,
        author: playlist.author,
        tracks: playlist.tracks,
        thumbnail_url: thumbnail_url(playlist.thumbnails),
    }
}

fn library_album(album: SearchResultAlbum) -> YouTubeLibraryAlbum {
    YouTubeLibraryAlbum {
        id: album.album_id.get_raw().to_string(),
        name: album.title,
        artist: album.artist,
        year: album.year,
        album_type: format!("{:?}", album.album_type),
        thumbnail_url: thumbnail_url(album.thumbnails),
    }
}

fn library_artist(artist: LibraryArtist) -> YouTubeLibraryArtist {
    YouTubeLibraryArtist {
        id: artist.channel_id.get_raw().to_string(),
        name: artist.artist,
        byline: artist.byline,
    }
}

fn thumbnail_url(thumbnails: Vec<Thumbnail>) -> Option<String> {
    thumbnails.into_iter().last().map(|thumbnail| thumbnail.url)
}

fn log_library_fetch_error(_kind: &str, err: &ytmapi_rs::Error) {
    if is_missing_library_grid_error(err) {
        crate::observability::log_safe_error!(
            info,
            crate::observability::DiagnosticCode::YOUTUBE_LIBRARY_GRID_MISSING,
            crate::observability::ErrorCategory::Contract,
            err,
            "YouTube Music library response had no grid; treating it as an empty pane"
        );
    } else {
        crate::observability::log_safe_error!(
            warn,
            crate::observability::DiagnosticCode::YOUTUBE_LIBRARY_FETCH_FAILED,
            crate::observability::ErrorCategory::Unavailable,
            err,
            "Unable to fetch a YouTube Music library pane"
        );
    }
}

fn library_error_message(kind: &str, missing_grid: bool) -> String {
    if missing_grid {
        format!("No {kind} were returned by the authenticated library endpoint")
    } else {
        format!("The YouTube Music {kind} section could not be loaded")
    }
}

fn library_truncated_message(kind: &str) -> String {
    format!(
        "The YouTube Music {kind} section is large; only the first {MAX_LIBRARY_PAGES} pages were loaded"
    )
}

fn is_missing_library_grid_error(err: &ytmapi_rs::Error) -> bool {
    err.to_string()
        .contains("gridRenderer not found in Api response")
}

#[cfg(test)]
mod tests {
    use std::{cell::Cell, sync::Arc};

    use super::{
        collect_library_pages, context_response_retry_reason, library_error_message,
        playlist_browse_id, playlist_context_title, playlist_mutation_id, try_seek_active_sink,
        with_context_response_retry, SeekAttempt, SeekSink, YouTubeAudioOutput,
        YouTubeLocalPlayback, YouTubeLocalPlayer, MAX_LIBRARY_PAGES,
    };

    struct FakeSeekSink {
        empty_before_seek: bool,
        empty_after_seek: bool,
        sought: Cell<bool>,
    }

    impl SeekSink for FakeSeekSink {
        fn is_empty(&self) -> bool {
            if self.sought.get() {
                self.empty_after_seek
            } else {
                self.empty_before_seek
            }
        }

        fn try_seek_to(&self, _position: std::time::Duration) -> std::result::Result<(), String> {
            self.sought.set(true);
            Ok(())
        }
    }

    #[test]
    fn playlist_mutations_use_canonical_playlist_ids() {
        assert_eq!(playlist_mutation_id("VLPL123"), "PL123");
        assert_eq!(playlist_mutation_id("PL123"), "PL123");
    }

    #[test]
    fn playlist_reads_use_browse_ids() {
        assert_eq!(playlist_browse_id("PL123"), "VLPL123");
        assert_eq!(playlist_browse_id("VLPL123"), "VLPL123");
        assert_eq!(playlist_browse_id("LM"), "VLLM");
    }

    #[test]
    fn library_error_messages_are_safe_and_bounded() {
        let warning = library_error_message("playlists", false);
        assert_eq!(
            warning,
            "The YouTube Music playlists section could not be loaded"
        );
        assert!(!warning.contains("private payload"));
        assert_eq!(
            library_error_message("albums", true),
            "No albums were returned by the authenticated library endpoint"
        );
    }

    #[test]
    fn context_retry_policy_only_covers_response_contract_errors() {
        let invalid_response = ytmapi_rs::error::ErrorKind::InvalidResponse {
            response: "not-json".to_owned(),
        };
        assert_eq!(
            context_response_retry_reason(&invalid_response),
            Some("invalid_response")
        );

        let transport = ytmapi_rs::error::ErrorKind::Web {
            message: "offline".to_owned(),
        };
        assert_eq!(context_response_retry_reason(&transport), None);
    }

    #[tokio::test]
    async fn context_response_contract_failure_is_retried_once() {
        let attempts = Cell::new(0);
        let output = with_context_response_retry("artist", || {
            let attempt = attempts.get();
            attempts.set(attempt + 1);
            async move {
                if attempt == 0 {
                    Err(ytmapi_rs::error::ErrorKind::InvalidResponse {
                        response: "not-json".to_owned(),
                    }
                    .into())
                } else {
                    Ok("loaded")
                }
            }
        })
        .await;

        assert_eq!(output.unwrap(), "loaded");
        assert_eq!(attempts.get(), 2);
    }

    #[tokio::test]
    async fn persistent_context_response_contract_failure_stops_after_retry() {
        let attempts = Cell::new(0);
        let output: ytmapi_rs::Result<()> = with_context_response_retry("artist", || {
            attempts.set(attempts.get() + 1);
            async {
                Err(ytmapi_rs::error::ErrorKind::InvalidResponse {
                    response: "not-json".to_owned(),
                }
                .into())
            }
        })
        .await;

        assert!(output.is_err());
        assert_eq!(attempts.get(), 2);
    }

    #[tokio::test]
    async fn context_non_contract_failure_is_not_retried() {
        let attempts = Cell::new(0);
        let output: ytmapi_rs::Result<()> = with_context_response_retry("artist", || {
            attempts.set(attempts.get() + 1);
            async {
                Err(ytmapi_rs::error::ErrorKind::Web {
                    message: "offline".to_owned(),
                }
                .into())
            }
        })
        .await;

        assert!(output.is_err());
        assert_eq!(attempts.get(), 1);
    }

    #[test]
    fn playlist_context_title_has_a_safe_generic_fallback() {
        assert_eq!(playlist_context_title(None), "YouTube Playlist");
        assert_eq!(
            playlist_context_title(Some("  ".to_owned())),
            "YouTube Playlist"
        );
        assert_eq!(
            playlist_context_title(Some("Road trip".to_owned())),
            "Road trip"
        );
    }

    #[test]
    fn local_player_can_cross_runtime_worker_boundaries() {
        fn assert_send<T: Send>() {}
        assert_send::<YouTubeLocalPlayer>();
    }

    fn player_with_sink(sink: Arc<rodio::Sink>) -> YouTubeLocalPlayer {
        YouTubeLocalPlayer {
            audio_output: None,
            suspended: None,
            playback: Some(YouTubeLocalPlayback {
                sink,
                track_id: "same-track".to_owned(),
                track: crate::state::YouTubeTrack {
                    id: "same-track".to_owned(),
                    name: "Track".to_owned(),
                    artists: "Artist".to_owned(),
                    album: None,
                    duration: "1:00".to_owned(),
                    explicit: false,
                    thumbnail_url: None,
                    is_video: false,
                },
                source: super::playback::ResolvedAudioSource {
                    media_id: "same-track".to_owned(),
                    #[cfg(feature = "private-capture")]
                    itag: 140,
                    url: reqwest::Url::parse("https://example.googlevideo.com/audio").unwrap(),
                    required_headers: reqwest::header::HeaderMap::new(),
                    mime_type: "audio/mp4".to_owned(),
                    bitrate: 128_000,
                    content_length: Some(1),
                    duration: Some(std::time::Duration::from_secs(60)),
                    expires_at_unix: None,
                    source_client: "TEST",
                },
                base_position: std::time::Duration::from_secs(12),
            }),
            volume: 100,
            mute_state: None,
        }
    }

    fn sink_with_audio(mixer: &rodio::mixer::Mixer) -> Arc<rodio::Sink> {
        let sink = Arc::new(rodio::Sink::connect_new(mixer));
        sink.append(rodio::source::Zero::new(2, 44_100));
        sink
    }

    #[test]
    fn monitor_snapshot_rejects_a_replaced_sink_for_the_same_track() {
        let (mixer, _source) = rodio::mixer::mixer(2, 44_100);
        let active_sink = Arc::new(rodio::Sink::connect_new(&mixer));
        let replaced_sink = Arc::new(rodio::Sink::connect_new(&mixer));
        let player = player_with_sink(active_sink.clone());

        assert!(player.snapshot_for_sink(&active_sink).is_some());
        assert!(player.snapshot_for_sink(&replaced_sink).is_none());
    }

    #[test]
    fn suspend_releases_playback_but_keeps_a_paused_restart_point() {
        let (mixer, _source) = rodio::mixer::mixer(2, 44_100);
        let mut player = player_with_sink(sink_with_audio(&mixer));

        let snapshot = player.suspend().unwrap().unwrap();

        assert!(!snapshot.is_playing);
        assert!(player.active_sink().is_none());
        assert!(player.resume().is_none());
        let restart = player.restart_info().unwrap();
        assert_eq!(restart.track.id, "same-track");
        assert_eq!(restart.progress, std::time::Duration::from_secs(12));
        assert!(!restart.was_playing);
    }

    #[test]
    fn stop_discards_a_suspended_restart_point() {
        let (mixer, _source) = rodio::mixer::mixer(2, 44_100);
        let mut player = player_with_sink(sink_with_audio(&mixer));
        player.suspend().unwrap();

        player.stop();

        assert!(player.restart_info().is_none());
    }

    #[test]
    fn unused_output_release_keeps_an_output_with_active_playback() {
        let (mixer, _source) = rodio::mixer::mixer(2, 44_100);
        let mut player = player_with_sink(sink_with_audio(&mixer));
        assert!(!player.release_output_if_unused().unwrap());

        player.stop();
        // No output was opened in this test, so there is nothing to release.
        assert!(!player.release_output_if_unused().unwrap());
    }

    #[test]
    fn idle_suspend_skips_resumed_or_replaced_playback() {
        let (mixer, _source) = rodio::mixer::mixer(2, 44_100);
        let sink = sink_with_audio(&mixer);
        let mut player = player_with_sink(sink.clone());

        // Still playing: the delayed release must not interrupt it.
        assert!(!player.suspend_if_idle(&sink).unwrap());
        player.pause();
        let replaced = Arc::new(rodio::Sink::connect_new(&mixer));
        assert!(!player.suspend_if_idle(&replaced).unwrap());
        assert!(player.active_sink().is_some());

        assert!(player.suspend_if_idle(&sink).unwrap());
        assert!(player.active_sink().is_none());
    }

    #[test]
    fn audio_output_worker_shutdown_is_joined_and_idempotent() {
        let (mixer, _source) = rodio::mixer::mixer(2, 44_100);
        let (shutdown_tx, shutdown_rx) = std::sync::mpsc::channel();
        let worker = std::thread::spawn(move || {
            let _ = shutdown_rx.recv();
        });
        let mut output = YouTubeAudioOutput {
            mixer,
            shutdown_tx: Some(shutdown_tx),
            worker: Some(worker),
        };

        output.stop_and_join().unwrap();
        output.stop_and_join().unwrap();
    }

    #[test]
    fn seek_that_exhausts_the_sink_requires_same_track_restart() {
        let sink = FakeSeekSink {
            empty_before_seek: false,
            empty_after_seek: true,
            sought: Cell::new(false),
        };

        assert_eq!(
            try_seek_active_sink(&sink, std::time::Duration::from_secs(15)).unwrap(),
            SeekAttempt::RestartRequired
        );
    }

    #[test]
    fn seek_that_keeps_audio_available_applies_in_place() {
        let sink = FakeSeekSink {
            empty_before_seek: false,
            empty_after_seek: false,
            sought: Cell::new(false),
        };

        assert_eq!(
            try_seek_active_sink(&sink, std::time::Duration::from_secs(15)).unwrap(),
            SeekAttempt::Applied
        );
    }

    #[tokio::test]
    async fn library_continuations_are_bounded_and_mark_truncation() {
        let pages = (0..=MAX_LIBRARY_PAGES)
            .map(|page| Ok(vec![page]))
            .collect::<Vec<ytmapi_rs::Result<Vec<usize>>>>();
        let (items, truncated) = collect_library_pages(futures::stream::iter(pages))
            .await
            .expect("library pages should collect");

        assert!(truncated);
        assert_eq!(items.len(), MAX_LIBRARY_PAGES);
        assert_eq!(items[0], 0);
        assert_eq!(items[items.len() - 1], MAX_LIBRARY_PAGES - 1);
    }
}
