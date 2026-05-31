//! GPU renderer for the terminal grid: a single instanced-quad wgpu pipeline
//! that paints cell backgrounds, glyphs (sampled from the [`Atlas`]), and the
//! cursor, driven from a [`GridSnapshot`] inside an egui paint callback.

mod atlas;

use atlas::{Atlas, FallbackGlyph, ShapedGlyph};
use eframe::egui_wgpu::{self, CallbackTrait};
use eframe::wgpu::{self, util::DeviceExt};

use crate::engine::{CursorShape, GridSnapshot, Rgb};

/// One instanced quad. `mode` 0 = solid fill (backgrounds/cursor), 1 = glyph
/// (alpha = atlas coverage). Rect and uv are absolute pixels / normalized UV.
#[repr(C)]
#[derive(Clone, Copy, bytemuck::Pod, bytemuck::Zeroable)]
struct Instance {
    rect: [f32; 4],
    uv: [f32; 4],
    color: [f32; 4],
    mode: u32,
    _pad: [u32; 3],
}

impl Instance {
    fn solid(rect: [f32; 4], color: [f32; 4]) -> Self {
        Self {
            rect,
            uv: [0.0; 4],
            color,
            mode: 0,
            _pad: [0; 3],
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
    num_instances: u32,
}

/// One pane (terminal grid) within a frame, positioned at `origin_px`.
pub struct PaneFrame {
    pub snapshot: GridSnapshot,
    pub origin_px: [f32; 2],
    /// Inclusive linear (row-major) cell range to highlight as selected.
    pub selection: Option<(usize, usize)>,
    /// Draw the block cursor as a hollow outline rather than a filled cell
    /// (Ghostty does this for unfocused panes / when the window loses focus).
    pub cursor_hollow: bool,
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

/// A maximal horizontal run of cells sharing fg color and style, gathered for
/// shaping. `byte_cell[i]` is the grid column the byte at offset `i` in `text`
/// came from, so a shaped glyph's cluster maps back to its origin cell.
struct GlyphRun {
    text: String,
    byte_cell: Vec<u16>,
    fg: Rgb,
    style: usize,
}

/// Build the renderer resources and register them with egui. Returns the
/// monospace cell size in physical pixels so the app can size the grid.
pub fn init(render_state: &egui_wgpu::RenderState, px: f32) -> (f32, f32) {
    let device = &render_state.device;
    let format = render_state.target_format;

    let atlas = Atlas::new(device, px, format.is_srgb());
    let (cell_w, cell_h) = (atlas.cell_w, atlas.cell_h);

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

    let resources = GpuResources {
        pipeline,
        bind_group,
        uniform,
        corners,
        indices,
        instances,
        capacity: INITIAL_INSTANCES,
        atlas,
        is_srgb: format.is_srgb(),
        num_instances: 0,
    };
    render_state
        .renderer
        .write()
        .callback_resources
        .insert(resources);

    (cell_w, cell_h)
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

    /// Build the instance list for one frame.
    fn build_instances(&mut self, frame: &TermFrame, queue: &wgpu::Queue) -> Vec<Instance> {
        let cw = self.atlas.cell_w;
        let ch = self.atlas.cell_h;
        let ascent = self.atlas.ascent;
        let line_h = (ch * 0.07).max(1.0);

        // Three passes (across all panes): backgrounds, then glyphs +
        // decorations, then non-block cursors on top.
        let mut out: Vec<Instance> = Vec::new();
        let mut glyphs: Vec<Instance> = Vec::new();
        let mut cursors: Vec<Instance> = Vec::new();
        // Reused per-row shaping scratch.
        let mut runs: Vec<GlyphRun> = Vec::new();
        let mut shaped: Vec<ShapedGlyph> = Vec::new();

        for pane in &frame.panes {
            let snap = &pane.snapshot;
            let [ox, oy] = pane.origin_px;
            // A filled block inverts the cell under it; a hollow block (set by
            // DECSCUSR, or applied when the pane/window is unfocused) draws an
            // outline and leaves the glyph's normal colors.
            let filled_block = snap.cursor_visible
                && snap.cursor_shape == CursorShape::Block
                && !pane.cursor_hollow;
            let hollow_block = snap.cursor_visible
                && (snap.cursor_shape == CursorShape::HollowBlock
                    || (snap.cursor_shape == CursorShape::Block && pane.cursor_hollow));

            for y in 0..snap.rows {
                for x in 0..snap.cols {
                    let Some(cell) = snap.cell(x, y) else {
                        continue;
                    };
                    let cell_left = ox + x as f32 * cw;
                    let cell_top = oy + y as f32 * ch;

                    let is_cursor_cell = filled_block && x == snap.cursor_x && y == snap.cursor_y;
                    let (fg, mut bg) = if is_cursor_cell {
                        (cell.bg, snap.cursor_color)
                    } else {
                        (cell.fg, cell.bg)
                    };

                    let lin = y as usize * snap.cols as usize + x as usize;
                    let selected = pane.selection.is_some_and(|(a, b)| lin >= a && lin <= b);
                    if selected && !is_cursor_cell {
                        bg = frame.selection_bg;
                    }

                    out.push(Instance::solid([cell_left, cell_top, cw, ch], self.color(bg, 1.0)));

                    if cell.underline {
                        let y = (cell_top + ascent + line_h).round();
                        glyphs.push(Instance::solid([cell_left, y, cw, line_h], self.color(fg, 1.0)));
                    }
                    if cell.strikethrough {
                        let y = (cell_top + ch * 0.5).round();
                        glyphs.push(Instance::solid([cell_left, y, cw, line_h], self.color(fg, 1.0)));
                    }
                }
            }

            // Glyph pass: shape each row into runs of uniform color+style so the
            // font's ligatures apply, then place the shaped glyphs back on the
            // grid at the cell their cluster came from.
            for y in 0..snap.rows {
                runs.clear();
                let mut cur: Option<GlyphRun> = None;
                for x in 0..snap.cols {
                    let Some(cell) = snap.cell(x, y) else { continue };
                    if cell.text.is_empty() {
                        if let Some(r) = cur.take() {
                            runs.push(r);
                        }
                        continue;
                    }
                    let is_cursor_cell =
                        filled_block && x == snap.cursor_x && y == snap.cursor_y;
                    let lin = y as usize * snap.cols as usize + x as usize;
                    let selected = pane.selection.is_some_and(|(a, b)| lin >= a && lin <= b);
                    let fg = if is_cursor_cell {
                        cell.bg
                    } else if selected {
                        frame.selection_fg.unwrap_or(cell.fg)
                    } else {
                        cell.fg
                    };
                    let style = Atlas::style_index(cell.bold, cell.italic);
                    match cur {
                        Some(ref mut r) if r.fg == fg && r.style == style => {
                            r.text.push_str(&cell.text);
                            r.byte_cell.resize(r.text.len(), x);
                        }
                        _ => {
                            if let Some(r) = cur.take() {
                                runs.push(r);
                            }
                            let mut r = GlyphRun {
                                text: String::new(),
                                byte_cell: Vec::new(),
                                fg,
                                style,
                            };
                            r.text.push_str(&cell.text);
                            r.byte_cell.resize(r.text.len(), x);
                            cur = Some(r);
                        }
                    }
                }
                if let Some(r) = cur.take() {
                    runs.push(r);
                }

                let cell_top = oy + y as f32 * ch;
                for r in &runs {
                    shaped.clear();
                    self.atlas.shape_run(&r.text, r.style, &mut shaped);
                    let color = self.color(r.fg, 1.0);
                    for sg in &shaped {
                        // glyph id 0 (.notdef) means the primary font lacks this
                        // character; resolve it from the fallback chain (color
                        // emoji → mode 2, monochrome → mode 1).
                        let placed: Option<(_, u32)> = if sg.glyph_id != 0 {
                            self.atlas.glyph(sg.glyph_id, r.style, queue).map(|g| (g, 1))
                        } else {
                            match r.text[sg.cluster as usize..]
                                .chars()
                                .next()
                                .and_then(|ch| self.atlas.glyph_fallback(ch, queue))
                            {
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
                        // Snap the glyph quad to whole physical pixels. The atlas
                        // bitmap is rasterized on the integer grid, so a whole-pixel
                        // destination keeps it 1:1 (crisp) instead of being resampled
                        // across pixel boundaries (blurry / shimmering on scroll).
                        glyphs.push(Instance {
                            rect: [
                                (cell_left + g.offset[0]).round(),
                                (cell_top + g.offset[1]).round(),
                                g.size[0],
                                g.size[1],
                            ],
                            uv: g.uv,
                            color,
                            mode,
                            _pad: [0; 3],
                        });
                    }
                }
            }

            let cur_left = ox + snap.cursor_x as f32 * cw;
            let cur_top = oy + snap.cursor_y as f32 * ch;
            let cur_color = self.color(snap.cursor_color, 1.0);
            if hollow_block {
                // Four 1px edges forming an outline around the cursor cell.
                let t = 1.0_f32;
                cursors.push(Instance::solid([cur_left, cur_top, cw, t], cur_color));
                cursors.push(Instance::solid([cur_left, cur_top + ch - t, cw, t], cur_color));
                cursors.push(Instance::solid([cur_left, cur_top, t, ch], cur_color));
                cursors.push(Instance::solid([cur_left + cw - t, cur_top, t, ch], cur_color));
            } else if snap.cursor_visible && !filled_block {
                let rect = match snap.cursor_shape {
                    CursorShape::Bar => [cur_left, cur_top, (cw * 0.12).max(1.0), ch],
                    _ => [cur_left, cur_top + ch - 2.0, cw, 2.0], // Underline
                };
                cursors.push(Instance::solid(rect, cur_color));
            }
        }

        out.append(&mut glyphs);
        out.append(&mut cursors);
        out
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
        queue.write_buffer(
            &res.uniform,
            0,
            bytemuck::cast_slice(&[sw as f32, sh as f32, 0.0, 0.0]),
        );

        let instances = res.build_instances(self, queue);
        res.num_instances = instances.len() as u32;

        let needed = instances.len() as u64;
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
        if !instances.is_empty() {
            queue.write_buffer(&res.instances, 0, bytemuck::cast_slice(&instances));
        }

        Vec::new()
    }

    fn paint(
        &self,
        _info: eframe::egui::PaintCallbackInfo,
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
        render_pass.draw_indexed(0..QUAD_INDICES.len() as u32, 0, 0..res.num_instances);
    }
}

const SHADER: &str = r#"
struct U { screen: vec2<f32>, pad: vec2<f32> };
@group(0) @binding(0) var<uniform> u: U;
@group(0) @binding(1) var atlas_tex: texture_2d<f32>;
@group(0) @binding(2) var atlas_smp: sampler;
@group(0) @binding(3) var color_tex: texture_2d<f32>;

struct VsOut {
  @builtin(position) pos: vec4<f32>,
  @location(0) uv: vec2<f32>,
  @location(1) color: vec4<f32>,
  @location(2) @interpolate(flat) mode: u32,
};

@vertex
fn vs(@location(0) corner: vec2<f32>,
      @location(1) rect: vec4<f32>,
      @location(2) uvr: vec4<f32>,
      @location(3) color: vec4<f32>,
      @location(4) mode: u32) -> VsOut {
  let px = rect.xy + corner * rect.zw;
  let ndc = vec2<f32>(px.x / u.screen.x * 2.0 - 1.0, 1.0 - px.y / u.screen.y * 2.0);
  var out: VsOut;
  out.pos = vec4<f32>(ndc, 0.0, 1.0);
  out.uv = uvr.xy + corner * uvr.zw;
  out.color = color;
  out.mode = mode;
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
  let cov = textureSample(atlas_tex, atlas_smp, in.uv).r;
  return vec4<f32>(in.color.rgb, in.color.a * cov);
}
"#;
