//! Text measuring and fitting by terminal display width.

use ratatui::{layout::Rect, style::Style, text::Span};
use unicode_width::UnicodeWidthChar;

/// Control characters rendered as visible stand-ins so they never break a layout.
fn visible(c: char) -> Option<char> {
    match c {
        '\r' => None,
        '\n' => Some('↵'),
        '\t' => Some(' '),
        c if c.is_control() => Some('·'),
        c => Some(c),
    }
}

/// Display width of `s`, counting at most up to `cap` columns.
pub fn measure(s: &str, cap: usize) -> usize {
    let mut width = 0;
    for c in s.chars().filter_map(visible) {
        width += c.width().unwrap_or(0);
        if width >= cap {
            return cap;
        }
    }
    width
}

/// Fits `s` into `width` columns, ending with an ellipsis when it was cut.
/// Only scans as far as needed, so huge values stay cheap.
pub fn fit(s: &str, width: usize) -> String {
    let mut out = String::new();
    let mut used = 0;
    for c in s.chars().filter_map(visible) {
        let w = c.width().unwrap_or(0);
        if used + w > width {
            while used + 1 > width {
                match out.pop() {
                    Some(p) => used -= p.width().unwrap_or(0),
                    None => return out,
                }
            }
            out.push('…');
            return out;
        }
        out.push(c);
        used += w;
    }
    out
}

/// `fit`, then padded to exactly `width` columns.
pub fn fit_aligned(s: &str, width: usize, right: bool) -> String {
    let fitted = fit(s, width);
    let pad = width.saturating_sub(measure(&fitted, width));
    if right { format!("{}{fitted}", " ".repeat(pad)) } else { format!("{fitted}{}", " ".repeat(pad)) }
}

/// 1234567 -> "1,234,567"
pub fn thousands(n: u64) -> String {
    let digits = n.to_string();
    let mut out = String::new();
    for (i, c) in digits.chars().enumerate() {
        if i > 0 && (digits.len() - i).is_multiple_of(3) {
            out.push(',');
        }
        out.push(c);
    }
    out
}

/// 2500 -> "2.5k", 20000 -> "20k", 1200000 -> "1.2M"
pub fn compact(n: u64) -> String {
    let scaled = |value: f64, suffix: &str| {
        if value < 10.0 && (value * 10.0).round() % 10.0 != 0.0 {
            format!("{value:.1}{suffix}")
        } else {
            format!("{}{suffix}", value.round() as u64)
        }
    };
    match n {
        0..1_000 => n.to_string(),
        1_000..999_500 => scaled(n as f64 / 1e3, "k"),
        999_500..999_500_000 => scaled(n as f64 / 1e6, "M"),
        _ => scaled(n as f64 / 1e9, "B"),
    }
}

/// Splits `text` into spans, styling the characters at `indices` with `hl`.
pub fn highlight(text: &str, indices: &[u32], base: Style, hl: Style) -> Vec<Span<'static>> {
    let mut spans = Vec::new();
    let mut run = String::new();
    let mut run_hl = false;
    let mut next = indices.iter().peekable();
    for (i, c) in text.chars().enumerate() {
        let is_hl = next.peek().is_some_and(|&&n| n as usize == i);
        if is_hl {
            next.next();
        }
        if is_hl != run_hl && !run.is_empty() {
            spans.push(Span::styled(std::mem::take(&mut run), if run_hl { base.patch(hl) } else { base }));
        }
        run_hl = is_hl;
        run.push(c);
    }
    if !run.is_empty() {
        spans.push(Span::styled(run, if run_hl { base.patch(hl) } else { base }));
    }
    spans
}

/// Character positions of every case-insensitive occurrence of `needle` in `haystack`.
pub fn find_all(haystack: &str, needle: &str) -> Vec<u32> {
    let hay: Vec<char> = haystack.chars().flat_map(char::to_lowercase).collect();
    let needle: Vec<char> = needle.chars().flat_map(char::to_lowercase).collect();
    // Lowercasing can change length for a few characters; only highlight when it didn't.
    if needle.is_empty() || hay.len() != haystack.chars().count() || needle.len() > hay.len() {
        return Vec::new();
    }
    let mut hits = Vec::new();
    let mut i = 0;
    while i + needle.len() <= hay.len() {
        if hay[i..i + needle.len()] == needle[..] {
            hits.extend((i..i + needle.len()).map(|n| n as u32));
            i += needle.len();
        } else {
            i += 1;
        }
    }
    hits
}

/// A `width` x `height` rectangle centred in `area`, clamped to fit.
pub fn centered(area: Rect, width: u16, height: u16) -> Rect {
    let width = width.min(area.width);
    let height = height.min(area.height);
    Rect { x: area.x + (area.width - width) / 2, y: area.y + (area.height - height) / 2, width, height }
}

/// Greedy word wrap by display width that keeps every character: runs of
/// spaces and indentation survive, and continuation lines keep the indent of
/// the line they continue. Words longer than the width are broken.
pub fn wrap(s: &str, width: usize) -> Vec<String> {
    let width = width.max(1);
    let char_width = |c: char| visible(c).and_then(|v| v.width()).unwrap_or(0);
    let mut lines = Vec::new();
    for paragraph in s.split('\n') {
        let depth = paragraph.len() - paragraph.trim_start_matches(' ').len();
        let indent = if depth < width / 2 { " ".repeat(depth) } else { String::new() };
        let mut line = String::new();
        let mut used = 0;
        for token in paragraph.split_inclusive(' ') {
            let word = measure(token.trim_end_matches(' '), usize::MAX);
            if used + word > width && !line.trim().is_empty() {
                lines.push(line.trim_end().to_owned());
                line.clone_from(&indent);
                used = indent.len();
            }
            for c in token.chars().filter_map(visible) {
                let w = char_width(c);
                if used + w > width {
                    if c == ' ' {
                        continue;
                    }
                    lines.push(std::mem::take(&mut line));
                    line.clone_from(&indent);
                    used = indent.len();
                }
                line.push(c);
                used += w;
            }
        }
        lines.push(line.trim_end().to_owned());
    }
    lines
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fits_wide_characters_and_control_characters() {
        assert_eq!(fit("hello world", 8), "hello w…");
        assert_eq!(fit("short", 8), "short");
        assert_eq!(fit("🦊🦊🦊", 5), "🦊🦊…");
        assert_eq!(fit("a\nb", 8), "a↵b");
        assert_eq!(fit("anything", 0), "");
    }

    #[test]
    fn wraps_without_losing_indentation() {
        assert_eq!(wrap("{\n  \"a\": 1\n}", 20), ["{", "  \"a\": 1", "}"]);
        assert_eq!(wrap("aaa bbb ccc", 7), ["aaa bbb", "ccc"]);
        assert_eq!(wrap("    indented long words here", 16), ["    indented", "    long words", "    here"]);
        assert_eq!(wrap("abcdefghij", 4), ["abcd", "efgh", "ij"]);
    }

    #[test]
    fn finds_matches_case_insensitively() {
        assert_eq!(find_all("Björn Ek", "ek"), [6, 7]);
        assert_eq!(find_all("aXa", "a"), [0, 2]);
        assert!(find_all("abc", "").is_empty());
    }

    #[test]
    fn formats_counts() {
        assert_eq!(thousands(20000), "20,000");
        assert_eq!(thousands(999), "999");
        assert_eq!(compact(2500), "2.5k");
        assert_eq!(compact(20000), "20k");
        assert_eq!(compact(999_999), "1M");
        assert_eq!(compact(1_200_000), "1.2M");
    }
}
