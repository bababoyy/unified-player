use anyhow::Result;
use rspotify::prelude::Id;

use crate::state::{
    BrowseData, Category, MemoryCaches, PageState, PlayerState, Playlist, SearchResults,
    SharedState, UiOperationKind, UiOperationState, YouTubePlayback, YouTubePlaybackPhase,
    YouTubeSearchResults, SETTINGS_ACTION_COMPLETED_CODE, SETTINGS_ACTION_COMPLETED_MESSAGE,
    SETTINGS_ACTION_FAILED_CODE, SETTINGS_ACTION_FAILED_MESSAGE, SETTINGS_ACTION_NEXT_ACTION,
    SETTINGS_ACTION_RUNNING_CODE, SETTINGS_ACTION_RUNNING_MESSAGE, TTL_CACHE_DURATION,
};

use super::youtube::{YouTubePlaybackControlSnapshot, YouTubePlaybackSnapshot};
use super::PlaylistMutationEffect;

/// Applies provider and request results to shared application state.
///
/// Keeping these transitions explicit makes request handlers responsible for
/// orchestration rather than duplicating UI and player-state mutation rules.
#[derive(Clone, Copy, Debug, Default)]
pub(super) struct StateApplicationService;

/// Follow-up work produced by a typed playlist result.  Provider adapters
/// never touch page/cache state; the application owner turns their effect
/// receipt into these explicit state transitions.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(super) enum PlaylistRefreshEffect {
    None,
    YouTubePlaylist { playlist_id: String },
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(super) struct PlaylistEffectApplication {
    pub appended_occurrence_token: Option<String>,
    pub refresh: PlaylistRefreshEffect,
}

// Keep this stateless service as an AppClient dependency so state writes stay
// explicit at orchestration call sites and can gain policy without rewiring them.
#[allow(deprecated)]
fn spotify_premium_status(
    product: Option<rspotify::model::SubscriptionLevel>,
    session_premium: Option<crate::config::SpotifyPremiumStatus>,
) -> crate::config::SpotifyPremiumStatus {
    match product {
        Some(rspotify::model::SubscriptionLevel::Premium) => {
            crate::config::SpotifyPremiumStatus::Premium
        }
        Some(rspotify::model::SubscriptionLevel::Free) => {
            crate::config::SpotifyPremiumStatus::NotPremium
        }
        None => session_premium.unwrap_or(crate::config::SpotifyPremiumStatus::Unknown),
    }
}

fn spotify_auth_snapshot(
    web_api_product: Option<rspotify::model::SubscriptionLevel>,
    session_premium: Option<crate::config::SpotifyPremiumStatus>,
) -> crate::config::SpotifyAuthSnapshot {
    crate::config::SpotifyAuthSnapshot {
        session_ready: true,
        premium: spotify_premium_status(web_api_product, session_premium),
    }
}

#[allow(clippy::unused_self)]
impl StateApplicationService {
    pub(super) fn apply_playlist_mutation_effect(
        self,
        state: &SharedState,
        effect: PlaylistMutationEffect,
    ) -> PlaylistEffectApplication {
        let PlaylistMutationEffect::InvalidatePlaylist {
            provider,
            playlist_id,
            appended_occurrence_token,
            ..
        } = effect;
        let refresh = match provider {
            crate::state::Provider::Spotify => {
                let uri = crate::state::PlaylistId::from_uri(&playlist_id)
                    .or_else(|_| crate::state::PlaylistId::from_id(&playlist_id))
                    .map(|id| id.uri())
                    .unwrap_or(playlist_id);
                state.data.write().caches.context.remove(&uri);
                PlaylistRefreshEffect::None
            }
            crate::state::Provider::YouTubeMusic => {
                PlaylistRefreshEffect::YouTubePlaylist { playlist_id }
            }
        };
        PlaylistEffectApplication {
            appended_occurrence_token,
            refresh,
        }
    }

    #[allow(deprecated)]
    pub(super) fn update_spotify_auth_status(
        self,
        state: &SharedState,
        session_premium: Option<crate::config::SpotifyPremiumStatus>,
    ) {
        let snapshot = {
            let data = state.data.read();
            // Spotify removed `product` from the Web API response. A
            // successfully connected librespot session has already passed
            // its Premium-only catalogue check, so use that provider-owned
            // evidence instead of treating it as unknown.
            spotify_auth_snapshot(
                data.user_data.user.as_ref().and_then(|user| user.product),
                session_premium,
            )
        };
        let mut ui = state.ui.lock();
        ui.spotify_auth_status = snapshot;
        ui.mark_setup_ready_if_possible();
    }

    pub(super) fn update_setup_success(self, state: &SharedState) {
        let mut ui = state.ui.lock();
        ui.mark_setup_ready_if_possible();
    }

    pub(super) fn update_setup_cancelled(self, state: &SharedState) {
        state.ui.lock().mark_setup_cancelled();
    }

    pub(super) fn update_setup_failure(
        self,
        state: &SharedState,
        failure: crate::config::SetupFailure,
    ) {
        state.ui.lock().mark_setup_failed(failure);
    }

    pub(super) fn apply_browse_categories(
        self,
        browse: &mut BrowseData,
        categories: Vec<Category>,
    ) {
        let started = std::time::Instant::now();
        browse.categories = categories;
        browse.categories_loaded = true;
        state_applied(started.elapsed());
    }

    pub(super) fn apply_browse_category_playlists(
        self,
        browse: &mut BrowseData,
        category_id: String,
        playlists: Vec<Playlist>,
    ) {
        let started = std::time::Instant::now();
        browse.category_playlists.insert(category_id, playlists);
        state_applied(started.elapsed());
    }

    pub(super) fn cache_spotify_search(
        self,
        caches: &mut MemoryCaches,
        query: String,
        results: SearchResults,
    ) {
        let started = std::time::Instant::now();
        caches
            .search
            .insert(query, std::sync::Arc::new(results), *TTL_CACHE_DURATION);
        state_applied(started.elapsed());
    }

    pub(super) fn cache_youtube_search(
        self,
        caches: &mut MemoryCaches,
        query: String,
        results: YouTubeSearchResults,
    ) {
        let started = std::time::Instant::now();
        caches
            .youtube_search
            .insert(query, std::sync::Arc::new(results), *TTL_CACHE_DURATION);
        state_applied(started.elapsed());
    }

    pub(super) fn apply_youtube_control_snapshot(
        self,
        player: &mut PlayerState,
        snapshot: &YouTubePlaybackControlSnapshot,
    ) {
        let started = std::time::Instant::now();
        if let Some(playback) = &mut player.youtube_playback {
            playback.is_playing = snapshot.is_playing;
            playback.progress = snapshot.progress;
            playback.volume = snapshot.volume;
            playback.mute_state = snapshot.mute_state;
        }
        player.youtube_playback_phase = if snapshot.is_playing {
            YouTubePlaybackPhase::Playing
        } else {
            YouTubePlaybackPhase::Paused
        };
        state_applied(started.elapsed());
    }

    pub(super) fn apply_youtube_playback_snapshot(
        self,
        player: &mut PlayerState,
        snapshot: YouTubePlaybackSnapshot,
    ) -> String {
        let started = std::time::Instant::now();
        let track_id = snapshot.track.id.clone();
        player.youtube_playback = Some(YouTubePlayback {
            track: snapshot.track,
            is_playing: snapshot.is_playing,
            progress: snapshot.progress,
            volume: snapshot.volume,
            mute_state: snapshot.mute_state,
            route: snapshot.route,
        });
        player.youtube_playback_phase = if snapshot.is_playing {
            YouTubePlaybackPhase::Playing
        } else {
            YouTubePlaybackPhase::Paused
        };
        state_applied(started.elapsed());
        track_id
    }

    pub(super) fn set_youtube_phase(self, player: &mut PlayerState, phase: YouTubePlaybackPhase) {
        let started = std::time::Instant::now();
        player.youtube_playback_phase = phase;
        state_applied(started.elapsed());
    }

    pub(super) fn youtube_duration_label(self, duration: std::time::Duration) -> Option<String> {
        (!duration.is_zero()).then(|| {
            let seconds = duration.as_secs();
            format!("{}:{:02}", seconds / 60, seconds % 60)
        })
    }

    pub(super) fn update_settings_action_status(
        self,
        state: &SharedState,
        key: &str,
        result: Result<String, &anyhow::Error>,
    ) {
        let started = std::time::Instant::now();
        let mut ui = state.ui.lock();
        if key == "spotify.auth" {
            ui.welcome_spotify_notice = Some(match &result {
                Ok(message) => message.clone(),
                Err(_) => {
                    "Spotify sign-in did not finish. Retry authentication or open Diagnostics."
                        .to_owned()
                }
            });
        }
        if matches!(key, "youtube.auth.browser_login" | "youtube.auth.test") {
            ui.welcome_youtube_notice = Some(match &result {
                Ok(message) => message.clone(),
                Err(_) => {
                    "YouTube check failed. Retry, choose another browser, or import fresh cookies."
                        .to_owned()
                }
            });
        }
        let has_setting = matches!(ui.current_page(), PageState::Settings { settings, .. }
            if settings.iter().any(|setting| setting.key == key));
        if !has_setting {
            return;
        }
        let operation_reference = ui.start_operation(
            UiOperationKind::ProviderCommand,
            SETTINGS_ACTION_RUNNING_CODE,
            SETTINGS_ACTION_RUNNING_MESSAGE,
        );
        let succeeded = {
            let PageState::Settings {
                saved,
                error,
                notice,
                ..
            } = ui.current_page_mut()
            else {
                return;
            };
            *saved = false;
            if let Ok(message) = result {
                *error = None;
                *notice = Some(message);
                true
            } else {
                *notice = None;
                *error = Some(SETTINGS_ACTION_FAILED_MESSAGE.to_owned());
                false
            }
        };
        if succeeded {
            ui.complete_operation(
                &operation_reference,
                UiOperationState::Completed,
                SETTINGS_ACTION_COMPLETED_CODE,
                SETTINGS_ACTION_COMPLETED_MESSAGE,
                None,
            );
        } else {
            ui.complete_operation(
                &operation_reference,
                UiOperationState::Failed,
                SETTINGS_ACTION_FAILED_CODE,
                SETTINGS_ACTION_FAILED_MESSAGE,
                Some(SETTINGS_ACTION_NEXT_ACTION),
            );
        }
        state_applied(started.elapsed());
    }
}

fn state_applied(elapsed: std::time::Duration) {
    crate::observability::operation_stage(
        crate::observability::Component::State,
        "state_applied",
        Some(elapsed),
        Some(crate::observability::OperationOutcome::Success),
    );
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use crate::state::{
        BrowseData, Category, MemoryCaches, PlayerState, SearchResults, YouTubePlayback,
        YouTubePlaybackPhase, YouTubeTrack,
    };

    use super::{
        spotify_auth_snapshot, spotify_premium_status, StateApplicationService,
        SETTINGS_ACTION_FAILED_CODE, SETTINGS_ACTION_FAILED_MESSAGE, SETTINGS_ACTION_NEXT_ACTION,
    };
    use crate::client::{
        request::RequestDomain,
        youtube::{YouTubePlaybackControlSnapshot, YouTubePlaybackSnapshot},
        ClientRequest,
    };

    fn track(id: &str) -> YouTubeTrack {
        YouTubeTrack {
            id: id.to_string(),
            name: "Track".to_string(),
            artists: "Artist".to_string(),
            album: None,
            duration: "3:00".to_string(),
            explicit: false,
            thumbnail_url: None,
            is_video: false,
        }
    }

    #[test]
    fn welcome_youtube_receives_backend_progress_and_safe_failure() {
        crate::ui::initialize_test_config();
        let ring = std::sync::Arc::new(parking_lot::Mutex::new(std::collections::VecDeque::new()));
        let (diagnostics, _runtime) = crate::observability::disabled(ring);
        let state = std::sync::Arc::new(crate::state::State::new(false, diagnostics));
        state.ui.lock().open_setup_page(false);
        StateApplicationService.update_settings_action_status(
            &state,
            "youtube.auth.browser_login",
            Ok("Waiting for Google sign-in".to_owned()),
        );
        assert_eq!(
            state.ui.lock().welcome_youtube_notice.as_deref(),
            Some("Waiting for Google sign-in")
        );
        StateApplicationService.update_settings_action_status(
            &state,
            "youtube.auth.browser_login",
            Err(&anyhow::anyhow!("synthetic-private-cookie")),
        );
        let notice = state.ui.lock().welcome_youtube_notice.clone().unwrap();
        assert!(!notice.contains("synthetic-private-cookie"));
        assert!(notice.contains("import"));
    }

    #[test]
    fn control_snapshot_updates_existing_playback_atomically() {
        let service = StateApplicationService;
        let mut player = PlayerState {
            youtube_playback: Some(YouTubePlayback {
                track: track("old"),
                is_playing: true,
                progress: Duration::from_secs(5),
                volume: 20,
                mute_state: None,
                route: crate::state::YouTubePlaybackRoute::default(),
            }),
            youtube_playback_phase: YouTubePlaybackPhase::Playing,
            ..PlayerState::default()
        };

        service.apply_youtube_control_snapshot(
            &mut player,
            &YouTubePlaybackControlSnapshot {
                is_playing: false,
                progress: Duration::from_secs(42),
                volume: 73,
                mute_state: Some(73),
            },
        );

        let playback = player.youtube_playback.expect("playback remains available");
        assert_eq!(playback.track.id, "old");
        assert!(!playback.is_playing);
        assert_eq!(playback.progress, Duration::from_secs(42));
        assert_eq!(playback.volume, 73);
        assert_eq!(playback.mute_state, Some(73));
        assert_eq!(player.youtube_playback_phase, YouTubePlaybackPhase::Paused);
    }

    #[test]
    fn full_snapshot_replaces_playback_and_returns_monitor_identity() {
        let service = StateApplicationService;
        let mut player = PlayerState::default();

        let track_id = service.apply_youtube_playback_snapshot(
            &mut player,
            YouTubePlaybackSnapshot {
                track: track("new"),
                is_playing: true,
                progress: Duration::from_secs(9),
                volume: 64,
                mute_state: None,
                route: crate::state::YouTubePlaybackRoute::default(),
            },
        );

        assert_eq!(track_id, "new");
        let playback = player
            .youtube_playback
            .expect("snapshot publishes playback");
        assert_eq!(playback.track.id, "new");
        assert_eq!(playback.progress, Duration::from_secs(9));
        assert_eq!(player.youtube_playback_phase, YouTubePlaybackPhase::Playing);
    }

    #[test]
    fn duration_label_rejects_unknown_zero_and_formats_known_duration() {
        let service = StateApplicationService;
        assert_eq!(service.youtube_duration_label(Duration::ZERO), None);
        assert_eq!(
            service.youtube_duration_label(Duration::from_secs(125)),
            Some("2:05".to_string())
        );
    }

    #[test]
    fn settings_action_failure_contract_is_bounded_and_diagnostic_safe() {
        assert_eq!(SETTINGS_ACTION_FAILED_CODE, "SETTINGS_ACTION_FAILED");
        assert_eq!(
            SETTINGS_ACTION_FAILED_MESSAGE,
            "The settings action could not be completed."
        );
        assert!(SETTINGS_ACTION_NEXT_ACTION.contains("Diagnostics"));
        assert!(!SETTINGS_ACTION_FAILED_MESSAGE.contains("error chain"));
    }

    #[test]
    fn provider_read_route_and_search_transition_form_request_to_effect_contract() {
        let request = ClientRequest::Search {
            query: "focus".to_string(),
            lifecycle_reference: "test-search".to_owned(),
        };
        assert_eq!(request.domain(), RequestDomain::ProviderRead);

        let service = StateApplicationService;
        let mut caches = MemoryCaches::new();
        service.cache_spotify_search(&mut caches, "focus".to_string(), SearchResults::default());

        assert!(caches.search.contains_key("focus"));
    }

    #[test]
    fn spotify_session_evidence_fills_removed_web_api_premium_field() {
        assert_eq!(
            spotify_premium_status(None, Some(crate::config::SpotifyPremiumStatus::Premium)),
            crate::config::SpotifyPremiumStatus::Premium
        );
        assert_eq!(
            spotify_premium_status(None, None),
            crate::config::SpotifyPremiumStatus::Unknown
        );
        assert_eq!(
            spotify_premium_status(
                Some(rspotify::model::SubscriptionLevel::Free),
                Some(crate::config::SpotifyPremiumStatus::Premium)
            ),
            crate::config::SpotifyPremiumStatus::NotPremium
        );
    }

    #[test]
    fn spotify_session_can_be_ready_while_rate_limited_profile_is_missing() {
        let snapshot =
            spotify_auth_snapshot(None, Some(crate::config::SpotifyPremiumStatus::Premium));
        assert!(snapshot.session_ready);
        assert_eq!(
            snapshot.premium,
            crate::config::SpotifyPremiumStatus::Premium
        );
    }

    #[test]
    fn browse_transition_replaces_the_visible_category_set() {
        let service = StateApplicationService;
        let mut browse = BrowseData::default();
        service.apply_browse_categories(
            &mut browse,
            vec![Category {
                id: "focus".to_string(),
                name: "Focus".to_string(),
            }],
        );

        assert_eq!(browse.categories.len(), 1);
        assert_eq!(browse.categories[0].id, "focus");
        assert!(browse.categories_loaded);
    }
}
