use super::scoped_selection::{ScopedSelection, ScopedSelectionError};
use super::selection::OccurrenceDescriptor;
use crate::state::{YouTubeContextId, YouTubeTrack};

/// The only selectable pane owned by a `YouTube` context page.
///
/// Keeping the pane as an explicit value makes the scope observable even
/// though the page currently has only one track table.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub enum YouTubeContextTrackPane {
    Tracks,
}

/// The privacy-safe scope of one `YouTube` context track table.
///
/// The account is represented only by the provider-local epoch. The exact
/// context ID and pane prevent a selection from crossing page boundaries.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct YouTubeContextSelectionScope {
    provider_selection_epoch: u64,
    context_id: YouTubeContextId,
    pane: YouTubeContextTrackPane,
}

impl YouTubeContextSelectionScope {
    pub fn new(
        provider_selection_epoch: u64,
        context_id: YouTubeContextId,
        pane: YouTubeContextTrackPane,
    ) -> Self {
        Self {
            provider_selection_epoch,
            context_id,
            pane,
        }
    }

    pub const fn provider_selection_epoch(&self) -> u64 {
        self.provider_selection_epoch
    }

    pub fn context_id(&self) -> &YouTubeContextId {
        &self.context_id
    }

    pub const fn pane(&self) -> YouTubeContextTrackPane {
        self.pane
    }
}

/// The row projection currently owned by a `YouTube` context.
#[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub enum YouTubeContextSelectionView {
    Unfiltered,
    Filtered(String),
}

/// A typed `YouTube` media kind used as part of a context row identity.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub enum YouTubeMediaKind {
    Song,
    Video,
}

/// The only identity retained by a `YouTube` context selection.
///
/// Fields are private so display metadata, ordinals, and local row IDs cannot
/// accidentally become part of selection identity.
#[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct YouTubeMediaIdentity {
    kind: YouTubeMediaKind,
    raw_id: String,
}

impl YouTubeMediaIdentity {
    pub fn from_track(track: &YouTubeTrack) -> Self {
        Self {
            kind: if track.is_video {
                YouTubeMediaKind::Video
            } else {
                YouTubeMediaKind::Song
            },
            raw_id: track.id.clone(),
        }
    }

    pub const fn kind(&self) -> YouTubeMediaKind {
        self.kind
    }

    pub fn raw_id(&self) -> &str {
        &self.raw_id
    }
}

/// Keyed selection for one `YouTube` context track table.
pub type YouTubeContextSelection = ScopedSelection<
    YouTubeContextSelectionScope,
    YouTubeContextSelectionView,
    YouTubeMediaIdentity,
>;

/// Synchronize an unfiltered context selection.
#[cfg(test)]
pub fn synchronize_youtube_context_tracks(
    selection: &mut YouTubeContextSelection,
    provider_selection_epoch: u64,
    context_id: &YouTubeContextId,
    tracks: &[YouTubeTrack],
) -> Result<(), ScopedSelectionError> {
    synchronize_filtered_youtube_context_tracks(
        selection,
        provider_selection_epoch,
        context_id,
        tracks,
        None,
    )
}

pub fn synchronize_filtered_youtube_context_tracks(
    selection: &mut YouTubeContextSelection,
    provider_selection_epoch: u64,
    context_id: &YouTubeContextId,
    tracks: &[YouTubeTrack],
    query: Option<&str>,
) -> Result<(), ScopedSelectionError> {
    let descriptors = tracks
        .iter()
        .map(|track| OccurrenceDescriptor::unique(YouTubeMediaIdentity::from_track(track)))
        .collect::<Vec<_>>();
    let query = query.filter(|query| !query.trim().is_empty());
    let visible = match query {
        Some(query) => crate::utils::filtered_items_from_query(query, tracks)
            .into_iter()
            .map(|track| OccurrenceDescriptor::unique(YouTubeMediaIdentity::from_track(track)))
            .collect(),
        None => descriptors.clone(),
    };
    selection.synchronize(
        YouTubeContextSelectionScope::new(
            provider_selection_epoch,
            context_id.clone(),
            YouTubeContextTrackPane::Tracks,
        ),
        descriptors,
        query.map_or(YouTubeContextSelectionView::Unfiltered, |query| {
            YouTubeContextSelectionView::Filtered(query.to_owned())
        }),
        visible,
    )
}

/// Resolve keyed rows in visible order, falling back to the cursor for an
/// empty or ambiguous selection.
pub fn youtube_context_selected_or_cursor_indices(
    selection: &YouTubeContextSelection,
    cursor: usize,
) -> Result<Vec<usize>, ScopedSelectionError> {
    let selected = selection.selected_visible_indices();
    if selected.is_empty() {
        selection.selected_or_cursor_visible_indices(cursor)
    } else {
        Ok(selected)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::state::ui::ScopedSelectionStatus;

    fn track(id: &str, is_video: bool) -> YouTubeTrack {
        YouTubeTrack {
            id: id.to_owned(),
            name: String::new(),
            artists: String::new(),
            album: None,
            duration: String::new(),
            explicit: false,
            thumbnail_url: None,
            is_video,
        }
    }

    #[test]
    fn scope_contains_epoch_context_and_exact_pane() {
        let base = YouTubeContextSelectionScope::new(
            1,
            YouTubeContextId::Playlist("one".to_owned()),
            YouTubeContextTrackPane::Tracks,
        );
        assert_ne!(
            base,
            YouTubeContextSelectionScope::new(
                2,
                YouTubeContextId::Playlist("one".to_owned()),
                YouTubeContextTrackPane::Tracks,
            )
        );
        assert_ne!(
            base,
            YouTubeContextSelectionScope::new(
                1,
                YouTubeContextId::Playlist("two".to_owned()),
                YouTubeContextTrackPane::Tracks,
            )
        );
        assert_eq!(base.provider_selection_epoch(), 1);
        assert_eq!(
            base.context_id(),
            &YouTubeContextId::Playlist("one".to_owned())
        );
        assert_eq!(base.pane(), YouTubeContextTrackPane::Tracks);
    }

    #[test]
    fn media_kind_and_raw_id_are_the_identity() {
        let song = track("same", false);
        let video = track("same", true);
        let song_identity = YouTubeMediaIdentity::from_track(&song);
        let video_identity = YouTubeMediaIdentity::from_track(&video);
        assert_eq!(song_identity.kind(), YouTubeMediaKind::Song);
        assert_eq!(video_identity.kind(), YouTubeMediaKind::Video);
        assert_eq!(song_identity.raw_id(), "same");
        assert_ne!(song_identity, video_identity);
    }

    #[test]
    fn duplicate_same_kind_rows_are_ambiguous_but_cursor_fallback_is_bounded() {
        let tracks = vec![track("same", false), track("same", false)];
        let mut selection = YouTubeContextSelection::default();
        synchronize_youtube_context_tracks(
            &mut selection,
            1,
            &YouTubeContextId::LikedTracks,
            &tracks,
        )
        .unwrap();
        assert_eq!(selection.status(), ScopedSelectionStatus::Ambiguous);
        assert_eq!(selection.selected_visible_indices(), Vec::<usize>::new());
        assert_eq!(
            youtube_context_selected_or_cursor_indices(&selection, 1),
            Ok(vec![1])
        );
        assert!(matches!(
            youtube_context_selected_or_cursor_indices(&selection, 2),
            Err(ScopedSelectionError::OutOfBounds { index: 2, len: 2 })
        ));
    }

    #[test]
    fn song_and_video_with_same_id_remain_keyed_distinct() {
        let tracks = vec![track("same", false), track("same", true)];
        let mut selection = YouTubeContextSelection::default();
        synchronize_youtube_context_tracks(
            &mut selection,
            1,
            &YouTubeContextId::Album("album".to_owned()),
            &tracks,
        )
        .unwrap();
        assert_eq!(selection.status(), ScopedSelectionStatus::Ready);
        selection.extend_range(0, 1).unwrap();
        assert_eq!(selection.selected_visible_indices(), vec![0, 1]);

        synchronize_youtube_context_tracks(
            &mut selection,
            1,
            &YouTubeContextId::Album("album".to_owned()),
            &[tracks[1].clone(), tracks[0].clone()],
        )
        .unwrap();
        assert_eq!(selection.selected_visible_indices(), vec![0, 1]);

        synchronize_youtube_context_tracks(
            &mut selection,
            1,
            &YouTubeContextId::Playlist("other".to_owned()),
            &tracks,
        )
        .unwrap();
        assert!(selection.selected_visible_indices().is_empty());
        selection.extend_range(0, 0).unwrap();

        synchronize_youtube_context_tracks(
            &mut selection,
            2,
            &YouTubeContextId::Playlist("other".to_owned()),
            &tracks,
        )
        .unwrap();
        assert!(selection.selected_visible_indices().is_empty());
    }

    #[test]
    fn keyed_selection_remains_actionable_when_cursor_is_out_of_bounds() {
        let tracks = vec![track("a", false), track("b", false)];
        let mut selection = YouTubeContextSelection::default();
        synchronize_youtube_context_tracks(
            &mut selection,
            1,
            &YouTubeContextId::LikedTracks,
            &tracks,
        )
        .unwrap();
        selection.extend_range(0, 0).unwrap();
        assert_eq!(
            youtube_context_selected_or_cursor_indices(&selection, 99),
            Ok(vec![0])
        );
    }
}
