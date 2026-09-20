# Benchmarking giest

giest reuses Ghostty's VT engine (libghostty-vt) but owns its whole host
environment (renderer, ConPTY I/O, input/layout). Its benchmarks deliberately
mirror [Ghostty's own suite](https://github.com/ghostty-org/ghostty) (`src/benchmark/`)
so engine numbers are comparable and the renderer's giest-only cost is covered too.
For the broader test/conformance picture see
[testing-host-env-gap-analysis.md](testing-host-env-gap-analysis.md).

## Inventory

| giest bench | What it measures | Mirrors (Ghostty) |
| --- | --- | --- |
| `stream` → `stream/*` | `engine.write()` throughput: VT parse **+** state apply, per grid size & input class (ascii/utf8/osc). The core hot path. | `TerminalStream`, `OscParser` |
| `stream` → `osc_scan/*` | giest's own OSC side-scanners (`OscColorScanner` etc.) (run on every PTY chunk). | — (giest-only) |
| `snapshot` | Per-frame `GridSnapshot` copy-out from the engine. | `ScreenClone` |
| `shaping` | rustybuzz shaping over the embedded font (ascii vs ligature-dense). | — (renderer) |
| `render` → `render_instances/*` | Per-frame instance assembly with a **warm** glyph atlas. | `ScreenClone --mode render` |
| `render` → `render_raster_cold/*` | Instance assembly with the glyph cache reset each iter — rasterization dominates. | — (renderer) |
| `tests/conpty_throughput.rs` (`#[ignore]`) | End-to-end ConPTY read-drain rate of a real shell into the engine. | — (host) |

Unicode metrics (`codepoint-width`, `grapheme-break`, `is-symbol`) and the bare
parser (`terminal-parser`) live **inside** libghostty-vt and are benched upstream
in Zig — giest doesn't re-bench them (the `stream` bench exercises them in
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
2. **Real corpus** — set `GIEST_BENCH_DATA` to a captured VT stream and the
   `stream` bench uses it verbatim (case labeled `corpus`), exactly like Ghostty's
   `--data <file>`:

   ```powershell
   $env:GIEST_BENCH_DATA = "benches\data\ansi-sample.vt"
   mise exec zig@0.16.0 -- cargo bench --bench stream
   Remove-Item Env:\GIEST_BENCH_DATA
   ```

   A small sample is checked in at `benches/data/` — capture larger streams
   yourself (see `benches/data/README.md`); keep big captures out of git.

## Comparing across giest branches

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
`ghostty-bench terminal-stream --data <corpus>`, then runs giest's `stream` bench
over the same corpus, and prints both:

```powershell
./scripts/bench-vs-ghostty.ps1 -Corpus C:\caps\big-session.vt -Cols 80 -Rows 24
./scripts/bench-vs-ghostty.ps1 -SkipGhostty            # giest side only (fast)
```

It auto-detects the fetched ghostty source under `target/` (or clones the pinned
commit), and degrades gracefully if Zig / the source / a corpus is missing.

Caveats — read the ratio, not the absolute figure:

- **Different harnesses.** Ghostty's bench is single-pass wall clock (incl. process
  startup); giest's is criterion steady-state. Use a **large** corpus (tens of MiB+)
  so processing dominates startup. Install `hyperfine` for better Ghostty-side timing.
- **Native lib optimization.** `cargo bench` builds the giest crate in release, but
  libghostty-vt's Zig optimization is chosen by the vt-sys `build.rs` from cargo's
  profile (`ReleaseFast` for release, `Debug` for the dev profile). For the fairest
  engine comparison, ensure the vt lib was built optimized (a release build) — a
  debug vt lib will read far slower than Ghostty's ReleaseFast bench.

## Cross-terminal comparison (`scripts/perf-vs.ps1`)

Runs the **same byte streams** inside giest, Windows Terminal and WezTerm and
records write-side throughput (how fast the terminal drains its PTY, as the
program sees it — the quantity `ghostty-bench` and vtebench time). Workloads:
a 17 MiB SGR/UTF-8 flood plus vtebench's suites, pre-generated to streams with
Git's bash (vtebench's own runner needs `/bin/sh`, so it records nothing on
Windows) and replayed by `scripts/perf-flood.ps1`.

```powershell
pwsh scripts/perf-vs.ps1 -WezTerm <path\to\wezterm-gui.exe>          # all three
pwsh scripts/perf-vs.ps1 -Only giest                                  # just giest
pwsh scripts/perf-vs.ps1 -Only giest-inbox -GiestExtraArgs '--conpty-passthrough=false' -GiestLabelSuffix '-inbox'
```

**Fairness rules the script enforces, learned the hard way:**

- **Same grid.** ConPTY's cost scales with the cell count, so every terminal is
  pinned to 120x40 (`--window-width/-height`, `wt --size`, WezTerm
  `initial_cols/rows`). Unpinned, each opens at its own default and you measure
  window size.
- **Same console host.** WezTerm bundles a newer OpenConsole; giest uses one only
  when `conpty.dll` + `OpenConsole.exe` sit beside the exe (`scripts/fetch-conpty.ps1`;
  `mise package` bundles them). This is the largest single factor — see below.
- Windows Terminal hosts the console layer in-process and never pays the pipe hop
  the others do; treat it as the ceiling, not a peer.
- Launch from PowerShell with the call operator or a bare `pwsh`, not
  `Start-Process -ArgumentList` with a full path: `-ArgumentList` joins its
  elements with spaces *without quoting*, so `C:\Program Files\...\pwsh.exe`
  arrives as two arguments. (This masqueraded as a `giest -e` bug for an hour;
  `-e` handles quoted paths with spaces fine.)

**Reference numbers** (2026-09-19, release build, 144 Hz, median of 3, MiB/s):

| workload | giest (OpenConsole) | giest (inbox conhost) | Windows Terminal | WezTerm |
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

Read: on the same host and grid giest is at Windows Terminal's level and ahead
of WezTerm; the inbox conhost costs 6x on byte-heavy work. The reader loop's
buffer size and wake rate were tested and are **not** a factor (see `pty.rs`).
Ghostty itself cannot be compared here: its app doesn't build on Windows and
giest already runs Ghostty's engine — the `stream` bench is that engine plus FFI.
