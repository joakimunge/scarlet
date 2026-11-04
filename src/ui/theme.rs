//! Styles drawn only from the terminal's own theme: its default foreground and
//! background plus its ANSI palette, so Scarlet looks native in any colour scheme.

use std::sync::OnceLock;

use ratatui::style::{Color, Modifier, Style};

const ACCENT: Color = Color::Blue;

fn no_color() -> bool {
    static NO_COLOR: OnceLock<bool> = OnceLock::new();
    *NO_COLOR.get_or_init(|| std::env::var_os("NO_COLOR").is_some_and(|v| !v.is_empty()))
}

pub fn accent() -> Style {
    if no_color() { Style::new().add_modifier(Modifier::BOLD) } else { Style::new().fg(ACCENT) }
}

pub fn muted() -> Style {
    Style::new().add_modifier(Modifier::DIM)
}

pub fn strong() -> Style {
    Style::new().add_modifier(Modifier::BOLD)
}

pub fn error() -> Style {
    if no_color() { Style::new().add_modifier(Modifier::BOLD) } else { Style::new().fg(Color::Red) }
}

pub fn null() -> Style {
    muted().add_modifier(Modifier::ITALIC)
}

/// The focused selection: the accent becomes the background, the terminal's
/// background becomes the text colour, which reads well on light and dark themes.
pub fn selection() -> Style {
    accent().add_modifier(Modifier::REVERSED)
}

pub fn border(focused: bool) -> Style {
    if focused { accent() } else { muted() }
}

/// Matched characters in fuzzy search results.
pub fn matched() -> Style {
    accent().add_modifier(Modifier::BOLD)
}

/// SQL keywords in the editor.
pub fn keyword() -> Style {
    accent()
}

pub fn string() -> Style {
    if no_color() { Style::new() } else { Style::new().fg(Color::Green) }
}

pub fn number() -> Style {
    if no_color() { Style::new() } else { Style::new().fg(Color::Magenta) }
}

pub fn comment() -> Style {
    muted().add_modifier(Modifier::ITALIC)
}
