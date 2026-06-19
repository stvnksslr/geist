//! Scrollback search: find a query across the screen's [`RowText`] and track the
//! search overlay's navigation state.
//!
//! Pure logic — no egui, no engine calls. The [`Session`](crate::session) reads
//! the screen text from its engine once (when the overlay opens or the query
//! changes), feeds it here, and uses the resulting [`Match`]es to scroll and to
//! highlight. Keeping the matching here makes it unit-testable in isolation.

use crate::engine::RowText;

/// A search hit: an inclusive span of cells on one absolute screen row
/// (`row` 0 = the oldest scrollback row, matching [`RowText::row`]).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Match {
    pub row: u32,
    pub col_start: u16,
    pub col_end: u16,
}

/// Find every non-overlapping occurrence of `needle` across `rows`, in row- then
/// column-order. Case-insensitive (ASCII) unless `case_sensitive`. An empty
/// `needle` yields no matches. Matches are within a single screen row — a
/// soft-wrapped line is several rows, so a query spanning a wrap is not found
/// (a documented v1 limitation).
pub fn search_rows(rows: &[RowText], needle: &str, case_sensitive: bool) -> Vec<Match> {
    if needle.is_empty() {
        return Vec::new();
    }
    let fold = |c: char| {
        if case_sensitive {
            c
        } else {
            c.to_ascii_lowercase()
        }
    };
    let pat: Vec<char> = needle.chars().map(fold).collect();
    let mut out = Vec::new();
    for row in rows {
        if row.chars.len() < pat.len() {
            continue;
        }
        let hay: Vec<char> = row.chars.iter().copied().map(fold).collect();
        let mut i = 0;
        while i + pat.len() <= hay.len() {
            if hay[i..i + pat.len()] == pat[..] {
                out.push(Match {
                    row: row.row,
                    col_start: row.cols[i],
                    col_end: row.cols[i + pat.len() - 1],
                });
                i += pat.len(); // non-overlapping
            } else {
                i += 1;
            }
        }
    }
    out
}

/// A match mapped into the current viewport for the renderer: the viewport row
/// and inclusive column span, plus whether it's the emphasized (current) match.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SearchHighlight {
    pub row: u16,
    pub col_start: u16,
    pub col_end: u16,
    pub current: bool,
}

/// The scrollback-search overlay's state, held by the focused session while the
/// overlay is open.
pub struct SearchState {
    /// The current search query.
    pub query: String,
    /// Whether matching is case-sensitive (default off).
    pub case_sensitive: bool,
    /// Matches for the current query over the captured screen text, oldest first.
    pub matches: Vec<Match>,
    /// Index of the emphasized ("current") match; only meaningful when `matches`
    /// is non-empty.
    pub current: usize,
    /// True on the first frame after opening so the input box grabs focus exactly
    /// once (mirrors the command palette's `just_opened`).
    pub just_opened: bool,
}

impl SearchState {
    pub fn new() -> Self {
        Self {
            query: String::new(),
            case_sensitive: false,
            matches: Vec::new(),
            current: 0,
            just_opened: true,
        }
    }

    /// Recompute matches for the current query over `rows`, clamping `current`
    /// so it stays in range.
    pub fn run(&mut self, rows: &[RowText]) {
        self.matches = search_rows(rows, &self.query, self.case_sensitive);
        if self.current >= self.matches.len() {
            self.current = 0;
        }
    }

    /// Number of matches for the current query.
    pub fn count(&self) -> usize {
        self.matches.len()
    }

    /// The emphasized match, if any.
    pub fn current_match(&self) -> Option<Match> {
        self.matches.get(self.current).copied()
    }

    /// Advance to the next (`forward`) or previous match, wrapping. No-op when
    /// there are no matches.
    pub fn step(&mut self, forward: bool) {
        let n = self.matches.len();
        if n == 0 {
            return;
        }
        self.current = if forward {
            (self.current + 1) % n
        } else {
            (self.current + n - 1) % n
        };
    }

    /// Point `current` at the match whose row is nearest `target_row` (ties pick
    /// the later/newer match). Used after a fresh query so navigation starts from
    /// the occurrence closest to where the viewport already sits. No-op if empty.
    pub fn select_nearest(&mut self, target_row: u32) {
        let mut best = 0;
        let mut best_dist = u64::MAX;
        for (i, m) in self.matches.iter().enumerate() {
            let dist = (i64::from(m.row) - i64::from(target_row)).unsigned_abs();
            if dist <= best_dist {
                best_dist = dist;
                best = i;
            }
        }
        self.current = best;
    }
}

impl Default for SearchState {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::engine::RowText;

    /// Build `RowText` rows from plain strings (each char in column = its index),
    /// numbered from `start_row`.
    fn rows(lines: &[&str], start_row: u32) -> Vec<RowText> {
        lines
            .iter()
            .enumerate()
            .map(|(i, line)| RowText {
                row: start_row + i as u32,
                chars: line.chars().collect(),
                cols: (0..line.chars().count() as u16).collect(),
            })
            .collect()
    }

    #[test]
    fn finds_matches_with_columns_and_rows() {
        let r = rows(&["the cat sat", "on the mat"], 5);
        let m = search_rows(&r, "the", false);
        assert_eq!(m.len(), 2);
        assert_eq!(m[0], Match { row: 5, col_start: 0, col_end: 2 });
        assert_eq!(m[1], Match { row: 6, col_start: 3, col_end: 5 });
    }

    #[test]
    fn case_insensitive_by_default_and_sensitive_when_asked() {
        let r = rows(&["Foo foo FOO"], 0);
        assert_eq!(search_rows(&r, "foo", false).len(), 3);
        assert_eq!(search_rows(&r, "foo", true).len(), 1);
        assert_eq!(search_rows(&r, "Foo", true)[0].col_start, 0);
    }

    #[test]
    fn non_overlapping_and_empty_query() {
        // "aa" in "aaaa" yields two non-overlapping hits, not three.
        let r = rows(&["aaaa"], 0);
        let m = search_rows(&r, "aa", false);
        assert_eq!(m.len(), 2);
        assert_eq!((m[0].col_start, m[0].col_end), (0, 1));
        assert_eq!((m[1].col_start, m[1].col_end), (2, 3));
        // An empty query matches nothing (no highlight-everything).
        assert!(search_rows(&r, "", false).is_empty());
    }

    #[test]
    fn maps_columns_through_wide_char_offsets() {
        // A wide char occupies one cell here but two columns in the source map:
        // chars = ['世','x'] with cols = [0, 2] (col 1 is the wide tail).
        let row = RowText {
            row: 0,
            chars: vec!['世', 'x'],
            cols: vec![0, 2],
        };
        let m = search_rows(&[row], "x", false);
        assert_eq!(m[0].col_start, 2, "match column follows the cols map");
    }

    #[test]
    fn step_wraps_both_directions() {
        let mut s = SearchState::new();
        s.query = "x".into();
        s.run(&rows(&["x", "x", "x"], 0));
        assert_eq!(s.count(), 3);
        s.current = 0;
        s.step(true);
        assert_eq!(s.current, 1);
        s.step(false);
        assert_eq!(s.current, 0);
        s.step(false); // wrap to last
        assert_eq!(s.current, 2);
        s.step(true); // wrap to first
        assert_eq!(s.current, 0);
    }

    #[test]
    fn step_and_current_are_safe_when_empty() {
        let mut s = SearchState::new();
        s.query = "nope".into();
        s.run(&rows(&["abc"], 0));
        assert_eq!(s.count(), 0);
        assert_eq!(s.current_match(), None);
        s.step(true); // no panic, no-op
        assert_eq!(s.current, 0);
    }

    #[test]
    fn select_nearest_picks_closest_row() {
        let mut s = SearchState::new();
        s.matches = vec![
            Match { row: 2, col_start: 0, col_end: 0 },
            Match { row: 10, col_start: 0, col_end: 0 },
            Match { row: 20, col_start: 0, col_end: 0 },
        ];
        s.select_nearest(11);
        assert_eq!(s.current, 1, "row 10 is nearest 11");
        s.select_nearest(0);
        assert_eq!(s.current, 0, "row 2 is nearest 0");
        s.select_nearest(100);
        assert_eq!(s.current, 2, "row 20 is nearest 100");
    }
}
