#![allow(unused_imports)]
use chrono::TimeDelta;
use rspotify::model::{
    AlbumId, EpisodeId, Offset, PlayContextId, PlayableId, PlaylistId, ShowId, TrackId,
};
use souvlaki::MediaPosition;
use souvlaki::{MediaControlEvent, MediaControls, MediaMetadata, MediaPlayback, PlatformConfig};

use crate::state::{ContextId, Playback};
use crate::utils;
use crate::{
    client::{
        ActivePlaybackControl, ActivePlaybackSeek, ClientRequest, PlaybackNavigation, PlayerRequest,
    },
    config::ActiveProvider,
    state::SharedState,
    utils::map_join,
};

fn active_playback_control(event: &MediaControlEvent) -> Option<ActivePlaybackControl> {
    match event {
        MediaControlEvent::Play => Some(ActivePlaybackControl::Play),
        MediaControlEvent::Pause | MediaControlEvent::Stop => Some(ActivePlaybackControl::Pause),
        MediaControlEvent::Toggle => Some(ActivePlaybackControl::Toggle),
        _ => None,
    }
}

fn active_playback_seek(event: &MediaControlEvent) -> Option<ActivePlaybackSeek> {
    match event {
        MediaControlEvent::SetPosition(MediaPosition(position)) => {
            Some(ActivePlaybackSeek::Absolute(*position))
        }
        _ => None,
    }
}

fn playback_navigation_request(
    event: &MediaControlEvent,
    active_provider: ActiveProvider,
    has_unified_queue: bool,
) -> Option<ClientRequest> {
    let navigation = match event {
        MediaControlEvent::Next => PlaybackNavigation::Next,
        MediaControlEvent::Previous => PlaybackNavigation::Previous,
        _ => return None,
    };
    Some(navigation.request(active_provider, has_unified_queue))
}

fn parse_youtube_duration(duration: &str) -> Option<std::time::Duration> {
    let mut seconds = 0_u64;
    for part in duration.split(':') {
        seconds = seconds.checked_mul(60)?;
        seconds = seconds.checked_add(part.parse::<u64>().ok()?)?;
    }
    Some(std::time::Duration::from_secs(seconds))
}

fn clear_control_metadata(
    controls: &mut MediaControls,
    prev_info: &mut String,
) -> Result<(), souvlaki::Error> {
    controls.set_playback(MediaPlayback::Stopped)?;
    if !prev_info.is_empty() {
        controls.set_metadata(MediaMetadata::default())?;
        prev_info.clear();
    }
    Ok(())
}

fn update_control_metadata(
    state: &SharedState,
    controls: &mut MediaControls,
    prev_info: &mut String,
) -> Result<(), souvlaki::Error> {
    let browsing_provider = state.ui.lock().active_provider;
    let player = state.player.read();
    let active_provider = player.effective_playback_provider(browsing_provider);

    match active_provider {
        ActiveProvider::Spotify => match player.current_playback() {
            None => clear_control_metadata(controls, prev_info)?,
            Some(playback) => {
                let progress = player
                    .playback_progress()
                    .and_then(|p| Some(MediaPosition(p.to_std().ok()?)));

                if playback.is_playing {
                    controls.set_playback(MediaPlayback::Playing { progress })?;
                } else {
                    controls.set_playback(MediaPlayback::Paused { progress })?;
                }

                match playback.item.as_ref() {
                    None | Some(rspotify::model::PlayableItem::Unknown(_)) => {}
                    Some(rspotify::model::PlayableItem::Track(track)) => {
                        // only update metadata when the track information is changed
                        let track_info = format!("{}/{}", track.name, track.album.name);
                        if track_info != *prev_info {
                            controls.set_metadata(MediaMetadata {
                                title: Some(&track.name),
                                album: Some(&track.album.name),
                                artist: Some(&map_join(&track.artists, |a| &a.name, ", ")),
                                duration: track.duration.to_std().ok(),
                                cover_url: utils::get_track_album_image_url(track),
                            })?;

                            *prev_info = track_info;
                        }
                    }
                    Some(rspotify::model::PlayableItem::Episode(episode)) => {
                        // only update metadata when the episode information is changed
                        let episode_info = format!("{}/{}", episode.name, episode.show.name);
                        if episode_info != *prev_info {
                            controls.set_metadata(MediaMetadata {
                                title: Some(&episode.name),
                                album: Some(&episode.show.name),
                                artist: Some(&episode.show.name),
                                duration: episode.duration.to_std().ok(),
                                cover_url: utils::get_episode_show_image_url(episode),
                            })?;

                            *prev_info = episode_info;
                        }
                    }
                }
            }
        },
        ActiveProvider::YouTubeMusic => {
            let Some(playback) = player.youtube_playback.as_ref() else {
                clear_control_metadata(controls, prev_info)?;
                return Ok(());
            };
            let progress = Some(MediaPosition(playback.progress));
            if playback.is_playing {
                controls.set_playback(MediaPlayback::Playing { progress })?;
            } else {
                controls.set_playback(MediaPlayback::Paused { progress })?;
            }

            let track_info = format!("youtube:{}", playback.track.id);
            if track_info != *prev_info {
                let album = playback.track.album.as_deref();
                controls.set_metadata(MediaMetadata {
                    title: Some(&playback.track.name),
                    album,
                    artist: Some(&playback.track.artists),
                    duration: parse_youtube_duration(&playback.track.duration),
                    cover_url: playback.track.thumbnail_url.as_deref(),
                })?;
                *prev_info = track_info;
            }
        }
    }

    Ok(())
}

/// Start the application's media control event watcher
pub fn start_event_watcher(
    state: &SharedState,
    client_pub: crate::client::ClientRequestSender,
    shutdown: &tokio_util::sync::CancellationToken,
) -> Result<(), souvlaki::Error> {
    tracing::info!("Initializing application's media control event watcher...");

    #[cfg(not(target_os = "windows"))]
    let hwnd = None;

    #[cfg(target_os = "windows")]
    let (hwnd, _dummy_window) = {
        let dummy_window = windows::DummyWindow::new().unwrap();
        let handle = Some(dummy_window.handle.0.cast());
        (handle, dummy_window)
    };

    let config = PlatformConfig {
        dbus_name: "unified_player",
        display_name: "Unified Player",
        hwnd,
    };
    let mut controls = MediaControls::new(config)?;
    let state = state.clone();
    let event_state = state.clone();

    controls.attach(move |e| {
        tracing::info!("Received a media-control event");
        if let Some(control) = active_playback_control(&e) {
            client_pub
                .send(ClientRequest::ActivePlaybackControl(control))
                .unwrap_or_default();
            return;
        }
        if let Some(seek) = active_playback_seek(&e) {
            client_pub
                .send(ClientRequest::ActivePlaybackSeek(seek))
                .unwrap_or_default();
            return;
        }
        let browsing_provider = event_state.ui.lock().active_provider;
        let (active_provider, has_unified_queue) = {
            let player = event_state.player.read();
            (
                player.effective_playback_provider(browsing_provider),
                player.unified_queue.is_some(),
            )
        };
        if let Some(request) = playback_navigation_request(&e, active_provider, has_unified_queue) {
            client_pub.send(request).unwrap_or_default();
            return;
        }
        match e {
            MediaControlEvent::Play
            | MediaControlEvent::Pause
            | MediaControlEvent::Stop
            | MediaControlEvent::Toggle => {
                unreachable!("active playback controls return before provider routing")
            }
            MediaControlEvent::SetPosition(_) => {
                unreachable!("active playback seeks return before provider routing")
            }
            MediaControlEvent::Next | MediaControlEvent::Previous => {
                unreachable!("playback navigation returns before provider routing")
            }
            MediaControlEvent::SetVolume(volume) => {
                let volume = (volume.clamp(0.0, 1.0) * 100.0).round() as u8;
                let request = match active_provider {
                    ActiveProvider::Spotify => ClientRequest::Player(PlayerRequest::Volume(volume)),
                    ActiveProvider::YouTubeMusic => ClientRequest::YouTubePlayer(
                        crate::client::YouTubePlayerRequest::Volume(volume),
                    ),
                };
                client_pub.send(request).unwrap_or_default();
            }
            MediaControlEvent::OpenUri(uri) => {
                let capabilities =
                    event_state
                        .player
                        .read()
                        .playback_capabilities(match active_provider {
                            ActiveProvider::Spotify => crate::state::Provider::Spotify,
                            ActiveProvider::YouTubeMusic => crate::state::Provider::YouTubeMusic,
                        });
                if capabilities.device != crate::state::PlaybackDeviceMode::Selectable {
                    return;
                }
                let mut split = uri.split(':');
                let (Some("spotify"), Some(uri_type), Some(id)) =
                    (split.next(), split.next(), split.next())
                else {
                    return;
                };
                let id = id.to_string();
                let playback = match uri_type {
                    "album" => AlbumId::from_id(id)
                        .ok()
                        .map(|album_id| Playback::Context(ContextId::Album(album_id), None)),
                    "track" => TrackId::from_id(id)
                        .ok()
                        .map(|track_id| Playback::URIs(vec![PlayableId::Track(track_id)], None)),
                    "playlist" => PlaylistId::from_id(id).ok().map(|playlist_id| {
                        Playback::Context(ContextId::Playlist(playlist_id), None)
                    }),
                    "show" => ShowId::from_id(id)
                        .ok()
                        .map(|show_id| Playback::Context(ContextId::Show(show_id), None)),
                    "episode" => EpisodeId::from_id(id).ok().map(|episode_id| {
                        Playback::URIs(vec![PlayableId::Episode(episode_id)], None)
                    }),
                    _ => None,
                };
                if let Some(playback) = playback {
                    client_pub
                        .send(ClientRequest::Player(PlayerRequest::StartPlayback(
                            playback, None,
                        )))
                        .unwrap_or_default();
                }
            }

            _ => {}
        }
    })?;
    // For some reason, on startup, media playback needs to be initialized with `Playing`
    // for the track metadata to be shown up on the MacOS media status bar.
    controls.set_playback(MediaPlayback::Playing { progress: None })?;

    // The below refresh duration should be no less than 1s to avoid **overloading** linux dbus
    // handler provided by the souvlaki library, which only handles an event every 1s.
    // [1]: https://github.com/Sinono3/souvlaki/blob/b4d47bb2797ffdd625c17192df640510466762e1/src/platform/linux/mod.rs#L450
    let refresh_duration = std::time::Duration::from_secs(1);
    let mut info = String::new();
    while !shutdown.is_cancelled() {
        update_control_metadata(&state, &mut controls, &mut info)?;
        std::thread::sleep(refresh_duration);

        // this must be run repeatedly to ensure that
        // the Windows event queue is processed by the app
        #[cfg(target_os = "windows")]
        windows::pump_event_queue();
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{
        active_playback_control, active_playback_seek, parse_youtube_duration,
        playback_navigation_request,
    };
    use crate::{
        client::{ActivePlaybackControl, ActivePlaybackSeek, ClientRequest},
        config::ActiveProvider,
    };
    use souvlaki::{MediaControlEvent, MediaPosition};

    #[test]
    fn youtube_duration_metadata_uses_colon_parts() {
        assert_eq!(
            parse_youtube_duration("4:02"),
            Some(std::time::Duration::from_secs(242))
        );
        assert_eq!(
            parse_youtube_duration("1:02:03"),
            Some(std::time::Duration::from_secs(3723))
        );
        assert_eq!(parse_youtube_duration("not-a-duration"), None);
    }

    #[test]
    fn direct_media_controls_do_not_select_a_provider() {
        assert_eq!(
            active_playback_control(&MediaControlEvent::Play),
            Some(ActivePlaybackControl::Play)
        );
        assert_eq!(
            active_playback_control(&MediaControlEvent::Pause),
            Some(ActivePlaybackControl::Pause)
        );
        assert_eq!(
            active_playback_control(&MediaControlEvent::Stop),
            Some(ActivePlaybackControl::Pause)
        );
        assert_eq!(
            active_playback_control(&MediaControlEvent::Toggle),
            Some(ActivePlaybackControl::Toggle)
        );
        assert_eq!(active_playback_control(&MediaControlEvent::Next), None);
        assert_eq!(
            active_playback_seek(&MediaControlEvent::SetPosition(MediaPosition(
                std::time::Duration::from_secs(42)
            ))),
            Some(ActivePlaybackSeek::Absolute(
                std::time::Duration::from_secs(42)
            ))
        );
    }

    #[test]
    fn media_navigation_uses_the_terminal_unified_queue_policy() {
        assert!(matches!(
            playback_navigation_request(&MediaControlEvent::Next, ActiveProvider::Spotify, true),
            Some(ClientRequest::UnifiedNext)
        ));
        assert!(matches!(
            playback_navigation_request(
                &MediaControlEvent::Previous,
                ActiveProvider::YouTubeMusic,
                true
            ),
            Some(ClientRequest::UnifiedPrevious)
        ));
        assert!(matches!(
            playback_navigation_request(
                &MediaControlEvent::Next,
                ActiveProvider::YouTubeMusic,
                false
            ),
            Some(ClientRequest::YouTubePlayer(
                crate::client::YouTubePlayerRequest::Next
            ))
        ));
    }
}

// demonstrates how to make a minimal window to allow use of media keys on the command line
// ref: https://github.com/Sinono3/souvlaki/blob/master/examples/print_events.rs
#[cfg(target_os = "windows")]
#[allow(unsafe_code)] // used to interact with the Windows API
mod windows {
    use std::io::Error;
    use std::mem;

    use windows::core::w;
    use windows::core::PCWSTR;
    use windows::Win32::Foundation::{HWND, LPARAM, LRESULT, WPARAM};
    use windows::Win32::System::LibraryLoader::GetModuleHandleW;
    use windows::Win32::UI::WindowsAndMessaging::{
        CreateWindowExW, DefWindowProcW, DestroyWindow, DispatchMessageW, GetAncestor,
        IsDialogMessageW, PeekMessageW, RegisterClassExW, TranslateMessage, GA_ROOT, MSG,
        PM_REMOVE, WINDOW_EX_STYLE, WINDOW_STYLE, WM_QUIT, WNDCLASSEXW,
    };

    pub struct DummyWindow {
        pub handle: HWND,
    }

    impl DummyWindow {
        pub fn new() -> Result<DummyWindow, String> {
            let class_name = w!("SimpleTray");

            unsafe {
                let instance = GetModuleHandleW(None)
                    .map_err(|e| format!("Getting module handle failed: {e}"))?;

                let wnd_class = WNDCLASSEXW {
                    cbSize: mem::size_of::<WNDCLASSEXW>() as u32,
                    hInstance: instance.into(),
                    lpszClassName: class_name,
                    lpfnWndProc: Some(Self::wnd_proc),
                    ..Default::default()
                };

                if RegisterClassExW(&raw const wnd_class) == 0 {
                    return Err(format!(
                        "Registering class failed: {}",
                        Error::last_os_error()
                    ));
                }

                let handle = CreateWindowExW(
                    WINDOW_EX_STYLE::default(),
                    class_name,
                    w!(""),
                    WINDOW_STYLE::default(),
                    0,
                    0,
                    0,
                    0,
                    None,
                    None,
                    instance,
                    None,
                )
                .map_err(|e| format!("Failed to create window: {e}"))?;

                if handle.0.is_null() {
                    Err(format!(
                        "Message only window creation failed: {}",
                        Error::last_os_error()
                    ))
                } else {
                    Ok(DummyWindow { handle })
                }
            }
        }
        extern "system" fn wnd_proc(
            hwnd: HWND,
            msg: u32,
            wparam: WPARAM,
            lparam: LPARAM,
        ) -> LRESULT {
            unsafe { DefWindowProcW(hwnd, msg, wparam, lparam) }
        }
    }

    impl Drop for DummyWindow {
        fn drop(&mut self) {
            unsafe {
                DestroyWindow(self.handle).unwrap();
            }
        }
    }

    pub fn pump_event_queue() -> bool {
        unsafe {
            let mut msg: MSG = std::mem::zeroed();
            let mut has_message = PeekMessageW(&raw mut msg, None, 0, 0, PM_REMOVE).as_bool();
            while msg.message != WM_QUIT && has_message {
                if !IsDialogMessageW(GetAncestor(msg.hwnd, GA_ROOT), &raw const msg).as_bool() {
                    let _ = TranslateMessage(&raw const msg);
                    let _ = DispatchMessageW(&raw const msg);
                }

                has_message = PeekMessageW(&raw mut msg, None, 0, 0, PM_REMOVE).as_bool();
            }

            msg.message == WM_QUIT
        }
    }
}
