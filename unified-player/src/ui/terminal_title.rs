//! Terminal window title rendered from the `terminal_title` templates.

use std::io::{self, Write};

use crate::{
    config,
    state::{PageType, SharedState, UIStateGuard},
};

const APP_NAME: &str = "Unified Player";
const MAX_TITLE_CHARS: usize = 200;
// xterm window-title stack (XTWINOPS 22/23); terminals without it ignore both.
const PUSH_TITLE: &str = "\x1b[22;0t";
const POP_TITLE: &str = "\x1b[23;0t";

#[derive(Debug, Default, PartialEq, Eq)]
struct NowPlaying {
    is_playing: bool,
    track: String,
    artists: String,
    album: String,
}

/// Keeps the terminal title in sync and restores the previous one on exit.
#[derive(Default)]
pub(super) struct TerminalTitle {
    current: Option<String>,
}

impl TerminalTitle {
    /// Write the title for the current state when it changed. The previous
    /// title is saved the first time this sets one.
    pub(super) fn update(
        &mut self,
        writer: &mut impl Write,
        state: &SharedState,
        ui: &UIStateGuard,
    ) -> io::Result<()> {
        if ui.terminal_title.is_empty() {
            // Disabling the title from Settings hands it back right away.
            return self.restore(writer);
        }
        let app_config = &config::get_config().app_config;
        let provider = state
            .player
            .read()
            .effective_playback_provider(ui.active_provider);
        let now_playing = now_playing(state, provider);
        let title = render_title(
            &ui.terminal_title,
            &ui.terminal_title_idle,
            now_playing.as_ref(),
            provider.title(),
            page_title(ui.current_page().page_type()),
            app_config,
        );
        if self.current.as_deref() == Some(title.as_str()) {
            return Ok(());
        }
        if self.current.is_none() {
            writer.write_all(PUSH_TITLE.as_bytes())?;
        }
        crossterm::execute!(writer, crossterm::terminal::SetTitle(&title))?;
        self.current = Some(title);
        Ok(())
    }

    /// Restore the title saved by the first `update`, if any.
    pub(super) fn restore(&mut self, writer: &mut impl Write) -> io::Result<()> {
        if self.current.take().is_some() {
            writer.write_all(POP_TITLE.as_bytes())?;
            writer.flush()?;
        }
        Ok(())
    }
}

fn now_playing(state: &SharedState, provider: config::ActiveProvider) -> Option<NowPlaying> {
    let player = state.player.read();
    match provider {
        config::ActiveProvider::YouTubeMusic => {
            let playback = player.youtube_playback.as_ref()?;
            Some(NowPlaying {
                is_playing: playback.is_playing,
                track: playback.track.name.clone(),
                artists: playback.track.artists.clone(),
                album: playback.track.album.clone().unwrap_or_default(),
            })
        }
        config::ActiveProvider::Spotify => {
            let playback = player.playback.as_ref()?;
            let (track, artists, album) = match playback.item.as_ref()? {
                rspotify::model::PlayableItem::Track(track) => (
                    track.name.clone(),
                    crate::utils::map_join(&track.artists, |artist| &artist.name, ", "),
                    track.album.name.clone(),
                ),
                rspotify::model::PlayableItem::Episode(episode) => (
                    episode.name.clone(),
                    episode.show.name.clone(),
                    String::new(),
                ),
                rspotify::model::PlayableItem::Unknown(_) => return None,
            };
            Some(NowPlaying {
                is_playing: playback.is_playing,
                track,
                artists,
                album,
            })
        }
    }
}

const fn page_title(page: PageType) -> &'static str {
    match page {
        PageType::Home | PageType::HomeShelfList => "Home",
        PageType::Welcome => "Setup",
        PageType::Library => "Library",
        PageType::Context | PageType::YouTubeContext => "Collection",
        PageType::UnifiedPlaylist => "Unified Playlist",
        PageType::Search => "Search",
        PageType::Browse => "Browse",
        PageType::Lyrics => "Lyrics",
        PageType::Journal | PageType::JournalLists | PageType::JournalList => "Journal",
        PageType::SessionHistory => "Session History",
        PageType::Queue => "Queue",
        PageType::Settings => "Settings",
        PageType::CommandHelp => "Help",
        PageType::Logs => "Diagnostics",
    }
}

fn render_title(
    playing_template: &str,
    idle_template: &str,
    now_playing: Option<&NowPlaying>,
    provider: &str,
    page: &str,
    app_config: &config::AppConfig,
) -> String {
    let (template, status, track, artists, album) = match now_playing {
        Some(now) => (
            playing_template,
            if now.is_playing {
                app_config.play_icon.as_str()
            } else {
                app_config.pause_icon.as_str()
            },
            now.track.as_str(),
            now.artists.as_str(),
            now.album.as_str(),
        ),
        None => (idle_template, "", "", "", ""),
    };
    let rendered = template
        .replace("{status}", status)
        .replace("{track}", track)
        .replace("{artists}", artists)
        .replace("{album}", album)
        .replace("{provider}", provider)
        .replace("{page}", page);
    // Provider metadata must not be able to inject terminal control sequences.
    let title = rendered
        .split(|c: char| c.is_whitespace() || c.is_control())
        .filter(|word| !word.is_empty())
        .collect::<Vec<_>>()
        .join(" ");
    if title.is_empty() {
        APP_NAME.to_owned()
    } else {
        title.chars().take(MAX_TITLE_CHARS).collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn playing() -> NowPlaying {
        NowPlaying {
            is_playing: true,
            track: "Song".to_owned(),
            artists: "Artist A, Artist B".to_owned(),
            album: "Album".to_owned(),
        }
    }

    fn render(now: Option<&NowPlaying>) -> String {
        let app_config = config::AppConfig::default();
        render_title(
            &app_config.terminal_title,
            &app_config.terminal_title_idle,
            now,
            "Spotify",
            "Library",
            &app_config,
        )
    }

    #[test]
    fn default_templates_show_the_track_or_the_page() {
        assert_eq!(
            render(Some(&playing())),
            "▶ Song · Artist A, Artist B — Unified Player"
        );
        let paused = NowPlaying {
            is_playing: false,
            ..playing()
        };
        assert_eq!(
            render(Some(&paused)),
            "▌▌ Song · Artist A, Artist B — Unified Player"
        );
        assert_eq!(render(None), "Library — Unified Player");
    }

    #[test]
    fn all_placeholders_are_replaced() {
        let app_config = config::AppConfig::default();
        let title = render_title(
            "{status}|{track}|{artists}|{album}|{provider}|{page}",
            "",
            Some(&playing()),
            "YouTube Music",
            "Queue",
            &app_config,
        );
        assert_eq!(title, "▶|Song|Artist A, Artist B|Album|YouTube Music|Queue");
    }

    #[test]
    fn control_characters_cannot_reach_the_terminal() {
        let hostile = NowPlaying {
            track: "Song\x1b]0;owned\x07\nnext".to_owned(),
            ..playing()
        };
        let title = render(Some(&hostile));
        assert!(!title.chars().any(char::is_control), "{title:?}");
        assert!(title.starts_with("▶ Song ]0;owned next ·"));
    }

    #[test]
    fn an_empty_result_falls_back_to_the_app_name_and_long_titles_are_cut() {
        let app_config = config::AppConfig::default();
        assert_eq!(
            render_title("x", "  ", None, "Spotify", "Library", &app_config),
            APP_NAME
        );
        let long = NowPlaying {
            track: "a".repeat(500),
            ..playing()
        };
        assert_eq!(render(Some(&long)).chars().count(), MAX_TITLE_CHARS);
    }

    #[test]
    fn the_previous_title_is_pushed_once_and_popped_on_restore() {
        let mut title = TerminalTitle {
            current: Some("set".to_owned()),
        };
        let mut output = Vec::new();
        title.restore(&mut output).unwrap();
        title.restore(&mut output).unwrap();
        assert_eq!(output, POP_TITLE.as_bytes());

        let mut untouched = TerminalTitle::default();
        let mut output = Vec::new();
        untouched.restore(&mut output).unwrap();
        assert!(output.is_empty());
    }
}
