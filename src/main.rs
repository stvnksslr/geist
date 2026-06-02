//! giest binary entry point. The terminal lives in the `giest` library crate
//! (see `lib.rs`); this just configures eframe and launches the [`App`].
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

use giest::app::App;

fn main() -> eframe::Result {
    let options = eframe::NativeOptions {
        renderer: eframe::Renderer::Wgpu,
        viewport: eframe::egui::ViewportBuilder::default()
            .with_inner_size([960.0, 600.0])
            .with_title("giest"),
        ..Default::default()
    };

    eframe::run_native(
        "giest",
        options,
        Box::new(|cc| Ok(Box::new(App::new(cc).expect("failed to initialize giest")))),
    )
}
