//! Choosing a database, and asking for a password when one is needed.

use ratatui::{
    Frame,
    layout::Rect,
    text::{Line, Span},
    widgets::{Block, BorderType, Clear},
};

use super::{
    input::Input,
    list::{Entry, FilterList},
    text, theme,
};
use crate::connect::{Target, is_url};

#[derive(Default)]
pub struct ConnectPicker {
    pub list: FilterList,
    pub error: Option<String>,
    labels: Vec<(String, String, String)>,
}

/// What the picker shows at the bottom while something is going on.
pub struct Activity<'a> {
    pub discovering: bool,
    pub connecting: Option<&'a Target>,
    pub spinner: &'a str,
}

impl ConnectPicker {
    /// Rebuilds the list from `targets`, keeping `keep` selected when given.
    pub fn refilter(&mut self, targets: &[Target], keep: Option<usize>) {
        self.labels = targets.iter().map(|t| (t.database().to_owned(), t.address(), t.source.label())).collect();
        let haystacks: Vec<String> =
            self.labels.iter().map(|(db, address, source)| format!("{db} {address} {source}")).collect();
        self.list.refilter(&haystacks, keep);
    }

    /// The pasted URL, when the search field holds one.
    pub fn url(&self) -> Option<&str> {
        let text = self.list.input.text.trim();
        (is_url(text) || text.contains("host=") || text.contains("dbname=")).then_some(text)
    }

    /// Draws the picker and returns where it went.
    pub fn render(&mut self, frame: &mut Frame, area: Rect, activity: Activity) -> Rect {
        let width = area.width.saturating_sub(4).min(84);
        let status = self.status_lines(width.saturating_sub(4) as usize, &activity);
        // Sized by everything there is, not what matches, so the box stays put while typing.
        let rows = self.labels.len().max(1) as u16;
        let height = (rows.min(12) + 4 + status.len() as u16).min(area.height.saturating_sub(2));
        let top = area.y + (area.height.saturating_sub(height)) / 3;
        let rect = Rect { x: area.x + (area.width - width) / 2, y: top, width, height };
        frame.render_widget(Clear, rect);
        let block = Block::bordered()
            .border_type(BorderType::Rounded)
            .border_style(theme::accent())
            .title(Line::from(vec![Span::raw(" "), Span::styled("Connect", theme::strong()), Span::raw(" ")]));
        let inner = block.inner(rect);
        frame.render_widget(block, rect);
        if inner.height < 3 {
            return rect;
        }

        frame.buffer_mut().set_string(inner.x + 1, inner.y, "›", theme::accent());
        let field = Rect { x: inner.x + 3, y: inner.y, width: inner.width.saturating_sub(4), height: 1 };
        self.list.input.render(frame, field, "Search databases, or paste a connection URL", false, true);
        frame.buffer_mut().set_string(inner.x, inner.y + 1, "─".repeat(inner.width as usize), theme::muted());

        let status_height = (status.len() as u16).min(inner.height.saturating_sub(3));
        let list_area = Rect { y: inner.y + 2, height: inner.height - 2 - status_height, ..inner };
        if self.url().is_some() {
            let row = Rect { height: 1, ..list_area };
            frame.buffer_mut().set_style(row, theme::selection());
            frame.buffer_mut().set_string(row.x + 1, row.y, "Connect to this URL", theme::selection());
        } else if self.list.len() > 0 {
            let labels = &self.labels;
            self.list.render(frame, list_area, |i| Entry {
                label: &labels[i].0,
                detail: &labels[i].1,
                tag: &labels[i].2,
            });
        }

        let status_y = inner.bottom() - status_height;
        for (i, (line, style)) in status.iter().take(status_height as usize).enumerate() {
            frame.buffer_mut().set_string(inner.x + 1, status_y + i as u16, line, *style);
        }
        rect
    }

    fn status_lines(&self, width: usize, activity: &Activity) -> Vec<(String, ratatui::style::Style)> {
        if let Some(target) = activity.connecting {
            let line = format!("{} Connecting to {} on {}", activity.spinner, target.database(), target.address());
            return vec![(text::fit(&line, width), theme::muted())];
        }
        if let Some(error) = &self.error {
            return text::wrap(error, width).into_iter().take(3).map(|l| (l, theme::error())).collect();
        }
        if activity.discovering {
            return vec![(format!("{} Looking for databases", activity.spinner), theme::muted())];
        }
        if self.url().is_none() && self.list.len() == 0 {
            let message = if self.list.input.is_empty() {
                "No databases found. Paste a connection URL to connect."
            } else {
                "Nothing matches. Paste a connection URL to connect."
            };
            return vec![(text::fit(message, width), theme::muted())];
        }
        Vec::new()
    }
}

pub struct PasswordPrompt {
    pub target: Target,
    pub input: Input,
    pub error: Option<String>,
}

impl PasswordPrompt {
    /// Draws the prompt and returns where it went.
    pub fn render(&mut self, frame: &mut Frame, area: Rect, connecting: bool, spinner: &str) -> Rect {
        let width = area.width.saturating_sub(4).min(60);
        let error = self.error.as_deref().map(|e| text::wrap(e, width.saturating_sub(4) as usize)).unwrap_or_default();
        let height = (5 + error.len().min(3) as u16).min(area.height);
        let rect = text::centered(area, width, height);
        frame.render_widget(Clear, rect);
        let block = Block::bordered()
            .border_type(BorderType::Rounded)
            .border_style(theme::accent())
            .title(Line::from(vec![Span::raw(" "), Span::styled("Password", theme::strong()), Span::raw(" ")]));
        let inner = block.inner(rect);
        frame.render_widget(block, rect);
        if inner.height < 3 {
            return rect;
        }
        let place = format!("{} on {}", self.target.database(), self.target.address());
        let buf = frame.buffer_mut();
        buf.set_string(inner.x + 1, inner.y, text::fit(&place, inner.width as usize - 2), theme::muted());
        buf.set_string(inner.x + 1, inner.y + 2, "›", theme::accent());
        let field = Rect { x: inner.x + 3, y: inner.y + 2, width: inner.width.saturating_sub(4), height: 1 };
        if connecting {
            frame.buffer_mut().set_string(field.x, field.y, format!("{spinner} Connecting"), theme::muted());
        } else {
            self.input.render(frame, field, "", true, true);
        }
        for (i, line) in error.iter().take(3).enumerate() {
            let y = inner.y + 3 + i as u16;
            if y < inner.bottom() {
                frame.buffer_mut().set_string(inner.x + 1, y, line, theme::error());
            }
        }
        rect
    }
}
