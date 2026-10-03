//! Home page model: named shelves of reopenable collections and tracks.
//!
//! Shelves are built from data the app already holds (local context and
//! session history, the saved library) plus Spotify's recently-played and
//! top-tracks reads, and label their source honestly; nothing here claims
//! to be a personalised recommendation.

use std::time::{Duration, Instant};

use rspotify::model::{Id as _, PlayableId};

use super::{AppData, HistoryContext, PlayableMedia, PlaylistFolderItem, Track, YouTubeTrack};
use crate::config::ActiveProvider;

/// Cards kept per shelf; shelves scroll horizontally within this bound.
pub const MAX_HOME_SHELF_CARDS: usize = 20;
/// Tiles in the quick-access grid.
pub const MAX_QUICK_ACCESS_TILES: usize = 8;
/// Entries listed by a full "Show all" view of a local shelf.
pub const MAX_HOME_LIST_ITEMS: usize = 200;
/// How long a Spotify Home read is reused before Home fetches it again.
pub const HOME_FEED_TTL: Duration = Duration::from_secs(10 * 60);
/// How long a failed Spotify Home read waits before Home tries it again.
/// Spotify's request limits are rolling windows, so a later read can succeed.
pub const HOME_FEED_RETRY_DELAY: Duration = Duration::from_secs(2 * 60);

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum HomeShelfKind {
    #[default]
    QuickAccess,
    UnifiedPlaylists,
    RecentlyPlayed,
    Continue,
    TopTracks,
    Playlists,
    Albums,
    Artists,
}

impl HomeShelfKind {
    pub const fn title(self) -> &'static str {
        match self {
            Self::QuickAccess => "Quick access",
            Self::UnifiedPlaylists => "Unified playlists",
            Self::RecentlyPlayed => "Recently played",
            Self::Continue => "Continue where you left off",
            Self::TopTracks => "Your top tracks",
            Self::Playlists => "Your playlists",
            Self::Albums => "Saved albums",
            Self::Artists => "Followed artists",
        }
    }
}

/// What a card does when chosen.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum HomeTarget {
    /// Open a collection; never starts playback.
    Context(HistoryContext),
    /// Play this track, continuing through the shelf's other tracks.
    Track(PlayableMedia),
    /// Open the shelf's full list.
    ShowAll(HomeShelfKind),
    /// Fetch the shelf's provider data again.
    Retry,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct HomeCard {
    pub title: String,
    pub subtitle: String,
    pub target: HomeTarget,
}

impl HomeCard {
    fn context(title: String, subtitle: String, context: HistoryContext) -> Self {
        Self {
            title,
            subtitle,
            target: HomeTarget::Context(context),
        }
    }

    fn show_all(kind: HomeShelfKind) -> Self {
        Self {
            title: "Show all".to_owned(),
            subtitle: "Open the full list".to_owned(),
            target: HomeTarget::ShowAll(kind),
        }
    }
}

/// Whether a shelf's source has data to show.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum HomeShelfStatus {
    #[default]
    Ready,
    Loading,
    Failed(HomeFeedFailure),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct HomeShelf {
    pub kind: HomeShelfKind,
    pub status: HomeShelfStatus,
    pub cards: Vec<HomeCard>,
    /// Shown instead of (or above) cards: loading, failure or empty text.
    pub message: Option<&'static str>,
}

/// The browsing scope Home is built for.
#[derive(Clone, Copy, Debug)]
pub struct HomeScope<'a> {
    pub provider: ActiveProvider,
    pub account: Option<&'a str>,
    /// Whether Spotify reads can run (authenticated session).
    pub spotify_ready: bool,
}

/// Spotify reads Home fetches; each fails or succeeds on its own.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum HomeFeedSource {
    RecentlyPlayed,
    TopTracks,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum HomeFeedStatus {
    #[default]
    Idle,
    Loading,
    Ready,
    Failed,
}

/// Why a Spotify Home read failed, as far as the user can act on it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum HomeFeedFailure {
    /// Spotify refused more requests from this app for now (HTTP 429).
    RequestLimit,
    /// Spotify refused this account or app access (HTTP 401/403).
    AccessDenied,
    Unavailable,
}

impl HomeFeedFailure {
    /// Short text for the Retry card.
    pub const fn label(self) -> &'static str {
        match self {
            Self::RequestLimit => "Spotify request limit",
            Self::AccessDenied => "Spotify denied access",
            Self::Unavailable => "Could not load",
        }
    }
}

#[derive(Debug, Default)]
pub struct HomeFeedSlot {
    pub status: HomeFeedStatus,
    pub failure: Option<HomeFeedFailure>,
    pub tracks: Vec<Track>,
    fetched_at: Option<Instant>,
    failed_at: Option<Instant>,
}

/// Session-local Spotify data for Home, scoped to one account. A fetch is
/// tagged with a generation so a response for an older account or an older
/// refresh is dropped.
#[derive(Debug, Default)]
pub struct HomeFeed {
    /// Whether a fetch has been started, and so whether `account` applies.
    scoped: bool,
    account: Option<String>,
    generation: u64,
    pub recently_played: HomeFeedSlot,
    pub top_tracks: HomeFeedSlot,
}

impl HomeFeed {
    /// Start a fetch for `account` when it has no data yet, its data went
    /// stale, the account changed, or `force` asks for a retry. A failed
    /// source is retried after `HOME_FEED_RETRY_DELAY`, except when access was
    /// denied, which waits for an explicit retry. Returns the fetch generation.
    pub fn begin_refresh(
        &mut self,
        account: Option<&str>,
        now: Instant,
        force: bool,
    ) -> Option<u64> {
        let account = account.map(str::to_owned);
        let same_account = self.scoped && self.account == account;
        let slots = [&self.recently_played, &self.top_tracks];
        let loading = slots
            .iter()
            .any(|slot| slot.status == HomeFeedStatus::Loading);
        let needed = slots.iter().any(|slot| match slot.status {
            HomeFeedStatus::Idle => true,
            HomeFeedStatus::Ready => slot
                .fetched_at
                .is_none_or(|fetched| now.duration_since(fetched) >= HOME_FEED_TTL),
            HomeFeedStatus::Failed => {
                slot.failure != Some(HomeFeedFailure::AccessDenied)
                    && slot
                        .failed_at
                        .is_none_or(|failed| now.duration_since(failed) >= HOME_FEED_RETRY_DELAY)
            }
            HomeFeedStatus::Loading => false,
        });
        if same_account && !force && (loading || !needed) {
            return None;
        }
        if !same_account {
            self.recently_played = HomeFeedSlot::default();
            self.top_tracks = HomeFeedSlot::default();
        }
        self.scoped = true;
        self.account = account;
        self.generation = self.generation.wrapping_add(1);
        self.recently_played.status = HomeFeedStatus::Loading;
        self.top_tracks.status = HomeFeedStatus::Loading;
        Some(self.generation)
    }

    /// Store one source's result if it belongs to the latest fetch.
    pub fn apply(
        &mut self,
        generation: u64,
        source: HomeFeedSource,
        result: Result<Vec<Track>, HomeFeedFailure>,
        now: Instant,
    ) -> bool {
        if generation != self.generation {
            return false;
        }
        let slot = match source {
            HomeFeedSource::RecentlyPlayed => &mut self.recently_played,
            HomeFeedSource::TopTracks => &mut self.top_tracks,
        };
        match result {
            Ok(tracks) => {
                slot.tracks = tracks;
                slot.status = HomeFeedStatus::Ready;
                slot.failure = None;
                slot.fetched_at = Some(now);
            }
            Err(failure) => {
                slot.status = HomeFeedStatus::Failed;
                slot.failure = Some(failure);
                slot.failed_at = Some(now);
            }
        }
        true
    }

    /// The slot for `source` if it was fetched for `account`.
    fn slot_for(&self, source: HomeFeedSource, account: Option<&str>) -> Option<&HomeFeedSlot> {
        if !self.scoped || self.account.as_deref() != account {
            return None;
        }
        Some(match source {
            HomeFeedSource::RecentlyPlayed => &self.recently_played,
            HomeFeedSource::TopTracks => &self.top_tracks,
        })
    }
}

/// Build the visible shelves for `scope`, in display order. Shelves with
/// nothing to show and no message are left out.
pub fn home_shelves(data: &AppData, scope: HomeScope<'_>) -> Vec<HomeShelf> {
    let recent_contexts = continue_cards(data, scope, MAX_HOME_SHELF_CARDS);
    let playlists = playlist_cards(data, scope.provider);
    let unified = unified_playlist_cards(data);
    let quick_access = quick_access_cards(scope.provider, &recent_contexts, &playlists, &unified);

    let mut shelves = vec![HomeShelf {
        kind: HomeShelfKind::QuickAccess,
        status: HomeShelfStatus::Ready,
        cards: quick_access,
        message: None,
    }];
    shelves.push(list_shelf(HomeShelfKind::UnifiedPlaylists, unified, None));
    shelves.extend(track_shelf(data, scope, HomeShelfKind::RecentlyPlayed));
    shelves.push(list_shelf(
        HomeShelfKind::Continue,
        recent_contexts,
        Some("Open a playlist, album or artist and it will appear here."),
    ));
    shelves.extend(track_shelf(data, scope, HomeShelfKind::TopTracks));
    shelves.push(list_shelf(HomeShelfKind::Playlists, playlists, None));
    shelves.push(list_shelf(
        HomeShelfKind::Albums,
        album_cards(data, scope.provider),
        None,
    ));
    shelves.push(list_shelf(
        HomeShelfKind::Artists,
        artist_cards(data, scope.provider),
        None,
    ));
    shelves.retain(|shelf| !shelf.cards.is_empty() || shelf.message.is_some());
    shelves
}

/// The full, uncapped list behind a shelf's "Show all", for shelves whose
/// full view lives on Home itself.
pub fn home_shelf_list(data: &AppData, scope: HomeScope<'_>, kind: HomeShelfKind) -> Vec<HomeCard> {
    match kind {
        HomeShelfKind::Continue => continue_cards(data, scope, MAX_HOME_LIST_ITEMS),
        HomeShelfKind::UnifiedPlaylists => unified_playlist_cards(data),
        HomeShelfKind::RecentlyPlayed => track_cards(data, scope, kind, MAX_HOME_LIST_ITEMS)
            .map(|(cards, _)| cards)
            .unwrap_or_default(),
        _ => Vec::new(),
    }
}

/// A shelf of collections with a trailing "Show all" card when non-empty.
fn list_shelf(
    kind: HomeShelfKind,
    mut cards: Vec<HomeCard>,
    empty: Option<&'static str>,
) -> HomeShelf {
    let message = if cards.is_empty() { empty } else { None };
    cards.truncate(MAX_HOME_SHELF_CARDS);
    if !cards.is_empty() {
        cards.push(HomeCard::show_all(kind));
    }
    HomeShelf {
        kind,
        status: HomeShelfStatus::Ready,
        cards,
        message,
    }
}

/// Recently played or top tracks; `None` when the source does not exist for
/// this provider or session.
fn track_shelf(data: &AppData, scope: HomeScope<'_>, kind: HomeShelfKind) -> Option<HomeShelf> {
    let (mut cards, status) = track_cards(data, scope, kind, MAX_HOME_SHELF_CARDS)?;
    let message = match status {
        HomeShelfStatus::Loading => Some("Loading…"),
        HomeShelfStatus::Failed(_) => Some("Could not load this shelf."),
        HomeShelfStatus::Ready if cards.is_empty() => Some(match kind {
            HomeShelfKind::TopTracks => "Spotify has no top tracks for this account yet.",
            _ => "Nothing played yet.",
        }),
        HomeShelfStatus::Ready => None,
    };
    match status {
        HomeShelfStatus::Failed(failure) => {
            cards = vec![HomeCard {
                title: "Retry".to_owned(),
                subtitle: failure.label().to_owned(),
                target: HomeTarget::Retry,
            }];
        }
        HomeShelfStatus::Ready if !cards.is_empty() => cards.push(HomeCard::show_all(kind)),
        _ => {}
    }
    Some(HomeShelf {
        kind,
        status,
        cards,
        message,
    })
}

fn track_cards(
    data: &AppData,
    scope: HomeScope<'_>,
    kind: HomeShelfKind,
    limit: usize,
) -> Option<(Vec<HomeCard>, HomeShelfStatus)> {
    match (scope.provider, kind) {
        (ActiveProvider::Spotify, HomeShelfKind::RecentlyPlayed | HomeShelfKind::TopTracks) => {
            if !scope.spotify_ready {
                return None;
            }
            let source = if kind == HomeShelfKind::TopTracks {
                HomeFeedSource::TopTracks
            } else {
                HomeFeedSource::RecentlyPlayed
            };
            let Some(slot) = data.home_feed.slot_for(source, scope.account) else {
                return Some((Vec::new(), HomeShelfStatus::Loading));
            };
            let status = match slot.status {
                // A refresh, failed or still running, keeps showing the last
                // tracks; a failure is retried after `HOME_FEED_RETRY_DELAY`.
                HomeFeedStatus::Loading | HomeFeedStatus::Failed if !slot.tracks.is_empty() => {
                    HomeShelfStatus::Ready
                }
                HomeFeedStatus::Idle | HomeFeedStatus::Loading => HomeShelfStatus::Loading,
                HomeFeedStatus::Failed => {
                    HomeShelfStatus::Failed(slot.failure.unwrap_or(HomeFeedFailure::Unavailable))
                }
                HomeFeedStatus::Ready => HomeShelfStatus::Ready,
            };
            let mut seen = std::collections::HashSet::new();
            let cards = slot
                .tracks
                .iter()
                .filter(|track| seen.insert(track.id.id().to_owned()))
                .take(limit)
                .map(spotify_track_card)
                .collect();
            Some((cards, status))
        }
        (ActiveProvider::YouTubeMusic, HomeShelfKind::RecentlyPlayed) => {
            let mut seen = std::collections::HashSet::new();
            let cards = data
                .session_history
                .newest_first()
                .filter(|entry| entry.media_id.provider == super::Provider::YouTubeMusic)
                .filter(|entry| seen.insert(entry.media_id.raw_id.clone()))
                .take(limit)
                .map(|entry| HomeCard {
                    title: entry.title.clone(),
                    subtitle: format!("Song · {}", entry.artists),
                    target: HomeTarget::Track(PlayableMedia::YouTube(YouTubeTrack {
                        id: entry.media_id.raw_id.clone(),
                        name: entry.title.clone(),
                        artists: entry.artists.clone(),
                        album: entry.album.clone(),
                        duration: entry
                            .duration_ms
                            .map(|ms| format!("{}:{:02}", ms / 60_000, (ms / 1_000) % 60))
                            .unwrap_or_default(),
                        explicit: false,
                        thumbnail_url: None,
                        is_video: entry.media_id.kind == super::MediaKind::Video,
                    })),
                })
                .collect::<Vec<_>>();
            // Local history is only a shelf once something was played.
            (!cards.is_empty()).then_some((cards, HomeShelfStatus::Ready))
        }
        _ => None,
    }
}

fn spotify_track_card(track: &Track) -> HomeCard {
    HomeCard {
        title: track.name.clone(),
        subtitle: format!("Song · {}", track.artists_info()),
        target: HomeTarget::Track(PlayableMedia::Spotify(PlayableId::Track(track.id.clone()))),
    }
}

fn continue_cards(data: &AppData, scope: HomeScope<'_>, limit: usize) -> Vec<HomeCard> {
    data.context_history
        .visible(scope.provider, scope.account)
        .take(limit)
        .map(|entry| {
            HomeCard::context(
                entry.title.clone(),
                entry.subtitle.clone(),
                entry.context.clone(),
            )
        })
        .collect()
}

/// Liked songs first, then recently opened collections, then saved and
/// unified playlists, without repeating a collection.
fn quick_access_cards(
    provider: ActiveProvider,
    recent: &[HomeCard],
    playlists: &[HomeCard],
    unified: &[HomeCard],
) -> Vec<HomeCard> {
    let liked = HomeCard::context(
        "Liked Music".to_owned(),
        "Your liked songs".to_owned(),
        match provider {
            ActiveProvider::Spotify => HistoryContext::SpotifyLikedTracks,
            ActiveProvider::YouTubeMusic => HistoryContext::YouTubeLikedTracks,
        },
    );
    let mut tiles: Vec<HomeCard> = Vec::with_capacity(MAX_QUICK_ACCESS_TILES);
    for card in std::iter::once(&liked)
        .chain(recent)
        .chain(playlists)
        .chain(unified)
    {
        if tiles.len() == MAX_QUICK_ACCESS_TILES {
            break;
        }
        if !tiles.iter().any(|tile| tile.target == card.target) {
            tiles.push(card.clone());
        }
    }
    tiles
}

fn playlist_cards(data: &AppData, provider: ActiveProvider) -> Vec<HomeCard> {
    match provider {
        ActiveProvider::Spotify => data
            .user_data
            .playlists
            .iter()
            .filter_map(|item| match item {
                PlaylistFolderItem::Playlist(playlist) => Some(HomeCard::context(
                    playlist.name.clone(),
                    format!("Playlist · {}", playlist.owner.0),
                    HistoryContext::SpotifyPlaylist(playlist.id.id().to_owned()),
                )),
                PlaylistFolderItem::Folder(_) => None,
            })
            .collect(),
        ActiveProvider::YouTubeMusic => data
            .user_data
            .youtube_library
            .playlists
            .iter()
            .map(|playlist| {
                HomeCard::context(
                    playlist.name.clone(),
                    if playlist.author.is_empty() {
                        "Playlist".to_owned()
                    } else {
                        format!("Playlist · {}", playlist.author)
                    },
                    HistoryContext::YouTubePlaylist(playlist.id.clone()),
                )
            })
            .collect(),
    }
}

/// Unified playlists span providers, so they show under every provider.
fn unified_playlist_cards(data: &AppData) -> Vec<HomeCard> {
    data.unified_playlists
        .iter()
        .map(|playlist| {
            HomeCard::context(
                playlist.name.clone(),
                "Unified playlist".to_owned(),
                HistoryContext::UnifiedPlaylist(playlist.id.clone()),
            )
        })
        .collect()
}

fn album_cards(data: &AppData, provider: ActiveProvider) -> Vec<HomeCard> {
    match provider {
        ActiveProvider::Spotify => data
            .user_data
            .saved_albums
            .iter()
            .map(|album| {
                HomeCard::context(
                    album.name.clone(),
                    format!(
                        "Album · {}",
                        crate::utils::map_join(&album.artists, |artist| &artist.name, ", ")
                    ),
                    HistoryContext::SpotifyAlbum(album.id.id().to_owned()),
                )
            })
            .collect(),
        ActiveProvider::YouTubeMusic => data
            .user_data
            .youtube_library
            .albums
            .iter()
            .map(|album| {
                HomeCard::context(
                    album.name.clone(),
                    format!("Album · {}", album.artist),
                    HistoryContext::YouTubeAlbum(album.id.clone()),
                )
            })
            .collect(),
    }
}

fn artist_cards(data: &AppData, provider: ActiveProvider) -> Vec<HomeCard> {
    match provider {
        ActiveProvider::Spotify => data
            .user_data
            .followed_artists
            .iter()
            .map(|artist| {
                HomeCard::context(
                    artist.name.clone(),
                    "Artist".to_owned(),
                    HistoryContext::SpotifyArtist(artist.id.id().to_owned()),
                )
            })
            .collect(),
        ActiveProvider::YouTubeMusic => data
            .user_data
            .youtube_library
            .artists
            .iter()
            .map(|artist| {
                HomeCard::context(
                    artist.name.clone(),
                    "Artist".to_owned(),
                    HistoryContext::YouTubeArtist(artist.id.clone()),
                )
            })
            .collect(),
    }
}

/// Keyboard selection on Home's shelves.
pub type HomePageUIState = crate::state::ShelfNav<HomeShelfKind>;

/// Home's shelves as the keyboard navigation sees them.
pub fn home_shelf_sizes(shelves: &[HomeShelf]) -> Vec<crate::state::ShelfSize<HomeShelfKind>> {
    shelves
        .iter()
        .map(|shelf| crate::state::ShelfSize {
            key: shelf.kind,
            len: shelf.cards.len(),
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::state::{ContextHistoryEntry, SessionEntry, UnifiedPlaylist};

    fn data(folder: &std::path::Path) -> AppData {
        AppData::new(folder, folder)
    }

    fn spotify(account: Option<&str>) -> HomeScope<'_> {
        HomeScope {
            provider: ActiveProvider::Spotify,
            account,
            spotify_ready: true,
        }
    }

    fn kinds(shelves: &[HomeShelf]) -> Vec<HomeShelfKind> {
        shelves.iter().map(|shelf| shelf.kind).collect()
    }

    fn shelf(shelves: &[HomeShelf], kind: HomeShelfKind) -> &HomeShelf {
        shelves.iter().find(|shelf| shelf.kind == kind).unwrap()
    }

    fn track(id: &str, name: &str) -> Track {
        Track {
            id: rspotify::model::TrackId::from_id(id).unwrap().into_static(),
            name: name.to_owned(),
            artists: Vec::new(),
            album: None,
            duration: Duration::from_secs(60),
            explicit: false,
            added_at: 0,
        }
    }

    fn unified(id: &str, name: &str) -> UnifiedPlaylist {
        UnifiedPlaylist {
            id: id.to_owned(),
            name: name.to_owned(),
            items: Vec::new(),
            updated_at: 0,
            next_entry_id: 1,
        }
    }

    #[test]
    fn an_empty_home_offers_liked_music_and_explains_continue() {
        let folder = tempfile::tempdir().unwrap();
        let scope = HomeScope {
            spotify_ready: false,
            ..spotify(None)
        };
        let shelves = home_shelves(&data(folder.path()), scope);

        assert_eq!(
            kinds(&shelves),
            [HomeShelfKind::QuickAccess, HomeShelfKind::Continue]
        );
        assert_eq!(
            shelves[0].cards[0].target,
            HomeTarget::Context(HistoryContext::SpotifyLikedTracks)
        );
        assert!(shelves[1].cards.is_empty());
        assert!(shelves[1].message.is_some());
    }

    #[test]
    fn continue_and_quick_access_follow_the_scoped_history_without_repeats() {
        let folder = tempfile::tempdir().unwrap();
        let mut data = data(folder.path());
        data.unified_playlists = vec![unified("u", "Road trip")];
        let playlist = unified("u", "Road trip");
        data.context_history
            .record(ContextHistoryEntry::from_unified(&playlist, 1));
        let mut other_account = ContextHistoryEntry::from_unified(&playlist, 2);
        other_account.context = HistoryContext::YouTubeAlbum("a".to_owned());
        other_account.namespace =
            crate::state::HistoryNamespace::for_context(&other_account.context, Some("other"));
        data.context_history.record(other_account);

        let scope = HomeScope {
            provider: ActiveProvider::YouTubeMusic,
            account: Some("me"),
            spotify_ready: true,
        };
        let shelves = home_shelves(&data, scope);
        let titles = |kind| -> Vec<String> {
            shelf(&shelves, kind)
                .cards
                .iter()
                .map(|card| card.title.clone())
                .collect()
        };
        assert_eq!(titles(HomeShelfKind::Continue), ["Road trip", "Show all"]);
        assert_eq!(
            titles(HomeShelfKind::QuickAccess),
            ["Liked Music", "Road trip"]
        );
        assert_eq!(
            home_shelf_list(&data, scope, HomeShelfKind::Continue).len(),
            1
        );
    }

    #[test]
    fn unified_playlists_get_their_own_shelf_under_every_provider() {
        let folder = tempfile::tempdir().unwrap();
        let mut data = data(folder.path());
        assert!(
            !kinds(&home_shelves(&data, spotify(None))).contains(&HomeShelfKind::UnifiedPlaylists)
        );

        data.unified_playlists = vec![unified("u", "Road trip")];
        for provider in [ActiveProvider::Spotify, ActiveProvider::YouTubeMusic] {
            let scope = HomeScope {
                provider,
                account: None,
                spotify_ready: false,
            };
            let shelves = home_shelves(&data, scope);
            let unified_shelf = shelf(&shelves, HomeShelfKind::UnifiedPlaylists);
            assert_eq!(unified_shelf.cards[0].title, "Road trip");
            assert!(!kinds(&shelves).contains(&HomeShelfKind::Playlists));
            assert_eq!(
                home_shelf_list(&data, scope, HomeShelfKind::UnifiedPlaylists).len(),
                1
            );
        }
    }

    #[test]
    fn spotify_shelves_show_loading_then_tracks_or_a_retry_per_source() {
        let folder = tempfile::tempdir().unwrap();
        let mut data = data(folder.path());
        let now = Instant::now();

        let loading = home_shelves(&data, spotify(Some("me")));
        assert_eq!(
            shelf(&loading, HomeShelfKind::RecentlyPlayed).status,
            HomeShelfStatus::Loading
        );

        let generation = data
            .home_feed
            .begin_refresh(Some("me"), now, false)
            .unwrap();
        let tracks = vec![
            track("track0000000000000000000000000001", "One"),
            track("track0000000000000000000000000001", "One again"),
            track("track0000000000000000000000000002", "Two"),
        ];
        assert!(data
            .home_feed
            .apply(generation, HomeFeedSource::RecentlyPlayed, Ok(tracks), now));
        assert!(data.home_feed.apply(
            generation,
            HomeFeedSource::TopTracks,
            Err(HomeFeedFailure::RequestLimit),
            now
        ));

        let shelves = home_shelves(&data, spotify(Some("me")));
        let recent = shelf(&shelves, HomeShelfKind::RecentlyPlayed);
        let titles: Vec<_> = recent.cards.iter().map(|c| c.title.as_str()).collect();
        assert_eq!(titles, ["One", "Two", "Show all"], "repeats collapse");
        let top = shelf(&shelves, HomeShelfKind::TopTracks);
        assert_eq!(
            top.status,
            HomeShelfStatus::Failed(HomeFeedFailure::RequestLimit)
        );
        assert_eq!(top.cards[0].subtitle, "Spotify request limit");
        assert_eq!(top.cards[0].target, HomeTarget::Retry);

        // Another account never sees this account's tracks.
        let other = home_shelves(&data, spotify(Some("other")));
        assert_eq!(
            shelf(&other, HomeShelfKind::RecentlyPlayed).status,
            HomeShelfStatus::Loading
        );
    }

    #[test]
    fn a_refresh_keeps_showing_the_last_tracks_until_it_succeeds() {
        let folder = tempfile::tempdir().unwrap();
        let mut data = data(folder.path());
        let now = Instant::now();
        let first = data.home_feed.begin_refresh(None, now, false).unwrap();
        let tracks = vec![track("track0000000000000000000000000001", "One")];
        data.home_feed
            .apply(first, HomeFeedSource::TopTracks, Ok(tracks), now);
        data.home_feed
            .apply(first, HomeFeedSource::RecentlyPlayed, Ok(Vec::new()), now);

        let later = now + HOME_FEED_TTL;
        let refresh = data.home_feed.begin_refresh(None, later, false).unwrap();
        let titles = |data: &AppData| {
            let shelves = home_shelves(data, spotify(None));
            let top = shelf(&shelves, HomeShelfKind::TopTracks);
            assert_eq!(top.status, HomeShelfStatus::Ready);
            top.cards
                .iter()
                .map(|card| card.title.clone())
                .collect::<Vec<_>>()
        };
        assert_eq!(titles(&data), ["One", "Show all"], "while refreshing");

        data.home_feed.apply(
            refresh,
            HomeFeedSource::TopTracks,
            Err(HomeFeedFailure::RequestLimit),
            later,
        );
        assert_eq!(titles(&data), ["One", "Show all"], "after a failed refresh");
    }

    #[test]
    fn a_response_for_an_older_fetch_or_account_is_dropped() {
        let mut feed = HomeFeed::default();
        let now = Instant::now();
        let first = feed.begin_refresh(Some("a"), now, false).unwrap();
        let second = feed.begin_refresh(Some("b"), now, false).unwrap();

        assert!(!feed.apply(first, HomeFeedSource::TopTracks, Ok(Vec::new()), now));
        assert!(feed.apply(second, HomeFeedSource::TopTracks, Ok(Vec::new()), now));
        assert_eq!(feed.top_tracks.status, HomeFeedStatus::Ready);
    }

    #[test]
    fn fetches_start_once_reuse_fresh_data_and_wait_for_retry_after_failure() {
        let mut feed = HomeFeed::default();
        let now = Instant::now();
        let generation = feed.begin_refresh(None, now, false).unwrap();
        assert!(
            feed.begin_refresh(None, now, false).is_none(),
            "already loading"
        );

        feed.apply(
            generation,
            HomeFeedSource::RecentlyPlayed,
            Ok(Vec::new()),
            now,
        );
        feed.apply(
            generation,
            HomeFeedSource::TopTracks,
            Err(HomeFeedFailure::RequestLimit),
            now,
        );
        assert!(
            feed.begin_refresh(None, now, false).is_none(),
            "fresh or failed"
        );
        assert!(
            feed.begin_refresh(None, now + HOME_FEED_TTL, false)
                .is_some(),
            "stale data refreshes"
        );

        let retry = feed.begin_refresh(None, now, true);
        assert!(retry.is_some(), "an explicit retry always fetches");
    }

    #[test]
    fn a_failed_read_retries_after_a_delay_unless_access_was_denied() {
        let now = Instant::now();
        let fail = |failure| {
            let mut feed = HomeFeed::default();
            let generation = feed.begin_refresh(None, now, false).unwrap();
            for source in [HomeFeedSource::RecentlyPlayed, HomeFeedSource::TopTracks] {
                feed.apply(generation, source, Err(failure), now);
            }
            feed
        };

        let mut limited = fail(HomeFeedFailure::RequestLimit);
        assert!(limited
            .begin_refresh(None, now + HOME_FEED_RETRY_DELAY / 2, false)
            .is_none());
        assert!(limited
            .begin_refresh(None, now + HOME_FEED_RETRY_DELAY, false)
            .is_some());

        let mut denied = fail(HomeFeedFailure::AccessDenied);
        assert!(denied
            .begin_refresh(None, now + HOME_FEED_TTL, false)
            .is_none());
        assert!(denied.begin_refresh(None, now, true).is_some());
    }

    #[test]
    fn youtube_recently_played_comes_from_local_history() {
        let folder = tempfile::tempdir().unwrap();
        let mut data = data(folder.path());
        let scope = HomeScope {
            provider: ActiveProvider::YouTubeMusic,
            account: None,
            spotify_ready: false,
        };
        assert!(!kinds(&home_shelves(&data, scope)).contains(&HomeShelfKind::RecentlyPlayed));

        for (index, id) in ["a", "b", "a"].into_iter().enumerate() {
            let track = YouTubeTrack {
                id: id.to_owned(),
                name: format!("Song {id}"),
                artists: "Artist".to_owned(),
                album: None,
                duration: "3:00".to_owned(),
                explicit: false,
                thumbnail_url: None,
                is_video: false,
            };
            data.session_history
                .record(SessionEntry::from_youtube_track(&track, index as u64), 100);
        }
        let shelves = home_shelves(&data, scope);
        let recent = shelf(&shelves, HomeShelfKind::RecentlyPlayed);
        let titles: Vec<_> = recent.cards.iter().map(|c| c.title.as_str()).collect();
        assert_eq!(titles, ["Song a", "Song b", "Show all"]);
        assert!(matches!(
            recent.cards[0].target,
            HomeTarget::Track(PlayableMedia::YouTube(_))
        ));
        assert!(!kinds(&shelves).contains(&HomeShelfKind::TopTracks));
    }
}
