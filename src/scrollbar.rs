//! Scrollbar geometry: where the thumb sits in its track, and where a drag on
//! it lands in the scrollback.
//!
//! Pure math — no egui, no engine calls, and deliberately no DPI. Everything
//! here consumes a *ratio* of rows and produces a *ratio* of the track, so
//! points and rows are the only units and `points_per_point` never enters the
//! picture; [`crate::session::Session`] does the one rows↔pixels conversion.
//!
//! The state contract is Ghostty's `{ total, offset, len }`, all in rows:
//! `total` scrollable rows, the viewport's `offset` from the top of them, and
//! the viewport's height `len`. geist reconstructs it from its own scroll state
//! rather than calling the binding's `Terminal::scrollbar()` — see
//! [`Session::scrollbar_state`](crate::session::Session::scrollbar_state).

use crate::config::Scrollbar;

/// Minimum thumb length, in points.
///
/// Ghostty's only explicit thumb geometry (`src/inspector/widgets/pagelist.zig`)
/// floors at 4.0 — but that is its ImGui *inspector* widget, not the app's
/// scroller, which is an AppKit `NSScroller` carrying AppKit's own (~20 pt)
/// minimum knob. 4 pt is unhittable with a mouse, so we take the real-scroller
/// value: with a 10k-line scrollback and a 24-row viewport the unclamped thumb
/// would be under 2 pt.
pub const MIN_THUMB_PTS: f32 = 20.0;

/// A thumb's position within its track, in points from the track's top.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Thumb {
    pub top: f32,
    pub len: f32,
    /// Usable travel, `track_h - len`: the distance `top` ranges over, and the
    /// denominator [`offset_from_thumb_top`] inverts through.
    pub travel: f32,
}

/// Thumb geometry for the `{ total, offset, len }` state (rows). `offset` is
/// deliberately `f32` — it carries the sub-line position of a smooth scroll, so
/// the thumb glides rather than stepping a whole cell at a time.
///
/// `None` when there is nothing to scroll (`len >= total`) or the track is too
/// short to hold a usable thumb — the caller draws nothing in either case.
pub fn thumb(track_h: f32, total: f32, offset: f32, len: f32) -> Option<Thumb> {
    // Reject non-finite inputs up front. Every ordering comparison below is
    // false against a NaN, so without this a NaN would slip past the range
    // guards and come back out as a NaN rect the painter would silently drop.
    if !(track_h.is_finite() && total.is_finite() && len.is_finite() && offset.is_finite()) {
        return None;
    }
    if track_h <= 0.0 || total <= 0.0 || len >= total {
        return None;
    }
    let max_offset = total - len; // > 0, given the guard above
    let thumb_len = (len / total * track_h).max(MIN_THUMB_PTS).min(track_h);
    let travel = track_h - thumb_len;
    if travel <= 0.0 {
        return None;
    }
    // Ghostty's inspector uses `top = (offset / total) * track_h`. That is the
    // *same* value whenever the minimum-size floor is inactive — unclamped
    // `thumb_len = len/total*track_h`, so `travel = track_h * max_offset/total`
    // and `offset/max_offset * travel == offset/total * track_h`. Once the floor
    // engages the two diverge, and Ghostty's form lets `top + len` exceed the
    // track and run the thumb off the bottom. Scaling by `travel` keeps the
    // thumb inside the track by construction and keeps the inverse exact.
    let top = (offset.clamp(0.0, max_offset) / max_offset) * travel;
    Some(Thumb {
        top,
        len: thumb_len,
        travel,
    })
}

/// A pointer-derived thumb top (points from the track's top, with the grab
/// offset already subtracted) back to an `offset` in rows.
///
/// The exact inverse of [`thumb`]'s `top` within range, and clamped outside it
/// so dragging past either end pins rather than overshoots. Ghostty's
/// `row_delta = (dy / track_h) * total` is the delta form of this map when the
/// minimum-size floor is inactive; the absolute form degrades correctly when it
/// isn't, and needs no delta accumulator.
pub fn offset_from_thumb_top(top: f32, t: &Thumb, total: f32, len: f32) -> f32 {
    let max_offset = (total - len).max(0.0);
    if t.travel <= 0.0 {
        return max_offset;
    }
    (top / t.travel).clamp(0.0, 1.0) * max_offset
}

/// Which way a press at `y` (points from the track's top) pages: `-1` above the
/// thumb, `+1` below it, `0` on it — and a press on the thumb starts a drag
/// instead of paging.
pub fn track_click_page(y: f32, t: &Thumb) -> i8 {
    if y < t.top {
        -1
    } else if y >= t.top + t.len {
        1
    } else {
        0
    }
}

/// Whether a pane is eligible for a scrollbar at all, before any fade state.
///
/// Hidden with nothing to scroll, and hidden while the program is reporting the
/// mouse: an alt-screen TUI has no scrollback to point at, and a bar over it
/// would compete with its own mouse handling for the pointer.
pub fn eligible(mode: Scrollbar, scrollback_rows: usize, tracking: bool) -> bool {
    matches!(mode, Scrollbar::System) && scrollback_rows > 0 && !tracking
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A viewport small enough that the minimum-size floor stays out of the way.
    const BIG: (f32, f32, f32) = (200.0, 100.0, 25.0); // track_h, total, len

    #[test]
    fn thumb_is_pinned_at_both_ends() {
        let (h, total, len) = BIG;
        let top = thumb(h, total, 0.0, len).unwrap();
        assert_eq!(top.top, 0.0);
        let bottom = thumb(h, total, total - len, len).unwrap();
        assert_eq!(bottom.top, bottom.travel);
        // Never escapes the track at either extreme.
        for t in [top, bottom] {
            assert!(t.top >= 0.0 && t.top + t.len <= h + 1e-4, "{t:?}");
        }
    }

    #[test]
    fn thumb_length_is_the_viewport_fraction_of_total() {
        let (h, total, len) = BIG;
        // A quarter of the content is on screen, so a quarter of the track.
        assert_eq!(thumb(h, total, 0.0, len).unwrap().len, 50.0);
    }

    #[test]
    fn thumb_length_clamps_to_minimum_and_stays_in_track() {
        // 24 rows visible out of 100k: the unclamped thumb would be 0.05 pt.
        let t = thumb(200.0, 100_000.0, 0.0, 24.0).unwrap();
        assert_eq!(t.len, MIN_THUMB_PTS);
        // At the far end the thumb still ends exactly at the track's bottom —
        // the clamp Ghostty's inspector formula lacks (it would give
        // `top = 200 * (99976/100000) = 199.95`, overhanging by ~20 pt).
        let end = thumb(200.0, 100_000.0, 100_000.0 - 24.0, 24.0).unwrap();
        assert!((end.top + end.len - 200.0).abs() < 1e-3, "{end:?}");
    }

    #[test]
    fn thumb_matches_ghosttys_formula_when_unclamped() {
        let (h, total, len) = BIG;
        for offset in [0.0, 10.0, 37.5, 75.0] {
            let t = thumb(h, total, offset, len).unwrap();
            assert!((t.len - len / total * h).abs() < 1e-3, "len at {offset}");
            assert!((t.top - offset / total * h).abs() < 1e-3, "top at {offset}");
        }
    }

    #[test]
    fn no_thumb_when_nothing_to_scroll() {
        // The whole buffer fits on screen — this is the empty-scrollback case.
        assert!(thumb(200.0, 24.0, 0.0, 24.0).is_none());
        assert!(thumb(200.0, 24.0, 0.0, 30.0).is_none());
        // Degenerate inputs must not divide by zero or emit NaN rects.
        assert!(thumb(200.0, 0.0, 0.0, 0.0).is_none());
        assert!(thumb(0.0, 100.0, 0.0, 25.0).is_none());
        // A NaN in any slot must reject, not paint a NaN rect.
        assert!(thumb(f32::NAN, 100.0, 0.0, 25.0).is_none());
        assert!(thumb(200.0, f32::NAN, 0.0, 25.0).is_none());
        assert!(thumb(200.0, 100.0, f32::NAN, 25.0).is_none());
        assert!(thumb(200.0, 100.0, 0.0, f32::NAN).is_none());
        // Track shorter than the minimum thumb: no travel, so nothing to drag.
        assert!(thumb(MIN_THUMB_PTS - 1.0, 100.0, 0.0, 1.0).is_none());
    }

    #[test]
    fn offset_from_thumb_top_inverts_thumb_top() {
        // Both regimes: floor inactive (a quarter on screen) and floor active
        // (24 rows of 100k). The round trip is what stands in for Ghostty's
        // drag-suppression logic — if it isn't an identity, a dragged thumb
        // fights the cursor.
        for (h, total, len) in [BIG, (400.0, 100_000.0, 24.0)] {
            let max = total - len;
            for offset in [0.0, 1.0, 7.5, max / 2.0, max] {
                let t = thumb(h, total, offset, len).unwrap();
                let back = offset_from_thumb_top(t.top, &t, total, len);
                assert!(
                    (back - offset).abs() < 1e-3,
                    "total={total} offset={offset} -> top={} -> {back}",
                    t.top
                );
            }
        }
    }

    #[test]
    fn drag_past_the_ends_clamps() {
        let (h, total, len) = BIG;
        let t = thumb(h, total, 0.0, len).unwrap();
        assert_eq!(offset_from_thumb_top(-50.0, &t, total, len), 0.0);
        assert_eq!(
            offset_from_thumb_top(t.travel + 50.0, &t, total, len),
            total - len
        );
    }

    #[test]
    fn track_click_pages_away_from_the_thumb() {
        let t = Thumb {
            top: 40.0,
            len: 50.0,
            travel: 150.0,
        };
        assert_eq!(track_click_page(0.0, &t), -1);
        assert_eq!(track_click_page(39.9, &t), -1);
        // The top edge belongs to the thumb; the bottom edge is already past it.
        assert_eq!(track_click_page(40.0, &t), 0);
        assert_eq!(track_click_page(89.9, &t), 0);
        assert_eq!(track_click_page(90.0, &t), 1);
        assert_eq!(track_click_page(200.0, &t), 1);
    }

    #[test]
    fn eligible_gating_table() {
        assert!(eligible(Scrollbar::System, 500, false));
        // Explicitly disabled.
        assert!(!eligible(Scrollbar::Never, 500, false));
        // Nothing scrolled off the top yet.
        assert!(!eligible(Scrollbar::System, 0, false));
        // An alt-screen TUI owns the mouse.
        assert!(!eligible(Scrollbar::System, 500, true));
    }
}
