//! Spotify artist page with a focused section and compact previews of the others.

use crate::state::{
    Album, Artist, ArtistFocusState, Context, ContextId, ContextPageUIState, ContextTrackPane,
    ListenBrainzArtistEnrichment, ListenBrainzCollectionStatus, PageState, SharedState, Track,
    UIStateGuard, WorkspaceFocusState, WorkspaceHit,
};
use crate::ui::components::collection::CollectionTrackRow;
use crate::ui::utils;
use ratatui::{
    layout::{Constraint, Layout, Rect},
    style::Style,
    text::{Line, Span},
    widgets::{Block, Paragraph},
    Frame,
};
use rspotify::prelude::Id;

use super::{
    collection_full_profile, listenbrainz_album_fallback_rows, listenbrainz_album_fallback_table,
    listenbrainz_fallback_rows, listenbrainz_fallback_table, render_workspace_collection_table,
    synchronize_context_render_selection, workspace_collection_visible_rows,
    workspace_context_heading, workspace_context_scope, workspace_text, CollectionDetailColumn,
};

enum SectionBody {
    Tracks {
        rows: Vec<CollectionTrackRow>,
        selected: Vec<usize>,
        playing: Option<usize>,
    },
    Albums(Vec<AlbumRow>),
    Artists(Vec<String>),
    /// `ListenBrainz` stand-ins keep their shared table widget and own no hits.
    ListenBrainzTracks(super::ListenBrainzFallbackTable, bool),
    ListenBrainzAlbums(super::ListenBrainzAlbumFallbackTable),
}

struct AlbumRow {
    year: String,
    name: String,
    kind: String,
}

struct Section {
    focus: ArtistFocusState,
    title: &'static str,
    len: usize,
    body: SectionBody,
}

/// Render a loaded Spotify artist. Returns `false` when the cached context is
/// not an artist, so the caller can fall back.
pub(super) fn render_workspace_spotify_artist(
    frame: &mut Frame,
    state: &SharedState,
    ui: &mut UIStateGuard,
    rect: Rect,
    context_id: &ContextId,
) -> bool {
    let Some((artist_name, sections)) = artist_sections(state, ui, context_id) else {
        return false;
    };
    let full_profile = collection_full_profile(frame, rect);
    workspace_context_heading(
        frame,
        &ui.theme,
        rect,
        &artist_name,
        &workspace_context_scope(ui),
        full_profile,
    );
    // The full profile keeps the heading's spacer rows (DESIGN-SPEC §9.3).
    let (body_top, gap) = if full_profile { (4, 1) } else { (1, 0) };
    let body = Rect::new(
        rect.x,
        rect.y.saturating_add(body_top),
        rect.width,
        rect.height
            .saturating_sub(body_top + u16::from(full_profile)),
    );
    let focus = match ui.current_page() {
        PageState::Context {
            state: Some(ContextPageUIState::Artist { focus, .. }),
            ..
        } => *focus,
        _ => return false,
    };
    let context_focused = ui.workspace_focus == WorkspaceFocusState::Context;
    let card_height = if body.height >= 8 { 3 } else { 1 };
    let panel_height = body.height.saturating_sub(card_height + gap);
    let panel = Rect::new(body.x, body.y, body.width, panel_height);
    let cards = Rect::new(
        body.x,
        panel.bottom().saturating_add(gap),
        body.width,
        card_height.min(body.height),
    );
    let mut collapsed = Vec::new();
    for section in sections {
        if section.focus == focus {
            render_section(
                frame,
                ui,
                panel,
                section,
                focus,
                context_focused,
                full_profile,
            );
        } else {
            collapsed.push(section);
        }
    }
    let card_rects = Layout::horizontal(vec![Constraint::Fill(1); collapsed.len()]).split(cards);
    for (section, rect) in collapsed.into_iter().zip(card_rects.iter()) {
        let card = Rect::new(rect.x, rect.y, rect.width.saturating_sub(1), rect.height);
        render_section_card(frame, ui, card, &section);
    }
    true
}

/// Snapshot the artist context into display rows while the data lock is held.
fn artist_sections(
    state: &SharedState,
    ui: &mut UIStateGuard,
    context_id: &ContextId,
) -> Option<(String, Vec<Section>)> {
    let data = state.data.read();
    let Some(Context::Artist {
        artist,
        top_tracks,
        listenbrainz,
        albums,
        related_artists,
    }) = data.caches.context.get(&context_id.uri())
    else {
        return None;
    };
    let artist_uri = artist.id.uri();
    let liked_tracks = data.user_data.liked_tracks_by_artist(artist);
    let playing_uri =
        state
            .player
            .read()
            .playback
            .as_ref()
            .and_then(|playback| match playback.item.as_ref() {
                Some(rspotify::model::PlayableItem::Track(track)) => {
                    track.id.as_ref().map(rspotify::prelude::Id::uri)
                }
                _ => None,
            });

    let top_tracks_body = if top_tracks.is_empty() {
        listenbrainz_fallback_rows(listenbrainz, &data.caches.listenbrainz_recordings).map(
            |fallback| {
                let selectable = matches!(
                    listenbrainz,
                    ListenBrainzArtistEnrichment::Available {
                        recordings_status: ListenBrainzCollectionStatus::Available,
                        ..
                    }
                );
                SectionBody::ListenBrainzTracks(fallback, selectable)
            },
        )
    } else {
        None
    };
    let (top_title, top_body) = match top_tracks_body {
        Some(SectionBody::ListenBrainzTracks(fallback, selectable)) => (
            fallback.title,
            SectionBody::ListenBrainzTracks(fallback, selectable),
        ),
        _ => (
            "Top tracks",
            track_body(
                ui,
                ContextTrackPane::ArtistTopTracks,
                &artist_uri,
                top_tracks,
                playing_uri.as_deref(),
            ),
        ),
    };
    let liked_body = track_body(
        ui,
        ContextTrackPane::ArtistLikedSongs,
        &artist_uri,
        &liked_tracks,
        playing_uri.as_deref(),
    );
    let (albums_title, albums_body) = match albums
        .is_empty()
        .then(|| listenbrainz_album_fallback_rows(listenbrainz, &data.caches.listenbrainz_albums))
        .flatten()
    {
        Some(fallback) => (fallback.title, SectionBody::ListenBrainzAlbums(fallback)),
        None => ("Albums", album_body(ui, albums)),
    };
    let artists_body = artist_body(ui, related_artists);

    let sections = [
        (ArtistFocusState::TopTracks, top_title, top_body),
        (ArtistFocusState::LikedSongs, "Liked songs", liked_body),
        (ArtistFocusState::Albums, albums_title, albums_body),
        (
            ArtistFocusState::RelatedArtists,
            "Related artists",
            artists_body,
        ),
    ]
    .into_iter()
    .map(|(focus, title, body)| Section {
        focus,
        title,
        len: body_len(&body),
        body,
    })
    .collect();
    Some((artist.name.clone(), sections))
}

fn track_body(
    ui: &mut UIStateGuard,
    pane: ContextTrackPane,
    artist_uri: &str,
    tracks: &[Track],
    playing_uri: Option<&str>,
) -> SectionBody {
    // Key handlers index the filtered rows, so rendering must use the same projection.
    let visible = ui.search_filtered_items_projection(tracks);
    let visible = visible.iter().collect::<Vec<_>>();
    let selected =
        synchronize_context_render_selection(ui, pane, artist_uri, tracks, visible.iter().copied())
            .unwrap_or_default();
    let playing =
        playing_uri.and_then(|uri| visible.iter().position(|track| track.id.uri() == uri));
    let rows = visible
        .iter()
        .map(|track| {
            let mut row = CollectionTrackRow::from_spotify(track);
            // The page is the artist; the album is the useful second column.
            row.artist = track
                .album
                .as_ref()
                .map(|album| album.name.clone())
                .unwrap_or_default();
            row
        })
        .collect();
    SectionBody::Tracks {
        rows,
        selected,
        playing,
    }
}

fn album_body(ui: &UIStateGuard, albums: &[Album]) -> SectionBody {
    SectionBody::Albums(
        ui.search_filtered_items_projection(albums)
            .iter()
            .map(|album| AlbumRow {
                year: album.release_date.chars().take(4).collect(),
                name: album.name.clone(),
                kind: album.album_type(),
            })
            .collect(),
    )
}

fn artist_body(ui: &UIStateGuard, artists: &[Artist]) -> SectionBody {
    SectionBody::Artists(
        ui.search_filtered_items_projection(artists)
            .iter()
            .map(|artist| artist.name.clone())
            .collect(),
    )
}

fn body_len(body: &SectionBody) -> usize {
    match body {
        SectionBody::Tracks { rows, .. } => rows.len(),
        SectionBody::Albums(rows) => rows.len(),
        SectionBody::Artists(rows) => rows.len(),
        SectionBody::ListenBrainzTracks(fallback, _) => match &fallback.body {
            super::ListenBrainzFallbackBody::Message(_) => 1,
            super::ListenBrainzFallbackBody::Recordings(rows) => rows.len(),
        },
        SectionBody::ListenBrainzAlbums(fallback) => match &fallback.body {
            super::ListenBrainzAlbumFallbackBody::Message(_) => 1,
            super::ListenBrainzAlbumFallbackBody::ReleaseGroups(rows) => rows.len(),
        },
    }
}

fn render_section_card(frame: &mut Frame, ui: &mut UIStateGuard, rect: Rect, section: &Section) {
    frame.render_widget(Block::default().style(ui.theme.workspace_panel()), rect);
    let count = section.len.to_string();
    let count_width = (count.len() as u16).min(rect.width.saturating_sub(2));
    let title_width = rect.width.saturating_sub(count_width + 3);
    workspace_text(
        frame,
        Rect::new(rect.x.saturating_add(1), rect.y, title_width, 1),
        utils::bounded_text(section.title, usize::from(title_width)),
        ui.theme.workspace_heading(),
    );
    workspace_text(
        frame,
        Rect::new(
            rect.right().saturating_sub(count_width + 1),
            rect.y,
            count_width,
            1,
        ),
        count,
        ui.theme.workspace_secondary_text(),
    );
    if rect.height >= 2 {
        let preview = match &section.body {
            SectionBody::Tracks { rows, .. } => rows.first().map(|row| row.title.as_str()),
            SectionBody::Albums(rows) => rows.first().map(|row| row.name.as_str()),
            SectionBody::Artists(rows) => rows.first().map(String::as_str),
            SectionBody::ListenBrainzTracks(..) | SectionBody::ListenBrainzAlbums(..) => {
                Some("Open recommendations")
            }
        }
        .unwrap_or("No items");
        workspace_text(
            frame,
            Rect::new(
                rect.x.saturating_add(1),
                rect.y + 1,
                rect.width.saturating_sub(2),
                1,
            ),
            utils::bounded_text(preview, usize::from(rect.width.saturating_sub(2))),
            ui.theme.workspace_secondary_text(),
        );
    }
    let index = section_selected(ui, section.focus).unwrap_or(0);
    ui.workspace_hits.push((
        rect,
        WorkspaceHit::ArtistRow {
            focus: section.focus,
            index,
        },
    ));
}

fn render_section(
    frame: &mut Frame,
    ui: &mut UIStateGuard,
    rect: Rect,
    section: Section,
    focus: ArtistFocusState,
    context_focused: bool,
    full_profile: bool,
) {
    let is_focused = section.focus == focus;
    let theme = ui.theme.clone();
    let title_style = if is_focused {
        theme.workspace_heading()
    } else {
        theme.workspace_secondary_text()
    };
    let title_rect = Rect::new(
        rect.x.saturating_add(2),
        rect.y,
        rect.width.saturating_sub(4),
        1,
    );
    frame.render_widget(
        Paragraph::new(Line::from(vec![
            Span::styled(section.title, title_style),
            Span::styled(
                format!("  {}", section.len),
                theme.workspace_secondary_text(),
            ),
        ])),
        title_rect,
    );
    // Clicking a title moves focus there without changing that section's cursor.
    let title_index = section_selected(ui, section.focus).unwrap_or(0);
    ui.workspace_hits.push((
        title_rect,
        WorkspaceHit::ArtistRow {
            focus: section.focus,
            index: title_index,
        },
    ));
    if rect.height < 2 {
        return;
    }
    if section.len == 0 {
        super::render_view_status(
            frame,
            &theme,
            crate::state::UiViewStatus::Empty,
            Rect::new(rect.x, rect.y + 1, rect.width, rect.height - 1),
        );
        return;
    }
    let header_y = rect.y + 1;
    let stride = if full_profile { 2 } else { 1 };
    let rows_rect = Rect::new(
        rect.x,
        header_y + stride,
        rect.width,
        rect.height.saturating_sub(1 + stride),
    );
    let active = is_focused && context_focused;

    if let SectionBody::Tracks {
        rows,
        selected,
        playing,
    } = &section.body
    {
        // The shared full table starts five rows below the collection heading;
        // here its header follows the section title by one row.
        let table_rect = if full_profile {
            Rect::new(
                rect.x,
                rect.y.saturating_sub(4),
                rect.width,
                rect.height.saturating_add(4),
            )
        } else {
            rect
        };
        let visible = workspace_collection_visible_rows(table_rect, full_profile);
        let focused_row = section_selected(ui, section.focus);
        let overflow = ui.presentation.focused_row_overflow;
        let phase = ui.focused_marquee_phase();
        let mut hits = Vec::new();
        if let Some(table) = section_table_state(ui, section.focus) {
            utils::adjust_table_offset(table, rows.len(), visible);
            render_workspace_collection_table(
                frame,
                &theme,
                table_rect,
                rows,
                0,
                rows.len(),
                focused_row,
                active,
                selected,
                *playing,
                table,
                &format!("{} tracks shown", rows.len()),
                theme.workspace_secondary_text(),
                CollectionDetailColumn {
                    label: "Album",
                    ..CollectionDetailColumn::ARTIST
                },
                full_profile,
                overflow,
                phase,
                &mut hits,
            );
        }
        ui.workspace_hits
            .extend(hits.into_iter().map(|(rect, hit)| {
                (
                    rect,
                    match hit {
                        WorkspaceHit::ContextRow(index) => WorkspaceHit::ArtistRow {
                            focus: section.focus,
                            index,
                        },
                        other => other,
                    },
                )
            }));
        return;
    }

    match section.body {
        SectionBody::ListenBrainzTracks(fallback, selectable) => {
            let (table, row_count) =
                listenbrainz_fallback_table(fallback, &theme, selectable && active);
            let table_rect = Rect::new(
                rect.x.saturating_add(2),
                header_y,
                rect.width.saturating_sub(3),
                rect.height - 1,
            );
            if let Some(state) = section_table_state(ui, section.focus) {
                utils::render_table_window(frame, table, table_rect, row_count, state);
            }
        }
        SectionBody::ListenBrainzAlbums(fallback) => {
            let compact = ui.layout_policy().mode.is_compact();
            let (table, row_count) =
                listenbrainz_album_fallback_table(fallback, &theme, compact, active);
            let table_rect = Rect::new(
                rect.x.saturating_add(2),
                header_y,
                rect.width.saturating_sub(3),
                rect.height - 1,
            );
            if let Some(state) = section_table_state(ui, section.focus) {
                utils::render_table_window(frame, table, table_rect, row_count, state);
            }
        }
        body => {
            let selected = section_selected(ui, section.focus);
            let visible = rows_rect.height.div_ceil(stride);
            let offset = section_offset(ui, section.focus, section.len, visible, is_focused);
            if active {
                utils::render_vertical_rule(
                    frame,
                    Rect::new(rect.x.saturating_add(1), header_y, 1, rect.height - 1),
                    "│",
                    theme.workspace_focus_indicator(),
                );
            }
            let row_style = |index: usize| {
                let is_cursor = is_focused && selected == Some(index);
                match (is_cursor, context_focused) {
                    (true, true) => Some(theme.workspace_selection_active()),
                    (true, false) => Some(theme.workspace_selection_inactive()),
                    (false, _) => None,
                }
            };
            let mut hits = Vec::new();
            let mut row_rect = |index: usize| {
                let y = rows_rect.y + (index - offset) as u16 * stride;
                let row = Rect::new(rect.x.saturating_add(2), y, rect.width.saturating_sub(3), 1);
                hits.push((
                    row,
                    WorkspaceHit::ArtistRow {
                        focus: section.focus,
                        index,
                    },
                ));
                row
            };
            let visible_range = offset..(offset + usize::from(visible)).min(section.len);
            match body {
                SectionBody::Albums(rows) => {
                    let name_x = rect.x.saturating_add(11);
                    let kind_width = if rect.width >= 50 { 8 } else { 0 };
                    let name_width = rect
                        .right()
                        .saturating_sub(4 + kind_width)
                        .saturating_sub(name_x);
                    let kind_x = name_x + name_width + 2;
                    let header = theme.workspace_table_header();
                    workspace_text(frame, Rect::new(rect.x + 5, header_y, 4, 1), "Year", header);
                    workspace_text(
                        frame,
                        Rect::new(name_x, header_y, name_width, 1),
                        "Title",
                        header,
                    );
                    if kind_width > 0 {
                        workspace_text(
                            frame,
                            Rect::new(kind_x, header_y, kind_width, 1),
                            "Type",
                            header,
                        );
                    }
                    for index in visible_range {
                        let row_rect = row_rect(index);
                        let style = row_style(index);
                        paint_row(frame, row_rect, style);
                        let text = style.unwrap_or_else(|| theme.workspace_base());
                        let album = &rows[index];
                        workspace_text(
                            frame,
                            Rect::new(rect.x + 5, row_rect.y, 4, 1),
                            album.year.clone(),
                            text,
                        );
                        workspace_text(
                            frame,
                            Rect::new(name_x, row_rect.y, name_width, 1),
                            utils::bounded_text(&album.name, usize::from(name_width)),
                            text,
                        );
                        if kind_width > 0 {
                            workspace_text(
                                frame,
                                Rect::new(kind_x, row_rect.y, kind_width, 1),
                                album.kind.clone(),
                                style.unwrap_or_else(|| theme.workspace_secondary_text()),
                            );
                        }
                    }
                }
                SectionBody::Artists(rows) => {
                    let name_width = rect.width.saturating_sub(9);
                    workspace_text(
                        frame,
                        Rect::new(rect.x + 5, header_y, name_width, 1),
                        "Name",
                        theme.workspace_table_header(),
                    );
                    for index in visible_range {
                        let row_rect = row_rect(index);
                        let style = row_style(index);
                        paint_row(frame, row_rect, style);
                        workspace_text(
                            frame,
                            Rect::new(rect.x + 5, row_rect.y, name_width, 1),
                            utils::bounded_text(&rows[index], usize::from(name_width)),
                            style.unwrap_or_else(|| theme.workspace_base()),
                        );
                    }
                }
                SectionBody::Tracks { .. }
                | SectionBody::ListenBrainzTracks(..)
                | SectionBody::ListenBrainzAlbums(..) => {
                    unreachable!("handled above")
                }
            }
            ui.workspace_hits.extend(hits);
        }
    }
}

fn paint_row(frame: &mut Frame, rect: Rect, style: Option<Style>) {
    if let Some(style) = style {
        frame.render_widget(Block::default().style(style), rect);
    }
}

fn artist_page_state<'a>(ui: &'a mut UIStateGuard<'_>) -> Option<&'a mut ContextPageUIState> {
    match ui.current_page_mut() {
        PageState::Context {
            state: Some(page_state @ ContextPageUIState::Artist { .. }),
            ..
        } => Some(page_state),
        _ => None,
    }
}

fn section_table_state<'a>(
    ui: &'a mut UIStateGuard<'_>,
    focus: ArtistFocusState,
) -> Option<&'a mut ratatui::widgets::TableState> {
    match artist_page_state(ui)? {
        ContextPageUIState::Artist {
            top_track_table,
            liked_track_table,
            album_table,
            ..
        } => match focus {
            ArtistFocusState::TopTracks => Some(top_track_table),
            ArtistFocusState::LikedSongs => Some(liked_track_table),
            ArtistFocusState::Albums => Some(album_table),
            ArtistFocusState::RelatedArtists => None,
        },
        _ => None,
    }
}

fn section_selected(ui: &mut UIStateGuard, focus: ArtistFocusState) -> Option<usize> {
    if focus == ArtistFocusState::RelatedArtists {
        return match artist_page_state(ui)? {
            ContextPageUIState::Artist {
                related_artist_list,
                ..
            } => related_artist_list.selected(),
            _ => None,
        };
    }
    section_table_state(ui, focus)?.selected()
}

/// Keep the cursor of the focused section inside its viewport. Unfocused
/// sections keep their offset so returning to them restores the view.
fn section_offset(
    ui: &mut UIStateGuard,
    focus: ArtistFocusState,
    len: usize,
    visible: u16,
    is_focused: bool,
) -> usize {
    let visible = usize::from(visible).max(1);
    let (selected, offset) = match artist_page_state(ui) {
        Some(ContextPageUIState::Artist {
            related_artist_list,
            ..
        }) if focus == ArtistFocusState::RelatedArtists => (
            related_artist_list.selected(),
            related_artist_list.offset_mut(),
        ),
        _ => match section_table_state(ui, focus) {
            Some(state) => (state.selected(), state.offset_mut()),
            None => return 0,
        },
    };
    if is_focused {
        if let Some(selected) = selected {
            if selected < *offset {
                *offset = selected;
            } else if selected >= *offset + visible {
                *offset = selected + 1 - visible;
            }
        }
    }
    *offset = (*offset).min(len.saturating_sub(visible));
    *offset
}
