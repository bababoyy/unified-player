use super::provider_metadata::SPOTIFY_API_ENDPOINT;

use anyhow::Result;
use rspotify::{http::Query, prelude::*};
use serde::Deserialize;

use crate::{
    config,
    state::{
        Album, Episode, EpisodeId, Playlist, PlaylistId, SearchResults, Show, ShowId, Track,
        TrackId, UserId,
    },
};

use super::{youtube, AppClient};

impl AppClient {
    fn schedule_youtube_playback_warmup(&self) {
        let resolver = self.youtube_audio_resolver.clone();
        tokio::spawn(async move {
            if resolver
                .warm_up(&tokio_util::sync::CancellationToken::new())
                .await
                .is_err()
            {
                tracing::debug!("YouTube playback warm-up did not complete");
            }
        });
    }

    /// Get recommendation (radio) tracks based on a seed
    pub async fn radio_tracks(&self, seed_uri: String) -> Result<Vec<Track>> {
        #[derive(Debug, Deserialize)]
        struct TrackData {
            original_gid: String,
        }
        #[derive(Debug, Deserialize)]
        struct RadioStationResponse {
            tracks: Vec<TrackData>,
        }

        let session = self.spotify.session().await;

        // Get an autoplay URI from the seed URI.
        // The return URI is a Spotify station's URI
        let autoplay_query_url = format!("hm://autoplay-enabled/query?uri={seed_uri}");
        let response = session
            .mercury()
            .get(autoplay_query_url)
            .map_err(|err| anyhow::anyhow!("Failed to get autoplay URI: {err:#}"))?
            .await?;
        if response.status_code != 200 {
            anyhow::bail!(
                "Failed to get autoplay URI: got non-OK status code: {}",
                response.status_code
            );
        }
        let autoplay_uri = String::from_utf8(response.payload[0].clone())?;

        // Retrieve radio's data based on the autoplay URI
        let radio_query_url = format!("hm://radio-apollo/v3/stations/{autoplay_uri}");
        let response = session
            .mercury()
            .get(radio_query_url)
            .map_err(|err| anyhow::anyhow!("Failed to get radio data of {autoplay_uri}: {err:#}"))?
            .await?;
        if response.status_code != 200 {
            anyhow::bail!(
                "Failed to get radio data of {autoplay_uri}: got non-OK status code: {}",
                response.status_code
            );
        }

        // Parse a list consisting of IDs of tracks inside the radio station
        let track_ids = serde_json::from_slice::<RadioStationResponse>(&response.payload[0])?
            .tracks
            .into_iter()
            .filter_map(|t| TrackId::from_id(t.original_gid).ok());

        // Retrieve tracks based on IDs
        let mut tracks = Vec::new();
        for track_id in track_ids {
            match self.track(track_id).await {
                Ok(track) => tracks.push(track),
                Err(err) => crate::observability::log_safe_error!(
                    warn,
                    crate::observability::DiagnosticCode::SPOTIFY_ARTIST_TRACKS_FAILED,
                    crate::observability::ErrorCategory::Unavailable,
                    &err,
                    "Failed to fetch a Spotify radio track"
                ),
            }
        }

        // Track-seeded radios in the official Spotify clients include the seed track itself
        // as the first item in the generated session.
        if let Ok(track_id) = TrackId::from_uri(&seed_uri) {
            match self.track(track_id).await {
                Ok(track) => move_seed_track_to_front(&mut tracks, track),
                Err(err) => {
                    crate::observability::log_safe_error!(
                        warn,
                        crate::observability::DiagnosticCode::SPOTIFY_ARTIST_TRACKS_FAILED,
                        crate::observability::ErrorCategory::Unavailable,
                        &err,
                        "Failed to fetch the Spotify radio seed track"
                    );
                }
            }
        }

        Ok(tracks)
    }

    /// Search for items (tracks, artists, albums, playlists) matching a given query
    pub async fn search(&self, query: &str) -> Result<SearchResults> {
        #[derive(Debug, Deserialize)]
        struct SearchResponse {
            tracks: Option<rspotify::model::Page<rspotify::model::FullTrack>>,
            artists: Option<rspotify::model::Page<rspotify::model::FullArtist>>,
            albums: Option<rspotify::model::Page<rspotify::model::SimplifiedAlbum>>,
            playlists: Option<rspotify::model::Page<serde_json::Value>>,
            shows: Option<rspotify::model::Page<serde_json::Value>>,
            episodes: Option<rspotify::model::Page<serde_json::Value>>,
        }

        let response = self
            .http_get::<SearchResponse>(
                &format!("{SPOTIFY_API_ENDPOINT}/search"),
                &Query::from([
                    ("q", query),
                    ("type", "track,artist,album,playlist,show,episode"),
                    ("limit", "10"),
                ]),
            )
            .await?;

        let tracks = response
            .tracks
            .map(|p| {
                p.items
                    .into_iter()
                    .filter_map(Track::try_from_full_track)
                    .collect()
            })
            .unwrap_or_default();
        let artists = response
            .artists
            .map(|p| p.items.into_iter().map(std::convert::Into::into).collect())
            .unwrap_or_default();
        let albums = response
            .albums
            .map(|p| {
                p.items
                    .into_iter()
                    .filter_map(Album::try_from_simplified_album)
                    .collect()
            })
            .unwrap_or_default();
        let playlists = response
            .playlists
            .map(|p| Self::playlists_from_search_items(p.items))
            .unwrap_or_default();
        let shows = response
            .shows
            .map(|p| Self::shows_from_search_items(p.items))
            .unwrap_or_default();
        let episodes = response
            .episodes
            .map(|p| Self::episodes_from_search_items(p.items))
            .unwrap_or_default();

        Ok(SearchResults {
            tracks,
            artists,
            albums,
            playlists,
            shows,
            episodes,
        })
    }

    pub async fn youtube_search(&self, query: &str) -> Result<crate::state::YouTubeSearchResults> {
        let mut youtube = self.youtube.lock().await;
        if youtube.is_none() {
            *youtube = Some(youtube::YouTubeMusic::new(config::get_config()).await?);
        }

        let result = youtube
            .as_ref()
            .expect("YouTube Music client initialized")
            .search(query)
            .await;
        if result.is_ok() {
            self.schedule_youtube_playback_warmup();
        }
        result
    }

    pub async fn youtube_library(&self) -> Result<crate::state::YouTubeLibrary> {
        let mut youtube = self.youtube.lock().await;
        if youtube.is_none() {
            *youtube = Some(youtube::YouTubeMusic::new(config::get_config()).await?);
        }

        let result = youtube
            .as_ref()
            .expect("YouTube Music client initialized")
            .library()
            .await;
        if result.is_ok() {
            self.schedule_youtube_playback_warmup();
        }
        result
    }

    pub async fn youtube_context(
        &self,
        id: &crate::state::YouTubeContextId,
    ) -> Result<crate::state::YouTubeContext> {
        let mut youtube = self.youtube.lock().await;
        if youtube.is_none() {
            *youtube = Some(youtube::YouTubeMusic::new(config::get_config()).await?);
        }

        let result = youtube
            .as_ref()
            .expect("YouTube Music client initialized")
            .context(id)
            .await;
        if result.is_ok() {
            self.schedule_youtube_playback_warmup();
        }
        result
    }
}

/// Search `YouTube` Music for an explicit playlist-projection resolution step.
/// Matching and mutation policy remains with the caller.
pub async fn youtube_search_for_projection(
    query: &str,
) -> Result<crate::state::YouTubeSearchResults> {
    let youtube = youtube::YouTubeMusic::new(config::get_config()).await?;
    youtube.search(query).await
}

impl AppClient {
    fn playlists_from_search_items(items: Vec<serde_json::Value>) -> Vec<Playlist> {
        #[derive(Debug, Deserialize)]
        struct SearchPlaylistItem {
            collaborative: Option<bool>,
            id: Option<PlaylistId<'static>>,
            name: Option<String>,
            owner: Option<SearchPlaylistOwner>,
            snapshot_id: Option<String>,
        }

        #[derive(Debug, Deserialize)]
        struct SearchPlaylistOwner {
            display_name: Option<String>,
            id: UserId<'static>,
        }

        items
            .into_iter()
            .filter_map(|item| {
                let item = serde_json::from_value::<SearchPlaylistItem>(item).ok()?;
                let owner = item.owner?;
                Some(Playlist {
                    id: item.id?,
                    collaborative: item.collaborative.unwrap_or_default(),
                    name: item.name.unwrap_or_default(),
                    owner: (owner.display_name.unwrap_or_default(), owner.id),
                    desc: String::new(),
                    current_folder_id: 0,
                    snapshot_id: item.snapshot_id.unwrap_or_default(),
                })
            })
            .collect()
    }

    fn shows_from_search_items(items: Vec<serde_json::Value>) -> Vec<Show> {
        #[derive(Debug, Deserialize)]
        struct SearchShowItem {
            id: Option<ShowId<'static>>,
            name: Option<String>,
        }

        items
            .into_iter()
            .filter_map(|item| {
                let item = serde_json::from_value::<SearchShowItem>(item).ok()?;
                Some(Show {
                    id: item.id?,
                    name: item.name.unwrap_or_default(),
                })
            })
            .collect()
    }

    fn episodes_from_search_items(items: Vec<serde_json::Value>) -> Vec<Episode> {
        #[derive(Debug, Deserialize)]
        struct SearchEpisodeItem {
            id: Option<EpisodeId<'static>>,
            name: Option<String>,
            description: Option<String>,
            duration_ms: Option<u64>,
            release_date: Option<String>,
            show: Option<SearchShowItem>,
        }

        #[derive(Debug, Deserialize)]
        struct SearchShowItem {
            id: Option<ShowId<'static>>,
            name: Option<String>,
        }

        items
            .into_iter()
            .filter_map(|item| {
                let item = serde_json::from_value::<SearchEpisodeItem>(item).ok()?;
                Some(Episode {
                    id: item.id?,
                    name: item.name.unwrap_or_default(),
                    description: item.description.unwrap_or_default(),
                    duration: std::time::Duration::from_millis(
                        item.duration_ms.unwrap_or_default(),
                    ),
                    show: item.show.and_then(|show| {
                        Some(Show {
                            id: show.id?,
                            name: show.name.unwrap_or_default(),
                        })
                    }),
                    release_date: item.release_date.unwrap_or_default(),
                })
            })
            .collect()
    }

    /// Search for items of a specific type matching a given query
    pub async fn search_specific_type(
        &self,
        query: &str,
        typ: rspotify::model::SearchType,
    ) -> Result<rspotify::model::SearchResult> {
        Ok(self
            .spotify_api()
            .search(query, typ, None, None, None, None)
            .await?)
    }
}

fn move_seed_track_to_front(tracks: &mut Vec<Track>, seed_track: Track) {
    tracks.retain(|track| track.id != seed_track.id);
    tracks.insert(0, seed_track);
}

#[cfg(test)]
mod tests {
    use super::move_seed_track_to_front;
    use crate::state::Track;
    use rspotify::model::TrackId;

    fn sample_track(id: &'static str, name: &str) -> Track {
        Track {
            id: TrackId::from_id(id).unwrap().into_static(),
            name: name.to_string(),
            artists: vec![],
            album: None,
            duration: std::time::Duration::default(),
            explicit: false,
            added_at: 0,
        }
    }

    #[test]
    fn move_seed_track_to_front_prepends_missing_seed() {
        let seed = sample_track("3n3Ppam7vgaVa1iaRUc9Lp", "seed");
        let second = sample_track("4uLU6hMCjMI75M1A2tKUQC", "second");
        let third = sample_track("1301WleyT98MSxVHPZCA6M", "third");
        let mut tracks = vec![second.clone(), third];

        move_seed_track_to_front(&mut tracks, seed.clone());

        assert_eq!(tracks.len(), 3);
        assert_eq!(tracks[0].id, seed.id);
        assert_eq!(tracks[1].id, second.id);
    }

    #[test]
    fn move_seed_track_to_front_reorders_existing_seed_without_duplication() {
        let seed = sample_track("3n3Ppam7vgaVa1iaRUc9Lp", "seed");
        let second = sample_track("4uLU6hMCjMI75M1A2tKUQC", "second");
        let mut tracks = vec![second.clone(), seed.clone()];

        move_seed_track_to_front(&mut tracks, seed.clone());

        assert_eq!(tracks.len(), 2);
        assert_eq!(tracks[0].id, seed.id);
        assert_eq!(tracks[1].id, second.id);
    }
}
