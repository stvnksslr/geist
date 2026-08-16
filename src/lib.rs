//! giest — a Rust/egui terminal for Windows on libghostty-vt.
//!
//! egui/eframe owns the window and chrome; the terminal grid is drawn by a
//! custom wgpu glyph-atlas pipeline (see [`render`]). The terminal state lives
//! in libghostty-vt behind the [`engine::TerminalEngine`] trait, fed by a
//! ConPTY-backed shell ([`pty`]).
//!
//! The modules are exposed as a library (in addition to the `giest` binary) so
//! integration tests (`tests/`) and benchmarks (`benches/`) can drive the host
//! environment — engine snapshots, the PTY, shaping — without going through the
//! GUI binary.

pub mod app;
pub mod bell;
pub mod bgimage;
pub mod blur;
pub mod command;
pub mod config;
pub mod decscusr;
pub mod engine;
pub mod hotkey;
pub mod icon;
pub mod keybind;
pub mod notify;
pub mod osc133;
pub mod osc52;
pub mod osc7;
pub mod osc_color;
pub mod osc_notify;
pub mod profiles;
pub mod pty;
pub mod quickterm;
pub mod render;
pub mod scrollbar;
pub mod search;
pub mod session;
pub mod shader;
pub mod state;
pub mod synthetic;
pub mod taskbar;
pub mod theme;
pub mod writefile;
