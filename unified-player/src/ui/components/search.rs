//! Shared provider-neutral search group projection and geometry.

use crate::state::SearchFocusState;
use ratatui::layout::Rect;

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct WorkspaceSearchItem {
    pub(crate) title: String,
    pub(crate) metadata: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct WorkspaceSearchGroup {
    pub(crate) focus: SearchFocusState,
    pub(crate) label: &'static str,
    pub(crate) metadata_width: usize,
    pub(crate) item_offset: usize,
    pub(crate) total_items: usize,
    pub(crate) selected_index: Option<usize>,
    pub(crate) selected_item: Option<WorkspaceSearchItem>,
    pub(crate) items: Vec<WorkspaceSearchItem>,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct WorkspaceSearchProjectionWindow {
    pub(crate) offset: usize,
    pub(crate) viewport: usize,
    pub(crate) selected: Option<usize>,
}

pub(crate) fn workspace_search_item(
    title: impl Into<String>,
    metadata: impl Into<String>,
) -> WorkspaceSearchItem {
    WorkspaceSearchItem {
        title: title.into(),
        metadata: metadata.into(),
    }
}

pub(crate) fn workspace_search_group_is_visible(
    visible_focus: Option<SearchFocusState>,
    group_focus: SearchFocusState,
) -> bool {
    visible_focus.is_none_or(|visible| visible == group_focus)
}

pub(crate) fn workspace_search_visible_focus(
    wide: bool,
    category: Option<crate::command::ProviderSearchPane>,
    focus: SearchFocusState,
) -> Option<SearchFocusState> {
    if wide {
        None
    } else if let Some(category) = category {
        Some(SearchFocusState::from_provider_pane(category))
    } else if matches!(focus, SearchFocusState::Category | SearchFocusState::Input) {
        Some(SearchFocusState::Tracks)
    } else {
        Some(focus)
    }
}

pub(crate) fn workspace_search_projection_window(
    windows: Option<&[(SearchFocusState, WorkspaceSearchProjectionWindow)]>,
    focus: SearchFocusState,
) -> WorkspaceSearchProjectionWindow {
    windows
        .and_then(|windows| {
            windows
                .iter()
                .find(|(window_focus, _)| *window_focus == focus)
                .map(|(_, window)| *window)
        })
        .unwrap_or(WorkspaceSearchProjectionWindow {
            offset: 0,
            viewport: usize::MAX,
            selected: None,
        })
}

fn workspace_search_visible_items<T>(
    items: &[T],
    window: WorkspaceSearchProjectionWindow,
) -> impl Iterator<Item = (usize, &T)> + '_ {
    let start = window.offset.min(items.len());
    let end = start.saturating_add(window.viewport).min(items.len());
    items
        .get(start..end)
        .unwrap_or_default()
        .iter()
        .enumerate()
        .map(move |(index, item)| (start.saturating_add(index), item))
}

pub(crate) fn workspace_search_group(
    focus: SearchFocusState,
    label: &'static str,
    metadata_width: usize,
    window: WorkspaceSearchProjectionWindow,
    total_items: usize,
    selected_item: Option<WorkspaceSearchItem>,
    items: Vec<WorkspaceSearchItem>,
) -> WorkspaceSearchGroup {
    WorkspaceSearchGroup {
        focus,
        label,
        metadata_width,
        item_offset: window.offset.min(total_items),
        total_items,
        selected_index: window.selected,
        selected_item,
        items,
    }
}

pub(crate) fn workspace_search_group_from_items<T, F>(
    focus: SearchFocusState,
    label: &'static str,
    metadata_width: usize,
    window: WorkspaceSearchProjectionWindow,
    source: Option<&[T]>,
    item_builder: F,
) -> WorkspaceSearchGroup
where
    F: Fn(&T) -> WorkspaceSearchItem,
{
    let total_items = source.map_or(0, <[T]>::len);
    let selected_item = source
        .and_then(|items| window.selected.and_then(|index| items.get(index)))
        .map(&item_builder);
    let items = source
        .map(|items| {
            workspace_search_visible_items(items, window)
                .map(|(_, item)| item_builder(item))
                .collect()
        })
        .unwrap_or_default();
    workspace_search_group(
        focus,
        label,
        metadata_width,
        window,
        total_items,
        selected_item,
        items,
    )
}

pub(crate) fn workspace_search_loaded_count(loaded: usize, viewport: u16) -> String {
    format!("{} of {} loaded", loaded.min(viewport as usize), loaded)
}

pub(crate) fn workspace_search_group_rects(rect: Rect, count: usize) -> Vec<Rect> {
    if count == 0 || rect.width < 4 || rect.height < 3 {
        return Vec::new();
    }
    let columns = 2_u16.min(count as u16);
    let rows = (count as u16).div_ceil(columns);
    let column_gap = if columns == 2 { 3 } else { 0 };
    let column_width = rect
        .width
        .saturating_sub(column_gap)
        .checked_div(columns)
        .unwrap_or(0);
    let row_gap: u16 = u16::from(rows > 1);
    let row_height = rect
        .height
        .saturating_sub(row_gap.saturating_mul(rows.saturating_sub(1)))
        .checked_div(rows)
        .unwrap_or(0);
    let mut result = Vec::with_capacity(count);
    for index in 0..count {
        let row = (index as u16) / columns;
        let column = (index as u16) % columns;
        let x = rect
            .x
            .saturating_add(column.saturating_mul(column_width.saturating_add(column_gap)));
        let y = rect
            .y
            .saturating_add(row.saturating_mul(row_height.saturating_add(row_gap)));
        result.push(Rect::new(x, y, column_width, row_height));
    }
    result
}

#[cfg(test)]
mod tests {
    use super::{
        workspace_search_group_from_items, workspace_search_group_rects,
        workspace_search_visible_focus, WorkspaceSearchProjectionWindow,
    };
    use crate::command::ProviderSearchPane;
    use crate::state::SearchFocusState;
    use ratatui::layout::Rect;

    #[test]
    fn grouped_projection_keeps_global_selection_and_panel_geometry() {
        let source = ["Item 0", "Item 1", "Item 2", "Item 3"];
        let group = workspace_search_group_from_items(
            SearchFocusState::Tracks,
            "Tracks",
            25,
            WorkspaceSearchProjectionWindow {
                offset: 1,
                viewport: 2,
                selected: Some(2),
            },
            Some(source.as_slice()),
            |item| super::workspace_search_item(*item, "metadata"),
        );
        assert_eq!(group.item_offset, 1);
        assert_eq!(group.selected_index, Some(2));
        assert_eq!(group.items.len(), 2);
        assert_eq!(group.selected_item.as_ref().unwrap().title, "Item 2");

        assert_eq!(
            workspace_search_group_rects(Rect::new(29, 10, 149, 26), 2),
            vec![Rect::new(29, 10, 73, 26), Rect::new(105, 10, 73, 26)]
        );
    }

    #[test]
    fn compact_focus_uses_the_selected_provider_pane() {
        assert_eq!(
            workspace_search_visible_focus(
                false,
                Some(ProviderSearchPane::Albums),
                SearchFocusState::Tracks,
            ),
            Some(SearchFocusState::Albums)
        );
    }
}
