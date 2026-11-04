//! Ctrl+K: jump to any table or run any command by typing a few letters.

use ratatui::{
    Frame,
    layout::Rect,
    widgets::{Block, BorderType, Clear},
};

use super::{
    list::{Entry, FilterList},
    theme,
};
use crate::db::{Relation, View};

pub enum Action {
    Open(Relation),
    /// Open a relation showing particular rows, remembering where we came from.
    Follow(Relation, View),
    Database(String),
    SwitchConnection,
    Query,
    Ask,
    AskPrivacy,
    AskSent,
    Update,
    Reload,
    Help,
    Quit,
}

pub struct Command {
    pub label: String,
    pub tag: String,
    pub action: Action,
}

pub struct Palette {
    pub list: FilterList,
    pub commands: Vec<Command>,
    placeholder: &'static str,
}

impl Palette {
    pub fn new(commands: Vec<Command>) -> Palette {
        Palette::with_placeholder(commands, "Jump to a table or run a command")
    }

    pub fn with_placeholder(commands: Vec<Command>, placeholder: &'static str) -> Palette {
        let mut palette = Palette { list: FilterList::default(), commands, placeholder };
        palette.refilter();
        palette
    }

    pub fn refilter(&mut self) {
        let labels: Vec<&str> = self.commands.iter().map(|c| c.label.as_str()).collect();
        self.list.refilter(&labels, None);
    }

    pub fn take_selected(mut self) -> Option<Action> {
        let index = self.list.selected()?;
        Some(self.commands.swap_remove(index).action)
    }

    pub fn render(&mut self, frame: &mut Frame, area: Rect) {
        let width = area.width.saturating_sub(4).min(76);
        let rows = (self.commands.len().max(1) as u16).min(14);
        let height = (rows + 4).min(area.height.saturating_sub(2));
        let top = area.y + (area.height.saturating_sub(height)) / 4;
        let rect = Rect { x: area.x + (area.width - width) / 2, y: top, width, height };
        frame.render_widget(Clear, rect);
        let block = Block::bordered().border_type(BorderType::Rounded).border_style(theme::accent());
        let inner = block.inner(rect);
        frame.render_widget(block, rect);
        if inner.height < 3 {
            return;
        }

        frame.buffer_mut().set_string(inner.x + 1, inner.y, "›", theme::accent());
        let field = Rect { x: inner.x + 3, y: inner.y, width: inner.width.saturating_sub(4), height: 1 };
        self.list.input.render(frame, field, self.placeholder, false, true);
        frame.buffer_mut().set_string(inner.x, inner.y + 1, "─".repeat(inner.width as usize), theme::muted());

        let list_area = Rect { y: inner.y + 2, height: inner.height - 2, ..inner };
        if self.list.len() == 0 {
            frame.buffer_mut().set_string(list_area.x + 1, list_area.y, "Nothing matches", theme::muted());
            return;
        }
        let commands = &self.commands;
        self.list.render(frame, list_area, |i| Entry { label: &commands[i].label, detail: "", tag: &commands[i].tag });
    }
}
