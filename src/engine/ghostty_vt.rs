//! [`TerminalEngine`] backed by libghostty-vt (the primary backend).

use std::cell::RefCell;
use std::rc::Rc;

use anyhow::Result;
use libghostty_vt::key::{Action, Encoder, Event, Key, Mods};
use libghostty_vt::mouse;
use libghostty_vt::paste;
use libghostty_vt::render::{CellIterator, CursorVisualStyle, RowIterator};
use libghostty_vt::style::Underline;
use libghostty_vt::terminal::{Mode, ScrollViewport};
use libghostty_vt::{RenderState, Terminal, TerminalOptions};

use super::{
    Cell, CursorShape, GridSnapshot, KeyCode, KeyInput, MouseAction, MouseButton, MouseInput, Rgb,
    TerminalEngine,
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
}

impl GhosttyVtEngine {
    pub fn new(cols: u16, rows: u16, max_scrollback: usize) -> Result<Self> {
        let mut term = Terminal::new(TerminalOptions {
            cols,
            rows,
            max_scrollback,
        })?;

        let responses: ResponseSink = Rc::new(RefCell::new(Vec::new()));
        let sink = responses.clone();
        term.on_pty_write(move |_term, data| {
            sink.borrow_mut().extend_from_slice(data);
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
        })
    }
}

fn map_key(code: KeyCode) -> Key {
    use KeyCode::*;
    match code {
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
    use super::GhosttyVtEngine;
    use crate::engine::{
        GridSnapshot, KeyCode, KeyInput, KeyMods, MouseAction, MouseButton, MouseInput, Rgb,
        TerminalEngine,
    };

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
    fn newline_advances_cursor_row() {
        let mut eng = GhosttyVtEngine::new(20, 3, 100).unwrap();
        eng.write(b"hi\r\n");
        let s = snap(&mut eng);
        assert_eq!(s.cell(0, 0).unwrap().text, "h");
        assert_eq!(s.cell(1, 0).unwrap().text, "i");
        assert_eq!(s.cursor_y, 1);
        assert_eq!(s.cursor_x, 0);
    }

    #[test]
    fn osc_sets_window_title() {
        let mut eng = GhosttyVtEngine::new(20, 3, 100).unwrap();
        eng.write(b"\x1b]2;hello\x07");
        assert_eq!(eng.title().as_deref(), Some("hello"));
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
    }

    fn scroll_to_bottom(&mut self) {
        self.term.scroll_viewport(ScrollViewport::Bottom);
    }

    fn scroll_to_top(&mut self) {
        self.term.scroll_viewport(ScrollViewport::Top);
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
        self.term.set_default_color_palette(Some(pal))?;
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

    fn take_responses(&mut self) -> Vec<u8> {
        std::mem::take(&mut *self.responses.borrow_mut())
    }

    fn title(&self) -> Option<String> {
        self.term
            .title()
            .ok()
            .map(str::to_string)
            .filter(|s| !s.is_empty())
    }

    fn snapshot(&mut self, out: &mut GridSnapshot) -> Result<()> {
        // Borrows of the distinct fields below are disjoint, so the snapshot
        // (which holds &mut render_state) coexists with the iterator buffers.
        let snapshot = self.render_state.update(&self.term)?;

        let colors = snapshot.colors()?;
        let cols = snapshot.cols()?;
        let rows = snapshot.rows()?;

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
            cell.underline = false;
            cell.strikethrough = false;
        }

        let mut y: usize = 0;
        let mut rows_iter = self.rows_buf.update(&snapshot)?;
        while let Some(row) = rows_iter.next() {
            if y >= rows as usize {
                break;
            }
            let mut x: usize = 0;
            let mut cells_iter = self.cells_buf.update(row)?;
            while let Some(cell) = cells_iter.next() {
                if x >= cols as usize {
                    break;
                }
                let style = cell.style()?;
                let mut fg = cell.fg_color()?.map(rgb).unwrap_or(out.default_fg);
                let mut bg = cell.bg_color()?.map(rgb).unwrap_or(out.default_bg);
                if style.inverse {
                    std::mem::swap(&mut fg, &mut bg);
                }

                // Fill in place, reusing the blanked cell's string buffer.
                let dst = &mut out.cells[y * cols as usize + x];
                for ch in cell.graphemes()? {
                    dst.text.push(ch);
                }
                dst.fg = fg;
                dst.bg = bg;
                dst.bold = style.bold;
                dst.italic = style.italic;
                dst.underline = !matches!(style.underline, Underline::None);
                dst.strikethrough = style.strikethrough;
                x += 1;
            }
            y += 1;
        }

        Ok(())
    }
}
