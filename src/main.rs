//! giest — a Rust/egui terminal for Windows on libghostty-vt.
//!
//! egui/eframe owns the window and chrome; the terminal grid is drawn by a
//! custom wgpu glyph-atlas pipeline (see [`render`]). The terminal state lives
//! in libghostty-vt behind the [`engine::TerminalEngine`] trait, fed by a
//! ConPTY-backed shell ([`pty`]).
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod app;
mod config;
mod engine;
mod osc52;
mod profiles;
mod pty;
mod render;
mod session;

use app::App;

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
