use anyhow::Context as _;
use command::CommandOrAction;
use std::collections::{HashMap, HashSet};

use super::bulk_action::{
    current_spotify_epoch, current_youtube_epoch, dispatch_queue_menu,
    dispatch_spotify_native_queue_tracks, dispatch_unified_playlist_menu,
    dispatch_youtube_queue_tracks, replan_queue_menu, replan_unified_playlist_menu,
    spotify_bulk_action_menu, youtube_bulk_action_menu,
};

use crate::command::{
    construct_album_actions, construct_playlist_actions, construct_show_actions,
    plan_structural_block_move, StructuralMoveHandle,
};
use crate::state::{
    journal_selected_or_cursor_indices,
    page::{SettingsCategory, SettingsRailItem, SettingsWorkspaceAction},
    queue_selected_or_cursor_indices, settings_filter_projection, synchronize_journal_uris,
    synchronize_native_spotify_queue, synchronize_unified_playlist_entries,
    synchronize_unified_queue_items, unified_playlist_action_model_with_listenbrainz,
    unified_playlist_selected_or_cursor_indices, youtube_context_selected_or_cursor_indices,
    JournalSelection, JournalSelectionScope, LyricsActionMenu, MediaId, MutablePlaylistController,
    OccurrenceDescriptor, PlaylistActionModel, PlaylistCapabilities, PlaylistSnapshot,
    QueueActionItem, QueueActionMenu, QueueActionPayload, QueueSelectionScope, QueueSelectionView,
    ScopedSelectionStatus, TrackJournalEntry, UnifiedPlaylistActionItem, UnifiedPlaylistActionMenu,
    UnifiedPlaylistContextActionMenu, UnifiedPlaylistSelectionScope, WelcomeAction, WelcomeMove,
    WelcomeOperation, WelcomeStep, WorkspaceAction, WorkspaceFocusState, WorkspaceHit,
    WorkspaceNavigationItem, WorkspacePlaybackOption, WorkspaceScopeKind, WorkspaceScopeOption,
    WorkspaceScopeSelection, YouTubeContext, YouTubeTrack,
};

use super::*;

const YOUTUBE_LIBRARY_ACTIONS_UNAVAILABLE_MESSAGE: &str =
    "Library row actions are unavailable for YouTube Music.";
const YOUTUBE_LIBRARY_ACTIONS_UNAVAILABLE_NEXT_ACTION: &str =
    "Press Enter to open it, then choose an action on its tracks.";

pub(crate) fn open_spotify_playlist_context_actions(
    playlist: &crate::state::Playlist,
    data: &DataReadGuard,
    ui: &mut UIStateGuard,
) -> bool {
    let modifiable = crate::state::spotify_playlist_is_modifiable(
        playlist,
        data.user_data.user.as_ref().map(|user| &user.id),
    );
    let actions = PlaylistActionModel::new(construct_playlist_actions(playlist, data))
        .constrained_by(&PlaylistCapabilities::spotify(modifiable))
        .context_actions()
        .to_vec();
    ui.popup = Some(PopupState::ActionList(
        Box::new(ActionListItem::Playlist(playlist.clone(), actions)),
        ListState::default(),
    ));
    true
}

fn open_youtube_playlist_context_actions(
    playlist_id: &str,
    name: &str,
    editable: bool,
    ui: &mut UIStateGuard,
) -> bool {
    let mut actions = vec![Action::CopyLink];
    if editable {
        actions.extend([Action::RenamePlaylist, Action::DeletePlaylist]);
    }
    let action_model = PlaylistActionModel::new(actions);
    ui.popup = Some(PopupState::ActionList(
        Box::new(ActionListItem::YouTubePlaylistContext(
            crate::state::YouTubePlaylistContextActionMenu::with_actions(
                playlist_id.to_owned(),
                name.to_owned(),
                action_model.context_actions().iter().copied(),
            ),
        )),
        ListState::default(),
    ));
    true
}

fn open_unified_playlist_context_actions(
    playlist_id: &str,
    name: &str,
    actions: impl IntoIterator<Item = Action>,
    ui: &mut UIStateGuard,
) -> bool {
    ui.popup = Some(PopupState::ActionList(
        Box::new(ActionListItem::UnifiedPlaylistContext(
            UnifiedPlaylistContextActionMenu::with_actions(
                playlist_id.to_owned(),
                name.to_owned(),
                actions,
            ),
        )),
        ListState::default(),
    ));
    true
}

pub fn handle_key_sequence_for_page(
    key_sequence: &KeySequence,
    client_pub: &crate::client::ClientRequestSender,
    state: &SharedState,
    ui: &mut UIStateGuard,
) -> Result<bool> {
    let page_type = ui.current_page().page_type();
    // handle search page separately as it needs access to the raw key sequence
    // as opposed to the matched command
    if page_type == PageType::Search {
        return handle_key_sequence_for_search_page(key_sequence, client_pub, state, ui);
    }

    if page_type == PageType::Home
        && ui.workspace_focus == WorkspaceFocusState::Context
        && super::home::handle_horizontal_key(key_sequence, state, ui)
    {
        return Ok(true);
    }

    if page_type == PageType::Settings && ui.workspace_focus == WorkspaceFocusState::Context {
        if let [crate::key::Key::None(code)] = key_sequence.keys.as_slice() {
            let delta = match code {
                crossterm::event::KeyCode::Left => Some(-1),
                crossterm::event::KeyCode::Right => Some(1),
                _ => None,
            };
            if let Some(delta) = delta {
                move_settings_tiles_horizontally(ui, delta);
                return Ok(true);
            }
        }
    }

    if page_type == PageType::Settings {
        if let Some(CommandOrAction::Command(command)) = config::get_config()
            .keymap_config
            .find_command_or_action_from_key_sequence(key_sequence)
        {
            match ui.workspace_focus {
                WorkspaceFocusState::Navigation => {
                    return handle_settings_workspace_navigation_command(command, ui);
                }
                WorkspaceFocusState::Actions => {
                    return handle_settings_workspace_secondary_command(command, ui);
                }
                WorkspaceFocusState::Context | WorkspaceFocusState::Queue => {}
            }
        }
    }

    match config::get_config()
        .keymap_config
        .find_command_or_action_from_key_sequence(key_sequence)
    {
        Some(CommandOrAction::Command(command)) => {
            // Welcome routes its rail and button row itself.
            if page_type != PageType::Welcome {
                match ui.workspace_focus {
                    WorkspaceFocusState::Navigation => {
                        return handle_workspace_navigation_command(command, client_pub, state, ui);
                    }
                    WorkspaceFocusState::Queue | WorkspaceFocusState::Actions => {
                        return handle_workspace_secondary_command(command, client_pub, state, ui);
                    }
                    WorkspaceFocusState::Context => {}
                }
            }
            match page_type {
                PageType::Home => {
                    super::home::handle_command_for_home_page(command, client_pub, state, ui)
                }
                PageType::HomeShelfList => {
                    super::home::handle_command_for_home_shelf_list(command, client_pub, state, ui)
                }
                PageType::Welcome => handle_command_for_welcome_page(command, client_pub, ui),
                PageType::Search => anyhow::bail!("page search type should already be handled!"),
                PageType::Library => {
                    handle_command_for_library_page(command, client_pub, ui, state)
                }
                PageType::Context => {
                    handle_command_for_context_page(command, client_pub, ui, state)
                }
                PageType::YouTubeContext => {
                    handle_command_for_youtube_context_page(command, client_pub, ui, state)
                }
                PageType::UnifiedPlaylist => {
                    handle_command_for_unified_playlist_page(command, client_pub, ui, state)
                }
                PageType::Browse => handle_command_for_browse_page(command, client_pub, ui, state),
                PageType::Lyrics => handle_command_for_lyrics_page(command, client_pub, state, ui),
                PageType::Journal => {
                    handle_command_for_journal_page(command, client_pub, ui, state)
                }
                PageType::JournalLists => handle_command_for_journal_lists_page(command, ui, state),
                PageType::JournalList => {
                    handle_command_for_journal_list_page(command, client_pub, ui, state)
                }
                PageType::SessionHistory => {
                    super::popup::handle_session_history_command(command, state, ui)
                }
                PageType::Queue => handle_command_for_queue_page(command, client_pub, state, ui),
                PageType::Settings => {
                    handle_command_for_settings_page(command, client_pub, state, ui)
                }
                PageType::CommandHelp => Ok(handle_command_for_command_help_page(command, ui)),
                PageType::Logs => Ok(handle_command_for_logs_page(command, state, ui)),
            }
        }
        Some(CommandOrAction::Action(action, ActionTarget::SelectedItem)) => match page_type {
            PageType::Search => anyhow::bail!("page search type should already be handled!"),
            PageType::Library => handle_action_for_library_page(action, client_pub, ui, state),
            PageType::Context => {
                window::handle_action_for_focused_context_page(action, client_pub, ui, state)
            }
            PageType::YouTubeContext => {
                handle_action_for_youtube_context_page(action, client_pub, ui, state)
            }
            PageType::UnifiedPlaylist => {
                handle_action_for_unified_playlist_page(action, client_pub, ui, state)
            }
            PageType::Browse => handle_action_for_browse_page(action, client_pub, ui, state),
            PageType::Journal => handle_action_for_journal_page(action, client_pub, ui, state),
            PageType::JournalList => {
                handle_action_for_journal_list_page(action, client_pub, ui, state)
            }
            PageType::Queue => handle_action_for_queue_page(action, client_pub, state, ui),
            _ => Ok(false),
        },
        _ => Ok(false),
    }
}

pub(super) fn handle_command_for_welcome_page(
    command: Command,
    client_pub: &crate::client::ClientRequestSender,
    ui: &mut UIStateGuard,
) -> Result<bool> {
    let current_selected = ui.current_page().selected_index().unwrap_or_default();
    let (selected, step, from_settings) = match ui.current_page() {
        PageState::Welcome {
            state,
            from_settings,
        } => (current_selected, state.step, *from_settings),
        _ => anyhow::bail!("expect a welcome page"),
    };

    let count = ui.count_prefix.unwrap_or(1).min(isize::MAX as usize) as isize;
    let movement = match command {
        Command::SelectNextOrScrollDown => Some(WelcomeMove::By(count)),
        Command::SelectPreviousOrScrollUp => Some(WelcomeMove::By(-count)),
        Command::SelectFirstOrScrollToTop | Command::PageSelectPreviousOrScrollUp => {
            Some(WelcomeMove::First)
        }
        Command::SelectLastOrScrollToBottom | Command::PageSelectNextOrScrollDown => {
            Some(WelcomeMove::Last)
        }
        _ => None,
    };
    if let Some(movement) = movement {
        ui.move_welcome(movement);
        return Ok(true);
    }
    // The rail only browses steps; choosing one moves into its actions.
    if ui.welcome_focus() == WorkspaceFocusState::Navigation
        && matches!(command, Command::ChooseSelected | Command::ResumePause)
    {
        ui.set_welcome_focus(WorkspaceFocusState::Context);
        return Ok(true);
    }

    match command {
        Command::PreviousPage => {
            if show_welcome_step(ui, step.previous()) {
                return Ok(true);
            }
            if from_settings && ui.history.len() > 1 {
                ui.history.pop();
                ui.sync_workspace_after_history_change();
            }
            Ok(true)
        }
        Command::ChooseSelected | Command::ResumePause => {
            let Some(action) = step.action_at(selected) else {
                return Ok(false);
            };
            match action {
                WelcomeAction::PreferencesNext
                | WelcomeAction::SpotifyNext
                | WelcomeAction::YouTubeNext
                | WelcomeAction::ListenBrainzNext => Ok(show_welcome_step(ui, step.next())),
                WelcomeAction::SpotifyBack
                | WelcomeAction::YouTubeBack
                | WelcomeAction::ListenBrainzBack
                | WelcomeAction::ReviewBack => Ok(show_welcome_step(ui, step.previous())),
                WelcomeAction::PreferencesStartupProvider => {
                    ui.setup_state.startup_provider = ui.setup_state.startup_provider.toggled();
                    ui.mark_setup_pending();
                    persist_setup_draft(ui);
                    Ok(true)
                }
                WelcomeAction::PreferencesStartPaused => {
                    ui.setup_state.pause_on_startup = !ui.setup_state.pause_on_startup;
                    ui.mark_setup_pending();
                    persist_setup_draft(ui);
                    Ok(true)
                }
                WelcomeAction::SpotifyBundledClient => {
                    save_welcome_spotify_client(ui, crate::auth::NCSPOT_CLIENT_ID);
                    Ok(true)
                }
                WelcomeAction::SpotifyCustomClient => {
                    if ui.welcome_spotify_auth_in_flight {
                        ui.welcome_spotify_notice = Some(
                            "Wait for Spotify sign-in/check to finish before changing the client."
                                .to_owned(),
                        );
                        return Ok(true);
                    }
                    ui.popup = Some(PopupState::ConfigEdit {
                        key: "client_id".to_owned(),
                        input: LineInput::new(
                            if ui.welcome_spotify_client_id == crate::auth::NCSPOT_CLIENT_ID {
                                Vec::new()
                            } else {
                                ui.welcome_spotify_client_id.chars().collect()
                            },
                        ),
                    });
                    Ok(true)
                }
                WelcomeAction::SpotifySignIn => {
                    if !ui.begin_welcome_spotify_action(WelcomeOperation::SigningIn) {
                        return Ok(true);
                    }
                    ui.welcome_spotify_library_tested = None;
                    ui.welcome_spotify_playback_tested = None;
                    ui.mark_setup_pending();
                    ui.welcome_spotify_notice = Some(
                        "Opening Spotify sign-in; complete both browser approvals if requested."
                            .to_owned(),
                    );
                    if let Err(error) =
                        client_pub.send(crate::client::ClientRequest::ReauthenticateSpotify)
                    {
                        ui.finish_welcome_spotify_action(WelcomeOperation::Failed);
                        ui.welcome_spotify_notice =
                            Some("Could not start Spotify sign-in; retry.".to_owned());
                        return Err(error.into());
                    }
                    Ok(true)
                }
                WelcomeAction::SpotifyCheckSession => {
                    if ui.welcome_spotify_client_pending
                        || ui.setup_state.spotify_reauthentication_required
                    {
                        ui.welcome_spotify_notice = Some(
                            "Client changed. Use Apply & sign in before checking this session."
                                .to_owned(),
                        );
                        return Ok(true);
                    }
                    if !ui.begin_welcome_spotify_action(WelcomeOperation::Checking) {
                        return Ok(true);
                    }
                    ui.welcome_spotify_library_tested = None;
                    ui.welcome_spotify_playback_tested = None;
                    ui.mark_setup_pending();
                    ui.welcome_spotify_notice = Some(
                        "Checking saved credentials and connecting Spotify playback...".to_owned(),
                    );
                    if let Err(error) =
                        client_pub.send(crate::client::ClientRequest::InitializeSpotifySession)
                    {
                        ui.finish_welcome_spotify_action(WelcomeOperation::Failed);
                        ui.welcome_spotify_notice =
                            Some("Could not start the Spotify check; retry.".to_owned());
                        return Err(error.into());
                    }
                    Ok(true)
                }
                WelcomeAction::YouTubeSignIn => {
                    if ui.welcome_youtube_login_active {
                        client_pub
                            .send(crate::client::ClientRequest::CancelYouTubeAuthentication)?;
                        ui.welcome_youtube_notice =
                            Some("Cancelling sign-in; wait for cleanup.".to_owned());
                        return Ok(true);
                    }
                    if !ui.begin_welcome_youtube_action(WelcomeOperation::SigningIn) {
                        return Ok(true);
                    }
                    ui.welcome_youtube_login_active = true;
                    ui.welcome_youtube_account_tested = None;
                    ui.welcome_youtube_playback_tested = None;
                    ui.mark_setup_pending();
                    ui.welcome_youtube_notice =
                        Some("Opening a dedicated browser for YouTube Music sign-in...".to_owned());
                    if let Err(error) =
                        client_pub.send(crate::client::ClientRequest::AuthenticateYouTubeBrowser)
                    {
                        ui.finish_welcome_youtube_action(WelcomeOperation::Failed);
                        ui.welcome_youtube_notice =
                            Some("Could not start YouTube Music sign-in; retry.".to_owned());
                        return Err(error.into());
                    }
                    Ok(true)
                }
                WelcomeAction::YouTubeTestAccount => {
                    if !ui.begin_welcome_youtube_action(WelcomeOperation::Checking) {
                        return Ok(true);
                    }
                    if !ui.setup_auth_snapshot().youtube.account_ready {
                        ui.welcome_youtube_notice =
                            Some("Sign in or import cookies before checking access.".to_owned());
                        ui.finish_welcome_youtube_action(WelcomeOperation::Idle);
                        return Ok(true);
                    }
                    ui.welcome_youtube_account_tested = None;
                    ui.welcome_youtube_playback_tested = None;
                    ui.mark_setup_pending();
                    ui.welcome_youtube_notice =
                        Some("Testing YouTube Music account and library access...".to_owned());
                    if let Err(error) =
                        client_pub.send(crate::client::ClientRequest::TestYouTubeAuth)
                    {
                        ui.finish_welcome_youtube_action(WelcomeOperation::Failed);
                        ui.welcome_youtube_notice =
                            Some("Could not start the YouTube Music test; retry.".to_owned());
                        return Err(error.into());
                    }
                    Ok(true)
                }
                WelcomeAction::YouTubeChooseBrowser | WelcomeAction::YouTubeImportCookies => {
                    if ui.welcome_youtube_operation.is_busy() {
                        ui.welcome_youtube_notice = Some(
                            "Cancel the current sign-in or wait for the check to finish."
                                .to_owned(),
                        );
                        return Ok(true);
                    }
                    let browser = action == WelcomeAction::YouTubeChooseBrowser;
                    ui.welcome_youtube_notice = None;
                    ui.popup = Some(PopupState::ConfigEdit {
                        key: if browser {
                            "welcome.youtube.browser"
                        } else {
                            "welcome.youtube.cookies"
                        }
                        .to_owned(),
                        input: LineInput::new(if browser {
                            ui.welcome_youtube_browser
                                .as_ref()
                                .map(|path| path.to_string_lossy().chars().collect())
                                .unwrap_or_default()
                        } else {
                            Vec::new()
                        }),
                    });
                    Ok(true)
                }
                WelcomeAction::YouTubeDetectBrowser => {
                    if ui.welcome_youtube_operation.is_busy() {
                        return Ok(true);
                    }
                    ui.finish_welcome_youtube_action(WelcomeOperation::Idle);
                    ui.welcome_youtube_browser = crate::client::resolve_browser_executable(
                        &config::get_config().config_folder,
                        None,
                    )
                    .ok();
                    ui.welcome_youtube_notice = Some(
                        if ui.welcome_youtube_browser.is_some() {
                            "Browser found. Choose Sign in to open it."
                        } else {
                            "No browser found. Choose a browser path or Import cookies."
                        }
                        .to_owned(),
                    );
                    Ok(true)
                }
                WelcomeAction::ListenBrainzEnterToken => {
                    ui.welcome_listenbrainz_identity = None;
                    ui.welcome_listenbrainz_username = None;
                    ui.welcome_listenbrainz_attempt += 1;
                    ui.welcome_listenbrainz_pending = None;
                    ui.welcome_listenbrainz_notice = None;
                    ui.popup = Some(PopupState::ListenBrainzToken {
                        input: crate::ui::single_line_input::SecretInput::default(),
                    });
                    Ok(true)
                }
                WelcomeAction::ListenBrainzCheckToken => {
                    if ui.welcome_listenbrainz_pending.is_some() {
                        return Ok(true);
                    }
                    if let Some(token) = config::get_config().listenbrainz_token() {
                        super::popup::submit_listenbrainz_token(client_pub, ui, token, false);
                    } else {
                        ui.welcome_listenbrainz_username = None;
                        ui.welcome_listenbrainz_identity = None;
                        ui.welcome_listenbrainz_notice = Some(
                            "No saved token. Enter a token or skip this optional step.".to_owned(),
                        );
                    }
                    Ok(true)
                }
                WelcomeAction::ListenBrainzFetchPlaylists => {
                    super::popup::start_listenbrainz_playlist_list(client_pub, ui);
                    Ok(true)
                }
                WelcomeAction::ReviewFixSpotify => {
                    Ok(show_welcome_step(ui, Some(WelcomeStep::Spotify)))
                }
                WelcomeAction::ReviewFixYouTube => {
                    Ok(show_welcome_step(ui, Some(WelcomeStep::YouTube)))
                }
                WelcomeAction::ReviewContinue => {
                    finish_welcome_setup(client_pub, ui, from_settings)
                }
                WelcomeAction::ReviewSkip => skip_welcome_setup(client_pub, ui, from_settings),
            }
        }
        _ => Ok(false),
    }
}

pub(super) fn show_welcome_step(ui: &mut UIStateGuard, step: Option<WelcomeStep>) -> bool {
    let Some(step) = step else {
        return false;
    };
    if !matches!(ui.current_page(), PageState::Welcome { .. }) {
        return false;
    }
    ui.show_welcome_step(step);
    true
}

pub(super) fn save_welcome_spotify_client(ui: &mut UIStateGuard, value: &str) -> bool {
    if ui.welcome_spotify_auth_in_flight {
        ui.welcome_spotify_notice =
            Some("Wait for Spotify sign-in/check to finish before changing the client.".to_owned());
        return false;
    }
    let value = value.trim();
    if value.len() != 32 || !value.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        ui.welcome_spotify_notice =
            Some("Enter a 32-character hexadecimal client ID, not a client secret.".to_owned());
        return false;
    }
    let configs = config::get_config();
    if config::save_welcome_spotify_client(&configs.config_folder, value).is_err() {
        ui.mark_setup_failed(config::SetupFailure::PersistenceFailed);
        ui.welcome_spotify_notice = Some(
            "Could not save the client choice. Check configuration permissions and retry."
                .to_owned(),
        );
        return false;
    }
    value.clone_into(&mut ui.welcome_spotify_client_id);
    ui.welcome_spotify_client_command = false;
    if let Ok(setup) = config::SetupState::load(&configs.config_folder) {
        ui.setup_state.spotify_reauthentication_required = setup.spotify_reauthentication_required;
    }
    ui.welcome_spotify_client_pending = ui.setup_state.spotify_reauthentication_required;
    if ui.welcome_spotify_client_pending {
        ui.spotify_auth_status = config::SpotifyAuthSnapshot::default();
        ui.welcome_spotify_web_token_cached = false;
        ui.welcome_spotify_library_tested = None;
        ui.welcome_spotify_playback_tested = None;
        ui.welcome_spotify_operation = WelcomeOperation::Idle;
        ui.mark_setup_pending();
    }
    ui.welcome_spotify_notice = Some(
        if ui.welcome_spotify_client_pending {
            "Saved. Choose Apply & sign in to use this client now."
        } else {
            "Client selected. Use the existing session or authenticate Spotify."
        }
        .to_owned(),
    );
    true
}

fn welcome_step_for_setup_failure(failure: config::SetupFailure) -> Option<WelcomeStep> {
    match failure {
        config::SetupFailure::MissingSpotifySession
        | config::SetupFailure::MissingSpotifyPremium
        | config::SetupFailure::SpotifySessionUnavailable => Some(WelcomeStep::Spotify),
        config::SetupFailure::MissingYouTubeAccountAuth => Some(WelcomeStep::YouTube),
        config::SetupFailure::AuthenticationFailed | config::SetupFailure::PersistenceFailed => {
            None
        }
    }
}

#[cfg(test)]
mod welcome_navigation_tests {
    use super::*;

    #[test]
    fn leaving_welcome_restores_workspace_mode() {
        crate::ui::initialize_test_config();
        for (from_settings, expected) in [(false, PageType::Home), (true, PageType::Settings)] {
            let ui = crate::state::TrackedMutex::new(crate::state::UIState::default());
            let mut ui = ui.lock();
            ui.history = vec![if from_settings {
                PageState::Settings {
                    list: ListState::default(),
                    shelves: crate::state::SettingsShelves::default(),
                    settings: Vec::new(),
                    saved: false,
                    error: None,
                    notice: None,
                }
            } else {
                PageState::Library {
                    state: crate::state::LibraryPageUIState::new(),
                }
            }];
            ui.open_setup_page(from_settings);
            assert!(ui.current_page().page_type() == PageType::Welcome);
            // Welcome's own pane focus must not leak into the page it returns to.
            ui.set_welcome_focus(WorkspaceFocusState::Actions);

            leave_welcome_page(&mut ui, from_settings);

            assert!(ui.current_page().page_type() == expected);
            assert_eq!(ui.workspace_focus, WorkspaceFocusState::Context);
        }
    }

    #[test]
    fn welcome_live_tab_cycles_panes_and_arrows_stay_inside_the_focused_pane() {
        use crate::state::WelcomePageUIState;
        use crossterm::event::{Event, KeyCode, KeyEvent, KeyModifiers};
        crate::ui::initialize_test_config();
        let ring = std::sync::Arc::new(parking_lot::Mutex::new(std::collections::VecDeque::new()));
        let (diagnostics, _runtime) = crate::observability::disabled(ring);
        let state = std::sync::Arc::new(crate::state::State::new(false, diagnostics));
        let (sender, receiver) = crate::client::client_request_channel();
        {
            let mut ui = state.ui.lock();
            let mut welcome = WelcomePageUIState::new();
            welcome.show_step(WelcomeStep::Spotify);
            // The step rail was drawn in the last frame.
            welcome.rail_visible = true;
            ui.history = vec![PageState::Welcome {
                state: welcome,
                from_settings: false,
            }];
            ui.popup = None;
            ui.workspace_focus = WorkspaceFocusState::Context;
        }
        let press = |code| {
            let key = Event::Key(KeyEvent::new(code, KeyModifiers::NONE));
            crate::event::handle_terminal_event(&key, &sender, &state).unwrap();
            let ui = state.ui.lock();
            let PageState::Welcome { state: page, .. } = ui.current_page() else {
                panic!("Welcome expected")
            };
            (ui.welcome_focus(), page.step, page.list.selected().unwrap())
        };
        use WorkspaceFocusState::{Actions, Context, Navigation};

        // Spotify: rows 0..4 are the step's actions, 4 and 5 are Back/Next.
        assert_eq!(press(KeyCode::Down), (Context, WelcomeStep::Spotify, 1));
        assert_eq!(press(KeyCode::Down), (Context, WelcomeStep::Spotify, 2));
        assert_eq!(press(KeyCode::Down), (Context, WelcomeStep::Spotify, 3));
        assert_eq!(press(KeyCode::Down), (Context, WelcomeStep::Spotify, 3));
        // Entering the button row lands on the forward button.
        assert_eq!(press(KeyCode::Tab), (Actions, WelcomeStep::Spotify, 5));
        assert_eq!(press(KeyCode::Up), (Actions, WelcomeStep::Spotify, 4));
        assert_eq!(press(KeyCode::Up), (Actions, WelcomeStep::Spotify, 4));
        // Returning to the content pane restores its row.
        assert_eq!(press(KeyCode::BackTab), (Context, WelcomeStep::Spotify, 3));
        assert_eq!(
            press(KeyCode::BackTab),
            (Navigation, WelcomeStep::Spotify, 3)
        );
        // The rail browses steps without activating anything.
        assert_eq!(press(KeyCode::Down), (Navigation, WelcomeStep::YouTube, 0));
        assert_eq!(press(KeyCode::Enter), (Context, WelcomeStep::YouTube, 0));
        assert!(receiver.try_recv().is_err());

        // Without a drawn rail, Tab alternates between the two remaining panes.
        if let PageState::Welcome { state, .. } = state.ui.lock().current_page_mut() {
            state.rail_visible = false;
        }
        assert_eq!(press(KeyCode::Tab), (Actions, WelcomeStep::YouTube, 6));
        assert_eq!(press(KeyCode::Tab), (Context, WelcomeStep::YouTube, 0));
        assert_eq!(press(KeyCode::BackTab), (Actions, WelcomeStep::YouTube, 6));
        assert!(receiver.try_recv().is_err());
    }

    #[test]
    fn setup_blocks_shortcuts_into_browsing_pages_but_keeps_help() {
        use crossterm::event::{Event, KeyCode, KeyEvent, KeyModifiers};
        crate::ui::initialize_test_config();
        let ring = std::sync::Arc::new(parking_lot::Mutex::new(std::collections::VecDeque::new()));
        let (diagnostics, _runtime) = crate::observability::disabled(ring);
        let state = std::sync::Arc::new(crate::state::State::new(false, diagnostics));
        let (sender, receiver) = crate::client::client_request_channel();
        {
            let mut ui = state.ui.lock();
            ui.history = vec![PageState::Welcome {
                state: crate::state::WelcomePageUIState::new(),
                from_settings: false,
            }];
            ui.popup = None;
        }
        let press = |code| {
            let key = Event::Key(KeyEvent::new(code, KeyModifiers::NONE));
            crate::event::handle_terminal_event(&key, &sender, &state).unwrap();
            state.ui.lock().current_page().page_type()
        };

        // `l` is the default Lyrics shortcut.
        assert!(press(KeyCode::Char('l')) == PageType::Welcome);
        assert!(state
            .ui
            .lock()
            .operation_status
            .as_ref()
            .is_some_and(|status| status
                .ordinary_display_line()
                .contains("unavailable during first-use setup")));
        assert!(receiver.try_recv().is_err());

        // Help opens as the workspace popup over setup.
        assert!(press(KeyCode::Char('?')) == PageType::Welcome);
        assert!(matches!(
            state.ui.lock().popup,
            Some(PopupState::CommandHelp { .. })
        ));
        assert!(press(KeyCode::Esc) == PageType::Welcome);
        assert!(state.ui.lock().popup.is_none());

        // Logs open as a page above setup, which still blocks shortcuts there.
        assert!(press(KeyCode::Char('g')) == PageType::Welcome);
        assert!(press(KeyCode::Char('o')) == PageType::Logs);
        assert!(press(KeyCode::Char('l')) == PageType::Logs);
        assert!(receiver.try_recv().is_err());
    }

    #[test]
    fn welcome_button_row_activation_moves_to_the_next_step_content() {
        crate::ui::initialize_test_config();
        let mut ui = crate::state::UIState::default();
        let mut welcome = crate::state::WelcomePageUIState::new();
        welcome.rail_visible = true;
        ui.history = vec![PageState::Welcome {
            state: welcome,
            from_settings: false,
        }];
        let ui = crate::state::TrackedMutex::new(ui);
        let mut ui = ui.lock();
        let (sender, _receiver) = crate::client::client_request_channel();
        ui.set_welcome_focus(WorkspaceFocusState::Actions);
        assert!(
            handle_command_for_welcome_page(Command::ChooseSelected, &sender, &mut ui).unwrap()
        );
        let PageState::Welcome { state, .. } = ui.current_page() else {
            panic!("Welcome expected")
        };
        assert_eq!(state.step, WelcomeStep::Spotify);
        assert_eq!(state.list.selected(), Some(0));
        assert_eq!(ui.welcome_focus(), WorkspaceFocusState::Context);
    }

    #[test]
    fn welcome_preferences_update_only_the_saved_draft() -> Result<()> {
        crate::ui::initialize_test_config();
        let state = crate::state::WelcomePageUIState::new();
        let mut ui = crate::state::UIState::default();
        ui.history = vec![PageState::Welcome {
            state,
            from_settings: false,
        }];
        let ui = crate::state::TrackedMutex::new(ui);
        let mut ui = ui.lock();
        let (sender, receiver) = crate::client::client_request_channel();
        let provider = ui.setup_state.startup_provider;
        let paused = ui.setup_state.pause_on_startup;

        ui.current_page_mut().select(0);
        assert!(handle_command_for_welcome_page(
            Command::ChooseSelected,
            &sender,
            &mut ui
        )?);
        assert_ne!(ui.setup_state.startup_provider, provider);
        assert_eq!(ui.setup_state.pause_on_startup, paused);
        assert!(receiver.try_recv().is_err());

        ui.current_page_mut().select(1);
        assert!(handle_command_for_welcome_page(
            Command::ChooseSelected,
            &sender,
            &mut ui
        )?);
        assert_eq!(ui.setup_state.pause_on_startup, !paused);
        assert!(receiver.try_recv().is_err());

        ui.current_page_mut().select(2);
        assert!(handle_command_for_welcome_page(
            Command::ChooseSelected,
            &sender,
            &mut ui
        )?);
        assert!(matches!(
            ui.current_page(),
            PageState::Welcome {
                state: crate::state::WelcomePageUIState {
                    step: WelcomeStep::Spotify,
                    ..
                },
                ..
            }
        ));
        assert!(receiver.try_recv().is_err());
        Ok(())
    }

    #[test]
    fn welcome_youtube_cancel_and_import_are_dispatchable_without_browser() -> Result<()> {
        crate::ui::initialize_test_config();
        let mut ui = crate::state::UIState::default();
        let mut welcome = crate::state::WelcomePageUIState::new();
        welcome.show_step(WelcomeStep::YouTube);
        ui.history = vec![PageState::Welcome {
            state: welcome,
            from_settings: false,
        }];
        let ui = crate::state::TrackedMutex::new(ui);
        let mut ui = ui.lock();
        let (sender, receiver) = crate::client::client_request_channel();
        ui.welcome_youtube_login_active = true;
        ui.welcome_youtube_operation = WelcomeOperation::Waiting;
        handle_command_for_welcome_page(Command::ChooseSelected, &sender, &mut ui)?;
        assert!(matches!(
            receiver.try_recv().unwrap().request(),
            crate::client::ClientRequest::CancelYouTubeAuthentication
        ));
        ui.finish_welcome_youtube_action(WelcomeOperation::Cancelled);
        ui.current_page_mut().select(4);
        handle_command_for_welcome_page(Command::ChooseSelected, &sender, &mut ui)?;
        assert!(
            matches!(&ui.popup, Some(PopupState::ConfigEdit { key, .. }) if key == "welcome.youtube.cookies")
        );
        assert!(receiver.try_recv().is_err());
        let folder = tempfile::tempdir().unwrap();
        let file = folder.path().join("cookies.txt");
        std::fs::write(&file, "synthetic input")?;
        ui.popup = Some(PopupState::ConfigEdit {
            key: "welcome.youtube.cookies".to_owned(),
            input: LineInput::new(file.to_string_lossy().chars().collect()),
        });
        super::super::popup::handle_key_sequence_for_config_edit_popup(
            &KeySequence {
                keys: vec![Key::None(crossterm::event::KeyCode::Enter)],
            },
            &sender,
            &mut ui,
        );
        assert!(
            matches!(receiver.try_recv().unwrap().request(), crate::client::ClientRequest::ImportYouTubeCookies(path) if path == &file)
        );
        assert!(ui.popup.is_none());
        assert!(ui.welcome_youtube_login_active);
        assert!(ui.welcome_youtube_browser.is_none());
        Ok(())
    }

    #[test]
    fn welcome_youtube_requires_sign_in_before_test_and_blocks_overlap() -> Result<()> {
        crate::ui::initialize_test_config();
        let mut state = crate::state::WelcomePageUIState::new();
        state.show_step(WelcomeStep::YouTube);
        let mut ui = crate::state::UIState::default();
        ui.history = vec![PageState::Welcome {
            state,
            from_settings: false,
        }];
        let ui = crate::state::TrackedMutex::new(ui);
        let mut ui = ui.lock();
        let (sender, receiver) = crate::client::client_request_channel();

        ui.current_page_mut().select(1);
        assert!(handle_command_for_welcome_page(
            Command::ChooseSelected,
            &sender,
            &mut ui
        )?);
        assert_eq!(ui.welcome_youtube_account_tested, None);
        assert_eq!(ui.welcome_youtube_playback_tested, None);
        assert!(receiver.try_recv().is_err());

        ui.current_page_mut().select(0);
        assert!(handle_command_for_welcome_page(
            Command::ChooseSelected,
            &sender,
            &mut ui
        )?);
        assert!(matches!(
            receiver.try_recv().unwrap().request(),
            crate::client::ClientRequest::AuthenticateYouTubeBrowser
        ));
        assert_eq!(ui.welcome_youtube_operation, WelcomeOperation::SigningIn);

        ui.current_page_mut().select(1);
        assert!(handle_command_for_welcome_page(
            Command::ChooseSelected,
            &sender,
            &mut ui
        )?);
        assert_eq!(ui.welcome_youtube_operation, WelcomeOperation::Waiting);
        assert!(receiver.try_recv().is_err());

        ui.finish_welcome_youtube_action(WelcomeOperation::Idle);
        ui.youtube_auth_status.ready = true;
        assert!(handle_command_for_welcome_page(
            Command::ChooseSelected,
            &sender,
            &mut ui
        )?);
        assert!(matches!(
            receiver.try_recv().unwrap().request(),
            crate::client::ClientRequest::TestYouTubeAuth
        ));
        assert_eq!(ui.welcome_youtube_operation, WelcomeOperation::Checking);
        Ok(())
    }

    #[test]
    fn review_correction_actions_route_without_starting_provider_work() -> Result<()> {
        let mut state = crate::state::WelcomePageUIState::new();
        state.show_step(WelcomeStep::Review);
        let mut ui = crate::state::UIState::default();
        ui.history = vec![PageState::Welcome {
            state,
            from_settings: false,
        }];
        let ui = crate::state::TrackedMutex::new(ui);
        let mut ui = ui.lock();
        let (sender, receiver) = crate::client::client_request_channel();

        for (selected, target) in [(0, WelcomeStep::Spotify), (1, WelcomeStep::YouTube)] {
            ui.current_page_mut().select(selected);
            assert!(handle_command_for_welcome_page(
                Command::ChooseSelected,
                &sender,
                &mut ui
            )?);
            assert!(matches!(
                ui.current_page(),
                PageState::Welcome {
                    state,
                    ..
                } if state.step == target
            ));
            assert!(receiver.try_recv().is_err());
            if target == WelcomeStep::Spotify {
                let PageState::Welcome { state, .. } = ui.current_page_mut() else {
                    unreachable!()
                };
                state.show_step(WelcomeStep::Review);
            }
        }
        Ok(())
    }

    #[test]
    fn review_continue_routes_missing_startup_readiness_to_its_provider() -> Result<()> {
        crate::ui::initialize_test_config();
        for (provider, target, expected_failure) in [
            (
                config::ActiveProvider::Spotify,
                WelcomeStep::Spotify,
                config::SetupFailure::MissingSpotifySession,
            ),
            (
                config::ActiveProvider::YouTubeMusic,
                WelcomeStep::YouTube,
                config::SetupFailure::MissingYouTubeAccountAuth,
            ),
        ] {
            let mut state = crate::state::WelcomePageUIState::new();
            state.show_step(WelcomeStep::Review);
            let mut ui = crate::state::UIState::default();
            ui.setup_state.startup_provider = provider;
            ui.history = vec![PageState::Welcome {
                state,
                from_settings: false,
            }];
            let ui = crate::state::TrackedMutex::new(ui);
            let mut ui = ui.lock();
            let (sender, receiver) = crate::client::client_request_channel();

            ui.current_page_mut().select(3);
            assert!(handle_command_for_welcome_page(
                Command::ChooseSelected,
                &sender,
                &mut ui
            )?);
            assert!(matches!(
                ui.current_page(),
                PageState::Welcome {
                    state,
                    ..
                } if state.step == target
            ));
            assert_eq!(ui.setup_state.failure, Some(expected_failure));
            assert!(receiver.try_recv().is_err());
        }
        Ok(())
    }

    #[test]
    fn welcome_changed_client_allows_fresh_sign_in_but_requires_it_before_check() {
        crate::ui::initialize_test_config();
        let mut state = crate::state::WelcomePageUIState::new();
        state.show_step(WelcomeStep::Spotify);
        let mut ui = crate::state::UIState::default();
        ui.history = vec![PageState::Welcome {
            state,
            from_settings: false,
        }];
        ui.welcome_spotify_client_pending = true;
        let ui = crate::state::TrackedMutex::new(ui);
        let mut ui = ui.lock();
        let (sender, receiver) = crate::client::client_request_channel();
        ui.setup_state.spotify_reauthentication_required = true;
        ui.current_page_mut().select(3);
        handle_command_for_welcome_page(Command::ChooseSelected, &sender, &mut ui).unwrap();
        assert!(receiver.try_recv().is_err());
        assert!(ui
            .welcome_spotify_notice
            .as_ref()
            .unwrap()
            .contains("Apply & sign in"));
        // The persistent flag also survives restart (the runtime pending bit does not).
        ui.welcome_spotify_client_pending = false;
        ui.current_page_mut().select(2);
        handle_command_for_welcome_page(Command::ChooseSelected, &sender, &mut ui).unwrap();
        assert!(matches!(
            receiver.try_recv().unwrap().request(),
            crate::client::ClientRequest::ReauthenticateSpotify
        ));
        assert!(ui.welcome_spotify_auth_in_flight);
    }

    #[test]
    fn welcome_spotify_client_edits_wait_for_auth_completion() {
        crate::ui::initialize_test_config();
        let mut state = crate::state::WelcomePageUIState::new();
        state.show_step(WelcomeStep::Spotify);
        let mut ui = crate::state::UIState::default();
        ui.history = vec![PageState::Welcome {
            state,
            from_settings: false,
        }];
        ui.welcome_spotify_auth_in_flight = true;
        let previous = ui.welcome_spotify_client_id.clone();
        let ui = crate::state::TrackedMutex::new(ui);
        let mut ui = ui.lock();
        let (sender, receiver) = crate::client::client_request_channel();
        ui.current_page_mut().select(1);
        handle_command_for_welcome_page(Command::ChooseSelected, &sender, &mut ui).unwrap();
        assert!(ui.popup.is_none());
        assert!(!save_welcome_spotify_client(
            &mut ui,
            "0123456789abcdef0123456789abcdef"
        ));
        assert_eq!(ui.welcome_spotify_client_id, previous);
        assert!(receiver.try_recv().is_err());
    }

    #[test]
    fn welcome_spotify_auth_second_dispatch_blocked_while_in_flight() {
        crate::ui::initialize_test_config();
        let mut state = crate::state::WelcomePageUIState::new();
        state.show_step(WelcomeStep::Spotify);
        let mut ui = crate::state::UIState::default();
        ui.history = vec![PageState::Welcome {
            state,
            from_settings: false,
        }];
        let ui = crate::state::TrackedMutex::new(ui);
        let mut ui = ui.lock();
        let (sender, receiver) = crate::client::client_request_channel();
        ui.current_page_mut().select(2);
        assert!(
            handle_command_for_welcome_page(Command::ChooseSelected, &sender, &mut ui).unwrap()
        );
        assert!(ui.welcome_spotify_auth_in_flight);
        receiver.try_recv().expect("first check dispatched");
        // While the first request is in flight, the auth row must not dispatch again.
        ui.current_page_mut().select(3);
        assert!(
            handle_command_for_welcome_page(Command::ChooseSelected, &sender, &mut ui).unwrap()
        );
        assert!(receiver.try_recv().is_err());
        assert_eq!(
            ui.welcome_spotify_notice.as_deref(),
            Some(crate::state::WELCOME_SPOTIFY_AUTH_IN_FLIGHT_NOTICE)
        );
        // After completion the slot is reusable.
        ui.finish_welcome_spotify_auth();
        assert!(
            handle_command_for_welcome_page(Command::ChooseSelected, &sender, &mut ui).unwrap()
        );
        receiver
            .try_recv()
            .expect("auth dispatched after completion");
    }

    #[test]
    fn review_failure_returns_to_the_provider_that_needs_attention() {
        assert_eq!(
            welcome_step_for_setup_failure(config::SetupFailure::MissingSpotifySession),
            Some(WelcomeStep::Spotify)
        );
        assert_eq!(
            welcome_step_for_setup_failure(config::SetupFailure::MissingYouTubeAccountAuth),
            Some(WelcomeStep::YouTube)
        );
        assert_eq!(
            welcome_step_for_setup_failure(config::SetupFailure::PersistenceFailed),
            None
        );
    }
}

fn persist_setup_draft(ui: &mut UIStateGuard) {
    let setup = ui.setup_state.clone();
    if setup.save(&config::get_config().config_folder).is_err() {
        tracing::warn!("First-use setup preferences could not be saved");
        ui.mark_setup_failed(config::SetupFailure::PersistenceFailed);
    }
}

fn persist_setup_completion(ui: &mut UIStateGuard) -> bool {
    let setup = ui.setup_state.clone();
    let provider = match setup.startup_provider {
        config::ActiveProvider::Spotify => "Spotify",
        config::ActiveProvider::YouTubeMusic => "YouTubeMusic",
    };
    // Persist app preferences first so a setup-file write failure leaves the
    // next launch in the attention-required path instead of a false Ready state.
    let result = config::save_app_config_override(
        &config::get_config().config_folder,
        "active_provider",
        provider,
    );
    #[cfg(feature = "streaming")]
    let result = result.and_then(|()| {
        config::save_app_config_override(
            &config::get_config().config_folder,
            "pause_on_startup",
            &setup.pause_on_startup.to_string(),
        )
    });
    let result = result.and_then(|()| setup.save(&config::get_config().config_folder));

    if result.is_err() {
        ui.mark_setup_failed(config::SetupFailure::PersistenceFailed);
        return false;
    }
    true
}

fn finish_welcome_setup(
    client_pub: &crate::client::ClientRequestSender,
    ui: &mut UIStateGuard,
    from_settings: bool,
) -> Result<bool> {
    if let Some(failure) = ui.setup_state.failure_for(ui.setup_auth_snapshot()) {
        ui.mark_setup_failed(failure);
        persist_setup_draft(ui);
        show_welcome_step(ui, welcome_step_for_setup_failure(failure));
        return Ok(true);
    }

    ui.setup_state.status = config::SetupStatus::Ready;
    ui.setup_state.failure = None;
    if !persist_setup_completion(ui) {
        return Ok(true);
    }

    let provider = ui.setup_state.startup_provider;
    if provider != ui.active_provider {
        client_pub.send(crate::client::ClientRequest::SwitchProvider(provider))?;
    }
    send_startup_requests(client_pub, ui)?;
    leave_welcome_page(ui, from_settings);
    Ok(true)
}

fn skip_welcome_setup(
    client_pub: &crate::client::ClientRequestSender,
    ui: &mut UIStateGuard,
    from_settings: bool,
) -> Result<bool> {
    let provider_ready = ui.setup_state.ready_for(ui.setup_auth_snapshot());
    if !provider_ready {
        // Skipping authentication must not make the next launch select an
        // unavailable provider. The chosen provider can be revisited later.
        ui.setup_state.startup_provider = ui.active_provider;
    }
    ui.setup_state.status = config::SetupStatus::Skipped;
    ui.setup_state.failure = None;
    if !persist_setup_completion(ui) {
        return Ok(true);
    }
    if provider_ready && ui.setup_state.startup_provider != ui.active_provider {
        client_pub.send(crate::client::ClientRequest::SwitchProvider(
            ui.setup_state.startup_provider,
        ))?;
    } else if !provider_ready {
        ui.set_unsupported_operation(
            "Setup was skipped; the selected provider is not ready yet.",
            "Reopen first-use setup from Settings when you are ready.",
        );
    }
    send_startup_requests(client_pub, ui)?;
    leave_welcome_page(ui, from_settings);
    Ok(true)
}

fn leave_welcome_page(ui: &mut UIStateGuard, from_settings: bool) {
    if from_settings && ui.history.len() > 1 {
        ui.history.pop();
    } else if let Some(page) = ui.history.last_mut() {
        *page = PageState::Home {
            state: crate::state::HomePageUIState::default(),
        };
    }
    // History was edited in place, so `push_page` never re-derived workspace mode.
    ui.sync_workspace_after_history_change();
    ui.popup = None;
    ui.bump_diagnostic_revision();
}

fn send_startup_requests(
    client_pub: &crate::client::ClientRequestSender,
    ui: &UIStateGuard,
) -> Result<()> {
    if ui.spotify_auth_status.ready() {
        client_pub.send(crate::client::ClientRequest::GetCurrentUser)?;
        client_pub.send(crate::client::ClientRequest::GetUserPlaylists)?;
        client_pub.send(crate::client::ClientRequest::GetUserFollowedArtists)?;
        client_pub.send(crate::client::ClientRequest::GetUserSavedAlbums)?;
        client_pub.send(crate::client::ClientRequest::GetContext(
            crate::state::ContextId::Tracks(crate::state::USER_LIKED_TRACKS_ID.to_owned()),
        ))?;
        client_pub.send(crate::client::ClientRequest::GetUserSavedShows)?;
    }
    if ui.setup_state.startup_provider == config::ActiveProvider::YouTubeMusic
        && ui.setup_auth_snapshot().youtube.account_ready
    {
        client_pub.send(crate::client::ClientRequest::GetYouTubeLibrary)?;
    }
    Ok(())
}

fn handle_action_for_library_page(
    action: Action,
    client_pub: &crate::client::ClientRequestSender,
    ui: &mut UIStateGuard,
    state: &SharedState,
) -> Result<bool> {
    if ui.active_provider == config::ActiveProvider::YouTubeMusic {
        ui.set_unsupported_operation(
            YOUTUBE_LIBRARY_ACTIONS_UNAVAILABLE_MESSAGE,
            YOUTUBE_LIBRARY_ACTIONS_UNAVAILABLE_NEXT_ACTION,
        );
        return Ok(true);
    }

    let data = state.data.read();
    let (focus_state, folder_id) = match ui.current_page() {
        PageState::Library { state } => (state.focus, state.playlist_folder_id),
        _ => anyhow::bail!("expect a library page state"),
    };
    if focus_state == LibraryFocusState::Playlists {
        let selected = ui.current_page().selected_index().unwrap_or_default();
        let native_count = ui
            .search_filtered_items(&data.user_data.folder_playlists_items(folder_id))
            .len();
        if spotify_library_unified_index(selected, native_count, data.unified_playlists.len())
            .is_some()
        {
            ui.set_unsupported_operation(
                "Actions on Unified playlists are handled inside the playlist.",
                "Press Enter to open it, then select tracks and choose an action.",
            );
            return Ok(true);
        }
    }
    match focus_state {
        LibraryFocusState::Playlists => window::handle_action_for_selected_item(
            action,
            &ui.search_filtered_items(&data.user_data.folder_playlists_items(folder_id))
                .into_iter()
                .copied()
                .collect::<Vec<_>>(),
            &data,
            ui,
            client_pub,
        ),
        LibraryFocusState::SavedAlbums => window::handle_action_for_selected_item(
            action,
            &ui.search_filtered_items(&data.user_data.saved_albums),
            &data,
            ui,
            client_pub,
        ),
        LibraryFocusState::FollowedArtists => window::handle_action_for_selected_item(
            action,
            &ui.search_filtered_items(&data.user_data.followed_artists),
            &data,
            ui,
            client_pub,
        ),
    }
}

fn handle_command_for_library_page(
    command: Command,
    client_pub: &crate::client::ClientRequestSender,
    ui: &mut UIStateGuard,
    state: &SharedState,
) -> Result<bool> {
    if command == Command::Search {
        ui.new_search_popup();
        return Ok(true);
    }

    if ui.active_provider == config::ActiveProvider::YouTubeMusic {
        return handle_command_for_youtube_library_page(command, client_pub, ui, state);
    }

    let (focus_state, folder_id) = match ui.current_page() {
        PageState::Library { state } => (state.focus, state.playlist_folder_id),
        _ => anyhow::bail!("expect a library page state"),
    };

    if command == Command::SortLibraryAlphabetically {
        let mut data = state.data.write();

        // Sort playlists alphabetically, keeping folders on top
        data.user_data.playlists.sort_by(|a, b| match (a, b) {
            (PlaylistFolderItem::Folder(_), PlaylistFolderItem::Playlist(_)) => {
                std::cmp::Ordering::Less
            }
            (PlaylistFolderItem::Playlist(_), PlaylistFolderItem::Folder(_)) => {
                std::cmp::Ordering::Greater
            }
            _ => a
                .to_string()
                .to_lowercase()
                .cmp(&b.to_string().to_lowercase()),
        });

        // Sort albums alphabetically
        data.user_data
            .saved_albums
            .sort_by_key(|x| x.name.to_lowercase());

        // Sort artists alphabetically
        data.user_data
            .followed_artists
            .sort_by_key(|x| x.name.to_lowercase());
        sort_unified_playlists_alphabetically(&mut data.unified_playlists);
    }

    if command == Command::SortLibraryByRecent {
        let mut data = state.data.write();

        // Sort playlists by `current_folder_id` and then by `snapshot_id`
        data.user_data.playlists.sort_by(|a, b| {
            match (a, b) {
                (PlaylistFolderItem::Playlist(p1), PlaylistFolderItem::Playlist(p2)) => {
                    if p1.current_folder_id == p2.current_folder_id {
                        p1.snapshot_id.cmp(&p2.snapshot_id)
                    } else {
                        p1.current_folder_id.cmp(&p2.current_folder_id)
                    }
                }
                (PlaylistFolderItem::Folder(_), PlaylistFolderItem::Playlist(_)) => {
                    std::cmp::Ordering::Less
                }
                (PlaylistFolderItem::Playlist(_), PlaylistFolderItem::Folder(_)) => {
                    std::cmp::Ordering::Greater
                }
                _ => std::cmp::Ordering::Equal, // Keep folders in place
            }
        });

        // Sort albums by recent addition
        data.user_data
            .saved_albums
            .sort_by_key(|a| std::cmp::Reverse(a.added_at));
    }

    if focus_state == LibraryFocusState::Playlists && command == Command::ChooseSelected {
        let selected = ui.current_page().selected_index().unwrap_or_default();
        let unified_playlist_id = {
            let data = state.data.read();
            let native_count = ui
                .search_filtered_items(&data.user_data.folder_playlists_items(folder_id))
                .len();
            spotify_library_unified_index(selected, native_count, data.unified_playlists.len())
                .and_then(|index| data.unified_playlists.get(index))
                .map(|playlist| playlist.id.clone())
        };
        if let Some(id) = unified_playlist_id {
            ui.new_page(PageState::new_unified_playlist(id));
            return Ok(true);
        }
    }

    if focus_state == LibraryFocusState::Playlists {
        if command == Command::ShowActionsOnSelectedItem {
            let selected = ui.current_page().selected_index().unwrap_or_default();
            let data = state.data.read();
            let native_count = ui
                .search_filtered_items(&data.user_data.folder_playlists_items(folder_id))
                .len();
            if let Some(index) =
                spotify_library_unified_index(selected, native_count, data.unified_playlists.len())
            {
                let Some(playlist) = data.unified_playlists.get(index) else {
                    return Ok(false);
                };
                let link = data
                    .playlist_links
                    .iter()
                    .find(|link| link.unified_playlist_id == playlist.id);
                let action_model = unified_playlist_action_model_with_listenbrainz(
                    link.is_some_and(|link| link.youtube_playlist_id.is_some()),
                    true,
                );
                return Ok(open_unified_playlist_context_actions(
                    &playlist.id,
                    &playlist.name,
                    action_model.context_actions().iter().copied(),
                    ui,
                ));
            }
        }
        let (selected, total) = {
            let data = state.data.read();
            (
                ui.current_page().selected_index().unwrap_or_default(),
                ui.search_filtered_items(&data.user_data.folder_playlists_items(folder_id))
                    .len()
                    + data.unified_playlists.len(),
            )
        };
        let count = ui.count_prefix;
        if handle_navigation_command(command, ui.current_page_mut(), selected, total, count) {
            return Ok(true);
        }
    }

    match focus_state {
        LibraryFocusState::Playlists => {
            let data = state.data.read();
            Ok(window::handle_command_for_playlist_list_window(
                command,
                &ui.search_filtered_items(&data.user_data.folder_playlists_items(folder_id))
                    .into_iter()
                    .copied()
                    .collect::<Vec<_>>(),
                &data,
                ui,
            ))
        }
        LibraryFocusState::SavedAlbums => {
            // Use a read lock for the function call
            let data = state.data.read();
            window::handle_command_for_album_list_window(
                command,
                &ui.search_filtered_items(&data.user_data.saved_albums),
                &data,
                ui,
                client_pub,
            )
        }
        LibraryFocusState::FollowedArtists => {
            // Handle artist-specific commands
            let data = state.data.read();
            Ok(window::handle_command_for_artist_list_window(
                command,
                &ui.search_filtered_items(&data.user_data.followed_artists),
                &data,
                ui,
            ))
        }
    }
}

fn handle_command_for_youtube_library_page(
    command: Command,
    client_pub: &crate::client::ClientRequestSender,
    ui: &mut UIStateGuard,
    state: &SharedState,
) -> Result<bool> {
    let focus_state = match ui.current_page() {
        PageState::Library { state } => state.focus,
        _ => anyhow::bail!("expect a library page state"),
    };

    if command == Command::SortLibraryAlphabetically {
        let mut data = state.data.write();
        data.user_data
            .youtube_library
            .playlists
            .sort_by_key(|item| item.name.to_lowercase());
        data.user_data
            .youtube_library
            .albums
            .sort_by_key(|item| item.name.to_lowercase());
        data.user_data
            .youtube_library
            .artists
            .sort_by_key(|item| item.name.to_lowercase());
        sort_unified_playlists_alphabetically(&mut data.unified_playlists);
        return Ok(true);
    }

    if command == Command::SortLibraryByRecent {
        ui.set_unsupported_operation(
            "Recent sorting is unavailable for the YouTube Music library.",
            "Use alphabetical sorting or navigate the provider's current order.",
        );
        return Ok(true);
    }

    if command == Command::ShowActionsOnSelectedItem {
        if focus_state == LibraryFocusState::Playlists {
            let selected = ui.current_page().selected_index().unwrap_or_default();
            let data = state.data.read();
            let playlists = ui.search_filtered_items(&data.user_data.youtube_library.playlists);
            if let Some(index) = youtube_library_unified_index(
                selected,
                playlists.len(),
                data.unified_playlists.len(),
            ) {
                let Some(playlist) = data.unified_playlists.get(index) else {
                    return Ok(false);
                };
                let link = data
                    .playlist_links
                    .iter()
                    .find(|link| link.unified_playlist_id == playlist.id);
                let action_model = unified_playlist_action_model_with_listenbrainz(
                    link.is_some_and(|link| link.youtube_playlist_id.is_some()),
                    true,
                );
                return Ok(open_unified_playlist_context_actions(
                    &playlist.id,
                    &playlist.name,
                    action_model.context_actions().iter().copied(),
                    ui,
                ));
            }
            if let Some(index) = youtube_library_playlist_index(selected, playlists.len()) {
                let Some(playlist) = playlists.get(index) else {
                    return Ok(false);
                };
                let editable = youtube_playlist_is_editable(
                    state,
                    &YouTubeContextId::Playlist(playlist.id.clone()),
                );
                return Ok(open_youtube_playlist_context_actions(
                    &playlist.id,
                    &playlist.name,
                    editable,
                    ui,
                ));
            }
        }
        ui.set_unsupported_operation(
            YOUTUBE_LIBRARY_ACTIONS_UNAVAILABLE_MESSAGE,
            YOUTUBE_LIBRARY_ACTIONS_UNAVAILABLE_NEXT_ACTION,
        );
        return Ok(true);
    }

    if command == Command::ChooseSelected {
        let selected = ui.current_page().selected_index().unwrap_or_default();
        if focus_state == LibraryFocusState::Playlists {
            if selected == YOUTUBE_LIKED_LIBRARY_ROW {
                let context_id = YouTubeContextId::LikedTracks;
                ui.new_page(PageState::YouTubeContext {
                    id: context_id.clone(),
                    context: None,
                    state: YouTubeContextPageUIState::new(),
                });
                client_pub.send(ClientRequest::GetYouTubeContext(context_id))?;
                return Ok(true);
            }
            let unified_playlist_id = {
                let data = state.data.read();
                let native_count = ui
                    .search_filtered_items(&data.user_data.youtube_library.playlists)
                    .len();
                youtube_library_unified_index(selected, native_count, data.unified_playlists.len())
                    .and_then(|index| data.unified_playlists.get(index))
                    .map(|playlist| playlist.id.clone())
            };
            if let Some(id) = unified_playlist_id {
                ui.new_page(PageState::new_unified_playlist(id));
                return Ok(true);
            }
        }
        let context_id = {
            let data = state.data.read();
            match focus_state {
                LibraryFocusState::Playlists => {
                    let playlists =
                        ui.search_filtered_items(&data.user_data.youtube_library.playlists);
                    youtube_library_playlist_index(selected, playlists.len())
                        .and_then(|index| playlists.get(index))
                        .map(|item| YouTubeContextId::Playlist(item.id.clone()))
                }
                LibraryFocusState::SavedAlbums => ui
                    .search_filtered_items(&data.user_data.youtube_library.albums)
                    .get(selected)
                    .map(|item| YouTubeContextId::Album(item.id.clone())),
                LibraryFocusState::FollowedArtists => ui
                    .search_filtered_items(&data.user_data.youtube_library.artists)
                    .get(selected)
                    .map(|item| YouTubeContextId::Artist(item.id.clone())),
            }
        };

        if let Some(context_id) = context_id {
            ui.new_page(PageState::YouTubeContext {
                id: context_id.clone(),
                context: None,
                state: YouTubeContextPageUIState::new(),
            });
            client_pub.send(ClientRequest::GetYouTubeContext(context_id))?;
            return Ok(true);
        }
        return Ok(false);
    }

    let len = {
        let data = state.data.read();
        match focus_state {
            LibraryFocusState::Playlists => youtube_library_total_items(
                ui.search_filtered_items(&data.user_data.youtube_library.playlists)
                    .len(),
                data.unified_playlists.len(),
            ),
            LibraryFocusState::SavedAlbums => ui
                .search_filtered_items(&data.user_data.youtube_library.albums)
                .len(),
            LibraryFocusState::FollowedArtists => ui
                .search_filtered_items(&data.user_data.youtube_library.artists)
                .len(),
        }
    };
    let selected = ui.current_page().selected_index().unwrap_or_default();
    let count = ui.count_prefix;

    Ok(handle_navigation_command(
        command,
        ui.current_page_mut(),
        selected,
        len,
        count,
    ))
}

fn sort_unified_playlists_alphabetically(playlists: &mut [crate::state::UnifiedPlaylist]) {
    playlists.sort_by_key(|playlist| playlist.name.to_lowercase());
}

fn workspace_navigation_move(command: Command, ui: &mut UIStateGuard) -> bool {
    let item = ui.workspace_navigation;
    let next = match command {
        Command::SelectNextOrScrollDown | Command::PageSelectNextOrScrollDown => {
            item.next().unwrap_or(item)
        }
        Command::SelectPreviousOrScrollUp | Command::PageSelectPreviousOrScrollUp => {
            item.previous().unwrap_or(item)
        }
        Command::SelectFirstOrScrollToTop => WorkspaceNavigationItem::first(),
        Command::SelectLastOrScrollToBottom => WorkspaceNavigationItem::last(),
        _ => return false,
    };
    ui.workspace_navigation = next;
    ui.bump_diagnostic_revision();
    true
}

fn workspace_library_history_index(ui: &UIStateGuard) -> Option<usize> {
    ui.history
        .iter()
        .rposition(|page| matches!(page, PageState::Library { .. }))
}

fn return_to_home(ui: &mut UIStateGuard) -> bool {
    match ui
        .history
        .iter()
        .rposition(|page| matches!(page, PageState::Home { .. }))
    {
        Some(index) => {
            ui.history.truncate(index.saturating_add(1));
            ui.sync_workspace_after_history_change();
        }
        None => ui.new_page(PageState::Home {
            state: crate::state::HomePageUIState::default(),
        }),
    }
    ui.bump_diagnostic_revision();
    true
}

/// Show the Library page focused on `navigation`'s list.
pub(super) fn open_workspace_library(ui: &mut UIStateGuard, navigation: WorkspaceNavigationItem) {
    return_to_workspace_library(ui, navigation);
}

fn return_to_workspace_library(ui: &mut UIStateGuard, navigation: WorkspaceNavigationItem) -> bool {
    // Home is the history root, so Library is opened on demand.
    let index = workspace_library_history_index(ui).unwrap_or_else(|| {
        ui.new_page(PageState::Library {
            state: crate::state::LibraryPageUIState::new(),
        });
        ui.history.len() - 1
    });
    ui.history.truncate(index.saturating_add(1));
    let Some(PageState::Library { state }) = ui.history.get_mut(index) else {
        return false;
    };
    state.focus = match navigation {
        WorkspaceNavigationItem::Albums => LibraryFocusState::SavedAlbums,
        WorkspaceNavigationItem::Artists => LibraryFocusState::FollowedArtists,
        _ => LibraryFocusState::Playlists,
    };
    match state.focus {
        LibraryFocusState::Playlists => state.playlist_list.select(Some(0)),
        LibraryFocusState::SavedAlbums => state.saved_album_list.select(Some(0)),
        LibraryFocusState::FollowedArtists => state.followed_artist_list.select(Some(0)),
    }
    ui.workspace_navigation = navigation;
    ui.sync_workspace_after_history_change();
    ui.bump_diagnostic_revision();
    true
}

fn handle_workspace_navigation_activation(
    item: WorkspaceNavigationItem,
    client_pub: &crate::client::ClientRequestSender,
    state: &SharedState,
    ui: &mut UIStateGuard,
) -> Result<bool> {
    match item {
        WorkspaceNavigationItem::Home => Ok(return_to_home(ui)),
        WorkspaceNavigationItem::Playlists
        | WorkspaceNavigationItem::Albums
        | WorkspaceNavigationItem::Artists => Ok(return_to_workspace_library(ui, item)),
        WorkspaceNavigationItem::LikedMusic => match ui.active_provider {
            config::ActiveProvider::Spotify => {
                let context_id = ContextId::Tracks(crate::state::USER_LIKED_TRACKS_ID.to_owned());
                ui.new_page(PageState::Context {
                    id: None,
                    context_page_type: ContextPageType::Browsing(context_id.clone()),
                    state: None,
                });
                client_pub.send(ClientRequest::GetContext(context_id))?;
                Ok(true)
            }
            config::ActiveProvider::YouTubeMusic => {
                let context_id = YouTubeContextId::LikedTracks;
                ui.new_page(PageState::YouTubeContext {
                    id: context_id.clone(),
                    context: None,
                    state: YouTubeContextPageUIState::new(),
                });
                client_pub.send(ClientRequest::GetYouTubeContext(context_id))?;
                Ok(true)
            }
        },
        WorkspaceNavigationItem::Search => {
            if matches!(ui.current_page(), PageState::Search { .. }) {
                ui.workspace_navigation = item;
                ui.workspace_focus = WorkspaceFocusState::Context;
                return Ok(true);
            }
            ui.new_page(PageState::Search {
                line_input: LineInput::default(),
                current_query: String::new(),
                state: SearchPageUIState::new(),
            });
            Ok(true)
        }
        WorkspaceNavigationItem::Queue => {
            ui.new_page(PageState::new_queue());
            if let Some(refresh_guard) = state.player.read().native_queue_refresh_guard() {
                client_pub.send(ClientRequest::GetCurrentUserQueue(refresh_guard))?;
            }
            Ok(true)
        }
    }
}

pub(super) fn handle_workspace_navigation_command(
    command: Command,
    client_pub: &crate::client::ClientRequestSender,
    state: &SharedState,
    ui: &mut UIStateGuard,
) -> Result<bool> {
    if workspace_navigation_move(command, ui) {
        return Ok(true);
    }
    let scope = match command {
        Command::SwitchProvider => Some(WorkspaceScopeKind::Browsing),
        Command::OpenAccountSelector => Some(WorkspaceScopeKind::Account),
        Command::SwitchPlaybackProvider => Some(WorkspaceScopeKind::Playback),
        _ => None,
    };
    if let Some(scope) = scope {
        let Some(anchor) = ui.workspace_hit_rect(WorkspaceHit::Scope(scope)) else {
            return Ok(false);
        };
        return open_workspace_scope_popup(scope, anchor, state, ui);
    }
    match command {
        Command::ChooseSelected => {
            handle_workspace_navigation_activation(ui.workspace_navigation, client_pub, state, ui)
        }
        Command::Search => {
            ui.new_search_popup();
            Ok(true)
        }
        _ => Ok(false),
    }
}

fn workspace_scope_options(
    kind: WorkspaceScopeKind,
    state: &SharedState,
    ui: &UIStateGuard,
) -> Result<(Vec<WorkspaceScopeOption>, usize)> {
    let configs = config::get_config();
    match kind {
        WorkspaceScopeKind::Browsing | WorkspaceScopeKind::Playback => {
            let current = match kind {
                WorkspaceScopeKind::Browsing => ui.active_provider,
                WorkspaceScopeKind::Playback => state
                    .player
                    .read()
                    .effective_playback_provider(ui.active_provider),
                WorkspaceScopeKind::Account => unreachable!(),
            };
            let options = [
                config::ActiveProvider::Spotify,
                config::ActiveProvider::YouTubeMusic,
            ]
            .into_iter()
            .map(|provider| WorkspaceScopeOption {
                label: provider.title().to_owned(),
                selection: WorkspaceScopeSelection::Provider(provider),
            })
            .collect::<Vec<_>>();
            let selected = options
                .iter()
                .position(|option| {
                    matches!(
                        option.selection,
                        WorkspaceScopeSelection::Provider(provider) if provider == current
                    )
                })
                .unwrap_or_default();
            Ok((options, selected))
        }
        WorkspaceScopeKind::Account => {
            let provider = ui.active_provider;
            let registry = config::AccountRegistry::load(&configs.config_folder)
                .context("load account registry")?;
            let summaries = registry.summaries(
                provider,
                &configs.config_folder,
                &configs.cache_folder,
                &configs.youtube_music_cookie_path(),
            );
            let selected = summaries
                .iter()
                .position(|summary| summary.active)
                .unwrap_or_default();
            let options = summaries
                .into_iter()
                .map(|summary| WorkspaceScopeOption {
                    label: if summary.ready {
                        summary.label
                    } else {
                        format!("{} (not ready)", summary.label)
                    },
                    selection: WorkspaceScopeSelection::Account {
                        provider,
                        account_id: summary.id,
                    },
                })
                .collect::<Vec<_>>();
            Ok((options, selected))
        }
    }
}

fn open_workspace_scope_popup(
    kind: WorkspaceScopeKind,
    anchor: ratatui::layout::Rect,
    state: &SharedState,
    ui: &mut UIStateGuard,
) -> Result<bool> {
    let (options, selected) = workspace_scope_options(kind, state, ui)?;
    if options.is_empty() {
        let message = match kind {
            WorkspaceScopeKind::Browsing => "No browsing providers are available.",
            WorkspaceScopeKind::Account => "No saved accounts are available.",
            WorkspaceScopeKind::Playback => "No playback providers are available.",
        };
        ui.set_unsupported_operation(
            message,
            "Add or authenticate a provider account, then try again.",
        );
        return Ok(true);
    }
    let mut list = ratatui::widgets::ListState::default();
    list.select(Some(selected.min(options.len().saturating_sub(1))));
    ui.popup = Some(PopupState::WorkspaceScope {
        kind,
        options,
        state: list,
        anchor,
    });
    ui.workspace_focus = WorkspaceFocusState::Navigation;
    ui.bump_diagnostic_revision();
    Ok(true)
}

pub(super) fn choose_workspace_scope(
    index: usize,
    client_pub: &crate::client::ClientRequestSender,
    state: &SharedState,
    ui: &mut UIStateGuard,
) -> Result<bool> {
    let Some(PopupState::WorkspaceScope { kind, options, .. }) = ui.popup.as_ref() else {
        return Ok(false);
    };
    let kind = *kind;
    let Some(option) = options.get(index) else {
        return Ok(false);
    };
    let selection = option.selection.clone();
    match selection {
        WorkspaceScopeSelection::Provider(provider) => {
            let current = match kind {
                WorkspaceScopeKind::Browsing => ui.active_provider,
                WorkspaceScopeKind::Playback => state
                    .player
                    .read()
                    .effective_playback_provider(ui.active_provider),
                WorkspaceScopeKind::Account => return Ok(false),
            };
            if provider != current {
                let command = match kind {
                    WorkspaceScopeKind::Browsing => Command::SwitchProvider,
                    WorkspaceScopeKind::Playback => Command::SwitchPlaybackProvider,
                    WorkspaceScopeKind::Account => unreachable!(),
                };
                super::handle_provider_playback_command(command, client_pub, state, ui)?;
            }
        }
        WorkspaceScopeSelection::Account {
            provider,
            account_id,
        } => {
            client_pub.send(ClientRequest::ManageAccount(
                crate::client::AccountOperation::Switch {
                    provider,
                    account_id,
                },
            ))?;
        }
    }
    ui.popup = None;
    ui.bump_diagnostic_revision();
    Ok(true)
}

fn workspace_queue_move(command: Command, state: &SharedState, ui: &mut UIStateGuard) -> bool {
    let len = state.player.read().queue_display_items().len();
    if len == 0 {
        return false;
    }
    let selected = ui
        .workspace_queue_list
        .selected()
        .unwrap_or_default()
        .min(len - 1);
    let next = match command {
        Command::SelectNextOrScrollDown => selected.saturating_add(ui.count_prefix.unwrap_or(1)),
        Command::SelectPreviousOrScrollUp => selected.saturating_sub(ui.count_prefix.unwrap_or(1)),
        Command::PageSelectNextOrScrollDown => selected.saturating_add(
            ui.count_prefix.unwrap_or(1) * config::get_config().app_config.page_size_in_rows,
        ),
        Command::PageSelectPreviousOrScrollUp => selected.saturating_sub(
            ui.count_prefix.unwrap_or(1) * config::get_config().app_config.page_size_in_rows,
        ),
        Command::SelectFirstOrScrollToTop => 0,
        Command::SelectLastOrScrollToBottom => len - 1,
        _ => return false,
    };
    ui.workspace_queue_list.select(Some(next.min(len - 1)));
    ui.bump_diagnostic_revision();
    true
}

fn workspace_action_move(command: Command, ui: &mut UIStateGuard) -> bool {
    let next = match command {
        Command::SelectNextOrScrollDown => ui.workspace_action.next(),
        Command::SelectPreviousOrScrollUp => ui.workspace_action.previous(),
        Command::SelectFirstOrScrollToTop => Some(WorkspaceAction::ALL[0]),
        Command::SelectLastOrScrollToBottom => WorkspaceAction::ALL.last().copied(),
        _ => return false,
    };
    let Some(next) = next else {
        return true;
    };
    ui.workspace_action = next;
    ui.bump_diagnostic_revision();
    true
}

fn handle_workspace_context_command(
    command: Command,
    client_pub: &crate::client::ClientRequestSender,
    state: &SharedState,
    ui: &mut UIStateGuard,
) -> Result<bool> {
    match ui.current_page().page_type() {
        PageType::Context => handle_command_for_context_page(command, client_pub, ui, state),
        PageType::YouTubeContext => {
            handle_command_for_youtube_context_page(command, client_pub, ui, state)
        }
        _ => Ok(false),
    }
}

fn settings_workspace_sources(
    settings: &[config::AppConfigSetting],
    category: SettingsCategory,
    query: Option<&str>,
) -> Vec<usize> {
    settings_filter_projection(settings, query)
        .into_iter()
        .filter(|(_, setting)| category.includes(setting))
        .map(|(source, _)| source)
        .collect()
}

fn handle_settings_workspace_navigation_command(
    command: Command,
    ui: &mut crate::state::UIState,
) -> Result<bool> {
    let category = ui.workspace_settings_category;
    match command {
        Command::ClosePopup | Command::PreviousPage => Ok(leave_settings_workspace(ui)),
        Command::SelectNextOrScrollDown | Command::PageSelectNextOrScrollDown => {
            set_settings_workspace_category(ui, category.next());
            ui.bump_diagnostic_revision();
            Ok(true)
        }
        Command::SelectPreviousOrScrollUp | Command::PageSelectPreviousOrScrollUp => {
            set_settings_workspace_category(ui, category.previous());
            ui.bump_diagnostic_revision();
            Ok(true)
        }
        Command::SelectFirstOrScrollToTop => {
            set_settings_workspace_category(ui, SettingsCategory::Preferences);
            ui.bump_diagnostic_revision();
            Ok(true)
        }
        Command::SelectLastOrScrollToBottom => {
            set_settings_workspace_category(ui, SettingsCategory::Accounts);
            ui.bump_diagnostic_revision();
            Ok(true)
        }
        Command::ChooseSelected => {
            let query = match ui.popup.as_ref() {
                Some(PopupState::Search { query }) => Some(query.as_str()),
                _ => None,
            };
            let source = match ui.current_page() {
                PageState::Settings { settings, .. } => {
                    settings_workspace_sources(settings, category, query)
                        .first()
                        .copied()
                }
                _ => None,
            };
            if let Some(source) = source {
                ui.current_page_mut().select(source);
            }
            ui.workspace_focus = WorkspaceFocusState::Context;
            ui.bump_diagnostic_revision();
            Ok(true)
        }
        Command::Search => {
            ui.new_search_popup();
            Ok(true)
        }
        _ => Ok(false),
    }
}

fn leave_settings_workspace(ui: &mut crate::state::UIState) -> bool {
    if ui.history.len() > 1 {
        ui.history.pop();
        ui.popup = None;
        ui.sync_workspace_after_history_change();
    }
    true
}

fn set_settings_workspace_category(ui: &mut crate::state::UIState, category: SettingsCategory) {
    let query = match ui.popup.as_ref() {
        Some(PopupState::Search { query }) => Some(query.as_str()),
        _ => None,
    };
    let first_source = match ui.current_page() {
        PageState::Settings { settings, .. } => {
            settings_workspace_sources(settings, category, query)
                .first()
                .copied()
        }
        _ => None,
    };
    ui.workspace_settings_category = category;
    if let Some(source) = first_source {
        ui.current_page_mut().select(source);
    }
}

/// Move through the Settings tiles: up/down through grid rows and sections,
/// page up/down between sections, and first/last to the ends.
fn handle_settings_workspace_list_navigation(
    command: Command,
    ui: &mut crate::state::UIState,
) -> bool {
    let count = isize::try_from(ui.count_prefix.unwrap_or(1)).unwrap_or(1);
    let moved = match command {
        Command::SelectNextOrScrollDown => {
            move_settings_tiles(ui, |nav, sizes| nav.move_vertical(sizes, count))
        }
        Command::SelectPreviousOrScrollUp => {
            move_settings_tiles(ui, |nav, sizes| nav.move_vertical(sizes, -count))
        }
        Command::PageSelectNextOrScrollDown => {
            move_settings_tiles(ui, |nav, sizes| nav.move_between_shelves(sizes, count))
        }
        Command::PageSelectPreviousOrScrollUp => {
            move_settings_tiles(ui, |nav, sizes| nav.move_between_shelves(sizes, -count))
        }
        Command::SelectFirstOrScrollToTop => move_settings_tiles(ui, |nav, sizes| {
            let Some(first) = sizes.iter().find(|size| size.len > 0) else {
                return false;
            };
            nav.select(first.key, 0);
            true
        }),
        Command::SelectLastOrScrollToBottom => move_settings_tiles(ui, |nav, sizes| {
            let Some(last) = sizes.iter().rfind(|size| size.len > 0) else {
                return false;
            };
            nav.select(last.key, last.len - 1);
            true
        }),
        _ => return false,
    };
    if moved {
        ui.bump_diagnostic_revision();
    }
    true
}

/// Move within the focused Settings section; Left/Right and the horizontal
/// wheel use this.
pub(super) fn move_settings_tiles_horizontally(ui: &mut crate::state::UIState, delta: isize) {
    if move_settings_tiles(ui, |nav, sizes| nav.move_horizontal(sizes, delta)) {
        ui.bump_diagnostic_revision();
    }
}

fn move_settings_tiles(
    ui: &mut crate::state::UIState,
    update: impl FnOnce(
        &mut crate::state::ShelfNav<config::AppConfigSection>,
        &[crate::state::ShelfSize<config::AppConfigSection>],
    ) -> bool,
) -> bool {
    let query = match ui.popup.as_ref() {
        Some(PopupState::Search { query }) => Some(query.clone()),
        _ => None,
    };
    let category = ui.workspace_settings_category;
    ui.current_page_mut()
        .update_settings_tiles(category, query.as_deref(), update)
}

fn handle_settings_workspace_secondary_command(
    command: Command,
    ui: &mut crate::state::UIState,
) -> Result<bool> {
    let action = match command {
        Command::ClosePopup | Command::PreviousPage => return Ok(leave_settings_workspace(ui)),
        Command::SelectNextOrScrollDown | Command::PageSelectNextOrScrollDown => {
            ui.workspace_settings_action.next()
        }
        Command::SelectPreviousOrScrollUp | Command::PageSelectPreviousOrScrollUp => {
            ui.workspace_settings_action.previous()
        }
        Command::SelectFirstOrScrollToTop => SettingsWorkspaceAction::Apply,
        Command::SelectLastOrScrollToBottom => SettingsWorkspaceAction::Discard,
        Command::ChooseSelected | Command::ResumePause => {
            match ui.workspace_settings_action {
                SettingsWorkspaceAction::Apply => {
                    set_settings_message(ui, "No unsaved changes; settings are already applied.");
                }
                SettingsWorkspaceAction::Discard => {
                    set_settings_message(ui, "No unsaved changes to discard.");
                }
            }
            return Ok(true);
        }
        _ => return Ok(false),
    };
    ui.workspace_settings_action = action;
    ui.bump_diagnostic_revision();
    Ok(true)
}

fn open_workspace_queue_page(
    selected: Option<usize>,
    client_pub: &crate::client::ClientRequestSender,
    state: &SharedState,
    ui: &mut UIStateGuard,
) -> Result<bool> {
    let handled = handle_workspace_navigation_activation(
        WorkspaceNavigationItem::Queue,
        client_pub,
        state,
        ui,
    )?;
    if handled {
        if let Some(selected) = selected {
            ui.current_page_mut().select(selected);
        }
    }
    Ok(handled)
}

fn handle_workspace_secondary_command(
    command: Command,
    client_pub: &crate::client::ClientRequestSender,
    state: &SharedState,
    ui: &mut UIStateGuard,
) -> Result<bool> {
    if matches!(ui.current_page(), PageState::Settings { .. }) {
        return handle_settings_workspace_secondary_command(command, ui);
    }
    match ui.workspace_focus {
        WorkspaceFocusState::Queue => {
            if workspace_queue_move(command, state, ui) {
                return Ok(true);
            }
            match command {
                Command::ChooseSelected | Command::Queue | Command::ShowActionsOnSelectedItem => {
                    open_workspace_queue_page(
                        ui.workspace_queue_list.selected(),
                        client_pub,
                        state,
                        ui,
                    )
                }
                _ => Ok(false),
            }
        }
        WorkspaceFocusState::Actions => {
            if workspace_action_move(command, ui) {
                return Ok(true);
            }
            match command {
                Command::ChooseSelected => handle_workspace_context_command(
                    ui.workspace_action.command(),
                    client_pub,
                    state,
                    ui,
                ),
                _ => Ok(false),
            }
        }
        WorkspaceFocusState::Navigation | WorkspaceFocusState::Context => Ok(false),
    }
}

pub(super) fn handle_workspace_mouse_hit(
    hit: WorkspaceHit,
    activate: bool,
    column: u16,
    row: u16,
    client_pub: &crate::client::ClientRequestSender,
    state: &SharedState,
    ui: &mut UIStateGuard,
) -> Result<bool> {
    if ui.popup.is_some() {
        return Ok(false);
    }
    match hit {
        WorkspaceHit::HomeCard { shelf, index } => {
            super::home::handle_card_hit(shelf, index, activate, client_pub, state, ui)
        }
        WorkspaceHit::HomeListRow(index) => {
            super::home::handle_list_row_hit(index, activate, client_pub, state, ui)
        }
        WorkspaceHit::WelcomeStep(_) | WorkspaceHit::WelcomeAction(_) => {
            if !matches!(ui.current_page(), PageState::Welcome { .. }) {
                return Ok(false);
            }
            ui.count_prefix = None;
            if ui.click_welcome(hit, activate) {
                handle_command_for_welcome_page(Command::ChooseSelected, client_pub, ui)
            } else {
                Ok(true)
            }
        }
        // The open editor handles its own controls before page hits.
        WorkspaceHit::WelcomeEditorInput
        | WorkspaceHit::WelcomeEditorConfirm
        | WorkspaceHit::WelcomeEditorCancel => Ok(false),
        WorkspaceHit::CloseWindow => {
            if activate {
                // Keep the mouse control on the exact global command path
                // used by the Backspace keymap entry.
                super::handle_global_command(Command::PreviousPage, client_pub, state, ui)
            } else {
                Ok(true)
            }
        }
        WorkspaceHit::Help => {
            if activate {
                super::handle_global_command(Command::OpenCommandHelp, client_pub, state, ui)
            } else {
                Ok(true)
            }
        }
        WorkspaceHit::VolumeMenu => {
            if activate {
                if let Some(anchor) = ui.workspace_hit_rect(hit) {
                    super::open_volume_popup(ui, anchor);
                }
            }
            Ok(true)
        }
        WorkspaceHit::PlaybackOption(option) => {
            if !activate {
                return Ok(true);
            }
            let command = match option {
                WorkspacePlaybackOption::Shuffle => Command::Shuffle,
                WorkspacePlaybackOption::Repeat => Command::Repeat,
                WorkspacePlaybackOption::Volume => {
                    let Some(rect) = ui.workspace_hit_rect(hit) else {
                        return Ok(false);
                    };
                    let Some(volume) = workspace_volume_at(rect, column, row) else {
                        return Ok(false);
                    };
                    let playback_provider = state
                        .player
                        .read()
                        .effective_playback_provider(ui.active_provider);
                    super::change_playback_volume(client_pub, state, playback_provider, |_| {
                        volume
                    })?;
                    return Ok(true);
                }
            };
            super::handle_global_command(command, client_pub, state, ui)
        }
        WorkspaceHit::Navigation(item) => {
            ui.workspace_navigation = item;
            ui.workspace_focus = WorkspaceFocusState::Navigation;
            if activate {
                handle_workspace_navigation_activation(item, client_pub, state, ui)
            } else {
                Ok(true)
            }
        }
        WorkspaceHit::Scope(kind) => {
            if !activate {
                ui.workspace_focus = WorkspaceFocusState::Navigation;
                ui.bump_diagnostic_revision();
                return Ok(true);
            }
            let Some(anchor) = ui.workspace_hit_rect(hit) else {
                return Ok(false);
            };
            open_workspace_scope_popup(kind, anchor, state, ui)
        }
        WorkspaceHit::SettingsRail(item) => match item {
            SettingsRailItem::Category(category) => {
                ui.workspace_settings_category = category;
                if let PageState::Settings { settings, .. } = ui.current_page() {
                    if let Some(source) = settings
                        .iter()
                        .position(|setting| category.includes(setting))
                    {
                        ui.current_page_mut().select(source);
                    }
                }
                ui.workspace_focus = WorkspaceFocusState::Navigation;
                if activate {
                    ui.workspace_focus = WorkspaceFocusState::Context;
                }
                ui.bump_diagnostic_revision();
                Ok(true)
            }
            SettingsRailItem::BackToPlayer => {
                if activate && ui.history.len() > 1 {
                    ui.history.pop();
                    ui.popup = None;
                    ui.sync_workspace_after_history_change();
                } else {
                    ui.workspace_focus = WorkspaceFocusState::Navigation;
                }
                Ok(true)
            }
        },
        WorkspaceHit::SettingsRow(source) => {
            if !matches!(ui.current_page(), PageState::Settings { .. }) {
                return Ok(false);
            }
            ui.current_page_mut().select(source);
            ui.workspace_focus = WorkspaceFocusState::Context;
            if activate {
                handle_command_for_settings_page(Command::ChooseSelected, client_pub, state, ui)
            } else {
                Ok(true)
            }
        }
        WorkspaceHit::SettingsAction(action) => {
            if !matches!(ui.current_page(), PageState::Settings { .. }) {
                return Ok(false);
            }
            ui.workspace_settings_action = action;
            ui.workspace_focus = WorkspaceFocusState::Actions;
            if activate {
                handle_settings_workspace_secondary_command(Command::ChooseSelected, ui)
            } else {
                Ok(true)
            }
        }
        WorkspaceHit::SearchCategory(category) => {
            if !matches!(ui.current_page(), PageState::Search { .. }) {
                return Ok(false);
            }
            if let PageState::Search { state, .. } = ui.current_page_mut() {
                state.category = category;
                state.focus = SearchFocusState::Category;
                if activate {
                    state.focus = category.map_or(
                        SearchFocusState::Input,
                        SearchFocusState::from_provider_pane,
                    );
                }
            }
            ui.workspace_focus = WorkspaceFocusState::Context;
            ui.bump_diagnostic_revision();
            Ok(true)
        }
        WorkspaceHit::SearchInput => {
            if !matches!(ui.current_page(), PageState::Search { .. }) {
                return Ok(false);
            }
            if let PageState::Search { state, .. } = ui.current_page_mut() {
                state.focus = SearchFocusState::Input;
            }
            ui.workspace_focus = WorkspaceFocusState::Context;
            Ok(true)
        }
        WorkspaceHit::SearchRow { focus, index } => {
            if !matches!(ui.current_page(), PageState::Search { .. }) {
                return Ok(false);
            }
            if let PageState::Search { state, .. } = ui.current_page_mut() {
                state.focus = focus;
            }
            ui.current_page_mut().select(index);
            ui.workspace_focus = WorkspaceFocusState::Context;
            if !activate {
                return Ok(true);
            }
            handle_key_sequence_for_search_page(
                &KeySequence {
                    keys: vec![Key::None(KeyCode::Enter)],
                },
                client_pub,
                state,
                ui,
            )
        }
        WorkspaceHit::BrowseRow(index) => {
            if !matches!(ui.current_page(), PageState::Browse { .. }) {
                return Ok(false);
            }
            ui.current_page_mut().select(index);
            ui.workspace_focus = WorkspaceFocusState::Context;
            if activate {
                handle_command_for_browse_page(Command::ChooseSelected, client_pub, ui, state)
            } else {
                Ok(true)
            }
        }
        WorkspaceHit::UnifiedPlaylistRow(index) => {
            if !matches!(ui.current_page(), PageState::UnifiedPlaylist { .. }) {
                return Ok(false);
            }
            ui.current_page_mut().select(index);
            ui.workspace_focus = WorkspaceFocusState::Context;
            if activate {
                handle_command_for_unified_playlist_page(
                    Command::ChooseSelected,
                    client_pub,
                    ui,
                    state,
                )
            } else {
                Ok(true)
            }
        }
        WorkspaceHit::LibraryRow { focus, index } => {
            let PageState::Library { state: library } = ui.current_page_mut() else {
                return Ok(false);
            };
            library.focus = focus;
            ui.current_page_mut().select(index);
            ui.workspace_focus = WorkspaceFocusState::Context;
            if !activate {
                return Ok(true);
            }
            handle_command_for_library_page(Command::ChooseSelected, client_pub, ui, state)
        }
        WorkspaceHit::ArtistRow { focus, index } => {
            let PageState::Context {
                state:
                    Some(ContextPageUIState::Artist {
                        focus: current,
                        listenbrainz_pending,
                        listenbrainz_album_pending,
                        ..
                    }),
                ..
            } = ui.current_page_mut()
            else {
                return Ok(false);
            };
            if *current != focus {
                // Same reset as keyboard focus changes (`Focusable::next`).
                *current = focus;
                *listenbrainz_pending = None;
                *listenbrainz_album_pending = None;
            }
            ui.workspace_focus = WorkspaceFocusState::Context;
            ui.current_page_mut().select(index);
            if !activate {
                return Ok(true);
            }
            handle_command_for_context_page(Command::ChooseSelected, client_pub, ui, state)
        }
        WorkspaceHit::ContextRow(index) => {
            if !ui.workspace_context_is_active() {
                return Ok(false);
            }
            ui.workspace_focus = WorkspaceFocusState::Context;
            ui.current_page_mut().select(index);
            if !activate {
                return Ok(true);
            }
            match ui.current_page().page_type() {
                PageType::Context => {
                    handle_command_for_context_page(Command::ChooseSelected, client_pub, ui, state)
                }
                PageType::YouTubeContext => handle_command_for_youtube_context_page(
                    Command::ChooseSelected,
                    client_pub,
                    ui,
                    state,
                ),
                _ => Ok(false),
            }
        }
        WorkspaceHit::QueueRow(index) => {
            let queue_page = matches!(ui.current_page(), PageState::Queue { .. });
            if !ui.workspace_layout.show_right && !queue_page {
                return Ok(false);
            }
            ui.workspace_queue_list.select(Some(index));
            if queue_page {
                ui.current_page_mut().select(index);
            }
            ui.workspace_focus = WorkspaceFocusState::Queue;
            if activate && !queue_page {
                open_workspace_queue_page(Some(index), client_pub, state, ui)
            } else {
                Ok(true)
            }
        }
        WorkspaceHit::JournalRow(index) => {
            if !matches!(
                ui.current_page(),
                PageState::Journal { .. }
                    | PageState::JournalLists { .. }
                    | PageState::JournalList { .. }
            ) {
                return Ok(false);
            }
            ui.current_page_mut().select(index);
            ui.workspace_focus = WorkspaceFocusState::Context;
            if !activate {
                return Ok(true);
            }
            match ui.current_page().page_type() {
                PageType::Journal => {
                    handle_command_for_journal_page(Command::ChooseSelected, client_pub, ui, state)
                }
                PageType::JournalLists => {
                    handle_command_for_journal_lists_page(Command::ChooseSelected, ui, state)
                }
                PageType::JournalList => handle_command_for_journal_list_page(
                    Command::ChooseSelected,
                    client_pub,
                    ui,
                    state,
                ),
                _ => Ok(false),
            }
        }
        WorkspaceHit::SessionHistoryRow(index) => {
            if !matches!(ui.current_page(), PageState::SessionHistory { .. }) {
                return Ok(false);
            }
            ui.current_page_mut().select(index);
            ui.workspace_focus = WorkspaceFocusState::Context;
            if !activate {
                return Ok(true);
            }
            super::popup::handle_session_history_command(Command::ChooseSelected, state, ui)
        }
        WorkspaceHit::DiagnosticRow(index) => {
            if !matches!(ui.current_page(), PageState::Logs { .. }) {
                return Ok(false);
            }
            let Some(rows) = diagnostic_rows_for_logs_page(state, ui) else {
                return Ok(false);
            };
            let PageState::Logs { state: page } = ui.current_page_mut() else {
                return Ok(false);
            };
            page.select_index(&rows, index);
            ui.workspace_focus = WorkspaceFocusState::Context;
            if activate {
                Ok(handle_command_for_logs_page(
                    Command::ShowActionsOnSelectedItem,
                    state,
                    ui,
                ))
            } else {
                Ok(true)
            }
        }
        WorkspaceHit::Action(action) => {
            if !ui.workspace_layout.show_right {
                return Ok(false);
            }
            ui.workspace_action = action;
            ui.workspace_focus = WorkspaceFocusState::Actions;
            if activate {
                handle_workspace_context_command(action.command(), client_pub, state, ui)
            } else {
                Ok(true)
            }
        }
    }
}

/// Map a pointer inside the rendered volume bar to an absolute percentage.
/// Bounds are checked before deriving the coordinate payload so rejected
/// terminal hits cannot underflow or index outside the bar.
pub(super) fn workspace_volume_at(rect: Rect, column: u16, row: u16) -> Option<u8> {
    if rect.width == 0 || rect.height == 0 || column < rect.x || row < rect.y {
        return None;
    }
    let right = rect.x.saturating_add(rect.width);
    let bottom = rect.y.saturating_add(rect.height);
    if column >= right || row >= bottom {
        return None;
    }

    let offset = column - rect.x;
    let span = rect.width.saturating_sub(1);
    if span == 0 {
        return Some(0);
    }
    Some((u32::from(offset.min(span)) * 100 / u32::from(span)) as u8)
}

fn anchor_workspace_action_popup(ui: &mut UIStateGuard, anchor: Rect) -> bool {
    let Some(PopupState::ActionList(item, state)) = ui.popup.take() else {
        return false;
    };
    ui.popup = Some(PopupState::AnchoredActionList {
        item,
        state,
        anchor,
    });
    true
}

fn handle_workspace_search_context_menu(
    focus: SearchFocusState,
    client_pub: &crate::client::ClientRequestSender,
    state: &SharedState,
    ui: &mut UIStateGuard,
) -> Result<bool> {
    let (query, provider) = match ui.current_page() {
        PageState::Search {
            current_query,
            state,
            ..
        } => (
            current_query.clone(),
            state.provider.unwrap_or(ui.active_provider),
        ),
        _ => return Ok(false),
    };
    if provider == config::ActiveProvider::YouTubeMusic {
        return handle_key_sequence_for_youtube_search_page(
            CommandOrAction::Command(Command::ShowActionsOnSelectedItem),
            client_pub,
            &query,
            state,
            ui,
        );
    }

    let data = state.data.read();
    let Some(results) = data.caches.search.get(&query) else {
        return Ok(false);
    };
    match focus {
        SearchFocusState::Tracks => {
            let tracks = results.tracks.iter().collect::<Vec<_>>();
            window::handle_command_for_track_list_window(
                Command::ShowActionsOnSelectedItem,
                client_pub,
                &tracks,
                &data,
                ui,
                state,
            )
        }
        SearchFocusState::Albums => {
            let albums = results.albums.iter().collect::<Vec<_>>();
            window::handle_command_for_album_list_window(
                Command::ShowActionsOnSelectedItem,
                &albums,
                &data,
                ui,
                client_pub,
            )
        }
        SearchFocusState::Artists => {
            let artists = results.artists.iter().collect::<Vec<_>>();
            Ok(window::handle_command_for_artist_list_window(
                Command::ShowActionsOnSelectedItem,
                &artists,
                &data,
                ui,
            ))
        }
        SearchFocusState::Playlists => {
            let playlists = results
                .playlists
                .iter()
                .map(|playlist| PlaylistFolderItem::Playlist(playlist.clone()))
                .collect::<Vec<_>>();
            let playlist_refs = playlists.iter().collect::<Vec<_>>();
            Ok(window::handle_command_for_playlist_list_window(
                Command::ShowActionsOnSelectedItem,
                &playlist_refs,
                &data,
                ui,
            ))
        }
        SearchFocusState::Shows => {
            let shows = results.shows.iter().collect::<Vec<_>>();
            Ok(window::handle_command_for_show_list_window(
                Command::ShowActionsOnSelectedItem,
                &shows,
                &data,
                ui,
            ))
        }
        SearchFocusState::Episodes => {
            let episodes = results.episodes.iter().collect::<Vec<_>>();
            window::handle_command_for_episode_list_window(
                Command::ShowActionsOnSelectedItem,
                client_pub,
                &episodes,
                &data,
                ui,
                state,
            )
        }
        SearchFocusState::Videos | SearchFocusState::Category | SearchFocusState::Input => {
            Ok(false)
        }
    }
}

fn open_workspace_queue_context_menu(
    index: usize,
    anchor: Rect,
    state: &SharedState,
    ui: &mut UIStateGuard,
) -> Result<bool> {
    let Some(item) = state
        .player
        .read()
        .queue_display_items()
        .get(index)
        .cloned()
    else {
        return Ok(false);
    };
    let data = state.data.read();
    let Some(action_item) = queue_action_list_item(&item, &data, ui.active_provider) else {
        ui.set_unsupported_operation(
            "Queue item actions are unavailable.",
            "Switch provider or open the item from its source page.",
        );
        return Ok(true);
    };
    ui.popup = Some(PopupState::AnchoredActionList {
        item: Box::new(action_item),
        state: ListState::default(),
        anchor,
    });
    Ok(true)
}

/// Select the row under a right click and reuse the existing action builders.
/// The resulting action list is converted to an anchored overlay after the
/// page-specific handler has populated it.
pub(super) fn handle_workspace_context_menu_hit(
    hit: WorkspaceHit,
    anchor: Rect,
    client_pub: &crate::client::ClientRequestSender,
    state: &SharedState,
    ui: &mut UIStateGuard,
) -> Result<bool> {
    if ui.popup.is_some() {
        return Ok(false);
    }

    match hit {
        WorkspaceHit::LibraryRow { .. }
        | WorkspaceHit::ContextRow(_)
        | WorkspaceHit::ArtistRow { .. }
        | WorkspaceHit::SearchRow { .. }
        | WorkspaceHit::UnifiedPlaylistRow(_)
        | WorkspaceHit::JournalRow(_)
        | WorkspaceHit::SessionHistoryRow(_)
        | WorkspaceHit::DiagnosticRow(_) => {
            if !handle_workspace_mouse_hit(hit, false, 0, 0, client_pub, state, ui)? {
                return Ok(false);
            }
            let opened = match hit {
                WorkspaceHit::LibraryRow { .. } => handle_command_for_library_page(
                    Command::ShowActionsOnSelectedItem,
                    client_pub,
                    ui,
                    state,
                )?,
                WorkspaceHit::ArtistRow { .. } => handle_command_for_context_page(
                    Command::ShowActionsOnSelectedItem,
                    client_pub,
                    ui,
                    state,
                )?,
                WorkspaceHit::ContextRow(_) => match ui.current_page().page_type() {
                    PageType::Context => handle_command_for_context_page(
                        Command::ShowActionsOnSelectedItem,
                        client_pub,
                        ui,
                        state,
                    )?,
                    PageType::YouTubeContext => handle_command_for_youtube_context_page(
                        Command::ShowActionsOnSelectedItem,
                        client_pub,
                        ui,
                        state,
                    )?,
                    _ => false,
                },
                WorkspaceHit::SearchRow { focus, .. } => {
                    handle_workspace_search_context_menu(focus, client_pub, state, ui)?
                }
                WorkspaceHit::UnifiedPlaylistRow(_) => handle_command_for_unified_playlist_page(
                    Command::ShowActionsOnSelectedItem,
                    client_pub,
                    ui,
                    state,
                )?,
                WorkspaceHit::JournalRow(_) => match ui.current_page().page_type() {
                    PageType::Journal => handle_command_for_journal_page(
                        Command::ShowActionsOnSelectedItem,
                        client_pub,
                        ui,
                        state,
                    )?,
                    PageType::JournalList => handle_command_for_journal_list_page(
                        Command::ShowActionsOnSelectedItem,
                        client_pub,
                        ui,
                        state,
                    )?,
                    _ => false,
                },
                WorkspaceHit::SessionHistoryRow(_) => super::popup::handle_session_history_command(
                    Command::ShowActionsOnSelectedItem,
                    state,
                    ui,
                )?,
                WorkspaceHit::DiagnosticRow(_) => {
                    handle_command_for_logs_page(Command::ShowActionsOnSelectedItem, state, ui)
                }
                _ => false,
            };
            if opened {
                anchor_workspace_action_popup(ui, anchor);
            }
            Ok(true)
        }
        WorkspaceHit::QueueRow(index) => {
            let queue_page = matches!(ui.current_page(), PageState::Queue { .. });
            if !ui.workspace_layout.show_right && !queue_page {
                return Ok(false);
            }
            ui.workspace_queue_list.select(Some(index));
            if queue_page {
                ui.current_page_mut().select(index);
            }
            ui.workspace_focus = WorkspaceFocusState::Queue;
            open_workspace_queue_context_menu(index, anchor, state, ui)
        }
        _ => Ok(false),
    }
}

fn spotify_library_unified_index(
    selected: usize,
    native_count: usize,
    unified_count: usize,
) -> Option<usize> {
    selected
        .checked_sub(native_count)
        .filter(|index| *index < unified_count)
}

const YOUTUBE_LIKED_LIBRARY_ROW: usize = 0;
const YOUTUBE_PLAYLIST_ROW_OFFSET: usize = 1;

fn youtube_library_playlist_index(selected: usize, playlist_count: usize) -> Option<usize> {
    selected
        .checked_sub(YOUTUBE_PLAYLIST_ROW_OFFSET)
        .filter(|index| *index < playlist_count)
}

fn youtube_library_unified_index(
    selected: usize,
    playlist_count: usize,
    unified_count: usize,
) -> Option<usize> {
    selected
        .checked_sub(YOUTUBE_PLAYLIST_ROW_OFFSET + playlist_count)
        .filter(|index| *index < unified_count)
}

fn youtube_library_total_items(playlist_count: usize, unified_count: usize) -> usize {
    YOUTUBE_PLAYLIST_ROW_OFFSET + playlist_count + unified_count
}

fn synchronize_youtube_playlist_projection(
    ui: &mut UIStateGuard,
    context_id: &YouTubeContextId,
    context: &YouTubeContext,
    editable: bool,
) -> Option<(PlaylistSnapshot, crate::state::MutablePlaylistProjection)> {
    let provider_epoch = ui.provider_selection_epoch(config::ActiveProvider::YouTubeMusic);
    let snapshot = PlaylistSnapshot::from_youtube_playlist(
        context_id,
        context,
        provider_epoch,
        crate::state::UiViewStatus::Ready,
        PlaylistCapabilities::youtube_music(editable),
        PlaylistActionModel::default(),
        |_| crate::command::construct_youtube_track_actions(),
    )?;
    let filter = match ui.popup.as_ref() {
        Some(PopupState::Search { query }) => Some(query.clone()),
        _ => None,
    };
    let projection = ui
        .current_page_mut()
        .mutable_playlist_state_mut()
        .and_then(|state| {
            MutablePlaylistController::synchronize(state, &snapshot, filter.as_deref()).ok()
        })?;
    Some((snapshot, projection))
}

fn synchronize_youtube_context_page_selection(
    ui: &mut UIStateGuard,
    context_id: &YouTubeContextId,
    tracks: &[YouTubeTrack],
) -> bool {
    let provider_epoch = ui.provider_selection_epoch(config::ActiveProvider::YouTubeMusic);
    let query = ui.search_query().map(str::to_owned);
    let result = ui
        .current_page_mut()
        .youtube_context_track_selection_mut()
        .map(|selection| {
            crate::state::synchronize_filtered_youtube_context_tracks(
                selection,
                provider_epoch,
                context_id,
                tracks,
                query.as_deref(),
            )
        });

    match result {
        Some(Ok(())) => true,
        Some(Err(error)) => {
            tracing::warn!(
                selection_error = ?error,
                "YouTube context selection projection could not be synchronized"
            );
            if let Some(selection) = ui.current_page_mut().youtube_context_track_selection_mut() {
                selection.clear();
            }
            false
        }
        None => false,
    }
}

fn youtube_playlist_is_editable(state: &SharedState, context_id: &YouTubeContextId) -> bool {
    let YouTubeContextId::Playlist(playlist_id) = context_id else {
        return false;
    };
    state
        .data
        .read()
        .user_data
        .youtube_library
        .playlists
        .iter()
        .any(|playlist| crate::state::youtube_playlist_ids_match(&playlist.id, playlist_id))
}

fn journal_filter_query(ui: &UIStateGuard) -> Option<String> {
    match ui.popup.as_ref() {
        Some(PopupState::Search { query }) => Some(query.clone()),
        _ => None,
    }
}

fn synchronize_journal_page_selection(
    ui: &mut UIStateGuard,
    complete: &[TrackJournalEntry],
    visible: &[TrackJournalEntry],
) -> bool {
    let provider_epoch = ui.provider_selection_epoch(config::ActiveProvider::Spotify);
    let filter_query = journal_filter_query(ui);
    let result = ui
        .current_page_mut()
        .journal_selection_mut()
        .map(|selection| {
            synchronize_journal_uris(
                selection,
                JournalSelectionScope::journal(provider_epoch),
                filter_query.as_deref(),
                complete.iter().map(|entry| entry.track.id.uri()),
                visible.iter().map(|entry| entry.track.id.uri()),
            )
        });

    match result {
        Some(Ok(())) => true,
        Some(Err(error)) => {
            tracing::warn!(
                selection_error = ?error,
                "Journal selection projection could not be synchronized"
            );
            if let Some(selection) = ui.current_page_mut().journal_selection_mut() {
                selection.clear();
            }
            false
        }
        None => false,
    }
}

fn synchronize_journal_list_page_selection(
    ui: &mut UIStateGuard,
    list_id: &str,
    complete_uris: &[String],
    visible: &[TrackJournalEntry],
) -> bool {
    let provider_epoch = ui.provider_selection_epoch(config::ActiveProvider::Spotify);
    let filter_query = journal_filter_query(ui);
    let result = ui
        .current_page_mut()
        .journal_list_selection_mut()
        .map(|selection| {
            synchronize_journal_uris(
                selection,
                JournalSelectionScope::journal_list(provider_epoch, list_id),
                filter_query.as_deref(),
                complete_uris.iter(),
                visible.iter().map(|entry| entry.track.id.uri()),
            )
        });

    match result {
        Some(Ok(())) => true,
        Some(Err(error)) => {
            tracing::warn!(
                selection_error = ?error,
                "Journal list selection projection could not be synchronized"
            );
            if let Some(selection) = ui.current_page_mut().journal_list_selection_mut() {
                selection.clear();
            }
            false
        }
        None => false,
    }
}

fn journal_projection(
    ui: &mut UIStateGuard,
    state: &SharedState,
) -> Option<(Vec<TrackJournalEntry>, Vec<TrackJournalEntry>)> {
    let complete = state.data.read().journal.entries_sorted();
    let visible = ui
        .search_filtered_items(&complete)
        .into_iter()
        .cloned()
        .collect::<Vec<_>>();
    synchronize_journal_page_selection(ui, &complete, &visible).then_some((complete, visible))
}

fn journal_list_projection(
    ui: &mut UIStateGuard,
    state: &SharedState,
) -> Option<(
    String,
    Vec<String>,
    Vec<TrackJournalEntry>,
    Vec<TrackJournalEntry>,
)> {
    let list_id = match ui.current_page() {
        PageState::JournalList { list_id, .. } => list_id.clone(),
        _ => return None,
    };
    let (complete_uris, entries) = {
        let data = state.data.read();
        let Some(list) = data.journal.list(&list_id) else {
            if let Some(selection) = ui.current_page_mut().journal_list_selection_mut() {
                selection.clear();
            }
            return None;
        };
        (list.track_uris.clone(), data.journal.list_entries(&list_id))
    };
    let visible = ui
        .search_filtered_items(&entries)
        .into_iter()
        .cloned()
        .collect::<Vec<_>>();
    synchronize_journal_list_page_selection(ui, &list_id, &complete_uris, &visible).then_some((
        list_id,
        complete_uris,
        entries,
        visible,
    ))
}

fn selected_journal_tracks_for_action(
    ui: &UIStateGuard,
    visible: &[TrackJournalEntry],
) -> Vec<Track> {
    let cursor = ui.current_page().diagnostic_selection().unwrap_or_default();
    let indices = match ui.current_page() {
        PageState::Journal {
            journal_selection, ..
        }
        | PageState::JournalList {
            journal_selection, ..
        } => journal_selected_or_cursor_indices(journal_selection, cursor).unwrap_or_default(),
        _ => Vec::new(),
    };
    indices
        .into_iter()
        .filter_map(|index| visible.get(index).map(|entry| entry.track.clone()))
        .collect()
}

fn journal_list_move_target(
    selection: &JournalSelection,
    complete_uris: &[String],
    visible_index: usize,
) -> Option<(usize, String)> {
    if selection.status() != ScopedSelectionStatus::Ready {
        return None;
    }
    let full_index = selection.visible_to_full_index(visible_index).ok()?;
    Some((full_index, complete_uris.get(full_index)?.clone()))
}

fn journal_list_cursor_for_uri(visible_uris: &[String], target_uri: &str) -> Option<usize> {
    visible_uris.iter().position(|uri| uri == target_uri)
}

fn journal_list_target_is_current(
    current_uris: &[String],
    captured_uris: &[String],
    full_index: usize,
    target_uri: &str,
) -> bool {
    current_uris == captured_uris
        && current_uris
            .get(full_index)
            .is_some_and(|uri| uri == target_uri)
}

fn journal_list_uri_target_is_current(
    current_uris: &[String],
    captured_uris: &[String],
    target_uri: &str,
) -> bool {
    current_uris == captured_uris
        && current_uris.iter().filter(|uri| *uri == target_uri).count() == 1
}

fn journal_entries_snapshot_is_current(current_uris: &[String], captured_uris: &[String]) -> bool {
    current_uris == captured_uris
}

fn journal_uri_target_is_safe(
    selection: &JournalSelection,
    complete_uris: &[String],
    visible_uris: &[String],
    target_uri: &str,
) -> bool {
    selection.status() == ScopedSelectionStatus::Ready
        && complete_uris
            .iter()
            .filter(|uri| *uri == target_uri)
            .count()
            == 1
        && visible_uris.iter().filter(|uri| *uri == target_uri).count() == 1
}

fn journal_single_target_index(
    selection: &JournalSelection,
    visible_len: usize,
    cursor: usize,
) -> Option<usize> {
    let indices = journal_selected_or_cursor_indices(selection, cursor).ok()?;
    (indices.len() == 1)
        .then(|| indices[0])
        .filter(|index| *index < visible_len)
}

fn journal_single_track_for_action(
    ui: &UIStateGuard,
    visible: &[TrackJournalEntry],
) -> Option<Track> {
    let cursor = ui.current_page().diagnostic_selection().unwrap_or_default();
    let (PageState::Journal {
        journal_selection: selection,
        ..
    }
    | PageState::JournalList {
        journal_selection: selection,
        ..
    }) = ui.current_page()
    else {
        return None;
    };
    let index = journal_single_target_index(selection, visible.len(), cursor)?;
    visible.get(index).map(|entry| entry.track.clone())
}

/// Remove a Journal URI only after the current page projection proves that the
/// captured row is a unique, current target. Popup actions use this helper too
/// because their captured track can outlive the page projection that created it.
pub(super) fn handle_safe_journal_destructive_action(
    action: Action,
    track: Track,
    state: &SharedState,
    ui: &mut UIStateGuard,
) -> Result<bool> {
    let target_uri = track.id.uri();
    match action {
        Action::RemoveFromJournal => {
            if matches!(ui.current_page(), PageState::Journal { .. }) {
                let Some((complete, visible)) = journal_projection(ui, state) else {
                    return Ok(false);
                };
                let Some(selection) = ui.current_page().journal_selection() else {
                    return Ok(false);
                };
                let complete_uris = complete
                    .iter()
                    .map(|entry| entry.track.id.uri())
                    .collect::<Vec<_>>();
                let visible_uris = visible
                    .iter()
                    .map(|entry| entry.track.id.uri())
                    .collect::<Vec<_>>();
                if !journal_uri_target_is_safe(
                    selection,
                    &complete_uris,
                    &visible_uris,
                    &target_uri,
                ) {
                    return Ok(false);
                }
                let mut captured_entry_uris = complete_uris.clone();
                captured_entry_uris.sort();

                let mut removed = false;
                update_track_journal(state, |journal| {
                    let mut current_entry_uris = journal
                        .entries
                        .values()
                        .map(|entry| entry.track.id.uri())
                        .collect::<Vec<_>>();
                    current_entry_uris.sort();
                    if !journal_entries_snapshot_is_current(
                        &current_entry_uris,
                        &captured_entry_uris,
                    ) {
                        return;
                    }
                    let Some(current_track) = journal
                        .entries
                        .get(&target_uri)
                        .map(|entry| entry.track.clone())
                    else {
                        return;
                    };
                    if current_track.id.uri() != target_uri {
                        return;
                    }
                    journal.remove_track(&current_track);
                    removed = true;
                })?;
                if removed {
                    ui.popup = None;
                }
                return Ok(removed);
            }

            if matches!(ui.current_page(), PageState::JournalList { .. }) {
                let Some((list_id, complete_uris, _, visible)) = journal_list_projection(ui, state)
                else {
                    return Ok(false);
                };
                let Some(selection) = ui.current_page().journal_list_selection() else {
                    return Ok(false);
                };
                let visible_uris = visible
                    .iter()
                    .map(|entry| entry.track.id.uri())
                    .collect::<Vec<_>>();
                if !journal_uri_target_is_safe(
                    selection,
                    &complete_uris,
                    &visible_uris,
                    &target_uri,
                ) {
                    return Ok(false);
                }
                let captured_uris = complete_uris.clone();
                let mut removed = false;
                update_track_journal(state, |journal| {
                    let Some(list) = journal.list(&list_id) else {
                        return;
                    };
                    if !journal_list_uri_target_is_current(
                        &list.track_uris,
                        &captured_uris,
                        &target_uri,
                    ) {
                        return;
                    }
                    let Some(current_track) = journal
                        .entries
                        .get(&target_uri)
                        .map(|entry| entry.track.clone())
                    else {
                        return;
                    };
                    journal.remove_track(&current_track);
                    removed = true;
                })?;
                if removed {
                    ui.popup = None;
                }
                return Ok(removed);
            }
            Ok(false)
        }
        Action::RemoveFromJournalList => {
            let Some((list_id, complete_uris, _, visible)) = journal_list_projection(ui, state)
            else {
                return Ok(false);
            };
            let Some(selection) = ui.current_page().journal_list_selection() else {
                return Ok(false);
            };
            let visible_uris = visible
                .iter()
                .map(|entry| entry.track.id.uri())
                .collect::<Vec<_>>();
            if !journal_uri_target_is_safe(selection, &complete_uris, &visible_uris, &target_uri) {
                return Ok(false);
            }
            let captured_uris = complete_uris.clone();
            let mut removed = false;
            update_track_journal(state, |journal| {
                let Some(list) = journal.list(&list_id) else {
                    return;
                };
                if !journal_list_uri_target_is_current(
                    &list.track_uris,
                    &captured_uris,
                    &target_uri,
                ) {
                    return;
                }
                journal.remove_track_from_list(&list_id, &target_uri);
                removed = true;
            })?;
            if removed {
                ui.popup = None;
            }
            Ok(removed)
        }
        _ => Ok(false),
    }
}

fn handle_command_for_youtube_context_page(
    command: Command,
    client_pub: &crate::client::ClientRequestSender,
    ui: &mut UIStateGuard,
    state: &SharedState,
) -> Result<bool> {
    let selected = ui.current_page().selected_index().unwrap_or_default();
    let (context_id, context) = match ui.current_page() {
        PageState::YouTubeContext {
            id,
            context: Some(context),
            ..
        } => (id.clone(), context.clone()),
        PageState::YouTubeContext { context: None, .. } => return Ok(false),
        _ => anyhow::bail!("expect a YouTube context page"),
    };
    let tracks = if matches!(context_id, YouTubeContextId::Playlist(_)) {
        context.tracks.clone()
    } else {
        ui.search_filtered_items(&context.tracks)
            .into_iter()
            .cloned()
            .collect()
    };

    let playlist_projection = if matches!(context_id, YouTubeContextId::Playlist(_)) {
        let editable = youtube_playlist_is_editable(state, &context_id);
        let Some(projection) =
            synchronize_youtube_playlist_projection(ui, &context_id, &context, editable)
        else {
            return Ok(false);
        };
        Some(projection)
    } else {
        if !synchronize_youtube_context_page_selection(ui, &context_id, &context.tracks) {
            return Ok(false);
        }
        None
    };

    if command == Command::ShowActionsOnCurrentContext {
        if let YouTubeContextId::Playlist(playlist_id) = &context_id {
            let editable = youtube_playlist_is_editable(state, &context_id);
            let name = match ui.current_page() {
                PageState::YouTubeContext {
                    context: Some(context),
                    ..
                } => context.title.clone(),
                _ => YouTubeContextId::Playlist(playlist_id.clone())
                    .title()
                    .to_owned(),
            };
            return Ok(open_youtube_playlist_context_actions(
                playlist_id,
                &name,
                editable,
                ui,
            ));
        }
        let details = match ui.current_page() {
            PageState::YouTubeContext {
                context: Some(context),
                ..
            } => context.artist.clone(),
            _ => None,
        };
        let Some(details) = details else {
            return Ok(false);
        };
        ui.popup = Some(PopupState::YouTubeArtistMenu {
            details,
            state: ListState::default(),
        });
        return Ok(true);
    }

    if is_selection_command(command) {
        return Ok(handle_page_selection_command(command, ui, None));
    }
    if command == Command::ChooseSelected {
        let start_index = playlist_projection
            .as_ref()
            .and_then(|(_, projection)| projection.visible_source_index(selected))
            .unwrap_or(selected);
        if tracks.get(start_index).is_some() {
            client_pub.send(ClientRequest::PlayYouTubeContext {
                tracks,
                start_index,
            })?;
            window::clear_track_selection(ui);
            return Ok(true);
        }
        return Ok(false);
    }

    if command == Command::ShowActionsOnSelectedItem {
        let menu_selection = playlist_projection
            .as_ref()
            .and_then(|(snapshot, projection)| {
                ui.current_page()
                    .mutable_playlist_state()
                    .and_then(|playlist_state| {
                        MutablePlaylistController::menu_selection(
                            playlist_state,
                            snapshot,
                            projection,
                        )
                    })
            });
        let selected_tracks = menu_selection.as_ref().map_or_else(
            || selected_youtube_tracks_for_action(ui, &tracks, selected),
            |selection| {
                selection
                    .source_indices
                    .iter()
                    .filter_map(|index| tracks.get(*index).cloned())
                    .collect()
            },
        );
        let row_actions = menu_selection.map_or_else(
            || youtube_context_track_actions(&context_id),
            |selection| selection.actions,
        );
        let item = match selected_tracks.as_slice() {
            [] => return Ok(false),
            [track] => ActionListItem::YouTubeTrack(track.clone(), row_actions.clone()),
            _ => {
                let Ok(menu) = youtube_bulk_action_menu(
                    &selected_tracks,
                    &row_actions,
                    current_youtube_epoch(ui).value(),
                ) else {
                    return Ok(false);
                };
                ActionListItem::YouTubeTracks(menu)
            }
        };
        ui.popup = Some(PopupState::ActionList(Box::new(item), ListState::default()));
        return Ok(true);
    }

    if command == Command::AddSelectedItemToQueue {
        let selected_tracks = playlist_projection
            .as_ref()
            .and_then(|(snapshot, projection)| {
                ui.current_page()
                    .mutable_playlist_state()
                    .and_then(|playlist_state| {
                        MutablePlaylistController::menu_selection(
                            playlist_state,
                            snapshot,
                            projection,
                        )
                    })
            })
            .map_or_else(
                || selected_youtube_tracks_for_action(ui, &tracks, selected),
                |selection| {
                    selection
                        .source_indices
                        .iter()
                        .filter_map(|index| tracks.get(*index).cloned())
                        .collect()
                },
            );
        if !selected_tracks.is_empty() {
            if selected_tracks.len() > 1 {
                let epoch = current_youtube_epoch(ui);
                dispatch_youtube_queue_tracks(ui, client_pub, selected_tracks, epoch)?;
            } else {
                client_pub.send(ClientRequest::AddItemsToUserQueue(
                    selected_tracks
                        .into_iter()
                        .map(crate::state::PlayableMedia::YouTube)
                        .collect(),
                ))?;
            }
            window::clear_track_selection(ui);
            return Ok(true);
        }
        return Ok(false);
    }

    if matches!(
        command,
        Command::ExtendSelectionNext | Command::ExtendSelectionPrevious
    ) {
        let visible_len = playlist_projection
            .as_ref()
            .map_or(tracks.len(), |(_, projection)| projection.visible_len());
        if visible_len == 0 {
            return Ok(false);
        }
        let direction = if command == Command::ExtendSelectionNext {
            1
        } else {
            -1
        };
        return Ok(window::extend_track_selection(
            ui,
            selected,
            visible_len,
            1,
            direction,
        ));
    }

    let count = ui.count_prefix;
    let visible_len = playlist_projection
        .as_ref()
        .map_or(tracks.len(), |(_, projection)| projection.visible_len());
    Ok(window::navigate_and_clear_selection(
        command,
        ui,
        selected,
        visible_len,
        count,
    ))
}

fn youtube_context_track_actions(context_id: &YouTubeContextId) -> Vec<Action> {
    if matches!(context_id, YouTubeContextId::LikedTracks) {
        crate::command::construct_youtube_liked_track_actions()
    } else {
        crate::command::construct_youtube_track_actions()
    }
}

fn handle_action_for_youtube_context_page(
    action: Action,
    client_pub: &crate::client::ClientRequestSender,
    ui: &mut UIStateGuard,
    state: &SharedState,
) -> Result<bool> {
    let selected = ui.current_page().selected_index().unwrap_or_default();
    let (context_id, context) = match ui.current_page() {
        PageState::YouTubeContext {
            id,
            context: Some(context),
            ..
        } => (id.clone(), context.clone()),
        PageState::YouTubeContext { context: None, .. } => return Ok(false),
        _ => anyhow::bail!("expect a YouTube context page"),
    };
    let tracks = if matches!(context_id, YouTubeContextId::Playlist(_)) {
        context.tracks.clone()
    } else {
        ui.search_filtered_items(&context.tracks)
            .into_iter()
            .cloned()
            .collect()
    };

    let playlist_projection = if matches!(context_id, YouTubeContextId::Playlist(_)) {
        let editable = youtube_playlist_is_editable(state, &context_id);
        synchronize_youtube_playlist_projection(ui, &context_id, &context, editable)
    } else {
        if !synchronize_youtube_context_page_selection(ui, &context_id, &context.tracks) {
            return Ok(false);
        }
        None
    };
    let selected_tracks = playlist_projection
        .as_ref()
        .and_then(|(snapshot, projection)| {
            ui.current_page()
                .mutable_playlist_state()
                .and_then(|playlist_state| {
                    MutablePlaylistController::menu_selection(playlist_state, snapshot, projection)
                })
        })
        .map_or_else(
            || selected_youtube_tracks_for_action(ui, &tracks, selected),
            |selection| {
                selection
                    .source_indices
                    .iter()
                    .filter_map(|index| tracks.get(*index).cloned())
                    .collect()
            },
        );
    let context = match selected_tracks.as_slice() {
        [] => return Ok(false),
        [track] => ActionContext::YouTubeTrack(track.clone()),
        _ => ActionContext::YouTubeTracks(selected_tracks),
    };
    let data = state.data.read();
    handle_action_in_context(action, context, client_pub, &data, ui)
}

fn queue_action_item_for_row(item: &QueueDisplayItem) -> Option<QueueActionItem> {
    let media_id = item.media_id()?;
    match item {
        QueueDisplayItem::Unified { item, .. } => Some(QueueActionItem::new(
            OccurrenceDescriptor::with_token(media_id.clone(), item.entry_id),
            media_id,
            QueueActionPayload::Unified(item.media.clone()),
        )),
        QueueDisplayItem::Spotify { item, .. } => {
            let playable = match item.as_ref() {
                rspotify::model::PlayableItem::Track(track) => {
                    rspotify::model::PlayableId::Track(track.id.as_ref()?.clone())
                }
                rspotify::model::PlayableItem::Episode(episode) => {
                    rspotify::model::PlayableId::Episode(episode.id.clone())
                }
                rspotify::model::PlayableItem::Unknown(_) => return None,
            };
            Some(QueueActionItem::new(
                OccurrenceDescriptor::<MediaId, u64>::unique(media_id.clone()),
                media_id,
                QueueActionPayload::Native(playable),
            ))
        }
    }
}

/// Synchronize the Queue adapter against the authoritative player snapshot
/// and return the same snapshot for action resolution.
pub(crate) fn synchronize_queue_page_selection(
    ui: &mut UIStateGuard,
    state: &SharedState,
) -> Option<(QueueSelectionScope, Vec<QueueDisplayItem>)> {
    let (items, unified_instance_id) = {
        let player = state.player.read();
        (
            player.queue_display_items(),
            player.authoritative_unified_queue_instance_id(),
        )
    };
    let provider_epoch = ui.provider_selection_epoch(config::ActiveProvider::Spotify);
    let scope = unified_instance_id.clone().map_or_else(
        || QueueSelectionScope::native_spotify(provider_epoch),
        QueueSelectionScope::unified,
    );
    let sync_result = ui
        .current_page_mut()
        .queue_selection_mut()
        .map(|selection| {
            if let Some(instance_id) = unified_instance_id {
                let rows = items
                    .iter()
                    .map(|item| item.media_id().zip(item.unified_entry_id()))
                    .collect::<Option<Vec<_>>>();
                match rows {
                    Some(rows) => synchronize_unified_queue_items(selection, instance_id, rows),
                    None => selection.synchronize_cursor_only(
                        scope.clone(),
                        QueueSelectionView::All,
                        items.len(),
                    ),
                }
            } else {
                synchronize_native_spotify_queue(
                    selection,
                    provider_epoch,
                    items.iter().map(QueueDisplayItem::media_id),
                )
            }
        });
    match sync_result {
        Some(Ok(())) => Some((scope, items)),
        Some(Err(error)) => {
            tracing::warn!(selection_error = ?error, "Queue selection projection could not be synchronized");
            None
        }
        None => None,
    }
}

pub(crate) fn queue_action_items_for_snapshot(
    menu: &QueueActionMenu,
    rows: &[QueueDisplayItem],
) -> Option<Vec<QueueActionItem>> {
    let current = rows
        .iter()
        .filter_map(queue_action_item_for_row)
        .collect::<Vec<_>>();
    if current.len() != rows.len()
        || current
            .iter()
            .map(|item| item.occurrence())
            .collect::<HashSet<_>>()
            .len()
            != current.len()
    {
        return None;
    }
    menu.items()
        .iter()
        .map(|item| {
            current
                .iter()
                .find(|candidate| candidate.occurrence() == item.occurrence())
                .cloned()
        })
        .collect()
}

fn unified_playlist_action_item_for_row(
    item: &crate::state::UnifiedPlaylistItem,
) -> Option<UnifiedPlaylistActionItem> {
    Some(UnifiedPlaylistActionItem::new(
        OccurrenceDescriptor::with_token(item.media_id.clone(), item.entry_id),
        item.clone(),
    ))
}

pub(crate) fn unified_playlist_action_items_for_snapshot(
    menu: &UnifiedPlaylistActionMenu,
    rows: &[crate::state::UnifiedPlaylistItem],
) -> Option<Vec<UnifiedPlaylistActionItem>> {
    let current = rows
        .iter()
        .filter_map(unified_playlist_action_item_for_row)
        .collect::<Vec<_>>();
    if current
        .iter()
        .map(|item| item.occurrence())
        .collect::<HashSet<_>>()
        .len()
        != current.len()
    {
        return None;
    }
    menu.items()
        .iter()
        .map(|item| {
            current
                .iter()
                .find(|candidate| candidate.occurrence() == item.occurrence())
                .cloned()
        })
        .collect()
}

fn handle_command_for_unified_playlist_page(
    command: Command,
    client_pub: &crate::client::ClientRequestSender,
    ui: &mut UIStateGuard,
    state: &SharedState,
) -> Result<bool> {
    let selected = ui.current_page().selected_index().unwrap_or_default();
    let playlist_id = match ui.current_page() {
        PageState::UnifiedPlaylist { id, .. } => id.clone(),
        _ => anyhow::bail!("expect a unified playlist page"),
    };
    let listenbrainz_lifecycle = match ui.current_page() {
        PageState::UnifiedPlaylist {
            listenbrainz_sync, ..
        } => *listenbrainz_sync,
        _ => unreachable!("page type checked above"),
    };
    let (items, snapshot) = {
        let data = state.data.read();
        let Some(playlist) = data
            .unified_playlists
            .iter()
            .find(|playlist| playlist.id == playlist_id)
        else {
            return Ok(false);
        };
        let link = data
            .playlist_links
            .iter()
            .find(|link| link.unified_playlist_id == playlist_id);
        let configs = config::get_config();
        let _summary = crate::state::ListenBrainzSyncSummary::project(
            configs.app_config.listenbrainz.enabled,
            configs.app_config.listenbrainz.read_only_checking,
            configs.listenbrainz_token().is_some(),
            playlist,
            link,
            listenbrainz_lifecycle,
        );
        // The context menu owns exactly one ListenBrainz entry. Every
        // per-operation action lives inside the workspace window so the
        // playlist menu stays uncrowded.
        let action_model = unified_playlist_action_model_with_listenbrainz(
            link.is_some_and(|link| link.youtube_playlist_id.is_some()),
            true,
        );
        let context_actions = action_model.context_actions().iter().copied();
        (
            playlist.items.clone(),
            PlaylistSnapshot::from_unified(
                playlist,
                PlaylistActionModel::default(),
                context_actions,
            ),
        )
    };
    let filter = match ui.popup.as_ref() {
        Some(PopupState::Search { query }) => Some(query.clone()),
        _ => None,
    };
    let projection =
        ui.current_page_mut()
            .mutable_playlist_state_mut()
            .and_then(|playlist_state| {
                MutablePlaylistController::synchronize(playlist_state, &snapshot, filter.as_deref())
                    .ok()
            });
    let Some(projection) = projection else {
        return Ok(false);
    };
    let visible_items = projection
        .visible_indices()
        .iter()
        .filter_map(|index| items.get(*index).cloned())
        .collect::<Vec<_>>();
    if is_selection_command(command) {
        return Ok(handle_page_selection_command(command, ui, None));
    }
    if command == Command::ShowActionsOnCurrentContext {
        return Ok(open_unified_playlist_context_actions(
            &playlist_id,
            &snapshot.title,
            snapshot.actions.context_actions().iter().copied(),
            ui,
        ));
    }
    let selection_status = ui
        .current_page()
        .unified_playlist_selection()
        .map_or(ScopedSelectionStatus::Unscoped, |selection| {
            selection.status()
        });

    if matches!(
        command,
        Command::ExtendSelectionNext | Command::ExtendSelectionPrevious
    ) {
        let offset = ui.count_prefix.unwrap_or(1);
        let direction = if command == Command::ExtendSelectionPrevious {
            -1isize
        } else {
            1isize
        };
        let handled =
            window::extend_track_selection(ui, selected, visible_items.len(), offset, direction);
        update_unified_playlist_cursor(ui, &visible_items);
        return Ok(handled);
    }

    if matches!(
        command,
        Command::MovePlaylistItemUp | Command::MovePlaylistItemDown
    ) {
        return move_unified_playlist_selection(
            command,
            &playlist_id,
            &items,
            &visible_items,
            selected,
            ui,
            state,
        );
    }

    if command == Command::ChooseSelected {
        let playable = visible_items
            .iter()
            .filter_map(crate::state::UnifiedPlaylistItem::playable_media)
            .collect::<Vec<_>>();
        let Some(start_index) = playable_index_for_selection(&visible_items, selected) else {
            return Ok(false);
        };
        client_pub.send(ClientRequest::PlayUnifiedItems {
            items: playable,
            start_index,
        })?;
        return Ok(true);
    }

    if command == Command::AddSelectedItemToQueue {
        let Some(menu_selection) =
            ui.current_page()
                .mutable_playlist_state()
                .and_then(|playlist_state| {
                    MutablePlaylistController::menu_selection(
                        playlist_state,
                        &snapshot,
                        &projection,
                    )
                })
        else {
            return Ok(false);
        };
        if menu_selection.source_indices.len() > 1 {
            let action_items = menu_selection
                .source_indices
                .iter()
                .filter_map(|index| {
                    items
                        .get(*index)
                        .and_then(unified_playlist_action_item_for_row)
                })
                .collect::<Vec<_>>();
            if action_items.len() != menu_selection.source_indices.len() {
                return Ok(false);
            }
            let scope = UnifiedPlaylistSelectionScope::new(playlist_id.clone());
            let menu = UnifiedPlaylistActionMenu::new(scope.clone(), action_items);
            let current_items = menu.items().to_vec();
            let Ok(plan) =
                replan_unified_playlist_menu(Action::AddToQueue, &menu, &scope, &current_items)
            else {
                return Ok(false);
            };
            dispatch_unified_playlist_menu(&menu, &plan, ui, client_pub)
                .map_err(|error| anyhow::anyhow!(error.to_string()))?;
            window::clear_track_selection(ui);
            return Ok(true);
        }
        let Some(index) = menu_selection.source_indices.first().copied() else {
            return Ok(false);
        };
        if let Some(item) = items
            .get(index)
            .and_then(crate::state::UnifiedPlaylistItem::playable_media)
        {
            client_pub.send(ClientRequest::AddItemsToUserQueue(vec![item]))?;
            window::clear_track_selection(ui);
            return Ok(true);
        }
        return Ok(false);
    }

    if command == Command::ShowActionsOnSelectedItem {
        if selection_status == ScopedSelectionStatus::Ambiguous {
            return Ok(false);
        }
        let Some(menu_selection) =
            ui.current_page()
                .mutable_playlist_state()
                .and_then(|playlist_state| {
                    MutablePlaylistController::menu_selection(
                        playlist_state,
                        &snapshot,
                        &projection,
                    )
                })
        else {
            return Ok(false);
        };
        let action_items = menu_selection
            .source_indices
            .iter()
            .filter_map(|index| {
                items
                    .get(*index)
                    .and_then(unified_playlist_action_item_for_row)
            })
            .collect::<Vec<_>>();
        if action_items.len() != menu_selection.source_indices.len() || action_items.is_empty() {
            return Ok(false);
        }
        ui.popup = Some(PopupState::ActionList(
            Box::new(ActionListItem::UnifiedPlaylist(
                UnifiedPlaylistActionMenu::with_actions(
                    UnifiedPlaylistSelectionScope::new(playlist_id),
                    action_items,
                    menu_selection.actions,
                ),
            )),
            ListState::default(),
        ));
        return Ok(true);
    }

    let count = ui.count_prefix;
    let handled =
        window::navigate_and_clear_selection(command, ui, selected, visible_items.len(), count);
    update_unified_playlist_cursor(ui, &visible_items);
    Ok(handled)
}

pub(crate) fn update_unified_playlist_cursor(
    ui: &mut UIStateGuard,
    visible_items: &[crate::state::UnifiedPlaylistItem],
) {
    let entry_id = ui
        .current_page()
        .selected_index()
        .and_then(|index| visible_items.get(index))
        .map(|item| item.entry_id);
    ui.current_page_mut()
        .set_unified_playlist_cursor_entry_id(entry_id);
}

pub(crate) fn unified_playlist_cursor_after_remove(
    complete_items: &[crate::state::UnifiedPlaylistItem],
    visible_items: &[crate::state::UnifiedPlaylistItem],
    prior_cursor: Option<crate::state::PlaylistEntryId>,
    first_removed_index: usize,
) -> Option<(usize, crate::state::PlaylistEntryId)> {
    prior_cursor
        .and_then(|entry_id| {
            let index = visible_items
                .iter()
                .position(|item| item.entry_id == entry_id)?;
            Some((index, entry_id))
        })
        .or_else(|| {
            let boundary = first_removed_index.min(complete_items.len().checked_sub(1)?);
            let full_indices = complete_items
                .iter()
                .enumerate()
                .map(|(index, item)| (item.entry_id, index))
                .collect::<HashMap<_, _>>();
            visible_items
                .iter()
                .enumerate()
                .filter_map(|(visible_index, item)| {
                    full_indices
                        .get(&item.entry_id)
                        .copied()
                        .map(|full_index| (visible_index, item.entry_id, full_index))
                })
                .filter(|(_, _, full_index)| *full_index >= boundary)
                .min_by_key(|(_, _, full_index)| *full_index)
                .or_else(|| {
                    visible_items
                        .iter()
                        .enumerate()
                        .filter_map(|(visible_index, item)| {
                            full_indices
                                .get(&item.entry_id)
                                .copied()
                                .map(|full_index| (visible_index, item.entry_id, full_index))
                        })
                        .max_by_key(|(_, _, full_index)| *full_index)
                })
                .map(|(visible_index, entry_id, _)| (visible_index, entry_id))
        })
}

fn move_unified_playlist_selection(
    command: Command,
    playlist_id: &str,
    items: &[crate::state::UnifiedPlaylistItem],
    visible_items: &[crate::state::UnifiedPlaylistItem],
    cursor: usize,
    ui: &mut UIStateGuard,
    state: &SharedState,
) -> Result<bool> {
    let selected_visible = ui
        .current_page()
        .unified_playlist_selection()
        .and_then(|selection| unified_playlist_selected_or_cursor_indices(selection, cursor).ok())
        .unwrap_or_default();
    let selected_ids = selected_visible
        .iter()
        .filter_map(|index| visible_items.get(*index).map(|item| item.entry_id))
        .collect::<Vec<_>>();
    if selected_ids.len() != selected_visible.len() || selected_ids.is_empty() {
        return Ok(false);
    }

    let selected_set = selected_ids.iter().copied().collect::<HashSet<_>>();
    let source_indices = items
        .iter()
        .enumerate()
        .filter_map(|(index, item)| selected_set.contains(&item.entry_id).then_some(index))
        .collect::<Vec<_>>();
    if source_indices.len() != selected_ids.len() {
        return Ok(false);
    }
    let distance = ui.count_prefix.unwrap_or(1);
    let destination_boundary = if command == Command::MovePlaylistItemUp {
        source_indices[0].saturating_sub(distance)
    } else {
        source_indices[source_indices.len() - 1]
            .saturating_add(1)
            .saturating_add(distance)
            .min(items.len())
    };
    let scope = playlist_id.to_owned();
    let snapshot = items.iter().map(|item| item.entry_id).collect::<Vec<_>>();
    let handles = selected_ids
        .iter()
        .copied()
        .map(|entry_id| StructuralMoveHandle::new(scope.clone(), entry_id))
        .collect::<Vec<_>>();
    let Ok(plan) = plan_structural_block_move(&scope, &snapshot, &handles, destination_boundary)
    else {
        return Ok(false);
    };
    if plan.is_no_op() {
        return Ok(true);
    }

    let reference = ui.start_operation(
        crate::state::UiOperationKind::ProviderCommand,
        crate::state::MUTATION_RUNNING_CODE,
        crate::state::MUTATION_RUNNING_MESSAGE,
    );
    let result = state.data.write().move_unified_playlist_items_if_current(
        playlist_id,
        &snapshot,
        plan.ordered_occurrence_keys(),
        plan.insertion_index(),
    );
    if let Err(error) = result {
        ui.complete_operation(
            &reference,
            crate::state::UiOperationState::Failed,
            crate::state::MUTATION_FAILURE_CODE,
            crate::state::MUTATION_FAILURE_MESSAGE,
            Some(crate::state::MUTATION_FAILURE_NEXT_ACTION),
        );
        return Err(error);
    }
    ui.complete_operation(
        &reference,
        crate::state::UiOperationState::Completed,
        crate::state::MUTATION_COMPLETED_CODE,
        crate::state::MUTATION_COMPLETED_MESSAGE,
        None,
    );

    let fresh_items = state
        .data
        .read()
        .unified_playlists
        .iter()
        .find(|playlist| playlist.id == playlist_id)
        .map(|playlist| playlist.items.clone())
        .unwrap_or_default();
    let fresh_visible = ui
        .search_filtered_items(&fresh_items)
        .into_iter()
        .cloned()
        .collect::<Vec<_>>();
    if let Some(selection) = ui.current_page_mut().unified_playlist_selection_mut() {
        synchronize_unified_playlist_entries(
            selection,
            playlist_id.to_owned(),
            fresh_items
                .iter()
                .map(|item| (item.media_id.clone(), item.entry_id)),
            fresh_visible
                .iter()
                .map(|item| (item.media_id.clone(), item.entry_id)),
        )
        .map_err(|error| anyhow::anyhow!("Unified playlist selection refresh failed: {error:?}"))?;
    }
    let preferred = ui
        .current_page()
        .unified_playlist_cursor_entry_id()
        .filter(|entry_id| selected_set.contains(entry_id))
        .unwrap_or(selected_ids[0]);
    if let Some(index) = fresh_visible
        .iter()
        .position(|item| item.entry_id == preferred)
    {
        ui.current_page_mut().select(index);
        ui.current_page_mut()
            .set_unified_playlist_cursor_entry_id(Some(preferred));
    } else {
        update_unified_playlist_cursor(ui, &fresh_visible);
    }
    Ok(true)
}

fn playable_index_for_selection(
    items: &[crate::state::UnifiedPlaylistItem],
    selected: usize,
) -> Option<usize> {
    items.get(selected)?.playable_media()?;
    Some(
        items[..selected]
            .iter()
            .filter_map(crate::state::UnifiedPlaylistItem::playable_media)
            .count(),
    )
}

#[allow(clippy::unnecessary_wraps)] // All page action handlers share this fallible contract.
fn handle_action_for_unified_playlist_page(
    action: Action,
    client_pub: &crate::client::ClientRequestSender,
    ui: &mut UIStateGuard,
    state: &SharedState,
) -> Result<bool> {
    if action == Action::AddToQueue {
        return handle_command_for_unified_playlist_page(
            Command::AddSelectedItemToQueue,
            client_pub,
            ui,
            state,
        );
    }
    Ok(false)
}

fn move_search_category(ui: &mut UIStateGuard, provider: config::ActiveProvider, forward: bool) {
    let panes = crate::command::provider_capabilities(provider).search_panes();
    let category_count = panes.len().saturating_add(1);
    if category_count == 0 {
        return;
    }
    if let PageState::Search { state, .. } = ui.current_page_mut() {
        let current_index = state
            .category
            .and_then(|category| panes.iter().position(|pane| *pane == category))
            .unwrap_or(0);
        let next_index = if forward {
            (current_index + 1) % category_count
        } else if current_index == 0 {
            category_count - 1
        } else {
            current_index - 1
        };
        state.category = next_index.checked_sub(1).map(|index| panes[index]);
        state.focus = SearchFocusState::Category;
    }
    ui.bump_diagnostic_revision();
}

fn start_search_request(
    client_pub: &crate::client::ClientRequestSender,
    provider: config::ActiveProvider,
    query: &str,
    ui: &mut UIStateGuard,
) -> Result<()> {
    let lifecycle_reference = ui.begin_search(provider, query);
    let request = match provider {
        config::ActiveProvider::Spotify => ClientRequest::Search {
            query: query.to_owned(),
            lifecycle_reference: lifecycle_reference.clone(),
        },
        config::ActiveProvider::YouTubeMusic => {
            let youtube_auth_status = config::get_config().youtube_music_auth_status();
            if youtube_auth_status.missing_message().is_some() {
                tracing::warn!("YouTube Music authentication is not ready");
                ui.finish_search_unavailable(provider, query, &lifecycle_reference);
                return Ok(());
            }
            ClientRequest::SearchYouTube {
                query: query.to_owned(),
                lifecycle_reference: lifecycle_reference.clone(),
            }
        }
    };
    let result = client_pub.send(request);
    if result.is_err() {
        ui.finish_search_failure(provider, query, &lifecycle_reference);
    }
    result?;
    Ok(())
}

/// Reload a retained Search page after account-data invalidation. This runs
/// after terminal events, so a history pop makes the restored Search page
/// active before this check dispatches its same query under the current
/// account.
pub(super) fn reload_invalidated_active_search(
    client_pub: &crate::client::ClientRequestSender,
    state: &SharedState,
) -> Result<()> {
    let mut ui = state.ui.lock();
    let active_provider = ui.active_provider;
    let reload = match ui.current_page() {
        PageState::Search {
            current_query,
            state,
            ..
        } if !current_query.trim().is_empty()
            && state.search_lifecycle == crate::state::SearchLifecycle::Idle =>
        {
            Some((
                state.provider.unwrap_or(active_provider),
                current_query.clone(),
            ))
        }
        _ => None,
    };
    if let Some((provider, query)) = reload {
        start_search_request(client_pub, provider, &query, &mut ui)?;
    }
    Ok(())
}

fn handle_key_sequence_for_search_page(
    key_sequence: &KeySequence,
    client_pub: &crate::client::ClientRequestSender,
    state: &SharedState,
    ui: &mut UIStateGuard,
) -> Result<bool> {
    let active_provider = ui.active_provider;
    let (focus_state, current_query, search_provider) = match ui.current_page() {
        PageState::Search {
            state,
            current_query,
            ..
        } => {
            let provider = state.provider.unwrap_or(active_provider);
            (state.focus, current_query.clone(), provider)
        }
        _ => anyhow::bail!("expect a search page"),
    };

    // Search participates in the persistent workspace, so navigation focus
    // must be handled before provider-specific search commands.
    if ui.workspace_focus == WorkspaceFocusState::Navigation {
        if let Some(CommandOrAction::Command(command)) = config::get_config()
            .keymap_config
            .find_command_or_action_from_key_sequence(key_sequence)
        {
            return handle_workspace_navigation_command(command, client_pub, state, ui);
        }
    }

    // Escape is local to Search: leave the query editor for the first result
    // group, and return to the editor from a result group.
    if key_sequence.keys.as_slice() == [Key::None(KeyCode::Esc)] {
        if focus_state == SearchFocusState::Category {
            if ui.workspace_layout.show_navigation {
                ui.workspace_focus = WorkspaceFocusState::Navigation;
            }
            ui.bump_diagnostic_revision();
            return Ok(true);
        }
        if let PageState::Search { state, .. } = ui.current_page_mut() {
            state.focus = if focus_state == SearchFocusState::Input {
                SearchFocusState::from_provider_pane(
                    crate::command::provider_capabilities(search_provider)
                        .search_panes()
                        .first()
                        .copied()
                        .unwrap_or(crate::command::ProviderSearchPane::Tracks),
                )
            } else {
                SearchFocusState::Input
            };
            ui.workspace_focus = WorkspaceFocusState::Context;
            ui.bump_diagnostic_revision();
        }
        return Ok(true);
    }

    if focus_state == SearchFocusState::Category && key_sequence.keys.len() == 1 {
        match key_sequence.keys[0] {
            Key::None(KeyCode::Left | KeyCode::Char('h')) => {
                move_search_category(ui, search_provider, false);
                return Ok(true);
            }
            Key::None(KeyCode::Right | KeyCode::Char('l')) => {
                move_search_category(ui, search_provider, true);
                return Ok(true);
            }
            Key::None(KeyCode::Enter) => {
                if let PageState::Search { state, .. } = ui.current_page_mut() {
                    state.focus = state.category.map_or(
                        SearchFocusState::Input,
                        SearchFocusState::from_provider_pane,
                    );
                }
                ui.workspace_focus = WorkspaceFocusState::Context;
                ui.bump_diagnostic_revision();
                return Ok(true);
            }
            _ => {}
        }
    }

    // handle user's input
    if let SearchFocusState::Input = focus_state {
        if key_sequence.keys.len() == 1 {
            match &key_sequence.keys[0] {
                Key::None(crossterm::event::KeyCode::Enter) => {
                    let (input_text, input_empty) = match ui.current_page() {
                        PageState::Search { line_input, .. } => {
                            (line_input.get_text(), line_input.is_empty())
                        }
                        _ => return Ok(false),
                    };
                    if !input_empty {
                        let parsed_query =
                            parse_provider_search_query(&input_text, active_provider);
                        if let Some(user_query) = parsed_query.spotify_user_query {
                            if !spotify_user_query_is_valid(&user_query) {
                                ui.set_unsupported_operation(
                                    "Spotify user search needs a name or profile URL.",
                                    "Use /user <name>, /user me, or paste a Spotify profile URL.",
                                );
                                return Ok(true);
                            }
                            let lookup_query = crate::ui::utils::bounded_text(&user_query, 80);
                            ui.popup = Some(PopupState::SpotifyUserSearch {
                                query: lookup_query.clone(),
                            });
                            if client_pub
                                .send(ClientRequest::GetSpotifyUser(lookup_query))
                                .is_err()
                            {
                                ui.popup = Some(PopupState::DeferredAction {
                                    title: "Spotify User Search".to_string(),
                                    message:
                                        "The Spotify user lookup could not be queued. Try again."
                                            .to_string(),
                                });
                            }
                            return Ok(true);
                        }
                        if !provider_search_query_is_valid(&parsed_query.query) {
                            ui.set_unsupported_operation(
                                "Search needs a query.",
                                "Type a term after /sp or /yt, then press Enter.",
                            );
                            return Ok(true);
                        }

                        let query = parsed_query.query;
                        let provider = parsed_query.provider;
                        if let PageState::Search {
                            current_query,
                            state,
                            ..
                        } = ui.current_page_mut()
                        {
                            current_query.clone_from(&query);
                            state.provider = Some(provider);
                        }
                        window::clear_track_selection(ui);
                        start_search_request(client_pub, provider, &query, ui)?;
                        if let PageState::Search { state, .. } = ui.current_page_mut() {
                            state.focus = SearchFocusState::from_provider_pane(
                                state.category.unwrap_or_else(|| {
                                    crate::command::provider_capabilities(provider).search_panes()
                                        [0]
                                }),
                            );
                        }
                        ui.current_page_mut().select(0);
                        ui.bump_diagnostic_revision();
                    }
                    return Ok(true);
                }
                k => {
                    let consumed = match ui.current_page_mut() {
                        PageState::Search { line_input, .. } => line_input.input(k).is_some(),
                        _ => false,
                    };
                    if consumed {
                        return Ok(true);
                    }
                }
            }
        }
    }

    let Some(found_keymap) = config::get_config()
        .keymap_config
        .find_command_or_action_from_key_sequence(key_sequence)
    else {
        return Ok(false);
    };

    if ui.workspace_focus == WorkspaceFocusState::Context {
        if let CommandOrAction::Command(command) = found_keymap {
            match command {
                Command::FocusNextWindow => {
                    if let PageState::Search { state, .. } = ui.current_page_mut() {
                        if state.focus == SearchFocusState::Category {
                            state.focus = SearchFocusState::Input;
                        } else {
                            state.focus.next_for_provider(search_provider);
                        }
                        ui.current_page_mut().select(0);
                        window::clear_track_selection(ui);
                        ui.bump_diagnostic_revision();
                    }
                    return Ok(true);
                }
                Command::FocusPreviousWindow if focus_state == SearchFocusState::Input => {
                    if ui.workspace_layout.show_navigation {
                        if let PageState::Search { state, .. } = ui.current_page_mut() {
                            state.focus = SearchFocusState::Category;
                        }
                        ui.bump_diagnostic_revision();
                    }
                    return Ok(true);
                }
                Command::FocusPreviousWindow if focus_state == SearchFocusState::Category => {
                    if ui.workspace_layout.show_navigation {
                        ui.workspace_focus = WorkspaceFocusState::Navigation;
                        ui.bump_diagnostic_revision();
                    }
                    return Ok(true);
                }
                _ => {}
            }
        }
    }

    let search_has_cached_results = {
        let data = state.data.read();
        match search_provider {
            config::ActiveProvider::Spotify => data.caches.search.contains_key(&current_query),
            config::ActiveProvider::YouTubeMusic => {
                data.caches.youtube_search.contains_key(&current_query)
            }
        }
    };

    if focus_state != SearchFocusState::Input
        && focus_state != SearchFocusState::Category
        && matches!(
            &found_keymap,
            CommandOrAction::Command(Command::ChooseSelected)
        )
        && !search_has_cached_results
        && ui.arm_search_lucky(search_provider, &current_query, focus_state)
    {
        return Ok(true);
    }

    if focus_state == SearchFocusState::Category {
        return Ok(false);
    }

    let data = state.data.read();
    if search_provider == config::ActiveProvider::YouTubeMusic {
        drop(data);
        return handle_key_sequence_for_youtube_search_page(
            found_keymap,
            client_pub,
            &current_query,
            state,
            ui,
        );
    }

    let search_results = data.caches.search.get(&current_query);
    if focus_state == SearchFocusState::Tracks {
        let tracks = search_results
            .map(|results| results.tracks.iter().collect::<Vec<_>>())
            .unwrap_or_default();
        synchronize_spotify_search_selection(ui, &current_query, &tracks);
        if let CommandOrAction::Command(command) = found_keymap {
            if handle_search_selection_command(command, ui) {
                return Ok(true);
            }
            if is_selection_command(command) {
                return Ok(false);
            }
        }
        if matches!(
            found_keymap,
            CommandOrAction::Action(Action::AddToListenLater, ActionTarget::SelectedItem)
        ) {
            let selected_tracks = window::selected_tracks_for_action(ui, &tracks);
            if selected_tracks.len() > 1 {
                drop(data);
                return handle_bulk_add_to_listen_later(selected_tracks, state, ui);
            }
        }
    }

    // While the YouTube client is active, `/sp ...` is the explicit escape
    // hatch for adding a Spotify item to the provider-neutral queue.
    if search_provider == config::ActiveProvider::Spotify
        && active_provider == config::ActiveProvider::YouTubeMusic
        && focus_state == SearchFocusState::Tracks
        && matches!(
            found_keymap,
            CommandOrAction::Command(Command::AddSelectedItemToQueue)
        )
    {
        let tracks = search_results
            .map(|results| results.tracks.iter().collect::<Vec<_>>())
            .unwrap_or_default();
        let selected_tracks = window::selected_tracks_for_action(ui, &tracks);
        if !selected_tracks.is_empty() {
            if selected_tracks.len() == 1 {
                let track = selected_tracks.into_iter().next().expect("one track");
                ui.spotify_queue_labels.remember_track(&track);
                client_pub.send(ClientRequest::AddItemsToUserQueue(vec![
                    crate::state::PlayableMedia::Spotify(track.id.into()),
                ]))?;
                window::clear_track_selection(ui);
                return Ok(true);
            }
            let tracks = selected_tracks.into_iter().collect::<Vec<_>>();
            for track in &tracks {
                ui.spotify_queue_labels.remember_track(track);
            }
            let plan = super::bulk_action::plan_spotify_tracks_for_owner(
                &tracks,
                current_spotify_epoch(ui),
                Action::AddToQueue,
                crate::command::BulkActionOwner::UnifiedQueue,
            )
            .map_err(|_| anyhow::anyhow!("bulk queue plan is unavailable"))?;
            let assignment = super::bulk_action::BulkRequestAssignment::new(
                ClientRequest::AddItemsToUserQueue(
                    tracks
                        .into_iter()
                        .map(|track| crate::state::PlayableMedia::Spotify(track.id.into()))
                        .collect(),
                ),
                plan.operation_ids(),
            );
            super::bulk_action::dispatch_bulk_requests(ui, client_pub, &plan, vec![assignment])?;
            window::clear_track_selection(ui);
            return Ok(true);
        }
        return Ok(false);
    }

    match focus_state {
        SearchFocusState::Category => anyhow::bail!("search category should be handled before"),
        SearchFocusState::Input => anyhow::bail!("user's search input should be handled before"),
        SearchFocusState::Tracks => {
            let tracks = search_results
                .map(|s| s.tracks.iter().collect::<Vec<_>>())
                .unwrap_or_default();

            match found_keymap {
                CommandOrAction::Command(command) => window::handle_command_for_track_list_window(
                    command, client_pub, &tracks, &data, ui, state,
                ),
                CommandOrAction::Action(action, ActionTarget::SelectedItem) => {
                    window::handle_action_for_search_track_list(
                        action, &tracks, &data, ui, client_pub, state,
                    )
                }
                CommandOrAction::Action(..) => Ok(false),
            }
        }
        SearchFocusState::Videos => Ok(false),
        SearchFocusState::Artists => {
            let artists = search_results
                .map(|s| s.artists.iter().collect::<Vec<_>>())
                .unwrap_or_default();

            match found_keymap {
                CommandOrAction::Command(command) => Ok(
                    window::handle_command_for_artist_list_window(command, &artists, &data, ui),
                ),
                CommandOrAction::Action(action, ActionTarget::SelectedItem) => {
                    window::handle_action_for_selected_item(action, &artists, &data, ui, client_pub)
                }
                CommandOrAction::Action(..) => Ok(false),
            }
        }
        SearchFocusState::Albums => {
            let albums = search_results
                .map(|s| s.albums.iter().collect::<Vec<_>>())
                .unwrap_or_default();

            match found_keymap {
                CommandOrAction::Command(command) => window::handle_command_for_album_list_window(
                    command, &albums, &data, ui, client_pub,
                ),
                CommandOrAction::Action(action, ActionTarget::SelectedItem) => {
                    window::handle_action_for_selected_item(action, &albums, &data, ui, client_pub)
                }
                CommandOrAction::Action(..) => Ok(false),
            }
        }
        SearchFocusState::Playlists => {
            let playlists = search_results
                .map(|s| {
                    s.playlists
                        .iter()
                        .map(|p| PlaylistFolderItem::Playlist(p.clone()))
                        .collect::<Vec<_>>()
                })
                .unwrap_or_default();
            let playlist_refs = playlists.iter().collect::<Vec<_>>();

            match found_keymap {
                CommandOrAction::Command(command) => {
                    Ok(window::handle_command_for_playlist_list_window(
                        command,
                        &playlist_refs,
                        &data,
                        ui,
                    ))
                }
                CommandOrAction::Action(action, ActionTarget::SelectedItem) => {
                    window::handle_action_for_selected_item(
                        action,
                        &playlist_refs,
                        &data,
                        ui,
                        client_pub,
                    )
                }
                CommandOrAction::Action(..) => Ok(false),
            }
        }
        SearchFocusState::Shows => {
            let shows = search_results
                .map(|s| s.shows.iter().collect::<Vec<_>>())
                .unwrap_or_default();

            match found_keymap {
                CommandOrAction::Command(command) => Ok(
                    window::handle_command_for_show_list_window(command, &shows, &data, ui),
                ),
                CommandOrAction::Action(action, ActionTarget::SelectedItem) => {
                    window::handle_action_for_selected_item(action, &shows, &data, ui, client_pub)
                }
                CommandOrAction::Action(..) => Ok(false),
            }
        }
        SearchFocusState::Episodes => {
            let episodes = match search_results {
                Some(s) => s.episodes.iter().collect(),
                None => Vec::new(),
            };

            match found_keymap {
                CommandOrAction::Command(command) => {
                    window::handle_command_for_episode_list_window(
                        command, client_pub, &episodes, &data, ui, state,
                    )
                }
                CommandOrAction::Action(action, ActionTarget::SelectedItem) => {
                    window::handle_action_for_selected_item(
                        action, &episodes, &data, ui, client_pub,
                    )
                }
                CommandOrAction::Action(..) => Ok(false),
            }
        }
    }
}

struct ParsedProviderSearchQuery {
    provider: config::ActiveProvider,
    query: String,
    spotify_user_query: Option<String>,
}

fn parse_provider_search_query(
    query: &str,
    default_provider: config::ActiveProvider,
) -> ParsedProviderSearchQuery {
    let trimmed = query.trim();
    if trimmed == "/user" || trimmed.starts_with("/user ") {
        let query = trimmed.strip_prefix("/user").unwrap_or_default();
        return ParsedProviderSearchQuery {
            provider: config::ActiveProvider::Spotify,
            query: String::new(),
            spotify_user_query: Some(query.trim().to_string()),
        };
    }
    if let Some(query) = trimmed
        .strip_prefix("/yt ")
        .or_else(|| trimmed.strip_prefix("/youtube "))
    {
        return ParsedProviderSearchQuery {
            provider: config::ActiveProvider::YouTubeMusic,
            query: query.trim().to_string(),
            spotify_user_query: None,
        };
    }

    if let Some(query) = trimmed.strip_prefix("/sp ") {
        return ParsedProviderSearchQuery {
            provider: config::ActiveProvider::Spotify,
            query: query.trim().to_string(),
            spotify_user_query: None,
        };
    }

    ParsedProviderSearchQuery {
        provider: default_provider,
        query: trimmed.to_string(),
        spotify_user_query: None,
    }
}

fn spotify_user_query_is_valid(query: &str) -> bool {
    !query.trim().is_empty()
}

fn provider_search_query_is_valid(query: &str) -> bool {
    !query.trim().is_empty()
}

#[allow(clippy::needless_pass_by_value)] // The handler owns and destructures the matched command.
fn handle_key_sequence_for_youtube_search_page(
    found_keymap: CommandOrAction,
    client_pub: &crate::client::ClientRequestSender,
    current_query: &str,
    state: &SharedState,
    ui: &mut UIStateGuard,
) -> Result<bool> {
    let focus = match ui.current_page() {
        PageState::Search { state, .. } => state.focus,
        _ => return Ok(false),
    };
    let (command, action) = match found_keymap {
        CommandOrAction::Command(command) => (Some(command), None),
        CommandOrAction::Action(action, ActionTarget::SelectedItem) => (None, Some(action)),
        CommandOrAction::Action(..) => return Ok(false),
    };
    if let Some(action) = action {
        let results = state
            .data
            .read()
            .caches
            .youtube_search
            .get(current_query)
            .cloned()
            .unwrap_or_default();
        synchronize_youtube_search_selection(ui, current_query, focus, &results);
        let tracks = match focus {
            SearchFocusState::Tracks => results.songs.clone(),
            SearchFocusState::Videos => results.videos.clone(),
            SearchFocusState::Episodes => results
                .episodes
                .iter()
                .map(|episode| episode.track.clone())
                .collect(),
            _ => {
                ui.set_unsupported_operation(
                    "Actions are unavailable for this YouTube result type.",
                    "Press Enter to open the selected album, artist, playlist, or podcast.",
                );
                return Ok(true);
            }
        };
        let selected = ui.current_page().selected_index().unwrap_or_default();
        let selected_tracks = selected_youtube_tracks_for_action(ui, &tracks, selected);
        let data = state.data.read();
        let context = match selected_tracks.as_slice() {
            [] => return Ok(false),
            [track] => ActionContext::YouTubeTrack(track.clone()),
            _ => ActionContext::YouTubeTracks(selected_tracks),
        };
        if !youtube_search_action_is_supported(action) {
            ui.set_unsupported_operation(
                "That action is unavailable for YouTube Music.",
                "Choose a supported YouTube Music action.",
            );
            return Ok(true);
        }
        return handle_action_in_context(action, context, client_pub, &data, ui);
    }
    let Some(command) = command else {
        return Ok(false);
    };
    if matches!(
        command,
        Command::FocusNextWindow | Command::FocusPreviousWindow
    ) {
        ui.clear_search_lucky();
        if let PageState::Search { state, .. } = ui.current_page_mut() {
            if command == Command::FocusNextWindow {
                state
                    .focus
                    .next_for_provider(config::ActiveProvider::YouTubeMusic);
            } else {
                state
                    .focus
                    .previous_for_provider(config::ActiveProvider::YouTubeMusic);
            }
            ui.current_page_mut().select(0);
            window::clear_track_selection(ui);
        }
        return Ok(true);
    }

    let results = state
        .data
        .read()
        .caches
        .youtube_search
        .get(current_query)
        .cloned()
        .unwrap_or_default();
    synchronize_youtube_search_selection(ui, current_query, focus, &results);
    if is_selection_command(command) {
        return Ok(handle_search_selection_command(command, ui));
    }
    let tracks = match focus {
        SearchFocusState::Tracks => Some(results.songs.clone()),
        SearchFocusState::Videos => Some(results.videos.clone()),
        SearchFocusState::Episodes => Some(
            results
                .episodes
                .iter()
                .map(|episode| episode.track.clone())
                .collect(),
        ),
        _ => None,
    };
    let len = match focus {
        SearchFocusState::Category | SearchFocusState::Input => 0,
        SearchFocusState::Tracks => results.songs.len(),
        SearchFocusState::Videos => results.videos.len(),
        SearchFocusState::Albums => results.albums.len(),
        SearchFocusState::Artists => results.artists.len(),
        SearchFocusState::Playlists => results.playlists.len(),
        SearchFocusState::Shows => results.podcasts.len(),
        SearchFocusState::Episodes => results.episodes.len(),
    };
    let selected = ui.current_page().selected_index().unwrap_or_default();
    let count = ui.count_prefix;

    if command == Command::ChooseSelected {
        if let Some(tracks) = tracks.filter(|tracks| tracks.get(selected).is_some()) {
            client_pub.send(ClientRequest::PlayYouTubeContext {
                tracks,
                start_index: selected,
            })?;
            window::clear_track_selection(ui);
            return Ok(true);
        }
        let context_id = match focus {
            SearchFocusState::Albums => results
                .albums
                .get(selected)
                .map(|album| YouTubeContextId::Album(album.id.clone())),
            SearchFocusState::Artists => results
                .artists
                .get(selected)
                .map(|artist| YouTubeContextId::Artist(artist.id.clone())),
            SearchFocusState::Playlists => results
                .playlists
                .get(selected)
                .map(|playlist| YouTubeContextId::Playlist(playlist.id.clone())),
            SearchFocusState::Shows => results
                .podcasts
                .get(selected)
                .map(|podcast| YouTubeContextId::Podcast(podcast.id.clone())),
            _ => None,
        };
        if let Some(context_id) = context_id {
            ui.new_page(PageState::YouTubeContext {
                id: context_id.clone(),
                context: None,
                state: YouTubeContextPageUIState::new(),
            });
            client_pub.send(ClientRequest::GetYouTubeContext(context_id))?;
            return Ok(true);
        }
        return Ok(false);
    }

    if command == Command::AddSelectedItemToQueue {
        let Some(tracks) = tracks.as_ref() else {
            return Ok(false);
        };
        let selected_tracks = selected_youtube_tracks_for_action(ui, tracks, selected);
        if !selected_tracks.is_empty() {
            if selected_tracks.len() > 1 {
                let epoch = current_youtube_epoch(ui);
                dispatch_youtube_queue_tracks(ui, client_pub, selected_tracks, epoch)?;
            } else {
                client_pub.send(ClientRequest::AddItemsToUserQueue(
                    selected_tracks
                        .into_iter()
                        .map(crate::state::PlayableMedia::YouTube)
                        .collect(),
                ))?;
            }
            window::clear_track_selection(ui);
            return Ok(true);
        }
        return Ok(false);
    }

    if command == Command::ShowActionsOnSelectedItem {
        let Some(tracks) = tracks.as_ref() else {
            ui.set_unsupported_operation(
                "Actions are unavailable for this YouTube result type.",
                "Press Enter to open the selected album, artist, playlist, or podcast.",
            );
            return Ok(true);
        };
        let selected_tracks = selected_youtube_tracks_for_action(ui, tracks, selected);
        let item = match selected_tracks.as_slice() {
            [] => return Ok(false),
            [track] => ActionListItem::YouTubeTrack(
                track.clone(),
                crate::command::construct_youtube_track_actions(),
            ),
            _ => {
                let actions = crate::command::construct_youtube_track_actions();
                let Ok(menu) = youtube_bulk_action_menu(
                    &selected_tracks,
                    &actions,
                    current_youtube_epoch(ui).value(),
                ) else {
                    return Ok(false);
                };
                ActionListItem::YouTubeTracks(menu)
            }
        };
        ui.popup = Some(PopupState::ActionList(Box::new(item), ListState::default()));
        return Ok(true);
    }

    if matches!(
        command,
        Command::ExtendSelectionNext | Command::ExtendSelectionPrevious
    ) {
        let Some(tracks) = tracks.as_ref() else {
            return Ok(false);
        };
        if tracks.is_empty() {
            return Ok(false);
        }
        let direction = if command == Command::ExtendSelectionNext {
            1
        } else {
            -1
        };
        return Ok(window::extend_track_selection(
            ui,
            selected,
            tracks.len(),
            count.unwrap_or(1),
            direction,
        ));
    }

    let handled = handle_navigation_command(command, ui.current_page_mut(), selected, len, count);
    if handled {
        window::clear_track_selection(ui);
    }
    Ok(handled)
}

fn selected_youtube_tracks_for_action(
    ui: &UIStateGuard,
    tracks: &[YouTubeTrack],
    fallback_index: usize,
) -> Vec<YouTubeTrack> {
    let indices = match ui.current_page() {
        PageState::Search { state, .. }
            if matches!(
                state.focus,
                SearchFocusState::Tracks | SearchFocusState::Videos
            ) =>
        {
            state
                .search_selection
                .selected_or_cursor(Some(fallback_index))
                .unwrap_or_default()
        }
        PageState::YouTubeContext {
            id: context_id @ YouTubeContextId::Playlist(_),
            ..
        } => {
            let context = YouTubeContext {
                title: context_id.title().to_owned(),
                tracks: tracks.to_vec(),
                ..YouTubeContext::default()
            };
            let epoch = ui.provider_selection_epoch(config::ActiveProvider::YouTubeMusic);
            let Some(snapshot) = PlaylistSnapshot::from_youtube_playlist(
                context_id,
                &context,
                epoch,
                crate::state::UiViewStatus::Ready,
                PlaylistCapabilities::read_only("Selection projection is read-only."),
                PlaylistActionModel::default(),
                |_| crate::command::construct_youtube_track_actions(),
            ) else {
                return Vec::new();
            };
            let filter = match ui.popup.as_ref() {
                Some(PopupState::Search { query }) => Some(query.as_str()),
                _ => None,
            };
            let visible = snapshot.visible_indices(filter);
            let visible_indices = ui
                .current_page()
                .mutable_playlist_state()
                .map(|state| state.selection().selected_visible_indices())
                .filter(|indices| !indices.is_empty())
                .unwrap_or_else(|| vec![fallback_index]);
            visible_indices
                .into_iter()
                .filter_map(|index| visible.get(index).copied())
                .collect()
        }
        PageState::YouTubeContext { .. } => ui
            .current_page()
            .youtube_context_track_selection()
            .and_then(|selection| {
                youtube_context_selected_or_cursor_indices(selection, fallback_index).ok()
            })
            .unwrap_or_default(),
        _ => return tracks.get(fallback_index).cloned().into_iter().collect(),
    };

    indices
        .into_iter()
        .filter_map(|index| tracks.get(index).cloned())
        .collect()
}

fn youtube_search_action_is_supported(action: Action) -> bool {
    crate::command::provider_capabilities(config::ActiveProvider::YouTubeMusic)
        .supports_track_action(action)
}

fn synchronize_spotify_search_selection(ui: &mut UIStateGuard, query: &str, tracks: &[&Track]) {
    synchronize_search_selection(
        ui,
        config::ActiveProvider::Spotify,
        query,
        SearchPane::SpotifyTracks,
        tracks.iter().map(|track| track.id.uri()).collect(),
    );
}

fn synchronize_youtube_search_selection(
    ui: &mut UIStateGuard,
    query: &str,
    focus: SearchFocusState,
    results: &crate::state::YouTubeSearchResults,
) {
    let (pane, ids) = match focus {
        SearchFocusState::Tracks => (
            SearchPane::YouTubeSongs,
            results.songs.iter().map(|track| track.id.clone()).collect(),
        ),
        SearchFocusState::Videos => (
            SearchPane::YouTubeVideos,
            results
                .videos
                .iter()
                .map(|track| track.id.clone())
                .collect(),
        ),
        SearchFocusState::Category
        | SearchFocusState::Input
        | SearchFocusState::Albums
        | SearchFocusState::Artists
        | SearchFocusState::Playlists
        | SearchFocusState::Shows
        | SearchFocusState::Episodes => return,
    };
    synchronize_search_selection(ui, config::ActiveProvider::YouTubeMusic, query, pane, ids);
}

fn synchronize_search_selection(
    ui: &mut UIStateGuard,
    provider: config::ActiveProvider,
    query: &str,
    pane: SearchPane,
    ids: Vec<String>,
) {
    let provider_epoch = ui.provider_selection_epoch(provider);
    if let PageState::Search { state, .. } = ui.current_page_mut() {
        if let Err(error) = state
            .search_selection
            .synchronize(SearchScope::new(provider, provider_epoch, query, pane), ids)
        {
            tracing::warn!(
                selection_error = ?error,
                "Search selection projection could not be synchronized"
            );
            state.search_selection.clear_selection();
        }
    }
}

fn handle_search_selection_command(command: Command, ui: &mut UIStateGuard) -> bool {
    let keyed = matches!(
        ui.current_page(),
        PageState::Search {
            state: SearchPageUIState {
                focus: SearchFocusState::Tracks | SearchFocusState::Videos,
                ..
            },
            ..
        }
    );
    if !keyed || !is_selection_command(command) {
        return false;
    }
    handle_page_selection_command(command, ui, None)
}

fn handle_command_for_context_page(
    command: Command,
    client_pub: &crate::client::ClientRequestSender,
    ui: &mut UIStateGuard,
    state: &SharedState,
) -> Result<bool> {
    match command {
        Command::Search => {
            ui.new_search_popup();
            Ok(true)
        }
        Command::ShowActionsOnCurrentContext => {
            let context_id = match ui.current_page() {
                PageState::Context { id, .. } => match id {
                    None => return Ok(false),
                    Some(id) => id,
                },
                _ => anyhow::bail!("expect a context page"),
            };
            let data = state.data.read();

            match data.caches.context.get(&context_id.uri()) {
                Some(context) => match context {
                    Context::Playlist { playlist, .. } => {
                        Ok(open_spotify_playlist_context_actions(playlist, &data, ui))
                    }
                    Context::Album { album, .. } => {
                        let actions = construct_album_actions(album, &data);
                        ui.popup = Some(PopupState::ActionList(
                            Box::new(ActionListItem::Album(album.clone(), actions)),
                            ListState::default(),
                        ));
                        Ok(true)
                    }
                    Context::Artist { artist, .. } => {
                        let actions = construct_artist_actions(artist, &data);
                        ui.popup = Some(PopupState::ActionList(
                            Box::new(ActionListItem::Artist(artist.clone(), actions)),
                            ListState::default(),
                        ));
                        Ok(true)
                    }
                    Context::Show { show, .. } => {
                        let actions = construct_show_actions(show, &data);
                        ui.popup = Some(PopupState::ActionList(
                            Box::new(ActionListItem::Show(show.clone(), actions)),
                            ListState::default(),
                        ));
                        Ok(true)
                    }
                    Context::Tracks { tracks: _, desc: _ } => Ok(false),
                },
                None => Ok(false),
            }
        }
        _ => window::handle_command_for_focused_context_window(command, client_pub, ui, state),
    }
}

fn handle_action_for_browse_page(
    action: Action,
    client_pub: &crate::client::ClientRequestSender,
    ui: &mut UIStateGuard,
    state: &SharedState,
) -> Result<bool> {
    let data = state.data.read();
    let selected = ui.current_page().selected_index().unwrap_or_default();

    match ui.current_page() {
        PageState::Browse { state } => match state {
            BrowsePageUIState::CategoryPlaylistList { category, .. } => {
                let Some(playlists) = data.browse.category_playlists.get(&category.id) else {
                    return Ok(false);
                };
                let playlists = ui.search_filtered_items(playlists);

                if selected >= playlists.len() {
                    return Ok(false);
                }

                handle_action_in_context(
                    action,
                    playlists[selected].clone().into(),
                    client_pub,
                    &data,
                    ui,
                )?;

                Ok(true)
            }
            BrowsePageUIState::CategoryList { .. } => Ok(false),
        },
        _ => anyhow::bail!("expect a browse page state"),
    }
}

#[cfg(test)]
mod upstream_browse_selection_tests {
    use super::*;

    #[test]
    fn browse_workspace_mouse_hit_selects_the_rendered_row() -> anyhow::Result<()> {
        crate::ui::initialize_test_config();
        let ring = std::sync::Arc::new(parking_lot::Mutex::new(std::collections::VecDeque::new()));
        let (diagnostics, _runtime) = crate::observability::disabled(ring);
        let state = std::sync::Arc::new(crate::state::State::new(false, diagnostics));
        state.data.write().browse.categories_loaded = true;
        state.data.write().browse.categories = vec![crate::state::Category {
            id: "mood".to_owned(),
            name: "Mood".to_owned(),
        }];
        let (sender, receiver) = crate::client::client_request_channel();
        let mut ui = state.ui.lock();
        ui.history = vec![PageState::Browse {
            state: BrowsePageUIState::CategoryList {
                state: ratatui::widgets::ListState::default(),
            },
        }];
        let row = ratatui::layout::Rect::new(30, 8, 40, 1);
        ui.workspace_hits.push((row, WorkspaceHit::BrowseRow(0)));

        assert!(handle_workspace_mouse_hit(
            WorkspaceHit::BrowseRow(0),
            false,
            row.x,
            row.y,
            &sender,
            &state,
            &mut ui,
        )?);
        assert_eq!(ui.current_page().selected_index(), Some(0));
        assert!(receiver.try_recv().is_err());
        Ok(())
    }

    #[test]
    fn filtered_browse_action_targets_visible_playlist_and_rejects_stale_selection() {
        crate::ui::initialize_test_config();
        let ring = std::sync::Arc::new(parking_lot::Mutex::new(std::collections::VecDeque::new()));
        let (diagnostics, _runtime) = crate::observability::disabled(ring);
        let state = std::sync::Arc::new(crate::state::State::new(false, diagnostics));
        let playlist = |name: &str, id: &'static str| crate::state::Playlist {
            id: rspotify::model::PlaylistId::from_id(id)
                .unwrap()
                .into_static(),
            name: name.to_owned(),
            collaborative: false,
            owner: (
                "Owner".to_owned(),
                rspotify::model::UserId::from_id("owner")
                    .unwrap()
                    .into_static(),
            ),
            desc: String::new(),
            current_folder_id: 0,
            snapshot_id: String::new(),
        };
        let wanted = playlist("Wanted", "wanted");
        state.data.write().browse.category_playlists.insert(
            "category".to_owned(),
            vec![playlist("Other", "other"), wanted.clone()],
        );
        let mut ui = state.ui.lock();
        ui.history = vec![PageState::Browse {
            state: BrowsePageUIState::CategoryPlaylistList {
                category: crate::state::Category {
                    id: "category".to_owned(),
                    name: "Category".to_owned(),
                },
                state: ratatui::widgets::ListState::default().with_selected(Some(0)),
            },
        }];
        ui.popup = Some(PopupState::Search {
            query: "Wanted".to_owned(),
        });
        let (sender, receiver) = crate::client::client_request_channel();
        assert!(
            handle_action_for_browse_page(Action::AddToLibrary, &sender, &mut ui, &state).unwrap()
        );
        let request = receiver.try_recv().unwrap();
        let ClientRequest::AddToLibrary(Item::Playlist(actual)) = request.request() else {
            panic!("expected playlist action")
        };
        assert_eq!(actual.id, wanted.id);
        ui.popup = Some(PopupState::Search {
            query: "Wanted".to_owned(),
        });
        ui.current_page_mut().select(1);
        assert!(
            !handle_action_for_browse_page(Action::AddToLibrary, &sender, &mut ui, &state).unwrap()
        );
        assert!(receiver.try_recv().is_err());
        ui.current_page_mut().select(0);
        ui.popup = Some(PopupState::Search {
            query: "No match".to_owned(),
        });
        assert!(
            !handle_action_for_browse_page(Action::AddToLibrary, &sender, &mut ui, &state).unwrap()
        );
        assert!(receiver.try_recv().is_err());
    }
}

fn handle_command_for_browse_page(
    command: Command,
    client_pub: &crate::client::ClientRequestSender,
    ui: &mut UIStateGuard,
    state: &SharedState,
) -> Result<bool> {
    let data = state.data.read();

    let len = match ui.current_page() {
        PageState::Browse { state } => match state {
            BrowsePageUIState::CategoryList { .. } => {
                ui.search_filtered_item_count(&data.browse.categories)
            }
            BrowsePageUIState::CategoryPlaylistList { category, .. } => data
                .browse
                .category_playlists
                .get(&category.id)
                .map(|v| ui.search_filtered_item_count(v))
                .unwrap_or_default(),
        },
        _ => anyhow::bail!("expect a browse page state"),
    };

    let count = ui.count_prefix;
    let selected = ui.current_page().selected_index().unwrap_or_default();
    let page_state = ui.current_page_mut();
    if selected >= len {
        return Ok(false);
    }

    if handle_navigation_command(command, page_state, selected, len, count) {
        return Ok(true);
    }
    match command {
        Command::ChooseSelected => match page_state {
            PageState::Browse { state } => match state {
                BrowsePageUIState::CategoryList { .. } => {
                    let categories = ui.search_filtered_items(&data.browse.categories);
                    client_pub.send(ClientRequest::GetBrowseCategoryPlaylists(
                        categories[selected].clone(),
                    ))?;
                    ui.new_page(PageState::Browse {
                        state: BrowsePageUIState::CategoryPlaylistList {
                            category: categories[selected].clone(),
                            state: ListState::default(),
                        },
                    });
                }
                BrowsePageUIState::CategoryPlaylistList { category, .. } => {
                    let playlists =
                        data.browse
                            .category_playlists
                            .get(&category.id)
                            .context(format!(
                                "expect to have playlists data for {category} category"
                            ))?;
                    let context_id = ContextId::Playlist(
                        ui.search_filtered_items(playlists)[selected].id.clone(),
                    );
                    ui.new_page(PageState::Context {
                        id: None,
                        context_page_type: ContextPageType::Browsing(context_id),
                        state: None,
                    });
                }
            },
            _ => anyhow::bail!("expect a browse page state"),
        },
        Command::Search => {
            ui.new_search_popup();
        }
        _ => return Ok(false),
    }
    Ok(true)
}

fn queue_action_menu_for_indices(
    scope: QueueSelectionScope,
    items: &[QueueDisplayItem],
    indices: &[usize],
) -> Option<QueueActionMenu> {
    let action_items = indices
        .iter()
        .filter_map(|index| items.get(*index).and_then(queue_action_item_for_row))
        .collect::<Vec<_>>();
    (action_items.len() == indices.len() && !action_items.is_empty())
        .then(|| QueueActionMenu::new(scope, action_items))
}

fn handle_command_for_queue_page(
    command: Command,
    client_pub: &crate::client::ClientRequestSender,
    state: &SharedState,
    ui: &mut UIStateGuard,
) -> Result<bool> {
    let Some((scope, items)) = synchronize_queue_page_selection(ui, state) else {
        return Ok(false);
    };
    if is_selection_command(command) {
        return Ok(handle_page_selection_command(command, ui, None));
    }
    let selected = ui.current_page().selected_index().unwrap_or_default();
    let selection_status = ui
        .current_page()
        .queue_selection()
        .map_or(ScopedSelectionStatus::Unscoped, |selection| {
            selection.status()
        });
    let selected_indices = ui
        .current_page()
        .queue_selection()
        .and_then(|selection| queue_selected_or_cursor_indices(selection, selected).ok())
        .unwrap_or_default();

    if command == Command::JumpToCurrentTrackInContext {
        let Some(current) = items.iter().position(QueueDisplayItem::is_current) else {
            return Ok(false);
        };
        ui.current_page_mut().select(current);
        return Ok(true);
    }

    if matches!(
        command,
        Command::ExtendSelectionNext | Command::ExtendSelectionPrevious
    ) {
        let offset = ui.count_prefix.unwrap_or(1);
        let direction = if command == Command::ExtendSelectionPrevious {
            -1isize
        } else {
            1isize
        };
        return Ok(window::extend_track_selection(
            ui,
            selected,
            items.len(),
            offset,
            direction,
        ));
    }

    if command == Command::ShowActionsOnSelectedItem {
        if selected_indices.len() > 1 && selection_status != ScopedSelectionStatus::Ambiguous {
            let Some(menu) = queue_action_menu_for_indices(scope, &items, &selected_indices) else {
                return Ok(false);
            };
            ui.popup = Some(PopupState::ActionList(
                Box::new(ActionListItem::Queue(menu)),
                ListState::default(),
            ));
            return Ok(true);
        }
        let action_index = selected_indices.first().copied().unwrap_or(selected);
        let Some(item) = items.get(action_index) else {
            return Ok(false);
        };
        let data = state.data.read();
        let Some(action_item) = queue_action_list_item(item, &data, ui.active_provider) else {
            ui.set_unsupported_operation(
                "Queue item actions are unavailable.",
                "Switch provider or open the item from its source page.",
            );
            return Ok(true);
        };
        ui.popup = Some(PopupState::ActionList(
            Box::new(action_item),
            ListState::default(),
        ));
        return Ok(true);
    }

    if command == Command::AddSelectedItemToQueue {
        if selected_indices.len() > 1 && selection_status != ScopedSelectionStatus::Ambiguous {
            let Some(menu) =
                queue_action_menu_for_indices(scope.clone(), &items, &selected_indices)
            else {
                return Ok(false);
            };
            let current_items = menu.items().to_vec();
            let Ok(plan) = replan_queue_menu(&menu, &scope, &current_items) else {
                return Ok(false);
            };
            if dispatch_queue_menu(&menu, &plan, ui, client_pub).is_ok() {
                window::clear_track_selection(ui);
                return Ok(true);
            }
            return Ok(false);
        }
        let Some(index) = selected_indices.first().copied() else {
            return Ok(false);
        };
        let Some(item) = items.get(index).and_then(queue_action_item_for_row) else {
            return Ok(false);
        };
        match item.payload() {
            QueueActionPayload::Unified(media) => {
                if client_pub
                    .send(ClientRequest::AddItemsToUserQueue(vec![media.clone()]))
                    .is_err()
                {
                    return Ok(false);
                }
            }
            QueueActionPayload::Native(playable) => {
                if client_pub
                    .send(ClientRequest::AddPlayableToQueue(playable.clone()))
                    .is_err()
                {
                    return Ok(false);
                }
            }
        }
        window::clear_track_selection(ui);
        return Ok(true);
    }

    let count = ui.count_prefix;
    Ok(window::navigate_and_clear_selection(
        command,
        ui,
        selected,
        items.len(),
        count,
    ))
}

fn handle_action_for_queue_page(
    action: Action,
    client_pub: &crate::client::ClientRequestSender,
    state: &SharedState,
    ui: &mut UIStateGuard,
) -> Result<bool> {
    if action == Action::AddToQueue {
        return handle_command_for_queue_page(
            Command::AddSelectedItemToQueue,
            client_pub,
            state,
            ui,
        );
    }
    let selected = ui.current_page().selected_index().unwrap_or_default();
    let Some(item) = state
        .player
        .read()
        .queue_display_items()
        .get(selected)
        .cloned()
    else {
        return Ok(false);
    };
    let Some(context) = queue_action_context(&item, ui.active_provider) else {
        ui.set_unsupported_operation(
            "Queue item actions are unavailable.",
            "Switch provider or open the item from its source page.",
        );
        return Ok(true);
    };
    let data = state.data.read();
    handle_action_in_context(action, context, client_pub, &data, ui)
}

fn queue_action_context(
    item: &QueueDisplayItem,
    active_provider: config::ActiveProvider,
) -> Option<ActionContext> {
    match item {
        QueueDisplayItem::Spotify { item, .. } => {
            if active_provider != config::ActiveProvider::Spotify {
                return None;
            }
            match item.as_ref() {
                rspotify::model::PlayableItem::Track(track) => {
                    Track::try_from_full_track(track.clone()).map(ActionContext::Track)
                }
                rspotify::model::PlayableItem::Episode(episode) => {
                    Some(ActionContext::Episode(episode.clone().into()))
                }
                rspotify::model::PlayableItem::Unknown(_) => None,
            }
        }
        QueueDisplayItem::Unified { item, .. } => {
            if active_provider
                != match &item.media {
                    crate::state::PlayableMedia::Spotify(_) => config::ActiveProvider::Spotify,
                    crate::state::PlayableMedia::YouTube(_) => config::ActiveProvider::YouTubeMusic,
                }
            {
                return None;
            }
            match &item.media {
                crate::state::PlayableMedia::YouTube(track) => {
                    Some(ActionContext::YouTubeTrack(track.clone()))
                }
                crate::state::PlayableMedia::Spotify(_) => None,
            }
        }
    }
}

fn queue_action_list_item(
    item: &QueueDisplayItem,
    data: &DataReadGuard,
    active_provider: config::ActiveProvider,
) -> Option<ActionListItem> {
    match item {
        QueueDisplayItem::Spotify { item, .. } => {
            if active_provider != config::ActiveProvider::Spotify {
                return None;
            }
            match item.as_ref() {
                rspotify::model::PlayableItem::Track(track) => {
                    let track = Track::try_from_full_track(track.clone())?;
                    Some(ActionListItem::Track(
                        track.clone(),
                        command::construct_track_actions(&track, data),
                    ))
                }
                rspotify::model::PlayableItem::Episode(episode) => {
                    let episode: Episode = episode.clone().into();
                    Some(ActionListItem::Episode(
                        episode.clone(),
                        command::construct_episode_actions(&episode, data),
                    ))
                }
                rspotify::model::PlayableItem::Unknown(_) => None,
            }
        }
        QueueDisplayItem::Unified { item, .. } => {
            let matches_provider = match &item.media {
                crate::state::PlayableMedia::Spotify(_) => {
                    active_provider == config::ActiveProvider::Spotify
                }
                crate::state::PlayableMedia::YouTube(_) => {
                    active_provider == config::ActiveProvider::YouTubeMusic
                }
            };
            if !matches_provider {
                return None;
            }
            match &item.media {
                crate::state::PlayableMedia::YouTube(track) => Some(ActionListItem::YouTubeTrack(
                    track.clone(),
                    command::construct_youtube_track_actions(),
                )),
                crate::state::PlayableMedia::Spotify(_) => None,
            }
        }
    }
}

fn handle_action_for_journal_page(
    action: Action,
    client_pub: &crate::client::ClientRequestSender,
    ui: &mut UIStateGuard,
    state: &SharedState,
) -> Result<bool> {
    let Some((_, visible)) = journal_projection(ui, state) else {
        return Ok(false);
    };
    let selected_tracks = selected_journal_tracks_for_action(ui, &visible);
    if selected_tracks.is_empty() {
        return Ok(false);
    }

    if action == Action::RemoveFromJournal {
        let Some(track) = journal_single_track_for_action(ui, &visible) else {
            return Ok(false);
        };
        return handle_safe_journal_destructive_action(action, track, state, ui);
    }
    if action == Action::RemoveFromJournalList {
        return Ok(false);
    }
    if action == Action::AddToListenLater && selected_tracks.len() > 1 {
        return handle_bulk_add_to_listen_later(selected_tracks, state, ui);
    }
    if is_track_journal_action(action) {
        let Some(track) = journal_single_track_for_action(ui, &visible) else {
            return Ok(false);
        };
        return handle_track_journal_action(action, track, state, ui);
    }

    let data = state.data.read();
    let context = match selected_tracks.as_slice() {
        [track] => ActionContext::Track(track.clone()),
        tracks => ActionContext::Tracks(tracks.to_vec()),
    };
    handle_action_in_context(action, context, client_pub, &data, ui)
}

fn handle_command_for_journal_page(
    command: Command,
    client_pub: &crate::client::ClientRequestSender,
    ui: &mut UIStateGuard,
    state: &SharedState,
) -> Result<bool> {
    if command == Command::Search {
        ui.new_search_popup();
        return Ok(true);
    }

    let Some((_, filtered_entries)) = journal_projection(ui, state) else {
        return Ok(false);
    };
    if is_selection_command(command) {
        return Ok(handle_page_selection_command(command, ui, None));
    }
    let selected = ui.current_page().diagnostic_selection().unwrap_or_default();
    let selected_tracks = selected_journal_tracks_for_action(ui, &filtered_entries);

    let count = ui.count_prefix;
    match command {
        Command::ExtendSelectionNext => {
            return Ok(window::extend_track_selection(
                ui,
                selected,
                filtered_entries.len(),
                count.unwrap_or(1),
                1,
            ));
        }
        Command::ExtendSelectionPrevious => {
            return Ok(window::extend_track_selection(
                ui,
                selected,
                filtered_entries.len(),
                count.unwrap_or(1),
                -1,
            ));
        }
        _ => {}
    }

    if handle_navigation_command(
        command,
        ui.current_page_mut(),
        selected,
        filtered_entries.len(),
        count,
    ) {
        window::clear_track_selection(ui);
        return Ok(true);
    }

    match command {
        Command::ChooseSelected => {
            let Some(track) = filtered_entries
                .get(selected)
                .map(|entry| entry.track.clone())
            else {
                return Ok(false);
            };
            state.player.write().currently_playing_tracks_id = None;
            client_pub.send(ClientRequest::Player(PlayerRequest::StartPlayback(
                Playback::URIs(vec![track.id.clone().into()], None),
                None,
            )))?;
        }
        Command::ShowActionsOnSelectedItem => {
            let Some(track) = selected_tracks.first().cloned() else {
                return Ok(false);
            };
            let data = state.data.read();
            if selected_tracks.len() > 1 {
                let actions = window::construct_tracks_actions(ui);
                let Ok(menu) = spotify_bulk_action_menu(
                    &selected_tracks,
                    &actions,
                    current_spotify_epoch(ui).value(),
                ) else {
                    return Ok(false);
                };
                ui.popup = Some(PopupState::ActionList(
                    Box::new(ActionListItem::Tracks(menu)),
                    ListState::default(),
                ));
            } else {
                let actions = command::construct_track_actions(&track, &data);
                ui.popup = Some(PopupState::ActionList(
                    Box::new(ActionListItem::Track(track, actions)),
                    ListState::default(),
                ));
            }
        }
        Command::AddSelectedItemToQueue => {
            if selected_tracks.len() > 1 {
                let epoch = current_spotify_epoch(ui);
                dispatch_spotify_native_queue_tracks(ui, client_pub, selected_tracks, epoch)?;
                window::clear_track_selection(ui);
            } else if let Some(track) = selected_tracks.first() {
                ui.spotify_queue_labels.remember_track(track);
                client_pub.send(ClientRequest::AddPlayableToQueue(track.id.clone().into()))?;
            } else {
                return Ok(false);
            }
        }
        _ => return Ok(false),
    }

    Ok(true)
}

fn handle_command_for_journal_lists_page(
    command: Command,
    ui: &mut UIStateGuard,
    state: &SharedState,
) -> Result<bool> {
    if command == Command::Search {
        ui.new_search_popup();
        return Ok(true);
    }
    if command == Command::CreatePlaylist {
        ui.popup = Some(PopupState::JournalListName {
            action: JournalListNameAction::Create,
            input: LineInput::default(),
        });
        return Ok(true);
    }

    let selected = ui.current_page().selected_index().unwrap_or_default();
    let list_ids = {
        let data = state.data.read();
        ui.search_filtered_items(&data.journal.lists)
            .into_iter()
            .map(|list| list.id.clone())
            .collect::<Vec<_>>()
    };
    if selected >= list_ids.len() {
        return Ok(false);
    }

    match command {
        Command::MovePlaylistItemUp | Command::MovePlaylistItemDown => {
            let offset = if command == Command::MovePlaylistItemUp {
                -1
            } else {
                1
            };
            let list_id = list_ids[selected].clone();
            let mut selected_after_move = None;
            update_track_journal(state, |journal| {
                if let Some(index) = journal.lists.iter().position(|list| list.id == list_id) {
                    selected_after_move = journal.move_list(index, offset);
                }
            })?;
            if let Some(new_index) = selected_after_move {
                ui.current_page_mut().select(new_index);
            }
            Ok(true)
        }
        Command::RenameJournalList => {
            let data = state.data.read();
            let Some(list) = data.journal.list(&list_ids[selected]) else {
                return Ok(false);
            };
            ui.popup = Some(PopupState::JournalListName {
                action: JournalListNameAction::Rename {
                    list_id: list.id.clone(),
                },
                input: LineInput::new(list.name.chars().collect()),
            });
            Ok(true)
        }
        Command::DeleteJournalList => {
            let data = state.data.read();
            let Some(list) = data.journal.list(&list_ids[selected]) else {
                return Ok(false);
            };
            ui.popup = Some(PopupState::ConfirmAction {
                message: format!("Delete journal list {}?", list.name),
                action: ConfirmableAction::DeleteJournalList(list.id.clone()),
            });
            Ok(true)
        }
        Command::ChooseSelected => {
            ui.new_page(PageState::JournalList {
                list_id: list_ids[selected].clone(),
                table: ratatui::widgets::TableState::default(),
                journal_selection: crate::state::JournalSelection::default(),
            });
            Ok(true)
        }
        _ => {
            let count = ui.count_prefix;
            Ok(handle_navigation_command(
                command,
                ui.current_page_mut(),
                selected,
                list_ids.len(),
                count,
            ))
        }
    }
}

fn handle_action_for_journal_list_page(
    action: Action,
    client_pub: &crate::client::ClientRequestSender,
    ui: &mut UIStateGuard,
    state: &SharedState,
) -> Result<bool> {
    let Some((_, _, _, visible)) = journal_list_projection(ui, state) else {
        return Ok(false);
    };
    let selected_tracks = selected_journal_tracks_for_action(ui, &visible);
    if selected_tracks.is_empty() {
        return Ok(false);
    }

    if action == Action::RemoveFromJournalList {
        let Some(track) = journal_single_track_for_action(ui, &visible) else {
            return Ok(false);
        };
        return handle_safe_journal_destructive_action(action, track, state, ui);
    }
    if action == Action::RemoveFromJournal {
        let Some(track) = journal_single_track_for_action(ui, &visible) else {
            return Ok(false);
        };
        return handle_safe_journal_destructive_action(action, track, state, ui);
    }
    if action == Action::AddToListenLater && selected_tracks.len() > 1 {
        return handle_bulk_add_to_listen_later(selected_tracks, state, ui);
    }
    if is_track_journal_action(action) {
        let Some(track) = journal_single_track_for_action(ui, &visible) else {
            return Ok(false);
        };
        return handle_track_journal_action(action, track, state, ui);
    }

    let data = state.data.read();
    let context = match selected_tracks.as_slice() {
        [track] => ActionContext::Track(track.clone()),
        tracks => ActionContext::Tracks(tracks.to_vec()),
    };
    handle_action_in_context(action, context, client_pub, &data, ui)
}

fn handle_command_for_journal_list_page(
    command: Command,
    client_pub: &crate::client::ClientRequestSender,
    ui: &mut UIStateGuard,
    state: &SharedState,
) -> Result<bool> {
    if command == Command::Search {
        ui.new_search_popup();
        return Ok(true);
    }

    let Some((list_id, complete_uris, _, filtered_entries)) = journal_list_projection(ui, state)
    else {
        return Ok(false);
    };
    if is_selection_command(command) {
        return Ok(handle_page_selection_command(command, ui, None));
    }
    let selected = ui.current_page().diagnostic_selection().unwrap_or_default();
    let len = filtered_entries.len();
    let selected_tracks = selected_journal_tracks_for_action(ui, &filtered_entries);

    match command {
        Command::ExtendSelectionNext => {
            let count = ui.count_prefix;
            Ok(window::extend_track_selection(
                ui,
                selected,
                len,
                count.unwrap_or(1),
                1,
            ))
        }
        Command::ExtendSelectionPrevious => {
            let count = ui.count_prefix;
            Ok(window::extend_track_selection(
                ui,
                selected,
                len,
                count.unwrap_or(1),
                -1,
            ))
        }
        Command::MovePlaylistItemUp | Command::MovePlaylistItemDown => {
            let offset = if command == Command::MovePlaylistItemUp {
                -1
            } else {
                1
            };
            let Some(selection) = ui.current_page().journal_list_selection() else {
                return Ok(false);
            };
            let Some((full_index, target_uri)) =
                journal_list_move_target(selection, &complete_uris, selected)
            else {
                return Ok(false);
            };
            let captured_uris = complete_uris.clone();
            let mut moved = false;
            update_track_journal(state, |journal| {
                let Some(list) = journal.list(&list_id) else {
                    return;
                };
                if !journal_list_target_is_current(
                    &list.track_uris,
                    &captured_uris,
                    full_index,
                    &target_uri,
                ) {
                    return;
                }
                moved = journal
                    .move_track_in_list(&list_id, full_index, offset)
                    .is_some();
            })?;
            if !moved {
                return Ok(false);
            }
            let Some((_, _, _, fresh_visible)) = journal_list_projection(ui, state) else {
                return Ok(false);
            };
            let fresh_visible_uris = fresh_visible
                .iter()
                .map(|entry| entry.track.id.uri())
                .collect::<Vec<_>>();
            if let Some(new_index) = journal_list_cursor_for_uri(&fresh_visible_uris, &target_uri) {
                ui.current_page_mut().select(new_index);
            }
            Ok(true)
        }
        Command::DeleteJournalList => {
            if selected_tracks.len() != 1 {
                return Ok(false);
            }
            let track = selected_tracks[0].clone();
            handle_safe_journal_destructive_action(Action::RemoveFromJournalList, track, state, ui)
        }
        Command::ChooseSelected => {
            let Some(track) = filtered_entries
                .get(selected)
                .map(|entry| entry.track.clone())
            else {
                return Ok(false);
            };
            state.player.write().currently_playing_tracks_id = None;
            client_pub.send(ClientRequest::Player(PlayerRequest::StartPlayback(
                Playback::URIs(vec![track.id.clone().into()], None),
                None,
            )))?;
            Ok(true)
        }
        Command::ShowActionsOnSelectedItem => {
            let Some(track) = selected_tracks.first().cloned() else {
                return Ok(false);
            };
            let data = state.data.read();
            if selected_tracks.len() > 1 {
                let actions = window::construct_tracks_actions(ui);
                let Ok(menu) = spotify_bulk_action_menu(
                    &selected_tracks,
                    &actions,
                    current_spotify_epoch(ui).value(),
                ) else {
                    return Ok(false);
                };
                ui.popup = Some(PopupState::ActionList(
                    Box::new(ActionListItem::Tracks(menu)),
                    ListState::default(),
                ));
            } else {
                let actions = command::construct_track_actions(&track, &data);
                ui.popup = Some(PopupState::ActionList(
                    Box::new(ActionListItem::Track(track, actions)),
                    ListState::default(),
                ));
            }
            Ok(true)
        }
        Command::AddSelectedItemToQueue => {
            if selected_tracks.len() > 1 {
                let epoch = current_spotify_epoch(ui);
                dispatch_spotify_native_queue_tracks(ui, client_pub, selected_tracks, epoch)?;
                window::clear_track_selection(ui);
            } else if let Some(track) = selected_tracks.first() {
                ui.spotify_queue_labels.remember_track(track);
                client_pub.send(ClientRequest::AddPlayableToQueue(track.id.clone().into()))?;
            } else {
                return Ok(false);
            }
            Ok(true)
        }
        _ => {
            let count = ui.count_prefix;
            Ok(window::navigate_and_clear_selection(
                command, ui, selected, len, count,
            ))
        }
    }
}

fn handle_command_for_command_help_page(command: Command, ui: &mut UIStateGuard) -> bool {
    let scroll_offset = match ui.current_page() {
        PageState::CommandHelp { scroll_offset } => *scroll_offset,
        _ => return false,
    };
    if command == Command::Search {
        ui.new_search_popup();
        return true;
    }
    let count = ui.count_prefix;
    handle_navigation_command(command, ui.current_page_mut(), scroll_offset, 10000, count)
}

pub(super) fn handle_command_for_lyrics_page(
    command: Command,
    client_pub: &crate::client::ClientRequestSender,
    state: &SharedState,
    ui: &mut UIStateGuard,
) -> Result<bool> {
    if command == Command::ShowActionsOnSelectedItem {
        let (track_uri, has_lyrics, has_timestamps) = match ui.current_page() {
            PageState::Lyrics {
                track_uri,
                lyrics_provider,
                ..
            } => {
                let data = state.data.read();
                let key = crate::state::LyricsCacheKey::new(track_uri, lyrics_provider.as_deref());
                let lyrics = data
                    .caches
                    .lyrics
                    .get(&key)
                    .and_then(|lyrics| lyrics.as_ref());
                (
                    track_uri.clone(),
                    lyrics.is_some(),
                    matches!(
                        lyrics.map(|lyrics| &lyrics.lines),
                        Some(crate::state::LyricsLines::Synced(_))
                    ),
                )
            }
            _ => return Ok(false),
        };
        ui.popup = Some(PopupState::ActionList(
            Box::new(ActionListItem::Lyrics(LyricsActionMenu::new(
                track_uri,
                has_lyrics,
                has_timestamps,
            ))),
            ratatui::widgets::ListState::default(),
        ));
        return Ok(true);
    }
    if matches!(command, Command::RetryLyrics | Command::CycleLyricsSource) {
        let (track_uri, youtube_track, current_provider) = match ui.current_page() {
            PageState::Lyrics {
                track_uri,
                youtube_track,
                lyrics_provider,
                ..
            } => (
                track_uri.clone(),
                youtube_track.clone(),
                lyrics_provider.clone(),
            ),
            _ => return Ok(false),
        };
        let provider = if command == Command::CycleLyricsSource {
            let cache_key =
                crate::state::LyricsCacheKey::new(&track_uri, current_provider.as_deref());
            let current_source = state
                .data
                .read()
                .caches
                .lyrics
                .get(&cache_key)
                .and_then(|lyrics| lyrics.as_ref().map(|lyrics| lyrics.source.clone()));
            next_lyrics_provider(
                current_provider.as_deref().or_else(|| {
                    current_source
                        .as_deref()
                        .and_then(lyrics_provider_id_from_label)
                }),
                &config::get_config().app_config.lyrics,
            )
        } else {
            current_provider
        };
        let request = lyrics_retry_request(&track_uri, youtube_track, provider.clone());
        let Some(request) = request else {
            return Ok(false);
        };
        let cache_key = crate::state::LyricsCacheKey::new(&track_uri, provider.as_deref());
        state.data.write().caches.lyrics.remove(&cache_key);
        if let PageState::Lyrics {
            status,
            lyrics_provider,
            ..
        } = ui.current_page_mut()
        {
            *lyrics_provider = provider;
            *status = crate::state::UiViewStatus::Loading;
        }
        client_pub.send(request)?;
        return Ok(true);
    }

    if command == Command::ToggleLyricsFollow {
        return Ok(ui.current_page_mut().toggle_lyrics_follow());
    }

    if !matches!(
        command,
        Command::SelectNextOrScrollDown
            | Command::SelectPreviousOrScrollUp
            | Command::PageSelectNextOrScrollDown
            | Command::PageSelectPreviousOrScrollUp
            | Command::SelectFirstOrScrollToTop
            | Command::SelectLastOrScrollToBottom
    ) {
        return Ok(false);
    }

    let (track_uri, source, scroll_offset) = match ui.current_page() {
        PageState::Lyrics {
            track_uri,
            lyrics_provider,
            scroll_offset,
            ..
        } => (track_uri.clone(), lyrics_provider.clone(), *scroll_offset),
        _ => return Ok(false),
    };
    let line_count = state
        .data
        .read()
        .caches
        .lyrics
        .get(&crate::state::LyricsCacheKey::new(
            &track_uri,
            source.as_deref(),
        ))
        .and_then(|lyrics| lyrics.as_ref())
        .map(|lyrics| match &lyrics.lines {
            crate::state::LyricsLines::Plain(lines) => lines.len(),
            crate::state::LyricsLines::Synced(lines) => lines.len(),
            crate::state::LyricsLines::Rich(lines) => lines.len(),
        })
        .unwrap_or_default();
    let count_prefix = ui.count_prefix;
    let handled = handle_navigation_command(
        command,
        ui.current_page_mut(),
        scroll_offset,
        line_count,
        count_prefix,
    );
    if handled {
        if let PageState::Lyrics {
            follow_playback, ..
        } = ui.current_page_mut()
        {
            *follow_playback = false;
        }
    }
    Ok(handled)
}

fn lyrics_retry_request(
    track_uri: &str,
    youtube_track: Option<YouTubeTrack>,
    provider: Option<String>,
) -> Option<ClientRequest> {
    match (youtube_track, provider) {
        (Some(track), Some(provider)) => {
            Some(ClientRequest::GetYouTubeLyricsFromProvider { track, provider })
        }
        (Some(track), None) => Some(ClientRequest::GetYouTubeLyrics(track)),
        (None, Some(provider)) => TrackId::from_uri(&parse_uri(track_uri))
            .ok()
            .map(|track_id| ClientRequest::GetLyricsFromProvider {
                track_id: track_id.into_static(),
                provider,
            }),
        (None, None) => TrackId::from_uri(&parse_uri(track_uri))
            .ok()
            .map(|track_id| ClientRequest::GetLyrics {
                track_id: track_id.into_static(),
            }),
    }
}

fn lyrics_provider_id_from_label(label: &str) -> Option<&'static str> {
    match label {
        "SimpMusic" => Some("simpmusic"),
        "LRCLIB" => Some("lrclib"),
        "Lyrics.ovh" => Some("lyricsovh"),
        "Musixmatch" => Some("musixmatch"),
        _ => None,
    }
}

fn next_lyrics_provider(current: Option<&str>, lyrics: &config::LyricsConfig) -> Option<String> {
    let providers = lyrics.enabled_provider_order();
    let next = current
        .and_then(|current| providers.iter().position(|provider| *provider == current))
        .map(|index| providers[(index + 1) % providers.len()])
        .or_else(|| providers.first().copied())?;
    Some(next.to_owned())
}

fn handle_command_for_logs_page(
    command: Command,
    app_state: &SharedState,
    ui: &mut UIStateGuard,
) -> bool {
    let Some(rows) = diagnostic_rows_for_logs_page(app_state, ui) else {
        return false;
    };
    #[cfg(feature = "private-capture")]
    let private_capture_snapshot = app_state.private_capture_operator_snapshot();
    let count = ui.count_prefix.unwrap_or(1);
    let PageState::Logs { state: page } = ui.current_page_mut() else {
        unreachable!("logs page changed while handling its command")
    };
    page.synchronize(&rows);
    if command == Command::ShowActionsOnSelectedItem {
        let Some(target) = page.selected_row.clone() else {
            return true;
        };
        #[cfg(feature = "private-capture")]
        let actions = if target == crate::observability::DiagnosticRowId::PrivateCapture {
            crate::observability::private_capture_actions(&private_capture_snapshot)
        } else {
            crate::observability::diagnostic_actions_for(
                &target,
                app_state.diagnostics.filter_snapshot().temporary,
                app_state.diagnostics.has_support_bundle(),
                page.support_focus_reference.is_some(),
            )
        };
        #[cfg(not(feature = "private-capture"))]
        let actions = crate::observability::diagnostic_actions_for(
            &target,
            app_state.diagnostics.filter_snapshot().temporary,
            app_state.diagnostics.has_support_bundle(),
            page.support_focus_reference.is_some(),
        );
        let mut list = ListState::default();
        list.select(Some(0));
        ui.popup = Some(PopupState::DiagnosticActions {
            target,
            actions,
            state: list,
        });
        return true;
    }
    let selected = page.list.selected().unwrap_or_default();
    let next = match command {
        Command::SelectNextOrScrollDown => selected.saturating_add(count),
        Command::SelectPreviousOrScrollUp => selected.saturating_sub(count),
        Command::PageSelectNextOrScrollDown => {
            selected.saturating_add(config::get_config().app_config.page_size_in_rows * count)
        }
        Command::PageSelectPreviousOrScrollUp => {
            selected.saturating_sub(config::get_config().app_config.page_size_in_rows * count)
        }
        Command::SelectFirstOrScrollToTop => 0,
        Command::SelectLastOrScrollToBottom => rows.len().saturating_sub(1),
        _ => return false,
    };
    page.select_index(&rows, next.min(rows.len().saturating_sub(1)));
    true
}

fn diagnostic_rows_for_logs_page(
    app_state: &SharedState,
    ui: &UIStateGuard,
) -> Option<Vec<crate::observability::DiagnosticRow>> {
    let follow_reference = match ui.current_page() {
        PageState::Logs { state } => state.follow_reference.clone(),
        _ => return None,
    };
    let mut operations = app_state.diagnostics.recent_operations(8);
    if let Some(timeline) = follow_reference
        .as_deref()
        .and_then(|reference| app_state.diagnostics.timeline(reference))
    {
        if !operations
            .iter()
            .any(|operation| operation.reference == timeline.reference)
        {
            operations.push(timeline);
        }
    }
    let mut rows = crate::observability::diagnostic_rows(
        &app_state.diagnostics.health_snapshot(),
        app_state.diagnostics.filter_snapshot(),
        &app_state.diagnostics.incident_states(),
        &operations,
    );
    let youtube_route = app_state
        .player
        .read()
        .youtube_playback
        .as_ref()
        .map(|playback| playback.route.clone());
    crate::observability::append_youtube_playback_route_row(&mut rows, youtube_route.as_ref());
    #[cfg(feature = "private-capture")]
    rows.push(crate::observability::private_capture_row(
        &app_state.private_capture_operator_snapshot(),
    ));
    Some(rows)
}

fn handle_command_for_settings_page(
    command: Command,
    client_pub: &crate::client::ClientRequestSender,
    _state: &SharedState,
    ui: &mut UIStateGuard,
) -> Result<bool> {
    let selected = ui.current_page().selected_index().unwrap_or_default();
    let filter_query = match ui.popup.as_ref() {
        Some(PopupState::Search { query }) => Some(query.clone()),
        _ => None,
    };
    let (n_settings, selected_source, selected_setting) = match ui.current_page() {
        PageState::Settings { settings, .. } => {
            let projection = settings_filter_projection(settings, filter_query.as_deref())
                .into_iter()
                .filter(|(_, setting)| ui.workspace_settings_category.includes(setting))
                .collect::<Vec<_>>();
            let selected_source = settings.get(selected).map(|_| selected);
            (
                projection.len(),
                selected_source,
                selected_source.and_then(|source_index| settings.get(source_index).cloned()),
            )
        }
        _ => return Ok(false),
    };

    if ui.workspace_focus == WorkspaceFocusState::Context
        && handle_settings_workspace_list_navigation(command, ui)
    {
        return Ok(true);
    }

    if command == Command::Search && filter_query.is_none() {
        ui.new_search_popup();
        ui.current_page_mut().select(0);
        return Ok(true);
    }

    if command == Command::ClosePopup && filter_query.is_none() {
        return Ok(leave_settings_workspace(ui));
    }

    if command == Command::ClosePopup && filter_query.is_some() {
        if let Some(source_index) = selected_source {
            ui.current_page_mut().select(source_index);
        }
        ui.popup = None;
        return Ok(true);
    }

    if matches!(command, Command::ChooseSelected | Command::ResumePause) {
        if let Some(setting) = selected_setting {
            let selected = selected_source.unwrap_or(selected);
            ui.current_page_mut().select(selected);
            match setting.kind {
                config::AppConfigValueKind::Bool => {
                    let value = (setting.value != "true").to_string();
                    save_settings_value(ui, &setting.key, &value, selected);
                }
                config::AppConfigValueKind::Choice(_) if setting.key == "theme" => {
                    ui.open_theme_picker();
                }
                config::AppConfigValueKind::Choice(options) => {
                    let mut state = ListState::default();
                    let selected = options
                        .iter()
                        .position(|option| setting.value.trim_matches('"') == option)
                        .unwrap_or_default();
                    state.select(Some(selected));
                    ui.popup = Some(PopupState::ConfigChoice {
                        key: setting.key,
                        options,
                        state,
                    });
                }
                config::AppConfigValueKind::MultiChoice(options) => {
                    let current = parse_settings_array_value(&setting.value);
                    let selected = options
                        .iter()
                        .map(|option| current.iter().any(|value| value == option))
                        .collect();
                    let mut state = ListState::default();
                    state.select(Some(0));
                    ui.popup = Some(PopupState::ConfigMultiChoice {
                        key: setting.key,
                        options,
                        selected,
                        state,
                    });
                }
                config::AppConfigValueKind::Value => {
                    ui.popup = Some(PopupState::ConfigEdit {
                        key: setting.key,
                        input: LineInput::new(setting.value.chars().collect()),
                    });
                }
                config::AppConfigValueKind::Status => return Ok(true),
                config::AppConfigValueKind::Action(action) => match action {
                    config::AppConfigAction::OpenWelcomeSetup => {
                        ui.open_setup_page(true);
                    }
                    config::AppConfigAction::AuthenticateSpotify => {
                        client_pub.send(ClientRequest::ReauthenticateSpotify)?;
                        set_settings_message(ui, "Spotify authentication started");
                    }
                    config::AppConfigAction::AuthenticateYouTubeBrowser => {
                        client_pub.send(ClientRequest::AuthenticateYouTubeBrowser)?;
                        set_settings_message(ui, "YouTube dedicated browser sign-in advanced");
                    }
                    config::AppConfigAction::AddSpotifyAccount => {
                        client_pub.send(ClientRequest::ManageAccount(
                            crate::client::AccountOperation::Add(config::ActiveProvider::Spotify),
                        ))?;
                        set_settings_message(ui, "Adding a Spotify account");
                    }
                    config::AppConfigAction::AddYouTubeAccount => {
                        client_pub.send(ClientRequest::ManageAccount(
                            crate::client::AccountOperation::Add(
                                config::ActiveProvider::YouTubeMusic,
                            ),
                        ))?;
                        set_settings_message(ui, "Adding a YouTube Music account");
                    }
                    config::AppConfigAction::ValidateSpotifyAccount => {
                        client_pub.send(ClientRequest::ManageAccount(
                            crate::client::AccountOperation::Validate(
                                config::ActiveProvider::Spotify,
                            ),
                        ))?;
                        set_settings_message(ui, "Validating the active Spotify account");
                    }
                    config::AppConfigAction::ValidateYouTubeAccount => {
                        client_pub.send(ClientRequest::ManageAccount(
                            crate::client::AccountOperation::Validate(
                                config::ActiveProvider::YouTubeMusic,
                            ),
                        ))?;
                        set_settings_message(ui, "Validating the active YouTube Music account");
                    }
                    config::AppConfigAction::RemoveSpotifyAccount => {
                        open_account_remove_confirmation(ui, config::ActiveProvider::Spotify)?;
                    }
                    config::AppConfigAction::RemoveYouTubeAccount => {
                        open_account_remove_confirmation(ui, config::ActiveProvider::YouTubeMusic)?;
                    }
                    config::AppConfigAction::ImportYouTubeAuth => {
                        super::import_youtube_auth(ui)?;
                        reload_settings(ui, selected);
                    }
                    config::AppConfigAction::TestYouTubeAuth => {
                        client_pub.send(ClientRequest::TestYouTubeAuth)?;
                        set_settings_message(ui, "YouTube Music authentication test started");
                    }
                    config::AppConfigAction::OpenLogs => {
                        ui.new_page(PageState::Logs {
                            state: crate::state::DiagnosticsPageUIState::new(),
                        });
                    }
                    config::AppConfigAction::ClearHomeHistory => {
                        ui.popup = Some(PopupState::ConfirmAction {
                            message: "Forget every collection listed under Continue on Home? Press y to confirm".to_owned(),
                            action: ConfirmableAction::ClearHomeHistory,
                        });
                    }
                    config::AppConfigAction::ResetAllConfiguration => {
                        ui.popup = Some(PopupState::ConfirmAction {
                            message: "Delete all saved preferences, accounts, journal, and history, then reopen first-use setup? Press y to confirm".to_owned(),
                            action: ConfirmableAction::ResetAllConfiguration,
                        });
                    }
                },
            }
            return Ok(true);
        }
    }

    let count = ui.count_prefix;
    Ok(handle_navigation_command(
        command,
        ui.current_page_mut(),
        selected,
        n_settings,
        count,
    ))
}

pub(super) fn set_settings_message(ui: &mut crate::state::UIState, message: &str) {
    if let PageState::Settings {
        saved,
        error,
        notice,
        ..
    } = ui.current_page_mut()
    {
        *saved = false;
        *error = None;
        *notice = Some(message.to_string());
    }
}

fn open_account_remove_confirmation(
    ui: &mut UIStateGuard,
    provider: config::ActiveProvider,
) -> Result<()> {
    let configs = config::get_config();
    let registry = config::AccountRegistry::load(&configs.config_folder)?;
    let Some(account_id) = registry.active_id(provider).map(str::to_owned) else {
        set_settings_message(ui, "No active account to remove");
        return Ok(());
    };
    let label = registry.record(provider, &account_id).map_or_else(
        || "active account".to_string(),
        |account| account.label.clone(),
    );
    ui.popup = Some(PopupState::ConfirmAction {
        message: format!(
            "Remove {label} from {}? Press y to confirm",
            provider.title()
        ),
        action: ConfirmableAction::RemoveAccount {
            provider,
            account_id,
        },
    });
    Ok(())
}

fn reload_settings(ui: &mut UIStateGuard, selected: usize) {
    let result = config::app_config_settings(&config::get_config().config_folder);

    let PageState::Settings {
        list,
        settings,
        saved,
        error,
        notice,
        ..
    } = ui.current_page_mut()
    else {
        return;
    };
    if let Ok(new_settings) = result {
        *settings = new_settings;
        if !settings.is_empty() {
            list.select(Some(selected.min(settings.len() - 1)));
        }
        *saved = true;
        *error = None;
        *notice = Some("YouTube Music credentials imported".to_string());
    } else {
        *notice = None;
        *error = Some(crate::state::SETTINGS_RELOAD_ERROR_MESSAGE.to_owned());
    }
}

fn parse_settings_array_value(value: &str) -> Vec<String> {
    value
        .parse::<toml::Value>()
        .ok()
        .and_then(|value| {
            value.as_array().map(|values| {
                values
                    .iter()
                    .filter_map(|value| value.as_str().map(ToString::to_string))
                    .collect()
            })
        })
        .unwrap_or_default()
}

pub(super) fn save_settings_value(ui: &mut UIStateGuard, key: &str, value: &str, selected: usize) {
    if key == "client_id" && ui.welcome_spotify_auth_in_flight {
        if let PageState::Settings { notice, .. } = ui.current_page_mut() {
            *notice = Some(
                "Wait for Spotify sign-in/check to finish before changing the client.".to_owned(),
            );
        }
        return;
    }
    let config_folder = config::get_config().config_folder.clone();
    let result = config::save_app_config_override(&config_folder, key, value)
        .and_then(|()| {
            if matches!(
                key,
                "presentation.compact_metadata" | "presentation.journal_indicators"
            ) {
                // Editing a concrete presentation field opts into Custom so
                // a preset cannot silently hide the user's new value.
                config::save_app_config_override(&config_folder, "presentation.profile", "Custom")
            } else {
                Ok(())
            }
        })
        .and_then(|()| config::app_config_settings(&config_folder));

    // The process-wide Configs handle is intentionally immutable after
    // startup. Presentation preferences are UI-owned, so refresh that small
    // runtime projection immediately instead of making a visual change wait
    // for a restart.
    let updated_presentation = if key.starts_with("presentation.")
        || matches!(
            key,
            "layout.playback_window_height"
                | "layout.playback_window_position"
                | "border_type"
                | "app_refresh_duration_in_ms"
                | "terminal_title"
                | "terminal_title_idle"
        ) {
        config::AppConfig::new(&config_folder).ok()
    } else {
        None
    };

    if result.is_ok() {
        if key == "client_id" {
            if let Ok(setup) = config::SetupState::load(&config_folder) {
                ui.setup_state.spotify_reauthentication_required =
                    setup.spotify_reauthentication_required;
                ui.welcome_spotify_client_pending = setup.spotify_reauthentication_required;
                value.trim().clone_into(&mut ui.welcome_spotify_client_id);
                ui.welcome_spotify_client_command = false;
                if setup.spotify_reauthentication_required {
                    ui.spotify_auth_status = config::SpotifyAuthSnapshot::default();
                    ui.welcome_spotify_web_token_cached = false;
                    ui.welcome_spotify_library_tested = None;
                    ui.welcome_spotify_playback_tested = None;
                    ui.mark_setup_pending();
                }
            }
        }
        if let Some(presentation) = updated_presentation {
            ui.apply_presentation_config(&presentation);
        }
    }

    if result.is_err() {
        tracing::error!("Failed to save a setting");
    }

    let PageState::Settings {
        list,
        settings,
        saved,
        error,
        notice,
        ..
    } = ui.current_page_mut()
    else {
        return;
    };

    if let Ok(new_settings) = result {
        *settings = new_settings;
        if !settings.is_empty() {
            list.select(Some(selected.min(settings.len() - 1)));
        }
        *saved = true;
        *error = None;
        *notice = None;
    } else {
        *saved = false;
        *notice = None;
        *error = Some(crate::state::SETTINGS_SAVE_ERROR_MESSAGE.to_owned());
    }
}

pub fn handle_navigation_command(
    command: Command,
    page: &mut PageState,
    id: usize,
    len: usize,
    count: Option<usize>,
) -> bool {
    if len == 0 {
        return false;
    }

    match command {
        Command::SelectNextOrScrollDown => {
            let offset = count.unwrap_or(1);
            page.select(std::cmp::min(id + offset, len - 1));
            true
        }
        Command::SelectPreviousOrScrollUp => {
            let offset = count.unwrap_or(1);
            page.select(id.saturating_sub(offset));
            true
        }
        Command::PageSelectNextOrScrollDown => {
            let page_size = config::get_config().app_config.page_size_in_rows;
            let offset = count.unwrap_or(1) * page_size;
            page.select(std::cmp::min(id + offset, len - 1));
            true
        }
        Command::PageSelectPreviousOrScrollUp => {
            let page_size = config::get_config().app_config.page_size_in_rows;
            let offset = count.unwrap_or(1) * page_size;
            page.select(id.saturating_sub(offset));
            true
        }
        Command::SelectLastOrScrollToBottom => {
            page.select(len - 1);
            true
        }
        Command::SelectFirstOrScrollToTop => {
            page.select(0);
            true
        }
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::SettingsCategory;
    use super::{
        choose_workspace_scope, handle_command_for_context_page, handle_command_for_library_page,
        handle_command_for_unified_playlist_page, handle_command_for_youtube_context_page,
        handle_settings_workspace_list_navigation, handle_settings_workspace_navigation_command,
        handle_settings_workspace_secondary_command, handle_workspace_context_menu_hit,
        handle_workspace_mouse_hit, handle_workspace_navigation_command,
        journal_entries_snapshot_is_current, journal_list_cursor_for_uri, journal_list_move_target,
        journal_list_target_is_current, journal_list_uri_target_is_current,
        journal_single_target_index, journal_uri_target_is_safe, lyrics_provider_id_from_label,
        lyrics_retry_request, next_lyrics_provider, open_workspace_scope_popup,
        parse_provider_search_query, playable_index_for_selection, provider_search_query_is_valid,
        queue_action_context, queue_action_item_for_row, queue_action_items_for_snapshot,
        sort_unified_playlists_alphabetically, spotify_library_unified_index,
        spotify_user_query_is_valid, synchronize_youtube_playlist_projection,
        unified_playlist_action_items_for_snapshot, unified_playlist_cursor_after_remove,
        youtube_library_playlist_index, youtube_library_total_items, youtube_library_unified_index,
        youtube_search_action_is_supported, YOUTUBE_LIBRARY_ACTIONS_UNAVAILABLE_MESSAGE,
        YOUTUBE_LIBRARY_ACTIONS_UNAVAILABLE_NEXT_ACTION, YOUTUBE_PLAYLIST_ROW_OFFSET,
    };
    use crate::client::ClientRequest;
    use crate::command::{Action, Command};
    use crate::config::ActiveProvider;
    use crate::state::{
        synchronize_journal_uris, ActionListItem, JournalSelection, JournalSelectionScope, MediaId,
        MediaKind, OccurrenceDescriptor, PageState, PlayableMedia, PopupState, Provider,
        ProviderOccurrenceToken, QueueActionMenu, QueueDisplayItem, QueueOrigin,
        QueueSelectionScope, QueuedItem, UIState, UnifiedPlaylistActionItem,
        UnifiedPlaylistActionMenu, UnifiedPlaylistItem, UnifiedPlaylistSelectionScope,
        WorkspaceFocusState, WorkspaceHit, WorkspaceScopeKind, WorkspaceScopeOption,
        WorkspaceScopeSelection, YouTubeContext, YouTubeContextId, YouTubeContextPageUIState,
        YouTubeTrack,
    };
    use anyhow::Result;
    use rspotify::prelude::Id;

    #[test]
    fn submitting_search_leaves_input_and_keeps_global_keys_out_of_the_query() -> Result<()> {
        use crate::state::{SearchFocusState, SearchPageUIState};
        use crossterm::event::{Event, KeyCode, KeyEvent, KeyModifiers};
        crate::ui::initialize_test_config();
        for category in [None, Some(crate::command::ProviderSearchPane::Albums)] {
            let ring =
                std::sync::Arc::new(parking_lot::Mutex::new(std::collections::VecDeque::new()));
            let (diagnostics, _runtime) = crate::observability::disabled(ring);
            let state = std::sync::Arc::new(crate::state::State::new(false, diagnostics));
            let (sender, receiver) = crate::client::client_request_channel();
            {
                let mut ui = state.ui.lock();
                ui.history.clear();
                let mut search = SearchPageUIState::new();
                search.category = category;
                ui.history.push(PageState::Search {
                    line_input: crate::ui::single_line_input::LineInput::new(
                        "neon".chars().collect(),
                    ),
                    current_query: String::new(),
                    state: search,
                });
            }
            let press = |code| {
                crate::event::handle_terminal_event(
                    &Event::Key(KeyEvent::new(code, KeyModifiers::NONE)),
                    &sender,
                    &state,
                )
            };
            press(KeyCode::Enter)?;
            assert!(
                matches!(receiver.recv()?.request(), ClientRequest::Search { query, .. } if query == "neon")
            );
            {
                let ui = state.ui.lock();
                let PageState::Search { state, .. } = ui.current_page() else {
                    unreachable!()
                };
                assert_eq!(
                    state.focus,
                    if category.is_some() {
                        SearchFocusState::Albums
                    } else {
                        SearchFocusState::Tracks
                    }
                );
            }
            press(KeyCode::Char('?'))?;
            let ui = state.ui.lock();
            assert!(matches!(ui.popup, Some(PopupState::CommandHelp { .. })));
            let PageState::Search {
                line_input,
                current_query,
                ..
            } = ui.current_page()
            else {
                unreachable!()
            };
            assert_eq!(line_input.get_text(), "neon");
            assert_eq!(current_query, "neon");
        }
        Ok(())
    }

    #[test]
    fn provider_search_prefixes_override_the_active_provider() {
        let youtube = parse_provider_search_query(" /yt  Mars - Runaway ", ActiveProvider::Spotify);
        assert_eq!(youtube.provider, ActiveProvider::YouTubeMusic);
        assert_eq!(youtube.query, "Mars - Runaway");

        let spotify = parse_provider_search_query("/sp  Bjork", ActiveProvider::YouTubeMusic);
        assert_eq!(spotify.provider, ActiveProvider::Spotify);
        assert_eq!(spotify.query, "Bjork");
    }

    #[test]
    fn settings_tiles_move_through_rows_sections_and_columns() {
        use crate::config::AppConfigSection::{Playback, SharedUi};
        crate::ui::initialize_test_config();
        let setting = |section, key: &str| crate::config::AppConfigSetting {
            section,
            key: key.to_owned(),
            value: "false".to_owned(),
            kind: crate::config::AppConfigValueKind::Bool,
            restart_required: false,
        };
        // Two-column grids: Playback holds sources 0-2, Shared UI 3-4.
        let mut shelves = crate::state::SettingsShelves::default();
        for section in [Playback, SharedUi] {
            shelves[SettingsCategory::Preferences.index()].set_layout(
                section,
                crate::state::ShelfLayout::Grid {
                    columns: 2,
                    step: 10,
                },
            );
        }
        let mut ui = crate::state::UIState::default();
        ui.history.clear();
        ui.history.push(PageState::Settings {
            list: ratatui::widgets::ListState::default().with_selected(Some(0)),
            shelves,
            settings: vec![
                setting(Playback, "p0"),
                setting(Playback, "p1"),
                setting(Playback, "p2"),
                setting(SharedUi, "u0"),
                setting(SharedUi, "u1"),
            ],
            saved: false,
            error: None,
            notice: None,
        });
        ui.workspace_focus = WorkspaceFocusState::Context;
        let selected = |ui: &crate::state::UIState| ui.current_page().selected_index();

        super::move_settings_tiles_horizontally(&mut ui, 1);
        assert_eq!(selected(&ui), Some(1));
        // No tile below in the second column, so Shared UI's second column.
        assert!(handle_settings_workspace_list_navigation(
            Command::SelectNextOrScrollDown,
            &mut ui
        ));
        assert_eq!(selected(&ui), Some(4));
        // Back into Playback from below: its short bottom row lacks the
        // second column, so the row above.
        assert!(handle_settings_workspace_list_navigation(
            Command::PageSelectPreviousOrScrollUp,
            &mut ui
        ));
        assert_eq!(selected(&ui), Some(1));
        assert!(handle_settings_workspace_list_navigation(
            Command::SelectLastOrScrollToBottom,
            &mut ui
        ));
        assert_eq!(selected(&ui), Some(4));
        assert!(handle_settings_workspace_list_navigation(
            Command::SelectFirstOrScrollToTop,
            &mut ui
        ));
        assert_eq!(selected(&ui), Some(0));
    }

    #[test]
    fn settings_workspace_navigation_projects_category_and_action_focus() {
        crate::ui::initialize_test_config();
        let mut ui = crate::state::UIState::default();
        ui.history.clear();
        ui.history.push(PageState::Settings {
            list: ratatui::widgets::ListState::default().with_selected(Some(0)),
            shelves: crate::state::SettingsShelves::default(),
            settings: vec![
                crate::config::AppConfigSetting {
                    section: crate::config::AppConfigSection::Playback,
                    key: "page_size_in_rows".to_owned(),
                    value: "20".to_owned(),
                    kind: crate::config::AppConfigValueKind::Value,
                    restart_required: false,
                },
                crate::config::AppConfigSetting {
                    section: crate::config::AppConfigSection::Accounts,
                    key: "accounts.spotify.status".to_owned(),
                    value: "Ready".to_owned(),
                    kind: crate::config::AppConfigValueKind::Status,
                    restart_required: false,
                },
            ],
            saved: false,
            error: None,
            notice: None,
        });
        ui.workspace_focus = WorkspaceFocusState::Navigation;

        assert!(handle_settings_workspace_navigation_command(
            Command::SelectNextOrScrollDown,
            &mut ui,
        )
        .unwrap());
        assert_eq!(ui.workspace_settings_category, SettingsCategory::Accounts);
        assert_eq!(ui.current_page().selected_index(), Some(1));

        ui.workspace_focus = WorkspaceFocusState::Context;
        assert!(handle_settings_workspace_list_navigation(
            Command::SelectPreviousOrScrollUp,
            &mut ui,
        ));
        assert_eq!(ui.current_page().selected_index(), Some(1));

        ui.workspace_focus = WorkspaceFocusState::Actions;
        assert!(
            handle_settings_workspace_secondary_command(Command::ChooseSelected, &mut ui,).unwrap()
        );
        assert_eq!(
            match ui.current_page() {
                PageState::Settings { notice, .. } => notice.as_deref(),
                _ => None,
            },
            Some("No unsaved changes; settings are already applied.")
        );
    }

    #[test]
    fn workspace_scope_keyboard_activation_opens_the_anchored_provider_selector() -> Result<()> {
        crate::ui::initialize_test_config();
        let ring = std::sync::Arc::new(parking_lot::Mutex::new(std::collections::VecDeque::new()));
        let (diagnostics, _runtime) = crate::observability::disabled(ring);
        let state = std::sync::Arc::new(crate::state::State::new(false, diagnostics));
        let (sender, receiver) = crate::client::client_request_channel();
        let anchor = ratatui::layout::Rect::new(2, 30, 22, 1);
        let mut ui = state.ui.lock();
        ui.history.clear();
        ui.history.push(PageState::Library {
            state: crate::state::LibraryPageUIState::new(),
        });
        ui.workspace_hits
            .push((anchor, WorkspaceHit::Scope(WorkspaceScopeKind::Browsing)));

        assert!(handle_workspace_navigation_command(
            Command::SwitchProvider,
            &sender,
            &state,
            &mut ui,
        )?);
        assert!(matches!(
            &ui.popup,
            Some(crate::state::PopupState::WorkspaceScope {
                kind: WorkspaceScopeKind::Browsing,
                options,
                anchor: popup_anchor,
                ..
            }) if options.len() == 2 && *popup_anchor == anchor
        ));
        assert!(receiver.try_recv().is_err());
        Ok(())
    }

    #[test]
    fn diagnostics_workspace_mouse_hit_selects_the_rendered_row() -> Result<()> {
        crate::ui::initialize_test_config();
        let ring = std::sync::Arc::new(parking_lot::Mutex::new(std::collections::VecDeque::new()));
        let (diagnostics, _runtime) = crate::observability::disabled(ring);
        let state = std::sync::Arc::new(crate::state::State::new(false, diagnostics));
        let (sender, receiver) = crate::client::client_request_channel();
        let mut ui = state.ui.lock();
        ui.history.clear();
        ui.history.push(PageState::Logs {
            state: crate::state::DiagnosticsPageUIState::new(),
        });
        let row = ratatui::layout::Rect::new(30, 8, 40, 1);
        ui.workspace_hits
            .push((row, WorkspaceHit::DiagnosticRow(0)));

        assert!(handle_workspace_mouse_hit(
            WorkspaceHit::DiagnosticRow(0),
            false,
            row.x,
            row.y,
            &sender,
            &state,
            &mut ui,
        )?);
        assert_eq!(ui.current_page().selected_index(), Some(0));
        assert!(matches!(
            ui.current_page(),
            PageState::Logs {
                state: crate::state::DiagnosticsPageUIState {
                    selected_row: Some(_),
                    ..
                }
            }
        ));
        assert!(ui.popup.is_none());
        assert!(receiver.try_recv().is_err());
        Ok(())
    }

    #[test]
    fn workspace_scope_account_selection_dispatches_an_account_switch() -> Result<()> {
        crate::ui::initialize_test_config();
        let ring = std::sync::Arc::new(parking_lot::Mutex::new(std::collections::VecDeque::new()));
        let (diagnostics, _runtime) = crate::observability::disabled(ring);
        let state = std::sync::Arc::new(crate::state::State::new(false, diagnostics));
        let (sender, receiver) = crate::client::client_request_channel();
        let mut ui = state.ui.lock();
        ui.popup = Some(crate::state::PopupState::WorkspaceScope {
            kind: WorkspaceScopeKind::Account,
            options: vec![WorkspaceScopeOption {
                label: "Work".to_owned(),
                selection: WorkspaceScopeSelection::Account {
                    provider: ActiveProvider::Spotify,
                    account_id: "spotify-2".to_owned(),
                },
            }],
            state: ratatui::widgets::ListState::default().with_selected(Some(0)),
            anchor: ratatui::layout::Rect::new(2, 30, 22, 1),
        });

        assert!(choose_workspace_scope(0, &sender, &state, &mut ui)?);
        assert!(ui.popup.is_none());
        assert!(matches!(
            receiver.try_recv().unwrap().request(),
            crate::client::ClientRequest::ManageAccount(
                crate::client::AccountOperation::Switch { provider, account_id }
            ) if *provider == ActiveProvider::Spotify && account_id == "spotify-2"
        ));
        Ok(())
    }

    #[test]
    fn account_change_invalidates_retained_search_and_reloads_it_for_the_active_account(
    ) -> Result<()> {
        crate::ui::initialize_test_config();
        let ring = std::sync::Arc::new(parking_lot::Mutex::new(std::collections::VecDeque::new()));
        let (diagnostics, _runtime) = crate::observability::disabled(ring);
        let state = std::sync::Arc::new(crate::state::State::new(false, diagnostics));
        let (sender, receiver) = crate::client::client_request_channel();
        let query = "account scoped query";
        let old_reference;

        {
            let mut ui = state.ui.lock();
            ui.history.clear();
            ui.history.push(PageState::Search {
                line_input: crate::ui::single_line_input::LineInput::default(),
                current_query: String::new(),
                state: crate::state::SearchPageUIState::new(),
            });
            ui.spotify_account_label = Some("Account A".to_owned());
            if let PageState::Search { state, .. } = ui.current_page_mut() {
                state.provider = Some(ActiveProvider::Spotify);
            }
            old_reference = ui.begin_search(ActiveProvider::Spotify, query);
            state.data.write().caches.search.insert(
                query.to_owned(),
                std::sync::Arc::new(crate::state::SearchResults::default()),
                std::time::Duration::from_secs(60),
            );
            ui.finish_search_success(ActiveProvider::Spotify, query, &old_reference, 2);
            ui.new_page(PageState::CommandHelp { scroll_offset: 0 });

            ui.spotify_account_label = Some("Account B".to_owned());
            assert!(ui.bump_provider_selection_epoch(ActiveProvider::Spotify));
            ui.invalidate_search_lifecycles();
            state.data.write().caches = crate::state::MemoryCaches::new();

            let retained = ui
                .history
                .iter()
                .find_map(|page| match page {
                    PageState::Search {
                        current_query,
                        state,
                        ..
                    } if current_query == query => Some(state),
                    _ => None,
                })
                .expect("Search page remains in history");
            assert_eq!(
                retained.search_lifecycle,
                crate::state::SearchLifecycle::Idle
            );
            assert!(retained.search_selection.selected_indices().is_empty());
            assert!(state.data.read().caches.search.get(query).is_none());

            ui.history.pop();
            ui.sync_workspace_after_history_change();
        }

        super::reload_invalidated_active_search(&sender, &state)?;
        let request = receiver.try_recv().expect("active Search is reloaded");
        let new_reference = match request.request() {
            ClientRequest::Search {
                query: request_query,
                lifecycle_reference,
            } => {
                assert_eq!(request_query, query);
                lifecycle_reference.clone()
            }
            _ => panic!("Spotify Search request is dispatched"),
        };
        assert_ne!(new_reference, old_reference);
        assert_eq!(
            state.ui.lock().spotify_account_label.as_deref(),
            Some("Account B")
        );

        {
            let mut ui = state.ui.lock();
            ui.finish_search_success(ActiveProvider::Spotify, query, &old_reference, 2);
            assert!(matches!(
                ui.current_page(),
                PageState::Search {
                    state: crate::state::SearchPageUIState {
                        search_lifecycle: crate::state::SearchLifecycle::Loading { reference, .. },
                        ..
                    },
                    ..
                } if reference == &new_reference
            ));
            ui.finish_search_success(ActiveProvider::Spotify, query, &new_reference, 1);
            assert!(matches!(
                ui.current_page(),
                PageState::Search {
                    state: crate::state::SearchPageUIState {
                        search_lifecycle: crate::state::SearchLifecycle::Ready { result_count: 1 },
                        ..
                    },
                    ..
                }
            ));
        }
        Ok(())
    }

    #[test]
    fn workspace_scope_account_without_saved_accounts_reports_a_bounded_fallback() -> Result<()> {
        crate::ui::initialize_test_config();
        let ring = std::sync::Arc::new(parking_lot::Mutex::new(std::collections::VecDeque::new()));
        let (diagnostics, _runtime) = crate::observability::disabled(ring);
        let state = std::sync::Arc::new(crate::state::State::new(false, diagnostics));
        let mut ui = state.ui.lock();

        assert!(open_workspace_scope_popup(
            WorkspaceScopeKind::Account,
            ratatui::layout::Rect::new(2, 30, 22, 1),
            &state,
            &mut ui,
        )?);
        assert!(ui.popup.is_none());
        Ok(())
    }

    #[test]
    fn workspace_right_click_track_reuses_actions_and_preserves_the_row_anchor() -> Result<()> {
        crate::ui::initialize_test_config();
        let ring = std::sync::Arc::new(parking_lot::Mutex::new(std::collections::VecDeque::new()));
        let (diagnostics, _runtime) = crate::observability::disabled(ring);
        let state = std::sync::Arc::new(crate::state::State::new(false, diagnostics));
        let track = crate::state::Track {
            id: rspotify::model::TrackId::from_id("4iV5W9uYEdYUVa79Axb7Rh")
                .unwrap()
                .into_static(),
            name: "Example track".to_owned(),
            artists: vec![crate::state::Artist {
                id: rspotify::model::ArtistId::from_id("0OdUWJ0sBjDrqHygGUXeCF")
                    .unwrap()
                    .into_static(),
                name: "Example artist".to_owned(),
            }],
            album: None,
            duration: std::time::Duration::from_secs(180),
            explicit: false,
            added_at: 0,
        };
        state.data.write().caches.search.insert(
            "right click".to_owned(),
            std::sync::Arc::new(crate::state::SearchResults {
                tracks: vec![track],
                ..crate::state::SearchResults::default()
            }),
            std::time::Duration::from_secs(60),
        );
        let (sender, receiver) = crate::client::client_request_channel();
        let anchor = ratatui::layout::Rect::new(30, 12, 40, 1);
        let mut ui = state.ui.lock();
        ui.history.clear();
        ui.history.push(PageState::Search {
            line_input: crate::ui::single_line_input::LineInput::default(),
            current_query: "right click".to_owned(),
            state: crate::state::SearchPageUIState::new(),
        });

        assert!(handle_workspace_context_menu_hit(
            WorkspaceHit::SearchRow {
                focus: crate::state::SearchFocusState::Tracks,
                index: 0,
            },
            anchor,
            &sender,
            &state,
            &mut ui,
        )?);
        assert!(matches!(
            &ui.popup,
            Some(PopupState::AnchoredActionList {
                anchor: popup_anchor,
                item,
                ..
            }) if *popup_anchor == anchor && item.n_actions() > 0
        ));
        assert!(receiver.try_recv().is_err());
        Ok(())
    }

    #[test]
    fn workspace_right_click_spotify_playlist_reuses_playlist_actions() -> Result<()> {
        crate::ui::initialize_test_config();
        let ring = std::sync::Arc::new(parking_lot::Mutex::new(std::collections::VecDeque::new()));
        let (diagnostics, _runtime) = crate::observability::disabled(ring);
        let state = std::sync::Arc::new(crate::state::State::new(false, diagnostics));
        let playlist = crate::state::Playlist {
            id: rspotify::model::PlaylistId::from_id("37i9dQZF1DXcBWIGoYBM5M")
                .unwrap()
                .into_static(),
            name: "Example playlist".to_owned(),
            collaborative: false,
            owner: (
                "Owner".to_owned(),
                rspotify::model::UserId::from_id("owner")
                    .unwrap()
                    .into_static(),
            ),
            desc: String::new(),
            current_folder_id: 0,
            snapshot_id: String::new(),
        };
        state.data.write().user_data.playlists =
            vec![crate::state::PlaylistFolderItem::Playlist(playlist)];
        let (sender, receiver) = crate::client::client_request_channel();
        let anchor = ratatui::layout::Rect::new(2, 6, 22, 1);
        let mut ui = state.ui.lock();
        ui.history.clear();
        ui.history.push(PageState::Library {
            state: crate::state::LibraryPageUIState::new(),
        });

        assert!(handle_workspace_context_menu_hit(
            WorkspaceHit::LibraryRow {
                focus: crate::state::LibraryFocusState::Playlists,
                index: 0,
            },
            anchor,
            &sender,
            &state,
            &mut ui,
        )?);
        assert!(matches!(
            &ui.popup,
            Some(PopupState::AnchoredActionList {
                anchor: popup_anchor,
                item,
                ..
            }) if *popup_anchor == anchor && matches!(item.as_ref(), ActionListItem::Playlist(_, _))
        ));
        assert!(receiver.try_recv().is_err());
        Ok(())
    }

    fn popup_action_descriptors(
        ui: &crate::state::UIState,
    ) -> Vec<crate::command::ActionDescriptor> {
        match ui.popup.as_ref() {
            Some(PopupState::ActionList(item, ..))
            | Some(PopupState::AnchoredActionList { item, .. }) => item.action_descriptors(),
            other => panic!("expected an action popup, got {other:?}"),
        }
    }

    fn parity_spotify_playlist() -> crate::state::Playlist {
        crate::state::Playlist {
            id: rspotify::model::PlaylistId::from_id("37i9dQZF1DXcBWIGoYBM5M")
                .unwrap()
                .into_static(),
            name: "Parity playlist".to_owned(),
            collaborative: false,
            owner: (
                "Owner".to_owned(),
                rspotify::model::UserId::from_id("owner")
                    .unwrap()
                    .into_static(),
            ),
            desc: String::new(),
            current_folder_id: 0,
            snapshot_id: String::new(),
        }
    }

    #[test]
    fn spotify_playlist_row_g_a_matches_playlist_context_a() -> Result<()> {
        crate::ui::initialize_test_config();
        let ring = std::sync::Arc::new(parking_lot::Mutex::new(std::collections::VecDeque::new()));
        let (diagnostics, _runtime) = crate::observability::disabled(ring);
        let state = std::sync::Arc::new(crate::state::State::new(false, diagnostics));
        let playlist = parity_spotify_playlist();
        let context_id = crate::state::ContextId::Playlist(playlist.id.clone());
        {
            let mut data = state.data.write();
            data.user_data.playlists =
                vec![crate::state::PlaylistFolderItem::Playlist(playlist.clone())];
            data.caches.context.insert(
                context_id.uri(),
                crate::state::Context::Playlist {
                    playlist: playlist.clone(),
                    tracks: Vec::new(),
                },
                *crate::state::TTL_CACHE_DURATION,
            );
        }
        let (sender, receiver) = crate::client::client_request_channel();
        let mut ui = state.ui.lock();
        ui.history.clear();
        ui.history.push(PageState::Library {
            state: crate::state::LibraryPageUIState::new(),
        });
        assert!(handle_command_for_library_page(
            Command::ShowActionsOnSelectedItem,
            &sender,
            &mut ui,
            &state,
        )?);
        let row_actions = popup_action_descriptors(&ui);

        ui.popup = None;
        ui.history.clear();
        ui.history.push(PageState::Context {
            id: Some(context_id.clone()),
            context_page_type: crate::state::ContextPageType::Browsing(context_id),
            state: None,
        });
        assert!(handle_command_for_context_page(
            Command::ShowActionsOnCurrentContext,
            &sender,
            &mut ui,
            &state,
        )?);
        assert_eq!(row_actions, popup_action_descriptors(&ui));
        assert!(receiver.try_recv().is_err());
        Ok(())
    }

    #[test]
    fn youtube_playlist_row_g_a_matches_playlist_context_a() -> Result<()> {
        crate::ui::initialize_test_config();
        let ring = std::sync::Arc::new(parking_lot::Mutex::new(std::collections::VecDeque::new()));
        let (diagnostics, _runtime) = crate::observability::disabled(ring);
        let state = std::sync::Arc::new(crate::state::State::new(false, diagnostics));
        let playlist = crate::state::YouTubeLibraryPlaylist {
            id: "PL-parity".to_owned(),
            name: "Parity playlist".to_owned(),
            author: "Owner".to_owned(),
            tracks: "1".to_owned(),
            thumbnail_url: None,
        };
        {
            let mut data = state.data.write();
            data.user_data.youtube_library.playlists = vec![playlist.clone()];
        }
        let (sender, receiver) = crate::client::client_request_channel();
        let context_id = YouTubeContextId::Playlist(playlist.id.clone());
        let mut ui = state.ui.lock();
        ui.active_provider = ActiveProvider::YouTubeMusic;
        ui.history.clear();
        ui.history.push(PageState::Library {
            state: crate::state::LibraryPageUIState::new(),
        });
        if let PageState::Library { state } = ui.current_page_mut() {
            state
                .playlist_list
                .select(Some(YOUTUBE_PLAYLIST_ROW_OFFSET));
        }
        assert!(handle_command_for_library_page(
            Command::ShowActionsOnSelectedItem,
            &sender,
            &mut ui,
            &state,
        )?);
        let row_actions = popup_action_descriptors(&ui);

        ui.popup = None;
        ui.history.clear();
        ui.history.push(PageState::YouTubeContext {
            id: context_id.clone(),
            context: Some(YouTubeContext {
                title: playlist.name.clone(),
                ..YouTubeContext::default()
            }),
            state: YouTubeContextPageUIState::new(),
        });
        assert!(handle_command_for_youtube_context_page(
            Command::ShowActionsOnCurrentContext,
            &sender,
            &mut ui,
            &state,
        )?);
        assert_eq!(row_actions, popup_action_descriptors(&ui));
        assert!(receiver.try_recv().is_err());
        Ok(())
    }

    #[test]
    fn unified_playlist_row_g_a_matches_unified_playlist_context_a() -> Result<()> {
        crate::ui::initialize_test_config();
        let ring = std::sync::Arc::new(parking_lot::Mutex::new(std::collections::VecDeque::new()));
        let (diagnostics, _runtime) = crate::observability::disabled(ring);
        let state = std::sync::Arc::new(crate::state::State::new(false, diagnostics));
        let playlist = crate::state::UnifiedPlaylist {
            id: "unified-parity".to_owned(),
            name: "Parity playlist".to_owned(),
            ..crate::state::UnifiedPlaylist::default()
        };
        state.data.write().unified_playlists = vec![playlist.clone()];
        let (sender, receiver) = crate::client::client_request_channel();
        let mut ui = state.ui.lock();
        ui.history.clear();
        ui.history.push(PageState::Library {
            state: crate::state::LibraryPageUIState::new(),
        });
        assert!(handle_command_for_library_page(
            Command::ShowActionsOnSelectedItem,
            &sender,
            &mut ui,
            &state,
        )?);
        let row_actions = popup_action_descriptors(&ui);

        ui.popup = None;
        ui.history.clear();
        ui.history
            .push(PageState::new_unified_playlist(playlist.id.clone()));
        assert!(handle_command_for_unified_playlist_page(
            Command::ShowActionsOnCurrentContext,
            &sender,
            &mut ui,
            &state,
        )?);
        assert_eq!(row_actions, popup_action_descriptors(&ui));
        assert!(receiver.try_recv().is_err());
        Ok(())
    }

    #[test]
    fn unified_playlist_workspace_mouse_hit_selects_the_rendered_row() -> Result<()> {
        crate::ui::initialize_test_config();
        let ring = std::sync::Arc::new(parking_lot::Mutex::new(std::collections::VecDeque::new()));
        let (diagnostics, _runtime) = crate::observability::disabled(ring);
        let state = std::sync::Arc::new(crate::state::State::new(false, diagnostics));
        state
            .data
            .write()
            .unified_playlists
            .push(crate::state::UnifiedPlaylist {
                id: "mouse-playlist".to_owned(),
                name: "Mouse playlist".to_owned(),
                items: vec![crate::state::UnifiedPlaylistItem {
                    entry_id: crate::state::PlaylistEntryId(1),
                    media_id: crate::state::MediaId {
                        provider: crate::state::Provider::Spotify,
                        kind: crate::state::MediaKind::Track,
                        raw_id: "mouse-track".to_owned(),
                    },
                    title: "Mouse row".to_owned(),
                    artists: "Artist".to_owned(),
                    ..crate::state::UnifiedPlaylistItem::default()
                }],
                next_entry_id: 2,
                ..crate::state::UnifiedPlaylist::default()
            });
        let (sender, receiver) = crate::client::client_request_channel();
        let mut ui = state.ui.lock();
        ui.history = vec![PageState::new_unified_playlist("mouse-playlist")];
        let row = ratatui::layout::Rect::new(30, 8, 40, 1);
        ui.workspace_hits
            .push((row, WorkspaceHit::UnifiedPlaylistRow(0)));

        assert!(handle_workspace_mouse_hit(
            WorkspaceHit::UnifiedPlaylistRow(0),
            false,
            row.x,
            row.y,
            &sender,
            &state,
            &mut ui,
        )?);
        assert_eq!(ui.current_page().selected_index(), Some(0));
        assert!(receiver.try_recv().is_err());
        Ok(())
    }

    #[test]
    fn lyrics_retry_request_reuses_the_exact_provider_target() {
        assert!(matches!(
            lyrics_retry_request("spotify:track:4uLU6hMCjMI75M1A2tKUQC", None, None),
            Some(ClientRequest::GetLyrics { track_id })
                if track_id.id() == "4uLU6hMCjMI75M1A2tKUQC"
        ));

        let youtube = YouTubeTrack {
            id: "video-id".to_owned(),
            name: "Video".to_owned(),
            artists: "Artist".to_owned(),
            album: Some("Album".to_owned()),
            duration: "3:00".to_owned(),
            explicit: false,
            thumbnail_url: None,
            is_video: true,
        };
        assert!(matches!(
            lyrics_retry_request("youtube:video-id", Some(youtube), Some("lyricsovh".to_owned())),
            Some(ClientRequest::GetYouTubeLyricsFromProvider { track, provider })
                if track.id == "video-id" && provider == "lyricsovh"
        ));
        assert!(lyrics_retry_request("not-a-provider-uri", None, None).is_none());
        assert_eq!(lyrics_provider_id_from_label("LRCLIB"), Some("lrclib"));
        assert_eq!(lyrics_provider_id_from_label("Spotify"), None);
        let lyrics = crate::config::LyricsConfig {
            providers: vec!["lrclib".to_owned(), "lyricsovh".to_owned()],
        };
        assert_eq!(
            next_lyrics_provider(None, &lyrics),
            Some("lrclib".to_owned())
        );
        assert_eq!(
            next_lyrics_provider(Some("lrclib"), &lyrics),
            Some("lyricsovh".to_owned())
        );
    }

    #[test]
    fn provider_search_without_a_prefix_uses_the_active_provider() {
        let query = parse_provider_search_query("  Massive Attack ", ActiveProvider::YouTubeMusic);
        assert_eq!(query.provider, ActiveProvider::YouTubeMusic);
        assert_eq!(query.query, "Massive Attack");
    }

    #[test]
    fn youtube_alias_is_supported() {
        let query = parse_provider_search_query("/youtube  Portishead", ActiveProvider::Spotify);
        assert_eq!(query.provider, ActiveProvider::YouTubeMusic);
        assert_eq!(query.query, "Portishead");
    }

    #[test]
    fn youtube_command_projection_retains_exact_delete_token_and_editability() {
        let track = YouTubeTrack {
            id: "video-id".to_owned(),
            name: "Video".to_owned(),
            artists: "Artist".to_owned(),
            album: None,
            duration: "3:00".to_owned(),
            explicit: false,
            thumbnail_url: None,
            is_video: false,
        };
        let context_id = YouTubeContextId::Playlist("PLplaylist".to_owned());
        let context = YouTubeContext {
            title: "Playlist".to_owned(),
            description: None,
            tracks: vec![track],
            playlist_set_video_ids: vec![Some("set-video".to_owned())],
            artist: None,
        };
        let mut ui_state = UIState::default();
        ui_state.history = vec![PageState::YouTubeContext {
            id: context_id.clone(),
            context: Some(context.clone()),
            state: YouTubeContextPageUIState::new(),
        }];
        let mutex = crate::state::TrackedMutex::new(ui_state);
        let mut ui = mutex.lock();
        let (snapshot, _) =
            synchronize_youtube_playlist_projection(&mut ui, &context_id, &context, true)
                .expect("editable playlist projection");
        assert!(matches!(
            &snapshot.entries[0].occurrence,
            ProviderOccurrenceToken::YouTubeMusic {
                set_video_id: Some(token),
                ..
            } if token == "set-video"
        ));
        assert!(snapshot.entries[0]
            .item_actions
            .contains(&Action::DeleteFromPlaylist));
    }

    #[test]
    fn spotify_user_prefix_is_parsed_as_a_separate_ui_intent() {
        let query = parse_provider_search_query(" /user  alice ", ActiveProvider::YouTubeMusic);
        assert_eq!(query.provider, ActiveProvider::Spotify);
        assert_eq!(query.query, "");
        assert_eq!(query.spotify_user_query.as_deref(), Some("alice"));
    }

    #[test]
    fn empty_spotify_user_prefix_is_rejected_before_dispatch() {
        assert!(!spotify_user_query_is_valid(""));
        assert!(!spotify_user_query_is_valid("   "));
        assert!(spotify_user_query_is_valid("me"));
        assert!(spotify_user_query_is_valid(
            "https://open.spotify.com/user/alice"
        ));
    }

    #[test]
    fn empty_provider_search_prefix_is_rejected_before_dispatch() {
        assert!(!provider_search_query_is_valid(""));
        assert!(!provider_search_query_is_valid("  "));
        assert!(provider_search_query_is_valid("Massive Attack"));
    }

    #[test]
    fn duplicate_playlist_items_keep_the_selected_occurrence() {
        let track = crate::state::YouTubeTrack {
            id: "duplicate-id".to_string(),
            name: "Duplicate".to_string(),
            artists: "Artist".to_string(),
            album: None,
            duration: "3:00".to_string(),
            explicit: false,
            thumbnail_url: None,
            is_video: false,
        };
        let item = crate::state::UnifiedPlaylistItem::from_youtube_track(&track);
        let items = vec![item.clone(), item];

        assert_eq!(playable_index_for_selection(&items, 1), Some(1));
    }

    #[test]
    fn youtube_search_action_maps_use_the_same_supported_actions_as_item_popups() {
        assert!(youtube_search_action_is_supported(Action::AddToPlaylist));
        assert!(youtube_search_action_is_supported(Action::AddToQueue));
        assert!(youtube_search_action_is_supported(Action::AddToLiked));
        assert!(!youtube_search_action_is_supported(Action::GoToArtist));
    }

    #[test]
    fn youtube_library_liked_row_has_stable_playlist_and_unified_offsets() {
        assert_eq!(youtube_library_playlist_index(0, 3), None);
        assert_eq!(youtube_library_playlist_index(1, 3), Some(0));
        assert_eq!(youtube_library_playlist_index(3, 3), Some(2));
        assert_eq!(youtube_library_playlist_index(4, 3), None);

        assert_eq!(youtube_library_unified_index(4, 3, 2), Some(0));
        assert_eq!(youtube_library_unified_index(5, 3, 2), Some(1));
        assert_eq!(youtube_library_unified_index(6, 3, 2), None);
        assert_eq!(youtube_library_total_items(3, 2), 6);
    }

    #[test]
    fn alphabetical_library_sort_includes_unified_playlists() {
        let mut playlists = vec![
            crate::state::UnifiedPlaylist {
                id: "z".to_owned(),
                name: "Zulu Mix".to_owned(),
                items: Vec::new(),
                updated_at: 0,
                next_entry_id: 1,
            },
            crate::state::UnifiedPlaylist {
                id: "a".to_owned(),
                name: "ambient mix".to_owned(),
                items: Vec::new(),
                updated_at: 0,
                next_entry_id: 1,
            },
        ];

        sort_unified_playlists_alphabetically(&mut playlists);

        assert_eq!(
            playlists
                .iter()
                .map(|playlist| playlist.name.as_str())
                .collect::<Vec<_>>(),
            ["ambient mix", "Zulu Mix"]
        );
    }

    #[test]
    fn spotify_library_unified_rows_have_stable_offsets() {
        assert_eq!(spotify_library_unified_index(0, 3, 2), None);
        assert_eq!(spotify_library_unified_index(2, 3, 2), None);
        assert_eq!(spotify_library_unified_index(3, 3, 2), Some(0));
        assert_eq!(spotify_library_unified_index(4, 3, 2), Some(1));
        assert_eq!(spotify_library_unified_index(5, 3, 2), None);
    }

    #[test]
    fn youtube_library_action_feedback_has_one_shared_next_step() {
        assert_eq!(
            YOUTUBE_LIBRARY_ACTIONS_UNAVAILABLE_MESSAGE,
            "Library row actions are unavailable for YouTube Music."
        );
        assert_eq!(
            YOUTUBE_LIBRARY_ACTIONS_UNAVAILABLE_NEXT_ACTION,
            "Press Enter to open it, then choose an action on its tracks."
        );
    }

    #[test]
    fn queue_actions_are_scoped_to_the_active_provider() {
        let track = YouTubeTrack {
            id: "queue-item".to_string(),
            name: "Queue item".to_string(),
            artists: "Artist".to_string(),
            album: None,
            duration: "3:00".to_string(),
            explicit: false,
            thumbnail_url: None,
            is_video: false,
        };
        let item = QueueDisplayItem::Unified {
            item: QueuedItem {
                entry_id: 1,
                media: PlayableMedia::YouTube(track),
                origin: QueueOrigin::UserAdded,
            },
            is_current: true,
        };

        assert!(queue_action_context(&item, ActiveProvider::YouTubeMusic).is_some());
        assert!(queue_action_context(&item, ActiveProvider::Spotify).is_none());
    }

    fn unified_item(provider: Provider, kind: MediaKind, id: &str) -> UnifiedPlaylistItem {
        UnifiedPlaylistItem {
            entry_id: crate::state::PlaylistEntryId(id.bytes().fold(0u64, |value, byte| {
                value.wrapping_mul(31).wrapping_add(u64::from(byte))
            })),
            media_id: MediaId {
                provider,
                kind,
                raw_id: id.to_owned(),
            },
            title: id.to_owned(),
            artists: "artist".to_owned(),
            duration_ms: Some(1_000),
            provider_url: None,
            ..UnifiedPlaylistItem::default()
        }
    }

    fn queue_row(item: UnifiedPlaylistItem, entry_id: u64) -> QueueDisplayItem {
        QueueDisplayItem::Unified {
            item: QueuedItem {
                entry_id,
                media: item.playable_media().expect("queue test item is playable"),
                origin: QueueOrigin::UserAdded,
            },
            is_current: false,
        }
    }

    #[test]
    fn queue_menu_snapshot_mapping_reconciles_reorder_and_rejects_removal_or_duplicates() {
        let first = unified_item(Provider::Spotify, MediaKind::Track, "first");
        let second = unified_item(Provider::YouTubeMusic, MediaKind::Track, "second");
        let first_row = queue_row(first.clone(), 11);
        let second_row = queue_row(second.clone(), 12);
        let first_action = queue_action_item_for_row(&first_row).expect("first queue item");
        let second_action = queue_action_item_for_row(&second_row).expect("second queue item");
        let scope = QueueSelectionScope::unified(crate::state::UnifiedQueue::empty().instance_id());
        let menu = QueueActionMenu::new(scope, vec![first_action, second_action]);

        let reordered = queue_action_items_for_snapshot(&menu, &[second_row.clone(), first_row]);
        assert_eq!(
            reordered.as_ref().map(|items| items
                .iter()
                .map(|item| item.occurrence())
                .collect::<Vec<_>>()),
            Some(vec![
                menu.items()[0].occurrence(),
                menu.items()[1].occurrence()
            ])
        );
        assert!(queue_action_items_for_snapshot(&menu, &[second_row.clone()]).is_none());
        assert!(
            queue_action_items_for_snapshot(&menu, &[second_row.clone(), second_row]).is_none()
        );
    }

    #[test]
    fn unified_playlist_snapshot_mapping_allows_unplayable_rows_and_rejects_duplicate_tokens() {
        let playable = unified_item(Provider::Spotify, MediaKind::Track, "playable");
        let other = unified_item(Provider::YouTubeMusic, MediaKind::Track, "other");
        let unplayable = unified_item(Provider::Spotify, MediaKind::Video, "video");
        let menu = UnifiedPlaylistActionMenu::new(
            UnifiedPlaylistSelectionScope::new("playlist"),
            vec![UnifiedPlaylistActionItem::new(
                OccurrenceDescriptor::with_token(playable.media_id.clone(), playable.entry_id),
                playable.clone(),
            )],
        );

        let mapped = unified_playlist_action_items_for_snapshot(
            &menu,
            &[other.clone(), unplayable, playable.clone()],
        );
        assert_eq!(
            mapped
                .as_ref()
                .map(|items| items[0].item().media_id.clone()),
            Some(playable.media_id.clone())
        );
        assert!(unified_playlist_action_items_for_snapshot(&menu, &[other.clone()]).is_none());
        assert!(
            unified_playlist_action_items_for_snapshot(&menu, &[playable.clone(), playable])
                .is_none()
        );
    }

    #[test]
    fn unified_playlist_remove_cursor_preserves_survivor_then_uses_nearest_occurrence() {
        let first = unified_item(Provider::Spotify, MediaKind::Track, "first");
        let second = unified_item(Provider::Spotify, MediaKind::Track, "second");
        let third = unified_item(Provider::Spotify, MediaKind::Track, "third");
        let rows = vec![first.clone(), third.clone()];

        assert_eq!(
            unified_playlist_cursor_after_remove(&rows, &rows, Some(first.entry_id), 1),
            Some((0, first.entry_id))
        );
        assert_eq!(
            unified_playlist_cursor_after_remove(&rows, &rows, Some(second.entry_id), 1),
            Some((1, third.entry_id))
        );
        assert_eq!(
            unified_playlist_cursor_after_remove(
                &[first.clone()],
                &[first.clone()],
                Some(second.entry_id),
                1,
            ),
            Some((0, first.entry_id))
        );
        assert_eq!(
            unified_playlist_cursor_after_remove(&[], &[], Some(second.entry_id), 0),
            None
        );

        let hidden = unified_item(Provider::Spotify, MediaKind::Track, "hidden");
        let complete = vec![hidden, third.clone(), first];
        assert_eq!(
            unified_playlist_cursor_after_remove(
                &complete,
                &[third.clone()],
                Some(second.entry_id),
                0,
            ),
            Some((0, third.entry_id))
        );
    }

    #[test]
    fn journal_list_move_maps_filtered_rows_and_recovers_cursor_by_uri() {
        let complete_uris = vec!["one".to_owned(), "missing".to_owned(), "two".to_owned()];
        let visible_uris = vec!["one".to_owned(), "two".to_owned()];
        let mut selection = JournalSelection::default();
        synchronize_journal_uris(
            &mut selection,
            JournalSelectionScope::journal_list(1, "list"),
            Some("query"),
            &complete_uris,
            &visible_uris,
        )
        .unwrap();

        let (full_index, target_uri) = journal_list_move_target(&selection, &complete_uris, 1)
            .expect("filtered row maps to its raw list occurrence");
        assert_eq!(full_index, 2);
        assert_eq!(target_uri, "two");
        assert_eq!(
            journal_list_cursor_for_uri(&["two".to_owned(), "one".to_owned()], &target_uri),
            Some(0)
        );
    }

    #[test]
    fn journal_list_mutation_gates_reject_mismatch_missing_and_duplicates() {
        let captured = vec!["one".to_owned(), "two".to_owned()];
        assert!(journal_list_target_is_current(
            &captured, &captured, 1, "two"
        ));
        assert!(!journal_list_target_is_current(
            &["one".to_owned(), "changed".to_owned()],
            &captured,
            1,
            "two"
        ));
        assert!(!journal_list_target_is_current(
            &["one".to_owned()],
            &captured,
            1,
            "two"
        ));
        assert!(!journal_list_uri_target_is_current(
            &["one".to_owned(), "one".to_owned()],
            &["one".to_owned(), "one".to_owned()],
            "one"
        ));
        assert!(!journal_list_uri_target_is_current(
            &captured, &captured, "missing"
        ));
        assert!(journal_entries_snapshot_is_current(&captured, &captured));
        assert!(!journal_entries_snapshot_is_current(
            &["one".to_owned(), "two".to_owned(), "new".to_owned()],
            &captured
        ));

        let mut selection = JournalSelection::default();
        synchronize_journal_uris(
            &mut selection,
            JournalSelectionScope::journal_list(1, "list"),
            None,
            ["one", "one"],
            ["one", "one"],
        )
        .unwrap();
        assert!(!journal_uri_target_is_safe(
            &selection,
            &["one".to_owned(), "one".to_owned()],
            &["one".to_owned(), "one".to_owned()],
            "one"
        ));
    }

    #[test]
    fn journal_single_target_uses_keyed_uri_when_cursor_is_out_of_bounds() {
        let mut selection = JournalSelection::default();
        synchronize_journal_uris(
            &mut selection,
            JournalSelectionScope::journal(1),
            None,
            ["one", "two"],
            ["one", "two"],
        )
        .unwrap();
        selection.extend_range(0, 0).unwrap();
        let visible_uris = vec!["one".to_owned(), "two".to_owned()];
        let selected_index =
            journal_single_target_index(&selection, visible_uris.len(), usize::MAX)
                .expect("single keyed target");
        assert_eq!(visible_uris[selected_index], "one");

        selection.extend_range(0, 1).unwrap();
        assert_eq!(
            journal_single_target_index(&selection, visible_uris.len(), usize::MAX),
            None
        );
    }
}
