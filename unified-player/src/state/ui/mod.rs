use crate::{
    config::{self, Theme},
    key,
    state::ui::page::{SettingsCategory, SettingsWorkspaceAction},
    state::{ContextId, YouTubeContextId},
    ui::{self, Orientation},
    utils::{filtered_item_count_from_query, filtered_items_from_query},
};
use ratatui::{
    layout::Rect,
    widgets::{ListState, TableState},
};
use std::time::Instant;

#[cfg(feature = "image")]
use crate::ui::cover_image::CoverImage;
#[cfg(feature = "image")]
use ratatui_image::picker::Picker;

pub type UIStateGuard<'a> = super::TrackedMutexGuard<'a, UIState>;

/// A search projection that keeps the common unfiltered path borrowed.
///
/// Filtered views still own the small borrowed-item vector produced by the
/// matcher, but opening no search popup does not allocate a new `Vec` just to
/// visit every source item.
pub(crate) struct SearchFilteredItems<'a, T> {
    source: SearchFilteredItemsSource<'a, T>,
}

enum SearchFilteredItemsSource<'a, T> {
    All(&'a [T]),
    Filtered(Vec<&'a T>),
}

pub(crate) struct SearchFilteredItemsIter<'projection, 'items, T> {
    source: SearchFilteredItemsIterSource<'projection, 'items, T>,
}

enum SearchFilteredItemsIterSource<'projection, 'items, T> {
    All(std::slice::Iter<'items, T>),
    Filtered(std::slice::Iter<'projection, &'items T>),
}

impl<'a, T> SearchFilteredItems<'a, T> {
    pub(crate) fn len(&self) -> usize {
        match &self.source {
            SearchFilteredItemsSource::All(items) => items.len(),
            SearchFilteredItemsSource::Filtered(items) => items.len(),
        }
    }

    pub(crate) fn is_empty(&self) -> bool {
        self.len() == 0
    }

    pub(crate) fn iter(&self) -> SearchFilteredItemsIter<'_, 'a, T> {
        let source = match &self.source {
            SearchFilteredItemsSource::All(items) => {
                SearchFilteredItemsIterSource::All(items.iter())
            }
            SearchFilteredItemsSource::Filtered(items) => {
                SearchFilteredItemsIterSource::Filtered(items.iter())
            }
        };
        SearchFilteredItemsIter { source }
    }
}

impl<'items, T> Iterator for SearchFilteredItemsIter<'_, 'items, T> {
    type Item = &'items T;

    fn next(&mut self) -> Option<Self::Item> {
        match &mut self.source {
            SearchFilteredItemsIterSource::All(items) => items.next(),
            SearchFilteredItemsIterSource::Filtered(items) => items.next().copied(),
        }
    }

    fn size_hint(&self) -> (usize, Option<usize>) {
        let len = match &self.source {
            SearchFilteredItemsIterSource::All(items) => items.len(),
            SearchFilteredItemsIterSource::Filtered(items) => items.len(),
        };
        (len, Some(len))
    }
}

impl<T> ExactSizeIterator for SearchFilteredItemsIter<'_, '_, T> {}

const MARQUEE_FRAME_MILLIS: u128 = 120;

#[derive(Debug)]
struct FocusedMarqueeState {
    key: String,
    epoch: Instant,
    phase: usize,
    /// Characters scrolled with Left/Right in `Manual` mode.
    manual_offset: usize,
    /// How far the focused row could scroll when it was last drawn.
    manual_extent: usize,
}

impl Default for FocusedMarqueeState {
    fn default() -> Self {
        Self {
            key: String::new(),
            epoch: Instant::now(),
            phase: 0,
            manual_offset: 0,
            manual_extent: 0,
        }
    }
}

impl FocusedMarqueeState {
    fn refresh(&mut self, key: &str, now: Instant, manual: bool) {
        if self.key != key {
            self.key.clear();
            self.key.push_str(key);
            self.epoch = now;
            self.manual_offset = 0;
        }
        self.phase = if manual {
            self.manual_offset
        } else {
            (now.saturating_duration_since(self.epoch).as_millis() / MARQUEE_FRAME_MILLIS) as usize
        };
    }

    /// Scroll one character, bounded by the last drawn extent. Returns false
    /// when nothing in the focused row overflows.
    fn scroll_manually(&mut self, forward: bool) -> bool {
        if self.manual_extent == 0 {
            return false;
        }
        let offset = self.manual_offset.min(self.manual_extent);
        self.manual_offset = if forward {
            (offset + 1).min(self.manual_extent)
        } else {
            offset.saturating_sub(1)
        };
        true
    }

    fn until_next_phase(&self, now: Instant) -> std::time::Duration {
        let elapsed = now.saturating_duration_since(self.epoch).as_millis();
        let remaining = MARQUEE_FRAME_MILLIS - elapsed % MARQUEE_FRAME_MILLIS;
        std::time::Duration::from_millis(remaining as u64)
    }
}

/// Keep navigation useful without allowing repeated page opens to grow state
/// without bound during a long-running session.
pub(crate) const MAX_PAGE_HISTORY: usize = 64;

#[allow(dead_code)]
mod context_selection;
#[allow(dead_code)]
mod journal_selection;
mod listenbrainz_sync;
mod multi_selection;
mod mutable_playlist;
mod operation;
pub(crate) mod page;
mod popup;
#[allow(dead_code)]
mod queue_selection;
#[allow(dead_code)]
mod scoped_selection;
mod search_selection;
#[allow(dead_code)]
mod unified_playlist_selection;
#[allow(dead_code)]
mod youtube_context_selection;
// The generic kernel intentionally exposes select-all/invert and projection
// helpers reserved for the later serial page migration packets.
#[allow(dead_code)]
mod selection;
mod shelf_nav;

#[allow(unused_imports)]
pub use context_selection::*;
pub use journal_selection::*;
pub use listenbrainz_sync::*;
pub use multi_selection::*;
pub use mutable_playlist::*;
pub use operation::*;
pub use page::*;
pub use popup::*;
#[allow(unused_imports)]
pub use queue_selection::*;
#[allow(unused_imports)]
pub use scoped_selection::*;
pub use search_selection::*;
pub use selection::*;
pub use shelf_nav::*;
#[allow(unused_imports)]
pub use unified_playlist_selection::*;
#[allow(unused_imports)]
pub use youtube_context_selection::*;

#[cfg(feature = "image")]
#[derive(Default)]
pub struct ImageRenderInfo {
    pub url: String,
    pub render_area: ratatui::layout::Rect,
    pub state: Option<CoverImage>,
    /// A cache miss is transient while the playback worker retrieves and decodes the image.
    /// Keeping that state separate from an encode failure lets the renderer retry exactly once
    /// when the same URL arrives in the cache without retrying deterministic failures every frame.
    pub awaiting_cache: bool,
}

#[cfg(feature = "image")]
impl std::fmt::Debug for ImageRenderInfo {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ImageRenderInfo")
            .field("url", &self.url)
            .field("render_area", &self.render_area)
            .field("state", &self.state.is_some())
            .field("awaiting_cache", &self.awaiting_cache)
            .finish()
    }
}

#[cfg(feature = "image")]
impl ImageRenderInfo {
    pub(crate) fn pending(url: &str, render_area: ratatui::layout::Rect) -> Self {
        Self {
            url: url.to_owned(),
            render_area,
            state: None,
            awaiting_cache: true,
        }
    }

    pub(crate) fn targets(&self, url: &str, render_area: ratatui::layout::Rect) -> bool {
        self.url == url && self.render_area == render_area
    }

    pub(crate) fn needs_prepare(&self, url: &str, render_area: ratatui::layout::Rect) -> bool {
        !self.targets(url, render_area) || self.awaiting_cache
    }
}

/// Notice shown when a Welcome Spotify action is requested while another
/// Spotify authentication or session check is still in flight.
pub(crate) const WELCOME_SPOTIFY_AUTH_IN_FLIGHT_NOTICE: &str =
    "Spotify authentication already in progress; wait for the current check to finish.";

/// Typed lifecycle state for provider actions rendered by Welcome. The
/// renderer uses this value for honest progress/result copy; it never infers
/// an HTTP or auth diagnosis by parsing a user-facing message.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum WelcomeOperation {
    #[default]
    Idle,
    SigningIn,
    Checking,
    Waiting,
    Succeeded,
    RateLimited,
    Failed,
    Cancelled,
}

impl WelcomeOperation {
    pub const fn is_busy(self) -> bool {
        matches!(self, Self::SigningIn | Self::Checking | Self::Waiting)
    }
}

/// Application's UI state
// The flags are independent pieces of UI state, not an encodable mode.
#[allow(clippy::struct_excessive_bools)]
#[derive(Debug)]
pub struct UIState {
    pub is_running: bool,
    pub active_provider: config::ActiveProvider,
    pub spotify_account_label: Option<String>,
    pub youtube_account_label: Option<String>,
    /// Cached provider identity used by account-scoped projections. Unlike a
    /// label, this value must remain stable when two accounts share a name.
    pub(crate) youtube_account_id: Option<String>,
    pub(crate) spotify_account_id: Option<String>,
    // Provider-local epochs invalidate keyed selections when account data is
    // cleared, without retaining or exposing account identity.
    spotify_selection_epoch: u64,
    youtube_selection_epoch: u64,
    pub spotify_auth_status: config::SpotifyAuthSnapshot,
    pub welcome_spotify_client_id: String,
    pub welcome_spotify_client_command: bool,
    pub welcome_spotify_client_pending: bool,
    pub welcome_spotify_notice: Option<String>,
    pub welcome_spotify_web_token_cached: bool,
    /// Whether the Welcome flow has completed an explicit library check.
    /// Cached credentials remain unverified until this is populated.
    pub welcome_spotify_library_tested: Option<bool>,
    /// Whether integrated Spotify playback has completed an explicit check.
    pub welcome_spotify_playback_tested: Option<bool>,
    /// True while a Welcome Spotify check or authentication request is in
    /// flight. Owned by the page/state layer so a second dispatch cannot
    /// overlap the first and recreate the integrated session.
    pub welcome_spotify_auth_in_flight: bool,
    pub welcome_spotify_operation: WelcomeOperation,
    pub youtube_auth_status: config::YouTubeMusicAuthStatus,
    pub welcome_youtube_notice: Option<String>,
    pub welcome_youtube_browser: Option<std::path::PathBuf>,
    pub welcome_youtube_login_active: bool,
    pub welcome_youtube_operation: WelcomeOperation,
    pub welcome_listenbrainz_attempt: u64,
    pub welcome_listenbrainz_pending: Option<u64>,
    pub welcome_listenbrainz_notice: Option<String>,
    pub welcome_listenbrainz_username: Option<String>,
    pub welcome_listenbrainz_identity:
        Option<crate::client::listenbrainz::ValidatedListenBrainzIdentity>,
    /// Whether the account/library credential has completed a live Welcome
    /// check. A saved credential alone remains "not checked".
    pub welcome_youtube_account_tested: Option<bool>,
    /// `Some(true)` is confirmed native playback; `Some(false)` is a tested
    /// unavailable transport. `None` means no playback test has run.
    pub welcome_youtube_playback_tested: Option<bool>,
    pub setup_state: config::SetupState,
    pub theme: config::Theme,
    /// Presentation preferences owned by the active UI session. Keeping a
    /// copy here lets settings changes apply immediately without replacing
    /// the process-wide immutable configuration handle.
    pub presentation: config::PresentationConfig,
    pub frame_layout: config::FrameLayoutConfig,
    /// Minimum time between redraws; the render loop reads it every frame.
    pub frame_interval: std::time::Duration,
    /// Terminal title templates; an empty `terminal_title` leaves the
    /// terminal's own title alone.
    pub terminal_title: String,
    pub terminal_title_idle: String,
    /// Names of Spotify items added to the unified queue, kept for display.
    pub spotify_queue_labels: crate::state::SpotifyQueueLabelCache,
    focused_marquee: FocusedMarqueeState,
    pub input_key_sequence: key::KeySequence,
    pub orientation: ui::Orientation,
    pub layout_mode: ui::LayoutMode,

    pub workspace_focus: WorkspaceFocusState,
    pub workspace_navigation: WorkspaceNavigationItem,
    pub workspace_action: WorkspaceAction,
    pub workspace_settings_category: SettingsCategory,
    pub workspace_settings_action: SettingsWorkspaceAction,
    pub(crate) workspace_queue_list: ListState,
    /// Persistent viewport state shared by the page and popup command-help
    /// projections. Renderers must not recreate stateful list widgets each
    /// frame because doing so resets their viewport bookkeeping.
    pub(crate) command_help_list_state: ListState,
    pub(crate) command_help_table_state: TableState,
    pub workspace_layout: ui::WorkspaceLayout,
    pub(crate) workspace_hits: Vec<(Rect, WorkspaceHit)>,
    pub(crate) workspace_popup_hits: Vec<(Rect, usize)>,
    /// The current frame's popup boundary. Pointer events use this owner
    /// rectangle so outside clicks cannot fall through to the page.
    pub(crate) popup_rect: Rect,
    workspace_last_click: Option<(WorkspaceHit, Instant)>,
    workspace_pointer: Option<(u16, u16)>,
    workspace_hover: Option<WorkspaceHit>,

    pub history: Vec<PageState>,
    pub popup: Option<PopupState>,

    /// Selection owned by the local session-history popup. Keys are indices
    /// in its newest-first projection, not provider identifiers.
    pub session_history_selection: SelectionState<usize>,

    /// The latest privacy-safe operation status shown in the ordinary UI.
    /// This is deliberately a single bounded item; detailed history belongs
    /// to the diagnostics page.
    pub operation_status: Option<UiOperationStatus>,

    /// The single active bulk operation, retained until every planned
    /// mutation reaches a terminal state.
    pub(crate) active_bulk_operation: Option<ActiveBulkOperation>,

    /// One explicit remote backup may be active at a time. The playlist ID
    /// and UI reference prevent late provider results from being attached to
    /// a different page or a newer operation.
    pub(crate) active_listenbrainz_backup: Option<ActiveListenBrainzBackup>,

    /// One read-only sync check may be active at a time. The captured target
    /// prevents page or row changes from retargeting a late provider result.
    pub(crate) active_listenbrainz_sync_check: Option<ActiveListenBrainzSyncCheck>,

    /// The rectangle representing the playback progress bar,
    /// which is mainly used to handle mouse click events (for seeking command)
    pub playback_progress_bar_rect: ratatui::layout::Rect,

    /// The full playback surface used to scope mouse-wheel volume changes.
    /// Keeping this hit area in UI state lets input handling follow the
    /// rendered layout instead of guessing from the current page.
    pub playback_window_rect: ratatui::layout::Rect,

    /// The visible play/pause control in the design-v1 bottom transport.
    /// Empty means the current frame does not expose a clickable toggle.
    pub playback_toggle_rect: ratatui::layout::Rect,

    /// Count prefix for vim-style navigation (e.g., 5j, 10k)
    pub count_prefix: Option<usize>,

    pub(crate) diagnostic_revision: u64,

    next_operation_reference: u64,

    #[cfg(feature = "image")]
    pub last_cover_image_render_info: ImageRenderInfo,

    #[cfg(feature = "image")]
    pub picker: Picker,
}

impl UIState {
    pub(crate) fn clear_playback_hit_regions(&mut self) {
        self.playback_window_rect = Rect::default();
        self.playback_toggle_rect = Rect::default();
        self.playback_progress_bar_rect = Rect::default();
        self.workspace_layout = ui::WorkspaceLayout::default();
        self.workspace_hits.clear();
        self.workspace_popup_hits.clear();
        self.popup_rect = Rect::default();
        self.workspace_hover = None;
    }

    pub(crate) fn clear_popup_hit_regions(&mut self) {
        self.workspace_popup_hits.clear();
        self.popup_rect = Rect::default();
    }

    pub(crate) fn reset_command_help_view(&mut self) {
        self.command_help_list_state.select(None);
        *self.command_help_list_state.offset_mut() = 0;
        self.command_help_table_state.select(None);
        *self.command_help_table_state.offset_mut() = 0;
    }

    pub(crate) fn workspace_context_is_active(&self) -> bool {
        matches!(
            self.current_page(),
            PageState::Context { .. } | PageState::YouTubeContext { .. }
        )
    }

    /// Whether the visible rail should emphasize a route for the current
    /// page. Pages with their own content model keep the rail available, but
    /// do not falsely present the default Playlists route as active.
    pub(crate) fn workspace_navigation_is_active(&self, item: WorkspaceNavigationItem) -> bool {
        matches!(
            self.current_page(),
            PageState::Home { .. }
                | PageState::HomeShelfList { .. }
                | PageState::Library { .. }
                | PageState::Context { .. }
                | PageState::YouTubeContext { .. }
                | PageState::Search { .. }
                | PageState::Queue { .. }
                | PageState::UnifiedPlaylist { .. }
        ) && self.workspace_navigation == item
    }

    pub(crate) fn workspace_queue_is_active(&self) -> bool {
        self.workspace_context_is_active()
            && self.workspace_layout.show_right
            && self.workspace_focus == WorkspaceFocusState::Queue
    }

    pub(crate) fn workspace_actions_is_active(&self) -> bool {
        self.workspace_context_is_active()
            && self.workspace_layout.show_right
            && self.workspace_focus == WorkspaceFocusState::Actions
    }

    pub(crate) fn workspace_hit_at(&self, column: u16, row: u16) -> Option<WorkspaceHit> {
        self.workspace_hits
            .iter()
            .find(|(rect, _)| {
                rect.width > 0
                    && rect.height > 0
                    && column >= rect.x
                    && column < rect.right()
                    && row >= rect.y
                    && row < rect.bottom()
            })
            .map(|(_, hit)| *hit)
    }

    pub(crate) fn workspace_hit_rect(&self, hit: WorkspaceHit) -> Option<Rect> {
        self.workspace_hits
            .iter()
            .find(|(_, candidate)| *candidate == hit)
            .map(|(rect, _)| *rect)
    }

    pub(crate) fn workspace_popup_hit_at(&self, column: u16, row: u16) -> Option<usize> {
        self.workspace_popup_hits
            .iter()
            .find(|(rect, _)| {
                rect.width > 0
                    && rect.height > 0
                    && column >= rect.x
                    && column < rect.right()
                    && row >= rect.y
                    && row < rect.bottom()
            })
            .map(|(_, index)| *index)
    }

    pub(crate) fn popup_contains_point(&self, column: u16, row: u16) -> bool {
        let rect = self.popup_rect;
        if rect.width > 0
            && rect.height > 0
            && column >= rect.x
            && column < rect.right()
            && row >= rect.y
            && row < rect.bottom()
        {
            true
        } else {
            // Keep synthetic/unit-test hit maps useful before a render pass
            // has populated the full popup boundary. Include one cell around
            // the visible rows so the popup border remains owned as well.
            let Some((min_x, max_x, min_y, max_y)) = self
                .workspace_popup_hits
                .iter()
                .filter(|(hit, _)| hit.width > 0 && hit.height > 0)
                .map(|(hit, _)| (hit.x, hit.right(), hit.y, hit.bottom()))
                .reduce(|(min_x, max_x, min_y, max_y), (x, right, y, bottom)| {
                    (
                        min_x.min(x),
                        max_x.max(right),
                        min_y.min(y),
                        max_y.max(bottom),
                    )
                })
            else {
                return false;
            };
            column >= min_x.saturating_sub(1)
                && column < max_x.saturating_add(1)
                && row >= min_y.saturating_sub(1)
                && row < max_y.saturating_add(1)
        }
    }

    /// Store the latest terminal pointer without making the renderers own
    /// transient input state. The current frame's hit map is reused when it
    /// is available; the render pass refreshes the same result after it has
    /// rebuilt responsive hit rectangles.
    pub(crate) fn set_workspace_pointer(&mut self, column: u16, row: u16) {
        self.workspace_pointer = Some((column, row));
        self.refresh_workspace_hover();
    }

    pub(crate) fn clear_workspace_pointer(&mut self) {
        self.workspace_pointer = None;
        self.workspace_hover = None;
    }

    pub(crate) fn refresh_workspace_hover(&mut self) {
        self.workspace_hover = self
            .workspace_pointer
            .filter(|_| self.popup.is_none())
            .and_then(|(column, row)| self.workspace_hit_at(column, row));
    }

    /// Forget the hovered page control while something covers the page.
    pub(crate) fn suppress_workspace_hover(&mut self) {
        self.workspace_hover = None;
    }

    pub(crate) fn workspace_hover_rect(&self) -> Option<Rect> {
        let hit = self.workspace_hover?;
        let (column, row) = self.workspace_pointer?;
        self.workspace_hits
            .iter()
            .find(|(rect, candidate)| {
                *candidate == hit
                    && rect.width > 0
                    && rect.height > 0
                    && column >= rect.x
                    && column < rect.right()
                    && row >= rect.y
                    && row < rect.bottom()
            })
            .map(|(rect, _)| *rect)
    }

    /// Return true on a second click of the same visible hit within the
    /// terminal double-click window. Single clicks only select and focus.
    pub(crate) fn workspace_click_activates(&mut self, hit: WorkspaceHit) -> bool {
        let now = Instant::now();
        let activates = self.workspace_last_click.is_some_and(|(previous, at)| {
            previous == hit
                && now.saturating_duration_since(at) <= std::time::Duration::from_millis(500)
        });
        self.workspace_last_click = Some((hit, now));
        activates
    }

    // Each focus table lists one row per (pane, direction) transition.
    #[allow(clippy::match_same_arms)]
    pub(crate) fn focus_workspace(&mut self, forward: bool) -> bool {
        if self.has_focused_popup() {
            return false;
        }
        if matches!(self.current_page(), PageState::Welcome { .. }) {
            return self.focus_welcome(forward);
        }
        if let PageState::Context {
            state: Some(ContextPageUIState::Artist { focus, .. }),
            ..
        } = self.current_page()
        {
            let sections = [
                ArtistFocusState::TopTracks,
                ArtistFocusState::LikedSongs,
                ArtistFocusState::Albums,
                ArtistFocusState::RelatedArtists,
            ];
            let total = if self.workspace_layout.show_right {
                7
            } else {
                5
            };
            let current = match self.workspace_focus {
                WorkspaceFocusState::Navigation => 0,
                WorkspaceFocusState::Context => {
                    sections
                        .iter()
                        .position(|item| item == focus)
                        .unwrap_or_default()
                        + 1
                }
                WorkspaceFocusState::Queue => 5,
                WorkspaceFocusState::Actions => 6,
            } % total;
            let next = if forward {
                (current + 1) % total
            } else {
                (current + total - 1) % total
            };
            self.workspace_focus = match next {
                0 => WorkspaceFocusState::Navigation,
                5 => WorkspaceFocusState::Queue,
                6 => WorkspaceFocusState::Actions,
                _ => WorkspaceFocusState::Context,
            };
            if (1..=4).contains(&next) {
                if let PageState::Context {
                    state:
                        Some(ContextPageUIState::Artist {
                            focus,
                            listenbrainz_pending,
                            listenbrainz_album_pending,
                            ..
                        }),
                    ..
                } = self.current_page_mut()
                {
                    if *focus != sections[next - 1] {
                        *focus = sections[next - 1];
                        *listenbrainz_pending = None;
                        *listenbrainz_album_pending = None;
                    }
                }
            }
            self.bump_diagnostic_revision();
            return true;
        }
        if matches!(self.current_page(), PageState::Settings { .. }) {
            self.workspace_focus = match (self.workspace_focus, forward) {
                (WorkspaceFocusState::Navigation, true) => WorkspaceFocusState::Context,
                (WorkspaceFocusState::Context, true) => WorkspaceFocusState::Actions,
                (WorkspaceFocusState::Actions, true) => WorkspaceFocusState::Navigation,
                (WorkspaceFocusState::Navigation, false) => WorkspaceFocusState::Actions,
                (WorkspaceFocusState::Context, false) => WorkspaceFocusState::Navigation,
                (WorkspaceFocusState::Actions, false) => WorkspaceFocusState::Context,
                (WorkspaceFocusState::Queue, _) => WorkspaceFocusState::Context,
            };
            self.bump_diagnostic_revision();
            return true;
        }
        if self.workspace_layout.show_right {
            self.workspace_focus = match (self.workspace_focus, forward) {
                (WorkspaceFocusState::Navigation, true) => WorkspaceFocusState::Context,
                (WorkspaceFocusState::Context, true) => WorkspaceFocusState::Queue,
                (WorkspaceFocusState::Queue, true) => WorkspaceFocusState::Actions,
                (WorkspaceFocusState::Actions, false) => WorkspaceFocusState::Queue,
                (WorkspaceFocusState::Queue, false) => WorkspaceFocusState::Context,
                (WorkspaceFocusState::Context, false) => WorkspaceFocusState::Navigation,
                (WorkspaceFocusState::Navigation, false) | (WorkspaceFocusState::Actions, true) => {
                    return false;
                }
            };
        } else {
            self.workspace_focus = match self.workspace_focus {
                WorkspaceFocusState::Navigation => WorkspaceFocusState::Context,
                WorkspaceFocusState::Context
                | WorkspaceFocusState::Queue
                | WorkspaceFocusState::Actions => WorkspaceFocusState::Navigation,
            };
        }
        self.bump_diagnostic_revision();
        true
    }

    /// Cycle Welcome focus through the step rail (when drawn), the step's
    /// actions, and the button row.
    fn focus_welcome(&mut self, forward: bool) -> bool {
        let PageState::Welcome { state, .. } = self.current_page() else {
            return false;
        };
        let panes: &[WorkspaceFocusState] = if state.rail_visible {
            &[
                WorkspaceFocusState::Navigation,
                WorkspaceFocusState::Context,
                WorkspaceFocusState::Actions,
            ]
        } else {
            &[WorkspaceFocusState::Context, WorkspaceFocusState::Actions]
        };
        let current = self.welcome_focus();
        let position = panes.iter().position(|pane| *pane == current).unwrap_or(0);
        let next = if forward {
            (position + 1) % panes.len()
        } else {
            (position + panes.len() - 1) % panes.len()
        };
        self.set_welcome_focus(panes[next]);
        true
    }

    /// The Welcome pane that owns keyboard input. A rail hidden by a resize
    /// hands its focus to the content pane.
    pub(crate) fn welcome_focus(&self) -> WorkspaceFocusState {
        match (self.workspace_focus, self.current_page()) {
            (WorkspaceFocusState::Navigation, PageState::Welcome { state, .. })
                if state.rail_visible =>
            {
                WorkspaceFocusState::Navigation
            }
            (WorkspaceFocusState::Actions, _) => WorkspaceFocusState::Actions,
            _ => WorkspaceFocusState::Context,
        }
    }

    pub(crate) fn set_welcome_focus(&mut self, focus: WorkspaceFocusState) {
        if let PageState::Welcome { state, .. } = self.current_page_mut() {
            state.enter(focus);
        }
        self.workspace_focus = focus;
        self.bump_diagnostic_revision();
    }

    /// Move inside the focused Welcome pane.
    pub(crate) fn move_welcome(&mut self, movement: WelcomeMove) {
        let focus = self.welcome_focus();
        if let PageState::Welcome { state, .. } = self.current_page_mut() {
            state.move_in(focus, movement);
        }
        self.bump_diagnostic_revision();
    }

    /// Select a Welcome action and focus the pane that owns it.
    pub(crate) fn select_welcome_action(&mut self, index: usize) {
        let PageState::Welcome { state, .. } = self.current_page_mut() else {
            return;
        };
        let pane = state.pane_of(index);
        state.list.select(Some(index));
        if pane == WorkspaceFocusState::Context {
            state.task_selection = index;
        }
        self.workspace_focus = pane;
        self.bump_diagnostic_revision();
    }

    /// Show a Welcome step. Leaving through the button row lands on the new
    /// step's first action; the rail keeps its focus while browsing steps.
    pub(crate) fn show_welcome_step(&mut self, step: WelcomeStep) {
        if let PageState::Welcome { state, .. } = self.current_page_mut() {
            state.show_step(step);
        }
        // Row indices now name the new step's actions; a click before the
        // next frame must not land on last frame's geometry.
        self.workspace_hits.clear();
        self.workspace_hover = None;
        if step != WelcomeStep::ListenBrainz {
            self.cancel_welcome_listenbrainz_check();
        }
        if self.workspace_focus != WorkspaceFocusState::Navigation {
            self.workspace_focus = WorkspaceFocusState::Context;
        }
        self.bump_diagnostic_revision();
    }

    /// Apply a click on a Welcome step or action with the workspace policy:
    /// the first click selects and focuses, a repeated click activates.
    /// Returns true when the selected action should run.
    pub(crate) fn click_welcome(&mut self, hit: WorkspaceHit, activate: bool) -> bool {
        match hit {
            WorkspaceHit::WelcomeStep(step) => {
                let current = match self.current_page() {
                    PageState::Welcome { state, .. } => state.step,
                    _ => return false,
                };
                if step != current {
                    self.show_welcome_step(step);
                }
                // Like the Settings rail: select the step, then enter it.
                if activate {
                    self.set_welcome_focus(WorkspaceFocusState::Context);
                } else {
                    self.workspace_focus = WorkspaceFocusState::Navigation;
                }
                false
            }
            WorkspaceHit::WelcomeAction(index) => {
                self.select_welcome_action(index);
                activate
            }
            _ => false,
        }
    }

    pub(crate) fn apply_presentation_config(&mut self, config: &config::AppConfig) {
        self.presentation = config.presentation.effective();
        self.frame_layout = config::FrameLayoutConfig::from(config);
        self.frame_interval = config.ui_frame_interval();
        self.terminal_title.clone_from(&config.terminal_title);
        self.terminal_title_idle
            .clone_from(&config.terminal_title_idle);
        self.clear_playback_hit_regions();
    }

    pub(crate) fn project_frame_chrome(&mut self) {
        let border_type = match self.presentation.layout_preset {
            config::LayoutPreset::Current => self.frame_layout.border_type.clone(),
            config::LayoutPreset::Borderless => config::BorderType::Hidden,
        };
        self.theme.project_border_type(border_type);
    }

    pub(crate) fn refresh_focused_marquee(&mut self) {
        self.refresh_focused_marquee_at(Instant::now());
    }

    fn refresh_focused_marquee_at(&mut self, now: Instant) {
        let mode = match self.presentation.focused_row_overflow {
            config::FocusedRowOverflow::Truncate => "truncate",
            config::FocusedRowOverflow::Marquee => "marquee",
            config::FocusedRowOverflow::Manual => "manual",
        };
        let focus = if self.has_focused_popup() {
            format!(
                "popup:{:?}:{}",
                self.popup.as_ref().map(std::mem::discriminant),
                self.popup
                    .as_ref()
                    .and_then(PopupState::list_selected)
                    .map_or_else(|| "none".to_owned(), |index| index.to_string())
            )
        } else {
            let page = self.current_page();
            let provider = match self.active_provider {
                config::ActiveProvider::Spotify => "spotify",
                config::ActiveProvider::YouTubeMusic => "youtube_music",
            };
            format!(
                "page:{provider}:{}:{}:{}:{}",
                self.history.len(),
                page_label(page.page_type()),
                page.marquee_focus_label(),
                page.selected_index()
                    .map_or_else(|| "none".to_owned(), |index| index.to_string())
            )
        };
        let manual = self.presentation.focused_row_overflow == config::FocusedRowOverflow::Manual;
        self.focused_marquee
            .refresh(&format!("{mode}:{focus}"), now, manual);
    }

    /// Record how far the focused row can scroll, as measured while drawing.
    pub(crate) fn set_manual_scroll_extent(&mut self, extent: usize) {
        self.focused_marquee.manual_extent = extent;
    }

    /// Scroll the focused row's overflowing text by one character in
    /// `Manual` mode. Returns false when the mode is different or nothing in
    /// the focused row overflows, so the key can be handled elsewhere.
    pub(crate) fn scroll_focused_row(&mut self, forward: bool) -> bool {
        self.presentation.focused_row_overflow == config::FocusedRowOverflow::Manual
            && self.focused_marquee.scroll_manually(forward)
    }

    pub(crate) const fn focused_marquee_phase(&self) -> usize {
        self.focused_marquee.phase
    }

    /// Time until the focused marquee advances to its next phase.
    pub(crate) fn until_next_marquee_phase(&self, now: Instant) -> std::time::Duration {
        self.focused_marquee.until_next_phase(now)
    }

    #[cfg(feature = "image")]
    pub(crate) fn cover_image_render_parts(&mut self) -> (&Picker, &mut ImageRenderInfo) {
        (&self.picker, &mut self.last_cover_image_render_info)
    }

    /// Return the current privacy-safe selection epoch for `provider`.
    pub(crate) const fn provider_selection_epoch(&self, provider: config::ActiveProvider) -> u64 {
        match provider {
            config::ActiveProvider::Spotify => self.spotify_selection_epoch,
            config::ActiveProvider::YouTubeMusic => self.youtube_selection_epoch,
        }
    }

    /// Advance the selection epoch for `provider`.
    ///
    /// The epoch intentionally carries no account identifier. Exhaustion is
    /// reported so an account transition cannot silently reuse a selection
    /// scope.
    pub(crate) fn bump_provider_selection_epoch(
        &mut self,
        provider: config::ActiveProvider,
    ) -> bool {
        let epoch = match provider {
            config::ActiveProvider::Spotify => &mut self.spotify_selection_epoch,
            config::ActiveProvider::YouTubeMusic => &mut self.youtube_selection_epoch,
        };
        let Some(next) = epoch.checked_add(1) else {
            return false;
        };
        *epoch = next;
        true
    }

    pub fn current_page(&self) -> &PageState {
        self.history.last().expect("non-empty history")
    }

    pub fn current_page_mut(&mut self) -> &mut PageState {
        self.history.last_mut().expect("non-empty history")
    }

    /// Return the single layout policy consumed by every page renderer.
    pub(crate) const fn layout_policy(&self) -> ui::LayoutPolicy {
        ui::LayoutPolicy::new(self.layout_mode, self.orientation)
    }

    pub fn new_search_popup(&mut self) {
        self.current_page_mut().select(0);
        self.popup = Some(PopupState::Search {
            query: String::new(),
        });
    }

    /// Open the theme picker shared by `SwitchTheme` and Settings. Moving the
    /// selection previews a theme live; choosing one saves it to the config.
    pub fn open_theme_picker(&mut self) {
        // The active theme goes first so closing the picker can restore it.
        let mut themes = config::get_config().theme_config.themes.clone();
        if let Some(id) = themes.iter().position(|t| t.name == self.theme.name) {
            let theme = themes.remove(id);
            themes.insert(0, theme);
        }
        self.popup = Some(PopupState::ThemeList(themes, ListState::default()));
    }

    /// The provider, account and session readiness Home is built for.
    pub(crate) fn home_scope(&self) -> crate::state::HomeScope<'_> {
        crate::state::HomeScope {
            provider: self.active_provider,
            account: self.account_id(self.active_provider),
            // Library reads need a signed-in session, not Premium.
            spotify_ready: self.spotify_auth_status.session_ready,
        }
    }

    /// The active account of `provider`, used to scope local history.
    pub(crate) fn account_id(&self, provider: config::ActiveProvider) -> Option<&str> {
        match provider {
            config::ActiveProvider::Spotify => self.spotify_account_id.as_deref(),
            config::ActiveProvider::YouTubeMusic => self.youtube_account_id.as_deref(),
        }
    }

    pub(crate) fn cancel_welcome_listenbrainz_check(&mut self) {
        if self.welcome_listenbrainz_pending.take().is_some() {
            self.welcome_listenbrainz_notice = Some("Token check cancelled.".to_owned());
        }
    }

    pub fn new_page(&mut self, page: PageState) {
        self.cancel_welcome_listenbrainz_check();
        self.clear_search_lucky();
        self.popup = None;
        self.workspace_hover = None;
        if let Some(current_page) = self.history.last() {
            if &page == current_page {
                return;
            }
        }
        self.history.push(page);
        if self.history.len() > MAX_PAGE_HISTORY {
            self.history.remove(0);
        }
        let page_type = self.current_page().page_type();
        self.workspace_navigation = workspace_navigation_for_page(self.current_page());
        self.workspace_focus = WorkspaceFocusState::Context;
        if page_type == PageType::Settings {
            self.workspace_settings_category = SettingsCategory::Preferences;
            self.workspace_settings_action = SettingsWorkspaceAction::Apply;
        }
        self.bump_diagnostic_revision();
    }

    /// Reconcile workspace focus and rail state after the page history was
    /// edited without going through `new_page` (Back, in-place replacement).
    pub(crate) fn sync_workspace_after_history_change(&mut self) {
        self.workspace_hover = None;
        // Utility pages keep whichever rail route the user came from.
        if matches!(
            self.current_page().page_type(),
            PageType::Home
                | PageType::HomeShelfList
                | PageType::Library
                | PageType::Context
                | PageType::YouTubeContext
                | PageType::Search
                | PageType::UnifiedPlaylist
                | PageType::Queue
        ) {
            self.workspace_navigation = workspace_navigation_for_page(self.current_page());
        }
        self.workspace_focus = WorkspaceFocusState::Context;
    }

    pub fn open_setup_page(&mut self, from_settings: bool) {
        let configs = config::get_config();
        if let Ok(saved) = config::AppConfig::new(&configs.config_folder) {
            self.welcome_spotify_client_id = saved.client_id;
            self.welcome_spotify_client_command = saved.client_id_command.is_some();
        }
        if let Ok(saved) = config::SetupState::load(&configs.config_folder) {
            self.setup_state.spotify_reauthentication_required =
                saved.spotify_reauthentication_required;
            self.welcome_spotify_client_pending = saved.spotify_reauthentication_required;
        }
        self.new_page(PageState::Welcome {
            state: WelcomePageUIState::new(),
            from_settings,
        });
    }

    pub(crate) fn setup_auth_snapshot(&self) -> config::SetupAuthSnapshot {
        config::SetupAuthSnapshot {
            spotify: config::SpotifyAuthSnapshot {
                session_ready: self.spotify_auth_status.session_ready
                    && !self.welcome_spotify_client_pending
                    && !self.setup_state.spotify_reauthentication_required
                    && !self.welcome_spotify_auth_in_flight
                    && self.welcome_spotify_library_tested != Some(false)
                    && self.welcome_spotify_playback_tested != Some(false),
                premium: self.spotify_auth_status.premium,
            },
            youtube: config::YouTubeAuthSnapshot {
                account_ready: self.youtube_auth_status.ready
                    && self.welcome_youtube_account_tested != Some(false),
            },
        }
    }

    pub(crate) fn mark_setup_pending(&mut self) {
        self.setup_state.status = config::SetupStatus::Pending;
        self.setup_state.failure = None;
        self.bump_diagnostic_revision();
    }

    pub(crate) fn mark_setup_ready_if_possible(&mut self) {
        if self.setup_state.status == config::SetupStatus::Skipped {
            return;
        }
        self.setup_state.failure = self.setup_state.failure_for(self.setup_auth_snapshot());
        self.setup_state.status = if self.setup_state.failure.is_some() {
            config::SetupStatus::Failed
        } else {
            config::SetupStatus::Ready
        };
        self.bump_diagnostic_revision();
    }

    pub(crate) fn mark_setup_failed(&mut self, failure: config::SetupFailure) {
        self.setup_state.status = config::SetupStatus::Failed;
        self.setup_state.failure = Some(failure);
        self.bump_diagnostic_revision();
    }

    pub(crate) fn mark_setup_cancelled(&mut self) {
        self.setup_state.status = config::SetupStatus::Pending;
        self.setup_state.failure = None;
        self.bump_diagnostic_revision();
    }

    /// Claim the Welcome Spotify auth slot. Returns false (and repeats the
    /// waiting notice) when another Spotify check or sign-in is in flight.
    #[allow(dead_code)] // Kept as the compatibility entry point for existing callers.
    pub(crate) fn begin_welcome_spotify_auth(&mut self) -> bool {
        self.begin_welcome_spotify_action(WelcomeOperation::Checking)
    }

    pub(crate) fn begin_welcome_spotify_action(&mut self, operation: WelcomeOperation) -> bool {
        if self.welcome_spotify_auth_in_flight {
            self.welcome_spotify_operation = WelcomeOperation::Waiting;
            self.welcome_spotify_notice = Some(WELCOME_SPOTIFY_AUTH_IN_FLIGHT_NOTICE.to_owned());
            return false;
        }
        self.welcome_spotify_auth_in_flight = true;
        self.welcome_spotify_operation = operation;
        true
    }

    /// Release the Welcome Spotify auth slot after completion or failure.
    #[allow(dead_code)] // Kept as the compatibility entry point for existing callers.
    pub(crate) fn finish_welcome_spotify_auth(&mut self) {
        self.finish_welcome_spotify_action(WelcomeOperation::Idle);
    }

    pub(crate) fn finish_welcome_spotify_action(&mut self, result: WelcomeOperation) {
        self.welcome_spotify_auth_in_flight = false;
        self.welcome_spotify_operation = result;
    }

    pub(crate) fn begin_welcome_youtube_action(&mut self, operation: WelcomeOperation) -> bool {
        if self.welcome_youtube_operation.is_busy() {
            self.welcome_youtube_operation = WelcomeOperation::Waiting;
            self.welcome_youtube_notice = Some(
                "A YouTube Music action is already in progress; wait for it to finish.".to_owned(),
            );
            return false;
        }
        self.welcome_youtube_operation = operation;
        true
    }

    pub(crate) fn finish_welcome_youtube_action(&mut self, result: WelcomeOperation) {
        self.welcome_youtube_login_active = false;
        self.welcome_youtube_operation = result;
    }

    pub(crate) fn start_operation(
        &mut self,
        kind: UiOperationKind,
        code: &'static str,
        message: &'static str,
    ) -> String {
        self.next_operation_reference = self.next_operation_reference.saturating_add(1);
        let reference = format!("ui-{:04}", self.next_operation_reference);
        self.operation_status = Some(UiOperationStatus {
            reference: reference.clone(),
            kind,
            state: UiOperationState::Running,
            code,
            message,
            details: None,
            next_action: None,
            expires_at: None,
            bulk_summary: None,
        });
        self.bump_diagnostic_revision();
        reference
    }

    pub(crate) fn start_listenbrainz_backup(&mut self, playlist_id: &str) -> Option<String> {
        if self.active_listenbrainz_backup.is_some() {
            return None;
        }
        let reference = self.start_operation(
            UiOperationKind::ProviderCommand,
            LISTENBRAINZ_BACKUP_RUNNING_CODE,
            LISTENBRAINZ_BACKUP_RUNNING_MESSAGE,
        );
        self.active_listenbrainz_backup = Some(ActiveListenBrainzBackup {
            reference: reference.clone(),
            playlist_id: playlist_id.to_owned(),
        });
        Some(reference)
    }

    fn take_listenbrainz_backup(&mut self, playlist_id: &str, reference: &str) -> bool {
        let matches = self
            .active_listenbrainz_backup
            .as_ref()
            .is_some_and(|active| {
                active.playlist_id == playlist_id && active.reference == reference
            });
        if matches {
            self.active_listenbrainz_backup = None;
        }
        matches
    }

    fn set_operation_details(&mut self, reference: &str, details: String) {
        let Some(status) = self.operation_status.as_mut() else {
            return;
        };
        if status.reference != reference {
            return;
        }
        status.details = Some(details);
        self.bump_diagnostic_revision();
    }

    pub(crate) fn finish_listenbrainz_backup_completed(
        &mut self,
        playlist_id: &str,
        reference: &str,
        playlist_mbid: &str,
    ) -> bool {
        if !self.take_listenbrainz_backup(playlist_id, reference) {
            return false;
        }
        self.complete_operation(
            reference,
            UiOperationState::Completed,
            LISTENBRAINZ_BACKUP_COMPLETED_CODE,
            LISTENBRAINZ_BACKUP_COMPLETED_MESSAGE,
            None,
        );
        self.set_operation_details(reference, format!("Remote playlist: {playlist_mbid}."));
        true
    }

    pub(crate) fn finish_listenbrainz_backup_partial(
        &mut self,
        playlist_id: &str,
        reference: &str,
        playlist_mbid: &str,
    ) -> bool {
        if !self.take_listenbrainz_backup(playlist_id, reference) {
            return false;
        }
        self.complete_operation(
            reference,
            UiOperationState::Partial,
            LISTENBRAINZ_BACKUP_PARTIAL_CODE,
            LISTENBRAINZ_BACKUP_PARTIAL_MESSAGE,
            Some(LISTENBRAINZ_BACKUP_PARTIAL_NEXT_ACTION),
        );
        self.set_operation_details(reference, format!("Remote playlist: {playlist_mbid}."));
        true
    }

    pub(crate) fn finish_listenbrainz_backup_failed(
        &mut self,
        playlist_id: &str,
        reference: &str,
    ) -> bool {
        if !self.take_listenbrainz_backup(playlist_id, reference) {
            return false;
        }
        self.complete_operation(
            reference,
            UiOperationState::Failed,
            LISTENBRAINZ_BACKUP_FAILED_CODE,
            LISTENBRAINZ_BACKUP_FAILED_MESSAGE,
            Some(LISTENBRAINZ_BACKUP_FAILED_NEXT_ACTION),
        );
        true
    }

    pub(crate) fn finish_listenbrainz_backup_cancelled(
        &mut self,
        playlist_id: &str,
        reference: &str,
    ) -> bool {
        if !self.take_listenbrainz_backup(playlist_id, reference) {
            return false;
        }
        self.complete_operation(
            reference,
            UiOperationState::Cancelled,
            LISTENBRAINZ_BACKUP_CANCELLED_CODE,
            LISTENBRAINZ_BACKUP_CANCELLED_MESSAGE,
            None,
        );
        true
    }

    pub(crate) fn start_listenbrainz_sync_check(
        &mut self,
        playlist_id: &str,
        lifecycle: ListenBrainzSyncLifecycle,
    ) -> Option<String> {
        if self.active_listenbrainz_sync_check.is_some() {
            return None;
        }
        let reference = self.start_operation(
            UiOperationKind::ProviderCommand,
            LISTENBRAINZ_SYNC_CHECK_RUNNING_CODE,
            LISTENBRAINZ_SYNC_CHECK_RUNNING_MESSAGE,
        );
        self.active_listenbrainz_sync_check = Some(ActiveListenBrainzSyncCheck {
            reference: reference.clone(),
            playlist_id: playlist_id.to_owned(),
        });
        self.set_listenbrainz_sync_preview(playlist_id, None);
        self.set_listenbrainz_sync_lifecycle(playlist_id, lifecycle);
        Some(reference)
    }

    fn take_listenbrainz_sync_check(&mut self, playlist_id: &str, reference: &str) -> bool {
        let matches = self
            .active_listenbrainz_sync_check
            .as_ref()
            .is_some_and(|active| {
                active.playlist_id == playlist_id && active.reference == reference
            });
        if matches {
            self.active_listenbrainz_sync_check = None;
        }
        matches
    }

    fn set_listenbrainz_sync_lifecycle(
        &mut self,
        playlist_id: &str,
        lifecycle: ListenBrainzSyncLifecycle,
    ) {
        if let Some(PageState::UnifiedPlaylist {
            listenbrainz_sync, ..
        }) =
            self.history.iter_mut().rev().find(
                |page| matches!(page, PageState::UnifiedPlaylist { id, .. } if id == playlist_id),
            )
        {
            *listenbrainz_sync = lifecycle;
            self.bump_diagnostic_revision();
        }
    }

    fn set_listenbrainz_sync_preview(
        &mut self,
        playlist_id: &str,
        preview: Option<ListenBrainzSyncPreview>,
    ) {
        if let Some(PageState::UnifiedPlaylist {
            listenbrainz_preview,
            ..
        }) =
            self.history.iter_mut().rev().find(
                |page| matches!(page, PageState::UnifiedPlaylist { id, .. } if id == playlist_id),
            )
        {
            *listenbrainz_preview = preview;
            self.bump_diagnostic_revision();
        }
    }

    pub(crate) fn reset_listenbrainz_sync_lifecycle(&mut self, playlist_id: &str) {
        self.set_listenbrainz_sync_lifecycle(playlist_id, ListenBrainzSyncLifecycle::Idle);
    }

    pub(crate) fn finish_listenbrainz_sync_check(
        &mut self,
        playlist_id: &str,
        reference: &str,
        lifecycle: ListenBrainzSyncLifecycle,
        preview: ListenBrainzSyncPreview,
    ) -> bool {
        if !self.take_listenbrainz_sync_check(playlist_id, reference) {
            return false;
        }
        self.set_listenbrainz_sync_preview(playlist_id, Some(preview));
        self.set_listenbrainz_sync_lifecycle(playlist_id, lifecycle);
        self.complete_operation(
            reference,
            UiOperationState::Completed,
            LISTENBRAINZ_SYNC_CHECK_COMPLETED_CODE,
            LISTENBRAINZ_SYNC_CHECK_COMPLETED_MESSAGE,
            None,
        );
        true
    }

    pub(crate) fn cancel_listenbrainz_sync_check(&mut self, playlist_id: &str) -> bool {
        let Some(active) = self.active_listenbrainz_sync_check.clone() else {
            return false;
        };
        if active.playlist_id != playlist_id {
            return false;
        }
        let lifecycle = self.history.iter().rev().find_map(|page| match page {
            PageState::UnifiedPlaylist {
                id,
                listenbrainz_sync,
                ..
            } if id == playlist_id => Some(*listenbrainz_sync),
            _ => None,
        });
        match lifecycle {
            Some(
                ListenBrainzSyncLifecycle::Checking
                | ListenBrainzSyncLifecycle::Planning
                | ListenBrainzSyncLifecycle::Recovering,
            ) => {
                self.active_listenbrainz_sync_check = None;
                self.set_listenbrainz_sync_lifecycle(playlist_id, ListenBrainzSyncLifecycle::Idle);
                self.complete_operation(
                    &active.reference,
                    UiOperationState::Cancelled,
                    LISTENBRAINZ_SYNC_CHECK_CANCELLED_CODE,
                    LISTENBRAINZ_SYNC_CHECK_CANCELLED_MESSAGE,
                    None,
                );
                true
            }
            Some(ListenBrainzSyncLifecycle::WritingListenBrainz) => {
                self.set_listenbrainz_sync_lifecycle(
                    playlist_id,
                    ListenBrainzSyncLifecycle::Verifying,
                );
                false
            }
            _ => false,
        }
    }

    pub(crate) fn finish_listenbrainz_sync_check_failed(
        &mut self,
        playlist_id: &str,
        reference: &str,
    ) -> bool {
        if !self.take_listenbrainz_sync_check(playlist_id, reference) {
            return false;
        }
        self.set_listenbrainz_sync_lifecycle(
            playlist_id,
            ListenBrainzSyncLifecycle::Failed {
                message: LISTENBRAINZ_SYNC_CHECK_FAILED_MESSAGE,
                next_action: LISTENBRAINZ_SYNC_CHECK_FAILED_NEXT_ACTION,
            },
        );
        self.complete_operation(
            reference,
            UiOperationState::Failed,
            LISTENBRAINZ_SYNC_CHECK_FAILED_CODE,
            LISTENBRAINZ_SYNC_CHECK_FAILED_MESSAGE,
            Some(LISTENBRAINZ_SYNC_CHECK_FAILED_NEXT_ACTION),
        );
        true
    }

    /// Peek without consuming: apply handlers check this immediately before
    /// any write so a pre-write cancellation stays a no-op instead of a late
    /// mutation. Post-write cancellation is handled by the existing take
    /// guards in the finish helpers below.
    pub(crate) fn listenbrainz_sync_check_is_active(
        &self,
        playlist_id: &str,
        reference: &str,
    ) -> bool {
        self.active_listenbrainz_sync_check
            .as_ref()
            .is_some_and(|active| {
                active.playlist_id == playlist_id && active.reference == reference
            })
    }

    /// Finish an apply: the preview it consumed is stale by definition, so
    /// the page returns to idle with a dynamic outcome summary. The caller
    /// passes a static completion message plus dynamic details.
    pub(crate) fn finish_listenbrainz_sync_applied(
        &mut self,
        playlist_id: &str,
        reference: &str,
        message: &'static str,
        details: String,
    ) -> bool {
        if !self.take_listenbrainz_sync_check(playlist_id, reference) {
            return false;
        }
        self.set_listenbrainz_sync_preview(playlist_id, None);
        self.set_listenbrainz_sync_lifecycle(playlist_id, ListenBrainzSyncLifecycle::Idle);
        self.complete_operation(
            reference,
            UiOperationState::Completed,
            LISTENBRAINZ_SYNC_APPLY_COMPLETED_CODE,
            message,
            None,
        );
        self.set_operation_details(reference, details);
        true
    }

    pub(crate) fn finish_listenbrainz_sync_apply_failed(
        &mut self,
        playlist_id: &str,
        reference: &str,
        message: &'static str,
        next_action: &'static str,
    ) -> bool {
        if !self.take_listenbrainz_sync_check(playlist_id, reference) {
            return false;
        }
        self.set_listenbrainz_sync_lifecycle(
            playlist_id,
            ListenBrainzSyncLifecycle::Failed {
                message,
                next_action,
            },
        );
        self.complete_operation(
            reference,
            UiOperationState::Failed,
            LISTENBRAINZ_SYNC_APPLY_FAILED_CODE,
            message,
            Some(next_action),
        );
        true
    }

    /// Start one fixed-size bulk outcome tracker from a validated plan.
    ///
    /// A second tracker is refused while the existing one is still active so
    /// late terminals cannot be reassigned to a newer operation.
    pub(crate) fn start_bulk_operation<K>(
        &mut self,
        plan: &crate::command::BulkActionPlan<K>,
    ) -> Result<BulkOperationHandle, BulkOperationRuntimeError>
    where
        K: Clone,
    {
        if self.active_bulk_operation.is_some() {
            return Err(BulkOperationRuntimeError::ActiveOperation);
        }
        if plan.operation_count() == 0 {
            return Err(BulkOperationRuntimeError::EmptyPlan);
        }

        self.next_operation_reference = self.next_operation_reference.saturating_add(1);
        let reference = format!("ui-{:04}", self.next_operation_reference);
        let handle = BulkOperationHandle::new(reference.clone(), plan.operation_ids());
        let outcome = crate::command::BulkActionOutcome::new(plan);
        let summary = BulkActionUiSummary::from(outcome.summary());
        self.active_bulk_operation = Some(ActiveBulkOperation { handle, outcome });
        self.set_bulk_operation_status(&reference, summary, true);
        self.bump_diagnostic_revision();
        Ok(self
            .active_bulk_operation
            .as_ref()
            .expect("bulk operation was installed")
            .handle
            .clone())
    }

    /// Record one terminal for one or more operation IDs in the active plan.
    /// Validation is completed before any state is changed, so unknown,
    /// duplicate, and late terminals fail closed without partial mutation.
    pub(crate) fn record_bulk_terminal(
        &mut self,
        reference: &str,
        operation_ids: &[crate::command::BulkOperationId],
        terminal: crate::command::BulkOperationTerminal,
    ) -> Result<(), BulkOperationRuntimeError> {
        if operation_ids.is_empty() {
            return Err(BulkOperationRuntimeError::EmptyOperationIds);
        }
        let active = self
            .active_bulk_operation
            .as_mut()
            .ok_or(BulkOperationRuntimeError::UnknownReference)?;
        if active.handle.reference() != reference {
            return Err(BulkOperationRuntimeError::UnknownReference);
        }

        let mut seen = Vec::with_capacity(operation_ids.len());
        for operation_id in operation_ids {
            if seen.contains(operation_id) {
                return Err(BulkOperationRuntimeError::DuplicateOperationId {
                    operation_id: *operation_id,
                });
            }
            seen.push(*operation_id);
            match active.outcome.state(*operation_id) {
                None => {
                    return Err(BulkOperationRuntimeError::UnknownOperation {
                        operation_id: *operation_id,
                    });
                }
                Some(crate::command::BulkOperationState::Pending) => {}
                Some(_) => {
                    return Err(BulkOperationRuntimeError::AlreadyTerminal {
                        operation_id: *operation_id,
                    });
                }
            }
        }

        for operation_id in operation_ids {
            active
                .outcome
                .record(*operation_id, terminal)
                .expect("bulk terminal was validated before mutation");
        }
        let summary = BulkActionUiSummary::from(active.outcome.summary());
        let is_terminal = summary.pending == 0;
        if is_terminal {
            self.active_bulk_operation = None;
        }
        self.set_bulk_operation_status(reference, summary, false);
        self.bump_diagnostic_revision();
        Ok(())
    }

    #[allow(dead_code)]
    pub(crate) fn active_bulk_operation(&self) -> Option<&ActiveBulkOperation> {
        self.active_bulk_operation.as_ref()
    }

    fn set_bulk_operation_status(
        &mut self,
        reference: &str,
        summary: BulkActionUiSummary,
        replace_current: bool,
    ) {
        if !replace_current
            && !self.operation_status.as_ref().is_some_and(|status| {
                status.kind == UiOperationKind::BulkAction && status.reference == reference
            })
        {
            return;
        }
        let (code, message, expires_at) = match summary.state() {
            UiOperationState::Running => {
                (BULK_ACTION_RUNNING_CODE, "Bulk action is running.", None)
            }
            UiOperationState::Completed => (
                BULK_ACTION_COMPLETED_CODE,
                "Bulk action completed.",
                Some(std::time::Instant::now() + TERMINAL_STATUS_TTL),
            ),
            UiOperationState::Partial => (
                BULK_ACTION_PARTIAL_CODE,
                "Bulk action partially completed.",
                Some(std::time::Instant::now() + TERMINAL_STATUS_TTL),
            ),
            UiOperationState::Failed => (
                BULK_ACTION_FAILED_CODE,
                "Bulk action failed.",
                Some(std::time::Instant::now() + TERMINAL_STATUS_TTL),
            ),
            UiOperationState::Cancelled => (
                BULK_ACTION_CANCELLED_CODE,
                "Bulk action cancelled.",
                Some(std::time::Instant::now() + TERMINAL_STATUS_TTL),
            ),
            UiOperationState::Superseded => (
                BULK_ACTION_SUPERSEDED_CODE,
                "Bulk action superseded.",
                Some(std::time::Instant::now() + TERMINAL_STATUS_TTL),
            ),
            UiOperationState::Unsupported => unreachable!("bulk outcomes never become unsupported"),
        };
        self.operation_status = Some(UiOperationStatus {
            reference: reference.to_owned(),
            kind: UiOperationKind::BulkAction,
            state: summary.state(),
            code,
            message,
            details: None,
            next_action: None,
            expires_at,
            bulk_summary: Some(summary),
        });
    }

    pub(crate) fn complete_operation(
        &mut self,
        reference: &str,
        state: UiOperationState,
        code: &'static str,
        message: &'static str,
        next_action: Option<&'static str>,
    ) {
        let Some(status) = self.operation_status.as_mut() else {
            return;
        };
        if status.reference != reference {
            return;
        }
        status.state = state;
        status.code = code;
        status.message = message;
        status.next_action = next_action;
        status.expires_at = Some(std::time::Instant::now() + TERMINAL_STATUS_TTL);
        self.bump_diagnostic_revision();
    }

    pub(crate) fn expire_operation_status(&mut self, now: std::time::Instant) {
        if self
            .operation_status
            .as_ref()
            .is_some_and(|status| status.is_expired(now))
        {
            self.operation_status = None;
            self.bump_diagnostic_revision();
        }
    }

    pub(crate) fn set_unsupported_operation(
        &mut self,
        message: &'static str,
        next_action: &'static str,
    ) {
        let reference = self.start_operation(
            UiOperationKind::ProviderCommand,
            PROVIDER_UNAVAILABLE_CODE,
            message,
        );
        self.complete_operation(
            &reference,
            UiOperationState::Unsupported,
            PROVIDER_UNAVAILABLE_CODE,
            message,
            Some(next_action),
        );
    }

    #[allow(dead_code)]
    pub(crate) fn set_operation_failure(
        &mut self,
        kind: UiOperationKind,
        code: &'static str,
        message: &'static str,
        next_action: &'static str,
    ) {
        let reference = self.start_operation(kind, code, message);
        self.complete_operation(
            &reference,
            UiOperationState::Failed,
            code,
            message,
            Some(next_action),
        );
    }

    pub(crate) fn set_playback_failure(&mut self) {
        let reference = self.start_operation(
            UiOperationKind::Playback,
            PLAYBACK_FAILURE_CODE,
            PLAYBACK_FAILURE_MESSAGE,
        );
        self.complete_operation(
            &reference,
            UiOperationState::Failed,
            PLAYBACK_FAILURE_CODE,
            PLAYBACK_FAILURE_MESSAGE,
            Some(PLAYBACK_FAILURE_NEXT_ACTION),
        );
    }

    pub(crate) fn set_lyrics_status(
        &mut self,
        expected_track_uri: &str,
        expected_source: Option<&str>,
        status: UiViewStatus,
    ) -> bool {
        let matches_track = matches!(
            self.current_page(),
            PageState::Lyrics {
                track_uri,
                lyrics_provider,
                ..
            } if track_uri == expected_track_uri
                && lyrics_provider.as_deref() == expected_source
        );
        if !matches_track {
            return false;
        }
        if let PageState::Lyrics {
            status: page_status,
            ..
        } = self.current_page_mut()
        {
            *page_status = status;
            self.bump_diagnostic_revision();
            true
        } else {
            false
        }
    }

    pub(crate) fn begin_search(&mut self, provider: config::ActiveProvider, query: &str) -> String {
        let superseded = matches!(
            self.current_page(),
            PageState::Search {
                state: SearchPageUIState {
                    search_lifecycle: SearchLifecycle::Loading { .. },
                    ..
                },
                ..
            }
        );
        let message = if superseded {
            SEARCH_REPLACED_MESSAGE
        } else {
            "Searching..."
        };
        let reference = self.start_operation(UiOperationKind::Search, "SEARCH_LOADING", message);
        if let PageState::Search {
            current_query,
            state,
            ..
        } = self.current_page_mut()
        {
            current_query.clear();
            current_query.push_str(query);
            state.category = state.category.filter(|category| {
                crate::command::provider_capabilities(provider)
                    .search_panes()
                    .contains(category)
            });
            state.provider = Some(provider);
            state.pending_lucky = None;
            state.search_lifecycle = SearchLifecycle::Loading {
                reference: reference.clone(),
                superseded,
            };
        }
        self.bump_diagnostic_revision();
        reference
    }

    /// Drop account-scoped Search projections after provider caches are
    /// invalidated. The page and query remain in history so the active page
    /// can issue a fresh request when it is shown again.
    pub(crate) fn invalidate_search_lifecycles(&mut self) {
        let mut has_search_page = false;
        for page in &mut self.history {
            let PageState::Search { state, .. } = page else {
                continue;
            };
            has_search_page = true;
            state.search_lifecycle = SearchLifecycle::Idle;
            state.pending_lucky = None;
            state.search_selection.clear_selection();
            state.track_list.select(None);
            state.video_list.select(None);
            state.album_list.select(None);
            state.artist_list.select(None);
            state.playlist_list.select(None);
            state.show_list.select(None);
            state.episode_list.select(None);
        }
        let had_search_status = self
            .operation_status
            .as_ref()
            .is_some_and(|status| status.kind == UiOperationKind::Search);
        if had_search_status {
            self.operation_status = None;
        }
        if has_search_page || had_search_status {
            self.bump_diagnostic_revision();
        }
    }

    fn search_page_for_lifecycle_mut(
        &mut self,
        provider: config::ActiveProvider,
        query: &str,
        lifecycle_reference: &str,
    ) -> Option<&mut SearchPageUIState> {
        let active_provider = self.active_provider;
        self.history.iter_mut().rev().find_map(|page| {
            let PageState::Search {
                current_query,
                state,
                ..
            } = page
            else {
                return None;
            };
            let is_current_lifecycle = matches!(
                &state.search_lifecycle,
                SearchLifecycle::Loading { reference, .. }
                    if reference == lifecycle_reference
            );
            (current_query == query
                && state.provider.unwrap_or(active_provider) == provider
                && is_current_lifecycle)
                .then_some(state)
        })
    }

    pub(crate) fn finish_search_success(
        &mut self,
        provider: config::ActiveProvider,
        query: &str,
        lifecycle_reference: &str,
        result_count: usize,
    ) {
        let Some(state) = self.search_page_for_lifecycle_mut(provider, query, lifecycle_reference)
        else {
            return;
        };
        if result_count == 0 {
            state.pending_lucky = None;
            state.search_lifecycle = SearchLifecycle::Empty;
        } else {
            state.search_lifecycle = SearchLifecycle::Ready { result_count };
        }
        if result_count == 0 {
            self.complete_operation(
                lifecycle_reference,
                UiOperationState::Completed,
                "SEARCH_EMPTY",
                SEARCH_EMPTY_MESSAGE,
                Some(SEARCH_EMPTY_NEXT_ACTION),
            );
        } else if self
            .operation_status
            .as_ref()
            .is_some_and(|status| status.reference == lifecycle_reference)
        {
            self.operation_status = None;
            self.bump_diagnostic_revision();
        }
        self.bump_diagnostic_revision();
    }

    pub(crate) fn finish_search_failure(
        &mut self,
        provider: config::ActiveProvider,
        query: &str,
        lifecycle_reference: &str,
    ) {
        let Some(state) = self.search_page_for_lifecycle_mut(provider, query, lifecycle_reference)
        else {
            return;
        };
        state.pending_lucky = None;
        state.search_lifecycle = SearchLifecycle::Failed {
            reference: lifecycle_reference.to_owned(),
            code: SEARCH_FAILURE_CODE,
            message: SEARCH_FAILURE_MESSAGE,
            next_action: SEARCH_FAILURE_NEXT_ACTION,
        };
        self.complete_operation(
            lifecycle_reference,
            UiOperationState::Failed,
            SEARCH_FAILURE_CODE,
            SEARCH_FAILURE_MESSAGE,
            Some(SEARCH_FAILURE_NEXT_ACTION),
        );
        self.bump_diagnostic_revision();
    }

    pub(crate) fn finish_search_unavailable(
        &mut self,
        provider: config::ActiveProvider,
        query: &str,
        lifecycle_reference: &str,
    ) {
        let Some(state) = self.search_page_for_lifecycle_mut(provider, query, lifecycle_reference)
        else {
            return;
        };
        state.pending_lucky = None;
        state.search_lifecycle = SearchLifecycle::Failed {
            reference: lifecycle_reference.to_owned(),
            code: SEARCH_UNAVAILABLE_CODE,
            message: SEARCH_UNAVAILABLE_MESSAGE,
            next_action: SEARCH_UNAVAILABLE_NEXT_ACTION,
        };
        self.complete_operation(
            lifecycle_reference,
            UiOperationState::Unsupported,
            SEARCH_UNAVAILABLE_CODE,
            SEARCH_UNAVAILABLE_MESSAGE,
            Some(SEARCH_UNAVAILABLE_NEXT_ACTION),
        );
        self.bump_diagnostic_revision();
    }

    pub(crate) fn finish_search_superseded(
        &mut self,
        provider: config::ActiveProvider,
        query: &str,
        lifecycle_reference: &str,
    ) {
        let Some(state) = self.search_page_for_lifecycle_mut(provider, query, lifecycle_reference)
        else {
            return;
        };
        state.pending_lucky = None;
        state.search_lifecycle = SearchLifecycle::Superseded {
            reference: lifecycle_reference.to_owned(),
        };
        self.complete_operation(
            lifecycle_reference,
            UiOperationState::Superseded,
            "SEARCH_SUPERSEDED",
            "A newer search replaced this request.",
            None,
        );
        self.bump_diagnostic_revision();
    }

    pub(crate) fn arm_search_lucky(
        &mut self,
        provider: config::ActiveProvider,
        query: &str,
        focus: SearchFocusState,
    ) -> bool {
        if focus == SearchFocusState::Input {
            return false;
        }
        let active_provider = self.active_provider;
        let armed = match self.current_page_mut() {
            PageState::Search {
                current_query,
                state,
                ..
            } if current_query == query
                && state.provider.unwrap_or(active_provider) == provider
                && matches!(state.search_lifecycle, SearchLifecycle::Loading { .. }) =>
            {
                state.pending_lucky = Some(SearchLuckyIntent {
                    provider,
                    query: query.to_owned(),
                    focus,
                });
                true
            }
            _ => false,
        };
        if armed {
            self.bump_diagnostic_revision();
        }
        armed
    }

    pub(crate) fn clear_search_lucky(&mut self) {
        let cleared = if let PageState::Search { state, .. } = self.current_page_mut() {
            state.pending_lucky.take().is_some()
        } else {
            false
        };
        if cleared {
            self.bump_diagnostic_revision();
        }
    }

    pub(crate) fn take_ready_search_lucky(&mut self) -> Option<SearchLuckyIntent> {
        let pending = match self.current_page_mut() {
            PageState::Search { state, .. }
                if matches!(state.search_lifecycle, SearchLifecycle::Ready { .. }) =>
            {
                state.pending_lucky.take()
            }
            _ => None,
        }?;

        let matches_page = matches!(
            self.current_page(),
            PageState::Search {
                current_query,
                state,
                ..
            } if current_query == &pending.query
                && state.provider.unwrap_or(self.active_provider) == pending.provider
                && state.focus == pending.focus
        );
        if matches_page {
            self.bump_diagnostic_revision();
            Some(pending)
        } else {
            None
        }
    }

    pub(crate) fn bump_diagnostic_revision(&mut self) {
        self.diagnostic_revision = self.diagnostic_revision.saturating_add(1);
    }

    pub(crate) fn diagnostic_snapshot(
        &self,
        shutdown_requested: bool,
    ) -> crate::observability::UiDiagnosticSnapshot {
        let page = self.current_page();
        let (meaningful_state, loading) = page.diagnostic_content_state();
        crate::observability::UiDiagnosticSnapshot {
            revision: self.diagnostic_revision,
            page: page_label(page.page_type()).to_owned(),
            popup: popup_label(self.popup.as_ref()).to_owned(),
            selection: self
                .popup
                .as_ref()
                .and_then(PopupState::list_selected)
                .or_else(|| page.diagnostic_selection())
                .and_then(|index| index.try_into().ok()),
            loading,
            meaningful_state: meaningful_state.to_owned(),
            provider: match self.active_provider {
                config::ActiveProvider::Spotify => "spotify",
                config::ActiveProvider::YouTubeMusic => "youtube_music",
            }
            .to_owned(),
            lifecycle: if shutdown_requested {
                "shutting_down"
            } else if self.is_running {
                "running"
            } else {
                "stopping"
            }
            .to_owned(),
        }
    }

    /// Return whether there exists a focused popup.
    ///
    /// Currently, only search popup is not focused when it's opened.
    pub fn has_focused_popup(&self) -> bool {
        match self.popup.as_ref() {
            None => false,
            Some(popup) => !matches!(popup, PopupState::Search { .. }),
        }
    }

    /// Return the active page-search query, when the search popup owns one.
    ///
    /// Keeping this lookup in the UI state layer lets renderers distinguish a
    /// real filter from the common unfiltered path without constructing a
    /// temporary projection first.
    pub fn search_query(&self) -> Option<&str> {
        match self.popup.as_ref() {
            Some(PopupState::Search { query }) => Some(query.as_str()),
            _ => None,
        }
    }

    /// Get a list of items possibly filtered by a search query if exists a search popup
    pub fn search_filtered_items<'a, T: std::fmt::Display>(&self, items: &'a [T]) -> Vec<&'a T> {
        match self.search_query().filter(|query| !query.trim().is_empty()) {
            Some(query) => filtered_items_from_query(query, items),
            None => items.iter().collect::<Vec<_>>(),
        }
    }

    /// Project items for the active search while preserving a borrowed
    /// iterator when the page is not filtered.
    pub(crate) fn search_filtered_items_projection<'a, T: std::fmt::Display>(
        &self,
        items: &'a [T],
    ) -> SearchFilteredItems<'a, T> {
        let source = match self.search_query().filter(|query| !query.trim().is_empty()) {
            Some(query) => {
                SearchFilteredItemsSource::Filtered(filtered_items_from_query(query, items))
            }
            None => SearchFilteredItemsSource::All(items),
        };
        SearchFilteredItems { source }
    }

    /// Count items visible under the active search without allocating the
    /// borrowed-item vector used by `search_filtered_items`.
    pub fn search_filtered_item_count<T: std::fmt::Display>(&self, items: &[T]) -> usize {
        match self.search_query().filter(|query| !query.trim().is_empty()) {
            Some(query) => filtered_item_count_from_query(query, items),
            None => items.len(),
        }
    }
}

fn workspace_navigation_for_page(page: &PageState) -> WorkspaceNavigationItem {
    match page {
        PageState::Home { .. } | PageState::HomeShelfList { .. } => WorkspaceNavigationItem::Home,
        PageState::Search { .. } => WorkspaceNavigationItem::Search,
        PageState::Queue { .. } => WorkspaceNavigationItem::Queue,
        PageState::Context {
            context_page_type: ContextPageType::Browsing(context),
            ..
        } => match context {
            ContextId::Playlist(_) => WorkspaceNavigationItem::Playlists,
            ContextId::Album(_) => WorkspaceNavigationItem::Albums,
            ContextId::Artist(_) => WorkspaceNavigationItem::Artists,
            ContextId::Tracks(_) | ContextId::Show(_) => WorkspaceNavigationItem::LikedMusic,
        },
        PageState::YouTubeContext { id, .. } => match id {
            YouTubeContextId::LikedTracks | YouTubeContextId::Podcast(_) => {
                WorkspaceNavigationItem::LikedMusic
            }
            YouTubeContextId::Playlist(_) => WorkspaceNavigationItem::Playlists,
            YouTubeContextId::Album(_) => WorkspaceNavigationItem::Albums,
            YouTubeContextId::Artist(_) => WorkspaceNavigationItem::Artists,
        },
        // Library, Unified playlists and every other page highlight Playlists.
        _ => WorkspaceNavigationItem::Playlists,
    }
}

impl Default for UIState {
    fn default() -> Self {
        // Read the terminal dimensions once so the initial orientation and
        // responsive layout band cannot disagree across a resize boundary.
        let initial_layout = match crossterm::terminal::size() {
            Ok((columns, rows)) => ui::LayoutPolicy::from_size(columns, rows),
            Err(err) => {
                crate::observability::log_safe_error!(
                    warn,
                    crate::observability::DiagnosticCode::TERMINAL_SIZE_FAILED,
                    crate::observability::ErrorCategory::Resource,
                    &err,
                    "Unable to determine the terminal size"
                );
                ui::LayoutPolicy::new(ui::LayoutMode::default(), Orientation::default())
            }
        };

        Self {
            is_running: true,
            active_provider: config::ActiveProvider::Spotify,
            spotify_account_label: None,
            youtube_account_label: None,
            youtube_account_id: None,
            spotify_account_id: None,
            spotify_selection_epoch: 0,
            youtube_selection_epoch: 0,
            spotify_auth_status: config::SpotifyAuthSnapshot::default(),
            welcome_spotify_client_id: crate::auth::NCSPOT_CLIENT_ID.to_owned(),
            welcome_spotify_client_command: false,
            welcome_spotify_client_pending: false,
            welcome_spotify_notice: None,
            welcome_spotify_web_token_cached: false,
            welcome_spotify_library_tested: None,
            welcome_spotify_playback_tested: None,
            welcome_spotify_auth_in_flight: false,
            welcome_spotify_operation: WelcomeOperation::Idle,
            youtube_auth_status: config::YouTubeMusicAuthStatus::default(),
            welcome_youtube_notice: None,
            welcome_youtube_browser: None,
            welcome_youtube_login_active: false,
            welcome_youtube_operation: WelcomeOperation::Idle,
            welcome_listenbrainz_attempt: 0,
            welcome_listenbrainz_pending: None,
            welcome_listenbrainz_notice: None,
            welcome_listenbrainz_username: None,
            welcome_listenbrainz_identity: None,
            welcome_youtube_account_tested: None,
            welcome_youtube_playback_tested: None,
            setup_state: config::SetupState::default(),
            theme: Theme::default(),
            presentation: config::PresentationConfig::default(),
            frame_layout: config::FrameLayoutConfig::default(),
            frame_interval: config::AppConfig::default().ui_frame_interval(),
            terminal_title: config::AppConfig::default().terminal_title,
            terminal_title_idle: config::AppConfig::default().terminal_title_idle,
            spotify_queue_labels: crate::state::SpotifyQueueLabelCache::default(),
            focused_marquee: FocusedMarqueeState::default(),
            input_key_sequence: key::KeySequence { keys: vec![] },
            orientation: initial_layout.orientation,
            layout_mode: initial_layout.mode,

            workspace_focus: WorkspaceFocusState::Context,
            workspace_navigation: WorkspaceNavigationItem::Home,
            workspace_action: WorkspaceAction::OpenSelected,
            workspace_settings_category: SettingsCategory::Preferences,
            workspace_settings_action: SettingsWorkspaceAction::Apply,
            workspace_queue_list: ListState::default(),
            command_help_list_state: ListState::default(),
            command_help_table_state: TableState::default(),
            workspace_layout: ui::WorkspaceLayout::default(),
            workspace_hits: Vec::new(),
            workspace_popup_hits: Vec::new(),
            popup_rect: Rect::default(),
            workspace_last_click: None,
            workspace_pointer: None,
            workspace_hover: None,

            history: vec![PageState::Home {
                state: crate::state::HomePageUIState::default(),
            }],
            popup: None,
            session_history_selection: SelectionState::default(),

            operation_status: None,
            active_bulk_operation: None,
            active_listenbrainz_backup: None,
            active_listenbrainz_sync_check: None,

            playback_progress_bar_rect: Rect::default(),
            playback_window_rect: Rect::default(),
            playback_toggle_rect: Rect::default(),

            count_prefix: None,

            diagnostic_revision: 0,

            next_operation_reference: 0,

            #[cfg(feature = "image")]
            last_cover_image_render_info: ImageRenderInfo::default(),

            // Will be reinitialize later in ui/mod.rs after init_ui()
            #[cfg(feature = "image")]
            picker: Picker::halfblocks(),
        }
    }
}

const fn page_label(page: PageType) -> &'static str {
    match page {
        PageType::Home => "home",
        PageType::HomeShelfList => "home_shelf_list",
        PageType::Welcome => "welcome",
        PageType::Library => "library",
        PageType::Context => "context",
        PageType::YouTubeContext => "youtube_context",
        PageType::UnifiedPlaylist => "unified_playlist",
        PageType::Search => "search",
        PageType::Browse => "browse",
        PageType::Lyrics => "lyrics",
        PageType::Journal => "journal",
        PageType::JournalLists => "journal_lists",
        PageType::JournalList => "journal_list",
        PageType::SessionHistory => "session_history",
        PageType::Queue => "queue",
        PageType::Settings => "settings",
        PageType::CommandHelp => "command_help",
        PageType::Logs => "diagnostics",
    }
}

const fn popup_label(popup: Option<&PopupState>) -> &'static str {
    match popup {
        None => "none",
        Some(PopupState::Search { .. }) => "search",
        Some(
            PopupState::ConfigEdit { .. }
            | PopupState::ConfigChoice { .. }
            | PopupState::ConfigMultiChoice { .. },
        ) => "settings",
        Some(PopupState::ConfirmAction { .. }) => "confirmation",
        Some(PopupState::DiagnosticActions { .. }) => "diagnostic_actions",
        Some(PopupState::DiagnosticDetail { .. }) => "diagnostic_detail",
        #[cfg(feature = "private-capture")]
        Some(PopupState::PrivateDerivativePreview { .. }) => "private_derivative_preview",
        #[cfg(feature = "private-capture")]
        Some(PopupState::PrivateCaptureSelector { .. }) => "private_capture_selector",
        #[cfg(feature = "private-capture")]
        Some(PopupState::PrivateCapturePassphrase { .. }) => "private_capture_passphrase",
        #[cfg(feature = "private-capture")]
        Some(PopupState::PrivateCaptureConfirm { .. }) => "private_capture_confirmation",
        Some(_) => "selection",
    }
}

#[cfg(test)]
mod focused_marquee_state_tests {
    use super::*;
    use std::time::Duration;

    #[test]
    fn phase_is_session_owned_and_resets_for_row_and_mode_changes() {
        let mut ui = UIState::default();
        ui.history = vec![PageState::Welcome {
            state: WelcomePageUIState::new(),
            from_settings: false,
        }];
        ui.presentation.focused_row_overflow = config::FocusedRowOverflow::Marquee;

        let started = Instant::now();
        ui.refresh_focused_marquee_at(started);
        assert_eq!(ui.focused_marquee_phase(), 0);

        ui.refresh_focused_marquee_at(started + Duration::from_millis(240));
        assert_eq!(ui.focused_marquee_phase(), 2);
        assert_eq!(ui.focused_marquee_phase(), 2);

        ui.current_page_mut().select(1);
        ui.refresh_focused_marquee_at(started + Duration::from_millis(360));
        assert_eq!(ui.focused_marquee_phase(), 0);

        ui.refresh_focused_marquee_at(started + Duration::from_millis(480));
        assert_eq!(ui.focused_marquee_phase(), 1);

        ui.presentation.focused_row_overflow = config::FocusedRowOverflow::Truncate;
        ui.refresh_focused_marquee_at(started + Duration::from_millis(600));
        assert_eq!(ui.focused_marquee_phase(), 0);
    }

    #[test]
    fn manual_scrolling_is_bounded_and_resets_when_focus_moves() {
        let mut ui = UIState::default();
        ui.history = vec![PageState::Welcome {
            state: WelcomePageUIState::new(),
            from_settings: false,
        }];
        let started = Instant::now();

        // Other modes leave Left/Right to the rest of the UI.
        ui.set_manual_scroll_extent(3);
        assert!(!ui.scroll_focused_row(true));

        ui.presentation.focused_row_overflow = config::FocusedRowOverflow::Manual;
        ui.refresh_focused_marquee_at(started);
        ui.set_manual_scroll_extent(0);
        assert!(!ui.scroll_focused_row(true), "nothing overflows");

        ui.set_manual_scroll_extent(3);
        for _ in 0..5 {
            assert!(ui.scroll_focused_row(true));
        }
        ui.refresh_focused_marquee_at(started + Duration::from_secs(5));
        assert_eq!(ui.focused_marquee_phase(), 3, "time does not move it");
        assert!(ui.scroll_focused_row(false));
        ui.refresh_focused_marquee_at(started);
        assert_eq!(ui.focused_marquee_phase(), 2);

        ui.current_page_mut().select(1);
        ui.refresh_focused_marquee_at(started);
        assert_eq!(ui.focused_marquee_phase(), 0);
    }

    #[test]
    fn next_phase_is_scheduled_at_the_phase_boundary() {
        let mut ui = UIState::default();
        let started = Instant::now();
        ui.refresh_focused_marquee_at(started);

        assert_eq!(
            ui.until_next_marquee_phase(started + Duration::from_millis(250)),
            Duration::from_millis(110)
        );
        assert_eq!(
            ui.until_next_marquee_phase(started + Duration::from_millis(360)),
            Duration::from_millis(120)
        );
    }
}

#[cfg(test)]
mod frame_interval_tests {
    use super::*;

    #[test]
    fn applying_config_updates_the_terminal_title_templates() {
        let mut ui = UIState::default();
        let mut app = config::AppConfig::default();
        app.terminal_title = "{track}".to_owned();
        app.terminal_title_idle = String::new();

        ui.apply_presentation_config(&app);

        assert_eq!(ui.terminal_title, "{track}");
        assert!(ui.terminal_title_idle.is_empty());
    }

    #[test]
    fn applying_config_updates_the_frame_interval_for_the_render_loop() {
        let mut ui = UIState::default();
        let mut app = config::AppConfig::default();
        app.app_refresh_duration_in_ms = 16;

        ui.apply_presentation_config(&app);

        assert_eq!(ui.frame_interval, std::time::Duration::from_millis(16));
    }
}

#[cfg(test)]
mod search_projection_tests {
    use super::*;

    #[test]
    fn unfiltered_projection_borrows_the_source_and_can_be_reiterated() {
        let ui = UIState::default();
        let items = ["one", "two", "three"];
        let projection = ui.search_filtered_items_projection(&items);

        assert!(matches!(
            &projection.source,
            SearchFilteredItemsSource::All(_)
        ));
        assert_eq!(
            projection.iter().collect::<Vec<_>>(),
            [&items[0], &items[1], &items[2]]
        );
        assert_eq!(projection.iter().count(), items.len());
    }

    #[test]
    fn filtered_projection_preserves_the_matching_items() {
        let mut ui = UIState::default();
        ui.popup = Some(PopupState::Search {
            query: "blue sky".to_owned(),
        });
        let items = ["Blue sky", "Blue ocean", "Green sky"];
        let projection = ui.search_filtered_items_projection(&items);

        assert!(matches!(
            &projection.source,
            SearchFilteredItemsSource::Filtered(_)
        ));
        assert_eq!(
            projection.iter().copied().collect::<Vec<_>>(),
            vec!["Blue sky"]
        );
    }

    #[test]
    fn unfiltered_item_count_uses_the_source_length() {
        let ui = UIState::default();
        let items = ["one", "two", "three"];

        assert_eq!(ui.search_query(), None);
        assert_eq!(ui.search_filtered_item_count(&items), items.len());
    }

    #[test]
    fn filtered_item_count_matches_the_visible_search_projection() {
        let mut ui = UIState::default();
        ui.popup = Some(PopupState::Search {
            query: "blue sky".to_owned(),
        });
        let items = ["Blue sky", "Blue ocean", "Green sky"];

        assert_eq!(ui.search_query(), Some("blue sky"));
        assert_eq!(ui.search_filtered_item_count(&items), 1);
    }

    #[test]
    fn whitespace_only_search_uses_the_unfiltered_count() {
        let mut ui = UIState::default();
        ui.popup = Some(PopupState::Search {
            query: "   ".to_owned(),
        });
        let items = ["one", "two"];

        assert_eq!(ui.search_filtered_item_count(&items), items.len());
    }
}

#[cfg(test)]
mod provider_selection_epoch_tests {
    use super::*;

    #[test]
    fn provider_epochs_are_independent_and_monotonic() {
        let mut ui = UIState::default();
        assert_eq!(
            ui.provider_selection_epoch(config::ActiveProvider::Spotify),
            0
        );
        assert_eq!(
            ui.provider_selection_epoch(config::ActiveProvider::YouTubeMusic),
            0
        );

        assert!(ui.bump_provider_selection_epoch(config::ActiveProvider::Spotify));
        assert!(ui.bump_provider_selection_epoch(config::ActiveProvider::Spotify));
        assert_eq!(
            ui.provider_selection_epoch(config::ActiveProvider::Spotify),
            2
        );
        assert_eq!(
            ui.provider_selection_epoch(config::ActiveProvider::YouTubeMusic),
            0
        );

        assert!(ui.bump_provider_selection_epoch(config::ActiveProvider::YouTubeMusic));
        assert_eq!(
            ui.provider_selection_epoch(config::ActiveProvider::Spotify),
            2
        );
        assert_eq!(
            ui.provider_selection_epoch(config::ActiveProvider::YouTubeMusic),
            1
        );
    }

    #[test]
    fn provider_epoch_exhaustion_is_observable_without_reusing_scope() {
        let mut ui = UIState::default();
        ui.spotify_selection_epoch = u64::MAX;

        assert!(!ui.bump_provider_selection_epoch(config::ActiveProvider::Spotify));
        assert_eq!(
            ui.provider_selection_epoch(config::ActiveProvider::Spotify),
            u64::MAX
        );
        assert_eq!(
            ui.provider_selection_epoch(config::ActiveProvider::YouTubeMusic),
            0
        );
    }
}

#[cfg(test)]
mod workspace_state_tests {
    use super::*;
    use crate::state::TracksId;

    fn liked_tracks_page() -> PageState {
        let context_id = ContextId::Tracks(TracksId {
            uri: "spotify:user:liked".to_owned(),
            kind: "Liked tracks".to_owned(),
        });
        PageState::Context {
            id: Some(context_id.clone()),
            context_page_type: ContextPageType::Browsing(context_id),
            state: None,
        }
    }

    #[test]
    fn opening_a_collection_keeps_the_workspace_and_projects_its_route() {
        let mut ui = UIState::default();

        ui.new_page(liked_tracks_page());

        assert!(ui.workspace_context_is_active());
        assert_eq!(ui.workspace_navigation, WorkspaceNavigationItem::LikedMusic);
        assert_eq!(ui.workspace_focus, WorkspaceFocusState::Context);
    }

    #[test]
    fn opening_search_keeps_the_workspace_and_projects_the_search_route() {
        let mut ui = UIState::default();

        ui.new_page(PageState::Search {
            line_input: crate::ui::single_line_input::LineInput::default(),
            current_query: String::new(),
            state: SearchPageUIState::new(),
        });

        assert_eq!(ui.workspace_navigation, WorkspaceNavigationItem::Search);
        assert_eq!(ui.workspace_focus, WorkspaceFocusState::Context);
    }

    #[test]
    fn opening_browse_uses_the_shared_workspace_shell() {
        let mut ui = UIState::default();

        ui.new_page(PageState::Browse {
            state: BrowsePageUIState::CategoryList {
                state: ListState::default(),
            },
        });

        assert_eq!(ui.workspace_focus, WorkspaceFocusState::Context);
        assert_eq!(ui.current_page().selected_index(), None);
    }

    #[test]
    fn opening_unified_playlist_uses_the_playlists_workspace_route() {
        let mut ui = UIState::default();

        ui.new_page(PageState::new_unified_playlist("unified"));

        assert_eq!(ui.workspace_navigation, WorkspaceNavigationItem::Playlists);
        assert_eq!(ui.workspace_focus, WorkspaceFocusState::Context);
    }

    #[test]
    fn opening_settings_uses_the_workspace_focus_order() {
        let mut ui = UIState::default();
        ui.new_page(PageState::Settings {
            list: ListState::default().with_selected(Some(0)),
            shelves: crate::state::SettingsShelves::default(),
            settings: vec![
                config::AppConfigSetting {
                    section: config::AppConfigSection::Playback,
                    key: "page_size_in_rows".to_owned(),
                    value: "20".to_owned(),
                    kind: config::AppConfigValueKind::Value,
                    restart_required: false,
                },
                config::AppConfigSetting {
                    section: config::AppConfigSection::Accounts,
                    key: "accounts.spotify.status".to_owned(),
                    value: "Ready".to_owned(),
                    kind: config::AppConfigValueKind::Status,
                    restart_required: false,
                },
            ],
            saved: false,
            error: None,
            notice: None,
        });

        assert_eq!(
            ui.workspace_settings_category,
            SettingsCategory::Preferences
        );
        assert_eq!(ui.workspace_focus, WorkspaceFocusState::Context);
        assert!(ui.focus_workspace(true));
        assert_eq!(ui.workspace_focus, WorkspaceFocusState::Actions);
        assert!(ui.focus_workspace(true));
        assert_eq!(ui.workspace_focus, WorkspaceFocusState::Navigation);
        assert!(ui.focus_workspace(true));
        assert_eq!(ui.workspace_focus, WorkspaceFocusState::Context);
    }

    #[test]
    fn tab_focus_cycles_between_navigation_and_context() {
        let mut ui = UIState::default();
        assert!(ui.focus_workspace(true));
        assert_eq!(ui.workspace_focus, WorkspaceFocusState::Navigation);
        assert!(ui.focus_workspace(false));
        assert_eq!(ui.workspace_focus, WorkspaceFocusState::Context);

        ui.new_page(liked_tracks_page());

        assert!(ui.focus_workspace(true));
        assert_eq!(ui.workspace_focus, WorkspaceFocusState::Navigation);
        assert!(ui.focus_workspace(true));
        assert_eq!(ui.workspace_focus, WorkspaceFocusState::Context);
        assert!(ui.focus_workspace(false));
        assert_eq!(ui.workspace_focus, WorkspaceFocusState::Navigation);
    }

    #[test]
    fn wide_collection_focus_visits_queue_and_actions_after_context() {
        let mut ui = UIState::default();
        ui.new_page(liked_tracks_page());
        ui.workspace_layout.show_right = true;

        assert!(ui.focus_workspace(true));
        assert_eq!(ui.workspace_focus, WorkspaceFocusState::Queue);
        assert!(ui.focus_workspace(true));
        assert_eq!(ui.workspace_focus, WorkspaceFocusState::Actions);
        assert!(ui.focus_workspace(false));
        assert_eq!(ui.workspace_focus, WorkspaceFocusState::Queue);
        assert!(ui.focus_workspace(false));
        assert_eq!(ui.workspace_focus, WorkspaceFocusState::Context);
    }

    #[test]
    fn opening_queue_uses_the_workspace_shell_and_route() {
        let mut ui = UIState::default();
        ui.new_page(PageState::new_queue());

        assert_eq!(ui.workspace_navigation, WorkspaceNavigationItem::Queue);
        assert_eq!(ui.workspace_focus, WorkspaceFocusState::Context);
    }

    #[test]
    fn opening_session_history_uses_the_workspace_shell_and_page_cursor() {
        let mut ui = UIState::default();
        ui.new_page(PageState::SessionHistory {
            list: ListState::default(),
        });

        assert_eq!(ui.workspace_focus, WorkspaceFocusState::Context);
        assert_eq!(ui.current_page().selected_index(), None);
        ui.current_page_mut().select(2);
        assert_eq!(ui.current_page().selected_index(), Some(2));
    }

    #[test]
    fn opening_command_help_uses_the_workspace_shell() {
        let mut ui = UIState::default();
        ui.new_page(PageState::CommandHelp { scroll_offset: 0 });

        assert_eq!(ui.workspace_focus, WorkspaceFocusState::Context);
        assert!(matches!(ui.current_page(), PageState::CommandHelp { .. }));
    }

    #[test]
    fn welcome_opens_like_any_workspace_page_with_content_focus() {
        let mut ui = UIState::default();
        ui.workspace_focus = WorkspaceFocusState::Actions;
        ui.new_page(PageState::Welcome {
            state: WelcomePageUIState::new(),
            from_settings: false,
        });

        assert_eq!(ui.workspace_focus, WorkspaceFocusState::Context);
        assert_eq!(ui.welcome_focus(), WorkspaceFocusState::Context);
    }

    #[test]
    fn opening_lyrics_uses_the_workspace_shell_without_rewriting_page_state() {
        let mut ui = UIState::default();
        ui.new_page(PageState::Lyrics {
            provider: config::ActiveProvider::Spotify,
            track_uri: "spotify:track:lyrics".to_owned(),
            track: "Track".to_owned(),
            artists: "Artist".to_owned(),
            youtube_track: None,
            lyrics_provider: None,
            scroll_offset: 0,
            follow_playback: true,
            status: UiViewStatus::Loading,
        });

        assert_eq!(ui.workspace_focus, WorkspaceFocusState::Context);
        assert!(matches!(ui.current_page(), PageState::Lyrics { .. }));
    }

    #[test]
    fn non_route_workspace_pages_do_not_highlight_the_default_route() {
        let mut ui = UIState::default();
        ui.new_page(PageState::Browse {
            state: BrowsePageUIState::CategoryList {
                state: ListState::default(),
            },
        });

        assert!(!ui.workspace_navigation_is_active(WorkspaceNavigationItem::Playlists));
        assert!(!ui.workspace_navigation_is_active(WorkspaceNavigationItem::Queue));

        ui.new_page(PageState::Lyrics {
            provider: config::ActiveProvider::Spotify,
            track_uri: "spotify:track:rail-test".to_owned(),
            track: "Track".to_owned(),
            artists: "Artist".to_owned(),
            youtube_track: None,
            lyrics_provider: None,
            scroll_offset: 0,
            follow_playback: true,
            status: UiViewStatus::Loading,
        });
        assert!(!ui.workspace_navigation_is_active(WorkspaceNavigationItem::Playlists));
    }

    #[test]
    fn workspace_hit_testing_rejects_every_boundary_miss_without_underflow() {
        let mut ui = UIState::default();
        let rect = Rect::new(10, 10, 4, 2);
        ui.workspace_hits.push((
            rect,
            WorkspaceHit::Navigation(WorkspaceNavigationItem::Albums),
        ));

        assert_eq!(
            ui.workspace_hit_at(10, 10),
            Some(WorkspaceHit::Navigation(WorkspaceNavigationItem::Albums))
        );
        assert_eq!(ui.workspace_hit_at(9, 10), None);
        assert_eq!(ui.workspace_hit_at(10, 9), None);
        assert_eq!(ui.workspace_hit_at(14, 10), None);
        assert_eq!(ui.workspace_hit_at(10, 12), None);
    }

    #[test]
    fn workspace_hover_reuses_current_hit_geometry_and_clears_on_miss() {
        let mut ui = UIState::default();
        let rect = Rect::new(10, 10, 4, 2);
        ui.workspace_hits.push((
            rect,
            WorkspaceHit::Navigation(WorkspaceNavigationItem::Albums),
        ));

        ui.set_workspace_pointer(10, 10);
        assert_eq!(ui.workspace_hover_rect(), Some(rect));

        ui.set_workspace_pointer(14, 10);
        assert_eq!(ui.workspace_hover_rect(), None);

        ui.clear_workspace_pointer();
        assert_eq!(ui.workspace_hover_rect(), None);
    }

    #[test]
    fn workspace_hover_uses_the_matching_rect_when_a_hit_is_repeated() {
        let mut ui = UIState::default();
        let first = Rect::new(10, 10, 4, 1);
        let second = Rect::new(30, 20, 6, 1);
        let hit = WorkspaceHit::PlaybackOption(WorkspacePlaybackOption::Volume);
        ui.workspace_hits.push((first, hit));
        ui.workspace_hits.push((second, hit));

        ui.set_workspace_pointer(second.x, second.y);
        assert_eq!(ui.workspace_hover_rect(), Some(second));
    }

    #[test]
    fn returning_home_restores_the_workspace_after_history_pop() {
        let mut ui = UIState::default();
        ui.new_page(liked_tracks_page());
        assert!(ui.workspace_context_is_active());

        ui.history.pop();
        ui.sync_workspace_after_history_change();

        assert!(matches!(ui.current_page(), PageState::Home { .. }));
        assert_eq!(ui.workspace_navigation, WorkspaceNavigationItem::Home);
    }
}

#[cfg(test)]
mod setup_state_tests {
    use super::*;

    #[test]
    fn spotify_review_rejects_changed_busy_and_failed_sessions() {
        for failure in 0..5 {
            let mut ui = UIState::default();
            ui.spotify_auth_status = config::SpotifyAuthSnapshot {
                session_ready: true,
                premium: config::SpotifyPremiumStatus::Premium,
            };
            assert!(ui.setup_auth_snapshot().spotify.ready());
            match failure {
                0 => ui.welcome_spotify_client_pending = true,
                1 => ui.setup_state.spotify_reauthentication_required = true,
                2 => ui.welcome_spotify_library_tested = Some(false),
                3 => ui.welcome_spotify_playback_tested = Some(false),
                _ => ui.welcome_spotify_auth_in_flight = true,
            }
            assert!(!ui.setup_auth_snapshot().spotify.ready());
            assert!(!ui.setup_state.ready_for(ui.setup_auth_snapshot()));
        }
    }

    #[test]
    fn browser_and_oauth_credentials_both_satisfy_youtube_account_readiness() {
        for auth_type in [
            config::YouTubeMusicAuthType::Browser,
            config::YouTubeMusicAuthType::OAuth,
        ] {
            let mut ui = UIState::default();
            ui.youtube_auth_status = config::YouTubeMusicAuthStatus {
                auth_type,
                credential_path: None,
                ready: true,
            };
            assert!(ui.setup_auth_snapshot().youtube.account_ready);
        }
    }

    #[test]
    fn welcome_failed_account_check_blocks_continue_even_with_saved_cookies() {
        let mut ui = UIState::default();
        ui.setup_state.startup_provider = config::ActiveProvider::YouTubeMusic;
        ui.youtube_auth_status.ready = true;
        ui.welcome_youtube_account_tested = Some(false);
        assert!(!ui.setup_state.ready_for(ui.setup_auth_snapshot()));
        ui.welcome_youtube_account_tested = Some(true);
        ui.welcome_youtube_playback_tested = Some(false);
        assert!(ui.setup_state.ready_for(ui.setup_auth_snapshot()));
    }

    #[test]
    fn setup_ui_transitions_cover_pending_ready_skipped_failed_and_cancelled() {
        let mut ui = UIState::default();
        ui.spotify_auth_status = config::SpotifyAuthSnapshot {
            session_ready: true,
            premium: config::SpotifyPremiumStatus::Premium,
        };

        ui.mark_setup_pending();
        assert_eq!(ui.setup_state.status, config::SetupStatus::Pending);

        ui.mark_setup_ready_if_possible();
        assert_eq!(ui.setup_state.status, config::SetupStatus::Ready);

        ui.setup_state.status = config::SetupStatus::Skipped;
        ui.mark_setup_ready_if_possible();
        assert_eq!(ui.setup_state.status, config::SetupStatus::Skipped);

        ui.mark_setup_failed(config::SetupFailure::AuthenticationFailed);
        assert_eq!(ui.setup_state.status, config::SetupStatus::Failed);
        assert_eq!(
            ui.setup_state.failure,
            Some(config::SetupFailure::AuthenticationFailed)
        );

        ui.mark_setup_cancelled();
        assert_eq!(ui.setup_state.status, config::SetupStatus::Pending);
        assert_eq!(ui.setup_state.failure, None);
    }

    #[test]
    fn welcome_spotify_auth_slot_blocks_overlap_until_finished() {
        let mut ui = UIState::default();
        assert!(ui.begin_welcome_spotify_auth());
        assert!(!ui.begin_welcome_spotify_auth());
        assert_eq!(
            ui.welcome_spotify_notice.as_deref(),
            Some(WELCOME_SPOTIFY_AUTH_IN_FLIGHT_NOTICE)
        );
        ui.finish_welcome_spotify_auth();
        assert!(ui.begin_welcome_spotify_auth());
    }

    #[test]
    fn welcome_youtube_action_slot_blocks_overlap_until_finished() {
        let mut ui = UIState::default();
        assert!(ui.begin_welcome_youtube_action(WelcomeOperation::SigningIn));
        assert!(!ui.begin_welcome_youtube_action(WelcomeOperation::Checking));
        assert_eq!(ui.welcome_youtube_operation, WelcomeOperation::Waiting);
        assert!(!ui.begin_welcome_youtube_action(WelcomeOperation::Checking));

        ui.finish_welcome_youtube_action(WelcomeOperation::Idle);
        assert!(ui.begin_welcome_youtube_action(WelcomeOperation::Checking));
    }

    #[test]
    fn setup_revisit_is_a_page_overlay_over_settings() {
        let mut ui = UIState::default();
        ui.new_page(PageState::Settings {
            list: ratatui::widgets::ListState::default(),
            shelves: crate::state::SettingsShelves::default(),
            settings: Vec::new(),
            saved: false,
            error: None,
            notice: None,
        });
        let settings_depth = ui.history.len();

        ui.open_setup_page(true);
        assert!(matches!(
            ui.current_page(),
            PageState::Welcome {
                from_settings: true,
                ..
            }
        ));
        assert_eq!(ui.history.len(), settings_depth + 1);

        ui.history.pop();
        assert!(matches!(ui.current_page(), PageState::Settings { .. }));
        assert_eq!(ui.history.len(), settings_depth);
    }

    #[test]
    fn page_history_is_bounded_and_keeps_the_current_page() {
        let mut ui = UIState::default();
        for index in 0..(MAX_PAGE_HISTORY + 8) {
            ui.new_page(PageState::Lyrics {
                provider: crate::config::ActiveProvider::Spotify,
                track_uri: format!("spotify:track:{index}"),
                track: format!("Track {index}"),
                artists: "Artist".to_owned(),
                youtube_track: None,
                lyrics_provider: None,
                scroll_offset: 0,
                follow_playback: true,
                status: UiViewStatus::Loading,
            });
        }

        assert_eq!(ui.history.len(), MAX_PAGE_HISTORY);
        assert!(matches!(
            ui.current_page(),
            PageState::Lyrics { track_uri, .. } if track_uri.ends_with("71")
        ));
    }
}

#[cfg(test)]
mod operation_state_tests {
    use super::*;

    fn bulk_plan(count: usize) -> crate::command::BulkActionPlan<u64> {
        let items = (0..count)
            .map(|index| {
                let media_id = crate::state::MediaId {
                    provider: crate::state::Provider::Spotify,
                    kind: crate::state::MediaKind::Track,
                    raw_id: format!("track-{index}"),
                };
                crate::command::BulkActionItem::new(
                    index as u64,
                    media_id,
                    [crate::command::BulkActionCapability::new(
                        crate::command::Action::AddToQueue,
                        crate::command::BulkActionOwner::UnifiedQueue,
                    )],
                )
            })
            .collect::<Vec<_>>();
        crate::command::plan_bulk_action(crate::command::Action::AddToQueue, &items)
            .expect("test plan")
    }

    fn search_ui() -> UIState {
        let mut ui = UIState::default();
        ui.new_page(PageState::Search {
            line_input: crate::ui::single_line_input::LineInput::default(),
            current_query: String::new(),
            state: SearchPageUIState::new(),
        });
        ui
    }

    #[test]
    fn search_transitions_distinguish_loading_empty_and_completed() {
        let mut ui = search_ui();
        let reference = ui.begin_search(config::ActiveProvider::Spotify, "private query");
        assert!(matches!(
            ui.current_page(),
            PageState::Search {
                state: SearchPageUIState {
                    search_lifecycle: SearchLifecycle::Loading {
                        superseded: false,
                        ..
                    },
                    ..
                },
                ..
            }
        ));
        assert_eq!(
            ui.current_page().diagnostic_content_state(),
            ("loading", true)
        );
        ui.finish_search_success(
            config::ActiveProvider::Spotify,
            "private query",
            &reference,
            0,
        );
        assert!(matches!(
            ui.current_page(),
            PageState::Search {
                state: SearchPageUIState {
                    search_lifecycle: SearchLifecycle::Empty,
                    ..
                },
                ..
            }
        ));
        assert_eq!(
            ui.current_page().diagnostic_content_state(),
            ("empty", false)
        );
        assert_eq!(
            ui.operation_status.as_ref().map(|status| status.code),
            Some("SEARCH_EMPTY")
        );
        assert_eq!(
            ui.operation_status.as_ref().map(|status| status.message),
            Some(SEARCH_EMPTY_MESSAGE)
        );

        ui.begin_search(config::ActiveProvider::Spotify, "new private query");
        let latest_reference =
            ui.begin_search(config::ActiveProvider::Spotify, "latest private query");
        assert!(matches!(
            ui.current_page(),
            PageState::Search {
                state: SearchPageUIState {
                    search_lifecycle: SearchLifecycle::Loading {
                        superseded: true,
                        ..
                    },
                    ..
                },
                ..
            }
        ));
        ui.finish_search_success(
            config::ActiveProvider::Spotify,
            "latest private query",
            &latest_reference,
            0,
        );
        assert!(matches!(
            ui.current_page(),
            PageState::Search {
                state: SearchPageUIState {
                    search_lifecycle: SearchLifecycle::Empty,
                    ..
                },
                ..
            }
        ));
        let reference = ui.begin_search(config::ActiveProvider::Spotify, "private query");
        ui.finish_search_success(
            config::ActiveProvider::Spotify,
            "private query",
            &reference,
            1,
        );
        assert!(matches!(
            ui.current_page(),
            PageState::Search {
                state: SearchPageUIState {
                    search_lifecycle: SearchLifecycle::Ready { result_count: 1 },
                    ..
                },
                ..
            }
        ));
        assert!(ui.operation_status.is_none());
        assert_eq!(
            ui.current_page().diagnostic_content_state(),
            ("ready", false)
        );
    }

    #[test]
    fn search_completion_updates_its_retained_page_after_navigation() {
        let mut ui = search_ui();
        let reference = ui.begin_search(config::ActiveProvider::Spotify, "private query");
        ui.new_page(PageState::CommandHelp { scroll_offset: 0 });

        ui.finish_search_success(
            config::ActiveProvider::Spotify,
            "private query",
            &reference,
            3,
        );

        let retained_search = ui
            .history
            .iter()
            .find_map(|page| match page {
                PageState::Search {
                    current_query,
                    state,
                    ..
                } if current_query == "private query" => Some(state),
                _ => None,
            })
            .expect("search page remains in history");
        assert_eq!(
            retained_search.search_lifecycle,
            SearchLifecycle::Ready { result_count: 3 }
        );
    }

    #[test]
    fn search_completion_requires_the_exact_foo_bar_foo_invocation() {
        let mut ui = search_ui();
        let first_foo = ui.begin_search(config::ActiveProvider::Spotify, "foo");
        ui.begin_search(config::ActiveProvider::Spotify, "bar");
        let latest_foo = ui.begin_search(config::ActiveProvider::Spotify, "foo");

        ui.finish_search_success(config::ActiveProvider::Spotify, "foo", &first_foo, 1);
        assert!(matches!(
            ui.current_page(),
            PageState::Search {
                state: SearchPageUIState {
                    search_lifecycle: SearchLifecycle::Loading { reference, .. },
                    ..
                },
                ..
            } if reference == &latest_foo
        ));

        ui.finish_search_success(config::ActiveProvider::Spotify, "foo", &latest_foo, 2);
        assert!(matches!(
            ui.current_page(),
            PageState::Search {
                state: SearchPageUIState {
                    search_lifecycle: SearchLifecycle::Ready { result_count: 2 },
                    ..
                },
                ..
            }
        ));
    }

    #[test]
    fn search_failure_and_supersession_are_safe_and_bounded() {
        let mut ui = search_ui();
        let reference = ui.begin_search(config::ActiveProvider::YouTubeMusic, "private query");
        ui.finish_search_failure(
            config::ActiveProvider::YouTubeMusic,
            "private query",
            &reference,
        );
        assert_eq!(
            ui.current_page().diagnostic_content_state(),
            ("failed", false)
        );
        let rendered = ui
            .operation_status
            .as_ref()
            .expect("failure remains visible briefly")
            .display_line();
        assert!(rendered.contains("SEARCH_FAILED"));
        assert!(!rendered.contains("private query"));

        let reference = ui.begin_search(config::ActiveProvider::YouTubeMusic, "recovery query");
        ui.finish_search_success(
            config::ActiveProvider::YouTubeMusic,
            "recovery query",
            &reference,
            2,
        );
        assert!(ui.operation_status.is_none());

        let reference = ui.begin_search(config::ActiveProvider::YouTubeMusic, "newer query");
        ui.finish_search_superseded(
            config::ActiveProvider::YouTubeMusic,
            "newer query",
            &reference,
        );
        assert!(matches!(
            ui.current_page(),
            PageState::Search {
                state: SearchPageUIState {
                    search_lifecycle: SearchLifecycle::Superseded { .. },
                    ..
                },
                ..
            }
        ));

        let reference = ui.begin_search(config::ActiveProvider::YouTubeMusic, "auth query");
        ui.finish_search_unavailable(
            config::ActiveProvider::YouTubeMusic,
            "auth query",
            &reference,
        );
        assert_eq!(
            ui.operation_status.as_ref().map(|status| status.state),
            Some(UiOperationState::Unsupported)
        );
        assert!(!ui
            .operation_status
            .as_ref()
            .expect("unavailable status")
            .display_line()
            .contains("auth query"));
    }

    #[test]
    fn terminal_status_expiry_is_idempotent() {
        let mut ui = search_ui();
        ui.set_unsupported_operation("safe unavailable", "safe next step");
        let now = std::time::Instant::now() + TERMINAL_STATUS_TTL;
        ui.expire_operation_status(now);
        ui.expire_operation_status(now);
        assert!(ui.operation_status.is_none());
    }

    #[test]
    fn lucky_search_intent_waits_for_the_matching_first_result() {
        let mut ui = search_ui();
        let reference = ui.begin_search(config::ActiveProvider::Spotify, "private query");
        if let PageState::Search { state, .. } = ui.current_page_mut() {
            state.focus = SearchFocusState::Tracks;
        }

        assert!(ui.arm_search_lucky(
            config::ActiveProvider::Spotify,
            "private query",
            SearchFocusState::Tracks
        ));
        assert!(ui.take_ready_search_lucky().is_none());

        ui.finish_search_success(
            config::ActiveProvider::Spotify,
            "private query",
            &reference,
            1,
        );
        let intent = ui
            .take_ready_search_lucky()
            .expect("ready search consumes the armed lucky action");
        assert_eq!(intent.query, "private query");
        assert_eq!(intent.focus, SearchFocusState::Tracks);
        assert!(ui.take_ready_search_lucky().is_none());
    }

    #[test]
    fn lucky_search_intent_is_cleared_when_search_is_replaced() {
        let mut ui = search_ui();
        ui.begin_search(config::ActiveProvider::Spotify, "old query");
        if let PageState::Search { state, .. } = ui.current_page_mut() {
            state.focus = SearchFocusState::Tracks;
        }
        assert!(ui.arm_search_lucky(
            config::ActiveProvider::Spotify,
            "old query",
            SearchFocusState::Tracks
        ));

        ui.begin_search(config::ActiveProvider::Spotify, "new query");
        assert!(ui.take_ready_search_lucky().is_none());
    }

    #[test]
    fn mutation_failure_status_is_bounded_and_points_to_diagnostics() {
        let mut ui = search_ui();
        ui.set_operation_failure(
            UiOperationKind::ProviderCommand,
            MUTATION_FAILURE_CODE,
            MUTATION_FAILURE_MESSAGE,
            MUTATION_FAILURE_NEXT_ACTION,
        );
        let status = ui.operation_status.as_ref().expect("failure status");
        assert_eq!(status.state, UiOperationState::Failed);
        assert_eq!(status.code, MUTATION_FAILURE_CODE);
        assert!(status
            .next_action
            .is_some_and(|action| action.contains("Diagnostics")));
        assert!(!status.display_line().contains("raw error"));
    }

    #[test]
    fn lyrics_lifecycle_uses_shared_view_status_and_rejects_stale_targets() {
        let mut ui = UIState::default();
        ui.new_page(PageState::Lyrics {
            provider: crate::config::ActiveProvider::Spotify,
            track_uri: "spotify:track:lyrics".to_owned(),
            track: "Track".to_owned(),
            artists: "Artist".to_owned(),
            youtube_track: None,
            lyrics_provider: None,
            scroll_offset: 0,
            follow_playback: true,
            status: UiViewStatus::Loading,
        });
        assert_eq!(
            ui.current_page().diagnostic_content_state(),
            ("loading", true)
        );
        assert!(!ui.set_lyrics_status("spotify:track:stale", None, UiViewStatus::Ready));
        assert!(ui.set_lyrics_status(
            "spotify:track:lyrics",
            None,
            UiViewStatus::Failed {
                code: LYRICS_FAILURE_CODE,
                message: LYRICS_FAILURE_MESSAGE,
                next_action: LYRICS_FAILURE_NEXT_ACTION,
            }
        ));
        assert_eq!(
            ui.current_page().diagnostic_content_state(),
            ("failed", false)
        );
    }

    #[test]
    fn mutation_status_keeps_one_reference_across_terminal_outcomes() {
        let mut ui = UIState::default();
        let reference = ui.start_operation(
            UiOperationKind::ProviderCommand,
            MUTATION_RUNNING_CODE,
            MUTATION_RUNNING_MESSAGE,
        );
        assert_eq!(
            ui.operation_status.as_ref().map(|status| (
                status.reference.as_str(),
                status.state,
                status.code,
            )),
            Some((
                reference.as_str(),
                UiOperationState::Running,
                MUTATION_RUNNING_CODE,
            ))
        );

        ui.complete_operation(
            &reference,
            UiOperationState::Completed,
            MUTATION_COMPLETED_CODE,
            MUTATION_COMPLETED_MESSAGE,
            None,
        );
        let status = ui.operation_status.as_ref().expect("completed status");
        assert_eq!(status.reference, reference);
        assert_eq!(status.state, UiOperationState::Completed);
        assert_eq!(status.code, MUTATION_COMPLETED_CODE);
    }

    #[test]
    fn listenbrainz_backup_deduplicates_and_rejects_stale_terminals() {
        let mut ui = UIState::default();
        let reference = ui
            .start_listenbrainz_backup("playlist-a")
            .expect("first backup starts");
        assert!(ui.start_listenbrainz_backup("playlist-a").is_none());
        assert!(ui.start_listenbrainz_backup("playlist-b").is_none());
        assert!(!ui.finish_listenbrainz_backup_completed(
            "playlist-b",
            &reference,
            "9ef9f54c-1d4b-4ac4-8b6e-a127c77021f1",
        ));
        assert!(ui.active_listenbrainz_backup.is_some());

        assert!(ui.finish_listenbrainz_backup_partial(
            "playlist-a",
            &reference,
            "9ef9f54c-1d4b-4ac4-8b6e-a127c77021f1",
        ));
        assert!(ui.active_listenbrainz_backup.is_none());
        let status = ui.operation_status.as_ref().expect("partial status");
        assert_eq!(status.state, UiOperationState::Partial);
        assert!(status
            .ordinary_display_line()
            .contains("9ef9f54c-1d4b-4ac4-8b6e-a127c77021f1"));
    }

    #[test]
    fn listenbrainz_sync_check_keeps_its_captured_page_target() {
        let mut ui = UIState::default();
        ui.new_page(PageState::new_unified_playlist("playlist-a"));
        let reference = ui
            .start_listenbrainz_sync_check("playlist-a", ListenBrainzSyncLifecycle::Checking)
            .expect("check starts");
        ui.new_page(PageState::new_unified_playlist("playlist-b"));
        let preview = ListenBrainzSyncPreview {
            playlist_id: "playlist-a".to_owned(),
            operation_reference: reference.clone(),
            rows: Vec::new(),
            conflicts: Vec::new(),
        };

        assert!(!ui.finish_listenbrainz_sync_check(
            "playlist-b",
            &reference,
            ListenBrainzSyncLifecycle::Ready {
                remote_changed: true,
                both_same: false,
                conflicts: 9,
                manifest_only: 9,
            },
            preview.clone(),
        ));
        assert!(ui.finish_listenbrainz_sync_check(
            "playlist-a",
            &reference,
            ListenBrainzSyncLifecycle::Ready {
                remote_changed: true,
                both_same: false,
                conflicts: 2,
                manifest_only: 3,
            },
            preview,
        ));

        assert!(matches!(
            &ui.history[1],
            PageState::UnifiedPlaylist {
                id,
                listenbrainz_sync: ListenBrainzSyncLifecycle::Ready {
                    remote_changed: true,
                    both_same: false,
                    conflicts: 2,
                    manifest_only: 3,
                },
                ..
            } if id == "playlist-a"
        ));
        assert!(matches!(
            ui.current_page(),
            PageState::UnifiedPlaylist {
                id,
                listenbrainz_sync: ListenBrainzSyncLifecycle::Idle,
                ..
            } if id == "playlist-b"
        ));
    }

    #[test]
    fn listenbrainz_sync_apply_finishes_idle_with_details_and_rejects_late_terminals() {
        let mut ui = UIState::default();
        ui.new_page(PageState::new_unified_playlist("playlist-a"));
        let reference = ui
            .start_listenbrainz_sync_check(
                "playlist-a",
                ListenBrainzSyncLifecycle::WritingListenBrainz,
            )
            .expect("apply starts");
        assert!(ui.listenbrainz_sync_check_is_active("playlist-a", &reference));
        assert!(!ui.listenbrainz_sync_check_is_active("playlist-a", "ui-0000"));
        assert!(ui.finish_listenbrainz_sync_applied(
            "playlist-a",
            &reference,
            LISTENBRAINZ_SYNC_APPLY_PUSH_COMPLETED_MESSAGE,
            "Pushed 2 native rows.".to_owned(),
        ));
        assert!(!ui.listenbrainz_sync_check_is_active("playlist-a", &reference));
        // Late terminals fail closed without touching the idle page.
        assert!(!ui.finish_listenbrainz_sync_applied(
            "playlist-a",
            &reference,
            LISTENBRAINZ_SYNC_APPLY_PUSH_COMPLETED_MESSAGE,
            "Pushed 2 native rows.".to_owned(),
        ));
        assert!(!ui.finish_listenbrainz_sync_apply_failed(
            "playlist-a",
            &reference,
            LISTENBRAINZ_SYNC_APPLY_FAILED_MESSAGE,
            LISTENBRAINZ_SYNC_APPLY_FAILED_NEXT_ACTION,
        ));
        assert!(matches!(
            ui.current_page(),
            PageState::UnifiedPlaylist {
                listenbrainz_sync: ListenBrainzSyncLifecycle::Idle,
                ..
            }
        ));
        let status = ui.operation_status.as_ref().expect("completed status");
        assert_eq!(status.state, UiOperationState::Completed);
        assert_eq!(status.code, LISTENBRAINZ_SYNC_APPLY_COMPLETED_CODE);
        assert!(status
            .details
            .as_deref()
            .is_some_and(|details| details.contains("Pushed 2 native rows")));
    }

    #[test]
    fn listenbrainz_sync_cancellation_is_idle_before_write_and_verifying_after_write() {
        let mut ui = UIState::default();
        ui.new_page(PageState::new_unified_playlist("playlist"));
        ui.start_listenbrainz_sync_check("playlist", ListenBrainzSyncLifecycle::Planning)
            .expect("planning starts");
        assert!(ui.cancel_listenbrainz_sync_check("playlist"));
        assert!(matches!(
            ui.current_page(),
            PageState::UnifiedPlaylist {
                listenbrainz_sync: ListenBrainzSyncLifecycle::Idle,
                ..
            }
        ));
        assert_eq!(
            ui.operation_status.as_ref().map(|status| status.state),
            Some(UiOperationState::Cancelled)
        );

        ui.start_listenbrainz_sync_check(
            "playlist",
            ListenBrainzSyncLifecycle::WritingListenBrainz,
        )
        .expect("write stage starts");
        assert!(!ui.cancel_listenbrainz_sync_check("playlist"));
        assert!(matches!(
            ui.current_page(),
            PageState::UnifiedPlaylist {
                listenbrainz_sync: ListenBrainzSyncLifecycle::Verifying,
                ..
            }
        ));
        assert!(ui.active_listenbrainz_sync_check.is_some());
    }

    #[test]
    fn bulk_runtime_has_one_active_tracker_and_rejects_late_terminals() {
        let mut ui = UIState::default();
        let plan = bulk_plan(2);
        let handle = ui.start_bulk_operation(&plan).expect("start bulk");
        assert!(ui.active_bulk_operation().is_some());
        assert_eq!(
            ui.operation_status.as_ref().map(|status| status.kind),
            Some(UiOperationKind::BulkAction)
        );
        assert_eq!(
            ui.operation_status
                .as_ref()
                .and_then(|status| status.bulk_summary)
                .map(|summary| (summary.selected_occurrences, summary.pending)),
            Some((2, 2))
        );
        assert_eq!(
            ui.start_bulk_operation(&plan),
            Err(BulkOperationRuntimeError::ActiveOperation)
        );

        let ids = handle.operation_ids().to_vec();
        ui.record_bulk_terminal(
            handle.reference(),
            &ids[..1],
            crate::command::BulkOperationTerminal::Succeeded,
        )
        .expect("first terminal");
        assert!(ui.active_bulk_operation().is_some());
        assert_eq!(
            ui.operation_status.as_ref().map(|status| status.state),
            Some(UiOperationState::Running)
        );
        assert_eq!(
            ui.record_bulk_terminal(
                handle.reference(),
                &[crate::command::BulkOperationId::from_index(99)],
                crate::command::BulkOperationTerminal::Succeeded,
            ),
            Err(BulkOperationRuntimeError::UnknownOperation {
                operation_id: crate::command::BulkOperationId::from_index(99),
            })
        );
        assert_eq!(
            ui.record_bulk_terminal(
                handle.reference(),
                &[ids[1], ids[1]],
                crate::command::BulkOperationTerminal::Succeeded,
            ),
            Err(BulkOperationRuntimeError::DuplicateOperationId {
                operation_id: ids[1]
            })
        );

        ui.record_bulk_terminal(
            handle.reference(),
            &ids[1..],
            crate::command::BulkOperationTerminal::Failed(
                crate::command::BulkOperationFailure::RequestFailed,
            ),
        )
        .expect("second terminal");
        assert!(ui.active_bulk_operation().is_none());
        assert_eq!(
            ui.operation_status.as_ref().map(|status| status.state),
            Some(UiOperationState::Partial)
        );
        assert_eq!(
            ui.record_bulk_terminal(
                handle.reference(),
                &ids[1..],
                crate::command::BulkOperationTerminal::Succeeded,
            ),
            Err(BulkOperationRuntimeError::UnknownReference)
        );
    }

    #[test]
    fn bulk_terminal_does_not_replace_a_newer_ui_status() {
        let mut ui = UIState::default();
        let plan = bulk_plan(1);
        let handle = ui.start_bulk_operation(&plan).expect("start bulk");
        let newer_reference = ui.start_operation(
            UiOperationKind::Search,
            "SEARCH_RUNNING",
            "Search is running.",
        );

        ui.record_bulk_terminal(
            handle.reference(),
            handle.operation_ids(),
            crate::command::BulkOperationTerminal::Succeeded,
        )
        .expect("bulk terminal");

        assert!(ui.active_bulk_operation().is_none());
        assert_eq!(
            ui.operation_status
                .as_ref()
                .map(|status| status.reference.as_str()),
            Some(newer_reference.as_str())
        );
        assert_eq!(
            ui.operation_status.as_ref().map(|status| status.kind),
            Some(UiOperationKind::Search)
        );
    }
}
