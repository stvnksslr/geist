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

pub mod a11y;
pub mod about;
pub mod app;
pub mod bell;
pub mod bgimage;
pub mod blur;
pub mod cli;
pub mod clipboard;
pub mod colorspace;
pub mod command;
pub mod config;
pub mod decscusr;
pub mod dropfiles;
pub mod engine;
pub mod handoff;
pub mod hotkey;
pub mod icon;
pub mod iconart;
pub mod ime;
pub mod indicators;
pub mod inspector;
pub mod ipc;
pub mod jumplist;
pub mod keybind;
pub mod keyremap;
pub mod links;
pub mod menu;
pub mod notify;
pub mod osc133;
pub mod osc_color;
pub mod osc_notify;
pub mod padding;
pub mod panedrag;
pub mod primary;
pub mod profilepage;
pub mod profiles;
pub mod profilestore;
pub mod prompt_click;
pub mod pty;
pub mod quickterm;
pub mod render;
pub mod restart;
pub mod scrollbar;
pub mod search;
pub mod session;
pub mod shader;
pub mod shellreg;
pub mod sprite;
pub mod state;
pub mod synthetic;
pub mod taskbar;
pub mod theme;
pub mod undo;
pub mod update;
pub mod winchrome;
pub mod writefile;
pub mod xtshiftescape;
