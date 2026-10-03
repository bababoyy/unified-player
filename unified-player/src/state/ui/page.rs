use super::{
    journal_selection::JournalSelection,
    mutable_playlist::{MutablePlaylistSelection, MutablePlaylistState, ProviderOccurrenceToken},
    operation::{SearchLifecycle, UiViewStatus},
    queue_selection::QueueSelection,
    youtube_context_selection::YouTubeContextSelection,
    ContextTrackPane, ContextTrackSelection, MultiSelectModel, SearchSelection, SelectionChange,
    ShelfNav, ShelfSize,
};
use crate::{
    config::{ActiveProvider, AppConfigSection, AppConfigSetting},
    state::model::{Category, ContextId, PlaylistEntryId, YouTubeContext, YouTubeContextId},
    ui::single_line_input::LineInput,
};
use ratatui::widgets::{ListState, TableState};

pub(crate) fn settings_filter_projection<'a>(
    settings: &'a [AppConfigSetting],
    query: Option<&str>,
) -> Vec<(usize, &'a AppConfigSetting)> {
    let terms = query
        .unwrap_or_default()
        .split_whitespace()
        .map(str::to_lowercase)
        .collect::<Vec<_>>();

    settings
        .iter()
        .enumerate()
        .filter(|(_, setting)| {
            if terms.is_empty() {
                return true;
            }
            let searchable = format!(
                "{} {} {} {} {}",
                setting.section.title(),
                crate::config::setting_label(&setting.key),
                crate::config::setting_description(&setting.key),
                setting.key,
                setting.value,
            )
            .to_lowercase();
            terms.iter().all(|term| searchable.contains(term))
        })
        .collect()
}

/// The settings `category` shows for `query`, as source indices grouped into
/// one shelf per section, in the order sections first appear.
pub(crate) fn settings_tile_shelves(
    settings: &[AppConfigSetting],
    category: SettingsCategory,
    query: Option<&str>,
) -> Vec<(AppConfigSection, Vec<usize>)> {
    let mut shelves: Vec<(AppConfigSection, Vec<usize>)> = Vec::new();
    for (source, setting) in settings_filter_projection(settings, query) {
        if !category.includes(setting) {
            continue;
        }
        match shelves
            .iter_mut()
            .find(|(section, _)| *section == setting.section)
        {
            Some((_, sources)) => sources.push(source),
            None => shelves.push((setting.section, vec![source])),
        }
    }
    shelves
}

/// Shelf sizes for [`ShelfNav`] from [`settings_tile_shelves`].
pub(crate) fn settings_shelf_sizes(
    shelves: &[(AppConfigSection, Vec<usize>)],
) -> Vec<ShelfSize<AppConfigSection>> {
    shelves
        .iter()
        .map(|(section, sources)| ShelfSize {
            key: *section,
            len: sources.len(),
        })
        .collect()
}

/// Tile navigation memory for Settings, one per category.
pub type SettingsShelves = [ShelfNav<AppConfigSection>; 2];

pub(crate) const SETTINGS_RELOAD_ERROR_MESSAGE: &str = "Settings could not be reloaded. Try again.";
pub(crate) const SETTINGS_SAVE_ERROR_MESSAGE: &str = "The setting could not be saved. Try again.";
pub(crate) const YOUTUBE_CONTEXT_ERROR_MESSAGE: &str = "Unable to load this context.";
pub(crate) const YOUTUBE_CONTEXT_ERROR_CODE: &str = "YOUTUBE_CONTEXT_LOAD_FAILED";
pub(crate) const YOUTUBE_CONTEXT_ERROR_NEXT_ACTION: &str = "Try again or choose another item.";
pub(crate) const YOUTUBE_CONTEXT_REFRESH_PARTIAL_CODE: &str = "YOUTUBE_CONTEXT_REFRESH_FAILED";
pub(crate) const YOUTUBE_CONTEXT_REFRESH_PARTIAL_MESSAGE: &str =
    "The remote change succeeded, but this view could not be refreshed.";
pub(crate) const YOUTUBE_CONTEXT_REFRESH_PARTIAL_NEXT_ACTION: &str =
    "Retry the context to verify the latest items.";
pub(crate) const CONTEXT_ERROR_MESSAGE: &str =
    "Unable to load this Spotify context. Try again or choose another item.";
pub(crate) const CONTEXT_ERROR_CODE: &str = "CONTEXT_LOAD_FAILED";
pub(crate) const CONTEXT_ERROR_NEXT_ACTION: &str = "Check the connection and try again.";
pub(crate) const YOUTUBE_LIBRARY_ERROR_MESSAGE: &str =
    "Unable to load the YouTube Music library. Try again or check Diagnostics.";
pub(crate) const UNIFIED_PLAYLIST_ERROR_MESSAGE: &str =
    "This Unified playlist is no longer available.";
pub(crate) const UNIFIED_PLAYLIST_ERROR_CODE: &str = "UNIFIED_PLAYLIST_MISSING";
pub(crate) const UNIFIED_PLAYLIST_ERROR_NEXT_ACTION: &str =
    "Return to the previous page and choose another playlist.";
pub(crate) const LYRICS_PLAYBACK_UNAVAILABLE_MESSAGE: &str =
    "Playback position is unavailable for synced lyrics.";
pub(crate) const LYRICS_PLAYBACK_UNAVAILABLE_CODE: &str = "LYRICS_PLAYBACK_UNAVAILABLE";
pub(crate) const LYRICS_PLAYBACK_UNAVAILABLE_NEXT_ACTION: &str =
    "Start playback or switch to plain lyrics.";

#[derive(Clone, Debug, PartialEq)]
pub enum PageState {
    Home {
        state: crate::state::HomePageUIState,
    },
    /// The full list behind a Home shelf's "Show all".
    HomeShelfList {
        shelf: crate::state::HomeShelfKind,
        list: ListState,
    },
    Welcome {
        state: WelcomePageUIState,
        from_settings: bool,
    },
    Library {
        state: LibraryPageUIState,
    },
    Context {
        id: Option<ContextId>,
        context_page_type: ContextPageType,
        state: Option<ContextPageUIState>,
    },
    YouTubeContext {
        id: YouTubeContextId,
        context: Option<YouTubeContext>,
        state: YouTubeContextPageUIState,
    },
    UnifiedPlaylist {
        id: String,
        playlist_state: MutablePlaylistState,
        listenbrainz_sync: super::ListenBrainzSyncLifecycle,
        listenbrainz_preview: Option<super::ListenBrainzSyncPreview>,
    },
    Search {
        line_input: LineInput,
        current_query: String,
        state: SearchPageUIState,
    },
    Lyrics {
        provider: ActiveProvider,
        track_uri: String,
        track: String,
        artists: String,
        youtube_track: Option<crate::state::YouTubeTrack>,
        lyrics_provider: Option<String>,
        scroll_offset: usize,
        follow_playback: bool,
        status: UiViewStatus,
    },
    Journal {
        table: TableState,
        journal_selection: JournalSelection,
    },
    JournalLists {
        list: ListState,
    },
    JournalList {
        list_id: String,
        table: TableState,
        journal_selection: JournalSelection,
    },
    SessionHistory {
        list: ListState,
    },
    Browse {
        state: BrowsePageUIState,
    },
    Queue {
        table: TableState,
        queue_selection: QueueSelection,
    },
    Settings {
        list: ListState,
        shelves: SettingsShelves,
        settings: Vec<AppConfigSetting>,
        saved: bool,
        error: Option<String>,
        notice: Option<String>,
    },
    CommandHelp {
        scroll_offset: usize,
    },
    Logs {
        state: DiagnosticsPageUIState,
    },
}

/// A page-owned view over the one selectable adapter that belongs to the
/// currently focused row surface. Rendering and event code can use this
/// erased bridge without knowing which provider-specific selection type owns
/// the keys underneath.
#[derive(Debug)]
pub enum PageSelectionAdapter<'a> {
    Search(&'a mut SearchSelection),
    Context(&'a mut ContextTrackSelection),
    YouTubeContext(&'a mut YouTubeContextSelection),
    Journal(&'a mut JournalSelection),
    Queue(&'a mut QueueSelection),
    MutablePlaylist(&'a mut MutablePlaylistSelection),
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct PageSelectionError;

impl MultiSelectModel for PageSelectionAdapter<'_> {
    type Error = PageSelectionError;

    fn select_all_visible(&mut self) -> Result<SelectionChange, Self::Error> {
        match self {
            Self::Search(selection) => selection
                .select_all_visible()
                .map_err(|_| PageSelectionError),
            Self::Context(selection) => selection
                .select_all_visible()
                .map_err(|_| PageSelectionError),
            Self::YouTubeContext(selection) => selection
                .select_all_visible()
                .map_err(|_| PageSelectionError),
            Self::Journal(selection) => selection
                .select_all_visible()
                .map_err(|_| PageSelectionError),
            Self::Queue(selection) => selection
                .select_all_visible()
                .map_err(|_| PageSelectionError),
            Self::MutablePlaylist(selection) => selection
                .select_all_visible()
                .map_err(|_| PageSelectionError),
        }
    }

    fn invert_visible(&mut self) -> Result<SelectionChange, Self::Error> {
        match self {
            Self::Search(selection) => selection.invert_visible().map_err(|_| PageSelectionError),
            Self::Context(selection) => selection.invert_visible().map_err(|_| PageSelectionError),
            Self::YouTubeContext(selection) => {
                selection.invert_visible().map_err(|_| PageSelectionError)
            }
            Self::Journal(selection) => selection.invert_visible().map_err(|_| PageSelectionError),
            Self::Queue(selection) => selection.invert_visible().map_err(|_| PageSelectionError),
            Self::MutablePlaylist(selection) => {
                selection.invert_visible().map_err(|_| PageSelectionError)
            }
        }
    }

    fn extend_visible_range(
        &mut self,
        cursor: usize,
        target: usize,
    ) -> Result<SelectionChange, Self::Error> {
        match self {
            Self::Search(selection) => selection
                .extend_visible_range(cursor, target)
                .map_err(|_| PageSelectionError),
            Self::Context(selection) => selection
                .extend_visible_range(cursor, target)
                .map_err(|_| PageSelectionError),
            Self::YouTubeContext(selection) => selection
                .extend_visible_range(cursor, target)
                .map_err(|_| PageSelectionError),
            Self::Journal(selection) => selection
                .extend_visible_range(cursor, target)
                .map_err(|_| PageSelectionError),
            Self::Queue(selection) => selection
                .extend_visible_range(cursor, target)
                .map_err(|_| PageSelectionError),
            Self::MutablePlaylist(selection) => selection
                .extend_visible_range(cursor, target)
                .map_err(|_| PageSelectionError),
        }
    }

    fn clear_selection(&mut self) -> SelectionChange {
        match self {
            Self::Search(selection) => selection.clear_selection(),
            Self::Context(selection) => selection.clear_selection(),
            Self::YouTubeContext(selection) => selection.clear_selection(),
            Self::Journal(selection) => selection.clear_selection(),
            Self::Queue(selection) => selection.clear_selection(),
            Self::MutablePlaylist(selection) => selection.clear_selection(),
        }
    }

    fn selected_visible_indices(&self) -> Vec<usize> {
        match self {
            Self::Search(selection) => selection.selected_visible_indices(),
            Self::Context(selection) => selection.selected_visible_indices(),
            Self::YouTubeContext(selection) => selection.selected_visible_indices(),
            Self::Journal(selection) => selection.selected_visible_indices(),
            Self::Queue(selection) => selection.selected_visible_indices(),
            Self::MutablePlaylist(selection) => selection.selected_visible_indices(),
        }
    }
}

#[derive(PartialEq, Eq, Clone, Copy)]
pub enum PageType {
    Home,
    HomeShelfList,
    Welcome,
    Library,
    Context,
    YouTubeContext,
    UnifiedPlaylist,
    Search,
    Browse,
    Lyrics,
    Journal,
    JournalLists,
    JournalList,
    SessionHistory,
    Queue,
    Settings,
    CommandHelp,
    Logs,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum WelcomeStep {
    #[default]
    Preferences,
    Spotify,
    YouTube,
    ListenBrainz,
    Review,
}

impl WelcomeStep {
    const PREFERENCES_ACTIONS: [WelcomeAction; 3] = [
        WelcomeAction::PreferencesStartupProvider,
        WelcomeAction::PreferencesStartPaused,
        WelcomeAction::PreferencesNext,
    ];
    const SPOTIFY_ACTIONS: [WelcomeAction; 6] = [
        WelcomeAction::SpotifyBundledClient,
        WelcomeAction::SpotifyCustomClient,
        WelcomeAction::SpotifySignIn,
        WelcomeAction::SpotifyCheckSession,
        WelcomeAction::SpotifyBack,
        WelcomeAction::SpotifyNext,
    ];
    const YOUTUBE_ACTIONS: [WelcomeAction; 7] = [
        WelcomeAction::YouTubeSignIn,
        WelcomeAction::YouTubeTestAccount,
        WelcomeAction::YouTubeChooseBrowser,
        WelcomeAction::YouTubeDetectBrowser,
        WelcomeAction::YouTubeImportCookies,
        WelcomeAction::YouTubeBack,
        WelcomeAction::YouTubeNext,
    ];
    const REVIEW_ACTIONS: [WelcomeAction; 5] = [
        WelcomeAction::ReviewFixSpotify,
        WelcomeAction::ReviewFixYouTube,
        WelcomeAction::ReviewBack,
        WelcomeAction::ReviewContinue,
        WelcomeAction::ReviewSkip,
    ];
    const LISTENBRAINZ_ACTIONS: [WelcomeAction; 5] = [
        WelcomeAction::ListenBrainzEnterToken,
        WelcomeAction::ListenBrainzCheckToken,
        WelcomeAction::ListenBrainzFetchPlaylists,
        WelcomeAction::ListenBrainzBack,
        WelcomeAction::ListenBrainzNext,
    ];
    pub const ALL: [Self; 5] = [
        Self::Preferences,
        Self::Spotify,
        Self::YouTube,
        Self::ListenBrainz,
        Self::Review,
    ];

    pub const fn index(self) -> usize {
        match self {
            Self::Preferences => 0,
            Self::Spotify => 1,
            Self::YouTube => 2,
            Self::ListenBrainz => 3,
            Self::Review => 4,
        }
    }

    pub const fn title(self) -> &'static str {
        match self {
            Self::Preferences => "Preferences",
            Self::Spotify => "Spotify",
            Self::YouTube => "YouTube",
            Self::ListenBrainz => "ListenBrainz (optional)",
            Self::Review => "Review",
        }
    }

    pub const fn row_count(self) -> usize {
        self.actions().len()
    }

    /// Number of step-specific actions. They precede the navigation buttons,
    /// so `0..task_count()` is the content pane and the rest is the button row.
    pub const fn task_count(self) -> usize {
        let actions = self.actions();
        let mut count = 0;
        while count < actions.len() && !actions[count].is_navigation() {
            count += 1;
        }
        count
    }

    /// The single source of truth for Welcome's selectable rows. Renderers,
    /// live events, pointer geometry, and the offline demo all project this
    /// same ordered list instead of repeating numeric row meanings.
    pub const fn actions(self) -> &'static [WelcomeAction] {
        match self {
            Self::Preferences => &Self::PREFERENCES_ACTIONS,
            Self::Spotify => &Self::SPOTIFY_ACTIONS,
            Self::YouTube => &Self::YOUTUBE_ACTIONS,
            Self::ListenBrainz => &Self::LISTENBRAINZ_ACTIONS,
            Self::Review => &Self::REVIEW_ACTIONS,
        }
    }

    pub const fn action_at(self, index: usize) -> Option<WelcomeAction> {
        if index < self.actions().len() {
            Some(self.actions()[index])
        } else {
            None
        }
    }

    pub const fn previous(self) -> Option<Self> {
        match self {
            Self::Preferences => None,
            Self::Spotify => Some(Self::Preferences),
            Self::YouTube => Some(Self::Spotify),
            Self::ListenBrainz => Some(Self::YouTube),
            Self::Review => Some(Self::ListenBrainz),
        }
    }

    pub const fn next(self) -> Option<Self> {
        match self {
            Self::Preferences => Some(Self::Spotify),
            Self::Spotify => Some(Self::YouTube),
            Self::YouTube => Some(Self::ListenBrainz),
            Self::ListenBrainz => Some(Self::Review),
            Self::Review => None,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum WelcomeAction {
    PreferencesStartupProvider,
    PreferencesStartPaused,
    PreferencesNext,
    SpotifyBundledClient,
    SpotifyCustomClient,
    SpotifySignIn,
    SpotifyCheckSession,
    SpotifyBack,
    SpotifyNext,
    YouTubeSignIn,
    YouTubeTestAccount,
    YouTubeChooseBrowser,
    YouTubeDetectBrowser,
    YouTubeImportCookies,
    YouTubeBack,
    YouTubeNext,
    ListenBrainzEnterToken,
    ListenBrainzCheckToken,
    ListenBrainzFetchPlaylists,
    ListenBrainzBack,
    ListenBrainzNext,
    ReviewFixSpotify,
    ReviewFixYouTube,
    ReviewBack,
    ReviewContinue,
    ReviewSkip,
}

impl WelcomeAction {
    pub const fn step(self) -> WelcomeStep {
        match self {
            Self::PreferencesStartupProvider
            | Self::PreferencesStartPaused
            | Self::PreferencesNext => WelcomeStep::Preferences,
            Self::SpotifyBundledClient
            | Self::SpotifyCustomClient
            | Self::SpotifySignIn
            | Self::SpotifyCheckSession
            | Self::SpotifyBack
            | Self::SpotifyNext => WelcomeStep::Spotify,
            Self::YouTubeSignIn
            | Self::YouTubeTestAccount
            | Self::YouTubeChooseBrowser
            | Self::YouTubeDetectBrowser
            | Self::YouTubeImportCookies
            | Self::YouTubeBack
            | Self::YouTubeNext => WelcomeStep::YouTube,
            Self::ListenBrainzEnterToken
            | Self::ListenBrainzCheckToken
            | Self::ListenBrainzFetchPlaylists
            | Self::ListenBrainzBack
            | Self::ListenBrainzNext => WelcomeStep::ListenBrainz,
            Self::ReviewFixSpotify
            | Self::ReviewFixYouTube
            | Self::ReviewBack
            | Self::ReviewContinue
            | Self::ReviewSkip => WelcomeStep::Review,
        }
    }

    /// The button that advances the wizard; it takes focus when the button
    /// row is entered.
    pub const fn is_forward(self) -> bool {
        matches!(
            self,
            Self::PreferencesNext
                | Self::SpotifyNext
                | Self::YouTubeNext
                | Self::ListenBrainzNext
                | Self::ReviewContinue
        )
    }

    pub const fn is_navigation(self) -> bool {
        matches!(
            self,
            Self::PreferencesNext
                | Self::SpotifyBack
                | Self::SpotifyNext
                | Self::YouTubeBack
                | Self::YouTubeNext
                | Self::ListenBrainzBack
                | Self::ListenBrainzNext
                | Self::ReviewBack
                | Self::ReviewContinue
                | Self::ReviewSkip
        )
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct WelcomePageUIState {
    pub step: WelcomeStep,
    pub list: ListState,
    pub layout: WelcomeLayout,
    /// Whether the last frame drew the step rail; focus skips it otherwise.
    pub rail_visible: bool,
    /// The content-pane row restored when focus returns from another pane.
    pub task_selection: usize,
}

/// Movement inside the focused Welcome pane.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum WelcomeMove {
    By(isize),
    First,
    Last,
}

impl WelcomeMove {
    fn apply(self, current: usize, len: usize) -> usize {
        let last = len.saturating_sub(1);
        match self {
            Self::By(delta) => current.saturating_add_signed(delta).min(last),
            Self::First => 0,
            Self::Last => last,
        }
    }
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum WelcomeLayout {
    Classic,
    Centered,
    #[default]
    Sidebar,
}

impl WelcomePageUIState {
    pub fn new() -> Self {
        let mut list = ListState::default();
        list.select(Some(0));
        Self {
            step: WelcomeStep::default(),
            list,
            layout: WelcomeLayout::default(),
            rail_visible: false,
            task_selection: 0,
        }
    }

    pub fn show_step(&mut self, step: WelcomeStep) {
        self.step = step;
        self.list.select(Some(0));
        *self.list.offset_mut() = 0;
        self.task_selection = 0;
    }

    /// Move the selection inside `focus`: the rail changes step, the content
    /// pane and the button row move within their own rows.
    pub fn move_in(&mut self, focus: WorkspaceFocusState, movement: WelcomeMove) {
        let selected = self.list.selected().unwrap_or(0);
        let tasks = self.step.task_count();
        match focus {
            WorkspaceFocusState::Navigation => {
                let index = movement.apply(self.step.index(), WelcomeStep::ALL.len());
                if index != self.step.index() {
                    self.show_step(WelcomeStep::ALL[index]);
                }
            }
            WorkspaceFocusState::Actions => {
                let buttons = self.step.row_count() - tasks;
                let local = movement.apply(selected.saturating_sub(tasks), buttons);
                self.list.select(Some(tasks + local));
            }
            WorkspaceFocusState::Context | WorkspaceFocusState::Queue => {
                let index = movement.apply(selected.min(tasks.saturating_sub(1)), tasks);
                self.list.select(Some(index));
                self.task_selection = index;
            }
        }
    }

    /// Select the row `focus` should land on when it is entered.
    pub fn enter(&mut self, focus: WorkspaceFocusState) {
        let selected = self.list.selected().unwrap_or(0);
        let tasks = self.step.task_count();
        if selected < tasks {
            self.task_selection = selected;
        }
        match focus {
            WorkspaceFocusState::Actions => {
                let forward = self
                    .step
                    .actions()
                    .iter()
                    .position(|action| action.is_forward())
                    .unwrap_or(tasks);
                self.list.select(Some(forward));
            }
            WorkspaceFocusState::Context | WorkspaceFocusState::Queue => {
                self.list
                    .select(Some(self.task_selection.min(tasks.saturating_sub(1))));
            }
            WorkspaceFocusState::Navigation => {}
        }
    }

    /// The pane that owns the action at `index`.
    pub fn pane_of(&self, index: usize) -> WorkspaceFocusState {
        if index < self.step.task_count() {
            WorkspaceFocusState::Context
        } else {
            WorkspaceFocusState::Actions
        }
    }
}

impl Default for WelcomePageUIState {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod welcome_step_tests {
    use super::*;

    #[test]
    fn wizard_steps_have_bounded_rows_and_bidirectional_order() {
        let mut state = WelcomePageUIState::new();
        assert_eq!(state.step, WelcomeStep::Preferences);
        assert_eq!(state.step.row_count(), 3);

        for step in WelcomeStep::ALL {
            assert!(step.actions().iter().all(|action| action.step() == step));
        }
        assert_eq!(
            WelcomeStep::Spotify.actions(),
            &[
                WelcomeAction::SpotifyBundledClient,
                WelcomeAction::SpotifyCustomClient,
                WelcomeAction::SpotifySignIn,
                WelcomeAction::SpotifyCheckSession,
                WelcomeAction::SpotifyBack,
                WelcomeAction::SpotifyNext,
            ]
        );
        assert!(WelcomeAction::SpotifyBack.is_navigation());
        assert!(!WelcomeAction::SpotifySignIn.is_navigation());

        state.list.select(Some(2));
        *state.list.offset_mut() = 2;
        state.show_step(state.step.next().unwrap());
        assert_eq!(state.step, WelcomeStep::Spotify);
        assert_eq!(state.step.row_count(), 6);
        assert_eq!(state.list.selected(), Some(0));
        assert_eq!(state.list.offset(), 0);

        assert_eq!(state.step.next(), Some(WelcomeStep::YouTube));
        assert_eq!(state.step.previous(), Some(WelcomeStep::Preferences));
        assert_eq!(WelcomeStep::Review.next(), None);
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct LibraryPageUIState {
    pub playlist_list: ListState,
    pub saved_album_list: ListState,
    pub followed_artist_list: ListState,
    pub focus: LibraryFocusState,
    pub playlist_folder_id: usize,
}

#[derive(Clone, Debug, PartialEq)]
pub struct SearchPageUIState {
    pub track_list: ListState,
    pub search_selection: SearchSelection,
    pub video_list: ListState,
    pub album_list: ListState,
    pub artist_list: ListState,
    pub playlist_list: ListState,
    pub show_list: ListState,
    pub episode_list: ListState,
    pub focus: SearchFocusState,
    /// The category recipe currently selected in the Search header. `None`
    /// is the six-panel overview (`All`); provider panes expand to one typed
    /// result view without discarding the other panes' cursors.
    pub category: Option<crate::command::ProviderSearchPane>,
    pub provider: Option<ActiveProvider>,
    pub search_lifecycle: SearchLifecycle,
    /// A user-confirmed first-result action waiting for the current search.
    pub pending_lucky: Option<SearchLuckyIntent>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SearchLuckyIntent {
    pub provider: ActiveProvider,
    pub query: String,
    pub focus: SearchFocusState,
}

#[derive(Clone, Debug, PartialEq)]
pub struct YouTubeContextPageUIState {
    pub track_list: TableState,
    pub youtube_context_selection: YouTubeContextSelection,
    pub mutable_playlist: MutablePlaylistState,
    pub status: UiViewStatus,
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct DiagnosticsPageUIState {
    pub list: ListState,
    pub selected_row: Option<crate::observability::DiagnosticRowId>,
    pub follow_reference: Option<String>,
    pub support_focus_reference: Option<String>,
}

impl DiagnosticsPageUIState {
    pub fn new() -> Self {
        let mut state = Self::default();
        state.list.select(Some(0));
        state
    }

    pub(crate) fn synchronize(&mut self, rows: &[crate::observability::DiagnosticRow]) {
        if rows.is_empty() {
            self.list.select(None);
            self.selected_row = None;
            return;
        }
        let old_index = self.list.selected().unwrap_or_default();
        let index = self
            .selected_row
            .as_ref()
            .and_then(|id| rows.iter().position(|row| &row.id == id))
            .unwrap_or_else(|| old_index.min(rows.len() - 1));
        self.list.select(Some(index));
        self.selected_row = Some(rows[index].id.clone());
    }

    pub(crate) fn select_index(
        &mut self,
        rows: &[crate::observability::DiagnosticRow],
        index: usize,
    ) {
        if let Some(row) = rows.get(index) {
            self.list.select(Some(index));
            self.selected_row = Some(row.id.clone());
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub enum ContextPageType {
    CurrentPlaying,
    Browsing(ContextId),
}

#[derive(Clone, Debug, PartialEq)]
pub enum ContextPageUIState {
    Failed {
        status: UiViewStatus,
    },
    Playlist {
        playlist_state: MutablePlaylistState,
    },
    Album {
        track_table: TableState,
        track_selection: ContextTrackSelection,
    },
    Artist {
        top_track_table: TableState,
        top_track_selection: ContextTrackSelection,
        listenbrainz_pending: Option<ListenBrainzPendingIntent>,
        listenbrainz_album_pending: Option<ListenBrainzAlbumPendingIntent>,
        album_table: TableState,
        related_artist_list: ListState,
        liked_track_table: TableState,
        liked_track_selection: ContextTrackSelection,
        focus: ArtistFocusState,
    },
    Tracks {
        track_table: TableState,
        track_selection: ContextTrackSelection,
    },
    Show {
        episode_table: TableState,
    },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ListenBrainzRecordingIntent {
    Play,
    OpenMenu,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ListenBrainzPendingIntent {
    pub request_id: u64,
    pub context_uri: String,
    pub recording_mbid: String,
    pub intent: ListenBrainzRecordingIntent,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ListenBrainzAlbumIntent {
    OpenPage,
    OpenMenu,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ListenBrainzAlbumPendingIntent {
    pub request_id: u64,
    pub context_uri: String,
    pub release_group_mbid: String,
    pub intent: ListenBrainzAlbumIntent,
}

#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum LibraryFocusState {
    Playlists,
    SavedAlbums,
    FollowedArtists,
}

/// The two user-facing Settings categories in the design-v1 workspace.
/// Provider-specific and shared settings remain real persisted settings; the
/// rail only groups them into the two stable navigation concepts.
#[derive(Copy, Clone, Debug, Default, PartialEq, Eq)]
pub enum SettingsCategory {
    #[default]
    Preferences,
    Accounts,
}

// `next`/`previous` list every variant so a new one must choose its neighbours.
#[allow(clippy::match_same_arms)]
impl SettingsCategory {
    pub const fn label(self) -> &'static str {
        match self {
            Self::Preferences => "Preferences",
            Self::Accounts => "Accounts",
        }
    }

    /// Position in [`SettingsShelves`].
    pub const fn index(self) -> usize {
        match self {
            Self::Preferences => 0,
            Self::Accounts => 1,
        }
    }

    pub const fn next(self) -> Self {
        match self {
            Self::Preferences => Self::Accounts,
            Self::Accounts => Self::Accounts,
        }
    }

    pub const fn previous(self) -> Self {
        match self {
            Self::Preferences => Self::Preferences,
            Self::Accounts => Self::Preferences,
        }
    }

    pub const fn includes(self, setting: &crate::config::AppConfigSetting) -> bool {
        match self {
            Self::Preferences => {
                !matches!(setting.section, crate::config::AppConfigSection::Accounts)
            }
            Self::Accounts => matches!(setting.section, crate::config::AppConfigSection::Accounts),
        }
    }
}

/// The passive category-rail target used by Settings mouse navigation.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum SettingsRailItem {
    Category(SettingsCategory),
    BackToPlayer,
}

/// The small action bar rendered below the Settings list.
#[derive(Copy, Clone, Debug, Default, PartialEq, Eq)]
pub enum SettingsWorkspaceAction {
    #[default]
    Apply,
    Discard,
}

// `next`/`previous` list every variant so a new one must choose its neighbours.
#[allow(clippy::match_same_arms)]
impl SettingsWorkspaceAction {
    pub const fn label(self) -> &'static str {
        match self {
            Self::Apply => "Apply changes",
            Self::Discard => "Discard",
        }
    }

    pub const fn next(self) -> Self {
        match self {
            Self::Apply => Self::Discard,
            Self::Discard => Self::Discard,
        }
    }

    pub const fn previous(self) -> Self {
        match self {
            Self::Apply => Self::Apply,
            Self::Discard => Self::Apply,
        }
    }
}

/// Keyboard focus for the persistent Library/Context/Queue composition.
#[derive(Copy, Clone, Debug, Default, PartialEq, Eq)]
pub enum WorkspaceFocusState {
    Navigation,
    #[default]
    Context,
    Queue,
    Actions,
}

/// The small action surface rendered beside a wide collection context.
#[derive(Copy, Clone, Debug, Default, PartialEq, Eq)]
pub enum WorkspaceAction {
    #[default]
    OpenSelected,
    AddToQueue,
    MoreActions,
}

impl WorkspaceAction {
    pub const ALL: [Self; 3] = [Self::OpenSelected, Self::AddToQueue, Self::MoreActions];

    pub const fn index(self) -> usize {
        match self {
            Self::OpenSelected => 0,
            Self::AddToQueue => 1,
            Self::MoreActions => 2,
        }
    }

    pub const fn label(self) -> &'static str {
        match self {
            Self::OpenSelected => "Play selected",
            Self::AddToQueue => "Add to queue",
            Self::MoreActions => "More actions",
        }
    }

    pub const fn command(self) -> crate::command::Command {
        match self {
            Self::OpenSelected => crate::command::Command::ChooseSelected,
            Self::AddToQueue => crate::command::Command::AddSelectedItemToQueue,
            Self::MoreActions => crate::command::Command::ShowActionsOnSelectedItem,
        }
    }

    pub const fn next(self) -> Option<Self> {
        let index = self.index();
        if index + 1 < Self::ALL.len() {
            Some(Self::ALL[index + 1])
        } else {
            None
        }
    }

    pub const fn previous(self) -> Option<Self> {
        let index = self.index();
        if index > 0 {
            Some(Self::ALL[index - 1])
        } else {
            None
        }
    }
}

/// The route entries rendered by the v1 persistent Library navigation.
#[derive(Copy, Clone, Debug, Default, PartialEq, Eq)]
pub enum WorkspaceNavigationItem {
    #[default]
    Home,
    Playlists,
    LikedMusic,
    Albums,
    Artists,
    Search,
    Queue,
}

impl WorkspaceNavigationItem {
    pub const ALL: [Self; 7] = [
        Self::Home,
        Self::Playlists,
        Self::LikedMusic,
        Self::Albums,
        Self::Artists,
        Self::Search,
        Self::Queue,
    ];

    pub const fn label(self) -> &'static str {
        match self {
            Self::Home => "Home",
            Self::LikedMusic => "Liked Music",
            Self::Playlists => "Playlists",
            Self::Albums => "Albums",
            Self::Artists => "Artists",
            Self::Search => "Search",
            Self::Queue => "Queue",
        }
    }

    pub const fn index(self) -> usize {
        match self {
            Self::Home => 0,
            Self::Playlists => 1,
            Self::LikedMusic => 2,
            Self::Albums => 3,
            Self::Artists => 4,
            Self::Search => 5,
            Self::Queue => 6,
        }
    }

    pub const fn next(self) -> Option<Self> {
        if self.index() + 1 < Self::ALL.len() {
            Some(Self::ALL[self.index() + 1])
        } else {
            None
        }
    }

    pub const fn previous(self) -> Option<Self> {
        if self.index() > 0 {
            Some(Self::ALL[self.index() - 1])
        } else {
            None
        }
    }

    pub const fn first() -> Self {
        Self::ALL[0]
    }

    pub const fn last() -> Self {
        Self::ALL[Self::ALL.len() - 1]
    }
}

#[cfg(test)]
mod workspace_navigation_tests {
    use super::{WorkspaceHit, WorkspaceNavigationItem, WorkspaceRailItem, WorkspaceScopeKind};

    #[test]
    fn home_leads_the_rail_ahead_of_the_library_routes() {
        assert_eq!(
            WorkspaceNavigationItem::default(),
            WorkspaceNavigationItem::Home
        );
        assert_eq!(
            WorkspaceNavigationItem::ALL,
            [
                WorkspaceNavigationItem::Home,
                WorkspaceNavigationItem::Playlists,
                WorkspaceNavigationItem::LikedMusic,
                WorkspaceNavigationItem::Albums,
                WorkspaceNavigationItem::Artists,
                WorkspaceNavigationItem::Search,
                WorkspaceNavigationItem::Queue,
            ]
        );
        assert_eq!(WorkspaceNavigationItem::Home.index(), 0);
        assert_eq!(WorkspaceNavigationItem::Playlists.index(), 1);
        assert_eq!(
            WorkspaceNavigationItem::first(),
            WorkspaceNavigationItem::Home
        );
        assert_eq!(
            WorkspaceNavigationItem::last(),
            WorkspaceNavigationItem::Queue
        );
        assert_eq!(
            WorkspaceNavigationItem::Playlists.next(),
            Some(WorkspaceNavigationItem::LikedMusic)
        );
        assert_eq!(
            WorkspaceNavigationItem::Queue.previous(),
            Some(WorkspaceNavigationItem::Search)
        );
    }

    #[test]
    fn rail_roles_keep_routes_and_scopes_distinct() {
        assert_eq!(
            WorkspaceRailItem::Route(WorkspaceNavigationItem::Queue).hit(),
            WorkspaceHit::Navigation(WorkspaceNavigationItem::Queue)
        );
        assert_eq!(
            WorkspaceRailItem::Scope(WorkspaceScopeKind::Playback).hit(),
            WorkspaceHit::Scope(WorkspaceScopeKind::Playback)
        );
        assert_eq!(WorkspaceScopeKind::ALL[1].title(), "Account");
    }
}

#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum WorkspaceScopeKind {
    Browsing,
    Account,
    Playback,
}

impl WorkspaceScopeKind {
    pub const ALL: [Self; 3] = [Self::Browsing, Self::Account, Self::Playback];

    pub const fn index(self) -> usize {
        match self {
            Self::Browsing => 0,
            Self::Account => 1,
            Self::Playback => 2,
        }
    }

    pub const fn title(self) -> &'static str {
        match self {
            Self::Browsing => "Browsing",
            Self::Account => "Account",
            Self::Playback => "Playback",
        }
    }
}

#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum WorkspaceHit {
    /// A Home card, by its shelf and position in that shelf.
    HomeCard {
        shelf: crate::state::HomeShelfKind,
        index: usize,
    },
    /// A row of a Home "Show all" list.
    HomeListRow(usize),
    /// A step in the Welcome rail.
    WelcomeStep(WelcomeStep),
    /// A Welcome action row or button, by its index in the step's actions.
    WelcomeAction(usize),
    /// The Welcome editor popup's input line and its two buttons.
    WelcomeEditorInput,
    WelcomeEditorConfirm,
    WelcomeEditorCancel,
    CloseWindow,
    Help,
    PlaybackOption(WorkspacePlaybackOption),
    /// The volume symbol shown instead of the slider on narrow playback rows.
    VolumeMenu,
    Navigation(WorkspaceNavigationItem),
    Scope(WorkspaceScopeKind),
    SettingsRail(SettingsRailItem),
    SettingsRow(usize),
    SettingsAction(SettingsWorkspaceAction),
    SearchCategory(Option<crate::command::ProviderSearchPane>),
    SearchInput,
    SearchRow {
        focus: SearchFocusState,
        index: usize,
    },
    BrowseRow(usize),
    UnifiedPlaylistRow(usize),
    LibraryRow {
        focus: LibraryFocusState,
        index: usize,
    },
    ContextRow(usize),
    /// A row (or section title) on the artist page; clicking moves focus to its section.
    ArtistRow {
        focus: ArtistFocusState,
        index: usize,
    },
    QueueRow(usize),
    JournalRow(usize),
    SessionHistoryRow(usize),
    DiagnosticRow(usize),
    Action(WorkspaceAction),
}

/// One entry in the persistent workspace rail.
///
/// Route rows and provider/account scope controls share geometry and focus
/// ownership, but retain distinct roles and hit variants. This keeps the
/// visible rail model explicit instead of making scope controls look like
/// routes to keyboard or mouse dispatch.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum WorkspaceRailItem {
    Route(WorkspaceNavigationItem),
    Scope(WorkspaceScopeKind),
}

impl WorkspaceRailItem {
    pub const fn hit(self) -> WorkspaceHit {
        match self {
            Self::Route(item) => WorkspaceHit::Navigation(item),
            Self::Scope(kind) => WorkspaceHit::Scope(kind),
        }
    }
}

#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum WorkspacePlaybackOption {
    Shuffle,
    Repeat,
    Volume,
}

#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum ArtistFocusState {
    TopTracks,
    Albums,
    RelatedArtists,
    LikedSongs,
}

#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum SearchFocusState {
    Category,
    Input,
    Tracks,
    Videos,
    Albums,
    Artists,
    Playlists,
    Shows,
    Episodes,
}

impl SearchFocusState {
    fn provider_pane(self) -> Option<crate::command::ProviderSearchPane> {
        Some(match self {
            Self::Tracks => crate::command::ProviderSearchPane::Tracks,
            Self::Videos => crate::command::ProviderSearchPane::Videos,
            Self::Albums => crate::command::ProviderSearchPane::Albums,
            Self::Artists => crate::command::ProviderSearchPane::Artists,
            Self::Playlists => crate::command::ProviderSearchPane::Playlists,
            Self::Shows => crate::command::ProviderSearchPane::Shows,
            Self::Episodes => crate::command::ProviderSearchPane::Episodes,
            Self::Category | Self::Input => return None,
        })
    }

    pub(crate) fn from_provider_pane(pane: crate::command::ProviderSearchPane) -> Self {
        match pane {
            crate::command::ProviderSearchPane::Tracks => Self::Tracks,
            crate::command::ProviderSearchPane::Videos => Self::Videos,
            crate::command::ProviderSearchPane::Albums => Self::Albums,
            crate::command::ProviderSearchPane::Artists => Self::Artists,
            crate::command::ProviderSearchPane::Playlists => Self::Playlists,
            crate::command::ProviderSearchPane::Shows => Self::Shows,
            crate::command::ProviderSearchPane::Episodes => Self::Episodes,
        }
    }

    pub fn next_for_provider(&mut self, provider: ActiveProvider) {
        let panes = crate::command::provider_capabilities(provider).search_panes();
        if *self == Self::Category {
            *self = Self::Input;
            return;
        }
        let next = match self.provider_pane() {
            None => panes.first().copied().map(Self::from_provider_pane),
            Some(current) => match panes.iter().position(|pane| *pane == current) {
                Some(index) if index + 1 < panes.len() => {
                    Some(Self::from_provider_pane(panes[index + 1]))
                }
                Some(_) => Some(Self::Input),
                None => panes.first().copied().map(Self::from_provider_pane),
            },
        };
        *self = next.unwrap_or(Self::Input);
    }

    pub fn previous_for_provider(&mut self, provider: ActiveProvider) {
        let panes = crate::command::provider_capabilities(provider).search_panes();
        if *self == Self::Category {
            *self = panes
                .last()
                .copied()
                .map_or(Self::Input, Self::from_provider_pane);
            return;
        }
        let previous = match self.provider_pane() {
            None => panes.last().copied().map(Self::from_provider_pane),
            Some(current) => match panes.iter().position(|pane| *pane == current) {
                Some(index) if index > 0 => Some(Self::from_provider_pane(panes[index - 1])),
                Some(_) => Some(Self::Input),
                None => panes.last().copied().map(Self::from_provider_pane),
            },
        };
        *self = previous.unwrap_or(Self::Input);
    }
}

#[cfg(test)]
mod search_focus_tests {
    use super::SearchFocusState;
    use crate::config::ActiveProvider;

    #[test]
    fn youtube_search_focus_visits_every_native_section() {
        let mut focus = SearchFocusState::Input;
        let mut visited = Vec::new();
        for _ in 0..8 {
            visited.push(focus);
            focus.next_for_provider(ActiveProvider::YouTubeMusic);
        }
        assert_eq!(
            visited,
            vec![
                SearchFocusState::Input,
                SearchFocusState::Tracks,
                SearchFocusState::Videos,
                SearchFocusState::Albums,
                SearchFocusState::Artists,
                SearchFocusState::Playlists,
                SearchFocusState::Shows,
                SearchFocusState::Episodes,
            ]
        );
        assert_eq!(focus, SearchFocusState::Input);
    }

    #[test]
    fn spotify_search_focus_skips_youtube_video_section() {
        let mut focus = SearchFocusState::Tracks;
        focus.next_for_provider(ActiveProvider::Spotify);
        assert_eq!(focus, SearchFocusState::Albums);
        focus.previous_for_provider(ActiveProvider::Spotify);
        assert_eq!(focus, SearchFocusState::Tracks);
    }

    #[test]
    fn provider_focus_normalizes_a_pane_not_supported_by_the_target_provider() {
        let mut focus = SearchFocusState::Videos;
        focus.next_for_provider(ActiveProvider::Spotify);
        assert_eq!(focus, SearchFocusState::Tracks);

        let mut focus = SearchFocusState::Videos;
        focus.previous_for_provider(ActiveProvider::Spotify);
        assert_eq!(focus, SearchFocusState::Episodes);
    }

    #[test]
    fn category_focus_sits_before_query_focus() {
        let mut focus = SearchFocusState::Category;
        focus.next_for_provider(ActiveProvider::Spotify);
        assert_eq!(focus, SearchFocusState::Input);

        let mut focus = SearchFocusState::Category;
        focus.previous_for_provider(ActiveProvider::Spotify);
        assert_eq!(focus, SearchFocusState::Episodes);
    }
}

#[derive(Clone, Debug, PartialEq)]
pub enum BrowsePageUIState {
    CategoryList {
        state: ListState,
    },
    CategoryPlaylistList {
        category: Category,
        state: ListState,
    },
}

pub enum MutableWindowState<'a> {
    Table(&'a mut TableState),
    List(&'a mut ListState),
    Scroll(&'a mut usize),
}

impl PageState {
    /// The type of the page.
    pub fn page_type(&self) -> PageType {
        match self {
            PageState::Welcome { .. } => PageType::Welcome,
            PageState::Home { .. } => PageType::Home,
            PageState::HomeShelfList { .. } => PageType::HomeShelfList,
            PageState::Library { .. } => PageType::Library,
            PageState::Context { .. } => PageType::Context,
            PageState::YouTubeContext { .. } => PageType::YouTubeContext,
            PageState::UnifiedPlaylist { .. } => PageType::UnifiedPlaylist,
            PageState::Search { .. } => PageType::Search,
            PageState::Browse { .. } => PageType::Browse,
            PageState::Lyrics { .. } => PageType::Lyrics,
            PageState::Journal { .. } => PageType::Journal,
            PageState::JournalLists { .. } => PageType::JournalLists,
            PageState::JournalList { .. } => PageType::JournalList,
            PageState::SessionHistory { .. } => PageType::SessionHistory,
            PageState::Queue { .. } => PageType::Queue,
            PageState::Settings { .. } => PageType::Settings,
            PageState::CommandHelp { .. } => PageType::CommandHelp,
            PageState::Logs { .. } => PageType::Logs,
        }
    }

    pub(crate) const fn marquee_focus_label(&self) -> &'static str {
        match self {
            Self::Home { .. } => "home",
            Self::HomeShelfList { .. } => "home_shelf_list",
            Self::Welcome { state, .. } => match state.step {
                WelcomeStep::Preferences => "preferences",
                WelcomeStep::Spotify => "spotify",
                WelcomeStep::YouTube => "youtube",
                WelcomeStep::ListenBrainz => "listenbrainz",
                WelcomeStep::Review => "review",
            },
            Self::Library { state } => match state.focus {
                LibraryFocusState::Playlists => "playlists",
                LibraryFocusState::SavedAlbums => "albums",
                LibraryFocusState::FollowedArtists => "artists",
            },
            Self::Search { state, .. } => match state.focus {
                SearchFocusState::Category => "category",
                SearchFocusState::Input => "input",
                SearchFocusState::Tracks => "tracks",
                SearchFocusState::Videos => "videos",
                SearchFocusState::Albums => "albums",
                SearchFocusState::Artists => "artists",
                SearchFocusState::Playlists => "playlists",
                SearchFocusState::Shows => "shows",
                SearchFocusState::Episodes => "episodes",
            },
            Self::Context { state, .. } => match state {
                Some(ContextPageUIState::Artist { focus, .. }) => match focus {
                    ArtistFocusState::TopTracks => "artist_top_tracks",
                    ArtistFocusState::Albums => "artist_albums",
                    ArtistFocusState::RelatedArtists => "artist_related",
                    ArtistFocusState::LikedSongs => "artist_liked",
                },
                Some(ContextPageUIState::Playlist { .. }) => "playlist",
                Some(ContextPageUIState::Album { .. }) => "album",
                Some(ContextPageUIState::Tracks { .. }) => "tracks",
                Some(ContextPageUIState::Show { .. }) => "show",
                Some(ContextPageUIState::Failed { .. }) => "failed",
                None => "loading",
            },
            Self::YouTubeContext { id, .. } => match id {
                YouTubeContextId::Playlist(_) => "playlist",
                YouTubeContextId::Album(_) => "album",
                YouTubeContextId::Artist(_) => "artist",
                YouTubeContextId::LikedTracks => "liked_tracks",
                YouTubeContextId::Podcast(_) => "podcast",
            },
            Self::Browse { state } => match state {
                BrowsePageUIState::CategoryList { .. } => "categories",
                BrowsePageUIState::CategoryPlaylistList { .. } => "category_playlists",
            },
            Self::UnifiedPlaylist { .. } => "playlist",
            Self::Lyrics { .. } => "lyrics",
            Self::Journal { .. } => "journal",
            Self::JournalLists { .. } => "journal_lists",
            Self::JournalList { .. } => "journal_list",
            Self::SessionHistory { .. } => "session_history",
            Self::Queue { .. } => "queue",
            Self::Settings { .. } => "settings",
            Self::CommandHelp { .. } => "commands",
            Self::Logs { .. } => "diagnostics",
        }
    }

    pub(crate) fn diagnostic_selection(&self) -> Option<usize> {
        match self {
            Self::Home { state } => Some(state.selected(state.focus)),
            Self::Welcome { state, .. } => state.list.selected(),
            Self::HomeShelfList { list, .. }
            | Self::JournalLists { list }
            | Self::Settings { list, .. }
            | Self::SessionHistory { list } => list.selected(),
            Self::Library { state } => match state.focus {
                LibraryFocusState::Playlists => state.playlist_list.selected(),
                LibraryFocusState::SavedAlbums => state.saved_album_list.selected(),
                LibraryFocusState::FollowedArtists => state.followed_artist_list.selected(),
            },
            Self::Search { state, .. } => match state.focus {
                SearchFocusState::Category | SearchFocusState::Input => None,
                SearchFocusState::Tracks => state.track_list.selected(),
                SearchFocusState::Videos => state.video_list.selected(),
                SearchFocusState::Albums => state.album_list.selected(),
                SearchFocusState::Artists => state.artist_list.selected(),
                SearchFocusState::Playlists => state.playlist_list.selected(),
                SearchFocusState::Shows => state.show_list.selected(),
                SearchFocusState::Episodes => state.episode_list.selected(),
            },
            Self::Context { state, .. } => state.as_ref().and_then(|state| match state {
                ContextPageUIState::Playlist { playlist_state } => {
                    playlist_state.table().selected()
                }
                ContextPageUIState::Album { track_table, .. }
                | ContextPageUIState::Tracks { track_table, .. } => track_table.selected(),
                ContextPageUIState::Artist {
                    top_track_table,
                    album_table,
                    related_artist_list,
                    liked_track_table,
                    focus,
                    ..
                } => match focus {
                    ArtistFocusState::TopTracks => top_track_table.selected(),
                    ArtistFocusState::Albums => album_table.selected(),
                    ArtistFocusState::RelatedArtists => related_artist_list.selected(),
                    ArtistFocusState::LikedSongs => liked_track_table.selected(),
                },
                ContextPageUIState::Show { episode_table } => episode_table.selected(),
                ContextPageUIState::Failed { .. } => None,
            }),
            Self::YouTubeContext {
                id: YouTubeContextId::Playlist(_),
                state,
                ..
            } => state.mutable_playlist.table().selected(),
            Self::YouTubeContext { state, .. } => state.track_list.selected(),
            Self::UnifiedPlaylist { playlist_state, .. } => playlist_state.table().selected(),
            Self::Browse { state } => match state {
                BrowsePageUIState::CategoryList { state }
                | BrowsePageUIState::CategoryPlaylistList { state, .. } => state.selected(),
            },
            Self::Journal { table, .. }
            | Self::JournalList { table, .. }
            | Self::Queue { table, .. } => table.selected(),
            Self::Logs { state } => state.list.selected(),
            Self::CommandHelp { scroll_offset } | Self::Lyrics { scroll_offset, .. } => {
                Some(*scroll_offset)
            }
        }
    }

    pub(crate) fn diagnostic_content_state(&self) -> (&'static str, bool) {
        match self {
            Self::Context { state: None, .. } => ("loading", true),
            Self::Context {
                state: Some(ContextPageUIState::Failed { status }),
                ..
            } => (status.label(), false),
            Self::Search { state, .. } => {
                let status = state.search_lifecycle.view_status();
                (status.label(), status == UiViewStatus::Loading)
            }
            Self::YouTubeContext { context, state, .. } => {
                if context.is_some() {
                    match state.status {
                        UiViewStatus::Loading => ("loading", true),
                        UiViewStatus::Empty => ("empty", false),
                        UiViewStatus::Ready => ("ready", false),
                        UiViewStatus::Idle => ("idle", false),
                        UiViewStatus::Partial { .. }
                        | UiViewStatus::Failed { .. }
                        | UiViewStatus::Unsupported { .. }
                        | UiViewStatus::Superseded { .. } => (state.status.label(), false),
                    }
                } else {
                    (state.status.label(), state.status == UiViewStatus::Loading)
                }
            }
            Self::Lyrics { status, .. } => (status.label(), *status == UiViewStatus::Loading),
            Self::Settings { error: Some(_), .. } => ("failed", false),
            Self::Settings {
                saved: true,
                error: None,
                ..
            }
            | Self::Settings {
                notice: Some(_),
                error: None,
                ..
            } => ("completed", false),
            _ => ("ready", false),
        }
    }

    /// Select a `id`-th item in the currently focused window of the page.
    pub fn select(&mut self, id: usize) {
        if let Some(mut state) = self.focus_window_state_mut() {
            state.select(id);
        }
        if let Some(playlist_state) = self.mutable_playlist_state_mut() {
            // A direct cursor command is authoritative. The next projection
            // resolves this visible index to its occurrence token.
            playlist_state.set_cursor_occurrence(None);
        }
    }

    /// Read the cursor without borrowing the page mutably.
    ///
    /// Renderers and read-only event routing should use this projection so
    /// cursor ownership stays with the page's focused window state instead of
    /// being recreated by a second UI model.
    pub fn selected_index(&self) -> Option<usize> {
        self.diagnostic_selection()
    }

    /// Move Settings selection to the next or previous section. This gives
    /// the section panels a keyboard focus model without introducing a second
    /// selection state alongside the existing row list.
    pub fn focus_settings_section(&mut self, forward: bool) -> bool {
        let Self::Settings { list, settings, .. } = self else {
            return false;
        };
        if settings.is_empty() {
            return false;
        }
        let selected = list.selected().unwrap_or_default().min(settings.len() - 1);
        let mut section_starts = vec![0];
        for index in 1..settings.len() {
            if settings[index - 1].section != settings[index].section {
                section_starts.push(index);
            }
        }
        let current_section = section_starts
            .iter()
            .rposition(|start| *start <= selected)
            .unwrap_or_default();
        let target_section = if forward {
            (current_section + 1) % section_starts.len()
        } else if current_section == 0 {
            section_starts.len() - 1
        } else {
            current_section - 1
        };
        list.select(section_starts.get(target_section).copied());
        true
    }

    /// Apply `update` to the Settings tile navigation of `category` and
    /// return its result. The list cursor stays the selected setting: it is
    /// loaded into the navigation first and written back afterwards.
    pub(crate) fn update_settings_tiles(
        &mut self,
        category: SettingsCategory,
        query: Option<&str>,
        update: impl FnOnce(&mut ShelfNav<AppConfigSection>, &[ShelfSize<AppConfigSection>]) -> bool,
    ) -> bool {
        let Self::Settings {
            list,
            settings,
            shelves: navs,
            ..
        } = self
        else {
            return false;
        };
        let shelves = settings_tile_shelves(settings, category, query);
        let sizes = settings_shelf_sizes(&shelves);
        let nav = &mut navs[category.index()];
        let current = list.selected().and_then(|selected| {
            shelves.iter().find_map(|(section, sources)| {
                let index = sources.iter().position(|source| *source == selected)?;
                Some((*section, index))
            })
        });
        match current {
            Some((section, index)) => nav.select(section, index),
            None => nav.clamp(&sizes),
        }
        let changed = update(nav, &sizes);
        let selected = shelves
            .iter()
            .find(|(section, _)| *section == nav.focus)
            .and_then(|(_, sources)| sources.get(nav.selected(nav.focus)))
            .copied();
        if selected.is_some() {
            list.select(selected);
        }
        changed
    }

    pub(crate) fn toggle_lyrics_follow(&mut self) -> bool {
        let Self::Lyrics {
            follow_playback, ..
        } = self
        else {
            return false;
        };
        *follow_playback = !*follow_playback;
        true
    }

    /// The currently focused window state of the page.
    pub fn focus_window_state_mut(&mut self) -> Option<MutableWindowState<'_>> {
        match self {
            Self::Home { .. } => None,
            Self::Welcome { state, .. } => Some(MutableWindowState::List(&mut state.list)),
            Self::Library {
                state:
                    LibraryPageUIState {
                        playlist_list,
                        saved_album_list,
                        followed_artist_list,
                        focus,
                        ..
                    },
            } => Some(match focus {
                LibraryFocusState::Playlists => MutableWindowState::List(playlist_list),
                LibraryFocusState::SavedAlbums => MutableWindowState::List(saved_album_list),
                LibraryFocusState::FollowedArtists => {
                    MutableWindowState::List(followed_artist_list)
                }
            }),
            Self::Search {
                state:
                    SearchPageUIState {
                        track_list,
                        video_list,
                        album_list,
                        artist_list,
                        playlist_list,
                        show_list,
                        episode_list,
                        focus,
                        ..
                    },
                ..
            } => match focus {
                SearchFocusState::Category | SearchFocusState::Input => None,
                SearchFocusState::Tracks => Some(MutableWindowState::List(track_list)),
                SearchFocusState::Videos => Some(MutableWindowState::List(video_list)),
                SearchFocusState::Albums => Some(MutableWindowState::List(album_list)),
                SearchFocusState::Artists => Some(MutableWindowState::List(artist_list)),
                SearchFocusState::Playlists => Some(MutableWindowState::List(playlist_list)),
                SearchFocusState::Shows => Some(MutableWindowState::List(show_list)),
                SearchFocusState::Episodes => Some(MutableWindowState::List(episode_list)),
            },
            Self::Context { state, .. } => state.as_mut().and_then(|state| match state {
                ContextPageUIState::Playlist { playlist_state } => {
                    Some(MutableWindowState::Table(playlist_state.table_mut()))
                }
                ContextPageUIState::Tracks { track_table, .. }
                | ContextPageUIState::Album { track_table, .. } => {
                    Some(MutableWindowState::Table(track_table))
                }
                ContextPageUIState::Artist {
                    top_track_table,
                    album_table,
                    related_artist_list,
                    liked_track_table,
                    focus,
                    ..
                } => match focus {
                    ArtistFocusState::TopTracks => Some(MutableWindowState::Table(top_track_table)),
                    ArtistFocusState::Albums => Some(MutableWindowState::Table(album_table)),
                    ArtistFocusState::RelatedArtists => {
                        Some(MutableWindowState::List(related_artist_list))
                    }
                    ArtistFocusState::LikedSongs => {
                        Some(MutableWindowState::Table(liked_track_table))
                    }
                },
                ContextPageUIState::Show { episode_table } => {
                    Some(MutableWindowState::Table(episode_table))
                }
                ContextPageUIState::Failed { .. } => None,
            }),
            Self::YouTubeContext {
                id: YouTubeContextId::Playlist(_),
                state,
                ..
            } => Some(MutableWindowState::Table(
                state.mutable_playlist.table_mut(),
            )),
            Self::YouTubeContext { state, .. } => {
                Some(MutableWindowState::Table(&mut state.track_list))
            }
            Self::UnifiedPlaylist { playlist_state, .. } => {
                Some(MutableWindowState::Table(playlist_state.table_mut()))
            }
            Self::Browse { state } => match state {
                BrowsePageUIState::CategoryList { state } => Some(MutableWindowState::List(state)),
                BrowsePageUIState::CategoryPlaylistList { state, .. } => {
                    Some(MutableWindowState::List(state))
                }
            },
            Self::Lyrics { scroll_offset, .. } | Self::CommandHelp { scroll_offset } => {
                Some(MutableWindowState::Scroll(scroll_offset))
            }
            Self::Journal { table, .. } | Self::JournalList { table, .. } => {
                Some(MutableWindowState::Table(table))
            }
            Self::HomeShelfList { list, .. }
            | Self::SessionHistory { list }
            | Self::JournalLists { list }
            | Self::Settings { list, .. } => Some(MutableWindowState::List(list)),
            Self::Logs { state } => Some(MutableWindowState::List(&mut state.list)),
            Self::Queue { table, .. } => Some(MutableWindowState::Table(table)),
        }
    }

    /// Return the exact Spotify Context track adapter for `pane`.
    ///
    /// `context_pane` is only consulted for Spotify Context pages; all other
    /// page families resolve their adapter from the focused page state.
    pub fn selection_adapter_mut(
        &mut self,
        context_pane: Option<ContextTrackPane>,
    ) -> Option<PageSelectionAdapter<'_>> {
        match self {
            Self::Search {
                state:
                    SearchPageUIState {
                        focus: SearchFocusState::Tracks | SearchFocusState::Videos,
                        search_selection,
                        ..
                    },
                ..
            } => Some(PageSelectionAdapter::Search(search_selection)),
            Self::Context {
                state: Some(ContextPageUIState::Playlist { playlist_state }),
                ..
            }
            | Self::UnifiedPlaylist { playlist_state, .. } => Some(
                PageSelectionAdapter::MutablePlaylist(playlist_state.selection_mut()),
            ),
            Self::Context { state, .. } => {
                let pane = context_pane?;
                state
                    .as_mut()
                    .and_then(|state| state.track_selection_for_pane_mut(pane))
                    .map(PageSelectionAdapter::Context)
            }
            Self::YouTubeContext {
                id: YouTubeContextId::Playlist(_),
                state,
                ..
            } => Some(PageSelectionAdapter::MutablePlaylist(
                state.mutable_playlist.selection_mut(),
            )),
            Self::YouTubeContext { state, .. } => Some(PageSelectionAdapter::YouTubeContext(
                &mut state.youtube_context_selection,
            )),
            Self::Journal {
                journal_selection, ..
            }
            | Self::JournalList {
                journal_selection, ..
            } => Some(PageSelectionAdapter::Journal(journal_selection)),
            Self::Queue {
                queue_selection, ..
            } => Some(PageSelectionAdapter::Queue(queue_selection)),
            _ => None,
        }
    }

    /// Return the exact Spotify Context track adapter for `pane`.
    #[allow(dead_code)]
    pub fn context_track_selection(
        &self,
        pane: ContextTrackPane,
    ) -> Option<&ContextTrackSelection> {
        match self {
            Self::Context { state, .. } => state
                .as_ref()
                .and_then(|state| state.track_selection_for_pane(pane)),
            _ => None,
        }
    }

    /// Return the mutable exact Spotify Context track adapter for `pane`.
    #[allow(dead_code)]
    pub fn context_track_selection_mut(
        &mut self,
        pane: ContextTrackPane,
    ) -> Option<&mut ContextTrackSelection> {
        match self {
            Self::Context { state, .. } => state
                .as_mut()
                .and_then(|state| state.track_selection_for_pane_mut(pane)),
            _ => None,
        }
    }

    /// Return the adapter for the currently focused Spotify Context track
    /// pane. Cursor-only panes and Show return `None`.
    #[allow(dead_code)]
    pub fn focused_context_track_selection(&self) -> Option<&ContextTrackSelection> {
        match self {
            Self::Context { state, .. } => state
                .as_ref()
                .and_then(ContextPageUIState::focused_track_selection),
            _ => None,
        }
    }

    /// Return the mutable adapter for the currently focused Spotify Context
    /// track pane. Cursor-only panes and Show return `None`.
    #[allow(dead_code)]
    pub fn focused_context_track_selection_mut(&mut self) -> Option<&mut ContextTrackSelection> {
        match self {
            Self::Context { state, .. } => state
                .as_mut()
                .and_then(ContextPageUIState::focused_track_selection_mut),
            _ => None,
        }
    }

    /// Return the keyed selection owned by a `YouTube` context track page.
    #[allow(dead_code)]
    pub fn youtube_context_track_selection(&self) -> Option<&YouTubeContextSelection> {
        match self {
            Self::YouTubeContext { state, .. } => Some(&state.youtube_context_selection),
            _ => None,
        }
    }

    /// Return the mutable keyed selection owned by a `YouTube` context track
    /// page.
    #[allow(dead_code)]
    pub fn youtube_context_track_selection_mut(&mut self) -> Option<&mut YouTubeContextSelection> {
        match self {
            Self::YouTubeContext { state, .. } => Some(&mut state.youtube_context_selection),
            _ => None,
        }
    }

    /// Return the keyed selection owned by the Journal page only.
    #[allow(dead_code)]
    pub fn journal_selection(&self) -> Option<&JournalSelection> {
        match self {
            Self::Journal {
                journal_selection, ..
            } => Some(journal_selection),
            _ => None,
        }
    }

    /// Return the mutable keyed selection owned by the Journal page only.
    #[allow(dead_code)]
    pub fn journal_selection_mut(&mut self) -> Option<&mut JournalSelection> {
        match self {
            Self::Journal {
                journal_selection, ..
            } => Some(journal_selection),
            _ => None,
        }
    }

    /// Return the keyed selection owned by the exact `JournalList` page.
    /// The list ID is part of synchronization scope, not accessor identity.
    #[allow(dead_code)]
    pub fn journal_list_selection(&self) -> Option<&JournalSelection> {
        match self {
            Self::JournalList {
                journal_selection, ..
            } => Some(journal_selection),
            _ => None,
        }
    }

    /// Return the mutable keyed selection owned by the exact `JournalList` page.
    #[allow(dead_code)]
    pub fn journal_list_selection_mut(&mut self) -> Option<&mut JournalSelection> {
        match self {
            Self::JournalList {
                journal_selection, ..
            } => Some(journal_selection),
            _ => None,
        }
    }

    /// Return the keyed selection owned by the `UnifiedPlaylist` page only.
    #[allow(dead_code)]
    pub fn unified_playlist_selection(&self) -> Option<&MutablePlaylistSelection> {
        match self {
            Self::UnifiedPlaylist { playlist_state, .. } => Some(playlist_state.selection()),
            _ => None,
        }
    }

    /// Return the mutable keyed selection owned by the `UnifiedPlaylist` page.
    #[allow(dead_code)]
    pub fn unified_playlist_selection_mut(&mut self) -> Option<&mut MutablePlaylistSelection> {
        match self {
            Self::UnifiedPlaylist { playlist_state, .. } => Some(playlist_state.selection_mut()),
            _ => None,
        }
    }

    pub fn unified_playlist_cursor_entry_id(&self) -> Option<PlaylistEntryId> {
        match self {
            Self::UnifiedPlaylist { playlist_state, .. } => playlist_state
                .cursor_occurrence()
                .and_then(|occurrence| match occurrence {
                    ProviderOccurrenceToken::Unified(entry_id) => Some(*entry_id),
                    _ => None,
                }),
            _ => None,
        }
    }

    pub fn set_unified_playlist_cursor_entry_id(&mut self, entry_id: Option<PlaylistEntryId>) {
        if let Self::UnifiedPlaylist { playlist_state, .. } = self {
            playlist_state.set_cursor_occurrence(entry_id.map(ProviderOccurrenceToken::Unified));
        }
    }

    pub fn mutable_playlist_state(&self) -> Option<&MutablePlaylistState> {
        match self {
            Self::UnifiedPlaylist { playlist_state, .. }
            | Self::Context {
                state: Some(ContextPageUIState::Playlist { playlist_state }),
                ..
            } => Some(playlist_state),
            Self::YouTubeContext {
                id: YouTubeContextId::Playlist(_),
                state,
                ..
            } => Some(&state.mutable_playlist),
            _ => None,
        }
    }

    pub fn mutable_playlist_state_mut(&mut self) -> Option<&mut MutablePlaylistState> {
        match self {
            Self::UnifiedPlaylist { playlist_state, .. }
            | Self::Context {
                state: Some(ContextPageUIState::Playlist { playlist_state }),
                ..
            } => Some(playlist_state),
            Self::YouTubeContext {
                id: YouTubeContextId::Playlist(_),
                state,
                ..
            } => Some(&mut state.mutable_playlist),
            _ => None,
        }
    }

    /// Return the keyed selection owned by the Queue page only.
    #[allow(dead_code)]
    pub fn queue_selection(&self) -> Option<&QueueSelection> {
        match self {
            Self::Queue {
                queue_selection, ..
            } => Some(queue_selection),
            _ => None,
        }
    }

    /// Return the mutable keyed selection owned by the Queue page.
    #[allow(dead_code)]
    pub fn queue_selection_mut(&mut self) -> Option<&mut QueueSelection> {
        match self {
            Self::Queue {
                queue_selection, ..
            } => Some(queue_selection),
            _ => None,
        }
    }
}

impl PageState {
    /// Construct an empty Queue page with fresh scoped selection state.
    pub fn new_queue() -> Self {
        Self::Queue {
            table: TableState::default(),
            queue_selection: QueueSelection::default(),
        }
    }

    /// Construct an empty `UnifiedPlaylist` page for `id`.
    pub fn new_unified_playlist(id: impl Into<String>) -> Self {
        Self::UnifiedPlaylist {
            id: id.into(),
            playlist_state: MutablePlaylistState::default(),
            listenbrainz_sync: super::ListenBrainzSyncLifecycle::Idle,
            listenbrainz_preview: None,
        }
    }
}

impl LibraryPageUIState {
    pub fn new() -> Self {
        Self {
            playlist_list: ListState::default(),
            saved_album_list: ListState::default(),
            followed_artist_list: ListState::default(),
            focus: LibraryFocusState::Playlists,
            playlist_folder_id: 0,
        }
    }
}

impl SearchPageUIState {
    pub fn new() -> Self {
        Self {
            track_list: ListState::default(),
            search_selection: SearchSelection::default(),
            video_list: ListState::default(),
            album_list: ListState::default(),
            artist_list: ListState::default(),
            playlist_list: ListState::default(),
            show_list: ListState::default(),
            episode_list: ListState::default(),
            focus: SearchFocusState::Input,
            category: None,
            provider: None,
            search_lifecycle: SearchLifecycle::default(),
            pending_lucky: None,
        }
    }
}

#[cfg(test)]
mod selection_accessor_tests {
    use super::*;
    use crate::config;
    use crate::state::{synchronize_journal_uris, JournalSelectionScope, ScopedSelectionStatus};

    #[test]
    fn youtube_context_diagnostic_state_uses_privacy_safe_lifecycle() {
        let mut page = PageState::YouTubeContext {
            id: YouTubeContextId::LikedTracks,
            context: None,
            state: YouTubeContextPageUIState::new(),
        };

        assert_eq!(page.diagnostic_content_state(), ("loading", true));

        if let PageState::YouTubeContext { state, .. } = &mut page {
            state.status = UiViewStatus::Failed {
                code: YOUTUBE_CONTEXT_ERROR_CODE,
                message: YOUTUBE_CONTEXT_ERROR_MESSAGE,
                next_action: YOUTUBE_CONTEXT_ERROR_NEXT_ACTION,
            };
        }
        if let PageState::YouTubeContext { state, .. } = &page {
            assert!(matches!(state.status, UiViewStatus::Failed { .. }));
        }
        assert_eq!(page.diagnostic_content_state(), ("failed", false));

        if let PageState::YouTubeContext { context, .. } = &mut page {
            *context = Some(crate::state::YouTubeContext::default());
        }
        assert_eq!(
            page.diagnostic_content_state(),
            ("failed", false),
            "a cached context must not hide a failed refresh"
        );

        if let PageState::YouTubeContext { state, .. } = &mut page {
            state.status = UiViewStatus::Empty;
        }
        assert_eq!(page.diagnostic_content_state(), ("empty", false));
    }

    #[test]
    fn spotify_context_routes_keyed_adapter() {
        let context = PageState::Context {
            id: None,
            context_page_type: ContextPageType::CurrentPlaying,
            state: Some(ContextPageUIState::new_tracks()),
        };
        assert!(context.focused_context_track_selection().is_some());
    }

    #[test]
    fn spotify_context_failure_is_visible_and_disables_row_navigation() {
        let mut page = PageState::Context {
            id: Some(ContextId::Tracks(crate::state::TracksId {
                uri: "spotify:top:tracks".to_owned(),
                kind: "Top tracks".to_owned(),
            })),
            context_page_type: ContextPageType::CurrentPlaying,
            state: Some(ContextPageUIState::Failed {
                status: UiViewStatus::Failed {
                    code: CONTEXT_ERROR_CODE,
                    message: CONTEXT_ERROR_MESSAGE,
                    next_action: CONTEXT_ERROR_NEXT_ACTION,
                },
            }),
        };

        assert_eq!(page.diagnostic_content_state(), ("failed", false));
        assert_eq!(page.selected_index(), None);
        assert!(page.focused_context_track_selection().is_none());
        assert!(page.focus_window_state_mut().is_none());
    }

    #[test]
    fn page_selection_adapter_owns_shared_range_and_clear_operations() {
        let mut page = PageState::Journal {
            table: TableState::default(),
            journal_selection: JournalSelection::default(),
        };
        synchronize_journal_uris(
            page.journal_selection_mut().unwrap(),
            JournalSelectionScope::journal(1),
            None,
            ["spotify:track:a", "spotify:track:b"],
            ["spotify:track:a", "spotify:track:b"],
        )
        .unwrap();

        let mut adapter = page.selection_adapter_mut(None).unwrap();
        assert!(adapter.extend_visible_range(0, 1).is_ok());
        assert_eq!(adapter.selected_visible_indices(), vec![0, 1]);
        adapter.clear_selection();
        assert!(adapter.selected_visible_indices().is_empty());
    }

    #[test]
    fn selected_index_projects_the_page_cursor_without_mutable_borrowing() {
        let mut page = PageState::Journal {
            table: TableState::default(),
            journal_selection: JournalSelection::default(),
        };

        assert_eq!(page.selected_index(), None);
        page.select(2);
        assert_eq!(page.selected_index(), Some(2));
    }

    #[test]
    fn settings_section_focus_jumps_between_panel_ranges() {
        let setting = |section: config::AppConfigSection, key: &str| config::AppConfigSetting {
            section,
            key: key.to_owned(),
            value: "false".to_owned(),
            kind: config::AppConfigValueKind::Bool,
            restart_required: false,
        };
        let mut page = PageState::Settings {
            list: ListState::default(),
            shelves: SettingsShelves::default(),
            settings: vec![
                setting(config::AppConfigSection::Accounts, "a1"),
                setting(config::AppConfigSection::Accounts, "a2"),
                setting(config::AppConfigSection::Playback, "p1"),
                setting(config::AppConfigSection::SharedUi, "u1"),
            ],
            saved: false,
            error: None,
            notice: None,
        };
        page.select(1);
        assert!(page.focus_settings_section(true));
        assert_eq!(page.selected_index(), Some(2));
        assert!(page.focus_settings_section(false));
        assert_eq!(page.selected_index(), Some(0));
        page.select(3);
        assert!(page.focus_settings_section(true));
        assert_eq!(page.selected_index(), Some(0));
    }

    #[test]
    fn settings_tiles_group_by_section_and_move_the_list_cursor() {
        let setting = |section: config::AppConfigSection, key: &str| config::AppConfigSetting {
            section,
            key: key.to_owned(),
            value: "false".to_owned(),
            kind: config::AppConfigValueKind::Bool,
            restart_required: false,
        };
        let mut page = PageState::Settings {
            list: ListState::default(),
            shelves: SettingsShelves::default(),
            settings: vec![
                setting(config::AppConfigSection::Accounts, "account"),
                setting(config::AppConfigSection::Playback, "p1"),
                setting(config::AppConfigSection::SharedUi, "u1"),
                setting(config::AppConfigSection::Playback, "p2"),
            ],
            saved: false,
            error: None,
            notice: None,
        };
        let PageState::Settings { settings, .. } = &page else {
            unreachable!()
        };
        assert_eq!(
            settings_tile_shelves(settings, SettingsCategory::Preferences, None),
            vec![
                (config::AppConfigSection::Playback, vec![1, 3]),
                (config::AppConfigSection::SharedUi, vec![2]),
            ]
        );

        page.select(1);
        let moved =
            page.update_settings_tiles(SettingsCategory::Preferences, None, |nav, sizes| {
                nav.move_horizontal(sizes, 1)
            });
        assert!(moved);
        assert_eq!(page.selected_index(), Some(3));
        page.update_settings_tiles(SettingsCategory::Preferences, None, |nav, sizes| {
            nav.move_between_shelves(sizes, 1)
        });
        assert_eq!(page.selected_index(), Some(2));
    }

    #[test]
    fn settings_filter_preserves_source_indices_and_matches_user_facing_copy() {
        let setting =
            |section: config::AppConfigSection, key: &str, value: &str| config::AppConfigSetting {
                section,
                key: key.to_owned(),
                value: value.to_owned(),
                kind: config::AppConfigValueKind::Bool,
                restart_required: false,
            };
        let settings = vec![
            setting(
                config::AppConfigSection::Spotify,
                "active_provider",
                "Spotify",
            ),
            setting(
                config::AppConfigSection::SharedUi,
                "enable_mouse_scroll_volume",
                "true",
            ),
            setting(
                config::AppConfigSection::Services,
                "listenbrainz.artist_enrichment",
                "false",
            ),
        ];

        let by_label = settings_filter_projection(&settings, Some("mouse volume"));
        assert_eq!(by_label.len(), 1);
        assert_eq!(by_label[0].0, 1);
        assert_eq!(by_label[0].1.key, "enable_mouse_scroll_volume");

        let by_description = settings_filter_projection(&settings, Some("spotify unavailable"));
        assert_eq!(by_description.len(), 1);
        assert_eq!(by_description[0].0, 2);
    }

    #[test]
    fn packet_e_accessors_route_exact_pages_and_keep_state_independent() {
        let mut youtube = PageState::YouTubeContext {
            id: YouTubeContextId::LikedTracks,
            context: None,
            state: YouTubeContextPageUIState::new(),
        };
        let mut journal = PageState::Journal {
            table: TableState::default(),
            journal_selection: JournalSelection::default(),
        };
        let journal_list = PageState::JournalList {
            list_id: "list".to_owned(),
            table: TableState::default(),
            journal_selection: JournalSelection::default(),
        };

        assert!(youtube.youtube_context_track_selection().is_some());
        assert!(youtube.journal_selection().is_none());
        assert!(youtube.journal_list_selection().is_none());
        assert!(journal.youtube_context_track_selection().is_none());
        assert!(journal.journal_selection().is_some());
        assert!(journal.journal_list_selection().is_none());
        assert!(journal_list.journal_selection().is_none());
        assert!(journal_list.journal_list_selection().is_some());

        synchronize_journal_uris(
            journal.journal_selection_mut().unwrap(),
            JournalSelectionScope::journal(1),
            None,
            ["spotify:track:one"],
            ["spotify:track:one"],
        )
        .unwrap();
        assert_eq!(
            journal.journal_selection().unwrap().status(),
            ScopedSelectionStatus::Ready
        );
        assert_eq!(
            journal_list.journal_list_selection().unwrap().status(),
            ScopedSelectionStatus::Unscoped
        );

        youtube
            .youtube_context_track_selection_mut()
            .unwrap()
            .clear();
        assert!(youtube
            .youtube_context_track_selection()
            .unwrap()
            .selected_visible_indices()
            .is_empty());
    }
}

#[cfg(test)]
mod context_selection_accessor_tests {
    use super::*;
    use crate::state::ui::{ContextSelectionScope, ContextSelectionView, OccurrenceDescriptor};

    fn descriptors(values: &[&str]) -> Vec<OccurrenceDescriptor<String>> {
        values
            .iter()
            .map(|value| OccurrenceDescriptor::unique((*value).to_owned()))
            .collect()
    }

    fn context(state: ContextPageUIState) -> PageState {
        PageState::Context {
            id: None,
            context_page_type: ContextPageType::CurrentPlaying,
            state: Some(state),
        }
    }

    #[test]
    fn each_standard_context_variant_routes_only_its_exact_track_pane() {
        let playlist = context(ContextPageUIState::new_playlist());
        assert!(playlist.mutable_playlist_state().is_some());
        assert!(playlist
            .context_track_selection(ContextTrackPane::Playlist)
            .is_none());
        let cases = [
            (ContextPageUIState::new_album(), [ContextTrackPane::Album]),
            (ContextPageUIState::new_tracks(), [ContextTrackPane::Tracks]),
        ];
        let all_panes = [
            ContextTrackPane::Playlist,
            ContextTrackPane::Album,
            ContextTrackPane::Tracks,
            ContextTrackPane::ArtistTopTracks,
            ContextTrackPane::ArtistLikedSongs,
        ];
        for (state, expected) in cases {
            let page = context(state);
            for pane in all_panes {
                assert_eq!(
                    page.context_track_selection(pane).is_some(),
                    expected.contains(&pane),
                    "unexpected adapter route for {pane:?}"
                );
            }
        }

        let artist = context(ContextPageUIState::new_artist());
        assert!(artist
            .context_track_selection(ContextTrackPane::ArtistTopTracks)
            .is_some());
        assert!(artist
            .context_track_selection(ContextTrackPane::ArtistLikedSongs)
            .is_some());
        assert!(artist
            .context_track_selection(ContextTrackPane::Playlist)
            .is_none());

        let show = context(ContextPageUIState::new_show());
        for pane in all_panes {
            assert!(show.context_track_selection(pane).is_none());
        }
        assert!(show.focused_context_track_selection().is_none());
    }

    #[test]
    fn artist_top_and_liked_adapters_remain_independent_across_focus_changes() {
        let mut page = context(ContextPageUIState::new_artist());
        let top_scope =
            ContextSelectionScope::new(3, "spotify:artist:one", ContextTrackPane::ArtistTopTracks);
        let liked_scope =
            ContextSelectionScope::new(3, "spotify:artist:one", ContextTrackPane::ArtistLikedSongs);

        page.context_track_selection_mut(ContextTrackPane::ArtistTopTracks)
            .unwrap()
            .synchronize(
                top_scope,
                descriptors(&["spotify:track:top-a", "spotify:track:top-b"]),
                ContextSelectionView::unfiltered(),
                descriptors(&["spotify:track:top-a", "spotify:track:top-b"]),
            )
            .unwrap();
        page.context_track_selection_mut(ContextTrackPane::ArtistTopTracks)
            .unwrap()
            .extend_range(0, 1)
            .unwrap();

        page.context_track_selection_mut(ContextTrackPane::ArtistLikedSongs)
            .unwrap()
            .synchronize(
                liked_scope,
                descriptors(&["spotify:track:liked-a", "spotify:track:liked-b"]),
                ContextSelectionView::unfiltered(),
                descriptors(&["spotify:track:liked-a", "spotify:track:liked-b"]),
            )
            .unwrap();
        page.context_track_selection_mut(ContextTrackPane::ArtistLikedSongs)
            .unwrap()
            .set_anchor(1)
            .unwrap();

        assert_eq!(
            page.context_track_selection(ContextTrackPane::ArtistTopTracks)
                .unwrap()
                .selected_visible_indices(),
            vec![0, 1]
        );
        assert_eq!(
            page.context_track_selection(ContextTrackPane::ArtistLikedSongs)
                .unwrap()
                .selected_visible_indices(),
            Vec::<usize>::new()
        );

        if let PageState::Context {
            state: Some(ContextPageUIState::Artist { focus, .. }),
            ..
        } = &mut page
        {
            *focus = ArtistFocusState::LikedSongs;
        }
        assert_eq!(
            page.focused_context_track_selection()
                .unwrap()
                .anchor_visible_index(),
            Some(1)
        );

        if let PageState::Context {
            state: Some(ContextPageUIState::Artist { focus, .. }),
            ..
        } = &mut page
        {
            *focus = ArtistFocusState::TopTracks;
        }
        assert_eq!(
            page.focused_context_track_selection()
                .unwrap()
                .selected_visible_indices(),
            vec![0, 1]
        );
        assert_eq!(
            page.context_track_selection(ContextTrackPane::ArtistLikedSongs)
                .unwrap()
                .anchor_visible_index(),
            Some(1)
        );
    }
}

#[cfg(test)]
mod diagnostics_state_tests {
    use super::*;
    use crate::observability::{DiagnosticRow, DiagnosticRowId, Severity};

    fn row(id: DiagnosticRowId, label: &str) -> DiagnosticRow {
        DiagnosticRow {
            id,
            label: label.to_owned(),
            summary: "healthy".to_owned(),
            severity: Severity::Info,
            acknowledged: false,
        }
    }

    #[test]
    fn diagnostic_selection_follows_typed_identity_during_live_refresh() {
        let runtime = DiagnosticRowId::Component(crate::observability::Component::Runtime);
        let logging = DiagnosticRowId::Logging;
        let mut page = DiagnosticsPageUIState::new();
        let initial = vec![
            row(runtime.clone(), "runtime"),
            row(logging.clone(), "logging"),
        ];
        page.select_index(&initial, 1);

        let refreshed = vec![
            row(
                DiagnosticRowId::Component(crate::observability::Component::Audio),
                "audio",
            ),
            row(runtime, "runtime"),
            row(logging.clone(), "logging recovered"),
        ];
        page.synchronize(&refreshed);

        assert_eq!(page.selected_row, Some(logging));
        assert_eq!(page.list.selected(), Some(2));
    }

    #[test]
    fn diagnostic_selection_recovers_when_a_bounded_row_expires() {
        let incident = DiagnosticRowId::Incident("I-ab12cd34".to_owned());
        let mut page = DiagnosticsPageUIState::new();
        let initial = vec![
            row(DiagnosticRowId::Logging, "logging"),
            row(incident, "incident"),
        ];
        page.select_index(&initial, 1);
        let refreshed = vec![row(DiagnosticRowId::Logging, "logging")];
        page.synchronize(&refreshed);
        assert_eq!(page.selected_row, Some(DiagnosticRowId::Logging));
        assert_eq!(page.list.selected(), Some(0));
    }

    #[cfg(feature = "private-capture")]
    #[test]
    fn private_capture_selection_is_stable_while_safe_state_changes() {
        let mut page = DiagnosticsPageUIState::new();
        let initial = vec![
            row(DiagnosticRowId::Logging, "logging"),
            row(DiagnosticRowId::PrivateCapture, "private capture / armed"),
        ];
        page.select_index(&initial, 1);

        let refreshed = vec![
            row(
                DiagnosticRowId::PrivateCapture,
                "private capture / capturing",
            ),
            row(DiagnosticRowId::Logging, "logging"),
        ];
        page.synchronize(&refreshed);

        assert_eq!(page.selected_row, Some(DiagnosticRowId::PrivateCapture));
        assert_eq!(page.list.selected(), Some(0));
    }
}

#[cfg(test)]
mod queue_state_tests {
    use super::{MutableWindowState, PageState, QueueSelection};
    use crate::config::ActiveProvider;
    use crate::state::UiViewStatus;
    use ratatui::widgets::TableState;

    #[test]
    fn queue_page_uses_a_selectable_table_state() {
        let mut page = PageState::Queue {
            table: TableState::default(),
            queue_selection: QueueSelection::default(),
        };

        page.select(3);

        assert_eq!(page.selected_index(), Some(3));
        assert!(matches!(
            page.focus_window_state_mut(),
            Some(MutableWindowState::Table(_))
        ));
    }

    #[test]
    fn queue_and_unified_playlist_accessors_route_exact_page_state() {
        let mut queue = PageState::new_queue();
        let mut playlist = PageState::new_unified_playlist("local-playlist");

        assert!(queue.queue_selection().is_some());
        assert!(queue.unified_playlist_selection().is_none());
        assert!(playlist.queue_selection().is_none());
        assert!(playlist.unified_playlist_selection().is_some());

        queue.queue_selection_mut().unwrap().clear();
        playlist.unified_playlist_selection_mut().unwrap().clear();
        assert!(queue
            .queue_selection()
            .unwrap()
            .selected_visible_indices()
            .is_empty());
        assert!(playlist
            .unified_playlist_selection()
            .unwrap()
            .selected_visible_indices()
            .is_empty());
    }

    #[test]
    fn lyrics_page_exposes_scroll_state_to_shared_navigation() {
        let mut page = PageState::Lyrics {
            provider: ActiveProvider::Spotify,
            track_uri: "spotify:track:lyrics".to_owned(),
            track: "Track".to_owned(),
            artists: "Artist".to_owned(),
            youtube_track: None,
            lyrics_provider: None,
            scroll_offset: 0,
            follow_playback: true,
            status: UiViewStatus::Ready,
        };

        page.select(4);

        assert_eq!(page.selected_index(), Some(4));
        assert!(matches!(
            page.focus_window_state_mut(),
            Some(MutableWindowState::Scroll(_))
        ));
        assert!(matches!(
            page,
            PageState::Lyrics {
                scroll_offset: 4,
                follow_playback: true,
                ..
            }
        ));
    }

    #[test]
    fn lyrics_follow_mode_toggles_without_affecting_scroll_position() {
        let mut page = PageState::Lyrics {
            provider: ActiveProvider::Spotify,
            track_uri: "spotify:track:lyrics".to_owned(),
            track: "Track".to_owned(),
            artists: "Artist".to_owned(),
            youtube_track: None,
            lyrics_provider: None,
            scroll_offset: 7,
            follow_playback: true,
            status: UiViewStatus::Ready,
        };

        assert!(page.toggle_lyrics_follow());
        assert!(matches!(
            page,
            PageState::Lyrics {
                scroll_offset: 7,
                follow_playback: false,
                ..
            }
        ));
        assert!(page.toggle_lyrics_follow());
        assert!(matches!(
            page,
            PageState::Lyrics {
                scroll_offset: 7,
                follow_playback: true,
                ..
            }
        ));
    }
}

impl YouTubeContextPageUIState {
    pub fn new() -> Self {
        Self {
            track_list: TableState::default(),
            youtube_context_selection: YouTubeContextSelection::default(),
            mutable_playlist: MutablePlaylistState::default(),
            status: UiViewStatus::Loading,
        }
    }
}

impl ContextPageType {
    pub fn title(&self) -> String {
        match self {
            ContextPageType::CurrentPlaying => String::from("Current Playing"),
            ContextPageType::Browsing(id) => match id {
                ContextId::Playlist(_) => String::from("Playlist"),
                ContextId::Album(_) => String::from("Album"),
                ContextId::Artist(_) => String::from("Artist"),
                ContextId::Tracks(id) => id.kind.clone(),
                ContextId::Show(_) => String::from("Show"),
            },
        }
    }
}

impl ContextPageUIState {
    /// Return the exact adapter owned by `pane`, independent of artist focus.
    #[allow(dead_code)]
    pub fn track_selection_for_pane(
        &self,
        pane: ContextTrackPane,
    ) -> Option<&ContextTrackSelection> {
        match (self, pane) {
            (
                Self::Album {
                    track_selection, ..
                },
                ContextTrackPane::Album,
            )
            | (
                Self::Tracks {
                    track_selection, ..
                },
                ContextTrackPane::Tracks,
            ) => Some(track_selection),
            (
                Self::Artist {
                    top_track_selection,
                    ..
                },
                ContextTrackPane::ArtistTopTracks,
            ) => Some(top_track_selection),
            (
                Self::Artist {
                    liked_track_selection,
                    ..
                },
                ContextTrackPane::ArtistLikedSongs,
            ) => Some(liked_track_selection),
            _ => None,
        }
    }

    /// Return the mutable exact adapter owned by `pane`, independent of artist
    /// focus.
    #[allow(dead_code)]
    pub fn track_selection_for_pane_mut(
        &mut self,
        pane: ContextTrackPane,
    ) -> Option<&mut ContextTrackSelection> {
        match (self, pane) {
            (
                Self::Album {
                    track_selection, ..
                },
                ContextTrackPane::Album,
            )
            | (
                Self::Tracks {
                    track_selection, ..
                },
                ContextTrackPane::Tracks,
            ) => Some(track_selection),
            (
                Self::Artist {
                    top_track_selection,
                    ..
                },
                ContextTrackPane::ArtistTopTracks,
            ) => Some(top_track_selection),
            (
                Self::Artist {
                    liked_track_selection,
                    ..
                },
                ContextTrackPane::ArtistLikedSongs,
            ) => Some(liked_track_selection),
            _ => None,
        }
    }

    /// Return the adapter for the currently focused Artist track pane.
    #[allow(dead_code)]
    pub fn focused_track_selection(&self) -> Option<&ContextTrackSelection> {
        match self {
            Self::Artist {
                top_track_selection,
                liked_track_selection,
                focus,
                ..
            } => match focus {
                ArtistFocusState::TopTracks => Some(top_track_selection),
                ArtistFocusState::LikedSongs => Some(liked_track_selection),
                ArtistFocusState::Albums | ArtistFocusState::RelatedArtists => None,
            },
            Self::Album {
                track_selection, ..
            }
            | Self::Tracks {
                track_selection, ..
            } => Some(track_selection),
            Self::Playlist { .. } | Self::Show { .. } | Self::Failed { .. } => None,
        }
    }

    /// Return the mutable adapter for the currently focused Artist track
    /// pane.
    #[allow(dead_code)]
    pub fn focused_track_selection_mut(&mut self) -> Option<&mut ContextTrackSelection> {
        match self {
            Self::Artist {
                top_track_selection,
                liked_track_selection,
                focus,
                ..
            } => match focus {
                ArtistFocusState::TopTracks => Some(top_track_selection),
                ArtistFocusState::LikedSongs => Some(liked_track_selection),
                ArtistFocusState::Albums | ArtistFocusState::RelatedArtists => None,
            },
            Self::Album {
                track_selection, ..
            }
            | Self::Tracks {
                track_selection, ..
            } => Some(track_selection),
            Self::Playlist { .. } | Self::Show { .. } | Self::Failed { .. } => None,
        }
    }
}

impl ContextPageUIState {
    pub fn new_playlist() -> Self {
        Self::Playlist {
            playlist_state: MutablePlaylistState::default(),
        }
    }

    pub fn new_album() -> Self {
        Self::Album {
            track_table: TableState::default(),
            track_selection: ContextTrackSelection::default(),
        }
    }

    pub fn new_artist() -> Self {
        Self::Artist {
            top_track_table: TableState::default(),
            top_track_selection: ContextTrackSelection::default(),
            listenbrainz_pending: None,
            listenbrainz_album_pending: None,
            album_table: TableState::default(),
            related_artist_list: ListState::default(),
            liked_track_table: TableState::default(),
            liked_track_selection: ContextTrackSelection::default(),
            focus: ArtistFocusState::TopTracks,
        }
    }

    pub fn new_tracks() -> Self {
        Self::Tracks {
            track_table: TableState::default(),
            track_selection: ContextTrackSelection::default(),
        }
    }

    pub fn new_show() -> Self {
        Self::Show {
            episode_table: TableState::default(),
        }
    }
}

impl MutableWindowState<'_> {
    pub fn select(&mut self, id: usize) {
        match self {
            Self::List(state) => state.select(Some(id)),
            Self::Table(state) => state.select(Some(id)),
            Self::Scroll(scroll_offset) => {
                **scroll_offset = id;
            }
        }
    }
}

pub trait Focusable {
    fn next(&mut self);
    fn previous(&mut self);
}

impl Focusable for PageState {
    fn next(&mut self) {
        match self {
            Self::Search {
                state: SearchPageUIState { focus, .. },
                ..
            } => focus.next(),
            Self::Library {
                state: LibraryPageUIState { focus, .. },
                ..
            } => focus.next(),
            Self::Context {
                state:
                    Some(ContextPageUIState::Artist {
                        focus,
                        listenbrainz_pending,
                        listenbrainz_album_pending,
                        ..
                    }),
                ..
            } => {
                *listenbrainz_pending = None;
                *listenbrainz_album_pending = None;
                focus.next();
            }
            _ => {}
        }

        // reset the list/table state of the focus window
        if let Some(mut state) = self.focus_window_state_mut() {
            state.select(0);
        }
    }

    fn previous(&mut self) {
        match self {
            Self::Search {
                state: SearchPageUIState { focus, .. },
                ..
            } => focus.previous(),
            Self::Library {
                state: LibraryPageUIState { focus, .. },
                ..
            } => focus.previous(),
            Self::Context {
                state:
                    Some(ContextPageUIState::Artist {
                        focus,
                        listenbrainz_pending,
                        listenbrainz_album_pending,
                        ..
                    }),
                ..
            } => {
                *listenbrainz_pending = None;
                *listenbrainz_album_pending = None;
                focus.previous();
            }
            _ => {}
        }

        // reset the list/table state of the focus window
        if let Some(mut state) = self.focus_window_state_mut() {
            state.select(0);
        }
    }
}

#[cfg(test)]
mod listenbrainz_pending_tests {
    use super::*;

    #[test]
    fn leaving_the_artist_recording_pane_invalidates_pending_intent() {
        let mut page = PageState::Context {
            id: None,
            context_page_type: ContextPageType::CurrentPlaying,
            state: Some(ContextPageUIState::new_artist()),
        };
        let PageState::Context {
            state:
                Some(ContextPageUIState::Artist {
                    listenbrainz_pending,
                    listenbrainz_album_pending,
                    ..
                }),
            ..
        } = &mut page
        else {
            unreachable!()
        };
        *listenbrainz_pending = Some(ListenBrainzPendingIntent {
            request_id: 1,
            context_uri: "spotify:artist:artist".to_owned(),
            recording_mbid: "recording".to_owned(),
            intent: ListenBrainzRecordingIntent::Play,
        });
        *listenbrainz_album_pending = Some(ListenBrainzAlbumPendingIntent {
            request_id: 2,
            context_uri: "spotify:artist:artist".to_owned(),
            release_group_mbid: "release-group".to_owned(),
            intent: ListenBrainzAlbumIntent::OpenPage,
        });

        page.next();

        let PageState::Context {
            state:
                Some(ContextPageUIState::Artist {
                    listenbrainz_pending,
                    listenbrainz_album_pending,
                    focus,
                    ..
                }),
            ..
        } = page
        else {
            unreachable!()
        };
        assert_eq!(focus, ArtistFocusState::LikedSongs);
        assert!(listenbrainz_pending.is_none());
        assert!(listenbrainz_album_pending.is_none());
    }
}

macro_rules! impl_focusable {
	($struct:ty, $([$field:ident, $next_field:ident]),+) => {
		impl Focusable for $struct {
            fn next(&mut self) {
                *self = match self {
                    $(
                        Self::$field => Self::$next_field,
                    )+
                };
            }

            fn previous(&mut self) {
                *self = match self {
                    $(
                        Self::$next_field => Self::$field,
                    )+
                };
            }
        }
	};
}

impl_focusable!(
    LibraryFocusState,
    [Playlists, SavedAlbums],
    [SavedAlbums, FollowedArtists],
    [FollowedArtists, Playlists]
);

impl_focusable!(
    ArtistFocusState,
    [TopTracks, LikedSongs],
    [LikedSongs, Albums],
    [Albums, RelatedArtists],
    [RelatedArtists, TopTracks]
);

impl_focusable!(
    SearchFocusState,
    [Category, Input],
    [Input, Tracks],
    [Tracks, Videos],
    [Videos, Albums],
    [Albums, Artists],
    [Artists, Playlists],
    [Playlists, Shows],
    [Shows, Episodes],
    [Episodes, Category]
);

#[cfg(test)]
mod mutable_playlist_page_tests {
    use super::*;
    use rspotify::model::PlaylistId;

    fn pages() -> Vec<PageState> {
        vec![
            PageState::new_unified_playlist("unified"),
            PageState::YouTubeContext {
                id: YouTubeContextId::Playlist("youtube".to_owned()),
                context: None,
                state: YouTubeContextPageUIState::new(),
            },
            PageState::Context {
                id: Some(ContextId::Playlist(
                    PlaylistId::from_id("37i9dQZF1DXcBWIGoYBM5M").unwrap(),
                )),
                context_page_type: ContextPageType::Browsing(ContextId::Playlist(
                    PlaylistId::from_id("37i9dQZF1DXcBWIGoYBM5M").unwrap(),
                )),
                state: Some(ContextPageUIState::new_playlist()),
            },
        ]
    }

    #[test]
    fn all_three_playlist_contexts_share_page_owned_cursor_state() {
        for mut page in pages() {
            page.mutable_playlist_state_mut()
                .unwrap()
                .set_cursor_occurrence(Some(ProviderOccurrenceToken::Unified(PlaylistEntryId(7))));
            page.select(3);
            let state = page.mutable_playlist_state().unwrap();
            assert_eq!(state.table().selected(), Some(3));
            assert_eq!(state.cursor_occurrence(), None);
        }
    }

    #[test]
    fn all_three_playlist_contexts_use_the_same_selection_adapter() {
        for mut page in pages() {
            assert!(matches!(
                page.selection_adapter_mut(Some(ContextTrackPane::Playlist)),
                Some(PageSelectionAdapter::MutablePlaylist(_))
            ));
        }
    }
}
