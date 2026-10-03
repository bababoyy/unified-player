//! Shared queue row projection and hit geometry.

use std::collections::HashMap;

use crate::{
    state::{AppData, PlayerState, QueueDisplayItemRef, UnifiedPlaylistItem},
    ui::utils::to_bidi_string,
    utils::format_duration,
};
use ratatui::layout::Rect;
use rspotify::model::{Id as _, PlayableId, PlayableItem};

/// Display details for Spotify entries of the unified queue, which carry only
/// a Spotify ID. Built per render from the playing item, labels remembered
/// when items were added, the local unified playlists, and saved tracks.
pub(crate) struct SpotifyQueueLabels<'a> {
    playing: Option<&'a PlayableItem>,
    listed: HashMap<&'a str, &'a UnifiedPlaylistItem>,
    saved: &'a HashMap<String, crate::state::Track>,
    added: &'a crate::state::SpotifyQueueLabelCache,
}

impl<'a> SpotifyQueueLabels<'a> {
    pub(crate) fn new(
        player: &'a PlayerState,
        data: &'a AppData,
        added: &'a crate::state::SpotifyQueueLabelCache,
    ) -> Self {
        let listed = data
            .unified_playlists
            .iter()
            .flat_map(|playlist| &playlist.items)
            .filter(|item| {
                item.media_id.provider == crate::state::Provider::Spotify
                    && !item.title.trim().is_empty()
            })
            .map(|item| (item.media_id.raw_id.as_str(), item))
            .collect();
        Self {
            playing: player
                .playback
                .as_ref()
                .and_then(|playback| playback.item.as_ref()),
            listed,
            saved: &data.user_data.saved_tracks,
            added,
        }
    }

    /// Title, artists and duration for `id`, if any local source knows it.
    fn label(&self, id: &PlayableId<'_>) -> Option<(String, String, String)> {
        if let Some(playing) = self
            .playing
            .filter(|item| playable_item_id(item) == Some(id.id()))
        {
            return spotify_item_columns(playing);
        }
        if let Some(label) = self.added.get(id.id()) {
            return Some((
                to_bidi_string(&label.title),
                to_bidi_string(&label.artists),
                chrono::Duration::from_std(label.duration)
                    .map(|duration| format_duration(&duration))
                    .unwrap_or_default(),
            ));
        }
        if let Some(item) = self.listed.get(id.id()) {
            return Some((
                to_bidi_string(&item.title),
                to_bidi_string(&item.artists),
                item.duration_ms
                    .and_then(|ms| i64::try_from(ms).ok())
                    .map(|ms| format_duration(&chrono::Duration::milliseconds(ms)))
                    .unwrap_or_default(),
            ));
        }
        self.saved.get(&id.uri()).map(|track| {
            (
                to_bidi_string(&track.name),
                to_bidi_string(&track.artists_info()),
                chrono::Duration::from_std(track.duration)
                    .map(|duration| format_duration(&duration))
                    .unwrap_or_default(),
            )
        })
    }
}

fn playable_item_id(item: &PlayableItem) -> Option<&str> {
    match item {
        PlayableItem::Track(track) => track.id.as_ref().map(|id| id.id()),
        PlayableItem::Episode(episode) => Some(episode.id.id()),
        PlayableItem::Unknown(_) => None,
    }
}

fn spotify_item_columns(item: &PlayableItem) -> Option<(String, String, String)> {
    match item {
        PlayableItem::Track(track) => Some((
            to_bidi_string(&track.name),
            to_bidi_string(&crate::utils::map_join(
                &track.artists,
                |artist| &artist.name,
                ", ",
            )),
            format_duration(&track.duration),
        )),
        PlayableItem::Episode(episode) => Some((
            to_bidi_string(&episode.name),
            to_bidi_string(&episode.show.name),
            format_duration(&episode.duration),
        )),
        PlayableItem::Unknown(_) => None,
    }
}

/// Provider-neutral queue data used by both compact sidebar rows and the full
/// queue table. Layout-specific renderers may choose different columns, but
/// they must consume this same projection.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct QueueRowProjection {
    pub(crate) title: String,
    pub(crate) artists: String,
    pub(crate) duration: String,
    pub(crate) source: String,
    pub(crate) is_current: bool,
}

impl QueueRowProjection {
    pub(crate) fn from_item(
        item: &QueueDisplayItemRef<'_>,
        spotify_labels: &SpotifyQueueLabels<'_>,
    ) -> Self {
        let (title, artists, duration, source) = match item {
            QueueDisplayItemRef::Spotify { item, .. } => {
                let (title, artists, duration) = spotify_item_columns(item)
                    .unwrap_or_else(|| ("Unknown item".to_owned(), String::new(), String::new()));
                (title, artists, duration, "Spotify".to_owned())
            }
            QueueDisplayItemRef::Unified { item, .. } => match &item.media {
                crate::state::PlayableMedia::YouTube(track) => (
                    to_bidi_string(&track.name),
                    to_bidi_string(&track.artists),
                    track.duration.clone(),
                    item.origin.label().to_owned(),
                ),
                crate::state::PlayableMedia::Spotify(id) => {
                    let (title, artists, duration) =
                        spotify_labels.label(id).unwrap_or_else(|| {
                            let fallback = match id {
                                PlayableId::Track(_) => "Spotify track",
                                PlayableId::Episode(_) => "Spotify episode",
                            };
                            (fallback.to_owned(), String::new(), String::new())
                        });
                    (title, artists, duration, item.origin.label().to_owned())
                }
            },
        };
        Self {
            title,
            artists,
            duration,
            source,
            is_current: item.is_current(),
        }
    }
}

/// Compute a queue row hit for either the compact sidebar or the full table.
pub(crate) fn row_hit_rect(
    origin: Rect,
    first_visible: usize,
    index: usize,
    row_height: u16,
    row_stride: u16,
) -> Option<Rect> {
    let position = index.checked_sub(first_visible)?;
    let offset = u16::try_from(position).ok()?.checked_mul(row_stride)?;
    let y = origin.y.checked_add(offset)?;
    let bottom = y.checked_add(row_height)?;
    if bottom > origin.bottom() {
        return None;
    }
    Some(Rect::new(origin.x, y, origin.width, row_height))
}

#[cfg(test)]
mod tests {
    use super::{row_hit_rect, QueueRowProjection, SpotifyQueueLabels};
    use crate::state::{
        AppData, MediaId, MediaKind, PlayableMedia, PlayerState, Provider, QueueDisplayItemRef,
        QueueOrigin, QueuedItem, SpotifyQueueLabelCache, UnifiedPlaylist, UnifiedPlaylistItem,
    };
    use ratatui::layout::Rect;
    use rspotify::model::{PlayableId, TrackId};

    const LISTED: &str = "track0000000000000000000000000001";
    const UNLISTED: &str = "track0000000000000000000000000002";

    fn spotify_entry(raw_id: &str) -> QueuedItem {
        QueuedItem {
            entry_id: 1,
            media: PlayableMedia::Spotify(PlayableId::Track(
                TrackId::from_id(raw_id).unwrap().into_static(),
            )),
            origin: QueueOrigin::Context,
        }
    }

    fn data_with_listed_track(folder: &std::path::Path) -> AppData {
        let mut data = AppData::new(folder, folder);
        data.unified_playlists = vec![UnifiedPlaylist {
            id: "mix".to_owned(),
            name: "Mix".to_owned(),
            items: vec![UnifiedPlaylistItem {
                media_id: MediaId {
                    provider: Provider::Spotify,
                    kind: MediaKind::Track,
                    raw_id: LISTED.to_owned(),
                },
                title: "Listed Song".to_owned(),
                artists: "Listed Artist".to_owned(),
                duration_ms: Some(225_000),
                ..UnifiedPlaylistItem::default()
            }],
            updated_at: 0,
            next_entry_id: 2,
        }];
        data
    }

    fn project(entry: &QueuedItem, labels: &SpotifyQueueLabels<'_>) -> QueueRowProjection {
        QueueRowProjection::from_item(
            &QueueDisplayItemRef::Unified {
                item: entry,
                is_current: false,
            },
            labels,
        )
    }

    #[test]
    fn spotify_queue_entries_show_their_playlist_details() {
        let folder = tempfile::tempdir().unwrap();
        let data = data_with_listed_track(folder.path());
        let player = PlayerState::default();
        let added = SpotifyQueueLabelCache::default();
        let labels = SpotifyQueueLabels::new(&player, &data, &added);

        let listed = project(&spotify_entry(LISTED), &labels);
        assert_eq!(
            (
                listed.title.as_str(),
                listed.artists.as_str(),
                listed.duration.as_str()
            ),
            ("Listed Song", "Listed Artist", "3:45")
        );

        let unlisted = project(&spotify_entry(UNLISTED), &labels);
        assert_eq!(unlisted.title, "Spotify track");
        assert!(unlisted.artists.is_empty());
    }

    #[test]
    fn spotify_tracks_added_from_search_keep_their_names() {
        let folder = tempfile::tempdir().unwrap();
        let data = AppData::new(folder.path(), folder.path());
        let player = PlayerState::default();
        let mut added = SpotifyQueueLabelCache::default();
        added.remember_track(&crate::state::Track {
            id: TrackId::from_id(UNLISTED).unwrap().into_static(),
            name: "Searched Song".to_owned(),
            artists: vec![crate::state::Artist {
                id: rspotify::model::ArtistId::from_id("artist0000000000000000000001")
                    .unwrap()
                    .into_static(),
                name: "Searched Artist".to_owned(),
            }],
            album: None,
            duration: std::time::Duration::from_secs(61),
            explicit: false,
            added_at: 0,
        });
        let labels = SpotifyQueueLabels::new(&player, &data, &added);

        let row = project(&spotify_entry(UNLISTED), &labels);
        assert_eq!(
            (
                row.title.as_str(),
                row.artists.as_str(),
                row.duration.as_str()
            ),
            ("Searched Song", "Searched Artist", "1:01")
        );
    }

    #[test]
    fn queue_hit_geometry_is_shared_by_compact_and_table_rows() {
        let sidebar = Rect::new(4, 10, 24, 7);
        assert_eq!(
            row_hit_rect(sidebar, 0, 1, 2, 3),
            Some(Rect::new(4, 13, 24, 2))
        );
        let table = Rect::new(8, 20, 30, 3);
        assert_eq!(
            row_hit_rect(table, 5, 7, 1, 1),
            Some(Rect::new(8, 22, 30, 1))
        );
        assert_eq!(row_hit_rect(table, 5, 8, 1, 1), None);
    }
}
