use crate::{
    command,
    state::{
        model::{
            Album, Artist, Episode, EpisodeId, MediaId, PlayableMedia, Playlist, PlaylistEntryId,
            PlaylistSeedItem, Show, Track, UnifiedPlaylistItem, YouTubeTrack,
        },
        ItemId, OccurrenceDescriptor, QueueSelectionScope, UnifiedPlaylistSelectionScope,
    },
    ui::single_line_input::LineInput,
};
use ratatui::{layout::Rect, widgets::ListState};
use rspotify::model::PlaylistId;

#[cfg(feature = "private-capture")]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum PrivateCapturePassphraseAction {
    Arm,
    ReviewCapture,
    Compare,
    ReplayOffline,
    ReplayFresh,
    PreviewDerivative,
    CreateDerivative,
}

#[cfg(feature = "private-capture")]
impl PrivateCapturePassphraseAction {
    pub(crate) const fn title(self) -> &'static str {
        match self {
            Self::Arm => "Consent to Private Capture",
            Self::ReviewCapture => "Review Encrypted Capture",
            Self::Compare => "Compare Encrypted Captures",
            Self::ReplayOffline => "Offline Replay",
            Self::ReplayFresh => "Fresh Network Replay",
            Self::PreviewDerivative => "Preview Safe Derivative",
            Self::CreateDerivative => "Create Safe Derivative",
        }
    }
}

#[cfg(feature = "private-capture")]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum PrivateCaptureConfirmation {
    FreshReplayFirst,
    FreshReplaySecond,
    OpenEncryptedFolder,
    DeleteSelected,
}

#[cfg(feature = "private-capture")]
impl PrivateCaptureConfirmation {
    pub(crate) const fn message(self) -> &'static str {
        match self {
            Self::FreshReplayFirst => {
                "Fresh replay can contact YouTube Music using current local authentication. Continue"
            }
            Self::FreshReplaySecond => {
                "This is the final confirmation before one bounded network replay. Continue"
            }
            Self::OpenEncryptedFolder => {
                "The folder contains encrypted private captures. Open it locally"
            }
            Self::DeleteSelected => {
                "Delete the selected encrypted capture and its private evidence"
            }
        }
    }
}

#[cfg(feature = "private-capture")]
#[derive(Debug, Default)]
pub(crate) struct PrivateCaptureSelectorState {
    pub(crate) list: ListState,
    pub(crate) selected: Option<crate::developer_capture::SafeCaptureRef>,
}

#[cfg(feature = "private-capture")]
impl PrivateCaptureSelectorState {
    pub(crate) fn new(
        artifacts: &[crate::developer_capture::SafeOperatorArtifact],
        selected: Option<crate::developer_capture::SafeCaptureRef>,
    ) -> Self {
        let mut state = Self {
            list: ListState::default(),
            selected,
        };
        state.synchronize(artifacts);
        state
    }

    pub(crate) fn synchronize(
        &mut self,
        artifacts: &[crate::developer_capture::SafeOperatorArtifact],
    ) {
        if artifacts.is_empty() {
            self.list.select(None);
            self.selected = None;
            return;
        }
        let old_index = self.list.selected().unwrap_or_default();
        let index = self
            .selected
            .and_then(|capture_ref| {
                artifacts
                    .iter()
                    .position(|artifact| artifact.capture_ref == capture_ref)
            })
            .unwrap_or_else(|| old_index.min(artifacts.len() - 1));
        self.list.select(Some(index));
        self.selected = Some(artifacts[index].capture_ref);
    }

    pub(crate) fn select_index(
        &mut self,
        artifacts: &[crate::developer_capture::SafeOperatorArtifact],
        index: usize,
    ) {
        if let Some(artifact) = artifacts.get(index) {
            self.list.select(Some(index));
            self.selected = Some(artifact.capture_ref);
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PlaylistCreateCurrentField {
    Target,
    Name,
    Desc,
}

impl PlaylistCreateCurrentField {
    pub const fn next(self, target: PlaylistCreateTarget) -> Self {
        match (self, target.supports_description()) {
            (Self::Target, _) => Self::Name,
            (Self::Name, true) => Self::Desc,
            (Self::Name, false) | (Self::Desc, _) => Self::Target,
        }
    }

    pub const fn previous(self, target: PlaylistCreateTarget) -> Self {
        match (self, target.supports_description()) {
            (Self::Target, true) => Self::Desc,
            (Self::Target, false) | (Self::Desc, _) => Self::Name,
            (Self::Name, _) => Self::Target,
        }
    }
}

/// The destination selected by the create workflow.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PlaylistCreateTarget {
    Spotify,
    YouTubeMusic,
    Unified,
}

impl PlaylistCreateTarget {
    pub const fn label(self) -> &'static str {
        match self {
            Self::Spotify => "Spotify",
            Self::YouTubeMusic => "YouTube Music",
            Self::Unified => "Unified (local)",
        }
    }

    pub const fn supports_description(self) -> bool {
        matches!(self, Self::Spotify)
    }

    /// A pending source can be sent to Unified or to the same native provider.
    /// Cross-provider native creation remains intentionally unsupported until a
    /// provider resolution adapter is introduced in a later phase.
    pub const fn accepts_source(self, source: Option<crate::state::Provider>) -> bool {
        match self {
            Self::Unified => true,
            Self::Spotify => matches!(source, Some(crate::state::Provider::Spotify)),
            Self::YouTubeMusic => matches!(source, Some(crate::state::Provider::YouTubeMusic)),
        }
    }

    pub const fn next(self) -> Self {
        match self {
            Self::Spotify => Self::YouTubeMusic,
            Self::YouTubeMusic => Self::Unified,
            Self::Unified => Self::Spotify,
        }
    }

    pub const fn previous(self) -> Self {
        match self {
            Self::Spotify => Self::Unified,
            Self::YouTubeMusic => Self::Spotify,
            Self::Unified => Self::YouTubeMusic,
        }
    }
}

#[derive(Debug, Clone)]
pub enum JournalListNameAction {
    Create,
    CreateWithTracks { tracks: Vec<Track> },
    CreateWithYouTubeTracks { tracks: Vec<YouTubeTrack> },
    Rename { list_id: String },
}

#[derive(Debug, Clone)]
pub enum PlaylistNameAction {
    Spotify { playlist_id: PlaylistId<'static> },
    YouTubeMusic { playlist_id: String },
    Unified { playlist_id: String },
}

#[derive(Debug, Clone)]
pub enum JournalListPopupAction {
    AddTracks { tracks: Vec<Track> },
    AddYouTubeTracks { tracks: Vec<YouTubeTrack> },
}

#[derive(Debug)]
pub enum PopupState {
    Search {
        query: String,
    },
    /// Fine volume control opened from the playback volume slider or symbol.
    /// `input` holds a typed percentage until Enter applies it.
    Volume {
        anchor: Rect,
        input: crate::ui::single_line_input::LineInput,
    },
    /// Frontend-only action handoff.  The UI owns the interaction while a
    /// provider/backend adapter is still being wired in.
    DeferredAction {
        title: String,
        message: String,
    },
    SpotifyUserSearch {
        query: String,
    },
    SpotifyUserCandidates {
        query: String,
        candidates: Vec<SpotifyUserCandidate>,
        state: ListState,
    },
    SpotifyUserPlaylists {
        profile: SpotifyUserProfile,
        playlists: Vec<Playlist>,
        state: ListState,
    },
    SpotifyUserProfile {
        profile: SpotifyUserProfile,
    },
    UnifiedPlaylistDestination {
        item_count: usize,
        items: Vec<UnifiedPlaylistItem>,
        options: Vec<(String, String)>,
        state: ListState,
    },
    UserPlaylistList(PlaylistPopupAction, ListState),
    YouTubePlaylistList(YouTubePlaylistPopupAction, ListState),
    YouTubeArtistMenu {
        details: crate::state::YouTubeArtistContext,
        state: ListState,
    },
    UserFollowedArtistList(ListState),
    UserSavedAlbumList(ListState),
    DeviceList(ListState),
    ArtistList(ArtistPopupAction, Vec<Artist>, ListState),
    ThemeList(Vec<crate::config::Theme>, ListState),
    ActionList(Box<ActionListItem>, ListState),
    AnchoredActionList {
        item: Box<ActionListItem>,
        state: ListState,
        anchor: Rect,
    },
    DiagnosticActions {
        target: crate::observability::DiagnosticRowId,
        actions: Vec<crate::observability::DiagnosticAction>,
        state: ListState,
    },
    DiagnosticDetail {
        title: String,
        lines: Vec<String>,
        scroll_offset: usize,
    },
    CommandHelp {
        scroll_offset: usize,
    },
    ListenBrainzSyncDetails {
        preview: super::ListenBrainzSyncPreview,
        state: ListState,
    },
    ListenBrainzWorkspace {
        playlist_id: String,
        playlist_name: String,
        state: ListState,
        changes: ratatui::widgets::TableState,
    },
    ListenBrainzResolve {
        menu: ListenBrainzResolveMenu,
        state: ListState,
    },
    #[cfg(feature = "private-capture")]
    PrivateDerivativePreview {
        preview: std::sync::Arc<crate::developer_capture::SafeDerivativePreview>,
        scroll_offset: usize,
        rendered_row_count: usize,
    },
    #[cfg(feature = "private-capture")]
    PrivateCaptureSelector {
        state: PrivateCaptureSelectorState,
    },
    #[cfg(feature = "private-capture")]
    PrivateCapturePassphrase {
        action: PrivateCapturePassphraseAction,
        input: crate::developer_capture::CapturePassphraseInput,
    },
    #[cfg(feature = "private-capture")]
    PrivateCaptureConfirm {
        action: PrivateCaptureConfirmation,
    },
    PlaylistCreate {
        target: PlaylistCreateTarget,
        public: bool,
        name: LineInput,
        desc: LineInput,
        current_field: PlaylistCreateCurrentField,
        /// Source rows are captured once and remain provider-neutral while the
        /// user chooses a destination and enters its name.
        pending_items: Option<Vec<PlaylistSeedItem>>,
        /// Provider-local selection epoch captured when source rows entered the
        /// workflow.  A request can be rejected if the source projection has
        /// changed while the user was naming the destination.
        source_provider: Option<crate::state::Provider>,
        source_epoch: Option<u64>,
    },
    TrackRating {
        track: Track,
        state: ListState,
    },
    TrackNote {
        track: Track,
        input: LineInput,
    },
    JournalListSelect(JournalListPopupAction, ListState),
    /// Name a local unified playlist generated from a captured history slice.
    SessionHistoryCreate {
        items: Vec<UnifiedPlaylistItem>,
        input: LineInput,
    },
    JournalListName {
        action: JournalListNameAction,
        input: LineInput,
    },
    PlaylistName {
        action: PlaylistNameAction,
        input: LineInput,
    },
    ListenBrainzPlaylists {
        operation: u64,
        identity: crate::client::listenbrainz::ValidatedListenBrainzIdentity,
        rows: Vec<crate::client::listenbrainz::ListenBrainzPlaylistSummary>,
        state: ListState,
        busy: bool,
        notice: String,
    },
    ListenBrainzToken {
        input: crate::ui::single_line_input::SecretInput,
    },
    ConfigEdit {
        key: String,
        input: LineInput,
    },
    ConfigChoice {
        key: String,
        options: Vec<String>,
        state: ListState,
    },
    ConfigMultiChoice {
        key: String,
        options: Vec<String>,
        selected: Vec<bool>,
        state: ListState,
    },
    ConfirmAction {
        message: String,
        action: ConfirmableAction,
    },
    WorkspaceScope {
        kind: crate::state::WorkspaceScopeKind,
        options: Vec<WorkspaceScopeOption>,
        state: ListState,
        anchor: ratatui::layout::Rect,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SpotifyUserProfile {
    pub id: String,
    pub display_name: Option<String>,
    pub profile_url: String,
    pub lookup_note: Option<String>,
}

#[derive(Debug, Clone)]
pub struct SpotifyUserCandidate {
    pub profile: SpotifyUserProfile,
    pub playlists: Vec<Playlist>,
}

#[cfg(test)]
mod playlist_creation_target_tests {
    use super::{PlaylistCreateCurrentField, PlaylistCreateTarget};

    #[test]
    fn destination_cycle_is_stable_and_wraps() {
        assert_eq!(
            PlaylistCreateTarget::Spotify.next(),
            PlaylistCreateTarget::YouTubeMusic
        );
        assert_eq!(
            PlaylistCreateTarget::YouTubeMusic.next(),
            PlaylistCreateTarget::Unified
        );
        assert_eq!(
            PlaylistCreateTarget::Unified.next(),
            PlaylistCreateTarget::Spotify
        );
        assert_eq!(
            PlaylistCreateTarget::Spotify.previous(),
            PlaylistCreateTarget::Unified
        );
        assert_eq!(
            PlaylistCreateTarget::Unified.previous(),
            PlaylistCreateTarget::YouTubeMusic
        );
    }

    #[test]
    fn destination_labels_are_user_facing_and_distinct() {
        let labels = [
            PlaylistCreateTarget::Spotify.label(),
            PlaylistCreateTarget::YouTubeMusic.label(),
            PlaylistCreateTarget::Unified.label(),
        ];
        assert_eq!(labels, ["Spotify", "YouTube Music", "Unified (local)"]);
    }

    #[test]
    fn destination_controls_description_field_visibility_and_navigation() {
        assert!(PlaylistCreateTarget::Spotify.supports_description());
        assert!(!PlaylistCreateTarget::YouTubeMusic.supports_description());
        assert!(!PlaylistCreateTarget::Unified.supports_description());

        assert_eq!(
            PlaylistCreateCurrentField::Name.next(PlaylistCreateTarget::Spotify),
            PlaylistCreateCurrentField::Desc
        );
        assert_eq!(
            PlaylistCreateCurrentField::Name.next(PlaylistCreateTarget::Unified),
            PlaylistCreateCurrentField::Target
        );
        assert_eq!(
            PlaylistCreateCurrentField::Target.previous(PlaylistCreateTarget::YouTubeMusic),
            PlaylistCreateCurrentField::Name
        );
        assert_eq!(
            PlaylistCreateCurrentField::Target.previous(PlaylistCreateTarget::Spotify),
            PlaylistCreateCurrentField::Desc
        );
    }

    #[test]
    fn source_acceptance_allows_unified_and_same_provider_only() {
        use crate::state::Provider;

        assert!(PlaylistCreateTarget::Unified.accepts_source(None));
        assert!(PlaylistCreateTarget::Unified.accepts_source(Some(Provider::Spotify)));
        assert!(PlaylistCreateTarget::Spotify.accepts_source(Some(Provider::Spotify)));
        assert!(!PlaylistCreateTarget::Spotify.accepts_source(Some(Provider::YouTubeMusic)));
        assert!(PlaylistCreateTarget::YouTubeMusic.accepts_source(Some(Provider::YouTubeMusic)));
        assert!(!PlaylistCreateTarget::YouTubeMusic.accepts_source(Some(Provider::Spotify)));
    }
}

/// Shared display model for numbered action popups. Ordinary actions may
/// provide a description, while diagnostics intentionally stay label-only;
/// both are rendered through the same bounded terminal row contract.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum PopupActionPayload {
    Ordinary(command::ActionDescriptor),
    Diagnostic(crate::observability::DiagnosticAction),
}

impl PopupActionPayload {
    pub(crate) const fn label(self) -> &'static str {
        match self {
            Self::Ordinary(descriptor) => descriptor.label,
            Self::Diagnostic(action) => action.label(),
        }
    }

    pub(crate) const fn description(self) -> Option<&'static str> {
        match self {
            Self::Ordinary(descriptor) => Some(descriptor.description),
            Self::Diagnostic(_) => None,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct PopupActionEntry {
    label: String,
    description: Option<String>,
}

impl PopupActionEntry {
    pub(crate) fn from_payload(payload: PopupActionPayload) -> Self {
        match payload.description() {
            Some(description) => Self::described(payload.label(), description),
            None => Self::label(payload.label()),
        }
    }

    pub(crate) fn label(label: impl Into<String>) -> Self {
        Self {
            label: label.into(),
            description: None,
        }
    }

    pub(crate) fn described(label: impl Into<String>, description: impl Into<String>) -> Self {
        Self {
            label: label.into(),
            description: Some(description.into()),
        }
    }

    pub(crate) fn label_text(&self) -> &str {
        &self.label
    }

    pub(crate) fn description_text(&self) -> Option<&str> {
        self.description.as_deref().filter(|text| !text.is_empty())
    }

    #[cfg(test)]
    pub(crate) fn display_text(&self) -> String {
        match self.description.as_deref() {
            Some(description) if !description.is_empty() => {
                format!("{}: {}", self.label, description)
            }
            _ => self.label.clone(),
        }
    }
}

#[derive(Debug, Clone)]
#[allow(clippy::enum_variant_names)] // The prefix makes destructive confirmation variants explicit.
pub enum ConfirmableAction {
    SpotifyPlaylistMutation(crate::client::SpotifyMutationIntent),
    YouTubePlaylistMutation(crate::client::YouTubeMutationIntent),
    DeleteTracksFromPlaylist {
        playlist_id: PlaylistId<'static>,
        tracks: TracksActionMenu,
    },
    DeleteFromLibrary(ItemId),
    DeleteYouTubePlaylist(String),
    DeleteUnifiedPlaylist(String),
    /// A conflict-aware projection repair.  The preview state is captured
    /// before this action is confirmed; dispatch is never implicit.
    ReconcileUnifiedPlaylistProjection(String),
    RollbackUnifiedPlaylistListenBrainzPull {
        unified_playlist_id: String,
        listenbrainz_playlist_id: String,
    },
    ApplyUnifiedPlaylistListenBrainzPush {
        unified_playlist_id: String,
    },
    ApplyUnifiedPlaylistListenBrainzPull {
        unified_playlist_id: String,
    },
    ApplyUnifiedPlaylistListenBrainzResolve {
        unified_playlist_id: String,
        policy: ResolutionPolicy,
        decisions: Vec<crate::client::listenbrainz_resolution::ConflictDecision>,
    },
    RemoveUnifiedPlaylistEntries(UnifiedPlaylistActionMenu),
    DeleteJournalList(String),
    ClearSessionHistory,
    ClearHomeHistory,
    RemoveAccount {
        provider: crate::config::ActiveProvider,
        account_id: String,
    },
    ResetAllConfiguration,
}

#[derive(Debug, Clone)]
/// Immutable payload and action snapshot captured when a multi-item action
/// menu opens.  The payload remains unchanged while a provider refreshes; the
/// epoch is checked again before an effect is dispatched.
pub struct BulkActionMenu<T> {
    items: Vec<T>,
    actions: Vec<command::ActionDescriptor>,
    epoch: BulkActionSelectionEpoch,
}

/// Shared read-only contract for every multi-select action popup.
///
/// The payload and dispatch path may differ by surface, but action projection
/// and capability checks should not.  Surface-specific menus can expose a
/// smaller or larger action set without changing popup navigation.
pub trait MultiSelectActionMenu {
    type Item;

    fn items(&self) -> &[Self::Item];

    fn actions(&self) -> &[command::ActionDescriptor];

    fn supports_action(&self, action: command::Action) -> bool {
        !self.items().is_empty()
            && self
                .actions()
                .iter()
                .any(|descriptor| descriptor.action == action)
    }
}

impl<T> BulkActionMenu<T> {
    pub fn new(
        items: Vec<T>,
        actions: Vec<command::ActionDescriptor>,
        epoch: BulkActionSelectionEpoch,
    ) -> Self {
        Self {
            items,
            actions,
            epoch,
        }
    }

    pub fn items(&self) -> &[T] {
        &self.items
    }

    pub fn actions(&self) -> &[command::ActionDescriptor] {
        &self.actions
    }

    pub const fn epoch(&self) -> BulkActionSelectionEpoch {
        self.epoch
    }
}

impl<T> MultiSelectActionMenu for BulkActionMenu<T> {
    type Item = T;

    fn items(&self) -> &[Self::Item] {
        self.items()
    }

    fn actions(&self) -> &[command::ActionDescriptor] {
        self.actions()
    }
}

/// Privacy-safe provider selection epoch attached to a bulk menu snapshot.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct BulkActionSelectionEpoch {
    provider: crate::state::Provider,
    value: u64,
}

impl BulkActionSelectionEpoch {
    pub const fn new(provider: crate::state::Provider, value: u64) -> Self {
        Self { provider, value }
    }

    pub const fn provider(self) -> crate::state::Provider {
        self.provider
    }

    pub const fn value(self) -> u64 {
        self.value
    }
}

pub type TracksActionMenu = BulkActionMenu<Track>;
pub type YouTubeTracksActionMenu = BulkActionMenu<YouTubeTrack>;

/// A frozen Queue row payload captured when a multi-action menu opens.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum QueueActionPayload {
    Unified(PlayableMedia),
    Native(rspotify::model::PlayableId<'static>),
}

/// One Queue occurrence in an immutable multi-action snapshot.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct QueueActionItem {
    occurrence: OccurrenceDescriptor<MediaId, u64>,
    media_id: MediaId,
    payload: QueueActionPayload,
}

impl QueueActionItem {
    pub fn new(
        occurrence: OccurrenceDescriptor<MediaId, u64>,
        media_id: MediaId,
        payload: QueueActionPayload,
    ) -> Self {
        Self {
            occurrence,
            media_id,
            payload,
        }
    }

    pub fn occurrence(&self) -> &OccurrenceDescriptor<MediaId, u64> {
        &self.occurrence
    }

    pub const fn media_id(&self) -> &MediaId {
        &self.media_id
    }

    pub const fn payload(&self) -> &QueueActionPayload {
        &self.payload
    }
}

/// Immutable, exact-scope Queue action menu snapshot.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct QueueActionMenu {
    scope: QueueSelectionScope,
    items: Vec<QueueActionItem>,
    actions: Vec<command::ActionDescriptor>,
}

impl QueueActionMenu {
    pub fn new(scope: QueueSelectionScope, items: Vec<QueueActionItem>) -> Self {
        Self::with_actions(
            scope,
            items,
            [command::Action::CopyLink, command::Action::AddToQueue],
        )
    }

    /// Build a queue menu with the surface-specific action set.  Callers can
    /// keep the recommended default from `new` or explicitly add/remove
    /// actions as capabilities evolve.
    pub fn with_actions(
        scope: QueueSelectionScope,
        items: Vec<QueueActionItem>,
        actions: impl IntoIterator<Item = command::Action>,
    ) -> Self {
        Self {
            scope,
            items,
            actions: crate::command::BulkActionCandidates::new(actions)
                .as_slice()
                .iter()
                .map(|action| action.descriptor())
                .collect(),
        }
    }

    pub const fn scope(&self) -> &QueueSelectionScope {
        &self.scope
    }

    pub fn items(&self) -> &[QueueActionItem] {
        &self.items
    }

    pub fn actions(&self) -> &[command::ActionDescriptor] {
        &self.actions
    }
}

impl MultiSelectActionMenu for QueueActionMenu {
    type Item = QueueActionItem;

    fn items(&self) -> &[Self::Item] {
        self.items()
    }

    fn actions(&self) -> &[command::ActionDescriptor] {
        self.actions()
    }
}

/// One exact `UnifiedPlaylist` occurrence in an immutable action snapshot.
/// Degraded rows remain removable even when they cannot be queued.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UnifiedPlaylistActionItem {
    occurrence: OccurrenceDescriptor<MediaId, PlaylistEntryId>,
    item: UnifiedPlaylistItem,
}

impl UnifiedPlaylistActionItem {
    pub fn new(
        occurrence: OccurrenceDescriptor<MediaId, PlaylistEntryId>,
        item: UnifiedPlaylistItem,
    ) -> Self {
        Self { occurrence, item }
    }

    pub const fn occurrence(&self) -> &OccurrenceDescriptor<MediaId, PlaylistEntryId> {
        &self.occurrence
    }

    pub const fn item(&self) -> &UnifiedPlaylistItem {
        &self.item
    }
}

/// Immutable, exact-playlist-scope `UnifiedPlaylist` action menu snapshot.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UnifiedPlaylistActionMenu {
    scope: UnifiedPlaylistSelectionScope,
    items: Vec<UnifiedPlaylistActionItem>,
    actions: Vec<command::ActionDescriptor>,
}

/// Immutable action snapshot for local session-history entries. History can
/// contain Spotify and `YouTube` items together, so this menu keeps the
/// provider-neutral entry payload instead of forcing one backend type.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionHistoryActionMenu {
    items: Vec<crate::state::SessionEntry>,
    actions: Vec<command::ActionDescriptor>,
}

/// Actions for the lyrics page. The payload keeps the page identity so a
/// retry or provider cycle cannot accidentally target a different track.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LyricsActionMenu {
    track_uri: String,
    actions: Vec<command::ActionDescriptor>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UnifiedPlaylistContextActionMenu {
    id: String,
    name: String,
    actions: Vec<command::ActionDescriptor>,
}

impl UnifiedPlaylistContextActionMenu {
    #[allow(dead_code)]
    pub fn new(id: String, name: String, linked: bool) -> Self {
        let mut actions: Vec<_> = vec![command::Action::CopyLink];
        actions.extend(if linked {
            vec![
                command::Action::SyncUnifiedPlaylistToYouTube,
                command::Action::UnlinkUnifiedPlaylistFromYouTube,
            ]
        } else {
            vec![command::Action::LinkUnifiedPlaylistToYouTube]
        });
        actions.extend([
            command::Action::RenamePlaylist,
            command::Action::DeletePlaylist,
        ]);
        Self::with_actions(id, name, actions)
    }

    pub fn with_actions(
        id: String,
        name: String,
        actions: impl IntoIterator<Item = command::Action>,
    ) -> Self {
        Self {
            id,
            name,
            actions: actions
                .into_iter()
                .map(command::Action::descriptor)
                .collect(),
        }
    }

    pub fn id(&self) -> &str {
        &self.id
    }

    pub fn name(&self) -> &str {
        &self.name
    }

    pub fn actions(&self) -> &[command::ActionDescriptor] {
        &self.actions
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct YouTubePlaylistContextActionMenu {
    id: String,
    name: String,
    actions: Vec<command::ActionDescriptor>,
}

pub(crate) use crate::client::listenbrainz_resolution::{ResolutionPolicy, ResolutionSide};

/// Immutable conflict-resolution menu captured when the resolve popup opens.
///
/// Conflict positions address the sync plan in order (see
/// `ListenBrainzSyncPreview::conflicts`); row order here never changes while
/// the popup is open. No destructive choice is preselected: every conflict
/// starts undecided and the apply row stays disabled until a policy and all
/// decisions are chosen.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ListenBrainzResolveMenu {
    playlist_id: String,
    playlist_name: String,
    conflicts: Vec<super::ListenBrainzSyncConflictKind>,
    policy: Option<ResolutionPolicy>,
    decisions: Vec<Option<ResolutionSide>>,
}

impl ListenBrainzResolveMenu {
    pub fn new(
        playlist_id: String,
        playlist_name: String,
        conflicts: Vec<super::ListenBrainzSyncConflictKind>,
    ) -> Self {
        let decisions = vec![None; conflicts.len()];
        Self {
            playlist_id,
            playlist_name,
            conflicts,
            policy: None,
            decisions,
        }
    }

    pub fn playlist_id(&self) -> &str {
        &self.playlist_id
    }

    pub fn playlist_name(&self) -> &str {
        &self.playlist_name
    }

    pub const fn policy(&self) -> Option<ResolutionPolicy> {
        self.policy
    }

    pub fn row_count(&self) -> usize {
        3 + self.conflicts.len() + 1
    }

    fn policy_at(index: usize) -> Option<ResolutionPolicy> {
        match index {
            0 => Some(ResolutionPolicy::KeepLocal),
            1 => Some(ResolutionPolicy::KeepListenBrainz),
            2 => Some(ResolutionPolicy::MergeNonConflicting),
            _ => None,
        }
    }

    const fn policy_label(policy: ResolutionPolicy) -> &'static str {
        match policy {
            ResolutionPolicy::KeepLocal => "Keep local",
            ResolutionPolicy::KeepListenBrainz => "Keep ListenBrainz",
            ResolutionPolicy::MergeNonConflicting => "Merge non-conflicting",
        }
    }

    pub fn row_label(&self, index: usize) -> Option<String> {
        if let Some(policy) = Self::policy_at(index) {
            let marker = if self.policy == Some(policy) {
                "[x]"
            } else {
                "[ ]"
            };
            return Some(format!("{marker} Policy: {}", Self::policy_label(policy)));
        }
        let apply_index = 3 + self.conflicts.len();
        if index == apply_index {
            return Some(if self.is_ready() {
                format!("Apply resolution ({})", self.apply_summary())
            } else {
                "Apply resolution (choose a policy and every conflict first)".to_owned()
            });
        }
        let conflict_index = index.checked_sub(3)?;
        let kind = self.conflicts.get(conflict_index)?;
        let choice = match self.decisions.get(conflict_index).copied().flatten() {
            None => "undecided",
            Some(ResolutionSide::Local) => "local",
            Some(ResolutionSide::ListenBrainz) => "listenbrainz",
        };
        Some(format!(
            "[{}] Conflict {}: {} — keep {choice}",
            conflict_index + 1,
            conflict_index + 1,
            kind.label(),
        ))
    }

    pub fn select_policy(&mut self, index: usize) -> bool {
        let Some(policy) = Self::policy_at(index) else {
            return false;
        };
        self.policy = Some(policy);
        true
    }

    /// Cycle one conflict through undecided → local → listenbrainz.
    /// Undecided is never skipped and no destructive side is preselected.
    pub fn cycle_decision(&mut self, index: usize) -> bool {
        let Some(conflict_index) = index.checked_sub(3) else {
            return false;
        };
        let Some(decision) = self.decisions.get_mut(conflict_index) else {
            return false;
        };
        *decision = Some(match *decision {
            None | Some(ResolutionSide::ListenBrainz) => ResolutionSide::Local,
            Some(ResolutionSide::Local) => ResolutionSide::ListenBrainz,
        });
        true
    }

    pub fn is_ready(&self) -> bool {
        self.policy.is_some() && self.decisions.iter().all(Option::is_some)
    }

    /// Mirror of the client fail-closed rule for unsafe remote rows: rows the
    /// TUI cannot map (drift, unlinked, duplicate, schema) must never resolve
    /// to the `ListenBrainz` side. Unlinked-row mapping stays a CLI operation.
    pub fn has_unsafe_remote_choice(&self) -> bool {
        use super::ListenBrainzSyncConflictKind as Kind;
        self.conflicts.iter().enumerate().any(|(index, kind)| {
            let unsafe_kind = matches!(
                kind,
                Kind::ManifestProjectionDrift
                    | Kind::UnlinkedRemoteRow
                    | Kind::DuplicateAmbiguity
                    | Kind::Schema
            );
            unsafe_kind
                && (self.policy == Some(ResolutionPolicy::KeepListenBrainz)
                    || self.decisions.get(index).copied().flatten()
                        == Some(ResolutionSide::ListenBrainz))
        })
    }

    pub fn apply_summary(&self) -> String {
        let local = self
            .decisions
            .iter()
            .filter(|decision| **decision == Some(ResolutionSide::Local))
            .count();
        let remote = self
            .decisions
            .iter()
            .filter(|decision| **decision == Some(ResolutionSide::ListenBrainz))
            .count();
        let policy = self.policy.map_or("no policy", Self::policy_label);
        format!("{policy} ({local} local, {remote} ListenBrainz)")
    }

    pub fn decisions(&self) -> Vec<crate::client::listenbrainz_resolution::ConflictDecision> {
        self.decisions
            .iter()
            .enumerate()
            .filter_map(|(conflict_index, side)| {
                side.map(
                    |side| crate::client::listenbrainz_resolution::ConflictDecision {
                        conflict_index,
                        side,
                    },
                )
            })
            .collect()
    }
}

impl YouTubePlaylistContextActionMenu {
    #[allow(dead_code)]
    pub fn new(id: String, name: String, editable: bool) -> Self {
        let mut actions = vec![command::Action::CopyLink];
        if editable {
            actions.extend([
                command::Action::RenamePlaylist,
                command::Action::DeletePlaylist,
            ]);
        }
        Self::with_actions(id, name, actions)
    }

    pub fn with_actions(
        id: String,
        name: String,
        actions: impl IntoIterator<Item = command::Action>,
    ) -> Self {
        Self {
            id,
            name,
            actions: actions
                .into_iter()
                .map(command::Action::descriptor)
                .collect(),
        }
    }

    pub fn id(&self) -> &str {
        &self.id
    }

    pub fn name(&self) -> &str {
        &self.name
    }

    pub fn actions(&self) -> &[command::ActionDescriptor] {
        &self.actions
    }
}

impl LyricsActionMenu {
    pub fn new(track_uri: String, has_lyrics: bool, has_timestamps: bool) -> Self {
        let mut actions = Vec::new();
        if has_lyrics {
            actions.push(command::Action::CopyLyrics);
        }
        if has_timestamps {
            actions.push(command::Action::CopyTimedLyrics);
        }
        actions.extend([
            command::Action::RetryLyrics,
            command::Action::CycleLyricsSource,
        ]);
        Self {
            track_uri,
            actions: actions
                .into_iter()
                .map(command::Action::descriptor)
                .collect(),
        }
    }

    pub fn track_uri(&self) -> &str {
        &self.track_uri
    }

    pub fn actions(&self) -> &[command::ActionDescriptor] {
        &self.actions
    }
}

impl SessionHistoryActionMenu {
    pub fn new(items: Vec<crate::state::SessionEntry>) -> Self {
        Self {
            items,
            actions: [
                command::Action::CopyLink,
                command::Action::AddToPlaylist,
                command::Action::ClearSessionHistory,
            ]
            .into_iter()
            .map(command::Action::descriptor)
            .collect(),
        }
    }

    pub fn items(&self) -> &[crate::state::SessionEntry] {
        &self.items
    }

    pub fn actions(&self) -> &[command::ActionDescriptor] {
        &self.actions
    }
}

impl MultiSelectActionMenu for SessionHistoryActionMenu {
    type Item = crate::state::SessionEntry;

    fn items(&self) -> &[Self::Item] {
        self.items()
    }

    fn actions(&self) -> &[command::ActionDescriptor] {
        self.actions()
    }
}

impl UnifiedPlaylistActionMenu {
    pub fn new(
        scope: UnifiedPlaylistSelectionScope,
        items: Vec<UnifiedPlaylistActionItem>,
    ) -> Self {
        let mut actions = vec![command::Action::CopyLink, command::Action::AddToPlaylist];
        if items
            .iter()
            .all(|item| item.item.playable_media().is_some())
        {
            actions.push(command::Action::AddToQueue);
        }
        actions.push(command::Action::DeleteFromPlaylist);
        Self::with_actions(scope, items, actions)
    }

    /// Build a unified-playlist menu with an explicit action set while
    /// retaining the standard AddToPlaylist/AddToQueue recommendation.
    pub fn with_actions(
        scope: UnifiedPlaylistSelectionScope,
        items: Vec<UnifiedPlaylistActionItem>,
        actions: impl IntoIterator<Item = command::Action>,
    ) -> Self {
        Self {
            scope,
            items,
            actions: crate::command::BulkActionCandidates::new(actions)
                .as_slice()
                .iter()
                .map(|action| action.descriptor())
                .collect(),
        }
    }

    pub const fn scope(&self) -> &UnifiedPlaylistSelectionScope {
        &self.scope
    }

    pub fn items(&self) -> &[UnifiedPlaylistActionItem] {
        &self.items
    }

    pub fn actions(&self) -> &[command::ActionDescriptor] {
        &self.actions
    }
}

impl MultiSelectActionMenu for UnifiedPlaylistActionMenu {
    type Item = UnifiedPlaylistActionItem;

    fn items(&self) -> &[Self::Item] {
        self.items()
    }

    fn actions(&self) -> &[command::ActionDescriptor] {
        self.actions()
    }
}

#[derive(Debug, Clone)]
pub enum ActionListItem {
    Track(Track, Vec<command::Action>),
    YouTubeTrack(YouTubeTrack, Vec<command::Action>),
    YouTubeTracks(YouTubeTracksActionMenu),
    Tracks(TracksActionMenu),
    Queue(QueueActionMenu),
    UnifiedPlaylist(UnifiedPlaylistActionMenu),
    SessionHistory(SessionHistoryActionMenu),
    Lyrics(LyricsActionMenu),
    UnifiedPlaylistContext(UnifiedPlaylistContextActionMenu),
    YouTubePlaylistContext(YouTubePlaylistContextActionMenu),
    Artist(Artist, Vec<command::Action>),
    Album(Album, Vec<command::Action>),
    Playlist(Playlist, Vec<command::Action>),
    Show(Show, Vec<command::Action>),
    Episode(Episode, Vec<command::Action>),
}

/// An action on an item in a playlist popup list
#[derive(Debug)]
pub enum PlaylistPopupAction {
    Browse {
        folder_id: usize,
        search_query: String,
    },
    AddTrack {
        folder_id: usize,
        track: Track,
        search_query: String,
    },
    AddTracks {
        folder_id: usize,
        tracks: TracksActionMenu,
        search_query: String,
    },
    AddEpisode {
        folder_id: usize,
        episode_id: EpisodeId<'static>,
        search_query: String,
    },
}

#[derive(Debug, Clone)]
pub enum YouTubePlaylistPopupAction {
    AddTrack {
        track: YouTubeTrack,
        search_query: String,
    },
    AddTracks {
        tracks: YouTubeTracksActionMenu,
        search_query: String,
    },
    LinkUnified {
        unified_playlist_id: String,
        search_query: String,
    },
}

/// An action on an item in an artist popup list
#[derive(Copy, Clone, Debug)]
pub enum ArtistPopupAction {
    Browse,
    ShowActions,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WorkspaceScopeSelection {
    Provider(crate::config::ActiveProvider),
    Account {
        provider: crate::config::ActiveProvider,
        account_id: String,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorkspaceScopeOption {
    pub label: String,
    pub selection: WorkspaceScopeSelection,
}

impl PopupState {
    /// gets the (immutable) list state of a (list) popup
    pub fn list_state(&self) -> Option<&ListState> {
        match self {
            #[cfg(feature = "private-capture")]
            Self::PrivateCaptureSelector { state } => Some(&state.list),
            Self::DeviceList(list_state)
            | Self::UserPlaylistList(.., list_state)
            | Self::YouTubePlaylistList(.., list_state)
            | Self::YouTubeArtistMenu {
                state: list_state, ..
            }
            | Self::UserFollowedArtistList(list_state)
            | Self::UserSavedAlbumList(list_state)
            | Self::ArtistList(.., list_state)
            | Self::ThemeList(.., list_state)
            | Self::ActionList(.., list_state)
            | Self::AnchoredActionList {
                state: list_state, ..
            }
            | Self::DiagnosticActions {
                state: list_state, ..
            }
            | Self::ListenBrainzSyncDetails {
                state: list_state, ..
            }
            | Self::ListenBrainzWorkspace {
                state: list_state, ..
            }
            | Self::ListenBrainzResolve {
                state: list_state, ..
            }
            | Self::JournalListSelect(.., list_state)
            | Self::ConfigChoice {
                state: list_state, ..
            }
            | Self::ConfigMultiChoice {
                state: list_state, ..
            }
            | Self::UnifiedPlaylistDestination {
                state: list_state, ..
            }
            | Self::SpotifyUserCandidates {
                state: list_state, ..
            }
            | Self::SpotifyUserPlaylists {
                state: list_state, ..
            }
            | Self::TrackRating {
                state: list_state, ..
            } => Some(list_state),
            Self::WorkspaceScope {
                state: list_state, ..
            } => Some(list_state),
            Self::ListenBrainzPlaylists {
                state: list_state, ..
            } => Some(list_state),
            Self::Search { .. }
            | Self::Volume { .. }
            | Self::DeferredAction { .. }
            | Self::SpotifyUserSearch { .. }
            | Self::SpotifyUserProfile { .. }
            | Self::PlaylistCreate { .. }
            | Self::SessionHistoryCreate { .. }
            | Self::TrackNote { .. }
            | Self::JournalListName { .. }
            | Self::PlaylistName { .. }
            | Self::ListenBrainzToken { .. }
            | Self::ConfigEdit { .. }
            | Self::ConfirmAction { .. }
            | Self::DiagnosticDetail { .. }
            | Self::CommandHelp { .. } => None,
            #[cfg(feature = "private-capture")]
            Self::PrivateCapturePassphrase { .. }
            | Self::PrivateCaptureConfirm { .. }
            | Self::PrivateDerivativePreview { .. } => None,
        }
    }

    /// gets the (mutable) list state of a (list) popup
    pub fn list_state_mut(&mut self) -> Option<&mut ListState> {
        match self {
            #[cfg(feature = "private-capture")]
            Self::PrivateCaptureSelector { state } => Some(&mut state.list),
            Self::DeviceList(list_state)
            | Self::UserPlaylistList(.., list_state)
            | Self::YouTubePlaylistList(.., list_state)
            | Self::YouTubeArtistMenu {
                state: list_state, ..
            }
            | Self::UserFollowedArtistList(list_state)
            | Self::UserSavedAlbumList(list_state)
            | Self::ArtistList(.., list_state)
            | Self::ThemeList(.., list_state)
            | Self::ActionList(.., list_state)
            | Self::AnchoredActionList {
                state: list_state, ..
            }
            | Self::DiagnosticActions {
                state: list_state, ..
            }
            | Self::ListenBrainzSyncDetails {
                state: list_state, ..
            }
            | Self::ListenBrainzWorkspace {
                state: list_state, ..
            }
            | Self::ListenBrainzResolve {
                state: list_state, ..
            }
            | Self::JournalListSelect(.., list_state)
            | Self::ConfigChoice {
                state: list_state, ..
            }
            | Self::ConfigMultiChoice {
                state: list_state, ..
            }
            | Self::UnifiedPlaylistDestination {
                state: list_state, ..
            }
            | Self::SpotifyUserCandidates {
                state: list_state, ..
            }
            | Self::SpotifyUserPlaylists {
                state: list_state, ..
            }
            | Self::TrackRating {
                state: list_state, ..
            } => Some(list_state),
            Self::WorkspaceScope {
                state: list_state, ..
            } => Some(list_state),
            Self::ListenBrainzPlaylists {
                state: list_state, ..
            } => Some(list_state),
            Self::Search { .. }
            | Self::Volume { .. }
            | Self::DeferredAction { .. }
            | Self::SpotifyUserSearch { .. }
            | Self::SpotifyUserProfile { .. }
            | Self::PlaylistCreate { .. }
            | Self::SessionHistoryCreate { .. }
            | Self::TrackNote { .. }
            | Self::JournalListName { .. }
            | Self::PlaylistName { .. }
            | Self::ListenBrainzToken { .. }
            | Self::ConfigEdit { .. }
            | Self::ConfirmAction { .. }
            | Self::DiagnosticDetail { .. }
            | Self::CommandHelp { .. } => None,
            #[cfg(feature = "private-capture")]
            Self::PrivateCapturePassphrase { .. }
            | Self::PrivateCaptureConfirm { .. }
            | Self::PrivateDerivativePreview { .. } => None,
        }
    }

    /// gets the selected position of a (list) popup
    pub fn list_selected(&self) -> Option<usize> {
        match self.list_state() {
            None => None,
            Some(state) => state.selected(),
        }
    }

    /// selects a position in a (list) popup
    pub fn list_select(&mut self, id: Option<usize>) {
        match self.list_state_mut() {
            None => {}
            Some(state) => state.select(id),
        }
    }
}

impl ActionListItem {
    pub fn n_actions(&self) -> usize {
        match self {
            ActionListItem::Track(.., actions)
            | ActionListItem::YouTubeTrack(.., actions)
            | ActionListItem::Artist(.., actions)
            | ActionListItem::Album(.., actions)
            | ActionListItem::Playlist(.., actions)
            | ActionListItem::Show(.., actions)
            | ActionListItem::Episode(.., actions) => actions.len(),
            ActionListItem::YouTubeTracks(menu) => menu.actions().len(),
            ActionListItem::Tracks(menu) => menu.actions().len(),
            ActionListItem::Queue(menu) => menu.actions().len(),
            ActionListItem::UnifiedPlaylist(menu) => menu.actions().len(),
            ActionListItem::SessionHistory(menu) => menu.actions().len(),
            ActionListItem::Lyrics(menu) => menu.actions().len(),
            ActionListItem::UnifiedPlaylistContext(menu) => menu.actions().len(),
            ActionListItem::YouTubePlaylistContext(menu) => menu.actions().len(),
        }
    }

    pub fn name(&self) -> String {
        match self {
            ActionListItem::Track(track, ..) => track.name.clone(),
            ActionListItem::YouTubeTrack(track, ..) => track.name.clone(),
            ActionListItem::YouTubeTracks(menu) => {
                format!("{} YouTube tracks", menu.items().len())
            }
            ActionListItem::Tracks(menu) => format!("{} tracks", menu.items().len()),
            ActionListItem::Queue(menu) => format!("{} queue items", menu.items().len()),
            ActionListItem::UnifiedPlaylist(menu) => {
                format!("{} playlist items", menu.items().len())
            }
            ActionListItem::SessionHistory(menu) => format!(
                "{} history entr{}",
                menu.items().len(),
                if menu.items().len() == 1 { "y" } else { "ies" }
            ),
            ActionListItem::Lyrics(_) => "current lyrics".to_owned(),
            ActionListItem::UnifiedPlaylistContext(menu) => menu.name().to_owned(),
            ActionListItem::YouTubePlaylistContext(menu) => menu.name().to_owned(),
            ActionListItem::Artist(artist, ..) => artist.name.clone(),
            ActionListItem::Album(album, ..) => album.name.clone(),
            ActionListItem::Playlist(playlist, ..) => playlist.name.clone(),
            ActionListItem::Show(show, ..) => show.name.clone(),
            ActionListItem::Episode(episode, ..) => episode.name.clone(),
        }
    }

    pub fn action_descriptors(&self) -> Vec<command::ActionDescriptor> {
        match self {
            ActionListItem::Track(.., actions)
            | ActionListItem::YouTubeTrack(.., actions)
            | ActionListItem::Artist(.., actions)
            | ActionListItem::Album(.., actions)
            | ActionListItem::Playlist(.., actions)
            | ActionListItem::Show(.., actions)
            | ActionListItem::Episode(.., actions) => actions
                .iter()
                .map(|action| action.descriptor())
                .collect::<Vec<_>>(),
            ActionListItem::YouTubeTracks(menu) => menu.actions().to_vec(),
            ActionListItem::Tracks(menu) => menu.actions().to_vec(),
            ActionListItem::Queue(menu) => menu.actions().to_vec(),
            ActionListItem::UnifiedPlaylist(menu) => menu.actions().to_vec(),
            ActionListItem::SessionHistory(menu) => menu.actions().to_vec(),
            ActionListItem::Lyrics(menu) => menu.actions().to_vec(),
            ActionListItem::UnifiedPlaylistContext(menu) => menu.actions().to_vec(),
            ActionListItem::YouTubePlaylistContext(menu) => menu.actions().to_vec(),
        }
    }
}

impl PopupState {
    /// Return the typed action payload at a visible popup index. Ordinary and
    /// diagnostic executors use this same bounded lookup, so shortcut and
    /// selection paths cannot resolve different rows.
    pub(crate) fn action_payload(&self, index: usize) -> Option<PopupActionPayload> {
        match self {
            Self::ActionList(item, ..) | Self::AnchoredActionList { item, .. } => item
                .action_descriptors()
                .get(index)
                .copied()
                .map(PopupActionPayload::Ordinary),
            Self::DiagnosticActions { actions, .. } => actions
                .get(index)
                .copied()
                .map(PopupActionPayload::Diagnostic),
            _ => None,
        }
    }
}

#[cfg(test)]
mod popup_action_payload_tests {
    use super::{
        ActionListItem, BulkActionSelectionEpoch, PopupActionPayload, PopupState,
        SessionHistoryActionMenu, TracksActionMenu, UnifiedPlaylistContextActionMenu,
        YouTubePlaylistContextActionMenu,
    };
    use crate::{
        command::Action,
        observability::{DiagnosticAction, DiagnosticRowId},
        state::Provider,
    };
    use ratatui::widgets::ListState;

    #[test]
    fn ordinary_and_diagnostic_popups_share_bounded_typed_payload_lookup() {
        let ordinary = PopupState::ActionList(
            Box::new(ActionListItem::Tracks(TracksActionMenu::new(
                Vec::new(),
                vec![Action::AddToQueue.descriptor()],
                BulkActionSelectionEpoch::new(Provider::Spotify, 1),
            ))),
            ListState::default(),
        );
        assert_eq!(
            ordinary.action_payload(0),
            Some(PopupActionPayload::Ordinary(
                Action::AddToQueue.descriptor()
            ))
        );
        assert_eq!(ordinary.action_payload(1), None);

        let diagnostic = PopupState::DiagnosticActions {
            target: DiagnosticRowId::WorkersEmpty,
            actions: vec![DiagnosticAction::ExplainState],
            state: ListState::default(),
        };
        assert_eq!(
            diagnostic.action_payload(0),
            Some(PopupActionPayload::Diagnostic(
                DiagnosticAction::ExplainState
            ))
        );
        assert_eq!(diagnostic.action_payload(1), None);
    }

    #[test]
    fn session_history_menu_exposes_explicit_local_actions() {
        let menu = SessionHistoryActionMenu::new(Vec::new());
        assert_eq!(menu.actions().len(), 3);
        assert_eq!(menu.actions()[0].action, Action::CopyLink);
        assert_eq!(menu.actions()[1].action, Action::AddToPlaylist);
        assert_eq!(menu.actions()[2].action, Action::ClearSessionHistory);
        assert!(ActionListItem::SessionHistory(menu).n_actions() == 3);
    }

    #[test]
    fn playlist_context_menus_expose_lifecycle_actions() {
        let unified =
            UnifiedPlaylistContextActionMenu::new("local-1".to_owned(), "Local".to_owned(), false);
        assert!(unified
            .actions()
            .iter()
            .any(|entry| entry.action == Action::RenamePlaylist));
        assert!(unified
            .actions()
            .iter()
            .any(|entry| entry.action == Action::DeletePlaylist));

        let youtube =
            YouTubePlaylistContextActionMenu::new("PL1".to_owned(), "Remote".to_owned(), true);
        assert_eq!(youtube.actions().len(), 3);
        assert!(ActionListItem::YouTubePlaylistContext(youtube).n_actions() == 3);
    }
}

#[cfg(test)]
mod listenbrainz_resolve_menu_tests {
    use super::{ListenBrainzResolveMenu, ResolutionPolicy, ResolutionSide};
    use crate::state::ListenBrainzSyncConflictKind;

    fn menu() -> ListenBrainzResolveMenu {
        ListenBrainzResolveMenu::new(
            "local".to_owned(),
            "Mix".to_owned(),
            vec![
                ListenBrainzSyncConflictKind::Reorder,
                ListenBrainzSyncConflictKind::UnlinkedRemoteRow,
            ],
        )
    }

    #[test]
    fn resolve_menu_starts_undecided_and_needs_policy_plus_every_decision() {
        let menu = menu();
        assert_eq!(menu.row_count(), 3 + 2 + 1);
        assert!(!menu.is_ready());
        assert!(!menu.is_ready());
        assert!(menu
            .row_label(5)
            .is_some_and(|label| label.contains("choose a policy")));
        assert!(menu
            .row_label(3)
            .is_some_and(|label| label.contains("undecided")));
        assert!(menu.row_label(6).is_none());
    }

    #[test]
    fn resolve_menu_cycles_decisions_without_preselecting_destruction() {
        let mut menu = menu();
        assert!(menu.select_policy(2));
        assert!(!menu.select_policy(3));
        assert!(menu.cycle_decision(3));
        assert!(!menu.is_ready());
        assert!(menu.cycle_decision(4));
        assert!(menu.is_ready());
        assert_eq!(menu.policy(), Some(ResolutionPolicy::MergeNonConflicting));
        assert!(menu
            .row_label(5)
            .is_some_and(|label| label.contains("Apply resolution (Merge non-conflicting")));
        // Cycling continues local -> listenbrainz -> local.
        assert!(menu.cycle_decision(3));
        assert!(menu
            .row_label(3)
            .is_some_and(|label| label.contains("keep listenbrainz")));
    }

    #[test]
    fn resolve_menu_flags_unsafe_remote_rows_before_any_write() {
        let mut menu = menu();
        menu.select_policy(2);
        menu.cycle_decision(3);
        menu.cycle_decision(4);
        menu.cycle_decision(4);
        // Unlinked row resolving to ListenBrainz needs CLI mapping.
        assert!(menu.has_unsafe_remote_choice());
        menu.cycle_decision(4);
        assert!(!menu.has_unsafe_remote_choice());
        let decisions = menu.decisions();
        assert_eq!(decisions.len(), 2);
        assert!(decisions
            .iter()
            .all(|decision| decision.side == ResolutionSide::Local));
    }

    #[test]
    fn resolve_menu_rejects_keep_listenbrainz_over_unsafe_rows() {
        let mut menu = menu();
        menu.select_policy(1);
        menu.cycle_decision(3);
        menu.cycle_decision(4);
        assert!(menu.is_ready());
        assert!(menu.has_unsafe_remote_choice());
    }
}

#[cfg(all(test, feature = "private-capture"))]
mod private_capture_tests {
    use super::{PopupState, PrivateCapturePassphraseAction, PrivateCaptureSelectorState};
    use crate::developer_capture::{
        CapturePassphraseInput, CaptureRef, SafeArtifactLabel, SafeOperatorArtifact,
    };

    fn artifact(byte: u8, label: SafeArtifactLabel) -> SafeOperatorArtifact {
        SafeOperatorArtifact {
            capture_ref: CaptureRef::from_bytes([byte; 16]).safe(),
            label,
        }
    }

    #[test]
    fn selector_keeps_the_same_safe_reference_across_refresh_and_reorder() {
        let first = artifact(1, SafeArtifactLabel::Working);
        let second = artifact(2, SafeArtifactLabel::Failing);
        let mut selector = PrivateCaptureSelectorState::new(&[first, second], None);
        selector.select_index(&[first, second], 1);
        assert_eq!(selector.selected, Some(second.capture_ref));

        selector.synchronize(&[second, first]);
        assert_eq!(selector.list.selected(), Some(0));
        assert_eq!(selector.selected, Some(second.capture_ref));

        selector.synchronize(&[first]);
        assert_eq!(selector.list.selected(), Some(0));
        assert_eq!(selector.selected, Some(first.capture_ref));

        selector.synchronize(&[]);
        assert_eq!(selector.list.selected(), None);
        assert_eq!(selector.selected, None);
    }

    #[test]
    fn popup_debug_never_contains_masked_input_contents() {
        let mut input = CapturePassphraseInput::new();
        for character in "never-render-this-private-value".chars() {
            assert!(input.push(character));
        }
        let popup = PopupState::PrivateCapturePassphrase {
            action: PrivateCapturePassphraseAction::ReviewCapture,
            input,
        };
        let debug = format!("{popup:?}");
        assert!(debug.contains("[private]"));
        assert!(!debug.contains("never-render"));
        assert!(!debug.contains("private-value"));
    }
}
