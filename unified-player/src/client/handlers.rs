use std::time::{Duration, Instant};

use anyhow::Context;
use rspotify::model::Id;

use crate::{
    config,
    state::{
        Context as SpotifyContext, ContextId, ContextPageType, ContextPageUIState,
        ListenBrainzArtistEnrichment, NativeQueueRefreshGuard, PageState, Playback,
        SearchFocusState, SearchLuckyIntent, SharedState, YouTubeContextId,
        YouTubeContextPageUIState, YouTubeTrack,
    },
};

use crate::utils::map_join;

use super::{ClientRequest, PlayerRequest};

struct LyricsPageTarget {
    provider: config::ActiveProvider,
    track_uri: String,
    track: String,
    artists: String,
    youtube_track: Option<YouTubeTrack>,
    request: ClientRequest,
}

fn active_lyrics_target(
    provider: config::ActiveProvider,
    spotify: Option<LyricsPageTarget>,
    youtube: Option<LyricsPageTarget>,
) -> Option<LyricsPageTarget> {
    match provider {
        config::ActiveProvider::Spotify => spotify,
        config::ActiveProvider::YouTubeMusic => youtube,
    }
}

fn begin_pending_listenbrainz_artist_enrichment(
    context_uri: &str,
    context: &mut SpotifyContext,
    request_id: u64,
) -> Option<ClientRequest> {
    let SpotifyContext::Artist {
        artist,
        top_tracks,
        listenbrainz,
        albums,
        ..
    } = context
    else {
        return None;
    };
    if !matches!(listenbrainz, ListenBrainzArtistEnrichment::Pending) {
        return None;
    }

    let spotify_artist_id = artist.id.id().to_owned();
    *listenbrainz = ListenBrainzArtistEnrichment::Loading { request_id };
    Some(ClientRequest::EnrichListenBrainzArtist {
        request_id,
        context_uri: context_uri.to_owned(),
        spotify_artist_id,
        include_recordings: top_tracks.is_empty(),
        include_release_groups: albums.is_empty(),
    })
}

fn reset_loading_listenbrainz_artist_enrichment(context: &mut SpotifyContext, request_id: u64) {
    let SpotifyContext::Artist { listenbrainz, .. } = context else {
        return;
    };
    if matches!(
        listenbrainz,
        ListenBrainzArtistEnrichment::Loading {
            request_id: current,
        } if *current == request_id
    ) {
        *listenbrainz = ListenBrainzArtistEnrichment::Pending;
    }
}

fn should_request_spotify_context(
    is_tracks_context: bool,
    is_cached: bool,
    is_failed: bool,
    id_changed: bool,
    since_last_request: Duration,
) -> bool {
    !is_tracks_context
        && !is_cached
        && !is_failed
        && (id_changed || since_last_request > Duration::from_secs(5))
}

fn current_lyrics_target(
    state: &SharedState,
    provider: config::ActiveProvider,
) -> Option<LyricsPageTarget> {
    let player = state.player.read();
    let spotify = player.currently_playing().and_then(|item| {
        let rspotify::model::PlayableItem::Track(track) = item else {
            return None;
        };
        let id = track.id.as_ref()?;
        Some(LyricsPageTarget {
            provider: config::ActiveProvider::Spotify,
            track_uri: id.uri(),
            track: track.name.clone(),
            artists: map_join(&track.artists, |artist| &artist.name, ", "),
            youtube_track: None,
            request: ClientRequest::GetLyrics {
                track_id: id.clone_static(),
            },
        })
    });
    let youtube = player.youtube_playback.as_ref().map(|playback| {
        let track = playback.track.clone();
        LyricsPageTarget {
            provider: config::ActiveProvider::YouTubeMusic,
            track_uri: format!("youtube:{}", track.id),
            track: track.name.clone(),
            artists: track.artists.clone(),
            youtube_track: Some(track.clone()),
            request: ClientRequest::GetYouTubeLyrics(track),
        }
    });
    active_lyrics_target(provider, spotify, youtube)
}

struct PlayerEventHandlerState {
    get_context_timer: Instant,
    last_playback_refresh_timer: Instant,
    native_queue_refresh: NativeQueueRefreshTracker,
    listenbrainz_artist_request: Option<(String, u64)>,
    /// The last context recorded for Home's "Continue" shelf, so a page that
    /// stays open is recorded once rather than every tick.
    last_opened_context: Option<(crate::state::HistoryNamespace, crate::state::HistoryContext)>,
}

impl PlayerEventHandlerState {
    fn new() -> Self {
        Self {
            get_context_timer: Instant::now(),
            last_playback_refresh_timer: Instant::now(),
            native_queue_refresh: NativeQueueRefreshTracker::default(),
            listenbrainz_artist_request: None,
            last_opened_context: None,
        }
    }
}

const NATIVE_QUEUE_REFRESH_RETRY_INTERVAL: Duration = Duration::from_secs(5);

/// Slowest periodic playback read while this app's integrated player owns
/// Spotify playback. Its events keep the state current and its session events
/// report another device taking over, so the read only reconciles missed events.
const LOCAL_PLAYBACK_POLL_INTERVAL: Duration = Duration::from_secs(60);

/// Interval between periodic Spotify playback reads, or `None` when none is
/// needed. `configured` is `playback_refresh_duration_in_ms`; zero disables
/// polling.
fn playback_poll_interval(
    configured: Duration,
    player: &crate::state::PlayerState,
) -> Option<Duration> {
    if configured.is_zero()
        || player.active_playback_provider == Some(config::ActiveProvider::YouTubeMusic)
    {
        return None;
    }
    // The session events are authoritative; a Web API device id can be stale
    // right after another device took over.
    Some(if player.active_integrated_device_id().is_some() {
        configured.max(LOCAL_PLAYBACK_POLL_INTERVAL)
    } else {
        configured
    })
}

#[derive(Default)]
struct NativeQueueRefreshTracker {
    last_request: Option<(NativeQueueRefreshGuard, Instant)>,
}

impl NativeQueueRefreshTracker {
    fn request_for(
        &mut self,
        candidate: Option<NativeQueueRefreshGuard>,
        now: Instant,
    ) -> Option<NativeQueueRefreshGuard> {
        let Some(candidate) = candidate else {
            self.last_request = None;
            return None;
        };
        let suppressed = self.last_request.as_ref().is_some_and(|(last, sent_at)| {
            last == &candidate
                && now.saturating_duration_since(*sent_at) < NATIVE_QUEUE_REFRESH_RETRY_INTERVAL
        });
        if suppressed {
            return None;
        }
        self.last_request = Some((candidate.clone(), now));
        Some(candidate)
    }
}

enum SearchLuckyAction {
    Request(ClientRequest),
    SpotifyContext(ContextId),
    YouTubeContext(YouTubeContextId),
}

fn search_lucky_action(
    state: &SharedState,
    intent: &SearchLuckyIntent,
) -> Option<SearchLuckyAction> {
    let data = state.data.read();
    match intent.provider {
        config::ActiveProvider::Spotify => {
            let results = data.caches.search.get(&intent.query)?;
            match intent.focus {
                SearchFocusState::Tracks => results.tracks.first().map(|track| {
                    SearchLuckyAction::Request(ClientRequest::Player(PlayerRequest::StartPlayback(
                        Playback::URIs(vec![track.id.clone().into()], None),
                        None,
                    )))
                }),
                SearchFocusState::Albums => results.albums.first().map(|item| {
                    SearchLuckyAction::SpotifyContext(ContextId::Album(item.id.clone()))
                }),
                SearchFocusState::Artists => results.artists.first().map(|item| {
                    SearchLuckyAction::SpotifyContext(ContextId::Artist(item.id.clone()))
                }),
                SearchFocusState::Playlists => results.playlists.first().map(|item| {
                    SearchLuckyAction::SpotifyContext(ContextId::Playlist(item.id.clone()))
                }),
                SearchFocusState::Shows => results.shows.first().map(|item| {
                    SearchLuckyAction::SpotifyContext(ContextId::Show(item.id.clone()))
                }),
                SearchFocusState::Episodes => results.episodes.first().map(|episode| {
                    SearchLuckyAction::Request(ClientRequest::Player(PlayerRequest::StartPlayback(
                        Playback::URIs(vec![episode.id.clone().into()], None),
                        None,
                    )))
                }),
                SearchFocusState::Category | SearchFocusState::Input | SearchFocusState::Videos => {
                    None
                }
            }
        }
        config::ActiveProvider::YouTubeMusic => {
            let results = data.caches.youtube_search.get(&intent.query)?;
            match intent.focus {
                SearchFocusState::Tracks => (!results.songs.is_empty()).then(|| {
                    SearchLuckyAction::Request(ClientRequest::PlayYouTubeContext {
                        tracks: results.songs.clone(),
                        start_index: 0,
                    })
                }),
                SearchFocusState::Videos => (!results.videos.is_empty()).then(|| {
                    SearchLuckyAction::Request(ClientRequest::PlayYouTubeContext {
                        tracks: results.videos.clone(),
                        start_index: 0,
                    })
                }),
                SearchFocusState::Episodes => {
                    let tracks = results
                        .episodes
                        .iter()
                        .map(|episode| episode.track.clone())
                        .collect::<Vec<_>>();
                    (!tracks.is_empty()).then_some(SearchLuckyAction::Request(
                        ClientRequest::PlayYouTubeContext {
                            tracks,
                            start_index: 0,
                        },
                    ))
                }
                SearchFocusState::Albums => results.albums.first().map(|item| {
                    SearchLuckyAction::YouTubeContext(YouTubeContextId::Album(item.id.clone()))
                }),
                SearchFocusState::Artists => results.artists.first().map(|item| {
                    SearchLuckyAction::YouTubeContext(YouTubeContextId::Artist(item.id.clone()))
                }),
                SearchFocusState::Playlists => results.playlists.first().map(|item| {
                    SearchLuckyAction::YouTubeContext(YouTubeContextId::Playlist(item.id.clone()))
                }),
                SearchFocusState::Shows => results.podcasts.first().map(|item| {
                    SearchLuckyAction::YouTubeContext(YouTubeContextId::Podcast(item.id.clone()))
                }),
                SearchFocusState::Category | SearchFocusState::Input => None,
            }
        }
    }
}

fn handle_search_lucky(
    state: &SharedState,
    client_pub: &crate::client::ClientRequestSender,
) -> anyhow::Result<()> {
    // Polled every tick: only a taken intent is a change worth redrawing.
    let intent = state.ui.lock_untracked().take_ready_search_lucky();
    let Some(intent) = intent else {
        return Ok(());
    };
    state.redraw.request();
    let Some(action) = search_lucky_action(state, &intent) else {
        return Ok(());
    };

    match action {
        SearchLuckyAction::Request(request) => {
            if intent.provider == config::ActiveProvider::Spotify
                && intent.focus == SearchFocusState::Episodes
            {
                state.player.write().currently_playing_tracks_id = None;
            }
            client_pub.send(request)?;
        }
        SearchLuckyAction::SpotifyContext(context_id) => {
            let mut ui = state.ui.lock();
            ui.new_page(PageState::Context {
                id: None,
                context_page_type: ContextPageType::Browsing(context_id),
                state: None,
            });
        }
        SearchLuckyAction::YouTubeContext(context_id) => {
            let mut ui = state.ui.lock();
            ui.new_page(PageState::YouTubeContext {
                id: context_id.clone(),
                context: None,
                state: YouTubeContextPageUIState::new(),
            });
            client_pub.send(ClientRequest::GetYouTubeContext(context_id))?;
        }
    }
    Ok(())
}

/// Interval between background session-validity checks.
const SESSION_CHECK_INTERVAL: Duration = Duration::from_secs(1);

pub async fn start_session_watcher(
    state: SharedState,
    client: super::AppClient,
    shutdown: tokio_util::sync::CancellationToken,
) {
    let mut interval = tokio::time::interval(SESSION_CHECK_INTERVAL);
    // If a check ever runs long (e.g. a slow reconnect), skip missed ticks
    // rather than firing them back-to-back.
    interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);

    loop {
        tokio::select! {
            () = shutdown.cancelled() => return,
            _ = interval.tick() => {}
        }
        tokio::select! {
            () = shutdown.cancelled() => return,
            result = client.check_valid_session(&state) => {
                if let Err(err) = result {
                    crate::observability::log_safe_error!(
                        error,
                        crate::observability::DiagnosticCode::SPOTIFY_AUTH_FAILED,
                        crate::observability::ErrorCategory::Authentication,
                        &err,
                        "Failed to check or reconnect the Spotify session"
                    );
                }
            }
        }
    }
}

fn handle_playback_change_event(
    state: &SharedState,
    client_pub: &crate::client::ClientRequestSender,
    handler_state: &mut PlayerEventHandlerState,
) -> anyhow::Result<()> {
    // An automatic queue read only serves a visible queue; opening the queue
    // page reads it.
    let queue_visible = {
        let ui = state.ui.lock_untracked();
        matches!(ui.current_page(), PageState::Queue { .. }) || ui.workspace_layout.show_right
    };
    let player = state.player.read();
    let (playback, duration) = match (
        player.buffered_playback.as_ref(),
        player.currently_playing(),
    ) {
        (Some(playback), Some(rspotify::model::PlayableItem::Track(track))) => {
            (playback, track.duration)
        }
        (Some(playback), Some(rspotify::model::PlayableItem::Episode(episode))) => {
            (playback, episode.duration)
        }
        _ => {
            handler_state
                .native_queue_refresh
                .request_for(None, Instant::now());
            return Ok(());
        }
    };

    // The integrated player reports its own track changes.
    if player.active_integrated_device_id().is_none() {
        if let Some(progress) = player.playback_progress() {
            // update the playback when the current track ends
            if progress >= duration && playback.is_playing {
                client_pub.send(ClientRequest::GetCurrentPlayback)?;
            }
        }
    }

    let refresh_guard = handler_state.native_queue_refresh.request_for(
        player
            .automatic_native_queue_refresh_guard()
            .filter(|_| queue_visible),
        Instant::now(),
    );
    drop(player);
    if let Some(refresh_guard) = refresh_guard {
        client_pub.send(ClientRequest::GetCurrentUserQueue(refresh_guard))?;
    }

    Ok(())
}

/// Fetch Spotify's Home reads when the shelves need them (or always with
/// `force`). Returns whether a request was sent.
pub(crate) fn request_home_feed(
    state: &SharedState,
    ui: &crate::state::UIState,
    client_pub: &crate::client::ClientRequestSender,
    force: bool,
) -> anyhow::Result<bool> {
    let scope = ui.home_scope();
    if scope.provider != crate::config::ActiveProvider::Spotify || !scope.spotify_ready {
        return Ok(false);
    }
    let generation =
        state
            .data
            .write()
            .home_feed
            .begin_refresh(scope.account, std::time::Instant::now(), force);
    let Some(generation) = generation else {
        return Ok(false);
    };
    client_pub.send(ClientRequest::GetHomeFeed {
        account: scope.account.map(str::to_owned),
        generation,
    })?;
    Ok(true)
}

/// Keep Home's Spotify shelves loaded while Home is visible.
fn refresh_home_feed(
    state: &SharedState,
    client_pub: &crate::client::ClientRequestSender,
) -> anyhow::Result<()> {
    let ui = state.ui.lock_untracked();
    if !matches!(ui.current_page(), PageState::Home { .. }) {
        return Ok(());
    }
    if request_home_feed(state, &ui, client_pub, false)? {
        state.redraw.request();
    }
    Ok(())
}

/// The history entry for the visible page once its collection has loaded.
/// Pages still loading, failed pages and the "now playing" context view do
/// not count as opened collections.
fn opened_context_entry(state: &SharedState) -> Option<crate::state::ContextHistoryEntry> {
    let ui = state.ui.lock_untracked();
    let now = crate::state::now_unix_secs();
    match ui.current_page() {
        PageState::Context {
            id: Some(id),
            context_page_type: ContextPageType::Browsing(_),
            state: Some(page_state),
        } if !matches!(page_state, ContextPageUIState::Failed { .. }) => {
            let data = state.data.read();
            let context = data.caches.context.get(&id.uri())?;
            crate::state::ContextHistoryEntry::from_spotify(
                id,
                context,
                ui.account_id(config::ActiveProvider::Spotify),
                now,
            )
        }
        PageState::YouTubeContext {
            id,
            context: Some(context),
            ..
        } => Some(crate::state::ContextHistoryEntry::from_youtube(
            id,
            context,
            ui.account_id(config::ActiveProvider::YouTubeMusic),
            now,
        )),
        PageState::UnifiedPlaylist { id, .. } => {
            let data = state.data.read();
            let playlist = data
                .unified_playlists
                .iter()
                .find(|playlist| &playlist.id == id)?;
            Some(crate::state::ContextHistoryEntry::from_unified(
                playlist, now,
            ))
        }
        _ => None,
    }
}

fn record_opened_context(state: &SharedState, handler_state: &mut PlayerEventHandlerState) {
    let Some(entry) = opened_context_entry(state) else {
        return;
    };
    let identity = (entry.namespace.clone(), entry.context.clone());
    if handler_state.last_opened_context.as_ref() == Some(&identity) {
        return;
    }
    handler_state.last_opened_context = Some(identity);
    if let Err(error) = state.data.write().record_context_open(entry) {
        crate::observability::log_safe_error!(
            warn,
            crate::observability::DiagnosticCode::CONTEXT_HISTORY_SAVE_FAILED,
            crate::observability::ErrorCategory::Storage,
            &error,
            "Local context history could not be saved"
        );
    }
}

fn handle_page_change_event(
    state: &SharedState,
    client_pub: &crate::client::ClientRequestSender,
    handler_state: &mut PlayerEventHandlerState,
) -> anyhow::Result<()> {
    // Polled every tick, so the lock does not request redraws by itself; the
    // branches below request one when they change the page.
    let mut ui = state.ui.lock_untracked();
    let active_provider = ui.active_provider;
    match ui.current_page_mut() {
        PageState::Context {
            id,
            context_page_type,
            state: page_state,
        } => {
            let expected_id = match context_page_type {
                ContextPageType::Browsing(context_id) => Some(context_id.clone()),
                ContextPageType::CurrentPlaying => state.player.read().playing_context_id(),
            };

            let new_id = if *id == expected_id {
                false
            } else {
                // update the context state and request new data when moving to a new context page
                tracing::info!("Playback context changed; updating the context state");

                *id = expected_id;
                state.redraw.request();

                // update the UI page state based on the context's type
                match id {
                    Some(id) => {
                        *page_state = Some(match id {
                            ContextId::Album(_) => ContextPageUIState::new_album(),
                            ContextId::Artist(_) => ContextPageUIState::new_artist(),
                            ContextId::Playlist(_) => ContextPageUIState::new_playlist(),
                            ContextId::Tracks(_) => ContextPageUIState::new_tracks(),
                            ContextId::Show(_) => ContextPageUIState::new_show(),
                        });
                    }
                    None => {
                        *page_state = None;
                    }
                }
                true
            };

            // request new context's data if not found in memory
            // To avoid making too many requests, only request if context id is changed
            // or it's been a while since the last request.
            if let Some(id) = id {
                let context_uri = id.uri();
                let is_cached = state.data.read().caches.context.contains_key(&context_uri);
                let is_failed =
                    matches!(page_state.as_ref(), Some(ContextPageUIState::Failed { .. }));
                if should_request_spotify_context(
                    matches!(id, ContextId::Tracks(_)),
                    is_cached,
                    is_failed,
                    new_id,
                    handler_state.get_context_timer.elapsed(),
                ) {
                    client_pub.send(ClientRequest::GetContext(id.clone()))?;
                    handler_state.get_context_timer = Instant::now();
                }

                let enrichment_request = {
                    let mut data = state.data.write();
                    let pending = matches!(
                        data.caches.context.get(&context_uri),
                        Some(SpotifyContext::Artist {
                            listenbrainz: ListenBrainzArtistEnrichment::Pending,
                            ..
                        })
                    );
                    if pending {
                        if let Some((previous_uri, previous_request_id)) =
                            handler_state.listenbrainz_artist_request.take()
                        {
                            if let Some(previous) = data.caches.context.get_mut(&previous_uri) {
                                reset_loading_listenbrainz_artist_enrichment(
                                    previous,
                                    previous_request_id,
                                );
                            }
                        }
                        let request_id = rand::random();
                        let request =
                            data.caches
                                .context
                                .get_mut(&context_uri)
                                .and_then(|context| {
                                    begin_pending_listenbrainz_artist_enrichment(
                                        &context_uri,
                                        context,
                                        request_id,
                                    )
                                });
                        if request.is_some() {
                            handler_state.listenbrainz_artist_request =
                                Some((context_uri.clone(), request_id));
                        }
                        request
                    } else {
                        None
                    }
                };
                if let Some(request) = enrichment_request {
                    client_pub.send(request)?;
                }
            }
        }

        PageState::Lyrics {
            provider,
            track_uri,
            track,
            artists,
            youtube_track,
            lyrics_provider,
            scroll_offset,
            follow_playback,
            status,
            ..
        } => {
            if let Some(target) = current_lyrics_target(state, active_provider) {
                if target.provider != *provider || target.track_uri != *track_uri {
                    tracing::info!(
                        provider = active_provider.title(),
                        "Lyrics page media identity changed; fetching active-provider lyrics"
                    );
                    *provider = target.provider;
                    *track_uri = target.track_uri;
                    *track = target.track;
                    *artists = target.artists;
                    *youtube_track = target.youtube_track;
                    *lyrics_provider = None;
                    *scroll_offset = 0;
                    *follow_playback = true;
                    *status = crate::state::UiViewStatus::Loading;
                    state.redraw.request();
                    client_pub.send(target.request)?;
                }
            }
        }
        _ => {}
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    mod playback_polling {
        use super::super::{playback_poll_interval, LOCAL_PLAYBACK_POLL_INTERVAL};
        use crate::config::ActiveProvider;
        use crate::state::PlayerState;
        use std::time::Duration;

        const CONFIGURED: Duration = Duration::from_secs(4);

        fn playing_on(device: &str) -> PlayerState {
            let playback = serde_json::from_value(serde_json::json!({
                "device": {
                    "id": device,
                    "is_active": true,
                    "is_private_session": false,
                    "is_restricted": false,
                    "name": "device",
                    "type": "Computer",
                    "volume_percent": 50
                },
                "repeat_state": "off",
                "shuffle_state": false,
                "context": null,
                "timestamp": 0,
                "progress_ms": 0,
                "is_playing": true,
                "item": null,
                "currently_playing_type": "track",
                "actions": {"disallows": {}}
            }))
            .unwrap();
            let mut player = PlayerState {
                playback: Some(playback),
                integrated_device_id: Some("integrated".to_owned()),
                ..PlayerState::default()
            };
            if device == "integrated" {
                player.apply_integrated_session_event(1, true);
            }
            player
        }

        #[test]
        fn local_playback_polls_slowly_and_other_devices_use_the_setting() {
            assert_eq!(
                playback_poll_interval(CONFIGURED, &playing_on("integrated")),
                Some(LOCAL_PLAYBACK_POLL_INTERVAL)
            );
            assert_eq!(
                playback_poll_interval(Duration::from_secs(120), &playing_on("integrated")),
                Some(Duration::from_secs(120))
            );
            assert_eq!(
                playback_poll_interval(CONFIGURED, &playing_on("phone")),
                Some(CONFIGURED)
            );
            assert_eq!(
                playback_poll_interval(CONFIGURED, &PlayerState::default()),
                Some(CONFIGURED)
            );
        }

        #[test]
        fn stale_web_api_device_after_takeover_uses_the_setting() {
            let mut player = playing_on("integrated");
            player.apply_integrated_session_event(1, false);
            assert_eq!(
                playback_poll_interval(CONFIGURED, &player),
                Some(CONFIGURED)
            );
        }

        #[test]
        fn zero_or_youtube_ownership_disables_polling() {
            assert_eq!(
                playback_poll_interval(Duration::ZERO, &playing_on("phone")),
                None
            );
            let youtube = PlayerState {
                active_playback_provider: Some(ActiveProvider::YouTubeMusic),
                ..playing_on("phone")
            };
            assert_eq!(playback_poll_interval(CONFIGURED, &youtube), None);
        }
    }

    mod opened_context_history {
        use super::super::{record_opened_context, PlayerEventHandlerState};
        use crate::state::{
            AppData, HistoryContext, PageState, UnifiedPlaylist, YouTubeContext, YouTubeContextId,
            YouTubeContextPageUIState,
        };

        fn handler_state() -> PlayerEventHandlerState {
            PlayerEventHandlerState::new()
        }

        fn recorded(state: &crate::state::SharedState) -> Vec<HistoryContext> {
            state
                .data
                .read()
                .context_history
                .visible(crate::config::ActiveProvider::YouTubeMusic, None)
                .map(|entry| entry.context.clone())
                .collect()
        }

        #[test]
        fn a_loaded_page_is_recorded_once_and_a_loading_page_not_at_all() {
            crate::ui::initialize_test_config();
            let ring =
                std::sync::Arc::new(parking_lot::Mutex::new(std::collections::VecDeque::new()));
            let (diagnostics, _runtime) = crate::observability::disabled(ring);
            let state = std::sync::Arc::new(crate::state::State::new(false, diagnostics));
            let folder = tempfile::tempdir().unwrap();
            *state.data.write() = AppData::new(folder.path(), folder.path());
            state.data.write().unified_playlists = vec![UnifiedPlaylist {
                id: "mix".to_owned(),
                name: "Mix".to_owned(),
                items: Vec::new(),
                updated_at: 0,
                next_entry_id: 1,
            }];
            {
                let mut ui = state.ui.lock();
                ui.youtube_account_id = None;
                ui.new_page(PageState::YouTubeContext {
                    id: YouTubeContextId::Album("album".to_owned()),
                    context: None,
                    state: YouTubeContextPageUIState::new(),
                });
            }
            let mut handler = handler_state();

            record_opened_context(&state, &mut handler);
            assert!(
                recorded(&state).is_empty(),
                "a loading page is not opened yet"
            );

            if let PageState::YouTubeContext { context, .. } = state.ui.lock().current_page_mut() {
                *context = Some(YouTubeContext {
                    title: "Album".to_owned(),
                    ..YouTubeContext::default()
                });
            }
            record_opened_context(&state, &mut handler);
            record_opened_context(&state, &mut handler);
            state
                .ui
                .lock()
                .new_page(PageState::new_unified_playlist("mix"));
            record_opened_context(&state, &mut handler);

            assert_eq!(
                recorded(&state),
                [
                    HistoryContext::UnifiedPlaylist("mix".to_owned()),
                    HistoryContext::YouTubeAlbum("album".to_owned()),
                ]
            );
            let restarted = AppData::new(folder.path(), folder.path());
            assert_eq!(
                restarted
                    .context_history
                    .visible(crate::config::ActiveProvider::YouTubeMusic, None)
                    .count(),
                2
            );
        }
    }

    use super::{
        active_lyrics_target, begin_pending_listenbrainz_artist_enrichment,
        reset_loading_listenbrainz_artist_enrichment, should_request_spotify_context,
        LyricsPageTarget, NativeQueueRefreshTracker, NATIVE_QUEUE_REFRESH_RETRY_INTERVAL,
    };
    use crate::{
        client::ClientRequest,
        config::ActiveProvider,
        state::{
            Artist, Context as SpotifyContext, ListenBrainzArtistEnrichment,
            NativeQueueRefreshGuard, Track, YouTubeTrack,
        },
    };

    #[test]
    fn failed_spotify_context_waits_for_explicit_page_retry() {
        let elapsed = std::time::Duration::from_secs(30);
        assert!(!should_request_spotify_context(
            false, false, true, false, elapsed
        ));
        assert!(should_request_spotify_context(
            false,
            false,
            false,
            true,
            std::time::Duration::ZERO
        ));
        assert!(!should_request_spotify_context(
            false,
            true,
            false,
            true,
            std::time::Duration::ZERO
        ));
    }

    fn youtube_track(id: &str) -> YouTubeTrack {
        YouTubeTrack {
            id: id.to_string(),
            name: "YouTube track".to_string(),
            artists: "YouTube artist".to_string(),
            album: None,
            duration: "3:00".to_string(),
            explicit: false,
            thumbnail_url: None,
            is_video: false,
        }
    }

    fn artist_context(enrichment: ListenBrainzArtistEnrichment) -> SpotifyContext {
        SpotifyContext::Artist {
            artist: Artist {
                id: rspotify::model::ArtistId::from_id("artist").unwrap(),
                name: "Artist".to_owned(),
            },
            top_tracks: Vec::new(),
            listenbrainz: enrichment,
            albums: Vec::new(),
            related_artists: Vec::new(),
        }
    }

    #[test]
    fn pending_listenbrainz_artist_enrichment_is_dispatched_once_with_stable_identity() {
        let mut context = artist_context(ListenBrainzArtistEnrichment::Pending);
        let request =
            begin_pending_listenbrainz_artist_enrichment("spotify:artist:artist", &mut context, 41)
                .expect("pending enrichment should dispatch");

        assert!(matches!(
            request,
            ClientRequest::EnrichListenBrainzArtist {
                request_id: 41,
                ref context_uri,
                ref spotify_artist_id,
                include_recordings: true,
                include_release_groups: true,
            } if context_uri == "spotify:artist:artist" && spotify_artist_id == "artist"
        ));
        assert!(matches!(
            context,
            SpotifyContext::Artist {
                listenbrainz: ListenBrainzArtistEnrichment::Loading { request_id: 41 },
                ..
            }
        ));
        assert!(begin_pending_listenbrainz_artist_enrichment(
            "spotify:artist:artist",
            &mut context,
            42,
        )
        .is_none());

        reset_loading_listenbrainz_artist_enrichment(&mut context, 40);
        assert!(matches!(
            context,
            SpotifyContext::Artist {
                listenbrainz: ListenBrainzArtistEnrichment::Loading { request_id: 41 },
                ..
            }
        ));
        reset_loading_listenbrainz_artist_enrichment(&mut context, 41);
        assert!(matches!(
            context,
            SpotifyContext::Artist {
                listenbrainz: ListenBrainzArtistEnrichment::Pending,
                ..
            }
        ));
    }

    #[test]
    fn album_only_fallback_does_not_request_replacement_recordings() {
        let mut context = artist_context(ListenBrainzArtistEnrichment::Pending);
        let SpotifyContext::Artist { top_tracks, .. } = &mut context else {
            unreachable!()
        };
        top_tracks.push(Track {
            id: rspotify::model::TrackId::from_id("track").unwrap(),
            name: "Spotify top track".to_owned(),
            artists: Vec::new(),
            album: None,
            duration: std::time::Duration::from_secs(180),
            explicit: false,
            added_at: 0,
        });

        let request =
            begin_pending_listenbrainz_artist_enrichment("spotify:artist:artist", &mut context, 42)
                .expect("missing albums should dispatch enrichment");

        assert!(matches!(
            request,
            ClientRequest::EnrichListenBrainzArtist {
                include_recordings: false,
                include_release_groups: true,
                ..
            }
        ));
    }

    #[test]
    fn lyrics_target_follows_active_provider_when_both_have_state() {
        let spotify = LyricsPageTarget {
            provider: ActiveProvider::Spotify,
            track_uri: "spotify:track:stale".to_string(),
            track: "Stale Spotify track".to_string(),
            artists: "Spotify artist".to_string(),
            youtube_track: None,
            request: ClientRequest::GetCurrentUser,
        };
        let youtube = LyricsPageTarget {
            provider: ActiveProvider::YouTubeMusic,
            track_uri: "youtube:current".to_string(),
            track: "Current YouTube track".to_string(),
            artists: "YouTube artist".to_string(),
            youtube_track: Some(youtube_track("current")),
            request: ClientRequest::GetYouTubeLyrics(youtube_track("current")),
        };

        let selected =
            active_lyrics_target(ActiveProvider::YouTubeMusic, Some(spotify), Some(youtube))
                .unwrap();

        assert_eq!(selected.track_uri, "youtube:current");
        assert_eq!(selected.track, "Current YouTube track");
        assert!(matches!(
            selected.request,
            ClientRequest::GetYouTubeLyrics(track) if track.id == "current"
        ));
    }

    #[test]
    fn native_queue_refresh_is_edge_triggered_with_bounded_retry() {
        let guard = NativeQueueRefreshGuard::new(
            Some("spotify:track:4iV5W9uYEdYUVa79Axb7Rh"),
            Some("device-a"),
        );
        let started = std::time::Instant::now();
        let mut tracker = NativeQueueRefreshTracker::default();

        assert_eq!(
            tracker.request_for(Some(guard.clone()), started),
            Some(guard.clone())
        );
        assert_eq!(
            tracker.request_for(
                Some(guard.clone()),
                started + std::time::Duration::from_millis(100)
            ),
            None
        );
        assert_eq!(
            tracker.request_for(
                Some(guard.clone()),
                started + NATIVE_QUEUE_REFRESH_RETRY_INTERVAL
            ),
            Some(guard.clone())
        );

        assert_eq!(
            tracker.request_for(
                None,
                started + NATIVE_QUEUE_REFRESH_RETRY_INTERVAL + std::time::Duration::from_millis(1)
            ),
            None
        );
        assert_eq!(
            tracker.request_for(
                Some(guard.clone()),
                started + NATIVE_QUEUE_REFRESH_RETRY_INTERVAL + std::time::Duration::from_millis(2)
            ),
            Some(guard)
        );
    }

    #[test]
    fn native_queue_refresh_identity_change_is_a_new_edge() {
        let first = NativeQueueRefreshGuard::new(None, Some("device-a"));
        let second = NativeQueueRefreshGuard::new(None, Some("device-b"));
        let started = std::time::Instant::now();
        let mut tracker = NativeQueueRefreshTracker::default();

        assert_eq!(
            tracker.request_for(Some(first), started),
            Some(NativeQueueRefreshGuard::new(None, Some("device-a")))
        );
        assert_eq!(
            tracker.request_for(
                Some(second.clone()),
                started + std::time::Duration::from_millis(100)
            ),
            Some(second)
        );
    }
}

/// Resolves a newly opened page for the offline screen preview, which runs no
/// player event watcher. The caller discards the requests this sends.
pub(crate) fn resolve_preview_page(
    state: &SharedState,
    client_pub: &crate::client::ClientRequestSender,
) -> anyhow::Result<()> {
    handle_page_change_event(state, client_pub, &mut PlayerEventHandlerState::new())
}

fn handle_player_event(
    state: &SharedState,
    client_pub: &crate::client::ClientRequestSender,
    handler_state: &mut PlayerEventHandlerState,
) -> anyhow::Result<()> {
    handle_search_lucky(state, client_pub).context("handle pending lucky search")?;
    handle_page_change_event(state, client_pub, handler_state)
        .context("handle page change event")?;
    record_opened_context(state, handler_state);
    refresh_home_feed(state, client_pub).context("refresh Home shelves")?;
    handle_playback_change_event(state, client_pub, handler_state)
        .context("handle playback change event")?;

    Ok(())
}

/// Starts event watcher listening to events and making update requests to the client if needed
pub fn start_player_event_watcher(
    state: &SharedState,
    client_pub: &crate::client::ClientRequestSender,
    shutdown: &tokio_util::sync::CancellationToken,
) {
    let configs = config::get_config();

    let refresh_duration = Duration::from_millis(100);
    let playback_refresh_duration =
        Duration::from_millis(configs.app_config.playback_refresh_duration_in_ms);
    let mut handler_state = PlayerEventHandlerState::new();

    while !shutdown.is_cancelled() {
        // periodically refresh the playback state (if enabled in config)
        let poll_interval = playback_poll_interval(playback_refresh_duration, &state.player.read());
        if poll_interval
            .is_some_and(|interval| handler_state.last_playback_refresh_timer.elapsed() >= interval)
        {
            client_pub
                .send(ClientRequest::GetCurrentPlayback)
                .unwrap_or_default();
            handler_state.last_playback_refresh_timer = Instant::now();
        }

        if let Err(err) = handle_player_event(state, client_pub, &mut handler_state) {
            crate::observability::log_safe_error!(
                error,
                crate::observability::DiagnosticCode::PLAYER_EVENT_HANDLE_FAILED,
                crate::observability::ErrorCategory::Contract,
                &err,
                "Failed to handle a player event"
            );
        }

        std::thread::sleep(refresh_duration);
    }
}
