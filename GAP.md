# giest → Ghostty macOS: Feature-Gap & Parity Roadmap

giest's north-star is feature parity with the **macOS Ghostty app**. This document audits the gap and
lays out a phased roadmap. It is grounded in three sources read directly: giest's `src/` tree, the
complete Ghostty source vendored locally under `target/.../out/ghostty-src/` (the macOS Swift app *and*
the Zig core / `Config.zig`), and the Rust binding `vendor/libghostty-rs/` (to judge, per gap, whether
the terminal data already exists and just needs wiring).

**Headline finding.** giest has a strong, correct *spine* — real VT engine, splits, tabs, ligatures,
emoji, smooth scroll, command palette — but it covered a fraction of Ghostty's config surface and
~90 keybind actions, with little of the macOS app's UX breadth. *(Measured since: **116 of Ghostty's
208 config keys** (upstream `main`, 2026-09-19) are now supported — see the config-surface ledger
and the full parity plan below.)* The encouraging part: **much of the gap
is plumbing, not greenfield.** libghostty-vt already surfaces underline styles, faint/overline, OSC 8
hyperlinks, OSC 133 semantic-prompt marks, kitty graphics, the bell, and a rich selection model — data
giest's single per-cell chokepoint (`engine/ghostty_vt.rs::copy_cell`) historically discarded.

---

## Full parity plan against Ghostty `main` (re-audited 2026-09-19)

**Baseline.** giest now builds on Ghostty `main` itself (@b32f20f, via libghostty-rs @5988a0b, Zig
0.16.0), so the plan and the engine describe the same Ghostty. Three sources were diffed:
(1) **config keys** in `Config.zig`, (2) **keybind actions** in `Binding.zig`, (3) **app-level
features** that are neither — surveyed from `macos/Sources` and from the 673 user-facing commits
between the old pin (b869a6e) and `main`. Key/action extraction is the config-surface ledger's
method, re-run against the `main` checkout the build fetches.

Scoreboard: **135 of 208** public config keys · **76 of 88** actions · app features in §C.
Effort: **S** <1d · **M** 1–3d · **L** ~1wk · **XL** multi-week.

### A. Config keys still unsupported (87)

**A1. Real work, cross-platform.** Ordered roughly by value.

| Key(s) | What it takes | Effort |
|---|---|---|
| ✅ `shell-integration`, `shell-integration-features` | Done. `none` disables every injected hook (pwsh/cmd prompt hooks + WSL). Features per shell: **pwsh/powershell/cmd** — `cursor` (bar at the prompt, `5`/`6` by `cursor-style-blink`; reset to default by the *session* on Enter, since these shells have no pre-exec hook) and `title` (cwd at the prompt; never the running command, same reason); `sudo`/`ssh-env`/`ssh-terminfo`/`path` N/A (no terminfo on Windows, no `ghostty` CLI). **WSL** — Ghostty's own bash/zsh/fish/elvish/nushell scripts, vendored in `assets/shell-integration/` (embedded, extracted to `%LOCALAPPDATA%\giest\shell-integration`), reached via WSLENV `/p` and injected by `giest-wsl.sh` with upstream's per-shell mechanism (bash `ENV`+`--posix`, zsh `ZDOTDIR`, fish/elvish/nushell `XDG_DATA_DIRS`); `cursor`/`title`/`sudo`/`path` pass through, `ssh-*` are **withheld** (upstream wraps `ssh` in `ghostty +ssh`, which doesn't exist in WSL). `detect` on WSL reads `$SHELL`; a forced `bash`/`zsh`/… only changes the WSL scheme (Ghostty has none for pwsh/cmd). Native Windows bash/zsh (`command = …bash.exe`) are not injected. Verified: pwsh/cmd features and Ghostty's bash script via the bootstrap (Git for Windows bash) in `tests/conpty_passthrough.rs`; zsh/fish/WSL itself unverified (no distro on the dev box). | ✅ |
| `scrollback-compression` | Engine now exposes `Terminal::compress` + `compression_activity`; needs a per-session idle timer (compress after N s without an activity-token change). | S |
| ~~`cursor-click-to-move`~~ | ✅ Port of `maybePromptClick`/`promptClickLine` (`prompt_click.rs`). `osc133.rs` side-scans `cl=`/`click_events=` off `A` (the C API exposes neither); the engine reports per-cell OSC 133 *input* content (`prompt_rows`). `cl` → arrows via the engine encoder (DECCKM-aware), `click_events=1|2` → SGR click. The pwsh/cmd hooks now send `A;cl=line`. Needs a live check that ConPTY keeps the typed text's input marking. | — |
| ~~`link`, `link-previews`~~ | ✅ Done (`links.rs`). Priority is upstream's: OSC 8, then `link` rules in order, then `link-url` last. Divergence: upstream declares `link` but cannot parse it ("TODO: This can't currently be set!"), so giest's syntax is its own — `link = <regex>`, repeatable, empty clears, action always "open"; a match is matched per row (no cross-wrap). `link-previews = true/false/osc8` gates the hover banner. | — |
| ~~`env`, `input`, `initial-command`, `wait-after-command`, `abnormal-command-exit-runtime`~~ | ✅ Done. `env` is an ordered map (empty resets, `KEY=` removes) passed to `CommandBuilder::env`; `input` decodes Zig escapes for `raw:`/`path:`/untagged, 10MB cap, all-or-nothing; `initial-command` resolves like `command` (a profile name keeps its prompt hooks) for the startup surface only — a `window-save-state` restore replaces it. The abnormal check needs a non-zero code (upstream waives that only on macOS) and measures runtime from `GetProcessTimes`, not from when the 500 ms idle poll noticed. | — |
| ~~`key-remap`~~ | ✅ `keyremap.rs`, applied to the pass's input in `Window::run_pass` (before bindings, `decide_key`, encoding, mouse). Divergence: egui has no sided modifiers (sided names act on both sides) and never reports the Win key, so `super` works only as a *target*. | — |
| ~~`font-codepoint-map`, `clipboard-codepoint-map`~~ | ✅ Mapped faces resolve like `font-family` and win after sprites, before the primary font (a face lacking the glyph falls through). `Session::copy_text` applies the clipboard map to every copy (not search / `write_selection_file`). | — |
| ~~`font-shaping-break`~~ | ✅ `cursor` (default on): the cursor cell is its own run; keyed on the snapshot's visibility, not the blink phase. | — |
| `grapheme-width-method` | Engine already has mode 2027; expose the option. | S |
| `cursor-text` | Text color under the cursor (incl. `cell-foreground`). | S |
| ~~`window-padding-color`~~ | ✅ Done (`padding.rs`), **needs human visual confirmation**. Upstream extends per pixel in its cell shader; giest paints the same nearest-cell colours as egui rects after the terminal callback (the per-pane scissor is the grid box, so the renderer cannot reach the band). `extend` applies upstream's `neverExtendBg` to the top/bottom rows (default-bg cell or powerline glyph) — minus the prompt-row check, since the snapshot has no per-row OSC 133 marks. A custom shader does not see the fill (it is painted after the offscreen pass). | — |
| ~~`window-show-tab-bar`, `maximize`, `fullscreen`, `title`, `initial-window`, `quit-after-last-window-closed` (+`-delay`)~~ | ✅ Done. **Resident mode** (no window open): the last window is kept as a tab-less template (`App::dormant`) and the root viewport is *hidden*, not closed — closing it is what ends an eframe process (eframe 0.34 still paints invisible windows, so timers and hotkeys run). Only a `global:` keybind (`new_window`, `new_tab`, `toggle_visibility`, `toggle_quick_terminal`) brings a window back — there is no tray icon yet. `quit-after-last-window-closed` defaults **true** (the Windows convention; upstream's default is "Linux only"); `-delay` quits when it expires with no window. `initial-window = false` starts resident (its first shell is spawned and immediately dropped — `Window::first` needs a session) and skips `window-save-state` restore; with no delay it stays resident. `window-show-tab-bar` **defaults to `always`** (divergence from `auto`): the strip holds the profile picker. `title`: a runtime `set_window_title`/`prompt_window_title` outranks it. `fullscreen = non-native*` behaves as `true` (upstream's non-macOS rule). The root × no longer lets eframe close the root itself (`CancelClose`, then the retire decides) — which also fixes the root × quitting giest while other windows were open. | — |
| ~~`split-preserve-zoom`~~ | ✅ `navigation`: `goto_split` moves the zoom to the newly focused pane. Without it, navigating out of a zoom now **unzooms and moves** (upstream); giest used to refuse to navigate while zoomed. Directional nav while zoomed uses the unzoomed layout (`Node::leaf_rects`). | — |
| ~~`mouse-shift-capture`, `click-repeat-interval`~~ | ✅ Held Shift takes the mouse back from a tracking program unless captured; `true`/`false` defer to `XTSHIFTESCAPE`, side-scanned (`xtshiftescape.rs`) — whether ConPTY forwards it is unprobed. `click-repeat-interval` sets egui's double-click window; `0` → `GetDoubleClickTime`. | — |
| `title-report`, `vt-kam-allowed` | Engine options. Title reports are now **off by default** upstream — a behavior change the bump brought in. | S |
| `palette-generate`, `palette-harmonious` | Generate the 256-color cube from the 16 base colors. | S |
| ~~`command-palette-entry`~~ | ✅ Upstream grammar incl. Zig-literal quoting; `clear` drops the built-ins, empty restores them; unparseable actions are dropped. | — |
| ~~`window-subtitle`, `window-title-font-family`~~ | ✅ ◐ Subtitle (`working-directory`) is appended to the window caption as `title — cwd` (a Windows caption has one line). The title font applies to the **tab strip** only — the caption is drawn by DWM with the system font; resolved through the renderer's font scan, startup-only. | — |
| ~~`app-notifications`~~ | ✅ In-app toasts ("Copied to clipboard", "Reloaded the configuration"); `Window::render_toast`, per-window egui temp data. | S |
| `language` | Only meaningful once the UI is localized — deferred. | — |

**A2. Windows analogues of platform keys.** Done (`winchrome.rs`): ✅ `window-decoration`
(`none`/`false` → `ViewportCommand::Decorations(false)`; `auto`/`client`/`server` all mean "native
caption", Windows having one decoration system; a reload re-reads it), ✅ `window-titlebar-background` /
`-foreground` (Win11 `DWMWA_CAPTION_COLOR` / `DWMWA_TEXT_COLOR`, applied to **every** top-level
window on the UI thread via `EnumThreadWindows` — child viewports have no reachable HWND; pre-Win11
ignores them; unlike GTK not gated on `window-theme = ghostty`), ✅ `window-vsync` (`AutoVsync` /
`AutoNoVsync`, **startup-only**: the swapchain predates the app), ◐ `window-step-resize` (a
`WM_SIZING` subclass on the **root window only** — the one HWND giest can reach — snapping the
client to whole cells of the non-grid overhead measured each frame; with splits it snaps the whole
pane area, not each pane), ✅ `quick-terminal-animation-duration` (slide in/out from the anchored
edge, ease-out cubic; `center` doesn't slide; a newly *created* quick terminal may show one frame at
rest before the slide starts). Still open: `window-colorspace` (display-p3 → HDR swapchain, L),
~~`font-thicken` + `-strength`~~ ✅ (1px coverage dilation weighted by strength; upstream is
macOS-only — needs eyeballing), ~~`drag-handle`~~ ✅ (see §C pane drag), `auto-update` / `auto-update-channel` (§C).

**A3. Blocked.** `enquiry-response` — ConPTY strips ENQ (probed; see the config-surface ledger).

**A4. N/A on Windows.** Every `gtk-*`, `linux-*`, `x11-*`, `class`, `async-backend`, most `macos-*`
(`macos-icon*` map to the runtime-icon row in §C), `quick-terminal-keyboard-interactivity` /
`-space-behavior`, `config-default-files` (CLI-only), `term` (ConPTY sets its own),
`freetype-load-flags`, and the private `_`-prefixed fields.

### B. Keybind actions still missing (17)

| Action | Plan | Effort |
|---|---|---|
| ~~`resize_split`, `equalize_splits`~~ ✅ | Done: `Node::Split::ratio`, nearest-ancestor resize (10–90% + 2-cell clamp), leaf-weighted equalize, ratios persisted (`S v 0.3`; old files read 0.5). Upstream's non-macOS `super+ctrl+shift+arrow` defaults are not bound — giest sees no Win-key modifier. | M |
| ~~`prompt_window_title`, `prompt_surface_title`~~ ✅ | A small modal (in both input gates) prefilled with the current title; empty clears the override. A modal rather than the inline tab-rename box: a pane or window has no strip slot to edit in. `new_split:left` / `:up` now put the new pane before the focused one instead of aliasing right/down. | S |
| ~~`move_tab_to_new_window`~~ ✅ | The active tab is moved (shells running) into `Window::sibling_with`; a window's only tab is a no-op, as upstream. Not undoable (upstream neither). | S |
| ~~`goto_window`, `toggle_visibility`~~ ✅ | `toggle_visibility` is app-scoped: `Visible(false/true)` to every window but the quick terminal, focus restored on show, no-op while fullscreen (upstream). Only a `global:` bind can bring hidden windows back. | S |
| `reset_window_size` | Re-apply `window-width` / `-height`. | S |
| `copy_url_to_clipboard` | `url_at` under the pointer → clipboard. | S |
| `scroll_to_selection`, ~~`paste_from_selection`~~ | `paste_from_selection` ✅: pastes the in-process PRIMARY emulation (`primary.rs`) through `paste_str`. | S |
| `end_key_sequence` | Flush a pending leader as literal keys. | S |
| ~~`toggle_window_decorations`~~ ✅ | Per window, via `ViewportCommand::Decorations`. | S |
| `check_for_updates` | With auto-update. | — |
| ~~`toggle_tab_overview`~~ ✅ | Modal grid of the window's tabs: title + a *text* thumbnail (bottom 12 non-blank rows of the focused pane's screen, captured when it opens — not a live render). Arrows/Home/End move, Enter or click switches, Esc, a backdrop click or the toggle's own binding (resolved by the modal) closes. In both input gates. Divergence: no rendered previews. Needs a human look. | M–L |
| ~~`show_on_screen_keyboard`~~ ✅ | `ITipInvocation::Toggle` on the touch-keyboard broker (hand-declared vtable); starts `TabTip.exe` if the broker isn't running; silent without one. It *toggles* — Windows exposes no reliable "is it visible" query on Win11 — so a second press hides it. Needs a human check on a touch-keyboard machine. | S |
| `crash`, `cursor_key`, `show_gtk_inspector` | Debug-only / internal / GTK — N/A. | — |

### C. App-level features (no key, no action)

| Feature | giest | Windows shape | Effort |
|---|---|---|---|
| **IME / preedit** (CJK, dead keys, Win+. emoji panel) | ◐ | Done: `PlatformOutput::ime` at the cursor cell (candidate window placement), `Event::Ime` preedit/commit (`ime.rs`, commit/Text dedupe, keys held back while composing), preedit drawn underlined at the cursor. Missing: preedit caret/segment styling (egui drops winit's cursor range), overlong preedit doesn't wrap; needs a human check with a real CJK IME | M |
| **Split divider drag** | ✅ | Grab band ±3pt around the gutter, resize cursor, clamped to 2 cells a side; double-click equalizes (upstream). Needs human visual confirmation. | M |
| **Pane drag-to-rearrange, drag out to tab/window** | ✅ | `drag-handle` (`auto`/`always`/`never`): an 80×12pt grab handle on each pane's top edge, dots shown in the top 20% band (upstream `SurfaceGrabHandle`). Drop zones are upstream's nearest-edge triangles with the half-pane highlight (`panedrag.rs`); a drop is detach-then-insert on the split tree (`take_grab`/`put_grab`), within a tab, across tabs and across windows (sessions *move*, ids renumbered from the target window's counter). On a tab strip → new tab; outside every window → new window at the pointer (only if the pane was split or the window has other tabs, as upstream). **Tab tear-out**: a tab dragged >24pt off its strip lands in another window's strip (or its panes → appended) or becomes a new window. Every move is one undo entry ("moved split"); a window a move empties is retired and its undo re-creates it. Divergences/limits: overlapping windows resolve source-window-first, not by z-order; windows on monitors with different DPI don't share a point space, so cross-monitor drops can land off by the scale ratio; the drop relies on winit's mouse capture delivering the release outside the window. Needs human drag tests. | L |
| **File drag-and-drop** → shell-quoted path | ✅ | `dropfiles.rs`: PowerShell single quotes, cmd double quotes, WSL `/mnt/c/…` (and `\\wsl$\distro\…` back to its Linux path) with POSIX quoting; through `Session::paste_str`. Lands in the pane under the pointer, else the focused one (Windows may report no pointer motion during an OLE drag); accent outline while hovering. Needs a human drag test. | S |
| **Accessibility** (Narrator/NVDA) | ⬜ | AccessKit via egui; grid as a text node | L |
| **Child-exited bar** (exit code, abnormal exit, press-any-key) | ✅ | Painter-only strip at the pane bottom (red on failure); any key dismisses and `reap_dead` closes the pane. A held pane is not alive, so it never counts as busy for quit confirmation. **Needs human visual confirmation.** Divergence: upstream prints its non-GUI fallback into the terminal and also shows it on a normal close for undo; giest shows the bar only while held. | S–M |
| Renderer-error / spawn-error views | ✅ | A failed spawn becomes `Session::failed`: a PTY-less pane that prints the error and holds a red exit bar until a key (the first window no longer aborts startup). A lost wgpu device (`set_device_lost_callback`) paints a message over the terminal **and** shows a native `MessageBoxW`, because egui draws through the same dead device. Device loss is untested on real hardware. | S |
| **Config-errors dialog** | ✅ | parser diagnostics (unknown keys, bad values, malformed lines, unreadable includes, missing theme) collected into `Config::diagnostics`; modal with Reload Configuration / Ignore, re-shown only when the set changes. Not every setter reports a bad value yet — many still keep the old value silently | S |
| Right-click menu completeness | ✅ | `menu.rs`: Copy URL (on a link, latched when the menu opens), Copy, Paste, Split Right/Left/Down/Up, Select All, Reset Terminal, Toggle Inspector, Read-only (checked), Change Tab Title…, Change Terminal Title…; routed through `execute_action` | S |
| Key-sequence / key-table indicator | ✅ | bottom-centre pill: `[table › table]  \|  ctrl+a …`. Divergence: not draggable top/bottom like macOS | S |
| Link hover preview | ✅ | banner at the pane's bottom-left, hops right when the pointer is over it (macOS `URLHoverBanner`); `link-previews` | S |
| Per-tab bell + progress indicators | ✅ | warn-coloured dot before the tab title (set for background tabs / unfocused windows, cleared when seen); 2pt `OSC 9;4` bar along the tab's top edge (red error, amber paused, sweeping indeterminate). No per-pane bar | S |
| Taskbar overlay badge (Dock badge analogue) | ✅ | `SetOverlayIcon` (vtable slot 18, layout pinned by a test) with a generated 16px red dot while a bell rang unseen in a background window. Only windows with a recorded `HWND` (the first) — same limit as progress | S |
| Notification click → focus pane (+ highlight flash) | ✅ | `notify.rs` subclasses the root window for the icon's `CALLBACK_MSG`; `NIN_BALLOONUSERCLICK` (or a click on the tray icon) focuses the **last** notification's `(window id, pane id)` — ids, never slots — with a 0.6 s accent highlight (`Session::request_highlight`). Suppressed per upstream `shouldPresentNotification` (window active *and* pane focused); `notify-on-command-finish` ones are `requireFocus: false`. Verified by posting the callback message; **a real toast click needs a human check** (Windows 11 renders balloons as toasts). | M |
| Undo feedback ("Undo Close Tab") | ✅ | toast naming the op (`Undo: reopened 2 tabs`, `Redo: closed split`) on the window it touched | S |
| Search bar match count, drag-to-corner | ✅ | `3/17` (upstream's format); a grip drags the bar and it snaps to the quadrant it was dropped in | S |
| About box | ✅ | modal (both gates) with version, commit (`build.rs` → `GIEST_GIT_COMMIT`), build profile, links; from the palette ("About giest", `show_about`) and the tab strip's ⏷ menu | S |
| **CLI arguments** (`giest <dir>`, `-e`, `+new-window`) | ✅ | `cli.rs`: positional dir (a file opens its folder; Explorer's `"C:\"` → `C:"` quoting accident repaired), `--working-directory`, `-e argv…` (swallows the rest; initial surface only; standalone instance, as upstream's implied `gtk-single-instance = false`), `--<key>=<value>` / `--config-file` replayed after the files on every load, `+new-window` / `+new-tab` (`--command`, `-e`), `+list` / `+focus` / `+action=` / `+input=`, `--help` / `--version` (a release build `AttachConsole`s to print), `--restore-session`. `--title` is not forwarded by `+new-window`. | S |
| **Single-instance IPC** (App Intents / AppleScript / Services analogue) | ✅ | `ipc.rs`: `\\.\pipe\giest-<SID>-<session>`, JSON lines with `"v": 1`: `new_window`, `new_tab`, `focus`, `input_text` (through `Session::paste_str` — a newline paste is held by paste protection, verified live), `run_action`, `list` (stable window/tab/pane ids). DACL = user SID + SYSTEM, remote clients rejected, `FILE_FLAG_FIRST_PIPE_INSTANCE`; the client checks the server runs as the same user and grants it `AllowSetForegroundWindow`. Opt-out: giest key `single-instance = false`. A plain relaunch opens a window, a positional dir a tab; `-e` or config overrides start standalone. | L |
| Explorer "Open giest here" | ✅ | `giest +register-shell-integration` / `+unregister-shell-integration` (`shellreg.rs`): HKCU `Directory\Background\shell`, `Directory\shell`, `Drive\shell` → `"<exe>" "%V"` → a new tab over IPC. Never registered implicitly. | S / M |
| Taskbar Jump List (Dock menu) | ✅ | `jumplist.rs`: `ICustomDestinationList` user tasks New Window / New Tab / one per profile (`+new-tab --command=<profile>`); hand-declared vtables pinned by an ignored host test under a throwaway AppUserModelID. Giest key `jump-list = false` deletes the list. **Needs a human glance at the taskbar menu.** | M |
| Restart restore (`RegisterApplicationRestart`), window frames in `state.rs` | ✅ | `restart.rs`: registered with `--restore-session` (no crash/hang restarts); the root window's subclass writes a ≤2 s-old layout snapshot on `WM_ENDSESSION`, since `on_exit` never runs then (verified by sending the message). State files gain an optional `F x y w h max` record per window — old files parse unchanged and older giests skip it; restored exactly at 150% DPI (verified). A real update/reboot relaunch needs a human. | S |
| Custom caption / tabs-in-titlebar | ⬜ | `WM_NCCALCSIZE` client-drawn caption | L |
| Runtime custom app icon | ⬜ | tinted icon via `icongen` + `ViewportCommand::Icon` | M |
| Auto-update | ⬜ | winget manifest (S) → MSIX App Installer (M) → self-updater with a pill (L) | S–L |
| Default-terminal handoff | ⬜ | `IConsoleHandoff` COM server, as Windows Terminal does | XL |
| Tab overview | ✅ | see §B (text thumbnails, not rendered previews) | M–L |

### D. Protocols and engine features new on `main`

| Item | State | Work |
|---|---|---|
| No scrollback pull on resize under ConPTY (c55f213) | ✅ | done (`0458c1c`) |
| `scrollback-limit-bytes` / `-lines` | ✅ | done |
| OSC 99 (kitty notifications) | ⬜ | engine parses; route to `notify.rs` alongside 9/777 — S |
| OSC 5522 kitty clipboard + paste-events mode 5522 | ✅ | `clipboard.rs` + engine callbacks, existing permission prompts; see the ledger "Kitty clipboard (OSC 5522) + engine-side OSC 52 / pwd" |
| OSC 52 / pwd **effects in lib-vt** | ✅ | `osc52.rs` and `osc7.rs` retired; same ledger |
| `ghostty_terminal_paste` | ◐ | wrapped (`Terminal::paste`, giest-local) and used for mode-5522 paste events; ordinary text pastes still go through `encode_paste` behind the same gate — S |
| Native search API (`ghostty_search_*`) | ✅ N/A | evaluated, not adopted: no case-sensitive mode and no regex, so it would lose the `Aa` toggle; regex search built on giest's own wrap-joined text instead — see the ledger "Regex search, and why not the native search API" |
| Dirty-row iteration | ✅ | `f00c510`: only dirty rows are re-copied; the render state is now acknowledged each frame (it reported `Full` forever before). Needs an eyeball pass for stale cells while typing, scrolling and changing themes. |
| Selection gesture engine | ⬜ | optional replacement for giest's click-count logic — M |
| Default cursor style/blink engine options | ✅ N/A | probed: equivalent to `decscusr.rs` for initial, `CSI 0 q`, RIS and mode 12 — except upstream ignores mode 12 when `cursor-style-blink` is set, which only the scanner does. The scanner stays. |
| OSC 72 kitty drag-and-drop | ⬜ | after file drop — M |
| Kitty animation / relative placements / glyph protocol | ◐ | unblocked by a **sideloaded ConPTY** (`conpty-passthrough`, `scripts/fetch-conpty.ps1`). Relative placements ✅, client-driven frames (`a=a,c=N`) ✅, transient ✅ (engine-side eviction); autoplay (`s=2/3`) ⬜ — `animationTick` isn't in the C API; glyph protocol ⬜ — no outline read-back in the C API, so it is **disabled** rather than advertised. See the kitty section |
| New `middle-click-action` / `copy-on-select` values, `~` in theme paths | ✅ | `clipboard-paste`; `none/primary/clipboard/both` (`true` = clipboard, as off-Linux upstream); `primary-paste` reads the PRIMARY emulation, falling back to the clipboard while it is empty; `~`/`~\` → `%USERPROFILE%` for every theme name incl. light/dark pairs |
| Free with the bump | ✅ | XTGETTCAP, ANSI DECRQM, DECECM report, mode 2048 size-on-enable, C0/C1 fixes, CSI 2K wrap reset, color-reset fix, RIS clears progress, MOK2 + F13–F25 key encoding, kitty graphics spec fixes |

### E. Order of attack

1. **Windows correctness:** IME (M) · child-exited bar + config-errors dialog (S) ·
   `scrollback-compression` idle timer (S) · OSC 99 (S).
2. **Split model:** ✅ ratios + divider drag + `resize_split` / `equalize_splits`; ✅ pane
   drag-rearrange + `drag-handle` + `move_tab_to_new_window` + tab tear-out (L).
3. **The S-sized sweep:** A1's window/mouse/cursor options, §B's small actions, §C's right-click
   menu, indicators, link preview, file drop, About box.
4. **Shell integration:** ✅ `shell-integration(-features)`, ✅ WSL scripts; ✅ `cursor-click-to-move`.
5. ✅ **Automation:** CLI args → named-pipe IPC → Explorer entry → Jump List → restart restore → notification click.
6. **Protocols:** ✅ OSC 5522; ✅ OSC 52 / OSC 7 scanners moved onto lib-vt; ✅ regex search (native search API evaluated, not adopted — see the ledger).
7. **Chrome:** custom caption + `window-decoration` + titlebar colors; runtime icon (L).
8. **Long tail:** accessibility (L), auto-update, ~~tab overview~~ ✅, default-terminal handoff (XL), and
   shipping the out-of-band ConPTY with release builds (today it is a manual
   `scripts/fetch-conpty.ps1` step) so kitty graphics work out of the box.

---

## Status (updated 2026-09-19)

**Phase 0 — Foundations: ✅ complete**
- **P1 — Config registry + themes** ✅ `config.rs` now uses a declarative `SETTERS` key→setter table
  (adding a key is one entry) plus `theme`/theme-file resolution (`themes/<name>`, paths, `light:/dark:`).
- **P3 — Rich SGR attributes** ✅ `Cell` carries underline *style* + color, faint, overline, blink,
  invisible; the renderer draws them (curly/dotted/dashed via a procedural shader `mode 3`).
- **P2 — Config-driven keybinds** ✅ new `keybind.rs` (`Chord`/`Keymap`), `command::Action::name/from_name`,
  `keybind = trigger=action`; `session::decide_key` consults the keymap so **any** bound chord is reserved
  from the shell (the unification).

**Phase 1 — in progress**
- **OSC 8 hyperlinks** ✅ engine `hyperlink_at`; Ctrl+click opens the real target (regex stays as fallback
  for plain-text URLs). Hover styling deferred.
- **Visual bell** ✅ `on_bell` → fading amber pane border; `bell-features` config. Audible deferred (no
  Windows beep dep).
- **OSC 133 + jump-to-prompt** ✅ pwsh/cmd hooks inject OSC 133 A/B (`profiles.rs`); engine `jump_to_prompt`
  scans semantic-prompt rows; `Ctrl+Shift+Up/Down`.
- **bold-is-bright / bold-color / minimum-contrast** ✅ resolved per-cell in `engine/ghostty_vt.rs`
  (`apply_bold_color` mirrors Ghostty's `Style.fg`; `enforce_contrast` mirrors the shader's
  `contrasted_color`, skipping box/block/powerline glyphs). New `bold-color` (`bright` | color),
  deprecated `bold-is-bright`, and `minimum-contrast` (clamped 1–21) config keys; engine setters
  `set_bold_color`/`set_min_contrast`, wired in `Session::new`/`apply_config`. The bold-bright bump and
  palette-indexed underline colors read the **live** snapshot palette, so OSC 4 redefinitions are
  honored. A min-contrast-forced glyph renders opaque (faint dropped), matching `contrasted_color`.
- **cursor-style / cursor-style-blink** ✅ new `decscusr.rs` side-scanner (the OSC 7/52 pattern) tracks
  whether the program is on its *default* cursor and the default-cursor blink. While default, the session
  substitutes the configured `cursor-style` (block/bar/underline); blink follows `cursor-style-blink`,
  defaulting **on** and honoring DEC mode 12 (`CSI ?12 h/l`) when unset — exactly Ghostty's
  `default_cursor` / `default_cursor_blink orelse true` model (the vt lib resets DECSCUSR-default to a
  hardcoded block, defaults mode 12 off, and exposes no default-shape setter). Unfocused panes always
  draw a hollow *block* for any shape (`render/mod.rs`), matching Ghostty's `cursor.zig`.
  *(cursor-opacity still pending — needs P4 alpha.)*
- **Color parsing parity** ✅ all color keys now resolve **X11 color names** (vendored `src/res/rgb.txt`,
  case-insensitive) and **3-digit short hex** (`#f80`) before falling back to `#rrggbb`, via a shared
  `parse_color` — matching Ghostty's `Color.parseCLI`/`fromHex`, so named-color configs are transposable.
- **font-family / font-feature** ✅ `render/atlas.rs` now loads a configured `font-family`
  (+`-bold`/`-italic`/`-bold-italic`) by scanning the Windows font dirs and matching the name table +
  style flags (`find_font`/`face_matches`/`resolve_slots`), falling back to the built-in JetBrains Mono
  per slot; the regular slot uses a *tolerant* lookup (`find_regular_font`) so an unusual family stays on
  the user's font instead of dropping to built-in. `font-feature` specs (e.g. `-calt` to drop ligatures,
  `liga off`, `ss01`, `cv01=2`) parse via rustybuzz `Feature` and apply in `shape_run`, with `liga` forced
  on as a baseline like Ghostty. Config values are **comma-split only** (a feature's own value may contain
  spaces), tags must be **4 chars** (Ghostty's rule; rustybuzz would pad short tags into bogus features),
  and there is no `clear` keyword (empty value resets) — all confirmed against Ghostty by an adversarial
  review. A neutral `FontSpec` threads from `Config` through `render::init`/`build_resources` into
  `Atlas::new`. *Applied at startup — `font-family`/`font-feature` need a restart (config reload re-applies
  colors/size, not fonts).*
  **`font-family` chains and synthetic bold/italic are now done** — see their ledger entry below.
  **Still deferred:** `font-feature` values with **spaces around `=`** (`cv01 = 2`) — rustybuzz's
  `Feature::from_str` rejects those (Ghostty accepts them), a rare form.
- **Scrollback search** ✅ (Tier-2) `Ctrl+Shift+F` opens a top-bar overlay over the focused pane: the
  engine reads the whole screen (scrollback + viewport) to text with a char→column map
  (`engine::screen_text` / `RowText`); the pure `search.rs` finds matches (case-insensitive substring,
  toggleable) and tracks navigation; the session scrolls to the current match and maps visible matches to
  viewport rows; the renderer tints match cells (current brighter). Enter / Shift+Enter step
  next/previous, `Aa` toggles case, Esc closes. Modal for the keyboard like the palette.
  An adversarial correctness review found 5 bugs (all fixed): wide-char/CJK matches (spacer-tail cells
  were emitting a space — now skipped), long ZWJ-emoji clusters (>8 codepoints, heap-buffer retry),
  focus-stealing from a searching pane in splits, resize-while-open recapture, and a scroll-animation tick
  while the overlay is modal.
  *Limitation: ASCII case-folding in substring mode (regex mode folds Unicode). **Regex search is now done.** **Cross-wrap matches and eviction drift are now
  fixed** — see the search ledger below.*

- **fullscreen / split zoom / tab-inherit-cwd** ✅ (Tier-1 UX cluster) `toggle_fullscreen` (`ctrl+enter`)
  flips the winit viewport, reading the live fullscreen state back so an OS-driven change doesn't desync.
  `toggle_split_zoom` (`ctrl+shift+enter`) makes the focused split fill the tab (`Tab::zoomed` + a
  `Node::collect_leaf` that lays out only that leaf full-area), drawn with a green accent border;
  navigating to hidden siblings is suppressed and the zoom is dropped when the layout changes (split/close)
  or the zoomed pane is reaped. `tab-inherit-working-directory` (default true, like Ghostty) makes a new tab
  start in the focused pane's cwd; `window-inherit-working-directory` is recognized but inert (single-window).
  All three triggers match Ghostty's defaults.

- **P4 transparency + the opacity cluster** ✅ (the architectural prerequisite, plus everything it
  unlocked at Tier 1) `background-opacity` / `-cells`, `unfocused-split-opacity` / `-fill`,
  `cursor-opacity`, `faint-opacity` (0.55 → Ghostty's 0.5), and `background-blur` as a **real Windows
  DWM acrylic/mica backdrop** (`blur.rs`) — beyond upstream, which has no Windows blur.
  `engine::Cell` gained `bg_explicit`/`inverse`, so `render::bg_alpha` can mirror Ghostty's per-cell
  decision table exactly: default-background cells emit **no quad**, letting one translucent window
  fill show through, while selected / reverse-video / explicitly-colored cells stay opaque. Two silent
  Windows traps had to be solved and are now recorded in CLAUDE.md: DX12 HWND swapchains are
  opaque-only (needs `Dx12SwapchainKind::DxgiFromVisual`), and eframe's default clear alpha caps the
  whole window's transparency. Unfocused-split dimming is an egui overlay at `1 - opacity`, matching
  Ghostty's apprt rather than its renderer. Opacity values live-reload; *enabling* transparency needs a
  restart (as on Ghostty/macOS).
  *Known divergence: Ghostty's `isCovering` rule (full-block glyphs like `█` render opaque under
  transparency) is not implemented. Deferred: custom shaders. **background-image is done** — see
  its ledger entry below.*
  A **third** Windows requirement turned up only by running it: the window must be created with
  `WS_EX_NOREDIRECTIONBITMAP`, which upstream egui-winit never sets and which cannot be added after
  creation — so `vendor/egui-winit` is now a second vendored-and-patched crate (one-line delta, see
  CLAUDE.md). Without it the window is a solid grey wash. Transparency is **verified**: over a
  full-screen green backdrop at `background-opacity = 0.5` the composite reads `R = 0x08`, `B = 0x0c`,
  exactly `bg*0.5`. *Blur, dimming and the per-cell alpha details still want human eyeballing.*

- **Tier 1 complete** ✅ the last five items:
  - **`bell-features`** is now a real bitfield with Ghostty's packed-struct semantics (bare bool sets
    all; a feature list starts from *defaults* so it replaces rather than accumulates; one unknown
    token rejects the whole value). `system` → `MessageBeep`, `audio` → `PlaySoundW`, `attention` →
    taskbar `FlashWindowEx` while unfocused, `title` → 🔔 until refocus, `border` → the existing pane
    flash. New `src/bell.rs` declares user32 directly and resolves winmm lazily, adding **no**
    windows-sys features. Fixed a latent double-consume: `bell_flash_alpha` *takes* the pending flag,
    so the audible path needed its own, plus a 0.25 s rate limit against BEL storms.
  - **OSC 10/11/12 queries** ✅ new `src/osc_color.rs` scanner (the osc7/osc52 pattern) recording the
    caller's terminator, since the reply echoes it. Engine tests first **proved the *sets* already
    worked** — libghostty applies them and the snapshot reads effective colors — so only the `?` form
    needed building. Replies are built *after* the whole drain loop and merged into `take_responses`,
    so a set-then-query in one batch reports the new value. `osc-color-report-format`
    (`none`/`8-bit`/`16-bit`, ×257 scaling on the default).
  - **Resize overlay** ✅ `resize-overlay` / `-position` / `-duration`, with a reusable Ghostty
    `Duration` parser (additive number+unit pairs). Reuses the visual-bell transient pattern.
  - **confirm-close-surface** ✅ `false`/`true`/`always` with `needs_confirm` as a pure decision, an
    `egui::Modal` dialog, and — the real gap — **the OS/titlebar close is now honored at all**
    (`close_requested` + `CancelClose` in the same pass; nothing in giest handled it before).
  - **Tab drag-reorder** ✅ pure `drop_index` (centre-crossing) + `reorder_tabs`, an insertion caret,
    and two latent bugs fixed along the way (`renaming` and the drag latch both hold tab *indices*
    that a reorder or a reap invalidates).

These ship with unit tests (engine capture, keymap, parser, scan, X11/contrast/blink, font
feature/discovery, search match/nav/mask, wide-char read, zoom layout/reap, inherit-cwd,
bg-alpha table/branch-order, opacity parse+clamp, blur grammar, dim alpha, bell-features grammar +
rate limit, OSC color scan/report/fallback, duration grammar, resize gating + anchors, close
decision table, drop-index + reorder) — **208 lib + 24 conformance tests pass**. Adversarial
Ghostty-source reviews confirmed
parity across these feature areas; the only deliberate divergence is **RIS (`ESC c`)**: giest resets the
cursor to the configured `cursor-style`, whereas Ghostty resets to a plain block until a config reload
(arguably its own quirk). Rendering/interaction remain **perceptual** and need human confirmation in the
running app (per CLAUDE.md): eyeball cursor shape/blink, bold colors, min-contrast, unfocused-pane cursors,
and a configured `font-family` / `font-feature = -calt`; curly-underline thickness tuning still outstanding.

---

## 1. The gap, by category (what Ghostty macOS has that giest still lacks)

**VT / protocols** — kitty graphics (inline images); OSC 9/777/99 desktop notifications; OSC 9;4 progress;
OSC 4/5/13-19 color *queries*. *(OSC 8 hyperlinks, OSC 133 prompts, styled underlines, OSC 10/11/12
dynamic colors incl. query replies — now done.)*

**Rendering / fonts** — COLRv1 emoji; from legacy computing, the diagonal *fills*, the separated
blocks and the segmented digits.
*(`font-family` (+ fallback chains, per-style overrides, **synthetic bold/italic**),
`font-feature`/ligature toggle, **`font-variation`** and **`font-style`**, each with their three
per-style siblings, `minimum-contrast`,
`bold-is-bright`/`bold-color`, `cursor-style`/`-blink`, **transparency + background-opacity**,
blur (Windows acrylic), `faint-opacity`, `cursor-opacity`, unfocused-split dimming, **custom
shaders**, **box-drawing/block/braille/powerline sprites**, the **legacy-computing mosaics**
(sextants, octants, eighth blocks, smooth mosaics, triangles and the corner diagonals), the
**`adjust-*` metric family** *in full* (including `adjust-icon-height`) and `isCovering` — now
done.)*

**Window / UI** — titlebar/decoration styles; settings UI; about dialog; custom app icons.
*(the **inspector** is now done — see its ledger.)*
*(fullscreen toggle, split zoom, tab drag-reorder, resize overlay, confirm-close-surface,
**multi-window**, **scrollbar**, **`window-theme` + the chrome design system** (`theme.rs`:
tab strip, palette, overlays and dialogs all derive their colors from
`background`/`foreground`/`palette` instead of egui's defaults, and no longer follow the OS
light/dark preference), **window/tab/split state restore** (`window-save-state`), **quick
(dropdown) terminal + global hotkey** — now done.)*

**Input / keybinds** — nothing outstanding. *(Everything in this area is done: config-driven
binding, leader sequences, key tables, `catch_all`, `chain=`, all four trigger flags — `global:`,
`performable:`, `unconsumed:` and `all:` — the `write_*_file` / `set_*_title` / `toggle_*` /
`text:` / `csi:` / `esc:` actions, and now **`undo`/`redo` + `undo-timeout`**. The only remaining
gap against upstream's action union is **`show_gtk_inspector`**, which is GTK's own widget
inspector and has no Windows counterpart. giest's `inspector:` is now done.)*

**Selection / scroll / search** — upstream's 60%-of-cell threshold for including the clicked/dragged
cell; the double-click-*drag* word-snapping refinement. *(**regex search** now done — see its ledger; scrollback search plus
**upstream's five search actions** (`start_search` / `end_search` / `navigate_search:` /
`search_selection` / `search:`),
**cross-wrap matches and drift-free tracking**, **semantic selection**, the **full binding-backed
selection** — reflow-correct and scrollback-spanning — and **`adjust_selection` / rectangle
selection / drag-past-edge autoscroll** — now all done.)*

**Shell integration** — OSC 133 C/D (command output marks → duration, notify-on-command-finish).
*(OSC 133 A/B prompt marks now injected; `tab-inherit-working-directory` now honored — new tabs inherit
the focused pane's cwd, like splits. `window-inherit-working-directory` is recognized but inert until
multi-window lands.)*

**Clipboard / security** — nothing outstanding. *(`clipboard-read`/`-write` permission prompts,
paste-protection confirmation, `clipboard-trim-trailing-spaces` and **readonly mode + its
indicator** — now done; **secure input is N/A on Windows**, see its ledger.)*

**Config / theming** — `palette-generate`/`harmonious`, conditional configuration,
and the **82 upstream keys still unsupported** (mostly `gtk-*`, `macos-*`, `linux-*` — see the
config-surface ledger). *(theme/theme-file now done.)*

**Notifications / bell** — desktop notifications (OSC 9/777/99); Win taskbar progress (OSC 9;4);
`bell-audio-volume` (needs a real audio backend — `PlaySoundW` has no volume knob).
*(full `bell-features`: visual border, audible, taskbar attention, title marker — now done.)*

**Automation / platform** — AppleScript / App Intents / Services equivalents (Windows IPC), auto-update.

---

## 2. Binding leverage — the effort driver

✅ already exposed (just wire) · ◐ partial · ✋ needs a giest side-scanner (the OSC 7/52 pattern) or
upstream patch · — pure giest concern.

| Already in the binding (✅) | Needs side-scan / upstream (✋) | Pure giest work (—) |
|---|---|---|
| Underline style+color, faint, blink, overline, invisible *(now wired)* | OSC 9/777/99 notifications | font-family / atlas multi-face |
| OSC 8 hyperlink URIs *(now wired)* | OSC 9;4 progress | background opacity/blur/image |
| OSC 133 semantic-prompt marks *(now wired)* | OSC 10/11/12 dynamic colors | custom shaders |
| Kitty graphics (images, placements; cargo feature) | OSC 52 read *(now wired, security-gated)* | COLRv1 emoji, synthetic bold/italic |
| Kitty keyboard (auto-applied) | ~~scrollback regex search~~ ✅ | multi-window, quick terminal, fullscreen |
| Bell `on_bell` *(now wired)*, color-scheme, scrollbar geometry *(deliberately unused — see below)* | | resize overlay, settings UI |
| Rich selection model (`selection.rs`) | | |
| Tracked grid refs surviving scroll/reflow | | |

> Sixel: confirmed **not** supported by Ghostty either — out of scope.

---

## 3. Architectural prerequisites

- **P1 — Config registry (M).** ✅ Done — `SETTERS` table + theme resolution in `config.rs`.
- **P3 — Extend `Cell`/`GridSnapshot` (M).** ✅ Done — rich attrs threaded through `copy_cell` + renderer.
- **P2 — Action/keybind registry (L).** ✅ Done — `keybind.rs` + `Action::name/from_name`, keymap feeds
  both `handle_shortcuts` and `decide_key`.
- **P4 — Transparent surface + per-cell alpha (M).** ✅ Done — `main.rs` requests a transparent
  framebuffer (`with_transparent` **+ `Dx12SwapchainKind::DxgiFromVisual`**, without which DX12 is
  opaque-only) and `App::clear_color` clears to `[0,0,0,0]`; per-cell alpha comes from
  `render::bg_alpha` on the CPU with the values riding `TermFrame` (so they live-reload), and the
  emoji shader branch now honors the instance alpha. *Unlocked opacity, blur, unfocused dimming and
  background-image; custom shaders remain.*
- **P5 — Multi-window (L).** ✅ Done — `App` now owns `Vec<Window>`; `windows[0]` draws into
  `ViewportId::ROOT` and the rest are **immediate** child viewports. Deferred viewports were
  impossible (`Fn + Send + Sync + 'static` vs a `!Send` `Session`), and immediate ones turn out to
  make the PTY wake path work unchanged: a reader thread wakes the root, and a root pass re-runs
  every window. **No renderer changes** — eframe keeps one `RenderState` for all viewports, so the
  pipeline and glyph atlas are shared. *Unblocks the quick terminal and session restore.*

---

## 4. Tiered roadmap

Effort: **S** <1d · **M** 1–3d · **L** ~1wk · **XL** multi-wk. Status: ✅ done · ⬜ pending.

### Tier 1 — high impact, low/medium effort
| Gap | Files | Binding | Effort | Status |
|---|---|---|---|---|
| Underline styles + color | `engine/*`, `render/mod.rs` | ✅ | S–M | ✅ |
| faint / overline / blink / invisible | `engine/*`, `render/mod.rs` | ✅ | S | ✅ |
| theme / theme-file | `config.rs` | — | M | ✅ |
| OSC 8 hyperlinks | `engine/*`, `session.rs` | ◐ | M | ✅ |
| Bell (visual) | `engine`, `app.rs`, `config.rs` | ✅ | S–M | ✅ |
| jump/scroll to prompt (OSC 133) | `profiles.rs`, `engine`, `session.rs` | ✅ | M | ✅ |
| Wire more keybind actions | `command.rs`, `app.rs` | mixed | M | ◐ (systematically diffed against Ghostty's 85; the easy remainder is done — see below) |
| font-family/bold/italic | `config.rs`, `render/atlas.rs` | — | M | ✅ (chains/synthesis deferred) |
| font-feature / ligature toggle | `render/atlas.rs`, `config.rs` | — | S | ✅ |
| bold-is-bright / bold-color / minimum-contrast | `engine/ghostty_vt.rs`, `config.rs` | ✅ | S–M | ✅ |
| cursor-style / -blink / -opacity config | `config.rs`, `engine`, `decscusr.rs` | ✅ | S | ✅ |
| **background-opacity** (+ `-cells`) | `main.rs`, `render/mod.rs`, `config.rs` | — | M | ✅ |
| faint-opacity | `config.rs`, `render/mod.rs` | ✅ | S | ✅ |
| Fullscreen toggle | `app.rs`, `command.rs` | — | S | ✅ |
| Split zoom + equalize | `app.rs`, `command.rs` | — | M | ✅ (zoom; ratios + `equalize_splits` weighted by leaf count like `SplitTree.equalize`) |
| Tab reorder / drag | `app.rs` | — | M | ✅ |
| tab/split inherit working-directory | `app.rs`, `session.rs` | ✅ | S | ✅ |
| unfocused-split dimming (`-opacity`/`-fill`) | `app.rs` | — | S | ✅ (egui overlay, like Ghostty's apprt) |
| dynamic colors OSC 10/11/12 | `osc_color.rs`, `session.rs` | ✋ | S–M | ✅ (sets already worked; queries added. OSC 4/5/13-19 queries deferred) |
| confirm-close-surface | `app.rs`, `config.rs` | — | S | ✅ (+ the OS close, previously unhandled) |
| resize overlay | `app.rs`, `session.rs`, `config.rs` | — | S | ✅ |
| audible bell (+ full `bell-features`) | `bell.rs`, `app.rs`, `config.rs` | — | S | ✅ |

### Tier 2 — high impact, higher effort (flagships)
| Gap | Files | Binding | Effort | Status |
|---|---|---|---|---|
| Full configurable keybinds (tables/sequences) | `keybind.rs`, `app.rs`, `session.rs` | ✅ | L | ◐ (chords + `>` sequences done; named key *tables* still open) |
| Quick / dropdown terminal | `quickterm.rs`, `hotkey.rs`, `app.rs` | — | L | ✅ (+ `global:` keybinds — see the ledger) |
| Kitty graphics (inline images) | `engine`, `GridSnapshot`, `render/mod.rs` | ✅ | L–XL | ✅ with a sideloaded ConPTY (rendering needs eyeballing); ⬜ over the inbox conhost, which strips APC — see below |
| Scrollback search overlay | `search.rs`, `session.rs`, `app.rs`, `engine`, `render` | ✋ | M–L | ✅ (single-row matches; pins deferred) |
| Custom shaders | `shader.rs`, `render/mod.rs`, `config.rs` | — | L | ✅ (Shadertoy GLSL→WGSL, offscreen chain, both keys; verified end to end) |
| background-image / blur | `bgimage.rs`, `render/mod.rs`, `config.rs` | — | M–L | ✅ (blur via Windows DWM acrylic/mica, `blur.rs`; image via `bgimage.rs` + shader mode 4) |
| Multi-window (`new_window` / `close_window`) | `app.rs`, `command.rs`, `keybind.rs` | — | L | ✅ |
| Session / window state restore | `app.rs`, `state.rs` | ◐ | L | ✅ (`window-save-state`; layout only — see the ledger) |
| Clipboard permission + paste protection | `session.rs`, `app.rs`, `config.rs`, `clipboard.rs` | ✋ | M | ✅ |
| Desktop notifications + notify-on-command-finish | `osc_notify.rs`, `notify.rs`, `osc133.rs`, `profiles.rs`, `session.rs`, `app.rs` | ✋ | M | ✅ (both halves; cmd can't report an exit code — see the ledger) |
| Real scrollbar widget | `scrollbar.rs`, `app.rs`, `session.rs` | ✅ | M | ✅ |
| Migrate to binding selection model | `session.rs`, `engine` | ✅ | M–L | ✅ (tracked-ref anchor, scrollback-spanning, reflow-correct, plus rectangle mode and drag autoscroll — see the ledgers) |

### Tier 3 — long tail / platform-specific
auto-update · about dialog /
custom icon · AppleScript / App-Intents / Services → Windows IPC ·
COLRv1 emoji · grapheme-width ·
window decorations / titlebar / colorspace · settings UI. *(all ⬜; the **inspector** is
done — see its ledger.)*

**Done from this tier:** window geometry (`window-width`/`-height` in cells, `window-position-x`/`-y`)
plus `toggle_maximize`, `toggle_window_float_on_top` and `toggle_background_opacity` — the last three
are all documented upstream as macOS-only or macOS-ineffective, and Windows supports every one, so
giest implements them regardless of which side Ghostty left them on. Notes:

- **Geometry is applied on the first frame, not at window creation.** The size is in *cells*, and
  cell metrics don't exist until the glyph atlas is built — which happens after the window. Doing it
  from the layout also makes the chrome exact: the tab strip's height is whatever the layout didn't
  give the terminal, rather than a guessed constant. It fires **once**; re-applying would fight the
  user every time they resized.
- **`window-position-*` is divided by `pixels_per_point`.** Ghostty documents the position in
  pixels; egui's viewport commands are in points. Passing the number through unchanged landed the
  window 25% off at 125% scaling — *measured*, then fixed, then measured again at exactly the
  requested coordinate.

**Also done from this tier:** `write_scrollback/screen/selection_file` (`writefile.rs`),
`mouse-hide-while-typing`, `mouse-reporting` +
`toggle_mouse_reporting`, `mouse-scroll-multiplier`, `scroll-to-bottom`, `focus-follows-mouse`
(the mouse/scroll cluster), and OSC 9;4 taskbar progress. Notes on the cluster:

- **`mouse-reporting` gates `is_mouse_tracking`, not the engine.** A program can still *request*
  tracking; nothing is sent. That's what makes `toggle_mouse_reporting` an escape hatch from a
  full-screen app that grabbed the pointer, rather than something that confuses the app's state.
  It's per-pane, and a config reload re-applies the key — a sticky runtime toggle surviving an
  explicit reload would be indistinguishable from the key not working.
- **`focus-follows-mouse` requires actual pointer movement.** Hover alone is a *state*, so a parked
  cursor would drag focus back every frame and make `focus_split_*` keybinds impossible to use.
- **`mouse-scroll-multiplier` maps egui's wheel units onto Ghostty's two device classes**:
  `Line`/`Page` are discrete, `Point` is precision.
- **`write_*_file`'s parameter acts on the file *path*, not the contents** — `copy` clipboards the
  path, `paste` types it, `open` shells it. That is Ghostty's design and the reason the feature is
  useful, but it reads as a bug if you assume otherwise, so it is spelled out in the guide. The
  parameter is required; there is no default, so a typo fails to bind rather than doing something
  unasked. `paste` routes through `Session::paste_str` like every other paste, so
  `clipboard-paste-protection` still applies — a temp path is always safe, but routing around that
  gate is how the next caller that *isn't* ends up bypassing it too.
- **Deferred: Ghostty's `vt` and `html` output formats.** giest writes `plain` only; the parameter
  grammar has no room for a format today, so this is a gap rather than a divergence.

---

## 5. Recommended next steps

With Phase 0 done, the remaining Tier-1 items are mostly small, registry-backed one-liners. Suggested order:
1. ✅ **cursor-style / bold-is-bright / minimum-contrast** config (binding ✅, rides P1) — *done* (needs eyeballing).
2. ✅ **font-family / font-feature** (atlas multi-face) — *done* (needs eyeballing; chains/synthesis deferred).
3. ✅ **P4 transparent surface** → **background-opacity** + **unfocused-split dimming** +
   **cursor-opacity** + **faint-opacity** + **background-blur** (Windows acrylic) — *done; the whole
   cluster still needs eyeballing, and transparency can't be self-captured (see CLAUDE.md).*
4. ✅ **fullscreen** + **split zoom** + **tab inherit-cwd** — *done* (fullscreen/zoom need eyeballing).
5. ✅ **Finish Tier 1** — `bell-features` + audible/attention/title bell, OSC 10/11/12 query replies,
   resize overlay, confirm-close-surface (+ the previously-unhandled OS close), tab drag-reorder.
   **Tier 1 is now complete.**
6. ✅ **P5 multi-window** — immediate viewports in one process, `new_window`/`close_window`.
7. ✅ **Kitty graphics** — end to end over a sideloaded ConPTY (`conpty-passthrough`); the inbox
   conhost still strips APC. *(Needs eyeballing.)* See below.
8. ✅ **Real scrollbar** — `scrollbar = system | never`, auto-hiding overlay, draggable.
   *(Needs eyeballing.)* See the divergence ledger below.
9. ✅ **Clipboard permissions + paste protection** — all five Ghostty keys, one gated paste path,
   and OSC 52 read now answerable under permission. See the ledger below.
10. ✅ **background-image** — all five Ghostty keys, PNG + JPEG, Ghostty's fit/position math and its
    compositing formula. *(Needs eyeballing.)* See the ledger below.
11. ◐ **Desktop notifications** — `OSC 9` / `OSC 777` + `desktop-notifications`, as Windows toasts.
    `notify-on-command-finish` still open: it needs OSC 133 **C/D**, and the shell hooks in
    `profiles.rs` inject only A/B today. See the ledger below.
12. ✅ **OSC 9;4 progress → Windows taskbar** (+ `progress-style`) — Tier 3, but it rides the OSC 9
    parser the notifications just built and is squarely Windows-native. See the ledger below.
13. ✅ **Session / window state restore** — `window-save-state`, a new `state.rs`. See the ledger below.
14. ✅ **Quick (dropdown) terminal + `global:` keybinds** — `quickterm.rs`, `hotkey.rs`. See below.
15. ✅ **Sprite glyphs** — box drawing (complete), blocks, braille and the geometric powerline
    separators drawn from the cell metrics (`sprite.rs`), plus `adjust-box-thickness`. See below.
16. ✅ **The `adjust-*` metric family + `isCovering`** — font-derived decoration metrics, all
    twelve keys. See the ledger below.
17. ✅ **Font fallback chains + synthetic bold/italic** — repeatable `font-family`,
    `font-synthetic-style`. See the ledger below.
18. ✅ **Semantic selection** — word / soft-wrapped line / command output from the binding, plus
    `selection-word-chars`. See the ledger below.
19. ✅ **Config-surface batch** — 11 keys found by a measured key diff. See the ledger below.
20. ✅ **`config-file` (include)** — recursive config loading. See the ledger below.
21. ✅ **`enquiry-response` closed out as blocked** — the ConPTY probe was written and run; ConPTY
    strips ENQ. See the config-surface ledger.
22. ✅ **The full selection migration** — engine-owned, tracked-ref, scrollback-spanning. See the
    ledger below.
23. ✅ **Search cross-wrap matches + drift-free tracking.** See the ledger below.
24. ✅ **`adjust_selection` + the `performable:` flag, rectangle selection, drag-past-edge
    autoscroll.** See the ledger below.
25. ✅ **Read-only indicator**, and **secure input closed out as N/A** on Windows. See the ledger.
26. ✅ **`text:`/`csi:`/`esc:`/`set_*_title:`**, on an `Action` that now owns its strings. See the
    ledger.
27. ✅ **Named key tables**, plus the `ignore`-vs-`unbind` fix they surfaced. See the ledger.
28. ✅ **`catch_all` and the `unconsumed:` trigger flag.** See the ledger.
29. ✅ **`chain=` multi-action bindings.** See the ledger.
30. ✅ **`all:` / broadcast input** — one feature under two names. See the ledger.
31. ✅ **`undo`/`redo` + `undo-timeout`** — close and creation both reversible, with the shells
    still running. See the ledger below.
32. ✅ **The five search actions** — `start_search` / `end_search` / `navigate_search:` /
    `search_selection` / `search:`, plus `escape=end_search` and a search bar that resolves its
    keys through the keymap. See the ledger below.
33. ✅ **The terminal inspector** — `inspector:toggle|show|hide` on `ctrl+shift+i`, with the
    keyboard and PTY logs the Windows-specific debugging actually needs. See the ledger below.
34. ✅ **`font-variation`** and its three per-style siblings — variable-font axes on both the
    rasterizer and the shaper. See the ledger below.
35. ✅ **`adjust-icon-height`** — the last of the `adjust-*` family. See the ledger below.
36. ✅ **Legacy-computing mosaics** — sextants, octants and the eighth/quarter blocks. See the
    ledger below.
37. ✅ **The diagonal legacy-computing families** — smooth mosaics, edge triangles, shaded corner
    triangles and the corner diagonal lines. See the ledger below.
38. ✅ **The `font-style` family** — named styles and style disabling. See the ledger below.
39. ✅ **Upstream re-audit + libghostty bump to Ghostty `main`** — see the full parity plan at the top.
40. Next: follow §E of the full parity plan at the top (IME, child-exited bar, config-errors
    dialog, OSC 99), then the split model.

### `font-style` and its three siblings — ✅ divergences

The last unfinished part of font *selection*. Each slot takes `default`, a style **name**, or
`false`.

- **A named style replaces the bold/italic test rather than adding to it**, and upstream says
  exactly why: "if a user says `font-style = italic` for the bold face, no results would be found if
  we restrict to ALSO searching for italic". So `font-style-bold = Heavy` finds the Heavy face
  whatever its style bits claim.
- **The match reads the *typographic* subfamily as well as the standard one, and that is what makes
  the feature work at all.** Measured on Windows' own Segoe UI Semibold: name ID 1 is "Segoe UI
  Semibold", ID 2 is **"Regular"**, and only IDs 16/17 carry "Segoe UI" / "Semibold". A family
  shipping more than four weights has to squash them into ID 2's Regular/Bold/Italic/Bold-Italic
  buckets and puts the real name in ID 17 — so checking ID 2 alone would fail on precisely the fonts
  someone writes this key for. The test asserts it against that font.
- **`false` disables the style; it does not synthesize one.** A program asking for a disabled style
  gets the regular face. That is the whole difference from `font-synthetic-style` — that key decides
  how a *missing* style is faked, this one says the family has no such style — and getting them
  confused would produce a slanted regular where the user asked for no italics at all. Upstream
  implements it the same way (`styles.set(.italic, config.@"font-style-italic" != .false)`).
- **Disabling works with no `font-family` set**, which needed care: two paths in `resolve_slots`
  return the built-in font early, and both now run the slots through `disable_styles` first.
  Upstream is explicit that this is the one case where `font-style` applies without a family, and it
  is the case someone reaches for — "never italicise my terminal" shouldn't depend on whether a
  custom font was found.
- **A style name may contain spaces** (`Light Italic` is a real subfamily), so only the surrounding
  whitespace is trimmed — and `true` is a *name*, not the opposite of `false`: upstream gives it no
  meaning and a font could advertise it.

**Divergences:**

- **A localized style name matches too.** The name table carries one entry per language, and the
  lookup scans all of them — so on a German system `font-style-bold = Fett` finds the bold face.
  Upstream matches the platform's own style string, which is likewise localized; this is the same
  behaviour reached by a different route, and it can only ever match *more*.
- **Applied at startup**, like every other font key.

**Verified**: `cargo test` — the three-way parse (including the space-carrying name, `true` as a
name, and the reset), per-slot independence, the typographic-subfamily match against a real Segoe UI
Semibold, and that a disabled style lands on the regular face with no synthesis both when a family
resolves and when it doesn't. The *appearance* of a named weight wants a human look.

### Legacy computing (mosaics *and* diagonals) + `adjust-icon-height` — ✅ divergences

Two Tier-3 items that finish what was already built. `adjust-icon-height` completes the `adjust-*`
family; the mosaics extend `sprite.rs` with the families that share its existing primitive.

**Drawn now:** U+1FB00–1FB3B **sextants** (2×3), U+1CD00–1CDE5 **octants** (2×4, the supplement
block), and U+1FB70–1FB97 — the eighth and quarter blocks, their L-shaped corners, the medium-shaded
halves, the checkerboards and the heavy horizontal fill. ~330 characters.

- **This is the case sprite drawing exists for, at its most extreme.** A picture rendered by `chafa`
  or `timg` is thousands of mosaic cells that have to meet exactly; a font's versions are drawn to
  its em box, so at any line spacing the image comes out with a grid of hairlines through it. The
  no-seam property is asserted as a *property* rather than eyeballed: at six cell sizes that divide
  evenly by neither 2, 3 nor 4, every pixel of every sextant and octant is fully on or fully off,
  and two complementary octants cover every pixel between them.
- **The fraction rule deliberately overlaps rather than risking a gap**, and a test tried to assert
  the opposite before the arithmetic corrected it. `frac_min`/`frac_max` resolve the same fraction
  to *different* pixels (upstream spells out why: at size 7 both halves come out 4px, `0..4` and
  `3..7`), so adjacent blocks share a pixel. Asserting a partition would have been asserting the bug
  the rule exists to avoid; the test asserts the union instead.
- **The sextant numbering is ported, not re-derived.** The block holds the 60 patterns that aren't
  already characters — everything but empty, full, `▌` and `▐` — and the two missing ones are in the
  *middle* of the range. Upstream's `idx + idx / 0x14 + 1` steps over them; a rederivation that
  disagreed would silently shift 40 characters, so the whole 60-long sequence is checked against the
  two exclusions.
- **The octant table is vendored, for the reason upstream states outright.** Its `octants.txt` says
  "we weren't able to discern a mathematical pattern for them" — the block omits 26 of the 256
  patterns and their *order* is not derivable. `src/res/octants.txt` is that file, parsed at first
  use, the same call giest already makes for `rgb.txt`. The test asserts the table's shape (230
  entries, no duplicates, none empty or full) rather than its contents.
- **`adjust-icon-height` adjusts only the ceiling.** Nothing else moves with it — the grid, the text
  and the decorations are unchanged — so bigger icons don't reflow the row; and only the *height*,
  letting the aspect ratio carry the width, which is what makes it read as "bigger icons" rather
  than stretched ones. Clamped like a thickness rather than free like a position: a ceiling of zero
  would make every icon vanish, which reads as a missing glyph.
- **Upstream's "Powerline symbols are not affected by `adjust-icon-height`" falls out by
  construction.** Box-drawing, block and Powerline glyphs are drawn procedurally from the cell
  metrics rather than rasterized, so they never consult the icon ceiling — no exception needed.

**The diagonal families, added after the rectangle ones** (U+1FB3C–1FB67 smooth mosaics,
U+1FB68–1FB6F edge triangles, U+1FB9A–1FB9F opposed and shaded triangles, U+1FBA0–1FBAF corner
diagonal lines — 74 more characters). They reuse the supersampled polygon fill the rounded corners
already needed, so no new primitive was required.

- **The mosaic table was extracted from Ghostty's source, not transcribed.** 44 patterns of four
  three-character rows is exactly the shape of table where one wrong character is invisible in
  review and produces a single subtly wrong glyph. A script pulled them out and checked the
  codepoints were contiguous across the whole range before anything was written down. Upstream's own
  note on the table: "Hand written lookup table for these shapes since I couldn't determine any sort
  of mathematical pattern in the codepoints."
- **A mid-edge point is a vertex only when the shape turns there.** Upstream's `SmoothMosaic.from`
  guards each of the six side points with "…and its neighbours aren't both filled". Skipping that
  would put a redundant vertex on a straight edge, which a polygon fill renders as a notch rather
  than ignoring. It has its own test, because the shapes it breaks are a minority of the 44.
- **The four inverse triangles are drawn as polygons, not by inverting the canvas.** Upstream fills
  the triangle, inverts, then re-clips to the cell; the complement of an edge triangle within a
  rectangle is just a pentagon, and giest's canvas has neither invert nor clip. Verified by the
  property that matters: the triangle and its inverse sum to exactly full coverage at every pixel —
  which they do, unlike the *block* mosaics, because they share one exact boundary and the fill
  computes real areas.
- **A shaded triangle scales the polygon's coverage rather than filling at a flat value.** Filling
  at half brightness would give a shape with a hard, aliased edge; multiplying keeps the
  anti-aliasing and matches how `░▒▓` already work.
- **The corner-diagonal centre rounds *up* on an odd cell** (`n / 2 + n % 2`, upstream's), which is
  what makes all four diagonals of `🮮` cross at one pixel instead of in a two-pixel knot.

**Still not drawn**, so still from the font: the diagonal *fills* (U+1FB98/1FB99), the separated
blocks and the segmented digits — repeating patterns and digit segments rather than a shape.

**Other divergences:**
- **U+1FB93 is claimed and drawn empty.** It is an unassigned hole in the block; upstream draws
  nothing there too, and the alternative is a tofu box for a codepoint that has no character.
- **The checkerboards use upstream's aspect-corrected grid** (four columns, `round(4·h/w)` rows) so
  the squares are square, rather than a 4×4 grid stretched to a 1:2 cell.

**Verified**: `cargo test` — the sextant sequence in full, the octant table's shape and two spot
entries, exact pixel coverage for a sextant and an octant, the no-seam property at six awkward cell
sizes, the eighth-block positions, the shade being coverage rather than a dither, and
`adjust-icon-height` moving the ceiling and nothing else. For the diagonals: the whole extracted
table's shape, the redundant-midpoint rule, every one of the 44 mosaics rendering something that
isn't the whole cell, one mosaic's wedge by computed corners, the triangle/inverse partition at
every edge, and the 15 corner-diagonal combinations being exactly the non-empty subsets. **Wants a human look**: an actual image
piped through `chafa` — the geometry is proven, how it reads is not.

### `font-variation` (variable fonts) — ✅ divergences

All four keys (`font-variation`, `-bold`, `-italic`, `-bold-italic`), parsed to Ghostty's grammar
and applied to the parsed faces at atlas construction.

- **Both faces, or neither.** giest keeps two views of each font: `ab_glyph::FontRef` rasterizes the
  outline and `rustybuzz::Face` decides the advance. A variation set on one alone draws glyphs of
  one weight on spacing computed for another, which reads as bad kerning rather than as a broken
  feature — so `apply_variations` sets both, and the test **measures** it: Segoe UI Variable's `M`
  advance moves 43.207 → 46.050 at `wght` 300 → 700, and the shaped advance has to move with it.
- **Applied before the metrics are derived.** An axis like `wght` or `wdth` changes advance widths,
  which is what the cell size is computed from. Ordering this the other way would give a grid sized
  for the font's default instance and glyphs drawn for the configured one.
- **The grammar is *not* `font-feature`'s, and the difference is load-bearing.** Upstream trims
  whitespace around both halves of `id=value` (so `wght = 350` is valid, where the same spacing is
  rejected for `font-feature` — there the value goes to rustybuzz's stricter parser), and it takes
  **one axis per occurrence** with no comma splitting. Accepting `wght=350, wdth=90` here would bind
  a config a real Ghostty rejects, so it is rejected, with a message.
- **The style slots do not inherit, which is upstream's behaviour and surprising.**
  `SharedGridSet.zig` hands each style descriptor only its own key's list, so `font-variation` alone
  leaves the bold face at the font's default weight. Pinned by a test so it can't quietly drift into
  inheritance, and called out in the guide, because it is the first thing someone will report as a
  bug.
- **The user's `font-family` chain gets the base variations; the system fallbacks do not.** Upstream
  builds a descriptor from *every* entry in `font-family` and gives each `font-variation`, so the
  chain matches. Nothing configured the system fallbacks, and an axis meant for a coding font has no
  business reshaping the emoji face that happens to share a tag name.
- **An unknown axis is reported once and skipped.** Upstream says "invalid ids and values are
  usually ignored", and a config shared between machines will name axes some installed fonts lack —
  but silence would make a typo in the tag indistinguishable from a font that doesn't have it, and
  that is the most likely way to get this wrong.

**Divergences:**

- **An out-of-range value is silently ignored, not clamped** — upstream's documented behaviour
  ("setting `wght=800` will do nothing") and, here, also a limit: neither backend reports whether a
  value was in range, so giest cannot warn about it the way it warns about an unknown axis.
- **The bundled font has no variation axes**, so the feature does nothing until `font-family` points
  at a variable font. That is a property of the shipped asset rather than of the feature, and it is
  asserted in a test: swapping the bundled JetBrains Mono for a VF fails there, and the
  documentation gets revisited with it.
- **Applied at startup**, like every other font key — a config reload re-applies colors and sizes,
  not fonts.

**Verified**: `cargo test` — the parser (trimming, repetition, the rejected comma list, the reset,
and six malformed forms), the per-slot independence, and the end-to-end axis effect measured on
Segoe UI Variable through `apply_variations`, asserting the rasterizer *and* the shaper moved. The
appearance of a variable font at a non-default weight still wants a human look.

### The terminal inspector — ✅ divergences

`inspector:toggle|show|hide` on `ctrl+shift+i` (upstream's own trigger, whose comment records it as
"matching Chromium"), drawn as an egui window over the pane it belongs to. New `src/inspector.rs`
holds the capture — bounded ring buffers and the byte rendering — and is pure and unit-tested;
`app.rs` draws it.

Four sections against upstream's five ImGui windows: **Surface** (grid, cell and pane geometry,
font size, DPI), **Terminal** (cursor, scrollback, resolved colors, mouse tracking, selection,
read-only, kitty placements, title, cwd), **Keyboard** and **Terminal IO**.

- **The two logs are the point, and they are Windows-shaped.** Upstream's `termio` window shows
  libghostty's *parsed* VT actions; giest's engine is behind a `write(&[u8])` trait, so what it can
  show is the byte stream — which turns out to be the more useful half here. ConPTY does not pipe a
  child's output through: it parses the VT and emits its *own* stream, silently dropping sequences
  it doesn't understand (APC, and with it kitty graphics). CLAUDE.md's standing advice for any
  escape-sequence work is to first confirm the bytes arrive, by adding a temporary probe to
  `GhosttyVtEngine::write`. This is that probe, made permanent and readable.
- **The keyboard log prints chords in *config spelling*.** Its question is "what do I put after
  `keybind =` to bind this?", so a `Debug` rendering (`ArrowLeft`) would be the wrong answer where
  `left` is the right one. That needed `keybind::key_name` as the inverse of `key_from_name` and a
  `Chord::name`, pinned by an **exhaustive** round-trip test over `KeyCode::ALL` × four modifier
  sets: one wrong row would be a chord the inspector prints and the config then refuses to bind.
- **Capture is taken at the two chokepoints that know the whole answer.** Keys are logged in
  `handle_input` right after `decide_key`, the only point that has the chord, the decision *and* the
  bytes the encoder produced — earlier and it couldn't report the encoding, later and a *swallowed*
  key wouldn't appear at all, which is exactly the press someone opens the panel to explain. Reads
  are logged in `pump_pty` **before** `engine.write`, the last honest view of the stream.
- **It costs nothing while closed.** The log is an `Option` on the `Session`, `None` until an
  inspector is opened on that pane, so both record calls are a null check. That is also why it is
  per-pane state rather than the window's — and it matches upstream, whose inspector is per-surface.
- **It is deliberately not a modal.** Every other overlay in giest takes the keyboard (CLAUDE.md's
  two-gate rule); this one is in *neither* gate, because a keyboard log you cannot type into and an
  IO log over a program you cannot watch redraw would both be useless. The pointer is a different
  matter: the pane underneath still hit-tests, so a click on `Pause` would also start a text
  selection. `Window::inspector_rect` — last frame's rect, the `last_layout` idiom — withholds the
  pointer (and the scrollbar's) where the panel is.
- **Every control byte gets its own glyph** (`␛`, `␇`, `␍` from the Control Pictures block), unlike
  `app::preview_text`, which collapses them onto one `␦`. That one is showing a human what they are
  about to paste; here the whole question is *which* control arrived. Invalid UTF-8 shows as `\xNN`
  rather than U+FFFD for the same reason — a replacement character would read as corruption in the
  terminal rather than in the byte stream.

**Divergences from upstream:**

- **No parsed VT actions, no DEC mode table, no per-cell or pagelist browsing, no renderer
  statistics.** All four read state the binding does not surface (`libghostty-vt` exposes no mode
  enumeration and no parser event stream), so they would be invented rather than reported.
- **Four collapsing sections, not five dockable windows.** giest has no docking, and five floating
  ImGui-style windows over a single terminal pane would be unusable at a terminal's size.
- **`show` on an already-open inspector keeps the capture**, rather than restarting it — the same
  rule `start_search` follows, and the one branch in the feature, so it has its own test.
- **Closing drops the capture.** Nothing quietly retains a buffer of the user's terminal output
  after the panel is closed, which is also what makes the closed cost zero.
- **`show_gtk_inspector` is not accepted**, even as a no-op: it is GTK's *widget* inspector, a
  different thing from the terminal one, and there is no Windows counterpart to point it at.

**Verified**: `cargo test` — the byte rendering (controls, DEL, UTF-8, invalid bytes, a real
`ESC [ 2 J`, and the cap counting input bytes), the ring buffers dropping the oldest, pause and
clear semantics, the mode decision, action round-trips and the default binding, plus the exhaustive
chord round-trip. **The panel itself needs a human look** — an attempt to capture it failed for the
reason CLAUDE.md records: synthetic keyboard input cannot take the foreground here, so the chord
never reached the app. Open it with `Ctrl+Shift+I`, type into the pane and watch both logs fill.

### The search action family — ✅ divergences

Upstream's five (`start_search`, `end_search`, `navigate_search:next|previous`, `search_selection`,
`search:<text>`) now sit alongside giest's own `toggle_search`, all bindable, all with upstream's
`performable:` semantics ported from `Surface.zig`'s return values rather than guessed at.

- **`escape` is bound to `end_search`, and `performable:` is what makes that safe.** This is
  upstream's non-Darwin default and the sharpest example of the flag in the whole table: with a
  search open the key closes the bar, with none open the action reports "not performed" and the key
  belongs entirely to the program. Without it the bind would swallow Escape in vim, every pager and
  every TUI — so it is asserted against the *real* default keymap in `session.rs`, both directions,
  rather than a constructed one.
- **The search bar now resolves its keys through the keymap.** It is modal — `run_pass` skips
  `handle_shortcuts` while it is up — so `navigate_search` bound to a key would have been dead
  exactly when it is wanted, and Escape closed the bar only because the close was *also* hardcoded,
  which made `keybind = escape=unbind` a lie. `Window::search_overlay_actions` is the restricted
  pass: it resolves each chord and runs only the search family, leaving every other bound chord
  swallowed. That is the same principle already recorded for the scrollback keys — a key handled
  only by a hardcoded branch can be rebound but never turned off.
- **A modifier-less printable key is left to the text field.** Upstream's search entry holds the
  keyboard the same way, so this is parity rather than a shortcut: a binding on a bare letter
  belongs to whoever is typing a query. `session::produces_text` (already written for the
  swallowed-key suppression) is the predicate, reused rather than re-derived.
- **`search:` had to go in the *payload* table, and the bare `search` alias had to go.** giest
  accepted `search` as a synonym for `toggle_search`; upstream's `search` takes a needle
  (`search:foo`). Keeping both would mean a transferred Ghostty config binding `search` silently did
  something else. The prefixes don't overlap with `search_selection`, so the two coexist.
- **An empty `search:` payload stops the search without hiding the bar**, which is upstream's split
  and the reason it isn't folded into `end_search`. Its `performable:` gate follows: an empty needle
  with nothing running has nothing to stop.
- **`start_search` on an already-open search is a no-op, not a recapture.** Upstream: "Start a
  search if it isn't started already." Re-opening would throw away a query the user is mid-way
  through typing — and `start_search` is precisely what someone binds when they also bind
  `end_search`.

**Divergences:**

- **`toggle_search` stays the `Ctrl+Shift+F` default**, rather than upstream's `start_search`.
  giest's toggle predates the pair and is a superset; a single key that both opens and closes is
  what Windows users reach for, and `start_search` is bindable for anyone who wants the split.
- **`search_selection` uses only the first line of a multi-line selection.** giest's search matches
  within a single (soft-wrapped) row, so a multi-line needle could never match — taking the first
  line finds something rather than nothing.
- **No default `navigate_search` binding.** Upstream binds `super+g`/`super+shift+g` on macOS only;
  its non-Darwin defaults have none either, and Enter / Shift+Enter in the bar already step. The
  action is bindable.
- **The needle from `search_selection` goes through `selection_text`**, so
  `clipboard-trim-trailing-spaces` applies to it. A needle with an invisible trailing space would
  match nothing and read as a broken feature.
- `search:` has no regex flag (upstream has no regex search at all); it sets a
  needle like anything typed into the bar, interpreted per the bar's current `.*` toggle.

**Verified**: `cargo test` — round-trip parsing for all five (including the empty-needle form and
the rejected bare `search`), the `can_perform` gate for each, and the Escape-reaches-the-program
assertion on the default keymap. The overlay's own key handling is interactive and wants a human
look: Escape and Ctrl+Shift+F should still close the bar, and Enter / Shift+Enter should still step.

### `undo` / `redo` — ✅ divergences

`Ctrl+Shift+Z` / `Ctrl+Shift+Y`, backed by a new `src/undo.rs`: a two-stack manager whose entries
**expire** on `undo-timeout` (Ghostty's key, default 5 s, `0` disables). Undoable: close split /
tab / window, close other tabs, close tabs to the right, and — as upstream also does — the
*creations*: new split / tab / window, whose undo closes them again. That is Ghostty's whole set
minus `move_split` (giest has no split move) and the app-quitting `close_all_windows`.

- **A restore is lossless, and that is the whole design.** Ghostty's undo retains the live
  `SurfaceView` tree, so undoing a close brings back the *running* terminal. giest does the same
  by construction: `Node::detach_leaf` **moves** the removed subtree into the undo entry (where
  `remove_leaf` used to drop it — that function is gone, its one caller converted), and closed tabs
  and windows are moved rather than dropped too. Scrollback, the running command and the cwd all
  come back. It is also why entries must expire: **an undo entry holds live processes**.
- **The inverse is produced by performing, not by a table.** `App::apply_undo_op` returns the op
  that reverses what it just did, and `UndoStack` files that return value onto the *opposite* stack
  based on the phase it is in — exactly `NSUndoManager`'s `isUndoing`/`isRedoing` rule. So undo,
  redo, and undoing a creation are one implementation walked in different directions; there is no
  second opinion about what "the opposite" is, which is the failure mode a hand-written redo table
  has. The six ops pair off: `RestorePane`/`RemovePane`, `RestoreTabs`/`RemoveTabs`,
  `RestoreWindow`/`RemoveWindow`.
- **A closed pane's slot is a path, not a neighbour.** The sibling a removed pane collapses into may
  be a whole subtree, so naming one of its leaves wouldn't say which ancestor to wrap. `PaneSlot`
  carries the root-downwards path to the old *parent split*, its axis and which side the pane was
  on. If the layout changed under the entry and the path no longer resolves, `attach_at` attaches
  at the deepest point it can reach rather than declining — a pane restored in the wrong place is
  recoverable; a dropped one takes a running shell with it.
- **Tabs needed a stable identity.** Everything in `app.rs` addresses tabs by *index*, and this
  repo's recorded trap is that a reorder or a reap invalidates one. That is survivable for state
  held within a frame; an undo entry outlives far more than a frame. `Tab` now carries an `id` from
  the same per-window counter as the leaf ids (reusing a leaf's number, which cannot collide with
  another tab's), and every undo op addresses tabs by it.
- **`reap_dead` is the one close that is deliberately *not* undoable.** A window whose last shell
  exited has nothing to restore, and an entry would hold a window of dead panes. `AppRequest::CloseWindow`
  therefore carries an `undoable` flag rather than being one shape for both callers.
- **Closing the last tab is re-routed rather than double-recorded.** It used to remove the tab and
  then ask for the window to close, which would have produced *two* entries (a tab restore into a
  window that no longer exists, and a window restore). It now hands the close straight to the
  window path with the tab still in place, so there is one entry: the whole window.
- **Undo can never quit giest.** Undoing the creation of what is now the only window would have to
  close it, which is how giest exits; `RemoveWindow` declines instead. Same rule for `RemoveTabs`
  when it would empty a window — that is a *window* close, a different op with a different entry,
  and silently escalating into one would surprise.

**Divergences from upstream:**

- **A restored window comes back as an ordinary child window**, at the position and size it had
  (a one-shot `place_geom` consumed on the next pass, the same mechanism the root-slot rehost uses),
  rather than reclaiming the root viewport slot. Reclaiming it would move a *different* window on
  screen — the one that slid into slot 0 when this one closed — which is a worse surprise than the
  window coming back stacked.
- **No action names.** Upstream sets "Undo Close Tab" etc. because macOS shows it in the Edit menu.
  giest has no Edit menu, so the palette lists a plain `Undo` / `Redo` and the names aren't carried.
- **The stacks are capped at 64 entries**, oldest dropped. Upstream has no cap and documents that a
  long `undo-timeout` grows the stack without bound; here that is unbounded *processes*, so the cap
  reaches the same end the timeout would have.
- **A shortened `undo-timeout` retires existing entries at the next prune**, rather than at the
  deadline they were recorded with. That is what makes `undo-timeout = 0` mean "off" the moment the
  config reloads, which is what a user turning it off is asking for.
- **A session sitting in an undo entry is not pumped.** Its PTY output buffers in the reader
  channel and is drained when it is restored, so nothing is lost; over the default 5 s window that
  is a few kilobytes. Ghostty's undone surfaces keep rendering, since they are only detached from a
  view hierarchy.
- **The triggers are `ctrl+shift+z` / `ctrl+shift+y`**, not upstream's `super+z` / `super+shift+z`
  (and upstream's second `super+shift+t` undo binding, which is macOS's "reopen closed tab"
  convention — `ctrl+shift+t` is already `new_tab` here). Bare `ctrl+z` is deliberately left unbound:
  it is the shell's. Both are `performable:` like upstream, so an empty stack hands the key to the
  shell rather than swallowing it — which needed `PerformCtx` to carry a new `UndoState`, mirrored
  onto each window every pass because the gate runs inside `session::decide_key`, which has no route
  back to `App`.

**Verified**: `cargo test` — the manager's ordering, phase and expiry rules (`src/undo.rs`, 8
tests), the `detach_leaf`/`attach_at` round-trip including a nested path and the changed-layout
fallback, and the two tab helpers now returning what they closed with the slots to put it back in.
The end-to-end restore is structural rather than visual, but a window coming back on screen at its
old geometry wants a human look.

### `all:` / broadcast input — ✅ divergences

`all:ctrl+alt+k=clear_screen` applies a surface action to **every pane**, not just the focused one —
which is the "broadcast input" line of Tier 3 and the `all:` trigger flag, one feature under two
names.

- **`Action::scope()` is ported verbatim from `Binding.zig`**, not hand-picked from what giest's
  implementation happens to touch. Several rows are counter-intuitive and are upstream's on purpose:
  `new_tab`, `goto_tab`, `close_tab` and `toggle_readonly` are **surface**-scoped ("relevant to the
  surface they come from"), while `new_window`, `quit` and `reload_config` are **app**-scoped and run
  once. A bespoke taxonomy here would be a second opinion free to drift — the failure this codebase
  keeps catching.
- **A second, narrower predicate is giest plumbing and says so.** `broadcasts_to_panes` is the subset
  of surface actions whose execution touches only the focused *session*, derived by reading
  `execute_action`'s arms. The window-structural remainder (new/close/goto tab, splits, focus moves)
  is surface-scoped upstream but runs **once** here, because `execute_action` acts on the focused
  pane and takes no target. *Deferred follow-up, named:* thread a target pane through
  `execute_action`. Repeating `close_tab` per pane would also not be upstream's behaviour — there
  each surface closes *its own* tab, where giest's closes the active one N times.
- **Broadcast covers every pane in every tab of the window that received the key**, background tabs
  included (upstream also broadcasts to invisible surfaces). *Divergence:* it stops at that window —
  upstream iterates every surface in the app, but `handle_shortcuts` is a `Window` method and cannot
  reach its siblings. The realistic multi-window ceiling here is 2–3 (see the P5 ledger).
- **Implemented by temporarily focusing each pane**, so every action reuses the single
  implementation in `execute_action` instead of growing a second one. Focus and the active tab are
  restored afterwards.
- **`all:` is dominant over the other flags, and that is upstream, not convenience.** It always
  consumes the key (`global or all → consumed = true`, so it overrides `unconsumed:`) and is always
  treated as performed ("all actions are always performed since they are global", so it skips
  `performable:`). Both gates short-circuit on it identically.
- **Sequences are rejected for `all:`** (upstream's rule, shared with `global:`), reported rather
  than quietly bound to the last chord. But **`all:` *is* legal inside a key table** — the OS-hook
  reason for rejecting `global:` there does not apply, so that rejection is deliberately not copied.
- **Broadcast `paste` raises the paste-protection prompt per pane**, since each `paste_str` gates
  independently — correct by the one-gate rule, though a background tab's prompt waits until you
  visit it. Broadcast `text:` still respects each pane's read-only flag.
- **Font-size actions are app-global in giest anyway** (one atlas, see the P5 ledger), so `all:` on
  them is already all-panes — parity for free rather than by design.
- **Related divergence, now nameable:** upstream's `global:` *implies* `all:`, so a global binding
  broadcasts surface actions app-wide. giest's global bindings run once against the last-used
  window. Not wired; recorded.

### `chain=` multi-action bindings — ✅ divergences

`keybind = chain=<action>` appends an action to the most recently defined binding, so one key can
run several actions in order.

- **A binding now holds `Vec<Action>`, never empty**, and `Lookup::Action` carries the whole list.
  A parallel "lookup the chain" API was rejected: two lookups is how the two gates end up disagreeing
  about what a key does, which this codebase has now hit three times. `lookup` still answers with the
  *first* action, so every existing caller is unchanged.
- **Performable for a chain is "any action can act", read from source rather than chosen.**
  `Surface.zig` accumulates `performed = performed or v` across the chain, so a chain counts as
  performed if *any* of its actions did. Guessing "the first one decides" was the obvious wrong
  answer.
- **The chain parent is parse state, and everything that isn't a plain bind clears it.** An
  `unbind`, a table definition, a `global:` bind, an unparseable trigger or an unknown action all
  reset it — upstream's own comment is "removal always resets our chain parent". Without that,
  `ctrl+a=new_window` / `ctrl+b=unbind` / `chain=…` would silently extend `ctrl+a`. Pinned by a test,
  because the wrong behaviour is invisible in a config.
- **`chain` is intercepted as a trigger name before anything else**, since it takes no table prefix
  (upstream: "chain itself doesn't get prefixed with the table name") and no flags ("chained actions
  cannot have prefixes") — the original binding's flags apply to the whole chain. A `chain=` with no
  parent is reported, not a panic.
- **A chained payload keeps its payload.** `chain=text:a=b` splits on the first `=` like every other
  keybind line, so the verbatim-payload rule from the `text:` pass holds on this new route too.
- **One-shot tables pop once per *binding*, not per chained action** — the pop already sits before
  the loop that runs them.
- **No round-trip for a chain, deliberately.** A multi-action binding has no single `name()`, which
  is the same asymmetry upstream has (chains are written as several lines). Checked that nothing
  serializes the keymap before accepting it: the only `Action::name()` consumer is the command
  palette's label, and the palette's catalog holds single actions.

### `catch_all` + the `unconsumed:` flag — ✅ divergences

- **`catch_all` is a *key name*, not a side flag.** `KeyCode::CatchAll` drops into the existing chord
  parser, so `ctrl+catch_all` and `copy/catch_all=ignore` fall out of the chord, sequence and table
  machinery already there. It can never be produced by a real key event, so the two places that map
  a `KeyCode` outward (the PTY encoder and the global-hotkey VK table) are explicit dead ends rather
  than a panic waiting to happen.
- **Resolved inside each set before falling outward** — upstream's `Set.getEvent`, read rather than
  guessed. The consequence is the interesting one: a key table's `catch_all` shadows an *exact*
  binding in an outer table or the root, which is what makes a table modal and is the whole reason
  to put one in a table.
- **Order within a set: exact → `catch_all` with the same modifiers → bare `catch_all`.** The bare
  fallback runs only if the press had modifiers, so a modifierless press gets exactly one try (for
  it the two lookups are the same). A table test pins all three, since the double-match is invisible.
- **A `catch_all` that would `ignore` swallows a broken sequence whole.** Upstream: an unbound key
  mid-sequence normally flushes every buffered key to the program, *unless* a `catch_all` would
  ignore it — then the whole sequence is dropped silently. Without that, a mistyped sequence inside
  a modal table would leak its keys. This is checkable only because the previous pass split `ignore`
  from `unbind`.
- **`unconsumed:` inverts a standing invariant of this codebase**, and both gates' comments now say
  so: a bound chord normally never reaches the shell, and an `unconsumed:` binding runs its action
  *and* encodes the key. Only for a **complete** binding — a sequence leader is still swallowed, or
  the sequence could never start.
- **A reserved namespace still beats `unconsumed:`.** `ctrl+shift+*` and friends never reach the
  shell under any binding, so honouring the flag there would be the one way to inject a Ctrl+Shift
  chord into a program. Documented and pinned rather than left to discover.
- **Flags stack in any order.** Upstream documents `global:unconsumed:…` and fixes no order, so the
  parser loops until nothing strips instead of testing one arrangement. `performable:unconsumed:`
  composes as upstream implies: unperformable is a plain fall-through (encode, no action), otherwise
  encode *and* run.
- **A modifierless binding was typed *and* run — found by tracing, not by a test.** egui delivers a
  printable key as an `Event::Key` **and** an `Event::Text`. `decide_key` swallows the Key, but the
  Text arm wrote to the PTY unconditionally, so `copy/j=scroll_page_down` scrolled *and* typed `j`.
  Nothing hit this before, because every binding had explicit modifiers and modified presses emit no
  text — key tables and `catch_all` are the first way to bind a bare printable key, so **this was a
  live bug in the key-table commit**, not just a `catch_all` concern. A swallowed press that will
  produce text now suppresses the matching `Event::Text`. Scoped to the frame's event list, since
  egui emits the pair back to back; and only presses that *can* produce text are counted (a
  ctrl/alt/super combo emits none, so counting it would eat a later, unrelated character — shift is
  deliberately not in that list, since `shift+a` types `A`). No unit test can see the event stream,
  so the predicate is tested and the pairing is stated here.
- **`unconsumed:` on a sequence applies to the completed binding.** The leader is still swallowed —
  otherwise the sequence could never start — and upstream forbids sequences only for `global:`/
  `all:`, so this is legal rather than rejected. Pinned by a test.
- **Not done: `all:`.** It broadcasts a surface action to every pane, which is the same feature as
  the Tier-3 "broadcast input" line — they are **one** gap, not two, and it needs a broadcast path
  through `execute_action` rather than a parse change. `chain=` multi-action bindings are also still
  open.

### Key tables — ✅ divergences

`<table>/<binding>` definitions plus `activate_key_table[_once]:<name>`, `deactivate_key_table` and
`deactivate_all_key_tables`. The mechanism behind a modal "copy mode" or vim-style layer.

- **Lookup falls from the innermost table *outward*, ending at the root**, which is upstream's rule
  and the non-obvious half of the feature: a table is **not modal by itself**. Root bindings stay
  reachable while a table is active, so shadowing one takes an explicit `ignore`.
- **That forced a real bug fix: giest treated `ignore` and `unbind` as the same thing.** Upstream's
  `unbind` is `set.remove` — the key goes back to the shell — while `ignore` *binds* it to nothing,
  black-holing it. giest removed the binding for both, so `keybind = ctrl+t=ignore` let the key
  through to the shell rather than swallowing it. They are now distinct (`ignore` →
  `Action::Noop("ignore")`), which is also what makes table shadowing expressible.
- **Only the bare `<name>/` form clears a table.** Naming a table defines it — that is what makes
  `activate_key_table:<name>` work before anything is bound in it — but clearing on *every* line
  would wipe the table's earlier bindings one line at a time. Found by a test, not by reading.
- **A table name is only read where one can legally appear.** Names cannot contain `/ = + >`
  (upstream's rule), which is exactly what keeps `ctrl+/` from being misread as a table named
  `ctrl+`. Pinned by a test.
- **The stack is per-window runtime state and is cleared on config reload**, along with any
  half-finished key sequence. A reload can delete a table whose name is still on the stack, and a
  stale name silently changing which bindings resolve is the worst outcome available. Upstream's
  behaviour here wasn't cheaply discoverable, so this is a deliberate choice rather than a port.
- **Both gates resolve against the same stack**, through one shared `TableEntry` type — the keymap
  reads its `name`, the app reads its `once`. This is the two-gate trap's third appearance: a
  mismatch means a table's key is swallowed-and-inert, or reaches the shell *and* runs.
- **`can_perform` grew to cover the table actions**, because upstream specifies them in performable
  terms: activating an unknown table, or one that is already innermost, "has no effect and
  performable will report false" (which is what stops `A -> B -> B`, while allowing `A -> B -> A`).
  The "no effect" half is enforced at execution too, since it holds whether or not the binding
  carried the `performable:` flag.
- **A one-shot table pops *before* its action runs**, so an action that activates another table
  isn't immediately popped by its predecessor's flag.
- **Not done, deliberately:** `catch_all` (orthogonal — it works in the root table too, so it is its
  own feature; without it, one-shot's "deactivated on any non-catch-all binding" degrades cleanly to
  "on any binding" and stays correct when catch_all lands); `chain=` multi-action bindings (also not
  table-specific); and **`global:` inside a table**, which is *reported rather than silently
  accepted* — a global chord is delivered by an OS keyboard hook that deliberately does the minimum
  and never consults app state, so it cannot ask which table is active, and registering it
  unconditionally would fire it outside the table.

### `text:` / `csi:` / `esc:` / `set_*_title:` — ✅ divergences

The five actions the keybind-coverage ledger listed as "not possible without changing `Action`".
`Action` now owns `Arc<str>` payloads and is `Clone` rather than `Copy`.

- **The refactor was measured, not argued.** Dropping `Copy` produced 16 errors, all mechanical
  (`.copied()` → `.cloned()`, `self` → `&self` on three methods, a few `.clone()`s at deferred-intent
  sites). It was done as its own pass — everything compiling and all tests green with **no behaviour
  change** — before a single new variant was added. The `const` action tables survived untouched,
  which was the one thing that might have forced a bigger change.
- **The `text:` escape grammar is ported, not invented.** Upstream runs the payload through
  `config/string.zig`, which is **Zig string-literal escapes**: `\n \r \t \\ \' \" \xNN \u{...}`. A
  different grammar here would produce a binding that looks right and sends the wrong bytes for a
  real Ghostty config — `\x1b` being ESC is the whole point of the feature. A malformed escape fails
  the *whole* payload rather than emitting it literally, which is also upstream's rule.
- **Payloads are stored raw and decoded at send time**, like upstream. That is what makes `name()`
  round-trip verbatim without a re-escaping pass, and it keeps the error where upstream puts it.
- **`csi:` and `esc:` take their payloads raw** — no escape decoding at all. Upstream simply prints
  `ESC [ {s}` and `ESC {s}`. Getting this backwards would break `csi:0m`.
- **The payload keeps its trailing whitespace, and the config layers had to be checked to know
  that.** `Action::from_name` matches these prefixes ahead of the trim every other action name gets
  — but two layers above it were trimming as well (`config.rs`'s keybind setter and
  `Keymap::from_config`), so the guarantee was only true of the function the unit tests called.
  Both now `trim_start` only. What giest cannot preserve is whitespace around the whole config
  *value*, which the line parser strips — and **upstream strips it too** (`cli/args.zig` trims the
  value and then unquotes), so `keybind = "ctrl+k=text:hello "` is the spelling in both. That is
  pinned by a **config-body** test in `tests/config_conformance.rs`, driving the real pipeline
  rather than the parser in isolation, and it also covers an `=` inside the payload.
- **`send_text` is not a paste.** It deliberately bypasses `Session::paste_str` — this is a fixed
  string from the user's own config, not clipboard content, so bracketing it or raising a
  paste-protection prompt would be wrong. It does scroll to the bottom (the user is "typing") and it
  respects read-only, for the same reason keys do.
- **`set_surface_title:` needed a per-pane title override**, which giest didn't have: the pane title
  came straight from the engine. An **empty** payload clears the override and hands the title back
  to the program — the only way to undo one.
- Verified by tests over the escape grammar (including the malformed cases) and a round-trip over
  every payload action, the untrimmed cases included.

### Read-only indicator, and secure input closed out as N/A — ✅ divergences

- **`toggle_readonly` had no user-visible effect.** It set the flag and printed to *stderr*, which
  nobody running a GUI ever sees — so a read-only pane was indistinguishable from a hung shell,
  which is the one thing the feature must not look like. There is now a persistent `READ-ONLY`
  badge in the pane's bottom-right corner.
- **Painter-only, and persistent.** An egui widget in the corner would eat clicks meant for the
  terminal (the standing rule for every overlay here), and the state doesn't fade the way the resize
  overlay does, so there is no repaint request and no alpha ramp. It is inset past the scrollbar's
  hot band so the two never overlap, and bottom-right so it misses the resize overlay's default
  centre.
- **The *warn* accent, not danger.** Read-only is a mode the user asked for, not an error. Colors
  come from `theme.rs` like every other chrome element, so it follows `window-theme` and the
  palette.
- **Secure input is Not Applicable on Windows, and that is measured rather than assumed.** Upstream
  documents the feature as macOS-only ("Ghostty on macOS will automatically enable the Secure Input
  feature…"), it wraps the macOS-specific `EnableSecureEventInput`, and the **GTK apprt lists
  `secure_input` under "Unimplemented"** (`apprt/gtk/class/application.zig`) — so the platform
  closest to giest's position doesn't have it either. Windows exposes no equivalent service: there
  is no API to stop other processes reading keystrokes. Recording it as N/A rather than leaving it
  on the roadmap as a permanently-open item.
- **`toggle_secure_input` still *binds*, as a no-op**, so a transferred Ghostty config doesn't log
  "unknown action" at a user who can't act on it — the `equalize_splits` precedent. That needed
  `Action::Noop` to carry the name it stands in for (it round-trips through `name()`, and two
  unimplementable actions can't share a nameless no-op); the string-owning `Action` made it free.

### Selection interaction: `adjust_selection`, rectangle drag, autoscroll — ✅ divergences

The three follow-ups the selection migration deferred, plus the `performable:` trigger flag they
needed.

- **`performable:` is now real**, not a listed gap. A performable binding only counts while its
  action can act; otherwise the key belongs to the shell. That is what keeps `shift+arrow` working
  in an editor with nothing selected, and it is checked at **both** gates through one
  `command::can_perform` — the gate deciding whether the shell sees the key and the gate running the
  action. Checking only the first delivers the key *and* runs the action; only the second swallows
  it and does nothing. The same two-gate trap as the modal one already recorded in CLAUDE.md.
- **Only the arrows are bound, and that is upstream, not a shortfall.** `Config.zig` binds
  `shift+home/end/pageup/pagedown` to `adjust_selection` too — and then registers the viewport-scroll
  bindings *after* them on every non-macOS platform, so those four are scroll bindings there. giest
  is Windows and matches. All ten direction names still parse, since a config may bind any of them.
- **The moves are the binding's, not cursor arithmetic.** `Left` goes to the previous *non-empty*
  cell, wrapping upward; `Down` to the next non-blank row. Reimplementing that on the grid would be
  a second opinion about what a selection is.
- **`adjust_selection` forced the selection *head* to be tracked too.** Adjusting means rebuilding
  the selection, and the terminal owns the live one but cannot be asked for it
  (`GHOSTTY_TERMINAL_DATA_SELECTION` is unbound in the binding). A drag still doesn't need it — the
  end is wherever the pointer is now — so this is the one caller that pays for the second pin.
- **The new end is scrolled to the nearest edge, not centred.** This fires on every repeat of a held
  shift+arrow; re-centring each time makes the view lurch.
- **Rectangle drag is ctrl+alt**, read off `surface_mouse.zig::isRectangleSelectState` (macOS uses a
  bare alt; every other platform ctrl+alt). It was cheap for the same reason the migration was
  worth it: the flag rides into `Selection::new` and both the highlight and copy already handle a
  block. The engine remembers it because `adjust_selection` rebuilds the selection — without that a
  shift+arrow would silently turn a block back into a run of text — and word/line/output selections
  reset it, those extents being runs of text by definition.
- **Drag-past-the-edge autoscroll is rate-limited to upstream's 15 ms per row, not one row per
  frame.** Per-frame ticking happens to equal upstream at 60 Hz and runs at more than twice its
  speed on a 144 Hz display — a divergence no screenshot or hand-drag would ever reveal, so the
  rate lives in a pure `autoscroll_rows` with a table test instead. A long stall is clamped (it must
  not bank a hundred rows and jump the viewport) and leaving the edge resets the accumulator, so
  re-entering starts a fresh tick rather than firing a burst. The repaint request is the other
  load-bearing part: egui reports the drag every frame, but with the pointer parked outside the pane
  nothing else would schedule those frames and the scroll would stall after one row.
  *Divergence:* the selection end is resolved against the frame's *current* viewport, so it trails
  the scroll by one frame and catches up on the next tick — structurally the same one-frame lag the
  scrollbar drag documents.
- **Not done:** upstream's **60%-of-cell-width threshold** for whether the clicked and dragged cells
  are included (`Surface.zig::mouseSelection`) — giest still includes on cell hit, so a drag can
  grab one more cell than Ghostty would; and the double-click-*drag* word-snapping refinement
  (`select_word_between`). Both remain from the migration ledger.
- Verified by engine tests driving real sequences: adjust moves the free end and leaves the anchor
  (read untrimmed, so the space it crosses is visible), adjust with no selection reports "not
  performed", a block selection takes three equal column spans where a linear one takes everything
  between, a block survives an adjust and is cleared by a word select, plus keymap tests for the
  flag, a `decide_key` test pinning both halves of the performable rule, a round-trip test over all
  ten `adjust_selection:` names (the *config* path the default binds never exercise), and the
  autoscroll rate table.
  **One assumption is unverified and needs a human drag:** that egui reports `dragged()` on frames
  with no pointer movement. The whole parked-pointer autoscroll rests on it. Drag past the bottom
  edge and *hold still* — it must keep scrolling; if it stalls after a row, the fix is to poll the
  pointer state rather than the drag response.

### Search: cross-wrap matches + drift-free tracking — ✅ divergences

The two limitations the scrollback-search entry shipped with are closed. A query spanning a soft
wrap is found, and match rows are corrected when scrollback eviction renumbers the screen.

- **Wrapped rows are joined in `search.rs`, not in the engine.** `screen_text` still yields one
  entry per *display* row and gained a `wrapped` flag (from `Row::is_wrapped`), because
  `write_scrollback_file` / `write_screen_file` read the same method — joining there would silently
  unwrap the file giest writes, which nobody asked for. Search does the joining itself, in pure code
  that hand-built rows can test.
- **A `Match` now carries a start *and* an end row.** A match across a wrap covers several display
  rows, so `search_highlights` emits one span per row: the first runs to the end of the line, the
  last starts at column 0. The renderer's mask builder already took a list, so it is unchanged.
- **One tracked reference corrects every match, not one per match.** Eviction drops the oldest rows
  and renumbers the whole screen by the same amount, so a single anchor gives the shift. Each
  tracked reference costs bookkeeping on every terminal mutation, and a search can have hundreds of
  matches.
- **The anchor is the capture's *last* row, and that is load-bearing.** The first row is the first
  to be evicted, and upstream moves a destroyed pin to the screen's **top-left** — so a top anchor
  would read as "row 0, no drift" at exactly the moment there was drift. `has_value` is checked
  before the point for the same reason; a bare point read is confidently wrong.
- **A match whose rows were evicted is dropped, not shifted.** Drawing it where those rows used to
  be would highlight unrelated text.
- **Measured, and worth knowing: pruning is page-granular.** A few lines past the limit evict
  *nothing*, so drift arrives in jumps and the correction is free in the common case. The test that
  forces a real prune is `#[ignore]`d because it needs ~12k lines (~8 s).
  **Also measured, and it corrected a documented "divergence" that never existed:**
  `max_scrollback = 10` retained ~7,200 rows after 20k lines. Chasing that through the vendored
  source: the C header calls the option "Maximum number of lines to keep in scrollback history", but
  it is passed straight to `Screen.init`, whose own comment reads *"max_scrollback is the amount of
  scrollback to keep in **bytes**"*, and `PageList.maxSize()` is
  `max(explicit_max_size, min_max_size)` — so a value below one page's worth is floored away. The
  header is wrong. giest's `scrollback-limit` is therefore in **bytes, exactly like Ghostty's**, and
  the note claiming "the key matches but the unit differs" was the error; `config.rs` and the
  configuration guide now say so.
- **`SpacerHead` is now skipped in `screen_text`.** When a wide character doesn't fit at the end of a
  row it moves to the next, leaving a spacer behind; emitting a space for it put one *inside* the
  wrapped word, so a query spanning the wrap could not match. (`SpacerTail` was already skipped.)
- **The anchor is a row *index*, not a line's contents.** It pins column 0 of the capture's last
  row, which is usually the blank live row the cursor is sitting on — and that line then gets text
  written to it. That doesn't matter: only the index is read, and the pin follows its line.
- **A prune that destroys the anchor triggers a recapture**, in `pump_pty`. Without it the shift
  would silently fall back to zero and every highlight would go back to being uncorrected — worse
  than useless, since wrong highlights read as right ones. The recapture costs a screen walk, but
  only at the moment a prune actually took the anchored row.
- **Unchanged, deliberately:** ASCII case folding. This pass was wrap + drift only; regex came later (next ledger).
- Verified by pure tests over hand-built rows (wrap join, a non-wrapped boundary *not* joined, a
  trailing wrapped row not running off the end, the shift arithmetic and the drop rule) and by
  engine tests on a real terminal (a real wrap marked and matched across, a wide character pushed
  over a wrap still matching, and the anchor naming the row its line is actually on).

### Regex search, and why not the native search API — ✅ divergences

A `.*` toggle beside `Aa` in the search bar switches the query to a regular expression (the `regex`
crate's syntax). An invalid pattern — usually one still being typed — shows a red `!` whose tooltip
gives the parse error, rather than a `0/0` that reads as "no matches".

- **lib-vt's `ghostty_search_*` was evaluated and not adopted.** Its header is explicit: "Matching is
  byte-exact except ASCII letters, which compare case-insensitively" — there is no case-sensitive
  mode and no regex. Backing the bar with it would *remove* the `Aa` toggle and still leave regex to
  be built elsewhere. What it would have added — matches tracked internally across resize, reflow
  and eviction, and primary-screen results kept across an alt-screen app — giest already covers for
  the cases that matter (wrap joining, the eviction anchor, recapture on resize). Revisit if
  upstream grows a case or regex option; its `SELECTED_MATCH`/`VIEWPORT_MATCHES` shape would then be
  a cleaner fit than giest's capture.
- **Regex runs over the same wrap-joined logical lines as substring search**, so a match can span
  a soft wrap. It never spans a hard line break, and `^`/`$` anchor to the logical line — what the
  program that printed the text meant by a line. Byte offsets are mapped back to chars and then to
  cell columns, so multi-byte and wide characters highlight the right cells.
- **Empty matches are skipped** (`a*` against `bbb`): no cell to highlight, and navigation would
  step through nothing.
- **Case folding differs by mode, deliberately.** Regex mode folds Unicode (the crate's own
  behaviour); substring mode still folds ASCII only, as upstream's search does.
- **No regex in the `search:` action syntax.** Upstream has no regex search, so there is no syntax
  to mirror; `search:` sets the needle and the bar's `.*` state decides how it's read.
  `search_selection` escapes the selection in regex mode, since a selection is literal text.
- **The toggle isn't bindable and doesn't persist** across closing the bar, like `Aa`.

### Selection migration (engine-owned, scrollback-spanning) — ✅ divergences

The selection now lives in the **VT engine**, not the app. `Session`'s two viewport
`(col,row)` pairs are gone; `GhosttyVtEngine` holds the drag anchor as a libghostty
**tracked grid ref** and installs the selection into the terminal itself.

- **This is what the semantic-selection ledger said was still open**, and the four limits it
  recorded are now gone: a selection survives scrolling, scrollback eviction and reflow; copy spans
  scrollback; a selection starting above the viewport is kept **whole** rather than clamped to the
  visible part; and `select_all` means everything rather than the viewport rectangle.
- **The binding already had every piece** — `track_grid_ref` (owned, `Drop`-freed, may outlive the
  terminal), `set_selection`, `format_selection_alloc`, and the render state's per-row selection
  range. Nothing needed patching. What made this expensive to *reason* about is that
  `Selection`/`GridRef` borrow the terminal, and the answer is that nothing borrowing is ever
  stored: every use snapshots the tracked anchor and drops the untracked ref in the same scope.
  Holding one across a `vt_write` is the `walk_placements` trap again.
- **Only the anchor is tracked.** The moving end is wherever the pointer is *now* and is resolved
  fresh on each update; the range itself is owned by the terminal, which `set_selection` converts
  to tracked state internally. giest keeps its own anchor solely because the binding exposes no way
  to read the active selection back (`GHOSTTY_TERMINAL_DATA_SELECTION` is unbound).
- **`selection_installed` mirrors terminal state that cannot be queried.** Same cause. It is the
  one piece of duplicated state here, and it exists rather than a guess from "is the anchor set".
  *Known consequence, judged not worth code:* the terminal can invalidate its own selection without
  telling us (the alt screen has its own; a reset degrades the range), so `has_selection()` can
  enable a Copy menu item that copies nothing. The text path stays honest either way —
  `format_selection_alloc` returns nothing — and `Ctrl+C` copy-or-interrupt reads the actual text,
  so it decides correctly.
- **Fixed in passing:** `equalize_splits` parsed to `Action::ClearSelection`. giest has nothing to
  equalize (splits are always 50/50) and the intent was a no-op, but the stand-in was a real action
  — so binding a Ghostty config's `equalize_splits` silently *dropped the user's selection*. There
  is now an `Action::Noop` that stands for nothing else, and a test, because in a config the wrong
  behaviour is invisible.
- **An anchor that loses its cell clears the selection.** Upstream's tracked pins move to the
  screen's top-left when their row is destroyed; extending a drag from a cell that no longer exists
  would select something the user never pointed at, so giest drops it instead.
- **A selection change forces a snapshot rebuild.** Installing a selection does not necessarily
  dirty the render state, and the "nothing changed" fast path would leave the highlight unpainted on
  an idle screen — the same insurance `viewport_moved` provides, and a silent failure without it.
- **The renderer stopped computing selection.** `PaneFrame::selection` (an inclusive linear cell
  range) is deleted; each `Cell` carries `selected`, filled from the row-local range the render
  state reports — asked **once per row**, which is what the C API recommends for a renderer that
  works in spans. A rectangle in app coordinates could not have expressed a soft-wrapped or
  reflowed selection at all.
- **`Session::extract_selection` and its tests are deleted, not kept as a fallback.** Copy reads
  through the engine now; a grid-scanning copy would be a second opinion about what is selected —
  the failure this repo keeps recording. `clipboard-trim-trailing-spaces` maps onto the formatter's
  own `trim` flag, and `unwrap` is on: a wrapped command copies as one line, not as the rows it was
  displayed on.
- **Still deferred, deliberately:** rectangle/block selection (the binding takes a `rectangle` flag
  and upstream drives it from a modifier); upstream's **60%-of-cell-width threshold** for whether
  the clicked and dragged cells are included (`Surface.zig::mouseSelection`) — giest includes on
  cell hit, so a drag can grab one more cell than Ghostty would; **drag-past-the-edge autoscroll**
  (upstream ticks one row per timer tick while the button is held); the `adjust_selection` keybinds
  (the `Adjustment` enum makes them cheap now); and **search** cross-wrap matches, which this
  unlocks but which are their own subsystem and their own pass.
- **Verified by engine tests driving real escape sequences**: word/`selection-word-chars`
  boundaries by their *copied text*, a soft-wrapped triple-click returning one unwrapped line while
  highlighting two display rows, OSC 133 command output, a selection made before ten screens of
  output still reading back correctly (the tracked-anchor case), a **resize that rewraps the text**
  leaving the selected word selected, `select_all` including a scrolled-off row, trim on/off, clear,
  and an update with no anchor. The highlight itself is **perceptual** and wants human confirmation
  in the running app.

### `config-file` (config includes) — ✅ divergences

`config-file = <path>`, repeatable, with the `?` optional prefix. `Config::load_from_file` walks the
include graph; `Config::load` and the reload path both go through it, so an include applies at
startup *and* on reload.

- **The traversal is upstream's, and upstream's is subtle in two ways** — read off
  `Config.zig::loadRecursiveFiles` rather than assumed. (1) An included file is loaded **after the
  entire file that named it**, so its keys beat that file's keys, not merely the lines above the
  `config-file` line (upstream's own doc comment calls this out in bold). (2) Nested includes append
  to the **end of one shared list**, making the walk **breadth-first**: `a` (including `deep`) then
  `b` loads `a`, `b`, `deep`, and `deep` wins. A depth-first walk is the natural implementation and
  would leave `b` winning; a test pins the ordering because nothing else would notice.
- **Cycle detection keys on the canonicalized path**, so `a/../b` and a symlink can't reintroduce a
  cycle by spelling the same file differently. A path that won't canonicalize (it doesn't exist)
  falls back to itself, which still catches a literal repeat. A repeat is skipped with a message and
  the load continues, like upstream — never a hang and never a lost config.
- **A missing *root* config is silent; a missing *include* is reported.** No config file at all is
  the normal first-run state, whereas an include is a filename the user typed.
- **Divergence: `"?name"` cannot quote a literal leading `?`.** giest's parser strips surrounding
  quotes before any setter sees a value, so the escape upstream offers has nowhere to live — and `?`
  is not a legal character in a Windows filename, so there is nothing to escape.
- **`config_file` is a staging field, not a setting.** The setter collects raw specs, `apply_body`
  drains them after each file, and a test asserts a loaded config's list is empty — a leftover would
  be a plausible-looking value for a caller to act on twice.
- **Not done: conditional configuration** (`?theme:dark` style predicates / `Conditional` in
  upstream), which is the other half of the "config include/conditional" line in §1.
- Verified by tests over real files in a scratch directory: override ordering across an include,
  breadth-first ordering, relative resolution from a subdirectory, optional-missing and
  required-missing, a two-file cycle plus a self-include, and the empty-value reset.

### Sprite glyphs (box drawing, blocks, braille, powerline) — ✅ divergences

giest now draws U+2500–257F (complete), U+2580–259F, U+2800–28FF and the geometric powerline
separators itself, ported from Ghostty's `font/sprite/draw/{box,block,braille,powerline}.zig`,
with `adjust-box-thickness`.

- **The drawn glyph beats the font**, which is upstream's precedence too (`CodepointResolver`
  checks its sprite face before any font lookup, after only the explicit codepoint overrides).
  These characters are defined relative to the *cell* and a font draws them relative to its *em
  box*, so the font's version is only ever right by luck — and is wrong for everybody at any line
  spacing. giest already stretched the font's versions to the cell (`Constraint::Fill`), so this is
  a sharpness/correctness change rather than a new capability, but it also covers fonts that lack
  the characters entirely.
- **The arm table was transcribed mechanically, not by hand.** 109 intersection characters × four
  arms is exactly where one typo yields a single subtly wrong corner nobody notices for months, so
  `LINES` was generated from upstream's `linesChar` call sites.
- **Ghostty's asymmetric `Fraction.min`/`max` is preserved deliberately.** At an odd cell size the
  two halves of a quadrant character *overlap* by one pixel rather than one being a pixel smaller:
  an overlap is invisible (both sides opaque), a gap is a visible seam. "Simplifying" both to a
  plain round is how quadrants end up with a hairline between them — a test pins it.
- **`box_thickness` is derived from the cell height (`h/12`, min 1)**, where upstream derives it
  from the font's underline thickness — giest's atlas doesn't carry that metric. Same shape of
  rule, same tuning knob (`adjust-box-thickness`), and it can never round to zero, since an
  invisible line reads as a missing glyph rather than a thin one.
- **Second pass: the anti-aliased half.** U+256D–2570 rounded corners, U+2571–2573 diagonals and
  the geometric powerline separators (U+E0B0–E0BF, E0D2, E0D4) are now drawn too — **U+2500–257F is
  complete**. Two primitives cover all of it: a **distance-field stroke**
  (`clamp(thick/2 + 0.5 - d, 0, 1)`, which is exact to within a pixel and needs no cap/join
  geometry) and an 8×8-supersampled **polygon fill**.
  - *Divergence:* the distance field gives **round caps and joins** where Ghostty butt-caps and
    mitres, so a thin chevron's point is rounded by `thick/2`. Sub-pixel at the 1–2px thickness a
    cell implies; a cap/join system would be a lot of machinery for it.
  - *Divergence:* each right-facing separator is drawn by **mirroring** its left-facing twin, where
    upstream spells some of them out as their own polygon. The supersample grid isn't symmetric
    about the cell centre, so the two spellings differ by a few coverage levels along the
    hypotenuse — a `` and a `` meeting in a prompt would have visibly different edges. A
    test asserts the pairs are exact mirrors.
  - E0B5/E0B7's stroke is Ghostty's `innerStrokePath`, approximated as **stroke ∩ fill**: the curve
    bulges to exactly the cell edge, so a centred stroke would hang half outside and clip flat.
  - Two constants that look unifiable and are not: the arcs' control fraction `s = 0.25` and the
    half-circles' `(√2−1)·4/3`. Different curves, both intentional.
  - **These change appearance for existing users** — previously the font's `╭` and ``, now drawn.
- **Not ported, deliberately, so they still come from the font:** the *stylized* powerline symbols
  (U+E0C0+, E0D0/E0D1/E0D3 — flames, hexagons, ice), which upstream doesn't draw either, and the
  legacy-computing symbols. Ghostty's `super_light` weight goes with the last of those and is
  therefore unused here.
- **Ghostty's `isCovering` rule is now done, and it is much narrower than this ledger previously
  claimed.** Reading `renderer/cell.zig` + `generic.zig` rather than trusting the earlier summary:
  it covers **U+2588 FULL BLOCK and nothing else**, and it changes only the cell's *background
  colour* — to the foreground, so the glyph and its background agree and the cell reads as one
  solid rectangle (which is what makes padding extension work). It does **not** force opacity:
  upstream's bg-alpha block never consults it, so a full block on a default background still emits
  no quad under `background-opacity`. giest applies it in the same place and the same way, and
  **not** to a selected or cursor cell — upstream's non-selected-only arm, without which a
  selection would lose its own background. The earlier "full-block glyphs render opaque under
  transparency" framing described a mechanism upstream doesn't have.
- **A cell too small for a dashed line draws a solid one** rather than nothing — upstream's
  fallback, and the right one: an empty cell reads as an unsupported character.
- **The whole `adjust-*` family is now implemented** — see its own ledger entry below.
- **Verified by computed capture, not by eye** (per CLAUDE.md), DPI-aware with
  `PrintWindow(…, 3)`. A `┌──┐ │ │ └──┘` box printed by a startup script measured as an exact
  rectangle: the top and bottom rules are **single unbroken runs** `x = 4..214` — no gap at any of
  the 21 cell boundaries — and the sides are unbroken `y = 117..157`, spanning both rules and the
  full height of the middle row. The corners contribute their two arms and nothing else.
  Then the discriminating probe, since a stretched font glyph could also be continuous:
  `adjust-box-thickness = 3` turned the 1px rules into 4px rules, still centred, which nothing on
  the font path can do.
  The **rounded** box was measured the same way: rules unbroken `x = 6..212` (2px narrower than the
  square box at each end — the curve pulling away from the corner), the left side unbroken
  `y = 99..135` on the same column `│` uses, 21 partial-coverage pixels in the corner proving the
  anti-aliased path actually ran, and the same 1px → 4px thickness response.
  *Line thickness and the corner radius are matters of taste (like the curly underline) and still
  want human eyeballing.*

### Config-surface batch (11 keys) — ✅ divergences

Chosen by **measuring** the gap rather than guessing, and the audit is re-runnable:

```sh
# upstream keys                                    # giest keys
grep -oE '^@"[a-z0-9-]+"' ghostty-src/src/config/Config.zig | tr -d '@"' | sort -u
grep -oE '\("[a-z0-9-]+", \|' src/config.rs | grep -oE '"[a-z0-9-]+"' | tr -d '"' | sort -u
```

**187 upstream keys; giest now sets 113, of which 105 are upstream's** (`config-file`, then
`undo-timeout`, the four `font-variation*` keys, `adjust-icon-height`, and the four `font-style*`
keys, since) (the rest are giest-specific,
e.g. `text-gamma`). That replaces the "~250 options / ~230 remaining" estimates this document opened
with, which were never counted.

Landed: `working-directory`, `window-new-tab-position`, `window-padding-balance`,
`split-divider-color`, `search-background`/`-foreground`, `search-selected-background`/`-foreground`,
`selection-clear-on-typing`, `selection-clear-on-copy`.

- **Three defaults change giest's existing behaviour**, all toward upstream:
  `window-new-tab-position` defaults to **`current`** (giest always appended), `selection-clear-on-copy`
  defaults to **false** (giest cleared on every copy), and search matches now use Ghostty's amber
  (`#FFE082` / `#F2A57E` for the current one) instead of giest's own darker pair.
- **A mid-list tab insert is a new instance of this repo's stale-index trap.** `window-new-tab-position
  = current` inserts before the end, shifting every later tab — so `renaming` and `tab_drag`, which
  hold tab *indices*, are cleared at the insert like they are at every other mutation point. The
  index arithmetic is a pure `new_tab_index` with tests rather than a comment.
- **The search *foreground* keys needed a hook that didn't exist.** giest only ever recolored a
  match's background; the glyph pass had overrides for the cursor and selection but not for search,
  so a themed foreground could leave a match unreadable on the amber. Both `search-*` colors also
  accept upstream's `cell-foreground` / `cell-background` keywords, resolved per cell.
- **`split-divider-color` is applied to every chrome hairline**, not just the split gutter. giest
  derives one `divider` color for the gutter, the tab-strip edge and the palette rows; honoring the
  key for only one of them would leave a single stripe a different color from the rest.
- **`working-directory` folded into the *existing* cwd decision** rather than adding a second one:
  inherited (per the `*-inherit-working-directory` key for that surface) → `working-directory` →
  the process's own directory. That also changes the state-restore fallback — a saved directory that
  no longer exists now lands on `working-directory`.
- **`selection-clear-on-typing` counts only input the program receives.** App shortcuts and reserved
  combos are excluded: they never reach the shell, so clearing on them would drop a selection the
  user is still working with. IME preedit commits count as typing (they clear the selection like any Text).
- **`enquiry-response` is blocked on ConPTY — now measured, not guessed.** The probe this entry
  called for was written and run (`enq_is_still_stripped_by_conpty`): a shell emitting
  `giest-enq-open`, `0x05`, `giest-enq-close` comes back as `giest-enq-opengiest-enq-close`. Both
  markers survive and the ENQ does not, so the byte never reaches the engine and there is nothing to
  answer — the same class of blocker as kitty graphics, and it affects any Windows terminal. The
  assertion is **inverted** like the APC one, so it fails if a future Windows build starts
  forwarding ENQ.
- **Verified**: parse tests per key, table tests for the two pure helpers (`balance_padding`,
  `new_tab_index`), and `working-directory` end-to-end through a free oracle — with
  `window-save-state = always`, the state file written at quit records the cwd the shell *itself*
  reported via OSC 7, which came back as the configured directory. Search colors and padding balance
  are visual and want eyeballing.

### Semantic selection — ✅ divergences

Double-click (word), triple-click (logical line) and Ctrl+triple-click (command output) now come
from the binding's selection model, plus `selection-word-chars`. Ghostty's click-count mapping was
read off `Surface.zig` rather than guessed.

- **The hand-rolled version was replaced, not layered on.** `session.rs` had its own `is_word_char` /
  `word_bounds` scan over the rendered grid; keeping it as a fallback would leave two sources free to
  disagree about what a word is — the exact failure mode this repo keeps documenting. Its tests went
  with it, replaced by engine-driven ones.
- **Line selection now follows soft wrapping** (and stops at a prompt via
  `with_semantic_prompt_boundary`). The old version selected one *visual* row, so triple-clicking a
  command longer than the window gave you a fragment of it.
- **No lifetime crosses the engine trait.** `Selection`/`GridRef` borrow the terminal and giest's
  selection outlives any frame, so the binding types stay inside `engine/ghostty_vt.rs`. That is what
  made this slice cheap where the full migration was not.
- ~~**Off-viewport ends are clamped, not dropped.**~~ **Superseded** by the selection migration
  above: this slice returned viewport cell pairs, so a selection beginning above the screen had to be
  clamped and copy saw only the visible part. The selection is now engine-owned and tracked, so such
  a range is kept whole and `select_all` spans scrollback. Recorded rather than deleted because the
  clamping behaviour shipped, and this is what changed.
- **A semantic selection that finds nothing leaves the existing one alone** rather than clearing it,
  so a stray double-click on blank space doesn't discard what the user had.
- **`selection-word-chars` replaces the engine's list rather than adding to it**, which is upstream's
  behaviour, and NUL is prepended (always a boundary upstream). Parsed by **character**, not byte —
  upstream's own default list contains `│` (U+2502), which a byte loop would split into three bogus
  boundaries. `\t`, `\n` and `\\` are honoured; other Zig escapes upstream accepts are not.
- **Not done: the double-click-*drag* refinement.** The binding exposes `select_word_between` with a
  both-directions recipe (upstream uses it at `Surface.zig:4713`) so dragging from one word to
  another snaps to whole words; giest still extends by cell after the initial double-click.
- **Not matched: upstream checks for a link under the cursor *before* word selection** on
  double-click, so double-clicking a URL selects the whole link. giest has `hyperlink_at` and could,
  but the double-click path doesn't consult it yet.
- **Verified by engine tests driving real sequences** — word boundaries with and without a custom
  list (the discriminator proving the config threads through), a soft-wrapped line returning both
  rows, and `select_output` over real OSC 133 `A`/`B`/`C`/`D` marks excluding the prompt row, plus
  the clamping case. *The click wiring itself is not separately verified live* — a synthesized
  double-click probe was inconclusive — so double-click/triple-click behaviour in the running app
  wants a human check.

### Font fallback chains + synthetic styles — ✅ divergences

`font-family` is now repeatable (a fallback chain) and `font-synthetic-style` synthesizes a missing
bold or italic, closing the two divergences the original font-family entry deferred.

- **The synthesis decision is made from what actually resolved**, not from which config key was set:
  a styled slot that had to fall back to a face whose OS/2 bits say it isn't that style gets
  synthesized. That is upstream's rule ("if the font has the requested style, the font is used
  as-is"), and it means the fallback chain can't quietly change the decision.
- **Bold-italic follows upstream's preference order**: slant a real bold if there is one, else
  thicken a real italic, else do both to the regular.
- **`font-synthetic-style` reuses the `bell-features` packed-struct grammar** — bare bool sets all
  three, a list starts from the defaults, `no-` prefix, one unknown token rejects the value. The
  three flags are **independent**, which upstream flags as the easy mistake: `no-bold` does not
  disable bold-italic. Pinned by a test, because "I turned bold off and it's still synthesized"
  reads as a bug.
- **Synthesis is a bitmap post-process, not an outline transform.** ab_glyph exposes no outline
  editing, so bold dilates the rasterized coverage (max-blended, so anti-aliased edges survive) and
  italic shears it per row. Both are pure `Raster → Raster` functions, so they're unit-testable like
  `sprite.rs`. Divergences: the dilation is slightly chunkier than upstream's outline embolden, and
  the per-row integer shifts staircase the slanted edges a little at small sizes.
- **The constants are ported, not guessed**: the embolden strength is upstream's `height/32`
  heuristic (it *has* to scale with size — a fixed pixel is invisible at 28px and clubby at 10px),
  and the shear is `tan(12°)`, upstream's angle.
- **The shear pivots on the baseline** and moves the glyph's bearing when a descender swings left,
  without which the glyph walks out of its cell rather than leaning inside it.
- **`Fill` glyphs are never synthesized.** Emboldening a glyph already stretched to the cell would
  push it past the edges and break the seamless tiling that constraint exists for. Box drawing is
  drawn by `sprite.rs` anyway.
- **Fixed in passing:** a `font-family` given as a *file path* was accepted for **every** style slot,
  so a path-configured font claimed to have a real bold face it didn't have — and would never
  synthesize one. `find_font` now style-checks a path against its own OS/2 bits, while
  `find_regular_font` still takes the file as an explicit choice for the primary slot.
- **Fallback faces stay style-less**: the chain (and the system fonts behind it) is consulted by
  character, with no style, so synthetic bold does not reach a fallback glyph — bold CJK renders
  regular. Pre-existing shape of the fallback mechanism, not new.
- **Verified live**, with a discriminating probe that doesn't depend on which system fonts are
  installed: the embedded *regular-only* TTF is written to a temp file and used as `font-family`,
  which forces every styled slot to fall back. Against `font-synthetic-style = false`, bold gained
  **+62% ink** and a pixel of height, while the italic line's glyph tops moved **3–4px right of
  their bottoms** across a ~14px band (14 × tan(12°) ≈ 3.0px) versus upright with synthesis off.
  Plain text was byte-identical in both — the control. *Weight and slant are taste calls and want
  human eyeballing.* The chain ordering itself is covered by the slot tests rather than measured.

### `adjust-*` metrics + `isCovering` — ✅ divergences

All twelve `adjust-*` keys giest can act on (`-cell-width`, `-cell-height`, `-font-baseline`,
`-underline-position`/`-thickness`, `-strikethrough-*`, `-overline-*`, `-cursor-thickness`,
`-cursor-height`, `-box-thickness`), plus `isCovering` (see the sprite ledger).

- **Decorations now come from the font, not from a fraction of the cell.** giest previously drew
  every line at `cell_h * 0.07`, the underline at `ascent + that`, and the strikethrough at exactly
  half the cell. They are now derived from the face's `post`/`OS/2` line metrics like upstream, so
  **underline and strikethrough placement changes for existing users** — it should look better, but
  it is a visible change.
- **The metrics come from two libraries at once.** ab_glyph gives the cell box (it is what
  rasterizes), while the underline/strikeout lines come from the **ttf-parser** face behind
  rustybuzz — ab_glyph exposes no line metrics at all. Both describe the same face, so mixing is
  safe; the alternative was the hardcoded fractions above.
- **Sign conventions are upstream's**: positions are measured from the **top of the cell** (so the
  overline's default position is literally 0), and `ascent` is top-to-baseline. Fonts report the
  underline as a distance *below* the baseline and the strikeout as one *above* it, so the
  conversions differ in sign — that is the kind of thing a test has to pin, and does.
- **A thickness `ceil`s and clamps to ≥1; a position rounds and does not clamp.** Zero and negative
  are meaningful placements, so putting positions through the thickness helper would silently
  discard half the useful range. `MetricModifier::apply` therefore does *no* rounding at all and the
  three kinds round at their own call sites.
- **`adjust-cell-*` is applied before the `ceil`, and that is load-bearing** — found by measuring,
  not by reasoning. A 6.36px advance is a 7px cell; rounding the adjusted value first turned it into
  a **6px** cell, costing every column a pixel with no adjustment configured at all. Same trap class
  as the `window-position-*` DPI division. A test pins it with a fractional advance.
- **`adjust-cell-height` re-centres the text** (half the growth above the baseline) and carries the
  underline/strikethrough with it, so it reads as line spacing rather than as text glued to the top
  of a taller cell. Ghostty splits the diff the same way.
- **`adjust-cursor-height` shortens the cursor from the top**, leaving it sitting on the bottom of
  the cell — which is where upstream's bearing-placed cursor sprite ends up.
- **Not implemented:** `adjust-icon-height` (giest has no icon-height constraint to adjust; Nerd
  Font icons use the generic `Fit` path) and `font-variation`.
- **Verified**: the derivation is unit-tested against a synthetic face (14 → 24px cell for `= 10`,
  the baseline and underline both moving by 5), and live — a printed box's row pitch grew from 40px
  to ~59–60px across two rows under `adjust-cell-height = 10`, i.e. ~+10px per row as configured.
  The live edges are ±1px ambiguous; the unit tests are the exact evidence. *Underline and cursor
  appearance want human eyeballing.*

### Quick terminal + global keybinds — ✅ divergences

`toggle_quick_terminal` plus `quick-terminal-position` / `-size` / `-autohide` / `-screen`, and the
`global:` trigger flag that makes them reachable — Ghostty's own example binding is
`global:cmd+grave`, and a dropdown terminal you can only summon while it's focused is no dropdown
terminal at all.

- **A low-level keyboard hook, not `RegisterHotKey`.** `RegisterHotKey` posts `WM_HOTKEY` to the
  *thread message queue*, and winit owns the message loop with no hook for unrecognized thread
  messages — the message would be dispatched and dropped somewhere we can't see. `WH_KEYBOARD_LL`
  calls back on the installing thread during the dispatch winit is already pumping. The callback is
  on the OS input path, so it does the minimum (compare a VK, read modifiers, set a bit in an
  atomic) and `try_lock`s rather than blocks: Windows silently *unregisters* a hook that exceeds
  `LowLevelHooksTimeout`, and the feature would then stop working with no error anywhere.
- **The hook is installed only when a `global:` binding exists**, and removed when the last one goes.
  A system-wide keyboard hook is a real cost to every application, and a terminal should not take
  one uninvited. There is no default global binding for the same reason.
- **A matched chord is swallowed.** The key must not also reach whatever app was focused — that is
  what separates a global binding from a listener.
- **A global binding is deliberately *not* also an in-app binding.** The hook fires regardless of
  focus, so keeping a copy in the ordinary keymap would run the action twice on a focused press.
  `Keymap::globals()` is therefore a separate list, and a test pins that `lookup`/`starts_binding`
  don't see it.
- **Global binds follow the *root* window's config.** Every window holds its own `Config` clone
  (giest reloads per window, like Ghostty's per-surface clone), but an OS registration is
  process-wide and needs one authority. A reload in a secondary window won't re-register them.
- **Hiding the quick terminal means not drawing its viewport.** A child viewport ignores
  `ViewportCommand::Close`; ceasing to show it is what destroys the native window — and because the
  `Window` stays in `App::windows`, every shell inside keeps running and reopening is instant.
- **The frame math is a CPU port of `QuickTerminalSize.calculate` + `finalOrigin`**, defaults
  included (400px primary, full screen secondary, 800×400 centered landscape), so it can be table-
  tested; upstream computes the same numbers inside AppKit calls that can only be checked by eye.
  The **one** intentional inversion is the Y axis: AppKit's `visibleFrame` is Y-up from the
  bottom-left, Windows' work area is Y-down from the top-left, so `top`/`bottom` use the opposite
  arithmetic to the Swift to get the same visual result.
- **The geometry is divided by `pixels_per_point`** — the work area is physical pixels, egui's
  viewport commands are points. Exactly the trap `window-position-*` hit.
- **Sizes are clamped to the work area.** A `200%` would otherwise put most of the window off the
  edge with nothing on screen to say why. And the work area (not the full screen) is what a
  `bottom`-positioned terminal anchors to, or it would sit under the taskbar.
- **`quick-terminal-screen` honors `main` only**: `mouse` needs per-monitor enumeration giest has no
  handle for, and `macos-menu-bar` has no Windows meaning. It says so rather than silently placing
  the window on the wrong screen.
- **`quick-terminal-autohide` defaults to `false`**, which is Ghostty's own non-macOS default —
  and the right one here for the same reason: a global hotkey is the only way back.
- **Not done:** the slide-in animation (`quick-terminal-animation-duration`, macOS-only upstream),
  `quick-terminal-space-behavior` (macOS spaces), the GTK/Wayland `-layer` and `-namespace` keys,
  and the other trigger flags (`all:`, `unconsumed:`, `performable:`).
- **Verified live, by measurement**: with `quick-terminal-size = 30%` on a 2560×1392 work area, the
  hotkey pressed while giest was **unfocused** produced a new top-level window at `0,0` sized
  `2560×417` — full width, and 30% of 1392 = 417.6 → 417. Toggling again removed the window and a
  third press brought it back.

### Session / window state restore — ✅ divergences

`window-save-state = default | never | always`, with Ghostty's default. On exit the window list is
written to `%APPDATA%\giest\state` (`$GIEST_STATE` overrides) and rebuilt at the next launch:
windows, tabs and their order and active index, each tab's nested split tree and focused pane, a
renamed tab's name, and every pane's OSC 7 working directory.

- **`default` behaves as `never`**, and that is parity rather than a shortfall: upstream's `default`
  means "restore when the OS asks", which on macOS is the system's own reopen-windows setting.
  Windows has no such mechanism, so there is nothing to defer to. `WindowSaveState::restores()` is
  the single predicate for both halves — a mode that saved but never restored would only ever
  accumulate a stale file.
- **Layout, not session.** Each pane gets a fresh shell in its saved directory; scrollback and shell
  state are gone. Ghostty is the same (macOS restores surfaces, not their history).
- **The snapshot is taken in `App::retire`, not `on_exit`.** Quitting *is* closing the last window,
  so by the time `on_exit` runs there are no windows left to read. `App::last_state` is refreshed at
  the top of every retire, which makes it describe exactly the moment before the close that ended
  the process — and closing windows one at a time therefore drops the earlier ones, matching macOS.
- **The file is consumed on read.** It describes one specific exit; leaving it would resurrect that
  layout after a later crash that never wrote its own, which reads as giest ignoring everything the
  user has done since.
- **Parsing is total.** A line-oriented text format (one record per line, free-form fields taking
  the rest of the line so nothing needs escaping) with a preorder tree — unambiguous for a binary
  tree whose interior nodes always have two children, so no delimiters or indentation. Anything
  malformed drops the affected record; a bad state file can never block startup, which is the worst
  possible trade for a convenience feature.
- **Restore is best-effort per leaf.** A saved directory that no longer exists falls back to the
  default (spawning into it would fail and silently cost a pane), a shell that won't spawn collapses
  out of its split rather than taking the tab with it, and a window that ends up with no tabs keeps
  the one it already had — the user gets their original window, never none.
- **The first window's initial shell is spawned and immediately dropped.** `Window::first` does the
  once-per-process setup (atlas, theme, profiles) and opens a session on the way; deciding before
  that would mean moving all of it. One short-lived shell is the cheaper trade.
- **Not saved: window size/position** (`window-width`/`-height`/`-position-*` already cover that for
  every launch), **split zoom** (a transient view; restoring one hides panes), tab colors, the
  per-pane profile — every restored pane runs the default profile, since a pane doesn't record which
  profile opened it — and a pane's `set_surface_title` override, which belongs to the shell session
  that is being replaced rather than to the layout.

### Clipboard permissions + paste protection — ✅

All five Ghostty keys, with Ghostty's defaults: `clipboard-paste-protection = true`,
`clipboard-paste-bracketed-safe = true`, `clipboard-trim-trailing-spaces = true`,
`clipboard-write = allow`, `clipboard-read = ask`. `config::paste_is_unsafe` is a verbatim port of
`Surface.zig`'s `completeClipboardPaste` rule, check order included.

- **One gate, enforced structurally.** Every paste — keyboard, context menu, middle-click,
  `paste_from_clipboard` — goes through `Session::paste_str`; `encode_paste` has exactly one
  caller (the private `write_paste`). Previously the `Event::Paste` arm encoded inline and the
  four app-side paths used the helper, so there were two independent writers to keep in step.
- **The pending request lives on the `Session`**, not the app, so the answer routes back without a
  pane index that a split-close or tab reorder could invalidate. A prompt raised by a background
  tab waits until you switch to it — nothing happens without an answer, which is the safe default.
- **At most one prompt at a time.** A program spamming OSC 52 can't stack dialogs: a newer program
  write replaces a waiting one (last write wins), and anything else arriving while a dialog is up is
  refused — which, unlike before, *answers* the program (see the 5522 ledger below).
- **The dialog has no Enter-to-accept.** Escape denies; allowing takes a click. A security prompt
  that a reflexive Return gets through isn't one. This is a deliberate divergence from the
  close-confirmation dialog, which does map Enter.
- **The preview is sanitized** (`app::preview_text`): control bytes are rendered as visible
  symbols and the text is capped, because it's chosen by whoever produced the paste — an escape
  passed through could dress the payload up as dialog chrome, and a megabyte-long paste could push
  the buttons off screen.
- **OSC 52 read is now answerable.** giest previously refused unconditionally; that is still
  available as `clipboard-read = deny`, but the default matches Ghostty's `ask`. *(Superseded: the
  engine now parses OSC 52 and the policy is in `clipboard.rs`; see the next ledger.)*
- **`clipboard-trim-trailing-spaces` was previously hardcoded on** (`extract_selection` always
  trimmed); it is now the configurable default.
- Readonly mode and its indicator are now done, and secure input is recorded as N/A on Windows —
  see their ledger.

### Kitty clipboard (OSC 5522) + engine-side OSC 52 / pwd — ✅ divergences

OSC 5522 reads and writes, the targets listing, and paste events (mode 5522) work, under the same
`clipboard-read` / `clipboard-write` / `clipboard-write-limit-bytes` and the same dialog as OSC 52.
Probed first: `conpty_passthrough::kitty_clipboard_protocol_survives_conpty` shows every 5522 form
and `CSI ? 5522 h` reach us through ConPTY.

- **One path for both protocols; the OSC 52 and OSC 7 side-scanners are gone.** Installing the
  engine's clipboard callbacks (needed for 5522) makes the engine handle OSC 52 too, so keeping
  `osc52.rs` would have answered every request twice. The engine path is a superset: chunked
  sequences, both terminators, targets, the "clear" shape. `osc7.rs` went for the same reason
  the CLAUDE.md gotcha existed: `Terminal::pwd()` used to be empty and now isn't —
  `ghostty_vt::tests::pwd_comes_from_the_engine` pins BEL/ST, split chunks, percent-escapes and a
  WSL path. The engine also takes OSC 9;9 and OSC 1337 CurrentDir, which the scanner didn't.
- **The binding's `on_clipboard_write` was broken, silently.** The pinned C API answers through a
  `reply` function pointer; the vendored wrapper still *returned* the result, which the C side
  ignored, so every write would have been denied. Fixed giest-locally, with `on_clipboard_read`,
  `Terminal::paste` and `Mode::PASTE_EVENTS` added beside it.
- **`ask` cuts the engine's refusal back out.** The callbacks are synchronous and an unanswered
  request is refused immediately; upstream's advice is to block on a modal, which the UI thread
  can't. The callback records the response-buffer offset instead, the refusal written there is cut
  out after `vt_write` and held, and the answer either releases it, flips it to `DONE` (writes), or
  replays the read into the engine under a one-shot grant so the engine formats the reply.
- **A refused OSC 52 read now gets an empty reply** (`ESC]52;c;ESC\`), where giest used to send
  nothing. That is lib-vt's behaviour and xterm's; a program no longer waits for a reply that
  never comes.
- **MIME types: text only.** `text/plain` and its spellings are served; a write keeps its text
  representation and drops HTML/images (rather than failing the whole write and losing the text),
  a write with no text is `ENOSYS`, and a read simply omits types it can't serve — the protocol's
  own "not available". The targets listing reports `text/plain` and needs no permission, as in
  kitty and upstream.
- **No `remember`.** A 5522 password could let the user grant a program for the session; the
  deferred reply goes out after the callback returns, when the engine no longer accepts a grant.
  Every request under `ask` prompts.
- **A paste event's follow-up read bypasses `clipboard-read`** and is served the *pasted* text, not
  whatever the clipboard holds by then. The user already pasted; refusing would break paste.
  Paste protection doesn't apply to an event either: nothing is typed into the program. The grant
  is keyed on password *and* name (`name=` must be echoed), as in upstream's own test.
- **Windows has no primary selection**: `loc=primary` reads and writes the one clipboard, as OSC 52
  `p`/`s` always did here.

### Keybind action coverage — ◐

Ghostty's `Action` union has 85 members; giest's was diffed against it directly rather than
guessed at, and the ones needing no new subsystem are now wired: `clear_screen`,
`copy_title_to_clipboard`, `toggle_readonly`, `move_tab:N`, `set_font_size:N`,
`scroll_page_lines:N`, `scroll_page_fractional:N`, `prompt_tab_title`, `quit` /
`close_all_windows`.

- **`clear_screen` writes `ESC [ 2J ESC [ 3J ESC [ H` into the engine** rather than poking the grid,
  so the terminal's own state machine stays consistent with what it just did.
- **`toggle_readonly` drops the bytes *after* the input loop**, so scrolling, selection and copy all
  still work. A pane you can read and search without disturbing is the point; a frozen one isn't.
- **`move_tab:N` clamps rather than wraps** (Ghostty clamps too), and delegates to the existing
  drag-reorder path, which already clears the stale `renaming` index.
- **`scroll_page_fractional` is stored ×100** so `Action` stays `Copy + Eq` without carrying a float.
- **`equalize_splits` is now real** (split ratios landed). It was once a no-op, and before that
  mapped onto `ClearSelection` — a real action that silently dropped the selection. *(Historical;
  see the selection-migration ledger.)*
- **`text:`, `csi:`, `esc:`, `set_tab_title:`, `set_surface_title:` are now done.** They needed
  `Action` to own strings, which it does — `Arc<str>` payloads, `Clone` instead of `Copy`. See their
  own ledger; the refactor cost 16 mechanical errors, measured rather than estimated.
- **`undo`/`redo` are now done** — see their own ledger below.
- **The five search actions are now done** — `start_search`, `end_search`,
  `navigate_search:next|previous`, `search_selection` and `search:<text>`, alongside giest's own
  `toggle_search`. See their ledger below.
- **`inspector:toggle|show|hide` is now done** — see its ledger below. Upstream's action union is
  now fully covered apart from `show_gtk_inspector`, which is GTK's widget inspector.

### Keybind sequences — ◐ divergences

`keybind = ctrl+a>n=new_tab`. The keymap now stores *sequences* rather than chords, and a plain
chord is simply a sequence of length one — so single binds and prefix bindings share one lookup
path instead of two that could disagree.

- **The leader has to be reserved from the shell, and `lookup` can't do it.** `ctrl+a` in
  `ctrl+a>n` is bound to no action of its own, so the existing "is this chord bound?" test says no
  and the key goes straight to the shell — the sequence would never start. `Keymap::starts_binding`
  is the predicate `session::decide_key` needs, and a test pins it, because `ctrl+a` is exactly the
  key a tmux user reaches for and the failure is silent.
- **A dead end flushes rather than eats.** `ctrl+a` then an unbound key sends *both* to the shell
  (`Session::send_chords`), matching Ghostty. The leaders were swallowed as they were typed, so
  they have to be delivered late, in order.
- **An exact binding beats being a prefix**, so `ctrl+a` and `ctrl+a>n` can coexist without the
  bare chord hanging forever on a second key that could never take effect.
- **Not done: the `all:` trigger flag** (the same gap as Tier-3 broadcast input).
  **`unconsumed:` is now done** — see its ledger. **`global:`, `performable:` and named key
  *tables* are now done** — see the quick-terminal, selection-interaction and key-table ledgers.
  `global:` is
  inherently non-sequenceable (the OS delivers one key, not a leader and a follower), which is true
  upstream too, so a global trigger must be a single chord.
- **Not done: `end_key_sequence`**, Ghostty's action for flushing the prior keys but *not* the one
  that triggered it. giest's dead-end flush includes the triggering key, which is Ghostty's default
  behaviour; only the opt-out is missing.

### Custom shaders — ✅ divergences

**Done — the whole shader *contract*:** `shader.rs` translates Shadertoy-format GLSL to WGSL
(GLSL → naga IR → WGSL); `tests/shader_gpu.rs` proves a **real device** accepts the result for a
trivial shader, one that touches *every* uniform in the block, and Ghostty's own CRT test shader;
and `shader::Globals` is the matching 352-byte uniform struct, with every field offset asserted
against naga's own layout (`uniform_layout_matches_naga`). That last test is the one that matters:
a layout drift produces no error at all, just shaders fed wrong numbers, so the offsets are taken
from the compiler rather than hand-derived from the std140 rules.

**The pipeline is done too**, along with both config keys. The terminal renders into an offscreen
texture inside `prepare` (the only hook with a `CommandEncoder` — `paint` is handed a pass egui has
already begun, and passes can't nest), each shader ping-pongs between two framebuffer-sized targets,
and `paint` blits the result. Sizing the targets to the **whole framebuffer** rather than the
terminal area is what lets every existing instance coordinate stay valid with no remapping.

Verified by measurement rather than by eye, each probe isolating one link:

| Probe | Result |
|---|---|
| Solid two-tone shader | renders; the chain runs at all |
| Vertically asymmetric | correct orientation (see the Y note below) |
| Pass-through, `background = #808080` | `#808080` exactly — the shader sees the terminal, and the offscreen round-trip is colour-exact |
| Two chained shaders (halve red, then blue) | `#408040` exactly — the ping-pong reads the right texture each pass |

That third row is the one that mattered: cells on the default background emit no quad, so without
moving the window fill into the renderer (`TermFrame::window_fill`) a shader would sample
transparent black and the screen would go dark.

Three naga constraints found the hard way, all of which shape [`shader::PREFIX`] and would
otherwise be rediscovered by whoever does the pipeline:

- **naga's GLSL frontend has no combined `sampler2D`.** It supports only the Vulkan-style separated
  `texture2D` + `sampler`, and rejects `uniform sampler2D` with "Not implemented: variable
  qualifier". Since every Shadertoy shader writes `texture(iChannel0, uv)`, the prefix declares the
  two halves and re-forms them with `#define iChannel0 sampler2D(tex, smp)`.
- **Functions must be emitted in dependency order.** Ghostty's prefix defines `main()` (calling
  `mainImage`) up front and relies on glslang to link; naga's IR does not, and fails validation with
  *"[0] of kind Function depends on [2] … which has not been processed yet"*. So giest appends the
  entry point as a **suffix**, after the user's code. A useful side effect: dropping the forward
  declaration means a misspelled `mainImage` is now a compile error rather than a call to nothing —
  and `compile` checks for the name explicitly, since naga does no link checking at all.
- **`float iChannelTime[4]` is not valid in a WebGPU uniform buffer.** Array stride must be a
  multiple of 16 and that one is 4. naga's own validator passes it; only the *device* rejects it,
  which is exactly why `tests/shader_gpu.rs` exists. A `vec4` indexes identically in GLSL, so
  `iChannelTime[0]` still works. (`iChannelResolution[4]` needs no change — `vec3` aligns to 16.)
- Also: only GLSL core **440/450/460** are accepted, so the prefix is `#version 450`, not Ghostty's
  430; and `gl_FragCoord` cannot be redeclared, as naga treats it as a built-in.

Further divergences:

- **`fragCoord` is Y-down, matching Ghostty but not shadertoy.com.** Shadertoy is Y-up for both
  `fragCoord` and channel textures; WGSL (and Metal) are Y-down for both. A flip was implemented
  first and *measured* upside down: in a fullscreen post-process the fragment writes to the pixel it
  is at, so flipping the coordinate moves where the output lands relative to where the input was
  read. The two conventions must agree, and reaching Y-up would need the `v` of every
  `texture(iChannel0, …)` flipped — user code we can't touch. Ghostty's prefix doesn't flip either,
  so a shader written for Ghostty behaves identically here; only one ported raw from Shadertoy is
  mirrored, on both.
- **`iResolution` is the framebuffer, not the terminal surface**, since that is the space the
  offscreen targets and every instance coordinate live in. It differs from the surface by the tab
  strip's height.
- **A failed shader is reported and skipped**, never fatal and never a blank screen — same as
  Ghostty, which also compiles on the render thread and logs.
- **`iMouse`, `iDate` and `iSampleRate` are zero**, and `iChannelTime`/`iChannelResolution` describe
  channels giest doesn't have. Ghostty leaves most of these inert too.

### OSC 9;4 progress → Windows taskbar — ✅ divergences

Ghostty's `progress-style` key (a bool despite the name) plus ConEmu's five `OSC 9;4` states,
parsed by the *same* function that decides notification-vs-ConEmu in `osc_notify.rs`.

- **The taskbar button, not an in-window bar.** Ghostty renders progress inside its own surface,
  because macOS and GTK have no equivalent. Windows does, and it's where ConEmu — which invented
  this sequence — and Windows Terminal both put it, so a Windows user already reads it without being
  told. `taskbar.rs`, via `ITaskbarList3`.
- **The COM vtable is declared by hand.** `windows-sys` ships no COM interfaces at all (they're in
  the much heavier `windows` crate); `ole32` is resolved lazily via `GetProcAddress`, the same trade
  `bell.rs` and `blur.rs` make. `taskbar::available()` exists purely so this is testable — a
  taskbar button's progress cannot be read back, so an `#[ignore]`d test asserting
  `CoCreateInstance` + `HrInit` succeed is the only automatable check that the CLSID, IID and vtable
  layout are right. Without it a wrong vtable prefix would fail in complete silence.
- **A state change with no percentage carries the previous one forward.** `9;4;2` (failed) almost
  always arrives bare, and Windows has no "recolour in place" call — so without this a failure would
  snap the bar to 0 or to full instead of leaving it where the job actually stopped.
- **One button, many panes**, so states merge worst-news-first: error → pause → indeterminate →
  the *lowest* determinate percentage. Ghostty never faces this, since it draws per-surface.
- **Parsing lives with the notifications on purpose.** `9;4;1;50` is a progress report and
  `9;4 tests passed` is a notification; that is one decision, and two parsers would be two copies of
  it free to drift.
- Verified: `tests/conpty_passthrough.rs` confirms ConPTY forwards `OSC 9;4` (it is Microsoft's own
  console and ConEmu's own sequence, but "obviously it passes" is the assumption that cost an
  afternoon on kitty graphics), and a live run drives every state through a real giest without a
  fault — which is what a wrong slot for `SetProgressValue`/`SetProgressState` would produce.

### Desktop notifications — ◐ divergences

`OSC 9` (iTerm2 form) and `OSC 777` (rxvt form) plus the `desktop-notifications` key. The other half
of this row — `notify-on-command-finish` and its `-action` / `-after` keys — is **not** done; it
needs OSC 133 **C/D**, and giest's shell hooks currently inject only A/B (see below).

- **Side-scanned, like OSC 7 and OSC 52.** libghostty-vt parses both forms, but its read-only stream
  drops the payload before anything we can read, so `osc_notify.rs` runs its own streaming parser
  over the same bytes. `is_conemu` mirrors `osc9.zig` branch for branch, *including* its
  fall-through: a payload that starts a ConEmu shape but doesn't complete it is a notification
  (`9;4;50` is a progress report, `9;4` is a notification whose body is `4`).
- **One genuinely lossy overload, inherited from upstream.** `OSC 9;5` is ConEmu's "wait for input"
  with no payload and no further validation, so a notification body starting with a bare `5` is
  swallowed. Ghostty does the same; there is no way to disambiguate from this side.
- **Notification-area balloon, not a WinRT toast.** `ToastNotificationManager` needs a registered
  AppUserModelID, which means installing a Start Menu shortcut — a permanent machine-wide side
  effect. `Shell_NotifyIconW` + `NIF_INFO` needs only an `HWND`, and Windows 10/11 render those
  balloons as toasts. Cost: one tray icon, added **lazily** on the first notification and removed on
  exit, so a session that never notifies never shows one.
- **App-scoped, not per-window.** There is one tray entry per process, and only the root window has
  a reachable `HWND` (a child viewport's isn't exposed) — so notifications from *every* window are
  drained into one app-level call. A per-window call would silently drop every secondary window's.
- **Silent by design.** `NIIF_NOSOUND` is set: the bell is a separate configurable feature, and
  anything that rings *and* notifies would otherwise make two noises.
- **Truncated at 63 / 255 characters** with an ellipsis. Those are Windows' own `szInfoTitle` /
  `szInfo` limits, which it otherwise enforces silently.
- **A burst is capped at 4 per pump.** No focus gate and no rate limit otherwise (Ghostty shows
  these regardless of focus); the cap is only a runaway guard, since Windows coalesces balloons and
  a program looping on OSC 9 would otherwise stack them without bound.
**`notify-on-command-finish`** (+ `-action`, `-after`) is done too, on OSC 133 `C`/`D` read by
`osc133.rs`. Its divergences:

- **The shell hooks emit `D` but not `C`.** `D` (command ended, with its exit code) goes at the top
  of the prompt, the only post-execution hook either shell offers. `C` (command *started*) would
  need a **pre**-execution hook: PowerShell has none short of overriding a PSReadLine key handler —
  and PSReadLine isn't always loaded — while cmd has none at all. So giest starts the clock from the
  Enter *it* sent, gated on the cursor being on a prompt row (`Session::note_command_submitted`).
  That is within microseconds of the real thing, works identically for both shells, and adds no
  dependency. A shell that *does* emit `C` takes precedence automatically.
- **An unmatched `D` is ignored**, which is load-bearing rather than defensive: emitting `D` from the
  prompt means every session opens with one for a command that never ran, and Enter on an empty
  prompt produces another.
- **cmd cannot report an exit code.** Its `prompt` is expanded by the console and has no token for
  `%ERRORLEVEL%`, so its `D` is bare and the notification reads "Command finished" with a duration
  and no verdict. PowerShell reports the real code, consulting `$?` **and** `$LASTEXITCODE` —
  neither alone is right (`$?` misses a native program's code, `$LASTEXITCODE` misses a failed
  cmdlet), and `$?` must be read as the prompt's very first statement or it is already clobbered.
  A test pins that ordering, and `tests/conpty_passthrough.rs` verifies the whole thing against a
  real PowerShell.
- **`action = bell` reuses the whole `bell-features` path**, so a user who has configured the bell to
  flash the pane or flag the taskbar gets that here too — which is what the option means upstream.
- The focus test uses the window's **last observed** focus, not the running pass's: `pump_all` runs
  for every window from the root pass, where `i.focused` would answer for the wrong viewport.
- Not covered: WSL and custom shells get no marks unless they emit their own (then it just works).

- **Verified against ConPTY**, which is not a formality here — ConPTY re-emits its own stream and
  silently drops what it doesn't understand, which is exactly what blocks kitty graphics.
  `tests/conpty_passthrough.rs` (ignored; needs a real shell) pins that OSC 9 and OSC 777 survive,
  along with the already-shipped OSC 7 / 52 / 133, and that APC is still stripped by the inbox
  conhost — inverted, so it *fails* if a future Windows build unblocks kitty graphics. Run with
  `GIEST_TEST_PASSTHROUGH=1` (after `scripts/fetch-conpty.ps1`) it asserts the opposite: over the
  sideloaded ConPTY, APC and ENQ arrive.

### background-image — ✅ divergences

All five Ghostty keys (`background-image`, `-opacity`, `-position`, `-fit`, `-repeat`), PNG and
JPEG, with the fit/position math and the compositing formula taken from `bg_image_vertex` /
`bg_image_fragment` in `renderer/shaders/shaders.metal`. Notes:

- **The fit/position math runs on the CPU, not per vertex.** Upstream recomputes `dest_size` and
  `dest_offset` in the vertex shader from a uniform, where it can't be unit-tested; giest computes
  it once per frame in `bgimage::dest_rect` — a pure function with a table test pinned to Ghostty's
  branches, plus invariant tests (`cover` never leaves a gap, `contain` never overflows) that the
  shader form has no way to express. Same numbers, testable.
- **Texture coordinates are image-normalized, not pixel-space.** Ghostty samples with
  `coord::pixel` and wraps with a double `fmod`; giest folds the mapping into the quad's own `uv`
  (which the existing vertex stage already interpolates), so `repeat` is one `fract` and
  "off the image" is a `0..1` bounds test. No second sampler and no new vertex plumbing.
- **The composite is rearranged from premultiplied to straight alpha**, because this pipeline blends
  with `SrcAlpha`, not `One`. The output is divided by `max(t, 1)` so an image opacity **above 1**
  overexposes exactly as it does upstream rather than merely saturating — the case Ghostty documents
  (`background-opacity = 0.5` + image opacity `1.5` → an effective `0.75`).
- **It replaces the window-background fill rather than layering over it.** With an image configured,
  `render_active` skips its `rect_filled` and the shader paints the background color itself — which
  is what Ghostty's bg-image pass does too, and here it is *mandatory*: two translucent layers over
  the same rect composite to `1-(1-a)²` (see the standing rule in CLAUDE.md).
- **One image per window, not per terminal.** Ghostty documents its image as per-terminal, repeated
  across splits, and warns that it's duplicated in VRAM per terminal ("a future improvement will
  address this"). giest draws it once across the whole terminal area and uploads one texture per
  process, so splits share it — a fix, not a behaviour change, and the VRAM warning doesn't apply.
- **CMYK JPEGs are rejected.** `jpeg-decoder` doesn't expose the Adobe inversion flag, so any
  conversion would silently render half of them inverted. Grayscale (8- and 16-bit) and RGB load.
- **The format is sniffed from the file's magic bytes**, not its extension.
- **Failures are non-fatal and reported once**, at load time — a bad path logs and leaves the plain
  background rather than retrying every frame.
- Not covered: the image is not re-read when the *file* changes on disk, only when the config is
  reloaded (`background-image` pointing at a live-updating file won't animate). Ghostty is the same.

### Scrollbar — ✅ divergences

Ghostty exposes exactly one key, `scrollbar = system | never`, and no width/opacity/always knob;
giest matches that. Notes where the implementation differs or is Windows-specific:

- **`system` is an auto-hiding overlay.** Ghostty's macOS apprt forces `scrollerStyle = .overlay`
  even against the OS preference, so "system" already means an overlay upstream; giest does the same
  and never reserves a gutter. Hidden at rest, raised by any scroll or by hovering a 4 pt band at the
  pane's right edge, fading after ~1 s.
- **`Terminal::scrollbar()` is deliberately unused.** The binding exposes it, but it's documented as
  expensive at arbitrary pins — precisely when a scrollbar is on screen, and per-pane per-frame in a
  split — and its integer `offset` is the whole-line engine pin, which would make the thumb step a
  full cell during a smooth scroll. giest reconstructs `{total, offset, len}` from
  `scrollback_rows()` (already read every frame) plus the continuous `scroll_px`.
  `scrollbar_state_matches_the_reconstruction_from_scrollback_rows` pins the two together, so an
  upstream change to the arithmetic fails a test rather than skewing the thumb.
- **Minimum thumb is 20 pt**, not the 4.0 in Ghostty's `pagelist.zig` — that figure is from its ImGui
  *inspector*, whereas the app's scroller is an `NSScroller` with AppKit's own ~20 pt knob. At a 10k
  scrollback the unclamped thumb is under 2 pt.
- **The thumb is clamped into the track** once the minimum-size floor engages. Ghostty's inspector
  formula (`top = offset/total*H`) lets `top + len` exceed the track and run the thumb off the
  bottom; giest scales by `travel = H - len` instead, which is the identical value whenever the floor
  is inactive and keeps the drag inverse exact.
- **One frame of content lag while dragging.** The thumb is exact under the cursor (the drag writes
  `scroll_px` directly, bypassing the ease); the grid follows on the next frame, since input and
  scroll easing run before the scrollbar pass. Structurally the same as Ghostty's macOS scroller
  posting `scroll_to_row` to the core.
- **The visible bar overlays the last column(s) at `window-padding-x < 14`**, which includes the
  default of 2 (Ghostty's own). That is deliberate parity: Ghostty forces an overlay scroller, so the
  bar floats over the grid rather than costing columns; the knob carries a 1 px background outline to
  stay legible over text. Raise the padding past ~14 and it sits inside the gutter instead. While
  hidden, only 4 pt is interactive, and with hover-only sense — so a click at the right edge still
  reaches the terminal.
- **Hidden while the program reports the mouse** (an alt-screen TUI has no scrollback, and the bar
  must not compete for the pointer). One bar per pane in splits, painted above the unfocused-split
  dim so it stays legible.
- **The scroll pin is distance-from-bottom, not a content pin**, so a streaming pane drifts under a
  scrolled-up viewport. The thumb reflects that faithfully — the scrollbar *exposes* a pre-existing
  divergence rather than introducing one.
- **`scroll_to_row:N` is bound-able.** Upstream leaves it unbound (it exists so the scroller can
  drive the core); giest parses it in `keybind` too, which costs nothing and keeps the absolute
  seek as real public API.
- **Fixed in passing:** `shift+home/end/pageup/pagedown` were handled by a hardcoded branch in
  `decide_key` rather than the keymap, so `keybind = shift+home=unbind` silently did nothing. They
  are now ordinary `default_binds` entries.

### Kitty graphics — ✅ over a sideloaded ConPTY (inbox conhost still strips APC)

#### ConPTY passthrough research (measured on Windows 11 build 26200.9457)

- **`PSEUDOCONSOLE_PASSTHROUGH_MODE` (`0x8`) does nothing we can use on any ConPTY we could
  test.** It was an experimental flag in Windows Terminal's OpenConsole 1.17–1.21 (never
  documented for the inbox API). On this machine the inbox `kernel32` ConPTY — whose host is
  still `conhost.exe` **10.0.26100.1**, i.e. no newer than 24H2 — *accepts* it (`S_OK`) and
  silently ignores it: APC and ENQ are still stripped, output byte-identical to the flagless run.
  So "does `CreatePseudoConsole` reject it" is not a usable capability probe.
- **What actually forwards APC is the rewritten ConPTY (OpenConsole 1.22+)**, shipped out-of-band
  as the MIT-licensed `Microsoft.Windows.Console.ConPTY` NuGet package (`conpty.dll` +
  `OpenConsole.exe`). Measured with 1.24.260710001: APC (`ESC _ G … ESC \`) and ENQ (`0x05`) arrive
  verbatim **with or without** the flag — the rewrite forwards what it doesn't understand, which
  makes a separate passthrough mode moot there. portable-pty already prefers a `conpty.dll` found
  on the DLL search path; the vendored copy (`vendor/portable-pty`, `[patch.crates-io]`) loads it by
  absolute path next to the exe instead (the search path includes the cwd — DLL planting) and makes
  it vetoable.
- **What changes with the sideloaded ConPTY** (all measured by `tests/conpty_passthrough.rs` in
  both modes, `GIEST_TEST_PASSTHROUGH=1`):
  - *Opening handshake.* Inbox: `ESC[6n ESC[?9001h ESC[?1004h ESC[m ESC]0;<exe path>BEL ESC[?25h`.
    Sideloaded: `ESC[1t ESC[6n ESC[c ESC[?1004h ESC[?9001h ESC[1;1H`. It still blocks on the
    `ESC[6n` answer (the harness rule in CLAUDE.md holds), and additionally asks DA1 (`ESC[c`),
    which the engine answers (`ESC[?62;22c`). No synthetic title — tabs show giest's own name until
    the shell sets one.
  - *Stream shape.* Inbox re-renders; the rewrite forwards the child's own sequences (OSC
    7/9/9;4/52/133/777, DECSCUSR and the cmd/pwsh/bash shell-integration tests all still pass).
  - *win32-input-mode* (`?9001h`) is requested by both, so key input is unchanged.
  - *Resize.* `set_resize_pull_scrollback(false)` stays right: both implementations keep their own
    scrollback-less buffer and reflow on `ResizePseudoConsole`. Not separately re-verified by a
    resize test — **needs a human** resizing a window with long wrapped lines under both modes.
  - *ENQ* now arrives too, so `enquiry-response` becomes implementable (sideloaded only).
- **Config:** `conpty-passthrough = auto | true | false` (**giest-specific**). `auto` (default) and
  `true` use a `conpty.dll` beside `giest.exe` when there is one and also pass the flag (for 1.17–1.21
  builds); `false` forces the inbox conhost. Startup-only (the library is loaded once). With no
  `conpty.dll` present, `auto` behaves exactly as before, so the default changes nothing until the
  pair is installed: `pwsh scripts/fetch-conpty.ps1` (debug + release target dirs).
- **Risks of shipping it on by default:** a second, Microsoft-versioned console host to ship and
  keep updated (it is what Windows Terminal runs, so it is well exercised); `OpenConsole.exe` must sit
  beside `conpty.dll` or spawns fail; legacy console apps now run under OpenConsole's semantics rather
  than conhost's (same as in Windows Terminal); programs that emit garbage APC/ENQ now reach the
  engine instead of being filtered; and the glyph-protocol hole below, which had to be closed first.

**The original blocker (inbox conhost): ConPTY strips APC sequences, so no kitty graphics command
ever reaches the VT engine.** ConPTY does not pipe a child's output through — it *re-renders* it and emits its own VT
stream, dropping sequences it doesn't understand. APC (`ESC _ G …`), which the kitty protocol uses,
is one of them. Traced end to end: the shell emits correct bytes (`<27>_Ga=T,t=d,f=24,…<27>\`), and
`TerminalEngine::write` never sees them.

The fix exists but is not exposed: `PSEUDOCONSOLE_PASSTHROUGH_MODE` (`0x8`), which portable-pty
declares at `src/win/psuedocon.rs:31` behind `#[allow(dead_code)]` and never passes —
`CreatePseudoConsole` gets only `RESIZE_QUIRK | WIN32_INPUT_MODE` (`:83-90`). Enabling it means
vendoring and patching portable-pty (the pattern already used for `libghostty-rs` and `egui-winit`),
and it changes stream handling globally rather than just for APC, so input, resize and legacy-app
behaviour would all need re-verifying. **This affects any Windows terminal, not just giest** — which
is also why upstream Ghostty offers no Windows precedent here.

**What is built, unit-tested and ready for the day the PTY can deliver APC** (C1–C4 of the plan):
- `image-storage-limit` config (Ghostty's name/default), applied at session creation and on reload.
- Engine plumbing: `ImageData`/`ImagePlacement` on `GridSnapshot`, a reusable `PlacementIterator`,
  the placement walk, an `Arc` pixel cache copied once per image id with evict-by-absence, and the
  `(z, image_id)` sort. Verified by tests driving **real escape sequences** into the engine — a 1×2
  RGB image becomes a correctly sized placement, a delete clears it, and pixels are shared between
  snapshots rather than re-copied.
- PNG decoding (`f=100`, what `icat` sends) via a `DecodePng` impl over the `png` crate.
- Renderer geometry: `image_layer`, `image_rect`, `image_uv`, `image_visible`, `split_draws`, the
  three z-layer emission points, and per-image draw splitting in `paint`.

**Corrections to earlier assumptions**, both now pinned by tests:
- Kitty graphics were **not** disabled by default — libghostty's library default is a 10 MB storage
  limit, so the protocol was already partly live. What `image-storage-limit` really controls is the
  budget, and specifically that **zero** disables it and wipes stored images.
- The kitty storage's dirty flag is not part of the render state's, so placements are refreshed on
  every snapshot ahead of the dirty-skip; otherwise a deleted image would persist forever.

**Finished once APC arrived** (verified live with a real sender — PowerShell emitting
`ESC_Gf=100,a=T,c=20,r=10;<base64 png>ESC\` — and a DPI-aware `PrintWindow` capture as a sanity
check: a 64² half-red/half-blue PNG lands as exactly 20×10 cells with equal red/blue halves):
- **Rendering:** mode 5 in the instanced pipeline samples a per-image texture bound at a new
  bind group 1 (placeholder bound for every other draw). Textures live in `GpuResources::img_cache`,
  **never the glyph atlas**, keyed by the `Arc<ImageData>` address (kitty ids are per terminal, so two
  panes can both have "image 1"); uploaded in `prepare` (the only hook with a device), evicted once
  only the cache still holds the `Arc` — not on first absence, since every window runs `prepare`
  against the one shared cache.
- **Re-transmit and animation frames:** the engine's pixel cache now keys on the image's
  **generation stamp**, which changes on every re-transmit *and* whenever an animated image's current
  frame changes (`image.data()` is the current frame). Fixes the old "same-size re-transmit is
  invisible" limitation and makes client-driven animation (`a=f` + `a=a,c=N`) work — verified live
  (red root frame → green frame 2).
- **Relative placements (`P=`/`Q=`/`H=`/`V=`):** resolved by the engine; verified by a unit test and
  live (child drawn exactly 22 columns right of its parent).
- **Transient images:** a usage hint the engine applies to its own eviction order; nothing to do
  on the render side beyond evict-by-absence, which already exists.
- **Autoplay (`a=a,s=2|3`) does not advance.** Upstream drives it from its *renderer*
  (`ImageStorage.animationTick`, called in `renderer/generic.zig`), and that function is not in
  the C API at this pin — so there is no clock to hand it. Needs an upstream C API (or a local patch
  to the fetched source) before a redraw timer would have anything to redraw.
- **Glyph protocol (APC `25a1`, upstream d3775d1 et seq.): evaluated, not implemented, and now
  explicitly disabled.** The parser, glossary and responses are in the pinned engine and are **on by
  default** — so the moment APC arrives, giest would answer `s` with `fmt=glyf` and accept `r`
  registrations it cannot draw (apps then print PUA codepoints as tofu). The C API has only the
  on/off switch: no way to read a registered glyf outline back for rasterizing, and upstream's
  renderer half (4c34ccf) isn't in the pin. `GhosttyVtEngine::new` sets it off; a test pins that
  nothing is answered. Implementing it needs an outline getter in the C API plus a glyf rasterizer
  into the R8 atlas (upstream has one in `font/`), and the protocol itself is still experimental.

**Deliberately out of scope even once unblocked:** unicode placeholders (`U=1`) — the binding
exposes `is_virtual()` but not the diacritic decoding upstream does in its renderer, so virtual
placements are skipped; and the file / temp-file /
shared-memory transmission mediums (`t=s` is unsupported on Windows upstream, and `t=f`/`t=t` resolve
paths against a hardcoded `/tmp`).

### P5 — multi-window (done)

`ctrl+shift+n` opens a window (Ghostty's non-Darwin default); `close_window` is in the palette and
bindable but **deliberately not bound to alt+f4**, because Windows already delivers that as WM_CLOSE
→ the close-confirmation flow. All three `*-inherit-working-directory` keys are now honored by one
shared decision table — `split-inherit-working-directory` was previously hardcoded on, and
`window-inherit-working-directory` was parsed but inert.

**Divergences and limits:**
- **No acrylic backdrop or taskbar attention flash on secondary windows.** eframe exposes a window
  handle only for the root viewport, so `blur.rs`/`bell.rs` have no HWND for the others.
  `Window::hwnd` is the single seam a follow-up would fix (correlate by a unique creation title +
  `EnumThreadWindows`).
- **Font size is app-global**, where Ghostty's is per-surface: `callback_resources` is type-keyed, so
  only one `GpuResources`/`Atlas` can exist. Same cause means two windows on different-DPI monitors
  can't both be right (`px = points × ppp`).
- **Immediate viewports repaint in lockstep** — each animation tick costs one full pass per window.
  Realistic ceiling is 2–3 windows.
- **Root-slot rehosting**: closing the first window while others are open moves a survivor into the
  root slot and repositions the native root window onto the closed window's geometry, so the window
  the user closed is the one that visually disappears. Perceptual — needs eyeballing.

### Divergences recorded in this batch
- `bell-features` `border` now defaults **false** (Ghostty parity), changing giest's previous
  always-flash behavior — `bell-features = border` restores it.
- `bell-audio-volume` is parsed, clamped and stored but **not honored**.
- `confirm-close-surface = true` uses an **OSC 133 prompt heuristic**, not a real process check;
  "can't tell" confirms, so WSL/custom shells effectively behave as `always`.
- `resize-overlay = after-first` genuinely suppresses the first layout, unlike Ghostty's GTK apprt
  which treats it as `always`; the overlay also has a 150 ms fade tail where Ghostty's is a hard hide.
- OSC 4 palette and 5/13-19 queries deferred; OSC 4/10/11/12 **sets** and 104/110-112 **resets**
  already work inside libghostty and are now pinned by tests.

**Verification note (per CLAUDE.md):** `cargo test` covers parser/registry/engine logic; any `render/*`,
transparency, font, or opacity change needs **human visual confirmation** in the running app
(`mise dev`) — screenshots have repeatedly produced false "it works" conclusions on these.
