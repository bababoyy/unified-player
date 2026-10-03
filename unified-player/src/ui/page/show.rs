//! Spotify podcast (show) page in the workspace shell: the shared collection
//! table with episodes as rows and the release date as the detail column.

use crate::state::{
    Context, ContextId, ContextPageUIState, Episode, PageState, SharedState, UIStateGuard,
    WorkspaceFocusState,
};
use crate::ui::components::collection::CollectionTrackRow;
use ratatui::{layout::Rect, Frame};
use rspotify::prelude::Id;

use super::{
    collection_full_profile, render_workspace_collection_table, workspace_collection_visible_rows,
    workspace_context_heading, workspace_context_scope, CollectionDetailColumn,
};

/// Render a loaded Spotify show. Returns `false` when the cached context is
/// not a show, so the caller can fall back.
pub(super) fn render_workspace_spotify_show(
    frame: &mut Frame,
    state: &SharedState,
    ui: &mut UIStateGuard,
    rect: Rect,
    context_id: &ContextId,
) -> bool {
    let (title, rows, playing_index) = {
        let data = state.data.read();
        let Some(Context::Show { show, episodes }) = data.caches.context.get(&context_id.uri())
        else {
            return false;
        };
        // The key handler indexes the filtered episodes, so rows must match it.
        let visible = ui.search_filtered_items_projection(episodes);
        let playing_uri =
            state.player.read().playback.as_ref().and_then(|playback| {
                match playback.item.as_ref() {
                    Some(rspotify::model::PlayableItem::Episode(episode)) => Some(episode.id.uri()),
                    _ => None,
                }
            });
        let playing_index =
            playing_uri.and_then(|uri| visible.iter().position(|episode| episode.id.uri() == uri));
        let rows = visible.iter().map(episode_row).collect::<Vec<_>>();
        (show.name.clone(), rows, playing_index)
    };

    let full_profile = collection_full_profile(frame, rect);
    workspace_context_heading(
        frame,
        &ui.theme,
        rect,
        &title,
        &workspace_context_scope(ui),
        full_profile,
    );
    let focused_row = ui.current_page().selected_index();
    let context_focused = ui.workspace_focus == WorkspaceFocusState::Context;
    let theme = ui.theme.clone();
    let focused_overflow = ui.presentation.focused_row_overflow;
    let focused_phase = ui.focused_marquee_phase();
    let episode_count = rows.len();
    let visible_rows = usize::from(workspace_collection_visible_rows(rect, full_profile));
    let PageState::Context {
        state: Some(ContextPageUIState::Show { episode_table }),
        ..
    } = ui.current_page_mut()
    else {
        return false;
    };
    crate::ui::utils::adjust_table_offset(episode_table, episode_count, visible_rows as u16);
    let row_offset = episode_table.offset();
    let window = rows
        .into_iter()
        .skip(row_offset)
        .take(visible_rows)
        .collect::<Vec<_>>();
    let mut hits = Vec::new();
    render_workspace_collection_table(
        frame,
        &theme,
        rect,
        &window,
        row_offset,
        episode_count,
        focused_row,
        context_focused,
        &[],
        playing_index,
        episode_table,
        &format!("{episode_count} episodes shown"),
        theme.workspace_secondary_text(),
        CollectionDetailColumn::RELEASE_DATE,
        full_profile,
        focused_overflow,
        focused_phase,
        &mut hits,
    );
    ui.workspace_hits.extend(hits);
    true
}

fn episode_row(episode: &Episode) -> CollectionTrackRow {
    CollectionTrackRow {
        title: episode.name.clone(),
        artist: episode.release_date.clone(),
        duration: episode_duration(episode.duration),
    }
}

/// `m:ss`, or `h:mm:ss` once an episode reaches an hour; the table widens its
/// time column for the longer form.
fn episode_duration(duration: std::time::Duration) -> String {
    let seconds = duration.as_secs();
    if seconds >= 3600 {
        format!(
            "{}:{:02}:{:02}",
            seconds / 3600,
            seconds / 60 % 60,
            seconds % 60
        )
    } else {
        format!("{}:{:02}", seconds / 60, seconds % 60)
    }
}

#[cfg(test)]
mod tests {
    use super::episode_duration;
    use std::time::Duration;

    #[test]
    fn episode_durations_switch_to_hours_at_sixty_minutes() {
        assert_eq!(episode_duration(Duration::from_secs(59 * 60 + 59)), "59:59");
        assert_eq!(episode_duration(Duration::from_hours(1)), "1:00:00");
        assert_eq!(
            episode_duration(Duration::from_secs(2 * 3600 + 5 * 60 + 7)),
            "2:05:07"
        );
    }
}
