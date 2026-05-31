# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## What this is

giest is a GPU-accelerated terminal emulator for **Windows**, in Rust. eframe (egui + wgpu)
owns the window; the terminal grid is drawn by a custom wgpu glyph-atlas pipeline; the
terminal state lives in **libghostty-vt** (Ghostty's VT engine) behind a backend-agnostic
trait; the shell runs over **ConPTY**. North-star goal: feature parity with the macOS Ghostty app.

## Build & test

```powershell
mise dev                 # debug build + run (recommended; injects Zig 0.15.2)
mise release             # optimized release build (recommended)
cargo test               # ~31 unit tests (engine, input, selection, paste, mouse, theming, ligatures, OSC 52, URL detection)
cargo test <name>        # single test by name substring
```

- **Requires Zig 0.15.2 on PATH** — the vendored `libghostty-vt-sys/build.rs` runs `zig build`
  to compile Ghostty's VT library. **0.16.x will NOT build it** (the pinned ghostty commit
  declares `minimum_zig_version = 0.15.2`). `mise.toml` pins `zig = "0.15.2"` and defines the
  `dev`/`release` tasks above, which run cargo from the project root with the pinned Zig on
  PATH — prefer them. Run `mise trust` once. Fallbacks: `mise exec zig@0.15.2 -- cargo build`,
  or plain `cargo build`/`cargo run --release` if Zig 0.15.2 is already on PATH.
- **Always run cargo from the project root.** Running it from inside `vendor/libghostty-rs/...`
  builds the *vendored crate* instead of giest (cargo walks up to the nearest Cargo.toml). A
  "Finished" that only mentions `libghostty-vt` compiling means you're in the wrong directory.
- First build takes minutes (fetches + compiles Ghostty via Zig); later builds skip Zig unless
  the native crate changes. Edition 2024, requires recent stable Rust.

## Architecture

Layered so the renderer/input never touch libghostty directly — leaving room for a pure-Rust
fallback engine without app changes:

- **`engine/mod.rs`** — the `TerminalEngine` trait (bytes in, grid out) plus neutral types
  (`GridSnapshot`, `Cell`, `KeyInput`, `MouseInput`, `Rgb`). The engine resolves palette indices
  and default fg/bg to concrete RGB so the renderer only ever deals in true color; `inverse` is
  pre-applied to cell fg/bg. `engine/ghostty_vt.rs` is the only libghostty-vt implementation.
- **`app.rs`** — the eframe app. Owns a `Vec<Tab>`; each `Tab` is a **binary split tree** of
  panes (`Node::Leaf | Split | Empty`) with one focused leaf id. Splits divide only the focused
  pane, so they nest like Ghostty rather than re-flowing onto a shared axis. Handles input →
  PTY bytes, grid sizing, repaint; all active panes are laid out and painted in a single GPU callback.
- **`session.rs`** — one session = a `Pty` + a `GhosttyVtEngine` + per-pane interaction state
  (selection anchor/head, held mouse button, alive flag, OSC 52 scanner). `pump_pty` drains PTY
  output into the engine.
- **`render/mod.rs`** — wgpu instanced-quad pipeline, per-row run shaping, instance builder, egui
  `CallbackTrait`. **`render/atlas.rs`** — rustybuzz shaping + ab_glyph rasterization (primary +
  system fallback faces) into an R8 atlas; COLR/CPAL color emoji composited into a separate RGBA atlas.
- **`pty.rs`** — ConPTY shell via portable-pty with a reader thread that wakes the UI on output.
- **`config.rs`** — TOML config from `%APPDATA%\giest\config.toml` (override with `GIEST_CONFIG`);
  defines the full ANSI 16 + 256-color palette. **`profiles.rs`** — shell profiles (pwsh/powershell/cmd/wsl).
  **`osc52.rs`** — side-stream parser for OSC 52 clipboard-set.

## Non-obvious gotchas

- **Vendored binding + Windows static-link patch.** `vendor/libghostty-rs/` is a vendored copy of
  Uzaaft/libghostty-rs (@9bf2bd29), depended on by path *specifically to apply one patch*: on
  Windows, upstream's `static=ghostty-vt` resolves to the DLL **import lib**, making the exe depend
  on `ghostty-vt.dll` whose runtime path crashes (access violation in `vt_write`). The patch in
  `vendor/.../libghostty-vt-sys/build.rs` links `static=ghostty-vt-static` (the real archive) on
  Windows. Don't "simplify" this back to the upstream form. Upstream's Windows CI only builds, never runs.
- **Detecting shell exit.** On Windows ConPTY the master *output* pipe usually does NOT reach EOF
  when the child exits (portable-pty keeps the pseudoconsole open), so the reader thread stays
  blocked and its channel never disconnects. Detect exit by polling the child process directly
  (`Pty::is_running` → `Child::try_wait`), not by read()==0. The app also `request_repaint_after(500ms)`
  so an idle exit still gets reaped (drop dead panes → drop emptied tabs → close window on last tab).
- **Clipboard keys are pre-translated by egui-winit.** `egui_winit` converts copy/cut/paste shortcuts
  into `Event::Copy`/`Cut`/`Paste(contents)` and returns *without* emitting a raw `Event::Key`. Its
  `is_copy_command` ignores Shift, so both Ctrl+C and Ctrl+Shift+C arrive as `Event::Copy`. Copy/paste
  logic must live in the `Event::Copy`/`Cut`/`Paste` arms, not the Key handler (that would be dead code).
  Windows-Terminal semantics: `Event::Copy` copies the selection if one exists, else sends `0x03` (SIGINT).
  OSC 52 clipboard *write* runs from `pump_pty` (no ctx) via `arboard` in `osc52.rs`; OSC 52 read/query
  is intentionally unanswered to avoid leaking the clipboard to terminal output.
