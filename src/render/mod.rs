//! GPU renderer for the terminal grid: a single instanced-quad wgpu pipeline
//! that paints cell backgrounds, glyphs (sampled from the [`Atlas`]), and the
//! cursor, driven from a [`GridSnapshot`] inside an egui paint callback.

mod atlas;

use std::ops::Range;
use std::sync::Arc;

pub use atlas::FontSpec;
use atlas::{Atlas, FallbackGlyph, ShapedGlyph};
use eframe::egui_wgpu::{self, CallbackTrait};
use eframe::wgpu::{self, util::DeviceExt};
use unicode_width::UnicodeWidthChar;

use crate::engine::{CursorShape, GridSnapshot, ImagePlacement, Rgb, UnderlineStyle};

/// The embedded primary (regular) monospace font bytes. Exposed for benches that
/// measure shaping throughput against the real shaping path (rustybuzz over this
/// face) without needing a GPU device to build a full [`Atlas`].
pub fn regular_font() -> &'static [u8] {
    atlas::FONT_REGULAR
}

/// One instanced quad. `mode` 0 = solid fill (backgrounds/cursor/straight
/// underlines), 1 = glyph (alpha = atlas coverage), 2 = color emoji, 3 =
/// procedural decoration (dotted/dashed/curly underline, sub-style chosen by
/// `param`), 4 = `background-image`. Rect and uv are absolute pixels /
/// normalized UV; for mode 3, `uv.x` is an absolute-pixel phase (so the pattern
/// tiles seamlessly across cells) and `uv.y` spans 0..1 over the quad height.
#[repr(C)]
#[derive(Clone, Copy, bytemuck::Pod, bytemuck::Zeroable)]
struct Instance {
    rect: [f32; 4],
    uv: [f32; 4],
    color: [f32; 4],
    mode: u32,
    param: u32,
    /// Two spare per-mode floats. Previously dead padding kept only for the
    /// 16-byte stride; mode 4 uses `extra[0]` as `background-image-opacity`.
    extra: [f32; 2],
}

/// Procedural underline sub-styles for `Instance::mode == 3`, passed in `param`.
const DECO_DOTTED: u32 = 0;
const DECO_DASHED: u32 = 1;
const DECO_CURLY: u32 = 2;

/// Background tint for scrollback-search matches; the *current* (navigated) match
/// uses the brighter shade so it stands out among the others.

impl Instance {
    fn solid(rect: [f32; 4], color: [f32; 4]) -> Self {
        Self {
            rect,
            uv: [0.0; 4],
            color,
            mode: 0,
            param: 0,
            extra: [0.0; 2],
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
            extra: [0.0; 2],
        }
    }
}

const QUAD_CORNERS: [[f32; 2]; 4] = [[0.0, 0.0], [1.0, 0.0], [0.0, 1.0], [1.0, 1.0]];
const QUAD_INDICES: [u16; 6] = [0, 1, 2, 2, 1, 3];
const INITIAL_INSTANCES: u64 = 8192;

/// One uploaded kitty image.
struct CachedImage {
    _data: Arc<crate::engine::ImageData>,
    _tex: wgpu::Texture,
    group: wgpu::BindGroup,
    last_used: u64,
}

/// Evict a kitty image texture after this many `prepare` calls unused (a few
/// seconds at 60 fps per window).
const IMAGE_EVICT_AFTER: u64 = 240;

/// Persistent GPU resources, stored in egui's `callback_resources`.
pub struct GpuResources {
    pipeline: wgpu::RenderPipeline,
    bind_group: wgpu::BindGroup,
    /// Kept so the bind group can be rebuilt when the `background-image`
    /// texture is (re)uploaded — the only binding that changes after startup.
    bind_layout: wgpu::BindGroupLayout,
    sampler: wgpu::Sampler,
    uniform: wgpu::Buffer,
    corners: wgpu::Buffer,
    indices: wgpu::Buffer,
    instances: wgpu::Buffer,
    capacity: u64,
    atlas: Atlas,
    /// The `background-image` texture at binding 4, and the decoded image it was
    /// uploaded from. A 1×1 transparent placeholder until one is configured,
    /// because the bind group must always satisfy the layout. Holding the `Arc`
    /// is what makes `Arc::ptr_eq` a sound "is this the same image" test.
    bg_image: wgpu::Texture,
    bg_view: wgpu::TextureView,
    bg_source: Option<Arc<crate::bgimage::BgImage>>,
    /// Instance index of the full-area `background-image` quad, when there is
    /// one. Drawn before (and outside) the per-pane scissor loop.
    bg_instance: Option<u32>,
    /// The `custom-shader` post-process chain, once one has been built.
    shaders: Option<ShaderChain>,
    /// Identity of the shader list the chain was built from, so it is rebuilt
    /// only when the config actually changes. Stored as an address rather than
    /// a pointer because `GpuResources` must stay `Send + Sync` for egui's
    /// `callback_resources`; the chain keeps the `Arc` alive, which is what
    /// makes comparing addresses sound (same trick as `bg_source`).
    shader_key: Option<usize>,
    is_srgb: bool,
    /// `window-colorspace = display-p3`, latched from the frame in `prepare`.
    display_p3: bool,
    /// The swapchain format. The custom-shader chain's offscreen targets must
    /// match it, or the blit changes the colour space midway.
    target_format: wgpu::TextureFormat,
    /// Coverage gamma for text antialiasing (>1 thickens light-on-dark AA).
    /// Passed to the shader as its reciprocal each frame.
    text_gamma: f32,
    num_instances: u32,
    /// Per pane: its clip rect (device px `[x, y, w, h]`) and the span of
    /// [`Self::draws`] it owns. `paint` sets the scissor once per pane — needed
    /// because smooth scrolling overdraws a partial row past the pane's edges —
    /// then issues that pane's draws.
    pane_ranges: Vec<([f32; 4], Range<usize>)>,
    /// Flat draw list: an instance range plus the kitty image whose texture must
    /// be bound for it (`None` = ordinary text/background quads). Flat and
    /// retained so the per-frame cost is a `clear()` rather than N allocations.
    draws: Vec<(Range<u32>, Option<u32>)>,
    /// Reusable per-frame scratch buffers. Retained across frames (cleared, not
    /// reallocated) so building the instance list does no per-frame growth
    /// allocation. `scratch_out` also holds the assembled instance list that
    /// `prepare` uploads after `build_instances` returns.
    scratch_out: Vec<Instance>,
    /// Kitty images: one texture each, **never** in the glyph atlas (whose RGBA
    /// shelf flushes wholesale on overflow). Bound at group 1 per image draw.
    img_layout: wgpu::BindGroupLayout,
    /// Group 1 for every non-image draw: the layout must always be satisfied.
    img_placeholder: wgpu::BindGroup,
    /// Keyed by the `Arc<ImageData>` address. Sound because the entry *holds*
    /// that `Arc`, so the allocation cannot be freed and its address reused
    /// while the key exists (same argument as `bg_source`). An address rather
    /// than the kitty image id because ids are per terminal: two panes can both
    /// have an image 1 with different pixels.
    img_cache: std::collections::HashMap<usize, CachedImage>,
    /// Per-frame: cache key of each image slot, and slot of each key. The slot
    /// is what `draws` carries (`Option<u32>`).
    img_slots: Vec<usize>,
    img_slot_of: std::collections::HashMap<usize, u32>,
    /// `prepare` counter, for evicting textures not used recently. Not evicted
    /// on first absence: every window runs its own `prepare` against this one
    /// shared cache, so "absent from this frame" is normal for another
    /// window's images, and evicting on it would re-upload them every frame.
    img_tick: u64,
    scratch_glyphs: Vec<Instance>,
    scratch_cursors: Vec<Instance>,
    scratch_runs: Vec<GlyphRun>,
    scratch_shaped: Vec<ShapedGlyph>,
}

/// One pane (terminal grid) within a frame, positioned at `origin_px`.
pub struct PaneFrame {
    pub snapshot: Arc<GridSnapshot>,
    pub origin_px: [f32; 2],
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
    /// Scrollback-search highlight colors (`search-*`), for an ordinary match
    /// and for the current one. `TerminalColor` rather than `Rgb` because
    /// upstream lets these defer to the cell's own fg/bg.
    pub search_bg: crate::config::TerminalColor,
    pub search_fg: crate::config::TerminalColor,
    pub search_selected_bg: crate::config::TerminalColor,
    pub search_selected_fg: crate::config::TerminalColor,
    /// `cursor-text`: glyph color under a block cursor; `None` = the cell's bg.
    pub cursor_text: Option<crate::config::TerminalColor>,
    /// `background-opacity`. Cells on the *default* background emit no quad, so
    /// the translucent window fill painted behind the grid is what actually
    /// carries this; the renderer needs the value only for
    /// [`Self::background_opacity_cells`]. See [`bg_alpha`].
    pub background_opacity: f32,
    /// `background-opacity-cells`: extend the opacity to explicitly-colored cells.
    pub background_opacity_cells: bool,
    /// `font-shaping-break = cursor`: shape the cursor cell as its own run, so a
    /// ligature never hides the character being edited.
    pub shaping_break_cursor: bool,
    /// `faint-opacity`: alpha for faint/dim (SGR 2) glyphs and decorations.
    pub faint_opacity: f32,
    /// `cursor-opacity`: alpha for a *focused* pane's cursor. An unfocused pane's
    /// hollow cursor is always opaque, matching Ghostty.
    pub cursor_opacity: f32,
    /// The window background color — what a cell on the *default* background
    /// shows.
    pub background_color: Rgb,
    /// `window-colorspace = display-p3`: terminal colours are P3 and are
    /// mapped into the sRGB swapchain (`colorspace.rs`).
    pub display_p3: bool,
    /// The terminal area in device pixels (`[x, y, w, h]`) — everything below
    /// the tab strip. Both the window fill and `background-image` cover exactly
    /// this.
    pub area_px: [f32; 4],
    /// Paint the window background fill **in the renderer** rather than letting
    /// the app's `rect_filled` do it.
    ///
    /// Cells on the default background emit no quad at all (see [`bg_alpha`]),
    /// so *something* has to put the background there. Normally that is an egui
    /// rect painted before this callback — but a custom shader reads its input
    /// from an offscreen texture that egui never touches, so with shaders active
    /// the fill has to come from here or the shader filters a transparent
    /// screen. `bg_image` supersedes this: it paints the colour itself.
    pub window_fill: bool,
    /// `background-image`, when one is configured and decoded. `Some` **moves
    /// the window-background fill into the renderer**: the app skips its
    /// `rect_filled` and this quad paints the color *and* the image, at
    /// `background_opacity`. Two layers would double-composite (see `bg_alpha`).
    pub bg_image: Option<BgImageFrame>,
    /// `custom-shader`, already translated to WGSL, in the order they run.
    /// Empty is the ordinary path: no offscreen pass at all.
    pub custom_shaders: Arc<Vec<CustomShader>>,
    /// Uniforms handed to every custom shader this frame.
    pub shader_globals: crate::shader::Globals,
}

/// One compiled custom shader.
pub struct CustomShader {
    /// Where it came from, for error messages.
    pub name: String,
    /// WGSL translated from the user's GLSL by [`crate::shader::compile`].
    pub wgsl: String,
}

/// The `background-image` state for one frame.
pub struct BgImageFrame {
    /// The decoded image. The renderer re-uploads its texture only when this
    /// `Arc` is not the one it already holds — and it *keeps* that one alive, so
    /// the comparison is sound: a live allocation's address can't be reused by
    /// the replacement. The app hands out one `Arc` per path across all windows
    /// (they share a single wgpu device, and so a single texture).
    pub source: Arc<crate::bgimage::BgImage>,
    pub fit: crate::config::BackgroundImageFit,
    pub position: crate::config::BackgroundImagePosition,
    pub repeat: bool,
    /// `background-image-opacity`, which may exceed 1.0.
    pub opacity: f32,
}

/// Where a kitty image sits relative to the text and cell backgrounds.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ImageLayer {
    /// Below even the cell backgrounds.
    BelowBg,
    /// Between the cell backgrounds and the text.
    BelowText,
    /// Over everything, including the cursor.
    AboveText,
}

/// Ghostty's kitty z-layer split: `z < i32::MIN/2` draws below the cell
/// backgrounds, any other negative `z` below the text, and `z >= 0` above it.
pub(crate) fn image_layer(z: i32) -> ImageLayer {
    const BG_LIMIT: i32 = i32::MIN / 2; // -1_073_741_824
    if z < BG_LIMIT {
        ImageLayer::BelowBg
    } else if z < 0 {
        ImageLayer::BelowText
    } else {
        ImageLayer::AboveText
    }
}

/// Destination rect (device px) for a placement.
///
/// `row` is viewport-relative and may be negative when the placement's origin
/// has scrolled above the viewport top. `shift` is the pane's sub-line
/// smooth-scroll offset — the **same** term the cell loop adds — so an image and
/// the text around it move as one unit through a scroll rather than the image
/// lagging by a fraction of a line. Nothing is clamped here: the pane scissor
/// does the cropping, which keeps the source mapping correct.
pub(crate) fn image_rect(
    origin: [f32; 2],
    cell: (f32, f32),
    p: &ImagePlacement,
    shift: f32,
) -> [f32; 4] {
    [
        origin[0] + p.col as f32 * cell.0 + p.x_offset as f32,
        origin[1] + p.row as f32 * cell.1 + shift + p.y_offset as f32,
        p.dest_w as f32,
        p.dest_h as f32,
    ]
}

/// Source rect as normalized UV (`[u_origin, v_origin, u_extent, v_extent]`,
/// the convention the mode-agnostic vertex stage expects).
///
/// A zero source width/height means "the whole image", matching kitty. Crop and
/// scale need no shader work at all: the crop is this window, and the scale is
/// the mismatch between it and the destination rect, resolved by the sampler.
pub(crate) fn image_uv(p: &ImagePlacement) -> [f32; 4] {
    let (iw, ih) = (p.data.width.max(1) as f32, p.data.height.max(1) as f32);
    let w = if p.src_w == 0 { iw } else { p.src_w as f32 };
    let h = if p.src_h == 0 { ih } else { p.src_h as f32 };
    [p.src_x as f32 / iw, p.src_y as f32 / ih, w / iw, h / ih]
}

/// Whether a placement should be emitted this frame.
///
/// The engine's own `visible` flag is computed against the *engine's* viewport,
/// which during a smooth scroll sits one line above what's actually on screen.
/// While `shift > 0` the row above the viewport is partly visible (it's where
/// `over_row` is drawn), so a placement ending on that row must still be drawn
/// or it pops out for the length of the animation.
pub(crate) fn image_visible(row: i32, grid_rows: u32, rows: u16, over: bool) -> bool {
    let top = if over { -1 } else { 0 };
    let bottom = row + grid_rows.max(1) as i32;
    bottom > top && row < rows as i32
}

/// Split a pane's contiguous instance range into draw calls at image
/// boundaries. `images` is `(instance_index, image_id)` in ascending order.
///
/// Pure because this is the part that silently drops or double-draws instances
/// when it's wrong, and that is invisible in a screenshot. The invariant its
/// tests assert: the emitted ranges concatenate to exactly `pane`, with no gaps
/// and no overlaps.
pub(crate) fn split_draws(
    pane: Range<u32>,
    images: &[(u32, u32)],
    out: &mut Vec<(Range<u32>, Option<u32>)>,
) {
    let mut at = pane.start;
    for &(idx, id) in images {
        if idx < at || idx >= pane.end {
            continue;
        }
        if idx > at {
            out.push((at..idx, None));
        }
        out.push((idx..idx + 1, Some(id)));
        at = idx + 1;
    }
    if at < pane.end {
        out.push((at..pane.end, None));
    }
}

/// Background alpha for one cell, mirroring Ghostty's decision table
/// (`renderer/generic.zig`, the `bg_alpha` block). **A return of `0.0` means emit
/// no quad at all** — the translucent window background painted behind the grid
/// shows through, which is how `background-opacity` reaches the screen.
///
/// The branch order is load-bearing: `inverse` must be tested before
/// `opacity_cells`, or reverse-video text would go translucent; `selected` beats
/// everything, so a selection stays readable at any opacity. `selected` folds in
/// search highlights, which Ghostty treats the same way.
pub(crate) fn bg_alpha(
    selected: bool,
    inverse: bool,
    bg_explicit: bool,
    opacity: f32,
    opacity_cells: bool,
) -> f32 {
    if selected || inverse {
        return 1.0;
    }
    if bg_explicit {
        // `background-opacity-cells` extends the opacity to cells that set their
        // own background (Neovim/tmux repaint theirs, so they'd otherwise stay
        // opaque). Like Ghostty this composites over the already-translucent
        // window fill, so the effective result is 1-(1-opacity)².
        return if opacity_cells { opacity } else { 1.0 };
    }
    0.0
}

/// Whether a cell's text is a "covering" glyph — one that fills its cell
/// completely, so the background should be painted in the *foreground* color and
/// the cell reads as a single solid rectangle.
///
/// Exactly Ghostty's `renderer/cell.zig::isCovering`, which is **U+2588 FULL
/// BLOCK and nothing else** — deliberately narrow, and worth resisting the urge
/// to extend to `▉▊▋` or the half blocks, which do *not* cover their cell and
/// would then paint their empty part in the foreground color.
pub(crate) fn is_covering(text: &str) -> bool {
    text == "\u{2588}"
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
    // Once per process, like `init` itself: one device for every window.
    render_state.device.set_device_lost_callback(|reason, msg| {
        // `Destroyed` is our own teardown at exit, not a failure.
        if reason != wgpu::DeviceLostReason::Destroyed
            && let Ok(mut slot) = DEVICE_LOST.lock()
        {
            *slot = Some(device_lost_message(&format!("{reason:?}"), &msg));
        }
    });
    cell
}

/// Set by the device-lost callback: why the GPU device went away. Read by
/// the app, which can then say so instead of leaving a frozen or blank window.
static DEVICE_LOST: std::sync::Mutex<Option<String>> = std::sync::Mutex::new(None);

/// The device-lost message, if the GPU device has been lost.
pub fn device_lost() -> Option<String> {
    DEVICE_LOST.lock().ok().and_then(|s| s.clone())
}

/// The text shown when the device is lost: what happened, the driver's words,
/// and the only remedy (a lost wgpu device cannot be revived in place).
pub fn device_lost_message(reason: &str, msg: &str) -> String {
    let msg = msg.trim();
    let detail = if msg.is_empty() {
        String::new()
    } else {
        format!(": {msg}")
    };
    format!(
        "The GPU device was lost ({reason}){detail}.\n\nThis is usually a driver reset or \
         crash. Your shells are still running, but giest cannot draw them any more \
         - restart giest."
    )
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
            // Binding 4: the `background-image` texture. One per window, not a
            // shelf in either atlas — the RGBA atlas flushes its *entire* cache
            // on overflow, so a wallpaper-sized image there would evict every
            // cached emoji (the same rule kitty images follow).
            wgpu::BindGroupLayoutEntry {
                binding: 4,
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
    // A 1×1 fully transparent placeholder so the bind group satisfies the layout
    // before (and without) a configured `background-image`.
    let bg_image = device.create_texture(&wgpu::TextureDescriptor {
        label: Some("term-bg-image-placeholder"),
        size: wgpu::Extent3d {
            width: 1,
            height: 1,
            depth_or_array_layers: 1,
        },
        mip_level_count: 1,
        sample_count: 1,
        dimension: wgpu::TextureDimension::D2,
        format: bg_image_format(format.is_srgb()),
        usage: wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_DST,
        view_formats: &[],
    });
    let bg_view = bg_image.create_view(&Default::default());
    let bind_group = build_bind_group(device, &bind_layout, &uniform, &atlas, &sampler, &bg_view);

    // Group 1: the kitty image texture for an image draw (mode 5).
    let img_layout = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
        label: Some("term-kitty-image-layout"),
        entries: &[wgpu::BindGroupLayoutEntry {
            binding: 0,
            visibility: wgpu::ShaderStages::FRAGMENT,
            ty: wgpu::BindingType::Texture {
                sample_type: wgpu::TextureSampleType::Float { filterable: true },
                view_dimension: wgpu::TextureViewDimension::D2,
                multisampled: false,
            },
            count: None,
        }],
    });
    let img_placeholder = {
        let view = bg_image.create_view(&Default::default());
        device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("term-kitty-image-placeholder"),
            layout: &img_layout,
            entries: &[wgpu::BindGroupEntry {
                binding: 0,
                resource: wgpu::BindingResource::TextureView(&view),
            }],
        })
    };

    let pipeline_layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
        label: Some("term-pipeline-layout"),
        bind_group_layouts: &[Some(&bind_layout), Some(&img_layout)],
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
                        4 => Uint32,     // mode
                        5 => Uint32,     // param (mode-3 sub-style / mode-4 repeat)
                        6 => Float32x2,  // extra (mode-4 image opacity)
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
        bind_layout,
        sampler,
        uniform,
        corners,
        indices,
        instances,
        capacity: INITIAL_INSTANCES,
        atlas,
        bg_image,
        bg_view,
        bg_source: None,
        bg_instance: None,
        shaders: None,
        shader_key: None,
        is_srgb: format.is_srgb(),
        display_p3: false,
        target_format: format,
        text_gamma,
        num_instances: 0,
        pane_ranges: Vec::new(),
        draws: Vec::new(),
        scratch_out: Vec::new(),
        img_layout,
        img_placeholder,
        img_cache: Default::default(),
        img_slots: Vec::new(),
        img_slot_of: Default::default(),
        img_tick: 0,
        scratch_glyphs: Vec::new(),
        scratch_cursors: Vec::new(),
        scratch_runs: Vec::new(),
        scratch_shaped: Vec::new(),
    }
}

/// The `custom-shader` post-process chain.
///
/// The terminal is drawn into `tex[0]`, then each shader reads one texture and
/// writes the other, ping-pong; `paint` blits whichever ended up holding the
/// result. Two textures is the minimum that allows a shader to read its input
/// and write its output in the same pass, and is all any number of shaders needs.
struct ShaderChain {
    /// Kept alive so `shader_key`'s address comparison stays sound: a freed
    /// allocation could otherwise be reused by the next shader list and read as
    /// "unchanged". Never read — holding it *is* the point.
    #[allow(dead_code)]
    source: Arc<Vec<CustomShader>>,
    pipelines: Vec<wgpu::RenderPipeline>,
    /// Copies the final texture to the screen.
    blit: wgpu::RenderPipeline,
    layout: wgpu::BindGroupLayout,
    sampler: wgpu::Sampler,
    uniform: wgpu::Buffer,
    tex: [wgpu::Texture; 2],
    view: [wgpu::TextureView; 2],
    /// `bind[i]` samples `tex[i]`.
    bind: [wgpu::BindGroup; 2],
    size: (u32, u32),
    /// Which texture holds the finished image (`pipelines.len() % 2`).
    final_index: usize,
}

/// Vertex + fragment stages shared by the chain's plumbing.
///
/// The fullscreen triangle matches Ghostty's (`vid 0` at `(-1,-3)`, `1` at
/// `(-1,1)`, `2` at `(3,1)`) so a shader's `gl_FragCoord` sees the same
/// coordinates it would there.
///
/// **Nothing here flips Y**, and that is load-bearing rather than an omission:
/// in a fullscreen post-process the fragment writes to the pixel it is at, so
/// flipping the coordinate moves where the output lands relative to where the
/// input was read. Coordinate and texture orientation must agree; both are
/// Y-down, which is also where Ghostty lands. See `shader::SUFFIX` — a flip was
/// tried here first and measurably produced an upside-down screen.
const CHAIN_SHADER: &str = r#"
@group(0) @binding(0) var src: texture_2d<f32>;
@group(0) @binding(2) var smp: sampler;

@vertex
fn vs(@builtin(vertex_index) vid: u32) -> @builtin(position) vec4<f32> {
  let x = select(-1.0, 3.0, vid == 2u);
  let y = select(1.0, -3.0, vid == 0u);
  return vec4<f32>(x, y, 0.0, 1.0);
}

@fragment
fn blit(@builtin(position) pos: vec4<f32>) -> @location(0) vec4<f32> {
  let dim = vec2<f32>(textureDimensions(src));
  let uv = vec2<f32>(pos.x / dim.x, pos.y / dim.y);
  return textureSampleLevel(src, smp, uv, 0.0);
}
"#;

/// Allocate the chain's two ping-pong targets and the bind group that samples
/// each. `bind[i]` reads `tex[i]`, so a pass writing `tex[1]` binds `bind[0]`.
fn chain_targets(
    device: &wgpu::Device,
    format: wgpu::TextureFormat,
    layout: &wgpu::BindGroupLayout,
    sampler: &wgpu::Sampler,
    uniform: &wgpu::Buffer,
    size: (u32, u32),
) -> (
    [wgpu::Texture; 2],
    [wgpu::TextureView; 2],
    [wgpu::BindGroup; 2],
) {
    let make = || {
        device.create_texture(&wgpu::TextureDescriptor {
            label: Some("custom-shader-target"),
            size: wgpu::Extent3d {
                width: size.0.max(1),
                height: size.1.max(1),
                depth_or_array_layers: 1,
            },
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format,
            usage: wgpu::TextureUsages::RENDER_ATTACHMENT | wgpu::TextureUsages::TEXTURE_BINDING,
            view_formats: &[],
        })
    };
    let tex = [make(), make()];
    let view = [
        tex[0].create_view(&Default::default()),
        tex[1].create_view(&Default::default()),
    ];
    let bind = [0usize, 1].map(|i| {
        device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("custom-shader-bind"),
            layout,
            entries: &[
                wgpu::BindGroupEntry {
                    binding: 0,
                    resource: wgpu::BindingResource::TextureView(&view[i]),
                },
                wgpu::BindGroupEntry {
                    binding: 1,
                    resource: uniform.as_entire_binding(),
                },
                wgpu::BindGroupEntry {
                    binding: 2,
                    resource: wgpu::BindingResource::Sampler(sampler),
                },
            ],
        })
    });
    (tex, view, bind)
}

impl ShaderChain {
    fn resize(&mut self, device: &wgpu::Device, format: wgpu::TextureFormat, size: (u32, u32)) {
        let (tex, view, bind) = chain_targets(
            device,
            format,
            &self.layout,
            &self.sampler,
            &self.uniform,
            size,
        );
        self.tex = tex;
        self.view = view;
        self.bind = bind;
        self.size = size;
    }
}

/// Texture format for the `background-image`.
///
/// Chosen to match the render target's color space so the sampler does the
/// decode for us: on an sRGB target the pipeline blends in linear (every color
/// goes through [`srgb_to_linear`] first), and an `*UnormSrgb` texture returns
/// linear samples, so the image lands in the same space as the background color
/// it is mixed with. On a non-sRGB target both stay raw. Getting this wrong
/// isn't an error anywhere — the image just renders visibly too bright or too
/// dark next to the text.
fn bg_image_format(is_srgb: bool) -> wgpu::TextureFormat {
    if is_srgb {
        wgpu::TextureFormat::Rgba8UnormSrgb
    } else {
        wgpu::TextureFormat::Rgba8Unorm
    }
}

/// Build the one bind group, which every binding but the background image is
/// fixed for the process. Factored out because the image can be replaced at
/// runtime (config reload), and a bind group can't be mutated in place.
fn build_bind_group(
    device: &wgpu::Device,
    layout: &wgpu::BindGroupLayout,
    uniform: &wgpu::Buffer,
    atlas: &Atlas,
    sampler: &wgpu::Sampler,
    bg_view: &wgpu::TextureView,
) -> wgpu::BindGroup {
    device.create_bind_group(&wgpu::BindGroupDescriptor {
        label: Some("term-bind-group"),
        layout,
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
                resource: wgpu::BindingResource::Sampler(sampler),
            },
            wgpu::BindGroupEntry {
                binding: 3,
                resource: wgpu::BindingResource::TextureView(&atlas.color_view),
            },
            wgpu::BindGroupEntry {
                binding: 4,
                resource: wgpu::BindingResource::TextureView(bg_view),
            },
        ],
    })
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
        if self.display_p3 {
            if self.is_srgb {
                let [r, g, b] = crate::colorspace::p3_to_linear_srgb(c);
                return [r, g, b, alpha];
            }
            let s = crate::colorspace::p3_to_srgb(c);
            return [
                s.r as f32 / 255.0,
                s.g as f32 / 255.0,
                s.b as f32 / 255.0,
                alpha,
            ];
        }
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

    /// Upload any kitty image this frame references that has no texture yet,
    /// assign this frame's image slots, and evict long-unused textures. Called
    /// from `prepare` — the only hook with a device — before the instances are
    /// built, since those carry the slot numbers.
    fn sync_images(&mut self, device: &wgpu::Device, queue: &wgpu::Queue, frame: &TermFrame) {
        self.img_tick += 1;
        let tick = self.img_tick;
        self.img_slots.clear();
        self.img_slot_of.clear();
        let format = bg_image_format(self.is_srgb);
        for pane in &frame.panes {
            for p in &pane.snapshot.images {
                let key = Arc::as_ptr(&p.data) as usize;
                if self.img_slot_of.contains_key(&key) {
                    continue;
                }
                let d = &p.data;
                if d.width == 0 || d.height == 0 || d.rgba.len() < (d.width * d.height * 4) as usize
                {
                    continue;
                }
                let entry = self.img_cache.entry(key).or_insert_with(|| {
                    let size = wgpu::Extent3d {
                        width: d.width,
                        height: d.height,
                        depth_or_array_layers: 1,
                    };
                    let tex = device.create_texture(&wgpu::TextureDescriptor {
                        label: Some("term-kitty-image"),
                        size,
                        mip_level_count: 1,
                        sample_count: 1,
                        dimension: wgpu::TextureDimension::D2,
                        format,
                        usage: wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_DST,
                        view_formats: &[],
                    });
                    queue.write_texture(
                        tex.as_image_copy(),
                        &d.rgba[..(d.width * d.height * 4) as usize],
                        wgpu::TexelCopyBufferLayout {
                            offset: 0,
                            bytes_per_row: Some(d.width * 4),
                            rows_per_image: Some(d.height),
                        },
                        size,
                    );
                    let view = tex.create_view(&Default::default());
                    let group = device.create_bind_group(&wgpu::BindGroupDescriptor {
                        label: Some("term-kitty-image"),
                        layout: &self.img_layout,
                        entries: &[wgpu::BindGroupEntry {
                            binding: 0,
                            resource: wgpu::BindingResource::TextureView(&view),
                        }],
                    });
                    CachedImage {
                        _data: d.clone(),
                        _tex: tex,
                        group,
                        last_used: tick,
                    }
                });
                entry.last_used = tick;
                self.img_slot_of.insert(key, self.img_slots.len() as u32);
                self.img_slots.push(key);
            }
        }
        // Evict once nothing but this cache holds the pixels: the engine drops
        // its `Arc` when an image is deleted, re-transmitted or advances an
        // animation frame, and a snapshot still on screen in *another* window
        // keeps the count above one — so this neither re-uploads the other
        // window's images every frame nor lets animation frames pile up.
        // The age bound is a backstop for anything held elsewhere by mistake.
        self.img_cache.retain(|_, e| {
            e.last_used == tick
                || (Arc::strong_count(&e._data) > 1
                    && tick.saturating_sub(e.last_used) <= IMAGE_EVICT_AFTER)
        });
    }

    /// (Re)upload the `background-image` texture and rebuild the bind group.
    ///
    /// Called from `prepare` only when the image actually changes, so a
    /// configured image costs one upload at load and nothing per frame. Passing
    /// `None` drops back to the 1×1 placeholder, which is what frees the VRAM
    /// when a reload removes the image.
    fn set_bg_image(
        &mut self,
        device: &wgpu::Device,
        queue: &wgpu::Queue,
        img: Option<&Arc<crate::bgimage::BgImage>>,
    ) {
        let (w, h, data): (u32, u32, &[u8]) = match img {
            Some(f) => (f.width.max(1), f.height.max(1), &f.rgba),
            None => (1, 1, &[0, 0, 0, 0]),
        };
        // A short buffer would be a decoder bug, but a wgpu validation panic is
        // a poor way to learn that: keep the placeholder instead.
        if data.len() < (w as usize * h as usize * 4) {
            return;
        }
        let size = wgpu::Extent3d {
            width: w,
            height: h,
            depth_or_array_layers: 1,
        };
        self.bg_image = device.create_texture(&wgpu::TextureDescriptor {
            label: Some("term-bg-image"),
            size,
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format: bg_image_format(self.is_srgb),
            usage: wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_DST,
            view_formats: &[],
        });
        queue.write_texture(
            self.bg_image.as_image_copy(),
            &data[..w as usize * h as usize * 4],
            wgpu::TexelCopyBufferLayout {
                offset: 0,
                bytes_per_row: Some(w * 4),
                rows_per_image: Some(h),
            },
            size,
        );
        self.bg_view = self.bg_image.create_view(&Default::default());
        self.bind_group = build_bind_group(
            device,
            &self.bind_layout,
            &self.uniform,
            &self.atlas,
            &self.sampler,
            &self.bg_view,
        );
        self.bg_source = img.cloned();
    }

    /// Build (or rebuild, or tear down) the custom-shader chain.
    ///
    /// A shader that fails to *compile* never reaches here — `shader::compile`
    /// rejects it at load time and the app reports it — so by this point the
    /// WGSL is known good and the only work is GPU objects.
    fn set_shaders(
        &mut self,
        device: &wgpu::Device,
        format: wgpu::TextureFormat,
        shaders: &Arc<Vec<CustomShader>>,
        size: (u32, u32),
    ) {
        if shaders.is_empty() {
            self.shaders = None;
            self.shader_key = None;
            return;
        }
        let key = Arc::as_ptr(shaders) as usize;
        // Rebuild on a config change; resize in place otherwise.
        if self.shader_key == Some(key)
            && let Some(chain) = &mut self.shaders
        {
            if chain.size != size {
                chain.resize(device, format, size);
            }
            return;
        }

        let layout = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("custom-shader-layout"),
            entries: &[
                // Bindings 0/1/2 are fixed by `shader::PREFIX`'s
                // `layout(binding = N)` declarations — texture, uniforms, sampler.
                wgpu::BindGroupLayoutEntry {
                    binding: 0,
                    visibility: wgpu::ShaderStages::FRAGMENT,
                    ty: wgpu::BindingType::Texture {
                        sample_type: wgpu::TextureSampleType::Float { filterable: true },
                        view_dimension: wgpu::TextureViewDimension::D2,
                        multisampled: false,
                    },
                    count: None,
                },
                wgpu::BindGroupLayoutEntry {
                    binding: 1,
                    visibility: wgpu::ShaderStages::FRAGMENT,
                    ty: wgpu::BindingType::Buffer {
                        ty: wgpu::BufferBindingType::Uniform,
                        has_dynamic_offset: false,
                        min_binding_size: None,
                    },
                    count: None,
                },
                wgpu::BindGroupLayoutEntry {
                    binding: 2,
                    visibility: wgpu::ShaderStages::FRAGMENT,
                    ty: wgpu::BindingType::Sampler(wgpu::SamplerBindingType::Filtering),
                    count: None,
                },
            ],
        });
        let pipeline_layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: Some("custom-shader-pipeline-layout"),
            bind_group_layouts: &[Some(&layout)],
            immediate_size: 0,
        });

        let chain_module = device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("custom-shader-chain"),
            source: wgpu::ShaderSource::Wgsl(CHAIN_SHADER.into()),
        });

        // A post-process replaces its input rather than compositing over it, so
        // every chain pass writes with blending off. Only the final blit
        // composites, and it does so premultiplied — which is what rendering
        // onto a cleared (transparent) target with SrcAlpha blending produces.
        let make = |frag_module: &wgpu::ShaderModule, entry: &str, blend| {
            device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
                label: Some("custom-shader-pass"),
                layout: Some(&pipeline_layout),
                vertex: wgpu::VertexState {
                    module: &chain_module,
                    entry_point: Some("vs"),
                    compilation_options: Default::default(),
                    buffers: &[],
                },
                fragment: Some(wgpu::FragmentState {
                    module: frag_module,
                    entry_point: Some(entry),
                    compilation_options: Default::default(),
                    targets: &[Some(wgpu::ColorTargetState {
                        format,
                        blend,
                        write_mask: wgpu::ColorWrites::ALL,
                    })],
                }),
                primitive: wgpu::PrimitiveState::default(),
                depth_stencil: None,
                multisample: wgpu::MultisampleState::default(),
                multiview_mask: None,
                cache: None,
            })
        };

        let mut pipelines = Vec::with_capacity(shaders.len());
        for s in shaders.iter() {
            let module = device.create_shader_module(wgpu::ShaderModuleDescriptor {
                label: Some(&s.name),
                source: wgpu::ShaderSource::Wgsl(s.wgsl.as_str().into()),
            });
            pipelines.push(make(&module, "main", None));
        }
        let blit = make(
            &chain_module,
            "blit",
            Some(wgpu::BlendState::PREMULTIPLIED_ALPHA_BLENDING),
        );

        let sampler = device.create_sampler(&wgpu::SamplerDescriptor {
            label: Some("custom-shader-sampler"),
            mag_filter: wgpu::FilterMode::Linear,
            min_filter: wgpu::FilterMode::Linear,
            // Shadertoy shaders routinely sample outside 0..1 (the CRT curve
            // does); clamping keeps the edge pixel rather than wrapping the
            // image around, which is what a real CRT bezel looks like.
            address_mode_u: wgpu::AddressMode::ClampToEdge,
            address_mode_v: wgpu::AddressMode::ClampToEdge,
            ..Default::default()
        });
        let uniform = device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("custom-shader-globals"),
            size: crate::shader::GLOBALS_SIZE as u64,
            usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });

        let final_index = pipelines.len() % 2;
        let (tex, view, bind) = chain_targets(device, format, &layout, &sampler, &uniform, size);
        self.shaders = Some(ShaderChain {
            source: shaders.clone(),
            pipelines,
            blit,
            layout,
            sampler,
            uniform,
            tex,
            view,
            bind,
            size,
            final_index,
        });
        self.shader_key = Some(key);
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
        // Decoration geometry comes from the font (and the `adjust-*` keys), not
        // from a fraction of the cell — see `atlas::derive_metrics`.
        let met = self.atlas.metrics;
        let line_h = met.underline_thick;

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
        let mut draws = std::mem::take(&mut self.draws);
        out.clear();
        ranges.clear();
        draws.clear();
        // (instance index, image id) for this pane's image quads, in emission
        // order — `split_draws` turns it into the pane's draw list.
        let mut pane_images: Vec<(u32, u32)> = Vec::new();
        let img_slot_of = std::mem::take(&mut self.img_slot_of);

        // `background-image` first, so every pane draws over it. It is a single
        // quad covering the whole terminal area and it paints the *background
        // color* too (see `BgImageFrame`), which is why the app skips its own
        // window fill whenever this is present.
        let [ax, ay, aw, ah] = frame.area_px;
        let mut bg_instance = None;
        if let Some(bg) = frame.bg_image.as_ref() {
            let tex = (
                bg.source.width.max(1) as f32,
                bg.source.height.max(1) as f32,
            );
            let dest = crate::bgimage::dest_rect((aw, ah), tex, bg.fit, bg.position);
            bg_instance = Some(out.len() as u32);
            out.push(Instance {
                rect: [ax, ay, aw, ah],
                uv: crate::bgimage::uv((aw, ah), dest),
                color: self.color(frame.background_color, frame.background_opacity),
                mode: 4,
                param: u32::from(bg.repeat),
                extra: [bg.opacity, 0.0],
            });
        } else if frame.window_fill {
            // Same job as the image quad, minus the image: put the window
            // background where cells that emit nothing would otherwise leave a
            // hole. Only reached with custom shaders active — see `window_fill`.
            bg_instance = Some(out.len() as u32);
            out.push(Instance::solid(
                [ax, ay, aw, ah],
                self.color(frame.background_color, frame.background_opacity),
            ));
        }
        self.bg_instance = bg_instance;

        for pane in &frame.panes {
            // Per pane, emit background → glyphs/decorations → non-block cursor,
            // contiguously, so the pane owns one instance range we can scissor.
            let pane_start = out.len() as u32;
            glyphs.clear();
            cursors.clear();
            pane_images.clear();

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

            // Kitty images for one z-layer. There is no depth buffer, so **draw
            // order is z order** — emitting these at the right three points in
            // the pane's instance stream is the entire z implementation.
            // `snap.images` arrives sorted by (z, image_id), so each layer is a
            // contiguous run and the within-layer order is already right.
            let emit_images =
                |layer: ImageLayer, out: &mut Vec<Instance>, pane_images: &mut Vec<(u32, u32)>| {
                    for p in snap.images.iter().filter(|p| image_layer(p.z) == layer) {
                        if !image_visible(p.row, p.grid_rows, snap.rows, has_over) {
                            continue;
                        }
                        // The slot `sync_images` assigned this frame; none means
                        // the image had no usable pixels, so there is nothing to draw.
                        let Some(&slot) = img_slot_of.get(&(Arc::as_ptr(&p.data) as usize)) else {
                            continue;
                        };
                        pane_images.push((out.len() as u32, slot));
                        out.push(Instance {
                            rect: image_rect([ox, oy], (cw, ch), p, shift),
                            uv: image_uv(p),
                            color: [1.0, 1.0, 1.0, 1.0],
                            mode: 5,
                            param: 0,
                            extra: [0.0; 2],
                        });
                    }
                };

            // Below the cell backgrounds (kitty z < i32::MIN/2).
            emit_images(ImageLayer::BelowBg, &mut out, &mut pane_images);

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
            // `cursor-opacity` applies only to a focused pane's cursor; an
            // unfocused one stays fully opaque (Ghostty's cursor.zig does the
            // same). `cursor_hollow` is exactly "this pane isn't focused".
            let cursor_alpha = if pane.cursor_hollow {
                1.0
            } else {
                frame.cursor_opacity
            };

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
                        (icc, cell.selected)
                    } else {
                        (false, false)
                    };
                    let (fg, mut bg) = if is_cursor_cell {
                        let fg = frame
                            .cursor_text
                            .map_or(cell.bg, |t| t.resolve(cell.fg, cell.bg));
                        (fg, snap.cursor_color)
                    } else {
                        (cell.fg, cell.bg)
                    };
                    // Ghostty's `isCovering`: a cell whose glyph fills it entirely
                    // takes the *foreground* as its background, so the two agree
                    // and the cell reads as one solid rectangle (which is what
                    // makes padding extension work). Upstream applies it as
                    // `inverse != isCovering` on the style colors; here `cell.fg`
                    // already has `inverse` applied, and both sides of that XOR
                    // land on the same answer — `bg = cell.fg` — so the swap is
                    // written once. Not applied to a selected or cursor cell,
                    // matching upstream's non-selected-only arm: a selection must
                    // keep its own background.
                    if !is_cursor_cell && !selected && is_covering(&cell.text) {
                        bg = fg;
                    }
                    if selected && !is_cursor_cell {
                        bg = frame.selection_bg;
                    }
                    // Search highlights tint the cell background (over a selection,
                    // since you're actively navigating matches); the cursor cell
                    // keeps its cursor color.
                    let mut searched = false;
                    if !is_cursor_cell && y >= 0 && !search_mask.is_empty() {
                        let idx = y as usize * snap.cols as usize + x as usize;
                        match search_mask.get(idx).copied().unwrap_or(0) {
                            2 => {
                                bg = frame.search_selected_bg.resolve(cell.fg, cell.bg);
                                searched = true;
                            }
                            1 => {
                                bg = frame.search_bg.resolve(cell.fg, cell.bg);
                                searched = true;
                            }
                            _ => {}
                        }
                    }

                    // Under `background-opacity` most cells emit nothing at all so
                    // the translucent window fill shows through — see `bg_alpha`.
                    // A search highlight counts as "selected": it must stay legible.
                    let a = if is_cursor_cell {
                        cursor_alpha
                    } else {
                        bg_alpha(
                            selected || searched,
                            cell.inverse,
                            cell.bg_explicit,
                            frame.background_opacity,
                            frame.background_opacity_cells,
                        )
                    };
                    if a > 0.0 {
                        out.push(Instance::solid(
                            [cell_left, cell_top, cw, ch],
                            self.color(bg, a),
                        ));
                    }

                    // Decorations (drawn in the glyph pass so they layer over the
                    // background but under nothing). Faint dims them like glyphs;
                    // a blink-off cell hides them with its glyph.
                    let deco_alpha = if cell.faint { frame.faint_opacity } else { 1.0 };
                    let deco_hidden = cell.blink && pane.blink_hidden;
                    if !deco_hidden && cell.underline != UnderlineStyle::None {
                        // Top of the underline, from the top of the cell.
                        let uy = (cell_top + met.underline_pos).round();
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
                        let oy = (cell_top + met.overline_pos).round();
                        glyphs.push(Instance::solid(
                            [cell_left, oy, cw, met.overline_thick],
                            self.color(fg, deco_alpha),
                        ));
                    }
                    if !deco_hidden && cell.strikethrough {
                        let y = (cell_top + met.strikethrough_pos).round();
                        glyphs.push(Instance::solid(
                            [cell_left, y, cw, met.strikethrough_thick],
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
                    if cell.text.is_empty() || cell.invisible || (cell.blink && pane.blink_hidden) {
                        cur_open = false;
                        continue;
                    }
                    let fg = if let Some(yu) = yu {
                        let is_cursor_cell =
                            filled_block && x == snap.cursor_x && yu == snap.cursor_y;
                        let lin = yu as usize * snap.cols as usize + x as usize;
                        let selected = cell.selected;
                        // A search match overrides the text color too — the bg
                        // pass already recolored the cell, and leaving the glyph
                        // at its own fg is how a match ends up unreadable
                        // (yellow-on-yellow) with a themed foreground.
                        let searched = if search_mask.is_empty() {
                            0
                        } else {
                            search_mask.get(lin).copied().unwrap_or(0)
                        };
                        if is_cursor_cell {
                            cell.bg
                        } else if searched == 2 {
                            frame.search_selected_fg.resolve(cell.fg, cell.bg)
                        } else if searched == 1 {
                            frame.search_fg.resolve(cell.fg, cell.bg)
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
                    // `font-shaping-break = cursor` (upstream's default): the
                    // cursor cell is a run of its own, so `!=` under the cursor
                    // shows as `!` and `=`. Keyed on the snapshot's visibility,
                    // not the blink phase — otherwise the ligature would form
                    // and break twice a second.
                    let at_cursor = frame.shaping_break_cursor
                        && snap.cursor_visible
                        && yu == Some(snap.cursor_y)
                        && x == snap.cursor_x;
                    if at_cursor {
                        cur_open = false;
                    }
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
                    // The cursor's run closes behind it, so the next cell
                    // starts fresh too.
                    cur_open = !at_cursor;
                }

                let cell_top = oy + y as f32 * ch + shift;
                for r in &runs[..run_count] {
                    shaped.clear();
                    self.atlas.shape_run(&r.text, r.style, &mut shaped);
                    let color = self.color(r.fg, if r.faint { frame.faint_opacity } else { 1.0 });
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
                        // Box drawing / blocks / braille are drawn by giest from
                        // the cell metrics, and win over the font — the same
                        // precedence Ghostty's `CodepointResolver` gives its
                        // sprite face. A font's versions are drawn to its em box,
                        // so they leave seams between cells at any line spacing.
                        let placed: Option<(_, u32)> =
                            if let Some(g) = self.atlas.sprite_glyph(ch_first, queue) {
                                Some((g, 1))
                            } else if let Some(g) = self.atlas.mapped_glyph(ch_first, span, queue) {
                                // `font-codepoint-map` beats the primary font:
                                // forcing a face is the point of the option.
                                Some((g, 1))
                            } else if sg.glyph_id != 0 {
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
                            extra: [0.0; 2],
                        });
                    }
                }
            }

            let cur_left = ox + snap.cursor_x as f32 * cw;
            let cur_top = oy + snap.cursor_y as f32 * ch + shift;
            let cur_color = self.color(snap.cursor_color, cursor_alpha);
            // `adjust-cursor-height` shortens the cursor from the **top**, so it
            // stays sitting on the text baseline area rather than floating —
            // upstream places the cursor sprite by its bearing from the bottom of
            // the cell, which comes out the same way.
            let cur_h = met.cursor_height.min(ch);
            let cur_y = cur_top + ch - cur_h;
            let t = met.cursor_thick;
            if hollow_block {
                // Four edges forming an outline around the cursor cell.
                cursors.push(Instance::solid([cur_left, cur_y, cw, t], cur_color));
                cursors.push(Instance::solid(
                    [cur_left, cur_y + cur_h - t, cw, t],
                    cur_color,
                ));
                cursors.push(Instance::solid([cur_left, cur_y, t, cur_h], cur_color));
                cursors.push(Instance::solid(
                    [cur_left + cw - t, cur_y, t, cur_h],
                    cur_color,
                ));
            } else if cursor_visible && !filled_block {
                let rect = match snap.cursor_shape {
                    // A bar is `cursor-thickness` wide; an underline is that tall,
                    // sitting on the bottom of the cell.
                    CursorShape::Bar => [cur_left, cur_y, t, cur_h],
                    _ => [cur_left, cur_top + ch - t, cw, t],
                };
                cursors.push(Instance::solid(rect, cur_color));
            }

            // At this point `out` holds exactly this pane's cell backgrounds
            // (decorations and glyphs went to the `glyphs` scratch), so this is
            // where a kitty image with negative z belongs: over the backgrounds,
            // under the text.
            emit_images(ImageLayer::BelowText, &mut out, &mut pane_images);

            // Append this pane's glyphs then cursors after its backgrounds.
            out.append(&mut glyphs);
            out.append(&mut cursors);

            // Non-negative z draws over everything, the cursor included — which
            // is also where Ghostty puts it.
            emit_images(ImageLayer::AboveText, &mut out, &mut pane_images);

            // Record the pane's clip rect (the grid box) and split its
            // contiguous instance range into draws at the image boundaries.
            let clip = [ox, oy, snap.cols as f32 * cw, snap.rows as f32 * ch];
            let first_draw = draws.len();
            split_draws(pane_start..out.len() as u32, &pane_images, &mut draws);
            ranges.push((clip, first_draw..draws.len()));
        }

        // Return the scratch (now reusable, with retained capacity) to `self`.
        // `out` holds the assembled instance list for `prepare` to upload.
        self.scratch_out = out;
        self.scratch_glyphs = glyphs;
        self.scratch_cursors = cursors;
        self.scratch_runs = runs;
        self.scratch_shaped = shaped;
        self.pane_ranges = ranges;
        self.draws = draws;
        self.img_slot_of = img_slot_of;
    }
}

impl CallbackTrait for TermFrame {
    fn prepare(
        &self,
        device: &wgpu::Device,
        queue: &wgpu::Queue,
        screen_descriptor: &egui_wgpu::ScreenDescriptor,
        encoder: &mut wgpu::CommandEncoder,
        resources: &mut egui_wgpu::CallbackResources,
    ) -> Vec<wgpu::CommandBuffer> {
        let res: &mut GpuResources = resources.get_mut().expect("GpuResources missing");

        let [sw, sh] = screen_descriptor.size_in_pixels;

        // Build/resize the custom-shader chain before anything reads it. The
        // offscreen targets are the *full framebuffer*, which is the whole
        // reason every instance coordinate below stays valid unchanged.
        let format = res.target_format;
        res.set_shaders(device, format, &self.custom_shaders, (sw, sh));
        res.display_p3 = self.display_p3;

        // Pass the gamma reciprocal so the shader applies `pow(cov, gamma_inv)`
        // with a single op; >1 text_gamma → exponent <1 → thicker AA coverage.
        let gamma_inv = 1.0 / res.text_gamma.max(0.1);
        queue.write_buffer(
            &res.uniform,
            0,
            bytemuck::cast_slice(&[sw as f32, sh as f32, gamma_inv, 0.0]),
        );
        if let Some(chain) = &res.shaders {
            queue.write_buffer(&chain.uniform, 0, bytemuck::bytes_of(&self.shader_globals));
        }

        // The background-image texture is the one binding that changes at
        // runtime, and it can only be replaced here — `paint` gets the resources
        // immutably and has no device.
        let want = self.bg_image.as_ref().map(|f| &f.source);
        let same = match (&res.bg_source, want) {
            (Some(a), Some(b)) => Arc::ptr_eq(a, b),
            (None, None) => true,
            _ => false,
        };
        if !same {
            res.set_bg_image(device, queue, want);
        }

        res.sync_images(device, queue, self);
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

        // Draw the grid offscreen and run it through the shaders. Must come
        // after the instance upload above, and can only happen here: `paint`
        // has no encoder, and its render pass cannot be nested.
        res.run_shader_chain(encoder, [sw, sh]);

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
        let egui_clip = info.clip_rect_in_pixels();
        let clip = [
            egui_clip.left_px,
            egui_clip.top_px,
            egui_clip.left_px + egui_clip.width_px,
            egui_clip.top_px + egui_clip.height_px,
        ];

        // With custom shaders active the grid was already drawn — offscreen, in
        // `prepare` — and filtered through the shader chain; all that's left is
        // to put the result on screen.
        if res.blit(render_pass, clip) {
            return;
        }
        res.record_grid(render_pass, clip);
    }
}

impl GpuResources {
    /// Run the offscreen render + shader chain for this frame.
    ///
    /// Everything happens here rather than in `paint` because this is the only
    /// hook with a `CommandEncoder`: `paint` is handed a render pass that egui
    /// has already begun on the swapchain, and a pass cannot be nested.
    fn run_shader_chain(&self, encoder: &mut wgpu::CommandEncoder, screen: [u32; 2]) {
        let Some(chain) = &self.shaders else { return };
        let clip = [0, 0, screen[0] as i32, screen[1] as i32];

        // 1. The terminal, into tex[0]. Cleared to transparent so the parts of
        //    the framebuffer the grid doesn't cover (the tab strip) stay empty
        //    and composite away at the blit.
        {
            let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                label: Some("custom-shader-grid"),
                color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                    view: &chain.view[0],
                    depth_slice: None,
                    resolve_target: None,
                    ops: wgpu::Operations {
                        load: wgpu::LoadOp::Clear(wgpu::Color::TRANSPARENT),
                        store: wgpu::StoreOp::Store,
                    },
                })],
                depth_stencil_attachment: None,
                timestamp_writes: None,
                occlusion_query_set: None,
                multiview_mask: None,
            });
            self.record_grid(&mut pass, clip);
        }

        // 2. One pass per shader, ping-ponging. Pass i reads tex[i % 2] and
        //    writes the other, so after n passes the result is in
        //    tex[n % 2] — which is what `final_index` records.
        for (i, pipeline) in chain.pipelines.iter().enumerate() {
            let src = i % 2;
            let dst = 1 - src;
            let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                label: Some("custom-shader-pass"),
                color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                    view: &chain.view[dst],
                    depth_slice: None,
                    resolve_target: None,
                    ops: wgpu::Operations {
                        // The shader writes every pixel, so there is nothing to
                        // preserve — and clearing is cheaper than loading.
                        load: wgpu::LoadOp::Clear(wgpu::Color::TRANSPARENT),
                        store: wgpu::StoreOp::Store,
                    },
                })],
                depth_stencil_attachment: None,
                timestamp_writes: None,
                occlusion_query_set: None,
                multiview_mask: None,
            });
            pass.set_pipeline(pipeline);
            pass.set_bind_group(0, &chain.bind[src], &[]);
            pass.draw(0..3, 0..1);
        }
    }

    /// Put the chain's finished image on screen. Returns `false` when no chain
    /// is active, meaning the caller should draw the grid directly instead.
    fn blit(&self, pass: &mut wgpu::RenderPass<'_>, clip: [i32; 4]) -> bool {
        let Some(chain) = &self.shaders else {
            return false;
        };
        let [x0, y0, x1, y1] = clip;
        if x1 <= x0 || y1 <= y0 {
            return true;
        }
        pass.set_scissor_rect(
            x0.max(0) as u32,
            y0.max(0) as u32,
            (x1 - x0) as u32,
            (y1 - y0) as u32,
        );
        pass.set_pipeline(&chain.blit);
        pass.set_bind_group(0, &chain.bind[chain.final_index], &[]);
        pass.draw(0..3, 0..1);
        true
    }

    /// Record the whole terminal — background quad, then every pane — into
    /// `pass`, clipped to `clip` (`[x0, y0, x1, y1]`, device px).
    ///
    /// Factored out of `paint` because the offscreen pass needs the identical
    /// sequence: the offscreen target is the *full framebuffer*, precisely so
    /// that every instance coordinate stays valid with no remapping.
    fn record_grid(&self, pass: &mut wgpu::RenderPass<'_>, clip: [i32; 4]) {
        if self.num_instances == 0 {
            return;
        }
        pass.set_pipeline(&self.pipeline);
        pass.set_bind_group(0, &self.bind_group, &[]);
        pass.set_bind_group(1, &self.img_placeholder, &[]);
        pass.set_vertex_buffer(0, self.corners.slice(..));
        pass.set_vertex_buffer(1, self.instances.slice(..));
        pass.set_index_buffer(self.indices.slice(..), wgpu::IndexFormat::Uint16);

        let [ex0, ey0, ex1, ey1] = clip;
        let res = self;
        let render_pass = pass;

        // `background-image` spans the whole terminal area, so it must be drawn
        // *before* the per-pane scissor loop and bounded only by egui's clip —
        // a pane scissor is the grid box, which excludes the padding band and
        // the split gutters.
        if let Some(i) = res.bg_instance {
            render_pass.set_scissor_rect(
                ex0.max(0) as u32,
                ey0.max(0) as u32,
                (ex1 - ex0).max(0) as u32,
                (ey1 - ey0).max(0) as u32,
            );
            render_pass.draw_indexed(0..QUAD_INDICES.len() as u32, 0, i..i + 1);
        }

        for (clip, span) in &res.pane_ranges {
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
            // One scissor per pane, then that pane's draws. A kitty image is its
            // own single-instance draw so its texture can be bound for it alone;
            // everything else batches as before. The scissor is what crops an
            // image at the pane edge, for free and at pixel precision.
            for (range, image) in &res.draws[span.clone()] {
                let Some(slot) = image else {
                    render_pass.draw_indexed(0..QUAD_INDICES.len() as u32, 0, range.clone());
                    continue;
                };
                let group = res
                    .img_slots
                    .get(*slot as usize)
                    .and_then(|k| res.img_cache.get(k));
                if let Some(img) = group {
                    render_pass.set_bind_group(1, &img.group, &[]);
                    render_pass.draw_indexed(0..QUAD_INDICES.len() as u32, 0, range.clone());
                    render_pass.set_bind_group(1, &res.img_placeholder, &[]);
                }
            }
        }
    }
}

const SHADER: &str = r#"
struct U { screen: vec2<f32>, gamma_inv: f32, pad: f32 };
@group(0) @binding(0) var<uniform> u: U;
@group(0) @binding(1) var atlas_tex: texture_2d<f32>;
@group(0) @binding(2) var atlas_smp: sampler;
@group(0) @binding(3) var color_tex: texture_2d<f32>;
@group(0) @binding(4) var bg_image_tex: texture_2d<f32>;
@group(1) @binding(0) var kitty_tex: texture_2d<f32>;

const TAU: f32 = 6.2831853;

struct VsOut {
  @builtin(position) pos: vec4<f32>,
  @location(0) uv: vec2<f32>,
  @location(1) color: vec4<f32>,
  @location(2) @interpolate(flat) mode: u32,
  @location(3) @interpolate(flat) param: u32,
  @location(4) @interpolate(flat) extra: vec2<f32>,
};

@vertex
fn vs(@location(0) corner: vec2<f32>,
      @location(1) rect: vec4<f32>,
      @location(2) uvr: vec4<f32>,
      @location(3) color: vec4<f32>,
      @location(4) mode: u32,
      @location(5) param: u32,
      @location(6) extra: vec2<f32>) -> VsOut {
  let px = rect.xy + corner * rect.zw;
  let ndc = vec2<f32>(px.x / u.screen.x * 2.0 - 1.0, 1.0 - px.y / u.screen.y * 2.0);
  var out: VsOut;
  out.pos = vec4<f32>(ndc, 0.0, 1.0);
  out.uv = uvr.xy + corner * uvr.zw;
  out.color = color;
  out.mode = mode;
  out.param = param;
  out.extra = extra;
  return out;
}

@fragment
fn fs(in: VsOut) -> @location(0) vec4<f32> {
  if (in.mode == 0u) {
    return in.color;
  }
  if (in.mode == 2u) {
    // Color emoji: straight-alpha RGBA sampled from the color atlas, modulated
    // by the instance alpha so emoji dim with faint (SGR 2) like every other
    // glyph. The atlas is straight-alpha, so this multiply is correct as-is —
    // do not premultiply it, the pipeline blends with SrcAlpha.
    let c = textureSample(color_tex, atlas_smp, in.uv);
    return vec4<f32>(c.rgb, c.a * in.color.a);
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
  if (in.mode == 5u) {
    // Kitty graphics image: straight-alpha RGBA from its own texture (group 1),
    // uv = the placement's source crop in image-normalized units.
    let c = textureSample(kitty_tex, atlas_smp, in.uv);
    return vec4<f32>(c.rgb, c.a * in.color.a);
  }
  if (in.mode == 4u) {
    // `background-image`. uv is in image-normalized units across the whole
    // terminal area, so `repeat` is a plain fract() and "off the image" is a
    // 0..1 bounds test — no second sampler, no pixel-space coordinates.
    // `in.color` is the window background color at `background-opacity`;
    // `in.extra.x` is `background-image-opacity`.
    var uv = in.uv;
    var inside = 1.0;
    if (in.param == 1u) {
      // fract() is x - floor(x), so it wraps negatives the way Ghostty's
      // double fmod does.
      uv = fract(uv);
    } else if (uv.x < 0.0 || uv.x > 1.0 || uv.y < 0.0 || uv.y > 1.0) {
      inside = 0.0;
    }
    let img = textureSample(bg_image_tex, atlas_smp, uv);
    // Ghostty's bg_image_fragment, rearranged from premultiplied to straight
    // alpha (this pipeline blends with SrcAlpha, not One):
    //   premul.rgb = img.rgb*t + bg.rgb*max(0, 1-t),  premul.a = max(t, 1)
    // where t = img.a * opacity. Dividing by premul.a recovers the straight
    // color, so an `opacity` above 1 overexposes exactly as it does upstream
    // instead of just saturating.
    let t = img.a * inside * in.extra.x;
    let denom = max(t, 1.0);
    let rgb = (img.rgb * t + in.color.rgb * max(0.0, 1.0 - t)) / denom;
    return vec4<f32>(rgb, denom * in.color.a);
  }
  // Coverage gamma thickens light-on-dark AA, which a linear-correct alpha
  // blend (sRGB framebuffer) otherwise renders too thin/spindly.
  let cov = textureSample(atlas_tex, atlas_smp, in.uv).r;
  return vec4<f32>(in.color.rgb, in.color.a * pow(cov, u.gamma_inv));
}
"#;

#[cfg(test)]
mod tests {
    use super::{
        ImageLayer, bg_alpha, build_search_mask, image_layer, image_rect, image_uv, image_visible,
        is_covering, split_draws,
    };
    use crate::engine::ImagePlacement;
    use crate::search::SearchHighlight;

    fn placement(col: i32, row: i32) -> ImagePlacement {
        ImagePlacement {
            image_id: 1,
            placement_id: 1,
            data: std::sync::Arc::new(crate::engine::ImageData {
                width: 100,
                height: 50,
                rgba: vec![0; 100 * 50 * 4],
            }),
            col,
            row,
            grid_cols: 4,
            grid_rows: 2,
            x_offset: 0,
            y_offset: 0,
            dest_w: 40,
            dest_h: 40,
            src_x: 0,
            src_y: 0,
            src_w: 0,
            src_h: 0,
            z: 0,
            visible: true,
        }
    }

    #[test]
    fn image_layer_matches_ghostty_thresholds() {
        assert_eq!(image_layer(i32::MIN), ImageLayer::BelowBg);
        assert_eq!(image_layer(i32::MIN / 2 - 1), ImageLayer::BelowBg);
        // The boundary itself is *not* below the background.
        assert_eq!(image_layer(i32::MIN / 2), ImageLayer::BelowText);
        assert_eq!(image_layer(-1), ImageLayer::BelowText);
        assert_eq!(image_layer(0), ImageLayer::AboveText);
        assert_eq!(image_layer(i32::MAX), ImageLayer::AboveText);
    }

    #[test]
    fn image_rect_places_at_cell_plus_offset_and_scroll() {
        let mut p = placement(3, 2);
        let r = image_rect([100.0, 50.0], (10.0, 20.0), &p, 0.0);
        assert_eq!(r, [130.0, 90.0, 40.0, 40.0]);

        // Cell pixel offsets (kitty X=/Y=) shift within the origin cell.
        p.x_offset = 4;
        p.y_offset = 7;
        assert_eq!(
            image_rect([100.0, 50.0], (10.0, 20.0), &p, 0.0),
            [134.0, 97.0, 40.0, 40.0]
        );

        // The smooth-scroll shift moves images exactly like the text.
        assert_eq!(
            image_rect([100.0, 50.0], (10.0, 20.0), &p, 6.0),
            [134.0, 103.0, 40.0, 40.0]
        );

        // A negative row (origin scrolled above the viewport) is legal.
        let p = placement(0, -1);
        assert_eq!(
            image_rect([0.0, 0.0], (10.0, 20.0), &p, 0.0),
            [0.0, -20.0, 40.0, 40.0]
        );
    }

    #[test]
    fn image_uv_is_the_source_rect_normalized() {
        // A zero source size means "the whole image".
        let p = placement(0, 0);
        assert_eq!(image_uv(&p), [0.0, 0.0, 1.0, 1.0]);

        let mut p = placement(0, 0);
        p.src_x = 25;
        p.src_y = 10;
        p.src_w = 50;
        p.src_h = 25;
        assert_eq!(image_uv(&p), [0.25, 0.2, 0.5, 0.5]);
    }

    #[test]
    fn image_visible_allows_one_row_of_overscroll() {
        // Fully on screen.
        assert!(image_visible(0, 2, 24, false));
        // Fully above: the bottom edge is at row 0, i.e. off the top.
        assert!(!image_visible(-2, 2, 24, false));
        // …but while smooth-scrolling, that same row is partly on screen.
        assert!(image_visible(-2, 2, 24, true));
        // Fully below.
        assert!(!image_visible(24, 2, 24, false));
        assert!(image_visible(23, 2, 24, false));
    }

    #[test]
    fn split_draws_covers_the_whole_pane_range() {
        let check = |images: &[(u32, u32)]| {
            let mut out = Vec::new();
            split_draws(10..20, images, &mut out);
            // The emitted ranges must tile the pane exactly.
            let mut at = 10;
            for (r, _) in &out {
                assert_eq!(r.start, at, "gap or overlap in {out:?}");
                assert!(r.end > r.start);
                at = r.end;
            }
            assert_eq!(at, 20, "did not reach the end in {out:?}");
            out
        };

        // No images: one plain draw.
        assert_eq!(check(&[]), vec![(10..20, None)]);
        // One in the middle: text, image, text.
        assert_eq!(
            check(&[(14, 7)]),
            vec![(10..14, None), (14..15, Some(7)), (15..20, None)]
        );
        // At the very start and the very end — no empty leading/trailing range.
        assert_eq!(
            check(&[(10, 1), (19, 2)]),
            vec![(10..11, Some(1)), (11..19, None), (19..20, Some(2))]
        );
        // Adjacent images produce no zero-length range between them.
        assert_eq!(
            check(&[(12, 1), (13, 2)]),
            vec![
                (10..12, None),
                (12..13, Some(1)),
                (13..14, Some(2)),
                (14..20, None)
            ]
        );
        // Out-of-range entries are ignored rather than corrupting the tiling.
        assert_eq!(check(&[(3, 9), (99, 9)]), vec![(10..20, None)]);
    }

    #[test]
    fn device_lost_message_names_reason_detail_and_remedy() {
        let m = super::device_lost_message("Unknown", " DXGI_ERROR_DEVICE_REMOVED \n");
        assert!(m.starts_with("The GPU device was lost (Unknown): DXGI_ERROR_DEVICE_REMOVED."));
        assert!(m.contains("restart giest"));
        let m = super::device_lost_message("Unknown", "");
        assert!(m.starts_with("The GPU device was lost (Unknown)."), "{m}");
    }

    #[test]
    fn bg_alpha_matches_ghostty_table() {
        let o = 0.8;
        // (selected, inverse, bg_explicit) -> alpha, with opacity-cells off.
        for (sel, inv, expl, want) in [
            (true, false, false, 1.0), // selection is always opaque
            (true, false, true, 1.0),
            (false, true, false, 1.0), // reverse video is always opaque
            (false, true, true, 1.0),
            (false, false, true, 1.0),  // explicit bg is opaque by default
            (false, false, false, 0.0), // default bg draws nothing
        ] {
            assert_eq!(
                bg_alpha(sel, inv, expl, o, false),
                want,
                "{sel} {inv} {expl}"
            );
        }

        // With `background-opacity-cells`, only the explicit-bg branch changes.
        assert_eq!(bg_alpha(false, false, true, o, true), o);
        assert_eq!(bg_alpha(false, false, false, o, true), 0.0);
    }

    #[test]
    fn is_covering_is_the_full_block_and_nothing_else() {
        assert!(is_covering("\u{2588}"), "U+2588 FULL BLOCK covers its cell");
        // The partial blocks leave part of the cell empty, so painting their
        // background in the foreground colour would fill in the gap they exist to
        // show. Upstream lists only the full block, and so does this.
        for t in [
            "\u{2589}", "\u{258C}", "\u{2580}", "\u{2584}", "\u{2591}", "\u{2593}", "a", " ", "",
        ] {
            assert!(!is_covering(t), "{t:?}");
        }
    }

    #[test]
    fn bg_alpha_branch_order_is_ghostty_s() {
        // Both rules that force opacity must be tested *before* opacity-cells,
        // otherwise selected/reverse-video cells would go translucent.
        assert_eq!(
            bg_alpha(true, false, true, 0.3, true),
            1.0,
            "selected beats cells"
        );
        assert_eq!(
            bg_alpha(false, true, true, 0.3, true),
            1.0,
            "inverse beats cells"
        );
    }

    #[test]
    fn bg_alpha_is_opaque_or_absent_at_full_opacity() {
        // At opacity 1.0 every combination is either a fully opaque quad or no
        // quad over an opaque fill — so the default config renders exactly as it
        // did before transparency existed.
        for cells in [false, true] {
            for sel in [false, true] {
                for inv in [false, true] {
                    for expl in [false, true] {
                        let a = bg_alpha(sel, inv, expl, 1.0, cells);
                        assert!(a == 1.0 || a == 0.0, "{sel} {inv} {expl} {cells} -> {a}");
                        // Only a default-background cell may be skipped.
                        assert_eq!(a == 0.0, !sel && !inv && !expl);
                    }
                }
            }
        }
    }

    #[test]
    fn search_mask_marks_match_and_current_spans() {
        let hl = vec![
            SearchHighlight {
                row: 0,
                col_start: 1,
                col_end: 3,
                current: false,
            },
            SearchHighlight {
                row: 1,
                col_start: 0,
                col_end: 0,
                current: true,
            },
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
            SearchHighlight {
                row: 0,
                col_start: 2,
                col_end: 99,
                current: true,
            },
            SearchHighlight {
                row: 5,
                col_start: 0,
                col_end: 1,
                current: false,
            },
        ];
        let m = build_search_mask(&hl, 3, 1);
        assert_eq!(m, vec![0, 0, 2]);
    }
}

/// Resolve an installed font family (or a font file path) to its bytes and
/// face index — the lookup `font-family` uses. For UI fonts such as
/// `window-title-font-family`.
pub fn find_ui_font(family: &str) -> Option<(&'static [u8], u32)> {
    atlas::find_regular_font(family)
}
