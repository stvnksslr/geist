//! The eframe application: tabs, each holding one or more split panes. The
//! active tab's panes are laid out, driven, and painted in a single GPU callback.

use std::time::Duration;

use anyhow::Result;
use eframe::egui;
use eframe::egui_wgpu;

use crate::command::{self, Action, PaletteState};
use crate::config::{Config, MiddleClickAction, ResizeOverlayPosition, RightClickAction};
use crate::profiles::{self, Profile};
use crate::keybind::{Chord, Keymap};
use crate::render::{self, PaneFrame, TermFrame};
use crate::session::{self, Session};

/// A tab: a binary tree of panes (`Node`) with one focused leaf (by id). Each
/// split divides only the focused pane, so splits nest (like Ghostty) instead of
/// re-flowing every pane onto a single shared axis. Generic over the leaf
/// payload `T` (the app uses `Session`; tests use a lightweight stand-in) so the
/// tree's structural logic is unit-testable without spawning a shell.
struct Tab<T> {
    root: Node<T>,
    /// Id of the focused leaf.
    focus: u64,
    /// User-set title override (via "Rename Tab…"); `None` uses the focused
    /// pane's terminal title.
    name: Option<String>,
    /// User-set tab tint (via "Tab Color"); `None` uses the default chrome color.
    color: Option<egui::Color32>,
    /// While `Some(id)`, that one leaf is "zoomed": it fills the whole tab area
    /// and the other splits are hidden (Ghostty's `toggle_split_zoom`). Cleared
    /// when the tree structure changes or the zoomed leaf goes away.
    zoomed: Option<u64>,
}

impl<T> Tab<T> {
    fn leaf(id: u64, payload: T) -> Self {
        Self {
            root: Node::Leaf { id, payload },
            focus: id,
            name: None,
            color: None,
            zoomed: None,
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
    Leaf {
        id: u64,
        payload: T,
    },
    Split {
        vertical: bool,
        first: Box<Node<T>>,
        second: Box<Node<T>>,
    },
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
            Node::Split { first, second, .. } => first.contains(target) || second.contains(target),
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

    /// Mutable counterpart of [`payload`](Self::payload) (the palette's focused-
    /// pane actions need `&mut`). Written with an early return rather than
    /// `or_else` so the borrow of `first` ends before `second` is tried.
    fn payload_mut(&mut self, target: u64) -> Option<&mut T> {
        match self {
            Node::Leaf { id, payload } if *id == target => Some(payload),
            Node::Leaf { .. } | Node::Empty => None,
            Node::Split { first, second, .. } => {
                if let Some(p) = first.payload_mut(target) {
                    return Some(p);
                }
                second.payload_mut(target)
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
                    second: Box::new(Node::Leaf {
                        id: new_id,
                        payload: new_payload,
                    }),
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
            Node::Split {
                vertical,
                first,
                second,
            } => match (first.remove_leaf(target), second.remove_leaf(target)) {
                (Some(a), Some(b)) => Some(Node::Split {
                    vertical,
                    first: Box::new(a),
                    second: Box::new(b),
                }),
                (Some(n), None) | (None, Some(n)) => Some(n),
                (None, None) => None,
            },
            other => Some(other),
        }
    }

    /// Drop leaves for which `dead` returns true, collapsing splits. `None` if
    /// the whole subtree is gone. The predicate decouples the tree from the
    /// liveness source (a live shell in the app; a flag in tests).
    fn prune(self, dead: &mut impl FnMut(&T) -> bool) -> Option<Node<T>> {
        match self {
            Node::Leaf { ref payload, .. } if dead(payload) => None,
            Node::Split {
                vertical,
                first,
                second,
            } => match (first.prune(&mut *dead), second.prune(&mut *dead)) {
                (Some(a), Some(b)) => Some(Node::Split {
                    vertical,
                    first: Box::new(a),
                    second: Box::new(b),
                }),
                (Some(n), None) | (None, Some(n)) => Some(n),
                (None, None) => None,
            },
            other => Some(other),
        }
    }

    /// Append every leaf id to `out` in tree (left-to-right) order. Used by
    /// split-focus cycling, which then sorts to recover creation order.
    fn leaf_ids(&self, out: &mut Vec<u64>) {
        match self {
            Node::Leaf { id, .. } => out.push(*id),
            Node::Split { first, second, .. } => {
                first.leaf_ids(out);
                second.leaf_ids(out);
            }
            Node::Empty => {}
        }
    }

    /// Append each leaf's (id, payload, rect) to `out`, dividing `area` by each
    /// split's axis (with a gutter between children).
    fn collect<'a>(&'a mut self, area: egui::Rect, out: &mut Vec<Leaf<'a, T>>) {
        match self {
            Node::Leaf { id, payload } => out.push(Leaf {
                id: *id,
                payload,
                rect: area,
            }),
            Node::Split {
                vertical,
                first,
                second,
            } => {
                let (a, b) = split_rect(area, *vertical);
                first.collect(a, out);
                second.collect(b, out);
            }
            Node::Empty => {}
        }
    }

    /// Push only the leaf with `target` id, occupying the whole `area` (split
    /// zoom: the focused pane fills the tab, the other splits are hidden).
    /// Mutable like [`collect`](Self::collect) so the zoomed pane can be driven.
    fn collect_leaf<'a>(&'a mut self, target: u64, area: egui::Rect, out: &mut Vec<Leaf<'a, T>>) {
        match self {
            Node::Leaf { id, payload } if *id == target => out.push(Leaf {
                id: *id,
                payload,
                rect: area,
            }),
            Node::Leaf { .. } | Node::Empty => {}
            Node::Split { first, second, .. } => {
                if first.contains(target) {
                    first.collect_leaf(target, area, out);
                } else {
                    second.collect_leaf(target, area, out);
                }
            }
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
        let Tab {
            root,
            focus,
            name,
            color,
            zoomed,
        } = tab;
        if let Some(root) = root.prune(&mut *dead) {
            if i <= active {
                new_active = survivors.len();
            }
            let focus = if root.contains(focus) {
                focus
            } else {
                root.first_leaf_id()
            };
            // A zoom on a pruned-away pane is stale; only keep it if it survived.
            let zoomed = zoomed.filter(|id| root.contains(*id));
            survivors.push(Tab {
                root,
                focus,
                name,
                color,
                zoomed,
            });
        }
    }
    let active = new_active.min(survivors.len().saturating_sub(1));
    (survivors, active)
}

/// Keep only `tabs[keep]` (Ghostty's "Close Other Tabs"), returning it at index 0
/// with the active selection on it. A no-op when `keep` is out of range. Pure core
/// of [`App::close_other_tabs`], split out so the reselection is testable.
fn keep_only_tab<T>(mut tabs: Vec<Tab<T>>, keep: usize, active: usize) -> (Vec<Tab<T>>, usize) {
    if keep >= tabs.len() {
        return (tabs, active);
    }
    tabs.swap(0, keep);
    tabs.truncate(1);
    (tabs, 0)
}

/// Drop every tab after `idx` (Ghostty's "Close Tabs to the Right"), clamping the
/// active selection into the survivors. Pure core of [`App::close_tabs_to_right`].
fn truncate_tabs_to_right<T>(
    mut tabs: Vec<Tab<T>>,
    idx: usize,
    active: usize,
) -> (Vec<Tab<T>>, usize) {
    if idx + 1 < tabs.len() {
        tabs.truncate(idx + 1);
        let active = active.min(tabs.len() - 1);
        (tabs, active)
    } else {
        (tabs, active)
    }
}

/// One laid-out pane: a focusable leaf with its payload and screen rect.
struct Leaf<'a, T> {
    id: u64,
    payload: &'a mut T,
    rect: egui::Rect,
}

/// One OS window: its tab list plus every piece of UI state that is per-window.
///
/// `Config`, `Keymap` and the profile list are held **per window rather than in
/// a shared cell**. They're cheap to clone, it keeps every method body free of
/// borrow plumbing, and it mirrors Ghostty, which clones the config per surface
/// (`shallowClone` in `newConfig`). The price is that anything which must stay
/// uniform across windows has to be fanned out explicitly by [`App`] — namely a
/// config reload and the font metrics, which are pinned together by the single
/// shared glyph atlas (see [`App::sync_font`]).
pub struct Window {
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
    /// The active tab's `(leaf id, rect)` layout from the previous frame, cached
    /// by `render_active` so `handle_shortcuts` (which runs before layout) can
    /// resolve directional split-focus navigation geometrically.
    last_layout: Vec<(u64, egui::Rect)>,
    /// When a tab is being renamed inline ("Rename Tab…"), its index and the
    /// in-progress edit text; `None` when no rename is active.
    renaming: Option<(usize, String)>,
    /// The open command palette (Ctrl+Shift+P), or `None` when closed. While
    /// `Some`, app shortcuts and terminal input are suppressed so the palette is
    /// modal (see `ui`/`render_active`).
    palette: Option<PaletteState>,
    /// Chord → action bindings (built-in defaults plus the config's `keybind`
    /// overrides). `handle_shortcuts` resolves each key event through this.
    keymap: Keymap,
    /// Whether the window is currently fullscreen (`toggle_fullscreen`). Tracked
    /// here because the viewport's fullscreen flag isn't readable back, so we flip
    /// our own copy and command winit to match.
    fullscreen: bool,
    /// The window's `HWND`, captured at startup for the DWM backdrop
    /// ([`crate::blur`]). `None` off Windows or if the handle wasn't available.
    hwnd: Option<isize>,
    /// Whether the surface was created transparent. Decided in `main.rs` from the
    /// config *before* the window exists, so it can't change without a restart —
    /// `reload_config` uses this to tell the user when a new opacity needs one.
    transparent_surface: bool,
    /// The tab index a drag-reorder started on, or `None`. All the persistent
    /// state a reorder needs — the rects and pointer-x are per-frame locals.
    tab_drag: Option<usize>,
    /// A close waiting on the confirmation modal. While `Some`, the modal is
    /// drawn and owns the keyboard — like `palette` and the search overlay.
    confirm: Option<PendingClose>,
    /// True once a confirmed *window* close has been issued. Without it the
    /// `ViewportCommand::Close` we send would come straight back as another
    /// `close_requested()` with nothing pending, re-opening the dialog forever.
    closing: bool,
    /// Whether a bell has marked the title with 🔔, pending a refocus.
    bell_title: bool,
    /// Previous frame's window focus, so a bell marker can be cleared on the
    /// unfocused→focused *edge* rather than every focused frame.
    was_focused: bool,
    /// Stable, slot-independent identity. Drives this window's child
    /// `ViewportId` and its egui-`Id` namespace, so a window keeps both across
    /// any reshuffle of the window list.
    window_id: u64,
    /// Whether this window occupies the root viewport. Owned by [`App`].
    is_root: bool,
    /// A BEL rang in one of this window's panes during the app-level pump;
    /// consumed by this window's own pass, which is where its focus state (and
    /// so the `attention` feature) is meaningful.
    pending_bell: bool,
    /// Last observed outer position and inner size, recorded each pass. Used
    /// when the root-slot window is retired: the survivor that inherits the root
    /// viewport is moved onto the geometry it already occupied, so the window
    /// that visually disappears is the one the user closed.
    geom: Option<(egui::Pos2, egui::Vec2)>,
    /// App-scoped intents raised anywhere in this window's pass, drained by
    /// [`Window::run_pass`] and applied by [`App`] once nothing is borrowed.
    /// The same deferred-intent idiom as `want_split` / `want_close`.
    requests: Vec<AppRequest>,
}

/// Something only [`App`] can do, raised from inside a window's pass.
#[derive(Clone, Debug, PartialEq, Eq)]
enum AppRequest {
    /// Open a window, with the working directory already resolved from the
    /// raising window's focused pane (Ghostty resolves it from the previously
    /// focused surface, not from the new one's parent).
    NewWindow(Option<std::path::PathBuf>),
    /// Retire the raising window. Quits giest when it's the last one.
    CloseWindow,
}

/// The whole application: every open window, plus the little state that has to
/// be coordinated between them.
pub struct App {
    /// `windows[0]` is always the one drawn into [`egui::ViewportId::ROOT`].
    windows: Vec<Window>,
    /// Index of the window that most recently reported focus. Recomputed every
    /// pass — never held across frames, since a retire can invalidate it.
    focused: usize,
    next_window_id: u64,
}

/// The kind of surface being created, for the working-directory inheritance
/// decision. Mirrors Ghostty's `apprt.surface.NewSurfaceContext`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum NewSurface {
    Window,
    Tab,
    Split,
}

/// Whether a new surface of this kind inherits the previously focused pane's
/// working directory (reported via OSC 7).
///
/// One table for all three kinds, exactly as Ghostty gates them in
/// `shouldInheritWorkingDirectory` — so the three call sites can't drift from
/// each other or from upstream. All three default to `true`.
pub fn should_inherit_cwd(what: NewSurface, cfg: &Config) -> bool {
    match what {
        NewSurface::Window => cfg.window_inherit_working_directory,
        NewSurface::Tab => cfg.tab_inherit_working_directory,
        NewSurface::Split => cfg.split_inherit_working_directory,
    }
}

/// What a confirmed close should do. One enum so the confirmation modal is a
/// single widget rather than one per close path.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum PendingClose {
    /// The focused pane (falling through to the tab, then the window).
    Pane,
    Tab(usize),
    /// Every tab except this one.
    OtherTabs(usize),
    /// Every tab after this one.
    TabsToRight(usize),
    /// The whole window — the titlebar `×` / Alt+F4.
    Window,
}

impl PendingClose {
    /// Wording for the confirmation dialog.
    fn description(self) -> &'static str {
        match self {
            Self::Pane => "This terminal will be closed.",
            Self::Tab(_) => "This tab and its terminals will be closed.",
            Self::OtherTabs(_) => "All other tabs and their terminals will be closed.",
            Self::TabsToRight(_) => "All tabs to the right will be closed.",
            Self::Window => "All tabs and terminals will be closed.",
        }
    }
}

/// Runtime font-size bounds in logical points.
const MIN_FONT_POINTS: f32 = 6.0;
const MAX_FONT_POINTS: f32 = 48.0;

/// Tab tint palette offered by the "Tab Color" context-menu submenu, mirroring
/// Ghostty's set. The menu also offers a "None" entry that clears the tint.
const TAB_COLORS: &[(&str, egui::Color32)] = &[
    ("Blue", egui::Color32::from_rgb(0x32, 0x7e, 0xff)),
    ("Purple", egui::Color32::from_rgb(0x9b, 0x59, 0xf6)),
    ("Pink", egui::Color32::from_rgb(0xff, 0x5c, 0xa8)),
    ("Red", egui::Color32::from_rgb(0xe7, 0x4c, 0x3c)),
    ("Orange", egui::Color32::from_rgb(0xf5, 0x9e, 0x0b)),
    ("Yellow", egui::Color32::from_rgb(0xf1, 0xc4, 0x0f)),
    ("Green", egui::Color32::from_rgb(0x2e, 0xcc, 0x71)),
    ("Teal", egui::Color32::from_rgb(0x1a, 0xbc, 0x9c)),
    ("Graphite", egui::Color32::from_rgb(0x60, 0x6a, 0x76)),
];

impl Window {
    /// Build the first window, doing the once-per-process setup on the way:
    /// the egui context options, the UI fallback font, the glyph atlas
    /// (`render::init`) and shell-profile detection. [`App::spawn_window`]
    /// builds every later window from an existing one instead.
    fn first(cc: &eframe::CreationContext<'_>, window_id: u64) -> Result<Self> {
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
        cc.egui_ctx.options_mut(|o| o.zoom_with_keyboard = false);
        install_ui_fallback_font(&cc.egui_ctx);
        let ppp = cc.egui_ctx.pixels_per_point().max(1.0);
        let px = (config.font_points * ppp).round();
        let (cell_w, cell_h) = render::init(render_state, px, config.text_gamma, &font_spec(&config));

        let (profiles, default_profile) = profiles::detect(config.shell.as_deref());
        let first = Session::new(&cc.egui_ctx, &config, &profiles[default_profile], None)?;

        let keymap = Keymap::from_config(&config.keybinds);

        // `main.rs` decides transparency from the same keys before the window is
        // created; recompute it here rather than plumbing a flag through eframe.
        let transparent_surface =
            config.background_opacity < 1.0 || config.background_blur.enabled();
        let hwnd = crate::blur::hwnd_of(cc);
        if transparent_surface {
            // Transparency fails *silently* if the surface didn't come from a
            // DirectComposition visual (see main.rs): egui-wgpu logs one warning
            // we don't surface and falls back to opaque. eframe exposes no way to
            // read the chosen alpha mode back, so report the backend — anything
            // other than Dx12 means the DComp path wasn't taken and the window
            // will stay solid.
            let backend = render_state.adapter.get_info().backend;
            eprintln!(
                "giest: transparency requested (background-opacity = {:.2}); \
                 rendering on {backend:?}",
                config.background_opacity
            );
        }
        if config.background_blur.enabled() && config.background_opacity >= 1.0 {
            eprintln!(
                "giest: background-blur has no visible effect at background-opacity = 1 — \
                 the blur shows *through* the window, so lower the opacity to see it."
            );
        }

        let app = Self {
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
            last_layout: Vec::new(),
            renaming: None,
            palette: None,
            keymap,
            fullscreen: false,
            hwnd,
            transparent_surface,
            tab_drag: None,
            confirm: None,
            closing: false,
            bell_title: false,
            was_focused: true,
            window_id,
            is_root: true,
            pending_bell: false,
            geom: None,
            requests: Vec::new(),
        };
        app.apply_backdrop();
        Ok(app)
    }

    /// Whether this window is the one drawn into [`egui::ViewportId::ROOT`].
    /// Maintained by [`App`], which keeps the root at slot 0 and re-stamps it on
    /// a rehost — closing the root is the only close that can end the process.
    fn is_root(&self) -> bool {
        self.is_root
    }

    /// Namespace an egui `Id` to this window.
    ///
    /// egui keys `Memory::areas`, `focus` and `interactions` by viewport, but
    /// **not `Memory::data`** — which is where `TextEditState`, `ScrollArea`
    /// offsets and `PanelState` live. Two windows reusing a literal id would
    /// therefore share a text cursor (palette, search, tab rename) and a tab-strip
    /// height. Keyed on the stable `window_id` rather than the slot, so a
    /// rehost can't shuffle widget state between windows.
    fn id(&self, what: impl std::hash::Hash) -> egui::Id {
        egui::Id::new(("giest-window", self.window_id, what))
    }

    /// Fire the out-of-band bell effects for the configured `bell-features`.
    /// Called once per frame when any pane rang, so several simultaneous bells
    /// produce one beep rather than one per pane.
    fn ring_bell(&mut self, ctx: &egui::Context) {
        let bell = self.config.bell;
        if bell.system {
            crate::bell::system_alert();
        }
        if bell.audio {
            if let Some(raw) = self.config.bell_audio_path.as_deref() {
                let dir = crate::config::config_path();
                let dir = dir.as_deref().and_then(|p| p.parent());
                if let Some(path) = crate::bell::resolve_audio_path(raw, dir) {
                    crate::bell::play_audio(&path);
                }
            }
        }
        // Ghostty requests attention only when the window is *unfocused* — a
        // taskbar flash on the window you're already looking at is just noise.
        if bell.attention && !ctx.input(|i| i.focused) {
            if let Some(hwnd) = self.hwnd {
                crate::bell::request_attention(hwnd);
            }
        }
        if bell.title {
            self.bell_title = true;
        }
    }

    /// Draw the close-confirmation modal, if one is pending, and act on the
    /// answer. Deferred intent like `render_palette`: the pending close is taken
    /// out and either put back, dropped (Cancel), or executed (Close).
    fn render_confirm_close(&mut self, ctx: &egui::Context) {
        let Some(what) = self.confirm else {
            return;
        };
        let mut decision: Option<bool> = None;
        let modal = egui::Modal::new(self.id("confirm-close")).show(ctx, |ui| {
            ui.set_width(320.0);
            ui.heading("Close terminal?");
            ui.add_space(6.0);
            ui.label(what.description());
            ui.add_space(12.0);
            ui.horizontal(|ui| {
                if ui.button("Cancel").clicked() {
                    decision = Some(false);
                }
                if ui.button("Close").clicked() {
                    decision = Some(true);
                }
            });
        });
        // Esc or a backdrop click cancels, like the palette.
        if modal.should_close() {
            decision = Some(false);
        }
        if ctx.input(|i| i.key_pressed(egui::Key::Enter)) {
            decision = Some(true);
        }
        match decision {
            Some(true) => {
                self.confirm = None;
                self.apply_close(what);
            }
            Some(false) => self.confirm = None,
            None => {}
        }
    }

    /// Apply the configured `background-blur` to the window (a no-op when the key
    /// is off). Safe to call repeatedly — the DWM attributes are set on the live
    /// window, so this is how a config reload takes effect.
    fn apply_backdrop(&self) {
        let Some(hwnd) = self.hwnd else {
            return;
        };
        // A backdrop can only show through pixels we actually left transparent.
        if !self.transparent_surface && !self.config.background_blur.enabled() {
            return;
        }
        crate::blur::apply(
            hwnd,
            self.config.background_blur,
            self.config.bg,
            self.config.background_opacity,
        );
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

    /// Build a sibling window from this one: same config/profiles/keymap/font
    /// metrics, a fresh session, and none of the per-window UI state.
    ///
    /// Returns `None` if the shell couldn't be spawned — a window with no tabs
    /// would panic in `render_active`, which indexes `tabs[active_tab]`.
    fn sibling(&self, window_id: u64, cwd: Option<&std::path::Path>) -> Option<Self> {
        let session = self.spawn_session(self.default_profile, cwd)?;
        Some(Self {
            tabs: vec![Tab::leaf(1, session)],
            active_tab: 0,
            next_id: 2,
            cell_w: self.cell_w,
            cell_h: self.cell_h,
            font_points: self.font_points,
            config: self.config.clone(),
            profiles: self.profiles.clone(),
            default_profile: self.default_profile,
            egui_ctx: self.egui_ctx.clone(),
            last_window_title: None,
            last_layout: Vec::new(),
            renaming: None,
            palette: None,
            keymap: self.keymap.clone(),
            fullscreen: false,
            // Only the root viewport has a reachable window handle, so a
            // secondary window gets no DWM backdrop and no taskbar flash.
            hwnd: None,
            transparent_surface: self.transparent_surface,
            tab_drag: None,
            confirm: None,
            closing: false,
            bell_title: false,
            was_focused: true,
            window_id,
            is_root: false,
            pending_bell: false,
            geom: None,
            requests: Vec::new(),
        })
    }

    /// The `ViewportBuilder` for this window as a child viewport.
    ///
    /// Built once and cloned verbatim every pass. Several `ViewportBuilder`
    /// fields force a full window recreation when patched, and eframe's recreate
    /// path clears **every** viewport's surface (not just this one) — a visible
    /// hitch on the root too. Anything dynamic goes through `ViewportCommand`.
    fn child_builder(&self) -> egui::ViewportBuilder {
        egui::ViewportBuilder::default()
            .with_title("giest")
            .with_inner_size([960.0, 600.0])
            // NOT optional. The vendored egui-winit patch reads `transparent` to
            // set WS_EX_NOREDIRECTIONBITMAP at creation, and children go through
            // the same `create_window`. Omit it and a secondary window renders as
            // the solid grey wash CLAUDE.md documents.
            .with_transparent(self.transparent_surface)
    }

    /// This window's child viewport id. Derived from the stable `window_id`, not
    /// the slot, so a window keeps its native window across a list reshuffle.
    fn viewport_id(&self) -> egui::ViewportId {
        egui::ViewportId(egui::Id::new(("giest-viewport", self.window_id)))
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
        let profile = self
            .profiles
            .get(idx)
            .unwrap_or(&self.profiles[self.default_profile]);
        Session::new(&self.egui_ctx, &self.config, profile, cwd)
            .map_err(|e| eprintln!("giest: failed to open session: {e}"))
            .ok()
    }

    /// Open a new tab running profile `idx`. When
    /// `tab-inherit-working-directory` is set (the default), the new tab starts in
    /// the previously focused pane's working directory (reported via OSC 7),
    /// matching Ghostty; otherwise it uses the profile's default directory.
    /// The focused pane's working directory (reported via OSC 7), or `None` if
    /// this shell never reported one. The single source for every
    /// inherit-working-directory path — Ghostty likewise resolves it from the
    /// previously focused surface rather than from the new surface's parent.
    fn focused_pwd(&self) -> Option<std::path::PathBuf> {
        let tab = self.tabs.get(self.active_tab)?;
        tab.root.payload(tab.focus).and_then(|s| s.pwd())
    }

    fn new_tab(&mut self, idx: usize) {
        let cwd = should_inherit_cwd(NewSurface::Tab, &self.config)
            .then(|| self.focused_pwd())
            .flatten();
        if let Some(s) = self.spawn_session(idx, cwd.as_deref()) {
            let id = self.alloc_id();
            self.tabs.push(Tab::leaf(id, s));
            self.active_tab = self.tabs.len() - 1;
        }
    }

    /// Split only the focused pane along `vertical` axis, focusing the new pane.
    /// The rest of the tab's split layout is untouched (splits nest). The new
    /// pane inherits the focused pane's working directory (via OSC 7).
    fn split(&mut self, vertical: bool) {
        let cwd = should_inherit_cwd(NewSurface::Split, &self.config)
            .then(|| self.focused_pwd())
            .flatten();
        if let Some(s) = self.spawn_session(self.default_profile, cwd.as_deref()) {
            let id = self.alloc_id();
            let tab = &mut self.tabs[self.active_tab];
            let focus = tab.focus;
            tab.root.split_leaf(focus, vertical, id, s);
            tab.focus = id;
            // A new split changes the layout, so any zoom is no longer meaningful.
            tab.zoomed = None;
        }
    }

    /// Toggle "split zoom": the focused pane fills the whole tab area, hiding the
    /// other splits; toggling again restores the split layout (Ghostty's
    /// `toggle_split_zoom`). A no-op in a tab with a single pane.
    fn toggle_split_zoom(&mut self) {
        let tab = &mut self.tabs[self.active_tab];
        if tab.leaf_count() <= 1 {
            tab.zoomed = None;
            return;
        }
        tab.zoomed = if tab.zoomed == Some(tab.focus) {
            None
        } else {
            Some(tab.focus)
        };
    }

    /// Toggle the window between fullscreen and windowed (Ghostty
    /// `toggle_fullscreen`). Reads the live viewport state when winit reports it
    /// (so an OS-driven change — e.g. the title-bar button — doesn't desync our
    /// flag), falling back to our own tracked flag, then commands the inverse.
    fn toggle_fullscreen(&mut self, ctx: &egui::Context) {
        let current = ctx
            .input(|i| i.viewport().fullscreen)
            .unwrap_or(self.fullscreen);
        self.fullscreen = !current;
        ctx.send_viewport_cmd(egui::ViewportCommand::Fullscreen(self.fullscreen));
    }

    /// Whether any modal overlay owns input this frame. The palette, the search
    /// bar and the close-confirmation dialog all suppress terminal keys, pane
    /// mouse handling and app shortcuts.
    fn modal_open(&self) -> bool {
        self.palette.is_some() || self.confirm.is_some() || self.focused_search_open()
    }

    /// Sessions affected by a pending close, for the busy check.
    fn close_targets(&mut self, what: PendingClose) -> Vec<Option<bool>> {
        let active = self.active_tab;
        let mut out = Vec::new();
        match what {
            PendingClose::Pane => {
                // Only the focused pane dies (or, if it's the last one, the tab
                // it's alone in — same session either way).
                let tab = &mut self.tabs[active];
                let focus = tab.focus;
                if let Some(s) = tab.root.payload_mut(focus) {
                    out.push(s.looks_busy());
                }
            }
            PendingClose::Tab(i) => {
                if let Some(tab) = self.tabs.get_mut(i) {
                    tab.root.for_each_mut(&mut |s: &mut Session| out.push(s.looks_busy()));
                }
            }
            PendingClose::OtherTabs(keep) => {
                for (i, tab) in self.tabs.iter_mut().enumerate() {
                    if i != keep {
                        tab.root.for_each_mut(&mut |s: &mut Session| out.push(s.looks_busy()));
                    }
                }
            }
            PendingClose::TabsToRight(from) => {
                for tab in self.tabs.iter_mut().skip(from + 1) {
                    tab.root.for_each_mut(&mut |s: &mut Session| out.push(s.looks_busy()));
                }
            }
            PendingClose::Window => {
                for tab in &mut self.tabs {
                    tab.root.for_each_mut(&mut |s: &mut Session| out.push(s.looks_busy()));
                }
            }
        }
        out
    }

    /// Ask for confirmation before `what`, or just do it.
    ///
    /// NOTE: this is only for *user-initiated* closes. A pane whose shell exited
    /// is reaped by `reap_dead` without ever coming through here — there is
    /// nothing to confirm once the process is gone.
    fn request_close(&mut self, what: PendingClose) {
        let mode = self.config.confirm_close;
        // Confirm if *any* affected pane is (or might be) busy.
        let busy = self
            .close_targets(what)
            .into_iter()
            .reduce(|a, b| match (a, b) {
                (Some(true), _) | (_, Some(true)) => Some(true),
                (None, _) | (_, None) => None,
                _ => Some(false),
            })
            .flatten();
        if crate::config::needs_confirm(mode, busy) {
            self.confirm = Some(what);
        } else {
            self.apply_close(what);
        }
    }

    /// Perform a close that has been confirmed (or didn't need confirming).
    fn apply_close(&mut self, what: PendingClose) {
        match what {
            PendingClose::Pane => self.close_focused(),
            PendingClose::Tab(i) => self.close_tab(i),
            PendingClose::OtherTabs(i) => self.close_other_tabs(i),
            PendingClose::TabsToRight(i) => self.close_tabs_to_right(i),
            PendingClose::Window => {
                // Latch so the `Close` that `App` issues for the root doesn't
                // come straight back as another `close_requested()` and re-open
                // this dialog forever.
                self.closing = true;
                self.requests.push(AppRequest::CloseWindow);
            }
        }
    }

    /// Close the focused pane; closing the last pane closes the tab, and the
    /// last tab closes the window.
    fn close_focused(&mut self) {
        let tab = &mut self.tabs[self.active_tab];
        if tab.leaf_count() > 1 {
            let focus = tab.focus;
            let root = std::mem::replace(&mut tab.root, Node::Empty);
            tab.root = root.remove_leaf(focus).unwrap_or(Node::Empty);
            tab.focus = tab.root.first_leaf_id();
            // Closing a pane changes the layout; drop any (now stale) zoom.
            tab.zoomed = None;
        } else if self.tabs.len() > 1 {
            self.tabs.remove(self.active_tab);
            self.active_tab = self.active_tab.min(self.tabs.len() - 1);
        } else {
            self.requests.push(AppRequest::CloseWindow);
        }
    }

    /// Switch to tab `idx` if it exists (Alt+number).
    fn goto_tab(&mut self, idx: usize) {
        if idx < self.tabs.len() {
            self.active_tab = idx;
        }
    }

    /// Activate the next tab, wrapping (Ctrl+Tab / palette "Next Tab").
    fn next_tab(&mut self) {
        let n = self.tabs.len();
        if n > 1 {
            self.active_tab = (self.active_tab + 1) % n;
        }
    }

    /// Activate the previous tab, wrapping (Ctrl+Shift+Tab / palette "Previous Tab").
    fn prev_tab(&mut self) {
        let n = self.tabs.len();
        if n > 1 {
            self.active_tab = (self.active_tab + n - 1) % n;
        }
    }

    /// Move focus to the spatially adjacent pane (`Ctrl+Alt+arrow`), using the
    /// previous frame's cached layout. A no-op if there is no neighbor that way.
    fn focus_dir(&mut self, dir: Dir) {
        // While a split is zoomed the siblings are hidden; don't navigate to one.
        if self.tabs[self.active_tab].zoomed.is_some() {
            return;
        }
        let focus = self.tabs[self.active_tab].focus;
        if let Some(id) = nav_dir(&self.last_layout, focus, dir) {
            self.tabs[self.active_tab].focus = id;
        }
    }

    /// Cycle focus to the next/previous pane in creation order (`Ctrl+Shift+]` /
    /// `Ctrl+Shift+[`), wrapping around. Leaf ids are monotonic, so sorting them
    /// ascending recovers creation order (matching Ghostty's `goto_split:next`).
    fn focus_cycle(&mut self, forward: bool) {
        let tab = &mut self.tabs[self.active_tab];
        // While a split is zoomed the siblings are hidden; don't cycle into one.
        if tab.zoomed.is_some() {
            return;
        }
        let mut ids = Vec::new();
        tab.root.leaf_ids(&mut ids);
        ids.sort_unstable();
        if let Some(id) = cycle_pick(&ids, tab.focus, forward) {
            tab.focus = id;
        }
    }

    /// Move tab `from` to insertion slot `to` (a drag reorder), keeping the same
    /// tab selected.
    fn move_tab(&mut self, from: usize, to: usize) {
        let tabs = std::mem::take(&mut self.tabs);
        let (tabs, active) = reorder_tabs(tabs, from, to, self.active_tab);
        self.tabs = tabs;
        self.active_tab = active;
        // `renaming` holds a tab *index*, which the move just invalidated —
        // leaving it would rename whichever tab slid into that slot.
        self.renaming = None;
    }

    /// Close tab `idx`; closing the last tab closes the window.
    fn close_tab(&mut self, idx: usize) {
        if idx >= self.tabs.len() {
            return;
        }
        self.tabs.remove(idx);
        if self.active_tab > idx {
            self.active_tab -= 1;
        }
        if self.tabs.is_empty() {
            self.requests.push(AppRequest::CloseWindow);
        } else {
            self.active_tab = self.active_tab.min(self.tabs.len() - 1);
        }
    }

    /// Close every tab except `keep`, leaving it focused (Ghostty's "Close Other
    /// Tabs"). A no-op if `keep` is out of range; always leaves one tab.
    fn close_other_tabs(&mut self, keep: usize) {
        let tabs = std::mem::take(&mut self.tabs);
        let (tabs, active) = keep_only_tab(tabs, keep, self.active_tab);
        self.tabs = tabs;
        self.active_tab = active;
    }

    /// Close every tab to the right of `idx` (Ghostty's "Close Tabs to the
    /// Right"), clamping the active tab into the survivors.
    fn close_tabs_to_right(&mut self, idx: usize) {
        let tabs = std::mem::take(&mut self.tabs);
        let (tabs, active) = truncate_tabs_to_right(tabs, idx, self.active_tab);
        self.tabs = tabs;
        self.active_tab = active;
    }

    /// Remove panes whose shell has exited; drop tabs that become empty and
    /// close the window when the last tab is gone. Returns `false` if the
    /// window is closing (caller should skip rendering this frame).
    fn reap_dead(&mut self) -> bool {
        let before = self.tabs.len();
        let tabs = std::mem::take(&mut self.tabs);
        let (survivors, active) =
            reap_tabs(tabs, self.active_tab, &mut |s: &Session| !s.is_alive());
        self.tabs = survivors;
        if self.tabs.is_empty() {
            self.requests.push(AppRequest::CloseWindow);
            return false;
        }
        if self.tabs.len() != before {
            // `PendingClose::Tab(i)` and the tab-drag latch both hold indices a
            // reap can invalidate; drop them rather than act on the wrong tab.
            self.confirm = None;
            self.tab_drag = None;
        }
        self.active_tab = active;
        true
    }

    /// App-level shortcuts, resolved through the [`Keymap`] (built-in defaults
    /// plus the config's `keybind` overrides). Each pressed-key event is turned
    /// into a [`Chord`] and dispatched to [`Self::execute_action`]. These chords
    /// live in the modifier namespaces `Session::decide_key` reserves, so the
    /// shell never sees them. `render_state` is `None` here — none of the
    /// keymap-dispatched actions need it (font/reload have their own paths).
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
            let Some(code) = session::map_egui_key(*key) else {
                continue;
            };
            let chord = Chord {
                mods: session::key_mods(modifiers),
                code,
            };
            if let Some(action) = self.keymap.lookup(&chord) {
                self.execute_action(ctx, None, action);
            }
        }
    }

    /// Build the command-palette catalog for the current shell profiles.
    fn build_catalog(&self) -> Vec<command::Command> {
        let names: Vec<String> = self.profiles.iter().map(|p| p.name.clone()).collect();
        command::build_catalog(&names)
    }

    /// Whether the focused pane's scrollback-search overlay is open. While it is,
    /// the overlay owns the keyboard (it's modal like the palette), so app
    /// shortcuts and the pane's terminal input are suppressed.
    fn focused_search_open(&self) -> bool {
        self.tabs
            .get(self.active_tab)
            .is_some_and(|t| t.focused_payload().search_active())
    }

    /// The focused pane's session, if any (the palette's pane-scoped actions —
    /// copy/paste/select/scroll — act on it).
    fn focused_session_mut(&mut self) -> Option<&mut Session> {
        let tab = self.tabs.get_mut(self.active_tab)?;
        let focus = tab.focus;
        tab.root.payload_mut(focus)
    }

    /// Nudge the font size by `delta` points (palette font commands; needs the
    /// render state to rebuild the atlas, like `handle_font_zoom`).
    fn font_zoom_by(&mut self, render_state: Option<&egui_wgpu::RenderState>, delta: f32) {
        if let Some(rs) = render_state {
            let ppp = self.egui_ctx.pixels_per_point().max(1.0);
            self.set_font_points(rs, self.font_points + delta, ppp);
        }
    }

    /// Reset the font size to the configured default (palette "Reset Font Size").
    fn font_reset(&mut self, render_state: Option<&egui_wgpu::RenderState>) {
        if let Some(rs) = render_state {
            let ppp = self.egui_ctx.pixels_per_point().max(1.0);
            self.set_font_points(rs, self.config.font_points, ppp);
        }
    }

    /// Reload the config file and re-apply what can change at runtime: the color
    /// theme/cursor (re-applied to every live engine) and the font size.
    /// Selection colors, padding, click actions, and the opacities are read fresh
    /// each frame, so they take effect on the next frame from `self.config`. NOTE:
    /// the scrollback limit is fixed at engine creation and is not changed here,
    /// and whether the *window* can be transparent at all is fixed at startup.
    fn reload_config(&mut self, render_state: Option<&egui_wgpu::RenderState>) {
        let cfg = Config::load();
        for tab in &mut self.tabs {
            tab.root.for_each_mut(&mut |s: &mut Session| s.apply_config(&cfg));
        }
        let new_font = cfg.font_points;
        self.keymap = Keymap::from_config(&cfg.keybinds);
        // Transparency is a property of the surface, requested once before the
        // window exists (see `main.rs`). Opacity *values* apply live, but turning
        // transparency on or off crosses that line and needs a restart — say so
        // rather than leaving the user wondering why nothing happened.
        let wants_transparent = cfg.background_opacity < 1.0 || cfg.background_blur.enabled();
        if wants_transparent && !self.transparent_surface {
            eprintln!(
                "giest: background-opacity/background-blur need a transparent window, \
                 which is set up at startup — restart giest to apply them."
            );
        }
        self.config = cfg;
        self.apply_backdrop();
        if let Some(rs) = render_state {
            let ppp = self.egui_ctx.pixels_per_point().max(1.0);
            self.set_font_points(rs, new_font, ppp);
        }
    }

    /// Run a command chosen from the palette by dispatching to the existing
    /// app/session methods. App-level actions mutate `self` directly; pane-scoped
    /// ones act on the focused session. `render_state` is needed only by the font
    /// and reload actions (it's `None` when unavailable, making those a no-op).
    fn execute_action(
        &mut self,
        ctx: &egui::Context,
        render_state: Option<&egui_wgpu::RenderState>,
        action: Action,
    ) {
        match action {
            Action::NewTab => self.new_tab(self.default_profile),
            Action::NewWindow => {
                // Resolve the cwd here, from *this* window's focused pane —
                // Ghostty likewise reads it from the previously focused surface
                // rather than from the new window.
                let cwd = should_inherit_cwd(NewSurface::Window, &self.config)
                    .then(|| self.focused_pwd())
                    .flatten();
                self.requests.push(AppRequest::NewWindow(cwd));
            }
            Action::CloseWindow => self.request_close(PendingClose::Window),
            Action::NewTabWithProfile(i) => self.new_tab(i),
            Action::CloseTab => self.request_close(PendingClose::Tab(self.active_tab)),
            Action::CloseOtherTabs => {
                self.request_close(PendingClose::OtherTabs(self.active_tab))
            }
            Action::CloseTabsToRight => {
                self.request_close(PendingClose::TabsToRight(self.active_tab))
            }
            Action::NextTab => self.next_tab(),
            Action::PrevTab => self.prev_tab(),
            Action::GotoTab(i) => self.goto_tab(i as usize),
            Action::LastTab => self.goto_tab(self.tabs.len().saturating_sub(1)),
            Action::TogglePalette => {
                self.palette = Some(PaletteState::new(self.build_catalog()))
            }
            Action::ToggleSearch => {
                if let Some(s) = self.focused_session_mut() {
                    if s.search_active() {
                        s.close_search();
                    } else {
                        s.open_search();
                    }
                }
            }
            Action::SplitRight => self.split(true),
            Action::SplitDown => self.split(false),
            Action::ToggleSplitZoom => self.toggle_split_zoom(),
            Action::ClosePane => self.request_close(PendingClose::Pane),
            Action::ToggleFullscreen => self.toggle_fullscreen(ctx),
            Action::FocusSplitLeft => self.focus_dir(Dir::Left),
            Action::FocusSplitRight => self.focus_dir(Dir::Right),
            Action::FocusSplitUp => self.focus_dir(Dir::Up),
            Action::FocusSplitDown => self.focus_dir(Dir::Down),
            Action::FocusSplitNext => self.focus_cycle(true),
            Action::FocusSplitPrev => self.focus_cycle(false),
            Action::IncreaseFontSize => self.font_zoom_by(render_state, 1.0),
            Action::DecreaseFontSize => self.font_zoom_by(render_state, -1.0),
            Action::ResetFontSize => self.font_reset(render_state),
            Action::Copy => {
                if let Some(s) = self.focused_session_mut() {
                    if let Some(text) = s.selection_text() {
                        ctx.copy_text(text);
                        s.clear_selection();
                    }
                }
            }
            Action::Paste => {
                if let Some(text) = session::read_clipboard() {
                    if let Some(s) = self.focused_session_mut() {
                        s.paste_str(&text);
                    }
                }
            }
            Action::SelectAll => {
                if let Some(s) = self.focused_session_mut() {
                    s.select_all();
                }
            }
            Action::ClearSelection => {
                if let Some(s) = self.focused_session_mut() {
                    s.clear_selection();
                }
            }
            Action::ResetTerminal => {
                if let Some(s) = self.focused_session_mut() {
                    s.reset();
                }
            }
            Action::ScrollPageUp => {
                let ch = self.cell_h;
                if let Some(s) = self.focused_session_mut() {
                    let page = s.page_lines();
                    s.scroll_lines(-page, ch);
                }
            }
            Action::ScrollPageDown => {
                let ch = self.cell_h;
                if let Some(s) = self.focused_session_mut() {
                    let page = s.page_lines();
                    s.scroll_lines(page, ch);
                }
            }
            Action::ScrollToTop => {
                if let Some(s) = self.focused_session_mut() {
                    s.scroll_to_top();
                }
            }
            Action::ScrollToBottom => {
                if let Some(s) = self.focused_session_mut() {
                    s.scroll_to_bottom_view();
                }
            }
            Action::JumpToPrompt(delta) => {
                let ch = self.cell_h;
                if let Some(s) = self.focused_session_mut() {
                    s.jump_to_prompt(delta as isize, ch);
                }
            }
            Action::OpenConfig => open_config(),
            Action::ReloadConfig => self.reload_config(render_state),
        }
        ctx.request_repaint();
    }

    /// Render the open command palette (if any) and return the action the user
    /// chose this frame. Ghostty-style: a dimmed click-to-dismiss backdrop and a
    /// centered, fuzzy-filtered command list. Uses the deferred-intent pattern —
    /// the `PaletteState` is taken out and either put back or dropped — so the
    /// returned action runs in `ui` with no borrow held on `self`.
    fn render_palette(&mut self, ctx: &egui::Context) -> Option<Action> {
        let mut state = self.palette.take()?;
        // The opening Ctrl+Shift+P is still queued on the first frame; remember
        // that so the toggle-close below doesn't immediately re-close it.
        let opened_this_frame = state.just_opened;
        let mut chosen: Option<Action> = None;
        let mut keep_open = true;
        let screen = ctx.content_rect();

        // Dimmed backdrop: above the panes (Middle), below the modal (Foreground);
        // a click anywhere on it closes the palette.
        egui::Area::new(self.id("palette-backdrop"))
            .order(egui::Order::Middle)
            .fixed_pos(egui::Pos2::ZERO)
            .show(ctx, |ui| {
                let resp = ui.allocate_rect(screen, egui::Sense::click());
                ui.painter()
                    .rect_filled(screen, 0.0, egui::Color32::from_black_alpha(160));
                if resp.clicked() {
                    keep_open = false;
                }
            });

        let width = (screen.width() * 0.6).clamp(560.0, 900.0).min(screen.width() - 40.0);
        egui::Area::new(self.id("palette"))
            .order(egui::Order::Foreground)
            .anchor(egui::Align2::CENTER_TOP, egui::vec2(0.0, screen.height() * 0.12))
            .show(ctx, |ui| {
                egui::Frame::NONE
                    .fill(ui.visuals().window_fill)
                    .stroke(ui.visuals().window_stroke)
                    .corner_radius(egui::CornerRadius::same(10))
                    .inner_margin(egui::Margin::same(10))
                    .show(ui, |ui| {
                        ui.set_width(width);

                        // Search box: auto-focus once; reset the selection on edit.
                        let resp = ui.add(
                            egui::TextEdit::singleline(&mut state.query)
                                .hint_text("Execute a command…")
                                .desired_width(f32::INFINITY)
                                .font(egui::FontId::proportional(20.0))
                                .margin(egui::Margin::symmetric(12, 10)),
                        );
                        if state.just_opened {
                            resp.request_focus();
                            state.just_opened = false;
                        }
                        if resp.changed() {
                            state.selected = 0;
                        }
                        // Enter in a singleline field arrives as lost_focus + the
                        // Enter press — the canonical egui "submit" signal.
                        let entered =
                            resp.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter));

                        let filtered = command::filter_commands(&state.catalog, &state.query);
                        let n = filtered.len();

                        // Navigation: Up/Down (and Ctrl+P/Ctrl+N) move the
                        // selection; consume them so the text box doesn't also act.
                        let (mut up, mut down) = (false, false);
                        ui.input_mut(|i| {
                            if i.consume_key(egui::Modifiers::NONE, egui::Key::ArrowDown)
                                || i.consume_key(egui::Modifiers::CTRL, egui::Key::N)
                            {
                                down = true;
                            }
                            if i.consume_key(egui::Modifiers::NONE, egui::Key::ArrowUp)
                                || i.consume_key(egui::Modifiers::CTRL, egui::Key::P)
                            {
                                up = true;
                            }
                        });
                        if ui.input(|i| i.key_pressed(egui::Key::Escape)) {
                            keep_open = false;
                        }
                        // Ctrl+Shift+P toggles the palette closed (parity with
                        // Ghostty's toggle binding), except on the frame it opened
                        // (the opening keypress is still in the queue).
                        let toggled = ui.input_mut(|i| {
                            i.consume_key(
                                egui::Modifiers {
                                    ctrl: true,
                                    shift: true,
                                    ..Default::default()
                                },
                                egui::Key::P,
                            )
                        });
                        if toggled && !opened_this_frame {
                            keep_open = false;
                        }
                        if n > 0 {
                            if down {
                                state.selected = (state.selected + 1) % n;
                            }
                            if up {
                                state.selected = (state.selected + n - 1) % n;
                            }
                        }
                        if state.selected >= n {
                            state.selected = n.saturating_sub(1);
                        }
                        // Hover also moves the selection, but only while the
                        // pointer is moving, so it doesn't fight the keyboard.
                        let pointer_moved = ctx.input(|i| i.pointer.delta() != egui::Vec2::ZERO);

                        ui.separator();
                        egui::ScrollArea::vertical()
                            .max_height(screen.height() * 0.6)
                            .show(ui, |ui| {
                                let font = egui::FontId::proportional(16.0);
                                for (row, &cmd_idx) in filtered.iter().enumerate() {
                                    let selected = row == state.selected;
                                    let (rect, resp) = ui.allocate_exact_size(
                                        egui::vec2(ui.available_width(), 30.0),
                                        egui::Sense::click(),
                                    );
                                    if resp.hovered() && pointer_moved {
                                        state.selected = row;
                                    }
                                    if selected {
                                        ui.painter().rect_filled(
                                            rect,
                                            egui::CornerRadius::same(4),
                                            ui.visuals().selection.bg_fill,
                                        );
                                    } else if resp.hovered() {
                                        ui.painter().rect_filled(
                                            rect,
                                            egui::CornerRadius::same(4),
                                            ui.visuals().widgets.hovered.weak_bg_fill,
                                        );
                                    }
                                    let cmd = &state.catalog[cmd_idx];
                                    let text_color = if selected {
                                        ui.visuals().selection.stroke.color
                                    } else {
                                        ui.visuals().text_color()
                                    };
                                    ui.painter().text(
                                        rect.left_center() + egui::vec2(10.0, 0.0),
                                        egui::Align2::LEFT_CENTER,
                                        &cmd.title,
                                        font.clone(),
                                        text_color,
                                    );
                                    if let Some(kb) = &cmd.keybind {
                                        ui.painter().text(
                                            rect.right_center() - egui::vec2(10.0, 0.0),
                                            egui::Align2::RIGHT_CENTER,
                                            kb,
                                            font.clone(),
                                            ui.visuals().weak_text_color(),
                                        );
                                    }
                                    if selected && (up || down) {
                                        resp.scroll_to_me(Some(egui::Align::Center));
                                    }
                                    if resp.clicked() {
                                        chosen = Some(cmd.action);
                                        keep_open = false;
                                    }
                                }
                            });

                        if entered {
                            if let Some(&idx) = filtered.get(state.selected) {
                                chosen = Some(state.catalog[idx].action);
                            }
                            keep_open = false;
                        }
                    });
            });

        // Put the state back unless the palette was dismissed or fired an action.
        if chosen.is_none() && keep_open {
            self.palette = Some(state);
        }
        chosen
    }

    /// Render the focused pane's scrollback-search overlay (a compact top-right
    /// bar) when its search is open, and apply the resulting edits to the session.
    /// Modal for the keyboard like the palette: it owns Enter / Shift+Enter (next
    /// / previous match), Esc and Ctrl+Shift+F (close), and the query box. Edits
    /// are deferred to after the egui closure so no session borrow spans it.
    fn render_search(&mut self, ctx: &egui::Context) {
        let cell_h = self.cell_h;
        // Snapshot the overlay state (and clear the one-shot focus flag).
        let (mut query, count, current, case, just_opened) = {
            let Some(s) = self.focused_session_mut() else {
                return;
            };
            if !s.search_active() {
                return;
            }
            let just = s.take_search_just_opened();
            let st = s.search_state().expect("search active");
            (
                st.query.clone(),
                st.count(),
                st.current,
                st.case_sensitive,
                just,
            )
        };

        let mut close = false;
        let mut changed = false;
        let mut next = false;
        let mut prev = false;
        let mut toggle_case = false;

        egui::Area::new(self.id("search"))
            .order(egui::Order::Foreground)
            .anchor(egui::Align2::RIGHT_TOP, egui::vec2(-16.0, 8.0))
            .show(ctx, |ui| {
                egui::Frame::NONE
                    .fill(ui.visuals().window_fill)
                    .stroke(ui.visuals().window_stroke)
                    .corner_radius(egui::CornerRadius::same(8))
                    .inner_margin(egui::Margin::same(8))
                    .show(ui, |ui| {
                        ui.horizontal(|ui| {
                            let resp = ui.add(
                                egui::TextEdit::singleline(&mut query)
                                    .hint_text("Find…")
                                    .desired_width(220.0)
                                    .font(egui::FontId::proportional(16.0)),
                            );
                            if just_opened {
                                resp.request_focus();
                            }
                            if resp.changed() {
                                changed = true;
                            }
                            // Enter submits → next match; Shift+Enter → previous.
                            // Re-focus so the box keeps the keyboard after submit.
                            if resp.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter)) {
                                if ui.input(|i| i.modifiers.shift) {
                                    prev = true;
                                } else {
                                    next = true;
                                }
                                resp.request_focus();
                            }
                            let label = if count == 0 {
                                if query.is_empty() {
                                    String::new()
                                } else {
                                    "0/0".to_string()
                                }
                            } else {
                                format!("{}/{}", current + 1, count)
                            };
                            ui.add_sized([54.0, 0.0], egui::Label::new(label));
                            if ui
                                .selectable_label(case, "Aa")
                                .on_hover_text("Match case")
                                .clicked()
                            {
                                toggle_case = true;
                            }
                            if ui
                                .button("\u{2191}")
                                .on_hover_text("Previous (Shift+Enter)")
                                .clicked()
                            {
                                prev = true;
                            }
                            if ui.button("\u{2193}").on_hover_text("Next (Enter)").clicked() {
                                next = true;
                            }
                            if ui.button("\u{00d7}").on_hover_text("Close (Esc)").clicked() {
                                close = true;
                            }
                        });
                    });
            });

        // Overlay-level modal keys: Esc and the Ctrl+Shift+F toggle close it
        // (skip the toggle on the opening frame, whose keypress is still queued).
        if ctx.input(|i| i.key_pressed(egui::Key::Escape)) {
            close = true;
        }
        let toggled = ctx.input_mut(|i| {
            i.consume_key(
                egui::Modifiers {
                    ctrl: true,
                    shift: true,
                    ..Default::default()
                },
                egui::Key::F,
            )
        });
        if toggled && !just_opened {
            close = true;
        }

        // Apply, deferred so no session borrow is held across the egui closure.
        // Fetch the session once, then act on the collected flags.
        if let Some(s) = self.focused_session_mut() {
            if close {
                s.close_search();
            } else {
                if toggle_case {
                    s.toggle_search_case(cell_h);
                }
                if changed {
                    s.set_search_query(query, cell_h);
                }
                if next {
                    s.step_search(true, cell_h);
                }
                if prev {
                    s.step_search(false, cell_h);
                }
            }
        }
        ctx.request_repaint();
    }

    fn tab_bar(&mut self, ui: &mut egui::Ui) {
        // Deferred intents collected while `self.tabs` is borrowed immutably for
        // iteration, then applied after the loop (the strip can't mutate the tabs
        // it's drawing). The `×`, middle-click, and right-click menu all feed these.
        let mut switch_to = None;
        let mut want_close = None;
        let mut want_close_others: Option<usize> = None;
        let mut want_close_right: Option<usize> = None;
        // Profile index to open a new tab with (default unless the menu picks one).
        let mut want_new: Option<usize> = None;
        let mut want_rename: Option<usize> = None;
        let mut want_color: Option<(usize, Option<egui::Color32>)> = None;
        let mut commit_rename: Option<(usize, Option<String>)> = None;
        let mut stop_rename = false;
        // Drag-to-reorder: this frame's tab rects, the tab a drag just started
        // on, and the resulting move.
        let mut tab_rects: Vec<egui::Rect> = Vec::with_capacity(self.tabs.len());
        let mut drag_from: Option<usize> = None;
        let mut want_move: Option<(usize, usize)> = None;
        // Pull the in-progress rename out so its buffer can be edited as a local
        // (it can't stay borrowed from `self` while we iterate `self.tabs`).
        let mut renaming = std::mem::take(&mut self.renaming);

        // Hoist the data the menu closures need, so they capture plain locals
        // instead of `self` (which iteration already borrows).
        let active = self.active_tab;
        let ntabs = self.tabs.len();
        let default_profile = self.default_profile;
        let profile_names: Vec<String> = self.profiles.iter().map(|p| p.name.clone()).collect();

        ui.horizontal(|ui| {
            for (i, tab) in self.tabs.iter().enumerate() {
                let raw = tab
                    .name
                    .clone()
                    .or_else(|| tab.focused_payload().title())
                    .unwrap_or_else(|| format!("shell {}", i + 1));
                let mut label = ellipsize(&raw, 24);
                let count = tab.leaf_count();
                if count > 1 {
                    label = format!("{label} [{count}]");
                }
                let editing = matches!(&renaming, Some((ri, _)) if *ri == i);

                // Each tab is one tinted frame holding the title and a folded-in
                // close `×`. The tint (when set) is the only visible difference
                // between a plain and a colored tab.
                let fill = tab.color.unwrap_or(egui::Color32::TRANSPARENT);
                let frame = egui::Frame::NONE
                    .fill(fill)
                    .inner_margin(egui::Margin::symmetric(4, 1))
                    .corner_radius(egui::CornerRadius::same(4));
                let title_resp = frame
                    .show(ui, |ui| {
                        ui.spacing_mut().item_spacing.x = 2.0;
                        ui.horizontal(|ui| {
                            let resp = if editing {
                                let text = &mut renaming.as_mut().unwrap().1;
                                let te =
                                    ui.add(egui::TextEdit::singleline(text).desired_width(120.0));
                                if !te.has_focus() {
                                    te.request_focus();
                                }
                                // Escape cancels; Enter or clicking away commits
                                // (empty text clears the override).
                                if ui.input(|i| i.key_pressed(egui::Key::Escape)) {
                                    stop_rename = true;
                                } else if te.lost_focus() {
                                    let val = if text.trim().is_empty() {
                                        None
                                    } else {
                                        Some(text.clone())
                                    };
                                    commit_rename = Some((i, val));
                                    stop_rename = true;
                                }
                                te
                            } else {
                                let resp = ui.selectable_label(i == active, label);
                                if resp.clicked() {
                                    switch_to = Some(i);
                                }
                                // Middle-click closes the tab, like a browser.
                                if resp.clicked_by(egui::PointerButton::Middle) {
                                    want_close = Some(i);
                                }
                                resp
                            };
                            if ui
                                .small_button("×")
                                .on_hover_text("Close tab (Ctrl+Shift+W)")
                                .clicked()
                            {
                                want_close = Some(i);
                            }
                            resp
                        })
                        .inner
                    })
                    .inner;

                tab_rects.push(title_resp.rect);
                // Drag to reorder. A separate drag-sensing widget over the
                // *title* rect (not the whole frame, so the `×` stays clickable).
                // egui resolves click-hits and drag-hits independently, so the
                // label's click-to-switch and middle-click-to-close still fire.
                // Skipped while renaming, where a drag is text selection.
                if !editing {
                    let drag = ui.interact(
                        title_resp.rect,
                        self.id(("tab-drag", i)),
                        egui::Sense::drag(),
                    );
                    // Primary only: egui starts drags on *any* held button, so a
                    // middle-press-and-jiggle would otherwise begin a reorder
                    // instead of closing the tab.
                    if drag.drag_started_by(egui::PointerButton::Primary) {
                        drag_from = Some(i);
                    }
                }

                if !editing {
                    title_resp.context_menu(|ui| {
                        if ui.button("New Tab").clicked() {
                            want_new = Some(default_profile);
                            ui.close();
                        }
                        ui.menu_button("New Tab with shell", |ui| {
                            for (pi, name) in profile_names.iter().enumerate() {
                                if ui.button(name).clicked() {
                                    want_new = Some(pi);
                                    ui.close();
                                }
                            }
                        });
                        ui.separator();
                        if ui.button("Rename Tab…").clicked() {
                            want_rename = Some(i);
                            ui.close();
                        }
                        ui.menu_button("Tab Color", |ui| {
                            if ui.button("None").clicked() {
                                want_color = Some((i, None));
                                ui.close();
                            }
                            for (name, col) in TAB_COLORS {
                                if ui.button(*name).clicked() {
                                    want_color = Some((i, Some(*col)));
                                    ui.close();
                                }
                            }
                        });
                        ui.separator();
                        if ui.button("Close Tab").clicked() {
                            want_close = Some(i);
                            ui.close();
                        }
                        if ui
                            .add_enabled(ntabs > 1, egui::Button::new("Close Other Tabs"))
                            .clicked()
                        {
                            want_close_others = Some(i);
                            ui.close();
                        }
                        if ui
                            .add_enabled(
                                i + 1 < ntabs,
                                egui::Button::new("Close Tabs to the Right"),
                            )
                            .clicked()
                        {
                            want_close_right = Some(i);
                            ui.close();
                        }
                    });
                }
            }
            // Profile picker: open a tab running a chosen shell (also the primary
            // new-tab affordance now that the standalone `+` is gone).
            ui.menu_button("⏷", |ui| {
                if ui.button("New Tab").clicked() {
                    want_new = Some(default_profile);
                    ui.close();
                }
                ui.separator();
                for (pi, name) in profile_names.iter().enumerate() {
                    if ui.button(name).clicked() {
                        want_new = Some(pi);
                        ui.close();
                    }
                }
            })
            .response
            .on_hover_text("New tab (pick a shell)");
        });

        // Resolve an in-progress tab drag. The tabs stay put while dragging; an
        // insertion caret shows where the drop would land.
        if let Some(from) = drag_from {
            self.tab_drag = Some(from);
        }
        if let Some(from) = self.tab_drag {
            let pointer = ui.ctx().input(|i| i.pointer.clone());
            let held = pointer.any_down();
            if let (Some(x), Some(&r0)) = (pointer.latest_pos().map(|p| p.x), tab_rects.first()) {
                let to = drop_index(&tab_rects, x);
                if held {
                    ui.ctx().set_cursor_icon(egui::CursorIcon::Grabbing);
                    // Caret at the drop slot: the left edge of the tab we'd land
                    // before, or the right edge of the strip when dropping last.
                    let cx = match tab_rects.get(to) {
                        Some(r) => r.left(),
                        None => tab_rects.last().map_or(r0.right(), |r| r.right()),
                    };
                    ui.painter().rect_filled(
                        egui::Rect::from_min_max(
                            egui::pos2(cx - 1.5, r0.top() - 2.0),
                            egui::pos2(cx + 1.5, r0.bottom() + 2.0),
                        ),
                        1.0,
                        egui::Color32::from_rgb(0x5a, 0x82, 0xc8),
                    );
                } else {
                    want_move = Some((from, to));
                }
            }
            // Clear the latch once no button is held — covers a drop outside the
            // strip and an Escape-aborted drag as well as a normal release.
            if !held {
                self.tab_drag = None;
            }
        }

        // Apply collected intents. Index-stable edits first; tab-removing actions
        // last so earlier intents still refer to valid indices.
        if stop_rename {
            renaming = None;
        }
        self.renaming = renaming;
        if let Some(i) = switch_to {
            self.active_tab = i;
        }
        if let Some((i, col)) = want_color {
            if let Some(t) = self.tabs.get_mut(i) {
                t.color = col;
            }
        }
        if let Some((i, val)) = commit_rename {
            if let Some(t) = self.tabs.get_mut(i) {
                t.name = val;
            }
        }
        if let Some(i) = want_rename {
            let cur = self
                .tabs
                .get(i)
                .and_then(|t| t.name.clone())
                .unwrap_or_default();
            self.renaming = Some((i, cur));
        }
        if let Some((from, to)) = want_move {
            self.move_tab(from, to);
        }
        if let Some(idx) = want_new {
            self.new_tab(idx);
        }
        // Every close route (the × button, middle-click, and the context menu)
        // funnels through `request_close`, so the confirmation gate is applied
        // once here rather than at each of them.
        if let Some(i) = want_close_others {
            self.request_close(PendingClose::OtherTabs(i));
        }
        if let Some(i) = want_close_right {
            self.request_close(PendingClose::TabsToRight(i));
        }
        if let Some(i) = want_close {
            self.request_close(PendingClose::Tab(i));
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
        let right_click_action = self.config.right_click_action;
        let middle_click_action = self.config.middle_click_action;
        let sel_bg = self.config.selection_bg;
        let sel_fg = self.config.selection_fg;
        // Opacities are read fresh each frame (like the selection colors), so a
        // config reload applies on the next frame with no explicit re-apply.
        let faint_opacity = self.config.faint_opacity;
        let cursor_opacity = self.config.cursor_opacity;
        let background_opacity = self.config.background_opacity;
        let background_opacity_cells = self.config.background_opacity_cells;
        let unfocused_split_opacity = self.config.unfocused_split_opacity;
        let unfocused_split_fill = self.config.unfocused_split_fill;
        let resize_overlay_position = self.config.resize_overlay_position;
        // Snapshot the keymap so the focused pane's `handle_input` can consult it
        // without holding a borrow on `self` across the pane-tree mutation below.
        // Cheap: a couple dozen (Chord, Action) entries, both `Copy`.
        let keymap = self.keymap.clone();
        let bell_border = self.config.bell.border;
        let now = ctx.input(|i| i.time);
        let active_tab = self.active_tab;
        // Hoisted so the per-pane widget id can be namespaced to this window
        // from inside the leaf loop, which borrows `self.tabs`.
        let win_id = self.window_id;
        // While a modal overlay is up it owns input: don't feed keys to the
        // focused pane or let it grab keyboard focus. `egui::Modal` blocks
        // *pointer* interaction and tab traversal on its own, but
        // `Ui::request_focus` is unconditional — so the pane's per-frame focus
        // grab has to be suppressed explicitly, or typing would still reach the
        // shell behind the confirmation dialog.
        let palette_open = self.palette.is_some() || self.confirm.is_some();

        // Fill the whole area (including the per-pane padding band and the split
        // gutters) with the focused pane's background. This single rect is what
        // carries `background-opacity`: cells left on the default background emit
        // no quad at all (see `render::bg_alpha`), so this is the layer that shows
        // through them. Nothing else may paint a translucent copy of it — two
        // stacked translucent fills would composite to 1-(1-a)² and read as much
        // darker than the configured opacity.
        //
        // Painted here, above the `leaves.is_empty()` early return below, so a tab
        // caught mid-teardown doesn't flash an unpainted (fully transparent) hole.
        let bg = self.tabs[active_tab].focused_payload().default_bg();
        let bg_alpha8 = (self.config.background_opacity * 255.0).round() as u8;
        ui.painter().rect_filled(
            full_area,
            0.0,
            egui::Color32::from_rgba_unmultiplied(bg.r, bg.g, bg.b, bg_alpha8),
        );

        let tab = &mut self.tabs[active_tab];
        let mut focus_id = tab.focus;
        // Right-click "Split" needs `&mut self`, which we can't take while
        // `leaves`/`tab` borrow `self.tabs`; defer it past the leaf loop.
        let mut want_split: Option<bool> = None;

        // A zoom on a pane that no longer exists (closed/reaped) is stale; drop it.
        tab.zoomed = tab.zoomed.filter(|id| tab.root.contains(*id));
        let zoomed = tab.zoomed;
        // Lay the split tree out across the full area; each leaf gets its rect
        // (padding is applied per-leaf below). When a split is zoomed, that one
        // leaf takes the whole area and the rest are hidden.
        let mut leaves: Vec<Leaf<Session>> = Vec::new();
        match zoomed {
            Some(id) => tab.root.collect_leaf(id, full_area, &mut leaves),
            None => tab.root.collect(full_area, &mut leaves),
        }
        if leaves.is_empty() {
            return;
        }
        // Cache this frame's layout so the next frame's `handle_shortcuts` (which
        // runs before layout) can resolve directional split-focus navigation.
        self.last_layout = leaves.iter().map(|l| (l.id, l.rect)).collect();

        // Focus-follows-click: a press inside a pane focuses it.
        let press_pos = ctx.input(|i| {
            i.events.iter().find_map(|e| match e {
                egui::Event::PointerButton {
                    pos, pressed: true, ..
                } => Some(*pos),
                _ => None,
            })
        });
        // Resolve the current focus first so we can tell whether its search
        // overlay is open. The overlay is modal and window-anchored, so in a
        // split it floats over a *different* pane — a click on it must NOT move
        // focus there (that would vanish the overlay mid-use and orphan the
        // searching pane's highlights). So suppress focus-follows-click while the
        // focused pane's search is open.
        if !leaves.iter().any(|l| l.id == focus_id) {
            focus_id = leaves[0].id;
        }
        let cur_idx = leaves.iter().position(|l| l.id == focus_id).unwrap();
        let search_open = leaves[cur_idx].payload.search_active();
        // Suppress the click-focus while search is modal (`None` = don't move).
        let click_pos = if search_open { None } else { press_pos };
        if let Some(pos) = click_pos {
            if let Some(l) = leaves.iter().find(|l| l.rect.contains(pos)) {
                focus_id = l.id;
            }
        }
        let focus_idx = leaves.iter().position(|l| l.id == focus_id).unwrap();
        // The search overlay is modal for the keyboard: while it's open the
        // search box owns input, so the pane doesn't take key/text input or grab
        // egui focus (see `render_search`).

        // Keyboard goes to the focused pane — unless the command palette is open
        // (it's modal and owns the keyboard; see `render_palette`). `tracking` is
        // also used by the mouse block below, so compute it regardless.
        let tracking = leaves[focus_idx].payload.is_mouse_tracking();
        if !palette_open && !search_open {
            leaves[focus_idx]
                .payload
                .handle_input(ctx, tracking, ch, &keymap);
        } else if search_open {
            // The search overlay owns the keyboard, so input (and its scroll
            // easing) is skipped — but keep the viewport easing toward the match
            // that `scroll_to_match` targeted.
            leaves[focus_idx].payload.tick_scroll(ctx, ch);
        }

        let mut frames: Vec<PaneFrame> = Vec::with_capacity(leaves.len());
        // (pane rect, flash intensity) for any pane ringing its visual bell.
        let mut bell_flashes: Vec<(egui::Rect, f32)> = Vec::new();
        // (pane rect, fill color) for each unfocused split to dim. Collected here
        // (where each leaf's own background is in scope) and painted over the
        // terminal after the callback below.
        let dimming = leaves.len() > 1 && unfocused_split_opacity < 1.0;
        let mut dim_rects: Vec<(egui::Rect, egui::Color32)> = Vec::new();
        // (pane rect, "COLS x ROWS", fade alpha) for any pane showing the
        // grid-size overlay after a resize.
        let mut resize_overlays: Vec<(egui::Rect, String, f32)> = Vec::new();
        for leaf in leaves.iter_mut() {
            // The pane occupies `leaf.rect`; the grid is inset by the padding so
            // text clears the pane's edges (window border or split divider alike).
            let prect = leaf.rect.shrink2(pad);
            let leaf_rect = leaf.rect;
            let leaf_id = leaf.id;
            let is_focus = leaf_id == focus_id;
            let session = &mut *leaf.payload;
            // Visual bell: advance/drain this pane's flash every frame (so a BEL
            // isn't lost even if its snapshot transiently fails below).
            let flash = session.bell_flash_alpha(now);
            if bell_border {
                if let Some(a) = flash {
                    bell_flashes.push((leaf_rect, a));
                }
            }
            // Dim every split except the focused one. Deliberately keyed on
            // `is_focus`, *not* on window focus: Ghostty leaves the last-focused
            // surface undimmed when the window itself is inactive, so an
            // unfocused giest window must not dim every one of its panes.
            if dimming && !is_focus {
                let fill = unfocused_split_fill.unwrap_or_else(|| session.default_bg());
                dim_rects.push((
                    leaf_rect,
                    egui::Color32::from_rgba_unmultiplied(
                        fill.r,
                        fill.g,
                        fill.b,
                        dim_alpha(unfocused_split_opacity),
                    ),
                ));
            }
            session.fit_grid(prect, ppp, cw, ch, now);
            if let Some(a) = session.resize_overlay_alpha(now) {
                let (cols, rows) = session.grid_size();
                // Label format matches Ghostty's overlay exactly.
                resize_overlays.push((leaf_rect, format!("{cols} x {rows}"), a));
            }
            if !session.update_snapshot() {
                continue;
            }

            // Mouse / selection interaction only for the focused pane, and not
            // while a modal overlay (palette or search) owns input/focus.
            if is_focus && !palette_open && !search_open {
                // Hold keyboard focus on the terminal and lock the navigation keys
                // to it. Otherwise egui's built-in focus traversal swallows Tab
                // (which the shell wants for completion) to cycle focus through the
                // tab-strip buttons, and a following Enter/Space fires whichever
                // button got focus — switching/closing tabs unexpectedly.
                let resp = ui.interact(
                    prect,
                    egui::Id::new(("giest-window", win_id, "pane", active_tab, leaf_id)),
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

                    // Right-click: context menu (default) or a direct action.
                    // Only reached when the app isn't capturing the mouse (the
                    // `tracking` branch above), matching Ghostty's suppression.
                    let copy_sel = |s: &Session| {
                        if let Some(text) = s.selection_text() {
                            ctx.copy_text(text);
                        }
                    };
                    let paste = |s: &mut Session| {
                        if let Some(text) = session::read_clipboard() {
                            s.paste_str(&text);
                        }
                    };
                    match right_click_action {
                        RightClickAction::ContextMenu => {
                            let has_sel = session.selection_range().is_some();
                            resp.context_menu(|ui| {
                                if ui.add_enabled(has_sel, egui::Button::new("Copy")).clicked() {
                                    copy_sel(session);
                                    ui.close();
                                }
                                if ui.button("Paste").clicked() {
                                    paste(session);
                                    ui.close();
                                }
                                ui.separator();
                                if ui.button("Split Right").clicked() {
                                    want_split = Some(true);
                                    ui.close();
                                }
                                if ui.button("Split Down").clicked() {
                                    want_split = Some(false);
                                    ui.close();
                                }
                                ui.separator();
                                if ui.button("Select All").clicked() {
                                    session.select_all();
                                    ui.close();
                                }
                                if ui.button("Reset Terminal").clicked() {
                                    session.reset();
                                    ui.close();
                                }
                            });
                        }
                        action if resp.clicked_by(egui::PointerButton::Secondary) => match action {
                            RightClickAction::Copy => copy_sel(session),
                            RightClickAction::Paste => paste(session),
                            RightClickAction::CopyOrPaste => {
                                if session.selection_range().is_some() {
                                    copy_sel(session);
                                } else {
                                    paste(session);
                                }
                            }
                            RightClickAction::Ignore | RightClickAction::ContextMenu => {}
                        },
                        _ => {}
                    }

                    // Middle-click pastes the clipboard (no PRIMARY on Windows).
                    if middle_click_action == MiddleClickAction::PrimaryPaste
                        && resp.clicked_by(egui::PointerButton::Middle)
                    {
                        paste(session);
                    }
                }
            }

            // Cheap Rc bump (not a grid clone); the blink toggle rides on a
            // separate flag so the snapshot stays shared and unmutated.
            let snapshot = session.snapshot.clone();
            // Only the focused pane of a focused window gets a live (solid,
            // blinking) cursor; every other visible cursor is drawn hollow.
            let pane_active = is_focus && window_focused;
            let cursor_blink_hidden =
                pane_active && snapshot.cursor_blinking && ctx.input(|i| i.time) % 1.0 >= 0.5;
            if pane_active && snapshot.cursor_blinking {
                ctx.request_repaint_after(Duration::from_millis(100));
            }
            // Text blink (SGR 5) rides the same ~1 Hz phase but applies to every
            // pane; only keep repainting while the grid actually has blink cells.
            let blink_hidden = ctx.input(|i| i.time) % 1.0 >= 0.5;
            if snapshot.has_blink {
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
                cursor_blink_hidden,
                blink_hidden,
                scroll_offset_px: session.scroll_offset_px(),
                search_highlights: session.search_highlights(),
            });
        }

        // One callback paints every pane (shared instance buffer).
        ui.painter().add(egui_wgpu::Callback::new_paint_callback(
            full_area,
            TermFrame {
                panes: frames,
                selection_bg: sel_bg,
                selection_fg: sel_fg,
                background_opacity,
                background_opacity_cells,
                faint_opacity,
                cursor_opacity,
            },
        ));

        // Dim the unfocused splits by painting a semi-transparent rectangle over
        // each — the same mechanism Ghostty uses (an overlay, not a renderer
        // effect). Painter-only, so it never intercepts clicks.
        for (rect, col) in &dim_rects {
            ui.painter().rect_filled(*rect, 0.0, *col);
        }

        // Outline the focused pane when the tab is split. Frame the *full* pane
        // rect (not the padded grid) so the padding band shows as a visible gap
        // between the border and the text, rather than the text hugging the line.
        if leaves.len() > 1 {
            ui.painter().rect_stroke(
                leaves[focus_idx].rect,
                0.0,
                egui::Stroke::new(2.0_f32, egui::Color32::from_rgb(90, 130, 200)),
                egui::StrokeKind::Inside,
            );
        } else if zoomed.is_some() {
            // A zoomed split hides its siblings, so a "split tab" can look like a
            // single pane. A green accent border signals the zoom is active
            // (toggle off with the same chord). Matches Ghostty's zoom indicator.
            ui.painter().rect_stroke(
                full_area,
                0.0,
                egui::Stroke::new(2.0_f32, egui::Color32::from_rgb(80, 200, 120)),
                egui::StrokeKind::Inside,
            );
        }

        // Visual bell: a warm border that fades over ~0.2s on each pane that rang.
        // Drawn over the terminal (after the pane callback) and kept animating by
        // requesting repaints while any flash is active.
        if !bell_flashes.is_empty() {
            ctx.request_repaint();
            for (rect, a) in &bell_flashes {
                let alpha = (a * 220.0) as u8;
                let col = egui::Color32::from_rgba_unmultiplied(0xff, 0xc6, 0x6b, alpha);
                ui.painter().rect_stroke(
                    *rect,
                    0.0,
                    egui::Stroke::new(3.0_f32, col),
                    egui::StrokeKind::Inside,
                );
            }
        }

        // Grid-size overlay after a resize. Drawn last so it reads as a HUD above
        // the dim rects and the focus border; painter-only, so it never
        // intercepts clicks. Must keep requesting repaints or the fade freezes on
        // an idle terminal.
        if !resize_overlays.is_empty() {
            ctx.request_repaint();
            for (rect, label, a) in &resize_overlays {
                let (anchor, align) = overlay_anchor(*rect, resize_overlay_position, 12.0);
                let font = egui::FontId::proportional(14.0);
                let galley = ui.painter().layout_no_wrap(
                    label.clone(),
                    font,
                    egui::Color32::from_rgba_unmultiplied(0xf0, 0xf0, 0xf0, (a * 255.0) as u8),
                );
                let text_rect = align.anchor_size(anchor, galley.size());
                let pill = text_rect.expand2(egui::vec2(10.0, 6.0));
                ui.painter().rect_filled(
                    pill,
                    6.0,
                    egui::Color32::from_rgba_unmultiplied(0x20, 0x22, 0x28, (a * 230.0) as u8),
                );
                ui.painter().rect_stroke(
                    pill,
                    6.0,
                    egui::Stroke::new(
                        1.0_f32,
                        egui::Color32::from_rgba_unmultiplied(0x60, 0x66, 0x78, (a * 220.0) as u8),
                    ),
                    egui::StrokeKind::Inside,
                );
                ui.painter().galley(text_rect.min, galley, egui::Color32::WHITE);
            }
        }

        // Commit the (possibly click-updated) focus back to the tab. Done last,
        // after the final use of `leaves` (which borrows `tab.root`).
        self.tabs[active_tab].focus = focus_id;

        // Apply a deferred right-click "Split" now that `leaves`/`tab` are no
        // longer borrowing `self.tabs`. `split` inherits the pane's cwd and
        // focuses the new pane.
        if let Some(vertical) = want_split {
            self.split(vertical);
        }
    }
}

impl Window {
    /// Drain this window's PTYs and latch any bell. Runs for **every** window
    /// from the root pass, not inside `run_pass`, so a background window's
    /// shells keep flowing and its shell-exit is still noticed while it isn't
    /// the one being drawn.
    ///
    /// The bell *effects* are only latched here: firing them needs this window's
    /// own focus state, which is only meaningful inside its own pass.
    fn pump_all(&mut self, now: f64) {
        let mut rang = false;
        for tab in &mut self.tabs {
            tab.root.for_each_mut(&mut |pane| {
                pane.pump_pty();
                rang |= pane.take_bell_effect(now);
            });
        }
        self.pending_bell |= rang;
    }

    /// Run one UI pass for this window, into whichever viewport is current.
    /// Returns the app-scoped intents it raised.
    ///
    /// Every viewport-scoped call below (`send_viewport_cmd`, `i.focused`,
    /// `i.viewport()`, `ctx.content_rect()`) resolves against the *running*
    /// pass, so this body is correct for the root window and for a child
    /// viewport without a single branch on which one it is.
    fn run_pass(
        &mut self,
        ui: &mut egui::Ui,
        render_state: Option<&egui_wgpu::RenderState>,
    ) -> Vec<AppRequest> {
        let ctx = ui.ctx().clone();

        // Record where this window is, for the root-slot rehost on retire.
        self.geom = ctx.input(|i| {
            let vp = i.viewport();
            Some((vp.outer_rect?.min, vp.inner_rect?.size()))
        });

        // Close panes/tabs whose shell exited; bail if that closed the window.
        // NOTE: this path is deliberately never confirmed — the process is
        // already gone, so there is nothing left to save.
        if !self.reap_dead() {
            return std::mem::take(&mut self.requests);
        }
        if std::mem::take(&mut self.pending_bell) {
            self.ring_bell(&ctx);
        }

        // The titlebar ×, Alt+F4 or the taskbar. eframe reads `close_requested`
        // from *this pass's* raw input and exits after the pass unless
        // `CancelClose` is sent within the same pass — so this must run every
        // frame, before anything can early-return.
        //
        // `CancelClose` is honoured for the **root** viewport only; a child's
        // close request is inert until the app stops rendering it, so a child
        // needs no cancel at all. Hence the `is_root()` guards below.
        if ctx.input(|i| i.viewport().close_requested()) && !self.closing {
            if self.confirm.is_none()
                && crate::config::needs_confirm(
                    self.config.confirm_close,
                    self.close_targets(PendingClose::Window)
                        .into_iter()
                        .reduce(|a, b| match (a, b) {
                            (Some(true), _) | (_, Some(true)) => Some(true),
                            (None, _) | (_, None) => None,
                            _ => Some(false),
                        })
                        .flatten(),
                )
            {
                if self.is_root() {
                    ctx.send_viewport_cmd(egui::ViewportCommand::CancelClose);
                }
                self.confirm = Some(PendingClose::Window);
            } else if self.confirm.is_some() {
                // A dialog is already up; don't let a second close request race it.
                if self.is_root() {
                    ctx.send_viewport_cmd(egui::ViewportCommand::CancelClose);
                }
            } else {
                // No confirmation wanted: close now. For the root this is the
                // path eframe would have taken anyway; for a child it's what
                // actually retires the window.
                self.requests.push(AppRequest::CloseWindow);
            }
        }

        // App shortcuts and font zoom are suppressed while a modal overlay (the
        // command palette, the scrollback-search bar or the close confirmation)
        // is open; each overlay handles its own keys, including its toggle-close.
        if !self.modal_open() {
            self.handle_shortcuts(&ctx);
            if let Some(render_state) = render_state {
                self.handle_font_zoom(&ctx, render_state);
            }
        }

        // A bell's 🔔 marker clears once the window is focused again — Ghostty
        // holds it "until the terminal is re-focused". Also stop the taskbar
        // flash ourselves rather than relying on FLASHW_TIMERNOFG, so the two
        // effects clear together.
        let focused = ctx.input(|i| i.focused);
        if focused && !self.was_focused {
            self.bell_title = false;
            if let Some(hwnd) = self.hwnd {
                crate::bell::clear_attention(hwnd);
            }
        }
        self.was_focused = focused;

        // Window title from the active tab's focused pane, with the bell marker.
        // `last_window_title` holds the **decorated** string: comparing against
        // the undecorated one would make clearing the 🔔 look like "no change"
        // and never re-send the title.
        let shown = {
            let base = self
                .tabs
                .get(self.active_tab)
                .and_then(|t| t.focused_payload().title())
                .unwrap_or_else(|| "giest".to_string());
            if self.bell_title { format!("🔔 {base}") } else { base }
        };
        if Some(&shown) != self.last_window_title.as_ref() {
            ctx.send_viewport_cmd(egui::ViewportCommand::Title(shown.clone()));
            self.last_window_title = Some(shown);
        }

        // Always show the tab strip so the new-tab profile picker (PowerShell /
        // cmd / WSL / …) is reachable even with a single tab.
        // Its fill keeps the theme's panel color but takes `background-opacity`,
        // so the strip is as translucent as the terminal below it. It occupies a
        // rect disjoint from the central panel, so this does not stack with the
        // window-background fill.
        let strip = ctx.global_style().visuals.panel_fill;
        let a8 = (self.config.background_opacity * 255.0).round() as u8;
        egui::Panel::top(self.id("tabs"))
            .frame(
                egui::Frame::side_top_panel(&ctx.global_style()).fill(
                    egui::Color32::from_rgba_unmultiplied(strip.r(), strip.g(), strip.b(), a8),
                ),
            )
            .show_inside(ui, |ui| self.tab_bar(ui));

        // No fill here: `render_active` paints the window background across this
        // whole area itself. Filling it here too would double-composite the
        // translucent color (1-(1-a)²) and read far darker than configured.
        egui::CentralPanel::default()
            .frame(egui::Frame::NONE)
            .show_inside(ui, |ui| self.render_active(ui, &ctx));

        // The command palette draws over everything; a chosen command runs after
        // the modal closes, so it mutates `self` with no outstanding borrow.
        if let Some(action) = self.render_palette(&ctx) {
            self.execute_action(&ctx, render_state, action);
        }
        // The scrollback-search overlay (self-gating: a no-op unless the focused
        // pane's search is open).
        self.render_search(&ctx);
        // The close confirmation draws over everything else.
        self.render_confirm_close(&ctx);

        std::mem::take(&mut self.requests)
    }
}

impl App {
    pub fn new(cc: &eframe::CreationContext<'_>) -> Result<Self> {
        Ok(Self {
            windows: vec![Window::first(cc, 0)?],
            focused: 0,
            next_window_id: 1,
        })
    }

    /// Apply the app-scoped intents raised by this pass's windows. Runs after
    /// every window pass, so nothing is borrowed and the window list is free to
    /// grow or shrink.
    ///
    /// Requests are keyed by [`Window::window_id`], not by slot: retiring one
    /// window renumbers the rest, so an index captured during the pass would
    /// point at the wrong window by the time we got here.
    fn apply_requests(&mut self, ctx: &egui::Context, requests: Vec<(u64, AppRequest)>) {
        for (id, req) in requests {
            match req {
                AppRequest::CloseWindow => self.retire(ctx, id),
                AppRequest::NewWindow(cwd) => self.spawn_window(ctx, cwd.as_deref()),
            }
        }
    }

    /// Open a new window, cloned from the focused one (falling back to the root)
    /// so it inherits the live config, profiles and font metrics.
    fn spawn_window(&mut self, ctx: &egui::Context, cwd: Option<&std::path::Path>) {
        let from = self.focused.min(self.windows.len().saturating_sub(1));
        let Some(src) = self.windows.get(from) else {
            return;
        };
        let id = self.next_window_id;
        // Only bump the counter on success, so a failed spawn doesn't burn an id.
        if let Some(w) = src.sibling(id, cwd) {
            self.next_window_id += 1;
            self.windows.push(w);
            ctx.request_repaint();
        }
    }

    /// Close the window with `id`, quitting giest when it was the last one.
    fn retire(&mut self, ctx: &egui::Context, id: u64) {
        let Some(idx) = self.windows.iter().position(|w| w.window_id == id) else {
            return;
        };
        // Remember where the surviving root-slot window is on screen *before*
        // the move, so a rehost can put the root native window there.
        let rehost_to = (idx == 0).then(|| self.windows.get(1).and_then(|w| w.geom)).flatten();

        let windows = std::mem::take(&mut self.windows);
        let (windows, focused) = retire_window(windows, idx, self.focused);
        self.windows = windows;
        self.focused = focused;

        if self.windows.is_empty() {
            // The last window went: closing the root viewport ends the process.
            ctx.send_viewport_cmd_to(egui::ViewportId::ROOT, egui::ViewportCommand::Close);
            return;
        }
        // Slot 0 is by definition the root viewport. If the old root was the one
        // retired, a survivor has just slid into that slot — move the native root
        // window onto the geometry that survivor used to occupy, so on screen the
        // window the user actually closed is the one that disappears.
        if idx == 0 {
            self.windows[0].is_root = true;
            if let Some(g) = rehost_to {
                ctx.send_viewport_cmd_to(
                    egui::ViewportId::ROOT,
                    egui::ViewportCommand::OuterPosition(g.0),
                );
                ctx.send_viewport_cmd_to(
                    egui::ViewportId::ROOT,
                    egui::ViewportCommand::InnerSize(g.1),
                );
            }
        }
        ctx.request_repaint();
    }
}

impl eframe::App for App {
    /// Clear the framebuffer to *fully transparent*.
    ///
    /// This is a precondition for `background-opacity`, not a nicety: eframe's
    /// default clear is `rgba_unmultiplied(12, 12, 12, 180)`, and egui's own alpha
    /// blend (`src * OneMinusDstAlpha + dst * One`) can only ever *raise* the
    /// framebuffer alpha. Starting at alpha 180/255 therefore caps the window at
    /// ~71% opacity no matter what we paint, and tints everything toward (12,12,12).
    /// It is also what lets the DWM acrylic backdrop show through (`crate::blur`).
    ///
    /// Harmless when opaque: the window-background fill covers every pixel.
    ///
    /// NOTE: eframe consults this for the **root** viewport only — it hardcodes
    /// `[0,0,0,0]` for immediate child viewports. Same value, so no divergence
    /// today; a future non-zero clear would silently not reach other windows.
    fn clear_color(&self, _visuals: &egui::Visuals) -> [f32; 4] {
        [0.0, 0.0, 0.0, 0.0]
    }

    fn ui(&mut self, ui: &mut egui::Ui, frame: &mut eframe::Frame) {
        let ctx = ui.ctx().clone();
        // One `RenderState` serves every viewport (egui-wgpu keeps a single
        // painter), so the root's is valid for all windows.
        let render_state = frame.wgpu_render_state().cloned();

        // Poll for shell exit even when idle (a shell that exits produces no
        // output, so nothing else would wake us to reap it). Issued on the root
        // only: children repaint via the root anyway, so a per-window timer would
        // just multiply into N root repaints.
        ctx.request_repaint_after(Duration::from_millis(500));

        // Pump every window's PTYs before drawing any of them, so background
        // windows keep flowing and their shell exits are noticed.
        let now = ctx.input(|i| i.time);
        for w in &mut self.windows {
            w.pump_all(now);
        }

        let mut requests: Vec<(u64, AppRequest)> = Vec::new();
        // The root window draws into the `Ui` eframe handed us.
        if let Some(w) = self.windows.first_mut() {
            let id = w.window_id;
            requests.extend(
                w.run_pass(ui, render_state.as_ref())
                    .into_iter()
                    .map(|r| (id, r)),
            );
        }

        // Every other window gets its own native window, via an *immediate*
        // viewport. Deferred viewports would repaint independently (cheaper),
        // but their callback must be `Send + Sync + 'static` and `Session` is
        // neither — it holds `Rc`s and an FFI terminal. Immediate viewports take
        // a plain `FnMut`, so the closure can borrow the window directly.
        for i in 1..self.windows.len() {
            // Split borrows: `windows` and the rest of `self` are disjoint
            // fields, so the closure can hold `&mut Window` while we read the
            // render state. Nothing here touches `self.windows` as a whole.
            let App { windows, .. } = self;
            let w = &mut windows[i];
            let (id, vp, builder) = (w.window_id, w.viewport_id(), w.child_builder());
            let out = ctx.show_viewport_immediate(vp, builder, |cui, _class| {
                w.run_pass(cui, render_state.as_ref())
            });
            requests.extend(out.into_iter().map(|r| (id, r)));
        }

        // Track which window has focus, for `new_window`'s cwd inheritance.
        // Recomputed every pass — a retire renumbers the slots.
        if let Some(i) = self
            .windows
            .iter()
            .position(|w| w.was_focused)
        {
            self.focused = i;
        }

        self.apply_requests(&ctx, requests);
    }
}

/// Drop window `idx`, returning the surviving list and the remapped focused
/// index so the *same* window stays focused where possible.
///
/// Split out from [`App`] — like `reap_tabs` and `reorder_tabs` — so the
/// reselection logic is testable without spawning a shell. Out-of-range `idx` is
/// a no-op rather than a panic, matching the other window/tab helpers.
fn retire_window<W>(mut windows: Vec<W>, idx: usize, focused: usize) -> (Vec<W>, usize) {
    if idx >= windows.len() {
        return (windows, focused);
    }
    windows.remove(idx);
    if windows.is_empty() {
        return (windows, 0);
    }
    // A focus past the removed slot shifts down; a focus *on* it lands on the
    // window that took its place (clamped at the end).
    let focused = if focused > idx { focused - 1 } else { focused };
    let last = windows.len() - 1;
    (windows, focused.min(last))
}

/// Which slot a tab dragged to pointer-x `x` should land in, given this frame's
/// tab rects: the number of tabs whose horizontal **centre** is left of `x`.
///
/// Centre-crossing (rather than edge-crossing) is what makes the swap happen at
/// the halfway point, which is what a drag reorder is expected to feel like.
/// Returns an insertion index in `0..=rects.len()`.
fn drop_index(rects: &[egui::Rect], x: f32) -> usize {
    rects.iter().filter(|r| r.center().x < x).count()
}

/// Move `tabs[from]` to insertion index `to`, returning the reordered list and
/// the remapped active index so the *same* tab stays selected.
///
/// `to` is an index into the **pre-removal** list (what [`drop_index`] computes),
/// so the real insert point shifts left by one when moving rightwards. Split out
/// from `App` — like `reap_tabs` and `keep_only_tab` — so the reselection logic
/// is testable without a shell.
fn reorder_tabs<T>(
    mut tabs: Vec<Tab<T>>,
    from: usize,
    to: usize,
    active: usize,
) -> (Vec<Tab<T>>, usize) {
    if from >= tabs.len() || to > tabs.len() {
        return (tabs, active);
    }
    let insert = to - usize::from(to > from);
    if insert == from {
        return (tabs, active);
    }
    // Track the active tab by identity across the move.
    let active_is_moving = active == from;
    let tab = tabs.remove(from);
    // After removal, an active index past `from` shifts down by one.
    let mut act = if active > from { active - 1 } else { active };
    tabs.insert(insert, tab);
    if active_is_moving {
        act = insert;
    } else if insert <= act {
        // The insert landed at or before it, pushing it back up.
        act += 1;
    }
    let last = tabs.len().saturating_sub(1);
    (tabs, act.min(last))
}

/// Anchor point and alignment for the resize overlay inside `pane`.
///
/// Returns an `Align2` rather than a final rect so the caller can let egui
/// measure the text — which keeps this independent of font metrics and therefore
/// testable on its own.
fn overlay_anchor(
    pane: egui::Rect,
    pos: ResizeOverlayPosition,
    margin: f32,
) -> (egui::Pos2, egui::Align2) {
    use ResizeOverlayPosition as P;
    use egui::Align2;
    let m = margin;
    match pos {
        P::Center => (pane.center(), Align2::CENTER_CENTER),
        P::TopLeft => (pane.left_top() + egui::vec2(m, m), Align2::LEFT_TOP),
        P::TopCenter => (
            egui::pos2(pane.center().x, pane.top() + m),
            Align2::CENTER_TOP,
        ),
        P::TopRight => (pane.right_top() + egui::vec2(-m, m), Align2::RIGHT_TOP),
        P::BottomLeft => (pane.left_bottom() + egui::vec2(m, -m), Align2::LEFT_BOTTOM),
        P::BottomCenter => (
            egui::pos2(pane.center().x, pane.bottom() - m),
            Align2::CENTER_BOTTOM,
        ),
        P::BottomRight => (
            pane.right_bottom() + egui::vec2(-m, -m),
            Align2::RIGHT_BOTTOM,
        ),
    }
}

/// Alpha of the rectangle painted over an unfocused split to dim it, from the
/// configured `unfocused-split-opacity`.
///
/// Note the inversion — the config names the split's *remaining* opacity, so the
/// overlay covering it takes the complement. Ghostty's GTK apprt writes exactly
/// this (`opacity: 1.0 - unfocused-split-opacity` in its generated CSS).
fn dim_alpha(split_opacity: f32) -> u8 {
    ((1.0 - split_opacity.clamp(0.0, 1.0)) * 255.0).round() as u8
}

/// Open a URL in the user's default handler (Windows). `explorer` routes
/// http/https/etc. to the default browser without flashing a console window.
fn open_url(url: &str) {
    let _ = std::process::Command::new("explorer").arg(url).spawn();
}

/// Append giest's embedded JetBrains Mono Nerd Font as the last fallback for both
/// egui UI font families. egui's bundled fonts (Ubuntu-Light / Hack) lack many
/// symbols — arrows, `×`, box/powerline glyphs — so chrome like the search bar's
/// prev/next/close buttons would otherwise render as tofu boxes. As a *fallback*
/// it only supplies glyphs the primary UI fonts are missing.
fn install_ui_fallback_font(ctx: &egui::Context) {
    use std::sync::Arc;
    let mut fonts = egui::FontDefinitions::default();
    fonts.font_data.insert(
        "giest-nerd".to_owned(),
        Arc::new(egui::FontData::from_static(render::regular_font())),
    );
    for family in [egui::FontFamily::Proportional, egui::FontFamily::Monospace] {
        fonts
            .families
            .entry(family)
            .or_default()
            .push("giest-nerd".to_owned());
    }
    ctx.set_fonts(fonts);
}

/// Build the renderer's neutral font selection from config. Cloned at atlas
/// construction (startup); `font-family`/`font-feature` are not re-applied on
/// config reload (the atlas isn't rebuilt there).
fn font_spec(config: &Config) -> render::FontSpec {
    render::FontSpec {
        family: config.font_family.clone(),
        family_bold: config.font_family_bold.clone(),
        family_italic: config.font_family_italic.clone(),
        family_bold_italic: config.font_family_bold_italic.clone(),
        features: config.font_features.clone(),
    }
}

/// Reveal the config file in Explorer (palette "Open Config"). Falls back to the
/// containing folder when the file doesn't exist yet, so the user can create it.
fn open_config() {
    let Some(path) = crate::config::config_path() else {
        return;
    };
    let target = if path.exists() {
        path
    } else {
        path.parent().map(|p| p.to_path_buf()).unwrap_or(path)
    };
    let _ = std::process::Command::new("explorer").arg(target).spawn();
}

/// Direction for split-focus navigation (`Ctrl+Alt+arrow`).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Dir {
    Left,
    Right,
    Up,
    Down,
}

/// Pick the spatially adjacent leaf to `focus` in direction `dir`, given the
/// laid-out `(id, rect)` pairs (from the previous frame). A candidate qualifies
/// when its center is beyond the focused pane on the primary axis *and* its rect
/// overlaps the focused pane on the cross axis — exact adjacency for a binary
/// split tree. The nearest such pane wins, ties broken by the closer cross-axis
/// center. Returns `None` when there is no neighbor on that side.
fn nav_dir(layout: &[(u64, egui::Rect)], focus: u64, dir: Dir) -> Option<u64> {
    let f = layout.iter().find(|(id, _)| *id == focus)?.1;
    let fc = f.center();
    let mut best: Option<(u64, f32, f32)> = None; // (id, primary gap, cross dist)
    for &(id, r) in layout {
        if id == focus {
            continue;
        }
        let c = r.center();
        let (beyond, gap, overlap, cross) = match dir {
            Dir::Left => (
                c.x < fc.x,
                fc.x - c.x,
                r.min.y < f.max.y && r.max.y > f.min.y,
                (c.y - fc.y).abs(),
            ),
            Dir::Right => (
                c.x > fc.x,
                c.x - fc.x,
                r.min.y < f.max.y && r.max.y > f.min.y,
                (c.y - fc.y).abs(),
            ),
            Dir::Up => (
                c.y < fc.y,
                fc.y - c.y,
                r.min.x < f.max.x && r.max.x > f.min.x,
                (c.x - fc.x).abs(),
            ),
            Dir::Down => (
                c.y > fc.y,
                c.y - fc.y,
                r.min.x < f.max.x && r.max.x > f.min.x,
                (c.x - fc.x).abs(),
            ),
        };
        if !beyond || !overlap {
            continue;
        }
        let better = match best {
            None => true,
            Some((_, bg, bc)) => gap < bg || (gap == bg && cross < bc),
        };
        if better {
            best = Some((id, gap, cross));
        }
    }
    best.map(|(id, _, _)| id)
}

/// The next (`forward`) or previous id in `ids` after `focus`, wrapping around.
/// `ids` is assumed sorted (creation order); an absent `focus` starts from the
/// first. Returns `None` when there are fewer than two panes to cycle through.
fn cycle_pick(ids: &[u64], focus: u64, forward: bool) -> Option<u64> {
    if ids.len() < 2 {
        return None;
    }
    let cur = ids.iter().position(|&id| id == focus).unwrap_or(0);
    let n = ids.len();
    let next = if forward {
        (cur + 1) % n
    } else {
        (cur + n - 1) % n
    };
    Some(ids[next])
}

/// Split `area` into two halves along one axis with a 1px gutter between them.
/// `vertical` = a vertical divider, i.e. side-by-side columns (Ctrl+Shift+O);
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
    use super::{
        Dir, Node, Tab, cycle_pick, dim_alpha, drop_index, ellipsize, keep_only_tab, nav_dir,
        overlay_anchor, reap_tabs, reorder_tabs, retire_window, split_rect, truncate_tabs_to_right,
    };
    use crate::config::ResizeOverlayPosition as P;
    use eframe::egui;
    use eframe::egui::Align2;

    #[test]
    fn retire_window_drops_it_and_remaps_focus() {
        let w = || vec![10, 11, 12, 13];
        // Dropping a window before the focused one shifts focus down by one, so
        // the *same* window stays focused.
        let (v, f) = retire_window(w(), 0, 2);
        assert_eq!(v, vec![11, 12, 13]);
        assert_eq!(v[f], 12);
        // Dropping one after it leaves focus alone.
        let (v, f) = retire_window(w(), 3, 1);
        assert_eq!(v, vec![10, 11, 12]);
        assert_eq!(v[f], 11);
        // Dropping the focused window lands on whatever took its slot.
        let (v, f) = retire_window(w(), 1, 1);
        assert_eq!(v, vec![10, 12, 13]);
        assert_eq!(v[f], 12);
        // Dropping the focused *last* window clamps back onto the new last.
        let (v, f) = retire_window(w(), 3, 3);
        assert_eq!(v, vec![10, 11, 12]);
        assert_eq!(v[f], 12);
    }

    #[test]
    fn retire_window_on_the_last_one_empties_the_list() {
        // An empty list is how `App` knows to quit giest.
        let (v, f) = retire_window(vec![10], 0, 0);
        assert!(v.is_empty());
        assert_eq!(f, 0);
    }

    #[test]
    fn retire_window_ignores_out_of_range() {
        let (v, f) = retire_window(vec![10, 11], 9, 1);
        assert_eq!(v, vec![10, 11]);
        assert_eq!(f, 1);
    }

    #[test]
    fn inherit_cwd_follows_the_per_context_config_key() {
        use super::{NewSurface, should_inherit_cwd};
        use crate::config::Config;

        // All three default on.
        let d = Config::default();
        for what in [NewSurface::Window, NewSurface::Tab, NewSurface::Split] {
            assert!(should_inherit_cwd(what, &d), "{what:?} defaults on");
        }

        // Each context reads its *own* key — a swapped arm is exactly the bug
        // this table exists to prevent, so check every off-by-one pairing.
        let cases = [
            (NewSurface::Window, "window"),
            (NewSurface::Tab, "tab"),
            (NewSurface::Split, "split"),
        ];
        for (what, key) in cases {
            let cfg = Config::from_ghostty_config(&format!("{key}-inherit-working-directory = false"));
            assert!(!should_inherit_cwd(what, &cfg), "{what:?} reads {key}-*");
            for (other, other_key) in cases {
                if other != what {
                    assert!(
                        should_inherit_cwd(other, &cfg),
                        "{other:?} must not read {key}-* (it has {other_key}-*)"
                    );
                }
            }
        }
    }

    #[test]
    fn drop_index_uses_tab_centres() {
        // Three 100pt tabs: [0,100) [100,200) [200,300), centres at 50/150/250.
        let rects: Vec<egui::Rect> = (0..3)
            .map(|i| {
                egui::Rect::from_min_size(
                    egui::pos2(i as f32 * 100.0, 0.0),
                    egui::vec2(100.0, 20.0),
                )
            })
            .collect();
        assert_eq!(drop_index(&rects, 10.0), 0);
        // Still left of the first centre.
        assert_eq!(drop_index(&rects, 49.0), 0);
        // Past it — the swap happens at the halfway point, not the edge.
        assert_eq!(drop_index(&rects, 51.0), 1);
        assert_eq!(drop_index(&rects, 140.0), 1);
        assert_eq!(drop_index(&rects, 160.0), 2);
        assert_eq!(drop_index(&rects, 1000.0), 3);
        assert_eq!(drop_index(&rects, -50.0), 0);
    }

    /// Tabs carrying an identifying payload, for the reorder tests.
    fn tabs_named(n: usize) -> Vec<Tab<usize>> {
        (0..n).map(|i| Tab::leaf(i as u64 + 1, i)).collect()
    }

    fn order(tabs: &[Tab<usize>]) -> Vec<usize> {
        tabs.iter().map(|t| *t.focused_payload()).collect()
    }

    #[test]
    fn reorder_tabs_moves_and_keeps_the_same_tab_active() {
        // Drag the first tab to the far right; it stays selected.
        let (tabs, act) = reorder_tabs(tabs_named(4), 0, 4, 0);
        assert_eq!(order(&tabs), vec![1, 2, 3, 0]);
        assert_eq!(act, 3);

        // …and back again.
        let (tabs, act) = reorder_tabs(tabs_named(4), 3, 0, 3);
        assert_eq!(order(&tabs), vec![3, 0, 1, 2]);
        assert_eq!(act, 0);
    }

    #[test]
    fn reorder_tabs_shifts_an_unmoved_active_index() {
        // Moving tab 0 rightwards past the active tab pulls the active index down.
        let (tabs, act) = reorder_tabs(tabs_named(4), 0, 3, 1);
        assert_eq!(order(&tabs), vec![1, 2, 0, 3]);
        assert_eq!(act, 0, "still tab 1");

        // Moving a later tab to the front pushes the active index up.
        let (tabs, act) = reorder_tabs(tabs_named(4), 2, 0, 1);
        assert_eq!(order(&tabs), vec![2, 0, 1, 3]);
        assert_eq!(act, 2, "still tab 1");
    }

    #[test]
    fn reorder_tabs_is_a_noop_for_an_unchanged_position() {
        // Dropping a tab on itself…
        let (tabs, act) = reorder_tabs(tabs_named(3), 1, 1, 1);
        assert_eq!(order(&tabs), vec![0, 1, 2]);
        assert_eq!(act, 1);
        // …and the classic off-by-one: inserting "just after itself" is the same
        // list, and must not shuffle anything.
        let (tabs, act) = reorder_tabs(tabs_named(3), 1, 2, 1);
        assert_eq!(order(&tabs), vec![0, 1, 2]);
        assert_eq!(act, 1);
    }

    #[test]
    fn reorder_tabs_ignores_out_of_range() {
        let (tabs, act) = reorder_tabs(tabs_named(3), 9, 0, 1);
        assert_eq!(order(&tabs), vec![0, 1, 2]);
        assert_eq!(act, 1);
        let (tabs, act) = reorder_tabs(tabs_named(3), 0, 9, 1);
        assert_eq!(order(&tabs), vec![0, 1, 2]);
        assert_eq!(act, 1);
    }

    #[test]
    fn overlay_anchor_places_each_position() {
        // A 200x100 pane at the origin, 10pt margin.
        let pane = egui::Rect::from_min_size(egui::pos2(0.0, 0.0), egui::vec2(200.0, 100.0));
        let m = 10.0;
        let cases = [
            (P::Center, egui::pos2(100.0, 50.0), Align2::CENTER_CENTER),
            (P::TopLeft, egui::pos2(10.0, 10.0), Align2::LEFT_TOP),
            (P::TopCenter, egui::pos2(100.0, 10.0), Align2::CENTER_TOP),
            (P::TopRight, egui::pos2(190.0, 10.0), Align2::RIGHT_TOP),
            (P::BottomLeft, egui::pos2(10.0, 90.0), Align2::LEFT_BOTTOM),
            (P::BottomCenter, egui::pos2(100.0, 90.0), Align2::CENTER_BOTTOM),
            (P::BottomRight, egui::pos2(190.0, 90.0), Align2::RIGHT_BOTTOM),
        ];
        for (pos, want_at, want_align) in cases {
            let (at, align) = overlay_anchor(pane, pos, m);
            assert_eq!(at, want_at, "{pos:?} anchor");
            assert_eq!(align, want_align, "{pos:?} align");
        }
    }

    #[test]
    fn dim_overlay_alpha_is_one_minus_split_opacity() {
        // Ghostty's default: a 0.7-opacity split is covered by a 30% overlay.
        // (1.0 - 0.7) * 255.0 lands on exactly 76.5 in f32, so the result depends
        // on the rounding mode — `round()` takes halves away from zero, giving 77
        // where a truncating cast would give 76. Pin it.
        assert_eq!(dim_alpha(0.7), 77);
        // Fully opaque split ⇒ no overlay at all.
        assert_eq!(dim_alpha(1.0), 0);
        // The config floor (0.15) is the strongest dim reachable.
        assert_eq!(dim_alpha(0.15), 217);
        // Out-of-range input can't produce a wrapped/garbage alpha.
        assert_eq!(dim_alpha(-1.0), 255);
        assert_eq!(dim_alpha(2.0), 0);
    }

    fn rect(x: f32, y: f32, w: f32, h: f32) -> egui::Rect {
        egui::Rect::from_min_size(egui::pos2(x, y), egui::vec2(w, h))
    }

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
    fn collect_leaf_yields_only_the_zoomed_pane_full_area() {
        // A 3-pane tree; zooming pane 2 should lay out only pane 2 at full area.
        let mut root = split(true, leaf(1, 1), split(false, leaf(2, 1), leaf(3, 1)));
        let area = rect(0.0, 0.0, 100.0, 80.0);
        let mut leaves = Vec::new();
        root.collect_leaf(2, area, &mut leaves);
        assert_eq!(leaves.len(), 1);
        assert_eq!(leaves[0].id, 2);
        assert_eq!(leaves[0].rect, area);
        // An absent id collects nothing (the render path then falls back to all).
        let mut none = Vec::new();
        root.collect_leaf(99, area, &mut none);
        assert!(none.is_empty());
    }

    #[test]
    fn reap_tabs_clears_zoom_when_zoomed_pane_dies() {
        // Tab zoomed on pane 2; pane 2's shell exits → zoom must drop, not dangle.
        let root = split(true, leaf(1, 1), leaf(2, 0));
        let tabs = vec![Tab {
            root,
            focus: 1,
            name: None,
            color: None,
            zoomed: Some(2),
        }];
        let (survivors, _active) = reap_tabs(tabs, 0, &mut |p: &u32| *p == 0);
        assert_eq!(survivors.len(), 1);
        assert_eq!(survivors[0].zoomed, None);
    }

    #[test]
    fn reap_tabs_keeps_zoom_when_zoomed_pane_survives() {
        let root = split(true, leaf(1, 1), leaf(2, 0));
        let tabs = vec![Tab {
            root,
            focus: 1,
            name: None,
            color: None,
            zoomed: Some(1),
        }];
        let (survivors, _active) = reap_tabs(tabs, 0, &mut |p: &u32| *p == 0);
        assert_eq!(survivors[0].zoomed, Some(1));
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
        let pruned = root
            .prune(&mut |p: &u32| *p == 0)
            .expect("survivors remain");
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
    fn payload_mut_finds_and_mutates_target() {
        let mut root = split(true, leaf(1, 10), split(false, leaf(2, 20), leaf(3, 30)));
        // A nested leaf is reachable and mutable through &mut.
        *root.payload_mut(2).expect("leaf 2 present") = 99;
        assert_eq!(root.payload(2), Some(&99));
        // The other leaves are untouched; an absent id yields None.
        assert_eq!(root.payload(3), Some(&30));
        assert!(root.payload_mut(42).is_none());
    }

    #[test]
    fn leaf_ids_walks_the_tree() {
        let root = split(false, split(true, leaf(7, 1), leaf(8, 1)), leaf(9, 1));
        let mut ids = Vec::new();
        root.leaf_ids(&mut ids);
        assert_eq!(ids, vec![7, 8, 9]);
    }

    #[test]
    fn cycle_pick_wraps_both_ways() {
        let ids = [1u64, 2, 3];
        assert_eq!(cycle_pick(&ids, 1, true), Some(2));
        assert_eq!(cycle_pick(&ids, 3, true), Some(1)); // wrap forward
        assert_eq!(cycle_pick(&ids, 1, false), Some(3)); // wrap backward
        assert_eq!(cycle_pick(&ids, 2, false), Some(1));
        // An absent focus starts from the first; <2 panes has nothing to cycle.
        assert_eq!(cycle_pick(&ids, 99, true), Some(2));
        assert_eq!(cycle_pick(&[5u64], 5, true), None);
    }

    #[test]
    fn nav_dir_picks_the_adjacent_pane() {
        // A 2x2 grid: 1 2 / 3 4 (1px gutters between the 50px cells).
        let layout = [
            (1u64, rect(0.0, 0.0, 50.0, 50.0)),
            (2, rect(51.0, 0.0, 50.0, 50.0)),
            (3, rect(0.0, 51.0, 50.0, 50.0)),
            (4, rect(51.0, 51.0, 50.0, 50.0)),
        ];
        // Directional moves pick the cross-axis-overlapping neighbor, not the diagonal.
        assert_eq!(nav_dir(&layout, 1, Dir::Right), Some(2));
        assert_eq!(nav_dir(&layout, 2, Dir::Left), Some(1));
        assert_eq!(nav_dir(&layout, 1, Dir::Down), Some(3));
        assert_eq!(nav_dir(&layout, 4, Dir::Up), Some(2));
        // No neighbor on that side → None (and unknown focus → None).
        assert_eq!(nav_dir(&layout, 1, Dir::Left), None);
        assert_eq!(nav_dir(&layout, 1, Dir::Up), None);
        assert_eq!(nav_dir(&layout, 99, Dir::Right), None);
    }

    #[test]
    fn nav_dir_prefers_the_nearest_on_axis() {
        // One tall pane on the left, two stacked on the right; from the left pane,
        // Right should pick whichever right pane overlaps its center band.
        let layout = [
            (1u64, rect(0.0, 0.0, 50.0, 100.0)),
            (2, rect(51.0, 0.0, 50.0, 50.0)),
            (3, rect(51.0, 51.0, 50.0, 50.0)),
        ];
        // Pane 1's center (y=50) sits on the boundary; both right panes overlap,
        // and the tie breaks to the nearer cross-axis center.
        let pick = nav_dir(&layout, 1, Dir::Right);
        assert!(pick == Some(2) || pick == Some(3));
        assert_eq!(nav_dir(&layout, 2, Dir::Down), Some(3));
        assert_eq!(nav_dir(&layout, 3, Dir::Up), Some(2));
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
        let tabs = vec![Tab {
            root,
            focus: 2,
            name: None,
            color: None,
            zoomed: None,
        }];
        let (survivors, _active) = reap_tabs(tabs, 0, &mut |p: &u32| *p == 0);
        assert_eq!(survivors.len(), 1);
        assert_eq!(survivors[0].leaf_count(), 1);
        assert_eq!(survivors[0].focus, 1);
    }

    #[test]
    fn keep_only_tab_collapses_to_one_and_selects_it() {
        let tabs = vec![Tab::leaf(1, 0u32), Tab::leaf(2, 0u32), Tab::leaf(3, 0u32)];
        let (survivors, active) = keep_only_tab(tabs, 1, 2);
        assert_eq!(survivors.len(), 1);
        assert_eq!(active, 0);
        assert_eq!(survivors[0].focus, 2); // the kept tab's leaf id
    }

    #[test]
    fn keep_only_tab_is_noop_when_out_of_range() {
        let tabs = vec![Tab::leaf(1, 0u32), Tab::leaf(2, 0u32)];
        let (survivors, active) = keep_only_tab(tabs, 5, 1);
        assert_eq!(survivors.len(), 2);
        assert_eq!(active, 1);
    }

    #[test]
    fn truncate_tabs_to_right_drops_later_tabs_and_clamps_active() {
        // Active is past the cut → clamps back to the new last index.
        let tabs = vec![
            Tab::leaf(1, 0u32),
            Tab::leaf(2, 0u32),
            Tab::leaf(3, 0u32),
            Tab::leaf(4, 0u32),
        ];
        let (survivors, active) = truncate_tabs_to_right(tabs, 1, 3);
        assert_eq!(survivors.len(), 2);
        assert_eq!(active, 1);
    }

    #[test]
    fn truncate_tabs_to_right_keeps_active_when_left_of_cut() {
        let tabs = vec![Tab::leaf(1, 0u32), Tab::leaf(2, 0u32), Tab::leaf(3, 0u32)];
        let (survivors, active) = truncate_tabs_to_right(tabs, 1, 0);
        assert_eq!(survivors.len(), 2);
        assert_eq!(active, 0);
    }

    #[test]
    fn truncate_tabs_to_right_is_noop_at_last_tab() {
        let tabs = vec![Tab::leaf(1, 0u32), Tab::leaf(2, 0u32)];
        let (survivors, active) = truncate_tabs_to_right(tabs, 1, 1);
        assert_eq!(survivors.len(), 2);
        assert_eq!(active, 1);
    }

    #[test]
    fn ellipsize_truncates_with_ellipsis() {
        assert_eq!(ellipsize("short", 10), "short");
        assert_eq!(ellipsize("a very long tab title", 6), "a ver…");
    }
}
