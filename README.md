# giest

A GPU-accelerated terminal emulator for **Windows**, written in Rust with
[egui]/[eframe] for the UI and [libghostty-vt] — the terminal-state engine
extracted from [Ghostty] — as the VT model. The terminal grid is rendered by a
custom **wgpu glyph-atlas** pipeline; the shell runs over **ConPTY**.

> Status: early but real. You get a working GPU-rendered PowerShell terminal —
> live output, truecolor + styles, mode-aware keyboard input, window-fit grid,
> and scrollback. It is **not** yet at feature parity with the macOS Ghostty app
> (no tabs/splits/config/ligatures/selection yet — see the roadmap).

## Architecture

```
eframe (egui + wgpu)
├─ app.rs          window, input → PTY bytes, grid sizing, repaint
├─ render/         custom wgpu instanced-quad pipeline + glyph atlas (egui paint callback)
│  ├─ mod.rs       pipeline, instance builder, CallbackTrait
│  └─ atlas.rs     ab_glyph rasterization → single R8 atlas texture
├─ engine/         TerminalEngine trait (backend-agnostic), bytes in / grid out
│  ├─ mod.rs       trait + neutral types (GridSnapshot, Cell, KeyInput, …)
│  └─ ghostty_vt.rs  libghostty-vt backend (state, render snapshot, key encoder)
└─ pty.rs          ConPTY shell via portable-pty + reader thread
```

The renderer and input layer only touch `TerminalEngine`, never libghostty
directly — leaving room for a pure-Rust fallback engine without app changes.

## Build prerequisites

- **Rust** (stable, edition 2024 — 1.95+).
- **Zig 0.15.2** on `PATH`. The vendored `libghostty-vt-sys` build script
  compiles Ghostty's VT library with Zig. The pinned Ghostty commit requires
  exactly the 0.15.x series; **0.16.x will not build it**. Get it from
  <https://ziglang.org/download/0.15.2/>.
- A working MSVC toolchain (the default `x86_64-pc-windows-msvc` target).
- Internet on first build: the build script fetches the pinned Ghostty source.

## Build & run

```powershell
# Make sure Zig 0.15.2 is first on PATH for the build:
$env:PATH = "C:\path\to\zig-0.15.2;$env:PATH"

cargo run            # debug
cargo run --release  # release (faster rendering, smaller VT lib)
```

The first build takes a few minutes (it fetches and compiles Ghostty's VT
library via Zig); subsequent builds are fast and don't re-invoke Zig unless the
native crate changes.

## Vendoring & the Windows static-link patch

`vendor/libghostty-rs/` is a vendored copy of [Uzaaft/libghostty-rs] (@9bf2bd29)
with one local patch: on Windows the upstream build links `static=ghostty-vt`,
which resolves to the DLL *import library* (`ghostty-vt.lib`) rather than the
real static archive (`ghostty-vt-static.lib`). That makes the binary depend on
`ghostty-vt.dll` at runtime, and the Windows DLL path crashes. The patch links
the static archive instead, so giest is a single self-contained `.exe`.

## Controls

- Type to send input to the shell; printable text respects your keyboard layout.
- Enter, Tab, Backspace, Esc, arrows, Home/End/PageUp/PageDown, Delete, F1–F12,
  and Ctrl/Alt combos are encoded via libghostty's mode-aware key encoder
  (cursor-key application mode, kitty keyboard protocol, etc.).
- **Ctrl+C** sends an interrupt (SIGINT). **Mouse wheel** scrolls scrollback.
- **Left-drag** selects text; **Ctrl+Shift+C** copies it to the clipboard.
  **Ctrl+V** pastes (bracketed-paste–aware).
- **Bold and italic** render with real font faces (JetBrains Mono Nerd Font),
  and **underline/strikethrough** are drawn. Nerd Font icon/powerline glyphs are
  available.
- **Tabs:** Ctrl+Shift+T new, Ctrl+Shift+W close, Ctrl+Tab / Ctrl+Shift+Tab to
  switch; click a tab or the `+` button. (The strip appears with 2+ tabs.)
- **Splits:** Ctrl+Shift+D splits side-by-side, Ctrl+Shift+E stacks; click a pane
  to focus it (focused pane is outlined). Ctrl+Shift+W closes the focused pane.
- The window title follows the active tab's shell (OSC 0/2). The grid resizes to
  fit the window.

## Configuration

On startup giest reads `%APPDATA%\giest\config.toml` (override the path with the
`GIEST_CONFIG` environment variable). All keys are optional:

```toml
font_points = 16.0       # logical font size (scaled by display DPI)
foreground  = "#c5c8c6"  # default text color
background  = "#101218"  # default background color
```

The bundled theme also defines the full ANSI 16 + 256-color palette (see
`src/config.rs`).

## Roadmap (toward macOS Ghostty parity)

Done: VT engine integration, ConPTY I/O, GPU glyph renderer, mode-aware input,
window-fit grid, scrollback scroll, text selection + clipboard copy, bracketed
paste, faux bold/italic, window title (OSC 0/2), mouse reporting (X10/SGR with
wheel + drag), a two-pass renderer (handles wide/italic overhang), and a color
theme (default fg/bg + full 256-color palette via `config.rs`). Engine, input,
selection, paste, mouse encoding, theming, and config parsing are covered by 14
unit tests (`cargo test`). **Tabs**, **split panes**, **real bold/italic faces**,
**underline/strikethrough**, **cursor blink**, and a **file-backed config** are
implemented.

Next: ligatures (rustybuzz shaping), wide-character/grapheme width tracking,
OSC 8 hyperlinks, OSC 52 clipboard, and the kitty graphics protocol.
(Note: the themed *default background* only shows where the shell hasn't erased
with an explicit background; the palette and default foreground apply to all
colored output.)

[egui]: https://github.com/emilk/egui
[eframe]: https://github.com/emilk/egui/tree/master/crates/eframe
[Ghostty]: https://ghostty.org
[libghostty-vt]: https://ghostty.org
[Uzaaft/libghostty-rs]: https://github.com/Uzaaft/libghostty-rs
