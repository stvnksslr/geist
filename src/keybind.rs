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
//! even for unbound combos, matching giest's long-standing host behavior.

use crate::command::Action;
use crate::engine::{KeyCode, KeyMods};

/// A key combination: the required modifiers plus the (non-modifier) key.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Chord {
    pub mods: KeyMods,
    pub code: KeyCode,
}

/// The app keymap: an ordered list of chord → action bindings. Later entries win
/// (config overrides are appended after the defaults), so lookup scans in
/// reverse.
#[derive(Clone, Debug)]
pub struct Keymap {
    binds: Vec<(Chord, Action)>,
}

impl Default for Keymap {
    fn default() -> Self {
        Self {
            binds: default_binds(),
        }
    }
}

impl Keymap {
    /// The action bound to `chord`, if any (most-recently-set wins).
    pub fn lookup(&self, chord: &Chord) -> Option<Action> {
        self.binds
            .iter()
            .rev()
            .find(|(c, _)| c == chord)
            .map(|(_, a)| *a)
    }

    /// Bind `chord` to `action`, replacing any existing binding for that chord.
    fn set(&mut self, chord: Chord, action: Action) {
        match self.binds.iter_mut().find(|(c, _)| *c == chord) {
            Some(slot) => slot.1 = action,
            None => self.binds.push((chord, action)),
        }
    }

    /// Remove any binding for `chord` (Ghostty's `unbind`).
    fn unset(&mut self, chord: &Chord) {
        self.binds.retain(|(c, _)| c != chord);
    }

    /// Build the keymap from the built-in defaults plus the user's `keybind`
    /// overrides (raw `trigger=action` pairs from the config). Unparseable
    /// triggers / unknown actions are logged and skipped; `unbind`/`ignore`
    /// removes a binding.
    pub fn from_config(overrides: &[(String, String)]) -> Self {
        let mut km = Self::default();
        for (trigger, action) in overrides {
            let Some(chord) = parse_chord(trigger) else {
                eprintln!("giest: ignoring keybind with unparseable trigger '{trigger}'");
                continue;
            };
            let a = action.trim();
            if a.eq_ignore_ascii_case("unbind") || a.eq_ignore_ascii_case("ignore") {
                km.unset(&chord);
                continue;
            }
            match Action::from_name(a) {
                Some(act) => km.set(chord, act),
                None => eprintln!("giest: ignoring keybind to unknown action '{a}'"),
            }
        }
        km
    }
}

/// The built-in default bindings, mirroring the host shortcuts giest has always
/// had. Every trigger here parses and sits inside an app-reserved namespace.
fn default_binds() -> Vec<(Chord, Action)> {
    const DEFAULTS: &[(&str, Action)] = &[
        ("ctrl+shift+t", Action::NewTab),
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
    ];
    DEFAULTS
        .iter()
        .filter_map(|(t, a)| parse_chord(t).map(|c| (c, *a)))
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

#[cfg(test)]
mod tests {
    use super::*;

    fn chord(s: &str) -> Chord {
        parse_chord(s).unwrap_or_else(|| panic!("chord {s:?} should parse"))
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
        assert_eq!(chord("CMD+k").mods.sup, true);
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
        assert_eq!(km.lookup(&chord("ctrl+alt+left")), Some(Action::FocusSplitLeft));
        assert_eq!(km.lookup(&chord("alt+1")), Some(Action::GotoTab(0)));
        assert_eq!(km.lookup(&chord("alt+9")), Some(Action::LastTab));
        assert_eq!(km.lookup(&chord("ctrl+tab")), Some(Action::NextTab));
        // An unbound chord resolves to nothing.
        assert_eq!(km.lookup(&chord("ctrl+shift+z")), None);
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
        assert_eq!(km.lookup(&chord("ctrl+shift+r")), Some(Action::ReloadConfig));
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
        ] {
            assert_eq!(Action::from_name(&a.name()), Some(a), "roundtrip {a:?}");
        }
    }

    #[test]
    fn default_keymap_binds_fullscreen_and_zoom() {
        let km = Keymap::default();
        assert_eq!(km.lookup(&chord("ctrl+enter")), Some(Action::ToggleFullscreen));
        assert_eq!(
            km.lookup(&chord("ctrl+shift+enter")),
            Some(Action::ToggleSplitZoom)
        );
    }
}
