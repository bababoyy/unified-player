#[allow(dead_code)] // The frozen planner model intentionally exposes API for later 10C consumers.
mod bulk_action;

#[allow(unused_imports)] // Keep the planner model available as one coherent command-layer API.
pub use bulk_action::{
    available_bulk_action_descriptors, describe_bulk_actions, plan_bulk_action,
    plan_structural_block_move, BulkActionCandidates, BulkActionCapability, BulkActionIdentity,
    BulkActionItem, BulkActionOutcome, BulkActionOverall, BulkActionOwner, BulkActionOwnerSummary,
    BulkActionPlan, BulkActionPlanError, BulkActionSemantics, BulkActionSummary,
    BulkOperationFailure, BulkOperationId, BulkOperationPartition, BulkOperationPlan,
    BulkOperationState, BulkOperationTerminal, BulkOutcomeCount, BulkOutcomeTransitionError,
    StructuralBlockMovePlan, StructuralMoveHandle, StructuralMovePlanError,
};

/// Resolve the one-based numeric shortcuts rendered by every action popup.
pub(crate) fn one_based_digit_index(character: char, item_count: usize) -> Option<usize> {
    let index = character.to_digit(10)?.checked_sub(1)? as usize;
    (index < item_count).then_some(index)
}

use crate::{
    config::ActiveProvider,
    state::{
        Album, Artist, DataReadGuard, Episode, Playlist, PlaylistFolder, PlaylistFolderItem, Show,
        Track, YouTubeTrack,
    },
};
use serde::Deserialize;

#[derive(Copy, Clone, Debug, Deserialize, PartialEq, Eq, PartialOrd, Ord)]
/// Application's command
pub enum Command {
    None,

    NextTrack,
    PreviousTrack,
    ResumePause,
    PlayRandom,
    Repeat,
    Shuffle,
    VolumeChange {
        offset: i32,
    },
    Mute,
    SeekStart,
    SeekForward {
        duration: Option<u16>,
    },
    SeekBackward {
        duration: Option<u16>,
    },

    Quit,
    OpenCommandHelp,
    ClosePopup,

    SelectNextOrScrollDown,
    SelectPreviousOrScrollUp,
    PageSelectNextOrScrollDown,
    PageSelectPreviousOrScrollUp,
    SelectFirstOrScrollToTop,
    SelectLastOrScrollToBottom,
    ExtendSelectionNext,
    ExtendSelectionPrevious,
    SelectAll,
    InvertSelection,

    JumpToCurrentTrackInContext,
    ChooseSelected,

    RefreshPlayback,

    #[cfg(feature = "streaming")]
    RestartIntegratedClient,

    FocusNextWindow,
    FocusPreviousWindow,

    SwitchTheme,
    SwitchDevice,
    SwitchProvider,
    SwitchPlaybackProvider,
    OpenAccountSelector,
    ImportYouTubeAuthFromClipboard,
    Search,
    Queue,

    ShowActionsOnSelectedItem,
    ShowActionsOnCurrentTrack,
    ShowActionsOnCurrentContext,
    AddSelectedItemToQueue,
    JumpToHighlightTrackInContext,

    BrowseUserPlaylists,
    BrowseUserFollowedArtists,
    BrowseUserSavedAlbums,

    CurrentlyPlayingContextPage,
    TopTrackPage,
    RecentlyPlayedTrackPage,
    LikedTrackPage,
    LyricsPage,
    ToggleLyricsFollow,
    RetryLyrics,
    CycleLyricsSource,
    SettingsPage,
    LibraryPage,
    JournalPage,
    JournalListsPage,
    SessionHistoryPage,
    CreatePlaylistFromSessionHistory,
    SearchPage,
    BrowsePage,
    PreviousPage,
    OpenSpotifyLinkFromClipboard,

    SortTrackByTitle,
    SortTrackByArtists,
    SortTrackByAlbum,
    SortTrackByDuration,
    SortTrackByAddedDate,
    ReverseTrackOrder,

    SortLibraryAlphabetically,
    SortLibraryByRecent,

    MovePlaylistItemUp,
    MovePlaylistItemDown,

    CreatePlaylist,
    LinkUnifiedPlaylistToYouTube,
    SyncUnifiedPlaylistToYouTube,
    UnlinkUnifiedPlaylistFromYouTube,
    RenameJournalList,
    DeleteJournalList,
    OpenLogs,
}

#[derive(Clone, Copy, Debug, Deserialize, PartialEq, Eq)]
pub enum Action {
    GoToArtist,
    GoToAlbum,
    GoToRadio,
    GoToShow,
    AddToLibrary,
    AddToPlaylist,
    AddToQueue,
    AddToLiked,
    AddToJournalList,
    AddAlbumToJournalList,
    AddArtistTracksToJournalList,
    AddToListenLater,
    MarkListened,
    MarkUnlistened,
    DeleteFromLiked,
    DeleteFromLibrary,
    RemovePlaylistFromLibrary,
    RenamePlaylist,
    DeletePlaylist,
    DeleteFromPlaylist,
    EditNote,
    ClearNote,
    RemoveFromJournal,
    RemoveFromJournalList,
    RemoveFromListenLater,
    SetRating,
    ShowActionsOnAlbum,
    ShowActionsOnArtist,
    ShowActionsOnShow,
    ShowJournalActions,
    ToggleLiked,
    CopyLink,
    CopyLyrics,
    CopyTimedLyrics,
    RetryLyrics,
    CycleLyricsSource,
    BackupUnifiedPlaylistToListenBrainz,
    CheckUnifiedPlaylistListenBrainzSync,
    RefreshUnifiedPlaylistListenBrainzSync,
    RetryUnifiedPlaylistListenBrainzSync,
    PreviewUnifiedPlaylistListenBrainzPush,
    PreviewUnifiedPlaylistListenBrainzPull,
    ReviewUnifiedPlaylistListenBrainzConflicts,
    RecoverUnifiedPlaylistListenBrainzSync,
    RollbackUnifiedPlaylistListenBrainzPull,
    OpenUnifiedPlaylistListenBrainzSync,
    InitializeUnifiedPlaylistListenBrainzBase,
    ApplyUnifiedPlaylistListenBrainzPush,
    ApplyUnifiedPlaylistListenBrainzPull,
    ApplyUnifiedPlaylistListenBrainzResolve,
    LinkUnifiedPlaylistToYouTube,
    SyncUnifiedPlaylistToYouTube,
    UnlinkUnifiedPlaylistFromYouTube,
    Follow,
    Unfollow,
    ClearSessionHistory,
}

/// Every action understood by the command/configuration layer.
///
/// Context-specific constructors decide which of these actions are available
/// for a row. Keeping the complete inventory here gives help and documentation
/// checks one stable source without pretending every provider supports every
/// action.
#[allow(dead_code)] // Also serves the documentation/source-contract checks.
pub const ALL_ACTIONS: &[Action] = &[
    Action::GoToArtist,
    Action::GoToAlbum,
    Action::GoToRadio,
    Action::GoToShow,
    Action::AddToLibrary,
    Action::AddToPlaylist,
    Action::AddToQueue,
    Action::AddToLiked,
    Action::AddToJournalList,
    Action::AddAlbumToJournalList,
    Action::AddArtistTracksToJournalList,
    Action::AddToListenLater,
    Action::MarkListened,
    Action::MarkUnlistened,
    Action::DeleteFromLiked,
    Action::DeleteFromLibrary,
    Action::RemovePlaylistFromLibrary,
    Action::RenamePlaylist,
    Action::DeletePlaylist,
    Action::DeleteFromPlaylist,
    Action::EditNote,
    Action::ClearNote,
    Action::RemoveFromJournal,
    Action::RemoveFromJournalList,
    Action::RemoveFromListenLater,
    Action::SetRating,
    Action::ShowActionsOnAlbum,
    Action::ShowActionsOnArtist,
    Action::ShowActionsOnShow,
    Action::ShowJournalActions,
    Action::ToggleLiked,
    Action::CopyLink,
    Action::CopyLyrics,
    Action::CopyTimedLyrics,
    Action::RetryLyrics,
    Action::CycleLyricsSource,
    Action::BackupUnifiedPlaylistToListenBrainz,
    Action::CheckUnifiedPlaylistListenBrainzSync,
    Action::RefreshUnifiedPlaylistListenBrainzSync,
    Action::RetryUnifiedPlaylistListenBrainzSync,
    Action::PreviewUnifiedPlaylistListenBrainzPush,
    Action::PreviewUnifiedPlaylistListenBrainzPull,
    Action::ReviewUnifiedPlaylistListenBrainzConflicts,
    Action::RecoverUnifiedPlaylistListenBrainzSync,
    Action::RollbackUnifiedPlaylistListenBrainzPull,
    Action::OpenUnifiedPlaylistListenBrainzSync,
    Action::InitializeUnifiedPlaylistListenBrainzBase,
    Action::ApplyUnifiedPlaylistListenBrainzPush,
    Action::ApplyUnifiedPlaylistListenBrainzPull,
    Action::ApplyUnifiedPlaylistListenBrainzResolve,
    Action::LinkUnifiedPlaylistToYouTube,
    Action::SyncUnifiedPlaylistToYouTube,
    Action::UnlinkUnifiedPlaylistFromYouTube,
    Action::Follow,
    Action::Unfollow,
    Action::ClearSessionHistory,
];

#[derive(Debug)]
pub enum ActionContext {
    Track(Track),
    YouTubeTrack(YouTubeTrack),
    YouTubeTracks(Vec<YouTubeTrack>),
    Tracks(Vec<Track>),
    Album(Album),
    Artist(Artist),
    Playlist(Playlist),
    Episode(Episode),
    #[allow(dead_code)]
    // TODO: support actions for playlist folders
    PlaylistFolder(PlaylistFolder),
    Show(Show),
}

#[derive(Debug, PartialEq, Eq, Clone, Deserialize, Default, Copy)]
pub enum ActionTarget {
    PlayingTrack,
    #[default]
    SelectedItem,
}

impl ActionTarget {
    pub const fn label(self) -> &'static str {
        match self {
            Self::PlayingTrack => "playing track",
            Self::SelectedItem => "selected item",
        }
    }
}

#[allow(dead_code)] // Availability is populated when a context-specific action list is built.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ActionAvailability {
    Available,
    Unavailable,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ActionConfirmation {
    None,
    Required,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ActionDescriptor {
    pub action: Action,
    pub target: ActionTarget,
    pub availability: ActionAvailability,
    pub confirmation: ActionConfirmation,
    pub label: &'static str,
    pub description: &'static str,
}

impl ActionDescriptor {
    pub const fn with_target(self, target: ActionTarget) -> Self {
        Self { target, ..self }
    }
}

impl Action {
    pub const fn descriptor(self) -> ActionDescriptor {
        let (label, description) = match self {
            Self::GoToArtist => ("Open artist", "Open the item's artist page"),
            Self::GoToAlbum => ("Open album", "Open the item's album page"),
            Self::GoToRadio => ("Start radio", "Start a radio context from this item"),
            Self::GoToShow => ("Open show", "Open the item's show page"),
            Self::AddToLibrary => ("Save to library", "Save this item to your library"),
            Self::AddToPlaylist => ("Add to playlist", "Add this item to a playlist"),
            Self::AddToQueue => ("Add to queue", "Add this item to the playback queue"),
            Self::AddToLiked => ("Like", "Add this item to liked items"),
            Self::AddToJournalList => ("Add to journal list", "Add this track to a journal list"),
            Self::AddAlbumToJournalList => (
                "Add album to journal list",
                "Add every album track to a journal list",
            ),
            Self::AddArtistTracksToJournalList => (
                "Add artist tracks to journal list",
                "Add the artist's tracks to a journal list",
            ),
            Self::AddToListenLater => {
                ("Add to listen later", "Mark this track for later listening")
            }
            Self::MarkListened => ("Mark listened", "Mark this track as listened"),
            Self::MarkUnlistened => ("Mark unlistened", "Mark this track as unlistened"),
            Self::DeleteFromLiked => ("Remove like", "Remove this item from liked items"),
            Self::DeleteFromLibrary => {
                ("Remove from library", "Remove this item from your library")
            }
            Self::RemovePlaylistFromLibrary => (
                "Remove playlist from library",
                "Unfollow this Spotify playlist without deleting it for its owner",
            ),
            Self::RenamePlaylist => ("Rename playlist", "Change the playlist name"),
            Self::DeletePlaylist => (
                "Delete playlist",
                "Delete this playlist; linked local/provider copies remain separate",
            ),
            Self::DeleteFromPlaylist => {
                ("Remove from playlist", "Remove this item from the playlist")
            }
            Self::EditNote => ("Edit note", "Edit the journal note for this track"),
            Self::ClearNote => ("Clear note", "Remove the journal note for this track"),
            Self::RemoveFromJournal => {
                ("Remove from journal", "Remove this track from the journal")
            }
            Self::RemoveFromJournalList => (
                "Remove from journal list",
                "Remove this track from the journal list",
            ),
            Self::RemoveFromListenLater => (
                "Remove from listen later",
                "Remove this track from listen later",
            ),
            Self::SetRating => ("Set rating", "Set or clear the journal rating"),
            Self::ShowActionsOnAlbum => ("Album actions", "Open actions for the item's album"),
            Self::ShowActionsOnArtist => ("Artist actions", "Open actions for the item's artist"),
            Self::ShowActionsOnShow => ("Show actions", "Open actions for the item's show"),
            Self::ShowJournalActions => ("Journal actions", "Open journal actions for this track"),
            Self::ToggleLiked => ("Toggle like", "Add or remove this item from liked items"),
            Self::CopyLink => ("Copy link", "Copy a safe provider link for this item"),
            Self::CopyLyrics => ("Copy lyrics", "Copy the current lyrics as plain text"),
            Self::CopyTimedLyrics => (
                "Copy timed lyrics",
                "Copy synchronized lyrics as an LRC file",
            ),
            Self::RetryLyrics => ("Retry lyrics", "Reload lyrics for the current track"),
            Self::CycleLyricsSource => (
                "Try another lyrics source",
                "Fetch lyrics from the next enabled source",
            ),
            Self::BackupUnifiedPlaylistToListenBrainz => (
                "Back up to ListenBrainz",
                "Create a private ListenBrainz backup for this Unified playlist",
            ),
            Self::CheckUnifiedPlaylistListenBrainzSync => (
                "Check for changes",
                "Refresh the read-only ListenBrainz sync summary for this Unified playlist",
            ),
            Self::RefreshUnifiedPlaylistListenBrainzSync => (
                "Refresh the preview",
                "Read the linked ListenBrainz playlist again and replace the current preview",
            ),
            Self::RetryUnifiedPlaylistListenBrainzSync => (
                "Retry the preview",
                "Retry a failed ListenBrainz pre-write preview without repeating a mutation",
            ),
            Self::PreviewUnifiedPlaylistListenBrainzPush => (
                "Review outgoing changes",
                "Review the local-to-ListenBrainz push plan without writing",
            ),
            Self::PreviewUnifiedPlaylistListenBrainzPull => (
                "Review incoming changes",
                "Review the ListenBrainz-to-local pull plan without writing",
            ),
            Self::ReviewUnifiedPlaylistListenBrainzConflicts => (
                "Review conflicts",
                "Inspect occurrence-level ListenBrainz conflicts before choosing a policy",
            ),
            Self::RecoverUnifiedPlaylistListenBrainzSync => (
                "Check an unfinished operation",
                "Read back an incomplete or unknown ListenBrainz operation without retrying it",
            ),
            Self::RollbackUnifiedPlaylistListenBrainzPull => (
                "Undo the last pull",
                "Restore the saved local pre-apply snapshot of the ListenBrainz pull",
            ),
            Self::OpenUnifiedPlaylistListenBrainzSync => (
                "ListenBrainz",
                "Open the ListenBrainz workspace for this Unified playlist",
            ),
            Self::InitializeUnifiedPlaylistListenBrainzBase => (
                "Start sync tracking",
                "Verify the linked ListenBrainz backup and store the initial sync base locally",
            ),
            Self::ApplyUnifiedPlaylistListenBrainzPush => (
                "Send my changes",
                "Write the previewed local state to ListenBrainz after confirmation (push)",
            ),
            Self::ApplyUnifiedPlaylistListenBrainzPull => (
                "Take their changes",
                "Write the previewed remote state locally after confirmation (pull)",
            ),
            Self::ApplyUnifiedPlaylistListenBrainzResolve => (
                "Apply my conflict choices",
                "Choose a conflict policy and apply the resolution after confirmation",
            ),
            Self::LinkUnifiedPlaylistToYouTube => (
                "Link to YouTube Music",
                "Choose the YouTube Music playlist linked to this Unified playlist",
            ),
            Self::SyncUnifiedPlaylistToYouTube => (
                "Sync to YouTube Music",
                "Append missing Unified playlist items to its linked YouTube playlist",
            ),
            Self::UnlinkUnifiedPlaylistFromYouTube => (
                "Unlink YouTube Music playlist",
                "Remove this Unified playlist's YouTube Music link",
            ),
            Self::Follow => ("Follow", "Follow this artist"),
            Self::Unfollow => ("Unfollow", "Stop following this artist"),
            Self::ClearSessionHistory => (
                "Clear all history",
                "Delete every locally stored session-history entry",
            ),
        };
        let confirmation = match self {
            Self::ClearNote
            | Self::DeleteFromLiked
            | Self::DeleteFromLibrary
            | Self::RemovePlaylistFromLibrary
            | Self::DeletePlaylist
            | Self::DeleteFromPlaylist
            | Self::RemoveFromJournal
            | Self::RemoveFromJournalList
            | Self::RemoveFromListenLater
            | Self::RollbackUnifiedPlaylistListenBrainzPull
            | Self::Unfollow
            | Self::ClearSessionHistory => ActionConfirmation::Required,
            _ => ActionConfirmation::None,
        };
        ActionDescriptor {
            action: self,
            target: ActionTarget::SelectedItem,
            availability: ActionAvailability::Available,
            confirmation,
            label,
            description,
        }
    }

    pub const fn is_journal_action(self) -> bool {
        matches!(
            self,
            Self::AddToJournalList
                | Self::AddAlbumToJournalList
                | Self::AddArtistTracksToJournalList
                | Self::AddToListenLater
                | Self::MarkListened
                | Self::MarkUnlistened
                | Self::EditNote
                | Self::ClearNote
                | Self::RemoveFromJournal
                | Self::RemoveFromJournalList
                | Self::RemoveFromListenLater
                | Self::SetRating
        )
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CommandOrAction {
    Command(Command),
    Action(Action, ActionTarget),
}

impl From<Track> for ActionContext {
    fn from(v: Track) -> Self {
        Self::Track(v)
    }
}

impl From<YouTubeTrack> for ActionContext {
    fn from(v: YouTubeTrack) -> Self {
        Self::YouTubeTrack(v)
    }
}

impl From<Artist> for ActionContext {
    fn from(v: Artist) -> Self {
        Self::Artist(v)
    }
}

impl From<Album> for ActionContext {
    fn from(v: Album) -> Self {
        Self::Album(v)
    }
}

impl From<Playlist> for ActionContext {
    fn from(v: Playlist) -> Self {
        Self::Playlist(v)
    }
}

impl From<Show> for ActionContext {
    fn from(v: Show) -> Self {
        Self::Show(v)
    }
}

impl From<Episode> for ActionContext {
    fn from(v: Episode) -> Self {
        Self::Episode(v)
    }
}

impl From<PlaylistFolderItem> for ActionContext {
    fn from(value: PlaylistFolderItem) -> Self {
        match value {
            PlaylistFolderItem::Playlist(p) => ActionContext::Playlist(p),
            PlaylistFolderItem::Folder(f) => ActionContext::PlaylistFolder(f),
        }
    }
}

impl ActionContext {
    pub fn get_available_actions(&self, data: &DataReadGuard) -> Vec<Action> {
        match self {
            Self::Track(track) => construct_track_actions(track, data),
            Self::YouTubeTrack(_) | Self::YouTubeTracks(_) => construct_youtube_track_actions(),
            Self::Tracks(_) => construct_tracks_actions(),
            Self::Album(album) => construct_album_actions(album, data),
            Self::Artist(artist) => construct_artist_actions(artist, data),
            Self::Playlist(playlist) => construct_playlist_actions(playlist, data),
            Self::Episode(episode) => construct_episode_actions(episode, data),
            // TODO: support actions for playlist folders
            Self::PlaylistFolder(_) => vec![],
            Self::Show(show) => construct_show_actions(show, data),
        }
    }
}

pub fn construct_youtube_track_actions() -> Vec<Action> {
    provider_capabilities(ActiveProvider::YouTubeMusic)
        .track_actions()
        .iter()
        .copied()
        .filter(|action| *action != Action::DeleteFromLiked)
        .collect()
}

/// Actions for the provider's liked context. Every row is already liked, so
/// expose the inverse mutation instead of offering a no-op Like action.
pub fn construct_youtube_liked_track_actions() -> Vec<Action> {
    construct_youtube_track_actions()
        .into_iter()
        .map(|action| {
            if action == Action::AddToLiked {
                Action::DeleteFromLiked
            } else {
                action
            }
        })
        .collect()
}

/// Provider-level action capabilities shared by search and item surfaces.
/// Item-specific state (for example Spotify liked/unliked state) is still
/// resolved by the existing constructors; this registry only describes the
/// provider boundary.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ProviderCapabilities {
    provider: ActiveProvider,
    track_actions: &'static [Action],
    search_panes: &'static [ProviderSearchPane],
}

/// Provider-owned search sections. The UI maps these stable descriptors to
/// its focus enum, keeping visible-pane policy out of event dispatch.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ProviderSearchPane {
    Tracks,
    Videos,
    Albums,
    Artists,
    Playlists,
    Shows,
    Episodes,
}

const SPOTIFY_TRACK_ACTIONS: &[Action] = &[
    Action::GoToArtist,
    Action::GoToAlbum,
    Action::GoToRadio,
    Action::CopyLink,
    Action::AddToPlaylist,
    Action::AddToQueue,
    Action::AddToLiked,
    Action::DeleteFromLiked,
    Action::ShowJournalActions,
];

const YOUTUBE_TRACK_ACTIONS: &[Action] = &[
    Action::CopyLink,
    Action::AddToPlaylist,
    Action::AddToJournalList,
    Action::AddToQueue,
    Action::AddToLiked,
    Action::DeleteFromLiked,
];

const SPOTIFY_SEARCH_PANES: &[ProviderSearchPane] = &[
    ProviderSearchPane::Tracks,
    ProviderSearchPane::Albums,
    ProviderSearchPane::Artists,
    ProviderSearchPane::Playlists,
    ProviderSearchPane::Shows,
    ProviderSearchPane::Episodes,
];

const YOUTUBE_SEARCH_PANES: &[ProviderSearchPane] = &[
    ProviderSearchPane::Tracks,
    ProviderSearchPane::Videos,
    ProviderSearchPane::Albums,
    ProviderSearchPane::Artists,
    ProviderSearchPane::Playlists,
    ProviderSearchPane::Shows,
    ProviderSearchPane::Episodes,
];

pub const fn provider_capabilities(provider: ActiveProvider) -> ProviderCapabilities {
    let (track_actions, search_panes) = match provider {
        ActiveProvider::Spotify => (SPOTIFY_TRACK_ACTIONS, SPOTIFY_SEARCH_PANES),
        ActiveProvider::YouTubeMusic => (YOUTUBE_TRACK_ACTIONS, YOUTUBE_SEARCH_PANES),
    };
    ProviderCapabilities {
        provider,
        track_actions,
        search_panes,
    }
}

impl ProviderCapabilities {
    #[allow(dead_code)]
    pub const fn provider(self) -> ActiveProvider {
        self.provider
    }

    pub const fn track_actions(self) -> &'static [Action] {
        self.track_actions
    }

    pub const fn search_panes(self) -> &'static [ProviderSearchPane] {
        self.search_panes
    }

    pub fn supports_track_action(self, action: Action) -> bool {
        self.track_actions.contains(&action)
    }
}

/// constructs a list of actions on multiple tracks
pub fn construct_tracks_actions() -> Vec<Action> {
    vec![
        Action::CopyLink,
        Action::AddToPlaylist,
        Action::AddToQueue,
        Action::AddToLiked,
        Action::AddToListenLater,
    ]
}

/// constructs a list of actions on a track
pub fn construct_track_actions(track: &Track, data: &DataReadGuard) -> Vec<Action> {
    let mut actions = vec![
        Action::GoToArtist,
        Action::GoToAlbum,
        Action::GoToRadio,
        Action::ShowActionsOnAlbum,
        Action::ShowActionsOnArtist,
        Action::CopyLink,
        Action::AddToPlaylist,
        Action::AddToQueue,
    ];

    if data.user_data.is_liked_track(track) {
        actions.push(Action::DeleteFromLiked);
    } else {
        actions.push(Action::AddToLiked);
    }

    actions.push(Action::ShowJournalActions);

    actions
}

pub fn construct_track_journal_actions(track: &Track, data: &DataReadGuard) -> Vec<Action> {
    let mut actions = vec![
        Action::AddToJournalList,
        Action::SetRating,
        Action::EditNote,
    ];
    let journal_entry = data.journal.entry_for_track(track);
    if journal_entry.is_some_and(|entry| !entry.note.is_empty()) {
        actions.push(Action::ClearNote);
    }
    if journal_entry.is_some_and(|entry| entry.listen_later) {
        actions.push(Action::RemoveFromListenLater);
    } else {
        actions.push(Action::AddToListenLater);
    }
    if journal_entry.is_some_and(|entry| entry.listened) {
        actions.push(Action::MarkUnlistened);
    } else {
        actions.push(Action::MarkListened);
    }
    if journal_entry.is_some() {
        actions.push(Action::RemoveFromJournal);
    }
    actions
}

/// constructs a list of actions on an album
pub fn construct_album_actions(album: &Album, data: &DataReadGuard) -> Vec<Action> {
    let mut actions = vec![
        Action::GoToArtist,
        Action::GoToRadio,
        Action::ShowActionsOnArtist,
        Action::CopyLink,
        Action::AddToQueue,
        Action::AddAlbumToJournalList,
    ];
    if data.user_data.saved_albums.iter().any(|a| a.id == album.id) {
        actions.push(Action::DeleteFromLibrary);
    } else {
        actions.push(Action::AddToLibrary);
    }
    actions
}

/// constructs a list of actions on an artist
pub fn construct_artist_actions(artist: &Artist, data: &DataReadGuard) -> Vec<Action> {
    let mut actions = vec![
        Action::GoToRadio,
        Action::CopyLink,
        Action::AddArtistTracksToJournalList,
    ];

    if data
        .user_data
        .followed_artists
        .iter()
        .any(|a| a.id == artist.id)
    {
        actions.push(Action::Unfollow);
    } else {
        actions.push(Action::Follow);
    }
    actions
}

/// constructs a list of actions on an playlist
pub fn construct_playlist_actions(playlist: &Playlist, data: &DataReadGuard) -> Vec<Action> {
    let mut actions = vec![Action::GoToRadio, Action::CopyLink, Action::RenamePlaylist];

    if data
        .user_data
        .playlists
        .iter()
        .any(|item| matches!(item, PlaylistFolderItem::Playlist(p) if p.id == playlist.id))
    {
        actions.push(Action::RemovePlaylistFromLibrary);
    } else {
        actions.push(Action::AddToLibrary);
    }
    actions
}

/// constructs a list of actions on a show
pub fn construct_show_actions(show: &Show, data: &DataReadGuard) -> Vec<Action> {
    let mut actions = vec![Action::CopyLink];
    if data.user_data.saved_shows.iter().any(|s| s.id == show.id) {
        actions.push(Action::DeleteFromLibrary);
    } else {
        actions.push(Action::AddToLibrary);
    }
    actions
}

/// constructs a list of actions on an episode
pub fn construct_episode_actions(episode: &Episode, _data: &DataReadGuard) -> Vec<Action> {
    let mut actions = vec![Action::CopyLink, Action::AddToPlaylist, Action::AddToQueue];
    if episode.show.is_some() {
        actions.push(Action::ShowActionsOnShow);
        actions.push(Action::GoToShow);
    }
    actions
}

impl Command {
    pub const fn label(self) -> &'static str {
        match self {
            Self::None => "No action",
            Self::NextTrack => "Next track",
            Self::PreviousTrack => "Previous track",
            Self::ResumePause => "Pause or resume",
            Self::PlayRandom => "Play random",
            Self::Repeat => "Repeat",
            Self::Shuffle => "Shuffle",
            Self::VolumeChange { .. } => "Change volume",
            Self::Mute => "Mute",
            Self::SeekStart => "Seek to start",
            Self::SeekForward { .. } => "Seek forward",
            Self::SeekBackward { .. } => "Seek backward",
            Self::Quit => "Quit",
            Self::OpenCommandHelp => "Command help",
            Self::ClosePopup => "Close popup",
            Self::SelectNextOrScrollDown => "Next item or scroll down",
            Self::SelectPreviousOrScrollUp => "Previous item or scroll up",
            Self::PageSelectNextOrScrollDown => "Next page or scroll down",
            Self::PageSelectPreviousOrScrollUp => "Previous page or scroll up",
            Self::SelectFirstOrScrollToTop => "First item or top",
            Self::SelectLastOrScrollToBottom => "Last item or bottom",
            Self::ExtendSelectionNext => "Extend selection down",
            Self::ExtendSelectionPrevious => "Extend selection up",
            Self::SelectAll => "Select all",
            Self::InvertSelection => "Invert selection",
            Self::JumpToCurrentTrackInContext => "Jump to current track",
            Self::ChooseSelected => "Choose selected",
            Self::RefreshPlayback => "Refresh playback",
            #[cfg(feature = "streaming")]
            Self::RestartIntegratedClient => "Restart integrated client",
            Self::FocusNextWindow => "Next window",
            Self::FocusPreviousWindow => "Previous window",
            Self::SwitchTheme => "Switch theme",
            Self::SwitchDevice => "Switch device",
            Self::SwitchProvider => "Switch provider",
            Self::SwitchPlaybackProvider => "Switch playback provider",
            Self::OpenAccountSelector => "Open account selector",
            Self::ImportYouTubeAuthFromClipboard => "Import YouTube authentication",
            Self::Search => "Search or filter",
            Self::Queue => "Open queue",
            Self::ShowActionsOnSelectedItem => "Actions on selected item",
            Self::ShowActionsOnCurrentTrack => "Actions on playing track",
            Self::ShowActionsOnCurrentContext => "Actions on current context",
            Self::AddSelectedItemToQueue => "Add selected item to queue",
            Self::JumpToHighlightTrackInContext => "Jump to highlighted track",
            Self::BrowseUserPlaylists => "Browse user playlists",
            Self::BrowseUserFollowedArtists => "Browse followed artists",
            Self::BrowseUserSavedAlbums => "Browse saved albums",
            Self::CurrentlyPlayingContextPage => "Current context page",
            Self::TopTrackPage => "Top tracks page",
            Self::RecentlyPlayedTrackPage => "Recently played page",
            Self::LikedTrackPage => "Liked tracks page",
            Self::LyricsPage => "Lyrics page",
            Self::ToggleLyricsFollow => "Toggle lyrics follow",
            Self::RetryLyrics => "Retry lyrics",
            Self::CycleLyricsSource => "Cycle lyrics source",
            Self::SettingsPage => "Settings page",
            Self::LibraryPage => "Library page",
            Self::JournalPage => "Journal page",
            Self::JournalListsPage => "Journal lists page",
            Self::SessionHistoryPage => "Session history",
            Self::CreatePlaylistFromSessionHistory => "Playlist from session history",
            Self::SearchPage => "Search page",
            Self::BrowsePage => "Browse page",
            Self::PreviousPage => "Previous page",
            Self::OpenSpotifyLinkFromClipboard => "Open Spotify link",
            Self::SortTrackByTitle => "Sort by title",
            Self::SortTrackByArtists => "Sort by artists",
            Self::SortTrackByAlbum => "Sort by album",
            Self::SortTrackByDuration => "Sort by duration",
            Self::SortTrackByAddedDate => "Sort by added date",
            Self::ReverseTrackOrder => "Reverse track order",
            Self::SortLibraryAlphabetically => "Sort library alphabetically",
            Self::SortLibraryByRecent => "Sort library by recent",
            Self::MovePlaylistItemUp => "Move playlist item up",
            Self::MovePlaylistItemDown => "Move playlist item down",
            Self::CreatePlaylist => "Create playlist",
            Self::LinkUnifiedPlaylistToYouTube => "Link Unified playlist to YouTube",
            Self::SyncUnifiedPlaylistToYouTube => "Sync Unified playlist to YouTube",
            Self::UnlinkUnifiedPlaylistFromYouTube => "Unlink Unified playlist from YouTube",
            Self::RenameJournalList => "Rename journal list",
            Self::DeleteJournalList => "Delete journal list",
            Self::OpenLogs => "Open diagnostics",
        }
    }

    pub fn desc(self) -> String {
        if let Self::VolumeChange { offset } = self {
            return format!("change playback volume by {offset}");
        }

        match self {
            Self::None => "do nothing",
            Self::NextTrack => "next track",
            Self::PreviousTrack => "previous track",
            Self::ResumePause => "resume/pause based on the current playback",
            Self::PlayRandom => "play a random track in the current context",
            Self::Repeat => "cycle the repeat mode",
            Self::Shuffle => "toggle the shuffle mode",
            Self::Mute => "toggle playback volume between 0% and previous level",
            Self::SeekStart => "seek to track start",
            Self::SeekForward { duration } => { return format!("seek forward by {}s", duration.unwrap_or(5)) },
            Self::SeekBackward { duration } => { return format!("seek backward by {}s", duration.unwrap_or(5)) },
            Self::Quit => "quit the application",
            Self::ClosePopup => "close a popup",
            #[cfg(feature = "streaming")]
            Self::RestartIntegratedClient => "restart the integrated client",
            Self::SelectNextOrScrollDown => "select the next item in a list/table or scroll down (supports vim-style count: 5j)",
            Self::SelectPreviousOrScrollUp => {
                "select the previous item in a list/table or scroll up (supports vim-style count: 10k)"
            }
            Self::PageSelectNextOrScrollDown => {
                "select the next page item in a list/table or scroll a page down (supports vim-style count: 3C-f)"
            }
            Self::PageSelectPreviousOrScrollUp => {
                "select the previous page item in a list/table or scroll a page up (supports vim-style count: 2C-b)"
            }
            Self::SelectFirstOrScrollToTop => {
                "select the first item in a list/table or scroll to the top"
            }
            Self::SelectLastOrScrollToBottom => {
                "select the last item in a list/table or scroll to the bottom"
            }
            Self::ExtendSelectionNext => "extend selection to the next item in a track list/table",
            Self::ExtendSelectionPrevious => {
                "extend selection to the previous item in a track list/table"
            }
            Self::SelectAll => "select every visible item in the current keyed track pane",
            Self::InvertSelection => {
                "invert the visible selection in the current keyed track pane"
            }
            Self::ChooseSelected => "choose the selected item and act on it",
            Self::JumpToCurrentTrackInContext => "jump to the current track in the context",
            Self::RefreshPlayback => "manually refresh the current playback",
            Self::ShowActionsOnSelectedItem => "open a popup showing actions on a selected item",
            Self::ShowActionsOnCurrentTrack => "open a popup showing actions on the current track",
            Self::ShowActionsOnCurrentContext => "open a popup showing actions on the current context",
            Self::AddSelectedItemToQueue => "add the selected item to queue",
            Self::JumpToHighlightTrackInContext => "jump to the currently highlighted search result in the context",
            Self::FocusNextWindow => "focus the next focusable window (if any)",
            Self::FocusPreviousWindow => "focus the previous focusable window (if any)",
            Self::SwitchTheme => "open a popup for switching theme",
            Self::SwitchDevice => "open a popup for switching device",
            Self::SwitchProvider => "switch between Spotify and YouTube Music modes",
            Self::SwitchPlaybackProvider => {
                "transfer playback ownership between Spotify and YouTube Music"
            }
            Self::OpenAccountSelector => {
                "open the account selector for the active browsing provider"
            }
            Self::Search => "open a popup for searching in the current page",
            Self::BrowseUserPlaylists => "open a popup for browsing user's playlists",
            Self::BrowseUserFollowedArtists => "open a popup for browsing user's followed artists",
            Self::BrowseUserSavedAlbums => "open a popup for browsing user's saved albums",
            Self::CurrentlyPlayingContextPage => "go to the currently playing context page",
            Self::TopTrackPage => "go to the user top track page",
            Self::RecentlyPlayedTrackPage => "go to the user recently played track page",
            Self::LikedTrackPage => "go to the user liked track page",
            Self::LyricsPage => "go to the lyrics page of the current track",
            Self::ToggleLyricsFollow => "toggle automatic lyrics follow mode",
            Self::RetryLyrics => "reload lyrics for the current lyrics page",
            Self::CycleLyricsSource => "try the next enabled lyrics provider",
            Self::SettingsPage => "go to the settings page",
            Self::LibraryPage => "go to the user library page",
            Self::JournalPage => "go to the track journal page",
            Self::JournalListsPage => "go to the journal lists page",
            Self::SessionHistoryPage => "open local playback history",
            Self::CreatePlaylistFromSessionHistory => {
                "create a local unified playlist from session history"
            }
            Self::SearchPage => "go to the search page",
            Self::BrowsePage => "go to the browse page",
            Self::Queue => "go to the queue page",
            Self::OpenCommandHelp => "go to the command help page",
            Self::PreviousPage => "go to the previous page",
            Self::OpenSpotifyLinkFromClipboard => "open a Spotify link from clipboard",
            Self::ImportYouTubeAuthFromClipboard => {
                "import YouTube Music browser cookie or OAuth JSON from clipboard"
            }
            Self::SortTrackByTitle => "sort the track table (if any) by track's title",
            Self::SortTrackByArtists => "sort the track table (if any) by track's artists",
            Self::SortTrackByAlbum => "sort the track table (if any) by track's album",
            Self::SortTrackByDuration => "sort the track table (if any) by track's duration",
            Self::SortTrackByAddedDate => "sort the track table (if any) by track's added date",
            Self::ReverseTrackOrder => "reverse the order of the track table (if any)",
            Self::SortLibraryAlphabetically => "sort the library alphabetically",
            Self::SortLibraryByRecent => {
                "sort the library (playlists and albums) by recently added items"
            }
            Self::MovePlaylistItemUp => "move playlist item up one position",
            Self::MovePlaylistItemDown => "move playlist item down one position",
            Self::CreatePlaylist => "create a new playlist",
            Self::LinkUnifiedPlaylistToYouTube => {
                "link the current Unified playlist to an existing YouTube playlist"
            }
            Self::SyncUnifiedPlaylistToYouTube => {
                "append missing items from the current Unified playlist to its linked YouTube playlist"
            }
            Self::UnlinkUnifiedPlaylistFromYouTube => {
                "remove the current Unified playlist's YouTube link"
            }
            Self::RenameJournalList => "rename a journal list",
            Self::DeleteJournalList => "delete a journal list",
            Self::VolumeChange { offset: _ } => unreachable!(),
            Self::OpenLogs => "open live diagnostics",
        }
        .to_string()
    }
}

#[cfg(test)]
mod descriptor_tests {
    use super::{Action, ActionAvailability, ActionConfirmation, Command, ALL_ACTIONS};
    use crate::config::ActiveProvider;

    #[test]
    fn action_descriptors_are_human_readable_and_typed() {
        let actions = [
            Action::GoToArtist,
            Action::GoToAlbum,
            Action::GoToRadio,
            Action::GoToShow,
            Action::AddToLibrary,
            Action::AddToPlaylist,
            Action::AddToQueue,
            Action::AddToLiked,
            Action::AddToJournalList,
            Action::AddAlbumToJournalList,
            Action::AddArtistTracksToJournalList,
            Action::AddToListenLater,
            Action::MarkListened,
            Action::MarkUnlistened,
            Action::DeleteFromLiked,
            Action::DeleteFromLibrary,
            Action::DeleteFromPlaylist,
            Action::EditNote,
            Action::ClearNote,
            Action::RemoveFromJournal,
            Action::RemoveFromJournalList,
            Action::RemoveFromListenLater,
            Action::SetRating,
            Action::ShowActionsOnAlbum,
            Action::ShowActionsOnArtist,
            Action::ShowActionsOnShow,
            Action::ShowJournalActions,
            Action::ToggleLiked,
            Action::CopyLink,
            Action::Follow,
            Action::Unfollow,
        ];
        for action in actions {
            let descriptor = action.descriptor();
            assert_eq!(descriptor.action, action);
            assert_eq!(descriptor.availability, ActionAvailability::Available);
            assert!(!descriptor.label.is_empty());
            assert!(!descriptor.description.is_empty());
            assert!(!descriptor.label.contains("::"));
        }
        assert_eq!(
            Action::DeleteFromLibrary.descriptor().confirmation,
            ActionConfirmation::Required
        );
        assert_eq!(
            Action::AddToQueue.descriptor().confirmation,
            ActionConfirmation::None
        );
    }

    #[test]
    fn command_labels_are_not_debug_enum_names() {
        assert_eq!(Command::OpenLogs.label(), "Open diagnostics");
        assert_eq!(
            Command::ShowActionsOnSelectedItem.label(),
            "Actions on selected item"
        );
        assert_eq!(Command::ToggleLyricsFollow.label(), "Toggle lyrics follow");
        assert_eq!(Command::RetryLyrics.label(), "Retry lyrics");
        assert_eq!(Command::CycleLyricsSource.label(), "Cycle lyrics source");
    }

    #[test]
    fn provider_capabilities_keep_search_and_item_boundaries_aligned() {
        let youtube = super::provider_capabilities(ActiveProvider::YouTubeMusic);
        assert_eq!(youtube.provider(), ActiveProvider::YouTubeMusic);
        assert!(youtube.supports_track_action(Action::AddToLiked));
        assert!(youtube.supports_track_action(Action::DeleteFromLiked));
        assert!(youtube.supports_track_action(Action::CopyLink));
        assert!(youtube.supports_track_action(Action::AddToJournalList));
        assert!(!youtube.supports_track_action(Action::GoToArtist));
        assert!(youtube
            .search_panes()
            .contains(&super::ProviderSearchPane::Videos));

        let spotify = super::provider_capabilities(ActiveProvider::Spotify);
        assert!(spotify.supports_track_action(Action::GoToArtist));
        assert!(spotify.supports_track_action(Action::DeleteFromLiked));
        assert!(!spotify
            .search_panes()
            .contains(&super::ProviderSearchPane::Videos));
    }

    #[test]
    fn youtube_liked_actions_replace_like_with_unlike() {
        let normal = super::construct_youtube_track_actions();
        let liked = super::construct_youtube_liked_track_actions();
        assert!(normal.contains(&Action::AddToLiked));
        assert!(!normal.contains(&Action::DeleteFromLiked));
        assert!(!liked.contains(&Action::AddToLiked));
        assert!(liked.contains(&Action::DeleteFromLiked));
    }

    #[test]
    fn every_action_is_listed_in_commands_doc() {
        let doc = include_str!(concat!(env!("CARGO_MANIFEST_DIR"), "/../docs/commands.md"));
        let missing = ALL_ACTIONS
            .iter()
            .filter(|action| !doc.contains(&format!("- `{:?}`", action)))
            .map(|action| format!("{action:?}"))
            .collect::<Vec<_>>();
        assert!(
            missing.is_empty(),
            "docs/commands.md action list is missing {missing:?}"
        );
    }
}
