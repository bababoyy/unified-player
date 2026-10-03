//! Home page: quick-access tiles above named, horizontally scrolling shelves.

use ratatui::{layout::Rect, Frame};

use crate::state::{
    home_shelves, HomePageUIState, HomeShelf, HomeShelfKind, PageState, SharedState, ShelfLayout,
    UIStateGuard, WorkspaceFocusState, WorkspaceHit,
};

use crate::ui::utils;

use super::shelf::{grid_card_width, grid_columns, render_card, render_shelf_heading, CARD_GAP};
use super::{
    record_workspace_list_hits_with, render_view_status, render_workspace_page_layout,
    workspace_content_frame, workspace_row_selection_style, workspace_text,
};

const TILE_MIN_WIDTH: u16 = 22;
const MAX_TILE_COLUMNS: u16 = 4;

/// Bodies shorter than this give the "more shelves" hint row to cards.
const HINT_MIN_BODY_HEIGHT: u16 = 6;

/// Content shorter than this drops the spacing inside shelves so short
/// terminals still show cards.
const COMPACT_BELOW_HEIGHT: u16 = 16;

/// Vertical spacing of the shelves; compact when the terminal is short.
#[derive(Clone, Copy)]
struct Spacing {
    heading_gap: u16,
    tile_step: u16,
    card_gap: u16,
}

impl Spacing {
    const fn for_height(height: u16) -> Self {
        if height < COMPACT_BELOW_HEIGHT {
            Self {
                heading_gap: 0,
                tile_step: 2,
                card_gap: 0,
            }
        } else {
            Self {
                heading_gap: 1,
                tile_step: 3,
                card_gap: 1,
            }
        }
    }

    /// Rows above a shelf's body.
    const fn header(self) -> u16 {
        1 + self.heading_gap
    }
}

fn shelf_body_height(shelf: &HomeShelf, columns: usize, spacing: Spacing) -> u16 {
    match shelf.kind {
        HomeShelfKind::QuickAccess => {
            let rows = shelf.cards.len().div_ceil(columns.max(1));
            if rows == 0 {
                0
            } else {
                u16::try_from(rows - 1)
                    .unwrap_or(u16::MAX)
                    .saturating_mul(spacing.tile_step)
                    .saturating_add(2)
            }
        }
        _ if shelf.cards.is_empty() => 1,
        _ => 2,
    }
}

/// Rows a shelf occupies, including the space before the following shelf.
fn shelf_height(shelf: &HomeShelf, columns: usize, spacing: Spacing) -> u16 {
    spacing
        .header()
        .saturating_add(shelf_body_height(shelf, columns, spacing))
        .saturating_add(spacing.card_gap + 1)
}

fn tile_columns(width: u16) -> usize {
    grid_columns(width, TILE_MIN_WIDTH, MAX_TILE_COLUMNS)
}

fn tile_width(width: u16, columns: usize) -> u16 {
    grid_card_width(width, columns)
}

fn card_width(width: u16, per_row: usize) -> u16 {
    grid_card_width(width, per_row)
}

fn visible_cards(width: u16) -> usize {
    tile_columns(width)
}

pub fn render_home_page(
    is_active: bool,
    frame: &mut Frame,
    state: &SharedState,
    ui: &mut UIStateGuard,
    rect: Rect,
) {
    let layout = ui
        .layout_policy()
        .workspace(rect, crate::ui::WorkspaceLayoutKind::Library);
    ui.workspace_layout = layout;
    if layout.show_navigation {
        let playback_provider = state
            .player
            .read()
            .effective_playback_provider(ui.active_provider);
        super::render_workspace_navigation(frame, ui, layout.navigation, playback_provider);
        super::workspace_vertical_rule(
            frame,
            &ui.theme,
            Rect::new(layout.navigation.right(), rect.y, 1, rect.height),
        );
    }
    if layout.content.is_empty() {
        return;
    }
    let body = workspace_content_frame(frame, ui, layout.content, "Home");
    let spacing = Spacing::for_height(body.height);
    let body = Rect::new(
        body.x.saturating_add(1),
        body.y.saturating_add(spacing.heading_gap),
        body.width.saturating_sub(2),
        body.height.saturating_sub(spacing.heading_gap),
    );
    if body.is_empty() {
        return;
    }

    let shelves = home_shelves(&state.data.read(), ui.home_scope());
    let focused = is_active && ui.workspace_focus == WorkspaceFocusState::Context;
    let columns = tile_columns(body.width);
    let per_row = visible_cards(body.width);

    let PageState::Home { state: home } = ui.current_page_mut() else {
        return;
    };
    for shelf in &shelves {
        let layout = if shelf.kind == HomeShelfKind::QuickAccess {
            ShelfLayout::Grid {
                columns,
                step: tile_width(body.width, columns) + CARD_GAP,
            }
        } else {
            ShelfLayout::Row {
                visible: per_row,
                step: card_width(body.width, per_row) + CARD_GAP,
            }
        };
        home.set_layout(shelf.kind, layout);
    }
    home.clamp(&crate::state::home_shelf_sizes(&shelves));
    for shelf in &shelves {
        if shelf.kind != HomeShelfKind::QuickAccess {
            home.scroll_row(shelf.kind, per_row);
        }
    }
    let home = home.clone();
    let total_height: u16 = shelves
        .iter()
        .map(|shelf| shelf_height(shelf, columns, spacing))
        .fold(0, u16::saturating_add)
        // The final shelf needs no separating space after its visible content.
        .saturating_sub(if shelves.is_empty() {
            0
        } else {
            spacing.card_gap + 1
        });
    // The "more shelves" hint is the first thing to go: short bodies spend
    // its row on cards instead.
    let hint_fits = body.height >= HINT_MIN_BODY_HEIGHT;
    let viewport = if total_height > body.height && hint_fits {
        body.height.saturating_sub(1)
    } else {
        body.height
    };
    let scroll = page_scroll(
        &shelves,
        &home,
        columns,
        spacing,
        viewport,
        total_height.saturating_sub(viewport),
    );

    let mut hits = Vec::new();
    let (mut hidden_above, mut hidden_below) = (false, false);
    // The last row is kept for the "more" hint whenever shelves overflow.
    let viewport_bottom = body.y.saturating_add(viewport);
    let mut y = i32::from(body.y) - i32::from(scroll);
    for shelf in &shelves {
        let height = shelf_height(shelf, columns, spacing);
        let top = y;
        y += i32::from(height);
        if top < i32::from(body.y) {
            hidden_above = true;
            continue;
        }
        // A shelf is drawn when its heading and its first whole row of
        // two-line cards fit; later rows are clipped at the viewport rather
        // than hiding the shelf, but a card is never cut in half.
        let Ok(top) = u16::try_from(top) else {
            continue;
        };
        let room = viewport_bottom.saturating_sub(top);
        let content_height = shelf_body_height(shelf, columns, spacing);
        if room < spacing.header() + content_height.min(2) {
            hidden_below = true;
            continue;
        }
        if room < spacing.header().saturating_add(content_height) {
            hidden_below = true;
        }
        let area = Rect::new(body.x, top, body.width, height.min(room));
        let shelf_focused = focused && home.focus == shelf.kind;
        render_home_shelf_heading(frame, ui, area, shelf, &home, shelf_focused);
        let content = Rect::new(
            area.x,
            area.y.saturating_add(spacing.header()),
            area.width,
            area.height
                .saturating_sub(spacing.header())
                .min(content_height),
        );
        match shelf.kind {
            HomeShelfKind::QuickAccess => {
                render_tiles(
                    frame,
                    ui,
                    content,
                    shelf,
                    &home,
                    shelf_focused,
                    columns,
                    spacing,
                    &mut hits,
                );
            }
            _ if shelf.cards.is_empty() => {
                if let Some(message) = shelf.message {
                    workspace_text(
                        frame,
                        Rect::new(content.x, content.y, content.width, 1),
                        message,
                        ui.theme.workspace_secondary_text(),
                    );
                }
            }
            _ => {
                render_cards(
                    frame,
                    ui,
                    content,
                    shelf,
                    &home,
                    shelf_focused,
                    home.offset(shelf.kind),
                    per_row,
                    &mut hits,
                );
            }
        }
    }

    if hint_fits && (hidden_above || hidden_below) {
        let hint = match (hidden_above, hidden_below) {
            (true, true) => "More shelves above and below",
            (true, false) => "More shelves above",
            _ => "More shelves below",
        };
        workspace_text(
            frame,
            Rect::new(body.x, body.bottom().saturating_sub(1), body.width, 1),
            hint,
            ui.theme.workspace_secondary_text(),
        );
    }
    if let PageState::Home { state: home } = ui.current_page_mut() {
        home.scroll = scroll;
    }
    ui.workspace_hits.extend(hits);
}

/// The full list behind a Home shelf's "Show all".
pub fn render_home_shelf_list(
    is_active: bool,
    frame: &mut Frame,
    state: &SharedState,
    ui: &mut UIStateGuard,
    rect: Rect,
) {
    let PageState::HomeShelfList { shelf, .. } = ui.current_page() else {
        return;
    };
    let kind = *shelf;
    let rect = {
        let content = render_workspace_page_layout(frame, state, ui, rect);
        workspace_content_frame(frame, ui, content, kind.title())
    };
    let cards = crate::state::home_shelf_list(&state.data.read(), ui.home_scope(), kind);
    if cards.is_empty() {
        render_view_status(frame, &ui.theme, crate::state::UiViewStatus::Empty, rect);
        return;
    }
    let items = cards
        .iter()
        .map(|card| (format!("{}  ·  {}", card.title, card.subtitle), false))
        .collect();
    let selected_index = is_active
        .then(|| ui.current_page().selected_index())
        .flatten();
    let (list, len) = utils::construct_list_widget_with_width(
        &ui.theme,
        items,
        is_active,
        selected_index,
        Some(rect.width as usize),
        ui.presentation.focused_row_overflow,
        ui.focused_marquee_phase(),
    );
    let list = list.highlight_style(workspace_row_selection_style(ui, is_active));
    let list_offset = if let PageState::HomeShelfList {
        list: list_state, ..
    } = ui.current_page_mut()
    {
        utils::render_list_window_with_scrollbar(frame, list, rect, len, list_state);
        Some(list_state.offset())
    } else {
        None
    };
    if let Some(start) = list_offset {
        record_workspace_list_hits_with(ui, rect, start, len, WorkspaceHit::HomeListRow);
    }
}

/// Keep the focused shelf on screen, preferring the previous position.
fn page_scroll(
    shelves: &[HomeShelf],
    home: &HomePageUIState,
    columns: usize,
    spacing: Spacing,
    height: u16,
    max_scroll: u16,
) -> u16 {
    let mut tops = Vec::with_capacity(shelves.len());
    let mut top = 0u16;
    let mut wanted = None;
    for shelf in shelves {
        tops.push(top);
        if shelf.kind == home.focus {
            let bottom = top
                .saturating_add(spacing.header())
                .saturating_add(shelf_body_height(shelf, columns, spacing));
            wanted = Some(if top < home.scroll {
                top
            } else if bottom > home.scroll.saturating_add(height) {
                bottom.saturating_sub(height).min(top)
            } else {
                home.scroll
            });
        }
        top = top.saturating_add(shelf_height(shelf, columns, spacing));
    }
    let Some(wanted) = wanted else {
        return 0;
    };
    let wanted = wanted.min(max_scroll);
    // A shelf cut by the top edge is skipped, so a scroll between shelf
    // starts would leave its rows blank: snap to the next shelf start, which
    // still shows the focused shelf because it starts at or below it.
    tops.into_iter()
        .find(|&start| start >= wanted)
        .unwrap_or(wanted)
}

fn render_home_shelf_heading(
    frame: &mut Frame,
    ui: &UIStateGuard,
    area: Rect,
    shelf: &HomeShelf,
    home: &HomePageUIState,
    focused: bool,
) {
    let position = (focused && !shelf.cards.is_empty())
        .then(|| (home.selected(shelf.kind), shelf.cards.len()));
    render_shelf_heading(frame, ui, area, shelf.kind.title(), position);
}

#[allow(clippy::too_many_arguments)]
fn render_tiles(
    frame: &mut Frame,
    ui: &UIStateGuard,
    area: Rect,
    shelf: &HomeShelf,
    home: &HomePageUIState,
    focused: bool,
    columns: usize,
    spacing: Spacing,
    hits: &mut Vec<(Rect, WorkspaceHit)>,
) {
    let tile_width = tile_width(area.width, columns);
    for (index, card) in shelf.cards.iter().enumerate() {
        let column = u16::try_from(index % columns).unwrap_or(0);
        let row = u16::try_from(index / columns).unwrap_or(0);
        let y = area.y + row * spacing.tile_step;
        if y >= area.bottom() {
            break;
        }
        let tile = Rect::new(
            area.x + column * (tile_width + CARD_GAP),
            y,
            tile_width,
            area.bottom().saturating_sub(y).min(2),
        );
        let selected = focused && home.selected(shelf.kind) == index;
        render_card(
            frame,
            ui,
            tile,
            &card.title,
            &card.subtitle,
            selected.then(|| ui.theme.workspace_selection_active()),
        );
        hits.push((
            tile,
            WorkspaceHit::HomeCard {
                shelf: shelf.kind,
                index,
            },
        ));
    }
}

#[allow(clippy::too_many_arguments)]
fn render_cards(
    frame: &mut Frame,
    ui: &UIStateGuard,
    area: Rect,
    shelf: &HomeShelf,
    home: &HomePageUIState,
    focused: bool,
    offset: usize,
    per_row: usize,
    hits: &mut Vec<(Rect, WorkspaceHit)>,
) {
    let card_width = card_width(area.width, per_row);
    for (slot, (index, card)) in shelf
        .cards
        .iter()
        .enumerate()
        .skip(offset)
        .take(per_row)
        .enumerate()
    {
        let x = area.x + u16::try_from(slot).unwrap_or(0) * (card_width + CARD_GAP);
        let card_rect = Rect::new(
            x,
            area.y,
            card_width.min(area.right() - x),
            area.height.min(2),
        );
        if card_rect.is_empty() {
            break;
        }
        let selected = focused && home.selected(shelf.kind) == index;
        render_card(
            frame,
            ui,
            card_rect,
            &card.title,
            &card.subtitle,
            selected.then(|| ui.theme.workspace_selection_active()),
        );
        hits.push((
            card_rect,
            WorkspaceHit::HomeCard {
                shelf: shelf.kind,
                index,
            },
        ));
    }
}

#[cfg(test)]
mod tests {
    use super::visible_cards;

    #[test]
    fn narrow_shelves_still_show_one_card() {
        assert_eq!(visible_cards(10), 1);
        assert_eq!(visible_cards(24), 1);
        assert_eq!(visible_cards(52), 2);
    }
}
