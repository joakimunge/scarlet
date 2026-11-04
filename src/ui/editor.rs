//! The SQL editor: a plain, non-modal multi-line text field with the keys
//! people expect (word moves, undo, paste), a gutter that marks the statement
//! `ctrl+r` will run, and a completion popup.

use std::ops::Range;

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui::{
    Frame,
    layout::Rect,
    style::Style,
    text::{Line, Span},
    widgets::{Block, BorderType, Clear},
};
use unicode_width::UnicodeWidthChar;

use super::{text, theme};

const INDENT: &str = "  ";
const UNDO_LIMIT: usize = 200;
const POPUP_ROWS: usize = 8;

/// One completion offered in the popup.
pub struct Suggestion {
    /// What gets inserted.
    pub text: String,
    /// Shown dimmed on the right: "table", "bigint", "keyword".
    pub detail: String,
    /// Matched character positions in `text`, for highlighting.
    pub highlights: Vec<u32>,
}

pub struct Popup {
    pub items: Vec<Suggestion>,
    pub selected: usize,
    /// Byte offset in the text where the word being completed starts.
    pub start: usize,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Edit {
    Insert,
    Delete,
    Other,
}

#[derive(Clone)]
struct Snapshot {
    lines: Vec<String>,
    row: usize,
    col: usize,
}

/// What a key did, so the caller knows whether to refresh completions.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Outcome {
    Edited,
    Moved,
    Ignored,
}

pub struct Editor {
    lines: Vec<String>,
    row: usize,
    /// Byte offset into the current line, always on a character boundary.
    col: usize,
    /// Display column kept while moving up and down through shorter lines.
    goal: Option<usize>,
    scroll_row: usize,
    scroll_col: usize,
    viewport: usize,
    undo: Vec<Snapshot>,
    redo: Vec<Snapshot>,
    last_edit: Edit,
    pub popup: Option<Popup>,
}

impl Editor {
    pub fn new(text: &str) -> Editor {
        let mut editor = Editor {
            lines: vec![String::new()],
            row: 0,
            col: 0,
            goal: None,
            scroll_row: 0,
            scroll_col: 0,
            viewport: 10,
            undo: Vec::new(),
            redo: Vec::new(),
            last_edit: Edit::Other,
            popup: None,
        };
        editor.lines = clean(text).split('\n').map(str::to_owned).collect();
        editor.row = editor.lines.len() - 1;
        editor.col = editor.lines[editor.row].len();
        editor
    }

    pub fn text(&self) -> String {
        self.lines.join("\n")
    }

    pub fn is_blank(&self) -> bool {
        self.lines.iter().all(|l| l.trim().is_empty())
    }

    /// The cursor as a byte offset into `text()`.
    pub fn cursor(&self) -> usize {
        self.lines[..self.row].iter().map(|l| l.len() + 1).sum::<usize>() + self.col
    }

    fn set_cursor(&mut self, offset: usize) {
        let mut remaining = offset;
        for (row, line) in self.lines.iter().enumerate() {
            if remaining <= line.len() {
                self.row = row;
                self.col = floor_boundary(line, remaining);
                return;
            }
            remaining -= line.len() + 1;
        }
        self.row = self.lines.len() - 1;
        self.col = self.lines[self.row].len();
    }

    /// Replaces the whole text (an external edit), undoably.
    pub fn set_text(&mut self, text: &str) {
        self.checkpoint(Edit::Other);
        let cursor = self.cursor();
        self.lines = clean(text).split('\n').map(str::to_owned).collect();
        self.set_cursor(cursor);
    }

    /// Replaces `range` of the text with `with` and puts the cursor after it.
    pub fn replace(&mut self, range: Range<usize>, with: &str) {
        self.checkpoint(Edit::Other);
        let mut text = self.text();
        let end = range.end.min(text.len());
        let start = range.start.min(end);
        text.replace_range(start..end, with);
        self.lines = text.split('\n').map(str::to_owned).collect();
        self.set_cursor(start + with.len());
        self.goal = None;
    }

    pub fn insert_str(&mut self, s: &str) {
        self.checkpoint(Edit::Other);
        self.insert(&clean(s));
    }

    fn insert(&mut self, s: &str) {
        let rest = self.lines[self.row].split_off(self.col);
        let mut parts = s.split('\n');
        self.lines[self.row].push_str(parts.next().unwrap_or_default());
        for part in parts {
            self.row += 1;
            self.lines.insert(self.row, part.to_owned());
        }
        self.col = self.lines[self.row].len();
        self.lines[self.row].push_str(&rest);
        self.goal = None;
    }

    /// Saves an undo point, merging runs of the same kind of edit.
    fn checkpoint(&mut self, edit: Edit) {
        if edit == Edit::Other || edit != self.last_edit {
            self.undo.push(Snapshot { lines: self.lines.clone(), row: self.row, col: self.col });
            if self.undo.len() > UNDO_LIMIT {
                self.undo.remove(0);
            }
            self.redo.clear();
        }
        self.last_edit = edit;
    }

    fn restore(&mut self, from_undo: bool) {
        let (source, target) =
            if from_undo { (&mut self.undo, &mut self.redo) } else { (&mut self.redo, &mut self.undo) };
        let Some(snapshot) = source.pop() else { return };
        target.push(Snapshot { lines: self.lines.clone(), row: self.row, col: self.col });
        self.lines = snapshot.lines;
        self.row = snapshot.row.min(self.lines.len() - 1);
        self.col = floor_boundary(&self.lines[self.row], snapshot.col);
        self.last_edit = Edit::Other;
        self.goal = None;
    }

    pub fn handle(&mut self, key: KeyEvent) -> Outcome {
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        let alt = key.modifiers.contains(KeyModifiers::ALT);
        let line_len = self.lines[self.row].len();
        match key.code {
            KeyCode::Char('z') if ctrl => self.restore(true),
            KeyCode::Char('y') if ctrl => self.restore(false),
            KeyCode::Char('a') if ctrl => self.col = 0,
            KeyCode::Char('u') if ctrl => {
                self.checkpoint(Edit::Other);
                self.lines[self.row].drain(..self.col);
                self.col = 0;
            }
            KeyCode::Char('w') if ctrl => self.delete_word(),
            KeyCode::Backspace if ctrl || alt => self.delete_word(),
            KeyCode::Char('b') if alt => self.word_left(),
            KeyCode::Char('f') if alt => self.word_right(),
            KeyCode::Left if ctrl || alt => self.word_left(),
            KeyCode::Right if ctrl || alt => self.word_right(),
            KeyCode::Char(c) if !ctrl && !alt => {
                // Words undo as a unit: a new undo point starts at each space.
                self.checkpoint(if c.is_whitespace() { Edit::Other } else { Edit::Insert });
                self.insert(c.encode_utf8(&mut [0; 4]));
                return Outcome::Edited;
            }
            KeyCode::Enter => {
                self.checkpoint(Edit::Other);
                let line = &self.lines[self.row];
                let indent: String = line.chars().take_while(|c| *c == ' ').collect();
                let indent = &indent[..indent.len().min(self.col)];
                self.insert(&format!("\n{indent}"));
            }
            KeyCode::Tab => {
                self.checkpoint(Edit::Other);
                self.insert(INDENT);
            }
            KeyCode::Backspace => {
                self.checkpoint(Edit::Delete);
                if self.col > 0 {
                    let line = &mut self.lines[self.row];
                    let previous = line[..self.col].chars().next_back().map_or(1, char::len_utf8);
                    self.col -= previous;
                    line.remove(self.col);
                } else if self.row > 0 {
                    let line = self.lines.remove(self.row);
                    self.row -= 1;
                    self.col = self.lines[self.row].len();
                    self.lines[self.row].push_str(&line);
                } else {
                    return Outcome::Ignored;
                }
            }
            KeyCode::Delete => {
                self.checkpoint(Edit::Delete);
                if self.col < line_len {
                    self.lines[self.row].remove(self.col);
                } else if self.row + 1 < self.lines.len() {
                    let next = self.lines.remove(self.row + 1);
                    self.lines[self.row].push_str(&next);
                } else {
                    return Outcome::Ignored;
                }
            }
            KeyCode::Left => {
                if let Some(c) = self.lines[self.row][..self.col].chars().next_back() {
                    self.col -= c.len_utf8();
                } else if self.row > 0 {
                    self.row -= 1;
                    self.col = self.lines[self.row].len();
                }
                self.goal = None;
                return Outcome::Moved;
            }
            KeyCode::Right => {
                if let Some(c) = self.lines[self.row][self.col..].chars().next() {
                    self.col += c.len_utf8();
                } else if self.row + 1 < self.lines.len() {
                    self.row += 1;
                    self.col = 0;
                }
                self.goal = None;
                return Outcome::Moved;
            }
            KeyCode::Up => return self.vertical(-1),
            KeyCode::Down => return self.vertical(1),
            KeyCode::PageUp => return self.vertical(-(self.viewport.max(2) as isize - 1)),
            KeyCode::PageDown => return self.vertical(self.viewport.max(2) as isize - 1),
            KeyCode::Home if ctrl => {
                (self.row, self.col, self.goal) = (0, 0, None);
                return Outcome::Moved;
            }
            KeyCode::End if ctrl => {
                self.row = self.lines.len() - 1;
                (self.col, self.goal) = (self.lines[self.row].len(), None);
                return Outcome::Moved;
            }
            KeyCode::Home => {
                // First press goes to the text, the second to the very start.
                let indent = self.lines[self.row].len() - self.lines[self.row].trim_start().len();
                self.col = if self.col == indent { 0 } else { indent };
                self.goal = None;
                return Outcome::Moved;
            }
            KeyCode::End => {
                self.col = line_len;
                self.goal = None;
                return Outcome::Moved;
            }
            _ => return Outcome::Ignored,
        }
        self.goal = None;
        Outcome::Edited
    }

    fn vertical(&mut self, delta: isize) -> Outcome {
        let goal = *self.goal.get_or_insert_with(|| display_width(&self.lines[self.row][..self.col]));
        let target = self.row.saturating_add_signed(delta).min(self.lines.len() - 1);
        if target == self.row {
            return Outcome::Ignored;
        }
        self.row = target;
        self.col = byte_at_column(&self.lines[self.row], goal);
        Outcome::Moved
    }

    fn word_left(&mut self) {
        if self.col == 0 {
            if self.row > 0 {
                self.row -= 1;
                self.col = self.lines[self.row].len();
            }
            return;
        }
        let before = &self.lines[self.row][..self.col];
        let trimmed = before.trim_end_matches(|c: char| !is_word(c));
        self.col = trimmed
            .rfind(|c: char| !is_word(c))
            .map_or(0, |i| i + trimmed[i..].chars().next().map_or(1, char::len_utf8));
        self.goal = None;
    }

    fn word_right(&mut self) {
        let line = &self.lines[self.row];
        if self.col == line.len() {
            if self.row + 1 < self.lines.len() {
                self.row += 1;
                self.col = 0;
            }
            return;
        }
        let after = &line[self.col..];
        let skip = after.find(is_word).unwrap_or(after.len());
        let word_end = after[skip..].find(|c: char| !is_word(c)).map_or(after.len(), |i| skip + i);
        self.col += word_end;
        self.goal = None;
    }

    fn delete_word(&mut self) {
        self.checkpoint(Edit::Other);
        let end = self.col;
        self.word_left();
        if self.col < end && self.row < self.lines.len() {
            self.lines[self.row].drain(self.col..end);
        }
    }

    /// Draws the editor. `styles` colours byte ranges of the text (sorted, not
    /// overlapping); `statement` is the range `ctrl+r` would run.
    pub fn render(
        &mut self,
        frame: &mut Frame,
        area: Rect,
        focused: bool,
        styles: &[(Range<usize>, Style)],
        statement: Option<Range<usize>>,
    ) {
        let block = Block::bordered()
            .border_type(BorderType::Rounded)
            .border_style(theme::border(focused))
            .title(Line::from(vec![Span::raw(" "), Span::styled("SQL", theme::strong()), Span::raw(" ")]));
        let inner = block.inner(area);
        frame.render_widget(block, area);
        if inner.height == 0 || inner.width < 8 {
            return;
        }

        let number_width = self.lines.len().to_string().len().max(2);
        let text_x = inner.x + number_width as u16 + 2;
        let text_width = inner.right().saturating_sub(text_x + 1) as usize;
        self.viewport = inner.height as usize;

        // Keep the cursor in view.
        if self.row < self.scroll_row {
            self.scroll_row = self.row;
        } else if self.row >= self.scroll_row + self.viewport {
            self.scroll_row = self.row + 1 - self.viewport;
        }
        let cursor_x = display_width(&self.lines[self.row][..self.col]);
        // Scrolled sideways only while the cursor would be out of sight otherwise.
        if cursor_x < text_width {
            self.scroll_col = 0;
        } else if cursor_x < self.scroll_col {
            self.scroll_col = cursor_x;
        } else if text_width > 0 && cursor_x >= self.scroll_col + text_width {
            self.scroll_col = cursor_x + 1 - text_width;
        }

        if self.is_blank() && !focused {
            frame.buffer_mut().set_string(text_x, inner.y, "ctrl+e to write SQL", theme::muted());
        } else if self.is_blank() {
            let hint = "Write SQL. ctrl+r runs the statement under the cursor.";
            frame.buffer_mut().set_stringn(text_x, inner.y, hint, text_width, theme::muted());
        }

        let mut line_start = self.lines[..self.scroll_row].iter().map(|l| l.len() + 1).sum::<usize>();
        let mut style_index = 0;
        for (row, line) in self.lines.iter().enumerate().skip(self.scroll_row).take(self.viewport) {
            let y = inner.y + (row - self.scroll_row) as u16;
            let line_end = line_start + line.len();
            let number_style = if row == self.row && focused { theme::accent() } else { theme::muted() };
            let buf = frame.buffer_mut();
            buf.set_string(inner.x + 1, y, format!("{:>number_width$}", row + 1), number_style);
            let in_statement = statement.as_ref().is_some_and(|s| s.start <= line_end && s.end >= line_start)
                && !line.trim().is_empty();
            if in_statement {
                buf.set_string(text_x - 1, y, "▎", if focused { theme::accent() } else { theme::muted() });
            }

            // Characters with their styles, scrolled horizontally.
            while style_index < styles.len() && styles[style_index].0.end <= line_start {
                style_index += 1;
            }
            let mut column = 0;
            let mut x = text_x;
            for (offset, c) in line.char_indices() {
                let width = c.width().unwrap_or(0);
                if column >= self.scroll_col {
                    if (x - text_x) as usize + width > text_width {
                        break;
                    }
                    let at = line_start + offset;
                    let style = styles[style_index..]
                        .iter()
                        .take_while(|(range, _)| range.start <= at)
                        .find(|(range, _)| range.contains(&at))
                        .map_or(Style::new(), |(_, style)| *style);
                    buf.set_string(x, y, c.encode_utf8(&mut [0; 4]), style);
                    x += width as u16;
                }
                column += width;
            }
            line_start = line_end + 1;
        }

        if focused {
            let x = text_x + (cursor_x - self.scroll_col) as u16;
            let y = inner.y + (self.row - self.scroll_row) as u16;
            frame.set_cursor_position((x, y));
            let typed = self.popup.as_ref().map_or(0, |popup| {
                let (text, cursor) = (self.text(), self.cursor());
                text.get(popup.start.min(cursor)..cursor).map_or(0, display_width)
            });
            self.render_popup(frame, area, x.saturating_sub(typed as u16), y);
        }
    }

    /// Draws the completion popup with its text aligned under the word at `word_x`.
    fn render_popup(&self, frame: &mut Frame, bounds: Rect, word_x: u16, y: u16) {
        let Some(popup) = &self.popup else { return };
        if popup.items.is_empty() {
            return;
        }
        let rows = popup.items.len().min(POPUP_ROWS);
        let label_width = popup.items.iter().map(|s| text::measure(&s.text, 40)).max().unwrap_or(0);
        let detail_width = popup.items.iter().map(|s| text::measure(&s.detail, 24)).max().unwrap_or(0);
        let width = (label_width + detail_width + 5) as u16;
        let screen = frame.area();
        let width = width.min(screen.width.saturating_sub(2));
        let height = rows as u16 + 2;
        // Below the cursor when it fits, otherwise above.
        let px = word_x.saturating_sub(2).min(screen.right().saturating_sub(width));
        let py = if y + 1 + height <= screen.bottom().saturating_sub(1) { y + 1 } else { y.saturating_sub(height) };
        let rect = Rect { x: px, y: py.max(bounds.y), width, height };
        frame.render_widget(Clear, rect);
        let block = Block::bordered().border_type(BorderType::Rounded).border_style(theme::muted());
        let inner = block.inner(rect);
        frame.render_widget(block, rect);

        let offset = popup.selected.saturating_sub(rows - 1);
        let buf = frame.buffer_mut();
        for (i, item) in popup.items.iter().enumerate().skip(offset).take(rows) {
            let y = inner.y + (i - offset) as u16;
            let selected = i == popup.selected;
            let base = if selected { theme::selection() } else { Style::new() };
            buf.set_style(Rect { x: inner.x, y, width: inner.width, height: 1 }, base);
            let hl = if selected { theme::strong() } else { theme::matched() };
            let label = text::fit(&item.text, label_width);
            let line = Line::from(text::highlight(&label, &item.highlights, base, hl));
            buf.set_line(inner.x + 1, y, &line, label_width as u16);
            let detail = text::fit(&item.detail, detail_width);
            let dx = inner.right().saturating_sub(text::measure(&detail, 99) as u16 + 1);
            buf.set_string(dx, y, detail, if selected { base } else { theme::muted() });
        }
    }
}

fn is_word(c: char) -> bool {
    c.is_alphanumeric() || c == '_' || c == '$'
}

/// Normalises pasted or loaded text: no carriage returns, tabs as spaces.
fn clean(text: &str) -> String {
    text.replace("\r\n", "\n").replace('\r', "\n").replace('\t', "    ")
}

fn display_width(s: &str) -> usize {
    s.chars().map(|c| c.width().unwrap_or(0)).sum()
}

/// The byte offset in `line` at display column `column` (or the end).
fn byte_at_column(line: &str, column: usize) -> usize {
    let mut width = 0;
    for (offset, c) in line.char_indices() {
        if width >= column {
            return offset;
        }
        width += c.width().unwrap_or(0);
    }
    line.len()
}

fn floor_boundary(line: &str, offset: usize) -> usize {
    let mut offset = offset.min(line.len());
    while !line.is_char_boundary(offset) {
        offset -= 1;
    }
    offset
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }

    fn type_text(editor: &mut Editor, text: &str) {
        for c in text.chars() {
            editor.handle(key(if c == '\n' { KeyCode::Enter } else { KeyCode::Char(c) }));
        }
    }

    #[test]
    fn keeps_indentation_and_undoes_by_word() {
        let mut editor = Editor::new("");
        type_text(&mut editor, "select *\n  from orders\nwhere");
        assert_eq!(editor.text(), "select *\n  from orders\n  where");
        editor.handle(KeyEvent::new(KeyCode::Char('z'), KeyModifiers::CONTROL));
        assert_eq!(editor.text(), "select *\n  from orders\n  ");
    }

    #[test]
    fn moves_by_words_and_joins_lines() {
        let mut editor = Editor::new("select id\nfrom t");
        editor.handle(KeyEvent::new(KeyCode::Left, KeyModifiers::ALT));
        assert_eq!(editor.cursor(), "select id\nfrom ".len());
        editor.handle(key(KeyCode::Home));
        editor.handle(key(KeyCode::Backspace));
        assert_eq!(editor.text(), "select idfrom t");
    }

    #[test]
    fn replaces_ranges_for_completion_with_multibyte_text() {
        let mut editor = Editor::new("select å from ord");
        let end = editor.cursor();
        editor.replace(end - 3..end, "orders");
        assert_eq!(editor.text(), "select å from orders");
        assert_eq!(editor.cursor(), editor.text().len());
    }
}
