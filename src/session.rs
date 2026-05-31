//! One terminal session: a shell on a PTY, its libghostty-vt engine, and the
//! per-tab interaction state (selection, held mouse button). The [`App`](crate::app)
//! owns a `Vec<Session>` for tabs; cell metrics live in the app and are passed in.

use anyhow::Result;
use eframe::egui;

use crate::config::Config;
use crate::engine::{
    GhosttyVtEngine, GridSnapshot, KeyCode, KeyInput, KeyMods, MouseAction, MouseButton,
    MouseInput, TerminalEngine,
};
use crate::pty::Pty;

const DEFAULT_COLS: u16 = 80;
const DEFAULT_ROWS: u16 = 24;
const SCROLLBACK: usize = 10_000;

pub struct Session {
    pty: Pty,
    engine: GhosttyVtEngine,
    pub snapshot: GridSnapshot,
    cols: u16,
    rows: u16,
    sel_anchor: Option<(u16, u16)>,
    sel_head: Option<(u16, u16)>,
    mouse_down: Option<MouseButton>,
}

impl Session {
    /// Spawn a shell and build its engine. `ctx` is cloned so the PTY reader
    /// thread can wake the UI when output arrives.
    pub fn new(ctx: &egui::Context, config: &Config) -> Result<Self> {
        let wake_ctx = ctx.clone();
        let pty = Pty::spawn("powershell.exe", DEFAULT_COLS, DEFAULT_ROWS, move || {
            wake_ctx.request_repaint()
        })?;
        let mut engine = GhosttyVtEngine::new(DEFAULT_COLS, DEFAULT_ROWS, SCROLLBACK)?;
        engine.apply_theme(config.fg, config.bg, &config.palette)?;

        Ok(Self {
            pty,
            engine,
            snapshot: GridSnapshot::default(),
            cols: DEFAULT_COLS,
            rows: DEFAULT_ROWS,
            sel_anchor: None,
            sel_head: None,
            mouse_down: None,
        })
    }

    /// Drain pending PTY output into the engine and flush responses back.
    pub fn pump_pty(&mut self) {
        while let Ok(chunk) = self.pty.output.try_recv() {
            self.engine.write(&chunk);
        }
        let responses = self.engine.take_responses();
        if !responses.is_empty() {
            let _ = self.pty.write(&responses);
        }
    }

    pub fn is_mouse_tracking(&self) -> bool {
        self.engine.is_mouse_tracking()
    }

    /// The shell-set window/tab title, if any.
    pub fn title(&self) -> Option<String> {
        self.engine.title()
    }

    pub fn update_snapshot(&mut self) -> bool {
        self.engine.snapshot(&mut self.snapshot).is_ok()
    }

    pub fn default_bg(&self) -> crate::engine::Rgb {
        self.snapshot.default_bg
    }

    /// Resize the grid (and PTY) to fit `area` (points) at scale `ppp`.
    pub fn fit_grid(&mut self, area: egui::Rect, ppp: f32, cell_w: f32, cell_h: f32) {
        let cols = ((area.width() * ppp / cell_w).floor() as u16).max(1);
        let rows = ((area.height() * ppp / cell_h).floor() as u16).max(1);
        if cols != self.cols || rows != self.rows {
            self.cols = cols;
            self.rows = rows;
            let _ = self.engine.resize(cols, rows, (cell_w as u32, cell_h as u32));
            let _ = self.pty.resize(cols, rows);
        }
    }

    /// Convert a pointer position (points) to a clamped grid cell.
    pub fn pos_to_cell(&self, pos: egui::Pos2, rect: egui::Rect, ppp: f32, cw: f32, ch: f32) -> (u16, u16) {
        let x = ((pos.x - rect.min.x) * ppp / cw).floor().max(0.0) as u16;
        let y = ((pos.y - rect.min.y) * ppp / ch).floor().max(0.0) as u16;
        (x.min(self.cols.saturating_sub(1)), y.min(self.rows.saturating_sub(1)))
    }

    pub fn begin_selection(&mut self, cell: (u16, u16)) {
        self.sel_anchor = Some(cell);
        self.sel_head = Some(cell);
    }
    pub fn update_selection(&mut self, cell: (u16, u16)) {
        self.sel_head = Some(cell);
    }
    pub fn clear_selection(&mut self) {
        self.sel_anchor = None;
        self.sel_head = None;
    }

    /// Current selection as an inclusive linear (row-major) cell range.
    pub fn selection_range(&self) -> Option<(usize, usize)> {
        let (a, h) = (self.sel_anchor?, self.sel_head?);
        let cols = self.cols as usize;
        let la = a.1 as usize * cols + a.0 as usize;
        let lh = h.1 as usize * cols + h.0 as usize;
        if la == lh {
            return None;
        }
        Some((la.min(lh), la.max(lh)))
    }

    fn selected_text(&self) -> Option<String> {
        let range = self.selection_range()?;
        Some(extract_selection(&self.snapshot, range))
    }

    /// Translate keyboard/text/paste events into PTY bytes. `Ctrl+Shift` combos
    /// are reserved for the app (copy, tab management) and never sent to the
    /// shell, as is `Ctrl+Tab`.
    pub fn handle_input(&mut self, ctx: &egui::Context, tracking: bool, cell_h: f32) {
        let (events, scroll_y, ppp) = ctx.input(|i| {
            (
                i.events.clone(),
                i.smooth_scroll_delta.y,
                i.pixels_per_point().max(1.0),
            )
        });

        if !tracking && scroll_y.abs() > 0.5 {
            let cell_h_pts = (cell_h / ppp).max(1.0);
            let lines = (scroll_y / cell_h_pts).round() as isize;
            if lines != 0 {
                self.engine.scroll(-lines);
            }
        }

        let mut bytes: Vec<u8> = Vec::new();
        for event in &events {
            match event {
                egui::Event::Text(text) => bytes.extend_from_slice(text.as_bytes()),
                egui::Event::Paste(text) => {
                    let encoded = self.engine.encode_paste(text);
                    bytes.extend_from_slice(&encoded);
                }
                egui::Event::Copy => bytes.push(0x03),
                egui::Event::Key {
                    key,
                    pressed: true,
                    modifiers,
                    ..
                } => {
                    let Some(code) = map_egui_key(*key) else {
                        continue;
                    };
                    // Ctrl+Shift is the app's namespace (copy, tab control); the
                    // shell never sees it. Ctrl+Tab is reserved for tab switching.
                    if modifiers.ctrl && modifiers.shift {
                        if code == KeyCode::C {
                            if let Some(text) = self.selected_text() {
                                ctx.copy_text(text);
                            }
                        }
                        continue;
                    }
                    if modifiers.ctrl && code == KeyCode::Tab {
                        continue;
                    }
                    let mods = key_mods(modifiers);
                    if is_text_producing(code) && !mods.ctrl && !mods.alt {
                        continue;
                    }
                    let encoded = self.engine.encode_key(&KeyInput {
                        code,
                        text: None,
                        mods,
                        press: true,
                    });
                    bytes.extend_from_slice(&encoded);
                }
                _ => {}
            }
        }
        if !bytes.is_empty() {
            let _ = self.pty.write(&bytes);
        }
    }

    /// Report mouse events to the running app (only when it tracks the mouse).
    pub fn handle_mouse(&mut self, ctx: &egui::Context, rect: egui::Rect, ppp: f32, cw: f32, ch: f32) {
        let (events, scroll_y) = ctx.input(|i| (i.events.clone(), i.smooth_scroll_delta.y));
        let cell_px = (cw as u32, ch as u32);
        let screen_px = ((self.cols as f32 * cw) as u32, (self.rows as f32 * ch) as u32);
        let to_px = |pos: egui::Pos2| -> (u32, u32) {
            (
                ((pos.x - rect.min.x) * ppp).max(0.0) as u32,
                ((pos.y - rect.min.y) * ppp).max(0.0) as u32,
            )
        };

        let mut out: Vec<u8> = Vec::new();
        for event in &events {
            match event {
                egui::Event::PointerButton {
                    pos,
                    button,
                    pressed,
                    modifiers,
                } => {
                    let b = match button {
                        egui::PointerButton::Primary => MouseButton::Left,
                        egui::PointerButton::Secondary => MouseButton::Right,
                        egui::PointerButton::Middle => MouseButton::Middle,
                        _ => continue,
                    };
                    let action = if *pressed {
                        self.mouse_down = Some(b);
                        MouseAction::Press
                    } else {
                        self.mouse_down = None;
                        MouseAction::Release
                    };
                    out.extend_from_slice(&self.engine.encode_mouse(&MouseInput {
                        action,
                        button: Some(b),
                        pos_px: to_px(*pos),
                        cell_px,
                        screen_px,
                        mods: key_mods(modifiers),
                    }));
                }
                egui::Event::PointerMoved(pos) => {
                    if let Some(b) = self.mouse_down {
                        out.extend_from_slice(&self.engine.encode_mouse(&MouseInput {
                            action: MouseAction::Motion,
                            button: Some(b),
                            pos_px: to_px(*pos),
                            cell_px,
                            screen_px,
                            mods: KeyMods::default(),
                        }));
                    }
                }
                _ => {}
            }
        }

        if scroll_y.abs() > 0.5 {
            let button = if scroll_y > 0.0 {
                MouseButton::WheelUp
            } else {
                MouseButton::WheelDown
            };
            let notches = ((scroll_y.abs() / 40.0).ceil() as usize).clamp(1, 5);
            let pos = ctx
                .input(|i| i.pointer.latest_pos())
                .unwrap_or_else(|| rect.center());
            for _ in 0..notches {
                out.extend_from_slice(&self.engine.encode_mouse(&MouseInput {
                    action: MouseAction::Press,
                    button: Some(button),
                    pos_px: to_px(pos),
                    cell_px,
                    screen_px,
                    mods: KeyMods::default(),
                }));
            }
        }

        if !out.is_empty() {
            let _ = self.pty.write(&out);
        }
    }
}

/// Translate egui modifiers to backend-neutral key modifiers.
fn key_mods(m: &egui::Modifiers) -> KeyMods {
    KeyMods {
        shift: m.shift,
        ctrl: m.ctrl || m.command,
        alt: m.alt,
        sup: false,
    }
}

/// Extract text from a snapshot over an inclusive linear cell range, following
/// text flow and trimming trailing blanks per line.
fn extract_selection(snap: &GridSnapshot, range: (usize, usize)) -> String {
    let (a, b) = range;
    let cols = snap.cols as usize;
    if cols == 0 {
        return String::new();
    }
    let mut lines: Vec<String> = Vec::new();
    let mut cur = String::new();
    for lin in a..=b {
        let x = lin % cols;
        if x == 0 && lin != a {
            lines.push(std::mem::take(&mut cur));
        }
        if let Some(c) = snap.cell(x as u16, (lin / cols) as u16) {
            cur.push_str(if c.text.is_empty() { " " } else { &c.text });
        }
    }
    lines.push(cur);
    lines
        .iter()
        .map(|l| l.trim_end())
        .collect::<Vec<_>>()
        .join("\n")
}

/// Map an egui key to our backend-neutral [`KeyCode`].
fn map_egui_key(key: egui::Key) -> Option<KeyCode> {
    use KeyCode as C;
    use egui::Key as K;
    Some(match key {
        K::A => C::A, K::B => C::B, K::C => C::C, K::D => C::D, K::E => C::E,
        K::F => C::F, K::G => C::G, K::H => C::H, K::I => C::I, K::J => C::J,
        K::K => C::K, K::L => C::L, K::M => C::M, K::N => C::N, K::O => C::O,
        K::P => C::P, K::Q => C::Q, K::R => C::R, K::S => C::S, K::T => C::T,
        K::U => C::U, K::V => C::V, K::W => C::W, K::X => C::X, K::Y => C::Y, K::Z => C::Z,
        K::Num0 => C::Digit0, K::Num1 => C::Digit1, K::Num2 => C::Digit2,
        K::Num3 => C::Digit3, K::Num4 => C::Digit4, K::Num5 => C::Digit5,
        K::Num6 => C::Digit6, K::Num7 => C::Digit7, K::Num8 => C::Digit8,
        K::Num9 => C::Digit9,
        K::Enter => C::Enter, K::Tab => C::Tab, K::Backspace => C::Backspace,
        K::Escape => C::Escape, K::Space => C::Space, K::Delete => C::Delete,
        K::Insert => C::Insert, K::Home => C::Home, K::End => C::End,
        K::PageUp => C::PageUp, K::PageDown => C::PageDown,
        K::ArrowUp => C::ArrowUp, K::ArrowDown => C::ArrowDown,
        K::ArrowLeft => C::ArrowLeft, K::ArrowRight => C::ArrowRight,
        K::F1 => C::F1, K::F2 => C::F2, K::F3 => C::F3, K::F4 => C::F4,
        K::F5 => C::F5, K::F6 => C::F6, K::F7 => C::F7, K::F8 => C::F8,
        K::F9 => C::F9, K::F10 => C::F10, K::F11 => C::F11, K::F12 => C::F12,
        K::Minus => C::Minus, K::Equals => C::Equal,
        K::OpenBracket => C::BracketLeft, K::CloseBracket => C::BracketRight,
        K::Backslash => C::Backslash, K::Semicolon => C::Semicolon,
        K::Quote => C::Quote, K::Backtick => C::Backquote,
        K::Comma => C::Comma, K::Period => C::Period, K::Slash => C::Slash,
        _ => return None,
    })
}

/// Keys that also produce an egui `Text` event when pressed without Ctrl/Alt.
fn is_text_producing(code: KeyCode) -> bool {
    use KeyCode::*;
    !matches!(
        code,
        Enter | Tab | Backspace | Escape | Delete | Insert | Home | End
            | PageUp | PageDown | ArrowUp | ArrowDown | ArrowLeft | ArrowRight
            | F1 | F2 | F3 | F4 | F5 | F6 | F7 | F8 | F9 | F10 | F11 | F12
    )
}

#[cfg(test)]
mod tests {
    use super::extract_selection;
    use crate::engine::{Cell, GridSnapshot};

    fn grid(rows: &[&str], cols: u16) -> GridSnapshot {
        let mut s = GridSnapshot {
            cols,
            rows: rows.len() as u16,
            ..Default::default()
        };
        for row in rows {
            let mut chars: Vec<char> = row.chars().collect();
            chars.resize(cols as usize, ' ');
            for ch in chars {
                let mut c = Cell::default();
                if ch != ' ' {
                    c.text = ch.to_string();
                }
                s.cells.push(c);
            }
        }
        s
    }

    #[test]
    fn selection_follows_text_flow() {
        let s = grid(&["hello", "world"], 5);
        assert_eq!(extract_selection(&s, (0, 6)), "hello\nwo");
        assert_eq!(extract_selection(&s, (0, 4)), "hello");
        let s2 = grid(&["hi   ", "bye  "], 5);
        assert_eq!(extract_selection(&s2, (0, 9)), "hi\nbye");
    }
}
