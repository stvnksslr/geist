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
use crate::osc52::Osc52Scanner;
use crate::profiles::Profile;
use crate::pty::Pty;

const DEFAULT_COLS: u16 = 80;
const DEFAULT_ROWS: u16 = 24;

pub struct Session {
    pty: Pty,
    engine: GhosttyVtEngine,
    pub snapshot: GridSnapshot,
    cols: u16,
    rows: u16,
    sel_anchor: Option<(u16, u16)>,
    sel_head: Option<(u16, u16)>,
    mouse_down: Option<MouseButton>,
    /// False once the shell has exited (PTY output channel disconnected).
    alive: bool,
    /// Side parser for OSC 52 clipboard-set sequences in the PTY output.
    osc52: Osc52Scanner,
}

impl Session {
    /// Spawn `profile`'s shell and build its engine. `ctx` is cloned so the PTY
    /// reader thread can wake the UI when output arrives.
    pub fn new(ctx: &egui::Context, config: &Config, profile: &Profile) -> Result<Self> {
        let wake_ctx = ctx.clone();
        let pty = Pty::spawn(
            &profile.program,
            &profile.args,
            DEFAULT_COLS,
            DEFAULT_ROWS,
            move || wake_ctx.request_repaint(),
        )?;
        let mut engine = GhosttyVtEngine::new(DEFAULT_COLS, DEFAULT_ROWS, config.scrollback_limit)?;
        engine.apply_theme(config.fg, config.bg, &config.palette)?;
        engine.set_cursor_color(config.cursor)?;

        Ok(Self {
            pty,
            engine,
            snapshot: GridSnapshot::default(),
            cols: DEFAULT_COLS,
            rows: DEFAULT_ROWS,
            sel_anchor: None,
            sel_head: None,
            mouse_down: None,
            alive: true,
            osc52: Osc52Scanner::new(),
        })
    }

    /// Drain pending PTY output into the engine and flush responses back.
    /// Marks the session dead when the shell has exited (channel disconnected).
    pub fn pump_pty(&mut self) {
        use std::sync::mpsc::TryRecvError;
        let mut clipboard_sets: Vec<String> = Vec::new();
        loop {
            match self.pty.output.try_recv() {
                Ok(chunk) => {
                    self.engine.write(&chunk);
                    self.osc52.feed(&chunk, &mut clipboard_sets);
                }
                Err(TryRecvError::Empty) => break,
                Err(TryRecvError::Disconnected) => {
                    self.alive = false;
                    break;
                }
            }
        }
        // A program copied to the clipboard via OSC 52 (last write wins).
        if let Some(text) = clipboard_sets.pop() {
            write_clipboard(&text);
        }
        // Primary exit signal on Windows: poll the shell process itself.
        if self.alive && !self.pty.is_running() {
            self.alive = false;
        }
        let responses = self.engine.take_responses();
        if !responses.is_empty() {
            let _ = self.pty.write(&responses);
        }
    }

    /// Whether the shell backing this session is still running.
    pub fn is_alive(&self) -> bool {
        self.alive
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

    /// Extend an existing selection to `cell` (Shift+click); starts a new one
    /// if nothing is selected yet.
    pub fn extend_selection(&mut self, cell: (u16, u16)) {
        if self.sel_anchor.is_some() {
            self.sel_head = Some(cell);
        } else {
            self.begin_selection(cell);
        }
    }
    pub fn clear_selection(&mut self) {
        self.sel_anchor = None;
        self.sel_head = None;
    }

    /// Select the whole word under `cell` (double-click).
    pub fn select_word(&mut self, cell: (u16, u16)) {
        let (l, r) = word_bounds(&self.snapshot, cell.0, cell.1);
        self.sel_anchor = Some((l, cell.1));
        self.sel_head = Some((r, cell.1));
    }

    /// Select the entire visual row under `cell` (triple-click).
    pub fn select_line(&mut self, cell: (u16, u16)) {
        self.sel_anchor = Some((0, cell.1));
        self.sel_head = Some((self.cols.saturating_sub(1), cell.1));
    }

    /// The URL under `cell`, if any (for Ctrl+click to open).
    pub fn url_at(&self, cell: (u16, u16)) -> Option<String> {
        find_url_at(&self.snapshot, cell.0, cell.1)
    }

    /// Current selection as an inclusive linear (row-major) cell range.
    pub fn selection_range(&self) -> Option<(usize, usize)> {
        let (a, h) = (self.sel_anchor?, self.sel_head?);
        let cols = self.cols as usize;
        let la = a.1 as usize * cols + a.0 as usize;
        let lh = h.1 as usize * cols + h.0 as usize;
        Some((la.min(lh), la.max(lh)))
    }

    fn selected_text(&self) -> Option<String> {
        let range = self.selection_range()?;
        Some(extract_selection(&self.snapshot, range))
    }

    /// The current selection's text, if any (for copy-on-select).
    pub fn selection_text(&self) -> Option<String> {
        self.selected_text()
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
                // egui delivers Ctrl+C, Ctrl+Shift+C and Ctrl+Insert (and Cut)
                // as these events — `command+C` matches whether or not Shift is
                // held. Windows-Terminal semantics: with a selection, copy it
                // (and clear); with none, Ctrl+C is an interrupt.
                egui::Event::Copy | egui::Event::Cut => {
                    if let Some(text) = self.selected_text() {
                        ctx.copy_text(text);
                        self.clear_selection();
                    } else {
                        bytes.push(0x03);
                    }
                }
                egui::Event::Key {
                    key,
                    pressed: true,
                    modifiers,
                    ..
                } => {
                    let Some(code) = map_egui_key(*key) else {
                        continue;
                    };
                    // Ctrl+Shift is the app's namespace; the shell never sees it.
                    // The clipboard combos (Ctrl+Shift+C/V/X) arrive as
                    // Copy/Paste/Cut events handled above, so here we just
                    // swallow every other Ctrl+Shift combo (tab/split shortcuts).
                    if modifiers.ctrl && modifiers.shift {
                        continue;
                    }
                    if modifiers.ctrl && code == KeyCode::Tab {
                        continue;
                    }
                    // Shift+PageUp/Down scroll by a page; Shift+Home/End jump to
                    // the top/bottom of scrollback. These drive the viewport
                    // instead of being sent to the shell (matches Ghostty).
                    if modifiers.shift
                        && matches!(
                            code,
                            KeyCode::PageUp | KeyCode::PageDown | KeyCode::Home | KeyCode::End
                        )
                    {
                        let page = self.rows.saturating_sub(1).max(1) as isize;
                        match code {
                            KeyCode::PageUp => self.engine.scroll(-page),
                            KeyCode::PageDown => self.engine.scroll(page),
                            KeyCode::Home => self.engine.scroll_to_top(),
                            KeyCode::End => self.engine.scroll_to_bottom(),
                            _ => {}
                        }
                        continue;
                    }
                    // Ctrl +/-/0 are reserved by the app for font zoom.
                    if modifiers.ctrl
                        && !modifiers.shift
                        && matches!(code, KeyCode::Equal | KeyCode::Minus | KeyCode::Digit0)
                    {
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
            // Typing returns the viewport to the bottom (Ghostty behavior).
            self.engine.scroll_to_bottom();
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

/// Write text to the system clipboard (best-effort; ignores failures).
fn write_clipboard(text: &str) {
    if let Ok(mut cb) = arboard::Clipboard::new() {
        let _ = cb.set_text(text.to_owned());
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

/// Whether `ch` counts as part of a word for double-click selection. Word
/// boundaries are whitespace and a small set of shell/bracket punctuation;
/// path/URL characters (`/ . - _ : @ ~`) stay part of the word so a whole path
/// or flag selects in one double-click.
fn is_word_char(ch: char) -> bool {
    !ch.is_whitespace()
        && !matches!(
            ch,
            '(' | ')' | '[' | ']' | '{' | '}' | '<' | '>' | '|' | '&' | ';' | ',' | '"' | '\'' | '`'
        )
}

/// Expand from cell `(x, y)` to the inclusive `[left, right]` column span of the
/// word it sits in. A non-word cell yields just itself.
fn word_bounds(snap: &GridSnapshot, x: u16, y: u16) -> (u16, u16) {
    let is_word = |cx: u16| {
        snap.cell(cx, y)
            .and_then(|c| c.text.chars().next())
            .is_some_and(is_word_char)
    };
    if !is_word(x) {
        return (x, x);
    }
    let mut l = x;
    while l > 0 && is_word(l - 1) {
        l -= 1;
    }
    let mut r = x;
    while r + 1 < snap.cols && is_word(r + 1) {
        r += 1;
    }
    (l, r)
}

/// Find a URL spanning column `x` on row `y`: expand over the contiguous
/// non-whitespace token under the cursor, strip trailing punctuation, and
/// accept it only if it has a known scheme (or a leading `www.`, which gets an
/// `https://` prefix). Returns the openable URL, else `None`.
fn find_url_at(snap: &GridSnapshot, x: u16, y: u16) -> Option<String> {
    let cols = snap.cols;
    if cols == 0 || x >= cols {
        return None;
    }
    let char_at = |cx: u16| {
        snap.cell(cx, y)
            .and_then(|c| c.text.chars().next())
            .filter(|c| !c.is_whitespace())
    };
    char_at(x)?;
    let mut l = x;
    while l > 0 && char_at(l - 1).is_some() {
        l -= 1;
    }
    let mut r = x;
    while r + 1 < cols && char_at(r + 1).is_some() {
        r += 1;
    }
    let token: String = (l..=r).filter_map(char_at).collect();
    let trimmed = token.trim_end_matches(|c| {
        matches!(c, '.' | ',' | ')' | ']' | '}' | '>' | '"' | '\'' | ';' | ':')
    });
    if trimmed.starts_with("http://")
        || trimmed.starts_with("https://")
        || trimmed.starts_with("ftp://")
        || trimmed.starts_with("file://")
    {
        Some(trimmed.to_string())
    } else if trimmed.starts_with("www.") {
        Some(format!("https://{trimmed}"))
    } else {
        None
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
    use super::{extract_selection, find_url_at, word_bounds};
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
                    c.text = ch.to_string().into();
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

    #[test]
    fn word_bounds_expands_over_word_chars() {
        // "ls /usr/bin foo" on a 16-wide row.
        let s = grid(&["ls /usr/bin foo "], 16);
        // Click inside "ls" (col 0..1).
        assert_eq!(word_bounds(&s, 1, 0), (0, 1));
        // Click inside the path "/usr/bin" (cols 3..10) — slashes stay in-word.
        assert_eq!(word_bounds(&s, 6, 0), (3, 10));
        // Click on a space is its own (empty) selection.
        assert_eq!(word_bounds(&s, 2, 0), (2, 2));
        // Click inside "foo" (cols 12..14).
        assert_eq!(word_bounds(&s, 13, 0), (12, 14));
    }

    #[test]
    fn word_bounds_stops_at_bracket_punctuation() {
        let s = grid(&["a(bc)d", "     "], 6);
        // '(' and ')' are separators, so "bc" is bounded by them.
        assert_eq!(word_bounds(&s, 3, 0), (2, 3));
        // 'a' alone before '('.
        assert_eq!(word_bounds(&s, 0, 0), (0, 0));
    }

    #[test]
    fn detects_url_under_cursor() {
        let s = grid(&["see https://aka.ms/x now"], 24);
        // Click inside the URL (cols 4..21).
        assert_eq!(
            find_url_at(&s, 10, 0).as_deref(),
            Some("https://aka.ms/x")
        );
        // Click on a plain word → no URL.
        assert_eq!(find_url_at(&s, 1, 0), None); // "see"
        // Trailing period is stripped.
        let s2 = grid(&["go http://x.io.        "], 23);
        assert_eq!(find_url_at(&s2, 5, 0).as_deref(), Some("http://x.io"));
        // Bare www. gets an https prefix.
        let s3 = grid(&["www.example.com        "], 23);
        assert_eq!(
            find_url_at(&s3, 2, 0).as_deref(),
            Some("https://www.example.com")
        );
    }
}
