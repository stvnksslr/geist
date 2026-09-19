//! Pure logic behind the small in-window indicators: the key-sequence /
//! key-table pill, the search bar's snap corner and match count, the per-tab
//! `OSC 9;4` progress bar, and the undo/redo toast text.
//!
//! Kept out of `app.rs` so each decision can be table-tested; the app only
//! paints what these return.

use eframe::egui;

use crate::keybind::{Chord, TableEntry};
use crate::taskbar::Progress;

/// A corner of a pane, for the search bar (macOS `SurfaceSearchOverlay.Corner`).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum Corner {
    TopLeft,
    #[default]
    TopRight,
    BottomLeft,
    BottomRight,
}

impl Corner {
    /// The egui anchor for an `Area` pinned to this corner, with `margin`
    /// points of inset.
    pub fn anchor(self, margin: f32) -> (egui::Align2, egui::Vec2) {
        match self {
            Corner::TopLeft => (egui::Align2::LEFT_TOP, egui::vec2(margin, margin)),
            Corner::TopRight => (egui::Align2::RIGHT_TOP, egui::vec2(-margin, margin)),
            Corner::BottomLeft => (egui::Align2::LEFT_BOTTOM, egui::vec2(margin, -margin)),
            Corner::BottomRight => (egui::Align2::RIGHT_BOTTOM, egui::vec2(-margin, -margin)),
        }
    }
}

/// Where a dragged search bar lands: the quadrant its centre was dropped in.
/// Upstream's `closestCorner` — strict `<` on both axes, so the exact midpoint
/// goes right/bottom.
pub fn closest_corner(center: egui::Pos2, container: egui::Rect) -> Corner {
    let mid = container.center();
    match (center.x < mid.x, center.y < mid.y) {
        (true, true) => Corner::TopLeft,
        (true, false) => Corner::BottomLeft,
        (false, true) => Corner::TopRight,
        (false, false) => Corner::BottomRight,
    }
}

/// The search bar's match counter, as upstream prints it: `3/17` for the
/// selected match, empty with no query, `0/0` when nothing matched.
pub fn search_count_label(count: usize, current: usize, query_empty: bool) -> String {
    if count == 0 {
        if query_empty { String::new() } else { "0/0".to_string() }
    } else {
        format!("{}/{}", current.min(count - 1) + 1, count)
    }
}

/// One chord in config spelling (`ctrl+shift+a`), for the key-state pill.
pub fn chord_label(chord: &Chord) -> String {
    let mut s = String::new();
    let m = chord.mods;
    for (on, name) in [(m.ctrl, "ctrl"), (m.alt, "alt"), (m.shift, "shift"), (m.sup, "super")] {
        if on {
            s.push_str(name);
            s.push('+');
        }
    }
    s.push_str(crate::keybind::key_name(chord.code));
    s
}

/// What the key-state indicator shows (macOS `KeyStateIndicator`): the active
/// key-table stack, outermost first, and the leaders of an unfinished key
/// sequence. `None` when neither is in play, so nothing is drawn.
pub fn key_state_label(pending: &[Chord], tables: &[TableEntry]) -> Option<String> {
    let mut parts: Vec<String> = Vec::new();
    if !tables.is_empty() {
        let names: Vec<&str> = tables.iter().map(|t| t.name.as_str()).collect();
        parts.push(format!("[{}]", names.join(" › ")));
    }
    if !pending.is_empty() {
        let keys: Vec<String> = pending.iter().map(chord_label).collect();
        parts.push(format!("{} …", keys.join(" ")));
    }
    (!parts.is_empty()).then(|| parts.join("  |  "))
}

/// The filled span of a tab's progress bar as fractions `(start, end)` of its
/// width, or `None` for no bar. Indeterminate progress is a third-width
/// segment sweeping across once per `SWEEP_SECS`, driven by `t` (seconds).
pub fn progress_span(p: Progress, t: f64) -> Option<(f32, f32)> {
    const SWEEP_SECS: f64 = 1.5;
    const SEG: f32 = 1.0 / 3.0;
    match p {
        Progress::None => None,
        Progress::Normal(v) | Progress::Error(v) | Progress::Paused(v) => {
            Some((0.0, f32::from(v.min(100)) / 100.0))
        }
        Progress::Indeterminate => {
            let phase = (t.rem_euclid(SWEEP_SECS) / SWEEP_SECS) as f32;
            // Travel from fully off the left edge to fully off the right one.
            let start = phase * (1.0 + SEG) - SEG;
            Some((start.max(0.0), (start + SEG).min(1.0)))
        }
    }
}

/// What an undo entry did, for the toast that confirms it (the macOS app
/// names the operation in its Edit menu; a toast is the Windows-visible form).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum UndoKind {
    ReopenSplit,
    CloseSplit,
    ReopenTabs(usize),
    CloseTabs(usize),
    ReopenWindow,
    CloseWindow,
}

/// The toast text for applying `kind` as an undo (`redo == false`) or a redo.
pub fn undo_toast(kind: UndoKind, redo: bool) -> String {
    let verb = if redo { "Redo" } else { "Undo" };
    let tabs = |n: usize| if n == 1 { "tab".to_string() } else { format!("{n} tabs") };
    let what = match kind {
        UndoKind::ReopenSplit => "reopened split".to_string(),
        UndoKind::CloseSplit => "closed split".to_string(),
        UndoKind::ReopenTabs(n) => format!("reopened {}", tabs(n)),
        UndoKind::CloseTabs(n) => format!("closed {}", tabs(n)),
        UndoKind::ReopenWindow => "reopened window".to_string(),
        UndoKind::CloseWindow => "closed window".to_string(),
    };
    format!("{verb}: {what}")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::engine::{KeyCode, KeyMods};

    #[test]
    fn corners_follow_the_drop_quadrant() {
        let r = egui::Rect::from_min_size(egui::pos2(100.0, 50.0), egui::vec2(400.0, 200.0));
        assert_eq!(closest_corner(egui::pos2(110.0, 60.0), r), Corner::TopLeft);
        assert_eq!(closest_corner(egui::pos2(490.0, 60.0), r), Corner::TopRight);
        assert_eq!(closest_corner(egui::pos2(110.0, 240.0), r), Corner::BottomLeft);
        assert_eq!(closest_corner(egui::pos2(490.0, 240.0), r), Corner::BottomRight);
        // The exact centre is not "less than" either midline.
        assert_eq!(closest_corner(r.center(), r), Corner::BottomRight);
    }

    #[test]
    fn corner_anchors_inset_toward_the_pane() {
        let (a, off) = Corner::BottomLeft.anchor(8.0);
        assert_eq!(a, egui::Align2::LEFT_BOTTOM);
        assert_eq!(off, egui::vec2(8.0, -8.0));
        let (a, off) = Corner::TopRight.anchor(8.0);
        assert_eq!(a, egui::Align2::RIGHT_TOP);
        assert_eq!(off, egui::vec2(-8.0, 8.0));
    }

    #[test]
    fn search_count_matches_upstream_format() {
        assert_eq!(search_count_label(0, 0, true), "");
        assert_eq!(search_count_label(0, 0, false), "0/0");
        assert_eq!(search_count_label(17, 2, false), "3/17");
        // A stale index never reads past the total.
        assert_eq!(search_count_label(3, 9, false), "3/3");
    }

    fn chord(ctrl: bool, shift: bool, code: KeyCode) -> Chord {
        Chord {
            mods: KeyMods { ctrl, shift, ..Default::default() },
            code,
        }
    }

    #[test]
    fn chords_read_in_config_spelling() {
        assert_eq!(chord_label(&chord(true, false, KeyCode::A)), "ctrl+a");
        assert_eq!(chord_label(&chord(true, true, KeyCode::ArrowLeft)), "ctrl+shift+left");
        assert_eq!(chord_label(&chord(false, false, KeyCode::Enter)), "enter");
    }

    #[test]
    fn key_state_shows_tables_and_pending_leaders() {
        assert_eq!(key_state_label(&[], &[]), None);
        let tables = vec![
            TableEntry { name: "resize".into(), once: false },
            TableEntry { name: "fine".into(), once: true },
        ];
        assert_eq!(key_state_label(&[], &tables).unwrap(), "[resize › fine]");
        let pending = [chord(true, false, KeyCode::A)];
        assert_eq!(key_state_label(&pending, &[]).unwrap(), "ctrl+a …");
        assert_eq!(
            key_state_label(&pending, &tables[..1]).unwrap(),
            "[resize]  |  ctrl+a …"
        );
    }

    #[test]
    fn progress_spans() {
        assert_eq!(progress_span(Progress::None, 0.0), None);
        assert_eq!(progress_span(Progress::Normal(50), 0.0), Some((0.0, 0.5)));
        assert_eq!(progress_span(Progress::Error(100), 0.0), Some((0.0, 1.0)));
        assert_eq!(progress_span(Progress::Paused(0), 0.0), Some((0.0, 0.0)));
        // Indeterminate: starts off-screen left (clamped), sweeps right.
        let (a, b) = progress_span(Progress::Indeterminate, 0.0).unwrap();
        assert_eq!(a, 0.0);
        assert!(b.abs() < 1e-6);
        let (a, b) = progress_span(Progress::Indeterminate, 0.75).unwrap();
        assert!(a > 0.0 && b < 1.0 && (b - a - 1.0 / 3.0).abs() < 1e-5);
    }

    #[test]
    fn undo_toasts_name_the_operation() {
        assert_eq!(undo_toast(UndoKind::ReopenTabs(1), false), "Undo: reopened tab");
        assert_eq!(undo_toast(UndoKind::ReopenTabs(3), false), "Undo: reopened 3 tabs");
        assert_eq!(undo_toast(UndoKind::CloseSplit, true), "Redo: closed split");
        assert_eq!(undo_toast(UndoKind::ReopenWindow, false), "Undo: reopened window");
    }
}
