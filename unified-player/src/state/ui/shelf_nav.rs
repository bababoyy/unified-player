//! Keyboard selection over a page of stacked shelves, each a grid of tiles or
//! a horizontally scrolling row of cards.
//!
//! Callers name shelves with their own key type and pass the visible shelves
//! in page order on every move; the renderer reports each shelf's layout and
//! keeps row offsets and the page scroll here.

/// A visible shelf in page order. Shelves without cards are skipped by
/// vertical moves.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ShelfSize<K> {
    pub key: K,
    pub len: usize,
}

/// How the renderer last laid out a shelf. `step` is one card's width, gap
/// included.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ShelfLayout {
    /// Cards wrap into rows of `columns`.
    Grid { columns: usize, step: u16 },
    /// One row showing `visible` cards that scrolls horizontally.
    Row { visible: usize, step: u16 },
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct ShelfEntry<K> {
    key: K,
    selected: usize,
    offset: usize,
    layout: Option<ShelfLayout>,
}

/// The focused shelf and each shelf's remembered card. Moving between shelves
/// keeps the screen column of the selection.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ShelfNav<K> {
    pub focus: K,
    entries: Vec<ShelfEntry<K>>,
    /// First visible row of the page, kept by the renderer.
    pub scroll: u16,
}

impl<K: Copy + Eq> ShelfNav<K> {
    fn entry(&self, key: K) -> Option<&ShelfEntry<K>> {
        self.entries.iter().find(|entry| entry.key == key)
    }

    fn entry_mut(&mut self, key: K) -> &mut ShelfEntry<K> {
        let position = self
            .entries
            .iter()
            .position(|entry| entry.key == key)
            .unwrap_or_else(|| {
                self.entries.push(ShelfEntry {
                    key,
                    selected: 0,
                    offset: 0,
                    layout: None,
                });
                self.entries.len() - 1
            });
        &mut self.entries[position]
    }

    pub fn selected(&self, key: K) -> usize {
        self.entry(key).map_or(0, |entry| entry.selected)
    }

    pub fn select(&mut self, key: K, index: usize) {
        self.focus = key;
        self.entry_mut(key).selected = index;
    }

    /// First visible card of a row shelf.
    pub fn offset(&self, key: K) -> usize {
        self.entry(key).map_or(0, |entry| entry.offset)
    }

    /// Record how a shelf was drawn.
    pub fn set_layout(&mut self, key: K, layout: ShelfLayout) {
        self.entry_mut(key).layout = Some(layout);
    }

    /// Scroll a row shelf just enough to show its selected card, returning the
    /// first visible card.
    pub fn scroll_row(&mut self, key: K, visible: usize) -> usize {
        let entry = self.entry_mut(key);
        entry.offset = scroll_into_view(entry.offset, entry.selected, visible);
        entry.offset
    }

    /// Keep focus on a shelf that is still visible and each selection inside
    /// its shelf.
    pub fn clamp(&mut self, shelves: &[ShelfSize<K>]) {
        for shelf in shelves {
            let entry = self.entry_mut(shelf.key);
            entry.selected = entry.selected.min(shelf.len.saturating_sub(1));
        }
        if !shelves
            .iter()
            .any(|shelf| shelf.key == self.focus && shelf.len > 0)
        {
            if let Some(first) = shelves.iter().find(|shelf| shelf.len > 0) {
                self.focus = first.key;
            }
        }
    }

    /// Move within the focused shelf.
    pub fn move_horizontal(&mut self, shelves: &[ShelfSize<K>], delta: isize) -> bool {
        let Some(shelf) = shelves.iter().find(|shelf| shelf.key == self.focus) else {
            return false;
        };
        let entry = self.entry_mut(shelf.key);
        let current = entry.selected;
        entry.selected = current
            .saturating_add_signed(delta)
            .min(shelf.len.saturating_sub(1));
        entry.selected != current
    }

    /// Move between grid rows, then between shelves with cards.
    pub fn move_vertical(&mut self, shelves: &[ShelfSize<K>], delta: isize) -> bool {
        self.move_grid_row(shelves, delta) || self.move_between_shelves(shelves, delta)
    }

    /// Move one row within the focused grid, if it has a row that way.
    fn move_grid_row(&mut self, shelves: &[ShelfSize<K>], delta: isize) -> bool {
        let Some(shelf) = shelves.iter().find(|shelf| shelf.key == self.focus) else {
            return false;
        };
        let entry = self.entry_mut(shelf.key);
        let Some(ShelfLayout::Grid { columns, .. }) = known(entry.layout) else {
            return false;
        };
        let target = if delta > 0 {
            entry
                .selected
                .checked_add(columns)
                .filter(|index| *index < shelf.len)
        } else {
            entry.selected.checked_sub(columns)
        };
        target.is_some_and(|target| {
            entry.selected = target;
            true
        })
    }

    /// Move `delta` shelves with cards away, skipping any remaining grid rows.
    pub fn move_between_shelves(&mut self, shelves: &[ShelfSize<K>], delta: isize) -> bool {
        let navigable: Vec<K> = shelves
            .iter()
            .filter(|shelf| shelf.len > 0)
            .map(|shelf| shelf.key)
            .collect();
        let Some(position) = navigable.iter().position(|key| *key == self.focus) else {
            return false;
        };
        let Some(next) = position
            .checked_add_signed(delta)
            .and_then(|index| navigable.get(index))
        else {
            return false;
        };
        self.enter_shelf(shelves, *next, delta < 0);
        true
    }

    /// Jump to the first or last shelf with cards.
    pub fn move_to_edge(&mut self, shelves: &[ShelfSize<K>], last: bool) -> bool {
        let mut navigable = shelves.iter().filter(|shelf| shelf.len > 0);
        let target = if last {
            navigable.next_back()
        } else {
            navigable.next()
        };
        let Some(target) = target else {
            return false;
        };
        if target.key != self.focus {
            let from_below = shelves.iter().position(|shelf| shelf.key == target.key)
                < shelves.iter().position(|shelf| shelf.key == self.focus);
            self.enter_shelf(shelves, target.key, from_below);
        }
        true
    }

    /// Focus `key`, selecting the card in the screen column of the current
    /// selection. A grid is entered on its bottom row when coming from below.
    /// Before either shelf is drawn the remembered card is kept.
    fn enter_shelf(&mut self, shelves: &[ShelfSize<K>], key: K, from_below: bool) {
        let x = self.entry(self.focus).and_then(column_x);
        self.focus = key;
        let Some(shelf) = shelves.iter().find(|shelf| shelf.key == key) else {
            return;
        };
        let entry = self.entry_mut(key);
        let (Some(x), Some(layout)) = (x, known(entry.layout)) else {
            return;
        };
        let last = shelf.len.saturating_sub(1);
        entry.selected = match layout {
            ShelfLayout::Grid { columns, step } => {
                let column = usize::from(x / step).min(columns - 1);
                let row = if from_below { last / columns } else { 0 };
                let index = row * columns + column;
                // A short bottom row has no card in this column; use the row above.
                if index > last {
                    index.saturating_sub(columns).min(last)
                } else {
                    index
                }
            }
            ShelfLayout::Row { visible, step } => {
                let slot = usize::from(x / step).min(visible - 1);
                (entry.offset + slot).min(last)
            }
        };
    }
}

/// The layout, if it has the non-zero sizes column arithmetic needs.
fn known(layout: Option<ShelfLayout>) -> Option<ShelfLayout> {
    layout.filter(|layout| match *layout {
        ShelfLayout::Grid { columns, step } => columns > 0 && step > 0,
        ShelfLayout::Row { visible, step } => visible > 0 && step > 0,
    })
}

/// Screen x of the middle of the selected card, relative to the shelf's left
/// edge.
fn column_x<K>(entry: &ShelfEntry<K>) -> Option<u16> {
    let (column, step) = match known(entry.layout)? {
        ShelfLayout::Grid { columns, step } => (entry.selected % columns, step),
        ShelfLayout::Row { step, .. } => (entry.selected.saturating_sub(entry.offset), step),
    };
    let column = u16::try_from(column).ok()?;
    Some(column.saturating_mul(step).saturating_add(step / 2))
}

/// First visible card that keeps `selected` within a row of `visible` cards.
fn scroll_into_view(offset: usize, selected: usize, visible: usize) -> usize {
    if selected < offset {
        selected
    } else if selected >= offset + visible {
        selected + 1 - visible
    } else {
        offset
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const GRID: u8 = 0;
    const EMPTY: u8 = 1;
    const ROW_A: u8 = 2;
    const ROW_B: u8 = 3;

    fn shelves() -> [ShelfSize<u8>; 4] {
        [
            ShelfSize { key: GRID, len: 6 },
            ShelfSize { key: EMPTY, len: 0 },
            ShelfSize {
                key: ROW_A,
                len: 10,
            },
            ShelfSize {
                key: ROW_B,
                len: 10,
            },
        ]
    }

    /// Four 30-wide tiles above rows of four 20-wide cards.
    fn laid_out() -> ShelfNav<u8> {
        let mut nav = ShelfNav::default();
        nav.set_layout(
            GRID,
            ShelfLayout::Grid {
                columns: 4,
                step: 30,
            },
        );
        for key in [ROW_A, ROW_B] {
            nav.set_layout(
                key,
                ShelfLayout::Row {
                    visible: 4,
                    step: 20,
                },
            );
        }
        nav
    }

    #[test]
    fn keyboard_moves_through_grid_rows_then_shelves_skipping_empty_ones() {
        let shelves = shelves();
        let mut nav = laid_out();

        assert!(nav.move_horizontal(&shelves, 1));
        assert!(nav.move_vertical(&shelves, 1));
        assert_eq!(nav.selected(GRID), 5);
        assert!(nav.move_vertical(&shelves, 1));
        assert_eq!(nav.focus, ROW_A);
        assert!(nav.move_horizontal(&shelves, 20));
        assert_eq!(nav.selected(ROW_A), 9);
        assert!(nav.move_to_edge(&shelves, true));
        assert_eq!(nav.focus, ROW_B);
        assert!(!nav.move_vertical(&shelves, 1));
    }

    #[test]
    fn moving_between_shelves_keeps_the_screen_column() {
        let shelves = shelves();
        let mut nav = laid_out();
        nav.select(ROW_B, 9);
        nav.scroll_row(ROW_B, 4);
        nav.select(ROW_A, 7);
        nav.scroll_row(ROW_A, 4);
        nav.select(ROW_A, 5);

        // The second visible card (x 30) lands on the second visible card
        // below, not on the remembered tenth.
        assert!(nav.move_vertical(&shelves, 1));
        assert_eq!(nav.selected(ROW_B), 7);

        // x 30 is the second tile column; from below, the grid is entered on
        // its bottom row, which holds tiles 4 and 5.
        nav.select(ROW_A, 5);
        assert!(nav.move_vertical(&shelves, -1));
        assert_eq!(nav.focus, GRID);
        assert_eq!(nav.selected(GRID), 5);

        // Tile 3 (x 105) is past the last visible card, so the row's last.
        nav.select(GRID, 3);
        assert!(nav.move_vertical(&shelves, 1));
        assert_eq!(nav.selected(ROW_A), 7);

        // That card (x 70) is in the third tile column, which the short
        // bottom row lacks; the row above has it.
        assert!(nav.move_to_edge(&shelves, false));
        assert_eq!(nav.selected(GRID), 2);
    }

    #[test]
    fn before_the_first_render_a_shelf_keeps_its_remembered_card() {
        let shelves = shelves();
        let mut nav = ShelfNav::default();
        nav.select(ROW_B, 8);
        nav.select(ROW_A, 2);
        assert!(nav.move_vertical(&shelves, 1));
        assert_eq!(nav.selected(ROW_B), 8);
    }

    #[test]
    fn a_row_scrolls_just_enough_to_show_the_selected_card() {
        assert_eq!(scroll_into_view(0, 2, 4), 0);
        assert_eq!(scroll_into_view(0, 5, 4), 2);
        assert_eq!(scroll_into_view(3, 1, 4), 1);
    }

    #[test]
    fn clamping_moves_focus_off_a_shelf_that_emptied() {
        let mut nav = ShelfNav::default();
        nav.select(ROW_A, 7);
        let shelves = [
            ShelfSize { key: GRID, len: 1 },
            ShelfSize { key: ROW_A, len: 3 },
        ];
        nav.clamp(&shelves);
        assert_eq!(nav.selected(ROW_A), 2);

        nav.clamp(&shelves[..1]);
        assert_eq!(nav.focus, GRID);
    }
}
