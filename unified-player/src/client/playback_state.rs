use std::time::{Duration, Instant};

use anyhow::{Context as _, Result};
use rspotify::{http::Query, prelude::Id};
use serde_json::Value;

use crate::{
    config,
    state::{now_unix_secs, PlaybackMetadata, SessionEntry, SharedState, TTL_CACHE_DURATION},
};

use super::{playback_coordinator, AppClient};

const VOLUME_REQUEST_DEDUP_WINDOW: Duration = Duration::from_millis(750);

fn parse_current_playback_response(
    text: &str,
) -> serde_json::Result<rspotify::model::CurrentPlaybackContext> {
    let mut value = serde_json::from_str::<Value>(text)?;
    normalize_current_playback_track(&mut value);
    serde_json::from_value(value)
}

fn normalize_current_playback_track(value: &mut Value) {
    if let Some(item) = value.get_mut("item") {
        normalize_track(item);
    }
}

/// Spotify no longer sends `external_ids`, which rspotify still requires, so
/// without a default every track would deserialize as `PlayableItem::Unknown`.
fn normalize_track(item: &mut Value) {
    let Some(item) = item.as_object_mut() else {
        return;
    };
    if item.get("type").and_then(Value::as_str) == Some("track") {
        item.entry("external_ids")
            .or_insert_with(|| Value::Object(serde_json::Map::new()));
    }
}

fn parse_current_user_queue_response(
    text: &str,
) -> serde_json::Result<rspotify::model::CurrentUserQueue> {
    let mut value = serde_json::from_str::<Value>(text)?;
    if let Some(item) = value.get_mut("currently_playing") {
        normalize_track(item);
    }
    if let Some(queue) = value.get_mut("queue").and_then(Value::as_array_mut) {
        queue.iter_mut().for_each(normalize_track);
    }
    serde_json::from_value(value)
}

#[derive(Default)]
pub(super) struct VolumeRequestState {
    last: Option<VolumeRequestRecord>,
}

struct VolumeRequestRecord {
    device_id: Option<String>,
    volume: u8,
    sent_at: Instant,
}

impl AppClient {
    pub async fn current_playback2(
        &self,
    ) -> Result<Option<rspotify::model::CurrentPlaybackContext>> {
        let text = self
            .spotify_api_get_text(
                "me/player",
                &Query::from([("additional_types", "track,episode")]),
            )
            .await?;

        if text.is_empty() {
            Ok(None)
        } else {
            parse_current_playback_response(&text)
                .map(Some)
                .context("parse current playback response")
        }
    }

    /// Spotify's native queue, with the same track normalization as
    /// [`Self::current_playback2`].
    pub(super) async fn current_user_queue(&self) -> Result<rspotify::model::CurrentUserQueue> {
        let text = self
            .spotify_api_get_text("me/player/queue", &Query::new())
            .await?;
        parse_current_user_queue_response(&text).context("parse Spotify queue response")
    }

    pub(super) async fn volume_with_diagnostics(
        &self,
        volume: u8,
        device_id: Option<&str>,
    ) -> Result<()> {
        anyhow::ensure!(volume <= 100, "volume should be between 0 and 100");

        {
            let mut state = self.volume_requests.lock().expect("volume mutex poisoned");
            let duplicate = state.last.as_ref().is_some_and(|last| {
                last.volume == volume
                    && last.device_id.as_deref() == device_id
                    && last.sent_at.elapsed() < VOLUME_REQUEST_DEDUP_WINDOW
            });

            if duplicate {
                tracing::debug!(
                    volume,
                    has_device = device_id.is_some(),
                    "Skipping duplicate Spotify volume request"
                );
                return Ok(());
            }

            state.last = Some(VolumeRequestRecord {
                device_id: device_id.map(str::to_owned),
                volume,
                sent_at: Instant::now(),
            });
        }

        let path = format!("me/player/volume?volume_percent={volume}");
        let path = if let Some(device_id) = device_id {
            Self::append_query_param(&path, "device_id", device_id)
        } else {
            path
        };

        self.spotify_api_put_empty(&path).await
    }

    /// Retrieve the latest playback state
    pub async fn retrieve_current_playback(
        &self,
        state: &SharedState,
        reset_buffered_playback: bool,
    ) -> Result<()> {
        if self.playback.stable_active_provider() == Some(config::ActiveProvider::YouTubeMusic) {
            let sessions = playback_coordinator::AppPlaybackSessions::new(state);
            self.playback
                .refresh_session(&sessions, config::ActiveProvider::Spotify);
            return Ok(());
        }
        let new_playback = {
            // update the playback state
            let playback = self.current_playback2().await?;
            let mut player = state.player.write();

            let prev_item = player.currently_playing();

            let prev_name = match prev_item {
                Some(rspotify::model::PlayableItem::Track(track)) => track.name.clone(),
                Some(rspotify::model::PlayableItem::Episode(episode)) => episode.name.clone(),
                Some(rspotify::model::PlayableItem::Unknown(_)) | None => String::new(),
            };

            player.playback = playback;
            player.playback_last_updated_time = Some(std::time::Instant::now());
            let account_label = player.playback.as_ref().and_then(|_| {
                let configs = config::get_config();
                config::AccountRegistry::load(&configs.config_folder)
                    .ok()
                    .and_then(|registry| {
                        registry
                            .active_label(config::ActiveProvider::Spotify)
                            .map(str::to_owned)
                    })
            });
            player.active_playback_account_provider = player
                .playback
                .as_ref()
                .map(|_| config::ActiveProvider::Spotify);
            player.active_playback_account_label = account_label;

            let curr_item = player.currently_playing();

            let curr_name = match curr_item {
                Some(rspotify::model::PlayableItem::Track(track)) => track.name.clone(),
                Some(rspotify::model::PlayableItem::Episode(episode)) => episode.name.clone(),
                Some(rspotify::model::PlayableItem::Unknown(_)) | None => String::new(),
            };

            let new_playback = prev_name != curr_name && !curr_name.is_empty();
            // check if we need to update the buffered playback
            let needs_update = match (&player.buffered_playback, &player.playback) {
                (Some(bp), Some(p)) => {
                    let remote_volume = p.device.volume_percent;
                    bp.device_id != p.device.id || bp.volume != remote_volume || new_playback
                }
                (None, None) => false,
                _ => true,
            };

            if reset_buffered_playback || needs_update {
                player.buffered_playback = player.playback.as_ref().map(|p| {
                    let mut playback = PlaybackMetadata::from_playback(p);

                    // handle additional data from the previous buffered state
                    // that is not available in a standard Spotify playback's state
                    if let Some(bp) = &player.buffered_playback {
                        let remote_volume = p.device.volume_percent.map(|volume| volume.min(100));
                        let preserve_local_mute = bp.mute_state.is_some_and(|_| {
                            remote_volume.is_none()
                                || remote_volume == Some(0)
                                || remote_volume == playback.volume
                        });
                        if let Some(volume) = bp.mute_state.filter(|_| preserve_local_mute) {
                            playback.volume = Some(volume);
                            playback.mute_state = Some(volume);
                        } else {
                            playback.mute_state = None;
                        }
                    }
                    playback
                });
            }
            new_playback
        };
        self.refresh_and_persist_session(state, config::ActiveProvider::Spotify);

        if !new_playback {
            return Ok(());
        }

        let session_entry = {
            let player = state.player.read();
            player
                .currently_playing()
                .and_then(|item| SessionEntry::from_spotify_item(item, now_unix_secs()))
        };
        if let Some(entry) = session_entry {
            let configs = config::get_config();
            if let Err(error) = state
                .data
                .write()
                .record_session_entry(entry, &configs.app_config.session_history)
            {
                crate::observability::log_safe_error!(
                    warn,
                    crate::observability::DiagnosticCode::SESSION_HISTORY_SAVE_FAILED,
                    crate::observability::ErrorCategory::Storage,
                    &error,
                    "Local Spotify session history could not be saved"
                );
            }
        }
        self.handle_new_playback_event(state).await?;

        Ok(())
    }

    // Handle new track event
    async fn handle_new_playback_event(&self, state: &SharedState) -> Result<()> {
        let configs = config::get_config();

        let curr_item = {
            let player = state.player.read();
            let Some(track_or_episode) = player.currently_playing() else {
                return Ok(());
            };
            track_or_episode.clone()
        };

        // retrieve current artist for genres if not in cache
        let curr_artist = match &curr_item {
            rspotify::model::PlayableItem::Track(full_track) => {
                let Some(first_artist) = full_track.artists.first() else {
                    return Ok(());
                };
                let cached = state
                    .data
                    .read()
                    .caches
                    .genres
                    .contains_key(&first_artist.name);

                if cached {
                    None
                } else {
                    match &first_artist.id {
                        Some(id) => self.artist_genres(id.clone()).await.ok(),
                        None => None,
                    }
                }
            }
            rspotify::model::PlayableItem::Episode(_)
            | rspotify::model::PlayableItem::Unknown(_) => None,
        };

        if let Some((artist_name, genres)) = curr_artist {
            state
                .data
                .write()
                .caches
                .genres
                .insert(artist_name, genres, *TTL_CACHE_DURATION);
        }

        let url = match curr_item {
            rspotify::model::PlayableItem::Track(ref track) => {
                crate::utils::get_track_album_image_url(track)
            }
            rspotify::model::PlayableItem::Episode(ref episode) => {
                crate::utils::get_episode_show_image_url(episode)
            }
            rspotify::model::PlayableItem::Unknown(_) => return Ok(()),
        };
        let Some(url) = url else {
            tracing::debug!("Current Spotify item has no cover image; skipping cover retrieval");
            return Ok(());
        };

        let filename = (match curr_item {
            rspotify::model::PlayableItem::Track(ref track) => {
                let artist = track
                    .album
                    .artists
                    .first()
                    .map_or("unknown", |artist| artist.name.as_str());
                let album_id = track.album.id.as_ref().map_or_else(
                    || "local".to_string(),
                    |id| id.id().chars().take(6).collect::<String>(),
                );
                format!("{}-{}-cover-{}.jpg", track.album.name, artist, album_id)
            }
            rspotify::model::PlayableItem::Episode(ref episode) => {
                format!(
                    "{}-{}-cover-{}.jpg",
                    episode.show.name,
                    episode.show.id.as_ref().id(),
                    // first 6 characters of the show's id
                    &episode.show.id.as_ref().id()[..6]
                )
            }
            rspotify::model::PlayableItem::Unknown(_) => return Ok(()),
        })
        .replace('/', ""); // remove invalid characters from the file's name
        let path = configs.cache_folder.join("image").join(filename);

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

        // notify user about the playback's change if any
        #[cfg(all(feature = "notify", feature = "streaming"))]
        if configs.app_config.enable_notify
            && (!configs.app_config.notify_streaming_only || self.stream_conn.lock().is_some())
        {
            Self::notify_new_playback(&curr_item, &path)?;
        }

        #[cfg(all(feature = "notify", not(feature = "streaming")))]
        if configs.app_config.enable_notify {
            Self::notify_new_playback(&curr_item, &path)?;
        }

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use rspotify::model::PlayableItem;
    use serde_json::{json, Value};

    use super::{parse_current_playback_response, parse_current_user_queue_response};

    fn current_playback_value() -> Value {
        json!({
            "device": {
                "id": "device-id",
                "is_active": true,
                "is_private_session": false,
                "is_restricted": false,
                "name": "unified-player",
                "supports_volume": true,
                "type": "Speaker",
                "volume_percent": 70
            },
            "shuffle_state": false,
            "smart_shuffle": false,
            "repeat_state": "off",
            "is_playing": true,
            "timestamp": 1_788_360_291_502_i64,
            "context": null,
            "progress_ms": 1492,
            "item": {
                "album": {
                    "album_type": "album",
                    "artists": [{
                        "external_urls": {"spotify": "https://open.spotify.com/artist/0hCNtLu0JehylgoiP8L4Gh"},
                        "href": "https://api.spotify.com/v1/artists/0hCNtLu0JehylgoiP8L4Gh",
                        "id": "0hCNtLu0JehylgoiP8L4Gh",
                        "name": "Artist",
                        "type": "artist",
                        "uri": "spotify:artist:0hCNtLu0JehylgoiP8L4Gh"
                    }],
                    "external_urls": {"spotify": "https://open.spotify.com/album/40XGTQ7FN6Y3dZXJhKBe96"},
                    "href": "https://api.spotify.com/v1/albums/40XGTQ7FN6Y3dZXJhKBe96",
                    "id": "40XGTQ7FN6Y3dZXJhKBe96",
                    "images": [],
                    "name": "Album",
                    "release_date": "2026-09-02",
                    "release_date_precision": "day",
                    "total_tracks": 1,
                    "type": "album",
                    "uri": "spotify:album:40XGTQ7FN6Y3dZXJhKBe96"
                },
                "artists": [{
                    "external_urls": {"spotify": "https://open.spotify.com/artist/0hCNtLu0JehylgoiP8L4Gh"},
                    "href": "https://api.spotify.com/v1/artists/0hCNtLu0JehylgoiP8L4Gh",
                    "id": "0hCNtLu0JehylgoiP8L4Gh",
                    "name": "Artist",
                    "type": "artist",
                    "uri": "spotify:artist:0hCNtLu0JehylgoiP8L4Gh"
                }],
                "disc_number": 1,
                "duration_ms": 229626,
                "explicit": true,
                "external_urls": {"spotify": "https://open.spotify.com/track/1gpVAJuEUwSpQpwfD8v852"},
                "href": "https://api.spotify.com/v1/tracks/1gpVAJuEUwSpQpwfD8v852",
                "id": "1gpVAJuEUwSpQpwfD8v852",
                "is_local": false,
                "name": "Track",
                "preview_url": null,
                "track_number": 1,
                "type": "track",
                "uri": "spotify:track:1gpVAJuEUwSpQpwfD8v852"
            },
            "currently_playing_type": "track",
            "actions": {"disallows": {}}
        })
    }

    #[test]
    fn current_playback_defaults_missing_track_external_ids() {
        let playback =
            parse_current_playback_response(&current_playback_value().to_string()).unwrap();

        let Some(PlayableItem::Track(track)) = playback.item else {
            panic!("expected a parsed Spotify track");
        };
        assert_eq!(track.name, "Track");
        assert!(track.external_ids.is_empty());
    }

    #[test]
    fn current_playback_preserves_present_track_external_ids() {
        let mut value = current_playback_value();
        value["item"]["external_ids"] = json!({"isrc": "TEST12345678"});

        let playback = parse_current_playback_response(&value.to_string()).unwrap();

        let Some(PlayableItem::Track(track)) = playback.item else {
            panic!("expected a parsed Spotify track");
        };
        assert_eq!(
            track.external_ids.get("isrc").map(String::as_str),
            Some("TEST12345678")
        );
    }

    #[test]
    fn queue_tracks_without_external_ids_are_parsed_as_tracks() {
        let track = current_playback_value()["item"].clone();
        let mut episode_like = track.clone();
        episode_like["type"] = json!("ad");
        let response = json!({
            "currently_playing": track,
            "queue": [track, episode_like],
        });

        let queue = parse_current_user_queue_response(&response.to_string()).unwrap();

        assert!(matches!(
            queue.currently_playing,
            Some(PlayableItem::Track(_))
        ));
        assert!(matches!(queue.queue[0], PlayableItem::Track(ref track) if track.name == "Track"));
        assert!(matches!(queue.queue[1], PlayableItem::Unknown(_)));
    }

    #[test]
    fn current_playback_does_not_project_unknown_items_as_tracks() {
        let mut value = current_playback_value();
        value["item"]["type"] = json!("ad");

        let playback = parse_current_playback_response(&value.to_string()).unwrap();

        let Some(PlayableItem::Unknown(item)) = playback.item else {
            panic!("expected an unknown Spotify item");
        };
        assert!(item.get("external_ids").is_none());
    }
}
