//! Dynamic glyph atlas: rasterize glyphs on demand with ab_glyph and pack them
//! into a single R8 wgpu texture, caching UV rects per (glyph id, style).
//!
//! Four JetBrains Mono Nerd Font variants are embedded so bold/italic render
//! with real glyphs (not faux styling), and Nerd Font icon/powerline glyphs are
//! available. Text is shaped with rustybuzz (see [`Atlas::shape_run`]) so the
//! font's ligatures / contextual alternates apply; the renderer then snaps the
//! shaped glyphs onto the terminal grid by cluster.

use std::collections::HashMap;

use ab_glyph::{Font, FontRef, FontVec, GlyphId, PxScale, ScaleFont, point};
use eframe::wgpu;
use rustybuzz::ttf_parser;
use rustybuzz::{Direction, Face as ShapeFace, UnicodeBuffer};

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

impl<'a> ttf_parser::colr::Painter<'a> for LayerCollector {
    fn outline_glyph(&mut self, glyph_id: ttf_parser::GlyphId) {
        self.pending = Some(glyph_id.0);
    }
    fn paint(&mut self, paint: ttf_parser::colr::Paint<'a>) {
        if let (Some(gid), ttf_parser::colr::Paint::Solid(c)) = (self.pending.take(), paint) {
            self.layers.push((gid, [c.red, c.green, c.blue, c.alpha]));
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

const FONT_REGULAR: &[u8] = include_bytes!("../../assets/fonts/JetBrainsMonoNerdFont-Regular.ttf");
const FONT_BOLD: &[u8] = include_bytes!("../../assets/fonts/JetBrainsMonoNerdFont-Bold.ttf");
const FONT_ITALIC: &[u8] = include_bytes!("../../assets/fonts/JetBrainsMonoNerdFont-Italic.ttf");
const FONT_BOLD_ITALIC: &[u8] =
    include_bytes!("../../assets/fonts/JetBrainsMonoNerdFont-BoldItalic.ttf");

/// Style index into the font table: bit 0 = bold, bit 1 = italic.
fn style_index(bold: bool, italic: bool) -> usize {
    (bold as usize) | ((italic as usize) << 1)
}

/// Placement of a rasterized glyph within the atlas, plus the offset needed to
/// position it inside a cell relative to the cell's top-left at the baseline.
#[derive(Clone, Copy)]
pub struct GlyphInfo {
    /// Atlas UV: (u_min, v_min, u_width, v_height), normalized 0..1.
    pub uv: [f32; 4],
    /// Pixel offset of the glyph quad from the cell's top-left.
    pub offset: [f32; 2],
    /// Glyph quad size in pixels.
    pub size: [f32; 2],
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
    /// Cache keyed by (glyph id, style index).
    cache: HashMap<(u16, usize), Option<GlyphInfo>>,
    /// Monochrome fallback glyphs cached by character.
    fallback_cache: HashMap<char, Option<GlyphInfo>>,
    /// Color emoji glyphs cached by character.
    color_cache: HashMap<char, Option<GlyphInfo>>,
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
    /// render target is sRGB (so color emoji are stored in linear space).
    pub fn new(device: &wgpu::Device, px: f32, srgb: bool) -> Self {
        let fonts = [
            FontRef::try_from_slice(FONT_REGULAR).expect("regular font"),
            FontRef::try_from_slice(FONT_BOLD).expect("bold font"),
            FontRef::try_from_slice(FONT_ITALIC).expect("italic font"),
            FontRef::try_from_slice(FONT_BOLD_ITALIC).expect("bold-italic font"),
        ];
        let shapers = [
            ShapeFace::from_slice(FONT_REGULAR, 0).expect("regular shaper"),
            ShapeFace::from_slice(FONT_BOLD, 0).expect("bold shaper"),
            ShapeFace::from_slice(FONT_ITALIC, 0).expect("italic shaper"),
            ShapeFace::from_slice(FONT_BOLD_ITALIC, 0).expect("bold-italic shaper"),
        ];
        // Load whichever system fallback fonts are present; missing ones are
        // simply skipped (e.g. a stripped-down Windows install).
        let mut fallbacks = Vec::new();
        for (path, index) in FALLBACK_FONTS {
            if let Ok(bytes) = std::fs::read(path) {
                if let Ok(font) = FontVec::try_from_vec_and_index(bytes, *index) {
                    fallbacks.push(font);
                }
            }
        }

        let scaled = fonts[0].as_scaled(PxScale::from(px));
        let cell_w = scaled.h_advance(fonts[0].glyph_id(' ')).ceil();
        let ascent = scaled.ascent();
        let cell_h = (scaled.ascent() - scaled.descent() + scaled.line_gap()).ceil();

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
            shape_cache: (0..4).map(|_| HashMap::new()).collect(),
            pen_x: 0,
            pen_y: 0,
            shelf_h: 0,
            cpen_x: 0,
            cpen_y: 0,
            cshelf_h: 0,
        }
    }

    /// Change the rasterization pixel size: recompute the monospace cell
    /// metrics, drop cached glyphs, and reset the shelf allocator so subsequent
    /// glyphs re-rasterize at the new size into the (reused) atlas texture.
    /// Reusing the texture keeps the existing bind group valid.
    pub fn set_px(&mut self, px: f32) {
        let scaled = self.fonts[0].as_scaled(PxScale::from(px));
        self.cell_w = scaled.h_advance(self.fonts[0].glyph_id(' ')).ceil();
        self.ascent = scaled.ascent();
        self.cell_h = (scaled.ascent() - scaled.descent() + scaled.line_gap()).ceil();
        self.px = px;
        self.cache.clear();
        self.fallback_cache.clear();
        self.color_cache.clear();
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
        let glyphs = rustybuzz::shape(&self.shapers[style], &[], buf);
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
    pub fn glyph(&mut self, glyph_id: u16, style: usize, queue: &wgpu::Queue) -> Option<GlyphInfo> {
        if let Some(info) = self.cache.get(&(glyph_id, style)) {
            return *info;
        }
        let info = self.rasterize(glyph_id, style, queue);
        self.cache.insert((glyph_id, style), info);
        info
    }

    /// Resolve `ch` the primary font lacks: prefer a COLR/CPAL color glyph
    /// (emoji), else the monochrome system-fallback chain. Returns `None` if
    /// nothing covers it.
    pub fn glyph_fallback(&mut self, ch: char, queue: &wgpu::Queue) -> Option<FallbackGlyph> {
        if let Some(info) = self.color_glyph(ch, queue) {
            return Some(FallbackGlyph::Color(info));
        }
        self.mono_fallback(ch, queue).map(FallbackGlyph::Mono)
    }

    /// Composite `ch` from the color (COLR/CPAL) emoji font into the color
    /// atlas, caching the result. `None` if there is no color font or `ch` is
    /// not a color glyph in it.
    fn color_glyph(&mut self, ch: char, queue: &wgpu::Queue) -> Option<GlyphInfo> {
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
        let info = composited.map(|(rgba, w, h, min)| self.upload_color(&rgba, w, h, min, queue));
        self.color_cache.insert(ch, info);
        info
    }

    /// Find the first monochrome fallback face that has an outline for `ch`,
    /// rasterize it into the coverage atlas, and cache the result.
    fn mono_fallback(&mut self, ch: char, queue: &wgpu::Queue) -> Option<GlyphInfo> {
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
        let info = raster.map(|r| self.upload(r, queue));
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
            uv: [ax as f32 * inv, ay as f32 * inv, w as f32 * inv, h as f32 * inv],
            offset: [min.0, self.ascent + min.1],
            size: [w as f32, h as f32],
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

    fn rasterize(&mut self, glyph_id: u16, style: usize, queue: &wgpu::Queue) -> Option<GlyphInfo> {
        let glyph = GlyphId(glyph_id).with_scale_and_position(self.px, point(0.0, 0.0));
        let raster = outline_to_bitmap(&self.fonts[style], glyph)?;
        Some(self.upload(raster, queue))
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
            uv: [ax as f32 * inv, ay as f32 * inv, w as f32 * inv, h as f32 * inv],
            offset: [min.0, self.ascent + min.1],
            size: [w as f32, h as f32],
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
        COLOR_FONT, ColorFont, FALLBACK_FONTS, FONT_REGULAR, LayerCollector, ShapeFace,
        composite_color_layers,
    };
    use ab_glyph::{Font, FontRef, FontVec};
    use rustybuzz::ttf_parser;
    use rustybuzz::{Direction, UnicodeBuffer};

    #[test]
    fn color_emoji_composites_to_colored_rgba() {
        // Skip on a system without the color emoji font.
        let Some(cf) = ColorFont::load(COLOR_FONT) else {
            return;
        };
        // U+1F600 GRINNING FACE is a COLR/CPAL color glyph.
        let gid = cf.face.glyph_index('😀').expect("emoji glyph index");
        assert!(cf.face.is_color_glyph(gid), "grinning face should be a color glyph");

        let mut collector = LayerCollector::default();
        let fg = ttf_parser::RgbaColor::new(255, 255, 255, 255);
        cf.face
            .paint_color_glyph(gid, 0, fg, &mut collector)
            .expect("paint color glyph");
        assert!(!collector.layers.is_empty(), "expected at least one color layer");

        let (rgba, w, h, _) =
            composite_color_layers(&cf.raster, &collector.layers, 32.0, false).expect("composite");
        assert!(w > 0 && h > 0);
        // At least one pixel must be opaque and have a non-zero color channel.
        let colored = rgba
            .chunks_exact(4)
            .any(|p| p[3] > 0 && (p[0] > 0 || p[1] > 0 || p[2] > 0));
        assert!(colored, "composited emoji should have colored opaque pixels");
    }

    #[test]
    fn fallback_chain_covers_cjk() {
        // The primary JetBrains Mono face has no CJK; the system fallback chain
        // must cover it so e.g. '中' renders instead of a blank cell.
        let primary = FontRef::try_from_slice(FONT_REGULAR).unwrap();
        assert_eq!(primary.glyph_id('中').0, 0, "primary unexpectedly covers CJK");

        let mut covered = false;
        let mut any_present = false;
        for (path, idx) in FALLBACK_FONTS {
            let Ok(bytes) = std::fs::read(path) else { continue };
            any_present = true;
            if let Ok(f) = FontVec::try_from_vec_and_index(bytes, *idx) {
                if f.glyph_id('中').0 != 0 {
                    covered = true;
                    break;
                }
            }
        }
        if any_present {
            assert!(covered, "a fallback font is present but none covered CJK '中'");
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
