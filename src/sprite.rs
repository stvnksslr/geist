//! **Sprite glyphs** — box drawing, block elements and braille drawn by giest
//! rather than taken from the font, mirroring Ghostty's `font/sprite`.
//!
//! Why a terminal draws these itself: the characters are *defined* relative to
//! the character cell, and a font's versions are drawn relative to its em box.
//! With any line spacing at all, a font's `│` stops short of the cell edges and
//! a column of them shows a dashed seam; `─` between two cells leaves a hairline
//! gap; and `█` doesn't quite cover its cell. Drawing them from the **cell**
//! metrics makes every one of those exact by construction — a `─` reaches both
//! edges, so neighbours connect with no seam whatever the font.
//!
//! Like upstream, the sprite face **wins over the font**: Ghostty's
//! `CodepointResolver` checks its sprite face before any font lookup (after only
//! the explicit codepoint overrides), so these characters look the same in every
//! font. giest does the same.
//!
//! Everything here is pure — a codepoint plus [`Metrics`] in, an 8-bit coverage
//! buffer out — so the geometry is unit-testable to the pixel. That is the whole
//! reason it lives outside `render/atlas.rs`: this is exactly the "compute the
//! expected pixels rather than eyeball them" case, and the rasterizer is the
//! only part that needs a GPU.
//!
//! Ported ranges: **U+2500–257F** box drawing *in full* (intersections, dashes,
//! rounded corners and diagonals), **U+2580–259F** block elements and shades,
//! **U+2800–28FF** braille, and the geometric **powerline** separators
//! (U+E0B0–E0BF plus E0D2 / E0D4).
//!
//! Two rasterizing primitives serve all of it: axis-aligned rectangles for the
//! straight work, and — for the curves and slopes — an anti-aliased pair of a
//! distance-field stroke and a supersampled polygon fill. Deliberately *not*
//! ported, so they still come from the font: the stylized powerline symbols
//! (E0C0+, E0D0/E0D1/E0D3 — flames, hexagons, ice), which upstream doesn't draw
//! either, and the legacy-computing symbols.

/// Cell metrics a sprite is drawn against, in **physical pixels**.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Metrics {
    pub w: u32,
    pub h: u32,
    /// The base ("light") line thickness. `heavy` is twice this and
    /// `super-light` is half, per Ghostty's `Thickness`.
    pub thickness: u32,
}

impl Metrics {
    /// Metrics for a cell, with Ghostty's default line thickness.
    ///
    /// Upstream derives `box_thickness` from the font's underline thickness;
    /// giest's atlas doesn't carry that, so it uses a fraction of the cell
    /// height — the same shape of rule, and the one users actually tune via
    /// `adjust-box-thickness`. Never zero: a zero-thickness line is invisible,
    /// which reads as the character being missing.
    pub fn new(w: u32, h: u32) -> Self {
        Self {
            w,
            h,
            thickness: (h / 12).max(1),
        }
    }

    /// The pixel thickness for a line weight. Ghostty also has a `super_light`
    /// (half) weight, used only by the legacy-computing symbols this port
    /// leaves to the font.
    fn line(self, weight: Weight) -> u32 {
        match weight {
            Weight::None => 0,
            Weight::Light | Weight::Double => self.thickness,
            Weight::Heavy => self.thickness * 2,
        }
    }
}

/// The weight of one arm of a box-drawing character.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Weight {
    None,
    Light,
    Heavy,
    Double,
}

impl Weight {
    fn from_bits(b: u8) -> Self {
        match b & 0b11 {
            1 => Weight::Light,
            2 => Weight::Heavy,
            3 => Weight::Double,
            _ => Weight::None,
        }
    }
}

/// The four arms of a box-drawing intersection character.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Lines {
    up: Weight,
    right: Weight,
    down: Weight,
    left: Weight,
}

impl Lines {
    fn from_bits(b: u8) -> Self {
        Self {
            up: Weight::from_bits(b),
            right: Weight::from_bits(b >> 2),
            down: Weight::from_bits(b >> 4),
            left: Weight::from_bits(b >> 6),
        }
    }
}

/// Arm weights for U+2500–257F, two bits per arm (up, right, down, left), or
/// `0` for the codepoints in the range that aren't intersection characters
/// (the dashes, the arcs and the diagonals).
///
/// Transcribed **mechanically** from Ghostty's `box.zig` `linesChar` calls
/// rather than by hand: 109 entries of four arms each is exactly the kind of
/// table where a single typo produces one subtly wrong corner that nobody
/// notices until a TUI looks broken.
#[rustfmt::skip]
const LINES: [u8; 128] = [
    0x44, 0x88, 0x11, 0x22, 0x00, 0x00, 0x00, 0x00,
    0x00, 0x00, 0x00, 0x00, 0x14, 0x18, 0x24, 0x28,
    0x50, 0x90, 0x60, 0xa0, 0x05, 0x09, 0x06, 0x0a,
    0x41, 0x81, 0x42, 0x82, 0x15, 0x19, 0x16, 0x25,
    0x26, 0x1a, 0x29, 0x2a, 0x51, 0x91, 0x52, 0x61,
    0x62, 0x92, 0xa1, 0xa2, 0x54, 0x94, 0x58, 0x98,
    0x64, 0xa4, 0x68, 0xa8, 0x45, 0x85, 0x49, 0x89,
    0x46, 0x86, 0x4a, 0x8a, 0x55, 0x95, 0x59, 0x99,
    0x56, 0x65, 0x66, 0x96, 0x5a, 0xa5, 0x69, 0x9a,
    0xa9, 0xa6, 0x6a, 0xaa, 0x00, 0x00, 0x00, 0x00,
    0xcc, 0x33, 0x1c, 0x34, 0x3c, 0xd0, 0x70, 0xf0,
    0x0d, 0x07, 0x0f, 0xc1, 0x43, 0xc3, 0x1d, 0x37,
    0x3f, 0xd1, 0x73, 0xf3, 0xdc, 0x74, 0xfc, 0xcd,
    0x47, 0xcf, 0xdd, 0x77, 0xff, 0x00, 0x00, 0x00,
    0x00, 0x00, 0x00, 0x00, 0x40, 0x01, 0x04, 0x10,
    0x80, 0x02, 0x08, 0x20, 0x48, 0x21, 0x84, 0x12,
];

/// A coverage canvas: one byte per pixel, 0 = empty, 255 = full.
struct Canvas {
    data: Vec<u8>,
    w: i32,
    h: i32,
}

impl Canvas {
    fn new(m: Metrics) -> Self {
        Self {
            data: vec![0; (m.w * m.h) as usize],
            w: m.w as i32,
            h: m.h as i32,
        }
    }

    /// Fill `[x0,x1) × [y0,y1)` with `shade`, clipped to the canvas.
    ///
    /// Clipping rather than asserting is deliberate: several of Ghostty's draw
    /// routines intentionally overshoot the cell so strokes meet across cell
    /// boundaries, and at tiny cell sizes the arithmetic can round outside.
    /// A clipped rectangle is right; a panic in the renderer is not.
    fn rect(&mut self, x0: i32, y0: i32, x1: i32, y1: i32, shade: u8) {
        for y in y0.max(0)..y1.min(self.h) {
            for x in x0.max(0)..x1.min(self.w) {
                self.data[(y * self.w + x) as usize] = shade;
            }
        }
    }

    /// Composite a coverage buffer (same dimensions) with `max`, so overlapping
    /// strokes never darken each other — the shapes are all one solid colour.
    fn merge(&mut self, cov: &[f64]) {
        for (dst, &c) in self.data.iter_mut().zip(cov) {
            let v = (c.clamp(0.0, 1.0) * 255.0).round() as u8;
            *dst = (*dst).max(v);
        }
    }

    fn stroke(&mut self, pts: &[Point], thick: f64) {
        let cov = stroke_coverage(pts, thick, self.w, self.h);
        self.merge(&cov);
    }

    fn fill_poly(&mut self, pts: &[Point]) {
        let cov = fill_coverage(pts, self.w, self.h);
        self.merge(&cov);
    }

    /// Ghostty's `innerStrokePath`: a stroke that lies entirely *inside* a
    /// closed path rather than straddling it.
    ///
    /// Needed by the half-circle separators, whose curve bulges to exactly the
    /// cell edge — a centred stroke there would hang half outside and be clipped
    /// flat. Approximated as an intersection: stroke at double width, then
    /// multiply by the path's own fill coverage, leaving a band of `thick`
    /// entirely within the boundary.
    fn stroke_inside(&mut self, pts: &[Point], thick: f64) {
        let stroke = stroke_coverage(pts, thick * 2.0, self.w, self.h);
        let inside = fill_coverage(pts, self.w, self.h);
        let cov: Vec<f64> = stroke.iter().zip(&inside).map(|(a, b)| a * b).collect();
        self.merge(&cov);
    }

    /// Mirror the canvas horizontally. Ghostty's `flipHorizontal`, used to build
    /// each right-facing powerline separator from its left-facing twin — one
    /// geometry, so the pair can't drift apart.
    fn flip_horizontal(&mut self) {
        let w = self.w as usize;
        for row in self.data.chunks_mut(w) {
            row.reverse();
        }
    }
}

/// A point in cell space (pixels, top-left origin). May sit outside the cell:
/// several shapes deliberately overshoot so they meet across cell boundaries.
type Point = (f64, f64);

/// Distance from `p` to the segment `a`–`b`.
fn dist_to_segment(p: Point, a: Point, b: Point) -> f64 {
    let (dx, dy) = (b.0 - a.0, b.1 - a.1);
    let len2 = dx * dx + dy * dy;
    let t = if len2 <= f64::EPSILON {
        0.0
    } else {
        (((p.0 - a.0) * dx + (p.1 - a.1) * dy) / len2).clamp(0.0, 1.0)
    };
    let (cx, cy) = (a.0 + t * dx, a.1 + t * dy);
    ((p.0 - cx).powi(2) + (p.1 - cy).powi(2)).sqrt()
}

/// Anti-aliased coverage for a polyline stroked at `thick` pixels wide.
///
/// A signed-distance field rather than a polygon stroker: coverage is
/// `thick/2 + 0.5 - d` clamped to `0..1`, which is the exact area a
/// round-capped stroke covers to within a pixel and needs no join/cap geometry
/// at all. The divergence that buys: Ghostty butt-caps its strokes and joins
/// them mitre-ish, so a chevron's point is rounded here by `thick/2`. At the
/// 1–2px thicknesses a terminal cell implies that is sub-pixel, and a cap/join
/// system would be a lot of machinery for it.
fn stroke_coverage(pts: &[Point], thick: f64, w: i32, h: i32) -> Vec<f64> {
    let mut cov = vec![0.0; (w * h) as usize];
    if pts.len() < 2 || thick <= 0.0 {
        return cov;
    }
    let r = thick / 2.0;
    for y in 0..h {
        for x in 0..w {
            let p = (x as f64 + 0.5, y as f64 + 0.5);
            let mut best = f64::MAX;
            for seg in pts.windows(2) {
                best = best.min(dist_to_segment(p, seg[0], seg[1]));
                if best <= 0.0 {
                    break;
                }
            }
            let c = (r + 0.5 - best).clamp(0.0, 1.0);
            if c > 0.0 {
                cov[(y * w + x) as usize] = c;
            }
        }
    }
    cov
}

/// Subsamples per axis for polygon fills: 8×8 gives 64 coverage levels, which
/// is plenty for a cell-sized glyph and costs nothing — every sprite is
/// rasterized once per (character, cell size) and cached.
const SUBSAMPLES: i32 = 8;

/// Anti-aliased coverage for a closed polygon (implicitly closed; the crossing
/// number is taken over every edge including last→first).
fn fill_coverage(pts: &[Point], w: i32, h: i32) -> Vec<f64> {
    let mut cov = vec![0.0; (w * h) as usize];
    if pts.len() < 3 {
        return cov;
    }
    let step = 1.0 / SUBSAMPLES as f64;
    for y in 0..h {
        for x in 0..w {
            let mut hits = 0;
            for sy in 0..SUBSAMPLES {
                for sx in 0..SUBSAMPLES {
                    let p = (
                        x as f64 + (sx as f64 + 0.5) * step,
                        y as f64 + (sy as f64 + 0.5) * step,
                    );
                    if inside(pts, p) {
                        hits += 1;
                    }
                }
            }
            if hits > 0 {
                cov[(y * w + x) as usize] = hits as f64 / (SUBSAMPLES * SUBSAMPLES) as f64;
            }
        }
    }
    cov
}

/// Crossing-number point-in-polygon test.
fn inside(pts: &[Point], p: Point) -> bool {
    let mut c = false;
    let mut j = pts.len() - 1;
    for i in 0..pts.len() {
        let (a, b) = (pts[i], pts[j]);
        if (a.1 > p.1) != (b.1 > p.1) {
            let x = (b.0 - a.0) * (p.1 - a.1) / (b.1 - a.1) + a.0;
            if p.0 < x {
                c = !c;
            }
        }
        j = i;
    }
    c
}

/// Sample a cubic Bézier into `steps` line segments, appending to `out` (the
/// start point is assumed already present).
///
/// Uniform in `t` rather than adaptive: at cell sizes the whole curve is a
/// dozen pixels long, so 24 segments is far below one pixel of error.
fn cubic(out: &mut Vec<Point>, p0: Point, c1: Point, c2: Point, p1: Point) {
    const STEPS: i32 = 24;
    for i in 1..=STEPS {
        let t = i as f64 / STEPS as f64;
        let u = 1.0 - t;
        let (a, b, c, d) = (u * u * u, 3.0 * u * u * t, 3.0 * u * t * t, t * t * t);
        out.push((
            a * p0.0 + b * c1.0 + c * c2.0 + d * p1.0,
            a * p0.1 + b * c1.1 + c * c2.1 + d * p1.1,
        ));
    }
}

/// Whether giest draws this character itself instead of asking the font.
pub fn covers(ch: char) -> bool {
    let c = ch as u32;
    matches!(c,
        0x2500..=0x257F        // box drawing, complete
        | 0x2580..=0x259F      // block elements
        | 0x2800..=0x28FF      // braille
        // Powerline: the geometric separators. E0C0+ are the stylized ones
        // (flames, hexagons, ice), which upstream doesn't draw either, so they
        // stay on the Nerd Font — as do E0D0/E0D1/E0D3.
        | 0xE0B0..=0xE0BF
        | 0xE0D2
        | 0xE0D4
    )
}

/// Draw `ch` into an `m.w × m.h` coverage buffer, or `None` if it isn't a
/// sprite character.
pub fn draw(ch: char, m: Metrics) -> Option<Vec<u8>> {
    if !covers(ch) || m.w == 0 || m.h == 0 {
        return None;
    }
    let mut c = Canvas::new(m);
    let cp = ch as u32;
    match cp {
        0x2500..=0x257F => draw_box(&mut c, m, cp),
        0x2580..=0x259F => draw_block(&mut c, m, cp),
        0x2800..=0x28FF => draw_braille(&mut c, m, cp),
        0xE0B0..=0xE0BF | 0xE0D2 | 0xE0D4 => draw_powerline(&mut c, m, cp),
        _ => return None,
    }
    Some(c.data)
}

fn draw_box(c: &mut Canvas, m: Metrics, cp: u32) {
    // The dashed lines. `count` is the number of dashes across the cell.
    let light = m.line(Weight::Light);
    let heavy = m.line(Weight::Heavy);
    match cp {
        0x2504 => return dash_h(c, m, 3, light),
        0x2505 => return dash_h(c, m, 3, heavy),
        0x2506 => return dash_v(c, m, 3, light),
        0x2507 => return dash_v(c, m, 3, heavy),
        0x2508 => return dash_h(c, m, 4, light),
        0x2509 => return dash_h(c, m, 4, heavy),
        0x250A => return dash_v(c, m, 4, light),
        0x250B => return dash_v(c, m, 4, heavy),
        0x254C => return dash_h(c, m, 2, light),
        0x254D => return dash_h(c, m, 2, heavy),
        0x254E => return dash_v(c, m, 2, light),
        0x254F => return dash_v(c, m, 2, heavy),
        _ => {}
    }
    match cp {
        // Rounded corners. The `Corner` names describe which way the arc bulges,
        // matching upstream's `arc(.br)` etc. rather than the character's name.
        0x256D => return arc(c, m, Corner::Br),
        0x256E => return arc(c, m, Corner::Bl),
        0x256F => return arc(c, m, Corner::Tl),
        0x2570 => return arc(c, m, Corner::Tr),
        0x2571 => return diagonal(c, m, Diagonal::UpRightToDownLeft),
        0x2572 => return diagonal(c, m, Diagonal::UpLeftToDownRight),
        0x2573 => {
            diagonal(c, m, Diagonal::UpRightToDownLeft);
            return diagonal(c, m, Diagonal::UpLeftToDownRight);
        }
        _ => {}
    }
    let bits = LINES[(cp - 0x2500) as usize];
    if bits != 0 {
        lines_char(c, m, Lines::from_bits(bits));
    }
}

/// Which way a rounded corner bulges.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Corner {
    Tl,
    Tr,
    Bl,
    Br,
}

/// A rounded corner (`╭╮╯╰`): a straight arm, a quarter-turn, another arm.
///
/// The centre is `(dim - thick)/2 + thick/2`, **not** `dim/2` — that is what
/// puts the arms on exactly the columns/rows `lines_char` draws `│` and `─` on,
/// so a rounded box lines up with a square one. It looks simplifiable and isn't.
/// The control-point fraction `s = 0.25` is upstream's; it is *not* the circular
/// arc constant used by the half-circle separators, and the two must not be
/// unified.
fn arc(c: &mut Canvas, m: Metrics, corner: Corner) {
    let thick = m.line(Weight::Light) as f64;
    let (fw, fh) = (m.w as f64, m.h as f64);
    let cx = ((m.w.saturating_sub(m.line(Weight::Light))) / 2) as f64 + thick / 2.0;
    let cy = ((m.h.saturating_sub(m.line(Weight::Light))) / 2) as f64 + thick / 2.0;
    let r = fw.min(fh) / 2.0;
    const S: f64 = 0.25;

    let mut pts: Vec<Point> = Vec::with_capacity(32);
    match corner {
        Corner::Tl => {
            pts.push((cx, 0.0));
            pts.push((cx, cy - r));
            cubic(
                &mut pts,
                (cx, cy - r),
                (cx, cy - S * r),
                (cx - S * r, cy),
                (cx - r, cy),
            );
            pts.push((0.0, cy));
        }
        Corner::Tr => {
            pts.push((cx, 0.0));
            pts.push((cx, cy - r));
            cubic(
                &mut pts,
                (cx, cy - r),
                (cx, cy - S * r),
                (cx + S * r, cy),
                (cx + r, cy),
            );
            pts.push((fw, cy));
        }
        Corner::Bl => {
            pts.push((cx, fh));
            pts.push((cx, cy + r));
            cubic(
                &mut pts,
                (cx, cy + r),
                (cx, cy + S * r),
                (cx - S * r, cy),
                (cx - r, cy),
            );
            pts.push((0.0, cy));
        }
        Corner::Br => {
            pts.push((cx, fh));
            pts.push((cx, cy + r));
            cubic(
                &mut pts,
                (cx, cy + r),
                (cx, cy + S * r),
                (cx + S * r, cy),
                (cx + r, cy),
            );
            pts.push((fw, cy));
        }
    }
    c.stroke(&pts, thick);
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Diagonal {
    UpLeftToDownRight,
    UpRightToDownLeft,
}

/// A diagonal (`╱╲╳`), overshooting each corner by half a slope step.
///
/// The overshoot is upstream's and load-bearing: without it two diagonals in
/// adjacent cells meet at a point instead of continuing, and a `╳` grid shows
/// dots at every corner. The endpoints are deliberately outside the cell; the
/// coverage code clips.
fn diagonal(c: &mut Canvas, m: Metrics, dir: Diagonal) {
    let (fw, fh) = (m.w as f64, m.h as f64);
    let slope_x = (fw / fh).min(1.0);
    let slope_y = (fh / fw).min(1.0);
    let pts: [Point; 2] = match dir {
        Diagonal::UpLeftToDownRight => [
            (-0.5 * slope_x, -0.5 * slope_y),
            (fw + 0.5 * slope_x, fh + 0.5 * slope_y),
        ],
        Diagonal::UpRightToDownLeft => [
            (fw + 0.5 * slope_x, -0.5 * slope_y),
            (-0.5 * slope_x, fh + 0.5 * slope_y),
        ],
    };
    c.stroke(&pts, m.line(Weight::Light) as f64);
}

/// Powerline separators (U+E0B0–E0BF, plus the two "trapezoid" pieces at E0D2 /
/// E0D4). Ported from `powerline.zig`.
///
/// The right-facing member of each pair is built by drawing its left-facing
/// twin and mirroring, exactly as upstream does — one geometry per pair, so the
/// two can't drift apart.
fn draw_powerline(c: &mut Canvas, m: Metrics, cp: u32) {
    let (fw, fh) = (m.w as f64, m.h as f64);
    let thick = m.line(Weight::Light) as f64;
    // The circle-approximating cubic constant. Distinct from the arcs' `S`.
    let k: f64 = (std::f64::consts::SQRT_2 - 1.0) * 4.0 / 3.0;

    // The half-circle boundary, shared by the filled and stroked forms.
    let half_circle = || {
        let radius = fw.min(fh / 2.0);
        let mut pts: Vec<Point> = vec![(0.0, 0.0)];
        cubic(
            &mut pts,
            (0.0, 0.0),
            (radius * k, 0.0),
            (radius, radius - radius * k),
            (radius, radius),
        );
        pts.push((radius, fh - radius));
        cubic(
            &mut pts,
            (radius, fh - radius),
            (radius, fh - radius + radius * k),
            (radius * k, fh),
            (0.0, fh),
        );
        pts
    };
    // The two trapezoid halves of E0D2, split by a gap of `thick` at the centre.
    let trapezoids = |c: &mut Canvas| {
        c.fill_poly(&[
            (0.0, 0.0),
            (fw, 0.0),
            (fw / 2.0, fh / 2.0 - thick / 2.0),
            (0.0, fh / 2.0 - thick / 2.0),
        ]);
        c.fill_poly(&[
            (0.0, fh),
            (fw, fh),
            (fw / 2.0, fh / 2.0 + thick / 2.0),
            (0.0, fh / 2.0 + thick / 2.0),
        ]);
    };

    match cp {
        // Solid left-pointing triangle, and its mirror. Upstream spells the
        // mirror out as its own triangle; giest flips instead, because the
        // supersample grid is not symmetric about the cell's centre — the two
        // spellings differ by a few levels along the hypotenuse, and a
        // `` and a `` meeting in a prompt would then have visibly
        // different edges.
        0xE0B0 => c.fill_poly(&[(0.0, 0.0), (fw, fh / 2.0), (0.0, fh)]),
        0xE0B2 => {
            c.fill_poly(&[(0.0, 0.0), (fw, fh / 2.0), (0.0, fh)]);
            c.flip_horizontal();
        }
        // The same outline as a stroke (the "thin" chevrons).
        0xE0B1 => c.stroke(&[(0.0, 0.0), (fw, fh / 2.0), (0.0, fh)], thick),
        0xE0B3 => {
            c.stroke(&[(0.0, 0.0), (fw, fh / 2.0), (0.0, fh)], thick);
            c.flip_horizontal();
        }
        // Half circles, filled and stroked.
        0xE0B4 => c.fill_poly(&half_circle()),
        0xE0B6 => {
            c.fill_poly(&half_circle());
            c.flip_horizontal();
        }
        0xE0B5 => c.stroke_inside(&half_circle(), thick),
        0xE0B7 => {
            c.stroke_inside(&half_circle(), thick);
            c.flip_horizontal();
        }
        // The corner triangles, and the diagonals that go with them.
        0xE0B8 => c.fill_poly(&[(0.0, 0.0), (fw, fh), (0.0, fh)]),
        0xE0BA => c.fill_poly(&[(fw, 0.0), (fw, fh), (0.0, fh)]),
        0xE0BC => c.fill_poly(&[(0.0, 0.0), (fw, 0.0), (0.0, fh)]),
        0xE0BE => c.fill_poly(&[(0.0, 0.0), (fw, 0.0), (fw, fh)]),
        0xE0B9 | 0xE0BF => diagonal(c, m, Diagonal::UpLeftToDownRight),
        0xE0BB | 0xE0BD => diagonal(c, m, Diagonal::UpRightToDownLeft),
        0xE0D2 => trapezoids(c),
        0xE0D4 => {
            trapezoids(c);
            c.flip_horizontal();
        }
        _ => {}
    }
}

/// Draw an intersection character. A port of `box.zig`'s `linesChar`, whose
/// four "where does this arm stop" expressions are the whole difficulty: an arm
/// runs past the centre to the *far* side of the crossing line so the corner is
/// solid, except where the crossing is doubled or absent.
fn lines_char(c: &mut Canvas, m: Metrics, l: Lines) {
    let light = m.line(Weight::Light) as i32;
    let heavy = m.line(Weight::Heavy) as i32;
    let (w, h) = (m.w as i32, m.h as i32);

    let h_light_top = (h - light).max(0) / 2;
    let h_light_bottom = h_light_top + light;
    let h_heavy_top = (h - heavy).max(0) / 2;
    let h_heavy_bottom = h_heavy_top + heavy;
    let h_double_top = h_light_top - light;
    let h_double_bottom = h_light_bottom + light;

    let v_light_left = (w - light).max(0) / 2;
    let v_light_right = v_light_left + light;
    let v_heavy_left = (w - heavy).max(0) / 2;
    let v_heavy_right = v_heavy_left + heavy;
    let v_double_left = v_light_left - light;
    let v_double_right = v_light_right + light;

    let is_double = |a: Weight, b: Weight| a == Weight::Double || b == Weight::Double;

    let up_bottom = if l.left == Weight::Heavy || l.right == Weight::Heavy {
        h_heavy_bottom
    } else if l.left != l.right || l.down == l.up {
        if is_double(l.left, l.right) {
            h_double_bottom
        } else {
            h_light_bottom
        }
    } else if l.left == Weight::None && l.right == Weight::None {
        h_light_bottom
    } else {
        h_light_top
    };

    let down_top = if l.left == Weight::Heavy || l.right == Weight::Heavy {
        h_heavy_top
    } else if l.left != l.right || l.up == l.down {
        if is_double(l.left, l.right) {
            h_double_top
        } else {
            h_light_top
        }
    } else if l.left == Weight::None && l.right == Weight::None {
        h_light_top
    } else {
        h_light_bottom
    };

    let left_right = if l.up == Weight::Heavy || l.down == Weight::Heavy {
        v_heavy_right
    } else if l.up != l.down || l.left == l.right {
        if is_double(l.up, l.down) {
            v_double_right
        } else {
            v_light_right
        }
    } else if l.up == Weight::None && l.down == Weight::None {
        v_light_right
    } else {
        v_light_left
    };

    let right_left = if l.up == Weight::Heavy || l.down == Weight::Heavy {
        v_heavy_left
    } else if l.up != l.down || l.right == l.left {
        if is_double(l.up, l.down) {
            v_double_left
        } else {
            v_light_left
        }
    } else if l.up == Weight::None && l.down == Weight::None {
        v_light_left
    } else {
        v_light_right
    };

    match l.up {
        Weight::None => {}
        Weight::Heavy => c.rect(v_heavy_left, 0, v_heavy_right, up_bottom, 0xFF),
        Weight::Double => {
            let left_bottom = if l.left == Weight::Double {
                h_light_top
            } else {
                up_bottom
            };
            let right_bottom = if l.right == Weight::Double {
                h_light_top
            } else {
                up_bottom
            };
            c.rect(v_double_left, 0, v_light_left, left_bottom, 0xFF);
            c.rect(v_light_right, 0, v_double_right, right_bottom, 0xFF);
        }
        _ => c.rect(v_light_left, 0, v_light_right, up_bottom, 0xFF),
    }

    match l.right {
        Weight::None => {}
        Weight::Heavy => c.rect(right_left, h_heavy_top, w, h_heavy_bottom, 0xFF),
        Weight::Double => {
            let top_left = if l.up == Weight::Double {
                v_light_right
            } else {
                right_left
            };
            let bottom_left = if l.down == Weight::Double {
                v_light_right
            } else {
                right_left
            };
            c.rect(top_left, h_double_top, w, h_light_top, 0xFF);
            c.rect(bottom_left, h_light_bottom, w, h_double_bottom, 0xFF);
        }
        _ => c.rect(right_left, h_light_top, w, h_light_bottom, 0xFF),
    }

    match l.down {
        Weight::None => {}
        Weight::Heavy => c.rect(v_heavy_left, down_top, v_heavy_right, h, 0xFF),
        Weight::Double => {
            let left_top = if l.left == Weight::Double {
                h_light_bottom
            } else {
                down_top
            };
            let right_top = if l.right == Weight::Double {
                h_light_bottom
            } else {
                down_top
            };
            c.rect(v_double_left, left_top, v_light_left, h, 0xFF);
            c.rect(v_light_right, right_top, v_double_right, h, 0xFF);
        }
        _ => c.rect(v_light_left, down_top, v_light_right, h, 0xFF),
    }

    match l.left {
        Weight::None => {}
        Weight::Heavy => c.rect(0, h_heavy_top, left_right, h_heavy_bottom, 0xFF),
        Weight::Double => {
            let top_right = if l.up == Weight::Double {
                v_light_left
            } else {
                left_right
            };
            let bottom_right = if l.down == Weight::Double {
                v_light_left
            } else {
                left_right
            };
            c.rect(0, h_double_top, top_right, h_light_top, 0xFF);
            c.rect(0, h_light_bottom, bottom_right, h_double_bottom, 0xFF);
        }
        _ => c.rect(0, h_light_top, left_right, h_light_bottom, 0xFF),
    }
}

/// The dash/gap layout for `count` dashes across `size` pixels, as
/// `(offset, length)` pairs. A port of `dashHorizontal`'s arithmetic, shared by
/// both axes because it is identical for each.
///
/// Two non-obvious rules, both of which matter when the dashes are tiled:
///
/// - **Half a gap at each end.** For N dashes there are N-1 gaps *between*
///   them; upstream budgets N and splits one across the two edges, so a row of
///   `┄` has an even rhythm across cell boundaries instead of doubling up at
///   every seam.
/// - **The gap is capped at `size / (2 * count)`** — never more than half the
///   space — and the leftover pixels go into the *dashes*, not the gaps, since
///   an uneven gap is far more visible than an uneven dash.
///
/// Returns `None` when there isn't room for one pixel of each dash and each
/// gap; the caller then draws a solid line, which is much closer to the
/// character's meaning than drawing nothing.
fn dash_runs(size: i32, count: i32, desired_gap: i32) -> Option<Vec<(i32, i32)>> {
    if size < count * 2 {
        return None;
    }
    let gap = desired_gap.min(size / (2 * count));
    let total_dash = size - gap * count;
    let dash = total_dash / count;
    let mut extra = total_dash % count;
    // Start half a gap in, so the dashes sit centred in the cell.
    let mut x = gap / 2;
    let mut out = Vec::with_capacity(count as usize);
    for _ in 0..count {
        let mut len = dash;
        if extra > 0 {
            extra -= 1;
            len += 1;
        }
        out.push((x, len));
        x += len + gap;
    }
    Some(out)
}

/// The desired gap for a dashed line: 4px, or the light thickness if that's
/// bigger (upstream's `@max(4, Thickness.light...)`).
fn desired_gap(m: Metrics) -> i32 {
    4.max(m.line(Weight::Light) as i32)
}

/// `count` dashes across the cell's width, on the horizontal centre line.
fn dash_h(c: &mut Canvas, m: Metrics, count: i32, thick: u32) {
    let y = (m.h as i32 - thick as i32).max(0) / 2;
    match dash_runs(m.w as i32, count, desired_gap(m)) {
        Some(runs) => {
            for (x, len) in runs {
                c.rect(x, y, x + len, y + thick as i32, 0xFF);
            }
        }
        None => hline_middle(c, m, m.line(Weight::Light)),
    }
}

/// `count` dashes down the cell, on the vertical centre line.
fn dash_v(c: &mut Canvas, m: Metrics, count: i32, thick: u32) {
    let x = (m.w as i32 - thick as i32).max(0) / 2;
    match dash_runs(m.h as i32, count, desired_gap(m)) {
        Some(runs) => {
            for (y, len) in runs {
                c.rect(x, y, x + thick as i32, y + len, 0xFF);
            }
        }
        None => vline_middle(c, m, m.line(Weight::Light)),
    }
}

fn hline_middle(c: &mut Canvas, m: Metrics, thick: u32) {
    let y = (m.h as i32 - thick as i32).max(0) / 2;
    c.rect(0, y, m.w as i32, y + thick as i32, 0xFF);
}

fn vline_middle(c: &mut Canvas, m: Metrics, thick: u32) {
    let x = (m.w as i32 - thick as i32).max(0) / 2;
    c.rect(x, 0, x + thick as i32, m.h as i32, 0xFF);
}

/// A fraction of the cell used as a **min** (left/top) coordinate.
///
/// Ghostty's `Fraction.min`/`max` are deliberately asymmetric: the min rounds
/// from the *far* edge so that, at an odd cell size, `start..half` and
/// `half..end` come out the same width instead of differing by a pixel. Getting
/// this "simplified" to a plain round is how quadrant characters end up with a
/// one-pixel seam between them.
fn frac_min(f: f64, size: u32) -> i32 {
    let s = size as f64;
    (s - ((1.0 - f) * s).round()) as i32
}

/// A fraction of the cell used as a **max** (right/bottom) coordinate.
fn frac_max(f: f64, size: u32) -> i32 {
    ((f * size as f64).round()) as i32
}

fn draw_block(c: &mut Canvas, m: Metrics, cp: u32) {
    const EIGHTH: f64 = 0.125;
    const QUARTER: f64 = 0.25;
    const THREE_EIGHTHS: f64 = 0.375;
    const HALF: f64 = 0.5;
    const FIVE_EIGHTHS: f64 = 0.625;
    const THREE_QUARTERS: f64 = 0.75;
    const SEVEN_EIGHTHS: f64 = 0.875;

    // Ghostty's four shade levels. `░▒▓` are a full-cell fill at partial
    // coverage rather than a dither pattern, which is what keeps them even at
    // any cell size.
    let full = |c: &mut Canvas, shade: u8| c.rect(0, 0, m.w as i32, m.h as i32, shade);
    // A block anchored to one edge, sized as a fraction of the cell.
    let lower = |c: &mut Canvas, f: f64| {
        let hpx = frac_max(f, m.h);
        c.rect(0, m.h as i32 - hpx, m.w as i32, m.h as i32, 0xFF);
    };
    let upper = |c: &mut Canvas, f: f64| {
        c.rect(0, 0, m.w as i32, frac_max(f, m.h), 0xFF);
    };
    let left = |c: &mut Canvas, f: f64| {
        c.rect(0, 0, frac_max(f, m.w), m.h as i32, 0xFF);
    };
    let right = |c: &mut Canvas, f: f64| {
        let wpx = frac_max(f, m.w);
        c.rect(m.w as i32 - wpx, 0, m.w as i32, m.h as i32, 0xFF);
    };
    // A quadrant, using the min/max fraction rule so the four tile exactly.
    let quad = |c: &mut Canvas, tl: bool, tr: bool, bl: bool, br: bool| {
        let (xm, ym) = (frac_min(0.5, m.w), frac_min(0.5, m.h));
        let (xh, yh) = (frac_max(0.5, m.w), frac_max(0.5, m.h));
        let (w, h) = (m.w as i32, m.h as i32);
        if tl {
            c.rect(0, 0, xh, yh, 0xFF);
        }
        if tr {
            c.rect(xm, 0, w, yh, 0xFF);
        }
        if bl {
            c.rect(0, ym, xh, h, 0xFF);
        }
        if br {
            c.rect(xm, ym, w, h, 0xFF);
        }
    };

    match cp {
        0x2580 => upper(c, HALF),
        0x2581 => lower(c, EIGHTH),
        0x2582 => lower(c, QUARTER),
        0x2583 => lower(c, THREE_EIGHTHS),
        0x2584 => lower(c, HALF),
        0x2585 => lower(c, FIVE_EIGHTHS),
        0x2586 => lower(c, THREE_QUARTERS),
        0x2587 => lower(c, SEVEN_EIGHTHS),
        0x2588 => full(c, 0xFF),
        0x2589 => left(c, SEVEN_EIGHTHS),
        0x258A => left(c, THREE_QUARTERS),
        0x258B => left(c, FIVE_EIGHTHS),
        0x258C => left(c, HALF),
        0x258D => left(c, THREE_EIGHTHS),
        0x258E => left(c, QUARTER),
        0x258F => left(c, EIGHTH),
        0x2590 => right(c, HALF),
        0x2591 => full(c, 0x40),
        0x2592 => full(c, 0x80),
        0x2593 => full(c, 0xC0),
        0x2594 => upper(c, EIGHTH),
        0x2595 => right(c, EIGHTH),
        0x2596 => quad(c, false, false, true, false),
        0x2597 => quad(c, false, false, false, true),
        0x2598 => quad(c, true, false, false, false),
        0x2599 => quad(c, true, false, true, true),
        0x259A => quad(c, true, false, false, true),
        0x259B => quad(c, true, true, true, false),
        0x259C => quad(c, true, true, false, true),
        0x259D => quad(c, false, true, false, false),
        0x259E => quad(c, false, true, true, false),
        0x259F => quad(c, false, true, true, true),
        _ => {}
    }
}

/// Braille: a 2×4 grid of square dots, sized and spaced by the same
/// give-and-take as `braille.zig` — dot size first, then margins, then spacing,
/// so a small cell still shows eight distinguishable dots rather than a blob.
///
/// The low byte of the codepoint **is** the dot pattern, but not in reading
/// order: bits 0–2 are the left column's top three dots, 3–5 the right column's,
/// and bits 6–7 the two bottom dots (added when braille was extended to 8 dots).
fn draw_braille(c: &mut Canvas, m: Metrics, cp: u32) {
    let (width, height) = (m.w as i32, m.h as i32);
    let mut w = (width / 4).min(height / 8);
    let mut x_spacing = width / 4;
    let mut y_spacing = height / 8;
    let mut x_margin = x_spacing / 2;
    let mut y_margin = y_spacing / 2;

    let mut x_left = width - 2 * x_margin - x_spacing - 2 * w;
    let mut y_left = height - 2 * y_margin - 3 * y_spacing - 4 * w;

    // First, try hard to ensure the dot width is non-zero.
    if x_left >= 2 && y_left >= 4 && w == 0 {
        w += 1;
        x_left -= 2;
        y_left -= 4;
    }
    // Second, prefer a non-zero margin.
    if x_left >= 2 && x_margin == 0 {
        x_margin = 1;
        x_left -= 2;
    }
    if y_left >= 2 && y_margin == 0 {
        y_margin = 1;
        y_left -= 2;
    }
    // Third, increase spacing.
    if x_left >= 1 {
        x_spacing += 1;
        x_left -= 1;
    }
    if y_left >= 3 {
        y_spacing += 1;
        y_left -= 3;
    }
    // Fourth, margins ("spacing", but on the sides).
    if x_left >= 2 {
        x_margin += 1;
        x_left -= 2;
    }
    if y_left >= 2 {
        y_margin += 1;
        y_left -= 2;
    }
    // Last, increase the dot width.
    if x_left >= 2 && y_left >= 4 {
        w += 1;
    }
    if w <= 0 {
        return;
    }

    let x = [x_margin, x_margin + w + x_spacing];
    let y = [
        y_margin,
        y_margin + w + y_spacing,
        y_margin + 2 * (w + y_spacing),
        y_margin + 3 * (w + y_spacing),
    ];
    let bits = (cp & 0xFF) as u8;
    // (bit, column, row) — the codepoint's bit order, spelled out.
    const DOTS: [(u8, usize, usize); 8] = [
        (0, 0, 0),
        (1, 0, 1),
        (2, 0, 2),
        (3, 1, 0),
        (4, 1, 1),
        (5, 1, 2),
        (6, 0, 3),
        (7, 1, 3),
    ];
    for (bit, col, row) in DOTS {
        if bits & (1 << bit) != 0 {
            c.rect(x[col], y[row], x[col] + w, y[row] + w, 0xFF);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const M: Metrics = Metrics {
        w: 10,
        h: 20,
        thickness: 2,
    };

    fn buf(ch: char, m: Metrics) -> Vec<u8> {
        draw(ch, m).unwrap_or_else(|| panic!("{ch:?} should be a sprite"))
    }

    fn at(b: &[u8], m: Metrics, x: u32, y: u32) -> u8 {
        b[(y * m.w + x) as usize]
    }

    /// The columns covered on row `y`.
    fn row_span(b: &[u8], m: Metrics, y: u32) -> Vec<u32> {
        (0..m.w).filter(|&x| at(b, m, x, y) > 0).collect()
    }

    /// The rows covered in column `x`.
    fn col_span(b: &[u8], m: Metrics, x: u32) -> Vec<u32> {
        (0..m.h).filter(|&y| at(b, m, x, y) > 0).collect()
    }

    #[test]
    fn a_horizontal_line_reaches_both_cell_edges() {
        // This is the whole reason sprites exist: the font's version stops short
        // and a row of them shows gaps at every cell boundary.
        let b = buf('─', M);
        let y = (M.h - M.thickness) / 2;
        assert_eq!(row_span(&b, M, y), (0..M.w).collect::<Vec<_>>());
        // …and it is exactly `thickness` tall, centred.
        assert_eq!(col_span(&b, M, 0), vec![y, y + 1]);
    }

    #[test]
    fn a_vertical_line_reaches_top_and_bottom() {
        let b = buf('│', M);
        let x = (M.w - M.thickness) / 2;
        assert_eq!(col_span(&b, M, x), (0..M.h).collect::<Vec<_>>());
        assert_eq!(row_span(&b, M, 0), vec![x, x + 1]);
    }

    #[test]
    fn heavy_is_exactly_twice_light() {
        let light = col_span(&buf('─', M), M, 0).len();
        let heavy = col_span(&buf('━', M), M, 0).len();
        assert_eq!(heavy, light * 2);
    }

    #[test]
    fn a_corner_covers_only_its_two_arms() {
        // '┌' — down and right. Nothing above the centre line, nothing left of it.
        let b = buf('┌', M);
        assert_eq!(at(&b, M, 0, 0), 0, "no coverage in the top-left");
        assert_eq!(at(&b, M, M.w - 1, M.h - 1), 0, "none in the bottom-right");
        // The right arm reaches the right edge, the down arm the bottom.
        let y = (M.h - M.thickness) / 2;
        assert!(at(&b, M, M.w - 1, y) > 0, "right arm reaches the edge");
        let x = (M.w - M.thickness) / 2;
        assert!(at(&b, M, x, M.h - 1) > 0, "down arm reaches the bottom");
    }

    #[test]
    fn opposite_corners_tile_into_a_continuous_line() {
        // '┐' beside '┌' (as `┌─┐` does at a box's top): the down-stroke of each
        // must be on the same rows, or a box's sides step by a pixel.
        let x = (M.w - M.thickness) / 2;
        assert_eq!(col_span(&buf('┌', M), M, x), col_span(&buf('┐', M), M, x));
    }

    #[test]
    fn a_cross_is_the_union_of_its_four_arms() {
        let b = buf('┼', M);
        let y = (M.h - M.thickness) / 2;
        let x = (M.w - M.thickness) / 2;
        assert_eq!(row_span(&b, M, y), (0..M.w).collect::<Vec<_>>());
        assert_eq!(col_span(&b, M, x), (0..M.h).collect::<Vec<_>>());
    }

    #[test]
    fn a_double_line_is_two_strokes_with_a_gap() {
        // '═' — two horizontal strokes, so the centre row is empty.
        let b = buf('═', M);
        let mid = M.h / 2;
        let rows = col_span(&b, M, 0);
        assert!(!rows.contains(&mid), "the centre of a double line is hollow");
        assert_eq!(rows.len(), (M.thickness * 2) as usize);
    }

    #[test]
    fn a_full_block_covers_every_pixel() {
        let b = buf('█', M);
        assert!(b.iter().all(|&v| v == 0xFF));
    }

    #[test]
    fn shades_are_partial_coverage_over_the_whole_cell() {
        for (ch, v) in [('░', 0x40), ('▒', 0x80), ('▓', 0xC0)] {
            let b = buf(ch, M);
            assert!(b.iter().all(|&x| x == v), "{ch:?}");
        }
    }

    #[test]
    fn half_blocks_split_the_cell_exactly() {
        let upper = buf('▀', M);
        let lower = buf('▄', M);
        for y in 0..M.h {
            for x in 0..M.w {
                let (u, l) = (at(&upper, M, x, y), at(&lower, M, x, y));
                // Every pixel belongs to exactly one of the two: no overlap (which
                // would double-darken under transparency) and no gap.
                assert_ne!(u > 0, l > 0, "at {x},{y}");
            }
        }
    }

    #[test]
    fn the_four_quadrants_tile_the_cell_exactly() {
        let quads = ['▘', '▝', '▖', '▗'].map(|ch| buf(ch, M));
        for y in 0..M.h {
            for x in 0..M.w {
                let n = quads.iter().filter(|b| at(b, M, x, y) > 0).count();
                assert_eq!(n, 1, "pixel {x},{y} is covered by {n} quadrants");
            }
        }
    }

    #[test]
    fn an_odd_cell_size_leaves_no_gap_and_keeps_the_halves_equal() {
        // The point of Ghostty's asymmetric min/max fraction rule. At an odd
        // size the two halves cannot both be exact, so upstream makes them the
        // same size and lets them **overlap** by one pixel rather than leaving a
        // one-pixel gap — a gap would show as a seam between quadrants, while
        // the overlap is invisible (both sides are opaque).
        let m = Metrics::new(7, 15);
        let quads = ['▘', '▝', '▖', '▗'].map(|ch| buf(ch, m));
        for y in 0..m.h {
            for x in 0..m.w {
                let n = quads.iter().filter(|b| at(b, m, x, y) > 0).count();
                assert!(n >= 1, "pixel {x},{y} is covered by nothing");
            }
        }
        // Left and right halves are the same width (4 of 7), and likewise the
        // top and bottom (8 of 15).
        let tl = buf('▘', m);
        let tr = buf('▝', m);
        assert_eq!(row_span(&tl, m, 0).len(), row_span(&tr, m, 0).len());
        assert_eq!(row_span(&tl, m, 0).len(), 4);
        assert_eq!(col_span(&tl, m, 0).len(), 8);
    }

    #[test]
    fn eighth_blocks_grow_monotonically() {
        let mut last = 0;
        for ch in ['▁', '▂', '▃', '▄', '▅', '▆', '▇', '█'] {
            let n = buf(ch, M).iter().filter(|&&v| v > 0).count();
            assert!(n > last, "{ch:?} must cover more than the one before");
            last = n;
        }
    }

    #[test]
    fn braille_dots_map_to_the_codepoints_bits() {
        // U+2801 is dot 1 only: the top-left dot.
        let b = buf('\u{2801}', M);
        assert!(b.iter().any(|&v| v > 0), "one dot is drawn");
        let lit: Vec<(u32, u32)> = (0..M.h)
            .flat_map(|y| (0..M.w).map(move |x| (x, y)))
            .filter(|&(x, y)| at(&b, M, x, y) > 0)
            .collect();
        // …and it is in the upper-left quadrant.
        assert!(lit.iter().all(|&(x, y)| x < M.w / 2 && y < M.h / 2));

        // U+2880 is dot 8 only: the bottom-right dot.
        let b = buf('\u{2880}', M);
        let lit: Vec<(u32, u32)> = (0..M.h)
            .flat_map(|y| (0..M.w).map(move |x| (x, y)))
            .filter(|&(x, y)| at(&b, M, x, y) > 0)
            .collect();
        assert!(!lit.is_empty());
        assert!(lit.iter().all(|&(x, y)| x >= M.w / 2 && y >= M.h / 2));
    }

    #[test]
    fn a_blank_braille_pattern_draws_nothing() {
        assert!(buf('\u{2800}', M).iter().all(|&v| v == 0));
    }

    #[test]
    fn a_full_braille_pattern_lights_all_eight_dots() {
        let b = buf('\u{28FF}', M);
        let dots = b.iter().filter(|&&v| v > 0).count();
        // Eight square dots of side `w`; the exact `w` depends on the cell, so
        // assert the count is a multiple of eight equal areas.
        assert!(dots > 0 && dots % 8 == 0, "{dots} lit pixels");
    }

    #[test]
    fn dashes_leave_gaps_but_a_tiny_cell_falls_back_to_a_solid_line() {
        let b = buf('┄', M);
        let y = (M.h - M.thickness) / 2;
        let span = row_span(&b, M, y);
        assert!(span.len() < M.w as usize, "a dashed line has gaps");
        assert!(!span.is_empty());

        // Too narrow for a pixel of each dash and each gap: a solid line beats
        // drawing nothing, which would look like the character is missing.
        let tiny = Metrics::new(5, 12);
        let b = buf('┄', tiny);
        let y = (tiny.h - tiny.thickness) / 2;
        assert_eq!(row_span(&b, tiny, y).len(), tiny.w as usize);
    }

    #[test]
    fn dash_runs_are_capped_gapped_and_evenly_distributed() {
        // The gap is never more than half the space, however wide it's asked to
        // be — otherwise the dashes shrink to specks.
        let runs = dash_runs(12, 3, 100).expect("fits");
        assert_eq!(runs.len(), 3);
        let gap = 12 / (2 * 3);
        // Half a gap of lead-in, so a row of these tiles evenly across cells.
        assert_eq!(runs[0].0, gap / 2);
        // Every dash is within one pixel of every other (the remainder goes into
        // the dashes, never the gaps).
        let (min, max) = (
            runs.iter().map(|r| r.1).min().unwrap(),
            runs.iter().map(|r| r.1).max().unwrap(),
        );
        assert!(max - min <= 1, "dash lengths {min}..{max}");
        // Nothing runs off the end.
        assert!(runs.last().unwrap().0 + runs.last().unwrap().1 <= 12);
        // And below the minimum there is no layout at all.
        assert_eq!(dash_runs(5, 3, 4), None);
    }

    #[test]
    fn unported_ranges_stay_on_the_font() {
        // The stylized powerline symbols (flames, hexagons, ice) aren't drawn
        // upstream either, so they must not be claimed here — claiming one
        // replaces a perfectly good Nerd Font glyph with nothing.
        for ch in ['\u{E0C0}', '\u{E0C4}', '\u{E0D0}', '\u{E0D1}', '\u{E0D3}'] {
            assert!(!covers(ch), "{ch:?}");
            assert!(draw(ch, M).is_none());
        }
        // …and things outside the sprite ranges entirely.
        for ch in ['a', ' ', '\u{1F600}', '\u{2190}'] {
            assert!(!covers(ch), "{ch:?}");
        }
    }

    #[test]
    fn every_ported_codepoint_draws_something() {
        // A gap in the table would be a character that silently vanishes: it is
        // claimed by `covers` (so the font is skipped) but draws nothing.
        for cp in (0x2500..=0x259F).chain(0xE0B0..=0xE0BF).chain([0xE0D2, 0xE0D4]) {
            let ch = char::from_u32(cp).unwrap();
            if !covers(ch) {
                continue;
            }
            let b = buf(ch, M);
            assert!(b.iter().any(|&v| v > 0), "U+{cp:04X} drew nothing");
        }
    }

    #[test]
    fn an_arcs_arms_land_where_the_straight_lines_do() {
        // The reason the arc centre is `(dim - thick)/2 + thick/2` rather than
        // `dim/2`: a rounded box has to line up with a square one, and with the
        // `│`/`─` that run between the corners.
        let vline = col_span(&buf('│', M), M, (M.w - M.thickness) / 2);
        let hline = row_span(&buf('─', M), M, (M.h - M.thickness) / 2);
        // Anti-aliased edges are legal off the arm's axis, so compare the solid
        // core rather than "any coverage".
        let solid_col = |b: &[u8], x: u32| -> Vec<u32> {
            (0..M.h).filter(|&y| at(b, M, x, y) >= 128).collect()
        };
        let solid_row = |b: &[u8], y: u32| -> Vec<u32> {
            (0..M.w).filter(|&x| at(b, M, x, y) >= 128).collect()
        };
        let x = (M.w - M.thickness) / 2;
        let y = (M.h - M.thickness) / 2;

        // '╭' has arms going down and right: its bottom edge sits on the same
        // column as '│', its right edge on the same row as '─'.
        let b = buf('╭', M);
        assert!(solid_col(&b, x).contains(&(M.h - 1)), "reaches the bottom edge");
        assert!(solid_row(&b, y).contains(&(M.w - 1)), "reaches the right edge");
        assert!(vline.contains(&(M.h - 1)) && hline.contains(&(M.w - 1)));

        // '╯' goes up and left.
        let b = buf('╯', M);
        assert!(solid_col(&b, x).contains(&0), "reaches the top edge");
        assert!(solid_row(&b, y).contains(&0), "reaches the left edge");

        // '╮' up-less: down and left. '╰': up and right.
        assert!(solid_col(&buf('╮', M), x).contains(&(M.h - 1)));
        assert!(solid_row(&buf('╮', M), y).contains(&0));
        assert!(solid_col(&buf('╰', M), x).contains(&0));
        assert!(solid_row(&buf('╰', M), y).contains(&(M.w - 1)));
    }

    #[test]
    fn a_diagonal_runs_corner_to_corner_and_is_antialiased() {
        let b = buf('╲', M);
        // Both corners are covered — the overshoot is what makes a run of them
        // continue across cells instead of meeting at a point.
        assert!(at(&b, M, 0, 0) > 0 && at(&b, M, M.w - 1, M.h - 1) > 0);
        assert_eq!(at(&b, M, M.w - 1, 0), 0, "the other corners stay empty");
        assert_eq!(at(&b, M, 0, M.h - 1), 0);
        // A partial pixel somewhere proves the anti-aliased path actually ran
        // (a rectangle-only implementation would be all 0 or 255).
        assert!(
            b.iter().any(|&v| v > 0 && v < 255),
            "a diagonal must have soft edges"
        );
        // '╳' is the union of the two diagonals.
        let cross = buf('╳', M);
        let other = buf('╱', M);
        for i in 0..cross.len() {
            assert_eq!(cross[i], b[i].max(other[i]), "at index {i}");
        }
    }

    #[test]
    fn powerline_separators_mirror_their_twins() {
        // Each right-facing separator is its left-facing twin flipped, so one
        // geometry serves both and they cannot drift apart.
        for (left, right) in [
            ('\u{E0B0}', '\u{E0B2}'),
            ('\u{E0B4}', '\u{E0B6}'),
            ('\u{E0B5}', '\u{E0B7}'),
            ('\u{E0B1}', '\u{E0B3}'),
        ] {
            let a = buf(left, M);
            let b = buf(right, M);
            let mirrored: Vec<u8> = a
                .chunks(M.w as usize)
                .flat_map(|row| row.iter().rev().copied())
                .collect();
            assert_eq!(mirrored, b, "U+{:04X} vs U+{:04X}", left as u32, right as u32);
        }
    }

    #[test]
    fn a_solid_powerline_triangle_covers_half_the_cell() {
        // Area rather than exact pixels: robust under supersampling, and it
        // catches a mirrored or degenerate polygon immediately.
        for ch in ['\u{E0B0}', '\u{E0B2}', '\u{E0B8}', '\u{E0BA}', '\u{E0BC}', '\u{E0BE}'] {
            let b = buf(ch, M);
            let area: f64 = b.iter().map(|&v| v as f64 / 255.0).sum();
            let half = (M.w * M.h) as f64 / 2.0;
            assert!(
                (area - half).abs() < half * 0.15,
                "{ch:?} covers {area:.1}, expected ~{half:.1}"
            );
            assert!(
                b.iter().any(|&v| v > 0 && v < 255),
                "{ch:?} must have an anti-aliased hypotenuse"
            );
        }
    }

    #[test]
    fn a_thin_chevron_is_a_stroke_not_a_fill() {
        let solid: f64 = buf('\u{E0B0}', M).iter().map(|&v| v as f64).sum();
        let thin: f64 = buf('\u{E0B1}', M).iter().map(|&v| v as f64).sum();
        // An outline of a shape, not the shape: much less ink, but not nothing.
        assert!(thin < solid * 0.75, "the thin variant is an outline");
        assert!(thin > 0.0);
    }

    #[test]
    fn a_stroked_half_circle_stays_inside_the_cell() {
        // The curve bulges to exactly the cell edge, so a centred stroke would
        // hang outside and clip flat. `stroke_inside` is what prevents that; the
        // symptom would be a *thinner* line down the right side than the left.
        let b = buf('\u{E0B5}', M);
        assert!(b.iter().any(|&v| v > 0));
        // The stroke is inside the filled version everywhere.
        let filled = buf('\u{E0B4}', M);
        for i in 0..b.len() {
            assert!(
                b[i] as u16 <= filled[i] as u16 + 1,
                "stroke escapes the fill at {i}"
            );
        }
    }

    #[test]
    fn a_degenerate_cell_never_panics() {
        for (w, h) in [(1, 1), (1, 20), (20, 1), (2, 3), (3, 2)] {
            let m = Metrics::new(w, h);
            for cp in [
                0x2500u32, 0x254B, 0x256C, 0x2588, 0x2596, 0x28FF, 0x2504, 0x256D, 0x2573, 0xE0B0,
                0xE0B1, 0xE0B4, 0xE0B5, 0xE0D2, 0xE0D4,
            ] {
                let ch = char::from_u32(cp).unwrap();
                assert_eq!(draw(ch, m).map(|b| b.len()), Some((w * h) as usize));
            }
        }
    }

    #[test]
    fn thickness_never_rounds_to_zero() {
        // An invisible line reads as a missing glyph, not a thin one.
        for h in 1..40 {
            assert!(Metrics::new(10, h).thickness >= 1);
        }
    }
}
