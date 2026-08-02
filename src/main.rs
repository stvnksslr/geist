//! giest binary entry point. The terminal lives in the `giest` library crate
//! (see `lib.rs`); this just configures eframe and launches the [`App`].
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

use giest::app::App;
use giest::config::Config;

fn main() -> eframe::Result {
    // Whether the window can be transparent at all is fixed when the surface is
    // created, so it has to be decided before eframe starts — hence loading the
    // config here as well as in `App::new` (one small file read). Ghostty has the
    // same restart requirement for `background-opacity` on macOS.
    let cfg = Config::load();
    let want_transparent = cfg.background_opacity < 1.0 || cfg.background_blur.enabled();

    let mut wgpu_options = eframe::egui_wgpu::WgpuConfiguration {
        present_mode: eframe::wgpu::PresentMode::AutoVsync,
        // Keep vsync (no tearing) but cap the swapchain to a single in-flight
        // frame instead of wgpu's default 2 — the content tracks the window
        // border tightly on resize and input feels ~1 frame snappier.
        desired_maximum_frame_latency: Some(1),
        ..Default::default()
    };

    if want_transparent {
        // `with_transparent(true)` on its own is NOT enough on Windows. wgpu's
        // default DX12 presentation path builds the swapchain straight from the
        // HWND, and such a surface advertises only `CompositeAlphaMode::Opaque`
        // (wgpu-hal `dx12/adapter.rs`; `Dx12SwapchainKind::DxgiFromHwnd` is
        // documented as "does not support transparency"). egui-wgpu then finds no
        // premultiplied mode, logs one `log::warn` we never surface, and silently
        // falls back to opaque — the window just stays solid with no error.
        //
        // Going through a DirectComposition visual gives us the premultiplied
        // alpha modes. It costs RenderDoc capture support, so we opt in only when
        // the user actually asked for transparency. Vulkan-on-Windows usually
        // reports opaque-only too, so pin the backend rather than risk adapter
        // selection landing there.
        if let eframe::egui_wgpu::WgpuSetup::CreateNew(setup) = &mut wgpu_options.wgpu_setup {
            setup.instance_descriptor.backend_options.dx12.presentation_system =
                eframe::wgpu::Dx12SwapchainKind::DxgiFromVisual;
            setup.instance_descriptor.backends = eframe::wgpu::Backends::DX12;
        }
    }

    let options = eframe::NativeOptions {
        renderer: eframe::Renderer::Wgpu,
        viewport: eframe::egui::ViewportBuilder::default()
            .with_inner_size([960.0, 600.0])
            .with_title("giest")
            .with_transparent(want_transparent),
        wgpu_options,
        ..Default::default()
    };

    eframe::run_native(
        "giest",
        options,
        Box::new(|cc| Ok(Box::new(App::new(cc).expect("failed to initialize giest")))),
    )
}
