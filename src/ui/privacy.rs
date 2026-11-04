//! Choosing what Ask may share with Claude, asked the first time per database.

use ratatui::{
    Frame,
    layout::Rect,
    style::Style,
    text::{Line, Span},
    widgets::{Block, BorderType, Clear},
};

use super::{ask::AskPrompt, text, theme};
use crate::agent::mask::Visibility;

pub struct PrivacyPicker {
    pub selected: usize,
    pub database: String,
    /// The question box to open (or return to) once a choice is made.
    pub then: Option<AskPrompt>,
    /// Whether a choice was saved before, so leaving without one keeps it.
    pub had_choice: bool,
}

impl PrivacyPicker {
    pub fn new(database: &str, current: Option<Visibility>, then: Option<AskPrompt>) -> PrivacyPicker {
        let selected = current.and_then(|c| Visibility::ALL.iter().position(|v| *v == c)).unwrap_or(0);
        PrivacyPicker { selected, database: database.to_owned(), then, had_choice: current.is_some() }
    }

    pub fn choice(&self) -> Visibility {
        Visibility::ALL[self.selected]
    }

    pub fn move_by(&mut self, delta: isize) {
        self.selected = self.selected.saturating_add_signed(delta).min(Visibility::ALL.len() - 1);
    }

    pub fn render(&self, frame: &mut Frame, area: Rect) {
        let width = area.width.saturating_sub(4).min(72);
        let text_width = width.saturating_sub(6) as usize;
        let intro = text::wrap(
            &format!(
                "Ask sends your question to Anthropic through your own Claude account, together with \
                 what Claude reads from {}.",
                self.database
            ),
            width.saturating_sub(4) as usize,
        );
        let mut lines: Vec<(u16, String, Style)> = intro.into_iter().map(|l| (1, l, Style::new())).collect();
        for (index, visibility) in Visibility::ALL.iter().enumerate() {
            lines.push((0, String::new(), Style::new()));
            let label = match visibility {
                Visibility::Masked => format!("{} (recommended)", visibility.label()),
                _ => visibility.label().to_owned(),
            };
            let style = if index == self.selected { theme::selection() } else { theme::strong() };
            lines.push((1, label, style));
            for line in text::wrap(visibility.explanation(), text_width) {
                lines.push((3, line, theme::muted()));
            }
        }
        lines.push((0, String::new(), Style::new()));
        lines.push((1, "Remembered for this database. ctrl+p in the question box changes it.".into(), theme::muted()));

        let height = (lines.len() as u16 + 2).min(area.height);
        let rect = text::centered(area, width, height);
        frame.render_widget(Clear, rect);
        let title =
            Line::from(vec![Span::raw(" "), Span::styled("What Claude can see", theme::strong()), Span::raw(" ")]);
        let block = Block::bordered().border_type(BorderType::Rounded).border_style(theme::accent()).title(title);
        let inner = block.inner(rect);
        frame.render_widget(block, rect);
        let buf = frame.buffer_mut();
        for (row, (indent, line, style)) in lines.iter().enumerate().take(inner.height as usize) {
            let y = inner.y + row as u16;
            if *style == theme::selection() {
                buf.set_style(Rect { x: inner.x, y, width: inner.width, height: 1 }, theme::selection());
            }
            let x = inner.x + indent;
            buf.set_stringn(x, y, line, inner.right().saturating_sub(x + 1) as usize, *style);
        }
    }
}
