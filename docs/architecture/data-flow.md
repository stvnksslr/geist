# Data flow

Two round trips capture almost everything geist does: a **keystroke going out**
to the shell, and a **chunk of output coming back** to the screen. Both pass
through the same narrow boundaries described in [Layered design](layers.md).

## Output: shell → screen

When the shell writes bytes, ConPTY delivers them on a background reader thread,
which wakes the UI. On the next egui frame the bytes are parsed by libghostty-vt
and the resulting grid is snapshotted and painted.

```mermaid
sequenceDiagram
    autonumber
    participant Shell as Shell (pwsh/cmd/wsl)
    participant ConPTY as ConPTY
    participant Reader as pty.rs reader thread
    participant Ctx as egui::Context
    participant Sess as session.rs
    participant Eng as ghostty_vt.rs
    participant VT as libghostty-vt
    participant Ren as render/
    participant GPU as GPU

    Shell->>ConPTY: writes stdout/stderr bytes
    ConPTY->>Reader: read() returns N bytes
    Reader->>Ctx: wake() → request_repaint()
    Note over Reader,Ctx: bytes queued on an mpsc channel
    Ctx->>Sess: next frame: pump_pty()
    Sess->>Eng: write(chunk)
    Eng->>VT: vt_write(bytes)
    Sess->>Sess: engine clipboard callbacks → clipboard set?
    Sess->>Eng: take_responses()
    Eng-->>Sess: device-query replies
    Sess->>ConPTY: pty.write(responses)
    Ctx->>Sess: update_snapshot()
    Sess->>Eng: snapshot(&mut GridSnapshot)
    Eng->>VT: RenderState + row/cell iterators
    VT-->>Eng: cells, cursor, colors
    Eng-->>Sess: filled GridSnapshot (true-color)
    Ctx->>Ren: paint callback with PaneFrames
    Ren->>GPU: instanced quads (bg, glyphs, cursor)
```

Key facts:

- The reader thread **never blocks the UI**; it only enqueues bytes and calls
  `wake()`.
- `take_responses()` drains bytes libghostty wants written back (replies to
  device-status queries etc.) and the session flushes them to the PTY — closing
  the loop without the renderer ever being involved.
- The engine resolves all colors to RGB and pre-applies `inverse` during
  `snapshot`, so `render/` reads a fully-resolved grid.

## Input: keystroke → shell

egui hands geist already-classified events. `session.rs` decides whether each
event is for the **app** (tabs, splits, scrollback, copy/paste) or the
**shell**, and only the latter is encoded to bytes and written to the PTY.

```mermaid
sequenceDiagram
    autonumber
    participant User
    participant Egui as egui (winit)
    participant App as app.rs
    participant Sess as session.rs
    participant Eng as ghostty_vt.rs
    participant VT as libghostty-vt
    participant PTY as pty.rs → ConPTY
    participant Shell

    User->>Egui: presses a key
    Egui->>App: Event::Key / Text / Paste / Copy
    App->>App: handle_shortcuts (Ctrl+Shift+*, Ctrl+Tab)
    Note over App: app shortcuts are consumed here,<br/>never reach the shell
    App->>Sess: handle_input(ctx, tracking, cell_h)
    alt App-reserved (Ctrl+Shift+*, Shift+PageUp, Ctrl+0/±)
        Sess->>Eng: scroll() / nothing → swallowed
    else Copy/Cut with a selection
        Sess->>Egui: ctx.copy_text(selection)
    else Real shell input
        Sess->>Eng: encode_key / encode_paste(KeyInput)
        Eng->>VT: Encoder.set_options_from_terminal + encode
        VT-->>Eng: mode-aware byte sequence
        Eng-->>Sess: Vec<u8>
        Sess->>Eng: scroll_to_bottom()
        Sess->>PTY: pty.write(bytes)
        PTY->>Shell: stdin bytes
    end
```

Key facts:

- **Ctrl+Shift+\*** is the app's namespace and is never sent to the shell.
- The key encoder reads the terminal's **live modes** (cursor-key application
  mode, kitty keyboard protocol, modifyOtherKeys) via
  `set_options_from_terminal`, so the same physical key encodes differently
  depending on what the program enabled.
- Typing calls `scroll_to_bottom()` first, matching Ghostty's "scroll to bottom
  on input."

## Mouse reporting

When the running program enables mouse tracking, pointer events are encoded
instead of driving selection:

```mermaid
flowchart TB
    ev["egui PointerButton / PointerMoved / scroll"]
    track{"engine.is_mouse_tracking()?"}
    enc["engine.encode_mouse(MouseInput)"]
    sel["selection / URL / focus handling in app.rs"]
    pty["pty.write(report)"]

    ev --> track
    track -- yes --> enc --> pty
    track -- no --> sel

    classDef n fill:#1f2a3d,stroke:#6c9cff,color:#e6f0ff
    class ev,enc,sel,pty n
```

The encoder is fed cell size, grid pixel size, and pointer pixel position, and
produces the report for whatever protocol the app selected (X10 / SGR 1006,
with wheel mapped to buttons 4/5).
