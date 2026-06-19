# giest → Ghostty macOS: Feature-Gap & Parity Roadmap

giest's north-star is feature parity with the **macOS Ghostty app**. This document audits the gap and
lays out a phased roadmap. It is grounded in three sources read directly: giest's `src/` tree, the
complete Ghostty source vendored locally under `target/.../out/ghostty-src/` (the macOS Swift app *and*
the Zig core / `Config.zig`), and the Rust binding `vendor/libghostty-rs/` (to judge, per gap, whether
the terminal data already exists and just needs wiring).

**Headline finding.** giest has a strong, correct *spine* — real VT engine, splits, tabs, ligatures,
emoji, smooth scroll, command palette — but it covers a fraction of Ghostty's ~250 config options and
~90 keybind actions, with little of the macOS app's UX breadth. The encouraging part: **much of the gap
is plumbing, not greenfield.** libghostty-vt already surfaces underline styles, faint/overline, OSC 8
hyperlinks, OSC 133 semantic-prompt marks, kitty graphics, the bell, and a rich selection model — data
giest's single per-cell chokepoint (`engine/ghostty_vt.rs::copy_cell`) historically discarded.

---

## Status (updated 2026-06-19)

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
  **Deferred (documented divergences):** `font-family` fallback **chains** (multiple families for glyph
  coverage); **synthetic** bold/italic + the `font-synthetic-style` / `font-style*` keys (giest uses real
  faces or the regular fallback, no faux slant/embolden); and `font-feature` values with **spaces around
  `=`** (`cv01 = 2`) — rustybuzz's `Feature::from_str` rejects those (Ghostty accepts them), a rare form.
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

These ship with unit tests (engine capture, keymap, parser, scan, X11/contrast/blink, font
feature/discovery, search match/nav/mask, wide-char read) — **155 lib + 24 conformance tests pass**. Adversarial
Ghostty-source reviews confirmed
parity across these feature areas; the only deliberate divergence is **RIS (`ESC c`)**: giest resets the
cursor to the configured `cursor-style`, whereas Ghostty resets to a plain block until a config reload
(arguably its own quirk). Rendering/interaction remain **perceptual** and need human confirmation in the
running app (per CLAUDE.md): eyeball cursor shape/blink, bold colors, min-contrast, unfocused-pane cursors,
and a configured `font-family` / `font-feature = -calt`; curly-underline thickness tuning still outstanding.

---

## 1. The gap, by category (what Ghostty macOS has that giest still lacks)

**VT / protocols** — kitty graphics (inline images); OSC 9/777/99 desktop notifications; OSC 9;4 progress;
OSC 10/11/12 dynamic color set/query. *(OSC 8 hyperlinks, OSC 133 prompts, styled underlines — now done.)*

**Rendering / fonts** — `font-family` fallback *chains* (multiple families) + synthetic bold/italic
(`font-synthetic-style`); `font-variation`; **background-opacity / transparency**, blur,
background-image, **custom shaders**; `faint-opacity`; `cursor-opacity`; `adjust-cell-*` metrics;
box-drawing/powerline/braille sprite synthesis; COLRv1 emoji; unfocused-split dimming.
*(`font-family` (+bold/italic), `font-feature`/ligature toggle, `minimum-contrast`,
`bold-is-bright`/`bold-color`, `cursor-style`/`-blink` — now done.)*

**Window / UI** — quick (dropdown) terminal w/ global hotkey; fullscreen (native + non-native);
**split zoom**, equalize, drag-reorder; multi-window; window/tab/split **state restore**; resize overlay;
titlebar/decoration styles; real scrollbar; settings UI; inspector; about dialog; custom app icons.

**Input / keybinds** — key tables / leader sequences; the remaining ~60 keybind *actions* (write_*_file,
set_*_title, toggle_*, send raw text/esc/csi, undo/redo, …). *(Config-driven binding + several actions
are now done.)*

**Selection / scroll / search** — semantic selection; `adjust_selection`; binding-backed (reflow-correct)
selection (would also give search cross-wrap matches + drift-free match tracking). *(scrollback search — now done.)*

**Shell integration** — OSC 133 C/D (command output marks → duration, notify-on-command-finish);
`window/tab/split-inherit-working-directory` for new tabs (today only splits inherit cwd). *(OSC 133 A/B
prompt marks now injected.)*

**Clipboard / security** — `clipboard-read`/`-write` permission prompts, paste-protection confirmation,
`clipboard-trim-trailing-spaces`; secure-input indicator; readonly mode.

**Config / theming** — `palette-generate`/`harmonious`, config `include`/conditional, the ~230 remaining
options. *(theme/theme-file now done.)*

**Notifications / bell** — **audible** bell; desktop notifications; Win taskbar progress (OSC 9;4).
*(visual bell now done.)*

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
| Kitty graphics (images, placements; cargo feature) | OSC 52 read (security-gated) | box/powerline/braille sprites, COLRv1 |
| Kitty keyboard (auto-applied) | scrollback regex search (build w/ `TrackedGridRef`) | multi-window, quick terminal, fullscreen |
| Bell `on_bell` *(now wired)*, color-scheme, scrollbar geometry | | resize overlay, settings UI |
| Rich selection model (`selection.rs`) | | |
| Tracked grid refs surviving scroll/reflow | | |

> Sixel: confirmed **not** supported by Ghostty either — out of scope.

---

## 3. Architectural prerequisites

- **P1 — Config registry (M).** ✅ Done — `SETTERS` table + theme resolution in `config.rs`.
- **P3 — Extend `Cell`/`GridSnapshot` (M).** ✅ Done — rich attrs threaded through `copy_cell` + renderer.
- **P2 — Action/keybind registry (L).** ✅ Done — `keybind.rs` + `Action::name/from_name`, keymap feeds
  both `handle_shortcuts` and `decide_key`.
- **P4 — Transparent surface + per-cell alpha (M).** ⬜ Pending — request a transparent framebuffer
  (eframe/winit `with_transparent` + wgpu surface alpha) and add a background-opacity uniform / per-cell
  alpha to the `fs` shader. *Prerequisite for opacity, blur, background-image, unfocused dimming.*
- **P5 — Multi-window (L).** ⬜ Pending — separate per-window state from shared config + event loop.
  *Prerequisite for new_window, quick terminal, session restore.*

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
| Wire more keybind actions | `command.rs`, `app.rs` | mixed | M | ◐ (registry + several) |
| font-family/bold/italic | `config.rs`, `render/atlas.rs` | — | M | ✅ (chains/synthesis deferred) |
| font-feature / ligature toggle | `render/atlas.rs`, `config.rs` | — | S | ✅ |
| bold-is-bright / bold-color / minimum-contrast | `engine/ghostty_vt.rs`, `config.rs` | ✅ | S–M | ✅ |
| cursor-style / -blink / -opacity config | `config.rs`, `engine`, `decscusr.rs` | ✅ | S | ◐ (style+blink; opacity needs P4) |
| **background-opacity** | `main.rs`, `render/mod.rs`, `config.rs` | — | M | ⬜ (needs P4) |
| Fullscreen toggle | `app.rs`, `command.rs` | — | S | ⬜ |
| Split zoom + equalize | `app.rs`, `command.rs` | — | M | ⬜ |
| Tab reorder / drag | `app.rs` | — | M | ⬜ |
| tab/split inherit working-directory | `app.rs`, `session.rs` | ✅ | S | ⬜ |
| unfocused-split dimming | `render/mod.rs` | — | S | ⬜ (needs P4) |
| dynamic colors OSC 10/11/12 | side-scan + `apply_theme` | ✋ | S–M | ⬜ |
| confirm-close-surface | `app.rs` | — | S | ⬜ |
| resize overlay | `app.rs`/`render` | — | S | ⬜ |
| audible bell | `app.rs`/`config.rs` (needs a beep API) | — | S | ⬜ |

### Tier 2 — high impact, higher effort (flagships)
| Gap | Files | Binding | Effort | Status |
|---|---|---|---|---|
| Full configurable keybinds (tables/sequences) | `keybind.rs`, `config.rs` | ✅ | L | ◐ (single-chord done) |
| Quick / dropdown terminal | new module + `app.rs`/`main.rs` | — | L | ⬜ (needs P2✅,P5) |
| Kitty graphics (inline images) | `engine`, `GridSnapshot`, `render/mod.rs` | ✅ | L–XL | ⬜ |
| Scrollback search overlay | `search.rs`, `session.rs`, `app.rs`, `engine`, `render` | ✋ | M–L | ✅ (single-row matches; pins deferred) |
| Custom shaders | `render/mod.rs`, `config.rs` | — | L | ⬜ (needs P4) |
| background-image / blur | `render/mod.rs`, `main.rs`, `config.rs` | — | M–L | ⬜ (needs P4) |
| Multi-window | `main.rs`, `app.rs` | — | L | ⬜ |
| Session / window state restore | `app.rs` + persistence module | ◐ | L | ⬜ (needs P5) |
| Clipboard permission + paste protection | `session.rs`, `app.rs`, `config.rs` | ✋ | M | ⬜ |
| Desktop notifications + notify-on-command-finish | side-scan + OSC 133 C/D + Win toast | ✋/✅ | M | ⬜ |
| Real scrollbar widget | `render`/`app.rs`, `engine` | ✅ | M | ⬜ |
| Migrate to binding selection model | `session.rs`, `engine` | ✅ | M–L | ⬜ |

### Tier 3 — long tail / platform-specific
OSC 9;4 progress (Win taskbar) · readonly mode · secure-input indicator · broadcast input ·
`toggle_mouse_reporting`/`toggle_background_opacity`/`float_on_top` · auto-update · about dialog /
custom icon · AppleScript / App-Intents / Services → Windows IPC · `write_scrollback/screen/selection`
to file · box-drawing/powerline/braille sprites · COLRv1 emoji · grapheme-width / `adjust-cell-*` ·
window decorations / titlebar / colorspace · focus-follows-mouse · mouse-hide-while-typing ·
scroll-multiplier · settings UI · inspector. *(all ⬜)*

---

## 5. Recommended next steps

With Phase 0 done, the remaining Tier-1 items are mostly small, registry-backed one-liners. Suggested order:
1. ✅ **cursor-style / bold-is-bright / minimum-contrast** config (binding ✅, rides P1) — *done* (needs eyeballing).
2. ✅ **font-family / font-feature** (atlas multi-face) — *done* (needs eyeballing; chains/synthesis deferred).
3. **P4 transparent surface** → **background-opacity** + **unfocused-split dimming** + **cursor-opacity**
   (a cluster unlocked together).
4. **fullscreen** + **split zoom** + **tab/split inherit-cwd** — small UX wins via the action registry.
5. Then Tier-2 flagships: scrollback search, kitty graphics, multi-window → quick terminal.

**Verification note (per CLAUDE.md):** `cargo test` covers parser/registry/engine logic; any `render/*`,
transparency, font, or opacity change needs **human visual confirmation** in the running app
(`mise dev`) — screenshots have repeatedly produced false "it works" conclusions on these.
