//! The profiles page's **working copy** — everything the dialog does to the
//! profile list, minus the drawing (which lives in `app.rs` like every other
//! modal). Kept here so the rules that are easy to get subtly wrong (what a
//! reorder does to the default index, what an empty program means, what a
//! reset restores) are unit-testable without an egui context.
//!
//! The page edits a copy and commits on Save: a half-finished custom profile
//! must not reach the new-tab menu, and Cancel has to be free.

use std::path::PathBuf;

use crate::profiles::Profile;
use crate::profilestore::Store;

/// One row of the page: a profile plus the text buffers the editor types into.
/// The buffers are the source of truth while the page is open — a program box
/// the user has cleared is a legal intermediate state, and forcing it back
/// through `Profile` every keystroke would fight the cursor.
#[derive(Clone, Debug)]
pub struct Row {
    pub profile: Profile,
    /// Arguments, **one per line**. A line may itself contain spaces, which is
    /// the whole reason this isn't one space-separated box: `-c "a b"` has to
    /// be expressible.
    pub args: String,
    pub cwd: String,
}

impl Row {
    fn new(profile: Profile) -> Self {
        Self {
            args: profile.args.join("\n"),
            cwd: profile
                .cwd
                .as_ref()
                .map(|p| p.display().to_string())
                .unwrap_or_default(),
            profile,
        }
    }

    /// The profile this row describes, with the text buffers folded back in.
    /// Blank lines are dropped rather than passed on as empty arguments — an
    /// empty argv entry is a real thing to pass a program, but never what a
    /// trailing newline in a text box meant.
    fn commit(&self) -> Profile {
        let mut p = self.profile.clone();
        if p.custom {
            p.args = self
                .args
                .lines()
                .map(str::trim)
                .filter(|a| !a.is_empty())
                .map(str::to_string)
                .collect();
            let cwd = self.cwd.trim();
            p.cwd = (!cwd.is_empty()).then(|| PathBuf::from(cwd));
        }
        p
    }

    /// Why this row can't be saved, if it can't. A detected shell is always
    /// valid — only the user-typed fields can be wrong.
    pub fn problem(&self) -> Option<&'static str> {
        if self.profile.name.trim().is_empty() {
            return Some("needs a name");
        }
        if self.profile.custom && self.profile.program.trim().is_empty() {
            return Some("needs a program");
        }
        None
    }
}

/// The page's state: the rows, which one is default, and the unedited detected
/// list to diff against (`Store::capture`) and to reset to.
#[derive(Clone, Debug)]
pub struct Page {
    pub rows: Vec<Row>,
    pub default: usize,
    /// The row whose details are expanded, by key — not by index, since a
    /// reorder or a delete moves every index after it.
    pub expanded: Option<String>,
    detected: Vec<Profile>,
    detected_default: usize,
}

impl Page {
    /// Open the page over the list the window is running (`live`/`default`),
    /// re-deriving the unedited detected list from `config_shell` so Save can
    /// record just the deltas.
    pub fn open(live: &[Profile], default: usize, config_shell: Option<&str>) -> Self {
        let (detected, detected_default) = crate::profiles::detect(config_shell);
        Self {
            rows: live.iter().cloned().map(Row::new).collect(),
            default,
            expanded: None,
            detected,
            detected_default,
        }
    }

    /// Swap row `i` with the one `delta` slots away, carrying the default with
    /// it. `delta` is ±1 — the page's ▲/▼ buttons — so a swap *is* a move; a
    /// larger step would be a swap, not the rotation it looks like. The default
    /// is stored as an *index*, so every reorder has to move it too or the
    /// default silently becomes whichever profile slid into that slot — the
    /// same stale-index trap tab drags have.
    pub fn reorder(&mut self, i: usize, delta: isize) {
        let Some(j) = i.checked_add_signed(delta) else {
            return;
        };
        if i >= self.rows.len() || j >= self.rows.len() {
            return;
        }
        self.rows.swap(i, j);
        if self.default == i {
            self.default = j;
        } else if self.default == j {
            self.default = i;
        }
    }

    /// Hide or show row `i`. Hiding the default is refused rather than silently
    /// moving the default elsewhere: it would leave the user's chosen shell
    /// both default and unreachable, and the page can say so instead.
    pub fn set_hidden(&mut self, i: usize, hidden: bool) {
        if hidden && i == self.default {
            return;
        }
        if let Some(r) = self.rows.get_mut(i) {
            r.profile.hidden = hidden;
        }
    }

    /// Make row `i` the default, un-hiding it — the default is always
    /// reachable from the new-tab menu (see [`Store::apply`]).
    pub fn set_default(&mut self, i: usize) {
        if i < self.rows.len() {
            self.default = i;
            self.rows[i].profile.hidden = false;
        }
    }

    /// Append a blank user-added profile and expand it. It is invalid until a
    /// program is typed (`Row::problem`), which is what stops Save writing an
    /// entry that would silently fail to launch.
    pub fn add_custom(&mut self) {
        let live: Vec<Profile> = self.rows.iter().map(|r| r.profile.clone()).collect();
        let key = Store::next_custom_key(&live);
        let row = Row::new(Profile::custom(&key, "New profile", "", Vec::new(), None));
        self.expanded = Some(key);
        self.rows.push(row);
    }

    /// Remove a user-added profile. Detected shells are hidden, never removed —
    /// they'd come straight back on the next launch, since detection, not the
    /// store, decides what exists.
    pub fn remove(&mut self, i: usize) {
        if !self.rows.get(i).is_some_and(|r| r.profile.custom) {
            return;
        }
        self.rows.remove(i);
        // The default is an index into a list that just got shorter.
        if self.default > i {
            self.default -= 1;
        } else if self.default == i {
            self.default = 0;
        }
        self.default = self.default.min(self.rows.len().saturating_sub(1));
    }

    /// Throw every edit away and go back to plain detection.
    pub fn reset(&mut self) {
        self.rows = self.detected.iter().cloned().map(Row::new).collect();
        self.default = self.detected_default.min(self.rows.len().saturating_sub(1));
        self.expanded = None;
    }

    /// The first row that can't be saved, as `(index, reason)`.
    pub fn problem(&self) -> Option<(usize, &'static str)> {
        self.rows
            .iter()
            .enumerate()
            .find_map(|(i, r)| r.problem().map(|p| (i, p)))
    }

    /// Fold the buffers back in and produce the list to run plus the deltas to
    /// write. `None` while any row is invalid, so Save is one call that either
    /// yields a consistent pair or refuses.
    pub fn commit(&self) -> Option<(Vec<Profile>, usize, Store)> {
        if self.problem().is_some() {
            return None;
        }
        let list: Vec<Profile> = self.rows.iter().map(Row::commit).collect();
        if list.is_empty() {
            return None;
        }
        let default = self.default.min(list.len() - 1);
        let store = Store::capture(&list, default, &self.detected);
        Some((list, default, store))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn page() -> Page {
        let detected = vec![
            Profile::new("PowerShell", "pwsh.exe"),
            Profile::new("Windows PowerShell", "powershell.exe"),
            Profile::new("Command Prompt", "cmd.exe"),
        ];
        Page {
            rows: detected.iter().cloned().map(Row::new).collect(),
            default: 0,
            expanded: None,
            detected,
            detected_default: 0,
        }
    }

    #[test]
    fn reorder_carries_the_default_with_the_row() {
        let mut p = page();
        p.set_default(2);
        p.reorder(2, -1);
        assert_eq!(p.default, 1);
        assert_eq!(p.rows[1].profile.key, "cmd");
    }

    #[test]
    fn reorder_moves_the_default_aside_when_another_row_takes_its_slot() {
        let mut p = page();
        p.set_default(0);
        p.reorder(1, -1);
        assert_eq!(p.default, 1);
        assert_eq!(p.rows[1].profile.key, "pwsh");
    }

    #[test]
    fn reorder_off_either_end_does_nothing() {
        let mut p = page();
        p.reorder(0, -1);
        p.reorder(2, 1);
        assert_eq!(
            p.rows
                .iter()
                .map(|r| r.profile.key.as_str())
                .collect::<Vec<_>>(),
            ["pwsh", "powershell", "cmd"]
        );
    }

    #[test]
    fn the_default_cannot_be_hidden() {
        let mut p = page();
        p.set_hidden(0, true);
        assert!(!p.rows[0].profile.hidden);
    }

    #[test]
    fn making_a_hidden_profile_the_default_un_hides_it() {
        let mut p = page();
        p.set_hidden(2, true);
        p.set_default(2);
        assert!(!p.rows[2].profile.hidden);
        assert_eq!(p.default, 2);
    }

    #[test]
    fn a_new_custom_profile_blocks_save_until_it_has_a_program() {
        let mut p = page();
        p.add_custom();
        assert_eq!(p.problem().map(|(i, _)| i), Some(3));
        assert!(p.commit().is_none());
        p.rows[3].profile.program = r"C:\Git\bin\bash.exe".into();
        assert!(p.commit().is_some());
    }

    #[test]
    fn a_blank_name_blocks_save_too() {
        let mut p = page();
        p.rows[1].profile.name = "  ".into();
        assert_eq!(p.problem(), Some((1, "needs a name")));
    }

    #[test]
    fn args_are_one_per_line_and_blank_lines_are_dropped() {
        let mut p = page();
        p.add_custom();
        p.rows[3].profile.program = "bash.exe".into();
        p.rows[3].args = "--login\n\n-c echo hi\n".into();
        let (list, _, _) = p.commit().unwrap();
        assert_eq!(
            list[3].args,
            vec!["--login".to_string(), "-c echo hi".into()]
        );
    }

    #[test]
    fn a_detected_rows_args_box_is_ignored_even_if_set() {
        let mut p = page();
        p.rows[0].args = "--nope".into();
        let (list, _, _) = p.commit().unwrap();
        assert!(list[0].args.is_empty());
    }

    #[test]
    fn a_blank_cwd_commits_as_none() {
        let mut p = page();
        p.add_custom();
        p.rows[3].profile.program = "bash.exe".into();
        p.rows[3].cwd = "   ".into();
        let (list, _, _) = p.commit().unwrap();
        assert!(list[3].cwd.is_none());
    }

    #[test]
    fn only_custom_profiles_can_be_removed() {
        let mut p = page();
        p.remove(0);
        assert_eq!(p.rows.len(), 3);
        p.add_custom();
        p.remove(3);
        assert_eq!(p.rows.len(), 3);
    }

    #[test]
    fn removing_a_row_before_the_default_keeps_the_same_profile_default() {
        let mut p = page();
        p.add_custom();
        p.rows[3].profile.program = "bash.exe".into();
        // Walk the custom row to the front one step at a time, the way the
        // page's ▲ button does.
        for i in (1..4).rev() {
            p.reorder(i, -1);
        }
        assert_eq!(
            p.rows
                .iter()
                .map(|r| r.profile.key.as_str())
                .collect::<Vec<_>>(),
            ["custom:1", "pwsh", "powershell", "cmd"]
        );
        p.set_default(3);
        p.remove(0);
        assert_eq!(p.rows[p.default].profile.key, "cmd");
    }

    #[test]
    fn removing_the_default_falls_back_to_the_first_row() {
        let mut p = page();
        p.add_custom();
        p.rows[3].profile.program = "bash.exe".into();
        p.set_default(3);
        p.remove(3);
        assert_eq!(p.default, 0);
    }

    #[test]
    fn reset_restores_plain_detection() {
        let mut p = page();
        p.add_custom();
        p.set_hidden(2, true);
        p.rows[1].profile.name = "renamed".into();
        p.reset();
        assert_eq!(p.rows.len(), 3);
        assert_eq!(p.rows[1].profile.name, "Windows PowerShell");
        assert!(p.rows.iter().all(|r| !r.profile.hidden));
        let (_, _, store) = p.commit().unwrap();
        assert!(store.hidden.is_empty() && store.names.is_empty() && store.custom.is_empty());
    }

    #[test]
    fn commit_records_the_edits_the_store_will_replay() {
        let mut p = page();
        p.rows[0].profile.name = "PS7".into();
        p.set_hidden(1, true);
        p.set_default(2);
        let (list, default, store) = p.commit().unwrap();
        assert_eq!(store.names, vec![("pwsh".into(), "PS7".into())]);
        assert_eq!(store.hidden, vec!["powershell".to_string()]);
        assert_eq!(store.default.as_deref(), Some("cmd"));
        // …and replaying them reproduces exactly what the page is showing.
        let (again, again_default) = store.apply(
            vec![
                Profile::new("PowerShell", "pwsh.exe"),
                Profile::new("Windows PowerShell", "powershell.exe"),
                Profile::new("Command Prompt", "cmd.exe"),
            ],
            0,
        );
        assert_eq!(again_default, default);
        assert_eq!(
            again
                .iter()
                .map(|p| (p.name.clone(), p.hidden))
                .collect::<Vec<_>>(),
            list.iter()
                .map(|p| (p.name.clone(), p.hidden))
                .collect::<Vec<_>>()
        );
    }
}
