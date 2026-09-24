# Building & testing

## Prerequisites

- **Rust** — stable, edition 2024 (1.95+).
- **Zig 0.16.0** on `PATH`. The vendored `libghostty-vt-sys` build script
  compiles Ghostty's VT library with Zig. The pinned Ghostty commit declares
  `minimum_zig_version = 0.16.0`, so **0.15.x will not build it**. Get it from
  <https://ziglang.org/download/0.16.0/>.
- **MSVC toolchain** — the default `x86_64-pc-windows-msvc` target.
- **Internet on the first build** — the build script fetches the pinned Ghostty
  source.

## Recommended: mise

The repo ships a `mise.toml` that pins `zig = "0.16.0"` and defines tasks that
run cargo from the project root with the right Zig on `PATH`.

```powershell
mise trust      # once
mise dev        # debug build + run
mise release    # optimized release build
```

## Cargo directly

```powershell
# Put Zig 0.16.0 first on PATH:
$env:PATH = "C:\path\to\zig-0.16.0;$env:PATH"

cargo run             # debug
cargo run --release   # release
```

Fallbacks: `mise exec zig@0.16.0 -- cargo build`, or plain
`cargo build`/`cargo run --release` if Zig 0.16.0 is already on `PATH`.

!!! warning "Always run cargo from the project root"
    Running cargo from inside `vendor/libghostty-rs/...` builds the **vendored
    crate** instead of geist (cargo walks up to the nearest `Cargo.toml`). A
    "Finished" that only mentions `libghostty-vt` compiling means you're in the
    wrong directory.

The first build takes a few minutes (it fetches and compiles Ghostty's VT
library via Zig); later builds skip Zig unless the native crate changes.

## What the build does

```mermaid
flowchart TB
    cargo["cargo build (project root)"]
    sys["libghostty-vt-sys build.rs"]
    zig["zig build (0.16.0)"]
    fetch["fetch pinned Ghostty source"]
    archive["ghostty-vt-static.lib"]
    link["link geist.exe (static archive on Windows)"]

    cargo --> sys --> zig --> fetch --> archive --> link --> cargo

    classDef v fill:#1a2b1f,stroke:#5cba7c,color:#e6ffe6
    class sys,zig,fetch,archive v
```

See the [libghostty touch point](../touchpoints/libghostty.md) for why the
Windows link targets the static archive rather than the DLL import lib.

## Tests

```powershell
cargo test          # ~31 unit tests
cargo test <name>   # single test by name substring
```

Coverage spans the engine, input encoding, selection, paste, mouse encoding,
theming, config parsing, ligatures, OSC 52 parsing, and URL detection — the
pure-logic seams that don't need a window or a GPU.

## Benchmarks

```powershell
cargo bench                  # all criterion benches (needs Zig 0.16.0 + release)
cargo bench --bench stream   # one bench (stream / snapshot / shaping / render)
```

The suite mirrors Ghostty's own benchmarks — VT-write/OSC throughput (`stream`),
snapshot copy-out (`snapshot`), text shaping (`shaping`), and headless renderer
cost (`render`, skipped when no GPU adapter is present). It can be compared
against upstream `ghostty-bench` over the same corpus via
`scripts/bench-vs-ghostty.ps1`. See **[Benchmarking](../benchmarking.md)** for the
full guide, the `geist_BENCH_DATA` corpus mechanism, and methodology caveats.

## Optimized release builds

`cargo build --release` (and `mise release`) use a max-optimization profile in
`Cargo.toml`: `lto = "fat"`, `codegen-units = 1`, `opt-level = 3`,
`panic = "abort"`, and `strip`. This trades longer compile times (a few minutes,
mostly LTO over wgpu/eframe) for the fastest runtime. The vendored libghostty-vt
Zig library is built `ReleaseFast` automatically for release.

!!! warning "Don't set `debug = true` in `[profile.release]`"
    Cargo exposes the profile's `debug` flag to build scripts as `DEBUG`, and
    `libghostty-vt-sys/build.rs` reads it: `DEBUG=true` silently downgrades the
    Zig VT library to a slow `Debug` build. Profile on a separate profile instead.

## Building these docs

```bash
pip install -r docs/requirements.txt
mkdocs serve     # live preview at http://127.0.0.1:8000
mkdocs build     # static site → ./site
```

The site uses Material for MkDocs; diagrams are [Mermaid](https://mermaid.js.org/)
fenced blocks rendered client-side via the theme's built-in support.

It deploys to GitHub Pages from `.github/workflows/docs.yml` on every push to
`main` that touches `docs/`, `mkdocs.yml`, or the workflow. The workflow builds
with `mkdocs build --strict` and publishes the `site/` artifact via
`actions/deploy-pages`. It also forces the repo's Pages **source** to "GitHub
Actions" (`configure-pages` for a fresh site, plus a `gh api PUT … build_type=workflow`
to switch an existing branch-based site) — required so the built site is served
instead of a Jekyll-rendered `README.md`.
