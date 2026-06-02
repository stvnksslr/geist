# `app.rs` — window, tabs & panes

`app.rs` is the eframe `App`: it owns the tabs, the split-tree of panes inside
each tab, the shared cell metrics, and the per-frame loop that drives and paints
everything.

## Tabs and the split tree

A `Tab` is a **binary split tree** of panes with one focused leaf. Splits divide
only the *focused* pane, so they nest like Ghostty rather than re-flowing onto a
shared axis.

```mermaid
classDiagram
    class App {
        tabs: Vec~Tab~
        active_tab: usize
        next_id: u64
        cell_w, cell_h: f32
        font_points: f32
        config: Config
        profiles: Vec~Profile~
    }
    class Tab {
        root: Node
        focus: u64
    }
    class Node {
        <<enum>>
        Leaf id session
        Split vertical first second
        Empty
    }
    App "1" --> "*" Tab
    Tab --> Node
    Node --> Node : Split has two children
```

`Node` operations are pure tree transforms:

- `split_leaf` — replace the focused leaf with a `Split` of the old pane + a new
  leaf.
- `remove_leaf` / `prune_dead` — drop a leaf (closed, or its shell exited),
  collapsing a split into its surviving child.
- `collect` — divide a rect by each split's axis (1px gutter) and emit a `Leaf`
  (id, session, rect) per pane.

```mermaid
flowchart TB
    subgraph before["Before split (focus = A)"]
        A1["Leaf A"]
    end
    subgraph after["After Ctrl+Shift+D (vertical)"]
        S["Split vertical"]
        A2["Leaf A"]
        B2["Leaf B (new, focused)"]
        S --> A2
        S --> B2
    end
    before -->|split_leaf| after
```

## The per-frame loop

`eframe::App::ui` runs once per frame:

```mermaid
flowchart TB
    repaint["request_repaint_after(500ms)<br/>(poll for idle shell exit)"]
    pump["pump every pane's PTY<br/>(all tabs, so background panes keep flowing)"]
    reap["reap_dead → drop dead panes/tabs<br/>close window if none remain"]
    short["handle_shortcuts (tabs, splits, focus)"]
    zoom["handle_font_zoom (Ctrl +/-/0)"]
    title["sync window title from focused pane (OSC 0/2)"]
    bar["draw tab strip + profile picker"]
    active["render_active: lay out, drive, paint panes"]

    repaint --> pump --> reap --> short --> zoom --> title --> bar --> active
```

### `render_active` in detail

```mermaid
flowchart TB
    layout["collect() split tree → Vec&lt;Leaf&gt; with rects"]
    focus["focus-follows-click:<br/>a press inside a pane focuses it"]
    kbd["focused pane: handle_input(ctx)"]
    loop["for each leaf:"]
    fit["fit_grid (resize engine + PTY to pane)"]
    snap["update_snapshot()"]
    interact["focused pane only:<br/>mouse tracking OR selection/URL/focus"]
    frame["build PaneFrame (snapshot, origin_px, selection, cursor_hollow)"]
    paint["one wgpu callback paints ALL panes"]
    outline["outline focused pane if tab is split"]

    layout --> focus --> kbd --> loop
    loop --> fit --> snap --> interact --> frame
    frame --> paint --> outline
```

Notable behaviors:

- **One GPU callback for all panes.** Every visible pane's `PaneFrame` is handed
  to a single `egui_wgpu::Callback`, so they share one instance buffer.
- **Focus lock.** The focused pane requests egui focus and locks Tab + arrow
  keys to itself, so egui's built-in focus traversal can't steal Tab (which the
  shell wants) or fire a tab-strip button on a following Enter/Space.
- **Hollow cursor.** Only the focused pane of a focused window gets a solid,
  blinking cursor; every other visible cursor is drawn hollow
  (`cursor_hollow`), matching Ghostty.
- **Pixel snapping.** The grid origin is snapped to the physical pixel grid so
  cell boundaries land on pixels and glyphs stay crisp.

## Keyboard shortcut ownership

`handle_shortcuts` (app-level) and `Session::handle_input` (shell-level)
partition the keyboard:

| Keys | Owner | Effect |
| --- | --- | --- |
| Ctrl+Shift+T / W | app | new / close tab (or pane) |
| Ctrl+Shift+D / E | app | split vertical / horizontal |
| Ctrl+Shift+1–9 | app | jump to tab (9 = last) |
| Ctrl+Tab / Ctrl+Shift+Tab | app | cycle tabs |
| Ctrl+ +/-/0 | app | font zoom in/out/reset |
| Shift+PageUp/Down/Home/End | session→engine | scrollback |
| everything else | session→engine | encoded to the shell |
