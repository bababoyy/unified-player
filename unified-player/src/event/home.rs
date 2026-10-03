//! Home page input: shelf/card movement, opening a card, and pointer hits.

use anyhow::Result;
use rspotify::model::{AlbumId, ArtistId, PlaylistId, ShowId};

use crate::{
    client::{ClientRequest, ClientRequestSender},
    command::Command,
    state::{
        home_shelf_list, home_shelf_sizes, home_shelves, ContextId, ContextPageType,
        HistoryContext, HomeCard, HomeShelf, HomeShelfKind, HomeTarget, PageState, SharedState,
        UIStateGuard, WorkspaceNavigationItem, YouTubeContextId, YouTubeContextPageUIState,
        USER_LIKED_TRACKS_ID, USER_RECENTLY_PLAYED_TRACKS_ID, USER_TOP_TRACKS_ID,
    },
};

/// The shelves Home currently shows, as the renderer builds them.
fn visible_shelves(state: &SharedState, ui: &UIStateGuard) -> Vec<HomeShelf> {
    home_shelves(&state.data.read(), ui.home_scope())
}

fn with_home_state(
    ui: &mut UIStateGuard,
    update: impl FnOnce(&mut crate::state::HomePageUIState) -> bool,
) -> bool {
    let PageState::Home { state } = ui.current_page_mut() else {
        return false;
    };
    let changed = update(state);
    if changed {
        ui.bump_diagnostic_revision();
    }
    changed
}

pub(super) fn handle_command_for_home_page(
    command: Command,
    client_pub: &ClientRequestSender,
    state: &SharedState,
    ui: &mut UIStateGuard,
) -> Result<bool> {
    let shelves = visible_shelves(state, ui);
    let sizes = home_shelf_sizes(&shelves);
    let count = ui.count_prefix.unwrap_or(1).min(isize::MAX as usize) as isize;
    // Movement is consumed even at an edge so it never falls through to
    // another handler.
    match command {
        Command::SelectNextOrScrollDown => {
            with_home_state(ui, |home| home.move_vertical(&sizes, count));
        }
        Command::SelectPreviousOrScrollUp => {
            with_home_state(ui, |home| home.move_vertical(&sizes, -count));
        }
        Command::PageSelectNextOrScrollDown | Command::SelectLastOrScrollToBottom => {
            with_home_state(ui, |home| home.move_to_edge(&sizes, true));
        }
        Command::PageSelectPreviousOrScrollUp | Command::SelectFirstOrScrollToTop => {
            with_home_state(ui, |home| home.move_to_edge(&sizes, false));
        }
        Command::ChooseSelected => {
            let Some((cards, index)) = selected_card(&shelves, ui) else {
                return Ok(false);
            };
            activate_card(&cards, index, client_pub, state, ui)?;
        }
        _ => return Ok(false),
    }
    Ok(true)
}

/// Left/Right move within the focused shelf; Home has no other horizontal
/// commands, so it reads the raw keys like Search does.
pub(super) fn handle_horizontal_key(
    key_sequence: &crate::key::KeySequence,
    state: &SharedState,
    ui: &mut UIStateGuard,
) -> bool {
    let [crate::key::Key::None(code)] = key_sequence.keys.as_slice() else {
        return false;
    };
    let delta = match code {
        crossterm::event::KeyCode::Left => -1,
        crossterm::event::KeyCode::Right => 1,
        _ => return false,
    };
    let sizes = home_shelf_sizes(&visible_shelves(state, ui));
    with_home_state(ui, |home| home.move_horizontal(&sizes, delta));
    true
}

/// Shift+wheel or a horizontal wheel moves through the focused shelf, like
/// Left/Right; the pointer position plays no part, as with the vertical wheel.
pub(super) fn handle_horizontal_wheel(
    delta: isize,
    state: &SharedState,
    ui: &mut UIStateGuard,
) -> bool {
    if !matches!(ui.current_page(), PageState::Home { .. }) {
        return false;
    }
    let sizes = home_shelf_sizes(&visible_shelves(state, ui));
    with_home_state(ui, |home| home.move_horizontal(&sizes, delta));
    ui.workspace_focus = crate::state::WorkspaceFocusState::Context;
    true
}

/// The focused shelf's cards and the selected position.
fn selected_card(shelves: &[HomeShelf], ui: &UIStateGuard) -> Option<(Vec<HomeCard>, usize)> {
    let PageState::Home { state } = ui.current_page() else {
        return None;
    };
    let shelf = shelves.iter().find(|shelf| shelf.kind == state.focus)?;
    let index = state.selected(state.focus);
    (index < shelf.cards.len()).then(|| (shelf.cards.clone(), index))
}

/// Select a card from a pointer hit, activating it when `activate` is set.
pub(super) fn handle_card_hit(
    kind: HomeShelfKind,
    index: usize,
    activate: bool,
    client_pub: &ClientRequestSender,
    state: &SharedState,
    ui: &mut UIStateGuard,
) -> Result<bool> {
    if !matches!(ui.current_page(), PageState::Home { .. }) {
        return Ok(false);
    }
    with_home_state(ui, |home| {
        home.select(kind, index);
        true
    });
    ui.workspace_focus = crate::state::WorkspaceFocusState::Context;
    if activate {
        let shelves = visible_shelves(state, ui);
        if let Some((cards, index)) = selected_card(&shelves, ui) {
            activate_card(&cards, index, client_pub, state, ui)?;
        }
    }
    Ok(true)
}

/// Run the card at `index`. Track cards play the list's tracks from that
/// card; every other card navigates without starting playback.
fn activate_card(
    cards: &[HomeCard],
    index: usize,
    client_pub: &ClientRequestSender,
    state: &SharedState,
    ui: &mut UIStateGuard,
) -> Result<()> {
    let Some(card) = cards.get(index) else {
        return Ok(());
    };
    match &card.target {
        HomeTarget::Context(context) => open_home_target(context, client_pub, state, ui),
        HomeTarget::Track(_) => {
            let tracks: Vec<_> = cards
                .iter()
                .filter_map(|card| match &card.target {
                    HomeTarget::Track(media) => Some(media.clone()),
                    _ => None,
                })
                .collect();
            let start_index = cards[..index]
                .iter()
                .filter(|card| matches!(card.target, HomeTarget::Track(_)))
                .count();
            client_pub.send(ClientRequest::PlayUnifiedItems {
                items: tracks,
                start_index,
            })?;
            Ok(())
        }
        HomeTarget::ShowAll(kind) => open_show_all(*kind, client_pub, ui),
        HomeTarget::Retry => {
            crate::client::request_home_feed(state, ui, client_pub, true)?;
            Ok(())
        }
    }
}

fn open_show_all(
    kind: HomeShelfKind,
    client_pub: &ClientRequestSender,
    ui: &mut UIStateGuard,
) -> Result<()> {
    let spotify_tracks = match (ui.active_provider, kind) {
        (crate::config::ActiveProvider::Spotify, HomeShelfKind::RecentlyPlayed) => {
            Some(USER_RECENTLY_PLAYED_TRACKS_ID.clone())
        }
        (_, HomeShelfKind::TopTracks) => Some(USER_TOP_TRACKS_ID.clone()),
        _ => None,
    };
    if let Some(tracks_id) = spotify_tracks {
        let context_id = ContextId::Tracks(tracks_id);
        ui.new_page(PageState::Context {
            id: None,
            context_page_type: ContextPageType::Browsing(context_id.clone()),
            state: None,
        });
        client_pub.send(ClientRequest::GetContext(context_id))?;
        return Ok(());
    }
    match kind {
        HomeShelfKind::Playlists => {
            super::page::open_workspace_library(ui, WorkspaceNavigationItem::Playlists);
        }
        HomeShelfKind::Albums => {
            super::page::open_workspace_library(ui, WorkspaceNavigationItem::Albums);
        }
        HomeShelfKind::Artists => {
            super::page::open_workspace_library(ui, WorkspaceNavigationItem::Artists);
        }
        HomeShelfKind::Continue
        | HomeShelfKind::RecentlyPlayed
        | HomeShelfKind::UnifiedPlaylists => {
            let mut list = ratatui::widgets::ListState::default();
            list.select(Some(0));
            ui.new_page(PageState::HomeShelfList { shelf: kind, list });
        }
        HomeShelfKind::QuickAccess | HomeShelfKind::TopTracks => {}
    }
    Ok(())
}

/// The cards listed by a "Show all" page.
pub(crate) fn shelf_list_cards(
    state: &SharedState,
    ui: &UIStateGuard,
    kind: HomeShelfKind,
) -> Vec<HomeCard> {
    home_shelf_list(&state.data.read(), ui.home_scope(), kind)
}

pub(super) fn handle_command_for_home_shelf_list(
    command: Command,
    client_pub: &ClientRequestSender,
    state: &SharedState,
    ui: &mut UIStateGuard,
) -> Result<bool> {
    let PageState::HomeShelfList { shelf, list } = ui.current_page() else {
        return Ok(false);
    };
    let (kind, selected) = (*shelf, list.selected().unwrap_or_default());
    let cards = shelf_list_cards(state, ui, kind);
    if command == Command::ChooseSelected {
        activate_card(&cards, selected, client_pub, state, ui)?;
        return Ok(true);
    }
    let count = ui.count_prefix;
    Ok(super::page::handle_navigation_command(
        command,
        ui.current_page_mut(),
        selected,
        cards.len(),
        count,
    ))
}

/// Select a "Show all" row from a pointer hit, activating it when asked.
pub(super) fn handle_list_row_hit(
    index: usize,
    activate: bool,
    client_pub: &ClientRequestSender,
    state: &SharedState,
    ui: &mut UIStateGuard,
) -> Result<bool> {
    let PageState::HomeShelfList { shelf, list } = ui.current_page_mut() else {
        return Ok(false);
    };
    list.select(Some(index));
    let kind = *shelf;
    ui.workspace_focus = crate::state::WorkspaceFocusState::Context;
    if activate {
        let cards = shelf_list_cards(state, ui, kind);
        activate_card(&cards, index, client_pub, state, ui)?;
    }
    Ok(true)
}

/// Open the collection behind a card. Opening never starts playback.
pub(super) fn open_home_target(
    target: &HistoryContext,
    client_pub: &ClientRequestSender,
    state: &SharedState,
    ui: &mut UIStateGuard,
) -> Result<()> {
    let spotify = match target {
        HistoryContext::SpotifyPlaylist(id) => PlaylistId::from_id(id.clone())
            .ok()
            .map(|id| ContextId::Playlist(id.into_static())),
        HistoryContext::SpotifyAlbum(id) => AlbumId::from_id(id.clone())
            .ok()
            .map(|id| ContextId::Album(id.into_static())),
        HistoryContext::SpotifyArtist(id) => ArtistId::from_id(id.clone())
            .ok()
            .map(|id| ContextId::Artist(id.into_static())),
        HistoryContext::SpotifyShow(id) => ShowId::from_id(id.clone())
            .ok()
            .map(|id| ContextId::Show(id.into_static())),
        HistoryContext::SpotifyLikedTracks => Some(ContextId::Tracks(USER_LIKED_TRACKS_ID.clone())),
        _ => None,
    };
    if let Some(context_id) = spotify {
        ui.new_page(PageState::Context {
            id: None,
            context_page_type: ContextPageType::Browsing(context_id.clone()),
            state: None,
        });
        client_pub.send(ClientRequest::GetContext(context_id))?;
        return Ok(());
    }
    let youtube = match target {
        HistoryContext::YouTubePlaylist(id) => Some(YouTubeContextId::Playlist(id.clone())),
        HistoryContext::YouTubeAlbum(id) => Some(YouTubeContextId::Album(id.clone())),
        HistoryContext::YouTubeArtist(id) => Some(YouTubeContextId::Artist(id.clone())),
        HistoryContext::YouTubePodcast(id) => Some(YouTubeContextId::Podcast(id.clone())),
        HistoryContext::YouTubeLikedTracks => Some(YouTubeContextId::LikedTracks),
        _ => None,
    };
    if let Some(context_id) = youtube {
        ui.new_page(PageState::YouTubeContext {
            id: context_id.clone(),
            context: None,
            state: YouTubeContextPageUIState::new(),
        });
        client_pub.send(ClientRequest::GetYouTubeContext(context_id))?;
        return Ok(());
    }
    if let HistoryContext::UnifiedPlaylist(id) = target {
        if state
            .data
            .read()
            .unified_playlists
            .iter()
            .any(|playlist| &playlist.id == id)
        {
            ui.new_page(PageState::new_unified_playlist(id.clone()));
        } else {
            ui.set_unsupported_operation(
                "That unified playlist no longer exists.",
                "It may have been deleted or renamed in another session.",
            );
        }
        return Ok(());
    }
    ui.set_unsupported_operation(
        "This item can no longer be opened.",
        "Its saved identity is not valid for the current provider.",
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{handle_command_for_home_page, open_home_target};
    use crate::client::ClientRequest;
    use crate::command::Command;
    use crate::state::{HistoryContext, HomeFeedSource, HomeShelfKind, PageState, Track};

    fn setup() -> (crate::state::SharedState, tempfile::TempDir) {
        crate::ui::initialize_test_config();
        let ring = std::sync::Arc::new(parking_lot::Mutex::new(std::collections::VecDeque::new()));
        let (diagnostics, _runtime) = crate::observability::disabled(ring);
        let state = std::sync::Arc::new(crate::state::State::new(false, diagnostics));
        let folder = tempfile::tempdir().unwrap();
        *state.data.write() = crate::state::AppData::new(folder.path(), folder.path());
        {
            let mut ui = state.ui.lock();
            ui.active_provider = crate::config::ActiveProvider::Spotify;
            ui.spotify_account_id = None;
            ui.spotify_auth_status.session_ready = true;
            ui.history = vec![PageState::Home {
                state: crate::state::HomePageUIState::default(),
            }];
        }
        (state, folder)
    }

    fn track(id: &str) -> Track {
        Track {
            id: rspotify::model::TrackId::from_id(id).unwrap().into_static(),
            name: id.to_owned(),
            artists: Vec::new(),
            album: None,
            duration: std::time::Duration::from_secs(60),
            explicit: false,
            added_at: 0,
        }
    }

    fn select(state: &crate::state::SharedState, kind: HomeShelfKind, index: usize) {
        if let PageState::Home { state: home } = state.ui.lock().current_page_mut() {
            home.select(kind, index);
        }
    }

    fn choose(state: &crate::state::SharedState, sender: &crate::client::ClientRequestSender) {
        let mut ui = state.ui.lock();
        assert!(
            handle_command_for_home_page(Command::ChooseSelected, sender, state, &mut ui).unwrap()
        );
    }

    #[test]
    fn opening_a_card_navigates_without_starting_playback() {
        let (state, _folder) = setup();
        let (sender, receiver) = crate::client::client_request_channel();
        let mut ui = state.ui.lock();

        open_home_target(
            &HistoryContext::YouTubeAlbum("MPREb_album".to_owned()),
            &sender,
            &state,
            &mut ui,
        )
        .unwrap();

        assert!(matches!(
            ui.current_page(),
            PageState::YouTubeContext { .. }
        ));
        let request = receiver.try_recv().unwrap();
        assert!(matches!(
            request.request(),
            ClientRequest::GetYouTubeContext(_)
        ));
        assert!(
            receiver.try_recv().is_err(),
            "opening must not queue playback"
        );
    }

    #[test]
    fn a_track_card_plays_its_shelf_from_that_track_and_show_all_opens_the_full_list() {
        let (state, _folder) = setup();
        let (sender, receiver) = crate::client::client_request_channel();
        {
            let mut data = state.data.write();
            let now = std::time::Instant::now();
            let generation = data.home_feed.begin_refresh(None, now, false).unwrap();
            let tracks = vec![
                track("track0000000000000000000000000001"),
                track("track0000000000000000000000000002"),
            ];
            data.home_feed
                .apply(generation, HomeFeedSource::RecentlyPlayed, Ok(tracks), now);
            data.home_feed
                .apply(generation, HomeFeedSource::TopTracks, Ok(Vec::new()), now);
        }

        select(&state, HomeShelfKind::RecentlyPlayed, 1);
        choose(&state, &sender);
        let request = receiver.try_recv().unwrap();
        let ClientRequest::PlayUnifiedItems { items, start_index } = request.request() else {
            panic!("a track card plays");
        };
        assert_eq!((items.len(), *start_index), (2, 1));

        select(&state, HomeShelfKind::RecentlyPlayed, 2);
        choose(&state, &sender);
        assert!(matches!(
            state.ui.lock().current_page(),
            PageState::Context { .. }
        ));
        assert!(matches!(
            receiver.try_recv().unwrap().request(),
            ClientRequest::GetContext(_)
        ));
    }

    #[test]
    fn shift_wheel_moves_the_focused_shelf_wherever_the_pointer_is() {
        use crossterm::event::{KeyModifiers, MouseEvent, MouseEventKind};
        let (state, _folder) = setup();
        let (sender, _receiver) = crate::client::client_request_channel();
        {
            let mut data = state.data.write();
            let now = std::time::Instant::now();
            let generation = data.home_feed.begin_refresh(None, now, false).unwrap();
            let tracks = vec![
                track("track0000000000000000000000000001"),
                track("track0000000000000000000000000002"),
            ];
            data.home_feed
                .apply(generation, HomeFeedSource::RecentlyPlayed, Ok(tracks), now);
        }
        select(&state, HomeShelfKind::RecentlyPlayed, 0);
        {
            // The pointer hovers a quick-access tile, which must not take over.
            let mut ui = state.ui.lock();
            ui.playback_window_rect = ratatui::layout::Rect::default();
            ui.workspace_hits = vec![(
                ratatui::layout::Rect::new(10, 20, 24, 1),
                crate::state::WorkspaceHit::HomeCard {
                    shelf: HomeShelfKind::QuickAccess,
                    index: 0,
                },
            )];
        }
        let wheel = |kind, modifiers| MouseEvent {
            kind,
            column: 12,
            row: 20,
            modifiers,
        };
        let selected = || match state.ui.lock().current_page() {
            PageState::Home { state: home } => {
                (home.focus, home.selected(HomeShelfKind::RecentlyPlayed))
            }
            _ => unreachable!(),
        };

        super::super::handle_mouse_event(
            wheel(MouseEventKind::ScrollDown, KeyModifiers::SHIFT),
            &sender,
            &state,
        )
        .unwrap();
        assert_eq!(selected(), (HomeShelfKind::RecentlyPlayed, 1));

        super::super::handle_mouse_event(
            wheel(MouseEventKind::ScrollLeft, KeyModifiers::NONE),
            &sender,
            &state,
        )
        .unwrap();
        assert_eq!(selected(), (HomeShelfKind::RecentlyPlayed, 0));
    }

    #[test]
    fn retry_fetches_a_failed_shelf_again() {
        let (state, _folder) = setup();
        let (sender, receiver) = crate::client::client_request_channel();
        {
            let mut data = state.data.write();
            let now = std::time::Instant::now();
            let generation = data.home_feed.begin_refresh(None, now, false).unwrap();
            data.home_feed.apply(
                generation,
                HomeFeedSource::RecentlyPlayed,
                Err(crate::state::HomeFeedFailure::Unavailable),
                now,
            );
            data.home_feed.apply(
                generation,
                HomeFeedSource::TopTracks,
                Err(crate::state::HomeFeedFailure::Unavailable),
                now,
            );
        }

        select(&state, HomeShelfKind::RecentlyPlayed, 0);
        choose(&state, &sender);

        assert!(matches!(
            receiver.try_recv().unwrap().request(),
            ClientRequest::GetHomeFeed { .. }
        ));
        assert_eq!(
            state.data.read().home_feed.recently_played.status,
            crate::state::HomeFeedStatus::Loading
        );
    }
}
