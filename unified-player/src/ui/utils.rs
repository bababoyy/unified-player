use super::{
    config, Block, BorderType, Borders, Frame, List, ListItem, ListState, Rect, Span, Style, Table,
    TableState,
};
use ratatui::widgets::{Scrollbar, ScrollbarOrientation, ScrollbarState};
use unicode_bidi::BidiInfo;

pub(crate) use super::components::list::{
    adjust_list_offset, adjust_multiline_list_offset, adjust_table_offset,
    list_offset_for_selection, prepare_list_viewport_with_scrollbar,
    prepare_table_viewport_with_scrollbar, vertical_scrollbar_regions,
};

// Table cells are projected before Ratatui resolves their column rectangles.
// Keep the fallback deliberately conservative; measured list/popup cells use
// their actual width instead.
const MARQUEE_FALLBACK_WIDTH: usize = 256;

/// Keep user-visible labels inside their terminal cell without relying on a
/// widget's implicit clipping. This is intentionally character-bounded; the
/// renderer still applies the terminal's final cell-width clipping for wide
/// glyphs.
pub fn bounded_text(text: &str, maximum: usize) -> String {
    let character_count = text.chars().count();
    if character_count <= maximum {
        return text.to_owned();
    }
    if maximum <= 3 {
        return text.chars().take(maximum).collect();
    }
    let mut result = text.chars().take(maximum - 3).collect::<String>();
    result.push_str("...");
    result
}

/// Render an overflowing focused-row cell according to the presentation
/// preference. The phase is supplied by the caller so rendering stays pure
/// and can be tested without a clock.
pub fn focused_overflow_text(
    text: &str,
    maximum: usize,
    mode: config::FocusedRowOverflow,
    phase: usize,
) -> String {
    match mode {
        config::FocusedRowOverflow::Truncate => bounded_text(text, maximum),
        config::FocusedRowOverflow::Marquee => marquee_text_at(text, maximum, phase),
        config::FocusedRowOverflow::Manual => manual_text_at(text, maximum, phase),
    }
}

thread_local! {
    static MANUAL_SCROLL_EXTENT: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

/// Render a manually scrolled cell at `offset` characters. At offset zero it
/// matches the truncated text, so the row looks unchanged until scrolled.
/// Records how far the cell can scroll for the Left/Right handler.
pub fn manual_text_at(text: &str, maximum: usize, offset: usize) -> String {
    let characters = text.chars().collect::<Vec<_>>();
    if maximum == 0 || characters.len() <= maximum {
        return bounded_text(text, maximum);
    }
    let extent = characters.len() - maximum;
    MANUAL_SCROLL_EXTENT.with(|recorded| recorded.set(recorded.get().max(extent)));
    let offset = offset.min(extent);
    if offset == 0 {
        return bounded_text(text, maximum);
    }
    characters[offset..offset + maximum].iter().collect()
}

/// Report and clear the largest manual scroll extent drawn since the last
/// call: how many characters the focused row can still scroll.
pub(crate) fn take_manual_scroll_extent() -> usize {
    MANUAL_SCROLL_EXTENT.with(std::cell::Cell::take)
}

/// Text for one cell of a list row: the focused row follows the overflow
/// preference, and every other row truncates.
pub fn focused_row_text(
    text: &str,
    maximum: usize,
    focused: bool,
    mode: config::FocusedRowOverflow,
    phase: usize,
) -> String {
    if focused {
        focused_overflow_text(text, maximum, mode, phase)
    } else {
        bounded_text(text, maximum)
    }
}

/// Render a focused row whose final cell width is not available yet.
///
/// Do not guess here. A guessed width makes a label scroll even when it fits
/// its cell, which is more distracting than letting the widget clip a long
/// label until the caller can provide its measured width.
fn focused_buffered_overflow_text(
    text: &str,
    mode: config::FocusedRowOverflow,
    phase: usize,
) -> String {
    if !mode.scrolls() || text.chars().count() <= MARQUEE_FALLBACK_WIDTH {
        return text.to_owned();
    }
    if mode == config::FocusedRowOverflow::Manual {
        return manual_text_at(text, MARQUEE_FALLBACK_WIDTH, phase);
    }
    // The fallback is only for truly enormous labels. Normal cells wait for
    // a measured-width renderer instead of guessing at their available space.
    marquee_window_at(
        &text.chars().collect::<Vec<_>>(),
        MARQUEE_FALLBACK_WIDTH,
        phase,
    )
}

/// Apply the shared focused-row overflow preference to a table cell.
///
/// Tables project their cells before Ratatui resolves column rectangles, so a
/// table cell uses a deliberately conservative fallback here unless a
/// measured width is supplied by a renderer. This avoids scrolling normal
/// labels that actually fit their column.
pub fn focused_table_text(
    text: String,
    focused: bool,
    mode: config::FocusedRowOverflow,
    phase: usize,
) -> String {
    focused_table_text_at(&text, focused, mode, phase)
}

/// Pure form of [`focused_table_text`] for renderer tests and deterministic
/// projections.
pub fn focused_table_text_at(
    text: &str,
    focused: bool,
    mode: config::FocusedRowOverflow,
    phase: usize,
) -> String {
    if focused && mode.scrolls() {
        focused_buffered_overflow_text(text, mode, phase)
    } else {
        text.to_owned()
    }
}

pub fn marquee_text_at(text: &str, maximum: usize, phase: usize) -> String {
    let characters = text.chars().collect::<Vec<_>>();
    if maximum == 0 {
        return String::new();
    }
    if characters.len() <= maximum {
        return text.to_owned();
    }
    if maximum <= 3 {
        return characters.into_iter().take(maximum).collect();
    }

    marquee_window_at(&characters, maximum, phase)
}

fn marquee_window_at(characters: &[char], maximum: usize, phase: usize) -> String {
    super::frame_schedule::mark_marquee_scrolled();
    let gap = 3;
    let cycle = characters.len() + gap;
    let offset = phase % cycle;
    let required = offset + maximum;
    let mut extended = Vec::with_capacity(required);
    while extended.len() < required {
        extended.extend(characters.iter().copied());
        extended.extend(std::iter::repeat_n(' ', gap));
    }
    extended[offset..offset + maximum].iter().collect()
}

/// Construct and render a block.
///
/// This function should only be used to render a window's borders and its title.
/// It returns the rectangle to render the inner widgets inside the block.
pub fn construct_and_render_block(
    title: &str,
    theme: &config::Theme,
    borders: Borders,
    frame: &mut Frame,
    rect: Rect,
) -> Rect {
    let mut title = bounded_text(title, rect.width.saturating_sub(2) as usize);

    let effective_border = theme.border_type();

    let (borders, border_type) = match effective_border {
        config::BorderType::Hidden | config::BorderType::Plain => (borders, BorderType::Plain),
        config::BorderType::Rounded => (borders, BorderType::Rounded),
        config::BorderType::Double => (borders, BorderType::Double),
        config::BorderType::Thick => (borders, BorderType::Thick),
    };

    let mut block = Block::default()
        .borders(borders)
        .border_style(theme.border())
        .border_type(border_type);

    let inner_rect = block.inner(rect);

    // Handle `BorderType::Hidden` after determining the inner rectangle
    // `Hidden` border can be done by setting the borders to be `NONE`.
    // NOTE: we want to handle the border after the inner rectangle computation,
    // so that paddings between windows are properly determined.
    if *effective_border == config::BorderType::Hidden {
        block = block.borders(Borders::NONE);
        // add padding to the title to ensure the inner text is aligned with the title
        title = format!(" {title}");
    }

    // Set `title` for the block
    block = block.title(Span::styled(title, theme.block_title()));

    frame.render_widget(block, rect);
    inner_rect
}

/// Construct a list while giving the focused-row projection its final label
/// width. Most lists do not know that width until render time; popups do.
pub fn construct_list_widget_with_width<'a>(
    theme: &config::Theme,
    items: Vec<(String, bool)>,
    is_active: bool,
    selected_index: Option<usize>,
    focused_label_width: Option<usize>,
    focused_overflow: config::FocusedRowOverflow,
    focused_phase: usize,
) -> (List<'a>, usize) {
    let total_items = items.len();
    construct_list_widget_with_width_impl(
        theme,
        items,
        is_active,
        selected_index,
        focused_label_width,
        focused_overflow,
        focused_phase,
        None,
        0,
        total_items,
    )
}

/// Construct a list from a viewport slice while retaining global list
/// numbering and selection semantics.
pub(crate) fn construct_list_widget_with_width_and_placeholder_style_viewport<'a>(
    theme: &config::Theme,
    items: Vec<(String, bool)>,
    item_offset: usize,
    total_items: usize,
    is_active: bool,
    selected_index: Option<usize>,
    focused_label_width: Option<usize>,
    focused_overflow: config::FocusedRowOverflow,
    focused_phase: usize,
    is_placeholder: bool,
    placeholder_style: Style,
) -> (List<'a>, usize) {
    construct_list_widget_with_width_impl(
        theme,
        items,
        is_active,
        selected_index,
        focused_label_width,
        focused_overflow,
        focused_phase,
        is_placeholder.then_some(placeholder_style),
        item_offset,
        total_items,
    )
}

fn construct_list_widget_with_width_impl<'a>(
    theme: &config::Theme,
    items: Vec<(String, bool)>,
    is_active: bool,
    selected_index: Option<usize>,
    focused_label_width: Option<usize>,
    focused_overflow: config::FocusedRowOverflow,
    focused_phase: usize,
    placeholder_style: Option<Style>,
    item_offset: usize,
    total_items: usize,
) -> (List<'a>, usize) {
    let configs = config::get_config();
    let n_items = total_items;

    (
        List::new(
            items
                .into_iter()
                .enumerate()
                .map(|(i, (s, is_playing))| {
                    let index = item_offset.saturating_add(i);
                    let s = if is_active
                        && selected_index == Some(index)
                        && focused_overflow.scrolls()
                    {
                        let prefix_width = if focused_label_width.is_some()
                            && configs.app_config.enable_relative_line_number
                        {
                            relative_line_number_prefix_width(n_items)
                        } else {
                            0
                        };
                        match focused_label_width {
                            Some(width) => focused_overflow_text(
                                &s,
                                width.saturating_sub(prefix_width),
                                focused_overflow,
                                focused_phase,
                            ),
                            None => {
                                focused_buffered_overflow_text(&s, focused_overflow, focused_phase)
                            }
                        }
                    } else {
                        s
                    };
                    let text = if is_active && configs.app_config.enable_relative_line_number {
                        if let Some(selected_index) = selected_index {
                            let diff = (index as isize - selected_index as isize).abs();
                            let width = relative_line_number_width(n_items);
                            format!("{diff:>width$}  {s}")
                        } else {
                            s
                        }
                    } else {
                        s
                    };
                    let row_style = if is_playing {
                        theme.current_playing()
                    } else if index == 0 {
                        placeholder_style.unwrap_or_default()
                    } else {
                        Style::default()
                    };
                    ListItem::new(text).style(row_style)
                })
                .collect::<Vec<_>>(),
        )
        .highlight_style(theme.selection(is_active)),
        n_items,
    )
}

fn relative_line_number_width(item_count: usize) -> usize {
    std::cmp::min(item_count.to_string().len(), 2)
}

pub(crate) fn relative_line_number_prefix_width(item_count: usize) -> usize {
    relative_line_number_width(item_count) + 2
}

/// Paint a one-row rule directly into the current frame buffer.
///
/// Workspace chrome frequently used to build a temporary repeated string for
/// a rule on every frame. The rule is a fixed-width glyph projection, so it
/// does not need paragraph layout or an intermediate allocation.
pub(crate) fn render_horizontal_rule(
    frame: &mut Frame,
    rect: Rect,
    symbol: &'static str,
    style: Style,
) {
    if rect.is_empty() {
        return;
    }
    let y = rect.y;
    for x in rect.x..rect.right() {
        if let Some(cell) = frame.buffer_mut().cell_mut((x, y)) {
            cell.set_symbol(symbol).set_style(style);
        }
    }
}

/// Paint a one-cell-wide vertical rule directly into the current frame
/// buffer, avoiding a temporary multiline string and paragraph layout.
pub(crate) fn render_vertical_rule(
    frame: &mut Frame,
    rect: Rect,
    symbol: &'static str,
    style: Style,
) {
    if rect.is_empty() {
        return;
    }
    let x = rect.x;
    for y in rect.y..rect.bottom() {
        if let Some(cell) = frame.buffer_mut().cell_mut((x, y)) {
            cell.set_symbol(symbol).set_style(style);
        }
    }
}

/// Project a table row number using the app's optional Vim-style relative
/// numbering while keeping the focused row's absolute position visible.
pub(crate) fn relative_table_line_number(index: usize, selected_index: Option<usize>) -> String {
    match selected_index {
        Some(selected) if index != selected => {
            (index as isize - selected as isize).abs().to_string()
        }
        _ => (index + 1).to_string(),
    }
}

pub fn render_list_window(
    frame: &mut Frame,
    widget: List,
    rect: Rect,
    len: usize,
    state: &mut ListState,
) {
    render_list_window_with_scrollbar(frame, widget, rect, len, state);
}

/// Render a list that was constructed from the current viewport while
/// preserving the page state's global selection and offset.
pub(crate) fn render_prepared_list_window(
    frame: &mut Frame,
    widget: List,
    rect: Rect,
    len: usize,
    state: &mut ListState,
) {
    let (content_rect, scrollbar_rect) = vertical_scrollbar_regions(rect);
    let viewport_rows = content_rect.height as usize;
    let range = prepare_list_viewport_with_scrollbar(rect, len, state);
    let mut viewport_state = ListState::default();
    viewport_state.select(
        state
            .selected()
            .and_then(|selected| selected.checked_sub(range.start))
            .filter(|selected| *selected < viewport_rows),
    );
    frame.render_stateful_widget(widget, content_rect, &mut viewport_state);
    render_vertical_scrollbar(frame, scrollbar_rect, len, viewport_rows, state.offset());
}

pub fn render_table_window(
    frame: &mut Frame,
    widget: Table,
    rect: Rect,
    len: usize,
    state: &mut TableState,
) {
    render_table_window_with_scrollbar(frame, widget, rect, len, state);
}

fn render_vertical_scrollbar(
    frame: &mut Frame,
    rect: Rect,
    content_length: usize,
    viewport_content_length: usize,
    position: usize,
) {
    if rect.is_empty() || content_length <= viewport_content_length {
        return;
    }

    let scrollbar = Scrollbar::new(ScrollbarOrientation::VerticalRight)
        .begin_symbol(None)
        .end_symbol(None);
    let mut scrollbar_state = ScrollbarState::new(content_length)
        .position(position)
        .viewport_content_length(viewport_content_length);
    frame.render_stateful_widget(scrollbar, rect, &mut scrollbar_state);
}

fn render_vertical_scrollbar_styled(
    frame: &mut Frame,
    rect: Rect,
    content_length: usize,
    viewport_content_length: usize,
    position: usize,
    track_style: Style,
    thumb_style: Style,
) {
    if rect.is_empty() || content_length <= viewport_content_length {
        return;
    }

    // The design-v1 recipe uses a floor-based thumb calculation. Ratatui's
    // ScrollbarState intentionally uses a different rounded model, so draw
    // this one-cell rail directly to keep the reference contract exact.
    let rail = rect.height as usize;
    let thumb = (rail
        .saturating_mul(viewport_content_length)
        .checked_div(content_length)
        .unwrap_or(0))
    .max(1)
    .min(rail);
    let max_offset = content_length.saturating_sub(viewport_content_length);
    let travel = rail.saturating_sub(thumb);
    let thumb_offset = if max_offset == 0 {
        0
    } else {
        travel
            .saturating_mul(position.min(max_offset))
            .checked_div(max_offset)
            .unwrap_or(0)
    };
    let x = rect.x;
    for (row, y) in (rect.y..rect.bottom()).enumerate() {
        let is_thumb = row >= thumb_offset && row < thumb_offset.saturating_add(thumb);
        if let Some(cell) = frame.buffer_mut().cell_mut((x, y)) {
            cell.set_symbol(if is_thumb { "┃" } else { "│" })
                .set_style(if is_thumb { thumb_style } else { track_style });
        }
    }
}

/// Render a list with a stable one-column gutter and a passive overflow indicator.
pub fn render_list_window_with_scrollbar(
    frame: &mut Frame,
    widget: List,
    rect: Rect,
    len: usize,
    state: &mut ListState,
) {
    let (content_rect, scrollbar_rect) = vertical_scrollbar_regions(rect);
    let viewport_rows = content_rect.height as usize;
    adjust_list_offset(state, len, content_rect.height);
    frame.render_stateful_widget(widget, content_rect, state);
    render_vertical_scrollbar(frame, scrollbar_rect, len, viewport_rows, state.offset());
}

/// Render a prepared list with the workspace design-v1 scrollbar treatment.
pub(crate) fn render_prepared_list_window_with_styled_scrollbar(
    frame: &mut Frame,
    widget: List,
    rect: Rect,
    len: usize,
    state: &mut ListState,
    track_style: Style,
    thumb_style: Style,
) {
    let (content_rect, scrollbar_rect) = vertical_scrollbar_regions(rect);
    let viewport_rows = content_rect.height as usize;
    adjust_list_offset(state, len, content_rect.height);
    let start = state.offset();
    let mut viewport_state = ListState::default();
    viewport_state.select(
        state
            .selected()
            .and_then(|selected| selected.checked_sub(start))
            .filter(|selected| *selected < viewport_rows),
    );
    frame.render_stateful_widget(widget, content_rect, &mut viewport_state);
    render_vertical_scrollbar_styled(
        frame,
        scrollbar_rect,
        len,
        viewport_rows,
        state.offset(),
        track_style,
        thumb_style,
    );
}

/// Render a table with a stable gutter; its header is excluded from the scrollbar track.
pub fn render_table_window_with_scrollbar(
    frame: &mut Frame,
    widget: Table,
    rect: Rect,
    len: usize,
    state: &mut TableState,
) {
    let (content_rect, mut scrollbar_rect) = vertical_scrollbar_regions(rect);
    let viewport_rows = content_rect.height.saturating_sub(1);
    adjust_table_offset(state, len, viewport_rows);
    frame.render_stateful_widget(widget, content_rect, state);

    scrollbar_rect.y = scrollbar_rect.y.saturating_add(1);
    scrollbar_rect.height = scrollbar_rect.height.saturating_sub(1);
    render_vertical_scrollbar(
        frame,
        scrollbar_rect,
        len,
        viewport_rows as usize,
        state.offset(),
    );
}

/// Render rows already sliced to the range returned by
/// [`prepare_table_viewport_with_scrollbar`] while retaining global page state.
pub(crate) fn render_prepared_table_viewport_with_scrollbar(
    frame: &mut Frame,
    widget: Table,
    rect: Rect,
    len: usize,
    state: &mut TableState,
) {
    let (content_rect, mut scrollbar_rect) = vertical_scrollbar_regions(rect);
    let viewport_rows = content_rect.height.saturating_sub(1);
    adjust_table_offset(state, len, viewport_rows);

    let start = state.offset();
    let mut viewport_state = TableState::default();
    viewport_state.select(
        state
            .selected()
            .and_then(|selected| selected.checked_sub(start))
            .filter(|selected| *selected < viewport_rows as usize),
    );
    frame.render_stateful_widget(widget, content_rect, &mut viewport_state);

    scrollbar_rect.y = scrollbar_rect.y.saturating_add(1);
    scrollbar_rect.height = scrollbar_rect.height.saturating_sub(1);
    render_vertical_scrollbar(
        frame,
        scrollbar_rect,
        len,
        viewport_rows as usize,
        state.offset(),
    );
}

#[cfg(test)]
mod tests {
    use super::{
        adjust_list_offset, adjust_multiline_list_offset, adjust_table_offset, bounded_text,
        construct_list_widget_with_width_and_placeholder_style_viewport, focused_overflow_text,
        focused_row_text, focused_table_text, focused_table_text_at, manual_text_at,
        marquee_text_at, prepare_table_viewport_with_scrollbar, relative_line_number_prefix_width,
        relative_table_line_number, render_horizontal_rule, render_list_window_with_scrollbar,
        render_prepared_list_window, render_prepared_list_window_with_styled_scrollbar,
        render_prepared_table_viewport_with_scrollbar, render_table_window_with_scrollbar,
        render_vertical_rule, take_manual_scroll_extent, vertical_scrollbar_regions,
    };
    use crate::config::{FocusedRowOverflow, Theme};
    use crate::ui::components::list::adjusted_selection;
    use ratatui::{
        backend::TestBackend,
        layout::{Constraint, Rect},
        style::{Color, Style},
        widgets::{List, ListItem, ListState, Row, Table, TableState},
        Terminal,
    };

    #[test]
    fn selection_survives_refresh_and_resize_bounds() {
        assert_eq!(adjusted_selection(Some(4), 10), Some(4));
        assert_eq!(adjusted_selection(Some(9), 5), Some(4));
        assert_eq!(adjusted_selection(Some(0), 0), Some(0));
        assert_eq!(adjusted_selection(None, 3), Some(0));
        assert_eq!(adjusted_selection(None, 0), None);
    }

    #[test]
    fn bounded_text_keeps_long_labels_inside_their_cell() {
        assert_eq!(bounded_text("short", 8), "short");
        assert_eq!(bounded_text("abcdefgh", 5), "ab...");
        assert_eq!(bounded_text("abcdefgh", 3), "abc");
    }

    #[test]
    fn popup_block_claims_inner_cells_before_children_render() {
        crate::ui::initialize_test_config();
        let theme = Theme::default();
        let popup_background = theme.workspace_elevated_surface().bg;
        let mut terminal = Terminal::new(TestBackend::new(12, 5)).unwrap();

        terminal
            .draw(|frame| {
                frame.render_widget(
                    super::Block::default().style(Style::default().bg(Color::Blue)),
                    frame.area(),
                );
                frame.render_widget(
                    ratatui::widgets::Paragraph::new(
                        "XXXXXXXXXXXX\nXXXXXXXXXXXX\nXXXXXXXXXXXX\nXXXXXXXXXXXX\nXXXXXXXXXXXX",
                    ),
                    frame.area(),
                );
                crate::ui::components::popup_surface::render(
                    "Popup",
                    &theme,
                    super::Borders::ALL,
                    frame,
                    Rect::new(2, 1, 8, 3),
                );
            })
            .unwrap();

        let buffer = terminal.backend().buffer();
        assert_eq!(buffer[(3, 2)].bg, popup_background.unwrap());
        assert_ne!(buffer[(3, 2)].bg, Color::Blue);
        for x in 3..9 {
            assert_eq!(buffer[(x, 2)].symbol(), " ");
        }
        assert_eq!(buffer[(1, 2)].symbol(), "X");
    }

    #[test]
    fn direct_rules_paint_only_their_declared_cells() {
        let mut terminal = Terminal::new(TestBackend::new(8, 5)).unwrap();
        let rule_style = Style::default().fg(Color::Yellow);

        terminal
            .draw(|frame| {
                render_horizontal_rule(frame, Rect::new(1, 2, 4, 2), "─", rule_style);
                render_vertical_rule(frame, Rect::new(5, 1, 1, 3), "│", rule_style);
            })
            .unwrap();

        let buffer = terminal.backend().buffer();
        for x in 1..5 {
            assert_eq!(buffer[(x, 2)].symbol(), "─");
            assert_eq!(buffer[(x, 2)].fg, Color::Yellow);
            assert_eq!(buffer[(x, 3)].symbol(), " ");
        }
        for y in 1..4 {
            assert_eq!(buffer[(5, y)].symbol(), "│");
            assert_eq!(buffer[(5, y)].fg, Color::Yellow);
        }
        assert_eq!(buffer[(0, 2)].symbol(), " ");
        assert_eq!(buffer[(4, 1)].symbol(), " ");
    }

    #[test]
    fn relative_table_line_number_keeps_the_focused_row_absolute() {
        assert_eq!(relative_table_line_number(0, Some(3)), "3");
        assert_eq!(relative_table_line_number(3, Some(3)), "4");
        assert_eq!(relative_table_line_number(5, Some(3)), "2");
        assert_eq!(relative_table_line_number(5, None), "6");
    }

    #[test]
    fn focused_overflow_can_scroll_without_changing_cell_width() {
        assert_eq!(
            focused_overflow_text("abcdefgh", 5, FocusedRowOverflow::Truncate, 0),
            "ab..."
        );
        assert_eq!(marquee_text_at("abcdefgh", 5, 0), "abcde");
        assert_eq!(marquee_text_at("abcdefgh", 5, 4), "efgh ");
        assert_eq!(marquee_text_at("abcdefgh", 5, 8), "   ab");
        assert_eq!(
            focused_overflow_text("abcdefgh", 5, FocusedRowOverflow::Marquee, 2),
            "cdefg"
        );
    }

    #[test]
    fn manual_cells_start_truncated_and_scroll_within_bounds() {
        let text = "abcdefghij";
        let _ = take_manual_scroll_extent();

        assert_eq!(manual_text_at(text, 6, 0), bounded_text(text, 6));
        assert_eq!(manual_text_at(text, 6, 2), "cdefgh");
        assert_eq!(manual_text_at(text, 6, 99), "efghij");
        assert_eq!(take_manual_scroll_extent(), 4);

        assert_eq!(manual_text_at("short", 6, 3), "short");
        assert_eq!(take_manual_scroll_extent(), 0, "fitting text cannot scroll");
        assert_eq!(
            focused_overflow_text(text, 6, FocusedRowOverflow::Manual, 2),
            "cdefgh"
        );
    }

    #[test]
    fn only_the_focused_row_follows_the_overflow_preference() {
        let marquee = crate::config::FocusedRowOverflow::Marquee;
        let truncate = crate::config::FocusedRowOverflow::Truncate;

        assert_eq!(focused_row_text("abcdefgh", 5, true, marquee, 4), "efgh ");
        assert_eq!(focused_row_text("abcdefgh", 5, false, marquee, 4), "ab...");
        assert_eq!(focused_row_text("abcdefgh", 5, true, truncate, 4), "ab...");
        assert_eq!(focused_row_text("abc", 5, true, marquee, 4), "abc");
    }

    #[test]
    fn known_width_marquee_only_scrolls_when_text_overflows() {
        assert_eq!(
            marquee_text_at("seventeen-char!!", 20, 7),
            "seventeen-char!!"
        );
        assert_eq!(marquee_text_at("abcdefgh", 0, 0), "");
    }

    #[test]
    fn focused_table_overflow_waits_for_a_measured_cell_width() {
        let text = "x".repeat(40);
        assert_eq!(
            focused_table_text_at(&text, false, FocusedRowOverflow::Marquee, 8),
            text
        );
        assert_eq!(
            focused_table_text_at(&text, true, FocusedRowOverflow::Marquee, 8),
            text
        );
    }

    #[test]
    fn focused_table_marquee_waits_for_a_measured_cell_width() {
        let text = "A long table label";
        assert_eq!(
            focused_table_text_at(text, true, FocusedRowOverflow::Marquee, 9),
            text
        );
    }

    #[test]
    fn focused_table_cells_consume_the_same_caller_owned_phase() {
        let title = format!("title-{}", "x".repeat(260));
        let artist = format!("artist-{}", "y".repeat(260));
        let phase = 7;

        assert_eq!(
            focused_table_text(title.clone(), true, FocusedRowOverflow::Marquee, phase),
            marquee_text_at(&title, super::MARQUEE_FALLBACK_WIDTH, phase)
        );
        assert_eq!(
            focused_table_text(artist.clone(), true, FocusedRowOverflow::Marquee, phase),
            marquee_text_at(&artist, super::MARQUEE_FALLBACK_WIDTH, phase)
        );
    }

    #[test]
    fn list_viewport_scrolls_only_when_selection_leaves_the_window() {
        let mut state = ListState::default();
        state.select(Some(4));
        *state.offset_mut() = 0;
        adjust_list_offset(&mut state, 20, 5);
        assert_eq!(state.selected(), Some(4));
        assert_eq!(state.offset(), 0);

        state.select(Some(3));
        adjust_list_offset(&mut state, 20, 5);
        assert_eq!(state.offset(), 0);

        state.select(Some(9));
        adjust_list_offset(&mut state, 20, 5);
        assert_eq!(state.offset(), 5);

        state.select(Some(2));
        adjust_list_offset(&mut state, 20, 5);
        assert_eq!(state.offset(), 2);
    }

    #[test]
    fn table_viewport_uses_the_same_edge_aware_rule() {
        let mut state = TableState::default();
        state.select(Some(8));
        adjust_table_offset(&mut state, 20, 5);
        assert_eq!(state.offset(), 4);

        state.select(Some(7));
        adjust_table_offset(&mut state, 20, 5);
        assert_eq!(state.offset(), 4);

        state.select(Some(3));
        adjust_table_offset(&mut state, 20, 5);
        assert_eq!(state.offset(), 3);
    }

    #[test]
    fn scrollbar_gutter_is_stable_and_preserves_single_column_viewports() {
        let rect = Rect::new(2, 3, 8, 5);
        let (content, gutter) = vertical_scrollbar_regions(rect);
        assert_eq!(content, Rect::new(2, 3, 7, 5));
        assert_eq!(gutter, Rect::new(9, 3, 1, 5));

        let narrow = Rect::new(2, 3, 1, 5);
        assert_eq!(
            vertical_scrollbar_regions(narrow),
            (narrow, Rect::default())
        );
    }

    #[test]
    fn list_scrollbar_renders_only_when_content_overflows() {
        let mut terminal = Terminal::new(TestBackend::new(5, 4)).unwrap();
        let mut state = ListState::default();
        state.select(Some(7));
        terminal
            .draw(|frame| {
                let items = (0..8)
                    .map(|index| ListItem::new(format!("row {index}")))
                    .collect::<Vec<_>>();
                render_list_window_with_scrollbar(
                    frame,
                    List::new(items),
                    frame.area(),
                    8,
                    &mut state,
                );
            })
            .unwrap();
        assert!((0..4).any(|y| terminal.backend().buffer()[(4, y)].symbol() != " "));

        let mut short_terminal = Terminal::new(TestBackend::new(5, 4)).unwrap();
        let mut short_state = ListState::default();
        short_terminal
            .draw(|frame| {
                render_list_window_with_scrollbar(
                    frame,
                    List::new([ListItem::new("one"), ListItem::new("two")]),
                    frame.area(),
                    2,
                    &mut short_state,
                );
            })
            .unwrap();
        assert!((0..4).all(|y| short_terminal.backend().buffer()[(4, y)].symbol() == " "));
    }

    #[test]
    fn shared_plain_and_prepared_lists_use_the_same_scrollbar_gutter() {
        let rect = Rect::new(0, 0, 8, 4);
        let items = (0..8)
            .map(|index| ListItem::new(format!("row {index}")))
            .collect::<Vec<_>>();

        let mut plain_terminal = Terminal::new(TestBackend::new(8, 4)).unwrap();
        let mut plain_state = ListState::default();
        plain_terminal
            .draw(|frame| {
                super::render_list_window(
                    frame,
                    List::new(items.clone()),
                    rect,
                    items.len(),
                    &mut plain_state,
                );
            })
            .unwrap();

        let mut prepared_terminal = Terminal::new(TestBackend::new(8, 4)).unwrap();
        let mut prepared_state = ListState::default();
        prepared_terminal
            .draw(|frame| {
                render_prepared_list_window(
                    frame,
                    List::new(items.clone()),
                    rect,
                    items.len(),
                    &mut prepared_state,
                );
            })
            .unwrap();

        assert!((0..4).any(|y| { plain_terminal.backend().buffer()[(7, y)].symbol() != " " }));
        assert!((0..4).any(|y| { prepared_terminal.backend().buffer()[(7, y)].symbol() != " " }));
    }

    #[test]
    fn table_scrollbar_keeps_the_header_row_clear() {
        let mut terminal = Terminal::new(TestBackend::new(8, 4)).unwrap();
        let mut state = TableState::default();
        state.select(Some(7));
        terminal
            .draw(|frame| {
                let rows = (0..8)
                    .map(|index| Row::new([format!("row {index}")]))
                    .collect::<Vec<_>>();
                let table = Table::new(rows, [Constraint::Fill(1)]).header(Row::new(["title"]));
                render_table_window_with_scrollbar(frame, table, frame.area(), 8, &mut state);
            })
            .unwrap();

        let buffer = terminal.backend().buffer();
        assert_eq!(buffer[(7, 0)].symbol(), " ");
        assert!((1..4).any(|y| buffer[(7, y)].symbol() != " "));
    }

    #[test]
    fn prepared_table_viewport_keeps_global_selection_and_bounds_projected_rows() {
        let rect = Rect::new(0, 0, 8, 4);
        let mut state = TableState::default();
        state.select(Some(999));

        let range = prepare_table_viewport_with_scrollbar(rect, 1_000, &mut state);
        assert_eq!(range, 997..1_000);
        assert_eq!(state.selected(), Some(999));
        assert_eq!(state.offset(), 997);

        let mut terminal = Terminal::new(TestBackend::new(8, 4)).unwrap();
        terminal
            .draw(|frame| {
                let rows = range
                    .clone()
                    .map(|index| Row::new([format!("row {index}")]))
                    .collect::<Vec<_>>();
                assert_eq!(rows.len(), 3);
                let table = Table::new(rows, [Constraint::Fill(1)]).header(Row::new(["title"]));
                render_prepared_table_viewport_with_scrollbar(
                    frame,
                    table,
                    frame.area(),
                    1_000,
                    &mut state,
                );
            })
            .unwrap();

        assert_eq!(state.selected(), Some(999));
        assert_eq!(state.offset(), 997);
        let rendered = (0..4)
            .map(|y| {
                (0..7)
                    .map(|x| terminal.backend().buffer()[(x, y)].symbol())
                    .collect::<String>()
            })
            .collect::<Vec<_>>()
            .join("\n");
        assert!(rendered.contains("row 999"));
        assert!(!rendered.contains("row 996"));
    }

    #[test]
    fn prepared_list_viewport_keeps_global_selection_and_bounds_projected_rows() {
        crate::ui::initialize_test_config();
        let rect = Rect::new(0, 0, 12, 3);
        let mut state = ListState::default();
        state.select(Some(999));
        *state.offset_mut() = 997;

        let (list, count) = construct_list_widget_with_width_and_placeholder_style_viewport(
            &Theme::default(),
            (997..1_000)
                .map(|index| (format!("row {index}"), false))
                .collect(),
            997,
            1_000,
            true,
            Some(999),
            Some(12),
            FocusedRowOverflow::Truncate,
            0,
            false,
            Style::default(),
        );
        assert_eq!(count, 1_000);

        let mut terminal = Terminal::new(TestBackend::new(12, 3)).unwrap();
        terminal
            .draw(|frame| render_prepared_list_window(frame, list, rect, count, &mut state))
            .unwrap();

        assert_eq!(state.selected(), Some(999));
        assert_eq!(state.offset(), 997);
        let rendered = (0..3)
            .map(|y| {
                (0..12)
                    .map(|x| terminal.backend().buffer()[(x, y)].symbol())
                    .collect::<String>()
            })
            .collect::<Vec<_>>()
            .join("\n");
        assert!(rendered.contains("row 999"));
        assert!(!rendered.contains("row 996"));
    }

    #[test]
    fn prepared_list_scrollbar_uses_the_full_global_row_count() {
        let rect = Rect::new(0, 0, 8, 3);
        let mut state = ListState::default();
        state.select(Some(999));
        *state.offset_mut() = 997;
        let list = List::new(
            (997..1_000)
                .map(|index| ListItem::new(format!("row {index}")))
                .collect::<Vec<_>>(),
        );

        let mut terminal = Terminal::new(TestBackend::new(8, 3)).unwrap();
        terminal
            .draw(|frame| {
                render_prepared_list_window_with_styled_scrollbar(
                    frame,
                    list,
                    rect,
                    1_000,
                    &mut state,
                    Style::default(),
                    Style::default(),
                )
            })
            .unwrap();

        assert_eq!(state.selected(), Some(999));
        assert_eq!(state.offset(), 997);
        assert!((0..3).any(|y| terminal.backend().buffer()[(7, y)].symbol() != " "));
    }

    #[test]
    fn multiline_viewport_accounts_for_section_header_lines() {
        let mut state = ListState::default();
        state.select(Some(3));
        adjust_multiline_list_offset(&mut state, &[2, 1, 1, 1], 4);
        assert_eq!(state.offset(), 1);

        state.select(Some(1));
        adjust_multiline_list_offset(&mut state, &[2, 1, 1, 1], 4);
        assert_eq!(state.offset(), 1);
    }

    #[test]
    fn relative_line_number_prefix_width_includes_separator() {
        assert_eq!(relative_line_number_prefix_width(9), 3);
        assert_eq!(relative_line_number_prefix_width(100), 4);
    }
}

/// Convert a string to a bidirectional string.
/// Used to handle RTL text properly in the UI.
pub fn to_bidi_string(s: &str) -> String {
    let bidi_info = BidiInfo::new(s, None);

    let bidi_string = if bidi_info.has_rtl() && !bidi_info.paragraphs.is_empty() {
        bidi_info
            .reorder_line(&bidi_info.paragraphs[0], 0..s.len())
            .into_owned()
    } else {
        s.to_string()
    };

    bidi_string
}

/// formats genres depending on the number of genres and `genre_num`
///
/// Examples for `genre_num = 2`
/// - 1 genre: "genre1"
/// - 2 genres: "genre1, genre2"
/// - \>= 3 genres: "genre1, genre2, ..."
pub fn format_genres(genres: &[String], genre_num: u8) -> String {
    let mut genre_str = String::with_capacity(64);

    if genre_num > 0 {
        for i in 0..genres.len() {
            genre_str.push_str(&genres[i]);

            if i + 1 != genres.len() {
                genre_str.push_str(", ");

                if i + 1 >= genre_num as usize {
                    genre_str.push_str("...");
                    break;
                }
            }
        }
    }

    genre_str
}
