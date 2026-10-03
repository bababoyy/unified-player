//! Settings as tiles: one shelf per section, each a grid of setting tiles
//! showing the label and value.

use ratatui::{layout::Rect, Frame};

use crate::config::{AppConfigSection, AppConfigSetting};
use crate::state::page::{settings_tile_shelves, SettingsCategory};
use crate::state::{ShelfLayout, ShelfNav, ShelfSize, UIStateGuard, WorkspaceHit};

use super::shelf::{grid_card_width, grid_columns, render_card, render_shelf_heading, CARD_GAP};
use super::{settings_display_label, settings_display_value, workspace_text};

const TILE_MIN_WIDTH: u16 = 26;
const MAX_TILE_COLUMNS: u16 = 4;
const TILE_HEIGHT: u16 = 2;
/// Tile areas shorter than this drop the blank row between tile rows.
const SPACIOUS_FROM_HEIGHT: u16 = 20;

/// Vertical geometry of the shelves, in page rows from the top of the first.
struct Geometry {
    columns: usize,
    row_step: u16,
}

impl Geometry {
    fn rows(&self, len: usize) -> u16 {
        u16::try_from(len.div_ceil(self.columns)).unwrap_or(u16::MAX)
    }

    /// Heading, tile rows, then a separating row.
    fn shelf_height(&self, len: usize) -> u16 {
        1 + self.rows(len).saturating_mul(self.row_step) + 1
    }

    /// Page row of tile `index` in a shelf starting at `top`.
    fn tile_top(&self, top: u16, index: usize) -> u16 {
        let row = u16::try_from(index / self.columns).unwrap_or(u16::MAX);
        top + 1 + row.saturating_mul(self.row_step)
    }
}

/// Draw the tiles of `category` into `area` and record their hits. The list
/// cursor names the selected setting; the category's [`ShelfNav`] keeps the
/// layout and page scroll.
pub(super) fn render_settings_tiles(
    frame: &mut Frame,
    ui: &mut UIStateGuard,
    area: Rect,
    settings: &[AppConfigSetting],
    category: SettingsCategory,
    query: Option<&str>,
    focused: bool,
) {
    let shelves = settings_tile_shelves(settings, category, query);
    if shelves.is_empty() {
        workspace_text(
            frame,
            Rect::new(area.x, area.y, area.width, 1),
            "No settings in this category",
            ui.theme.workspace_secondary_text(),
        );
        return;
    }
    let columns = grid_columns(area.width, TILE_MIN_WIDTH, MAX_TILE_COLUMNS);
    let tile_width = grid_card_width(area.width, columns);
    let geometry = Geometry {
        columns,
        row_step: TILE_HEIGHT + u16::from(area.height >= SPACIOUS_FROM_HEIGHT),
    };
    let total: u16 = shelves
        .iter()
        .map(|(_, sources)| geometry.shelf_height(sources.len()))
        .fold(0, u16::saturating_add);
    // The last row is kept for the "more" hint whenever shelves overflow.
    let viewport = if total > area.height {
        area.height.saturating_sub(1)
    } else {
        area.height
    };

    let mut nav = None;
    ui.current_page_mut()
        .update_settings_tiles(category, query, |shelf_nav, sizes| {
            for size in sizes {
                shelf_nav.set_layout(
                    size.key,
                    ShelfLayout::Grid {
                        columns,
                        step: tile_width + CARD_GAP,
                    },
                );
            }
            shelf_nav.clamp(sizes);
            shelf_nav.scroll = page_scroll(sizes, shelf_nav, &geometry, viewport);
            nav = Some(shelf_nav.clone());
            false
        });
    let Some(nav) = nav else {
        return;
    };

    let mut hits = Vec::new();
    let (mut hidden_above, mut hidden_below) = (false, false);
    let visible = |page_row: u16, height: u16| {
        page_row >= nav.scroll && page_row + height <= nav.scroll + viewport
    };
    let screen_y = |page_row: u16| area.y + page_row - nav.scroll;
    let mut top = 0u16;
    for (section, sources) in &shelves {
        let shelf_focused = focused && nav.focus == *section;
        if visible(top, 1) {
            let position = shelf_focused.then(|| (nav.selected(*section), sources.len()));
            render_shelf_heading(
                frame,
                ui,
                Rect::new(area.x, screen_y(top), area.width, 1),
                section.title(),
                position,
            );
        } else if top < nav.scroll {
            hidden_above = true;
        } else {
            hidden_below = true;
        }
        for (index, source) in sources.iter().enumerate() {
            let tile_top = geometry.tile_top(top, index);
            if !visible(tile_top, TILE_HEIGHT) {
                if tile_top < nav.scroll {
                    hidden_above = true;
                } else {
                    hidden_below = true;
                }
                continue;
            }
            let column = u16::try_from(index % columns).unwrap_or(0);
            let rect = Rect::new(
                area.x + column * (tile_width + CARD_GAP),
                screen_y(tile_top),
                tile_width,
                TILE_HEIGHT,
            );
            let setting = &settings[*source];
            let selection = (nav.focus == *section && nav.selected(*section) == index).then(|| {
                if focused {
                    ui.theme.workspace_selection_active()
                } else {
                    ui.theme.workspace_selection_inactive()
                }
            });
            render_card(
                frame,
                ui,
                rect,
                &settings_display_label(&setting.key),
                &settings_display_value(&setting.value),
                selection,
            );
            hits.push((rect, WorkspaceHit::SettingsRow(*source)));
        }
        top += geometry.shelf_height(sources.len());
    }

    if hidden_above || hidden_below {
        let hint = match (hidden_above, hidden_below) {
            (true, true) => "More settings above and below",
            (true, false) => "More settings above",
            _ => "More settings below",
        };
        workspace_text(
            frame,
            Rect::new(area.x, area.bottom().saturating_sub(1), area.width, 1),
            hint,
            ui.theme.workspace_secondary_text(),
        );
    }
    ui.workspace_hits.extend(hits);
}

/// Keep the selected tile on screen, preferring the previous scroll. A tile
/// on a shelf's first row brings the shelf heading along.
fn page_scroll(
    sizes: &[ShelfSize<AppConfigSection>],
    nav: &ShelfNav<AppConfigSection>,
    geometry: &Geometry,
    viewport: u16,
) -> u16 {
    let mut top = 0u16;
    for size in sizes {
        if size.key == nav.focus {
            let index = nav.selected(size.key);
            let tile_top = geometry.tile_top(top, index);
            let wanted_top = if index < geometry.columns {
                top
            } else {
                tile_top
            };
            let bottom = tile_top + TILE_HEIGHT;
            return if wanted_top < nav.scroll {
                wanted_top
            } else if bottom > nav.scroll.saturating_add(viewport) {
                bottom.saturating_sub(viewport).min(wanted_top)
            } else {
                nav.scroll
            };
        }
        top = top.saturating_add(geometry.shelf_height(size.len));
    }
    0
}

#[cfg(test)]
mod tests {
    use super::{page_scroll, Geometry};
    use crate::config::AppConfigSection;
    use crate::state::{ShelfNav, ShelfSize};

    #[test]
    fn scrolling_keeps_the_selected_tile_and_its_first_row_heading_visible() {
        // Two-column grids with a blank row between tile rows.
        let geometry = Geometry {
            columns: 2,
            row_step: 3,
        };
        let sizes = [
            ShelfSize {
                key: AppConfigSection::Spotify,
                len: 6,
            },
            ShelfSize {
                key: AppConfigSection::Playback,
                len: 2,
            },
        ];
        let mut nav = ShelfNav::default();
        // Spotify spans rows 0..11; its last tile row starts at row 7.
        nav.select(AppConfigSection::Spotify, 5);
        assert_eq!(page_scroll(&sizes, &nav, &geometry, 6), 3);

        // Playback starts at row 11; its heading comes along with row one.
        nav.scroll = 3;
        nav.select(AppConfigSection::Playback, 1);
        assert_eq!(page_scroll(&sizes, &nav, &geometry, 6), 8);

        // Moving back up reveals the Spotify heading again.
        nav.scroll = 8;
        nav.select(AppConfigSection::Spotify, 0);
        assert_eq!(page_scroll(&sizes, &nav, &geometry, 6), 0);
    }
}
