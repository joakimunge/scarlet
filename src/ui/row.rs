//! One row laid out vertically: every column with its whole value, which is
//! the comfortable way to read wide tables.

use ratatui::{
    Frame,
    layout::Rect,
    style::Style,
    text::{Line, Span},
    widgets::{Block, BorderType, Clear},
};

use super::{text, theme};

pub struct Field {
    pub name: String,
    /// The full value as displayed, `None` for NULL.
    pub value: Option<String>,
}

/// A wrapped display line: the field name on its first line only.
struct Wrapped {
    name: Option<usize>,
    text: String,
    null: bool,
}

pub struct RowView {
    pub title: String,
    pub fields: Vec<Field>,
    /// The row as JSON, for copying.
    pub json: Option<String>,
    scroll: usize,
    viewport: usize,
    lines: Vec<Wrapped>,
    wrapped_at: usize,
}

impl RowView {
    pub fn new(title: String, fields: Vec<Field>, json: Option<String>) -> RowView {
        RowView { title, fields, json, scroll: 0, viewport: 1, lines: Vec::new(), wrapped_at: 0 }
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

    fn name_width(&self) -> usize {
        self.fields.iter().map(|f| text::measure(&f.name, 28)).max().unwrap_or(0)
    }

    fn wrap(&mut self, width: usize) {
        let name_width = self.name_width();
        let value_width = width.saturating_sub(name_width + 3).max(8);
        self.lines.clear();
        for (i, field) in self.fields.iter().enumerate() {
            let value = field.value.as_deref().unwrap_or("null").replace('\r', "").replace('\t', "    ");
            for (n, text) in text::wrap(&value, value_width).into_iter().enumerate() {
                self.lines.push(Wrapped { name: (n == 0).then_some(i), text, null: field.value.is_none() });
            }
        }
        self.wrapped_at = width;
    }

    pub fn render(&mut self, frame: &mut Frame, area: Rect) {
        let width = area.width.saturating_sub(4).min(110);
        let inner_width = width.saturating_sub(4) as usize;
        if inner_width != self.wrapped_at {
            self.wrap(inner_width);
        }
        let height = (self.lines.len() as u16 + 2).clamp(3, area.height.saturating_sub(2));
        let rect = text::centered(area, width, height);
        frame.render_widget(Clear, rect);
        let mut block = Block::bordered()
            .border_type(BorderType::Rounded)
            .border_style(theme::accent())
            .title(Line::from(vec![Span::raw(" "), Span::styled(self.title.clone(), theme::strong()), Span::raw(" ")]));
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

        let name_width = self.name_width();
        let value_x = inner.x + 1 + name_width as u16 + 2;
        let value_width = inner.right().saturating_sub(value_x + 1) as usize;
        let buf = frame.buffer_mut();
        for (row, line) in self.lines.iter().skip(self.scroll).take(self.viewport).enumerate() {
            let y = inner.y + row as u16;
            if let Some(i) = line.name {
                let name = text::fit_aligned(&self.fields[i].name, name_width, true);
                buf.set_string(inner.x + 1, y, name, theme::muted());
            }
            let style = if line.null { theme::null() } else { Style::new() };
            buf.set_stringn(value_x, y, &line.text, value_width, style);
        }
    }
}
