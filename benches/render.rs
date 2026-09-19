//! Renderer cost — the per-frame CPU + GPU-upload work the wgpu pipeline does
//! before the draw call: shaping each row, rasterizing glyphs into the atlas, and
//! assembling the instance list (`GpuResources::build_frame_instances`). This is
//! the giest-side counterpart to Ghostty's `ScreenClone --mode render`; Ghostty
//! has no public glyph-atlas bench, so this also covers giest-only ground.
//!
//! Unlike the other benches it needs a real `wgpu::Device`, so it spins up a
//! **headless** adapter (no window/surface). When no adapter is available (e.g. a
//! headless CI box with no GPU/software rasterizer), it prints a notice and skips
//! rather than failing — consistent with the GPU work being out of CI scope.
//!
//! Two groups:
//!   - `render_instances`: steady-state per-frame assembly with a **warm** atlas
//!     (the common case — glyphs already cached), across grid sizes and input
//!     classes. Throughput is in cells.
//!   - `render_raster_cold`: assembly with the glyph cache reset each iteration,
//!     so rasterization (the cache-miss path) dominates.
//!
//! Run: `cargo bench --bench render`.

use std::sync::Arc;

use criterion::{Criterion, Throughput, criterion_group, criterion_main};
use eframe::wgpu;
use giest::engine::{GhosttyVtEngine, GridSnapshot, Rgb, TerminalEngine};
use giest::render::{self, PaneFrame, TermFrame};
use giest::synthetic;

const FONT_PX: f32 = 16.0;
const TEXT_GAMMA: f32 = 1.0;
const TARGET_FORMAT: wgpu::TextureFormat = wgpu::TextureFormat::Rgba8UnormSrgb;

/// Acquire a headless wgpu device + queue, or `None` if no adapter is available.
fn headless_device() -> Option<(wgpu::Device, wgpu::Queue)> {
    let instance = wgpu::Instance::new(wgpu::InstanceDescriptor::new_without_display_handle());
    let adapter = pollster::block_on(instance.request_adapter(&wgpu::RequestAdapterOptions {
        power_preference: wgpu::PowerPreference::HighPerformance,
        force_fallback_adapter: false,
        compatible_surface: None,
    }))
    .ok()?;
    let (device, queue) = pollster::block_on(adapter.request_device(&wgpu::DeviceDescriptor {
        label: Some("giest-bench-device"),
        ..Default::default()
    }))
    .ok()?;
    Some((device, queue))
}

/// A representative ligature-heavy line, repeated to fill ~`cells` columns of
/// programming-operator runs (exercises contextual-alternate substitution).
fn ligature_stream(cells: usize) -> Vec<u8> {
    let unit = "=> != === >= <= |> <| -> <- ... :: && || ++ -- ==> <=> /* */ ";
    let mut s = String::new();
    while s.len() < cells {
        s.push_str(unit);
    }
    s.into_bytes()
}

/// Build a `GridSnapshot` for `cols`×`rows` pre-filled with ~2 screenfuls of
/// `kind` data, via the real engine (so cell contents/colors are realistic).
fn snapshot_for(cols: u16, rows: u16, kind: &str) -> GridSnapshot {
    let mut eng = GhosttyVtEngine::new(cols, rows, 2000).expect("engine");
    let bytes = cols as usize * rows as usize * 2;
    let data = match kind {
        "utf8" => synthetic::utf8(bytes, 1),
        "ligature" => ligature_stream(bytes),
        _ => synthetic::ascii(bytes, 1),
    };
    eng.write(&data);
    let mut snap = GridSnapshot::default();
    eng.snapshot(&mut snap).expect("snapshot");
    snap
}

/// One full-window pane over `snap`, with no selection/scroll offset.
fn term_frame(snap: GridSnapshot) -> TermFrame {
    TermFrame {
        panes: vec![PaneFrame {
            snapshot: Arc::new(snap),
            origin_px: [0.0, 0.0],
            cursor_hollow: false,
            cursor_blink_hidden: false,
            blink_hidden: false,
            scroll_offset_px: 0.0,
            search_highlights: Vec::new(),
        }],
        selection_bg: Rgb::new(40, 60, 90),
        selection_fg: None,
        search_bg: giest::config::TerminalColor::CellBackground,
        search_fg: giest::config::TerminalColor::CellForeground,
        search_selected_bg: giest::config::TerminalColor::CellBackground,
        search_selected_fg: giest::config::TerminalColor::CellForeground,
        cursor_text: None,
        // Fully opaque: the bench measures the default (no-transparency) path.
        background_opacity: 1.0,
        background_opacity_cells: false,
        faint_opacity: 0.5,
        cursor_opacity: 1.0,
        background_color: Rgb::new(0x10, 0x12, 0x18),
        area_px: [0.0, 0.0, 0.0, 0.0],
        window_fill: false,
        // No `background-image`: this measures the grid path, and the image is
        // one extra quad that would only add noise.
        bg_image: None,
        // No custom shaders either — the bench measures instance assembly, and
        // the shader chain is per-frame GPU work with no CPU-path component.
        custom_shaders: Arc::new(Vec::new()),
        shader_globals: Default::default(),
    }
}

fn bench_render(c: &mut Criterion) {
    let Some((device, queue)) = headless_device() else {
        eprintln!(
            "render bench skipped: no wgpu adapter available (headless CI / no GPU). \
             The CPU shaping path is still covered by `cargo bench --bench shaping`."
        );
        return;
    };

    // Steady-state per-frame instance assembly with a warm atlas.
    let mut g = c.benchmark_group("render_instances");
    for &(cols, rows) in &[(80u16, 24u16), (200, 50), (400, 100)] {
        for kind in ["ascii", "utf8", "ligature"] {
            let frame = term_frame(snapshot_for(cols, rows, kind));
            let mut res =
            render::build_resources(&device, TARGET_FORMAT, FONT_PX, TEXT_GAMMA, &render::FontSpec::default());
            // Warm the atlas so the timed loop measures the warm-cache path.
            res.build_frame_instances(&frame, &queue);
            g.throughput(Throughput::Elements(cols as u64 * rows as u64));
            g.bench_function(format!("{kind}/{cols}x{rows}"), |b| {
                b.iter(|| std::hint::black_box(res.build_frame_instances(&frame, &queue)))
            });
        }
    }
    g.finish();

    // Cold rasterization: reset the glyph cache each iteration so the cache-miss
    // (rasterize + atlas upload) path dominates. The reset itself (clearing a few
    // hashmaps) is cheap relative to rasterizing a screenful of glyphs.
    let mut r = c.benchmark_group("render_raster_cold");
    let (cols, rows) = (200u16, 50u16);
    for kind in ["ascii", "utf8", "ligature"] {
        let frame = term_frame(snapshot_for(cols, rows, kind));
        let mut res =
            render::build_resources(&device, TARGET_FORMAT, FONT_PX, TEXT_GAMMA, &render::FontSpec::default());
        r.throughput(Throughput::Elements(cols as u64 * rows as u64));
        r.bench_function(kind, |b| {
            b.iter(|| {
                res.reset_atlas_cache();
                std::hint::black_box(res.build_frame_instances(&frame, &queue))
            })
        });
    }
    r.finish();
}

criterion_group!(benches, bench_render);
criterion_main!(benches);
