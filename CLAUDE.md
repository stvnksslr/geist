# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## What this is

giest is a GPU-accelerated terminal emulator for **Windows**, in Rust. eframe (egui + wgpu)
owns the window; the terminal grid is drawn by a custom wgpu glyph-atlas pipeline; the
terminal state lives in **libghostty-vt** (Ghostty's VT engine) behind a backend-agnostic
trait; the shell runs over **ConPTY**. North-star goal: feature parity with the macOS Ghostty app.

## Build & test

```powershell
mise dev                 # debug build + run (recommended; injects Zig 0.16.0)
mise release             # optimized release build (recommended)
cargo test               # ~79 unit tests (engine, input, selection, paste, mouse, theming, ligatures, OSC 7/52, URL detection)
cargo test <name>        # single test by name substring
cargo bench              # criterion perf benches (stream, snapshot, shaping, render) — see docs/benchmarking.md
```

Benchmarks mirror Ghostty's suite (VT-write/OSC throughput, snapshot copy, shaping,
headless renderer cost) and can be compared against upstream `ghostty-bench` via
`scripts/bench-vs-ghostty.ps1`. Full guide: **docs/benchmarking.md**.

- **Requires Zig 0.16.0 on PATH** — the vendored `libghostty-vt-sys/build.rs` runs `zig build`
  to compile Ghostty's VT library. **0.15.x will NOT build it** (the pinned ghostty commit
  declares `minimum_zig_version = 0.16.0`). `mise.toml` pins `zig = "0.16.0"` and defines the
  `dev`/`release` tasks above, which run cargo from the project root with the pinned Zig on
  PATH — prefer them. Run `mise trust` once. Fallbacks: `mise exec zig@0.16.0 -- cargo build`,
  or plain `cargo build`/`cargo run --release` if Zig 0.16.0 is already on PATH.
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
  (selection anchor/head, held mouse button, alive flag, pending clipboard dialog). `pump_pty`
  drains PTY output into the engine.
- **`render/mod.rs`** — wgpu instanced-quad pipeline, per-row run shaping, instance builder, egui
  `CallbackTrait`. **`render/atlas.rs`** — rustybuzz shaping + ab_glyph rasterization (primary +
  system fallback faces) into an R8 atlas; COLR/CPAL color emoji composited into a separate RGBA atlas.
- **`pty.rs`** — ConPTY shell via portable-pty with a reader thread that wakes the UI on output.
  A second backend (`Pty::from_handoff`) drives a pseudoconsole giest did *not* create.
- **`handoff.rs`** — default-terminal handoff: `giest -Embedding` is an out-of-proc COM server for
  `ITerminalHandoff3` (hand-declared vtables), `+register-default-terminal` /
  `+unregister-default-terminal` write HKCU only with exact restore. See GAP.md for the chain.
- **`bell.rs`** — the non-visual `bell-features`: `MessageBeep` (system), `PlaySoundW` (audio),
  `FlashWindowEx` (attention). Declares user32 directly and resolves winmm lazily rather than
  enabling large `windows-sys` feature modules for three functions.
- **`osc_color.rs`** — side-scanner answering OSC 10/11/12 color *queries*. The engine already
  applies the set/reset forms; only `?` is dropped upstream.
- **`engine/png_decode.rs`** — PNG decoding for kitty graphics (`f=100`). libghostty leaves this to a
  hook that is unset by default. The install guard is **thread-local, not a `Once`**: the binding
  stores the callback per-thread, so a `Once` would leave every thread but the first without PNG
  support — including every `cargo test` after the first.
- **`blur.rs`** — Windows DWM backdrop (acrylic/mica) for `background-blur`: the documented Win11
  `DWMWA_SYSTEMBACKDROP_TYPE`, falling back to the undocumented `SetWindowCompositionAttribute` accent
  policy (resolved via `GetProcAddress`, never linked) on Win10, then to nothing.
- **`osc_notify.rs`** — side-scanner for OSC 9 / OSC 777 desktop-notification requests (the engine
  drops them on the read-only path). lib-vt's `on_desktop_notification`/`on_progress_report` were
  evaluated and **not adopted**: they match the scanner on OSC 9/777/9;4 but never see kitty OSC 99
  (title+body only, no chunking/occasion). `engine_callbacks_match_the_scanner_except_kitty_osc99`
  is the tripwire. **`notify.rs`** — shows them as Windows toasts via the notification-area
  balloon API; WinRT toasts would need a registered AppUserModelID (i.e. a Start Menu shortcut).
  **`shader.rs`** — `custom-shader`: Shadertoy GLSL → naga IR → WGSL. **The prefix's oddities are
  all forced by naga, not style**: no combined `sampler2D` (Vulkan-style `texture2D`+`sampler`
  re-formed by a `#define`), the entry point appended as a *suffix* because naga's IR needs
  functions in dependency order, `#version 450` (only 440/450/460 parse), and `vec4 iChannelTime`
  rather than `float[4]` because WebGPU requires a 16-byte array stride in uniform space — a rule
  naga's own validator does **not** enforce, so `tests/shader_gpu.rs` compiles on a *real device*
  to catch it. The render pipeline isn't built yet; see GAP.md.
  **`writefile.rs`** — `write_scrollback/screen/selection_file`: capture terminal text to a temp
  file. Its `copy`/`paste`/`open` parameter acts on the file **path**, not the contents (Ghostty's
  design); `paste` still goes through `Session::paste_str` so the paste gate holds.
  **`taskbar.rs`** — `ITaskbarList3` progress on the taskbar button, for ConEmu's `OSC 9;4`.
  windows-sys ships no COM interfaces, so the vtable prefix is declared by hand; getting a slot
  wrong fails silently, which is what `taskbar::available()` and its ignored test exist to catch.
  **`osc133.rs`** — side-scanner for the OSC 133 `C`/`D` *command* marks. The engine applies the
  `A`/`B` prompt marks to the screen (that's what `cursor_at_prompt` reads), but `D`'s exit code
  never lands on a cell, so `notify-on-command-finish` has to read it off the stream.
- **`bgimage.rs`** — `background-image`: PNG/JPEG decode (format sniffed from magic bytes) plus the
  pure fit/position geometry. Ghostty computes that geometry per vertex in its shader; giest does it
  on the CPU so it can be table-tested, and the shader (`render` mode 4) just samples.
- **Automation.** **`cli.rs`** parses the command line (`-e` swallows the rest; `--key=value` is
  replayed after the config files on every load via `config::set_cli_overrides`). **`ipc.rs`** is
  the single-instance pipe (`\\.\pipe\giest-<SID>-<session>`, JSON lines, `$GIEST_IPC_PIPE`
  overrides the name — use it for any live test so you never talk to the user's running giest);
  its threads hand requests to the UI over a channel and wake **ROOT**, and `App::answer_ipc`
  addresses windows/tabs/panes by **id**. `input_text` is a paste and goes through
  `Session::paste_str`. **`shellreg.rs`** (Explorer verb, HKCU, explicit CLI only),
  **`jumplist.rs`** (hand-declared COM vtables; ignored host test), **`restart.rs`**
  (`RegisterApplicationRestart` + a root-window subclass that writes a layout snapshot on
  `WM_ENDSESSION`, because `on_exit` never runs when Windows ends the session). Clean up after
  live tests: `+unregister-shell-integration`, and run with `jump-list = false`.
- **Distribution.** `mise package` (`scripts/package.ps1`) builds `dist/<v>/`: zip, MSIX (unsigned
  unless `-CertPath`), `giest.appinstaller`, `giest-manifest.json`. **`update.rs`** consumes that
  manifest: SHA-256 is verified *before* extraction, and a staged update is applied only by
  `startup_apply` at the top of `main` — replaced files are **renamed** to `*.old`, never deleted
  while running. Its network goes through the `Http` trait; tests use a mock. Live-test the apply
  path with a temp `LOCALAPPDATA` and a copied exe, never against `target\` or a real install.
- **`config.rs`** — Ghostty-format config (`key = value` lines, kebab-case keys, unquoted
  colors, repeatable `palette`) from `%APPDATA%\giest\config` (override with `GIEST_CONFIG`);
  defines the full ANSI 16 + 256-color palette. **`profiles.rs`** — shell profiles (pwsh/powershell/cmd/wsl).
  **`clipboard.rs`** — policy for program clipboard access, OSC 52 *and* kitty OSC 5522 (+ paste
  events, mode 5522). The engine parses both and calls `on_clipboard_write`/`on_clipboard_read`
  (installed in `ghostty_vt::install_clipboard`); this module decides `clipboard-read`/`-write`/
  `-write-limit-bytes` and MIME support. There is no OSC 52 or OSC 7 side-scanner any more.

## Non-obvious gotchas

- **Vendored binding, ahead of its own upstream.** `vendor/libghostty-rs/` is Uzaaft/libghostty-rs
  @5988a0b with **Ghostty itself bumped past the binding's pin** to `ghostty-org/ghostty` `main`
  @b32f20f (the binding pins 22d1317, 876 commits older). That bump carries three local deltas:
  `GHOSTTY_COMMIT` in `libghostty-vt-sys/build.rs`; a **regenerated** `bindings.rs`; and
  `render.rs::colors()` moved from the removed `ghostty_render_state_colors_get` onto
  `ghostty_render_state_get(.., RenderStateData::COLORS, ..)`. Plus **giest-local** wrappers in
  `terminal.rs`: `on_clipboard_write` fixed to the pinned reply-function ABI (upstream's still
  *returned* the result, which the C side ignored — every write was silently denied),
  `on_clipboard_read`, `Terminal::paste` (`ghostty_terminal_paste`), and `Mode::PASTE_EVENTS`. The old Windows static-link patch
  (`static=ghostty-vt-static`, without which the exe loads `ghostty-vt.dll` and crashes in
  `vt_write`) is now **upstream** (8272abe) — don't re-add it, but *do* check it survives a bump.
  **Regenerating `bindings.rs` on Windows has two traps:** bindgen needs `libclang.dll` (the
  `libclang` PyPI wheel is a zip that has one; point `LIBCLANG_PATH` at its `clang/native`), and it
  must target **Linux** — a Windows target types every C enum `i32` instead of upstream's `u32` and
  the safe crate stops compiling. Zig ships the headers:
  `BINDGEN_EXTRA_CLANG_ARGS="--target=x86_64-unknown-linux-musl -isystem <zig>/lib/include -isystem
  <zig>/lib/libc/include/{x86_64-linux-musl,generic-musl,x86-linux-any,any-linux-any}"` (glibc's
  headers fail on `__STD_TYPE`), then `GHOSTTY_INCLUDE_DIR=<out>/ghostty-install/include cargo run
  --manifest-path vendor/libghostty-rs/Cargo.toml -p libghostty-vt-sys --features bindgen-tool
  --bin gen-bindings`. Then build *and launch* — a link-mode regression only shows at runtime.
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
  OSC 52 / OSC 5522 run inside `engine.write` (no ctx) via `arboard`; see the clipboard-callback
  gotcha below.
- **Clipboard callbacks are synchronous; `ask` is built by cutting the engine's refusal back out.**
  `on_clipboard_read/write` must answer before returning, and an unanswered request is refused *on
  the spot* (OSC 52: empty reply; 5522: `EPERM`) straight into the response buffer. giest can't block
  on a modal, so under `ask` the callback records the buffer's length and doesn't answer; after
  `vt_write`, `collect_clipboard_deferrals` cuts the refusal at that offset out and holds it. Deny
  sends it; allow writes + sends it flipped to `DONE`, or *replays* the read into the engine with a
  one-shot grant. The offset is exact only because the stream is paused for the callback — do not
  write responses from anywhere else inside it. A paste event's follow-up read (`granted`) bypasses
  `clipboard-read` and is served the pasted text.
- **Every paste must go through `Session::paste_str`.** It is the one gate that applies
  `clipboard-paste-protection`, and unsafe text becomes a pending `ClipboardRequest` instead of
  reaching the PTY. `encode_paste` therefore has exactly one caller (the private `write_paste`);
  a new paste path that calls the engine directly would silently bypass the protection. The
  request lives on the **`Session`**, not the app, deliberately — routing it by pane index would
  hit the stale-index trap below.
- **`$?` must be the first statement in the PowerShell prompt hook.** It reflects only the
  immediately preceding command, so *any* statement above it — even an assignment — resets it and
  every command reports success. The exit code needs `$?` **and** `$LASTEXITCODE`: `$?` alone misses
  a native program's real code, `$LASTEXITCODE` alone misses a failed cmdlet. A test in
  `profiles.rs` pins the ordering, because the hook ships as base64 inside `-EncodedCommand` where a
  mistake produces no diagnostic at all — the marks simply never appear.
- **A new modal must be added to *two* gates, not one.** `App::modal_open` (shortcuts + font zoom)
  and the `palette_open` local in `render_active` (terminal keys, pane mouse, scrollbar) are
  separate lists, and missing either means the dialog is up while the terminal still takes input.
- **The working dir comes from the engine now (`GhosttyVtEngine::pwd` → `Terminal::pwd()`).** On the
  old pin it was always empty, so `osc7.rs` side-scanned OSC 7; since the b32f20f bump `vt_write`
  keeps OSC 7 (and OSC 9;9 / OSC 1337 CurrentDir), and `ghostty_vt::tests::pwd_comes_from_the_engine`
  pins it — the scanner is gone. A new split's "inherit the parent's cwd" feeds that raw URI to
  `osc7_to_path` in `Session::pwd`. PowerShell
  and cmd don't emit OSC 7 by default, so `Profile::launch_args` (`profiles.rs`) injects a prompt hook at
  spawn (`pwsh`/`powershell` via `-EncodedCommand`, `cmd` via `prompt $E]7;…`), and WSL runs Ghostty's own
  scripts (`assets/shell-integration/`, reporting Linux paths that `Pty::spawn` drops unless they map
  to a Windows dir, e.g. `/mnt/c/…`); custom shells just
  fall back to the default dir. The split's cwd is read in `App::split` and passed through `Session::new`
  → `Pty::spawn` → `CommandBuilder::cwd`.

- **ConPTY silently strips APC sequences — and re-renders everything else.** ConPTY does not pipe a
  child's output through; it parses the child's VT and emits its *own* stream, dropping sequences it
  doesn't understand. `ESC _ … ESC \` (APC, used by kitty graphics) never reaches
  `TerminalEngine::write` at all, while ordinary text and OSC pass fine — so the symptom is "the
  feature does nothing" with no error anywhere. Before debugging any new escape-sequence support,
  **confirm the bytes actually arrive**. That probe is now permanent: `Ctrl+Shift+I` opens the
  inspector, whose Terminal IO log records every PTY read with each control byte rendered
  distinctly, taken *before* `engine.write` sees it. **That is the *inbox* ConPTY.**
  `PSEUDOCONSOLE_PASSTHROUGH_MODE` (`0x8`) is **not** the fix, whatever older notes say: the inbox
  conhost (10.0.26100 even on build 26200) returns `S_OK` for it and still strips APC — measured.
  The fix is the *out-of-band* ConPTY (`conpty.dll` + `OpenConsole.exe`, 1.22+, from the
  `Microsoft.Windows.Console.ConPTY` NuGet; `scripts/fetch-conpty.ps1` installs it beside the exe),
  which forwards APC and ENQ with or without the flag. Third vendored crate: **`vendor/portable-pty`**
  (`[patch.crates-io]`) loads that `conpty.dll` by absolute path beside the exe, lets
  `conpty-passthrough = false` veto it, and passes the flag for 1.17–1.21 builds. The choice is
  **latched on the first PTY** (`ConptyPassthrough::apply` runs in `main` before any window) — and
  it is process-global, so tests select it by environment, never per test. Re-apply on a
  portable-pty bump. See GAP.md's kitty-graphics section.
  **`tests/conpty_passthrough.rs` is the probe, made permanent** — an ignored host test that spawns
  a real shell and asserts which sequences survive (OSC 7/9/52/133/777/5522 and DECSET 5522 do; APC does not). Run it
  *first* for any new escape-sequence work: `cargo test --test conpty_passthrough -- --ignored`,
  and again with `GIEST_TEST_PASSTHROUGH=1` for the sideloaded ConPTY (where APC/ENQ must arrive).
  In the default mode the APC case is asserted **inverted** — it fails if a future Windows build
  stops stripping APC, which is how we'd learn kitty graphics works without the sideload.
- **Anything newly forwarded by the sideloaded ConPTY is newly *live* in the engine.** libghostty
  enables some APC features by default — the glyph protocol (`25a1`) would answer support queries
  giest can't render — so `GhosttyVtEngine::new` switches it off. Check engine defaults for any
  sequence family that starts arriving. Likewise **OSC 72** (kitty drag-and-drop): the engine answers
  `t=q`, but the C API cannot deliver a drop, so `write` strips every OSC 72 reply (`strip_osc72`)
  rather than advertise it. ENQ is answered via `on_enquiry` (`enquiry-response`) — which is why
  that key only works under the sideloaded ConPTY.
- **Kitty image textures are keyed by `Arc<ImageData>` address, not image id** (ids are per
  terminal), and the engine's pixel copy is keyed by the image's **generation** stamp (changes on
  re-transmit and on every animation frame change). Autoplaying animations don't advance: upstream
  ticks them from its renderer and `animationTick` isn't in the C API.
- **A PTY harness with no VT engine must answer `ESC[6n` itself, or it gets nothing.** ConPTY opens
  by requesting a cursor-position report and withholds the child's output until it is answered; the
  child then blocks on the full pipe and never exits either. The app never notices because the
  engine auto-replies via `take_responses` — but any test that drives `Pty` directly must write
  `ESC[1;1R` back, and must not wait on process exit alone. `conpty_throughput.rs` does neither and
  hangs until its timeout; `conpty_passthrough.rs` does both.
- **OSC 9 is overloaded, and the disambiguation is a faithful copy, not tidy code.** ConEmu claims
  `9;1`–`9;9`; only a payload that *doesn't* match one is an iTerm2 notification. `osc_notify.rs`'s
  `parse_osc9` mirrors Ghostty's `osc9.zig` branch for branch **including its fall-through**: a
  payload that begins a ConEmu shape but doesn't complete it is a notification (`9;4;1;50` is a
  progress report; `9;4` is a notification whose body is the text `4`). Do not "simplify" it into a
  leading-digit test — that would swallow every message starting with a digit. Notifications and
  `9;4` taskbar progress therefore share **one** parser and one module: they are the same decision,
  and two parsers would be two copies of it free to drift.
- **Kitty graphics: the borrow the compiler won't catch for you.**
  `PlacementIterator::update` returns an iteration whose lifetime is tied to the *iterator*, not to
  the `Graphics` handle — so nothing stops a `vt_write` mid-walk from invalidating every pointer it
  holds, and `Image::data()` hands out a slice straight into that storage.
  `ghostty_vt::walk_placements` is therefore a **free function taking `term: &Terminal`**, which
  makes such a write a compile error. Do not "simplify" it into a `&mut self` method.
- **Images must never touch the glyph atlas.** The RGBA atlas is a fixed 2048² shelf that flushes the
  *entire* cache on overflow, so one 1080p image would evict every cached emoji and stall on
  re-rasterization. Kitty images get their own textures.
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
- **The wgpu backend must stay pinned to DX12, or a driver we never use can kill startup.**
  `Backends::all()` doesn't just *list* backends — enumerating adapters loads and initializes each
  one's driver, the OpenGL ICD included. On an AMD card that ICD (`atio6axx.dll`) faulted with an
  access violation, so the exe died before any window with **no panic, no stderr and no message of
  any kind** — the only evidence was the Windows Application event log's faulting-module line
  (`Get-WinEvent -FilterHashtable @{LogName='Application'; ProviderName='Application Error'}`;
  reach for it first for any silent-exit report, as an `0xC0000005` from a vendor DLL looks exactly
  like a giest bug). The pin in `main.rs` is therefore **unconditional**, not part of the
  transparency branch it originally lived in — that branch pinned DX12 for its own reasons, which
  masked this everywhere `background-opacity < 1` and left the *default* config as the one that
  crashed. `WGPU_BACKEND` still overrides, deliberately.
- **A keybind *leader* is bound to nothing, so `Keymap::lookup` won't reserve it.** In
  `ctrl+a>n=new_tab`, `ctrl+a` has no action of its own — a plain lookup returns `None`, the key
  reaches the shell, and the sequence never starts. `session::decide_key` must use
  `starts_binding` (exact match **or** prefix). Silent failure, and `ctrl+a` is the common case.
- **A custom shader can only see what the *renderer* drew.** Its input is an offscreen texture built
  in `prepare`; egui's own painting never reaches it. Since cells on the default background emit no
  quad (see the rule below), the window fill has to come from `TermFrame::window_fill` whenever
  shaders are active, or the shader samples transparent black and the screen goes dark. Same move
  `background-image` makes, same reason. Also: the offscreen pass **must** live in `prepare` — it is
  the only hook with a `CommandEncoder`, and `paint`'s render pass cannot be nested.
- **Don't "fix" the custom-shader Y orientation.** Shadertoy is Y-up for `fragCoord` *and* channel
  textures; WGSL is Y-down for both. Flipping `gl_FragCoord` looks like the fix and is not — in a
  fullscreen post-process the fragment writes to the pixel it is at, so the flip moves the output
  relative to the input and the screen renders upside down (this was built, measured, and reverted).
  Y-down is also what Ghostty does, so shaders written for it match.
- **`background-image` *replaces* the window fill; it does not sit on top of it.** With an image
  configured, `render_active` skips its `rect_filled` entirely and the mode-4 shader paints the
  background color *and* the image in one quad (Ghostty's bg-image pass does the same). This is
  forced by the rule above, not a style choice: painting both would put two translucent layers over
  the same rect. Two more non-obvious pieces: the quad is drawn **before** the per-pane scissor loop
  in `paint` (a pane scissor is the *grid box*, so it would clip away the padding band and the split
  gutters), and its texture can only be created in `prepare` — `paint` gets `&CallbackResources` and
  no device.
- **One decoded `background-image` `Arc` per path, process-wide (`app.rs::BG_IMAGE_CACHE`).** eframe
  keeps a single `RenderState`, so there is exactly *one* bg-image texture for every window, and the
  renderer decides whether to re-upload it by `Arc::ptr_eq`. Two windows each holding their own
  `Arc` of the same file would each see the other's texture as foreign and re-upload it **every
  frame, forever**. The cache is what makes that impossible; `Window::sibling` clones the parent's
  `Arc` for the same reason. (The pointer comparison is sound only because `GpuResources` *holds*
  the `Arc` it uploaded — a freed allocation's address could otherwise be reused.)
- **`Cell::bg_explicit` polarity is deliberate.** `false` (the `Default`) means "draw no background
  quad". Cells the VT iterators never yield get blanked to the default, so inverting the flag's sense
  (`bg_is_default`) would make every one of them paint opaque black over a translucent window.
- **The app icon is *two* mechanisms, and the window one shares one `Arc` process-wide.** `build.rs`
  embeds `assets/icon.ico` as a Win32 `RT_GROUP_ICON` (what Explorer/Start/a pinned shortcut read
  without running the exe); `icon::apply` sets the 256px master on each `ViewportBuilder` (what the
  taskbar button and Alt-Tab show). Neither covers the other's case. `icon::apply` hands out one
  shared `Arc` from a `OnceLock` because `ViewportBuilder::patch` compares icons with **`Arc::ptr_eq`**
  (egui `viewport.rs`) and `App::child_builder` rebuilds its builder *every pass* — a per-call `Arc`
  would look like a new icon every frame and push a `ViewportCommand::Icon` for every child window,
  forever. Same trap as `BG_IMAGE_CACHE` above, same fix. A missing `rc.exe` only *warns*, so an
  icon-less exe is a build-log line, not a failure. Both assets are **generated**: `mise exec --
  cargo run --example icongen` redraws every size from the per-size geometry tables in
  `src/iconart.rs` (each ICO size is drawn on its own integer pixel grid — never downscaled,
  which is what made earlier revisions mushy). Regenerate; don't hand-edit. The same module redraws
  the master in another palette for `macos-icon*` at runtime; `icon::configure` caches that `Arc` by
  source, so the shared-`Arc` rule still holds, and a test pins the official palette byte-for-byte
  against `assets/icon.png`.
- **The client-drawn caption (`macos-titlebar-style = tabs|hidden`) is a subclass on *every*
  top-level window**, found by `EnumThreadWindows` each pass (children have no HWND), so all of its
  inputs are app-global (style, strip height) and the caption buttons are **non-client**
  (`HTMAXBUTTON` is what triggers Win11 snap layouts). egui therefore never sees the pointer over
  them: hover/press live in `winchrome` and are matched to a window by screen position. Don't turn
  them into egui buttons — snap layouts would silently disappear.

- **`egui::Modal` does not stop the terminal grabbing the keyboard.** It blocks pointer interaction
  and tab-traversal focus, but `Memory::request_focus` is unconditional and the pane calls
  `resp.request_focus()` every frame — so any new modal must also be added to the *manual* gate in
  `render_active` (the `palette_open` local) or typing goes straight to the shell behind the dialog.
  A modal also stops `handle_shortcuts` running at all, so **any keybind the modal itself wants has
  to be resolved by the modal**, against the keymap rather than a hardcoded key — otherwise the
  binding is dead exactly where it is meant to work, and `unbind` on it is a lie. The search bar's
  `search_overlay_actions` is the worked example. The **inspector is the deliberate exception**: it
  is in neither gate, because a keyboard log you cannot type into is useless — but a non-modal
  overlay still has to withhold the *pointer* where it sits, or a click on its own button also
  starts a text selection in the pane below (`Window::inspector_rect`).
- **`close_requested` must be answered in the same pass.** eframe reads it from that pass's raw input
  and exits afterwards unless `ViewportCommand::CancelClose` appears in the same pass's output. And
  once a confirmed close sends `ViewportCommand::Close`, the resulting pass sees `close_requested`
  again — without the `closing` latch that re-opens the dialog forever, i.e. an unclosable window.
  `reap_dead` (shell exited) deliberately bypasses all of this: there's nothing left to confirm.
- **Indices held across frames go stale.** `App::renaming`, `tab_drag` and `PendingClose::Tab` all
  store a tab *index*; a reorder or a `reap_dead` invalidates them, and acting on a stale one edits
  the wrong tab. Clear them at both mutation points. Anything that outlives more than a frame must
  use `Tab::id` instead — that is why undo entries address tabs by id and not by slot.
- **An undo entry owns *live* sessions, and they are not pumped while it holds them.** Undoing a
  close is lossless because `Node::detach_leaf` (and the tab/window equivalents) **move** the pane
  out rather than dropping it, so the shell keeps running — which also means an entry that never
  expires is a process that never exits. `App::ui` prunes the stack every pass for that reason, not
  only when undo is used. Their PTY output buffers in the reader channel meanwhile and is drained
  on restore, so nothing is lost. And a restored `Window` must have its `closing`/`confirm` latches
  cleared: `closing` is what stops a confirmed close's own `ViewportCommand::Close` re-opening the
  dialog forever, and left set on a restored window it swallows every later close request too,
  leaving a window the × cannot shut.
- **Default-terminal handoff has four silent traps** (`handoff.rs`). (1) Only a **release** exe can
  be the COM server: a debug build is console-subsystem, COM gives it a console, and creating that
  console is *itself* delegated (to Windows Terminal, or to giest - a deadlock); it never reaches
  `main` and the caller sees `CO_E_SERVER_EXEC_FAILURE` after 30 s. `register` refuses such an exe.
  (2) The proxy DLL must be built with `midl /target NT100` and link `ole32.lib`; without the target
  flag it links but the first `EstablishPtyHandoff` is an access violation *in the caller*. (3) "Let
  Windows decide" (`{0…0}`) on Windows 11 **is** Windows Terminal, so any console window a test opens
  goes through WT's OpenConsole - kill leftover `OpenConsole.exe -Embedding` processes after an aborted
  test. (4) A crash skips `Drop`, so a live test that registers anything must do its risky COM calls
  in a **child process** (`tests/handoff_com.rs`) and keep the restore guard in the parent. Also: a
  stand-in client that exits in < `abnormal-command-exit-runtime` is held open as "failed to launch",
  which looks exactly like a reaping bug (`ping` spawned from a test does this). Live tests:
  `cargo test --test handoff_com -- --ignored` (never touches `%%Startup`) and `--test
  default_terminal` (the real chain, delegation live ~1 s); both need `cargo build --release`,
  `scripts/build-handoff-proxy.ps1`, and no giest running.
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

### Solving a capture instead of eyeballing it

A capture *can* be proof when the expected pixel value is **computed rather than recognised** — the
same discipline as the transparency probe above. This worked for `background-image`: render a
known image with a known fit, then assert the exact colour and the exact band edges. It caught
nothing (the feature was right) but it would have caught a wrong texture format, a mirrored UV, or
an off-centre anchor, none of which a human glance reliably distinguishes.

Two traps make every such measurement silently wrong, and both cost real time here:

- **The probing process must be per-monitor DPI aware** (`SetProcessDpiAwarenessContext(-4)`).
  Otherwise Windows *virtualises* `GetClientRect` and `PrintWindow` for it: at 125% scaling a
  1200×750 client is reported as 960×600 and the capture is silently downscaled by 0.8, so every
  measured edge is off by a fifth and none of the arithmetic closes.
- **`PrintWindow` needs `PW_CLIENTONLY | PW_RENDERFULLCONTENT` (`3`).** With `2` alone it renders the
  *whole* window — title bar and border included — so client-relative coordinates are shifted by an
  unknown amount.

Pick discriminating colours: mid-grey (`#808080`) moves to ~`#37` or ~`#BC` under a wrong sRGB
transform, while saturated primaries are invariant and pass either way. And derive the terminal
area's top from the tab strip's height, **not** from where the background colour first appears —
PowerShell paints its first row with an explicit background, so that row is opaque and sits between
the two.

The auto-hiding scrollbar can be captured too: park the pointer with `SetCursorPos` inside the 4-pt
hot band at the pane's right edge (2 px in — 6 px is already outside it), nudge *vertically* so the
nudge can't leave the band, and restore the cursor afterwards. Give the pane real scrollback with a
`command =` pointing at a `.cmd` that prints a few hundred lines and then blocks on `pause`; the
`command` key names a **program, not a command line**, so args have to live in a script.
