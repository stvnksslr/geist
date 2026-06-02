# Layered design

giest has four conceptual layers. Data crosses each boundary in a deliberately
narrow shape, which is what keeps the renderer and input code free of any
libghostty or Windows specifics.

```mermaid
flowchart TB
    subgraph L1["1 · Presentation (egui + wgpu)"]
        A["app.rs — tabs, split tree, per-frame loop"]
        R["render/ — glyph atlas + instanced quads"]
    end
    subgraph L2["2 · Session glue"]
        S["session.rs — selection, mouse, OSC 52, PTY pump"]
    end
    subgraph L3["3 · Engine contract"]
        E["engine/mod.rs — TerminalEngine trait + neutral types"]
    end
    subgraph L4["4 · Backend + OS"]
        G["engine/ghostty_vt.rs — libghostty-vt backend"]
        P["pty.rs — ConPTY shell"]
    end

    A --> S
    R -.reads.-> S
    S --> E
    E --> G
    S --> P

    classDef l1 fill:#2a1f3d,stroke:#a06cff,color:#f0e6ff
    classDef l2 fill:#1f2a3d,stroke:#6c9cff,color:#e6f0ff
    classDef l3 fill:#3d2f1f,stroke:#ffb86c,color:#fff0e0
    classDef l4 fill:#1a2b1f,stroke:#5cba7c,color:#e6ffe6
    class A,R l1
    class S l2
    class E l3
    class G,P l4
```

## 1 · Presentation

`app.rs` is the eframe `App`. Every frame it:

1. Pumps PTY output into each pane's engine.
2. Reaps panes/tabs whose shell exited.
3. Handles app-level shortcuts (tabs, splits, focus, font zoom).
4. Lays the focused tab's split tree out into rects.
5. Routes keyboard to the focused pane and mouse/selection interactions.
6. Builds a `PaneFrame` per pane and paints them all in **one** wgpu callback.

`render/` turns each `GridSnapshot` into instanced quads (cell backgrounds,
shaped glyphs sampled from the atlas, cursor) and submits them to the GPU.

## 2 · Session glue

`session.rs` owns the bits that are neither pure rendering nor pure terminal
state: the selection anchor/head, the held mouse button, the alive flag, and
the OSC 52 scanner. `pump_pty` drains PTY output into the engine and flushes
the engine's responses back to the PTY. It also translates egui events into the
neutral `KeyInput` / `MouseInput` values the engine encodes.

## 3 · Engine contract

[`engine/mod.rs`](../components/engine.md) defines the **only** vocabulary the
upper layers share with the terminal model:

- **Inputs:** `write(bytes)`, `encode_key`, `encode_mouse`, `encode_paste`,
  `resize`, `scroll*`, `apply_theme`, `set_cursor_color`.
- **Outputs:** `snapshot(&mut GridSnapshot)`, `take_responses`, `title`,
  `is_mouse_tracking`.

The engine resolves palette indices and the default fg/bg to **concrete RGB**,
and pre-applies `inverse`, so the renderer only ever deals in true color.

## 4 · Backend + OS

`engine/ghostty_vt.rs` is the sole bridge to libghostty-vt. `pty.rs` is the
sole bridge to the Windows ConPTY shell. Swapping either one out (a different
VT engine, a different PTY transport) is a localized change — nothing above
layer 3 references them by name.

## The data that crosses each boundary

```mermaid
flowchart LR
    egui["egui events<br/>(Key, Text, Pointer, Paste, Copy)"]
    neutral["Neutral types<br/>KeyInput · MouseInput · paste str"]
    bytes["VT byte sequences"]
    grid["GridSnapshot<br/>cells · cursor · colors"]
    quads["Instance quads"]

    egui -->|session.rs translates| neutral
    neutral -->|engine encodes| bytes
    bytes -->|to PTY / from PTY| bytes
    bytes -->|engine.write + snapshot| grid
    grid -->|render builds| quads

    classDef n fill:#1f2a3d,stroke:#6c9cff,color:#e6f0ff
    class egui,neutral,bytes,grid,quads n
```
