//! Fuzzy matching (the matcher from the Helix editor) with match positions for highlighting.

use nucleo_matcher::{
    Config, Matcher, Utf32Str,
    pattern::{CaseMatching, Normalization, Pattern},
};

pub struct Fuzzy {
    matcher: Matcher,
    buf: Vec<char>,
}

impl Default for Fuzzy {
    fn default() -> Self {
        Fuzzy { matcher: Matcher::new(Config::DEFAULT), buf: Vec::new() }
    }
}

impl Fuzzy {
    pub fn pattern(query: &str) -> Pattern {
        Pattern::parse(query, CaseMatching::Smart, Normalization::Smart)
    }

    /// Score and sorted character positions of the match, or `None`.
    pub fn score(&mut self, pattern: &Pattern, text: &str) -> Option<(u32, Vec<u32>)> {
        let mut indices = Vec::new();
        let score = pattern.indices(Utf32Str::new(text, &mut self.buf), &mut self.matcher, &mut indices)?;
        indices.sort_unstable();
        indices.dedup();
        Some((score, indices))
    }
}
