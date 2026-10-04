use crate::{
    client::AppClient,
    config,
    state::{SharedState, SpotifyPlaybackEvent},
};
use anyhow::Context;
use librespot_connect::{ConnectConfig, Spirc};
use librespot_core::authentication::Credentials;
use librespot_core::config::DeviceType;
use librespot_core::{spotify_uri, Session, SpotifyUri};
#[cfg(any(test, not(feature = "rodio-backend")))]
use librespot_playback::audio_backend::SinkError;
use librespot_playback::audio_backend::{Sink, SinkResult};
use librespot_playback::mixer::MixerConfig;
#[cfg(not(feature = "rodio-backend"))]
use librespot_playback::{audio_backend, config::AudioFormat};
use librespot_playback::{
    config::{Bitrate, PlayerConfig},
    mixer::{self, Mixer},
    player,
};
use librespot_playback::{convert::Converter, decoder::AudioPacket};
use rspotify::model::{EpisodeId, Id, PlayableId, TrackId};
use serde::Serialize;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;

#[cfg(feature = "rodio-backend")]
mod rodio_output;

/// Whether the next streaming connection is the first one of the process.
///
/// Used to scope `pause_on_startup` to application startup only, so that
/// reconnecting mid-session (e.g. via `RestartIntegratedClient`) does not
/// pause an intentionally playing track.
static IS_FIRST_CONNECTION: AtomicBool = AtomicBool::new(true);

/// Identifies streaming connections so session events of a replaced
/// connection cannot change the active state of its successor.
static NEXT_CONNECTION_ID: AtomicU64 = AtomicU64::new(0);

#[cfg(not(any(
    feature = "rodio-backend",
    feature = "alsa-backend",
    feature = "pulseaudio-backend",
    feature = "portaudio-backend",
    feature = "jackaudio-backend",
    feature = "rodiojack-backend",
    feature = "sdl-backend",
    feature = "gstreamer-backend"
)))]
compile_error!("Streaming feature is enabled but no audio backend has been selected. Consider adding one of the following features:
    rodio-backend,
    alsa-backend,
    pulseaudio-backend,
    portaudio-backend,
    jackaudio-backend,
    rodiojack-backend,
    sdl-backend,
    gstreamer-backend
For more information, visit https://github.com/bababoyy/unified-player?tab=readme-ov-file#streaming
");

#[derive(Debug, Serialize)]
enum PlayerEvent {
    VolumeChanged {
        volume: u8,
    },
    Changed {
        playable_id: PlayableId<'static>,
    },
    Playing {
        playable_id: PlayableId<'static>,
        position_ms: u32,
    },
    Paused {
        playable_id: PlayableId<'static>,
        position_ms: u32,
    },
    EndOfTrack {
        playable_id: PlayableId<'static>,
    },
}

impl PlayerEvent {
    const fn kind(&self) -> &'static str {
        match self {
            Self::VolumeChanged { .. } => "volume_changed",
            Self::Changed { .. } => "changed",
            Self::Playing { .. } => "playing",
            Self::Paused { .. } => "paused",
            Self::EndOfTrack { .. } => "end_of_track",
        }
    }

    /// gets the event's arguments
    pub fn args(&self) -> Vec<String> {
        match self {
            PlayerEvent::VolumeChanged { volume } => {
                vec!["VolumeChanged".to_string(), volume.to_string()]
            }
            PlayerEvent::Changed { playable_id } => {
                vec!["Changed".to_string(), playable_id.uri()]
            }
            PlayerEvent::Playing {
                playable_id,
                position_ms,
            } => vec![
                "Playing".to_string(),
                playable_id.uri(),
                position_ms.to_string(),
            ],
            PlayerEvent::Paused {
                playable_id,
                position_ms,
            } => vec![
                "Paused".to_string(),
                playable_id.uri(),
                position_ms.to_string(),
            ],
            PlayerEvent::EndOfTrack { playable_id } => {
                vec!["EndOfTrack".to_string(), playable_id.uri()]
            }
        }
    }

    fn as_state_event(&self) -> SpotifyPlaybackEvent {
        match self {
            Self::VolumeChanged { volume } => {
                SpotifyPlaybackEvent::VolumeChanged { volume: *volume }
            }
            Self::Changed { playable_id } => SpotifyPlaybackEvent::Changed {
                playable_id: playable_id.clone(),
            },
            Self::Playing {
                playable_id,
                position_ms,
            } => SpotifyPlaybackEvent::Playing {
                playable_id: playable_id.clone(),
                position_ms: *position_ms,
            },
            Self::Paused {
                playable_id,
                position_ms,
            } => SpotifyPlaybackEvent::Paused {
                playable_id: playable_id.clone(),
                position_ms: *position_ms,
            },
            Self::EndOfTrack { playable_id } => SpotifyPlaybackEvent::EndOfTrack {
                playable_id: playable_id.clone(),
            },
        }
    }
}

fn should_reconcile_spotify_event(
    visible_provider: config::ActiveProvider,
    needs_refresh: bool,
) -> bool {
    visible_provider == config::ActiveProvider::Spotify && needs_refresh
}

fn librespot_volume_to_percent(volume: u16) -> u8 {
    (((u32::from(volume) * 100) + 32_767) / 65_535).min(100) as u8
}

fn spotify_id_to_playable_id(uri: &spotify_uri::SpotifyUri) -> anyhow::Result<PlayableId<'static>> {
    match uri {
        SpotifyUri::Track { .. } => {
            let uri = uri.to_uri()?;
            Ok(TrackId::from_uri(&uri)?.into_static().into())
        }
        SpotifyUri::Episode { .. } => {
            let uri = uri.to_uri()?;
            Ok(EpisodeId::from_uri(&uri)?.into_static().into())
        }
        _ => anyhow::bail!("unexpected spotify_id {uri:?}"),
    }
}

impl PlayerEvent {
    pub fn from_librespot_player_event(e: player::PlayerEvent) -> anyhow::Result<Option<Self>> {
        Ok(match e {
            player::PlayerEvent::TrackChanged { audio_item } => Some(PlayerEvent::Changed {
                playable_id: spotify_id_to_playable_id(&audio_item.track_id)?,
            }),
            player::PlayerEvent::Playing {
                track_id,
                position_ms,
                ..
            } => Some(PlayerEvent::Playing {
                playable_id: spotify_id_to_playable_id(&track_id)?,
                position_ms,
            }),
            player::PlayerEvent::Paused {
                track_id,
                position_ms,
                ..
            } => Some(PlayerEvent::Paused {
                playable_id: spotify_id_to_playable_id(&track_id)?,
                position_ms,
            }),
            player::PlayerEvent::EndOfTrack { track_id, .. } => Some(PlayerEvent::EndOfTrack {
                playable_id: spotify_id_to_playable_id(&track_id)?,
            }),
            player::PlayerEvent::VolumeChanged { volume } => Some(PlayerEvent::VolumeChanged {
                volume: librespot_volume_to_percent(volume),
            }),
            _ => None,
        })
    }
}

const fn librespot_event_kind(event: &player::PlayerEvent) -> &'static str {
    use player::PlayerEvent as E;
    match event {
        E::PlayRequestIdChanged { .. } => "play_request_id_changed",
        E::Stopped { .. } => "stopped",
        E::Loading { .. } => "loading",
        E::Preloading { .. } => "preloading",
        E::Playing { .. } => "playing",
        E::Paused { .. } => "paused",
        E::TimeToPreloadNextTrack { .. } => "time_to_preload_next_track",
        E::EndOfTrack { .. } => "end_of_track",
        E::Unavailable { .. } => "unavailable",
        E::VolumeChanged { .. } => "volume_changed",
        E::PositionCorrection { .. } => "position_correction",
        E::PositionChanged { .. } => "position_changed",
        E::Seeked { .. } => "seeked",
        E::TrackChanged { .. } => "track_changed",
        E::SessionConnected { .. } => "session_connected",
        E::SessionDisconnected { .. } => "session_disconnected",
        E::SessionClientChanged { .. } => "session_client_changed",
        E::ShuffleChanged { .. } => "shuffle_changed",
        E::RepeatChanged { .. } => "repeat_changed",
        E::AutoPlayChanged { .. } => "auto_play_changed",
        E::FilterExplicitContentChanged { .. } => "filter_explicit_content_changed",
    }
}

fn execute_player_event_hook_command(
    cmd: &config::Command,
    event: &PlayerEvent,
) -> anyhow::Result<()> {
    cmd.execute(Some(event.args()))?;

    Ok(())
}

/// Create a new streaming connection
pub async fn new_connection(
    client: AppClient,
    state: SharedState,
    session: Session,
    creds: Credentials,
) -> anyhow::Result<Spirc> {
    let configs = config::get_config();
    let device = &configs.app_config.device;

    // `librespot` volume is a u16 number ranging from 0 to 65535,
    // while a percentage volume value (from 0 to 100) is used for the device configuration.
    // So we need to convert from one format to another
    let volume = (f64::from(std::cmp::min(device.volume, 100_u8)) / 100.0 * 65535.0).round() as u16;

    let connect_config = ConnectConfig {
        name: device.name.clone(),
        device_type: device.device_type.parse::<DeviceType>().unwrap_or_default(),
        initial_volume: volume,

        // non-configurable fields, use default values.
        // We may allow users to configure these fields in a future release
        is_group: false,
        disable_volume: false,
        volume_steps: 64,
    };

    tracing::info!("Spotify Connect configuration prepared");

    let mixer = Arc::new(
        mixer::softmixer::SoftMixer::open(MixerConfig::default()).context("opening softmixer")?,
    );
    mixer.set_volume(volume);

    let open_output = output_opener();
    let player_config = PlayerConfig {
        bitrate: device
            .bitrate
            .to_string()
            .parse::<Bitrate>()
            .unwrap_or_default(),
        normalisation: device.normalization,
        ..Default::default()
    };

    tracing::info!("Initializing a new integrated Spotify player");

    let player = {
        // Clone the Option<Arc<...>> so the factory closure can move it.
        // vis_bands is Some iff enable_audio_visualization is true.
        let vis_bands = state.vis_bands.as_ref().map(Arc::clone);
        player::Player::new(
            player_config,
            session.clone(),
            mixer.get_soft_volume(),
            move || -> Box<dyn Sink> {
                let real: Box<dyn Sink> = Box::new(ReleasingSink::new(open_output));
                if let Some(ref bands) = vis_bands {
                    Box::new(crate::ui::streaming::VisualizationSink::new(
                        real,
                        Arc::clone(bands),
                        // librespot defaults to 44100 Hz; adjust here if
                        // PlayerConfig::sample_rate is changed in the future.
                        44_100.0,
                    ))
                } else {
                    real
                }
            },
        )
    };

    // When `pause_on_startup` is enabled, suppress Spotify's auto-resume of the
    // previous session by pausing the first auto-started playback. Scoped to the
    // first connection of the process so mid-session reconnects are unaffected.
    let pause_on_startup =
        configs.app_config.pause_on_startup && IS_FIRST_CONNECTION.swap(false, Ordering::SeqCst);
    let spotify_queue_completion = client.register_spotify_queue_completion_slot();
    let connection_id = NEXT_CONNECTION_ID.fetch_add(1, Ordering::Relaxed);
    let connection_state = state.clone();

    let player_event_task = tokio::task::spawn({
        let mut channel = player.get_player_event_channel();
        async move {
            let mut pause_armed = pause_on_startup;
            while let Some(event) = channel.recv().await {
                // `state` is one of the fields the diagnostics sink keeps, so
                // the raw event sequence survives into incident evidence.
                tracing::debug!(
                    state = librespot_event_kind(&event),
                    "Received a librespot player event"
                );
                match &event {
                    player::PlayerEvent::SessionConnected { .. } => state
                        .player
                        .write()
                        .apply_integrated_session_event(connection_id, true),
                    player::PlayerEvent::SessionDisconnected { .. } => state
                        .player
                        .write()
                        .apply_integrated_session_event(connection_id, false),
                    player::PlayerEvent::Seeked {
                        track_id,
                        position_ms,
                        ..
                    }
                    | player::PlayerEvent::PositionCorrection {
                        track_id,
                        position_ms,
                        ..
                    } => {
                        if let Ok(uri) = track_id.to_uri() {
                            state
                                .player
                                .write()
                                .apply_integrated_position(&uri, *position_ms);
                        }
                    }
                    _ => {}
                }
                // Suppress Spotify's auto-resume of the previous session on
                // startup. The `librespot` connect transfer finalizes the
                // play state asynchronously, so a single reactive pause is not
                // reliable on its own:
                if pause_armed {
                    match &event {
                        // Best-effort: pause as the track starts loading, before
                        // the audio sink starts, so no audible blip occurs. This
                        // is a no-op if the transfer has not set the play state
                        // yet, so we do NOT disarm here.
                        player::PlayerEvent::Loading { .. } => {
                            tracing::info!(
                                state = "startup_pause_on_loading",
                                "Pausing Spotify's resumed session on startup"
                            );
                            client.pause_streaming_on_startup();
                        }
                        // Authoritative: playback actually started (the transfer
                        // finalized into "playing"). Pause and stop interfering.
                        player::PlayerEvent::Playing { .. } => {
                            tracing::info!(
                                state = "startup_pause_on_playing",
                                "Pausing Spotify's resumed session on startup"
                            );
                            if client.pause_streaming_on_startup() {
                                pause_armed = false;
                            }
                        }
                        // The track finished loading already paused, i.e. the
                        // `Loading` pause above took effect and no audio played.
                        player::PlayerEvent::Paused { .. } => {
                            pause_armed = false;
                        }
                        _ => {}
                    }
                }

                match PlayerEvent::from_librespot_player_event(event) {
                    Err(err) => {
                        crate::observability::log_safe_error!(
                            warn,
                            crate::observability::DiagnosticCode::PLAYER_EVENT_CONVERT_FAILED,
                            crate::observability::ErrorCategory::Contract,
                            &err,
                            "Failed to convert a librespot player event"
                        );
                    }
                    Ok(Some(event)) => {
                        tracing::info!(state = event.kind(), "Received a Spotify player event");
                        let visible_provider = state.ui.lock().active_provider;
                        // Keep the Spotify session projection current even while
                        // YouTube is the visible provider. Only the REST
                        // reconciliation remains scoped to the Spotify surface.
                        let needs_refresh = state
                            .player
                            .write()
                            .apply_spotify_playback_event(&event.as_state_event());
                        let should_reconcile =
                            should_reconcile_spotify_event(visible_provider, needs_refresh);
                        if !should_reconcile {
                            client.cancel_spotify_playback_update();
                        }
                        if let PlayerEvent::Changed { playable_id }
                        | PlayerEvent::Playing { playable_id, .. } = &event
                        {
                            let media = crate::state::PlayableMedia::Spotify(playable_id.clone());
                            let completion = client
                                .capture_unified_queue_completion(
                                    &state,
                                    config::ActiveProvider::Spotify,
                                    &media,
                                )
                                .map(|completion| (playable_id.clone(), completion));
                            AppClient::set_spotify_queue_completion(
                                &spotify_queue_completion,
                                completion,
                            );
                        }
                        match event {
                            PlayerEvent::Playing { .. } => {
                                if let Some(ref bands) = state.vis_bands {
                                    bands.lock().is_active = true;
                                }
                            }
                            PlayerEvent::Paused { .. } => {
                                if let Some(ref bands) = state.vis_bands {
                                    bands.lock().is_active = false;
                                }
                            }
                            PlayerEvent::EndOfTrack { ref playable_id } => {
                                let completion = AppClient::take_spotify_queue_completion(
                                    &spotify_queue_completion,
                                    playable_id,
                                );
                                if let Some(completion) = completion {
                                    client.emit_unified_queue_completion(completion);
                                }
                            }
                            PlayerEvent::VolumeChanged { .. } | PlayerEvent::Changed { .. } => {}
                        }
                        if should_reconcile {
                            client.update_playback(&state);
                        }
                        // This player's own metadata shows the new track; the
                        // scheduled Web API read stays only as the fallback.
                        if let PlayerEvent::Changed { ref playable_id } = event {
                            if should_reconcile
                                && state.player.read().active_integrated_device_id().is_some()
                            {
                                let client = client.clone();
                                let state = state.clone();
                                let id = playable_id.clone();
                                tokio::task::spawn(async move {
                                    if let Err(err) =
                                        client.project_integrated_track_change(&state, id).await
                                    {
                                        crate::observability::log_safe_error!(
                                            debug,
                                            crate::observability::DiagnosticCode::SPOTIFY_PLAYBACK_REFRESH_FAILED,
                                            crate::observability::ErrorCategory::Unavailable,
                                            &err,
                                            "Integrated track metadata unavailable; using the Web API read"
                                        );
                                    }
                                });
                            }
                        }

                        // execute a player event hook command
                        if let Some(ref cmd) = configs.app_config.player_event_hook_command {
                            if let Err(err) = execute_player_event_hook_command(cmd, &event) {
                                crate::observability::log_safe_error!(
                                    warn,
                                    crate::observability::DiagnosticCode::EVENT_HOOK_FAILED,
                                    crate::observability::ErrorCategory::ExternalCommand,
                                    &err,
                                    "Failed to execute the player event hook command"
                                );
                            }
                        }
                    }
                    Ok(None) => {}
                }
            }
            client.release_spotify_queue_completion_slot(&spotify_queue_completion);
        }
    });

    tracing::info!("Starting an integrated Spotify player using librespot's spirc protocol");

    let (spirc, spirc_task) = Spirc::new(connect_config, session, creds, player, mixer)
        .await
        .context("initialize spirc")?;

    tokio::task::spawn(async move {
        tokio::select! {
            () = spirc_task => {},
            _ = player_event_task => {}
        }
        // A connection that ends without a disconnect event can no longer take commands.
        connection_state
            .player
            .write()
            .apply_integrated_session_event(connection_id, false);
    });

    tracing::info!("New streaming connection has been established!");

    Ok(spirc)
}

/// The audio output factory for the integrated player.
#[cfg(feature = "rodio-backend")]
fn output_opener() -> impl FnMut() -> SinkResult<Box<dyn Sink>> + Send + 'static {
    rodio_output::open
}

/// The audio output factory for the integrated player.
///
/// Alternate backends still panic when their device cannot be opened, so the
/// panic is converted into a start failure here.
#[cfg(not(feature = "rodio-backend"))]
fn output_opener() -> impl FnMut() -> SinkResult<Box<dyn Sink>> + Send + 'static {
    let backend = audio_backend::find(None).expect("should be able to find an audio backend");
    move || {
        std::panic::catch_unwind(|| backend(None, AudioFormat::default())).map_err(|_| {
            SinkError::ConnectionRefused("audio output could not be opened".to_owned())
        })
    }
}

/// Opens the audio backend only while librespot is playing.
///
/// librespot keeps one sink for the player's lifetime, and the rodio backend's
/// `stop` only pauses it, so an idle or switched-away Spotify player would
/// stream silence and keep the audio device awake. librespot stops the sink on
/// pause and stop (not between gapless tracks), so releasing it there costs one
/// reopen per resume.
///
/// A failed open is reported as a start failure, which librespot handles by
/// pausing; the next resume tries the device again.
struct ReleasingSink<F> {
    open: F,
    active: Option<Box<dyn Sink>>,
}

impl<F: FnMut() -> SinkResult<Box<dyn Sink>>> ReleasingSink<F> {
    fn new(open: F) -> Self {
        Self { open, active: None }
    }

    fn active(&mut self) -> SinkResult<&mut Box<dyn Sink>> {
        if self.active.is_none() {
            let opened = (self.open)().and_then(|mut sink| sink.start().map(|()| sink));
            match opened {
                Ok(sink) => {
                    crate::observability::set_component_health(
                        crate::observability::Component::Audio,
                        crate::observability::HealthStatus::Healthy,
                        "output-ready",
                    );
                    self.active = Some(sink);
                }
                Err(err) => {
                    crate::observability::set_component_health(
                        crate::observability::Component::Audio,
                        crate::observability::HealthStatus::Degraded,
                        "output-unavailable",
                    );
                    crate::observability::log_safe_error!(
                        error,
                        crate::observability::DiagnosticCode::SPOTIFY_AUDIO_OUTPUT_FAILED,
                        crate::observability::ErrorCategory::Resource,
                        &err,
                        "Spotify audio output could not be opened"
                    );
                    return Err(err);
                }
            }
        }
        Ok(self.active.as_mut().expect("audio output opened above"))
    }
}

impl<F: FnMut() -> SinkResult<Box<dyn Sink>>> Sink for ReleasingSink<F> {
    fn start(&mut self) -> SinkResult<()> {
        self.active().map(|_| ())
    }

    fn stop(&mut self) -> SinkResult<()> {
        // librespot exits the process when `stop` fails, and dropping the
        // backend closes the output either way, so a failed drain is only logged.
        if let Some(mut sink) = self.active.take() {
            if sink.stop().is_err() {
                tracing::warn!("Spotify audio output did not stop cleanly");
            }
        }
        Ok(())
    }

    fn write(&mut self, packet: AudioPacket, converter: &mut Converter) -> SinkResult<()> {
        self.active()?.write(packet, converter)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Default)]
    struct SinkLog {
        opened: usize,
        started: usize,
        stopped: usize,
        dropped: usize,
        written: usize,
    }

    struct LoggingSink(std::rc::Rc<std::cell::RefCell<SinkLog>>);

    impl Sink for LoggingSink {
        fn start(&mut self) -> SinkResult<()> {
            self.0.borrow_mut().started += 1;
            Ok(())
        }

        fn stop(&mut self) -> SinkResult<()> {
            self.0.borrow_mut().stopped += 1;
            Ok(())
        }

        fn write(&mut self, _: AudioPacket, _: &mut Converter) -> SinkResult<()> {
            self.0.borrow_mut().written += 1;
            Ok(())
        }
    }

    impl Drop for LoggingSink {
        fn drop(&mut self) {
            self.0.borrow_mut().dropped += 1;
        }
    }

    fn releasing_sink(
        log: &std::rc::Rc<std::cell::RefCell<SinkLog>>,
    ) -> ReleasingSink<impl FnMut() -> SinkResult<Box<dyn Sink>>> {
        let log = log.clone();
        ReleasingSink::new(move || -> SinkResult<Box<dyn Sink>> {
            log.borrow_mut().opened += 1;
            Ok(Box::new(LoggingSink(log.clone())))
        })
    }

    #[test]
    fn releasing_sink_opens_on_start_and_closes_on_stop() {
        let log = std::rc::Rc::default();
        let mut sink = releasing_sink(&log);
        assert_eq!(log.borrow().opened, 0);

        sink.start().unwrap();
        sink.write(
            AudioPacket::Samples(vec![0.0; 4]),
            &mut Converter::new(None),
        )
        .unwrap();
        sink.stop().unwrap();

        let log = log.borrow();
        assert_eq!(
            (
                log.opened,
                log.started,
                log.written,
                log.stopped,
                log.dropped
            ),
            (1, 1, 1, 1, 1)
        );
    }

    #[test]
    fn releasing_sink_reopens_for_each_resume_and_tolerates_repeated_stops() {
        let log = std::rc::Rc::default();
        let mut sink = releasing_sink(&log);

        sink.stop().unwrap();
        sink.start().unwrap();
        sink.start().unwrap();
        sink.stop().unwrap();
        sink.write(
            AudioPacket::Samples(vec![0.0; 4]),
            &mut Converter::new(None),
        )
        .unwrap();

        let log = log.borrow();
        assert_eq!((log.opened, log.started, log.dropped), (2, 2, 1));
    }

    #[test]
    fn releasing_sink_reports_a_missing_device_and_retries_on_the_next_start() {
        let log: std::rc::Rc<std::cell::RefCell<SinkLog>> = std::rc::Rc::default();
        let attempts = std::cell::Cell::new(0);
        let mut sink = ReleasingSink::new(|| -> SinkResult<Box<dyn Sink>> {
            attempts.set(attempts.get() + 1);
            if attempts.get() == 1 {
                Err(SinkError::ConnectionRefused("no device".to_owned()))
            } else {
                log.borrow_mut().opened += 1;
                Ok(Box::new(LoggingSink(log.clone())))
            }
        });

        assert!(matches!(sink.start(), Err(SinkError::ConnectionRefused(_))));
        assert!(sink.stop().is_ok());
        assert!(sink.start().is_ok());
        sink.stop().unwrap();

        assert_eq!(attempts.get(), 2);
        let log = log.borrow();
        assert_eq!((log.opened, log.started, log.dropped), (1, 1, 1));
    }

    #[test]
    fn releasing_sink_drops_an_output_that_fails_to_start() {
        struct FailingStart(std::rc::Rc<std::cell::RefCell<SinkLog>>);

        impl Sink for FailingStart {
            fn start(&mut self) -> SinkResult<()> {
                Err(SinkError::StateChange("refused".to_owned()))
            }

            fn stop(&mut self) -> SinkResult<()> {
                self.0.borrow_mut().stopped += 1;
                Ok(())
            }

            fn write(&mut self, _: AudioPacket, _: &mut Converter) -> SinkResult<()> {
                Ok(())
            }
        }

        impl Drop for FailingStart {
            fn drop(&mut self) {
                self.0.borrow_mut().dropped += 1;
            }
        }

        let log: std::rc::Rc<std::cell::RefCell<SinkLog>> = std::rc::Rc::default();
        let mut sink = ReleasingSink::new(|| -> SinkResult<Box<dyn Sink>> {
            log.borrow_mut().opened += 1;
            Ok(Box::new(FailingStart(log.clone())))
        });

        assert!(sink.start().is_err());
        assert!(sink
            .write(
                AudioPacket::Samples(vec![0.0; 4]),
                &mut Converter::new(None)
            )
            .is_err());
        assert!(sink.stop().is_ok());

        let log = log.borrow();
        assert_eq!((log.opened, log.dropped, log.stopped), (2, 2, 0));
    }

    #[test]
    fn player_events_preserve_state_projection_payload() {
        let playable_id = PlayableId::Track(
            TrackId::from_id("4iV5W9uYEdYUVa79Axb7Rh")
                .unwrap()
                .into_static(),
        );
        let event = PlayerEvent::Playing {
            playable_id: playable_id.clone(),
            position_ms: 1_250,
        };

        assert_eq!(
            event.as_state_event(),
            SpotifyPlaybackEvent::Playing {
                playable_id,
                position_ms: 1_250,
            }
        );
    }

    #[test]
    fn librespot_volume_event_is_projected_to_percent_without_reconciliation() {
        let event = PlayerEvent::from_librespot_player_event(player::PlayerEvent::VolumeChanged {
            volume: 32_768,
        })
        .unwrap()
        .unwrap();

        assert_eq!(event.kind(), "volume_changed");
        assert_eq!(event.args(), ["VolumeChanged", "50"]);
        assert_eq!(
            event.as_state_event(),
            SpotifyPlaybackEvent::VolumeChanged { volume: 50 }
        );
        assert!(!should_reconcile_spotify_event(
            config::ActiveProvider::Spotify,
            false
        ));
    }

    #[test]
    fn background_spotify_events_skip_rest_reconciliation() {
        assert!(!should_reconcile_spotify_event(
            config::ActiveProvider::YouTubeMusic,
            true
        ));
        assert!(should_reconcile_spotify_event(
            config::ActiveProvider::Spotify,
            true
        ));
        assert!(!should_reconcile_spotify_event(
            config::ActiveProvider::Spotify,
            false
        ));
    }
}
