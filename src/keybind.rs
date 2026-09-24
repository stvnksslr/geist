//! Keybind chords and the keymap that backs the app's shortcuts.
//!
//! A [`Chord`] is a backend-neutral key combination (modifiers + a [`KeyCode`]).
//! The [`Keymap`] maps chords to [`Action`]s; it starts from a built-in default
//! set and is overridden by `keybind = <trigger>=<action>` config lines.
//!
//! The app builds a chord from each live `egui` key event (via
//! `session::map_egui_key` / `session::key_mods`, so the modifier semantics match
//! the PTY-encode path) and looks it up here to decide which [`Action`] to run.
//! The chord *parser* ([`parse_chord`]) is `egui`-free so this module stays
//! testable without a UI.
//!
//! Reserving from the shell: `session::decide_key` consults this keymap and
//! swallows any bound chord, so a `keybind` works regardless of its modifiers
//! (it never also reaches the shell). It additionally reserves whole modifier
//! *namespaces* (`ctrl+shift+*`, `ctrl+alt+arrows`, `alt+digit`, `ctrl+=/-/0`)
//! even for unbound combos, matching geist's long-standing host behavior.

use crate::command::Action;
use crate::engine::{KeyCode, KeyMods, SelectionAdjust};

/// A key combination: the required modifiers plus the (non-modifier) key.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Chord {
    pub mods: KeyMods,
    pub code: KeyCode,
}

/// What a key press means, given the keys already pressed in this sequence.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Lookup {
    /// A complete binding: run these, in order. Never empty; more than one
    /// means the binding was extended with `chain=`.
    Action(Vec<Action>),
    /// A *prefix* of one or more longer bindings — the leader of a sequence.
    /// Consume the key and wait for the next one.
    Pending,
    /// Bound to nothing.
    None,
}

/// One entry on the active key-table stack.
///
/// Declared here, next to the keymap that resolves against it, so the app and
/// the keymap share one type: the keymap reads `name`, the app reads `once`, and
/// there is no per-frame conversion between two shapes of the same stack.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TableEntry {
    pub name: String,
    /// A one-shot activation, popped as soon as one of its bindings runs.
    pub once: bool,
}

/// One binding: a key sequence, the action it runs, and whether it is
/// *performable*.
///
/// A performable binding only counts when the action can actually do something
/// right now; otherwise the key falls through to the shell. Ghostty's
/// `performable:` trigger flag, and the reason `shift+left` still sends its
/// normal escape sequence when there is no selection to adjust.
#[derive(Clone, Debug, PartialEq, Eq)]
struct Bind {
    seq: Vec<Chord>,
    /// The actions to run, in order. **Never empty** — parsing guarantees at
    /// least one, and `chain=` appends to the most recently defined binding
    /// (Ghostty `chain`).
    actions: Vec<Action>,
    performable: bool,
    /// Ghostty's `unconsumed:` flag: run the action **and** let the key reach
    /// the program, instead of swallowing it.
    unconsumed: bool,
    /// Ghostty's `all:` flag: apply the action to every pane, not just the
    /// focused one. Upstream forces consumption and skips the performable
    /// check for these, so it overrides both other flags.
    all: bool,
}

/// The app keymap: an ordered list of *sequence* → action bindings. A plain
/// `ctrl+shift+t` is simply a sequence of length one, so single chords and
/// multi-key sequences (`ctrl+a>n`) share one lookup path. Later entries win
/// (config overrides are appended after the defaults), so lookup scans in
/// reverse.
#[derive(Clone, Debug)]
pub struct Keymap {
    binds: Vec<Bind>,
    /// Named **key tables** (`<table>/<binding>` in the config), each a bind
    /// list that only applies while that table is on the active stack. The
    /// mechanism behind a "copy mode" or a modal vim-style layer.
    tables: std::collections::HashMap<String, Vec<Bind>>,
    /// `global:` bindings — chords that fire even when geist isn't focused.
    /// Kept **out** of `binds` on purpose: they are delivered by the OS-level
    /// hook ([`crate::hotkey`]) whether or not geist has focus, so putting them
    /// in the ordinary keymap too would run the action twice on a focused press.
    globals: Vec<(Chord, Action)>,
}

impl Default for Keymap {
    fn default() -> Self {
        Self {
            binds: default_binds(),
            tables: std::collections::HashMap::new(),
            // No global binding by default: a low-level keyboard hook is a
            // system-wide cost, and a terminal should not take one uninvited.
            globals: Vec::new(),
        }
    }
}

impl Keymap {
    /// The action bound to a single `chord`, if any (most-recently-set wins).
    ///
    /// Only matches bindings that are *complete* at one chord — the leader of a
    /// sequence returns `None` here. Callers that need to know about leaders
    /// must use [`Self::lookup_seq`].
    pub fn lookup(&self, chord: &Chord) -> Option<Action> {
        self.lookup_in(&[], chord)
    }

    /// Every action a single `chord` runs, in order (a `chain=` binding has more
    /// than one). Empty when the chord is bound to nothing.
    pub fn lookup_chain(&self, stack: &[TableEntry], chord: &Chord) -> Vec<Action> {
        match self.lookup_seq_in(stack, std::slice::from_ref(chord)) {
            Lookup::Action(a) => a,
            _ => Vec::new(),
        }
    }

    /// The **first** action a chord runs, against an active key-table `stack`
    /// (innermost **last**). A `chain=` binding has more than one; use
    /// [`Self::lookup_chain`] when every action matters.
    pub fn lookup_in(&self, stack: &[TableEntry], chord: &Chord) -> Option<Action> {
        self.lookup_chain(stack, chord).into_iter().next()
    }

    /// The bind lists to search, innermost table first and the root last.
    ///
    /// Upstream's rule: "binding lookup proceeds from the innermost table
    /// outward, so keybinds in the default table remain available unless
    /// explicitly unbound in an inner table" — which is why the root is always
    /// the final entry rather than being skipped while a table is active. A
    /// table is therefore *not* modal on its own; shadowing a root binding takes
    /// an explicit `ignore`.
    fn search_order<'a>(&'a self, stack: &'a [TableEntry]) -> impl Iterator<Item = &'a Vec<Bind>> {
        stack
            .iter()
            .rev()
            .filter_map(move |t| self.tables.get(&t.name))
            .chain(std::iter::once(&self.binds))
    }

    /// Resolve a whole key sequence.
    ///
    /// An exact binding wins over being a prefix, so `ctrl+a=x` alongside
    /// `ctrl+a>n=y` runs `x` immediately rather than waiting forever for a
    /// second key that can never arrive — the same precedence a shell gives an
    /// exact match.
    pub fn lookup_seq(&self, keys: &[Chord]) -> Lookup {
        self.lookup_seq_in(&[], keys)
    }

    /// [`Self::lookup_seq`] against an active key-table `stack`.
    pub fn lookup_seq_in(&self, stack: &[TableEntry], keys: &[Chord]) -> Lookup {
        if keys.is_empty() {
            return Lookup::None;
        }
        // Each table is resolved completely before falling outward: an exact
        // match in an inner table beats being a prefix there, and both beat
        // anything in an outer one.
        for binds in self.search_order(stack) {
            if let Some(b) = binds.iter().rev().find(|b| b.seq == keys) {
                return Lookup::Action(b.actions.clone());
            }
            if binds
                .iter()
                .any(|b| b.seq.len() > keys.len() && &b.seq[..keys.len()] == keys)
            {
                return Lookup::Pending;
            }
            // `catch_all` is tried **inside each set**, before falling outward —
            // which is upstream's `Set.getEvent`, and is what makes a key table
            // modal: the table's `catch_all` shadows an exact binding in an
            // outer table or the root, which is the whole point of putting one
            // in a table.
            if let Some(a) = catch_all_in(binds, keys) {
                return Lookup::Action(a);
            }
        }
        Lookup::None
    }

    /// Whether the binding for `keys` is *performable* — i.e. it only applies
    /// when its action can currently do something, and otherwise the key belongs
    /// to the shell.
    ///
    /// The caller answers "can it?" itself, because that depends on live state
    /// (a selection, say) this module deliberately knows nothing about. **Both**
    /// gates must consult it: the one that decides whether the shell sees the
    /// key, and the one that runs the action. Checking only the first sends the
    /// key to the shell *and* runs the action; only the second swallows the key
    /// and does nothing.
    pub fn is_performable(&self, keys: &[Chord]) -> bool {
        self.is_performable_in(&[], keys)
    }

    /// [`Self::is_performable`] against an active key-table `stack`. Resolved
    /// through the same innermost-outward walk as the lookup, so the flag always
    /// comes from the binding that would actually run.
    pub fn is_performable_in(&self, stack: &[TableEntry], keys: &[Chord]) -> bool {
        for binds in self.search_order(stack) {
            if let Some(b) = binds.iter().rev().find(|b| b.seq == keys) {
                return b.performable;
            }
            if binds
                .iter()
                .any(|b| b.seq.len() > keys.len() && &b.seq[..keys.len()] == keys)
            {
                return false;
            }
        }
        false
    }

    /// Whether the binding for `keys` is flagged `all:` — its surface-scoped
    /// actions apply to every pane rather than the focused one.
    ///
    /// Upstream makes this flag dominant: an `all:` binding always consumes the
    /// key (overriding `unconsumed:`) and is always treated as performed
    /// (skipping `performable:`), because it isn't tied to one surface.
    pub fn is_all(&self, stack: &[TableEntry], keys: &[Chord]) -> bool {
        for binds in self.search_order(stack) {
            if let Some(b) = binds.iter().rev().find(|b| b.seq == keys) {
                return b.all;
            }
            if binds
                .iter()
                .any(|b| b.seq.len() > keys.len() && &b.seq[..keys.len()] == keys)
            {
                return false;
            }
        }
        false
    }

    /// Whether the binding for `keys` is flagged `unconsumed:` — the action
    /// runs *and* the key still reaches the program.
    ///
    /// Resolved through the same innermost-outward walk as the lookup, so the
    /// flag always comes from the binding that would actually run.
    pub fn is_unconsumed(&self, stack: &[TableEntry], keys: &[Chord]) -> bool {
        for binds in self.search_order(stack) {
            if let Some(b) = binds.iter().rev().find(|b| b.seq == keys) {
                return b.unconsumed;
            }
            if binds
                .iter()
                .any(|b| b.seq.len() > keys.len() && &b.seq[..keys.len()] == keys)
            {
                return false;
            }
        }
        false
    }

    /// Whether `chord` begins any binding — a complete one *or* a sequence.
    /// This is what tells the PTY path to swallow a leader like `ctrl+a`, which
    /// on its own is bound to nothing.
    pub fn starts_binding(&self, chord: &Chord) -> bool {
        self.starts_binding_in(&[], chord)
    }

    /// [`Self::starts_binding`] against an active key-table `stack`.
    pub fn starts_binding_in(&self, stack: &[TableEntry], chord: &Chord) -> bool {
        !matches!(
            self.lookup_seq_in(stack, std::slice::from_ref(chord)),
            Lookup::None
        )
    }

    /// The `global:` bindings, in config order.
    pub fn globals(&self) -> &[(Chord, Action)] {
        &self.globals
    }

    /// Bind `seq` to `action` in `table` (`None` = the root table), replacing
    /// any existing binding for it.
    fn set(
        &mut self,
        table: Option<&str>,
        seq: Vec<Chord>,
        action: Action,
        performable: bool,
        unconsumed: bool,
        all: bool,
    ) {
        let binds = self.binds_mut(table);
        match binds.iter_mut().find(|b| b.seq == seq) {
            Some(slot) => {
                slot.actions = vec![action];
                slot.performable = performable;
                slot.unconsumed = unconsumed;
                slot.all = all;
            }
            None => binds.push(Bind {
                seq,
                actions: vec![action],
                performable,
                unconsumed,
                all,
            }),
        }
    }

    /// Append `action` to the binding a `chain=` line refers to — the most
    /// recently *defined* one, identified by `(table, seq)`.
    ///
    /// Returns whether there was one to chain onto; a `chain=` with no parent is
    /// reported rather than silently attached to some older binding.
    fn chain(&mut self, parent: Option<&(Option<String>, Vec<Chord>)>, action: Action) -> bool {
        let Some((table, seq)) = parent else {
            return false;
        };
        match self
            .binds_mut(table.as_deref())
            .iter_mut()
            .find(|b| &b.seq == seq)
        {
            Some(b) => {
                b.actions.push(action);
                true
            }
            None => false,
        }
    }

    /// Remove any binding for `seq` (Ghostty's `unbind`).
    fn unset(&mut self, table: Option<&str>, seq: &[Chord]) {
        self.binds_mut(table).retain(|b| b.seq != seq);
    }

    fn binds_mut(&mut self, table: Option<&str>) -> &mut Vec<Bind> {
        match table {
            None => &mut self.binds,
            Some(t) => self.tables.entry(t.to_string()).or_default(),
        }
    }

    /// Whether a key table with this name has been defined.
    ///
    /// Activating an undefined table is a no-op upstream *and reports
    /// performable false*, so this is consulted by the performable gate.
    pub fn has_table(&self, name: &str) -> bool {
        self.tables.contains_key(name)
    }

    /// Build the keymap from the built-in defaults plus the user's `keybind`
    /// overrides (raw `trigger=action` pairs from the config). Unparseable
    /// triggers / unknown actions are logged and skipped; `unbind`/`ignore`
    /// removes a binding.
    pub fn from_config(overrides: &[(String, String)]) -> Self {
        let mut km = Self::default();
        // The binding a `chain=` line extends: the most recently *defined* one.
        // Anything that is not a plain bind clears it, so a chain can never
        // silently attach itself to some older binding — upstream's own
        // `removeExact` says "removal always resets our chain parent".
        let mut chain_parent: Option<(Option<String>, Vec<Chord>)> = None;
        for (trigger, action) in overrides {
            // `chain=<action>` appends to that parent. Checked before anything
            // else, since `chain` is a trigger *name*: it takes no table prefix
            // (upstream: "chain itself doesn't get prefixed with the table
            // name") and no flags ("chained actions cannot have prefixes"), the
            // original binding's flags applying to the whole chain.
            if trigger.trim() == "chain" {
                let a = action.trim_start();
                match Action::from_name(a) {
                    Some(act) => {
                        if !km.chain(chain_parent.as_ref(), act) {
                            eprintln!(
                                "geist: ignoring 'chain={a}' — no preceding keybind to chain onto"
                            );
                        }
                    }
                    None => eprintln!("geist: ignoring keybind to unknown action '{a}'"),
                }
                continue;
            }
            // `<table>/<binding>` puts the binding in a **named key table**,
            // which only applies while that table is active. `<name>/` with no
            // binding defines and clears the table.
            let (table, trigger) = match split_table(trigger) {
                Some((name, rest)) => {
                    // Naming a table defines it, which is what makes
                    // `activate_key_table:<name>` work before anything is bound
                    // in it. Only the *bare* `<name>/` form clears it — clearing
                    // on every line would wipe the table's earlier bindings, one
                    // line at a time.
                    let binds = km.tables.entry(name.to_string()).or_default();
                    if rest.trim().is_empty() {
                        binds.clear();
                        chain_parent = None;
                        continue;
                    }
                    (Some(name.to_string()), rest.to_string())
                }
                None => (None, trigger.clone()),
            };
            // Every path below either defines a new parent or invalidates the
            // old one; set it here so no `continue` can leave a stale one.
            chain_parent = None;
            // Strip the trigger flags in **any order** — upstream documents
            // stacking them (`global:unconsumed:ctrl+a=…`) and does not fix
            // their order, so this loops rather than testing one arrangement.
            let mut rest = trigger.as_str();
            let (mut global, mut performable, mut unconsumed, mut all) =
                (false, false, false, false);
            loop {
                if let Some(r) = strip_flag(rest, "global") {
                    global = true;
                    rest = r;
                } else if let Some(r) = strip_flag(rest, "performable") {
                    performable = true;
                    rest = r;
                } else if let Some(r) = strip_flag(rest, "unconsumed") {
                    unconsumed = true;
                    rest = r;
                } else if let Some(r) = strip_flag(rest, "all") {
                    all = true;
                    rest = r;
                } else {
                    break;
                }
            }
            let flagless = rest.trim().to_string();
            let trigger = &trigger;
            // Ghostty's `global:` trigger flag. It is inherently unsequenceable
            // upstream too — the OS delivers one key, not a leader and a
            // follower — so a global trigger must be a single chord.
            if global {
                let rest = flagless.as_str();
                if table.is_some() {
                    // Upstream allows `foo/global:…`, but a global chord is
                    // delivered by an OS keyboard hook that deliberately does
                    // the minimum and never consults app state (see the
                    // quick-terminal ledger) — it cannot ask which table is
                    // active. Reported rather than silently registered as an
                    // unconditional global, which would fire outside the table.
                    eprintln!(
                        "geist: 'global:' inside a key table is not supported, ignoring '{trigger}'"
                    );
                    continue;
                }
                let Some(chord) = parse_chord(rest.trim()) else {
                    eprintln!(
                        "geist: ignoring global keybind with unparseable trigger '{trigger}'"
                    );
                    continue;
                };
                let a = action.trim();
                if a.eq_ignore_ascii_case("unbind") || a.eq_ignore_ascii_case("ignore") {
                    km.globals.retain(|(c, _)| *c != chord);
                    continue;
                }
                match Action::from_name(a) {
                    Some(act) => match km.globals.iter_mut().find(|(c, _)| *c == chord) {
                        Some(slot) => slot.1 = act,
                        None => km.globals.push((chord, act)),
                    },
                    None => eprintln!("geist: ignoring keybind to unknown action '{a}'"),
                }
                continue;
            }
            let Some(seq) = parse_sequence(&flagless) else {
                eprintln!("geist: ignoring keybind with unparseable trigger '{trigger}'");
                continue;
            };
            // Upstream: "trigger sequences are not allowed for `global:` or
            // `all:`-prefixed triggers". Rejected rather than quietly bound to
            // the last chord, which is what accepting it would amount to.
            if all && seq.len() > 1 {
                eprintln!("geist: 'all:' does not support key sequences, ignoring '{trigger}'");
                continue;
            }
            // `trim_start` only: a payload action carries its trailing
            // whitespace deliberately (`text:hello `), and `Action::from_name`
            // trims the names that should be trimmed itself.
            let a = action.trim_start();
            // `unbind` **removes** the binding, so the key goes back to the
            // shell. `ignore` **binds** it to nothing, black-holing the key —
            // upstream's two are genuinely different actions (`set.remove`
            // versus a bound no-op), and geist treated them as the same. The
            // difference is what lets a key table shadow a root binding:
            // `foo/ctrl+t=ignore` silences ctrl+t while `foo` is active, where
            // `unbind` there would let the root's ctrl+t through.
            if a.trim().eq_ignore_ascii_case("unbind") {
                km.unset(table.as_deref(), &seq);
                continue;
            }
            match Action::from_name(a) {
                Some(act) => {
                    km.set(
                        table.as_deref(),
                        seq.clone(),
                        act,
                        performable,
                        unconsumed,
                        all,
                    );
                    // Only a successful plain bind becomes a chain parent.
                    chain_parent = Some((table, seq));
                }
                None => eprintln!("geist: ignoring keybind to unknown action '{a}'"),
            }
        }
        km
    }
}

/// The `catch_all` binding in `binds` matching this key press, if any.
///
/// Upstream's order (`Binding.zig`'s `Set.getEvent`): the same modifiers first,
/// then — **only if the press had modifiers** — bare `catch_all`. A
/// modifierless press therefore gets exactly one try, since for it the two are
/// the same lookup.
///
/// Only a single chord can catch: `catch_all` describes one key press, not a
/// sequence, so a partially-typed sequence is not caught here (see the
/// dead-end handling for what happens then).
fn catch_all_in(binds: &[Bind], keys: &[Chord]) -> Option<Vec<Action>> {
    let [chord] = keys else { return None };
    let find = |mods: KeyMods| {
        binds
            .iter()
            .rev()
            .find(|b| {
                b.seq.len() == 1 && b.seq[0].code == KeyCode::CatchAll && b.seq[0].mods == mods
            })
            .map(|b| b.actions.clone())
    };
    find(chord.mods).or_else(|| {
        (chord.mods != KeyMods::default())
            .then(|| find(KeyMods::default()))
            .flatten()
    })
}

/// Split a `<table>/<binding>` trigger into its table name and the rest.
///
/// A table name may contain "anything except `/`, `=`, `+`, and `>`" (upstream's
/// rule), and that exclusion is what makes this unambiguous: a `/` used as a
/// *key* (`ctrl+/`) always has a `+` before it, and a `/` inside a key sequence
/// has a `>`. So a prefix containing any of those is not a table name, and the
/// trigger is an ordinary one.
fn split_table(trigger: &str) -> Option<(&str, &str)> {
    let (name, rest) = trigger.split_once('/')?;
    let name = name.trim();
    if name.is_empty() || name.contains(['=', '+', '>']) {
        return None;
    }
    Some((name, rest))
}

/// Strip a leading Ghostty trigger flag (`global:`, `all:`, …), returning the
/// rest of the trigger when it matches.
///
/// Case-insensitive, and it will not mistake a *key* for a flag: the match needs
/// the colon, and `global` alone is not a key name anyway.
fn strip_flag<'a>(trigger: &'a str, flag: &str) -> Option<&'a str> {
    let t = trigger.trim_start();
    let (head, rest) = t.split_once(':')?;
    head.trim().eq_ignore_ascii_case(flag).then_some(rest)
}

/// Parse a `>`-separated key sequence like `ctrl+a>n` into its chords.
///
/// A trigger with no `>` yields a one-element sequence, which is why the rest of
/// the keymap needs no special case for plain chords. Returns `None` if any
/// chord fails to parse or the sequence is empty.
pub fn parse_sequence(s: &str) -> Option<Vec<Chord>> {
    let seq: Option<Vec<Chord>> = s.split('>').map(|part| parse_chord(part.trim())).collect();
    seq.filter(|v: &Vec<Chord>| !v.is_empty())
}

/// The built-in default bindings, mirroring the host shortcuts geist has always
/// had. Every trigger here parses and sits inside an app-reserved namespace.
fn default_binds() -> Vec<Bind> {
    const DEFAULTS: &[(&str, Action)] = &[
        ("ctrl+shift+t", Action::NewTab),
        // Ghostty's non-Darwin default for `new_window`.
        ("ctrl+shift+n", Action::NewWindow),
        // NOTE: `close_window` is deliberately left unbound. Ghostty binds it to
        // alt+f4, but on Windows the OS already delivers Alt+F4 as WM_CLOSE →
        // `close_requested`, which geist answers with the confirmation flow.
        // Binding it too would raise an action *and* a close request in the same
        // pass. Users who want the explicit action can add
        // `keybind = alt+f4=close_window`.
        ("ctrl+shift+w", Action::ClosePane),
        // D mirrors macOS Ghostty's Cmd+D; O matches the GTK default. Both split right.
        ("ctrl+shift+d", Action::SplitRight),
        ("ctrl+shift+o", Action::SplitRight),
        ("ctrl+shift+e", Action::SplitDown),
        // Ghostty defaults: ctrl+enter fullscreen, ctrl+shift+enter zoom split.
        ("ctrl+enter", Action::ToggleFullscreen),
        ("ctrl+shift+enter", Action::ToggleSplitZoom),
        ("ctrl+shift+[", Action::FocusSplitPrev),
        ("ctrl+shift+]", Action::FocusSplitNext),
        ("ctrl+shift+p", Action::TogglePalette),
        ("ctrl+shift+f", Action::ToggleSearch),
        // Ghostty's own trigger, which its comment records as "matching
        // Chromium" — the devtools chord every browser uses.
        (
            "ctrl+shift+i",
            Action::Inspector(crate::inspector::InspectorMode::Toggle),
        ),
        ("ctrl+alt+left", Action::FocusSplitLeft),
        ("ctrl+alt+right", Action::FocusSplitRight),
        ("ctrl+alt+up", Action::FocusSplitUp),
        ("ctrl+alt+down", Action::FocusSplitDown),
        ("alt+1", Action::GotoTab(0)),
        ("alt+2", Action::GotoTab(1)),
        ("alt+3", Action::GotoTab(2)),
        ("alt+4", Action::GotoTab(3)),
        ("alt+5", Action::GotoTab(4)),
        ("alt+6", Action::GotoTab(5)),
        ("alt+7", Action::GotoTab(6)),
        ("alt+8", Action::GotoTab(7)),
        ("alt+9", Action::LastTab),
        ("ctrl+tab", Action::NextTab),
        ("ctrl+shift+tab", Action::PrevTab),
        ("ctrl+shift+up", Action::JumpToPrompt(-1)),
        ("ctrl+shift+down", Action::JumpToPrompt(1)),
        // Ghostty's non-Darwin scrollback bindings. These live here rather than
        // in `decide_key` so that `keybind = shift+home=unbind` actually works:
        // an unbind removes a keymap entry, so a key handled only by a
        // hardcoded branch could be *re*bound but never turned off.
        ("shift+pageup", Action::ScrollPageUp),
        ("shift+pagedown", Action::ScrollPageDown),
        ("shift+home", Action::ScrollToTop),
        ("shift+end", Action::ScrollToBottom),
    ];
    // Ghostty binds shift+arrows to `adjust_selection`, and **performable** —
    // with no selection the key is the shell's, which is what keeps shift+arrow
    // working in editors. It also binds shift+home/end/pageup/pagedown to
    // adjust_selection, but only on macOS: on every other platform the viewport
    // scrolling bindings above are registered *after* them and win. geist is
    // Windows, so those four stay scroll bindings — matching upstream, not
    // diverging from it.
    //
    // Undo/redo join them: upstream binds `undo`/`redo` performable too, so a
    // chord with an empty stack behind it falls through to the shell instead of
    // being swallowed. The triggers are Windows-conventional rather than
    // upstream's `super+z` / `super+shift+z` — and deliberately *not* bare
    // `ctrl+z`, which is the shell's own (SIGSTOP on a POSIX shell, and undo in
    // every readline-alike).
    //
    // `escape` joins them for the same reason, and it is the sharpest example
    // in the table: upstream binds it to `end_search` **performable**, so the
    // key closes the search bar when one is open and belongs entirely to the
    // program (vim, a pager, a TUI) when one isn't. Without the flag this bind
    // would make Escape unusable in every full-screen application.
    const PERFORMABLE: &[(&str, Action)] = &[
        ("ctrl+shift+z", Action::Undo),
        ("ctrl+shift+y", Action::Redo),
        ("escape", Action::EndSearch),
        ("shift+left", Action::AdjustSelection(SelectionAdjust::Left)),
        (
            "shift+right",
            Action::AdjustSelection(SelectionAdjust::Right),
        ),
        ("shift+up", Action::AdjustSelection(SelectionAdjust::Up)),
        ("shift+down", Action::AdjustSelection(SelectionAdjust::Down)),
    ];
    let plain = DEFAULTS.iter().map(|(t, a)| (*t, a.clone(), false));
    let performable = PERFORMABLE.iter().map(|(t, a)| (*t, a.clone(), true));
    plain
        .chain(performable)
        .filter_map(|(t, action, performable)| {
            parse_chord(t).map(|c| Bind {
                seq: vec![c],
                actions: vec![action],
                performable,
                unconsumed: false,
                all: false,
            })
        })
        .collect()
}

/// Parse a `+`-separated chord like `ctrl+shift+t` into a [`Chord`]. Modifier
/// tokens are case-insensitive (`ctrl`/`control`, `shift`, `alt`/`opt`/`option`,
/// `super`/`cmd`/`command`/`win`/`meta`); the remaining token is the key (see
/// [`key_from_name`]). Returns `None` if there is no valid key.
pub fn parse_chord(s: &str) -> Option<Chord> {
    let mut mods = KeyMods::default();
    let mut code: Option<KeyCode> = None;
    for part in s.split('+') {
        let p = part.trim().to_ascii_lowercase();
        match p.as_str() {
            "" => {}
            "ctrl" | "control" => mods.ctrl = true,
            "shift" => mods.shift = true,
            "alt" | "opt" | "option" => mods.alt = true,
            "super" | "cmd" | "command" | "win" | "meta" => mods.sup = true,
            other => code = key_from_name(other),
        }
    }
    Some(Chord { mods, code: code? })
}

/// Map a key token (already lowercased) to a [`KeyCode`]. Covers letters,
/// digits, function keys, arrows, and the punctuation/named keys that appear in
/// shortcuts. Returns `None` for anything unrecognized.
fn key_from_name(name: &str) -> Option<KeyCode> {
    use KeyCode::*;
    // Ghostty's `catch_all` pseudo-key, checked first so it can't be mistaken
    // for anything else. It is a *key name*, so `ctrl+catch_all` and
    // `copy/catch_all` fall out of the existing chord and table machinery.
    if name == "catch_all" {
        return Some(CatchAll);
    }
    // Single ASCII letter or digit.
    if name.len() == 1 {
        let ch = name.as_bytes()[0];
        match ch {
            b'a'..=b'z' => {
                const LETTERS: [KeyCode; 26] = [
                    A, B, C, D, E, F, G, H, I, J, K, L, M, N, O, P, Q, R, S, T, U, V, W, X, Y, Z,
                ];
                return Some(LETTERS[(ch - b'a') as usize]);
            }
            b'0'..=b'9' => {
                const DIGITS: [KeyCode; 10] = [
                    Digit0, Digit1, Digit2, Digit3, Digit4, Digit5, Digit6, Digit7, Digit8, Digit9,
                ];
                return Some(DIGITS[(ch - b'0') as usize]);
            }
            _ => {}
        }
    }
    Some(match name {
        "left" | "arrowleft" => ArrowLeft,
        "right" | "arrowright" => ArrowRight,
        "up" | "arrowup" => ArrowUp,
        "down" | "arrowdown" => ArrowDown,
        "tab" => Tab,
        "enter" | "return" => Enter,
        "space" => Space,
        "escape" | "esc" => Escape,
        "backspace" => Backspace,
        "delete" | "del" => Delete,
        "insert" | "ins" => Insert,
        "home" => Home,
        "end" => End,
        "pageup" | "pgup" => PageUp,
        "pagedown" | "pgdn" => PageDown,
        "minus" | "-" => Minus,
        "equal" | "equals" | "=" | "plus" => Equal,
        "[" | "bracketleft" | "leftbracket" => BracketLeft,
        "]" | "bracketright" | "rightbracket" => BracketRight,
        "\\" | "backslash" => Backslash,
        ";" | "semicolon" => Semicolon,
        "'" | "quote" | "apostrophe" => Quote,
        "`" | "backquote" | "grave" => Backquote,
        "," | "comma" => Comma,
        "." | "period" => Period,
        "/" | "slash" => Slash,
        "f1" => F1,
        "f2" => F2,
        "f3" => F3,
        "f4" => F4,
        "f5" => F5,
        "f6" => F6,
        "f7" => F7,
        "f8" => F8,
        "f9" => F9,
        "f10" => F10,
        "f11" => F11,
        "f12" => F12,
        _ => return None,
    })
}

/// The config spelling of a key code — the inverse of [`key_from_name`].
///
/// Written out rather than derived from `Debug`, because its consumer is the
/// inspector's keyboard log, whose whole job is to answer "what do I type after
/// `keybind =` to bind this key?". `ArrowLeft` is not that answer; `left` is.
/// Where `key_from_name` accepts several spellings this returns the one
/// upstream's own configs use.
pub fn key_name(code: KeyCode) -> &'static str {
    use KeyCode::*;
    match code {
        CatchAll => "catch_all",
        A => "a",
        B => "b",
        C => "c",
        D => "d",
        E => "e",
        F => "f",
        G => "g",
        H => "h",
        I => "i",
        J => "j",
        K => "k",
        L => "l",
        M => "m",
        N => "n",
        O => "o",
        P => "p",
        Q => "q",
        R => "r",
        S => "s",
        T => "t",
        U => "u",
        V => "v",
        W => "w",
        X => "x",
        Y => "y",
        Z => "z",
        Digit0 => "0",
        Digit1 => "1",
        Digit2 => "2",
        Digit3 => "3",
        Digit4 => "4",
        Digit5 => "5",
        Digit6 => "6",
        Digit7 => "7",
        Digit8 => "8",
        Digit9 => "9",
        ArrowLeft => "left",
        ArrowRight => "right",
        ArrowUp => "up",
        ArrowDown => "down",
        Tab => "tab",
        Enter => "enter",
        Space => "space",
        Escape => "escape",
        Backspace => "backspace",
        Delete => "delete",
        Insert => "insert",
        Home => "home",
        End => "end",
        PageUp => "pageup",
        PageDown => "pagedown",
        Minus => "minus",
        Equal => "equal",
        BracketLeft => "[",
        BracketRight => "]",
        Backslash => "\\",
        Semicolon => ";",
        Quote => "'",
        Backquote => "`",
        Comma => ",",
        Period => ".",
        Slash => "/",
        F1 => "f1",
        F2 => "f2",
        F3 => "f3",
        F4 => "f4",
        F5 => "f5",
        F6 => "f6",
        F7 => "f7",
        F8 => "f8",
        F9 => "f9",
        F10 => "f10",
        F11 => "f11",
        F12 => "f12",
    }
}

impl Chord {
    /// This chord in config spelling, e.g. `ctrl+shift+t`.
    ///
    /// Modifier order is upstream's (`super`, `ctrl`, `alt`, `shift`), so a
    /// chord copied out of the inspector into a config parses back to itself.
    pub fn name(&self) -> String {
        let mut out = String::new();
        for (on, name) in [
            (self.mods.sup, "super"),
            (self.mods.ctrl, "ctrl"),
            (self.mods.alt, "alt"),
            (self.mods.shift, "shift"),
        ] {
            if on {
                out.push_str(name);
                out.push('+');
            }
        }
        out.push_str(key_name(self.code));
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The active-table stack for a test, innermost last.
    fn stack(names: &[&str]) -> Vec<TableEntry> {
        names
            .iter()
            .map(|n| TableEntry {
                name: n.to_string(),
                once: false,
            })
            .collect()
    }

    #[test]
    fn action_scope_matches_upstreams_table() {
        use crate::command::Scope;
        // The counter-intuitive rows, which are upstream's on purpose: an
        // action is surface-scoped when it is "relevant to the surface it comes
        // from", even when it visibly affects the window.
        assert_eq!(Action::NewTab.scope(), Scope::Surface);
        assert_eq!(Action::GotoTab(1).scope(), Scope::Surface);
        assert_eq!(Action::CloseTab.scope(), Scope::Surface);
        assert_eq!(Action::ToggleReadonly.scope(), Scope::Surface);
        assert_eq!(Action::SplitRight.scope(), Scope::Surface);
        // …while these are app-scoped, so `all:` runs them once.
        assert_eq!(Action::NewWindow.scope(), Scope::App);
        assert_eq!(Action::Quit.scope(), Scope::App);
        assert_eq!(Action::ReloadConfig.scope(), Scope::App);
        assert_eq!(Action::OpenConfig.scope(), Scope::App);
        assert_eq!(Action::ToggleQuickTerminal.scope(), Scope::App);
        assert_eq!(Action::Noop("ignore".into()).scope(), Scope::App);

        // The broadcast subset is narrower than Surface, and deliberately so:
        // these are the ones geist can source to a pane.
        assert!(Action::SendText("x".into()).broadcasts_to_panes());
        assert!(Action::ClearScreen.broadcasts_to_panes());
        assert!(Action::ScrollPageUp.broadcasts_to_panes());
        assert!(!Action::NewTab.broadcasts_to_panes(), "window-structural");
        assert!(!Action::NewWindow.broadcasts_to_panes(), "app-scoped");
    }

    #[test]
    fn all_is_dominant_over_the_other_flags_and_rejects_sequences() {
        let km = Keymap::from_config(&[("all:ctrl+alt+k".into(), "clear_screen".into())]);
        let seq = [chord("ctrl+alt+k")];
        assert!(km.is_all(&[], &seq));
        assert_eq!(km.lookup(&chord("ctrl+alt+k")), Some(Action::ClearScreen));

        // Stacks in any order with the others, and stays dominant.
        for t in ["all:unconsumed:ctrl+alt+k", "unconsumed:all:ctrl+alt+k"] {
            let km = Keymap::from_config(&[(t.into(), "clear_screen".into())]);
            assert!(km.is_all(&[], &seq), "{t}");
        }

        // Sequences are rejected, as upstream rejects them for `global:`/`all:`.
        let km = Keymap::from_config(&[("all:ctrl+a>n".into(), "clear_screen".into())]);
        assert_eq!(km.lookup_seq(&[chord("ctrl+a"), chord("n")]), Lookup::None);

        // Unlike `global:`, `all:` is legal inside a key table — the OS-hook
        // reason for rejecting a global there doesn't apply.
        let km = Keymap::from_config(&[("copy/all:j".into(), "clear_screen".into())]);
        let s = stack(&["copy"]);
        assert_eq!(km.lookup_in(&s, &chord("j")), Some(Action::ClearScreen));
        assert!(km.is_all(&s, &[chord("j")]));
    }

    #[test]
    fn chain_appends_actions_to_the_most_recent_binding() {
        let km = Keymap::from_config(&[
            ("ctrl+alt+a".into(), "new_window".into()),
            ("chain".into(), "new_tab".into()),
            ("chain".into(), "toggle_fullscreen".into()),
        ]);
        assert_eq!(
            km.lookup_chain(&[], &chord("ctrl+alt+a")),
            vec![Action::NewWindow, Action::NewTab, Action::ToggleFullscreen],
            "in the order they were written"
        );
        // `lookup` still answers with the first action, so every existing
        // caller keeps working.
        assert_eq!(km.lookup(&chord("ctrl+alt+a")), Some(Action::NewWindow));
    }

    #[test]
    fn a_chain_needs_a_parent_and_never_borrows_an_older_one() {
        // The dangerous case: an intervening line that is *not* a plain bind
        // must clear the parent, or the chain silently attaches to whatever was
        // defined before it. Upstream: "removal always resets our chain parent".
        let km = Keymap::from_config(&[
            ("ctrl+alt+a".into(), "new_window".into()),
            ("ctrl+alt+b".into(), "unbind".into()),
            ("chain".into(), "new_tab".into()),
        ]);
        assert_eq!(
            km.lookup_chain(&[], &chord("ctrl+alt+a")),
            vec![Action::NewWindow],
            "the chain was dropped, not attached to ctrl+alt+a"
        );

        // A chain before any binding is reported, not a panic.
        let km = Keymap::from_config(&[("chain".into(), "new_tab".into())]);
        assert_eq!(km.lookup(&chord("ctrl+alt+a")), None);

        // A table definition also clears it.
        let km = Keymap::from_config(&[
            ("ctrl+alt+a".into(), "new_window".into()),
            ("scratch/".into(), "".into()),
            ("chain".into(), "new_tab".into()),
        ]);
        assert_eq!(
            km.lookup_chain(&[], &chord("ctrl+alt+a")),
            vec![Action::NewWindow]
        );
    }

    #[test]
    fn chain_works_inside_tables_and_sequences() {
        // Upstream: "chain itself doesn't get prefixed with the table name,
        // since it applies to the most recent binding in any table".
        let km = Keymap::from_config(&[
            ("copy/j".into(), "scroll_page_down".into()),
            ("chain".into(), "new_tab".into()),
        ]);
        assert_eq!(
            km.lookup_chain(&stack(&["copy"]), &chord("j")),
            vec![Action::ScrollPageDown, Action::NewTab]
        );

        // "Chains with key sequences apply to the most recent binding in the
        // sequence" — i.e. the completed one.
        let km = Keymap::from_config(&[
            ("ctrl+a>n".into(), "new_window".into()),
            ("chain".into(), "new_tab".into()),
        ]);
        assert_eq!(
            km.lookup_seq(&[chord("ctrl+a"), chord("n")]),
            Lookup::Action(vec![Action::NewWindow, Action::NewTab])
        );
    }

    #[test]
    fn a_chained_payload_action_keeps_its_payload() {
        // A new route into the payload parser: everything after the *first* `=`
        // is the action, so a payload with its own `=` survives.
        let km = Keymap::from_config(&[
            ("ctrl+alt+a".into(), "new_window".into()),
            ("chain".into(), "text:a=b".into()),
        ]);
        assert_eq!(
            km.lookup_chain(&[], &chord("ctrl+alt+a")),
            vec![Action::NewWindow, Action::SendText("a=b".into())]
        );
    }

    #[test]
    fn catch_all_matches_only_what_is_not_otherwise_bound() {
        let km = Keymap::from_config(&[
            ("catch_all".into(), "scroll_page_down".into()),
            ("ctrl+catch_all".into(), "scroll_page_up".into()),
            ("q".into(), "new_tab".into()),
        ]);
        // An exact binding always wins.
        assert_eq!(km.lookup(&chord("q")), Some(Action::NewTab));
        // Anything else is caught, by the entry matching its modifiers…
        assert_eq!(km.lookup(&chord("z")), Some(Action::ScrollPageDown));
        assert_eq!(km.lookup(&chord("ctrl+z")), Some(Action::ScrollPageUp));
        // …and a modified press falls back to the bare `catch_all` when there
        // is no entry for its modifiers. A *modifierless* press gets one try,
        // since for it the two lookups are the same.
        assert_eq!(km.lookup(&chord("alt+z")), Some(Action::ScrollPageDown));

        // With only a modified catch_all, an unmodified key is not caught.
        let km = Keymap::from_config(&[("ctrl+catch_all".into(), "new_tab".into())]);
        assert_eq!(km.lookup(&chord("ctrl+z")), Some(Action::NewTab));
        assert_eq!(km.lookup(&chord("z")), None);
    }

    #[test]
    fn a_tables_catch_all_shadows_outer_bindings() {
        // `catch_all` is resolved **inside each set** before falling outward
        // (upstream's `Set.getEvent`), which is what makes a key table modal —
        // and is the reason to put one in a table at all.
        let km = Keymap::from_config(&[
            ("copy/j".into(), "scroll_page_down".into()),
            ("copy/catch_all".into(), "ignore".into()),
        ]);
        let s = stack(&["copy"]);
        assert_eq!(km.lookup_in(&s, &chord("j")), Some(Action::ScrollPageDown));
        // The root's own binding loses to the table's catch_all.
        assert_eq!(km.lookup(&chord("ctrl+shift+t")), Some(Action::NewTab));
        assert!(matches!(
            km.lookup_in(&s, &chord("ctrl+shift+t")),
            Some(Action::Noop(_))
        ));
        // And it is inert outside the table.
        assert_eq!(km.lookup(&chord("z")), None);
    }

    #[test]
    fn a_non_ignoring_catch_all_does_not_swallow_a_broken_sequence() {
        // Upstream drops a broken sequence silently *only* when a `catch_all`
        // would `ignore` the breaking key; any other catch_all still flushes,
        // and its action does not run. The distinction lives in the app's
        // dead-end branch, so what this pins is the input to that decision.
        let ignoring = Keymap::from_config(&[
            ("ctrl+a>n".into(), "new_tab".into()),
            ("catch_all".into(), "ignore".into()),
        ]);
        assert!(matches!(
            ignoring.lookup(&chord("x")),
            Some(Action::Noop(ref n)) if &**n == "ignore"
        ));

        let scrolling = Keymap::from_config(&[
            ("ctrl+a>n".into(), "new_tab".into()),
            ("catch_all".into(), "scroll_page_down".into()),
        ]);
        assert_eq!(scrolling.lookup(&chord("x")), Some(Action::ScrollPageDown));
        assert!(
            !matches!(scrolling.lookup(&chord("x")), Some(Action::Noop(_))),
            "not an ignore, so the sequence is flushed rather than dropped"
        );
    }

    #[test]
    fn unconsumed_applies_to_a_sequences_final_chord_only() {
        // Upstream forbids sequences for `global:`/`all:` and says nothing about
        // `unconsumed:`, so it is legal here — and applies to the binding, i.e.
        // the completed sequence. The leader must still be swallowed, or the
        // sequence could never start.
        let km = Keymap::from_config(&[("unconsumed:ctrl+a>n".into(), "new_tab".into())]);
        let (a, n) = (chord("ctrl+a"), chord("n"));
        assert!(!km.is_unconsumed(&[], &[a]), "the leader is consumed");
        assert!(
            km.is_unconsumed(&[], &[a, n]),
            "the complete binding is not"
        );
        assert_eq!(km.lookup_seq(&[a]), Lookup::Pending);
    }

    #[test]
    fn trigger_flags_stack_in_any_order() {
        // Upstream documents stacking (`global:unconsumed:…`) without fixing an
        // order, so both spellings must parse the same.
        for t in [
            "performable:unconsumed:ctrl+alt+k",
            "unconsumed:performable:ctrl+alt+k",
        ] {
            let km = Keymap::from_config(&[(t.into(), "new_tab".into())]);
            let seq = [chord("ctrl+alt+k")];
            assert_eq!(km.lookup(&chord("ctrl+alt+k")), Some(Action::NewTab), "{t}");
            assert!(km.is_performable(&seq), "{t}");
            assert!(km.is_unconsumed(&[], &seq), "{t}");
        }
        // A plain binding is neither.
        let km = Keymap::from_config(&[("ctrl+alt+k".into(), "new_tab".into())]);
        assert!(!km.is_unconsumed(&[], &[chord("ctrl+alt+k")]));
    }

    #[test]
    fn a_key_table_binding_only_applies_while_its_table_is_active() {
        let km = Keymap::from_config(&[
            ("copy/j".into(), "scroll_page_down".into()),
            ("copy/ctrl+n".into(), "new_tab".into()),
        ]);
        // Inert with no table active — `j` is just a letter.
        assert_eq!(km.lookup(&chord("j")), None);
        assert!(!km.starts_binding(&chord("j")));

        let s = stack(&["copy"]);
        assert_eq!(
            km.lookup_in(&s, &chord("j")),
            Some(Action::ScrollPageDown),
            "the table's binding applies once it is active"
        );
        assert!(km.starts_binding_in(&s, &chord("j")));
        // An unknown table name on the stack is simply skipped.
        assert_eq!(km.lookup_in(&stack(&["nope"]), &chord("j")), None);
    }

    #[test]
    fn lookup_falls_outward_from_the_innermost_table_to_the_root() {
        // Upstream: "keybinds in the default table remain available unless
        // explicitly unbound in an inner table" — so a table is *not* modal by
        // itself, and shadowing takes an explicit `ignore`.
        let km = Keymap::from_config(&[
            ("copy/j".into(), "scroll_page_down".into()),
            // Shadows the root's ctrl+shift+t while `copy` is active.
            ("copy/ctrl+shift+t".into(), "ignore".into()),
            ("inner/j".into(), "scroll_page_up".into()),
        ]);
        let copy = stack(&["copy"]);

        // Root bindings stay reachable through an active table.
        assert_eq!(
            km.lookup(&chord("ctrl+shift+p")),
            Some(Action::TogglePalette)
        );
        assert_eq!(
            km.lookup_in(&copy, &chord("ctrl+shift+p")),
            Some(Action::TogglePalette)
        );
        // …unless the table binds them to `ignore`, which *binds* rather than
        // removes. `unbind` in a table would let the root's binding through.
        assert!(matches!(
            km.lookup_in(&copy, &chord("ctrl+shift+t")),
            Some(Action::Noop(_))
        ));
        assert_eq!(km.lookup(&chord("ctrl+shift+t")), Some(Action::NewTab));

        // Innermost wins between two active tables.
        let both = stack(&["copy", "inner"]);
        assert_eq!(km.lookup_in(&both, &chord("j")), Some(Action::ScrollPageUp));
    }

    #[test]
    fn ignore_binds_to_nothing_while_unbind_removes_the_binding() {
        // These were treated as the same thing, and they are not: upstream's
        // `unbind` is `set.remove`, while `ignore` black-holes the key. The
        // difference is invisible until something else would answer the key.
        let ignored = Keymap::from_config(&[("ctrl+shift+t".into(), "ignore".into())]);
        assert!(matches!(
            ignored.lookup(&chord("ctrl+shift+t")),
            Some(Action::Noop(_))
        ));
        assert!(
            ignored.starts_binding(&chord("ctrl+shift+t")),
            "still bound, so the shell never sees it"
        );

        let unbound = Keymap::from_config(&[("ctrl+shift+t".into(), "unbind".into())]);
        assert_eq!(unbound.lookup(&chord("ctrl+shift+t")), None);
    }

    #[test]
    fn a_table_can_be_defined_empty_and_redefining_it_clears_it() {
        // `<name>/` with no binding defines and clears a table — which is what
        // makes `activate_key_table:<name>` work before anything is bound in it.
        let km = Keymap::from_config(&[("scratch/".into(), "".into())]);
        assert!(km.has_table("scratch"));
        assert!(!km.has_table("other"));

        let km = Keymap::from_config(&[
            ("copy/j".into(), "scroll_page_down".into()),
            ("copy/".into(), "".into()),
        ]);
        assert!(km.has_table("copy"));
        assert_eq!(
            km.lookup_in(&stack(&["copy"]), &chord("j")),
            None,
            "redefining the table reset its bindings"
        );
    }

    #[test]
    fn a_table_name_is_only_read_where_one_is_legal() {
        // Table names cannot contain `= + >`, which is what keeps a `/` used as
        // a *key* from being misread as a table prefix.
        let km = Keymap::from_config(&[("ctrl+/".into(), "new_tab".into())]);
        assert_eq!(km.lookup(&chord("ctrl+/")), Some(Action::NewTab));
        assert!(!km.has_table("ctrl+"));

        // Sequences work inside a table.
        let km = Keymap::from_config(&[("copy/ctrl+a>n".into(), "new_tab".into())]);
        let s = stack(&["copy"]);
        assert_eq!(km.lookup_seq_in(&s, &[chord("ctrl+a")]), Lookup::Pending);
        assert_eq!(
            km.lookup_seq_in(&s, &[chord("ctrl+a"), chord("n")]),
            Lookup::Action(vec![Action::NewTab])
        );
        // …and are inert while the table is not active.
        assert_eq!(km.lookup_seq(&[chord("ctrl+a")]), Lookup::None);
    }

    #[test]
    fn table_actions_round_trip_through_their_names() {
        for name in [
            "activate_key_table:copy",
            "activate_key_table_once:copy",
            "deactivate_key_table",
            "deactivate_all_key_tables",
        ] {
            let a = Action::from_name(name).unwrap_or_else(|| panic!("parsing {name}"));
            assert_eq!(a.name(), name);
        }
    }

    #[test]
    fn shift_arrows_are_performable_adjust_selection_binds() {
        // Ghostty's defaults, and the flag is what keeps shift+arrow usable in
        // an editor: with no selection the key belongs to the shell.
        let km = Keymap::default();
        assert_eq!(
            km.lookup(&chord("shift+left")),
            Some(Action::AdjustSelection(SelectionAdjust::Left))
        );
        assert!(km.is_performable(&[chord("shift+down")]));
        // The scroll binds next to them are *not* performable — they always act.
        assert_eq!(
            km.lookup(&chord("shift+pageup")),
            Some(Action::ScrollPageUp)
        );
        assert!(!km.is_performable(&[chord("shift+pageup")]));
    }

    #[test]
    fn the_performable_trigger_flag_parses_and_can_be_unbound() {
        let km = Keymap::from_config(&[("performable:ctrl+alt+k".into(), "new_tab".into())]);
        assert_eq!(km.lookup(&chord("ctrl+alt+k")), Some(Action::NewTab));
        assert!(km.is_performable(&[chord("ctrl+alt+k")]));
        // A plain binding of the same chord clears the flag rather than keeping
        // a stale one.
        let km = Keymap::from_config(&[
            ("performable:ctrl+alt+k".into(), "new_tab".into()),
            ("ctrl+alt+k".into(), "new_tab".into()),
        ]);
        assert!(!km.is_performable(&[chord("ctrl+alt+k")]));
    }

    #[test]
    fn a_global_trigger_binds_globally_and_not_in_the_ordinary_keymap() {
        let km =
            Keymap::from_config(&[("global:ctrl+alt+g".into(), "toggle_quick_terminal".into())]);
        assert_eq!(
            km.globals(),
            [(chord("ctrl+alt+g"), Action::ToggleQuickTerminal)]
        );
        // Not also in `binds`: the OS hook delivers it whether or not geist is
        // focused, so a second copy here would run the action twice.
        assert_eq!(km.lookup(&chord("ctrl+alt+g")), None);
        assert!(!km.starts_binding(&chord("ctrl+alt+g")));
    }

    #[test]
    fn a_global_trigger_can_be_rebound_and_unbound() {
        let km = Keymap::from_config(&[
            ("global:ctrl+alt+g".into(), "toggle_quick_terminal".into()),
            ("global:ctrl+alt+g".into(), "new_window".into()),
        ]);
        assert_eq!(km.globals(), [(chord("ctrl+alt+g"), Action::NewWindow)]);

        let km = Keymap::from_config(&[
            ("global:ctrl+alt+g".into(), "toggle_quick_terminal".into()),
            ("global:ctrl+alt+g".into(), "unbind".into()),
        ]);
        assert!(km.globals().is_empty());
    }

    #[test]
    fn a_global_trigger_is_case_insensitive_and_tolerates_spacing() {
        let km = Keymap::from_config(&[("GLOBAL: ctrl+alt+g".into(), "new_window".into())]);
        assert_eq!(km.globals().len(), 1);
    }

    #[test]
    fn a_bad_global_trigger_is_skipped_without_touching_the_keymap() {
        let km = Keymap::from_config(&[("global:ctrl+nope".into(), "new_window".into())]);
        assert!(km.globals().is_empty());
        // The defaults still work — one bad line must not take the file down.
        assert_eq!(km.lookup(&chord("ctrl+shift+t")), Some(Action::NewTab));
    }

    #[test]
    fn there_are_no_global_bindings_by_default() {
        // A low-level keyboard hook is a system-wide cost; nothing installs one
        // until the user asks for it.
        assert!(Keymap::default().globals().is_empty());
    }

    fn chord(s: &str) -> Chord {
        parse_chord(s).unwrap_or_else(|| panic!("chord {s:?} should parse"))
    }

    fn seq(s: &str) -> Vec<Chord> {
        parse_sequence(s).unwrap_or_else(|| panic!("sequence {s:?} should parse"))
    }

    /// A keymap with one two-key sequence bound, for the state-machine tests.
    fn with_sequence() -> Keymap {
        Keymap::from_config(&[("ctrl+a>n".into(), "new_tab".into())])
    }

    #[test]
    fn newly_wired_ghostty_actions_parse() {
        for (name, want) in [
            ("clear_screen", Action::ClearScreen),
            ("copy_title_to_clipboard", Action::CopyTitle),
            ("toggle_readonly", Action::ToggleReadonly),
            ("prompt_tab_title", Action::PromptTabTitle),
            ("quit", Action::Quit),
        ] {
            assert_eq!(Action::from_name(name), Some(want.clone()), "{name}");
            assert_eq!(want.name(), name, "name() must round-trip");
        }
        // Ghostty's alias for quit-by-closing-everything.
        assert_eq!(Action::from_name("close_all_windows"), Some(Action::Quit));
        assert_eq!(
            Action::from_name("equalize_splits"),
            Some(Action::EqualizeSplits)
        );
    }

    #[test]
    fn parameterised_actions_parse_and_clamp() {
        assert_eq!(Action::from_name("move_tab:1"), Some(Action::MoveTab(1)));
        assert_eq!(Action::from_name("move_tab:-2"), Some(Action::MoveTab(-2)));
        assert_eq!(Action::from_name("move_tab:x"), None);

        assert_eq!(
            Action::from_name("set_font_size:14"),
            Some(Action::SetFontSize(14))
        );
        // Ghostty's parameter is a float; geist's font size is whole points, so
        // round rather than reject — 13.5 meaning 14 beats doing nothing.
        assert_eq!(
            Action::from_name("set_font_size:13.5"),
            Some(Action::SetFontSize(14))
        );
        assert_eq!(Action::from_name("set_font_size:0"), None);

        assert_eq!(
            Action::from_name("scroll_page_lines:-5"),
            Some(Action::ScrollLines(-5))
        );
        // Stored x100 so `Action` stays `Copy + Eq` without carrying a float.
        assert_eq!(
            Action::from_name("scroll_page_fractional:0.5"),
            Some(Action::ScrollPageFraction(50))
        );
        assert_eq!(
            Action::from_name("scroll_page_fractional:-1"),
            Some(Action::ScrollPageFraction(-100))
        );

        // Every one round-trips through `name()`, which is what makes a config
        // written by geist re-readable by geist.
        for a in [
            Action::MoveTab(-2),
            Action::SetFontSize(14),
            Action::ScrollLines(-5),
            Action::ScrollPageFraction(50),
        ] {
            assert_eq!(
                Action::from_name(&a.name()),
                Some(a.clone()),
                "{}",
                a.name()
            );
        }
    }

    #[test]
    fn window_toggle_actions_use_ghosttys_names() {
        // The names are the compatibility surface: a Ghostty config must bind.
        for (name, want) in [
            ("toggle_maximize", Action::ToggleMaximize),
            ("toggle_window_float_on_top", Action::ToggleFloatOnTop),
            ("toggle_background_opacity", Action::ToggleBackgroundOpacity),
        ] {
            assert_eq!(Action::from_name(name), Some(want.clone()), "{name}");
            assert_eq!(want.name(), name, "name() must round-trip");
        }
    }

    #[test]
    fn write_file_actions_parse_and_round_trip() {
        use crate::writefile::{WriteAction, WriteScope};

        // All three scopes × all three path actions.
        for (name, scope) in [
            ("write_scrollback_file", WriteScope::Scrollback),
            ("write_screen_file", WriteScope::Screen),
            ("write_selection_file", WriteScope::Selection),
        ] {
            for act in [WriteAction::Copy, WriteAction::Paste, WriteAction::Open] {
                let spec = format!("{name}:{}", act.name());
                let parsed =
                    Action::from_name(&spec).unwrap_or_else(|| panic!("{spec} should parse"));
                assert_eq!(parsed, Action::WriteFile(scope, act));
                assert_eq!(parsed.name(), spec, "name() must round-trip");
            }
        }

        // The parameter is required: Ghostty has no default, and inventing one
        // would make a typo silently do something other than what was written.
        assert_eq!(Action::from_name("write_screen_file"), None);
        assert_eq!(Action::from_name("write_screen_file:"), None);
        assert_eq!(Action::from_name("write_screen_file:email"), None);
        assert_eq!(Action::from_name("write_nonsense_file:copy"), None);
    }

    #[test]
    fn parses_a_multi_key_sequence() {
        let s = seq("ctrl+a>n");
        assert_eq!(s.len(), 2);
        assert!(s[0].mods.ctrl && s[0].code == KeyCode::A);
        assert!(!s[1].mods.ctrl && s[1].code == KeyCode::N);

        // A plain chord is a one-element sequence — that is what lets single
        // binds and sequences share one lookup path.
        assert_eq!(seq("ctrl+shift+t").len(), 1);
        // Whitespace around the separator is tolerated.
        assert_eq!(seq("ctrl+a > n").len(), 2);
        // Three keys work as well as two.
        assert_eq!(seq("ctrl+a>b>c").len(), 3);
        // A malformed element rejects the whole trigger rather than binding a
        // truncated prefix, which would silently steal a key.
        assert!(parse_sequence("ctrl+a>").is_none());
        assert!(parse_sequence(">n").is_none());
        assert!(parse_sequence("ctrl+a>nonsensekey").is_none());
    }

    #[test]
    fn a_sequence_resolves_one_key_at_a_time() {
        let km = with_sequence();
        let a = chord("ctrl+a");
        let n = chord("n");

        // The leader alone is not an action — it is a promise of one.
        assert_eq!(km.lookup_seq(&[a]), Lookup::Pending);
        assert_eq!(
            km.lookup(&a),
            None,
            "a leader must not resolve as an action"
        );
        // …and completing it runs the binding.
        assert_eq!(km.lookup_seq(&[a, n]), Lookup::Action(vec![Action::NewTab]));
        // A wrong second key is a dead end, not a partial match.
        assert_eq!(km.lookup_seq(&[a, chord("x")]), Lookup::None);
        // The second key on its own means nothing.
        assert_eq!(km.lookup_seq(&[n]), Lookup::None);
    }

    #[test]
    fn a_leader_is_reserved_from_the_shell() {
        // The whole feature hinges on this: `ctrl+a` is bound to no action, so a
        // plain `lookup` says "not ours" and the shell would receive it — and
        // the sequence would never begin.
        let km = with_sequence();
        assert!(km.starts_binding(&chord("ctrl+a")));
        assert!(!km.starts_binding(&chord("ctrl+q")));
        // A complete single binding still counts as starting one.
        assert!(km.starts_binding(&chord("ctrl+shift+t")));
    }

    #[test]
    fn an_exact_binding_beats_being_a_prefix() {
        // With both `ctrl+a` and `ctrl+a>n` bound, the bare `ctrl+a` must fire
        // immediately rather than hang waiting for a second key.
        let km = Keymap::from_config(&[
            ("ctrl+a>n".into(), "new_tab".into()),
            ("ctrl+a".into(), "new_window".into()),
        ]);
        assert_eq!(
            km.lookup_seq(&[chord("ctrl+a")]),
            Lookup::Action(vec![Action::NewWindow])
        );
        // The longer binding becomes unreachable, which is the user's choice to
        // make — but it must not break lookup.
        assert_eq!(
            km.lookup_seq(&[chord("ctrl+a"), chord("n")]),
            Lookup::Action(vec![Action::NewTab])
        );
    }

    #[test]
    fn sequences_can_be_rebound_and_unbound() {
        let km = Keymap::from_config(&[
            ("ctrl+a>n".into(), "new_tab".into()),
            ("ctrl+a>n".into(), "new_window".into()),
        ]);
        assert_eq!(
            km.lookup_seq(&[chord("ctrl+a"), chord("n")]),
            Lookup::Action(vec![Action::NewWindow]),
            "the later binding must win"
        );

        let km = Keymap::from_config(&[
            ("ctrl+a>n".into(), "new_tab".into()),
            ("ctrl+a>n".into(), "unbind".into()),
        ]);
        assert_eq!(km.lookup_seq(&[chord("ctrl+a"), chord("n")]), Lookup::None);
        // …and with the only sequence gone, the leader is released back to the
        // shell rather than being swallowed forever.
        assert!(!km.starts_binding(&chord("ctrl+a")));
    }

    #[test]
    fn unbinding_one_branch_keeps_the_others() {
        let km = Keymap::from_config(&[
            ("ctrl+a>n".into(), "new_tab".into()),
            ("ctrl+a>w".into(), "close_surface".into()),
            ("ctrl+a>n".into(), "unbind".into()),
        ]);
        assert_eq!(km.lookup_seq(&[chord("ctrl+a"), chord("n")]), Lookup::None);
        assert!(matches!(
            km.lookup_seq(&[chord("ctrl+a"), chord("w")]),
            Lookup::Action(_)
        ));
        assert!(
            km.starts_binding(&chord("ctrl+a")),
            "the leader still leads somewhere"
        );
    }

    #[test]
    fn defaults_are_unaffected_by_sequence_support() {
        // Every built-in is a one-key binding and must still resolve in one
        // press, with no pending state.
        let km = Keymap::default();
        assert_eq!(
            km.lookup_seq(&[chord("ctrl+shift+t")]),
            Lookup::Action(vec![Action::NewTab])
        );
        assert_eq!(km.lookup(&chord("ctrl+shift+t")), Some(Action::NewTab));
        assert_eq!(km.lookup_seq(&[]), Lookup::None);
    }

    #[test]
    fn parses_modifiers_and_key() {
        let c = chord("ctrl+shift+t");
        assert!(c.mods.ctrl && c.mods.shift && !c.mods.alt);
        assert_eq!(c.code, KeyCode::T);
    }

    #[test]
    fn modifier_aliases_and_case_insensitive() {
        assert_eq!(chord("Control+Option+Left"), chord("ctrl+alt+left"));
        assert!(chord("CMD+k").mods.sup);
    }

    #[test]
    fn rejects_modifier_only_or_unknown_key() {
        assert!(parse_chord("ctrl+shift").is_none());
        assert!(parse_chord("ctrl+nope").is_none());
    }

    #[test]
    fn default_keymap_resolves_core_shortcuts() {
        let km = Keymap::default();
        assert_eq!(km.lookup(&chord("ctrl+shift+t")), Some(Action::NewTab));
        assert_eq!(km.lookup(&chord("ctrl+shift+e")), Some(Action::SplitDown));
        assert_eq!(
            km.lookup(&chord("ctrl+alt+left")),
            Some(Action::FocusSplitLeft)
        );
        assert_eq!(km.lookup(&chord("alt+1")), Some(Action::GotoTab(0)));
        assert_eq!(km.lookup(&chord("alt+9")), Some(Action::LastTab));
        assert_eq!(km.lookup(&chord("ctrl+tab")), Some(Action::NextTab));
        // An unbound chord resolves to nothing.
        assert_eq!(km.lookup(&chord("ctrl+shift+j")), None);
    }

    #[test]
    fn every_chord_round_trips_through_its_config_spelling() {
        // `Chord::name` feeds the inspector's keyboard log, which exists to tell
        // you what to put in your config — so the string it prints has to parse
        // back to the chord it printed. Exhaustive over the key table, because a
        // single wrong row would be a name that silently doesn't bind.
        for &code in crate::engine::KeyCode::ALL {
            for mods in [
                KeyMods::default(),
                KeyMods {
                    ctrl: true,
                    ..Default::default()
                },
                KeyMods {
                    ctrl: true,
                    shift: true,
                    ..Default::default()
                },
                KeyMods {
                    sup: true,
                    ctrl: true,
                    alt: true,
                    shift: true,
                },
            ] {
                let chord = Chord { mods, code };
                let name = chord.name();
                assert_eq!(parse_chord(&name), Some(chord), "round-trip {name:?}");
            }
        }
    }

    #[test]
    fn ctrl_shift_i_toggles_the_inspector() {
        // Upstream's trigger, whose own comment records it as "matching
        // Chromium" — the devtools chord.
        let km = Keymap::default();
        assert_eq!(
            km.lookup(&chord("ctrl+shift+i")),
            Some(Action::Inspector(crate::inspector::InspectorMode::Toggle))
        );
        // Not performable: upstream binds it with a plain `put`, and the panel
        // should open whatever the pane is doing.
        assert!(!km.is_performable(&[chord("ctrl+shift+i")]));
    }

    #[test]
    fn escape_is_a_performable_end_search_bind() {
        // Upstream's non-Darwin default. The `performable:` flag is the whole
        // point: without it this bind would swallow Escape in every full-screen
        // program, which is about the worst regression a terminal can ship.
        let km = Keymap::default();
        assert_eq!(km.lookup(&chord("escape")), Some(Action::EndSearch));
        assert!(km.is_performable(&[chord("escape")]));
    }

    #[test]
    fn default_keymap_binds_undo_and_redo_performable() {
        let km = Keymap::default();
        assert_eq!(km.lookup(&chord("ctrl+shift+z")), Some(Action::Undo));
        assert_eq!(km.lookup(&chord("ctrl+shift+y")), Some(Action::Redo));
        // Performable, like upstream: with an empty stack the chord is the
        // shell's rather than being swallowed.
        assert!(km.is_performable(&[chord("ctrl+shift+z")]));
        assert!(km.is_performable(&[chord("ctrl+shift+y")]));
        // And bare ctrl+z stays the shell's under every circumstance.
        assert_eq!(km.lookup(&chord("ctrl+z")), None);
    }

    #[test]
    fn default_keymap_binds_new_window() {
        let km = Keymap::default();
        assert_eq!(km.lookup(&chord("ctrl+shift+n")), Some(Action::NewWindow));
    }

    #[test]
    fn default_keymap_leaves_alt_f4_unbound() {
        // Deliberate divergence from Ghostty: Windows already delivers Alt+F4 as
        // WM_CLOSE, which geist answers with the close-confirmation flow. Binding
        // `close_window` here too would raise an action *and* a close request in
        // the same pass.
        let km = Keymap::default();
        assert_eq!(km.lookup(&chord("alt+f4")), None);
        // …but a user can still opt in explicitly.
        let km = Keymap::from_config(&[("alt+f4".to_string(), "close_window".to_string())]);
        assert_eq!(km.lookup(&chord("alt+f4")), Some(Action::CloseWindow));
    }

    #[test]
    fn config_override_rebinds_and_adds() {
        let overrides = vec![
            ("ctrl+shift+t".to_string(), "close_tab".to_string()),
            ("ctrl+shift+r".to_string(), "reload_config".to_string()),
        ];
        let km = Keymap::from_config(&overrides);
        // Existing chord rebound to the new action.
        assert_eq!(km.lookup(&chord("ctrl+shift+t")), Some(Action::CloseTab));
        // New chord added.
        assert_eq!(
            km.lookup(&chord("ctrl+shift+r")),
            Some(Action::ReloadConfig)
        );
        // Untouched default still resolves.
        assert_eq!(km.lookup(&chord("ctrl+shift+e")), Some(Action::SplitDown));
    }

    #[test]
    fn config_unbind_removes_a_default() {
        let overrides = vec![("ctrl+shift+t".to_string(), "unbind".to_string())];
        let km = Keymap::from_config(&overrides);
        assert_eq!(km.lookup(&chord("ctrl+shift+t")), None);
    }

    #[test]
    fn action_name_roundtrips() {
        for a in [
            Action::NewTab,
            Action::SplitRight,
            Action::FocusSplitPrev,
            Action::GotoTab(2),
            Action::LastTab,
            Action::TogglePalette,
            Action::ReloadConfig,
            Action::ToggleSplitZoom,
            Action::ToggleFullscreen,
            Action::ScrollToRow(200),
        ] {
            assert_eq!(
                Action::from_name(&a.name()),
                Some(a.clone()),
                "roundtrip {a:?}"
            );
        }
    }

    /// The scrollback keys are keymap entries rather than a hardcoded branch in
    /// `decide_key`, which is what makes them *unbindable* — see below.
    #[test]
    fn default_keymap_binds_the_scrollback_keys() {
        let km = Keymap::default();
        assert_eq!(
            km.lookup(&chord("shift+pageup")),
            Some(Action::ScrollPageUp)
        );
        assert_eq!(
            km.lookup(&chord("shift+pagedown")),
            Some(Action::ScrollPageDown)
        );
        assert_eq!(km.lookup(&chord("shift+home")), Some(Action::ScrollToTop));
        assert_eq!(km.lookup(&chord("shift+end")), Some(Action::ScrollToBottom));
    }

    #[test]
    fn config_unbind_removes_shift_home() {
        let overrides = vec![("shift+home".to_string(), "unbind".to_string())];
        assert_eq!(
            Keymap::from_config(&overrides).lookup(&chord("shift+home")),
            None
        );
    }

    #[test]
    fn default_keymap_binds_fullscreen_and_zoom() {
        let km = Keymap::default();
        assert_eq!(
            km.lookup(&chord("ctrl+enter")),
            Some(Action::ToggleFullscreen)
        );
        assert_eq!(
            km.lookup(&chord("ctrl+shift+enter")),
            Some(Action::ToggleSplitZoom)
        );
    }
}
