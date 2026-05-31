//! Dynamic glyph atlas: rasterize glyphs on demand with ab_glyph and pack them
//! into a single R8 wgpu texture, caching UV rects per (character, style).
//!
//! Four JetBrains Mono Nerd Font variants are embedded so bold/italic render
//! with real glyphs (not faux styling), and Nerd Font icon/powerline glyphs are
//! available.

use std::collections::HashMap;

use ab_glyph::{Font, FontRef, PxScale, ScaleFont, point};
use eframe::wgpu;

const ATLAS_SIZE: u32 = 2048;

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

pub struct Atlas {
    fonts: [FontRef<'static>; 4],
    px: f32,
    /// Monospace cell metrics in pixels (from the regular face).
    pub cell_w: f32,
    pub cell_h: f32,
    pub ascent: f32,
    texture: wgpu::Texture,
    pub view: wgpu::TextureView,
    /// Cache keyed by (char, style index).
    cache: HashMap<(char, usize), Option<GlyphInfo>>,
    // Shelf allocator state.
    pen_x: u32,
    pen_y: u32,
    shelf_h: u32,
}

impl Atlas {
    /// Build an atlas for the given pixel font size.
    pub fn new(device: &wgpu::Device, px: f32) -> Self {
        let fonts = [
            FontRef::try_from_slice(FONT_REGULAR).expect("regular font"),
            FontRef::try_from_slice(FONT_BOLD).expect("bold font"),
            FontRef::try_from_slice(FONT_ITALIC).expect("italic font"),
            FontRef::try_from_slice(FONT_BOLD_ITALIC).expect("bold-italic font"),
        ];
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

        Self {
            fonts,
            px,
            cell_w,
            cell_h,
            ascent,
            texture,
            view,
            cache: HashMap::new(),
            pen_x: 0,
            pen_y: 0,
            shelf_h: 0,
        }
    }

    /// Get the glyph for `ch` in the requested style, rasterizing and uploading
    /// it on first use. Returns `None` for whitespace / outline-less glyphs.
    pub fn glyph(&mut self, ch: char, bold: bool, italic: bool, queue: &wgpu::Queue) -> Option<GlyphInfo> {
        let style = style_index(bold, italic);
        if let Some(info) = self.cache.get(&(ch, style)) {
            return *info;
        }
        let info = self.rasterize(ch, style, queue);
        self.cache.insert((ch, style), info);
        info
    }

    fn rasterize(&mut self, ch: char, style: usize, queue: &wgpu::Queue) -> Option<GlyphInfo> {
        let glyph = self.fonts[style]
            .glyph_id(ch)
            .with_scale_and_position(self.px, point(0.0, 0.0));
        let outline = self.fonts[style].outline_glyph(glyph)?;
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
        Some(GlyphInfo {
            uv: [ax as f32 * inv, ay as f32 * inv, w as f32 * inv, h as f32 * inv],
            offset: [bounds.min.x, self.ascent + bounds.min.y],
            size: [w as f32, h as f32],
        })
    }

    /// Reserve a `w`x`h` slot, advancing the shelf allocator.
    fn alloc(&mut self, w: u32, h: u32) -> (u32, u32) {
        if self.pen_x + w + 1 > ATLAS_SIZE {
            self.pen_x = 0;
            self.pen_y += self.shelf_h + 1;
            self.shelf_h = 0;
        }
        debug_assert!(self.pen_y + h <= ATLAS_SIZE, "glyph atlas overflow");
        let pos = (self.pen_x, self.pen_y);
        self.pen_x += w + 1;
        self.shelf_h = self.shelf_h.max(h);
        pos
    }
}
