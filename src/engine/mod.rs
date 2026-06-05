//! Terminal-state engine abstraction.
//!
//! The renderer and app talk to the terminal model through [`TerminalEngine`],
//! never directly to libghostty-vt. This keeps the GPU renderer and input layer
//! independent of the backend and leaves room for a pure-Rust fallback engine
//! (see the project plan) without touching the rest of the app.

use anyhow::Result;
use compact_str::CompactString;

pub mod ghostty_vt;

pub use ghostty_vt::GhosttyVtEngine;

/// 24-bit color. The engine resolves palette indices and the default fg/bg to
/// concrete RGB so the renderer only ever deals in true color.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Rgb {
    pub r: u8,
    pub g: u8,
    pub b: u8,
}

impl Rgb {
    pub const fn new(r: u8, g: u8, b: u8) -> Self {
        Self { r, g, b }
    }
}

/// A single rendered grid cell. `inverse` is already applied to `fg`/`bg`, so
/// the renderer can paint these colors directly.
#[derive(Clone, Debug, Default)]
pub struct Cell {
    /// Grapheme cluster for this cell. Empty means a blank cell (or the tail
    /// half of a wide character). An inline string: clusters of ≤24 bytes (the
    /// overwhelming majority) live on the stack, so building and cloning the
    /// per-frame snapshot does not allocate per cell.
    pub text: CompactString,
    pub fg: Rgb,
    pub bg: Rgb,
    pub bold: bool,
    pub italic: bool,
    pub underline: bool,
    pub strikethrough: bool,
}

/// Cursor shape reported by the terminal (DECSCUSR / app-set).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum CursorShape {
    #[default]
    Block,
    Bar,
    Underline,
    HollowBlock,
}

/// An immutable view of the screen at one instant, in cells. Row-major:
/// `cells[y * cols + x]`. Reused across frames — callers pass a `&mut` to
/// [`TerminalEngine::snapshot`] which clears and refills it.
#[derive(Clone, Debug, Default)]
pub struct GridSnapshot {
    pub cols: u16,
    pub rows: u16,
    pub cells: Vec<Cell>,
    /// The single grid row immediately *above* the viewport top — the line a
    /// sub-line upward scroll partially reveals. Populated only while smooth
    /// scrolling (see [`crate::session::Session::scroll_offset_px`]); empty
    /// otherwise. `cols` cells wide.
    pub over_row: Vec<Cell>,
    pub cursor_x: u16,
    pub cursor_y: u16,
    pub cursor_visible: bool,
    pub cursor_blinking: bool,
    pub cursor_shape: CursorShape,
    pub cursor_color: Rgb,
    pub default_fg: Rgb,
    pub default_bg: Rgb,
}

impl GridSnapshot {
    pub fn cell(&self, x: u16, y: u16) -> Option<&Cell> {
        if x >= self.cols || y >= self.rows {
            return None;
        }
        self.cells.get(y as usize * self.cols as usize + x as usize)
    }
}

/// Keyboard modifiers, backend-neutral.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct KeyMods {
    pub shift: bool,
    pub ctrl: bool,
    pub alt: bool,
    pub sup: bool,
}

/// A physical key, named after the W3C `KeyboardEvent.code` values that
/// libghostty's key encoder uses. Only the subset we translate is listed;
/// printable text is delivered separately via [`KeyInput::text`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum KeyCode {
    A, B, C, D, E, F, G, H, I, J, K, L, M,
    N, O, P, Q, R, S, T, U, V, W, X, Y, Z,
    Digit0, Digit1, Digit2, Digit3, Digit4, Digit5, Digit6, Digit7, Digit8, Digit9,
    Enter, Tab, Backspace, Escape, Space, Delete, Insert,
    Home, End, PageUp, PageDown,
    ArrowUp, ArrowDown, ArrowLeft, ArrowRight,
    F1, F2, F3, F4, F5, F6, F7, F8, F9, F10, F11, F12,
    Minus, Equal, BracketLeft, BracketRight, Backslash,
    Semicolon, Quote, Backquote, Comma, Period, Slash,
}

/// A keyboard event to be encoded into a terminal byte sequence.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct KeyInput {
    pub code: KeyCode,
    /// Resolved text for this key, if egui produced any (used for layout/IME
    /// correctness). Usually `None` for the key-event path.
    pub text: Option<String>,
    pub mods: KeyMods,
    pub press: bool,
}

/// A mouse button, backend-neutral. Wheel up/down map to buttons 4/5.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MouseButton {
    Left,
    Middle,
    Right,
    WheelUp,
    WheelDown,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MouseAction {
    Press,
    Release,
    Motion,
}

/// A mouse event to encode for the PTY when the app has mouse reporting on.
#[derive(Clone, Copy, Debug)]
pub struct MouseInput {
    pub action: MouseAction,
    pub button: Option<MouseButton>,
    /// Position in pixels relative to the grid's top-left.
    pub pos_px: (u32, u32),
    /// Cell size in pixels.
    pub cell_px: (u32, u32),
    /// Total grid size in pixels (cols*cell_w, rows*cell_h).
    pub screen_px: (u32, u32),
    pub mods: KeyMods,
}

/// The backend-agnostic terminal model: bytes in, grid out.
pub trait TerminalEngine {
    /// Feed VT-encoded bytes (typically from the PTY) into the parser.
    fn write(&mut self, bytes: &[u8]);

    /// Resize the terminal grid. `cell_px` is the cell size in pixels, used
    /// only for in-band size reports / image protocols.
    fn resize(&mut self, cols: u16, rows: u16, cell_px: (u32, u32)) -> Result<()>;

    /// Scroll the viewport through scrollback by `delta` lines (negative scrolls
    /// up toward older output).
    fn scroll(&mut self, delta: isize);

    /// Jump the viewport to the bottom (newest output). Called when the user
    /// types, matching Ghostty's "scroll to bottom on input".
    fn scroll_to_bottom(&mut self);

    /// Jump the viewport to the top (oldest scrollback).
    fn scroll_to_top(&mut self);

    /// Number of rows in scrollback above the viewport (i.e. how many lines the
    /// viewport can still be scrolled up). Used to clamp the scroll position.
    fn scrollback_rows(&self) -> usize;

    /// Capture the single grid row immediately above the current viewport top
    /// (the line a sub-line upward scroll reveals) into `out` (`cols` cells).
    /// Leaves the viewport position unchanged. Used only for smooth scrolling.
    fn snapshot_over_row(&mut self, out: &mut Vec<Cell>) -> Result<()>;

    /// Apply a color theme: default foreground/background and the 256-color
    /// palette. Snapshots taken afterward resolve colors against these.
    fn apply_theme(&mut self, fg: Rgb, bg: Rgb, palette: &[Rgb; 256]) -> Result<()>;

    /// Set the default cursor color, or clear it (`None`) so the running
    /// program / engine default applies. Snapshots resolve the cursor color
    /// against this.
    fn set_cursor_color(&mut self, color: Option<Rgb>) -> Result<()>;

    /// Whether the running app has enabled mouse reporting (any tracking mode).
    fn is_mouse_tracking(&self) -> bool;

    /// Encode a mouse event into the PTY byte sequence for the active mouse
    /// reporting mode/protocol. Empty if the event produces no report.
    fn encode_mouse(&mut self, m: &MouseInput) -> Vec<u8>;

    /// Refill `out` with the current screen contents.
    fn snapshot(&mut self, out: &mut GridSnapshot) -> Result<()>;

    /// Encode a key event into the terminal byte sequence to send to the PTY,
    /// honoring the terminal's current modes (cursor-key application mode,
    /// kitty keyboard protocol, etc.). May return empty (e.g. bare modifiers).
    fn encode_key(&mut self, input: &KeyInput) -> Vec<u8>;

    /// Encode pasted text for the PTY: wrapped in bracketed-paste markers when
    /// the app enabled mode 2004, otherwise with newlines normalized to CR.
    fn encode_paste(&mut self, text: &str) -> Vec<u8>;

    /// Drain any bytes the terminal wants written back to the PTY (responses to
    /// device queries, status reports, etc.). Returns empty when there's none.
    fn take_responses(&mut self) -> Vec<u8>;

    /// The window title set by the shell via OSC 0/2, if any.
    fn title(&self) -> Option<String>;
}
