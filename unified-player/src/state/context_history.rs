//! Local history of opened collections, used by Home's "Continue" shelf.
//!
//! Track session history cannot reconstruct the playlist, album or artist
//! page a user opened, so this records successfully opened contexts with the
//! identity and display text needed to reopen them.

use std::path::Path;

use serde::{Deserialize, Serialize};

use crate::config::ActiveProvider;

const CONTEXT_HISTORY_FILE: &str = "context-history.json";
const CONTEXT_HISTORY_SCHEMA_VERSION: u32 = 1;
/// Entries kept per provider/account namespace.
pub const MAX_CONTEXT_HISTORY_PER_NAMESPACE: usize = 50;

/// A reopenable collection. IDs are raw provider IDs, never URLs or tokens.
#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(tag = "kind", content = "id", rename_all = "snake_case")]
pub enum HistoryContext {
    SpotifyPlaylist(String),
    SpotifyAlbum(String),
    SpotifyArtist(String),
    SpotifyShow(String),
    SpotifyLikedTracks,
    YouTubePlaylist(String),
    YouTubeAlbum(String),
    YouTubeArtist(String),
    YouTubePodcast(String),
    YouTubeLikedTracks,
    UnifiedPlaylist(String),
}

impl HistoryContext {
    /// The provider whose account owns this context; local unified playlists
    /// belong to none.
    pub const fn provider(&self) -> Option<ActiveProvider> {
        match self {
            Self::SpotifyPlaylist(_)
            | Self::SpotifyAlbum(_)
            | Self::SpotifyArtist(_)
            | Self::SpotifyShow(_)
            | Self::SpotifyLikedTracks => Some(ActiveProvider::Spotify),
            Self::YouTubePlaylist(_)
            | Self::YouTubeAlbum(_)
            | Self::YouTubeArtist(_)
            | Self::YouTubePodcast(_)
            | Self::YouTubeLikedTracks => Some(ActiveProvider::YouTubeMusic),
            Self::UnifiedPlaylist(_) => None,
        }
    }

    pub const fn kind_label(&self) -> &'static str {
        match self {
            Self::SpotifyPlaylist(_) | Self::YouTubePlaylist(_) => "Playlist",
            Self::SpotifyAlbum(_) | Self::YouTubeAlbum(_) => "Album",
            Self::SpotifyArtist(_) | Self::YouTubeArtist(_) => "Artist",
            Self::SpotifyShow(_) | Self::YouTubePodcast(_) => "Podcast",
            Self::SpotifyLikedTracks | Self::YouTubeLikedTracks => "Liked songs",
            Self::UnifiedPlaylist(_) => "Unified playlist",
        }
    }
}

/// Which provider account a history entry was recorded under. Unified
/// playlists are local and carry no provider or account.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct HistoryNamespace {
    pub provider: Option<ActiveProvider>,
    pub account: Option<String>,
}

impl HistoryNamespace {
    pub fn for_context(context: &HistoryContext, account: Option<&str>) -> Self {
        let provider = context.provider();
        Self {
            provider,
            account: provider.and(account.map(str::to_owned)),
        }
    }

    /// Whether an entry is visible while browsing `provider` as `account`.
    fn is_visible_to(&self, provider: ActiveProvider, account: Option<&str>) -> bool {
        match self.provider {
            None => true,
            Some(owner) => owner == provider && self.account.as_deref() == account,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ContextHistoryEntry {
    pub namespace: HistoryNamespace,
    pub context: HistoryContext,
    pub title: String,
    pub subtitle: String,
    pub opened_at: u64,
}

impl ContextHistoryEntry {
    /// An entry for a loaded Spotify context; `None` for track lists other
    /// than Liked Songs (radio, top and recent tracks are not collections).
    pub fn from_spotify(
        id: &super::ContextId,
        context: &super::Context,
        account: Option<&str>,
        opened_at: u64,
    ) -> Option<Self> {
        use super::{Context, ContextId};
        use rspotify::model::Id as _;
        let (history, title, subtitle) = match (id, context) {
            (ContextId::Playlist(id), Context::Playlist { playlist, .. }) => (
                HistoryContext::SpotifyPlaylist(id.id().to_owned()),
                playlist.name.clone(),
                format!("Playlist · {}", playlist.owner.0),
            ),
            (ContextId::Album(id), Context::Album { album, .. }) => (
                HistoryContext::SpotifyAlbum(id.id().to_owned()),
                album.name.clone(),
                format!(
                    "Album · {}",
                    crate::utils::map_join(&album.artists, |artist| &artist.name, ", ")
                ),
            ),
            (ContextId::Artist(id), Context::Artist { artist, .. }) => (
                HistoryContext::SpotifyArtist(id.id().to_owned()),
                artist.name.clone(),
                "Artist".to_owned(),
            ),
            (ContextId::Show(id), Context::Show { show, .. }) => (
                HistoryContext::SpotifyShow(id.id().to_owned()),
                show.name.clone(),
                "Podcast".to_owned(),
            ),
            (ContextId::Tracks(id), Context::Tracks { .. })
                if id.uri == super::USER_LIKED_TRACKS_URI =>
            {
                (
                    HistoryContext::SpotifyLikedTracks,
                    "Liked Music".to_owned(),
                    "Your liked songs".to_owned(),
                )
            }
            _ => return None,
        };
        Some(Self::new(history, title, subtitle, account, opened_at))
    }

    /// An entry for a loaded `YouTube` Music context.
    pub fn from_youtube(
        id: &super::YouTubeContextId,
        context: &super::YouTubeContext,
        account: Option<&str>,
        opened_at: u64,
    ) -> Self {
        use super::YouTubeContextId;
        let (history, title) = match id {
            YouTubeContextId::LikedTracks => {
                (HistoryContext::YouTubeLikedTracks, "Liked Music".to_owned())
            }
            YouTubeContextId::Playlist(id) => (
                HistoryContext::YouTubePlaylist(id.clone()),
                context.title.clone(),
            ),
            YouTubeContextId::Album(id) => (
                HistoryContext::YouTubeAlbum(id.clone()),
                context.title.clone(),
            ),
            YouTubeContextId::Artist(id) => (
                HistoryContext::YouTubeArtist(id.clone()),
                context.title.clone(),
            ),
            YouTubeContextId::Podcast(id) => (
                HistoryContext::YouTubePodcast(id.clone()),
                context.title.clone(),
            ),
        };
        let subtitle = match &history {
            HistoryContext::YouTubeLikedTracks => "Your liked songs".to_owned(),
            other => other.kind_label().to_owned(),
        };
        Self::new(history, title, subtitle, account, opened_at)
    }

    /// An entry for a local unified playlist.
    pub fn from_unified(playlist: &super::UnifiedPlaylist, opened_at: u64) -> Self {
        let count = playlist.items.len();
        Self::new(
            HistoryContext::UnifiedPlaylist(playlist.id.clone()),
            playlist.name.clone(),
            format!(
                "Unified playlist · {count} {}",
                if count == 1 { "item" } else { "items" }
            ),
            None,
            opened_at,
        )
    }

    fn new(
        context: HistoryContext,
        title: String,
        subtitle: String,
        account: Option<&str>,
        opened_at: u64,
    ) -> Self {
        Self {
            namespace: HistoryNamespace::for_context(&context, account),
            context,
            title,
            subtitle,
            opened_at,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ContextHistory {
    #[serde(default = "context_history_schema_version")]
    pub schema_version: u32,
    /// Newest first.
    #[serde(default)]
    entries: Vec<ContextHistoryEntry>,
}

impl Default for ContextHistory {
    fn default() -> Self {
        Self {
            schema_version: CONTEXT_HISTORY_SCHEMA_VERSION,
            entries: Vec::new(),
        }
    }
}

const fn context_history_schema_version() -> u32 {
    CONTEXT_HISTORY_SCHEMA_VERSION
}

impl ContextHistory {
    /// Load the history, starting empty when it is missing, unreadable or
    /// from a newer schema.
    pub fn load(config_folder: &Path) -> Self {
        let path = config_folder.join(CONTEXT_HISTORY_FILE);
        let content = match std::fs::read_to_string(&path) {
            Ok(content) => content,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Self::default(),
            Err(error) => {
                crate::observability::log_safe_error!(
                    warn,
                    crate::observability::DiagnosticCode::CONTEXT_HISTORY_OPEN_FAILED,
                    crate::observability::ErrorCategory::Storage,
                    &error,
                    "Local context history could not be opened; starting empty"
                );
                return Self::default();
            }
        };
        match serde_json::from_str::<Self>(&content) {
            Ok(history) if history.schema_version <= CONTEXT_HISTORY_SCHEMA_VERSION => {
                let mut history = history;
                history.schema_version = CONTEXT_HISTORY_SCHEMA_VERSION;
                history.enforce_bounds();
                history
            }
            Ok(_) => Self::default(),
            Err(error) => {
                crate::observability::log_safe_error!(
                    warn,
                    crate::observability::DiagnosticCode::CONTEXT_HISTORY_DECODE_FAILED,
                    crate::observability::ErrorCategory::Decode,
                    &error,
                    "Local context history could not be read; starting empty"
                );
                Self::default()
            }
        }
    }

    pub fn save(&self, config_folder: &Path) -> anyhow::Result<()> {
        std::fs::create_dir_all(config_folder)?;
        let path = config_folder.join(CONTEXT_HISTORY_FILE);
        let file = std::fs::File::create(path)?;
        serde_json::to_writer_pretty(std::io::BufWriter::new(file), self)?;
        Ok(())
    }

    /// Move `entry` to the front, replacing an earlier visit to the same
    /// context in the same namespace. Returns whether anything changed.
    pub fn record(&mut self, entry: ContextHistoryEntry) -> bool {
        if self.entries.first().is_some_and(|first| {
            first.namespace == entry.namespace
                && first.context == entry.context
                && first.title == entry.title
                && first.subtitle == entry.subtitle
        }) {
            return false;
        }
        self.entries.retain(|existing| {
            existing.namespace != entry.namespace || existing.context != entry.context
        });
        self.entries.insert(0, entry);
        self.enforce_bounds();
        true
    }

    pub fn clear(&mut self) {
        self.entries.clear();
    }

    /// Entries visible while browsing `provider` as `account`, newest first.
    pub fn visible<'a>(
        &'a self,
        provider: ActiveProvider,
        account: Option<&'a str>,
    ) -> impl Iterator<Item = &'a ContextHistoryEntry> {
        self.entries
            .iter()
            .filter(move |entry| entry.namespace.is_visible_to(provider, account))
    }

    #[cfg(test)]
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    fn enforce_bounds(&mut self) {
        let mut kept: Vec<&HistoryNamespace> = Vec::new();
        let mut counts: Vec<usize> = Vec::new();
        let mut keep = Vec::with_capacity(self.entries.len());
        for entry in &self.entries {
            let slot = kept
                .iter()
                .position(|ns| **ns == entry.namespace)
                .unwrap_or_else(|| {
                    kept.push(&entry.namespace);
                    counts.push(0);
                    counts.len() - 1
                });
            counts[slot] += 1;
            keep.push(counts[slot] <= MAX_CONTEXT_HISTORY_PER_NAMESPACE);
        }
        let mut keep = keep.into_iter();
        self.entries.retain(|_| keep.next().unwrap_or(false));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(context: HistoryContext, account: Option<&str>, title: &str) -> ContextHistoryEntry {
        ContextHistoryEntry {
            namespace: HistoryNamespace::for_context(&context, account),
            context,
            title: title.to_owned(),
            subtitle: String::new(),
            opened_at: 0,
        }
    }

    fn titles(history: &ContextHistory, provider: ActiveProvider, account: &str) -> Vec<String> {
        history
            .visible(provider, Some(account))
            .map(|entry| entry.title.clone())
            .collect()
    }

    #[test]
    fn revisiting_moves_a_context_to_the_front_without_duplicating_it() {
        let mut history = ContextHistory::default();
        let a = HistoryContext::SpotifyPlaylist("a".to_owned());
        let b = HistoryContext::SpotifyAlbum("b".to_owned());
        assert!(history.record(entry(a.clone(), Some("me"), "A")));
        assert!(history.record(entry(b, Some("me"), "B")));
        assert!(history.record(entry(a.clone(), Some("me"), "A renamed")));
        assert!(!history.record(entry(a, Some("me"), "A renamed")));

        assert_eq!(
            titles(&history, ActiveProvider::Spotify, "me"),
            ["A renamed", "B"]
        );
    }

    #[test]
    fn same_id_or_title_in_different_kinds_stays_distinct() {
        let mut history = ContextHistory::default();
        history.record(entry(
            HistoryContext::SpotifyPlaylist("x".to_owned()),
            Some("me"),
            "Same",
        ));
        history.record(entry(
            HistoryContext::SpotifyAlbum("x".to_owned()),
            Some("me"),
            "Same",
        ));
        assert_eq!(history.len(), 2);
    }

    #[test]
    fn entries_are_isolated_by_provider_and_account_but_local_ones_are_shared() {
        let mut history = ContextHistory::default();
        history.record(entry(
            HistoryContext::SpotifyPlaylist("s".to_owned()),
            Some("one"),
            "Spotify one",
        ));
        history.record(entry(
            HistoryContext::SpotifyPlaylist("s".to_owned()),
            Some("two"),
            "Spotify two",
        ));
        history.record(entry(
            HistoryContext::YouTubePlaylist("y".to_owned()),
            Some("one"),
            "YouTube one",
        ));
        history.record(entry(
            HistoryContext::UnifiedPlaylist("u".to_owned()),
            Some("ignored"),
            "Local",
        ));

        assert_eq!(
            titles(&history, ActiveProvider::Spotify, "one"),
            ["Local", "Spotify one"]
        );
        assert_eq!(
            titles(&history, ActiveProvider::Spotify, "two"),
            ["Local", "Spotify two"]
        );
        assert_eq!(
            titles(&history, ActiveProvider::YouTubeMusic, "one"),
            ["Local", "YouTube one"]
        );
    }

    #[test]
    fn each_namespace_is_bounded_independently() {
        let mut history = ContextHistory::default();
        for index in 0..=MAX_CONTEXT_HISTORY_PER_NAMESPACE {
            history.record(entry(
                HistoryContext::SpotifyPlaylist(index.to_string()),
                Some("me"),
                &index.to_string(),
            ));
        }
        history.record(entry(
            HistoryContext::YouTubeLikedTracks,
            Some("me"),
            "Liked",
        ));

        let spotify = titles(&history, ActiveProvider::Spotify, "me");
        assert_eq!(spotify.len(), MAX_CONTEXT_HISTORY_PER_NAMESPACE);
        assert_eq!(spotify[0], MAX_CONTEXT_HISTORY_PER_NAMESPACE.to_string());
        assert!(!spotify.contains(&"0".to_owned()));
        assert_eq!(
            titles(&history, ActiveProvider::YouTubeMusic, "me"),
            ["Liked"]
        );
    }

    #[test]
    fn history_survives_a_restart_and_clearing_persists() {
        let folder = tempfile::tempdir().unwrap();
        let mut history = ContextHistory::default();
        history.record(entry(
            HistoryContext::YouTubeAlbum("album".to_owned()),
            Some("me"),
            "Album",
        ));
        history.save(folder.path()).unwrap();

        let mut restarted = ContextHistory::load(folder.path());
        assert_eq!(restarted, history);

        restarted.clear();
        restarted.save(folder.path()).unwrap();
        assert_eq!(ContextHistory::load(folder.path()).len(), 0);
    }

    #[test]
    fn unreadable_or_newer_files_start_empty() {
        let folder = tempfile::tempdir().unwrap();
        std::fs::write(folder.path().join(CONTEXT_HISTORY_FILE), b"not-json").unwrap();
        assert_eq!(ContextHistory::load(folder.path()).len(), 0);

        let newer = serde_json::json!({
            "schema_version": CONTEXT_HISTORY_SCHEMA_VERSION + 1,
            "entries": [],
        });
        std::fs::write(folder.path().join(CONTEXT_HISTORY_FILE), newer.to_string()).unwrap();
        assert_eq!(
            ContextHistory::load(folder.path()).schema_version,
            CONTEXT_HISTORY_SCHEMA_VERSION
        );
    }
}
