//! The command palette's data model: the catalog of invokable [`Action`]s, the
//! fuzzy matcher that filters them, and the open-palette UI [`PaletteState`].
//!
//! This module is deliberately free of egui/app state so the interesting logic
//! (the action catalog and the fuzzy ranking) is pure and unit-testable. The
//! [`App`](crate::app) owns an `Option<PaletteState>`, renders it, and maps a
//! chosen `Action` onto its existing methods via `App::execute_action` —
//! mirroring how Ghostty's palette is a thin layer over its keybind actions.

/// A single thing the palette can do. Every variant maps to an existing
/// `App`/`Session` method in `App::execute_action`; the palette never reaches
/// into app internals itself. `Copy` so a chosen action survives past the UI
/// closure that produced it (the deferred-intent pattern used throughout `app.rs`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Action {
    NewTab,
    /// Open a new tab running the shell profile at this index.
    NewTabWithProfile(usize),
    CloseTab,
    CloseOtherTabs,
    CloseTabsToRight,
    NextTab,
    PrevTab,
    SplitRight,
    SplitDown,
    ClosePane,
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
            Action::NewTab => "New Tab",
            Action::NewTabWithProfile(_) => "New Tab with Shell",
            Action::CloseTab => "Close Tab",
            Action::CloseOtherTabs => "Close Other Tabs",
            Action::CloseTabsToRight => "Close Tabs to the Right",
            Action::NextTab => "Next Tab",
            Action::PrevTab => "Previous Tab",
            Action::SplitRight => "Split Right",
            Action::SplitDown => "Split Down",
            Action::ClosePane => "Close Pane",
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
            Action::OpenConfig => "Open Config",
            Action::ReloadConfig => "Reload Config",
        }
    }

    /// The default keybinding label shown right-aligned in the palette (matching
    /// the bindings in `handle_shortcuts`/`decide_key`), or `None` for actions
    /// reachable only via the palette/menus.
    fn keybind(self) -> Option<&'static str> {
        Some(match self {
            Action::NewTab => "Ctrl+Shift+T",
            Action::NextTab => "Ctrl+Tab",
            Action::PrevTab => "Ctrl+Shift+Tab",
            Action::SplitRight => "Ctrl+Shift+D",
            Action::SplitDown => "Ctrl+Shift+E",
            Action::ClosePane => "Ctrl+Shift+W",
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
            // No default binding.
            Action::NewTabWithProfile(_)
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
}

/// Every catalog action except the per-profile `NewTabWithProfile` rows, which
/// are appended at build time (one per detected shell). Adding an `Action`
/// variant means adding it here too; `Action::title`/`keybind` being exhaustive
/// catches a forgotten title/binding at compile time.
const BASE_ACTIONS: &[Action] = &[
    Action::NewTab,
    Action::CloseTab,
    Action::CloseOtherTabs,
    Action::CloseTabsToRight,
    Action::NextTab,
    Action::PrevTab,
    Action::SplitRight,
    Action::SplitDown,
    Action::ClosePane,
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
