//! Offline screen previews: the production workspace renderer driven by
//! synthetic state, with no provider sessions, network, or playback workers.

use std::{collections::VecDeque, fmt::Write as _, sync::Arc, time::Duration};

use anyhow::{bail, Context as _, Result};
use parking_lot::Mutex;
use ratatui::{
    backend::TestBackend,
    buffer::Buffer,
    style::{Color, Modifier},
    widgets::Block,
    Terminal,
};
use rspotify::prelude::Id;

use crate::{
    config::{ActiveProvider, Configs},
    state::{
        Album, Artist, Context, ContextId, ContextPageType, ContextPageUIState, PageState,
        Playlist, PlaylistFolderItem, SharedState, State, Track, UiViewStatus, YouTubeContext,
        YouTubeContextId, YouTubeContextPageUIState, YouTubeTrack,
    },
};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum PreviewScreen {
    Library,
    SpotifyPlaylist,
    SpotifyAlbum,
    SpotifyArtist,
    SpotifyShow,
    YouTubePlaylist,
    Queue,
    Home,
    Settings,
}

impl PreviewScreen {
    pub(crate) const NAMES: [&'static str; 9] = [
        "home",
        "library",
        "spotify-playlist",
        "spotify-album",
        "spotify-artist",
        "spotify-show",
        "youtube-playlist",
        "queue",
        "settings",
    ];

    pub(crate) fn from_cli(value: &str) -> Result<Self> {
        Ok(match value {
            "home" => Self::Home,
            "library" => Self::Library,
            "spotify-playlist" => Self::SpotifyPlaylist,
            "spotify-album" => Self::SpotifyAlbum,
            "spotify-artist" => Self::SpotifyArtist,
            "spotify-show" => Self::SpotifyShow,
            "youtube-playlist" => Self::YouTubePlaylist,
            "queue" => Self::Queue,
            "settings" => Self::Settings,
            _ => bail!("unknown preview screen {value:?}"),
        })
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum PreviewScenario {
    Ready,
    Loading,
    Partial,
    Failed,
    Empty,
    /// Ready, with a realistic catalog and every library collection loaded,
    /// for README screenshots and recordings.
    Showcase,
}

impl PreviewScenario {
    pub(crate) const NAMES: [&'static str; 6] =
        ["ready", "loading", "partial", "failed", "empty", "showcase"];

    pub(crate) fn from_cli(value: &str) -> Result<Self> {
        Ok(match value {
            "ready" => Self::Ready,
            "loading" => Self::Loading,
            "partial" => Self::Partial,
            "failed" => Self::Failed,
            "empty" => Self::Empty,
            "showcase" => Self::Showcase,
            _ => bail!("unknown preview scenario {value:?}"),
        })
    }
}

/// Parse `WIDTHxHEIGHT`, e.g. `80x24`.
pub(crate) fn preview_size_from_cli(value: &str) -> Result<(u16, u16)> {
    let (width, height) = value
        .split_once(['x', 'X'])
        .with_context(|| format!("size {value:?} must look like 80x24"))?;
    let width: u16 = width.trim().parse().context("invalid width")?;
    let height: u16 = height.trim().parse().context("invalid height")?;
    if !(20..=400).contains(&width) || !(6..=200).contains(&height) {
        bail!("size {value:?} is outside 20x6..400x200");
    }
    Ok((width, height))
}

const TRACK_COUNT: usize = 40;

const FAILED_STATUS: UiViewStatus = UiViewStatus::Failed {
    code: crate::state::CONTEXT_ERROR_CODE,
    message: crate::state::CONTEXT_ERROR_MESSAGE,
    next_action: crate::state::CONTEXT_ERROR_NEXT_ACTION,
};

const YOUTUBE_FAILED_STATUS: UiViewStatus = UiViewStatus::Failed {
    code: crate::state::YOUTUBE_CONTEXT_ERROR_CODE,
    message: crate::state::YOUTUBE_CONTEXT_ERROR_MESSAGE,
    next_action: crate::state::YOUTUBE_CONTEXT_ERROR_NEXT_ACTION,
};

const PARTIAL_STATUS: UiViewStatus = UiViewStatus::Partial {
    code: "PREVIEW_PARTIAL",
    message: "Some results could not be loaded.",
    next_action: "Open Diagnostics or retry the request.",
};

/// Spotify Home reads in the state each scenario describes.
fn seed_home_feed(state: &SharedState, scenario: PreviewScenario, catalog: DataSet) {
    use crate::state::{HomeFeedFailure, HomeFeedSource};
    let mut data = state.data.write();
    let now = std::time::Instant::now();
    let Some(generation) = data.home_feed.begin_refresh(None, now, true) else {
        return;
    };
    let (recent, top) = match scenario {
        PreviewScenario::Loading => return,
        PreviewScenario::Failed => (
            Err(HomeFeedFailure::Unavailable),
            Err(HomeFeedFailure::AccessDenied),
        ),
        PreviewScenario::Empty => (Ok(Vec::new()), Ok(Vec::new())),
        PreviewScenario::Partial => (
            Ok(spotify_tracks(catalog, 12)),
            Err(HomeFeedFailure::RequestLimit),
        ),
        PreviewScenario::Ready => (
            Ok(spotify_tracks(catalog, 12)),
            Ok(spotify_tracks(catalog, 8)),
        ),
        // Different slices, so the two shelves do not repeat each other.
        PreviewScenario::Showcase => (
            Ok(spotify_tracks(catalog, 12)),
            Ok(spotify_tracks(catalog, 20).split_off(9)),
        ),
    };
    data.home_feed
        .apply(generation, HomeFeedSource::RecentlyPlayed, recent, now);
    data.home_feed
        .apply(generation, HomeFeedSource::TopTracks, top, now);
}

/// Synthetic "Continue" entries for the Home preview.
fn seed_context_history(state: &SharedState, catalog: DataSet) {
    let mut data = state.data.write();
    for (index, name) in catalog.continue_names().into_iter().enumerate() {
        let mut entry = crate::state::ContextHistoryEntry::from_unified(
            &crate::state::UnifiedPlaylist {
                id: format!("preview-{index}"),
                name: name.to_owned(),
                items: Vec::new(),
                updated_at: 0,
                next_entry_id: 1,
            },
            0,
        );
        entry.context = crate::state::HistoryContext::SpotifyPlaylist(match catalog {
            DataSet::Preview => preview_id(name, 3),
            // Showcase entries open the loaded library playlist of that name.
            DataSet::Showcase => playlist(catalog, name).id.id().to_owned(),
        });
        catalog.continue_subtitle().clone_into(&mut entry.subtitle);
        entry.namespace = crate::state::HistoryNamespace::for_context(&entry.context, None);
        // Recording in memory only; previews never write user files.
        data.context_history.record(entry);
    }
}

fn preview_state(
    configs: &Configs,
    screen: PreviewScreen,
    scenario: PreviewScenario,
) -> SharedState {
    let ring = Arc::new(Mutex::new(VecDeque::new()));
    // The runtime half only drains diagnostics; previews never start it.
    let (diagnostics, _runtime) = crate::observability::disabled(ring);
    let state: SharedState = Arc::new(State::new_with_configs(false, diagnostics, configs));
    // Previews must not show, or write to, this machine's library, playlists
    // and history, so the app data reads from a folder that does not exist.
    let isolated = std::env::temp_dir().join(format!(
        "unified-player-screen-preview-{}",
        std::process::id()
    ));
    *state.data.write() = crate::state::AppData::new(&isolated, &isolated);
    let catalog = DataSet::of(scenario);
    let youtube_tracks = youtube_tracks(catalog, TRACK_COUNT);
    seed_library(&state, catalog);
    if scenario == PreviewScenario::Showcase {
        seed_showcase_contexts(&state, catalog);
        seed_showcase_youtube_library(&state);
        seed_showcase_devices(&state);
    }
    seed_playback(&state, &youtube_tracks, catalog);
    if scenario == PreviewScenario::Showcase {
        showcase_tick(&state, Duration::ZERO);
    }

    let mut ui = state.ui.lock();
    ui.spotify_account_label = Some(catalog.account_label().to_owned());
    ui.youtube_account_label = Some(catalog.account_label().to_owned());
    // Previews must not depend on the account registry of the machine.
    ui.spotify_account_id = None;
    ui.youtube_account_id = None;
    if scenario == PreviewScenario::Showcase {
        ui.youtube_auth_status.ready = true;
        ui.spotify_auth_status.session_ready = true;
    }
    ui.history.clear();
    let tracks = if scenario == PreviewScenario::Empty {
        Vec::new()
    } else {
        spotify_tracks(catalog, TRACK_COUNT)
    };
    let page = match screen {
        PreviewScreen::Library => PageState::Library {
            state: crate::state::LibraryPageUIState::new(),
        },
        PreviewScreen::Queue => PageState::new_queue(),
        PreviewScreen::Settings => {
            // Defaults from an empty folder, so the preview never shows this
            // machine's settings or accounts. Each call gets its own folder:
            // previews built in parallel would otherwise delete each other's.
            static NEXT_FOLDER: std::sync::atomic::AtomicUsize =
                std::sync::atomic::AtomicUsize::new(0);
            let folder = std::env::temp_dir().join(format!(
                "unified-player-settings-preview-{}-{}",
                std::process::id(),
                NEXT_FOLDER.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
            ));
            let settings = std::fs::create_dir_all(&folder)
                .map_err(anyhow::Error::from)
                .and_then(|()| crate::config::app_config_settings(&folder));
            let _ = std::fs::remove_dir_all(&folder);
            let settings = settings.expect("load default settings");
            let mut list = ratatui::widgets::ListState::default();
            list.select(Some(0));
            PageState::Settings {
                list,
                shelves: crate::state::SettingsShelves::default(),
                settings: if scenario == PreviewScenario::Empty {
                    Vec::new()
                } else {
                    settings
                },
                saved: false,
                error: None,
                notice: None,
            }
        }
        PreviewScreen::Home => {
            if scenario != PreviewScenario::Empty {
                seed_context_history(&state, catalog);
            }
            ui.spotify_auth_status.session_ready = true;
            seed_home_feed(&state, scenario, catalog);
            PageState::Home {
                state: crate::state::HomePageUIState::default(),
            }
        }
        PreviewScreen::SpotifyPlaylist => {
            let playlist = playlist(catalog, &catalog.playlists()[0]);
            spotify_context_page(
                &state,
                ContextId::Playlist(playlist.id.clone()),
                Context::Playlist { playlist, tracks },
                ContextPageUIState::new_playlist(),
                scenario,
            )
        }
        PreviewScreen::SpotifyAlbum => {
            let album = album(catalog, 0);
            spotify_context_page(
                &state,
                ContextId::Album(album.id.clone()),
                Context::Album { album, tracks },
                ContextPageUIState::new_album(),
                scenario,
            )
        }
        PreviewScreen::SpotifyArtist => {
            let page_artist = artist(&catalog.artists()[0]);
            // Liked songs are the user's saved tracks credited to this artist.
            state.data.write().user_data.saved_tracks = spotify_tracks(catalog, 6)
                .into_iter()
                .map(|mut track| {
                    track.id = rspotify::model::TrackId::from_id(preview_id(&track.name, 7))
                        .expect("preview track id")
                        .into_static();
                    track.artists = vec![page_artist.clone()];
                    (track.id.uri(), track)
                })
                .collect();
            spotify_context_page(
                &state,
                ContextId::Artist(page_artist.id.clone()),
                Context::Artist {
                    artist: page_artist,
                    top_tracks: tracks.into_iter().take(10).collect(),
                    listenbrainz: crate::state::ListenBrainzArtistEnrichment::default(),
                    albums: artist_albums(catalog),
                    related_artists: related_artists(catalog),
                },
                ContextPageUIState::new_artist(),
                scenario,
            )
        }
        PreviewScreen::SpotifyShow => {
            let show = show(catalog.show_name());
            let episodes = if scenario == PreviewScenario::Empty {
                Vec::new()
            } else {
                episodes(&show, 30)
            };
            spotify_context_page(
                &state,
                ContextId::Show(show.id.clone()),
                Context::Show { show, episodes },
                ContextPageUIState::new_show(),
                scenario,
            )
        }
        PreviewScreen::YouTubePlaylist => {
            ui.active_provider = ActiveProvider::YouTubeMusic;
            let mut page_state = YouTubeContextPageUIState::new();
            page_state.status = match scenario {
                PreviewScenario::Ready | PreviewScenario::Empty | PreviewScenario::Showcase => {
                    UiViewStatus::Ready
                }
                PreviewScenario::Loading => UiViewStatus::Loading,
                PreviewScenario::Partial => PARTIAL_STATUS,
                PreviewScenario::Failed => YOUTUBE_FAILED_STATUS,
            };
            let context = (scenario != PreviewScenario::Loading).then(|| YouTubeContext {
                title: catalog.mix_name().to_owned(),
                description: None,
                tracks: if scenario == PreviewScenario::Empty {
                    Vec::new()
                } else {
                    youtube_tracks.clone()
                },
                playlist_set_video_ids: Vec::new(),
                artist: None,
            });
            PageState::YouTubeContext {
                id: YouTubeContextId::LikedTracks,
                context,
                state: page_state,
            }
        }
    };
    ui.history.push(page);
    ui.sync_workspace_after_history_change();
    ui.current_page_mut().select(2);
    drop(ui);
    state
}

fn spotify_context_page(
    state: &SharedState,
    id: ContextId,
    context: Context,
    page_state: ContextPageUIState,
    scenario: PreviewScenario,
) -> PageState {
    match scenario {
        // Loading: the page exists but the context cache has not been filled yet.
        PreviewScenario::Loading => {}
        PreviewScenario::Failed => {
            return PageState::Context {
                id: Some(id.clone()),
                context_page_type: ContextPageType::Browsing(id),
                state: Some(ContextPageUIState::Failed {
                    status: FAILED_STATUS,
                }),
            };
        }
        PreviewScenario::Ready
        | PreviewScenario::Partial
        | PreviewScenario::Empty
        | PreviewScenario::Showcase => {
            state.data.write().caches.context.insert(
                id.uri(),
                context,
                *crate::state::TTL_CACHE_DURATION,
            );
        }
    }
    PageState::Context {
        id: Some(id.clone()),
        context_page_type: ContextPageType::Browsing(id),
        state: Some(page_state),
    }
}

fn library_names(first: &str, rest: &'static str) -> impl Iterator<Item = String> {
    std::iter::once(first.to_owned()).chain((1..=11).map(move |i| format!("{rest} {i}")))
}

/// The names synthetic state is built from. `Preview` exercises layout edge
/// cases (long names, numbering); `Showcase` reads like a real library.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum DataSet {
    Preview,
    Showcase,
}

const SHOWCASE_TITLES: [&str; 24] = [
    "Neon Harbor",
    "Paper Moons",
    "Static Bloom",
    "Slow Orbit",
    "Coastline Radio",
    "Velvet Hours",
    "Glasshouse",
    "Northbound",
    "Tidal Memory",
    "Saturday Static",
    "Lanterns",
    "Heat Mirage",
    "Small Hours",
    "Copper Skies",
    "Fieldnotes",
    "Afterimage",
    "Wildflower Code",
    "Midnight Ferry",
    "Low Light Waltz",
    "Signal Fire",
    "Open Water",
    "Polaroid Summer",
    "Quiet Machines",
    "Golden Hour Loop",
];
const SHOWCASE_ARTISTS: [&str; 7] = [
    "Lumen Drift",
    "Clara Vey",
    "The Night Office",
    "Kaito Mori",
    "Saffron Lane",
    "Juniper & Ash",
    "Tidewater",
];
const SHOWCASE_PLAYLISTS: [&str; 12] = [
    "Late Night Drive",
    "Deep Focus",
    "Sunday Morning",
    "Running Club",
    "Rainy Day Jazz",
    "Lo-fi Study",
    "Indie Mixtape",
    "Throwback Summer",
    "Dinner Party",
    "Chill Electronic",
    "Road Trip 2026",
    "Acoustic Evenings",
];
const SHOWCASE_ALBUMS: [&str; 8] = [
    "Afterglow Atlas",
    "Paper Moons",
    "Night Office Hours",
    "Slow Orbit",
    "Velvet Hours",
    "Copper Skies",
    "Signal Fire",
    "Open Water",
];

impl DataSet {
    fn of(scenario: PreviewScenario) -> Self {
        if scenario == PreviewScenario::Showcase {
            Self::Showcase
        } else {
            Self::Preview
        }
    }

    fn track_name(self, index: usize) -> String {
        match self {
            Self::Preview => format!("{} {}", TITLES[index % TITLES.len()], index + 1),
            Self::Showcase => SHOWCASE_TITLES[index % SHOWCASE_TITLES.len()].to_owned(),
        }
    }

    fn track_artist(self, index: usize) -> &'static str {
        match self {
            Self::Preview => ARTISTS[index % ARTISTS.len()],
            Self::Showcase => SHOWCASE_ARTISTS[index % SHOWCASE_ARTISTS.len()],
        }
    }

    fn playlists(self) -> Vec<String> {
        match self {
            Self::Preview => library_names("Preview playlist", "Playlist").collect(),
            Self::Showcase => SHOWCASE_PLAYLISTS.map(str::to_owned).to_vec(),
        }
    }

    fn albums(self) -> Vec<String> {
        match self {
            Self::Preview => library_names("Preview album", "Saved album").collect(),
            Self::Showcase => SHOWCASE_ALBUMS.map(str::to_owned).to_vec(),
        }
    }

    fn artists(self) -> Vec<String> {
        match self {
            Self::Preview => library_names("Preview artist", "Artist").collect(),
            Self::Showcase => SHOWCASE_ARTISTS.map(str::to_owned).to_vec(),
        }
    }

    fn continue_names(self) -> [&'static str; 4] {
        match self {
            Self::Preview => ["Night drive", "Morning focus", "Weekend mix", "Road trip"],
            Self::Showcase => [
                "Deep Focus",
                "Rainy Day Jazz",
                "Running Club",
                "Sunday Morning",
            ],
        }
    }

    fn continue_subtitle(self) -> &'static str {
        match self {
            Self::Preview => "Playlist · preview",
            Self::Showcase => "Playlist · Spotify",
        }
    }

    fn account_label(self) -> &'static str {
        match self {
            Self::Preview => "preview",
            Self::Showcase => "listener",
        }
    }

    fn show_name(self) -> &'static str {
        match self {
            Self::Preview => "Preview podcast",
            Self::Showcase => "Liner Notes",
        }
    }

    fn mix_name(self) -> &'static str {
        match self {
            Self::Preview => "Preview mix",
            Self::Showcase => "Liked music",
        }
    }
}

fn seed_library(state: &SharedState, catalog: DataSet) {
    // The opened collections are part of the library, as when opened from it.
    let mut data = state.data.write();
    data.user_data.playlists = catalog
        .playlists()
        .iter()
        .map(|name| PlaylistFolderItem::Playlist(playlist(catalog, name)))
        .collect();
    data.user_data.saved_albums = (0..catalog.albums().len())
        .map(|index| album(catalog, index))
        .collect();
    data.user_data.saved_shows = vec![show(catalog.show_name())];
    data.user_data.followed_artists = catalog.artists().iter().map(|name| artist(name)).collect();
}

/// Loads every library collection, so the interactive showcase can open any
/// of them without a provider.
/// The integrated player's default device name, as a real session shows it.
const SHOWCASE_DEVICE_NAME: &str = "unified-player";

/// Spotify Connect devices for the device picker: this player plus a few
/// fictional speakers.
fn seed_showcase_devices(state: &SharedState) {
    state.player.write().devices = [
        ("showcase-device", SHOWCASE_DEVICE_NAME, true),
        ("living-room", "Living Room Speaker", false),
        ("kitchen", "Kitchen Display", false),
        ("work-laptop", "Work Laptop", false),
    ]
    .into_iter()
    .map(|(id, name, is_integrated)| crate::state::Device {
        id: id.to_owned(),
        name: name.to_owned(),
        is_integrated,
    })
    .collect();
}

fn seed_showcase_contexts(state: &SharedState, catalog: DataSet) {
    let tracks = spotify_tracks(catalog, SHOWCASE_TITLES.len());
    let rotated = |offset: usize, count: usize| -> Vec<Track> {
        tracks
            .iter()
            .cycle()
            .skip(offset)
            .take(count)
            .cloned()
            .collect()
    };
    let mut contexts = Vec::new();
    for (index, name) in catalog.playlists().iter().enumerate() {
        let playlist = playlist(catalog, name);
        contexts.push((
            ContextId::Playlist(playlist.id.clone()),
            Context::Playlist {
                playlist,
                tracks: rotated(index * 5, 18 + index % 4 * 3),
            },
        ));
    }
    for index in 0..catalog.albums().len() {
        let album = album(catalog, index);
        let tracks = rotated(index * 3, 9)
            .into_iter()
            .map(|mut track| {
                track.album = Some(album.clone());
                track.artists.clone_from(&album.artists);
                track
            })
            .collect();
        contexts.push((
            ContextId::Album(album.id.clone()),
            Context::Album { album, tracks },
        ));
    }
    for (index, name) in catalog.artists().iter().enumerate() {
        let artist = artist(name);
        contexts.push((
            ContextId::Artist(artist.id.clone()),
            Context::Artist {
                top_tracks: rotated(index * 4, 10),
                listenbrainz: crate::state::ListenBrainzArtistEnrichment::default(),
                albums: artist_albums(catalog),
                related_artists: related_artists(catalog)
                    .into_iter()
                    .filter(|related| related.name != artist.name)
                    .collect(),
                artist,
            },
        ));
    }
    let mut data = state.data.write();
    for (id, context) in contexts {
        data.caches
            .context
            .insert(id.uri(), context, *crate::state::TTL_CACHE_DURATION);
    }
}

fn seed_showcase_youtube_library(state: &SharedState) {
    use crate::state::{
        YouTubeLibrary, YouTubeLibraryAlbum, YouTubeLibraryArtist, YouTubeLibraryPlaylist,
    };
    let mut data = state.data.write();
    data.user_data.youtube_library = YouTubeLibrary {
        loaded: true,
        playlists: [
            "Harbor After Dark",
            "Morning Signals",
            "Quiet Corners",
            "Night Bus Radio",
        ]
        .iter()
        .enumerate()
        .map(|(index, name)| YouTubeLibraryPlaylist {
            id: preview_id("youtube-playlist", index),
            name: (*name).to_owned(),
            author: "listener".to_owned(),
            tracks: "18 tracks".to_owned(),
            thumbnail_url: None,
        })
        .collect(),
        albums: SHOWCASE_ALBUMS
            .iter()
            .enumerate()
            .map(|(index, name)| YouTubeLibraryAlbum {
                id: preview_id("youtube-album", index),
                name: (*name).to_owned(),
                artist: DataSet::Showcase.track_artist(index).to_owned(),
                year: "2024".to_owned(),
                album_type: "Album".to_owned(),
                thumbnail_url: None,
            })
            .collect(),
        artists: SHOWCASE_ARTISTS
            .iter()
            .enumerate()
            .map(|(index, name)| YouTubeLibraryArtist {
                id: preview_id("youtube-artist", index),
                name: (*name).to_owned(),
                byline: "Artist".to_owned(),
            })
            .collect(),
        errors: Vec::new(),
    };
    let playlists = data.user_data.youtube_library.playlists.clone();
    for playlist in playlists.iter().take(3) {
        let id = YouTubeContextId::Playlist(playlist.id.clone());
        let context = showcase_youtube_context(&id, &data.user_data.youtube_library);
        data.context_history
            .record(crate::state::ContextHistoryEntry::from_youtube(
                &id,
                &context,
                None,
                crate::state::now_unix_secs(),
            ));
    }
}

fn showcase_youtube_context(
    id: &YouTubeContextId,
    library: &crate::state::YouTubeLibrary,
) -> YouTubeContext {
    let (title, offset) = match id {
        YouTubeContextId::LikedTracks => ("Liked Music".to_owned(), 0),
        YouTubeContextId::Playlist(id) => library
            .playlists
            .iter()
            .enumerate()
            .find(|(_, item)| &item.id == id)
            .map_or(("Harbor After Dark".to_owned(), 0), |(index, item)| {
                (item.name.clone(), index * 5)
            }),
        YouTubeContextId::Album(id) => library
            .albums
            .iter()
            .enumerate()
            .find(|(_, item)| &item.id == id)
            .map_or(("Afterglow Atlas".to_owned(), 0), |(index, item)| {
                (item.name.clone(), index * 3)
            }),
        YouTubeContextId::Artist(id) => library
            .artists
            .iter()
            .enumerate()
            .find(|(_, item)| &item.id == id)
            .map_or(("Lumen Drift".to_owned(), 0), |(index, item)| {
                (item.name.clone(), index * 4)
            }),
        YouTubeContextId::Podcast(_) => ("Liner Notes".to_owned(), 0),
    };
    YouTubeContext {
        title,
        description: Some("An offline collection from the fictional showcase catalog.".to_owned()),
        tracks: youtube_tracks(DataSet::Showcase, SHOWCASE_TITLES.len())
            .into_iter()
            .cycle()
            .skip(offset)
            .take(18)
            .collect(),
        ..YouTubeContext::default()
    }
}

fn showcase_spotify_search(query: &str) -> crate::state::SearchResults {
    let query = query.trim().to_lowercase();
    let matches = |value: &str| value.to_lowercase().contains(&query);
    let catalog = DataSet::Showcase;
    let page_show = show(catalog.show_name());
    crate::state::SearchResults {
        tracks: spotify_tracks(catalog, SHOWCASE_TITLES.len())
            .into_iter()
            .filter(|track| {
                matches(&format!(
                    "{} {} {}",
                    track.name,
                    track.artists_info(),
                    track.album.as_ref().map_or("", |album| album.name.as_str())
                ))
            })
            .collect(),
        albums: (0..SHOWCASE_ALBUMS.len())
            .map(|index| album(catalog, index))
            .filter(|album| {
                matches(&format!(
                    "{} {}",
                    album.name,
                    crate::utils::map_join(&album.artists, |artist| &artist.name, ", ")
                ))
            })
            .collect(),
        artists: SHOWCASE_ARTISTS
            .iter()
            .map(|name| artist(name))
            .filter(|artist| matches(&artist.name))
            .collect(),
        playlists: SHOWCASE_PLAYLISTS
            .iter()
            .map(|name| playlist(catalog, name))
            .filter(|playlist| matches(&playlist.name))
            .collect(),
        shows: matches(&page_show.name)
            .then_some(page_show.clone())
            .into_iter()
            .collect(),
        episodes: episodes(&page_show, 6)
            .into_iter()
            .enumerate()
            .map(|(index, mut episode)| {
                episode.name = format!("Liner Notes: {}", SHOWCASE_TITLES[index]);
                episode
            })
            .filter(|episode| matches(&episode.name))
            .collect(),
    }
}

fn showcase_youtube_search(
    query: &str,
    library: &crate::state::YouTubeLibrary,
) -> crate::state::YouTubeSearchResults {
    let query = query.trim().to_lowercase();
    let matches = |value: &str| value.to_lowercase().contains(&query);
    crate::state::YouTubeSearchResults {
        songs: youtube_tracks(DataSet::Showcase, SHOWCASE_TITLES.len())
            .into_iter()
            .filter(|track| matches(&format!("{} {}", track.name, track.artists)))
            .collect(),
        albums: library
            .albums
            .iter()
            .filter(|album| matches(&format!("{} {}", album.name, album.artist)))
            .cloned()
            .collect(),
        artists: library
            .artists
            .iter()
            .filter(|artist| matches(&artist.name))
            .cloned()
            .collect(),
        playlists: library
            .playlists
            .iter()
            .filter(|playlist| matches(&playlist.name))
            .cloned()
            .collect(),
        ..crate::state::YouTubeSearchResults::default()
    }
}

// These snapshots are consumed by the ordinary transport and queue renderers.
// Only the offline Showcase request loop calls these helpers.
#[allow(deprecated)]
fn showcase_start_current(state: &SharedState) {
    use crate::state::PlayableMedia;
    let mut player = state.player.write();
    let Some(media) = player
        .unified_queue
        .as_ref()
        .and_then(|q| q.current())
        .cloned()
    else {
        return;
    };
    let provider = match media {
        PlayableMedia::YouTube(track) => {
            player.youtube_playback = Some(crate::state::YouTubePlayback {
                track,
                is_playing: true,
                progress: Duration::ZERO,
                volume: 70,
                mute_state: None,
                route: crate::state::YouTubePlaybackRoute::default(),
            });
            ActiveProvider::YouTubeMusic
        }
        PlayableMedia::Spotify(id) => {
            let Some(track) = spotify_tracks(DataSet::Showcase, TRACK_COUNT)
                .into_iter()
                .find(|track| track.id.uri() == id.uri())
            else {
                return;
            };
            let artists = track
                .artists
                .iter()
                .map(|artist| rspotify::model::SimplifiedArtist {
                    id: Some(artist.id.clone()),
                    name: artist.name.clone(),
                    ..Default::default()
                })
                .collect();
            let album = track
                .album
                .as_ref()
                .map(|album| rspotify::model::SimplifiedAlbum {
                    id: Some(album.id.clone()),
                    name: album.name.clone(),
                    ..Default::default()
                })
                .unwrap_or_default();
            let item = rspotify::model::FullTrack {
                album,
                artists,
                available_markets: Vec::new(),
                disc_number: 1,
                duration: chrono::Duration::from_std(track.duration).expect("fixture duration"),
                explicit: track.explicit,
                external_ids: std::collections::HashMap::default(),
                external_urls: std::collections::HashMap::default(),
                href: None,
                id: Some(track.id),
                is_local: false,
                is_playable: Some(true),
                linked_from: None,
                restrictions: None,
                name: track.name,
                popularity: 0,
                preview_url: None,
                track_number: 1,
                r#type: rspotify::model::Type::Track,
            };
            let playback = rspotify::model::CurrentPlaybackContext {
                device: rspotify::model::Device {
                    id: Some("showcase-device".to_owned()),
                    is_active: true,
                    is_private_session: false,
                    is_restricted: false,
                    name: SHOWCASE_DEVICE_NAME.to_owned(),
                    _type: rspotify::model::DeviceType::Computer,
                    volume_percent: Some(70),
                },
                repeat_state: rspotify::model::RepeatState::Off,
                shuffle_state: false,
                context: None,
                timestamp: chrono::Utc::now(),
                progress: Some(chrono::Duration::zero()),
                is_playing: true,
                item: Some(rspotify::model::PlayableItem::Track(item)),
                currently_playing_type: rspotify::model::CurrentlyPlayingType::Track,
                actions: rspotify::model::Actions::default(),
            };
            player.buffered_playback =
                Some(crate::state::PlaybackMetadata::from_playback(&playback));
            player.playback = Some(playback);
            player.playback_last_updated_time = Some(std::time::Instant::now());
            ActiveProvider::Spotify
        }
    };
    player.active_playback_provider = Some(provider);
    player.active_playback_account_provider = Some(provider);
    player.active_playback_account_label = Some(DataSet::Showcase.account_label().to_owned());
}

fn showcase_start_items(
    state: &SharedState,
    items: Vec<crate::state::PlayableMedia>,
    start: usize,
) {
    if items.is_empty() {
        return;
    }
    {
        let mut ui = state.ui.lock();
        for track in spotify_tracks(DataSet::Showcase, TRACK_COUNT) {
            ui.spotify_queue_labels.remember_track(&track);
        }
    }
    state.player.write().unified_queue = Some(crate::state::UnifiedQueue::new(items, start));
    showcase_start_current(state);
}

fn showcase_navigate(state: &SharedState, next: bool) {
    let changed = state
        .player
        .write()
        .unified_queue
        .as_mut()
        .and_then(|queue| if next { queue.next() } else { queue.previous() })
        .is_some();
    if changed {
        showcase_start_current(state);
    }
}

fn showcase_control(state: &SharedState, playing: Option<bool>) {
    let mut player = state.player.write();
    if player.active_playback_provider == Some(ActiveProvider::Spotify) {
        let progress = player.playback_progress();
        if let Some(playback) = &mut player.playback {
            playback.progress = progress;
            playback.is_playing = playing.unwrap_or(!playback.is_playing);
            let is_playing = playback.is_playing;
            if let Some(buffered) = &mut player.buffered_playback {
                buffered.is_playing = is_playing;
            }
        }
        player.playback_last_updated_time = Some(std::time::Instant::now());
    } else if let Some(playback) = &mut player.youtube_playback {
        playback.is_playing = playing.unwrap_or(!playback.is_playing);
    }
}

fn showcase_tick(state: &SharedState, elapsed: Duration) {
    let mut player = state.player.write();
    if player.active_playback_provider == Some(ActiveProvider::YouTubeMusic) {
        if let Some(playback) = &mut player.youtube_playback {
            if playback.is_playing {
                playback.progress += elapsed;
            }
        }
    }
    drop(player);
    #[cfg(feature = "streaming")]
    showcase_bands(state);
}

fn showcase_lyrics(state: &SharedState, uri: String, source: Option<&str>) {
    // An original song, laid out as verse, chorus, verse, chorus, bridge,
    // chorus; an empty line separates the sections.
    const VERSE_1: [&str; 4] = [
        "Streetlights draw a map across the rain",
        "We fold the evening into paper planes",
        "A silver window catches every spark",
        "Our quiet footsteps measure out the dark",
    ];
    const CHORUS: [&str; 4] = [
        "So hold the harbor, hold the fading blue",
        "Every distant signal leads me back to you",
        "We keep the lantern burning on the sill",
        "Until the night is quiet, until the night is still",
    ];
    const VERSE_2: [&str; 4] = [
        "The last bus carries lanterns down the shore",
        "We leave a little light beside the door",
        "A rooftop garden listens to the sky",
        "The clocks slow down as satellites go by",
    ];
    const BRIDGE: [&str; 4] = [
        "And if the static swallows every word",
        "I'll hum the part that only you have heard",
        "We send tomorrow drifting through the air",
        "And find the morning waiting for us there",
    ];
    let sections = [VERSE_1, CHORUS, VERSE_2, CHORUS, BRIDGE, CHORUS];
    let mut seconds = 6;
    let mut lines = Vec::new();
    for (index, section) in sections.iter().enumerate() {
        if index > 0 {
            lines.push((chrono::Duration::seconds(seconds), String::new()));
            seconds += 2;
        }
        for line in section {
            lines.push((chrono::Duration::seconds(seconds), (*line).to_owned()));
            seconds += 4;
        }
    }
    let lyrics = crate::state::Lyrics {
        lines: crate::state::LyricsLines::Synced(lines),
        source: "Showcase · original lyrics".to_owned(),
    };
    state.data.write().caches.lyrics.insert(
        crate::state::LyricsCacheKey::new(&uri, source),
        Some(lyrics),
        *crate::state::TTL_CACHE_DURATION,
    );
    state
        .ui
        .lock()
        .set_lyrics_status(&uri, source, UiViewStatus::Ready);
}

#[cfg(feature = "streaming")]
fn showcase_bands(state: &SharedState) {
    let Some(bands) = &state.vis_bands else {
        return;
    };
    let player = state.player.read();
    let (playing, progress) = if player.active_playback_provider == Some(ActiveProvider::Spotify) {
        (
            player.playback.as_ref().is_some_and(|p| p.is_playing),
            player
                .playback_progress()
                .and_then(|p| p.to_std().ok())
                .unwrap_or_default(),
        )
    } else {
        player
            .youtube_playback
            .as_ref()
            .map(|p| (p.is_playing, p.progress))
            .unwrap_or_default()
    };
    drop(player);
    let mut bands = bands.lock();
    bands.is_active = playing;
    if !playing {
        return;
    }
    let phase = progress.as_secs_f32();
    // A music-like spectrum: falling treble, a 120 BPM kick on the bass, two
    // slowly wandering melody peaks, and per-frame jitter.
    let beat = (-(phase % 0.5) * 9.0).exp();
    let frame = (phase * 30.0) as u32;
    let count = bands.values.len() as f32;
    for (index, value) in bands.values.iter_mut().enumerate() {
        let x = index as f32 / count;
        let tilt = 0.5 * (1.0 - x).powf(2.2) + 0.08;
        let kick = beat * 0.55 * (-x * 12.0).exp();
        let peak = |centre: f32, width: f32| (-((x - centre) / width).powi(2)).exp();
        let melody = 0.32 * peak(0.28 + 0.08 * (phase * 0.9).sin(), 0.07)
            + 0.22 * peak(0.55 + 0.1 * (phase * 0.6).cos(), 0.05);
        let jitter = 0.35 + 1.1 * showcase_noise(index as u32, frame);
        // Squared, so the renderer's sqrt curve gives the intended contrast.
        *value = ((tilt + kick + melody) * jitter).clamp(0.0, 1.0).powi(2);
    }
    bands.peak_envelope = 1.0;
    bands.updated_at = std::time::Instant::now();
}

/// Deterministic noise in [0, 1) for a band and frame.
#[cfg(feature = "streaming")]
fn showcase_noise(band: u32, frame: u32) -> f32 {
    let mut hash = band.wrapping_mul(0x9E37_79B1) ^ frame.wrapping_mul(0x85EB_CA77);
    hash ^= hash >> 15;
    hash = hash.wrapping_mul(0x2C1B_3C6D);
    hash ^= hash >> 12;
    (hash & 0xFFFF) as f32 / 65_536.0
}

fn handle_showcase_request(state: &SharedState, request: crate::client::ClientRequest) {
    use crate::client::ClientRequest;
    match request {
        ClientRequest::GetLyrics { track_id } => showcase_lyrics(state, track_id.uri(), None),
        ClientRequest::GetLyricsFromProvider { track_id, provider } => {
            showcase_lyrics(state, track_id.uri(), Some(&provider));
        }
        ClientRequest::GetYouTubeLyrics(track) => {
            showcase_lyrics(state, format!("youtube:{}", track.id), None);
        }
        ClientRequest::GetYouTubeLyricsFromProvider { track, provider } => {
            showcase_lyrics(state, format!("youtube:{}", track.id), Some(&provider));
        }

        ClientRequest::PlayYouTubeContext {
            tracks,
            start_index,
        } => showcase_start_items(
            state,
            tracks
                .into_iter()
                .map(crate::state::PlayableMedia::YouTube)
                .collect(),
            start_index,
        ),
        ClientRequest::PlayUnifiedItems { items, start_index } => {
            showcase_start_items(state, items, start_index);
        }
        ClientRequest::Player(crate::client::PlayerRequest::StartPlayback(playback, _)) => {
            let (ids, offset) = match playback {
                crate::state::Playback::URIs(ids, offset) => (ids, offset),
                crate::state::Playback::Context(id, offset) => {
                    let ids = state
                        .data
                        .read()
                        .context_tracks(&id)
                        .cloned()
                        .unwrap_or_default()
                        .into_iter()
                        .map(|track| track.id.into())
                        .collect();
                    (ids, offset)
                }
            };
            let start = match offset {
                Some(rspotify::model::Offset::Position(index)) => {
                    usize::try_from(index.num_milliseconds()).unwrap_or_default()
                }
                Some(rspotify::model::Offset::Uri(uri)) => {
                    ids.iter().position(|id| id.uri() == uri).unwrap_or(0)
                }
                None => 0,
            };
            showcase_start_items(
                state,
                ids.into_iter()
                    .map(crate::state::PlayableMedia::Spotify)
                    .collect(),
                start,
            );
        }
        ClientRequest::AddItemsToUserQueue(items) => {
            state
                .player
                .write()
                .unified_queue
                .get_or_insert_with(crate::state::UnifiedQueue::empty)
                .enqueue_user(items);
        }
        ClientRequest::AddPlayableToQueue(id) => handle_showcase_request(
            state,
            ClientRequest::AddItemsToUserQueue(vec![crate::state::PlayableMedia::Spotify(id)]),
        ),
        ClientRequest::UnifiedNext
        | ClientRequest::Player(crate::client::PlayerRequest::NextTrack)
        | ClientRequest::YouTubePlayer(crate::client::YouTubePlayerRequest::Next) => {
            showcase_navigate(state, true);
        }
        ClientRequest::UnifiedPrevious
        | ClientRequest::Player(crate::client::PlayerRequest::PreviousTrack)
        | ClientRequest::YouTubePlayer(crate::client::YouTubePlayerRequest::Previous) => {
            showcase_navigate(state, false);
        }
        ClientRequest::ActivePlaybackControl(control) => showcase_control(
            state,
            match control {
                crate::client::ActivePlaybackControl::Play => Some(true),
                crate::client::ActivePlaybackControl::Pause => Some(false),
                crate::client::ActivePlaybackControl::Toggle => None,
            },
        ),
        ClientRequest::Player(crate::client::PlayerRequest::ResumePause) => {
            showcase_control(state, None);
        }
        ClientRequest::Player(crate::client::PlayerRequest::Resume)
        | ClientRequest::YouTubePlayer(crate::client::YouTubePlayerRequest::Resume) => {
            showcase_control(state, Some(true));
        }
        ClientRequest::Player(crate::client::PlayerRequest::Pause)
        | ClientRequest::YouTubePlayer(crate::client::YouTubePlayerRequest::Pause) => {
            showcase_control(state, Some(false));
        }
        ClientRequest::Search {
            query,
            lifecycle_reference,
        } => {
            let results = showcase_spotify_search(&query);
            let count = results.tracks.len()
                + results.albums.len()
                + results.artists.len()
                + results.playlists.len()
                + results.shows.len()
                + results.episodes.len();
            state.data.write().caches.search.insert(
                query.clone(),
                Arc::new(results),
                *crate::state::TTL_CACHE_DURATION,
            );
            state.ui.lock().finish_search_success(
                ActiveProvider::Spotify,
                &query,
                &lifecycle_reference,
                count,
            );
        }
        ClientRequest::SearchYouTube {
            query,
            lifecycle_reference,
        } => {
            let results =
                showcase_youtube_search(&query, &state.data.read().user_data.youtube_library);
            let count = results.songs.len()
                + results.albums.len()
                + results.artists.len()
                + results.playlists.len();
            state.data.write().caches.youtube_search.insert(
                query.clone(),
                Arc::new(results),
                *crate::state::TTL_CACHE_DURATION,
            );
            state.ui.lock().finish_search_success(
                ActiveProvider::YouTubeMusic,
                &query,
                &lifecycle_reference,
                count,
            );
        }
        ClientRequest::GetYouTubeLibrary => seed_showcase_youtube_library(state),
        ClientRequest::GetYouTubeContext(id) => {
            let context =
                showcase_youtube_context(&id, &state.data.read().user_data.youtube_library);
            let mut ui = state.ui.lock();
            for page in &mut ui.history {
                if let PageState::YouTubeContext {
                    id: page_id,
                    context: current,
                    state: page_state,
                } = page
                {
                    if *page_id == id {
                        *current = Some(context.clone());
                        page_state.status = UiViewStatus::Ready;
                    }
                }
            }
        }
        ClientRequest::SwitchProvider(provider) => {
            let mut ui = state.ui.lock();
            ui.active_provider = provider;
            ui.youtube_auth_status.ready = true;
        }
        _ => {}
    }
}

fn prepare_showcase_auth(configs: &Configs) -> Result<()> {
    // Production commands recheck file presence. The CLI gives every demo
    // fresh roots; this marker is never read by a live provider client.
    let path = configs.youtube_music_cookie_path();
    if !path.starts_with(&configs.config_folder) {
        bail!("showcase credential marker must stay inside its isolated config folder");
    }
    std::fs::create_dir_all(path.parent().context("showcase marker parent")?)
        .context("create showcase auth folder")?;
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&path)
        .context("create isolated showcase auth marker")?;
    std::io::Write::write_all(
        &mut file,
        b"Offline showcase marker; contains no credentials.\n",
    )
    .context("write showcase auth marker")?;
    Ok(())
}

fn artist_albums(catalog: DataSet) -> Vec<Album> {
    match catalog {
        DataSet::Preview => (1..=8)
            .map(|i| album_by(&format!("Album {i}"), "Preview artist"))
            .collect(),
        DataSet::Showcase => (0..SHOWCASE_ALBUMS.len())
            .map(|index| album(catalog, index))
            .collect(),
    }
}

fn related_artists(catalog: DataSet) -> Vec<Artist> {
    match catalog {
        DataSet::Preview => (1..=6).map(|i| artist(&format!("Related {i}"))).collect(),
        DataSet::Showcase => SHOWCASE_ARTISTS.iter().map(|name| artist(name)).collect(),
    }
}

fn seed_playback(state: &SharedState, tracks: &[YouTubeTrack], catalog: DataSet) {
    let mut player = state.player.write();
    player.active_playback_provider = Some(ActiveProvider::YouTubeMusic);
    if catalog == DataSet::Showcase {
        player.active_playback_account_provider = Some(ActiveProvider::YouTubeMusic);
        player.active_playback_account_label = Some(catalog.account_label().to_owned());
    }
    player.unified_queue = Some(crate::state::UnifiedQueue::new(
        tracks
            .iter()
            .take(8)
            .cloned()
            .map(crate::state::PlayableMedia::YouTube)
            .collect(),
        0,
    ));
    player.youtube_playback = tracks.first().map(|track| crate::state::YouTubePlayback {
        track: track.clone(),
        is_playing: true,
        progress: Duration::from_secs(74),
        volume: 70,
        mute_state: None,
        route: crate::state::YouTubePlaybackRoute::default(),
    });
}

/// Deterministic base62 id so previews never depend on real catalog ids.
fn preview_id(kind: &str, index: usize) -> String {
    const ALPHABET: &[u8] = b"0123456789ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz";
    let seed = kind.bytes().fold(index as u64 + 1, |acc, byte| {
        acc.wrapping_mul(131).wrapping_add(u64::from(byte))
    });
    (0..22)
        .map(|i| {
            let value = seed
                .wrapping_mul(6_364_136_223_846_793_005)
                .rotate_left(i * 3);
            ALPHABET[(value % 62) as usize] as char
        })
        .collect()
}

fn artist(name: &str) -> Artist {
    Artist {
        id: rspotify::model::ArtistId::from_id(preview_id(name, 0))
            .expect("preview artist id")
            .into_static(),
        name: name.to_owned(),
    }
}

/// The catalog's album at `index`, credited to one of its artists.
fn album(catalog: DataSet, index: usize) -> Album {
    let name = &catalog.albums()[index];
    match catalog {
        DataSet::Preview => album_by(name, "Preview artist"),
        DataSet::Showcase => album_by(name, catalog.track_artist(index)),
    }
}

fn album_by(name: &str, artist_name: &str) -> Album {
    Album {
        id: rspotify::model::AlbumId::from_id(preview_id(name, 0))
            .expect("preview album id")
            .into_static(),
        release_date: "2024-05-17".to_owned(),
        name: name.to_owned(),
        artists: vec![artist(artist_name)],
        typ: None,
        added_at: 0,
    }
}

fn show(name: &str) -> crate::state::Show {
    crate::state::Show {
        id: rspotify::model::ShowId::from_id(preview_id(name, 0))
            .expect("preview show id")
            .into_static(),
        name: name.to_owned(),
    }
}

fn episodes(show: &crate::state::Show, count: usize) -> Vec<crate::state::Episode> {
    (0..count)
        .map(|i| crate::state::Episode {
            id: rspotify::model::EpisodeId::from_id(preview_id("episode", i))
                .expect("preview episode id")
                .into_static(),
            name: format!("Episode {}: {}", count - i, TITLES[i % TITLES.len()]),
            description: String::new(),
            // One long-form episode exercises the widened time column.
            duration: Duration::from_secs(if i == 1 { 5_400 } else { 1_500 + i as u64 * 61 }),
            show: Some(show.clone()),
            release_date: format!("2024-{:02}-{:02}", 12 - i % 12, 28 - i % 27),
        })
        .collect()
}

fn playlist(catalog: DataSet, name: &str) -> Playlist {
    Playlist {
        id: rspotify::model::PlaylistId::from_id(preview_id(name, 0))
            .expect("preview playlist id")
            .into_static(),
        collaborative: false,
        name: name.to_owned(),
        owner: (
            match catalog {
                DataSet::Preview => "Preview",
                DataSet::Showcase => "listener",
            }
            .to_owned(),
            rspotify::model::UserId::from_id("preview")
                .expect("preview user id")
                .into_static(),
        ),
        desc: "Synthetic playlist for layout previews".to_owned(),
        current_folder_id: 0,
        snapshot_id: String::new(),
    }
}

const TITLES: [&str; 8] = [
    "Midnight Transit",
    "Paper Satellites",
    "A Very Long Song Title That Keeps Going Past Any Reasonable Column",
    "Glass",
    "Halcyon (Extended Mix)",
    "Signals",
    "Low Tide",
    "Kaleidoscope Hearts",
];
const ARTISTS: [&str; 4] = [
    "Northern Lines",
    "The Very Long Named Collective Orchestra",
    "Mira",
    "Parallel",
];

fn spotify_tracks(catalog: DataSet, count: usize) -> Vec<Track> {
    (0..count)
        .map(|i| Track {
            id: rspotify::model::TrackId::from_id(preview_id("track", i))
                .expect("preview track id")
                .into_static(),
            name: catalog.track_name(i),
            artists: vec![artist(catalog.track_artist(i))],
            album: Some(match catalog {
                DataSet::Preview => album(catalog, 0),
                DataSet::Showcase => album(catalog, i % SHOWCASE_ALBUMS.len()),
            }),
            duration: Duration::from_secs(150 + (i as u64 * 17) % 180),
            explicit: catalog == DataSet::Preview && i % 5 == 0,
            added_at: 0,
        })
        .collect()
}

fn youtube_tracks(catalog: DataSet, count: usize) -> Vec<YouTubeTrack> {
    (0..count)
        .map(|i| {
            let seconds = 150 + (i * 17) % 180;
            YouTubeTrack {
                id: preview_id("video", i),
                name: catalog.track_name(i),
                artists: catalog.track_artist(i).to_owned(),
                album: None,
                duration: format!("{}:{:02}", seconds / 60, seconds % 60),
                explicit: false,
                thumbnail_url: None,
                is_video: false,
            }
        })
        .collect()
}

fn draw(frame: &mut super::Frame, state: &SharedState, ui: &mut crate::state::UIStateGuard) {
    let rect = frame.area();
    frame.render_widget(Block::default().style(ui.theme.workspace_base()), rect);
    super::render_application(frame, state, ui, rect);
}

/// Render `screen` at each size and return the frames, separated by headings.
pub(crate) fn render_screen_preview(
    configs: &Configs,
    screen: PreviewScreen,
    scenario: PreviewScenario,
    sizes: &[(u16, u16)],
    color: bool,
) -> Result<String> {
    let mut output = String::new();
    for &(width, height) in sizes {
        let state = preview_state(configs, screen, scenario);
        let mut ui = state.ui.lock();
        let policy = super::LayoutPolicy::from_size(width, height);
        ui.orientation = policy.orientation;
        ui.layout_mode = policy.mode;
        let mut terminal = Terminal::new(TestBackend::new(width, height))?;
        terminal.draw(|frame| draw(frame, &state, &mut ui))?;
        writeln!(output, "── {width}x{height} ──")?;
        output.push_str(&buffer_text(terminal.backend().buffer(), color));
        output.push('\n');
    }
    Ok(output)
}

/// Open the preview in the current terminal, routing input through the
/// production event handlers. Showcase resolves fixture requests locally.
pub(crate) fn run_screen_preview_interactive(
    configs: &Configs,
    screen: PreviewScreen,
    scenario: PreviewScenario,
) -> Result<()> {
    crate::config::make_settings_read_only();
    if scenario == PreviewScenario::Showcase {
        prepare_showcase_auth(configs)?;
    }
    let state = preview_state(configs, screen, scenario);
    let mut terminal = super::init_interactive_demo_terminal()?;
    let run_result = run_preview_loop(&mut terminal, &state, scenario);
    let cleanup_result = super::clean_up(terminal);
    match (run_result, cleanup_result) {
        (Err(error), _) => Err(error),
        (Ok(()), Err(error)) => Err(error).context("restore terminal after screen preview"),
        (Ok(()), Ok(())) => Ok(()),
    }
}

fn run_preview_loop(
    terminal: &mut super::Terminal,
    state: &SharedState,
    scenario: PreviewScenario,
) -> Result<()> {
    let (client_pub, client_sub) = crate::client::client_request_channel();
    {
        let size = terminal.size()?;
        let policy = super::LayoutPolicy::from_size(size.width, size.height);
        let mut ui = state.ui.lock();
        ui.orientation = policy.orientation;
        ui.layout_mode = policy.mode;
    }
    let mut last_tick = std::time::Instant::now();
    while !state.shutdown_requested() && state.ui.lock().is_running {
        if scenario == PreviewScenario::Showcase {
            showcase_tick(state, last_tick.elapsed());
        }
        last_tick = std::time::Instant::now();
        terminal.draw(|frame| {
            let mut ui = state.ui.lock();
            draw(frame, state, &mut ui);
        })?;
        if !crossterm::event::poll(Duration::from_millis(250))? {
            continue;
        }
        let event = crossterm::event::read()?;
        if let Err(err) = crate::event::handle_terminal_event(&event, &client_pub, state) {
            crate::observability::log_safe_error!(
                warn,
                crate::observability::DiagnosticCode::TERMINAL_EVENT_HANDLE_FAILED,
                crate::observability::ErrorCategory::Contract,
                &err,
                "Failed to handle a screen preview event"
            );
        }
        if let Err(err) = crate::client::resolve_preview_page(state, &client_pub) {
            crate::observability::log_safe_error!(
                warn,
                crate::observability::DiagnosticCode::PLAYER_EVENT_HANDLE_FAILED,
                crate::observability::ErrorCategory::Contract,
                &err,
                "Failed to resolve a screen preview page"
            );
        }
        while let Ok(request) = client_sub.try_recv() {
            if scenario == PreviewScenario::Showcase {
                handle_showcase_request(state, request.request().clone());
            }
        }
    }
    Ok(())
}

fn buffer_text(buffer: &Buffer, color: bool) -> String {
    let area = buffer.area;
    let mut output = String::new();
    for y in area.top()..area.bottom() {
        let mut current = None;
        let mut line = String::new();
        for x in area.left()..area.right() {
            let cell = &buffer[(x, y)];
            if color {
                let style = (cell.fg, cell.bg, cell.modifier);
                if current != Some(style) {
                    line.push_str(&sgr(cell.fg, cell.bg, cell.modifier));
                    current = Some(style);
                }
            }
            line.push_str(cell.symbol());
        }
        if color {
            line.push_str("\x1b[0m");
        } else {
            line.truncate(line.trim_end().len());
        }
        output.push_str(&line);
        output.push('\n');
    }
    output
}

fn sgr(fg: Color, bg: Color, modifier: Modifier) -> String {
    let mut codes = vec!["0".to_owned()];
    for (flag, code) in [
        (Modifier::BOLD, "1"),
        (Modifier::DIM, "2"),
        (Modifier::ITALIC, "3"),
        (Modifier::UNDERLINED, "4"),
        (Modifier::REVERSED, "7"),
        (Modifier::CROSSED_OUT, "9"),
    ] {
        if modifier.contains(flag) {
            codes.push(code.to_owned());
        }
    }
    codes.extend(color_code(fg, false));
    codes.extend(color_code(bg, true));
    format!("\x1b[{}m", codes.join(";"))
}

fn color_code(color: Color, background: bool) -> Option<String> {
    let base: u8 = if background { 40 } else { 30 };
    let named = |offset: u8, bright: bool| {
        let base = if bright { base + 60 } else { base };
        Some((base + offset).to_string())
    };
    match color {
        Color::Reset => None,
        Color::Black => named(0, false),
        Color::Red => named(1, false),
        Color::Green => named(2, false),
        Color::Yellow => named(3, false),
        Color::Blue => named(4, false),
        Color::Magenta => named(5, false),
        Color::Cyan => named(6, false),
        Color::Gray => named(7, false),
        Color::DarkGray => named(0, true),
        Color::LightRed => named(1, true),
        Color::LightGreen => named(2, true),
        Color::LightYellow => named(3, true),
        Color::LightBlue => named(4, true),
        Color::LightMagenta => named(5, true),
        Color::LightCyan => named(6, true),
        Color::White => named(7, true),
        Color::Indexed(index) => Some(format!("{};5;{index}", base + 8)),
        Color::Rgb(r, g, b) => Some(format!("{};2;{r};{g};{b}", base + 8)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn collection_filter_rows_and_cursor_follow_the_visible_projection() {
        let configs = crate::ui::initialize_test_config();
        for (screen, playlist_context) in [
            (PreviewScreen::SpotifyPlaylist, false),
            (PreviewScreen::SpotifyAlbum, false),
            (PreviewScreen::YouTubePlaylist, false),
            (PreviewScreen::YouTubePlaylist, true),
        ] {
            for (width, height) in [(80, 24), (120, 35), (180, 49), (140, 40)] {
                let state = preview_state(configs, screen, PreviewScenario::Ready);
                let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
                let mut ui = state.ui.lock();
                if playlist_context {
                    let PageState::YouTubeContext { id, .. } = ui.current_page_mut() else {
                        unreachable!()
                    };
                    *id = YouTubeContextId::Playlist("fixture-playlist".to_owned());
                }
                terminal.draw(|frame| draw(frame, &state, &mut ui)).unwrap();
                ui.current_page_mut().select(0);
                ui.popup = Some(crate::state::PopupState::Search {
                    query: "Paper Satellites".to_owned(),
                });
                terminal.draw(|frame| draw(frame, &state, &mut ui)).unwrap();
                let text = buffer_text(terminal.backend().buffer(), false);
                assert!(
                    text.contains("Paper Satellites"),
                    "{screen:?}/{width}x{height}"
                );
                assert!(
                    text.contains("5 tracks shown"),
                    "{screen:?}/{width}x{height}: {text}"
                );
                assert!(
                    !text.contains("Halcyon"),
                    "unmatched row rendered: {screen:?}"
                );
                assert_eq!(ui.current_page().selected_index(), Some(0));
                ui.current_page_mut().select(1);
                terminal.draw(|frame| draw(frame, &state, &mut ui)).unwrap();
                drop(ui);
                let (client_pub, _client_sub) = crate::client::client_request_channel();
                crate::event::handle_terminal_event(
                    &crossterm::event::Event::Key(crossterm::event::KeyEvent::new(
                        crossterm::event::KeyCode::Esc,
                        crossterm::event::KeyModifiers::NONE,
                    )),
                    &client_pub,
                    &state,
                )
                .unwrap();
                let mut ui = state.ui.lock();
                terminal.draw(|frame| draw(frame, &state, &mut ui)).unwrap();
                assert!(buffer_text(terminal.backend().buffer(), false).contains("40 tracks shown"));
                assert_eq!(
                    ui.current_page().selected_index(),
                    Some(9),
                    "selected occurrence moved: {screen:?}"
                );
                ui.popup = Some(crate::state::PopupState::Search {
                    query: "no matching fixture".to_owned(),
                });
                terminal.draw(|frame| draw(frame, &state, &mut ui)).unwrap();
                let text = buffer_text(terminal.backend().buffer(), false);
                assert!(text.contains("No items"), "{screen:?}/{width}x{height}");
                assert!(text.contains("0 tracks shown"));
            }
        }
    }

    #[test]
    fn queue_uses_collection_spacing_and_keeps_scrolled_mouse_targets() {
        let configs = crate::ui::initialize_test_config();
        for name in ["default", "carbonfox", "catppuccin", "catppuccin-light"] {
            for (width, height) in [(80, 24), (120, 35), (180, 49), (140, 40)] {
                let state = preview_state(configs, PreviewScreen::Queue, PreviewScenario::Showcase);
                let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
                let mut ui = state.ui.lock();
                ui.theme = configs.theme_config.find_theme(name).unwrap();
                terminal.draw(|frame| draw(frame, &state, &mut ui)).unwrap();
                let text = buffer_text(terminal.backend().buffer(), false);
                assert!(
                    text.contains("8 From context"),
                    "{name}/{width}x{height}: {text}"
                );
                assert_eq!(text.matches("From context").count(), 1);
                assert!(!text.contains("Playback Queue"));
                assert!(!text.contains(">1"));
                assert!(text.contains("▶"));
                ui.current_page_mut().select(7);
                terminal.draw(|frame| draw(frame, &state, &mut ui)).unwrap();
                assert_eq!(ui.current_page().selected_index(), Some(7));
                let hit = ui
                    .workspace_hits
                    .iter()
                    .find(|(_, hit)| *hit == crate::state::WorkspaceHit::QueueRow(7))
                    .expect("last queue row is visible")
                    .0;
                assert_eq!(hit.right(), ui.workspace_layout.content.right() - 1);
                let text = buffer_text(terminal.backend().buffer(), false);
                assert!(text.contains("8 queued"));
            }
        }
    }

    #[test]
    fn popup_overlays_preserve_page_geometry_and_stay_above_transport() {
        let configs = crate::ui::initialize_test_config();
        for name in ["default", "carbonfox", "catppuccin", "catppuccin-light"] {
            for (width, height) in [(80, 24), (120, 35), (180, 49), (140, 40)] {
                let state = preview_state(configs, PreviewScreen::Queue, PreviewScenario::Showcase);
                let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
                let mut ui = state.ui.lock();
                ui.theme = configs.theme_config.find_theme(name).unwrap();
                ui.current_page_mut().select(7);
                terminal.draw(|frame| draw(frame, &state, &mut ui)).unwrap();
                let navigation = ui.workspace_layout.navigation;
                let content = ui.workspace_layout.content;
                let offset = match ui.current_page() {
                    PageState::Queue { table, .. } => table.offset(),
                    _ => unreachable!(),
                };
                for popup in [
                    crate::state::PopupState::Search {
                        query: "North".to_owned(),
                    },
                    crate::state::PopupState::ThemeList(
                        vec![ui.theme.clone(); 20],
                        ratatui::widgets::ListState::default(),
                    ),
                    crate::state::PopupState::CommandHelp { scroll_offset: 0 },
                    crate::state::PopupState::DeviceList(ratatui::widgets::ListState::default()),
                    crate::state::PopupState::DeferredAction {
                        title: "Unavailable action".to_owned(),
                        message: "Choose another item.".to_owned(),
                    },
                ] {
                    ui.popup = Some(popup);
                    terminal.draw(|frame| draw(frame, &state, &mut ui)).unwrap();
                    assert_eq!(ui.workspace_layout.navigation, navigation);
                    assert_eq!(ui.workspace_layout.content, content);
                    assert!(!ui.popup_rect.is_empty());
                    assert_eq!(ui.popup_rect.intersection(content), ui.popup_rect);
                    assert!(ui.popup_rect.intersection(navigation).is_empty());
                    assert_eq!(ui.current_page().selected_index(), Some(7));
                    match ui.current_page() {
                        PageState::Queue { table, .. } => assert_eq!(table.offset(), offset),
                        _ => unreachable!(),
                    }
                }
                ui.popup = None;
                terminal.draw(|frame| draw(frame, &state, &mut ui)).unwrap();
                assert!(ui.popup_rect.is_empty());
            }
        }
    }

    #[test]
    fn theme_picker_shows_palettes_current_theme_and_scrolled_hits() {
        let configs = crate::ui::initialize_test_config();
        for name in ["default", "carbonfox", "catppuccin", "catppuccin-light"] {
            for (width, height) in [(80, 24), (120, 35), (180, 49), (140, 40)] {
                let state = preview_state(
                    configs,
                    PreviewScreen::SpotifyPlaylist,
                    PreviewScenario::Showcase,
                );
                let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
                let mut ui = state.ui.lock();
                ui.theme = configs.theme_config.find_theme(name).unwrap();
                ui.open_theme_picker();
                terminal.draw(|frame| draw(frame, &state, &mut ui)).unwrap();
                let text = buffer_text(terminal.backend().buffer(), false);
                assert!(text.contains("Themes"));
                assert!(text.contains("(current)"));
                assert!(text.contains("██"));
                if height >= 35 {
                    assert!(ui.workspace_popup_hits.len() >= 10);
                }
                let first = ui.workspace_popup_hits[0].0;
                let buffer = terminal.backend().buffer();
                let colors = (first.x..first.right())
                    .filter_map(|x| {
                        (buffer[(x, first.y)].symbol() == "█").then_some(buffer[(x, first.y)].fg)
                    })
                    .collect::<std::collections::HashSet<_>>();
                assert!(colors.len() >= 2, "{name}/{width}x{height}");
                let total = match ui.popup.as_ref().unwrap() {
                    crate::state::PopupState::ThemeList(themes, _) => themes.len(),
                    _ => unreachable!(),
                };
                ui.popup
                    .as_mut()
                    .unwrap()
                    .list_state_mut()
                    .unwrap()
                    .select(Some(total - 1));
                terminal.draw(|frame| draw(frame, &state, &mut ui)).unwrap();
                assert!(ui
                    .workspace_popup_hits
                    .iter()
                    .any(|(_, index)| *index == total - 1));
                for (hit, index) in &ui.workspace_popup_hits {
                    assert_eq!(ui.workspace_popup_hit_at(hit.x, hit.y), Some(*index));
                    assert_eq!(ui.workspace_popup_hit_at(hit.right(), hit.y), None);
                }
            }
        }
    }

    #[test]
    fn command_help_keys_labels_and_scrolling_work_at_workspace_sizes() {
        let configs = crate::ui::initialize_test_config();
        let total = configs.keymap_config.resolved_bindings().len();
        for name in ["default", "carbonfox", "catppuccin", "catppuccin-light"] {
            for (width, height) in [(80, 24), (120, 35), (180, 49), (140, 40)] {
                let state = preview_state(
                    configs,
                    PreviewScreen::SpotifyPlaylist,
                    PreviewScenario::Showcase,
                );
                let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
                let mut ui = state.ui.lock();
                ui.theme = configs.theme_config.find_theme(name).unwrap();
                ui.popup = Some(crate::state::PopupState::CommandHelp { scroll_offset: 0 });
                terminal.draw(|frame| draw(frame, &state, &mut ui)).unwrap();
                let text = buffer_text(terminal.backend().buffer(), false);
                assert!(text.contains("Commands"));
                assert!(text.contains("Next track"));
                assert!(!text.contains("Command:"));
                assert!(!text.contains("[n]"));
                let hit = ui.workspace_popup_hits[0].0;
                let first = (hit.x..hit.right())
                    .map(|x| terminal.backend().buffer()[(x, hit.y)].symbol())
                    .collect::<String>();
                assert!(
                    first.trim_start().trim_start_matches("> ").starts_with('n'),
                    "{name}/{width}x{height}: {first}"
                );
                ui.popup = Some(crate::state::PopupState::CommandHelp {
                    scroll_offset: total - 1,
                });
                terminal.draw(|frame| draw(frame, &state, &mut ui)).unwrap();
                assert!(ui
                    .workspace_popup_hits
                    .iter()
                    .any(|(_, index)| *index == total - 1));
                let hits = ui.workspace_popup_hits.clone();
                terminal.draw(|frame| draw(frame, &state, &mut ui)).unwrap();
                assert_eq!(ui.workspace_popup_hits, hits);
            }
        }
    }

    #[test]
    fn popup_groups_keep_owned_surfaces_and_list_indices_at_workspace_sizes() {
        use crate::state::{PlaylistCreateCurrentField, PlaylistCreateTarget, PopupState};
        use crate::ui::single_line_input::LineInput;
        use ratatui::{layout::Rect, widgets::ListState};
        let configs = crate::ui::initialize_test_config();
        for name in ["default", "carbonfox", "catppuccin", "catppuccin-light"] {
            for (width, height) in [(80, 24), (120, 35), (180, 49), (140, 40)] {
                let state = preview_state(
                    configs,
                    PreviewScreen::SpotifyPlaylist,
                    PreviewScenario::Showcase,
                );
                state.player.write().devices = (0..8)
                    .map(|index| crate::state::Device {
                        id: format!("fixture-{index}"),
                        name: format!("Desk {index}"),
                        is_integrated: false,
                    })
                    .collect();
                let track = youtube_tracks(DataSet::Showcase, 1).remove(0);
                let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
                let mut ui = state.ui.lock();
                ui.theme = configs.theme_config.find_theme(name).unwrap();
                terminal.draw(|frame| draw(frame, &state, &mut ui)).unwrap();
                let body = crate::ui::LayoutPolicy::from_size(width, height)
                    .workspace_frame(Rect::new(0, 0, width, height))
                    .body;
                let anchor = Rect::new(body.right().saturating_sub(10), body.bottom(), 8, 1);
                let content = ui.workspace_layout.content;
                for popup in [
                    PopupState::DeviceList(ListState::default().with_selected(Some(7))),
                    PopupState::ConfigChoice {
                        key: "presentation.focused_row_overflow".to_owned(),
                        options: (0..20)
                            .map(|index| format!("Fixture choice {index}"))
                            .collect(),
                        state: ListState::default().with_selected(Some(19)),
                    },
                    PopupState::ConfigEdit {
                        key: "theme".to_owned(),
                        input: LineInput::new("a readable value".chars().collect()),
                    },
                    PopupState::SpotifyUserSearch {
                        query: "Fictional listener".to_owned(),
                    },
                    PopupState::DiagnosticDetail {
                        title: "Worker state".to_owned(),
                        lines: vec!["Synthetic worker ready".to_owned(); 10],
                        scroll_offset: 0,
                    },
                    PopupState::ActionList(
                        Box::new(crate::state::ActionListItem::YouTubeTrack(
                            track.clone(),
                            vec![
                                crate::command::Action::AddToQueue,
                                crate::command::Action::GoToArtist,
                            ],
                        )),
                        ListState::default(),
                    ),
                    PopupState::Volume {
                        anchor,
                        input: LineInput::default(),
                    },
                    PopupState::PlaylistCreate {
                        target: PlaylistCreateTarget::Unified,
                        public: false,
                        name: LineInput::new("Night Drive".chars().collect()),
                        desc: LineInput::default(),
                        current_field: PlaylistCreateCurrentField::Name,
                        pending_items: None,
                        source_provider: None,
                        source_epoch: None,
                    },
                    PopupState::UserSavedAlbumList(ListState::default()),
                ] {
                    ui.popup = Some(popup);
                    terminal.draw(|frame| draw(frame, &state, &mut ui)).unwrap();
                    assert_eq!(ui.workspace_layout.content, content);
                    assert!(
                        !ui.popup_rect.is_empty(),
                        "{name}/{width}x{height}: {:?}",
                        ui.popup
                    );
                    assert_eq!(ui.popup_rect.intersection(body), ui.popup_rect);
                    for (hit, index) in &ui.workspace_popup_hits {
                        assert_eq!(hit.intersection(ui.popup_rect), *hit);
                        assert_eq!(ui.workspace_popup_hit_at(hit.x, hit.y), Some(*index));
                        assert_eq!(ui.workspace_popup_hit_at(hit.right(), hit.y), None);
                    }
                    if matches!(ui.popup, Some(PopupState::DeviceList(..))) {
                        assert!(ui.workspace_popup_hits.iter().any(|(_, index)| *index == 7));
                        assert!(buffer_text(terminal.backend().buffer(), false).contains("Desk 7"));
                    }
                    if matches!(ui.popup, Some(PopupState::ConfigChoice { .. })) {
                        assert!(ui
                            .workspace_popup_hits
                            .iter()
                            .any(|(_, index)| *index == 19));
                    }
                }
            }
        }
    }

    #[test]
    fn artist_sections_share_collection_stride_and_preserve_cursors() {
        use crate::state::{ArtistFocusState, ContextPageUIState, WorkspaceHit};
        let configs = crate::ui::initialize_test_config();
        for name in ["default", "carbonfox", "catppuccin", "catppuccin-light"] {
            for (width, height) in [(80, 24), (120, 35), (180, 49), (140, 40)] {
                let state = preview_state(
                    configs,
                    PreviewScreen::SpotifyArtist,
                    PreviewScenario::Showcase,
                );
                let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
                let mut ui = state.ui.lock();
                ui.theme = configs.theme_config.find_theme(name).unwrap();
                for current in [
                    ArtistFocusState::TopTracks,
                    ArtistFocusState::LikedSongs,
                    ArtistFocusState::Albums,
                    ArtistFocusState::RelatedArtists,
                ] {
                    if let PageState::Context {
                        state: Some(ContextPageUIState::Artist { focus, .. }),
                        ..
                    } = ui.current_page_mut()
                    {
                        *focus = current;
                    }
                    ui.current_page_mut().select(0);
                    terminal.draw(|frame| draw(frame, &state, &mut ui)).unwrap();
                    let row = |index| {
                        ui.workspace_hits
                            .iter()
                            .rev()
                            .find(|(_, hit)| {
                                *hit == WorkspaceHit::ArtistRow {
                                    focus: current,
                                    index,
                                }
                            })
                            .unwrap()
                            .0
                    };
                    assert_eq!(
                        row(1).y - row(0).y,
                        if height >= 32 { 2 } else { 1 },
                        "{name}/{width}x{height}/{current:?}"
                    );
                    let content = ui.workspace_layout.content;
                    for (hit, _) in ui
                        .workspace_hits
                        .iter()
                        .filter(|(_, hit)| matches!(hit, WorkspaceHit::ArtistRow { .. }))
                    {
                        assert_eq!(hit.intersection(content), *hit);
                    }
                    ui.current_page_mut().select(2);
                    terminal.draw(|frame| draw(frame, &state, &mut ui)).unwrap();
                    assert_eq!(ui.current_page().selected_index(), Some(2));
                }
                for current in [
                    ArtistFocusState::TopTracks,
                    ArtistFocusState::LikedSongs,
                    ArtistFocusState::Albums,
                    ArtistFocusState::RelatedArtists,
                ] {
                    if let PageState::Context {
                        state: Some(ContextPageUIState::Artist { focus, .. }),
                        ..
                    } = ui.current_page_mut()
                    {
                        *focus = current;
                    }
                    terminal.draw(|frame| draw(frame, &state, &mut ui)).unwrap();
                    assert_eq!(ui.current_page().selected_index(), Some(2));
                }
            }
        }
    }

    #[test]
    fn artist_keyboard_focus_visits_sections_without_resetting_their_cursors() {
        use crate::state::{ArtistFocusState, ContextPageUIState, WorkspaceFocusState};
        let configs = crate::ui::initialize_test_config();
        let state = preview_state(
            configs,
            PreviewScreen::SpotifyArtist,
            PreviewScenario::Showcase,
        );
        let mut ui = state.ui.lock();
        if let PageState::Context {
            state:
                Some(ContextPageUIState::Artist {
                    top_track_table,
                    liked_track_table,
                    album_table,
                    related_artist_list,
                    ..
                }),
            ..
        } = ui.current_page_mut()
        {
            top_track_table.select(Some(2));
            liked_track_table.select(Some(3));
            album_table.select(Some(4));
            related_artist_list.select(Some(5));
        }
        for wide in [false, true] {
            ui.workspace_layout.show_right = wide;
            let mut seen = Vec::new();
            let count = if wide { 7 } else { 5 };
            for _ in 0..count {
                assert!(ui.focus_workspace(true));
                if ui.workspace_focus == WorkspaceFocusState::Context {
                    if let PageState::Context {
                        state: Some(ContextPageUIState::Artist { focus, .. }),
                        ..
                    } = ui.current_page()
                    {
                        seen.push(*focus);
                    }
                }
            }
            assert_eq!(
                seen,
                vec![
                    ArtistFocusState::LikedSongs,
                    ArtistFocusState::Albums,
                    ArtistFocusState::RelatedArtists,
                    ArtistFocusState::TopTracks
                ]
            );
            assert_eq!(ui.current_page().selected_index(), Some(2));
            for _ in 0..count {
                assert!(ui.focus_workspace(false));
            }
            assert_eq!(ui.current_page().selected_index(), Some(2));
        }
        if let PageState::Context {
            state:
                Some(ContextPageUIState::Artist {
                    top_track_table,
                    liked_track_table,
                    album_table,
                    related_artist_list,
                    ..
                }),
            ..
        } = ui.current_page()
        {
            assert_eq!(top_track_table.selected(), Some(2));
            assert_eq!(liked_track_table.selected(), Some(3));
            assert_eq!(album_table.selected(), Some(4));
            assert_eq!(related_artist_list.selected(), Some(5));
        }
        let mut terminal = Terminal::new(TestBackend::new(140, 40)).unwrap();
        ui.popup = Some(crate::state::PopupState::Search {
            query: "no artist fixture match".to_owned(),
        });
        terminal.draw(|frame| draw(frame, &state, &mut ui)).unwrap();
        assert!(buffer_text(terminal.backend().buffer(), false).contains("No items were found."));
    }

    #[test]
    fn every_screen_and_scenario_renders_at_small_and_canonical_sizes() {
        let configs = crate::ui::initialize_test_config();
        for screen in PreviewScreen::NAMES {
            for scenario in PreviewScenario::NAMES {
                let output = render_screen_preview(
                    configs,
                    PreviewScreen::from_cli(screen).unwrap(),
                    PreviewScenario::from_cli(scenario).unwrap(),
                    &[(60, 20), (180, 49)],
                    false,
                )
                .unwrap();
                assert!(output.contains("── 60x20 ──"), "{screen}/{scenario}");
                assert!(output.contains("── 180x49 ──"), "{screen}/{scenario}");
            }
        }
    }

    #[test]
    fn ready_collections_show_synthetic_rows() {
        let configs = crate::ui::initialize_test_config();
        for screen in [
            PreviewScreen::SpotifyPlaylist,
            PreviewScreen::YouTubePlaylist,
        ] {
            let output =
                render_screen_preview(configs, screen, PreviewScenario::Ready, &[(100, 30)], false)
                    .unwrap();
            assert!(output.contains("Midnight Transit 1"), "{screen:?}");
            assert!(output.contains("40 tracks shown"), "{screen:?}");
        }
    }

    #[test]
    fn loading_and_failed_collections_keep_the_workspace_heading() {
        let configs = crate::ui::initialize_test_config();
        let render = |screen, scenario| {
            render_screen_preview(configs, screen, scenario, &[(90, 14), (140, 40)], false).unwrap()
        };
        for screen in [
            PreviewScreen::SpotifyPlaylist,
            PreviewScreen::SpotifyAlbum,
            PreviewScreen::SpotifyShow,
        ] {
            let loading = render(screen, PreviewScenario::Loading);
            let failed = render(screen, PreviewScenario::Failed);
            for output in [&loading, &failed] {
                // The library already knows the name; the legacy bordered page never draws.
                assert!(output.contains("Preview "), "{screen:?}");
                assert!(output.contains("Spotify · preview"), "{screen:?}");
                assert!(!output.contains('┌'), "{screen:?}");
            }
            assert!(loading.contains("Loading..."), "{screen:?}");
            assert!(
                failed.contains(crate::state::CONTEXT_ERROR_NEXT_ACTION),
                "{screen:?}"
            );
        }

        let loading = render(PreviewScreen::YouTubePlaylist, PreviewScenario::Loading);
        assert!(loading.contains("Loading..."));
        let failed = render(PreviewScreen::YouTubePlaylist, PreviewScenario::Failed);
        assert!(failed.contains(crate::state::YOUTUBE_CONTEXT_ERROR_MESSAGE));
        assert!(failed.contains(crate::state::YOUTUBE_CONTEXT_ERROR_NEXT_ACTION));
        // A failed load hides the cached context, its title included.
        assert!(!failed.contains("Preview mix"));
    }

    #[test]
    fn artist_section_cards_preserve_the_cursor_when_clicked() {
        use crate::state::{ArtistFocusState, ContextPageUIState, WorkspaceHit};
        let configs = crate::ui::initialize_test_config();
        let state = preview_state(
            configs,
            PreviewScreen::SpotifyArtist,
            PreviewScenario::Ready,
        );
        let mut terminal = Terminal::new(TestBackend::new(100, 30)).unwrap();
        {
            let mut ui = state.ui.lock();
            if let PageState::Context {
                state: Some(crate::state::ContextPageUIState::Artist { album_table, .. }),
                ..
            } = ui.current_page_mut()
            {
                album_table.select(Some(1));
            }
            terminal.draw(|frame| draw(frame, &state, &mut ui)).unwrap();
        }
        let text = buffer_text(terminal.backend().buffer(), false);
        for title in ["Top tracks", "Liked songs", "Albums", "Related artists"] {
            assert!(text.contains(title), "missing section {title:?}");
        }
        assert!(
            !text.contains('┌'),
            "the legacy bordered artist page rendered"
        );

        let album_row = {
            let ui = state.ui.lock();
            ui.workspace_hit_rect(WorkspaceHit::ArtistRow {
                focus: ArtistFocusState::Albums,
                index: 1,
            })
            .expect("the Albums card retains its section cursor")
        };
        let (client_pub, _client_sub) = crate::client::client_request_channel();
        crate::event::handle_terminal_event(
            &crossterm::event::Event::Mouse(crossterm::event::MouseEvent {
                kind: crossterm::event::MouseEventKind::Down(crossterm::event::MouseButton::Left),
                column: album_row.x + 4,
                row: album_row.y,
                modifiers: crossterm::event::KeyModifiers::NONE,
            }),
            &client_pub,
            &state,
        )
        .unwrap();

        let ui = state.ui.lock();
        let crate::state::PageState::Context {
            state:
                Some(ContextPageUIState::Artist {
                    focus, album_table, ..
                }),
            ..
        } = ui.current_page()
        else {
            panic!("the artist page stays open after a single click");
        };
        assert_eq!(*focus, ArtistFocusState::Albums);
        assert_eq!(album_table.selected(), Some(1));
    }

    #[test]
    fn podcast_pages_use_the_collection_table_with_release_dates() {
        let configs = crate::ui::initialize_test_config();
        let output = render_screen_preview(
            configs,
            PreviewScreen::SpotifyShow,
            PreviewScenario::Ready,
            &[(120, 35)],
            false,
        )
        .unwrap();
        assert!(output.contains("Preview podcast"));
        assert!(output.contains("Released"));
        assert!(output.contains("30 episodes shown"));
        // The 90-minute episode keeps its hours instead of being cut to five cells.
        assert!(output.contains("1:30:00"));
        assert!(!output.contains('┌'), "the legacy bordered page rendered");
    }

    #[test]
    fn unresolved_context_pages_show_loading_or_nothing_playing() {
        let configs = crate::ui::initialize_test_config();
        let state = preview_state(configs, PreviewScreen::Library, PreviewScenario::Ready);
        let render = |page: PageState| {
            let mut ui = state.ui.lock();
            ui.history.push(page);
            ui.sync_workspace_after_history_change();
            let mut terminal = Terminal::new(TestBackend::new(100, 20)).unwrap();
            terminal.draw(|frame| draw(frame, &state, &mut ui)).unwrap();
            buffer_text(terminal.backend().buffer(), false)
        };

        // Just opened from the library: the id is known, the page state is not yet.
        let playlist_id = ContextId::Playlist(playlist(DataSet::Preview, "Preview playlist").id);
        let opening = render(PageState::Context {
            id: None,
            context_page_type: ContextPageType::Browsing(playlist_id),
            state: None,
        });
        assert!(opening.contains("Preview playlist"));
        assert!(opening.contains("Loading..."));
        assert!(!opening.contains('┌'));

        // Current Playing without a Spotify playback context never resolves.
        let idle = render(PageState::Context {
            id: None,
            context_page_type: ContextPageType::CurrentPlaying,
            state: None,
        });
        assert!(idle.contains("Nothing is playing."));
        assert!(!idle.contains("Loading..."));
        assert!(!idle.contains('┌'));
    }

    #[test]
    fn showcase_uses_its_own_catalog_and_opens_library_collections() {
        let configs = crate::ui::initialize_test_config();
        let output = render_screen_preview(
            configs,
            PreviewScreen::Home,
            PreviewScenario::Showcase,
            &[(120, 34)],
            false,
        )
        .unwrap();
        assert!(output.contains("Late Night Drive"));
        assert!(output.contains("Neon Harbor"));
        assert!(!output.contains("Preview"));
        assert!(!output.contains("account unavailable"));
        // The duration and the playback scope stay apart at medium widths.
        assert!(output.contains("2:30  YouTube Music / listener"));

        let state = preview_state(configs, PreviewScreen::Library, PreviewScenario::Showcase);
        let data = state.data.read();
        for name in SHOWCASE_PLAYLISTS {
            let id = ContextId::Playlist(playlist(DataSet::Showcase, name).id);
            assert!(data.caches.context.contains_key(&id.uri()), "{name}");
        }
        for index in 0..SHOWCASE_ALBUMS.len() {
            let id = ContextId::Album(album(DataSet::Showcase, index).id);
            assert!(data.caches.context.contains_key(&id.uri()), "{index}");
        }
    }

    #[test]
    fn library_headings_and_owner_summary_stay_distinct() {
        let configs = crate::ui::initialize_test_config();
        for name in ["default", "carbonfox", "catppuccin", "catppuccin-light"] {
            for (width, height) in [(80, 24), (120, 35), (180, 49), (140, 40)] {
                let state =
                    preview_state(configs, PreviewScreen::Library, PreviewScenario::Showcase);
                let mut ui = state.ui.lock();
                ui.theme = configs.theme_config.find_theme(name).unwrap();
                ui.current_page_mut().select(2);
                let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
                terminal.draw(|frame| draw(frame, &state, &mut ui)).unwrap();
                let content = ui.workspace_layout.content;
                let buffer = terminal.backend().buffer();
                let text = (content.y..content.bottom())
                    .map(|y| {
                        (content.x..content.right())
                            .map(|x| buffer[(x, y)].symbol())
                            .collect::<String>()
                    })
                    .collect::<Vec<_>>()
                    .join("\n");
                assert_eq!(
                    text.matches("Library").count(),
                    1,
                    "{name}/{width}x{height}"
                );
                assert_eq!(text.matches("Playlists").count(), 1, "{text}");
                // Account identity and the selected owner remain visible in their summaries.
                assert_eq!(text.matches("listener").count(), 2, "{text}");
                assert_eq!(ui.current_page().selected_index(), Some(2));
                assert!(ui.workspace_hits.iter().any(|(_, hit)| *hit
                    == crate::state::WorkspaceHit::LibraryRow {
                        focus: crate::state::LibraryFocusState::Playlists,
                        index: 2,
                    }));
            }
        }
    }

    #[test]
    fn scrolled_home_starts_at_a_shelf_instead_of_blank_rows() {
        use crate::state::HomeShelfKind;
        let configs = crate::ui::initialize_test_config();
        for focus in [
            HomeShelfKind::RecentlyPlayed,
            HomeShelfKind::Continue,
            HomeShelfKind::TopTracks,
            HomeShelfKind::Playlists,
            HomeShelfKind::Albums,
            HomeShelfKind::Artists,
        ] {
            let state = preview_state(configs, PreviewScreen::Home, PreviewScenario::Showcase);
            let mut ui = state.ui.lock();
            let mut terminal = Terminal::new(TestBackend::new(80, 24)).unwrap();
            if let PageState::Home { state: home } = ui.current_page_mut() {
                home.focus = focus;
            }
            // The first draw scrolls to the focus; the second shows the result.
            terminal.draw(|frame| draw(frame, &state, &mut ui)).unwrap();
            terminal.draw(|frame| draw(frame, &state, &mut ui)).unwrap();
            let content = ui.workspace_layout.content;
            let buffer = terminal.backend().buffer();
            let row_text = |y: u16| {
                (content.x..content.right())
                    .map(|x| buffer[(x, y)].symbol())
                    .collect::<String>()
            };
            // Title row, then the first shelf within the frame's spacing.
            assert!(
                (content.y + 2..=content.y + 3).any(|y| !row_text(y).trim().is_empty()),
                "{focus:?}: blank rows under the title\n{}",
                buffer_text(buffer, false)
            );
        }
    }

    #[test]
    fn page_controls_lose_pointer_hover_under_the_key_sequence_hint() {
        let configs = crate::ui::initialize_test_config();
        let state = preview_state(configs, PreviewScreen::Home, PreviewScenario::Showcase);
        let mut ui = state.ui.lock();
        let mut terminal = Terminal::new(TestBackend::new(140, 40)).unwrap();
        terminal.draw(|frame| draw(frame, &state, &mut ui)).unwrap();
        // A Home card in the middle of the page, where the hint will appear.
        let (card, _) = ui
            .workspace_hits
            .iter()
            .copied()
            .filter(|(rect, _)| rect.y > 8 && rect.y < 20 && rect.x > 40)
            .max_by_key(|(rect, _)| rect.width)
            .expect("a Home card under the hint area");
        ui.set_workspace_pointer(card.x + 1, card.y);
        terminal.draw(|frame| draw(frame, &state, &mut ui)).unwrap();
        assert!(
            ui.workspace_hover_rect().is_some(),
            "hover works without the hint"
        );

        ui.input_key_sequence = crate::key::KeySequence::from("g");
        terminal.draw(|frame| draw(frame, &state, &mut ui)).unwrap();
        assert!(buffer_text(terminal.backend().buffer(), false).contains("Shortcuts"));
        assert!(ui.workspace_hover_rect().is_none());
    }

    #[test]
    fn sidebar_scope_values_remain_readable_in_all_workspace_sizes() {
        use crate::state::{WorkspaceHit, WorkspaceScopeKind};
        let configs = crate::ui::initialize_test_config();
        for name in ["default", "carbonfox", "catppuccin", "catppuccin-light"] {
            for (width, height) in [(80, 24), (120, 35), (180, 49), (140, 40)] {
                for provider in [ActiveProvider::Spotify, ActiveProvider::YouTubeMusic] {
                    let state =
                        preview_state(configs, PreviewScreen::Library, PreviewScenario::Showcase);
                    let mut ui = state.ui.lock();
                    ui.theme = configs.theme_config.find_theme(name).unwrap();
                    ui.active_provider = provider;
                    ui.spotify_account_label = Some("listener".to_owned());
                    ui.youtube_account_label = Some("listener".to_owned());
                    let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
                    terminal.draw(|frame| draw(frame, &state, &mut ui)).unwrap();
                    for kind in WorkspaceScopeKind::ALL {
                        let rect = ui.workspace_hit_rect(WorkspaceHit::Scope(kind)).unwrap();
                        assert_eq!(rect.intersection(ui.workspace_layout.navigation), rect);
                        let text = (rect.x..rect.right())
                            .map(|x| terminal.backend().buffer()[(x, rect.y)].symbol())
                            .collect::<String>();
                        match kind {
                            WorkspaceScopeKind::Browsing => assert!(
                                text.contains(match provider {
                                    ActiveProvider::Spotify => "Spotify",
                                    ActiveProvider::YouTubeMusic => "YouTube",
                                }),
                                "{name}/{width}x{height}: {text}"
                            ),
                            WorkspaceScopeKind::Account => {
                                assert!(text.contains("@listener"), "{text}")
                            }
                            WorkspaceScopeKind::Playback => {
                                assert!(text.contains("YouTube"), "{text}");
                                // Both provider rows use the same wording form.
                                assert!(!text.contains("YTM") && !text.contains(" SP"), "{text}");
                            }
                        }
                    }
                }
            }
        }
    }

    #[test]
    fn header_back_hint_matches_navigation_and_keeps_its_hit_inside_the_header() {
        let configs = crate::ui::initialize_test_config();
        for name in ["default", "carbonfox", "catppuccin", "catppuccin-light"] {
            for (width, height) in [(80, 24), (120, 35), (180, 49), (140, 40)] {
                for screen in [PreviewScreen::Library, PreviewScreen::Settings] {
                    let state = preview_state(configs, screen, PreviewScenario::Showcase);
                    let mut ui = state.ui.lock();
                    ui.theme = configs.theme_config.find_theme(name).unwrap();
                    let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
                    terminal.draw(|frame| draw(frame, &state, &mut ui)).unwrap();
                    let rect = ui
                        .workspace_hit_rect(crate::state::WorkspaceHit::CloseWindow)
                        .unwrap();
                    let header = crate::ui::LayoutPolicy::from_size(width, height)
                        .workspace_frame(terminal.backend().buffer().area)
                        .header;
                    assert_eq!(rect.intersection(header), rect);
                    let text = (rect.x..rect.right())
                        .map(|x| terminal.backend().buffer()[(x, rect.y)].symbol())
                        .collect::<String>();
                    assert_eq!(text, "backspace Back", "{name}/{width}x{height}/{screen:?}");
                    assert_eq!(
                        terminal.backend().buffer()[(rect.x, rect.y)].fg,
                        ui.theme.workspace_hint_key().fg.unwrap()
                    );
                    assert_eq!(
                        terminal.backend().buffer()[(rect.right() - 1, rect.y)].fg,
                        ui.theme.workspace_hint_text().fg.unwrap()
                    );
                }
            }
        }
    }

    #[test]
    fn home_cards_share_columns_and_subtitles_without_resetting_selection() {
        use crate::state::{HomeShelfKind, WorkspaceHit};
        let configs = crate::ui::initialize_test_config();
        for name in ["default", "carbonfox", "catppuccin", "catppuccin-light"] {
            for (width, height) in [(80, 24), (120, 35), (180, 49), (140, 40)] {
                let state = preview_state(configs, PreviewScreen::Home, PreviewScenario::Showcase);
                let mut ui = state.ui.lock();
                ui.theme = configs.theme_config.find_theme(name).unwrap();
                let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
                terminal.draw(|frame| draw(frame, &state, &mut ui)).unwrap();
                let first = ui
                    .workspace_hit_rect(WorkspaceHit::HomeCard {
                        shelf: HomeShelfKind::QuickAccess,
                        index: 0,
                    })
                    .unwrap();
                assert_eq!(first.height, 2);
                let subtitle = (first.x..first.right())
                    .map(|x| terminal.backend().buffer()[(x, first.y + 1)].symbol())
                    .collect::<String>();
                assert!(
                    !subtitle.trim().is_empty(),
                    "Quick access subtitle missing: {name}/{width}x{height}"
                );
                let quick = ui
                    .workspace_hits
                    .iter()
                    .filter_map(|(rect, hit)| match hit {
                        WorkspaceHit::HomeCard {
                            shelf: HomeShelfKind::QuickAccess,
                            ..
                        } if rect.y == first.y => Some(*rect),
                        _ => None,
                    })
                    .collect::<Vec<_>>();
                for focus in [
                    HomeShelfKind::RecentlyPlayed,
                    HomeShelfKind::Continue,
                    HomeShelfKind::Playlists,
                    HomeShelfKind::Albums,
                    HomeShelfKind::Artists,
                ] {
                    if let PageState::Home { state: home } = ui.current_page_mut() {
                        home.select(focus, 0);
                    }
                    terminal.draw(|frame| draw(frame, &state, &mut ui)).unwrap();
                    let cards = ui
                        .workspace_hits
                        .iter()
                        .filter_map(|(rect, hit)| match hit {
                            WorkspaceHit::HomeCard { shelf, .. } if *shelf == focus => Some(*rect),
                            _ => None,
                        })
                        .collect::<Vec<_>>();
                    assert!(!cards.is_empty(), "{focus:?}/{width}x{height}");
                    for (card, anchor) in cards.iter().zip(&quick) {
                        assert_eq!((card.x, card.width), (anchor.x, anchor.width));
                        assert_eq!(card.intersection(ui.workspace_layout.content), *card);
                    }
                    if width == 180 && focus == HomeShelfKind::RecentlyPlayed {
                        assert!(buffer_text(terminal.backend().buffer(), false)
                            .contains("Song · The Night Office"));
                    }
                }
                if let PageState::Home { state: home } = ui.current_page_mut() {
                    home.select(HomeShelfKind::RecentlyPlayed, 10);
                }
                terminal.draw(|frame| draw(frame, &state, &mut ui)).unwrap();
                let selected = ui
                    .workspace_hit_rect(WorkspaceHit::HomeCard {
                        shelf: HomeShelfKind::RecentlyPlayed,
                        index: 10,
                    })
                    .unwrap();
                assert_eq!(selected.width, first.width);
                if let PageState::Home { state: home } = ui.current_page_mut() {
                    home.focus = HomeShelfKind::QuickAccess;
                }
                terminal.draw(|frame| draw(frame, &state, &mut ui)).unwrap();
                if let PageState::Home { state: home } = ui.current_page() {
                    assert_eq!(home.selected(HomeShelfKind::RecentlyPlayed), 10);
                    assert!(home.offset(HomeShelfKind::RecentlyPlayed) > 0);
                }
            }
        }
    }

    #[test]
    fn home_omits_more_hint_when_only_trailing_spacing_would_overflow() {
        let configs = crate::ui::initialize_test_config();
        let state = preview_state(configs, PreviewScreen::Home, PreviewScenario::Empty);
        {
            let mut data = state.data.write();
            data.user_data.playlists.clear();
            data.user_data.saved_albums.clear();
            data.user_data.followed_artists.clear();
        }
        let mut ui = state.ui.lock();
        ui.spotify_auth_status.session_ready = false;
        let mut terminal = Terminal::new(TestBackend::new(120, 9)).unwrap();
        terminal
            .draw(|frame| {
                super::super::page::render_home_page(true, frame, &state, &mut ui, frame.area());
            })
            .unwrap();
        let text = buffer_text(terminal.backend().buffer(), false);
        assert!(text.contains("Liked Music"));
        assert!(
            text.contains("Open a playlist, album or artist and it will appear here."),
            "{text}"
        );
        assert!(!text.contains("More shelves"), "{text}");
        drop(ui);

        // Shelves that really overflow show the hint, until the body is so
        // short that its row is worth more as another row of cards.
        let state = preview_state(configs, PreviewScreen::Home, PreviewScenario::Showcase);
        let mut ui = state.ui.lock();
        let render = |ui: &mut crate::state::UIStateGuard, height| {
            let mut terminal = Terminal::new(TestBackend::new(120, height)).unwrap();
            terminal
                .draw(|frame| {
                    super::super::page::render_home_page(true, frame, &state, ui, frame.area());
                })
                .unwrap();
            buffer_text(terminal.backend().buffer(), false)
        };
        let text = render(&mut ui, 8);
        assert!(text.contains("More shelves below"), "{text}");
        let text = render(&mut ui, 6);
        assert!(!text.contains("More shelves"), "{text}");
        assert!(text.contains("Your liked songs"), "{text}");
    }

    #[test]
    fn transport_uses_available_progress_width_and_quiet_status() {
        let configs = crate::ui::initialize_test_config();
        for name in ["default", "carbonfox", "catppuccin", "catppuccin-light"] {
            let mut previous = 0;
            for (width, height) in [(80, 24), (120, 35), (140, 40), (180, 49)] {
                let state =
                    preview_state(configs, PreviewScreen::Library, PreviewScenario::Showcase);
                let mut ui = state.ui.lock();
                ui.theme = configs.theme_config.find_theme(name).unwrap();
                let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
                terminal.draw(|frame| draw(frame, &state, &mut ui)).unwrap();
                let bar = ui.playback_progress_bar_rect;
                assert_eq!(bar.intersection(ui.playback_window_rect), bar);
                assert!(bar.width > 0);
                // Compact transports put the source on a separate row.
                if height >= 35 {
                    assert!(bar.width > previous, "{name}/{width}x{height}: {bar:?}");
                    previous = bar.width;
                }
                let text = buffer_text(terminal.backend().buffer(), false);
                assert!(text.contains("Playing"));
                assert!(!text.contains("PLAYING"));
                if height >= 35 {
                    assert!(text.contains("Shuffle Off · Repeat Off"));
                }
                if width == 180 {
                    assert!(bar.width > 72);
                }
                let transport = ui.playback_window_rect;
                let text = (transport.y..transport.bottom())
                    .map(|y| {
                        (transport.x..transport.right())
                            .map(|x| terminal.backend().buffer()[(x, y)].symbol())
                            .collect::<String>()
                    })
                    .collect::<Vec<_>>()
                    .join("\n");
                assert_eq!(text.matches("YouTube Music").count(), 1, "{text}");
            }
        }
    }

    #[test]
    fn every_workspace_page_uses_the_same_single_row_footer_styles() {
        use crate::state::{HomeShelfKind, SearchPageUIState, WelcomePageUIState};
        let configs = crate::ui::initialize_test_config();
        for name in ["default", "carbonfox", "catppuccin", "catppuccin-light"] {
            for (width, height) in [(80, 24), (120, 35), (180, 49), (140, 40)] {
                let state = preview_state(configs, PreviewScreen::Home, PreviewScenario::Showcase);
                let mut pages = PreviewScreen::NAMES
                    .iter()
                    .map(|screen| {
                        preview_state(
                            configs,
                            PreviewScreen::from_cli(screen).unwrap(),
                            PreviewScenario::Showcase,
                        )
                        .ui
                        .lock()
                        .current_page()
                        .clone()
                    })
                    .collect::<Vec<_>>();
                pages.extend([
                    PageState::HomeShelfList {
                        shelf: HomeShelfKind::Continue,
                        list: Default::default(),
                    },
                    PageState::Welcome {
                        state: WelcomePageUIState::new(),
                        from_settings: false,
                    },
                    PageState::new_unified_playlist("footer-fixture"),
                    PageState::Search {
                        line_input: Default::default(),
                        current_query: String::new(),
                        state: SearchPageUIState::new(),
                    },
                    PageState::Browse {
                        state: crate::state::BrowsePageUIState::CategoryList {
                            state: Default::default(),
                        },
                    },
                    PageState::Lyrics {
                        provider: ActiveProvider::Spotify,
                        track_uri: "footer-track".to_owned(),
                        track: "Neon Harbor".to_owned(),
                        artists: "Lumen Drift".to_owned(),
                        youtube_track: None,
                        lyrics_provider: None,
                        scroll_offset: 0,
                        follow_playback: true,
                        status: UiViewStatus::Empty,
                    },
                    PageState::Journal {
                        table: Default::default(),
                        journal_selection: Default::default(),
                    },
                    PageState::JournalLists {
                        list: Default::default(),
                    },
                    PageState::JournalList {
                        list_id: "footer-list".to_owned(),
                        table: Default::default(),
                        journal_selection: Default::default(),
                    },
                    PageState::SessionHistory {
                        list: Default::default(),
                    },
                    PageState::CommandHelp { scroll_offset: 0 },
                    PageState::Logs {
                        state: crate::state::DiagnosticsPageUIState::new(),
                    },
                ]);
                for page in pages {
                    let mut ui = state.ui.lock();
                    ui.theme = configs.theme_config.find_theme(name).unwrap();
                    ui.history = vec![page];
                    ui.sync_workspace_after_history_change();
                    let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
                    terminal.draw(|frame| draw(frame, &state, &mut ui)).unwrap();
                    let footer = crate::ui::LayoutPolicy::from_size(width, height)
                        .workspace_frame(terminal.backend().buffer().area)
                        .footer;
                    let hints = super::super::workspace_footer_line(
                        &ui,
                        usize::from(footer.width.saturating_sub(4)),
                    );
                    let expected = hints.spans(ui.theme.workspace_hint_key());
                    let mut x = footer.x + 2;
                    for span in expected {
                        for symbol in span.content.chars() {
                            let cell = &terminal.backend().buffer()[(x, footer.y)];
                            assert_eq!(
                                cell.symbol(),
                                symbol.to_string(),
                                "{name}/{width}x{height}: {:?}",
                                ui.current_page()
                            );
                            assert_eq!(
                                cell.fg,
                                span.style
                                    .fg
                                    .unwrap_or(ui.theme.workspace_hint_text().fg.unwrap())
                            );
                            x += 1;
                        }
                    }
                    for (rect, _) in ui
                        .workspace_hits
                        .iter()
                        .filter(|(_, hit)| *hit == crate::state::WorkspaceHit::Help)
                    {
                        assert_eq!(rect.intersection(footer), *rect);
                        assert_eq!(rect.height, 1);
                    }
                    for y in footer.y + 1..footer.bottom() {
                        assert!(
                            (0..width).all(|x| terminal.backend().buffer()[(x, y)].symbol() == " ")
                        );
                    }
                }
            }
        }
    }

    #[test]
    fn showcase_youtube_library_and_contexts_are_local_and_scenario_specific() {
        let configs = crate::ui::initialize_test_config();
        let state = preview_state(configs, PreviewScreen::Home, PreviewScenario::Showcase);
        assert!(state.ui.lock().youtube_auth_status.ready);
        assert!(state.data.read().user_data.youtube_library.loaded);
        let library = state.data.read().user_data.youtube_library.clone();
        assert_eq!(library.playlists.len(), 4);
        for id in [
            YouTubeContextId::LikedTracks,
            YouTubeContextId::Playlist(library.playlists[2].id.clone()),
            YouTubeContextId::Album(library.albums[1].id.clone()),
            YouTubeContextId::Artist(library.artists[3].id.clone()),
        ] {
            state.ui.lock().new_page(PageState::YouTubeContext {
                id: id.clone(),
                context: None,
                state: YouTubeContextPageUIState::new(),
            });
            handle_showcase_request(&state, crate::client::ClientRequest::GetYouTubeContext(id));
            if let PageState::YouTubeContext {
                context: Some(context),
                state: page_state,
                ..
            } = state.ui.lock().current_page()
            {
                assert_eq!(page_state.status, UiViewStatus::Ready);
                assert_eq!(context.tracks.len(), 18);
                assert!(context
                    .tracks
                    .iter()
                    .all(|track| SHOWCASE_TITLES.contains(&track.name.as_str())));
            } else {
                panic!("showcase context was not populated");
            }
        }
        state.ui.lock().active_provider = ActiveProvider::YouTubeMusic;
        assert!(
            crate::state::home_shelves(&state.data.read(), state.ui.lock().home_scope())
                .iter()
                .any(|shelf| shelf
                    .cards
                    .iter()
                    .any(|card| card.title == "Harbor After Dark"))
        );
        for scenario in [
            PreviewScenario::Ready,
            PreviewScenario::Loading,
            PreviewScenario::Partial,
            PreviewScenario::Failed,
            PreviewScenario::Empty,
        ] {
            let state = preview_state(configs, PreviewScreen::Home, scenario);
            assert!(!state.data.read().user_data.youtube_library.loaded);
        }
    }

    #[test]
    fn showcase_auth_marker_stays_in_its_fresh_config_folder() {
        let root = tempfile::tempdir().unwrap();
        let mut configs = Configs::new(root.path(), root.path()).unwrap();
        prepare_showcase_auth(&configs).unwrap();
        assert!(configs.youtube_music_auth_status().ready);
        assert_eq!(
            std::fs::read_to_string(configs.youtube_music_cookie_path()).unwrap(),
            "Offline showcase marker; contains no credentials.\n"
        );
        assert!(
            prepare_showcase_auth(&configs).is_err(),
            "never overwrite an existing file"
        );
        let outside = tempfile::tempdir().unwrap();
        configs.app_config.youtube.cookie_file = Some(outside.path().join("cookie.txt"));
        assert!(prepare_showcase_auth(&configs).is_err());
        assert!(!outside.path().join("cookie.txt").exists());
    }

    #[test]
    fn showcase_search_filters_catalog_and_finishes_both_provider_lifecycles() {
        use crate::state::{SearchLifecycle, SearchPageUIState};
        let configs = crate::ui::initialize_test_config();
        for provider in [ActiveProvider::Spotify, ActiveProvider::YouTubeMusic] {
            let state = preview_state(configs, PreviewScreen::Home, PreviewScenario::Showcase);
            let mut ui = state.ui.lock();
            ui.active_provider = provider;
            ui.new_page(PageState::Search {
                line_input: Default::default(),
                current_query: String::new(),
                state: SearchPageUIState::new(),
            });
            drop(ui);
            for query in ["  NEON  ", "Lumen Drift", "no fixture matches this", ""] {
                let reference = state.ui.lock().begin_search(provider, query);
                let request = match provider {
                    ActiveProvider::Spotify => crate::client::ClientRequest::Search {
                        query: query.to_owned(),
                        lifecycle_reference: reference,
                    },
                    ActiveProvider::YouTubeMusic => crate::client::ClientRequest::SearchYouTube {
                        query: query.to_owned(),
                        lifecycle_reference: reference,
                    },
                };
                handle_showcase_request(&state, request);
                let mut ui = state.ui.lock();
                let PageState::Search { state: search, .. } = ui.current_page() else {
                    panic!("Search must remain open")
                };
                if query.starts_with("no fixture") {
                    assert_eq!(search.search_lifecycle, SearchLifecycle::Empty);
                } else {
                    assert!(
                        matches!(search.search_lifecycle, SearchLifecycle::Ready { result_count } if result_count > 0)
                    );
                }
                let data = state.data.read();
                match provider {
                    ActiveProvider::Spotify => {
                        let result = data.caches.search.get(&query.to_owned()).unwrap();
                        if query.contains("NEON") {
                            assert_eq!(result.tracks.len(), 1);
                            assert_eq!(result.tracks[0].name, "Neon Harbor");
                        }
                        if query == "Lumen Drift" {
                            assert!(result
                                .tracks
                                .iter()
                                .all(|track| track.artists_info().contains(query)));
                        }
                    }
                    ActiveProvider::YouTubeMusic => {
                        let result = data.caches.youtube_search.get(&query.to_owned()).unwrap();
                        if query.contains("NEON") {
                            assert_eq!(result.songs.len(), 1);
                            assert_eq!(result.songs[0].name, "Neon Harbor");
                        }
                        if query == "Lumen Drift" {
                            assert!(result
                                .songs
                                .iter()
                                .all(|track| track.artists.contains(query)));
                        }
                    }
                }
                drop(data);
                if query.contains("NEON") {
                    for name in ["default", "carbonfox", "catppuccin", "catppuccin-light"] {
                        for (width, height) in [(80, 24), (120, 35), (180, 49), (140, 40)] {
                            ui.theme = configs.theme_config.find_theme(name).unwrap();
                            let mut terminal =
                                Terminal::new(TestBackend::new(width, height)).unwrap();
                            terminal.draw(|frame| draw(frame, &state, &mut ui)).unwrap();
                            assert!(
                                buffer_text(terminal.backend().buffer(), false)
                                    .contains("Neon Harbor"),
                                "{provider:?}/{name}/{width}x{height}"
                            );
                        }
                    }
                }
            }
        }
    }

    #[test]
    fn showcase_search_submission_focuses_rendered_results_at_workspace_sizes() {
        use crossterm::event::{Event, KeyCode, KeyEvent, KeyModifiers};
        let configs = crate::ui::initialize_test_config();
        for provider in [ActiveProvider::Spotify] {
            for name in ["default", "carbonfox", "catppuccin", "catppuccin-light"] {
                for (width, height) in [(80, 24), (120, 35), (180, 49), (140, 40)] {
                    let state =
                        preview_state(configs, PreviewScreen::Home, PreviewScenario::Showcase);
                    {
                        let mut ui = state.ui.lock();
                        ui.active_provider = provider;
                        ui.theme = configs.theme_config.find_theme(name).unwrap();
                        ui.new_page(PageState::Search {
                            line_input: Default::default(),
                            current_query: String::new(),
                            state: crate::state::SearchPageUIState::new(),
                        });
                        if let PageState::Search { state: search, .. } = ui.current_page_mut() {
                            search.focus = crate::state::SearchFocusState::Input;
                        }
                        ui.workspace_focus = crate::state::WorkspaceFocusState::Context;
                    }
                    let (sender, receiver) = crate::client::client_request_channel();
                    for key in "neon".chars().map(KeyCode::Char).chain([KeyCode::Enter]) {
                        crate::event::handle_terminal_event(
                            &Event::Key(KeyEvent::new(key, KeyModifiers::NONE)),
                            &sender,
                            &state,
                        )
                        .unwrap();
                    }
                    while let Ok(request) = receiver.try_recv() {
                        handle_showcase_request(&state, request.request().clone());
                    }
                    let mut ui = state.ui.lock();
                    let PageState::Search {
                        state: search,
                        current_query,
                        ..
                    } = ui.current_page()
                    else {
                        panic!("Search remains active")
                    };
                    assert_eq!(current_query, "neon");
                    assert_eq!(search.focus, crate::state::SearchFocusState::Tracks);
                    assert!(
                        matches!(
                            search.search_lifecycle,
                            crate::state::SearchLifecycle::Ready { .. }
                        ),
                        "{provider:?}/{name}/{width}x{height}: {:?}",
                        search.search_lifecycle
                    );
                    let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
                    terminal.draw(|frame| draw(frame, &state, &mut ui)).unwrap();
                    let text = buffer_text(terminal.backend().buffer(), false);
                    assert!(
                        text.contains("Neon Harbor"),
                        "{provider:?}/{name}/{width}x{height}: {text}"
                    );
                    assert!(!text.contains("Searching"));
                }
            }
        }
    }

    #[test]
    fn sizes_parse_and_reject_out_of_range_values() {
        assert_eq!(preview_size_from_cli("80x24").unwrap(), (80, 24));
        assert_eq!(preview_size_from_cli("180X49").unwrap(), (180, 49));
        assert!(preview_size_from_cli("80").is_err());
        assert!(preview_size_from_cli("10x5").is_err());
    }

    #[test]
    fn showcase_playback_uses_queue_lanes_and_freezes_paused_progress() {
        let configs = crate::ui::initialize_test_config();
        use crate::{
            client::{ActivePlaybackControl, ClientRequest},
            state::PlayableMedia,
        };
        let state = preview_state(configs, PreviewScreen::Home, PreviewScenario::Showcase);
        let tracks = youtube_tracks(DataSet::Showcase, 3);
        handle_showcase_request(
            &state,
            ClientRequest::PlayYouTubeContext {
                tracks: tracks.clone(),
                start_index: 1,
            },
        );
        showcase_tick(&state, Duration::from_secs(3));
        assert_eq!(
            state
                .player
                .read()
                .youtube_playback
                .as_ref()
                .unwrap()
                .progress,
            Duration::from_secs(3)
        );
        handle_showcase_request(
            &state,
            ClientRequest::ActivePlaybackControl(ActivePlaybackControl::Toggle),
        );
        showcase_tick(&state, Duration::from_secs(2));
        assert_eq!(
            state
                .player
                .read()
                .youtube_playback
                .as_ref()
                .unwrap()
                .progress,
            Duration::from_secs(3)
        );
        handle_showcase_request(
            &state,
            ClientRequest::AddItemsToUserQueue(vec![PlayableMedia::YouTube(tracks[0].clone())]),
        );
        handle_showcase_request(&state, ClientRequest::UnifiedNext);
        assert_eq!(
            state
                .player
                .read()
                .youtube_playback
                .as_ref()
                .unwrap()
                .track
                .id,
            tracks[0].id
        );
        handle_showcase_request(&state, ClientRequest::UnifiedPrevious);
        assert_eq!(
            state
                .player
                .read()
                .youtube_playback
                .as_ref()
                .unwrap()
                .track
                .id,
            tracks[1].id
        );
        let last_track = spotify_tracks(DataSet::Showcase, TRACK_COUNT)
            .pop()
            .unwrap();
        assert_eq!(
            state
                .ui
                .lock()
                .spotify_queue_labels
                .get(last_track.id.id())
                .unwrap()
                .title,
            last_track.name
        );
        let spotify = spotify_tracks(DataSet::Showcase, 3);
        handle_showcase_request(
            &state,
            ClientRequest::PlayUnifiedItems {
                items: spotify
                    .iter()
                    .map(|track| PlayableMedia::Spotify(track.id.clone().into()))
                    .collect(),
                start_index: 1,
            },
        );
        assert_eq!(
            state.player.read().active_playback_provider,
            Some(ActiveProvider::Spotify)
        );
        assert_eq!(
            match state.player.read().currently_playing().unwrap() {
                rspotify::model::PlayableItem::Track(track) => track.name.clone(),
                _ => panic!("expected fixture track"),
            },
            spotify[1].name
        );
        handle_showcase_request(&state, ClientRequest::UnifiedNext);
        assert_eq!(
            match state.player.read().currently_playing().unwrap() {
                rspotify::model::PlayableItem::Track(track) => track.name.clone(),
                _ => panic!("expected fixture track"),
            },
            spotify[2].name
        );
        handle_showcase_request(&state, ClientRequest::UnifiedPrevious);
        assert_eq!(
            match state.player.read().currently_playing().unwrap() {
                rspotify::model::PlayableItem::Track(track) => track.name.clone(),
                _ => panic!("expected fixture track"),
            },
            spotify[1].name
        );
        handle_showcase_request(
            &state,
            ClientRequest::ActivePlaybackControl(ActivePlaybackControl::Pause),
        );
        assert!(!state.player.read().playback.as_ref().unwrap().is_playing);
        assert!(
            !state
                .player
                .read()
                .buffered_playback
                .as_ref()
                .unwrap()
                .is_playing
        );
    }

    #[test]
    fn showcase_lyrics_requests_render_synced_lines_for_both_providers() {
        let configs = crate::ui::initialize_test_config();
        for provider in [ActiveProvider::Spotify, ActiveProvider::YouTubeMusic] {
            for name in ["default", "carbonfox", "catppuccin", "catppuccin-light"] {
                for (width, height) in [(80, 24), (120, 35), (180, 49), (140, 40)] {
                    let state =
                        preview_state(configs, PreviewScreen::Home, PreviewScenario::Showcase);
                    let (uri, request, media) = if provider == ActiveProvider::Spotify {
                        let track = spotify_tracks(DataSet::Showcase, 1).remove(0);
                        (
                            track.id.uri(),
                            crate::client::ClientRequest::GetLyricsFromProvider {
                                track_id: track.id.clone(),
                                provider: "lrclib".to_owned(),
                            },
                            crate::state::PlayableMedia::Spotify(track.id.into()),
                        )
                    } else {
                        let track = youtube_tracks(DataSet::Showcase, 1).remove(0);
                        (
                            format!("youtube:{}", track.id),
                            crate::client::ClientRequest::GetYouTubeLyricsFromProvider {
                                track: track.clone(),
                                provider: "lrclib".to_owned(),
                            },
                            crate::state::PlayableMedia::YouTube(track),
                        )
                    };
                    showcase_start_items(&state, vec![media], 0);
                    showcase_control(&state, Some(false));
                    {
                        let mut player = state.player.write();
                        if provider == ActiveProvider::Spotify {
                            player.playback.as_mut().unwrap().progress =
                                Some(chrono::Duration::seconds(10));
                        } else {
                            player.youtube_playback.as_mut().unwrap().progress =
                                Duration::from_secs(10);
                        }
                    }
                    {
                        let mut ui = state.ui.lock();
                        ui.active_provider = provider;
                        ui.theme = configs.theme_config.find_theme(name).unwrap();
                        ui.new_page(PageState::Lyrics {
                            provider,
                            track_uri: uri.clone(),
                            track: "Neon Harbor".to_owned(),
                            artists: "Lumen Drift".to_owned(),
                            youtube_track: None,
                            lyrics_provider: Some("lrclib".to_owned()),
                            scroll_offset: 0,
                            follow_playback: true,
                            status: UiViewStatus::Loading,
                        });
                    }
                    handle_showcase_request(&state, request);
                    assert!(state
                        .data
                        .read()
                        .caches
                        .lyrics
                        .contains_key(&crate::state::LyricsCacheKey::new(&uri, Some("lrclib"))));
                    let mut ui = state.ui.lock();
                    assert!(matches!(
                        ui.current_page(),
                        PageState::Lyrics {
                            status: UiViewStatus::Ready,
                            ..
                        }
                    ));
                    let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
                    terminal.draw(|frame| draw(frame, &state, &mut ui)).unwrap();
                    let text = buffer_text(terminal.backend().buffer(), false);
                    assert!(
                        text.contains("original lyrics"),
                        "{provider:?}/{name}/{width}x{height}: {text}"
                    );
                    assert!(
                        text.contains("A silver window catches every spark"),
                        "{text}"
                    );
                    assert!(terminal.backend().buffer().content.iter().any(|cell| cell
                        .symbol()
                        .trim()
                        != ""
                        && cell.fg == ui.theme.lyrics_playing().fg.unwrap()));
                }
            }
        }
    }

    #[cfg(feature = "streaming")]
    #[test]
    fn showcase_bands_follow_local_progress_and_stop_when_paused() {
        crate::ui::initialize_test_config();
        let root = tempfile::tempdir().unwrap();
        let mut configs = Configs::new(root.path(), root.path()).unwrap();
        configs.app_config.enable_audio_visualization = true;
        let state = preview_state(&configs, PreviewScreen::Home, PreviewScenario::Showcase);
        let bands = state.vis_bands.as_ref().unwrap();
        assert!(bands.lock().is_active);
        let before = bands.lock().values;
        showcase_tick(&state, Duration::from_secs(1));
        assert_ne!(bands.lock().values, before);
        assert!(bands
            .lock()
            .values
            .iter()
            .all(|value| value.is_finite() && (0.0..=1.0).contains(value)));
        let mut terminal = Terminal::new(TestBackend::new(80, 8)).unwrap();
        terminal
            .draw(|frame| {
                crate::ui::streaming::render_audio_visualization(
                    frame,
                    &state,
                    frame.area(),
                    Color::Rgb(40, 40, 40),
                    Color::Rgb(120, 170, 255),
                );
            })
            .unwrap();
        assert!(terminal
            .backend()
            .buffer()
            .content
            .iter()
            .any(|cell| "▁▂▃▄▅▆▇█".contains(cell.symbol())));
        showcase_control(&state, Some(false));
        showcase_tick(&state, Duration::ZERO);
        assert!(!bands.lock().is_active);
        let ready = preview_state(&configs, PreviewScreen::Home, PreviewScenario::Ready);
        assert!(!ready.vis_bands.as_ref().unwrap().lock().is_active);
    }

    #[test]
    fn color_output_encodes_rgb_and_resets_each_line() {
        let mut buffer = Buffer::empty(ratatui::layout::Rect::new(0, 0, 2, 1));
        buffer[(0, 0)].set_fg(Color::Rgb(1, 2, 3)).set_symbol("a");
        let text = buffer_text(&buffer, true);
        assert!(text.contains("\x1b[0;38;2;1;2;3ma"));
        assert!(text.ends_with("\x1b[0m\n"));
    }
}
