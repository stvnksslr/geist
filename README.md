# giest

A GPU-accelerated terminal emulator for **Windows**, written in Rust with
[egui]/[eframe] for the UI and [libghostty-vt] — the terminal-state engine
extracted from [Ghostty] — as the VT model. The terminal grid is rendered by a
custom **wgpu glyph-atlas** pipeline; the shell runs over **ConPTY**.

> Status: a daily-usable GPU-rendered PowerShell terminal — live output,
> truecolor + styles, mode-aware keyboard input, mouse reporting, window-fit
> grid, scrollback, **tabs** + **selectable shell profiles** (pwsh/cmd/WSL),
> **split panes**, **text selection + clipboard** (incl. OSC 52), **ligatures**,
> **system font fallback** (CJK/symbols), and **color emoji**. It is approaching
> macOS Ghostty parity; the remaining gaps (kitty graphics, explicit OSC 8) are
> in the roadmap below.

## Architecture

```
eframe (egui + wgpu)
├─ app.rs          window, input → PTY bytes, grid sizing, repaint
├─ render/         custom wgpu instanced-quad pipeline + glyph atlas (egui paint callback)
│  ├─ mod.rs       pipeline, per-row run shaping, instance builder, CallbackTrait
│  └─ atlas.rs     rustybuzz shaping + ab_glyph rasterization (primary + system
│                  fallback faces) → R8 atlas; COLR/CPAL emoji → RGBA atlas
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

If you use [mise], the bundled `mise.toml` pins `zig = "0.15.2"` for this
project, so `mise trust` once and the right Zig is on `PATH` automatically.

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
- **Ctrl+C** copies the selection when there is one (Windows-Terminal style),
  otherwise sends an interrupt (SIGINT). **Mouse wheel**, **Shift+PageUp/Down**
  scroll scrollback, and **Shift+Home/End** jump to the top/bottom; typing jumps
  back to the bottom.
- **Ctrl+= / Ctrl+- / Ctrl+0** grow / shrink / reset the font size at runtime.
- **Ctrl+click** a URL (`http(s)://…`, `ftp://`, `file://`, `www.…`) to open it.
- **Left-drag** selects text; **double-click** selects a word (paths/URLs stay
  whole), **triple-click** selects the line, **Shift+click** extends the
  selection. **Ctrl+C** / **Ctrl+Shift+C** copy the selection to the clipboard;
  **Ctrl+V** / **Ctrl+Shift+V** paste (bracketed-paste–aware). Programs can also
  copy to the clipboard via **OSC 52** (e.g. tmux/vim over SSH).
- **Bold and italic** render with real font faces (JetBrains Mono Nerd Font),
  and **underline/strikethrough** are drawn. Nerd Font icon/powerline glyphs are
  available. **Ligatures** (`=>`, `!=`, `->`, `==`, …) are shaped with rustybuzz.
  Characters the primary font lacks (CJK, many symbols) fall back to system
  fonts, and **color emoji** are composited from Segoe UI Emoji's COLR/CPAL
  layers into a separate RGBA atlas.
- The active cursor is a solid block; unfocused panes / an unfocused window show
  a **hollow** cursor, matching Ghostty.
- **Tabs:** Ctrl+Shift+T new, Ctrl+Shift+W close, Ctrl+Tab / Ctrl+Shift+Tab to
  cycle, **Ctrl+Shift+1‑8** jump to a tab (**9** = last); click a tab or the `+`
  button, click `×` or middle-click a tab to close it.
- **Shell profiles:** the strip's `▾` menu opens a new tab running a chosen
  shell — **PowerShell 7 (pwsh)**, **Windows PowerShell**, **Command Prompt**,
  or **WSL** (whichever are installed). The default (used by `+` and
  Ctrl+Shift+T) prefers `pwsh`, overridable with `shell` in the config.
- **Splits:** Ctrl+Shift+D splits side-by-side, Ctrl+Shift+E stacks; click a pane
  to focus it (focused pane is outlined). Ctrl+Shift+W closes the focused pane.
- The window title follows the active tab's shell (OSC 0/2). The grid resizes to
  fit the window.

## Configuration

On startup giest reads `%APPDATA%\giest\config.toml` (override the path with the
`GIEST_CONFIG` environment variable). All keys are optional:

```toml
shell        = "pwsh"     # default shell: name (pwsh/powershell/cmd/wsl) or a
                          # full path; unset auto-detects (PowerShell 7 preferred)
font_points  = 16.0       # logical font size (scaled by display DPI)
foreground   = "#c5c8c6"  # default text color
background   = "#101218"  # default background color
cursor_color = "#c5c8c6"  # cursor color (omit to defer to the program/default)
padding_x    = 2.0        # logical-point padding left/right of the grid
padding_y    = 2.0        # logical-point padding above/below the grid

scrollback_limit = 10000   # max scrollback lines retained per pane

selection_background = "#385a9c"  # selected-cell background
selection_foreground = "#ffffff"  # text color over a selection (optional)
copy_on_select       = false      # copy to clipboard as soon as text is selected

# Palette overrides, Ghostty-style ("<index>=#rrggbb"):
palette = ["0=#101218", "1=#cc6666", "8=#666a73"]
```

The bundled theme also defines the full ANSI 16 + 256-color palette (see
`src/config.rs`); `palette` entries override individual indices.

## Roadmap (toward macOS Ghostty parity)

Done: VT engine integration, ConPTY I/O, GPU glyph renderer, mode-aware input,
window-fit grid, scrollback scroll, text selection + clipboard copy, bracketed
paste, real bold/italic faces, underline/strikethrough, cursor blink, window
title (OSC 0/2), mouse reporting (X10/SGR with wheel + drag), a multi-pass
renderer (wide/italic overhang), tabs, split panes, a file-backed config, and a
color theme (default fg/bg + full 256-color palette). New this round:
**ligatures** (rustybuzz run-shaping, glyphs snapped to the grid by cluster),
**system font fallback** for characters the primary font lacks (CJK/symbols),
**color emoji** (COLR/CPAL layers composited into an RGBA atlas), a **hollow
cursor** for unfocused
panes/window, **window padding**, **cursor-color + palette overrides** in
config, **double-click word / triple-click line selection**, **runtime font
resizing** (Ctrl +/-/0), **Ctrl+click to open URLs**, **keyboard scrollback**
(Shift+PageUp/Down/Home/End, scroll-to-bottom on input), configurable
**selection colors** + **copy-on-select**, **Shift+click** to extend a
selection, **Ctrl+Shift+1‑9** tab switching, per-tab **close buttons**, and
panes/tabs that **close when their shell exits**. Engine, input, selection, paste, mouse encoding, theming, config
parsing, ligature shaping, font fallback, color-emoji compositing, word/line
selection, URL detection, and OSC 52 parsing are covered by 31 unit tests
(`cargo test`).

Next:
- **COLRv1 emoji** — COLRv0 (solid layers, e.g. Segoe UI Emoji) is rendered;
  gradient/transform-based COLRv1 layers are not yet composited.
- **OSC 8 hyperlinks** — auto-detected URLs are already Ctrl+clickable, but
  *explicit* OSC 8 hyperlinks need per-cell link attribution the binding doesn't
  surface. (OSC 52 clipboard *write* is now supported via a side stream parser;
  OSC 52 read/query is intentionally not answered, to avoid leaking the
  clipboard to terminal output.)
- **Kitty graphics protocol** (inline images) and finer wide-grapheme selection.

(Note: the themed *default background* only shows where the shell hasn't erased
with an explicit background; the palette and default foreground apply to all
colored output.)

[egui]: https://github.com/emilk/egui
[eframe]: https://github.com/emilk/egui/tree/master/crates/eframe
[Ghostty]: https://ghostty.org
[libghostty-vt]: https://ghostty.org
[Uzaaft/libghostty-rs]: https://github.com/Uzaaft/libghostty-rs
[mise]: https://mise.jdx.dev
