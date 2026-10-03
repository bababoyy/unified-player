use anyhow::{bail, Context, Result};
use crossterm::event::{self, Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use ratatui::{backend::TestBackend, widgets::Block, Terminal};

use crate::{
    config::{
        ActiveProvider, Configs, SetupFailure, SetupStatus, SpotifyAuthSnapshot,
        SpotifyPremiumStatus, YouTubeMusicAuthStatus, YouTubeMusicAuthType,
    },
    state::{
        PageState, PopupState, UIState, WelcomeAction, WelcomeLayout, WelcomeMove,
        WelcomeOperation, WelcomePageUIState, WelcomeStep, WorkspaceFocusState,
    },
};

use super::page;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum WelcomeDemoScenario {
    Fresh,
    SpotifyReady,
    YouTubeBrowserReady,
    YouTubeOAuthReady,
    AuthFailed,
    SpotifyChecking,
    YouTubeChecking,
    SpotifyRestartRequired,
    SpotifyCached,
    YouTubeWaiting,
    SpotifyRateLimited,
}

impl WelcomeDemoScenario {
    pub(crate) fn from_cli(value: &str) -> Result<Self> {
        match value {
            "fresh" => Ok(Self::Fresh),
            "spotify-ready" => Ok(Self::SpotifyReady),
            "youtube-browser-ready" => Ok(Self::YouTubeBrowserReady),
            "youtube-oauth-ready" => Ok(Self::YouTubeOAuthReady),
            "auth-failed" => Ok(Self::AuthFailed),
            "spotify-checking" => Ok(Self::SpotifyChecking),
            "youtube-checking" => Ok(Self::YouTubeChecking),
            "spotify-restart-required" => Ok(Self::SpotifyRestartRequired),
            "spotify-cached" => Ok(Self::SpotifyCached),
            "youtube-waiting" => Ok(Self::YouTubeWaiting),
            "spotify-rate-limited" => Ok(Self::SpotifyRateLimited),
            _ => bail!("unknown Welcome demo scenario"),
        }
    }

    fn apply(self, ui: &mut UIState) {
        ui.setup_state.status = SetupStatus::Pending;
        ui.setup_state.failure = None;
        match self {
            Self::Fresh => {}
            Self::SpotifyReady => {
                ui.welcome_spotify_web_token_cached = true;
                ui.welcome_spotify_library_tested = Some(true);
                ui.welcome_spotify_playback_tested = Some(true);
                ui.welcome_spotify_operation = WelcomeOperation::Succeeded;
                ui.setup_state.status = SetupStatus::Ready;
                ui.spotify_auth_status = SpotifyAuthSnapshot {
                    session_ready: true,
                    premium: SpotifyPremiumStatus::Premium,
                };
            }
            Self::YouTubeBrowserReady => {
                ui.setup_state.status = SetupStatus::Ready;
                ui.setup_state.startup_provider = ActiveProvider::YouTubeMusic;
                ui.youtube_auth_status = YouTubeMusicAuthStatus {
                    auth_type: YouTubeMusicAuthType::Browser,
                    credential_path: None,
                    ready: true,
                };
                ui.welcome_youtube_account_tested = Some(true);
                ui.welcome_youtube_playback_tested = Some(true);
                ui.welcome_youtube_operation = WelcomeOperation::Succeeded;
            }
            Self::YouTubeOAuthReady => {
                ui.setup_state.status = SetupStatus::Ready;
                ui.setup_state.startup_provider = ActiveProvider::YouTubeMusic;
                ui.youtube_auth_status = YouTubeMusicAuthStatus {
                    auth_type: YouTubeMusicAuthType::OAuth,
                    credential_path: None,
                    ready: true,
                };
                ui.welcome_youtube_account_tested = Some(true);
                ui.welcome_youtube_playback_tested = Some(true);
                ui.welcome_youtube_operation = WelcomeOperation::Succeeded;
            }
            Self::AuthFailed => {
                ui.setup_state.status = SetupStatus::Failed;
                ui.setup_state.failure = Some(SetupFailure::AuthenticationFailed);
                ui.welcome_spotify_operation = WelcomeOperation::Failed;
                ui.welcome_spotify_library_tested = Some(false);
                ui.welcome_spotify_playback_tested = Some(false);
                ui.welcome_spotify_notice =
                    Some("Authentication failed; retry the provider sign-in.".to_owned());
                ui.welcome_youtube_operation = WelcomeOperation::Failed;
                ui.welcome_youtube_account_tested = Some(false);
                ui.welcome_youtube_notice =
                    Some("Authentication failed; retry the provider sign-in.".to_owned());
            }
            Self::SpotifyChecking => {
                ui.welcome_spotify_web_token_cached = true;
                ui.welcome_spotify_auth_in_flight = true;
                ui.welcome_spotify_operation = WelcomeOperation::Checking;
                ui.welcome_spotify_notice =
                    Some("Checking saved credentials and integrated playback...".to_owned());
            }
            Self::YouTubeChecking => {
                ui.youtube_auth_status = YouTubeMusicAuthStatus {
                    auth_type: YouTubeMusicAuthType::Browser,
                    credential_path: None,
                    ready: true,
                };
                ui.welcome_youtube_operation = WelcomeOperation::Checking;
                ui.welcome_youtube_notice =
                    Some("Testing account and library access...".to_owned());
            }
            Self::SpotifyRestartRequired => {
                "0123456789abcdef0123456789abcdef".clone_into(&mut ui.welcome_spotify_client_id); // gitleaks:allow - placeholder
                ui.welcome_spotify_client_pending = true;
                ui.setup_state.spotify_reauthentication_required = true;
                ui.welcome_spotify_notice =
                    Some("Saved client choice; use Apply & sign in.".to_owned());
            }
            Self::SpotifyCached => {
                ui.welcome_spotify_web_token_cached = true;
                ui.welcome_spotify_notice =
                    Some("Credentials saved; use Check existing session.".to_owned());
            }
            Self::YouTubeWaiting => {
                ui.youtube_auth_status = YouTubeMusicAuthStatus {
                    auth_type: YouTubeMusicAuthType::Browser,
                    credential_path: None,
                    ready: true,
                };
                ui.welcome_youtube_operation = WelcomeOperation::Waiting;
                ui.welcome_youtube_notice =
                    Some("Waiting for the current YouTube Music action...".to_owned());
            }
            Self::SpotifyRateLimited => {
                ui.welcome_spotify_web_token_cached = true;
                ui.welcome_spotify_operation = WelcomeOperation::RateLimited;
                ui.welcome_spotify_notice =
                    Some("Spotify is temporarily limited; retry later.".to_owned());
            }
        }
    }
}

pub(crate) fn welcome_demo_step_from_cli(value: &str) -> Result<WelcomeStep> {
    match value {
        "preferences" => Ok(WelcomeStep::Preferences),
        "spotify" => Ok(WelcomeStep::Spotify),
        "youtube" => Ok(WelcomeStep::YouTube),
        "listenbrainz" => Ok(WelcomeStep::ListenBrainz),
        "review" => Ok(WelcomeStep::Review),
        _ => bail!("unknown Welcome demo step"),
    }
}

pub(crate) fn welcome_demo_layout_from_cli(value: &str) -> Result<WelcomeLayout> {
    match value {
        "classic" => Ok(WelcomeLayout::Classic),
        "centered" => Ok(WelcomeLayout::Centered),
        "sidebar" => Ok(WelcomeLayout::Sidebar),
        _ => bail!("unknown Welcome demo layout"),
    }
}

fn welcome_demo_ui(configs: &Configs, scenario: WelcomeDemoScenario, step: WelcomeStep) -> UIState {
    let mut ui = UIState::default();
    if let Some(theme) = configs.theme_config.find_theme(&configs.app_config.theme) {
        ui.theme = theme;
    }
    ui.apply_presentation_config(&configs.app_config);
    ui.project_frame_chrome();
    scenario.apply(&mut ui);
    ui.history.clear();
    let mut welcome = WelcomePageUIState::new();
    welcome.show_step(step);
    ui.history.push(PageState::Welcome {
        state: welcome,
        from_settings: false,
    });
    ui
}

/// Render the production Welcome page against synthetic, credential-free state.
#[cfg(test)]
fn render_welcome_demo(
    configs: &Configs,
    scenario: WelcomeDemoScenario,
    step: WelcomeStep,
    width: u16,
    height: u16,
) -> Result<String> {
    render_welcome_demo_layout(
        configs,
        scenario,
        step,
        width,
        height,
        WelcomeLayout::Classic,
    )
}

pub(crate) fn render_welcome_demo_layout(
    configs: &Configs,
    scenario: WelcomeDemoScenario,
    step: WelcomeStep,
    width: u16,
    height: u16,
    layout: WelcomeLayout,
) -> Result<String> {
    let ui = crate::state::TrackedMutex::new(welcome_demo_ui(configs, scenario, step));
    let mut ui = ui.lock();
    if let PageState::Welcome { state, .. } = ui.current_page_mut() {
        state.layout = layout;
    }
    let mut terminal = Terminal::new(TestBackend::new(width, height))?;
    terminal.draw(|frame| draw_welcome_demo(frame, &mut ui))?;

    let buffer = terminal.backend().buffer();
    let mut output = String::new();
    for y in 0..height {
        let row = (0..width)
            .map(|x| buffer[(x, y)].symbol())
            .collect::<String>();
        output.push_str(row.trim_end());
        if y + 1 < height {
            output.push('\n');
        }
    }
    Ok(output.trim_end().to_owned())
}

/// Run the production Welcome renderer with synthetic state and no application workers.
pub(crate) fn run_welcome_demo_interactive(
    configs: &Configs,
    scenario: WelcomeDemoScenario,
    step: WelcomeStep,
    layout: WelcomeLayout,
) -> Result<()> {
    let mut terminal = super::init_interactive_demo_terminal()?;

    let ui = crate::state::TrackedMutex::new(welcome_demo_ui(configs, scenario, step));
    if let PageState::Welcome { state, .. } = ui.lock().current_page_mut() {
        state.layout = layout;
    }
    let run_result = run_welcome_demo_loop(&mut terminal, &ui);
    let cleanup_result = super::clean_up(terminal);
    match (run_result, cleanup_result) {
        (Err(error), _) => Err(error),
        (Ok(()), Err(error)) => Err(error).context("restore terminal after Welcome demo"),
        (Ok(()), Ok(())) => Ok(()),
    }
}

fn run_welcome_demo_loop(
    terminal: &mut super::Terminal,
    ui: &crate::state::TrackedMutex<UIState>,
) -> Result<()> {
    loop {
        terminal.draw(|frame| {
            let mut ui = ui.lock();
            draw_welcome_demo(frame, &mut ui);
        })?;

        match event::read()? {
            Event::Key(key) if key.kind == KeyEventKind::Press => {
                let mut ui = ui.lock();
                if handle_welcome_demo_key(&mut ui, key) {
                    return Ok(());
                }
            }
            Event::Mouse(mouse) => {
                let mut ui = ui.lock();
                if handle_welcome_demo_mouse(
                    &mut ui,
                    mouse,
                    crate::config::get_config()
                        .app_config
                        .enable_mouse_navigation,
                ) {
                    return Ok(());
                }
            }
            Event::Resize(_, _) => ui.lock().clear_playback_hit_regions(),
            _ => {}
        }
    }
}

fn draw_welcome_demo(frame: &mut super::Frame, ui: &mut crate::state::UIStateGuard) {
    // Like the application frame, every draw replaces the previous hit geometry.
    ui.clear_playback_hit_regions();
    ui.refresh_focused_marquee();
    let rect = frame.area();
    frame.render_widget(Block::default().style(ui.theme.workspace_base()), rect);
    let active = ui.popup.is_none();
    let rect = if active {
        rect
    } else {
        super::welcome::render_client_editor(frame, ui, rect)
    };
    page::render_welcome_page(active, frame, ui, rect);
}

fn handle_welcome_demo_key(ui: &mut UIState, key: KeyEvent) -> bool {
    if key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('c') {
        return true;
    }
    if let Some(PopupState::ListenBrainzToken { input }) = &mut ui.popup {
        match key.code {
            KeyCode::Enter | KeyCode::Esc => {
                ui.popup = None;
                ui.welcome_listenbrainz_notice =
                    Some("Demo: no token checked or saved.".to_owned());
            }
            _ => {
                input.input(&crate::key::Key::None(key.code));
            }
        }
        return false;
    }
    if let Some(PopupState::ConfigEdit {
        key: editor_key,
        input,
    }) = &mut ui.popup
    {
        if key.code == KeyCode::Enter && editor_key.starts_with("welcome.youtube.") {
            ui.welcome_youtube_notice =
                Some("Demo: no credentials imported or browser path saved.".to_owned());
            ui.popup = None;
            return false;
        }
        match key.code {
            KeyCode::Esc => ui.popup = None,
            KeyCode::Enter => {
                let value = input.get_text();
                let value = value.trim();
                if value.len() == 32 && value.bytes().all(|byte| byte.is_ascii_hexdigit()) {
                    value.clone_into(&mut ui.welcome_spotify_client_id);
                    ui.welcome_spotify_notice =
                        Some("Demo: custom client selected; nothing is saved.".to_owned());
                    ui.popup = None;
                } else {
                    ui.welcome_spotify_notice =
                        Some("Enter a 32-character hexadecimal client ID.".to_owned());
                }
            }
            _ => {
                input.input(&crate::key::Key::None(key.code));
            }
        }
        return false;
    }
    match key.code {
        KeyCode::Char('q') => true,
        KeyCode::Up | KeyCode::Char('k') => {
            move_welcome_demo_selection(ui, false);
            false
        }
        KeyCode::Down | KeyCode::Char('j') => {
            move_welcome_demo_selection(ui, true);
            false
        }
        KeyCode::Tab | KeyCode::BackTab => {
            ui.focus_workspace(key.code == KeyCode::Tab);
            false
        }
        KeyCode::Esc => {
            let step = welcome_demo_position(ui).map(|(step, _)| step);
            if step == Some(WelcomeStep::Preferences) {
                true
            } else {
                show_welcome_demo_step(ui, step.and_then(WelcomeStep::previous));
                false
            }
        }
        KeyCode::Enter if ui.welcome_focus() == WorkspaceFocusState::Navigation => {
            ui.set_welcome_focus(WorkspaceFocusState::Context);
            false
        }
        KeyCode::Enter => activate_welcome_demo_selection(ui),
        _ => false,
    }
}

fn handle_welcome_demo_mouse(
    ui: &mut UIState,
    event: crossterm::event::MouseEvent,
    enabled: bool,
) -> bool {
    use crate::state::WorkspaceHit;
    use crossterm::event::{MouseButton, MouseEventKind};
    if !enabled {
        return false;
    }
    if crate::event::welcome_editor_is_open(ui) {
        if event.kind != MouseEventKind::Down(MouseButton::Left) {
            return false;
        }
        if !ui.popup_contains_point(event.column, event.row) {
            ui.popup = None;
            return false;
        }
        match ui.workspace_hit_at(event.column, event.row) {
            Some(WorkspaceHit::WelcomeEditorInput) => {
                let x = ui
                    .workspace_hit_rect(WorkspaceHit::WelcomeEditorInput)
                    .map_or(event.column, |rect| rect.x);
                let column = event.column.saturating_sub(x);
                match &mut ui.popup {
                    Some(PopupState::ListenBrainzToken { input }) => {
                        input.set_cursor_column(column);
                    }
                    Some(PopupState::ConfigEdit { input, .. }) => input.set_cursor_column(column),
                    _ => {}
                }
            }
            Some(WorkspaceHit::WelcomeEditorConfirm) => {
                return handle_welcome_demo_key(
                    ui,
                    KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE),
                );
            }
            Some(WorkspaceHit::WelcomeEditorCancel) => ui.popup = None,
            _ => {}
        }
        return false;
    }
    // Any other popup owns the pointer; the page behind it does not react.
    if ui.popup.is_some() {
        return false;
    }
    match event.kind {
        MouseEventKind::Down(MouseButton::Left) => {
            ui.set_workspace_pointer(event.column, event.row);
            let Some(hit) = ui.workspace_hit_at(event.column, event.row) else {
                return false;
            };
            let activate = ui.workspace_click_activates(hit);
            ui.click_welcome(hit, activate) && activate_welcome_demo_selection(ui)
        }
        MouseEventKind::ScrollUp | MouseEventKind::ScrollDown => {
            move_welcome_demo_selection(ui, event.kind == MouseEventKind::ScrollDown);
            false
        }
        MouseEventKind::Moved => {
            ui.set_workspace_pointer(event.column, event.row);
            false
        }
        _ => false,
    }
}

fn welcome_demo_position(ui: &UIState) -> Option<(WelcomeStep, usize)> {
    match ui.current_page() {
        PageState::Welcome { state, .. } => Some((state.step, state.list.selected().unwrap_or(0))),
        _ => None,
    }
}

fn move_welcome_demo_selection(ui: &mut UIState, forward: bool) {
    ui.move_welcome(WelcomeMove::By(if forward { 1 } else { -1 }));
}

fn show_welcome_demo_step(ui: &mut UIState, step: Option<WelcomeStep>) {
    if let Some(step) = step {
        ui.show_welcome_step(step);
    }
}

fn activate_welcome_demo_selection(ui: &mut UIState) -> bool {
    let Some((step, selected)) = welcome_demo_position(ui) else {
        return false;
    };
    let Some(action) = step.action_at(selected) else {
        return false;
    };
    match action {
        WelcomeAction::PreferencesNext
        | WelcomeAction::SpotifyNext
        | WelcomeAction::YouTubeNext
        | WelcomeAction::ListenBrainzNext => show_welcome_demo_step(ui, step.next()),
        WelcomeAction::SpotifyBack
        | WelcomeAction::YouTubeBack
        | WelcomeAction::ListenBrainzBack
        | WelcomeAction::ReviewBack => show_welcome_demo_step(ui, step.previous()),
        WelcomeAction::PreferencesStartupProvider => {
            ui.setup_state.startup_provider = ui.setup_state.startup_provider.toggled();
            ui.mark_setup_pending();
        }
        WelcomeAction::PreferencesStartPaused => {
            ui.setup_state.pause_on_startup = !ui.setup_state.pause_on_startup;
            ui.mark_setup_pending();
        }
        WelcomeAction::SpotifyBundledClient => {
            crate::auth::NCSPOT_CLIENT_ID.clone_into(&mut ui.welcome_spotify_client_id);
            ui.welcome_spotify_notice =
                Some("Demo: bundled client selected; nothing is saved.".to_owned());
        }
        WelcomeAction::SpotifyCustomClient => {
            ui.welcome_spotify_notice = None;
            ui.popup = Some(PopupState::ConfigEdit {
                key: "client_id".to_owned(),
                input: super::single_line_input::LineInput::default(),
            });
        }
        WelcomeAction::SpotifySignIn | WelcomeAction::SpotifyCheckSession => {
            ui.welcome_spotify_web_token_cached = true;
            ui.welcome_spotify_library_tested = Some(true);
            ui.welcome_spotify_playback_tested = Some(true);
            ui.spotify_auth_status = SpotifyAuthSnapshot {
                session_ready: true,
                premium: SpotifyPremiumStatus::Premium,
            };
            ui.welcome_spotify_auth_in_flight = false;
            ui.welcome_spotify_operation = WelcomeOperation::Succeeded;
            ui.welcome_spotify_notice =
                Some("Demo: Spotify account and playback are ready.".to_owned());
            ui.mark_setup_pending();
        }
        WelcomeAction::YouTubeSignIn => {
            ui.youtube_auth_status = YouTubeMusicAuthStatus {
                auth_type: YouTubeMusicAuthType::Browser,
                credential_path: None,
                ready: true,
            };
            ui.welcome_youtube_account_tested = None;
            ui.welcome_youtube_playback_tested = None;
            ui.welcome_youtube_operation = WelcomeOperation::Succeeded;
            ui.welcome_youtube_notice = Some("Demo: dedicated browser sign-in saved.".to_owned());
            ui.mark_setup_pending();
        }
        WelcomeAction::YouTubeTestAccount => {
            if !ui.youtube_auth_status.ready {
                ui.welcome_youtube_notice =
                    Some("Sign in with a dedicated browser before testing the account.".to_owned());
                return false;
            }
            ui.welcome_youtube_account_tested = Some(true);
            ui.welcome_youtube_playback_tested = Some(true);
            ui.welcome_youtube_operation = WelcomeOperation::Succeeded;
            ui.welcome_youtube_notice =
                Some("Demo: account, library, and playback are ready.".to_owned());
            ui.mark_setup_pending();
        }
        WelcomeAction::YouTubeChooseBrowser | WelcomeAction::YouTubeImportCookies => {
            ui.popup = Some(PopupState::ConfigEdit {
                key: if action == WelcomeAction::YouTubeChooseBrowser {
                    "welcome.youtube.browser"
                } else {
                    "welcome.youtube.cookies"
                }
                .to_owned(),
                input: super::single_line_input::LineInput::default(),
            });
        }
        WelcomeAction::YouTubeDetectBrowser => {
            ui.welcome_youtube_notice =
                Some("Demo: detection skipped; import is available.".to_owned());
        }
        WelcomeAction::ListenBrainzEnterToken => {
            ui.popup = Some(PopupState::ListenBrainzToken {
                input: crate::ui::single_line_input::SecretInput::default(),
            });
        }
        WelcomeAction::ListenBrainzCheckToken => {
            ui.welcome_listenbrainz_notice = Some("Demo: no token checked or saved.".to_owned());
        }
        WelcomeAction::ListenBrainzFetchPlaylists => {
            ui.welcome_listenbrainz_notice = Some(
                "Demo: validate a token in the live application to fetch playlists.".to_owned(),
            );
        }
        WelcomeAction::ReviewFixSpotify => show_welcome_demo_step(ui, Some(WelcomeStep::Spotify)),
        WelcomeAction::ReviewFixYouTube => show_welcome_demo_step(ui, Some(WelcomeStep::YouTube)),
        WelcomeAction::ReviewContinue | WelcomeAction::ReviewSkip => return true,
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::state::WorkspaceHit as Hit;
    use crossterm::event::{MouseButton, MouseEvent, MouseEventKind};

    fn mouse(kind: MouseEventKind, column: u16, row: u16) -> MouseEvent {
        MouseEvent {
            kind,
            column,
            row,
            modifiers: KeyModifiers::NONE,
        }
    }

    fn draw(ui: &mut crate::state::UIStateGuard, width: u16, height: u16) -> String {
        draw_focus(ui, width, height).0
    }

    /// Draw one frame and return its text with the focused action rows.
    fn draw_focus(
        ui: &mut crate::state::UIStateGuard,
        width: u16,
        height: u16,
    ) -> (String, Vec<usize>) {
        let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
        terminal.draw(|frame| draw_welcome_demo(frame, ui)).unwrap();
        let buffer = terminal.backend().buffer();
        let text = buffer.content.iter().map(|cell| cell.symbol()).collect();
        (text, super::super::welcome::focused_rows(ui, buffer))
    }

    fn render_focus(
        scenario: WelcomeDemoScenario,
        step: WelcomeStep,
        width: u16,
        height: u16,
        layout: WelcomeLayout,
    ) -> (String, Vec<usize>) {
        let configs = crate::ui::initialize_test_config();
        let ui = crate::state::TrackedMutex::new(welcome_demo_ui(configs, scenario, step));
        let mut ui = ui.lock();
        if let PageState::Welcome { state, .. } = ui.current_page_mut() {
            state.layout = layout;
        }
        draw_focus(&mut ui, width, height)
    }

    #[test]
    fn listenbrainz_editor_masks_token_and_supports_mouse_cancel_and_navigation() {
        crate::ui::initialize_test_config();
        let configs = crate::config::get_config();
        for (width, height) in [(40, 18), (80, 24)] {
            let ui = crate::state::TrackedMutex::new(welcome_demo_ui(
                configs,
                WelcomeDemoScenario::Fresh,
                WelcomeStep::ListenBrainz,
            ));
            let mut ui = ui.lock();
            assert!(!activate_welcome_demo_selection(&mut ui));
            for c in "sensitive-token".chars() {
                handle_welcome_demo_key(
                    &mut ui,
                    KeyEvent::new(KeyCode::Char(c), KeyModifiers::NONE),
                );
            }
            assert!(!format!("{:?}", *ui).contains("sensitive-token"));
            let rendered = draw(&mut ui, width, height);
            assert!(!rendered.contains("sensitive-token"));
            assert!(rendered.contains("********"));
            let cancel = target(&ui, Hit::WelcomeEditorCancel);
            handle_welcome_demo_mouse(
                &mut ui,
                mouse(MouseEventKind::Down(MouseButton::Left), cancel.x, cancel.y),
                true,
            );
            assert!(ui.popup.is_none());
            draw(&mut ui, width, height);
            ui.current_page_mut()
                .select(crate::state::WelcomeStep::ListenBrainz.row_count() - 1);
            activate_welcome_demo_selection(&mut ui);
            assert_eq!(welcome_demo_position(&ui), Some((WelcomeStep::Review, 0)));
        }
    }

    fn target(ui: &UIState, action: Hit) -> ratatui::layout::Rect {
        ui.workspace_hit_rect(action)
            .unwrap_or_else(|| panic!("{action:?} was not drawn"))
    }

    /// Click `action` the way the workspace policy activates it: twice.
    fn double_click(ui: &mut UIState, action: Hit) -> bool {
        let rect = target(ui, action);
        let click = mouse(MouseEventKind::Down(MouseButton::Left), rect.x, rect.y);
        assert!(!handle_welcome_demo_mouse(ui, click, true));
        handle_welcome_demo_mouse(ui, click, true)
    }

    #[test]
    fn welcome_tab_moves_focus_between_panes_and_highlights_only_the_focused_one() {
        let configs = crate::ui::initialize_test_config();
        assert_eq!(WelcomePageUIState::new().layout, WelcomeLayout::Sidebar);
        for layout in [WelcomeLayout::Centered, WelcomeLayout::Sidebar] {
            let ui = crate::state::TrackedMutex::new(welcome_demo_ui(
                configs,
                WelcomeDemoScenario::Fresh,
                WelcomeStep::Spotify,
            ));
            let mut ui = ui.lock();
            if let PageState::Welcome { state, .. } = ui.current_page_mut() {
                state.layout = layout;
            }
            let press = |ui: &mut UIState, code| {
                assert!(!handle_welcome_demo_key(
                    ui,
                    KeyEvent::new(code, KeyModifiers::NONE)
                ));
            };
            assert_eq!(draw_focus(&mut ui, 110, 30).1, vec![0]);

            // Left/Right no longer move between buttons.
            press(&mut ui, KeyCode::Right);
            assert_eq!(welcome_demo_position(&ui), Some((WelcomeStep::Spotify, 0)));

            press(&mut ui, KeyCode::Tab);
            assert_eq!(ui.welcome_focus(), WorkspaceFocusState::Actions);
            assert_eq!(draw_focus(&mut ui, 110, 30).1, vec![5]);
            press(&mut ui, KeyCode::Up);
            assert_eq!(draw_focus(&mut ui, 110, 30).1, vec![4]);

            // The rail only joins the cycle where it is drawn.
            press(&mut ui, KeyCode::Tab);
            let expected = if layout == WelcomeLayout::Sidebar {
                WorkspaceFocusState::Navigation
            } else {
                WorkspaceFocusState::Context
            };
            assert_eq!(ui.welcome_focus(), expected);
            if expected == WorkspaceFocusState::Navigation {
                // No row is drawn as active while the rail owns focus.
                assert!(draw_focus(&mut ui, 110, 30).1.is_empty());
                press(&mut ui, KeyCode::Down);
                assert_eq!(welcome_demo_position(&ui), Some((WelcomeStep::YouTube, 0)));
                press(&mut ui, KeyCode::Enter);
                assert_eq!(ui.welcome_focus(), WorkspaceFocusState::Context);
            }
            assert_eq!(draw_focus(&mut ui, 110, 30).1.len(), 1);
        }
    }

    #[test]
    fn welcome_layout_mouse_activation_matches_keyboard_and_honors_disable() {
        let configs = crate::ui::initialize_test_config();
        for layout in [
            WelcomeLayout::Classic,
            WelcomeLayout::Centered,
            WelcomeLayout::Sidebar,
        ] {
            let ui = crate::state::TrackedMutex::new(welcome_demo_ui(
                configs,
                WelcomeDemoScenario::Fresh,
                WelcomeStep::Spotify,
            ));
            let mut ui = ui.lock();
            if let PageState::Welcome { state, .. } = ui.current_page_mut() {
                state.layout = layout;
            }
            let rendered = draw(&mut ui, 110, 30);
            let rect = target(&ui, Hit::WelcomeAction(3));
            let event = mouse(MouseEventKind::Down(MouseButton::Left), rect.x, rect.y);
            assert!(!handle_welcome_demo_mouse(&mut ui, event, false));
            assert_eq!(welcome_demo_position(&ui), Some((WelcomeStep::Spotify, 0)));
            // The first click selects and focuses, like a Settings row.
            assert!(!handle_welcome_demo_mouse(&mut ui, event, true));
            assert_eq!(welcome_demo_position(&ui), Some((WelcomeStep::Spotify, 3)));
            assert!(!ui.spotify_auth_status.ready());
            // A repeated click activates the same action as Enter.
            assert!(!handle_welcome_demo_mouse(&mut ui, event, true));
            assert!(ui.spotify_auth_status.ready());
            let mut keyboard =
                welcome_demo_ui(configs, WelcomeDemoScenario::Fresh, WelcomeStep::Spotify);
            keyboard.current_page_mut().select(3);
            handle_welcome_demo_key(
                &mut keyboard,
                KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE),
            );
            assert_eq!(ui.spotify_auth_status, keyboard.spotify_auth_status);
            if layout != WelcomeLayout::Classic {
                assert!(rendered.contains("Connect your Spotify account"));
            }
        }
    }

    #[test]
    fn welcome_compact_mouse_targets_follow_visible_scroll_offset_and_step_changes() {
        let configs = crate::ui::initialize_test_config();
        for layout in [WelcomeLayout::Centered, WelcomeLayout::Sidebar] {
            let ui = crate::state::TrackedMutex::new(welcome_demo_ui(
                configs,
                WelcomeDemoScenario::Fresh,
                WelcomeStep::Spotify,
            ));
            let mut ui = ui.lock();
            if let PageState::Welcome { state, .. } = ui.current_page_mut() {
                state.layout = layout;
            }
            ui.current_page_mut().select(5);
            assert!(draw(&mut ui, 40, 16).contains("Next: YouTube"));
            let rect = target(&ui, Hit::WelcomeAction(5));
            double_click(&mut ui, Hit::WelcomeAction(5));
            assert_eq!(welcome_demo_position(&ui), Some((WelcomeStep::YouTube, 0)));
            // The step change drops last frame's rows before the next draw.
            assert_eq!(ui.workspace_hit_at(rect.x, rect.y), None);
        }
    }

    #[test]
    fn welcome_editor_mouse_owns_input_save_cancel_and_blocks_background_wheel() {
        let configs = crate::ui::initialize_test_config();
        let ui = crate::state::TrackedMutex::new(welcome_demo_ui(
            configs,
            WelcomeDemoScenario::Fresh,
            WelcomeStep::Spotify,
        ));
        let mut ui = ui.lock();
        draw(&mut ui, 100, 30);
        double_click(&mut ui, Hit::WelcomeAction(1));
        assert!(matches!(ui.popup, Some(PopupState::ConfigEdit { .. })));
        draw(&mut ui, 100, 30);
        // The wheel never reaches the page behind the editor.
        let behind = target(&ui, Hit::WelcomeAction(3));
        handle_welcome_demo_mouse(
            &mut ui,
            mouse(MouseEventKind::ScrollDown, behind.x, behind.y),
            true,
        );
        assert_eq!(welcome_demo_position(&ui), Some((WelcomeStep::Spotify, 1)));
        assert!(ui.popup.is_some());
        let save = target(&ui, Hit::WelcomeEditorConfirm);
        handle_welcome_demo_mouse(
            &mut ui,
            mouse(MouseEventKind::Down(MouseButton::Left), save.x, save.y),
            true,
        );
        assert!(ui.popup.is_some()); // invalid empty input remains editable
        if let Some(PopupState::ConfigEdit { input, .. }) = &mut ui.popup {
            *input = super::super::single_line_input::LineInput::new(
                "0123456789abcdef0123456789abcdef".chars().collect(),
            );
        }
        draw(&mut ui, 100, 30);
        let save = target(&ui, Hit::WelcomeEditorConfirm);
        handle_welcome_demo_mouse(
            &mut ui,
            mouse(MouseEventKind::Down(MouseButton::Left), save.x, save.y),
            true,
        );
        assert!(ui.popup.is_none());
        assert_eq!(
            ui.welcome_spotify_client_id,
            "0123456789abcdef0123456789abcdef"
        );
        ui.current_page_mut().select(1);
        activate_welcome_demo_selection(&mut ui);
        draw(&mut ui, 40, 16);
        let cancel = target(&ui, Hit::WelcomeEditorCancel);
        handle_welcome_demo_mouse(
            &mut ui,
            mouse(MouseEventKind::Down(MouseButton::Left), cancel.x, cancel.y),
            true,
        );
        assert!(ui.popup.is_none());
        assert_eq!(
            ui.welcome_spotify_client_id,
            "0123456789abcdef0123456789abcdef"
        );

        // A click outside the editor closes it without activating the row there.
        activate_welcome_demo_selection(&mut ui);
        draw(&mut ui, 100, 30);
        let behind = target(&ui, Hit::WelcomeAction(3));
        handle_welcome_demo_mouse(
            &mut ui,
            mouse(MouseEventKind::Down(MouseButton::Left), behind.x, behind.y),
            true,
        );
        assert!(ui.popup.is_none());
        assert!(!ui.spotify_auth_status.ready());
    }

    #[test]
    fn welcome_layout_sidebar_steps_and_wheel_use_page_state() {
        let configs = crate::ui::initialize_test_config();
        let ui = crate::state::TrackedMutex::new(welcome_demo_ui(
            configs,
            WelcomeDemoScenario::Fresh,
            WelcomeStep::Spotify,
        ));
        let mut ui = ui.lock();
        if let PageState::Welcome { state, .. } = ui.current_page_mut() {
            state.layout = WelcomeLayout::Sidebar;
        }
        draw(&mut ui, 110, 30);
        let row = target(&ui, Hit::WelcomeAction(0));
        handle_welcome_demo_mouse(
            &mut ui,
            mouse(MouseEventKind::ScrollDown, row.x, row.y),
            true,
        );
        assert_eq!(welcome_demo_position(&ui), Some((WelcomeStep::Spotify, 1)));
        draw(&mut ui, 110, 30);
        let step = target(&ui, Hit::WelcomeStep(WelcomeStep::Preferences));
        handle_welcome_demo_mouse(
            &mut ui,
            mouse(MouseEventKind::Down(MouseButton::Left), step.x, step.y),
            true,
        );
        assert_eq!(
            welcome_demo_position(&ui),
            Some((WelcomeStep::Preferences, 0))
        );
    }

    #[test]
    fn welcome_resized_frame_replaces_old_targets_and_other_popups_block_them() {
        let configs = crate::ui::initialize_test_config();
        let ui = crate::state::TrackedMutex::new(welcome_demo_ui(
            configs,
            WelcomeDemoScenario::Fresh,
            WelcomeStep::Spotify,
        ));
        let mut ui = ui.lock();
        if let PageState::Welcome { state, .. } = ui.current_page_mut() {
            state.layout = WelcomeLayout::Centered;
        }
        draw(&mut ui, 110, 40);
        let old = target(&ui, Hit::WelcomeAction(3));
        draw(&mut ui, 40, 16);
        assert_eq!(
            ui.workspace_hit_at(old.x.saturating_add(old.width).saturating_sub(1), old.y),
            None
        );
        let row = target(&ui, Hit::WelcomeAction(2));
        ui.popup = Some(PopupState::ConfigEdit {
            key: "theme".to_owned(),
            input: super::super::single_line_input::LineInput::default(),
        });
        let click = mouse(MouseEventKind::Down(MouseButton::Left), row.x, row.y);
        assert!(!handle_welcome_demo_mouse(&mut ui, click, true));
        assert!(!handle_welcome_demo_mouse(&mut ui, click, true));
        assert_eq!(welcome_demo_position(&ui), Some((WelcomeStep::Spotify, 0)));
    }

    #[test]
    fn welcome_youtube_recovery_keeps_focus_result_and_popup_mouse_ownership() {
        let configs = crate::ui::initialize_test_config();
        let ui = crate::state::TrackedMutex::new(welcome_demo_ui(
            configs,
            WelcomeDemoScenario::Fresh,
            WelcomeStep::YouTube,
        ));
        let mut ui = ui.lock();
        ui.welcome_youtube_operation = WelcomeOperation::Failed;
        ui.welcome_youtube_notice =
            Some("No browser found. Choose a path or Import cookies.".to_owned());
        for row in 0..WelcomeStep::YouTube.row_count() {
            ui.current_page_mut().select(row);
            let rendered = draw(&mut ui, 40, 16);
            assert!(target(&ui, Hit::WelcomeAction(row)).height > 0);
            assert!(rendered.contains("No browser found"));
            assert!(!rendered.contains("Browser missing"));
        }
        ui.welcome_youtube_notice =
            Some("Browser could not start. Choose a browser path or Import cookies.".to_owned());
        let wide = draw(&mut ui, 120, 30);
        assert!(wide.contains("Browser could not start."));
        assert!(wide.contains("cookies."), "{wide}");
        ui.popup = Some(PopupState::ConfigEdit {
            key: "welcome.youtube.cookies".to_owned(),
            input: super::super::single_line_input::LineInput::default(),
        });
        let rendered = draw(&mut ui, 40, 16);
        assert!(rendered.contains("Import YouTube cookies"));
        let save = target(&ui, Hit::WelcomeEditorConfirm);
        assert_eq!(
            ui.workspace_hit_at(save.x, save.y),
            Some(Hit::WelcomeEditorConfirm)
        );
        let cancel = target(&ui, Hit::WelcomeEditorCancel);
        assert_eq!(
            ui.workspace_hit_at(cancel.x, cancel.y),
            Some(Hit::WelcomeEditorCancel)
        );
        // The wheel over the editor leaves the page and the editor alone.
        let selected = welcome_demo_position(&ui);
        handle_welcome_demo_mouse(
            &mut ui,
            mouse(MouseEventKind::ScrollDown, save.x, save.y),
            true,
        );
        assert_eq!(welcome_demo_position(&ui), selected);
        assert!(ui.popup.is_some());
    }

    #[test]
    fn welcome_compact_provider_focus_keeps_selected_auth_action_visible() {
        let configs = crate::ui::initialize_test_config();
        let ui = crate::state::TrackedMutex::new(welcome_demo_ui(
            configs,
            WelcomeDemoScenario::Fresh,
            WelcomeStep::Spotify,
        ));
        let mut ui = ui.lock();
        ui.current_page_mut().select(3);
        let (rendered, focused) = draw_focus(&mut ui, 40, 16);
        assert!(rendered.contains("Check existing session"));
        assert_eq!(focused, vec![3]);
        assert!(target(&ui, Hit::WelcomeAction(3)).height > 0);
    }

    #[test]
    fn welcome_spotify_choices_and_auth_boundaries_render_without_client_values() {
        let configs = crate::ui::initialize_test_config();
        let rendered = render_welcome_demo(
            configs,
            WelcomeDemoScenario::Fresh,
            WelcomeStep::Spotify,
            100,
            30,
        )
        .unwrap();
        assert!(rendered.contains("Bundled client (ncspot)"));
        assert!(rendered.contains("Not connected"));
        assert!(rendered.contains("Use my own client ID"));
        assert!(rendered.contains("separate checks"));
        assert!(!rendered.contains(crate::auth::NCSPOT_CLIENT_ID));
    }

    #[test]
    fn custom_client_guidance_shows_the_configured_redirect_uri() {
        let configs = crate::ui::initialize_test_config();
        let redirect_uri = configs.app_config.login_redirect_uri.as_str();
        let custom = WelcomeStep::Spotify
            .actions()
            .iter()
            .position(|action| *action == WelcomeAction::SpotifyCustomClient)
            .unwrap();

        // The wide inspector explains the selected custom-client choice.
        let ui = crate::state::TrackedMutex::new(welcome_demo_ui(
            configs,
            WelcomeDemoScenario::Fresh,
            WelcomeStep::Spotify,
        ));
        let mut ui = ui.lock();
        ui.current_page_mut().select(custom);
        assert!(draw(&mut ui, 140, 30).contains(redirect_uri));

        // The editor repeats it in full, even on the narrowest supported size.
        assert!(!activate_welcome_demo_selection(&mut ui));
        assert!(matches!(ui.popup, Some(PopupState::ConfigEdit { .. })));
        for (width, height) in [(40, 16), (100, 30)] {
            let rendered = draw(&mut ui, width, height);
            assert!(rendered.contains(redirect_uri), "{width}x{height}");
            assert!(rendered.contains("Redirect URIs"), "{width}x{height}");
            assert!(
                rendered.contains("completes automatically"),
                "{width}x{height}"
            );
        }
    }

    #[test]
    fn all_welcome_steps_share_the_production_frame_and_action_focus() {
        for step in WelcomeStep::ALL {
            let (rendered, focused) = render_focus(
                WelcomeDemoScenario::Fresh,
                step,
                120,
                40,
                WelcomeLayout::Sidebar,
            );
            assert!(rendered.contains("Setup"));
            assert!(rendered.contains(&format!(
                "Step {} of 5 · {}",
                step.index() + 1,
                step.title()
            )));
            assert_eq!(focused, vec![0]);
        }
    }

    #[test]
    fn welcome_provider_content_keeps_status_and_result_between_action_groups() {
        let (spotify, spotify_focus) = render_focus(
            WelcomeDemoScenario::SpotifyCached,
            WelcomeStep::Spotify,
            80,
            24,
            WelcomeLayout::Centered,
        );
        let spotify_positions = [
            "Bundled client",
            "Library:",
            "Sign in with Spotify",
            "Check existing session",
            "Result:",
        ]
        .map(|label| spotify.find(label).expect(label));
        assert!(spotify_positions.windows(2).all(|pair| pair[0] < pair[1]));

        let (youtube, youtube_focus) = render_focus(
            WelcomeDemoScenario::YouTubeChecking,
            WelcomeStep::YouTube,
            80,
            24,
            WelcomeLayout::Centered,
        );
        let youtube_positions = [
            "Account:",
            "Sign in with a dedicated browser",
            "Check account & playback",
            "Result:",
        ]
        .map(|label| youtube.find(label).expect(label));
        assert!(youtube_positions.windows(2).all(|pair| pair[0] < pair[1]));
        assert_eq!(spotify_focus, vec![0]);
        assert_eq!(youtube_focus, vec![0]);
    }

    #[test]
    fn welcome_spotify_narrow_view_keeps_last_action_reachable_and_apply_visible() {
        let configs = crate::ui::initialize_test_config();
        let ui = crate::state::TrackedMutex::new(welcome_demo_ui(
            configs,
            WelcomeDemoScenario::Fresh,
            WelcomeStep::Spotify,
        ));
        let mut ui = ui.lock();
        ui.welcome_spotify_client_pending = true;
        ui.current_page_mut().select(5);
        let mut terminal = Terminal::new(TestBackend::new(40, 16)).unwrap();
        terminal
            .draw(|frame| page::render_welcome_page(true, frame, &mut ui, frame.area()))
            .unwrap();
        let buffer = terminal.backend().buffer();
        let rendered: String = buffer.content.iter().map(|cell| cell.symbol()).collect();
        assert!(rendered.contains("Next: YouTube"));
        assert!(rendered.contains("Apply & sign in"));
        assert!(target(&ui, Hit::WelcomeAction(5)).height > 0);
    }

    #[test]
    fn welcome_spotify_results_wrap_and_selected_action_remains_clickable() {
        for (width, height) in [(40, 16), (80, 24), (100, 26)] {
            for selected in 0..4 {
                let configs = crate::ui::initialize_test_config();
                let ui = crate::state::TrackedMutex::new(welcome_demo_ui(
                    configs,
                    WelcomeDemoScenario::AuthFailed,
                    WelcomeStep::Spotify,
                ));
                let mut ui = ui.lock();
                ui.welcome_spotify_operation = crate::state::WelcomeOperation::RateLimited;
                ui.welcome_spotify_notice = Some(
                    "Spotify limited requests. Retry after 42s using Check existing session."
                        .to_owned(),
                );
                ui.current_page_mut().select(selected);
                let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
                terminal
                    .draw(|frame| page::render_welcome_page(true, frame, &mut ui, frame.area()))
                    .unwrap();
                let rendered: String = terminal
                    .backend()
                    .buffer()
                    .content
                    .iter()
                    .map(|cell| cell.symbol())
                    .collect();
                assert!(rendered.contains("42s"), "{width}x{height}: {rendered}");
                assert!(target(&ui, Hit::WelcomeAction(selected)).height > 0);
            }
        }
    }

    #[test]
    fn oauth_demo_uses_the_real_welcome_readiness_copy() {
        let (rendered, focused) = render_focus(
            WelcomeDemoScenario::YouTubeOAuthReady,
            WelcomeStep::YouTube,
            100,
            26,
            WelcomeLayout::Classic,
        );

        assert!(rendered.contains("Setup · Step 3 of 5 · YouTube"));
        assert!(rendered.contains("native streams first"));
        assert!(rendered.contains("OAuth ready"));
        assert!(rendered.contains("Next: ListenBrainz"));
        assert_eq!(focused, vec![0]);
        assert!(!rendered.contains("oauth.json"));
        assert!(rendered.contains("Import cookies"));
    }

    #[test]
    fn demo_scenarios_reject_unlisted_input() {
        assert!(WelcomeDemoScenario::from_cli("live-account").is_err());
        assert!(welcome_demo_step_from_cli("account-picker").is_err());
        assert!(welcome_demo_layout_from_cli("web").is_err());
        for layout in [WelcomeLayout::Centered, WelcomeLayout::Sidebar] {
            let rendered = render_welcome_demo_layout(
                crate::ui::initialize_test_config(),
                WelcomeDemoScenario::AuthFailed,
                WelcomeStep::Spotify,
                110,
                30,
                layout,
            )
            .unwrap();
            assert!(rendered.contains("Authentication failed"));
        }
    }

    #[test]
    fn bounded_demo_sizes_keep_the_actions_visible() {
        for (width, height) in [(40, 16), (80, 24), (240, 80)] {
            let (rendered, focused) = render_focus(
                WelcomeDemoScenario::Fresh,
                WelcomeStep::Review,
                width,
                height,
                WelcomeLayout::Classic,
            );
            assert!(rendered.contains("Review"));
            assert!(rendered.contains("Continue with"));
            assert!(rendered.contains("Skip setup"));
            assert_eq!(focused, vec![0]);
        }
    }

    #[test]
    fn welcome_renderer_handles_zero_and_tiny_buffers_without_panicking() {
        let configs = crate::ui::initialize_test_config();
        for (width, height) in [(0, 0), (1, 1), (10, 4), (39, 15)] {
            render_welcome_demo_layout(
                configs,
                WelcomeDemoScenario::Fresh,
                WelcomeStep::Review,
                width,
                height,
                WelcomeLayout::Centered,
            )
            .unwrap();
        }
    }

    #[test]
    fn wide_review_table_keeps_each_provider_status_visible() {
        let configs = crate::ui::initialize_test_config();
        let rendered = render_welcome_demo_layout(
            configs,
            WelcomeDemoScenario::Fresh,
            WelcomeStep::Review,
            120,
            40,
            WelcomeLayout::Sidebar,
        )
        .unwrap();
        for value in [
            "Provider readiness",
            "Library: missing",
            "Playback: missing",
            "Premium: unverified",
            "Auth: missing",
            "Account: unchecked",
            "Playback: untested",
            "Fix Spotify",
            "Fix YouTube",
        ] {
            assert!(rendered.contains(value), "missing {value:?} in {rendered}");
        }
    }

    #[test]
    fn narrow_provider_step_uses_complete_compact_labels() {
        let configs = crate::ui::initialize_test_config();
        let rendered = render_welcome_demo(
            configs,
            WelcomeDemoScenario::Fresh,
            WelcomeStep::YouTube,
            40,
            16,
        )
        .unwrap();
        assert!(rendered.contains("Account:"));
        assert!(rendered.contains("Sign in with a dedicated browser"));
    }

    #[test]
    fn interactive_demo_navigation_and_auth_changes_only_synthetic_state() {
        let configs = crate::ui::initialize_test_config();
        let mut ui = welcome_demo_ui(
            configs,
            WelcomeDemoScenario::Fresh,
            WelcomeStep::Preferences,
        );

        assert!(!handle_welcome_demo_key(
            &mut ui,
            KeyEvent::new(KeyCode::Down, KeyModifiers::NONE)
        ));
        assert_eq!(
            welcome_demo_position(&ui),
            Some((WelcomeStep::Preferences, 1))
        );
        assert!(!activate_welcome_demo_selection(&mut ui));
        assert!(ui.setup_state.pause_on_startup);

        show_welcome_demo_step(&mut ui, Some(WelcomeStep::Spotify));
        move_welcome_demo_selection(&mut ui, true);
        move_welcome_demo_selection(&mut ui, true);
        assert!(!activate_welcome_demo_selection(&mut ui));
        assert!(ui.spotify_auth_status.ready());

        show_welcome_demo_step(&mut ui, Some(WelcomeStep::YouTube));
        assert!(!activate_welcome_demo_selection(&mut ui));
        assert!(ui.youtube_auth_status.ready);
        assert_eq!(
            ui.youtube_auth_status.auth_type,
            YouTubeMusicAuthType::Browser
        );

        show_welcome_demo_step(&mut ui, Some(WelcomeStep::Review));
        ui.current_page_mut().select(3);
        assert!(activate_welcome_demo_selection(&mut ui));
    }
}
