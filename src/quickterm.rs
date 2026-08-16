//! The **quick terminal** — Ghostty's dropdown/Quake-style window: a terminal
//! that slides in over whatever you were doing, on a global hotkey, and goes
//! away again.
//!
//! This module is the part that can be reasoned about without a window: the
//! `quick-terminal-size` grammar, and the geometry that turns a position + a
//! size + the screen's work area into a frame. Both are direct ports of
//! Ghostty's `QuickTerminalSize.calculate` and `QuickTerminalPosition`
//! (`finalOrigin`) so the two agree on every default and rounding decision, and
//! both are pure so a table test can pin them — upstream computes the same
//! numbers inside AppKit calls that can only be checked by looking at a screen.
//!
//! **The one coordinate difference**: AppKit's `visibleFrame` is Y-**up** with
//! the origin at the bottom-left of the screen, and Windows' work area is
//! Y-**down** from the top-left. The ports below are written for Y-down, so
//! `top` places the window at the work area's `y` and `bottom` at its bottom
//! edge minus the height — which is the *opposite* arithmetic to the Swift, and
//! the same visual result. Flipping this "back" to match the source line for
//! line would put the window off the wrong edge.

/// Where the quick terminal appears. Ghostty `quick-terminal-position`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum Position {
    #[default]
    Top,
    Bottom,
    Left,
    Right,
    Center,
}

impl Position {
    pub fn parse(v: &str) -> Option<Self> {
        Some(match v.trim().to_ascii_lowercase().as_str() {
            "top" => Position::Top,
            "bottom" => Position::Bottom,
            "left" => Position::Left,
            "right" => Position::Right,
            "center" => Position::Center,
            _ => return None,
        })
    }
}

/// One axis of `quick-terminal-size`: a percentage of the screen or an absolute
/// pixel count. A bare number is a **config error** upstream, and is here too —
/// `20` is ambiguous between "20%" and "20px", and guessing either would be
/// wrong most of the time.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Size {
    Percentage(f32),
    Pixels(u32),
}

impl Size {
    pub fn parse(input: &str) -> Option<Self> {
        let s = input.trim();
        if let Some(n) = s.strip_suffix("px") {
            return n.trim().parse::<u32>().ok().map(Size::Pixels);
        }
        if let Some(n) = s.strip_suffix('%') {
            let v: f32 = n.trim().parse().ok()?;
            // Upstream rejects a negative percentage rather than clamping it.
            return (v >= 0.0).then_some(Size::Percentage(v));
        }
        None
    }

    /// Resolve against the parent dimension, exactly as `toPixels` does.
    pub fn to_pixels(self, parent: f32) -> f32 {
        match self {
            Size::Percentage(v) => parent * v / 100.0,
            Size::Pixels(v) => v as f32,
        }
    }
}

/// `quick-terminal-size`: the primary axis (whichever one the position makes
/// meaningful) and optionally the secondary one, comma-separated. Either may be
/// unset, in which case `calculate` supplies Ghostty's default for that axis.
#[derive(Clone, Copy, Debug, PartialEq, Default)]
pub struct QuickSize {
    pub primary: Option<Size>,
    pub secondary: Option<Size>,
}

impl QuickSize {
    /// Parse `50%`, `300px`, or `50%,500px`. Returns `None` if any component is
    /// malformed — a half-applied size would be harder to spot than a rejected
    /// one.
    pub fn parse(v: &str) -> Option<Self> {
        let mut parts = v.split(',');
        let primary = Size::parse(parts.next()?)?;
        let secondary = match parts.next() {
            Some(s) => Some(Size::parse(s)?),
            None => None,
        };
        // A third component is a typo, not an axis.
        if parts.next().is_some() {
            return None;
        }
        Some(Self {
            primary: Some(primary),
            secondary,
        })
    }

    /// The window size for this position on a screen whose work area is
    /// `screen_w` × `screen_h`. A direct port of `QuickTerminalSize.calculate`,
    /// **including its defaults** (400 on the primary axis, the full screen on
    /// the secondary, and 800×400 for a centered landscape window).
    pub fn calculate(&self, position: Position, screen_w: f32, screen_h: f32) -> (f32, f32) {
        let p = |d: f32, default: f32| self.primary.map_or(default, |s| s.to_pixels(d));
        let s = |d: f32, default: f32| self.secondary.map_or(default, |s| s.to_pixels(d));
        match position {
            Position::Left | Position::Right => (p(screen_w, 400.0), s(screen_h, screen_h)),
            Position::Top | Position::Bottom => (s(screen_w, screen_w), p(screen_h, 400.0)),
            Position::Center => {
                if screen_w >= screen_h {
                    // Landscape: the primary axis is the width.
                    (p(screen_w, 800.0), s(screen_h, 400.0))
                } else {
                    (s(screen_w, 400.0), p(screen_h, 800.0))
                }
            }
        }
    }
}

/// A screen rectangle in physical pixels, top-left origin (Windows convention).
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Rect {
    pub x: f32,
    pub y: f32,
    pub w: f32,
    pub h: f32,
}

/// The final on-screen frame for the quick terminal: `calculate`'s size, placed
/// against the work area per `position`.
///
/// The size is clamped to the work area — a `200%` or a `4000px` on a 1080p
/// screen would otherwise put most of the window (and possibly all of its text)
/// off the edge, with nothing on screen to say why.
pub fn frame(position: Position, size: &QuickSize, work: Rect) -> Rect {
    let (w, h) = size.calculate(position, work.w, work.h);
    let w = w.clamp(1.0, work.w);
    let h = h.clamp(1.0, work.h);
    // Centering is rounded, like upstream: a half-pixel origin blurs the whole
    // window under any scaling.
    let cx = (work.x + (work.w - w) / 2.0).round();
    let cy = (work.y + (work.h - h) / 2.0).round();
    let (x, y) = match position {
        Position::Top => (cx, work.y),
        Position::Bottom => (cx, work.y + work.h - h),
        Position::Left => (work.x, cy),
        Position::Right => (work.x + work.w - w, cy),
        Position::Center => (cx, cy),
    };
    Rect { x, y, w, h }
}

/// The primary monitor's **work area** — the desktop minus the taskbar — in
/// physical pixels.
///
/// `quick-terminal-screen` is honored only as `main`: the other values are
/// macOS concepts (`macos-menu-bar`) or need per-monitor enumeration giest has
/// no handle for (`mouse`), so they fall back here rather than silently placing
/// the window somewhere unrelated.
///
/// The taskbar exclusion is the point: a `bottom`-positioned quick terminal that
/// used the full screen would sit *under* the taskbar.
#[cfg(windows)]
pub fn work_area() -> Option<Rect> {
    #[repr(C)]
    struct WinRect {
        left: i32,
        top: i32,
        right: i32,
        bottom: i32,
    }
    const SPI_GETWORKAREA: u32 = 0x0030;

    #[link(name = "user32")]
    unsafe extern "system" {
        fn SystemParametersInfoW(action: u32, param: u32, pv: *mut std::ffi::c_void, ini: u32)
        -> i32;
    }

    let mut r = WinRect {
        left: 0,
        top: 0,
        right: 0,
        bottom: 0,
    };
    // SAFETY: SPI_GETWORKAREA writes exactly one RECT through `pv`.
    let ok = unsafe {
        SystemParametersInfoW(
            SPI_GETWORKAREA,
            0,
            (&raw mut r).cast::<std::ffi::c_void>(),
            0,
        )
    };
    if ok == 0 || r.right <= r.left || r.bottom <= r.top {
        return None;
    }
    Some(Rect {
        x: r.left as f32,
        y: r.top as f32,
        w: (r.right - r.left) as f32,
        h: (r.bottom - r.top) as f32,
    })
}

#[cfg(not(windows))]
pub fn work_area() -> Option<Rect> {
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    const SCREEN: Rect = Rect {
        x: 0.0,
        y: 0.0,
        w: 1920.0,
        h: 1040.0,
    };

    #[test]
    fn size_grammar_matches_ghosttys() {
        assert_eq!(Size::parse("20%"), Some(Size::Percentage(20.0)));
        assert_eq!(Size::parse("300px"), Some(Size::Pixels(300)));
        assert_eq!(Size::parse(" 12.5 % "), Some(Size::Percentage(12.5)));
        // A bare number is a config error upstream, not a default unit.
        assert_eq!(Size::parse("300"), None);
        assert_eq!(Size::parse("-5%"), None);
        assert_eq!(Size::parse(""), None);
        assert_eq!(Size::parse("px"), None);
    }

    #[test]
    fn a_second_component_sets_the_secondary_axis() {
        let s = QuickSize::parse("50%,500px").expect("parses");
        assert_eq!(s.primary, Some(Size::Percentage(50.0)));
        assert_eq!(s.secondary, Some(Size::Pixels(500)));
        assert_eq!(QuickSize::parse("50%").unwrap().secondary, None);
        // One bad component rejects the whole value rather than half-applying it.
        assert_eq!(QuickSize::parse("50%,nope"), None);
        assert_eq!(QuickSize::parse("50%,10%,10%"), None);
    }

    #[test]
    fn unset_axes_use_ghosttys_defaults() {
        let none = QuickSize::default();
        // Top/bottom: full screen wide, 400 tall.
        assert_eq!(none.calculate(Position::Top, 1920.0, 1040.0), (1920.0, 400.0));
        // Left/right: 400 wide, full height.
        assert_eq!(none.calculate(Position::Left, 1920.0, 1040.0), (400.0, 1040.0));
        // Center on a landscape screen: 800×400…
        assert_eq!(none.calculate(Position::Center, 1920.0, 1040.0), (800.0, 400.0));
        // …and the axes swap on a portrait one.
        assert_eq!(none.calculate(Position::Center, 1080.0, 1920.0), (400.0, 800.0));
    }

    #[test]
    fn a_single_size_applies_to_the_position_dependent_primary_axis() {
        let s = QuickSize::parse("25%").unwrap();
        // Top: the primary axis is height.
        assert_eq!(s.calculate(Position::Top, 1920.0, 1040.0), (1920.0, 260.0));
        // Left: the primary axis is width.
        assert_eq!(s.calculate(Position::Left, 1920.0, 1040.0), (480.0, 1040.0));
    }

    #[test]
    fn each_position_anchors_to_its_own_edge() {
        let s = QuickSize::parse("25%").unwrap();
        let top = frame(Position::Top, &s, SCREEN);
        assert_eq!((top.x, top.y), (0.0, 0.0));
        let bottom = frame(Position::Bottom, &s, SCREEN);
        assert_eq!(bottom.y + bottom.h, SCREEN.h, "bottom must sit on the bottom edge");
        let left = frame(Position::Left, &s, SCREEN);
        assert_eq!(left.x, 0.0);
        let right = frame(Position::Right, &s, SCREEN);
        assert_eq!(right.x + right.w, SCREEN.w);
        let center = frame(Position::Center, &QuickSize::default(), SCREEN);
        assert_eq!(center.x, (1920.0 - 800.0) / 2.0);
        assert_eq!(center.y, (1040.0 - 400.0) / 2.0);
    }

    #[test]
    fn the_frame_is_offset_by_the_work_areas_own_origin() {
        // A taskbar on the left, or a second monitor: the work area doesn't
        // start at 0,0 and every anchor has to follow it.
        let work = Rect {
            x: 100.0,
            y: 40.0,
            w: 1000.0,
            h: 800.0,
        };
        let f = frame(Position::Top, &QuickSize::default(), work);
        assert_eq!((f.x, f.y, f.w), (100.0, 40.0, 1000.0));
        let b = frame(Position::Bottom, &QuickSize::default(), work);
        assert_eq!(b.y + b.h, 840.0);
    }

    #[test]
    fn an_oversized_value_is_clamped_into_the_work_area() {
        let s = QuickSize::parse("200%,300%").unwrap();
        let f = frame(Position::Center, &s, SCREEN);
        assert_eq!((f.w, f.h), (SCREEN.w, SCREEN.h));
        assert!(f.x >= 0.0 && f.y >= 0.0, "a clamped window still starts on screen");
    }

    #[test]
    fn position_parses_ghosttys_five_values() {
        for (s, p) in [
            ("top", Position::Top),
            ("bottom", Position::Bottom),
            ("left", Position::Left),
            ("right", Position::Right),
            ("CENTER", Position::Center),
        ] {
            assert_eq!(Position::parse(s), Some(p));
        }
        assert_eq!(Position::parse("middle"), None);
    }
}
