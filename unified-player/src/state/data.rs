use std::io::{BufReader, BufWriter, Write as _};
use std::path::PathBuf;
use std::{collections::HashMap, path::Path};

use serde::{de::DeserializeOwned, Deserialize, Serialize};
use std::sync::{Arc, LazyLock};

use super::model::{
    Album, Artist, Category, Context, ContextId, Id, ListenBrainzLocalApplySnapshot,
    ListenBrainzRecoveryDisposition, ListenBrainzSyncBase, ListenBrainzSyncIntent,
    ListenBrainzSyncState, Playlist, PlaylistEntryId, PlaylistFolderItem, PlaylistFolderNode,
    PlaylistLink, PlaylistProjectionMapping, PlaylistProjectionRecovery, PlaylistProjectionState,
    PlaylistProjectionStatus, Provider, SearchResults, Show, Track, UnifiedPlaylist,
    UnifiedPlaylistItem, YouTubeLibrary, YouTubeSearchResults,
};
use super::Lyrics;
use super::TrackJournal;
use super::{SessionEntry, SessionHistory};
use crate::config::SessionHistoryConfig;

pub type DataReadGuard<'a> = parking_lot::RwLockReadGuard<'a, AppData>;

#[derive(Debug, Copy, Clone)]
pub enum FileCacheKey {
    Playlists,
    PlaylistFolders,
    FollowedArtists,
    SavedShows,
    SavedAlbums,
    SavedTracks,
}

/// default time-to-live cache duration
pub static TTL_CACHE_DURATION: LazyLock<std::time::Duration> =
    LazyLock::new(|| std::time::Duration::from_secs(60 * 60));
pub const LISTENBRAINZ_RESOLUTION_PENDING_TTL: std::time::Duration =
    std::time::Duration::from_secs(15);
pub const LISTENBRAINZ_RESOLUTION_NEGATIVE_TTL: std::time::Duration =
    std::time::Duration::from_secs(10 * 60);
pub const LISTENBRAINZ_RESOLUTION_FAILURE_TTL: std::time::Duration =
    std::time::Duration::from_secs(30);

const UNIFIED_PLAYLISTS_FILE: &str = "unified_playlists.json";
const UNIFIED_PLAYLISTS_BACKUP_FILE: &str = "unified_playlists.json.bak";
const UNIFIED_PLAYLISTS_SCHEMA_VERSION: u32 = 3;

#[derive(Debug, Serialize, Deserialize)]
struct UnifiedPlaylistStore {
    schema_version: u32,
    playlists: Vec<UnifiedPlaylist>,
    #[serde(default)]
    links: Vec<PlaylistLink>,
}

#[derive(Debug, Deserialize)]
#[serde(untagged)]
enum UnifiedPlaylistStoreFile {
    Versioned(UnifiedPlaylistStore),
    Legacy(Vec<UnifiedPlaylist>),
}

/// the application's data
pub struct AppData {
    pub user_data: UserData,
    pub journal: TrackJournal,
    pub session_history: SessionHistory,
    pub context_history: super::ContextHistory,
    /// Spotify reads for Home; session-local.
    pub home_feed: super::HomeFeed,
    pub caches: MemoryCaches,
    pub browse: BrowseData,
    #[allow(dead_code)]
    pub unified_playlists: Vec<UnifiedPlaylist>,
    pub playlist_links: Vec<PlaylistLink>,
    #[allow(dead_code)]
    config_folder: PathBuf,
    unified_playlist_store_writable: bool,
}

#[derive(Debug)]
/// current user's data
pub struct UserData {
    pub user: Option<rspotify::model::PrivateUser>,
    pub playlists: Vec<PlaylistFolderItem>,
    pub playlist_folder_node: Option<PlaylistFolderNode>,
    pub followed_artists: Vec<Artist>,
    pub saved_shows: Vec<Show>,
    pub saved_albums: Vec<Album>,
    pub saved_tracks: HashMap<String, Track>,
    pub youtube_library: YouTubeLibrary,
}

/// the application's in-memory caches
pub struct MemoryCaches {
    pub context: ttl_cache::TtlCache<String, Context>,
    pub listenbrainz_recordings: ttl_cache::TtlCache<String, ListenBrainzRecordingResolution>,
    pub listenbrainz_albums: ttl_cache::TtlCache<String, ListenBrainzAlbumResolution>,
    pub search: ttl_cache::TtlCache<String, Arc<SearchResults>>,
    pub youtube_search: ttl_cache::TtlCache<String, Arc<YouTubeSearchResults>>,
    pub lyrics: ttl_cache::TtlCache<LyricsCacheKey, Option<Lyrics>>,
    pub genres: ttl_cache::TtlCache<String, Vec<String>>,
    #[cfg(feature = "image")]
    pub images: ttl_cache::TtlCache<String, image::DynamicImage>,
}

/// Identity of one lyrics lookup. The selected external source is part of the
/// key so a late result from an older source cannot replace the visible
/// source's lyrics for the same track.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct LyricsCacheKey {
    pub track_uri: String,
    pub source: Option<String>,
}

impl LyricsCacheKey {
    pub fn new(track_uri: impl Into<String>, source: Option<&str>) -> Self {
        Self {
            track_uri: track_uri.into(),
            source: source.map(str::to_owned),
        }
    }
}

#[derive(Clone, Debug)]
pub enum ListenBrainzRecordingResolution {
    Resolving { request_id: u64 },
    Resolved(Track),
    NoSpotifyRelation,
    Unavailable,
}

pub fn listenbrainz_recording_key(recording_mbid: &str) -> String {
    format!("listenbrainz:recording:{recording_mbid}")
}

#[derive(Clone, Debug)]
pub enum ListenBrainzAlbumResolution {
    Resolving { request_id: u64 },
    Resolved(Album),
    NoSpotifyRelation,
    Unavailable,
}

pub fn listenbrainz_album_key(release_group_mbid: &str) -> String {
    format!("listenbrainz:release-group:{release_group_mbid}")
}

#[derive(Default, Debug)]
/// Spotify browse data
pub struct BrowseData {
    pub categories: Vec<Category>,
    /// Distinguishes the initial request from a provider-confirmed empty set.
    pub categories_loaded: bool,
    pub category_playlists: HashMap<String, Vec<Playlist>>,
}

impl MemoryCaches {
    pub fn new() -> Self {
        Self {
            context: ttl_cache::TtlCache::new(64),
            listenbrainz_recordings: ttl_cache::TtlCache::new(64),
            listenbrainz_albums: ttl_cache::TtlCache::new(64),
            search: ttl_cache::TtlCache::new(64),
            youtube_search: ttl_cache::TtlCache::new(64),
            lyrics: ttl_cache::TtlCache::new(64),
            genres: ttl_cache::TtlCache::new(64),
            #[cfg(feature = "image")]
            images: ttl_cache::TtlCache::new(64),
        }
    }
}

impl AppData {
    pub fn new(config_folder: &Path, cache_folder: &Path) -> Self {
        let (unified_playlists, playlist_links, unified_playlist_store_writable) =
            match load_unified_playlist_store(config_folder) {
                Ok(Some((playlists, links))) => (playlists, links, true),
                Ok(None) => (Vec::new(), Vec::new(), true),
                Err(err) => {
                    crate::observability::log_safe_error!(
                        error,
                        crate::observability::DiagnosticCode::UNIFIED_PLAYLIST_LOAD_FAILED,
                        crate::observability::ErrorCategory::Storage,
                        &err,
                        "Unified playlists could not be loaded and will not be overwritten"
                    );
                    (Vec::new(), Vec::new(), false)
                }
            };
        Self {
            user_data: UserData::new_from_file_caches(cache_folder),
            journal: TrackJournal::load(config_folder),
            session_history: SessionHistory::load(config_folder),
            context_history: super::ContextHistory::load(config_folder),
            home_feed: super::HomeFeed::default(),
            caches: MemoryCaches::new(),
            browse: BrowseData::default(),
            unified_playlists,
            playlist_links,
            config_folder: config_folder.to_path_buf(),
            unified_playlist_store_writable,
        }
    }

    pub fn record_session_entry(
        &mut self,
        entry: SessionEntry,
        config: &SessionHistoryConfig,
    ) -> anyhow::Result<bool> {
        if !config.enabled {
            return Ok(false);
        }
        let previous = self.session_history.clone();
        if !self.session_history.record(entry, config.max_entries) {
            return Ok(false);
        }
        if let Err(error) = self.session_history.save(&self.config_folder) {
            self.session_history = previous;
            return Err(error);
        }
        Ok(true)
    }

    /// Record an opened context for Home's "Continue" shelf.
    pub fn record_context_open(
        &mut self,
        entry: super::ContextHistoryEntry,
    ) -> anyhow::Result<bool> {
        let previous = self.context_history.clone();
        if !self.context_history.record(entry) {
            return Ok(false);
        }
        if let Err(error) = self.context_history.save(&self.config_folder) {
            self.context_history = previous;
            return Err(error);
        }
        Ok(true)
    }

    pub fn clear_context_history(&mut self) -> anyhow::Result<()> {
        self.context_history.clear();
        self.context_history.save(&self.config_folder)
    }

    #[allow(dead_code)]
    pub fn clear_session_history(&mut self) -> anyhow::Result<()> {
        self.session_history.clear();
        self.session_history.save(&self.config_folder)
    }

    #[allow(dead_code)]
    pub fn save_unified_playlists(&self) -> anyhow::Result<()> {
        anyhow::ensure!(
            self.unified_playlist_store_writable,
            "refusing to overwrite a unified playlist store that could not be loaded"
        );
        std::fs::create_dir_all(&self.config_folder)?;
        let path = self.config_folder.join(UNIFIED_PLAYLISTS_FILE);
        if path.exists() && read_unified_playlist_store_file(&path).is_ok() {
            let primary = std::fs::read(&path)?;
            atomic_write(
                &self.config_folder.join(UNIFIED_PLAYLISTS_BACKUP_FILE),
                &primary,
            )?;
        }
        write_unified_playlists(&path, &self.unified_playlists, &self.playlist_links)?;
        Ok(())
    }

    pub fn export_unified_playlists(&self, path: &Path) -> anyhow::Result<()> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        write_unified_playlists(path, &self.unified_playlists, &self.playlist_links)
    }

    #[allow(dead_code)]
    pub fn upsert_unified_playlist(&mut self, playlist: UnifiedPlaylist) -> anyhow::Result<()> {
        let previous = self.unified_playlists.clone();
        let mut playlist = playlist;
        playlist.normalize_entry_ids()?;
        if let Some(existing) = self
            .unified_playlists
            .iter_mut()
            .find(|existing| existing.id == playlist.id)
        {
            *existing = playlist;
        } else {
            self.unified_playlists.push(playlist);
        }
        if let Err(error) = self.save_unified_playlists() {
            self.unified_playlists = previous;
            return Err(error);
        }
        Ok(())
    }

    /// Replace a playlist and its optional projection link in one local save.
    /// This is used by import/restore so a failed write cannot leave one side
    /// of the local relationship updated in memory.
    pub fn upsert_unified_playlist_with_link(
        &mut self,
        playlist: UnifiedPlaylist,
        link: PlaylistLink,
    ) -> anyhow::Result<()> {
        anyhow::ensure!(
            playlist.id == link.unified_playlist_id,
            "unified playlist link targets a different playlist"
        );
        let previous_playlists = self.unified_playlists.clone();
        let previous_links = self.playlist_links.clone();
        let mut playlist = playlist;
        playlist.normalize_entry_ids()?;
        if let Some(existing) = self
            .unified_playlists
            .iter_mut()
            .find(|existing| existing.id == playlist.id)
        {
            *existing = playlist;
        } else {
            self.unified_playlists.push(playlist);
        }
        if let Some(existing) = self
            .playlist_links
            .iter_mut()
            .find(|existing| existing.unified_playlist_id == link.unified_playlist_id)
        {
            *existing = link;
        } else {
            self.playlist_links.push(link);
        }
        if let Err(error) = self.save_unified_playlists() {
            self.unified_playlists = previous_playlists;
            self.playlist_links = previous_links;
            return Err(error);
        }
        Ok(())
    }

    pub fn rename_unified_playlist(
        &mut self,
        playlist_id: &str,
        name: String,
    ) -> anyhow::Result<()> {
        let playlist = self
            .unified_playlists
            .iter_mut()
            .find(|playlist| playlist.id == playlist_id)
            .ok_or_else(|| anyhow::anyhow!("unified playlist not found"))?;
        let previous = playlist.clone();
        playlist.name = name;
        playlist.updated_at = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)?
            .as_secs();
        if let Err(error) = self.save_unified_playlists() {
            if let Some(current) = self
                .unified_playlists
                .iter_mut()
                .find(|current| current.id == playlist_id)
            {
                *current = previous;
            }
            return Err(error);
        }
        Ok(())
    }

    pub fn delete_unified_playlist(&mut self, playlist_id: &str) -> anyhow::Result<()> {
        let previous_playlists = self.unified_playlists.clone();
        let previous_links = self.playlist_links.clone();
        let before = self.unified_playlists.len();
        self.unified_playlists
            .retain(|playlist| playlist.id != playlist_id);
        anyhow::ensure!(
            self.unified_playlists.len() != before,
            "unified playlist not found"
        );
        self.playlist_links
            .retain(|link| link.unified_playlist_id != playlist_id);
        if let Err(error) = self.save_unified_playlists() {
            self.unified_playlists = previous_playlists;
            self.playlist_links = previous_links;
            return Err(error);
        }
        Ok(())
    }

    /// Append exact item occurrences and persist them as one transaction.
    /// Duplicate media identities are intentionally retained: a unified
    /// playlist is an ordered occurrence list, not a set.
    pub fn append_unified_playlist_items(
        &mut self,
        playlist_id: &str,
        items: Vec<UnifiedPlaylistItem>,
    ) -> anyhow::Result<usize> {
        anyhow::ensure!(!items.is_empty(), "unified playlist mutation has no items");
        let playlist_index = self
            .unified_playlists
            .iter()
            .position(|playlist| playlist.id == playlist_id)
            .ok_or_else(|| anyhow::anyhow!("unified playlist not found"))?;
        let previous = self.unified_playlists[playlist_index].clone();
        let count = items.len();
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)?
            .as_secs();
        self.unified_playlists[playlist_index].normalize_entry_ids()?;
        let playlist = &mut self.unified_playlists[playlist_index];
        for item in items {
            let item = playlist.allocate_item(item)?;
            playlist.items.push(item);
        }
        self.unified_playlists[playlist_index].updated_at = now;
        if let Err(error) = self.save_unified_playlists() {
            self.unified_playlists[playlist_index] = previous;
            return Err(error);
        }
        Ok(count)
    }

    /// Remove exact playlist occurrences by their durable local IDs.
    #[allow(dead_code)]
    pub fn remove_unified_playlist_items(
        &mut self,
        playlist_id: &str,
        entry_ids: &[PlaylistEntryId],
    ) -> anyhow::Result<usize> {
        anyhow::ensure!(
            !entry_ids.is_empty(),
            "unified playlist mutation has no entries"
        );
        let playlist_index = self
            .unified_playlists
            .iter()
            .position(|playlist| playlist.id == playlist_id)
            .ok_or_else(|| anyhow::anyhow!("unified playlist not found"))?;
        let previous = self.unified_playlists[playlist_index].clone();
        let now = unix_timestamp()?;
        let requested = entry_ids
            .iter()
            .copied()
            .collect::<std::collections::HashSet<_>>();
        anyhow::ensure!(
            requested.len() == entry_ids.len(),
            "unified playlist mutation contains duplicate entry IDs"
        );
        let playlist = &mut self.unified_playlists[playlist_index];
        let available = playlist
            .items
            .iter()
            .map(|item| item.entry_id)
            .collect::<std::collections::HashSet<_>>();
        anyhow::ensure!(
            available.len() == playlist.items.len(),
            "unified playlist contains duplicate entry IDs"
        );
        anyhow::ensure!(
            requested
                .iter()
                .all(|entry_id| available.contains(entry_id)),
            "unified playlist entries are stale or missing"
        );
        let before = playlist.items.len();
        playlist
            .items
            .retain(|item| !requested.contains(&item.entry_id));
        let removed = before - playlist.items.len();
        anyhow::ensure!(
            removed == requested.len(),
            "unified playlist entries are stale or missing"
        );
        playlist.updated_at = now;
        if let Err(error) = self.save_unified_playlists() {
            self.unified_playlists[playlist_index] = previous;
            return Err(error);
        }
        Ok(removed)
    }

    #[allow(dead_code)]
    pub fn remove_unified_playlist_item(
        &mut self,
        playlist_id: &str,
        entry_id: PlaylistEntryId,
    ) -> anyhow::Result<()> {
        self.remove_unified_playlist_items(playlist_id, &[entry_id])?;
        Ok(())
    }

    /// Remove exact occurrences only when the complete occurrence order still
    /// matches the confirmation snapshot.
    pub fn remove_unified_playlist_items_if_current(
        &mut self,
        playlist_id: &str,
        expected_order: &[PlaylistEntryId],
        entry_ids: &[PlaylistEntryId],
    ) -> anyhow::Result<usize> {
        let current_order = self
            .unified_playlists
            .iter()
            .find(|playlist| playlist.id == playlist_id)
            .ok_or_else(|| anyhow::anyhow!("unified playlist not found"))?
            .items
            .iter()
            .map(|item| item.entry_id)
            .collect::<Vec<_>>();
        anyhow::ensure!(
            current_order == expected_order,
            "unified playlist removal snapshot is stale"
        );
        self.remove_unified_playlist_items(playlist_id, entry_ids)
    }

    /// Move a selected block to a post-removal insertion index while retaining
    /// every occurrence ID. The index is clamped to the resulting list.
    #[allow(dead_code)]
    pub fn move_unified_playlist_items(
        &mut self,
        playlist_id: &str,
        entry_ids: &[PlaylistEntryId],
        target_index: usize,
    ) -> anyhow::Result<()> {
        anyhow::ensure!(
            !entry_ids.is_empty(),
            "unified playlist mutation has no entries"
        );
        let playlist_index = self
            .unified_playlists
            .iter()
            .position(|playlist| playlist.id == playlist_id)
            .ok_or_else(|| anyhow::anyhow!("unified playlist not found"))?;
        let previous = self.unified_playlists[playlist_index].clone();
        let now = unix_timestamp()?;
        let selected = entry_ids
            .iter()
            .copied()
            .collect::<std::collections::HashSet<_>>();
        anyhow::ensure!(
            selected.len() == entry_ids.len(),
            "unified playlist mutation contains duplicate entry IDs"
        );
        let playlist = &mut self.unified_playlists[playlist_index];
        let available = playlist
            .items
            .iter()
            .map(|item| item.entry_id)
            .collect::<std::collections::HashSet<_>>();
        anyhow::ensure!(
            available.len() == playlist.items.len(),
            "unified playlist contains duplicate entry IDs"
        );
        anyhow::ensure!(
            selected.iter().all(|entry_id| available.contains(entry_id)),
            "unified playlist entries are stale or missing"
        );
        let mut moved = Vec::new();
        let mut remaining = Vec::with_capacity(playlist.items.len());
        for item in playlist.items.drain(..) {
            if selected.contains(&item.entry_id) {
                moved.push(item);
            } else {
                remaining.push(item);
            }
        }
        anyhow::ensure!(
            moved.len() == selected.len(),
            "unified playlist entries are stale or missing"
        );
        let insertion = target_index.min(remaining.len());
        remaining.splice(insertion..insertion, moved);
        playlist.items = remaining;
        playlist.updated_at = now;
        if let Err(error) = self.save_unified_playlists() {
            self.unified_playlists[playlist_index] = previous;
            return Err(error);
        }
        Ok(())
    }

    /// Apply a structural move only when the complete occurrence order still
    /// matches the snapshot used by the planner.
    pub fn move_unified_playlist_items_if_current(
        &mut self,
        playlist_id: &str,
        expected_order: &[PlaylistEntryId],
        entry_ids: &[PlaylistEntryId],
        target_index: usize,
    ) -> anyhow::Result<()> {
        let current_order = self
            .unified_playlists
            .iter()
            .find(|playlist| playlist.id == playlist_id)
            .ok_or_else(|| anyhow::anyhow!("unified playlist not found"))?
            .items
            .iter()
            .map(|item| item.entry_id)
            .collect::<Vec<_>>();
        anyhow::ensure!(
            current_order == expected_order,
            "unified playlist move snapshot is stale"
        );
        self.move_unified_playlist_items(playlist_id, entry_ids, target_index)
    }

    #[allow(dead_code)]
    pub fn move_unified_playlist_item(
        &mut self,
        playlist_id: &str,
        entry_id: PlaylistEntryId,
        target_index: usize,
    ) -> anyhow::Result<()> {
        self.move_unified_playlist_items(playlist_id, &[entry_id], target_index)
    }

    pub fn upsert_playlist_link(&mut self, link: PlaylistLink) -> anyhow::Result<()> {
        let previous = self.playlist_links.clone();
        if let Some(existing) = self
            .playlist_links
            .iter_mut()
            .find(|existing| existing.unified_playlist_id == link.unified_playlist_id)
        {
            *existing = link;
        } else {
            self.playlist_links.push(link);
        }
        if let Err(error) = self.save_unified_playlists() {
            self.playlist_links = previous;
            return Err(error);
        }
        Ok(())
    }

    pub fn store_verified_listenbrainz_base(
        &mut self,
        unified_playlist_id: &str,
        remote_playlist_id: &str,
        sync: ListenBrainzSyncState,
    ) -> anyhow::Result<()> {
        sync.validate()?;
        anyhow::ensure!(
            sync.base.unified_playlist_id == unified_playlist_id,
            "ListenBrainz sync base targets a different Unified playlist"
        );
        let previous = self.playlist_links.clone();
        let link = self
            .playlist_links
            .iter_mut()
            .find(|link| link.unified_playlist_id == unified_playlist_id)
            .ok_or_else(|| anyhow::anyhow!("unified playlist link not found"))?;
        anyhow::ensure!(
            link.listenbrainz_playlist_id.as_deref() == Some(remote_playlist_id),
            "ListenBrainz sync base targets a different remote playlist"
        );
        anyhow::ensure!(
            link.listenbrainz_sync.is_none(),
            "ListenBrainz sync base is already initialized"
        );
        link.listenbrainz_sync = Some(sync);
        link.updated_at = unix_timestamp()?;
        if let Err(error) = self.save_unified_playlists() {
            self.playlist_links = previous;
            return Err(error);
        }
        Ok(())
    }

    pub fn begin_listenbrainz_sync_intent(
        &mut self,
        unified_playlist_id: &str,
        remote_playlist_id: &str,
        intent: ListenBrainzSyncIntent,
    ) -> anyhow::Result<bool> {
        let previous = self.playlist_links.clone();
        let link = self
            .playlist_links
            .iter_mut()
            .find(|link| link.unified_playlist_id == unified_playlist_id)
            .ok_or_else(|| anyhow::anyhow!("unified playlist link not found"))?;
        anyhow::ensure!(
            link.listenbrainz_playlist_id.as_deref() == Some(remote_playlist_id),
            "ListenBrainz pending intent targets a different remote playlist"
        );
        let sync = link
            .listenbrainz_sync
            .as_mut()
            .ok_or_else(|| anyhow::anyhow!("ListenBrainz sync base is not initialized"))?;
        if !sync.begin_intent(intent)? {
            return Ok(false);
        }
        link.updated_at = unix_timestamp()?;
        if let Err(error) = self.save_unified_playlists() {
            self.playlist_links = previous;
            return Err(error);
        }
        Ok(true)
    }

    pub fn begin_listenbrainz_resolution_intent(
        &mut self,
        unified_playlist_id: &str,
        remote_playlist_id: &str,
        intent: ListenBrainzSyncIntent,
        verified_remote_fingerprint: &str,
    ) -> anyhow::Result<bool> {
        let previous = self.playlist_links.clone();
        let link = self
            .playlist_links
            .iter_mut()
            .find(|link| link.unified_playlist_id == unified_playlist_id)
            .ok_or_else(|| anyhow::anyhow!("unified playlist link not found"))?;
        anyhow::ensure!(
            link.listenbrainz_playlist_id.as_deref() == Some(remote_playlist_id),
            "ListenBrainz resolution targets a different remote playlist"
        );
        let sync = link
            .listenbrainz_sync
            .as_mut()
            .ok_or_else(|| anyhow::anyhow!("ListenBrainz sync base is not initialized"))?;
        if !sync.begin_resolution_intent(intent, verified_remote_fingerprint)? {
            return Ok(false);
        }
        link.updated_at = unix_timestamp()?;
        if let Err(error) = self.save_unified_playlists() {
            self.playlist_links = previous;
            return Err(error);
        }
        Ok(true)
    }

    pub fn recover_listenbrainz_sync_intent(
        &mut self,
        unified_playlist_id: &str,
        remote_playlist_id: &str,
        verified_remote: Option<ListenBrainzSyncBase>,
        observed_at: u64,
    ) -> anyhow::Result<ListenBrainzRecoveryDisposition> {
        let previous = self.playlist_links.clone();
        let link = self
            .playlist_links
            .iter_mut()
            .find(|link| link.unified_playlist_id == unified_playlist_id)
            .ok_or_else(|| anyhow::anyhow!("unified playlist link not found"))?;
        anyhow::ensure!(
            link.listenbrainz_playlist_id.as_deref() == Some(remote_playlist_id),
            "ListenBrainz recovery targets a different remote playlist"
        );
        let sync = link
            .listenbrainz_sync
            .as_mut()
            .ok_or_else(|| anyhow::anyhow!("ListenBrainz sync base is not initialized"))?;
        let disposition = sync.recover_pending_intent(verified_remote, observed_at)?;
        if disposition == ListenBrainzRecoveryDisposition::NoPendingIntent {
            return Ok(disposition);
        }
        link.updated_at = unix_timestamp()?;
        if let Err(error) = self.save_unified_playlists() {
            self.playlist_links = previous;
            return Err(error);
        }
        Ok(disposition)
    }

    pub fn record_listenbrainz_sync_outcome(
        &mut self,
        unified_playlist_id: &str,
        remote_playlist_id: &str,
        status: crate::state::ListenBrainzSyncStatus,
        disposition: ListenBrainzRecoveryDisposition,
        observed_at: u64,
    ) -> anyhow::Result<()> {
        let previous = self.playlist_links.clone();
        let link = self
            .playlist_links
            .iter_mut()
            .find(|link| link.unified_playlist_id == unified_playlist_id)
            .ok_or_else(|| anyhow::anyhow!("unified playlist link not found"))?;
        anyhow::ensure!(
            link.listenbrainz_playlist_id.as_deref() == Some(remote_playlist_id),
            "ListenBrainz outcome targets a different remote playlist"
        );
        let sync = link
            .listenbrainz_sync
            .as_mut()
            .ok_or_else(|| anyhow::anyhow!("ListenBrainz sync base is not initialized"))?;
        sync.record_pending_outcome(status, disposition, observed_at)?;
        link.updated_at = unix_timestamp()?;
        if let Err(error) = self.save_unified_playlists() {
            self.playlist_links = previous;
            return Err(error);
        }
        Ok(())
    }

    pub fn begin_listenbrainz_pull_apply(
        &mut self,
        unified_playlist_id: &str,
        remote_playlist_id: &str,
        operation_id: &str,
        captured_at: u64,
    ) -> anyhow::Result<bool> {
        anyhow::ensure!(
            !operation_id.trim().is_empty(),
            "ListenBrainz pull operation has no operation ID"
        );
        let playlist = self
            .unified_playlists
            .iter()
            .find(|playlist| playlist.id == unified_playlist_id)
            .ok_or_else(|| anyhow::anyhow!("unified playlist not found"))?
            .clone();
        let snapshot = ListenBrainzLocalApplySnapshot {
            operation_id: operation_id.to_owned(),
            snapshot_hash: playlist.snapshot_hash(),
            playlist,
            captured_at,
        };
        snapshot.validate()?;

        let previous = self.playlist_links.clone();
        let link = self
            .playlist_links
            .iter_mut()
            .find(|link| link.unified_playlist_id == unified_playlist_id)
            .ok_or_else(|| anyhow::anyhow!("unified playlist link not found"))?;
        anyhow::ensure!(
            link.listenbrainz_playlist_id.as_deref() == Some(remote_playlist_id),
            "ListenBrainz pull targets a different remote playlist"
        );
        let sync = link
            .listenbrainz_sync
            .as_mut()
            .ok_or_else(|| anyhow::anyhow!("ListenBrainz sync base is not initialized"))?;
        sync.validate()?;
        anyhow::ensure!(
            sync.pending_intent.is_none(),
            "a ListenBrainz remote write outcome is still pending"
        );
        if let Some(existing) = sync.local_apply_snapshot.as_ref() {
            if existing.operation_id == snapshot.operation_id
                && existing.snapshot_hash == snapshot.snapshot_hash
            {
                return Ok(false);
            }
            anyhow::ensure!(
                sync.status == crate::state::ListenBrainzSyncStatus::Clean,
                "a different ListenBrainz local apply snapshot requires rollback first"
            );
        }
        sync.local_apply_snapshot = Some(snapshot);
        sync.status = crate::state::ListenBrainzSyncStatus::Pending;
        link.updated_at = captured_at;
        if let Err(error) = self.save_unified_playlists() {
            self.playlist_links = previous;
            return Err(error);
        }
        Ok(true)
    }

    pub fn commit_listenbrainz_pull_apply(
        &mut self,
        unified_playlist_id: &str,
        remote_playlist_id: &str,
        operation_id: &str,
        mut replacement: UnifiedPlaylist,
        verified_base: ListenBrainzSyncBase,
    ) -> anyhow::Result<()> {
        replacement.normalize_entry_ids()?;
        verified_base.validate()?;
        anyhow::ensure!(
            replacement.id == unified_playlist_id
                && verified_base.unified_playlist_id == unified_playlist_id,
            "ListenBrainz pull apply targets another Unified playlist"
        );
        anyhow::ensure!(
            replacement.snapshot_hash() == verified_base.local_snapshot_hash,
            "ListenBrainz pull apply did not reproduce the verified remote snapshot"
        );
        let playlist_index = self
            .unified_playlists
            .iter()
            .position(|playlist| playlist.id == unified_playlist_id)
            .ok_or_else(|| anyhow::anyhow!("unified playlist not found"))?;
        let link_index = self
            .playlist_links
            .iter()
            .position(|link| link.unified_playlist_id == unified_playlist_id)
            .ok_or_else(|| anyhow::anyhow!("unified playlist link not found"))?;
        anyhow::ensure!(
            self.playlist_links[link_index]
                .listenbrainz_playlist_id
                .as_deref()
                == Some(remote_playlist_id),
            "ListenBrainz pull targets a different remote playlist"
        );
        let sync = self.playlist_links[link_index]
            .listenbrainz_sync
            .as_ref()
            .ok_or_else(|| anyhow::anyhow!("ListenBrainz sync base is not initialized"))?;
        let snapshot = sync
            .local_apply_snapshot
            .as_ref()
            .ok_or_else(|| anyhow::anyhow!("ListenBrainz pull apply has no pre-apply snapshot"))?;
        anyhow::ensure!(
            snapshot.operation_id == operation_id,
            "ListenBrainz pull apply operation ID does not match its snapshot"
        );
        anyhow::ensure!(
            self.unified_playlists[playlist_index].snapshot_hash() == snapshot.snapshot_hash,
            "local Unified playlist changed after the pull snapshot was saved"
        );

        let previous_playlists = self.unified_playlists.clone();
        let previous_links = self.playlist_links.clone();
        self.unified_playlists[playlist_index] = replacement;
        let link = &mut self.playlist_links[link_index];
        let sync = link.listenbrainz_sync.as_mut().expect("validated above");
        sync.base = verified_base;
        sync.status = crate::state::ListenBrainzSyncStatus::Clean;
        sync.recovery = None;
        link.updated_at = sync.base.verified_at;
        if let Err(error) = self.save_unified_playlists() {
            self.unified_playlists = previous_playlists;
            self.playlist_links = previous_links;
            return Err(error);
        }
        anyhow::ensure!(
            self.unified_playlists[playlist_index].snapshot_hash()
                == self.playlist_links[link_index]
                    .listenbrainz_sync
                    .as_ref()
                    .expect("sync state remains present")
                    .base
                    .local_snapshot_hash,
            "persisted ListenBrainz pull verification failed"
        );
        Ok(())
    }

    pub fn rollback_listenbrainz_pull_apply(
        &mut self,
        unified_playlist_id: &str,
        remote_playlist_id: &str,
    ) -> anyhow::Result<String> {
        let rolled_back_at = unix_timestamp()?;
        let playlist_index = self
            .unified_playlists
            .iter()
            .position(|playlist| playlist.id == unified_playlist_id)
            .ok_or_else(|| anyhow::anyhow!("unified playlist not found"))?;
        let link_index = self
            .playlist_links
            .iter()
            .position(|link| link.unified_playlist_id == unified_playlist_id)
            .ok_or_else(|| anyhow::anyhow!("unified playlist link not found"))?;
        anyhow::ensure!(
            self.playlist_links[link_index]
                .listenbrainz_playlist_id
                .as_deref()
                == Some(remote_playlist_id),
            "ListenBrainz rollback targets a different remote playlist"
        );
        let previous_playlists = self.unified_playlists.clone();
        let previous_links = self.playlist_links.clone();
        let sync = self.playlist_links[link_index]
            .listenbrainz_sync
            .as_mut()
            .ok_or_else(|| anyhow::anyhow!("ListenBrainz sync base is not initialized"))?;
        anyhow::ensure!(
            sync.pending_intent.is_none(),
            "a ListenBrainz remote write outcome is still pending"
        );
        let snapshot = sync
            .local_apply_snapshot
            .take()
            .ok_or_else(|| anyhow::anyhow!("no ListenBrainz local apply snapshot is available"))?;
        snapshot.validate()?;
        let operation_id = snapshot.operation_id.clone();
        self.unified_playlists[playlist_index] = snapshot.playlist;
        sync.status = if self.unified_playlists[playlist_index].snapshot_hash()
            == sync.base.local_snapshot_hash
        {
            crate::state::ListenBrainzSyncStatus::Clean
        } else {
            crate::state::ListenBrainzSyncStatus::Drifted
        };
        self.playlist_links[link_index].updated_at = rolled_back_at;
        if let Err(error) = self.save_unified_playlists() {
            self.unified_playlists = previous_playlists;
            self.playlist_links = previous_links;
            return Err(error);
        }
        Ok(operation_id)
    }

    /// Persist one account-scoped projection without replacing other provider
    /// targets or the legacy `ListenBrainz` link metadata.
    #[allow(dead_code)]
    pub fn upsert_playlist_projection(
        &mut self,
        unified_playlist_id: &str,
        projection: PlaylistProjectionState,
    ) -> anyhow::Result<()> {
        let previous = self.playlist_links.clone();
        let link = self
            .playlist_links
            .iter_mut()
            .find(|link| link.unified_playlist_id == unified_playlist_id)
            .ok_or_else(|| anyhow::anyhow!("unified playlist link not found"))?;
        link.upsert_projection(projection);
        link.updated_at = unix_timestamp()?;
        if let Err(error) = self.save_unified_playlists() {
            self.playlist_links = previous;
            return Err(error);
        }
        Ok(())
    }

    /// Mark every target for a provider detached before its account-owned
    /// caches are cleared.  Detached state is retained for inspection and is
    /// never rendered as clean.
    pub fn detach_provider_projections(&mut self, provider: Provider) -> anyhow::Result<usize> {
        let previous = self.playlist_links.clone();
        let mut changed = 0;
        for link in &mut self.playlist_links {
            for projection in &mut link.projections {
                if projection.target.provider == provider
                    && projection.status != PlaylistProjectionStatus::Detached
                {
                    projection.status = PlaylistProjectionStatus::Detached;
                    projection.conflicts.clear();
                    changed += 1;
                }
            }
        }
        if changed == 0 {
            return Ok(0);
        }
        if let Err(error) = self.save_unified_playlists() {
            self.playlist_links = previous;
            return Err(error);
        }
        Ok(changed)
    }

    #[allow(dead_code)]
    pub fn accept_projection_mapping(
        &mut self,
        unified_playlist_id: &str,
        provider: Provider,
        account_id: &str,
        account_epoch: u64,
        playlist_id: &str,
        mapping: PlaylistProjectionMapping,
    ) -> anyhow::Result<()> {
        let previous = self.playlist_links.clone();
        let link = self
            .playlist_links
            .iter_mut()
            .find(|link| link.unified_playlist_id == unified_playlist_id)
            .ok_or_else(|| anyhow::anyhow!("unified playlist link not found"))?;
        let projection = link
            .projection_for_mut(provider, account_id, account_epoch, playlist_id)
            .ok_or_else(|| anyhow::anyhow!("projection target not found"))?;
        if let Some(existing) = projection
            .mappings
            .iter_mut()
            .find(|existing| existing.local_entry_id == mapping.local_entry_id)
        {
            *existing = mapping;
        } else {
            projection.mappings.push(mapping);
        }
        // A newly accepted mapping changes the meaning of an otherwise
        // unchanged local-revision intent, so permit one fresh projection
        // attempt rather than treating the old acknowledgement as final.
        projection.acknowledged_intents.clear();
        if let Err(error) = self.save_unified_playlists() {
            self.playlist_links = previous;
            return Err(error);
        }
        Ok(())
    }

    /// Acknowledgement is persisted as an idempotency fence.  Repeating the
    /// same operation ID returns false and does not schedule another write.
    pub fn acknowledge_projection_operation(
        &mut self,
        unified_playlist_id: &str,
        provider: Provider,
        account_id: &str,
        account_epoch: u64,
        playlist_id: &str,
        operation_id: &str,
    ) -> anyhow::Result<bool> {
        let previous = self.playlist_links.clone();
        let link = self
            .playlist_links
            .iter_mut()
            .find(|link| link.unified_playlist_id == unified_playlist_id)
            .ok_or_else(|| anyhow::anyhow!("unified playlist link not found"))?;
        let projection = link
            .projection_for_mut(provider, account_id, account_epoch, playlist_id)
            .ok_or_else(|| anyhow::anyhow!("projection target not found"))?;
        if !projection.acknowledge_operation(operation_id.to_owned()) {
            return Ok(false);
        }
        if let Err(error) = self.save_unified_playlists() {
            self.playlist_links = previous;
            return Err(error);
        }
        Ok(true)
    }

    pub fn acknowledge_projection_intent(
        &mut self,
        unified_playlist_id: &str,
        provider: Provider,
        account_id: &str,
        account_epoch: u64,
        playlist_id: &str,
        intent_key: &str,
    ) -> anyhow::Result<bool> {
        let previous = self.playlist_links.clone();
        let link = self
            .playlist_links
            .iter_mut()
            .find(|link| link.unified_playlist_id == unified_playlist_id)
            .ok_or_else(|| anyhow::anyhow!("unified playlist link not found"))?;
        let projection = link
            .projection_for_mut(provider, account_id, account_epoch, playlist_id)
            .ok_or_else(|| anyhow::anyhow!("projection target not found"))?;
        if !projection.acknowledge_intent(intent_key.to_owned()) {
            return Ok(false);
        }
        if let Err(error) = self.save_unified_playlists() {
            self.playlist_links = previous;
            return Err(error);
        }
        Ok(true)
    }

    #[allow(dead_code)]
    pub fn set_projection_status(
        &mut self,
        unified_playlist_id: &str,
        provider: Provider,
        account_id: &str,
        account_epoch: u64,
        playlist_id: &str,
        status: PlaylistProjectionStatus,
    ) -> anyhow::Result<()> {
        let previous = self.playlist_links.clone();
        let link = self
            .playlist_links
            .iter_mut()
            .find(|link| link.unified_playlist_id == unified_playlist_id)
            .ok_or_else(|| anyhow::anyhow!("unified playlist link not found"))?;
        let projection = link
            .projection_for_mut(provider, account_id, account_epoch, playlist_id)
            .ok_or_else(|| anyhow::anyhow!("projection target not found"))?;
        projection.status = status;
        if status != PlaylistProjectionStatus::Clean {
            projection.acknowledged_intents.clear();
        }
        if matches!(
            status,
            PlaylistProjectionStatus::Clean
                | PlaylistProjectionStatus::Pending
                | PlaylistProjectionStatus::Detached
        ) {
            projection.conflicts.clear();
        }
        if let Err(error) = self.save_unified_playlists() {
            self.playlist_links = previous;
            return Err(error);
        }
        Ok(())
    }

    pub fn set_projection_recovery(
        &mut self,
        unified_playlist_id: &str,
        provider: Provider,
        account_id: &str,
        account_epoch: u64,
        playlist_id: &str,
        status: PlaylistProjectionStatus,
        recovery: PlaylistProjectionRecovery,
    ) -> anyhow::Result<()> {
        let previous = self.playlist_links.clone();
        let link = self
            .playlist_links
            .iter_mut()
            .find(|link| link.unified_playlist_id == unified_playlist_id)
            .ok_or_else(|| anyhow::anyhow!("unified playlist link not found"))?;
        let projection = link
            .projection_for_mut(provider, account_id, account_epoch, playlist_id)
            .ok_or_else(|| anyhow::anyhow!("projection target not found"))?;
        projection.status = status;
        projection.recovery = Some(recovery);
        projection.acknowledged_intents.clear();
        if let Err(error) = self.save_unified_playlists() {
            self.playlist_links = previous;
            return Err(error);
        }
        Ok(())
    }

    pub fn unified_playlist_projection_status(
        &self,
        unified_playlist_id: &str,
        account_id: &str,
        account_epoch: u64,
    ) -> Option<PlaylistProjectionStatus> {
        let playlist = self
            .unified_playlists
            .iter()
            .find(|playlist| playlist.id == unified_playlist_id)?;
        let link = self
            .playlist_links
            .iter()
            .find(|link| link.unified_playlist_id == unified_playlist_id)?;
        link.youtube_playlist_id.as_ref()?;
        link.projections
            .iter()
            .find(|projection| {
                projection.target.provider == Provider::YouTubeMusic
                    && projection.target.account_id == account_id
                    && projection.target.account_epoch == account_epoch
                    && Some(projection.target.playlist_id.as_str())
                        == link.youtube_playlist_id.as_deref()
            })
            .map(|projection| {
                if projection.is_clean_for(&playlist.snapshot_hash()) {
                    PlaylistProjectionStatus::Clean
                } else if projection.status == PlaylistProjectionStatus::Clean {
                    PlaylistProjectionStatus::Drifted
                } else {
                    projection.status
                }
            })
            .or(Some(PlaylistProjectionStatus::OutcomeUnknown))
    }

    /// Run and persist a provider-neutral dry run.  Provider adapters supply
    /// the deterministic remote rows; this state layer owns conflict status
    /// and leaves destructive reconciliation to an explicit UI confirmation.
    #[allow(dead_code)]
    pub fn apply_projection_dry_run(
        &mut self,
        unified_playlist_id: &str,
        provider: Provider,
        account_id: &str,
        account_epoch: u64,
        playlist_id: &str,
        local: &[UnifiedPlaylistItem],
        remote: &[UnifiedPlaylistItem],
        mappings: &[PlaylistProjectionMapping],
    ) -> anyhow::Result<crate::state::ProjectionConflictPlan> {
        let plan = crate::state::dry_run_projection(local, remote, mappings);
        let previous = self.playlist_links.clone();
        let link = self
            .playlist_links
            .iter_mut()
            .find(|link| link.unified_playlist_id == unified_playlist_id)
            .ok_or_else(|| anyhow::anyhow!("unified playlist link not found"))?;
        let projection = link
            .projection_for_mut(provider, account_id, account_epoch, playlist_id)
            .ok_or_else(|| anyhow::anyhow!("projection target not found"))?;
        projection.status = plan.status;
        projection.conflicts.clone_from(&plan.conflicts);
        if plan.status != PlaylistProjectionStatus::Clean {
            projection.acknowledged_intents.clear();
        }
        if let Err(error) = self.save_unified_playlists() {
            self.playlist_links = previous;
            return Err(error);
        }
        Ok(plan)
    }

    pub fn unified_playlist_projection_preview(
        &self,
        unified_playlist_id: &str,
        account_id: &str,
        account_epoch: u64,
    ) -> Option<String> {
        let link = self
            .playlist_links
            .iter()
            .find(|link| link.unified_playlist_id == unified_playlist_id)?;
        let target_id = link.youtube_playlist_id.as_deref()?;
        let projection = link.projections.iter().find(|projection| {
            projection.target.provider == Provider::YouTubeMusic
                && projection.target.account_id == account_id
                && projection.target.account_epoch == account_epoch
                && projection.target.playlist_id == target_id
        })?;
        Some(projection.preview_summary())
    }

    pub fn context_tracks(&self, id: &ContextId) -> Option<&Vec<Track>> {
        let c = self.caches.context.get(&id.uri())?;
        Some(match c {
            Context::Album { tracks, .. }
            | Context::Playlist { tracks, .. }
            | Context::Tracks { tracks, .. }
            | Context::Artist {
                top_tracks: tracks, ..
            } => tracks,
            Context::Show { .. } => {
                return None;
            }
        })
    }
}

fn load_unified_playlist_store(
    config_folder: &Path,
) -> anyhow::Result<Option<(Vec<UnifiedPlaylist>, Vec<PlaylistLink>)>> {
    let primary = config_folder.join(UNIFIED_PLAYLISTS_FILE);
    let backup = config_folder.join(UNIFIED_PLAYLISTS_BACKUP_FILE);
    if !primary.exists() && !backup.exists() {
        return Ok(None);
    }

    match read_unified_playlist_store_file(&primary) {
        Ok(store) => {
            persist_migrated_store(&primary, &primary, &backup, &store)?;
            Ok(Some(store))
        }
        Err(StoreFileError::NewerSchema(version)) => anyhow::bail!(
            "unified playlist schema {version} is newer than supported schema {UNIFIED_PLAYLISTS_SCHEMA_VERSION}"
        ),
        Err(StoreFileError::Unavailable(primary_error)) => {
            match read_unified_playlist_store_file(&backup) {
                Ok(store) => {
                    persist_migrated_store(&backup, &primary, &backup, &store)?;
                    crate::observability::log_safe_error!(
                        warn,
                        crate::observability::DiagnosticCode::UNIFIED_PLAYLIST_RECOVERY_USED,
                        crate::observability::ErrorCategory::Storage,
                        &primary_error,
                        "Recovered unified playlists from the backup after the primary store failed"
                    );
                    Ok(Some(store))
                }
                Err(StoreFileError::NewerSchema(version)) => anyhow::bail!(
                    "backup unified playlist schema {version} is newer than supported schema {UNIFIED_PLAYLISTS_SCHEMA_VERSION}"
                ),
                Err(StoreFileError::Unavailable(backup_error)) => Err(anyhow::anyhow!(
                    "failed to load primary unified playlists: {primary_error:#}; failed to load backup: {backup_error:#}"
                )),
            }
        }
    }
}

enum StoreFileError {
    Unavailable(anyhow::Error),
    NewerSchema(u32),
}

fn read_unified_playlist_store_file(
    path: &Path,
) -> Result<(Vec<UnifiedPlaylist>, Vec<PlaylistLink>), StoreFileError> {
    let file = std::fs::File::open(path).map_err(|err| {
        StoreFileError::Unavailable(
            anyhow::Error::new(err).context(format!("open {}", path.display())),
        )
    })?;
    let store = serde_json::from_reader(file).map_err(|err| {
        StoreFileError::Unavailable(
            anyhow::Error::new(err).context(format!("parse {}", path.display())),
        )
    })?;
    match store {
        UnifiedPlaylistStoreFile::Versioned(store) => {
            if store.schema_version > UNIFIED_PLAYLISTS_SCHEMA_VERSION {
                return Err(StoreFileError::NewerSchema(store.schema_version));
            }
            validate_listenbrainz_sync_links(&store.links).map_err(StoreFileError::Unavailable)?;
            Ok((
                migrate_playlists(store.playlists).map_err(StoreFileError::Unavailable)?,
                store.links,
            ))
        }
        UnifiedPlaylistStoreFile::Legacy(playlists) => Ok((
            migrate_playlists(playlists).map_err(StoreFileError::Unavailable)?,
            Vec::new(),
        )),
    }
}

fn validate_listenbrainz_sync_links(links: &[PlaylistLink]) -> anyhow::Result<()> {
    for link in links {
        let Some(sync) = &link.listenbrainz_sync else {
            continue;
        };
        anyhow::ensure!(
            link.listenbrainz_playlist_id.is_some(),
            "persisted ListenBrainz sync state has no remote playlist link"
        );
        anyhow::ensure!(
            sync.base.unified_playlist_id == link.unified_playlist_id,
            "persisted ListenBrainz sync base targets a different Unified playlist"
        );
        sync.validate()?;
    }
    Ok(())
}

fn migrate_playlists(mut playlists: Vec<UnifiedPlaylist>) -> anyhow::Result<Vec<UnifiedPlaylist>> {
    for playlist in &mut playlists {
        playlist.normalize_entry_ids()?;
    }
    Ok(playlists)
}

fn persist_migrated_store(
    source: &Path,
    primary: &Path,
    backup: &Path,
    store: &(Vec<UnifiedPlaylist>, Vec<PlaylistLink>),
) -> anyhow::Result<()> {
    let raw = std::fs::read(source).ok();
    let needs_migration = raw.as_deref().is_some_and(|bytes| {
        serde_json::from_slice::<serde_json::Value>(bytes)
            .ok()
            .and_then(|value| {
                value
                    .get("schema_version")
                    .and_then(serde_json::Value::as_u64)
                    .map(|version| version < u64::from(UNIFIED_PLAYLISTS_SCHEMA_VERSION))
                    .or_else(|| value.is_array().then_some(true))
            })
            .unwrap_or(false)
    });
    if needs_migration {
        if source == backup {
            // Keep the known-good v1 backup intact while replacing a broken
            // or absent primary with the normalized schema-v2 form.
            write_unified_playlists(primary, &store.0, &store.1)?;
        } else {
            if let Some(raw) = raw {
                atomic_write(backup, &raw)?;
            }
            write_unified_playlists(primary, &store.0, &store.1)?;
        }
    }
    Ok(())
}

fn write_unified_playlists(
    path: &Path,
    playlists: &[UnifiedPlaylist],
    links: &[PlaylistLink],
) -> anyhow::Result<()> {
    let playlists = migrate_playlists(playlists.to_vec())?;
    validate_listenbrainz_sync_links(links)?;
    let data = serde_json::to_vec_pretty(&UnifiedPlaylistStore {
        schema_version: UNIFIED_PLAYLISTS_SCHEMA_VERSION,
        playlists,
        links: links.to_vec(),
    })?;
    atomic_write(path, &data)
}

#[allow(dead_code)]
fn unix_timestamp() -> anyhow::Result<u64> {
    Ok(std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)?
        .as_secs())
}

fn atomic_write(path: &Path, data: &[u8]) -> anyhow::Result<()> {
    atomicwrites::AtomicFile::new(path, atomicwrites::AllowOverwrite)
        .write(|file| file.write_all(data))?;
    Ok(())
}

impl UserData {
    /// Construct a new user data based on file caches
    pub fn new_from_file_caches(cache_folder: &Path) -> Self {
        Self {
            user: None,
            playlists: load_data_from_file_cache(FileCacheKey::Playlists, cache_folder)
                .unwrap_or_default(),
            playlist_folder_node: load_data_from_file_cache(
                FileCacheKey::PlaylistFolders,
                cache_folder,
            ),
            followed_artists: load_data_from_file_cache(
                FileCacheKey::FollowedArtists,
                cache_folder,
            )
            .unwrap_or_default(),
            saved_shows: load_data_from_file_cache(FileCacheKey::SavedShows, cache_folder)
                .unwrap_or_default(),
            saved_albums: load_data_from_file_cache(FileCacheKey::SavedAlbums, cache_folder)
                .unwrap_or_default(),
            saved_tracks: load_data_from_file_cache(FileCacheKey::SavedTracks, cache_folder)
                .unwrap_or_default(),
            youtube_library: YouTubeLibrary::default(),
        }
    }

    /// Get a list of playlist items that are **possibly** modifiable by user
    ///
    /// If `folder_id` is provided, returns items in the given folder id.
    /// Otherwise, returns the all items.
    pub fn modifiable_playlist_items(&self, folder_id: Option<usize>) -> Vec<&PlaylistFolderItem> {
        match self.user {
            None => vec![],
            Some(ref u) => self
                .playlists
                .iter()
                // filter items in a folder (if specified)
                .filter(|item| {
                    if let Some(folder_id) = folder_id {
                        match item {
                            PlaylistFolderItem::Playlist(p) => p.current_folder_id == folder_id,
                            PlaylistFolderItem::Folder(f) => f.current_id == folder_id,
                        }
                    } else {
                        true
                    }
                })
                // filter modifiable items
                .filter(|item| match item {
                    PlaylistFolderItem::Playlist(p) => p.owner.1 == u.id || p.collaborative,
                    PlaylistFolderItem::Folder(_) => true,
                })
                .collect(),
        }
    }

    /// Get playlists items for the given folder id
    pub fn folder_playlists_items(&self, folder_id: usize) -> Vec<&PlaylistFolderItem> {
        self.playlists
            .iter()
            .filter(|item| match item {
                PlaylistFolderItem::Playlist(p) => p.current_folder_id == folder_id,
                PlaylistFolderItem::Folder(f) => f.current_id == folder_id,
            })
            .collect()
    }

    /// Check if a track is a liked track
    pub fn is_liked_track(&self, track: &Track) -> bool {
        self.saved_tracks.contains_key(&track.id.uri())
    }

    /// Get the user's liked tracks by the given artist, sorted by liked date (newest first)
    pub fn liked_tracks_by_artist(&self, artist: &Artist) -> Vec<Track> {
        let mut tracks: Vec<Track> = self
            .saved_tracks
            .values()
            .filter(|t| t.artists.iter().any(|a| a.id == artist.id))
            .cloned()
            .collect();
        tracks.sort_by_key(|t| std::cmp::Reverse(t.added_at));
        tracks
    }
}

pub fn store_data_into_file_cache<T: Serialize>(
    key: FileCacheKey,
    cache_folder: &Path,
    data: &T,
) -> std::io::Result<()> {
    let path = cache_folder.join(format!("{key:?}_cache.json"));
    let f = BufWriter::new(std::fs::File::create(path)?);
    serde_json::to_writer(f, data)?;
    Ok(())
}

pub fn load_data_from_file_cache<T>(key: FileCacheKey, cache_folder: &Path) -> Option<T>
where
    T: DeserializeOwned,
{
    let path = cache_folder.join(format!("{key:?}_cache.json"));
    if path.exists() {
        tracing::info!(cache_kind = ?key, "Loading cached application data");
        let f = BufReader::new(std::fs::File::open(path).expect("path exists"));
        match serde_json::from_reader(f) {
            Ok(data) => {
                tracing::info!("Successfully loaded {key:?} data!");
                Some(data)
            }
            Err(err) => {
                crate::observability::log_safe_error!(
                    error,
                    crate::observability::DiagnosticCode::DATA_CACHE_DECODE_FAILED,
                    crate::observability::ErrorCategory::Decode,
                    &err,
                    "Failed to decode cached application data"
                );
                None
            }
        }
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::state::{
        ListenBrainzProjectionStatus, ListenBrainzSyncBase, ListenBrainzSyncBaseEntry,
        ListenBrainzSyncState, ListenBrainzSyncStatus, MediaId, MediaKind,
        PlaylistProjectionTarget, Provider,
    };

    fn playlist() -> UnifiedPlaylist {
        UnifiedPlaylist {
            id: "local-1".to_string(),
            name: "Mixed".to_string(),
            items: Vec::new(),
            updated_at: 1,
            next_entry_id: 1,
        }
    }

    #[test]
    fn lyrics_cache_keys_distinguish_selected_sources_for_one_track() {
        let default = LyricsCacheKey::new("spotify:track:one", None);
        let lrclib = LyricsCacheKey::new("spotify:track:one", Some("lrclib"));
        let lyrics_ovh = LyricsCacheKey::new("spotify:track:one", Some("lyricsovh"));

        assert_ne!(default, lrclib);
        assert_ne!(lrclib, lyrics_ovh);
        assert_eq!(default.track_uri, lrclib.track_uri);
    }

    fn temporary_folder(name: &str) -> PathBuf {
        std::env::temp_dir().join(format!(
            "unified-player-{name}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ))
    }

    fn listenbrainz_sync_state(status: ListenBrainzSyncStatus) -> ListenBrainzSyncState {
        let mut state = ListenBrainzSyncState::verified(ListenBrainzSyncBase {
            manifest_schema_version: 2,
            unified_playlist_id: "local-1".to_owned(),
            playlist_name: "Mixed".to_owned(),
            local_snapshot_hash: "1".repeat(64),
            canonical_manifest_hash: "2".repeat(64),
            remote_fingerprint: "3".repeat(64),
            entries: vec![ListenBrainzSyncBaseEntry {
                occurrence: PlaylistEntryId(7),
                media_id: MediaId {
                    provider: Provider::Spotify,
                    kind: MediaKind::Track,
                    raw_id: "track-1".to_owned(),
                },
                projection_status: ListenBrainzProjectionStatus::Resolved,
                recording_mbid: Some("12345678-1234-1234-1234-123456789abc".to_owned()),
            }],
            verified_at: 11,
        })
        .unwrap();
        state.status = status;
        state
    }

    fn listenbrainz_intent() -> ListenBrainzSyncIntent {
        ListenBrainzSyncIntent {
            operation_id: "operation-1".to_owned(),
            expected_remote_fingerprint: "3".repeat(64),
            target_manifest_hash: "4".repeat(64),
            target_local_snapshot_hash: "5".repeat(64),
            started_at: 12,
        }
    }

    #[test]
    fn listenbrainz_resolution_cache_is_keyed_only_by_stable_recording_mbid() {
        let first = listenbrainz_recording_key("recording-mbid");
        let second = listenbrainz_recording_key("recording-mbid");
        assert_eq!(first, second);
        assert!(!first.contains("Track title"));

        let mut caches = MemoryCaches::new();
        caches.listenbrainz_recordings.insert(
            first.clone(),
            ListenBrainzRecordingResolution::Resolving { request_id: 9 },
            *TTL_CACHE_DURATION,
        );
        assert!(matches!(
            caches.listenbrainz_recordings.get(&second),
            Some(ListenBrainzRecordingResolution::Resolving { request_id: 9 })
        ));
    }

    #[test]
    fn listenbrainz_album_cache_is_keyed_only_by_stable_release_group_mbid() {
        let first = listenbrainz_album_key("release-group-mbid");
        let second = listenbrainz_album_key("release-group-mbid");
        assert_eq!(first, second);
        assert!(!first.contains("Album title"));

        let mut caches = MemoryCaches::new();
        caches.listenbrainz_albums.insert(
            first,
            ListenBrainzAlbumResolution::Resolving { request_id: 11 },
            *TTL_CACHE_DURATION,
        );
        assert!(matches!(
            caches.listenbrainz_albums.get(&second),
            Some(ListenBrainzAlbumResolution::Resolving { request_id: 11 })
        ));
    }

    #[test]
    fn unified_playlist_store_loads_legacy_array() {
        let json = serde_json::to_string(&vec![playlist()]).unwrap();
        let parsed = serde_json::from_str::<UnifiedPlaylistStoreFile>(&json).unwrap();
        assert!(matches!(parsed, UnifiedPlaylistStoreFile::Legacy(items) if items.len() == 1));
    }

    #[test]
    fn unified_playlist_store_round_trips_versioned_envelope() {
        let store = UnifiedPlaylistStore {
            schema_version: UNIFIED_PLAYLISTS_SCHEMA_VERSION,
            playlists: vec![playlist()],
            links: vec![PlaylistLink {
                unified_playlist_id: "local-1".to_string(),
                spotify_playlist_id: Some("spotify-1".to_string()),
                ..PlaylistLink::default()
            }],
        };
        let json = serde_json::to_string(&store).unwrap();
        let parsed = serde_json::from_str::<UnifiedPlaylistStoreFile>(&json).unwrap();
        assert!(matches!(
            parsed,
            UnifiedPlaylistStoreFile::Versioned(store)
                if store.schema_version == UNIFIED_PLAYLISTS_SCHEMA_VERSION
                    && store.playlists.len() == 1
                    && store.links.len() == 1
        ));
    }

    #[test]
    fn listenbrainz_link_and_dry_run_fields_survive_portable_round_trip() {
        let store = UnifiedPlaylistStore {
            schema_version: UNIFIED_PLAYLISTS_SCHEMA_VERSION,
            playlists: vec![playlist()],
            links: vec![PlaylistLink {
                unified_playlist_id: "local-1".to_owned(),
                listenbrainz_playlist_id: Some("lb-1".to_owned()),
                last_local_snapshot: Some("local-revision".to_owned()),
                last_youtube_snapshot: Some("remote-revision".to_owned()),
                ..PlaylistLink::default()
            }],
        };
        let parsed = serde_json::from_str::<UnifiedPlaylistStoreFile>(
            &serde_json::to_string(&store).unwrap(),
        )
        .unwrap();
        let UnifiedPlaylistStoreFile::Versioned(parsed) = parsed else {
            panic!("expected versioned store");
        };
        assert_eq!(
            parsed.links[0].listenbrainz_playlist_id.as_deref(),
            Some("lb-1")
        );
        assert_eq!(
            parsed.links[0].last_local_snapshot.as_deref(),
            Some("local-revision")
        );
        assert_eq!(
            parsed.links[0].last_youtube_snapshot.as_deref(),
            Some("remote-revision")
        );
    }

    #[test]
    fn listenbrainz_sync_base_and_statuses_survive_restart() {
        for status in [
            ListenBrainzSyncStatus::Pending,
            ListenBrainzSyncStatus::Clean,
            ListenBrainzSyncStatus::Drifted,
            ListenBrainzSyncStatus::Conflict,
            ListenBrainzSyncStatus::Partial,
            ListenBrainzSyncStatus::OutcomeUnknown,
            ListenBrainzSyncStatus::Detached,
        ] {
            let folder = temporary_folder(&format!("listenbrainz-sync-base-{status:?}"));
            std::fs::create_dir_all(&folder).unwrap();
            let mut data = AppData::new(&folder, &folder);
            data.upsert_unified_playlist(playlist()).unwrap();
            data.upsert_playlist_link(PlaylistLink {
                unified_playlist_id: "local-1".to_owned(),
                listenbrainz_playlist_id: Some("remote-1".to_owned()),
                ..PlaylistLink::default()
            })
            .unwrap();
            data.store_verified_listenbrainz_base(
                "local-1",
                "remote-1",
                listenbrainz_sync_state(status),
            )
            .unwrap();

            let restarted = AppData::new(&folder, &folder);
            let sync = restarted.playlist_links[0]
                .listenbrainz_sync
                .as_ref()
                .unwrap();
            assert_eq!(sync.status, status);
            assert_eq!(sync.base.entries[0].occurrence, PlaylistEntryId(7));
            assert_eq!(sync.base.canonical_manifest_hash, "2".repeat(64));
            assert_eq!(restarted.unified_playlists, vec![playlist()]);
            std::fs::remove_dir_all(folder).unwrap();
        }
    }

    #[test]
    fn legacy_listenbrainz_link_migrates_without_inventing_a_base() {
        let folder = temporary_folder("listenbrainz-sync-base-migration");
        std::fs::create_dir_all(&folder).unwrap();
        let legacy = serde_json::json!({
            "schema_version": 2,
            "playlists": [playlist()],
            "links": [{
                "unified_playlist_id": "local-1",
                "listenbrainz_playlist_id": "remote-1",
                "spotify_playlist_id": null,
                "youtube_playlist_id": null,
                "last_local_snapshot": null,
                "last_youtube_snapshot": null,
                "projections": [],
                "updated_at": 1
            }]
        });
        std::fs::write(
            folder.join(UNIFIED_PLAYLISTS_FILE),
            serde_json::to_vec(&legacy).unwrap(),
        )
        .unwrap();

        let restarted = AppData::new(&folder, &folder);
        assert_eq!(restarted.playlist_links.len(), 1);
        assert!(restarted.playlist_links[0].listenbrainz_sync.is_none());
        let persisted: serde_json::Value =
            serde_json::from_slice(&std::fs::read(folder.join(UNIFIED_PLAYLISTS_FILE)).unwrap())
                .unwrap();
        assert_eq!(
            persisted["schema_version"],
            UNIFIED_PLAYLISTS_SCHEMA_VERSION
        );
        assert!(persisted["links"][0]["listenbrainz_sync"].is_null());
        std::fs::remove_dir_all(folder).unwrap();
    }

    #[test]
    fn unsupported_persisted_listenbrainz_sync_schema_fails_closed() {
        let folder = temporary_folder("listenbrainz-sync-base-future-schema");
        std::fs::create_dir_all(&folder).unwrap();
        let mut link = PlaylistLink {
            unified_playlist_id: "local-1".to_owned(),
            listenbrainz_playlist_id: Some("remote-1".to_owned()),
            listenbrainz_sync: Some(listenbrainz_sync_state(ListenBrainzSyncStatus::Clean)),
            ..PlaylistLink::default()
        };
        link.listenbrainz_sync.as_mut().unwrap().schema_version += 1;
        let store = UnifiedPlaylistStore {
            schema_version: UNIFIED_PLAYLISTS_SCHEMA_VERSION,
            playlists: vec![playlist()],
            links: vec![link],
        };
        std::fs::write(
            folder.join(UNIFIED_PLAYLISTS_FILE),
            serde_json::to_vec(&store).unwrap(),
        )
        .unwrap();

        assert!(load_unified_playlist_store(&folder)
            .unwrap_err()
            .to_string()
            .contains("unsupported ListenBrainz sync-state schema"));
        std::fs::remove_dir_all(folder).unwrap();
    }

    #[test]
    fn storing_listenbrainz_base_rolls_back_when_persistence_fails() {
        let folder = temporary_folder("listenbrainz-sync-base-rollback");
        let mut data = AppData::new(&folder, &folder);
        data.playlist_links.push(PlaylistLink {
            unified_playlist_id: "local-1".to_owned(),
            listenbrainz_playlist_id: Some("remote-1".to_owned()),
            ..PlaylistLink::default()
        });
        data.unified_playlist_store_writable = false;

        assert!(data
            .store_verified_listenbrainz_base(
                "local-1",
                "remote-1",
                listenbrainz_sync_state(ListenBrainzSyncStatus::Clean),
            )
            .is_err());
        assert!(data.playlist_links[0].listenbrainz_sync.is_none());
    }

    #[test]
    fn pending_listenbrainz_intent_survives_restart_and_is_idempotent() {
        let folder = temporary_folder("listenbrainz-pending-intent-restart");
        std::fs::create_dir_all(&folder).unwrap();
        let mut data = AppData::new(&folder, &folder);
        data.upsert_unified_playlist(playlist()).unwrap();
        data.upsert_playlist_link(PlaylistLink {
            unified_playlist_id: "local-1".to_owned(),
            listenbrainz_playlist_id: Some("remote-1".to_owned()),
            listenbrainz_sync: Some(listenbrainz_sync_state(ListenBrainzSyncStatus::Clean)),
            ..PlaylistLink::default()
        })
        .unwrap();

        assert!(data
            .begin_listenbrainz_sync_intent("local-1", "remote-1", listenbrainz_intent())
            .unwrap());
        assert!(!data
            .begin_listenbrainz_sync_intent("local-1", "remote-1", listenbrainz_intent())
            .unwrap());

        let restarted = AppData::new(&folder, &folder);
        let sync = restarted.playlist_links[0]
            .listenbrainz_sync
            .as_ref()
            .unwrap();
        assert_eq!(sync.status, ListenBrainzSyncStatus::Pending);
        assert_eq!(
            sync.pending_intent.as_ref().unwrap().operation_id,
            "operation-1"
        );
        assert_eq!(restarted.unified_playlists, vec![playlist()]);
        std::fs::remove_dir_all(folder).unwrap();
    }

    fn pull_apply_data(folder: &Path) -> AppData {
        std::fs::create_dir_all(folder).unwrap();
        let mut data = AppData::new(folder, folder);
        let local = playlist();
        let base = ListenBrainzSyncBase {
            manifest_schema_version: 2,
            unified_playlist_id: local.id.clone(),
            playlist_name: local.name.clone(),
            local_snapshot_hash: local.snapshot_hash(),
            canonical_manifest_hash: "2".repeat(64),
            remote_fingerprint: "3".repeat(64),
            entries: Vec::new(),
            verified_at: 11,
        };
        data.upsert_unified_playlist(local).unwrap();
        data.upsert_playlist_link(PlaylistLink {
            unified_playlist_id: "local-1".to_owned(),
            listenbrainz_playlist_id: Some("remote-1".to_owned()),
            listenbrainz_sync: Some(ListenBrainzSyncState::verified(base).unwrap()),
            ..PlaylistLink::default()
        })
        .unwrap();
        data
    }

    fn pulled_playlist_and_base() -> (UnifiedPlaylist, ListenBrainzSyncBase) {
        let pulled = UnifiedPlaylist {
            id: "local-1".to_owned(),
            name: "Remote".to_owned(),
            items: vec![UnifiedPlaylistItem {
                entry_id: PlaylistEntryId(7),
                media_id: MediaId {
                    provider: Provider::Spotify,
                    kind: MediaKind::Track,
                    raw_id: "track-1".to_owned(),
                },
                ..UnifiedPlaylistItem::default()
            }],
            updated_at: 20,
            next_entry_id: 8,
        };
        let base = ListenBrainzSyncBase {
            manifest_schema_version: 2,
            unified_playlist_id: pulled.id.clone(),
            playlist_name: pulled.name.clone(),
            local_snapshot_hash: pulled.snapshot_hash(),
            canonical_manifest_hash: "4".repeat(64),
            remote_fingerprint: "5".repeat(64),
            entries: vec![ListenBrainzSyncBaseEntry {
                occurrence: PlaylistEntryId(7),
                media_id: pulled.items[0].media_id.clone(),
                projection_status: ListenBrainzProjectionStatus::Resolved,
                recording_mbid: Some("12345678-1234-1234-1234-123456789abc".to_owned()),
            }],
            verified_at: 20,
        };
        (pulled, base)
    }

    #[test]
    fn listenbrainz_pull_snapshot_survives_restart_before_local_mutation() {
        let folder = temporary_folder("listenbrainz-pull-snapshot-restart");
        let mut data = pull_apply_data(&folder);

        assert!(data
            .begin_listenbrainz_pull_apply("local-1", "remote-1", "pull-1", 19)
            .unwrap());

        let restarted = AppData::new(&folder, &folder);
        let sync = restarted.playlist_links[0]
            .listenbrainz_sync
            .as_ref()
            .unwrap();
        assert_eq!(sync.status, ListenBrainzSyncStatus::Pending);
        assert_eq!(
            sync.local_apply_snapshot.as_ref().unwrap().operation_id,
            "pull-1"
        );
        assert_eq!(restarted.unified_playlists[0], playlist());
        std::fs::remove_dir_all(folder).unwrap();
    }

    #[test]
    fn verified_pull_commit_advances_base_and_keeps_one_step_rollback() {
        let folder = temporary_folder("listenbrainz-pull-commit-rollback");
        let mut data = pull_apply_data(&folder);
        let original = data.unified_playlists[0].clone();
        let (pulled, verified_base) = pulled_playlist_and_base();
        data.begin_listenbrainz_pull_apply("local-1", "remote-1", "pull-1", 19)
            .unwrap();

        data.commit_listenbrainz_pull_apply(
            "local-1",
            "remote-1",
            "pull-1",
            pulled.clone(),
            verified_base.clone(),
        )
        .unwrap();

        assert_eq!(data.unified_playlists[0], pulled);
        let sync = data.playlist_links[0].listenbrainz_sync.as_ref().unwrap();
        assert_eq!(sync.status, ListenBrainzSyncStatus::Clean);
        assert_eq!(sync.base, verified_base);
        assert!(sync.local_apply_snapshot.is_some());
        assert_eq!(
            data.rollback_listenbrainz_pull_apply("local-1", "remote-1")
                .unwrap(),
            "pull-1"
        );
        assert_eq!(data.unified_playlists[0], original);
        let sync = data.playlist_links[0].listenbrainz_sync.as_ref().unwrap();
        assert_eq!(sync.status, ListenBrainzSyncStatus::Drifted);
        assert!(sync.local_apply_snapshot.is_none());
        std::fs::remove_dir_all(folder).unwrap();
    }

    #[test]
    fn failed_pull_commit_restores_memory_and_leaves_durable_snapshot() {
        let folder = temporary_folder("listenbrainz-pull-commit-failure");
        let mut data = pull_apply_data(&folder);
        let original = data.unified_playlists[0].clone();
        let (pulled, verified_base) = pulled_playlist_and_base();
        data.begin_listenbrainz_pull_apply("local-1", "remote-1", "pull-1", 19)
            .unwrap();
        data.unified_playlist_store_writable = false;

        assert!(data
            .commit_listenbrainz_pull_apply("local-1", "remote-1", "pull-1", pulled, verified_base,)
            .is_err());

        assert_eq!(data.unified_playlists[0], original);
        let sync = data.playlist_links[0].listenbrainz_sync.as_ref().unwrap();
        assert_eq!(sync.status, ListenBrainzSyncStatus::Pending);
        assert!(sync.local_apply_snapshot.is_some());
        let restarted = AppData::new(&folder, &folder);
        assert!(restarted.playlist_links[0]
            .listenbrainz_sync
            .as_ref()
            .unwrap()
            .local_apply_snapshot
            .is_some());
        std::fs::remove_dir_all(folder).unwrap();
    }

    #[test]
    fn base_initialization_cannot_overwrite_an_existing_pending_state() {
        let folder = temporary_folder("listenbrainz-base-no-overwrite");
        std::fs::create_dir_all(&folder).unwrap();
        let mut data = AppData::new(&folder, &folder);
        data.upsert_unified_playlist(playlist()).unwrap();
        let mut existing = listenbrainz_sync_state(ListenBrainzSyncStatus::Clean);
        existing.begin_intent(listenbrainz_intent()).unwrap();
        data.upsert_playlist_link(PlaylistLink {
            unified_playlist_id: "local-1".to_owned(),
            listenbrainz_playlist_id: Some("remote-1".to_owned()),
            listenbrainz_sync: Some(existing.clone()),
            ..PlaylistLink::default()
        })
        .unwrap();

        assert!(data
            .store_verified_listenbrainz_base(
                "local-1",
                "remote-1",
                listenbrainz_sync_state(ListenBrainzSyncStatus::Clean),
            )
            .is_err());
        assert_eq!(
            data.playlist_links[0].listenbrainz_sync.as_ref(),
            Some(&existing)
        );
        std::fs::remove_dir_all(folder).unwrap();
    }

    #[test]
    fn verified_target_readback_finalizes_pending_intent_once() {
        let folder = temporary_folder("listenbrainz-pending-intent-applied");
        std::fs::create_dir_all(&folder).unwrap();
        let mut data = AppData::new(&folder, &folder);
        data.upsert_unified_playlist(playlist()).unwrap();
        let sync = listenbrainz_sync_state(ListenBrainzSyncStatus::Clean);
        data.upsert_playlist_link(PlaylistLink {
            unified_playlist_id: "local-1".to_owned(),
            listenbrainz_playlist_id: Some("remote-1".to_owned()),
            listenbrainz_sync: Some(sync.clone()),
            ..PlaylistLink::default()
        })
        .unwrap();
        data.begin_listenbrainz_sync_intent("local-1", "remote-1", listenbrainz_intent())
            .unwrap();
        let mut verified_target = sync.base;
        verified_target.canonical_manifest_hash = "4".repeat(64);
        verified_target.local_snapshot_hash = "5".repeat(64);
        verified_target.remote_fingerprint = "6".repeat(64);
        verified_target.verified_at = 20;

        assert_eq!(
            data.recover_listenbrainz_sync_intent(
                "local-1",
                "remote-1",
                Some(verified_target.clone()),
                21,
            )
            .unwrap(),
            ListenBrainzRecoveryDisposition::AlreadyApplied
        );
        assert_eq!(
            data.recover_listenbrainz_sync_intent(
                "local-1",
                "remote-1",
                Some(verified_target.clone()),
                22,
            )
            .unwrap(),
            ListenBrainzRecoveryDisposition::NoPendingIntent
        );
        let restarted = AppData::new(&folder, &folder);
        let recovered = restarted.playlist_links[0]
            .listenbrainz_sync
            .as_ref()
            .unwrap();
        assert_eq!(recovered.status, ListenBrainzSyncStatus::Clean);
        assert!(recovered.pending_intent.is_none());
        assert_eq!(recovered.base, verified_target);
        assert_eq!(
            recovered.recovery.as_ref().unwrap().disposition,
            ListenBrainzRecoveryDisposition::AlreadyApplied
        );
        std::fs::remove_dir_all(folder).unwrap();
    }

    #[test]
    fn pending_recovery_never_retries_ambiguous_or_different_remote_state() {
        for (verified_remote, expected) in [
            (None, ListenBrainzRecoveryDisposition::OutcomeUnknown),
            (
                Some({
                    let mut remote = listenbrainz_sync_state(ListenBrainzSyncStatus::Clean).base;
                    remote.canonical_manifest_hash = "7".repeat(64);
                    remote
                }),
                ListenBrainzRecoveryDisposition::OutcomeUnknown,
            ),
            (
                Some({
                    let mut remote = listenbrainz_sync_state(ListenBrainzSyncStatus::Clean).base;
                    remote.canonical_manifest_hash = "7".repeat(64);
                    remote.remote_fingerprint = "8".repeat(64);
                    remote
                }),
                ListenBrainzRecoveryDisposition::Conflict,
            ),
        ] {
            let mut sync = listenbrainz_sync_state(ListenBrainzSyncStatus::Clean);
            assert!(sync.begin_intent(listenbrainz_intent()).unwrap());
            assert_eq!(
                sync.recover_pending_intent(verified_remote, 30).unwrap(),
                expected
            );
            assert!(sync.pending_intent.is_some());
            assert!(matches!(
                sync.status,
                ListenBrainzSyncStatus::OutcomeUnknown | ListenBrainzSyncStatus::Conflict
            ));
            assert!(sync
                .begin_intent(ListenBrainzSyncIntent {
                    operation_id: "automatic-retry".to_owned(),
                    ..listenbrainz_intent()
                })
                .is_err());
        }
    }

    #[test]
    fn pending_intent_and_recovery_roll_back_when_persistence_fails() {
        let folder = temporary_folder("listenbrainz-pending-intent-rollback");
        let mut data = AppData::new(&folder, &folder);
        data.playlist_links.push(PlaylistLink {
            unified_playlist_id: "local-1".to_owned(),
            listenbrainz_playlist_id: Some("remote-1".to_owned()),
            listenbrainz_sync: Some(listenbrainz_sync_state(ListenBrainzSyncStatus::Clean)),
            ..PlaylistLink::default()
        });
        data.unified_playlist_store_writable = false;

        assert!(data
            .begin_listenbrainz_sync_intent("local-1", "remote-1", listenbrainz_intent())
            .is_err());
        assert!(data.playlist_links[0]
            .listenbrainz_sync
            .as_ref()
            .unwrap()
            .pending_intent
            .is_none());

        let sync = data.playlist_links[0].listenbrainz_sync.as_mut().unwrap();
        sync.begin_intent(listenbrainz_intent()).unwrap();
        let mut verified_target = sync.base.clone();
        verified_target.canonical_manifest_hash = "4".repeat(64);
        verified_target.local_snapshot_hash = "5".repeat(64);
        verified_target.remote_fingerprint = "6".repeat(64);
        assert!(data
            .recover_listenbrainz_sync_intent("local-1", "remote-1", Some(verified_target), 99,)
            .is_err());
        let sync = data.playlist_links[0].listenbrainz_sync.as_ref().unwrap();
        assert_eq!(sync.status, ListenBrainzSyncStatus::Pending);
        assert!(sync.pending_intent.is_some());
        assert!(sync.recovery.is_none());
    }

    #[test]
    fn account_scoped_projection_and_mapping_survive_restart() {
        let folder = temporary_folder("projection-restart");
        std::fs::create_dir_all(&folder).unwrap();
        let mut data = AppData::new(&folder, &folder);
        data.upsert_unified_playlist(playlist()).unwrap();
        data.upsert_playlist_link(PlaylistLink {
            unified_playlist_id: "local-1".to_owned(),
            youtube_playlist_id: Some("remote".to_owned()),
            ..PlaylistLink::default()
        })
        .unwrap();
        data.upsert_playlist_projection(
            "local-1",
            PlaylistProjectionState::pending(PlaylistProjectionTarget {
                provider: Provider::YouTubeMusic,
                account_id: "account-1".to_owned(),
                account_epoch: 4,
                playlist_id: "remote".to_owned(),
            }),
        )
        .unwrap();
        data.accept_projection_mapping(
            "local-1",
            Provider::YouTubeMusic,
            "account-1",
            4,
            "remote",
            PlaylistProjectionMapping {
                local_entry_id: PlaylistEntryId(1),
                remote_media_id: MediaId {
                    provider: Provider::YouTubeMusic,
                    kind: MediaKind::Track,
                    raw_id: "video-1".to_owned(),
                },
                remote_occurrence_token: None,
                accepted_at: 9,
            },
        )
        .unwrap();
        let dry_run = data
            .apply_projection_dry_run(
                "local-1",
                Provider::YouTubeMusic,
                "account-1",
                4,
                "remote",
                &[],
                &[UnifiedPlaylistItem {
                    media_id: MediaId {
                        provider: Provider::YouTubeMusic,
                        kind: MediaKind::Track,
                        raw_id: "video-2".to_owned(),
                    },
                    ..UnifiedPlaylistItem::default()
                }],
                &[],
            )
            .unwrap();
        assert_eq!(dry_run.status, PlaylistProjectionStatus::Conflict);
        let restarted = AppData::new(&folder, &folder);
        let projection = &restarted.playlist_links[0].projections[0];
        assert_eq!(projection.target.account_id, "account-1");
        assert_eq!(projection.target.account_epoch, 4);
        assert_eq!(projection.mappings.len(), 1);
        std::fs::remove_dir_all(folder).unwrap();
    }

    #[test]
    fn unified_playlist_store_recovers_from_valid_backup() {
        let folder = temporary_folder("unified-playlist-recovery");
        std::fs::create_dir_all(&folder).unwrap();
        write_unified_playlists(
            &folder.join(UNIFIED_PLAYLISTS_BACKUP_FILE),
            &[playlist()],
            &[],
        )
        .unwrap();
        std::fs::write(folder.join(UNIFIED_PLAYLISTS_FILE), b"{truncated").unwrap();

        let (playlists, links) = load_unified_playlist_store(&folder).unwrap().unwrap();
        assert_eq!(playlists, vec![playlist()]);
        assert!(links.is_empty());

        std::fs::remove_dir_all(folder).unwrap();
    }

    #[test]
    fn schema_v1_migration_assigns_ordered_ids_and_persists_v2() {
        let folder = temporary_folder("unified-playlist-v1-migration");
        std::fs::create_dir_all(&folder).unwrap();
        let legacy = serde_json::json!({
            "schema_version": 1,
            "playlists": [{
                "id": "legacy",
                "name": "Legacy",
                "items": [
                    {"media_id": {"provider": "Spotify", "kind": "Track", "raw_id": "same"}, "title": "One", "artists": "A", "duration_ms": 1000, "provider_url": null},
                    {"media_id": {"provider": "Spotify", "kind": "Track", "raw_id": "same"}, "title": "Two", "artists": "A", "duration_ms": 2, "duration_unit": "seconds", "provider_url": null}
                ],
                "updated_at": 7
            }],
            "links": []
        });
        std::fs::write(
            folder.join(UNIFIED_PLAYLISTS_FILE),
            serde_json::to_vec(&legacy).unwrap(),
        )
        .unwrap();

        let (playlists, _) = load_unified_playlist_store(&folder).unwrap().unwrap();
        let playlist = &playlists[0];
        assert_eq!(
            playlist
                .items
                .iter()
                .map(|item| item.entry_id.0)
                .collect::<Vec<_>>(),
            vec![1, 2]
        );
        assert_eq!(playlist.next_entry_id, 3);
        assert_eq!(playlist.items[1].duration_ms, Some(2_000));
        assert_eq!(
            playlist.items[1].duration_unit,
            crate::state::DurationUnit::Milliseconds
        );
        let persisted: serde_json::Value =
            serde_json::from_slice(&std::fs::read(folder.join(UNIFIED_PLAYLISTS_FILE)).unwrap())
                .unwrap();
        assert_eq!(
            persisted["schema_version"],
            UNIFIED_PLAYLISTS_SCHEMA_VERSION
        );
        assert_eq!(
            serde_json::from_slice::<serde_json::Value>(
                &std::fs::read(folder.join(UNIFIED_PLAYLISTS_BACKUP_FILE)).unwrap(),
            )
            .unwrap()["schema_version"],
            1
        );
        let (restarted, _) = load_unified_playlist_store(&folder).unwrap().unwrap();
        assert_eq!(
            restarted[0]
                .items
                .iter()
                .map(|item| item.entry_id.0)
                .collect::<Vec<_>>(),
            vec![1, 2]
        );
        assert_eq!(restarted[0].next_entry_id, 3);
        std::fs::remove_dir_all(folder).unwrap();
    }

    #[test]
    fn newer_unified_playlist_schema_is_not_downgraded_from_backup() {
        let folder = temporary_folder("unified-playlist-newer-schema");
        std::fs::create_dir_all(&folder).unwrap();
        let newer = serde_json::json!({
            "schema_version": UNIFIED_PLAYLISTS_SCHEMA_VERSION + 1,
            "playlists": [],
            "links": []
        });
        std::fs::write(
            folder.join(UNIFIED_PLAYLISTS_FILE),
            serde_json::to_vec(&newer).unwrap(),
        )
        .unwrap();
        write_unified_playlists(
            &folder.join(UNIFIED_PLAYLISTS_BACKUP_FILE),
            &[playlist()],
            &[],
        )
        .unwrap();

        assert!(load_unified_playlist_store(&folder).is_err());

        std::fs::remove_dir_all(folder).unwrap();
    }

    #[test]
    fn appending_unified_items_preserves_duplicate_occurrences() {
        let folder = temporary_folder("unified-playlist-append");
        let mut data = AppData::new(&folder, &folder);
        data.unified_playlists.push(playlist());
        let item = UnifiedPlaylistItem {
            media_id: MediaId {
                provider: Provider::YouTubeMusic,
                kind: MediaKind::Track,
                raw_id: "video".to_owned(),
            },
            title: "Video".to_owned(),
            artists: "Artist".to_owned(),
            duration_ms: None,
            provider_url: Some("https://music.youtube.com/watch?v=video".to_owned()),
            ..UnifiedPlaylistItem::default()
        };
        data.append_unified_playlist_items("local-1", vec![item.clone(), item])
            .unwrap();
        assert_eq!(data.unified_playlists[0].items.len(), 2);
        assert_eq!(
            data.unified_playlists[0].items[0].media_id,
            data.unified_playlists[0].items[1].media_id
        );
        assert_ne!(
            data.unified_playlists[0].items[0].entry_id,
            data.unified_playlists[0].items[1].entry_id
        );
        assert_eq!(data.unified_playlists[0].next_entry_id, 3);
        std::fs::remove_dir_all(folder).unwrap();
    }

    #[test]
    fn creating_unified_playlist_rolls_back_when_persistence_is_unavailable() {
        let folder = temporary_folder("unified-playlist-create-rollback");
        let mut data = AppData::new(&folder, &folder);
        data.unified_playlist_store_writable = false;

        assert!(data.upsert_unified_playlist(playlist()).is_err());
        assert!(data.unified_playlists.is_empty());
        let _ = std::fs::remove_dir_all(folder);
    }

    #[test]
    fn appending_unified_items_rolls_back_when_persistence_is_unavailable() {
        let folder = temporary_folder("unified-playlist-append-rollback");
        let mut data = AppData::new(&folder, &folder);
        data.unified_playlists.push(playlist());
        data.unified_playlist_store_writable = false;
        let item = UnifiedPlaylistItem {
            media_id: MediaId {
                provider: Provider::Spotify,
                kind: MediaKind::Track,
                raw_id: "track".to_owned(),
            },
            title: "Track".to_owned(),
            artists: "Artist".to_owned(),
            duration_ms: Some(1_000),
            provider_url: None,
            ..UnifiedPlaylistItem::default()
        };

        assert!(data
            .append_unified_playlist_items("local-1", vec![item])
            .is_err());
        assert!(data.unified_playlists[0].items.is_empty());
        let _ = std::fs::remove_dir_all(folder);
    }

    #[test]
    fn unified_playlist_lifecycle_renames_deletes_and_removes_links() {
        let folder = temporary_folder("unified-playlist-lifecycle");
        let mut data = AppData::new(&folder, &folder);
        data.unified_playlists.push(playlist());
        data.playlist_links.push(PlaylistLink {
            unified_playlist_id: "local-1".to_owned(),
            youtube_playlist_id: Some("PL1".to_owned()),
            ..PlaylistLink::default()
        });

        data.rename_unified_playlist("local-1", "Renamed".to_owned())
            .unwrap();
        assert_eq!(data.unified_playlists[0].name, "Renamed");
        data.delete_unified_playlist("local-1").unwrap();
        assert!(data.unified_playlists.is_empty());
        assert!(data.playlist_links.is_empty());

        let _ = std::fs::remove_dir_all(folder);
    }

    #[test]
    fn remove_move_and_rename_roll_back_when_persistence_is_unavailable() {
        let folder = temporary_folder("unified-playlist-edit-rollback");
        let mut data = AppData::new(&folder, &folder);
        data.unified_playlists.push(playlist());
        let item = UnifiedPlaylistItem {
            media_id: MediaId {
                provider: Provider::Spotify,
                kind: MediaKind::Track,
                raw_id: "track".to_owned(),
            },
            title: "Track".to_owned(),
            artists: "Artist".to_owned(),
            duration_ms: Some(1_000),
            provider_url: None,
            ..UnifiedPlaylistItem::default()
        };
        data.append_unified_playlist_items("local-1", vec![item.clone(), item.clone(), item])
            .unwrap();
        let ids = data.unified_playlists[0]
            .items
            .iter()
            .map(|item| item.entry_id)
            .collect::<Vec<_>>();
        let before = data.unified_playlists.clone();
        data.unified_playlist_store_writable = false;
        assert!(data
            .remove_unified_playlist_items("local-1", &[ids[0]])
            .is_err());
        assert_eq!(data.unified_playlists, before);
        assert!(data
            .move_unified_playlist_items("local-1", &[ids[0], ids[2]], 1)
            .is_err());
        assert_eq!(data.unified_playlists, before);
        assert!(data
            .rename_unified_playlist("local-1", "Changed".to_owned())
            .is_err());
        assert_eq!(data.unified_playlists, before);
        let _ = std::fs::remove_dir_all(folder);
    }

    #[test]
    fn removed_entry_ids_are_not_reused_after_append() {
        let folder = temporary_folder("unified-playlist-id-monotonic");
        let mut data = AppData::new(&folder, &folder);
        data.unified_playlists.push(playlist());
        let item = UnifiedPlaylistItem {
            media_id: MediaId {
                provider: Provider::Spotify,
                kind: MediaKind::Track,
                raw_id: "track".to_owned(),
            },
            title: "Track".to_owned(),
            artists: "Artist".to_owned(),
            duration_ms: None,
            provider_url: None,
            ..UnifiedPlaylistItem::default()
        };
        data.append_unified_playlist_items("local-1", vec![item.clone()])
            .unwrap();
        let first_id = data.unified_playlists[0].items[0].entry_id;
        data.remove_unified_playlist_item("local-1", first_id)
            .unwrap();
        data.append_unified_playlist_items("local-1", vec![item])
            .unwrap();
        assert_eq!(
            data.unified_playlists[0].items[0].entry_id.0,
            first_id.0 + 1
        );
        let _ = std::fs::remove_dir_all(folder);
    }

    #[test]
    fn exact_remove_and_move_reject_partial_stale_or_duplicate_ids_transactionally() {
        let folder = temporary_folder("unified-playlist-exact-edit-validation");
        let mut data = AppData::new(&folder, &folder);
        let mut playlist = playlist();
        playlist.items = (1..=4)
            .map(|entry_id| UnifiedPlaylistItem {
                entry_id: PlaylistEntryId(entry_id),
                media_id: MediaId {
                    provider: Provider::Spotify,
                    kind: MediaKind::Track,
                    raw_id: "duplicate".to_owned(),
                },
                title: format!("Occurrence {entry_id}"),
                ..UnifiedPlaylistItem::default()
            })
            .collect();
        playlist.next_entry_id = 5;
        data.unified_playlists.push(playlist);
        let before = data.unified_playlists.clone();

        assert!(data
            .remove_unified_playlist_items("local-1", &[PlaylistEntryId(2), PlaylistEntryId(99)],)
            .is_err());
        assert_eq!(data.unified_playlists, before);
        assert!(data
            .move_unified_playlist_items("local-1", &[PlaylistEntryId(2), PlaylistEntryId(2)], 0,)
            .is_err());
        assert_eq!(data.unified_playlists, before);
        assert!(data
            .move_unified_playlist_items("local-1", &[PlaylistEntryId(2), PlaylistEntryId(99)], 0,)
            .is_err());
        assert_eq!(data.unified_playlists, before);

        data.unified_playlists[0].items[1].entry_id = PlaylistEntryId(1);
        let malformed = data.unified_playlists.clone();
        assert!(data
            .remove_unified_playlist_items("local-1", &[PlaylistEntryId(1)])
            .is_err());
        assert_eq!(data.unified_playlists, malformed);
        assert!(data
            .move_unified_playlist_items("local-1", &[PlaylistEntryId(1)], 2)
            .is_err());
        assert_eq!(data.unified_playlists, malformed);
        let _ = std::fs::remove_dir_all(folder);
    }

    #[test]
    fn structural_move_preserves_duplicate_occurrence_ids_in_stable_block_order() {
        let folder = temporary_folder("unified-playlist-structural-move");
        let mut data = AppData::new(&folder, &folder);
        let mut playlist = playlist();
        playlist.items = (1..=5)
            .map(|entry_id| UnifiedPlaylistItem {
                entry_id: PlaylistEntryId(entry_id),
                media_id: MediaId {
                    provider: Provider::Spotify,
                    kind: MediaKind::Track,
                    raw_id: "duplicate".to_owned(),
                },
                title: format!("Occurrence {entry_id}"),
                ..UnifiedPlaylistItem::default()
            })
            .collect();
        playlist.next_entry_id = 6;
        data.unified_playlists.push(playlist);

        data.move_unified_playlist_items("local-1", &[PlaylistEntryId(4), PlaylistEntryId(2)], 3)
            .unwrap();
        assert_eq!(
            data.unified_playlists[0]
                .items
                .iter()
                .map(|item| item.entry_id.0)
                .collect::<Vec<_>>(),
            [1, 3, 5, 2, 4]
        );
        let _ = std::fs::remove_dir_all(folder);
    }

    #[test]
    fn structural_move_rejects_a_changed_full_order_before_mutation() {
        let folder = temporary_folder("unified-playlist-stale-structural-move");
        let mut data = AppData::new(&folder, &folder);
        let mut playlist = playlist();
        playlist.items = (1..=4)
            .map(|entry_id| UnifiedPlaylistItem {
                entry_id: PlaylistEntryId(entry_id),
                title: format!("Occurrence {entry_id}"),
                ..UnifiedPlaylistItem::default()
            })
            .collect();
        playlist.next_entry_id = 5;
        data.unified_playlists.push(playlist);
        let expected = [PlaylistEntryId(1), PlaylistEntryId(2), PlaylistEntryId(3)];
        let before = data.unified_playlists.clone();

        assert!(data
            .move_unified_playlist_items_if_current("local-1", &expected, &[PlaylistEntryId(2)], 0,)
            .is_err());
        assert_eq!(data.unified_playlists, before);
        assert!(data
            .remove_unified_playlist_items_if_current("local-1", &expected, &[PlaylistEntryId(2)],)
            .is_err());
        assert_eq!(data.unified_playlists, before);
        let _ = std::fs::remove_dir_all(folder);
    }

    #[test]
    fn exact_remove_addresses_selected_duplicate_occurrences_without_touching_the_other() {
        let folder = temporary_folder("unified-playlist-exact-duplicate-remove");
        let mut data = AppData::new(&folder, &folder);
        let mut playlist = playlist();
        playlist.items = (1..=3)
            .map(|entry_id| UnifiedPlaylistItem {
                entry_id: PlaylistEntryId(entry_id),
                media_id: MediaId {
                    provider: Provider::Spotify,
                    kind: MediaKind::Track,
                    raw_id: "duplicate".to_owned(),
                },
                title: format!("Occurrence {entry_id}"),
                ..UnifiedPlaylistItem::default()
            })
            .collect();
        playlist.next_entry_id = 4;
        data.unified_playlists.push(playlist);

        data.remove_unified_playlist_items("local-1", &[PlaylistEntryId(2), PlaylistEntryId(3)])
            .unwrap();
        assert_eq!(data.unified_playlists[0].items.len(), 1);
        assert_eq!(
            data.unified_playlists[0].items[0].entry_id,
            PlaylistEntryId(1)
        );
        assert_eq!(
            data.unified_playlists[0].items[0].media_id.raw_id,
            "duplicate"
        );
        let _ = std::fs::remove_dir_all(folder);
    }
}
