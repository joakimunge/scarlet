//! The data grid. Rows are fetched in pages around the viewport, so tables of
//! any size open instantly and scroll without loading everything. A `View`
//! (filter and sort) decides which rows; changing it refetches without
//! describing the table again.

use std::{
    borrow::Cow,
    collections::{HashMap, HashSet},
    time::Instant,
};

use ratatui::{
    Frame,
    layout::Rect,
    style::{Modifier, Style},
    text::{Line, Span},
    widgets::{Block, BorderType},
};

use super::{input::Input, text, theme};
use crate::db::{Column, Filter, Link, Relation, RelationKind, Row, Sort, TableInfo, ValueKind, View, row_alias};

pub const PAGE_SIZE: usize = 500;
const MAX_WIDTH: usize = 40;
const MIN_WIDTH: usize = 3;
const GAP: usize = 2;
/// Pages kept in memory on either side of the cursor.
const KEEP_PAGES: usize = 6;
/// Rows sampled to size columns.
const SAMPLE_ROWS: usize = 200;

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Count {
    Exact(u64),
    Estimate(u64),
    Unknown,
}

pub enum Status {
    Describing,
    Ready,
    Failed(String),
}

pub struct Grid {
    pub relation: Relation,
    pub generation: u64,
    pub status: Status,
    pub columns: Vec<Column>,
    pub order_by: Vec<String>,
    pub links_out: Vec<Link>,
    pub links_in: Vec<Link>,
    /// What the rows are called in generated SQL (see `db::row_alias`).
    pub alias: String,
    /// On-disk size, to decide whether an exact count is cheap.
    pub bytes: u64,
    pub view: View,
    /// The filter bar while it is being edited, and the filter it started from.
    pub filter_input: Option<(Input, String)>,
    /// Where the user came from when following a link, for the title.
    pub came_from: Option<String>,
    pub count: Count,
    /// When the oldest page request still in flight was sent.
    pub busy_since: Option<Instant>,
    /// A query result held in memory: all its rows, before the view's search and sort.
    local: Option<Vec<Row>>,
    /// Shown after the title, e.g. how long a query took.
    pub note: Option<String>,
    pub row: usize,
    pub col: usize,
    widths: Vec<usize>,
    pages: HashMap<usize, Vec<Row>>,
    requested: HashSet<usize>,
    /// The user asked for the last row before the total was settled; the
    /// cursor follows the end until they move elsewhere.
    wants_bottom: bool,
    /// Same for the last column, before the columns are known.
    wants_last_column: bool,
    /// Rows proven to exist by loaded pages.
    known_rows: usize,
    /// Exact number of rows, once a short page has revealed the end.
    end: Option<usize>,
    /// Upper bound from an empty page past the end when the estimate overshot.
    bound: Option<usize>,
    row_offset: usize,
    col_offset: usize,
    viewport: usize,
}

impl Grid {
    pub fn new(relation: Relation, generation: u64, view: View) -> Grid {
        Grid {
            relation,
            generation,
            status: Status::Describing,
            columns: Vec::new(),
            order_by: Vec::new(),
            links_out: Vec::new(),
            links_in: Vec::new(),
            alias: String::new(),
            bytes: 0,
            view,
            filter_input: None,
            came_from: None,
            count: Count::Unknown,
            busy_since: None,
            local: None,
            note: None,
            row: 0,
            col: 0,
            widths: Vec::new(),
            pages: HashMap::new(),
            requested: HashSet::new(),
            known_rows: 0,
            wants_bottom: false,
            wants_last_column: false,
            end: None,
            bound: None,
            row_offset: 0,
            col_offset: 0,
            viewport: 20,
        }
    }

    /// A grid over rows already in memory (a query result).
    pub fn from_rows(columns: Vec<Column>, rows: Vec<Row>, note: String, generation: u64) -> Grid {
        let relation =
            Relation { schema: String::new(), name: "Result".into(), kind: RelationKind::View, estimate: None };
        let mut grid = Grid::new(relation, generation, View::default());
        grid.widths = columns.iter().map(|c| text::measure(&c.name, MAX_WIDTH).max(MIN_WIDTH)).collect();
        grid.columns = columns;
        grid.local = Some(rows);
        grid.note = Some(note);
        grid.status = Status::Ready;
        grid.load_local();
        grid
    }

    pub fn is_local(&self) -> bool {
        self.local.is_some()
    }

    /// Lays the in-memory rows out as pages after applying the view: searched
    /// as text, sorted with numbers as numbers and NULLs last.
    fn load_local(&mut self) {
        let Some(all) = &self.local else { return };
        let mut rows: Vec<&Row> = match self.view.filter() {
            Filter::Search(words) => all
                .iter()
                .filter(|row| row.iter().flatten().any(|value| !text::find_all(value, words).is_empty()))
                .collect(),
            _ => all.iter().collect(),
        };
        if let Some(sort) = &self.view.sort
            && let Some(i) = self.columns.iter().position(|c| c.name == sort.column)
        {
            let numeric = self.columns[i].kind == ValueKind::Number;
            rows.sort_by(|a, b| {
                let (a, b) = (a.get(i).and_then(Option::as_deref), b.get(i).and_then(Option::as_deref));
                let order = match (a, b) {
                    (None, None) => std::cmp::Ordering::Equal,
                    (None, Some(_)) => return std::cmp::Ordering::Greater,
                    (Some(_), None) => return std::cmp::Ordering::Less,
                    (Some(a), Some(b)) if numeric => match (a.parse::<f64>(), b.parse::<f64>()) {
                        (Ok(x), Ok(y)) => x.total_cmp(&y),
                        _ => a.cmp(b),
                    },
                    (Some(a), Some(b)) => a.cmp(b),
                };
                if sort.descending { order.reverse() } else { order }
            });
        }
        let rows: Vec<Row> = rows.into_iter().cloned().collect();
        self.pages.clear();
        for (page, chunk) in rows.chunks(PAGE_SIZE).enumerate() {
            self.pages.insert(page, chunk.to_vec());
        }
        if let Some(first) = self.pages.get(&0).cloned() {
            self.size_columns(&first);
        }
        self.known_rows = rows.len();
        self.end = Some(rows.len());
        self.count = Count::Exact(rows.len() as u64);
        self.clamp();
    }

    /// Starts over with fresh data and a fresh description, keeping the view and cursor.
    pub fn reload(&mut self, generation: u64) {
        let mut fresh = Grid::new(self.relation.clone(), generation, self.view.clone());
        (fresh.row, fresh.col, fresh.row_offset, fresh.col_offset) =
            (self.row, self.col, self.row_offset, self.col_offset);
        fresh.came_from = self.came_from.take();
        *self = fresh;
    }

    /// Shows a different set of rows. The description is kept; the data is
    /// fetched again from the top.
    pub fn set_view(&mut self, view: View, generation: u64) {
        self.view = view;
        self.generation = generation;
        self.pages.clear();
        self.requested.clear();
        self.busy_since = None;
        self.known_rows = 0;
        self.end = None;
        self.bound = None;
        self.wants_bottom = false;
        self.row = 0;
        self.row_offset = 0;
        self.count = self.initial_count();
        if !self.columns.is_empty() {
            self.status = Status::Ready;
        }
        if self.local.is_some() {
            self.load_local();
        }
        if let Some(sort) = &self.view.sort
            && let Some(i) = self.columns.iter().position(|c| c.name == sort.column)
        {
            // Room for the sort arrow.
            self.widths[i] = self.widths[i].max(text::measure(&self.columns[i].name, MAX_WIDTH) + 2).min(MAX_WIDTH);
        }
    }

    fn initial_count(&self) -> Count {
        match (self.view.filter(), self.relation.estimate) {
            (Filter::All, Some(estimate)) => Count::Estimate(estimate),
            _ => Count::Unknown,
        }
    }

    pub fn described(&mut self, info: TableInfo) {
        self.widths = info.columns.iter().map(|c| text::measure(&c.name, MAX_WIDTH).max(MIN_WIDTH)).collect();
        self.alias = row_alias(&self.relation, &info.columns);
        self.columns = info.columns;
        self.order_by = info.order_by;
        self.links_out = info.links_out;
        self.links_in = info.links_in;
        self.bytes = info.bytes;
        self.count = self.initial_count();
        if let Some(sort) = &self.view.sort
            && let Some(i) = self.columns.iter().position(|c| c.name == sort.column)
        {
            self.widths[i] = self.widths[i].max(text::measure(&self.columns[i].name, MAX_WIDTH) + 2).min(MAX_WIDTH);
        }
        self.col = self.col.min(self.columns.len().saturating_sub(1));
        self.status = Status::Ready;
        if std::mem::take(&mut self.wants_last_column) {
            self.last_column();
        }
        if std::mem::take(&mut self.wants_bottom) {
            self.bottom();
        }
    }

    pub fn is_busy(&self) -> bool {
        matches!(self.status, Status::Describing) || !self.requested.is_empty()
    }

    /// Rows the cursor can move through right now.
    pub fn rows(&self) -> usize {
        if let Some(end) = self.end {
            return end;
        }
        let guess = match self.count {
            Count::Exact(n) => return n as usize,
            Count::Estimate(n) => n as usize,
            Count::Unknown => 0,
        };
        let mut rows = guess.max(self.known_rows);
        if let Some(bound) = self.bound {
            rows = rows.min(bound);
        }
        rows.max(self.known_rows)
    }

    /// Whether more rows may exist beyond what is known.
    fn open_ended(&self) -> bool {
        self.end.is_none()
            && !matches!(self.count, Count::Exact(_))
            && self.known_rows.is_multiple_of(PAGE_SIZE)
            && self.bound.is_none_or(|b| b > self.known_rows)
    }

    /// Pages needed around the viewport that are neither loaded nor in flight.
    /// They are marked as requested.
    pub fn wanted_pages(&mut self) -> Vec<usize> {
        if !matches!(self.status, Status::Ready) || self.local.is_some() {
            return Vec::new();
        }
        let rows = self.rows();
        let limit = if self.open_ended() { rows + 1 } else { rows };
        let start = self.row_offset.saturating_sub(self.viewport);
        let stop = (self.row_offset + self.viewport * 2).min(limit);
        let mut pages: Vec<usize> = (start / PAGE_SIZE..stop.div_ceil(PAGE_SIZE)).collect();
        // The estimate overshot: bisect between the last full page and the empty one.
        if self.end.is_none()
            && let Some(bound) = self.bound
        {
            let (low, high) = (self.known_rows / PAGE_SIZE, bound / PAGE_SIZE);
            if high > low + 1 {
                pages.push((low + high) / 2);
            }
        }
        pages.sort_unstable();
        pages.dedup();
        pages.retain(|p| !self.pages.contains_key(p) && !self.requested.contains(p));
        if self.requested.is_empty() && !pages.is_empty() {
            self.busy_since = Some(Instant::now());
        }
        self.requested.extend(&pages);
        pages
    }

    /// Puts the grid aside (when following a link). Requests in flight are
    /// forgotten so they are asked for again when it comes back.
    pub fn suspend(&mut self) {
        self.requested.clear();
        self.busy_since = None;
        self.filter_input = None;
    }

    /// Asks for `page` again (its query was cancelled under it).
    pub fn retry(&mut self, page: usize) {
        self.requested.remove(&page);
        if self.requested.is_empty() {
            self.busy_since = None;
        }
    }

    pub fn page_loaded(&mut self, page: usize, rows: Vec<Row>) {
        self.requested.remove(&page);
        if self.requested.is_empty() {
            self.busy_since = None;
        }
        let start = page * PAGE_SIZE;
        let len = rows.len();
        if len > 0 {
            self.known_rows = self.known_rows.max(start + len);
        }
        if len < PAGE_SIZE {
            if len > 0 || page == 0 {
                self.end = Some(start + len);
            } else {
                self.bound = Some(self.bound.map_or(start, |b| b.min(start)));
            }
        }
        if self.end.is_none() && self.bound == Some(self.known_rows) {
            self.end = Some(self.known_rows);
        }
        if len > 0 {
            self.size_columns(&rows);
        }
        if len > 0 {
            self.pages.insert(page, rows);
        }
        if self.wants_bottom {
            self.bottom();
        }
        let current = self.row / PAGE_SIZE;
        self.pages.retain(|p, _| p.abs_diff(current) <= KEEP_PAGES);
        self.clamp();
    }

    pub fn failed(&mut self, message: String) {
        self.status = Status::Failed(message);
        self.requested.clear();
        self.busy_since = None;
    }

    /// Whether this table is small enough, or filtered, so counting is worth it.
    pub fn wants_count(&self, limit_bytes: u64) -> bool {
        match self.relation.kind {
            _ if self.local.is_some() => false,
            _ if !matches!(self.view.filter(), Filter::All) => true,
            RelationKind::Table | RelationKind::MaterializedView => self.bytes <= limit_bytes,
            RelationKind::View | RelationKind::ForeignTable => true,
        }
    }

    /// The view with the sort on the current column moved one step:
    /// ascending, descending, off.
    pub fn next_sort(&self) -> Option<View> {
        let column = self.columns.get(self.col)?.name.clone();
        let sort = match &self.view.sort {
            Some(Sort { column: c, descending: false }) if *c == column => Some(Sort { column, descending: true }),
            Some(Sort { column: c, descending: true }) if *c == column => None,
            _ => Some(Sort { column, descending: false }),
        };
        Some(View { sort, ..self.view.clone() })
    }

    /// The foreign key the current column belongs to, if any.
    pub fn link_out(&self) -> Option<&Link> {
        let name = &self.columns.get(self.col)?.name;
        self.links_out.iter().find(|l| l.columns.contains(name))
    }

    /// Foreign keys elsewhere that point at the current column.
    pub fn links_in(&self) -> Vec<&Link> {
        let Some(column) = self.columns.get(self.col) else { return Vec::new() };
        self.links_in.iter().filter(|l| l.columns.contains(&column.name)).collect()
    }

    /// A column's value in the current row: `None` if the row isn't loaded,
    /// `Some(None)` for NULL.
    pub fn value_of(&self, column: &str) -> Option<Option<&str>> {
        let index = self.columns.iter().position(|c| c.name == column)?;
        Some(self.row_data(self.row)?.get(index)?.as_deref())
    }

    pub fn column(&self, name: &str) -> Option<&Column> {
        self.columns.iter().find(|c| c.name == name)
    }

    /// Every column of the current row, for the row view.
    pub fn current_row(&self) -> Option<Vec<(&Column, Option<&str>)>> {
        let row = self.row_data(self.row)?;
        Some(self.columns.iter().zip(row.iter().map(Option::as_deref)).collect())
    }

    /// The current row as a JSON object, with numbers (digit for digit), booleans and JSON kept typed.
    pub fn row_json(&self) -> Option<String> {
        use serde_json::{Map, Value};
        let mut object = Map::new();
        for (column, value) in self.current_row()? {
            let value = match value {
                None => Value::Null,
                Some(v) => match column.kind {
                    ValueKind::Bool => Value::Bool(v == "t"),
                    ValueKind::Number => {
                        v.parse::<serde_json::Number>().map_or_else(|_| Value::String(v.into()), Value::Number)
                    }
                    ValueKind::Json => serde_json::from_str(v).unwrap_or_else(|_| Value::String(v.into())),
                    _ => Value::String(v.into()),
                },
            };
            object.insert(column.name.clone(), value);
        }
        serde_json::to_string_pretty(&Value::Object(object)).ok()
    }

    /// Widens columns to fit a newly loaded page. Columns only ever grow, so the
    /// layout settles quickly and never jumps back and forth.
    fn size_columns(&mut self, sample: &[Row]) {
        for (i, column) in self.columns.iter().enumerate() {
            let data = sample
                .iter()
                .take(SAMPLE_ROWS)
                .map(|row| match row.get(i).and_then(Option::as_deref) {
                    None => 4,
                    Some(value) => text::measure(&display(value, column.kind), MAX_WIDTH),
                })
                .max()
                .unwrap_or(0);
            self.widths[i] = self.widths[i].max(data).clamp(MIN_WIDTH, MAX_WIDTH);
        }
    }

    fn row_data(&self, row: usize) -> Option<&Row> {
        self.pages.get(&(row / PAGE_SIZE))?.get(row % PAGE_SIZE)
    }

    /// The column and value under the cursor, if that row is loaded.
    pub fn current(&self) -> Option<(&Column, Option<&str>)> {
        let column = self.columns.get(self.col)?;
        let row = self.row_data(self.row)?;
        Some((column, row.get(self.col)?.as_deref()))
    }

    fn clamp(&mut self) {
        let rows = self.rows();
        if self.row >= rows {
            self.row = rows.saturating_sub(1);
        }
    }

    pub fn move_by(&mut self, rows: isize, cols: isize) {
        if rows != 0 {
            self.wants_bottom = false;
        }
        if cols != 0 {
            self.wants_last_column = false;
        }
        self.row = self.row.saturating_add_signed(rows);
        self.col = self.col.saturating_add_signed(cols).min(self.columns.len().saturating_sub(1));
        self.clamp();
    }

    pub fn page(&mut self, direction: isize, half: bool) {
        let step = if half { self.viewport / 2 } else { self.viewport.saturating_sub(1) };
        self.move_by(direction * step.max(1) as isize, 0);
    }

    pub fn top(&mut self) {
        self.wants_bottom = false;
        self.row = 0;
    }

    pub fn bottom(&mut self) {
        self.wants_bottom = !self.settled();
        self.row = self.rows().saturating_sub(1);
    }

    /// Whether the number of rows is final.
    fn settled(&self) -> bool {
        matches!(self.status, Status::Ready) && (self.end.is_some() || matches!(self.count, Count::Exact(_)))
    }

    pub fn set_count(&mut self, count: u64) {
        self.count = Count::Exact(count);
        if self.wants_bottom {
            self.bottom();
        }
        self.clamp();
    }

    pub fn first_column(&mut self) {
        self.wants_last_column = false;
        self.col = 0;
    }

    pub fn last_column(&mut self) {
        self.wants_last_column = self.columns.is_empty();
        self.col = self.columns.len().saturating_sub(1);
    }

    fn span(&self, from: usize, to: usize) -> usize {
        self.widths[from..=to].iter().sum::<usize>() + GAP * (to - from)
    }

    /// "12 / 20,000" style position, with "~" for estimates and "+" while open-ended.
    fn position(&self) -> String {
        let rows = self.rows();
        if rows == 0 {
            return String::new();
        }
        let total = match (self.end, self.count) {
            (Some(end), _) => text::thousands(end as u64),
            (None, Count::Exact(n)) => text::thousands(n),
            (None, Count::Estimate(n)) if n as usize >= self.known_rows => format!("~{}", text::compact(n)),
            _ => format!("{}+", text::thousands(self.known_rows as u64)),
        };
        format!("{} / {total}", text::thousands(self.row as u64 + 1))
    }

    pub fn render(&mut self, frame: &mut Frame, area: Rect, focused: bool, spinner: &str) {
        let mut title = vec![Span::raw(" ")];
        if let Some(from) = &self.came_from {
            title.push(Span::styled(format!("{from} › "), theme::muted()));
        }
        if !self.relation.schema.is_empty() && self.relation.schema != "public" {
            title.push(Span::styled(format!("{}.", self.relation.schema), theme::muted()));
        }
        title.push(Span::styled(self.relation.name.clone(), theme::strong()));
        if let Some(note) = &self.note {
            title.push(Span::styled(format!("  {note}"), theme::muted()));
        }
        title.push(Span::raw(" "));
        if self.is_busy() {
            title.push(Span::styled(format!("{spinner} "), theme::accent()));
        }
        let mut block = Block::bordered()
            .border_type(BorderType::Rounded)
            .border_style(theme::border(focused))
            .title(Line::from(title));
        if let Some(column) = self.columns.get(self.col) {
            let mut info = vec![
                Span::raw(" "),
                Span::styled(column.name.clone(), theme::strong()),
                Span::styled(format!("  {}", column.type_name), theme::muted()),
            ];
            if column.primary_key {
                info.push(Span::styled("  primary key", theme::muted()));
            }
            if let Some(link) = self.link_out() {
                info.push(Span::styled(format!("  → {}", link.table), theme::muted()));
            }
            let incoming = self.links_in();
            if !incoming.is_empty() {
                let mut names: Vec<&str> = incoming.iter().map(|l| l.table.as_str()).collect();
                names.dedup();
                let more = names.len().saturating_sub(2);
                let mut list = names.into_iter().take(2).collect::<Vec<_>>().join(", ");
                if more > 0 {
                    list.push_str(&format!(" +{more}"));
                }
                info.push(Span::styled(format!("  ← {list}"), theme::muted()));
            }
            info.push(Span::raw(" "));
            block = block.title_bottom(Line::from(info));
        }
        let position = self.position();
        if !position.is_empty() {
            block = block.title_bottom(Line::from(format!(" {position} ")).style(theme::muted()).right_aligned());
        }
        let mut inner = block.inner(area);
        frame.render_widget(block, area);

        // The filter bar takes the bottom line while a filter is set or being edited.
        if (self.filter_input.is_some() || !self.view.filter.is_empty()) && inner.height > 3 {
            inner.height -= 1;
            let y = inner.bottom();
            let editing = self.filter_input.is_some();
            frame.buffer_mut().set_string(inner.x + 1, y, "/", if editing { theme::accent() } else { theme::muted() });
            let tag = match self.view.filter() {
                _ if editing => {
                    let text = &self.filter_input.as_ref().map(|(i, _)| i.text.clone()).unwrap_or_default();
                    if text.trim().is_empty() {
                        ""
                    } else if crate::db::looks_like_condition(text) {
                        "sql"
                    } else {
                        "search"
                    }
                }
                Filter::All => "",
                Filter::Search(_) => "search",
                Filter::Condition(_) => "sql",
            };
            let field = Rect { x: inner.x + 3, y, width: inner.width.saturating_sub(5 + tag.len() as u16), height: 1 };
            match &self.filter_input {
                Some((input, _)) => {
                    input.render(frame, field, "Search rows, or write a condition like  total > 100", false, focused)
                }
                None => {
                    let shown = text::fit(&self.view.filter, field.width as usize);
                    frame.buffer_mut().set_string(field.x, y, shown, Style::new());
                }
            }
            if !tag.is_empty() {
                frame.buffer_mut().set_string(inner.right() - 1 - tag.len() as u16, y, tag, theme::muted());
            }
        }

        if let Status::Failed(message) = &self.status {
            let lines = text::wrap(message, inner.width.saturating_sub(2) as usize);
            for (i, line) in lines.iter().take(inner.height as usize).enumerate() {
                frame.buffer_mut().set_string(inner.x + 1, inner.y + i as u16, line, theme::error());
            }
            return;
        }
        if self.columns.is_empty() || inner.height < 3 || inner.width < 10 {
            return;
        }

        let rows = self.rows();
        // A narrow gutter holds the current-row marker and the scrolled-left hint.
        let data_x = inner.x as usize + 2;
        let right = inner.right() as usize - 1;
        let avail = right.saturating_sub(data_x);
        let header_y = inner.y;
        let body_y = inner.y + 2;
        self.viewport = (inner.height - 2) as usize;

        // Keep the cursor in view.
        if self.col < self.col_offset {
            self.col_offset = self.col;
        }
        while self.col_offset < self.col && self.span(self.col_offset, self.col) > avail {
            self.col_offset += 1;
        }
        if self.row < self.row_offset {
            self.row_offset = self.row;
        } else if self.row >= self.row_offset + self.viewport {
            self.row_offset = self.row + 1 - self.viewport;
        }
        self.row_offset = self.row_offset.min(rows.saturating_sub(self.viewport));

        // Visible columns as (index, x, width). Only whole columns are shown, so
        // a cut never looks like truncated data; a column wider than the whole
        // view is the exception and gets clipped.
        let mut visible = Vec::new();
        let mut x = data_x;
        for c in self.col_offset..self.columns.len() {
            if x + self.widths[c] > right {
                if visible.is_empty() {
                    visible.push((c, x as u16, right.saturating_sub(x)));
                }
                break;
            }
            visible.push((c, x as u16, self.widths[c]));
            x += self.widths[c] + GAP;
        }
        let more_right = visible.last().is_some_and(|&(c, _, _)| c + 1 < self.columns.len());

        let buf = frame.buffer_mut();
        for &(c, x, width) in &visible {
            let column = &self.columns[c];
            let style = if c == self.col && focused { theme::accent().patch(theme::strong()) } else { theme::strong() };
            let label = match &self.view.sort {
                Some(sort) if sort.column == column.name => {
                    format!("{} {}", column.name, if sort.descending { "↓" } else { "↑" })
                }
                _ => column.name.clone(),
            };
            let label = text::fit_aligned(&label, width, column.kind == ValueKind::Number);
            buf.set_string(x, header_y, label, style);
        }
        if self.col_offset > 0 {
            buf.set_string(inner.x, header_y, "‹", theme::muted());
        }
        if more_right {
            buf.set_string(right as u16, header_y, "›", theme::muted());
        }
        buf.set_string(inner.x, header_y + 1, "─".repeat(inner.width as usize), theme::muted());

        if rows == 0 {
            if self.end == Some(0) || self.count == Count::Exact(0) {
                let message = if self.view.filter.is_empty() { "No rows" } else { "No rows match the filter" };
                buf.set_string(data_x as u16, body_y, message, theme::muted());
            }
            return;
        }
        let search = match self.view.filter() {
            Filter::Search(words) => Some(words.to_owned()),
            _ => None,
        };

        for i in 0..self.viewport {
            let r = self.row_offset + i;
            if r >= rows {
                break;
            }
            let y = body_y + i as u16;
            let is_cursor_row = r == self.row;
            if is_cursor_row {
                let style = if focused { theme::accent() } else { theme::muted() };
                buf.set_string(inner.x, y, "▌", style);
            }

            let Some(data) = self.row_data(r) else { continue };
            for &(c, x, width) in &visible {
                let column = &self.columns[c];
                let right_align = column.kind == ValueKind::Number;
                let value = data.get(c).and_then(Option::as_deref);
                let content = value.map_or(Cow::Borrowed("null"), |v| display(v, column.kind));
                let selected = is_cursor_row && c == self.col && focused;
                let style = match (selected, value) {
                    (true, None) => theme::selection().add_modifier(Modifier::ITALIC),
                    (true, Some(_)) => theme::selection(),
                    (false, None) => theme::null(),
                    (false, Some(_)) => Style::new(),
                };
                let fitted = text::fit(&content, width);
                let hits = match (&search, value) {
                    (Some(words), Some(_)) => text::find_all(&fitted, words),
                    _ => Vec::new(),
                };
                if hits.is_empty() {
                    buf.set_string(x, y, text::fit_aligned(&content, width, right_align), style);
                } else {
                    // Fill the cell (for the selection colour), then draw the text with its matches.
                    buf.set_string(x, y, " ".repeat(width), style);
                    let pad = width.saturating_sub(text::measure(&fitted, width));
                    let text_x = if right_align { x + pad as u16 } else { x };
                    let hl = if selected {
                        Style::new().add_modifier(Modifier::UNDERLINED | Modifier::BOLD)
                    } else {
                        theme::matched()
                    };
                    let line = Line::from(text::highlight(&fitted, &hits, style, hl));
                    buf.set_line(text_x, y, &line, (width - pad) as u16);
                }
            }
        }

        // Scrollbar thumb drawn over the right border.
        if rows > self.viewport && self.viewport > 0 {
            let track = self.viewport;
            let thumb = (track * track / rows).clamp(1, track);
            let max_offset = rows - self.viewport;
            let top = (track - thumb) * self.row_offset / max_offset.max(1);
            let style = if focused { theme::accent() } else { Style::new() };
            for i in 0..thumb {
                buf.set_string(inner.right(), body_y + (top + i) as u16, "┃", style);
            }
        }
    }
}

/// How a value is shown in a cell and copied.
pub fn display(value: &str, kind: ValueKind) -> Cow<'_, str> {
    match (kind, value) {
        (ValueKind::Bool, "t") => Cow::Borrowed("true"),
        (ValueKind::Bool, "f") => Cow::Borrowed("false"),
        _ => Cow::Borrowed(value),
    }
}

/// The value as shown in full, for the value viewer and the clipboard.
pub fn full_value(value: &str, kind: ValueKind) -> String {
    match kind {
        ValueKind::Json => serde_json::from_str::<serde_json::Value>(value)
            .and_then(|v| serde_json::to_string_pretty(&v))
            .unwrap_or_else(|_| value.to_owned()),
        _ => display(value, kind).into_owned(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::{RelationKind, TableInfo};

    #[test]
    fn row_json_keeps_every_digit() {
        let relation =
            Relation { schema: "public".into(), name: "t".into(), kind: RelationKind::Table, estimate: None };
        let mut grid = Grid::new(relation, 1, View::default());
        let column =
            |name: &str, kind| Column { name: name.into(), type_name: String::new(), kind, primary_key: false };
        grid.described(TableInfo {
            columns: vec![column("amount", ValueKind::Number), column("ok", ValueKind::Bool)],
            order_by: Vec::new(),
            bytes: 0,
            links_out: Vec::new(),
            links_in: Vec::new(),
        });
        grid.page_loaded(0, vec![vec![Some("12345678901234567890.1234567890".into()), Some("t".into())]]);
        let json = grid.row_json().unwrap();
        assert!(json.contains("\"amount\": 12345678901234567890.1234567890"), "{json}");
        assert!(json.contains("\"ok\": true"), "{json}");
    }
}
