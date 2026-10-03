use super::{
    config, Alignment, Block, Frame, Line, Paragraph, PlaybackMetadata, Rect, SharedState, Span,
    UIStateGuard, YouTubePlayback,
};

#[cfg(feature = "image")]
use crate::state::ImageRenderInfo;
use crate::{
    state::{PlayerState, UnifiedQueue, WorkspaceHit, WorkspacePlaybackOption},
    ui::utils::{self, format_genres, to_bidi_string},
};
use rspotify::model::Id;

#[derive(Debug, Clone, PartialEq, Eq)]
struct PlaybackIdentity {
    track: String,
    track_number: Option<String>,
    artists: String,
    album: String,
    genres: Option<String>,
    liked: bool,
    duration_label: String,
    cover_identity: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct PlaybackControlsPresentation {
    is_playing: bool,
    volume: u32,
    muted_at_volume: Option<u32>,
    repeat: rspotify::model::RepeatState,
    shuffle: bool,
    source_label: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct PlaybackPresentation {
    provider: config::ActiveProvider,
    capabilities: crate::state::PlaybackCapabilities,
    identity: PlaybackIdentity,
    controls: PlaybackControlsPresentation,
    progress: chrono::Duration,
    duration: Option<chrono::Duration>,
    metadata_ready: bool,
}

impl PlaybackPresentation {
    fn spotify(
        identity: PlaybackIdentity,
        mut controls: PlaybackControlsPresentation,
        progress: chrono::Duration,
        duration: chrono::Duration,
        metadata_ready: bool,
        capabilities: crate::state::PlaybackCapabilities,
    ) -> Self {
        // Provider/device labels enter the shared presentation boundary here,
        // so every playback layout receives the same bidi-safe projection.
        controls.source_label = to_bidi_string(&controls.source_label);
        Self {
            provider: config::ActiveProvider::Spotify,
            capabilities,
            identity,
            controls,
            progress,
            duration: usable_spotify_duration(duration),
            metadata_ready,
        }
    }

    fn youtube(
        playback: &YouTubePlayback,
        repeat: rspotify::model::RepeatState,
        shuffle: bool,
        capabilities: crate::state::PlaybackCapabilities,
    ) -> Self {
        let track = &playback.track;
        let duration = usable_youtube_duration(&track.duration);
        Self {
            provider: config::ActiveProvider::YouTubeMusic,
            capabilities,
            identity: PlaybackIdentity {
                track: format!(
                    "{}{}",
                    to_bidi_string(&track.name),
                    if track.explicit { " (E)" } else { "" }
                ),
                track_number: None,
                artists: to_bidi_string(&track.artists),
                album: to_bidi_string(track.album.as_deref().unwrap_or_default()),
                genres: None,
                liked: false,
                duration_label: track.duration.clone(),
                cover_identity: track.thumbnail_url.clone(),
            },
            controls: PlaybackControlsPresentation {
                is_playing: playback.is_playing,
                volume: u32::from(playback.volume),
                muted_at_volume: playback.mute_state.map(u32::from),
                repeat,
                shuffle,
                source_label: "local YouTube audio".to_string(),
            },
            progress: chrono::Duration::from_std(playback.progress).unwrap_or_default(),
            duration,
            metadata_ready: true,
        }
    }
}

/// Render the compact seven-row playback surface used by the design-v1
/// workspace, with the cover image (`image` feature) on canonical heights.
pub fn render_workspace_playback_window(
    frame: &mut Frame,
    state: &SharedState,
    ui: &mut UIStateGuard,
    rect: Rect,
) {
    ui.playback_window_rect = rect;
    ui.playback_toggle_rect = Rect::default();
    ui.playback_progress_bar_rect = Rect::default();
    if rect.is_empty() {
        return;
    }

    let player = state.player.read();
    let playback_provider = player.effective_playback_provider(ui.active_provider);
    let account = playback_account_label(&player, playback_provider);
    if rect.width < 4 {
        return;
    }
    let surface = Rect::new(
        rect.x.saturating_add(2),
        rect.y,
        rect.width.saturating_sub(4),
        rect.height,
    );
    frame.render_widget(
        Block::default().style(ui.theme.workspace_playback_surface()),
        surface,
    );
    utils::render_vertical_rule(
        frame,
        Rect::new(surface.x, surface.y, 1, surface.height),
        "│",
        ui.theme.workspace_focus_indicator(),
    );
    let inner = Rect::new(
        surface.x.saturating_add(2),
        surface.y,
        // Keep the same two-cell right inset used by the left identity inset;
        // the design-v1 option and scope rows end at x=176 on the canonical
        // 180-column surface.
        surface.width.saturating_sub(4),
        surface.height,
    );
    if inner.is_empty() {
        return;
    }

    let presentation = match playback_provider {
        config::ActiveProvider::YouTubeMusic => player.youtube_playback.as_ref().map(|playback| {
            let repeat = player
                .unified_queue
                .as_ref()
                .map_or(rspotify::model::RepeatState::Off, UnifiedQueue::repeat);
            let shuffle = player
                .unified_queue
                .as_ref()
                .is_some_and(UnifiedQueue::is_shuffled);
            PlaybackPresentation::youtube(
                playback,
                repeat,
                shuffle,
                player.playback_capabilities(crate::state::Provider::YouTubeMusic),
            )
        }),
        config::ActiveProvider::Spotify => {
            let playback = player.current_playback();
            playback.as_ref().and_then(|playback| {
                let item = playback.item.as_ref()?;
                let progress = player.playback_progress().unwrap_or_default();
                let data = state.data.read();
                spotify_playback_presentation(
                    item,
                    playback,
                    player.buffered_playback.as_ref(),
                    progress,
                    &data,
                    &config::get_config().app_config,
                    player.playback_capabilities(crate::state::Provider::Spotify),
                )
            })
        }
    };

    let Some(presentation) = presentation else {
        #[cfg(feature = "image")]
        {
            ui.last_cover_image_render_info = ImageRenderInfo::default();
        }
        let message = match (playback_provider, &player.youtube_playback_phase) {
            (config::ActiveProvider::Spotify, _) if player.playback_last_updated_time.is_none() => {
                "Loading playback…"
            }
            (
                config::ActiveProvider::YouTubeMusic,
                crate::state::YouTubePlaybackPhase::Failed(_),
            ) => "YouTube Music could not start this item. Try again or choose another item.",
            (
                config::ActiveProvider::YouTubeMusic,
                phase @ (crate::state::YouTubePlaybackPhase::Resolving
                | crate::state::YouTubePlaybackPhase::Buffering),
            ) => phase.label(),
            _ => "Nothing playing",
        };
        frame.render_widget(
            Paragraph::new(message)
                .style(ui.theme.workspace_secondary_text())
                .alignment(Alignment::Center),
            inner,
        );
        return;
    };

    if presentation.controls.is_playing {
        if let Some(duration) = presentation.duration {
            super::frame_schedule::request_progress_frame(std::cmp::min(
                presentation.progress,
                duration,
            ));
        }
    }
    #[cfg(feature = "image")]
    let inner = render_workspace_cover_image(
        frame,
        state,
        ui,
        presentation.identity.cover_identity.as_deref(),
        inner,
    );

    let configs = config::get_config();
    let content_x = inner.x;
    let content_width = inner.width;
    if content_width == 0 {
        return;
    }
    let status = if presentation.controls.is_playing {
        "Playing"
    } else {
        "Paused"
    };
    let identity = format!(
        "{}  {}",
        presentation.identity.track, presentation.identity.artists
    );
    let identity = utils::bounded_text(&identity, content_width as usize);
    if inner.height >= 7 {
        frame.render_widget(
            Paragraph::new(status).style(ui.theme.workspace_secondary_text()),
            Rect::new(content_x, inner.y.saturating_add(1), content_width, 1),
        );
        frame.render_widget(
            Paragraph::new(workspace_identity_line(
                &ui.theme,
                &presentation,
                &configs.app_config.liked_icon,
                content_width as usize,
            )),
            Rect::new(content_x, inner.y.saturating_add(2), content_width, 1),
        );
        render_workspace_playback_options(frame, ui, &presentation, inner, content_x);
        render_workspace_playback_transport(
            frame,
            ui,
            &presentation,
            account,
            playback_provider,
            inner,
            content_x,
            content_width,
            configs,
        );
    } else if inner.height >= 3 {
        let summary = format!("{status}  {identity}");
        frame.render_widget(
            Paragraph::new(utils::bounded_text(&summary, content_width as usize))
                .style(ui.theme.playback_track()),
            Rect::new(content_x, inner.y, content_width, 1),
        );
        render_workspace_playback_transport(
            frame,
            ui,
            &presentation,
            account,
            playback_provider,
            inner,
            content_x,
            content_width,
            configs,
        );
        let scope = format!(
            "{} / {} · {}",
            playback_provider.title(),
            account,
            if presentation.controls.source_label.is_empty() {
                "device unavailable"
            } else {
                presentation.controls.source_label.as_str()
            }
        );
        frame.render_widget(
            Paragraph::new(utils::bounded_text(&scope, content_width as usize))
                .style(ui.theme.workspace_secondary_text()),
            Rect::new(
                content_x,
                inner.bottom().saturating_sub(1),
                content_width,
                1,
            ),
        );
    } else {
        frame.render_widget(
            Paragraph::new(utils::bounded_text(
                &format!("{status}  {identity}"),
                content_width as usize,
            ))
            .style(ui.theme.playback_track()),
            Rect::new(content_x, inner.y, content_width, 1),
        );
    }
}

/// The canonical identity row: track, liked marker, and artists in their own
/// theme roles, falling back to one truncated run when they do not fit.
fn workspace_identity_line(
    theme: &config::Theme,
    presentation: &PlaybackPresentation,
    liked_icon: &str,
    width: usize,
) -> Line<'static> {
    let identity = &presentation.identity;
    let liked = if identity.liked {
        format!(" {liked_icon}")
    } else {
        String::new()
    };
    let full = format!("{}{liked}  {}", identity.track, identity.artists);
    if full.chars().count() > width {
        return Line::styled(utils::bounded_text(&full, width), theme.playback_track());
    }
    Line::from(vec![
        Span::styled(identity.track.clone(), theme.playback_track()),
        Span::styled(liked, theme.like()),
        Span::raw("  "),
        Span::styled(identity.artists.clone(), theme.playback_artists()),
    ])
}

fn playback_account_label(player: &PlayerState, provider: config::ActiveProvider) -> &str {
    if player.active_playback_account_provider == Some(provider) {
        player
            .active_playback_account_label
            .as_deref()
            .unwrap_or("account unavailable")
    } else {
        "account unavailable"
    }
}

fn render_workspace_playback_options(
    frame: &mut Frame,
    ui: &mut UIStateGuard,
    presentation: &PlaybackPresentation,
    inner: Rect,
    content_x: u16,
) {
    let content_width = inner.width;
    let shuffle = if presentation.capabilities.shuffle == crate::state::PlaybackSupport::Supported {
        if presentation.controls.shuffle {
            "On"
        } else {
            "Off"
        }
    } else {
        presentation.capabilities.shuffle.label()
    };
    let repeat = if presentation.capabilities.repeat == crate::state::PlaybackSupport::Supported {
        match presentation.controls.repeat {
            rspotify::model::RepeatState::Off => "Off",
            rspotify::model::RepeatState::Track => "Track",
            rspotify::model::RepeatState::Context => "Context",
        }
    } else {
        presentation.capabilities.repeat.label()
    };
    let shuffle_text = format!("Shuffle {shuffle}");
    let repeat_text = format!("Repeat {repeat}");
    let volume_bar = workspace_volume_bar(presentation.controls.volume);
    let volume_text =
        if presentation.capabilities.volume == crate::state::PlaybackSupport::Supported {
            format!(
                "Volume {volume_bar} {}%",
                presentation.controls.volume.min(100)
            )
        } else {
            format!("Volume {}", presentation.capabilities.volume.label())
        };
    let separator = " · ";
    let option_text_width = shuffle_text
        .chars()
        .count()
        .saturating_add(separator.chars().count())
        .saturating_add(repeat_text.chars().count())
        .saturating_add(separator.chars().count())
        .saturating_add(volume_text.chars().count());
    let option_width = option_text_width.min(content_width as usize) as u16;
    let option_x = inner.right().saturating_sub(option_width);
    let album_width = option_x
        .saturating_sub(content_x)
        .saturating_sub(2)
        .min(content_width);
    let album = utils::bounded_text(&presentation.identity.album, album_width as usize);
    let genres = presentation
        .identity
        .genres
        .as_deref()
        .map(|genres| format!(" · {genres}"))
        .filter(|genres| album.chars().count() + genres.chars().count() <= album_width as usize)
        .unwrap_or_default();
    frame.render_widget(
        Paragraph::new(Line::from(vec![
            Span::styled(album, ui.theme.playback_album()),
            Span::styled(genres, ui.theme.playback_genres()),
        ])),
        Rect::new(content_x, inner.y.saturating_add(3), album_width, 1),
    );
    if option_width > 0 {
        let option_y = inner.y.saturating_add(3);
        let option_right = option_x.saturating_add(option_width);
        let option_text = format!("{shuffle_text}{separator}{repeat_text}{separator}{volume_text}");
        frame.render_widget(
            Paragraph::new(utils::bounded_text(&option_text, option_width as usize))
                .style(ui.theme.workspace_secondary_text())
                .alignment(Alignment::Right),
            Rect::new(option_x, option_y, option_width, 1),
        );

        let visible_hit_rect = |start: usize, length: usize| {
            let start = start as u16;
            let x = option_x.saturating_add(start);
            let width = option_right.saturating_sub(x).min(length as u16);
            Rect::new(x, option_y, width, 1)
        };
        let shuffle_rect = visible_hit_rect(0, shuffle_text.chars().count());
        if shuffle_rect.width > 0
            && presentation.capabilities.shuffle == crate::state::PlaybackSupport::Supported
        {
            ui.workspace_hits.push((
                shuffle_rect,
                WorkspaceHit::PlaybackOption(WorkspacePlaybackOption::Shuffle),
            ));
        }
        let repeat_start = shuffle_text
            .chars()
            .count()
            .saturating_add(separator.chars().count());
        let repeat_rect = visible_hit_rect(repeat_start, repeat_text.chars().count());
        if repeat_rect.width > 0
            && presentation.capabilities.repeat == crate::state::PlaybackSupport::Supported
        {
            ui.workspace_hits.push((
                repeat_rect,
                WorkspaceHit::PlaybackOption(WorkspacePlaybackOption::Repeat),
            ));
        }
        let volume_start = repeat_start
            .saturating_add(repeat_text.chars().count())
            .saturating_add(separator.chars().count());
        let volume_bar_start = volume_start.saturating_add("Volume ".chars().count());
        let volume_rect = visible_hit_rect(volume_bar_start, volume_bar.chars().count());
        if volume_rect.width > 0
            && presentation.capabilities.volume == crate::state::PlaybackSupport::Supported
        {
            ui.workspace_hits.push((
                volume_rect,
                WorkspaceHit::PlaybackOption(WorkspacePlaybackOption::Volume),
            ));
        }
    }
}

const WORKSPACE_VOLUME_BAR_WIDTH: usize = 10;

fn workspace_volume_bar(volume: u32) -> String {
    let volume = volume.min(100) as usize;
    let filled = volume.saturating_mul(WORKSPACE_VOLUME_BAR_WIDTH) / 100;
    format!(
        "[{}{}]",
        "━".repeat(filled),
        "─".repeat(WORKSPACE_VOLUME_BAR_WIDTH.saturating_sub(filled))
    )
}

#[allow(clippy::too_many_arguments)]
fn render_workspace_playback_transport(
    frame: &mut Frame,
    ui: &mut UIStateGuard,
    presentation: &PlaybackPresentation,
    account: &str,
    playback_provider: config::ActiveProvider,
    inner: Rect,
    content_x: u16,
    content_width: u16,
    configs: &config::Configs,
) {
    if content_width <= 2 {
        return;
    }
    let controls_y = inner.bottom().saturating_sub(2);
    if controls_y < inner.y {
        return;
    }
    let play_pause = if presentation.controls.is_playing {
        &configs.app_config.pause_icon
    } else {
        &configs.app_config.play_icon
    };
    let toggle_width = play_pause.chars().count().max(2) as u16;
    let toggle_rect = Rect::new(content_x, controls_y, toggle_width.min(content_width), 1);
    frame.render_widget(
        Paragraph::new(utils::bounded_text(play_pause, toggle_rect.width as usize))
            .style(ui.theme.playback_status()),
        toggle_rect,
    );
    if presentation.capabilities.play_pause == crate::state::PlaybackSupport::Supported {
        ui.playback_toggle_rect = toggle_rect;
    }

    let elapsed = crate::utils::format_duration(&presentation.progress);
    let duration = presentation.duration.map_or_else(
        || "--:--".to_owned(),
        |duration| crate::utils::format_duration(&duration),
    );
    let elapsed_x = toggle_rect.right().saturating_add(3);
    let elapsed_width = elapsed.chars().count().max(4) as u16;
    let bar_x = elapsed_x.saturating_add(elapsed_width).saturating_add(2);
    let duration_width = duration.chars().count().max(4) as u16;
    let scope = format!(
        "{} / {} · {}",
        playback_provider.title(),
        account,
        if presentation.controls.source_label.is_empty() {
            "device unavailable"
        } else {
            presentation.controls.source_label.as_str()
        }
    );
    // On compact workspace transports the provider/source label is optional:
    // keeping a usable progress bar is more valuable than squeezing a long
    // scope string into the same row. The full label remains visible in the
    // canonical seven-row transport.
    let scope_wanted = if inner.height >= 7 {
        scope.chars().count() as u16
    } else {
        0
    };
    // Where the options row has no room for the volume slider, a symbol at
    // the end of the transport opens the volume popup instead.
    let volume_icon = (presentation.capabilities.volume
        == crate::state::PlaybackSupport::Supported
        && ui
            .workspace_hit_rect(WorkspaceHit::PlaybackOption(
                WorkspacePlaybackOption::Volume,
            ))
            .is_none())
    .then_some(configs.app_config.volume_icon.as_str());
    let icon_width = volume_icon.map_or(0, |icon| {
        u16::try_from(Line::from(icon).width()).unwrap_or(u16::MAX)
    });
    let icon_x = inner.right().saturating_sub(icon_width);
    // Play/pause, both times and a usable bar outrank the scope label: it
    // takes only the room left after them, and is dropped rather than cut
    // to a stub.
    let scope_room = icon_x
        .saturating_sub(if icon_width > 0 { 2 } else { 0 })
        .saturating_sub(bar_x)
        .saturating_sub(TRANSPORT_MIN_BAR_WIDTH + 2 + duration_width + 2);
    let scope_width = if scope_room >= TRANSPORT_MIN_SCOPE_WIDTH {
        scope_wanted.min(scope_room)
    } else {
        0
    };
    let scope_x = icon_x
        .saturating_sub(if icon_width > 0 { 2 } else { 0 })
        .saturating_sub(scope_width);
    let available_bar_width = scope_x
        .saturating_sub(bar_x)
        .saturating_sub(duration_width)
        .saturating_sub(2)
        // Keep the duration from running into the scope label.
        .saturating_sub(if scope_width > 0 { 2 } else { 0 });
    let bar_width = available_bar_width;

    frame.render_widget(
        Paragraph::new(elapsed).style(ui.theme.workspace_secondary_text()),
        Rect::new(elapsed_x, controls_y, elapsed_width, 1),
    );
    if let Some(duration_value) = presentation.duration {
        if bar_width > 0 {
            render_workspace_progress_bar(
                frame,
                ui,
                std::cmp::min(presentation.progress, duration_value),
                duration_value,
                Rect::new(bar_x, controls_y, bar_width, 1),
            );
        }
    } else {
        if bar_width > 0 {
            frame.render_widget(
                Paragraph::new("━".repeat(bar_width as usize))
                    .style(ui.theme.workspace_progress_remaining()),
                Rect::new(bar_x, controls_y, bar_width, 1),
            );
        }
        ui.playback_progress_bar_rect = Rect::default();
    }
    let duration_x = bar_x.saturating_add(bar_width).saturating_add(2);
    frame.render_widget(
        Paragraph::new(duration).style(ui.theme.workspace_secondary_text()),
        Rect::new(duration_x, controls_y, duration_width, 1),
    );
    if scope_width > 0 {
        frame.render_widget(
            Paragraph::new(utils::bounded_text(&scope, scope_width as usize))
                .style(ui.theme.workspace_secondary_text())
                .alignment(Alignment::Right),
            Rect::new(scope_x, controls_y, scope_width, 1),
        );
    }
    if let Some(icon) = volume_icon {
        if icon_x >= duration_x.saturating_add(duration_width) {
            let icon_rect = Rect::new(icon_x, controls_y, icon_width, 1);
            frame.render_widget(
                Paragraph::new(icon).style(ui.theme.workspace_secondary_text()),
                icon_rect,
            );
            ui.workspace_hits
                .push((icon_rect, WorkspaceHit::VolumeMenu));
        }
    }
}

/// The narrowest progress bar the transport keeps before showing its scope.
const TRANSPORT_MIN_BAR_WIDTH: u16 = 10;
/// The transport scope label is dropped rather than shown narrower than this.
const TRANSPORT_MIN_SCOPE_WIDTH: u16 = 12;

fn render_workspace_progress_bar(
    frame: &mut Frame,
    ui: &mut UIStateGuard,
    progress: chrono::Duration,
    duration: chrono::Duration,
    rect: Rect,
) {
    if rect.is_empty() {
        return;
    }
    let duration_millis = duration.num_milliseconds();
    let ratio = if duration_millis <= 0 {
        0.0
    } else {
        (progress.num_milliseconds().max(0) as f64 / duration_millis as f64).clamp(0.0, 1.0)
    };
    let filled = (f64::from(rect.width) * ratio).floor() as u16;
    let remaining = rect.width.saturating_sub(filled);
    let line = Line::from(vec![
        Span::styled(
            "━".repeat(filled as usize),
            ui.theme.playback_progress_bar(),
        ),
        Span::styled(
            "━".repeat(remaining as usize),
            ui.theme.workspace_progress_remaining(),
        ),
    ]);
    frame.render_widget(Paragraph::new(line), rect);
    ui.playback_progress_bar_rect = rect;
}

fn spotify_playback_presentation(
    item: &rspotify::model::PlayableItem,
    playback: &rspotify::model::CurrentPlaybackContext,
    buffered: Option<&PlaybackMetadata>,
    progress: chrono::Duration,
    data: &crate::state::AppData,
    app_config: &config::AppConfig,
    capabilities: crate::state::PlaybackCapabilities,
) -> Option<PlaybackPresentation> {
    let (identity, duration) = match item {
        rspotify::model::PlayableItem::Track(track) => {
            let track_name = if track.is_playable.unwrap_or(true) && track.id.is_some() {
                let display_name = if track.explicit {
                    format!("{} {}", track.name, app_config.explicit_icon)
                } else {
                    track.name.clone()
                };
                to_bidi_string(&display_name)
            } else {
                "Unknown Track".to_string()
            };
            let liked = track
                .id
                .as_ref()
                .is_some_and(|id| data.user_data.saved_tracks.contains_key(&id.uri()));
            let genres = track
                .artists
                .first()
                .and_then(|artist| data.caches.genres.get(&artist.name))
                .map(|genres| format_genres(genres, app_config.genre_num))
                .filter(|genres| !genres.is_empty());
            (
                PlaybackIdentity {
                    track: track_name,
                    track_number: Some(to_bidi_string(&track.track_number.to_string())),
                    artists: to_bidi_string(&crate::utils::map_join(
                        &track.artists,
                        |artist| &artist.name,
                        ", ",
                    )),
                    album: to_bidi_string(&track.album.name),
                    genres: genres.as_deref().map(to_bidi_string),
                    liked,
                    duration_label: crate::utils::format_duration(&track.duration),
                    cover_identity: crate::utils::get_track_album_image_url(track)
                        .map(String::from),
                },
                track.duration,
            )
        }
        rspotify::model::PlayableItem::Episode(episode) => {
            let name = to_bidi_string(&episode.name);
            (
                PlaybackIdentity {
                    track: if episode.explicit {
                        format!("{name} (E)")
                    } else {
                        name
                    },
                    track_number: None,
                    artists: to_bidi_string(&episode.show.name),
                    album: to_bidi_string(&episode.show.name),
                    genres: None,
                    liked: false,
                    duration_label: crate::utils::format_duration(&episode.duration),
                    cover_identity: crate::utils::get_episode_show_image_url(episode)
                        .map(String::from),
                },
                episode.duration,
            )
        }
        rspotify::model::PlayableItem::Unknown(_) => return None,
    };

    let controls = buffered.map_or_else(
        || PlaybackControlsPresentation {
            is_playing: playback.is_playing,
            volume: playback.device.volume_percent.unwrap_or_default(),
            muted_at_volume: None,
            repeat: playback.repeat_state,
            shuffle: playback.shuffle_state,
            source_label: playback.device.name.clone(),
        },
        |buffered| PlaybackControlsPresentation {
            is_playing: buffered.is_playing,
            volume: buffered.volume.unwrap_or_default(),
            muted_at_volume: buffered.mute_state,
            repeat: buffered.repeat_state,
            shuffle: buffered.shuffle_state,
            source_label: buffered.device_name.clone(),
        },
    );

    Some(PlaybackPresentation::spotify(
        identity,
        controls,
        progress,
        duration,
        buffered.is_some(),
        capabilities,
    ))
}

/// Narrowest identity column kept beside the cover image.
#[cfg(feature = "image")]
const WORKSPACE_COVER_MIN_TEXT_WIDTH: u16 = 40;

/// Draw the cover image at the left of the canonical transport and return the
/// area left for the identity and controls. Shorter or narrower transports
/// keep the whole area for text.
#[cfg(feature = "image")]
fn render_workspace_cover_image(
    frame: &mut Frame,
    state: &SharedState,
    ui: &mut UIStateGuard,
    cover_identity: Option<&str>,
    inner: Rect,
) -> Rect {
    let configs = config::get_config();
    // The canonical transport draws text on rows 1..=5; the cover spans them.
    let rows = u16::try_from(configs.app_config.cover_img_width)
        .unwrap_or(u16::MAX)
        .min(inner.height.saturating_sub(2));
    let length = cover_img_length(configs, &ui.picker, rows);
    let text_width = inner.width.saturating_sub(length).saturating_sub(2);
    let Some(url) = cover_identity.filter(|_| {
        inner.height >= 7 && rows > 0 && length > 0 && text_width >= WORKSPACE_COVER_MIN_TEXT_WIDTH
    }) else {
        ui.last_cover_image_render_info = ImageRenderInfo::default();
        return inner;
    };
    render_cover_image(
        frame,
        state,
        ui,
        url,
        Rect::new(inner.x, inner.y.saturating_add(1), length, rows),
    );
    Rect::new(
        inner.x.saturating_add(length).saturating_add(2),
        inner.y,
        text_width,
        inner.height,
    )
}

#[cfg(feature = "image")]
fn render_cover_image(
    frame: &mut Frame,
    state: &SharedState,
    ui: &mut UIStateGuard,
    url: &str,
    area: Rect,
) {
    let data = state.data.read();
    let image = data.caches.images.get(url);
    let image_is_cached = image.is_some();
    let (picker, info) = ui.cover_image_render_parts();
    update_cover_image_render_info(info, picker, url, area, image);

    // Do not render a stale protocol after a cache eviction. Once the image is
    // inserted, the pending state above is rebuilt before this branch runs.
    if image_is_cached {
        let render_area = info.render_area;
        if let Some(cover) = info.state.as_mut() {
            cover.render(frame, render_area);
        }
    }
}

#[cfg(feature = "image")]
fn update_cover_image_render_info(
    info: &mut ImageRenderInfo,
    picker: &ratatui_image::picker::Picker,
    url: &str,
    area: Rect,
    image: Option<&image::DynamicImage>,
) {
    let Some(image) = image else {
        if !info.targets(url, area) {
            crate::observability::log_safe_error!(
                debug,
                crate::observability::DiagnosticCode::IMAGE_CACHE_MISS,
                crate::observability::ErrorCategory::Unavailable,
                &anyhow::anyhow!("cover image is not cached"),
                "Cover image is not available yet"
            );
            *info = ImageRenderInfo::pending(url, area);
        }
        return;
    };

    if !info.needs_prepare(url, area) {
        return;
    }

    let state = match crate::ui::cover_image::CoverImage::new(picker, image, area) {
        Ok(cover) => Some(cover),
        Err(err) => {
            crate::observability::log_safe_error!(
                error,
                crate::observability::DiagnosticCode::IMAGE_ENCODE_FAILED,
                crate::observability::ErrorCategory::Decode,
                &err,
                "Failed to encode a cover image"
            );
            None
        }
    };
    *info = ImageRenderInfo {
        url: url.to_owned(),
        render_area: area,
        state,
        // A cache hit has now been observed. Keep encode failures terminal for
        // this `(url, area)` so a bad image cannot log and retry every frame.
        awaiting_cache: false,
    };
}

fn usable_spotify_duration(duration: chrono::Duration) -> Option<chrono::Duration> {
    (duration > chrono::Duration::zero()).then_some(duration)
}

#[cfg(test)]
mod tests {
    use std::{collections::VecDeque, sync::Arc};

    #[cfg(feature = "image")]
    use super::update_cover_image_render_info;
    use super::{
        playback_account_label, usable_spotify_duration, usable_youtube_duration,
        PlaybackControlsPresentation, PlaybackIdentity, PlaybackPresentation,
    };
    #[cfg(feature = "image")]
    use crate::state::ImageRenderInfo;
    #[cfg(feature = "image")]
    use crate::ui::cover_image::CoverImage;
    use crate::{
        config::ActiveProvider,
        observability::UiDiagnosticEntry,
        state::{PlayerState, SharedState, State},
        ui::utils::to_bidi_string,
    };
    use parking_lot::Mutex;
    #[cfg(feature = "image")]
    use ratatui::layout::Rect;
    use ratatui::{backend::TestBackend, Terminal};

    fn playback_test_state() -> SharedState {
        let configs = crate::ui::initialize_test_config();
        let diagnostics_ring = Arc::new(Mutex::new(VecDeque::<UiDiagnosticEntry>::new()));
        let (diagnostics, _runtime) = crate::observability::disabled(diagnostics_ring);
        Arc::new(State::new_with_configs(false, diagnostics, configs))
    }

    #[test]
    fn playback_account_uses_session_provider_without_browsing_attribution() {
        let mut player = PlayerState::default();
        player.active_playback_account_provider = Some(ActiveProvider::Spotify);
        player.active_playback_account_label = Some("Spotify Listener".to_owned());
        assert_eq!(
            playback_account_label(&player, ActiveProvider::Spotify),
            "Spotify Listener"
        );
        // A label owned by the other provider is never borrowed.
        assert_eq!(
            playback_account_label(&player, ActiveProvider::YouTubeMusic),
            "account unavailable"
        );

        player.active_playback_account_provider = None;
        assert_eq!(
            playback_account_label(&player, ActiveProvider::Spotify),
            "account unavailable"
        );
    }

    #[test]
    fn stale_playback_snapshot_is_not_relabelled_with_the_browsing_account() {
        let state = playback_test_state();
        // Account changes stop the coordinator before credentials change, but
        // a retained provider snapshot can still render. Neither an in-flight
        // selection nor a failed rollback may relabel that stale snapshot.
        {
            let mut player = state.player.write();
            player.active_playback_provider = None;
            player.active_playback_account_provider = None;
            player.active_playback_account_label = None;
            player.youtube_playback = Some(crate::state::YouTubePlayback {
                track: crate::state::YouTubeTrack {
                    id: "stale-youtube-snapshot".to_owned(),
                    name: "Stale track".to_owned(),
                    artists: "Artist".to_owned(),
                    album: None,
                    duration: "3:00".to_owned(),
                    explicit: false,
                    thumbnail_url: None,
                    is_video: false,
                },
                is_playing: false,
                progress: std::time::Duration::ZERO,
                volume: 50,
                mute_state: None,
                route: Default::default(),
            });
        }
        for account_label in ["Switch candidate", "Restored after failed switch"] {
            {
                let mut ui = state.ui.lock();
                ui.active_provider = ActiveProvider::YouTubeMusic;
                ui.youtube_account_label = Some(account_label.to_owned());
            }
            let mut terminal = Terminal::new(TestBackend::new(140, 7)).unwrap();
            {
                let mut ui = state.ui.lock();
                terminal
                    .draw(|frame| {
                        super::render_workspace_playback_window(
                            frame,
                            &state,
                            &mut ui,
                            frame.area(),
                        );
                    })
                    .unwrap();
            }
            let rendered: String = terminal
                .backend()
                .buffer()
                .content()
                .iter()
                .map(|cell| cell.symbol())
                .collect();
            assert!(rendered.contains("YouTube Music / account unavailable"));
            assert!(!rendered.contains(account_label));
        }
    }

    #[test]
    fn unavailable_or_zero_youtube_duration_is_not_seekable() {
        assert!(usable_youtube_duration("unknown").is_none());
        assert!(usable_youtube_duration("0:00").is_none());
        assert_eq!(
            usable_youtube_duration("3:00"),
            Some(chrono::Duration::minutes(3))
        );
    }

    #[test]
    fn unavailable_or_subsecond_spotify_duration_is_not_rendered_as_a_ratio() {
        assert!(usable_spotify_duration(chrono::Duration::zero()).is_none());
        assert_eq!(
            usable_spotify_duration(chrono::Duration::milliseconds(250)),
            Some(chrono::Duration::milliseconds(250))
        );
    }

    #[cfg(feature = "image")]
    #[test]
    fn cover_cache_miss_rebuilds_once_when_same_target_arrives() {
        let picker = ratatui_image::picker::Picker::halfblocks();
        let area = Rect::new(0, 0, 8, 4);
        let mut info = ImageRenderInfo::default();

        update_cover_image_render_info(&mut info, &picker, "cover", area, None);
        assert!(info.awaiting_cache);
        assert!(info.state.is_none());

        let image = image::DynamicImage::ImageRgb8(image::ImageBuffer::from_pixel(
            2,
            2,
            image::Rgb([32, 96, 160]),
        ));
        update_cover_image_render_info(&mut info, &picker, "cover", area, Some(&image));
        assert!(!info.awaiting_cache);
        assert!(matches!(info.state, Some(CoverImage::Widget(_))));
        let first_protocol = match info.state.as_ref() {
            Some(CoverImage::Widget(protocol)) => protocol.as_ref() as *const _,
            _ => panic!("halfblocks image protocol was not prepared"),
        };

        update_cover_image_render_info(&mut info, &picker, "cover", area, Some(&image));
        let second_protocol = match info.state.as_ref() {
            Some(CoverImage::Widget(protocol)) => protocol.as_ref() as *const _,
            _ => panic!("halfblocks image protocol was discarded"),
        };
        assert_eq!(first_protocol, second_protocol);
    }

    #[cfg(feature = "image")]
    #[test]
    fn workspace_cover_takes_the_left_of_the_canonical_transport_only() {
        let state = playback_test_state();
        let mut ui = state.ui.lock();
        let mut terminal = Terminal::new(TestBackend::new(140, 7)).unwrap();
        let mut draw = |ui: &mut crate::state::UIStateGuard, inner: Rect| {
            let mut remaining = Rect::default();
            terminal
                .draw(|frame| {
                    remaining = super::render_workspace_cover_image(
                        frame,
                        &state,
                        ui,
                        Some("cover"),
                        inner,
                    );
                })
                .unwrap();
            remaining
        };

        // The canonical transport moves the text right of a cover spanning rows 1..=5.
        let remaining = draw(&mut ui, Rect::new(4, 0, 132, 7));
        assert!(remaining.x > 4);
        assert_eq!(remaining.right(), 136);
        let cover = ui.last_cover_image_render_info.render_area;
        assert_eq!((cover.x, cover.y, cover.height), (4, 1, 5));
        assert!(cover.right() < remaining.x);

        // Short or narrow transports keep the full text area and drop the cover.
        for inner in [Rect::new(4, 0, 132, 3), Rect::new(4, 0, 30, 7)] {
            assert_eq!(draw(&mut ui, inner), inner);
            assert!(ui.last_cover_image_render_info.url.is_empty());
        }
    }

    #[test]
    fn spotify_presentation_keeps_identity_and_timing() {
        let presentation = PlaybackPresentation::spotify(
            PlaybackIdentity {
                track: "A Track (E)".to_string(),
                track_number: Some("7".to_string()),
                artists: "Artist One, Artist Two".to_string(),
                album: "An Album".to_string(),
                genres: Some("jazz, soul".to_string()),
                liked: true,
                duration_label: "3:00".to_string(),
                cover_identity: Some("spotify-cover".to_string()),
            },
            PlaybackControlsPresentation {
                is_playing: true,
                volume: 42,
                muted_at_volume: Some(42),
                repeat: rspotify::model::RepeatState::Track,
                shuffle: true,
                source_label: "Desktop".to_string(),
            },
            chrono::Duration::seconds(30),
            chrono::Duration::minutes(3),
            false,
            crate::state::PlaybackCapabilities::for_provider(
                crate::state::Provider::Spotify,
                true,
                true,
            ),
        );

        assert_eq!(
            presentation.provider,
            crate::config::ActiveProvider::Spotify
        );
        assert_eq!(presentation.progress, chrono::Duration::seconds(30));
        assert_eq!(presentation.duration, Some(chrono::Duration::minutes(3)));
        assert_eq!(
            presentation.identity.cover_identity.as_deref(),
            Some("spotify-cover")
        );
        assert!(!presentation.metadata_ready);
    }

    #[test]
    fn spotify_presentation_projects_external_device_labels_for_bidi_text() {
        let device = "iPhone שלום";
        let presentation = PlaybackPresentation::spotify(
            PlaybackIdentity {
                track: "Track".to_owned(),
                track_number: None,
                artists: "Artist".to_owned(),
                album: "Album".to_owned(),
                genres: None,
                liked: false,
                duration_label: "1:00".to_owned(),
                cover_identity: None,
            },
            PlaybackControlsPresentation {
                is_playing: false,
                volume: 50,
                muted_at_volume: None,
                repeat: rspotify::model::RepeatState::Off,
                shuffle: false,
                source_label: device.to_owned(),
            },
            chrono::Duration::zero(),
            chrono::Duration::minutes(1),
            true,
            crate::state::PlaybackCapabilities::for_provider(
                crate::state::Provider::Spotify,
                true,
                true,
            ),
        );

        assert_eq!(presentation.controls.source_label, to_bidi_string(device));
    }

    #[test]
    fn youtube_presentation_keeps_identity_and_timing() {
        let playback = crate::state::YouTubePlayback {
            track: crate::state::YouTubeTrack {
                id: "video-id".to_string(),
                name: "A Video".to_string(),
                artists: "An Artist".to_string(),
                album: Some("An Album".to_string()),
                duration: "3:00".to_string(),
                explicit: true,
                thumbnail_url: Some("youtube-cover".to_string()),
                is_video: true,
            },
            is_playing: false,
            progress: std::time::Duration::from_secs(45),
            volume: 73,
            mute_state: Some(73),
            route: crate::state::YouTubePlaybackRoute::default(),
        };
        let presentation = PlaybackPresentation::youtube(
            &playback,
            rspotify::model::RepeatState::Context,
            true,
            crate::state::PlaybackCapabilities::for_provider(
                crate::state::Provider::YouTubeMusic,
                true,
                true,
            ),
        );

        assert_eq!(
            presentation.provider,
            crate::config::ActiveProvider::YouTubeMusic
        );
        assert_eq!(presentation.identity.track, "A Video (E)");
        assert_eq!(presentation.progress, chrono::Duration::seconds(45));
        assert_eq!(presentation.duration, Some(chrono::Duration::minutes(3)));
        assert_eq!(
            presentation.identity.cover_identity.as_deref(),
            Some("youtube-cover")
        );
    }
}

fn parse_youtube_duration(duration: &str) -> Option<chrono::Duration> {
    let mut seconds = 0_i64;
    for part in duration.split(':') {
        seconds = seconds.checked_mul(60)?;
        seconds = seconds.checked_add(part.parse::<i64>().ok()?)?;
    }
    chrono::Duration::try_seconds(seconds)
}

fn usable_youtube_duration(duration: &str) -> Option<chrono::Duration> {
    parse_youtube_duration(duration).filter(|duration| *duration > chrono::Duration::zero())
}

/// Determine the cover image box's width in columns for a box `rows` tall.
#[cfg(feature = "image")]
fn cover_img_length(
    configs: &config::Configs,
    picker: &ratatui_image::picker::Picker,
    rows: u16,
) -> u16 {
    match configs.app_config.cover_img_length {
        // When `cover_img_length` is `0` (the default), derive it from the terminal's cell aspect ratio
        0 => {
            let font_size = picker.font_size();
            rows.saturating_mul(font_size.height) / font_size.width.max(1)
        }
        length => length.min(usize::from(u16::MAX)) as u16,
    }
}
