//! Scrollback search: find a query across the screen's [`RowText`] and track the
//! search overlay's navigation state.
//!
//! Pure logic — no egui, no engine calls. The [`Session`](crate::session) reads
//! the screen text from its engine once (when the overlay opens or the query
//! changes), feeds it here, and uses the resulting [`Match`]es to scroll and to
//! highlight. Keeping the matching here makes it unit-testable in isolation.

use crate::engine::RowText;

/// A search hit, as an inclusive `(row, column)` start and end in absolute
/// screen coordinates (`row` 0 = the oldest scrollback row, matching
/// [`RowText::row`]).
///
/// Start and end can be on **different rows**: a soft-wrapped line is several
/// display rows and a match may span the wrap.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Match {
    pub row: u32,
    pub col_start: u16,
    /// Row the match ends on — equal to `row` for the ordinary single-row case.
    pub end_row: u32,
    pub col_end: u16,
}

impl Match {
    /// A match confined to one row.
    pub fn single(row: u32, col_start: u16, col_end: u16) -> Self {
        Self {
            row,
            col_start,
            end_row: row,
            col_end,
        }
    }
}

/// One position inside a joined logical line: which screen row and column the
/// character at that index came from.
#[derive(Clone, Copy)]
struct Origin {
    row: u32,
    col: u16,
}

/// Find every non-overlapping occurrence of `needle` across `rows`, in row- then
/// column-order. Case-insensitive (ASCII) unless `case_sensitive`. An empty
/// `needle` yields no matches.
///
/// Soft-wrapped rows are **joined into logical lines first** (`RowText::wrapped`
/// marks a row that continues onto the next), so a query spanning a wrap is
/// found — the row and column of each end are then recovered from the joined
/// line's origin map. Searching row by row, as this used to, silently missed
/// every match that happened to straddle the window's right edge.
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
    let mut hay: Vec<char> = Vec::new();
    for_each_line(rows, |chars, origin| {
        if chars.len() < pat.len() {
            return;
        }
        hay.clear();
        hay.extend(chars.iter().map(|&c| fold(c)));
        let mut j = 0;
        while j + pat.len() <= hay.len() {
            if hay[j..j + pat.len()] == pat[..] {
                out.push(span(origin, j, j + pat.len() - 1));
                j += pat.len(); // non-overlapping
            } else {
                j += 1;
            }
        }
    });
    out
}

/// Compile a regex query. Case-insensitive unless `case_sensitive` — with
/// Unicode case folding, unlike the substring search's ASCII fold, because
/// that is what the `regex` crate does and there is no reason to cripple it.
pub fn compile_regex(needle: &str, case_sensitive: bool) -> Result<regex::Regex, String> {
    regex::RegexBuilder::new(needle)
        .case_insensitive(!case_sensitive)
        .build()
        .map_err(|e| match e {
            regex::Error::Syntax(s) => s.lines().last().unwrap_or("invalid pattern").to_string(),
            other => other.to_string(),
        })
}

/// Every non-overlapping, non-empty match of `re` across `rows`, in the same
/// shape and order as [`search_rows`]. Matching runs over the same wrap-joined
/// logical lines, so a match may span a soft wrap — but never a hard line
/// break: `^`/`$` anchor to a logical line, as they would in the program that
/// printed it. Empty matches (`a*` against `b`) are skipped: there is no cell
/// to highlight and they would make navigation step through nothing.
pub fn search_rows_regex(rows: &[RowText], re: &regex::Regex) -> Vec<Match> {
    let mut out = Vec::new();
    let mut text = String::new();
    let mut byte_to_char: Vec<usize> = Vec::new();
    for_each_line(rows, |chars, origin| {
        text.clear();
        byte_to_char.clear();
        for (i, &c) in chars.iter().enumerate() {
            text.push(c);
            byte_to_char.resize(text.len(), i);
        }
        for m in re.find_iter(&text) {
            if m.is_empty() {
                continue;
            }
            out.push(span(origin, byte_to_char[m.start()], byte_to_char[m.end() - 1]));
        }
    });
    out
}

/// The match covering joined-line chars `first..=last`.
fn span(origin: &[Origin], first: usize, last: usize) -> Match {
    let (s, e) = (origin[first], origin[last]);
    Match {
        row: s.row,
        col_start: s.col,
        end_row: e.row,
        col_end: e.col,
    }
}

/// Call `f` once per logical line: the chars of a row plus every row it
/// soft-wraps onto, with each char's origin cell.
fn for_each_line(rows: &[RowText], mut f: impl FnMut(&[char], &[Origin])) {
    let mut chars: Vec<char> = Vec::new();
    let mut origin: Vec<Origin> = Vec::new();
    let mut i = 0;
    while i < rows.len() {
        chars.clear();
        origin.clear();
        // Consume this row and every row it wraps onto.
        loop {
            let r = &rows[i];
            for (k, &ch) in r.chars.iter().enumerate() {
                chars.push(ch);
                origin.push(Origin {
                    row: r.row,
                    col: r.cols.get(k).copied().unwrap_or(0),
                });
            }
            // A wrapped *last* row has nothing to join to; stop either way.
            if !r.wrapped || i + 1 >= rows.len() {
                i += 1;
                break;
            }
            i += 1;
        }
        f(&chars, &origin);
    }
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
    /// Treat `query` as a regular expression (default off). giest-only:
    /// Ghostty's search, and lib-vt's native search API, are substring-only.
    pub regex: bool,
    /// Why the regex query doesn't compile, for the search bar. `None` when it
    /// does, or when regex mode is off.
    pub error: Option<String>,
    /// Matches for the current query over the captured screen text, oldest first.
    pub matches: Vec<Match>,
    /// Index of the emphasized ("current") match; only meaningful when `matches`
    /// is non-empty.
    pub current: usize,
    /// True on the first frame after opening so the input box grabs focus exactly
    /// once (mirrors the command palette's `just_opened`).
    pub just_opened: bool,
    /// Absolute screen row the anchor pointed at when the text was captured.
    /// Compared against where the engine says that row is *now* to correct for
    /// scrollback eviction — see [`SearchState::row_shift`].
    pub anchor_at_capture: u32,
}

/// How far match rows have drifted, given where the capture-time anchor row sits
/// now. Negative means rows were evicted and everything moved up.
///
/// Eviction drops the oldest rows, renumbering the whole screen by the same
/// amount, so one anchor corrects every match. Pinning the capture's **last**
/// row rather than its first is deliberate: the first row is the first to be
/// evicted, and a pin whose row is destroyed reports no value — the correction
/// would stop working exactly when it became necessary.
pub fn row_shift(anchor_at_capture: u32, anchor_now: u32) -> i64 {
    i64::from(anchor_now) - i64::from(anchor_at_capture)
}

/// Apply `shift` to a match's rows, or `None` if that puts it off the top of the
/// screen — those rows have been evicted, and highlighting where they used to be
/// would mark unrelated text.
pub fn shifted(m: Match, shift: i64) -> Option<Match> {
    let row = i64::from(m.row) + shift;
    let end_row = i64::from(m.end_row) + shift;
    (row >= 0 && end_row >= 0).then_some(Match {
        row: row as u32,
        end_row: end_row as u32,
        ..m
    })
}

impl SearchState {
    pub fn new() -> Self {
        Self {
            query: String::new(),
            case_sensitive: false,
            regex: false,
            error: None,
            matches: Vec::new(),
            current: 0,
            just_opened: true,
            anchor_at_capture: 0,
        }
    }

    /// Recompute matches for the current query over `rows`, clamping `current`
    /// so it stays in range.
    pub fn run(&mut self, rows: &[RowText]) {
        self.error = None;
        self.matches = if !self.regex {
            search_rows(rows, &self.query, self.case_sensitive)
        } else if self.query.is_empty() {
            Vec::new()
        } else {
            match compile_regex(&self.query, self.case_sensitive) {
                Ok(re) => search_rows_regex(rows, &re),
                // Mid-typing (`foo(`) is the common case: show why, match nothing.
                Err(e) => {
                    self.error = Some(e);
                    Vec::new()
                }
            }
        };
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
    /// numbered from `start_row`. No row is soft-wrapped.
    fn rows(lines: &[&str], start_row: u32) -> Vec<RowText> {
        lines
            .iter()
            .enumerate()
            .map(|(i, line)| RowText {
                row: start_row + i as u32,
                chars: line.chars().collect(),
                cols: (0..line.chars().count() as u16).collect(),
                wrapped: false,
            })
            .collect()
    }

    /// The same, but every row except the last continues onto the next — i.e.
    /// one logical line broken across the grid's width.
    fn wrapped_rows(lines: &[&str], start_row: u32) -> Vec<RowText> {
        let mut r = rows(lines, start_row);
        let n = r.len();
        for row in r.iter_mut().take(n.saturating_sub(1)) {
            row.wrapped = true;
        }
        r
    }

    #[test]
    fn finds_matches_with_columns_and_rows() {
        let r = rows(&["the cat sat", "on the mat"], 5);
        let m = search_rows(&r, "the", false);
        assert_eq!(m.len(), 2);
        assert_eq!(m[0], Match::single(5, 0, 2));
        assert_eq!(m[1], Match::single(6, 3, 5));
    }

    #[test]
    fn a_match_spanning_a_soft_wrap_is_found() {
        // The headline fix. "wonderful" is split across the grid's right edge;
        // searching row by row never saw it.
        let r = wrapped_rows(&["hello wond", "erful"], 0);
        let m = search_rows(&r, "wonderful", false);
        assert_eq!(m.len(), 1, "the wrap is joined before matching");
        assert_eq!(
            m[0],
            Match {
                row: 0,
                col_start: 6,
                end_row: 1,
                col_end: 4
            },
            "and each end reports its own row"
        );
    }

    #[test]
    fn joining_stops_at_a_line_that_does_not_wrap() {
        // Two separate lines must not be glued together, or a query straddling
        // the join would match text that isn't contiguous on screen.
        let r = rows(&["abc", "def"], 0);
        assert!(search_rows(&r, "cd", false).is_empty());
        // …while the same rows *marked* wrapped do join.
        let w = wrapped_rows(&["abc", "def"], 0);
        assert_eq!(search_rows(&w, "cd", false).len(), 1);
    }

    #[test]
    fn a_trailing_wrapped_row_does_not_run_off_the_end() {
        // The last captured row can be marked wrapped (its continuation hasn't
        // been written yet). Joining must stop rather than index past the end.
        let mut r = rows(&["abc"], 0);
        r[0].wrapped = true;
        assert_eq!(search_rows(&r, "abc", false).len(), 1);
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
            wrapped: false,
        };
        let m = search_rows(&[row], "x", false);
        assert_eq!(m[0].col_start, 2, "match column follows the cols map");
    }

    fn re(p: &str) -> regex::Regex {
        compile_regex(p, false).unwrap()
    }

    #[test]
    fn regex_finds_patterns_with_rows_and_columns() {
        let r = rows(&["err 404 ok", "err 500"], 3);
        let m = search_rows_regex(&r, &re(r"\d{3}"));
        assert_eq!(m, vec![Match::single(3, 4, 6), Match::single(4, 4, 6)]);
    }

    #[test]
    fn regex_matches_across_a_soft_wrap_but_not_a_hard_break() {
        let w = wrapped_rows(&["hello wond", "erful"], 0);
        let m = search_rows_regex(&w, &re("won.*ful"));
        assert_eq!(m, vec![Match { row: 0, col_start: 6, end_row: 1, col_end: 4 }]);
        // `$` anchors to the logical line, so it isn't the wrap point…
        assert!(search_rows_regex(&w, &re("wond$")).is_empty());
        // …while unwrapped rows are separate lines.
        let r = rows(&["abc", "def"], 0);
        assert!(search_rows_regex(&r, &re("c.d")).is_empty());
        assert_eq!(search_rows_regex(&r, &re("^d")).len(), 1);
    }

    #[test]
    fn regex_maps_multibyte_chars_to_their_columns() {
        // '世' is 3 UTF-8 bytes and 2 columns wide: byte offsets must not leak
        // into column numbers.
        let row = RowText { row: 0, chars: vec!['世', 'x', 'y'], cols: vec![0, 2, 3], wrapped: false };
        assert_eq!(search_rows_regex(&[row.clone()], &re("xy")), vec![Match::single(0, 2, 3)]);
        assert_eq!(search_rows_regex(&[row], &re("世x")), vec![Match::single(0, 0, 2)]);
    }

    #[test]
    fn regex_skips_empty_matches_and_honours_case() {
        let r = rows(&["bbb"], 0);
        assert!(search_rows_regex(&r, &re("a*")).is_empty());
        let r = rows(&["Foo foo"], 0);
        assert_eq!(search_rows_regex(&r, &re("foo")).len(), 2);
        assert_eq!(search_rows_regex(&r, &compile_regex("foo", true).unwrap()).len(), 1);
        // Unicode folding in regex mode (the substring search folds ASCII only).
        let r = rows(&["ÉCOLE"], 0);
        assert_eq!(search_rows_regex(&r, &re("école")).len(), 1);
    }

    #[test]
    fn an_invalid_regex_reports_why_and_matches_nothing() {
        let mut s = SearchState::new();
        s.regex = true;
        s.query = "foo(".into();
        s.run(&rows(&["foo("], 0));
        assert_eq!(s.count(), 0);
        assert!(s.error.is_some());
        // The same text as a plain query is fine, and clears the error.
        s.regex = false;
        s.run(&rows(&["foo("], 0));
        assert_eq!(s.count(), 1);
        assert_eq!(s.error, None);
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
    fn eviction_shifts_match_rows_and_drops_the_ones_that_are_gone() {
        // Rows were captured with the anchor (the capture's last row) at 100.
        // Ten rows have since been evicted, so that same row is now 90 and every
        // match moved up by ten.
        let shift = row_shift(100, 90);
        assert_eq!(shift, -10);
        assert_eq!(shifted(Match::single(50, 1, 3), shift), Some(Match::single(40, 1, 3)));
        // A match in the rows that were evicted is dropped, not drawn ten lines
        // higher over unrelated text.
        assert_eq!(shifted(Match::single(4, 0, 0), shift), None);
        // A match whose *end* fell off is dropped too.
        let straddling = Match { row: 12, col_start: 0, end_row: 3, col_end: 0 };
        assert_eq!(shifted(straddling, shift), None);
        // Nothing evicted: identity.
        assert_eq!(row_shift(100, 100), 0);
        assert_eq!(shifted(Match::single(7, 0, 1), 0), Some(Match::single(7, 0, 1)));
    }

    #[test]
    fn select_nearest_picks_closest_row() {
        let mut s = SearchState::new();
        s.matches = vec![
            Match::single(2, 0, 0),
            Match::single(10, 0, 0),
            Match::single(20, 0, 0),
        ];
        s.select_nearest(11);
        assert_eq!(s.current, 1, "row 10 is nearest 11");
        s.select_nearest(0);
        assert_eq!(s.current, 0, "row 2 is nearest 0");
        s.select_nearest(100);
        assert_eq!(s.current, 2, "row 20 is nearest 100");
    }
}
