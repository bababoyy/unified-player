//! Shared list/table hit geometry.

use ratatui::layout::Rect;
use ratatui::widgets::{ListState, TableState};
use std::ops::Range;

/// Clamp a selection after a refresh or resize without losing the nearest
/// still-visible item.
pub(crate) fn adjusted_selection(selected: Option<usize>, len: usize) -> Option<usize> {
    if len == 0 {
        return selected.map(|_| 0);
    }
    Some(selected.unwrap_or_default().min(len - 1))
}

fn edge_aware_viewport_offset(
    selected: Option<usize>,
    current_offset: usize,
    len: usize,
    viewport_rows: u16,
) -> usize {
    let visible = usize::from(viewport_rows).max(1);
    let max_offset = len.saturating_sub(visible);
    let current = current_offset.min(max_offset);
    let Some(selected) = selected else {
        return current;
    };
    let next = if selected < current {
        selected
    } else if selected >= current.saturating_add(visible) {
        selected.saturating_add(1).saturating_sub(visible)
    } else {
        current
    };
    next.min(max_offset)
}

fn adjust_list_state(state: &mut ListState, len: usize) {
    state.select(adjusted_selection(state.selected(), len));
}

/// The offset `adjust_list_offset` settles on for this selection. Renderers
/// that project only the visible rows compute it before projecting, so the
/// projected window matches the rows the list draws in the same frame.
pub(crate) fn list_offset_for_selection(
    selected: Option<usize>,
    current_offset: usize,
    len: usize,
    viewport_rows: u16,
) -> usize {
    edge_aware_viewport_offset(
        adjusted_selection(selected, len),
        current_offset,
        len,
        viewport_rows,
    )
}

/// Keep a selected row visible without moving the viewport unnecessarily.
pub(crate) fn adjust_list_offset(state: &mut ListState, len: usize, viewport_rows: u16) {
    let offset = list_offset_for_selection(state.selected(), state.offset(), len, viewport_rows);
    adjust_list_state(state, len);
    *state.offset_mut() = offset;
}

fn multiline_viewport_offset(
    selected: Option<usize>,
    current_offset: usize,
    row_heights: &[usize],
    viewport_rows: u16,
) -> usize {
    let visible = usize::from(viewport_rows).max(1);
    let len = row_heights.len();
    let current = current_offset.min(len.saturating_sub(1));
    let Some(selected) = selected else {
        return current;
    };
    if selected < current {
        return selected;
    }

    let used = row_heights[current..=selected]
        .iter()
        .copied()
        .sum::<usize>();
    if used <= visible {
        return current;
    }

    let mut start = selected;
    let mut height = row_heights[selected];
    while start > 0 && height + row_heights[start - 1] <= visible {
        start -= 1;
        height += row_heights[start];
    }
    start
}

/// Keep a selected item visible when list items can occupy multiple rows.
pub(crate) fn adjust_multiline_list_offset(
    state: &mut ListState,
    row_heights: &[usize],
    viewport_rows: u16,
) {
    adjust_list_state(state, row_heights.len());
    *state.offset_mut() =
        multiline_viewport_offset(state.selected(), state.offset(), row_heights, viewport_rows);
}

fn adjust_table_state(state: &mut TableState, len: usize) {
    state.select(adjusted_selection(state.selected(), len));
}

/// Keep a selected table row visible using the same edge-aware rule as lists.
pub(crate) fn adjust_table_offset(state: &mut TableState, len: usize, viewport_rows: u16) {
    adjust_table_state(state, len);
    *state.offset_mut() =
        edge_aware_viewport_offset(state.selected(), state.offset(), len, viewport_rows);
}

/// Reserve a stable one-cell scrollbar gutter for every list/table viewport.
pub(crate) fn vertical_scrollbar_regions(rect: Rect) -> (Rect, Rect) {
    if rect.width < 2 {
        return (rect, Rect::default());
    }

    (
        Rect {
            width: rect.width - 1,
            ..rect
        },
        Rect {
            x: rect.right() - 1,
            width: 1,
            ..rect
        },
    )
}

/// Adjust a global list state and return the full-dataset range visible in the viewport.
pub(crate) fn prepare_list_viewport_with_scrollbar(
    rect: Rect,
    len: usize,
    state: &mut ListState,
) -> Range<usize> {
    let (content_rect, _) = vertical_scrollbar_regions(rect);
    let viewport_rows = content_rect.height as usize;
    adjust_list_offset(state, len, content_rect.height);
    let start = state.offset();
    let end = start.saturating_add(viewport_rows).min(len);
    start..end
}

/// Adjust a global table state and return the visible body-row range.
pub(crate) fn prepare_table_viewport_with_scrollbar(
    rect: Rect,
    len: usize,
    state: &mut TableState,
) -> Range<usize> {
    let viewport_rows = rect.height.saturating_sub(1);
    adjust_table_offset(state, len, viewport_rows);
    let start = state.offset();
    let end = start.saturating_add(viewport_rows as usize).min(len);
    start..end
}

/// Record visible global row indices with one shared list/table hit rule.
pub(crate) fn record_visible_hits<T, F>(
    hits: &mut Vec<(Rect, T)>,
    rect: Rect,
    start: usize,
    item_count: usize,
    header_rows: u16,
    make_hit: F,
) where
    F: Fn(usize) -> T,
{
    let visible_rows = rect.height.saturating_sub(header_rows) as usize;
    if rect.width == 0 || visible_rows == 0 || item_count == 0 {
        return;
    }
    let row_width = if rect.width >= 2 {
        rect.width - 1
    } else {
        rect.width
    };
    let start = start.min(item_count);
    let end = start.saturating_add(visible_rows).min(item_count);
    for index in start..end {
        hits.push((
            Rect::new(
                rect.x,
                rect.y
                    .saturating_add(header_rows)
                    .saturating_add((index - start) as u16),
                row_width,
                1,
            ),
            make_hit(index),
        ));
    }
}

/// Publish the visible global row indices for a list viewport.
///
/// Renderers may choose different widgets or styles, but every list uses the
/// same row-width and viewport-to-global-index rule for mouse ownership.
pub(crate) fn record_visible_row_hits(
    hits: &mut Vec<(Rect, usize)>,
    rect: Rect,
    start: usize,
    item_count: usize,
) {
    record_visible_hits(hits, rect, start, item_count, 0, |index| index);
}

#[cfg(test)]
mod tests {
    use super::{
        adjust_list_offset, adjust_table_offset, adjusted_selection, record_visible_hits,
        record_visible_row_hits, vertical_scrollbar_regions,
    };
    use ratatui::layout::Rect;
    use ratatui::widgets::{ListState, TableState};

    #[test]
    fn visible_row_hits_keep_global_indices_and_scrollbar_gutter() {
        let mut hits = Vec::new();
        record_visible_row_hits(&mut hits, Rect::new(4, 6, 12, 3), 7, 20);

        assert_eq!(
            hits,
            vec![
                (Rect::new(4, 6, 11, 1), 7),
                (Rect::new(4, 7, 11, 1), 8),
                (Rect::new(4, 8, 11, 1), 9),
            ]
        );
    }

    #[test]
    fn list_and_table_viewports_share_selection_and_gutter_rules() {
        assert_eq!(adjusted_selection(Some(8), 4), Some(3));
        let (content, gutter) = vertical_scrollbar_regions(Rect::new(0, 0, 10, 4));
        assert_eq!(content.width, 9);
        assert_eq!(gutter, Rect::new(9, 0, 1, 4));

        let mut list = ListState::default().with_selected(Some(8));
        adjust_list_offset(&mut list, 10, 3);
        let mut table = TableState::default().with_selected(Some(8));
        adjust_table_offset(&mut table, 10, 3);
        assert_eq!(list.selected(), Some(8));
        assert_eq!(table.selected(), Some(8));
        assert_eq!(list.offset(), table.offset());
    }

    #[test]
    fn row_hit_contract_applies_the_same_header_and_global_index_rule() {
        let mut hits = Vec::new();
        record_visible_hits(&mut hits, Rect::new(2, 3, 12, 4), 5, 20, 1, |index| index);
        assert_eq!(hits[0], (Rect::new(2, 4, 11, 1), 5));
        assert_eq!(hits[2], (Rect::new(2, 6, 11, 1), 7));

        let mut list_hits = Vec::new();
        record_visible_row_hits(&mut list_hits, Rect::new(2, 3, 12, 3), 5, 20);
        assert_eq!(hits[0].0.y, 4);
        assert_eq!(list_hits[0].0.y, 3);
    }
}
