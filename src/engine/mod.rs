//! Terminal-state engine abstraction.
//!
//! The renderer and app talk to the terminal model through [`TerminalEngine`],
//! never directly to libghostty-vt. This keeps the GPU renderer and input layer
//! independent of the backend and leaves room for a pure-Rust fallback engine
//! (see the project plan) without touching the rest of the app.

use std::sync::Arc;

use anyhow::Result;
use compact_str::CompactString;

pub mod ghostty_vt;
pub mod png_decode;

pub use ghostty_vt::GhosttyVtEngine;
/// WCAG color math, shared by the engine's `minimum-contrast` and the chrome's
/// accent floor (see [`crate::theme`]) so both agree on what "readable" means.
pub use ghostty_vt::{contrast_ratio, luminance};

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

/// Policy for the foreground color of **bold** text. Ghostty's `bold-color`
/// option, which also subsumes the deprecated `bold-is-bright`. Resolved in the
/// engine's per-cell color pass (it needs the cell's *raw* style color, which
/// the renderer never sees once palette indices are flattened to RGB).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum BoldColor {
    /// No special treatment: bold text keeps the cell's own foreground. Default.
    #[default]
    None,
    /// Bold text using a standard ANSI color (0–7) uses the bright variant
    /// (8–15). This is `bold-is-bright`.
    Bright,
    /// Bold text uses this fixed color (for default/RGB foregrounds; an explicit
    /// ANSI palette color is still brightened, matching Ghostty).
    Color(Rgb),
}

/// Underline style reported by the terminal (SGR 4 and its `4:n` variants).
/// Mirrors libghostty-vt's `style::Underline`.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum UnderlineStyle {
    #[default]
    None,
    Single,
    Double,
    Curly,
    Dotted,
    Dashed,
}

/// A single rendered grid cell. `inverse` is already applied to `fg`/`bg`, so
/// the renderer can paint these colors directly. The remaining decoration
/// attributes (underline style/color, overline, strikethrough, faint, blink,
/// invisible) are carried verbatim from the VT engine for the renderer to honor.
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
    /// Underline style; [`UnderlineStyle::None`] means no underline.
    pub underline: UnderlineStyle,
    /// Explicit underline color (SGR 58); `None` underlines in the cell's `fg`.
    pub underline_color: Option<Rgb>,
    pub strikethrough: bool,
    /// Overline (SGR 53).
    pub overline: bool,
    /// Faint / dim (SGR 2): the glyph is drawn at reduced intensity.
    pub faint: bool,
    /// Blink (SGR 5): the glyph is hidden on the blink-off phase.
    pub blink: bool,
    /// Invisible / conceal (SGR 8): the glyph is not drawn (background remains).
    pub invisible: bool,
    /// The style set an *explicit* background color, i.e. the cell's `bg` is not
    /// the terminal default. Recorded before `inverse` is applied.
    ///
    /// Under `background-opacity` a cell whose background is the terminal default
    /// draws no background quad at all, so the translucent window background shows
    /// through it — Ghostty's `bg_style != null` test (`renderer/generic.zig`).
    ///
    /// NOTE the polarity: `false` (the [`Default`]) means "no quad". A cell the VT
    /// iterators never yield is blanked to the default, and phrasing this the other
    /// way round (`bg_is_default`) would make every one of those paint opaque black.
    pub bg_explicit: bool,
    /// Inverse / reverse video (SGR 7). Already applied to `fg`/`bg`; carried
    /// separately because an inverse cell's background is always opaque regardless
    /// of `background-opacity`, and that rule is checked before the
    /// `background-opacity-cells` one.
    pub inverse: bool,
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
    /// True if any cell in this snapshot has the blink attribute set, so the app
    /// knows to keep repainting for the blink animation (and can skip the timer
    /// when nothing blinks).
    pub has_blink: bool,
    /// Kitty graphics placements visible in this viewport, sorted by
    /// `(z, image_id)` — Ghostty's draw order. Rebuilt on **every** snapshot,
    /// including ones that take the "nothing changed" fast path.
    pub images: Vec<ImagePlacement>,
}

/// Decoded pixels for one kitty graphics image, always straight-alpha RGBA8.
///
/// The VT engine lends image data only until the next terminal write, while a
/// [`GridSnapshot`] is an owned value cloned per pane and uploaded to the GPU
/// from a *later* frame. So pixels are copied out during the borrow — once per
/// image id, not per frame — and shared by [`Arc`] from there on.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ImageData {
    pub width: u32,
    pub height: u32,
    /// `width * height * 4` bytes, straight (non-premultiplied) alpha.
    pub rgba: Vec<u8>,
}

/// One kitty graphics placement, positioned relative to the **viewport**.
#[derive(Clone, Debug)]
pub struct ImagePlacement {
    pub image_id: u32,
    pub placement_id: u32,
    /// Shared with every other placement and snapshot referencing this image id.
    /// The renderer keys its texture cache on `Arc::ptr_eq` against this, which
    /// is how it notices a *re-transmit* — the VT engine exposes no generation
    /// counter or transmit timestamp.
    pub data: Arc<ImageData>,
    /// Viewport-relative top-left cell. `row` is signed because a placement's
    /// origin can scroll above the viewport top while part of it is still shown.
    pub col: i32,
    pub row: i32,
    /// Cells the placement spans.
    pub grid_cols: u32,
    pub grid_rows: u32,
    /// Pixel offset within the origin cell (kitty `X=` / `Y=`).
    pub x_offset: u32,
    pub y_offset: u32,
    /// Destination size in pixels, already resolved for the source rect, any
    /// `c=`/`r=` cell span, and aspect ratio by the VT engine.
    pub dest_w: u32,
    pub dest_h: u32,
    /// Source rectangle in image pixels, already clamped to the image bounds.
    pub src_x: u32,
    pub src_y: u32,
    pub src_w: u32,
    pub src_h: u32,
    /// Kitty z-index: `< 0` draws under text, `>= 0` over it (see
    /// `crate::render::image_layer`).
    pub z: i32,
    /// The engine's own visibility verdict, computed against the engine's
    /// viewport. The renderer widens it by a row while smooth-scrolling.
    pub visible: bool,
}

impl GridSnapshot {
    pub fn cell(&self, x: u16, y: u16) -> Option<&Cell> {
        if x >= self.cols || y >= self.rows {
            return None;
        }
        self.cells.get(y as usize * self.cols as usize + x as usize)
    }
}

/// One screen row rendered to text for scrollback search: the row's characters
/// (each cell's grapheme, blank cells as a space, trailing blanks trimmed) and,
/// parallel to `chars`, the grid column each char originated from — so a match's
/// char range maps back to a cell-column span.
#[derive(Clone, Debug, Default)]
pub struct RowText {
    /// Absolute screen-row index (0 = the oldest scrollback row).
    pub row: u32,
    pub chars: Vec<char>,
    pub cols: Vec<u16>,
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
    A,
    B,
    C,
    D,
    E,
    F,
    G,
    H,
    I,
    J,
    K,
    L,
    M,
    N,
    O,
    P,
    Q,
    R,
    S,
    T,
    U,
    V,
    W,
    X,
    Y,
    Z,
    Digit0,
    Digit1,
    Digit2,
    Digit3,
    Digit4,
    Digit5,
    Digit6,
    Digit7,
    Digit8,
    Digit9,
    Enter,
    Tab,
    Backspace,
    Escape,
    Space,
    Delete,
    Insert,
    Home,
    End,
    PageUp,
    PageDown,
    ArrowUp,
    ArrowDown,
    ArrowLeft,
    ArrowRight,
    F1,
    F2,
    F3,
    F4,
    F5,
    F6,
    F7,
    F8,
    F9,
    F10,
    F11,
    F12,
    Minus,
    Equal,
    BracketLeft,
    BracketRight,
    Backslash,
    Semicolon,
    Quote,
    Backquote,
    Comma,
    Period,
    Slash,
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

    /// Whether the cursor sits on an OSC 133 semantic-prompt row, i.e. the shell
    /// is waiting at a prompt rather than running a command. `None` when the
    /// engine can't tell.
    ///
    /// Used by `confirm-close-surface` to decide whether a pane is busy. Note
    /// that `Some(false)` is ambiguous — the shell may simply not emit prompt
    /// marks — so the caller latches "we have ever seen a mark" to disambiguate.
    fn cursor_at_prompt(&self) -> Option<bool> {
        None
    }

    /// The *effective* default foreground / background / cursor colors: the
    /// configured values as overridden by OSC 10/11/12, which the VT engine
    /// applies internally. `cursor` is `None` when no cursor color is set.
    ///
    /// Needed to answer an OSC color **query**, which libghostty-vt's read-only
    /// stream parses and then discards, so giest side-scans and replies itself
    /// (see [`crate::osc_color`]).
    fn dynamic_colors(&self) -> (Rgb, Rgb, Option<Rgb>);

    /// Set the bold-text foreground policy (Ghostty `bold-color` /
    /// `bold-is-bright`). Applied while building each snapshot's cell colors.
    fn set_bold_color(&mut self, bold: BoldColor) -> Result<()>;

    /// Set the kitty-graphics image storage limit in bytes (Ghostty
    /// `image-storage-limit`).
    ///
    /// **Zero disables the protocol entirely and deletes every stored image** —
    /// and zero is where libghostty starts, so inline images do not work at all
    /// until this is called. Engines without image support ignore it.
    fn set_image_storage_limit(&mut self, _bytes: u64) -> Result<()> {
        Ok(())
    }

    /// Set the minimum foreground/background contrast ratio (WCAG; Ghostty
    /// `minimum-contrast`, in `1.0..=21.0`; `1.0` disables it). When a cell's
    /// resolved colors fall below this ratio the foreground is forced to pure
    /// black or white — whichever contrasts more — as the snapshot is built.
    fn set_min_contrast(&mut self, ratio: f32) -> Result<()>;

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

    /// Whether the running app has enabled bracketed paste (DECSET 2004) — i.e.
    /// whether it has promised to treat pasted text as data rather than input.
    ///
    /// [`Self::encode_paste`] consults the same mode, but swallows the answer;
    /// paste protection has to know it *before* deciding whether to encode at
    /// all (see [`crate::config::paste_is_unsafe`]).
    fn bracketed_paste(&self) -> bool;

    /// Drain any bytes the terminal wants written back to the PTY (responses to
    /// device queries, status reports, etc.). Returns empty when there's none.
    fn take_responses(&mut self) -> Vec<u8>;

    /// Take and clear the "a bell rang since the last drain" flag (set when the
    /// terminal processed a BEL, 0x07). One-shot. Engines without bell support
    /// return `false` (the default).
    fn take_bell(&mut self) -> bool {
        false
    }

    /// The window title set by the shell via OSC 0/2, if any.
    fn title(&self) -> Option<String>;

    /// The OSC 8 hyperlink URI at viewport cell `(x, y)`, if that cell is part of
    /// a hyperlink. Resolved on demand (e.g. on Ctrl+click), not per frame — the
    /// backing grid-reference API is explicitly *not* meant for the render loop.
    /// Engines without hyperlink support return `None` (the default).
    fn hyperlink_at(&self, _x: u16, _y: u16) -> Option<String> {
        None
    }

    /// Read every screen row (scrollback + viewport), oldest first, as text for
    /// scrollback search. Resolved on demand (when a search runs/refreshes), never
    /// per frame — it walks the whole grid cell-by-cell. Engines without support
    /// return an empty vec (the default).
    fn screen_text(&self) -> Vec<RowText> {
        Vec::new()
    }

    /// Find the OSC 133 prompt to jump to: the `delta`-th semantic-prompt row
    /// above (`delta < 0`) or below (`delta > 0`) the current viewport top.
    /// Returns the target's offset in whole lines above the live bottom (`0` =
    /// bottom), which the caller turns into a scroll position, or `None` if there
    /// is no such prompt (or the engine lacks semantic-prompt support — the
    /// default). Resolved on demand (a keypress), not per frame.
    fn jump_to_prompt(&self, _delta: isize) -> Option<usize> {
        None
    }
}
