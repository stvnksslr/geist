//! `key-remap`: application-level modifier remapping (Ghostty 1.3+).
//!
//! Applied to egui's input **once per pass, before anything reads it**
//! (`Window::run_pass`), so keybind lookup, `decide_key`, the key encoder and
//! mouse reporting all see the same remapped modifiers — upstream's "this
//! affects both keybind matching and terminal input encoding".
//!
//! Semantics mirror upstream exactly where Windows allows:
//! - one-way (`ctrl=super` makes Ctrl act as Super; Super stays Super);
//! - **not transitive** (`ctrl=super` + `alt=ctrl`: Alt produces Ctrl, not
//!   Super) — every mapping reads the *physical* state, never another
//!   mapping's output;
//! - a later line for the same source wins.
//!
//! **Divergence:** egui reports modifiers without a side, so a sided name
//! (`left_ctrl`) is accepted but acts on both sides. And egui-winit never
//! reports the Windows key as a modifier, so `super` as a *source* never
//! matches; as a *target* it sets `mac_cmd`, which [`crate::session::key_mods`]
//! reads as `super` for bindings and the encoder.

use eframe::egui;

/// A remappable modifier.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Mod {
    Ctrl,
    Alt,
    Shift,
    Super,
}

/// One `from=to` line.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Remap {
    pub from: Mod,
    pub to: Mod,
}

/// Parse a modifier name, generic or sided (the side is accepted and dropped —
/// see the module docs).
fn parse_mod(s: &str) -> Option<Mod> {
    let s = s.trim().to_ascii_lowercase();
    let base = s
        .strip_prefix("left_")
        .or_else(|| s.strip_prefix("right_"))
        .unwrap_or(&s);
    Some(match base {
        "ctrl" | "control" => Mod::Ctrl,
        "alt" | "opt" | "option" => Mod::Alt,
        "shift" => Mod::Shift,
        "super" | "cmd" | "command" => Mod::Super,
        _ => return None,
    })
}

/// Parse one `key-remap` value (`from=to`). `None` for anything malformed.
pub fn parse(v: &str) -> Option<Remap> {
    let (from, to) = v.split_once('=')?;
    Some(Remap {
        from: parse_mod(from)?,
        to: parse_mod(to)?,
    })
}

/// Remap `m` through `set`. Pure, so the non-transitivity rule is testable.
pub fn remap(set: &[Remap], m: egui::Modifiers) -> egui::Modifiers {
    if set.is_empty() {
        return m;
    }
    let held = |md: Mod| match md {
        Mod::Ctrl => m.ctrl,
        Mod::Alt => m.alt,
        Mod::Shift => m.shift,
        Mod::Super => m.mac_cmd,
    };
    let target = |md: Mod| set.iter().rev().find(|r| r.from == md).map_or(md, |r| r.to);
    let mut out = egui::Modifiers::NONE;
    for md in [Mod::Ctrl, Mod::Alt, Mod::Shift, Mod::Super] {
        if !held(md) {
            continue;
        }
        match target(md) {
            Mod::Ctrl => out.ctrl = true,
            Mod::Alt => out.alt = true,
            Mod::Shift => out.shift = true,
            Mod::Super => out.mac_cmd = true,
        }
    }
    // Windows semantics: `command` is Ctrl.
    out.command = out.ctrl;
    out
}

/// Rewrite this pass's input in place: the aggregate modifier state and every
/// event that carries its own copy.
pub fn apply(set: &[Remap], input: &mut egui::InputState) {
    if set.is_empty() {
        return;
    }
    input.modifiers = remap(set, input.modifiers);
    for e in &mut input.events {
        match e {
            egui::Event::Key { modifiers, .. }
            | egui::Event::PointerButton { modifiers, .. }
            | egui::Event::MouseWheel { modifiers, .. } => *modifiers = remap(set, *modifiers),
            _ => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn m(ctrl: bool, alt: bool, shift: bool) -> egui::Modifiers {
        egui::Modifiers {
            ctrl,
            alt,
            shift,
            command: ctrl,
            mac_cmd: false,
        }
    }

    #[test]
    fn parses_generic_and_sided_names() {
        assert_eq!(
            parse("ctrl=alt"),
            Some(Remap {
                from: Mod::Ctrl,
                to: Mod::Alt
            })
        );
        assert_eq!(
            parse("left_control=right_alt"),
            Some(Remap {
                from: Mod::Ctrl,
                to: Mod::Alt
            })
        );
        assert_eq!(
            parse("cmd=shift"),
            Some(Remap {
                from: Mod::Super,
                to: Mod::Shift
            })
        );
        assert_eq!(parse("ctrl"), None);
        assert_eq!(parse("ctrl=hyper"), None);
    }

    #[test]
    fn swap_is_one_way_and_not_transitive() {
        let set = [parse("ctrl=super").unwrap(), parse("alt=ctrl").unwrap()];
        // Alt produces Ctrl, not Super.
        let out = remap(&set, m(false, true, false));
        assert!(out.ctrl && out.command && !out.alt && !out.mac_cmd);
        // Ctrl produces Super.
        let out = remap(&set, m(true, false, false));
        assert!(out.mac_cmd && !out.ctrl && !out.command);
        // Shift is untouched.
        assert_eq!(remap(&set, m(false, false, true)), m(false, false, true));
    }

    #[test]
    fn later_line_wins_and_empty_set_is_identity() {
        let set = [parse("ctrl=alt").unwrap(), parse("ctrl=shift").unwrap()];
        assert_eq!(remap(&set, m(true, false, false)), m(false, false, true));
        assert_eq!(remap(&[], m(true, true, false)), m(true, true, false));
    }
}
