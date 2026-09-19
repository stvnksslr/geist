//! The eframe application: tabs, each holding one or more split panes. The
//! active tab's panes are laid out, driven, and painted in a single GPU callback.

use std::sync::Arc;
use std::time::Duration;

use anyhow::Result;
use eframe::egui;
use eframe::egui_wgpu;

use crate::command::{self, Action, PaletteState};
use crate::config::{Config, MiddleClickAction, ResizeOverlayPosition, RightClickAction};
use crate::profiles::{self, Profile};
use crate::keybind::{Chord, Keymap};
use crate::render::{self, BgImageFrame, PaneFrame, TermFrame};
use crate::scrollbar;
use crate::session::{self, Session};
use crate::theme;

/// A tab: a binary tree of panes (`Node`) with one focused leaf (by id). Each
/// split divides only the focused pane, so splits nest (like Ghostty) instead of
/// re-flowing every pane onto a single shared axis. Generic over the leaf
/// payload `T` (the app uses `Session`; tests use a lightweight stand-in) so the
/// tree's structural logic is unit-testable without spawning a shell.
struct Tab<T> {
    /// Stable identity, drawn from the same per-window counter as the leaf ids.
    ///
    /// Tabs are addressed by *index* everywhere they are addressed within one
    /// pass — a reorder or a reap invalidates an index, which is why every
    /// mutation point clears the ones held across frames. An undo entry outlives
    /// far more than a frame, so it holds this instead.
    id: u64,
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
    /// A one-pane tab. `id` names both the tab and its single leaf: the ids come
    /// from one monotonic per-window counter, so reusing the value for the tab
    /// costs nothing and cannot collide with another tab's.
    fn leaf(id: u64, payload: T) -> Self {
        Self {
            id,
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
    /// The id of the first leaf (in layout order) whose payload satisfies `f`.
    fn find_leaf(&self, f: &mut impl FnMut(&T) -> bool) -> Option<u64> {
        match self {
            Node::Leaf { id, payload } => f(payload).then_some(*id),
            Node::Split { first, second, .. } => {
                first.find_leaf(f).or_else(|| second.find_leaf(f))
            }
            Node::Empty => None,
        }
    }

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

    /// Remove leaf `target` and hand it back with enough context to put it
    /// exactly where it was — the primitive behind undoing a closed split.
    ///
    /// The pane is *moved* out rather than dropped, so its shell keeps running
    /// while the undo entry holds it (Ghostty's undo does the same, retaining
    /// the live `SurfaceView`). [`Node::prune`] is the discarding counterpart,
    /// for panes whose shell already exited. Returns the surviving
    /// subtree, plus a [`PaneSlot`] describing where the removed leaf sat and
    /// the leaf itself.
    ///
    /// The slot's path names the **parent split**, because that is what the
    /// sibling collapses into: after the removal, the subtree that replaced the
    /// parent is at exactly that path. A `None` slot means `target` wasn't a
    /// child of any split here — the caller closes the tab instead of a pane.
    fn detach_leaf(self, target: u64) -> (Option<Node<T>>, Detached<T>) {
        let Node::Split {
            vertical,
            first,
            second,
        } = self
        else {
            // A bare leaf (or `Empty`): nothing to collapse into.
            return (Some(self), None);
        };
        let in_first = first.contains(target);
        let (child, sibling) = if in_first {
            (*first, *second)
        } else {
            (*second, *first)
        };
        if matches!(&child, Node::Leaf { id, .. } if *id == target) {
            let slot = PaneSlot {
                path: Vec::new(),
                vertical,
                first: in_first,
            };
            return (Some(sibling), Some((slot, child)));
        }
        let (rebuilt, taken) = child.detach_leaf(target);
        // Prepend this level's step, so the path reads root-downwards.
        let taken = taken.map(|(mut slot, node)| {
            slot.path.insert(0, in_first);
            (slot, node)
        });
        let node = match rebuilt {
            Some(c) => {
                let (f, s) = if in_first { (c, sibling) } else { (sibling, c) };
                Node::Split {
                    vertical,
                    first: Box::new(f),
                    second: Box::new(s),
                }
            }
            None => sibling,
        };
        (Some(node), taken)
    }

    /// Put a subtree back at `slot`, re-creating the split it was removed from.
    ///
    /// Walks the slot's path to the subtree that took the old parent's place and
    /// wraps it in a split again, on the original axis and the original side. If
    /// the path no longer resolves to a split — the layout changed under the
    /// undo entry — it attaches at the deepest point it *could* reach rather
    /// than giving up: a pane restored in the wrong place is recoverable; a
    /// dropped one takes a running shell with it.
    fn attach_at(&mut self, slot: &PaneSlot, node: Node<T>) {
        let mut cur = self;
        for step in &slot.path {
            let Node::Split { first, second, .. } = cur else {
                break;
            };
            cur = if *step { first } else { second };
        }
        let sibling = std::mem::replace(cur, Node::Empty);
        let (first, second) = if slot.first {
            (node, sibling)
        } else {
            (sibling, node)
        };
        *cur = Node::Split {
            vertical: slot.vertical,
            first: Box::new(first),
            second: Box::new(second),
        };
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
    fn collect<'a>(&'a mut self, area: egui::Rect, ppp: f32, out: &mut Vec<Leaf<'a, T>>) {
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
                let (a, _, b) = split_rect(area, *vertical, ppp);
                first.collect(a, ppp, out);
                second.collect(b, ppp, out);
            }
            Node::Empty => {}
        }
    }

    /// Append every split gutter's rect to `out`, using the same division as
    /// [`collect`](Self::collect).
    ///
    /// Separate from `collect` — and immutable — so the caller can gather the
    /// gutters *before* `collect` takes `&mut self` for the panes. The two
    /// borrows are sequential, so neither needs to know about the other.
    fn gutters(&self, area: egui::Rect, ppp: f32, out: &mut Vec<egui::Rect>) {
        if let Node::Split {
            vertical,
            first,
            second,
        } = self
        {
            let (a, g, b) = split_rect(area, *vertical, ppp);
            out.push(g);
            first.gutters(a, ppp, out);
            second.gutters(b, ppp, out);
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
            id,
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
                id,
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
///
/// The third element is what was closed, as `(original index, tab)` in ascending
/// order — the shape an undo entry needs to put them back where they were. The
/// tabs are handed back rather than dropped, so their shells keep running.
fn keep_only_tab<T>(
    tabs: Vec<Tab<T>>,
    keep: usize,
    active: usize,
) -> (Vec<Tab<T>>, usize, ClosedTabs<T>) {
    if keep >= tabs.len() {
        return (tabs, active, Vec::new());
    }
    let mut kept = Vec::with_capacity(1);
    let mut closed = Vec::with_capacity(tabs.len() - 1);
    // A partition rather than the swap-and-truncate this used to be: the swap
    // discarded which slot each closed tab came from, which is exactly what a
    // restore needs.
    for (i, tab) in tabs.into_iter().enumerate() {
        if i == keep {
            kept.push(tab);
        } else {
            closed.push((i, tab));
        }
    }
    (kept, 0, closed)
}

/// Drop every tab after `idx` (Ghostty's "Close Tabs to the Right"), clamping the
/// active selection into the survivors. Pure core of [`App::close_tabs_to_right`].
fn truncate_tabs_to_right<T>(
    mut tabs: Vec<Tab<T>>,
    idx: usize,
    active: usize,
) -> (Vec<Tab<T>>, usize, ClosedTabs<T>) {
    if idx + 1 < tabs.len() {
        let closed = tabs
            .split_off(idx + 1)
            .into_iter()
            .enumerate()
            .map(|(k, t)| (idx + 1 + k, t))
            .collect();
        let active = active.min(tabs.len() - 1);
        (tabs, active, closed)
    } else {
        (tabs, active, Vec::new())
    }
}

/// Re-insert closed tabs at the indices they came from, ascending, and return
/// their ids plus the selection to leave behind.
///
/// The inverse of [`keep_only_tab`] / [`truncate_tabs_to_right`] and of a plain
/// `close_tab`, and pure for the same reason they are: the index arithmetic is
/// the part that can be off by one. Ascending order is what makes the naive
/// insert correct — everything before a slot is already back in place, so the
/// slot means what it meant when the tab was closed.
///
/// `active` is the selection from *before* the close, which is also the right
/// one after: restoring the original order restores the original indices.
fn reinsert_tabs<T>(
    tabs: &mut Vec<Tab<T>>,
    closed: ClosedTabs<T>,
    active: usize,
) -> (Vec<u64>, usize) {
    let mut ids = Vec::with_capacity(closed.len());
    for (at, tab) in closed {
        ids.push(tab.id);
        tabs.insert(at.min(tabs.len()), tab);
    }
    (ids, active.min(tabs.len().saturating_sub(1)))
}

/// Remove the tabs with `ids`, handing them back with the slots they came from.
///
/// `None` when the ids name every tab there is: a window with no tabs is a
/// *closed window*, which is a different operation with a different undo entry,
/// and quietly escalating into one would surprise. Unknown ids are skipped —
/// the tab was already closed some other way.
fn remove_tabs_by_id<T>(
    tabs: &mut Vec<Tab<T>>,
    ids: &[u64],
    active: usize,
) -> Option<(ClosedTabs<T>, usize)> {
    let mut at: Vec<usize> = ids
        .iter()
        .filter_map(|id| tabs.iter().position(|t| t.id == *id))
        .collect();
    at.sort_unstable();
    at.dedup();
    if at.is_empty() || at.len() >= tabs.len() {
        return None;
    }
    // Back-to-front, so the earlier indices stay valid while removing; then
    // ascending again, which is the order `reinsert_tabs` needs.
    let mut closed: ClosedTabs<T> = at.iter().rev().map(|&i| (i, tabs.remove(i))).collect();
    closed.reverse();
    Some((closed, active.min(tabs.len() - 1)))
}

/// Snapshot a split tree for `window-save-state`, marking the leaf with id
/// `focus`. Generic over the payload (and taking the working directory through a
/// closure) so the walk is unit-testable without spawning shells — the same
/// reason [`Node`] itself is generic.
fn capture_node_with<T>(
    node: &Node<T>,
    focus: u64,
    pwd: &impl Fn(&T) -> Option<String>,
) -> crate::state::SavedNode {
    match node {
        Node::Split {
            vertical,
            first,
            second,
        } => crate::state::SavedNode::Split {
            vertical: *vertical,
            first: Box::new(capture_node_with(first, focus, pwd)),
            second: Box::new(capture_node_with(second, focus, pwd)),
        },
        Node::Leaf { id, payload } => crate::state::SavedNode::Leaf {
            cwd: pwd(payload),
            focused: *id == focus,
        },
        // `Empty` only ever exists mid-restructure, never at capture time; a
        // pane-less leaf is the closest honest thing to write.
        Node::Empty => crate::state::SavedNode::Leaf {
            cwd: None,
            focused: false,
        },
    }
}

/// [`capture_node_with`] over live sessions, reading each pane's OSC 7 cwd.
fn capture_node(node: &Node<Session>, focus: u64) -> crate::state::SavedNode {
    capture_node_with(node, focus, &|s: &Session| {
        s.pwd().map(|p| p.display().to_string())
    })
}

/// Where a pane sat in its tab's split tree, so a removed one can go back.
///
/// A *path plus an axis and a side*, rather than a neighbour's id: the sibling a
/// removed pane collapsed into may itself be a whole subtree, and naming one of
/// its leaves would not say which ancestor to wrap.
#[derive(Clone, Debug, PartialEq, Eq)]
struct PaneSlot {
    /// Root-downwards steps to the parent split's position, `true` = first child.
    path: Vec<bool>,
    /// The axis that split was on.
    vertical: bool,
    /// Whether the removed pane was that split's *first* child.
    first: bool,
}

/// A pane taken out of a split tree: where it was, and the subtree itself.
type Detached<T> = Option<(PaneSlot, Node<T>)>;

/// Tabs removed from a window, each with the index it came from, ascending.
/// The shape an undo entry re-inserts from.
type ClosedTabs<T> = Vec<(usize, Tab<T>)>;

/// Everything the inspector shows, read off the session in one go.
///
/// A snapshot rather than a live borrow, because the panel is drawn inside an
/// egui closure that already borrows `self` — the same deferred shape the
/// palette and the search overlay use.
#[derive(Default)]
struct InspectorFacts {
    surface: Vec<(String, String)>,
    terminal: Vec<(String, String)>,
    keys: Vec<crate::inspector::KeyRecord>,
    io: Vec<crate::inspector::IoRecord>,
    paused: bool,
}

/// One `label: value` line in the inspector, values in the mono face so
/// coordinates and hex colors line up down the column.
fn fact_row(ui: &mut egui::Ui, key: &str, value: &str, mono: &egui::FontId) {
    ui.horizontal(|ui| {
        ui.add_sized([130.0, 0.0], egui::Label::new(egui::RichText::new(key).weak()));
        ui.label(egui::RichText::new(value).font(mono.clone()));
    });
}

fn yes_no(v: bool) -> &'static str {
    if v { "yes" } else { "no" }
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
    /// The chrome palette derived from `config`, cached so painting code can
    /// reach for a named color instead of a literal. Rebuilt on config reload
    /// alongside the egui `Style` (see [`crate::theme::install`]).
    chrome: theme::Chrome,
    /// Available shell profiles and the default index, detected at startup.
    profiles: Vec<Profile>,
    default_profile: usize,
    egui_ctx: egui::Context,
    last_window_title: Option<String>,
    /// `set_window_title:` — replaces the focused pane's title as the window's.
    title_override: Option<String>,
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
    /// Leaders pressed so far in a multi-key sequence (`ctrl+a>n`), empty when
    /// no sequence is in progress. Held across frames because that is exactly
    /// what a sequence is: state between two key events.
    pending_keys: Vec<Chord>,
    /// Active key tables, innermost **last**. Runtime state, not config: the
    /// stack is per window and is cleared on a config reload, since a reload can
    /// delete a table whose name is still on it — and a stale name silently
    /// changing which bindings resolve is the worst outcome available.
    /// `true` marks a one-shot activation, popped when one of its binds runs.
    key_tables: Vec<crate::keybind::TableEntry>,
    /// Whether `mouse-hide-while-typing` has the pointer hidden right now.
    pointer_hidden: bool,
    /// Counter for `write_*_file` temp names. A counter, not a clock: two
    /// captures in the same second would collide on a timestamp.
    write_file_seq: u64,
    /// Tracked rather than read back: winit exposes no maximized query through
    /// eframe, so `toggle_maximize` keeps its own flag (same shape as
    /// `fullscreen`).
    maximized: bool,
    /// Whether `toggle_window_float_on_top` has the window pinned above others.
    float_on_top: bool,
    /// `toggle_background_opacity`: force fully opaque, overriding the config.
    opaque_override: bool,
    /// Whether `window-width`/`-height`/`-position-*` have been applied. They are
    /// *initial* geometry, so this fires once — re-applying would fight the user
    /// every time they resized.
    geometry_applied: bool,
    /// Whether the window is currently fullscreen (`toggle_fullscreen`). Tracked
    /// here because the viewport's fullscreen flag isn't readable back, so we flip
    /// our own copy and command winit to match.
    fullscreen: bool,
    /// Whether this window is *the* quick terminal (Ghostty's dropdown
    /// terminal). At most one exists; it is an ordinary window in every other
    /// respect, so tabs, splits and the palette all work inside it.
    quick: bool,
    /// Whether the quick terminal is on screen. Hiding it means **not drawing
    /// its viewport**, which destroys the native window while leaving the
    /// `Window` and its running shells untouched — the whole point of a
    /// dropdown terminal is that it comes back exactly as you left it.
    quick_visible: bool,
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
    /// The diagnostics set the user dismissed with "Ignore". The config-errors
    /// dialog shows while `config.diagnostics` is non-empty and differs from
    /// this, so an ignored set stays hidden until a reload produces a new one.
    config_errors_ignored: Option<Vec<String>>,
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
    /// Whether any pane's `OSC 9;4` progress changed this pass, so the taskbar
    /// button needs updating. Set here, consumed by [`App::update_progress`] —
    /// the button belongs to the process, not to a pane.
    progress_dirty: bool,
    /// The decoded `background-image`. Decoded when the config is (re)loaded,
    /// never per frame; `None` when the key is unset or the file failed to load
    /// (the error is reported once, at load time). The renderer identifies the
    /// texture it holds by this `Arc`, so sharing one across windows matters —
    /// see [`load_bg_image`].
    bg_image: Option<Arc<crate::bgimage::BgImage>>,
    /// `custom-shader`, compiled to WGSL when the config was (re)loaded. The
    /// renderer keys its GPU pipelines on this `Arc`'s identity, so it must be
    /// replaced wholesale rather than mutated. Empty means no offscreen pass at
    /// all — the ordinary render path.
    custom_shaders: Arc<Vec<render::CustomShader>>,
    /// When the shader clock started, for `iTime`. Reset whenever the shader
    /// set is reloaded so an edited shader restarts from zero.
    shader_epoch: std::time::Instant,
    /// Previous frame's `iTime`, for `iTimeDelta`.
    shader_last_time: f32,
    /// Frames rendered with shaders active, for `iFrame`.
    shader_frame: i32,
    /// Whether the app's undo/redo stacks have anything on them, mirrored here
    /// every pass by [`App::ui`]. The `performable:` gate that decides whether
    /// `ctrl+shift+z` is the app's key or the shell's runs inside
    /// `session::decide_key`, which has no route back to [`App`].
    undo_state: crate::command::UndoState,
    /// Last frame's inspector-window rect, when one was drawn.
    ///
    /// The inspector is **not** modal, so the pane underneath still takes the
    /// pointer — a click on its Pause button would otherwise also start a text
    /// selection. `render_active` runs before the window exists, so this is the
    /// previous frame's rect, the same idiom as `last_layout`.
    inspector_rect: Option<egui::Rect>,
    /// Geometry to command this window onto on its next pass, then forget.
    ///
    /// Set when `undo` re-opens a closed window: a restored window has to come
    /// back where it was, and its `ViewportBuilder` can't say so — the builder
    /// is rebuilt and compared every pass, so a position in it would either be
    /// re-sent forever or fight the user's next drag.
    place_geom: Option<(egui::Pos2, egui::Vec2)>,
}


/// Process-wide one-slot cache of the decoded `background-image`, keyed by its
/// resolved path.
///
/// Not just an optimization. Every window shares a single eframe `RenderState`,
/// and therefore **one** background-image texture; the renderer decides whether
/// to re-upload it by comparing `Arc` identity. Two windows holding distinct
/// `Arc`s of the same file would each see the other's texture as foreign and
/// re-upload it every frame, forever. Handing out one `Arc` per path makes that
/// impossible, and makes a reload that doesn't change the path free.
static BG_IMAGE_CACHE: std::sync::Mutex<Option<(std::path::PathBuf, Arc<crate::bgimage::BgImage>)>> =
    std::sync::Mutex::new(None);

/// Load and translate every configured `custom-shader`.
///
/// A shader that fails to read or compile is **reported and skipped**, not
/// fatal: Ghostty does the same, and the alternative — refusing to start over a
/// typo in a decorative effect — is worse. The message carries the GLSL
/// compiler's own diagnostic, since nothing else is actionable.
fn load_custom_shaders(cfg: &Config) -> Arc<Vec<render::CustomShader>> {
    if cfg.custom_shaders.is_empty() {
        return Arc::new(Vec::new());
    }
    let dir = crate::config::config_dir();
    let mut out = Vec::new();
    for raw in &cfg.custom_shaders {
        let Some(path) = crate::config::resolve_path(raw, dir.as_deref()) else {
            continue;
        };
        let name = path
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_else(|| raw.clone());
        match std::fs::read_to_string(&path) {
            Ok(src) => match crate::shader::compile(&src) {
                Ok(wgsl) => out.push(render::CustomShader { name, wgsl }),
                Err(e) => eprintln!(
                    "giest: custom-shader {} failed to compile:\n{e:#}",
                    path.display()
                ),
            },
            Err(e) => eprintln!("giest: could not read custom-shader {}: {e}", path.display()),
        }
    }
    Arc::new(out)
}

/// Decode the configured `background-image`, reporting a failure once.
///
/// Returns `None` for "no image", including on error: a bad path must not be
/// fatal and must not retry every frame, so this is called only when the config
/// is (re)loaded. Relative paths resolve against the config directory, like
/// every other `Path`-valued key.
fn load_bg_image(cfg: &Config) -> Option<Arc<crate::bgimage::BgImage>> {
    let raw = cfg.background_image.as_deref()?;
    let dir = crate::config::config_dir();
    let path = crate::config::resolve_path(raw, dir.as_deref())?;

    let mut cache = BG_IMAGE_CACHE.lock().ok()?;
    if let Some((cached, img)) = cache.as_ref()
        && *cached == path
    {
        return Some(img.clone());
    }
    match crate::bgimage::load(&path) {
        Ok(img) => {
            let img = Arc::new(img);
            *cache = Some((path, img.clone()));
            Some(img)
        }
        Err(e) => {
            eprintln!("giest: could not load background-image {}: {e}", path.display());
            // Drop any previously cached image so its VRAM is released once the
            // renderer swaps to the placeholder.
            *cache = None;
            None
        }
    }
}

/// A structural change with a known inverse, held so `undo` can take it back.
///
/// The vocabulary is deliberately **symmetric**: every op has a counterpart that
/// undoes it, and performing either one records that counterpart (see
/// [`crate::undo`]). So undoing a close and undoing a *creation* are the same
/// two code paths run in opposite directions, and there is no third
/// implementation of "the opposite of this" to drift.
///
/// The `Restore*` variants **own live sessions**: the shells they hold keep
/// running while the entry does, which is what makes an undo lossless — and
/// exactly why entries expire.
enum UndoOp {
    /// Put a removed pane subtree back into a tab, where it was.
    ///
    /// The subtree is boxed because a `Node::Leaf` holds a whole `Session`
    /// inline: unboxed it would make every `UndoOp` — and so every
    /// `AppRequest` — as large as a session.
    RestorePane {
        window: u64,
        tab: u64,
        slot: PaneSlot,
        node: Box<Node<Session>>,
    },
    /// Take that pane back out again (also: undo a `new_split`).
    RemovePane { window: u64, tab: u64, leaf: u64 },
    /// Re-insert closed tabs at their old indices, in ascending order.
    RestoreTabs {
        window: u64,
        tabs: Vec<(usize, Tab<Session>)>,
        /// The tab to leave selected — an index into the window *after* the
        /// re-insertion, so it survives the shifting the inserts cause.
        active: usize,
    },
    /// Close those tabs again (also: undo a `new_tab`), by tab id.
    RemoveTabs { window: u64, ids: Vec<u64> },
    /// Re-open a closed window, with every shell in it still running.
    RestoreWindow { window: Box<Window> },
    /// Close it again (also: undo a `new_window`).
    RemoveWindow { window: u64 },
}

/// Something only [`App`] can do, raised from inside a window's pass.
enum AppRequest {
    /// Open a window, with the working directory already resolved from the
    /// raising window's focused pane (Ghostty resolves it from the previously
    /// focused surface, not from the new one's parent).
    NewWindow(Option<std::path::PathBuf>),
    /// Retire the raising window. Quits giest when it's the last one.
    ///
    /// `undoable` is false for the one path that must never be reversible: a
    /// window whose last shell *exited*. There is nothing to restore — the
    /// processes are gone — and an undo entry would hold a window of dead panes.
    CloseWindow { undoable: bool },
    /// Show or hide the quick terminal, creating it on first use.
    ToggleQuickTerminal,
    /// File `op` as the reverse of a change this window just made.
    Record(UndoOp),
    /// Ghostty `undo` / `redo`.
    Undo,
    Redo,
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
    /// The layout as it stood at the last window close, for `window-save-state`.
    ///
    /// Held because by the time `on_exit` runs there are no windows left to read
    /// — quitting *is* closing the last one. Refreshed at the top of every
    /// [`App::retire`], so it always describes the moment before the close that
    /// ended the process, which is what a restore should reopen.
    last_state: crate::state::SavedState,
    /// Actions bound to `global:` triggers, indexed the same way as the
    /// bindings handed to [`crate::hotkey`] — the hook reports an index, this
    /// turns it back into an action.
    global_actions: Vec<Action>,
    /// The chords currently registered with the hook, so a pass only re-installs
    /// when they actually changed (a config reload). Re-registering every frame
    /// would take a lock on the OS input path 60 times a second.
    global_chords: Vec<crate::keybind::Chord>,
    /// Ghostty `undo` / `redo`. App-scoped rather than per-window because a
    /// *window* close is itself undoable — a stack living on the window that
    /// closed would go with it. Its entries own the removed panes, tabs and
    /// windows, shells still running, until they expire.
    undo: crate::undo::UndoStack<UndoOp>,
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

/// How much of a clipboard payload the confirmation dialog shows.
const PREVIEW_LIMIT: usize = 2000;

/// Render clipboard text for the confirmation dialog: control characters made
/// visible, and the whole thing capped at `limit` characters.
///
/// Both halves matter, because this text is chosen by whoever produced the
/// paste. Escapes are shown as `␛` rather than passed through, so a payload
/// can't use them to dress itself up as part of the dialog — and the newline
/// count is *why* the user is being asked, so it has to be legible rather than
/// silently laid out as ordinary wrapped text. The cap keeps a megabyte-long
/// paste from making the dialog unusable; the count tells the user what's hidden.
fn preview_text(text: &str, limit: usize) -> String {
    let mut out = String::with_capacity(text.len().min(limit) + 32);
    for ch in text.chars().take(limit) {
        match ch {
            '\n' => out.push_str("⏎\n"),
            '\t' => out.push('→'),
            '\r' => out.push('␍'),
            '\x1b' => out.push('␛'),
            // Every other C0 control plus DEL.
            c if (c.is_control() && c != '\n') || c == '\u{7f}' => out.push('␦'),
            c => out.push(c),
        }
    }
    let remaining = text.chars().count().saturating_sub(limit);
    if remaining > 0 {
        out.push_str(&format!("\n… {remaining} more characters"));
    }
    out
}

/// Lay out `text` with the characters at `matched` in `hit` and the rest in
/// `base`.
///
/// `matched` holds **char** indices (that is what
/// [`command::fuzzy_match_indices`] returns) while [`egui::text::LayoutJob`]
/// sections are **byte** ranges, so they are mapped rather than used directly.
/// Every command title is ASCII today — but `New Tab with <shell>` interpolates
/// a detected profile name, which need not be, and a byte range landing mid-UTF-8
/// panics inside egui rather than merely looking wrong.
///
/// Highlighting is by color only, never weight: swapping in a bold face changes
/// glyph advances, so the row would visibly reflow with each keystroke.
fn highlight_job(
    text: &str,
    matched: &[usize],
    font: egui::FontId,
    base: egui::Color32,
    hit: egui::Color32,
) -> egui::text::LayoutJob {
    use egui::text::{LayoutJob, TextFormat};
    let mut job = LayoutJob::default();
    let plain = TextFormat::simple(font.clone(), base);
    if matched.is_empty() {
        job.append(text, 0.0, plain);
        return job;
    }
    let lit = TextFormat::simple(font, hit);
    // Walk the string once, grouping neighbouring chars that share a state into
    // one section — a section per character would be pathological.
    let mut run_start = 0usize;
    let mut run_hit = false;
    let mut mi = 0usize;
    for (ci, (bi, ch)) in text.char_indices().enumerate() {
        while mi < matched.len() && matched[mi] < ci {
            mi += 1;
        }
        let is_hit = mi < matched.len() && matched[mi] == ci;
        if ci == 0 {
            run_hit = is_hit;
        } else if is_hit != run_hit {
            job.append(
                &text[run_start..bi],
                0.0,
                if run_hit { lit.clone() } else { plain.clone() },
            );
            run_start = bi;
            run_hit = is_hit;
        }
        let _ = ch;
    }
    job.append(
        &text[run_start..],
        0.0,
        if run_hit { lit } else { plain },
    );
    job
}

/// How long an in-app toast stays up (GTK's `adw_toast` default is ~2-3 s).
const TOAST_SECS: f32 = 2.0;

/// The per-window egui temp-data slot holding the current toast. Namespaced
/// like `Window::id` (see CLAUDE.md: `Memory::data` is not viewport-keyed).
/// A free function so pane code that holds a `self.tabs` borrow can still post
/// a toast with just the window id.
fn toast_id(window_id: u64) -> egui::Id {
    egui::Id::new(("giest-window", window_id, "toast"))
}

/// Show `msg` as this window's toast, replacing any current one. Callers check
/// the relevant `app-notifications` flag.
fn push_toast(ctx: &egui::Context, window_id: u64, msg: &str) {
    ctx.data_mut(|d| d.insert_temp(toast_id(window_id), (msg.to_string(), std::time::Instant::now())));
    ctx.request_repaint();
}

/// A dialog's button row: right-aligned, primary first (so it lands right-most,
/// the Windows convention), both buttons the same width.
///
/// Returns `(primary_clicked, secondary_clicked)`.
///
/// Shared by both modals so they can't drift apart. The row used to be a plain
/// left-aligned `ui.horizontal` of two default buttons, which gave the
/// destructive answer and the safe one identical weight and let their widths
/// differ by however long their labels were.
fn dialog_buttons(
    ui: &mut egui::Ui,
    chrome: &theme::Chrome,
    primary: &str,
    primary_danger: bool,
    secondary: &str,
) -> (bool, bool) {
    let (fill, ink) = if primary_danger {
        (chrome.danger, chrome.on_danger)
    } else {
        (chrome.accent, chrome.on_accent)
    };
    let size = egui::vec2(96.0, 28.0);
    let mut yes = false;
    let mut no = false;
    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
        ui.spacing_mut().item_spacing.x = 8.0;
        yes = ui
            .add_sized(
                size,
                egui::Button::new(egui::RichText::new(primary).color(ink)).fill(fill),
            )
            .clicked();
        no = ui.add_sized(size, egui::Button::new(secondary)).clicked();
    });
    (yes, no)
}

/// Scrollbar metrics, in logical points.
///
/// The bar is an *overlay*: it floats over the pane's right edge rather than
/// reserving a column, which is what Ghostty's scroller does too (its macOS
/// apprt forces `scrollerStyle = .overlay` even against the OS preference). The
/// knob is drawn narrower than the track it's hit-tested against, and grows on
/// hover. While hidden, only `SCROLLBAR_HOT_W` at the very edge is interactive
/// *and* it senses hover only, so an auto-hidden bar can never steal a click
/// meant for the last column.
const SCROLLBAR_TRACK_W: f32 = 12.0;
const SCROLLBAR_KNOB_W: f32 = 6.0;
const SCROLLBAR_KNOB_W_HOT: f32 = 10.0;
const SCROLLBAR_HOT_W: f32 = 4.0;
const SCROLLBAR_INSET: f32 = 2.0;
/// Gap between the track's outer edge and the pane's.
///
/// Without it the hot knob (10pt, ending flush with the pane) sits *under* the
/// 2pt focused-split border, which is stroked `Inside` on the same rect — the
/// knob's own edge and the focus ring overlap and fight.
const SCROLLBAR_EDGE_INSET: f32 = 2.0;

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
        // Pin the chrome to the terminal's own theme. egui otherwise follows the
        // *OS* preference, which renders a light tab strip and light dialogs
        // over a dark terminal. `Style` lives in egui's process-global
        // `Options`, so this is once-per-process like the atlas above — a later
        // window inherits it, and a config reload in any window restyles all.
        theme::install(&cc.egui_ctx, &config);
        let ppp = cc.egui_ctx.pixels_per_point().max(1.0);
        let px = (config.font_points * ppp).round();
        let (cell_w, cell_h) = render::init(render_state, px, config.text_gamma, &font_spec(&config));

        let (profiles, default_profile) = profiles::detect(config.shell.as_deref());
        // The very first session predates the `Window`, so it resolves
        // `working-directory` directly (nothing can be inherited yet). It is
        // also the one surface `initial-command` applies to (upstream's
        // `app.first`); every later pane uses `command`.
        let initial = config
            .initial_command
            .as_deref()
            .filter(|c| !c.trim().is_empty())
            .map(|c| profiles::for_command(&profiles, c));
        let first = Session::new(
            &cc.egui_ctx,
            &config,
            initial.as_ref().unwrap_or(&profiles[default_profile]),
            config.working_directory.as_deref(),
        )?;

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

        let bg_image = load_bg_image(&config);
        let custom_shaders = load_custom_shaders(&config);

        let app = Self {
            tabs: vec![Tab::leaf(1, first)],
            active_tab: 0,
            next_id: 2,
            cell_w,
            cell_h,
            font_points: config.font_points,
            chrome: theme::chrome(&config),
            config,
            profiles,
            default_profile,
            egui_ctx: cc.egui_ctx.clone(),
            last_window_title: None,
            title_override: None,
            last_layout: Vec::new(),
            renaming: None,
            palette: None,
            keymap,
            pending_keys: Vec::new(),
            key_tables: Vec::new(),
            pointer_hidden: false,
            write_file_seq: 0,
            maximized: false,
            float_on_top: false,
            opaque_override: false,
            geometry_applied: false,
            fullscreen: false,
            quick: false,
            quick_visible: false,
            hwnd,
            transparent_surface,
            tab_drag: None,
            confirm: None,
            config_errors_ignored: None,
            closing: false,
            bell_title: false,
            was_focused: true,
            window_id,
            is_root: true,
            pending_bell: false,
            geom: None,
            requests: Vec::new(),
            progress_dirty: false,
            bg_image,
            custom_shaders,
            shader_epoch: std::time::Instant::now(),
            shader_last_time: 0.0,
            shader_frame: 0,
            undo_state: crate::command::UndoState::default(),
            inspector_rect: None,
            place_geom: None,
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
        let chrome = self.chrome;
        let mut decision: Option<bool> = None;
        let modal = egui::Modal::new(self.id("confirm-close")).show(ctx, |ui| {
            ui.set_width(360.0);
            ui.heading("Close terminal?");
            ui.add_space(8.0);
            ui.label(egui::RichText::new(what.description()).color(chrome.weak_text));
            ui.add_space(16.0);
            // Closing is destructive and irreversible (the panes' scrollback goes
            // with them), so it is the red one — not merely the default.
            let (yes, no) = dialog_buttons(ui, &chrome, "Close", true, "Cancel");
            if yes {
                decision = Some(true);
            }
            if no {
                decision = Some(false);
            }
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

    /// Draw the config-errors dialog (macOS `ConfigurationErrorsView`): the
    /// problems the last load reported, with "Reload Configuration" and
    /// "Ignore". Ignore (and Esc / a backdrop click) records this exact set so
    /// it stays hidden until a reload yields a different one.
    fn render_config_errors(
        &mut self,
        ctx: &egui::Context,
        render_state: Option<&egui_wgpu::RenderState>,
    ) {
        if !self.config_errors_open() {
            return;
        }
        let chrome = self.chrome;
        let diags = &self.config.diagnostics;
        let mut decision: Option<bool> = None;
        let modal = egui::Modal::new(self.id("config-errors")).show(ctx, |ui| {
            ui.set_width(520.0);
            ui.heading("Configuration Errors");
            ui.add_space(8.0);
            ui.label(
                egui::RichText::new(format!(
                    "{} error{} found while loading the configuration. Please review the \
                     errors below and reload your configuration or ignore the erroneous lines.",
                    diags.len(),
                    if diags.len() == 1 { " was" } else { "s were" }
                ))
                .color(chrome.weak_text),
            );
            ui.add_space(8.0);
            egui::Frame::group(ui.style()).show(ui, |ui| {
                egui::ScrollArea::vertical()
                    .max_height(220.0)
                    .id_salt(self.id("config-errors-list"))
                    .show(ui, |ui| {
                        for d in diags {
                            ui.add(
                                egui::Label::new(egui::RichText::new(d).monospace()).wrap(),
                            );
                        }
                    });
            });
            ui.add_space(16.0);
            let (reload, ignore) =
                dialog_buttons(ui, &chrome, "Reload Configuration", false, "Ignore");
            if reload {
                decision = Some(true);
            }
            if ignore {
                decision = Some(false);
            }
        });
        if modal.should_close() {
            decision = Some(false);
        }
        match decision {
            Some(true) => self.reload_config(render_state),
            Some(false) => self.config_errors_ignored = Some(self.config.diagnostics.clone()),
            None => {}
        }
    }

    /// Draw the current toast, if any, bottom-centre of the window, fading out
    /// over its last few hundred milliseconds.
    fn render_toast(&mut self, ctx: &egui::Context) {
        let id = toast_id(self.window_id);
        let Some((msg, at)) = ctx.data(|d| d.get_temp::<(String, std::time::Instant)>(id))
        else {
            return;
        };
        let age = at.elapsed().as_secs_f32();
        if age >= TOAST_SECS {
            ctx.data_mut(|d| d.remove::<(String, std::time::Instant)>(id));
            return;
        }
        let alpha = ((TOAST_SECS - age) / 0.3).clamp(0.0, 1.0);
        let chrome = self.chrome;
        egui::Area::new(self.id("toast"))
            .order(egui::Order::Tooltip)
            .anchor(egui::Align2::CENTER_BOTTOM, egui::vec2(0.0, -24.0))
            .interactable(false)
            .show(ctx, |ui| {
                ui.set_opacity(alpha);
                egui::Frame::popup(ui.style()).show(ui, |ui| {
                    ui.label(egui::RichText::new(msg).color(chrome.weak_text));
                });
            });
        ctx.request_repaint_after(std::time::Duration::from_millis(33));
    }

    /// Draw the clipboard-permission modal, if a pane in the active tab is
    /// waiting on one, and hand the answer back to that pane.
    ///
    /// Unlike the close dialog, **Enter does not accept.** This is a security
    /// prompt: the whole point is that it interrupts a reflex, and a user
    /// hammering Return to get through a build would paste the very thing the
    /// protection exists to catch. Escape (and a backdrop click) still deny,
    /// because denying is always the safe answer.
    fn render_clipboard_confirm(&mut self, ctx: &egui::Context) {
        let Some(leaf_id) = self.clipboard_prompt() else {
            return;
        };
        let active = self.active_tab;
        let Some(req) = self.tabs[active]
            .root
            .payload(leaf_id)
            .and_then(|s: &Session| s.pending_clipboard())
            .cloned()
        else {
            return;
        };

        let chrome = self.chrome;
        let mut decision: Option<bool> = None;
        let modal = egui::Modal::new(self.id("clipboard-confirm")).show(ctx, |ui| {
            ui.set_width(440.0);
            ui.heading(req.title());
            ui.add_space(8.0);
            ui.label(egui::RichText::new(req.detail()).color(chrome.weak_text));
            if let Some(text) = req.preview() {
                ui.add_space(8.0);
                // The preview is attacker-controlled text, so it's sanitized and
                // capped (see `preview_text`) and given a bounded, scrolling box
                // — a long paste must not push the buttons off the dialog.
                egui::Frame::group(ui.style()).show(ui, |ui| {
                    egui::ScrollArea::vertical()
                        .max_height(140.0)
                        .id_salt(self.id("clipboard-preview"))
                        .show(ui, |ui| {
                            ui.add(
                                egui::Label::new(
                                    egui::RichText::new(preview_text(text, PREVIEW_LIMIT))
                                        .monospace(),
                                )
                                .wrap(),
                            );
                        });
                });
            }
            ui.add_space(16.0);
            // "Allow" is styled as the *destructive* action, not the primary
            // one: it is the answer that can leak the clipboard or run a
            // command. Nothing here requests focus — egui activates a focused
            // button on Space/Enter, so focusing "Allow" would quietly restore
            // exactly the reflex this prompt exists to interrupt.
            let (yes, no) = dialog_buttons(ui, &chrome, "Allow", true, "Deny");
            if yes {
                decision = Some(true);
            }
            if no {
                decision = Some(false);
            }
        });
        if modal.should_close() {
            decision = Some(false);
        }
        if let Some(allow) = decision {
            if let Some(s) = self.tabs[active].root.payload_mut(leaf_id) {
                s.resolve_clipboard(allow);
            }
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
            chrome: self.chrome,
            config: self.config.clone(),
            profiles: self.profiles.clone(),
            default_profile: self.default_profile,
            egui_ctx: self.egui_ctx.clone(),
            last_window_title: None,
            title_override: None,
            last_layout: Vec::new(),
            renaming: None,
            palette: None,
            keymap: self.keymap.clone(),
            pending_keys: Vec::new(),
            key_tables: Vec::new(),
            pointer_hidden: false,
            write_file_seq: 0,
            maximized: false,
            float_on_top: false,
            opaque_override: false,
            geometry_applied: false,
            fullscreen: false,
            quick: false,
            quick_visible: false,
            // Only the root viewport has a reachable window handle, so a
            // secondary window gets no DWM backdrop and no taskbar flash.
            hwnd: None,
            transparent_surface: self.transparent_surface,
            tab_drag: None,
            confirm: None,
            // The errors belong to the config, not a window: the window that
            // loaded it already shows (or was told to ignore) them.
            config_errors_ignored: Some(self.config.diagnostics.clone()),
            closing: false,
            bell_title: false,
            was_focused: true,
            window_id,
            is_root: false,
            pending_bell: false,
            geom: None,
            requests: Vec::new(),
            progress_dirty: false,
            // Share the parent's `Arc` rather than re-decoding: the renderer
            // holds one texture for the whole process and identifies it by this
            // pointer (see `load_bg_image`).
            bg_image: self.bg_image.clone(),
            // Shared like `bg_image`, and for the same reason: the renderer
            // keys its pipelines on this `Arc`'s identity, and there is one
            // renderer for every window.
            custom_shaders: self.custom_shaders.clone(),
            shader_epoch: std::time::Instant::now(),
            shader_last_time: 0.0,
            shader_frame: 0,
            undo_state: crate::command::UndoState::default(),
            inspector_rect: None,
            place_geom: None,
        })
    }

    /// Snapshot this window's layout for `window-save-state`.
    fn capture_state(&self) -> crate::state::SavedWindow {
        crate::state::SavedWindow {
            tabs: self
                .tabs
                .iter()
                .map(|t| crate::state::SavedTab {
                    name: t.name.clone(),
                    // The *zoom* is deliberately not saved: it is a transient
                    // view of a layout, and restoring one would hide panes the
                    // user would then have to discover.
                    tree: capture_node(&t.root, t.focus),
                })
                .collect(),
            active_tab: self.active_tab,
        }
    }

    /// Rebuild `saved`'s tabs into this window, spawning one shell per leaf.
    ///
    /// Replaces whatever the window already had — for the first window that is
    /// the one session [`Window::first`] just opened, which is dropped here. The
    /// alternative (deciding before the window exists) would mean doing the
    /// once-per-process setup somewhere else entirely; one short-lived shell is
    /// the cheaper trade.
    ///
    /// A leaf whose shell fails to spawn collapses out of the tree rather than
    /// taking the tab with it, and a window that ends up with no tabs at all is
    /// left untouched — the user gets their original window, never none.
    fn restore(&mut self, saved: &crate::state::SavedWindow) {
        let mut next = 1;
        let mut tabs = Vec::new();
        for st in &saved.tabs {
            let mut focus = None;
            let Some(root) = self.build_saved(&st.tree, &mut next, &mut focus) else {
                continue;
            };
            let focus = focus.unwrap_or_else(|| root.first_leaf_id());
            let id = next;
            next += 1;
            tabs.push(Tab {
                id,
                root,
                focus,
                name: st.name.clone(),
                color: None,
                zoomed: None,
            });
        }
        if tabs.is_empty() {
            return;
        }
        self.active_tab = saved.active_tab.min(tabs.len() - 1);
        self.tabs = tabs;
        self.next_id = next;
    }

    /// Spawn the sessions for one saved subtree. `next` is a plain counter
    /// rather than [`Window::alloc_id`] because the walk holds `&self` for
    /// `spawn_session`; the caller stores it back as `next_id`.
    fn build_saved(
        &self,
        node: &crate::state::SavedNode,
        next: &mut u64,
        focus: &mut Option<u64>,
    ) -> Option<Node<Session>> {
        match node {
            crate::state::SavedNode::Leaf { cwd, focused } => {
                let cwd = cwd.as_ref().map(std::path::PathBuf::from);
                // A directory that no longer exists would fail the spawn, which
                // would silently cost the user a pane — fall back to the default.
                let cwd = cwd.filter(|p| p.is_dir());
                let session = self.spawn_session(self.default_profile, cwd.as_deref())?;
                let id = *next;
                *next += 1;
                if *focused {
                    *focus = Some(id);
                }
                Some(Node::Leaf {
                    id,
                    payload: session,
                })
            }
            crate::state::SavedNode::Split {
                vertical,
                first,
                second,
            } => {
                let first = self.build_saved(first, next, focus);
                let second = self.build_saved(second, next, focus);
                match (first, second) {
                    (Some(a), Some(b)) => Some(Node::Split {
                        vertical: *vertical,
                        first: Box::new(a),
                        second: Box::new(b),
                    }),
                    (Some(n), None) | (None, Some(n)) => Some(n),
                    (None, None) => None,
                }
            }
        }
    }

    /// The `ViewportBuilder` for this window as a child viewport.
    ///
    /// Built once and cloned verbatim every pass. Several `ViewportBuilder`
    /// fields force a full window recreation when patched, and eframe's recreate
    /// path clears **every** viewport's surface (not just this one) — a visible
    /// hitch on the root too. Anything dynamic goes through `ViewportCommand`.
    fn child_builder(&self) -> egui::ViewportBuilder {
        if self.quick {
            return self.quick_builder();
        }
        // `icon::apply` hands over a process-wide shared `Arc`, which this call
        // site *requires*: it runs on every pass, and `ViewportBuilder::patch`
        // tests the icon with `Arc::ptr_eq`. A per-call `Arc` would read as a
        // new icon every frame and re-set it on every child window forever.
        crate::icon::apply(
            egui::ViewportBuilder::default()
                .with_title("giest")
                .with_inner_size([960.0, 600.0])
                // NOT optional. The vendored egui-winit patch reads `transparent` to
                // set WS_EX_NOREDIRECTIONBITMAP at creation, and children go through
                // the same `create_window`. Omit it and a secondary window renders as
                // the solid grey wash CLAUDE.md documents.
                .with_transparent(self.transparent_surface),
        )
    }

    /// The `ViewportBuilder` for the quick terminal: undecorated, above other
    /// windows, and parked against the configured screen edge.
    ///
    /// The geometry is computed in **physical pixels** by `quickterm::frame`
    /// (that is what the OS work area is in) and divided by `pixels_per_point`
    /// here, because egui's viewport geometry is in points. Passing physical
    /// pixels through unchanged is the same bug `window-position-*` had: at 125%
    /// scaling the window lands a quarter of the way off.
    ///
    /// Falls back to the plain child geometry if the work area can't be read —
    /// an unplaced window is recoverable; no window at all is not.
    fn quick_builder(&self) -> egui::ViewportBuilder {
        let base = crate::icon::apply(
            egui::ViewportBuilder::default()
                .with_title("giest quick terminal")
                // No titlebar: a dropdown terminal is chrome the user never
                // drags or minimizes, and the strip would eat a row of cells.
                .with_decorations(false)
                .with_always_on_top()
                // It must not steal a taskbar button from the real windows.
                .with_taskbar(false)
                // NOT optional — see `child_builder`.
                .with_transparent(self.transparent_surface),
        );
        let Some(work) = crate::quickterm::work_area() else {
            return base.with_inner_size([960.0, 400.0]);
        };
        let f = crate::quickterm::frame(
            self.config.quick_terminal_position,
            &self.config.quick_terminal_size,
            work,
        );
        let ppp = self.egui_ctx.pixels_per_point().max(1.0);
        base.with_inner_size([f.w / ppp, f.h / ppp])
            .with_position([f.x / ppp, f.y / ppp])
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
        // One cwd decision, in priority order: an inherited directory (when the
        // `*-inherit-working-directory` key for this surface allows it and the
        // shell reported one), else `working-directory`, else the process's own.
        // Kept here rather than at each call site so the three inherit paths and
        // the state-restore path can't drift — a restored directory that no
        // longer exists now lands on `working-directory` too.
        let cwd = cwd.or(self.config.working_directory.as_deref());
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
            let at = new_tab_index(
                self.config.new_tab_position,
                self.active_tab,
                self.tabs.len(),
            );
            self.tabs.insert(at, Tab::leaf(id, s));
            self.active_tab = at;
            // Ghostty registers an undo for `new_tab` too: the reverse of making
            // a tab is closing it. It goes through the same `RemoveTabs` path a
            // redo would, so "close this tab" has one implementation.
            self.requests.push(AppRequest::Record(UndoOp::RemoveTabs {
                window: self.window_id,
                ids: vec![id],
            }));
            // An insert *before* the end shifts every later tab, so anything
            // holding a tab index is now pointing at the wrong tab. Every other
            // mutation point in this file clears these for the same reason; a
            // mid-list insert is a new one, introduced by
            // `window-new-tab-position = current`.
            self.renaming = None;
            self.tab_drag = None;
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
            let window = self.window_id;
            let tab = &mut self.tabs[self.active_tab];
            let focus = tab.focus;
            let tab_id = tab.id;
            tab.root.split_leaf(focus, vertical, id, s);
            tab.focus = id;
            // A new split changes the layout, so any zoom is no longer meaningful.
            tab.zoomed = None;
            self.requests.push(AppRequest::Record(UndoOp::RemovePane {
                window,
                tab: tab_id,
                leaf: id,
            }));
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
        self.palette.is_some()
            || self.confirm.is_some()
            || self.focused_search_open()
            || self.clipboard_prompt().is_some()
            || self.config_errors_open()
    }

    /// Whether the config-errors dialog is up: the loaded config had problems
    /// and the user hasn't ignored this exact set.
    fn config_errors_open(&self) -> bool {
        !self.config.diagnostics.is_empty()
            && self.config_errors_ignored.as_ref() != Some(&self.config.diagnostics)
    }

    /// The active tab's first pane waiting on a clipboard decision, if any.
    ///
    /// Scanned per frame rather than cached on the app: the request lives on the
    /// `Session` that raised it, so there is no pane index to go stale when a
    /// split closes or a tab is reordered — the prompt simply stops being found.
    /// A request from a *background* tab stays pending until you switch to that
    /// tab, which is the safe way round (nothing happens until you say so).
    fn clipboard_prompt(&self) -> Option<u64> {
        let tab = self.tabs.get(self.active_tab)?;
        tab.root
            .find_leaf(&mut |s: &Session| s.pending_clipboard().is_some())
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
                self.requests.push(AppRequest::CloseWindow { undoable: true });
            }
        }
    }

    /// Close the focused pane; closing the last pane closes the tab, and the
    /// last tab closes the window.
    fn close_focused(&mut self) {
        let window = self.window_id;
        let tab = &mut self.tabs[self.active_tab];
        if tab.leaf_count() > 1 {
            let (focus, id) = (tab.focus, tab.id);
            let root = std::mem::replace(&mut tab.root, Node::Empty);
            // `detach_leaf`, not `remove_leaf`: the pane is *moved* into the undo
            // entry with its shell still running, so an undo puts the same
            // terminal back rather than a fresh one.
            let (rest, taken) = root.detach_leaf(focus);
            tab.root = rest.unwrap_or(Node::Empty);
            tab.focus = tab.root.first_leaf_id();
            // Closing a pane changes the layout; drop any (now stale) zoom.
            tab.zoomed = None;
            if let Some((slot, node)) = taken {
                self.requests.push(AppRequest::Record(UndoOp::RestorePane {
                    window,
                    tab: id,
                    slot,
                    node: Box::new(node),
                }));
            }
        } else if self.tabs.len() > 1 {
            // The last pane of a tab *is* the tab; one path, one undo entry.
            self.close_tab(self.active_tab);
        } else {
            self.requests.push(AppRequest::CloseWindow { undoable: true });
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

    /// Record `tabs` (index → tab, ascending) as restorable by `undo`, with the
    /// selection that was active before the close.
    ///
    /// The tabs are **moved** in, shells and all; the entry keeps them running
    /// until it expires. `active` is the pre-close index, which is also the
    /// right post-restore one: re-inserting at the same indices reproduces the
    /// original order, so the tab the user was on is back where it was.
    fn record_closed_tabs(&mut self, active: usize, tabs: ClosedTabs<Session>) {
        if tabs.is_empty() {
            return;
        }
        self.requests.push(AppRequest::Record(UndoOp::RestoreTabs {
            window: self.window_id,
            tabs,
            active,
        }));
    }

    /// Close tab `idx`; closing the last tab closes the window.
    fn close_tab(&mut self, idx: usize) {
        if idx >= self.tabs.len() {
            return;
        }
        // The last tab *is* the window. Hand the close to the window path with
        // the tab still in place, so the undo entry is one restorable window
        // rather than a tab pointing at a window that no longer exists.
        if self.tabs.len() == 1 {
            self.requests.push(AppRequest::CloseWindow { undoable: true });
            return;
        }
        let active = self.active_tab;
        let tab = self.tabs.remove(idx);
        if self.active_tab > idx {
            self.active_tab -= 1;
        }
        self.active_tab = self.active_tab.min(self.tabs.len() - 1);
        self.record_closed_tabs(active, vec![(idx, tab)]);
    }

    /// Close every tab except `keep`, leaving it focused (Ghostty's "Close Other
    /// Tabs"). A no-op if `keep` is out of range; always leaves one tab.
    fn close_other_tabs(&mut self, keep: usize) {
        let was_active = self.active_tab;
        let tabs = std::mem::take(&mut self.tabs);
        let (tabs, active, closed) = keep_only_tab(tabs, keep, self.active_tab);
        self.tabs = tabs;
        self.active_tab = active;
        self.record_closed_tabs(was_active, closed);
    }

    /// Close every tab to the right of `idx` (Ghostty's "Close Tabs to the
    /// Right"), clamping the active tab into the survivors.
    fn close_tabs_to_right(&mut self, idx: usize) {
        let was_active = self.active_tab;
        let tabs = std::mem::take(&mut self.tabs);
        let (tabs, active, closed) = truncate_tabs_to_right(tabs, idx, self.active_tab);
        self.tabs = tabs;
        self.active_tab = active;
        self.record_closed_tabs(was_active, closed);
    }

    /// Remove panes whose shell has exited; drop tabs that become empty and
    /// close the window when the last tab is gone. Returns `false` if the
    /// window is closing (caller should skip rendering this frame).
    fn reap_dead(&mut self) -> bool {
        let before = self.tabs.len();
        let tabs = std::mem::take(&mut self.tabs);
        let (survivors, active) =
            reap_tabs(tabs, self.active_tab, &mut |s: &Session| s.should_reap());
        self.tabs = survivors;
        if self.tabs.is_empty() {
            self.requests.push(AppRequest::CloseWindow { undoable: false });
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
        // Mid-composition every key belongs to the IME; a chord reaching us here
        // must not fire a binding the user was typing *through*.
        if self.focused_session().is_some_and(|s| s.preedit().is_some()) {
            return;
        }
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
            // Sequence state machine. `pending_keys` holds the leaders pressed
            // so far; a plain binding is just a sequence that completes on the
            // first key, so both share this path.
            self.pending_keys.push(chord);
            // Resolved against the active key-table stack, exactly as
            // `decide_key` does — the two must agree about which binding a key
            // resolves to, or a table's key is swallowed and inert, or reaches
            // the shell *and* runs.
            let stack = self.key_tables.clone();
            match self.keymap.lookup_seq_in(&stack, &self.pending_keys) {
                crate::keybind::Lookup::Action(actions) => {
                    // A `performable:` binding only counts while its action can
                    // act; otherwise the key was already left to the shell by
                    // `decide_key`, and running it here would do both. For a
                    // `chain=` binding upstream ORs the results
                    // (`performed = performed or v`), so the chain counts as
                    // performed if **any** of its actions can act.
                    let performable = self.keymap.is_performable_in(&stack, &self.pending_keys);
                    // `all:` is always treated as performed upstream, since it
                    // isn't tied to one surface — so it skips the performable
                    // check entirely, matching `decide_key`'s short-circuit.
                    let all = self.keymap.is_all(&stack, &self.pending_keys);
                    self.pending_keys.clear();
                    let ctx_perform = self.perform_ctx();
                    let can = actions
                        .iter()
                        .any(|a| crate::command::can_perform(a, ctx_perform));
                    if all {
                        self.pop_one_shot_table();
                        for action in actions {
                            self.execute_action_all(ctx, action);
                        }
                        continue;
                    }
                    if !performable || can {
                        // A one-shot table pops as soon as one of its bindings
                        // runs — once per *binding*, not per chained action, and
                        // before them, so an action that activates another table
                        // isn't popped by its predecessor's one-shot flag.
                        self.pop_one_shot_table();
                        for action in actions {
                            self.execute_action(ctx, None, action);
                        }
                    }
                }
                // A leader: swallow and wait. `decide_key` keeps the shell from
                // seeing these too, so nothing leaks mid-sequence.
                crate::keybind::Lookup::Pending => {}
                crate::keybind::Lookup::None => {
                    // A dead end. Ghostty flushes the buffered keys *and* the
                    // one that broke the sequence to the terminal, so a mistyped
                    // `ctrl+a x` still delivers both to the shell rather than
                    // silently eating them.
                    //
                    // Exception, and upstream's: if a `catch_all` binding would
                    // `ignore` the breaking key, the *whole* sequence is dropped
                    // and nothing is sent. That is what makes a modal table with
                    // `catch_all=ignore` actually silent — otherwise a mistyped
                    // sequence inside it would leak its keys to the program.
                    let swallow_all = matches!(
                        self.keymap.lookup_in(&stack, &chord),
                        Some(crate::command::Action::Noop(ref n)) if &**n == "ignore"
                    );
                    let flush = std::mem::take(&mut self.pending_keys);
                    if swallow_all {
                        continue;
                    }
                    if flush.len() > 1
                        && let Some(s) = self.focused_session_mut()
                    {
                        s.send_chords(&flush);
                    }
                }
            }
        }
    }

    /// Move the active tab by `delta` positions (Ghostty `move_tab:N`).
    ///
    /// Clamped rather than wrapped: Ghostty clamps, and a tab silently
    /// teleporting from one end to the other is disorienting when you're just
    /// holding the key down.
    fn move_tab_by(&mut self, delta: i8) {
        if self.tabs.len() < 2 || delta == 0 {
            return;
        }
        let from = self.active_tab;
        let to = (from as isize + delta as isize).clamp(0, self.tabs.len() as isize - 1) as usize;
        if to != from {
            // Delegates to the drag-reorder path, which already handles the
            // insertion-slot arithmetic and clears the stale `renaming` index.
            self.move_tab(from, to);
        }
    }

    /// Capture the focused pane's text to a temp file and act on the **path**.
    ///
    /// The path, not the contents — that is Ghostty's design and the point of
    /// the feature: it gets a large scrollback out of the terminal and into a
    /// real tool, so `paste` hands the shell a filename to pipe somewhere and
    /// `open` gives it to the OS.
    fn write_terminal_file(
        &mut self,
        scope: crate::writefile::WriteScope,
        what: crate::writefile::WriteAction,
    ) {
        use crate::writefile::WriteAction;

        let Some(text) = self
            .focused_session_mut()
            .and_then(|s| s.capture_text(scope))
        else {
            // Nothing to write. `write_selection_file` with no selection is a
            // documented no-op upstream, and an empty screen is the same case.
            return;
        };
        // A counter, not a clock: two captures in the same second would collide
        // on a timestamp alone.
        self.write_file_seq = self.write_file_seq.wrapping_add(1);
        let path = match crate::writefile::write(
            &std::env::temp_dir(),
            scope,
            self.write_file_seq,
            &text,
        ) {
            Ok(p) => p,
            Err(e) => {
                eprintln!("giest: {e:#}");
                return;
            }
        };
        let display = path.display().to_string();
        match what {
            WriteAction::Copy => self.egui_ctx.copy_text(display),
            // Through `paste_str`, so `clipboard-paste-protection` applies —
            // the one gate every paste path must go through. A temp path is
            // always safe, but routing around the gate is how the next path
            // that isn't ends up bypassing it too.
            WriteAction::Paste => {
                if let Some(s) = self.focused_session_mut() {
                    s.paste_str(&display);
                }
            }
            WriteAction::Open => open_url(&display),
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
    /// The live state a `performable:` binding is judged against. **One
    /// builder** for both gates: the one deciding whether the shell sees a key
    /// and the one running the action.
    fn perform_ctx(&self) -> crate::command::PerformCtx<'_> {
        crate::command::PerformCtx {
            has_selection: self
                .focused_session()
                .is_some_and(crate::session::Session::has_selection),
            keymap: Some(&self.keymap),
            tables: &self.key_tables,
            undo: self.undo_state,
            search_active: self.focused_search_open(),
        }
    }

    /// Run `action` for an `all:`-flagged binding.
    ///
    /// App-scoped actions run **once** (upstream does the same — they aren't
    /// repeated per surface). Surface-scoped actions that giest can source to a
    /// pane run on **every pane in every tab of this window**, including panes
    /// in background tabs, which is what makes `all:` a broadcast-input feature.
    ///
    /// Two divergences from upstream, both structural and both recorded in
    /// GAP.md: it stops at this window (upstream iterates every surface in the
    /// app; `handle_shortcuts` is a `Window` method and cannot reach its
    /// siblings), and the window-structural surface actions — new/close/goto
    /// tab, splits, focus moves — run once rather than once per pane, because
    /// `execute_action` acts on the *focused* pane and has no target parameter.
    fn execute_action_all(&mut self, ctx: &egui::Context, action: Action) {
        if !action.broadcasts_to_panes() {
            self.execute_action(ctx, None, action);
            return;
        }
        // Broadcast by temporarily focusing each pane, so every action reuses
        // the single implementation in `execute_action` rather than growing a
        // second one that could drift. Focus is restored afterwards.
        let saved_tab = self.active_tab;
        let mut targets: Vec<(usize, u64)> = Vec::new();
        for (i, tab) in self.tabs.iter().enumerate() {
            let mut ids = Vec::new();
            tab.root.leaf_ids(&mut ids);
            targets.extend(ids.into_iter().map(|id| (i, id)));
        }
        let saved_focus: Vec<u64> = self.tabs.iter().map(|t| t.focus).collect();
        for (tab_i, leaf) in targets {
            self.active_tab = tab_i;
            if let Some(t) = self.tabs.get_mut(tab_i) {
                t.focus = leaf;
            }
            self.execute_action(ctx, None, action.clone());
        }
        self.active_tab = saved_tab.min(self.tabs.len().saturating_sub(1));
        for (t, f) in self.tabs.iter_mut().zip(saved_focus) {
            t.focus = f;
        }
    }

    /// Pop a one-shot key table now that one of its bindings has run.
    fn pop_one_shot_table(&mut self) {
        if self.key_tables.last().is_some_and(|t| t.once) {
            self.key_tables.pop();
        }
    }

    fn focused_session(&self) -> Option<&Session> {
        let tab = self.tabs.get(self.active_tab)?;
        tab.root.payload(tab.focus)
    }

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
        // A reload can delete a key table whose name is still on the stack, and
        // a stale name silently changing which bindings resolve is the worst
        // available outcome — so the stack is dropped rather than filtered.
        // Any half-finished key sequence goes with it for the same reason.
        self.key_tables.clear();
        self.pending_keys.clear();
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
        // Re-decode the background image (a no-op when the path is unchanged —
        // `load_bg_image` caches by path), so editing the key applies live like
        // the colors do. Only the *surface's* transparency is startup-only.
        self.config = cfg;
        if self.config.app_notifications.config_reload {
            push_toast(&self.egui_ctx, self.window_id, "Reloaded the configuration");
        }
        // Re-derive the chrome and re-install the egui `Style`, so editing
        // `background`/`foreground`/`palette`/`window-theme` restyles the tab
        // strip, palette, overlays and dialogs live like the terminal colors do.
        // `Style` is process-global, so this restyles every window at once.
        self.chrome = theme::chrome(&self.config);
        theme::install(&self.egui_ctx, &self.config);
        self.bg_image = load_bg_image(&self.config);
        // Rebuilt wholesale: the renderer keys its pipelines on this `Arc`.
        self.custom_shaders = load_custom_shaders(&self.config);
        self.shader_epoch = std::time::Instant::now();
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
            // Bound, and deliberately does nothing (see `Action::Noop`).
            Action::Noop(_) => {}
            // Key tables. `can_perform` has already rejected the no-op cases
            // (an unknown table, or one that is already innermost), so these
            // only run when they will actually change the stack.
            Action::ActivateKeyTable(ref name) | Action::ActivateKeyTableOnce(ref name) => {
                // Guarded here as well as at the performable gate: upstream's
                // "no effect" for an unknown or already-innermost table holds
                // whether or not the binding was flagged `performable:`.
                let once = matches!(action, Action::ActivateKeyTableOnce(_));
                if crate::command::can_perform(&action, self.perform_ctx()) {
                    self.key_tables.push(crate::keybind::TableEntry {
                        name: name.to_string(),
                        once,
                    });
                }
            }
            Action::DeactivateKeyTable => {
                self.key_tables.pop();
            }
            Action::DeactivateAllKeyTables => self.key_tables.clear(),
            // `text:` decodes its escapes at send time, like upstream — a bad
            // escape logs and sends nothing rather than emitting the payload
            // literally. `csi:`/`esc:` payloads are raw and simply get their
            // prefix; both also scroll to the bottom, since the user just
            // "typed" something.
            Action::SendText(ref s) => {
                let decoded = crate::command::decode_escapes(s);
                match decoded {
                    Some(text) => {
                        if let Some(sess) = self.focused_session_mut() {
                            sess.send_text(&text);
                        }
                    }
                    None => eprintln!("giest: invalid escape sequence in 'text:{s}'"),
                }
            }
            Action::SendCsi(ref s) => {
                let seq = format!("\x1b[{s}");
                if let Some(sess) = self.focused_session_mut() {
                    sess.send_text(&seq);
                }
            }
            Action::SendEsc(ref s) => {
                let seq = format!("\x1b{s}");
                if let Some(sess) = self.focused_session_mut() {
                    sess.send_text(&seq);
                }
            }
            Action::SetTabTitle(ref s) => {
                let name = (!s.is_empty()).then(|| s.to_string());
                let i = self.active_tab;
                if let Some(t) = self.tabs.get_mut(i) {
                    t.name = name;
                }
            }
            Action::SetWindowTitle(ref s) => {
                self.title_override = (!s.is_empty()).then(|| s.to_string());
            }
            Action::SetSurfaceTitle(ref s) => {
                let s = s.clone();
                if let Some(sess) = self.focused_session_mut() {
                    sess.set_title_override(&s);
                }
            }
            Action::AdjustSelection(dir) => {
                let cell_h = self.cell_h;
                if let Some(s) = self.focused_session_mut() {
                    s.adjust_selection(dir, cell_h);
                }
            }
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
            Action::WriteFile(scope, what) => self.write_terminal_file(scope, what),
            Action::ClearScreen => {
                if let Some(s) = self.focused_session_mut() {
                    s.clear_screen();
                }
            }
            Action::CopyTitle => {
                if let Some(title) = self
                    .tabs
                    .get(self.active_tab)
                    .and_then(|t| t.focused_payload().title())
                {
                    ctx.copy_text(title);
                }
            }
            Action::ToggleReadonly => {
                // The badge drawn over the pane is the feedback; this used to
                // only print to stderr, which nobody running a GUI ever sees —
                // so toggling read-only was indistinguishable from a hung shell.
                if let Some(s) = self.focused_session_mut() {
                    s.toggle_readonly();
                }
            }
            Action::MoveTab(delta) => self.move_tab_by(delta),
            Action::SetFontSize(pt) => {
                if let Some(rs) = render_state {
                    let ppp = ctx.pixels_per_point().max(1.0);
                    self.set_font_points(rs, f32::from(pt), ppp);
                }
            }
            Action::ScrollLines(n) => {
                let ch = self.cell_h;
                if let Some(s) = self.focused_session_mut() {
                    s.scroll_lines(isize::from(n), ch);
                }
            }
            Action::ScrollPageFraction(hundredths) => {
                let ch = self.cell_h;
                if let Some(s) = self.focused_session_mut() {
                    // `page_lines` is the same rows-1 the page-up/down actions
                    // use, so a fraction of 1.0 matches `scroll_page_down`.
                    let lines = f32::from(hundredths) / 100.0 * s.page_lines() as f32;
                    s.scroll_lines(lines.round() as isize, ch);
                }
            }
            // Both are app-scoped: the stack spans every window, because a
            // *window* close is one of the things it can take back.
            Action::Undo => self.requests.push(AppRequest::Undo),
            Action::Redo => self.requests.push(AppRequest::Redo),
            Action::Quit => self.requests.push(AppRequest::CloseWindow { undoable: true }),
            Action::PromptTabTitle => {
                let i = self.active_tab;
                if let Some(t) = self.tabs.get(i) {
                    self.renaming = Some((i, t.name.clone().unwrap_or_default()));
                }
            }
            Action::ToggleMaximize => {
                self.maximized = !self.maximized;
                ctx.send_viewport_cmd(egui::ViewportCommand::Maximized(self.maximized));
            }
            Action::ToggleFloatOnTop => {
                self.float_on_top = !self.float_on_top;
                ctx.send_viewport_cmd(egui::ViewportCommand::WindowLevel(if self.float_on_top {
                    egui::WindowLevel::AlwaysOnTop
                } else {
                    egui::WindowLevel::Normal
                }));
            }
            // Flip between the configured opacity and fully opaque. Only
            // meaningful on a surface that was *created* transparent — see the
            // startup restriction in `main.rs` — so say so rather than appearing
            // to do nothing.
            Action::ToggleBackgroundOpacity => {
                if self.transparent_surface {
                    self.opaque_override = !self.opaque_override;
                } else {
                    eprintln!(
                        "giest: toggle_background_opacity needs a transparent window, which is \
                         set up at startup — set background-opacity below 1 and restart."
                    );
                }
            }
            Action::ToggleMouseReporting => {
                if let Some(s) = self.focused_session_mut() {
                    let on = s.toggle_mouse_reporting();
                    eprintln!(
                        "giest: mouse reporting {}",
                        if on { "enabled" } else { "disabled" }
                    );
                }
            }
            // Per *pane*, like upstream's per-surface inspector: the log it
            // shows is that pane's own, and focusing another shows theirs (or
            // nothing). Deliberately **not** added to `modal_open` or the
            // `palette_open` gate — a keyboard log you cannot type into, over a
            // program you cannot watch redraw, would be useless.
            Action::Inspector(mode) => {
                if let Some(s) = self.focused_session_mut() {
                    s.set_inspector(mode);
                }
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
            // Upstream's split of the toggle. `start_search` on an already-open
            // search is deliberately a no-op rather than a recapture: it is what
            // a user binds when they also bind `end_search`, and re-opening
            // would throw away the query they are in the middle of typing.
            Action::StartSearch => {
                if let Some(s) = self.focused_session_mut()
                    && !s.search_active()
                {
                    s.open_search();
                }
            }
            Action::EndSearch => {
                if let Some(s) = self.focused_session_mut() {
                    s.close_search();
                }
            }
            Action::NavigateSearch(next) => {
                let cell_h = self.cell_h;
                if let Some(s) = self.focused_session_mut() {
                    s.step_search(next, cell_h);
                }
            }
            // Ghostty opens the search with the selection as its needle. The
            // selection is read through the same `selection_text` the clipboard
            // uses, so `clipboard-trim-trailing-spaces` applies here too — a
            // needle with a trailing space the user cannot see would match
            // nothing and look like a broken feature.
            Action::SearchSelection => {
                let cell_h = self.cell_h;
                if let Some(s) = self.focused_session_mut() {
                    let Some(needle) = s.selection_text().filter(|t| !t.is_empty()) else {
                        return;
                    };
                    // One line only: the overlay is a substring search over
                    // single rows, so a multi-line selection could never match.
                    let needle = needle.lines().next().unwrap_or_default().to_string();
                    if !s.search_active() {
                        s.open_search();
                    }
                    s.set_search_query(needle, cell_h);
                }
            }
            // An empty needle *stops* the search without hiding the overlay —
            // upstream's rule, and the reason this isn't folded into
            // `end_search`.
            Action::Search(text) => {
                let cell_h = self.cell_h;
                if let Some(s) = self.focused_session_mut() {
                    if text.is_empty() {
                        if s.search_active() {
                            s.set_search_query(String::new(), cell_h);
                        }
                    } else {
                        if !s.search_active() {
                            s.open_search();
                        }
                        s.set_search_query(text.to_string(), cell_h);
                    }
                }
            }
            Action::SplitRight => self.split(true),
            Action::SplitDown => self.split(false),
            Action::ToggleSplitZoom => self.toggle_split_zoom(),
            Action::ClosePane => self.request_close(PendingClose::Pane),
            Action::ToggleFullscreen => self.toggle_fullscreen(ctx),
            // App-level: there is one quick terminal for the process, and it may
            // not exist yet.
            Action::ToggleQuickTerminal => {
                self.requests.push(AppRequest::ToggleQuickTerminal)
            }
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
                let toast = self.config.app_notifications.clipboard_copy;
                let win_id = self.window_id;
                if let Some(s) = self.focused_session_mut() {
                    if let Some(text) = s.selection_text() {
                        ctx.copy_text(text);
                        s.clear_selection();
                        if toast {
                            push_toast(ctx, win_id, "Copied to clipboard");
                        }
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
            Action::ScrollToRow(row) => {
                let ch = self.cell_h;
                if let Some(s) = self.focused_session_mut() {
                    // Eased, unlike the scrollbar's own drag: a keybind is a
                    // discrete jump, so animating it reads as intentional.
                    s.scroll_to_row(row as f32, ch, false);
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

        let chrome = self.chrome;
        // Ghostty's palette is a fixed 700pt dialog (`command-palette.blp`'s
        // `content-width: 700`), not a fraction of the window.
        let width = 700.0_f32.min(screen.width() - 80.0);
        egui::Area::new(self.id("palette"))
            .order(egui::Order::Foreground)
            .anchor(egui::Align2::CENTER_TOP, egui::vec2(0.0, screen.height() * 0.12))
            .show(ctx, |ui| {
                egui::Frame::popup(ui.style())
                    .inner_margin(egui::Margin::same(10))
                    .show(ui, |ui| {
                        ui.set_width(width);

                        // Search box: auto-focus once; reset the selection on edit.
                        // Frameless, so it reads as a search *line* above the
                        // separator rather than a boxed input inside a box.
                        let resp = ui.add(
                            egui::TextEdit::singleline(&mut state.query)
                                .hint_text("Execute a command…")
                                .desired_width(f32::INFINITY)
                                .frame(egui::Frame::NONE)
                                .font(egui::FontId::proportional(18.0))
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
                        // Ghostty's list is `min-content-height: 300` with
                        // separators between rows, each row a title over a
                        // monospace action key with the shortcut on the right.
                        egui::ScrollArea::vertical()
                            .min_scrolled_height(300.0)
                            .max_height(screen.height() * 0.6)
                            .show(ui, |ui| {
                                let title_font = egui::FontId::proportional(15.0);
                                let sub_font = egui::FontId::monospace(12.0);
                                for (row, &cmd_idx) in filtered.iter().enumerate() {
                                    let selected = row == state.selected;
                                    let (rect, resp) = ui.allocate_exact_size(
                                        egui::vec2(ui.available_width(), 44.0),
                                        egui::Sense::click(),
                                    );
                                    if resp.hovered() && pointer_moved {
                                        state.selected = row;
                                    }
                                    if selected {
                                        ui.painter().rect_filled(
                                            rect,
                                            egui::CornerRadius::same(theme::RADIUS_MD),
                                            chrome.accent,
                                        );
                                    } else if resp.hovered() {
                                        ui.painter().rect_filled(
                                            rect,
                                            egui::CornerRadius::same(theme::RADIUS_MD),
                                            chrome.fill_hover,
                                        );
                                    } else if row > 0 {
                                        ui.painter().hline(
                                            (rect.left() + 12.0)..=(rect.right() - 12.0),
                                            rect.top(),
                                            egui::Stroke::new(1.0_f32, chrome.divider),
                                        );
                                    }
                                    let cmd = &state.catalog[cmd_idx];
                                    let (ink, dim) = if selected {
                                        (chrome.on_accent, chrome.on_accent)
                                    } else {
                                        (chrome.text, chrome.weak_text)
                                    };

                                    // Title, with the fuzzy-matched characters
                                    // picked out. `fuzzy_match_indices` has
                                    // always returned these; nothing in the UI
                                    // used them until now.
                                    let matched = command::fuzzy_match_indices(
                                        &state.query,
                                        &cmd.title,
                                    )
                                    .map(|(_, ix)| ix)
                                    .unwrap_or_default();
                                    let job = highlight_job(
                                        &cmd.title,
                                        &matched,
                                        title_font.clone(),
                                        ink,
                                        if selected { chrome.on_accent } else { chrome.accent },
                                    );
                                    let galley = ui.painter().layout_job(job);
                                    ui.painter().galley(
                                        egui::pos2(rect.left() + 12.0, rect.top() + 6.0),
                                        galley,
                                        ink,
                                    );

                                    // The action key, monospace and dim.
                                    ui.painter().text(
                                        egui::pos2(rect.left() + 12.0, rect.bottom() - 8.0),
                                        egui::Align2::LEFT_BOTTOM,
                                        cmd.action.name(),
                                        sub_font.clone(),
                                        dim,
                                    );

                                    // Shortcut, right-aligned as one chip per
                                    // key (Ghostty uses a `Gtk.ShortcutLabel`).
                                    if let Some(kb) = &cmd.keybind {
                                        let mut x = rect.right() - 12.0;
                                        for key in kb.rsplit('+') {
                                            let g = ui.painter().layout_no_wrap(
                                                key.to_owned(),
                                                sub_font.clone(),
                                                dim,
                                            );
                                            let chip = egui::Rect::from_min_size(
                                                egui::pos2(
                                                    x - g.size().x - 12.0,
                                                    rect.center().y - g.size().y * 0.5 - 3.0,
                                                ),
                                                g.size() + egui::vec2(12.0, 6.0),
                                            );
                                            if !selected {
                                                ui.painter().rect_filled(
                                                    chip,
                                                    egui::CornerRadius::same(theme::RADIUS_SM),
                                                    chrome.fill_weak,
                                                );
                                            }
                                            ui.painter().galley(
                                                chip.min + egui::vec2(6.0, 3.0),
                                                g,
                                                dim,
                                            );
                                            x = chip.left() - 4.0;
                                        }
                                    }
                                    if selected && (up || down) {
                                        resp.scroll_to_me(Some(egui::Align::Center));
                                    }
                                    if resp.clicked() {
                                        chosen = Some(cmd.action.clone());
                                        keep_open = false;
                                    }
                                }
                            });

                        if entered {
                            if let Some(&idx) = filtered.get(state.selected) {
                                chosen = Some(state.catalog[idx].action.clone());
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
    /// Whether `action` is one the search overlay handles while it owns the
    /// keyboard. Upstream's five, plus giest's own toggle.
    fn is_search_action(action: &Action) -> bool {
        matches!(
            action,
            Action::ToggleSearch
                | Action::StartSearch
                | Action::EndSearch
                | Action::NavigateSearch(_)
                | Action::SearchSelection
                | Action::Search(_)
        )
    }

    /// Resolve key presses through the keymap while the search overlay is open,
    /// returning the search actions to run and consuming the keys that matched.
    ///
    /// The overlay is **modal** — `run_pass` skips `handle_shortcuts` while it
    /// is up — so without this a `keybind = ctrl+g=navigate_search:next` would
    /// be dead exactly when it is wanted, and Escape would close the bar only
    /// because the close was also hardcoded (making `escape=unbind` a lie). It
    /// is the same principle already recorded for the scrollback keys: a key
    /// handled only by a hardcoded branch can be rebound but never turned off.
    ///
    /// Only the search family acts; every other bound chord stays swallowed,
    /// which is what modal means. **Modifier-less printable keys are skipped**
    /// and left to the text field — upstream's search entry holds the keyboard
    /// for the same reason, so a binding on a bare letter belongs to whoever is
    /// typing, not to the binding.
    fn search_overlay_actions(&self, ctx: &egui::Context) -> Vec<Action> {
        let events = ctx.input(|i| i.events.clone());
        let mut out = Vec::new();
        let mut consume = Vec::new();
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
            if crate::session::produces_text(*key, modifiers) {
                continue;
            }
            let Some(code) = session::map_egui_key(*key) else {
                continue;
            };
            let chord = Chord {
                mods: session::key_mods(modifiers),
                code,
            };
            // A plain lookup, not the sequence machine: a leader would have to
            // be held across frames while the overlay also owns the keyboard,
            // and a half-entered sequence inside a search box is a worse
            // failure than not supporting one there.
            let Some(action) = self.keymap.lookup_in(&self.key_tables, &chord) else {
                continue;
            };
            if !Self::is_search_action(&action) {
                continue;
            }
            consume.push((*modifiers, *key));
            out.push(action);
        }
        // Consumed so the text field doesn't also act on them — Escape in
        // particular, which a `TextEdit` treats as "defocus".
        ctx.input_mut(|i| {
            for (m, k) in consume {
                i.consume_key(m, k);
            }
        });
        out
    }

    /// Draw the terminal inspector over the focused pane (Ghostty
    /// `inspector:`), if that pane has one open.
    ///
    /// **Not a modal**, and that is the whole design: the keyboard log is
    /// worthless if opening it stops you typing, and watching a program redraw
    /// is why the IO log exists. So this appears in neither `modal_open` nor
    /// `render_active`'s `palette_open` gate — the one overlay in giest that
    /// doesn't take the keyboard.
    ///
    /// Per *pane*, like upstream's per-surface inspector: the state and the logs
    /// belong to the `Session`, so focusing another pane shows that pane's
    /// inspector or none at all.
    fn render_inspector(&mut self, ctx: &egui::Context) {
        self.inspector_rect = None;
        let focus = self.tabs.get(self.active_tab).map(|t| t.focus);
        let Some(pane) = focus.and_then(|f| self.last_layout.iter().find(|(id, _)| *id == f)) else {
            return;
        };
        let pane = pane.1;
        if !self
            .focused_session()
            .is_some_and(|s| s.inspector().is_some())
        {
            return;
        }

        // Collected inside the closure and applied after, so no session borrow
        // is held across the egui frame (the same shape `render_search` uses).
        let mut close = false;
        let mut clear = false;
        let mut toggle_pause = false;

        let facts = self.inspector_facts();
        let mono = egui::FontId::monospace(12.0);
        let response = egui::Window::new("Terminal Inspector")
            .id(self.id("inspector"))
            .constrain_to(pane)
            .default_pos(pane.left_top() + egui::vec2(16.0, 16.0))
            .default_size([460.0, 420.0])
            .collapsible(true)
            .resizable(true)
            .show(ctx, |ui| {
                let paused = facts.paused;
                ui.horizontal(|ui| {
                    if ui
                        .selectable_label(paused, if paused { "Resume" } else { "Pause" })
                        .on_hover_text("Stop recording (the terminal keeps running)")
                        .clicked()
                    {
                        toggle_pause = true;
                    }
                    if ui.button("Clear").clicked() {
                        clear = true;
                    }
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        if ui.button("\u{00d7}").on_hover_text("Close (Ctrl+Shift+I)").clicked() {
                            close = true;
                        }
                    });
                });
                ui.separator();

                egui::ScrollArea::vertical().show(ui, |ui| {
                    // Upstream's five windows, as collapsing sections: giest has
                    // no docking, and five floating windows over one pane would
                    // be unusable at a terminal's size.
                    egui::CollapsingHeader::new("Surface")
                        .default_open(true)
                        .show(ui, |ui| {
                            for (k, v) in &facts.surface {
                                fact_row(ui, k, v, &mono);
                            }
                        });
                    egui::CollapsingHeader::new("Terminal")
                        .default_open(true)
                        .show(ui, |ui| {
                            for (k, v) in &facts.terminal {
                                fact_row(ui, k, v, &mono);
                            }
                        });
                    egui::CollapsingHeader::new(format!("Keyboard ({})", facts.keys.len()))
                        .default_open(true)
                        .show(ui, |ui| {
                            if facts.keys.is_empty() {
                                ui.weak("Press a key in this pane.");
                            }
                            // Newest first: the press you are explaining is the
                            // one you just made, and a log that grows downward
                            // puts it off the bottom of a short panel.
                            for k in facts.keys.iter().rev() {
                                ui.horizontal(|ui| {
                                    ui.weak(format!("{:>4}", k.seq));
                                    ui.label(
                                        egui::RichText::new(&k.chord).font(mono.clone()).strong(),
                                    );
                                    ui.weak(k.outcome.label());
                                    if !k.bytes.is_empty() {
                                        ui.label(
                                            egui::RichText::new(&k.bytes).font(mono.clone()),
                                        );
                                    }
                                });
                            }
                        });
                    egui::CollapsingHeader::new(format!("Terminal IO ({})", facts.io.len()))
                        .default_open(true)
                        .show(ui, |ui| {
                            if facts.io.is_empty() {
                                ui.weak("Nothing read from the shell yet.");
                            }
                            for r in facts.io.iter().rev() {
                                ui.horizontal_wrapped(|ui| {
                                    ui.spacing_mut().item_spacing.x = 4.0;
                                    ui.weak(format!("{:>4}", r.seq));
                                    ui.weak(format!("{}B", r.len));
                                    ui.label(egui::RichText::new(&r.text).font(mono.clone()));
                                    if r.truncated {
                                        ui.weak("…");
                                    }
                                });
                            }
                        });
                });
            });

        // Remembered for the *next* frame's pane hit-testing, the same
        // previous-frame idiom as `last_layout`: the pane's `interact` runs in
        // `render_active`, which is over by the time this window exists, so a
        // click on "Pause" would otherwise also start a selection underneath.
        self.inspector_rect = response.map(|r| r.response.rect);

        if let Some(s) = self.focused_session_mut() {
            if close {
                s.set_inspector(crate::inspector::InspectorMode::Hide);
            } else if let Some(log) = s.inspector_mut() {
                if toggle_pause {
                    log.paused = !log.paused;
                }
                if clear {
                    log.clear();
                }
            }
        }
    }

    /// Snapshot everything the inspector displays, so the egui closure holds no
    /// borrow on the session it is describing.
    fn inspector_facts(&self) -> InspectorFacts {
        let mut facts = InspectorFacts::default();
        let Some(s) = self.focused_session() else {
            return facts;
        };
        let Some(log) = s.inspector() else {
            return facts;
        };
        facts.paused = log.paused;
        facts.keys = log.keys().cloned().collect();
        facts.io = log.io().cloned().collect();

        let ppp = self.egui_ctx.pixels_per_point();
        let (cols, rows) = s.grid_size();
        let snap = &s.snapshot;
        let mut surface = vec![
            ("Grid size".into(), format!("{cols} × {rows} cells")),
            (
                "Cell size".into(),
                format!("{:.1} × {:.1} px", self.cell_w, self.cell_h),
            ),
            ("Font size".into(), format!("{:.1} pt", self.font_points)),
            ("Scale".into(), format!("{ppp:.2} ×")),
            (
                "Padding".into(),
                format!(
                    "{:.0} × {:.0} pt",
                    self.config.padding_x, self.config.padding_y
                ),
            ),
        ];
        if let Some((_, rect)) = self
            .tabs
            .get(self.active_tab)
            .map(|t| t.focus)
            .and_then(|f| self.last_layout.iter().find(|(id, _)| *id == f))
        {
            surface.push((
                "Pane".into(),
                format!("{:.0} × {:.0} pt", rect.width(), rect.height()),
            ));
        }
        facts.surface = surface;

        let rgb = |c: crate::engine::Rgb| format!("#{:02x}{:02x}{:02x}", c.r, c.g, c.b);
        facts.terminal = vec![
            (
                "Cursor".into(),
                format!(
                    "({}, {}) {:?}{}{}",
                    snap.cursor_x,
                    snap.cursor_y,
                    snap.cursor_shape,
                    if snap.cursor_visible { "" } else { " hidden" },
                    if snap.cursor_blinking { " blinking" } else { "" },
                ),
            ),
            ("Scrollback".into(), format!("{} rows", s.scrollback_rows())),
            (
                "Colors".into(),
                format!(
                    "fg {} · bg {} · cursor {}",
                    rgb(snap.default_fg),
                    rgb(snap.default_bg),
                    rgb(snap.cursor_color)
                ),
            ),
            (
                "Mouse tracking".into(),
                yes_no(s.is_mouse_tracking()).into(),
            ),
            ("Selection".into(), yes_no(s.has_selection()).into()),
            ("Read-only".into(), yes_no(s.readonly()).into()),
            ("Images".into(), format!("{} placements", snap.images.len())),
            (
                "Title".into(),
                s.title().unwrap_or_else(|| "(none)".into()),
            ),
            (
                "Working directory".into(),
                s.pwd()
                    .map(|p| p.display().to_string())
                    .unwrap_or_else(|| "(not reported)".into()),
            ),
        ];
        facts
    }

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

        // Anchor inside the *focused pane*, not the window.
        //
        // An `Area`'s anchor resolves against its `constrain_rect`, which
        // defaults to `ctx.content_rect()` — the whole window, tab strip
        // included. At 8pt from that rect's top the overlay landed **on top of
        // the tab strip**, covering the rightmost tabs and the new-tab control
        // and, being an interactable `Order::Foreground` area, winning the hit
        // test there for as long as search was open.
        //
        // `last_layout` is written by `render_active`, which runs earlier in
        // this same pass (see `run_pass`), so the rect is current rather than a
        // frame stale. The fallback covers the first frame and teardown.
        // Anchoring per-pane also matches Ghostty, whose search bar belongs to a
        // surface rather than to the window.
        let focus = self.tabs.get(self.active_tab).map(|t| t.focus);
        let pane = focus
            .and_then(|f| self.last_layout.iter().find(|(id, _)| *id == f))
            .map(|(_, r)| *r)
            .unwrap_or_else(|| ctx.content_rect());

        egui::Area::new(self.id("search"))
            .order(egui::Order::Foreground)
            .constrain_to(pane)
            .anchor(egui::Align2::RIGHT_TOP, egui::vec2(-8.0, 8.0))
            .show(ctx, |ui| {
                // Ghostty's own search overlay: `padding: 6px 8px; margin: 8px;
                // border-radius: 8px` with a 1px outline (its `style.css`).
                egui::Frame::NONE
                    .fill(ui.visuals().window_fill)
                    .stroke(ui.visuals().window_stroke)
                    .corner_radius(egui::CornerRadius::same(theme::RADIUS_LG))
                    .inner_margin(egui::Margin::symmetric(8, 6))
                    .show(ui, |ui| {
                        // The four trailing buttons are a cluster, not four
                        // separate controls; the default 8pt gap reads as the
                        // latter and leaves them touching the text field.
                        ui.spacing_mut().item_spacing.x = 4.0;
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

        // Apply the overlay's own widgets first, deferred so no session borrow
        // is held across the egui closure. Fetch the session once, then act on
        // the collected flags.
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

        // Then the bound keys — Escape, the Ctrl+Shift+F toggle and anything
        // else the user bound to the search family. Skipped on the frame the
        // overlay opened, whose own opening keypress is still in the queue and
        // would toggle it straight back shut.
        if !close && !just_opened {
            for action in self.search_overlay_actions(ctx) {
                self.execute_action(ctx, None, action);
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

        let chrome = self.chrome;
        // Reserve the new-tab controls *before* the tabs, and give the tabs only
        // what's left. The strip used to be one flat `ui.horizontal`, so past
        // ~8 tabs the row overflowed the panel's clip rect — and because egui
        // hit-tests against `rect ∩ clip_rect`, the overflowed tabs *and the
        // trailing new-tab button* became invisible and unclickable at once,
        // with no way to open a tab from the strip at all.
        let ctl_w = 30.0 + 26.0 + theme::TAB_GAP;

        let row = ui.horizontal(|ui| {
            let avail = (ui.available_width() - ctl_w).max(theme::TAB_MIN_W);
            // Shrink toward the minimum before scrolling, so a handful of tabs
            // stay fully readable and only a genuinely full strip scrolls.
            let natural = |w: f32| w.clamp(theme::TAB_MIN_W, theme::TAB_MAX_W);
            let budget = if ntabs > 0 {
                natural((avail - theme::TAB_GAP * (ntabs as f32 - 1.0)) / ntabs as f32)
            } else {
                theme::TAB_MAX_W
            };

            ui.allocate_ui_with_layout(
                egui::vec2(avail, theme::TAB_H),
                egui::Layout::left_to_right(egui::Align::Center),
                |ui| {
                    egui::ScrollArea::horizontal()
                        // Not optional: `Memory::data` (where a `ScrollArea`
                        // keeps its offset) is *not* viewport-keyed, so two
                        // windows would share one scroll position.
                        .id_salt(self.id("tab-scroll"))
                        .scroll_bar_visibility(
                            egui::scroll_area::ScrollBarVisibility::AlwaysHidden,
                        )
                        .auto_shrink([false, false])
                        .show(ui, |ui| {
                            ui.spacing_mut().item_spacing.x = theme::TAB_GAP;
                            for (i, tab) in self.tabs.iter().enumerate() {
                                let raw = tab
                                    .name
                                    .clone()
                                    .or_else(|| tab.focused_payload().title())
                                    .unwrap_or_else(|| format!("shell {}", i + 1));
                                let count = tab.leaf_count();
                                let suffix =
                                    if count > 1 { format!(" [{count}]") } else { String::new() };
                                let editing = matches!(&renaming, Some((ri, _)) if *ri == i);
                                let is_active = i == active;

                                if editing {
                                    // The rename box replaces the tab entirely.
                                    // Its rect deliberately does *not* join
                                    // `tab_rects`: at 140pt it is wider than any
                                    // tab and would skew every drop midpoint (a
                                    // drag is suppressed while renaming anyway).
                                    let text = &mut renaming.as_mut().unwrap().1;
                                    let te = ui.add_sized(
                                        [140.0, theme::TAB_H],
                                        egui::TextEdit::singleline(text),
                                    );
                                    if !te.has_focus() {
                                        te.request_focus();
                                    }
                                    // Escape cancels; Enter or clicking away
                                    // commits (empty text clears the override).
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
                                    tab_rects.push(te.rect);
                                    continue;
                                }

                                // Allocate the WHOLE tab and sense on it, so the
                                // active/hover states cover the tab rather than
                                // just its label, and so the padding and tint
                                // band answer clicks and the context menu. This
                                // is registered *before* the close button:
                                // egui's hit test breaks a tie by taking the
                                // last-registered widget, so the `×` must come
                                // second to stay clickable. The resulting
                                // click/drag split gives the `×` the click and
                                // the tab the drag, so press-and-drag from over
                                // the `×` still reorders.
                                let (rect, resp) = ui.allocate_exact_size(
                                    egui::vec2(budget, theme::TAB_H),
                                    egui::Sense::click_and_drag(),
                                );
                                tab_rects.push(rect);

                                let show_close = is_active || resp.hovered();
                                let fill = if is_active {
                                    chrome.fill_active
                                } else if resp.is_pointer_button_down_on() {
                                    chrome.fill_active
                                } else if resp.hovered() {
                                    chrome.fill_hover
                                } else {
                                    egui::Color32::TRANSPARENT
                                };
                                let p = ui.painter();
                                if fill != egui::Color32::TRANSPARENT {
                                    p.rect_filled(
                                        rect,
                                        egui::CornerRadius::same(theme::RADIUS_SM),
                                        fill,
                                    );
                                }
                                // The tab's tint is a band along the bottom edge
                                // rather than the tab's fill, so a *tinted
                                // inactive* tab still reads as inactive — as its
                                // fill, the tint overrode the one signal that
                                // matters most in the strip.
                                if let Some(tint) = tab.color {
                                    p.rect_filled(
                                        egui::Rect::from_min_max(
                                            egui::pos2(rect.left(), rect.bottom() - theme::TAB_TINT_H),
                                            rect.max,
                                        ),
                                        egui::CornerRadius {
                                            nw: 0,
                                            ne: 0,
                                            sw: theme::RADIUS_SM,
                                            se: theme::RADIUS_SM,
                                        },
                                        tint,
                                    );
                                }
                                if is_active {
                                    // The active tab's underline. Ghostty/Adw
                                    // marks the selected tab this way, and it
                                    // survives a tab tint sitting beneath it.
                                    p.rect_filled(
                                        egui::Rect::from_min_max(
                                            egui::pos2(rect.left() + 4.0, rect.bottom() - 2.0),
                                            egui::pos2(rect.right() - 4.0, rect.bottom()),
                                        ),
                                        egui::CornerRadius::ZERO,
                                        chrome.accent,
                                    );
                                }

                                // Text is truncated by *measured width*, not by a
                                // character count: the chrome font is
                                // proportional, so 24 chars of "WWWW…" and of
                                // "iiii…" are wildly different widths and the
                                // old cap either overflowed or wasted the tab.
                                // `TAB_CLOSE_COL` stays reserved whether or not
                                // the `×` is showing, so nothing reflows on
                                // hover.
                                let text_max =
                                    (rect.width() - 8.0 - theme::TAB_CLOSE_COL).max(8.0);
                                let font = egui::TextStyle::Button.resolve(ui.style());
                                let ink = if is_active { chrome.text } else { chrome.weak_text };
                                let label = {
                                    let p = ui.painter();
                                    let f = font.clone();
                                    truncate_to_width(&raw, &suffix, text_max, |s| {
                                        p.layout_no_wrap(
                                            s.to_owned(),
                                            f.clone(),
                                            egui::Color32::WHITE,
                                        )
                                        .size()
                                        .x
                                    })
                                };
                                p.text(
                                    egui::pos2(rect.left() + 8.0, rect.center().y),
                                    egui::Align2::LEFT_CENTER,
                                    &label,
                                    font,
                                    ink,
                                );

                                if resp.clicked() {
                                    switch_to = Some(i);
                                }
                                // Middle-click closes the tab, like a browser.
                                if resp.clicked_by(egui::PointerButton::Middle) {
                                    want_close = Some(i);
                                }
                                // Primary only: egui starts drags on *any* held
                                // button, so a middle-press-and-jiggle would
                                // otherwise begin a reorder instead of closing.
                                if resp.drag_started_by(egui::PointerButton::Primary) {
                                    drag_from = Some(i);
                                }
                                if label != raw {
                                    resp.clone().on_hover_text(&raw);
                                }

                                // The close button, registered last so it wins
                                // the click where it overlaps the tab.
                                if show_close {
                                    let cb = egui::Rect::from_center_size(
                                        egui::pos2(
                                            rect.right() - theme::TAB_CLOSE_COL * 0.5 - 2.0,
                                            rect.center().y,
                                        ),
                                        egui::vec2(16.0, 16.0),
                                    );
                                    let cr = ui.interact(
                                        cb,
                                        self.id(("tab-close", i)),
                                        egui::Sense::click(),
                                    );
                                    if cr.hovered() {
                                        ui.painter().rect_filled(
                                            cb,
                                            egui::CornerRadius::same(theme::RADIUS_SM),
                                            chrome.fill_hover,
                                        );
                                    }
                                    ui.painter().text(
                                        cb.center(),
                                        egui::Align2::CENTER_CENTER,
                                        "×",
                                        egui::TextStyle::Body.resolve(ui.style()),
                                        if cr.hovered() { chrome.text } else { chrome.weak_text },
                                    );
                                    if cr.on_hover_text("Close tab (Ctrl+Shift+W)").clicked() {
                                        want_close = Some(i);
                                    }
                                }

                                // On the whole-tab response, so the padding, the
                                // tint band and the `×` all answer a right-click
                                // (it used to be on the label alone).
                                resp.context_menu(|ui| {
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
                        });
                },
            );

            // The new-tab controls, pinned: they are laid out in the space
            // reserved above, so they stay put and stay reachable no matter how
            // many tabs are open.
            if ui
                .add_sized([26.0, theme::TAB_H], egui::Button::new("+").frame(false))
                .on_hover_text("New tab (Ctrl+Shift+T)")
                .clicked()
            {
                want_new = Some(default_profile);
            }
            // Profile picker: open a tab running a chosen shell.
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

        // Measured from the laid-out row, not from `ui.max_rect()`: a top panel
        // sizes itself to its content, so before the row is built `max_rect` is
        // the whole remaining window, not the strip.
        let strip_top = row.response.rect.top();
        let strip_bottom = row.response.rect.bottom();

        // A hairline along the panel's bottom edge, so the strip reads as chrome
        // sitting above the terminal rather than blending into it (at opacity 1
        // the two fills are close enough to merge). Ghostty's tab bar has the
        // same edge. `clip_rect` is the panel, so the line spans its full width
        // rather than stopping at the content margin.
        //
        // Snapped to device pixels, like the split gutter, so the line sits in
        // the same place every frame instead of drifting with the panel's
        // fractional height.
        //
        // It will still render slightly soft: egui feathers *all* geometry for
        // anti-aliasing, so a one-device-pixel band spreads over two rows at
        // partial coverage no matter how it is drawn (measured at 1.5×:
        // `divider` `#2D2F34` lands as two rows of `#282A2F`, ~83% each).
        // Snapping is worth it anyway — an unsnapped line moves between the two
        // rows as the window resizes, which reads as flicker.
        let ppp = ui.ctx().pixels_per_point().max(1.0);
        let top = ((strip_bottom + 3.0) * ppp).round() / ppp;
        let clip = ui.clip_rect();
        ui.painter().rect_filled(
            egui::Rect::from_min_max(
                egui::pos2(clip.left(), top),
                egui::pos2(clip.right(), top + 1.0 / ppp),
            ),
            egui::CornerRadius::ZERO,
            chrome.divider,
        );

        // Resolve an in-progress tab drag. The tabs stay put while dragging; an
        // insertion caret shows where the drop would land.
        //
        // Suppressed entirely while a rename is open: the rename box is not in
        // `tab_rects` (it is far wider than a tab), so the midpoints a drop would
        // be computed from don't describe the strip on screen.
        if let Some(from) = drag_from {
            self.tab_drag = Some(from);
        }
        if renaming.is_some() {
            self.tab_drag = None;
        } else if let Some(from) = self.tab_drag {
            let pointer = ui.ctx().input(|i| i.pointer.clone());
            let held = pointer.any_down();
            if let Some(x) = pointer.latest_pos().map(|p| p.x) {
                let to = drop_index(&tab_rects, x);
                if held && !tab_rects.is_empty() {
                    ui.ctx().set_cursor_icon(egui::CursorIcon::Grabbing);
                    // Caret centred in the *gap* the tab would land in. Drawn at
                    // a tab's edge it sat inside the neighbouring tab instead of
                    // between the two, which reads as "replace this one" rather
                    // than "insert here".
                    let last = tab_rects.len() - 1;
                    let cx = if to == 0 {
                        tab_rects[0].left() - theme::TAB_GAP * 0.5
                    } else if to > last {
                        tab_rects[last].right() + theme::TAB_GAP * 0.5
                    } else {
                        (tab_rects[to - 1].right() + tab_rects[to].left()) * 0.5
                    };
                    ui.painter().rect_filled(
                        egui::Rect::from_min_max(
                            egui::pos2(cx - 1.0, strip_top),
                            egui::pos2(cx + 1.0, strip_bottom),
                        ),
                        egui::CornerRadius::same(1),
                        chrome.accent,
                    );
                } else if !held {
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
        // `toggle_background_opacity` forces fully opaque over the config.
        let background_opacity = if self.opaque_override {
            1.0
        } else {
            self.config.background_opacity
        };
        let background_opacity_cells = self.config.background_opacity_cells;
        let unfocused_split_opacity = self.config.unfocused_split_opacity;
        let unfocused_split_fill = self.config.unfocused_split_fill;
        let resize_overlay_position = self.config.resize_overlay_position;
        let scrollbar_mode = self.config.scrollbar;
        // Snapshot the keymap so the focused pane's `handle_input` can consult it
        // without holding a borrow on `self` across the pane-tree mutation below.
        // Cheap: a couple dozen (Chord, Action) entries, both `Copy`.
        let keymap = self.keymap.clone();
        // The key-table stack the focused pane resolves keys against — the same
        // one `handle_shortcuts` uses, cloned for the same borrow reason.
        let tables = self.key_tables.clone();
        let undo_state = self.undo_state;
        let bell_border = self.config.bell.border;
        let now = ctx.input(|i| i.time);
        let active_tab = self.active_tab;
        // Hoisted so the per-pane widget id can be namespaced to this window
        // from inside the leaf loop, which borrows `self.tabs`.
        let win_id = self.window_id;
        // `app-notifications = clipboard-copy`, read here for the same reason.
        let copy_toast = self.config.app_notifications.clipboard_copy;
        // While a modal overlay is up it owns input: don't feed keys to the
        // focused pane or let it grab keyboard focus. `egui::Modal` blocks
        // *pointer* interaction and tab traversal on its own, but
        // `Ui::request_focus` is unconditional — so the pane's per-frame focus
        // grab has to be suppressed explicitly, or typing would still reach the
        // shell behind the confirmation dialog.
        let palette_open =
            self.palette.is_some()
                || self.confirm.is_some()
                || self.clipboard_prompt().is_some()
                || self.config_errors_open();

        // The inspector is deliberately **not** in that list — it must never
        // take the keyboard, or its own keyboard log would have nothing to
        // show. But it does sit over the pane, so the *pointer* has to be
        // withheld where it is, or a click on its Pause button also starts a
        // text selection underneath. Last frame's rect, since the window is
        // drawn after this (the `last_layout` idiom).
        let over_inspector = self.inspector_rect.is_some_and(|r| {
            ctx.pointer_latest_pos().is_some_and(|p| r.contains(p))
        });

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
        //
        // With a `background-image` configured, this fill moves *into* the
        // renderer: its shader paints the background color and the image in one
        // quad (Ghostty's bg-image pass does the same, and for the same reason).
        // Painting both would be exactly the double-composite described above.
        let bg = self.tabs[active_tab].focused_payload().default_bg();
        let bg_image = self.bg_image.clone();
        let custom_shaders = self.custom_shaders.clone();
        // A custom shader reads the terminal from an offscreen texture that egui
        // never draws into, so with one active the fill has to come from the
        // renderer instead (`TermFrame::window_fill`) or the shader filters a
        // transparent screen.
        if bg_image.is_none() && custom_shaders.is_empty() {
            let bg_alpha8 = (self.config.background_opacity * 255.0).round() as u8;
            ui.painter().rect_filled(
                full_area,
                0.0,
                egui::Color32::from_rgba_unmultiplied(bg.r, bg.g, bg.b, bg_alpha8),
            );
        }
        let area_px = [
            (full_area.min.x * ppp).round(),
            (full_area.min.y * ppp).round(),
            (full_area.width() * ppp).round(),
            (full_area.height() * ppp).round(),
        ];
        let bg_image = bg_image.map(|source| BgImageFrame {
            source,
            fit: self.config.background_image_fit,
            position: self.config.background_image_position,
            repeat: self.config.background_image_repeat,
            opacity: self.config.background_image_opacity,
        });

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
        //
        // The gutters are gathered first, through an immutable walk, because
        // `collect` needs `&mut tab.root` for the payloads. Zoomed tabs show one
        // pane and therefore have none.
        let mut leaves: Vec<Leaf<Session>> = Vec::new();
        let mut gutters: Vec<egui::Rect> = Vec::new();
        match zoomed {
            Some(id) => tab.root.collect_leaf(id, full_area, &mut leaves),
            None => {
                tab.root.gutters(full_area, ppp, &mut gutters);
                tab.root.collect(full_area, ppp, &mut leaves);
            }
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
        let (link_osc8, link_url) = (self.config.link_osc8, self.config.link_url);
        // `focus-follows-mouse`: hovering a split focuses it, no click needed.
        // Gated on the pointer having actually *moved* — otherwise a parked
        // cursor would drag focus back every frame and make `focus_split_*`
        // keybinds impossible to use.
        if self.config.focus_follows_mouse && !search_open {
            let moved = ctx.input(|i| i.pointer.velocity() != egui::Vec2::ZERO);
            if moved
                && let Some(pos) = ctx.input(|i| i.pointer.latest_pos())
                && let Some(l) = leaves.iter().find(|l| l.rect.contains(pos))
            {
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
                .handle_input(ctx, tracking, ch, &keymap, &tables, undo_state);
            // Ctrl+C / Ctrl+Shift+C copies happen inside the session (they
            // arrive as `Event::Copy`), so it reports them back for the toast.
            if leaves[focus_idx].payload.take_copied() && copy_toast {
                push_toast(ctx, win_id, "Copied to clipboard");
            }
        } else if search_open {
            // The search overlay owns the keyboard, so input (and its scroll
            // easing) is skipped — but keep the viewport easing toward the match
            // that `scroll_to_match` targeted.
            leaves[focus_idx].payload.tick_scroll(ctx, ch);
        }
        // A modal took the keyboard mid-composition: its IME events now go to
        // the modal's text box, so this preedit could never be finished or
        // cleared and would sit over the grid forever.
        if palette_open || search_open {
            leaves[focus_idx].payload.clear_preedit();
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
        // Panes in read-only mode, which get a persistent badge — without one,
        // `toggle_readonly` looks exactly like a hung shell.
        let mut readonly_panes: Vec<egui::Rect> = Vec::new();
        // (cursor cell, preedit text, default fg, default bg) for the focused
        // pane's in-progress IME composition, painted over the grid below.
        let mut ime_preedit: Option<(egui::Rect, String, crate::engine::Rgb, crate::engine::Rgb)> =
            None;
        // (pane rect, message, is-error) for panes held open after their shell
        // exited (`wait-after-command` / `abnormal-command-exit-runtime`).
        let mut exit_bars: Vec<(egui::Rect, String, bool)> = Vec::new();
        for leaf in leaves.iter_mut() {
            // The pane occupies `leaf.rect`; the grid is inset by the padding so
            // text clears the pane's edges (window border or split divider alike).
            // `window-padding-balance` then shares out the leftover space — the
            // remainder of dividing the pane by whole cells, which otherwise all
            // piles up on the right and bottom.
            let prect = balance_pane(
                leaf.rect.shrink2(pad),
                self.config.window_padding_balance,
                self.cell_w,
                self.cell_h,
                ctx.pixels_per_point().max(1.0),
            );
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
            if session.readonly() {
                readonly_panes.push(leaf_rect);
            }
            if let Some((msg, error)) = session.exit_bar() {
                exit_bars.push((leaf_rect, msg, error));
            }
            if let Some(a) = session.resize_overlay_alpha(now) {
                let (cols, rows) = session.grid_size();
                // Label format matches Ghostty's overlay exactly.
                resize_overlays.push((leaf_rect, format!("{cols} x {rows}"), a));
            }
            if !session.update_snapshot() {
                continue;
            }

            // IME: allow it for the focused pane and park the candidate window
            // on the terminal cursor. egui-winit turns `PlatformOutput::ime`
            // into `set_ime_allowed` + `set_ime_cursor_area`, placing the
            // candidates under `rect` — so `rect` is the cursor cell, not the
            // pane. Not while a modal owns the keyboard (its own text box sets
            // this instead), mirroring the `handle_input` gate above.
            if is_focus && !palette_open && !search_open {
                let snap = &session.snapshot;
                let (cwp, chp) = (cw / ppp, ch / ppp);
                let cell = egui::Rect::from_min_size(
                    prect.min
                        + egui::vec2(snap.cursor_x as f32 * cwp, snap.cursor_y as f32 * chp),
                    egui::vec2(cwp, chp),
                );
                ctx.output_mut(|o| {
                    o.ime = Some(egui::output::IMEOutput {
                        rect: cell,
                        cursor_rect: cell,
                    })
                });
                if let Some(text) = session.preedit() {
                    ime_preedit = Some((cell, text.to_owned(), snap.default_fg, snap.default_bg));
                }
            }

            // Mouse / selection interaction only for the focused pane, and not
            // while a modal overlay (palette or search) owns input/focus.
            if is_focus && !palette_open && !search_open && !over_inspector {
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

                // `handle_mouse` reads raw `ctx.input` events rather than the
                // `Response` above, so egui's widget arbitration does *not*
                // keep a scrollbar drag out of it — the grab check is what
                // does. Normally the bar is already hidden while `tracking`
                // (see `scrollbar::eligible`); this covers the frame a program
                // enables tracking with the thumb still held.
                if tracking {
                    if !session.scrollbar_grabbed() {
                        session.handle_mouse(ctx, prect, ppp, cw, ch);
                    }
                    session.clear_selection();
                } else {
                    let cell_at = |p: egui::Pos2, s: &Session| s.pos_to_cell(p, prect, ppp, cw, ch);
                    if resp.triple_clicked() {
                        if let Some(p) = resp.interact_pointer_pos() {
                            let c = cell_at(p, session);
                            // Ctrl+triple-click selects the command's *output*
                            // rather than the line, matching Ghostty's
                            // `Surface.zig` click-count handling.
                            if ctx.input(|i| i.modifiers.ctrl) {
                                session.select_output(c);
                            } else {
                                session.select_line(c);
                            }
                        }
                    } else if resp.double_clicked() {
                        if let Some(p) = resp.interact_pointer_pos() {
                            let c = cell_at(p, session);
                            session.select_word(c);
                        }
                    } else if resp.drag_started() {
                        if let Some(p) = resp.interact_pointer_pos() {
                            let c = cell_at(p, session);
                            let rect = ctx.input(|i| crate::session::is_rectangle_select(&i.modifiers));
                            session.begin_selection(c, rect);
                        }
                    } else if resp.dragged() {
                        if let Some(p) = resp.interact_pointer_pos() {
                            let c = cell_at(p, session);
                            let rect = ctx.input(|i| crate::session::is_rectangle_select(&i.modifiers));
                            // Dragging past the top or bottom edge scrolls, so a
                            // selection can run into the scrollback without
                            // letting go. Rate-limited to Ghostty's 15 ms per
                            // row rather than one row per frame, which would run
                            // at the display's refresh rate. The repaint request
                            // is what keeps it ticking with the pointer parked —
                            // egui reports the drag every frame, but nothing
                            // else would schedule those frames.
                            let dir = if p.y < prect.min.y {
                                -1
                            } else if p.y > prect.max.y {
                                1
                            } else {
                                0
                            };
                            let dt = ctx.input(|i| i.stable_dt);
                            let rows = session.autoscroll_step(dir, dt);
                            if rows != 0 {
                                session.scroll_lines(rows, ch);
                            }
                            if dir != 0 {
                                ctx.request_repaint();
                            }
                            // The cell is resolved against *this* frame's
                            // viewport, so the end trails the scroll by one
                            // frame and catches up on the next tick — the same
                            // ordering the scrollbar drag documents.
                            session.update_selection(c, rect);
                        }
                    } else if resp.clicked() {
                        let mods = ctx.input(|i| i.modifiers);
                        if mods.shift {
                            // Shift+click extends the current selection.
                            if let Some(p) = resp.interact_pointer_pos() {
                                let c = cell_at(p, session);
                                session.extend_selection(
                                    c,
                                    crate::session::is_rectangle_select(&mods),
                                );
                            }
                        } else {
                            // Ctrl+click opens a URL under the cursor; a plain
                            // click clears the selection.
                            let opened = (mods.ctrl || mods.command)
                                && resp
                                    .interact_pointer_pos()
                                    .map(|p| cell_at(p, session))
                                    .and_then(|c| session.url_at(c, link_osc8, link_url))
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
                            if copy_toast {
                                push_toast(ctx, win_id, "Copied to clipboard");
                            }
                        }
                    };
                    let paste = |s: &mut Session| {
                        if let Some(text) = session::read_clipboard() {
                            s.paste_str(&text);
                        }
                    };
                    match right_click_action {
                        RightClickAction::ContextMenu => {
                            let has_sel = session.has_selection();
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
                                if session.has_selection() {
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
                cursor_hollow: !pane_active,
                cursor_blink_hidden,
                blink_hidden,
                scroll_offset_px: session.scroll_offset_px(),
                search_highlights: session.search_highlights(),
            });
        }

        // Scrollbars. Deliberately a *second* pass over the leaves, after the
        // loop above: egui resolves which widget owns the pointer from the last
        // interested widget registered in the layer, so the bar has to register
        // after each pane's `ui.interact` to win an overlap. That's what keeps a
        // thumb drag from also painting a text selection, and it only matters
        // when `window-padding-x` is small enough for the two rects to touch —
        // at the default 20pt the bar sits entirely inside the padding gutter.
        //
        // Not gated on `is_focus`: every split shows its own bar.
        let mut scrollbars: Vec<(egui::Rect, scrollbar::Thumb, f32, bool)> = Vec::new();
        // `is_pointer_button_down_on` is true for *any* button, so gate the grab
        // on the primary one — a right-click on the bar must not drag it.
        let primary_down = ctx.input(|i| i.pointer.primary_down());
        for leaf in leaves.iter_mut() {
            let leaf_rect = leaf.rect;
            let leaf_id = leaf.id;
            let session = &mut *leaf.payload;
            // Per-pane, *not* the focused pane's `tracking` from above.
            if !scrollbar::eligible(
                scrollbar_mode,
                session.scrollback_rows(),
                session.is_mouse_tracking(),
            ) {
                // Drop a grab that a program just invalidated by taking the mouse.
                session.set_scrollbar_grab(None);
                continue;
            }
            let Some(state) = session.scrollbar_state(ch) else {
                continue;
            };
            let right = leaf_rect.right() - SCROLLBAR_EDGE_INSET;
            let track = egui::Rect::from_min_max(
                egui::pos2(right - SCROLLBAR_TRACK_W, leaf_rect.top() + SCROLLBAR_INSET),
                egui::pos2(right, leaf_rect.bottom() - SCROLLBAR_INSET),
            );
            let Some(thumb) = scrollbar::thumb(track.height(), state.total, state.offset, state.len)
            else {
                continue;
            };
            let alpha = session.scrollbar_alpha(now);

            // While a modal overlay is up, paint the bar but don't interact.
            // The palette is an `egui::Modal` and blocks the pointer itself, but
            // the search overlay is a plain `Area` and does not — so without
            // this, a click meant for the search box could reach a bar behind it.
            // Still painted, so a search jump visibly moves the thumb.
            if palette_open || search_open || over_inspector {
                if let Some(a) = alpha {
                    scrollbars.push((track, thumb, a, false));
                }
                continue;
            }

            // Hidden: interact with a narrow band at the very edge, so hovering
            // there wakes the bar (Ghostty's macOS scroller flashes on hover for
            // the same reason). Visible: the full track is the hit target.
            let hit = if alpha.is_some() {
                track
            } else {
                egui::Rect::from_min_max(
                    egui::pos2(right - SCROLLBAR_HOT_W, track.top()),
                    egui::pos2(right, track.bottom()),
                )
            };
            let sense = if alpha.is_some() {
                egui::Sense::click_and_drag()
            } else {
                egui::Sense::hover()
            };
            let resp = ui.interact(
                hit,
                egui::Id::new(("giest-window", win_id, "scrollbar", active_tab, leaf_id)),
                sense,
            );
            let hovered = resp.hovered();
            if hovered {
                session.mark_scrollbar_active(now);
            }

            // Press. Taken on the press frame rather than `drag_started()`,
            // which fires a frame later — by then the pointer has already moved
            // past the drag threshold, and that displacement would be baked into
            // the grab offset as a permanent few-pixel error.
            if primary_down && resp.is_pointer_button_down_on() && session.scrollbar_grab().is_none()
            {
                if let Some(pos) = resp.interact_pointer_pos() {
                    let y = pos.y - track.top();
                    match scrollbar::track_click_page(y, &thumb) {
                        0 => session.set_scrollbar_grab(Some(y - thumb.top)),
                        dir => {
                            // Click in the track pages toward the pointer, eased.
                            // `scroll_lines` takes a negative delta to move *up*
                            // into history, which is the same sign convention
                            // `track_click_page` uses (-1 = above the thumb).
                            let page = session.page_lines();
                            session.scroll_lines(dir as isize * page, ch);
                        }
                    }
                }
            }

            // Drag. `interact_pointer_pos` keeps reporting while the button is
            // held *outside* the widget, so the drag continues off the bar.
            if let (Some(grab), Some(pos)) = (session.scrollbar_grab(), resp.interact_pointer_pos())
            {
                let top = pos.y - track.top() - grab;
                let offset = scrollbar::offset_from_thumb_top(top, &thumb, state.total, state.len);
                session.scroll_to_row(offset, ch, true);
                session.mark_scrollbar_active(now);
            }
            if !primary_down {
                session.set_scrollbar_grab(None);
            }

            // Re-read: a drag or a hover this frame may have raised the bar that
            // was hidden when `alpha` was sampled above.
            let grabbed = session.scrollbar_grabbed();
            if let Some(a) = session.scrollbar_alpha(now) {
                // Recompute against the position the drag just wrote, so the
                // thumb lands exactly under the cursor with no easing lag.
                let live = session
                    .scrollbar_state(ch)
                    .and_then(|s| scrollbar::thumb(track.height(), s.total, s.offset, s.len))
                    .unwrap_or(thumb);
                scrollbars.push((track, live, a, hovered || grabbed));
            }
        }

        // `window-width`/`-height`/`-position-*`, applied exactly once.
        //
        // Deferred to here rather than done at window creation because the size
        // is in **cells**, and cell metrics don't exist until the glyph atlas is
        // built — which happens *after* the window. Doing it here also makes the
        // chrome exact: the tab strip's height is simply whatever the layout
        // didn't give the terminal, rather than a guessed constant.
        if !self.geometry_applied {
            self.geometry_applied = true;
            let cfg = &self.config;
            if cfg.window_width > 0 || cfg.window_height > 0 {
                let chrome_h = ctx.content_rect().height() - full_area.height();
                let cur = full_area.size();
                let w = if cfg.window_width > 0 {
                    cfg.window_width as f32 * cw / ppp + 2.0 * self.config.padding_x
                } else {
                    cur.x
                };
                let h = if cfg.window_height > 0 {
                    cfg.window_height as f32 * ch / ppp + 2.0 * self.config.padding_y + chrome_h
                } else {
                    cur.y + chrome_h
                };
                ctx.send_viewport_cmd(egui::ViewportCommand::InnerSize(egui::vec2(w, h)));
            }
            // Both or neither, which is Ghostty's rule — a half-specified
            // position is more likely a typo than an intent.
            //
            // Divided by `ppp` because Ghostty documents the position in
            // **pixels** while egui's viewport commands are in points: at 125%
            // scaling, passing the number through unchanged lands the window 25%
            // too far down and right (measured).
            if let (Some(x), Some(y)) = (cfg.window_position_x, cfg.window_position_y) {
                ctx.send_viewport_cmd(egui::ViewportCommand::OuterPosition(egui::pos2(
                    x as f32 / ppp,
                    y as f32 / ppp,
                )));
            }
        }

        // `mouse-hide-while-typing`: hide the pointer on a key press, bring it
        // back as soon as the mouse moves. Latched on `self` rather than derived
        // per frame, since "typing" is an edge and "hidden" is a state that has
        // to persist through the still frames in between.
        if self.config.mouse_hide_while_typing {
            let (typed, moved) = ctx.input(|i| {
                (
                    i.events.iter().any(|e| {
                        matches!(e, egui::Event::Key { pressed: true, .. } | egui::Event::Text(_))
                    }),
                    i.pointer.velocity() != egui::Vec2::ZERO,
                )
            });
            if moved {
                self.pointer_hidden = false;
            } else if typed {
                self.pointer_hidden = true;
            }
            if self.pointer_hidden {
                ctx.set_cursor_icon(egui::CursorIcon::None);
            }
        }

        // Shader uniforms for this frame. `iResolution` is the **framebuffer**,
        // not the terminal area: the offscreen targets are framebuffer-sized so
        // that every instance coordinate stays valid without remapping, so that
        // is also the space a shader's `fragCoord` lives in.
        let shader_globals = if custom_shaders.is_empty() {
            crate::shader::Globals::default()
        } else {
            let screen = ctx.viewport_rect();
            let now = self.shader_epoch.elapsed().as_secs_f32();
            let mut g = crate::shader::Globals::default();
            g.set_resolution((screen.width() * ppp).round(), (screen.height() * ppp).round());
            g.set_time(now);
            g.time_delta = (now - self.shader_last_time).max(0.0);
            g.frame_rate = if g.time_delta > 0.0 { 1.0 / g.time_delta } else { 0.0 };
            g.frame = self.shader_frame;
            g.focus = i32::from(window_focused);
            let rgb = |c: crate::engine::Rgb| {
                [c.r as f32 / 255.0, c.g as f32 / 255.0, c.b as f32 / 255.0, 1.0]
            };
            g.background_color = rgb(bg);
            g.foreground_color = rgb(self.config.fg);
            g.selection_background_color = rgb(sel_bg);
            g.selection_foreground_color = rgb(sel_fg.unwrap_or(self.config.fg));
            g.cursor_color = rgb(self.config.cursor.unwrap_or(self.config.fg));
            g.cursor_text = rgb(match self.config.cursor_text {
                Some(crate::config::TerminalColor::Color(c)) => c,
                _ => bg,
            });
            self.shader_last_time = now;
            self.shader_frame = self.shader_frame.wrapping_add(1);
            // An animated shader has to be driven: nothing else repaints an idle
            // terminal, so without this the effect freezes between keystrokes.
            if self.config.custom_shader_animation.animates(window_focused) {
                ctx.request_repaint();
            }
            g
        };

        // One callback paints every pane (shared instance buffer).
        ui.painter().add(egui_wgpu::Callback::new_paint_callback(
            full_area,
            TermFrame {
                panes: frames,
                selection_bg: sel_bg,
                selection_fg: sel_fg,
                search_bg: self.config.search_bg,
                search_fg: self.config.search_fg,
                search_selected_bg: self.config.search_selected_bg,
                search_selected_fg: self.config.search_selected_fg,
                cursor_text: self.config.cursor_text,
                background_opacity,
                background_opacity_cells,
                faint_opacity,
                cursor_opacity,
                background_color: bg,
                area_px,
                // Only the renderer can put the background where a custom
                // shader will see it; without shaders the app's own
                // `rect_filled` above is still the one true fill.
                window_fill: !custom_shaders.is_empty(),
                bg_image,
                custom_shaders,
                shader_globals,
            },
        ));

        // Paint the split gutters.
        //
        // Nothing used to draw them: the 1pt band was simply left out of both
        // panes and showed whatever was underneath. Under `background-opacity`
        // that "whatever" is the translucent window fill, so every divider was a
        // transparent slit straight through to the desktop. Ghostty paints its
        // separator explicitly for the same reason.
        //
        // This is not the double-composite the fill above warns about: `divider`
        // is opaque, and the gutter rects are disjoint from every pane rect.
        for rect in &gutters {
            ui.painter().rect_filled(*rect, 0.0, self.chrome.divider);
        }

        // Dim the unfocused splits by painting a semi-transparent rectangle over
        // each — the same mechanism Ghostty uses (an overlay, not a renderer
        // effect). Painter-only, so it never intercepts clicks.
        for (rect, col) in &dim_rects {
            ui.painter().rect_filled(*rect, 0.0, *col);
        }

        // Border widths are snapped for the same reason the gutter is: a 2pt
        // stroke at 1.25× scaling straddles a pixel boundary and rasterizes as a
        // blurred 3px band on one edge and a crisp 2px one on another.
        let stroke_w = |pt: f32| (pt * ppp).round().max(1.0) / ppp;

        // Outline the focused pane when the tab is split. Frame the *full* pane
        // rect (not the padded grid) so the padding band shows as a visible gap
        // between the border and the text, rather than the text hugging the line.
        if leaves.len() > 1 {
            ui.painter().rect_stroke(
                leaves[focus_idx].rect,
                0.0,
                egui::Stroke::new(stroke_w(2.0), self.chrome.accent),
                egui::StrokeKind::Inside,
            );
        } else if zoomed.is_some() {
            // A zoomed split hides its siblings, so a "split tab" can look like a
            // single pane. A green accent border signals the zoom is active
            // (toggle off with the same chord). Matches Ghostty's zoom indicator.
            ui.painter().rect_stroke(
                full_area,
                0.0,
                egui::Stroke::new(stroke_w(2.0), self.chrome.accent_zoom),
                egui::StrokeKind::Inside,
            );
        }

        // Visual bell: a warm border that fades over ~0.2s on each pane that rang.
        // Drawn over the terminal (after the pane callback) and kept animating by
        // requesting repaints while any flash is active.
        if !bell_flashes.is_empty() {
            ctx.request_repaint();
            for (rect, a) in &bell_flashes {
                let w = self.chrome.accent_warn;
                let col =
                    egui::Color32::from_rgba_unmultiplied(w.r(), w.g(), w.b(), (a * 220.0) as u8);
                ui.painter().rect_stroke(
                    *rect,
                    0.0,
                    egui::Stroke::new(stroke_w(3.0), col),
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
                // Ghostty's resize overlay: `padding: 4px 8px; border-radius: 6`
                // with a 1px outline (its `style.css`).
                let fade = |c: egui::Color32, mul: f32| {
                    egui::Color32::from_rgba_unmultiplied(c.r(), c.g(), c.b(), (a * mul) as u8)
                };
                let (anchor, align) = overlay_anchor(*rect, resize_overlay_position, 12.0);
                let font = egui::FontId::proportional(14.0);
                let galley =
                    ui.painter()
                        .layout_no_wrap(label.clone(), font, fade(self.chrome.text, 255.0));
                let text_rect = align.anchor_size(anchor, galley.size());
                let pill = text_rect.expand2(egui::vec2(8.0, 4.0));
                let r = egui::CornerRadius::same(theme::RADIUS_MD);
                ui.painter()
                    .rect_filled(pill, r, fade(self.chrome.window_fill, 230.0));
                ui.painter().rect_stroke(
                    pill,
                    r,
                    egui::Stroke::new(stroke_w(1.0), fade(self.chrome.divider, 220.0)),
                    egui::StrokeKind::Inside,
                );
                ui.painter()
                    .galley(text_rect.min, galley, self.chrome.text);
            }
        }

        // Child-exited bar, the macOS app's `ChildExitedMessageBar`: a strip
        // along the pane's bottom edge, red for a failure, neutral otherwise.
        // Painter-only like the badges — the key that dismisses it is read by
        // `Session::handle_input`, so a widget here would only eat clicks.
        for (rect, msg, error) in &exit_bars {
            let font = egui::FontId::proportional(13.0);
            let (fill, text) = if *error {
                (self.chrome.danger, self.chrome.on_accent)
            } else {
                (self.chrome.window_fill, self.chrome.text)
            };
            let galley = ui.painter().layout(
                msg.clone(),
                font,
                text,
                (rect.width() - 24.0).max(40.0),
            );
            let h = galley.size().y + 12.0;
            let bar = egui::Rect::from_min_max(
                egui::pos2(rect.left(), rect.bottom() - h),
                rect.right_bottom(),
            );
            ui.painter().rect_filled(bar, 0.0, fill);
            ui.painter().hline(
                bar.x_range(),
                bar.top(),
                egui::Stroke::new(stroke_w(1.0), self.chrome.divider),
            );
            ui.painter()
                .galley(bar.left_top() + egui::vec2(12.0, 6.0), galley, text);
        }

        // Read-only badge: persistent (the state is), painter-only (a widget
        // here would eat clicks meant for the terminal), and bottom-right so it
        // doesn't collide with the resize overlay's default centre or the
        // scrollbar's right edge band. No repaint request — nothing animates.
        for rect in &readonly_panes {
            let font = egui::FontId::proportional(12.0);
            let galley = ui.painter().layout_no_wrap(
                "READ-ONLY".to_string(),
                font,
                self.chrome.on_accent,
            );
            // Inset past the scrollbar's hot band so the two never overlap.
            let anchor = rect.right_bottom() + egui::vec2(-(SCROLLBAR_HOT_W + 8.0), -8.0);
            let text_rect = egui::Align2::RIGHT_BOTTOM.anchor_size(anchor, galley.size());
            let pill = text_rect.expand2(egui::vec2(7.0, 3.0));
            let r = egui::CornerRadius::same(theme::RADIUS_MD);
            // The warn accent, not danger: read-only is a mode the user asked
            // for, not an error.
            ui.painter().rect_filled(pill, r, self.chrome.accent_warn);
            ui.painter()
                .galley(text_rect.min, galley, self.chrome.on_accent);
        }

        // IME preedit: the uncommitted composition, drawn at the cursor in the
        // terminal's own colours and underlined (the platform convention for
        // "not yet typed"). Painter-only and opaque, so it covers the cells it
        // overlaps; it is egui text rather than atlas glyphs because it never
        // enters the grid — the shell only sees the commit.
        if let Some((cell, text, fg, bg)) = &ime_preedit {
            let fg = egui::Color32::from_rgb(fg.r, fg.g, fg.b);
            let bg = egui::Color32::from_rgb(bg.r, bg.g, bg.b);
            let font = egui::FontId::monospace((cell.height() * 0.8).max(6.0));
            let galley = ui.painter().layout_no_wrap(text.clone(), font, fg);
            let size = egui::vec2(galley.size().x.max(cell.width()), cell.height());
            let rect = egui::Rect::from_min_size(cell.min, size);
            ui.painter().rect_filled(rect, 0.0, bg);
            let y = rect.center().y - galley.size().y * 0.5;
            ui.painter().galley(egui::pos2(rect.left(), y), galley, fg);
            let underline_y = rect.bottom() - stroke_w(1.0);
            ui.painter().hline(
                rect.x_range(),
                underline_y,
                egui::Stroke::new(stroke_w(1.0), fg),
            );
        }

        // Scrollbars, painted last so a bar over an unfocused split stays
        // legible through the dim rect. Painter-only — the interaction already
        // happened above, where registration order settles it against the pane.
        // Must keep requesting repaints or the auto-hide fade freezes on an
        // idle terminal.
        if !scrollbars.is_empty() {
            ctx.request_repaint();
            for (track, thumb, a, hot) in &scrollbars {
                let w = if *hot {
                    SCROLLBAR_KNOB_W_HOT
                } else {
                    SCROLLBAR_KNOB_W
                };
                let knob = egui::Rect::from_min_size(
                    egui::pos2(track.center().x - w * 0.5, track.top() + thumb.top),
                    egui::vec2(w, thumb.len),
                );
                // The knob is an overlay: it floats over the grid rather than
                // over a chrome surface, so it needs a hairline of the *window*
                // background around it to separate it from whatever text is
                // underneath. Without the outline it reads as a smear over a
                // dense line of output.
                let peak = if *hot { 0.95 } else { 0.65 };
                let a8 = |c: egui::Color32, mul: f32| {
                    egui::Color32::from_rgba_unmultiplied(
                        c.r(),
                        c.g(),
                        c.b(),
                        (a * mul * 255.0) as u8,
                    )
                };
                let fill = if *hot {
                    self.chrome.text
                } else {
                    self.chrome.knob
                };
                let radius = egui::CornerRadius::same((w * 0.5).round() as u8);
                ui.painter().rect_stroke(
                    knob.expand(1.0),
                    radius,
                    egui::Stroke::new(stroke_w(1.0), a8(egui::Color32::from_rgb(bg.r, bg.g, bg.b), peak)),
                    egui::StrokeKind::Inside,
                );
                ui.painter().rect_filled(knob, radius, a8(fill, peak));
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
    fn pump_all(&mut self, now: f64, notifications: &mut Vec<crate::osc_notify::Notification>) {
        let was_focused = self.was_focused;
        let mut rang = false;
        let mut finished: Vec<session::CommandFinish> = Vec::new();
        let mut progress_changed = false;
        for tab in &mut self.tabs {
            tab.root.for_each_mut(&mut |pane| {
                pane.pump_pty();
                pane.idle_work();
                rang |= pane.take_bell_effect(now);
                let focused = was_focused;
                notifications.extend(pane.take_notifications().into_iter().filter(|n| {
                    // Kitty `o=unfocused` / `o=invisible`: nothing to tell a user who
                    // is already looking at this window.
                    n.occasion == crate::osc_notify::Occasion::Always || !focused
                }));
                finished.append(&mut pane.take_command_finishes());
                progress_changed |= pane.take_progress().is_some();
            });
        }
        self.pending_bell |= rang;
        if progress_changed {
            self.progress_dirty = true;
        }

        // `notify-on-command-finish`. The mode and the `unfocused` test both
        // belong to *this* window (a background window's long build should still
        // report), so it is decided here rather than in the app-level drain —
        // but a `notify` action still goes out through that drain, since only
        // the root window has an `HWND` to hang a toast on.
        let mode = self.config.notify_on_command_finish;
        if mode == crate::config::NotifyOnCommandFinish::Never || finished.is_empty() {
            return;
        }
        // `i.focused` is per-viewport and only meaningful during that viewport's
        // own pass, so read this window's *last observed* focus rather than the
        // running pass's — `pump_all` runs for every window from the root pass.
        if !crate::config::should_notify_on_finish(mode, self.was_focused) {
            return;
        }
        let after = Duration::from_millis(self.config.notify_on_command_finish_after_ms);
        let action = self.config.notify_on_command_finish_action;
        for f in finished.iter().filter(|f| f.duration >= after) {
            if action.bell {
                // Reuses the whole `bell-features` path, so a user who has
                // configured the bell to flash or flag the taskbar gets that
                // here too — which is what "action = bell" means upstream.
                self.pending_bell = true;
            }
            if action.notify {
                notifications.push(crate::osc_notify::Notification {
                    title: f.title().to_string(),
                    body: f.body(),
                    ..Default::default()
                });
            }
        }
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

        // A window `undo` just re-opened: put it back where it was. Sent as
        // viewport commands rather than built into `child_builder`, which is
        // rebuilt and diffed every pass — a position in there would be re-sent
        // forever and fight the user's next drag.
        if let Some((pos, size)) = self.place_geom.take() {
            ctx.send_viewport_cmd(egui::ViewportCommand::OuterPosition(pos));
            ctx.send_viewport_cmd(egui::ViewportCommand::InnerSize(size));
        }

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
                self.requests.push(AppRequest::CloseWindow { undoable: true });
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
                .title_override
                .clone()
                .or_else(|| self.tabs.get(self.active_tab).and_then(|t| t.focused_payload().title()))
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
                egui::Frame::side_top_panel(&ctx.global_style())
                    // A stable margin: the default `(8, 2)` leaves the tabs
                    // touching the strip's edges once they have a real height.
                    .inner_margin(egui::Margin::symmetric(6, 3))
                    .fill(egui::Color32::from_rgba_unmultiplied(
                        strip.r(),
                        strip.g(),
                        strip.b(),
                        a8,
                    )),
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
        // The inspector: not modal, so it goes *under* every dialog that is.
        self.render_inspector(&ctx);
        // Config errors: modal but merely informational, so *under* the
        // dialogs that are waiting on a decision.
        self.render_config_errors(&ctx, render_state);
        // The close confirmation draws over everything else.
        self.render_confirm_close(&ctx);
        // …and the clipboard permission prompt over that: it's the one dialog
        // whose answer can leak data or run a command, so nothing may sit on
        // top of it and take the click meant for "Deny".
        self.render_clipboard_confirm(&ctx);
        // Transient, non-interactive, and last so it is never hidden.
        self.render_toast(&ctx);

        std::mem::take(&mut self.requests)
    }
}

impl App {
    pub fn new(cc: &eframe::CreationContext<'_>) -> Result<Self> {
        let first = Window::first(cc, 0)?;
        let undo = crate::undo::UndoStack::new(first.config.undo_timeout_ms);
        let mut app = Self {
            windows: vec![first],
            focused: 0,
            next_window_id: 1,
            last_state: crate::state::SavedState::default(),
            global_actions: Vec::new(),
            global_chords: Vec::new(),
            undo,
        };
        app.restore_state();
        app.sync_global_binds(&cc.egui_ctx);
        Ok(app)
    }

    /// Rebuild the previous session's windows when `window-save-state = always`.
    ///
    /// The file is **consumed** — deleted as soon as it is read — because it
    /// describes one specific exit. Leaving it would resurrect that layout after
    /// a later crash that never got to write its own, which reads as giest
    /// ignoring everything the user has done since.
    fn restore_state(&mut self) {
        if !self.windows[0].config.window_save_state.restores() {
            return;
        }
        let saved = crate::state::load();
        crate::state::clear();
        let mut windows = saved.windows.iter();
        let Some(root) = windows.next() else {
            return;
        };
        self.windows[0].restore(root);
        for w in windows {
            let id = self.next_window_id;
            let Some(mut win) = self.windows[0].sibling(id, None) else {
                continue;
            };
            self.next_window_id += 1;
            win.restore(w);
            self.windows.push(win);
        }
    }

    /// Register the config's `global:` keybinds with the OS-level hook, if they
    /// changed since the last pass.
    ///
    /// Read from `windows[0]`: every window holds its own `Config` clone, but a
    /// global binding is a *process*-wide OS registration, so it needs one
    /// authority rather than the last window to be drawn.
    fn sync_global_binds(&mut self, ctx: &egui::Context) {
        let Some(w) = self.windows.first() else {
            return;
        };
        let globals = w.keymap.globals();
        let chords: Vec<crate::keybind::Chord> = globals.iter().map(|(c, _)| *c).collect();
        if chords == self.global_chords {
            return;
        }
        self.global_chords = chords;
        self.global_actions = globals.iter().map(|(_, a)| a.clone()).collect();
        // A chord whose key has no virtual-key code can't be registered; drop it
        // *and* its action together so the indices stay aligned with the hook's.
        let mut binds = Vec::new();
        let mut actions = Vec::new();
        for (chord, action) in globals {
            match crate::hotkey::GlobalBind::from_chord(chord) {
                Some(b) => {
                    binds.push(b);
                    actions.push(action.clone());
                }
                None => eprintln!("giest: global keybind key has no Windows virtual-key code"),
            }
        }
        self.global_actions = actions;
        crate::hotkey::set_binds(ctx, binds);
    }

    /// Run the actions of any global bindings that fired since the last pass.
    fn dispatch_global_binds(
        &mut self,
        ctx: &egui::Context,
        render_state: Option<&egui_wgpu::RenderState>,
    ) {
        for i in crate::hotkey::fired() {
            let Some(action) = self.global_actions.get(i).cloned() else {
                continue;
            };
            if action == Action::ToggleQuickTerminal {
                self.toggle_quick_terminal(ctx);
                continue;
            }
            // Everything else is a *window* action, and the focused window is
            // the only sensible target — including when giest isn't focused at
            // all, where "the window you last used" is what a user means.
            let idx = self.focused.min(self.windows.len().saturating_sub(1));
            if let Some(w) = self.windows.get_mut(idx) {
                w.execute_action(ctx, render_state, action);
            }
        }
    }

    /// Show or hide the quick terminal, creating it on first use.
    ///
    /// The window is an ordinary [`Window`] in the list — it has tabs, splits
    /// and the palette like any other — flagged so it gets the dropdown
    /// geometry and so hiding it doesn't retire its sessions. Hiding works by
    /// *not drawing the viewport*: eframe destroys the native window, while the
    /// `Window` (and every shell inside it) stays in the list, so reopening is
    /// instant and nothing in the terminal has moved.
    fn toggle_quick_terminal(&mut self, ctx: &egui::Context) {
        if let Some(i) = self.windows.iter().position(|w| w.quick) {
            let w = &mut self.windows[i];
            w.quick_visible = !w.quick_visible;
            if w.quick_visible {
                self.focused = i;
            }
            ctx.request_repaint();
            return;
        }
        let id = self.next_window_id;
        // Cloned from the focused window so it inherits the live config, the
        // profiles and the font metrics, exactly like `new_window`.
        let from = self.focused.min(self.windows.len().saturating_sub(1));
        let Some(src) = self.windows.get(from) else {
            return;
        };
        let Some(mut w) = src.sibling(id, None) else {
            return;
        };
        self.next_window_id += 1;
        w.quick = true;
        w.quick_visible = true;
        self.windows.push(w);
        self.focused = self.windows.len() - 1;
        ctx.request_repaint();
    }

    /// Hide the quick terminal when it loses focus (`quick-terminal-autohide`).
    ///
    /// Read from the window's *last observed* focus rather than this pass's
    /// input: `ui` runs from the root pass, where `i.focused` answers for the
    /// root viewport, not for the quick terminal's.
    fn autohide_quick_terminal(&mut self) {
        for w in &mut self.windows {
            if w.quick && w.quick_visible && w.config.quick_terminal_autohide && !w.was_focused {
                w.quick_visible = false;
            }
        }
    }

    /// Whether state saving is on. Read from the root window's config, which is
    /// the one a reload fans out from.
    fn saves_state(&self) -> bool {
        self.windows
            .first()
            .is_some_and(|w| w.config.window_save_state.restores())
    }

    /// Refresh [`App::last_state`] from the live windows.
    fn snapshot_state(&mut self) {
        if !self.saves_state() {
            // Cleared rather than left alone, so turning the key off mid-session
            // (a config reload) can't still write the layout it captured while
            // it was on.
            self.last_state = crate::state::SavedState::default();
            return;
        }
        self.last_state = crate::state::SavedState {
            // The quick terminal is excluded: it is summoned by a hotkey and has
            // its own chrome and geometry, so restoring it as an ordinary window
            // would hand the user a decorated window they never opened — the
            // same reason split zoom isn't saved.
            windows: self
                .windows
                .iter()
                .filter(|w| !w.quick)
                .map(Window::capture_state)
                .collect(),
        };
    }

    /// Write the saved layout on the way out. A no-op unless
    /// `window-save-state = always`, and an empty layout deletes the file rather
    /// than leaving a stale one.
    fn save_state(&self) {
        if !self.last_state.is_empty() {
            crate::state::save(&self.last_state);
        }
    }

    /// Push the aggregate `OSC 9;4` progress onto the taskbar button.
    ///
    /// There is **one** button for the process but any number of panes, so the
    /// states have to be merged. The rule is worst-news-wins: a failure is what
    /// you need to see, then a pause, then indeterminate work, and only then a
    /// plain percentage — which is the *lowest* of the running jobs, since the
    /// button should read "how far along is the slowest thing", not flicker
    /// between them. Ghostty sidesteps all of this by drawing per-surface.
    fn update_progress(&mut self) {
        use crate::taskbar::Progress;

        let dirty = self.windows.iter_mut().any(|w| std::mem::take(&mut w.progress_dirty));
        if !dirty {
            return;
        }
        let Some(hwnd) = self.windows.first().and_then(|w| w.hwnd) else {
            return;
        };

        let mut worst: Option<Progress> = None;
        for w in &mut self.windows {
            for tab in &mut w.tabs {
                // `for_each_mut` is the only traversal the split tree exposes;
                // this closure only reads.
                tab.root.for_each_mut(&mut |pane: &mut Session| {
                    let Some(p) = pane.progress() else { return };
                    worst = Some(match (worst, p) {
                        (None, p) => p,
                        // A failure outranks everything.
                        (Some(Progress::Error(a)), Progress::Error(b)) => Progress::Error(a.min(b)),
                        (Some(Progress::Error(a)), _) => Progress::Error(a),
                        (Some(_), Progress::Error(b)) => Progress::Error(b),
                        // Then a pause.
                        (Some(Progress::Paused(a)), Progress::Paused(b)) => {
                            Progress::Paused(a.min(b))
                        }
                        (Some(Progress::Paused(a)), _) => Progress::Paused(a),
                        (Some(_), Progress::Paused(b)) => Progress::Paused(b),
                        // Then unknown-length work, which can't be averaged in.
                        (Some(Progress::Indeterminate), _) | (_, Progress::Indeterminate) => {
                            Progress::Indeterminate
                        }
                        // Then the least-far-along determinate job.
                        (Some(Progress::Normal(a)), Progress::Normal(b)) => {
                            Progress::Normal(a.min(b))
                        }
                        (Some(Progress::Normal(a)), Progress::None) => Progress::Normal(a),
                        (Some(Progress::None), p) => p,
                    });
                });
            }
        }
        crate::taskbar::set(hwnd, worst.unwrap_or(Progress::None));
    }

    /// Raise the desktop notifications requested by any pane in any window
    /// during this pass's pump.
    ///
    /// App-scoped rather than per-window on purpose. There is exactly one
    /// notification-area entry for the process, and `Shell_NotifyIconW` just
    /// needs *a* window handle owned by this thread — but only the root window
    /// has a reachable one (a child viewport's is not exposed), so a per-window
    /// call would silently drop every notification from a secondary window.
    ///
    /// Unlike the bell there is no rate limit and no focus gate: OSC 9 / OSC 777
    /// are explicit requests from the program, and Ghostty shows them whatever
    /// the focus state. `MAX_BURST` is only a runaway guard — a program looping
    /// on OSC 9 could otherwise queue an unbounded stack of toasts.
    fn raise_notifications(&mut self, notifications: Vec<crate::osc_notify::Notification>) {
        /// Most notifications raised from a single pump. Windows coalesces
        /// balloons anyway, so beyond a handful they'd be invisible *and*
        /// expensive.
        const MAX_BURST: usize = 4;

        if notifications.is_empty() {
            return;
        }
        let Some(hwnd) = self.windows.first().and_then(|w| w.hwnd) else {
            return;
        };
        for n in notifications.iter().filter(|n| !n.is_empty()).take(MAX_BURST) {
            crate::notify::show(hwnd, &n.title, &n.body);
        }
    }

    /// Apply the app-scoped intents raised by this pass's windows. Runs after
    /// every window pass, so nothing is borrowed and the window list is free to
    /// grow or shrink.
    ///
    /// Requests are keyed by [`Window::window_id`], not by slot: retiring one
    /// window renumbers the rest, so an index captured during the pass would
    /// point at the wrong window by the time we got here.
    fn apply_requests(&mut self, ctx: &egui::Context, now: f64, requests: Vec<(u64, AppRequest)>) {
        for (id, req) in requests {
            match req {
                AppRequest::CloseWindow { undoable } => self.retire(ctx, now, id, undoable),
                AppRequest::NewWindow(cwd) => self.spawn_window(ctx, now, cwd.as_deref()),
                AppRequest::ToggleQuickTerminal => self.toggle_quick_terminal(ctx),
                AppRequest::Record(op) => self.undo.record(now, op),
                AppRequest::Undo => self.undo_or_redo(ctx, now, false),
                AppRequest::Redo => self.undo_or_redo(ctx, now, true),
            }
        }
    }

    /// Ghostty `undo` / `redo`: take the newest entry off the stack, apply it,
    /// and file the operation that reverses *that* onto the opposite stack.
    ///
    /// The inverse comes from applying, not from a table — [`App::apply_undo_op`]
    /// returns it — so undo and redo are one implementation walked in either
    /// direction. An op that can no longer apply (its window closed in the
    /// meantime) records nothing and is simply gone.
    fn undo_or_redo(&mut self, ctx: &egui::Context, now: f64, redo: bool) {
        let op = if redo {
            self.undo.begin_redo(now)
        } else {
            self.undo.begin_undo(now)
        };
        if let Some(op) = op {
            if let Some(inverse) = self.apply_undo_op(ctx, op) {
                self.undo.record(now, inverse);
            }
            ctx.request_repaint();
        }
        // Unconditional: leaving the stack in its undoing phase would file the
        // user's next ordinary close as a *redo*.
        self.undo.end();
    }

    fn window_mut(&mut self, id: u64) -> Option<&mut Window> {
        self.windows.iter_mut().find(|w| w.window_id == id)
    }

    /// Apply one undoable operation, returning the operation that undoes it.
    ///
    /// Every arm is the exact inverse of the one paired with it, which is what
    /// lets an entry bounce between the two stacks indefinitely. `None` means
    /// the op no longer applies — its window or tab is gone — and the entry
    /// dies with whatever it was holding.
    fn apply_undo_op(&mut self, ctx: &egui::Context, op: UndoOp) -> Option<UndoOp> {
        match op {
            UndoOp::RestorePane {
                window,
                tab,
                slot,
                node,
            } => {
                let w = self.window_mut(window)?;
                let i = w.tabs.iter().position(|t| t.id == tab)?;
                let leaf = node.first_leaf_id();
                w.tabs[i].root.attach_at(&slot, *node);
                // Focus the pane that just came back, and surface its tab: an
                // undo the user cannot see is indistinguishable from one that
                // did nothing.
                w.tabs[i].focus = leaf;
                w.tabs[i].zoomed = None;
                w.active_tab = i;
                Some(UndoOp::RemovePane { window, tab, leaf })
            }
            UndoOp::RemovePane { window, tab, leaf } => {
                let w = self.window_mut(window)?;
                let i = w.tabs.iter().position(|t| t.id == tab)?;
                // The last pane in a tab isn't a pane operation at all — it is
                // the tab. Rather than silently widening the op, decline: the
                // entry only ever described one split.
                if w.tabs[i].leaf_count() <= 1 {
                    return None;
                }
                let root = std::mem::replace(&mut w.tabs[i].root, Node::Empty);
                let (rest, taken) = root.detach_leaf(leaf);
                w.tabs[i].root = rest.unwrap_or(Node::Empty);
                w.tabs[i].focus = w.tabs[i].root.first_leaf_id();
                w.tabs[i].zoomed = None;
                w.active_tab = i;
                let (slot, node) = taken?;
                Some(UndoOp::RestorePane {
                    window,
                    tab,
                    slot,
                    node: Box::new(node),
                })
            }
            UndoOp::RestoreTabs {
                window,
                tabs,
                active,
            } => {
                let w = self.window_mut(window)?;
                let (ids, active) = reinsert_tabs(&mut w.tabs, tabs, active);
                w.active_tab = active;
                // Mid-list inserts move every later tab; anything holding an
                // index is now pointing at the wrong one.
                w.renaming = None;
                w.tab_drag = None;
                Some(UndoOp::RemoveTabs { window, ids })
            }
            UndoOp::RemoveTabs { window, ids } => {
                let w = self.window_mut(window)?;
                let active = w.active_tab;
                let (tabs, now_active) = remove_tabs_by_id(&mut w.tabs, &ids, active)?;
                w.active_tab = now_active;
                w.renaming = None;
                w.tab_drag = None;
                // The op that puts them back carries the selection from *before*
                // this removal, not the clamped one — that is what the restore
                // is undoing.
                Some(UndoOp::RestoreTabs {
                    window,
                    tabs,
                    active,
                })
            }
            UndoOp::RestoreWindow { window } => {
                let mut w = *window;
                let id = w.window_id;
                // Slot 0 belongs to whichever window holds it now: the root
                // viewport was rehosted when this one closed, and taking it back
                // would move a window the user never asked to move.
                w.is_root = false;
                w.place_geom = w.geom;
                // Clear the close latches the window was carrying when it went.
                // `closing` is what stops a confirmed close's own
                // `ViewportCommand::Close` re-opening the dialog forever — left
                // set on a restored window it would swallow every *later* close
                // request too, leaving a window the × can't shut.
                w.closing = false;
                w.confirm = None;
                self.windows.push(w);
                ctx.request_repaint();
                Some(UndoOp::RemoveWindow { window: id })
            }
            UndoOp::RemoveWindow { window } => {
                // Closing the last window quits giest, and an undo must never be
                // able to end the process.
                if self.windows.len() <= 1 {
                    return None;
                }
                let w = self.take_window(ctx, window)?;
                Some(UndoOp::RestoreWindow {
                    window: Box::new(w),
                })
            }
        }
    }

    /// Open a new window, cloned from the focused one (falling back to the root)
    /// so it inherits the live config, profiles and font metrics.
    fn spawn_window(&mut self, ctx: &egui::Context, now: f64, cwd: Option<&std::path::Path>) {
        let from = self.focused.min(self.windows.len().saturating_sub(1));
        let Some(src) = self.windows.get(from) else {
            return;
        };
        let id = self.next_window_id;
        // Only bump the counter on success, so a failed spawn doesn't burn an id.
        if let Some(w) = src.sibling(id, cwd) {
            self.next_window_id += 1;
            self.windows.push(w);
            // Upstream registers an undo for `new_window` too — undoing a
            // creation closes it, through the same path a redo of a close uses.
            self.undo.record(now, UndoOp::RemoveWindow { window: id });
            ctx.request_repaint();
        }
    }

    /// Close the window with `id`, quitting giest when it was the last one.
    ///
    /// `undoable` is what separates a user's close from a window whose last
    /// shell exited: the first keeps the window (and its running panes) alive in
    /// an undo entry, the second has nothing left to keep.
    fn retire(&mut self, ctx: &egui::Context, now: f64, id: u64, undoable: bool) {
        let Some(window) = self.take_window(ctx, id) else {
            return;
        };
        // Never after the *last* window: the process is on its way out, and an
        // entry would only hold its shells open through the shutdown.
        if undoable && !self.windows.is_empty() {
            self.undo.record(
                now,
                UndoOp::RestoreWindow {
                    window: Box::new(window),
                },
            );
        }
    }

    /// Remove the window with `id` and hand it back, still whole.
    ///
    /// The retire path proper, shared with `undo`: dropping the returned window
    /// is an ordinary close, keeping it is an undoable one. Closing the last
    /// window still ends the process — that decision belongs to the removal, not
    /// to what the caller does with the result.
    fn take_window(&mut self, ctx: &egui::Context, id: u64) -> Option<Window> {
        let idx = self.windows.iter().position(|w| w.window_id == id)?;
        // Capture *before* the removal: closing the last window is how giest
        // quits, so this is the only moment the exiting layout still exists.
        self.snapshot_state();
        // Remember where the surviving root-slot window is on screen *before*
        // the move, so a rehost can put the root native window there.
        let rehost_to = (idx == 0).then(|| self.windows.get(1).and_then(|w| w.geom)).flatten();

        let windows = std::mem::take(&mut self.windows);
        let (windows, focused, removed) = retire_window(windows, idx, self.focused);
        self.windows = windows;
        self.focused = focused;

        if self.windows.is_empty() {
            // The last window went: closing the root viewport ends the process.
            ctx.send_viewport_cmd_to(egui::ViewportId::ROOT, egui::ViewportCommand::Close);
            return removed;
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
        removed
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

    /// Remove the notification-area icon on the way out.
    ///
    /// The shell would eventually garbage-collect a stale one, but only when the
    /// user next hovers the tray — until then giest appears to still be running.
    /// A no-op unless something actually notified (see [`crate::notify`]).
    fn on_exit(&mut self) {
        // A window still standing means the process is going down some other way
        // (an OS shutdown, a `quit` that skipped the close path) — snapshot it.
        if !self.windows.is_empty() {
            self.snapshot_state();
        }
        self.save_state();
        if let Some(hwnd) = self.windows.first().and_then(|w| w.hwnd) {
            crate::notify::shutdown(hwnd);
        }
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
        let mut notifications = Vec::new();
        for w in &mut self.windows {
            w.pump_all(now, &mut notifications);
        }

        // Retire undo entries every pass, not only when undo is used: an expired
        // entry is holding shells open, and they should end when it does. The
        // timeout is re-read here rather than pushed on reload — `reload_config`
        // is a `Window` method and the stack is the app's.
        if let Some(w) = self.windows.first() {
            self.undo.set_timeout(w.config.undo_timeout_ms);
        }
        self.undo.expire(now);
        let undo_state = crate::command::UndoState {
            can_undo: self.undo.can_undo(),
            can_redo: self.undo.can_redo(),
        };
        for w in &mut self.windows {
            w.undo_state = undo_state;
        }
        self.raise_notifications(notifications);
        self.update_progress();

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
            // A hidden quick terminal: skipping the viewport is what *closes*
            // the native window (a child ignores `ViewportCommand::Close`), and
            // its sessions keep running because the `Window` stays in the list.
            if w.quick && !w.quick_visible {
                continue;
            }
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

        self.apply_requests(&ctx, now, requests);
        // After the requests, so a `global:` binding sees the layout this pass
        // produced rather than the previous one's.
        self.sync_global_binds(&ctx);
        self.dispatch_global_binds(&ctx, render_state.as_ref());
        self.autohide_quick_terminal();
    }
}

/// Drop window `idx`, returning the surviving list and the remapped focused
/// index so the *same* window stays focused where possible.
///
/// Split out from [`App`] — like `reap_tabs` and `reorder_tabs` — so the
/// reselection logic is testable without spawning a shell. Out-of-range `idx` is
/// a no-op rather than a panic, matching the other window/tab helpers.
fn retire_window<W>(
    mut windows: Vec<W>,
    idx: usize,
    focused: usize,
) -> (Vec<W>, usize, Option<W>) {
    if idx >= windows.len() {
        return (windows, focused, None);
    }
    // Handed back, not dropped: an undoable close keeps the window (and every
    // shell in it) alive in the undo entry.
    let removed = windows.remove(idx);
    if windows.is_empty() {
        return (windows, 0, Some(removed));
    }
    // A focus past the removed slot shifts down; a focus *on* it lands on the
    // window that took its place (clamped at the end).
    let focused = if focused > idx { focused - 1 } else { focused };
    let last = windows.len() - 1;
    (windows, focused.min(last), Some(removed))
}

/// Apply `window-padding-balance` to a pane's grid rect.
///
/// The grid fits a whole number of cells, so dividing the pane leaves a
/// remainder on each axis — by default all of it sits to the right of and below
/// the text. Balancing moves half of it to the other side. Cell metrics are in
/// physical pixels while the rect is in points, hence the `ppp` conversion; the
/// arithmetic itself lives in `config::balance_padding`, where it is table-tested.
fn balance_pane(
    rect: egui::Rect,
    mode: crate::config::PaddingBalance,
    cell_w: f32,
    cell_h: f32,
    ppp: f32,
) -> egui::Rect {
    if mode == crate::config::PaddingBalance::None {
        return rect;
    }
    let (cw, ch) = ((cell_w / ppp).max(1.0), (cell_h / ppp).max(1.0));
    let (lx, _) = crate::config::balance_padding(mode, rect.width() % cw, cw);
    let (ly, _) = crate::config::balance_padding(mode, rect.height() % ch, ch);
    egui::Rect::from_min_max(rect.min + egui::vec2(lx, ly), rect.max)
}

/// Where a new tab is inserted, given `window-new-tab-position`.
///
/// `current` means *after* the focused tab, not at it — a pure function so the
/// off-by-one has a test rather than a comment. Clamped to `len`, so it is
/// always a valid `Vec::insert` index even if the focus is somehow stale.
fn new_tab_index(pos: crate::config::NewTabPosition, active: usize, len: usize) -> usize {
    match pos {
        crate::config::NewTabPosition::End => len,
        crate::config::NewTabPosition::Current => (active + 1).min(len),
    }
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
        families: config.font_family.clone(),
        family_bold: config.font_family_bold.clone(),
        family_italic: config.font_family_italic.clone(),
        family_bold_italic: config.font_family_bold_italic.clone(),
        features: config.font_features.clone(),
        styles: config.font_styles.clone(),
        variations: config.font_variations.clone(),
        adjust: config.adjust,
        synthetic: config.font_synthetic_style,
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

/// Nominal width of a split gutter, in logical points.
const SPLIT_GUTTER_PT: f32 = 1.0;

/// Round a logical-point coordinate to a device-pixel boundary.
fn snap(v: f32, ppp: f32) -> f32 {
    (v * ppp).round() / ppp
}

/// Split `area` into two halves along one axis, returning
/// `(first, gutter, second)`.
///
/// `vertical` = a vertical divider, i.e. side-by-side columns (Ctrl+Shift+O);
/// otherwise stacked rows (Ctrl+Shift+E).
///
/// Both the gutter's width and the boundary between the halves are snapped to
/// device pixels. A nominal 1pt gutter is *not* one pixel at fractional scaling:
/// left as a raw float it lands between pixels and rasterizes at 0, 1 or 2 px
/// depending on where the split happens to fall, so a divider could vanish
/// entirely on one split and read as a fat seam on another.
///
/// The trailing half is built with `from_min_max` so it ends exactly on the
/// parent's edge. Sizing both halves independently (as this used to) leaves a
/// sub-pixel sliver at the far edge whenever the arithmetic doesn't divide
/// evenly — and once the boundary is snapped, it never does.
fn split_rect(area: egui::Rect, vertical: bool, ppp: f32) -> (egui::Rect, egui::Rect, egui::Rect) {
    let ppp = ppp.max(1.0);
    // At least one whole device pixel, so the gutter is always visible.
    let gap = ((SPLIT_GUTTER_PT * ppp).round().max(1.0)) / ppp;
    if vertical {
        let mid = snap(area.min.x + ((area.width() - gap) * 0.5).max(1.0), ppp)
            .clamp(area.min.x, (area.max.x - gap).max(area.min.x));
        (
            egui::Rect::from_min_max(area.min, egui::pos2(mid, area.max.y)),
            egui::Rect::from_min_max(
                egui::pos2(mid, area.min.y),
                egui::pos2(mid + gap, area.max.y),
            ),
            egui::Rect::from_min_max(egui::pos2(mid + gap, area.min.y), area.max),
        )
    } else {
        let mid = snap(area.min.y + ((area.height() - gap) * 0.5).max(1.0), ppp)
            .clamp(area.min.y, (area.max.y - gap).max(area.min.y));
        (
            egui::Rect::from_min_max(area.min, egui::pos2(area.max.x, mid)),
            egui::Rect::from_min_max(
                egui::pos2(area.min.x, mid),
                egui::pos2(area.max.x, mid + gap),
            ),
            egui::Rect::from_min_max(egui::pos2(area.min.x, mid + gap), area.max),
        )
    }
}

/// Fit `title + suffix` into `max` logical points, truncating the *title* with
/// an ellipsis and always keeping the suffix.
///
/// Measured rather than counted. The chrome font is proportional, so a character
/// cap (what this used to be) is only right for one string: 24 chars of `W` and
/// 24 of `i` differ by more than a tab's width, so the same cap both overflowed
/// wide titles and wasted most of the tab on narrow ones.
///
/// The suffix is the split count (`" [3]"`), which is information the ellipsis
/// must not eat — it is what tells you the tab has hidden panes.
/// `width` measures a string in the font the label will be drawn in; it is
/// injected rather than taken from a `Ui` so the fitting logic is testable
/// without a font atlas (a headless egui context measures everything as zero).
fn truncate_to_width(title: &str, suffix: &str, max: f32, w: impl Fn(&str) -> f32) -> String {
    let full = format!("{title}{suffix}");
    if w(&full) <= max {
        return full;
    }
    let budget = (max - w(suffix) - w("…")).max(0.0);
    // Longest prefix that fits. Linear over chars: a tab title is short, and
    // this runs once per tab per frame.
    let mut out = String::new();
    for c in title.chars() {
        let mut probe = out.clone();
        probe.push(c);
        if w(&probe) > budget {
            break;
        }
        out = probe;
    }
    format!("{}…{suffix}", out.trim_end())
}

#[cfg(test)]
mod tests {
    use super::{
        Dir, Node, Tab, capture_node_with, cycle_pick, dim_alpha, drop_index, highlight_job,
        keep_only_tab, nav_dir, new_tab_index,
        overlay_anchor, preview_text, reap_tabs, reinsert_tabs, remove_tabs_by_id, reorder_tabs,
        retire_window, split_rect,
        truncate_tabs_to_right, truncate_to_width,
    };
    use crate::config::ResizeOverlayPosition as P;
    use eframe::egui;
    use eframe::egui::Align2;

    #[test]
    fn retire_window_drops_it_and_remaps_focus() {
        let w = || vec![10, 11, 12, 13];
        // Dropping a window before the focused one shifts focus down by one, so
        // the *same* window stays focused.
        let (v, f, out) = retire_window(w(), 0, 2);
        assert_eq!(v, vec![11, 12, 13]);
        assert_eq!(v[f], 12);
        // The removed window is handed back, not dropped — that is what lets an
        // undoable close keep it (and its shells) alive.
        assert_eq!(out, Some(10));
        // Dropping one after it leaves focus alone.
        let (v, f, out) = retire_window(w(), 3, 1);
        assert_eq!(v, vec![10, 11, 12]);
        assert_eq!(v[f], 11);
        assert_eq!(out, Some(13));
        // Dropping the focused window lands on whatever took its slot.
        let (v, f, _) = retire_window(w(), 1, 1);
        assert_eq!(v, vec![10, 12, 13]);
        assert_eq!(v[f], 12);
        // Dropping the focused *last* window clamps back onto the new last.
        let (v, f, _) = retire_window(w(), 3, 3);
        assert_eq!(v, vec![10, 11, 12]);
        assert_eq!(v[f], 12);
    }

    #[test]
    fn retire_window_on_the_last_one_empties_the_list() {
        // An empty list is how `App` knows to quit giest.
        let (v, f, out) = retire_window(vec![10], 0, 0);
        assert!(v.is_empty());
        assert_eq!(f, 0);
        assert_eq!(out, Some(10));
    }

    #[test]
    fn retire_window_ignores_out_of_range() {
        let (v, f, out) = retire_window(vec![10, 11], 9, 1);
        assert_eq!(v, vec![10, 11]);
        assert_eq!(f, 1);
        assert_eq!(out, None);
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
    fn new_tab_index_inserts_after_the_focused_tab_or_at_the_end() {
        use crate::config::NewTabPosition::{Current, End};
        // `current` means *after* the focused tab, not at it.
        assert_eq!(new_tab_index(Current, 0, 3), 1);
        assert_eq!(new_tab_index(Current, 2, 3), 3);
        assert_eq!(new_tab_index(End, 0, 3), 3);
        assert_eq!(new_tab_index(End, 2, 3), 3);
        // Both are valid `Vec::insert` indices even from an empty list or a
        // stale focus, so a mid-list insert can never panic.
        assert_eq!(new_tab_index(Current, 0, 0), 0);
        assert_eq!(new_tab_index(Current, 9, 3), 3);
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
    /// The tree's shape and leaf ids as a string, so two trees can be compared
    /// without `Node` needing `PartialEq` over a payload it is generic in.
    fn shape(n: &Node<u32>) -> String {
        match n {
            Node::Leaf { id, .. } => format!("{id}"),
            Node::Split {
                vertical,
                first,
                second,
            } => format!(
                "({}{}{})",
                shape(first),
                if *vertical { "|" } else { "/" },
                shape(second)
            ),
            Node::Empty => "_".into(),
        }
    }

    fn split(vertical: bool, first: Node<u32>, second: Node<u32>) -> Node<u32> {
        Node::Split {
            vertical,
            first: Box::new(first),
            second: Box::new(second),
        }
    }

    #[test]
    fn capture_node_mirrors_the_tree_and_marks_the_focused_leaf() {
        // The payload stands in for a session's cwd: pane 7 reported one, 3 didn't.
        let tree = split(true, leaf(3, 0), split(false, leaf(7, 1), leaf(9, 0)));
        let cwd = |p: &u32| (*p == 1).then(|| r"C:\src".to_string());
        let got = capture_node_with(&tree, 7, &cwd);

        let crate::state::SavedNode::Split {
            vertical,
            first,
            second,
        } = got
        else {
            panic!("root must stay a split");
        };
        assert!(vertical);
        assert_eq!(
            *first,
            crate::state::SavedNode::Leaf {
                cwd: None,
                focused: false
            }
        );
        let crate::state::SavedNode::Split {
            vertical,
            first: inner,
            second: last,
        } = *second
        else {
            panic!("nested split must survive");
        };
        assert!(!vertical, "the inner axis is independent of the outer one");
        assert_eq!(
            *inner,
            crate::state::SavedNode::Leaf {
                cwd: Some(r"C:\src".into()),
                focused: true
            }
        );
        assert_eq!(
            *last,
            crate::state::SavedNode::Leaf {
                cwd: None,
                focused: false
            }
        );
    }

    #[test]
    fn capture_node_marks_nothing_when_the_focus_id_is_gone() {
        let got = capture_node_with(&leaf(3, 0), 99, &|_: &u32| None);
        assert_eq!(
            got,
            crate::state::SavedNode::Leaf {
                cwd: None,
                focused: false
            },
            "a stale focus id must not silently focus a different pane"
        );
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
            id: 1,
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
            id: 1,
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
    fn detach_leaf_collapses_into_sibling_and_hands_the_pane_back() {
        let root = split(true, leaf(1, 1), leaf(2, 1));
        let (after, taken) = root.detach_leaf(2);
        let after = after.expect("one leaf survives");
        assert_eq!(after.leaf_count(), 1);
        assert!(after.contains(1) && !after.contains(2));
        // The removed pane is *handed back*, not dropped — that is what keeps
        // its shell running inside the undo entry.
        let (slot, node) = taken.expect("the closed pane comes back");
        assert!(node.contains(2));
        assert_eq!(slot.path, Vec::<bool>::new());
        assert!(slot.vertical);
        assert!(!slot.first); // it was the *second* child
    }

    #[test]
    fn detach_then_attach_restores_the_original_tree() {
        // A nested tree, so the slot's path has to carry more than one step:
        // [1 | (2 / 3)]. Closing 2 collapses its split into 3.
        let build = || split(true, leaf(1, 1), split(false, leaf(2, 1), leaf(3, 1)));
        let (after, taken) = build().detach_leaf(2);
        let mut after = after.expect("survivors");
        let (slot, node) = taken.expect("the closed pane");
        // The parent split was the root's second child, one step down.
        assert_eq!(slot.path, vec![false]);
        assert!(!slot.vertical);
        assert!(slot.first);
        // Undo: put it back, and the tree is indistinguishable from the original.
        after.attach_at(&slot, node);
        assert_eq!(shape(&after), shape(&build()));
    }

    #[test]
    fn attach_at_falls_back_to_the_deepest_reachable_point() {
        // The layout changed under the undo entry: the path names a split that
        // is now a bare leaf. Restoring in the wrong place beats dropping a
        // running shell, so it attaches where it can.
        let (after, taken) = split(true, leaf(1, 1), split(false, leaf(2, 1), leaf(3, 1)))
            .detach_leaf(2);
        let (slot, node) = taken.expect("the closed pane");
        let mut other = split(true, leaf(9, 1), leaf(8, 1));
        drop(after);
        other.attach_at(&slot, node);
        assert_eq!(other.leaf_count(), 3);
        assert!(other.contains(2));
    }

    #[test]
    fn detach_leaf_declines_a_bare_leaf() {
        // The last pane in a tab isn't a split; the caller closes the tab.
        let (after, taken) = leaf(1, 1).detach_leaf(1);
        assert!(taken.is_none());
        assert_eq!(after.expect("unchanged").leaf_count(), 1);
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
    fn preview_makes_control_characters_visible() {
        // The newline is *why* the user is being asked, so it must be legible
        // rather than laid out as ordinary wrapped text.
        assert_eq!(preview_text("ls\nrm -rf /", 100), "ls⏎\nrm -rf /");
        // An escape can't be passed through to dress the payload up as dialog
        // chrome — nor can any other control byte.
        assert_eq!(preview_text("a\x1b[201~b", 100), "a␛[201~b");
        assert_eq!(preview_text("a\x07\x00b", 100), "a␦␦b");
        assert_eq!(preview_text("a\tb\rc", 100), "a→b␍c");
        // Ordinary text is untouched.
        assert_eq!(preview_text("hello world", 100), "hello world");
    }

    #[test]
    fn preview_caps_length_and_says_how_much_is_hidden() {
        let long = "x".repeat(50);
        let out = preview_text(&long, 10);
        assert!(out.starts_with("xxxxxxxxxx"), "{out}");
        assert!(out.ends_with("… 40 more characters"), "{out}");
        // Exactly at the limit, nothing is elided.
        assert_eq!(preview_text("abcde", 5), "abcde");
        // Counted in characters, not bytes, so multi-byte text isn't cut short.
        assert_eq!(preview_text("äöüßé", 5), "äöüßé");
    }

    #[test]
    fn find_leaf_returns_the_first_match_in_layout_order() {
        let root = split(true, leaf(1, 10), split(false, leaf(2, 20), leaf(3, 20)));
        assert_eq!(root.find_leaf(&mut |p| *p == 20), Some(2));
        assert_eq!(root.find_leaf(&mut |p| *p == 10), Some(1));
        assert_eq!(root.find_leaf(&mut |p| *p == 99), None);
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
        root.collect(area, 1.0, &mut leaves);
        assert_eq!(leaves.len(), 2);
        assert_eq!(leaves[0].id, 1);
        assert_eq!(leaves[1].id, 2);
        // (101 - 1 gutter) / 2 = 50 per column.
        assert!((leaves[0].rect.width() - 50.0).abs() < 0.01);
        assert!(leaves[1].rect.min.x >= leaves[0].rect.max.x);
    }

    /// The gutters must line up exactly with the space `collect` leaves between
    /// panes — they are painted, so a mismatch shows as a seam or an overlap.
    #[test]
    fn gutters_fill_the_space_between_panes() {
        let mut root = split(true, leaf(1, 1), split(false, leaf(2, 1), leaf(3, 1)));
        let area = egui::Rect::from_min_size(egui::pos2(0.0, 0.0), egui::vec2(400.0, 300.0));
        for ppp in [1.0, 1.25, 1.5, 2.0] {
            let mut leaves = Vec::new();
            let mut gutters = Vec::new();
            root.gutters(area, ppp, &mut gutters);
            root.collect(area, ppp, &mut leaves);
            assert_eq!(leaves.len(), 3);
            assert_eq!(gutters.len(), 2, "one gutter per split");
            // The vertical gutter abuts both columns exactly.
            assert!((gutters[0].left() - leaves[0].rect.right()).abs() < 1e-4);
            assert!((gutters[0].right() - leaves[1].rect.left()).abs() < 1e-4);
            for g in &gutters {
                let px = g.width().min(g.height()) * ppp;
                assert!(
                    (px - px.round()).abs() < 1e-3 && px >= 1.0,
                    "gutter is {px} device px at ppp {ppp}; must be a whole pixel"
                );
            }
        }
    }

    #[test]
    fn split_rect_halves_each_axis() {
        let area = egui::Rect::from_min_size(egui::pos2(0.0, 0.0), egui::vec2(101.0, 51.0));
        let (a, g, b) = split_rect(area, true, 1.0); // columns
        assert!((a.width() - 50.0).abs() < 0.01);
        assert!((b.width() - 50.0).abs() < 0.01);
        assert_eq!(a.height(), 51.0);
        assert_eq!(g.width(), 1.0);
        let (c, _, _) = split_rect(area, false, 1.0); // rows
        assert!((c.height() - 25.0).abs() < 0.01);
        assert_eq!(c.width(), 101.0);
    }

    /// The two halves plus the gutter must cover the parent exactly. Sizing each
    /// half independently (the old shape) left a sub-pixel sliver at the far
    /// edge whenever the arithmetic didn't divide evenly — and once the boundary
    /// is snapped to device pixels, it never does.
    #[test]
    fn split_rect_covers_the_parent_exactly() {
        for ppp in [1.0, 1.25, 1.5, 2.0] {
            for extent in [100.0, 101.0, 137.5, 401.0] {
                let area =
                    egui::Rect::from_min_size(egui::pos2(3.0, 7.0), egui::vec2(extent, extent));
                let (a, g, b) = split_rect(area, true, ppp);
                assert_eq!(a.left(), area.left());
                assert_eq!(b.right(), area.right());
                assert_eq!(a.right(), g.left());
                assert_eq!(g.right(), b.left());

                let (a, g, b) = split_rect(area, false, ppp);
                assert_eq!(a.top(), area.top());
                assert_eq!(b.bottom(), area.bottom());
                assert_eq!(a.bottom(), g.top());
                assert_eq!(g.bottom(), b.top());
            }
        }
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
            id: 1,
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
        let (survivors, active, closed) = keep_only_tab(tabs, 1, 2);
        assert_eq!(survivors.len(), 1);
        assert_eq!(active, 0);
        assert_eq!(survivors[0].focus, 2); // the kept tab's leaf id
        // What was closed comes back with the slot each tab came from, in
        // ascending order — the shape `undo` re-inserts from.
        assert_eq!(closed.iter().map(|(i, t)| (*i, t.id)).collect::<Vec<_>>(), vec![(0, 1), (2, 3)]);
    }

    #[test]
    fn keep_only_tab_is_noop_when_out_of_range() {
        let tabs = vec![Tab::leaf(1, 0u32), Tab::leaf(2, 0u32)];
        let (survivors, active, closed) = keep_only_tab(tabs, 5, 1);
        assert_eq!(survivors.len(), 2);
        assert_eq!(active, 1);
        assert!(closed.is_empty());
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
        let (survivors, active, closed) = truncate_tabs_to_right(tabs, 1, 3);
        assert_eq!(survivors.len(), 2);
        assert_eq!(active, 1);
        assert_eq!(closed.iter().map(|(i, t)| (*i, t.id)).collect::<Vec<_>>(), vec![(2, 3), (3, 4)]);
    }

    #[test]
    fn truncate_tabs_to_right_keeps_active_when_left_of_cut() {
        let tabs = vec![Tab::leaf(1, 0u32), Tab::leaf(2, 0u32), Tab::leaf(3, 0u32)];
        let (survivors, active, closed) = truncate_tabs_to_right(tabs, 1, 0);
        assert_eq!(survivors.len(), 2);
        assert_eq!(active, 0);
        assert_eq!(closed.len(), 1);
    }

    #[test]
    fn truncate_tabs_to_right_is_noop_at_last_tab() {
        let tabs = vec![Tab::leaf(1, 0u32), Tab::leaf(2, 0u32)];
        let (survivors, active, closed) = truncate_tabs_to_right(tabs, 1, 1);
        assert_eq!(survivors.len(), 2);
        assert_eq!(active, 1);
        assert!(closed.is_empty());
    }

    #[test]
    fn closing_and_reinserting_tabs_is_a_round_trip() {
        // "Close other tabs" on the middle of three, then undo. The kept tab has
        // to end up selected *and* back in the middle.
        let tabs = vec![Tab::leaf(1, 0u32), Tab::leaf(2, 0u32), Tab::leaf(3, 0u32)];
        let was_active = 1;
        let (mut survivors, active, closed) = keep_only_tab(tabs, 1, was_active);
        assert_eq!(active, 0);
        let (ids, active) = reinsert_tabs(&mut survivors, closed, was_active);
        assert_eq!(ids, vec![1, 3]);
        assert_eq!(
            survivors.iter().map(|t| t.id).collect::<Vec<_>>(),
            vec![1, 2, 3]
        );
        assert_eq!(active, 1); // still the tab that was kept
    }

    #[test]
    fn reinserting_tabs_closed_from_the_right_restores_the_order() {
        let tabs = vec![
            Tab::leaf(1, 0u32),
            Tab::leaf(2, 0u32),
            Tab::leaf(3, 0u32),
            Tab::leaf(4, 0u32),
        ];
        let (mut survivors, _, closed) = truncate_tabs_to_right(tabs, 1, 3);
        let (ids, active) = reinsert_tabs(&mut survivors, closed, 3);
        assert_eq!(ids, vec![3, 4]);
        assert_eq!(
            survivors.iter().map(|t| t.id).collect::<Vec<_>>(),
            vec![1, 2, 3, 4]
        );
        assert_eq!(active, 3);
    }

    #[test]
    fn remove_tabs_by_id_is_the_inverse_of_reinsert() {
        let mut tabs = vec![Tab::leaf(1, 0u32), Tab::leaf(2, 0u32), Tab::leaf(3, 0u32)];
        // Redo of the "close other tabs" undo above: take 1 and 3 back out.
        let (closed, active) = remove_tabs_by_id(&mut tabs, &[1, 3], 1).expect("two of three");
        assert_eq!(
            closed.iter().map(|(i, t)| (*i, t.id)).collect::<Vec<_>>(),
            vec![(0, 1), (2, 3)]
        );
        assert_eq!(tabs.iter().map(|t| t.id).collect::<Vec<_>>(), vec![2]);
        assert_eq!(active, 0);
        // …and putting them back lands exactly where they started.
        let (_, active) = reinsert_tabs(&mut tabs, closed, 1);
        assert_eq!(tabs.iter().map(|t| t.id).collect::<Vec<_>>(), vec![1, 2, 3]);
        assert_eq!(active, 1);
    }

    #[test]
    fn remove_tabs_by_id_declines_to_empty_the_window() {
        // Emptying a window is a *window* close: a different op with its own
        // entry, so this one refuses rather than escalating.
        let mut tabs = vec![Tab::leaf(1, 0u32)];
        assert!(remove_tabs_by_id(&mut tabs, &[1], 0).is_none());
        assert_eq!(tabs.len(), 1);
        // An id that is already gone is skipped, not an error…
        let mut tabs = vec![Tab::leaf(1, 0u32), Tab::leaf(2, 0u32)];
        let (closed, _) = remove_tabs_by_id(&mut tabs, &[2, 99], 0).expect("one known id");
        assert_eq!(closed.len(), 1);
        // …but if *every* id is gone there is nothing to do.
        assert!(remove_tabs_by_id(&mut tabs, &[99], 0).is_none());
    }

    #[test]
    fn truncate_to_width_fits_and_keeps_the_suffix() {
        // A proportional metric, so the test exercises the thing a character
        // count got wrong: `W` is four times the width of `i`.
        let w = |s: &str| {
            s.chars()
                .map(|c| match c {
                    'W' | 'M' => 4.0,
                    'i' | 'l' | ' ' => 1.0,
                    '…' => 2.0,
                    _ => 2.0,
                })
                .sum::<f32>()
        };

        // Fits: returned whole, with the suffix.
        assert_eq!(truncate_to_width("pwsh", " [2]", 400.0, w), "pwsh [2]");

        // Doesn't fit: truncated, ellipsis added, suffix kept — and the result
        // actually fits the budget it was given.
        let long = "WWWWWWWWWW";
        let out = truncate_to_width(long, " [3]", 20.0, w);
        assert!(out.ends_with("… [3]"), "got {out:?}");
        assert!(w(&out) <= 20.0, "{out:?} is {} wide, budget 20", w(&out));

        // Same budget, narrow glyphs → more characters survive. This is exactly
        // what a character cap could not express.
        let narrow = truncate_to_width("iiiiiiiiiiiiiiiiiiii", " [3]", 20.0, w);
        assert!(
            narrow.chars().count() > out.chars().count(),
            "narrow {narrow:?} should keep more chars than wide {out:?}"
        );

        // The split-count suffix survives even a budget too small for it: it is
        // what tells you the tab has hidden panes.
        assert!(truncate_to_width(long, " [9]", 1.0, w).ends_with(" [9]"));
    }

    /// The palette highlights fuzzy matches by re-coloring runs of the title.
    /// `fuzzy_match_indices` counts in **chars** and `LayoutJob` slices in
    /// **bytes**, so a multi-byte title is the case that would panic inside egui
    /// rather than merely look wrong.
    #[test]
    fn highlight_job_maps_char_indices_to_byte_ranges() {
        let font = egui::FontId::proportional(15.0);
        let base = egui::Color32::WHITE;
        let hit = egui::Color32::RED;

        // No matches → one plain section covering the whole string.
        let job = highlight_job("New Tab", &[], font.clone(), base, hit);
        assert_eq!(job.text, "New Tab");
        assert_eq!(job.sections.len(), 1);
        assert_eq!(job.sections[0].format.color, base);

        // Neighbouring matches collapse into one run rather than one section
        // per character.
        let job = highlight_job("New Tab", &[0, 1, 2], font.clone(), base, hit);
        assert_eq!(job.text, "New Tab");
        assert_eq!(job.sections.len(), 2);
        assert_eq!(job.sections[0].byte_range, 0..3);
        assert_eq!(job.sections[0].format.color, hit);
        assert_eq!(job.sections[1].byte_range, 3..7);

        // Multi-byte: char 0 is a 2-byte 'é', so a matched char 2 is byte 3..4.
        // Slicing by char index would land mid-codepoint and panic.
        let text = "éa-b";
        assert_eq!(text.len(), 5);
        let job = highlight_job(text, &[2], font, base, hit);
        assert_eq!(job.text, text);
        let lit: Vec<_> = job
            .sections
            .iter()
            .filter(|s| s.format.color == hit)
            .map(|s| s.byte_range.clone())
            .collect();
        assert_eq!(lit, vec![3..4]);
        // Every section boundary is a char boundary.
        for s in &job.sections {
            assert!(text.is_char_boundary(s.byte_range.start));
            assert!(text.is_char_boundary(s.byte_range.end));
        }
    }
}
