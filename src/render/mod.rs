//! GPU renderer for the terminal grid: a single instanced-quad wgpu pipeline
//! that paints cell backgrounds, glyphs (sampled from the [`Atlas`]), and the
//! cursor, driven from a [`GridSnapshot`] inside an egui paint callback.

mod atlas;

use std::ops::Range;
use std::sync::Arc;

use atlas::{Atlas, FallbackGlyph, ShapedGlyph};
pub use atlas::FontSpec;
use eframe::egui_wgpu::{self, CallbackTrait};
use eframe::wgpu::{self, util::DeviceExt};
use unicode_width::UnicodeWidthChar;

use crate::engine::{CursorShape, GridSnapshot, Rgb, UnderlineStyle};

/// The embedded primary (regular) monospace font bytes. Exposed for benches that
/// measure shaping throughput against the real shaping path (rustybuzz over this
/// face) without needing a GPU device to build a full [`Atlas`].
pub fn regular_font() -> &'static [u8] {
    atlas::FONT_REGULAR
}

/// One instanced quad. `mode` 0 = solid fill (backgrounds/cursor/straight
/// underlines), 1 = glyph (alpha = atlas coverage), 2 = color emoji, 3 =
/// procedural decoration (dotted/dashed/curly underline, sub-style chosen by
/// `param`). Rect and uv are absolute pixels / normalized UV; for mode 3, `uv.x`
/// is an absolute-pixel phase (so the pattern tiles seamlessly across cells) and
/// `uv.y` spans 0..1 over the quad height.
#[repr(C)]
#[derive(Clone, Copy, bytemuck::Pod, bytemuck::Zeroable)]
struct Instance {
    rect: [f32; 4],
    uv: [f32; 4],
    color: [f32; 4],
    mode: u32,
    param: u32,
    _pad: [u32; 2],
}

/// Procedural underline sub-styles for `Instance::mode == 3`, passed in `param`.
const DECO_DOTTED: u32 = 0;
const DECO_DASHED: u32 = 1;
const DECO_CURLY: u32 = 2;

/// Alpha applied to faint/dim (SGR 2) glyphs and decorations — a partial
/// opacity over the background, matching Ghostty's default faint look.
const FAINT_ALPHA: f32 = 0.55;

/// Background tint for scrollback-search matches; the *current* (navigated) match
/// uses the brighter shade so it stands out among the others.
const SEARCH_MATCH_BG: Rgb = Rgb::new(0x53, 0x49, 0x1a);
const SEARCH_CURRENT_BG: Rgb = Rgb::new(0xc2, 0x9c, 0x22);

impl Instance {
    fn solid(rect: [f32; 4], color: [f32; 4]) -> Self {
        Self {
            rect,
            uv: [0.0; 4],
            color,
            mode: 0,
            param: 0,
            _pad: [0; 2],
        }
    }

    /// A procedural decoration quad (mode 3). `phase_x0` is the quad's left edge
    /// in absolute pixels and `period` the pattern period in pixels, encoded into
    /// `uv` so the fragment shader can evaluate the pattern continuously across
    /// adjacent cells.
    fn deco(rect: [f32; 4], color: [f32; 4], style: u32, period: f32) -> Self {
        let p = period.max(1.0);
        Self {
            rect,
            uv: [rect[0] / p, 0.0, rect[2] / p, 1.0],
            color,
            mode: 3,
            param: style,
            _pad: [0; 2],
        }
    }
}

const QUAD_CORNERS: [[f32; 2]; 4] = [[0.0, 0.0], [1.0, 0.0], [0.0, 1.0], [1.0, 1.0]];
const QUAD_INDICES: [u16; 6] = [0, 1, 2, 2, 1, 3];
const INITIAL_INSTANCES: u64 = 8192;

/// Persistent GPU resources, stored in egui's `callback_resources`.
pub struct GpuResources {
    pipeline: wgpu::RenderPipeline,
    bind_group: wgpu::BindGroup,
    uniform: wgpu::Buffer,
    corners: wgpu::Buffer,
    indices: wgpu::Buffer,
    instances: wgpu::Buffer,
    capacity: u64,
    atlas: Atlas,
    is_srgb: bool,
    /// Coverage gamma for text antialiasing (>1 thickens light-on-dark AA).
    /// Passed to the shader as its reciprocal each frame.
    text_gamma: f32,
    num_instances: u32,
    /// Per-pane instance ranges and their clip rect (device px `[x, y, w, h]`),
    /// so `paint` can scissor each pane independently — needed because smooth
    /// scrolling overdraws a partial row past the pane's top/bottom edge.
    pane_ranges: Vec<(Range<u32>, [f32; 4])>,
    /// Reusable per-frame scratch buffers. Retained across frames (cleared, not
    /// reallocated) so building the instance list does no per-frame growth
    /// allocation. `scratch_out` also holds the assembled instance list that
    /// `prepare` uploads after `build_instances` returns.
    scratch_out: Vec<Instance>,
    scratch_glyphs: Vec<Instance>,
    scratch_cursors: Vec<Instance>,
    scratch_runs: Vec<GlyphRun>,
    scratch_shaped: Vec<ShapedGlyph>,
}

/// One pane (terminal grid) within a frame, positioned at `origin_px`.
pub struct PaneFrame {
    pub snapshot: Arc<GridSnapshot>,
    pub origin_px: [f32; 2],
    /// Inclusive linear (row-major) cell range to highlight as selected.
    pub selection: Option<(usize, usize)>,
    /// Draw the block cursor as a hollow outline rather than a filled cell
    /// (Ghostty does this for unfocused panes / when the window loses focus).
    pub cursor_hollow: bool,
    /// Hide the cursor this frame for blink-off (kept off the snapshot so the
    /// blink toggle needs no snapshot mutation/clone).
    pub cursor_blink_hidden: bool,
    /// Hide blink-attributed cells this frame (the blink-off phase). Shared
    /// across panes from one app-level clock; only matters when the snapshot has
    /// blinking cells (`GridSnapshot::has_blink`).
    pub blink_hidden: bool,
    /// Sub-line vertical offset (device px, `0..cell_h`) to shift the grid down
    /// by for smooth scrolling. When `> 0` the snapshot's `over_row` is drawn at
    /// the top and the bottom row overhangs; the pane scissor trims both.
    pub scroll_offset_px: f32,
    /// Scrollback-search matches visible in this pane's viewport (their cells get
    /// a highlight tint; the `current` one is brighter). Empty when no search.
    pub search_highlights: Vec<crate::search::SearchHighlight>,
}

/// Per-frame data handed to the paint callback: every visible pane. Rendering
/// all panes in one callback keeps them on a single shared instance buffer.
pub struct TermFrame {
    pub panes: Vec<PaneFrame>,
    /// Background color for selected cells.
    pub selection_bg: Rgb,
    /// Text color over a selection; `None` keeps each cell's own foreground.
    pub selection_fg: Option<Rgb>,
}

/// Build the per-cell search-highlight mask for a viewport (`0` none, `1` match,
/// `2` current match), indexed `row * cols + col`. Returns an empty vec when
/// there's nothing to highlight so the cell loop can skip the lookup entirely.
fn build_search_mask(
    highlights: &[crate::search::SearchHighlight],
    cols: u16,
    rows: u16,
) -> Vec<u8> {
    if highlights.is_empty() || cols == 0 || rows == 0 {
        return Vec::new();
    }
    let mut mask = vec![0u8; cols as usize * rows as usize];
    for h in highlights {
        if h.row >= rows {
            continue;
        }
        let base = h.row as usize * cols as usize;
        let last = h.col_end.min(cols - 1);
        for c in h.col_start..=last {
            let v = if h.current { 2 } else { 1 };
            mask[base + c as usize] = mask[base + c as usize].max(v);
        }
    }
    mask
}

/// A maximal horizontal run of cells sharing fg color and style, gathered for
/// shaping. `byte_cell[i]` is the grid column the byte at offset `i` in `text`
/// came from, so a shaped glyph's cluster maps back to its origin cell.
struct GlyphRun {
    text: String,
    byte_cell: Vec<u16>,
    fg: Rgb,
    style: usize,
    /// Faint (SGR 2) cells render at reduced alpha and break runs from
    /// non-faint cells so a single run has a uniform opacity.
    faint: bool,
}

/// Build the renderer resources and register them with egui. Returns the
/// monospace cell size in physical pixels so the app can size the grid.
pub fn init(
    render_state: &egui_wgpu::RenderState,
    px: f32,
    text_gamma: f32,
    font: &FontSpec,
) -> (f32, f32) {
    let resources = build_resources(
        &render_state.device,
        render_state.target_format,
        px,
        text_gamma,
        font,
    );
    let cell = resources.cell_size();
    render_state
        .renderer
        .write()
        .callback_resources
        .insert(resources);
    cell
}

/// Build the renderer's persistent GPU resources for `format` at pixel font size
/// `px`. Used by [`init`] (which then registers them with egui) and by the
/// headless render bench, which drives [`GpuResources::build_frame_instances`]
/// directly against an offscreen device — no surface or egui needed.
pub fn build_resources(
    device: &wgpu::Device,
    format: wgpu::TextureFormat,
    px: f32,
    text_gamma: f32,
    font: &FontSpec,
) -> GpuResources {
    let atlas = Atlas::new(device, px, format.is_srgb(), font);

    let shader = device.create_shader_module(wgpu::ShaderModuleDescriptor {
        label: Some("term-shader"),
        source: wgpu::ShaderSource::Wgsl(SHADER.into()),
    });

    let bind_layout = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
        label: Some("term-bind-layout"),
        entries: &[
            wgpu::BindGroupLayoutEntry {
                binding: 0,
                visibility: wgpu::ShaderStages::VERTEX_FRAGMENT,
                ty: wgpu::BindingType::Buffer {
                    ty: wgpu::BufferBindingType::Uniform,
                    has_dynamic_offset: false,
                    min_binding_size: None,
                },
                count: None,
            },
            wgpu::BindGroupLayoutEntry {
                binding: 1,
                visibility: wgpu::ShaderStages::FRAGMENT,
                ty: wgpu::BindingType::Texture {
                    sample_type: wgpu::TextureSampleType::Float { filterable: true },
                    view_dimension: wgpu::TextureViewDimension::D2,
                    multisampled: false,
                },
                count: None,
            },
            wgpu::BindGroupLayoutEntry {
                binding: 2,
                visibility: wgpu::ShaderStages::FRAGMENT,
                ty: wgpu::BindingType::Sampler(wgpu::SamplerBindingType::Filtering),
                count: None,
            },
            // Binding 3: RGBA color atlas (emoji).
            wgpu::BindGroupLayoutEntry {
                binding: 3,
                visibility: wgpu::ShaderStages::FRAGMENT,
                ty: wgpu::BindingType::Texture {
                    sample_type: wgpu::TextureSampleType::Float { filterable: true },
                    view_dimension: wgpu::TextureViewDimension::D2,
                    multisampled: false,
                },
                count: None,
            },
        ],
    });

    let uniform = device.create_buffer(&wgpu::BufferDescriptor {
        label: Some("term-uniform"),
        size: 16,
        usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
        mapped_at_creation: false,
    });
    let sampler = device.create_sampler(&wgpu::SamplerDescriptor {
        label: Some("term-sampler"),
        mag_filter: wgpu::FilterMode::Linear,
        min_filter: wgpu::FilterMode::Linear,
        ..Default::default()
    });
    let bind_group = device.create_bind_group(&wgpu::BindGroupDescriptor {
        label: Some("term-bind-group"),
        layout: &bind_layout,
        entries: &[
            wgpu::BindGroupEntry {
                binding: 0,
                resource: uniform.as_entire_binding(),
            },
            wgpu::BindGroupEntry {
                binding: 1,
                resource: wgpu::BindingResource::TextureView(&atlas.view),
            },
            wgpu::BindGroupEntry {
                binding: 2,
                resource: wgpu::BindingResource::Sampler(&sampler),
            },
            wgpu::BindGroupEntry {
                binding: 3,
                resource: wgpu::BindingResource::TextureView(&atlas.color_view),
            },
        ],
    });

    let pipeline_layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
        label: Some("term-pipeline-layout"),
        bind_group_layouts: &[Some(&bind_layout)],
        immediate_size: 0,
    });

    let pipeline = device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
        label: Some("term-pipeline"),
        layout: Some(&pipeline_layout),
        vertex: wgpu::VertexState {
            module: &shader,
            entry_point: Some("vs"),
            compilation_options: Default::default(),
            buffers: &[
                // Unit-quad corners.
                wgpu::VertexBufferLayout {
                    array_stride: 8,
                    step_mode: wgpu::VertexStepMode::Vertex,
                    attributes: &wgpu::vertex_attr_array![0 => Float32x2],
                },
                // Per-instance data.
                wgpu::VertexBufferLayout {
                    array_stride: std::mem::size_of::<Instance>() as u64,
                    step_mode: wgpu::VertexStepMode::Instance,
                    attributes: &wgpu::vertex_attr_array![
                        1 => Float32x4, // rect
                        2 => Float32x4, // uv
                        3 => Float32x4, // color
                        4 => Uint32,    // mode
                        5 => Uint32,    // param (mode-3 decoration sub-style)
                    ],
                },
            ],
        },
        fragment: Some(wgpu::FragmentState {
            module: &shader,
            entry_point: Some("fs"),
            compilation_options: Default::default(),
            targets: &[Some(wgpu::ColorTargetState {
                format,
                blend: Some(wgpu::BlendState {
                    color: wgpu::BlendComponent {
                        src_factor: wgpu::BlendFactor::SrcAlpha,
                        dst_factor: wgpu::BlendFactor::OneMinusSrcAlpha,
                        operation: wgpu::BlendOperation::Add,
                    },
                    alpha: wgpu::BlendComponent {
                        src_factor: wgpu::BlendFactor::One,
                        dst_factor: wgpu::BlendFactor::OneMinusSrcAlpha,
                        operation: wgpu::BlendOperation::Add,
                    },
                }),
                write_mask: wgpu::ColorWrites::ALL,
            })],
        }),
        primitive: wgpu::PrimitiveState::default(),
        depth_stencil: None,
        multisample: wgpu::MultisampleState::default(),
        multiview_mask: None,
        cache: None,
    });

    let corners = device.create_buffer_init(&wgpu::util::BufferInitDescriptor {
        label: Some("term-corners"),
        contents: bytemuck::cast_slice(&QUAD_CORNERS),
        usage: wgpu::BufferUsages::VERTEX,
    });
    let indices = device.create_buffer_init(&wgpu::util::BufferInitDescriptor {
        label: Some("term-indices"),
        contents: bytemuck::cast_slice(&QUAD_INDICES),
        usage: wgpu::BufferUsages::INDEX,
    });
    let instances = device.create_buffer(&wgpu::BufferDescriptor {
        label: Some("term-instances"),
        size: INITIAL_INSTANCES * std::mem::size_of::<Instance>() as u64,
        usage: wgpu::BufferUsages::VERTEX | wgpu::BufferUsages::COPY_DST,
        mapped_at_creation: false,
    });

    GpuResources {
        pipeline,
        bind_group,
        uniform,
        corners,
        indices,
        instances,
        capacity: INITIAL_INSTANCES,
        atlas,
        is_srgb: format.is_srgb(),
        text_gamma,
        num_instances: 0,
        pane_ranges: Vec::new(),
        scratch_out: Vec::new(),
        scratch_glyphs: Vec::new(),
        scratch_cursors: Vec::new(),
        scratch_runs: Vec::new(),
        scratch_shaped: Vec::new(),
    }
}

/// Re-rasterize the glyph atlas at a new pixel font size and return the new
/// monospace cell size in physical pixels. The atlas texture (and thus the
/// pipeline's bind group) is reused; only cached glyphs and cell metrics change.
pub fn resize_font(render_state: &egui_wgpu::RenderState, px: f32) -> (f32, f32) {
    let mut renderer = render_state.renderer.write();
    let res: &mut GpuResources = renderer
        .callback_resources
        .get_mut()
        .expect("GpuResources missing");
    res.atlas.set_px(px);
    (res.atlas.cell_w, res.atlas.cell_h)
}

fn srgb_to_linear(c: u8) -> f32 {
    let s = c as f32 / 255.0;
    if s <= 0.04045 {
        s / 12.92
    } else {
        ((s + 0.055) / 1.055).powf(2.4)
    }
}

impl GpuResources {
    fn color(&self, c: Rgb, alpha: f32) -> [f32; 4] {
        if self.is_srgb {
            [
                srgb_to_linear(c.r),
                srgb_to_linear(c.g),
                srgb_to_linear(c.b),
                alpha,
            ]
        } else {
            [
                c.r as f32 / 255.0,
                c.g as f32 / 255.0,
                c.b as f32 / 255.0,
                alpha,
            ]
        }
    }

    /// Monospace cell size (physical px) the atlas computed for the current font
    /// size. Same value [`init`] returns.
    pub fn cell_size(&self) -> (f32, f32) {
        (self.atlas.cell_w, self.atlas.cell_h)
    }

    /// Assemble the instance list for `frame` — shaping, glyph rasterization +
    /// atlas upload (via `queue`), and instance build into the reusable scratch
    /// buffer. This is the per-frame CPU/upload work the renderer does before the
    /// draw; returns the instance count produced. Exposed for the headless render
    /// bench (the real frame path runs this from `CallbackTrait::prepare`).
    pub fn build_frame_instances(&mut self, frame: &TermFrame, queue: &wgpu::Queue) -> u32 {
        self.build_instances(frame, queue);
        self.scratch_out.len() as u32
    }

    /// Drop all cached glyphs / shaped runs so the next `build_frame_instances`
    /// re-rasterizes from cold — lets a bench measure rasterization cost rather
    /// than the warm-atlas steady state.
    pub fn reset_atlas_cache(&mut self) {
        self.atlas.reset_cache();
    }

    /// Build the instance list for one frame into `self.scratch_out`, grouped so
    /// each pane's instances occupy a contiguous range (recorded in
    /// `self.pane_ranges` with its clip rect) for per-pane scissoring in `paint`.
    fn build_instances(&mut self, frame: &TermFrame, queue: &wgpu::Queue) {
        let cw = self.atlas.cell_w;
        let ch = self.atlas.cell_h;
        let ascent = self.atlas.ascent;
        let line_h = (ch * 0.07).max(1.0);

        // Move the reusable scratch out of `self` (so the atlas can be borrowed
        // alongside) and clear it; capacity carries over from previous frames.
        // `runs` keeps its `GlyphRun` slots (and their string/Vec buffers) across
        // frames too — we track how many are live per row with `run_count`.
        let mut out = std::mem::take(&mut self.scratch_out);
        let mut glyphs = std::mem::take(&mut self.scratch_glyphs);
        let mut cursors = std::mem::take(&mut self.scratch_cursors);
        let mut runs = std::mem::take(&mut self.scratch_runs);
        let mut shaped = std::mem::take(&mut self.scratch_shaped);
        let mut ranges = std::mem::take(&mut self.pane_ranges);
        out.clear();
        ranges.clear();

        for pane in &frame.panes {
            // Per pane, emit background → glyphs/decorations → non-block cursor,
            // contiguously, so the pane owns one instance range we can scissor.
            let pane_start = out.len() as u32;
            glyphs.clear();
            cursors.clear();

            let snap = &pane.snapshot;
            let [ox, oy] = pane.origin_px;
            // Sub-line smooth-scroll shift (device px, whole-pixel). When > 0 the
            // row above the viewport (`over_row`) is drawn at virtual row -1 and
            // the bottom row overhangs; both are trimmed by the pane scissor.
            let shift = pane.scroll_offset_px;
            let over = &snap.over_row;
            let has_over = shift > 0.0 && !over.is_empty();
            let start_y: i32 = if has_over { -1 } else { 0 };
            let cell_at = |x: u16, y: i32| {
                if y < 0 {
                    over.get(x as usize)
                } else {
                    snap.cell(x, y as u16)
                }
            };

            // A filled block inverts the cell under it; a hollow block (set by
            // DECSCUSR, or applied when the pane/window is unfocused) draws an
            // outline and leaves the glyph's normal colors. Blink-off hides the
            // cursor without touching the snapshot.
            let cursor_visible = snap.cursor_visible && !pane.cursor_blink_hidden;
            let filled_block =
                cursor_visible && snap.cursor_shape == CursorShape::Block && !pane.cursor_hollow;
            // An unfocused pane always shows a hollow *block*, whatever the
            // requested shape (Ghostty's cursor.zig returns block_hollow for any
            // unfocused cursor before consulting the terminal style) — so a bar
            // or underline cursor must not fall through to its filled form here.
            let hollow_block = cursor_visible
                && (pane.cursor_hollow || snap.cursor_shape == CursorShape::HollowBlock);

            // Per-cell search-highlight mask (0 = none, 1 = match, 2 = current),
            // built only when a search is active so the common path pays nothing.
            let search_mask = build_search_mask(&pane.search_highlights, snap.cols, snap.rows);

            for y in start_y..snap.rows as i32 {
                for x in 0..snap.cols {
                    let Some(cell) = cell_at(x, y) else {
                        continue;
                    };
                    let cell_left = ox + x as f32 * cw;
                    let cell_top = oy + y as f32 * ch + shift;

                    // The cursor and selection live in the base grid (y >= 0);
                    // the over-row (y == -1) is plain history.
                    let (is_cursor_cell, selected) = if y >= 0 {
                        let yu = y as u16;
                        let icc = filled_block && x == snap.cursor_x && yu == snap.cursor_y;
                        let lin = yu as usize * snap.cols as usize + x as usize;
                        let sel = pane.selection.is_some_and(|(a, b)| lin >= a && lin <= b);
                        (icc, sel)
                    } else {
                        (false, false)
                    };
                    let (fg, mut bg) = if is_cursor_cell {
                        (cell.bg, snap.cursor_color)
                    } else {
                        (cell.fg, cell.bg)
                    };
                    if selected && !is_cursor_cell {
                        bg = frame.selection_bg;
                    }
                    // Search highlights tint the cell background (over a selection,
                    // since you're actively navigating matches); the cursor cell
                    // keeps its cursor color.
                    if !is_cursor_cell && y >= 0 && !search_mask.is_empty() {
                        let idx = y as usize * snap.cols as usize + x as usize;
                        match search_mask.get(idx).copied().unwrap_or(0) {
                            2 => bg = SEARCH_CURRENT_BG,
                            1 => bg = SEARCH_MATCH_BG,
                            _ => {}
                        }
                    }

                    out.push(Instance::solid(
                        [cell_left, cell_top, cw, ch],
                        self.color(bg, 1.0),
                    ));

                    // Decorations (drawn in the glyph pass so they layer over the
                    // background but under nothing). Faint dims them like glyphs;
                    // a blink-off cell hides them with its glyph.
                    let deco_alpha = if cell.faint { FAINT_ALPHA } else { 1.0 };
                    let deco_hidden = cell.blink && pane.blink_hidden;
                    if !deco_hidden && cell.underline != UnderlineStyle::None {
                        // Top of the underline line, just below the baseline.
                        let uy = (cell_top + ascent + line_h).round();
                        let uc = self.color(cell.underline_color.unwrap_or(fg), deco_alpha);
                        match cell.underline {
                            UnderlineStyle::Single => {
                                glyphs.push(Instance::solid([cell_left, uy, cw, line_h], uc));
                            }
                            UnderlineStyle::Double => {
                                glyphs.push(Instance::solid([cell_left, uy, cw, line_h], uc));
                                glyphs.push(Instance::solid(
                                    [cell_left, uy + 2.0 * line_h, cw, line_h],
                                    uc,
                                ));
                            }
                            UnderlineStyle::Dotted => {
                                glyphs.push(Instance::deco(
                                    [cell_left, uy, cw, line_h],
                                    uc,
                                    DECO_DOTTED,
                                    (line_h * 3.0).max(3.0),
                                ));
                            }
                            UnderlineStyle::Dashed => {
                                glyphs.push(Instance::deco(
                                    [cell_left, uy, cw, line_h],
                                    uc,
                                    DECO_DASHED,
                                    (ch * 0.4).max(4.0),
                                ));
                            }
                            UnderlineStyle::Curly => {
                                // A tall band gives the thick wave vertical room,
                                // centered on the single-underline position; the
                                // pane scissor trims any spill past the cell.
                                let band = (line_h * 4.0).max(4.0);
                                let top = uy + line_h * 0.5 - band * 0.5;
                                glyphs.push(Instance::deco(
                                    [cell_left, top, cw, band],
                                    uc,
                                    DECO_CURLY,
                                    (ch * 0.5).max(6.0),
                                ));
                            }
                            UnderlineStyle::None => {}
                        }
                    }
                    if !deco_hidden && cell.overline {
                        let oy = cell_top.round();
                        glyphs.push(Instance::solid(
                            [cell_left, oy, cw, line_h],
                            self.color(fg, deco_alpha),
                        ));
                    }
                    if !deco_hidden && cell.strikethrough {
                        let y = (cell_top + ch * 0.5).round();
                        glyphs.push(Instance::solid(
                            [cell_left, y, cw, line_h],
                            self.color(fg, deco_alpha),
                        ));
                    }
                }
            }

            // Glyph pass: shape each row into runs of uniform color+style so the
            // font's ligatures apply, then place the shaped glyphs back on the
            // grid at the cell their cluster came from.
            for y in start_y..snap.rows as i32 {
                // Live runs for this row occupy runs[0..run_count]; slots beyond
                // are reused (their buffers retained) on the next row/frame.
                let mut run_count = 0usize;
                // Whether runs[run_count - 1] is still open for appending (a blank
                // cell breaks the run so the next non-blank starts a fresh one).
                let mut cur_open = false;
                let yu = if y >= 0 { Some(y as u16) } else { None };
                for x in 0..snap.cols {
                    let Some(cell) = cell_at(x, y) else { continue };
                    // A blank, concealed (invisible), or blink-off cell draws no
                    // glyph and breaks the run so the next visible cell starts a
                    // fresh one. The background/decorations were already emitted.
                    if cell.text.is_empty()
                        || cell.invisible
                        || (cell.blink && pane.blink_hidden)
                    {
                        cur_open = false;
                        continue;
                    }
                    let fg = if let Some(yu) = yu {
                        let is_cursor_cell =
                            filled_block && x == snap.cursor_x && yu == snap.cursor_y;
                        let lin = yu as usize * snap.cols as usize + x as usize;
                        let selected = pane.selection.is_some_and(|(a, b)| lin >= a && lin <= b);
                        if is_cursor_cell {
                            cell.bg
                        } else if selected {
                            frame.selection_fg.unwrap_or(cell.fg)
                        } else {
                            cell.fg
                        }
                    } else {
                        cell.fg
                    };
                    let style = Atlas::style_index(cell.bold, cell.italic);
                    let faint = cell.faint;
                    if cur_open {
                        let r = &mut runs[run_count - 1];
                        if r.fg == fg && r.style == style && r.faint == faint {
                            r.text.push_str(&cell.text);
                            r.byte_cell.resize(r.text.len(), x);
                            continue;
                        }
                    }
                    // Start a new run, reusing an existing slot's buffers if one is
                    // free, otherwise growing the pool.
                    if run_count == runs.len() {
                        runs.push(GlyphRun {
                            text: String::new(),
                            byte_cell: Vec::new(),
                            fg,
                            style,
                            faint,
                        });
                    }
                    let r = &mut runs[run_count];
                    r.text.clear();
                    r.byte_cell.clear();
                    r.fg = fg;
                    r.style = style;
                    r.faint = faint;
                    r.text.push_str(&cell.text);
                    r.byte_cell.resize(r.text.len(), x);
                    run_count += 1;
                    cur_open = true;
                }

                let cell_top = oy + y as f32 * ch + shift;
                for r in &runs[..run_count] {
                    shaped.clear();
                    self.atlas.shape_run(&r.text, r.style, &mut shaped);
                    let color = self.color(r.fg, if r.faint { FAINT_ALPHA } else { 1.0 });
                    for sg in &shaped {
                        // The cluster's leading char drives cell-fit classification
                        // and display width (1 or 2 cells), so icons/box-drawing/
                        // emoji are sized to their cell span.
                        let ch_first = r.text[sg.cluster as usize..].chars().next().unwrap_or(' ');
                        let constraint = atlas::classify(ch_first);
                        let span = UnicodeWidthChar::width(ch_first).unwrap_or(1).max(1) as u16;
                        // glyph id 0 (.notdef) means the primary font lacks this
                        // character; resolve it from the fallback chain (color
                        // emoji → mode 2, monochrome → mode 1).
                        let placed: Option<(_, u32)> = if sg.glyph_id != 0 {
                            self.atlas
                                .glyph(sg.glyph_id, r.style, constraint, span, queue)
                                .map(|g| (g, 1))
                        } else {
                            match self.atlas.glyph_fallback(ch_first, span, queue) {
                                Some(FallbackGlyph::Mono(g)) => Some((g, 1)),
                                Some(FallbackGlyph::Color(g)) => Some((g, 2)),
                                None => None,
                            }
                        };
                        let Some((g, mode)) = placed else {
                            continue;
                        };
                        let cell_x = r.byte_cell.get(sg.cluster as usize).copied().unwrap_or(0);
                        let cell_left = ox + cell_x as f32 * cw;
                        // A Fill glyph is a cell-sized coverage tile: draw it at the
                        // cell origin so neighbouring box/block cells tile seamlessly.
                        // Otherwise snap the glyph quad to whole physical pixels — the
                        // atlas bitmap is rasterized on the integer grid, so a
                        // whole-pixel destination keeps it 1:1 (crisp) instead of
                        // being resampled across pixel boundaries (blurry on scroll).
                        // `cell_top` already includes the whole-pixel scroll shift.
                        let rect = if g.fill {
                            [cell_left.round(), cell_top.round(), g.size[0], g.size[1]]
                        } else {
                            [
                                (cell_left + g.offset[0]).round(),
                                (cell_top + g.offset[1]).round(),
                                g.size[0],
                                g.size[1],
                            ]
                        };
                        glyphs.push(Instance {
                            rect,
                            uv: g.uv,
                            color,
                            mode,
                            param: 0,
                            _pad: [0; 2],
                        });
                    }
                }
            }

            let cur_left = ox + snap.cursor_x as f32 * cw;
            let cur_top = oy + snap.cursor_y as f32 * ch + shift;
            let cur_color = self.color(snap.cursor_color, 1.0);
            if hollow_block {
                // Four 1px edges forming an outline around the cursor cell.
                let t = 1.0_f32;
                cursors.push(Instance::solid([cur_left, cur_top, cw, t], cur_color));
                cursors.push(Instance::solid(
                    [cur_left, cur_top + ch - t, cw, t],
                    cur_color,
                ));
                cursors.push(Instance::solid([cur_left, cur_top, t, ch], cur_color));
                cursors.push(Instance::solid(
                    [cur_left + cw - t, cur_top, t, ch],
                    cur_color,
                ));
            } else if cursor_visible && !filled_block {
                let rect = match snap.cursor_shape {
                    CursorShape::Bar => [cur_left, cur_top, (cw * 0.12).max(1.0), ch],
                    _ => [cur_left, cur_top + ch - 2.0, cw, 2.0], // Underline
                };
                cursors.push(Instance::solid(rect, cur_color));
            }

            // Append this pane's glyphs then cursors after its backgrounds, and
            // record the contiguous range with its clip rect (the grid box).
            out.append(&mut glyphs);
            out.append(&mut cursors);
            let clip = [ox, oy, snap.cols as f32 * cw, snap.rows as f32 * ch];
            ranges.push((pane_start..out.len() as u32, clip));
        }

        // Return the scratch (now reusable, with retained capacity) to `self`.
        // `out` holds the assembled instance list for `prepare` to upload.
        self.scratch_out = out;
        self.scratch_glyphs = glyphs;
        self.scratch_cursors = cursors;
        self.scratch_runs = runs;
        self.scratch_shaped = shaped;
        self.pane_ranges = ranges;
    }
}

impl CallbackTrait for TermFrame {
    fn prepare(
        &self,
        device: &wgpu::Device,
        queue: &wgpu::Queue,
        screen_descriptor: &egui_wgpu::ScreenDescriptor,
        _encoder: &mut wgpu::CommandEncoder,
        resources: &mut egui_wgpu::CallbackResources,
    ) -> Vec<wgpu::CommandBuffer> {
        let res: &mut GpuResources = resources.get_mut().expect("GpuResources missing");

        let [sw, sh] = screen_descriptor.size_in_pixels;
        // Pass the gamma reciprocal so the shader applies `pow(cov, gamma_inv)`
        // with a single op; >1 text_gamma → exponent <1 → thicker AA coverage.
        let gamma_inv = 1.0 / res.text_gamma.max(0.1);
        queue.write_buffer(
            &res.uniform,
            0,
            bytemuck::cast_slice(&[sw as f32, sh as f32, gamma_inv, 0.0]),
        );

        res.build_instances(self, queue);
        let needed = res.scratch_out.len() as u64;
        res.num_instances = needed as u32;

        if needed > res.capacity {
            let new_cap = needed.next_power_of_two();
            res.instances = device.create_buffer(&wgpu::BufferDescriptor {
                label: Some("term-instances"),
                size: new_cap * std::mem::size_of::<Instance>() as u64,
                usage: wgpu::BufferUsages::VERTEX | wgpu::BufferUsages::COPY_DST,
                mapped_at_creation: false,
            });
            res.capacity = new_cap;
        }
        if !res.scratch_out.is_empty() {
            queue.write_buffer(&res.instances, 0, bytemuck::cast_slice(&res.scratch_out));
        }

        Vec::new()
    }

    fn paint(
        &self,
        info: eframe::egui::PaintCallbackInfo,
        render_pass: &mut wgpu::RenderPass<'static>,
        resources: &egui_wgpu::CallbackResources,
    ) {
        let res: &GpuResources = resources.get().expect("GpuResources missing");
        if res.num_instances == 0 {
            return;
        }
        render_pass.set_pipeline(&res.pipeline);
        render_pass.set_bind_group(0, &res.bind_group, &[]);
        render_pass.set_vertex_buffer(0, res.corners.slice(..));
        render_pass.set_vertex_buffer(1, res.instances.slice(..));
        render_pass.set_index_buffer(res.indices.slice(..), wgpu::IndexFormat::Uint16);

        // egui's own clip (already clamped to the framebuffer) bounds every
        // pane scissor, so intersections stay within the attachment.
        let egui_clip = info.clip_rect_in_pixels();
        let (ex0, ey0) = (egui_clip.left_px, egui_clip.top_px);
        let (ex1, ey1) = (ex0 + egui_clip.width_px, ey0 + egui_clip.height_px);

        for (range, clip) in &res.pane_ranges {
            // Intersect the pane's grid box with egui's clip; skip if empty.
            let px0 = clip[0].round() as i32;
            let py0 = clip[1].round() as i32;
            let px1 = (clip[0] + clip[2]).round() as i32;
            let py1 = (clip[1] + clip[3]).round() as i32;
            let x0 = px0.max(ex0);
            let y0 = py0.max(ey0);
            let x1 = px1.min(ex1);
            let y1 = py1.min(ey1);
            if x1 <= x0 || y1 <= y0 {
                continue;
            }
            render_pass.set_scissor_rect(x0 as u32, y0 as u32, (x1 - x0) as u32, (y1 - y0) as u32);
            render_pass.draw_indexed(0..QUAD_INDICES.len() as u32, 0, range.clone());
        }
    }
}

const SHADER: &str = r#"
struct U { screen: vec2<f32>, gamma_inv: f32, pad: f32 };
@group(0) @binding(0) var<uniform> u: U;
@group(0) @binding(1) var atlas_tex: texture_2d<f32>;
@group(0) @binding(2) var atlas_smp: sampler;
@group(0) @binding(3) var color_tex: texture_2d<f32>;

const TAU: f32 = 6.2831853;

struct VsOut {
  @builtin(position) pos: vec4<f32>,
  @location(0) uv: vec2<f32>,
  @location(1) color: vec4<f32>,
  @location(2) @interpolate(flat) mode: u32,
  @location(3) @interpolate(flat) param: u32,
};

@vertex
fn vs(@location(0) corner: vec2<f32>,
      @location(1) rect: vec4<f32>,
      @location(2) uvr: vec4<f32>,
      @location(3) color: vec4<f32>,
      @location(4) mode: u32,
      @location(5) param: u32) -> VsOut {
  let px = rect.xy + corner * rect.zw;
  let ndc = vec2<f32>(px.x / u.screen.x * 2.0 - 1.0, 1.0 - px.y / u.screen.y * 2.0);
  var out: VsOut;
  out.pos = vec4<f32>(ndc, 0.0, 1.0);
  out.uv = uvr.xy + corner * uvr.zw;
  out.color = color;
  out.mode = mode;
  out.param = param;
  return out;
}

@fragment
fn fs(in: VsOut) -> @location(0) vec4<f32> {
  if (in.mode == 0u) {
    return in.color;
  }
  if (in.mode == 2u) {
    // Color emoji: straight-alpha RGBA sampled from the color atlas.
    return textureSample(color_tex, atlas_smp, in.uv);
  }
  if (in.mode == 3u) {
    // Procedural underline decoration. uv.x is an absolute-pixel phase divided
    // by the pattern period (continuous across cells); uv.y spans 0..1.
    var cov = 0.0;
    if (in.param == 0u) {            // dotted: ~50% duty cycle
      cov = select(0.0, 1.0, fract(in.uv.x) < 0.5);
    } else if (in.param == 1u) {     // dashed: ~66% duty cycle
      cov = select(0.0, 1.0, fract(in.uv.x) < 0.66);
    } else {                         // curly: a thick sine wave within the band
      // `band` height is 4*line_h, so a half-thickness of 0.20 makes the stroke
      // ~1.6*line_h — noticeably heavier than a flat 1*line_h line, which a wavy
      // path otherwise reads thinner than. Amplitude 0.22 keeps peaks in-band.
      let center = 0.5 + 0.22 * sin(in.uv.x * TAU);
      cov = 1.0 - smoothstep(0.20, 0.26, abs(in.uv.y - center));
    }
    return vec4<f32>(in.color.rgb, in.color.a * cov);
  }
  // Coverage gamma thickens light-on-dark AA, which a linear-correct alpha
  // blend (sRGB framebuffer) otherwise renders too thin/spindly.
  let cov = textureSample(atlas_tex, atlas_smp, in.uv).r;
  return vec4<f32>(in.color.rgb, in.color.a * pow(cov, u.gamma_inv));
}
"#;

#[cfg(test)]
mod tests {
    use super::build_search_mask;
    use crate::search::SearchHighlight;

    #[test]
    fn search_mask_marks_match_and_current_spans() {
        let hl = vec![
            SearchHighlight { row: 0, col_start: 1, col_end: 3, current: false },
            SearchHighlight { row: 1, col_start: 0, col_end: 0, current: true },
        ];
        let m = build_search_mask(&hl, 4, 2);
        // Row 0 cols 1..=3 are plain matches (1); col 0 untouched.
        assert_eq!(m, vec![0, 1, 1, 1, 2, 0, 0, 0]);
    }

    #[test]
    fn search_mask_empty_when_no_highlights() {
        assert!(build_search_mask(&[], 10, 10).is_empty());
    }

    #[test]
    fn search_mask_clamps_out_of_range() {
        // col_end past the last column is clamped; an off-grid row is skipped.
        let hl = vec![
            SearchHighlight { row: 0, col_start: 2, col_end: 99, current: true },
            SearchHighlight { row: 5, col_start: 0, col_end: 1, current: false },
        ];
        let m = build_search_mask(&hl, 3, 1);
        assert_eq!(m, vec![0, 0, 2]);
    }
}
