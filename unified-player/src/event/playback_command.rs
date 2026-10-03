use crate::{
    client::{
        ActivePlaybackControl, ActivePlaybackSeek, ClientRequest, PlaybackNavigation,
        PlayerRequest, YouTubePlayerRequest,
    },
    command::Command,
    config::ActiveProvider,
    state::{PageType, PlayerState},
};

#[derive(Clone, Copy)]
pub(super) struct PlaybackCommandSnapshot {
    active_provider: ActiveProvider,
    has_unified_queue: bool,
    spotify_volume: Option<u8>,
    youtube_volume: Option<u8>,
    youtube_mute_state: Option<u8>,
    _page_type: PageType,
    default_seek_seconds: u16,
    youtube_switch_allowed: bool,
}

impl PlaybackCommandSnapshot {
    pub(super) fn capture(
        player: Option<&PlayerState>,
        _command: Command,
        active_provider: ActiveProvider,
        page_type: PageType,
        default_seek_seconds: u16,
        youtube_switch_allowed: bool,
    ) -> Self {
        let youtube_playback = player.and_then(|player| player.youtube_playback.as_ref());
        Self {
            active_provider,
            has_unified_queue: player.is_some_and(|player| player.unified_queue.is_some()),
            spotify_volume: player
                .and_then(|player| player.buffered_playback.as_ref())
                .and_then(|playback| playback.volume)
                .map(|volume| volume.min(100) as u8),
            youtube_volume: youtube_playback.map(|playback| playback.volume.min(100)),
            youtube_mute_state: youtube_playback.and_then(|playback| playback.mute_state),
            _page_type: page_type,
            default_seek_seconds,
            youtube_switch_allowed,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum LocalPlaybackUpdate {
    ApplySpotifyVolume(u8),
    ApplyYouTubePlayback { volume: u8, mute_state: Option<u8> },
}

#[derive(Clone, Debug)]
pub(super) struct PlaybackCommandPlan {
    local_update: Option<LocalPlaybackUpdate>,
    requests: Vec<ClientRequest>,
}

impl PlaybackCommandPlan {
    fn empty() -> Self {
        Self {
            local_update: None,
            requests: Vec::new(),
        }
    }

    fn request(request: ClientRequest) -> Self {
        Self {
            local_update: None,
            requests: vec![request],
        }
    }

    fn with_update(update: LocalPlaybackUpdate, request: ClientRequest) -> Self {
        Self {
            local_update: Some(update),
            requests: vec![request],
        }
    }
}

pub(super) fn is_provider_playback_command(command: Command) -> bool {
    matches!(
        command,
        Command::NextTrack
            | Command::PreviousTrack
            | Command::ResumePause
            | Command::Repeat
            | Command::Shuffle
            | Command::VolumeChange { .. }
            | Command::Mute
            | Command::SeekStart
            | Command::SeekForward { .. }
            | Command::SeekBackward { .. }
            | Command::RefreshPlayback
            | Command::SwitchProvider
            | Command::SwitchPlaybackProvider
    )
}

pub(super) fn updates_local_playback(command: Command, active_provider: ActiveProvider) -> bool {
    matches!(command, Command::VolumeChange { .. })
        || (command == Command::Mute && active_provider == ActiveProvider::YouTubeMusic)
}

pub(super) fn reads_playback_state(command: Command) -> bool {
    matches!(
        command,
        Command::NextTrack | Command::PreviousTrack | Command::Repeat | Command::Shuffle
    )
}

pub(super) fn plan_provider_playback_command(
    command: Command,
    snapshot: PlaybackCommandSnapshot,
) -> Option<PlaybackCommandPlan> {
    use LocalPlaybackUpdate::{ApplySpotifyVolume, ApplyYouTubePlayback};

    let plan = match command {
        Command::NextTrack => PlaybackCommandPlan::request(
            PlaybackNavigation::Next.request(snapshot.active_provider, snapshot.has_unified_queue),
        ),
        Command::PreviousTrack => PlaybackCommandPlan::request(
            PlaybackNavigation::Previous
                .request(snapshot.active_provider, snapshot.has_unified_queue),
        ),
        Command::ResumePause => PlaybackCommandPlan::request(ClientRequest::ActivePlaybackControl(
            ActivePlaybackControl::Toggle,
        )),
        Command::Repeat => PlaybackCommandPlan::request(
            if snapshot.has_unified_queue
                || snapshot.active_provider == ActiveProvider::YouTubeMusic
            {
                ClientRequest::YouTubePlayer(YouTubePlayerRequest::Repeat)
            } else {
                ClientRequest::Player(PlayerRequest::Repeat)
            },
        ),
        Command::Shuffle => PlaybackCommandPlan::request(
            if snapshot.has_unified_queue
                || snapshot.active_provider == ActiveProvider::YouTubeMusic
            {
                ClientRequest::YouTubePlayer(YouTubePlayerRequest::Shuffle)
            } else {
                ClientRequest::Player(PlayerRequest::Shuffle)
            },
        ),
        Command::VolumeChange { offset } => {
            let current = match snapshot.active_provider {
                ActiveProvider::Spotify => snapshot.spotify_volume,
                ActiveProvider::YouTubeMusic => snapshot.youtube_volume,
            };
            current.map_or_else(PlaybackCommandPlan::empty, |current| {
                let volume = (i32::from(current) + offset).clamp(0, 100) as u8;
                if volume == current {
                    PlaybackCommandPlan::empty()
                } else {
                    match snapshot.active_provider {
                        ActiveProvider::Spotify => PlaybackCommandPlan::with_update(
                            ApplySpotifyVolume(volume),
                            ClientRequest::Player(PlayerRequest::Volume(volume)),
                        ),
                        ActiveProvider::YouTubeMusic => PlaybackCommandPlan::with_update(
                            ApplyYouTubePlayback {
                                volume,
                                mute_state: None,
                            },
                            ClientRequest::YouTubePlayer(YouTubePlayerRequest::Volume(volume)),
                        ),
                    }
                }
            })
        }
        Command::Mute => match snapshot.active_provider {
            ActiveProvider::Spotify => {
                PlaybackCommandPlan::request(ClientRequest::Player(PlayerRequest::ToggleMute))
            }
            ActiveProvider::YouTubeMusic => {
                let local_update = snapshot.youtube_volume.map(|current_volume| {
                    let (volume, mute_state) = match snapshot.youtube_mute_state {
                        Some(previous_volume) => (previous_volume.min(100), None),
                        None => (current_volume, Some(current_volume)),
                    };
                    ApplyYouTubePlayback { volume, mute_state }
                });
                PlaybackCommandPlan {
                    local_update,
                    requests: vec![ClientRequest::YouTubePlayer(
                        YouTubePlayerRequest::ToggleMute,
                    )],
                }
            }
        },
        Command::SeekStart => PlaybackCommandPlan::request(ClientRequest::ActivePlaybackSeek(
            ActivePlaybackSeek::Start,
        )),
        Command::SeekForward { duration } => {
            let offset = chrono::Duration::seconds(i64::from(
                duration.unwrap_or(snapshot.default_seek_seconds),
            ));
            PlaybackCommandPlan::request(ClientRequest::ActivePlaybackSeek(
                ActivePlaybackSeek::Relative(offset),
            ))
        }
        Command::SeekBackward { duration } => {
            let offset = chrono::Duration::seconds(-i64::from(
                duration.unwrap_or(snapshot.default_seek_seconds),
            ));
            PlaybackCommandPlan::request(ClientRequest::ActivePlaybackSeek(
                ActivePlaybackSeek::Relative(offset),
            ))
        }
        Command::RefreshPlayback => PlaybackCommandPlan::request(match snapshot.active_provider {
            ActiveProvider::Spotify => ClientRequest::GetCurrentPlayback,
            ActiveProvider::YouTubeMusic => {
                ClientRequest::YouTubePlayer(YouTubePlayerRequest::Refresh)
            }
        }),
        Command::SwitchProvider => {
            let target = snapshot.active_provider.toggled();
            if target == ActiveProvider::YouTubeMusic && !snapshot.youtube_switch_allowed {
                PlaybackCommandPlan::empty()
            } else {
                // The provider-switch handler owns the library refresh so
                // direct switch requests from setup and account flows get
                // the same guarantee as the keyboard command.
                let requests = vec![ClientRequest::SwitchProvider(target)];
                PlaybackCommandPlan {
                    local_update: None,
                    requests,
                }
            }
        }
        Command::SwitchPlaybackProvider => {
            let target = snapshot.active_provider.toggled();
            if target == ActiveProvider::YouTubeMusic && !snapshot.youtube_switch_allowed {
                PlaybackCommandPlan::empty()
            } else {
                PlaybackCommandPlan::request(ClientRequest::SwitchProvider(target))
            }
        }
        _ => return None,
    };

    Some(plan)
}

pub(super) fn apply_local_effects(
    player: Option<&mut PlayerState>,
    plan: PlaybackCommandPlan,
) -> Vec<ClientRequest> {
    if let Some(local_update) = plan.local_update {
        match local_update {
            LocalPlaybackUpdate::ApplySpotifyVolume(volume) => {
                let player = player.expect("volume plans are applied with the player write lock");
                if let Some(playback) = player.buffered_playback.as_mut() {
                    playback.volume = Some(u32::from(volume));
                }
            }
            LocalPlaybackUpdate::ApplyYouTubePlayback { volume, mute_state } => {
                let player = player.expect("volume plans are applied with the player write lock");
                if let Some(playback) = player.youtube_playback.as_mut() {
                    playback.volume = volume;
                    playback.mute_state = mute_state;
                }
            }
        }
    }
    plan.requests
}

#[cfg(test)]
mod tests {
    use super::*;

    fn snapshot(active_provider: ActiveProvider) -> PlaybackCommandSnapshot {
        PlaybackCommandSnapshot {
            active_provider,
            has_unified_queue: false,
            spotify_volume: Some(40),
            youtube_volume: Some(40),
            youtube_mute_state: None,
            _page_type: PageType::Search,
            default_seek_seconds: 5,
            youtube_switch_allowed: true,
        }
    }

    fn requests(command: Command, snapshot: PlaybackCommandSnapshot) -> Vec<ClientRequest> {
        plan_provider_playback_command(command, snapshot)
            .expect("playback command")
            .requests
    }

    fn assert_spotify_request(request: &ClientRequest) {
        assert!(
            matches!(
                request,
                ClientRequest::Player(_) | ClientRequest::GetCurrentPlayback
            ),
            "expected a Spotify-only request, got {request:?}"
        );
    }

    fn assert_youtube_request(request: &ClientRequest) {
        assert!(
            matches!(request, ClientRequest::YouTubePlayer(_)),
            "expected a YouTube-only request, got {request:?}"
        );
    }

    #[test]
    fn spotify_transport_commands_never_emit_youtube_requests() {
        let cases = [
            Command::NextTrack,
            Command::PreviousTrack,
            Command::Repeat,
            Command::Shuffle,
            Command::VolumeChange { offset: 5 },
            Command::Mute,
            Command::RefreshPlayback,
        ];

        for command in cases {
            let requests = requests(command, snapshot(ActiveProvider::Spotify));
            assert_eq!(
                requests.len(),
                1,
                "unexpected request count for {command:?}"
            );
            assert_spotify_request(&requests[0]);
        }
    }

    #[test]
    fn youtube_transport_commands_never_emit_spotify_requests() {
        let cases = [
            Command::NextTrack,
            Command::PreviousTrack,
            Command::Repeat,
            Command::Shuffle,
            Command::VolumeChange { offset: 5 },
            Command::Mute,
            Command::RefreshPlayback,
        ];

        for command in cases {
            let requests = requests(command, snapshot(ActiveProvider::YouTubeMusic));
            assert_eq!(
                requests.len(),
                1,
                "unexpected request count for {command:?}"
            );
            assert_youtube_request(&requests[0]);
        }
    }

    #[test]
    fn state_independent_commands_plan_without_a_player_lock() {
        let cases = [
            Command::ResumePause,
            Command::SeekStart,
            Command::SeekForward { duration: Some(7) },
            Command::SeekBackward { duration: Some(7) },
            Command::RefreshPlayback,
            Command::SwitchProvider,
            Command::SwitchPlaybackProvider,
        ];

        for provider in [ActiveProvider::Spotify, ActiveProvider::YouTubeMusic] {
            for command in cases {
                assert!(!reads_playback_state(command));
                let snapshot = PlaybackCommandSnapshot::capture(
                    None,
                    command,
                    provider,
                    PageType::Search,
                    5,
                    true,
                );
                assert!(plan_provider_playback_command(command, snapshot).is_some());
            }
        }

        assert!(!reads_playback_state(Command::Mute));
        assert!(!updates_local_playback(
            Command::Mute,
            ActiveProvider::Spotify
        ));
    }

    #[test]
    fn next_previous_and_queue_modes_keep_their_existing_routes() {
        let mut state = snapshot(ActiveProvider::Spotify);
        assert!(matches!(
            requests(Command::NextTrack, state)[0],
            ClientRequest::Player(PlayerRequest::NextTrack)
        ));
        assert!(matches!(
            requests(Command::PreviousTrack, state)[0],
            ClientRequest::Player(PlayerRequest::PreviousTrack)
        ));

        state.active_provider = ActiveProvider::YouTubeMusic;
        assert!(matches!(
            requests(Command::NextTrack, state)[0],
            ClientRequest::YouTubePlayer(YouTubePlayerRequest::Next)
        ));
        assert!(matches!(
            requests(Command::PreviousTrack, state)[0],
            ClientRequest::YouTubePlayer(YouTubePlayerRequest::Previous)
        ));

        state.has_unified_queue = true;
        assert!(matches!(
            requests(Command::NextTrack, state)[0],
            ClientRequest::UnifiedNext
        ));
        assert!(matches!(
            requests(Command::PreviousTrack, state)[0],
            ClientRequest::UnifiedPrevious
        ));
        assert!(matches!(
            requests(Command::Repeat, state)[0],
            ClientRequest::YouTubePlayer(YouTubePlayerRequest::Repeat)
        ));
        assert!(matches!(
            requests(Command::Shuffle, state)[0],
            ClientRequest::YouTubePlayer(YouTubePlayerRequest::Shuffle)
        ));
    }

    #[test]
    fn toggle_is_provider_neutral_even_when_the_snapshot_owner_is_stale() {
        for provider in [ActiveProvider::Spotify, ActiveProvider::YouTubeMusic] {
            assert!(matches!(
                requests(Command::ResumePause, snapshot(provider))[0],
                ClientRequest::ActivePlaybackControl(ActivePlaybackControl::Toggle)
            ));
        }
    }

    #[test]
    fn repeat_shuffle_and_refresh_map_to_exact_provider_variants() {
        let spotify = snapshot(ActiveProvider::Spotify);
        assert!(matches!(
            requests(Command::Repeat, spotify)[0],
            ClientRequest::Player(PlayerRequest::Repeat)
        ));
        assert!(matches!(
            requests(Command::Shuffle, spotify)[0],
            ClientRequest::Player(PlayerRequest::Shuffle)
        ));
        assert!(matches!(
            requests(Command::RefreshPlayback, spotify)[0],
            ClientRequest::GetCurrentPlayback
        ));

        let youtube = snapshot(ActiveProvider::YouTubeMusic);
        assert!(matches!(
            requests(Command::Repeat, youtube)[0],
            ClientRequest::YouTubePlayer(YouTubePlayerRequest::Repeat)
        ));
        assert!(matches!(
            requests(Command::Shuffle, youtube)[0],
            ClientRequest::YouTubePlayer(YouTubePlayerRequest::Shuffle)
        ));
        assert!(matches!(
            requests(Command::RefreshPlayback, youtube)[0],
            ClientRequest::YouTubePlayer(YouTubePlayerRequest::Refresh)
        ));
    }

    #[test]
    fn volume_plans_preserve_immediate_state_updates_and_provider_requests() {
        let spotify = plan_provider_playback_command(
            Command::VolumeChange { offset: 70 },
            snapshot(ActiveProvider::Spotify),
        )
        .unwrap();
        assert!(matches!(
            spotify.local_update,
            Some(LocalPlaybackUpdate::ApplySpotifyVolume(100))
        ));
        assert!(matches!(
            spotify.requests.as_slice(),
            [ClientRequest::Player(PlayerRequest::Volume(100))]
        ));

        let youtube = plan_provider_playback_command(
            Command::VolumeChange { offset: -15 },
            snapshot(ActiveProvider::YouTubeMusic),
        )
        .unwrap();
        assert!(matches!(
            youtube.local_update,
            Some(LocalPlaybackUpdate::ApplyYouTubePlayback {
                volume: 25,
                mute_state: None
            })
        ));
        assert!(matches!(
            youtube.requests.as_slice(),
            [ClientRequest::YouTubePlayer(YouTubePlayerRequest::Volume(
                25
            ))]
        ));
    }

    #[test]
    fn volume_without_metadata_or_at_a_bound_is_a_handled_noop() {
        let mut state = snapshot(ActiveProvider::Spotify);
        state.spotify_volume = None;
        assert!(requests(Command::VolumeChange { offset: 5 }, state).is_empty());

        state.spotify_volume = Some(100);
        assert!(requests(Command::VolumeChange { offset: 5 }, state).is_empty());
    }

    #[test]
    fn youtube_mute_plan_preserves_saved_volume_and_request() {
        let muted =
            plan_provider_playback_command(Command::Mute, snapshot(ActiveProvider::YouTubeMusic))
                .unwrap();
        assert!(matches!(
            muted.local_update,
            Some(LocalPlaybackUpdate::ApplyYouTubePlayback {
                volume: 40,
                mute_state: Some(40)
            })
        ));
        assert!(matches!(
            muted.requests.as_slice(),
            [ClientRequest::YouTubePlayer(
                YouTubePlayerRequest::ToggleMute
            )]
        ));

        let mut state = snapshot(ActiveProvider::YouTubeMusic);
        state.youtube_volume = Some(0);
        state.youtube_mute_state = Some(130);
        let unmuted = plan_provider_playback_command(Command::Mute, state).unwrap();
        assert!(matches!(
            unmuted.local_update,
            Some(LocalPlaybackUpdate::ApplyYouTubePlayback {
                volume: 100,
                mute_state: None
            })
        ));
        assert!(matches!(
            unmuted.requests.as_slice(),
            [ClientRequest::YouTubePlayer(
                YouTubePlayerRequest::ToggleMute
            )]
        ));
    }

    #[test]
    fn spotify_mute_maps_to_the_spotify_toggle_only() {
        let requests = requests(Command::Mute, snapshot(ActiveProvider::Spotify));
        assert!(matches!(
            requests.as_slice(),
            [ClientRequest::Player(PlayerRequest::ToggleMute)]
        ));
    }

    #[test]
    fn seek_plans_are_provider_neutral_even_when_the_snapshot_owner_is_stale() {
        for provider in [ActiveProvider::Spotify, ActiveProvider::YouTubeMusic] {
            assert!(matches!(
                requests(Command::SeekStart, snapshot(provider))[0],
                ClientRequest::ActivePlaybackSeek(ActivePlaybackSeek::Start)
            ));
            assert!(matches!(
                requests(Command::SeekForward { duration: None }, snapshot(provider))[0],
                ClientRequest::ActivePlaybackSeek(ActivePlaybackSeek::Relative(offset))
                    if offset == chrono::Duration::seconds(5)
            ));
            assert!(matches!(
                requests(
                    Command::SeekBackward { duration: Some(7) },
                    snapshot(provider)
                )[0],
                ClientRequest::ActivePlaybackSeek(ActivePlaybackSeek::Relative(offset))
                    if offset == chrono::Duration::seconds(-7)
            ));
        }
    }

    #[test]
    fn provider_switch_routes_through_the_central_switch_handler_from_any_page() {
        for page_type in [
            PageType::Welcome,
            PageType::Library,
            PageType::Context,
            PageType::YouTubeContext,
            PageType::UnifiedPlaylist,
            PageType::Search,
            PageType::Browse,
            PageType::Lyrics,
            PageType::Journal,
            PageType::JournalLists,
            PageType::JournalList,
            PageType::Queue,
            PageType::Settings,
            PageType::CommandHelp,
            PageType::Logs,
        ] {
            let mut state = snapshot(ActiveProvider::Spotify);
            state._page_type = page_type;
            let effects = requests(Command::SwitchProvider, state);
            assert_eq!(effects.len(), 1);
            assert!(matches!(
                effects[0],
                ClientRequest::SwitchProvider(ActiveProvider::YouTubeMusic)
            ));
        }
    }

    #[test]
    fn provider_switch_auth_gate_and_reverse_route_are_explicit() {
        let mut spotify = snapshot(ActiveProvider::Spotify);
        spotify.youtube_switch_allowed = false;
        assert!(requests(Command::SwitchProvider, spotify).is_empty());

        let youtube = snapshot(ActiveProvider::YouTubeMusic);
        let effects = requests(Command::SwitchProvider, youtube);
        assert_eq!(effects.len(), 1);
        assert!(matches!(
            effects[0],
            ClientRequest::SwitchProvider(ActiveProvider::Spotify)
        ));
    }

    #[test]
    fn playback_provider_takeover_routes_from_the_actual_owner() {
        let youtube = snapshot(ActiveProvider::YouTubeMusic);
        let spotify = snapshot(ActiveProvider::Spotify);

        assert!(matches!(
            requests(Command::SwitchPlaybackProvider, youtube).as_slice(),
            [ClientRequest::SwitchProvider(ActiveProvider::Spotify)]
        ));
        assert!(matches!(
            requests(Command::SwitchPlaybackProvider, spotify).as_slice(),
            [ClientRequest::SwitchProvider(ActiveProvider::YouTubeMusic)]
        ));
    }

    #[test]
    fn playback_provider_takeover_honors_the_youtube_auth_gate() {
        let mut spotify = snapshot(ActiveProvider::Spotify);
        spotify.youtube_switch_allowed = false;

        assert!(requests(Command::SwitchPlaybackProvider, spotify).is_empty());
    }
}
