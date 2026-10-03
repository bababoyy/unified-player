use crate::{
    client::{
        dispatch_legacy_playlist, ActivePlaybackControl, ActivePlaybackSeek, ClientRequest,
        PlayerRequest, YouTubePlayerRequest,
    },
    command::{
        self, construct_artist_actions, Action, ActionContext, ActionTarget, Command,
        CommandOrAction,
    },
    config,
    key::{Key, KeySequence},
    state::{
        ActionListItem, Album, AlbumId, Artist, ArtistFocusState, ArtistId, ArtistPopupAction,
        BrowsePageUIState, ConfirmableAction, Context, ContextId, ContextPageType,
        ContextPageUIState, DataReadGuard, Episode, Focusable, Id, Item, ItemId,
        JournalListNameAction, JournalListPopupAction, LibraryFocusState, LibraryPageUIState,
        MultiSelectModel, PageState, PageType, PlayableId, Playback, PlaylistCreateCurrentField,
        PlaylistCreateTarget, PlaylistFolderItem, PlaylistId, PlaylistPopupAction, PopupState,
        ProviderOccurrenceToken, QueueDisplayItem, SearchFocusState, SearchPageUIState, SearchPane,
        SearchScope, SharedState, ShowId, Track, TrackId, TrackOrder, TracksId, UIStateGuard,
        UnifiedPlaylistItem, WorkspaceHit, YouTubeContextId, YouTubeContextPageUIState,
        YouTubePlaylistPopupAction, USER_LIKED_TRACKS_ID, USER_RECENTLY_PLAYED_TRACKS_ID,
        USER_TOP_TRACKS_ID,
    },
    ui::single_line_input::LineInput,
    utils::parse_uri,
};

use crate::utils::map_join;
use anyhow::{Context as _, Result};
use crossterm::event::KeyCode;
use rand::RngExt;

use clipboard::{execute_copy_command, get_clipboard_content};
use ratatui::{layout::Rect, widgets::ListState};

mod bulk_action;
mod clipboard;
mod home;
mod page;
mod playback_command;
mod popup;
mod window;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum MouseWheelOwner {
    Volume,
    Page,
    None,
}

/// Who handles the wheel at the pointer. Only the volume slider changes the
/// volume; the rest of the playback surface ignores the wheel.
const fn mouse_wheel_owner(
    pointer_in_playback: bool,
    pointer_on_volume: bool,
    volume_enabled: bool,
    navigation_enabled: bool,
) -> MouseWheelOwner {
    if pointer_in_playback {
        if pointer_on_volume && volume_enabled {
            MouseWheelOwner::Volume
        } else {
            MouseWheelOwner::None
        }
    } else if navigation_enabled {
        MouseWheelOwner::Page
    } else {
        MouseWheelOwner::None
    }
}

fn page_accepts_mouse_navigation(page: &PageState, popup_open: bool) -> bool {
    !popup_open
        && !matches!(
            page,
            PageState::Search {
                state: SearchPageUIState {
                    focus: SearchFocusState::Input,
                    ..
                },
                ..
            }
        )
}

fn current_youtube_projection_scope(ui: &UIStateGuard) -> (String, u64) {
    let account_id = ui
        .youtube_account_id
        .clone()
        .unwrap_or_else(|| "unknown".to_owned());
    (
        account_id,
        ui.provider_selection_epoch(config::ActiveProvider::YouTubeMusic),
    )
}

/// Return whether `command` mutates a keyed visible selection rather than
/// navigating or acting on a row. Page handlers use this gate so unsupported
/// panes remain unhandled instead of accidentally falling through to generic
/// navigation.
pub(super) const fn is_selection_command(command: Command) -> bool {
    matches!(command, Command::SelectAll | Command::InvertSelection)
}

/// Apply one of the two visible-selection operations and translate typed
/// adapter failures into a fail-closed, unhandled result. Every adapter
/// validates its projection before mutating, so an ambiguous or cursor-only
/// pane remains unchanged when the operation is refused.
pub(super) fn handle_selection_command<M>(command: Command, selection: &mut M) -> bool
where
    M: MultiSelectModel,
{
    if !is_selection_command(command) {
        return false;
    }
    let result = match command {
        Command::SelectAll => selection.select_all_visible(),
        Command::InvertSelection => selection.invert_visible(),
        _ => unreachable!("selection command gate"),
    };
    match result {
        Ok(_) => true,
        Err(error) => {
            tracing::debug!(selection_error = ?error, "selection command refused");
            false
        }
    }
}

/// Dispatch a visible-selection command through the page-owned adapter. The
/// optional pane is only needed for Spotify Context pages; every other page
/// resolves its exact adapter from the current page state.
pub(super) fn handle_page_selection_command(
    command: Command,
    ui: &mut UIStateGuard,
    context_pane: Option<crate::state::ContextTrackPane>,
) -> bool {
    if !is_selection_command(command) {
        return false;
    }
    ui.current_page_mut()
        .selection_adapter_mut(context_pane)
        .is_some_and(|mut selection| handle_selection_command(command, &mut selection))
}

use playback_command::{
    apply_local_effects, is_provider_playback_command, plan_provider_playback_command,
    reads_playback_state, updates_local_playback, PlaybackCommandSnapshot,
};

/// Start a terminal event handler (key pressed, mouse clicked, etc)
pub fn start_event_handler(state: &SharedState, client_pub: &crate::client::ClientRequestSender) {
    while !state.shutdown_requested() {
        match crossterm::event::poll(std::time::Duration::from_millis(100)) {
            Ok(false) => continue,
            Err(err) => {
                crate::observability::log_safe_error!(
                    error,
                    crate::observability::DiagnosticCode::TERMINAL_POLL_FAILED,
                    crate::observability::ErrorCategory::Resource,
                    &err,
                    "Failed to poll terminal input"
                );
                break;
            }
            Ok(true) => {}
        }
        let event = match crossterm::event::read() {
            Ok(event) => event,
            Err(err) => {
                crate::observability::log_safe_error!(
                    error,
                    crate::observability::DiagnosticCode::TERMINAL_READ_FAILED,
                    crate::observability::ErrorCategory::Resource,
                    &err,
                    "Failed to read terminal input"
                );
                break;
            }
        };
        let _enter = tracing::info_span!("terminal_event").entered();
        let event_result = handle_terminal_event(&event, client_pub, state);
        let event_succeeded = event_result.is_ok();
        if let Err(err) = event_result {
            crate::observability::log_safe_error!(
                error,
                crate::observability::DiagnosticCode::TERMINAL_EVENT_HANDLE_FAILED,
                crate::observability::ErrorCategory::Contract,
                &err,
                "Failed to handle a terminal event"
            );
        }
        if event_succeeded {
            if let Err(err) = page::reload_invalidated_active_search(client_pub, state) {
                crate::observability::log_safe_error!(
                    error,
                    crate::observability::DiagnosticCode::TERMINAL_EVENT_HANDLE_FAILED,
                    crate::observability::ErrorCategory::Contract,
                    &err,
                    "Failed to reload the active Search page after account invalidation"
                );
            }
        }
    }
}

/// Apply one terminal event to the shared UI state. Shared by the application
/// event thread and the offline screen preview.
pub(crate) fn handle_terminal_event(
    event: &crossterm::event::Event,
    client_pub: &crate::client::ClientRequestSender,
    state: &SharedState,
) -> Result<()> {
    match event {
        crossterm::event::Event::Mouse(event) => handle_mouse_event(*event, client_pub, state),
        &crossterm::event::Event::Resize(columns, rows) => {
            let mut ui = state.ui.lock();
            ui.clear_playback_hit_regions();
            let policy = crate::ui::LayoutPolicy::from_size(columns, rows);
            ui.orientation = policy.orientation;
            ui.layout_mode = policy.mode;
            ui.bump_diagnostic_revision();
            Ok(())
        }
        crossterm::event::Event::Key(event) => {
            if event.kind == crossterm::event::KeyEventKind::Press {
                // only handle key press event to avoid handling a key event multiple times
                // context:
                // - https://github.com/crossterm-rs/crossterm/issues/752
                // - https://github.com/aome510/spotify-player/issues/136
                handle_key_event(*event, client_pub, state)
            } else {
                Ok(())
            }
        }
        _ => Ok(()),
    }
}

// Handle a terminal mouse event
fn handle_mouse_event(
    event: crossterm::event::MouseEvent,
    client_pub: &crate::client::ClientRequestSender,
    state: &SharedState,
) -> Result<()> {
    tracing::debug!("Handling a mouse event");

    let enable_volume_scroll = config::get_config().app_config.enable_mouse_scroll_volume;
    let enable_page_scroll = config::get_config().app_config.enable_mouse_navigation;
    let playback_rect = state.ui.lock().playback_window_rect;
    let pointer_in_playback = rect_contains_point(playback_rect, event.column, event.row);
    let pointer_on_volume = matches!(
        state.ui.lock().workspace_hit_at(event.column, event.row),
        Some(
            WorkspaceHit::PlaybackOption(crate::state::WorkspacePlaybackOption::Volume)
                | WorkspaceHit::VolumeMenu
        )
    );

    if matches!(event.kind, crossterm::event::MouseEventKind::Moved) {
        let mut ui = state.ui.lock();
        let action_popup = matches!(
            &ui.popup,
            Some(
                PopupState::ActionList(..)
                    | PopupState::AnchoredActionList { .. }
                    | PopupState::DiagnosticActions { .. }
            )
        );
        if action_popup {
            if let Some(index) = ui.workspace_popup_hit_at(event.column, event.row) {
                if let Some(list) = ui.popup.as_mut().and_then(PopupState::list_state_mut) {
                    list.select(Some(index));
                }
            }
        } else if ui.popup.is_some() {
            if let Some(index) = ui.workspace_popup_hit_at(event.column, event.row) {
                if let Some(list) = ui.popup.as_mut().and_then(PopupState::list_state_mut) {
                    list.select(Some(index));
                }
            }
            // A focused popup owns hover state even over its border or empty
            // area; the page behind it must not react.
            ui.clear_workspace_pointer();
        } else if enable_page_scroll && ui.popup.is_none() {
            ui.set_workspace_pointer(event.column, event.row);
        } else {
            ui.clear_workspace_pointer();
        }
        return Ok(());
    }

    {
        let mut ui = state.ui.lock();
        if welcome_editor_is_open(&ui) {
            handle_welcome_editor_mouse(event, enable_page_scroll, client_pub, &mut ui);
            return Ok(());
        }
    }

    if handle_volume_popup_mouse(event, client_pub, state)? {
        return Ok(());
    }

    if matches!(
        event.kind,
        crossterm::event::MouseEventKind::Down(crossterm::event::MouseButton::Right)
    ) {
        let mut ui = state.ui.lock();
        if ui.popup.is_none() {
            let hit = ui.workspace_hit_at(event.column, event.row);
            if let Some(
                hit @ (WorkspaceHit::PlaybackOption(crate::state::WorkspacePlaybackOption::Volume)
                | WorkspaceHit::VolumeMenu),
            ) = hit
            {
                if let Some(anchor) = ui.workspace_hit_rect(hit) {
                    open_volume_popup(&mut ui, anchor);
                }
                return Ok(());
            }
        }
        if enable_page_scroll && !pointer_in_playback && ui.popup.is_none() {
            if let Some(hit) = ui.workspace_hit_at(event.column, event.row) {
                if let Some(anchor) = ui.workspace_hit_rect(hit) {
                    page::handle_workspace_context_menu_hit(
                        hit, anchor, client_pub, state, &mut ui,
                    )?;
                }
            }
        }
        return Ok(());
    }

    if matches!(
        event.kind,
        crossterm::event::MouseEventKind::Down(crossterm::event::MouseButton::Left)
    ) {
        let mut ui = state.ui.lock();
        if matches!(ui.popup, Some(PopupState::CommandHelp { .. })) {
            if !ui.popup_contains_point(event.column, event.row) {
                ui.popup = None;
                ui.clear_popup_hit_regions();
                return Ok(());
            }
            if let Some(index) = ui.workspace_popup_hit_at(event.column, event.row) {
                if let Some(PopupState::CommandHelp { scroll_offset }) = ui.popup.as_mut() {
                    *scroll_offset = index;
                }
            }
            // Help owns the click even when it lands on empty popup space;
            // never let a click fall through to the page underneath.
            return Ok(());
        }
        if ui.popup.is_none() && matches!(ui.current_page(), PageState::CommandHelp { .. }) {
            if let Some(index) = ui.workspace_popup_hit_at(event.column, event.row) {
                if let PageState::CommandHelp { scroll_offset } = ui.current_page_mut() {
                    *scroll_offset = index;
                }
                return Ok(());
            }
        }
        if matches!(ui.popup, Some(PopupState::WorkspaceScope { .. })) {
            if !ui.popup_contains_point(event.column, event.row) {
                ui.popup = None;
                ui.clear_popup_hit_regions();
                return Ok(());
            }
            if let Some(index) = ui.workspace_popup_hit_at(event.column, event.row) {
                if let Some(list) = ui.popup.as_mut().and_then(PopupState::list_state_mut) {
                    list.select(Some(index));
                }
                page::choose_workspace_scope(index, client_pub, state, &mut ui)?;
            }
            return Ok(());
        }
        if matches!(
            ui.popup,
            Some(
                PopupState::ActionList(..)
                    | PopupState::AnchoredActionList { .. }
                    | PopupState::DiagnosticActions { .. }
            )
        ) {
            if !ui.popup_contains_point(event.column, event.row) {
                ui.popup = None;
                ui.clear_popup_hit_regions();
                return Ok(());
            }
            if matches!(ui.popup, Some(PopupState::AnchoredActionList { .. })) {
                if let Some(index) = ui.workspace_popup_hit_at(event.column, event.row) {
                    popup::handle_item_action(index, client_pub, state, &mut ui)?;
                }
            } else if let Some(index) = ui.workspace_popup_hit_at(event.column, event.row) {
                if let Some(list) = ui.popup.as_mut().and_then(PopupState::list_state_mut) {
                    list.select(Some(index));
                }
                // Item actions run on click, through the same path as Enter.
                // Diagnostic actions (tracing, captures) only select, so a
                // stray click never starts one.
                let runs_on_click = matches!(ui.popup, Some(PopupState::ActionList(..)));
                if let Some(key_sequence) = runs_on_click
                    .then(|| {
                        config::get_config()
                            .keymap_config
                            .key_sequence_for_command(Command::ChooseSelected)
                    })
                    .flatten()
                    .cloned()
                {
                    popup::handle_key_sequence_for_popup(
                        &key_sequence,
                        client_pub,
                        state,
                        &mut ui,
                    )?;
                }
            }
            return Ok(());
        }
        if ui.popup.is_some() && !ui.popup_contains_point(event.column, event.row) {
            ui.popup = None;
            ui.clear_popup_hit_regions();
            return Ok(());
        }
        if ui.popup.as_ref().and_then(PopupState::list_state).is_some() {
            if let Some(index) = ui.workspace_popup_hit_at(event.column, event.row) {
                if let Some(list) = ui.popup.as_mut().and_then(PopupState::list_state_mut) {
                    list.select(Some(index));
                }
                if let Some(key_sequence) = config::get_config()
                    .keymap_config
                    .key_sequence_for_command(Command::ChooseSelected)
                    .cloned()
                {
                    popup::handle_key_sequence_for_popup(
                        &key_sequence,
                        client_pub,
                        state,
                        &mut ui,
                    )?;
                }
            }
            // Ordinary list popups own their pointer events. A row click
            // selects and activates through the same ChooseSelected path as
            // the keyboard; empty popup space remains consumed.
            return Ok(());
        }
        if ui.popup.is_none() {
            if enable_page_scroll {
                ui.set_workspace_pointer(event.column, event.row);
            }
            if let Some(hit) = ui.workspace_hit_at(event.column, event.row) {
                // Header controls are buttons: a single click activates the
                // same command as their keyboard equivalent. Content rows
                // retain the existing select-then-activate click policy.
                let activate = matches!(
                    hit,
                    WorkspaceHit::CloseWindow
                        | WorkspaceHit::Help
                        | WorkspaceHit::PlaybackOption(_)
                        | WorkspaceHit::VolumeMenu
                ) || ui.workspace_click_activates(hit);
                if page::handle_workspace_mouse_hit(
                    hit,
                    activate,
                    event.column,
                    event.row,
                    client_pub,
                    state,
                    &mut ui,
                )? {
                    return Ok(());
                }
            }
        }
    }
    let wheel_owner = mouse_wheel_owner(
        pointer_in_playback,
        pointer_on_volume,
        enable_volume_scroll,
        enable_page_scroll,
    );

    let horizontal = match event.kind {
        crossterm::event::MouseEventKind::ScrollLeft => Some(-1),
        crossterm::event::MouseEventKind::ScrollRight => Some(1),
        crossterm::event::MouseEventKind::ScrollUp
            if event
                .modifiers
                .contains(crossterm::event::KeyModifiers::SHIFT) =>
        {
            Some(-1)
        }
        crossterm::event::MouseEventKind::ScrollDown
            if event
                .modifiers
                .contains(crossterm::event::KeyModifiers::SHIFT) =>
        {
            Some(1)
        }
        _ => None,
    };
    if let Some(delta) = horizontal.filter(|_| wheel_owner == MouseWheelOwner::Page) {
        let mut ui = state.ui.lock();
        if ui.popup.is_none() && home::handle_horizontal_wheel(delta, state, &mut ui) {
            return Ok(());
        }
        if ui.popup.is_none() && matches!(ui.current_page(), PageState::Settings { .. }) {
            page::move_settings_tiles_horizontally(&mut ui, delta);
            ui.workspace_focus = crate::state::WorkspaceFocusState::Context;
            return Ok(());
        }
    }

    match event.kind {
        crossterm::event::MouseEventKind::ScrollUp if wheel_owner == MouseWheelOwner::Volume => {
            step_playback_volume(client_pub, state, true)?;
        }
        crossterm::event::MouseEventKind::ScrollDown if wheel_owner == MouseWheelOwner::Volume => {
            step_playback_volume(client_pub, state, false)?;
        }
        crossterm::event::MouseEventKind::ScrollUp if wheel_owner == MouseWheelOwner::Page => {
            handle_mouse_page_navigation(Command::SelectPreviousOrScrollUp, client_pub, state)?;
        }
        crossterm::event::MouseEventKind::ScrollDown if wheel_owner == MouseWheelOwner::Page => {
            handle_mouse_page_navigation(Command::SelectNextOrScrollDown, client_pub, state)?;
        }
        // a left click event
        crossterm::event::MouseEventKind::Down(crossterm::event::MouseButton::Left) => {
            let (toggle, progress) = {
                let ui = state.ui.lock();
                (ui.playback_toggle_rect, ui.playback_progress_bar_rect)
            };
            if rect_contains_point(toggle, event.column, event.row) {
                client_pub.send(ClientRequest::ActivePlaybackControl(
                    ActivePlaybackControl::Toggle,
                ))?;
            } else if let Some(seek) = playback_bar_seek(progress, event.column, event.row) {
                client_pub.send(ClientRequest::ActivePlaybackSeek(seek))?;
            }
        }
        _ => {}
    }
    Ok(())
}

fn handle_mouse_page_navigation(
    command: Command,
    client_pub: &crate::client::ClientRequestSender,
    state: &SharedState,
) -> Result<bool> {
    let Some(key_sequence) = config::get_config()
        .keymap_config
        .key_sequence_for_command(command)
        .cloned()
    else {
        return Ok(false);
    };
    let mut ui = state.ui.lock();
    if !page_accepts_mouse_navigation(ui.current_page(), ui.popup.is_some()) {
        return Ok(false);
    }
    ui.count_prefix = None;
    page::handle_key_sequence_for_page(&key_sequence, client_pub, state, &mut ui)
}

/// Whether a Welcome credential editor owns the pointer.
pub(crate) fn welcome_editor_is_open(ui: &crate::state::UIState) -> bool {
    // Settings edits `client_id` with the ordinary editor; only Welcome draws this one.
    matches!(ui.current_page(), PageState::Welcome { .. })
        && match &ui.popup {
            Some(PopupState::ConfigEdit { key, .. }) => {
                key == "client_id" || key.starts_with("welcome.youtube.")
            }
            Some(PopupState::ListenBrainzToken { .. }) => true,
            _ => false,
        }
}

/// The open Welcome editor owns every pointer event: a click outside closes
/// it, its controls act like their keys, and the wheel never reaches the
/// page behind it.
fn handle_welcome_editor_mouse(
    event: crossterm::event::MouseEvent,
    enabled: bool,
    client_pub: &crate::client::ClientRequestSender,
    ui: &mut UIStateGuard,
) {
    use crossterm::event::{MouseButton, MouseEventKind};
    if !enabled || event.kind != MouseEventKind::Down(MouseButton::Left) {
        return;
    }
    if !ui.popup_contains_point(event.column, event.row) {
        ui.popup = None;
        ui.clear_popup_hit_regions();
        return;
    }
    let enter = || KeySequence {
        keys: vec![Key::None(KeyCode::Enter)],
    };
    match ui.workspace_hit_at(event.column, event.row) {
        Some(WorkspaceHit::WelcomeEditorInput) => {
            let x = ui
                .workspace_hit_rect(WorkspaceHit::WelcomeEditorInput)
                .map_or(event.column, |rect| rect.x);
            let column = event.column.saturating_sub(x);
            match &mut ui.popup {
                Some(PopupState::ListenBrainzToken { input }) => input.set_cursor_column(column),
                Some(PopupState::ConfigEdit { input, .. }) => input.set_cursor_column(column),
                _ => {}
            }
        }
        Some(WorkspaceHit::WelcomeEditorConfirm) => {
            if matches!(ui.popup, Some(PopupState::ListenBrainzToken { .. })) {
                popup::handle_listenbrainz_token_popup(&enter(), client_pub, ui);
            } else {
                popup::handle_key_sequence_for_config_edit_popup(&enter(), client_pub, ui);
            }
        }
        Some(WorkspaceHit::WelcomeEditorCancel) => {
            if matches!(ui.popup, Some(PopupState::ListenBrainzToken { .. })) {
                ui.welcome_listenbrainz_pending = None;
            }
            ui.popup = None;
        }
        _ => {}
    }
}

fn rect_contains_point(rect: Rect, column: u16, row: u16) -> bool {
    rect.width > 0
        && rect.height > 0
        && column >= rect.x
        && column < rect.x.saturating_add(rect.width)
        && row >= rect.y
        && row < rect.y.saturating_add(rect.height)
}

fn playback_bar_seek(rect: Rect, column: u16, row: u16) -> Option<ActivePlaybackSeek> {
    (rect.width > 0
        && row == rect.y
        && column >= rect.x
        && column < rect.x.saturating_add(rect.width))
    .then(|| ActivePlaybackSeek::Fraction {
        numerator: column - rect.x,
        denominator: rect.width,
    })
}

/// Change the volume of `provider`'s playback.
pub(super) fn change_playback_volume(
    client_pub: &crate::client::ClientRequestSender,
    state: &SharedState,
    provider: config::ActiveProvider,
    change: impl FnOnce(u8) -> u8,
) -> Result<()> {
    match provider {
        config::ActiveProvider::Spotify => change_buffered_volume(client_pub, state, change),
        config::ActiveProvider::YouTubeMusic => change_youtube_volume(client_pub, state, change),
    }
}

/// Raise or lower the playing provider's volume by `volume_scroll_step`.
fn step_playback_volume(
    client_pub: &crate::client::ClientRequestSender,
    state: &SharedState,
    up: bool,
) -> Result<()> {
    let provider = {
        let active = state.ui.lock().active_provider;
        state.player.read().effective_playback_provider(active)
    };
    let step = config::get_config().app_config.volume_scroll_step;
    change_playback_volume(client_pub, state, provider, |volume| {
        if up {
            volume.saturating_add(step).min(100)
        } else {
            volume.saturating_sub(step)
        }
    })
}

pub(super) fn open_volume_popup(ui: &mut crate::state::UIState, anchor: Rect) {
    ui.popup = Some(PopupState::Volume {
        anchor,
        input: crate::ui::single_line_input::LineInput::new(Vec::new()),
    });
}

/// Pointer input while the volume popup is open. Clicks and drags on its
/// slider set the volume and the wheel over it steps the volume; a click
/// outside closes it. Returns whether the event was consumed.
fn handle_volume_popup_mouse(
    event: crossterm::event::MouseEvent,
    client_pub: &crate::client::ClientRequestSender,
    state: &SharedState,
) -> Result<bool> {
    use crossterm::event::{MouseButton, MouseEventKind};
    let mut ui = state.ui.lock();
    if !matches!(ui.popup, Some(PopupState::Volume { .. })) {
        return Ok(false);
    }
    let inside = ui.popup_contains_point(event.column, event.row);
    match event.kind {
        MouseEventKind::Down(MouseButton::Left) | MouseEventKind::Drag(MouseButton::Left)
            if inside =>
        {
            let slider = ui
                .workspace_popup_hits
                .iter()
                .find(|(_, index)| *index == 0)
                .map(|(rect, _)| *rect);
            let volume =
                slider.and_then(|rect| page::workspace_volume_at(rect, event.column, event.row));
            if let Some(volume) = volume {
                let provider = state
                    .player
                    .read()
                    .effective_playback_provider(ui.active_provider);
                drop(ui);
                change_playback_volume(client_pub, state, provider, |_| volume)?;
            }
            Ok(true)
        }
        MouseEventKind::ScrollUp | MouseEventKind::ScrollDown if inside => {
            drop(ui);
            step_playback_volume(client_pub, state, event.kind == MouseEventKind::ScrollUp)?;
            Ok(true)
        }
        MouseEventKind::Down(_) if !inside => {
            ui.popup = None;
            ui.clear_popup_hit_regions();
            // A right click may land on the slider and reopen the popup there.
            Ok(event.kind == MouseEventKind::Down(MouseButton::Left))
        }
        _ => Ok(inside),
    }
}

fn change_buffered_volume(
    client_pub: &crate::client::ClientRequestSender,
    state: &SharedState,
    change: impl FnOnce(u8) -> u8,
) -> Result<()> {
    let Some(new_volume) = ({
        let mut player = state.player.write();
        let Some(playback) = player.buffered_playback.as_mut() else {
            return Ok(());
        };
        let Some(volume) = playback.volume else {
            return Ok(());
        };

        let current_volume = volume.min(100) as u8;
        let new_volume = change(current_volume);
        if new_volume == current_volume {
            None
        } else {
            playback.volume = Some(u32::from(new_volume));
            Some(new_volume)
        }
    }) else {
        return Ok(());
    };

    client_pub.send(ClientRequest::Player(PlayerRequest::Volume(new_volume)))?;

    Ok(())
}

#[cfg(test)]
mod manual_row_scroll_tests {
    use super::handle_manual_row_scroll;
    use crate::{
        config::FocusedRowOverflow,
        key::{Key, KeySequence},
        state::{PopupState, TrackedMutex, UIState},
    };
    use crossterm::event::KeyCode;

    fn keys(code: KeyCode) -> KeySequence {
        KeySequence {
            keys: vec![Key::None(code)],
        }
    }

    #[test]
    fn left_and_right_scroll_only_an_overflowing_focused_row_outside_popups() {
        let mutex = TrackedMutex::new(UIState::default());
        let mut ui = mutex.lock();
        ui.presentation.focused_row_overflow = FocusedRowOverflow::Manual;
        ui.refresh_focused_marquee();
        ui.set_manual_scroll_extent(2);

        assert!(handle_manual_row_scroll(&keys(KeyCode::Right), &mut ui));
        assert!(handle_manual_row_scroll(&keys(KeyCode::Right), &mut ui));
        assert!(handle_manual_row_scroll(&keys(KeyCode::Left), &mut ui));
        ui.refresh_focused_marquee();
        assert_eq!(ui.focused_marquee_phase(), 1);

        assert!(!handle_manual_row_scroll(&keys(KeyCode::Up), &mut ui));
        ui.popup = Some(PopupState::Search {
            query: String::new(),
        });
        assert!(!handle_manual_row_scroll(&keys(KeyCode::Right), &mut ui));
    }
}

#[cfg(test)]
mod mouse_hit_tests {
    use super::popup;

    #[test]
    fn workspace_transport_toggle_click_dispatches_the_provider_neutral_command() {
        use super::*;
        crate::ui::initialize_test_config();
        let ring = std::sync::Arc::new(parking_lot::Mutex::new(std::collections::VecDeque::new()));
        let (diagnostics, _runtime) = crate::observability::disabled(ring);
        let state = std::sync::Arc::new(crate::state::State::new(false, diagnostics));
        let (sender, receiver) = crate::client::client_request_channel();
        {
            let mut ui = state.ui.lock();
            ui.playback_window_rect = Rect::new(0, 0, 180, 7);
            ui.playback_toggle_rect = Rect::new(4, 4, 2, 1);
        }

        handle_mouse_event(
            crossterm::event::MouseEvent {
                kind: crossterm::event::MouseEventKind::Down(crossterm::event::MouseButton::Left),
                column: 4,
                row: 4,
                modifiers: crossterm::event::KeyModifiers::NONE,
            },
            &sender,
            &state,
        )
        .unwrap();
        assert!(matches!(
            receiver.try_recv().unwrap().request(),
            ClientRequest::ActivePlaybackControl(ActivePlaybackControl::Toggle)
        ));
    }

    #[test]
    fn workspace_shuffle_and_repeat_clicks_dispatch_provider_commands_once() {
        use super::*;
        use crate::state::{LibraryPageUIState, PageState, WorkspaceHit, WorkspacePlaybackOption};

        for (option, expected) in [
            (
                WorkspacePlaybackOption::Shuffle,
                ClientRequest::Player(PlayerRequest::Shuffle),
            ),
            (
                WorkspacePlaybackOption::Repeat,
                ClientRequest::Player(PlayerRequest::Repeat),
            ),
        ] {
            crate::ui::initialize_test_config();
            let ring =
                std::sync::Arc::new(parking_lot::Mutex::new(std::collections::VecDeque::new()));
            let (diagnostics, _runtime) = crate::observability::disabled(ring);
            let state = std::sync::Arc::new(crate::state::State::new(false, diagnostics));
            let (sender, receiver) = crate::client::client_request_channel();
            let rect = Rect::new(40, 4, 8, 1);
            {
                let mut ui = state.ui.lock();
                ui.history.clear();
                ui.history.push(PageState::Library {
                    state: LibraryPageUIState::new(),
                });
                ui.workspace_hits
                    .push((rect, WorkspaceHit::PlaybackOption(option)));
            }

            handle_mouse_event(
                crossterm::event::MouseEvent {
                    kind: crossterm::event::MouseEventKind::Down(
                        crossterm::event::MouseButton::Left,
                    ),
                    column: rect.x,
                    row: rect.y,
                    modifiers: crossterm::event::KeyModifiers::NONE,
                },
                &sender,
                &state,
            )
            .unwrap();

            assert_eq!(
                receiver.try_recv().unwrap().request().operation_name(),
                expected.operation_name()
            );
            assert!(receiver.try_recv().is_err());
        }
    }

    #[test]
    fn workspace_playback_option_hover_is_tracked_inside_transport() {
        use super::*;
        use crate::state::{LibraryPageUIState, PageState, WorkspaceHit, WorkspacePlaybackOption};

        crate::ui::initialize_test_config();
        let ring = std::sync::Arc::new(parking_lot::Mutex::new(std::collections::VecDeque::new()));
        let (diagnostics, _runtime) = crate::observability::disabled(ring);
        let state = std::sync::Arc::new(crate::state::State::new(false, diagnostics));
        let (sender, receiver) = crate::client::client_request_channel();
        let rect = Rect::new(40, 4, 8, 1);
        {
            let mut ui = state.ui.lock();
            ui.history.clear();
            ui.history.push(PageState::Library {
                state: LibraryPageUIState::new(),
            });
            ui.playback_window_rect = Rect::new(0, 0, 180, 7);
            ui.workspace_hits.push((
                rect,
                WorkspaceHit::PlaybackOption(WorkspacePlaybackOption::Shuffle),
            ));
        }

        handle_mouse_event(
            crossterm::event::MouseEvent {
                kind: crossterm::event::MouseEventKind::Moved,
                column: rect.x,
                row: rect.y,
                modifiers: crossterm::event::KeyModifiers::NONE,
            },
            &sender,
            &state,
        )
        .unwrap();

        assert_eq!(state.ui.lock().workspace_hover_rect(), Some(rect));
        assert!(receiver.try_recv().is_err());
    }

    #[test]
    fn workspace_volume_mapping_rejects_boundary_misses() {
        use super::page::workspace_volume_at;

        let rect = Rect::new(10, 20, 11, 1);
        assert_eq!(workspace_volume_at(rect, 10, 20), Some(0));
        assert_eq!(workspace_volume_at(rect, 20, 20), Some(100));
        assert_eq!(workspace_volume_at(rect, 9, 20), None);
        assert_eq!(workspace_volume_at(rect, 21, 20), None);
        assert_eq!(workspace_volume_at(rect, 10, 19), None);
        assert_eq!(workspace_volume_at(rect, 10, 21), None);
    }

    #[test]
    fn workspace_volume_bar_click_routes_absolute_volume_to_playback_owner() {
        use super::*;
        use crate::state::{
            LibraryPageUIState, PageState, PlaybackMetadata, WorkspaceHit, WorkspacePlaybackOption,
            YouTubePlayback, YouTubeTrack,
        };

        for provider in [
            config::ActiveProvider::Spotify,
            config::ActiveProvider::YouTubeMusic,
        ] {
            crate::ui::initialize_test_config();
            let ring =
                std::sync::Arc::new(parking_lot::Mutex::new(std::collections::VecDeque::new()));
            let (diagnostics, _runtime) = crate::observability::disabled(ring);
            let state = std::sync::Arc::new(crate::state::State::new(false, diagnostics));
            let (sender, receiver) = crate::client::client_request_channel();
            let rect = Rect::new(40, 4, 11, 1);
            {
                let mut ui = state.ui.lock();
                ui.active_provider = provider;
                ui.history.clear();
                ui.history.push(PageState::Library {
                    state: LibraryPageUIState::new(),
                });
                ui.workspace_hits.push((
                    rect,
                    WorkspaceHit::PlaybackOption(WorkspacePlaybackOption::Volume),
                ));
            }
            {
                let mut player = state.player.write();
                player.active_playback_provider = Some(provider);
                match provider {
                    config::ActiveProvider::Spotify => {
                        player.buffered_playback = Some(PlaybackMetadata {
                            device_name: "unified-player".to_owned(),
                            device_id: Some("integrated-device".to_owned()),
                            volume: Some(35),
                            is_playing: true,
                            repeat_state: rspotify::model::RepeatState::Off,
                            shuffle_state: false,
                            mute_state: None,
                        });
                    }
                    config::ActiveProvider::YouTubeMusic => {
                        player.youtube_playback = Some(YouTubePlayback {
                            track: YouTubeTrack {
                                id: "video".to_owned(),
                                name: "title".to_owned(),
                                artists: "artist".to_owned(),
                                album: None,
                                duration: "1:00".to_owned(),
                                explicit: false,
                                thumbnail_url: None,
                                is_video: false,
                            },
                            is_playing: true,
                            progress: std::time::Duration::ZERO,
                            volume: 35,
                            mute_state: Some(35),
                            route: Default::default(),
                        });
                    }
                }
            }

            handle_mouse_event(
                crossterm::event::MouseEvent {
                    kind: crossterm::event::MouseEventKind::Down(
                        crossterm::event::MouseButton::Left,
                    ),
                    column: rect.x + rect.width - 1,
                    row: rect.y,
                    modifiers: crossterm::event::KeyModifiers::NONE,
                },
                &sender,
                &state,
            )
            .unwrap();

            let message = receiver.try_recv().unwrap();
            let request = message.request();
            assert!(matches!(
                request,
                ClientRequest::Player(PlayerRequest::Volume(100))
                    | ClientRequest::YouTubePlayer(YouTubePlayerRequest::Volume(100))
            ));
            let player = state.player.read();
            match provider {
                config::ActiveProvider::Spotify => {
                    assert_eq!(player.buffered_playback.as_ref().unwrap().volume, Some(100));
                    assert_eq!(player.buffered_playback.as_ref().unwrap().mute_state, None);
                }
                config::ActiveProvider::YouTubeMusic => {
                    assert_eq!(player.youtube_playback.as_ref().unwrap().volume, 100);
                    assert_eq!(player.youtube_playback.as_ref().unwrap().mute_state, None);
                }
            }
        }
    }

    #[test]
    fn volume_popup_opens_on_right_click_and_sets_the_volume_by_pointer_or_keys() {
        use super::*;
        use crate::key::{Key, KeySequence};
        use crate::state::{LibraryPageUIState, PageState, PlaybackMetadata, WorkspaceHit};
        crate::ui::initialize_test_config();
        let ring = std::sync::Arc::new(parking_lot::Mutex::new(std::collections::VecDeque::new()));
        let (diagnostics, _runtime) = crate::observability::disabled(ring);
        let state = std::sync::Arc::new(crate::state::State::new(false, diagnostics));
        let (sender, receiver) = crate::client::client_request_channel();
        let icon = Rect::new(70, 20, 2, 1);
        {
            let mut ui = state.ui.lock();
            ui.active_provider = config::ActiveProvider::Spotify;
            ui.history.clear();
            ui.history.push(PageState::Library {
                state: LibraryPageUIState::new(),
            });
            ui.playback_window_rect = Rect::new(0, 18, 80, 4);
            ui.workspace_hits.push((icon, WorkspaceHit::VolumeMenu));
        }
        state.player.write().buffered_playback = Some(PlaybackMetadata {
            device_name: "unified-player".to_owned(),
            device_id: Some("integrated-device".to_owned()),
            volume: Some(35),
            is_playing: true,
            repeat_state: rspotify::model::RepeatState::Off,
            shuffle_state: false,
            mute_state: None,
        });
        let mouse = |kind, column, row| {
            handle_mouse_event(
                crossterm::event::MouseEvent {
                    kind,
                    column,
                    row,
                    modifiers: crossterm::event::KeyModifiers::NONE,
                },
                &sender,
                &state,
            )
            .unwrap();
        };
        let volume = || {
            state
                .player
                .read()
                .buffered_playback
                .as_ref()
                .unwrap()
                .volume
        };
        let left = crossterm::event::MouseEventKind::Down(crossterm::event::MouseButton::Left);
        let right = crossterm::event::MouseEventKind::Down(crossterm::event::MouseButton::Right);

        mouse(right, icon.x, icon.y);
        assert!(matches!(
            state.ui.lock().popup,
            Some(PopupState::Volume { anchor, .. }) if anchor == icon
        ));

        // The renderer records the popup and its slider; stand in for it.
        let slider = Rect::new(52, 13, 21, 1);
        {
            let mut ui = state.ui.lock();
            ui.popup_rect = Rect::new(50, 12, 26, 6);
            ui.workspace_popup_hits.push((slider, 0));
        }
        mouse(left, slider.right() - 1, slider.y);
        assert_eq!(volume(), Some(100));
        mouse(crossterm::event::MouseEventKind::ScrollDown, 51, 15);
        assert_eq!(volume(), Some(95));

        let press = |code| {
            let mut ui = state.ui.lock();
            popup::handle_key_sequence_for_popup(
                &KeySequence {
                    keys: vec![Key::None(code)],
                },
                &sender,
                &state,
                &mut ui,
            )
            .unwrap()
        };
        assert!(press(crossterm::event::KeyCode::Left));
        assert_eq!(volume(), Some(94));
        assert!(press(crossterm::event::KeyCode::Char('4')));
        assert!(press(crossterm::event::KeyCode::Char('2')));
        assert!(press(crossterm::event::KeyCode::Enter));
        assert_eq!(volume(), Some(42));
        assert!(state.ui.lock().popup.is_none());
        let requests: Vec<_> = receiver
            .try_iter()
            .map(|message| message.request().clone())
            .collect();
        assert!(matches!(
            requests.last(),
            Some(ClientRequest::Player(PlayerRequest::Volume(42)))
        ));

        // A click outside closes a reopened popup without reaching the page.
        mouse(right, icon.x, icon.y);
        mouse(left, 5, 5);
        assert!(state.ui.lock().popup.is_none());
    }

    #[test]
    fn welcome_live_click_dispatches_once_and_matches_keyboard_feedback() {
        use super::*;
        use crate::state::{WelcomePageUIState, WelcomeStep, WorkspaceHit};
        crate::ui::initialize_test_config();
        let ring = std::sync::Arc::new(parking_lot::Mutex::new(std::collections::VecDeque::new()));
        let (diagnostics, _runtime) = crate::observability::disabled(ring);
        let state = std::sync::Arc::new(crate::state::State::new(false, diagnostics));
        let (sender, receiver) = crate::client::client_request_channel();
        {
            let mut ui = state.ui.lock();
            let mut welcome = WelcomePageUIState::new();
            welcome.show_step(WelcomeStep::Spotify);
            ui.history = vec![PageState::Welcome {
                state: welcome,
                from_settings: false,
            }];
            ui.popup = None;
            ui.playback_window_rect = Rect::default();
            // The last frame drew "Check existing session" here.
            ui.workspace_hits = vec![(Rect::new(10, 10, 20, 1), WorkspaceHit::WelcomeAction(3))];
        }
        let click = crossterm::event::MouseEvent {
            kind: crossterm::event::MouseEventKind::Down(crossterm::event::MouseButton::Left),
            column: 10,
            row: 10,
            modifiers: crossterm::event::KeyModifiers::NONE,
        };
        // The first click selects, like a Settings row; the repeat activates.
        handle_mouse_event(click, &sender, &state).unwrap();
        assert_eq!(receiver.len(), 0);
        assert_eq!(state.ui.lock().current_page().selected_index(), Some(3));
        handle_mouse_event(click, &sender, &state).unwrap();
        assert_eq!(receiver.len(), 1);
        // A further click while the request is in flight does not dispatch again.
        handle_mouse_event(click, &sender, &state).unwrap();
        assert_eq!(receiver.len(), 1);
        let mut ui = state.ui.lock();
        // Keyboard parity while the click's request is in flight: re-activation
        // is blocked with the waiting notice instead of dispatching again.
        page::handle_command_for_welcome_page(Command::ChooseSelected, &sender, &mut ui).unwrap();
        assert_eq!(receiver.len(), 1);
        assert_eq!(
            ui.welcome_spotify_notice.as_deref(),
            Some(crate::state::WELCOME_SPOTIFY_AUTH_IN_FLIGHT_NOTICE)
        );
    }

    #[test]
    fn welcome_live_wheel_and_hover_use_the_workspace_pointer() {
        use super::*;
        use crate::state::{WelcomePageUIState, WelcomeStep, WorkspaceHit};
        use crossterm::event::{KeyModifiers, MouseEvent, MouseEventKind};
        crate::ui::initialize_test_config();
        let ring = std::sync::Arc::new(parking_lot::Mutex::new(std::collections::VecDeque::new()));
        let (diagnostics, _runtime) = crate::observability::disabled(ring);
        let state = std::sync::Arc::new(crate::state::State::new(false, diagnostics));
        let (sender, receiver) = crate::client::client_request_channel();
        let row = Rect::new(10, 10, 20, 1);
        {
            let mut ui = state.ui.lock();
            let mut welcome = WelcomePageUIState::new();
            welcome.show_step(WelcomeStep::Spotify);
            ui.history = vec![PageState::Welcome {
                state: welcome,
                from_settings: false,
            }];
            ui.popup = None;
            ui.playback_window_rect = Rect::default();
            ui.workspace_hits = vec![(row, WorkspaceHit::WelcomeAction(2))];
        }
        let at = |kind| MouseEvent {
            kind,
            column: row.x,
            row: row.y,
            modifiers: KeyModifiers::NONE,
        };
        handle_mouse_event(at(MouseEventKind::Moved), &sender, &state).unwrap();
        assert_eq!(state.ui.lock().workspace_hover_rect(), Some(row));
        handle_mouse_event(at(MouseEventKind::ScrollDown), &sender, &state).unwrap();
        handle_mouse_event(at(MouseEventKind::ScrollDown), &sender, &state).unwrap();
        assert_eq!(state.ui.lock().current_page().selected_index(), Some(2));
        assert!(receiver.try_recv().is_err());
    }

    use super::{
        handle_key_event, handle_mouse_event, mouse_wheel_owner, page_accepts_mouse_navigation,
        playback_bar_seek, rect_contains_point, MouseWheelOwner,
    };
    use crate::{
        client::ActivePlaybackSeek,
        state::{LibraryPageUIState, PageState, SearchFocusState, SearchPageUIState},
        ui::single_line_input::LineInput,
    };
    use ratatui::layout::Rect;

    #[test]
    fn playback_hit_area_is_inclusive_at_top_left_and_exclusive_at_bottom_right() {
        let rect = Rect::new(4, 8, 10, 3);

        assert!(rect_contains_point(rect, 4, 8));
        assert!(rect_contains_point(rect, 13, 10));
        assert!(!rect_contains_point(rect, 14, 10));
        assert!(!rect_contains_point(rect, 13, 11));
        assert!(!rect_contains_point(Rect::default(), 0, 0));
    }

    #[test]
    fn only_the_volume_slider_turns_the_wheel_into_volume() {
        assert_eq!(
            mouse_wheel_owner(true, true, true, true),
            MouseWheelOwner::Volume
        );
        // Elsewhere on the playback surface the wheel does nothing, and it
        // never falls through to the page behind.
        assert_eq!(
            mouse_wheel_owner(true, false, true, true),
            MouseWheelOwner::None
        );
        assert_eq!(
            mouse_wheel_owner(true, true, false, true),
            MouseWheelOwner::None
        );
        assert_eq!(
            mouse_wheel_owner(false, false, true, true),
            MouseWheelOwner::Page
        );
        assert_eq!(
            mouse_wheel_owner(false, false, true, false),
            MouseWheelOwner::None
        );
    }

    #[test]
    fn popup_and_search_input_keep_background_page_still() {
        let library = PageState::Library {
            state: LibraryPageUIState::new(),
        };
        assert!(page_accepts_mouse_navigation(&library, false));
        assert!(!page_accepts_mouse_navigation(&library, true));

        let mut search_state = SearchPageUIState::new();
        let search = |state| PageState::Search {
            line_input: LineInput::default(),
            current_query: String::new(),
            state,
        };
        assert!(!page_accepts_mouse_navigation(
            &search(search_state.clone()),
            false
        ));
        search_state.focus = SearchFocusState::Tracks;
        assert!(page_accepts_mouse_navigation(&search(search_state), false));
    }

    #[test]
    fn playback_bar_click_produces_a_provider_neutral_fraction() {
        let rect = Rect::new(4, 8, 10, 1);
        assert_eq!(
            playback_bar_seek(rect, 9, 8),
            Some(ActivePlaybackSeek::Fraction {
                numerator: 5,
                denominator: 10,
            })
        );
        assert_eq!(playback_bar_seek(rect, 14, 8), None);
        assert_eq!(playback_bar_seek(rect, 3, 8), None);
        assert_eq!(playback_bar_seek(rect, 9, 9), None);
    }

    #[test]
    fn workspace_mouse_move_updates_hover_without_dispatching_a_command() {
        use crate::state::{WorkspaceHit, WorkspaceNavigationItem};

        crate::ui::initialize_test_config();
        let ring = std::sync::Arc::new(parking_lot::Mutex::new(std::collections::VecDeque::new()));
        let (diagnostics, _runtime) = crate::observability::disabled(ring);
        let state = std::sync::Arc::new(crate::state::State::new(false, diagnostics));
        let (sender, receiver) = crate::client::client_request_channel();
        let rect = Rect::new(10, 10, 4, 2);
        {
            let mut ui = state.ui.lock();
            ui.history.clear();
            ui.history.push(PageState::Library {
                state: LibraryPageUIState::new(),
            });
            ui.workspace_hits.push((
                rect,
                WorkspaceHit::Navigation(WorkspaceNavigationItem::Albums),
            ));
        }

        handle_mouse_event(
            crossterm::event::MouseEvent {
                kind: crossterm::event::MouseEventKind::Moved,
                column: 10,
                row: 10,
                modifiers: crossterm::event::KeyModifiers::NONE,
            },
            &sender,
            &state,
        )
        .unwrap();
        assert_eq!(state.ui.lock().workspace_hover_rect(), Some(rect));
        assert!(receiver.try_recv().is_err());

        handle_mouse_event(
            crossterm::event::MouseEvent {
                kind: crossterm::event::MouseEventKind::Moved,
                column: 14,
                row: 10,
                modifiers: crossterm::event::KeyModifiers::NONE,
            },
            &sender,
            &state,
        )
        .unwrap();
        assert_eq!(state.ui.lock().workspace_hover_rect(), None);
    }

    #[test]
    fn workspace_close_button_dispatches_previous_page_on_a_single_click() {
        use crate::state::{LibraryPageUIState, PageState, WorkspaceHit};

        crate::ui::initialize_test_config();
        let ring = std::sync::Arc::new(parking_lot::Mutex::new(std::collections::VecDeque::new()));
        let (diagnostics, _runtime) = crate::observability::disabled(ring);
        let state = std::sync::Arc::new(crate::state::State::new(false, diagnostics));
        let (sender, receiver) = crate::client::client_request_channel();
        let close_rect = ratatui::layout::Rect::new(40, 1, 7, 1);
        {
            let mut ui = state.ui.lock();
            ui.history.clear();
            ui.history.push(PageState::Library {
                state: LibraryPageUIState::new(),
            });
            ui.history.push(PageState::Library {
                state: LibraryPageUIState::new(),
            });
            ui.workspace_hits
                .push((close_rect, WorkspaceHit::CloseWindow));
        }

        handle_mouse_event(
            crossterm::event::MouseEvent {
                kind: crossterm::event::MouseEventKind::Down(crossterm::event::MouseButton::Left),
                column: close_rect.x,
                row: close_rect.y,
                modifiers: crossterm::event::KeyModifiers::NONE,
            },
            &sender,
            &state,
        )
        .unwrap();

        assert_eq!(state.ui.lock().history.len(), 1);
        assert!(receiver.try_recv().is_err());
    }

    #[test]
    fn workspace_help_hit_opens_the_command_help_popup_on_a_single_click() {
        use crate::state::{LibraryPageUIState, PageState, PopupState, WorkspaceHit};

        crate::ui::initialize_test_config();
        let ring = std::sync::Arc::new(parking_lot::Mutex::new(std::collections::VecDeque::new()));
        let (diagnostics, _runtime) = crate::observability::disabled(ring);
        let state = std::sync::Arc::new(crate::state::State::new(false, diagnostics));
        let (sender, receiver) = crate::client::client_request_channel();
        let help_rect = ratatui::layout::Rect::new(40, 18, 10, 1);
        {
            let mut ui = state.ui.lock();
            ui.history.clear();
            ui.history.push(PageState::Library {
                state: LibraryPageUIState::new(),
            });
            ui.workspace_hits.push((help_rect, WorkspaceHit::Help));
        }

        handle_mouse_event(
            crossterm::event::MouseEvent {
                kind: crossterm::event::MouseEventKind::Down(crossterm::event::MouseButton::Left),
                column: help_rect.x,
                row: help_rect.y,
                modifiers: crossterm::event::KeyModifiers::NONE,
            },
            &sender,
            &state,
        )
        .unwrap();

        let ui = state.ui.lock();
        assert_eq!(ui.history.len(), 1);
        assert!(matches!(ui.popup, Some(PopupState::CommandHelp { .. })));
        assert!(receiver.try_recv().is_err());
    }

    #[test]
    fn workspace_question_mark_opens_help_popup_without_replacing_the_page() {
        use crate::state::{LibraryPageUIState, PageState, PopupState};

        crate::ui::initialize_test_config();
        let ring = std::sync::Arc::new(parking_lot::Mutex::new(std::collections::VecDeque::new()));
        let (diagnostics, _runtime) = crate::observability::disabled(ring);
        let state = std::sync::Arc::new(crate::state::State::new(false, diagnostics));
        let (sender, receiver) = crate::client::client_request_channel();
        {
            let mut ui = state.ui.lock();
            ui.history.clear();
            ui.history.push(PageState::Library {
                state: LibraryPageUIState::new(),
            });
        }

        handle_key_event(
            crossterm::event::KeyEvent::new(
                crossterm::event::KeyCode::Char('?'),
                crossterm::event::KeyModifiers::NONE,
            ),
            &sender,
            &state,
        )
        .unwrap();

        let ui = state.ui.lock();
        assert_eq!(ui.history.len(), 1);
        assert!(matches!(ui.current_page(), PageState::Library { .. }));
        assert!(matches!(ui.popup, Some(PopupState::CommandHelp { .. })));
        assert!(receiver.try_recv().is_err());
    }

    #[test]
    fn command_help_popup_keeps_navigation_and_escape_ownership() {
        use crate::state::{PageState, PopupState};

        crate::ui::initialize_test_config();
        let ring = std::sync::Arc::new(parking_lot::Mutex::new(std::collections::VecDeque::new()));
        let (diagnostics, _runtime) = crate::observability::disabled(ring);
        let state = std::sync::Arc::new(crate::state::State::new(false, diagnostics));
        let (sender, receiver) = crate::client::client_request_channel();
        let mut ui = state.ui.lock();
        ui.history.clear();
        ui.history.push(PageState::Library {
            state: LibraryPageUIState::new(),
        });
        ui.popup = Some(PopupState::CommandHelp { scroll_offset: 0 });

        assert!(popup::handle_key_sequence_for_popup(
            &crate::key::KeySequence {
                keys: vec![crate::key::Key::None(crossterm::event::KeyCode::Char('j'),)],
            },
            &sender,
            &state,
            &mut ui,
        )
        .unwrap());
        assert!(matches!(
            ui.popup,
            Some(PopupState::CommandHelp { scroll_offset: 1 })
        ));

        assert!(popup::handle_key_sequence_for_popup(
            &crate::key::KeySequence {
                keys: vec![crate::key::Key::None(crossterm::event::KeyCode::Esc)],
            },
            &sender,
            &state,
            &mut ui,
        )
        .unwrap());
        assert!(ui.popup.is_none());
        assert!(matches!(ui.current_page(), PageState::Library { .. }));
        assert!(receiver.try_recv().is_err());
    }

    #[test]
    fn command_help_popup_mouse_click_selects_a_row_without_background_dispatch() {
        use crate::state::{LibraryPageUIState, PageState, PopupState};

        crate::ui::initialize_test_config();
        let ring = std::sync::Arc::new(parking_lot::Mutex::new(std::collections::VecDeque::new()));
        let (diagnostics, _runtime) = crate::observability::disabled(ring);
        let state = std::sync::Arc::new(crate::state::State::new(false, diagnostics));
        let (sender, receiver) = crate::client::client_request_channel();
        let hit_rect = ratatui::layout::Rect::new(10, 8, 24, 1);
        {
            let mut ui = state.ui.lock();
            ui.history.clear();
            ui.history.push(PageState::Library {
                state: LibraryPageUIState::new(),
            });
            ui.popup = Some(PopupState::CommandHelp { scroll_offset: 0 });
            ui.workspace_popup_hits.push((hit_rect, 3));
        }

        handle_mouse_event(
            crossterm::event::MouseEvent {
                kind: crossterm::event::MouseEventKind::Down(crossterm::event::MouseButton::Left),
                column: hit_rect.x,
                row: hit_rect.y,
                modifiers: crossterm::event::KeyModifiers::NONE,
            },
            &sender,
            &state,
        )
        .unwrap();

        let ui = state.ui.lock();
        assert!(matches!(
            ui.popup,
            Some(PopupState::CommandHelp { scroll_offset: 3 })
        ));
        assert!(matches!(ui.current_page(), PageState::Library { .. }));
        assert!(receiver.try_recv().is_err());
    }

    #[test]
    fn ordinary_list_popup_mouse_click_activates_selected_row_without_background_dispatch() {
        use crate::state::{LibraryPageUIState, PageState, PopupState};

        crate::ui::initialize_test_config();
        let ring = std::sync::Arc::new(parking_lot::Mutex::new(std::collections::VecDeque::new()));
        let (diagnostics, _runtime) = crate::observability::disabled(ring);
        let state = std::sync::Arc::new(crate::state::State::new(false, diagnostics));
        let (sender, receiver) = crate::client::client_request_channel();
        let hit_rect = ratatui::layout::Rect::new(10, 8, 24, 1);
        {
            let mut ui = state.ui.lock();
            ui.history.clear();
            ui.history.push(PageState::Library {
                state: LibraryPageUIState::new(),
            });
            ui.popup = Some(PopupState::ThemeList(
                vec![crate::config::Theme::default(); 4],
                ratatui::widgets::ListState::default(),
            ));
            ui.workspace_popup_hits.push((hit_rect, 2));
        }

        handle_mouse_event(
            crossterm::event::MouseEvent {
                kind: crossterm::event::MouseEventKind::Down(crossterm::event::MouseButton::Left),
                column: hit_rect.x,
                row: hit_rect.y,
                modifiers: crossterm::event::KeyModifiers::NONE,
            },
            &sender,
            &state,
        )
        .unwrap();

        let ui = state.ui.lock();
        assert!(ui.popup.is_none());
        assert!(matches!(ui.current_page(), PageState::Library { .. }));
        assert!(receiver.try_recv().is_err());
    }

    #[test]
    fn ordinary_list_popup_mouse_move_updates_hovered_row_without_dispatch() {
        use crate::state::{LibraryPageUIState, PageState, PopupState};

        crate::ui::initialize_test_config();
        let ring = std::sync::Arc::new(parking_lot::Mutex::new(std::collections::VecDeque::new()));
        let (diagnostics, _runtime) = crate::observability::disabled(ring);
        let state = std::sync::Arc::new(crate::state::State::new(false, diagnostics));
        let (sender, receiver) = crate::client::client_request_channel();
        let hit_rect = ratatui::layout::Rect::new(10, 8, 24, 1);
        {
            let mut ui = state.ui.lock();
            ui.history.clear();
            ui.history.push(PageState::Library {
                state: LibraryPageUIState::new(),
            });
            ui.popup = Some(PopupState::ThemeList(
                vec![crate::config::Theme::default(); 4],
                ratatui::widgets::ListState::default(),
            ));
            ui.workspace_popup_hits.push((hit_rect, 2));
        }

        handle_mouse_event(
            crossterm::event::MouseEvent {
                kind: crossterm::event::MouseEventKind::Moved,
                column: hit_rect.x,
                row: hit_rect.y,
                modifiers: crossterm::event::KeyModifiers::NONE,
            },
            &sender,
            &state,
        )
        .unwrap();

        let ui = state.ui.lock();
        assert_eq!(
            ui.popup.as_ref().and_then(PopupState::list_selected),
            Some(2)
        );
        assert!(receiver.try_recv().is_err());
    }

    #[test]
    fn ordinary_popup_outside_click_closes_without_background_dispatch() {
        use crate::state::{LibraryPageUIState, PageState, PopupState};

        crate::ui::initialize_test_config();
        let ring = std::sync::Arc::new(parking_lot::Mutex::new(std::collections::VecDeque::new()));
        let (diagnostics, _runtime) = crate::observability::disabled(ring);
        let state = std::sync::Arc::new(crate::state::State::new(false, diagnostics));
        let (sender, receiver) = crate::client::client_request_channel();
        {
            let mut ui = state.ui.lock();
            ui.history.clear();
            ui.history.push(PageState::Library {
                state: LibraryPageUIState::new(),
            });
            ui.popup = Some(PopupState::ThemeList(
                vec![crate::config::Theme::default(); 4],
                ratatui::widgets::ListState::default(),
            ));
            ui.popup_rect = Rect::new(10, 8, 24, 5);
        }

        handle_mouse_event(
            crossterm::event::MouseEvent {
                kind: crossterm::event::MouseEventKind::Down(crossterm::event::MouseButton::Left),
                column: 2,
                row: 2,
                modifiers: crossterm::event::KeyModifiers::NONE,
            },
            &sender,
            &state,
        )
        .unwrap();

        let ui = state.ui.lock();
        assert!(ui.popup.is_none());
        assert_eq!(ui.popup_rect, Rect::default());
        assert!(receiver.try_recv().is_err());
    }

    #[test]
    fn action_list_popup_mouse_navigation_selects_without_background_dispatch() {
        use crate::state::{LibraryPageUIState, PageState, PopupState};

        crate::ui::initialize_test_config();
        let ring = std::sync::Arc::new(parking_lot::Mutex::new(std::collections::VecDeque::new()));
        let (diagnostics, _runtime) = crate::observability::disabled(ring);
        let state = std::sync::Arc::new(crate::state::State::new(false, diagnostics));
        let (sender, receiver) = crate::client::client_request_channel();
        let hit_rect = ratatui::layout::Rect::new(10, 8, 24, 1);
        {
            let mut ui = state.ui.lock();
            ui.history.clear();
            ui.history.push(PageState::Library {
                state: LibraryPageUIState::new(),
            });
            ui.popup = Some(PopupState::DiagnosticActions {
                target: crate::observability::DiagnosticRowId::WorkersEmpty,
                actions: vec![
                    crate::observability::DiagnosticAction::ExplainState,
                    crate::observability::DiagnosticAction::StopTrace,
                ],
                state: ratatui::widgets::ListState::default(),
            });
            ui.popup_rect = Rect::new(8, 6, 30, 6);
            ui.workspace_popup_hits.push((hit_rect, 1));
        }

        handle_mouse_event(
            crossterm::event::MouseEvent {
                kind: crossterm::event::MouseEventKind::Moved,
                column: hit_rect.x,
                row: hit_rect.y,
                modifiers: crossterm::event::KeyModifiers::NONE,
            },
            &sender,
            &state,
        )
        .unwrap();
        handle_mouse_event(
            crossterm::event::MouseEvent {
                kind: crossterm::event::MouseEventKind::Down(crossterm::event::MouseButton::Left),
                column: hit_rect.x,
                row: hit_rect.y,
                modifiers: crossterm::event::KeyModifiers::NONE,
            },
            &sender,
            &state,
        )
        .unwrap();

        let ui = state.ui.lock();
        assert_eq!(
            ui.popup.as_ref().and_then(PopupState::list_selected),
            Some(1)
        );
        assert!(matches!(ui.current_page(), PageState::Library { .. }));
        assert!(receiver.try_recv().is_err());
    }

    #[test]
    fn queue_workspace_row_mouse_hit_selects_the_full_queue_page() {
        use crate::state::{PageState, WorkspaceFocusState, WorkspaceHit};

        crate::ui::initialize_test_config();
        let ring = std::sync::Arc::new(parking_lot::Mutex::new(std::collections::VecDeque::new()));
        let (diagnostics, _runtime) = crate::observability::disabled(ring);
        let state = std::sync::Arc::new(crate::state::State::new(false, diagnostics));
        let (sender, receiver) = crate::client::client_request_channel();
        let row = Rect::new(30, 8, 40, 1);
        {
            let mut ui = state.ui.lock();
            ui.history.clear();
            ui.history.push(PageState::new_queue());
            ui.workspace_hits.push((row, WorkspaceHit::QueueRow(1)));
        }

        handle_mouse_event(
            crossterm::event::MouseEvent {
                kind: crossterm::event::MouseEventKind::Down(crossterm::event::MouseButton::Left),
                column: row.x,
                row: row.y,
                modifiers: crossterm::event::KeyModifiers::NONE,
            },
            &sender,
            &state,
        )
        .unwrap();

        let ui = state.ui.lock();
        assert!(matches!(ui.current_page(), PageState::Queue { .. }));
        assert_eq!(ui.current_page().selected_index(), Some(1));
        assert_eq!(ui.workspace_focus, WorkspaceFocusState::Queue);
        assert!(receiver.try_recv().is_err());
    }

    #[test]
    fn journal_workspace_row_mouse_hit_selects_without_background_dispatch() {
        use crate::state::{JournalSelection, PageState, WorkspaceHit};

        crate::ui::initialize_test_config();
        let ring = std::sync::Arc::new(parking_lot::Mutex::new(std::collections::VecDeque::new()));
        let (diagnostics, _runtime) = crate::observability::disabled(ring);
        let state = std::sync::Arc::new(crate::state::State::new(false, diagnostics));
        let (sender, receiver) = crate::client::client_request_channel();
        let row = Rect::new(30, 8, 40, 1);
        {
            let mut ui = state.ui.lock();
            ui.history.clear();
            ui.history.push(PageState::Journal {
                table: ratatui::widgets::TableState::default(),
                journal_selection: JournalSelection::default(),
            });
            ui.workspace_hits.push((row, WorkspaceHit::JournalRow(1)));
        }

        handle_mouse_event(
            crossterm::event::MouseEvent {
                kind: crossterm::event::MouseEventKind::Down(crossterm::event::MouseButton::Left),
                column: row.x,
                row: row.y,
                modifiers: crossterm::event::KeyModifiers::NONE,
            },
            &sender,
            &state,
        )
        .unwrap();

        let ui = state.ui.lock();
        assert_eq!(ui.current_page().selected_index(), Some(1));
        assert!(receiver.try_recv().is_err());
    }

    #[test]
    fn session_history_workspace_row_mouse_hit_selects_without_background_dispatch() {
        use crate::state::{PageState, WorkspaceHit};

        crate::ui::initialize_test_config();
        let ring = std::sync::Arc::new(parking_lot::Mutex::new(std::collections::VecDeque::new()));
        let (diagnostics, _runtime) = crate::observability::disabled(ring);
        let state = std::sync::Arc::new(crate::state::State::new(false, diagnostics));
        let (sender, receiver) = crate::client::client_request_channel();
        let row = Rect::new(30, 8, 40, 1);
        {
            let mut ui = state.ui.lock();
            ui.history.clear();
            ui.history.push(PageState::SessionHistory {
                list: ratatui::widgets::ListState::default(),
            });
            ui.workspace_hits
                .push((row, WorkspaceHit::SessionHistoryRow(1)));
        }

        handle_mouse_event(
            crossterm::event::MouseEvent {
                kind: crossterm::event::MouseEventKind::Down(crossterm::event::MouseButton::Left),
                column: row.x,
                row: row.y,
                modifiers: crossterm::event::KeyModifiers::NONE,
            },
            &sender,
            &state,
        )
        .unwrap();

        let ui = state.ui.lock();
        assert_eq!(ui.current_page().selected_index(), Some(1));
        assert!(receiver.try_recv().is_err());
    }

    #[test]
    fn workspace_scope_popup_mouse_hit_is_bounded_and_dispatches_account_selection() {
        use crate::state::{WorkspaceScopeKind, WorkspaceScopeOption, WorkspaceScopeSelection};

        crate::ui::initialize_test_config();
        let ring = std::sync::Arc::new(parking_lot::Mutex::new(std::collections::VecDeque::new()));
        let (diagnostics, _runtime) = crate::observability::disabled(ring);
        let state = std::sync::Arc::new(crate::state::State::new(false, diagnostics));
        let (sender, receiver) = crate::client::client_request_channel();
        let hit_rect = Rect::new(20, 12, 24, 1);
        {
            let mut ui = state.ui.lock();
            ui.history.clear();
            ui.history.push(PageState::Library {
                state: LibraryPageUIState::new(),
            });
            ui.popup = Some(crate::state::PopupState::WorkspaceScope {
                kind: WorkspaceScopeKind::Account,
                options: vec![WorkspaceScopeOption {
                    label: "Work".to_owned(),
                    selection: WorkspaceScopeSelection::Account {
                        provider: crate::config::ActiveProvider::Spotify,
                        account_id: "spotify-2".to_owned(),
                    },
                }],
                state: ratatui::widgets::ListState::default().with_selected(Some(0)),
                anchor: Rect::new(2, 30, 22, 1),
            });
            ui.workspace_popup_hits.push((hit_rect, 0));
        }

        handle_mouse_event(
            crossterm::event::MouseEvent {
                kind: crossterm::event::MouseEventKind::Down(crossterm::event::MouseButton::Left),
                column: 19,
                row: 12,
                modifiers: crossterm::event::KeyModifiers::NONE,
            },
            &sender,
            &state,
        )
        .unwrap();
        assert!(state.ui.lock().popup.is_some());
        assert!(receiver.try_recv().is_err());

        handle_mouse_event(
            crossterm::event::MouseEvent {
                kind: crossterm::event::MouseEventKind::Down(crossterm::event::MouseButton::Left),
                column: 20,
                row: 12,
                modifiers: crossterm::event::KeyModifiers::NONE,
            },
            &sender,
            &state,
        )
        .unwrap();
        assert!(state.ui.lock().popup.is_none());
        assert!(matches!(
            receiver.try_recv().unwrap().request(),
            crate::client::ClientRequest::ManageAccount(
                crate::client::AccountOperation::Switch { provider, account_id }
            ) if *provider == crate::config::ActiveProvider::Spotify && account_id == "spotify-2"
        ));
    }

    #[test]
    fn workspace_scope_popup_keyboard_choose_closes_without_background_dispatch() {
        use crate::state::{WorkspaceScopeKind, WorkspaceScopeOption, WorkspaceScopeSelection};

        crate::ui::initialize_test_config();
        let ring = std::sync::Arc::new(parking_lot::Mutex::new(std::collections::VecDeque::new()));
        let (diagnostics, _runtime) = crate::observability::disabled(ring);
        let state = std::sync::Arc::new(crate::state::State::new(false, diagnostics));
        let (sender, receiver) = crate::client::client_request_channel();
        let mut ui = state.ui.lock();
        ui.popup = Some(crate::state::PopupState::WorkspaceScope {
            kind: WorkspaceScopeKind::Browsing,
            options: vec![WorkspaceScopeOption {
                label: "Spotify".to_owned(),
                selection: WorkspaceScopeSelection::Provider(
                    crate::config::ActiveProvider::Spotify,
                ),
            }],
            state: ratatui::widgets::ListState::default().with_selected(Some(0)),
            anchor: Rect::new(2, 30, 22, 1),
        });

        assert!(popup::handle_key_sequence_for_popup(
            &crate::key::KeySequence {
                keys: vec![crate::key::Key::None(crossterm::event::KeyCode::Enter)],
            },
            &sender,
            &state,
            &mut ui,
        )
        .unwrap());
        assert!(ui.popup.is_none());
        assert!(receiver.try_recv().is_err());
    }

    #[test]
    fn workspace_right_click_routes_a_track_row_to_an_anchored_action_popup() {
        use crate::state::{SearchFocusState, SearchPageUIState, SearchResults, WorkspaceHit};

        crate::ui::initialize_test_config();
        let ring = std::sync::Arc::new(parking_lot::Mutex::new(std::collections::VecDeque::new()));
        let (diagnostics, _runtime) = crate::observability::disabled(ring);
        let state = std::sync::Arc::new(crate::state::State::new(false, diagnostics));
        let (sender, receiver) = crate::client::client_request_channel();
        let track = crate::state::Track {
            id: rspotify::model::TrackId::from_id("4iV5W9uYEdYUVa79Axb7Rh")
                .unwrap()
                .into_static(),
            name: "Example track".to_owned(),
            artists: Vec::new(),
            album: None,
            duration: std::time::Duration::from_secs(180),
            explicit: false,
            added_at: 0,
        };
        state.data.write().caches.search.insert(
            "right click".to_owned(),
            std::sync::Arc::new(SearchResults {
                tracks: vec![track],
                ..SearchResults::default()
            }),
            std::time::Duration::from_secs(60),
        );
        let anchor = Rect::new(30, 12, 40, 1);
        {
            let mut ui = state.ui.lock();
            ui.history.clear();
            ui.history.push(PageState::Search {
                line_input: crate::ui::single_line_input::LineInput::default(),
                current_query: "right click".to_owned(),
                state: SearchPageUIState::new(),
            });
            ui.workspace_hits.push((
                anchor,
                WorkspaceHit::SearchRow {
                    focus: SearchFocusState::Tracks,
                    index: 0,
                },
            ));
        }

        handle_mouse_event(
            crossterm::event::MouseEvent {
                kind: crossterm::event::MouseEventKind::Down(crossterm::event::MouseButton::Right),
                column: anchor.x,
                row: anchor.y,
                modifiers: crossterm::event::KeyModifiers::NONE,
            },
            &sender,
            &state,
        )
        .unwrap();
        assert!(matches!(
            &state.ui.lock().popup,
            Some(crate::state::PopupState::AnchoredActionList {
                anchor: popup_anchor,
                ..
            }) if *popup_anchor == anchor
        ));
        assert!(receiver.try_recv().is_err());
    }

    #[test]
    fn anchored_action_popup_mouse_hit_runs_the_existing_action_dispatcher() {
        use rspotify::prelude::Id;

        crate::ui::initialize_test_config();
        let ring = std::sync::Arc::new(parking_lot::Mutex::new(std::collections::VecDeque::new()));
        let (diagnostics, _runtime) = crate::observability::disabled(ring);
        let state = std::sync::Arc::new(crate::state::State::new(false, diagnostics));
        let (sender, receiver) = crate::client::client_request_channel();
        let track = crate::state::Track {
            id: rspotify::model::TrackId::from_id("4iV5W9uYEdYUVa79Axb7Rh")
                .unwrap()
                .into_static(),
            name: "Example track".to_owned(),
            artists: Vec::new(),
            album: None,
            duration: std::time::Duration::from_secs(180),
            explicit: false,
            added_at: 0,
        };
        let action_rect = Rect::new(32, 14, 20, 1);
        {
            let mut ui = state.ui.lock();
            ui.history.clear();
            ui.history.push(PageState::Library {
                state: LibraryPageUIState::new(),
            });
            ui.popup = Some(crate::state::PopupState::AnchoredActionList {
                item: Box::new(crate::state::ActionListItem::Track(
                    track,
                    vec![crate::command::Action::AddToQueue],
                )),
                state: ratatui::widgets::ListState::default().with_selected(Some(0)),
                anchor: Rect::new(2, 10, 20, 1),
            });
            ui.popup_rect = Rect::new(30, 12, 30, 5);
            ui.workspace_popup_hits.push((action_rect, 0));
        }

        handle_mouse_event(
            crossterm::event::MouseEvent {
                kind: crossterm::event::MouseEventKind::Down(crossterm::event::MouseButton::Left),
                column: action_rect.x,
                row: action_rect.y,
                modifiers: crossterm::event::KeyModifiers::NONE,
            },
            &sender,
            &state,
        )
        .unwrap();
        assert!(state.ui.lock().popup.is_none());
        assert!(matches!(
            receiver.try_recv().unwrap().request(),
            crate::client::ClientRequest::AddPlayableToQueue(id)
                if id.id() == "4iV5W9uYEdYUVa79Axb7Rh"
        ));
    }

    #[test]
    fn action_popup_outside_click_closes_without_background_dispatch() {
        use crate::state::{ActionListItem, LibraryPageUIState, PageState, PopupState, Track};

        crate::ui::initialize_test_config();
        let ring = std::sync::Arc::new(parking_lot::Mutex::new(std::collections::VecDeque::new()));
        let (diagnostics, _runtime) = crate::observability::disabled(ring);
        let state = std::sync::Arc::new(crate::state::State::new(false, diagnostics));
        let (sender, receiver) = crate::client::client_request_channel();
        let track = Track {
            id: rspotify::model::TrackId::from_id("4iV5W9uYEdYUVa79Axb7Rh")
                .unwrap()
                .into_static(),
            name: "Example track".to_owned(),
            artists: Vec::new(),
            album: None,
            duration: std::time::Duration::from_secs(180),
            explicit: false,
            added_at: 0,
        };

        for popup in [0, 1, 2] {
            let mut ui = state.ui.lock();
            ui.history.clear();
            ui.history.push(PageState::Library {
                state: LibraryPageUIState::new(),
            });
            ui.popup = Some(match popup {
                0 => PopupState::ActionList(
                    Box::new(ActionListItem::Track(
                        track.clone(),
                        vec![crate::command::Action::AddToQueue],
                    )),
                    ratatui::widgets::ListState::default(),
                ),
                1 => PopupState::AnchoredActionList {
                    item: Box::new(ActionListItem::Track(
                        track.clone(),
                        vec![crate::command::Action::AddToQueue],
                    )),
                    state: ratatui::widgets::ListState::default(),
                    anchor: Rect::new(2, 10, 20, 1),
                },
                _ => PopupState::DiagnosticActions {
                    target: crate::observability::DiagnosticRowId::WorkersEmpty,
                    actions: vec![crate::observability::DiagnosticAction::ExplainState],
                    state: ratatui::widgets::ListState::default(),
                },
            });
            ui.popup_rect = Rect::new(20, 10, 30, 6);
            ui.workspace_popup_hits.clear();
            drop(ui);

            handle_mouse_event(
                crossterm::event::MouseEvent {
                    kind: crossterm::event::MouseEventKind::Down(
                        crossterm::event::MouseButton::Left,
                    ),
                    column: 5,
                    row: 5,
                    modifiers: crossterm::event::KeyModifiers::NONE,
                },
                &sender,
                &state,
            )
            .unwrap();

            let ui = state.ui.lock();
            assert!(ui.popup.is_none(), "popup variant {popup} remained open");
            assert!(ui.workspace_popup_hits.is_empty());
        }
        assert!(receiver.try_recv().is_err());
    }

    #[test]
    fn centred_action_popup_row_click_runs_the_action() {
        use crate::state::{ActionListItem, LibraryPageUIState, PageState, PopupState, Track};

        crate::ui::initialize_test_config();
        let ring = std::sync::Arc::new(parking_lot::Mutex::new(std::collections::VecDeque::new()));
        let (diagnostics, _runtime) = crate::observability::disabled(ring);
        let state = std::sync::Arc::new(crate::state::State::new(false, diagnostics));
        let (sender, receiver) = crate::client::client_request_channel();
        let track = Track {
            id: rspotify::model::TrackId::from_id("4iV5W9uYEdYUVa79Axb7Rh")
                .unwrap()
                .into_static(),
            name: "Example track".to_owned(),
            artists: Vec::new(),
            album: None,
            duration: std::time::Duration::from_secs(180),
            explicit: false,
            added_at: 0,
        };
        {
            let mut ui = state.ui.lock();
            ui.history.clear();
            ui.history.push(PageState::Library {
                state: LibraryPageUIState::new(),
            });
            ui.popup = Some(PopupState::ActionList(
                Box::new(ActionListItem::Track(
                    track,
                    vec![crate::command::Action::AddToQueue],
                )),
                ratatui::widgets::ListState::default(),
            ));
            ui.popup_rect = Rect::new(20, 10, 30, 6);
            ui.workspace_popup_hits = vec![(Rect::new(21, 11, 28, 1), 0)];
        }

        handle_mouse_event(
            crossterm::event::MouseEvent {
                kind: crossterm::event::MouseEventKind::Down(crossterm::event::MouseButton::Left),
                column: 25,
                row: 11,
                modifiers: crossterm::event::KeyModifiers::NONE,
            },
            &sender,
            &state,
        )
        .unwrap();

        assert!(
            receiver.try_recv().is_ok(),
            "clicking a row dispatches its action"
        );
        assert!(state.ui.lock().popup.is_none());
    }
}

fn change_youtube_volume(
    client_pub: &crate::client::ClientRequestSender,
    state: &SharedState,
    change: impl FnOnce(u8) -> u8,
) -> Result<()> {
    let Some(new_volume) = ({
        let mut player = state.player.write();
        let Some(playback) = player.youtube_playback.as_mut() else {
            return Ok(());
        };

        let current_volume = playback.volume.min(100);
        let new_volume = change(current_volume);
        if new_volume == current_volume {
            None
        } else {
            playback.volume = new_volume;
            playback.mute_state = None;
            Some(new_volume)
        }
    }) else {
        return Ok(());
    };

    client_pub.send(ClientRequest::YouTubePlayer(YouTubePlayerRequest::Volume(
        new_volume,
    )))?;

    Ok(())
}

pub(super) fn import_youtube_auth(ui: &mut UIStateGuard) -> Result<()> {
    let configs = config::get_config();
    let auth_type = configs.app_config.youtube.auth_type;
    let content = get_clipboard_content()
        .context("get YouTube Music credentials from clipboard")?
        .trim()
        .to_owned();
    if content.is_empty() {
        anyhow::bail!("clipboard is empty; copy a Cookie header or OAuth token JSON first");
    }

    let path = match auth_type {
        config::YouTubeMusicAuthType::Browser => {
            if !content.contains('=') {
                anyhow::bail!("browser auth expects the full Cookie request-header value");
            }
            configs.youtube_music_cookie_path()
        }
        config::YouTubeMusicAuthType::OAuth => {
            serde_json::from_str::<serde_json::Value>(&content)
                .context("clipboard does not contain valid OAuth JSON")?;
            configs.youtube_music_oauth_path()
        }
        config::YouTubeMusicAuthType::Unauthenticated => {
            anyhow::bail!("set [youtube].auth_type to Browser or OAuth first")
        }
    };
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).context("create YouTube credential directory")?;
    }
    std::fs::write(&path, content).context("write YouTube Music credentials")?;
    ui.youtube_auth_status = configs.youtube_music_auth_status();
    tracing::info!("Imported YouTube Music credentials into the configured credential store");
    Ok(())
}

// Handle a terminal key pressed event
fn handle_key_event(
    event: crossterm::event::KeyEvent,
    client_pub: &crate::client::ClientRequestSender,
    state: &SharedState,
) -> Result<()> {
    let key: Key = event.into();
    let mut ui = state.ui.lock();

    let mut key_sequence = ui.input_key_sequence.clone();
    key_sequence.keys.push(key);

    // check if the current key sequence matches any keymap's prefix
    // if not, reset the key sequence
    let keymap_config = &config::get_config().keymap_config;
    if !keymap_config.has_matched_prefix(&key_sequence) {
        key_sequence = KeySequence { keys: vec![key] };
    }

    tracing::debug!("Handling a keyboard event");
    let handled = {
        if ui.popup.is_none() {
            page::handle_key_sequence_for_page(&key_sequence, client_pub, state, &mut ui)?
        } else {
            popup::handle_key_sequence_for_popup(&key_sequence, client_pub, state, &mut ui)?
        }
    };

    // if the key sequence is not handled, let the global handler handle it
    let handled = if handled {
        true
    } else {
        match keymap_config.find_command_or_action_from_key_sequence(&key_sequence) {
            Some(CommandOrAction::Action(action, target)) => {
                handle_global_action(action, target, client_pub, state, &mut ui)?
            }
            Some(CommandOrAction::Command(command)) => {
                handle_global_command(command, client_pub, state, &mut ui)?
            }
            None => handle_manual_row_scroll(&key_sequence, &mut ui),
        }
    };

    if !matches!(ui.current_page(), PageState::Welcome { state, .. } if state.step == crate::state::WelcomeStep::ListenBrainz)
    {
        ui.cancel_welcome_listenbrainz_check();
    }
    // if handled, clear the key sequence and count prefix
    // otherwise, the current key sequence can be a prefix of a command's shortcut
    if handled {
        ui.input_key_sequence.keys = vec![];
        ui.count_prefix = None;
        ui.bump_diagnostic_revision();
    } else {
        // update the count prefix if the key is a digit
        match key {
            Key::None(KeyCode::Char(c)) if c.is_ascii_digit() => {
                let digit = c.to_digit(10).unwrap() as usize;
                ui.input_key_sequence.keys = vec![];
                ui.count_prefix = match ui.count_prefix {
                    Some(count) => Some(count * 10 + digit),
                    None => {
                        if digit > 0 {
                            Some(digit)
                        } else {
                            None
                        }
                    }
                };
            }
            _ => {
                ui.input_key_sequence = key_sequence;
                ui.count_prefix = None;
            }
        }
    }
    Ok(())
}

/// Left/Right scroll the focused row in `Manual` overflow mode. Only keys
/// that no page handler or keymap claimed reach here, so Welcome buttons, the
/// search category bar, text inputs, and user bindings keep precedence.
/// Popups keep their own Left/Right handling.
fn handle_manual_row_scroll(key_sequence: &KeySequence, ui: &mut UIStateGuard) -> bool {
    if ui.popup.is_some() {
        return false;
    }
    match key_sequence.keys.as_slice() {
        [Key::None(KeyCode::Right)] => ui.scroll_focused_row(true),
        [Key::None(KeyCode::Left)] => ui.scroll_focused_row(false),
        _ => false,
    }
}

fn spotify_exact_removal_intent(
    ui: &UIStateGuard,
    data: &DataReadGuard,
    playlist_id: &PlaylistId<'static>,
    track: &Track,
) -> Option<crate::client::SpotifyMutationIntent> {
    let Context::Playlist { playlist, tracks } = data.caches.context.get(&playlist_id.uri())?
    else {
        return None;
    };
    let playlist_state = ui.current_page().mutable_playlist_state()?;
    let selected = playlist_state.selection().selected_visible_indices();
    let occurrence = if selected.len() == 1 {
        let position = playlist_state
            .selection()
            .visible_to_full_index(selected[0])
            .ok()?;
        crate::client::SpotifyExactOccurrence {
            position,
            snapshot_id: playlist.snapshot_id.clone(),
        }
    } else if selected.is_empty() {
        crate::client::SpotifyExactOccurrence::from_token(playlist_state.cursor_occurrence()?)?
    } else {
        return None;
    };
    if playlist.snapshot_id != occurrence.snapshot_id
        || tracks.get(occurrence.position)?.id != track.id
    {
        return None;
    }
    Some(crate::client::SpotifyMutationIntent::RemoveOccurrence {
        operation_id: crate::client::PlaylistMutationOperationId(rand::rng().random()),
        playlist_id: playlist_id.uri(),
        media_uri: track.id.uri(),
        occurrence,
    })
}

fn youtube_exact_removal_intent(
    ui: &UIStateGuard,
    track: &crate::state::YouTubeTrack,
) -> Option<crate::client::YouTubeMutationIntent> {
    let PageState::YouTubeContext {
        id: YouTubeContextId::Playlist(playlist_id),
        context: Some(context),
        ..
    } = ui.current_page()
    else {
        return None;
    };
    let playlist_state = ui.current_page().mutable_playlist_state()?;
    let selected = playlist_state.selection().selected_visible_indices();
    let (position, cursor_token) = if selected.len() == 1 {
        (
            playlist_state
                .selection()
                .visible_to_full_index(selected[0])
                .ok()?,
            None,
        )
    } else if selected.is_empty() {
        match playlist_state.cursor_occurrence()? {
            ProviderOccurrenceToken::YouTubeMusic {
                position,
                revision,
                set_video_id: Some(set_video_id),
            } => (*position, Some((revision.as_str(), set_video_id.as_str()))),
            _ => return None,
        }
    } else {
        return None;
    };
    let revision = crate::state::UnifiedPlaylist::youtube_tracks_snapshot_hash(&context.tracks);
    let set_video_id = context
        .playlist_set_video_ids
        .get(position)
        .and_then(|token| token.as_ref())?;
    if let Some((token_revision, token_set_video_id)) = cursor_token {
        if token_revision != revision || token_set_video_id != set_video_id {
            return None;
        }
    }
    if context.tracks.get(position)?.id != track.id {
        return None;
    }
    Some(crate::client::YouTubeMutationIntent::RemoveOccurrence {
        operation_id: crate::client::PlaylistMutationOperationId(rand::rng().random()),
        playlist_id: playlist_id.clone(),
        video_id: Some(track.id.clone()),
        set_video_id: Some(set_video_id.clone()),
    })
}

fn youtube_track_action_is_allowed(action: Action, exact_playlist_row: bool) -> bool {
    command::provider_capabilities(config::ActiveProvider::YouTubeMusic)
        .supports_track_action(action)
        || (action == Action::DeleteFromPlaylist && exact_playlist_row)
}

pub fn handle_action_in_context(
    action: Action,
    context: ActionContext,
    client_pub: &crate::client::ClientRequestSender,
    data: &DataReadGuard,
    ui: &mut UIStateGuard,
) -> Result<bool> {
    let exact_youtube_playlist_row = matches!(&context, ActionContext::YouTubeTrack(_))
        && matches!(
            ui.current_page(),
            PageState::YouTubeContext {
                id: YouTubeContextId::Playlist(_),
                ..
            }
        );
    if matches!(
        &context,
        ActionContext::YouTubeTrack(_) | ActionContext::YouTubeTracks(_)
    ) && !youtube_track_action_is_allowed(action, exact_youtube_playlist_row)
    {
        ui.set_unsupported_operation(
            "That action is unavailable for YouTube Music.",
            "Choose a supported YouTube Music action.",
        );
        return Ok(true);
    }

    if let ActionContext::Playlist(playlist) = &context {
        let modifiable = crate::state::spotify_playlist_is_modifiable(
            playlist,
            data.user_data.user.as_ref().map(|user| &user.id),
        );
        if action == Action::RenamePlaylist && !modifiable {
            ui.set_unsupported_operation(
                "Only owned or collaborative Spotify playlists can be renamed.",
                "Choose a playlist you can edit.",
            );
            return Ok(true);
        }
    }

    let bulk_plan = match &context {
        ActionContext::Tracks(tracks) => Some(bulk_action::plan_spotify_tracks(
            tracks,
            bulk_action::current_spotify_epoch(ui),
            action,
        )),
        ActionContext::YouTubeTracks(tracks) if action != Action::AddToJournalList => {
            Some(bulk_action::plan_youtube_tracks(
                tracks,
                bulk_action::current_youtube_epoch(ui),
                action,
            ))
        }
        _ => None,
    };
    if bulk_plan.as_ref().is_some_and(|result| result.is_err()) {
        return Ok(false);
    }

    match context {
        ActionContext::YouTubeTrack(track) => match action {
            Action::DeleteFromPlaylist => {
                if let Some(intent) = youtube_exact_removal_intent(ui, &track) {
                    ui.popup = Some(PopupState::ConfirmAction {
                        message: format!(
                            "Delete this occurrence of {} from the playlist?",
                            track.name
                        ),
                        action: ConfirmableAction::YouTubePlaylistMutation(intent),
                    });
                } else {
                    tracing::warn!("Exact YouTube playlist deletion refused by token gate");
                    ui.popup = None;
                }
                Ok(true)
            }
            Action::CopyLink => {
                let track_url = format!("https://music.youtube.com/watch?v={}", track.id);
                execute_copy_command(track_url)?;
                ui.popup = None;
                Ok(true)
            }
            Action::AddToPlaylist => {
                client_pub.send(ClientRequest::GetYouTubeLibrary)?;
                ui.popup = Some(PopupState::YouTubePlaylistList(
                    YouTubePlaylistPopupAction::AddTrack {
                        track,
                        search_query: String::new(),
                    },
                    ListState::default(),
                ));
                Ok(true)
            }
            Action::AddToJournalList => {
                open_add_youtube_tracks_to_journal_list_popup(vec![track], data, ui);
                Ok(true)
            }
            Action::AddToQueue => {
                client_pub.send(ClientRequest::AddItemsToUserQueue(vec![
                    crate::state::PlayableMedia::YouTube(track),
                ]))?;
                ui.popup = None;
                Ok(true)
            }
            Action::AddToLiked => {
                client_pub.send(ClientRequest::RateYouTubeTrack { track, liked: true })?;
                ui.popup = None;
                Ok(true)
            }
            Action::DeleteFromLiked => {
                client_pub.send(ClientRequest::RateYouTubeTrack {
                    track,
                    liked: false,
                })?;
                ui.popup = None;
                Ok(true)
            }
            _ => Ok(false),
        },
        ActionContext::YouTubeTracks(tracks) => match action {
            Action::CopyLink => {
                let plan = bulk_plan
                    .expect("multi YouTube action was prevalidated")
                    .map_err(|_| anyhow::anyhow!("bulk copy plan is unavailable"))?;
                let operation_ids = plan.operation_ids();
                let handle = bulk_action::start_bulk_local(ui, &plan, &operation_ids)?;
                let links = tracks
                    .iter()
                    .map(|track| format!("https://music.youtube.com/watch?v={}", track.id))
                    .collect::<Vec<_>>()
                    .join("\n");
                if let Err(error) = execute_copy_command(links) {
                    bulk_action::complete_bulk_local(ui, &handle, &operation_ids, false)?;
                    return Err(error);
                }
                bulk_action::complete_bulk_local(ui, &handle, &operation_ids, true)?;
                ui.popup = None;
                window::clear_track_selection(ui);
                Ok(true)
            }
            Action::AddToPlaylist => {
                let menu = bulk_action::youtube_bulk_action_menu(
                    &tracks,
                    &[Action::AddToPlaylist],
                    bulk_action::current_youtube_epoch(ui).value(),
                )
                .map_err(|_| anyhow::anyhow!("bulk playlist menu is unavailable"))?;
                client_pub.send(ClientRequest::GetYouTubeLibrary)?;
                ui.popup = Some(PopupState::YouTubePlaylistList(
                    YouTubePlaylistPopupAction::AddTracks {
                        tracks: menu,
                        search_query: String::new(),
                    },
                    ListState::default(),
                ));
                Ok(true)
            }
            Action::AddToJournalList => {
                open_add_youtube_tracks_to_journal_list_popup(tracks, data, ui);
                Ok(true)
            }
            Action::AddToQueue => {
                let plan = bulk_plan
                    .expect("multi YouTube action was prevalidated")
                    .map_err(|_| anyhow::anyhow!("bulk queue plan is unavailable"))?;
                let operation_ids = plan.operation_ids();
                let assignment = bulk_action::BulkRequestAssignment::new(
                    ClientRequest::AddItemsToUserQueue(
                        tracks
                            .into_iter()
                            .map(crate::state::PlayableMedia::YouTube)
                            .collect(),
                    ),
                    operation_ids,
                );
                bulk_action::dispatch_bulk_requests(ui, client_pub, &plan, vec![assignment])?;
                ui.popup = None;
                window::clear_track_selection(ui);
                Ok(true)
            }
            Action::AddToLiked => {
                let plan = bulk_plan
                    .expect("multi YouTube action was prevalidated")
                    .map_err(|_| anyhow::anyhow!("bulk like plan is unavailable"))?;
                let assignments = plan
                    .operation_ids()
                    .into_iter()
                    .zip(tracks)
                    .map(|(operation_id, track)| {
                        bulk_action::BulkRequestAssignment::one(
                            ClientRequest::RateYouTubeTrack { track, liked: true },
                            operation_id,
                        )
                    })
                    .collect();
                bulk_action::dispatch_bulk_requests(ui, client_pub, &plan, assignments)?;
                ui.popup = None;
                window::clear_track_selection(ui);
                Ok(true)
            }
            Action::DeleteFromLiked => {
                let plan = bulk_plan
                    .expect("multi YouTube unlike plan was prevalidated")
                    .map_err(|_| anyhow::anyhow!("bulk unlike plan is unavailable"))?;
                let assignments = plan
                    .operation_ids()
                    .into_iter()
                    .zip(tracks)
                    .map(|(operation_id, track)| {
                        bulk_action::BulkRequestAssignment::one(
                            ClientRequest::RateYouTubeTrack {
                                track,
                                liked: false,
                            },
                            operation_id,
                        )
                    })
                    .collect();
                bulk_action::dispatch_bulk_requests(ui, client_pub, &plan, assignments)?;
                ui.popup = None;
                window::clear_track_selection(ui);
                Ok(true)
            }
            _ => Ok(false),
        },
        ActionContext::Track(track) => match action {
            Action::GoToAlbum => {
                if let Some(album) = track.album {
                    let context_id = ContextId::Album(
                        AlbumId::from_uri(&parse_uri(&album.id.uri()))?.into_static(),
                    );
                    ui.new_page(PageState::Context {
                        id: None,
                        context_page_type: ContextPageType::Browsing(context_id),
                        state: None,
                    });
                    return Ok(true);
                }
                Ok(false)
            }
            Action::GoToArtist => {
                handle_go_to_artist(track.artists, ui);
                Ok(true)
            }
            Action::AddToQueue => {
                ui.spotify_queue_labels.remember_track(&track);
                client_pub.send(ClientRequest::AddPlayableToQueue(track.id.into()))?;
                ui.popup = None;
                Ok(true)
            }
            Action::AddToJournalList => {
                open_add_tracks_to_journal_list_popup(vec![track], data, ui);
                Ok(true)
            }
            Action::CopyLink => {
                let track_url = format!("https://open.spotify.com/track/{}", track.id.id());
                execute_copy_command(track_url)?;
                ui.popup = None;
                Ok(true)
            }
            Action::AddToPlaylist => {
                client_pub.send(ClientRequest::GetUserPlaylists)?;
                ui.popup = Some(PopupState::UserPlaylistList(
                    PlaylistPopupAction::AddTrack {
                        folder_id: 0,
                        track,
                        search_query: String::new(),
                    },
                    ListState::default(),
                ));
                Ok(true)
            }
            Action::ToggleLiked => {
                if data.user_data.is_liked_track(&track) {
                    client_pub.send(ClientRequest::DeleteFromLibrary(ItemId::Track(track.id)))?;
                } else {
                    client_pub.send(ClientRequest::AddToLibrary(Item::Track(track)))?;
                }
                ui.popup = None;
                Ok(true)
            }
            Action::AddToLiked => {
                client_pub.send(ClientRequest::AddToLibrary(Item::Track(track)))?;
                ui.popup = None;
                Ok(true)
            }
            Action::DeleteFromLiked => {
                client_pub.send(ClientRequest::DeleteFromLibrary(ItemId::Track(track.id)))?;
                ui.popup = None;
                Ok(true)
            }
            Action::GoToRadio => {
                handle_go_to_radio(&track.id.uri(), &track.name, ui, client_pub)?;
                Ok(true)
            }
            Action::ShowActionsOnArtist => {
                handle_show_actions_on_artist(track.artists, data, ui);
                Ok(true)
            }
            Action::ShowActionsOnAlbum => {
                if let Some(album) = track.album {
                    let context = ActionContext::Album(album.clone());
                    ui.popup = Some(PopupState::ActionList(
                        Box::new(ActionListItem::Album(
                            album,
                            context.get_available_actions(data),
                        )),
                        ListState::default(),
                    ));
                    return Ok(true);
                }
                Ok(false)
            }
            Action::ShowJournalActions => {
                let actions = command::construct_track_journal_actions(&track, data);
                ui.popup = Some(PopupState::ActionList(
                    Box::new(ActionListItem::Track(track, actions)),
                    ListState::default(),
                ));
                Ok(true)
            }
            Action::DeleteFromPlaylist => {
                if let PageState::Context {
                    id: Some(ContextId::Playlist(playlist_id)),
                    ..
                } = ui.current_page()
                {
                    if let Some(intent) =
                        spotify_exact_removal_intent(ui, data, playlist_id, &track)
                    {
                        ui.popup = Some(PopupState::ConfirmAction {
                            message: format!(
                                "Delete this occurrence of {} from the playlist?",
                                track.name
                            ),
                            action: ConfirmableAction::SpotifyPlaylistMutation(intent),
                        });
                    } else {
                        tracing::warn!("Exact playlist occurrence deletion refused by token gate");
                        ui.popup = None;
                    }
                } else {
                    tracing::warn!("Exact playlist occurrence deletion refused by page gate");
                    ui.popup = None;
                }
                Ok(true)
            }
            Action::SetRating => {
                let selected = data
                    .journal
                    .entry_for_track(&track)
                    .and_then(|entry| entry.rating)
                    .map(|rating| rating.saturating_sub(1) as usize)
                    .unwrap_or_default();
                let mut state = ListState::default();
                state.select(Some(selected));
                ui.popup = Some(PopupState::TrackRating { track, state });
                Ok(true)
            }
            Action::EditNote => {
                let note = data
                    .journal
                    .entry_for_track(&track)
                    .map(|entry| entry.note.clone())
                    .unwrap_or_default();
                ui.popup = Some(PopupState::TrackNote {
                    track,
                    input: LineInput::new(note.chars().collect()),
                });
                Ok(true)
            }
            _ => Ok(false),
        },
        ActionContext::Tracks(tracks) => match action {
            Action::CopyLink => {
                let plan = bulk_plan
                    .expect("multi Spotify action was prevalidated")
                    .map_err(|_| anyhow::anyhow!("bulk copy plan is unavailable"))?;
                let operation_ids = plan.operation_ids();
                let handle = bulk_action::start_bulk_local(ui, &plan, &operation_ids)?;
                let track_urls = tracks
                    .iter()
                    .map(|track| format!("https://open.spotify.com/track/{}", track.id.id()))
                    .collect::<Vec<_>>()
                    .join("\n");
                if let Err(error) = execute_copy_command(track_urls) {
                    bulk_action::complete_bulk_local(ui, &handle, &operation_ids, false)?;
                    return Err(error);
                }
                bulk_action::complete_bulk_local(ui, &handle, &operation_ids, true)?;
                ui.popup = None;
                Ok(true)
            }
            Action::AddToPlaylist => {
                let menu = bulk_action::spotify_bulk_action_menu(
                    &tracks,
                    &[Action::AddToPlaylist],
                    bulk_action::current_spotify_epoch(ui).value(),
                )
                .map_err(|_| anyhow::anyhow!("bulk playlist menu is unavailable"))?;
                client_pub.send(ClientRequest::GetUserPlaylists)?;
                ui.popup = Some(PopupState::UserPlaylistList(
                    PlaylistPopupAction::AddTracks {
                        folder_id: 0,
                        tracks: menu,
                        search_query: String::new(),
                    },
                    ListState::default(),
                ));
                Ok(true)
            }
            Action::AddToQueue => {
                let plan = bulk_plan
                    .expect("multi Spotify action was prevalidated")
                    .map_err(|_| anyhow::anyhow!("bulk queue plan is unavailable"))?;
                let assignments = plan
                    .operation_ids()
                    .into_iter()
                    .zip(tracks)
                    .map(|(operation_id, track)| {
                        ui.spotify_queue_labels.remember_track(&track);
                        bulk_action::BulkRequestAssignment::one(
                            ClientRequest::AddPlayableToQueue(track.id.into()),
                            operation_id,
                        )
                    })
                    .collect();
                bulk_action::dispatch_bulk_requests(ui, client_pub, &plan, assignments)?;
                ui.popup = None;
                window::clear_track_selection(ui);
                Ok(true)
            }
            Action::AddToLiked => {
                let plan = bulk_plan
                    .expect("multi Spotify action was prevalidated")
                    .map_err(|_| anyhow::anyhow!("bulk like plan is unavailable"))?;
                let operation_ids = plan.operation_ids();
                let assignment = bulk_action::BulkRequestAssignment::new(
                    ClientRequest::AddTracksToLibrary(tracks),
                    operation_ids,
                );
                bulk_action::dispatch_bulk_requests(ui, client_pub, &plan, vec![assignment])?;
                ui.popup = None;
                window::clear_track_selection(ui);
                Ok(true)
            }
            Action::DeleteFromLiked => {
                let plan = bulk_plan
                    .expect("multi Spotify action was prevalidated")
                    .map_err(|_| anyhow::anyhow!("bulk unlike plan is unavailable"))?;
                let operation_ids = plan.operation_ids();
                let assignment = bulk_action::BulkRequestAssignment::new(
                    ClientRequest::DeleteTracksFromLibrary(
                        tracks.into_iter().map(|track| track.id).collect(),
                    ),
                    operation_ids,
                );
                bulk_action::dispatch_bulk_requests(ui, client_pub, &plan, vec![assignment])?;
                ui.popup = None;
                window::clear_track_selection(ui);
                Ok(true)
            }
            Action::DeleteFromPlaylist => {
                if let PageState::Context {
                    id: Some(ContextId::Playlist(playlist_id)),
                    ..
                } = ui.current_page()
                {
                    let track_ids = tracks
                        .iter()
                        .map(|track| track.id.clone())
                        .collect::<Vec<_>>();
                    if window::playlist_delete_is_safe(data, playlist_id, Some(&track_ids)) {
                        let menu = bulk_action::spotify_bulk_action_menu(
                            &tracks,
                            &[Action::DeleteFromPlaylist],
                            bulk_action::current_spotify_epoch(ui).value(),
                        )
                        .map_err(|_| anyhow::anyhow!("bulk delete menu is unavailable"))?;
                        ui.popup = Some(PopupState::ConfirmAction {
                            message: format!(
                                "Delete {} tracks from this playlist?",
                                track_ids.len()
                            ),
                            action: ConfirmableAction::DeleteTracksFromPlaylist {
                                playlist_id: playlist_id.clone_static(),
                                tracks: menu,
                            },
                        });
                    } else {
                        tracing::warn!("Playlist deletion action refused by safety gate");
                        ui.popup = None;
                    }
                } else {
                    tracing::warn!("Playlist deletion action refused by safety gate");
                    ui.popup = None;
                }
                Ok(true)
            }
            _ => Ok(false),
        },
        ActionContext::Album(album) => match action {
            Action::GoToArtist => {
                handle_go_to_artist(album.artists, ui);
                Ok(true)
            }
            Action::GoToRadio => {
                handle_go_to_radio(&album.id.uri(), &album.name, ui, client_pub)?;
                Ok(true)
            }
            Action::ShowActionsOnArtist => {
                handle_show_actions_on_artist(album.artists, data, ui);
                Ok(true)
            }
            Action::AddToLibrary => {
                client_pub.send(ClientRequest::AddToLibrary(Item::Album(album)))?;
                ui.popup = None;
                Ok(true)
            }
            Action::DeleteFromLibrary => {
                ui.popup = Some(PopupState::ConfirmAction {
                    message: format!("Delete {} from your library?", album.name),
                    action: ConfirmableAction::DeleteFromLibrary(ItemId::Album(album.id)),
                });
                Ok(true)
            }
            Action::CopyLink => {
                let album_url = format!("https://open.spotify.com/album/{}", album.id.id());
                execute_copy_command(album_url)?;
                ui.popup = None;
                Ok(true)
            }
            Action::AddToQueue => {
                client_pub.send(ClientRequest::AddAlbumToQueue(album.id))?;
                ui.popup = None;
                Ok(true)
            }
            Action::AddAlbumToJournalList => {
                let Some(Context::Album { tracks, .. }) = data.caches.context.get(&album.id.uri())
                else {
                    tracing::warn!(
                        "Album tracks are not loaded; open the album context before adding it to a journal list"
                    );
                    ui.popup = None;
                    return Ok(true);
                };
                open_add_tracks_to_journal_list_popup(tracks.clone(), data, ui);
                Ok(true)
            }
            _ => Ok(false),
        },
        ActionContext::Artist(artist) => match action {
            Action::Follow => {
                client_pub.send(ClientRequest::AddToLibrary(Item::Artist(artist)))?;
                ui.popup = None;
                Ok(true)
            }
            Action::Unfollow => {
                client_pub.send(ClientRequest::DeleteFromLibrary(ItemId::Artist(artist.id)))?;
                ui.popup = None;
                Ok(true)
            }
            Action::CopyLink => {
                let artist_url = format!("https://open.spotify.com/artist/{}", artist.id.id());
                execute_copy_command(artist_url)?;
                ui.popup = None;
                Ok(true)
            }
            Action::GoToRadio => {
                handle_go_to_radio(&artist.id.uri(), &artist.name, ui, client_pub)?;
                Ok(true)
            }
            Action::AddArtistTracksToJournalList => {
                let Some(Context::Artist { top_tracks, .. }) =
                    data.caches.context.get(&artist.id.uri())
                else {
                    tracing::warn!(
                        "Artist tracks are not loaded; open the artist context before adding tracks to a journal list"
                    );
                    ui.popup = None;
                    return Ok(true);
                };
                open_add_tracks_to_journal_list_popup(top_tracks.clone(), data, ui);
                Ok(true)
            }
            _ => Ok(false),
        },
        ActionContext::Playlist(playlist) => match action {
            Action::AddToLibrary => {
                client_pub.send(ClientRequest::AddToLibrary(Item::Playlist(playlist)))?;
                ui.popup = None;
                Ok(true)
            }
            Action::GoToRadio => {
                handle_go_to_radio(&playlist.id.uri(), &playlist.name, ui, client_pub)?;
                Ok(true)
            }
            Action::CopyLink => {
                let playlist_url =
                    format!("https://open.spotify.com/playlist/{}", playlist.id.id());
                execute_copy_command(playlist_url)?;
                ui.popup = None;
                Ok(true)
            }
            Action::DeleteFromLibrary => {
                ui.popup = Some(PopupState::ConfirmAction {
                    message: format!("Delete {} from your library?", playlist.name),
                    action: ConfirmableAction::DeleteFromLibrary(ItemId::Playlist(playlist.id)),
                });
                Ok(true)
            }
            Action::RemovePlaylistFromLibrary => {
                ui.popup = Some(PopupState::ConfirmAction {
                    message: format!("Remove {} from your Spotify library?", playlist.name),
                    action: ConfirmableAction::DeleteFromLibrary(ItemId::Playlist(playlist.id)),
                });
                Ok(true)
            }
            Action::RenamePlaylist => {
                ui.popup = Some(PopupState::PlaylistName {
                    action: crate::state::PlaylistNameAction::Spotify {
                        playlist_id: playlist.id,
                    },
                    input: crate::ui::single_line_input::LineInput::new(
                        playlist.name.chars().collect(),
                    ),
                });
                Ok(true)
            }
            _ => Ok(false),
        },
        ActionContext::Show(show) => match action {
            Action::CopyLink => {
                let show_url = format!("https://open.spotify.com/show/{}", show.id.id());
                execute_copy_command(show_url)?;
                ui.popup = None;
                Ok(true)
            }
            Action::AddToLibrary => {
                client_pub.send(ClientRequest::AddToLibrary(Item::Show(show)))?;
                ui.popup = None;
                Ok(true)
            }
            Action::DeleteFromLibrary => {
                ui.popup = Some(PopupState::ConfirmAction {
                    message: format!("Delete {} from your library?", show.name),
                    action: ConfirmableAction::DeleteFromLibrary(ItemId::Show(show.id)),
                });
                Ok(true)
            }
            _ => Ok(false),
        },
        ActionContext::Episode(episode) => match action {
            Action::GoToShow => {
                if let Some(show) = episode.show {
                    let context_id = ContextId::Show(
                        ShowId::from_uri(&parse_uri(&show.id.uri()))?.into_static(),
                    );
                    ui.new_page(PageState::Context {
                        id: None,
                        context_page_type: ContextPageType::Browsing(context_id),
                        state: None,
                    });
                    return Ok(true);
                }
                Ok(false)
            }
            Action::AddToQueue => {
                ui.spotify_queue_labels.remember_episode(&episode);
                client_pub.send(ClientRequest::AddPlayableToQueue(episode.id.into()))?;
                ui.popup = None;
                Ok(true)
            }
            Action::CopyLink => {
                let episode_url = format!("https://open.spotify.com/episode/{}", episode.id.id());
                execute_copy_command(episode_url)?;
                ui.popup = None;
                Ok(true)
            }
            Action::AddToPlaylist => {
                client_pub.send(ClientRequest::GetUserPlaylists)?;
                ui.popup = Some(PopupState::UserPlaylistList(
                    PlaylistPopupAction::AddEpisode {
                        folder_id: 0,
                        episode_id: episode.id,
                        search_query: String::new(),
                    },
                    ListState::default(),
                ));
                Ok(true)
            }
            Action::ShowActionsOnShow => {
                if let Some(show) = episode.show {
                    let context = ActionContext::Show(show.clone());
                    ui.popup = Some(PopupState::ActionList(
                        Box::new(ActionListItem::Show(
                            show,
                            context.get_available_actions(data),
                        )),
                        ListState::default(),
                    ));
                    return Ok(true);
                }
                Ok(false)
            }
            _ => Ok(false),
        },
        // TODO: support actions for playlist folders
        ActionContext::PlaylistFolder(_) => Ok(false),
    }
}

pub(super) fn handle_track_journal_action(
    action: Action,
    track: Track,
    state: &SharedState,
    ui: &mut UIStateGuard,
) -> Result<bool> {
    match action {
        Action::SetRating | Action::EditNote => {
            let data = state.data.read();
            match action {
                Action::SetRating => {
                    let selected = data
                        .journal
                        .entry_for_track(&track)
                        .and_then(|entry| entry.rating)
                        .map(|rating| rating.saturating_sub(1) as usize)
                        .unwrap_or_default();
                    let mut list_state = ListState::default();
                    list_state.select(Some(selected));
                    ui.popup = Some(PopupState::TrackRating {
                        track,
                        state: list_state,
                    });
                }
                Action::EditNote => {
                    let note = data
                        .journal
                        .entry_for_track(&track)
                        .map(|entry| entry.note.clone())
                        .unwrap_or_default();
                    ui.popup = Some(PopupState::TrackNote {
                        track,
                        input: LineInput::new(note.chars().collect()),
                    });
                }
                _ => unreachable!(),
            }
            Ok(true)
        }
        Action::AddToListenLater => {
            update_track_journal(state, |journal| journal.set_listen_later(track, true))?;
            ui.popup = None;
            Ok(true)
        }
        Action::RemoveFromListenLater => {
            update_track_journal(state, |journal| journal.set_listen_later(track, false))?;
            ui.popup = None;
            Ok(true)
        }
        Action::MarkListened => {
            update_track_journal(state, |journal| journal.set_listened(track, true))?;
            ui.popup = None;
            Ok(true)
        }
        Action::MarkUnlistened => {
            update_track_journal(state, |journal| journal.set_listened(track, false))?;
            ui.popup = None;
            Ok(true)
        }
        Action::ClearNote => {
            update_track_journal(state, |journal| journal.set_note(track, String::new()))?;
            ui.popup = None;
            Ok(true)
        }
        Action::RemoveFromJournal => {
            update_track_journal(state, |journal| journal.remove_track(&track))?;
            ui.popup = None;
            Ok(true)
        }
        _ => Ok(false),
    }
}

pub(super) fn update_track_journal(
    state: &SharedState,
    update: impl FnOnce(&mut crate::state::TrackJournal),
) -> Result<()> {
    let configs = config::get_config();
    let mut data = state.data.write();
    update(&mut data.journal);
    data.journal
        .save(&configs.config_folder)
        .context("save track journal")?;
    Ok(())
}

pub(super) fn handle_bulk_add_to_listen_later(
    tracks: Vec<Track>,
    state: &SharedState,
    ui: &mut UIStateGuard,
) -> Result<bool> {
    let plan = bulk_action::plan_spotify_tracks(
        &tracks,
        bulk_action::current_spotify_epoch(ui),
        Action::AddToListenLater,
    )
    .map_err(|_| anyhow::anyhow!("bulk journal plan is unavailable"))?;
    let operation_ids = plan.operation_ids();
    let handle = bulk_action::start_bulk_local(ui, &plan, &operation_ids)?;
    if let Err(error) = update_track_journal(state, |journal| {
        for track in &tracks {
            journal.set_listen_later(track.clone(), true);
        }
    }) {
        bulk_action::complete_bulk_local(ui, &handle, &operation_ids, false)?;
        return Err(error);
    }
    bulk_action::complete_bulk_local(ui, &handle, &operation_ids, true)?;
    ui.popup = None;
    Ok(true)
}

pub(super) fn is_track_journal_action(action: Action) -> bool {
    matches!(
        action,
        Action::SetRating
            | Action::EditNote
            | Action::ClearNote
            | Action::RemoveFromJournal
            | Action::RemoveFromJournalList
            | Action::AddToListenLater
            | Action::RemoveFromListenLater
            | Action::MarkListened
            | Action::MarkUnlistened
    )
}

fn open_add_tracks_to_journal_list_popup(
    tracks: Vec<Track>,
    data: &DataReadGuard,
    ui: &mut UIStateGuard,
) {
    if data.journal.lists.is_empty() {
        ui.popup = Some(PopupState::JournalListName {
            action: JournalListNameAction::CreateWithTracks { tracks },
            input: LineInput::default(),
        });
    } else {
        ui.popup = Some(PopupState::JournalListSelect(
            JournalListPopupAction::AddTracks { tracks },
            ListState::default(),
        ));
    }
}

fn open_add_youtube_tracks_to_journal_list_popup(
    tracks: Vec<crate::state::YouTubeTrack>,
    data: &DataReadGuard,
    ui: &mut UIStateGuard,
) {
    if data.journal.lists.is_empty() {
        ui.popup = Some(PopupState::JournalListName {
            action: JournalListNameAction::CreateWithYouTubeTracks { tracks },
            input: LineInput::default(),
        });
    } else {
        ui.popup = Some(PopupState::JournalListSelect(
            JournalListPopupAction::AddYouTubeTracks { tracks },
            ListState::default(),
        ));
    }
}

fn handle_go_to_radio(
    seed_uri: &str,
    seed_name: &str,
    ui: &mut UIStateGuard,
    client_pub: &crate::client::ClientRequestSender,
) -> anyhow::Result<()> {
    let radio_id = TracksId::new(format!("radio:{seed_uri}"), format!("{seed_name} Radio"));
    ui.new_page(PageState::Context {
        id: None,
        context_page_type: ContextPageType::Browsing(ContextId::Tracks(radio_id.clone())),
        state: None,
    });
    client_pub.send(ClientRequest::GetContext(ContextId::Tracks(radio_id)))?;
    Ok(())
}

fn handle_go_to_artist(artists: Vec<Artist>, ui: &mut UIStateGuard) {
    if artists.len() == 1 {
        let context_id = ContextId::Artist(artists[0].id.clone());
        ui.new_page(PageState::Context {
            id: None,
            context_page_type: ContextPageType::Browsing(context_id),
            state: None,
        });
    } else {
        ui.popup = Some(PopupState::ArtistList(
            ArtistPopupAction::Browse,
            artists,
            ListState::default(),
        ));
    }
}

fn handle_show_actions_on_artist(
    artists: Vec<Artist>,
    data: &DataReadGuard,
    ui: &mut UIStateGuard,
) {
    if artists.len() == 1 {
        let actions = construct_artist_actions(&artists[0], data);
        ui.popup = Some(PopupState::ActionList(
            Box::new(ActionListItem::Artist(artists[0].clone(), actions)),
            ListState::default(),
        ));
    } else {
        ui.popup = Some(PopupState::ArtistList(
            ArtistPopupAction::ShowActions,
            artists,
            ListState::default(),
        ));
    }
}

/// Handle a global action, currently this is only used to target
/// the currently playing item instead of the selection.
fn handle_global_action(
    action: Action,
    target: ActionTarget,
    client_pub: &crate::client::ClientRequestSender,
    state: &SharedState,
    ui: &mut UIStateGuard,
) -> Result<bool> {
    if target == ActionTarget::PlayingTrack {
        let player = state.player.read();
        match player.playing_provider() {
            Some(config::ActiveProvider::YouTubeMusic) => {
                let playback = player
                    .youtube_playback
                    .as_ref()
                    .expect("provider target requires YouTube playback");
                return match action {
                    Action::AddToLiked | Action::DeleteFromLiked => {
                        client_pub.send(ClientRequest::RateYouTubeTrack {
                            track: playback.track.clone(),
                            liked: action == Action::AddToLiked,
                        })?;
                        Ok(true)
                    }
                    _ => Ok(false),
                };
            }
            Some(config::ActiveProvider::Spotify) => {}
            None => return Ok(false),
        }
        let data = state.data.read();

        if let Some(currently_playing) = player.currently_playing() {
            match currently_playing {
                rspotify::model::PlayableItem::Track(track) => {
                    if let Some(track) = Track::try_from_full_track(track.clone()) {
                        return handle_action_in_context(
                            action,
                            ActionContext::Track(track),
                            client_pub,
                            &data,
                            ui,
                        );
                    }
                }
                rspotify::model::PlayableItem::Episode(episode) => {
                    return handle_action_in_context(
                        action,
                        ActionContext::Episode(episode.clone().into()),
                        client_pub,
                        &data,
                        ui,
                    );
                }
                rspotify::model::PlayableItem::Unknown(_) => {
                    return Ok(false);
                }
            }
        }
    }

    Ok(false)
}

fn handle_provider_playback_command(
    command: Command,
    client_pub: &crate::client::ClientRequestSender,
    state: &SharedState,
    ui: &mut UIStateGuard,
) -> Result<bool> {
    if !is_provider_playback_command(command) {
        return Ok(false);
    }

    let configs = config::get_config();
    if command == Command::SwitchProvider {
        let target = ui.active_provider.toggled();
        if target == config::ActiveProvider::YouTubeMusic {
            ui.youtube_auth_status = configs.youtube_music_auth_status();
            if ui.youtube_auth_status.missing_message().is_some() {
                tracing::warn!("YouTube Music authentication is not ready");
                ui.set_unsupported_operation(
                    "YouTube Music is unavailable until sign-in is complete.",
                    "Authenticate YouTube Music, then try again.",
                );
                return Ok(true);
            }
        }
        // `g m` changes the browsing provider only. Explicit playback
        // transfer remains available through the client request used by setup
        // and coordinator flows, but browsing must not interrupt a song.
        ui.active_provider = target;
        if target == config::ActiveProvider::YouTubeMusic {
            client_pub.send(ClientRequest::GetYouTubeLibrary)?;
        }
        tracing::info!("Selected provider {}", target.title());
        return Ok(true);
    }

    if command == Command::SwitchPlaybackProvider {
        let active_provider = state
            .player
            .read()
            .effective_playback_provider(ui.active_provider);
        let target = active_provider.toggled();
        if target == config::ActiveProvider::YouTubeMusic {
            ui.youtube_auth_status = configs.youtube_music_auth_status();
            if ui.youtube_auth_status.missing_message().is_some() {
                tracing::warn!("YouTube Music authentication is not ready");
                ui.set_unsupported_operation(
                    "YouTube Music is unavailable until sign-in is complete.",
                    "Authenticate YouTube Music, then try the playback takeover again.",
                );
                return Ok(true);
            }
        }
        tracing::info!(
            "Requesting playback ownership transfer from {} to {}",
            active_provider.title(),
            target.title()
        );
    }

    // Provider selection is handled above. Remaining playback commands are
    // always planned for an already-selected, authenticated provider.
    let youtube_switch_allowed = true;

    let page_type = ui.current_page().page_type();
    let active_provider = state
        .player
        .read()
        .effective_playback_provider(ui.active_provider);
    let default_seek_seconds = configs.app_config.seek_duration_secs;
    let requests = if updates_local_playback(command, active_provider) {
        let mut player = state.player.write();
        let snapshot = PlaybackCommandSnapshot::capture(
            Some(&player),
            command,
            active_provider,
            page_type,
            default_seek_seconds,
            youtube_switch_allowed,
        );
        let effects = plan_provider_playback_command(command, snapshot)
            .expect("provider playback commands have a plan");
        apply_local_effects(Some(&mut player), effects)
    } else if reads_playback_state(command) {
        let effects = {
            let player = state.player.read();
            let snapshot = PlaybackCommandSnapshot::capture(
                Some(&player),
                command,
                active_provider,
                page_type,
                default_seek_seconds,
                youtube_switch_allowed,
            );
            plan_provider_playback_command(command, snapshot)
                .expect("provider playback commands have a plan")
        };
        apply_local_effects(None, effects)
    } else {
        let snapshot = PlaybackCommandSnapshot::capture(
            None,
            command,
            active_provider,
            page_type,
            default_seek_seconds,
            youtube_switch_allowed,
        );
        let effects = plan_provider_playback_command(command, snapshot)
            .expect("provider playback commands have a plan");
        apply_local_effects(None, effects)
    };

    for request in requests {
        client_pub.send(request)?;
    }

    Ok(true)
}

pub(super) fn session_history_playlist_items(state: &SharedState) -> Vec<UnifiedPlaylistItem> {
    state
        .data
        .read()
        .session_history
        .newest_first()
        .map(|entry| UnifiedPlaylistItem {
            media_id: entry.media_id.clone(),
            title: entry.title.clone(),
            artists: entry.artists.clone(),
            duration_ms: entry.duration_ms,
            provider_url: None,
            ..UnifiedPlaylistItem::default()
        })
        .collect()
}

pub(super) fn unified_playlist_item_link(item: &UnifiedPlaylistItem) -> String {
    if item
        .provider_url
        .as_deref()
        .is_some_and(|url| url.starts_with("https://") || url.starts_with("http://"))
    {
        return item.provider_url.clone().unwrap_or_default();
    }
    media_id_link(&item.media_id)
}

pub(super) fn media_id_link(media_id: &crate::state::MediaId) -> String {
    match media_id.provider {
        crate::state::Provider::Spotify => {
            let kind = match media_id.kind {
                crate::state::MediaKind::Episode => "episode",
                _ => "track",
            };
            format!("https://open.spotify.com/{kind}/{}", media_id.raw_id)
        }
        crate::state::Provider::YouTubeMusic => {
            format!("https://music.youtube.com/watch?v={}", media_id.raw_id)
        }
    }
}

/// Global commands that would take the user from first-use setup into a
/// browsing surface (library, search, collection pages and menus) or change
/// the browsing provider. Help, logs, playback controls, and quitting stay
/// available, and help and logs return to setup with Back.
const fn leaves_setup(command: Command) -> bool {
    matches!(
        command,
        Command::SettingsPage
            | Command::ShowActionsOnCurrentTrack
            | Command::CurrentlyPlayingContextPage
            | Command::BrowseUserPlaylists
            | Command::BrowseUserFollowedArtists
            | Command::BrowseUserSavedAlbums
            | Command::TopTrackPage
            | Command::RecentlyPlayedTrackPage
            | Command::LikedTrackPage
            | Command::LibraryPage
            | Command::JournalPage
            | Command::JournalListsPage
            | Command::SessionHistoryPage
            | Command::CreatePlaylistFromSessionHistory
            | Command::SearchPage
            | Command::BrowsePage
            | Command::OpenSpotifyLinkFromClipboard
            | Command::LyricsPage
            | Command::Queue
            | Command::CreatePlaylist
            | Command::LinkUnifiedPlaylistToYouTube
            | Command::SyncUnifiedPlaylistToYouTube
            | Command::UnlinkUnifiedPlaylistFromYouTube
            | Command::JumpToCurrentTrackInContext
            | Command::SwitchProvider
    )
}

/// Handle a global command that is not specific to any page/popup
fn handle_global_command(
    command: Command,
    client_pub: &crate::client::ClientRequestSender,
    state: &SharedState,
    ui: &mut UIStateGuard,
) -> Result<bool> {
    // Help and logs open on top of setup, which stays in the history until
    // it is finished or skipped.
    let setup_open = ui
        .history
        .iter()
        .any(|page| matches!(page, PageState::Welcome { .. }));
    if setup_open {
        if command == Command::ImportYouTubeAuthFromClipboard {
            ui.set_unsupported_operation(
                "Clipboard credential import is unavailable during first-use setup.",
                "Use the dedicated browser sign-in row instead.",
            );
            return Ok(true);
        }
        if leaves_setup(command) {
            ui.set_unsupported_operation(
                "This view is unavailable during first-use setup.",
                "Finish or skip setup on the Review step first.",
            );
            return Ok(true);
        }
    }

    if handle_provider_playback_command(command, client_pub, state, ui)? {
        return Ok(true);
    }

    match command {
        Command::Quit => {
            state.request_shutdown()?;
            ui.is_running = false;
        }
        Command::OpenCommandHelp => {
            ui.reset_command_help_view();
            ui.popup = Some(PopupState::CommandHelp { scroll_offset: 0 });
        }
        Command::OpenLogs => {
            ui.new_page(PageState::Logs {
                state: crate::state::DiagnosticsPageUIState::new(),
            });
        }
        Command::SettingsPage => {
            let mut list = ListState::default();
            list.select(Some(0));
            let settings = config::app_config_settings(&config::get_config().config_folder)?;
            ui.new_page(PageState::Settings {
                list,
                shelves: crate::state::SettingsShelves::default(),
                settings,
                saved: false,
                error: None,
                notice: None,
            });
        }
        Command::ShowActionsOnCurrentTrack => {
            if ui.active_provider == config::ActiveProvider::YouTubeMusic {
                if let Some(track) = state
                    .player
                    .read()
                    .youtube_playback
                    .as_ref()
                    .map(|playback| playback.track.clone())
                {
                    ui.popup = Some(PopupState::ActionList(
                        Box::new(ActionListItem::YouTubeTrack(
                            track,
                            command::construct_youtube_track_actions(),
                        )),
                        ListState::default(),
                    ));
                }
            } else if let Some(currently_playing) = state.player.read().currently_playing() {
                match currently_playing {
                    rspotify::model::PlayableItem::Track(track) => {
                        if let Some(track) = Track::try_from_full_track(track.clone()) {
                            let data = state.data.read();
                            let actions = command::construct_track_actions(&track, &data);
                            ui.popup = Some(PopupState::ActionList(
                                Box::new(ActionListItem::Track(track, actions)),
                                ListState::default(),
                            ));
                        }
                    }
                    rspotify::model::PlayableItem::Episode(episode) => {
                        let episode = episode.clone().into();
                        let data = state.data.read();
                        let actions = command::construct_episode_actions(&episode, &data);
                        ui.popup = Some(PopupState::ActionList(
                            Box::new(ActionListItem::Episode(episode, actions)),
                            ListState::default(),
                        ));
                    }
                    rspotify::model::PlayableItem::Unknown(_) => {}
                }
            }
        }
        Command::CurrentlyPlayingContextPage => {
            if ui.active_provider == config::ActiveProvider::YouTubeMusic {
                let player = state.player.read();
                let tracks = player
                    .unified_queue
                    .as_ref()
                    .map(|queue| {
                        queue
                            .display_items()
                            .into_iter()
                            .filter_map(|item| match &item.media {
                                crate::state::PlayableMedia::YouTube(track) => Some(track.clone()),
                                crate::state::PlayableMedia::Spotify(_) => None,
                            })
                            .collect()
                    })
                    .or_else(|| {
                        player
                            .youtube_playback
                            .as_ref()
                            .map(|playback| vec![playback.track.clone()])
                    })
                    .unwrap_or_default();
                ui.new_page(PageState::YouTubeContext {
                    id: YouTubeContextId::Playlist("local:current-queue".to_string()),
                    context: Some(crate::state::YouTubeContext {
                        title: "Current Queue".to_string(),
                        description: None,
                        tracks,
                        playlist_set_video_ids: Vec::new(),
                        artist: None,
                    }),
                    state: YouTubeContextPageUIState::new(),
                });
            } else {
                ui.new_page(PageState::Context {
                    id: None,
                    context_page_type: ContextPageType::CurrentPlaying,
                    state: None,
                });
            }
        }
        Command::BrowseUserPlaylists => {
            client_pub.send(ClientRequest::GetUserPlaylists)?;
            ui.popup = Some(PopupState::UserPlaylistList(
                PlaylistPopupAction::Browse {
                    folder_id: 0,
                    search_query: String::new(),
                },
                ListState::default(),
            ));
        }
        Command::BrowseUserFollowedArtists => {
            client_pub.send(ClientRequest::GetUserFollowedArtists)?;
            ui.popup = Some(PopupState::UserFollowedArtistList(ListState::default()));
        }
        Command::BrowseUserSavedAlbums => {
            client_pub.send(ClientRequest::GetUserSavedAlbums)?;
            ui.popup = Some(PopupState::UserSavedAlbumList(ListState::default()));
        }
        Command::TopTrackPage => {
            ui.new_page(PageState::Context {
                id: None,
                context_page_type: ContextPageType::Browsing(ContextId::Tracks(
                    USER_TOP_TRACKS_ID.to_owned(),
                )),
                state: None,
            });
            client_pub.send(ClientRequest::GetContext(ContextId::Tracks(
                USER_TOP_TRACKS_ID.to_owned(),
            )))?;
        }
        Command::RecentlyPlayedTrackPage => {
            ui.new_page(PageState::Context {
                id: None,
                context_page_type: ContextPageType::Browsing(ContextId::Tracks(
                    USER_RECENTLY_PLAYED_TRACKS_ID.to_owned(),
                )),
                state: None,
            });
            client_pub.send(ClientRequest::GetContext(ContextId::Tracks(
                USER_RECENTLY_PLAYED_TRACKS_ID.to_owned(),
            )))?;
        }
        Command::LikedTrackPage => {
            if ui.active_provider == config::ActiveProvider::YouTubeMusic {
                let context_id = YouTubeContextId::LikedTracks;
                ui.new_page(PageState::YouTubeContext {
                    id: context_id.clone(),
                    context: None,
                    state: YouTubeContextPageUIState::new(),
                });
                client_pub.send(ClientRequest::GetYouTubeContext(context_id))?;
            } else {
                ui.new_page(PageState::Context {
                    id: None,
                    context_page_type: ContextPageType::Browsing(ContextId::Tracks(
                        USER_LIKED_TRACKS_ID.to_owned(),
                    )),
                    state: None,
                });
                client_pub.send(ClientRequest::GetContext(ContextId::Tracks(
                    USER_LIKED_TRACKS_ID.to_owned(),
                )))?;
            }
        }
        Command::LibraryPage => {
            ui.new_page(PageState::Library {
                state: LibraryPageUIState::new(),
            });
            if ui.active_provider == config::ActiveProvider::YouTubeMusic {
                ui.youtube_auth_status = config::get_config().youtube_music_auth_status();
                if ui.youtube_auth_status.missing_message().is_some() {
                    tracing::warn!("YouTube Music authentication is not ready");
                    ui.set_unsupported_operation(
                        "The YouTube Music library is unavailable until sign-in is complete.",
                        "Authenticate YouTube Music, then open the library again.",
                    );
                } else {
                    client_pub.send(ClientRequest::GetYouTubeLibrary)?;
                }
            }
        }
        Command::JournalPage => {
            ui.new_page(PageState::Journal {
                table: ratatui::widgets::TableState::default(),
                journal_selection: crate::state::JournalSelection::default(),
            });
        }
        Command::JournalListsPage => {
            ui.new_page(PageState::JournalLists {
                list: ListState::default(),
            });
        }
        Command::SessionHistoryPage => {
            ui.session_history_selection.clear();
            ui.new_page(PageState::SessionHistory {
                list: ListState::default(),
            });
        }
        Command::CreatePlaylistFromSessionHistory => {
            let items = session_history_playlist_items(state);
            if items.is_empty() {
                ui.set_unsupported_operation(
                    "Session history is empty.",
                    "Play an item, then try creating a playlist from history again.",
                );
            } else {
                ui.popup = Some(PopupState::SessionHistoryCreate {
                    items,
                    input: LineInput::default(),
                });
            }
        }
        Command::SearchPage => {
            ui.new_page(PageState::Search {
                line_input: LineInput::default(),
                current_query: String::new(),
                state: SearchPageUIState::new(),
            });
        }
        Command::BrowsePage => {
            ui.new_page(PageState::Browse {
                state: BrowsePageUIState::CategoryList {
                    state: ListState::default(),
                },
            });
            client_pub.send(ClientRequest::GetBrowseCategories)?;
        }
        Command::PreviousPage => {
            if ui.history.len() > 1 {
                ui.history.pop();
                ui.popup = None;
                ui.sync_workspace_after_history_change();
            }
        }
        Command::OpenSpotifyLinkFromClipboard => {
            let content = get_clipboard_content().context("get clipboard's content")?;
            let re = regex::Regex::new(
                r"https://open.spotify.com/(?P<type>.*?)/(?P<id>[[:alnum:]]*).*",
            )?;
            if let Some(cap) = re.captures(&content) {
                let typ = cap.name("type").expect("valid capture").as_str();
                let id = cap.name("id").expect("valid capture").as_str();
                match typ {
                    // for track link, play the song
                    "track" => {
                        let id = TrackId::from_id(id)?.into_static();

                        // Clear Tracks context when playing from clipboard link
                        state.player.write().currently_playing_tracks_id = None;

                        client_pub.send(ClientRequest::Player(PlayerRequest::StartPlayback(
                            Playback::URIs(vec![id.into()], None),
                            None,
                        )))?;
                    }
                    // for playlist/artist/album link, go to the corresponding context page
                    "playlist" => {
                        let id = PlaylistId::from_id(id)?.into_static();
                        ui.new_page(PageState::Context {
                            id: None,
                            context_page_type: ContextPageType::Browsing(ContextId::Playlist(id)),
                            state: None,
                        });
                    }
                    "artist" => {
                        let id = ArtistId::from_id(id)?.into_static();
                        ui.new_page(PageState::Context {
                            id: None,
                            context_page_type: ContextPageType::Browsing(ContextId::Artist(id)),
                            state: None,
                        });
                    }
                    "album" => {
                        let id = AlbumId::from_id(id)?.into_static();
                        ui.new_page(PageState::Context {
                            id: None,
                            context_page_type: ContextPageType::Browsing(ContextId::Album(id)),
                            state: None,
                        });
                    }
                    e => anyhow::bail!("unsupported Spotify type {e}!"),
                }
            } else {
                tracing::warn!("Clipboard content is not a valid Spotify link");
            }
        }
        Command::LyricsPage => {
            if ui.active_provider == config::ActiveProvider::YouTubeMusic {
                let track = state
                    .player
                    .read()
                    .youtube_playback
                    .as_ref()
                    .map(|p| p.track.clone());
                if let Some(track) = track {
                    ui.new_page(PageState::Lyrics {
                        provider: config::ActiveProvider::YouTubeMusic,
                        track_uri: format!("youtube:{}", track.id),
                        track: track.name.clone(),
                        artists: track.artists.clone(),
                        youtube_track: Some(track.clone()),
                        lyrics_provider: None,
                        scroll_offset: 0,
                        follow_playback: true,
                        status: crate::state::UiViewStatus::Loading,
                    });
                    client_pub.send(ClientRequest::GetYouTubeLyrics(track))?;
                }
            } else if let Some(rspotify::model::PlayableItem::Track(track)) =
                state.player.read().currently_playing()
            {
                if let Some(id) = &track.id {
                    let artists = map_join(&track.artists, |a| &a.name, ", ");
                    ui.new_page(PageState::Lyrics {
                        provider: config::ActiveProvider::Spotify,
                        track_uri: id.uri(),
                        track: track.name.clone(),
                        artists,
                        youtube_track: None,
                        lyrics_provider: None,
                        scroll_offset: 0,
                        follow_playback: true,
                        status: crate::state::UiViewStatus::Loading,
                    });

                    client_pub.send(ClientRequest::GetLyrics {
                        track_id: id.clone_static(),
                    })?;
                }
            }
        }
        Command::SwitchDevice => {
            let playback_provider = state
                .player
                .read()
                .effective_playback_provider(ui.active_provider);
            if playback_provider == config::ActiveProvider::YouTubeMusic {
                tracing::debug!("Spotify device switching is unavailable in YouTube Music mode");
                ui.set_unsupported_operation(
                    "Device switching is unavailable in YouTube Music mode.",
                    "Switch to Spotify to choose a device.",
                );
            } else {
                ui.popup = Some(PopupState::DeviceList(ListState::default()));
                client_pub.send(ClientRequest::GetDevices)?;
            }
        }
        Command::ImportYouTubeAuthFromClipboard => {
            let settings = config::app_config_settings(&config::get_config().config_folder)?;
            let selected = settings
                .iter()
                .position(|setting| setting.key == "youtube.auth.import")
                .unwrap_or_default();
            let mut list = ListState::default();
            list.select(Some(selected));
            ui.new_page(PageState::Settings {
                list,
                shelves: crate::state::SettingsShelves::default(),
                settings,
                saved: false,
                error: None,
                notice: Some(
                    "YouTube credential import is selected; press Enter to run it".to_string(),
                ),
            });
        }
        Command::SwitchTheme => ui.open_theme_picker(),
        #[cfg(feature = "streaming")]
        Command::RestartIntegratedClient => {
            client_pub.send(ClientRequest::RestartIntegratedClient)?;
        }
        Command::FocusNextWindow => {
            let workspace_focus_changed = ui.focus_workspace(true);
            if !workspace_focus_changed && !ui.has_focused_popup() {
                let provider = match ui.current_page() {
                    PageState::Search { state, .. } => {
                        Some(state.provider.unwrap_or(ui.active_provider))
                    }
                    _ => None,
                };
                if let Some(provider) = provider {
                    ui.clear_search_lucky();
                    if let PageState::Search { state, .. } = ui.current_page_mut() {
                        state.focus.next_for_provider(provider);
                        ui.current_page_mut().select(0);
                        window::clear_track_selection(ui);
                    }
                } else if matches!(ui.current_page(), PageState::Settings { .. }) {
                    ui.current_page_mut().focus_settings_section(true);
                } else {
                    ui.current_page_mut().next();
                }
            }
        }
        Command::FocusPreviousWindow => {
            let workspace_focus_changed = ui.focus_workspace(false);
            if !workspace_focus_changed && !ui.has_focused_popup() {
                let provider = match ui.current_page() {
                    PageState::Search { state, .. } => {
                        Some(state.provider.unwrap_or(ui.active_provider))
                    }
                    _ => None,
                };
                if let Some(provider) = provider {
                    ui.clear_search_lucky();
                    if let PageState::Search { state, .. } = ui.current_page_mut() {
                        state.focus.previous_for_provider(provider);
                        ui.current_page_mut().select(0);
                        window::clear_track_selection(ui);
                    }
                } else if matches!(ui.current_page(), PageState::Settings { .. }) {
                    ui.current_page_mut().focus_settings_section(false);
                } else {
                    ui.current_page_mut().previous();
                }
            }
        }
        Command::Queue => {
            ui.new_page(PageState::new_queue());
            let refresh_guard = state.player.read().native_queue_refresh_guard();
            if let Some(refresh_guard) = refresh_guard {
                client_pub.send(ClientRequest::GetCurrentUserQueue(refresh_guard))?;
            }
        }
        Command::CreatePlaylist => {
            let target = match ui.active_provider {
                config::ActiveProvider::Spotify => PlaylistCreateTarget::Spotify,
                config::ActiveProvider::YouTubeMusic => PlaylistCreateTarget::YouTubeMusic,
            };
            ui.popup = Some(PopupState::PlaylistCreate {
                target,
                public: false,
                name: LineInput::default(),
                desc: LineInput::default(),
                // Keep the legacy flow intact: typing immediately after N edits the name.
                // Shift-Tab reaches the destination selector when it is needed.
                current_field: PlaylistCreateCurrentField::Name,
                pending_items: None,
                source_provider: None,
                source_epoch: None,
            });
        }
        Command::LinkUnifiedPlaylistToYouTube => {
            let Some(unified_playlist_id) = (match ui.current_page() {
                PageState::UnifiedPlaylist { id, .. } => Some(id.clone()),
                _ => None,
            }) else {
                ui.set_unsupported_operation(
                    "Linking is available from a Unified playlist page.",
                    "Open a Unified playlist, then press g y.",
                );
                return Ok(true);
            };
            ui.youtube_auth_status = config::get_config().youtube_music_auth_status();
            if ui.youtube_auth_status.missing_message().is_some() {
                ui.set_unsupported_operation(
                    "YouTube Music is not ready for playlist linking.",
                    "Authenticate YouTube Music, then try again.",
                );
            } else {
                client_pub.send(ClientRequest::GetYouTubeLibrary)?;
                ui.popup = Some(PopupState::YouTubePlaylistList(
                    YouTubePlaylistPopupAction::LinkUnified {
                        unified_playlist_id,
                        search_query: String::new(),
                    },
                    ListState::default(),
                ));
            }
        }
        Command::SyncUnifiedPlaylistToYouTube => {
            let Some(unified_playlist_id) = (match ui.current_page() {
                PageState::UnifiedPlaylist { id, .. } => Some(id.clone()),
                _ => None,
            }) else {
                ui.set_unsupported_operation(
                    "Syncing is available from a Unified playlist page.",
                    "Open a Unified playlist, then press g P.",
                );
                return Ok(true);
            };
            let (account_id, account_epoch) = current_youtube_projection_scope(ui);
            let (projection_status, projection_preview) = {
                let data = state.data.read();
                (
                    data.unified_playlist_projection_status(
                        &unified_playlist_id,
                        &account_id,
                        account_epoch,
                    ),
                    data.unified_playlist_projection_preview(
                        &unified_playlist_id,
                        &account_id,
                        account_epoch,
                    ),
                )
            };
            match projection_status {
                None => ui.set_unsupported_operation(
                    "This Unified playlist has no YouTube link.",
                    "Press g p to choose a YouTube playlist target.",
                ),
                Some(crate::state::PlaylistProjectionStatus::OutcomeUnknown) => ui
                    .set_unsupported_operation(
                    "The remote projection outcome is unknown.",
                    "Refresh the linked target or relink it; retry remains blocked until verified.",
                ),
                Some(status) if status.needs_preview_confirmation() => {
                    ui.popup = Some(PopupState::ConfirmAction {
                        message: format!(
                            "Previewed {status:?} projection ({}); reconcile Unified playlist?",
                            projection_preview
                                .as_deref()
                                .unwrap_or("conflict details unavailable")
                        ),
                        action: ConfirmableAction::ReconcileUnifiedPlaylistProjection(
                            unified_playlist_id,
                        ),
                    });
                }
                Some(_) => {
                    dispatch_legacy_playlist(
                        client_pub,
                        ClientRequest::SyncUnifiedPlaylistToYouTube {
                            unified_playlist_id,
                        },
                    )?;
                }
            }
        }
        Command::UnlinkUnifiedPlaylistFromYouTube => {
            let Some(unified_playlist_id) = (match ui.current_page() {
                PageState::UnifiedPlaylist { id, .. } => Some(id.clone()),
                _ => None,
            }) else {
                ui.set_unsupported_operation(
                    "Unlinking is available from a Unified playlist page.",
                    "Open a Unified playlist, then press g u.",
                );
                return Ok(true);
            };
            let linked = state.data.read().playlist_links.iter().any(|link| {
                link.unified_playlist_id == unified_playlist_id
                    && link.youtube_playlist_id.is_some()
            });
            if linked {
                dispatch_legacy_playlist(
                    client_pub,
                    ClientRequest::UnlinkUnifiedPlaylistFromYouTube {
                        unified_playlist_id,
                    },
                )?;
            } else {
                ui.set_unsupported_operation(
                    "This Unified playlist has no YouTube link.",
                    "Press g p to choose a YouTube playlist target.",
                );
            }
        }
        Command::JumpToCurrentTrackInContext => {
            let track_id = match state.player.read().currently_playing() {
                Some(rspotify::model::PlayableItem::Track(track)) => {
                    PlayableId::Track(track.id.clone().expect("all non-local tracks have ids"))
                }
                Some(rspotify::model::PlayableItem::Episode(episode)) => {
                    PlayableId::Episode(episode.id.clone())
                }
                Some(rspotify::model::PlayableItem::Unknown(_)) | None => return Ok(false),
            };

            if let PageState::Context {
                id: Some(context_id),
                ..
            } = ui.current_page()
            {
                let context_track_pos = state
                    .data
                    .read()
                    .context_tracks(context_id)
                    .and_then(|tracks| tracks.iter().position(|t| t.id.uri() == track_id.uri()));

                if let Some(p) = context_track_pos {
                    ui.current_page_mut().select(p);
                }
            }
        }
        Command::ClosePopup => {
            if ui.popup.is_none() {
                let playlist_id = match ui.current_page() {
                    PageState::UnifiedPlaylist { id, .. } => Some(id.clone()),
                    _ => None,
                };
                if let Some(playlist_id) = playlist_id {
                    ui.cancel_listenbrainz_sync_check(&playlist_id);
                }
            }
            ui.popup = None;
        }
        _ => return Ok(false),
    }
    Ok(true)
}

#[cfg(test)]
mod selection_command_tests {
    use super::{handle_selection_command, Command};
    use crate::config::ActiveProvider;
    use crate::state::{
        synchronize_context_track_uris, synchronize_journal_uris, synchronize_native_spotify_queue,
        synchronize_unified_playlist_entries, synchronize_unified_queue_items,
        synchronize_youtube_context_tracks, ContextTrackPane, ContextTrackSelection,
        JournalSelection, JournalSelectionScope, MediaId, MediaKind, NativeQueueRow,
        PlaylistEntryId, Provider, QueueSelection, SearchPane, SearchScope, SearchSelection,
        UnifiedPlaylistSelection, UnifiedQueue, YouTubeContextId, YouTubeContextSelection,
        YouTubeTrack,
    };

    fn youtube(id: &str) -> YouTubeTrack {
        YouTubeTrack {
            id: id.to_owned(),
            name: id.to_owned(),
            artists: "artist".to_owned(),
            album: None,
            duration: "0:01".to_owned(),
            explicit: false,
            thumbnail_url: None,
            is_video: false,
        }
    }

    fn media(provider: Provider, raw_id: &str) -> MediaId {
        MediaId {
            provider,
            kind: MediaKind::Track,
            raw_id: raw_id.to_owned(),
        }
    }

    macro_rules! apply_visible_operations {
        ($selection:expr) => {{
            assert!(handle_selection_command(
                Command::SelectAll,
                &mut $selection
            ));
            assert!(handle_selection_command(
                Command::InvertSelection,
                &mut $selection
            ));
        }};
    }

    #[test]
    fn shared_selection_helper_routes_all_keyed_adapter_families() {
        let mut search = SearchSelection::default();
        search
            .synchronize(
                SearchScope::new(ActiveProvider::Spotify, 0, "q", SearchPane::SpotifyTracks),
                ["a", "b"],
            )
            .unwrap();
        apply_visible_operations!(search);

        let mut context = ContextTrackSelection::default();
        synchronize_context_track_uris(
            &mut context,
            0,
            "spotify:album:one",
            ContextTrackPane::Album,
            None,
            ["spotify:track:a", "spotify:track:b"],
            ["spotify:track:a", "spotify:track:b"],
        )
        .unwrap();
        apply_visible_operations!(context);

        let mut youtube_context = YouTubeContextSelection::default();
        let youtube_tracks = vec![youtube("a"), youtube("b")];
        synchronize_youtube_context_tracks(
            &mut youtube_context,
            0,
            &YouTubeContextId::Playlist("ctx".to_owned()),
            &youtube_tracks,
        )
        .unwrap();
        apply_visible_operations!(youtube_context);

        let mut journal = JournalSelection::default();
        synchronize_journal_uris(
            &mut journal,
            JournalSelectionScope::journal(0),
            None,
            ["spotify:track:a", "spotify:track:b"],
            ["spotify:track:a", "spotify:track:b"],
        )
        .unwrap();
        apply_visible_operations!(journal);

        let mut journal_list = JournalSelection::default();
        synchronize_journal_uris(
            &mut journal_list,
            JournalSelectionScope::journal_list(0, "list"),
            None,
            ["spotify:track:a", "spotify:track:b"],
            ["spotify:track:a", "spotify:track:b"],
        )
        .unwrap();
        apply_visible_operations!(journal_list);

        let mut queue = QueueSelection::default();
        let instance_id = UnifiedQueue::empty().instance_id();
        synchronize_unified_queue_items(
            &mut queue,
            instance_id,
            [
                (media(Provider::Spotify, "a"), 1),
                (media(Provider::Spotify, "b"), 2),
            ],
        )
        .unwrap();
        apply_visible_operations!(queue);

        let mut native_queue = QueueSelection::default();
        synchronize_native_spotify_queue(
            &mut native_queue,
            0,
            [NativeQueueRow::Media(media(Provider::Spotify, "a"))],
        )
        .unwrap();
        apply_visible_operations!(native_queue);

        let mut playlist = UnifiedPlaylistSelection::default();
        let entries = [
            (media(Provider::Spotify, "a"), PlaylistEntryId(1)),
            (media(Provider::YouTubeMusic, "b"), PlaylistEntryId(2)),
        ];
        synchronize_unified_playlist_entries(&mut playlist, "playlist", entries.clone(), entries)
            .unwrap();
        apply_visible_operations!(playlist);
    }

    #[test]
    fn shared_selection_helper_refuses_non_selection_and_adapter_errors() {
        let mut selection = SearchSelection::default();
        assert!(!handle_selection_command(
            Command::ChooseSelected,
            &mut selection
        ));
        selection
            .synchronize(
                SearchScope::new(ActiveProvider::Spotify, 0, "q", SearchPane::SpotifyTracks),
                ["same", "same"],
            )
            .unwrap();
        assert!(!handle_selection_command(
            Command::SelectAll,
            &mut selection
        ));
        assert!(selection.selected_indices().is_empty());
    }
}

#[cfg(test)]
mod provider_action_tests {
    use super::{media_id_link, unified_playlist_item_link, youtube_track_action_is_allowed};
    use crate::command::Action;
    use crate::state::{MediaId, MediaKind, Provider, UnifiedPlaylistItem};

    #[test]
    fn unified_playlist_links_are_provider_specific_and_prefer_http_sources() {
        let spotify = UnifiedPlaylistItem {
            media_id: MediaId {
                provider: Provider::Spotify,
                kind: MediaKind::Track,
                raw_id: "sp-track".to_owned(),
            },
            title: "Track".to_owned(),
            artists: "Artist".to_owned(),
            duration_ms: None,
            provider_url: Some("spotify:track:sp-track".to_owned()),
            ..UnifiedPlaylistItem::default()
        };
        assert_eq!(
            unified_playlist_item_link(&spotify),
            "https://open.spotify.com/track/sp-track"
        );

        let youtube = UnifiedPlaylistItem {
            media_id: MediaId {
                provider: Provider::YouTubeMusic,
                kind: MediaKind::Video,
                raw_id: "yt-video".to_owned(),
            },
            title: "Video".to_owned(),
            artists: "Artist".to_owned(),
            duration_ms: None,
            provider_url: Some("https://music.youtube.com/watch?v=canonical".to_owned()),
            ..UnifiedPlaylistItem::default()
        };
        assert_eq!(
            unified_playlist_item_link(&youtube),
            "https://music.youtube.com/watch?v=canonical"
        );
        assert_eq!(
            media_id_link(&MediaId {
                provider: Provider::Spotify,
                kind: MediaKind::Episode,
                raw_id: "episode".to_owned(),
            }),
            "https://open.spotify.com/episode/episode"
        );
    }

    #[test]
    fn youtube_exact_delete_bypasses_the_global_track_gate_only_on_playlist_rows() {
        assert!(youtube_track_action_is_allowed(Action::CopyLink, false));
        assert!(!youtube_track_action_is_allowed(
            Action::DeleteFromPlaylist,
            false
        ));
        assert!(youtube_track_action_is_allowed(
            Action::DeleteFromPlaylist,
            true
        ));
    }
}

#[cfg(test)]
mod listenbrainz_picker_mouse_tests {
    use super::*;
    #[test]
    fn listenbrainz_picker_mouse_and_keyboard_share_selection_and_cancel() {
        crate::ui::initialize_test_config();
        let ring = std::sync::Arc::new(parking_lot::Mutex::new(std::collections::VecDeque::new()));
        let (diagnostics, _runtime) = crate::observability::disabled(ring);
        let state = std::sync::Arc::new(crate::state::State::new(false, diagnostics));
        let (sender, receiver) = crate::client::client_request_channel();
        let rect = Rect::new(10, 10, 20, 1);
        let identity = crate::client::listenbrainz::ValidatedListenBrainzIdentity {
            username: "owner".to_owned(),
            token: crate::client::listenbrainz::ListenBrainzToken::new("test".to_owned()),
        };
        {
            let mut ui = state.ui.lock();
            let mut welcome = crate::state::WelcomePageUIState::new();
            welcome.show_step(crate::state::WelcomeStep::ListenBrainz);
            ui.history = vec![PageState::Welcome {
                state: welcome,
                from_settings: false,
            }];
            ui.popup = Some(PopupState::ListenBrainzPlaylists {
                operation: 1,
                identity,
                rows: Vec::new(),
                state: ratatui::widgets::ListState::default().with_selected(Some(0)),
                busy: true,
                notice: "Loading".to_owned(),
            });
            ui.workspace_popup_hits = vec![(rect, 1)];
        }
        handle_mouse_event(
            crossterm::event::MouseEvent {
                kind: crossterm::event::MouseEventKind::Down(crossterm::event::MouseButton::Left),
                column: 10,
                row: 10,
                modifiers: crossterm::event::KeyModifiers::NONE,
            },
            &sender,
            &state,
        )
        .unwrap();
        assert!(state.ui.lock().popup.is_none());
        assert!(receiver.try_recv().is_err());
        // Navigation invalidates old geometry before a second click can use it.
        {
            let mut ui = state.ui.lock();
            ui.popup = Some(PopupState::ListenBrainzPlaylists {
                operation: 2,
                identity: crate::client::listenbrainz::ValidatedListenBrainzIdentity {
                    username: "owner".to_owned(),
                    token: crate::client::listenbrainz::ListenBrainzToken::new("test".to_owned()),
                },
                rows: Vec::new(),
                state: Default::default(),
                busy: false,
                notice: String::new(),
            });
            ui.workspace_popup_hits = vec![(rect, 1)];
            popup::handle_listenbrainz_playlist_command(
                Command::SelectNextOrScrollDown,
                &sender,
                &mut ui,
            )
            .unwrap();
            assert_eq!(ui.popup.as_ref().unwrap().list_selected(), Some(1));
            assert!(ui.workspace_popup_hits.is_empty());
            popup::handle_listenbrainz_playlist_command(Command::ChooseSelected, &sender, &mut ui)
                .unwrap();
            assert!(ui.popup.is_none());
        }
    }
}
