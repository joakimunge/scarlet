//! A single-line text field with the editing keys people expect from a shell.

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui::{Frame, layout::Rect, text::Span};
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

use super::theme;

#[derive(Default)]
pub struct Input {
    pub text: String,
    /// Byte offset into `text`.
    cursor: usize,
}

impl Input {
    pub fn is_empty(&self) -> bool {
        self.text.is_empty()
    }

    /// Replaces the text, with the cursor at the end.
    pub fn set(&mut self, text: &str) {
        self.text = text.to_owned();
        self.cursor = self.text.len();
    }

    pub fn clear(&mut self) {
        self.text.clear();
        self.cursor = 0;
    }

    pub fn insert_str(&mut self, s: &str) {
        let clean: String = s.chars().filter(|c| !c.is_control()).collect();
        self.text.insert_str(self.cursor, &clean);
        self.cursor += clean.len();
    }

    /// Applies an editing key. Returns whether it was consumed.
    pub fn handle(&mut self, key: KeyEvent) -> bool {
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        let alt = key.modifiers.contains(KeyModifiers::ALT);
        match key.code {
            KeyCode::Char(c) if !ctrl && !alt => self.insert_str(c.encode_utf8(&mut [0; 4])),
            KeyCode::Char('a') if ctrl => self.cursor = 0,
            KeyCode::Char('e') if ctrl => self.cursor = self.text.len(),
            KeyCode::Char('u') if ctrl => {
                self.text.drain(..self.cursor);
                self.cursor = 0;
            }
            KeyCode::Char('w') if ctrl => self.delete_word(),
            KeyCode::Backspace if ctrl || alt => self.delete_word(),
            KeyCode::Backspace => {
                if let Some(c) = self.text[..self.cursor].chars().next_back() {
                    self.cursor -= c.len_utf8();
                    self.text.remove(self.cursor);
                }
            }
            KeyCode::Delete => {
                if self.cursor < self.text.len() {
                    self.text.remove(self.cursor);
                }
            }
            KeyCode::Left => {
                if let Some(c) = self.text[..self.cursor].chars().next_back() {
                    self.cursor -= c.len_utf8();
                }
            }
            KeyCode::Right => {
                if let Some(c) = self.text[self.cursor..].chars().next() {
                    self.cursor += c.len_utf8();
                }
            }
            KeyCode::Home => self.cursor = 0,
            KeyCode::End => self.cursor = self.text.len(),
            _ => return false,
        }
        true
    }

    fn delete_word(&mut self) {
        let before = self.text[..self.cursor].trim_end();
        let start = before.char_indices().rev().find(|(_, c)| c.is_whitespace()).map_or(0, |(i, c)| i + c.len_utf8());
        self.text.drain(start..self.cursor);
        self.cursor = start;
    }

    /// Draws the text, scrolled so the cursor stays visible, and places the
    /// terminal cursor when `focused`. `mask` hides the characters (passwords).
    pub fn render(&self, frame: &mut Frame, area: Rect, placeholder: &str, mask: bool, focused: bool) {
        if area.width == 0 {
            return;
        }
        let shown = if mask { "•".repeat(self.text.chars().count()) } else { self.text.clone() };
        let before = if mask { self.text[..self.cursor].chars().count() } else { self.text[..self.cursor].width() };
        let width = area.width as usize;
        let skip = before.saturating_sub(width - 1);

        if self.text.is_empty() {
            frame.buffer_mut().set_stringn(area.x, area.y, placeholder, width, theme::muted());
        } else {
            let mut skipped = 0;
            let visible: String = shown
                .chars()
                .skip_while(|c| {
                    let skip_it = skipped < skip;
                    skipped += c.width().unwrap_or(0);
                    skip_it
                })
                .collect();
            let span = Span::raw(visible);
            frame.buffer_mut().set_span(area.x, area.y, &span, area.width);
        }
        if focused {
            frame.set_cursor_position((area.x + (before - skip) as u16, area.y));
        }
    }
}
