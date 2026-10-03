use std::path::Path;

use rspotify::prelude::Id;
use serde::{Deserialize, Serialize};

use super::{MediaId, MediaKind, Provider, YouTubeTrack};

const SESSION_HISTORY_FILE: &str = "session-history.json";
const SESSION_HISTORY_SCHEMA_VERSION: u32 = 1;
const MAX_SESSION_HISTORY_ENTRIES: usize = 10_000;

/// A provider-neutral, local record of a playback transition. Metadata is
/// intentionally denormalized so future playlist generation works offline.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionEntry {
    pub media_id: MediaId,
    pub title: String,
    pub artists: String,
    pub album: Option<String>,
    pub duration_ms: Option<u64>,
    pub started_at: u64,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionHistory {
    #[serde(default = "session_history_schema_version")]
    pub schema_version: u32,
    #[serde(default)]
    pub entries: Vec<SessionEntry>,
}

impl Default for SessionHistory {
    fn default() -> Self {
        Self {
            schema_version: SESSION_HISTORY_SCHEMA_VERSION,
            entries: Vec::new(),
        }
    }
}

const fn session_history_schema_version() -> u32 {
    SESSION_HISTORY_SCHEMA_VERSION
}

impl SessionHistory {
    pub fn load(config_folder: &Path) -> Self {
        let path = config_folder.join(SESSION_HISTORY_FILE);
        if !path.exists() {
            return Self {
                schema_version: SESSION_HISTORY_SCHEMA_VERSION,
                entries: Vec::new(),
            };
        }
        let file = match std::fs::File::open(path) {
            Ok(file) => file,
            Err(error) => {
                crate::observability::log_safe_error!(
                    warn,
                    crate::observability::DiagnosticCode::SESSION_HISTORY_OPEN_FAILED,
                    crate::observability::ErrorCategory::Storage,
                    &error,
                    "Local session history could not be opened; starting empty"
                );
                return Self::default();
            }
        };
        match serde_json::from_reader(file) {
            Ok(history) => history,
            Err(error) => {
                crate::observability::log_safe_error!(
                    warn,
                    crate::observability::DiagnosticCode::SESSION_HISTORY_DECODE_FAILED,
                    crate::observability::ErrorCategory::Decode,
                    &error,
                    "Local session history could not be read; starting empty"
                );
                Self {
                    schema_version: SESSION_HISTORY_SCHEMA_VERSION,
                    entries: Vec::new(),
                }
            }
        }
    }

    pub fn save(&self, config_folder: &Path) -> anyhow::Result<()> {
        std::fs::create_dir_all(config_folder)?;
        let path = config_folder.join(SESSION_HISTORY_FILE);
        let file = std::fs::File::create(path)?;
        serde_json::to_writer_pretty(std::io::BufWriter::new(file), self)?;
        Ok(())
    }

    /// Record a transition, suppressing duplicate refreshes for the same item.
    pub fn record(&mut self, entry: SessionEntry, max_entries: usize) -> bool {
        let max_entries = max_entries.min(MAX_SESSION_HISTORY_ENTRIES);
        if max_entries == 0
            || self
                .entries
                .last()
                .is_some_and(|previous| previous.media_id == entry.media_id)
        {
            return false;
        }
        self.entries.push(entry);
        if self.entries.len() > max_entries {
            let remove = self.entries.len() - max_entries;
            self.entries.drain(..remove);
        }
        true
    }

    #[allow(dead_code)]
    pub fn clear(&mut self) {
        self.entries.clear();
    }

    #[allow(dead_code)]
    pub fn newest_first(&self) -> impl Iterator<Item = &SessionEntry> {
        self.entries.iter().rev()
    }
}

impl SessionEntry {
    pub fn from_youtube_track(track: &YouTubeTrack, started_at: u64) -> Self {
        Self {
            media_id: MediaId {
                provider: Provider::YouTubeMusic,
                kind: if track.is_video {
                    MediaKind::Video
                } else {
                    MediaKind::Track
                },
                raw_id: track.id.clone(),
            },
            title: track.name.clone(),
            artists: track.artists.clone(),
            album: track.album.clone(),
            duration_ms: parse_duration_ms(&track.duration),
            started_at,
        }
    }

    pub fn from_spotify_item(
        item: &rspotify::model::PlayableItem,
        started_at: u64,
    ) -> Option<Self> {
        match item {
            rspotify::model::PlayableItem::Track(track) => Some(Self {
                media_id: MediaId {
                    provider: Provider::Spotify,
                    kind: MediaKind::Track,
                    raw_id: track.id.as_ref()?.id().to_owned(),
                },
                title: track.name.clone(),
                artists: track
                    .artists
                    .iter()
                    .map(|artist| artist.name.as_str())
                    .collect::<Vec<_>>()
                    .join(", "),
                album: Some(track.album.name.clone()),
                duration_ms: track
                    .duration
                    .to_std()
                    .ok()
                    .map(|value| value.as_millis() as u64),
                started_at,
            }),
            rspotify::model::PlayableItem::Episode(episode) => Some(Self {
                media_id: MediaId {
                    provider: Provider::Spotify,
                    kind: MediaKind::Episode,
                    raw_id: episode.id.id().to_owned(),
                },
                title: episode.name.clone(),
                artists: episode.show.name.clone(),
                album: Some(episode.show.name.clone()),
                duration_ms: episode
                    .duration
                    .to_std()
                    .ok()
                    .map(|value| value.as_millis() as u64),
                started_at,
            }),
            rspotify::model::PlayableItem::Unknown(_) => None,
        }
    }
}

fn parse_duration_ms(value: &str) -> Option<u64> {
    let mut seconds = 0_u64;
    for part in value.split(':') {
        seconds = seconds.checked_mul(60)?.checked_add(part.parse().ok()?)?;
    }
    Some(seconds.saturating_mul(1_000))
}

pub fn now_unix_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |duration| duration.as_secs())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(id: &str) -> SessionEntry {
        SessionEntry {
            media_id: MediaId {
                provider: Provider::Spotify,
                kind: MediaKind::Track,
                raw_id: id.to_owned(),
            },
            title: id.to_owned(),
            artists: String::new(),
            album: None,
            duration_ms: None,
            started_at: 1,
        }
    }

    #[test]
    fn history_is_bounded_and_deduplicates_refreshes() {
        let mut history = SessionHistory::default();
        assert!(history.record(entry("a"), 2));
        assert!(!history.record(entry("a"), 2));
        assert!(history.record(entry("b"), 2));
        assert!(history.record(entry("c"), 2));
        assert_eq!(history.entries.len(), 2);
        assert_eq!(history.entries[0].media_id.raw_id, "b");
    }

    #[test]
    fn youtube_duration_parsing_is_provider_neutral() {
        let track = YouTubeTrack {
            id: "v".to_owned(),
            name: "Video".to_owned(),
            artists: "Artist".to_owned(),
            album: None,
            duration: "1:02".to_owned(),
            explicit: false,
            thumbnail_url: None,
            is_video: true,
        };
        let entry = SessionEntry::from_youtube_track(&track, 10);
        assert_eq!(entry.duration_ms, Some(62_000));
        assert_eq!(entry.media_id.kind, MediaKind::Video);
    }

    #[test]
    fn malformed_history_fails_closed_to_an_empty_current_schema() {
        let folder = tempfile::tempdir().unwrap();
        std::fs::write(folder.path().join(SESSION_HISTORY_FILE), b"not-json").unwrap();

        let history = SessionHistory::load(folder.path());

        assert_eq!(history.schema_version, SESSION_HISTORY_SCHEMA_VERSION);
        assert!(history.entries.is_empty());
    }
}
