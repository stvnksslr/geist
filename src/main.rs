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
        // `window-vsync`. Startup-only: the swapchain is built before the app
        // exists, and eframe offers no way to rebuild it with a new mode.
        present_mode: if cfg.window_vsync {
            eframe::wgpu::PresentMode::AutoVsync
        } else {
            eframe::wgpu::PresentMode::AutoNoVsync
        },
        // Keep vsync (no tearing) but cap the swapchain to a single in-flight
        // frame instead of wgpu's default 2 — the content tracks the window
        // border tightly on resize and input feels ~1 frame snappier.
        desired_maximum_frame_latency: Some(1),
        ..Default::default()
    };

    if let eframe::egui_wgpu::WgpuSetup::CreateNew(setup) = &mut wgpu_options.wgpu_setup {
        // Pin the backend to DX12 — **always**, not just for transparency.
        //
        // wgpu's default is `Backends::all()`, and enumerating adapters *loads
        // and initializes every backend's driver*, including the OpenGL ICD. On
        // this AMD driver (`atio6axx.dll`, amdogl.inf) that enumeration faults
        // with an access violation, so giest died at startup with no window, no
        // panic and no stderr — a crash inside a vendor DLL giest never asked
        // for and never uses. Since GL is not a backend we would ever pick, the
        // fix is to never enumerate it. giest is Windows-only, so DX12 is the
        // one backend that matters.
        //
        // `WGPU_BACKEND` still overrides, as an escape hatch for debugging
        // (`WGPU_BACKEND=vulkan`) — an explicit request is the user's call.
        setup.instance_descriptor.backends =
            eframe::wgpu::Backends::from_env().unwrap_or(eframe::wgpu::Backends::DX12);

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
            // the user actually asked for transparency. (Vulkan-on-Windows usually
            // reports opaque-only too, which is a second reason the pin above is
            // DX12 rather than a wider set.)
            setup.instance_descriptor.backend_options.dx12.presentation_system =
                eframe::wgpu::Dx12SwapchainKind::DxgiFromVisual;
        }
    }

    let options = eframe::NativeOptions {
        renderer: eframe::Renderer::Wgpu,
        viewport: giest::icon::apply(
            eframe::egui::ViewportBuilder::default()
                .with_inner_size([960.0, 600.0])
                .with_title("giest")
                .with_transparent(want_transparent)
                // Set here as well as commanded on the first pass, so the
                // window is *created* in its configured state instead of
                // flashing a frame of the default one.
                .with_decorations(cfg.window_decoration.decorated())
                .with_maximized(cfg.maximize && !cfg.fullscreen)
                .with_fullscreen(cfg.fullscreen)
                // `initial-window = false` starts resident with the root hidden.
                .with_visible(cfg.initial_window),
        ),
        vsync: cfg.window_vsync,
        wgpu_options,
        ..Default::default()
    };

    eframe::run_native(
        "giest",
        options,
        Box::new(|cc| Ok(Box::new(App::new(cc).expect("failed to initialize giest")))),
    )
}
