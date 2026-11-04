//! A closer look at one value: wrapped text, pretty-printed JSON.

use ratatui::{
    Frame,
    layout::Rect,
    text::{Line, Span},
    widgets::{Block, BorderType, Clear},
};

use super::{text, theme};

pub struct ValueView {
    pub column: String,
    pub type_name: String,
    /// The full value, `None` for NULL.
    pub value: Option<String>,
    /// The text as displayed and its widest line, worked out once.
    source: String,
    natural_width: usize,
    scroll: usize,
    lines: Vec<String>,
    wrapped_at: usize,
    viewport: usize,
}

impl ValueView {
    pub fn new(column: String, type_name: String, value: Option<String>) -> ValueView {
        let source = value.as_deref().unwrap_or("null").replace('\r', "").replace('\t', "    ");
        let natural_width = source.lines().map(|l| text::measure(l, 200)).max().unwrap_or(0);
        ValueView {
            column,
            type_name,
            value,
            source,
            natural_width,
            scroll: 0,
            lines: Vec::new(),
            wrapped_at: 0,
            viewport: 1,
        }
    }

    pub fn scroll_by(&mut self, delta: isize) {
        let max = self.lines.len().saturating_sub(self.viewport);
        self.scroll = self.scroll.saturating_add_signed(delta).min(max);
    }

    pub fn page(&mut self, direction: isize, half: bool) {
        let step = if half { self.viewport / 2 } else { self.viewport.saturating_sub(1) };
        self.scroll_by(direction * step.max(1) as isize);
    }

    pub fn top(&mut self) {
        self.scroll = 0;
    }

    pub fn bottom(&mut self) {
        self.scroll = self.lines.len().saturating_sub(self.viewport);
    }

    pub fn render(&mut self, frame: &mut Frame, area: Rect) {
        // Hug the content: as wide as its longest line (or the title), within reason.
        let natural = self.natural_width;
        let title = text::measure(&self.column, 100) + text::measure(&self.type_name, 100) + 6;
        let width = ((natural.max(title) + 4) as u16).clamp(36, 100).min(area.width.saturating_sub(4));
        let wrap = width.saturating_sub(4) as usize;
        if wrap != self.wrapped_at {
            self.lines = text::wrap(&self.source, wrap);
            self.wrapped_at = wrap;
        }
        let height = (self.lines.len() as u16 + 2).clamp(3, area.height.saturating_sub(2));
        let rect = text::centered(area, width, height);
        frame.render_widget(Clear, rect);
        let mut block =
            Block::bordered().border_type(BorderType::Rounded).border_style(theme::accent()).title(Line::from(vec![
                Span::raw(" "),
                Span::styled(self.column.clone(), theme::strong()),
                Span::styled(format!("  {}", self.type_name), theme::muted()),
                Span::raw(" "),
            ]));
        let inner = block.inner(rect);
        self.viewport = inner.height as usize;
        self.scroll = self.scroll.min(self.lines.len().saturating_sub(self.viewport));
        if self.lines.len() > self.viewport {
            let last = (self.scroll + self.viewport).min(self.lines.len());
            block = block.title_bottom(
                Line::from(format!(" {}–{} of {} lines ", self.scroll + 1, last, self.lines.len()))
                    .style(theme::muted())
                    .right_aligned(),
            );
        }
        frame.render_widget(block, rect);
        let style = if self.value.is_some() { Default::default() } else { theme::null() };
        for (i, line) in self.lines.iter().skip(self.scroll).take(self.viewport).enumerate() {
            frame.buffer_mut().set_stringn(inner.x + 1, inner.y + i as u16, line, wrap, style);
        }
    }
}
