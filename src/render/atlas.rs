//! Dynamic glyph atlas: rasterize glyphs on demand with ab_glyph and pack them
//! into a single R8 wgpu texture, caching UV rects per (glyph id, style).
//!
//! Four JetBrains Mono Nerd Font variants are embedded so bold/italic render
//! with real glyphs (not faux styling), and Nerd Font icon/powerline glyphs are
//! available. Text is shaped with rustybuzz (see [`Atlas::shape_run`]) so the
//! font's ligatures / contextual alternates apply; the renderer then snaps the
//! shaped glyphs onto the terminal grid by cluster.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::str::FromStr;

use ab_glyph::{Font, FontRef, FontVec, GlyphId, PxScale, ScaleFont, VariableFont, point};
use eframe::wgpu;
use rustybuzz::ttf_parser;
use rustybuzz::{Direction, Face as ShapeFace, Feature, UnicodeBuffer};

const ATLAS_SIZE: u32 = 2048;

/// Max number of distinct shaped runs cached across all styles before the shape
/// cache is dropped wholesale (a coarse bound, matching the atlas's own reset).
const SHAPE_CACHE_CAP: usize = 8192;

/// Ordered system fallback fonts (path, collection index). Tried in order for
/// characters the primary font lacks (CJK, symbols). Color emoji are handled
/// separately via the COLR/CPAL path (see [`COLOR_FONT`]).
const FALLBACK_FONTS: &[(&str, u32)] = &[
    (r"C:\Windows\Fonts\seguisym.ttf", 0), // Segoe UI Symbol
    (r"C:\Windows\Fonts\seguiemj.ttf", 0), // Segoe UI Emoji (outline layer)
    (r"C:\Windows\Fonts\msyh.ttc", 0),     // Microsoft YaHei (CJK)
    (r"C:\Windows\Fonts\malgun.ttf", 0),   // Malgun Gothic (Korean)
    (r"C:\Windows\Fonts\YuGothM.ttc", 0),  // Yu Gothic (Japanese)
    (r"C:\Windows\Fonts\segoeui.ttf", 0),  // Segoe UI (Latin/general)
];

/// Color (COLR/CPAL) font used for emoji. Its glyphs are layered colored shapes
/// rather than single outlines, composited into the RGBA color atlas.
const COLOR_FONT: &str = r"C:\Windows\Fonts\seguiemj.ttf";

/// A rasterized glyph bitmap awaiting upload into the atlas: coverage values
/// (`w`×`h`, row-major) plus the glyph's pixel min corner relative to the pen.
struct Raster {
    bitmap: Vec<u8>,
    w: u32,
    h: u32,
    min: (f32, f32),
}

/// Which synthetic styles to apply to a face's glyphs, when the configured
/// family has no real face for the requested style (`font-synthetic-style`).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Synth {
    pub bold: bool,
    pub italic: bool,
}

impl Synth {
    fn any(self) -> bool {
        self.bold || self.italic
    }
}

/// Ghostty's synthetic-bold strength, in pixels.
///
/// Upstream emboldens the *outline* by `ceil(font_height * 64/2048)` in 26.6
/// units, which works out to about `height / 32` pixels — a heuristic its
/// comment says was tuned across many fonts. It has to scale with the size: a
/// fixed 1px is invisible at 28px and clubby at 10px.
fn embolden_strength(line_height: f32) -> f32 {
    (line_height * 2.0).ceil() / 64.0
}

/// The italic shear coefficient: `tan(12°)`, upstream's angle.
const ITALIC_SKEW: f32 = 0.212_556_5;

/// Synthetic bold: dilate the coverage by `strength` pixels.
///
/// Upstream emboldens the glyph *outline* before rasterizing; giest has no
/// outline access through ab_glyph, so this dilates the rasterized coverage
/// instead — max-blended (never summed) so anti-aliased edges stay smooth
/// rather than clipping to a hard block. Slightly chunkier than a real outline
/// embolden, and it grows the bitmap rather than the glyph's advance, which is
/// what a monospace cell wants anyway.
fn embolden(src: &Raster, strength: f32) -> Raster {
    let grow = (strength.round() as u32).max(1);
    let (w, h) = (src.w + grow, src.h + grow);
    let mut bitmap = vec![0u8; (w * h) as usize];
    for dy in 0..=grow {
        for dx in 0..=grow {
            for y in 0..src.h {
                for x in 0..src.w {
                    let v = src.bitmap[(y * src.w + x) as usize];
                    if v == 0 {
                        continue;
                    }
                    let d = &mut bitmap[((y + dy) * w + (x + dx)) as usize];
                    *d = (*d).max(v);
                }
            }
        }
    }
    Raster {
        bitmap,
        w,
        h,
        // Growing symmetrically would shift the glyph right and down; upstream's
        // embolden grows about the outline, so keep the original bearing and let
        // the extra weight fall to the right and below, which is what a bold face
        // does relative to its regular.
        min: src.min,
    }
}

/// Synthetic italic: shear the coverage by [`ITALIC_SKEW`] about the baseline.
///
/// `baseline_row` is the row within the bitmap where the baseline lies (i.e.
/// `-min.1`): rows above it move right and any descender below moves left, so
/// the glyph pivots on the baseline instead of leaning off the top of the cell.
/// The buffer grows on both sides and `min.0` moves left by the descender-side
/// growth — without that the glyph's bearing would be wrong and it would drift
/// out of its cell.
///
/// Per-row integer shifts, so the slanted edges staircase slightly at small
/// sizes. Upstream shears the outline before rasterizing and gets a clean edge.
fn shear(src: &Raster, skew: f32, baseline_row: f32) -> Raster {
    let shift_at = |y: u32| -> i32 { ((baseline_row - y as f32) * skew).round() as i32 };
    let (mut lo, mut hi) = (0i32, 0i32);
    for y in 0..src.h {
        let s = shift_at(y);
        lo = lo.min(s);
        hi = hi.max(s);
    }
    let w = (src.w as i32 + hi - lo).max(1) as u32;
    let mut bitmap = vec![0u8; (w * src.h) as usize];
    for y in 0..src.h {
        let s = shift_at(y) - lo;
        for x in 0..src.w {
            let v = src.bitmap[(y * src.w + x) as usize];
            if v == 0 {
                continue;
            }
            let tx = x as i32 + s;
            if tx >= 0 && (tx as u32) < w {
                bitmap[(y * w + tx as u32) as usize] = v;
            }
        }
    }
    Raster {
        bitmap,
        w,
        h: src.h,
        min: (src.min.0 + lo as f32, src.min.1),
    }
}

/// Rasterize `glyph` (already scaled/positioned) from `font` into a [`Raster`],
/// or `None` for outline-less glyphs (spaces, missing). Borrows only the font,
/// so the caller can upload afterward without a borrow conflict.
fn outline_to_bitmap<F: Font>(font: &F, glyph: ab_glyph::Glyph) -> Option<Raster> {
    let outline = font.outline_glyph(glyph)?;
    let bounds = outline.px_bounds();
    let w = bounds.width().ceil() as u32;
    let h = bounds.height().ceil() as u32;
    if w == 0 || h == 0 {
        return None;
    }
    let mut bitmap = vec![0u8; (w * h) as usize];
    outline.draw(|x, y, c| {
        if x < w && y < h {
            bitmap[(y * w + x) as usize] = (c * 255.0) as u8;
        }
    });
    Some(Raster {
        bitmap,
        w,
        h,
        min: (bounds.min.x, bounds.min.y),
    })
}

/// A loaded color font: a ttf-parser face (for COLR/CPAL layer + palette
/// lookup) and an ab_glyph face over the same bytes (to rasterize layer
/// outlines). Both borrow the same leaked `'static` font data.
struct ColorFont {
    face: ttf_parser::Face<'static>,
    raster: FontRef<'static>,
}

impl ColorFont {
    fn load(path: &str) -> Option<Self> {
        let bytes: &'static [u8] = Box::leak(std::fs::read(path).ok()?.into_boxed_slice());
        let face = ttf_parser::Face::parse(bytes, 0).ok()?;
        let raster = FontRef::try_from_slice(bytes).ok()?;
        // Only useful if it actually carries color tables.
        if face.color_palettes().is_none() {
            return None;
        }
        Some(Self { face, raster })
    }
}

/// Collects the solid-color layers of a COLRv0 glyph as ttf-parser drives it.
/// Gradient/transform/clip operations (COLRv1) are ignored — Segoe UI Emoji is
/// COLRv0, so each layer is just an outline glyph painted with a solid color.
#[derive(Default)]
struct LayerCollector {
    layers: Vec<(u16, [u8; 4])>,
    pending: Option<u16>,
}

/// Average a gradient's color stops into one representative solid color (RGBA).
/// Win11's Segoe UI Emoji is COLRv1 with gradient-filled shapes; rather than
/// rasterize true gradients we flatten each to its mean stop color so the whole
/// shape renders (flat-shaded but complete) instead of vanishing. `None` if the
/// gradient has no stops.
fn average_stops<I: Iterator<Item = ttf_parser::colr::ColorStop>>(stops: I) -> Option<[u8; 4]> {
    let (mut r, mut g, mut b, mut a, mut n) = (0u32, 0u32, 0u32, 0u32, 0u32);
    for s in stops {
        r += s.color.red as u32;
        g += s.color.green as u32;
        b += s.color.blue as u32;
        a += s.color.alpha as u32;
        n += 1;
    }
    if n == 0 {
        return None;
    }
    Some([(r / n) as u8, (g / n) as u8, (b / n) as u8, (a / n) as u8])
}

impl<'a> ttf_parser::colr::Painter<'a> for LayerCollector {
    fn outline_glyph(&mut self, glyph_id: ttf_parser::GlyphId) {
        self.pending = Some(glyph_id.0);
    }
    fn paint(&mut self, paint: ttf_parser::colr::Paint<'a>) {
        use ttf_parser::colr::Paint;
        let Some(gid) = self.pending.take() else {
            return;
        };
        // Resolve the layer's fill to a single solid color. Gradients (COLRv1)
        // are approximated by their mean stop color so the whole shape paints.
        let color = match paint {
            Paint::Solid(c) => Some([c.red, c.green, c.blue, c.alpha]),
            Paint::LinearGradient(grad) => average_stops(grad.stops(0, &[])),
            Paint::RadialGradient(grad) => average_stops(grad.stops(0, &[])),
            Paint::SweepGradient(grad) => average_stops(grad.stops(0, &[])),
        };
        if let Some(color) = color {
            self.layers.push((gid, color));
        }
    }
    fn push_clip(&mut self) {}
    fn push_clip_box(&mut self, _: ttf_parser::colr::ClipBox) {}
    fn pop_clip(&mut self) {}
    fn push_layer(&mut self, _: ttf_parser::colr::CompositeMode) {}
    fn pop_layer(&mut self) {}
    fn push_transform(&mut self, _: ttf_parser::Transform) {}
    fn pop_transform(&mut self) {}
}

/// Composite a glyph's COLRv0 `layers` into a straight-alpha RGBA bitmap by
/// rasterizing each layer with `raster` and alpha-compositing in source order.
/// `srgb` converts palette colors to linear so the result matches the sRGB
/// render target. Returns the RGBA buffer, its size, and its pixel min corner.
fn composite_color_layers(
    raster: &FontRef<'static>,
    layers: &[(u16, [u8; 4])],
    px: f32,
    srgb: bool,
) -> Option<(Vec<u8>, u32, u32, (f32, f32))> {
    // Rasterize every layer once and find the union pixel bounds.
    let mut rastered: Vec<(Raster, [u8; 4])> = Vec::with_capacity(layers.len());
    let (mut min_x, mut min_y) = (f32::MAX, f32::MAX);
    let (mut max_x, mut max_y) = (f32::MIN, f32::MIN);
    for &(gid, color) in layers {
        let glyph = GlyphId(gid).with_scale_and_position(px, point(0.0, 0.0));
        let Some(r) = outline_to_bitmap(raster, glyph) else {
            continue;
        };
        min_x = min_x.min(r.min.0);
        min_y = min_y.min(r.min.1);
        max_x = max_x.max(r.min.0 + r.w as f32);
        max_y = max_y.max(r.min.1 + r.h as f32);
        rastered.push((r, color));
    }
    if rastered.is_empty() {
        return None;
    }
    let ox = min_x.floor();
    let oy = min_y.floor();
    let w = (max_x.ceil() - ox).max(1.0) as u32;
    let h = (max_y.ceil() - oy).max(1.0) as u32;
    if w == 0 || h == 0 || (w as u64 * h as u64) > (ATLAS_SIZE as u64 * ATLAS_SIZE as u64) {
        return None;
    }

    // Straight-alpha accumulator (f32) then packed to u8 RGBA.
    let mut acc = vec![[0.0f32; 4]; (w * h) as usize];
    for (r, color) in &rastered {
        let chan = |c: u8| {
            let s = c as f32 / 255.0;
            if srgb { srgb_channel_to_linear(s) } else { s }
        };
        let (lr, lg, lb) = (chan(color[0]), chan(color[1]), chan(color[2]));
        let la = color[3] as f32 / 255.0;
        let dx = (r.min.0.floor() - ox) as i32;
        let dy = (r.min.1.floor() - oy) as i32;
        for y in 0..r.h {
            for x in 0..r.w {
                let cov = r.bitmap[(y * r.w + x) as usize] as f32 / 255.0;
                let sa = cov * la;
                if sa <= 0.0 {
                    continue;
                }
                let tx = dx + x as i32;
                let ty = dy + y as i32;
                if tx < 0 || ty < 0 || tx >= w as i32 || ty >= h as i32 {
                    continue;
                }
                let d = &mut acc[(ty as u32 * w + tx as u32) as usize];
                // Straight-alpha "over": out = src + dst*(1-src_a).
                let inv = 1.0 - sa;
                let out_a = sa + d[3] * inv;
                if out_a > 0.0 {
                    d[0] = (lr * sa + d[0] * d[3] * inv) / out_a;
                    d[1] = (lg * sa + d[1] * d[3] * inv) / out_a;
                    d[2] = (lb * sa + d[2] * d[3] * inv) / out_a;
                    d[3] = out_a;
                }
            }
        }
    }

    let mut rgba = vec![0u8; (w * h * 4) as usize];
    for (i, p) in acc.iter().enumerate() {
        rgba[i * 4] = (p[0].clamp(0.0, 1.0) * 255.0) as u8;
        rgba[i * 4 + 1] = (p[1].clamp(0.0, 1.0) * 255.0) as u8;
        rgba[i * 4 + 2] = (p[2].clamp(0.0, 1.0) * 255.0) as u8;
        rgba[i * 4 + 3] = (p[3].clamp(0.0, 1.0) * 255.0) as u8;
    }
    Some((rgba, w, h, (ox, oy)))
}

/// sRGB→linear for a single 0..1 channel (matches the renderer's conversion).
fn srgb_channel_to_linear(s: f32) -> f32 {
    if s <= 0.04045 {
        s / 12.92
    } else {
        ((s + 0.055) / 1.055).powf(2.4)
    }
}

pub(crate) const FONT_REGULAR: &[u8] =
    include_bytes!("../../assets/fonts/JetBrainsMonoNerdFont-Regular.ttf");
const FONT_BOLD: &[u8] = include_bytes!("../../assets/fonts/JetBrainsMonoNerdFont-Bold.ttf");
const FONT_ITALIC: &[u8] = include_bytes!("../../assets/fonts/JetBrainsMonoNerdFont-Italic.ttf");
const FONT_BOLD_ITALIC: &[u8] =
    include_bytes!("../../assets/fonts/JetBrainsMonoNerdFont-BoldItalic.ttf");

/// User font selection, neutral of `Config` (the app builds it from config so
/// the render layer stays config-agnostic). Empty/`None` everywhere means the
/// built-in JetBrains Mono with the font's default OpenType features.
#[derive(Clone, Debug, Default)]
pub struct FontSpec {
    /// Families in order: the first that resolves is the primary, the rest are
    /// a fallback chain tried before the system fonts. Empty = built-in font.
    pub families: Vec<String>,
    /// Per-style family overrides; each falls back to `family` when `None`.
    pub family_bold: Option<String>,
    pub family_italic: Option<String>,
    pub family_bold_italic: Option<String>,
    /// OpenType feature specs (e.g. `-calt`, `ss01`, `cv01=2`) applied at shaping.
    pub features: Vec<String>,
    /// Variable-font axis settings per style slot (regular, bold, italic,
    /// bold-italic). Applied to the parsed face, so they move the *outlines* and
    /// therefore the cell metrics — unlike `features`, which act at shaping.
    pub variations: [Vec<crate::config::FontVariation>; 4],
    /// The `adjust-*` metric modifiers.
    pub adjust: crate::config::MetricAdjust,
    /// Which missing styles may be synthesized (`font-synthetic-style`).
    pub synthetic: crate::config::SyntheticStyle,
}

/// The cell and decoration metrics the renderer draws from, in physical pixels,
/// after the `adjust-*` modifiers.
///
/// Mirrors Ghostty's `font.Metrics`, including its sign conventions: every
/// *position* is measured from the **top of the cell** (so an overline sits at
/// 0), while `ascent` is the top-to-baseline distance. Positions and thicknesses
/// are adjusted differently on purpose — a thickness is clamped to ≥1 (an
/// invisible line reads as a missing glyph), a position is not (zero and
/// negative are meaningful there).
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct CellMetrics {
    pub cell_w: f32,
    pub cell_h: f32,
    /// Top of the cell to the text baseline.
    pub ascent: f32,
    /// Top of the cell to the top of the underline.
    pub underline_pos: f32,
    pub underline_thick: f32,
    pub strikethrough_pos: f32,
    pub strikethrough_thick: f32,
    pub overline_pos: f32,
    pub overline_thick: f32,
    /// Width of a bar cursor / thickness of an underline or hollow cursor.
    pub cursor_thick: f32,
    /// Height of the cursor, anchored to the bottom of the cell.
    pub cursor_height: f32,
    /// Thickness of the drawn box-drawing lines.
    pub box_thick: f32,
    /// Ceiling a Nerd Font icon is scaled down to fit (`adjust-icon-height`).
    /// The cell height by default; only `Constraint::Fit` glyphs consult it.
    pub icon_height: f32,
}

/// Derive the cell and decoration metrics for a face at `px`, then apply the
/// `adjust-*` modifiers. A free function of the raw font metrics so the whole
/// derivation is unit-testable without a GPU or a font file.
///
/// `raw` is what the font says: `(advance, ascent, descent, line_gap,
/// underline_pos_below_baseline, underline_thick, strikethrough_pos_above_baseline,
/// strikethrough_thick)`, all already scaled to pixels.
#[allow(clippy::too_many_arguments)]
pub fn derive_metrics(raw: RawFontMetrics, adjust: &crate::config::MetricAdjust) -> CellMetrics {
    // `adjust-cell-*` is applied **before** the ceil, not after. A 4% adjustment
    // on a 9.6px advance would otherwise be entirely eaten by the rounding that
    // already happened — the same trap `window-position-*` hit with DPI.
    // …and the cell dimensions `ceil`, they don't round: a 6.36px advance is a
    // 7px cell, and rounding it to 6 costs every column a pixel.
    let cell_w = adjust.cell_width.apply(raw.advance).ceil().max(1.0);
    let raw_h = raw.ascent - raw.descent + raw.line_gap;
    let cell_h = adjust.cell_height.apply(raw_h).ceil().max(1.0);

    // The face is *centered* in an adjusted cell rather than pinned to the top,
    // so `adjust-cell-height` reads as line spacing (Ghostty splits the diff
    // between the top and the bottom the same way). Half the growth goes above
    // the text, which is why the baseline moves with it.
    let half = ((cell_h - raw_h) / 2.0).round();
    // `adjust-font-baseline` is documented as a distance from the *bottom* of the
    // cell, so a positive value lifts the text — hence the subtraction.
    let ascent = (raw.ascent + half - adjust.font_baseline.apply(0.0)).round();

    let underline_thick = adjust.underline_thickness.apply_thickness(raw.underline_thick);
    let strikethrough_thick = adjust
        .strikethrough_thickness
        .apply_thickness(raw.strikethrough_thick);
    CellMetrics {
        cell_w,
        cell_h,
        ascent,
        // Font metrics give the underline as a distance *below* the baseline;
        // positions here are from the top of the cell.
        underline_pos: adjust
            .underline_position
            .apply((ascent + raw.underline_pos).round()),
        underline_thick,
        strikethrough_pos: adjust
            .strikethrough_position
            .apply((ascent - raw.strikethrough_pos).round()),
        strikethrough_thick,
        // Upstream's defaults: the overline sits at the very top of the cell and
        // shares the underline's thickness, as do the box-drawing lines.
        overline_pos: adjust.overline_position.apply(0.0),
        overline_thick: adjust.overline_thickness.apply_thickness(underline_thick),
        cursor_thick: adjust.cursor_thickness.apply_thickness(underline_thick),
        cursor_height: adjust.cursor_height.apply_thickness(cell_h),
        box_thick: adjust.box_thickness.apply_thickness(underline_thick),
        // `apply_thickness`, not `apply`: a zero or negative ceiling would make
        // every icon vanish, which reads as a missing glyph rather than as a
        // mis-set option — the same reason every other *thickness* clamps.
        icon_height: adjust.icon_height.apply_thickness(cell_h),
    }
}

/// Read the raw metrics from a face at `px`.
///
/// The cell box comes from ab_glyph (which is what rasterizes), while the
/// underline and strikeout lines come from the **ttf-parser** face behind
/// rustybuzz — ab_glyph exposes no `post`/`OS/2` line metrics at all. Both
/// describe the same face, so mixing the two is safe; the alternative would be
/// deriving the decorations from the cell box, which is what giest did before
/// and is why underlines sat at a hardcoded 7% of the cell.
fn raw_metrics(font: &FontRef<'static>, face: &ShapeFace<'static>, px: f32) -> RawFontMetrics {
    let scaled = font.as_scaled(PxScale::from(px));
    let upem = face.units_per_em() as f32;
    let scale = if upem > 0.0 { px / upem } else { 0.0 };
    // Fallbacks mirror Ghostty's estimates for a font that declares neither: an
    // underline one thickness below the baseline, and a strikethrough centred on
    // half the ex-height.
    let underline = face.underline_metrics();
    let strikeout = face.strikeout_metrics();
    let underline_thick = underline.map_or(0.0, |m| m.thickness as f32 * scale).max(1.0);
    let x_height = face.x_height().unwrap_or(0) as f32 * scale;
    RawFontMetrics {
        advance: scaled.h_advance(font.glyph_id(' ')),
        ascent: scaled.ascent(),
        descent: scaled.descent(),
        line_gap: scaled.line_gap(),
        // ttf-parser reports the underline position as the (negative) distance
        // from the baseline to the line's *centre*, positive-up; we want the
        // distance down to its top.
        underline_pos: underline
            .map_or(underline_thick, |m| -(m.position as f32 * scale) - underline_thick / 2.0),
        underline_thick,
        strikethrough_pos: strikeout
            .map_or(x_height / 2.0, |m| m.position as f32 * scale + m.thickness as f32 * scale),
        strikethrough_thick: strikeout
            .map_or(underline_thick, |m| m.thickness as f32 * scale)
            .max(1.0),
    }
}

/// The raw, pixel-scaled metrics [`derive_metrics`] works from.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct RawFontMetrics {
    /// Advance width of a space (the monospace cell width).
    pub advance: f32,
    pub ascent: f32,
    /// Negative, as fonts report it.
    pub descent: f32,
    pub line_gap: f32,
    /// Distance *below* the baseline to the top of the underline.
    pub underline_pos: f32,
    pub underline_thick: f32,
    /// Distance *above* the baseline to the top of the strikethrough.
    pub strikethrough_pos: f32,
    pub strikethrough_thick: f32,
}

/// Style index into the font table: bit 0 = bold, bit 1 = italic.
fn style_index(bold: bool, italic: bool) -> usize {
    (bold as usize) | ((italic as usize) << 1)
}

/// Parse `font-feature` specs into rustybuzz features, skipping unparseable ones
/// (with a warning). The standard HarfBuzz syntax is accepted (`kern`, `+liga`,
/// `-calt`, `ss01=1`, `liga off`, …) via [`Feature::from_str`]. A tag that isn't
/// exactly 4 characters is rejected up front (Ghostty's rule): rustybuzz would
/// otherwise space-pad a short tag and apply a bogus OpenType feature.
fn parse_features(specs: &[String]) -> Vec<Feature> {
    let mut out = Vec::new();
    for spec in specs {
        if !has_4char_tag(spec) {
            eprintln!("giest: ignoring font-feature '{spec}' (tag must be 4 characters)");
            continue;
        }
        match Feature::from_str(spec) {
            Ok(f) => out.push(f),
            Err(_) => eprintln!("giest: ignoring invalid font-feature '{spec}'"),
        }
    }
    out
}

/// Whether `spec`'s OpenType tag is exactly 4 characters, mirroring Ghostty
/// (which rejects any other length). The tag is the leading ASCII-alphanumeric
/// run after an optional `+`/`-` sign and an optional opening quote.
fn has_4char_tag(spec: &str) -> bool {
    let s = spec.trim();
    let s = s.strip_prefix(['+', '-']).unwrap_or(s);
    let s = s.strip_prefix(['\'', '"']).unwrap_or(s);
    s.chars().take_while(char::is_ascii_alphanumeric).count() == 4
}

/// Apply `font-variation` axes to a style slot's rasterizer and shaper faces.
///
/// An axis the font doesn't have is **reported once and skipped**, not an error:
/// upstream says "invalid ids and values are usually ignored", and a config
/// shared between machines will name axes some installed fonts lack. But it is
/// reported, because "nothing happened" is otherwise indistinguishable from a
/// typo in the tag — the most likely way to get this wrong.
///
/// Out-of-range *values* are not detected here and cannot be: upstream notes
/// they are ignored rather than clamped ("if a font only supports weights from
/// 100 to 700, setting `wght=800` will do nothing"), and neither backend
/// reports the difference.
fn apply_variations(
    font: &mut FontRef<'static>,
    shaper: &mut ShapeFace<'static>,
    variations: &[crate::config::FontVariation],
) {
    for v in variations {
        // The shaper's setter reports nothing, so the rasterizer's return is the
        // only signal either way — and they parse the same file, so it speaks
        // for both.
        let known = font.set_variation(&v.tag, v.value);
        shaper.set_variations(&[rustybuzz::Variation {
            tag: rustybuzz::ttf_parser::Tag::from_bytes(&v.tag),
            value: v.value,
        }]);
        if !known {
            eprintln!(
                "giest: font-variation '{}' is not an axis of this font; ignoring it",
                v.tag_str()
            );
        }
    }
}

/// Leak font bytes to `'static`. The atlas (and its `FontRef`/`ShapeFace`, which
/// borrow `'static`) lives until the process exits or a restart-triggering
/// config change; user fonts are loaded once at atlas construction, mirroring the
/// embedded `'static` consts and the leaked color/fallback faces.
fn leak_font(bytes: Vec<u8>) -> &'static [u8] {
    Box::leak(bytes.into_boxed_slice())
}

/// Directories scanned for a configured `font-family`: the user's per-account
/// font store first (installed-for-me fonts), then the system store.
fn font_dirs() -> Vec<PathBuf> {
    let mut dirs = Vec::new();
    if let Some(local) = std::env::var_os("LOCALAPPDATA") {
        dirs.push(PathBuf::from(local).join(r"Microsoft\Windows\Fonts"));
    }
    let windir = std::env::var_os("WINDIR").unwrap_or_else(|| r"C:\Windows".into());
    dirs.push(PathBuf::from(windir).join("Fonts"));
    dirs
}

/// Whether `face` is the requested `family` (matched against the family and
/// typographic-family name records, case-insensitively) in the requested style.
fn face_matches(face: &ttf_parser::Face, family: &str, bold: bool, italic: bool) -> bool {
    family_name_matches(face, family) && face.is_bold() == bold && face.is_italic() == italic
}

/// Whether `face`'s family matches `family` by its family / typographic-family
/// name records, case-insensitively (the name half of [`face_matches`]).
fn family_name_matches(face: &ttf_parser::Face, family: &str) -> bool {
    let want = family.trim();
    face.names().into_iter().any(|n| {
        matches!(
            n.name_id,
            ttf_parser::name_id::FAMILY | ttf_parser::name_id::TYPOGRAPHIC_FAMILY
        ) && n.to_string().is_some_and(|s| s.trim().eq_ignore_ascii_case(want))
    })
}

/// Scan the font sources for the first face that `accept`s. A `family` that is an
/// existing file path is loaded directly (face index 0, unconditionally — the
/// user named the exact file); otherwise the font directories are scanned and
/// every face tested. The returned bytes are leaked to `'static`.
fn scan_fonts(
    family: &str,
    accept: impl Fn(&ttf_parser::Face) -> bool,
) -> Option<(&'static [u8], u32)> {
    let family = family.trim();
    if family.is_empty() {
        return None;
    }
    let path = Path::new(family);
    if path.is_file() {
        let bytes = std::fs::read(path).ok()?;
        return Some((leak_font(bytes), 0));
    }
    for dir in font_dirs() {
        let Ok(entries) = std::fs::read_dir(&dir) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            let ext = path
                .extension()
                .and_then(|e| e.to_str())
                .map(str::to_ascii_lowercase);
            if !matches!(ext.as_deref(), Some("ttf" | "otf" | "ttc" | "otc")) {
                continue;
            }
            let Ok(bytes) = std::fs::read(&path) else {
                continue;
            };
            let faces = ttf_parser::fonts_in_collection(&bytes).unwrap_or(1);
            for idx in 0..faces {
                if ttf_parser::Face::parse(&bytes, idx).is_ok_and(|face| accept(&face)) {
                    return Some((leak_font(bytes), idx));
                }
            }
        }
    }
    None
}

/// Resolve a font `family` + exact style to its data and face index, or `None`.
///
/// A `family` that names a **file** is style-checked against the file's own OS/2
/// bits rather than accepted outright. `scan_fonts` takes a path as "use this
/// file", which is right for the primary slot but would otherwise claim the file
/// satisfies *every* style — so a path-configured font would report a real bold
/// face it doesn't have, and never synthesize one.
fn find_font(family: &str, bold: bool, italic: bool) -> Option<(&'static [u8], u32)> {
    if Path::new(family.trim()).is_file() {
        let (bytes, idx) = scan_fonts(family, |_| true)?;
        let face = ttf_parser::Face::parse(bytes, idx).ok()?;
        return (face.is_bold() == bold && face.is_italic() == italic).then_some((bytes, idx));
    }
    scan_fonts(family, |face| face_matches(face, family, bold, italic))
}

/// Resolve the *regular* slot tolerantly: prefer an exact flags-clear regular
/// face, else accept the family's first face of any style. Ghostty's regular
/// discovery doesn't constrain on style bits, so this keeps the user on their
/// configured font when a family ships only a styled master or carries unusual
/// OS/2 style metadata, rather than dropping the whole selection to the built-in.
fn find_regular_font(family: &str) -> Option<(&'static [u8], u32)> {
    find_font(family, false, false)
        .or_else(|| scan_fonts(family, |face| family_name_matches(face, family)))
        // A path is an explicit choice of file: the primary slot uses it whatever
        // its style bits say (`find_font` above only style-checks it so the
        // *synthesis* decision is honest).
        .or_else(|| scan_fonts(family, |_| true).filter(|_| Path::new(family.trim()).is_file()))
}

/// Resolve the four style slots (regular, bold, italic, bold-italic) to font
/// data + face index. A configured `font-family` whose *regular* face can't be
/// found is reported and ignored (the built-in font is kept rather than half
/// applying); missing style variants of a found family fall back to that
/// family's regular face, then to the built-in per-slot face.
/// Also decides, per slot, whether the style must be **synthesized**: a slot
/// that fell back to a face which isn't really the requested style gets an
/// embolden and/or a shear (unless `font-synthetic-style` disables it), which is
/// upstream's rule — "if the font has the requested style, the font is used
/// as-is". The decision is made from what actually resolved rather than from
/// which config key was set, so the fallback *chain* can't quietly change it.
fn resolve_slots(spec: &FontSpec) -> ([(&'static [u8], u32); 4], [Synth; 4]) {
    let embedded = [
        (FONT_REGULAR, 0u32),
        (FONT_BOLD, 0),
        (FONT_ITALIC, 0),
        (FONT_BOLD_ITALIC, 0),
    ];
    let primary = spec.families.first().map(String::as_str);
    // The family for each slot: a per-style override, else the primary family.
    fn slot_family<'a>(over: &'a Option<String>, primary: Option<&'a str>) -> Option<&'a str> {
        over.as_deref().or(primary)
    }
    let slots = [
        (primary, false, false),
        (slot_family(&spec.family_bold, primary), true, false),
        (slot_family(&spec.family_italic, primary), false, true),
        (slot_family(&spec.family_bold_italic, primary), true, true),
    ];

    let regular = primary.and_then(find_regular_font);
    // If the primary family is set but unresolvable (and no per-style override is
    // picking up the slack), keep the built-in font entirely.
    if primary.is_some()
        && regular.is_none()
        && spec.family_bold.is_none()
        && spec.family_italic.is_none()
        && spec.family_bold_italic.is_none()
    {
        if let Some(f) = primary {
            eprintln!("giest: font-family '{f}' not found; using the built-in font");
        }
        return (embedded, [Synth::default(); 4]);
    }

    let mut out = embedded;
    let mut synth = [Synth::default(); 4];
    // Which styled slots found a *real* face for their style. Computed first
    // because the bold-italic rule depends on the other two.
    let real: [bool; 4] = std::array::from_fn(|i| {
        let (family, bold, italic) = slots[i];
        i == 0 || family.is_some_and(|f| find_font(f, bold, italic).is_some())
    });

    for (i, (family, bold, italic)) in slots.into_iter().enumerate() {
        // Regular slot: the tolerant lookup. Styled slots: exact style → the
        // family's regular → built-in for this slot.
        let found = if i == 0 {
            regular
        } else {
            family.and_then(|f| find_font(f, bold, italic)).or(regular)
        };
        if let Some(found) = found {
            out[i] = found;
        }
    }
    // Nothing to synthesize when no family resolved: the built-in font has all
    // four real styles.
    if regular.is_none() {
        return (out, synth);
    }
    if !real[1] && spec.synthetic.bold {
        synth[1].bold = true;
    }
    if !real[2] && spec.synthetic.italic {
        synth[2].italic = true;
    }
    if !real[3] && spec.synthetic.bold_italic {
        // Upstream's preference order: shear the real bold if there is one, else
        // embolden the real italic, else do both to the regular. Slot 3 already
        // holds the family's regular here, so point it at whichever real styled
        // face exists.
        if real[1] {
            out[3] = out[1];
            synth[3].italic = true;
        } else if real[2] {
            out[3] = out[2];
            synth[3].bold = true;
        } else {
            synth[3] = Synth {
                bold: true,
                italic: true,
            };
        }
    }
    (out, synth)
}

/// How a glyph should be fitted to the terminal cell. Most characters are
/// [`Constraint::None`] (natural metrics); box-drawing and icon/emoji ranges are
/// sized to the cell so they tile seamlessly / don't overflow into neighbours.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub enum Constraint {
    /// Natural metrics, baseline-placed (normal text, CJK, ligatures).
    None,
    /// Stretch to fill `span*cell_w × cell_h` exactly so adjacent cells tile with
    /// no seam (box drawing, block elements, braille, powerline separators).
    Fill,
    /// Scale down (preserving aspect) to fit `span*cell_w × cell_h`, then centre
    /// horizontally in the span and vertically in the cell (Nerd Font icons,
    /// misc symbols, color emoji).
    Fit,
}

/// Shrink-only scale factor to fit a `gw × gh` glyph within `max_w × max_h`
/// preserving aspect (never enlarges). 0 if the glyph has no area.
fn fit_scale(gw: f32, gh: f32, max_w: f32, max_h: f32) -> f32 {
    if gw <= 0.0 || gh <= 0.0 {
        return 0.0;
    }
    (max_w / gw).min(max_h / gh).min(1.0)
}

/// Classify a character into a cell-fitting [`Constraint`] by Unicode range.
/// Keyed on the character (not the glyph id, which isn't stable across fonts).
pub fn classify(ch: char) -> Constraint {
    let c = ch as u32;
    match c {
        // Box drawing + block elements + geometric blocks → tile exactly. This
        // arm precedes the geometric-shapes Fit arm so blocks get Fill.
        0x2500..=0x259F => Constraint::Fill,
        // Braille patterns are designed to tile the cell.
        0x2800..=0x28FF => Constraint::Fill,
        // The Symbols for Legacy Computing mosaics giest draws (sextants,
        // octants, the eighth/quarter blocks). Listed for the same reason the
        // box-drawing arm is: the sprite path takes these *before* `classify` is
        // consulted, but the two tables should agree about what tiles the cell —
        // narrowing `sprite::covers` later would otherwise silently start
        // centring them instead.
        0x1FB00..=0x1FB6F | 0x1FB70..=0x1FB97 | 0x1FB9A..=0x1FBAF | 0x1CD00..=0x1CDE5 => {
            Constraint::Fill
        }
        // Powerline separators/arrows must touch cell edges — matched before the
        // broad PUA Fit arm below so they stay Fill.
        0xE0B0..=0xE0D4 => Constraint::Fill,
        // The entire Private Use Area is Nerd Font icon territory → fit (scale
        // down + centre). Enumerating individual icon ranges leaves gaps that
        // drop glyphs back to natural size (overflowing / clipped), so cover the
        // whole PUA and both supplementary PUA planes uniformly.
        0xE000..=0xF8FF                    // BMP Private Use Area
        | 0xF0000..=0xFFFFD               // Supplementary PUA-A (Material Design)
        | 0x100000..=0x10FFFD => Constraint::Fit, // Supplementary PUA-B
        // Misc symbols / arrows / dingbats / emoji pictographs → fit.
        0x2190..=0x21FF        // arrows
        | 0x2300..=0x24FF      // misc technical + control pictures
        | 0x25A0..=0x27BF      // geometric shapes (non-block) + misc symbols + dingbats
        | 0x2B00..=0x2BFF      // misc symbols and arrows (e.g. heavy circle prompts)
        | 0x1F000..=0x1FAFF => Constraint::Fit, // emoji & pictographs
        _ => Constraint::None,
    }
}

/// Placement of a rasterized glyph within the atlas, plus the offset needed to
/// position it inside a cell relative to the cell's top-left at the baseline.
#[derive(Clone, Copy)]
pub struct GlyphInfo {
    /// Atlas UV: (u_min, v_min, u_width, v_height), normalized 0..1.
    pub uv: [f32; 4],
    /// Pixel offset of the glyph quad from the cell's top-left. For `None`
    /// glyphs this is baseline-relative; for `Fit` glyphs it is cell-box-relative
    /// (already centred). Ignored for layout when `fill` is set.
    pub offset: [f32; 2],
    /// Glyph quad size in pixels. Ignored for layout when `fill` is set.
    pub size: [f32; 2],
    /// A `Fill` glyph: the renderer draws it at the exact integer cell rect
    /// (ignoring `offset`/`size`) so neighbouring box/block cells tile with no
    /// hairline seam. The coverage in the atlas was rasterized to fill the cell.
    pub fill: bool,
}

/// One shaped glyph from a run: the resolved glyph id and the byte offset
/// (cluster) into the run string it originated from — used to place it on the
/// grid cell where its first source character lives.
#[derive(Clone, Copy)]
pub struct ShapedGlyph {
    pub glyph_id: u16,
    pub cluster: u32,
}

/// A fallback lookup result: a monochrome coverage glyph (tinted by the cell's
/// foreground at draw time) or a color glyph sampled straight from the RGBA
/// color atlas.
#[derive(Clone, Copy)]
pub enum FallbackGlyph {
    Mono(GlyphInfo),
    Color(GlyphInfo),
}

pub struct Atlas {
    fonts: [FontRef<'static>; 4],
    shapers: [ShapeFace<'static>; 4],
    /// OpenType features applied to every shaped run (from `font-feature`). Empty
    /// = the font's own defaults (which still include `calt`/`liga`).
    features: Vec<Feature>,
    /// System fallback faces (owned font data), tried in order for characters
    /// the primary font lacks.
    fallbacks: Vec<FontVec>,
    /// Color (COLR/CPAL) emoji font, if present on the system.
    color_font: Option<ColorFont>,
    /// Reusable shaping buffer (taken out during `shape_run`, returned after).
    shape_buf: Option<UnicodeBuffer>,
    px: f32,
    /// Whether the render target is sRGB (color emoji are stored linear if so).
    srgb: bool,
    /// Monospace cell metrics in pixels (from the regular face).
    pub cell_w: f32,
    pub cell_h: f32,
    pub ascent: f32,
    texture: wgpu::Texture,
    pub view: wgpu::TextureView,
    /// RGBA color atlas for emoji.
    color_texture: wgpu::Texture,
    pub color_view: wgpu::TextureView,
    /// Cache keyed by (glyph id, style index, constraint). The constraint is
    /// part of the key because `Fill` bakes the cell box into the bitmap and
    /// `Fit` into the placement, so the same glyph id can have distinct entries.
    cache: HashMap<(u16, usize, Constraint), Option<GlyphInfo>>,
    /// Monochrome fallback glyphs cached by character.
    fallback_cache: HashMap<char, Option<GlyphInfo>>,
    /// Color emoji glyphs cached by character.
    color_cache: HashMap<char, Option<GlyphInfo>>,
    /// Sprite glyphs (box drawing / blocks / braille that giest draws itself)
    /// cached by character. Cleared with the rest on a font resize, since a
    /// sprite is drawn from the *cell* metrics and every one changes.
    sprite_cache: HashMap<char, Option<GlyphInfo>>,
    /// The `adjust-*` modifiers, kept so a font resize can re-derive.
    adjust: crate::config::MetricAdjust,
    /// Per-slot synthetic styling, decided once at load from what actually
    /// resolved (see `resolve_slots`).
    synth: [Synth; 4],
    /// Cell and decoration metrics, after the `adjust-*` modifiers.
    pub metrics: CellMetrics,
    /// Shaped-run cache, one map per style index, keyed by the run's text. Lets
    /// repeated frames skip rustybuzz for unchanged rows. Cleared on font resize
    /// (glyph ids change) and when it grows past `SHAPE_CACHE_CAP`.
    shape_cache: Vec<HashMap<Box<str>, Vec<ShapedGlyph>>>,
    // Coverage-atlas shelf allocator state.
    pen_x: u32,
    pen_y: u32,
    shelf_h: u32,
    // Color-atlas shelf allocator state.
    cpen_x: u32,
    cpen_y: u32,
    cshelf_h: u32,
}

impl Atlas {
    /// Build an atlas for the given pixel font size. `srgb` is whether the
    /// render target is sRGB (so color emoji are stored in linear space). `spec`
    /// selects the user font family and OpenType features (default = built-in
    /// JetBrains Mono with the font's own default features).
    pub fn new(device: &wgpu::Device, px: f32, srgb: bool, spec: &FontSpec) -> Self {
        // Resolve the four style slots to font data + face index (built-in font
        // when unconfigured / unresolved), then build the rasterizer and shaper
        // faces over the same bytes per slot.
        let (slots, synth) = resolve_slots(spec);
        let face = |i: usize| -> (FontRef<'static>, ShapeFace<'static>) {
            let (bytes, idx) = slots[i];
            // The built-in fonts always parse; a resolved user font that fails to
            // build (corrupt/unsupported) falls back to the built-in for that slot.
            let embedded = [FONT_REGULAR, FONT_BOLD, FONT_ITALIC, FONT_BOLD_ITALIC][i];
            let mut font = FontRef::try_from_slice_and_index(bytes, idx)
                .or_else(|_| FontRef::try_from_slice(embedded))
                .expect("font face");
            let mut shaper = ShapeFace::from_slice(bytes, idx)
                .or_else(|| ShapeFace::from_slice(embedded, 0))
                .expect("shaper face");
            // `font-variation` must reach **both** faces, and the reason is not
            // symmetry: the rasterizer draws the outline while the shaper decides
            // the advance, so setting an axis on one alone gives glyphs of one
            // weight positioned for another.
            apply_variations(&mut font, &mut shaper, &spec.variations[i]);
            (font, shaper)
        };
        let (f0, s0) = face(0);
        let (f1, s1) = face(1);
        let (f2, s2) = face(2);
        let (f3, s3) = face(3);
        let fonts = [f0, f1, f2, f3];
        let shapers = [s0, s1, s2, s3];
        // Mirror Ghostty: `liga` is forced on as a baseline, then the user's
        // features are appended so a later `-liga` (or any override) wins.
        let mut features = vec![Feature::from_str("liga").expect("liga feature")];
        features.extend(parse_features(&spec.features));
        // Load whichever system fallback fonts are present; missing ones are
        // simply skipped (e.g. a stripped-down Windows install).
        let mut fallbacks = Vec::new();
        // The user's own `font-family` chain comes first: a second family is
        // configured precisely to cover what the primary lacks, so it must beat
        // the system fonts. The primary itself is skipped — it is `fonts[0]` and
        // this list is only consulted for characters it doesn't have.
        for family in spec.families.iter().skip(1) {
            match find_regular_font(family)
                .and_then(|(bytes, idx)| FontVec::try_from_vec_and_index(bytes.to_vec(), idx).ok())
            {
                Some(mut font) => {
                    // The chain is part of `font-family`, and upstream applies
                    // `font-variation` to *every* descriptor built from that
                    // list — so these get it too. The system fallbacks below
                    // deliberately do not: nothing configured them, and an axis
                    // meant for your coding font has no business reshaping the
                    // emoji face it happens to share a tag with.
                    for v in &spec.variations[0] {
                        font.set_variation(&v.tag, v.value);
                    }
                    fallbacks.push(font);
                }
                None => eprintln!("giest: font-family '{family}' not found; skipping it"),
            }
        }
        for (path, index) in FALLBACK_FONTS {
            if let Ok(bytes) = std::fs::read(path) {
                if let Ok(font) = FontVec::try_from_vec_and_index(bytes, *index) {
                    fallbacks.push(font);
                }
            }
        }

        let metrics = derive_metrics(raw_metrics(&fonts[0], &shapers[0], px), &spec.adjust);
        let (cell_w, cell_h, ascent) = (metrics.cell_w, metrics.cell_h, metrics.ascent);

        let texture = device.create_texture(&wgpu::TextureDescriptor {
            label: Some("glyph-atlas"),
            size: wgpu::Extent3d {
                width: ATLAS_SIZE,
                height: ATLAS_SIZE,
                depth_or_array_layers: 1,
            },
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format: wgpu::TextureFormat::R8Unorm,
            usage: wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_DST,
            view_formats: &[],
        });
        let view = texture.create_view(&wgpu::TextureViewDescriptor::default());

        let color_texture = device.create_texture(&wgpu::TextureDescriptor {
            label: Some("glyph-color-atlas"),
            size: wgpu::Extent3d {
                width: ATLAS_SIZE,
                height: ATLAS_SIZE,
                depth_or_array_layers: 1,
            },
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format: wgpu::TextureFormat::Rgba8Unorm,
            usage: wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_DST,
            view_formats: &[],
        });
        let color_view = color_texture.create_view(&wgpu::TextureViewDescriptor::default());

        Self {
            fonts,
            shapers,
            features,
            fallbacks,
            color_font: ColorFont::load(COLOR_FONT),
            shape_buf: Some(UnicodeBuffer::new()),
            px,
            srgb,
            cell_w,
            cell_h,
            ascent,
            texture,
            view,
            color_texture,
            color_view,
            cache: HashMap::new(),
            fallback_cache: HashMap::new(),
            color_cache: HashMap::new(),
            sprite_cache: HashMap::new(),
            adjust: spec.adjust,
            synth,
            metrics,
            shape_cache: (0..4).map(|_| HashMap::new()).collect(),
            pen_x: 0,
            pen_y: 0,
            shelf_h: 0,
            cpen_x: 0,
            cpen_y: 0,
            cshelf_h: 0,
        }
    }

    /// Drop all cached glyphs/shaped runs and reset the shelf allocator so the
    /// next glyph requests re-rasterize from cold (metrics unchanged). Used by the
    /// render bench to measure rasterization cost rather than warm-cache lookups.
    pub fn reset_cache(&mut self) {
        self.set_px(self.px);
    }

    /// Change the rasterization pixel size: recompute the monospace cell
    /// metrics, drop cached glyphs, and reset the shelf allocator so subsequent
    /// glyphs re-rasterize at the new size into the (reused) atlas texture.
    /// Reusing the texture keeps the existing bind group valid.
    pub fn set_px(&mut self, px: f32) {
        self.metrics = derive_metrics(raw_metrics(&self.fonts[0], &self.shapers[0], px), &self.adjust);
        self.cell_w = self.metrics.cell_w;
        self.ascent = self.metrics.ascent;
        self.cell_h = self.metrics.cell_h;
        self.px = px;
        self.cache.clear();
        self.fallback_cache.clear();
        self.color_cache.clear();
        self.sprite_cache.clear();
        // Shaped runs (glyph ids) are size-independent, so this isn't required for
        // correctness, but a font resize is a natural point to bound cache memory.
        for m in &mut self.shape_cache {
            m.clear();
        }
        self.pen_x = 0;
        self.pen_y = 0;
        self.shelf_h = 0;
        self.cpen_x = 0;
        self.cpen_y = 0;
        self.cshelf_h = 0;
    }

    /// Shape `text` in the given style with rustybuzz, appending the resulting
    /// glyphs (id + source cluster) to `out`. Ligatures and contextual
    /// alternates in the font are applied here. Direction is forced LTR to match
    /// the terminal grid.
    pub fn shape_run(&mut self, text: &str, style: usize, out: &mut Vec<ShapedGlyph>) {
        // Identical (text, style) runs recur every frame for static content; skip
        // rustybuzz when we've already shaped this one.
        if let Some(cached) = self.shape_cache[style].get(text) {
            out.extend_from_slice(cached);
            return;
        }

        let mut buf = self.shape_buf.take().unwrap_or_else(UnicodeBuffer::new);
        buf.push_str(text);
        buf.set_direction(Direction::LeftToRight);
        let glyphs = rustybuzz::shape(&self.shapers[style], &self.features, buf);
        let start = out.len();
        for info in glyphs.glyph_infos() {
            out.push(ShapedGlyph {
                glyph_id: info.glyph_id as u16,
                cluster: info.cluster,
            });
        }
        self.shape_buf = Some(glyphs.clear());

        // Cache the freshly shaped glyphs, dropping the whole cache first if it
        // has grown too large (coarse bound; runs are re-shaped on the next miss).
        let total: usize = self.shape_cache.iter().map(HashMap::len).sum();
        if total >= SHAPE_CACHE_CAP {
            for m in &mut self.shape_cache {
                m.clear();
            }
        }
        self.shape_cache[style].insert(text.into(), out[start..].to_vec());
    }

    /// Style index (bit 0 = bold, bit 1 = italic) for the given attributes.
    pub fn style_index(bold: bool, italic: bool) -> usize {
        style_index(bold, italic)
    }

    /// Get the glyph for shaped `glyph_id` in the requested style, rasterizing
    /// and uploading it on first use. Returns `None` for blank / outline-less
    /// glyphs (e.g. the space glyph).
    pub fn glyph(
        &mut self,
        glyph_id: u16,
        style: usize,
        constraint: Constraint,
        span: u16,
        queue: &wgpu::Queue,
    ) -> Option<GlyphInfo> {
        if let Some(info) = self.cache.get(&(glyph_id, style, constraint)) {
            return *info;
        }
        let info = self.rasterize(glyph_id, style, constraint, span, queue);
        self.cache.insert((glyph_id, style, constraint), info);
        info
    }

    /// A **sprite** glyph: box drawing, block elements or braille, drawn by
    /// giest from the cell metrics rather than taken from the font.
    ///
    /// `None` for anything [`crate::sprite::covers`] doesn't claim, which is the
    /// signal to fall through to the normal font path. Checked *before* the
    /// font, matching Ghostty's `CodepointResolver` — these characters are
    /// defined relative to the cell, so the drawn version is right in every font
    /// and the font's is only ever right by luck.
    pub fn sprite_glyph(&mut self, ch: char, queue: &wgpu::Queue) -> Option<GlyphInfo> {
        if !crate::sprite::covers(ch) {
            return None;
        }
        if let Some(info) = self.sprite_cache.get(&ch) {
            return *info;
        }
        let info = self.rasterize_sprite(ch, queue);
        self.sprite_cache.insert(ch, info);
        info
    }

    /// The cell metrics a sprite is drawn against, with `adjust-box-thickness`
    /// applied. Rounded the same way the renderer rounds a cell rect, so the
    /// drawn tile lands on exactly the pixels the cell occupies.
    fn sprite_metrics(&self) -> crate::sprite::Metrics {
        crate::sprite::Metrics {
            w: self.cell_w.ceil().max(1.0) as u32,
            h: self.cell_h.ceil().max(1.0) as u32,
            // Font-derived now (the underline thickness, like upstream), not a
            // fraction of the cell height.
            thickness: self.metrics.box_thick.max(1.0) as u32,
        }
    }

    fn rasterize_sprite(&mut self, ch: char, queue: &wgpu::Queue) -> Option<GlyphInfo> {
        let m = self.sprite_metrics();
        let coverage = crate::sprite::draw(ch, m)?;
        Some(self.upload_coverage(&coverage, m.w, m.h, queue))
    }

    /// Resolve `ch` the primary font lacks: prefer a COLR/CPAL color glyph
    /// (emoji), else the monochrome system-fallback chain. Returns `None` if
    /// nothing covers it.
    pub fn glyph_fallback(
        &mut self,
        ch: char,
        span: u16,
        queue: &wgpu::Queue,
    ) -> Option<FallbackGlyph> {
        if let Some(info) = self.color_glyph(ch, span, queue) {
            return Some(FallbackGlyph::Color(info));
        }
        self.mono_fallback(ch, span, queue).map(FallbackGlyph::Mono)
    }

    /// Composite `ch` from the color (COLR/CPAL) emoji font into the color
    /// atlas, caching the result. `None` if there is no color font or `ch` is
    /// not a color glyph in it.
    fn color_glyph(&mut self, ch: char, span: u16, queue: &wgpu::Queue) -> Option<GlyphInfo> {
        if let Some(info) = self.color_cache.get(&ch) {
            return *info;
        }
        // Collect layers (borrowing the color font) and composite, before the
        // mutable upload borrow.
        let composited = self.color_font.as_ref().and_then(|cf| {
            let gid = cf.face.glyph_index(ch)?;
            if !cf.face.is_color_glyph(gid) {
                return None;
            }
            let mut collector = LayerCollector::default();
            let fg = ttf_parser::RgbaColor::new(255, 255, 255, 255);
            cf.face.paint_color_glyph(gid, 0, fg, &mut collector)?;
            composite_color_layers(&cf.raster, &collector.layers, self.px, self.srgb)
        });
        // Emoji em-boxes dwarf a text cell, so always fit + centre them.
        let info = composited.map(|(rgba, w, h, min)| {
            let raw = self.upload_color(&rgba, w, h, min, queue);
            self.constrain(raw, Constraint::Fit, span)
        });
        self.color_cache.insert(ch, info);
        info
    }

    /// Find the first monochrome fallback face that has an outline for `ch`,
    /// rasterize it into the coverage atlas, and cache the result.
    fn mono_fallback(&mut self, ch: char, span: u16, queue: &wgpu::Queue) -> Option<GlyphInfo> {
        if let Some(info) = self.fallback_cache.get(&ch) {
            return *info;
        }
        let mut raster = None;
        for fb in &self.fallbacks {
            let gid = fb.glyph_id(ch);
            if gid.0 == 0 {
                continue;
            }
            let glyph = gid.with_scale_and_position(self.px, point(0.0, 0.0));
            if let Some(r) = outline_to_bitmap(fb, glyph) {
                raster = Some(r);
                break;
            }
        }
        let info = raster.map(|r| {
            let raw = self.upload(r, queue);
            self.constrain(raw, classify(ch), span)
        });
        self.fallback_cache.insert(ch, info);
        info
    }

    /// Pack a composited RGBA color glyph into the color atlas and return its
    /// placement.
    fn upload_color(
        &mut self,
        rgba: &[u8],
        w: u32,
        h: u32,
        min: (f32, f32),
        queue: &wgpu::Queue,
    ) -> GlyphInfo {
        let (ax, ay) = self.alloc_color(w, h);
        queue.write_texture(
            wgpu::TexelCopyTextureInfo {
                texture: &self.color_texture,
                mip_level: 0,
                origin: wgpu::Origin3d { x: ax, y: ay, z: 0 },
                aspect: wgpu::TextureAspect::All,
            },
            rgba,
            wgpu::TexelCopyBufferLayout {
                offset: 0,
                bytes_per_row: Some(w * 4),
                rows_per_image: Some(h),
            },
            wgpu::Extent3d {
                width: w,
                height: h,
                depth_or_array_layers: 1,
            },
        );
        let inv = 1.0 / ATLAS_SIZE as f32;
        GlyphInfo {
            uv: [
                ax as f32 * inv,
                ay as f32 * inv,
                w as f32 * inv,
                h as f32 * inv,
            ],
            offset: [min.0, self.ascent + min.1],
            size: [w as f32, h as f32],
            fill: false,
        }
    }

    /// Reserve a `w`×`h` slot in the color atlas, advancing its shelf allocator.
    /// When the atlas fills vertically, flush it (reset the allocator and drop
    /// the color cache) and start over, so glyphs are never lost — they simply
    /// re-rasterize on next use. Without this, the allocator walked off the
    /// bottom edge and new emoji rendered as garbage or vanished permanently.
    fn alloc_color(&mut self, w: u32, h: u32) -> (u32, u32) {
        if self.cpen_x + w + 1 > ATLAS_SIZE {
            self.cpen_x = 0;
            self.cpen_y += self.cshelf_h + 1;
            self.cshelf_h = 0;
        }
        if self.cpen_y + h + 1 > ATLAS_SIZE {
            self.cpen_x = 0;
            self.cpen_y = 0;
            self.cshelf_h = 0;
            self.color_cache.clear();
        }
        let pos = (self.cpen_x, self.cpen_y);
        self.cpen_x += w + 1;
        self.cshelf_h = self.cshelf_h.max(h);
        pos
    }

    fn rasterize(
        &mut self,
        glyph_id: u16,
        style: usize,
        constraint: Constraint,
        span: u16,
        queue: &wgpu::Queue,
    ) -> Option<GlyphInfo> {
        match constraint {
            // Non-uniformly scale the glyph so its advance maps to the cell width
            // and its em-height to the cell height, then composite it into a
            // cell-sized coverage tile (see `upload_fill`). Box/block/braille
            // glyphs are axis-aligned, so anisotropic scaling keeps lines
            // straight, and cell-sized tiles let neighbours meet with no seam.
            Constraint::Fill => {
                let target_w = self.cell_w * span as f32;
                let target_h = self.cell_h;
                let font = &self.fonts[style];
                let scaled = font.as_scaled(PxScale::from(self.px));
                let adv = scaled.h_advance(GlyphId(glyph_id)).max(1.0);
                let em_h = (scaled.ascent() - scaled.descent()).max(1.0);
                let sx = self.px * (target_w / adv);
                let sy = self.px * (target_h / em_h);
                // Baseline within the scaled glyph: the em maps onto [0, cell_h],
                // so the baseline sits `ascent` of the way down that range.
                let fill_baseline = scaled.ascent() * (target_h / em_h);
                let glyph = GlyphId(glyph_id)
                    .with_scale_and_position(PxScale { x: sx, y: sy }, point(0.0, 0.0));
                let raster = outline_to_bitmap(font, glyph)?;
                // No synthesis on a `Fill` glyph: box/block characters are drawn
                // by `crate::sprite` anyway, and stretching one to the cell and
                // *then* emboldening it would push it past the cell's edges and
                // break the seamless tiling that constraint exists for.
                Some(self.upload_fill(raster, span, fill_baseline, queue))
            }
            _ => {
                let glyph = GlyphId(glyph_id).with_scale_and_position(self.px, point(0.0, 0.0));
                let raster = outline_to_bitmap(&self.fonts[style], glyph)?;
                let raster = self.synthesize(raster, style);
                let info = self.upload(raster, queue);
                Some(self.constrain(info, constraint, span))
            }
        }
    }

    /// Apply this slot's synthetic styling to a rasterized glyph, if any.
    ///
    /// Bold before italic: emboldening a sheared bitmap would thicken it along
    /// the slant and make the stems look uneven, which is also the order
    /// upstream composes them in (it emboldens the outline, then skews it).
    fn synthesize(&self, raster: Raster, style: usize) -> Raster {
        let synth = self.synth[style];
        if !synth.any() {
            return raster;
        }
        let raster = if synth.bold {
            embolden(&raster, embolden_strength(self.cell_h))
        } else {
            raster
        };
        if synth.italic {
            // `min.1` is the top of the bitmap relative to the baseline (negative
            // above it), so `-min.1` is the baseline's row within the bitmap.
            shear(&raster, ITALIC_SKEW, -raster.min.1)
        } else {
            raster
        }
    }

    /// Apply a `Fit` constraint to a naturally-rasterized glyph's placement:
    /// scale it DOWN (never up) to fit within `span*cell_w × cell_h` preserving
    /// aspect, then centre it horizontally in the span and vertically in the
    /// cell. The resulting `offset` is cell-box-relative (not baseline-relative).
    /// `None` glyphs are returned untouched (natural, baseline-placed).
    fn constrain(&self, mut info: GlyphInfo, constraint: Constraint, span: u16) -> GlyphInfo {
        if constraint != Constraint::Fit {
            return info;
        }
        let (gw, gh) = (info.size[0], info.size[1]);
        // Allow icons up to 2 cells wide (many Nerd Font glyphs are designed
        // double-width even when the grid reserves one cell) and the full cell
        // height; only ever shrink. This keeps wide icons (e.g. a folder) from
        // collapsing to a sliver while still capping overflow.
        let max_w = self.cell_w * span.max(2) as f32;
        // The *height* ceiling is `adjust-icon-height`, which defaults to the
        // cell height. Only the height: upstream adjusts the icon's maximum
        // height and lets the aspect ratio carry the width, so a taller icon is
        // also wider — which is what makes the option read as "bigger icons"
        // rather than "stretched" ones.
        let scale = fit_scale(gw, gh, max_w, self.metrics.icon_height);
        if scale <= 0.0 {
            return info;
        }
        let nh = gh * scale;
        info.size = [gw * scale, nh];
        // Keep the glyph's natural horizontal bearing (left-aligned in the cell,
        // overflowing right into the following cell — usually a space), and
        // centre it vertically within the cell.
        info.offset = [info.offset[0] * scale, (self.cell_h - nh) * 0.5];
        info.fill = false;
        info
    }

    /// Composite a non-uniformly-scaled `Fill` glyph into a full cell-sized
    /// coverage tile and upload it. The tile spans exactly `span*cell_w × cell_h`
    /// with the glyph placed at its scaled left bearing / baseline, so drawing
    /// the tile at the cell origin makes adjacent box/block cells tile seamlessly.
    fn upload_fill(
        &mut self,
        raster: Raster,
        span: u16,
        fill_baseline: f32,
        queue: &wgpu::Queue,
    ) -> GlyphInfo {
        let cw = (self.cell_w * span as f32).ceil() as u32;
        let ch = self.cell_h.ceil() as u32;
        let mut canvas = vec![0u8; (cw * ch) as usize];
        let x0 = raster.min.0.round() as i32;
        let y0 = (fill_baseline + raster.min.1).round() as i32;
        for gy in 0..raster.h as i32 {
            let ty = y0 + gy;
            if ty < 0 || ty >= ch as i32 {
                continue;
            }
            for gx in 0..raster.w as i32 {
                let tx = x0 + gx;
                if tx < 0 || tx >= cw as i32 {
                    continue;
                }
                let v = raster.bitmap[(gy as u32 * raster.w + gx as u32) as usize];
                let d = &mut canvas[(ty as u32 * cw + tx as u32) as usize];
                *d = (*d).max(v);
            }
        }
        self.upload_coverage(&canvas, cw, ch, queue)
    }

    /// Pack a `w × h` cell-sized coverage tile into the atlas as a `Fill` glyph.
    ///
    /// Shared by the stretched-font path ([`Self::upload_fill`]) and the drawn
    /// sprites, which produce the same thing by different means: a buffer that
    /// covers the cell exactly and is drawn at the cell origin.
    fn upload_coverage(
        &mut self,
        canvas: &[u8],
        cw: u32,
        ch: u32,
        queue: &wgpu::Queue,
    ) -> GlyphInfo {
        let (ax, ay) = self.alloc(cw, ch);
        queue.write_texture(
            wgpu::TexelCopyTextureInfo {
                texture: &self.texture,
                mip_level: 0,
                origin: wgpu::Origin3d { x: ax, y: ay, z: 0 },
                aspect: wgpu::TextureAspect::All,
            },
            canvas,
            wgpu::TexelCopyBufferLayout {
                offset: 0,
                bytes_per_row: Some(cw),
                rows_per_image: Some(ch),
            },
            wgpu::Extent3d {
                width: cw,
                height: ch,
                depth_or_array_layers: 1,
            },
        );
        let inv = 1.0 / ATLAS_SIZE as f32;
        GlyphInfo {
            uv: [
                ax as f32 * inv,
                ay as f32 * inv,
                cw as f32 * inv,
                ch as f32 * inv,
            ],
            offset: [0.0, 0.0],
            size: [cw as f32, ch as f32],
            fill: true,
        }
    }

    /// Pack an already-rasterized glyph bitmap into the atlas and return its
    /// placement. Split out from rasterization so the (immutable) font borrow
    /// is released before this (mutable) atlas borrow — letting both the
    /// primary faces and the owned fallback faces share one upload path.
    fn upload(&mut self, raster: Raster, queue: &wgpu::Queue) -> GlyphInfo {
        let Raster { bitmap, w, h, min } = raster;
        let (ax, ay) = self.alloc(w, h);
        queue.write_texture(
            wgpu::TexelCopyTextureInfo {
                texture: &self.texture,
                mip_level: 0,
                origin: wgpu::Origin3d { x: ax, y: ay, z: 0 },
                aspect: wgpu::TextureAspect::All,
            },
            &bitmap,
            wgpu::TexelCopyBufferLayout {
                offset: 0,
                bytes_per_row: Some(w),
                rows_per_image: Some(h),
            },
            wgpu::Extent3d {
                width: w,
                height: h,
                depth_or_array_layers: 1,
            },
        );

        let inv = 1.0 / ATLAS_SIZE as f32;
        GlyphInfo {
            uv: [
                ax as f32 * inv,
                ay as f32 * inv,
                w as f32 * inv,
                h as f32 * inv,
            ],
            offset: [min.0, self.ascent + min.1],
            size: [w as f32, h as f32],
            fill: false,
        }
    }

    /// Reserve a `w`x`h` slot, advancing the shelf allocator. When the atlas
    /// fills vertically, flush it (reset the allocator and drop the coverage
    /// caches) and start over, so glyphs are never lost — they re-rasterize on
    /// next use. Without this, the allocator walked off the bottom edge once the
    /// atlas filled, so new glyphs rendered as garbage or vanished permanently
    /// (the failed lookup was cached as `None` forever).
    fn alloc(&mut self, w: u32, h: u32) -> (u32, u32) {
        if self.pen_x + w + 1 > ATLAS_SIZE {
            self.pen_x = 0;
            self.pen_y += self.shelf_h + 1;
            self.shelf_h = 0;
        }
        if self.pen_y + h + 1 > ATLAS_SIZE {
            self.pen_x = 0;
            self.pen_y = 0;
            self.shelf_h = 0;
            self.cache.clear();
            self.fallback_cache.clear();
        }
        let pos = (self.pen_x, self.pen_y);
        self.pen_x += w + 1;
        self.shelf_h = self.shelf_h.max(h);
        pos
    }
}

#[cfg(test)]
mod tests {
    use super::{
        COLOR_FONT, ColorFont, Constraint, FALLBACK_FONTS, FONT_BOLD, FONT_BOLD_ITALIC,
        FONT_ITALIC, FONT_REGULAR, Feature, FontSpec, LayerCollector, ShapeFace, classify,
        composite_color_layers, face_matches, family_name_matches, find_font, find_regular_font,
        derive_metrics, fit_scale, has_4char_tag, parse_features, resolve_slots, RawFontMetrics,
        Raster, Synth, apply_variations, embolden, embolden_strength, leak_font, shear,
        ITALIC_SKEW,
    };
    use crate::config::MetricAdjust;
    use ab_glyph::{Font, FontRef, FontVec, ScaleFont, VariableFont};
    use rustybuzz::ttf_parser;
    use rustybuzz::{Direction, UnicodeBuffer};
    use std::str::FromStr;

    /// A plausible 14px face: 10×20 cell, underline 2px below the baseline.
    fn raw() -> RawFontMetrics {
        RawFontMetrics {
            advance: 10.0,
            ascent: 16.0,
            descent: -4.0,
            line_gap: 0.0,
            underline_pos: 2.0,
            underline_thick: 1.0,
            strikethrough_pos: 5.0,
            strikethrough_thick: 1.0,
        }
    }

    #[test]
    fn metrics_without_adjustments_follow_the_font() {
        let m = derive_metrics(raw(), &MetricAdjust::default());
        assert_eq!((m.cell_w, m.cell_h), (10.0, 20.0));
        assert_eq!(m.ascent, 16.0);
        // Positions are measured from the top of the cell.
        assert_eq!(m.underline_pos, 18.0, "baseline 16 + 2 below it");
        assert_eq!(m.strikethrough_pos, 11.0, "baseline 16 - 5 above it");
        // Upstream's defaults for the metrics no font provides.
        assert_eq!(m.overline_pos, 0.0, "the overline is the top of the cell");
        assert_eq!(m.overline_thick, m.underline_thick);
        assert_eq!(m.box_thick, m.underline_thick);
        assert_eq!(m.cursor_height, m.cell_h);
    }

    #[test]
    fn adjust_cell_height_centres_the_text_and_moves_the_decorations_with_it() {
        // The line-spacing case: +8px of cell height puts 4 above the text.
        let mut a = MetricAdjust::default();
        a.cell_height = crate::config::MetricModifier::Pixels(8);
        let m = derive_metrics(raw(), &a);
        assert_eq!(m.cell_h, 28.0);
        assert_eq!(m.ascent, 20.0, "half the growth goes above the baseline");
        // The underline follows the baseline rather than staying put, or it would
        // drift into the text as the spacing grows.
        assert_eq!(m.underline_pos, 22.0);
        assert_eq!(m.cell_w, 10.0, "height alone must not change the width");
    }

    #[test]
    fn adjust_cell_width_is_applied_before_the_ceil() {
        // The trap: with a fractional advance the ceil has already eaten a small
        // percentage adjustment, so applying it afterwards does nothing.
        let mut fractional = raw();
        fractional.advance = 9.6;
        let mut a = MetricAdjust::default();
        a.cell_width = crate::config::MetricModifier::Percent(0.04);
        assert_eq!(derive_metrics(fractional, &a).cell_w, 10.0);
        // …and the same adjustment on a whole-pixel advance still moves it.
        a.cell_width = crate::config::MetricModifier::Pixels(2);
        assert_eq!(derive_metrics(raw(), &a).cell_w, 12.0);
    }

    #[test]
    fn positions_and_thicknesses_adjust_independently() {
        let mut a = MetricAdjust::default();
        a.underline_position = crate::config::MetricModifier::Pixels(3);
        a.underline_thickness = crate::config::MetricModifier::Pixels(2);
        a.strikethrough_position = crate::config::MetricModifier::Pixels(-2);
        a.overline_position = crate::config::MetricModifier::Pixels(1);
        let m = derive_metrics(raw(), &a);
        assert_eq!(m.underline_pos, 21.0);
        assert_eq!(m.underline_thick, 3.0);
        assert_eq!(m.strikethrough_pos, 9.0);
        assert_eq!(m.overline_pos, 1.0);
        // The overline and box thicknesses default *from* the underline's, so an
        // underline adjustment carries into them unless they say otherwise.
        assert_eq!(m.overline_thick, 3.0);
        assert_eq!(m.box_thick, 3.0);
    }

    #[test]
    fn a_thickness_can_never_be_adjusted_to_zero() {
        let mut a = MetricAdjust::default();
        a.underline_thickness = crate::config::MetricModifier::Percent(-1.0);
        a.cursor_thickness = crate::config::MetricModifier::Pixels(-50);
        let m = derive_metrics(raw(), &a);
        assert_eq!(m.underline_thick, 1.0);
        assert_eq!(m.cursor_thick, 1.0);
        // A position, by contrast, may go negative — that is a real placement.
        a.overline_position = crate::config::MetricModifier::Pixels(-3);
        assert_eq!(derive_metrics(raw(), &a).overline_pos, -3.0);
    }

    #[test]
    fn adjust_icon_height_moves_only_the_icon_ceiling() {
        let mut a = MetricAdjust::default();
        // Unset, the ceiling *is* the cell height — an icon is scaled to fit the
        // cell, which is what every other metric here assumes.
        let base = derive_metrics(raw(), &a);
        assert_eq!(base.icon_height, base.cell_h);
        a.icon_height = crate::config::MetricModifier::Percent(0.25);
        let m = derive_metrics(raw(), &a);
        assert_eq!(m.icon_height, base.cell_h * 1.25);
        // …and nothing else moves with it: the grid, the text and the
        // decorations are unchanged, so a bigger icon does not reflow the row.
        assert_eq!(m.cell_h, base.cell_h);
        assert_eq!(m.cell_w, base.cell_w);
        assert_eq!(m.ascent, base.ascent);
        assert_eq!(m.underline_pos, base.underline_pos);
        // Clamped like a thickness, not free like a position: a ceiling of zero
        // would make every icon vanish, which reads as a missing glyph rather
        // than as a mis-set option.
        a.icon_height = crate::config::MetricModifier::Percent(-1.0);
        assert_eq!(derive_metrics(raw(), &a).icon_height, 1.0);
    }

    #[test]
    fn adjust_font_baseline_lifts_the_text_off_the_bottom() {
        let mut a = MetricAdjust::default();
        // Documented as a distance from the *bottom* of the cell, so a positive
        // value moves the text up — which is a *smaller* top-to-baseline ascent.
        a.font_baseline = crate::config::MetricModifier::Pixels(2);
        assert_eq!(derive_metrics(raw(), &a).ascent, 14.0);
    }

    #[test]
    fn color_emoji_composites_to_colored_rgba() {
        // Skip on a system without the color emoji font.
        let Some(cf) = ColorFont::load(COLOR_FONT) else {
            return;
        };
        // U+1F600 GRINNING FACE is a COLR/CPAL color glyph.
        let gid = cf.face.glyph_index('😀').expect("emoji glyph index");
        assert!(
            cf.face.is_color_glyph(gid),
            "grinning face should be a color glyph"
        );

        let mut collector = LayerCollector::default();
        let fg = ttf_parser::RgbaColor::new(255, 255, 255, 255);
        cf.face
            .paint_color_glyph(gid, 0, fg, &mut collector)
            .expect("paint color glyph");
        assert!(
            !collector.layers.is_empty(),
            "expected at least one color layer"
        );

        let (rgba, w, h, _) =
            composite_color_layers(&cf.raster, &collector.layers, 32.0, false).expect("composite");
        assert!(w > 0 && h > 0);
        // At least one pixel must be opaque and have a non-zero color channel.
        let colored = rgba
            .chunks_exact(4)
            .any(|p| p[3] > 0 && (p[0] > 0 || p[1] > 0 || p[2] > 0));
        assert!(
            colored,
            "composited emoji should have colored opaque pixels"
        );
    }

    #[test]
    fn fallback_chain_covers_cjk() {
        // The primary JetBrains Mono face has no CJK; the system fallback chain
        // must cover it so e.g. '中' renders instead of a blank cell.
        let primary = FontRef::try_from_slice(FONT_REGULAR).unwrap();
        assert_eq!(
            primary.glyph_id('中').0,
            0,
            "primary unexpectedly covers CJK"
        );

        let mut covered = false;
        let mut any_present = false;
        for (path, idx) in FALLBACK_FONTS {
            let Ok(bytes) = std::fs::read(path) else {
                continue;
            };
            any_present = true;
            if let Ok(f) = FontVec::try_from_vec_and_index(bytes, *idx) {
                if f.glyph_id('中').0 != 0 {
                    covered = true;
                    break;
                }
            }
        }
        if any_present {
            assert!(
                covered,
                "a fallback font is present but none covered CJK '中'"
            );
        }
    }

    /// Glyph ids the regular face produces for `s` after shaping.
    fn glyph_ids(s: &str) -> Vec<u16> {
        let face = ShapeFace::from_slice(FONT_REGULAR, 0).unwrap();
        let mut buf = UnicodeBuffer::new();
        buf.push_str(s);
        buf.set_direction(Direction::LeftToRight);
        rustybuzz::shape(&face, &[], buf)
            .glyph_infos()
            .iter()
            .map(|i| i.glyph_id as u16)
            .collect()
    }

    #[test]
    fn ligatures_apply_contextual_substitution() {
        // JetBrains Mono (like Fira Code) renders ligatures as contextual
        // alternates: each operator character keeps its own cell/advance, but
        // the shaper swaps in joined-shape glyphs. So shaping "!=" still yields
        // one glyph per cell, but those glyph ids differ from the standalone
        // '!' and '=' glyphs. If `calt` weren't applied they'd be identical —
        // this proves the shaper is doing ligature substitution.
        let plain = vec![glyph_ids("!")[0], glyph_ids("=")[0]];
        for lig in ["!=", "=>", "->", "==", ">=", "<="] {
            let ids = glyph_ids(lig);
            assert_eq!(
                ids.len(),
                lig.chars().count(),
                "{lig:?} should keep one glyph per cell (monospace advance)"
            );
        }
        assert_ne!(
            glyph_ids("!="),
            plain,
            "calt should substitute ligature-part glyphs for !="
        );
        // A plain letter pair is unaffected by ligature substitution.
        assert_eq!(glyph_ids("ab"), vec![glyph_ids("a")[0], glyph_ids("b")[0]]);
    }

    /// Glyph ids the regular face produces for `s` with the given OpenType
    /// features applied (the same path `shape_run` drives).
    fn glyph_ids_feat(s: &str, feats: &[&str]) -> Vec<u16> {
        let face = ShapeFace::from_slice(FONT_REGULAR, 0).unwrap();
        let features: Vec<Feature> = feats.iter().map(|f| Feature::from_str(f).unwrap()).collect();
        let mut buf = UnicodeBuffer::new();
        buf.push_str(s);
        buf.set_direction(Direction::LeftToRight);
        rustybuzz::shape(&face, &features, buf)
            .glyph_infos()
            .iter()
            .map(|i| i.glyph_id as u16)
            .collect()
    }

    #[test]
    fn disabling_calt_suppresses_ligatures() {
        // The standalone glyphs for '!' and '='.
        let plain = vec![glyph_ids("!")[0], glyph_ids("=")[0]];
        // calt on (font default): "!=" is ligature-substituted, so it differs.
        assert_ne!(glyph_ids("!="), plain, "ligature applies by default");
        // calt off (font-feature = -calt): "!=" reverts to the standalone glyphs,
        // proving the feature reaches the shaper.
        assert_eq!(
            glyph_ids_feat("!=", &["-calt"]),
            plain,
            "-calt suppresses the ligature substitution"
        );
    }

    #[test]
    fn parse_features_keeps_valid_drops_invalid() {
        let f = parse_features(&[
            "-calt".to_string(),
            "ss01".to_string(),
            String::new(), // invalid (empty)
        ]);
        assert_eq!(f.len(), 2, "two valid features parsed, the empty one dropped");
    }

    #[test]
    fn feature_tag_must_be_four_chars() {
        // Ghostty rejects non-4-char tags; rustybuzz would otherwise space-pad a
        // short tag into a bogus feature. The leading run after +/- and a quote
        // is the tag.
        assert!(has_4char_tag("ss01"));
        assert!(has_4char_tag("-calt"));
        assert!(has_4char_tag("cv01=2"));
        assert!(has_4char_tag("liga off"));
        assert!(has_4char_tag("'aalt' 2"));
        assert!(!has_4char_tag("sht"), "3-char tag rejected");
        assert!(!has_4char_tag("k"), "1-char tag rejected");
        assert!(!has_4char_tag("toolong"), "long tag rejected");
        assert!(!has_4char_tag(""));
        // parse_features applies the guard: the short tag is dropped.
        assert_eq!(parse_features(&["sht".into(), "ss01".into()]).len(), 1);
    }

    #[test]
    fn find_regular_font_prefers_exact_then_any() {
        // Consolas ships with Windows; skip cleanly if absent.
        if !std::path::Path::new(r"C:\Windows\Fonts\consola.ttf").exists() {
            return;
        }
        // The tolerant regular lookup resolves to a real Consolas face.
        let (bytes, idx) = find_regular_font("Consolas").expect("resolve Consolas regular");
        let face = ttf_parser::Face::parse(bytes, idx).unwrap();
        assert!(family_name_matches(&face, "consolas"));
        // The exact path is preferred, so the regular face is neither bold/italic.
        assert!(!face.is_bold() && !face.is_italic());
    }

    #[test]
    fn default_spec_uses_embedded_fonts() {
        // With no family configured, every slot is the built-in JetBrains Mono.
        // (Compare by content — a `const` may be duplicated in rodata, so pointer
        // identity isn't reliable; `==` is a cheap memcmp here.)
        let (slots, synth) = resolve_slots(&FontSpec::default());
        assert!(slots[0].0 == FONT_REGULAR && slots[0].1 == 0);
        assert!(slots[1].0 == FONT_BOLD);
        assert!(slots[2].0 == FONT_ITALIC);
        assert!(slots[3].0 == FONT_BOLD_ITALIC);
        // The built-in font has all four real styles, so nothing is synthesized.
        assert_eq!(synth, [Synth::default(); 4]);
    }

    /// A 4×4 solid square with its baseline at the bottom row.
    fn square() -> Raster {
        Raster {
            bitmap: vec![255; 16],
            w: 4,
            h: 4,
            min: (0.0, -4.0),
        }
    }

    #[test]
    fn embolden_adds_ink_without_moving_the_glyphs_origin() {
        let out = embolden(&square(), 1.0);
        assert_eq!((out.w, out.h), (5, 5), "one pixel of growth on each axis");
        assert!(out.bitmap.iter().filter(|&&v| v > 0).count() > 16, "more ink");
        // The bearing is unchanged: the extra weight falls right/below, the way a
        // bold face is heavier than its regular without shifting left.
        assert_eq!(out.min, (0.0, -4.0));
        // Corners of the original stay lit — a dilation, never a shift.
        assert_eq!(out.bitmap[0], 255);
    }

    #[test]
    fn embolden_max_blends_so_antialiased_edges_survive() {
        // A single mid-grey pixel must not sum to white when the copies overlap.
        let src = Raster {
            bitmap: vec![128],
            w: 1,
            h: 1,
            min: (0.0, 0.0),
        };
        let out = embolden(&src, 1.0);
        assert!(out.bitmap.iter().all(|&v| v == 0 || v == 128));
    }

    #[test]
    fn embolden_strength_scales_with_the_font_size() {
        // A fixed pixel amount would be invisible when large and clubby when
        // small, so upstream ties it to the height (~height/32).
        let small = embolden_strength(10.0);
        let large = embolden_strength(40.0);
        assert!(large > small, "{small} vs {large}");
        assert!((large / small - 4.0).abs() < 0.2, "roughly linear");
    }

    #[test]
    fn shear_leans_the_glyph_about_its_baseline() {
        // A tall bar: rows above the baseline move right, and by more the higher
        // they are.
        let src = Raster {
            bitmap: vec![255; 10],
            w: 1,
            h: 10,
            min: (0.0, -10.0),
        };
        let out = shear(&src, ITALIC_SKEW, 10.0);
        let lit_x = |row: u32| (0..out.w).find(|&x| out.bitmap[(row * out.w + x) as usize] > 0);
        let top = lit_x(0).expect("top row is lit");
        let bottom = lit_x(9).expect("bottom row is lit");
        assert!(top > bottom, "the top leans right of the bottom");
        // …by about tan(12°) of the height.
        assert!(((top - bottom) as f32 - 10.0 * ITALIC_SKEW).abs() <= 1.0);
        assert_eq!(out.h, src.h, "shearing never changes the height");
    }

    #[test]
    fn shear_moves_the_bearing_when_a_descender_swings_left() {
        // With the baseline above the bitmap's bottom, the descender rows shift
        // *left* — and the bearing has to follow, or the glyph walks out of its
        // cell instead of leaning inside it.
        let src = Raster {
            bitmap: vec![255; 8],
            w: 1,
            h: 8,
            min: (0.0, -4.0),
        };
        let out = shear(&src, ITALIC_SKEW, 4.0);
        assert!(out.min.0 < 0.0, "the bearing moved left with the descender");
    }

    #[test]
    fn a_family_missing_a_style_synthesizes_it_unless_disabled() {
        // A path to the embedded *regular* face: every styled slot must fall back
        // to it, which is exactly the "family has no bold" case. No dependence on
        // which system fonts happen to be installed.
        let dir = std::env::temp_dir().join("giest-synth-test");
        std::fs::create_dir_all(&dir).expect("temp dir");
        let path = dir.join("regular-only.ttf");
        std::fs::write(&path, FONT_REGULAR).expect("write font");
        let family = path.to_string_lossy().to_string();

        let spec = FontSpec {
            families: vec![family.clone()],
            ..Default::default()
        };
        let (_, synth) = resolve_slots(&spec);
        assert_eq!(synth[0], Synth::default(), "regular is never synthesized");
        assert!(synth[1].bold && !synth[1].italic, "bold is emboldened");
        assert!(synth[2].italic && !synth[2].bold, "italic is sheared");
        assert!(
            synth[3].bold && synth[3].italic,
            "with neither real style, bold-italic gets both"
        );

        // `font-synthetic-style = false` turns all three off; the slots then just
        // use the regular face as-is, which is upstream's documented behaviour.
        let spec = FontSpec {
            families: vec![family.clone()],
            synthetic: crate::config::SyntheticStyle {
                bold: false,
                italic: false,
                bold_italic: false,
            },
            ..Default::default()
        };
        let (_, synth) = resolve_slots(&spec);
        assert_eq!(synth, [Synth::default(); 4]);

        // The flags are independent: disabling `bold` leaves bold-italic alone.
        let spec = FontSpec {
            families: vec![family],
            synthetic: crate::config::SyntheticStyle {
                bold: false,
                italic: true,
                bold_italic: true,
            },
            ..Default::default()
        };
        let (_, synth) = resolve_slots(&spec);
        assert_eq!(synth[1], Synth::default(), "bold synthesis is off");
        assert!(synth[3].bold && synth[3].italic, "bold-italic is untouched");
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn the_bundled_font_has_no_variation_axes() {
        // Not a limitation of the *feature* — a fact about the asset, and the
        // one users will trip on: `font-variation` does nothing until you point
        // `font-family` at a variable font, because the bundled JetBrains Mono
        // is a static instance. Asserted so that swapping the asset for a VF
        // fails here and the documentation gets revisited with it.
        for bytes in [FONT_REGULAR, FONT_BOLD, FONT_ITALIC, FONT_BOLD_ITALIC] {
            let font = FontRef::try_from_slice(bytes).expect("bundled font parses");
            assert!(
                font.variations().is_empty(),
                "the bundled font gained variation axes; revisit the font-variation docs"
            );
        }
    }

    #[test]
    fn a_variation_moves_the_outline_and_the_shaper_together() {
        // The measurable half of `font-variation`, on a font Windows 10+ ships:
        // Segoe UI Variable's `wght` axis runs 300–700 and changes the advance
        // width of `M` (measured: 43.207 → 46.050 at 64px). That advance is what
        // makes this worth asserting rather than eyeballing — a variation
        // applied to the rasterizer but *not* the shaper would draw bold glyphs
        // on regular-width spacing, which reads as bad kerning rather than as a
        // missing feature.
        const PATH: &str = r"C:\Windows\Fonts\SegUIVar.ttf";
        let Ok(bytes) = std::fs::read(PATH) else {
            // Not every Windows install has it; the parser tests carry the
            // grammar and this one only adds the end-to-end effect.
            eprintln!("skipping: {PATH} not present");
            return;
        };
        let bytes = leak_font(bytes);
        let mut font = FontRef::try_from_slice(bytes).expect("variable font parses");
        let mut shaper = ShapeFace::from_slice(bytes, 0).expect("shaper face");
        let axes = font.variations();
        assert!(
            axes.iter().any(|a| &a.tag == b"wght"),
            "Segoe UI Variable should expose a wght axis"
        );

        let glyph = font.glyph_id('M');
        let before = font.as_scaled(64.0).h_advance(glyph);
        let before_shaped = shaped_advance(&shaper, "M");

        apply_variations(
            &mut font,
            &mut shaper,
            &[crate::config::FontVariation {
                tag: *b"wght",
                value: 700.0,
            }],
        );

        let after = font.as_scaled(64.0).h_advance(glyph);
        let after_shaped = shaped_advance(&shaper, "M");
        assert!(
            after > before,
            "a heavier weight should widen M: {before} -> {after}"
        );
        assert!(
            after_shaped > before_shaped,
            "the shaper must move with the outline: {before_shaped} -> {after_shaped}"
        );
    }

    #[test]
    fn an_unknown_axis_is_ignored_rather_than_fatal() {
        // Upstream: "invalid ids and values are usually ignored". A config
        // shared between machines will name axes some fonts lack.
        let mut font = FontRef::try_from_slice(FONT_REGULAR).expect("bundled font");
        let mut shaper = ShapeFace::from_slice(FONT_REGULAR, 0).expect("shaper");
        let glyph = font.glyph_id('M');
        let before = font.as_scaled(64.0).h_advance(glyph);
        apply_variations(
            &mut font,
            &mut shaper,
            &[crate::config::FontVariation {
                tag: *b"nope",
                value: 1.0,
            }],
        );
        assert_eq!(font.as_scaled(64.0).h_advance(glyph), before);
    }

    /// The advance rustybuzz reports for `text`, in font units — the shaper's
    /// half of the pair above.
    fn shaped_advance(face: &ShapeFace<'static>, text: &str) -> i32 {
        let mut buf = UnicodeBuffer::new();
        buf.push_str(text);
        let glyphs = rustybuzz::shape(face, &[], buf);
        glyphs.glyph_positions().iter().map(|p| p.x_advance).sum()
    }

    #[test]
    fn unknown_family_falls_back_to_embedded() {
        let spec = FontSpec {
            families: vec!["This Font Surely Does Not Exist 9000".into()],
            ..Default::default()
        };
        let (slots, _) = resolve_slots(&spec);
        assert!(
            slots[0].0 == FONT_REGULAR,
            "an unresolvable family keeps the built-in font"
        );
    }

    #[test]
    fn embedded_font_covers_ui_symbol_glyphs() {
        // The egui chrome (search bar buttons) falls back to this font for symbols
        // Ubuntu-Light lacks; assert the glyphs we use actually exist so they
        // don't render as tofu boxes.
        let f = FontRef::try_from_slice(FONT_REGULAR).unwrap();
        // The search-bar buttons: prev (↑), next (↓), close (×).
        for ch in ['\u{2191}', '\u{2193}', '\u{00d7}'] {
            assert_ne!(
                f.glyph_id(ch).0,
                0,
                "embedded font missing glyph U+{:04X}",
                ch as u32
            );
        }
    }

    #[test]
    fn finds_font_by_path_and_matches_family_and_style() {
        // Consolas ships with Windows; skip cleanly if absent.
        let consola = r"C:\Windows\Fonts\consola.ttf";
        if !std::path::Path::new(consola).exists() {
            return;
        }
        // An explicit path loads directly (face index 0).
        let (bytes, idx) = find_font(consola, false, false).expect("load Consolas by path");
        let face = ttf_parser::Face::parse(bytes, idx).unwrap();
        // Family match is case-insensitive; the regular face is neither bold/italic.
        assert!(face_matches(&face, "Consolas", false, false));
        assert!(face_matches(&face, "consolas", false, false));
        assert!(!face_matches(&face, "Consolas", true, false), "regular ≠ bold");
        assert!(!face_matches(&face, "Arial", false, false), "wrong family");
    }

    #[test]
    fn classify_assigns_expected_constraints() {
        // Box drawing, block elements, braille, powerline separators → Fill.
        assert_eq!(classify('─'), Constraint::Fill, "horizontal box line");
        assert_eq!(classify('│'), Constraint::Fill, "vertical box line");
        assert_eq!(classify('█'), Constraint::Fill, "full block");
        assert_eq!(classify('\u{2580}'), Constraint::Fill, "upper half block");
        assert_eq!(classify('\u{2800}'), Constraint::Fill, "braille blank");
        assert_eq!(classify('\u{1FB00}'), Constraint::Fill, "sextant");
        assert_eq!(classify('\u{1CD00}'), Constraint::Fill, "octant");
        assert_eq!(classify('\u{1FB82}'), Constraint::Fill, "upper quarter block");
        assert_eq!(classify('\u{1FB3C}'), Constraint::Fill, "smooth mosaic");
        assert_eq!(classify('\u{1FBA0}'), Constraint::Fill, "corner diagonal");
        assert_eq!(
            classify('\u{E0B0}'),
            Constraint::Fill,
            "powerline separator"
        );
        // Nerd Font icons + misc symbols + emoji → Fit.
        assert_eq!(classify('\u{EA60}'), Constraint::Fit, "codicon");
        assert_eq!(
            classify('\u{F0001}'),
            Constraint::Fit,
            "material design icon"
        );
        assert_eq!(classify('→'), Constraint::Fit, "arrow");
        assert_eq!(classify('😀'), Constraint::Fit, "emoji");
        // The whole PUA is Fit with no gaps — codepoints that previously fell
        // between enumerated icon ranges (and rendered un-fitted / cut off) must
        // now classify as Fit. These sit in former gaps.
        for &gap in &[
            '\u{E100}', '\u{E2C0}', '\u{E4A0}', '\u{E900}', '\u{EC80}', '\u{F418}', '\u{F600}',
        ] {
            assert_eq!(
                classify(gap),
                Constraint::Fit,
                "PUA gap {:#X} must fit",
                gap as u32
            );
        }
        // Ordinary text → None.
        assert_eq!(classify('a'), Constraint::None);
        assert_eq!(classify('中'), Constraint::None);
        assert_eq!(classify(' '), Constraint::None);
    }

    #[test]
    fn fit_scale_shrinks_oversized_preserving_aspect() {
        // A glyph wider/taller than the box shrinks by the binding dimension.
        // 40×48 into 20×24 → both dims bind equally → 0.5.
        assert!((fit_scale(40.0, 48.0, 20.0, 24.0) - 0.5).abs() < 1e-3);
        // A wide-but-short glyph (folder-like) binds on width only.
        assert!((fit_scale(30.0, 10.0, 15.0, 24.0) - 0.5).abs() < 1e-3);
    }

    #[test]
    fn average_stops_means_gradient_colors() {
        use rustybuzz::ttf_parser::RgbaColor;
        use rustybuzz::ttf_parser::colr::ColorStop;
        let stops = vec![
            ColorStop {
                stop_offset: 0.0,
                color: RgbaColor::new(0, 0, 0, 255),
            },
            ColorStop {
                stop_offset: 1.0,
                color: RgbaColor::new(200, 100, 50, 255),
            },
        ];
        assert_eq!(
            super::average_stops(stops.into_iter()),
            Some([100, 50, 25, 255])
        );
        // No stops → nothing to fill.
        assert_eq!(super::average_stops(std::iter::empty()), None);
    }

    #[test]
    fn fit_scale_never_upscales_or_divides_by_zero() {
        // A glyph already within the box keeps its natural size (scale 1).
        assert_eq!(fit_scale(6.0, 8.0, 16.0, 24.0), 1.0);
        // A zero-area glyph yields scale 0 (nothing to place).
        assert_eq!(fit_scale(0.0, 8.0, 16.0, 24.0), 0.0);
    }

    #[test]
    fn cluster_maps_back_to_source_byte() {
        // Shaped glyph clusters are byte offsets into the input, so they index
        // the run's byte→cell table the renderer builds.
        let face = ShapeFace::from_slice(FONT_REGULAR, 0).unwrap();
        let mut buf = UnicodeBuffer::new();
        buf.push_str("a=>b");
        buf.set_direction(Direction::LeftToRight);
        let g = rustybuzz::shape(&face, &[], buf);
        let clusters: Vec<u32> = g.glyph_infos().iter().map(|i| i.cluster).collect();
        // 'a' at 0, the "=>" ligature at byte 1, 'b' at byte 3.
        assert!(clusters.contains(&0));
        assert!(clusters.contains(&1));
        assert!(clusters.contains(&3));
    }
}
