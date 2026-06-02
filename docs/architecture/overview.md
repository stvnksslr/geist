# Architecture overview

giest is layered so that the **renderer and input layer never touch
libghostty-vt directly** — they only ever talk to the backend-neutral
[`TerminalEngine`](../components/engine.md) trait. This leaves room for a
pure-Rust fallback engine in the future without changing the rest of the app.

## The big picture

```mermaid
flowchart TB
    subgraph win["Windows"]
        conpty["ConPTY pseudo-console"]
        shell["Shell process<br/>(pwsh / cmd / wsl)"]
        clip["System clipboard"]
        gpu["GPU (wgpu / DX12)"]
        winsys["Window · keyboard · mouse"]
    end

    subgraph app["giest process"]
        direction TB
        main["main.rs<br/>eframe bootstrap"]
        appmod["app.rs<br/>tabs · split-tree panes · per-frame loop"]
        session["session.rs<br/>PTY + engine + selection state"]
        pty["pty.rs<br/>portable-pty + reader thread"]
        engine["engine/mod.rs<br/>TerminalEngine trait"]
        ghostty["engine/ghostty_vt.rs<br/>libghostty-vt backend"]
        render["render/*.rs<br/>wgpu glyph-atlas pipeline"]
        osc52["osc52.rs<br/>clipboard side-parser"]
    end

    vt["libghostty-vt<br/>(native, built with Zig)"]

    winsys -->|egui events| appmod
    appmod --> session
    session --> pty
    pty <-->|bytes| conpty
    conpty <--> shell
    session --> engine
    engine --> ghostty
    ghostty <-->|FFI| vt
    session --> osc52
    osc52 --> clip
    appmod --> render
    render -->|instanced quads| gpu
    appmod -->|copy/paste| clip

    classDef osNode fill:#1f2933,stroke:#7c5cff,color:#e6e6e6
    classDef vtNode fill:#1a2b1f,stroke:#5cba7c,color:#e6ffe6
    class conpty,shell,clip,gpu,winsys osNode
    class vt vtNode
```

## Source map

| Module | Responsibility |
| --- | --- |
| `main.rs` | eframe bootstrap; configures the wgpu renderer and window, constructs `App`. |
| `app.rs` | The eframe `App`. Owns a `Vec<Tab>`; each tab is a **binary split tree** of panes with one focused leaf. Routes input → PTY, sizes grids, drives the per-frame loop, paints all panes in one GPU callback. |
| `session.rs` | One session = a `Pty` + a `GhosttyVtEngine` + per-pane interaction state (selection, held mouse button, alive flag, OSC 52 scanner). |
| `engine/mod.rs` | The `TerminalEngine` trait and neutral types (`GridSnapshot`, `Cell`, `KeyInput`, `MouseInput`, `Rgb`). |
| `engine/ghostty_vt.rs` | The only libghostty-vt implementation of the trait. |
| `pty.rs` | ConPTY shell via portable-pty, with a reader thread that wakes the UI on output. |
| `render/mod.rs` | wgpu instanced-quad pipeline, per-row run shaping, instance builder, egui `CallbackTrait`. |
| `render/atlas.rs` | rustybuzz shaping + ab_glyph rasterization into an R8 atlas; COLR/CPAL color emoji into an RGBA atlas. |
| `config.rs` | TOML config from `%APPDATA%\giest\config.toml`; the full ANSI 16 + 256-color palette. |
| `profiles.rs` | Shell profiles (pwsh / powershell / cmd / wsl). |
| `osc52.rs` | Side-stream parser for OSC 52 clipboard-set. |

## Why the layering matters

The arrow from `app.rs`/`session.rs` into the terminal model **stops at the
trait boundary** (`engine/mod.rs`). The renderer consumes a `GridSnapshot` of
neutral types — true-color `Rgb`, plain `bool` style flags, `CompactString`
grapheme clusters — and never imports anything from `libghostty_vt`. Likewise
the input layer produces neutral `KeyInput` / `MouseInput` values; only
`engine/ghostty_vt.rs` knows how to turn those into libghostty calls.

The next page breaks the layers down in detail, and
[Data flow](data-flow.md) traces one keystroke and one chunk of shell output
all the way through.
