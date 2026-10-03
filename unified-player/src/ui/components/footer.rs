//! Workspace footer hint line.

use super::super::utils;
use ratatui::{style::Style, text::Span};

const HINT_SEPARATOR: &str = "   ";

/// A single `key label` hint.
#[derive(Debug, Clone)]
pub(crate) struct FooterHint {
    key: String,
    label: &'static str,
    /// Optional hints are dropped, in order, when the line is too narrow to
    /// also show the help hint.
    optional: bool,
}

impl FooterHint {
    pub(crate) fn new(key: impl Into<String>, label: &'static str) -> Self {
        Self {
            key: key.into(),
            label,
            optional: false,
        }
    }

    pub(crate) fn optional(key: impl Into<String>, label: &'static str) -> Self {
        Self {
            optional: true,
            ..Self::new(key, label)
        }
    }

    fn width(&self) -> usize {
        self.key.chars().count() + 1 + self.label.chars().count()
    }
}

/// A laid-out footer line.
///
/// Key positions are recorded while the text is built, so styling and hit
/// regions never depend on searching the rendered text for a key label.
#[derive(Debug, Default, PartialEq, Eq)]
pub(crate) struct FooterLine {
    text: String,
    /// `(char offset, char width)` of each fully visible key label.
    keys: Vec<(usize, usize)>,
    /// `(char offset, char width)` of the whole help hint, when fully visible.
    help: Option<(usize, usize)>,
}

impl FooterLine {
    /// Lay out `hints` left-aligned and `help` right-aligned within `available`
    /// columns, dropping optional hints before falling back to truncation.
    pub(crate) fn layout(
        mut hints: Vec<FooterHint>,
        help: Option<FooterHint>,
        available: usize,
    ) -> Self {
        let Some(help) = help else {
            return Self::build(&hints, None, 0).truncated(available);
        };
        loop {
            let left_width = Self::joined_width(&hints);
            if left_width + HINT_SEPARATOR.len() + help.width() <= available {
                return Self::build(&hints, Some(&help), available - help.width());
            }
            let Some(index) = hints.iter().position(|hint| hint.optional) else {
                break;
            };
            hints.remove(index);
        }
        // The help hint outlives every other hint: drop them from the end
        // until it fits, and only truncate when help alone is too wide.
        while !hints.is_empty()
            && Self::joined_width(&hints) + HINT_SEPARATOR.len() + help.width() > available
        {
            hints.pop();
        }
        if help.width() > available {
            return Self::build(&[], Some(&help), 0).truncated(available);
        }
        Self::build(&hints, Some(&help), available - help.width())
    }

    /// `(char offset, char width)` of the help hint, when it is fully visible.
    pub(crate) fn help(&self) -> Option<(usize, usize)> {
        self.help
    }

    #[cfg(test)]
    pub(crate) fn text(&self) -> &str {
        &self.text
    }

    /// Split the line into spans, styling key labels with `key_style` and
    /// leaving the rest to inherit the paragraph style.
    pub(crate) fn spans(&self, key_style: Style) -> Vec<Span<'static>> {
        let byte_offset = |chars: usize| {
            self.text
                .char_indices()
                .nth(chars)
                .map_or(self.text.len(), |(byte, _)| byte)
        };
        let mut spans = Vec::new();
        let mut cursor = 0;
        for &(start, width) in &self.keys {
            let (start, end) = (byte_offset(start), byte_offset(start + width));
            if cursor < start {
                spans.push(Span::raw(self.text[cursor..start].to_owned()));
            }
            spans.push(Span::styled(self.text[start..end].to_owned(), key_style));
            cursor = end;
        }
        if cursor < self.text.len() {
            spans.push(Span::raw(self.text[cursor..].to_owned()));
        }
        spans
    }

    fn joined_width(hints: &[FooterHint]) -> usize {
        let separators = hints.len().saturating_sub(1) * HINT_SEPARATOR.len();
        hints.iter().map(FooterHint::width).sum::<usize>() + separators
    }

    /// Build the untruncated line, padding so `help` starts at `help_column`.
    fn build(hints: &[FooterHint], help: Option<&FooterHint>, help_column: usize) -> Self {
        let mut line = Self::default();
        let mut width = 0;
        for (index, hint) in hints.iter().enumerate() {
            if index > 0 {
                line.text.push_str(HINT_SEPARATOR);
                width += HINT_SEPARATOR.len();
            }
            line.push_hint(hint, &mut width);
        }
        if let Some(help) = help {
            let padding = help_column.saturating_sub(width);
            line.text.push_str(&" ".repeat(padding));
            width += padding;
            line.help = Some((width, help.width()));
            line.push_hint(help, &mut width);
        }
        line
    }

    fn push_hint(&mut self, hint: &FooterHint, width: &mut usize) {
        let key_width = hint.key.chars().count();
        self.keys.push((*width, key_width));
        self.text.push_str(&hint.key);
        self.text.push(' ');
        self.text.push_str(hint.label);
        *width += hint.width();
    }

    fn truncated(mut self, available: usize) -> Self {
        let width = self.text.chars().count();
        if width <= available {
            return self;
        }
        self.text = utils::bounded_text(&self.text, available);
        // `bounded_text` replaces the tail with "..." when there is room for it.
        let visible = if available > 3 {
            available - 3
        } else {
            available
        };
        let fits = |&(start, width): &(usize, usize)| start + width <= visible;
        self.keys.retain(fits);
        self.help = self.help.filter(fits);
        self
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn styled_keys(line: &FooterLine) -> Vec<String> {
        let key_style = Style::default().fg(ratatui::style::Color::Red);
        line.spans(key_style)
            .into_iter()
            .filter(|span| span.style == key_style)
            .map(|span| span.content.into_owned())
            .collect()
    }

    #[test]
    fn keys_contained_in_earlier_labels_are_styled_at_their_own_position() {
        let line = FooterLine::layout(
            vec![
                FooterHint::new("enter", "Open"),
                FooterHint::new("n", "Next"),
                FooterHint::new("e", "Edit"),
            ],
            None,
            80,
        );
        assert_eq!(line.text(), "enter Open   n Next   e Edit");
        assert_eq!(styled_keys(&line), ["enter", "n", "e"]);
        let rebuilt: String = line
            .spans(Style::default())
            .iter()
            .map(|span| span.content.as_ref())
            .collect();
        assert_eq!(rebuilt, line.text());
    }

    #[test]
    fn help_is_right_aligned_and_reported_at_its_offset() {
        let line = FooterLine::layout(
            vec![FooterHint::new("enter", "Open")],
            Some(FooterHint::new("?", "Commands")),
            30,
        );
        assert_eq!(line.text().chars().count(), 30);
        assert!(line.text().ends_with("? Commands"));
        assert_eq!(line.help(), Some((20, 10)));
        assert_eq!(styled_keys(&line), ["enter", "?"]);
    }

    #[test]
    fn optional_hints_drop_in_order_regardless_of_key_bindings() {
        let hints = vec![
            FooterHint::new("enter", "Open"),
            FooterHint::optional("p", "Play/pause"),
            FooterHint::optional("l", "Next"),
            FooterHint::optional("x", "Queue"),
        ];
        let line = FooterLine::layout(hints, Some(FooterHint::new("?", "Commands")), 45);
        assert!(line.text().starts_with("enter Open   l Next   x Queue"));
        assert!(!line.text().contains("Play/pause"));
        assert!(line.help().is_some());
    }

    #[test]
    fn narrow_footers_keep_help_and_drop_other_hints_from_the_end() {
        let hints = || {
            vec![
                FooterHint::new("enter", "Open"),
                FooterHint::new("tab", "Pane"),
            ]
        };
        let help = || Some(FooterHint::new("?", "Commands"));

        let line = FooterLine::layout(hints(), help(), 26);
        assert_eq!(line.text(), "enter Open      ? Commands");
        assert_eq!(line.help(), Some((16, 10)));

        let line = FooterLine::layout(hints(), help(), 16);
        assert_eq!(line.text(), "      ? Commands");
        assert_eq!(line.help(), Some((6, 10)));

        // Only when help alone does not fit is it truncated.
        let line = FooterLine::layout(hints(), help(), 8);
        assert_eq!(line.text().chars().count(), 8);
        assert_eq!(line.help(), None);
    }
}
