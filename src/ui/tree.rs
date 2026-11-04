//! The sidebar: schemas and their tables, with fuzzy filtering.

use std::{cmp::Reverse, collections::HashSet};

use ratatui::{
    Frame,
    layout::Rect,
    text::{Line, Span},
    widgets::{Block, BorderType},
};

use super::{fuzzy::Fuzzy, input::Input, text, theme};
use crate::db::{Relation, RelationKind, Schema};

/// A fuzzy match: score, relation index, matched character positions.
type Hit = (u32, usize, Vec<u32>);

enum Item {
    Schema(usize),
    Relation { schema: usize, index: usize, highlights: Vec<u32> },
}

pub enum Selected<'a> {
    Schema(&'a Schema),
    Relation(&'a Relation),
}

#[derive(Default)]
pub struct Tree {
    pub schemas: Vec<Schema>,
    pub loaded: bool,
    pub error: Option<String>,
    pub filter: Input,
    /// The filter field has keyboard focus.
    pub filtering: bool,
    expanded: HashSet<String>,
    items: Vec<Item>,
    cursor: usize,
    offset: usize,
    viewport: usize,
    fuzzy: Fuzzy,
}

impl Tree {
    pub fn set_catalog(&mut self, schemas: Vec<Schema>) {
        let previous = self.selected_key();
        if !self.loaded {
            let first = schemas.iter().find(|s| s.name == "public").or_else(|| schemas.first());
            if let Some(schema) = first {
                self.expanded.insert(schema.name.clone());
            }
        }
        self.schemas = schemas;
        self.loaded = true;
        self.error = None;
        self.rebuild();
        match previous {
            Some(key) => self.select_key(&key),
            None => self.select_first_relation(),
        }
    }

    pub fn selected(&self) -> Option<Selected<'_>> {
        match self.items.get(self.cursor)? {
            Item::Schema(s) => Some(Selected::Schema(&self.schemas[*s])),
            Item::Relation { schema, index, .. } => Some(Selected::Relation(&self.schemas[*schema].relations[*index])),
        }
    }

    pub fn selected_relation(&self) -> Option<&Relation> {
        match self.selected()? {
            Selected::Relation(r) => Some(r),
            Selected::Schema(_) => None,
        }
    }

    pub fn move_by(&mut self, delta: isize) {
        let last = self.items.len().saturating_sub(1);
        self.cursor = self.cursor.saturating_add_signed(delta).min(last);
    }

    pub fn page(&mut self, direction: isize, half: bool) {
        let step = if half { self.viewport / 2 } else { self.viewport.saturating_sub(1) };
        self.move_by(direction * step.max(1) as isize);
    }

    pub fn top(&mut self) {
        self.cursor = 0;
    }

    pub fn bottom(&mut self) {
        self.cursor = self.items.len().saturating_sub(1);
    }

    /// Expands a collapsed schema, or steps into an expanded one.
    pub fn expand(&mut self) {
        let Some(Item::Schema(s)) = self.items.get(self.cursor) else { return };
        let name = self.schemas[*s].name.clone();
        if self.expanded.insert(name) {
            self.rebuild();
        } else if matches!(self.items.get(self.cursor + 1), Some(Item::Relation { .. })) {
            self.cursor += 1;
        }
    }

    pub fn toggle(&mut self) {
        let Some(Item::Schema(s)) = self.items.get(self.cursor) else { return };
        let name = self.schemas[*s].name.clone();
        if !self.expanded.remove(&name) {
            self.expanded.insert(name);
        }
        self.rebuild();
    }

    /// Collapses the schema, or from a table jumps to its schema.
    pub fn collapse(&mut self) {
        match self.items.get(self.cursor) {
            Some(Item::Relation { schema, .. }) => {
                let schema = *schema;
                if let Some(i) = self.items.iter().position(|it| matches!(it, Item::Schema(s) if *s == schema)) {
                    self.cursor = i;
                }
            }
            Some(Item::Schema(s)) if self.filter.is_empty() => {
                let name = self.schemas[*s].name.clone();
                if self.expanded.remove(&name) {
                    self.rebuild();
                }
            }
            _ => {}
        }
    }

    pub fn clear_filter(&mut self) {
        let key = self.selected_key();
        self.filter.clear();
        self.filtering = false;
        // Keep the table that was found visible once the filter is gone.
        if let Some((schema, _)) = &key {
            self.expanded.insert(schema.clone());
        }
        self.rebuild();
        if let Some(key) = key {
            self.select_key(&key);
        }
    }

    /// Clears any filter and selects `relation`, expanding its schema.
    pub fn reveal(&mut self, relation: &Relation) {
        self.filter.clear();
        self.filtering = false;
        self.expanded.insert(relation.schema.clone());
        self.rebuild();
        self.select_key(&(relation.schema.clone(), Some(relation.name.clone())));
    }

    pub fn filter_changed(&mut self) {
        self.rebuild();
        // Land on the best match.
        self.cursor = self.items.iter().position(|it| matches!(it, Item::Relation { .. })).unwrap_or(0);
    }

    fn rebuild(&mut self) {
        self.items.clear();
        let query = self.filter.text.trim();
        if query.is_empty() {
            for (s, schema) in self.schemas.iter().enumerate() {
                self.items.push(Item::Schema(s));
                if self.expanded.contains(&schema.name) {
                    for index in 0..schema.relations.len() {
                        self.items.push(Item::Relation { schema: s, index, highlights: Vec::new() });
                    }
                }
            }
        } else {
            let pattern = Fuzzy::pattern(query);
            let qualified = query.contains('.');
            let mut groups: Vec<(u32, usize, Vec<Hit>)> = Vec::new();
            for (s, schema) in self.schemas.iter().enumerate() {
                let mut hits: Vec<Hit> = schema
                    .relations
                    .iter()
                    .enumerate()
                    .filter_map(|(index, rel)| {
                        if qualified {
                            let full = format!("{}.{}", rel.schema, rel.name);
                            let prefix = rel.schema.chars().count() as u32 + 1;
                            let (score, hl) = self.fuzzy.score(&pattern, &full)?;
                            Some((score, index, hl.into_iter().filter(|&i| i >= prefix).map(|i| i - prefix).collect()))
                        } else {
                            let (score, hl) = self.fuzzy.score(&pattern, &rel.name)?;
                            Some((score, index, hl))
                        }
                    })
                    .collect();
                if hits.is_empty() {
                    continue;
                }
                hits.sort_by_key(|hit| Reverse(hit.0));
                groups.push((hits[0].0, s, hits));
            }
            groups.sort_by_key(|group| Reverse(group.0));
            for (_, s, hits) in groups {
                self.items.push(Item::Schema(s));
                for (_, index, highlights) in hits {
                    self.items.push(Item::Relation { schema: s, index, highlights });
                }
            }
        }
        self.cursor = self.cursor.min(self.items.len().saturating_sub(1));
    }

    fn selected_key(&self) -> Option<(String, Option<String>)> {
        Some(match self.selected()? {
            Selected::Schema(s) => (s.name.clone(), None),
            Selected::Relation(r) => (r.schema.clone(), Some(r.name.clone())),
        })
    }

    fn select_key(&mut self, key: &(String, Option<String>)) {
        let found = self.items.iter().position(|item| match (item, &key.1) {
            (Item::Schema(s), None) => self.schemas[*s].name == key.0,
            (Item::Relation { schema, index, .. }, Some(name)) => {
                let rel = &self.schemas[*schema].relations[*index];
                rel.schema == key.0 && &rel.name == name
            }
            _ => false,
        });
        match found {
            Some(i) => self.cursor = i,
            None => self.select_first_relation(),
        }
    }

    fn select_first_relation(&mut self) {
        self.cursor = self.items.iter().position(|it| matches!(it, Item::Relation { .. })).unwrap_or(0);
    }

    pub fn render(&mut self, frame: &mut Frame, area: Rect, focused: bool, title: &str, busy: Option<&str>) {
        let mut heading = vec![Span::raw(" "), Span::styled(title.to_owned(), theme::strong()), Span::raw(" ")];
        if let Some(spinner) = busy {
            heading.push(Span::styled(format!("{spinner} "), theme::accent()));
        }
        let block = Block::bordered()
            .border_type(BorderType::Rounded)
            .border_style(theme::border(focused))
            .title(Line::from(heading));
        let inner = block.inner(area);
        frame.render_widget(block, area);
        if inner.height == 0 || inner.width < 4 {
            return;
        }

        let show_filter = self.filtering || !self.filter.is_empty();
        let list_height = inner.height.saturating_sub(show_filter as u16);
        let list = Rect { height: list_height, ..inner };
        self.viewport = list.height as usize;

        if show_filter {
            let y = inner.bottom() - 1;
            let prompt_style = if self.filtering { theme::accent() } else { theme::muted() };
            frame.buffer_mut().set_string(inner.x + 1, y, "/", prompt_style);
            let field = Rect { x: inner.x + 3, y, width: inner.width.saturating_sub(4), height: 1 };
            self.filter.render(frame, field, "filter tables", false, self.filtering && focused);
        }

        if let Some(error) = &self.error {
            let lines = text::wrap(error, list.width.saturating_sub(2) as usize);
            for (i, line) in lines.iter().take(list.height as usize).enumerate() {
                frame.buffer_mut().set_string(list.x + 1, list.y + i as u16, line, theme::error());
            }
            return;
        }
        if self.loaded && self.items.is_empty() {
            let message = if self.filter.is_empty() { "No tables" } else { "No matches" };
            frame.buffer_mut().set_string(list.x + 1, list.y, message, theme::muted());
            return;
        }

        if self.cursor < self.offset {
            self.offset = self.cursor;
        } else if self.viewport > 0 && self.cursor >= self.offset + self.viewport {
            self.offset = self.cursor + 1 - self.viewport;
        }
        self.offset = self.offset.min(self.items.len().saturating_sub(self.viewport));

        let width = list.width as usize;
        for (row, item) in self.items.iter().enumerate().skip(self.offset).take(self.viewport) {
            let y = list.y + (row - self.offset) as u16;
            let selected = row == self.cursor;
            let line_style = if selected && focused { theme::selection() } else { Default::default() };
            let buf = frame.buffer_mut();
            buf.set_style(Rect { x: list.x, y, width: list.width, height: 1 }, line_style);
            match item {
                Item::Schema(s) => {
                    let schema = &self.schemas[*s];
                    let open = self.expanded.contains(&schema.name) || !self.filter.is_empty();
                    let arrow = if open { "▾" } else { "▸" };
                    let arrow_style = if selected && focused { line_style } else { theme::muted() };
                    buf.set_string(list.x + 1, y, arrow, arrow_style);
                    let name = text::fit(&schema.name, width.saturating_sub(4));
                    buf.set_string(list.x + 3, y, name, line_style.patch(theme::strong()));
                }
                Item::Relation { schema, index, highlights } => {
                    let rel = &self.schemas[*schema].relations[*index];
                    let hint = match rel.kind {
                        RelationKind::Table => rel.estimate.map(text::compact).unwrap_or_default(),
                        kind => kind.label().to_owned(),
                    };
                    let hint_width = hint.chars().count();
                    let name_width = width.saturating_sub(3 + hint_width + 2);
                    let name = text::fit(&rel.name, name_width);
                    let base = if selected && !focused { theme::strong() } else { line_style };
                    let hl = if selected && focused { theme::strong() } else { theme::matched() };
                    let line = Line::from(text::highlight(&name, highlights, base, hl));
                    buf.set_line(list.x + 3, y, &line, name_width as u16);
                    if hint_width > 0 {
                        let hint_style = if selected && focused { line_style } else { theme::muted() };
                        let x = list.right().saturating_sub(hint_width as u16 + 1);
                        buf.set_string(x, y, &hint, hint_style);
                    }
                }
            }
        }
    }
}
