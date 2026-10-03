//! Drawing shared by pages built from shelves of cards (Home, Settings).

use ratatui::{layout::Rect, widgets::Block, Frame};

use crate::config;
use crate::state::UIStateGuard;
use crate::ui::utils;

use super::workspace_text;

/// Horizontal space between cards.
pub(super) const CARD_GAP: u16 = 2;

/// Width of each of `columns` cards filling `width`.
pub(super) fn grid_card_width(width: u16, columns: usize) -> u16 {
    let columns = u16::try_from(columns).unwrap_or(1).max(1);
    width.saturating_sub(CARD_GAP * (columns - 1)) / columns
}

/// Columns of at least `min_width` that fit `width`, at most `max_columns`.
pub(super) fn grid_columns(width: u16, min_width: u16, max_columns: u16) -> usize {
    usize::from(
        (width.saturating_add(CARD_GAP) / (min_width + CARD_GAP)).clamp(1, max_columns.max(1)),
    )
}

/// A shelf's title, with the selected card's position when it has focus.
pub(super) fn render_shelf_heading(
    frame: &mut Frame,
    ui: &UIStateGuard,
    area: Rect,
    title: &str,
    position: Option<(usize, usize)>,
) {
    let style = if position.is_some() {
        ui.theme.workspace_focus_indicator()
    } else {
        ui.theme.workspace_heading()
    };
    workspace_text(
        frame,
        Rect::new(area.x, area.y, area.width, 1),
        title,
        style,
    );
    if let Some((index, len)) = position {
        let position = format!("{}/{len}", index + 1);
        let width = u16::try_from(position.chars().count()).unwrap_or(area.width);
        workspace_text(
            frame,
            Rect::new(
                area.right().saturating_sub(width),
                area.y,
                width.min(area.width),
                1,
            ),
            position,
            ui.theme.workspace_secondary_text(),
        );
    }
}

/// A card with a title line and, when `rect` has room, a subtitle line.
/// `selection` is the selection style when the card is selected.
pub(super) fn render_card(
    frame: &mut Frame,
    ui: &UIStateGuard,
    rect: Rect,
    title: &str,
    subtitle: &str,
    selection: Option<ratatui::style::Style>,
) {
    let heading = ui.theme.workspace_heading();
    let (surface, title_style, secondary) = if let Some(style) = selection {
        (style, config::Theme::on_selection(style, heading), style)
    } else {
        let surface = ui.theme.workspace_elevated_surface();
        (
            surface,
            surface.patch(heading),
            surface.patch(ui.theme.workspace_secondary_text()),
        )
    };
    frame.render_widget(Block::default().style(surface), rect);
    let text_width = rect.width.saturating_sub(2);
    workspace_text(
        frame,
        Rect::new(rect.x + 1, rect.y, text_width, 1),
        utils::bounded_text(title, usize::from(text_width)),
        title_style,
    );
    if rect.height > 1 {
        workspace_text(
            frame,
            Rect::new(rect.x + 1, rect.y + 1, text_width, 1),
            utils::bounded_text(subtitle, usize::from(text_width)),
            secondary,
        );
    }
}
