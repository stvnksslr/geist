//! Pane and tab drag-and-drop geometry: which drop zone of a pane the pointer
//! is in, and which window / pane / tab strip a screen point lands on.
//!
//! Pure functions over rects so the hit-testing can be table-tested; the tree
//! surgery (detach-then-insert) lives with the split tree in `app.rs`.
//!
//! Coordinates: every window reports its panes and tab strip in its own local
//! egui points, plus where its client area sits on screen. A drag is tracked in
//! **screen** points, because it can leave the window it started in — which is
//! the whole point of dragging a pane to another window. Windows on monitors
//! with different scale factors don't share one point space; see GAP.md.

use eframe::egui::{Pos2, Rect, Vec2};

/// Which side of a pane a dragged pane would land on. Ghostty's
/// `TerminalSplitDropZone`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Zone {
    Left,
    Right,
    Top,
    Bottom,
}

impl Zone {
    /// Whether dropping here makes a side-by-side split (geist's
    /// `vertical = true`: the divider is a vertical line).
    pub fn vertical(self) -> bool {
        matches!(self, Zone::Left | Zone::Right)
    }

    /// Whether the dropped pane takes the split's *first* slot.
    pub fn before(self) -> bool {
        matches!(self, Zone::Left | Zone::Top)
    }
}

/// The zone of `rect` that `p` is in: the edge it is nearest to, relative to
/// the pane's size (so the four zones are the triangles between the
/// diagonals). A straight port of `TerminalSplitDropZone.calculate`, including
/// its tie order (left, right, top, bottom).
pub fn drop_zone(rect: Rect, p: Pos2) -> Zone {
    let w = rect.width().max(f32::EPSILON);
    let h = rect.height().max(f32::EPSILON);
    let rx = (p.x - rect.left()) / w;
    let ry = (p.y - rect.top()) / h;
    let (l, r, t, b) = (rx, 1.0 - rx, ry, 1.0 - ry);
    let min = l.min(r).min(t).min(b);
    if min == l {
        Zone::Left
    } else if min == r {
        Zone::Right
    } else if min == t {
        Zone::Top
    } else {
        Zone::Bottom
    }
}

/// The half of `rect` highlighted for `zone` — upstream's overlay covers the
/// half the dropped pane will occupy, not the triangle that selects it.
pub fn zone_rect(rect: Rect, zone: Zone) -> Rect {
    let c = rect.center();
    match zone {
        Zone::Left => Rect::from_min_max(rect.min, Pos2::new(c.x, rect.max.y)),
        Zone::Right => Rect::from_min_max(Pos2::new(c.x, rect.min.y), rect.max),
        Zone::Top => Rect::from_min_max(rect.min, Pos2::new(rect.max.x, c.y)),
        Zone::Bottom => Rect::from_min_max(Pos2::new(rect.min.x, c.y), rect.max),
    }
}

/// Ghostty's grab handle: an 80×12pt strip centred on the pane's top edge.
pub const HANDLE_SIZE: Vec2 = Vec2::new(80.0, 12.0);
/// The handle is revealed anywhere in this top fraction of the pane
/// (`SurfaceGrabHandle.hoverHeightFactor`).
pub const HOVER_FACTOR: f32 = 0.2;
/// How far the pointer must travel with the button down before a press on the
/// handle becomes a drag. A click on the handle is not a move.
pub const DRAG_THRESHOLD: f32 = 4.0;

/// Where a pane's grab handle sits.
pub fn handle_rect(pane: Rect) -> Rect {
    let size = Vec2::new(
        HANDLE_SIZE.x.min(pane.width()),
        HANDLE_SIZE.y.min(pane.height()),
    );
    Rect::from_min_size(Pos2::new(pane.center().x - size.x * 0.5, pane.top()), size)
}

/// The band at the top of a pane that reveals its handle under `auto`.
pub fn hover_band(pane: Rect) -> Rect {
    Rect::from_min_max(
        pane.min,
        Pos2::new(pane.max.x, pane.top() + pane.height() * HOVER_FACTOR),
    )
}

/// One window's drop targets as laid out on its last frame.
#[derive(Clone, Debug)]
pub struct WindowGeom {
    pub window: u64,
    /// The client area, in screen points.
    pub screen: Rect,
    /// The active tab's id — only the visible tab's panes are targets.
    pub tab: u64,
    /// Visible panes, `(leaf id, rect)` in window-local points.
    pub panes: Vec<(u64, Rect)>,
    /// The tab strip, window-local; `None` when it is hidden.
    pub strip: Option<Rect>,
    /// Each tab's rect in the strip, window-local, in tab order.
    pub tab_rects: Vec<Rect>,
}

/// What a drop at some screen point lands on.
#[derive(Clone, Debug, PartialEq)]
pub enum DropTarget {
    /// Beside a pane, on `zone`.
    Pane {
        window: u64,
        tab: u64,
        leaf: u64,
        zone: Zone,
    },
    /// Into a window's tab strip, as a new tab at `index`.
    Strip { window: u64, index: usize },
    /// Inside a window, but on nothing that takes a drop (padding, chrome).
    Window { window: u64 },
    /// Outside every geist window.
    Outside,
}

/// How far above and below the strip a drop still counts as "on the strip".
/// The strip is short; a drop a few points off it plainly meant it.
const STRIP_SLOP: f32 = 6.0;

/// Resolve a drop at `screen`. When windows overlap, `prefer` (the window the
/// drag started in) wins, then list order — geist can't read the z-order.
pub fn resolve(geoms: &[WindowGeom], screen: Pos2, prefer: u64) -> DropTarget {
    let mut order: Vec<&WindowGeom> = geoms.iter().filter(|g| g.window == prefer).collect();
    order.extend(geoms.iter().filter(|g| g.window != prefer));
    for g in order {
        if !g.screen.contains(screen) {
            continue;
        }
        let local = screen - g.screen.min.to_vec2();
        if let Some(strip) = g.strip
            && strip.expand2(Vec2::new(0.0, STRIP_SLOP)).contains(local)
        {
            return DropTarget::Strip {
                window: g.window,
                index: crate::app::drop_index(&g.tab_rects, local.x),
            };
        }
        if let Some((leaf, rect)) = g.panes.iter().find(|(_, r)| r.contains(local)) {
            return DropTarget::Pane {
                window: g.window,
                tab: g.tab,
                leaf: *leaf,
                zone: drop_zone(*rect, local),
            };
        }
        return DropTarget::Window { window: g.window };
    }
    DropTarget::Outside
}

#[cfg(test)]
mod tests {
    use super::*;

    fn r(x: f32, y: f32, w: f32, h: f32) -> Rect {
        Rect::from_min_size(Pos2::new(x, y), Vec2::new(w, h))
    }

    #[test]
    fn drop_zone_is_the_nearest_edge_relative_to_size() {
        let pane = r(0.0, 0.0, 200.0, 100.0);
        assert_eq!(drop_zone(pane, Pos2::new(10.0, 50.0)), Zone::Left);
        assert_eq!(drop_zone(pane, Pos2::new(190.0, 50.0)), Zone::Right);
        assert_eq!(drop_zone(pane, Pos2::new(100.0, 5.0)), Zone::Top);
        assert_eq!(drop_zone(pane, Pos2::new(100.0, 95.0)), Zone::Bottom);
        // Relative, not absolute: 30pt from the left of a 200-wide pane is
        // 15%, while 20pt from the top of a 100-tall one is 20% — left wins.
        assert_eq!(drop_zone(pane, Pos2::new(30.0, 20.0)), Zone::Left);
        // Dead centre ties everything; upstream's order picks left.
        assert_eq!(drop_zone(pane, Pos2::new(100.0, 50.0)), Zone::Left);
    }

    #[test]
    fn zone_rect_is_the_half_the_pane_will_take() {
        let pane = r(10.0, 20.0, 200.0, 100.0);
        assert_eq!(zone_rect(pane, Zone::Left), r(10.0, 20.0, 100.0, 100.0));
        assert_eq!(zone_rect(pane, Zone::Right), r(110.0, 20.0, 100.0, 100.0));
        assert_eq!(zone_rect(pane, Zone::Top), r(10.0, 20.0, 200.0, 50.0));
        assert_eq!(zone_rect(pane, Zone::Bottom), r(10.0, 70.0, 200.0, 50.0));
    }

    #[test]
    fn zone_axis_and_side() {
        assert!(Zone::Left.vertical() && Zone::Left.before());
        assert!(Zone::Right.vertical() && !Zone::Right.before());
        assert!(!Zone::Top.vertical() && Zone::Top.before());
        assert!(!Zone::Bottom.vertical() && !Zone::Bottom.before());
    }

    #[test]
    fn handle_is_centred_on_the_top_edge_and_shrinks_with_the_pane() {
        assert_eq!(
            handle_rect(r(0.0, 0.0, 200.0, 100.0)),
            r(60.0, 0.0, 80.0, 12.0)
        );
        assert_eq!(handle_rect(r(0.0, 0.0, 40.0, 8.0)), r(0.0, 0.0, 40.0, 8.0));
        assert_eq!(
            hover_band(r(0.0, 10.0, 200.0, 100.0)),
            r(0.0, 10.0, 200.0, 20.0)
        );
    }

    fn two_windows() -> Vec<WindowGeom> {
        vec![
            WindowGeom {
                window: 1,
                screen: r(100.0, 100.0, 400.0, 300.0),
                tab: 7,
                panes: vec![
                    (1, r(0.0, 30.0, 200.0, 270.0)),
                    (2, r(200.0, 30.0, 200.0, 270.0)),
                ],
                strip: Some(r(0.0, 0.0, 400.0, 30.0)),
                tab_rects: vec![r(0.0, 0.0, 100.0, 30.0), r(100.0, 0.0, 100.0, 30.0)],
            },
            WindowGeom {
                window: 2,
                screen: r(300.0, 200.0, 400.0, 300.0),
                tab: 9,
                panes: vec![(5, r(0.0, 0.0, 400.0, 300.0))],
                strip: None,
                tab_rects: vec![],
            },
        ]
    }

    #[test]
    fn resolve_finds_pane_strip_window_and_outside() {
        let g = two_windows();
        assert_eq!(
            resolve(&g, Pos2::new(110.0, 250.0), 1),
            DropTarget::Pane {
                window: 1,
                tab: 7,
                leaf: 1,
                zone: Zone::Left
            }
        );
        // The strip, with the index from the tab centres.
        assert_eq!(
            resolve(&g, Pos2::new(260.0, 110.0), 1),
            DropTarget::Strip {
                window: 1,
                index: 2
            }
        );
        assert_eq!(
            resolve(&g, Pos2::new(120.0, 110.0), 1),
            DropTarget::Strip {
                window: 1,
                index: 0
            }
        );
        assert_eq!(resolve(&g, Pos2::new(50.0, 50.0), 1), DropTarget::Outside);
        // Only window 2 covers this point.
        assert!(matches!(
            resolve(&g, Pos2::new(650.0, 450.0), 1),
            DropTarget::Pane {
                window: 2,
                leaf: 5,
                ..
            }
        ));
    }

    #[test]
    fn resolve_prefers_the_source_window_where_they_overlap() {
        let g = two_windows();
        let p = Pos2::new(450.0, 300.0); // inside both
        assert!(matches!(
            resolve(&g, p, 1),
            DropTarget::Pane { window: 1, .. }
        ));
        assert!(matches!(
            resolve(&g, p, 2),
            DropTarget::Pane { window: 2, .. }
        ));
    }

    #[test]
    fn resolve_inside_a_window_but_off_every_target() {
        let mut g = two_windows();
        g[0].panes.clear();
        assert_eq!(
            resolve(&g, Pos2::new(110.0, 250.0), 1),
            DropTarget::Window { window: 1 }
        );
    }
}
