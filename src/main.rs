//! giest binary entry point. The terminal lives in the `giest` library crate
//! (see `lib.rs`); this just configures eframe and launches the [`App`].
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

use giest::app::App;
use giest::cli::{self, Plan, Verb};
use giest::config::Config;
use giest::ipc::{self, SendError};

/// A release build is a GUI-subsystem exe, so it has no console: `--help` and
/// `+list` would print into the void. Attach to the invoking terminal's
/// console, if there is one, before writing anything. (std looks the standard
/// handles up on every write, so attaching late still works.)
fn attach_console() {
    #[cfg(windows)]
    {
        #[link(name = "kernel32")]
        unsafe extern "system" {
            fn AttachConsole(pid: u32) -> i32;
        }
        // SAFETY: ATTACH_PARENT_PROCESS; failing (no parent console, or one
        // already attached in a debug build) is fine.
        unsafe {
            AttachConsole(u32::MAX);
        }
    }
}

/// Handle everything that doesn't open a window in this process. Returns an
/// exit code to stop with, or `None` to go on and start the terminal.
fn run_cli(cli: &cli::Cli) -> Option<i32> {
    if !cli.errors.is_empty() {
        attach_console();
        for e in &cli.errors {
            eprintln!("giest: {e}");
        }
        eprintln!("Run 'giest --help' for usage.");
        return Some(2);
    }
    match cli.verb {
        Verb::Help => {
            attach_console();
            println!("{}", cli::HELP);
            return Some(0);
        }
        Verb::Version => {
            attach_console();
            println!("{}", cli::version_string());
            return Some(0);
        }
        Verb::RegisterShellIntegration | Verb::UnregisterShellIntegration => {
            attach_console();
            let register = cli.verb == Verb::RegisterShellIntegration;
            let r = if register {
                giest::shellreg::register()
            } else {
                giest::shellreg::unregister()
            };
            return Some(match r {
                Ok(()) => {
                    println!(
                        "giest: Explorer \"{}\" {}",
                        giest::shellreg::LABEL,
                        if register { "registered" } else { "removed" }
                    );
                    0
                }
                Err(e) => {
                    eprintln!("giest: {e}");
                    1
                }
            });
        }
        Verb::RegisterDefaultTerminal | Verb::UnregisterDefaultTerminal => {
            attach_console();
            let r = if cli.verb == Verb::RegisterDefaultTerminal {
                giest::handoff::register()
            } else {
                giest::handoff::unregister()
            };
            return Some(match r {
                Ok(report) => {
                    print!("{report}");
                    0
                }
                Err(e) => {
                    eprintln!("giest: {e}");
                    1
                }
            });
        }
        _ => {}
    }

    // The command line's overrides must be in place before the first load, so
    // `single-instance` can itself be overridden (`--single-instance=false`).
    giest::config::set_cli_overrides(cli.override_body());
    let startup_cfg = Config::load();
    match cli.plan_with(startup_cfg.single_instance, startup_cfg.drop_behavior) {
        Plan::ForwardOnly(req) => {
            attach_console();
            match ipc::send(&ipc::pipe_name(), &req) {
                Ok(resp) => {
                    println!("{}", serde_json::to_string_pretty(&resp).unwrap_or_default());
                    Some(if resp.ok { 0 } else { 1 })
                }
                Err(SendError::NoServer) => {
                    eprintln!("giest: no running giest instance");
                    Some(1)
                }
                Err(SendError::Failed(e)) => {
                    eprintln!("giest: {e}");
                    Some(1)
                }
            }
        }
        Plan::Forward(req) => {
            // Try the running instance; a second attempt covers the race where
            // two launches both found no server and the other one won.
            for attempt in 0..2 {
                match ipc::send(&ipc::pipe_name(), &req) {
                    Ok(resp) if resp.ok => return Some(0),
                    Ok(resp) => {
                        attach_console();
                        eprintln!("giest: {}", resp.error.unwrap_or_else(|| "request failed".into()));
                        return Some(1);
                    }
                    Err(SendError::NoServer) if attempt == 0 => {
                        if ipc::start_server() {
                            return None;
                        }
                    }
                    Err(SendError::NoServer) => break,
                    Err(SendError::Failed(e)) => {
                        attach_console();
                        eprintln!("giest: {e}; starting a separate instance");
                        break;
                    }
                }
            }
            None
        }
        Plan::Local { serve } => {
            if serve {
                ipc::start_server();
            }
            None
        }
    }
}

/// `giest -Embedding`: COM started us as the default terminal (`handoff.rs`).
/// Take OpenConsole's one handoff, then give it to the running instance, or
/// keep it (returning `None`) and become the instance that shows it.
fn run_embedding() -> Option<i32> {
    // COM starts us with no console, so a failure here is otherwise
    // invisible: the console program just never appears.
    giest::handoff::log("-Embedding started");
    let a = match giest::handoff::serve_one(std::time::Duration::from_secs(30)) {
        Ok(a) => a,
        Err(e) => {
            giest::handoff::log(&format!("no session: {e}"));
            return Some(1);
        }
    };
    giest::handoff::log(&format!(
        "session received: title {:?}, client pid {}",
        a.title,
        giest::handoff::process_id(&a.client)
    ));
    if Config::load().single_instance {
        let req = ipc::Request::Handoff {
            pid: std::process::id(),
            handles: a.raw(),
            title: a.title.clone(),
            show_window: a.show_window,
        };
        match ipc::send(&ipc::pipe_name(), &req) {
            // The instance duplicated the handles in; ours can go.
            Ok(resp) if resp.ok => {
                giest::handoff::log("forwarded to the running instance");
                return Some(0);
            }
            Ok(resp) => giest::handoff::log(&format!("running instance refused: {:?}", resp.error)),
            Err(SendError::Failed(e)) => giest::handoff::log(&format!("forwarding failed: {e}")),
            Err(SendError::NoServer) => {
                ipc::start_server();
            }
        }
    }
    giest::handoff::set_initial(a);
    None
}

fn main() -> eframe::Result {
    let embedding = giest::handoff::is_embedding(&std::env::args().skip(1).collect::<Vec<_>>());
    let cli = if embedding {
        // No update apply here: OpenConsole is waiting on the COM call.
        if let Some(code) = run_embedding() {
            std::process::exit(code);
        }
        cli::Cli::default()
    } else {
        cli::from_env()
    };
    // A verified update staged by `auto-update` is installed here, before
    // anything loads conpty.dll or opens the IPC pipe; the new exe is then
    // launched with the same arguments and this (old) one steps aside.
    if embedding || matches!(cli.verb, Verb::Help | Verb::Version) {
    } else if giest::update::startup_apply() {
        std::process::exit(0);
    }
    if !embedding && let Some(code) = run_cli(&cli) {
        std::process::exit(code);
    }
    if let Some(argv) = cli.initial_argv() {
        cli::set_initial_argv(argv.to_vec());
    }
    cli::set_restore_session(cli.restore_session);
    cli::set_explicit_start(cli.cwd.is_some() || cli.command.is_some());

    // Whether the window can be transparent at all is fixed when the surface is
    // created, so it has to be decided before eframe starts — hence loading the
    // config here as well as in `App::new` (one small file read). Ghostty has the
    // same restart requirement for `background-opacity` on macOS.
    let cfg = Config::load();
    // Which ConPTY carries the shells is latched on the first spawn, so it is
    // decided here, before any window (and so any PTY) exists.
    cfg.conpty_passthrough.apply();
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

    // `macos-icon`: select the configured icon before the root builder takes
    // it, so the window is created with it (the first call reports no change).
    let _ = giest::icon::configure(&cfg);
    // `macos-titlebar-style = tabs | hidden`: the client-drawn caption. Latched
    // before any window exists so the root is subclassed on its first pass.
    giest::winchrome::set_caption_style(match cfg.titlebar_style {
        giest::config::TitlebarStyle::Tabs => giest::winchrome::CaptionStyle::Tabs,
        giest::config::TitlebarStyle::Hidden => giest::winchrome::CaptionStyle::Hidden,
        _ => giest::winchrome::CaptionStyle::Native,
    });

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
