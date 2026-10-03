use std::collections::HashSet;

use ratatui::widgets::TableState;
use rspotify::prelude::Id;

use crate::{
    command::Action,
    state::{
        MediaId, Playlist, PlaylistEntryId, PlaylistSeedItem, Provider, ScopedSelection,
        ScopedSelectionError, Track, UiViewStatus, UnifiedPlaylist, YouTubeContext,
        YouTubeContextId, YouTubeTrack,
    },
};

use super::OccurrenceDescriptor;

const UNIFIED_METADATA_PARTIAL_CODE: &str = "UNIFIED_METADATA_PARTIAL";
const UNIFIED_METADATA_PARTIAL_MESSAGE: &str = "Some imported items are missing provider metadata.";
const UNIFIED_METADATA_PARTIAL_NEXT_ACTION: &str =
    "Retry the import when the provider is available.";

/// Stable identity for one concrete mutable-playlist page.
#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub struct PlaylistRef {
    provider: Option<Provider>,
    id: String,
    account_epoch: u64,
}

impl PlaylistRef {
    pub fn new(provider: Option<Provider>, id: impl Into<String>, account_epoch: u64) -> Self {
        Self {
            provider,
            id: id.into(),
            account_epoch,
        }
    }
}

/// UI-facing occurrence identity. Provider-native mutation tokens remain P5.
#[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub enum ProviderOccurrenceToken {
    Unified(PlaylistEntryId),
    Spotify {
        position: usize,
        snapshot_id: String,
    },
    YouTubeMusic {
        position: usize,
        revision: String,
        set_video_id: Option<String>,
    },
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum Capability {
    Supported,
    Unsupported { reason: &'static str },
}

impl Capability {
    const fn is_supported(&self) -> bool {
        matches!(self, Self::Supported)
    }
}

// `can_*` reads as a capability question at every use site.
#[allow(clippy::struct_field_names)]
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PlaylistCapabilities {
    pub can_append: Capability,
    pub can_remove_occurrences: Capability,
    pub can_reorder: Capability,
    pub can_rename: Capability,
    pub can_delete: Capability,
}

impl PlaylistCapabilities {
    pub fn read_only(reason: &'static str) -> Self {
        let unsupported = || Capability::Unsupported { reason };
        Self {
            can_append: unsupported(),
            can_remove_occurrences: unsupported(),
            can_reorder: unsupported(),
            can_rename: unsupported(),
            can_delete: unsupported(),
        }
    }

    pub fn spotify(is_modifiable: bool) -> Self {
        if !is_modifiable {
            return Self::read_only(
                "Spotify only permits mutation of owned or collaborative playlists.",
            );
        }
        Self {
            can_append: Capability::Supported,
            can_remove_occurrences: Capability::Supported,
            can_reorder: Capability::Supported,
            can_rename: Capability::Supported,
            can_delete: Capability::Unsupported {
                reason: "Spotify playlist deletion is not supported; library removal is distinct.",
            },
        }
    }

    pub fn youtube_music(is_editable: bool) -> Self {
        if !is_editable {
            return Self::read_only(
                "YouTube Music only permits mutation of playlists in this account's library.",
            );
        }
        Self {
            can_append: Capability::Supported,
            can_remove_occurrences: Capability::Supported,
            can_reorder: Capability::Unsupported {
                reason: "YouTube Music reorder has no verified provider contract.",
            },
            can_rename: Capability::Supported,
            can_delete: Capability::Supported,
        }
    }
}

/// One action model shared by every mutable-playlist surface.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PlaylistActionModel {
    context_actions: Vec<Action>,
}

impl Default for PlaylistActionModel {
    fn default() -> Self {
        Self::new(std::iter::empty())
    }
}

const COMMON_ITEM_ACTIONS: &[Action] =
    &[Action::CopyLink, Action::AddToPlaylist, Action::AddToQueue];
const COMMON_CONTEXT_ACTIONS: &[Action] = &[Action::CopyLink];

impl PlaylistActionModel {
    pub fn new(provider_context_actions: impl IntoIterator<Item = Action>) -> Self {
        Self {
            context_actions: unique_actions(
                COMMON_CONTEXT_ACTIONS
                    .iter()
                    .copied()
                    .chain(provider_context_actions),
            ),
        }
    }

    /// Keep common commands in one stable order, then append provider-specific
    /// capabilities without duplication.
    pub fn item_actions(&self, supported: &[Action]) -> Vec<Action> {
        let common = COMMON_ITEM_ACTIONS
            .iter()
            .copied()
            .filter(|action| supported.contains(action));
        let provider = supported
            .iter()
            .copied()
            .filter(|action| !COMMON_ITEM_ACTIONS.contains(action));
        unique_actions(common.chain(provider))
    }

    pub fn context_actions(&self) -> &[Action] {
        &self.context_actions
    }

    pub fn constrained_by(mut self, capabilities: &PlaylistCapabilities) -> Self {
        self.context_actions.retain(|action| match action {
            Action::RenamePlaylist => capabilities.can_rename.is_supported(),
            Action::DeletePlaylist => capabilities.can_delete.is_supported(),
            _ => true,
        });
        self
    }
}

pub(crate) fn unified_playlist_action_model_with_listenbrainz(
    youtube_linked: bool,
    include_listenbrainz: bool,
) -> PlaylistActionModel {
    let mut actions = Vec::new();
    if include_listenbrainz {
        actions.push(Action::OpenUnifiedPlaylistListenBrainzSync);
    }
    if youtube_linked {
        actions.extend([
            Action::SyncUnifiedPlaylistToYouTube,
            Action::UnlinkUnifiedPlaylistFromYouTube,
        ]);
    } else {
        actions.push(Action::LinkUnifiedPlaylistToYouTube);
    }
    actions.extend([Action::RenamePlaylist, Action::DeletePlaylist]);
    PlaylistActionModel::new(actions)
}

fn unique_actions(actions: impl IntoIterator<Item = Action>) -> Vec<Action> {
    let mut result = Vec::new();
    for action in actions {
        if !result.contains(&action) {
            result.push(action);
        }
    }
    result
}

pub fn youtube_playlist_ids_match(left: &str, right: &str) -> bool {
    !left.is_empty()
        && !right.is_empty()
        && (left == right
            || left.strip_prefix("VL") == Some(right)
            || right.strip_prefix("VL") == Some(left))
}

pub fn spotify_playlist_is_modifiable(
    playlist: &Playlist,
    current_user_id: Option<&rspotify::model::UserId<'_>>,
) -> bool {
    current_user_id.is_some_and(|user_id| &playlist.owner.1 == user_id) || playlist.collaborative
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PlaylistEntrySnapshot {
    pub occurrence: ProviderOccurrenceToken,
    pub item: PlaylistSeedItem,
    pub source_index: usize,
    pub item_actions: Vec<Action>,
}

impl PlaylistEntrySnapshot {
    pub fn matches_filter(&self, query: &str) -> bool {
        playlist_seed_matches_filter(&self.item, query)
    }
}

pub fn playlist_seed_matches_filter(item: &PlaylistSeedItem, query: &str) -> bool {
    if query.is_empty() {
        return true;
    }
    let query = query.to_lowercase();
    [
        item.title.as_str(),
        item.artists.as_str(),
        item.album.as_deref().unwrap_or_default(),
        item.media_id.raw_id.as_str(),
    ]
    .iter()
    .any(|value| value.to_lowercase().contains(&query))
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PlaylistSnapshot {
    pub playlist: PlaylistRef,
    pub title: String,
    pub revision: String,
    pub capabilities: PlaylistCapabilities,
    pub actions: PlaylistActionModel,
    pub entries: Vec<PlaylistEntrySnapshot>,
    pub status: UiViewStatus,
}

impl PlaylistSnapshot {
    pub fn visible_indices(&self, filter_query: Option<&str>) -> Vec<usize> {
        self.entries
            .iter()
            .enumerate()
            .filter_map(|(index, entry)| {
                filter_query
                    .is_none_or(|query| entry.matches_filter(query))
                    .then_some(index)
            })
            .collect()
    }

    /// Return the stable intersection of row actions for a bulk occurrence
    /// selection. This keeps menu parity independent of provider page type.
    pub fn actions_for_sources(&self, source_indices: &[usize]) -> Vec<Action> {
        let Some(first) = source_indices
            .first()
            .and_then(|index| self.entries.get(*index))
        else {
            return Vec::new();
        };
        first
            .item_actions
            .iter()
            .copied()
            .filter(|action| {
                let native_exact_remove_is_single = *action == Action::DeleteFromPlaylist
                    && source_indices.len() > 1
                    && matches!(
                        first.occurrence,
                        ProviderOccurrenceToken::Spotify { .. }
                            | ProviderOccurrenceToken::YouTubeMusic { .. }
                    );
                !native_exact_remove_is_single
                    && source_indices.iter().skip(1).all(|index| {
                        self.entries
                            .get(*index)
                            .is_some_and(|entry| entry.item_actions.contains(action))
                    })
            })
            .collect()
    }

    pub fn from_unified(
        playlist: &UnifiedPlaylist,
        actions: PlaylistActionModel,
        context_actions: impl IntoIterator<Item = Action>,
    ) -> Self {
        let item_actions = actions.item_actions(&[
            Action::CopyLink,
            Action::AddToPlaylist,
            Action::AddToQueue,
            Action::DeleteFromPlaylist,
        ]);
        Self {
            playlist: PlaylistRef::new(None, playlist.id.clone(), 0),
            title: playlist.name.clone(),
            revision: playlist.snapshot_hash(),
            capabilities: PlaylistCapabilities {
                can_append: Capability::Supported,
                can_remove_occurrences: Capability::Supported,
                can_reorder: Capability::Supported,
                can_rename: Capability::Supported,
                can_delete: Capability::Supported,
            },
            actions: PlaylistActionModel::new(context_actions),
            entries: playlist
                .items
                .iter()
                .enumerate()
                .map(|(source_index, item)| PlaylistEntrySnapshot {
                    occurrence: ProviderOccurrenceToken::Unified(item.entry_id),
                    item: PlaylistSeedItem::from_unified_playlist_item(item),
                    source_index,
                    item_actions: if item.playable_media().is_some() {
                        item_actions.clone()
                    } else {
                        item_actions
                            .iter()
                            .copied()
                            .filter(|action| *action != Action::AddToQueue)
                            .collect()
                    },
                })
                .collect(),
            status: if playlist.items.is_empty() {
                UiViewStatus::Empty
            } else if playlist
                .items
                .iter()
                .any(|item| item.metadata.degraded || item.metadata.metadata_pending)
            {
                UiViewStatus::Partial {
                    code: UNIFIED_METADATA_PARTIAL_CODE,
                    message: UNIFIED_METADATA_PARTIAL_MESSAGE,
                    next_action: UNIFIED_METADATA_PARTIAL_NEXT_ACTION,
                }
            } else {
                UiViewStatus::Ready
            },
        }
    }

    pub fn from_youtube_playlist<F>(
        context_id: &YouTubeContextId,
        context: &YouTubeContext,
        account_epoch: u64,
        status: UiViewStatus,
        mut capabilities: PlaylistCapabilities,
        actions: PlaylistActionModel,
        mut item_actions: F,
    ) -> Option<Self>
    where
        F: FnMut(&YouTubeTrack) -> Vec<Action>,
    {
        let YouTubeContextId::Playlist(playlist_id) = context_id else {
            return None;
        };
        if !context.playlist_set_video_ids.iter().any(|token| {
            token
                .as_deref()
                .is_some_and(|token| !token.trim().is_empty())
        }) {
            capabilities.can_remove_occurrences = Capability::Unsupported {
                reason: "Exact removal is unavailable because this read omitted SetVideoID.",
            };
        }
        let revision = UnifiedPlaylist::youtube_tracks_snapshot_hash(&context.tracks);
        let actions = actions.constrained_by(&capabilities);
        let can_remove_occurrences = capabilities.can_remove_occurrences.is_supported();
        Some(Self {
            playlist: PlaylistRef::new(
                Some(Provider::YouTubeMusic),
                playlist_id.clone(),
                account_epoch,
            ),
            title: context.title.clone(),
            revision: revision.clone(),
            capabilities,
            actions: actions.clone(),
            entries: context
                .tracks
                .iter()
                .enumerate()
                .map(|(source_index, track)| {
                    let set_video_id = context
                        .playlist_set_video_ids
                        .get(source_index)
                        .cloned()
                        .flatten()
                        .filter(|token| !token.trim().is_empty());
                    let mut supported_actions = item_actions(track);
                    if set_video_id.is_some() && can_remove_occurrences {
                        supported_actions.push(Action::DeleteFromPlaylist);
                    }
                    PlaylistEntrySnapshot {
                        occurrence: ProviderOccurrenceToken::YouTubeMusic {
                            position: source_index,
                            revision: revision.clone(),
                            set_video_id,
                        },
                        item: PlaylistSeedItem::from_youtube_track(track),
                        source_index,
                        item_actions: actions.item_actions(&supported_actions),
                    }
                })
                .collect(),
            status: if context.tracks.is_empty() && status == UiViewStatus::Ready {
                UiViewStatus::Empty
            } else {
                status
            },
        })
    }

    pub fn from_spotify_playlist<F>(
        playlist: &Playlist,
        tracks: &[Track],
        account_epoch: u64,
        mut capabilities: PlaylistCapabilities,
        actions: PlaylistActionModel,
        mut item_actions: F,
    ) -> Self
    where
        F: FnMut(&Track) -> Vec<Action>,
    {
        if playlist.snapshot_id.is_empty() {
            capabilities.can_remove_occurrences = Capability::Unsupported {
                reason: "Exact removal requires a current Spotify snapshot revision.",
            };
            capabilities.can_reorder = Capability::Unsupported {
                reason: "Reorder requires a current Spotify snapshot revision.",
            };
        }
        let actions = actions.constrained_by(&capabilities);
        let can_remove_occurrences = capabilities.can_remove_occurrences.is_supported();
        Self {
            playlist: PlaylistRef::new(Some(Provider::Spotify), playlist.id.uri(), account_epoch),
            title: playlist.name.clone(),
            revision: playlist.snapshot_id.clone(),
            capabilities,
            actions: actions.clone(),
            entries: tracks
                .iter()
                .enumerate()
                .map(|(source_index, track)| {
                    let supported_actions = item_actions(track)
                        .into_iter()
                        .filter(|action| {
                            *action != Action::DeleteFromPlaylist || can_remove_occurrences
                        })
                        .collect::<Vec<_>>();
                    PlaylistEntrySnapshot {
                        occurrence: ProviderOccurrenceToken::Spotify {
                            position: source_index,
                            snapshot_id: playlist.snapshot_id.clone(),
                        },
                        item: PlaylistSeedItem::from_spotify_track(track),
                        source_index,
                        item_actions: actions.item_actions(&supported_actions),
                    }
                })
                .collect(),
            status: if tracks.is_empty() {
                UiViewStatus::Empty
            } else {
                UiViewStatus::Ready
            },
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum MutablePlaylistView {
    All,
}

pub type MutablePlaylistSelection =
    ScopedSelection<PlaylistRef, MutablePlaylistView, MediaId, ProviderOccurrenceToken>;

#[derive(Clone, Debug, Default, PartialEq)]
pub struct MutablePlaylistState {
    table: TableState,
    selection: MutablePlaylistSelection,
    cursor_occurrence: Option<ProviderOccurrenceToken>,
}

impl MutablePlaylistState {
    pub fn table(&self) -> &TableState {
        &self.table
    }

    pub fn table_mut(&mut self) -> &mut TableState {
        &mut self.table
    }

    pub fn selection(&self) -> &MutablePlaylistSelection {
        &self.selection
    }

    pub fn selection_mut(&mut self) -> &mut MutablePlaylistSelection {
        &mut self.selection
    }

    pub fn cursor_occurrence(&self) -> Option<&ProviderOccurrenceToken> {
        self.cursor_occurrence.as_ref()
    }

    pub fn set_cursor_occurrence(&mut self, occurrence: Option<ProviderOccurrenceToken>) {
        self.cursor_occurrence = occurrence;
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct MutablePlaylistProjection {
    visible_indices: Vec<usize>,
    selected_visible_indices: Vec<usize>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PlaylistMenuSelection {
    pub source_indices: Vec<usize>,
    pub actions: Vec<Action>,
}

impl MutablePlaylistProjection {
    pub fn visible_indices(&self) -> &[usize] {
        &self.visible_indices
    }

    pub fn selected_visible_indices(&self) -> &[usize] {
        &self.selected_visible_indices
    }

    pub fn visible_len(&self) -> usize {
        self.visible_indices.len()
    }

    pub fn visible_source_index(&self, visible_index: usize) -> Option<usize> {
        self.visible_indices.get(visible_index).copied()
    }
}

pub struct MutablePlaylistController;

impl MutablePlaylistController {
    pub fn synchronize(
        state: &mut MutablePlaylistState,
        snapshot: &PlaylistSnapshot,
        filter_query: Option<&str>,
    ) -> Result<MutablePlaylistProjection, ScopedSelectionError> {
        debug_assert!(Self::projection_is_unique(snapshot));
        let visible_indices = snapshot.visible_indices(filter_query);
        let complete = snapshot.entries.iter().map(|entry| {
            OccurrenceDescriptor::with_token(entry.item.media_id.clone(), entry.occurrence.clone())
        });
        let visible = visible_indices.iter().map(|index| {
            let entry = &snapshot.entries[*index];
            OccurrenceDescriptor::with_token(entry.item.media_id.clone(), entry.occurrence.clone())
        });
        state.selection.synchronize_retaining_hidden(
            snapshot.playlist.clone(),
            complete,
            MutablePlaylistView::All,
            visible,
        )?;

        if visible_indices.is_empty() {
            state.table.select(None);
            state.cursor_occurrence = None;
        } else {
            let fallback = state
                .table
                .selected()
                .unwrap_or_default()
                .min(visible_indices.len() - 1);
            let resolved_cursor = state.cursor_occurrence.as_ref().and_then(|occurrence| {
                visible_indices
                    .iter()
                    .position(|index| snapshot.entries[*index].occurrence == *occurrence)
            });
            let cursor = resolved_cursor.unwrap_or(fallback);
            state.table.select(Some(cursor));
            let cursor_still_exists = state.cursor_occurrence.as_ref().is_some_and(|occurrence| {
                snapshot
                    .entries
                    .iter()
                    .any(|entry| entry.occurrence == *occurrence)
            });
            if resolved_cursor.is_some() || !cursor_still_exists {
                state.cursor_occurrence = visible_indices
                    .get(cursor)
                    .map(|index| snapshot.entries[*index].occurrence.clone());
            }
        }

        let selected_visible_indices = state.selection.selected_visible_indices();
        Ok(MutablePlaylistProjection {
            visible_indices,
            selected_visible_indices,
        })
    }

    pub fn projection_is_unique(snapshot: &PlaylistSnapshot) -> bool {
        let mut occurrences = HashSet::with_capacity(snapshot.entries.len());
        snapshot
            .entries
            .iter()
            .all(|entry| occurrences.insert(entry.occurrence.clone()))
    }

    pub fn menu_selection(
        state: &MutablePlaylistState,
        snapshot: &PlaylistSnapshot,
        projection: &MutablePlaylistProjection,
    ) -> Option<PlaylistMenuSelection> {
        let cursor = state.table.selected().unwrap_or_default();
        let visible_indices = state.selection.selected_visible_indices();
        let visible_indices = if visible_indices.is_empty() {
            state
                .selection
                .selected_or_cursor_visible_indices(cursor)
                .ok()?
        } else {
            visible_indices
        };
        let source_indices = visible_indices
            .into_iter()
            .filter_map(|index| projection.visible_source_index(index))
            .collect::<Vec<_>>();
        if source_indices.is_empty() {
            return None;
        }
        Some(PlaylistMenuSelection {
            actions: snapshot.actions_for_sources(&source_indices),
            source_indices,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::state::{MediaKind, Provider, UnifiedPlaylist, UnifiedPlaylistItem};
    use rspotify::model::{PlaylistId, UserId};

    fn entry(position: usize, raw_id: &str, title: &str) -> PlaylistEntrySnapshot {
        PlaylistEntrySnapshot {
            occurrence: ProviderOccurrenceToken::Spotify {
                position,
                snapshot_id: "revision".to_owned(),
            },
            item: PlaylistSeedItem {
                media_id: MediaId {
                    provider: Provider::Spotify,
                    kind: MediaKind::Track,
                    raw_id: raw_id.to_owned(),
                },
                title: title.to_owned(),
                artists: "artist".to_owned(),
                album: None,
                duration_ms: None,
                explicit: None,
                provider_url: None,
                artwork_url: None,
                metadata_degraded: false,
                metadata_pending: false,
            },
            source_index: position,
            item_actions: Vec::new(),
        }
    }

    #[test]
    fn unified_listenbrainz_action_is_projected_without_changing_common_action_order() {
        let disabled = unified_playlist_action_model_with_listenbrainz(false, false);
        assert!(!disabled
            .context_actions()
            .contains(&Action::OpenUnifiedPlaylistListenBrainzSync));

        let enabled = unified_playlist_action_model_with_listenbrainz(false, true);
        assert_eq!(
            enabled.context_actions(),
            [
                Action::CopyLink,
                Action::OpenUnifiedPlaylistListenBrainzSync,
                Action::LinkUnifiedPlaylistToYouTube,
                Action::RenamePlaylist,
                Action::DeletePlaylist,
            ]
        );
        // The workspace owns every per-operation action, so the context menu
        // never exposes individual sync operations or rollback directly.
        for action in [
            Action::BackupUnifiedPlaylistToListenBrainz,
            Action::InitializeUnifiedPlaylistListenBrainzBase,
            Action::CheckUnifiedPlaylistListenBrainzSync,
            Action::PreviewUnifiedPlaylistListenBrainzPush,
            Action::PreviewUnifiedPlaylistListenBrainzPull,
            Action::ReviewUnifiedPlaylistListenBrainzConflicts,
            Action::RecoverUnifiedPlaylistListenBrainzSync,
            Action::RollbackUnifiedPlaylistListenBrainzPull,
            Action::ApplyUnifiedPlaylistListenBrainzPush,
            Action::ApplyUnifiedPlaylistListenBrainzPull,
            Action::ApplyUnifiedPlaylistListenBrainzResolve,
        ] {
            assert!(!enabled.context_actions().contains(&action));
        }

        let already_linked = unified_playlist_action_model_with_listenbrainz(true, true);
        assert!(already_linked
            .context_actions()
            .contains(&Action::OpenUnifiedPlaylistListenBrainzSync));
        assert!(already_linked
            .context_actions()
            .contains(&Action::SyncUnifiedPlaylistToYouTube));
    }

    #[test]
    fn unified_snapshot_surfaces_degraded_rows_as_partial_and_preserves_status() {
        let playlist = UnifiedPlaylist {
            id: "unresolved".to_owned(),
            name: "Imported".to_owned(),
            items: vec![UnifiedPlaylistItem {
                title: "Unknown track".to_owned(),
                metadata: crate::state::UnifiedPlaylistMetadata {
                    degraded: true,
                    metadata_pending: false,
                    ..Default::default()
                },
                ..Default::default()
            }],
            ..Default::default()
        };
        let snapshot = PlaylistSnapshot::from_unified(
            &playlist,
            PlaylistActionModel::default(),
            std::iter::empty(),
        );
        assert!(matches!(
            snapshot.status,
            UiViewStatus::Partial {
                code: UNIFIED_METADATA_PARTIAL_CODE,
                ..
            }
        ));
        assert!(snapshot.entries[0].item.metadata_degraded);
    }

    fn snapshot(entries: Vec<PlaylistEntrySnapshot>) -> PlaylistSnapshot {
        PlaylistSnapshot {
            playlist: PlaylistRef::new(Some(Provider::Spotify), "playlist", 1),
            title: "Playlist".to_owned(),
            revision: "revision".to_owned(),
            capabilities: PlaylistCapabilities::read_only("test"),
            actions: PlaylistActionModel::default(),
            entries,
            status: UiViewStatus::Ready,
        }
    }

    #[test]
    fn controller_retains_occurrence_selection_through_filter_refresh_and_resize() {
        let first = entry(0, "same", "first");
        let second = entry(1, "same", "second");
        let mut state = MutablePlaylistState::default();
        let original = snapshot(vec![first.clone(), second.clone()]);
        MutablePlaylistController::synchronize(&mut state, &original, None).unwrap();
        state.selection.extend_range(1, 1).unwrap();
        state.table.select(Some(1));
        state.set_cursor_occurrence(Some(second.occurrence.clone()));

        let filtered =
            MutablePlaylistController::synchronize(&mut state, &original, Some("first")).unwrap();
        assert_eq!(state.selection.selected_len(), 1);
        assert!(filtered.selected_visible_indices().is_empty());

        let reordered = snapshot(vec![second, first]);
        let visible = MutablePlaylistController::synchronize(&mut state, &reordered, None).unwrap();
        assert_eq!(visible.selected_visible_indices(), &[0]);
        assert_eq!(state.table.selected(), Some(0));
        *state.table.offset_mut() = usize::MAX;
        assert_eq!(state.selection.selected_len(), 1);
    }

    #[test]
    fn common_action_model_orders_shared_actions_once_and_keeps_provider_extras() {
        let model = PlaylistActionModel::new([Action::RenamePlaylist]);
        assert_eq!(
            model.item_actions(&[
                Action::AddToQueue,
                Action::GoToArtist,
                Action::CopyLink,
                Action::AddToQueue,
            ]),
            [Action::CopyLink, Action::AddToQueue, Action::GoToArtist,]
        );
        assert_eq!(
            model.context_actions(),
            &[Action::CopyLink, Action::RenamePlaylist]
        );
    }

    #[test]
    fn three_playlist_contexts_receive_common_actions_from_one_owner() {
        let unified = PlaylistActionModel::new([Action::DeletePlaylist]);
        let youtube = PlaylistActionModel::new([Action::RenamePlaylist]);
        let spotify = PlaylistActionModel::new([Action::GoToRadio]);
        for model in [unified, youtube, spotify] {
            assert_eq!(model.context_actions().first(), Some(&Action::CopyLink));
            assert_eq!(
                model.item_actions(&[Action::CopyLink, Action::AddToQueue]),
                [Action::CopyLink, Action::AddToQueue]
            );
        }
    }

    #[test]
    fn provider_capabilities_reflect_native_occurrence_adapters() {
        let spotify = PlaylistCapabilities::spotify(true);
        assert_eq!(spotify.can_reorder, Capability::Supported);
        assert_eq!(spotify.can_remove_occurrences, Capability::Supported);
        assert!(matches!(spotify.can_delete, Capability::Unsupported { .. }));

        let youtube = PlaylistCapabilities::youtube_music(true);
        assert_eq!(youtube.can_delete, Capability::Supported);
        assert!(matches!(
            youtube.can_reorder,
            Capability::Unsupported { .. }
        ));
        assert_eq!(youtube.can_remove_occurrences, Capability::Supported);
    }

    #[test]
    fn spotify_read_only_page_and_library_list_menus_hide_rename() {
        let read_only_actions = PlaylistActionModel::new([
            Action::RenamePlaylist,
            Action::DeletePlaylist,
            Action::GoToRadio,
        ])
        .constrained_by(&PlaylistCapabilities::spotify(false));
        assert_eq!(
            read_only_actions.context_actions(),
            [Action::CopyLink, Action::GoToRadio]
        );
    }

    #[test]
    fn spotify_rename_ownership_gate_is_shared_by_menu_and_action_dispatch() {
        let owner = UserId::from_id("owner").unwrap();
        let mut playlist = Playlist {
            id: PlaylistId::from_id("37i9dQZF1DXcBWIGoYBM5M").unwrap(),
            collaborative: false,
            name: "Playlist".to_owned(),
            owner: ("Owner".to_owned(), owner.clone()),
            desc: String::new(),
            current_folder_id: 0,
            snapshot_id: "revision".to_owned(),
        };
        assert!(!spotify_playlist_is_modifiable(&playlist, None));
        assert!(spotify_playlist_is_modifiable(&playlist, Some(&owner)));
        playlist.collaborative = true;
        assert!(spotify_playlist_is_modifiable(&playlist, None));
    }

    #[test]
    fn spotify_snapshot_without_revision_hides_exact_mutation_capabilities() {
        let owner = UserId::from_id("owner").unwrap();
        let playlist = Playlist {
            id: PlaylistId::from_id("37i9dQZF1DXcBWIGoYBM5M").unwrap(),
            collaborative: false,
            name: "Playlist".to_owned(),
            owner: ("Owner".to_owned(), owner),
            desc: String::new(),
            current_folder_id: 0,
            snapshot_id: String::new(),
        };
        let snapshot = PlaylistSnapshot::from_spotify_playlist(
            &playlist,
            &[],
            1,
            PlaylistCapabilities::spotify(true),
            PlaylistActionModel::default(),
            |_| vec![Action::DeleteFromPlaylist],
        );
        assert!(matches!(
            snapshot.capabilities.can_remove_occurrences,
            Capability::Unsupported { .. }
        ));
        assert!(matches!(
            snapshot.capabilities.can_reorder,
            Capability::Unsupported { .. }
        ));
    }

    #[test]
    fn youtube_library_identity_normalization_is_shared_and_fail_closed() {
        assert!(youtube_playlist_ids_match("VLPL123", "PL123"));
        assert!(youtube_playlist_ids_match("PL123", "VLPL123"));
        assert!(youtube_playlist_ids_match("PL123", "PL123"));
        assert!(!youtube_playlist_ids_match("VLPL123", "PL999"));
        assert!(!youtube_playlist_ids_match("VL", ""));
    }

    #[test]
    fn youtube_snapshot_retains_set_video_id_and_gates_exact_remove_per_row() {
        let track = YouTubeTrack {
            id: "video".to_owned(),
            name: "Track".to_owned(),
            artists: "Artist".to_owned(),
            album: None,
            duration: "1:00".to_owned(),
            explicit: false,
            thumbnail_url: None,
            is_video: false,
        };
        let context_id = YouTubeContextId::Playlist("playlist".to_owned());
        let snapshot = PlaylistSnapshot::from_youtube_playlist(
            &context_id,
            &YouTubeContext {
                title: "Playlist".to_owned(),
                description: None,
                tracks: vec![track.clone(), track],
                playlist_set_video_ids: vec![Some("set-video".to_owned()), Some("   ".to_owned())],
                artist: None,
            },
            1,
            UiViewStatus::Ready,
            PlaylistCapabilities::youtube_music(true),
            PlaylistActionModel::default(),
            |_| vec![Action::CopyLink],
        )
        .unwrap();
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
        assert!(!snapshot.entries[1]
            .item_actions
            .contains(&Action::DeleteFromPlaylist));

        let tokenless = PlaylistSnapshot::from_youtube_playlist(
            &context_id,
            &YouTubeContext {
                title: "Playlist".to_owned(),
                description: None,
                tracks: snapshot
                    .entries
                    .iter()
                    .map(|entry| YouTubeTrack {
                        id: entry.item.media_id.raw_id.clone(),
                        name: entry.item.title.clone(),
                        artists: entry.item.artists.clone(),
                        album: entry.item.album.clone(),
                        duration: String::new(),
                        explicit: false,
                        thumbnail_url: None,
                        is_video: false,
                    })
                    .collect(),
                playlist_set_video_ids: Vec::new(),
                artist: None,
            },
            1,
            UiViewStatus::Ready,
            PlaylistCapabilities::youtube_music(true),
            PlaylistActionModel::default(),
            |_| vec![Action::CopyLink],
        )
        .unwrap();
        assert!(matches!(
            tokenless.capabilities.can_remove_occurrences,
            Capability::Unsupported { .. }
        ));
    }

    #[test]
    fn controller_menu_selection_maps_filtered_cursor_to_source_and_actions() {
        let mut first = entry(0, "first", "hidden");
        first.item_actions = vec![Action::CopyLink];
        let mut second = entry(1, "second", "visible");
        second.item_actions = vec![Action::CopyLink, Action::AddToQueue];
        let snapshot = snapshot(vec![first, second]);
        let mut state = MutablePlaylistState::default();
        let projection =
            MutablePlaylistController::synchronize(&mut state, &snapshot, Some("visible")).unwrap();
        let menu = MutablePlaylistController::menu_selection(&state, &snapshot, &projection)
            .expect("filtered cursor menu");
        assert_eq!(menu.source_indices, [1]);
        assert_eq!(menu.actions, [Action::CopyLink, Action::AddToQueue]);
    }

    #[test]
    fn controller_projects_identical_real_menu_actions_for_three_occurrence_domains() {
        let item = entry(0, "same", "row").item;
        let occurrences = [
            ProviderOccurrenceToken::Unified(PlaylistEntryId(1)),
            ProviderOccurrenceToken::Spotify {
                position: 0,
                snapshot_id: "revision".to_owned(),
            },
            ProviderOccurrenceToken::YouTubeMusic {
                position: 0,
                revision: "revision".to_owned(),
                set_video_id: None,
            },
        ];
        for occurrence in occurrences {
            let snapshot = snapshot(vec![PlaylistEntrySnapshot {
                occurrence,
                item: item.clone(),
                source_index: 0,
                item_actions: PlaylistActionModel::default()
                    .item_actions(&[Action::CopyLink, Action::AddToQueue]),
            }]);
            let mut state = MutablePlaylistState::default();
            let projection =
                MutablePlaylistController::synchronize(&mut state, &snapshot, None).unwrap();
            let menu = MutablePlaylistController::menu_selection(&state, &snapshot, &projection)
                .expect("menu selection");
            assert_eq!(menu.actions, [Action::CopyLink, Action::AddToQueue]);
        }
    }

    #[test]
    fn duplicate_media_remains_unique_in_all_provider_occurrence_domains() {
        let item = entry(0, "same", "duplicate").item;
        let domains = [
            vec![
                PlaylistEntrySnapshot {
                    occurrence: ProviderOccurrenceToken::Unified(PlaylistEntryId(1)),
                    item: item.clone(),
                    source_index: 0,
                    item_actions: Vec::new(),
                },
                PlaylistEntrySnapshot {
                    occurrence: ProviderOccurrenceToken::Unified(PlaylistEntryId(2)),
                    item: item.clone(),
                    source_index: 1,
                    item_actions: Vec::new(),
                },
            ],
            vec![entry(0, "same", "duplicate"), entry(1, "same", "duplicate")],
            vec![
                PlaylistEntrySnapshot {
                    occurrence: ProviderOccurrenceToken::YouTubeMusic {
                        position: 0,
                        revision: "r".to_owned(),
                        set_video_id: None,
                    },
                    item: item.clone(),
                    source_index: 0,
                    item_actions: Vec::new(),
                },
                PlaylistEntrySnapshot {
                    occurrence: ProviderOccurrenceToken::YouTubeMusic {
                        position: 1,
                        revision: "r".to_owned(),
                        set_video_id: None,
                    },
                    item: item.clone(),
                    source_index: 1,
                    item_actions: Vec::new(),
                },
            ],
        ];
        for entries in domains {
            assert!(MutablePlaylistController::projection_is_unique(&snapshot(
                entries
            )));
        }
    }

    #[test]
    fn menu_intersection_is_occurrence_aware_and_stably_ordered() {
        let mut first = entry(0, "same", "first");
        first.item_actions = vec![Action::CopyLink, Action::AddToQueue, Action::GoToArtist];
        let mut second = entry(1, "same", "second");
        second.item_actions = vec![Action::CopyLink, Action::AddToQueue];
        assert_eq!(
            snapshot(vec![first, second]).actions_for_sources(&[0, 1]),
            [Action::CopyLink, Action::AddToQueue]
        );
    }
}
