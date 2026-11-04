//! The keyboard reference behind `?`.

use ratatui::{
    Frame,
    layout::Rect,
    style::Style,
    text::{Line, Span},
    widgets::{Block, BorderType, Clear},
};

use super::{text, theme};
use crate::vimkeys::{Motion, VimKeys};

const SECTIONS: &[(&str, &[(&str, &str)])] = &[
    (
        "Move",
        &[
            ("↑ ↓ ← →  or  h j k l", "move"),
            ("page up / down  or  ctrl+u / d", "scroll a page / half a page"),
            ("home end  or  g g  G", "first / last row"),
            ("0  $", "first / last column"),
            ("tab", "switch between tables and data"),
        ],
    ),
    ("Tables", &[("enter  →", "open"), ("/", "filter by name"), ("r", "reload the list")]),
    (
        "Data",
        &[
            ("enter", "follow a link, or view the whole value"),
            ("v", "the whole row"),
            ("/", "filter: words search, SQL is a condition"),
            ("=  !", "only rows with / without this value"),
            ("s", "sort by this column"),
            ("c  y  Y", "copy the value / the row as JSON"),
            ("r", "reload the rows"),
            ("esc  backspace  ctrl+o", "back, clear the filter, to tables"),
        ],
    ),
    (
        "SQL",
        &[
            ("ctrl+e", "open the editor, or go back to tables"),
            ("ctrl+r", "run the statement under the cursor"),
            ("ctrl+space  tab", "complete / accept a suggestion"),
            ("ctrl+z  ctrl+y", "undo / redo"),
            ("ctrl+o", "edit in $EDITOR"),
            ("esc", "to the result, or stop a running query"),
        ],
    ),
    (
        "Anywhere",
        &[
            ("ctrl+k", "jump to a table or run a command"),
            ("space", "what can be done here"),
            ("?", "this list"),
            ("q  ctrl+c", "quit"),
        ],
    ),
];

/// Keys from the user's vim config that differ from Scarlet's defaults.
fn personal_keys(vim: &VimKeys) -> Vec<(String, String)> {
    const DEFAULTS: &[(&str, Motion)] = &[
        ("j", Motion::Down),
        ("k", Motion::Up),
        ("h", Motion::Left),
        ("l", Motion::Right),
        ("G", Motion::Bottom),
        ("0", Motion::FirstColumn),
        ("^", Motion::FirstColumn),
        ("$", Motion::LastColumn),
        ("ctrl+d", Motion::HalfDown),
        ("ctrl+u", Motion::HalfUp),
        ("ctrl+f", Motion::PageDown),
        ("ctrl+b", Motion::PageUp),
        ("↑", Motion::Up),
        ("↓", Motion::Down),
        ("←", Motion::Left),
        ("→", Motion::Right),
        ("page up", Motion::PageUp),
        ("page down", Motion::PageDown),
    ];
    vim.entries()
        .iter()
        .map(|(lhs, motion)| (key_label(lhs), *motion))
        .filter(|(label, motion)| !DEFAULTS.contains(&(label.as_str(), *motion)))
        .map(|(label, motion)| (label, motion_label(motion).to_owned()))
        .collect()
}

/// `<C-D>` -> `ctrl+d`, `<S-Down>` -> `shift+↓`, `€` -> `€`.
fn key_label(lhs: &str) -> String {
    let Some(inner) = lhs.strip_prefix('<').and_then(|l| l.strip_suffix('>')).filter(|i| i.len() > 1) else {
        return lhs.to_owned();
    };
    let (mods, key) = match inner.rsplit_once('-') {
        Some((mods, key)) if !key.is_empty() => (mods, key),
        _ => ("", inner),
    };
    let mut parts: Vec<String> = mods
        .split('-')
        .filter(|m| !m.is_empty())
        .map(|m| match m.to_ascii_uppercase().as_str() {
            "C" => "ctrl".to_owned(),
            "M" | "A" => "alt".to_owned(),
            "S" => "shift".to_owned(),
            "D" => "cmd".to_owned(),
            other => other.to_lowercase(),
        })
        .collect();
    let key = match key.to_ascii_lowercase().as_str() {
        "down" => "↓".to_owned(),
        "up" => "↑".to_owned(),
        "left" => "←".to_owned(),
        "right" => "→".to_owned(),
        "pageup" => "page up".to_owned(),
        "pagedown" => "page down".to_owned(),
        "cr" | "enter" | "return" => "enter".to_owned(),
        "bs" => "backspace".to_owned(),
        _ if key.chars().count() == 1 && !parts.is_empty() => key.to_lowercase(),
        lower => lower.to_owned(),
    };
    parts.push(key);
    parts.join("+")
}

fn motion_label(motion: Motion) -> &'static str {
    match motion {
        Motion::Up => "move up",
        Motion::Down => "move down",
        Motion::Left => "move left",
        Motion::Right => "move right",
        Motion::PageUp => "scroll up a page",
        Motion::PageDown => "scroll down a page",
        Motion::HalfUp => "scroll up half a page",
        Motion::HalfDown => "scroll down half a page",
        Motion::Top => "first row",
        Motion::Bottom => "last row",
        Motion::FirstColumn => "first column",
        Motion::LastColumn => "last column",
        Motion::FocusLeft => "go to tables",
        Motion::FocusRight => "go to data",
    }
}

/// A titled list of (keys, what they do).
type Section = (String, Vec<(String, String)>);

pub fn render(frame: &mut Frame, area: Rect, vim: Option<&VimKeys>) {
    let mut sections: Vec<Section> = SECTIONS
        .iter()
        .map(|(title, keys)| {
            ((*title).to_owned(), keys.iter().map(|(k, a)| ((*k).to_owned(), (*a).to_owned())).collect())
        })
        .collect();
    if let Some(vim) = vim {
        let keys = personal_keys(vim);
        if !keys.is_empty() {
            let title = if vim.source.starts_with("vim") { "From your vimrc" } else { "From your nvim config" };
            sections.push((title.to_owned(), keys));
        }
    }

    // Two columns when one would not fit: sections split where the halves balance.
    let height =
        |part: &[Section]| -> u16 { part.iter().map(|(_, keys)| keys.len() as u16 + 2).sum::<u16>().saturating_sub(1) };
    let two = height(&sections) + 2 > area.height && area.width >= 100;
    let split = if two {
        (1..sections.len()).min_by_key(|&i| height(&sections[..i]).max(height(&sections[i..]))).unwrap_or(1)
    } else {
        sections.len()
    };
    let columns: Vec<&[Section]> = if two { vec![&sections[..split], &sections[split..]] } else { vec![&sections[..]] };
    let key_width =
        sections.iter().flat_map(|(_, keys)| keys.iter()).map(|(k, _)| text::measure(k, 40)).max().unwrap_or(0);
    let column_width: u16 = 70;
    let rows = columns.iter().map(|c| height(c)).max().unwrap_or(0);
    let width = (column_width * columns.len() as u16 + 2).min(area.width.saturating_sub(4));
    let rect = text::centered(area, width, rows + 2);
    frame.render_widget(Clear, rect);
    let block = Block::bordered()
        .border_type(BorderType::Rounded)
        .border_style(theme::accent())
        .title(Line::from(vec![Span::raw(" "), Span::styled("Keys", theme::strong()), Span::raw(" ")]));
    let inner = block.inner(rect);
    frame.render_widget(block, rect);

    let buf = frame.buffer_mut();
    for (index, column) in columns.iter().enumerate() {
        let left = inner.x + index as u16 * column_width;
        let right = (left + column_width).min(inner.right());
        let mut y = inner.y;
        for (title, keys) in column.iter() {
            if y >= inner.bottom() {
                break;
            }
            buf.set_string(left + 1, y, title, theme::strong());
            y += 1;
            for (key, action) in keys {
                if y >= inner.bottom() {
                    break;
                }
                buf.set_stringn(left + 3, y, key, key_width, theme::accent());
                let x = left + 3 + key_width as u16 + 3;
                buf.set_stringn(x, y, action, right.saturating_sub(x + 1) as usize, Style::new());
                y += 1;
            }
            y += 1;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::key_label;

    #[test]
    fn labels_keys_the_way_the_footer_does() {
        assert_eq!(key_label("<C-D>"), "ctrl+d");
        assert_eq!(key_label("<M-l>"), "alt+l");
        assert_eq!(key_label("<S-Down>"), "shift+↓");
        assert_eq!(key_label("€"), "€");
        assert_eq!(key_label("<"), "<");
    }
}
