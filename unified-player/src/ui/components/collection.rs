//! Shared provider-neutral collection row projections.

use crate::state::{Track, YouTubeTrack};

/// Common title/artist/duration fields used by workspace and legacy
/// collection/context renderers. Provider-specific columns remain page-owned.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct CollectionTrackRow {
    pub(crate) title: String,
    pub(crate) artist: String,
    pub(crate) duration: String,
}

impl CollectionTrackRow {
    pub(crate) fn from_youtube(track: &YouTubeTrack) -> Self {
        Self {
            title: track.name.clone(),
            artist: track.artists.clone(),
            duration: track.duration.clone(),
        }
    }

    pub(crate) fn from_spotify(track: &Track) -> Self {
        Self {
            title: track.display_name().into_owned(),
            artist: track.artists_info(),
            duration: format!(
                "{}:{:02}",
                track.duration.as_secs() / 60,
                track.duration.as_secs() % 60
            ),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::CollectionTrackRow;
    use crate::state::YouTubeTrack;

    #[test]
    fn provider_collection_rows_share_title_artist_duration_fields() {
        let youtube = YouTubeTrack {
            id: "youtube".to_owned(),
            name: "Video".to_owned(),
            artists: "Artist".to_owned(),
            album: None,
            duration: "3:21".to_owned(),
            explicit: false,
            thumbnail_url: None,
            is_video: false,
        };
        let row = CollectionTrackRow::from_youtube(&youtube);
        assert_eq!(row.title, "Video");
        assert_eq!(row.artist, "Artist");
        assert_eq!(row.duration, "3:21");
    }
}
