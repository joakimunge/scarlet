//! A fuzzy-filtered list with a search field, shared by the palette and the
//! connection picker.

use ratatui::{Frame, layout::Rect, text::Line};

use super::{fuzzy::Fuzzy, input::Input, text, theme};

pub struct Match {
    pub index: usize,
    pub highlights: Vec<u32>,
}

/// What one row shows: the label is matched and highlighted, the detail and
/// the right-aligned tag are secondary.
pub struct Entry<'a> {
    pub label: &'a str,
    pub detail: &'a str,
    pub tag: &'a str,
}

#[derive(Default)]
pub struct FilterList {
    pub input: Input,
    pub matches: Vec<Match>,
    cursor: usize,
    offset: usize,
    viewport: usize,
    fuzzy: Fuzzy,
}

impl FilterList {
    /// Re-runs the search over `haystacks`. With `keep` the selected item stays
    /// selected (used when items arrive); otherwise the best match is selected.
    pub fn refilter<S: AsRef<str>>(&mut self, haystacks: &[S], keep: Option<usize>) {
        let query = self.input.text.trim();
        if query.is_empty() {
            self.matches = (0..haystacks.len()).map(|index| Match { index, highlights: Vec::new() }).collect();
        } else {
            let pattern = Fuzzy::pattern(query);
            let mut scored: Vec<(u32, Match)> = haystacks
                .iter()
                .enumerate()
                .filter_map(|(index, text)| {
                    let (score, highlights) = self.fuzzy.score(&pattern, text.as_ref())?;
                    Some((score, Match { index, highlights }))
                })
                .collect();
            scored.sort_by_key(|(score, _)| std::cmp::Reverse(*score));
            self.matches = scored.into_iter().map(|(_, m)| m).collect();
        }
        self.cursor = keep.and_then(|index| self.matches.iter().position(|m| m.index == index)).unwrap_or(0);
    }

    pub fn selected(&self) -> Option<usize> {
        self.matches.get(self.cursor).map(|m| m.index)
    }

    pub fn move_by(&mut self, delta: isize) {
        let last = self.matches.len().saturating_sub(1);
        self.cursor = self.cursor.saturating_add_signed(delta).min(last);
    }

    pub fn page(&mut self, direction: isize) {
        self.move_by(direction * self.viewport.saturating_sub(1).max(1) as isize);
    }

    pub fn len(&self) -> usize {
        self.matches.len()
    }

    pub fn render<'a>(&mut self, frame: &mut Frame, area: Rect, entry: impl Fn(usize) -> Entry<'a>) {
        self.viewport = area.height as usize;
        if self.cursor < self.offset {
            self.offset = self.cursor;
        } else if self.viewport > 0 && self.cursor >= self.offset + self.viewport {
            self.offset = self.cursor + 1 - self.viewport;
        }
        self.offset = self.offset.min(self.matches.len().saturating_sub(self.viewport));

        let width = area.width as usize;
        // Details line up in one column after the longest visible label.
        let label_column = self
            .matches
            .iter()
            .skip(self.offset)
            .take(self.viewport)
            .map(|m| text::measure(entry(m.index).label, width / 2))
            .max()
            .unwrap_or(0);
        for (row, m) in self.matches.iter().enumerate().skip(self.offset).take(self.viewport) {
            let y = area.y + (row - self.offset) as u16;
            let selected = row == self.cursor;
            let line_style = if selected { theme::selection() } else { Default::default() };
            let secondary = if selected { line_style } else { theme::muted() };
            let Entry { label, detail, tag } = entry(m.index);
            let buf = frame.buffer_mut();
            buf.set_style(Rect { x: area.x, y, width: area.width, height: 1 }, line_style);

            let tag_width = text::measure(tag, width);
            let room = width.saturating_sub(2 + tag_width + if tag_width > 0 { 2 } else { 0 });
            let label_text = text::fit(label, room);
            let label_width = text::measure(&label_text, room);
            let hl = if selected { theme::strong() } else { theme::matched() };
            let line = Line::from(text::highlight(&label_text, &m.highlights, line_style, hl));
            buf.set_line(area.x + 1, y, &line, room as u16);
            let detail_x = label_column.max(label_width) + 3;
            let detail_room = room.saturating_sub(detail_x - 1);
            if !detail.is_empty() && detail_room > 1 {
                buf.set_string(area.x + detail_x as u16, y, text::fit(detail, detail_room), secondary);
            }
            if tag_width > 0 {
                buf.set_string(area.right() - 1 - tag_width as u16, y, text::fit(tag, tag_width), secondary);
            }
        }
    }
}
