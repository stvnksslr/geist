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
cargo test               # ~79 unit tests (engine, input, selection, paste, mouse, theming, ligatures, OSC 7/52, URL detection)
cargo test <name>        # single test by name substring
cargo bench              # criterion perf benches (stream, snapshot, shaping, render) — see docs/benchmarking.md
```

Benchmarks mirror Ghostty's suite (VT-write/OSC throughput, snapshot copy, shaping,
headless renderer cost) and can be compared against upstream `ghostty-bench` via
`scripts/bench-vs-ghostty.ps1`. Full guide: **docs/benchmarking.md**.

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
- **`bell.rs`** — the non-visual `bell-features`: `MessageBeep` (system), `PlaySoundW` (audio),
  `FlashWindowEx` (attention). Declares user32 directly and resolves winmm lazily rather than
  enabling large `windows-sys` feature modules for three functions.
- **`osc_color.rs`** — side-scanner answering OSC 10/11/12 color *queries*. The engine already
  applies the set/reset forms; only `?` is dropped upstream.
- **`blur.rs`** — Windows DWM backdrop (acrylic/mica) for `background-blur`: the documented Win11
  `DWMWA_SYSTEMBACKDROP_TYPE`, falling back to the undocumented `SetWindowCompositionAttribute` accent
  policy (resolved via `GetProcAddress`, never linked) on Win10, then to nothing.
- **`config.rs`** — Ghostty-format config (`key = value` lines, kebab-case keys, unquoted
  colors, repeatable `palette`) from `%APPDATA%\giest\config` (override with `GIEST_CONFIG`);
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
- **OSC 7 (working dir) must be side-scanned — `Terminal::pwd()` is always empty.** libghostty-vt's
  *read-only* stream parses OSC 7 but discards `report_pwd` (it never reaches the terminal's `pwd`), so
  the binding's `pwd()` returns `None` even after a valid report (unlike `title()`, which works). So a new
  split's "inherit the parent's cwd" is built by side-scanning the PTY bytes ourselves in `osc7.rs`
  (`Osc7Scanner`, fed from `pump_pty` like `osc52.rs`) and feeding the URI to `Session::pwd`. PowerShell
  and cmd don't emit OSC 7 by default, so `Profile::launch_args` (`profiles.rs`) injects a prompt hook at
  spawn (`pwsh`/`powershell` via `-EncodedCommand`, `cmd` via `prompt $E]7;…`); WSL/custom shells just
  fall back to the default dir. The split's cwd is read in `App::split` and passed through `Session::new`
  → `Pty::spawn` → `CommandBuilder::cwd`.

- **Multi-window: one `RenderState`, immediate viewports, and one mandatory line.** eframe keeps a
  *single* `RenderState` (and so one wgpu device, one `callback_resources`, one glyph atlas) for
  every viewport — `render::init` is once-per-process and `TermFrame` resolves fine in a child
  window. Consequences: (1) secondary windows must be **immediate** viewports, because
  `show_viewport_deferred` needs `Fn + Send + Sync + 'static` and `Session` is `!Send` (`Rc`s + an
  FFI terminal); (2) a child's `ViewportBuilder` **must** set `.with_transparent(...)` — the vendored
  egui-winit patch reads it to set `WS_EX_NOREDIRECTIONBITMAP` at creation, so omitting it renders
  that window as the grey wash below; (3) build the child builder **once** and clone it verbatim —
  patching a recreate-class field makes eframe clear *every* viewport's surface, root included;
  (4) font size is app-global, since only one `GpuResources` can live in the type-keyed
  `callback_resources`.
- **`CancelClose` is honoured for the ROOT viewport only.** A child's `ViewportCommand::Close` just
  re-arms `close_requested`; a child window is destroyed by *ceasing to call*
  `show_viewport_immediate` for it. And the PTY wake deliberately targets `ViewportId::ROOT`: only a
  root pass re-runs every window, so "fixing" it to wake the owning child would stop background
  windows updating.
- **`Memory::data` is not viewport-keyed** (unlike `areas`/`focus`/`interactions`), and it holds
  `TextEditState`, `ScrollArea` offsets and `PanelState`. Every egui `Id` must therefore be
  namespaced per window (`Window::id`), or two windows share a text cursor and a tab-strip height.
- **Second vendored crate + Windows patch: `vendor/egui-winit`.** A transparent window on Windows also
  needs `WS_EX_NOREDIRECTIONBITMAP`, which upstream egui-winit never sets (it sets only
  `.with_transparent(...)`). wgpu presents a transparent surface through DirectComposition and builds
  its target with `CreateTargetForHwnd(hwnd, topmost = false)` (`wgpu-hal` `dx12/dcomp.rs`), placing
  the visual *below* the window's redirection surface — so without the flag the window composites over
  that opaque surface and renders as a **solid grey wash** with `background-opacity` having no effect.
  The ex-style is honored **only at window creation**, so the app cannot add it later
  (`SetWindowLongPtrW` + `SWP_FRAMECHANGED` was tried and does nothing), and eframe's `window_builder`
  hook hands you egui's `ViewportBuilder`, not winit's `WindowAttributes`. Hence a
  `[patch.crates-io]` onto a vendored copy whose *only* delta is that one flag. **Re-apply on every
  egui upgrade**; without it transparency silently regresses to grey.
- **Window transparency needs two more non-obvious things, and fails *silently* without them.**
  (1) `ViewportBuilder::with_transparent(true)` is **not enough on Windows**: wgpu's default DX12
  presentation path builds the swapchain straight from the HWND, and such a surface advertises only
  `CompositeAlphaMode::Opaque` (`wgpu-hal` `dx12/adapter.rs`; `Dx12SwapchainKind::DxgiFromHwnd` is
  documented as "does not support transparency"). egui-wgpu then finds no premultiplied mode, logs one
  `log::warn` giest never surfaces (no logger installed), and falls back to opaque — the window just
  stays solid with no error. `main.rs` must also set `presentation_system = DxgiFromVisual` (the
  DirectComposition path, which costs RenderDoc capture support, so it's opt-in when opacity < 1).
  (2) `App::clear_color` must return `[0,0,0,0]`. eframe's default clear is
  `rgba_unmultiplied(12,12,12,180)`, and egui's alpha blend (`src*OneMinusDstAlpha + dst*One`) can only
  ever *raise* framebuffer alpha — so a non-zero clear alpha caps the whole window's transparency and
  tints it. Both are startup-only, hence the documented restart requirement (Ghostty/macOS is the same).
- **Exactly one layer may carry `background-opacity`.** The pane-area `rect_filled` in `render_active`
  is it; the `CentralPanel` frame must stay `Frame::NONE`. Both used to paint the same rect in the same
  color, which under transparency composites to `1-(1-a)²` (a=0.5 reads as 0.75). Cells on the *default*
  background emit no quad at all (`render::bg_alpha`, mirroring Ghostty), so that one fill is what shows
  through them — any second translucent fill over the same area is a bug.
- **`Cell::bg_explicit` polarity is deliberate.** `false` (the `Default`) means "draw no background
  quad". Cells the VT iterators never yield get blanked to the default, so inverting the flag's sense
  (`bg_is_default`) would make every one of them paint opaque black over a translucent window.

- **`egui::Modal` does not stop the terminal grabbing the keyboard.** It blocks pointer interaction
  and tab-traversal focus, but `Memory::request_focus` is unconditional and the pane calls
  `resp.request_focus()` every frame — so any new modal must also be added to the *manual* gate in
  `render_active` (the `palette_open` local) or typing goes straight to the shell behind the dialog.
- **`close_requested` must be answered in the same pass.** eframe reads it from that pass's raw input
  and exits afterwards unless `ViewportCommand::CancelClose` appears in the same pass's output. And
  once a confirmed close sends `ViewportCommand::Close`, the resulting pass sees `close_requested`
  again — without the `closing` latch that re-opens the dialog forever, i.e. an unclosable window.
  `reap_dead` (shell exited) deliberately bypasses all of this: there's nothing left to confirm.
- **Indices held across frames go stale.** `App::renaming`, `tab_drag` and `PendingClose::Tab` all
  store a tab *index*; a reorder or a `reap_dead` invalidates them, and acting on a stale one edits
  the wrong tab. Clear them at both mutation points.
- **After `cargo test`, the *binary* is still stale.** `cargo test --lib` builds only the test
  harness, so launching `target\debug\giest.exe` to check a change runs the previous build — which
  looks exactly like the feature not working. Run `cargo build` before any manual/screenshot check.

## Verifying visual/rendering changes

A passing `cargo build`/`cargo test` does **not** confirm a rendering change *looks* right — the
hard bugs here (glyph clipping, emoji fragments, pane padding, scroll pacing, transparency) are
perceptual and the unit tests don't see them. Self-captured screenshots have repeatedly produced
false "it works" conclusions on exactly these tasks.

- For any change to `render/*`, padding, font sizing, or window compositing: state plainly that it
  needs **human visual confirmation**, describe what should look different, and ask the user to
  eyeball it rather than declaring success from a screenshot harness.
- If you do capture, the working method is: inject deterministic glyphs via a startup shell-wrapper
  (no synthetic keyboard) and grab the window with **PrintWindow** — not a generic screen grab.
  Treat the capture as a sanity check, not proof.
- **Transparency *can* be checked by a full-screen grab — `PrintWindow` cannot.** `PrintWindow` only
  captures the window's own bitmap, so it can never show what's behind. But
  `Graphics.CopyFromScreen` does reproduce DWM composition faithfully; it was right when the window
  was broken (grey) and right again once it was fixed. The reliable probe: put a **saturated
  full-screen window behind** (e.g. pure green), set `background-opacity = 0.5`, and check that the
  channels match `bg*0.5 + backdrop*0.5` — with `background = #101218` a correct composite reads
  `R = 0x08`, `B = 0x0c` exactly. Sample away from the window's drop shadow, which darkens the
  backdrop near the edges. Don't trust a single pixel's *appearance*; solve the blend.
- Effect sizes can be below the visible threshold (e.g. 8px padding read as "flush"). When a change
  "should" be visible but isn't, suspect the magnitude before re-debugging the mechanism.
