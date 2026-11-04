//! The wordmark on the start screen, in scarlet whatever the terminal theme.

use ratatui::{Frame, layout::Rect, style::Style};

use super::theme;

const LETTERS: [[&str; 3]; 7] = [
    ["█▀▀▀", "▀▀▀█", "▀▀▀▀"],
    ["█▀▀▀", "█   ", "▀▀▀▀"],
    ["█▀▀█", "█▀▀█", "▀  ▀"],
    ["█▀▀█", "█▀▀▄", "▀  ▀"],
    ["█   ", "█   ", "▀▀▀▀"],
    ["█▀▀▀", "█▀▀ ", "▀▀▀▀"],
    ["▀█▀", " █ ", " ▀ "],
];

pub const HEIGHT: u16 = 3;

pub fn width() -> u16 {
    LETTERS.iter().map(|letter| letter[0].chars().count() as u16 + 1).sum::<u16>() - 1
}

/// Draws the wordmark centred on `area`'s width, starting at its top row.
pub fn render(frame: &mut Frame, area: Rect) {
    let width = width();
    if area.width < width || area.height < HEIGHT {
        return;
    }
    let buf = frame.buffer_mut();
    let mut x = area.x + (area.width - width) / 2;
    for (index, letter) in LETTERS.iter().enumerate() {
        let letter_width = letter[0].chars().count() as u16;
        for (row, line) in letter.iter().enumerate() {
            for (column, c) in line.chars().enumerate().filter(|(_, c)| *c != ' ') {
                let column = x + column as u16;
                // A smooth sweep in 24-bit colour; otherwise whole letters, so none is split.
                let along = if theme::truecolor() {
                    f32::from(column - area.x - (area.width - width) / 2) / f32::from(width - 1)
                } else {
                    index as f32 / (LETTERS.len() - 1) as f32
                };
                buf.set_string(column, area.y + row as u16, c.to_string(), Style::new().fg(theme::scarlet(along)));
            }
        }
        x += letter_width + 1;
    }
}
