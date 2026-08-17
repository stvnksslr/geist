//! The command palette's data model: the catalog of invokable [`Action`]s, the
//! fuzzy matcher that filters them, and the open-palette UI [`PaletteState`].
//!
//! This module is deliberately free of egui/app state so the interesting logic
//! (the action catalog and the fuzzy ranking) is pure and unit-testable. The
//! [`App`](crate::app) owns an `Option<PaletteState>`, renders it, and maps a
//! chosen `Action` onto its existing methods via `App::execute_action` —
//! mirroring how Ghostty's palette is a thin layer over its keybind actions.

/// The live state a *performable* binding is judged against.
///
/// Deliberately a plain value rather than a borrow of the session: it is read by
/// the gate that decides whether the shell sees a key and by the gate that runs
/// the action, and those sit on opposite sides of the app.
#[derive(Clone, Copy, Debug, Default)]
pub struct PerformCtx {
    pub has_selection: bool,
}

/// Whether `action` can do anything right now.
///
/// Only consulted for bindings flagged `performable:`; everything else runs
/// regardless. **One function for both gates** — a second opinion here means the
/// key is either swallowed and does nothing, or reaches the shell *and* runs the
/// action.
pub fn can_perform(action: Action, ctx: PerformCtx) -> bool {
    match action {
        // Upstream returns "not performed" with no selection, letting the key
        // fall through to the terminal (`Surface.zig`'s `.adjust_selection`).
        Action::AdjustSelection(_) => ctx.has_selection,
        _ => true,
    }
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
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Action {
    /// Bind-able and does nothing, for a Ghostty action giest has no work to do
    /// for. Rejecting the name would log an "unknown action" the user cannot act
    /// on; mapping it onto some *other* action would silently do the wrong
    /// thing, which is exactly what `equalize_splits` used to do.
    Noop,
    /// Move the selection's free end (Ghostty `adjust_selection:<direction>`).
    /// Bound to shift+arrows and **performable**: with no selection the key is
    /// the shell's.
    AdjustSelection(crate::engine::SelectionAdjust),
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
    fn title(self) -> &'static str {
        match self {
            // Never listed in the palette (see `CATALOG`), but `title` must be
            // total.
            Action::Noop => "Do Nothing",
            Action::AdjustSelection(_) => "Adjust Selection",
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
            Action::Quit => "Quit",
            Action::PromptTabTitle => "Rename Tab",
            Action::ToggleMaximize => "Toggle Maximize",
            Action::ToggleFloatOnTop => "Toggle Always on Top",
            Action::ToggleBackgroundOpacity => "Toggle Background Opacity",
            Action::ToggleMouseReporting => "Toggle Mouse Reporting",
            Action::ToggleSearch => "Search Scrollback",
            Action::JumpToPrompt(d) => {
                if d < 0 {
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
    fn keybind(self) -> Option<&'static str> {
        Some(match self {
            Action::Noop | Action::AdjustSelection(_) => return None,
            Action::NewTab => "Ctrl+Shift+T",
            Action::NewWindow => "Ctrl+Shift+N",
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
            Action::JumpToPrompt(d) if d < 0 => "Ctrl+Shift+\u{2191}",
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
    pub fn name(self) -> String {
        match self {
            // Round-trips as the Ghostty name it stands in for.
            Action::Noop => "equalize_splits".into(),
            Action::AdjustSelection(d) => format!("adjust_selection:{}", adjust_name(d)),
            Action::NewTab => "new_tab".into(),
            Action::NewTabWithProfile(i) => format!("new_tab_with_profile:{i}"),
            Action::NewWindow => "new_window".into(),
            Action::CloseWindow => "close_window".into(),
            Action::CloseTab => "close_tab".into(),
            Action::CloseOtherTabs => "close_other_tabs".into(),
            Action::CloseTabsToRight => "close_tabs_to_right".into(),
            Action::NextTab => "next_tab".into(),
            Action::PrevTab => "previous_tab".into(),
            Action::GotoTab(i) => format!("goto_tab:{}", i as u16 + 1),
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
                format!("scroll_page_fractional:{}", f32::from(n) / 100.0)
            }
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
            "quit" | "close_all_windows" => Action::Quit,
            "prompt_tab_title" => Action::PromptTabTitle,
            // giest splits are always 50/50, so there is nothing to equalize.
            // Accepted as a no-op so a Ghostty config binds without an error
            // rather than logging an "unknown action" the user cannot act on.
            "equalize_splits" => Action::Noop,
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
            "toggle_search" | "search" => Action::ToggleSearch,
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
    BASE_ACTIONS.iter().copied().map(Command::from_action).collect()
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
        assert_eq!(Action::from_name("equalize_splits"), Some(Action::Noop));
        assert_ne!(
            Action::from_name("equalize_splits"),
            Some(Action::ClearSelection)
        );
        // …and it round-trips as the Ghostty name it stands in for.
        assert_eq!(Action::Noop.name(), "equalize_splits");
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
