//! The eframe application: tabs, each holding one or more split panes. The
//! active tab's panes are laid out, driven, and painted in a single GPU callback.

use std::time::Duration;

use anyhow::Result;
use eframe::egui;
use eframe::egui_wgpu;

use crate::config::Config;
use crate::profiles::{self, Profile};
use crate::render::{self, PaneFrame, TermFrame};
use crate::session::Session;

/// A tab: a binary tree of panes (`Node`) with one focused leaf (by id). Each
/// split divides only the focused pane, so splits nest (like Ghostty) instead of
/// re-flowing every pane onto a single shared axis. Generic over the leaf
/// payload `T` (the app uses `Session`; tests use a lightweight stand-in) so the
/// tree's structural logic is unit-testable without spawning a shell.
struct Tab<T> {
    root: Node<T>,
    /// Id of the focused leaf.
    focus: u64,
}

impl<T> Tab<T> {
    fn leaf(id: u64, payload: T) -> Self {
        Self {
            root: Node::Leaf { id, payload },
            focus: id,
        }
    }
    fn focused_payload(&self) -> &T {
        self.root
            .payload(self.focus)
            .unwrap_or_else(|| self.root.first_payload())
    }
    fn leaf_count(&self) -> usize {
        self.root.leaf_count()
    }
}

/// A node in a tab's split tree: a single pane (`Leaf`) or a binary split of two
/// subtrees along one axis. `Empty` is a transient placeholder used only while
/// restructuring the tree (never laid out or rendered).
enum Node<T> {
    Leaf { id: u64, payload: T },
    Split { vertical: bool, first: Box<Node<T>>, second: Box<Node<T>> },
    Empty,
}

impl<T> Node<T> {
    fn leaf_count(&self) -> usize {
        match self {
            Node::Leaf { .. } => 1,
            Node::Split { first, second, .. } => first.leaf_count() + second.leaf_count(),
            Node::Empty => 0,
        }
    }

    /// Whether the subtree contains a leaf with `target` id.
    fn contains(&self, target: u64) -> bool {
        match self {
            Node::Leaf { id, .. } => *id == target,
            Node::Split { first, second, .. } => {
                first.contains(target) || second.contains(target)
            }
            Node::Empty => false,
        }
    }

    /// The payload for leaf `target`, if present.
    fn payload(&self, target: u64) -> Option<&T> {
        match self {
            Node::Leaf { id, payload } if *id == target => Some(payload),
            Node::Leaf { .. } | Node::Empty => None,
            Node::Split { first, second, .. } => {
                first.payload(target).or_else(|| second.payload(target))
            }
        }
    }

    /// Any leaf's payload (the tree always has at least one outside restructuring).
    fn first_payload(&self) -> &T {
        match self {
            Node::Leaf { payload, .. } => payload,
            Node::Split { first, .. } => first.first_payload(),
            Node::Empty => unreachable!("empty split tree"),
        }
    }

    /// Any leaf's id (0 if the tree is empty).
    fn first_leaf_id(&self) -> u64 {
        match self {
            Node::Leaf { id, .. } => *id,
            Node::Split { first, .. } => first.first_leaf_id(),
            Node::Empty => 0,
        }
    }

    fn for_each_mut(&mut self, f: &mut impl FnMut(&mut T)) {
        match self {
            Node::Leaf { payload, .. } => f(payload),
            Node::Split { first, second, .. } => {
                first.for_each_mut(f);
                second.for_each_mut(f);
            }
            Node::Empty => {}
        }
    }

    /// Replace leaf `target` with a `Split` of the existing pane and a new leaf
    /// (`new_id`/`new_payload`) along `vertical`. Only the focused leaf changes;
    /// the rest of the tree keeps its shape. Returns false if `target` is absent.
    fn split_leaf(&mut self, target: u64, vertical: bool, new_id: u64, new_payload: T) -> bool {
        match self {
            Node::Leaf { id, .. } if *id == target => {
                let old = std::mem::replace(self, Node::Empty);
                *self = Node::Split {
                    vertical,
                    first: Box::new(old),
                    second: Box::new(Node::Leaf { id: new_id, payload: new_payload }),
                };
                true
            }
            Node::Split { first, second, .. } => {
                if first.contains(target) {
                    first.split_leaf(target, vertical, new_id, new_payload)
                } else {
                    second.split_leaf(target, vertical, new_id, new_payload)
                }
            }
            _ => false,
        }
    }

    /// Remove leaf `target`, collapsing a split that loses a child into its
    /// surviving child. Returns the new subtree (`None` if it became empty).
    fn remove_leaf(self, target: u64) -> Option<Node<T>> {
        match self {
            Node::Leaf { id, .. } if id == target => None,
            Node::Split { vertical, first, second } => {
                match (first.remove_leaf(target), second.remove_leaf(target)) {
                    (Some(a), Some(b)) => Some(Node::Split {
                        vertical,
                        first: Box::new(a),
                        second: Box::new(b),
                    }),
                    (Some(n), None) | (None, Some(n)) => Some(n),
                    (None, None) => None,
                }
            }
            other => Some(other),
        }
    }

    /// Drop leaves for which `dead` returns true, collapsing splits. `None` if
    /// the whole subtree is gone. The predicate decouples the tree from the
    /// liveness source (a live shell in the app; a flag in tests).
    fn prune(self, dead: &mut impl FnMut(&T) -> bool) -> Option<Node<T>> {
        match self {
            Node::Leaf { ref payload, .. } if dead(payload) => None,
            Node::Split { vertical, first, second } => {
                match (first.prune(&mut *dead), second.prune(&mut *dead)) {
                    (Some(a), Some(b)) => Some(Node::Split {
                        vertical,
                        first: Box::new(a),
                        second: Box::new(b),
                    }),
                    (Some(n), None) | (None, Some(n)) => Some(n),
                    (None, None) => None,
                }
            }
            other => Some(other),
        }
    }

    /// Append each leaf's (id, payload, rect) to `out`, dividing `area` by each
    /// split's axis (with a gutter between children).
    fn collect<'a>(&'a mut self, area: egui::Rect, out: &mut Vec<Leaf<'a, T>>) {
        match self {
            Node::Leaf { id, payload } => out.push(Leaf { id: *id, payload, rect: area }),
            Node::Split { vertical, first, second } => {
                let (a, b) = split_rect(area, *vertical);
                first.collect(a, out);
                second.collect(b, out);
            }
            Node::Empty => {}
        }
    }
}

/// Drop dead leaves/tabs across `tabs`, keeping the active tab selected (falling
/// back to the nearest surviving tab before it if the active one died). Returns
/// the surviving tabs and the new active index (0 when empty). Pure structural
/// core of [`App::reap_dead`], split out so the side-effect-free reselection
/// logic is testable; the caller handles the "no tabs left → close window" case.
fn reap_tabs<T>(
    tabs: Vec<Tab<T>>,
    active: usize,
    dead: &mut impl FnMut(&T) -> bool,
) -> (Vec<Tab<T>>, usize) {
    let mut survivors: Vec<Tab<T>> = Vec::with_capacity(tabs.len());
    let mut new_active = 0;
    for (i, tab) in tabs.into_iter().enumerate() {
        let Tab { root, focus } = tab;
        if let Some(root) = root.prune(&mut *dead) {
            if i <= active {
                new_active = survivors.len();
            }
            let focus = if root.contains(focus) {
                focus
            } else {
                root.first_leaf_id()
            };
            survivors.push(Tab { root, focus });
        }
    }
    let active = new_active.min(survivors.len().saturating_sub(1));
    (survivors, active)
}

/// One laid-out pane: a focusable leaf with its payload and screen rect.
struct Leaf<'a, T> {
    id: u64,
    payload: &'a mut T,
    rect: egui::Rect,
}

pub struct App {
    tabs: Vec<Tab<Session>>,
    active_tab: usize,
    /// Monotonic source of unique pane (leaf) ids, used to track focus across
    /// splits/closes that reshape the tree.
    next_id: u64,
    /// Cell size in physical pixels (from the glyph atlas), shared by all panes.
    cell_w: f32,
    cell_h: f32,
    /// Current logical font size in points (adjusted at runtime with Ctrl +/-/0).
    font_points: f32,
    config: Config,
    /// Available shell profiles and the default index, detected at startup.
    profiles: Vec<Profile>,
    default_profile: usize,
    egui_ctx: egui::Context,
    last_window_title: Option<String>,
}

/// Runtime font-size bounds in logical points.
const MIN_FONT_POINTS: f32 = 6.0;
const MAX_FONT_POINTS: f32 = 48.0;

impl App {
    pub fn new(cc: &eframe::CreationContext<'_>) -> Result<Self> {
        let render_state = cc
            .wgpu_render_state
            .as_ref()
            .expect("eframe must run with the wgpu backend");

        let config = Config::load();
        // Disable egui's built-in Ctrl +/-/0 zoom. It scales the global
        // `pixels_per_point`, which grows the *whole* UI (tab strip, padding,
        // split layout). We want those shortcuts to change only the text size,
        // which `handle_font_zoom` does by re-rasterizing the glyph atlas and
        // re-fitting the grid. Leaving both enabled makes them fight: the chrome
        // scales up while the text appears to stay the same size.
        cc.egui_ctx
            .options_mut(|o| o.zoom_with_keyboard = false);
        let ppp = cc.egui_ctx.pixels_per_point().max(1.0);
        let px = (config.font_points * ppp).round();
        let (cell_w, cell_h) = render::init(render_state, px, config.text_gamma);

        let (profiles, default_profile) = profiles::detect(config.shell.as_deref());
        let first = Session::new(&cc.egui_ctx, &config, &profiles[default_profile], None)?;

        Ok(Self {
            tabs: vec![Tab::leaf(1, first)],
            active_tab: 0,
            next_id: 2,
            cell_w,
            cell_h,
            font_points: config.font_points,
            config,
            profiles,
            default_profile,
            egui_ctx: cc.egui_ctx.clone(),
            last_window_title: None,
        })
    }

    /// Apply a new logical font size: re-rasterize the atlas and update the
    /// shared cell metrics (panes re-fit their grids on the next frame).
    fn set_font_points(&mut self, render_state: &egui_wgpu::RenderState, points: f32, ppp: f32) {
        let points = points.clamp(MIN_FONT_POINTS, MAX_FONT_POINTS);
        if (points - self.font_points).abs() < 0.01 {
            return;
        }
        self.font_points = points;
        let px = (points * ppp).round();
        let (cw, ch) = render::resize_font(render_state, px);
        self.cell_w = cw;
        self.cell_h = ch;
        self.egui_ctx.request_repaint();
    }

    /// Handle Ctrl +/-/0 font-size shortcuts (needs the render state to rebuild
    /// the atlas, so this runs from `ui` where the frame is available).
    fn handle_font_zoom(&mut self, ctx: &egui::Context, render_state: &egui_wgpu::RenderState) {
        let mut target = None;
        ctx.input(|i| {
            for event in &i.events {
                if let egui::Event::Key {
                    key,
                    pressed: true,
                    modifiers,
                    ..
                } = event
                {
                    if modifiers.ctrl && !modifiers.shift {
                        match key {
                            egui::Key::Equals | egui::Key::Plus => {
                                target = Some(self.font_points + 1.0)
                            }
                            egui::Key::Minus => target = Some(self.font_points - 1.0),
                            egui::Key::Num0 => target = Some(self.config.font_points),
                            _ => {}
                        }
                    }
                }
            }
        });
        if let Some(points) = target {
            let ppp = ctx.pixels_per_point().max(1.0);
            self.set_font_points(render_state, points, ppp);
        }
    }

    /// Allocate a fresh unique pane id.
    fn alloc_id(&mut self) -> u64 {
        let id = self.next_id;
        self.next_id += 1;
        id
    }

    /// Spawn a session for profile `idx` (clamped to the default if invalid),
    /// starting in `cwd` when given (else the process default directory).
    fn spawn_session(&self, idx: usize, cwd: Option<&std::path::Path>) -> Option<Session> {
        let profile = self.profiles.get(idx).unwrap_or(&self.profiles[self.default_profile]);
        Session::new(&self.egui_ctx, &self.config, profile, cwd)
            .map_err(|e| eprintln!("giest: failed to open session: {e}"))
            .ok()
    }

    /// Open a new tab running profile `idx`.
    fn new_tab(&mut self, idx: usize) {
        if let Some(s) = self.spawn_session(idx, None) {
            let id = self.alloc_id();
            self.tabs.push(Tab::leaf(id, s));
            self.active_tab = self.tabs.len() - 1;
        }
    }

    /// Split only the focused pane along `vertical` axis, focusing the new pane.
    /// The rest of the tab's split layout is untouched (splits nest). The new
    /// pane inherits the focused pane's working directory (via OSC 7).
    fn split(&mut self, vertical: bool) {
        let cwd = {
            let tab = &self.tabs[self.active_tab];
            tab.root.payload(tab.focus).and_then(|s| s.pwd())
        };
        if let Some(s) = self.spawn_session(self.default_profile, cwd.as_deref()) {
            let id = self.alloc_id();
            let tab = &mut self.tabs[self.active_tab];
            let focus = tab.focus;
            tab.root.split_leaf(focus, vertical, id, s);
            tab.focus = id;
        }
    }

    /// Close the focused pane; closing the last pane closes the tab, and the
    /// last tab closes the window.
    fn close_focused(&mut self, ctx: &egui::Context) {
        let tab = &mut self.tabs[self.active_tab];
        if tab.leaf_count() > 1 {
            let focus = tab.focus;
            let root = std::mem::replace(&mut tab.root, Node::Empty);
            tab.root = root.remove_leaf(focus).unwrap_or(Node::Empty);
            tab.focus = tab.root.first_leaf_id();
        } else if self.tabs.len() > 1 {
            self.tabs.remove(self.active_tab);
            self.active_tab = self.active_tab.min(self.tabs.len() - 1);
        } else {
            ctx.send_viewport_cmd(egui::ViewportCommand::Close);
        }
    }

    /// Switch to tab `idx` if it exists (Ctrl+Shift+number).
    fn goto_tab(&mut self, idx: usize) {
        if idx < self.tabs.len() {
            self.active_tab = idx;
        }
    }

    /// Close tab `idx`; closing the last tab closes the window.
    fn close_tab(&mut self, idx: usize, ctx: &egui::Context) {
        if idx >= self.tabs.len() {
            return;
        }
        self.tabs.remove(idx);
        if self.active_tab > idx {
            self.active_tab -= 1;
        }
        if self.tabs.is_empty() {
            ctx.send_viewport_cmd(egui::ViewportCommand::Close);
        } else {
            self.active_tab = self.active_tab.min(self.tabs.len() - 1);
        }
    }

    /// Remove panes whose shell has exited; drop tabs that become empty and
    /// close the window when the last tab is gone. Returns `false` if the
    /// window is closing (caller should skip rendering this frame).
    fn reap_dead(&mut self, ctx: &egui::Context) -> bool {
        let tabs = std::mem::take(&mut self.tabs);
        let (survivors, active) =
            reap_tabs(tabs, self.active_tab, &mut |s: &Session| !s.is_alive());
        self.tabs = survivors;
        if self.tabs.is_empty() {
            ctx.send_viewport_cmd(egui::ViewportCommand::Close);
            return false;
        }
        self.active_tab = active;
        true
    }

    /// App-level shortcuts (reserved by `Session::handle_input`, never sent to
    /// the shell): tabs, splits, focus switching.
    fn handle_shortcuts(&mut self, ctx: &egui::Context) {
        let events = ctx.input(|i| i.events.clone());
        for event in &events {
            let egui::Event::Key {
                key,
                pressed: true,
                modifiers,
                ..
            } = event
            else {
                continue;
            };
            if modifiers.ctrl && modifiers.shift {
                match key {
                    egui::Key::T => self.new_tab(self.default_profile),
                    egui::Key::W => self.close_focused(ctx),
                    egui::Key::D => self.split(true),  // vertical (columns)
                    egui::Key::E => self.split(false), // horizontal (rows)
                    // Ctrl+Shift+1..8 jump to that tab; Ctrl+Shift+9 → last tab.
                    egui::Key::Num1 => self.goto_tab(0),
                    egui::Key::Num2 => self.goto_tab(1),
                    egui::Key::Num3 => self.goto_tab(2),
                    egui::Key::Num4 => self.goto_tab(3),
                    egui::Key::Num5 => self.goto_tab(4),
                    egui::Key::Num6 => self.goto_tab(5),
                    egui::Key::Num7 => self.goto_tab(6),
                    egui::Key::Num8 => self.goto_tab(7),
                    egui::Key::Num9 => self.goto_tab(self.tabs.len().saturating_sub(1)),
                    _ => {}
                }
            } else if modifiers.ctrl && *key == egui::Key::Tab {
                let n = self.tabs.len();
                if n > 1 {
                    self.active_tab = if modifiers.shift {
                        (self.active_tab + n - 1) % n
                    } else {
                        (self.active_tab + 1) % n
                    };
                }
            }
        }
    }

    fn tab_bar(&mut self, ui: &mut egui::Ui) {
        let mut switch_to = None;
        let mut want_close = None;
        // Profile index to open a new tab with (default unless the menu picks one).
        let mut want_new: Option<usize> = None;
        ui.horizontal(|ui| {
            for (i, tab) in self.tabs.iter().enumerate() {
                let raw = tab
                    .focused_payload()
                    .title()
                    .unwrap_or_else(|| format!("shell {}", i + 1));
                let mut label = ellipsize(&raw, 24);
                let count = tab.leaf_count();
                if count > 1 {
                    label = format!("{label} [{count}]");
                }
                let resp = ui.selectable_label(i == self.active_tab, label);
                if resp.clicked() {
                    switch_to = Some(i);
                }
                // Middle-click closes the tab, like a browser.
                if resp.clicked_by(egui::PointerButton::Middle) {
                    want_close = Some(i);
                }
                if ui
                    .small_button("×")
                    .on_hover_text("Close tab (Ctrl+Shift+W)")
                    .clicked()
                {
                    want_close = Some(i);
                }
            }
            if ui
                .button("+")
                .on_hover_text("New tab (Ctrl+Shift+T)")
                .clicked()
            {
                want_new = Some(self.default_profile);
            }
            // Profile picker: open a tab running a chosen shell.
            ui.menu_button("⏷", |ui| {
                for (i, profile) in self.profiles.iter().enumerate() {
                    if ui.button(&profile.name).clicked() {
                        want_new = Some(i);
                        ui.close();
                    }
                }
            })
            .response
            .on_hover_text("New tab with a specific shell");
        });
        if let Some(i) = switch_to {
            self.active_tab = i;
        }
        if let Some(idx) = want_new {
            self.new_tab(idx);
        }
        if let Some(i) = want_close {
            self.close_tab(i, &ui.ctx().clone());
        }
    }

    /// Lay out, drive, and paint the active tab's panes.
    fn render_active(&mut self, ui: &mut egui::Ui, ctx: &egui::Context) {
        let (cw, ch) = (self.cell_w, self.cell_h);
        let ppp = ctx.pixels_per_point().max(1.0);
        // Inset *each* pane by the configured padding (applied per-leaf below, not
        // once on the outer area) so the padding band wraps every split, not just
        // the window edges. The band keeps the background color (filled below) so
        // text never butts against the window edge or a split divider. The window's
        // own focus state drives whether the active cursor is solid or hollow.
        let full_area = ui.max_rect();
        let pad = egui::vec2(self.config.padding_x, self.config.padding_y);
        let window_focused = ctx.input(|i| i.focused);
        let copy_on_select = self.config.copy_on_select;
        let sel_bg = self.config.selection_bg;
        let sel_fg = self.config.selection_fg;
        let active_tab = self.active_tab;

        let tab = &mut self.tabs[active_tab];
        let mut focus_id = tab.focus;

        // Lay the split tree out across the full area; each leaf gets its rect
        // (padding is applied per-leaf below).
        let mut leaves: Vec<Leaf<Session>> = Vec::new();
        tab.root.collect(full_area, &mut leaves);
        if leaves.is_empty() {
            return;
        }

        // Focus-follows-click: a press inside a pane focuses it.
        let press_pos = ctx.input(|i| {
            i.events.iter().find_map(|e| match e {
                egui::Event::PointerButton {
                    pos, pressed: true, ..
                } => Some(*pos),
                _ => None,
            })
        });
        if let Some(pos) = press_pos {
            if let Some(l) = leaves.iter().find(|l| l.rect.contains(pos)) {
                focus_id = l.id;
            }
        }
        if !leaves.iter().any(|l| l.id == focus_id) {
            focus_id = leaves[0].id;
        }
        let focus_idx = leaves.iter().position(|l| l.id == focus_id).unwrap();

        // Keyboard goes to the focused pane.
        let tracking = leaves[focus_idx].payload.is_mouse_tracking();
        leaves[focus_idx].payload.handle_input(ctx, tracking, ch);

        let mut frames: Vec<PaneFrame> = Vec::with_capacity(leaves.len());
        for leaf in leaves.iter_mut() {
            // The pane occupies `leaf.rect`; the grid is inset by the padding so
            // text clears the pane's edges (window border or split divider alike).
            let prect = leaf.rect.shrink2(pad);
            let leaf_id = leaf.id;
            let is_focus = leaf_id == focus_id;
            let session = &mut *leaf.payload;
            session.fit_grid(prect, ppp, cw, ch);
            if !session.update_snapshot() {
                continue;
            }

            // Mouse / selection interaction only for the focused pane.
            if is_focus {
                // Hold keyboard focus on the terminal and lock the navigation keys
                // to it. Otherwise egui's built-in focus traversal swallows Tab
                // (which the shell wants for completion) to cycle focus through the
                // tab-strip buttons, and a following Enter/Space fires whichever
                // button got focus — switching/closing tabs unexpectedly.
                let resp = ui.interact(
                    prect,
                    egui::Id::new(("giest-pane", active_tab, leaf_id)),
                    egui::Sense::click_and_drag(),
                );
                resp.request_focus();
                ctx.memory_mut(|m| {
                    m.set_focus_lock_filter(
                        resp.id,
                        egui::EventFilter {
                            tab: true,
                            horizontal_arrows: true,
                            vertical_arrows: true,
                            escape: false,
                        },
                    )
                });

                if tracking {
                    session.handle_mouse(ctx, prect, ppp, cw, ch);
                    session.clear_selection();
                } else {
                    let cell_at = |p: egui::Pos2, s: &Session| s.pos_to_cell(p, prect, ppp, cw, ch);
                    if resp.triple_clicked() {
                        if let Some(p) = resp.interact_pointer_pos() {
                            let c = cell_at(p, session);
                            session.select_line(c);
                        }
                    } else if resp.double_clicked() {
                        if let Some(p) = resp.interact_pointer_pos() {
                            let c = cell_at(p, session);
                            session.select_word(c);
                        }
                    } else if resp.drag_started() {
                        if let Some(p) = resp.interact_pointer_pos() {
                            let c = cell_at(p, session);
                            session.begin_selection(c);
                        }
                    } else if resp.dragged() {
                        if let Some(p) = resp.interact_pointer_pos() {
                            let c = cell_at(p, session);
                            session.update_selection(c);
                        }
                    } else if resp.clicked() {
                        let mods = ctx.input(|i| i.modifiers);
                        if mods.shift {
                            // Shift+click extends the current selection.
                            if let Some(p) = resp.interact_pointer_pos() {
                                let c = cell_at(p, session);
                                session.extend_selection(c);
                            }
                        } else {
                            // Ctrl+click opens a URL under the cursor; a plain
                            // click clears the selection.
                            let opened = (mods.ctrl || mods.command)
                                && resp
                                    .interact_pointer_pos()
                                    .map(|p| cell_at(p, session))
                                    .and_then(|c| session.url_at(c))
                                    .map(|url| open_url(&url))
                                    .is_some();
                            if !opened {
                                session.clear_selection();
                            }
                        }
                    }
                    // Copy as soon as a selection is completed, if enabled.
                    if copy_on_select
                        && (resp.double_clicked() || resp.triple_clicked() || resp.drag_stopped())
                    {
                        if let Some(text) = session.selection_text() {
                            ctx.copy_text(text);
                        }
                    }
                }
            }

            let mut snapshot = session.snapshot.clone();
            // Only the focused pane of a focused window gets a live (solid,
            // blinking) cursor; every other visible cursor is drawn hollow.
            let pane_active = is_focus && window_focused;
            if pane_active && snapshot.cursor_blinking {
                if ctx.input(|i| i.time) % 1.0 >= 0.5 {
                    snapshot.cursor_visible = false;
                }
                ctx.request_repaint_after(Duration::from_millis(100));
            }
            frames.push(PaneFrame {
                snapshot,
                // Snap the grid origin to the physical pixel grid. The cell size
                // is integer (ceil'd in the atlas), so an integer origin makes
                // every cell boundary land on a pixel — no anti-aliased seams
                // between rows/cells, and glyphs (rasterized on the integer grid)
                // stay crisp. Matches Ghostty / Windows Terminal pixel snapping.
                origin_px: [(prect.min.x * ppp).round(), (prect.min.y * ppp).round()],
                selection: session.selection_range(),
                cursor_hollow: !pane_active,
            });
        }

        // Fill the whole area (including the padding band) with the focused
        // pane's background first.
        let bg = leaves[focus_idx].payload.default_bg();
        ui.painter()
            .rect_filled(full_area, 0.0, egui::Color32::from_rgb(bg.r, bg.g, bg.b));

        // One callback paints every pane (shared instance buffer).
        ui.painter().add(egui_wgpu::Callback::new_paint_callback(
            full_area,
            TermFrame {
                panes: frames,
                selection_bg: sel_bg,
                selection_fg: sel_fg,
            },
        ));

        // Outline the focused pane when the tab is split. Frame the *full* pane
        // rect (not the padded grid) so the padding band shows as a visible gap
        // between the border and the text, rather than the text hugging the line.
        if leaves.len() > 1 {
            ui.painter().rect_stroke(
                leaves[focus_idx].rect,
                0.0,
                egui::Stroke::new(2.0, egui::Color32::from_rgb(90, 130, 200)),
                egui::StrokeKind::Inside,
            );
        }

        // Commit the (possibly click-updated) focus back to the tab. Done last,
        // after the final use of `leaves` (which borrows `tab.root`).
        self.tabs[active_tab].focus = focus_id;
    }
}

impl eframe::App for App {
    fn ui(&mut self, ui: &mut egui::Ui, frame: &mut eframe::Frame) {
        let ctx = ui.ctx().clone();

        // Poll for shell exit even when idle (a shell that exits produces no
        // output, so nothing else would wake us to reap it).
        ctx.request_repaint_after(Duration::from_millis(500));

        // Pump every pane in every tab so background sessions keep flowing.
        for tab in &mut self.tabs {
            tab.root.for_each_mut(&mut |pane| pane.pump_pty());
        }
        // Close panes/tabs whose shell exited; bail if that closed the window.
        if !self.reap_dead(&ctx) {
            return;
        }
        self.handle_shortcuts(&ctx);
        if let Some(render_state) = frame.wgpu_render_state() {
            let render_state = render_state.clone();
            self.handle_font_zoom(&ctx, &render_state);
        }

        // Window title from the active tab's focused pane.
        let title = self
            .tabs
            .get(self.active_tab)
            .and_then(|t| t.focused_payload().title());
        if title != self.last_window_title {
            let shown = title.clone().unwrap_or_else(|| "giest".to_string());
            ctx.send_viewport_cmd(egui::ViewportCommand::Title(shown));
            self.last_window_title = title;
        }

        // Always show the tab strip so the new-tab profile picker (PowerShell /
        // cmd / WSL / …) is reachable even with a single tab.
        egui::Panel::top("giest-tabs").show_inside(ui, |ui| self.tab_bar(ui));

        let bg = self.tabs[self.active_tab].focused_payload().default_bg();
        egui::CentralPanel::default()
            .frame(egui::Frame::NONE.fill(egui::Color32::from_rgb(bg.r, bg.g, bg.b)))
            .show_inside(ui, |ui| self.render_active(ui, &ctx));
    }
}

/// Open a URL in the user's default handler (Windows). `explorer` routes
/// http/https/etc. to the default browser without flashing a console window.
fn open_url(url: &str) {
    let _ = std::process::Command::new("explorer").arg(url).spawn();
}

/// Split `area` into two halves along one axis with a 1px gutter between them.
/// `vertical` = a vertical divider, i.e. side-by-side columns (Ctrl+Shift+D);
/// otherwise stacked rows (Ctrl+Shift+E).
fn split_rect(area: egui::Rect, vertical: bool) -> (egui::Rect, egui::Rect) {
    let gap = 1.0;
    if vertical {
        let w = ((area.width() - gap) / 2.0).max(1.0);
        (
            egui::Rect::from_min_size(area.min, egui::vec2(w, area.height())),
            egui::Rect::from_min_size(
                egui::pos2(area.min.x + w + gap, area.min.y),
                egui::vec2(w, area.height()),
            ),
        )
    } else {
        let h = ((area.height() - gap) / 2.0).max(1.0);
        (
            egui::Rect::from_min_size(area.min, egui::vec2(area.width(), h)),
            egui::Rect::from_min_size(
                egui::pos2(area.min.x, area.min.y + h + gap),
                egui::vec2(area.width(), h),
            ),
        )
    }
}

/// Truncate a tab label to `max` chars with an ellipsis.
fn ellipsize(s: &str, max: usize) -> String {
    let chars: Vec<char> = s.chars().collect();
    if chars.len() <= max {
        s.to_string()
    } else {
        let mut out: String = chars[..max.saturating_sub(1)].iter().collect();
        out.push('…');
        out
    }
}

#[cfg(test)]
mod tests {
    use super::{Node, Tab, ellipsize, reap_tabs, split_rect};
    use eframe::egui;

    // The split tree is generic over its leaf payload; tests use a `u32` where
    // `0` marks a "dead" leaf, so structure/liveness logic needs no real shell.
    fn leaf(id: u64, payload: u32) -> Node<u32> {
        Node::Leaf { id, payload }
    }
    fn split(vertical: bool, first: Node<u32>, second: Node<u32>) -> Node<u32> {
        Node::Split {
            vertical,
            first: Box::new(first),
            second: Box::new(second),
        }
    }

    #[test]
    fn split_leaf_nests_only_the_target() {
        let mut root = leaf(1, 1);
        assert!(root.split_leaf(1, true, 2, 1));
        assert_eq!(root.leaf_count(), 2);
        assert!(root.contains(1) && root.contains(2));
        // Splitting the newly focused leaf nests under the existing split.
        assert!(root.split_leaf(2, false, 3, 1));
        assert_eq!(root.leaf_count(), 3);
        // An absent target is a no-op.
        assert!(!root.split_leaf(99, false, 4, 1));
        assert_eq!(root.leaf_count(), 3);
    }

    #[test]
    fn remove_leaf_collapses_into_sibling() {
        let root = split(true, leaf(1, 1), leaf(2, 1));
        let after = root.remove_leaf(2).expect("one leaf survives");
        assert_eq!(after.leaf_count(), 1);
        assert!(after.contains(1) && !after.contains(2));
        // Removing the only leaf empties the tree.
        assert!(leaf(1, 1).remove_leaf(1).is_none());
    }

    #[test]
    fn prune_drops_dead_and_collapses() {
        // Tree: [1 | (2 / 3)] with leaf 2 dead.
        let root = split(true, leaf(1, 1), split(false, leaf(2, 0), leaf(3, 1)));
        let pruned = root.prune(&mut |p: &u32| *p == 0).expect("survivors remain");
        assert_eq!(pruned.leaf_count(), 2);
        assert!(pruned.contains(1) && pruned.contains(3) && !pruned.contains(2));
        // Every leaf dead → whole subtree gone.
        let all_dead = split(true, leaf(1, 0), leaf(2, 0));
        assert!(all_dead.prune(&mut |p: &u32| *p == 0).is_none());
    }

    #[test]
    fn first_leaf_id_walks_to_leftmost() {
        let root = split(false, split(true, leaf(7, 1), leaf(8, 1)), leaf(9, 1));
        assert_eq!(root.first_leaf_id(), 7);
        assert_eq!(Node::<u32>::Empty.first_leaf_id(), 0);
    }

    #[test]
    fn collect_divides_area_with_gutter() {
        // Vertical split → side-by-side columns with a 1px gutter.
        let mut root = split(true, leaf(1, 1), leaf(2, 1));
        let area = egui::Rect::from_min_size(egui::pos2(0.0, 0.0), egui::vec2(101.0, 50.0));
        let mut leaves = Vec::new();
        root.collect(area, &mut leaves);
        assert_eq!(leaves.len(), 2);
        assert_eq!(leaves[0].id, 1);
        assert_eq!(leaves[1].id, 2);
        // (101 - 1 gutter) / 2 = 50 per column.
        assert!((leaves[0].rect.width() - 50.0).abs() < 0.01);
        assert!(leaves[1].rect.min.x >= leaves[0].rect.max.x);
    }

    #[test]
    fn split_rect_halves_each_axis() {
        let area = egui::Rect::from_min_size(egui::pos2(0.0, 0.0), egui::vec2(101.0, 51.0));
        let (a, b) = split_rect(area, true); // columns
        assert!((a.width() - 50.0).abs() < 0.01);
        assert!((b.width() - 50.0).abs() < 0.01);
        assert_eq!(a.height(), 51.0);
        let (c, _d) = split_rect(area, false); // rows
        assert!((c.height() - 25.0).abs() < 0.01);
        assert_eq!(c.width(), 101.0);
    }

    #[test]
    fn reap_tabs_drops_dead_tab_and_reselects_before_it() {
        // Active tab (index 1) dies entirely → fall back to the nearest tab before it.
        let tabs = vec![Tab::leaf(1, 1u32), Tab::leaf(2, 0u32), Tab::leaf(3, 1u32)];
        let (survivors, active) = reap_tabs(tabs, 1, &mut |p: &u32| *p == 0);
        assert_eq!(survivors.len(), 2);
        assert_eq!(active, 0);
        assert_eq!(survivors[0].focus, 1);
    }

    #[test]
    fn reap_tabs_keeps_surviving_active_after_earlier_drop() {
        // Tab 0 dies; the active tab (index 1) survives and shifts to index 0.
        let tabs = vec![Tab::leaf(1, 0u32), Tab::leaf(2, 1u32)];
        let (survivors, active) = reap_tabs(tabs, 1, &mut |p: &u32| *p == 0);
        assert_eq!(survivors.len(), 1);
        assert_eq!(active, 0);
        assert_eq!(survivors[0].focus, 2);
    }

    #[test]
    fn reap_tabs_falls_back_focus_when_focused_pane_dies() {
        // A split tab whose focused leaf (2) dies keeps the tab, refocusing the survivor.
        let root = split(true, leaf(1, 1), leaf(2, 0));
        let tabs = vec![Tab { root, focus: 2 }];
        let (survivors, _active) = reap_tabs(tabs, 0, &mut |p: &u32| *p == 0);
        assert_eq!(survivors.len(), 1);
        assert_eq!(survivors[0].leaf_count(), 1);
        assert_eq!(survivors[0].focus, 1);
    }

    #[test]
    fn ellipsize_truncates_with_ellipsis() {
        assert_eq!(ellipsize("short", 10), "short");
        assert_eq!(ellipsize("a very long tab title", 6), "a ver…");
    }
}
