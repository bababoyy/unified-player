use crate::state::{
    AlbumId, Category, ContextId, Item, ItemId, ListenBrainzAlbumIntent,
    ListenBrainzRecordingIntent, NativeQueueRefreshGuard, PlayableId, PlayableMedia, Playback,
    PlaylistId, TrackId, YouTubeContextId,
};

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum AccountOperation {
    Add(crate::config::ActiveProvider),
    Switch {
        provider: crate::config::ActiveProvider,
        account_id: String,
    },
    Validate(crate::config::ActiveProvider),
    Remove(crate::config::ActiveProvider),
}

impl AccountOperation {
    pub(crate) const fn provider(&self) -> crate::config::ActiveProvider {
        match self {
            Self::Add(provider)
            | Self::Validate(provider)
            | Self::Remove(provider)
            | Self::Switch { provider, .. } => *provider,
        }
    }
}

#[derive(Clone, Debug)]
/// A request that modifies the player's playback
pub enum PlayerRequest {
    NextTrack,
    PreviousTrack,
    Resume,
    Pause,
    ResumePause,
    SeekTrack(chrono::Duration),
    Repeat,
    Shuffle,
    Volume(u8),
    ToggleMute,
    TransferPlayback(String, bool),
    StartPlayback(Playback, Option<bool>),
}

#[derive(Clone, Debug)]
pub enum YouTubePlayerRequest {
    Next,
    Previous,
    Refresh,
    Repeat,
    Shuffle,
    #[allow(dead_code)]
    Resume,
    #[allow(dead_code)]
    Pause,
    Volume(u8),
    ToggleMute,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ActivePlaybackControl {
    // Only constructed by media_control.rs.
    #[cfg_attr(not(feature = "media-control"), allow(dead_code))]
    Play,
    // Only constructed by media_control.rs.
    #[cfg_attr(not(feature = "media-control"), allow(dead_code))]
    Pause,
    Toggle,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum PlaybackNavigation {
    Next,
    Previous,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ActivePlaybackSeek {
    Start,
    // Only constructed by media_control.rs.
    #[cfg_attr(not(feature = "media-control"), allow(dead_code))]
    Absolute(std::time::Duration),
    Relative(chrono::Duration),
    Fraction {
        numerator: u16,
        denominator: u16,
    },
}

impl ActivePlaybackControl {
    pub(crate) const fn operation_name(self) -> &'static str {
        match self {
            Self::Play => "playback_play",
            Self::Pause => "playback_pause",
            Self::Toggle => "playback_toggle_pause",
        }
    }
}

impl PlaybackNavigation {
    pub(crate) fn request(
        self,
        active_provider: crate::config::ActiveProvider,
        has_unified_queue: bool,
    ) -> ClientRequest {
        if has_unified_queue {
            return match self {
                Self::Next => ClientRequest::UnifiedNext,
                Self::Previous => ClientRequest::UnifiedPrevious,
            };
        }

        match (self, active_provider) {
            (Self::Next, crate::config::ActiveProvider::Spotify) => {
                ClientRequest::Player(PlayerRequest::NextTrack)
            }
            (Self::Previous, crate::config::ActiveProvider::Spotify) => {
                ClientRequest::Player(PlayerRequest::PreviousTrack)
            }
            (Self::Next, crate::config::ActiveProvider::YouTubeMusic) => {
                ClientRequest::YouTubePlayer(YouTubePlayerRequest::Next)
            }
            (Self::Previous, crate::config::ActiveProvider::YouTubeMusic) => {
                ClientRequest::YouTubePlayer(YouTubePlayerRequest::Previous)
            }
        }
    }
}

/// Provider completion evidence captured while one unified queue occurrence
/// and one playback activation are current.
#[derive(Clone, Debug)]
pub(crate) struct UnifiedQueueCompletion {
    pub(crate) source: crate::config::ActiveProvider,
    pub(crate) queue: crate::state::UnifiedQueueCompletionToken,
    pub(crate) activation_generation: u64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum PlaybackControlRejection {
    NoActivePlayback,
    TransitionInProgress,
    SeekUnavailable,
    ShuttingDown,
}

impl PlaybackControlRejection {
    pub(crate) const fn diagnostic_reason(self) -> &'static str {
        match self {
            Self::NoActivePlayback => "no_active_playback",
            Self::TransitionInProgress => "playback_transition_in_progress",
            Self::SeekUnavailable => "seek_position_unavailable",
            Self::ShuttingDown => "playback_shutting_down",
        }
    }
}

/// One application-level playlist request.  Provider-specific payloads stay
/// behind this boundary, while the operation id gives planning, scheduling,
/// execution, and result publication one correlation key.
#[derive(Clone, Debug)]
pub struct PlaylistRequest {
    pub operation_id: super::PlaylistMutationOperationId,
    pub operation: PlaylistRequestKind,
}

#[derive(Clone, Debug)]
pub enum PlaylistRequestKind {
    AddYouTubeTrack {
        playlist_id: String,
        track: crate::state::YouTubeTrack,
    },
    AddItemsToUnified {
        playlist_id: String,
        items: Vec<crate::state::UnifiedPlaylistItem>,
        operation: Option<crate::state::PlaylistOperationEnvelope>,
    },
    AddPlayableToPlaylist {
        playlist_id: PlaylistId<'static>,
        playable_id: PlayableId<'static>,
    },
    RemoveUnifiedOccurrences {
        playlist_id: String,
        expected_order: Vec<crate::state::PlaylistEntryId>,
        entry_ids: Vec<crate::state::PlaylistEntryId>,
    },
    SpotifyMutation(super::SpotifyMutationIntent),
    YouTubeMutation(super::YouTubeMutationIntent),
    CreatePlaylist {
        playlist_name: String,
        public: bool,
        collab: bool,
        desc: String,
    },
    CreateSpotifyPlaylistWithTracks {
        playlist_name: String,
        public: bool,
        collab: bool,
        desc: String,
        tracks: Vec<crate::state::Track>,
    },
    CreateYouTubePlaylist {
        playlist_name: String,
        public: bool,
    },
    CreateYouTubePlaylistWithTracks {
        playlist_name: String,
        public: bool,
        tracks: Vec<crate::state::YouTubeTrack>,
    },
    CreateUnifiedPlaylist {
        playlist_name: String,
    },
    CreateUnifiedPlaylistWithItems {
        playlist_name: String,
        items: Vec<crate::state::UnifiedPlaylistItem>,
        operation: Option<crate::state::PlaylistOperationEnvelope>,
    },
    CreateUnifiedPlaylistFromHistory {
        playlist_name: String,
        items: Vec<crate::state::UnifiedPlaylistItem>,
    },
    RenameSpotifyPlaylist {
        playlist_id: PlaylistId<'static>,
        name: String,
    },
    RenameYouTubePlaylist {
        playlist_id: String,
        name: String,
    },
    RenameUnifiedPlaylist {
        playlist_id: String,
        name: String,
    },
    DeleteYouTubePlaylist {
        playlist_id: String,
    },
    DeleteUnifiedPlaylist {
        playlist_id: String,
    },
    LinkUnifiedPlaylistToYouTube {
        unified_playlist_id: String,
        youtube_playlist_id: String,
    },
    SyncUnifiedPlaylistToYouTube {
        unified_playlist_id: String,
    },
    UnlinkUnifiedPlaylistFromYouTube {
        unified_playlist_id: String,
    },
}

impl PlaylistRequest {
    pub fn new(
        operation_id: super::PlaylistMutationOperationId,
        mut operation: PlaylistRequestKind,
    ) -> Self {
        match &mut operation {
            PlaylistRequestKind::AddItemsToUnified {
                operation: Some(envelope),
                ..
            }
            | PlaylistRequestKind::CreateUnifiedPlaylistWithItems {
                operation: Some(envelope),
                ..
            } => envelope.application_operation_id = Some(operation_id.0),
            _ => {}
        }
        Self {
            operation_id,
            operation,
        }
    }

    pub(crate) const fn operation_name(&self) -> &'static str {
        match &self.operation {
            PlaylistRequestKind::AddYouTubeTrack { .. } => "add_youtube_playlist_item",
            PlaylistRequestKind::AddItemsToUnified { .. } => "add_unified_playlist_items",
            PlaylistRequestKind::AddPlayableToPlaylist { .. } => "add_spotify_playlist_item",
            PlaylistRequestKind::RemoveUnifiedOccurrences { .. } => {
                "remove_unified_playlist_occurrences"
            }
            PlaylistRequestKind::SpotifyMutation(_) => "mutate_spotify_playlist",
            PlaylistRequestKind::YouTubeMutation(_) => "mutate_youtube_playlist",
            PlaylistRequestKind::CreatePlaylist { .. } => "create_spotify_playlist",
            PlaylistRequestKind::CreateSpotifyPlaylistWithTracks { .. } => {
                "create_spotify_playlist_with_tracks"
            }
            PlaylistRequestKind::CreateYouTubePlaylist { .. } => "create_youtube_playlist",
            PlaylistRequestKind::CreateYouTubePlaylistWithTracks { .. } => {
                "create_youtube_playlist_with_tracks"
            }
            PlaylistRequestKind::CreateUnifiedPlaylist { .. } => "create_unified_playlist",
            PlaylistRequestKind::CreateUnifiedPlaylistWithItems { .. } => {
                "create_unified_playlist_with_items"
            }
            PlaylistRequestKind::CreateUnifiedPlaylistFromHistory { .. } => {
                "create_unified_playlist_from_history"
            }
            PlaylistRequestKind::RenameSpotifyPlaylist { .. } => "rename_spotify_playlist",
            PlaylistRequestKind::RenameYouTubePlaylist { .. } => "rename_youtube_playlist",
            PlaylistRequestKind::RenameUnifiedPlaylist { .. } => "rename_unified_playlist",
            PlaylistRequestKind::DeleteYouTubePlaylist { .. } => "delete_youtube_playlist",
            PlaylistRequestKind::DeleteUnifiedPlaylist { .. } => "delete_unified_playlist",
            PlaylistRequestKind::LinkUnifiedPlaylistToYouTube { .. } => {
                "link_unified_playlist_to_youtube"
            }
            PlaylistRequestKind::SyncUnifiedPlaylistToYouTube { .. } => {
                "sync_unified_playlist_to_youtube"
            }
            PlaylistRequestKind::UnlinkUnifiedPlaylistFromYouTube { .. } => {
                "unlink_unified_playlist_from_youtube"
            }
        }
    }

    pub(crate) const fn provider(&self) -> Option<crate::observability::ProviderKind> {
        use crate::observability::ProviderKind::{Spotify, YoutubeMusic};
        match &self.operation {
            PlaylistRequestKind::AddYouTubeTrack { .. }
            | PlaylistRequestKind::YouTubeMutation(_)
            | PlaylistRequestKind::CreateYouTubePlaylist { .. }
            | PlaylistRequestKind::CreateYouTubePlaylistWithTracks { .. }
            | PlaylistRequestKind::RenameYouTubePlaylist { .. }
            | PlaylistRequestKind::DeleteYouTubePlaylist { .. } => Some(YoutubeMusic),
            PlaylistRequestKind::AddPlayableToPlaylist { .. }
            | PlaylistRequestKind::SpotifyMutation(_)
            | PlaylistRequestKind::CreatePlaylist { .. }
            | PlaylistRequestKind::CreateSpotifyPlaylistWithTracks { .. }
            | PlaylistRequestKind::RenameSpotifyPlaylist { .. } => Some(Spotify),
            PlaylistRequestKind::RemoveUnifiedOccurrences { .. }
            | PlaylistRequestKind::AddItemsToUnified { .. }
            | PlaylistRequestKind::CreateUnifiedPlaylist { .. }
            | PlaylistRequestKind::CreateUnifiedPlaylistWithItems { .. }
            | PlaylistRequestKind::CreateUnifiedPlaylistFromHistory { .. }
            | PlaylistRequestKind::RenameUnifiedPlaylist { .. }
            | PlaylistRequestKind::DeleteUnifiedPlaylist { .. }
            | PlaylistRequestKind::LinkUnifiedPlaylistToYouTube { .. }
            | PlaylistRequestKind::SyncUnifiedPlaylistToYouTube { .. }
            | PlaylistRequestKind::UnlinkUnifiedPlaylistFromYouTube { .. } => None,
        }
    }

    pub(crate) fn into_legacy(self) -> ClientRequest {
        match self.operation {
            PlaylistRequestKind::AddYouTubeTrack { playlist_id, track } => {
                ClientRequest::AddYouTubeTrackToPlaylist {
                    operation_id: self.operation_id,
                    playlist_id,
                    track,
                }
            }
            PlaylistRequestKind::AddItemsToUnified {
                playlist_id,
                items,
                operation,
            } => ClientRequest::AddItemsToUnifiedPlaylist {
                playlist_id,
                items,
                operation,
            },
            PlaylistRequestKind::AddPlayableToPlaylist {
                playlist_id,
                playable_id,
            } => ClientRequest::AddPlayableToPlaylist(playlist_id, playable_id),
            PlaylistRequestKind::RemoveUnifiedOccurrences { .. } => {
                unreachable!("local Unified removals are applied by the application service")
            }
            PlaylistRequestKind::SpotifyMutation(intent) => {
                ClientRequest::SpotifyPlaylistMutation(intent)
            }
            PlaylistRequestKind::YouTubeMutation(intent) => {
                ClientRequest::YouTubePlaylistMutation(intent)
            }
            PlaylistRequestKind::CreatePlaylist {
                playlist_name,
                public,
                collab,
                desc,
            } => ClientRequest::CreatePlaylist {
                playlist_name,
                public,
                collab,
                desc,
            },
            PlaylistRequestKind::CreateSpotifyPlaylistWithTracks {
                playlist_name,
                public,
                collab,
                desc,
                tracks,
            } => ClientRequest::CreateSpotifyPlaylistWithTracks {
                playlist_name,
                public,
                collab,
                desc,
                tracks,
            },
            PlaylistRequestKind::CreateYouTubePlaylist {
                playlist_name,
                public,
            } => ClientRequest::CreateYouTubePlaylist {
                playlist_name,
                public,
            },
            PlaylistRequestKind::CreateYouTubePlaylistWithTracks {
                playlist_name,
                public,
                tracks,
            } => ClientRequest::CreateYouTubePlaylistWithTracks {
                playlist_name,
                public,
                tracks,
            },
            PlaylistRequestKind::CreateUnifiedPlaylist { playlist_name } => {
                ClientRequest::CreateUnifiedPlaylist { playlist_name }
            }
            PlaylistRequestKind::CreateUnifiedPlaylistWithItems {
                playlist_name,
                items,
                operation,
            } => ClientRequest::CreateUnifiedPlaylistWithItems {
                playlist_name,
                items,
                operation,
            },
            PlaylistRequestKind::CreateUnifiedPlaylistFromHistory {
                playlist_name,
                items,
            } => ClientRequest::CreateUnifiedPlaylistFromHistory {
                playlist_name,
                items,
            },
            PlaylistRequestKind::RenameSpotifyPlaylist { playlist_id, name } => {
                ClientRequest::RenameSpotifyPlaylist { playlist_id, name }
            }
            PlaylistRequestKind::RenameYouTubePlaylist { playlist_id, name } => {
                ClientRequest::RenameYouTubePlaylist { playlist_id, name }
            }
            PlaylistRequestKind::RenameUnifiedPlaylist { playlist_id, name } => {
                ClientRequest::RenameUnifiedPlaylist { playlist_id, name }
            }
            PlaylistRequestKind::DeleteYouTubePlaylist { playlist_id } => {
                ClientRequest::DeleteYouTubePlaylist { playlist_id }
            }
            PlaylistRequestKind::DeleteUnifiedPlaylist { playlist_id } => {
                ClientRequest::DeleteUnifiedPlaylist { playlist_id }
            }
            PlaylistRequestKind::LinkUnifiedPlaylistToYouTube {
                unified_playlist_id,
                youtube_playlist_id,
            } => ClientRequest::LinkUnifiedPlaylistToYouTube {
                unified_playlist_id,
                youtube_playlist_id,
            },
            PlaylistRequestKind::SyncUnifiedPlaylistToYouTube {
                unified_playlist_id,
            } => ClientRequest::SyncUnifiedPlaylistToYouTube {
                unified_playlist_id,
            },
            PlaylistRequestKind::UnlinkUnifiedPlaylistFromYouTube {
                unified_playlist_id,
            } => ClientRequest::UnlinkUnifiedPlaylistFromYouTube {
                unified_playlist_id,
            },
        }
    }
}

#[derive(Clone, Debug)]
/// A request to the client
pub enum ClientRequest {
    GetCurrentUser,
    GetDevices,
    GetBrowseCategories,
    GetBrowseCategoryPlaylists(Category),
    GetUserPlaylists,
    GetUserSavedAlbums,
    GetUserSavedShows,
    GetUserFollowedArtists,
    GetYouTubeLibrary,
    ListListenBrainzPlaylists {
        operation: u64,
        identity: super::listenbrainz::ValidatedListenBrainzIdentity,
    },
    ImportListenBrainzPlaylist {
        operation: u64,
        identity: super::listenbrainz::ValidatedListenBrainzIdentity,
        playlist_id: String,
    },
    ValidateListenBrainzToken {
        attempt: u64,
        token: super::listenbrainz::ListenBrainzToken,
        save: bool,
    },
    TestYouTubeAuth,
    AuthenticateYouTubeBrowser,
    ImportYouTubeCookies(std::path::PathBuf),
    CancelYouTubeAuthentication,
    InitializeSpotifySession,
    ReauthenticateSpotify,
    ManageAccount(AccountOperation),
    ResetAllConfiguration,
    ShutdownPlayback,
    SwitchProvider(crate::config::ActiveProvider),
    ActivePlaybackControl(ActivePlaybackControl),
    ActivePlaybackSeek(ActivePlaybackSeek),
    GetYouTubeContext(YouTubeContextId),
    GetContext(ContextId),
    EnrichListenBrainzArtist {
        request_id: u64,
        context_uri: String,
        spotify_artist_id: String,
        include_recordings: bool,
        include_release_groups: bool,
    },
    ResolveListenBrainzRecording {
        request_id: u64,
        context_uri: String,
        recording_mbid: String,
        intent: ListenBrainzRecordingIntent,
    },
    ResolveListenBrainzAlbum {
        request_id: u64,
        context_uri: String,
        release_group_mbid: String,
        intent: ListenBrainzAlbumIntent,
    },
    GetCurrentPlayback,
    Search {
        query: String,
        lifecycle_reference: String,
    },
    GetSpotifyUser(String),
    SearchYouTube {
        query: String,
        lifecycle_reference: String,
    },
    PlayYouTubeContext {
        tracks: Vec<crate::state::YouTubeTrack>,
        start_index: usize,
    },
    PlayUnifiedItems {
        items: Vec<PlayableMedia>,
        start_index: usize,
    },
    AddItemsToUserQueue(Vec<PlayableMedia>),
    ContinueUnifiedQueue(UnifiedQueueCompletion),
    Playlist(PlaylistRequest),
    AddYouTubeTrackToPlaylist {
        operation_id: super::PlaylistMutationOperationId,
        playlist_id: String,
        track: crate::state::YouTubeTrack,
    },
    AddItemsToUnifiedPlaylist {
        playlist_id: String,
        items: Vec<crate::state::UnifiedPlaylistItem>,
        operation: Option<crate::state::PlaylistOperationEnvelope>,
    },
    UnifiedNext,
    UnifiedPrevious,
    YouTubePlayer(YouTubePlayerRequest),
    AddPlayableToQueue(PlayableId<'static>),
    AddAlbumToQueue(AlbumId<'static>),
    AddPlayableToPlaylist(PlaylistId<'static>, PlayableId<'static>),
    SpotifyPlaylistMutation(super::SpotifyMutationIntent),
    YouTubePlaylistMutation(super::YouTubeMutationIntent),
    AddToLibrary(Item),
    AddTracksToLibrary(Vec<crate::state::Track>),
    DeleteTracksFromLibrary(Vec<crate::state::TrackId<'static>>),
    DeleteFromLibrary(ItemId),
    Player(PlayerRequest),
    GetCurrentUserQueue(NativeQueueRefreshGuard),
    /// Spotify's recently played and top tracks for Home, tagged with the
    /// account and fetch generation they were requested for.
    GetHomeFeed {
        account: Option<String>,
        generation: u64,
    },
    GetLyrics {
        track_id: TrackId<'static>,
    },
    GetYouTubeLyrics(crate::state::YouTubeTrack),
    GetLyricsFromProvider {
        track_id: TrackId<'static>,
        provider: String,
    },
    GetYouTubeLyricsFromProvider {
        track: crate::state::YouTubeTrack,
        provider: String,
    },
    RateYouTubeTrack {
        track: crate::state::YouTubeTrack,
        liked: bool,
    },
    SubscribeYouTubeArtist {
        channel_id: String,
    },
    UnsubscribeYouTubeArtist {
        channel_id: String,
    },
    #[cfg(feature = "streaming")]
    RestartIntegratedClient,
    CreatePlaylist {
        playlist_name: String,
        public: bool,
        collab: bool,
        desc: String,
    },
    CreateSpotifyPlaylistWithTracks {
        playlist_name: String,
        public: bool,
        collab: bool,
        desc: String,
        tracks: Vec<crate::state::Track>,
    },
    CreateYouTubePlaylist {
        playlist_name: String,
        public: bool,
    },
    CreateYouTubePlaylistWithTracks {
        playlist_name: String,
        public: bool,
        tracks: Vec<crate::state::YouTubeTrack>,
    },
    CreateUnifiedPlaylist {
        playlist_name: String,
    },
    CreateUnifiedPlaylistWithItems {
        playlist_name: String,
        items: Vec<crate::state::UnifiedPlaylistItem>,
        operation: Option<crate::state::PlaylistOperationEnvelope>,
    },
    CreateUnifiedPlaylistFromHistory {
        playlist_name: String,
        items: Vec<crate::state::UnifiedPlaylistItem>,
    },
    RenameSpotifyPlaylist {
        playlist_id: PlaylistId<'static>,
        name: String,
    },
    RenameYouTubePlaylist {
        playlist_id: String,
        name: String,
    },
    RenameUnifiedPlaylist {
        playlist_id: String,
        name: String,
    },
    DeleteYouTubePlaylist {
        playlist_id: String,
    },
    DeleteUnifiedPlaylist {
        playlist_id: String,
    },
    BackupUnifiedPlaylistToListenBrainz {
        unified_playlist_id: String,
        operation_reference: String,
    },
    CheckUnifiedPlaylistListenBrainzSync {
        unified_playlist_id: String,
        operation_reference: String,
        mode: ListenBrainzSyncReadMode,
    },
    InitializeUnifiedPlaylistListenBrainzBase {
        unified_playlist_id: String,
        operation_reference: String,
    },
    ApplyUnifiedPlaylistListenBrainzPush {
        unified_playlist_id: String,
        operation_reference: String,
        operation_id: String,
    },
    ApplyUnifiedPlaylistListenBrainzPull {
        unified_playlist_id: String,
        operation_reference: String,
        operation_id: String,
    },
    ApplyUnifiedPlaylistListenBrainzResolve {
        unified_playlist_id: String,
        operation_reference: String,
        operation_id: String,
        policy: crate::client::listenbrainz_resolution::ResolutionPolicy,
        decisions: Vec<crate::client::listenbrainz_resolution::ConflictDecision>,
    },
    LinkUnifiedPlaylistToYouTube {
        unified_playlist_id: String,
        youtube_playlist_id: String,
    },
    SyncUnifiedPlaylistToYouTube {
        unified_playlist_id: String,
    },
    UnlinkUnifiedPlaylistFromYouTube {
        unified_playlist_id: String,
    },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ListenBrainzSyncReadMode {
    Preview,
    Recovery,
}

impl ClientRequest {
    pub(crate) const fn operation_name(&self) -> &'static str {
        match self {
            Self::GetCurrentUser => "get_current_user",
            Self::GetDevices => "get_devices",
            Self::GetBrowseCategories => "get_browse_categories",
            Self::GetBrowseCategoryPlaylists(_) => "get_browse_category_playlists",
            Self::GetUserPlaylists => "get_user_playlists",
            Self::GetUserSavedAlbums => "get_user_saved_albums",
            Self::GetUserSavedShows => "get_user_saved_shows",
            Self::GetUserFollowedArtists => "get_user_followed_artists",
            Self::GetYouTubeLibrary => "get_youtube_library",
            Self::ListListenBrainzPlaylists { .. } => "list_listenbrainz_playlists",
            Self::ImportListenBrainzPlaylist { .. } => "import_listenbrainz_playlist",
            Self::ValidateListenBrainzToken { .. } => "validate_listenbrainz_token",
            Self::TestYouTubeAuth => "test_youtube_auth",
            Self::AuthenticateYouTubeBrowser => "authenticate_youtube_browser",
            Self::ImportYouTubeCookies(_) => "import_youtube_cookies",
            Self::CancelYouTubeAuthentication => "cancel_youtube_authentication",
            Self::InitializeSpotifySession => "initialize_spotify_session",
            Self::ReauthenticateSpotify => "reauthenticate_spotify",
            Self::ResetAllConfiguration => "reset_all_configuration",
            Self::ManageAccount(_) => "manage_account",
            Self::ShutdownPlayback => "shutdown_playback",
            Self::SwitchProvider(_) => "switch_provider",
            Self::ActivePlaybackControl(control) => control.operation_name(),
            Self::ActivePlaybackSeek(_) => "playback_seek",
            Self::GetYouTubeContext(_) => "get_youtube_context",
            Self::GetContext(_) => "get_spotify_context",
            Self::EnrichListenBrainzArtist { .. } => "enrich_listenbrainz_artist",
            Self::ResolveListenBrainzRecording { .. } => "resolve_listenbrainz_recording",
            Self::ResolveListenBrainzAlbum { .. } => "resolve_listenbrainz_album",
            Self::GetCurrentPlayback => "get_current_playback",
            Self::Search { .. } => "search_spotify",
            Self::GetSpotifyUser(_) => "get_spotify_user",
            Self::SearchYouTube { .. } => "search_youtube",
            Self::PlayYouTubeContext { .. } => "play_youtube_context",
            Self::PlayUnifiedItems { .. } => "play_unified_items",
            Self::AddItemsToUserQueue(_) => "add_to_user_queue",
            Self::ContinueUnifiedQueue(_) => "unified_queue_continue",
            Self::Playlist(request) => request.operation_name(),
            Self::AddYouTubeTrackToPlaylist { .. } => "add_youtube_playlist_item",
            Self::AddItemsToUnifiedPlaylist { .. } => "add_unified_playlist_items",
            Self::UnifiedNext => "unified_next",
            Self::UnifiedPrevious => "unified_previous",
            Self::YouTubePlayer(request) => request.operation_name(),
            Self::AddPlayableToQueue(_) => "add_spotify_queue_item",
            Self::AddAlbumToQueue(_) => "add_spotify_album_to_queue",
            Self::AddPlayableToPlaylist(_, _) => "add_spotify_playlist_item",
            Self::SpotifyPlaylistMutation(_) => "mutate_spotify_playlist",
            Self::YouTubePlaylistMutation(_) => "mutate_youtube_playlist",
            Self::AddToLibrary(_) => "add_to_spotify_library",
            Self::AddTracksToLibrary(_) => "add_tracks_to_spotify_library",
            Self::DeleteTracksFromLibrary(_) => "delete_tracks_from_spotify_library",
            Self::DeleteFromLibrary(_) => "delete_from_spotify_library",
            Self::Player(request) => request.operation_name(),
            Self::GetCurrentUserQueue(_) => "get_current_user_queue",
            Self::GetHomeFeed { .. } => "get_home_feed",
            Self::GetLyrics { .. } => "get_spotify_lyrics",
            Self::GetYouTubeLyrics(_) => "get_youtube_lyrics",
            Self::GetLyricsFromProvider { .. } => "get_spotify_lyrics_provider",
            Self::GetYouTubeLyricsFromProvider { .. } => "get_youtube_lyrics_provider",
            Self::RateYouTubeTrack { .. } => "rate_youtube_track",
            Self::SubscribeYouTubeArtist { .. } => "subscribe_youtube_artist",
            Self::UnsubscribeYouTubeArtist { .. } => "unsubscribe_youtube_artist",
            #[cfg(feature = "streaming")]
            Self::RestartIntegratedClient => "restart_integrated_client",
            Self::CreatePlaylist { .. } => "create_spotify_playlist",
            Self::CreateSpotifyPlaylistWithTracks { .. } => "create_spotify_playlist_with_tracks",
            Self::CreateYouTubePlaylist { .. } => "create_youtube_playlist",
            Self::CreateYouTubePlaylistWithTracks { .. } => "create_youtube_playlist_with_tracks",
            Self::CreateUnifiedPlaylist { .. } => "create_unified_playlist",
            Self::CreateUnifiedPlaylistWithItems { .. } => "create_unified_playlist_with_items",
            Self::CreateUnifiedPlaylistFromHistory { .. } => "create_unified_playlist_from_history",
            Self::RenameSpotifyPlaylist { .. } => "rename_spotify_playlist",
            Self::RenameYouTubePlaylist { .. } => "rename_youtube_playlist",
            Self::RenameUnifiedPlaylist { .. } => "rename_unified_playlist",
            Self::DeleteYouTubePlaylist { .. } => "delete_youtube_playlist",
            Self::DeleteUnifiedPlaylist { .. } => "delete_unified_playlist",
            Self::BackupUnifiedPlaylistToListenBrainz { .. } => {
                "backup_unified_playlist_to_listenbrainz"
            }
            Self::CheckUnifiedPlaylistListenBrainzSync { .. } => {
                "check_unified_playlist_listenbrainz_sync"
            }
            Self::InitializeUnifiedPlaylistListenBrainzBase { .. } => {
                "initialize_unified_playlist_listenbrainz_base"
            }
            Self::ApplyUnifiedPlaylistListenBrainzPush { .. } => {
                "apply_unified_playlist_listenbrainz_push"
            }
            Self::ApplyUnifiedPlaylistListenBrainzPull { .. } => {
                "apply_unified_playlist_listenbrainz_pull"
            }
            Self::ApplyUnifiedPlaylistListenBrainzResolve { .. } => {
                "apply_unified_playlist_listenbrainz_resolve"
            }
            Self::LinkUnifiedPlaylistToYouTube { .. } => "link_unified_playlist_to_youtube",
            Self::SyncUnifiedPlaylistToYouTube { .. } => "sync_unified_playlist_to_youtube",
            Self::UnlinkUnifiedPlaylistFromYouTube { .. } => "unlink_unified_playlist_from_youtube",
        }
    }

    pub(crate) const fn provider(&self) -> Option<crate::observability::ProviderKind> {
        use crate::observability::ProviderKind::{Spotify, YoutubeMusic};
        match self {
            Self::GetYouTubeLibrary
            | Self::TestYouTubeAuth
            | Self::AuthenticateYouTubeBrowser
            | Self::ImportYouTubeCookies(_)
            | Self::CancelYouTubeAuthentication
            | Self::GetYouTubeContext(_)
            | Self::SearchYouTube { .. }
            | Self::PlayYouTubeContext { .. }
            | Self::AddYouTubeTrackToPlaylist { .. }
            | Self::YouTubePlayer(_)
            | Self::GetYouTubeLyrics(_)
            | Self::GetYouTubeLyricsFromProvider { .. }
            | Self::RateYouTubeTrack { .. }
            | Self::SubscribeYouTubeArtist { .. }
            | Self::UnsubscribeYouTubeArtist { .. } => Some(YoutubeMusic),
            Self::Playlist(request) => request.provider(),
            Self::YouTubePlaylistMutation(_) => Some(YoutubeMusic),
            Self::ReauthenticateSpotify
            | Self::InitializeSpotifySession
            | Self::GetContext(_)
            | Self::EnrichListenBrainzArtist { .. }
            | Self::ResolveListenBrainzRecording { .. }
            | Self::ResolveListenBrainzAlbum { .. }
            | Self::Search { .. }
            | Self::GetSpotifyUser(_)
            | Self::AddPlayableToQueue(_)
            | Self::AddAlbumToQueue(_)
            | Self::AddPlayableToPlaylist(_, _)
            | Self::SpotifyPlaylistMutation(_)
            | Self::AddToLibrary(_)
            | Self::AddTracksToLibrary(_)
            | Self::DeleteTracksFromLibrary(_)
            | Self::DeleteFromLibrary(_)
            | Self::Player(_)
            | Self::GetLyrics { .. }
            | Self::GetLyricsFromProvider { .. }
            | Self::CreatePlaylist { .. } => Some(Spotify),
            Self::CreateSpotifyPlaylistWithTracks { .. } => Some(Spotify),
            Self::CreateYouTubePlaylist { .. } => Some(YoutubeMusic),
            Self::CreateYouTubePlaylistWithTracks { .. } => Some(YoutubeMusic),
            Self::CreateUnifiedPlaylist { .. } => None,
            Self::CreateUnifiedPlaylistWithItems { .. } => None,
            Self::CreateUnifiedPlaylistFromHistory { .. } => None,
            Self::RenameSpotifyPlaylist { .. } => Some(Spotify),
            Self::RenameYouTubePlaylist { .. } => Some(YoutubeMusic),
            Self::RenameUnifiedPlaylist { .. } | Self::DeleteUnifiedPlaylist { .. } => None,
            Self::DeleteYouTubePlaylist { .. } => Some(YoutubeMusic),
            Self::BackupUnifiedPlaylistToListenBrainz { .. } => None,
            Self::CheckUnifiedPlaylistListenBrainzSync { .. } => None,
            Self::InitializeUnifiedPlaylistListenBrainzBase { .. } => None,
            Self::ApplyUnifiedPlaylistListenBrainzPush { .. } => None,
            Self::ApplyUnifiedPlaylistListenBrainzPull { .. } => None,
            Self::ApplyUnifiedPlaylistListenBrainzResolve { .. } => None,
            Self::LinkUnifiedPlaylistToYouTube { .. } => None,
            Self::SyncUnifiedPlaylistToYouTube { .. } => None,
            Self::UnlinkUnifiedPlaylistFromYouTube { .. } => None,
            Self::ManageAccount(operation) => Some(match operation.provider() {
                crate::config::ActiveProvider::Spotify => Spotify,
                crate::config::ActiveProvider::YouTubeMusic => YoutubeMusic,
            }),
            #[cfg(feature = "streaming")]
            Self::RestartIntegratedClient => Some(Spotify),
            Self::SwitchProvider(provider) => Some(match provider {
                crate::config::ActiveProvider::Spotify => Spotify,
                crate::config::ActiveProvider::YouTubeMusic => YoutubeMusic,
            }),
            Self::GetCurrentUser
            | Self::GetDevices
            | Self::GetBrowseCategories
            | Self::GetBrowseCategoryPlaylists(_)
            | Self::GetUserPlaylists
            | Self::GetUserSavedAlbums
            | Self::GetUserSavedShows
            | Self::GetUserFollowedArtists
            | Self::ShutdownPlayback
            | Self::ActivePlaybackControl(_)
            | Self::ActivePlaybackSeek(_)
            | Self::GetCurrentPlayback
            | Self::PlayUnifiedItems { .. }
            | Self::AddItemsToUserQueue(_)
            | Self::ContinueUnifiedQueue(_)
            | Self::AddItemsToUnifiedPlaylist { .. }
            | Self::UnifiedNext
            | Self::UnifiedPrevious
            | Self::GetCurrentUserQueue(_)
            | Self::GetHomeFeed { .. }
            | Self::ListListenBrainzPlaylists { .. }
            | Self::ImportListenBrainzPlaylist { .. }
            | Self::ValidateListenBrainzToken { .. }
            | Self::ResetAllConfiguration => None,
        }
    }
    pub(crate) fn domain(&self) -> RequestDomain {
        use RequestDomain::{AuthSession, Playback, PlaylistMutation, ProviderRead};

        match self {
            Self::TestYouTubeAuth
            | Self::AuthenticateYouTubeBrowser
            | Self::ImportYouTubeCookies(_)
            | Self::CancelYouTubeAuthentication
            | Self::InitializeSpotifySession
            | Self::ReauthenticateSpotify
            | Self::ListListenBrainzPlaylists { .. }
            | Self::ImportListenBrainzPlaylist { .. }
            | Self::ValidateListenBrainzToken { .. }
            | Self::ResetAllConfiguration
            | Self::ManageAccount(_) => AuthSession,
            #[cfg(feature = "streaming")]
            Self::RestartIntegratedClient => AuthSession,

            Self::GetCurrentUser
            | Self::GetDevices
            | Self::GetBrowseCategories
            | Self::GetBrowseCategoryPlaylists(_)
            | Self::GetUserPlaylists
            | Self::GetUserSavedAlbums
            | Self::GetUserSavedShows
            | Self::GetUserFollowedArtists
            | Self::GetYouTubeLibrary
            | Self::GetYouTubeContext(_)
            | Self::GetContext(_)
            | Self::EnrichListenBrainzArtist { .. }
            | Self::ResolveListenBrainzRecording { .. }
            | Self::ResolveListenBrainzAlbum { .. }
            | Self::Search { .. }
            | Self::GetSpotifyUser(_)
            | Self::SearchYouTube { .. }
            | Self::GetCurrentUserQueue(_)
            | Self::GetHomeFeed { .. }
            | Self::GetLyrics { .. }
            | Self::GetLyricsFromProvider { .. }
            | Self::GetYouTubeLyrics(_)
            | Self::GetYouTubeLyricsFromProvider { .. }
            | Self::CheckUnifiedPlaylistListenBrainzSync { .. }
            | Self::InitializeUnifiedPlaylistListenBrainzBase { .. } => ProviderRead,

            Self::ShutdownPlayback
            | Self::SwitchProvider(_)
            | Self::ActivePlaybackControl(_)
            | Self::ActivePlaybackSeek(_)
            | Self::GetCurrentPlayback
            | Self::PlayYouTubeContext { .. }
            | Self::PlayUnifiedItems { .. }
            | Self::ContinueUnifiedQueue(_)
            | Self::UnifiedNext
            | Self::UnifiedPrevious
            | Self::Player(_)
            | Self::YouTubePlayer(_) => Playback,

            Self::Playlist(_)
            | Self::AddItemsToUserQueue(_)
            | Self::AddYouTubeTrackToPlaylist { .. }
            | Self::AddItemsToUnifiedPlaylist { .. }
            | Self::AddPlayableToQueue(_)
            | Self::AddAlbumToQueue(_)
            | Self::AddPlayableToPlaylist(_, _)
            | Self::SpotifyPlaylistMutation(_)
            | Self::YouTubePlaylistMutation(_)
            | Self::AddToLibrary(_)
            | Self::AddTracksToLibrary(_)
            | Self::DeleteTracksFromLibrary(_)
            | Self::DeleteFromLibrary(_)
            | Self::RateYouTubeTrack { .. }
            | Self::SubscribeYouTubeArtist { .. }
            | Self::UnsubscribeYouTubeArtist { .. }
            | Self::CreatePlaylist { .. }
            | Self::CreateSpotifyPlaylistWithTracks { .. }
            | Self::CreateYouTubePlaylist { .. }
            | Self::CreateYouTubePlaylistWithTracks { .. }
            | Self::CreateUnifiedPlaylist { .. }
            | Self::CreateUnifiedPlaylistWithItems { .. }
            | Self::CreateUnifiedPlaylistFromHistory { .. } => PlaylistMutation,
            Self::RenameSpotifyPlaylist { .. }
            | Self::RenameYouTubePlaylist { .. }
            | Self::RenameUnifiedPlaylist { .. }
            | Self::DeleteYouTubePlaylist { .. }
            | Self::DeleteUnifiedPlaylist { .. } => PlaylistMutation,
            Self::BackupUnifiedPlaylistToListenBrainz { .. } => PlaylistMutation,
            Self::ApplyUnifiedPlaylistListenBrainzPush { .. } => PlaylistMutation,
            Self::ApplyUnifiedPlaylistListenBrainzPull { .. } => PlaylistMutation,
            Self::ApplyUnifiedPlaylistListenBrainzResolve { .. } => PlaylistMutation,
            Self::LinkUnifiedPlaylistToYouTube { .. } => PlaylistMutation,
            Self::SyncUnifiedPlaylistToYouTube { .. } => PlaylistMutation,
            Self::UnlinkUnifiedPlaylistFromYouTube { .. } => PlaylistMutation,
        }
    }

    pub(crate) fn delivery_policy(&self) -> RequestDelivery {
        use RequestDelivery::{Coalescible, LatestWins, MustDeliver, Ordered};
        use RequestKey::{
            BrowseCategories, BrowseCategory, CurrentPlayback, CurrentUser, CurrentUserQueue,
            Devices, Lyrics, PlaybackSelection, SpotifyAlbums, SpotifyArtists, SpotifyContext,
            SpotifyPlaybackSeek, SpotifyPlaybackVolume, SpotifyPlaylists, SpotifySearch,
            SpotifyShows, SpotifyUserSearch, YouTubeContext, YouTubeLibrary,
            YouTubePlaybackRefresh, YouTubePlaybackVolume, YouTubeSearch,
        };

        match self {
            Self::ShutdownPlayback
            | Self::TestYouTubeAuth
            | Self::AuthenticateYouTubeBrowser
            | Self::ImportYouTubeCookies(_)
            | Self::CancelYouTubeAuthentication
            | Self::InitializeSpotifySession
            | Self::ReauthenticateSpotify
            | Self::ListListenBrainzPlaylists { .. }
            | Self::ImportListenBrainzPlaylist { .. }
            | Self::ValidateListenBrainzToken { .. }
            | Self::ResetAllConfiguration => MustDeliver,
            Self::ManageAccount(_) => Ordered,
            #[cfg(feature = "streaming")]
            Self::RestartIntegratedClient => MustDeliver,

            Self::GetCurrentUser => Coalescible(CurrentUser),
            Self::GetDevices => Coalescible(Devices),
            Self::GetBrowseCategories => Coalescible(BrowseCategories),
            Self::GetUserPlaylists => Coalescible(SpotifyPlaylists),
            Self::GetUserSavedAlbums => Coalescible(SpotifyAlbums),
            Self::GetUserSavedShows => Coalescible(SpotifyShows),
            Self::GetUserFollowedArtists => Coalescible(SpotifyArtists),
            Self::GetYouTubeLibrary => Coalescible(YouTubeLibrary),
            Self::GetCurrentPlayback => Coalescible(CurrentPlayback),
            Self::GetCurrentUserQueue(_) => Coalescible(CurrentUserQueue),
            Self::GetHomeFeed { .. } => LatestWins(RequestKey::HomeFeed),
            Self::YouTubePlayer(YouTubePlayerRequest::Refresh) => {
                Coalescible(YouTubePlaybackRefresh)
            }

            Self::GetBrowseCategoryPlaylists(_) => LatestWins(BrowseCategory),
            Self::GetYouTubeContext(_) => LatestWins(YouTubeContext),
            Self::GetContext(_) => LatestWins(SpotifyContext),
            Self::EnrichListenBrainzArtist { .. } => {
                LatestWins(RequestKey::ListenBrainzArtistEnrichment)
            }
            Self::ResolveListenBrainzRecording { .. } => {
                LatestWins(RequestKey::ListenBrainzResolution)
            }
            Self::ResolveListenBrainzAlbum { .. } => {
                LatestWins(RequestKey::ListenBrainzAlbumResolution)
            }
            Self::Search { .. } => LatestWins(SpotifySearch),
            Self::GetSpotifyUser(_) => LatestWins(SpotifyUserSearch),
            Self::SearchYouTube { .. } => LatestWins(YouTubeSearch),
            Self::GetLyrics { .. }
            | Self::GetLyricsFromProvider { .. }
            | Self::GetYouTubeLyrics(_)
            | Self::GetYouTubeLyricsFromProvider { .. } => LatestWins(Lyrics),
            Self::SwitchProvider(_)
            | Self::PlayYouTubeContext { .. }
            | Self::PlayUnifiedItems { .. }
            | Self::ContinueUnifiedQueue(_)
            | Self::UnifiedNext
            | Self::UnifiedPrevious
            | Self::Player(PlayerRequest::StartPlayback(..))
            | Self::YouTubePlayer(YouTubePlayerRequest::Next | YouTubePlayerRequest::Previous) => {
                LatestWins(PlaybackSelection)
            }
            Self::Player(PlayerRequest::SeekTrack(_)) => LatestWins(SpotifyPlaybackSeek),
            Self::Player(PlayerRequest::Volume(_)) => LatestWins(SpotifyPlaybackVolume),
            Self::YouTubePlayer(YouTubePlayerRequest::Volume(_)) => {
                LatestWins(YouTubePlaybackVolume)
            }
            Self::ActivePlaybackControl(_) => LatestWins(RequestKey::ActivePlaybackControl),
            Self::ActivePlaybackSeek(_) => LatestWins(RequestKey::ActivePlaybackSeek),
            Self::BackupUnifiedPlaylistToListenBrainz { .. } => {
                Coalescible(RequestKey::ListenBrainzPlaylistBackup)
            }
            Self::CheckUnifiedPlaylistListenBrainzSync { .. } => Ordered,
            Self::InitializeUnifiedPlaylistListenBrainzBase { .. } => Ordered,
            Self::ApplyUnifiedPlaylistListenBrainzPush { .. } => Ordered,
            Self::ApplyUnifiedPlaylistListenBrainzPull { .. } => Ordered,
            Self::ApplyUnifiedPlaylistListenBrainzResolve { .. } => Ordered,

            Self::Playlist(_)
            | Self::AddItemsToUserQueue(_)
            | Self::AddYouTubeTrackToPlaylist { .. }
            | Self::AddItemsToUnifiedPlaylist { .. }
            | Self::AddPlayableToQueue(_)
            | Self::AddAlbumToQueue(_)
            | Self::AddPlayableToPlaylist(_, _)
            | Self::SpotifyPlaylistMutation(_)
            | Self::YouTubePlaylistMutation(_)
            | Self::AddToLibrary(_)
            | Self::AddTracksToLibrary(_)
            | Self::DeleteTracksFromLibrary(_)
            | Self::DeleteFromLibrary(_)
            | Self::RateYouTubeTrack { .. }
            | Self::SubscribeYouTubeArtist { .. }
            | Self::UnsubscribeYouTubeArtist { .. }
            | Self::CreatePlaylist { .. }
            | Self::CreateSpotifyPlaylistWithTracks { .. }
            | Self::CreateYouTubePlaylist { .. }
            | Self::CreateYouTubePlaylistWithTracks { .. }
            | Self::CreateUnifiedPlaylist { .. }
            | Self::CreateUnifiedPlaylistWithItems { .. }
            | Self::CreateUnifiedPlaylistFromHistory { .. }
            | Self::RenameSpotifyPlaylist { .. }
            | Self::RenameYouTubePlaylist { .. }
            | Self::RenameUnifiedPlaylist { .. }
            | Self::DeleteYouTubePlaylist { .. }
            | Self::DeleteUnifiedPlaylist { .. }
            | Self::LinkUnifiedPlaylistToYouTube { .. }
            | Self::SyncUnifiedPlaylistToYouTube { .. }
            | Self::UnlinkUnifiedPlaylistFromYouTube { .. }
            | Self::Player(_)
            | Self::YouTubePlayer(_) => Ordered,
        }
    }

    pub(crate) fn is_shutdown(&self) -> bool {
        matches!(self, Self::ShutdownPlayback)
    }

    pub(crate) fn is_account_change(&self) -> bool {
        matches!(self, Self::ManageAccount(_))
    }

    pub(crate) fn reserves_playback_activation(&self) -> bool {
        matches!(
            self,
            Self::SwitchProvider(_)
                | Self::PlayYouTubeContext { .. }
                | Self::PlayUnifiedItems { .. }
                | Self::ContinueUnifiedQueue(_)
                | Self::UnifiedNext
                | Self::UnifiedPrevious
                | Self::Player(PlayerRequest::StartPlayback(..))
                | Self::YouTubePlayer(YouTubePlayerRequest::Next | YouTubePlayerRequest::Previous)
        ) || matches!(
            self,
            Self::ResolveListenBrainzRecording {
                intent: ListenBrainzRecordingIntent::Play,
                ..
            }
        )
    }

    pub(crate) const fn unified_queue_completion(&self) -> Option<&UnifiedQueueCompletion> {
        match self {
            Self::ContinueUnifiedQueue(completion) => Some(completion),
            _ => None,
        }
    }
}

impl PlayerRequest {
    const fn operation_name(&self) -> &'static str {
        match self {
            Self::NextTrack => "spotify_next",
            Self::PreviousTrack => "spotify_previous",
            Self::Resume => "spotify_resume",
            Self::Pause => "spotify_pause",
            Self::ResumePause => "spotify_toggle_pause",
            Self::SeekTrack(_) => "spotify_seek",
            Self::Repeat => "spotify_repeat",
            Self::Shuffle => "spotify_shuffle",
            Self::Volume(_) => "spotify_volume",
            Self::ToggleMute => "spotify_mute",
            Self::TransferPlayback(_, _) => "spotify_transfer",
            Self::StartPlayback(_, _) => "spotify_play",
        }
    }
}

impl YouTubePlayerRequest {
    const fn operation_name(&self) -> &'static str {
        match self {
            Self::Next => "youtube_next",
            Self::Previous => "youtube_previous",
            Self::Refresh => "youtube_refresh",
            Self::Repeat => "youtube_repeat",
            Self::Shuffle => "youtube_shuffle",
            Self::Resume => "youtube_resume",
            Self::Pause => "youtube_pause",
            Self::Volume(_) => "youtube_volume",
            Self::ToggleMute => "youtube_mute",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum RequestDomain {
    AuthSession,
    ProviderRead,
    Playback,
    PlaylistMutation,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum RequestDelivery {
    /// Never discard this request during normal ingress; independent work may overlap.
    MustDeliver,
    /// Execute one ordered request at a time, in accepted order.
    Ordered,
    /// Keep only the newest pending request for this work class.
    LatestWins(RequestKey),
    /// Ignore a duplicate while the same work is pending or active.
    Coalescible(RequestKey),
}

impl RequestDelivery {
    pub(crate) fn key(self) -> Option<RequestKey> {
        match self {
            Self::LatestWins(key) | Self::Coalescible(key) => Some(key),
            Self::MustDeliver | Self::Ordered => None,
        }
    }
}

#[derive(Clone, Copy, Debug, Hash, PartialEq, Eq)]
pub(crate) enum RequestKey {
    CurrentUser,
    Devices,
    BrowseCategories,
    BrowseCategory,
    SpotifyPlaylists,
    SpotifyAlbums,
    SpotifyShows,
    SpotifyArtists,
    YouTubeLibrary,
    CurrentPlayback,
    CurrentUserQueue,
    HomeFeed,
    SpotifySearch,
    SpotifyUserSearch,
    YouTubeSearch,
    SpotifyContext,
    YouTubeContext,
    Lyrics,
    PlaybackSelection,
    SpotifyPlaybackSeek,
    SpotifyPlaybackVolume,
    YouTubePlaybackRefresh,
    YouTubePlaybackVolume,
    ActivePlaybackControl,
    ActivePlaybackSeek,
    ListenBrainzArtistEnrichment,
    ListenBrainzResolution,
    ListenBrainzAlbumResolution,
    ListenBrainzPlaylistBackup,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn playback_activation_requests_are_reserved_before_dispatch() {
        assert!(
            ClientRequest::SwitchProvider(crate::config::ActiveProvider::YouTubeMusic)
                .reserves_playback_activation()
        );
        assert!(ClientRequest::PlayYouTubeContext {
            tracks: Vec::new(),
            start_index: 0,
        }
        .reserves_playback_activation());
        assert!(
            ClientRequest::YouTubePlayer(YouTubePlayerRequest::Next).reserves_playback_activation()
        );
        assert!(ClientRequest::Player(PlayerRequest::StartPlayback(
            crate::state::Playback::URIs(Vec::new(), None),
            None,
        ))
        .reserves_playback_activation());

        assert!(!ClientRequest::Player(PlayerRequest::Pause).reserves_playback_activation());
        assert!(!ClientRequest::Player(PlayerRequest::ResumePause).reserves_playback_activation());
        assert!(
            !ClientRequest::ActivePlaybackSeek(ActivePlaybackSeek::Absolute(
                std::time::Duration::from_secs(10),
            ))
            .reserves_playback_activation()
        );
        assert!(
            !ClientRequest::YouTubePlayer(YouTubePlayerRequest::Volume(50))
                .reserves_playback_activation()
        );
        assert!(!ClientRequest::GetCurrentUser.reserves_playback_activation());

        let play = ClientRequest::ResolveListenBrainzRecording {
            request_id: 1,
            context_uri: "spotify:artist:artist".to_owned(),
            recording_mbid: "recording".to_owned(),
            intent: ListenBrainzRecordingIntent::Play,
        };
        assert_eq!(play.domain(), RequestDomain::ProviderRead);
        assert_eq!(
            play.delivery_policy(),
            RequestDelivery::LatestWins(RequestKey::ListenBrainzResolution)
        );
        assert!(play.reserves_playback_activation());

        let menu = ClientRequest::ResolveListenBrainzRecording {
            request_id: 2,
            context_uri: "spotify:artist:artist".to_owned(),
            recording_mbid: "recording".to_owned(),
            intent: ListenBrainzRecordingIntent::OpenMenu,
        };
        assert_eq!(menu.domain(), RequestDomain::ProviderRead);
        assert!(!menu.reserves_playback_activation());

        let album = ClientRequest::ResolveListenBrainzAlbum {
            request_id: 3,
            context_uri: "spotify:artist:artist".to_owned(),
            release_group_mbid: "release-group".to_owned(),
            intent: ListenBrainzAlbumIntent::OpenPage,
        };
        assert_eq!(album.domain(), RequestDomain::ProviderRead);
        assert_eq!(
            album.delivery_policy(),
            RequestDelivery::LatestWins(RequestKey::ListenBrainzAlbumResolution)
        );
        assert!(!album.reserves_playback_activation());
    }

    #[test]
    fn listenbrainz_playlist_backup_is_a_coalesced_provider_mutation() {
        let request = ClientRequest::BackupUnifiedPlaylistToListenBrainz {
            unified_playlist_id: "playlist".to_owned(),
            operation_reference: "ui-0001".to_owned(),
        };
        assert_eq!(request.domain(), RequestDomain::PlaylistMutation);
        assert_eq!(
            request.delivery_policy(),
            RequestDelivery::Coalescible(RequestKey::ListenBrainzPlaylistBackup)
        );
        assert_eq!(request.provider(), None);
    }

    #[test]
    fn listenbrainz_sync_check_is_an_ordered_read_with_no_provider_mutation_domain() {
        let request = ClientRequest::CheckUnifiedPlaylistListenBrainzSync {
            unified_playlist_id: "playlist".to_owned(),
            operation_reference: "ui-0002".to_owned(),
            mode: ListenBrainzSyncReadMode::Preview,
        };
        assert_eq!(request.domain(), RequestDomain::ProviderRead);
        assert_eq!(request.delivery_policy(), RequestDelivery::Ordered);
        assert_eq!(request.provider(), None);
        assert_eq!(
            request.operation_name(),
            "check_unified_playlist_listenbrainz_sync"
        );
    }

    #[test]
    fn listenbrainz_sync_init_is_an_ordered_read_with_no_provider_mutation_domain() {
        let request = ClientRequest::InitializeUnifiedPlaylistListenBrainzBase {
            unified_playlist_id: "playlist".to_owned(),
            operation_reference: "ui-0003".to_owned(),
        };
        assert_eq!(request.domain(), RequestDomain::ProviderRead);
        assert_eq!(request.delivery_policy(), RequestDelivery::Ordered);
        assert_eq!(request.provider(), None);
        assert_eq!(
            request.operation_name(),
            "initialize_unified_playlist_listenbrainz_base"
        );
    }

    #[test]
    fn listenbrainz_sync_applies_are_ordered_provider_mutations() {
        for request in [
            ClientRequest::ApplyUnifiedPlaylistListenBrainzPush {
                unified_playlist_id: "playlist".to_owned(),
                operation_reference: "ui-0004".to_owned(),
                operation_id: "ui-0004".to_owned(),
            },
            ClientRequest::ApplyUnifiedPlaylistListenBrainzPull {
                unified_playlist_id: "playlist".to_owned(),
                operation_reference: "ui-0005".to_owned(),
                operation_id: "ui-0005".to_owned(),
            },
            ClientRequest::ApplyUnifiedPlaylistListenBrainzResolve {
                unified_playlist_id: "playlist".to_owned(),
                operation_reference: "ui-0006".to_owned(),
                operation_id: "ui-0006".to_owned(),
                policy:
                    crate::client::listenbrainz_resolution::ResolutionPolicy::MergeNonConflicting,
                decisions: Vec::new(),
            },
        ] {
            assert_eq!(request.domain(), RequestDomain::PlaylistMutation);
            assert_eq!(request.delivery_policy(), RequestDelivery::Ordered);
            assert_eq!(request.provider(), None);
        }
        assert_eq!(
            ClientRequest::ApplyUnifiedPlaylistListenBrainzPush {
                unified_playlist_id: "playlist".to_owned(),
                operation_reference: "ui-0004".to_owned(),
                operation_id: "ui-0004".to_owned(),
            }
            .operation_name(),
            "apply_unified_playlist_listenbrainz_push"
        );
    }

    #[test]
    fn listenbrainz_artist_enrichment_is_independent_provider_read_work() {
        let request = ClientRequest::EnrichListenBrainzArtist {
            request_id: 1,
            context_uri: "spotify:artist:artist".to_owned(),
            spotify_artist_id: "artist".to_owned(),
            include_recordings: true,
            include_release_groups: true,
        };

        assert_eq!(request.domain(), RequestDomain::ProviderRead);
        assert_eq!(
            request.delivery_policy(),
            RequestDelivery::LatestWins(RequestKey::ListenBrainzArtistEnrichment)
        );
        assert!(!request.reserves_playback_activation());
    }

    #[test]
    fn request_domains_route_to_one_focused_handler() {
        assert_eq!(
            ClientRequest::ReauthenticateSpotify.domain(),
            RequestDomain::AuthSession
        );
        assert_eq!(
            ClientRequest::InitializeSpotifySession.domain(),
            RequestDomain::AuthSession
        );
        assert_eq!(
            ClientRequest::Search {
                query: "query".to_string(),
                lifecycle_reference: String::new(),
            }
            .domain(),
            RequestDomain::ProviderRead
        );
        assert_eq!(
            ClientRequest::SwitchProvider(crate::config::ActiveProvider::YouTubeMusic).domain(),
            RequestDomain::Playback
        );
        assert_eq!(
            ClientRequest::AddItemsToUserQueue(Vec::new()).domain(),
            RequestDomain::PlaylistMutation
        );
        assert_eq!(
            ClientRequest::ManageAccount(AccountOperation::Validate(
                crate::config::ActiveProvider::YouTubeMusic,
            ))
            .domain(),
            RequestDomain::AuthSession
        );
        assert_eq!(
            ClientRequest::ResetAllConfiguration.domain(),
            RequestDomain::AuthSession
        );
    }

    #[test]
    fn playlist_creation_requests_keep_provider_ownership_explicit() {
        let spotify = ClientRequest::CreatePlaylist {
            playlist_name: "spotify".to_owned(),
            public: false,
            collab: false,
            desc: String::new(),
        };
        assert_eq!(spotify.operation_name(), "create_spotify_playlist");
        assert_eq!(
            spotify.provider(),
            Some(crate::observability::ProviderKind::Spotify)
        );

        let youtube = ClientRequest::CreateYouTubePlaylist {
            playlist_name: "youtube".to_owned(),
            public: false,
        };
        assert_eq!(youtube.operation_name(), "create_youtube_playlist");
        assert_eq!(
            youtube.provider(),
            Some(crate::observability::ProviderKind::YoutubeMusic)
        );

        let unified = ClientRequest::CreateUnifiedPlaylist {
            playlist_name: "unified".to_owned(),
        };
        assert_eq!(unified.operation_name(), "create_unified_playlist");
        assert_eq!(unified.provider(), None);
        assert_eq!(unified.domain(), RequestDomain::PlaylistMutation);

        let generated = ClientRequest::CreateUnifiedPlaylistFromHistory {
            playlist_name: "history".to_owned(),
            items: Vec::new(),
        };
        assert_eq!(
            generated.operation_name(),
            "create_unified_playlist_from_history"
        );
        assert_eq!(generated.provider(), None);
        assert_eq!(generated.domain(), RequestDomain::PlaylistMutation);
        assert_eq!(generated.delivery_policy(), RequestDelivery::Ordered);

        let link = ClientRequest::LinkUnifiedPlaylistToYouTube {
            unified_playlist_id: "local-id".to_owned(),
            youtube_playlist_id: "youtube-id".to_owned(),
        };
        assert_eq!(link.operation_name(), "link_unified_playlist_to_youtube");
        assert_eq!(link.provider(), None);
        assert_eq!(link.domain(), RequestDomain::PlaylistMutation);
        assert_eq!(link.delivery_policy(), RequestDelivery::Ordered);

        let sync = ClientRequest::SyncUnifiedPlaylistToYouTube {
            unified_playlist_id: "local-id".to_owned(),
        };
        assert_eq!(sync.operation_name(), "sync_unified_playlist_to_youtube");
        assert_eq!(sync.provider(), None);
        assert_eq!(sync.domain(), RequestDomain::PlaylistMutation);
        assert_eq!(sync.delivery_policy(), RequestDelivery::Ordered);

        let unlink = ClientRequest::UnlinkUnifiedPlaylistFromYouTube {
            unified_playlist_id: "local-id".to_owned(),
        };
        assert_eq!(
            unlink.operation_name(),
            "unlink_unified_playlist_from_youtube"
        );
        assert_eq!(unlink.provider(), None);
        assert_eq!(unlink.domain(), RequestDomain::PlaylistMutation);
        assert_eq!(unlink.delivery_policy(), RequestDelivery::Ordered);
    }

    #[test]
    fn native_playlist_mutations_keep_provider_ownership_and_ordered_delivery() {
        let spotify =
            ClientRequest::SpotifyPlaylistMutation(crate::client::SpotifyMutationIntent::Reorder {
                operation_id: crate::client::PlaylistMutationOperationId(1),
                playlist_id: "spotify".to_owned(),
                range_start: 0,
                insert_before: 2,
                range_length: 1,
                snapshot_id: "base".to_owned(),
            });
        assert_eq!(spotify.operation_name(), "mutate_spotify_playlist");
        assert_eq!(
            spotify.provider(),
            Some(crate::observability::ProviderKind::Spotify)
        );
        assert_eq!(spotify.domain(), RequestDomain::PlaylistMutation);
        assert_eq!(spotify.delivery_policy(), RequestDelivery::Ordered);

        let youtube =
            ClientRequest::YouTubePlaylistMutation(crate::client::YouTubeMutationIntent::Reorder {
                operation_id: crate::client::PlaylistMutationOperationId(2),
                playlist_id: "youtube".to_owned(),
            });
        assert_eq!(youtube.operation_name(), "mutate_youtube_playlist");
        assert_eq!(
            youtube.provider(),
            Some(crate::observability::ProviderKind::YoutubeMusic)
        );
        assert_eq!(youtube.domain(), RequestDomain::PlaylistMutation);
        assert_eq!(youtube.delivery_policy(), RequestDelivery::Ordered);
    }

    #[test]
    fn nested_playlist_request_keeps_one_metadata_descriptor() {
        let request = ClientRequest::Playlist(PlaylistRequest::new(
            crate::client::PlaylistMutationOperationId(7),
            PlaylistRequestKind::CreateUnifiedPlaylist {
                playlist_name: "local".to_owned(),
            },
        ));
        assert_eq!(request.operation_name(), "create_unified_playlist");
        assert_eq!(request.provider(), None);
        assert_eq!(request.domain(), RequestDomain::PlaylistMutation);
        assert_eq!(request.delivery_policy(), RequestDelivery::Ordered);
    }

    #[test]
    fn unified_seed_append_request_keeps_operation_envelope_and_ordering() {
        let seed = crate::state::PlaylistSeedItem {
            media_id: crate::state::MediaId {
                provider: crate::state::Provider::Spotify,
                kind: crate::state::MediaKind::Track,
                raw_id: "track-1".to_owned(),
            },
            title: "Track".to_owned(),
            artists: "Artist".to_owned(),
            album: None,
            duration_ms: Some(1_000),
            explicit: Some(false),
            provider_url: Some("spotify:track:track-1".to_owned()),
            artwork_url: None,
            metadata_degraded: false,
            metadata_pending: false,
        };
        let operation = crate::state::PlaylistOperationEnvelope::new(
            "operation-1",
            "idempotency-1",
            Some(crate::state::Provider::Spotify),
            Some(3),
            crate::state::PlaylistDestination::Existing {
                target: crate::state::PlaylistTargetKind::Unified,
                id: "local-1".to_owned(),
            },
            crate::state::PlaylistIntent::Append {
                seed: vec![seed.clone()],
            },
            Some("revision-1".to_owned()),
        );
        let request = ClientRequest::AddItemsToUnifiedPlaylist {
            playlist_id: "local-1".to_owned(),
            items: vec![seed.into_unified_playlist_item()],
            operation: Some(operation.clone()),
        };
        assert_eq!(request.operation_name(), "add_unified_playlist_items");
        assert_eq!(request.domain(), RequestDomain::PlaylistMutation);
        assert_eq!(request.delivery_policy(), RequestDelivery::Ordered);
        let ClientRequest::AddItemsToUnifiedPlaylist {
            operation: Some(received),
            ..
        } = request
        else {
            panic!("expected a correlated Unified append request");
        };
        assert_eq!(received, operation);
    }

    #[test]
    fn delivery_policy_distinguishes_guaranteed_ordered_latest_and_duplicate_work() {
        assert_eq!(
            ClientRequest::ShutdownPlayback.delivery_policy(),
            RequestDelivery::MustDeliver
        );
        assert_eq!(
            ClientRequest::Player(PlayerRequest::Pause).delivery_policy(),
            RequestDelivery::Ordered
        );
        let active_control = ClientRequest::ActivePlaybackControl(ActivePlaybackControl::Toggle);
        assert_eq!(active_control.operation_name(), "playback_toggle_pause");
        assert_eq!(active_control.provider(), None);
        assert_eq!(active_control.domain(), RequestDomain::Playback);
        assert_eq!(
            active_control.delivery_policy(),
            RequestDelivery::LatestWins(RequestKey::ActivePlaybackControl)
        );
        let active_seek = ClientRequest::ActivePlaybackSeek(ActivePlaybackSeek::Relative(
            chrono::Duration::seconds(5),
        ));
        assert_eq!(active_seek.operation_name(), "playback_seek");
        assert_eq!(active_seek.provider(), None);
        assert_eq!(active_seek.domain(), RequestDomain::Playback);
        assert_eq!(
            active_seek.delivery_policy(),
            RequestDelivery::LatestWins(RequestKey::ActivePlaybackSeek)
        );
        assert_eq!(
            ClientRequest::Search {
                query: "latest".to_string(),
                lifecycle_reference: String::new(),
            }
            .delivery_policy(),
            RequestDelivery::LatestWins(RequestKey::SpotifySearch)
        );
        assert_eq!(
            ClientRequest::GetSpotifyUser("alice".to_string()).delivery_policy(),
            RequestDelivery::LatestWins(RequestKey::SpotifyUserSearch)
        );
        assert_eq!(
            ClientRequest::UnifiedNext.delivery_policy(),
            RequestDelivery::LatestWins(RequestKey::PlaybackSelection)
        );
        assert_eq!(
            ClientRequest::GetCurrentPlayback.delivery_policy(),
            RequestDelivery::Coalescible(RequestKey::CurrentPlayback)
        );
        assert_eq!(
            ClientRequest::ManageAccount(AccountOperation::Validate(
                crate::config::ActiveProvider::Spotify,
            ))
            .delivery_policy(),
            RequestDelivery::Ordered
        );
    }

    #[test]
    fn account_operations_never_carry_credentials() {
        let request = ClientRequest::ManageAccount(AccountOperation::Switch {
            provider: crate::config::ActiveProvider::Spotify,
            account_id: "spotify-2".to_string(),
        });
        assert!(request.is_account_change());
        assert_eq!(request.operation_name(), "manage_account");
        assert_eq!(
            request.provider(),
            Some(crate::observability::ProviderKind::Spotify)
        );
    }
}
