//! [`TerminalEngine`] backed by libghostty-vt (the primary backend).

use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::Rc;
use std::sync::Arc;

use anyhow::Result;
use libghostty_vt::kitty::graphics::{Compression, ImageFormat, PlacementIterator};
use libghostty_vt::key::{Action, Encoder, Event, Key, Mods};
use libghostty_vt::mouse;
use libghostty_vt::paste;
use libghostty_vt::render::{CellIteration, CellIterator, CursorVisualStyle, Dirty, RowIterator};
use libghostty_vt::screen::{CellWide, RowSemanticPrompt, TrackedGridRef};
use libghostty_vt::style::{StyleColor, Underline};
use libghostty_vt::terminal::{Mode, Point, PointCoordinate, PointSpace, ScrollViewport};
use libghostty_vt::{RenderState, Terminal};

use super::{
    BoldColor, Cell, CursorShape, GridSnapshot, ImageData, ImagePlacement, KeyCode, KeyInput,
    MouseAction, MouseButton, MouseInput, Rgb, SelectKind, TerminalEngine, UnderlineStyle,
};

/// Shared sink for bytes libghostty wants written back to the PTY. The
/// `on_pty_write` callback must not call back into the terminal, so it only
/// pushes bytes here; we drain them after each `write`.
type ResponseSink = Rc<RefCell<Vec<u8>>>;

pub struct GhosttyVtEngine {
    term: Terminal<'static, 'static>,
    render_state: RenderState<'static>,
    rows_buf: RowIterator<'static>,
    cells_buf: CellIterator<'static>,
    encoder: Encoder<'static>,
    key_event: Event<'static>,
    mouse_encoder: mouse::Encoder<'static>,
    mouse_event: mouse::Event<'static>,
    responses: ResponseSink,
    /// Set by the `on_bell` callback when the terminal processes a BEL (0x07);
    /// drained one-shot by [`Self::take_bell`] so the app can flash a visual bell.
    /// (Fully qualified to avoid clashing with the grid [`Cell`].)
    bell: Rc<std::cell::Cell<bool>>,
    /// Set when the viewport is scrolled, forcing the next [`Self::snapshot`] to
    /// do a full rebuild even if libghostty reports the frame clean — insurance
    /// for the dirty-skip fast path against any case where a pure viewport move
    /// isn't flagged dirty.
    viewport_moved: bool,
    /// Bold-text foreground policy (Ghostty `bold-color` / `bold-is-bright`),
    /// applied per cell in [`copy_cell`].
    bold_color: BoldColor,
    /// Minimum fg/bg contrast ratio (Ghostty `minimum-contrast`); `1.0` = off.
    min_contrast: f32,
    /// Reusable kitty-graphics placement iterator. Owned, with a `Drop` that
    /// frees the FFI object, so it's allocated once like `rows_buf`/`cells_buf`
    /// rather than per snapshot.
    placements: PlacementIterator<'static>,
    /// Copied image pixels, keyed by kitty image id. The copy out of the VT
    /// engine's storage happens **once per id**, not per frame; later snapshots
    /// just clone an `Arc`. Entries are evicted by *absence* from the placement
    /// walk — the binding exposes neither image enumeration nor any delete
    /// notification, so absence is the only signal available.
    image_cache: HashMap<u32, Arc<ImageData>>,
    /// Retained scratch: image ids seen during this frame's walk, for eviction.
    image_ids_seen: Vec<u32>,
    /// The selection's fixed end (the drag anchor), as a **tracked** reference:
    /// libghostty follows it through scrolling, scrollback eviction and reflow,
    /// so a drag started ten screens ago still extends from the right cell.
    ///
    /// The moving end is not tracked — it is wherever the pointer is *now*, and
    /// is resolved fresh on every update. The selection itself is owned by the
    /// terminal (`set_selection` converts it to tracked state internally); this
    /// anchor exists only because the binding exposes no way to read the active
    /// selection back.
    sel_anchor: Option<TrackedGridRef>,
    /// The selection's moving end, tracked for the same reason as the anchor —
    /// but only because `adjust_selection` has to *rebuild* the selection to
    /// move it, and the terminal's own copy cannot be read back (the binding
    /// leaves `GHOSTTY_TERMINAL_DATA_SELECTION` unbound). A drag doesn't need
    /// it: the end is wherever the pointer is now.
    sel_head: Option<TrackedGridRef>,
    /// Whether the live selection is a rectangle/block. Remembered because
    /// `adjust_selection` *rebuilds* the selection and would otherwise silently
    /// turn a block selection back into a linear one.
    sel_rectangle: bool,
    /// Whether a selection is installed in the terminal. Mirrors the terminal's
    /// own state, which the binding cannot be asked for.
    selection_installed: bool,
    /// Tracked reference to the row scrollback search captured last, so match
    /// rows can be corrected when eviction renumbers the screen. One reference
    /// for the whole match list — see [`TerminalEngine::set_row_anchor`].
    row_anchor: Option<TrackedGridRef>,
    /// A selection changed since the last snapshot. Forces a full rebuild, the
    /// same insurance `viewport_moved` provides: installing a selection does not
    /// necessarily dirty the render state, and an otherwise-idle frame taking
    /// the clean fast path would leave the highlight unpainted.
    selection_dirty: bool,
}

impl GhosttyVtEngine {
    /// Start tracking the cell an untracked [`GridRef`] points at.
    ///
    /// A tracked reference is created from a *point*, not from a ref, so the ref
    /// is resolved to screen coordinates first. Screen space (not viewport) is
    /// deliberate: it is stable against scrolling, which is the whole reason the
    /// anchor is tracked.
    fn track(&self, gr: &libghostty_vt::screen::GridRef<'_>) -> Option<TrackedGridRef> {
        let p = self
            .term
            .point_from_grid_ref(gr, PointSpace::Screen)
            .ok()??;
        self.term.track_grid_ref(Point::Screen(p)).ok()
    }

    /// Start tracking a viewport cell.
    fn track_viewport(&self, x: u16, y: u16) -> Option<TrackedGridRef> {
        self.term
            .track_grid_ref(Point::Viewport(PointCoordinate { x, y: y as u32 }))
            .ok()
    }

    /// Track both ends of `sel` and install it as the terminal's selection.
    ///
    /// Order is load-bearing: the ends are tracked **before** `set_selection`,
    /// which is a mutating call that invalidates every untracked reference —
    /// including the two inside `sel`.
    #[expect(clippy::type_complexity, reason = "two pins, returned together")]
    fn install_selection(
        &self,
        sel: &libghostty_vt::selection::Selection<'_>,
    ) -> Option<(Option<TrackedGridRef>, Option<TrackedGridRef>)> {
        let ends = (self.track(&sel.start()), self.track(&sel.end()));
        self.term.set_selection(Some(sel)).ok()?;
        Some(ends)
    }

    /// Take ownership of the pins from a successful [`Self::install_selection`],
    /// marking the frame dirty. Returns whether anything was installed, which is
    /// what every selection entry point reports back.
    fn adopt_selection(
        &mut self,
        installed: Option<(Option<TrackedGridRef>, Option<TrackedGridRef>)>,
    ) -> bool {
        match installed {
            Some((anchor, head)) => {
                self.sel_anchor = anchor;
                self.sel_head = head;
                self.selection_installed = true;
                self.selection_dirty = true;
                true
            }
            None => false,
        }
    }

    pub fn new(cols: u16, rows: u16, max_scrollback: usize) -> Result<Self> {
        // Kitty `f=100` (PNG) transmissions are rejected until a decoder is
        // installed on *this* thread; see `png_decode::install`.
        super::png_decode::install();

        let mut term = Terminal::new(cols, rows)?;
        // `usize::MAX` is `scrollback-limit-bytes = unlimited`.
        term.set_scrollback_max_bytes((max_scrollback != usize::MAX).then_some(max_scrollback))?;

        let responses: ResponseSink = Rc::new(RefCell::new(Vec::new()));
        let sink = responses.clone();
        term.on_pty_write(move |_term, data| {
            sink.borrow_mut().extend_from_slice(data);
        })?;

        let bell = Rc::new(std::cell::Cell::new(false));
        let bell_sink = bell.clone();
        term.on_bell(move |_term| {
            bell_sink.set(true);
        })?;

        Ok(Self {
            term,
            render_state: RenderState::new()?,
            rows_buf: RowIterator::new()?,
            cells_buf: CellIterator::new()?,
            encoder: Encoder::new()?,
            key_event: Event::new()?,
            mouse_encoder: mouse::Encoder::new()?,
            mouse_event: mouse::Event::new()?,
            responses,
            bell,
            viewport_moved: false,
            bold_color: BoldColor::None,
            min_contrast: 1.0,
            placements: PlacementIterator::new()?,
            image_cache: HashMap::new(),
            image_ids_seen: Vec::new(),
            sel_anchor: None,
            sel_head: None,
            sel_rectangle: false,
            row_anchor: None,
            selection_installed: false,
            selection_dirty: false,
        })
    }
}

/// Expand the VT engine's stored pixel data to straight-alpha RGBA8.
///
/// Only four formats can actually arrive here: libghostty decompresses zlib and
/// decodes PNG at *load* time and rewrites the metadata accordingly
/// (`compression = none`, `format = rgba`), and it validates `len == w*h*bpp`
/// before storing. The length checks below are therefore belt-and-braces, and
/// `Png`/compressed data are rejected rather than trusted. Both enums are
/// `#[non_exhaustive]`, so the wildcard also future-proofs.
fn to_rgba(fmt: ImageFormat, comp: Compression, w: u32, h: u32, src: &[u8]) -> Option<Vec<u8>> {
    if comp != Compression::None {
        return None;
    }
    // A 10000x10000 image is 400 MB; `u32` intermediates would wrap.
    let px = (w as u64).checked_mul(h as u64)?;
    let out_len = usize::try_from(px.checked_mul(4)?).ok()?;
    let mut out = Vec::new();
    match fmt {
        ImageFormat::Rgba => {
            if src.len() as u64 != px * 4 {
                return None;
            }
            out.extend_from_slice(src);
        }
        ImageFormat::Rgb => {
            if src.len() as u64 != px * 3 {
                return None;
            }
            out.reserve_exact(out_len);
            for c in src.chunks_exact(3) {
                out.extend_from_slice(&[c[0], c[1], c[2], 0xff]);
            }
        }
        ImageFormat::Gray => {
            if src.len() as u64 != px {
                return None;
            }
            out.reserve_exact(out_len);
            for &g in src {
                out.extend_from_slice(&[g, g, g, 0xff]);
            }
        }
        ImageFormat::GrayAlpha => {
            if src.len() as u64 != px * 2 {
                return None;
            }
            out.reserve_exact(out_len);
            for c in src.chunks_exact(2) {
                out.extend_from_slice(&[c[0], c[0], c[0], c[1]]);
            }
        }
        // `Png` never reaches storage (it's decoded to RGBA on load), and the
        // enum is non-exhaustive.
        _ => return None,
    }
    Some(out)
}

/// Drop cached image pixels for ids that no longer appear in any placement.
///
/// Eviction by absence is the only strategy the VT engine's API supports: it
/// exposes no way to enumerate stored images and no delete notification.
fn evict_absent(cache: &mut HashMap<u32, Arc<ImageData>>, seen: &[u32]) {
    if cache.len() > seen.len() {
        cache.retain(|id, _| seen.contains(id));
    }
}

/// Rebuild `out` from the terminal's kitty image storage, copying any image
/// pixels not already in `cache`.
///
/// Takes `term: &Terminal` **deliberately**. `PlacementIterator::update` returns
/// an iteration whose lifetime is tied to the *iterator*, not to the `Graphics`
/// handle — so nothing in the type system stops a `vt_write` mid-walk from
/// invalidating every pointer the iteration holds, and `Image::data()` hands out
/// a slice straight into that storage. Holding a shared borrow of the terminal
/// across this whole body makes such a write a **compile error** instead of
/// undefined behaviour. Do not "simplify" this into a `&mut self` method.
fn walk_placements(
    term: &Terminal<'static, 'static>,
    iter: &mut PlacementIterator<'static>,
    cache: &mut HashMap<u32, Arc<ImageData>>,
    seen: &mut Vec<u32>,
    out: &mut Vec<ImagePlacement>,
) -> Result<()> {
    out.clear();
    seen.clear();
    // Fails when the protocol is disabled (a zero storage limit); that must
    // clear the placements rather than propagate.
    let Ok(graphics) = term.kitty_graphics() else {
        cache.clear();
        return Ok(());
    };

    // One pass with the default `All` layer: filtering per layer would lose the
    // intra-layer ordering, which comes from the (z, image_id) sort below.
    let mut it = iter.update(&graphics)?;
    while let Some(p) = it.next() {
        // Virtual (unicode-placeholder) placements can't be resolved through the
        // binding — `viewport_pos` returns `None` for them — so skip them.
        if p.is_virtual().unwrap_or(false) {
            continue;
        }
        let Ok(image_id) = p.image_id() else { continue };
        let Some(image) = graphics.image(image_id) else {
            continue;
        };
        let Ok(info) = p.placement_render_info(&image, term) else {
            continue;
        };
        // A zero-size placement would later mean a zero-size texture, which is a
        // wgpu validation error.
        if info.pixel_width == 0 || info.pixel_height == 0 {
            continue;
        }

        let (Ok(w), Ok(h)) = (image.width(), image.height()) else {
            continue;
        };
        let expected = (w as u64).saturating_mul(h as u64).saturating_mul(4);
        let data = match cache.get(&image_id) {
            // Cheap fingerprint: the binding exposes no transmit timestamp, so a
            // re-transmit at identical dimensions is indistinguishable.
            Some(d) if d.width == w && d.height == h && d.rgba.len() as u64 == expected => {
                d.clone()
            }
            _ => {
                let (Ok(fmt), Ok(comp), Ok(bytes)) =
                    (image.format(), image.compression(), image.data())
                else {
                    continue;
                };
                let Some(rgba) = bytes.and_then(|b| to_rgba(fmt, comp, w, h, b)) else {
                    continue;
                };
                let d = Arc::new(ImageData { width: w, height: h, rgba });
                cache.insert(image_id, d.clone());
                d
            }
        };

        seen.push(image_id);
        out.push(ImagePlacement {
            image_id,
            placement_id: p.placement_id().unwrap_or(0),
            data,
            col: info.viewport_col,
            row: info.viewport_row,
            grid_cols: info.grid_cols,
            grid_rows: info.grid_rows,
            x_offset: p.x_offset().unwrap_or(0),
            y_offset: p.y_offset().unwrap_or(0),
            dest_w: info.pixel_width,
            dest_h: info.pixel_height,
            src_x: info.source_x,
            src_y: info.source_y,
            src_w: info.source_width,
            src_h: info.source_height,
            z: p.z().unwrap_or(0),
            visible: info.viewport_visible,
        });
    }
    drop(it);
    // Ghostty's draw order: ascending z, ties broken by image id.
    out.sort_by_key(|p| (p.z, p.image_id));
    evict_absent(cache, seen);
    Ok(())
}

fn map_key(code: KeyCode) -> Key {
    use KeyCode::*;
    match code {
        // A configured `catch_all` never becomes a key *event*, so it can never
        // be encoded. Mapped to something inert rather than left to panic.
        CatchAll => Key::Unidentified,
        A => Key::A,
        B => Key::B,
        C => Key::C,
        D => Key::D,
        E => Key::E,
        F => Key::F,
        G => Key::G,
        H => Key::H,
        I => Key::I,
        J => Key::J,
        K => Key::K,
        L => Key::L,
        M => Key::M,
        N => Key::N,
        O => Key::O,
        P => Key::P,
        Q => Key::Q,
        R => Key::R,
        S => Key::S,
        T => Key::T,
        U => Key::U,
        V => Key::V,
        W => Key::W,
        X => Key::X,
        Y => Key::Y,
        Z => Key::Z,
        Digit0 => Key::Digit0,
        Digit1 => Key::Digit1,
        Digit2 => Key::Digit2,
        Digit3 => Key::Digit3,
        Digit4 => Key::Digit4,
        Digit5 => Key::Digit5,
        Digit6 => Key::Digit6,
        Digit7 => Key::Digit7,
        Digit8 => Key::Digit8,
        Digit9 => Key::Digit9,
        Enter => Key::Enter,
        Tab => Key::Tab,
        Backspace => Key::Backspace,
        Escape => Key::Escape,
        Space => Key::Space,
        Delete => Key::Delete,
        Insert => Key::Insert,
        Home => Key::Home,
        End => Key::End,
        PageUp => Key::PageUp,
        PageDown => Key::PageDown,
        ArrowUp => Key::ArrowUp,
        ArrowDown => Key::ArrowDown,
        ArrowLeft => Key::ArrowLeft,
        ArrowRight => Key::ArrowRight,
        F1 => Key::F1,
        F2 => Key::F2,
        F3 => Key::F3,
        F4 => Key::F4,
        F5 => Key::F5,
        F6 => Key::F6,
        F7 => Key::F7,
        F8 => Key::F8,
        F9 => Key::F9,
        F10 => Key::F10,
        F11 => Key::F11,
        F12 => Key::F12,
        Minus => Key::Minus,
        Equal => Key::Equal,
        BracketLeft => Key::BracketLeft,
        BracketRight => Key::BracketRight,
        Backslash => Key::Backslash,
        Semicolon => Key::Semicolon,
        Quote => Key::Quote,
        Backquote => Key::Backquote,
        Comma => Key::Comma,
        Period => Key::Period,
        Slash => Key::Slash,
    }
}

#[cfg(test)]
mod tests {
    use super::{Compression, GhosttyVtEngine, ImageFormat, to_rgba};
    use crate::engine::{
        GridSnapshot, KeyCode, KeyInput, KeyMods, MouseAction, MouseButton, MouseInput, Rgb,
        SelectKind, TerminalEngine, UnderlineStyle,
    };
    use std::collections::HashMap;
    use std::sync::Arc;

    #[test]
    fn apply_theme_sets_default_colors() {
        let mut eng = GhosttyVtEngine::new(20, 3, 100).unwrap();
        let pal = [Rgb::new(0, 0, 0); 256];
        eng.apply_theme(Rgb::new(200, 100, 50), Rgb::new(16, 18, 24), &pal)
            .unwrap();
        let mut s = GridSnapshot::default();
        eng.snapshot(&mut s).unwrap();
        assert_eq!(s.default_bg, Rgb::new(16, 18, 24));
        assert_eq!(s.default_fg, Rgb::new(200, 100, 50));

        // A shell clears/resets the screen on startup; the configured default
        // background must survive SGR reset + erase.
        eng.write(b"\x1b[0m\x1b[2J\x1b[Hhello");
        let mut s2 = GridSnapshot::default();
        eng.snapshot(&mut s2).unwrap();
        assert_eq!(
            s2.default_bg,
            Rgb::new(16, 18, 24),
            "default bg should survive reset/erase"
        );
    }

    fn snap(eng: &mut GhosttyVtEngine) -> GridSnapshot {
        let mut s = GridSnapshot::default();
        eng.snapshot(&mut s).unwrap();
        s
    }

    #[test]
    fn captures_text_styles_and_color() {
        let mut eng = GhosttyVtEngine::new(20, 3, 100).unwrap();
        // bold 'B', italic 'I', red 'R'.
        eng.write(b"\x1b[1mB\x1b[0m\x1b[3mI\x1b[0m\x1b[31mR\x1b[0m");
        let s = snap(&mut eng);

        let b = s.cell(0, 0).unwrap();
        assert_eq!(b.text, "B");
        assert!(b.bold, "expected bold cell");

        let i = s.cell(1, 0).unwrap();
        assert!(i.italic, "expected italic cell");

        let r = s.cell(2, 0).unwrap();
        assert_eq!(r.text, "R");
        // Default ANSI red has a dominant red channel.
        assert!(
            r.fg.r > r.fg.g && r.fg.r > r.fg.b,
            "expected reddish fg, got {:?}",
            r.fg
        );
    }

    #[test]
    fn captures_extended_text_attrs() {
        let mut eng = GhosttyVtEngine::new(20, 3, 100).unwrap();
        // faint F, double-underline D, curly-underline C, overline O, blink K,
        // invisible V — each style reset before the next cell.
        eng.write(
            b"\x1b[2mF\x1b[0m\
              \x1b[4:2mD\x1b[0m\
              \x1b[4:3mC\x1b[0m\
              \x1b[53mO\x1b[0m\
              \x1b[5mK\x1b[0m\
              \x1b[8mV\x1b[0m",
        );
        let s = snap(&mut eng);
        assert!(s.cell(0, 0).unwrap().faint, "expected faint cell");
        assert_eq!(s.cell(1, 0).unwrap().underline, UnderlineStyle::Double);
        assert_eq!(s.cell(2, 0).unwrap().underline, UnderlineStyle::Curly);
        assert!(s.cell(3, 0).unwrap().overline, "expected overline cell");
        assert!(s.cell(4, 0).unwrap().blink, "expected blink cell");
        assert!(s.has_blink, "snapshot.has_blink set when any cell blinks");
        assert!(s.cell(5, 0).unwrap().invisible, "expected invisible cell");
    }

    #[test]
    fn resolves_palette_underline_color() {
        let mut eng = GhosttyVtEngine::new(20, 3, 100).unwrap();
        // The render iterator resolves fg/bg but not the underline color, so the
        // engine resolves a palette-index underline color against its palette.
        let mut pal = [Rgb::new(0, 0, 0); 256];
        pal[5] = Rgb::new(200, 50, 25);
        eng.apply_theme(Rgb::new(200, 200, 200), Rgb::new(0, 0, 0), &pal)
            .unwrap();
        // Underline on; underline color = palette index 5 (SGR 58:5:5).
        eng.write(b"\x1b[4m\x1b[58:5:5mU");
        let c = snap(&mut eng).cell(0, 0).unwrap().clone();
        assert_eq!(c.underline, UnderlineStyle::Single);
        assert_eq!(c.underline_color, Some(Rgb::new(200, 50, 25)));
    }

    #[test]
    fn bold_is_bright_uses_bright_palette_variant() {
        use crate::engine::BoldColor;
        let mut eng = GhosttyVtEngine::new(20, 3, 100).unwrap();
        let mut pal = [Rgb::new(0, 0, 0); 256];
        pal[1] = Rgb::new(100, 0, 0); // ANSI red
        pal[9] = Rgb::new(200, 50, 25); // bright red
        eng.apply_theme(Rgb::new(200, 200, 200), Rgb::new(0, 0, 0), &pal)
            .unwrap();

        // Without the policy, bold red stays the plain ANSI red (palette 1).
        eng.write(b"\x1b[1;31mR\x1b[0m");
        assert_eq!(snap(&mut eng).cell(0, 0).unwrap().fg, Rgb::new(100, 0, 0));

        // With bold-is-bright, bold + ANSI red (idx 1) brightens to idx 9.
        eng.set_bold_color(BoldColor::Bright).unwrap();
        eng.write(b"\x1b[2J\x1b[H\x1b[1;31mR\x1b[0m");
        assert_eq!(snap(&mut eng).cell(0, 0).unwrap().fg, Rgb::new(200, 50, 25));
    }

    #[test]
    fn bold_color_sets_fixed_color_for_default_fg() {
        use crate::engine::BoldColor;
        let mut eng = GhosttyVtEngine::new(20, 3, 100).unwrap();
        let mut pal = [Rgb::new(0, 0, 0); 256];
        pal[1] = Rgb::new(100, 0, 0);
        pal[9] = Rgb::new(200, 50, 25);
        eng.apply_theme(Rgb::new(200, 200, 200), Rgb::new(0, 0, 0), &pal)
            .unwrap();
        eng.set_bold_color(BoldColor::Color(Rgb::new(10, 20, 30))).unwrap();

        // A bold default-foreground cell takes the fixed bold color.
        eng.write(b"\x1b[1mB\x1b[0m");
        assert_eq!(snap(&mut eng).cell(0, 0).unwrap().fg, Rgb::new(10, 20, 30));

        // A bold ANSI palette color is still brightened (not the fixed color).
        eng.write(b"\x1b[2J\x1b[H\x1b[1;31mR\x1b[0m");
        assert_eq!(snap(&mut eng).cell(0, 0).unwrap().fg, Rgb::new(200, 50, 25));
    }

    #[test]
    fn bold_is_bright_tracks_osc4_palette_redefinition() {
        use crate::engine::BoldColor;
        let mut eng = GhosttyVtEngine::new(20, 3, 100).unwrap();
        let mut pal = [Rgb::new(0, 0, 0); 256];
        pal[1] = Rgb::new(100, 0, 0);
        pal[9] = Rgb::new(200, 50, 25);
        eng.apply_theme(Rgb::new(200, 200, 200), Rgb::new(0, 0, 0), &pal)
            .unwrap();
        eng.set_bold_color(BoldColor::Bright).unwrap();
        // Redefine bright-red (index 9) to green via OSC 4; the bold-bright bump
        // must follow the live palette, not the static config copy.
        eng.write(b"\x1b]4;9;rgb:00/ff/00\x1b\\");
        eng.write(b"\x1b[1;31mR\x1b[0m");
        assert_eq!(snap(&mut eng).cell(0, 0).unwrap().fg, Rgb::new(0, 255, 0));
    }

    #[test]
    fn min_contrast_forced_glyph_drops_faint() {
        let mut eng = GhosttyVtEngine::new(20, 3, 100).unwrap();
        eng.apply_theme(Rgb::new(30, 30, 30), Rgb::new(0, 0, 0), &[Rgb::new(0, 0, 0); 256])
            .unwrap();
        eng.set_min_contrast(21.0).unwrap();
        // Faint + a low-contrast fg: min-contrast forces white, and faint is
        // dropped so the now-readable glyph renders opaque (matching Ghostty's
        // contrasted_color returning alpha 1.0).
        eng.write(b"\x1b[2mX"); // SGR 2 = faint
        let c = snap(&mut eng).cell(0, 0).unwrap().clone();
        assert_eq!(c.fg, Rgb::new(255, 255, 255));
        assert!(!c.faint, "faint is dropped when min-contrast forces a color");

        // A faint glyph that already passes contrast keeps its faint flag
        // (truecolor white on black easily clears the ratio).
        eng.write(b"\x1b[2J\x1b[H\x1b[2;38;2;255;255;255mX");
        let c2 = snap(&mut eng).cell(0, 0).unwrap().clone();
        assert!(c2.faint, "faint kept when the cell already meets contrast");
    }

    #[test]
    fn minimum_contrast_forces_readable_foreground() {
        let mut eng = GhosttyVtEngine::new(20, 3, 100).unwrap();
        // Near-black fg on black bg: far below any real contrast ratio.
        eng.apply_theme(Rgb::new(30, 30, 30), Rgb::new(0, 0, 0), &[Rgb::new(0, 0, 0); 256])
            .unwrap();

        // Off (ratio 1.0): the faint foreground is left untouched.
        eng.write(b"X");
        assert_eq!(snap(&mut eng).cell(0, 0).unwrap().fg, Rgb::new(30, 30, 30));

        // With a high minimum, fg is forced to white (it contrasts black best).
        eng.set_min_contrast(21.0).unwrap();
        eng.write(b"\x1b[2J\x1b[HX");
        assert_eq!(snap(&mut eng).cell(0, 0).unwrap().fg, Rgb::new(255, 255, 255));
    }

    /// An engine sized for kitty tests. `pixelSize` recovers the cell size by
    /// dividing `width_px / cols`, so an engine that was never resized reports a
    /// 0x0 cell and every `c=`/`r=` placement comes out zero-sized.
    fn kitty_engine() -> GhosttyVtEngine {
        let mut eng = GhosttyVtEngine::new(80, 24, 100).unwrap();
        eng.set_image_storage_limit(64 * 1024 * 1024).unwrap();
        eng.resize(80, 24, (10, 20)).unwrap();
        eng
    }

    /// `a=T` transmit+display, `t=d` direct, `f=24` RGB, 1x2 px (6 bytes), shown
    /// across 4x2 cells. The payload is base64 of six 0xFF bytes.
    const KITTY_RGB_1X2: &[u8] = b"\x1b_Ga=T,t=d,f=24,i=1,p=1,s=1,v=2,c=4,r=2;////////\x1b\\";

    #[test]
    fn rgb_expands_to_opaque_rgba() {
        let src = [1, 2, 3, 4, 5, 6];
        let got = to_rgba(ImageFormat::Rgb, Compression::None, 2, 1, &src).unwrap();
        assert_eq!(got, vec![1, 2, 3, 0xff, 4, 5, 6, 0xff]);
    }

    #[test]
    fn gray_and_gray_alpha_expand_to_rgba() {
        let got = to_rgba(ImageFormat::Gray, Compression::None, 2, 1, &[9, 200]).unwrap();
        assert_eq!(got, vec![9, 9, 9, 0xff, 200, 200, 200, 0xff]);
        let got = to_rgba(ImageFormat::GrayAlpha, Compression::None, 1, 1, &[9, 128]).unwrap();
        assert_eq!(got, vec![9, 9, 9, 128]);
    }

    #[test]
    fn rgba_is_copied_and_wrong_length_rejected() {
        let src = [1, 2, 3, 4];
        assert_eq!(
            to_rgba(ImageFormat::Rgba, Compression::None, 1, 1, &src).unwrap(),
            src.to_vec()
        );
        // One byte short for a 1x1 RGBA image.
        assert!(to_rgba(ImageFormat::Rgba, Compression::None, 1, 1, &src[..3]).is_none());
        // Dimensions that don't match the buffer at all.
        assert!(to_rgba(ImageFormat::Rgb, Compression::None, 100, 100, &src).is_none());
    }

    #[test]
    fn png_and_compressed_data_are_rejected() {
        // Both are resolved by the VT engine before storage, so reaching here
        // means something is wrong; refuse rather than misinterpret the bytes.
        assert!(to_rgba(ImageFormat::Png, Compression::None, 1, 1, &[0; 4]).is_none());
        assert!(
            to_rgba(ImageFormat::Rgba, Compression::ZlibDeflate, 1, 1, &[0; 4]).is_none()
        );
    }

    #[test]
    fn image_storage_limit_of_zero_disables_the_protocol() {
        // libghostty does NOT start at zero — the library default is a small
        // non-zero limit (10 MB), so kitty graphics are live before giest sets
        // anything. What `image-storage-limit` really controls is the budget,
        // and specifically that **zero turns the protocol off and wipes stored
        // images**, which is the behaviour worth pinning.
        let mut eng = GhosttyVtEngine::new(80, 24, 100).unwrap();
        eng.resize(80, 24, (10, 20)).unwrap();
        assert!(
            eng.term.kitty_image_storage_limit().unwrap() > 0,
            "libghostty starts with a non-zero default limit"
        );
        eng.write(KITTY_RGB_1X2);
        assert_eq!(snap(&mut eng).images.len(), 1);

        // Zero disables the protocol and deletes everything already stored.
        eng.set_image_storage_limit(0).unwrap();
        assert!(snap(&mut eng).images.is_empty(), "zero wipes stored images");
        eng.write(b"\x1b_Ga=T,t=d,f=24,i=2,p=1,s=1,v=2,c=4,r=2;////////\x1b\\");
        assert!(snap(&mut eng).images.is_empty(), "zero refuses new images");

        // …and raising it again accepts new transmissions.
        eng.set_image_storage_limit(64 * 1024 * 1024).unwrap();
        eng.write(b"\x1b_Ga=T,t=d,f=24,i=3,p=1,s=1,v=2,c=4,r=2;////////\x1b\\");
        assert_eq!(snap(&mut eng).images.len(), 1, "re-enabled by a new limit");
    }

    #[test]
    fn kitty_transmit_and_display_yields_a_placement() {
        let mut eng = kitty_engine();
        eng.write(KITTY_RGB_1X2);
        let s = snap(&mut eng);
        assert_eq!(s.images.len(), 1);
        let p = &s.images[0];
        assert_eq!((p.image_id, p.placement_id), (1, 1));
        assert_eq!((p.data.width, p.data.height), (1, 2));
        assert_eq!(p.data.rgba.len(), 1 * 2 * 4, "RGB expanded to RGBA");
        assert_eq!((p.grid_cols, p.grid_rows), (4, 2));
        // 4x2 cells at a 10x20 cell size.
        assert_eq!((p.dest_w, p.dest_h), (40, 40));
        assert_eq!((p.col, p.row), (0, 0));
        assert!(p.visible);
    }

    #[test]
    fn kitty_delete_clears_placements_without_a_dirty_frame() {
        // A delete touches only the image storage, whose dirty flag is not part
        // of the render state's. If placements were refreshed only on a dirty
        // frame, the deleted image would stay on screen forever.
        let mut eng = kitty_engine();
        eng.write(KITTY_RGB_1X2);
        assert_eq!(snap(&mut eng).images.len(), 1);
        // Take a second snapshot first, so the dirty-skip fast path is armed.
        assert_eq!(snap(&mut eng).images.len(), 1);

        eng.write(b"\x1b_Ga=d,d=I,i=1\x1b\\");
        assert!(snap(&mut eng).images.is_empty(), "delete must take effect");
    }

    #[test]
    fn kitty_png_is_decoded_by_the_installed_decoder() {
        // `f=100` is what icat and most tools send, and libghostty rejects it
        // outright unless a decoder is installed **on this thread**. This is the
        // only end-to-end proof that `png_decode::install` ran here — it would
        // fail immediately if the guard were a `Once` instead of thread-local,
        // since every test runs on its own thread.
        let mut eng = kitty_engine();
        eng.write(
            b"\x1b_Ga=T,f=100,i=7,p=1,q=1;\
              iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAYAAAAfFcSJAAAA\
              DUlEQVR4nGP4z8DwHwAFAAH/iZk9HQAAAABJRU5ErkJggg==\
              \x1b\\",
        );
        let s = snap(&mut eng);
        assert_eq!(s.images.len(), 1, "PNG rejected — decoder not installed?");
        let p = &s.images[0];
        assert_eq!((p.data.width, p.data.height), (1, 1));
        assert_eq!(p.data.rgba.len(), 4);
    }

    #[test]
    fn evict_absent_drops_unseen_ids() {
        use super::evict_absent;
        let mk = || Arc::new(crate::engine::ImageData { width: 1, height: 1, rgba: vec![0; 4] });
        let mut cache = HashMap::from([(1, mk()), (2, mk()), (3, mk())]);
        evict_absent(&mut cache, &[1, 3]);
        assert_eq!(cache.len(), 2);
        assert!(cache.contains_key(&1) && cache.contains_key(&3));
        // Nothing seen at all clears the cache — that's a deleted-everything frame.
        evict_absent(&mut cache, &[]);
        assert!(cache.is_empty());
    }

    #[test]
    fn kitty_image_pixels_are_shared_across_snapshots() {
        // The copy out of the VT engine's storage must happen once per image id,
        // not once per frame.
        let mut eng = kitty_engine();
        eng.write(KITTY_RGB_1X2);
        let a = snap(&mut eng).images[0].data.clone();
        let b = snap(&mut eng).images[0].data.clone();
        assert!(Arc::ptr_eq(&a, &b), "pixels re-copied every frame");
    }

    #[test]
    fn osc_dynamic_color_sets_apply_without_a_scanner() {
        // libghostty applies OSC 10/11/12 *sets* internally and the snapshot
        // reads the effective colors, so giest needs no scanner for them — only
        // for the `?` queries, which the vt library drops. This test is the
        // safety net for that claim.
        let mut eng = GhosttyVtEngine::new(20, 3, 100).unwrap();
        let fg = Rgb::new(0xc5, 0xc8, 0xc6);
        let bg = Rgb::new(0x10, 0x12, 0x18);
        eng.apply_theme(fg, bg, &[Rgb::new(0, 0, 0); 256]).unwrap();

        eng.write(b"\x1b]11;rgb:00/ff/00\x07");
        assert_eq!(snap(&mut eng).default_bg, Rgb::new(0, 255, 0));
        eng.write(b"\x1b]10;rgb:ff/00/00\x07");
        assert_eq!(snap(&mut eng).default_fg, Rgb::new(255, 0, 0));

        // OSC 110/111 reset to the configured defaults.
        eng.write(b"\x1b]111\x07\x1b]110\x07");
        let s = snap(&mut eng);
        assert_eq!(s.default_bg, bg);
        assert_eq!(s.default_fg, fg);
    }

    #[test]
    fn dynamic_colors_reports_effective_values() {
        let mut eng = GhosttyVtEngine::new(20, 3, 100).unwrap();
        eng.apply_theme(
            Rgb::new(0xc5, 0xc8, 0xc6),
            Rgb::new(0x10, 0x12, 0x18),
            &[Rgb::new(0, 0, 0); 256],
        )
        .unwrap();
        eng.set_cursor_color(None).unwrap();

        let (fg, bg, cursor) = eng.dynamic_colors();
        assert_eq!(fg, Rgb::new(0xc5, 0xc8, 0xc6));
        assert_eq!(bg, Rgb::new(0x10, 0x12, 0x18));
        assert_eq!(cursor, None, "no cursor color set");

        // An OSC 11 override is reflected without any snapshot in between.
        eng.write(b"\x1b]11;rgb:00/00/ff\x07");
        assert_eq!(eng.dynamic_colors().1, Rgb::new(0, 0, 255));

        eng.set_cursor_color(Some(Rgb::new(1, 2, 3))).unwrap();
        assert_eq!(eng.dynamic_colors().2, Some(Rgb::new(1, 2, 3)));
    }

    #[test]
    fn dynamic_colors_does_not_break_the_dirty_skip() {
        // `dynamic_colors` must not consume the terminal's dirty state — doing so
        // would make the following snapshot take the "nothing changed" fast path
        // and render an empty grid.
        let mut eng = GhosttyVtEngine::new(20, 3, 100).unwrap();
        eng.write(b"hello");
        let _ = eng.dynamic_colors();
        let s = snap(&mut eng);
        assert_eq!(s.cell(0, 0).unwrap().text.as_str(), "h");
        assert_eq!(s.cell(4, 0).unwrap().text.as_str(), "o");
    }

    #[test]
    fn default_background_cells_are_flagged() {
        let mut eng = GhosttyVtEngine::new(20, 3, 100).unwrap();
        let bg = Rgb::new(0x1d, 0x1f, 0x21);
        let mut pal = [Rgb::new(0, 0, 0); 256];
        pal[1] = Rgb::new(200, 0, 0); // ANSI red
        eng.apply_theme(Rgb::new(200, 200, 200), bg, &pal).unwrap();

        // 'a' default bg, 'b' on ANSI red, then back to default for ' ' and 'c'.
        eng.write(b"a\x1b[41mb\x1b[m c");
        let s = snap(&mut eng);

        assert!(!s.cell(0, 0).unwrap().bg_explicit, "default bg is not explicit");
        assert!(s.cell(1, 0).unwrap().bg_explicit, "SGR 41 sets an explicit bg");
        assert_eq!(s.cell(1, 0).unwrap().bg, pal[1]);
        assert!(!s.cell(2, 0).unwrap().bg_explicit, "SGR 0 resets to default bg");
        assert!(!s.cell(3, 0).unwrap().bg_explicit);
        // A cell the row iterator never yields is blanked, and a blank cell must
        // read as default-bg — otherwise it would paint an opaque quad and punch
        // a hole in a translucent background.
        assert!(!s.cell(19, 2).unwrap().bg_explicit, "untouched cells stay default");
    }

    #[test]
    fn inverse_cell_is_flagged_and_colors_swapped() {
        let mut eng = GhosttyVtEngine::new(20, 3, 100).unwrap();
        let fg = Rgb::new(200, 200, 200);
        let bg = Rgb::new(0x1d, 0x1f, 0x21);
        eng.apply_theme(fg, bg, &[Rgb::new(0, 0, 0); 256]).unwrap();

        eng.write(b"\x1b[7mX");
        let c = snap(&mut eng).cell(0, 0).unwrap().clone();
        assert!(c.inverse, "SGR 7 is carried through to the renderer");
        assert_eq!(c.fg, bg, "inverse swaps fg/bg");
        assert_eq!(c.bg, fg);
        // The swap does not invent an explicit background: the flag reports what
        // the *style* set, so `inverse` alone is what forces the cell opaque.
        assert!(!c.bg_explicit);
    }

    #[test]
    fn newline_advances_cursor_row() {
        let mut eng = GhosttyVtEngine::new(20, 3, 100).unwrap();
        eng.write(b"hi\r\n");
        let s = snap(&mut eng);
        assert_eq!(s.cell(0, 0).unwrap().text, "h");
        assert_eq!(s.cell(1, 0).unwrap().text, "i");
        assert_eq!(s.cursor_y, 1);
        assert_eq!(s.cursor_x, 0);
    }

    /// The selected text, trimmed — what a copy would put on the clipboard.
    fn sel_text(eng: &GhosttyVtEngine) -> Option<String> {
        eng.selected_text(true)
    }

    /// Inclusive column range marked selected on viewport row `y`, from a fresh
    /// snapshot — i.e. what the renderer would highlight.
    fn sel_span(eng: &mut GhosttyVtEngine, y: u16) -> Option<(u16, u16)> {
        let s = snap(eng);
        let cols = s.cols;
        let mut range: Option<(u16, u16)> = None;
        for x in 0..cols {
            if s.cell(x, y).is_some_and(|c| c.selected) {
                range = Some(match range {
                    None => (x, x),
                    Some((a, _)) => (a, x),
                });
            }
        }
        range
    }

    #[test]
    fn select_word_uses_the_terminals_own_boundaries() {
        let mut eng = GhosttyVtEngine::new(20, 3, 100).unwrap();
        eng.write(b"ls /usr/bin foo");
        let mut word = |x: u16| {
            assert!(eng.select_semantic(SelectKind::Word, x, 0, &[]));
            sel_text(&eng)
        };
        assert_eq!(word(1).as_deref(), Some("ls"));
        // A path selects whole — slashes are not boundaries by default, which is
        // what makes double-clicking a path useful.
        assert_eq!(word(6).as_deref(), Some("/usr/bin"));
        assert_eq!(word(13).as_deref(), Some("foo"));
        // And the highlight the renderer paints matches the text.
        assert_eq!(sel_span(&mut eng, 0), Some((12, 14)));
    }

    #[test]
    fn selection_word_chars_changes_where_a_word_ends() {
        // The discriminator that the config actually threads through to the
        // engine: with `/` as a boundary, the path splits into its components.
        let mut eng = GhosttyVtEngine::new(20, 3, 100).unwrap();
        eng.write(b"ls /usr/bin foo");
        let boundaries = ['\0', ' ', '/'];
        assert!(eng.select_semantic(SelectKind::Word, 6, 0, &boundaries));
        assert_eq!(
            sel_text(&eng).as_deref(),
            Some("usr"),
            "bounded by the slashes"
        );
        // …and the same click with the defaults keeps the whole path.
        assert!(eng.select_semantic(SelectKind::Word, 6, 0, &[]));
        assert_eq!(sel_text(&eng).as_deref(), Some("/usr/bin"));
    }

    #[test]
    fn select_line_unwraps_a_soft_wrapped_row() {
        // Text longer than the grid wraps, and a triple-click has to take the
        // whole logical line — as **one** line, not as the rows it was displayed
        // on. That unwrapping is what the engine-side read buys.
        let mut eng = GhosttyVtEngine::new(10, 4, 100).unwrap();
        eng.write(b"abcdefghijKLMNO");
        assert!(eng.select_semantic(SelectKind::Line, 2, 0, &[]));
        assert_eq!(sel_text(&eng).as_deref(), Some("abcdefghijKLMNO"));
        // It is highlighted across both display rows.
        assert_eq!(sel_span(&mut eng, 0), Some((0, 9)));
        assert_eq!(sel_span(&mut eng, 1), Some((0, 4)));
    }

    #[test]
    fn select_output_covers_the_commands_output_not_its_prompt() {
        // OSC 133: A = prompt start, B = command start, C = output start,
        // D = command end. Driven as real escape sequences, like the kitty tests.
        let mut eng = GhosttyVtEngine::new(20, 6, 100).unwrap();
        eng.write(b"\x1b]133;A\x07$ ");
        eng.write(b"\x1b]133;B\x07echo hi\r\n");
        eng.write(b"\x1b]133;C\x07out1\r\nout2\r\n");
        eng.write(b"\x1b]133;D;0\x07");
        eng.write(b"\x1b]133;A\x07$ ");
        // Click on the first output row.
        assert!(eng.select_semantic(SelectKind::Output, 0, 1, &[]));
        assert_eq!(sel_text(&eng).as_deref(), Some("out1\nout2"));
        // Clicking the prompt row itself is not output — and a gesture that
        // finds nothing leaves the existing selection alone.
        assert!(!eng.select_semantic(SelectKind::Output, 0, 0, &[]));
        assert_eq!(sel_text(&eng).as_deref(), Some("out1\nout2"));
    }

    #[test]
    fn a_selection_starting_above_the_viewport_is_kept_whole() {
        // The old viewport-scoped model had to **clamp** a selection that began
        // off the top of the screen, so copy only ever saw the visible part.
        // Tracked references have no such limit: the output selected here starts
        // three rows above a two-row viewport and still copies in full.
        let mut eng = GhosttyVtEngine::new(20, 2, 100).unwrap();
        eng.write(b"\x1b]133;A\x07$ \x1b]133;B\x07cmd\r\n");
        eng.write(b"\x1b]133;C\x07a\r\nb\r\nc\r\n");
        assert!(eng.select_semantic(SelectKind::Output, 0, 0, &[]));
        assert_eq!(sel_text(&eng).as_deref(), Some("a\nb\nc"));
    }

    #[test]
    fn scrollback_line_cap_bounds_history() {
        // `scrollback-limit-lines` rounds up to whole pages, so assert a bound
        // well under the uncapped count rather than the exact figure.
        let lines = |cap| {
            let mut eng = GhosttyVtEngine::new(20, 5, 10_000_000).unwrap();
            eng.set_scrollback_lines(cap).unwrap();
            for i in 0..4_000 {
                eng.write(format!("{i}\r\n").as_bytes());
            }
            eng.scrollback_rows()
        };
        let (capped, uncapped) = (lines(Some(50)), lines(None));
        assert!(uncapped >= 3_900, "uncapped kept {uncapped}");
        assert!(capped < uncapped / 2, "capped kept {capped} of {uncapped}");
    }

    #[test]
    fn a_drag_selection_survives_scrolling_and_scrollback() {
        // The core of the migration: the anchor is a tracked reference, so a
        // selection made at the bottom of the screen still reads correctly after
        // enough output to push it into scrollback. Viewport (col,row) pairs
        // silently addressed *different* cells after a scroll.
        let mut eng = GhosttyVtEngine::new(20, 3, 100).unwrap();
        eng.write(b"alpha\r\n");
        eng.selection_begin(0, 0, false);
        eng.selection_update(4, 0, false);
        assert_eq!(sel_text(&eng).as_deref(), Some("alpha"));

        for _ in 0..10 {
            eng.write(b"filler\r\n");
        }
        assert_eq!(
            sel_text(&eng).as_deref(),
            Some("alpha"),
            "the tracked anchor followed its row into scrollback"
        );
    }

    #[test]
    fn a_selection_survives_a_resize_that_reflows_the_text() {
        // Reflow is where a tracked reference differs most from a coordinate: a
        // narrower grid rewraps rows, so every cell moves. The old model had no
        // answer at all — `fit_grid` simply left the anchor pointing at whatever
        // was now at that (col,row).
        let mut eng = GhosttyVtEngine::new(20, 4, 100).unwrap();
        eng.write(b"hello wonderful world");
        eng.select_semantic(SelectKind::Word, 6, 0, &[]);
        assert_eq!(sel_text(&eng).as_deref(), Some("wonderful"));

        eng.resize(10, 6, (8, 16)).unwrap();
        assert_eq!(
            sel_text(&eng).as_deref(),
            Some("wonderful"),
            "the selection followed its cells through the rewrap"
        );
    }

    #[test]
    fn select_all_spans_scrollback_not_just_the_viewport() {
        // The old `select_all` was `(0,0)..(cols-1, rows-1)` — the viewport and
        // nothing else. This is the behaviour change.
        let mut eng = GhosttyVtEngine::new(20, 2, 100).unwrap();
        eng.write(b"one\r\ntwo\r\nthree\r\n");
        assert!(eng.select_all());
        let text = sel_text(&eng).expect("a selection");
        assert!(text.contains("one"), "scrolled-off rows are included: {text:?}");
        assert!(text.contains("three"), "as is the live row: {text:?}");
    }

    #[test]
    fn trailing_space_trim_is_configurable() {
        // Ghostty `clipboard-trim-trailing-spaces`: off, the selection carries
        // the grid's own padding, which is what you want for column-aligned
        // output and noise the rest of the time.
        // The spaces are *written*, not just unwritten cells: the formatter emits
        // only as far as a row was actually filled, so trailing blanks have to
        // exist for there to be anything to trim.
        let mut eng = GhosttyVtEngine::new(6, 2, 100).unwrap();
        eng.write(b"hi    ");
        eng.selection_begin(0, 0, false);
        eng.selection_update(5, 0, false);
        assert_eq!(eng.selected_text(true).as_deref(), Some("hi"));
        assert_eq!(eng.selected_text(false).as_deref(), Some("hi    "));
    }

    #[test]
    fn adjust_selection_moves_the_free_end_and_leaves_the_anchor() {
        use crate::engine::SelectionAdjust as A;

        let mut eng = GhosttyVtEngine::new(20, 3, 100).unwrap();
        eng.write(b"hello world");
        eng.selection_begin(0, 0, false);
        eng.selection_update(4, 0, false); // "hello"
        assert_eq!(sel_text(&eng).as_deref(), Some("hello"));

        // Read untrimmed here: trimming would hide the space the selection picks
        // up as it crosses the gap, which is the thing being measured.
        let raw = |e: &GhosttyVtEngine| e.selected_text(false);

        // Right extends; the anchor stays put.
        assert!(eng.selection_adjust(A::Right).is_some());
        assert_eq!(raw(&eng).as_deref(), Some("hello "));
        assert!(eng.selection_adjust(A::Right).is_some());
        assert_eq!(raw(&eng).as_deref(), Some("hello w"));
        // Left takes it back.
        assert!(eng.selection_adjust(A::Left).is_some());
        assert_eq!(raw(&eng).as_deref(), Some("hello "));
        // End of line reaches the last cell of the row.
        assert!(eng.selection_adjust(A::EndOfLine).is_some());
        assert_eq!(sel_text(&eng).as_deref(), Some("hello world"));
    }

    #[test]
    fn a_rectangle_drag_selects_a_block_not_a_run_of_text() {
        // The discriminator: over three rows of text, a linear selection from
        // (2,0) to (4,2) takes everything in between, while a block takes only
        // columns 2..4 of each row.
        let mut eng = GhosttyVtEngine::new(8, 3, 100).unwrap();
        eng.write(b"abcdefg\r\nhijklmn\r\nopqrstu");

        eng.selection_begin(2, 0, false);
        eng.selection_update(4, 2, false);
        assert_eq!(sel_text(&eng).as_deref(), Some("cdefg\nhijklmn\nopqrs"));

        eng.selection_begin(2, 0, true);
        eng.selection_update(4, 2, true);
        assert_eq!(sel_text(&eng).as_deref(), Some("cde\njkl\nqrs"));

        // The highlight the renderer paints agrees: three equal spans, not one
        // long run.
        assert_eq!(sel_span(&mut eng, 0), Some((2, 4)));
        assert_eq!(sel_span(&mut eng, 1), Some((2, 4)));
        assert_eq!(sel_span(&mut eng, 2), Some((2, 4)));
    }

    #[test]
    fn a_rectangle_survives_adjust_and_a_word_select_clears_it() {
        // `adjust_selection` rebuilds the selection, so the block flag has to be
        // remembered or a shift+arrow would silently turn a block back into a
        // run of text.
        let mut eng = GhosttyVtEngine::new(8, 3, 100).unwrap();
        eng.write(b"abcdefg\r\nhijklmn\r\nopqrstu");
        eng.selection_begin(2, 0, true);
        eng.selection_update(4, 2, true);
        assert!(eng.selection_adjust(crate::engine::SelectionAdjust::Right).is_some());
        let text = sel_text(&eng).expect("still selected");
        assert!(
            text.lines().count() == 3 && text.lines().all(|l| l.len() == 4),
            "still a block, one column wider: {text:?}"
        );

        // A semantic selection is a run of text, so the flag must reset.
        assert!(eng.select_semantic(SelectKind::Line, 0, 0, &[]));
        assert_eq!(sel_text(&eng).as_deref(), Some("abcdefg"));
    }

    #[test]
    fn adjust_selection_does_nothing_without_a_selection() {
        // Upstream returns "not performed" so the key falls through to the
        // shell; giest's `performable:` gate keys off exactly this `None`.
        let mut eng = GhosttyVtEngine::new(20, 3, 100).unwrap();
        eng.write(b"hello");
        assert_eq!(eng.selection_adjust(crate::engine::SelectionAdjust::Right), None);
        assert!(!eng.selection_active());
    }

    #[test]
    fn clearing_a_selection_leaves_no_text_and_no_highlight() {
        let mut eng = GhosttyVtEngine::new(10, 2, 100).unwrap();
        eng.write(b"hello");
        eng.selection_begin(0, 0, false);
        eng.selection_update(4, 0, false);
        assert!(eng.selection_active());
        assert_eq!(sel_span(&mut eng, 0), Some((0, 4)));

        eng.selection_clear();
        assert!(!eng.selection_active());
        assert_eq!(sel_text(&eng), None);
        assert_eq!(sel_span(&mut eng, 0), None);
    }

    #[test]
    fn an_update_without_an_anchor_selects_nothing() {
        // `update_selection` can arrive without a `begin` (a drag that started
        // outside the pane). It must not install a selection from a stale or
        // absent anchor.
        let mut eng = GhosttyVtEngine::new(10, 2, 100).unwrap();
        eng.write(b"hello");
        eng.selection_update(4, 0, false);
        assert!(!eng.selection_active());
        assert_eq!(sel_text(&eng), None);
    }

    #[test]
    fn screen_text_marks_soft_wrapped_rows_so_search_can_join_them() {
        // The flag search relies on, read off a real wrap rather than assumed:
        // 10 columns, 15 characters, so row 0 continues onto row 1.
        let mut eng = GhosttyVtEngine::new(10, 4, 100).unwrap();
        eng.write(b"hello wonderful");
        let rows = eng.screen_text();
        assert!(rows[0].wrapped, "row 0 continues onto row 1");
        assert!(!rows[1].wrapped, "row 1 does not");

        // And the join actually finds a query straddling the edge.
        let m = crate::search::search_rows(&rows, "wonderful", false);
        assert_eq!(m.len(), 1, "found across the wrap");
        assert_eq!((m[0].row, m[0].end_row), (0, 1), "spanning both rows");
    }

    #[test]
    fn a_wide_char_pushed_over_a_wrap_leaves_no_gap_in_the_text() {
        // When a wide character doesn't fit at the end of a row it moves to the
        // next one, leaving a `SpacerHead` blank behind. Emitting a space for it
        // would put one *inside* the wrapped word and a query spanning the wrap
        // would not match.
        let mut eng = GhosttyVtEngine::new(6, 3, 100).unwrap();
        // 5 narrow cells then a wide char: it cannot fit in the last column.
        eng.write("abcde世".as_bytes());
        let rows = eng.screen_text();
        let joined: String = rows
            .iter()
            .take(2)
            .flat_map(|r| r.chars.iter().copied())
            .collect();
        assert_eq!(joined, "abcde世", "no spacer leaked into the text");
        assert_eq!(
            crate::search::search_rows(&rows, "e世", false).len(),
            1,
            "and a query across the wrap matches"
        );
    }

    #[test]
    fn the_row_anchor_reports_where_a_row_moved_to_after_eviction() {
        // The drift fix. A 10-row scrollback plus a 2-row viewport holds 12 rows,
        // so writing past that evicts the oldest and renumbers the screen.
        let mut eng = GhosttyVtEngine::new(10, 2, 10).unwrap();
        for i in 0..12 {
            eng.write(format!("row{i}\r\n").as_bytes());
        }

        // Anchor a row with known text on it — that text is the oracle. Wherever
        // the anchor says the row is now, that row must still read the same.
        let find = |eng: &GhosttyVtEngine, want: &str| -> Option<u32> {
            eng.screen_text()
                .into_iter()
                .find(|r| r.chars.iter().collect::<String>() == want)
                .map(|r| r.row)
        };
        let captured = find(&eng, "row11").expect("the row we just wrote");
        eng.set_row_anchor(Some(captured));
        assert_eq!(eng.row_anchor_now(), Some(captured), "no drift yet");

        // Measured, not assumed: libghostty frees scrollback a **page** at a
        // time, so a few lines past the limit prune nothing and every row keeps
        // its number. Drift therefore arrives in jumps, and until one lands the
        // anchor keeps reporting the same row — which is correct, and is why the
        // correction is free in the common case.
        for i in 12..200 {
            eng.write(format!("row{i}\r\n").as_bytes());
        }
        let now = eng.row_anchor_now().expect("nothing pruned yet");
        assert_eq!(
            find(&eng, "row11"),
            Some(now),
            "the anchor names the row that line is actually on"
        );
        assert_eq!(crate::search::row_shift(captured, now), 0, "no drift yet");

        // Clearing the anchor stops the tracking.
        eng.set_row_anchor(None);
        assert_eq!(eng.row_anchor_now(), None);
    }

    #[test]
    #[ignore = "writes ~12k lines to force a scrollback prune; ~8s"]
    fn a_pruned_row_anchor_reports_no_value_rather_than_a_stale_row() {
        // The case that would otherwise silently mis-highlight: upstream moves a
        // destroyed pin to the screen's **top-left**, so reading its point
        // without checking `has_value` comes back as a confident "row 0" — a
        // zero drift correction at exactly the moment the correction is needed.
        //
        // Ignored only because forcing a real prune is expensive: libghostty
        // frees scrollback a page at a time, and `max_scrollback = 10` still
        // retained ~7k rows at 20k lines written (measured, not assumed).
        let mut eng = GhosttyVtEngine::new(10, 2, 10).unwrap();
        for i in 0..12 {
            eng.write(format!("row{i}\r\n").as_bytes());
        }
        eng.set_row_anchor(Some(11));
        assert_eq!(eng.row_anchor_now(), Some(11));

        for i in 12..12_000 {
            eng.write(format!("row{i}\r\n").as_bytes());
        }
        assert_eq!(eng.row_anchor_now(), None, "the anchored row was pruned");
    }

    #[test]
    fn screen_text_reads_rows_with_column_mapping() {
        let mut eng = GhosttyVtEngine::new(20, 3, 100).unwrap();
        eng.write(b"hello\r\nworld\r\n");
        let rows = eng.screen_text();
        assert_eq!(rows.len(), 3, "scrollback(0) + 3 viewport rows");
        assert_eq!(rows[0].chars.iter().collect::<String>(), "hello");
        assert_eq!(rows[0].cols, vec![0, 1, 2, 3, 4], "each char maps to its column");
        assert_eq!(rows[0].row, 0);
        assert_eq!(rows[1].chars.iter().collect::<String>(), "world");
        // The empty cursor row trims to nothing.
        assert!(rows[2].chars.is_empty());
    }

    #[test]
    fn screen_text_skips_wide_char_tail_cells() {
        // Wide (CJK) chars occupy two columns; their empty spacer-tail cell must
        // be skipped so the codepoints stay adjacent (searchable), with columns
        // reflecting physical placement (世 at col 0, 界 at col 2, x at col 4).
        let mut eng = GhosttyVtEngine::new(20, 2, 100).unwrap();
        eng.write("世界x".as_bytes());
        let rows = eng.screen_text();
        assert_eq!(rows[0].chars.iter().collect::<String>(), "世界x");
        assert_eq!(rows[0].cols, vec![0, 2, 4]);
    }

    #[test]
    fn osc_sets_window_title() {
        let mut eng = GhosttyVtEngine::new(20, 3, 100).unwrap();
        eng.write(b"\x1b]2;hello\x07");
        assert_eq!(eng.title().as_deref(), Some("hello"));
    }

    #[test]
    fn jump_to_prompt_finds_marked_prompts() {
        // Without OSC 133 marks there is nothing to jump to, even with scrollback.
        let mut eng = GhosttyVtEngine::new(20, 4, 100).unwrap();
        eng.write(b"plain\r\noutput\r\nlines\r\nhere\r\nmore\r\n");
        assert_eq!(eng.jump_to_prompt(-1), None);

        // Three OSC 133 A prompt marks separated by output; with rows = 4 the
        // earliest prompts spill into scrollback above the live viewport.
        let mut eng = GhosttyVtEngine::new(20, 4, 100).unwrap();
        let mark = b"\x1b]133;A\x1b\\";
        let mut data = Vec::new();
        for body in [&b"p1\r\na\r\nb\r\n"[..], &b"p2\r\nc\r\nd\r\n"[..], &b"p3"[..]] {
            data.extend_from_slice(mark);
            data.extend_from_slice(body);
        }
        eng.write(&data);
        // From the live bottom, a previous prompt sits above the viewport top.
        let up = eng.jump_to_prompt(-1);
        assert!(up.is_some_and(|n| n >= 1), "expected a prompt above, got {up:?}");
    }

    #[test]
    fn bell_sets_one_shot_flag() {
        let mut eng = GhosttyVtEngine::new(20, 3, 100).unwrap();
        assert!(!eng.take_bell(), "no bell yet");
        eng.write(b"a\x07b");
        assert!(eng.take_bell(), "BEL (0x07) should set the bell flag");
        assert!(!eng.take_bell(), "the flag is one-shot and clears on read");
    }

    #[test]
    fn reads_osc8_hyperlink_uri() {
        let mut eng = GhosttyVtEngine::new(20, 3, 100).unwrap();
        // OSC 8 hyperlink: start (empty params, URI), display text "LINK", end.
        // ST is ESC '\'. The link covers the four cells of "LINK".
        eng.write(b"\x1b]8;;https://example.com\x1b\\LINK\x1b]8;;\x1b\\");
        assert_eq!(
            eng.hyperlink_at(0, 0).as_deref(),
            Some("https://example.com")
        );
        assert_eq!(
            eng.hyperlink_at(3, 0).as_deref(),
            Some("https://example.com")
        );
        // A cell past the link text carries no hyperlink.
        assert_eq!(eng.hyperlink_at(10, 0), None);
    }

    /// giest reconstructs the scrollbar's `{total, offset, len}` from
    /// `scrollback_rows()` + the viewport height instead of calling the
    /// binding's `Terminal::scrollbar()` (expensive at arbitrary pins, and its
    /// integer `offset` can't carry a sub-line position). That's only valid if
    /// the two agree — so compare them directly, and catch it here rather than
    /// as a subtly skewed thumb if upstream ever changes the arithmetic.
    #[test]
    fn scrollbar_state_matches_the_reconstruction_from_scrollback_rows() {
        const ROWS: u16 = 24;
        let mut eng = GhosttyVtEngine::new(80, ROWS, 1000).unwrap();
        for i in 0..200 {
            eng.write(format!("line {i}\r\n").as_bytes());
        }
        let scrollback = eng.scrollback_rows();
        assert!(scrollback > 0, "200 lines into a 24-row grid must scroll off");

        let bar = eng.term.scrollbar().unwrap();
        assert_eq!(bar.len as usize, ROWS as usize, "len is the viewport height");
        assert_eq!(
            bar.total as usize,
            scrollback + ROWS as usize,
            "total is scrollback plus the viewport"
        );
        // Parked at the live bottom, so the viewport starts at the last page.
        assert_eq!(bar.offset as usize, scrollback);

        // And it tracks the pin: scrolling up 10 lines moves `offset` by 10.
        eng.scroll(-10);
        assert_eq!(eng.term.scrollbar().unwrap().offset as usize, scrollback - 10);
    }

    #[test]
    fn enter_key_encodes_carriage_return() {
        let mut eng = GhosttyVtEngine::new(20, 3, 100).unwrap();
        let bytes = eng.encode_key(&KeyInput {
            code: KeyCode::Enter,
            text: None,
            mods: KeyMods::default(),
            press: true,
        });
        assert_eq!(bytes, b"\r");
    }

    #[test]
    fn paste_normalizes_newlines_when_not_bracketed() {
        let mut eng = GhosttyVtEngine::new(20, 3, 100).unwrap();
        // Default: bracketed paste off → newlines become carriage returns.
        assert_eq!(eng.encode_paste("a\nb"), b"a\rb");
    }

    /// Paste protection needs the mode *before* encoding, so it reads it
    /// through its own accessor rather than inferring it from the output.
    #[test]
    fn bracketed_paste_mode_is_reported() {
        let mut eng = GhosttyVtEngine::new(20, 3, 100).unwrap();
        assert!(!eng.bracketed_paste(), "off until the program asks");
        eng.write(b"\x1b[?2004h");
        assert!(eng.bracketed_paste(), "DECSET 2004 enables it");
        eng.write(b"\x1b[?2004l");
        assert!(!eng.bracketed_paste(), "DECRST 2004 disables it");
    }

    #[test]
    fn paste_wraps_in_brackets_when_enabled() {
        let mut eng = GhosttyVtEngine::new(20, 3, 100).unwrap();
        // Enable bracketed paste (DEC mode 2004) the way an app would.
        eng.write(b"\x1b[?2004h");
        let out = eng.encode_paste("hi");
        assert_eq!(out, b"\x1b[200~hi\x1b[201~");
    }

    #[test]
    fn mouse_left_press_sgr() {
        let mut eng = GhosttyVtEngine::new(80, 24, 100).unwrap();
        // Enable button tracking (1000) + SGR extended mode (1006).
        eng.write(b"\x1b[?1000h\x1b[?1006h");
        assert!(eng.is_mouse_tracking());
        let out = eng.encode_mouse(&MouseInput {
            action: MouseAction::Press,
            button: Some(MouseButton::Left),
            pos_px: (25, 30),
            cell_px: (10, 20),
            screen_px: (800, 480),
            mods: KeyMods::default(),
        });
        // Cell (col 2, row 1) 0-based → SGR 1-based col 3, row 2; button 0; press 'M'.
        assert_eq!(out, b"\x1b[<0;3;2M");
    }

    #[test]
    fn ctrl_c_encodes_etx() {
        let mut eng = GhosttyVtEngine::new(20, 3, 100).unwrap();
        let bytes = eng.encode_key(&KeyInput {
            code: KeyCode::C,
            text: None,
            mods: KeyMods {
                ctrl: true,
                ..Default::default()
            },
            press: true,
        });
        assert_eq!(bytes, b"\x03");
    }

    // --- Encode integration: verify giest drives libghostty's encoder with the
    // live terminal modes (these assert standard xterm sequences, not a
    // re-derivation of the protocol — that's libghostty's own test surface). ---

    fn key(eng: &mut GhosttyVtEngine, code: KeyCode, mods: KeyMods) -> Vec<u8> {
        eng.encode_key(&KeyInput {
            code,
            text: None,
            mods,
            press: true,
        })
    }

    #[test]
    fn arrows_normal_mode() {
        let mut eng = GhosttyVtEngine::new(80, 24, 100).unwrap();
        let none = KeyMods::default();
        assert_eq!(key(&mut eng, KeyCode::ArrowUp, none), b"\x1b[A");
        assert_eq!(key(&mut eng, KeyCode::ArrowDown, none), b"\x1b[B");
        assert_eq!(key(&mut eng, KeyCode::ArrowRight, none), b"\x1b[C");
        assert_eq!(key(&mut eng, KeyCode::ArrowLeft, none), b"\x1b[D");
    }

    #[test]
    fn arrows_application_cursor_mode() {
        let mut eng = GhosttyVtEngine::new(80, 24, 100).unwrap();
        // DECCKM (application cursor keys) switches CSI to SS3 form.
        eng.write(b"\x1b[?1h");
        assert_eq!(
            key(&mut eng, KeyCode::ArrowUp, KeyMods::default()),
            b"\x1bOA"
        );
        assert_eq!(
            key(&mut eng, KeyCode::ArrowRight, KeyMods::default()),
            b"\x1bOC"
        );
    }

    #[test]
    fn modified_arrows_emit_csi_modifier() {
        let mut eng = GhosttyVtEngine::new(80, 24, 100).unwrap();
        let shift = KeyMods {
            shift: true,
            ..Default::default()
        };
        let ctrl = KeyMods {
            ctrl: true,
            ..Default::default()
        };
        let alt = KeyMods {
            alt: true,
            ..Default::default()
        };
        // xterm modifier params: shift=2, alt=3, ctrl=5 (param-1 bitmask + 1).
        assert_eq!(key(&mut eng, KeyCode::ArrowRight, shift), b"\x1b[1;2C");
        assert_eq!(key(&mut eng, KeyCode::ArrowRight, alt), b"\x1b[1;3C");
        assert_eq!(key(&mut eng, KeyCode::ArrowRight, ctrl), b"\x1b[1;5C");
    }

    #[test]
    fn mouse_release_and_large_position_sgr() {
        let mut eng = GhosttyVtEngine::new(200, 60, 100).unwrap();
        eng.write(b"\x1b[?1000h\x1b[?1006h");
        let out = eng.encode_mouse(&MouseInput {
            action: MouseAction::Release,
            button: Some(MouseButton::Left),
            // 1005/10 = col 100 → 1-based 101; 1190/20 = row 59 → 1-based 60.
            pos_px: (1005, 1190),
            cell_px: (10, 20),
            screen_px: (2000, 1200),
            mods: KeyMods::default(),
        });
        // Release in SGR mode terminates with 'm' (press would be 'M').
        assert_eq!(out, b"\x1b[<0;101;60m");
    }
}

fn map_mouse_button(b: MouseButton) -> mouse::Button {
    match b {
        MouseButton::Left => mouse::Button::Left,
        MouseButton::Middle => mouse::Button::Middle,
        MouseButton::Right => mouse::Button::Right,
        MouseButton::WheelUp => mouse::Button::Four,
        MouseButton::WheelDown => mouse::Button::Five,
    }
}

fn map_mouse_action(a: MouseAction) -> mouse::Action {
    match a {
        MouseAction::Press => mouse::Action::Press,
        MouseAction::Release => mouse::Action::Release,
        MouseAction::Motion => mouse::Action::Motion,
    }
}

fn map_mods(m: super::KeyMods) -> Mods {
    let mut out = Mods::empty();
    if m.shift {
        out |= Mods::SHIFT;
    }
    if m.ctrl {
        out |= Mods::CTRL;
    }
    if m.alt {
        out |= Mods::ALT;
    }
    if m.sup {
        out |= Mods::SUPER;
    }
    out
}

fn rgb(c: libghostty_vt::style::RgbColor) -> Rgb {
    Rgb::new(c.r, c.g, c.b)
}

/// Map libghostty's underline style to the neutral [`UnderlineStyle`]. The
/// binding enum is `#[non_exhaustive]`, so any future variant degrades to a
/// plain single underline rather than failing to compile.
fn map_underline(u: Underline) -> UnderlineStyle {
    match u {
        Underline::None => UnderlineStyle::None,
        Underline::Single => UnderlineStyle::Single,
        Underline::Double => UnderlineStyle::Double,
        Underline::Curly => UnderlineStyle::Curly,
        Underline::Dotted => UnderlineStyle::Dotted,
        Underline::Dashed => UnderlineStyle::Dashed,
        _ => UnderlineStyle::Single,
    }
}

/// Resolve a style color (none / palette index / direct RGB) to concrete RGB
/// against `palette`. Used for the underline color, which the render iterator
/// (unlike `fg_color`/`bg_color`) does not pre-resolve. `None` means unset.
fn resolve_color(c: StyleColor, palette: &[Rgb; 256]) -> Option<Rgb> {
    match c {
        StyleColor::None => None,
        StyleColor::Rgb(c) => Some(rgb(c)),
        StyleColor::Palette(idx) => palette.get(idx.0 as usize).copied(),
    }
}

/// Adjust an already-resolved bold foreground for the bold-color policy, mirroring
/// Ghostty's `Style.fg` (see `terminal/style.zig`). Only called for bold cells
/// when a policy is active. `resolved` is the cell's normal foreground (from the
/// render iterator, which honors the terminal's live palette); `raw` is the cell's
/// unflattened style color, needed to tell an explicit ANSI palette index from a
/// default/RGB foreground:
/// - an ANSI palette color (0–7) brightens to its 8–15 variant (under *either*
///   policy — `bright` or a fixed color);
/// - a `none`/default or default-valued RGB foreground takes the fixed color
///   (and is left untouched under `bright`).
///
/// `inverse` is **not** applied here — the caller swaps fg/bg afterward, matching
/// where Ghostty applies it.
fn apply_bold_color(
    raw: StyleColor,
    resolved: Rgb,
    default_fg: Rgb,
    palette: &[Rgb; 256],
    policy: BoldColor,
) -> Rgb {
    match raw {
        StyleColor::Palette(idx) => {
            let i = idx.0;
            if i < 8 {
                return palette.get((i + 8) as usize).copied().unwrap_or(resolved);
            }
            resolved
        }
        StyleColor::None => match policy {
            BoldColor::Color(c) => c,
            _ => resolved,
        },
        StyleColor::Rgb(_) => match policy {
            BoldColor::Color(c) if resolved == default_fg => c,
            _ => resolved,
        },
    }
}

/// Linearize one sRGB channel (0–255) to linear light in `0.0..=1.0`, matching
/// the shader's `linearize` (the WCAG transfer function Ghostty uses for
/// contrast). Required so the contrast ratio is computed in the same space.
fn srgb_to_linear(c: u8) -> f32 {
    let v = c as f32 / 255.0;
    if v <= 0.04045 {
        v / 12.92
    } else {
        ((v + 0.055) / 1.055).powf(2.4)
    }
}

/// Relative luminance of an sRGB color, per WCAG (linearized channels weighted
/// 0.2126/0.7152/0.0722). Mirrors the renderer's `luminance`.
pub fn luminance(c: Rgb) -> f32 {
    0.2126 * srgb_to_linear(c.r) + 0.7152 * srgb_to_linear(c.g) + 0.0722 * srgb_to_linear(c.b)
}

/// WCAG contrast ratio between two colors (`1.0..=21.0`). Mirrors the renderer's
/// `contrast_ratio`.
///
/// Also the chrome's contrast floor (see [`crate::theme`]), so the UI and the
/// terminal's `minimum-contrast` agree on what "readable" means.
pub fn contrast_ratio(a: Rgb, b: Rgb) -> f32 {
    let la = luminance(a) + 0.05;
    let lb = luminance(b) + 0.05;
    la.max(lb) / la.min(lb)
}

/// If `fg` on `bg` fails the `min` contrast ratio, replace it with pure white or
/// black (whichever contrasts more), exactly like the shader's `contrasted_color`.
/// Otherwise `fg` is returned unchanged. `min <= 1.0` is a no-op.
fn enforce_contrast(fg: Rgb, bg: Rgb, min: f32) -> Rgb {
    if min <= 1.0 || contrast_ratio(fg, bg) >= min {
        return fg;
    }
    let white = Rgb::new(255, 255, 255);
    let black = Rgb::new(0, 0, 0);
    if contrast_ratio(white, bg) > contrast_ratio(black, bg) {
        white
    } else {
        black
    }
}

/// Whether minimum-contrast should be skipped for `text` because it is a
/// graphics glyph (box-drawing, block, legacy-computing, or Powerline), where
/// forcing pure black/white looks wrong. Mirrors Ghostty's `noMinContrast`
/// (`renderer/cell.zig`). Tested on the first scalar of the cell's grapheme.
fn is_graphics_element(text: &str) -> bool {
    let Some(ch) = text.chars().next() else {
        return false;
    };
    let c = ch as u32;
    matches!(c,
        0x2500..=0x257F   // box drawing
        | 0x2580..=0x259F // block elements
        | 0x1FB00..=0x1FBFF | 0x1CC00..=0x1CEBF // legacy computing (+ supplement)
        | 0xE0B0..=0xE0D7) // Powerline
}

/// Copy one libghostty render cell into a neutral [`Cell`], resolving colors
/// (applying `inverse`) against the given defaults and reusing `dst`'s inline
/// string buffer. Shared by the full-grid snapshot and the smooth-scroll
/// over-row read.
fn copy_cell(
    cell: &CellIteration<'_, '_>,
    default_fg: Rgb,
    default_bg: Rgb,
    palette: &[Rgb; 256],
    bold_color: BoldColor,
    min_contrast: f32,
    dst: &mut Cell,
) -> Result<()> {
    let style = cell.style()?;
    let mut fg = cell.fg_color()?.map(rgb).unwrap_or(default_fg);
    // The bold-color policy needs the *raw* style color (an explicit ANSI palette
    // index is lost once `fg_color()` flattens it to RGB).
    if style.bold && bold_color != BoldColor::None {
        fg = apply_bold_color(style.fg_color, fg, default_fg, palette, bold_color);
    }
    // Keep the *raw* option: once flattened to the default there is no way back,
    // and the renderer needs to know whether this background is the terminal
    // default (such a cell draws no background quad under `background-opacity`).
    let bg_raw = cell.bg_color()?;
    let mut bg = bg_raw.map(rgb).unwrap_or(default_bg);
    if style.inverse {
        std::mem::swap(&mut fg, &mut bg);
    }
    dst.text.clear();
    for ch in cell.graphemes()? {
        dst.text.push(ch);
    }
    // Minimum-contrast runs on the final (post-inverse) colors, skipping graphics
    // glyphs — matching where Ghostty's shader applies it.
    let mut faint = style.faint;
    if min_contrast > 1.0 && !is_graphics_element(&dst.text) {
        let forced = enforce_contrast(fg, bg, min_contrast);
        if forced != fg {
            // Ghostty's `contrasted_color` returns a fully opaque white/black,
            // discarding the premultiplied faint alpha. Drop faint so the forced
            // color renders at full strength — otherwise dimming a color that was
            // just forced for readability would defeat minimum-contrast.
            faint = false;
            fg = forced;
        }
    }
    dst.fg = fg;
    dst.bg = bg;
    dst.bold = style.bold;
    dst.italic = style.italic;
    dst.underline = map_underline(style.underline);
    dst.underline_color = resolve_color(style.underline_color, palette);
    dst.strikethrough = style.strikethrough;
    dst.overline = style.overline;
    dst.faint = faint;
    dst.blink = style.blink;
    dst.invisible = style.invisible;
    dst.bg_explicit = bg_raw.is_some();
    dst.inverse = style.inverse;
    Ok(())
}

impl TerminalEngine for GhosttyVtEngine {
    fn write(&mut self, bytes: &[u8]) {
        self.term.vt_write(bytes);
    }

    fn resize(&mut self, cols: u16, rows: u16, cell_px: (u32, u32)) -> Result<()> {
        self.term.resize(cols, rows, cell_px.0, cell_px.1)?;
        Ok(())
    }

    fn encode_key(&mut self, input: &KeyInput) -> Vec<u8> {
        self.key_event
            .set_action(if input.press {
                Action::Press
            } else {
                Action::Release
            })
            .set_key(map_key(input.code))
            .set_mods(map_mods(input.mods))
            .set_utf8(input.text.clone());

        // Pick up cursor-key/keypad/kitty/modifyOtherKeys modes from live state.
        self.encoder.set_options_from_terminal(&self.term);

        let mut out = Vec::with_capacity(16);
        if self
            .encoder
            .encode_to_vec(&self.key_event, &mut out)
            .is_err()
        {
            out.clear();
        }
        out
    }

    fn scroll(&mut self, delta: isize) {
        self.term.scroll_viewport(ScrollViewport::Delta(delta));
        self.viewport_moved = true;
    }

    fn scroll_to_bottom(&mut self) {
        self.term.scroll_viewport(ScrollViewport::Bottom);
        self.viewport_moved = true;
    }

    fn scroll_to_top(&mut self) {
        self.term.scroll_viewport(ScrollViewport::Top);
        self.viewport_moved = true;
    }

    fn scrollback_rows(&self) -> usize {
        self.term.scrollback_rows().unwrap_or(0)
    }

    fn snapshot_over_row(&mut self, out: &mut Vec<Cell>) -> Result<()> {
        // Reveal the line just above the viewport top by scrolling up one line,
        // read its cells, then restore the viewport. The net Delta is zero, so
        // the caller's pin is unchanged.
        self.term.scroll_viewport(ScrollViewport::Delta(-1));
        let bold_color = self.bold_color;
        let min_contrast = self.min_contrast;
        let read = (|| -> Result<()> {
            let snapshot = self.render_state.update(&self.term)?;
            let colors = snapshot.colors()?;
            let cols = snapshot.cols()? as usize;
            let default_fg = rgb(colors.foreground);
            let default_bg = rgb(colors.background);
            // Live palette (OSC-4-aware), as in `snapshot`.
            let palette = colors.palette.map(rgb);
            out.clear();
            out.resize(cols, Cell::default());
            let mut rows_iter = self.rows_buf.update(&snapshot)?;
            if let Some(row) = rows_iter.next() {
                let mut x = 0usize;
                let mut cells_iter = self.cells_buf.update(row)?;
                while let Some(cell) = cells_iter.next() {
                    if x >= cols {
                        break;
                    }
                    copy_cell(
                        cell,
                        default_fg,
                        default_bg,
                        &palette,
                        bold_color,
                        min_contrast,
                        &mut out[x],
                    )?;
                    x += 1;
                }
            }
            Ok(())
        })();
        // Restore the viewport regardless of read errors.
        self.term.scroll_viewport(ScrollViewport::Delta(1));
        read
    }

    fn apply_theme(&mut self, fg: Rgb, bg: Rgb, palette: &[Rgb; 256]) -> Result<()> {
        let to_c = |c: Rgb| libghostty_vt::style::RgbColor {
            r: c.r,
            g: c.g,
            b: c.b,
        };
        let mut pal = [libghostty_vt::style::RgbColor::default(); 256];
        for (dst, src) in pal.iter_mut().zip(palette.iter()) {
            *dst = to_c(*src);
        }
        self.term.set_default_fg_color(Some(to_c(fg)))?;
        self.term.set_default_bg_color(Some(to_c(bg)))?;
        self.term.set_default_color_palette(Some(libghostty_vt::style::Palette(pal)))?;
        Ok(())
    }

    fn set_cursor_color(&mut self, color: Option<Rgb>) -> Result<()> {
        let c = color.map(|c| libghostty_vt::style::RgbColor {
            r: c.r,
            g: c.g,
            b: c.b,
        });
        self.term.set_default_cursor_color(c)?;
        Ok(())
    }

    fn cursor_at_prompt(&self) -> Option<bool> {
        // The row-level semantic-prompt enum only has None/Prompt/Continuation —
        // there is no Command/Output variant — so "not a prompt row" is ambiguous
        // between "a command is running" and "this shell emits no OSC 133 marks".
        // Report `Some(false)` for it and let the session's latch disambiguate.
        let y = self.term.cursor_y().ok()?;
        let gr = self
            .term
            .grid_ref(Point::Viewport(PointCoordinate { x: 0, y: y as u32 }))
            .ok()?;
        let sp = gr.row().ok()?.semantic_prompt().ok()?;
        Some(matches!(
            sp,
            RowSemanticPrompt::Prompt | RowSemanticPrompt::Continuation
        ))
    }

    fn dynamic_colors(&self) -> (Rgb, Rgb, Option<Rgb>) {
        // Read straight off the terminal, NOT through `RenderState::update` (the
        // path `snapshot` uses). That call consumes the terminal's dirty state,
        // so routing this through it would make the next real snapshot see
        // `Dirty::Clean`, take the skip fast path, and freeze the grid.
        let fg = self.term.fg_color().ok().flatten().map(rgb);
        let bg = self.term.bg_color().ok().flatten().map(rgb);
        let cursor = self.term.cursor_color().ok().flatten().map(rgb);
        (
            fg.unwrap_or(Rgb::new(0xc5, 0xc8, 0xc6)),
            bg.unwrap_or(Rgb::new(0x10, 0x12, 0x18)),
            cursor,
        )
    }

    fn set_bold_color(&mut self, bold: BoldColor) -> Result<()> {
        self.bold_color = bold;
        Ok(())
    }

    fn set_image_storage_limit(&mut self, bytes: u64) -> Result<()> {
        self.term.set_kitty_image_storage_limit(bytes)?;
        // Refuse every non-direct transmission medium. `t=s` (shared memory) is
        // a hard `UnsupportedMedium` on Windows upstream, and `t=f`/`t=t` resolve
        // paths with posix `realpath`/`unlink` against a hardcoded `/tmp` and
        // `/dev/shm`. The binding already defaults these off; setting them
        // explicitly documents the divergence and survives a change to those
        // defaults.
        self.term.set_kitty_image_from_file_allowed(false)?;
        self.term.set_kitty_image_temp_file_dir(None)?;
        self.term.set_kitty_image_from_shared_mem_allowed(false)?;
        Ok(())
    }

    fn set_min_contrast(&mut self, ratio: f32) -> Result<()> {
        self.min_contrast = ratio;
        Ok(())
    }

    fn set_scrollback_lines(&mut self, lines: Option<usize>) -> Result<()> {
        self.term.set_scrollback_max_lines(lines)?;
        Ok(())
    }

    fn is_mouse_tracking(&self) -> bool {
        self.term.is_mouse_tracking().unwrap_or(false)
    }

    fn encode_mouse(&mut self, m: &MouseInput) -> Vec<u8> {
        let size = mouse::EncoderSize {
            screen_width: m.screen_px.0.max(1),
            screen_height: m.screen_px.1.max(1),
            cell_width: m.cell_px.0.max(1),
            cell_height: m.cell_px.1.max(1),
            padding_top: 0,
            padding_bottom: 0,
            padding_right: 0,
            padding_left: 0,
        };
        self.mouse_encoder
            .set_options_from_terminal(&self.term)
            .set_size(size);
        self.mouse_event
            .set_action(map_mouse_action(m.action))
            .set_button(m.button.map(map_mouse_button))
            .set_mods(map_mods(m.mods))
            .set_position(mouse::Position {
                x: m.pos_px.0 as f32,
                y: m.pos_px.1 as f32,
            });

        let mut out = Vec::with_capacity(16);
        if self
            .mouse_encoder
            .encode_to_vec(&self.mouse_event, &mut out)
            .is_err()
        {
            out.clear();
        }
        out
    }

    fn encode_paste(&mut self, text: &str) -> Vec<u8> {
        let bracketed = self.term.mode(Mode::BRACKETED_PASTE).unwrap_or(false);
        let src = text.as_bytes();
        let mut data = src.to_vec();
        // Bracketed markers add 12 bytes; newline→CR is length-preserving.
        let mut buf = vec![0u8; src.len() + 16];
        match paste::encode(&mut data, bracketed, &mut buf) {
            Ok(n) => {
                buf.truncate(n);
                buf
            }
            Err(_) => src.to_vec(),
        }
    }

    fn bracketed_paste(&self) -> bool {
        self.term.mode(Mode::BRACKETED_PASTE).unwrap_or(false)
    }

    fn take_responses(&mut self) -> Vec<u8> {
        std::mem::take(&mut *self.responses.borrow_mut())
    }

    fn take_bell(&mut self) -> bool {
        self.bell.replace(false)
    }

    fn title(&self) -> Option<String> {
        self.term
            .title()
            .ok()
            .map(str::to_string)
            .filter(|s| !s.is_empty())
    }

    fn hyperlink_at(&self, x: u16, y: u16) -> Option<String> {
        // Resolve a grid reference for the viewport cell and read its OSC 8 URI.
        // `hyperlink_uri` writes 0 bytes when the cell has no hyperlink, and the
        // grid ref is read immediately (valid only until the next terminal write).
        let gr = self
            .term
            .grid_ref(Point::Viewport(PointCoordinate { x, y: y as u32 }))
            .ok()?;
        let mut buf = [0u8; 2048];
        let n = gr.hyperlink_uri(&mut buf).ok()?;
        (n > 0).then(|| String::from_utf8_lossy(&buf[..n]).into_owned())
    }

    fn select_semantic(
        &mut self,
        kind: SelectKind,
        x: u16,
        y: u16,
        word_boundaries: &[char],
    ) -> bool {
        use libghostty_vt::selection::{SelectLineOptions, SelectWordOptions};

        let installed = (|| {
            let gr = self
                .term
                .grid_ref(Point::Viewport(PointCoordinate { x, y: y as u32 }))
                .ok()?;
            let sel = match kind {
                SelectKind::Word => {
                    let mut opts = SelectWordOptions::new(gr);
                    // An empty list means "use Ghostty's defaults" — passing an
                    // empty slice would instead mean *no* boundaries, i.e. the
                    // whole line is one word.
                    if !word_boundaries.is_empty() {
                        opts = opts.with_boundary_codepoints(word_boundaries);
                    }
                    self.term.select_word(opts).ok()?
                }
                // `with_semantic_prompt_boundary` stops a line selection at a
                // prompt, so triple-clicking a command doesn't drag in the
                // shell's output.
                SelectKind::Line => self
                    .term
                    .select_line(SelectLineOptions::new(gr).with_semantic_prompt_boundary(true))
                    .ok()?,
                SelectKind::Output => self.term.select_output(gr).ok()?,
            }?;
            self.install_selection(&sel)
        })();

        // A gesture that finds nothing leaves the existing selection alone.
        if installed.is_some() {
            // Word / line / output extents are runs of text, never blocks.
            self.sel_rectangle = false;
        }
        self.adopt_selection(installed)
    }

    fn selection_begin(&mut self, x: u16, y: u16, rectangle: bool) {
        self.sel_anchor = self.track_viewport(x, y);
        // A fresh drag selects the single cell under the pointer until it moves.
        self.selection_update(x, y, rectangle);
    }

    fn selection_update(&mut self, x: u16, y: u16, rectangle: bool) {
        // The anchor's tracked reference is resolved to an untracked snapshot and
        // consumed *within this call* — the binding's untracked refs are invalid
        // after any mutating terminal operation, and `set_selection` is one.
        let installed = (|| {
            let anchor = self.sel_anchor.as_ref()?;
            if !anchor.has_value() {
                return None;
            }
            let start = anchor.snapshot(&self.term).ok()??;
            let end = self
                .term
                .grid_ref(Point::Viewport(PointCoordinate { x, y: y as u32 }))
                .ok()?;
            let sel = libghostty_vt::selection::Selection::new(start, end, rectangle);
            self.install_selection(&sel)
        })();
        self.sel_rectangle = rectangle;
        if !self.adopt_selection(installed) {
            // The anchor lost its cell (the screen was reset or its row pruned
            // beyond recovery). Dropping the selection is the honest outcome —
            // extending from a cell that no longer exists would select something
            // the user never pointed at.
            self.selection_clear();
        }
    }

    fn selection_clear(&mut self) {
        self.sel_anchor = None;
        self.sel_head = None;
        self.sel_rectangle = false;
        if self.selection_installed {
            let _ = self.term.set_selection(None);
            self.selection_dirty = true;
        }
        self.selection_installed = false;
    }

    fn select_all(&mut self) -> bool {
        let installed = (|| {
            let sel = self.term.select_all().ok()??;
            self.install_selection(&sel)
        })();
        if installed.is_some() {
            self.sel_rectangle = false;
        }
        self.adopt_selection(installed)
    }

    fn selection_adjust(&mut self, how: super::SelectionAdjust) -> Option<u32> {
        use libghostty_vt::selection::Adjustment;

        let how = match how {
            super::SelectionAdjust::Left => Adjustment::Left,
            super::SelectionAdjust::Right => Adjustment::Right,
            super::SelectionAdjust::Up => Adjustment::Up,
            super::SelectionAdjust::Down => Adjustment::Down,
            super::SelectionAdjust::PageUp => Adjustment::PageUp,
            super::SelectionAdjust::PageDown => Adjustment::PageDown,
            super::SelectionAdjust::Home => Adjustment::Home,
            super::SelectionAdjust::End => Adjustment::End,
            super::SelectionAdjust::BeginningOfLine => Adjustment::BeginningOfLine,
            super::SelectionAdjust::EndOfLine => Adjustment::EndOfLine,
        };

        // Rebuild the selection from both tracked ends, move its end, reinstall.
        // The terminal owns the live selection but cannot be asked for it, which
        // is the whole reason the head is tracked at all.
        let (installed, end_row) = {
            let out = (|| {
                if !self.selection_installed {
                    return None;
                }
                let (a, h) = (self.sel_anchor.as_ref()?, self.sel_head.as_ref()?);
                if !a.has_value() || !h.has_value() {
                    return None;
                }
                let start = a.snapshot(&self.term).ok()??;
                let end = h.snapshot(&self.term).ok()??;
                let mut sel =
                    libghostty_vt::selection::Selection::new(start, end, self.sel_rectangle);
                sel.adjust(&self.term, how).ok()?;
                // Read the new end's row *before* installing: installing is a
                // mutating call and invalidates these untracked refs.
                let row = self
                    .term
                    .point_from_grid_ref(&sel.end(), PointSpace::Screen)
                    .ok()
                    .flatten()
                    .map(|p| p.y);
                self.install_selection(&sel).map(|pins| (pins, row))
            })();
            match out {
                Some((pins, row)) => (Some(pins), row),
                None => (None, None),
            }
        };
        self.adopt_selection(installed).then_some(end_row).flatten()
    }

    fn selection_active(&self) -> bool {
        self.selection_installed
    }

    fn selected_text(&self, trim: bool) -> Option<String> {
        use libghostty_vt::selection::FormatOptions;

        if !self.selection_installed {
            return None;
        }
        // `unwrap` + `trim` is documented by the binding as Ghostty's own
        // `Screen.selectionString()` clipboard behaviour; `trim` is the user's
        // `clipboard-trim-trailing-spaces`. With no `with_selection`, this
        // formats the terminal's *active* selection — the tracked one, so the
        // read spans scrollback without giest holding any pins for it.
        let opts = FormatOptions::new().with_unwrap(true).with_trim(trim);
        let bytes = self.term.format_selection_alloc(None, opts).ok()??;
        Some(String::from_utf8_lossy(&bytes).into_owned())
    }

    fn set_row_anchor(&mut self, row: Option<u32>) {
        self.row_anchor = row.and_then(|y| {
            self.term
                .track_grid_ref(Point::Screen(PointCoordinate { x: 0, y }))
                .ok()
        });
    }

    fn row_anchor_now(&self) -> Option<u32> {
        let a = self.row_anchor.as_ref()?;
        // A tracked reference whose row was destroyed reports no value — and
        // upstream *also* moves such a pin to the screen's top-left, so trusting
        // a bare point would silently read as "row 0, no drift" at exactly the
        // moment there is drift. `has_value` is the discriminator.
        if !a.has_value() {
            return None;
        }
        Some(a.point(PointSpace::Screen).ok()??.y)
    }

    fn jump_to_prompt(&self, delta: isize) -> Option<usize> {
        if delta == 0 {
            return None;
        }
        let rows = self.term.rows().ok()? as u32;
        // `scrollback_rows` = total rows minus the viewport height, i.e. the
        // screen-space y of the viewport top when resting at the live bottom and
        // the maximum lines the viewport can scroll up.
        let bottom_top = self.term.scrollback_rows().unwrap_or(0) as u32;
        let last = bottom_top + rows.saturating_sub(1);

        // The current viewport top in absolute screen coordinates.
        let vp_gr = self
            .term
            .grid_ref(Point::Viewport(PointCoordinate { x: 0, y: 0 }))
            .ok()?;
        let vp_top = self
            .term
            .point_from_grid_ref(&vp_gr, PointSpace::Screen)
            .ok()??
            .y;

        // Is the screen row at `y` a (primary) semantic-prompt row?
        let is_prompt = |y: u32| -> bool {
            self.term
                .grid_ref(Point::Screen(PointCoordinate { x: 0, y }))
                .ok()
                .and_then(|gr| gr.row().ok())
                .and_then(|row| row.semantic_prompt().ok())
                .is_some_and(|sp| sp == RowSemanticPrompt::Prompt)
        };

        // Walk outward from the current top (excluding it) until the |delta|-th
        // prompt row, then report its offset above the live bottom.
        let want = delta.unsigned_abs();
        let mut found = 0usize;
        if delta < 0 {
            let mut y = vp_top;
            while y > 0 {
                y -= 1;
                if is_prompt(y) {
                    found += 1;
                    if found == want {
                        return Some(bottom_top.saturating_sub(y) as usize);
                    }
                }
            }
        } else {
            let mut y = vp_top;
            while y < last {
                y += 1;
                if is_prompt(y) {
                    found += 1;
                    if found == want {
                        return Some(bottom_top.saturating_sub(y) as usize);
                    }
                }
            }
        }
        None
    }

    fn screen_text(&self) -> Vec<super::RowText> {
        let cols = self.term.cols().unwrap_or(0);
        let rows = self.term.rows().unwrap_or(0) as u32;
        let scrollback = self.term.scrollback_rows().unwrap_or(0) as u32;
        let total = scrollback + rows;
        let mut out = Vec::with_capacity(total as usize);
        // Grapheme clusters are almost always 1 char; 8 covers base + combining.
        // `big` is a heap fallback for the rare cluster that overflows `buf`.
        let mut buf = ['\0'; 8];
        let mut big: Vec<char> = Vec::new();
        for y in 0..total {
            let mut chars: Vec<char> = Vec::new();
            let mut col_of = Vec::new();
            // Chars up to and including the last non-blank cell, so trailing
            // blanks (the spaces we emit for empty cells) are dropped.
            let mut last_non_blank = 0usize;
            for x in 0..cols {
                let Ok(gr) = self.term.grid_ref(Point::Screen(PointCoordinate { x, y })) else {
                    // Unreadable cell: keep the column alignment with a blank.
                    chars.push(' ');
                    col_of.push(x);
                    continue;
                };
                // The tail half of a wide char is an empty spacer cell — skip it
                // (emit nothing, don't advance a column) so the wide char's
                // codepoint stays adjacent to its neighbor; a space here would
                // defeat search/copy of e.g. "世界".
                //
                // `SpacerHead` is the same problem at the other end: the blank
                // left at the end of a soft-wrapped row when a wide character
                // didn't fit and moved to the next row. Emitting a space for it
                // would put one *inside* a word that wrapped, so a query
                // spanning the wrap would not match.
                if matches!(
                    gr.cell().ok().and_then(|c| c.wide().ok()),
                    Some(CellWide::SpacerTail | CellWide::SpacerHead)
                ) {
                    continue;
                }
                // Read the grapheme, retrying on a heap buffer for clusters longer
                // than `buf` (long ZWJ emoji) so they stay searchable, not blanked.
                let cluster: &[char] = match gr.graphemes(&mut buf) {
                    Ok(n) => &buf[..n],
                    Err(libghostty_vt::error::Error::OutOfSpace { required }) => {
                        big.clear();
                        big.resize(required, '\0');
                        match gr.graphemes(&mut big) {
                            Ok(n) => &big[..n],
                            Err(_) => &[],
                        }
                    }
                    Err(_) => &[],
                };
                if cluster.is_empty() {
                    // A genuine blank cell: a space keeps char→column alignment.
                    chars.push(' ');
                    col_of.push(x);
                } else {
                    for &ch in cluster {
                        chars.push(ch);
                        col_of.push(x);
                    }
                    last_non_blank = chars.len();
                }
            }
            chars.truncate(last_non_blank);
            col_of.truncate(last_non_blank);
            // Does this row continue onto the next? Search joins such rows into
            // one logical line so a query can span the wrap.
            let wrapped = self
                .term
                .grid_ref(Point::Screen(PointCoordinate { x: 0, y }))
                .ok()
                .and_then(|gr| gr.row().ok())
                .and_then(|r| r.is_wrapped().ok())
                .unwrap_or(false);
            out.push(super::RowText {
                row: y,
                chars,
                cols: col_of,
                wrapped,
            });
        }
        out
    }

    fn snapshot(&mut self, out: &mut GridSnapshot) -> Result<()> {
        // A selection change is treated exactly like a viewport move: it may not
        // dirty the render state, and the clean fast path below would then leave
        // the highlight unpainted on an idle screen.
        let moved = self.viewport_moved || self.selection_dirty;
        self.viewport_moved = false;
        self.selection_dirty = false;

        // Borrows of the distinct fields below are disjoint, so the snapshot
        // (which holds &mut render_state) coexists with the iterator buffers.
        let snapshot = self.render_state.update(&self.term)?;

        let cols = snapshot.cols()?;
        let rows = snapshot.rows()?;

        // Kitty placements are refreshed on EVERY snapshot, deliberately ahead
        // of the dirty-skip below. libghostty's image storage keeps its own dirty
        // flag that the C API doesn't expose and that isn't part of the render
        // state's, so a kitty delete or a place-only command can leave the frame
        // `Clean` — and an image that was just deleted would otherwise stay on
        // screen forever. This is a handful of FFI reads over 1-10 placements;
        // the expensive part (the pixel copy) is keyed on image id and skipped
        // on a hit. Errors are swallowed: a malformed image must never blank the
        // pane, which is what returning `Err` from here would do.
        let _ = walk_placements(
            &self.term,
            &mut self.placements,
            &mut self.image_cache,
            &mut self.image_ids_seen,
            &mut out.images,
        );

        // Nothing changed since the last snapshot of this same-sized grid: keep
        // the previously-filled cells and skip the O(rows*cols) per-cell FFI
        // walk. (`update` consumed the dirty state; writes re-dirty it, and a
        // viewport scroll sets `moved`, so only idle/cursor-blink frames skip.)
        if !moved
            && matches!(snapshot.dirty()?, Dirty::Clean)
            && !out.cells.is_empty()
            && out.cols == cols
            && out.rows == rows
        {
            return Ok(());
        }

        let colors = snapshot.colors()?;

        out.cols = cols;
        out.rows = rows;
        out.default_fg = rgb(colors.foreground);
        out.default_bg = rgb(colors.background);
        out.cursor_color = colors.cursor.map(rgb).unwrap_or(out.default_fg);

        out.cursor_visible = snapshot.cursor_visible()?;
        out.cursor_blinking = snapshot.cursor_blinking()?;
        out.cursor_shape = match snapshot.cursor_visual_style()? {
            CursorVisualStyle::Bar => CursorShape::Bar,
            CursorVisualStyle::Block => CursorShape::Block,
            CursorVisualStyle::Underline => CursorShape::Underline,
            CursorVisualStyle::BlockHollow => CursorShape::HollowBlock,
            _ => CursorShape::Block,
        };
        if let Some(cur) = snapshot.cursor_viewport()? {
            out.cursor_x = cur.x;
            out.cursor_y = cur.y;
        }

        // Resize to the grid and blank every cell up front (reusing each cell's
        // inline-string buffer via `clear()` rather than reallocating). Cells the
        // iterators don't yield therefore read back blank, matching a fresh grid.
        let total = cols as usize * rows as usize;
        out.cells.resize(total, Cell::default());
        for cell in out.cells.iter_mut() {
            cell.text.clear();
            cell.fg = Rgb::default();
            cell.bg = Rgb::default();
            cell.bold = false;
            cell.italic = false;
            cell.underline = UnderlineStyle::None;
            cell.underline_color = None;
            cell.strikethrough = false;
            cell.overline = false;
            cell.faint = false;
            cell.blink = false;
            cell.invisible = false;
            // `bg` above is a placeholder the renderer never paints: a blank cell
            // has no explicit background, so it emits no background quad at all.
            cell.bg_explicit = false;
            cell.inverse = false;
            cell.selected = false;
        }

        let default_fg = out.default_fg;
        let default_bg = out.default_bg;
        // Use the *live* palette from the render snapshot (it reflects OSC 4
        // redefinitions), not the static config copy, so the bold-is-bright bump
        // and palette-indexed underline colors track runtime changes — matching
        // Ghostty's `Style.fg`, which reads the live terminal palette.
        let palette = colors.palette.map(rgb);
        let bold_color = self.bold_color;
        let min_contrast = self.min_contrast;
        let mut has_blink = false;
        let mut y: usize = 0;
        let mut rows_iter = self.rows_buf.update(&snapshot)?;
        while let Some(row) = rows_iter.next() {
            if y >= rows as usize {
                break;
            }
            // The row-local selection range, asked once per row rather than per
            // cell — which is what the C API recommends for a renderer that can
            // work in spans, and it is where a soft-wrapped, scrollback-spanning
            // or reflowed selection resolves to actual columns.
            let sel = row.selection().ok().flatten();
            let mut x: usize = 0;
            let mut cells_iter = self.cells_buf.update(row)?;
            while let Some(cell) = cells_iter.next() {
                if x >= cols as usize {
                    break;
                }
                // Fill in place, reusing the blanked cell's string buffer.
                let idx = y * cols as usize + x;
                copy_cell(
                    cell,
                    default_fg,
                    default_bg,
                    &palette,
                    bold_color,
                    min_contrast,
                    &mut out.cells[idx],
                )?;
                out.cells[idx].selected = sel
                    .is_some_and(|s| x >= s.start_x as usize && x <= s.end_x as usize);
                has_blink |= out.cells[idx].blink;
                x += 1;
            }
            y += 1;
        }
        out.has_blink = has_blink;

        Ok(())
    }
}
