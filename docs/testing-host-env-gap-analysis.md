# Host-Environment Test Suite — Ghostty Review & Gap Analysis

> **Status:** the roadmap below has been **implemented** (testability refactor +
> all four conformance areas + the perf benches and ConPTY harness). The unit
> suite went from 40 → 66 tests. See [§7 Implementation status](#7-implementation-status)
> for what landed and the one piece intentionally deferred.

---

## 1. Scope & the libghostty / host boundary

geist reuses Ghostty's VT engine (**libghostty-vt**, vendored, compiled by Zig)
but owns its entire **host environment**: the egui/wgpu renderer, the
egui→engine input translation layer, ConPTY I/O, and the tab/split window model.

The single most important fact for this analysis:

> **geist delegates escape-sequence generation to libghostty-vt.**
> `encode_key` / `encode_mouse` / `encode_paste` are thin wrappers over
> libghostty's `Encoder`, `mouse::Encoder`, and `paste::encode`
> (`src/engine/ghostty_vt.rs:276-386`).

So *protocol conformance* — CSI-u, the kitty keyboard protocol, the SGR/X10/
URXVT mouse formats, bracketed-paste framing — is **libghostty's
responsibility** and is tested upstream in Zig. We do **not** reimplement those.

What geist **owns and must test** is the **translation / gating / layout layer**
that sits around the engine:

| Owned by geist (test these)                        | Owned by libghostty (don't retest) |
| -------------------------------------------------- | ---------------------------------- |
| egui event → neutral `KeyInput`/`MouseInput`       | `KeyInput` → bytes (the encoder)   |
| reserved-combo gating (app shortcuts, scroll, zoom)| CSI-u / kitty / DECCKM rules       |
| pointer px → grid cell math, grid sizing           | SGR mouse byte format              |
| tab / split-tree layout & lifecycle                | VT parser, screen, OSC parsing     |
| ConPTY spawn / read-drain / exit detection         | grapheme/width/symbol Unicode      |
| renderer CPU instance assembly, glyph atlas        | —                                  |

---

## 2. Ghostty test inventory

Ghostty = Zig core + Swift/macOS app + GTK/Linux app. Findings from reviewing
the upstream repo (`github.com/ghostty-org/ghostty`).

### 2a. Exclude — libghostty-vt internal (covered upstream)

| Area                       | Upstream location                                   | Why excluded                          |
| -------------------------- | --------------------------------------------------- | ------------------------------------- |
| VT parser                  | `src/terminal/Parser.zig` (16 test blocks)          | Inside libghostty-vt                  |
| Screen / render            | `src/terminal/render.zig`                            | Inside libghostty-vt                  |
| OSC parsing                | `src/terminal/osc.zig`                               | Inside libghostty-vt                  |
| Unicode metrics            | `src/benchmark/{CodepointWidth,GraphemeBreak,IsSymbol}.zig` | Internal text metrics          |
| Fuzzing                    | `test/fuzz-libghostty/` (osc/parser/stream)         | VT engine memory-safety               |
| Stream/parse throughput    | `src/benchmark/{TerminalStream,TerminalParser,OscParser}.zig` | VT engine throughput          |

### 2b. Relevant — host-level concepts to mirror in Rust

| Area                  | Upstream location                            | geist mapping                                            |
| --------------------- | -------------------------------------------- | ------------------------------------------------------- |
| Key encoding          | `src/input/key_encode.zig`, `key.zig`        | Becomes a **wiring/integration** check (libghostty encodes) |
| Mouse encoding        | `src/input/mouse_encode.zig` (17 protocol cases) | Same — verify geist feeds the encoder correctly      |
| Config parsing        | `src/config/Config.zig`                       | geist equivalent already strong (`src/config.rs`)       |
| Screen clone cost     | `src/benchmark/ScreenClone.zig` (the one host-ish bench) | geist analog = snapshot copy loop cost       |
| Bench methodology     | `src/synthetic/` (`ascii`/`utf8`/`osc` generators), separate generate→measure, hyperfine | Reuse the *approach* with criterion + generators |

> **Note:** Ghostty's **macOS/Swift and GTK/Linux apps have essentially no
> automated unit tests** — host behavior (window/tab/split management, IME,
> clipboard integration) is exercised by a manual test matrix (see upstream
> `HACKING.md`). So there is no large host suite to port; we port the
> *concepts*, expressed as Rust unit tests over geist's own host code.

---

## 3. geist current coverage (40 unit tests)

| Module                        | File                        | Count | What it covers                                                |
| ----------------------------- | --------------------------- | ----- | ------------------------------------------------------------ |
| Config                        | `src/config.rs`             | 15    | palette well-formedness, hex parse, Ghostty-format overrides, empty-resets, unknown-key tolerance, gamma clamp |
| Config conformance            | `tests/config_conformance.rs` | 22  | Ghostty config-format parity: unquoted colors, comment/blank/malformed lines, last-wins, empty-resets, repeatable palette, every supported key, realistic pasted Ghostty config |
| Profiles                      | `src/profiles.rs`           | 4     | shell detection, override, case/exe-insensitive match        |
| OSC 52 / 5522 clipboard       | `src/clipboard.rs`, `src/engine/ghostty_vt.rs` | 18 | MIME choice, policy, ask cut/replay, paste events, write limit |
| Selection / word / URL        | `src/session.rs`            | 4     | text-flow extraction, word bounds, URL-under-cursor          |
| Engine snapshot / encode      | `src/engine/ghostty_vt.rs`  | 9     | theme/default colors, styles, newline, title, enter, paste, mouse SGR, ctrl-c |
| Atlas shaping / raster        | `src/render/atlas.rs`       | 8     | color emoji, CJK fallback, ligatures, constraint classify, fit/scale, cluster map |

**Well covered:** config, profiles, OSC 52, selection/word/URL, atlas
shaping/rasterization, basic engine snapshot + encode.

---

## 4. Gap matrix

| Host area               | Owned by geist                                                                 | Current | Gap                                                                                              | Priority |
| ----------------------- | ----------------------------------------------------------------------------- | ------- | ----------------------------------------------------------------------------------------------- | -------- |
| **Input gating**        | `session.rs::handle_input` (`:200-302`), `map_egui_key`/`key_mods`/`is_text_producing` | none    | Ctrl+Shift & Ctrl+Tab swallow; Shift+PageUp/Home/End scroll-not-send; Ctrl+/-/0 zoom swallow; text-producing suppression; Copy/Cut selection-or-SIGINT | **High** |
| **Coordinate/resize**   | `pos_to_cell` (`:132`), `fit_grid` (`:120`), `to_px`/wheel-notch (`:305-383`)  | none    | cell math + clamping at edges/negatives; cols/rows across ppp+padding+min-1; notch clamp 1..5    | **High** |
| **Split-tree & tabs**   | `app.rs` `Node::{split_leaf,remove_leaf,prune_dead,collect,contains,first_leaf_id}` (`:50-188`), `reap_dead` (`:376`), `split_rect` (`:770`) | none    | split nesting, collapse-on-remove, dead-leaf prune, rect division+gutter, focus fallback, active-tab reselection | **High** |
| **Encode integration**  | `engine/ghostty_vt.rs::{encode_key,encode_mouse}` (`:276-371`)                 | thin (3 cases) | arrows normal vs DECCKM app mode, function keys, Ctrl/Alt/Shift (CSI-u where active), SGR coords at large positions + release | **Medium** |
| **ConPTY throughput**   | `pty.rs` (`Pty::spawn`/reader thread/`is_running`), `session.rs::pump_pty` (`:67`) | none (integration) | read-drain rate of a large known stream end-to-end into the engine                          | **Medium** |
| **Renderer CPU path**   | `render/mod.rs::build_instances` (`:338`), `render/atlas.rs::shape_run` (`:513`) | shaping covered; assembly not benched | per-frame instance assembly cost across grid sizes; shaping throughput by input class | **Medium** |

**Root cause the high-priority gaps are untested today:** `handle_input` reads
from `egui::Context` and writes a live `Pty`; `Node` embeds a real `Session`
(which spawns a shell on construction). There is no seam to drive these without
a window and a child process — hence the testability refactor below.

---

## 5. Recommended roadmap

### 5.1 Testability refactor (prerequisite for §5.2)

Behavior-preserving, guarded by the existing 40 tests plus the new ones.

1. **Make `Node` generic over its leaf payload** — `Node<T>` / `Leaf<T>`, app
   uses `Node<Session>`. Split-tree tests then instantiate `Node<u64>` (or a
   tiny fake), so `split_leaf` / `remove_leaf` / `prune_dead` / `collect` /
   `contains` / `leaf_count` / `first_leaf_id` become pure and unit-testable
   with **no shell spawn**. For `prune_dead` / `reap_dead`, abstract the
   "is alive" check via a closure or small trait so the fake can drive death
   without a PTY.

2. **Extract input gating into a pure decision function.** Refactor
   `handle_input` so the event→outcome mapping is a free function, e.g.
   `decide_key(code, mods, rows, mouse_tracking) -> KeyAction`, where
   ```
   KeyAction = Encode(KeyInput)   // hand to engine.encode_key
             | Scroll(isize) | ScrollTop | ScrollBottom
             | Swallow          // app namespace (Ctrl+Shift, Ctrl+Tab, zoom)
             | Suppress         // text-producing key, defer to egui Text
   ```
   `handle_input` then just executes the action against engine/pty. Every gating
   branch becomes testable without egui or a PTY. Keep `map_egui_key`,
   `key_mods`, and `is_text_producing` as separately tested primitives.

3. **Expose coordinate/sizing math as pure helpers.** `pos_to_cell` and
   `fit_grid` are already nearly pure (they only read `self.cols/self.rows`);
   factor the arithmetic into free functions taking explicit dims so tests assert
   them directly. Same for `to_px` and the wheel-notch calculation.

### 5.2 Conformance tests (all four areas)

- **Input gating** (`src/session.rs`, via `decide_key`): each reserved combo is
  swallowed/redirected as specified; plain printable keys yield `Suppress`
  (deferred to egui `Text`); Ctrl/Alt combos encode; Copy/Cut with a selection
  copies + clears, without a selection emits `0x03`.
- **Coordinate/resize math:** `pos_to_cell` clamps at right/bottom edges and on
  negative positions; `fit_grid` computes cols/rows correctly across ppp +
  padding, never below 1; wheel notch count clamps to `1..=5`.
- **Split-tree & tabs** (`src/app.rs`, via `Node<fake>`): split nesting only
  divides the focused leaf; remove collapses a split into its surviving child;
  `prune_dead` drops dead leaves and collapses; `collect` divides rects with the
  1px gutter (`split_rect`); focus falls back to `first_leaf_id` after closing
  the focused pane; `reap_dead` prunes emptied tabs and reselects the nearest
  surviving active tab.
- **Encode integration** (`src/engine/ghostty_vt.rs`): expand the existing table
  — arrows in normal vs DECCKM (`\x1b[?1h`) app-cursor mode, function keys,
  Ctrl/Alt/Shift modifier encodings (CSI-u when `\x1b[>1u` is active), SGR mouse
  coordinates at large positions and on release. Framed as "geist wires
  libghostty's encoder correctly" (it sets options from the live terminal via
  `set_options_from_terminal`), **not** re-deriving the protocol.

### 5.3 Performance: criterion benches + ConPTY harness

- Add `criterion` as a dev-dependency; `[[bench]]` targets under `benches/`.
- **Synthetic generators** (Ghostty-style, seeded & deterministic): `ascii`,
  `utf8`, `osc` stream producers, shared by the benches. Keep generation
  separate from measurement.
- **CPU-path benches** (host-owned work):
  - **Snapshot copy loop** (`ghostty_vt.rs:400-480`) — geist's `ScreenClone`
    analog; measure across grid sizes (80×24, 200×50, 400×100).
  - **Atlas `shape_run` + rasterization** (`render/atlas.rs:513`) — ascii vs
    utf8 vs emoji vs ligature-heavy input.
  - **Instance assembly** — factor the CPU portion of `build_instances`
    (`render/mod.rs:338`) into a function that fills the `Vec<Instance>` scratch
    **without** the trailing `queue.write_buffer`, then bench that across grid
    sizes. (The GPU upload itself is out of scope — see below.)
- **ConPTY throughput harness** (Windows integration; `tests/` ignored-by-
  default test or a dedicated bench): spawn a shell that emits a large known
  stream, measure the read-drain rate through `Pty::output` → `pump_pty` →
  engine. Document caveats: needs Zig 0.16.0 + a real shell, so it's **not** run
  in plain `cargo test`/CI — gate behind `#[ignore]` or a feature flag.

### 5.4 Explicitly out of scope

- **Input→render latency** and **GPU-submit timing** — need a live window/GPU,
  and Ghostty itself does not measure latency internally (treats it as out of
  scope for the emulator). If pursued later, use an external typometer-style
  approach rather than an in-process bench.
- **IME / dead-key / CJK composition matrix** — upstream tests this manually;
  same applies on Windows. Track as a manual checklist, not unit tests.

---

## 6. Effort / risk & sequencing

| Step                              | Effort | Risk | Notes                                                        |
| --------------------------------- | ------ | ---- | ----------------------------------------------------------- |
| §5.1 testability refactor         | M      | Low–Med | `Node<T>` touches `app.rs` broadly but mechanically; `decide_key` is an extract-method. Guarded by existing 40 tests. |
| §5.2 conformance tests            | M      | Low  | Pure logic once §5.1 lands; highest coverage-per-effort.    |
| §5.3 CPU benches                  | S–M    | Low  | New `benches/`; instance-assembly needs a small CPU/GPU split. |
| §5.3 ConPTY harness               | M      | Med  | Flaky-prone; keep `#[ignore]`, assert throughput floor not exact timing. |

**Suggested order:** §5.1 → §5.2 (the high-value, low-risk core) → §5.3 benches
→ §5.3 ConPTY harness.

---

## 7. Implementation status

All sections landed behaviour-preservingly; the existing 40 tests plus 26 new
ones (66 total) pass under `cargo test`.

### Landed

- **§5.1 Testability refactor** — `Node<T>`/`Tab<T>`/`Leaf<'a, T>` are now generic
  over the leaf payload (`app.rs`); `prune` takes a `dead: &mut impl FnMut(&T) ->
  bool` predicate; `reap_dead`'s structural core was extracted to a pure
  `reap_tabs<T>(..)`. Input gating moved into a pure `decide_key(key, mods, rows)
  -> KeyAction` and `copy_or_interrupt(sel) -> CopyAction` (`session.rs`).
  Coordinate/sizing math extracted to pure `grid_dims`, `cell_from_pos`,
  `wheel_notches`, `px_offset`.
- **§5.2 Conformance tests** — input gating (8 tests), coordinate/resize math
  (4), split-tree & tabs (10, via `Node<u32>`), encode integration (4: arrows
  normal vs DECCKM app mode, modified-arrow CSI params, SGR mouse release at a
  large position). The encode assertions matched libghostty's output as written.
- **§5.3 Perf** — `criterion` dev-dep; `benches/snapshot.rs` (snapshot copy loop,
  ascii/utf8 × 80×24/200×50/400×100) and `benches/shaping.rs` (rustybuzz over the
  embedded font, ascii vs ligature-dense); seeded synthetic `ascii`/`utf8`/`osc`
  generators in `src/synthetic.rs`. ConPTY throughput harness in
  `tests/conpty_throughput.rs` (`#[ignore]`d; spawns real PowerShell).
- **§5.3 Perf (round 2)** — closed the remaining gaps vs Ghostty's suite:
  `benches/stream.rs` (the `TerminalStream`/`OscParser` analog — `engine.write()`
  throughput for ascii/utf8/osc across grid sizes, plus geist's own
  `Osc52Scanner`/`Osc7Scanner` side-scanners); `benches/render.rs` (the deferred
  headless-wgpu bench, see below); `synthetic::corpus` + `geist_BENCH_DATA` real-
  corpus support (`benches/data/`, mirrors Ghostty's `--data`); and
  `scripts/bench-vs-ghostty.ps1` to compare geist vs upstream `ghostty-bench` over
  the same corpus. Full guide in [benchmarking.md](benchmarking.md).
- **Library target** — `src/lib.rs` now exposes the modules so `benches/` and
  `tests/` can drive the host code; `main.rs` is a thin binary over `geist::app`.

### Previously deferred — now landed

- **Instance-assembly + GPU-upload bench.** `Atlas::new` and `build_instances`
  required a live `wgpu::Device`/`Queue`. `render/mod.rs` now exposes a headless
  path — `build_resources(device, format, px, gamma)` (the non-egui half of
  `init`) plus `GpuResources::build_frame_instances`/`reset_atlas_cache` — and
  `benches/render.rs` spins up an offscreen adapter (no surface) to bench warm
  per-frame instance assembly and cold rasterization across grid sizes/classes.
  It **skips gracefully** when no adapter is available, so CI without a GPU is
  unaffected (consistent with §5.4: live GPU-submit/latency timing stays out of
  scope — this measures CPU assembly + atlas upload, not swapchain present).

### Still deferred (with reason)

- **Input→render latency / GPU-submit timing** — per §5.4, needs a live
  window/GPU and an external typometer-style approach; Ghostty doesn't measure it
  internally either.
