//! The command palette's data model: the catalog of invokable [`Action`]s, the
//! fuzzy matcher that filters them, and the open-palette UI [`PaletteState`].
//!
//! This module is deliberately free of egui/app state so the interesting logic
//! (the action catalog and the fuzzy ranking) is pure and unit-testable. The
//! [`App`](crate::app) owns an `Option<PaletteState>`, renders it, and maps a
//! chosen `Action` onto its existing methods via `App::execute_action` —
//! mirroring how Ghostty's palette is a thin layer over its keybind actions.

use std::sync::Arc;

/// The live state a *performable* binding is judged against.
///
/// Deliberately a plain value rather than a borrow of the session: it is read by
/// the gate that decides whether the shell sees a key and by the gate that runs
/// the action, and those sit on opposite sides of the app.
#[derive(Clone, Copy, Debug, Default)]
pub struct PerformCtx<'a> {
    pub has_selection: bool,
    /// The keymap, so the key-table actions can be judged against which tables
    /// actually exist. Borrowed rather than pre-resolved into flags: the answer
    /// depends on the action's own payload, and pre-resolving it per call site
    /// is how the two gates would drift.
    pub keymap: Option<&'a crate::keybind::Keymap>,
    /// The active key-table stack, innermost last.
    pub tables: &'a [crate::keybind::TableEntry],
    /// Whether the undo/redo stacks have anything on them.
    pub undo: UndoState,
    /// Whether the focused pane has a search open.
    pub search_active: bool,
}

/// Whether `undo` / `redo` have anything to act on.
///
/// Mirrored from [`crate::undo::UndoStack`] onto each window every pass rather
/// than read from the app: the `performable:` gate runs deep inside a session's
/// key handling, which has no route back to `App`. One value rather than two
/// loose booleans, so the mirror is threaded as a unit and can't half-arrive.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct UndoState {
    pub can_undo: bool,
    pub can_redo: bool,
}

/// Whether `action` can do anything right now.
///
/// Only consulted for bindings flagged `performable:`; everything else runs
/// regardless. **One function for both gates** — a second opinion here means the
/// key is either swallowed and does nothing, or reaches the shell *and* runs the
/// action.
pub fn can_perform(action: &Action, ctx: PerformCtx<'_>) -> bool {
    match action {
        // Upstream returns "not performed" with no selection, letting the key
        // fall through to the terminal (`Surface.zig`'s `.adjust_selection`).
        Action::AdjustSelection(_) => ctx.has_selection,
        // Upstream: activating a table that doesn't exist, or one that is
        // already the innermost, "has no effect and performable will report
        // false". The second rule is what stops a table's own activation key
        // from stacking the same table forever — `A -> B -> A` is fine,
        // `A -> B -> B` is not.
        Action::ActivateKeyTable(name) | Action::ActivateKeyTableOnce(name) => {
            let exists = ctx.keymap.is_some_and(|km| km.has_table(name));
            let innermost = ctx.tables.last().map(|t| t.name.as_str());
            exists && innermost != Some(&**name)
        }
        Action::DeactivateKeyTable | Action::DeactivateAllKeyTables => !ctx.tables.is_empty(),
        // Upstream binds both `performable:`, and `NSUndoManager` reports
        // `canUndo`/`canRedo` — so with nothing to undo the chord is the
        // shell's, exactly like shift+arrow with no selection.
        Action::Undo => ctx.undo.can_undo,
        Action::Redo => ctx.undo.can_redo,
        // Upstream returns "not performed" when there is no search to end or
        // navigate. `escape` is bound to `end_search` by default, so this is
        // the whole reason a full-screen program still receives Escape.
        Action::EndSearch | Action::NavigateSearch(_) => ctx.search_active,
        // Upstream: no selection, nothing to search for.
        Action::SearchSelection => ctx.has_selection,
        // Upstream: an empty needle with no search running has nothing to stop.
        Action::Search(text) => !text.is_empty() || ctx.search_active,
        _ => true,
    }
}

/// Whether an action belongs to the **app** or to one **surface** (a pane).
///
/// Ghostty's `Binding.Action.scope`, and the thing `all:` dispatches on: an
/// app-scoped action runs once, a surface-scoped one runs on every surface.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Scope {
    App,
    Surface,
}

impl Action {
    /// This action's scope, **ported verbatim** from `Binding.zig`'s `scope()`
    /// rather than judged by what giest's implementation happens to touch.
    ///
    /// Several rows are counter-intuitive and are upstream's on purpose:
    /// `new_tab` / `goto_tab` / `toggle_readonly` are **surface**-scoped
    /// (upstream: "they are relevant to the surface they come from — `new_window`
    /// needs to be sourced to a surface so inheritance can be done correctly"),
    /// while `new_window`, `undo`/`redo` and `quit` are **app**-scoped. A
    /// hand-picked list here would be a second taxonomy free to drift from the
    /// one it is copying.
    pub fn scope(&self) -> Scope {
        use Scope::{App, Surface};
        match self {
            // `ignore`/`unbind` don't really matter; upstream says app.
            Action::Noop(_) => App,
            // Obviously app actions.
            Action::OpenConfig
            | Action::ReloadConfig
            | Action::Quit
            | Action::ToggleQuickTerminal
            | Action::Undo
            | Action::Redo
            // App, but special-cased in a surface context upstream.
            | Action::NewWindow => App,
            // Everything else is surface-scoped, including the "less obvious"
            // ones upstream calls out.
            _ => Surface,
        }
    }

    /// Whether `all:` can broadcast this action to every pane in giest.
    ///
    /// A subset of [`Scope::Surface`], and the split is **giest plumbing, not
    /// Ghostty semantics**: these are the actions whose execution touches only
    /// the focused *session*, so running them per pane is a loop over the same
    /// call. The window-structural remainder (new/close/goto tab, splits, focus
    /// moves) is surface-scoped upstream but cannot be sourced to a pane here —
    /// `execute_action` acts on the focused one — so `all:` runs those **once**.
    /// Fixing that means threading a target pane through `execute_action`, which
    /// is a refactor rather than a wiring change.
    pub fn broadcasts_to_panes(&self) -> bool {
        matches!(
            self,
            Action::SendText(_)
                | Action::SendCsi(_)
                | Action::SendEsc(_)
                | Action::Paste
                | Action::ClearScreen
                | Action::ResetTerminal
                | Action::SelectAll
                | Action::ClearSelection
                | Action::ToggleReadonly
                | Action::ToggleMouseReporting
                | Action::ScrollPageUp
                | Action::ScrollPageDown
                | Action::ScrollToTop
                | Action::ScrollToBottom
                | Action::ScrollLines(_)
                | Action::ScrollPageFraction(_)
                | Action::ScrollToRow(_)
                | Action::JumpToPrompt(_)
                | Action::AdjustSelection(_)
                | Action::SetSurfaceTitle(_)
        )
    }
}

/// The `<prefix>:<payload>` actions, whose payload is taken verbatim.
/// One table so parsing and [`Action::name`] cannot drift apart.
type PayloadCtor = fn(Arc<str>) -> Action;
const PAYLOAD_ACTIONS: &[(&str, PayloadCtor)] = &[
    ("text:", Action::SendText),
    ("activate_key_table:", Action::ActivateKeyTable),
    ("activate_key_table_once:", Action::ActivateKeyTableOnce),
    ("csi:", Action::SendCsi),
    ("esc:", Action::SendEsc),
    ("set_tab_title:", Action::SetTabTitle),
    ("set_surface_title:", Action::SetSurfaceTitle),
    // Checked before the plain-name table below, which is why `search:foo`
    // can coexist with `search_selection` — the prefixes don't overlap.
    ("search:", Action::Search),
];

/// Decode a `text:` payload's escape sequences.
///
/// Ghostty runs these through `config/string.zig`, which is **Zig string-literal
/// escapes**: `\n \r \t \\ \' \" \xNN \u{...}`. Ported rather than invented,
/// because a different grammar here is silently incompatible with a real Ghostty
/// config — the binding would "work" and send the wrong bytes.
///
/// Returns `None` on a malformed escape, which upstream also treats as a failure
/// of the whole payload rather than emitting it literally.
pub fn decode_escapes(s: &str) -> Option<String> {
    let mut out = String::with_capacity(s.len());
    let mut it = s.chars();
    while let Some(c) = it.next() {
        if c != '\\' {
            out.push(c);
            continue;
        }
        match it.next()? {
            'n' => out.push('\n'),
            'r' => out.push('\r'),
            't' => out.push('\t'),
            '\\' => out.push('\\'),
            '\'' => out.push('\''),
            '"' => out.push('"'),
            'x' => {
                // Exactly two hex digits, and the *byte* they name — so `\x1b`
                // is ESC. Zig requires both digits.
                let hi = it.next()?.to_digit(16)?;
                let lo = it.next()?.to_digit(16)?;
                out.push(char::from_u32(hi * 16 + lo)?);
            }
            'u' => {
                // `\u{1F600}` — braces required, hex inside.
                if it.next()? != '{' {
                    return None;
                }
                let mut hex = String::new();
                loop {
                    match it.next()? {
                        '}' => break,
                        d => hex.push(d),
                    }
                }
                out.push(char::from_u32(u32::from_str_radix(&hex, 16).ok()?)?);
            }
            _ => return None,
        }
    }
    Some(out)
}

/// Ghostty's `adjust_selection` parameter names, both ways.
fn adjust_name(d: crate::engine::SelectionAdjust) -> &'static str {
    use crate::engine::SelectionAdjust as A;
    match d {
        A::Left => "left",
        A::Right => "right",
        A::Up => "up",
        A::Down => "down",
        A::PageUp => "page_up",
        A::PageDown => "page_down",
        A::Home => "home",
        A::End => "end",
        A::BeginningOfLine => "beginning_of_line",
        A::EndOfLine => "end_of_line",
    }
}

fn adjust_from_name(s: &str) -> Option<crate::engine::SelectionAdjust> {
    use crate::engine::SelectionAdjust as A;
    Some(match s {
        "left" => A::Left,
        "right" => A::Right,
        "up" => A::Up,
        "down" => A::Down,
        "page_up" => A::PageUp,
        "page_down" => A::PageDown,
        "home" => A::Home,
        "end" => A::End,
        "beginning_of_line" => A::BeginningOfLine,
        "end_of_line" => A::EndOfLine,
        _ => return None,
    })
}

/// A single thing the palette can do. Every variant maps to an existing
/// `App`/`Session` method in `App::execute_action`; the palette never reaches
/// into app internals itself. `Copy` so a chosen action survives past the UI
/// closure that produced it (the deferred-intent pattern used throughout `app.rs`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Action {
    /// Bind-able and does nothing, for a Ghostty action giest has no work to do
    /// for. Rejecting the name would log an "unknown action" the user cannot act
    /// on; mapping it onto some *other* action would silently do the wrong
    /// thing, which is exactly what `equalize_splits` used to do.
    Noop(Arc<str>),
    /// Move the selection's free end (Ghostty `adjust_selection:<direction>`).
    /// Bound to shift+arrows and **performable**: with no selection the key is
    /// the shell's.
    AdjustSelection(crate::engine::SelectionAdjust),
    /// Send literal text to the shell (Ghostty `text:`). The payload is stored
    /// **raw** and its escapes are decoded at send time, exactly as upstream
    /// does — which is also what makes `name()` round-trip without re-escaping.
    SendText(Arc<str>),
    /// Send `ESC [ <payload>` (Ghostty `csi:`). The payload is raw: upstream
    /// does no escape decoding for this one.
    SendCsi(Arc<str>),
    /// Send `ESC <payload>` (Ghostty `esc:`). Raw, like `csi:`.
    SendEsc(Arc<str>),
    /// Push a named key table onto the active stack (Ghostty
    /// `activate_key_table:<name>`).
    ActivateKeyTable(Arc<str>),
    /// Same, but the table pops as soon as one of its bindings runs (Ghostty
    /// `activate_key_table_once:<name>`).
    ActivateKeyTableOnce(Arc<str>),
    /// Pop the innermost key table (Ghostty `deactivate_key_table`).
    DeactivateKeyTable,
    /// Pop every active key table (Ghostty `deactivate_all_key_tables`).
    DeactivateAllKeyTables,
    /// Rename the current tab (Ghostty `set_tab_title:`).
    SetTabTitle(Arc<str>),
    /// Override the focused pane's title (Ghostty `set_surface_title:`). An
    /// empty value hands the title back to the program.
    SetSurfaceTitle(Arc<str>),
    NewTab,
    /// Open a new tab running the shell profile at this index.
    NewTabWithProfile(usize),
    /// Open a new OS window.
    NewWindow,
    /// Close the current window and everything in it.
    CloseWindow,
    CloseTab,
    CloseOtherTabs,
    CloseTabsToRight,
    NextTab,
    PrevTab,
    /// Jump to the tab at this 0-based index (Ghostty `goto_tab:N`, 1-based).
    GotoTab(u8),
    /// Jump to the last tab (Ghostty `last_tab`).
    LastTab,
    /// Open the command palette (Ghostty `toggle_command_palette`).
    TogglePalette,
    /// Write terminal text to a temp file and act on its **path** (Ghostty
    /// `write_scrollback_file` / `write_screen_file` / `write_selection_file`).
    WriteFile(crate::writefile::WriteScope, crate::writefile::WriteAction),
    /// Clear the screen **and** the scrollback. Ghostty `clear_screen`.
    ClearScreen,
    /// Copy the shell-set window/tab title. Ghostty `copy_title_to_clipboard`.
    CopyTitle,
    /// Refuse keyboard input to the shell until toggled back. Ghostty
    /// `toggle_readonly`.
    ToggleReadonly,
    /// Move the current tab by this many positions, clamped at the ends
    /// (Ghostty `move_tab:N`).
    MoveTab(i8),
    /// Set the font size to this many points (Ghostty `set_font_size:N`).
    SetFontSize(u8),
    /// Scroll by this many lines (Ghostty `scroll_page_lines:N`).
    ScrollLines(i16),
    /// Scroll by this fraction of a page (Ghostty `scroll_page_fractional:N`,
    /// scaled by 100 so the action stays `Copy` without a float).
    ScrollPageFraction(i16),
    /// Show, hide or toggle the terminal inspector on the focused pane
    /// (Ghostty `inspector:toggle|show|hide`).
    Inspector(crate::inspector::InspectorMode),
    /// Open the search overlay if it isn't open already (Ghostty
    /// `start_search`). Unlike [`Action::ToggleSearch`] a second press is a
    /// no-op, which is what makes it safe to bind alongside `end_search`.
    StartSearch,
    /// Close the search overlay (Ghostty `end_search`). **Performable only
    /// while a search is open** — that is what lets `escape` be bound to it and
    /// still reach a full-screen program when nothing is being searched.
    EndSearch,
    /// Step to the next (`true`) or previous match (Ghostty
    /// `navigate_search:next|previous`). Performable only while searching.
    NavigateSearch(bool),
    /// Search for the current selection (Ghostty `search_selection`), opening
    /// the overlay if needed. Performable only with a selection.
    SearchSelection,
    /// Set the needle directly (Ghostty `search:<text>`). An **empty** payload
    /// stops the search without closing the overlay, which is upstream's
    /// documented behaviour — `end_search` is the one that hides the UI.
    Search(Arc<str>),
    /// Reverse the last undoable structural change — a closed split, tab or
    /// window comes back with its shell still running; a newly created one
    /// goes away again. Ghostty `undo`.
    Undo,
    /// Re-apply the change `Undo` took back. Ghostty `redo`.
    Redo,
    /// Close every window, quitting giest. Ghostty `close_all_windows` / `quit`.
    Quit,
    /// Open the inline tab-rename box. Ghostty `prompt_tab_title`.
    PromptTabTitle,
    /// Maximize or restore the window. Ghostty `toggle_maximize` — which it
    /// notes has no effect on macOS; Windows has real maximize, so this does.
    ToggleMaximize,
    /// Keep the window above others even when unfocused. Ghostty
    /// `toggle_window_float_on_top` (macOS-only upstream).
    ToggleFloatOnTop,
    /// Flip between the configured `background-opacity` and fully opaque.
    /// Ghostty `toggle_background_opacity` (macOS-only upstream).
    ToggleBackgroundOpacity,
    /// Turn mouse reporting on/off for the focused pane, so a full-screen app
    /// that captured the pointer can be escaped without quitting it.
    /// Ghostty `toggle_mouse_reporting`.
    ToggleMouseReporting,
    /// Toggle the scrollback-search overlay on the focused pane.
    ToggleSearch,
    /// Jump to the previous (`delta < 0`) or next (`delta > 0`) OSC 133 prompt
    /// (Ghostty `jump_to_prompt:N`).
    JumpToPrompt(i8),
    SplitRight,
    SplitDown,
    /// Zoom the focused split to fill the tab, hiding the other panes; toggles
    /// (Ghostty `toggle_split_zoom`).
    ToggleSplitZoom,
    ClosePane,
    /// Toggle the window between fullscreen and windowed (Ghostty
    /// `toggle_fullscreen`).
    ToggleFullscreen,
    /// Show/hide the dropdown "quick terminal" (`toggle_quick_terminal`).
    ToggleQuickTerminal,
    FocusSplitLeft,
    FocusSplitRight,
    FocusSplitUp,
    FocusSplitDown,
    FocusSplitNext,
    FocusSplitPrev,
    IncreaseFontSize,
    DecreaseFontSize,
    ResetFontSize,
    Copy,
    Paste,
    SelectAll,
    ClearSelection,
    ResetTerminal,
    ScrollPageUp,
    ScrollPageDown,
    ScrollToTop,
    ScrollToBottom,
    /// Scroll so this absolute row (from the top of scrollback) is at the
    /// viewport top. Ghostty `scroll_to_row:N`, which upstream leaves unbound —
    /// it exists so the scrollbar can drive the core.
    ScrollToRow(u32),
    OpenConfig,
    ReloadConfig,
}

impl Action {
    /// The human-readable title shown in the palette. Exhaustive so adding an
    /// `Action` variant is a compile error until it gets a title (the per-profile
    /// `NewTabWithProfile` title is replaced at catalog-build time with the
    /// shell's name).
    fn title(&self) -> &'static str {
        match self {
            // Never listed in the palette (see `CATALOG`), but `title` must be
            // total.
            Action::Noop(_) => "Do Nothing",
            Action::AdjustSelection(_) => "Adjust Selection",
            Action::SendText(_) => "Send Text",
            Action::SendCsi(_) => "Send CSI Sequence",
            Action::SendEsc(_) => "Send Escape Sequence",
            Action::ActivateKeyTable(_) => "Activate Key Table",
            Action::ActivateKeyTableOnce(_) => "Activate Key Table (Once)",
            Action::DeactivateKeyTable => "Deactivate Key Table",
            Action::DeactivateAllKeyTables => "Deactivate All Key Tables",
            Action::SetTabTitle(_) => "Set Tab Title",
            Action::SetSurfaceTitle(_) => "Set Pane Title",
            Action::NewTab => "New Tab",
            Action::NewTabWithProfile(_) => "New Tab with Shell",
            Action::NewWindow => "New Window",
            Action::CloseWindow => "Close Window",
            Action::CloseTab => "Close Tab",
            Action::CloseOtherTabs => "Close Other Tabs",
            Action::CloseTabsToRight => "Close Tabs to the Right",
            Action::NextTab => "Next Tab",
            Action::PrevTab => "Previous Tab",
            Action::GotoTab(_) => "Go to Tab",
            Action::LastTab => "Go to Last Tab",
            Action::TogglePalette => "Command Palette",
            Action::WriteFile(scope, _) => match scope {
                crate::writefile::WriteScope::Scrollback => "Write Scrollback to File",
                crate::writefile::WriteScope::Screen => "Write Screen to File",
                crate::writefile::WriteScope::Selection => "Write Selection to File",
            },
            Action::ClearScreen => "Clear Screen",
            Action::CopyTitle => "Copy Title",
            Action::ToggleReadonly => "Toggle Read-Only",
            Action::MoveTab(_) => "Move Tab",
            Action::SetFontSize(_) => "Set Font Size",
            Action::ScrollLines(_) => "Scroll Lines",
            Action::ScrollPageFraction(_) => "Scroll Page",
            Action::Inspector(_) => "Terminal Inspector",
            Action::StartSearch => "Search",
            Action::EndSearch => "End Search",
            Action::NavigateSearch(true) => "Find Next",
            Action::NavigateSearch(false) => "Find Previous",
            Action::SearchSelection => "Search Selection",
            Action::Search(_) => "Search For",
            Action::Undo => "Undo",
            Action::Redo => "Redo",
            Action::Quit => "Quit",
            Action::PromptTabTitle => "Rename Tab",
            Action::ToggleMaximize => "Toggle Maximize",
            Action::ToggleFloatOnTop => "Toggle Always on Top",
            Action::ToggleBackgroundOpacity => "Toggle Background Opacity",
            Action::ToggleMouseReporting => "Toggle Mouse Reporting",
            Action::ToggleSearch => "Search Scrollback",
            Action::JumpToPrompt(d) => {
                if *d < 0 {
                    "Jump to Previous Prompt"
                } else {
                    "Jump to Next Prompt"
                }
            }
            Action::SplitRight => "Split Right",
            Action::SplitDown => "Split Down",
            Action::ToggleSplitZoom => "Toggle Split Zoom",
            Action::ClosePane => "Close Pane",
            Action::ToggleFullscreen => "Toggle Fullscreen",
            Action::ToggleQuickTerminal => "Toggle Quick Terminal",
            Action::FocusSplitLeft => "Focus Split: Left",
            Action::FocusSplitRight => "Focus Split: Right",
            Action::FocusSplitUp => "Focus Split: Up",
            Action::FocusSplitDown => "Focus Split: Down",
            Action::FocusSplitNext => "Focus Split: Next",
            Action::FocusSplitPrev => "Focus Split: Previous",
            Action::IncreaseFontSize => "Increase Font Size",
            Action::DecreaseFontSize => "Decrease Font Size",
            Action::ResetFontSize => "Reset Font Size",
            Action::Copy => "Copy",
            Action::Paste => "Paste",
            Action::SelectAll => "Select All",
            Action::ClearSelection => "Clear Selection",
            Action::ResetTerminal => "Reset Terminal",
            Action::ScrollPageUp => "Scroll Page Up",
            Action::ScrollPageDown => "Scroll Page Down",
            Action::ScrollToTop => "Scroll to Top",
            Action::ScrollToBottom => "Scroll to Bottom",
            Action::ScrollToRow(_) => "Scroll to Row",
            Action::OpenConfig => "Open Config",
            Action::ReloadConfig => "Reload Config",
        }
    }

    /// The default keybinding label shown right-aligned in the palette (matching
    /// the bindings in `handle_shortcuts`/`decide_key`), or `None` for actions
    /// reachable only via the palette/menus.
    fn keybind(&self) -> Option<&'static str> {
        Some(match self {
            Action::Noop(_)
            | Action::AdjustSelection(_)
            | Action::SendText(_)
            | Action::SendCsi(_)
            | Action::SendEsc(_)
            | Action::ActivateKeyTable(_)
            | Action::ActivateKeyTableOnce(_)
            | Action::DeactivateKeyTable
            | Action::DeactivateAllKeyTables
            | Action::SetTabTitle(_)
            | Action::SetSurfaceTitle(_) => return None,
            Action::NewTab => "Ctrl+Shift+T",
            Action::NewWindow => "Ctrl+Shift+N",
            Action::Inspector(_) => "Ctrl+Shift+I",
            Action::EndSearch => "Esc",
            Action::StartSearch
            | Action::NavigateSearch(_)
            | Action::SearchSelection
            | Action::Search(_) => return None,
            Action::Undo => "Ctrl+Shift+Z",
            Action::Redo => "Ctrl+Shift+Y",
            Action::NextTab => "Ctrl+Tab",
            Action::PrevTab => "Ctrl+Shift+Tab",
            Action::LastTab => "Alt+9",
            Action::TogglePalette => "Ctrl+Shift+P",
            Action::WriteFile(..)
            | Action::ClearScreen
            | Action::CopyTitle
            | Action::ToggleReadonly
            | Action::MoveTab(_)
            | Action::SetFontSize(_)
            | Action::ScrollLines(_)
            | Action::ScrollPageFraction(_)
            | Action::Quit
            | Action::PromptTabTitle
            | Action::ToggleMaximize
            | Action::ToggleFloatOnTop
            | Action::ToggleBackgroundOpacity
            | Action::ToggleMouseReporting => return None,
            Action::ToggleSearch => "Ctrl+Shift+F",
            Action::JumpToPrompt(d) if *d < 0 => "Ctrl+Shift+\u{2191}",
            Action::JumpToPrompt(_) => "Ctrl+Shift+\u{2193}",
            Action::SplitRight => "Ctrl+Shift+D",
            Action::SplitDown => "Ctrl+Shift+E",
            Action::ToggleSplitZoom => "Ctrl+Shift+Enter",
            Action::ClosePane => "Ctrl+Shift+W",
            Action::ToggleFullscreen => "Ctrl+Enter",
            Action::ToggleQuickTerminal => "",
            Action::FocusSplitLeft => "Ctrl+Alt+\u{2190}",
            Action::FocusSplitRight => "Ctrl+Alt+\u{2192}",
            Action::FocusSplitUp => "Ctrl+Alt+\u{2191}",
            Action::FocusSplitDown => "Ctrl+Alt+\u{2193}",
            Action::FocusSplitNext => "Ctrl+Shift+]",
            Action::FocusSplitPrev => "Ctrl+Shift+[",
            Action::IncreaseFontSize => "Ctrl++",
            Action::DecreaseFontSize => "Ctrl+-",
            Action::ResetFontSize => "Ctrl+0",
            Action::Copy => "Ctrl+Shift+C",
            Action::Paste => "Ctrl+Shift+V",
            Action::ScrollPageUp => "Shift+PgUp",
            Action::ScrollPageDown => "Shift+PgDn",
            Action::ScrollToTop => "Shift+Home",
            Action::ScrollToBottom => "Shift+End",
            // No default binding (or per-index, shown elsewhere).
            Action::NewTabWithProfile(_)
            | Action::GotoTab(_)
            | Action::ScrollToRow(_)
            // Deliberately unbound: Windows already delivers Alt+F4 as WM_CLOSE,
            // which giest answers with the close-confirmation flow. See keybind.rs.
            | Action::CloseWindow
            | Action::CloseTab
            | Action::CloseOtherTabs
            | Action::CloseTabsToRight
            | Action::SelectAll
            | Action::ClearSelection
            | Action::ResetTerminal
            | Action::OpenConfig
            | Action::ReloadConfig => return None,
        })
    }

    /// The action's stable, Ghostty-style name — the identifier used in
    /// `keybind = <trigger>=<name>` config lines and the inverse of
    /// [`Action::from_name`]. Parametrized actions encode the parameter
    /// (`goto_tab:2`, 1-based like Ghostty).
    pub fn name(&self) -> String {
        match self {
            Action::Noop(name) => name.to_string(),
            Action::AdjustSelection(d) => format!("adjust_selection:{}", adjust_name(*d)),
            // Payloads are stored raw, so these round-trip verbatim.
            Action::SendText(s) => format!("text:{s}"),
            Action::SendCsi(s) => format!("csi:{s}"),
            Action::SendEsc(s) => format!("esc:{s}"),
            Action::ActivateKeyTable(s) => format!("activate_key_table:{s}"),
            Action::ActivateKeyTableOnce(s) => format!("activate_key_table_once:{s}"),
            Action::DeactivateKeyTable => "deactivate_key_table".into(),
            Action::DeactivateAllKeyTables => "deactivate_all_key_tables".into(),
            Action::SetTabTitle(s) => format!("set_tab_title:{s}"),
            Action::SetSurfaceTitle(s) => format!("set_surface_title:{s}"),
            Action::NewTab => "new_tab".into(),
            Action::NewTabWithProfile(i) => format!("new_tab_with_profile:{i}"),
            Action::NewWindow => "new_window".into(),
            Action::CloseWindow => "close_window".into(),
            Action::CloseTab => "close_tab".into(),
            Action::CloseOtherTabs => "close_other_tabs".into(),
            Action::CloseTabsToRight => "close_tabs_to_right".into(),
            Action::NextTab => "next_tab".into(),
            Action::PrevTab => "previous_tab".into(),
            Action::GotoTab(i) => format!("goto_tab:{}", *i as u16 + 1),
            Action::LastTab => "last_tab".into(),
            Action::SplitRight => "new_split:right".into(),
            Action::SplitDown => "new_split:down".into(),
            Action::ToggleSplitZoom => "toggle_split_zoom".into(),
            Action::ClosePane => "close_surface".into(),
            Action::ToggleFullscreen => "toggle_fullscreen".into(),
            Action::ToggleQuickTerminal => "toggle_quick_terminal".into(),
            Action::FocusSplitLeft => "goto_split:left".into(),
            Action::FocusSplitRight => "goto_split:right".into(),
            Action::FocusSplitUp => "goto_split:up".into(),
            Action::FocusSplitDown => "goto_split:down".into(),
            Action::FocusSplitNext => "goto_split:next".into(),
            Action::FocusSplitPrev => "goto_split:previous".into(),
            Action::IncreaseFontSize => "increase_font_size".into(),
            Action::DecreaseFontSize => "decrease_font_size".into(),
            Action::ResetFontSize => "reset_font_size".into(),
            Action::Copy => "copy_to_clipboard".into(),
            Action::Paste => "paste_from_clipboard".into(),
            Action::SelectAll => "select_all".into(),
            Action::ClearSelection => "clear_selection".into(),
            Action::ResetTerminal => "reset".into(),
            Action::ScrollPageUp => "scroll_page_up".into(),
            Action::ScrollPageDown => "scroll_page_down".into(),
            Action::ScrollToTop => "scroll_to_top".into(),
            Action::ScrollToBottom => "scroll_to_bottom".into(),
            Action::ScrollToRow(n) => format!("scroll_to_row:{n}"),
            Action::OpenConfig => "open_config".into(),
            Action::ReloadConfig => "reload_config".into(),
            Action::TogglePalette => "toggle_command_palette".into(),
            Action::WriteFile(scope, act) => {
                format!("write_{}_file:{}", scope.name(), act.name())
            }
            Action::ClearScreen => "clear_screen".into(),
            Action::CopyTitle => "copy_title_to_clipboard".into(),
            Action::ToggleReadonly => "toggle_readonly".into(),
            Action::MoveTab(n) => format!("move_tab:{n}"),
            Action::SetFontSize(n) => format!("set_font_size:{n}"),
            Action::ScrollLines(n) => format!("scroll_page_lines:{n}"),
            Action::ScrollPageFraction(n) => {
                format!("scroll_page_fractional:{}", f32::from(*n) / 100.0)
            }
            Action::Inspector(m) => format!("inspector:{}", m.name()),
            Action::StartSearch => "start_search".into(),
            Action::EndSearch => "end_search".into(),
            Action::NavigateSearch(next) => {
                let dir = if *next { "next" } else { "previous" };
                format!("navigate_search:{dir}")
            }
            Action::SearchSelection => "search_selection".into(),
            Action::Search(t) => format!("search:{t}"),
            Action::Undo => "undo".into(),
            Action::Redo => "redo".into(),
            Action::Quit => "quit".into(),
            Action::PromptTabTitle => "prompt_tab_title".into(),
            Action::ToggleMaximize => "toggle_maximize".into(),
            Action::ToggleFloatOnTop => "toggle_window_float_on_top".into(),
            Action::ToggleBackgroundOpacity => "toggle_background_opacity".into(),
            Action::ToggleMouseReporting => "toggle_mouse_reporting".into(),
            Action::ToggleSearch => "toggle_search".into(),
            Action::JumpToPrompt(d) => format!("jump_to_prompt:{d}"),
        }
    }

    /// Parse a Ghostty-style action name (the value side of a `keybind` line)
    /// into an [`Action`]. Accepts the canonical names from [`Action::name`] plus
    /// a few common aliases; returns `None` for unknown or unsupported actions.
    /// `NewTabWithProfile` is intentionally not parseable (it is profile-relative
    /// and built only from the live profile list).
    pub fn from_name(s: &str) -> Option<Action> {
        // The payload-carrying actions are matched against the **untrimmed**
        // input: a trailing space in `text:hello ` is part of the text, and
        // trimming `csi:0m ` would silently change the sequence sent.
        for (prefix, make) in PAYLOAD_ACTIONS {
            if let Some(rest) = s.strip_prefix(*prefix) {
                return Some(make(Arc::from(rest)));
            }
        }
        let s = s.trim();
        if let Some(rest) = s.strip_prefix("goto_tab:") {
            // Ghostty's goto_tab is 1-based; giest indexes tabs from 0.
            let n: u16 = rest.trim().parse().ok()?;
            return n.checked_sub(1).map(|i| Action::GotoTab(i.min(u8::MAX as u16) as u8));
        }
        if let Some(rest) = s.strip_prefix("scroll_to_row:") {
            return rest.trim().parse().ok().map(Action::ScrollToRow);
        }
        if let Some(rest) = s.strip_prefix("move_tab:") {
            return rest.parse::<i8>().ok().map(Action::MoveTab);
        }
        // All ten upstream directions are accepted even though only the four
        // arrows are bound by default on this platform — a config may use any,
        // and the binding implements all ten.
        if let Some(rest) = s.strip_prefix("adjust_selection:") {
            return adjust_from_name(rest.trim()).map(Action::AdjustSelection);
        }
        if let Some(rest) = s.strip_prefix("set_font_size:") {
            // Ghostty takes a float; giest's font size is whole points, so round
            // rather than reject — `set_font_size:13.5` asking for 14 is closer
            // to the intent than doing nothing.
            return rest
                .parse::<f32>()
                .ok()
                .filter(|n| *n >= 1.0 && *n <= 255.0)
                .map(|n| Action::SetFontSize(n.round() as u8));
        }
        if let Some(rest) = s.strip_prefix("scroll_page_lines:") {
            return rest.parse::<i16>().ok().map(Action::ScrollLines);
        }
        if let Some(rest) = s.strip_prefix("scroll_page_fractional:") {
            // Stored ×100 so `Action` stays `Copy`-and-`Eq` without a float.
            return rest
                .parse::<f32>()
                .ok()
                .filter(|n| n.is_finite())
                .map(|n| Action::ScrollPageFraction((n * 100.0).round() as i16));
        }
        if let Some(rest) = s.strip_prefix("inspector:") {
            return crate::inspector::InspectorMode::from_name(rest).map(Action::Inspector);
        }
        if let Some(rest) = s.strip_prefix("navigate_search:") {
            return match rest.trim() {
                "next" => Some(Action::NavigateSearch(true)),
                "previous" | "prev" => Some(Action::NavigateSearch(false)),
                _ => None,
            };
        }
        if let Some(rest) = s.strip_prefix("jump_to_prompt:") {
            let n: i8 = rest.trim().parse().ok()?;
            return Some(Action::JumpToPrompt(n));
        }
        if let Some(rest) = s.strip_prefix("new_split:") {
            // giest splits the focused pane; left/right share an axis, up/down too.
            return match rest.trim() {
                "right" | "left" => Some(Action::SplitRight),
                "down" | "up" => Some(Action::SplitDown),
                _ => None,
            };
        }
        if let Some(rest) = s.strip_prefix("goto_split:") {
            return match rest.trim() {
                "left" => Some(Action::FocusSplitLeft),
                "right" => Some(Action::FocusSplitRight),
                "up" => Some(Action::FocusSplitUp),
                "down" => Some(Action::FocusSplitDown),
                "next" => Some(Action::FocusSplitNext),
                "previous" | "prev" => Some(Action::FocusSplitPrev),
                _ => None,
            };
        }
        Some(match s {
            "new_tab" => Action::NewTab,
            "new_window" => Action::NewWindow,
            "close_window" => Action::CloseWindow,
            "close_tab" => Action::CloseTab,
            "close_other_tabs" => Action::CloseOtherTabs,
            "close_tabs_to_right" => Action::CloseTabsToRight,
            "next_tab" => Action::NextTab,
            "previous_tab" | "prev_tab" => Action::PrevTab,
            "last_tab" => Action::LastTab,
            "close_surface" | "close_pane" => Action::ClosePane,
            "toggle_split_zoom" => Action::ToggleSplitZoom,
            "toggle_fullscreen" => Action::ToggleFullscreen,
            "toggle_quick_terminal" => Action::ToggleQuickTerminal,
            "increase_font_size" => Action::IncreaseFontSize,
            "decrease_font_size" => Action::DecreaseFontSize,
            "reset_font_size" => Action::ResetFontSize,
            "copy_to_clipboard" | "copy" => Action::Copy,
            "paste_from_clipboard" | "paste" => Action::Paste,
            "select_all" => Action::SelectAll,
            "clear_selection" => Action::ClearSelection,
            "reset" | "reset_terminal" => Action::ResetTerminal,
            "scroll_page_up" => Action::ScrollPageUp,
            "scroll_page_down" => Action::ScrollPageDown,
            "scroll_to_top" => Action::ScrollToTop,
            "scroll_to_bottom" => Action::ScrollToBottom,
            "open_config" => Action::OpenConfig,
            "reload_config" => Action::ReloadConfig,
            "toggle_command_palette" => Action::TogglePalette,
            "clear_screen" => Action::ClearScreen,
            "copy_title_to_clipboard" => Action::CopyTitle,
            "toggle_readonly" => Action::ToggleReadonly,
            "undo" => Action::Undo,
            "redo" => Action::Redo,
            "quit" | "close_all_windows" => Action::Quit,
            "prompt_tab_title" => Action::PromptTabTitle,
            // giest splits are always 50/50, so there is nothing to equalize.
            // Accepted as a no-op so a Ghostty config binds without an error
            // rather than logging an "unknown action" the user cannot act on.
            // Ghostty's `ignore`: bind the key to nothing, black-holing it. Not
            // the same as `unbind`, which removes the binding and lets the key
            // reach the shell — that one is handled by the keymap.
            "ignore" => Action::Noop(Arc::from("ignore")),
            "equalize_splits" => Action::Noop(Arc::from("equalize_splits")),
            // macOS-only upstream and unimplementable on Windows (no API stops
            // other processes reading keystrokes) — accepted so a transferred
            // config binds without an "unknown action" the user cannot act on.
            "toggle_secure_input" => Action::Noop(Arc::from("toggle_secure_input")),
            "deactivate_key_table" => Action::DeactivateKeyTable,
            "deactivate_all_key_tables" => Action::DeactivateAllKeyTables,
            "toggle_maximize" => Action::ToggleMaximize,
            "toggle_window_float_on_top" => Action::ToggleFloatOnTop,
            "toggle_background_opacity" => Action::ToggleBackgroundOpacity,
            "toggle_mouse_reporting" => Action::ToggleMouseReporting,
            // `write_<scope>_file:<action>`. The parameter is required — Ghostty
            // has no default, and silently picking one would mean a typo does
            // something other than what was written.
            _ if s.starts_with("write_") => {
                let (name, param) = s.split_once(':')?;
                let scope = match name {
                    "write_scrollback_file" => crate::writefile::WriteScope::Scrollback,
                    "write_screen_file" => crate::writefile::WriteScope::Screen,
                    "write_selection_file" => crate::writefile::WriteScope::Selection,
                    _ => return None,
                };
                Action::WriteFile(scope, crate::writefile::WriteAction::from_name(param)?)
            }
            // `toggle_search` is giest's own: one key that opens *and* closes.
            // Upstream splits that into `start_search` / `end_search`, and both
            // are bindable here too — the toggle predates them and stays the
            // default because a single Ctrl+Shift+F is what Windows users reach
            // for. There is no bare `search` alias: `search` takes a payload
            // upstream (`search:foo`), so accepting the bare word for something
            // else would make a transferred config do the wrong thing silently.
            "toggle_search" => Action::ToggleSearch,
            "start_search" => Action::StartSearch,
            "end_search" => Action::EndSearch,
            "search_selection" => Action::SearchSelection,
            _ => return None,
        })
    }
}

/// Every catalog action except the per-profile `NewTabWithProfile` rows, which
/// are appended at build time (one per detected shell). Adding an `Action`
/// variant means adding it here too; `Action::title`/`keybind` being exhaustive
/// catches a forgotten title/binding at compile time.
const BASE_ACTIONS: &[Action] = &[
    Action::NewTab,
    Action::Undo,
    Action::Redo,
    Action::SearchSelection,
    Action::Inspector(crate::inspector::InspectorMode::Toggle),
    Action::NewWindow,
    Action::CloseWindow,
    Action::CloseTab,
    Action::CloseOtherTabs,
    Action::CloseTabsToRight,
    Action::NextTab,
    Action::PrevTab,
    Action::SplitRight,
    Action::SplitDown,
    Action::ToggleSplitZoom,
    Action::ClosePane,
    Action::ToggleFullscreen,
    Action::ToggleQuickTerminal,
    Action::FocusSplitLeft,
    Action::FocusSplitRight,
    Action::FocusSplitUp,
    Action::FocusSplitDown,
    Action::FocusSplitNext,
    Action::FocusSplitPrev,
    Action::IncreaseFontSize,
    Action::DecreaseFontSize,
    Action::ResetFontSize,
    Action::Copy,
    Action::Paste,
    Action::SelectAll,
    Action::ClearSelection,
    Action::ResetTerminal,
    Action::ScrollPageUp,
    Action::ScrollPageDown,
    Action::ScrollToTop,
    Action::ScrollToBottom,
    Action::JumpToPrompt(-1),
    Action::JumpToPrompt(1),
    Action::ToggleSearch,
    Action::ToggleMouseReporting,
    Action::ToggleMaximize,
    Action::ToggleFloatOnTop,
    Action::ToggleBackgroundOpacity,
    Action::OpenConfig,
    Action::ReloadConfig,
];

/// One palette entry: a display title, an optional keybinding label, and the
/// action it invokes. `title` is owned (not `&'static str`) because the
/// per-profile rows interpolate the shell's runtime name.
pub struct Command {
    pub title: String,
    pub keybind: Option<String>,
    pub action: Action,
}

impl Command {
    fn from_action(action: Action) -> Self {
        Self {
            title: action.title().to_string(),
            keybind: action.keybind().map(str::to_string),
            action,
        }
    }
}

/// The fixed command set (no per-profile rows). See [`build_catalog`] for the
/// catalog the palette actually shows.
pub fn base_catalog() -> Vec<Command> {
    BASE_ACTIONS.iter().cloned().map(Command::from_action).collect()
}

/// The full palette catalog: the [`base_catalog`] plus one "New Tab with
/// <shell>" row per detected profile, so a specific shell is one search away.
pub fn build_catalog(profile_names: &[String]) -> Vec<Command> {
    let mut catalog = base_catalog();
    for (i, name) in profile_names.iter().enumerate() {
        catalog.push(Command {
            title: format!("New Tab with {name}"),
            keybind: None,
            action: Action::NewTabWithProfile(i),
        });
    }
    catalog
}

/// The open command palette's transient UI state. Rebuilt each time the palette
/// opens (the catalog is captured then so filtering doesn't re-borrow the app).
pub struct PaletteState {
    /// The current search query.
    pub query: String,
    /// Index into the *filtered* list of the highlighted row.
    pub selected: usize,
    /// The command catalog, captured at open.
    pub catalog: Vec<Command>,
    /// True only on the first frame after opening, so the search box grabs
    /// keyboard focus exactly once (re-requesting every frame would fight any
    /// future in-palette focus moves).
    pub just_opened: bool,
}

impl PaletteState {
    pub fn new(catalog: Vec<Command>) -> Self {
        Self {
            query: String::new(),
            selected: 0,
            catalog,
            just_opened: true,
        }
    }
}

/// Case-insensitive fuzzy score of `query` against `candidate`, or `None` if not
/// every query char appears in order. Higher is better. An empty query matches
/// everything with a neutral score. See [`fuzzy_match_indices`].
pub fn fuzzy_match(query: &str, candidate: &str) -> Option<i32> {
    fuzzy_match_indices(query, candidate).map(|(score, _)| score)
}

/// Like [`fuzzy_match`] but also returns the candidate char indices that matched
/// (for bolding them in the UI). Greedy subsequence match with bonuses for
/// contiguous runs and word-boundary/initial hits, a small leading-gap penalty,
/// and a prefix/exact bonus — roughly Ghostty's "substring + initials" feel.
pub fn fuzzy_match_indices(query: &str, candidate: &str) -> Option<(i32, Vec<usize>)> {
    let query = query.trim();
    if query.is_empty() {
        return Some((0, Vec::new()));
    }
    let cand: Vec<char> = candidate.chars().collect();
    let needle: Vec<char> = query.chars().map(|c| c.to_ascii_lowercase()).collect();

    let mut score = 0i32;
    let mut indices = Vec::with_capacity(needle.len());
    let mut qi = 0usize;
    let mut prev_match: Option<usize> = None;
    let mut leading_gap = 0i32;
    for (ci, &ch) in cand.iter().enumerate() {
        if qi >= needle.len() {
            break;
        }
        if ch.to_ascii_lowercase() == needle[qi] {
            score += 1;
            if prev_match == Some(ci.wrapping_sub(1)) {
                score += 5; // contiguous with the previous match
            }
            let after_sep = ci > 0 && matches!(cand[ci - 1], ' ' | '-' | '/' | '_' | ':');
            let camel = ci > 0 && cand[ci - 1].is_ascii_lowercase() && ch.is_ascii_uppercase();
            if ci == 0 || after_sep || camel {
                score += 10; // start of a word / an initial
            }
            indices.push(ci);
            prev_match = Some(ci);
            qi += 1;
        } else if prev_match.is_none() {
            leading_gap += 1;
        }
    }
    if qi != needle.len() {
        return None;
    }
    score -= leading_gap.min(10);

    // Prefix / exact bonuses, computed on the lowercased candidate.
    let cand_lower: String = cand.iter().map(|c| c.to_ascii_lowercase()).collect();
    let needle_str: String = needle.iter().collect();
    if cand_lower == needle_str {
        score += 30;
    } else if cand_lower.starts_with(&needle_str) {
        score += 15;
    }
    Some((score, indices))
}

/// Filter and rank `catalog` against `query`, returning the surviving entries'
/// indices best-first (score desc, then title asc). An empty query returns every
/// index in catalog order.
pub fn filter_commands(catalog: &[Command], query: &str) -> Vec<usize> {
    if query.trim().is_empty() {
        return (0..catalog.len()).collect();
    }
    let mut scored: Vec<(usize, i32)> = catalog
        .iter()
        .enumerate()
        .filter_map(|(i, c)| fuzzy_match(query, &c.title).map(|s| (i, s)))
        .collect();
    scored.sort_by(|a, b| {
        b.1.cmp(&a.1)
            .then_with(|| catalog[a.0].title.cmp(&catalog[b.0].title))
    });
    scored.into_iter().map(|(i, _)| i).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn text_payload_escapes_follow_ghosttys_grammar() {
        // Ported from `config/string.zig`, which uses Zig string-literal
        // escapes. Inventing a grammar here would produce a binding that looks
        // right and sends the wrong bytes against a real Ghostty config.
        let d = |s: &str| decode_escapes(s);
        assert_eq!(d("hello").as_deref(), Some("hello"));
        assert_eq!(d(r"a\nb").as_deref(), Some("a\nb"));
        assert_eq!(d(r"\r\t").as_deref(), Some("\r\t"));
        assert_eq!(d(r"\\").as_deref(), Some("\\"));
        assert_eq!(d(r#"\'\""#).as_deref(), Some("'\""));
        // `\x1b` is ESC — the reason this grammar matters at all.
        assert_eq!(d(r"\x1bOA").as_deref(), Some("\x1bOA"));
        assert_eq!(d(r"\u{1F600}").as_deref(), Some("😀"));
        // Malformed escapes fail the whole payload rather than being emitted
        // literally, which is upstream's behaviour too.
        assert_eq!(d(r"\q"), None);
        assert_eq!(d(r"\x1"), None, "\\x needs two digits");
        // A `\u` escape without its braces (built by hand so the test source
        // itself can't be misread as a Rust escape).
        let no_braces = format!("{}u1F600", '\\');
        assert_eq!(d(&no_braces), None, "the u escape needs braces");
        assert_eq!(d("trailing\\"), None);
    }

    #[test]
    fn payload_actions_parse_verbatim_and_round_trip() {
        // The payload is *not* trimmed: a trailing space is part of the text,
        // and trimming `csi:0m ` would change the sequence sent.
        for raw in [
            "text:hello world",
            r"text:\x1bOA",
            "text:trailing ",
            "csi:0m",
            "esc:OA",
            "set_tab_title:build",
            "set_surface_title:",
        ] {
            let a = Action::from_name(raw).unwrap_or_else(|| panic!("parsing {raw:?}"));
            assert_eq!(a.name(), raw, "round-trip {raw:?}");
        }
        assert_eq!(
            Action::from_name("csi:0m"),
            Some(Action::SendCsi(Arc::from("0m")))
        );
        // An empty title payload is meaningful — it clears the override.
        assert_eq!(
            Action::from_name("set_surface_title:"),
            Some(Action::SetSurfaceTitle(Arc::from("")))
        );
    }

    #[test]
    fn the_inspector_action_parses_and_round_trips() {
        use crate::inspector::InspectorMode;
        for raw in ["inspector:toggle", "inspector:show", "inspector:hide"] {
            let a = Action::from_name(raw).unwrap_or_else(|| panic!("parsing {raw:?}"));
            assert_eq!(a.name(), raw, "round-trip {raw:?}");
        }
        assert_eq!(
            Action::from_name("inspector:toggle"),
            Some(Action::Inspector(InspectorMode::Toggle))
        );
        // The parameter is required and there is no default: a typo should fail
        // to bind rather than silently pick a mode.
        assert_eq!(Action::from_name("inspector"), None);
        assert_eq!(Action::from_name("inspector:open"), None);
    }

    #[test]
    fn the_search_action_family_parses_and_round_trips() {
        for raw in [
            "start_search",
            "end_search",
            "search_selection",
            "navigate_search:next",
            "navigate_search:previous",
            "search:needle",
            // An empty needle is meaningful: it *stops* the search without
            // hiding the bar, which is what separates it from `end_search`.
            "search:",
        ] {
            let a = Action::from_name(raw).unwrap_or_else(|| panic!("parsing {raw:?}"));
            assert_eq!(a.name(), raw, "round-trip {raw:?}");
        }
        // `prev` is accepted as an alias, like `goto_split:prev`, but normalizes
        // to upstream's spelling.
        assert_eq!(
            Action::from_name("navigate_search:prev"),
            Some(Action::NavigateSearch(false))
        );
        assert_eq!(Action::from_name("navigate_search:sideways"), None);
        // `search` takes a payload upstream, so the bare word is *not* an alias
        // for giest's toggle — accepting it would make a transferred config do
        // something other than what it says.
        assert_eq!(Action::from_name("search"), None);
        assert_eq!(Action::from_name("toggle_search"), Some(Action::ToggleSearch));
    }

    #[test]
    fn the_search_actions_report_when_they_can_act() {
        // These gates are what make `escape=end_search` safe to ship as a
        // default: with no search open the key is the program's.
        let idle = PerformCtx::default();
        let searching = PerformCtx {
            search_active: true,
            ..Default::default()
        };
        let selected = PerformCtx {
            has_selection: true,
            ..Default::default()
        };
        assert!(!can_perform(&Action::EndSearch, idle));
        assert!(can_perform(&Action::EndSearch, searching));
        assert!(!can_perform(&Action::NavigateSearch(true), idle));
        assert!(can_perform(&Action::NavigateSearch(true), searching));
        assert!(!can_perform(&Action::SearchSelection, idle));
        assert!(can_perform(&Action::SearchSelection, selected));
        // `start_search` always acts — it opens the bar.
        assert!(can_perform(&Action::StartSearch, idle));
        // A needle always acts; an *empty* one only has something to stop when
        // a search is already running.
        assert!(can_perform(&Action::Search(Arc::from("x")), idle));
        assert!(!can_perform(&Action::Search(Arc::from("")), idle));
        assert!(can_perform(&Action::Search(Arc::from("")), searching));
    }

    #[test]
    fn every_adjust_selection_direction_parses_and_round_trips() {
        // The default binds construct the action directly, so nothing else here
        // exercises the *config* path — and a typo in the prefix arm would only
        // ever show up as "unknown action" for a user typing
        // `keybind = shift+left=adjust_selection:left`.
        use crate::engine::SelectionAdjust as A;
        const ALL: &[(&str, A)] = &[
            ("left", A::Left),
            ("right", A::Right),
            ("up", A::Up),
            ("down", A::Down),
            ("page_up", A::PageUp),
            ("page_down", A::PageDown),
            ("home", A::Home),
            ("end", A::End),
            ("beginning_of_line", A::BeginningOfLine),
            ("end_of_line", A::EndOfLine),
        ];
        for (name, dir) in ALL {
            let parsed = Action::from_name(&format!("adjust_selection:{name}"));
            assert_eq!(parsed, Some(Action::AdjustSelection(*dir)), "parsing {name}");
            // Round-trips, which is what keeps the two name tables in sync.
            assert_eq!(parsed.unwrap().name(), format!("adjust_selection:{name}"));
        }
        assert_eq!(Action::from_name("adjust_selection:sideways"), None);
        assert_eq!(Action::from_name("adjust_selection:"), None);
    }

    #[test]
    fn equalize_splits_binds_to_a_real_no_op() {
        // It used to parse to `ClearSelection`, so binding a Ghostty config's
        // `equalize_splits` silently *dropped the user's selection*. A no-op
        // action that stands for nothing else is the only safe target, and this
        // pins it because the wrong behaviour is invisible in a config.
        assert_eq!(
            Action::from_name("equalize_splits"),
            Some(Action::Noop(Arc::from("equalize_splits")))
        );
        assert_ne!(
            Action::from_name("equalize_splits"),
            Some(Action::ClearSelection)
        );
        // The no-op carries the name it stands in for, so it round-trips — and
        // so several unimplementable actions can share it without colliding.
        assert_eq!(
            Action::from_name("equalize_splits").unwrap().name(),
            "equalize_splits"
        );

        // `toggle_secure_input` is macOS-only upstream and has no Windows
        // equivalent (no API stops other processes reading keystrokes), so it
        // binds as a no-op rather than logging "unknown action" at a user who
        // transferred a working Ghostty config.
        let secure = Action::from_name("toggle_secure_input").expect("binds");
        assert_eq!(secure, Action::Noop(Arc::from("toggle_secure_input")));
        assert_eq!(secure.name(), "toggle_secure_input");
    }

    #[test]
    fn empty_query_matches_with_neutral_score() {
        assert_eq!(fuzzy_match("", "New Tab"), Some(0));
        assert_eq!(fuzzy_match("   ", "Anything"), Some(0));
    }

    #[test]
    fn matches_subsequence_and_rejects_non_subsequence() {
        assert!(fuzzy_match("nt", "New Tab").is_some()); // initials
        assert!(fuzzy_match("tab", "New Tab").is_some()); // substring
        assert!(fuzzy_match("newtab", "New Tab").is_some()); // across the space
        assert_eq!(fuzzy_match("xyz", "New Tab"), None);
        assert_eq!(fuzzy_match("tt", "New Tab"), None); // only one 't'
    }

    #[test]
    fn contiguous_beats_scattered() {
        // "new" (contiguous prefix) should outscore "nt" (scattered initials).
        assert!(fuzzy_match("new", "New Tab") > fuzzy_match("nt", "New Tab"));
    }

    #[test]
    fn prefix_beats_mid_string() {
        assert!(fuzzy_match("close", "Close Tab") > fuzzy_match("ose", "Close Tab"));
    }

    #[test]
    fn indices_report_matched_positions() {
        let (_, idx) = fuzzy_match_indices("nt", "New Tab").unwrap();
        assert_eq!(idx, vec![0, 4]); // 'N' at 0, 'T' at 4
        let (_, idx) = fuzzy_match_indices("tab", "New Tab").unwrap();
        assert_eq!(idx, vec![4, 5, 6]);
    }

    #[test]
    fn filter_orders_by_score_then_title() {
        let cat = base_catalog();
        let order = filter_commands(&cat, "tab");
        // The best "tab" match is the short, prefix-friendly "New Tab".
        assert_eq!(cat[order[0]].title, "New Tab");
        // Every surviving entry actually contains the subsequence.
        for &i in &order {
            assert!(fuzzy_match("tab", &cat[i].title).is_some());
        }
    }

    #[test]
    fn empty_query_returns_all_in_order() {
        let cat = base_catalog();
        let order = filter_commands(&cat, "");
        assert_eq!(order.len(), cat.len());
        assert!(order.iter().enumerate().all(|(i, &idx)| i == idx));
    }

    #[test]
    fn base_catalog_titles_are_unique_and_nonempty() {
        let cat = base_catalog();
        assert_eq!(cat.len(), BASE_ACTIONS.len());
        let mut titles: Vec<&str> = cat.iter().map(|c| c.title.as_str()).collect();
        assert!(titles.iter().all(|t| !t.is_empty()));
        titles.sort_unstable();
        let unique = titles.len();
        titles.dedup();
        assert_eq!(titles.len(), unique, "duplicate command title");
    }

    #[test]
    fn build_catalog_appends_one_row_per_profile() {
        let names = vec!["PowerShell".to_string(), "Command Prompt".to_string()];
        let cat = build_catalog(&names);
        assert_eq!(cat.len(), base_catalog().len() + 2);
        assert!(cat.iter().any(|c| c.title == "New Tab with PowerShell"
            && c.action == Action::NewTabWithProfile(0)));
        assert!(cat.iter().any(|c| c.title == "New Tab with Command Prompt"
            && c.action == Action::NewTabWithProfile(1)));
    }
}
