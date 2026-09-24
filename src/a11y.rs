//! Screen-reader support: each pane as an AccessKit text node (UI Automation on
//! Windows), plus a polite live region announcing new output.
//!
//! Parity target is the macOS app's VoiceOver support (`SurfaceView_AppKit.swift`,
//! "Accessibility"): a read-only text area whose value is the visible text,
//! with the insertion point at the terminal cursor and the selection as the
//! selected range. AccessKit expresses a text node as a parent (role
//! [`Role::Terminal`], exposed to UIA as a Document with the Text pattern) with
//! one `TextRun` child per line; positions are `(run node, char index)`.
//!
//! Upstream has no "announce output" feature — VoiceOver users re-read the text
//! area. Windows screen readers expect terminals to speak new output (Windows
//! Terminal does, via UIA notifications), so geist adds a live region, gated by
//! `accessibility-announce-output` and rate-limited by [`Announcer`].
//!
//! Everything here is built **only while an assistive technology is attached**:
//! egui returns `None` from `accesskit_node_builder` otherwise, and the caller
//! stops there, so there is no cost to users without one.

use std::hash::{Hash, Hasher};

use eframe::egui;
use egui::accesskit::{self, Role};

use crate::engine::GridSnapshot;

/// AccessKit's `word_starts` are `u8`, so a run can hold at most this many
/// characters; longer lines are split into chained runs (as egui does).
pub const MAX_CHARS_PER_RUN: usize = 255;

/// Longest announcement kept (its tail): a burst of output is summarised by
/// its end, which is what the user is waiting for.
const MAX_ANNOUNCE_CHARS: usize = 2000;

/// Minimum seconds between two announcements — the rate limit that keeps a
/// fast-scrolling build log from flooding the speech queue.
pub const ANNOUNCE_INTERVAL: f64 = 0.5;

/// One visible line as text, with each character's grid placement.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Row {
    /// The line without its trailing newline. Trailing blanks are trimmed,
    /// except up to the cursor and the selection's end, which must land on
    /// characters that exist.
    pub text: String,
    /// Per character: `(start column, cell width)`.
    pub cells: Vec<(u16, u8)>,
}

impl Row {
    /// Character index for grid column `col`: the character covering it, or
    /// the line length when `col` is past the end.
    pub fn char_at_col(&self, col: u16) -> usize {
        self.cells
            .iter()
            .position(|&(c, w)| col < c + w as u16)
            .unwrap_or(self.cells.len())
    }
}

/// A `(row, character index)` position in a [`PaneText`].
pub type Pos = (usize, usize);

/// The visible grid as text, with caret and selection — the model the
/// accessibility node is built from.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct PaneText {
    pub rows: Vec<Row>,
    /// The terminal cursor.
    pub caret: Pos,
    /// `(anchor, focus)`: focus is *exclusive* (one past the last selected
    /// character), as a text selection is.
    pub selection: Option<(Pos, Pos)>,
}

impl PaneText {
    /// The whole text, lines joined by `\n` (the node's value).
    pub fn plain(&self) -> String {
        let mut s = String::new();
        for (i, r) in self.rows.iter().enumerate() {
            if i > 0 {
                s.push('\n');
            }
            s.push_str(&r.text);
        }
        s
    }

    /// Character offset of `pos` in [`Self::plain`].
    pub fn offset(&self, pos: Pos) -> usize {
        self.rows[..pos.0]
            .iter()
            .map(|r| r.cells.len() + 1)
            .sum::<usize>()
            + pos.1
    }

    /// The selected text, if any (what a screen reader's "read selection" gets).
    pub fn selected_text(&self) -> Option<String> {
        let (a, f) = self.selection?;
        let plain = self.plain();
        let (s, e) = (self.offset(a), self.offset(f));
        Some(plain.chars().skip(s).take(e.saturating_sub(s)).collect())
    }

    /// Each line trimmed of trailing blanks (the unit output diffing works on).
    pub fn lines(&self) -> Vec<String> {
        self.rows
            .iter()
            .map(|r| r.text.trim_end().to_string())
            .collect()
    }
}

/// Cheap change detector for a snapshot: everything [`extract`] reads.
/// Hashing is much cheaper than building the strings, so a pane rebuilds its
/// model only when this changes.
pub fn fingerprint(snap: &GridSnapshot) -> u64 {
    let mut h = std::collections::hash_map::DefaultHasher::new();
    (snap.cols, snap.rows, snap.cursor_x, snap.cursor_y).hash(&mut h);
    for c in &snap.cells {
        c.text.as_str().hash(&mut h);
        c.selected.hash(&mut h);
    }
    h.finish()
}

/// Build the text model from a snapshot.
pub fn extract(snap: &GridSnapshot) -> PaneText {
    let cols = snap.cols as usize;
    let nrows = snap.rows as usize;
    let cell = |r: usize, c: usize| snap.cells.get(r * cols + c);

    // Selection bounds in grid coordinates, row-major. A rectangular selection
    // is flattened to its first..last cell — a text range can't express a block.
    let mut sel: Option<((usize, usize), (usize, usize))> = None;
    for r in 0..nrows {
        for c in 0..cols {
            if cell(r, c).is_some_and(|x| x.selected) {
                sel = Some(match sel {
                    None => ((r, c), (r, c)),
                    Some((s, _)) => (s, (r, c)),
                });
            }
        }
    }

    let cursor_row = (snap.cursor_y as usize).min(nrows.saturating_sub(1));
    let mut rows = Vec::with_capacity(nrows);
    for r in 0..nrows {
        let mut row = Row::default();
        let mut c = 0;
        while c < cols {
            let (text, width) = match cell(r, c) {
                Some(x) if !x.text.is_empty() => {
                    let wide = x
                        .text
                        .chars()
                        .next()
                        .and_then(unicode_width::UnicodeWidthChar::width)
                        == Some(2);
                    (x.text.as_str(), if wide && c + 1 < cols { 2 } else { 1 })
                }
                _ => (" ", 1),
            };
            // A cluster is one character to the reader, whatever its length:
            // `cells` has one entry per `char` so byte/char maths stay simple.
            for (i, _) in text.chars().enumerate() {
                row.cells
                    .push((c as u16, if i == 0 { width as u8 } else { 0 }));
            }
            row.text.push_str(text);
            c += width;
        }
        // Trim trailing blanks, but never past a column something points at.
        let mut keep: u16 = 0;
        if r == cursor_row {
            keep = keep.max(snap.cursor_x);
        }
        if let Some((_, (er, ec))) = sel
            && er == r
        {
            keep = keep.max(ec as u16 + 1);
        }
        while row.text.ends_with(' ') && row.cells.last().is_some_and(|&(c, _)| c >= keep) {
            row.text.pop();
            row.cells.pop();
        }
        rows.push(row);
    }

    let caret = if nrows == 0 {
        (0, 0)
    } else {
        (cursor_row, rows[cursor_row].char_at_col(snap.cursor_x))
    };
    let selection = sel.map(|((sr, sc), (er, ec))| {
        let end = &rows[er];
        let fe = (end.char_at_col(ec as u16) + 1).min(end.cells.len());
        ((sr, rows[sr].char_at_col(sc as u16)), (er, fe))
    });
    PaneText {
        rows,
        caret,
        selection,
    }
}

/// Which lines of `new` are new output relative to `old`, allowing for the
/// screen having scrolled up by any number of lines in between.
///
/// Picks the scroll amount that preserves the longest run of leading lines,
/// then reports everything after that run (trailing blank lines dropped). A
/// change confined to the cursor's own line is *not* output: that is the
/// user's typing being echoed, which the screen reader already speaks.
pub fn new_output(old: &[String], new: &[String], cursor_row: usize) -> Vec<String> {
    let n = new.len();
    let lead = |k: usize| {
        (0..n.saturating_sub(k))
            .take_while(|&i| old.get(i + k) == Some(&new[i]))
            .count()
    };
    // Prefer the longest lead, then the smallest scroll (strict `>`).
    let mut best_lead = lead(0);
    for k in 1..n {
        let l = lead(k);
        // A match made only of blank lines proves nothing about scrolling.
        let meaningful = new[..l].iter().any(|s| !s.is_empty());
        if l > best_lead && meaningful {
            best_lead = l;
        }
    }
    let mut end = n;
    while end > best_lead && new[end - 1].is_empty() {
        end -= 1;
    }
    if end <= best_lead || (best_lead == cursor_row && end == cursor_row + 1) {
        return Vec::new();
    }
    new[best_lead..end].to_vec()
}

/// Per-pane live-region state: the last seen lines, output waiting to be
/// spoken, and the text currently in the live region.
#[derive(Debug, Default)]
pub struct Announcer {
    prev: Option<Vec<String>>,
    pending: Vec<String>,
    last_flush: f64,
    /// The live region's current text. Re-set only on a flush, so the
    /// adapter raises exactly one `LiveRegionChanged` per announcement.
    pub current: String,
    seq: u64,
}

impl Announcer {
    /// Feed the latest text. The first observation only sets the baseline, so
    /// attaching a screen reader doesn't read out the whole screen.
    pub fn observe(&mut self, lines: Vec<String>, cursor_row: usize) {
        if let Some(prev) = &self.prev {
            self.pending.extend(new_output(prev, &lines, cursor_row));
            let over = self.pending.len().saturating_sub(200);
            self.pending.drain(..over);
        }
        self.prev = Some(lines);
    }

    /// Drop the baseline (e.g. while scrolled back, where a changed screen is
    /// not new output); the next observation re-establishes it silently.
    pub fn reset(&mut self) {
        self.prev = None;
        self.pending.clear();
    }

    /// Move pending output into the live region if the rate limit allows.
    /// Returns whether [`Self::current`] changed.
    pub fn flush(&mut self, now: f64) -> bool {
        if self.pending.is_empty() || now - self.last_flush < ANNOUNCE_INTERVAL {
            return false;
        }
        let text = std::mem::take(&mut self.pending).join("\n");
        let count = text.chars().count();
        let mut text: String = text
            .chars()
            .skip(count.saturating_sub(MAX_ANNOUNCE_CHARS))
            .collect();
        // UIA announces on a *name change*: the same output twice in a row
        // (`ok`, `ok`) would otherwise be silent. Alternate a trailing space.
        self.seq += 1;
        if self.seq.is_multiple_of(2) {
            text.push(' ');
        }
        self.current = text;
        self.last_flush = now;
        true
    }

    pub fn has_pending(&self) -> bool {
        !self.pending.is_empty()
    }
}

/// Everything a pane keeps for accessibility between frames.
#[derive(Debug, Default)]
pub struct PaneA11y {
    fingerprint: Option<u64>,
    pub text: PaneText,
    pub announcer: Announcer,
}

impl PaneA11y {
    /// Refresh the model from `snap` if it changed. Returns whether it did.
    pub fn update(&mut self, snap: &GridSnapshot, announce: bool, scrolled_back: bool) -> bool {
        let fp = fingerprint(snap);
        if self.fingerprint == Some(fp) {
            return false;
        }
        self.fingerprint = Some(fp);
        self.text = extract(snap);
        if !announce || scrolled_back {
            self.announcer.reset();
        } else {
            self.announcer.observe(self.text.lines(), self.text.caret.0);
        }
        true
    }
}

fn ak_rect(r: egui::Rect) -> accesskit::Rect {
    accesskit::Rect {
        x0: r.min.x.into(),
        y0: r.min.y.into(),
        x1: r.max.x.into(),
        y1: r.max.y.into(),
    }
}

/// Give a custom-drawn widget (an `allocate_*`/`interact` region egui knows
/// nothing about) a role and a name, so it isn't an anonymous "custom" element
/// to UI Automation. A no-op without an assistive technology.
pub fn name_widget(resp: &egui::Response, role: Role, name: &str, selected: Option<bool>) {
    resp.ctx.accesskit_node_builder(resp.id, |n| {
        n.set_role(role);
        n.set_label(name);
        if let Some(s) = selected {
            n.set_selected(s);
        }
    });
}

/// Name an egui widget whose own label is missing or unhelpful (a text box
/// with only a placeholder), keeping the role egui gave it.
pub fn label_widget(resp: &egui::Response, name: &str) {
    resp.ctx
        .accesskit_node_builder(resp.id, |n| n.set_label(name));
}

/// [`label_widget`] in builder position: `named(ui.button("x"), "Close")`.
pub fn named(resp: egui::Response, name: &str) -> egui::Response {
    label_widget(&resp, name);
    resp
}

fn is_word_char(c: char) -> bool {
    c.is_alphanumeric() || c == '_'
}

/// Emit the AccessKit nodes for one pane: the `Terminal` node on `pane_id`
/// (the id the focused pane's `interact` uses, so keyboard focus and the
/// screen reader's focus are the same node) and its text runs, plus a
/// sibling polite live region when `live` is `Some`.
///
/// `grid` is the grid's top-left in points and `cell` one cell's size.
#[allow(clippy::too_many_arguments)]
pub fn build_nodes(
    ui: &mut egui::Ui,
    pane_id: egui::Id,
    name: &str,
    model: &PaneText,
    pane_rect: egui::Rect,
    grid: egui::Pos2,
    cell: egui::Vec2,
    live: Option<&str>,
) {
    let ctx = ui.ctx().clone();
    let on = ctx
        .accesskit_node_builder(pane_id, |n| {
            n.set_role(Role::Terminal);
            n.set_label(name);
            n.set_read_only();
            n.set_bounds(ak_rect(pane_rect));
        })
        .is_some();
    if !on {
        return;
    }

    // Runs are child `Ui`s parented to the pane node: `accessibility_parent`
    // is the only public way to give a node a parent other than the `Ui` it
    // is built in. `run_ids[row][chunk]`.
    let mut run_ids: Vec<Vec<egui::Id>> = Vec::with_capacity(model.rows.len());
    let last = model.rows.len().saturating_sub(1);
    for (r, row) in model.rows.iter().enumerate() {
        let newline = r < last;
        let total = row.cells.len() + newline as usize;
        let chunks = total.div_ceil(MAX_CHARS_PER_RUN).max(1);
        let chars: Vec<char> = row.text.chars().collect();
        let y = grid.y + r as f32 * cell.y;
        let mut ids = Vec::with_capacity(chunks);
        for k in 0..chunks {
            let (s, e) = (
                k * MAX_CHARS_PER_RUN,
                ((k + 1) * MAX_CHARS_PER_RUN).min(total),
            );
            let col_of = |i: usize| {
                row.cells.get(i).map(|&(c, _)| c as f32).unwrap_or_else(|| {
                    row.cells
                        .last()
                        .map_or(0.0, |&(c, w)| (c + w as u16) as f32)
                })
            };
            let x0 = grid.x + col_of(s) * cell.x;
            let x1 = grid.x + col_of(e) * cell.x;
            let rect =
                egui::Rect::from_min_max(egui::pos2(x0, y), egui::pos2(x1.max(x0), y + cell.y));
            let child = ui.new_child(
                egui::UiBuilder::new()
                    .id_salt((pane_id, "a11y-run", r, k))
                    .accessibility_parent(pane_id)
                    .max_rect(rect),
            );
            let id = child.unique_id();
            ids.push(id);

            let mut value = String::new();
            let mut lengths = Vec::with_capacity(e - s);
            let mut positions = Vec::with_capacity(e - s);
            let mut widths = Vec::with_capacity(e - s);
            let mut word_starts = Vec::new();
            let mut prev_word = false;
            for i in s..e {
                let (ch, (col, w)) = match chars.get(i) {
                    Some(&ch) => (ch, row.cells[i]),
                    None => ('\n', (col_of(i) as u16, 0)),
                };
                let word = is_word_char(ch);
                if word && !prev_word && i > s {
                    word_starts.push((i - s) as u8);
                }
                prev_word = word;
                let before = value.len();
                value.push(ch);
                lengths.push((value.len() - before) as u8);
                positions.push((col as f32 - col_of(s)) * cell.x);
                widths.push(w as f32 * cell.x);
            }
            ctx.accesskit_node_builder(id, |n| {
                n.set_role(Role::TextRun);
                n.set_text_direction(accesskit::TextDirection::LeftToRight);
                n.set_bounds(ak_rect(rect));
                n.set_value(value);
                n.set_character_lengths(lengths);
                n.set_character_positions(positions);
                n.set_character_widths(widths);
                n.set_word_starts(word_starts);
            });
            if k > 0 {
                let prev = ids[k - 1];
                ctx.accesskit_node_builder(id, |n| n.set_previous_on_line(prev.accesskit_id()));
                ctx.accesskit_node_builder(prev, |n| n.set_next_on_line(id.accesskit_id()));
            }
        }
        run_ids.push(ids);
    }

    // A position on a chunk boundary belongs to the *end* of the earlier run
    // (egui's `text_run_position` rule), so a caret at column 255 stays on the
    // first run rather than jumping to an empty second one.
    let position = |(r, c): Pos| {
        let ids = &run_ids[r.min(run_ids.len() - 1)];
        let k = if c > 0 && c.is_multiple_of(MAX_CHARS_PER_RUN) {
            c / MAX_CHARS_PER_RUN - 1
        } else {
            c / MAX_CHARS_PER_RUN
        }
        .min(ids.len() - 1);
        accesskit::TextPosition {
            node: ids[k].accesskit_id(),
            character_index: c - k * MAX_CHARS_PER_RUN,
        }
    };
    if !run_ids.is_empty() {
        let (anchor, focus) = model.selection.unwrap_or((model.caret, model.caret));
        let sel = accesskit::TextSelection {
            anchor: position(anchor),
            focus: position(focus),
        };
        ctx.accesskit_node_builder(pane_id, |n| n.set_text_selection(sel));
    }

    if let Some(text) = live {
        let child = ui.new_child(
            egui::UiBuilder::new()
                .id_salt((pane_id, "a11y-live"))
                .max_rect(egui::Rect::from_min_size(pane_rect.min, egui::Vec2::ZERO)),
        );
        ctx.accesskit_node_builder(child.unique_id(), |n| {
            n.set_role(Role::Label);
            n.set_live(accesskit::Live::Polite);
            n.set_value(text);
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::engine::Cell;

    /// A snapshot from text lines (each padded to `cols`), cursor at `cur`.
    fn snap(lines: &[&str], cols: u16, cur: (u16, u16)) -> GridSnapshot {
        let mut cells = Vec::new();
        for l in lines {
            let mut n = 0;
            for ch in l.chars() {
                let wide = unicode_width::UnicodeWidthChar::width(ch) == Some(2);
                cells.push(Cell {
                    text: ch.to_string().into(),
                    ..Default::default()
                });
                n += 1;
                if wide {
                    cells.push(Cell::default());
                    n += 1;
                }
            }
            while n < cols {
                cells.push(Cell::default());
                n += 1;
            }
        }
        GridSnapshot {
            cols,
            rows: lines.len() as u16,
            cells,
            cursor_x: cur.0,
            cursor_y: cur.1,
            ..Default::default()
        }
    }

    fn select(s: &mut GridSnapshot, from: (usize, usize), to: (usize, usize)) {
        let cols = s.cols as usize;
        for i in from.1 * cols + from.0..=to.1 * cols + to.0 {
            s.cells[i].selected = true;
        }
    }

    #[test]
    fn value_is_visible_text_trimmed() {
        let t = extract(&snap(&["hello", "", "world"], 10, (0, 2)));
        assert_eq!(t.plain(), "hello\n\nworld");
    }

    #[test]
    fn caret_follows_cursor_even_past_the_text() {
        // A prompt with the cursor after a trailing space: the space is kept
        // so the caret lands on a real position.
        let t = extract(&snap(&["PS C:\\> "], 20, (8, 0)));
        assert_eq!(t.plain(), "PS C:\\> ");
        assert_eq!(t.caret, (0, 8));
        let t = extract(&snap(&["ab", "cd"], 10, (1, 1)));
        assert_eq!(t.caret, (1, 1));
        assert_eq!(t.offset(t.caret), 4);
    }

    #[test]
    fn wide_characters_are_one_character() {
        let t = extract(&snap(&["a\u{6f22}b"], 10, (4, 0)));
        assert_eq!(t.plain(), "a\u{6f22}b");
        // Column 3 is `b` (the ideograph spans columns 1-2).
        assert_eq!(t.rows[0].char_at_col(2), 1);
        assert_eq!(t.rows[0].char_at_col(3), 2);
        assert_eq!(t.caret, (0, 3));
    }

    #[test]
    fn selection_maps_to_a_text_range() {
        let mut s = snap(&["hello world", "second line"], 12, (0, 1));
        select(&mut s, (6, 0), (5, 1));
        let t = extract(&s);
        assert_eq!(t.selection, Some(((0, 6), (1, 6))));
        assert_eq!(t.selected_text().as_deref(), Some("world\nsecond"));
    }

    #[test]
    fn selection_into_blanks_keeps_them() {
        let mut s = snap(&["ab"], 10, (0, 0));
        select(&mut s, (0, 0), (4, 0));
        let t = extract(&s);
        assert_eq!(t.selected_text().as_deref(), Some("ab   "));
    }

    #[test]
    fn fingerprint_tracks_text_cursor_and_selection() {
        let a = snap(&["x"], 4, (0, 0));
        let mut b = a.clone();
        assert_eq!(fingerprint(&a), fingerprint(&b));
        b.cursor_x = 1;
        assert_ne!(fingerprint(&a), fingerprint(&b));
        let mut c = a.clone();
        c.cells[1].selected = true;
        assert_ne!(fingerprint(&a), fingerprint(&c));
    }

    fn v(xs: &[&str]) -> Vec<String> {
        xs.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn output_without_scroll() {
        let old = v(&["$ ls", "", "", ""]);
        let new = v(&["$ ls", "a.txt", "b.txt", "$"]);
        assert_eq!(new_output(&old, &new, 3), v(&["a.txt", "b.txt", "$"]));
    }

    #[test]
    fn output_after_scroll() {
        let old = v(&["1", "2", "3", "$ cmd"]);
        let new = v(&["3", "$ cmd", "out", "$"]);
        assert_eq!(new_output(&old, &new, 3), v(&["out", "$"]));
    }

    #[test]
    fn typing_on_the_cursor_line_is_not_output() {
        let old = v(&["$ ec", ""]);
        let new = v(&["$ ech", ""]);
        assert!(new_output(&old, &new, 0).is_empty());
        assert!(new_output(&new, &new, 0).is_empty());
    }

    #[test]
    fn announcer_is_rate_limited_and_silent_on_first_sight() {
        let mut a = Announcer::default();
        a.observe(v(&["$", ""]), 0);
        assert!(!a.flush(10.0), "baseline must not be announced");
        a.observe(v(&["$", "hi", "$"]), 2);
        assert!(a.flush(10.0));
        assert_eq!(a.current.trim_end(), "hi\n$");
        a.observe(v(&["$", "hi", "$", "more", "$"]), 4);
        assert!(!a.flush(10.1), "within the interval");
        assert!(a.flush(10.6));
        assert_eq!(a.current.trim_end(), "more\n$");
    }

    #[test]
    fn repeated_announcements_still_change_the_name() {
        let mut a = Announcer::default();
        a.observe(v(&["", ""]), 1);
        a.observe(v(&["ok", ""]), 1);
        a.flush(1.0);
        let first = a.current.clone();
        a.observe(v(&["", ""]), 1);
        a.observe(v(&["ok", ""]), 1);
        a.flush(2.0);
        assert_ne!(first, a.current);
    }
}
