//! `window-padding-color = extend | extend-always`: paint the padding band
//! around a pane's grid in the colour of the nearest cell, so a full-screen
//! program's background runs to the window edge instead of stopping at a
//! frame of the default background.
//!
//! Ghostty does this per pixel in its cell shader (the padding pixel clamps to
//! the nearest grid cell); geist paints the same result as rectangles — one per
//! edge row/column plus the four corners — because its per-pane scissor is the
//! grid box and so the renderer cannot draw outside it. The decisions are
//! here, pure, and the app only paints the rects this returns.

use eframe::egui;

use crate::config::PaddingColor;
use crate::engine::{Cell, GridSnapshot, Rgb};

/// Which padding edges extend. Mirrors Ghostty's `padding_extend` uniform.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Edges {
    pub up: bool,
    pub down: bool,
    pub left: bool,
    pub right: bool,
}

/// Powerline "perfect fit" glyphs, which look wrong stretched into the padding
/// (upstream's `neverExtendBg` list, verbatim).
fn is_powerline(c: char) -> bool {
    matches!(c as u32, 0xE0B0..=0xE0C8 | 0xE0CA | 0xE0CC..=0xE0D2 | 0xE0D4)
}

/// The background a cell actually paints, or `None` when it paints none (it
/// is on the default background, so the window fill shows through).
fn cell_bg(cell: &Cell) -> Option<Rgb> {
    (cell.bg_explicit || cell.inverse || cell.selected).then_some(cell.bg)
}

/// Upstream's `row.neverExtendBg`: a row whose background should not be
/// stretched vertically — any cell on the default background (the default
/// already looks right as padding), or any powerline glyph.
///
/// Upstream also refuses a *prompt* row (OSC 133); the grid snapshot carries
/// no per-row semantic marks, so that one heuristic is not applied here.
pub fn row_never_extends(row: &[Cell], default_bg: Rgb) -> bool {
    row.iter().any(|c| {
        c.text.chars().next().is_some_and(is_powerline)
            || cell_bg(c).is_none_or(|bg| bg == default_bg)
    })
}

/// Which edges extend for `mode`. `background` extends nothing,
/// `extend-always` everything, and `extend` always extends sideways but only
/// extends up/down when the nearest row passes [`row_never_extends`] —
/// upstream applies that regardless of primary/alternate screen.
pub fn extend_edges(mode: PaddingColor, snap: &GridSnapshot) -> Edges {
    let all = Edges {
        up: true,
        down: true,
        left: true,
        right: true,
    };
    match mode {
        PaddingColor::Background => Edges::default(),
        PaddingColor::ExtendAlways => all,
        PaddingColor::Extend => {
            if snap.cols == 0 || snap.rows == 0 {
                return Edges::default();
            }
            let cols = snap.cols as usize;
            let row = |y: usize| &snap.cells[y * cols..(y + 1) * cols];
            let last = snap.rows as usize - 1;
            Edges {
                up: !row_never_extends(row(0), snap.default_bg),
                down: !row_never_extends(row(last), snap.default_bg),
                ..all
            }
        }
    }
}

/// The padding rectangles to paint, as `(rect, colour)` in points.
///
/// `pane` is the whole pane, `grid` the cell area inside it; `cell` is one
/// cell's size in points. Only cells that paint a background contribute — a
/// default-background cell extends as "nothing", which is what the window
/// fill already shows.
pub fn padding_rects(
    snap: &GridSnapshot,
    edges: Edges,
    pane: egui::Rect,
    grid: egui::Rect,
    cell: egui::Vec2,
) -> Vec<(egui::Rect, Rgb)> {
    let mut out = Vec::new();
    if snap.cols == 0
        || snap.rows == 0
        || snap.cells.len() < snap.cols as usize * snap.rows as usize
    {
        return out;
    }
    let (cols, rows) = (snap.cols, snap.rows);
    let at = |x: u16, y: u16| snap.cell(x, y).and_then(cell_bg);
    let mut push = |r: egui::Rect, c: Option<Rgb>| {
        if let Some(c) = c
            && r.width() > 0.0
            && r.height() > 0.0
        {
            out.push((r, c));
        }
    };
    let (gl, gr, gt, gb) = (
        grid.left(),
        grid.left() + cols as f32 * cell.x,
        grid.top(),
        grid.top() + rows as f32 * cell.y,
    );
    let row_y = |y: u16| (gt + y as f32 * cell.y, gt + (y + 1) as f32 * cell.y);
    let col_x = |x: u16| (gl + x as f32 * cell.x, gl + (x + 1) as f32 * cell.x);
    let span = |x0: f32, x1: f32, y0: f32, y1: f32| {
        egui::Rect::from_min_max(egui::pos2(x0, y0), egui::pos2(x1, y1))
    };
    if edges.left {
        for y in 0..rows {
            let (y0, y1) = row_y(y);
            push(span(pane.left(), gl, y0, y1), at(0, y));
        }
    }
    if edges.right {
        for y in 0..rows {
            let (y0, y1) = row_y(y);
            push(span(gr, pane.right(), y0, y1), at(cols - 1, y));
        }
    }
    if edges.up {
        for x in 0..cols {
            let (x0, x1) = col_x(x);
            push(span(x0, x1, pane.top(), gt), at(x, 0));
        }
    }
    if edges.down {
        for x in 0..cols {
            let (x0, x1) = col_x(x);
            push(span(x0, x1, gb, pane.bottom()), at(x, rows - 1));
        }
    }
    // Corners take the corner cell, and only when both of their edges extend
    // (a clamped lookup in the shader lands on the corner cell the same way).
    if edges.up && edges.left {
        push(span(pane.left(), gl, pane.top(), gt), at(0, 0));
    }
    if edges.up && edges.right {
        push(span(gr, pane.right(), pane.top(), gt), at(cols - 1, 0));
    }
    if edges.down && edges.left {
        push(span(pane.left(), gl, gb, pane.bottom()), at(0, rows - 1));
    }
    if edges.down && edges.right {
        push(
            span(gr, pane.right(), gb, pane.bottom()),
            at(cols - 1, rows - 1),
        );
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    const DEF: Rgb = Rgb { r: 0, g: 0, b: 0 };
    const RED: Rgb = Rgb { r: 200, g: 0, b: 0 };

    fn cell(bg: Option<Rgb>, text: &str) -> Cell {
        Cell {
            text: text.into(),
            bg: bg.unwrap_or(DEF),
            bg_explicit: bg.is_some(),
            ..Default::default()
        }
    }

    fn grid(cols: u16, rows: u16, f: impl Fn(u16, u16) -> Cell) -> GridSnapshot {
        let mut s = GridSnapshot {
            cols,
            rows,
            default_bg: DEF,
            ..Default::default()
        };
        for y in 0..rows {
            for x in 0..cols {
                s.cells.push(f(x, y));
            }
        }
        s
    }

    #[test]
    fn modes_select_edges() {
        let full = grid(2, 2, |_, _| cell(Some(RED), "a"));
        assert_eq!(
            extend_edges(PaddingColor::Background, &full),
            Edges::default()
        );
        let all = Edges {
            up: true,
            down: true,
            left: true,
            right: true,
        };
        assert_eq!(extend_edges(PaddingColor::ExtendAlways, &full), all);
        assert_eq!(extend_edges(PaddingColor::Extend, &full), all);
    }

    #[test]
    fn extend_heuristics_only_gate_vertical_edges() {
        // Bottom row has one default-background cell: no downward extension.
        let s = grid(2, 2, |x, y| cell((y == 0 || x == 0).then_some(RED), "a"));
        let e = extend_edges(PaddingColor::Extend, &s);
        assert!(e.up && !e.down && e.left && e.right);
        // A cell explicitly set to the default colour counts as default.
        let s = grid(2, 1, |x, _| cell(Some(if x == 0 { RED } else { DEF }), "a"));
        assert!(!extend_edges(PaddingColor::Extend, &s).up);
        // A powerline glyph vetoes too, even on a coloured cell.
        let s = grid(2, 1, |x, _| {
            cell(Some(RED), if x == 1 { "\u{E0B0}" } else { "a" })
        });
        assert!(!extend_edges(PaddingColor::Extend, &s).up);
        // extend-always ignores all of it.
        assert!(extend_edges(PaddingColor::ExtendAlways, &s).up);
    }

    #[test]
    fn rects_cover_the_band_in_the_nearest_cell_colour() {
        // 2x1 grid, left cell red, right cell default. 10pt padding all round.
        let s = grid(2, 1, |x, _| cell((x == 0).then_some(RED), "a"));
        let pane = egui::Rect::from_min_size(egui::pos2(0.0, 0.0), egui::vec2(40.0, 40.0));
        let grid_r = egui::Rect::from_min_size(egui::pos2(10.0, 10.0), egui::vec2(20.0, 20.0));
        let all = Edges {
            up: true,
            down: true,
            left: true,
            right: true,
        };
        let rects = padding_rects(&s, all, pane, grid_r, egui::vec2(10.0, 20.0));
        let r = |x0: f32, y0: f32, x1: f32, y1: f32| {
            egui::Rect::from_min_max(egui::pos2(x0, y0), egui::pos2(x1, y1))
        };
        // Left band, the top/bottom strips above/below column 0, and the two
        // left corners — every one red. Nothing on the right: that cell paints
        // no background.
        let want = vec![
            (r(0.0, 10.0, 10.0, 30.0), RED),
            (r(10.0, 0.0, 20.0, 10.0), RED),
            (r(10.0, 30.0, 20.0, 40.0), RED),
            (r(0.0, 0.0, 10.0, 10.0), RED),
            (r(0.0, 30.0, 10.0, 40.0), RED),
        ];
        assert_eq!(rects, want);
        assert!(
            padding_rects(&s, Edges::default(), pane, grid_r, egui::vec2(10.0, 20.0)).is_empty()
        );
    }
}
