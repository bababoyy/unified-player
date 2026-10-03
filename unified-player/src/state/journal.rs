use std::{
    collections::HashMap,
    io::{BufReader, BufWriter},
    path::Path,
};

use anyhow::Result;
use serde::{Deserialize, Serialize};

use super::model::{Id, Track, YouTubeTrack};

const JOURNAL_FILE: &str = "journal.json";

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TrackJournal {
    #[serde(default)]
    pub schema_version: u8,
    pub entries: HashMap<String, TrackJournalEntry>,
    /// Provider-neutral journal entries that do not have a Spotify URI.
    /// Spotify entries stay in `entries` for backward-compatible rendering and
    /// migration; `YouTube` identities are keyed by their stable video URI here.
    #[serde(default)]
    pub youtube_entries: HashMap<String, YouTubeJournalEntry>,
    #[serde(default)]
    pub lists: Vec<JournalList>,
}

impl Default for TrackJournal {
    fn default() -> Self {
        Self {
            schema_version: 2,
            entries: HashMap::new(),
            youtube_entries: HashMap::new(),
            lists: Vec::new(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct JournalList {
    pub id: String,
    pub name: String,
    #[serde(default)]
    pub track_uris: Vec<String>,
    pub created_at: u64,
    pub updated_at: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct YouTubeJournalEntry {
    pub track: YouTubeTrack,
    pub created_at: u64,
    pub updated_at: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TrackJournalEntry {
    pub track: Track,
    pub rating: Option<u8>,
    pub note: String,
    pub listen_later: bool,
    pub listened: bool,
    pub created_at: u64,
    pub updated_at: u64,
}

impl TrackJournal {
    pub fn load(config_folder: &Path) -> Self {
        let path = config_folder.join(JOURNAL_FILE);
        if !path.exists() {
            return Self::default();
        }

        tracing::info!("Loading the track journal");
        let file = match std::fs::File::open(&path) {
            Ok(file) => file,
            Err(err) => {
                crate::observability::log_safe_error!(
                    error,
                    crate::observability::DiagnosticCode::JOURNAL_OPEN_FAILED,
                    crate::observability::ErrorCategory::Storage,
                    &err,
                    "Failed to open the track journal"
                );
                return Self::default();
            }
        };

        match serde_json::from_reader::<_, Self>(BufReader::new(file)) {
            Ok(mut journal) => {
                journal.migrate();
                journal
            }
            Err(err) => {
                crate::observability::log_safe_error!(
                    error,
                    crate::observability::DiagnosticCode::JOURNAL_DECODE_FAILED,
                    crate::observability::ErrorCategory::Decode,
                    &err,
                    "Failed to decode the track journal"
                );
                Self::default()
            }
        }
    }

    pub fn save(&self, config_folder: &Path) -> Result<()> {
        std::fs::create_dir_all(config_folder)?;
        let path = config_folder.join(JOURNAL_FILE);
        let file = BufWriter::new(std::fs::File::create(path)?);
        serde_json::to_writer_pretty(file, self)?;
        Ok(())
    }

    pub fn entries_sorted(&self) -> Vec<TrackJournalEntry> {
        let mut entries = self.entries.values().cloned().collect::<Vec<_>>();
        entries.sort_by_key(|entry| {
            (
                entry.listened,
                !entry.listen_later,
                std::cmp::Reverse(entry.updated_at),
            )
        });
        entries
    }

    pub fn list_entries(&self, list_id: &str) -> Vec<TrackJournalEntry> {
        let Some(list) = self.lists.iter().find(|list| list.id == list_id) else {
            return Vec::new();
        };

        list.track_uris
            .iter()
            .filter_map(|uri| self.entries.get(uri).cloned())
            .collect()
    }

    pub fn list(&self, list_id: &str) -> Option<&JournalList> {
        self.lists.iter().find(|list| list.id == list_id)
    }

    pub fn entry_for_track(&self, track: &Track) -> Option<&TrackJournalEntry> {
        self.entries.get(&track.id.uri())
    }

    pub fn set_rating(&mut self, track: Track, rating_half_steps: Option<u8>) {
        let key = track.id.uri();
        let entry = self.entry_mut(track);
        entry.rating = rating_half_steps;
        entry.updated_at = now_secs();
        self.prune_empty_entry(&key);
    }

    pub fn set_note(&mut self, track: Track, note: String) {
        let key = track.id.uri();
        let entry = self.entry_mut(track);
        entry.note = note;
        entry.updated_at = now_secs();
        self.prune_empty_entry(&key);
    }

    pub fn set_listen_later(&mut self, track: Track, listen_later: bool) {
        let key = track.id.uri();
        let entry = self.entry_mut(track);
        entry.listen_later = listen_later;
        entry.updated_at = now_secs();
        self.prune_empty_entry(&key);
    }

    pub fn set_listened(&mut self, track: Track, listened: bool) {
        let key = track.id.uri();
        let entry = self.entry_mut(track);
        entry.listened = listened;
        if listened {
            entry.listen_later = false;
        }
        entry.updated_at = now_secs();
        self.prune_empty_entry(&key);
    }

    pub fn remove_track(&mut self, track: &Track) {
        let uri = track.id.uri();
        self.entries.remove(&uri);
        for list in &mut self.lists {
            list.track_uris.retain(|track_uri| track_uri != &uri);
        }
    }

    pub fn add_youtube_tracks_to_list(
        &mut self,
        list_id: &str,
        tracks: impl IntoIterator<Item = YouTubeTrack>,
    ) {
        let now = now_secs();
        let mut uris = Vec::new();
        for track in tracks {
            let uri = youtube_uri(&track.id);
            self.youtube_entries
                .entry(uri.clone())
                .and_modify(|entry| entry.updated_at = now)
                .or_insert_with(|| YouTubeJournalEntry {
                    track,
                    created_at: now,
                    updated_at: now,
                });
            uris.push(uri);
        }

        if let Some(list) = self.lists.iter_mut().find(|list| list.id == list_id) {
            for uri in uris {
                if !list.track_uris.iter().any(|track_uri| track_uri == &uri) {
                    list.track_uris.push(uri);
                }
            }
            list.updated_at = now;
        }
    }

    pub fn create_list(&mut self, name: String) -> String {
        let now = now_secs();
        let id = format!("list-{now}-{}", self.lists.len() + 1);
        self.lists.push(JournalList {
            id: id.clone(),
            name,
            track_uris: Vec::new(),
            created_at: now,
            updated_at: now,
        });
        id
    }

    pub fn rename_list(&mut self, list_id: &str, name: String) {
        if let Some(list) = self.lists.iter_mut().find(|list| list.id == list_id) {
            list.name = name;
            list.updated_at = now_secs();
        }
    }

    pub fn delete_list(&mut self, list_id: &str) {
        self.lists.retain(|list| list.id != list_id);
    }

    pub fn move_list(&mut self, index: usize, offset: isize) -> Option<usize> {
        if index >= self.lists.len() {
            return None;
        }

        let new_index = move_index(index, offset, self.lists.len());
        if new_index != index {
            self.lists.swap(index, new_index);
        }
        Some(new_index)
    }

    pub fn add_tracks_to_list(&mut self, list_id: &str, tracks: impl IntoIterator<Item = Track>) {
        let now = now_secs();
        let mut uris = Vec::new();
        for track in tracks {
            let uri = track.id.uri();
            self.entry_mut(track).updated_at = now;
            uris.push(uri);
        }

        if let Some(list) = self.lists.iter_mut().find(|list| list.id == list_id) {
            for uri in uris {
                if !list.track_uris.iter().any(|track_uri| track_uri == &uri) {
                    list.track_uris.push(uri);
                }
            }
            list.updated_at = now;
        }
    }

    pub fn remove_track_from_list(&mut self, list_id: &str, track_uri: &str) {
        if let Some(list) = self.lists.iter_mut().find(|list| list.id == list_id) {
            list.track_uris.retain(|uri| uri != track_uri);
            list.updated_at = now_secs();
        }
    }

    pub fn move_track_in_list(
        &mut self,
        list_id: &str,
        index: usize,
        offset: isize,
    ) -> Option<usize> {
        let list = self.lists.iter_mut().find(|list| list.id == list_id)?;
        if index >= list.track_uris.len() {
            return None;
        }

        let new_index = move_index(index, offset, list.track_uris.len());
        if new_index != index {
            list.track_uris.swap(index, new_index);
            list.updated_at = now_secs();
        }
        Some(new_index)
    }

    fn entry_mut(&mut self, track: Track) -> &mut TrackJournalEntry {
        let key = track.id.uri();
        self.entries.entry(key).or_insert_with(|| {
            let now = now_secs();
            TrackJournalEntry {
                track,
                rating: None,
                note: String::new(),
                listen_later: false,
                listened: false,
                created_at: now,
                updated_at: now,
            }
        })
    }

    fn prune_empty_entry(&mut self, key: &str) {
        if self
            .entries
            .get(key)
            .is_some_and(TrackJournalEntry::is_empty)
            && !self
                .lists
                .iter()
                .any(|list| list.track_uris.iter().any(|uri| uri == key))
        {
            self.entries.remove(key);
        }
    }

    fn migrate(&mut self) {
        if self.schema_version < 1 {
            for entry in self.entries.values_mut() {
                if let Some(rating) = entry.rating {
                    entry.rating = Some(rating.saturating_mul(2).min(10));
                }
            }
            self.schema_version = 1;
        }
        if self.schema_version < 2 {
            self.schema_version = 2;
        }
    }
}

impl JournalList {
    pub fn track_count(&self) -> usize {
        self.track_uris.len()
    }
}

impl std::fmt::Display for JournalList {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.name)
    }
}

impl TrackJournalEntry {
    pub fn rating_text(&self) -> String {
        self.rating.map(format_rating).unwrap_or_default()
    }

    fn is_empty(&self) -> bool {
        self.rating.is_none() && self.note.is_empty() && !self.listen_later && !self.listened
    }
}

pub fn format_rating(rating_half_steps: u8) -> String {
    let whole = rating_half_steps / 2;
    if rating_half_steps.is_multiple_of(2) {
        format!("{whole}/5")
    } else if whole == 0 {
        "½/5".to_string()
    } else {
        format!("{whole}½/5")
    }
}

impl std::fmt::Display for TrackJournalEntry {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "{} {} {} {}",
            self.track.name,
            self.track.artists_info(),
            self.track.album_info(),
            self.note
        )
    }
}

fn now_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

pub fn youtube_uri(video_id: &str) -> String {
    format!("youtube:track:{video_id}")
}

fn move_index(index: usize, offset: isize, len: usize) -> usize {
    if offset.is_negative() {
        index.saturating_sub(offset.unsigned_abs())
    } else {
        std::cmp::min(index + offset as usize, len - 1)
    }
}

#[cfg(test)]
mod tests {
    use super::{youtube_uri, TrackJournal};
    use crate::state::YouTubeTrack;

    fn track(id: &str) -> YouTubeTrack {
        YouTubeTrack {
            id: id.to_owned(),
            name: "Track".to_owned(),
            artists: "Artist".to_owned(),
            album: None,
            duration: "3:00".to_owned(),
            explicit: false,
            thumbnail_url: None,
            is_video: false,
        }
    }

    #[test]
    fn youtube_entries_keep_provider_identity_in_journal_lists() {
        let mut journal = TrackJournal::default();
        let list_id = journal.create_list("Mixed".to_owned());
        journal.add_youtube_tracks_to_list(&list_id, [track("video")]);

        assert_eq!(journal.lists[0].track_uris, vec![youtube_uri("video")]);
        assert_eq!(journal.youtube_entries.len(), 1);
        assert_eq!(
            journal.youtube_entries[&youtube_uri("video")].track.id,
            "video"
        );

        let encoded = serde_json::to_string(&journal).expect("encode journal");
        let decoded: TrackJournal = serde_json::from_str(&encoded).expect("decode journal");
        assert_eq!(decoded.lists[0].track_uris, journal.lists[0].track_uris);
        assert_eq!(decoded.youtube_entries, journal.youtube_entries);
    }
}
