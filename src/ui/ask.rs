//! The question field for asking a database something in plain language.

use ratatui::{
    Frame,
    layout::Rect,
    text::{Line, Span},
    widgets::{Block, BorderType, Clear},
};

use super::{input::Input, text, theme};
use crate::agent::mask::Visibility;

pub struct AskPrompt {
    pub input: Input,
    /// The question continues an earlier conversation.
    pub follow_up: bool,
    pub database: String,
    pub visibility: Visibility,
}

impl AskPrompt {
    pub fn render(&self, frame: &mut Frame, area: Rect) {
        let width = area.width.saturating_sub(4).min(76);
        let height = 5.min(area.height.saturating_sub(2));
        let top = area.y + (area.height.saturating_sub(height)) / 4;
        let rect = Rect { x: area.x + (area.width - width) / 2, y: top, width, height };
        frame.render_widget(Clear, rect);
        let title = Line::from(vec![Span::raw(" "), Span::styled("Ask", theme::strong()), Span::raw(" ")]);
        let block = Block::bordered().border_type(BorderType::Rounded).border_style(theme::accent()).title(title);
        let inner = block.inner(rect);
        frame.render_widget(block, rect);
        if inner.height < 3 {
            return;
        }

        let buf = frame.buffer_mut();
        buf.set_string(inner.x + 1, inner.y, "›", theme::accent());
        buf.set_string(inner.x, inner.y + 1, "─".repeat(inner.width as usize), theme::muted());
        let note = if self.follow_up {
            "Follows on from your last question. ctrl+n starts over.".to_owned()
        } else {
            self.visibility.summary(&self.database)
        };
        let note = text::fit(&note, inner.width.saturating_sub(2) as usize);
        buf.set_string(inner.x + 1, inner.y + 2, note, theme::muted());
        let field = Rect { x: inner.x + 3, y: inner.y, width: inner.width.saturating_sub(4), height: 1 };
        self.input.render(frame, field, "What would you like to know?", false, true);
    }
}
