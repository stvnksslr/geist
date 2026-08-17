# giest → Ghostty macOS: Feature-Gap & Parity Roadmap

giest's north-star is feature parity with the **macOS Ghostty app**. This document audits the gap and
lays out a phased roadmap. It is grounded in three sources read directly: giest's `src/` tree, the
complete Ghostty source vendored locally under `target/.../out/ghostty-src/` (the macOS Swift app *and*
the Zig core / `Config.zig`), and the Rust binding `vendor/libghostty-rs/` (to judge, per gap, whether
the terminal data already exists and just needs wiring).

**Headline finding.** giest has a strong, correct *spine* — real VT engine, splits, tabs, ligatures,
emoji, smooth scroll, command palette — but it covered a fraction of Ghostty's config surface and
~90 keybind actions, with little of the macOS app's UX breadth. *(Measured since: **94 of Ghostty's
187 config keys** are now supported — see the config-surface ledger for the re-runnable audit.)* The encouraging part: **much of the gap
is plumbing, not greenfield.** libghostty-vt already surfaces underline styles, faint/overline, OSC 8
hyperlinks, OSC 133 semantic-prompt marks, kitty graphics, the bell, and a rich selection model — data
giest's single per-cell chokepoint (`engine/ghostty_vt.rs::copy_cell`) historically discarded.

---

## Status (updated 2026-08-01)

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
  *Limitations: matches don't span soft-wrapped rows, ASCII case-folding, and matches are tracked in
  absolute screen rows so scrollback **eviction** during heavy streaming can drift them until the query is
  re-typed (Ghostty uses tracked pins — a follow-up).*

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

**Rendering / fonts** — `font-variation`; `adjust-icon-height`; legacy-computing sprites;
COLRv1 emoji.
*(`font-family` (+ fallback chains, per-style overrides, **synthetic bold/italic**),
`font-feature`/ligature toggle, `minimum-contrast`,
`bold-is-bright`/`bold-color`, `cursor-style`/`-blink`, **transparency + background-opacity**,
blur (Windows acrylic), `faint-opacity`, `cursor-opacity`, unfocused-split dimming, **custom
shaders**, **box-drawing/block/braille/powerline sprites**, the **`adjust-*` metric family** and
`isCovering` — now done.)*

**Window / UI** — titlebar/decoration styles; settings UI; inspector; about dialog; custom app icons.
*(fullscreen toggle, split zoom, tab drag-reorder, resize overlay, confirm-close-surface,
**multi-window**, **scrollbar**, **`window-theme` + the chrome design system** (`theme.rs`:
tab strip, palette, overlays and dialogs all derive their colors from
`background`/`foreground`/`palette` instead of egui's defaults, and no longer follow the OS
light/dark preference), **window/tab/split state restore** (`window-save-state`), **quick
(dropdown) terminal + global hotkey** — now done.)*

**Input / keybinds** — key tables / leader sequences; the remaining ~60 keybind *actions* (write_*_file,
set_*_title, toggle_*, send raw text/esc/csi, undo/redo, …). *(Config-driven binding + several actions
are now done.)*

**Selection / scroll / search** — `adjust_selection`; fully binding-backed (reflow-correct,
scrollback-spanning) selection, which would also give search cross-wrap matches + drift-free match
tracking. *(scrollback search, **semantic selection** (word / wrapped line / command output) — now
done.)*

**Shell integration** — OSC 133 C/D (command output marks → duration, notify-on-command-finish).
*(OSC 133 A/B prompt marks now injected; `tab-inherit-working-directory` now honored — new tabs inherit
the focused pane's cwd, like splits. `window-inherit-working-directory` is recognized but inert until
multi-window lands.)*

**Clipboard / security** — secure-input indicator; readonly mode. *(`clipboard-read`/`-write`
permission prompts, paste-protection confirmation and `clipboard-trim-trailing-spaces` — now done.)*

**Config / theming** — `palette-generate`/`harmonious`, config `include`/conditional (`config-file`),
and the **93 upstream keys still unsupported** (mostly `gtk-*`, `macos-*`, `linux-*` — see the
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
| Kitty keyboard (auto-applied) | scrollback regex search (build w/ `TrackedGridRef`) | multi-window, quick terminal, fullscreen |
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
| Split zoom + equalize | `app.rs`, `command.rs` | — | M | ◐ (zoom done; splits are always 50/50, so equalize is a no-op) |
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
| Kitty graphics (inline images) | `engine`, `GridSnapshot`, `render/mod.rs` | ✅ | L–XL | ◐ **blocked on ConPTY** — engine + geometry done, see below |
| Scrollback search overlay | `search.rs`, `session.rs`, `app.rs`, `engine`, `render` | ✋ | M–L | ✅ (single-row matches; pins deferred) |
| Custom shaders | `shader.rs`, `render/mod.rs`, `config.rs` | — | L | ✅ (Shadertoy GLSL→WGSL, offscreen chain, both keys; verified end to end) |
| background-image / blur | `bgimage.rs`, `render/mod.rs`, `config.rs` | — | M–L | ✅ (blur via Windows DWM acrylic/mica, `blur.rs`; image via `bgimage.rs` + shader mode 4) |
| Multi-window (`new_window` / `close_window`) | `app.rs`, `command.rs`, `keybind.rs` | — | L | ✅ |
| Session / window state restore | `app.rs`, `state.rs` | ◐ | L | ✅ (`window-save-state`; layout only — see the ledger) |
| Clipboard permission + paste protection | `session.rs`, `app.rs`, `config.rs`, `osc52.rs` | ✋ | M | ✅ |
| Desktop notifications + notify-on-command-finish | `osc_notify.rs`, `notify.rs`, `osc133.rs`, `profiles.rs`, `session.rs`, `app.rs` | ✋ | M | ✅ (both halves; cmd can't report an exit code — see the ledger) |
| Real scrollbar widget | `scrollbar.rs`, `app.rs`, `session.rs` | ✅ | M | ✅ |
| Migrate to binding selection model | `session.rs`, `engine` | ✅ | M–L | ◐ (semantic selection done; scrollback-spanning ranges still open) |

### Tier 3 — long tail / platform-specific
readonly mode · secure-input indicator · broadcast input ·
auto-update · about dialog /
custom icon · AppleScript / App-Intents / Services → Windows IPC ·
legacy-computing sprites · COLRv1 emoji · grapheme-width ·
window decorations / titlebar / colorspace · settings UI · inspector. *(all ⬜)*

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
7. ◐ **Kitty graphics** — engine, decoding and renderer geometry built and tested; **blocked on
   ConPTY**, see below.
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
20. Next: `config-file` (include) — the "config include/conditional" line in §1 — then the **full**
    selection migration (scrollback-spanning selections, search cross-wrap matches), the readonly /
    secure-input indicators, and `enquiry-response` (behind a ConPTY probe).

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

**187 upstream keys; giest now sets 102, of which 94 are upstream's** (the rest are giest-specific,
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
  user is still working with. IME preedit (which upstream also clears on) doesn't exist in giest.
- **Not attempted: `enquiry-response`.** ENQ is handled by Ghostty's *app* layer, not the terminal
  core, so libghostty-vt's read-only stream almost certainly drops it — and a bare `0x05` may not
  survive ConPTY at all. Per this repo's own rule, that needs a `tests/conpty_passthrough.rs` probe
  *first*; wiring it on the assumption that the bytes arrive is exactly the mistake kitty graphics
  cost an afternoon to learn.
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
  selection outlives any frame, so the trait returns plain viewport cell pairs and the binding types
  stay inside `engine/ghostty_vt.rs`. That is what makes this slice cheap where the full selection
  migration is not.
- **Off-viewport ends are clamped, not dropped.** `point_from_grid_ref` returns `None` in viewport
  space for a cell that has scrolled off, and a wrapped line or a command's output very often starts
  above the top of the screen — so the conversion runs in *screen* space and clamps. Returning
  `None` would make Ctrl+triple-click do nothing in the common case. The cost, stated plainly: copy
  only sees the visible part. **giest's selection model stays viewport-scoped**; scrollback-spanning
  selections still need the full migration (which is also why `select_all` is unchanged — the
  binding's version returns a scrollback-spanning range this model can't hold).
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
  every launch), **split zoom** (a transient view; restoring one hides panes), tab colors, and the
  per-pane profile — every restored pane runs the default profile, since a pane doesn't record which
  profile opened it.

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
- **At most one prompt at a time.** A program spamming OSC 52 can't stack dialogs, and only the
  last set/query of a pump batch is acted on (set first, so a set-then-query reports the new value).
- **The dialog has no Enter-to-accept.** Escape denies; allowing takes a click. A security prompt
  that a reflexive Return gets through isn't one. This is a deliberate divergence from the
  close-confirmation dialog, which does map Enter.
- **The preview is sanitized** (`app::preview_text`): control bytes are rendered as visible
  symbols and the text is capped, because it's chosen by whoever produced the paste — an escape
  passed through could dress the payload up as dialog chrome, and a megabyte-long paste could push
  the buttons off screen.
- **OSC 52 read is now answerable.** giest previously refused unconditionally; that is still
  available as `clipboard-read = deny`, but the default matches Ghostty's `ask`. `osc52.rs` parses
  only — all policy is in `Session::handle_osc52` — and the reply is always ST-terminated.
- **`clipboard-trim-trailing-spaces` was previously hardcoded on** (`extract_selection` always
  trimmed); it is now the configurable default.
- Not done, and still listed above: the secure-input indicator and readonly mode.

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
- **`equalize_splits` is accepted as a no-op.** giest's splits are always 50/50 so there is nothing
  to equalize, but binding it must not log an "unknown action" a user cannot act on.
- **Not possible without changing `Action`:** `text:`, `csi:`, `esc:`, `set_tab_title:`,
  `set_surface_title:` — all carry a string, and `Action` is `Copy` so a chosen action can outlive
  the UI closure that produced it (the deferred-intent pattern used throughout `app.rs`). Making it
  own strings is a real refactor, not an oversight.
- Still open and genuinely large: `undo`/`redo`, the inspector, key tables, and the finer-grained
  search actions (`start_search` / `navigate_search` / `search_selection`) against giest's single
  `toggle_search`.

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
- **Not done: named key *tables*** (`activate_key_table`), and the `all:` / `unconsumed:` /
  `performable:` trigger flags. **`global:` is now done** — see the quick-terminal ledger. It is
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
  along with the already-shipped OSC 7 / 52 / 133, and that APC is still stripped — the last one
  inverted, so it *fails* if a future Windows build unblocks kitty graphics.

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

### Kitty graphics — ◐ blocked on ConPTY

**The blocker: ConPTY strips APC sequences, so no kitty graphics command ever reaches the VT
engine.** ConPTY does not pipe a child's output through — it *re-renders* it and emits its own VT
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
  three z-layer emission points, and per-image draw splitting in `paint`. Each placement currently
  emits a magenta placeholder quad carrying its real rect and real source-crop UV, so finishing this
  is a change of `mode`/`color` plus the texture bind group.

**Corrections to earlier assumptions**, both now pinned by tests:
- Kitty graphics were **not** disabled by default — libghostty's library default is a 10 MB storage
  limit, so the protocol was already partly live. What `image-storage-limit` really controls is the
  budget, and specifically that **zero** disables it and wipes stored images.
- The kitty storage's dirty flag is not part of the render state's, so placements are refreshed on
  every snapshot ahead of the dirty-skip; otherwise a deleted image would persist forever.

**Deliberately out of scope even once unblocked:** unicode placeholders (`U=1`) — the binding
exposes `is_virtual()` but not the diacritic decoding upstream does in its renderer, so virtual
placements are skipped; animation (upstream doesn't implement it either); and the file / temp-file /
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
