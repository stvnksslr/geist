//! One terminal session: a shell on a PTY, its libghostty-vt engine, and the
//! per-tab interaction state (selection, held mouse button). The [`App`](crate::app)
//! owns a `Vec<Session>` for tabs; cell metrics live in the app and are passed in.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::Result;
use eframe::egui;

use crate::config::{
    self, ClipboardAccess, ClipboardPolicy, Config, OscColorReportFormat, ResizeOverlay,
};
use crate::decscusr::DecscusrScanner;
use crate::engine::{
    CursorShape, GhosttyVtEngine, GridSnapshot, KeyCode, KeyInput, KeyMods, MouseAction,
    MouseButton, MouseInput, RowText, SelectKind, TerminalEngine,
};
use crate::keybind::{Chord, Keymap, Lookup};
use crate::osc7::Osc7Scanner;
use crate::search::{SearchHighlight, SearchState};
use crate::osc52::{Osc52, Osc52Scanner};
use crate::osc_color::{ColorQuery, OscColorScanner, Terminator};
use crate::osc_notify::{Notification, Osc9, OscNotifyScanner};
use crate::osc133::{Mark, Osc133Scanner};
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
    // NOTE: the selection itself lives in the **engine**, not here. It is held as
    // a tracked reference the VT engine follows through scrolling, scrollback
    // eviction and reflow, which viewport (col,row) pairs could never do — see
    // `TerminalEngine::selection_begin`. Only the interaction state below is the
    // session's.
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
    /// Side parser for OSC 9 / OSC 777 desktop-notification requests.
    osc_notify: OscNotifyScanner,
    /// Side parser for OSC 133 `C`/`D` command marks. The engine applies the
    /// `A`/`B` prompt marks to the screen, but `D`'s exit code never lands on a
    /// cell, so it has to be read off the stream.
    osc133: Osc133Scanner,
    /// When the running command started, if one is running. Set by a `C` mark or
    /// — since neither of giest's shell hooks can emit one — by the Enter that
    /// submitted it. `Instant`, not egui time: `pump_pty` has no `Context`, and a
    /// duration wants a monotonic clock anyway.
    command_started: Option<std::time::Instant>,
    /// Commands that finished since the app last drained them.
    pending_command_finish: Vec<CommandFinish>,
    /// Notifications requested since the app last drained them. Held on the
    /// session (like `pending_clipboard`) rather than pushed straight out: the
    /// pump has no `Context`, no window handle, and no idea whether the window
    /// is focused — all of which the app-level drain needs.
    pending_notifications: Vec<Notification>,
    /// This pane's latest `OSC 9;4` progress state, or `None` if it has never
    /// reported one. **Latest wins rather than queued**: a build emits hundreds
    /// of these and only the current one means anything.
    progress: Option<crate::taskbar::Progress>,
    /// Whether [`Self::progress`] changed since the app last read it.
    progress_dirty: bool,
    /// `desktop-notifications`. Checked at pump time so a disabled config costs
    /// nothing per chunk; a reload must push it (see `apply_config`).
    desktop_notifications: bool,
    /// `progress-style`. Same reasoning as above.
    progress_style: bool,
    /// `mouse-reporting`, plus whatever `toggle_mouse_reporting` has done to it
    /// since. Per-pane, because the toggle is a per-surface escape hatch.
    mouse_reporting: bool,
    /// `mouse-scroll-multiplier`, read in the wheel path.
    mouse_scroll_multiplier: crate::config::MouseScrollMultiplier,
    /// `toggle_readonly`: refuse to send keyboard input to the shell. Per-pane
    /// and runtime-only — there is no config key for it upstream either.
    readonly: bool,
    /// IME composition in progress, drawn at the cursor by `render_active`.
    /// Per-pane so a split switch mid-composition can't move it onto another
    /// pane's cursor.
    preedit: crate::ime::Preedit,
    /// `scroll-to-bottom`, read on keystroke and on new output.
    scroll_to_bottom: crate::config::ScrollToBottom,
    /// Ghostty `scrollback-compression`; drives [`Session::idle_work`].
    scrollback_compression: bool,
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
    /// Seconds accumulated toward the next drag-autoscroll row. Ghostty ticks a
    /// **15 ms timer** while the pointer is held past the pane's edge; ticking
    /// once per *frame* instead would scroll at the refresh rate, which is
    /// upstream's speed at 60 Hz and more than twice it at 144 Hz.
    autoscroll_accum: f32,
    /// Explicit pane title from `set_surface_title`, winning over the program's
    /// own OSC 0/2 title until cleared with an empty value.
    title_override: Option<String>,
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
    /// Word-boundary codepoints for double-click selection
    /// (`selection-word-chars`); empty = the engine's own defaults. Held here
    /// rather than read from a `Config` because a selection happens on a click,
    /// not in a pass that has the config to hand.
    selection_word_chars: Vec<char>,
    /// `selection-clear-on-typing` / `-on-copy`. Consulted in the input loop, so
    /// they are pushed here rather than read from a `Config` (see `apply_config`).
    selection_clear_on_typing: bool,
    selection_clear_on_copy: bool,
    /// Clipboard permissions and paste protection. Consulted at paste and pump
    /// time rather than per frame, so a config reload must push it
    /// (see `apply_config`).
    clipboard: ClipboardPolicy,
    /// A clipboard operation waiting on the user's answer. At most one at a
    /// time: a second request while one is pending is dropped, so a program
    /// spamming OSC 52 can't queue up a stack of dialogs.
    pending_clipboard: Option<ClipboardRequest>,
    /// egui-time deadline of the auto-hiding scrollbar (hold, then fade), or
    /// `None` when it's hidden. Same transient shape as `resize_overlay_until`.
    scrollbar_shown_until: Option<f64>,
    /// While `Some`, the thumb is grabbed; the value is where inside the thumb
    /// (points from its top) the pointer went down, so the thumb doesn't jump
    /// under the cursor on the first drag frame. Also gates the pane's
    /// raw-input mouse reporting, which reads `ctx.input` rather than a
    /// `Response` and so isn't covered by egui's widget arbitration.
    scrollbar_grab: Option<f32>,
    /// Last frame's `scroll_px`, so `animate_scroll` can raise the scrollbar on
    /// *any* movement — wheel, keys, thumb drag, search jump, prompt jump —
    /// from one place instead of every caller remembering to.
    scrollbar_last_px: f32,
    /// Scrollback-search overlay state (query + matches + current) while open.
    /// This pane's inspector capture (Ghostty `inspector:`), `None` while no
    /// inspector is open on it — which is what keeps the record calls on the
    /// input and PTY paths free.
    inspect: Option<crate::inspector::Log>,
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
            // Wake the **root** viewport, explicitly.
            //
            // A PTY reader thread has no egui pass in flight, so a bare
            // `request_repaint()` already resolves to the root — naming it is
            // documentation, not a change. It matters because secondary windows
            // are *immediate* viewports: only a root pass re-runs each window's
            // `show_viewport_immediate`, so this is what wakes a background
            // window too. Retargeting it at the owning child would look more
            // correct and would silently stop that window updating.
            move || wake_ctx.request_repaint_of(egui::ViewportId::ROOT),
        )?;
        let mut engine = GhosttyVtEngine::new(DEFAULT_COLS, DEFAULT_ROWS, config.scrollback_limit)?;
        engine.apply_theme(config.fg, config.bg, &config.effective_palette())?;
        engine.set_cursor_color(config.cursor)?;
        engine.set_bold_color(config.bold_color)?;
        engine.set_min_contrast(config.min_contrast)?;
        engine.set_scrollback_lines(config.scrollback_limit_lines)?;
        engine.set_vt_policy(config.title_report, config.vt_kam_allowed, config.grapheme_unicode)?;
        // Kitty graphics start disabled in libghostty, so this is what turns
        // inline images on at all.
        engine.set_image_storage_limit(config.image_storage_limit as u64)?;

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
            mouse_down: None,
            alive: true,
            osc52: Osc52Scanner::new(),
            osc7: Osc7Scanner::new(),
            decscusr: DecscusrScanner::new(),
            osc_color: OscColorScanner::new(),
            osc_notify: OscNotifyScanner::new(),
            osc133: Osc133Scanner::new(),
            command_started: None,
            pending_command_finish: Vec::new(),
            pending_notifications: Vec::new(),
            progress: None,
            progress_dirty: false,
            desktop_notifications: config.desktop_notifications,
            progress_style: config.progress_style,
            mouse_reporting: config.mouse_reporting,
            mouse_scroll_multiplier: config.mouse_scroll_multiplier,
            readonly: false,
            preedit: crate::ime::Preedit::default(),
            scroll_to_bottom: config.scroll_to_bottom,
            scrollback_compression: config.scrollback_compression,
            osc_color_report_format: config.osc_color_report_format,
            cursor_style: config.cursor_style,
            cursor_style_blink: config.cursor_style_blink,
            autoscroll_accum: 0.0,
            title_override: None,
            bell_pending: false,
            bell_flash_until: None,
            bell_effect_pending: false,
            bell_effect_last: None,
            saw_prompt_mark: false,
            resize_overlay_until: None,
            sized_once: false,
            resize_overlay: config.resize_overlay,
            resize_overlay_duration_ms: config.resize_overlay_duration_ms,
            selection_word_chars: config.selection_word_chars.clone(),
            selection_clear_on_typing: config.selection_clear_on_typing,
            selection_clear_on_copy: config.selection_clear_on_copy,
            clipboard: config.clipboard,
            pending_clipboard: None,
            scrollbar_shown_until: None,
            scrollbar_grab: None,
            scrollbar_last_px: 0.0,
            search: None,
            inspect: None,
            search_text: Vec::new(),
        })
    }

    /// Drain pending PTY output into the engine and flush responses back.
    /// Marks the session dead when the shell has exited (channel disconnected).
    /// Background housekeeping, run each pump: idle scrollback compression.
    /// The budget keeps one step from stalling a frame; the app's 500 ms
    /// heartbeat keeps it going in an otherwise idle window.
    pub fn idle_work(&mut self) {
        if self.scrollback_compression {
            let budget = std::time::Duration::from_millis(2);
            let _ = self.engine.compress_tick(std::time::Instant::now(), budget);
        }
    }

    pub fn pump_pty(&mut self) {
        use std::sync::mpsc::TryRecvError;
        let mut clipboard_requests: Vec<Osc52> = Vec::new();
        let mut color_queries: Vec<(ColorQuery, Terminator)> = Vec::new();
        let mut marks: Vec<Mark> = Vec::new();
        let mut osc9: Vec<Osc9> = Vec::new();
        loop {
            match self.pty.output.try_recv() {
                Ok(chunk) => {
                    // `scroll-to-bottom = output` (off by default): new data
                    // yanks the viewport to the live edge. Off by default in
                    // Ghostty too, because it fights you while you're reading
                    // scrollback of a command that is still producing output.
                    if self.scroll_to_bottom.output {
                        self.scroll_target_px = 0.0;
                    }
                    // The inspector's IO log, in the one place every byte from
                    // the shell passes through. `None` unless an inspector is
                    // open on this pane, so this is a null check otherwise.
                    //
                    // Recorded *before* the engine sees it, deliberately: the
                    // question this log answers is "did the sequence arrive?",
                    // and ConPTY re-renders a child's output rather than piping
                    // it (see CLAUDE.md), so what lands here is the last honest
                    // view of the stream.
                    if let Some(log) = self.inspect.as_mut() {
                        log.read(&chunk);
                    }
                    self.engine.write(&chunk);
                    self.osc52.feed(&chunk, &mut clipboard_requests);
                    self.osc7.feed(&chunk);
                    self.decscusr.feed(&chunk);
                    self.osc_color.feed(&chunk, &mut color_queries);
                    self.osc_notify.feed(&chunk, &mut osc9);
                    self.osc133.feed(&chunk, &mut marks);
                }
                Err(TryRecvError::Empty) => break,
                Err(TryRecvError::Disconnected) => {
                    self.alive = false;
                    break;
                }
            }
        }
        self.handle_osc52(&mut clipboard_requests);
        self.handle_command_marks(&marks);
        self.handle_osc9(osc9);
        self.refresh_search_after_prune();
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

    /// Apply `clipboard-write` / `clipboard-read` to the OSC 52 requests seen in
    /// this pump.
    ///
    /// Only the **last** set is considered (last write wins — a program that
    /// copies repeatedly shouldn't stack prompts), and a query is answered from
    /// the *current* clipboard, so a set-then-query in one batch reports the
    /// value just written.
    fn handle_osc52(&mut self, requests: &mut Vec<Osc52>) {
        if requests.is_empty() {
            return;
        }
        let (set, query) = osc52_reduce(requests);
        let last_set = set.map(str::to_string);
        let last_query = query.map(str::to_string);
        requests.clear();

        // Ghostty `clipboard-write-limit-bytes`: an oversized write is dropped.
        let last_set = last_set.filter(|t| self.clipboard.write_limit.is_none_or(|n| t.len() <= n));
        if let Some(text) = last_set {
            match self.clipboard.write {
                ClipboardAccess::Allow => write_clipboard(&text),
                ClipboardAccess::Deny => {}
                ClipboardAccess::Ask => self.queue_clipboard(ClipboardRequest::Write(text)),
            }
        }
        if let Some(targets) = last_query {
            match self.clipboard.read {
                ClipboardAccess::Allow => self.reply_to_clipboard_query(&targets),
                ClipboardAccess::Deny => {}
                ClipboardAccess::Ask => self.queue_clipboard(ClipboardRequest::Read(targets)),
            }
        }
    }

    /// Raise a request for confirmation, unless one is already waiting.
    fn queue_clipboard(&mut self, req: ClipboardRequest) {
        if self.pending_clipboard.is_none() {
            self.pending_clipboard = Some(req);
        }
    }

    /// Send the clipboard back to the program, echoing `targets`. An empty or
    /// unreadable clipboard is silently not answered, which is what xterm does
    /// and avoids telling the program anything it didn't already know.
    fn reply_to_clipboard_query(&mut self, targets: &str) {
        if let Some(text) = read_clipboard() {
            let _ = self.pty.write(&crate::osc52::query_reply(targets, &text));
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
        transient_alpha(&mut self.resize_overlay_until, now, 0.15)
    }

    // --- Scrollbar ----------------------------------------------------------

    /// Raise the scrollbar (or hold it up) as of egui time `now`.
    ///
    /// Called every frame while the pointer is over the bar or the thumb is
    /// grabbed, which is what makes the whole auto-hide behaviour fall out with
    /// no extra state: a repeated call keeps pushing the deadline forward so
    /// alpha pins at 1.0, and it fades on its own the moment the calls stop.
    pub fn mark_scrollbar_active(&mut self, now: f64) {
        self.scrollbar_shown_until = Some(now + SCROLLBAR_SHOW_SECS + SCROLLBAR_FADE_SECS);
    }

    /// Opacity of the auto-hiding scrollbar at egui time `now`, or `None` when
    /// it's hidden (in which case the caller draws nothing and interacts only
    /// with the narrow wake-up band).
    pub fn scrollbar_alpha(&mut self, now: f64) -> Option<f32> {
        transient_alpha(
            &mut self.scrollbar_shown_until,
            now,
            SCROLLBAR_FADE_SECS,
        )
    }

    /// Whether the scrollbar thumb is currently being dragged.
    pub fn scrollbar_grabbed(&self) -> bool {
        self.scrollbar_grab.is_some()
    }

    /// The pointer's offset inside the thumb at grab time, or `None` when the
    /// thumb isn't held.
    pub fn scrollbar_grab(&self) -> Option<f32> {
        self.scrollbar_grab
    }

    pub fn set_scrollbar_grab(&mut self, grab: Option<f32>) {
        self.scrollbar_grab = grab;
    }

    /// How many rows have scrolled off the top into scrollback.
    pub fn scrollback_rows(&self) -> usize {
        self.engine.scrollback_rows()
    }

    /// The scrollbar's `{ total, offset, len }` state in rows, or `None` when
    /// nothing has scrolled off yet.
    ///
    /// Deliberately reconstructed from giest's own scroll state rather than the
    /// binding's `Terminal::scrollbar()`. That call is documented as expensive
    /// when the viewport sits at an arbitrary pin — which is exactly whenever a
    /// scrollbar is on screen, and in a split you'd pay it per pane per frame.
    /// And its `offset` is the whole-line engine pin, so a thumb driven from it
    /// would step a full cell at a time during a smooth scroll; `scroll_px` is
    /// the only continuous position giest has. The three numbers cost nothing
    /// here: `scrollback_rows()` is already read every frame by `animate_scroll`.
    pub fn scrollbar_state(&self, cell_h: f32) -> Option<ScrollbarState> {
        let scrollback = self.engine.scrollback_rows();
        if scrollback == 0 {
            return None;
        }
        Some(scrollbar_rows(scrollback, self.rows, self.scroll_px, cell_h))
    }

    /// Scroll so screen row `offset_rows` (rows from the top of scrollback)
    /// sits at the viewport's top — Ghostty's `scroll_to_row`, and what a
    /// dragged thumb drives.
    ///
    /// `immediate` writes `scroll_px` as well as the target, bypassing the ease.
    /// A drag needs that: the thumb is painted *from* `scroll_px`, so easing
    /// would leave it trailing the cursor and the user would over-correct
    /// chasing it. Writing both makes "thumb position for the pointer I'm at"
    /// an identity — giest's stand-in for the live-scroll suppression both of
    /// Ghostty's apprts need. Keyboard and click-to-page seeks pass `false` and
    /// animate.
    ///
    /// Safe for `engine_pin_lines` either way: `animate_scroll` always issues a
    /// *relative* `base - engine_pin_lines`, so the pin reconciles next tick
    /// whatever `scroll_px` becomes.
    pub fn scroll_to_row(&mut self, offset_rows: f32, cell_h: f32, immediate: bool) {
        let ch = cell_h.max(1.0);
        let scrollback = self.engine.scrollback_rows() as f32;
        // Never `f32::INFINITY` here (unlike `scroll_to_top`): `immediate` puts
        // this straight into `scroll_px`, which `animate_scroll` does not clamp.
        let px = ((scrollback - offset_rows) * ch).max(0.0);
        self.scroll_target_px = px;
        if immediate {
            self.scroll_px = px;
        }
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

    /// Whether the program is receiving mouse events right now.
    ///
    /// Gated by `mouse-reporting`: with it off the program may still *ask* for
    /// tracking, but nothing is sent and the mouse keeps selecting. That's the
    /// point of the key — and of `toggle_mouse_reporting`, which is how you
    /// escape a full-screen app that has captured the pointer.
    pub fn is_mouse_tracking(&self) -> bool {
        self.mouse_reporting && self.engine.is_mouse_tracking()
    }

    /// Toggle `mouse-reporting` for this pane (Ghostty's
    /// `toggle_mouse_reporting`). Returns the new state.
    pub fn toggle_mouse_reporting(&mut self) -> bool {
        self.mouse_reporting = !self.mouse_reporting;
        self.mouse_reporting
    }

    /// The shell-set window/tab title, if any.
    /// The pane's title: an explicit override (Ghostty `set_surface_title`) if
    /// one was set, otherwise whatever the program reported via OSC 0/2.
    pub fn title(&self) -> Option<String> {
        self.title_override
            .clone()
            .or_else(|| self.engine.title())
    }

    /// Override the pane's title (Ghostty `set_surface_title`). An empty value
    /// clears the override and hands the title back to the program, which is
    /// the only way to undo one.
    pub fn set_title_override(&mut self, title: &str) {
        self.title_override = (!title.is_empty()).then(|| title.to_string());
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
                let anchor = self.capture_search_text();
                // Raw, not `capture_space_row`: the recapture above just reset
                // the anchor, so capture space *is* live space here.
                let target = self.viewport_bottom_row();
                if let Some(s) = self.search.as_mut() {
                    s.anchor_at_capture = anchor;
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

    /// Begin a drag selection. `rectangle` selects a block rather than a run of
    /// text — Ghostty's ctrl+alt drag on this platform (see
    /// [`is_rectangle_select`]).
    pub fn begin_selection(&mut self, cell: (u16, u16), rectangle: bool) {
        self.engine.selection_begin(cell.0, cell.1, rectangle);
    }
    pub fn update_selection(&mut self, cell: (u16, u16), rectangle: bool) {
        self.engine.selection_update(cell.0, cell.1, rectangle);
    }

    /// Extend an existing selection to `cell` (Shift+click); starts a new one
    /// if nothing is selected yet.
    pub fn extend_selection(&mut self, cell: (u16, u16), rectangle: bool) {
        if self.engine.selection_active() {
            self.engine.selection_update(cell.0, cell.1, rectangle);
        } else {
            self.begin_selection(cell, rectangle);
        }
    }
    pub fn clear_selection(&mut self) {
        self.engine.selection_clear();
    }

    /// Advance the drag-autoscroll clock by `dt` seconds and return how many
    /// rows to scroll in `dir` (`-1` up, `+1` down).
    ///
    /// Ghostty's timer is 15 ms per row, so this is rate rather than refresh
    /// rate: one row per frame would match upstream at 60 Hz and run at more
    /// than twice its speed on a 144 Hz display. The accumulator is reset when
    /// the drag leaves the edge, so re-entering starts a fresh tick instead of
    /// firing a burst of banked rows.
    pub fn autoscroll_step(&mut self, dir: isize, dt: f32) -> isize {
        autoscroll_rows(&mut self.autoscroll_accum, dir, dt)
    }

    /// Whether this pane currently has a selection.
    pub fn has_selection(&self) -> bool {
        self.engine.selection_active()
    }

    /// Move the selection's free end (Ghostty `adjust_selection`), scrolling the
    /// new end into view — upstream does the same, and without it a selection
    /// extended past the viewport grows invisibly.
    pub fn adjust_selection(&mut self, how: crate::engine::SelectionAdjust, cell_h: f32) {
        let Some(end_row) = self.engine.selection_adjust(how) else {
            return;
        };
        let (top, bottom) = (self.viewport_top_row(), self.viewport_bottom_row());
        if end_row >= top && end_row <= bottom {
            return;
        }
        // Put the end on the nearest edge rather than centring it: this fires on
        // every keypress of a held shift+arrow, and re-centring each time would
        // make the view lurch.
        let scrollback = self.engine.scrollback_rows() as u32;
        let target_top = if end_row < top {
            end_row
        } else {
            end_row.saturating_sub(self.rows.saturating_sub(1) as u32)
        };
        self.scroll_target_px = scrollback.saturating_sub(target_top) as f32 * cell_h;
    }

    /// Select the whole word under `cell` (double-click).
    ///
    /// Resolved by the VT engine, so word boundaries are the terminal's own
    /// (honouring `selection-word-chars`) rather than a second opinion computed
    /// from the rendered grid. The old hand-rolled scan was *replaced* rather
    /// than kept as a fallback: two selection sources would be free to disagree
    /// about what a word is, which is the failure this codebase keeps
    /// documenting elsewhere.
    pub fn select_word(&mut self, cell: (u16, u16)) {
        self.select_semantic(SelectKind::Word, cell);
    }

    /// Select the logical line under `cell` (triple-click) — which **follows
    /// soft wrapping**, so a command longer than the window selects whole rather
    /// than one screen row of itself.
    pub fn select_line(&mut self, cell: (u16, u16)) {
        self.select_semantic(SelectKind::Line, cell);
    }

    /// Select the output of the command that produced this row
    /// (Ctrl+triple-click, as upstream), delimited by its OSC 133 marks.
    /// A no-op in a shell that doesn't mark its prompts.
    pub fn select_output(&mut self, cell: (u16, u16)) {
        self.select_semantic(SelectKind::Output, cell);
    }

    fn select_semantic(&mut self, kind: SelectKind, cell: (u16, u16)) {
        // A gesture that finds nothing (an empty cell, or a shell with no prompt
        // marks) leaves any existing selection alone rather than clearing it, so
        // a stray double-click doesn't discard what the user had. The engine
        // enforces that; the return value is ignored here on purpose.
        let _ = self
            .engine
            .select_semantic(kind, cell.0, cell.1, &self.selection_word_chars);
    }

    /// Select everything the terminal holds — **including scrollback**, not just
    /// the visible viewport.
    pub fn select_all(&mut self) {
        let _ = self.engine.select_all();
    }

    /// Paste `text` into the shell — **the single gated entry point** for every
    /// paste (keyboard, menu, middle-click, `paste_from_clipboard`).
    ///
    /// Text that looks unsafe is not written; it becomes a pending
    /// [`ClipboardRequest`] for the app to confirm, exactly as Ghostty's
    /// `completeClipboardPaste` returns `error.UnsafePaste` for the apprt to
    /// turn into a dialog. Nothing reaches the PTY until the user says yes.
    pub fn paste_str(&mut self, text: &str) {
        if text.is_empty() {
            return;
        }
        let bracketed = self.engine.bracketed_paste();
        if config::paste_is_unsafe(self.clipboard, bracketed, text) {
            // Drop rather than queue if a dialog is already up (see the field).
            if self.pending_clipboard.is_none() {
                self.pending_clipboard = Some(ClipboardRequest::Paste(text.to_string()));
            }
            return;
        }
        self.write_paste(text);
    }

    /// Encode and write a paste that has passed (or been excused from) the
    /// safety check. Scrolls to the live bottom, like Ghostty and like typing.
    fn write_paste(&mut self, text: &str) {
        let encoded = self.engine.encode_paste(text);
        let _ = self.pty.write(&encoded);
        self.scroll_target_px = 0.0;
    }

    /// The clipboard operation awaiting the user's answer, if any.
    pub fn pending_clipboard(&self) -> Option<&ClipboardRequest> {
        self.pending_clipboard.as_ref()
    }

    /// Answer the pending clipboard request. `allow` performs it; anything else
    /// discards it. A no-op when nothing is pending.
    pub fn resolve_clipboard(&mut self, allow: bool) {
        let Some(req) = self.pending_clipboard.take() else {
            return;
        };
        if !allow {
            return;
        }
        match req {
            // Deliberately *not* re-checked: the user has just been shown what
            // makes it unsafe and said yes. This is Ghostty's `allow_unsafe`.
            ClipboardRequest::Paste(text) => self.write_paste(&text),
            ClipboardRequest::Write(text) => write_clipboard(&text),
            ClipboardRequest::Read(targets) => self.reply_to_clipboard_query(&targets),
        }
    }

    /// Send a full terminal reset (RIS) to the shell (menu "Reset Terminal").
    pub fn reset(&mut self) {
        let _ = self.pty.write(b"\x1bc");
    }

    /// Re-apply runtime-changeable config to the live engine (command palette's
    /// "Reload Config"): the color theme, cursor color/style, bold-color policy,
    /// and minimum-contrast. The next frame's snapshot picks up the new values.
    /// `scrollback-limit` is fixed at engine creation and is intentionally *not*
    /// changed here — but `image-storage-limit` genuinely is re-appliable, and
    /// setting it to zero wipes every stored image live.
    pub fn apply_config(&mut self, config: &Config) {
        let _ = self.engine.apply_theme(config.fg, config.bg, &config.effective_palette());
        let _ = self.engine.set_cursor_color(config.cursor);
        let _ = self.engine.set_bold_color(config.bold_color);
        let _ = self.engine.set_min_contrast(config.min_contrast);
        let _ = self
            .engine
            .set_vt_policy(config.title_report, config.vt_kam_allowed, config.grapheme_unicode);
        let _ = self
            .engine
            .set_image_storage_limit(config.image_storage_limit as u64);
        self.selection_word_chars = config.selection_word_chars.clone();
        self.selection_clear_on_typing = config.selection_clear_on_typing;
        self.selection_clear_on_copy = config.selection_clear_on_copy;
        self.cursor_style = config.cursor_style;
        self.cursor_style_blink = config.cursor_style_blink;
        // Consulted at pump/resize time rather than per frame, so these need an
        // explicit push here.
        self.osc_color_report_format = config.osc_color_report_format;
        self.resize_overlay = config.resize_overlay;
        self.resize_overlay_duration_ms = config.resize_overlay_duration_ms;
        self.clipboard = config.clipboard;
        self.desktop_notifications = config.desktop_notifications;
        self.progress_style = config.progress_style;
        // NOTE: `mouse_reporting` is deliberately re-seeded from the config on
        // reload, discarding any `toggle_mouse_reporting` — a reload is an
        // explicit "apply what I wrote", and a sticky runtime toggle surviving it
        // would be indistinguishable from the key not working.
        self.mouse_reporting = config.mouse_reporting;
        self.mouse_scroll_multiplier = config.mouse_scroll_multiplier;
        self.scroll_to_bottom = config.scroll_to_bottom;
        self.scrollback_compression = config.scrollback_compression;
    }

    /// Route the OSC 9 / OSC 777 requests seen in this pump.
    ///
    /// Notifications queue (each one is an event the user should see), but
    /// progress reports **collapse to the latest** — a build emits hundreds and
    /// only the current one means anything. The two config gates are applied
    /// here rather than at scan time so one disabled feature can't suppress the
    /// other: they share a parser precisely because they share a namespace.
    fn handle_osc9(&mut self, events: Vec<Osc9>) {
        for e in events {
            match e {
                Osc9::Notify(n) => {
                    if self.desktop_notifications {
                        self.pending_notifications.push(n);
                    }
                }
                Osc9::Progress(r) => {
                    if !self.progress_style {
                        continue;
                    }
                    // Carry the displayed percentage forward: `9;4;2` (failed)
                    // usually arrives with no value of its own, and the useful
                    // reading is "it stopped *here*", not "it stopped at 0".
                    let last = self.progress.and_then(|p| p.value()).unwrap_or(0);
                    let next = crate::taskbar::Progress::from_report(r, last);
                    if self.progress != Some(next) {
                        self.progress = Some(next);
                        self.progress_dirty = true;
                    }
                }
            }
        }
    }

    /// This pane's progress state, if it changed since the last call.
    pub fn take_progress(&mut self) -> Option<crate::taskbar::Progress> {
        if std::mem::take(&mut self.progress_dirty) {
            self.progress
        } else {
            None
        }
    }

    /// This pane's current progress state, whether or not it just changed.
    pub fn progress(&self) -> Option<crate::taskbar::Progress> {
        self.progress
    }

    /// Pair up the OSC 133 command marks seen in this pump.
    ///
    /// An **unmatched `D` is ignored**, which is load-bearing rather than
    /// defensive: neither shell hook has a post-execution hook, so both emit `D`
    /// from the *prompt* — meaning every session opens with a `D` for a command
    /// that never ran, and pressing Enter on an empty prompt produces another.
    /// Requiring a start to have been recorded is what filters those out.
    fn handle_command_marks(&mut self, marks: &[Mark]) {
        for mark in marks {
            match *mark {
                // A real `C` from a shell that emits one wins over the Enter
                // heuristic — it's the actual moment execution began.
                Mark::CommandStart => self.command_started = Some(std::time::Instant::now()),
                Mark::CommandEnd { exit_code } => {
                    if let Some(started) = self.command_started.take() {
                        self.pending_command_finish.push(CommandFinish {
                            duration: started.elapsed(),
                            exit_code,
                        });
                    }
                }
            }
        }
    }

    /// Note that the user just submitted a command, so its duration can be
    /// measured from here.
    ///
    /// This is giest's stand-in for the OSC 133 `C` mark. Emitting a real one
    /// needs a *pre-execution* hook: PowerShell has none short of overriding a
    /// PSReadLine key handler (and PSReadLine isn't always loaded), and cmd has
    /// none at all — so on Windows the shells simply can't tell us. giest can,
    /// because it is the thing that sent the Enter: `at_prompt` confirms the
    /// cursor was on a prompt row, so this fires for a submitted command and not
    /// for a newline typed into `vim` or at a continuation prompt.
    ///
    /// A `C` mark from a shell that does emit one still takes precedence — it
    /// simply overwrites this timestamp microseconds later.
    fn note_command_submitted(&mut self) {
        if self.engine.cursor_at_prompt() == Some(true) {
            self.saw_prompt_mark = true;
            self.command_started = Some(std::time::Instant::now());
        }
    }

    /// Clear the screen **and** the scrollback (Ghostty `clear_screen`).
    ///
    /// Sent as escape sequences rather than poked into the engine directly, so
    /// the terminal's own state machine does the work and stays consistent:
    /// `ESC [ 2J` erases the display, `ESC [ 3J` drops the scrollback, and
    /// `ESC [ H` puts the cursor home.
    pub fn clear_screen(&mut self) {
        self.engine.write(b"\x1b[2J\x1b[3J\x1b[H");
        self.scroll_target_px = 0.0;
        self.scroll_px = 0.0;
    }

    /// Toggle read-only for this pane (Ghostty `toggle_readonly`). Returns the
    /// new state.
    pub fn toggle_readonly(&mut self) -> bool {
        self.readonly = !self.readonly;
        self.readonly
    }

    /// Whether this pane refuses to send keyboard input to the shell.
    pub fn readonly(&self) -> bool {
        self.readonly
    }

    /// The IME composition to draw at the cursor, if one is in progress.
    pub fn preedit(&self) -> Option<&str> {
        self.preedit.active().then(|| self.preedit.text())
    }

    /// Drop a composition this pane can no longer finish — a modal took the
    /// keyboard, so `handle_input` stops seeing the IME's events.
    pub fn clear_preedit(&mut self) {
        self.preedit.clear();
    }

    /// Capture this pane's text for `write_*_file`.
    ///
    /// Returns `None` when there is nothing to write — notably for
    /// `Selection` with no selection, which Ghostty documents as a no-op rather
    /// than an empty file.
    pub fn capture_text(&mut self, scope: crate::writefile::WriteScope) -> Option<String> {
        use crate::writefile::{WriteScope, tidy};
        let text = match scope {
            WriteScope::Selection => self.selection_text()?,
            // `screen_text` walks scrollback *and* viewport, so the viewport-only
            // capture is its tail: the last `rows` rows.
            WriteScope::Scrollback | WriteScope::Screen => {
                let rows = self.engine.screen_text();
                let start = if scope == WriteScope::Screen {
                    rows.len().saturating_sub(self.rows as usize)
                } else {
                    0
                };
                let lines: Vec<String> = rows[start..]
                    .iter()
                    .map(|r| r.chars.iter().collect())
                    .collect();
                tidy(&lines)
            }
        };
        if text.trim().is_empty() {
            return None;
        }
        Some(text)
    }

    /// Send a run of key chords straight to the shell.
    ///
    /// Used to flush a key sequence that turned out not to match: the leaders
    /// were swallowed as they were typed, so if the sequence dies they have to
    /// be delivered late, in order, or `ctrl+a` followed by an unbound key would
    /// silently vanish. Ghostty flushes the same way.
    /// Write `text` to the shell verbatim (Ghostty `text:` / `csi:` / `esc:`).
    ///
    /// **Not** a paste: it does not go through `paste_str`, because it isn't
    /// clipboard content — it is a fixed string the user put in their own
    /// config, so bracketing it or raising a paste-protection prompt for it
    /// would be wrong. It does scroll to the bottom, since it is the user
    /// "typing", and it respects read-only for the same reason keys do.
    pub fn send_text(&mut self, text: &str) {
        if text.is_empty() || self.readonly {
            return;
        }
        self.scroll_target_px = 0.0;
        let _ = self.pty.write(text.as_bytes());
    }

    pub fn send_chords(&mut self, chords: &[crate::keybind::Chord]) {
        let mut bytes = Vec::new();
        for c in chords {
            bytes.extend_from_slice(&self.engine.encode_key(&KeyInput {
                code: c.code,
                mods: c.mods,
                text: None,
                press: true,
            }));
        }
        if !bytes.is_empty() {
            self.scroll_target_px = 0.0;
            let _ = self.pty.write(&bytes);
        }
    }

    /// Take the commands that finished since the last call.
    pub fn take_command_finishes(&mut self) -> Vec<CommandFinish> {
        std::mem::take(&mut self.pending_command_finish)
    }

    /// Take the desktop notifications requested since the last call.
    ///
    /// Returned rather than raised here because the pump knows none of what the
    /// decision needs: the window handle, and whether the window is focused.
    pub fn take_notifications(&mut self) -> Vec<Notification> {
        std::mem::take(&mut self.pending_notifications)
    }

    /// Scroll the viewport by `delta` lines (negative scrolls up into history).
    /// The eased `animate_scroll` chases this target on the next frame.
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

    // --- Inspector ----------------------------------------------------------

    /// Record one key press for the inspector's keyboard log.
    ///
    /// A key with no [`crate::engine::KeyCode`] (a modifier press, a media key)
    /// is skipped rather than logged under a placeholder: the log's column is a
    /// *config spelling*, and a row that can't be bound would read as one that
    /// can.
    fn log_key(
        &mut self,
        key: egui::Key,
        mods: &egui::Modifiers,
        outcome: crate::inspector::KeyOutcome,
        bytes: &[u8],
    ) {
        let Some(code) = map_egui_key(key) else {
            return;
        };
        let chord = crate::keybind::Chord {
            mods: key_mods(mods),
            code,
        };
        if let Some(log) = self.inspect.as_mut() {
            log.key(chord.name(), outcome, bytes);
        }
    }

    /// This pane's inspector capture, or `None` when no inspector is open on it.
    pub fn inspector(&self) -> Option<&crate::inspector::Log> {
        self.inspect.as_ref()
    }

    pub fn inspector_mut(&mut self) -> Option<&mut crate::inspector::Log> {
        self.inspect.as_mut()
    }

    /// Apply Ghostty's `inspector:<mode>` to this pane.
    ///
    /// The log is created on show and **dropped on hide**, which is what makes
    /// the capture free while it is closed: the record calls on the input and
    /// PTY paths become a null check. Dropping it also discards the capture,
    /// matching the rest of giest's overlays — nothing here quietly retains a
    /// buffer of the user's terminal output after they close the panel.
    pub fn set_inspector(&mut self, mode: crate::inspector::InspectorMode) {
        let want = mode.wants(self.inspect.is_some());
        // `show` on an already-open inspector keeps the capture rather than
        // restarting it — the same rule as `start_search`.
        if want == self.inspect.is_some() {
            return;
        }
        self.inspect = want.then(crate::inspector::Log::new);
    }

    // --- Scrollback search --------------------------------------------------

    /// Open the search overlay: capture the current screen (scrollback + viewport)
    /// as text so subsequent keystrokes re-search the snapshot rather than
    /// re-walking the grid. Idempotent — re-opening recaptures.
    pub fn open_search(&mut self) {
        let mut state = SearchState::new();
        state.anchor_at_capture = self.capture_search_text();
        self.search = Some(state);
    }

    /// Capture the screen text and anchor its **last** row, returning that row.
    ///
    /// The anchor is what keeps match rows correct while output streams: heavy
    /// output evicts the oldest rows and renumbers everything, and comparing the
    /// anchor's row now against this one gives the shift. Called on open and on
    /// every recapture (resize).
    fn capture_search_text(&mut self) -> u32 {
        self.search_text = self.engine.screen_text();
        let last = self.search_text.last().map(|r| r.row).unwrap_or(0);
        self.engine.set_row_anchor(Some(last));
        last
    }

    /// Close the search overlay and drop the captured text (highlights vanish).
    pub fn close_search(&mut self) {
        self.search = None;
        self.search_text = Vec::new();
        // Stop paying for the tracked reference; it costs bookkeeping on every
        // terminal mutation.
        self.engine.set_row_anchor(None);
    }

    /// Recapture an open search when its anchor row has been pruned away.
    ///
    /// Once the anchor is gone there is nothing left to measure the drift
    /// against, so every match row would silently go back to being uncorrected.
    /// Recapturing costs a screen walk, but only at the moment a prune actually
    /// destroyed the anchored row — which the page-granular pruning below makes
    /// rare — and the alternative is highlights that quietly point at the wrong
    /// lines until the user re-types the query.
    fn refresh_search_after_prune(&mut self) {
        if self.search.is_none() || self.engine.row_anchor_now().is_some() {
            return;
        }
        let anchor = self.capture_search_text();
        if let Some(s) = self.search.as_mut() {
            s.anchor_at_capture = anchor;
            s.run(&self.search_text);
        }
    }

    /// Convert a live absolute screen row into the capture's coordinate space,
    /// so it can be compared with match rows.
    fn capture_space_row(&self, live: u32) -> u32 {
        (i64::from(live) - self.search_row_shift()).max(0) as u32
    }

    /// How far the captured rows have drifted since capture (0 when nothing has
    /// been evicted, or when the anchor is gone and a recapture is due).
    fn search_row_shift(&self) -> i64 {
        let Some(s) = &self.search else { return 0 };
        match self.engine.row_anchor_now() {
            Some(now) => crate::search::row_shift(s.anchor_at_capture, now),
            None => 0,
        }
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
        // `select_nearest` compares against *capture-space* rows, so a live
        // viewport row has to have the drift taken back out of it.
        let target = self.capture_space_row(self.viewport_bottom_row());
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
        // `select_nearest` compares against *capture-space* rows, so a live
        // viewport row has to have the drift taken back out of it.
        let target = self.capture_space_row(self.viewport_bottom_row());
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
        let shift = self.search_row_shift();
        let vp_top = self.viewport_top_row();
        let rows = self.rows as u32;
        let cols = self.cols.saturating_sub(1);
        let mut out = Vec::new();
        for (i, m) in s.matches.iter().enumerate() {
            // Correct for eviction; a match whose rows are gone is dropped
            // rather than drawn over whatever now sits there.
            let Some(m) = crate::search::shifted(*m, shift) else {
                continue;
            };
            // A match that spans a soft wrap covers several display rows: the
            // first runs to the end of the line, the last starts at column 0.
            for row in m.row..=m.end_row {
                if row < vp_top || row >= vp_top + rows {
                    continue;
                }
                let col_start = if row == m.row { m.col_start } else { 0 };
                let col_end = if row == m.end_row { m.col_end } else { cols };
                out.push(SearchHighlight {
                    row: (row - vp_top) as u16,
                    col_start,
                    col_end,
                    current: i == s.current,
                });
            }
        }
        out
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
        // Same eviction correction the highlights make; without it a jump after
        // heavy output lands on the wrong line.
        let m = match crate::search::shifted(m, self.search_row_shift()) {
            Some(m) => m,
            None => return,
        };
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
    ///
    /// `osc8` / `bare` are Ghostty's `link-osc8` / `link-url` switches.
    pub fn url_at(&self, cell: (u16, u16), osc8: bool, bare: bool) -> Option<String> {
        osc8.then(|| self.engine.hyperlink_at(cell.0, cell.1))
            .flatten()
            .or_else(|| bare.then(|| find_url_at(&self.snapshot, cell.0, cell.1)).flatten())
    }

    /// The current selection's text, if any (for copy-on-select).
    ///
    /// Read from the VT engine rather than from the rendered grid, so it spans
    /// scrollback and unwraps soft wrapping — a selection is no longer limited
    /// to what happens to be on screen.
    pub fn selection_text(&self) -> Option<String> {
        self.engine
            .selected_text(self.clipboard.trim_trailing_spaces)
            .filter(|s| !s.is_empty())
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
        // Active key tables, innermost last — the same stack `App::handle_shortcuts`
        // resolves against, so both gates agree about which binding a key hits.
        tables: &[crate::keybind::TableEntry],
        // Whether `undo`/`redo` have anything to act on, so their `performable:`
        // bindings fall through to the shell when they don't.
        undo: crate::command::UndoState,
    ) {
        let (events, ppp) = ctx.input(|i| (i.events.clone(), i.pixels_per_point().max(1.0)));
        let cell_h_pts = (cell_h / ppp).max(1.0);

        let mut bytes: Vec<u8> = Vec::new();
        // Whether this frame typed anything *into the shell*, for
        // `selection-clear-on-typing`. App shortcuts and reserved combos are
        // deliberately excluded: they never reach the program, so clearing on
        // them would drop a selection the user is still working with.
        let mut typed = false;
        // Printable keys arrive as an `Event::Key` **and** an `Event::Text` for
        // the same press. A binding on a *modifierless* key — which only became
        // possible with key tables and `catch_all` — is swallowed by the Key arm
        // but would still be typed by the Text arm, so `copy/j=scroll_page_down`
        // would scroll *and* type `j`. This counts the swallows that are about
        // to produce text and skips that many Text events.
        //
        // Scoped to this frame's event list, so it cannot leak into the next
        // one: egui emits the pair back to back, and anything left over is
        // discarded when the loop ends.
        let mut suppress_text = 0usize;
        // Drops the second half of a `Commit`/`Text` pair for the same text —
        // see `ime.rs`.
        let mut dedupe = crate::ime::FrameDedupe::default();
        for event in &events {
            match event {
                // Raw wheel deltas set the scroll *target* immediately (no egui
                // input smoothing). The mouse-reporting path (`tracking`)
                // forwards the wheel to the app in `handle_mouse` instead, so
                // only drive the local viewport when not tracking. Positive
                // `delta.y` moves content down = scroll up into history.
                egui::Event::MouseWheel { unit, delta, .. } if !tracking => {
                    // `mouse-scroll-multiplier` splits by device because a
                    // notched wheel and a trackpad emit very different deltas:
                    // egui's `Line`/`Page` are the discrete kinds, `Point` the
                    // precision one.
                    let m = self.mouse_scroll_multiplier;
                    let pts = match unit {
                        egui::MouseWheelUnit::Line => delta.y * LINE_SCROLL_PTS * m.discrete,
                        egui::MouseWheelUnit::Point => delta.y * m.precision,
                        egui::MouseWheelUnit::Page => {
                            delta.y * self.rows as f32 * cell_h_pts * m.discrete
                        }
                    };
                    self.scroll_target_px += pts * ppp;
                }
                egui::Event::Text(text) => {
                    if suppress_text > 0 {
                        suppress_text -= 1;
                        continue;
                    }
                    // Mid-composition the IME owns the keyboard, and the echo
                    // of a commit already typed must not type twice.
                    if self.preedit.active() || !dedupe.text(text) {
                        continue;
                    }
                    bytes.extend_from_slice(text.as_bytes());
                    typed = true;
                }
                // Committed IME text is typed exactly like `Text`: raw bytes, no
                // keymap (a composed string is not a key), and still behind the
                // read-only gate below.
                egui::Event::Ime(ime) => {
                    if let Some(text) = self.preedit.apply(ime) {
                        if dedupe.commit(&text) {
                            bytes.extend_from_slice(text.as_bytes());
                            typed = true;
                        }
                    }
                }
                // While composing, Enter/Backspace/arrows edit the preedit —
                // none of them may reach the shell or trigger a binding.
                egui::Event::Key { .. } if self.preedit.active() => {}
                // Routed through `paste_str` like every other paste path, so
                // protection can't be bypassed by using the keyboard. It writes
                // to the PTY itself (or raises a confirmation and writes
                // nothing), so flush anything typed earlier this frame first —
                // otherwise the paste would overtake it.
                egui::Event::Paste(text) => {
                    if !bytes.is_empty() {
                        let _ = self.pty.write(&bytes);
                        bytes.clear();
                    }
                    self.paste_str(text);
                }
                // egui delivers Ctrl+C, Ctrl+Shift+C and Ctrl+Insert (and Cut)
                // as these events — `command+C` matches whether or not Shift is
                // held. Windows-Terminal semantics: with a selection, copy it
                // (and clear); with none, Ctrl+C is an interrupt.
                egui::Event::Copy | egui::Event::Cut => {
                    match copy_or_interrupt(self.selection_text()) {
                        CopyAction::Copy(text) => {
                            ctx.copy_text(text);
                            // `selection-clear-on-copy` — **false** by default
                            // upstream, where giest used to clear
                            // unconditionally. Keeping the selection lets you
                            // see what was copied and act on it again.
                            if self.selection_clear_on_copy {
                                self.clear_selection();
                            }
                        }
                        CopyAction::Interrupt => bytes.push(0x03),
                    }
                }
                egui::Event::Key {
                    key,
                    pressed: true,
                    modifiers,
                    ..
                } => {
                    let decision = decide_key(
                        *key,
                        modifiers,
                        keymap,
                        crate::command::PerformCtx {
                            has_selection: self.engine.selection_active(),
                            keymap: Some(keymap),
                            tables,
                            undo,
                            search_active: self.search.is_some(),
                        },
                        tables,
                    );
                    // Where the inspector's keyboard log is taken from: the one
                    // point that knows the chord, the decision *and* the bytes
                    // it produced. Anywhere earlier and it couldn't report the
                    // encoding; anywhere later and a swallowed key wouldn't
                    // appear at all — which is exactly the press someone opens
                    // this panel to explain.
                    // Read off *before* the match consumes the decision.
                    let outcome = key_outcome(&decision);
                    let before = bytes.len();
                    match decision {
                        KeyAction::Encode(input) => {
                            bytes.extend_from_slice(&self.engine.encode_key(&input));
                            typed = true;
                        }
                        // Reserved app combos (Ctrl+Shift/Ctrl+Tab/Ctrl-zoom, and
                        // anything bound in the keymap — including the scrollback
                        // keys, which `App::handle_shortcuts` runs as actions) and
                        // text-producing keys (handled by the `Text` event) emit no
                        // bytes here.
                        KeyAction::Swallow => {
                            // …but a swallowed key that is *about* to produce a
                            // `Text` event has to suppress that too, or the
                            // character is typed anyway. Only a press that can
                            // produce text counts: ctrl/alt/super combos emit no
                            // `Text`, so counting them would eat a later, unrelated
                            // character.
                            if produces_text(*key, modifiers) {
                                suppress_text += 1;
                            }
                        }
                        KeyAction::Suppress => {}
                    }
                    if self.inspect.is_some() {
                        self.log_key(*key, modifiers, outcome, &bytes[before..]);
                    }
                }
                _ => {}
            }
        }
        // Read-only drops everything bound for the shell, but *after* the loop
        // above so scrolling, selection and copy still work — the point is a
        // pane you can read and search without disturbing, not a frozen one.
        if self.readonly {
            bytes.clear();
        }
        if !bytes.is_empty() {
            // A carriage return submits whatever is on the line. Checked
            // *before* the write, so `cursor_at_prompt` still describes the
            // prompt the user is submitting from rather than whatever the shell
            // does next.
            if bytes.contains(&b'\r') {
                self.note_command_submitted();
            }
            // `scroll-to-bottom = keystroke` (on by default): typing returns the
            // viewport to the live edge.
            if self.scroll_to_bottom.keystroke {
                self.scroll_target_px = 0.0;
            }
            let _ = self.pty.write(&bytes);
        }
        if typed && self.selection_clear_on_typing {
            self.clear_selection();
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

        // Raise the auto-hiding scrollbar on *any* movement. Doing it here — the
        // one place every scroll path converges on — means the wheel, the keys,
        // a thumb drag, a search jump and a prompt jump all light the bar up
        // without each caller having to remember to. The early return above is
        // why this can't miss: it only fires when nothing moved.
        if self.scroll_px != self.scrollbar_last_px {
            self.scrollbar_last_px = self.scroll_px;
            let now = ctx.input(|i| i.time);
            self.mark_scrollbar_active(now);
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

/// A command that finished, for `notify-on-command-finish`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct CommandFinish {
    /// How long it ran. Compared against `notify-on-command-finish-after`.
    pub duration: std::time::Duration,
    /// Its exit code, when the shell reported one. `None` for cmd, whose
    /// `prompt` can't interpolate `%ERRORLEVEL%` at render time.
    pub exit_code: Option<i32>,
}

impl CommandFinish {
    /// Notification title: whether it worked, at a glance.
    pub fn title(&self) -> &'static str {
        match self.exit_code {
            Some(0) | None => "Command finished",
            Some(_) => "Command failed",
        }
    }

    /// Notification body: how long it took, and the exit code when there is one.
    pub fn body(&self) -> String {
        let d = format_duration(self.duration);
        match self.exit_code {
            Some(0) | None => format!("took {d}"),
            Some(c) => format!("exit {c}, took {d}"),
        }
    }
}

/// Render a command duration the way a person reads a stopwatch: seconds under
/// a minute, `m s` under an hour, `h m` beyond. Never more than two units — the
/// point is "was that quick?", not precision.
fn format_duration(d: std::time::Duration) -> String {
    let secs = d.as_secs();
    if secs < 60 {
        // Sub-minute is where the tenth actually tells you something.
        return format!("{:.1}s", d.as_secs_f64());
    }
    if secs < 3600 {
        return format!("{}m {}s", secs / 60, secs % 60);
    }
    format!("{}h {}m", secs / 3600, (secs % 3600) / 60)
}

/// A clipboard operation that needs the user's approval before it happens.
///
/// Mirrors Ghostty's `ClipboardRequestType`. Held on the [`Session`] that
/// raised it (rather than on the app) so the answer routes back without any
/// pane-index bookkeeping — indices go stale, sessions don't.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ClipboardRequest {
    /// A paste whose contents look unsafe (see [`config::paste_is_unsafe`]).
    Paste(String),
    /// The program asked to *set* the clipboard, under `clipboard-write = ask`.
    Write(String),
    /// The program asked to *read* the clipboard, under `clipboard-read = ask`.
    /// Carries the OSC 52 target selection to echo in the reply.
    Read(String),
}

impl ClipboardRequest {
    /// The dialog's title.
    pub fn title(&self) -> &'static str {
        match self {
            Self::Paste(_) => "Paste this text?",
            Self::Write(_) => "Let the program set your clipboard?",
            Self::Read(_) => "Let the program read your clipboard?",
        }
    }

    /// Why the user is being asked — the part that actually decides the answer.
    pub fn detail(&self) -> &'static str {
        match self {
            Self::Paste(_) => {
                "This text contains line breaks or a paste-end marker, so the shell may run \
                 part of it as a command as soon as it arrives."
            }
            Self::Write(_) => {
                "A program running in this pane wants to replace the contents of your \
                 system clipboard."
            }
            Self::Read(_) => {
                "A program running in this pane wants to read your system clipboard. \
                 Anything you have copied would be sent to it."
            }
        }
    }

    /// The text at stake, for the dialog's preview. `None` for a read, where
    /// there is nothing to show *yet* — that's the point of asking.
    pub fn preview(&self) -> Option<&str> {
        match self {
            Self::Paste(t) | Self::Write(t) => Some(t),
            Self::Read(_) => None,
        }
    }
}

/// What a key event resolves to once the host's reserved combos and viewport
/// shortcuts are applied. The byte encoding itself is left to libghostty's
/// encoder (the `Encode` arm); everything else is giest's own gating.
#[derive(Clone, Debug, PartialEq, Eq)]
enum KeyAction {
    /// Hand this neutral key event to the engine's encoder for the PTY.
    Encode(KeyInput),
    /// A combo reserved by the app (Ctrl+Shift namespace, Ctrl+Tab, Ctrl +/-/0
    /// font zoom, and everything in the keymap): the shell never sees it.
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
    keymap: &Keymap,
    perform: crate::command::PerformCtx<'_>,
    tables: &[crate::keybind::TableEntry],
) -> KeyAction {
    let Some(code) = map_egui_key(key) else {
        return KeyAction::Suppress;
    };
    // Anything bound to an app action is handled by `App::handle_shortcuts`, so
    // the shell must never see it — swallow it here. This covers custom keybinds
    // that fall outside the structural namespaces below (e.g. `ctrl+a`); the
    // default binds also match, redundantly with those namespaces.
    //
    // `starts_binding`, not `lookup`: the *leader* of a sequence (`ctrl+a` in
    // `ctrl+a>n`) is bound to no action of its own, so a plain lookup would let
    // it straight through to the shell and the sequence would never start.
    let chord = Chord {
        mods: key_mods(modifiers),
        code,
    };
    if keymap.starts_binding_in(tables, &chord) {
        // A `performable:` binding whose action can't act right now is *not* a
        // binding: the key belongs to the shell. This is what keeps shift+arrow
        // working in an editor when there's nothing selected — and it has to
        // agree with the gate that runs the action, so both call `can_perform`.
        let seq = std::slice::from_ref(&chord);
        // `all:` is dominant: upstream treats such a binding as always
        // performed and always consuming, since it isn't tied to one surface.
        // So it short-circuits both other flags.
        if keymap.is_all(tables, seq) {
            return KeyAction::Swallow;
        }
        let unperformable = keymap.is_performable_in(tables, seq)
            && match keymap.lookup_in(tables, &chord) {
                Some(a) => !crate::command::can_perform(&a, perform),
                None => false,
            };
        if !unperformable {
            // Ghostty's `unconsumed:` flag inverts the standing rule that a
            // bound chord never reaches the shell: the action runs *and* the
            // key is encoded. Only for a **complete** binding — a sequence
            // leader is still swallowed, or the sequence could never start.
            //
            // It composes with `performable:` exactly as upstream stacks the
            // prefixes: unperformable is handled above (plain fall-through, no
            // action), performable-and-able falls here (encode *and* run).
            //
            // The reserved namespaces below still win, deliberately: those
            // combos never reach the shell under any binding, so honouring
            // `unconsumed:` there would be the one way to inject a Ctrl+Shift
            // chord into a program.
            if keymap.is_unconsumed(tables, seq)
                && matches!(keymap.lookup_seq_in(tables, seq), Lookup::Action(_))
            {
                // Fall through to the namespace rules, then the encoder.
            } else {
                return KeyAction::Swallow;
            }
        }
        // Fall through: the reserved-namespace rules below still apply.
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
    // NOTE: Shift+PageUp/Down/Home/End used to be handled right here. They now
    // live in `Keymap::default_binds`, so the lookup above swallows them and
    // `App::handle_shortcuts` runs the matching `Action::Scroll*` — which is
    // what makes `keybind = shift+home=unbind` work, since an unbind can only
    // remove a keymap entry.
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

/// How long the scrollbar stays fully opaque after the last activity, and how
/// long it then takes to fade out.
const SCROLLBAR_SHOW_SECS: f64 = 1.0;
const SCROLLBAR_FADE_SECS: f64 = 0.35;

/// Opacity of a transient overlay whose deadline is `until`: 1.0 until the last
/// `fade` seconds, then linear to 0. Returns `None` — and clears the deadline —
/// once it has expired, so the caller stops drawing and stops repainting.
///
/// Shared by the resize overlay and the scrollbar. (`bell_flash_alpha` looks
/// similar but isn't: it fades across its *whole* duration and consumes a
/// one-shot flag, so folding it in here would change how the bell looks.)
fn transient_alpha(until: &mut Option<f64>, now: f64, fade: f64) -> Option<f32> {
    let deadline = (*until)?;
    if now >= deadline {
        *until = None;
        return None;
    }
    let left = deadline - now;
    Some(if left >= fade {
        1.0
    } else {
        (left / fade.max(1.0e-6)) as f32
    })
}

/// Ghostty's `{ total, offset, len }` scrollbar state, in rows.
///
/// `offset` is fractional on purpose — it carries the sub-line position of a
/// smooth scroll, which the engine's whole-line viewport pin cannot express.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct ScrollbarState {
    pub total: f32,
    pub offset: f32,
    pub len: f32,
}

/// Map giest's scroll state onto that contract.
///
/// `scroll_px` counts device pixels *above the live bottom*, so it runs the
/// opposite way to `offset`: at `scroll_px == 0` the viewport is at the bottom
/// and `offset` is at its maximum (`total - len`); fully scrolled up,
/// `offset` is 0.
fn scrollbar_rows(scrollback: usize, rows: u16, scroll_px: f32, cell_h: f32) -> ScrollbarState {
    let len = rows as f32;
    let scrollback = scrollback as f32;
    let up = (scroll_px / cell_h.max(1.0)).clamp(0.0, scrollback);
    ScrollbarState {
        total: scrollback + len,
        offset: scrollback - up,
        len,
    }
}

/// Reduce a pump's worth of OSC 52 requests to the one set and the one query
/// worth acting on: `(last set payload, last query targets)`.
///
/// Only the last of each survives. A program that copies in a loop should
/// leave one value on the clipboard, not raise a stack of prompts — and since
/// the set is applied before the query is answered, a set-then-query in the
/// same batch correctly reports the value just written.
fn osc52_reduce(requests: &[Osc52]) -> (Option<&str>, Option<&str>) {
    let mut set = None;
    let mut query = None;
    for r in requests {
        match r {
            Osc52::Set(text) => set = Some(text.as_str()),
            Osc52::Query(targets) => query = Some(targets.as_str()),
        }
    }
    (set, query)
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

/// Rows to autoscroll this frame, advancing `accum` by `dt` seconds.
///
/// Pure so the rate can be table-tested: a per-frame tick would be *upstream's*
/// speed at 60 Hz and over twice it at 144 Hz, and that is not something a
/// screenshot or a hand-drag would ever reveal.
fn autoscroll_rows(accum: &mut f32, dir: isize, dt: f32) -> isize {
    /// Ghostty's `selection_scroll_ms` (`termio/Thread.zig`).
    const TICK: f32 = 0.015;
    if dir == 0 {
        *accum = 0.0;
        return 0;
    }
    // Clamp: a stalled frame (or the first frame after a breakpoint) must not
    // bank a hundred rows and jump the viewport.
    *accum = (*accum + dt).min(TICK * 8.0);
    let rows = (*accum / TICK).floor();
    *accum -= rows * TICK;
    dir * rows as isize
}

/// How a [`KeyAction`] reads in the inspector's keyboard log.
///
/// A swallow is the app taking the key — a binding, or a reserved namespace —
/// and either way the shell saw nothing. A suppress is the encoder standing
/// aside for the matching `Event::Text`, which is the case that most often
/// looks like a bug from outside: the key *did* produce bytes, just not here.
fn key_outcome(action: &KeyAction) -> crate::inspector::KeyOutcome {
    use crate::inspector::KeyOutcome;
    match action {
        KeyAction::Encode(_) => KeyOutcome::Encoded,
        KeyAction::Swallow => KeyOutcome::Swallowed,
        KeyAction::Suppress => KeyOutcome::Text,
    }
}

/// Whether this key press will also arrive as an `Event::Text`.
///
/// Only presses without ctrl/alt/super can: those modifiers suppress text entry
/// on every platform egui runs on. Shift does not — `shift+a` types `A` — so it
/// is deliberately not in the list. Used to keep a *swallowed* printable key
/// from being typed anyway (see the suppression counter in `handle_input`).
pub fn produces_text(key: egui::Key, m: &egui::Modifiers) -> bool {
    if m.ctrl || m.alt || m.command || m.mac_cmd {
        return false;
    }
    // A conservative allow-list: letters, digits and punctuation produce text;
    // named keys (arrows, function keys, escape…) do not. `Key::name` is
    // stable enough for this — a single-character name is a printable key.
    key.name().chars().count() == 1
}

/// Whether these modifiers mean "select a rectangle" while dragging.
///
/// Ghostty's `surface_mouse.zig::isRectangleSelectState`: **ctrl+alt** on every
/// platform but macOS (which uses a bare alt, since alt-drag there isn't spoken
/// for). giest is Windows, so ctrl+alt.
pub fn is_rectangle_select(m: &egui::Modifiers) -> bool {
    (m.ctrl || m.command) && m.alt
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
        CommandFinish, CopyAction, KeyAction, bell_effect_due, cell_from_pos, copy_or_interrupt,
        autoscroll_rows, find_url_at, produces_text, format_duration, grid_dims, notch_split, osc7_to_path,
        px_offset, osc52_reduce, scroll_split, scrollbar_rows, transient_alpha,
    };
    use crate::osc52::Osc52;
    use crate::engine::{Cell, GridSnapshot, KeyCode, KeyInput, KeyMods};
    use crate::keybind::Keymap;
    use eframe::egui;
    use std::path::PathBuf;
    use std::time::Duration;

    fn finish(secs: f64, exit_code: Option<i32>) -> CommandFinish {
        CommandFinish {
            duration: Duration::from_secs_f64(secs),
            exit_code,
        }
    }

    #[test]
    fn command_finish_text_reflects_the_exit_code() {
        assert_eq!(finish(1.0, Some(0)).title(), "Command finished");
        assert_eq!(finish(1.0, Some(1)).title(), "Command failed");
        assert_eq!(finish(1.0, Some(-1)).title(), "Command failed");
        // cmd can't report a code; "finished" is the honest reading, since
        // "failed" would be a claim we can't support.
        assert_eq!(finish(1.0, None).title(), "Command finished");

        assert_eq!(finish(3.25, Some(0)).body(), "took 3.2s");
        assert_eq!(finish(3.25, None).body(), "took 3.2s");
        assert_eq!(finish(3.25, Some(2)).body(), "exit 2, took 3.2s");
    }

    #[test]
    fn durations_read_like_a_stopwatch() {
        let d = |s| format_duration(Duration::from_secs_f64(s));
        assert_eq!(d(0.0), "0.0s");
        assert_eq!(d(7.5), "7.5s");
        assert_eq!(d(59.9), "59.9s");
        // At a minute it switches to whole units — a tenth stops being useful.
        assert_eq!(d(60.0), "1m 0s");
        assert_eq!(d(95.0), "1m 35s");
        assert_eq!(d(3599.0), "59m 59s");
        assert_eq!(d(3600.0), "1h 0m");
        assert_eq!(d(9000.0), "2h 30m");
        // Never more than two units, at any magnitude.
        for secs in [0.0, 1.0, 61.0, 3661.0, 90_000.0] {
            assert!(d(secs).split(' ').count() <= 2, "{secs}");
        }
    }

    /// Drive the real `decide_key` with the built-in default keymap, so the
    /// existing 3-argument call sites stay unchanged. The default keymap binds
    /// exactly the host shortcuts the namespace gating already reserves, so it
    /// does not alter any of these assertions.
    fn decide_key(key: egui::Key, modifiers: &egui::Modifiers, _rows: u16) -> KeyAction {
        super::decide_key(key, modifiers, &Keymap::default(), Default::default(), &[])
    }

    #[test]
    fn drag_autoscroll_ticks_at_ghosttys_rate_not_the_refresh_rate() {
        // 15 ms per row (Ghostty's `selection_scroll_ms`), so the speed is the
        // same on any display — the point of rate-limiting instead of ticking
        // once per frame.
        let mut a = 0.0f32;
        // 60 Hz: one row per frame, which is where the two happen to agree.
        assert_eq!(autoscroll_rows(&mut a, -1, 1.0 / 60.0), -1);
        assert_eq!(autoscroll_rows(&mut a, -1, 1.0 / 60.0), -1);

        // 144 Hz: not every frame, and the same rows per second.
        a = 0.0;
        let ticks: isize = (0..144).map(|_| autoscroll_rows(&mut a, 1, 1.0 / 144.0)).sum();
        assert!(
            (65..=67).contains(&ticks),
            "≈66 rows in a second, not 144: {ticks}"
        );

        // A stalled frame cannot bank an unbounded jump.
        a = 0.0;
        assert!(autoscroll_rows(&mut a, 1, 10.0) <= 8, "a long pause is clamped");

        // Leaving the edge resets, so re-entering doesn't fire a burst.
        assert_eq!(autoscroll_rows(&mut a, 0, 1.0), 0);
        assert_eq!(
            autoscroll_rows(&mut a, 1, 0.001),
            0,
            "a fresh tick has to accumulate again"
        );
    }

    #[test]
    fn only_text_producing_presses_suppress_a_text_event() {
        // The predicate behind the double-delivery fix: a swallowed printable
        // key must also swallow its `Event::Text`, and a swallowed *modified*
        // combo must not — it produces no text, and counting it would eat the
        // next unrelated character.
        let none = egui::Modifiers::default();
        let shift = egui::Modifiers {
            shift: true,
            ..Default::default()
        };
        let ctrl = egui::Modifiers {
            ctrl: true,
            ..Default::default()
        };
        assert!(produces_text(egui::Key::J, &none));
        // Shift still types (`shift+a` is `A`), so it is not in the list.
        assert!(produces_text(egui::Key::J, &shift));
        assert!(!produces_text(egui::Key::J, &ctrl));
        // Named keys never produce text.
        assert!(!produces_text(egui::Key::ArrowLeft, &none));
        assert!(!produces_text(egui::Key::F5, &none));
        assert!(!produces_text(egui::Key::Escape, &none));
    }

    #[test]
    fn an_all_bind_consumes_the_key_even_with_unconsumed() {
        // Upstream: `global or all → consumed = true`. `all:` isn't tied to one
        // surface, so it always consumes and is always treated as performed —
        // it overrides both other flags, at both gates.
        let km = Keymap::from_config(&[("all:unconsumed:ctrl+alt+k".into(), "clear_screen".into())]);
        let mods = egui::Modifiers {
            ctrl: true,
            alt: true,
            ..Default::default()
        };
        assert_eq!(
            super::decide_key(egui::Key::K, &mods, &km, Default::default(), &[]),
            KeyAction::Swallow
        );
    }

    #[test]
    fn an_unconsumed_bind_reaches_the_shell_as_well_as_running() {
        // `unconsumed:` inverts the standing rule that a bound chord never
        // reaches the shell: the action runs *and* the key is encoded.
        let km = Keymap::from_config(&[("unconsumed:ctrl+alt+k".into(), "new_tab".into())]);
        let mods = egui::Modifiers {
            ctrl: true,
            alt: true,
            ..Default::default()
        };
        assert!(
            matches!(
                super::decide_key(egui::Key::K, &mods, &km, Default::default(), &[]),
                KeyAction::Encode(_)
            ),
            "the key still reaches the program"
        );
        // Without the flag, the same binding swallows it.
        let km = Keymap::from_config(&[("ctrl+alt+k".into(), "new_tab".into())]);
        assert_eq!(
            super::decide_key(egui::Key::K, &mods, &km, Default::default(), &[]),
            KeyAction::Swallow
        );

        // A reserved namespace still wins: those combos never reach the shell
        // under any binding, and honouring `unconsumed:` there would be the one
        // way to inject a Ctrl+Shift chord into a program.
        let km = Keymap::from_config(&[("unconsumed:ctrl+shift+k".into(), "new_tab".into())]);
        let cs = egui::Modifiers {
            ctrl: true,
            shift: true,
            ..Default::default()
        };
        assert_eq!(
            super::decide_key(egui::Key::K, &cs, &km, Default::default(), &[]),
            KeyAction::Swallow
        );
    }

    #[test]
    fn a_performable_bind_reaches_the_shell_only_when_it_cannot_act() {
        // shift+left is `adjust_selection:left`, performable. With a selection
        // it is the app's; without one it is the shell's — which is what keeps
        // shift+arrow working in an editor. Silent either way if it's wrong:
        // swallowed-and-inert, or delivered *and* acted on.
        let km = Keymap::default();
        let shift = egui::Modifiers {
            shift: true,
            ..Default::default()
        };
        let with = crate::command::PerformCtx {
            has_selection: true,
            ..Default::default()
        };
        let without = crate::command::PerformCtx {
            has_selection: false,
            ..Default::default()
        };
        assert_eq!(
            super::decide_key(egui::Key::ArrowLeft, &shift, &km, with, &[]),
            KeyAction::Swallow
        );
        assert!(matches!(
            super::decide_key(egui::Key::ArrowLeft, &shift, &km, without, &[]),
            KeyAction::Encode(_)
        ));
        // A non-performable bind on the same modifier is unaffected.
        assert_eq!(
            super::decide_key(egui::Key::PageUp, &shift, &km, without, &[]),
            KeyAction::Swallow
        );
    }

    #[test]
    fn escape_reaches_the_program_unless_a_search_is_open() {
        // `escape` is bound to `end_search` by default, and *only* the
        // `performable:` flag keeps it usable. If this ever regressed, Escape
        // would be swallowed in vim, every pager and every TUI — so it is
        // asserted on the real default keymap rather than a constructed one.
        let km = Keymap::default();
        let none = egui::Modifiers::default();
        let searching = crate::command::PerformCtx {
            search_active: true,
            ..Default::default()
        };
        assert!(matches!(
            super::decide_key(egui::Key::Escape, &none, &km, Default::default(), &[]),
            KeyAction::Encode(_)
        ));
        // …and with a search open it belongs to the overlay instead.
        assert_eq!(
            super::decide_key(egui::Key::Escape, &none, &km, searching, &[]),
            KeyAction::Swallow
        );
    }

    #[test]
    fn a_sequence_leader_is_swallowed_from_the_shell() {
        // `ctrl+a` is bound to no *action* — it only leads to one — and it sits
        // in no reserved modifier namespace, so nothing else here would stop it.
        // If it reached the shell the sequence could never start, and `ctrl+a`
        // is precisely the key a tmux user binds.
        let km = Keymap::from_config(&[("ctrl+a>n".into(), "new_tab".into())]);
        let ctrl = egui::Modifiers {
            ctrl: true,
            ..Default::default()
        };
        assert_eq!(
            super::decide_key(egui::Key::A, &ctrl, &km, Default::default(), &[]),
            KeyAction::Swallow
        );
        // An unrelated ctrl chord is still the shell's.
        assert!(matches!(
            super::decide_key(egui::Key::Q, &ctrl, &km, Default::default(), &[]),
            KeyAction::Encode(_)
        ));
        // …and with no sequence bound, ctrl+a goes to the shell as before.
        assert!(matches!(
            super::decide_key(egui::Key::A, &ctrl, &Keymap::default(), Default::default(), &[]),
            KeyAction::Encode(_)
        ));
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

    // NOTE: selection text extraction is no longer a session concern. It is read
    // from the VT engine (which spans scrollback and unwraps soft wrapping), so
    // the tests that used to live here — over a snapshot and a linear cell range
    // — are now engine tests in `engine/ghostty_vt.rs`, driving real escape
    // sequences. Keeping a grid-scanning copy here would be a second opinion
    // about what is selected, which is the failure this codebase keeps recording.

    #[test]
    fn osc52_batch_keeps_only_the_last_set_and_query() {
        let set = |s: &str| Osc52::Set(s.to_string());
        let query = |s: &str| Osc52::Query(s.to_string());

        assert_eq!(osc52_reduce(&[]), (None, None));
        // A program copying in a loop leaves one value, not a stack of prompts.
        assert_eq!(
            osc52_reduce(&[set("one"), set("two"), set("three")]),
            (Some("three"), None)
        );
        // Sets and queries are tracked independently…
        assert_eq!(
            osc52_reduce(&[set("a"), query("c"), set("b")]),
            (Some("b"), Some("c"))
        );
        // …and the caller applies the set first, so a set-then-query in one
        // batch reports the value just written.
        assert_eq!(osc52_reduce(&[query("p"), query("c")]), (None, Some("c")));
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
        // These are keymap entries now, not a hardcoded branch, so `decide_key`
        // keeps them from the shell and `App::handle_shortcuts` runs the action.
        for key in [
            egui::Key::PageUp,
            egui::Key::PageDown,
            egui::Key::Home,
            egui::Key::End,
        ] {
            assert_eq!(
                decide_key(key, &mods(false, true, false), 25),
                KeyAction::Swallow,
                "shift+{key:?} must never reach the shell"
            );
        }
        // Unshifted, they're ordinary keys the program gets to handle.
        assert!(matches!(
            decide_key(egui::Key::Home, &mods(false, false, false), 25),
            KeyAction::Encode(_)
        ));
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
            super::decide_key(egui::Key::A, &mods(true, false, false), &km, Default::default(), &[]),
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
    fn transient_alpha_holds_then_fades_then_clears() {
        // Deadline at t=10 with a 0.5s fade tail.
        let mut until = Some(10.0_f64);
        assert_eq!(transient_alpha(&mut until, 0.0, 0.5), Some(1.0));
        assert_eq!(transient_alpha(&mut until, 9.5, 0.5), Some(1.0));
        // Into the tail: linear down to zero.
        let mid = transient_alpha(&mut until, 9.75, 0.5).unwrap();
        assert!((mid - 0.5).abs() < 1.0e-6, "{mid}");
        assert!(until.is_some(), "still live inside the fade");
        // Past the deadline it reports nothing *and* clears itself, so the
        // caller stops both drawing and requesting repaints.
        assert_eq!(transient_alpha(&mut until, 10.0, 0.5), None);
        assert_eq!(until, None);
        // Idle stays idle.
        assert_eq!(transient_alpha(&mut until, 11.0, 0.5), None);
    }

    #[test]
    fn scrollbar_state_maps_scroll_px_to_rows() {
        // 100 rows of scrollback under a 24-row viewport, 14px cells.
        let st = |px| scrollbar_rows(100, 24, px, 14.0);

        // `total`/`len` don't depend on the position.
        assert_eq!(st(0.0).total, 124.0);
        assert_eq!(st(0.0).len, 24.0);

        // At the live bottom the viewport sits at the very end: offset is at
        // its maximum, `total - len`.
        assert_eq!(st(0.0).offset, 100.0);
        // Fully scrolled up: the top of scrollback.
        assert_eq!(st(100.0 * 14.0).offset, 0.0);
        // Ten lines up.
        assert_eq!(st(10.0 * 14.0).offset, 90.0);
        // Half a cell: fractional, which is the entire reason `offset` is f32
        // rather than the engine's whole-line pin.
        assert_eq!(st(7.0).offset, 99.5);

        // Over-scroll in either direction clamps rather than escaping the range.
        assert_eq!(st(-50.0).offset, 100.0);
        assert_eq!(st(1.0e9).offset, 0.0);
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
