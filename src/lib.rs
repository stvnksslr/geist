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
pub mod config;
pub mod engine;
pub mod osc52;
pub mod osc7;
pub mod profiles;
pub mod pty;
pub mod render;
pub mod session;
pub mod synthetic;
