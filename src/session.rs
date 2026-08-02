//! One terminal session: a shell on a PTY, its libghostty-vt engine, and the
//! per-tab interaction state (selection, held mouse button). The [`App`](crate::app)
//! owns a `Vec<Session>` for tabs; cell metrics live in the app and are passed in.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::Result;
use eframe::egui;

use crate::config::{Config, OscColorReportFormat, ResizeOverlay};
use crate::decscusr::DecscusrScanner;
use crate::engine::{
    CursorShape, GhosttyVtEngine, GridSnapshot, KeyCode, KeyInput, KeyMods, MouseAction,
    MouseButton, MouseInput, RowText, TerminalEngine,
};
use crate::keybind::{Chord, Keymap};
use crate::osc7::Osc7Scanner;
use crate::search::{SearchHighlight, SearchState};
use crate::osc52::Osc52Scanner;
use crate::osc_color::{ColorQuery, OscColorScanner, Terminator};
use crate::profiles::Profile;
use crate::pty::Pty;

const DEFAULT_COLS: u16 = 80;
const DEFAULT_ROWS: u16 = 24;

pub struct Session {
    pty: Pty,
    engine: GhosttyVtEngine,
    /// Shared so the per-frame render copy is a refcount bump, not a deep clone
    /// of the cell grid (see `App::render_active`). `Arc` (not `Rc`) because the
    /// egui paint callback that borrows it must be `Send + Sync`.
    pub snapshot: Arc<GridSnapshot>,
    cols: u16,
    rows: u16,
    /// Eased on-screen scroll position, in **device pixels** above the live
    /// bottom (≥ 0), that chases `scroll_target_px`. Whole-line part drives the
    /// engine viewport; the sub-line remainder becomes `scroll_offset_px` for
    /// the renderer.
    scroll_px: f32,
    /// Immediate, un-smoothed scroll target (device px) set straight from raw
    /// wheel/keyboard input. `scroll_px` eases toward it each frame, giving a
    /// snappy-but-smooth response without egui's ~100ms input smoothing lag.
    scroll_target_px: f32,
    /// Sub-line vertical offset (device px, `0..cell_h`) the renderer shifts the
    /// grid down by, derived from `scroll_px` each frame.
    scroll_offset_px: f32,
    /// The engine viewport's current offset from the bottom, in whole lines —
    /// what we last commanded via `engine.scroll`. Lets us issue minimal
    /// relative deltas as `scroll_px` changes.
    engine_pin_lines: isize,
    /// Fractional-notch carry for the mouse-reporting wheel path (`handle_mouse`),
    /// so wheel events forwarded to vim/less are evenly paced.
    scroll_notch_accum: f32,
    sel_anchor: Option<(u16, u16)>,
    sel_head: Option<(u16, u16)>,
    mouse_down: Option<MouseButton>,
    /// False once the shell has exited (PTY output channel disconnected).
    alive: bool,
    /// Side parser for OSC 52 clipboard-set sequences in the PTY output.
    osc52: Osc52Scanner,
    /// Side parser tracking the shell's OSC 7 working directory, so a new split
    /// can inherit it.
    osc7: Osc7Scanner,
    /// Side parser tracking DECSCUSR, so the configured default cursor style is
    /// substituted only while the program hasn't picked its own shape.
    decscusr: DecscusrScanner,
    /// Side parser for OSC color *queries*, which the VT engine drops.
    osc_color: OscColorScanner,
    /// Precision of OSC color-query replies. Read at pump time rather than per
    /// frame, so a config reload must push it (see `apply_config`).
    osc_color_report_format: OscColorReportFormat,
    /// Configured default cursor shape (`cursor-style`), applied via `decscusr`.
    cursor_style: CursorShape,
    /// Configured default cursor blink (`cursor-style-blink`); `None` follows the
    /// program/terminal.
    cursor_style_blink: Option<bool>,
    /// One-shot flag set when a BEL rang during the last pump; consumed by
    /// `bell_flash_alpha` to (re)start the visual bell flash.
    ///
    /// Deliberately separate from `bell_effect_pending`: `bell_flash_alpha`
    /// *consumes* this flag, so a single flag shared with the audible path would
    /// silently drop either the beep or the flash depending on call order.
    bell_pending: bool,
    /// egui-time deadline of the active visual bell flash, or `None` when idle.
    bell_flash_until: Option<f64>,
    /// One-shot flag for the out-of-band bell effects (audible / attention /
    /// title), consumed by `take_bell_effect`.
    bell_effect_pending: bool,
    /// egui time the last out-of-band bell effect fired, for rate limiting.
    bell_effect_last: Option<f64>,
    /// True once this shell has ever emitted an OSC 133 prompt mark. Without it,
    /// "the cursor isn't on a prompt row" can't be told apart from "this shell
    /// never marks its prompts", and every close would look busy.
    saw_prompt_mark: bool,
    /// egui-time deadline of the grid-size overlay, or `None` when idle.
    resize_overlay_until: Option<f64>,
    /// False until this session's grid has been sized once. `resize-overlay =
    /// after-first` suppresses the overlay for that first layout, which matters
    /// because a session is created at a default size and immediately re-fit, so
    /// the resize edge always fires on the first frame.
    sized_once: bool,
    /// Configured resize-overlay mode and duration. Consulted inside `fit_grid`
    /// rather than per frame, so a config reload must push them (`apply_config`).
    resize_overlay: ResizeOverlay,
    resize_overlay_duration_ms: u64,
    /// Scrollback-search overlay state (query + matches + current) while open.
    search: Option<SearchState>,
    /// Screen text captured when the search opened. Re-searched on each keystroke
    /// so matching doesn't re-walk the whole grid every frame. Empty when closed.
    search_text: Vec<RowText>,
}

impl Session {
    /// Spawn `profile`'s shell and build its engine. `ctx` is cloned so the PTY
    /// reader thread can wake the UI when output arrives.
    pub fn new(
        ctx: &egui::Context,
        config: &Config,
        profile: &Profile,
        cwd: Option<&Path>,
    ) -> Result<Self> {
        let wake_ctx = ctx.clone();
        let pty = Pty::spawn(
            &profile.program,
            &profile.launch_args(),
            cwd,
            DEFAULT_COLS,
            DEFAULT_ROWS,
            move || wake_ctx.request_repaint(),
        )?;
        let mut engine = GhosttyVtEngine::new(DEFAULT_COLS, DEFAULT_ROWS, config.scrollback_limit)?;
        engine.apply_theme(config.fg, config.bg, &config.palette)?;
        engine.set_cursor_color(config.cursor)?;
        engine.set_bold_color(config.bold_color)?;
        engine.set_min_contrast(config.min_contrast)?;

        Ok(Self {
            pty,
            engine,
            snapshot: Arc::new(GridSnapshot::default()),
            cols: DEFAULT_COLS,
            rows: DEFAULT_ROWS,
            scroll_px: 0.0,
            scroll_target_px: 0.0,
            scroll_offset_px: 0.0,
            engine_pin_lines: 0,
            scroll_notch_accum: 0.0,
            sel_anchor: None,
            sel_head: None,
            mouse_down: None,
            alive: true,
            osc52: Osc52Scanner::new(),
            osc7: Osc7Scanner::new(),
            decscusr: DecscusrScanner::new(),
            osc_color: OscColorScanner::new(),
            osc_color_report_format: config.osc_color_report_format,
            cursor_style: config.cursor_style,
            cursor_style_blink: config.cursor_style_blink,
            bell_pending: false,
            bell_flash_until: None,
            bell_effect_pending: false,
            bell_effect_last: None,
            saw_prompt_mark: false,
            resize_overlay_until: None,
            sized_once: false,
            resize_overlay: config.resize_overlay,
            resize_overlay_duration_ms: config.resize_overlay_duration_ms,
            search: None,
            search_text: Vec::new(),
        })
    }

    /// Drain pending PTY output into the engine and flush responses back.
    /// Marks the session dead when the shell has exited (channel disconnected).
    pub fn pump_pty(&mut self) {
        use std::sync::mpsc::TryRecvError;
        let mut clipboard_sets: Vec<String> = Vec::new();
        let mut color_queries: Vec<(ColorQuery, Terminator)> = Vec::new();
        loop {
            match self.pty.output.try_recv() {
                Ok(chunk) => {
                    self.engine.write(&chunk);
                    self.osc52.feed(&chunk, &mut clipboard_sets);
                    self.osc7.feed(&chunk);
                    self.decscusr.feed(&chunk);
                    self.osc_color.feed(&chunk, &mut color_queries);
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
        let mut responses = self.engine.take_responses();
        // Answer the color queries the VT engine drops. Built *after* the whole
        // drain loop, so a set-then-query in one batch reports the new value, and
        // appended to the engine's own responses so everything we owe the program
        // leaves in one ordered write.
        if !color_queries.is_empty() {
            let (fg, bg, cursor) = self.engine.dynamic_colors();
            for (q, term) in color_queries.drain(..) {
                if let Some(color) = crate::osc_color::query_color(q, fg, bg, cursor) {
                    responses.extend_from_slice(&crate::osc_color::color_report(
                        q,
                        color,
                        self.osc_color_report_format,
                        term,
                    ));
                }
            }
        }
        if !responses.is_empty() {
            let _ = self.pty.write(&responses);
        }
        // A BEL during this pump arms both bell paths for the next frame. Two
        // flags, not one: each is consumed by a different caller (see the field
        // docs), and the visual flash is drawn per-pane while the audible /
        // attention / title effects fire once for the whole app.
        if self.engine.take_bell() {
            self.bell_pending = true;
            self.bell_effect_pending = true;
        }
    }

    /// Visual-bell flash intensity (1.0 → 0.0) for the frame at egui time `now`,
    /// or `None` when no flash is active. Consumes the one-shot bell flag from the
    /// last pump to (re)start the flash. The caller draws an overlay scaled by the
    /// returned value and keeps repainting while it stays `Some`.
    pub fn bell_flash_alpha(&mut self, now: f64) -> Option<f32> {
        const FLASH_SECS: f64 = 0.2;
        if std::mem::take(&mut self.bell_pending) {
            self.bell_flash_until = Some(now + FLASH_SECS);
        }
        let until = self.bell_flash_until?;
        if now >= until {
            self.bell_flash_until = None;
            return None;
        }
        Some((((until - now) / FLASH_SECS) as f32).clamp(0.0, 1.0))
    }

    /// The current grid size in cells — the resize overlay's label.
    pub fn grid_size(&self) -> (u16, u16) {
        (self.cols, self.rows)
    }

    /// Whether this pane looks like it's running something, for
    /// `confirm-close-surface = true`.
    ///
    /// `None` means "can't tell" — the shell emits no OSC 133 prompt marks, so
    /// there's no signal either way — which the caller treats as "confirm". A
    /// dead shell is never busy.
    pub fn looks_busy(&mut self) -> Option<bool> {
        if !self.alive {
            return Some(false);
        }
        let at_prompt = self.engine.cursor_at_prompt();
        if at_prompt == Some(true) {
            self.saw_prompt_mark = true;
        }
        match at_prompt {
            // Sitting on a prompt row: idle.
            Some(true) => Some(false),
            // Not on a prompt row. Only meaningful once we know this shell marks
            // its prompts at all; otherwise every pane would look busy forever.
            Some(false) if self.saw_prompt_mark => Some(true),
            _ => None,
        }
    }

    /// Opacity of the grid-size overlay at egui time `now` (1.0, then fading over
    /// the last stretch), or `None` when idle. Self-clearing, like
    /// [`Self::bell_flash_alpha`].
    ///
    /// *The fade tail is a giest nicety — Ghostty's overlay is a hard show/hide.*
    pub fn resize_overlay_alpha(&mut self, now: f64) -> Option<f32> {
        const FADE_SECS: f64 = 0.15;
        let until = self.resize_overlay_until?;
        if now >= until {
            self.resize_overlay_until = None;
            return None;
        }
        let left = until - now;
        Some(if left >= FADE_SECS {
            1.0
        } else {
            (left / FADE_SECS) as f32
        })
    }

    /// Whether an out-of-band bell effect (audible / attention / title) should
    /// fire at egui time `now`, consuming the one-shot flag from the last pump.
    ///
    /// Rate-limited, unlike the visual flash: a BEL storm (`yes $'\a'`) merely
    /// restarts the flash fade, but `MessageBeep` / `PlaySoundW` /
    /// `FlashWindowEx` are system-wide effects that would machine-gun.
    pub fn take_bell_effect(&mut self, now: f64) -> bool {
        if !std::mem::take(&mut self.bell_effect_pending) {
            return false;
        }
        if !bell_effect_due(self.bell_effect_last, now) {
            return false;
        }
        self.bell_effect_last = Some(now);
        true
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

    /// The shell's current working directory (reported via OSC 7), as a usable
    /// filesystem path. `None` if the shell never reported one.
    pub fn pwd(&self) -> Option<PathBuf> {
        osc7_to_path(self.osc7.pwd()?)
    }

    pub fn update_snapshot(&mut self) -> bool {
        // `make_mut` clones only if the previous frame's render still holds the
        // Arc; in steady state it's released by now, so this is a no-op bump.
        let snap = Arc::make_mut(&mut self.snapshot);
        // While mid-line (a sub-line offset is showing), also capture the row
        // the offset reveals above the viewport top.
        if self.scroll_offset_px > 0.0 {
            if self.engine.snapshot_over_row(&mut snap.over_row).is_err() {
                return false;
            }
        } else {
            snap.over_row.clear();
        }
        if self.engine.snapshot(snap).is_err() {
            return false;
        }
        // While the program is on its default cursor, substitute the configured
        // `cursor-style` (the engine resets DECSCUSR-default to a hardcoded block
        // and exposes no way to set the default shape). Re-applied each frame so
        // it survives the engine's clean-frame fast path. The renderer still
        // draws a hollow block for unfocused panes regardless of this shape.
        //
        // Blink mirrors Ghostty: an explicit `cursor-style-blink` wins (and, like
        // Ghostty, that makes the program's DEC mode 12 a no-op); when unset, the
        // default-cursor blink defaults on but follows the program's mode 12 —
        // both tracked by the scanner.
        if self.decscusr.is_default() {
            snap.cursor_shape = self.cursor_style;
            snap.cursor_blinking = self
                .cursor_style_blink
                .unwrap_or_else(|| self.decscusr.default_blink());
        }
        true
    }

    /// The sub-line vertical offset (device px) the renderer shifts this pane's
    /// grid down by for smooth scrolling. `0` when resting on a line boundary.
    pub fn scroll_offset_px(&self) -> f32 {
        self.scroll_offset_px
    }

    pub fn default_bg(&self) -> crate::engine::Rgb {
        self.snapshot.default_bg
    }

    /// Resize the grid (and PTY) to fit `area` (points) at scale `ppp`. `now` is
    /// egui time, used to schedule the grid-size overlay.
    pub fn fit_grid(&mut self, area: egui::Rect, ppp: f32, cell_w: f32, cell_h: f32, now: f64) {
        let (cols, rows) = grid_dims(area.width(), area.height(), ppp, cell_w, cell_h);
        if cols != self.cols || rows != self.rows {
            if crate::config::show_resize_overlay(self.resize_overlay, !self.sized_once) {
                self.resize_overlay_until =
                    Some(now + self.resize_overlay_duration_ms as f64 / 1000.0);
            }
            self.sized_once = true;
            self.cols = cols;
            self.rows = rows;
            let _ = self
                .engine
                .resize(cols, rows, (cell_w as u32, cell_h as u32));
            let _ = self.pty.resize(cols, rows);
            // Reflow invalidates the line-based pin; snap back to the live bottom
            // so scroll bookkeeping stays consistent.
            self.scroll_px = 0.0;
            self.scroll_offset_px = 0.0;
            self.engine_pin_lines = 0;
            self.engine.scroll_to_bottom();
            // A reflow renumbers rows/columns, so an open search's captured text
            // and matches are stale — recapture and re-run against the new grid.
            if self.search.is_some() {
                self.search_text = self.engine.screen_text();
                let target = self.viewport_bottom_row();
                if let Some(s) = self.search.as_mut() {
                    s.run(&self.search_text);
                    s.select_nearest(target);
                }
            }
        }
    }

    /// Convert a pointer position (points) to a clamped grid cell.
    pub fn pos_to_cell(
        &self,
        pos: egui::Pos2,
        rect: egui::Rect,
        ppp: f32,
        cw: f32,
        ch: f32,
    ) -> (u16, u16) {
        cell_from_pos(
            pos.x - rect.min.x,
            pos.y - rect.min.y,
            ppp,
            cw,
            ch,
            self.cols,
            self.rows,
        )
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

    /// Select the entire visible viewport (right-click menu "Select All").
    /// Viewport-scoped: giest's selection model is grid-cell based, so this does
    /// not span scrollback (matching double/triple-click selection).
    pub fn select_all(&mut self) {
        self.sel_anchor = Some((0, 0));
        self.sel_head = Some((self.cols.saturating_sub(1), self.rows.saturating_sub(1)));
    }

    /// Paste `text` into the shell, honoring bracketed-paste mode (menu Paste /
    /// middle-click). Mirrors the `Event::Paste` arm in `handle_input`.
    pub fn paste_str(&mut self, text: &str) {
        let encoded = self.engine.encode_paste(text);
        let _ = self.pty.write(&encoded);
    }

    /// Send a full terminal reset (RIS) to the shell (menu "Reset Terminal").
    pub fn reset(&mut self) {
        let _ = self.pty.write(b"\x1bc");
    }

    /// Re-apply runtime-changeable config to the live engine (command palette's
    /// "Reload Config"): the color theme, cursor color/style, bold-color policy,
    /// and minimum-contrast. The next frame's snapshot picks up the new values.
    /// `scrollback-limit` is fixed at engine creation and is intentionally *not*
    /// changed here.
    pub fn apply_config(&mut self, config: &Config) {
        let _ = self.engine.apply_theme(config.fg, config.bg, &config.palette);
        let _ = self.engine.set_cursor_color(config.cursor);
        let _ = self.engine.set_bold_color(config.bold_color);
        let _ = self.engine.set_min_contrast(config.min_contrast);
        self.cursor_style = config.cursor_style;
        self.cursor_style_blink = config.cursor_style_blink;
        // Consulted at pump/resize time rather than per frame, so these need an
        // explicit push here.
        self.osc_color_report_format = config.osc_color_report_format;
        self.resize_overlay = config.resize_overlay;
        self.resize_overlay_duration_ms = config.resize_overlay_duration_ms;
    }

    /// Scroll the viewport by `delta` lines (negative scrolls up into history),
    /// mirroring the `KeyAction::Scroll` arm in `handle_input`. The eased
    /// `animate_scroll` chases this target on the next frame.
    pub fn scroll_lines(&mut self, delta: isize, cell_h: f32) {
        self.scroll_target_px -= delta as f32 * cell_h;
    }

    /// Jump the viewport to the top of scrollback (`Shift+Home` equivalent).
    pub fn scroll_to_top(&mut self) {
        self.scroll_target_px = f32::INFINITY;
    }

    /// Jump the viewport to the live bottom (`Shift+End` equivalent).
    pub fn scroll_to_bottom_view(&mut self) {
        self.scroll_target_px = 0.0;
    }

    /// Scroll so the `delta`-th OSC 133 prompt above (`delta < 0`) or below
    /// (`delta > 0`) the viewport top sits at the top. A no-op if there is no
    /// such prompt (e.g. the shell emits no semantic-prompt marks). The eased
    /// `animate_scroll` carries the viewport there and clamps to scrollback.
    pub fn jump_to_prompt(&mut self, delta: isize, cell_h: f32) {
        if let Some(lines_up) = self.engine.jump_to_prompt(delta) {
            self.scroll_target_px = lines_up as f32 * cell_h;
        }
    }

    /// One page of scrolling in lines (grid height minus one), matching the page
    /// size `decide_key` uses for `Shift+PageUp/Down`.
    pub fn page_lines(&self) -> isize {
        self.rows.saturating_sub(1).max(1) as isize
    }

    // --- Scrollback search --------------------------------------------------

    /// Open the search overlay: capture the current screen (scrollback + viewport)
    /// as text so subsequent keystrokes re-search the snapshot rather than
    /// re-walking the grid. Idempotent — re-opening recaptures.
    pub fn open_search(&mut self) {
        self.search_text = self.engine.screen_text();
        self.search = Some(SearchState::new());
    }

    /// Close the search overlay and drop the captured text (highlights vanish).
    pub fn close_search(&mut self) {
        self.search = None;
        self.search_text = Vec::new();
    }

    /// Whether the search overlay is open.
    pub fn search_active(&self) -> bool {
        self.search.is_some()
    }

    /// The open search state (query, matches, current), for the overlay UI.
    pub fn search_state(&self) -> Option<&SearchState> {
        self.search.as_ref()
    }

    /// One-shot: whether the overlay just opened, clearing the flag — so the UI
    /// grabs keyboard focus exactly once.
    pub fn take_search_just_opened(&mut self) -> bool {
        match self.search.as_mut() {
            Some(s) => std::mem::replace(&mut s.just_opened, false),
            None => false,
        }
    }

    /// Set the query, re-match the captured text, point at the occurrence nearest
    /// the current viewport, and scroll there. No-op if the overlay is closed.
    pub fn set_search_query(&mut self, query: String, cell_h: f32) {
        let target = self.viewport_bottom_row();
        let current = {
            let Some(s) = self.search.as_mut() else {
                return;
            };
            s.query = query;
            s.run(&self.search_text);
            s.select_nearest(target);
            s.current_match()
        };
        if let Some(m) = current {
            self.scroll_to_match(m, cell_h);
        }
    }

    /// Step to the next (`forward`) / previous match and scroll to it.
    pub fn step_search(&mut self, forward: bool, cell_h: f32) {
        let current = {
            let Some(s) = self.search.as_mut() else {
                return;
            };
            s.step(forward);
            s.current_match()
        };
        if let Some(m) = current {
            self.scroll_to_match(m, cell_h);
        }
    }

    /// Toggle case-sensitivity and re-match, keeping the view near the same place.
    pub fn toggle_search_case(&mut self, cell_h: f32) {
        let target = self.viewport_bottom_row();
        let current = {
            let Some(s) = self.search.as_mut() else {
                return;
            };
            s.case_sensitive = !s.case_sensitive;
            s.run(&self.search_text);
            s.select_nearest(target);
            s.current_match()
        };
        if let Some(m) = current {
            self.scroll_to_match(m, cell_h);
        }
    }

    /// The matches visible in the current viewport, mapped to viewport rows for
    /// the renderer (the `current` one flagged). Empty when no search is open.
    /// Computed against the live scroll position; matches are tracked in absolute
    /// screen rows, which stay valid until scrollback eviction (heavy streaming
    /// during a search) — re-typing the query recaptures.
    pub fn search_highlights(&self) -> Vec<SearchHighlight> {
        let Some(s) = &self.search else {
            return Vec::new();
        };
        let vp_top = self.viewport_top_row();
        let rows = self.rows as u32;
        s.matches
            .iter()
            .enumerate()
            .filter(|(_, m)| m.row >= vp_top && m.row < vp_top + rows)
            .map(|(i, m)| SearchHighlight {
                row: (m.row - vp_top) as u16,
                col_start: m.col_start,
                col_end: m.col_end,
                current: i == s.current,
            })
            .collect()
    }

    /// Absolute screen row of the viewport's top, given the current scroll pin.
    fn viewport_top_row(&self) -> u32 {
        let scrollback = self.engine.scrollback_rows() as u32;
        scrollback.saturating_sub(self.engine_pin_lines.max(0) as u32)
    }

    /// Absolute screen row of the viewport's bottom-most line (search anchor).
    fn viewport_bottom_row(&self) -> u32 {
        self.viewport_top_row() + self.rows.saturating_sub(1) as u32
    }

    /// Scroll so match `m` sits about a third of the way down the viewport (for
    /// surrounding context), clamped to the live scrollback by `animate_scroll`.
    fn scroll_to_match(&mut self, m: crate::search::Match, cell_h: f32) {
        let scrollback = self.engine.scrollback_rows() as u32;
        let context = (self.rows / 3) as u32;
        let target_top = m.row.saturating_sub(context);
        let lines_above = scrollback.saturating_sub(target_top);
        self.scroll_target_px = lines_above as f32 * cell_h;
    }

    /// The URL under `cell`, if any (for Ctrl+click to open). An explicit OSC 8
    /// hyperlink (the program marked this cell clickable, possibly with display
    /// text that differs from the target) takes priority; otherwise fall back to
    /// detecting a bare URL in the rendered cell text.
    pub fn url_at(&self, cell: (u16, u16)) -> Option<String> {
        self.engine
            .hyperlink_at(cell.0, cell.1)
            .or_else(|| find_url_at(&self.snapshot, cell.0, cell.1))
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
    pub fn handle_input(
        &mut self,
        ctx: &egui::Context,
        tracking: bool,
        cell_h: f32,
        keymap: &Keymap,
    ) {
        let (events, ppp) = ctx.input(|i| (i.events.clone(), i.pixels_per_point().max(1.0)));
        let cell_h_pts = (cell_h / ppp).max(1.0);

        let mut bytes: Vec<u8> = Vec::new();
        for event in &events {
            match event {
                // Raw wheel deltas set the scroll *target* immediately (no egui
                // input smoothing). The mouse-reporting path (`tracking`)
                // forwards the wheel to the app in `handle_mouse` instead, so
                // only drive the local viewport when not tracking. Positive
                // `delta.y` moves content down = scroll up into history.
                egui::Event::MouseWheel { unit, delta, .. } if !tracking => {
                    let pts = match unit {
                        egui::MouseWheelUnit::Line => delta.y * LINE_SCROLL_PTS,
                        egui::MouseWheelUnit::Point => delta.y,
                        egui::MouseWheelUnit::Page => delta.y * self.rows as f32 * cell_h_pts,
                    };
                    self.scroll_target_px += pts * ppp;
                }
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
                    match copy_or_interrupt(self.selected_text()) {
                        CopyAction::Copy(text) => {
                            ctx.copy_text(text);
                            self.clear_selection();
                        }
                        CopyAction::Interrupt => bytes.push(0x03),
                    }
                }
                egui::Event::Key {
                    key,
                    pressed: true,
                    modifiers,
                    ..
                } => match decide_key(*key, modifiers, self.rows, keymap) {
                    KeyAction::Encode(input) => {
                        bytes.extend_from_slice(&self.engine.encode_key(&input));
                    }
                    // Keyboard scrolling feeds the same target. `delta` is in
                    // lines with the engine's sign (negative = up), so moving up
                    // adds to the target.
                    KeyAction::Scroll(delta) => self.scroll_target_px -= delta as f32 * cell_h,
                    KeyAction::ScrollTop => self.scroll_target_px = f32::INFINITY,
                    KeyAction::ScrollBottom => self.scroll_target_px = 0.0,
                    // Reserved app combos (Ctrl+Shift/Ctrl+Tab/Ctrl-zoom) and
                    // text-producing keys (handled by the `Text` event) emit no
                    // bytes here.
                    KeyAction::Swallow | KeyAction::Suppress => {}
                },
                _ => {}
            }
        }
        if !bytes.is_empty() {
            // Typing returns the viewport to the bottom (Ghostty behavior).
            self.scroll_target_px = 0.0;
            let _ = self.pty.write(&bytes);
        }

        // Ease the on-screen position toward the target and commit it to the
        // engine viewport + renderer, once per frame.
        self.animate_scroll(ctx, cell_h);
    }

    /// Drive the scroll easing for one frame without processing input. Used while
    /// a modal overlay (search) owns the keyboard so `handle_input` is skipped —
    /// the search's `scroll_to_match` still needs the viewport to ease to the
    /// match.
    pub fn tick_scroll(&mut self, ctx: &egui::Context, cell_h: f32) {
        self.animate_scroll(ctx, cell_h);
    }

    /// Ease `scroll_px` toward `scroll_target_px` (snappy exponential), then
    /// resolve it into a whole-line engine viewport position plus a sub-line
    /// render offset. Requests a repaint while still animating so frames don't
    /// starve (egui stops repainting once its own coarse scroll delta decays).
    /// Re-anchors to the live bottom when fully scrolled down (clearing pin
    /// drift from streamed output), and does zero work when idle at the bottom.
    fn animate_scroll(&mut self, ctx: &egui::Context, cell_h: f32) {
        // Idle at the live bottom: nothing to ease, clamp, or repaint.
        if self.scroll_target_px == 0.0 && self.scroll_px == 0.0 && self.engine_pin_lines == 0 {
            self.scroll_offset_px = 0.0;
            return;
        }
        let ch = cell_h.max(1.0);
        let scrollback = self.engine.scrollback_rows();
        let max = scrollback as f32 * ch;
        self.scroll_target_px = self.scroll_target_px.clamp(0.0, max);

        // Snappy ease (~90% in 60ms); snap and stop repainting once within ½px.
        let diff = self.scroll_target_px - self.scroll_px;
        if diff.abs() < 0.5 {
            self.scroll_px = self.scroll_target_px;
        } else {
            let dt = ctx.input(|i| i.stable_dt).clamp(1.0e-4, 1.0 / 30.0);
            self.scroll_px += diff * exp_smooth_factor(0.9, 0.06, dt);
            ctx.request_repaint();
        }

        let (base, frac) = scroll_split(self.scroll_px, ch, scrollback);
        if base <= 0 {
            if self.engine_pin_lines != 0 {
                self.engine.scroll_to_bottom();
                self.engine_pin_lines = 0;
            }
        } else {
            let delta = base - self.engine_pin_lines;
            if delta != 0 {
                self.engine.scroll(-delta);
                self.engine_pin_lines = base;
            }
        }
        self.scroll_offset_px = frac;
    }

    /// Report mouse events to the running app (only when it tracks the mouse).
    pub fn handle_mouse(
        &mut self,
        ctx: &egui::Context,
        rect: egui::Rect,
        ppp: f32,
        cw: f32,
        ch: f32,
    ) {
        let (events, scroll_y) = ctx.input(|i| (i.events.clone(), i.smooth_scroll_delta.y));
        let cell_px = (cw as u32, ch as u32);
        let screen_px = (
            (self.cols as f32 * cw) as u32,
            (self.rows as f32 * ch) as u32,
        );
        let to_px = |pos: egui::Pos2| -> (u32, u32) {
            (
                px_offset(pos.x - rect.min.x, ppp),
                px_offset(pos.y - rect.min.y, ppp),
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

        let (notches, button) = {
            let (n, accum) = notch_split(self.scroll_notch_accum, scroll_y, NOTCH_PTS);
            self.scroll_notch_accum = accum;
            let button = if n > 0 {
                MouseButton::WheelUp
            } else {
                MouseButton::WheelDown
            };
            // Cap per frame so a fast fling can't flood the PTY with reports.
            ((n.unsigned_abs()).min(MAX_WHEEL_NOTCHES_PER_FRAME), button)
        };
        if notches > 0 {
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

/// Read text from the system clipboard (best-effort). Windows has no separate
/// PRIMARY selection, so middle-click / menu paste reads this.
pub fn read_clipboard() -> Option<String> {
    arboard::Clipboard::new().ok()?.get_text().ok()
}

/// Translate egui modifiers to backend-neutral key modifiers. `pub(crate)` so
/// the app can build keybind chords from live key events with identical
/// modifier semantics (notably `command` folding into `ctrl`).
pub(crate) fn key_mods(m: &egui::Modifiers) -> KeyMods {
    KeyMods {
        shift: m.shift,
        ctrl: m.ctrl || m.command,
        alt: m.alt,
        sup: false,
    }
}

/// What a key event resolves to once the host's reserved combos and viewport
/// shortcuts are applied. The byte encoding itself is left to libghostty's
/// encoder (the `Encode` arm); everything else is giest's own gating.
#[derive(Clone, Debug, PartialEq, Eq)]
enum KeyAction {
    /// Hand this neutral key event to the engine's encoder for the PTY.
    Encode(KeyInput),
    /// Scroll the viewport by `delta` lines (Shift+PageUp/PageDown).
    Scroll(isize),
    /// Jump the viewport to the top of scrollback (Shift+Home).
    ScrollTop,
    /// Jump the viewport to the bottom (Shift+End).
    ScrollBottom,
    /// A combo reserved by the app (Ctrl+Shift namespace, Ctrl+Tab, Ctrl +/-/0
    /// font zoom): the shell never sees it.
    Swallow,
    /// A text-producing key (or one we don't map): no bytes here — the matching
    /// egui `Text` event carries the character.
    Suppress,
}

/// Decide what a pressed key does, given the live modifiers, the grid height
/// (for page scrolling), and the active `keymap`. Pure: depends only on its
/// arguments (the keymap is config-derived data), so every gating branch is
/// unit-testable. Mirrors the Windows-Terminal/Ghostty host bindings.
fn decide_key(
    key: egui::Key,
    modifiers: &egui::Modifiers,
    rows: u16,
    keymap: &Keymap,
) -> KeyAction {
    let Some(code) = map_egui_key(key) else {
        return KeyAction::Suppress;
    };
    // Anything bound to an app action is handled by `App::handle_shortcuts`, so
    // the shell must never see it — swallow it here. This covers custom keybinds
    // that fall outside the structural namespaces below (e.g. `ctrl+a`); the
    // default binds also match, redundantly with those namespaces.
    if keymap
        .lookup(&Chord {
            mods: key_mods(modifiers),
            code,
        })
        .is_some()
    {
        return KeyAction::Swallow;
    }
    // Ctrl+Shift is the app's namespace; the shell never sees it. The clipboard
    // combos (Ctrl+Shift+C/V/X) arrive as Copy/Paste/Cut events handled
    // elsewhere, so here we swallow every other Ctrl+Shift combo (tab/split).
    if modifiers.ctrl && modifiers.shift {
        return KeyAction::Swallow;
    }
    if modifiers.ctrl && code == KeyCode::Tab {
        return KeyAction::Swallow;
    }
    // Ctrl+Alt+arrow moves focus between splits (goto_split:dir) — app-reserved.
    // Kept arrow-specific so Ctrl+Alt (AltGr) text entry is otherwise untouched.
    if modifiers.ctrl
        && modifiers.alt
        && matches!(
            code,
            KeyCode::ArrowLeft | KeyCode::ArrowRight | KeyCode::ArrowUp | KeyCode::ArrowDown
        )
    {
        return KeyAction::Swallow;
    }
    // Alt+1..9 jump to a tab (Ghostty's goto_tab) — reserved by the app.
    if modifiers.alt
        && !modifiers.ctrl
        && !modifiers.shift
        && matches!(
            code,
            KeyCode::Digit1
                | KeyCode::Digit2
                | KeyCode::Digit3
                | KeyCode::Digit4
                | KeyCode::Digit5
                | KeyCode::Digit6
                | KeyCode::Digit7
                | KeyCode::Digit8
                | KeyCode::Digit9
        )
    {
        return KeyAction::Swallow;
    }
    // Shift+PageUp/Down scroll by a page; Shift+Home/End jump to the top/bottom
    // of scrollback. These drive the viewport instead of going to the shell.
    if modifiers.shift
        && matches!(
            code,
            KeyCode::PageUp | KeyCode::PageDown | KeyCode::Home | KeyCode::End
        )
    {
        let page = rows.saturating_sub(1).max(1) as isize;
        return match code {
            KeyCode::PageUp => KeyAction::Scroll(-page),
            KeyCode::PageDown => KeyAction::Scroll(page),
            KeyCode::Home => KeyAction::ScrollTop,
            KeyCode::End => KeyAction::ScrollBottom,
            _ => unreachable!(),
        };
    }
    // Ctrl +/-/0 are reserved by the app for font zoom.
    if modifiers.ctrl
        && !modifiers.shift
        && matches!(code, KeyCode::Equal | KeyCode::Minus | KeyCode::Digit0)
    {
        return KeyAction::Swallow;
    }
    let mods = key_mods(modifiers);
    if is_text_producing(code) && !mods.ctrl && !mods.alt {
        return KeyAction::Suppress;
    }
    KeyAction::Encode(KeyInput {
        code,
        text: None,
        mods,
        press: true,
    })
}

/// What a Copy/Cut event does, given the current selection text.
#[derive(Clone, Debug, PartialEq, Eq)]
enum CopyAction {
    /// Copy this text to the clipboard (and clear the selection).
    Copy(String),
    /// No selection: send SIGINT (`0x03`) to the shell, like Windows Terminal.
    Interrupt,
}

/// Resolve a Copy/Cut event: copy the selection if there is one, else interrupt.
fn copy_or_interrupt(selection: Option<String>) -> CopyAction {
    match selection {
        Some(text) => CopyAction::Copy(text),
        None => CopyAction::Interrupt,
    }
}

/// Compute the grid dimensions (cols, rows) that fit `width`×`height` points at
/// scale `ppp` given the cell size in pixels. Always at least 1×1.
/// Minimum spacing between out-of-band bell effects. Long enough that a BEL
/// storm can't machine-gun the speaker or the taskbar, short enough that
/// deliberate bells a moment apart are each heard.
const BELL_RATE_LIMIT_SECS: f64 = 0.25;

/// Whether a bell effect that last fired at `last` may fire again at `now`.
/// `None` (never fired) always passes.
fn bell_effect_due(last: Option<f64>, now: f64) -> bool {
    match last {
        None => true,
        Some(prev) => now - prev >= BELL_RATE_LIMIT_SECS,
    }
}

fn grid_dims(width_pts: f32, height_pts: f32, ppp: f32, cell_w: f32, cell_h: f32) -> (u16, u16) {
    let cols = ((width_pts * ppp / cell_w).floor() as u16).max(1);
    let rows = ((height_pts * ppp / cell_h).floor() as u16).max(1);
    (cols, rows)
}

/// Map a pointer offset (points, relative to the grid's top-left) to a grid
/// cell, clamped to the last valid cell. Negative offsets clamp to cell 0.
fn cell_from_pos(
    rel_x: f32,
    rel_y: f32,
    ppp: f32,
    cw: f32,
    ch: f32,
    cols: u16,
    rows: u16,
) -> (u16, u16) {
    let x = (rel_x * ppp / cw).floor().max(0.0) as u16;
    let y = (rel_y * ppp / ch).floor().max(0.0) as u16;
    (x.min(cols.saturating_sub(1)), y.min(rows.saturating_sub(1)))
}

/// Points of smooth-scroll travel per reported wheel "notch" on the
/// mouse-reporting path. Chosen to roughly match a physical wheel detent.
const NOTCH_PTS: f32 = 40.0;
/// Cap on wheel notches reported to the app in a single frame, so a fast fling
/// can't flood the PTY.
const MAX_WHEEL_NOTCHES_PER_FRAME: usize = 8;

/// Points of scroll travel per wheel line (egui's default `line_scroll_speed`),
/// applied to raw `MouseWheelUnit::Line` deltas so wheel feel matches egui.
const LINE_SCROLL_PTS: f32 = 40.0;

/// Per-frame easing fraction to reach `reach` of the remaining distance in
/// `secs` seconds given frame time `dt` — `1 - (1-reach)^(dt/secs)`. Mirrors
/// egui's `exponential_smooth_factor`; frame-rate independent.
fn exp_smooth_factor(reach: f32, secs: f32, dt: f32) -> f32 {
    1.0 - (1.0 - reach).powf(dt / secs.max(1.0e-4))
}

/// Split a continuous pixel scroll position into a whole-line engine viewport
/// offset (`base`, lines above the bottom) and a sub-line render remainder
/// (`frac`, device px in `0..cell_h`). `scroll_px` is clamped to the available
/// scrollback first; `frac` stays whole-pixel (since `scroll_px` is rounded and
/// `cell_h` is integer) so glyph quads remain crisp during a scroll.
fn scroll_split(scroll_px: f32, cell_h: f32, scrollback_rows: usize) -> (isize, f32) {
    let ch = cell_h.max(1.0);
    let max = scrollback_rows as f32 * ch;
    let q = scroll_px.clamp(0.0, max).round();
    let base = (q / ch).floor() as isize;
    let frac = q - base as f32 * ch;
    (base, frac)
}

/// Accumulate a fractional smooth-scroll delta into whole wheel notches,
/// retaining the remainder so no motion is lost across frames. Returns the
/// signed notch count to report this frame (positive = up) and the carry.
fn notch_split(accum: f32, scroll_y: f32, notch_pts: f32) -> (isize, f32) {
    let acc = (accum + scroll_y / notch_pts.max(1.0)).clamp(-1.0e4, 1.0e4);
    let whole = acc.trunc();
    (whole as isize, acc - whole)
}

/// Convert a pointer offset (points, relative to the grid origin) to a
/// non-negative pixel coordinate at scale `ppp`.
fn px_offset(rel: f32, ppp: f32) -> u32 {
    (rel * ppp).max(0.0) as u32
}

/// Whether `ch` counts as part of a word for double-click selection. Word
/// boundaries are whitespace and a small set of shell/bracket punctuation;
/// path/URL characters (`/ . - _ : @ ~`) stay part of the word so a whole path
/// or flag selects in one double-click.
fn is_word_char(ch: char) -> bool {
    !ch.is_whitespace()
        && !matches!(
            ch,
            '(' | ')'
                | '['
                | ']'
                | '{'
                | '}'
                | '<'
                | '>'
                | '|'
                | '&'
                | ';'
                | ','
                | '"'
                | '\''
                | '`'
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
        matches!(
            c,
            '.' | ',' | ')' | ']' | '}' | '>' | '"' | '\'' | ';' | ':'
        )
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
/// Map an egui logical key to a backend-neutral [`KeyCode`]. `pub(crate)` so the
/// app can resolve keybind chords from live key events with the same mapping the
/// PTY-encode path uses.
pub(crate) fn map_egui_key(key: egui::Key) -> Option<KeyCode> {
    use KeyCode as C;
    use egui::Key as K;
    Some(match key {
        K::A => C::A,
        K::B => C::B,
        K::C => C::C,
        K::D => C::D,
        K::E => C::E,
        K::F => C::F,
        K::G => C::G,
        K::H => C::H,
        K::I => C::I,
        K::J => C::J,
        K::K => C::K,
        K::L => C::L,
        K::M => C::M,
        K::N => C::N,
        K::O => C::O,
        K::P => C::P,
        K::Q => C::Q,
        K::R => C::R,
        K::S => C::S,
        K::T => C::T,
        K::U => C::U,
        K::V => C::V,
        K::W => C::W,
        K::X => C::X,
        K::Y => C::Y,
        K::Z => C::Z,
        K::Num0 => C::Digit0,
        K::Num1 => C::Digit1,
        K::Num2 => C::Digit2,
        K::Num3 => C::Digit3,
        K::Num4 => C::Digit4,
        K::Num5 => C::Digit5,
        K::Num6 => C::Digit6,
        K::Num7 => C::Digit7,
        K::Num8 => C::Digit8,
        K::Num9 => C::Digit9,
        K::Enter => C::Enter,
        K::Tab => C::Tab,
        K::Backspace => C::Backspace,
        K::Escape => C::Escape,
        K::Space => C::Space,
        K::Delete => C::Delete,
        K::Insert => C::Insert,
        K::Home => C::Home,
        K::End => C::End,
        K::PageUp => C::PageUp,
        K::PageDown => C::PageDown,
        K::ArrowUp => C::ArrowUp,
        K::ArrowDown => C::ArrowDown,
        K::ArrowLeft => C::ArrowLeft,
        K::ArrowRight => C::ArrowRight,
        K::F1 => C::F1,
        K::F2 => C::F2,
        K::F3 => C::F3,
        K::F4 => C::F4,
        K::F5 => C::F5,
        K::F6 => C::F6,
        K::F7 => C::F7,
        K::F8 => C::F8,
        K::F9 => C::F9,
        K::F10 => C::F10,
        K::F11 => C::F11,
        K::F12 => C::F12,
        K::Minus => C::Minus,
        K::Equals => C::Equal,
        K::OpenBracket => C::BracketLeft,
        K::CloseBracket => C::BracketRight,
        K::Backslash => C::Backslash,
        K::Semicolon => C::Semicolon,
        K::Quote => C::Quote,
        K::Backtick => C::Backquote,
        K::Comma => C::Comma,
        K::Period => C::Period,
        K::Slash => C::Slash,
        _ => return None,
    })
}

/// Convert the value reported via OSC 7 into a filesystem path. The canonical
/// form is a `file://HOST/PATH` URI, but we tolerate a missing scheme, a missing
/// host, percent-encoding, and Windows backslashes (cmd emits `file://HOST/C:\d`).
/// Returns `None` for empty/non-absolute values.
fn osc7_to_path(raw: &str) -> Option<PathBuf> {
    let raw = raw.trim();
    if raw.is_empty() {
        return None;
    }
    // Strip the scheme and host: after `file://` (or a bare `//`), everything up
    // to the first `/` is the host, which we drop, keeping the path from `/`.
    let path = if let Some(rest) = raw.strip_prefix("file:") {
        let rest = rest.strip_prefix("//").unwrap_or(rest);
        match rest.find('/') {
            Some(i) => &rest[i..],
            // No path separator after the host — nothing usable.
            None => return None,
        }
    } else {
        raw
    };
    let decoded = percent_encoding::percent_decode_str(path)
        .decode_utf8()
        .ok()?;
    // A Windows file URI path is `/C:/...`; drop the leading slash before the
    // drive letter so it becomes a real path.
    let bytes = decoded.as_bytes();
    let trimmed = if bytes.len() >= 3
        && bytes[0] == b'/'
        && bytes[1].is_ascii_alphabetic()
        && bytes[2] == b':'
    {
        &decoded[1..]
    } else {
        &decoded[..]
    };
    if trimmed.is_empty() {
        return None;
    }
    Some(PathBuf::from(trimmed))
}

/// Keys that also produce an egui `Text` event when pressed without Ctrl/Alt.
fn is_text_producing(code: KeyCode) -> bool {
    use KeyCode::*;
    !matches!(
        code,
        Enter
            | Tab
            | Backspace
            | Escape
            | Delete
            | Insert
            | Home
            | End
            | PageUp
            | PageDown
            | ArrowUp
            | ArrowDown
            | ArrowLeft
            | ArrowRight
            | F1
            | F2
            | F3
            | F4
            | F5
            | F6
            | F7
            | F8
            | F9
            | F10
            | F11
            | F12
    )
}

#[cfg(test)]
mod tests {
    use super::{
        CopyAction, KeyAction, bell_effect_due, cell_from_pos, copy_or_interrupt,
        extract_selection, find_url_at, grid_dims, notch_split, osc7_to_path, px_offset,
        scroll_split, word_bounds,
    };
    use crate::engine::{Cell, GridSnapshot, KeyCode, KeyInput, KeyMods};
    use crate::keybind::Keymap;
    use eframe::egui;
    use std::path::PathBuf;

    /// Drive the real `decide_key` with the built-in default keymap, so the
    /// existing 3-argument call sites stay unchanged. The default keymap binds
    /// exactly the host shortcuts the namespace gating already reserves, so it
    /// does not alter any of these assertions.
    fn decide_key(key: egui::Key, modifiers: &egui::Modifiers, rows: u16) -> KeyAction {
        super::decide_key(key, modifiers, rows, &Keymap::default())
    }

    #[test]
    fn bell_effect_due_rate_limits_a_storm() {
        // The first bell always rings.
        assert!(bell_effect_due(None, 10.0));
        // A second one a frame later is swallowed…
        assert!(!bell_effect_due(Some(10.0), 10.016));
        assert!(!bell_effect_due(Some(10.0), 10.2));
        // …but one past the limit rings again.
        assert!(bell_effect_due(Some(10.0), 10.25));
        assert!(bell_effect_due(Some(10.0), 11.0));
    }

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
    fn osc7_parses_file_uris_and_paths() {
        let p = |s: &str| osc7_to_path(s);
        // file:// URI with a host, forward slashes.
        assert_eq!(
            p("file://HOST/C:/Users/foo"),
            Some(PathBuf::from("C:/Users/foo"))
        );
        // Empty host (`file:///...`).
        assert_eq!(
            p("file:///C:/Users/foo"),
            Some(PathBuf::from("C:/Users/foo"))
        );
        // cmd's backslash form round-trips.
        assert_eq!(
            p("file://HOST/C:\\Users\\foo"),
            Some(PathBuf::from("C:\\Users\\foo"))
        );
        // Percent-encoded spaces are decoded.
        assert_eq!(
            p("file://HOST/C:/Program%20Files"),
            Some(PathBuf::from("C:/Program Files"))
        );
        // A bare absolute path with no scheme is accepted as-is.
        assert_eq!(p("C:\\Users\\foo"), Some(PathBuf::from("C:\\Users\\foo")));
        // Empty / unusable values yield None.
        assert_eq!(p(""), None);
        assert_eq!(p("   "), None);
        assert_eq!(p("file://HOST"), None);
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
        assert_eq!(find_url_at(&s, 10, 0).as_deref(), Some("https://aka.ms/x"));
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

    // --- Input gating (decide_key / copy_or_interrupt) ---------------------

    fn mods(ctrl: bool, shift: bool, alt: bool) -> egui::Modifiers {
        egui::Modifiers {
            alt,
            ctrl,
            shift,
            mac_cmd: false,
            command: false,
        }
    }

    #[test]
    fn ctrl_shift_combos_are_app_namespace() {
        // Ctrl+Shift+D (split) and any other Ctrl+Shift combo never reach the shell.
        assert_eq!(
            decide_key(egui::Key::D, &mods(true, true, false), 24),
            KeyAction::Swallow
        );
        assert_eq!(
            decide_key(egui::Key::Num1, &mods(true, true, false), 24),
            KeyAction::Swallow
        );
    }

    #[test]
    fn ctrl_tab_is_swallowed() {
        assert_eq!(
            decide_key(egui::Key::Tab, &mods(true, false, false), 24),
            KeyAction::Swallow
        );
    }

    #[test]
    fn shift_scrollback_keys_drive_viewport_not_shell() {
        // rows=25 → a page is rows-1 = 24 lines.
        assert_eq!(
            decide_key(egui::Key::PageUp, &mods(false, true, false), 25),
            KeyAction::Scroll(-24)
        );
        assert_eq!(
            decide_key(egui::Key::PageDown, &mods(false, true, false), 25),
            KeyAction::Scroll(24)
        );
        assert_eq!(
            decide_key(egui::Key::Home, &mods(false, true, false), 25),
            KeyAction::ScrollTop
        );
        assert_eq!(
            decide_key(egui::Key::End, &mods(false, true, false), 25),
            KeyAction::ScrollBottom
        );
    }

    #[test]
    fn ctrl_zoom_keys_are_swallowed() {
        for k in [egui::Key::Equals, egui::Key::Minus, egui::Key::Num0] {
            assert_eq!(
                decide_key(k, &mods(true, false, false), 24),
                KeyAction::Swallow,
                "Ctrl+{k:?} should be reserved for font zoom"
            );
        }
    }

    #[test]
    fn plain_printable_defers_to_text_event() {
        // A bare letter is produced as an egui Text event, so the key path emits nothing.
        assert_eq!(
            decide_key(egui::Key::A, &mods(false, false, false), 24),
            KeyAction::Suppress
        );
    }

    #[test]
    fn keymap_reserves_custom_out_of_namespace_bind() {
        // With the built-in keymap, Ctrl+A is encoded to the shell (^A).
        assert!(matches!(
            decide_key(egui::Key::A, &mods(true, false, false), 24),
            KeyAction::Encode(_)
        ));
        // Binding Ctrl+A to an app action reserves it: decide_key now swallows it
        // so the shell never sees the key (the app runs the action instead).
        let km = Keymap::from_config(&[("ctrl+a".to_string(), "new_tab".to_string())]);
        assert_eq!(
            super::decide_key(egui::Key::A, &mods(true, false, false), 24, &km),
            KeyAction::Swallow
        );
    }

    #[test]
    fn ctrl_combo_and_control_keys_encode() {
        assert_eq!(
            decide_key(egui::Key::C, &mods(true, false, false), 24),
            KeyAction::Encode(KeyInput {
                code: KeyCode::C,
                text: None,
                mods: KeyMods {
                    ctrl: true,
                    ..Default::default()
                },
                press: true,
            })
        );
        // Non-text-producing keys encode even with no modifiers.
        assert!(matches!(
            decide_key(egui::Key::Enter, &mods(false, false, false), 24),
            KeyAction::Encode(_)
        ));
        assert!(matches!(
            decide_key(egui::Key::ArrowUp, &mods(false, false, false), 24),
            KeyAction::Encode(_)
        ));
    }

    #[test]
    fn copy_or_interrupt_branches() {
        assert_eq!(
            copy_or_interrupt(Some("sel".to_string())),
            CopyAction::Copy("sel".to_string())
        );
        assert_eq!(copy_or_interrupt(None), CopyAction::Interrupt);
    }

    // --- Coordinate / resize math ------------------------------------------

    #[test]
    fn grid_dims_floors_and_scales() {
        // 800pt × 240pt, 10×20px cells, ppp 1 → 80×12.
        assert_eq!(grid_dims(800.0, 240.0, 1.0, 10.0, 20.0), (80, 12));
        // ppp 2 doubles both dimensions.
        assert_eq!(grid_dims(800.0, 240.0, 2.0, 10.0, 20.0), (160, 24));
        // A sub-cell area still yields a 1×1 grid.
        assert_eq!(grid_dims(1.0, 1.0, 1.0, 10.0, 20.0), (1, 1));
    }

    #[test]
    fn cell_from_pos_clamps_edges_and_negatives() {
        // Inside the grid: (25,30)pt at 10×20px → cell (2,1).
        assert_eq!(cell_from_pos(25.0, 30.0, 1.0, 10.0, 20.0, 80, 24), (2, 1));
        // Negative offsets clamp to cell (0,0).
        assert_eq!(cell_from_pos(-5.0, -9.0, 1.0, 10.0, 20.0, 80, 24), (0, 0));
        // Far beyond the grid clamps to the last cell.
        assert_eq!(
            cell_from_pos(1.0e6, 1.0e6, 1.0, 10.0, 20.0, 80, 24),
            (79, 23)
        );
    }

    #[test]
    fn scroll_split_quantizes_to_line_plus_pixel_remainder() {
        // ch=14, 100 lines of scrollback (max scroll = 1400px).
        // At the bottom: no line offset, no sub-line remainder.
        assert_eq!(scroll_split(0.0, 14.0, 100), (0, 0.0));
        // A 2px scroll stays on line 0 with a 2px sub-line offset.
        assert_eq!(scroll_split(2.0, 14.0, 100), (0, 2.0));
        // Exactly one line: offset 1, remainder 0.
        assert_eq!(scroll_split(14.0, 14.0, 100), (1, 0.0));
        // One line plus 2px.
        assert_eq!(scroll_split(16.0, 14.0, 100), (1, 2.0));
        // The remainder is always whole-pixel and < cell_h (kept crisp).
        let (_, frac) = scroll_split(13.6, 14.0, 100);
        assert_eq!(frac, 0.0); // 13.6 rounds to 14 -> line 1, frac 0
        // Clamped to the available scrollback (can't scroll past the top).
        assert_eq!(scroll_split(1.0e6, 14.0, 100), (100, 0.0));
        // No scrollback -> always pinned to the bottom.
        assert_eq!(scroll_split(500.0, 14.0, 0), (0, 0.0));
    }

    #[test]
    fn notch_split_accumulates_without_losing_motion() {
        // Sub-notch deltas accrue rather than rounding to zero/one each frame.
        let (n1, a1) = notch_split(0.0, 10.0, 40.0); // 0.25 notch
        assert_eq!(n1, 0);
        let (n2, a2) = notch_split(a1, 10.0, 40.0); // 0.5
        assert_eq!(n2, 0);
        let (n3, a3) = notch_split(a2, 10.0, 40.0); // 0.75
        assert_eq!(n3, 0);
        let (n4, _) = notch_split(a3, 10.0, 40.0); // 1.0 -> one notch
        assert_eq!(n4, 1);
        // Four 10pt deltas == one 40pt delta == one notch.
        assert_eq!(notch_split(0.0, 40.0, 40.0).0, 1);
        // Downward scroll yields a negative notch count.
        assert_eq!(notch_split(0.0, -80.0, 40.0).0, -2);
    }

    #[test]
    fn px_offset_never_negative() {
        assert_eq!(px_offset(10.0, 2.0), 20);
        assert_eq!(px_offset(-3.0, 2.0), 0);
    }
}
