//! IME composition (preedit) state, kept free of egui's window plumbing so the
//! routing rules can be table-tested.
//!
//! Three rules live here, each one a silent failure if it drifts:
//! - **While a preedit is showing, keys belong to the IME.** Enter commits the
//!   composition and Backspace edits it; letting either reach the shell as well
//!   would submit the line or eat a character the user can't see.
//!   egui-winit already drops Windows' `VK_PROCESSKEY` presses, so this is the
//!   backstop for anything that slips past that filter.
//! - **A commit is typed exactly once.** winit on Windows can report the same
//!   composed text as both `Ime::Commit` and an ordinary `Text`, in either
//!   order within one frame. [`FrameDedupe`] drops the second of the pair.
//! - **Disabled / an empty preedit clears it**, so a cancelled composition
//!   (Esc, focus loss) doesn't leave stale text drawn over the grid.

/// The composition currently shown at the cursor. Per-pane (it lives on the
/// `Session`), so switching panes mid-composition can't draw one pane's
/// preedit over another's cursor.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct Preedit {
    text: String,
}

impl Preedit {
    /// Apply one IME event. Returns the text to type into the shell, if any.
    pub fn apply(&mut self, ev: &eframe::egui::ImeEvent) -> Option<String> {
        match ev {
            eframe::egui::ImeEvent::Preedit(t) => {
                self.text.clone_from(t);
                None
            }
            eframe::egui::ImeEvent::Commit(t) => {
                self.text.clear();
                (!t.is_empty()).then(|| t.clone())
            }
            eframe::egui::ImeEvent::Enabled => None,
            eframe::egui::ImeEvent::Disabled => {
                self.text.clear();
                None
            }
        }
    }

    /// Whether a composition is in progress — keys then belong to the IME.
    pub fn active(&self) -> bool {
        !self.text.is_empty()
    }

    pub fn text(&self) -> &str {
        &self.text
    }

    pub fn clear(&mut self) {
        self.text.clear();
    }
}

/// One frame's memory of what was typed as `Text` and what was committed, so
/// the duplicate half of a `Commit`/`Text` pair is dropped whichever arrives
/// first. Scoped to a single frame's event list, like `suppress_text`.
#[derive(Debug, Default)]
pub struct FrameDedupe {
    typed: Vec<String>,
    committed: Vec<String>,
}

impl FrameDedupe {
    /// A `Commit` arrived: `true` if it should be typed (no matching `Text`
    /// already went out this frame).
    pub fn commit(&mut self, text: &str) -> bool {
        if take(&mut self.typed, text) {
            return false;
        }
        self.committed.push(text.to_owned());
        true
    }

    /// A `Text` arrived: `true` if it should be typed (it isn't the echo of a
    /// commit already sent this frame).
    pub fn text(&mut self, text: &str) -> bool {
        if take(&mut self.committed, text) {
            return false;
        }
        self.typed.push(text.to_owned());
        true
    }
}

/// Remove one occurrence of `text`, reporting whether there was one — each
/// event can cancel at most one partner, so typing `aa` quickly still types two.
fn take(v: &mut Vec<String>, text: &str) -> bool {
    match v.iter().position(|s| s == text) {
        Some(i) => {
            v.remove(i);
            true
        }
        None => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use eframe::egui::ImeEvent as E;

    #[test]
    fn preedit_then_commit_types_once_and_clears() {
        let mut p = Preedit::default();
        assert_eq!(p.apply(&E::Enabled), None);
        assert_eq!(p.apply(&E::Preedit("ni".into())), None);
        assert!(p.active());
        assert_eq!(p.text(), "ni");
        assert_eq!(p.apply(&E::Commit("你".into())), Some("你".into()));
        assert!(!p.active());
    }

    #[test]
    fn cancel_paths_clear_without_typing() {
        let mut p = Preedit::default();
        p.apply(&E::Preedit("ka".into()));
        assert_eq!(p.apply(&E::Preedit(String::new())), None);
        assert!(!p.active());
        p.apply(&E::Preedit("ka".into()));
        assert_eq!(p.apply(&E::Disabled), None);
        assert!(!p.active());
        assert_eq!(p.apply(&E::Commit(String::new())), None);
    }

    #[test]
    fn commit_then_text_echo_is_dropped() {
        let mut d = FrameDedupe::default();
        assert!(d.commit("你好"));
        assert!(!d.text("你好"));
        // The pair is spent: a later identical keystroke is real input.
        assert!(d.text("你好"));
    }

    #[test]
    fn text_then_commit_echo_is_dropped() {
        let mut d = FrameDedupe::default();
        assert!(d.text("é"));
        assert!(!d.commit("é"));
    }

    #[test]
    fn unrelated_text_is_not_swallowed() {
        let mut d = FrameDedupe::default();
        assert!(d.commit("日本"));
        assert!(d.text("a"));
        assert!(d.text("a"));
    }
}
