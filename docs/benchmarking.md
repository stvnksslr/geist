# Benchmarking geist

geist reuses Ghostty's VT engine (libghostty-vt) but owns its whole host
environment (renderer, ConPTY I/O, input/layout). Its benchmarks deliberately
mirror [Ghostty's own suite](https://github.com/ghostty-org/ghostty) (`src/benchmark/`)
so engine numbers are comparable and the renderer's geist-only cost is covered too.
For the broader test/conformance picture see
[testing-host-env-gap-analysis.md](testing-host-env-gap-analysis.md).

## Inventory

| geist bench | What it measures | Mirrors (Ghostty) |
| --- | --- | --- |
| `stream` → `stream/*` | `engine.write()` throughput: VT parse **+** state apply, per grid size & input class (ascii/utf8/osc/tui). The core hot path. | `TerminalStream`, `OscParser` |
| `stream` → `osc_scan/*` | geist's own side-scanners, which run over every PTY chunk: `osc_color_scan` alone on OSC-only input, and `side_scanners/{tui,ascii}` — all five together, as `pump_pty` runs them. | — (geist-only) |
| `snapshot` → `snapshot/*` | Per-frame `GridSnapshot` copy-out with one dirty row (typing). | `ScreenClone` |
| `snapshot` → `snapshot/full/*` | The same with every row re-copied: a scroll, a flood, a fullscreen redraw. | `ScreenClone` |
| `shaping` | rustybuzz shaping over the embedded font (ascii vs ligature-dense). | — (renderer) |
| `render` → `render_instances/*` | Per-frame instance assembly with a **warm** glyph atlas. | `ScreenClone --mode render` |
| `render` → `render_raster_cold/*` | Instance assembly with the glyph cache reset each iter — rasterization dominates. | — (renderer) |
| `tests/conpty_throughput.rs` (`#[ignore]`) | End-to-end ConPTY read-drain rate of a real shell into the engine. | — (host) |

Unicode metrics (`codepoint-width`, `grapheme-break`, `is-symbol`) and the bare
parser (`terminal-parser`) live **inside** libghostty-vt and are benched upstream
in Zig — geist doesn't re-bench them (the `stream` bench exercises them in
aggregate).

## Running

Benches need **Zig 0.16.0** on PATH (to build libghostty-vt) and build in release.
Prefer `mise`:

```powershell
mise exec zig@0.16.0 -- cargo bench                      # all benches
mise exec zig@0.16.0 -- cargo bench --bench stream       # one bench
mise exec zig@0.16.0 -- cargo bench --bench stream -- ascii/80x24   # filter by id
```

- The `render` bench needs a **wgpu adapter** (real or software GPU). With none
  available (headless CI), it prints a notice and skips rather than failing — the
  CPU shaping path is still covered by `--bench shaping`.
- Quick smoke run (less rigorous, faster): append
  `-- --warm-up-time 0.5 --measurement-time 1 --sample-size 10`.

## Input data

Two sources, mirroring Ghostty's separate generate→measure approach:

1. **Seeded synthetic** (default) — deterministic `ascii`/`utf8`/`osc` generators
   in `src/synthetic.rs` (seeded LCG; byte-for-byte reproducible across runs and
   branches). Generation happens once during setup, never inside the timed loop.
2. **Real corpus** — set `geist_BENCH_DATA` to a captured VT stream and the
   `stream` bench uses it verbatim (case labeled `corpus`), exactly like Ghostty's
   `--data <file>`:

   ```powershell
   $env:geist_BENCH_DATA = "benches\data\ansi-sample.vt"
   mise exec zig@0.16.0 -- cargo bench --bench stream
   Remove-Item Env:\geist_BENCH_DATA
   ```

   A small sample is checked in at `benches/data/` — capture larger streams
   yourself (see `benches/data/README.md`); keep big captures out of git.

## Comparing across geist branches

criterion has built-in baselines (Ghostty uses `hyperfine` for the same goal):

```powershell
git switch main;        mise exec zig@0.16.0 -- cargo bench --bench stream -- --save-baseline main
git switch my-branch;   mise exec zig@0.16.0 -- cargo bench --bench stream -- --baseline main
```

The second run prints the percentage change per case. Keep grid dimensions and the
corpus identical between the two, and don't run other heavy work concurrently.

## Comparing against Ghostty (apples-to-apples)

`scripts/bench-vs-ghostty.ps1` builds Ghostty's own `ghostty-bench` from the **same
pinned commit** the vt lib is built from (`-Demit-bench`, always ReleaseFast), runs
`ghostty-bench terminal-stream --data <corpus>`, then runs geist's `stream` bench
over the same corpus, and prints both:

```powershell
./scripts/bench-vs-ghostty.ps1 -Corpus C:\caps\big-session.vt -Cols 80 -Rows 24
./scripts/bench-vs-ghostty.ps1 -SkipGhostty            # geist side only (fast)
```

It auto-detects the fetched ghostty source under `target/` (or clones the pinned
commit), and degrades gracefully if Zig / the source / a corpus is missing.

Caveats — read the ratio, not the absolute figure:

- **Different harnesses.** Ghostty's bench is single-pass wall clock (incl. process
  startup); geist's is criterion steady-state. Use a **large** corpus (tens of MiB+)
  so processing dominates startup. Install `hyperfine` for better Ghostty-side timing.
- **Native lib optimization.** `cargo bench` builds the geist crate in release, but
  libghostty-vt's Zig optimization is chosen by the vt-sys `build.rs` from cargo's
  profile (`ReleaseFast` for release, `Debug` for the dev profile). For the fairest
  engine comparison, ensure the vt lib was built optimized (a release build) — a
  debug vt lib will read far slower than Ghostty's ReleaseFast bench.

## Cross-terminal comparison (`scripts/perf-vs.ps1`)

Runs the **same byte streams** inside geist, Windows Terminal and WezTerm and
records write-side throughput (how fast the terminal drains its PTY, as the
program sees it — the quantity `ghostty-bench` and vtebench time). Workloads:
a 17 MiB SGR/UTF-8 flood plus vtebench's suites, pre-generated to streams with
Git's bash (vtebench's own runner needs `/bin/sh`, so it records nothing on
Windows) and replayed by `scripts/perf-flood.ps1`.

```powershell
pwsh scripts/perf-vs.ps1 -WezTerm <path\to\wezterm-gui.exe>          # all three
pwsh scripts/perf-vs.ps1 -Only geist                                  # just geist
pwsh scripts/perf-vs.ps1 -Only geist-inbox -geistExtraArgs '--conpty-passthrough=false' -geistLabelSuffix '-inbox'
```

**Fairness rules the script enforces, learned the hard way:**

- **Same grid.** ConPTY's cost scales with the cell count, so every terminal is
  pinned to 120x40 (`--window-width/-height`, `wt --size`, WezTerm
  `initial_cols/rows`). Unpinned, each opens at its own default and you measure
  window size.
- **Same console host.** WezTerm bundles a newer OpenConsole; geist uses one only
  when `conpty.dll` + `OpenConsole.exe` sit beside the exe (`scripts/fetch-conpty.ps1`;
  `mise package` bundles them). This is the largest single factor — see below.
- Windows Terminal hosts the console layer in-process and never pays the pipe hop
  the others do; treat it as the ceiling, not a peer.
- Launch from PowerShell with the call operator or a bare `pwsh`, not
  `Start-Process -ArgumentList` with a full path: `-ArgumentList` joins its
  elements with spaces *without quoting*, so `C:\Program Files\...\pwsh.exe`
  arrives as two arguments. (This masqueraded as a `geist -e` bug for an hour;
  `-e` handles quoted paths with spaces fine.)

**Reference numbers** (2026-09-19, release build, 144 Hz, median of 3, MiB/s):

| workload | geist (OpenConsole) | geist (inbox conhost) | Windows Terminal | WezTerm |
|---|---|---|---|---|
| 17 MiB flood | 84.4 | 12.9 | 87.5 | 16.5 |
| cursor_motion | 70.0 | 26.5 | 65.3 | 32.9 |
| dense_cells | 100.7 | 50.2 | 98.6 | 73.3 |
| light_cells | 307 | 339 | 328 | 245 |
| medium_cells | 84.1 | 9.6 | 81.6 | 7.5 |
| sync_medium_cells | 84.2 | 10.0 | 57.6 | 7.2 |
| scrolling | 18.3 | 0.7 | 12.0 | 0.6 |
| scrolling_fullscreen | 25.0 | 0.9 | 16.9 | 0.8 |
| unicode | 103 | 25.1 | 110 | 7.6 |

Read: on the same host and grid geist is at Windows Terminal's level and ahead
of WezTerm; the inbox conhost costs 6x on byte-heavy work. The reader loop's
buffer size and wake rate were tested and are **not** a factor (see `pty.rs`).
Ghostty itself cannot be compared here: its app doesn't build on Windows and
geist already runs Ghostty's engine — the `stream` bench is that engine plus FFI.

## Sampling profiler (`tools/sampler`, `scripts/perf-profile.ps1`)

The criterion benches time geist's pieces one at a time; this answers "where
does the *running app's* time go, end to end". `tools/sampler` is an
out-of-process sampling profiler that needs no admin rights (WPR and samply
need an elevated shell for ETW): it suspends the target's threads, walks their
stacks with dbghelp, and expands inlined frames, which matters under fat LTO.
`scripts/perf-profile.ps1` launches an isolated geist (its own config and IPC
pipe), runs a workload from `scripts/perf-workload.py` inside it, and points
the sampler at both geist and its OpenConsole.

```powershell
$env:LIBGHOSTTY_VT_SYS_OPTIMIZE = "ReleaseFast"   # trap 1 below
cargo build --profile profiling                    # release codegen + PDB
pwsh scripts/fetch-conpty.ps1 -Profiles profiling
cargo build --release --manifest-path tools/sampler/Cargo.toml

pwsh scripts/perf-profile.ps1 -Mode tui -NoSample          # throughput only
pwsh scripts/perf-profile.ps1 -Mode tui -CpuOnly           # + per-thread CPU, no suspension
pwsh scripts/perf-profile.ps1 -Mode flood -SamplerArgs --only,main,--interval-ms,10,--folded,target/perf-profile/flood.folded
python tools/sampler/incl.py target/perf-profile/flood.folded   # inclusive % per geist frame
```

Workloads: `tui` is a fullscreen app redrawing every row each frame inside a
synchronized update (Claude Code, htop, lazygit); `flood` is colored log lines
scrolling as fast as the terminal accepts them. Both report what the *program*
sees (MiB/s, frames/s); a `.folded` file is flamegraph input.

**Three traps, each of which produced a confident wrong answer:**

1. **A profiling build has a Debug engine unless told otherwise.** The vendored
   `libghostty-vt-sys/build.rs` picks Zig's optimize mode from cargo's `DEBUG`,
   which is true whenever debug info is on, not only in unoptimized builds. The
   first profiles were dominated by Zig safety checks (`verifyIntegrity`,
   `assert`). `LIBGHOSTTY_VT_SYS_OPTIMIZE=ReleaseFast` fixes it.
2. **Sampling perturbs the pipeline it measures.** The UI thread *is* the PTY
   drain, so suspending it about 1,000 times a second stalls ConPTY and the
   program behind it. Sampling every thread cut a TUI from about 7,600 to about
   400 frames/s, while making OpenConsole look 99% blocked in `WriteFile`.
   Throughput comes only from `-NoSample` / `-CpuOnly` runs; stacks from
   `--only main` at 100 Hz or less, which measured as harmless.
3. **This is a shared desktop.** A CPU-heavy job running alongside produces a
   bimodal "slow mode" (about 600 against about 9,000 frames/s) in which *no*
   thread is busy. Interleave A/B runs, and compare **UI-thread CPU per MiB**
   from `-CpuOnly`, which contention barely moves.

### What it found (2026-10-21)

Before, the UI thread was **saturated (91–100% of a core)** in every workload,
so geist, not ConPTY, set the ceiling. The largest item was geist's own: the
five side scanners each stepped a state machine through every byte. Per MiB
of TUI output (`stream/tui`, `osc_scan/side_scanners/tui`) the engine's whole
parse cost **4.6 ms** and the scanners **5.4 ms**, about 31% of the UI thread
in a whole-app profile. Smaller items, as shares of the UI thread under TUI
load:

| | share of UI thread |
| --- | --- |
| snapshot `copy_cell` (one heap allocation per cell, via `graphemes()`) | ~3% of its ~7% |
| `build_instances` (three `powf` per colour) | part of ~6% |
| counting global allocator's atomics (feeding an unread probe) | ~2.5% (flood) |

Fixed: the scanners skip to the only sequences they act on (`osc_scan`:
**184 MiB/s → 1.1 GiB/s** on TUI output, **513 MiB/s → 12 GiB/s** on plain
text; `tests/scanner_equivalence.rs` holds each skip to its byte-by-byte walk);
`copy_cell` reads graphemes into a stack buffer; sRGB→linear is a table
(`render_instances` **−15 to −31%**, every size and class); the allocator counts
only while `geist_HEAP_PROBE` is set. (The PTY queue also became bounded in
bytes rather than reads; on its own that measured as no throughput change.)

End to end, release builds, medians of 3 interleaved `-CpuOnly` rounds:

| grid | workload | throughput (MiB/s) | UI-thread CPU per MiB | UI thread busy |
| --- | --- | --- | --- | --- |
| 120x40 | flood | 135 → **180** (+34%) | 7.5 → **4.5 ms** (−40%) | 100% → 80% |
| 120x40 | tui | 94 → **112** (+19%, best of 3) | 13.0 → **7.1 ms** (−45%) | 91% → 78% |
| 200x60 | flood | 119 → **186** (+56%) | 8.3 → **4.0 ms** (−52%) | 98% → 74% |
| 200x60 | tui | 82 → **112** (+36%) | 12.2 → **7.9 ms** (−35%) | 100% → 89% |

The UI thread now has headroom in every case. What remains of its time is
mostly the engine's own parse (upstream), and taking the parse off the UI
thread would need the engine to become `Send`.
