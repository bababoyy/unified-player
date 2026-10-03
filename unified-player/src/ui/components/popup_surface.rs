//! Shared popup surface ownership.

use super::super::{config, utils};
use ratatui::{layout::Rect, widgets::Borders, Frame};

/// Paint the complete elevated popup surface before its children.
///
/// The border and title remain delegated to the existing block primitive so
/// border-type configuration stays centralized. This component owns the
/// background claim, which prevents transparent child cells from exposing a
/// page or a previous frame.
pub(crate) fn render(
    title: &str,
    theme: &config::Theme,
    borders: Borders,
    frame: &mut Frame,
    rect: Rect,
) -> Rect {
    frame.render_widget(ratatui::widgets::Clear, rect);
    frame.render_widget(
        ratatui::widgets::Block::default().style(theme.workspace_elevated_surface()),
        rect,
    );
    let title = if title.is_empty() {
        String::new()
    } else {
        format!(" {title} ")
    };
    utils::construct_and_render_block(&title, theme, borders, frame, rect)
}
