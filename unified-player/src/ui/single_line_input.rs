use crossterm::event::KeyCode;

use super::{Line, Modifier, Paragraph, Span, Style};
use crate::key::Key;

#[derive(Debug, Clone, PartialEq)]
pub struct LineInput {
    // This is less space-efficient than String, but it's easier to work with text manipulation at the
    // cursor. Otherwise, you have to shuffle back and forth between String and String::chars().
    line: Vec<char>,
    cursor: usize,
}

pub enum InputEffect {
    TextChanged,
    CursorMoved,
    // Sometimes a given input has no effect, but it is should still be considered as 'consumed' by
    // the input element. For instance, pressing backspace when there is no text.
    Ack,
}

impl Default for LineInput {
    fn default() -> Self {
        Self::new(Vec::new())
    }
}

impl LineInput {
    /// Place the caret using a terminal cell column, including wide characters.
    pub fn set_cursor_column(&mut self, column: u16) {
        let mut cells = 0;
        self.cursor = self.line.len();
        for (index, character) in self.line.iter().enumerate() {
            let width = Span::raw(character.to_string()).width();
            if cells + width > usize::from(column) {
                self.cursor = index;
                break;
            }
            cells += width;
        }
    }
    pub fn new(str: Vec<char>) -> Self {
        let cursor = str.len();
        Self { line: str, cursor }
    }

    pub fn input(&mut self, key: &Key) -> Option<InputEffect> {
        match key {
            Key::None(c) => match c {
                KeyCode::Char(c) => {
                    if self.cursor == self.line.len() {
                        self.line.push(*c);
                    } else {
                        self.line.insert(self.cursor, *c);
                    }
                    self.cursor += 1;
                    Some(InputEffect::TextChanged)
                }
                KeyCode::Backspace => {
                    if self.line.is_empty() || self.cursor == 0 {
                        Some(InputEffect::Ack)
                    } else {
                        // Perform the decrement first.
                        self.cursor -= 1;
                        self.line.remove(self.cursor);
                        Some(InputEffect::TextChanged)
                    }
                }
                KeyCode::Left => {
                    if self.cursor == 0 {
                        Some(InputEffect::Ack)
                    } else {
                        self.cursor -= 1;
                        Some(InputEffect::CursorMoved)
                    }
                }
                KeyCode::Right => {
                    if self.cursor == self.line.len() {
                        Some(InputEffect::Ack)
                    } else {
                        self.cursor += 1;
                        Some(InputEffect::CursorMoved)
                    }
                }
                _ => None,
            },
            _ => None,
        }
    }

    pub fn widget(&self, is_active: bool) -> Paragraph<'static> {
        if !is_active {
            let converted_str: String = self.line.iter().collect();
            return Paragraph::new(converted_str);
        }

        let before_cursor: String = self.line[0..self.cursor].iter().collect();
        let after_cursor: String = if self.cursor == self.line.len() {
            String::new()
        } else {
            self.line[self.cursor + 1..].iter().collect()
        };
        let cursor = if self.cursor == self.line.len() {
            " ".to_string()
        } else {
            self.line[self.cursor].to_string()
        };

        let text_style = Style::default();
        let cursor_style = Style::default().add_modifier(Modifier::REVERSED);
        let formatted_line = Line::from(vec![
            Span::styled(before_cursor, text_style),
            Span::styled(cursor, cursor_style),
            Span::styled(after_cursor, text_style),
        ]);

        Paragraph::new(formatted_line)
    }

    /// Render the input with caller-owned semantic text and caret styles.
    ///
    /// The ordinary input widget intentionally keeps the legacy reversed
    /// caret. Workspace screens need the same editing state with the v1
    /// palette's explicit inverse foreground/background pair, so the layout
    /// owns the styles while this type continues to own the cursor position.
    pub fn widget_with_styles(
        &self,
        is_active: bool,
        text_style: Style,
        cursor_style: Style,
        placeholder: &str,
        placeholder_style: Style,
    ) -> Paragraph<'static> {
        if self.is_empty() {
            let mut spans = Vec::with_capacity(2);
            if is_active {
                spans.push(Span::styled(" ", cursor_style));
            }
            spans.push(Span::styled(placeholder.to_owned(), placeholder_style));
            return Paragraph::new(Line::from(spans));
        }

        if !is_active {
            return Paragraph::new(Line::from(Span::styled(
                self.line.iter().collect::<String>(),
                text_style,
            )));
        }

        let before_cursor: String = self.line[0..self.cursor].iter().collect();
        let after_cursor: String = if self.cursor == self.line.len() {
            String::new()
        } else {
            self.line[self.cursor + 1..].iter().collect()
        };
        let cursor = if self.cursor == self.line.len() {
            " ".to_owned()
        } else {
            self.line[self.cursor].to_string()
        };
        Paragraph::new(Line::from(vec![
            Span::styled(before_cursor, text_style),
            Span::styled(cursor, cursor_style),
            Span::styled(after_cursor, text_style),
        ]))
    }

    pub fn is_empty(&self) -> bool {
        self.line.is_empty()
    }

    pub fn get_text(&self) -> String {
        self.line.iter().collect()
    }
}

#[cfg(test)]
mod pointer_tests {
    use super::*;
    use ratatui::{backend::TestBackend, layout::Rect, style::Color, Terminal};

    #[test]
    fn welcome_input_click_respects_wide_characters_and_end_of_text() {
        let mut input = LineInput::new("a界b".chars().collect());
        input.set_cursor_column(2);
        input.input(&Key::None(KeyCode::Char('x')));
        assert_eq!(input.get_text(), "ax界b");
        input.set_cursor_column(100);
        input.input(&Key::None(KeyCode::Char('z')));
        assert_eq!(input.get_text(), "ax界bz");
        input.set_cursor_column(0);
        input.input(&Key::None(KeyCode::Char('0')));
        assert_eq!(input.get_text(), "0ax界bz");
    }

    #[test]
    fn styled_widget_uses_the_workspace_caret_without_legacy_reverse_modifier() {
        let mut input = LineInput::new("ab".chars().collect());
        input.set_cursor_column(1);
        let text_style = Style::default().fg(Color::White).bg(Color::Black);
        let cursor_style = Style::default().fg(Color::Black).bg(Color::White);
        let mut terminal = Terminal::new(TestBackend::new(4, 1)).unwrap();
        terminal
            .draw(|frame| {
                frame.render_widget(
                    input.widget_with_styles(
                        true,
                        text_style,
                        cursor_style,
                        "placeholder",
                        Style::default(),
                    ),
                    Rect::new(0, 0, 4, 1),
                );
            })
            .unwrap();

        let caret = &terminal.backend().buffer()[(1, 0)];
        assert_eq!(caret.symbol(), "b");
        assert_eq!(caret.fg, Color::Black);
        assert_eq!(caret.bg, Color::White);
        assert!(!caret.modifier.contains(Modifier::REVERSED));
    }
}

/// Text editing with a redacted debug representation and masked rendering.
#[derive(Clone, Default, PartialEq)]
pub struct SecretInput(LineInput);
impl std::fmt::Debug for SecretInput {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("SecretInput([REDACTED])")
    }
}
impl SecretInput {
    pub fn input(&mut self, key: &Key) -> Option<InputEffect> {
        self.0.input(key)
    }
    pub fn get_text(&self) -> String {
        self.0.get_text()
    }
    pub fn set_cursor_column(&mut self, column: u16) {
        self.0.cursor = usize::from(column).min(self.0.line.len());
    }
    pub fn widget(&self) -> Paragraph<'static> {
        LineInput {
            line: vec!['*'; self.0.line.len()],
            cursor: self.0.cursor,
        }
        .widget(true)
    }
}
