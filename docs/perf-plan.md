# Performance plan: WezTerm as the bar

## Progress (2026-09-20)

| phase | state | outcome |
|---|---|---|
| 1a fonts mmap + dedupe | **done** | Rust heap 76.6 -> 3.7 MB; working set 274 -> 201 MB; throughput unchanged (84.4 -> 85.5 MiB/s); 784 tests pass |
| 1b lazy fallback loading | **dropped** | mmap made it moot — see below |
| 1c GPU driver enumeration | **closed — accepted** | ~114 MB of shared driver images; keeping DX12 is worth it (2026-09-20) |
| 2 latency vs WezTerm | **done — giest wins** | 38.2 ms p50 vs WezTerm's 62.3 (20 samples each, identical 1200x800 client) |
| 3 dev build uses OpenConsole | **done** | `mise dev` now runs `fetch-conpty.ps1` (0.3 s no-op once installed) |
| 4 memory series in the harness | **blocked** | needs the perf-harness branch merged; commit signing is down |

Running total: **274 -> 201 MB** working set against WezTerm's 78, with the
remaining 123 MB attributed and reproducible (1c).

### 1b: dropped, and why

The plan assumed parsing six font files at startup cost memory worth reclaiming.
After 1a it does not: the Rust heap is 3.7 MB total, and a mapped file only
makes resident the pages actually read — table directories, not glyph outlines.
Lazy loading would now save a few handles and a millisecond of startup (already
15-55 ms warm, vs WezTerm's ~265 ms). Implementing it would be motion without
measurable benefit, so it is deliberately not done.

### 1c: closed — the overhead is accepted

**Decision (2026-09-20): keep DX12 and live with the idle memory.** giest idles
at ~201 MB against WezTerm's ~78. The measurements below stand; what they buy
is not worth either price on offer, so no more work is planned here.

Why accepting is defensible: the gap is almost entirely *shared, pageable
driver images* (`nvwgf2umx` 86 MB, `amdxc64` 78 MB, `d3d10warp` 6 MB) mapped by
wgpu's adapter enumeration, not memory giest allocates — its own Rust heap is
3.7 MB after 1a. The cost scales with how many GPUs the machine has, and the
pages are reclaimable under pressure.

**What would reopen it:** giest wanting idle memory as a headline number; a
wgpu release exposing an adapter filter (then it is a config change, not a
vendored patch); or a report of real memory pressure on a multi-GPU machine.
The route, if so, is option (1) below — everything needed to act is recorded.

### 1c: the measurements behind that decision

Neither `WGPU_ADAPTER_NAME=NVIDIA` nor `WGPU_POWER_PREF=high` changes anything —
both still map `nvwgf2umx` (86 MB) + `amdxc64` (78 MB) + `d3d10warp` (6 MB) and
idle at 201 MB. Adapter *selection* happens after wgpu-hal has already called
`D3D12CreateDevice` on every DXGI adapter, which is what loads each vendor's
driver. Same build on the GL backend, which touches only the NVIDIA GL driver,
idles at **87 MB** — at WezTerm's 78. So essentially the whole remaining gap is
this, and it is worth roughly 114 MB.

Three ways forward, none of which I took without a call from you:

1. **Vendor + patch `wgpu-hal`** so DX12 enumerates via
   `IDXGIFactory6::EnumAdapterByGpuPreference` and only creates a device for the
   adapter it will use. Contained in one function, but `wgpu-hal` is a large,
   fast-moving crate and CLAUDE.md's vendoring rule ("one delta, re-applied on
   every upgrade") would bind us to it at every wgpu bump. This is the only
   option that keeps DX12 *and* recovers the memory.
2. **Switch the default backend to GL.** Gets 87 MB today, and costs what
   CLAUDE.md already documents: DirectComposition transparency (so
   `background-opacity` regresses to opaque) and exposure to the AMD OpenGL ICD
   that crashed startup with no error at all. Not recommended.
3. **Accept and document it.** These are shared, pageable driver images; the
   cost is real in working set but is not giest's own allocation, and it tracks
   the machine's GPU count. giest stays ~2.5x WezTerm on idle memory.

**Chosen: (3).** Option (1) would bind giest to a vendored copy of a large,
fast-moving crate at every wgpu bump, and (2) trades memory for transparency
and a startup crash. Neither is worth ~114 MB of shared driver pages.

### 2: measured — latency is not a gap

Keystroke to pixel, `scripts/perf-latency.ps1`, 20 samples each, both windows
forced to the same 1200x800 client area, same machine, nothing else running:

| | p50 | p95 | min | mean |
|---|---|---|---|---|
| **giest** | **38.2 ms** | 56.2 | 13.3 | 33.4 |
| WezTerm | 62.3 ms | 64.9 | 37.3 | 59.4 |

giest is ~24 ms faster at p50 and ~2.8x faster at its best case. Both numbers
are inflated by the probe's own poll period (a `PrintWindow` + hash of the
client area sits inside the measured interval), equally for both terminals —
which is why the windows must be the same size: at their own defaults WezTerm's
window is 1.6x the area, worth ~8 ms of its first, discarded result.

**So there is no second big item.** giest leads on throughput and latency; the
only dimension where WezTerm is ahead is idle memory (1c).

Four traps cost real time here and are all now guarded or documented in the
script — each produced a *plausible* wrong answer rather than an error:

- `Process.MainWindowHandle` latches onto winit's 22x22 helper window, which
  exists before the real one. Every capture then returned 1936 bytes (22*22*4)
  of nothing, which read exactly like "PrintWindow cannot see a GPU surface".
- A giest launched while another is reachable on the default IPC pipe forwards
  its request and exits in ~10 ms; the probe now gives each run its own pipe.
- `-TermArgs a,b` collapses to one comma-joined string across `pwsh -File`, so
  the terminal launches with one bogus argument and exits instantly.
- Launched from a child `pwsh -File`, every terminal died within ~10 ms; run
  the script in your own shell with `& ./scripts/perf-latency.ps1 ...`.

### 2 (superseded): why there was no number at first

`scripts/perf-latency.ps1` is written and preflight-guarded, but it cannot be
run from an automation shell: `SendInput` delivers nothing there (verified
against Notepad — its title never gained the unsaved-changes marker) and both
`PrintWindow` and a screen-DC `BitBlt` return an all-but-empty image that never
changes while the shell prints. Neither is a terminal defect; that context has
no interactive desktop. The script now throws with that explanation instead of
reporting a plausible-looking zero.

**To get the number, run from a terminal you opened yourself:**

```powershell
pwsh scripts/perf-latency.ps1 -Exe target/release/giest.exe -Args '--window-width=120','--window-height=40' -Label giest
pwsh scripts/perf-latency.ps1 -Exe <wezterm-gui.exe> -Args '--config','initial_cols=120','--config','initial_rows=40' -Label wezterm
```

Don't touch the keyboard while it runs — it types into the focused window.


WezTerm is the reference because it is the closest peer: a GPU terminal, an
external ConPTY client (unlike Windows Terminal, which hosts the console layer
in-process and skips the pipe hop), and it runs on this machine. Every number
below was measured on 2026-09-19/20 on the same box (144 Hz, NVIDIA + AMD
GPUs), same 120x40 grid, release builds; the methods are in
`docs/benchmarking.md`. **Nothing in this plan is a guess about where time or
memory goes — each item names the measurement that found it and the one that
will show it fixed.**

## Where giest stands today

| dimension | giest | WezTerm | verdict |
|---|---|---|---|
| write throughput, 17 MiB flood | 84.4 MiB/s | 16.5 | won (5x) — with the bundled OpenConsole |
| same, on the inbox conhost | 12.9 | — | the console host is a 6x factor by itself |
| vtebench streams (8) | ahead on 7, level on 1 | | won |
| idle working set | 274 MB | 78 MB | **3.5x behind** |
| idle private bytes | 433 MB | 171 MB | 2.5x behind |
| startup to window | ~280 ms (cold) | ~270–500 ms | level |
| idle CPU | ~0% | ~0% | level |
| frame CPU under flood (p95) | 0.6 ms at 144 Hz | not measurable externally | rendering is not the bound |
| keystroke → paint (p50) | 19 ms, of which ~16 is ConPTY | **not yet measured for WezTerm** | unknown |

So the plan has one large target (memory), one unknown (latency vs WezTerm),
and a set of guards so the throughput lead is not lost.

## 1. Memory — 274 MB → target ≤ 120 MB working set

Attributed with a module breakdown, a per-backend probe and a counting
allocator (`GIEST_HEAP_PROBE`, `main.rs`):

| component | size | evidence |
|---|---|---|
| fallback fonts read whole into the heap at startup | **~73 MB** | Rust heap live 76.6 MB idle; the six `FALLBACK_FONTS` files total 60.5 MB and `seguiemj.ttf` (12.3 MB) is read a second time as `COLOR_FONT` |
| second GPU vendor's DX12 driver + WARP, loaded by wgpu's adapter enumeration | ~60 MB WS / ~180 MB private | `amdxc64.dll` 78 MB + `d3d10warp.dll` mapped under DX12; the GL backend (NVIDIA only, same DLL set as WezTerm) idles at 214 MB WS |
| NVIDIA DX12 driver itself | ~85 MB mapped (shared) | `nvwgf2umx.dll`; WezTerm pays the GL equivalent (`nvoglv64.dll`, 47 MB) |
| everything else giest allocates | ~4 MB | remainder of the Rust heap |

### 1a. Memory-map the fonts instead of reading them (S, ~70 MB)

`ttf_parser`, `ab_glyph` and `rustybuzz` all work on `&[u8]`; today that
slice comes from `std::fs::read` + `Box::leak`. Map the files (`memmap2`) and
hand out the mapped slice: pages of a 19 MB CJK collection that no glyph ever
touches never enter the working set. Also load `seguiemj.ttf` once and share it
between the outline fallback and the color font. Expected: Rust heap 77 → ~4 MB,
working set down by roughly the same. Gate: `GIEST_HEAP_PROBE` reading and the
idle-WS number in `scripts/perf-vs.ps1`'s startup probe (to be added, see 4).

### 1b. Lazy fallback loading (S, on top of 1a)

Even mapped, six files are opened and their tables parsed at startup. Load a
fallback face on the first codepoint the earlier faces miss. Most sessions never
render CJK or Korean. Keeps startup flat and cuts handle count.

### 1c. Stop initializing every GPU adapter (M) — CLOSED, accepted

> Superseded by the decision at the top of this file: the overhead is accepted
> and no work is planned. Kept because it records the options and what each
> costs, for whoever reopens it. The "~60 MB" estimate below was measured
> before 1a landed; the true figure is ~114 MB.


wgpu-hal's DX12 backend opens a D3D12 device on *every* DXGI adapter while
enumerating, which loads each vendor's user-mode driver and WARP. giest already
pins the backend to DX12 for a crash reason (CLAUDE.md); the remaining cost is
enumeration. Options, in order of preference:

1. `Instance::request_adapter` with a `compatible_surface` and
   `PowerPreference::HighPerformance` still enumerates — verify with the module
   list whether it *keeps* the other drivers mapped once the device is chosen, or
   whether they are only touched during enumeration and can be unloaded.
2. Filter DXGI adapters by the LUID of the monitor the window is on before
   exposing them — a small patch to the vendored-if-needed `wgpu-hal`
   (the repo already carries three vendored crates with one-line patches; this
   would be the fourth, same rule: one delta, re-applied on upgrade).
3. Do nothing and document it: the driver DLLs are shared, pageable images; the
   working-set cost is real but the private cost is mostly driver-side scratch.

Gate: `d3d10warp.dll` and the non-hosting vendor's DLL absent from the module
list; idle WS in the startup probe.

**Not doing:** switching giest to the GL backend to match WezTerm's driver set.
GL measured only 60 MB better, loses DirectComposition transparency (see
CLAUDE.md), and the OpenGL ICD is the one that faulted on the AMD card.

## 2. Input latency — measure WezTerm, then decide

giest's harness reports 19 ms keystroke-to-paint at p50, with ~16 ms of that
inside ConPTY before the byte reaches the engine. Whether WezTerm is faster is
**unknown**: its number can only be taken externally. Plan:

- Build `scripts/perf-latency.ps1`: inject a key with `SendInput`, poll the
  window with `PrintWindow` (client-only, DPI-aware — the two traps in
  CLAUDE.md) until the prompt-line pixels change, 30 samples each, run against
  both terminals on the same OpenConsole. The camera-free version of typometer.
- If WezTerm is within noise of giest, close this item: the floor is ConPTY.
- If WezTerm is meaningfully faster, the suspects are, in order: giest's PTY
  wake path (the reader posts an event, the UI thread then pumps, snapshots and
  renders — measure the gap between `pump_ms` and `frame_cpu_ms` in the
  harness); the 500 ms exit-poll heartbeat coalescing with a real wake; and
  present mode (`window-vsync` is `AutoVsync`; a triple-buffered swapchain adds
  up to one refresh).

Target: within 1 ms of WezTerm at p50, or a written reason it cannot be.

## 3. Keep the throughput lead

- **Ship OpenConsole by default.** `mise package` already bundles `conpty.dll`
  + `OpenConsole.exe`; `mise dev` should run `scripts/fetch-conpty.ps1` so
  developers measure the shipped configuration, not the inbox one (6x slower).
- The reader loop (64 KiB reads, wake coalescing) measured no change and is
  documented as such in `pty.rs`; don't spend more there.
- Under flood 196 of 234 snapshots are full re-copies because every frame
  scrolls. The dirty-row path pays off for typing, not floods. The remaining
  per-frame cost (0.6 ms) is already below one refresh at 144 Hz; only revisit
  if a 240 Hz display or a 4K grid shows `frame_cpu_ms` p95 above ~4 ms.

## 4. Make regressions visible automatically

- `mise perf` (in-app harness) gates frame CPU, echo latency and flood drain
  against `perf/baseline.json`. Add **idle working set** and **Rust heap live**
  as gated series (the counting allocator already exists; expose it through
  `perf_stats`).
- `scripts/perf-vs.ps1` records the cross-terminal numbers; add the startup /
  idle-RSS / idle-CPU probe used for this plan as a scenario, so the WezTerm
  comparison is one command.
- Run both before merging anything that touches `render/`, `pty.rs`, the
  engine snapshot, or font loading.

## Order

All done or closed except one:

1. ~~**1a** fonts mmap + dedupe~~ done — the largest win, and the whole of it.
2. ~~**2** latency probe~~ done — giest leads, so there was no second big item.
3. **4** memory series in the harness, so 1a stays fixed. **Still open**, and
   blocked on merging the perf-harness branch (`src/perf.rs`, `mise perf`).
4. ~~**1b**~~ dropped (mmap made it moot), ~~**1c**~~ closed (accepted).
5. ~~**3** dev-build OpenConsole fetch~~ done.

With 1c accepted, giest's standing against WezTerm is: **ahead** on throughput
(5x) and keystroke latency (1.6x), **level** on startup and idle CPU, **behind**
on idle memory by ~2.5x, by choice and for a known reason.

## Decided against

- Matching WezTerm's GL backend (above).
- Reducing the atlases: 2048x2048 R8 + RGBA is 20 MB of *GPU* memory and
  not in the working-set numbers; halving it would raise cache flushes.
- A counting allocator in release builds *permanently*: it is two relaxed
  atomics per allocation; keep it until the harness `frame_cpu_ms` shows any
  cost, then gate it behind a feature.
