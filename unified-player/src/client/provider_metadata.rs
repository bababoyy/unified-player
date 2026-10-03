use std::{
    error::Error as StdError,
    fmt,
    future::Future,
    io::Write,
    time::{Duration, Instant},
};

use anyhow::{Context as _, Result};
use futures::StreamExt as _;
use librespot_core::SpotifyUri;
use librespot_metadata::Metadata as _;
use reqwest::{header::HeaderMap, StatusCode};
use rspotify::{http::Query, prelude::*};
use serde::Deserialize;

#[cfg(feature = "image")]
use crate::state::TTL_CACHE_DURATION;
use crate::{
    config,
    state::{
        Album, AlbumId, Artist, ArtistId, Context, Playlist, PlaylistId, SharedState, Show, ShowId,
        Track, TrackId, UserId,
    },
};

use super::AppClient;

pub(super) const SPOTIFY_API_ENDPOINT: &str = "https://api.spotify.com/v1";
const SPOTIFY_API_MAX_RETRIES: usize = 3;
const SPOTIFY_CONTEXT_MAX_RETRIES: usize = 2;
pub(super) const DEFAULT_RATE_LIMIT_DELAY: Duration = Duration::from_secs(2);
const DEFAULT_CONTEXT_SERVER_RETRY_DELAY: Duration = Duration::from_millis(300);
const PAGING_REQUEST_DELAY: Duration = Duration::from_millis(150);
const INTEGRATED_PLAYLIST_METADATA_CONCURRENCY: usize = 8;

#[derive(Debug, Deserialize)]
struct SpotifyPlaylistItemsSummary {
    total: u32,
}

#[derive(Debug, Deserialize)]
struct SpotifyPlaylistOwner {
    #[serde(default)]
    display_name: Option<String>,
    id: String,
}

/// Spotify deliberately omits both item collection fields for playlists that
/// the current user neither owns nor collaborates on. Keep the metadata shape
/// tolerant so that absence can select the integrated read path instead of
/// triggering rspotify's `missing items/tracks` panic.
#[derive(Debug, Deserialize)]
struct SpotifyPlaylistMetadata {
    collaborative: bool,
    #[serde(default)]
    description: Option<String>,
    name: String,
    owner: SpotifyPlaylistOwner,
    snapshot_id: String,
    #[serde(default)]
    items: Option<SpotifyPlaylistItemsSummary>,
    #[serde(default)]
    tracks: Option<SpotifyPlaylistItemsSummary>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum SpotifyPlaylistReadPath {
    WebApi { total: usize },
    Integrated,
}

impl SpotifyPlaylistMetadata {
    fn read_path(&self) -> SpotifyPlaylistReadPath {
        self.items.as_ref().or(self.tracks.as_ref()).map_or(
            SpotifyPlaylistReadPath::Integrated,
            |items| SpotifyPlaylistReadPath::WebApi {
                total: items.total as usize,
            },
        )
    }

    fn into_playlist(
        self,
        playlist_id: PlaylistId<'static>,
        read_path: SpotifyPlaylistReadPath,
    ) -> Result<Playlist> {
        let owner_id = UserId::from_id(self.owner.id)?.into_static();
        let description = self.description.unwrap_or_default();
        let tags = regex::Regex::new("(<.*?>|</.*?>)").expect("valid regex");
        let description =
            html_escape::decode_html_entities(&tags.replace_all(&description, "")).to_string();

        Ok(Playlist {
            id: playlist_id,
            // `collaborative` describes the playlist, not whether this user is
            // a collaborator. The Web API only includes `items` when the user
            // actually owns or collaborates on it, so an integrated fallback
            // must remain read-only even if the public flag is true.
            collaborative: self.collaborative
                && matches!(read_path, SpotifyPlaylistReadPath::WebApi { .. }),
            name: self.name,
            owner: (self.owner.display_name.unwrap_or_default(), owner_id),
            desc: description,
            current_folder_id: 0,
            snapshot_id: self.snapshot_id,
        })
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum SpotifyThrottleAction {
    NotThrottled,
    DeferAndRetry,
    FailQuotaExceeded,
}

fn spotify_throttle_action(status: StatusCode, response_text: &str) -> SpotifyThrottleAction {
    if status != StatusCode::TOO_MANY_REQUESTS {
        return SpotifyThrottleAction::NotThrottled;
    }

    let quota_exceeded = serde_json::from_str::<serde_json::Value>(response_text)
        .ok()
        .is_some_and(|body| {
            body.pointer("/error/reason")
                .and_then(serde_json::Value::as_str)
                == Some("QUOTA_EXCEEDED")
        });
    if quota_exceeded {
        SpotifyThrottleAction::FailQuotaExceeded
    } else {
        SpotifyThrottleAction::DeferAndRetry
    }
}

fn is_retryable_spotify_context_status(status: StatusCode) -> bool {
    status == StatusCode::TOO_MANY_REQUESTS || status.is_server_error()
}

fn context_server_retry_delay(attempt: usize) -> Duration {
    let multiplier = (attempt as u32).saturating_add(1);
    DEFAULT_CONTEXT_SERVER_RETRY_DELAY.saturating_mul(multiplier)
}

const fn should_fetch_listenbrainz_artist_fallback(
    spotify_top_tracks_failed: bool,
    spotify_albums_empty: bool,
    listenbrainz_enabled: bool,
) -> bool {
    (spotify_top_tracks_failed || spotify_albums_empty) && listenbrainz_enabled
}

fn artist_albums_or_empty(result: Result<Vec<Album>>) -> Vec<Album> {
    match result {
        Ok(albums) => albums,
        Err(error) => {
            let category = artist_albums_error_category(&error);
            crate::observability::log_safe_error!(
                warn,
                crate::observability::DiagnosticCode::SPOTIFY_ARTIST_ALBUMS_FAILED,
                category,
                &error,
                "Spotify artist albums are unavailable; continuing with partial context"
            );
            Vec::new()
        }
    }
}

fn artist_albums_error_category(error: &anyhow::Error) -> crate::observability::ErrorCategory {
    if spotify_api_status_code(error) == Some(429) {
        crate::observability::ErrorCategory::RateLimited
    } else {
        crate::observability::ErrorCategory::Unavailable
    }
}

fn spotify_context_error_status(error: &rspotify::ClientError) -> Option<StatusCode> {
    let rspotify::ClientError::Http(http_error) = error else {
        return None;
    };
    let rspotify::http::HttpError::StatusCode(response) = http_error.as_ref() else {
        return None;
    };
    Some(response.status())
}

/// The HTTP status behind a failed Spotify Web API call, from either the
/// app's own request helpers or rspotify.
pub(super) fn spotify_error_status(error: &anyhow::Error) -> Option<StatusCode> {
    if let Some(status) = error.downcast_ref::<SpotifyApiStatusError>() {
        return Some(status.status);
    }
    let rspotify::ClientError::Http(http) = error.downcast_ref::<rspotify::ClientError>()? else {
        return None;
    };
    if let rspotify::http::HttpError::StatusCode(response) = http.as_ref() {
        Some(response.status())
    } else {
        None
    }
}

#[derive(Debug)]
pub(super) struct SpotifyApiStatusError {
    method: &'static str,
    status: StatusCode,
}

impl fmt::Display for SpotifyApiStatusError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            formatter,
            "Spotify API {} failed with status {}",
            self.method, self.status
        )
    }
}

impl StdError for SpotifyApiStatusError {}

pub(super) fn spotify_api_status_code(error: &anyhow::Error) -> Option<u16> {
    error.chain().find_map(|cause| {
        cause
            .downcast_ref::<SpotifyApiStatusError>()
            .map(|status| status.status.as_u16())
    })
}

/// A missing active Spotify device is an idempotent stopped state when it is
/// observed while reconciling a failed playback command. Keep this decision
/// tied to the structured response status instead of parsing display text.
pub(super) fn is_spotify_no_active_device(error: &anyhow::Error) -> bool {
    error.chain().any(|cause| {
        cause
            .downcast_ref::<SpotifyApiStatusError>()
            .is_some_and(|status| status.method == "GET" && status.status == StatusCode::NOT_FOUND)
    })
}

impl AppClient {
    /// Keep rspotify metadata calls on the same bounded backoff path as the
    /// app-owned Spotify HTTP helpers.
    async fn with_spotify_context_retry<T, F, Fut>(
        &self,
        operation: &'static str,
        mut request: F,
    ) -> Result<T>
    where
        F: FnMut() -> Fut,
        Fut: Future<Output = rspotify::ClientResult<T>>,
    {
        for attempt in 0..=SPOTIFY_CONTEXT_MAX_RETRIES {
            self.wait_for_spotify_rate_limit().await;
            match request().await {
                Ok(value) => return Ok(value),
                Err(error) => {
                    let Some(status) = spotify_context_error_status(&error) else {
                        return Err(error.into());
                    };
                    let Some(delay) = self.spotify_context_retry_delay(&error, attempt) else {
                        tracing::warn!(
                            operation,
                            status = status.as_u16(),
                            "Spotify context request failed without retry"
                        );
                        return Err(error.into());
                    };

                    if attempt == SPOTIFY_CONTEXT_MAX_RETRIES {
                        tracing::warn!(
                            operation,
                            status = status.as_u16(),
                            attempt = attempt + 1,
                            "Spotify context retry budget exhausted"
                        );
                        return Err(error.into());
                    }

                    tracing::warn!(
                        operation,
                        status = status.as_u16(),
                        attempt = attempt + 1,
                        retry_after_ms = delay.as_millis(),
                        "Spotify context request was temporarily unavailable; retrying"
                    );
                    tokio::time::sleep(delay).await;
                }
            }
        }

        unreachable!("context retry loop always returns")
    }

    fn spotify_context_retry_delay(
        &self,
        error: &rspotify::ClientError,
        attempt: usize,
    ) -> Option<Duration> {
        let status = spotify_context_error_status(error)?;
        if !is_retryable_spotify_context_status(status) {
            return None;
        }

        let delay = if status == StatusCode::TOO_MANY_REQUESTS {
            let rspotify::ClientError::Http(http_error) = error else {
                return None;
            };
            let rspotify::http::HttpError::StatusCode(response) = http_error.as_ref() else {
                return None;
            };
            self.defer_spotify_requests(response.headers())
        } else {
            context_server_retry_delay(attempt)
        };

        Some(delay)
    }

    /// Get a track data
    pub async fn track(&self, track_id: TrackId<'_>) -> Result<Track> {
        Track::try_from_full_track(
            self.spotify_api()
                .track(track_id, Some(rspotify::model::Market::FromToken))
                .await?,
        )
        .context("convert FullTrack into Track")
    }

    /// Get a playlist context data
    pub async fn playlist_context(&self, playlist_id: PlaylistId<'_>) -> Result<Context> {
        tracing::info!("Fetching Spotify playlist context");

        let playlist = self.spotify_playlist_metadata(&playlist_id).await?;
        let read_path = playlist.read_path();

        let tracks = match read_path {
            SpotifyPlaylistReadPath::WebApi { total } => self
                .all_paging_items(
                    &format!(
                        "{SPOTIFY_API_ENDPOINT}/playlists/{}/items",
                        playlist_id.id(),
                    ),
                    total,
                )
                .await?
                .into_iter()
                .filter_map(Track::try_from_playlist_item)
                .collect::<Vec<_>>(),
            SpotifyPlaylistReadPath::Integrated => {
                tracing::info!(
                    "Spotify Web API omitted playlist contents; using integrated metadata"
                );
                self.integrated_playlist_tracks(&playlist_id).await?
            }
        };

        let playlist = playlist.into_playlist(playlist_id.clone_static(), read_path)?;

        Ok(Context::Playlist { playlist, tracks })
    }

    async fn spotify_playlist_metadata(
        &self,
        playlist_id: &PlaylistId<'_>,
    ) -> Result<SpotifyPlaylistMetadata> {
        let path = format!("playlists/{}", playlist_id.id());
        let query = Query::from([("market", "from_token")]);

        for attempt in 0..=SPOTIFY_CONTEXT_MAX_RETRIES {
            match self.spotify_api_get_text(&path, &query).await {
                Ok(text) => {
                    return serde_json::from_str(&text)
                        .context("parse Spotify playlist metadata response")
                }
                Err(error) => {
                    let is_server_error = spotify_api_status_code(&error)
                        .and_then(|status| StatusCode::from_u16(status).ok())
                        .is_some_and(|status| status.is_server_error());
                    if !is_server_error || attempt == SPOTIFY_CONTEXT_MAX_RETRIES {
                        return Err(error);
                    }

                    let delay = context_server_retry_delay(attempt);
                    tracing::warn!(
                        operation = "playlist",
                        attempt = attempt + 1,
                        retry_after_ms = delay.as_millis(),
                        "Spotify context request was temporarily unavailable; retrying"
                    );
                    tokio::time::sleep(delay).await;
                }
            }
        }

        unreachable!("playlist metadata retry loop always returns")
    }

    async fn integrated_playlist_tracks(&self, playlist_id: &PlaylistId<'_>) -> Result<Vec<Track>> {
        let session = self
            .spotify
            .session_if_present()
            .await
            .context("integrated Spotify session is unavailable")?;
        let playlist_uri = SpotifyUri::from_uri(&playlist_id.uri())
            .context("convert Spotify playlist URI for integrated metadata")?;
        let playlist = librespot_metadata::Playlist::get(&session, &playlist_uri)
            .await
            .context("load playlist through integrated Spotify metadata")?;

        anyhow::ensure!(
            !playlist.contents.is_truncated,
            "integrated Spotify playlist metadata returned a truncated item list"
        );

        let jobs = playlist
            .contents
            .items
            .0
            .into_iter()
            .enumerate()
            .filter_map(|(index, item)| {
                matches!(item.id, SpotifyUri::Track { .. }).then_some((index, item))
            })
            .collect::<Vec<_>>();
        let expected_tracks = jobs.len();

        let mut resolved = futures::stream::iter(jobs.into_iter().map(|(index, item)| {
            let session = session.clone();
            async move {
                let added_at_ms = item.attributes.timestamp.as_timestamp_ms();
                let metadata = librespot_metadata::Track::get(&session, &item.id).await;
                (index, added_at_ms, metadata)
            }
        }))
        .buffer_unordered(INTEGRATED_PLAYLIST_METADATA_CONCURRENCY)
        .collect::<Vec<_>>()
        .await;
        resolved.sort_by_key(|(index, _, _)| *index);

        let mut failed = 0usize;
        let tracks = resolved
            .into_iter()
            .filter_map(|(_, added_at_ms, result)| {
                let track = result
                    .ok()
                    .and_then(|track| track_from_integrated_metadata(track, added_at_ms));
                if track.is_none() {
                    failed += 1;
                }
                track
            })
            .collect::<Vec<_>>();

        if failed > 0 {
            tracing::warn!(
                failed_track_count = failed,
                expected_track_count = expected_tracks,
                "Some integrated Spotify playlist tracks could not be resolved"
            );
        }
        anyhow::ensure!(
            expected_tracks == 0 || !tracks.is_empty(),
            "integrated Spotify playlist track metadata is unavailable"
        );

        Ok(tracks)
    }

    /// Get an album context data
    pub async fn album_context(&self, album_id: AlbumId<'_>) -> Result<Context> {
        tracing::info!("Fetching Spotify album context");

        let album = self
            .with_spotify_context_retry("album", || async {
                self.spotify_api()
                    .album(album_id.clone(), Some(rspotify::model::Market::FromToken))
                    .await
            })
            .await?;

        let total_tracks = album.tracks.total as usize;

        // converts `rspotify::model::FullAlbum` into `state::Album`
        let album: Album = album.into();

        // get the album's tracks
        let tracks = self
            .all_paging_items(
                &format!("{SPOTIFY_API_ENDPOINT}/albums/{}/tracks", album_id.id()),
                total_tracks,
            )
            .await?
            .into_iter()
            .filter_map(|t| {
                // simplified track doesn't have album so
                // we need to manually include one during
                // converting into `state::Track`
                Track::try_from_simplified_track(t).map(|mut t| {
                    t.album = Some(album.clone());
                    t
                })
            })
            .collect::<Vec<_>>();

        Ok(Context::Album { album, tracks })
    }

    /// Get an artist context data
    pub async fn artist_context(&self, artist_id: ArtistId<'_>) -> Result<Context> {
        tracing::info!("Fetching Spotify artist context");

        // get the artist's information, including top tracks, related artists, and albums

        let artist = self
            .with_spotify_context_retry("artist", || async {
                self.spotify_api().artist(artist_id.as_ref()).await
            })
            .await
            .context("get artist")?
            .into();

        #[allow(deprecated)]
        let (top_tracks, spotify_top_tracks_failed) = match self
            .spotify_api()
            .artist_top_tracks(artist_id.as_ref(), Some(rspotify::model::Market::FromToken))
            .await
        {
            Ok(tracks) => (
                tracks
                    .into_iter()
                    .filter_map(Track::try_from_full_track)
                    .collect::<Vec<_>>(),
                false,
            ),
            Err(err) => {
                crate::observability::log_safe_error!(
                    warn,
                    crate::observability::DiagnosticCode::SPOTIFY_ARTIST_TRACKS_FAILED,
                    crate::observability::ErrorCategory::Unavailable,
                    &err,
                    "Spotify artist top-tracks endpoint is unavailable for this client"
                );
                (Vec::new(), true)
            }
        };

        let albums = artist_albums_or_empty(self.artist_albums(artist_id.as_ref()).await);

        let listenbrainz_config = &config::get_config().app_config.listenbrainz;
        let listenbrainz_enabled =
            listenbrainz_config.enabled && listenbrainz_config.artist_enrichment;
        let listenbrainz = if should_fetch_listenbrainz_artist_fallback(
            spotify_top_tracks_failed,
            albums.is_empty(),
            listenbrainz_enabled,
        ) {
            crate::state::ListenBrainzArtistEnrichment::Pending
        } else {
            crate::state::ListenBrainzArtistEnrichment::NotRequested
        };

        #[allow(deprecated)]
        let related_artists = self
            .spotify_api()
            .artist_related_artists(artist_id.as_ref())
            .await
            .ok()
            .unwrap_or_default()
            .into_iter()
            .map(std::convert::Into::into)
            .collect::<Vec<_>>();

        Ok(Context::Artist {
            artist,
            top_tracks,
            listenbrainz,
            albums,
            related_artists,
        })
    }

    /// Get a show context data
    pub async fn show_context(&self, show_id: ShowId<'_>) -> Result<Context> {
        tracing::info!("Fetching Spotify show context");

        let show = self
            .with_spotify_context_retry("show", || async {
                self.spotify_api().get_a_show(show_id.clone(), None).await
            })
            .await?;

        // get the show's episodes
        let episodes = self
            .all_paging_items::<rspotify::model::SimplifiedEpisode>(
                &format!("{SPOTIFY_API_ENDPOINT}/shows/{}/episodes", show_id.id()),
                show.episodes.total as usize,
            )
            .await?
            .into_iter()
            .map(std::convert::Into::into)
            .collect::<Vec<_>>();

        // converts `rspotify::model::FullShow` into `state::Show`
        let show: Show = show.into();

        Ok(Context::Show { show, episodes })
    }

    fn spotify_api_url(path: &str) -> String {
        format!("{SPOTIFY_API_ENDPOINT}/{}", path.trim_start_matches('/'))
    }

    pub(super) fn append_query_param(path: &str, key: &str, value: &str) -> String {
        let separator = if path.contains('?') { '&' } else { '?' };
        format!("{path}{separator}{key}={value}")
    }

    async fn wait_for_spotify_rate_limit(&self) {
        loop {
            let delay = {
                let until = *self
                    .spotify_rate_limit_until
                    .lock()
                    .expect("rate-limit mutex poisoned");
                until.and_then(|until| until.checked_duration_since(Instant::now()))
            };

            match delay {
                Some(delay) if !delay.is_zero() => tokio::time::sleep(delay).await,
                _ => return,
            }
        }
    }

    fn defer_spotify_requests(&self, headers: &HeaderMap) -> Duration {
        let delay = headers
            .get(reqwest::header::RETRY_AFTER)
            .and_then(|value| value.to_str().ok())
            .and_then(|value| value.parse::<u64>().ok())
            .map(Duration::from_secs)
            .filter(|delay| !delay.is_zero())
            .unwrap_or(DEFAULT_RATE_LIMIT_DELAY);
        let until = Instant::now() + delay;

        let mut current_until = self
            .spotify_rate_limit_until
            .lock()
            .expect("rate-limit mutex poisoned");
        if current_until.is_none_or(|current_until| until > current_until) {
            *current_until = Some(until);
        }

        delay
    }

    fn handle_spotify_throttle(
        &self,
        method: &'static str,
        status: StatusCode,
        headers: &HeaderMap,
        response_text: &str,
        attempt: usize,
    ) -> Result<bool> {
        match spotify_throttle_action(status, response_text) {
            SpotifyThrottleAction::FailQuotaExceeded => {
                tracing::warn!(
                    method,
                    status_class = "quota_exceeded",
                    retryable = false,
                    "Spotify API quota bucket is exhausted"
                );
                Err(SpotifyApiStatusError { method, status }.into())
            }
            SpotifyThrottleAction::DeferAndRetry => {
                let delay = self.defer_spotify_requests(headers);
                let should_retry = attempt < SPOTIFY_API_MAX_RETRIES;
                if should_retry {
                    tracing::warn!(
                        method,
                        attempt,
                        retry_after_ms = delay.as_millis(),
                        "Spotify API request was rate-limited; retrying"
                    );
                }
                Ok(should_retry)
            }
            SpotifyThrottleAction::NotThrottled => Ok(false),
        }
    }

    pub(super) fn defer_spotify_rate_limit_error(
        &self,
        error: &rspotify::ClientError,
    ) -> Option<Duration> {
        let rspotify::ClientError::Http(http_error) = error else {
            return None;
        };
        let rspotify::http::HttpError::StatusCode(response) = http_error.as_ref() else {
            return None;
        };
        (response.status() == StatusCode::TOO_MANY_REQUESTS)
            .then(|| self.defer_spotify_requests(response.headers()))
    }

    pub(super) fn spotify_rate_limit_remaining(&self) -> Option<Duration> {
        self.spotify_rate_limit_until
            .lock()
            .expect("rate-limit mutex poisoned")
            .and_then(|until| until.checked_duration_since(Instant::now()))
    }

    pub(super) async fn spotify_api_get_text(
        &self,
        path: &str,
        payload: &Query<'_>,
    ) -> Result<String> {
        let access_token = self.token().await.context("get token")?;
        let url = Self::spotify_api_url(path);

        for attempt in 0..=SPOTIFY_API_MAX_RETRIES {
            self.wait_for_spotify_rate_limit().await;
            tracing::debug!(method = "GET", attempt, "Sending Spotify API request");

            let response = self
                .http
                .get(&url)
                .query(payload)
                .header(
                    reqwest::header::AUTHORIZATION,
                    format!("Bearer {access_token}"),
                )
                .send()
                .await?;

            let status = response.status();
            let headers = response.headers().clone();
            let text = response.text().await?;
            tracing::debug!(
                status = status.as_u16(),
                response_bytes = text.len(),
                "Received Spotify API response"
            );

            if self.handle_spotify_throttle("GET", status, &headers, &text, attempt)? {
                continue;
            }

            if !status.is_success() {
                return Err(SpotifyApiStatusError {
                    method: "GET",
                    status,
                }
                .into());
            }

            return Ok(text);
        }

        unreachable!("retry loop always returns or bails")
    }

    pub(super) async fn spotify_api_put_empty(&self, path: &str) -> Result<()> {
        let access_token = self.token().await.context("get token")?;
        let url = Self::spotify_api_url(path);

        for attempt in 0..=SPOTIFY_API_MAX_RETRIES {
            self.wait_for_spotify_rate_limit().await;
            tracing::debug!(method = "PUT", attempt, "Sending Spotify API request");

            let response = self
                .http
                .put(&url)
                .header(
                    reqwest::header::AUTHORIZATION,
                    format!("Bearer {access_token}"),
                )
                .send()
                .await?;

            let status = response.status();
            let headers = response.headers().clone();
            let text = response.text().await?;
            tracing::debug!(
                status = status.as_u16(),
                response_bytes = text.len(),
                "Received Spotify API response"
            );

            if self.handle_spotify_throttle("PUT", status, &headers, &text, attempt)? {
                continue;
            }

            if !status.is_success() {
                anyhow::bail!("Spotify API PUT failed with status {status}");
            }

            return Ok(());
        }

        unreachable!("retry loop always returns or bails")
    }

    /// Make a GET HTTP request to the Spotify server
    pub(super) async fn http_get<T>(&self, url: &str, payload: &Query<'_>) -> Result<T>
    where
        T: serde::de::DeserializeOwned,
    {
        /// a helper function to process an API response from Spotify server
        ///
        /// This function is mainly used to patch upstream API bugs , resulting in
        /// a type error when a third-party library like `rspotify` parses the response
        fn process_spotify_api_response(text: &str) -> String {
            text.to_string()
        }

        let access_token = self.token().await.context("get token")?;

        for attempt in 0..=SPOTIFY_API_MAX_RETRIES {
            self.wait_for_spotify_rate_limit().await;
            tracing::debug!(method = "GET", attempt, "Sending Spotify API request");

            let response = self
                .http
                .get(url)
                .query(payload)
                .header(
                    reqwest::header::AUTHORIZATION,
                    format!("Bearer {access_token}"),
                )
                .send()
                .await?;

            let status = response.status();
            let headers = response.headers().clone();
            let text = process_spotify_api_response(&response.text().await?);
            tracing::debug!(
                status = status.as_u16(),
                response_bytes = text.len(),
                "Received Spotify API response"
            );

            if self.handle_spotify_throttle("GET", status, &headers, &text, attempt)? {
                continue;
            }

            if status != StatusCode::OK {
                return Err(SpotifyApiStatusError {
                    method: "GET",
                    status,
                }
                .into());
            }

            return Ok(serde_json::from_str(&text)?);
        }

        unreachable!("retry loop always returns or bails")
    }

    pub(super) async fn all_paging_items<T>(&self, base_url: &str, count: usize) -> Result<Vec<T>>
    where
        T: serde::de::DeserializeOwned + std::fmt::Debug,
    {
        const PAGE_LIMIT: usize = 50;
        self.all_paging_items_with_limit(base_url, count, PAGE_LIMIT)
            .await
    }

    pub(super) async fn all_paging_items_with_limit<T>(
        &self,
        base_url: &str,
        mut count: usize,
        page_limit: usize,
    ) -> Result<Vec<T>>
    where
        T: serde::de::DeserializeOwned + std::fmt::Debug,
    {
        const MAX_PARALLEL: usize = 1;
        assert!(page_limit > 0, "Spotify page limit must be positive");

        let mut all_items = Vec::new();
        let mut offset = 0;
        let count_was_unknown = count == 0;

        // if count is 0 (i.e., unknown), set it to usize::MAX to fetch until no more items
        if count_was_unknown {
            count = usize::MAX;
        }

        while offset < count {
            let n_jobs = std::cmp::min(MAX_PARALLEL, (count - offset).div_ceil(page_limit));

            let mut futures = Vec::with_capacity(n_jobs);

            for i in 0..n_jobs {
                let current_offset = offset + i * page_limit;
                let (limit_str, offset_str) = paging_query_values(page_limit, current_offset);

                futures.push(async move {
                    let params = Query::from([
                        ("market", "from_token"),
                        ("limit", &limit_str),
                        ("offset", &offset_str),
                    ]);
                    self.http_get::<rspotify::model::Page<T>>(base_url, &params)
                        .await
                });
            }

            let results = futures::future::try_join_all(futures).await?;

            let mut found_empty = false;
            for mut page in results {
                if count_was_unknown {
                    count = page.total as usize;
                }
                if page.items.is_empty() {
                    found_empty = true;
                    break;
                }
                all_items.append(&mut page.items);
            }

            if found_empty {
                break;
            }

            offset += n_jobs * page_limit;
            if offset < count {
                tokio::time::sleep(PAGING_REQUEST_DELAY).await;
            }
        }

        Ok(all_items)
    }

    /// Get all cursor-based paging items starting from a pagination object of the first page
    pub(super) async fn all_cursor_based_paging_items<T>(
        &self,
        first_page: rspotify::model::CursorBasedPage<T>,
    ) -> Result<Vec<T>>
    where
        T: serde::de::DeserializeOwned,
    {
        let mut items = first_page.items;
        let mut maybe_next = first_page.next;
        while let Some(url) = maybe_next {
            let mut next_page = self
                .http_get::<rspotify::model::CursorBasedPage<T>>(&url, &Query::new())
                .await?;
            items.append(&mut next_page.items);
            maybe_next = next_page.next;
        }
        Ok(items)
    }
}

fn artist_from_integrated_metadata(artist: librespot_metadata::artist::Artist) -> Option<Artist> {
    let uri = artist.id.to_uri().ok()?;
    Some(Artist {
        id: ArtistId::from_uri(&uri).ok()?.into_static(),
        name: artist.name,
    })
}

fn album_from_integrated_metadata(album: librespot_metadata::Album) -> Option<Album> {
    let uri = album.id.to_uri().ok()?;
    let date = album.date;
    let release_date = format!(
        "{:04}-{:02}-{:02}",
        date.year(),
        u8::from(date.month()),
        date.day()
    );
    let typ = match album.type_str.to_ascii_lowercase().as_str() {
        "album" => Some(rspotify::model::AlbumType::Album),
        "single" | "ep" => Some(rspotify::model::AlbumType::Single),
        "compilation" => Some(rspotify::model::AlbumType::Compilation),
        _ => None,
    };

    Some(Album {
        id: AlbumId::from_uri(&uri).ok()?.into_static(),
        release_date,
        name: album.name,
        artists: album
            .artists
            .0
            .into_iter()
            .filter_map(artist_from_integrated_metadata)
            .collect(),
        typ,
        added_at: 0,
    })
}

fn track_from_integrated_metadata(
    track: librespot_metadata::Track,
    added_at_ms: i64,
) -> Option<Track> {
    let uri = track.id.to_uri().ok()?;
    Some(Track {
        id: TrackId::from_uri(&uri).ok()?.into_static(),
        name: track.name,
        artists: track
            .artists
            .0
            .into_iter()
            .filter_map(artist_from_integrated_metadata)
            .collect(),
        album: album_from_integrated_metadata(track.album),
        duration: Duration::from_millis(u64::try_from(track.duration).unwrap_or_default()),
        explicit: track.is_explicit,
        added_at: u64::try_from(added_at_ms).unwrap_or_default() / 1_000,
    })
}

fn paging_query_values(page_limit: usize, offset: usize) -> (String, String) {
    (page_limit.to_string(), offset.to_string())
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::{
        artist_albums_error_category, artist_albums_or_empty, context_server_retry_delay,
        is_retryable_spotify_context_status, is_spotify_no_active_device, paging_query_values,
        should_fetch_listenbrainz_artist_fallback, spotify_error_status, spotify_throttle_action,
        SpotifyApiStatusError, SpotifyPlaylistMetadata, SpotifyPlaylistReadPath,
        SpotifyThrottleAction,
    };
    use crate::observability::ErrorCategory;
    use crate::state::{spotify_playlist_is_modifiable, Album, AlbumId, PlaylistId, UserId};

    #[test]
    fn the_status_of_a_failed_spotify_call_is_recovered_for_classification() {
        let error: anyhow::Error = SpotifyApiStatusError {
            method: "GET",
            status: reqwest::StatusCode::TOO_MANY_REQUESTS,
        }
        .into();
        assert_eq!(
            spotify_error_status(&error.context("load top tracks")),
            Some(reqwest::StatusCode::TOO_MANY_REQUESTS)
        );
        assert_eq!(spotify_error_status(&anyhow::anyhow!("offline")), None);
    }
    use reqwest::StatusCode;

    #[test]
    fn playlist_metadata_without_contents_selects_integrated_read() {
        let metadata: SpotifyPlaylistMetadata = serde_json::from_str(
            r#"{
                "collaborative": true,
                "description": "A &amp; B <b>mix</b>",
                "name": "Foreign mix",
                "owner": {"display_name": "Other user", "id": "other-user"},
                "snapshot_id": "snapshot"
            }"#,
        )
        .expect("metadata without item fields must remain readable");

        assert_eq!(metadata.read_path(), SpotifyPlaylistReadPath::Integrated);
        let playlist = metadata
            .into_playlist(
                PlaylistId::from_id("37i9dQZF1DXcBWIGoYBM5M").unwrap(),
                SpotifyPlaylistReadPath::Integrated,
            )
            .unwrap();
        assert_eq!(playlist.name, "Foreign mix");
        assert_eq!(playlist.owner.0, "Other user");
        assert_eq!(playlist.desc, "A & B mix");
        assert!(!playlist.collaborative);
        let current_user = UserId::from_id("current-user").unwrap();
        assert!(!spotify_playlist_is_modifiable(
            &playlist,
            Some(&current_user)
        ));
    }

    #[test]
    fn playlist_metadata_contents_keep_web_api_read_path() {
        for (field, expected_total) in [("items", 0), ("tracks", 42)] {
            let value = format!(
                r#"{{
                    "collaborative": false,
                    "description": null,
                    "name": "Owned mix",
                    "owner": {{"display_name": null, "id": "owner"}},
                    "snapshot_id": "snapshot",
                    "{field}": {{"total": {expected_total}}}
                }}"#
            );
            let metadata: SpotifyPlaylistMetadata = serde_json::from_str(&value).unwrap();
            assert_eq!(
                metadata.read_path(),
                SpotifyPlaylistReadPath::WebApi {
                    total: expected_total
                }
            );
        }
    }

    #[test]
    fn only_not_found_gets_are_treated_as_missing_active_playback() {
        let not_found = anyhow::Error::new(SpotifyApiStatusError {
            method: "GET",
            status: StatusCode::NOT_FOUND,
        });
        assert!(is_spotify_no_active_device(&not_found));

        let forbidden = anyhow::Error::new(SpotifyApiStatusError {
            method: "GET",
            status: StatusCode::FORBIDDEN,
        });
        assert!(!is_spotify_no_active_device(&forbidden));
    }

    #[test]
    fn context_retry_policy_only_covers_rate_limits_and_server_errors() {
        for status in [
            StatusCode::TOO_MANY_REQUESTS,
            StatusCode::INTERNAL_SERVER_ERROR,
            StatusCode::BAD_GATEWAY,
            StatusCode::SERVICE_UNAVAILABLE,
        ] {
            assert!(is_retryable_spotify_context_status(status));
        }

        for status in [
            StatusCode::BAD_REQUEST,
            StatusCode::UNAUTHORIZED,
            StatusCode::FORBIDDEN,
            StatusCode::NOT_FOUND,
        ] {
            assert!(!is_retryable_spotify_context_status(status));
        }
    }

    #[test]
    fn development_quota_exhaustion_fails_without_using_the_global_gate() {
        let response = r#"{
            "error": {
                "status": 429,
                "message": "Too many requests",
                "reason": "QUOTA_EXCEEDED"
            }
        }"#;

        assert_eq!(
            spotify_throttle_action(StatusCode::TOO_MANY_REQUESTS, response),
            SpotifyThrottleAction::FailQuotaExceeded
        );
    }

    #[test]
    fn ordinary_or_unclassified_429_keeps_retry_after_behavior() {
        for response in [
            "",
            "not-json",
            r#"{"error":{"status":429}}"#,
            r#"{"error":{"reason":"ENDPOINT_RATE_LIMIT"}}"#,
            r#"{"reason":"QUOTA_EXCEEDED"}"#,
        ] {
            assert_eq!(
                spotify_throttle_action(StatusCode::TOO_MANY_REQUESTS, response),
                SpotifyThrottleAction::DeferAndRetry
            );
        }
        assert_eq!(
            spotify_throttle_action(
                StatusCode::SERVICE_UNAVAILABLE,
                r#"{"error":{"reason":"QUOTA_EXCEEDED"}}"#,
            ),
            SpotifyThrottleAction::NotThrottled
        );
    }

    #[test]
    fn quota_classification_does_not_retain_response_secrets() {
        let secret = "private-access-token";
        let response = format!(
            r#"{{"error":{{"reason":"QUOTA_EXCEEDED","message":"{secret}"}},"token":"{secret}"}}"#
        );

        let action = spotify_throttle_action(StatusCode::TOO_MANY_REQUESTS, &response);
        let rendered = format!("{action:?}");

        assert_eq!(action, SpotifyThrottleAction::FailQuotaExceeded);
        assert!(!rendered.contains(secret));
        assert!(!rendered.contains("message"));
        assert!(!rendered.contains("token"));
    }

    #[test]
    fn context_server_retry_delay_is_bounded_and_increases_per_attempt() {
        assert_eq!(context_server_retry_delay(0), Duration::from_millis(300));
        assert_eq!(context_server_retry_delay(1), Duration::from_millis(600));
        assert_eq!(context_server_retry_delay(2), Duration::from_millis(900));
    }

    #[test]
    fn paging_query_uses_the_endpoint_specific_limit() {
        let (limit, offset) = paging_query_values(10, 20);

        assert_eq!(limit, "10");
        assert_eq!(offset, "20");
    }

    #[test]
    fn listenbrainz_artist_fallback_requires_missing_spotify_data_and_opt_in() {
        assert!(!should_fetch_listenbrainz_artist_fallback(
            false, false, true
        ));
        assert!(!should_fetch_listenbrainz_artist_fallback(
            true, true, false
        ));
        assert!(should_fetch_listenbrainz_artist_fallback(true, false, true));
        assert!(should_fetch_listenbrainz_artist_fallback(false, true, true));
    }

    #[test]
    fn supplementary_artist_album_failure_returns_an_empty_section() {
        let albums = artist_albums_or_empty(Err(anyhow::anyhow!("private provider failure")));

        assert!(albums.is_empty());
    }

    #[test]
    fn supplementary_artist_album_success_preserves_the_result() {
        let album = Album {
            id: AlbumId::from_id("album0000000000000000000000000001")
                .expect("valid album id")
                .into_static(),
            release_date: "2026-09-03".to_owned(),
            name: "Preserved album".to_owned(),
            artists: Vec::new(),
            typ: None,
            added_at: 0,
        };

        let albums = artist_albums_or_empty(Ok(vec![album]));

        assert_eq!(albums.len(), 1);
        assert_eq!(albums[0].name, "Preserved album");
    }

    #[test]
    fn supplementary_artist_album_429_uses_rate_limited_classification() {
        let error = anyhow::Error::new(SpotifyApiStatusError {
            method: "GET",
            status: StatusCode::TOO_MANY_REQUESTS,
        });

        assert_eq!(
            artist_albums_error_category(&error),
            ErrorCategory::RateLimited
        );
        assert!(artist_albums_or_empty(Err(error)).is_empty());
    }
}

impl AppClient {
    /// Retrieve an image from a `url` or a cached `path`.
    /// If `saved` is specified, the retrieved image is saved to the cached `path`.
    pub(super) async fn retrieve_image(
        &self,
        url: &str,
        path: &std::path::Path,
        saved: bool,
    ) -> Result<Vec<u8>> {
        if path.exists() {
            tracing::debug!("Retrieving image from the local cache");
            return Ok(std::fs::read(path)?);
        }

        tracing::info!("Retrieving image from its remote provider");

        let bytes = self
            .http
            .get(url)
            .send()
            .await
            .with_context(|| format!("get image from url {url}"))?
            .bytes()
            .await?;

        if saved {
            tracing::info!("Saving the retrieved image to the local cache");
            let mut file = std::fs::File::create(path)?;
            file.write_all(&bytes)?;
        }

        Ok(bytes.to_vec())
    }

    pub(super) async fn load_youtube_thumbnail(
        &self,
        #[cfg_attr(not(feature = "image"), allow(unused_variables))] state: &SharedState,
        track: &crate::state::YouTubeTrack,
    ) -> Result<()> {
        let Some(url) = track.thumbnail_url.as_deref() else {
            return Ok(());
        };

        let configs = config::get_config();
        let path = configs
            .cache_folder
            .join("image")
            .join(format!("youtube-{}-cover.jpg", track.id));

        if configs.app_config.enable_cover_image_cache {
            self.retrieve_image(url, &path, true).await?;
        }

        #[cfg(feature = "image")]
        if !state.data.read().caches.images.contains_key(url) {
            let bytes = self.retrieve_image(url, &path, false).await?;

            #[cfg(not(feature = "pixelate"))]
            let image =
                image::load_from_memory(&bytes).context("Failed to load image from memory")?;
            #[cfg(feature = "pixelate")]
            let mut image =
                image::load_from_memory(&bytes).context("Failed to load image from memory")?;

            #[cfg(feature = "pixelate")]
            {
                Self::pixelate_image(&mut image);
            }

            state
                .data
                .write()
                .caches
                .images
                .insert(url.to_owned(), image, *TTL_CACHE_DURATION);
        }

        Ok(())
    }

    #[cfg(feature = "pixelate")]
    fn pixelate_image(image: &mut image::DynamicImage) {
        let pixels = config::get_config().app_config.cover_img_pixels;
        let pixelated_image = image.resize(pixels, pixels, image::imageops::FilterType::Nearest);
        *image = pixelated_image.resize(
            image.width(),
            image.height(),
            image::imageops::FilterType::Nearest,
        );
    }

    /// Process a list of albums, which includes
    /// - sort albums by the release date
    /// - sort albums by the type if `sort_artist_albums_by_type` config is enabled
    pub(super) fn process_artist_albums(mut albums: Vec<Album>) -> Vec<Album> {
        albums.sort_by(|x, y| y.release_date.partial_cmp(&x.release_date).unwrap());

        if config::get_config().app_config.sort_artist_albums_by_type {
            fn get_priority(album_type: &str) -> usize {
                match album_type {
                    "album" => 0,
                    "single" => 1,
                    "appears_on" => 2,
                    "compilation" => 3,
                    _ => 4,
                }
            }
            albums.sort_by_key(|a| get_priority(&a.album_type()));
        }

        albums
    }
}
