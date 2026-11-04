//! The leader menu: what can be done right here, one key away. Pressing a key
//! closes it and does that thing, so it teaches the shortcuts as it is used.

use ratatui::{
    Frame,
    layout::Rect,
    style::Style,
    text::{Line, Span},
    widgets::{Block, BorderType, Clear},
};

use super::{text, theme};

pub struct Menu {
    pub title: String,
    pub items: Vec<(String, String)>,
}

impl Menu {
    pub fn render(&self, frame: &mut Frame, area: Rect) {
        let key_width = self.items.iter().map(|(k, _)| text::measure(k, 20)).max().unwrap_or(0);
        let label_width = self.items.iter().map(|(_, l)| text::measure(l, 60)).max().unwrap_or(0);
        let width = ((key_width + label_width + 7) as u16).max(text::measure(&self.title, 60) as u16 + 6);
        let width = width.min(area.width.saturating_sub(2));
        let height = (self.items.len() as u16 + 2).min(area.height.saturating_sub(2));
        // Bottom right, floating just inside the panes, near the footer hints.
        let rect = Rect {
            x: area.right().saturating_sub(width + 2),
            y: area.bottom().saturating_sub(height + 1),
            width,
            height,
        };
        frame.render_widget(Clear, rect);
        let block = Block::bordered()
            .border_type(BorderType::Rounded)
            .border_style(theme::accent())
            .title(Line::from(vec![Span::raw(" "), Span::styled(self.title.clone(), theme::strong()), Span::raw(" ")]));
        let inner = block.inner(rect);
        frame.render_widget(block, rect);
        let buf = frame.buffer_mut();
        for (i, (key, label)) in self.items.iter().enumerate().take(inner.height as usize) {
            let y = inner.y + i as u16;
            buf.set_stringn(inner.x + 1, y, key, key_width, theme::accent());
            let x = inner.x + 1 + key_width as u16 + 3;
            buf.set_stringn(x, y, label, inner.right().saturating_sub(x + 1) as usize, Style::new());
        }
    }
}
