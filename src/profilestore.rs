//! Persisted user edits to the shell profiles — what the profiles page writes.
//!
//! [`profiles::detect`](crate::profiles::detect) is the source of truth for
//! *what is installed*; this file records only the user's **deltas** on top of
//! it: which profiles are hidden from the new-tab menu, what they are called,
//! what order they appear in, which one is the default, and any profiles the
//! user added by hand.
//!
//! It is a separate file (`%APPDATA%\geist\profiles`, override with
//! `$geist_PROFILES`) rather than keys in the config deliberately: the config
//! is hand-maintained, and a UI that rewrites it would have to preserve the
//! user's comments, ordering and formatting to be non-destructive. Storing the
//! deltas beside `state` means the profiles page never touches it.
//!
//! The format is the same line-oriented text as [`crate::state`], for the same
//! two reasons: no serialization dependency, and a file a human can repair by
//! deleting a bad line. Every record is one line, `<tag> <fields…>`, with any
//! free-form field taking the **rest of the line** so nothing needs escaping:
//!
//! ```text
//! geist-profiles 1
//! D pwsh                  default profile, by key
//! X cmd                   hidden from the new-tab menu
//! R pwsh PowerShell 7     renamed: key, then the display name
//! C custom:1 Git Bash     a user-added profile: key, then its display name
//! P C:\Git\bin\bash.exe   …its program  (applies to the C above)
//! A --login               …one argument (repeatable, in order)
//! W C:\src                …its starting directory
//! O pwsh custom:1 cmd     menu order, by key; anything unlisted keeps its
//!                         detected position at the end
//! ```
//!
//! Keys never contain a space, which is what lets `O` be one line. Parsing is
//! total: a malformed line drops that record rather than failing the load, and
//! a key naming a profile that is no longer installed is simply ignored — so
//! uninstalling a shell degrades to "the entry disappears", not an error.

use std::path::PathBuf;

use crate::profiles::Profile;

/// Header of a profiles file. Bumped if the grammar ever changes incompatibly;
/// a file with any other version is ignored rather than guessed at.
const HEADER: &str = "geist-profiles 1";

/// A profile the user added by hand, which detection knows nothing about.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Custom {
    pub key: String,
    pub name: String,
    pub program: String,
    pub args: Vec<String>,
    pub cwd: Option<String>,
}

/// The user's deltas over the detected profile list.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Store {
    /// The default profile for new windows and tabs, by key.
    pub default: Option<String>,
    pub hidden: Vec<String>,
    /// `(key, display name)` for profiles the user renamed.
    pub names: Vec<(String, String)>,
    pub custom: Vec<Custom>,
    /// Menu order, by key. Keys not listed keep their detected order, after
    /// the ones that are.
    pub order: Vec<String>,
}

impl Store {
    /// Capture the deltas implied by a live profile list (what the profiles
    /// page saves). `detected` is the unedited list from `profiles::detect`,
    /// so a name that matches detection is *not* recorded as a rename — the
    /// file stays a set of real edits rather than a full copy of the list.
    pub fn capture(live: &[Profile], default: usize, detected: &[Profile]) -> Self {
        let detected_name = |key: &str| {
            detected
                .iter()
                .find(|p| p.key == key)
                .map(|p| p.name.as_str())
        };
        Self {
            default: live.get(default).map(|p| p.key.clone()),
            hidden: live
                .iter()
                .filter(|p| p.hidden)
                .map(|p| p.key.clone())
                .collect(),
            names: live
                .iter()
                .filter(|p| !p.custom && detected_name(&p.key) != Some(p.name.as_str()))
                .map(|p| (p.key.clone(), p.name.clone()))
                .collect(),
            custom: live
                .iter()
                .filter(|p| p.custom)
                .map(|p| Custom {
                    key: p.key.clone(),
                    name: p.name.clone(),
                    program: p.program.clone(),
                    args: p.args.clone(),
                    cwd: p.cwd.as_ref().map(|c| c.to_string_lossy().into_owned()),
                })
                .collect(),
            order: live.iter().map(|p| p.key.clone()).collect(),
        }
    }

    /// Lay the deltas over a freshly detected list: append the custom
    /// profiles, rename, hide, reorder, and resolve the default. `default` is
    /// detection's own pick (from `shell =`), used when the file names no
    /// default or names one that is no longer installed.
    ///
    /// The resolved default is always **visible**: a hidden default is a
    /// terminal with no reachable shell, so hiding wins nothing and costs the
    /// user their way back.
    pub fn apply(&self, mut list: Vec<Profile>, default: usize) -> (Vec<Profile>, usize) {
        for c in &self.custom {
            // A custom key colliding with a detected one would make the two
            // indistinguishable to every lookup below; drop it instead.
            if list.iter().any(|p| p.key == c.key) {
                continue;
            }
            list.push(Profile::custom(
                &c.key,
                &c.name,
                &c.program,
                c.args.clone(),
                c.cwd.as_deref().map(PathBuf::from),
            ));
        }
        for (key, name) in &self.names {
            if let Some(p) = list.iter_mut().find(|p| &p.key == key) {
                p.name = name.clone();
            }
        }
        for key in &self.hidden {
            if let Some(p) = list.iter_mut().find(|p| &p.key == key) {
                p.hidden = true;
            }
        }

        // Reorder: the keys named in `O`, in that order, then everything else
        // in its detected order. Remember the detected default by *key* first,
        // since its index is about to move.
        let fallback = list.get(default).map(|p| p.key.clone());
        let mut ordered: Vec<Profile> = Vec::with_capacity(list.len());
        for key in &self.order {
            if let Some(i) = list.iter().position(|p| &p.key == key) {
                ordered.push(list.remove(i));
            }
        }
        ordered.append(&mut list);

        let key = self
            .default
            .as_ref()
            .filter(|k| ordered.iter().any(|p| &&p.key == k))
            .cloned()
            .or(fallback);
        let default = key
            .and_then(|k| ordered.iter().position(|p| p.key == k))
            .unwrap_or(0);
        if let Some(p) = ordered.get_mut(default) {
            p.hidden = false;
        }
        (ordered, default)
    }

    /// A key no existing profile uses, for a newly added custom profile.
    pub fn next_custom_key(list: &[Profile]) -> String {
        (1..)
            .map(|n| format!("custom:{n}"))
            .find(|k| !list.iter().any(|p| p.key == *k))
            .expect("an unbounded range always yields a free key")
    }
}

pub fn serialize(store: &Store) -> String {
    let mut out = String::from(HEADER);
    out.push('\n');
    if let Some(d) = &store.default {
        out.push_str(&format!("D {d}\n"));
    }
    for key in &store.hidden {
        out.push_str(&format!("X {key}\n"));
    }
    for (key, name) in &store.names {
        out.push_str(&format!("R {key} {name}\n"));
    }
    for c in &store.custom {
        out.push_str(&format!("C {} {}\n", c.key, c.name));
        out.push_str(&format!("P {}\n", c.program));
        for a in &c.args {
            out.push_str(&format!("A {a}\n"));
        }
        if let Some(w) = &c.cwd {
            out.push_str(&format!("W {w}\n"));
        }
    }
    if !store.order.is_empty() {
        out.push_str(&format!("O {}\n", store.order.join(" ")));
    }
    out
}

pub fn parse(text: &str) -> Store {
    let mut store = Store::default();
    let mut lines = text.lines();
    if lines.next().map(str::trim) != Some(HEADER) {
        return store;
    }
    // `P`/`A`/`W` continue the `C` above them; a stray one (a hand-edited file
    // with the `C` deleted) has nothing to attach to and is dropped.
    for line in lines {
        let line = line.trim_end_matches('\r');
        let (tag, rest) = match line.split_once(' ') {
            Some((t, r)) => (t, r.trim_start()),
            None => continue,
        };
        if rest.is_empty() {
            continue;
        }
        match tag {
            "D" => store.default = Some(rest.to_string()),
            "X" => store.hidden.push(rest.to_string()),
            "R" => {
                if let Some((key, name)) = rest.split_once(' ') {
                    store
                        .names
                        .push((key.to_string(), name.trim_start().to_string()));
                }
            }
            "C" => {
                if let Some((key, name)) = rest.split_once(' ') {
                    store.custom.push(Custom {
                        key: key.to_string(),
                        name: name.trim_start().to_string(),
                        ..Custom::default()
                    });
                }
            }
            "P" => {
                if let Some(c) = store.custom.last_mut() {
                    c.program = rest.to_string();
                }
            }
            "A" => {
                if let Some(c) = store.custom.last_mut() {
                    c.args.push(rest.to_string());
                }
            }
            "W" => {
                if let Some(c) = store.custom.last_mut() {
                    c.cwd = Some(rest.to_string());
                }
            }
            "O" => store.order = rest.split_whitespace().map(str::to_string).collect(),
            _ => {}
        }
    }
    // A custom profile with no program can never launch; it would show in the
    // menu as an entry that silently fails.
    store.custom.retain(|c| !c.program.trim().is_empty());
    store
}

pub fn store_path() -> Option<PathBuf> {
    if let Some(p) = std::env::var_os("geist_PROFILES") {
        return Some(PathBuf::from(p));
    }
    Some(crate::config::config_dir()?.join("profiles"))
}

/// Write the store, creating the directory if needed. Failures are reported
/// and otherwise ignored — the profiles page has already applied the edits to
/// the running app, and a write error must not undo them mid-session.
pub fn save(store: &Store) {
    let Some(path) = store_path() else {
        return;
    };
    if let Some(dir) = path.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    if let Err(e) = std::fs::write(&path, serialize(store)) {
        eprintln!("geist: could not save profiles to {}: {e}", path.display());
    }
}

/// Read the saved deltas, or an empty set if there is no file.
pub fn load() -> Store {
    let Some(path) = store_path() else {
        return Store::default();
    };
    match std::fs::read_to_string(&path) {
        Ok(text) => parse(&text),
        Err(_) => Store::default(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn detected() -> Vec<Profile> {
        vec![
            Profile::new("PowerShell", "pwsh.exe"),
            Profile::new("Windows PowerShell", "powershell.exe"),
            Profile::new("Command Prompt", "cmd.exe"),
        ]
    }

    #[test]
    fn round_trips_every_field() {
        let store = Store {
            default: Some("cmd".into()),
            hidden: vec!["powershell".into()],
            names: vec![("pwsh".into(), "PowerShell 7".into())],
            custom: vec![Custom {
                key: "custom:1".into(),
                name: "Git Bash".into(),
                program: r"C:\Git\bin\bash.exe".into(),
                args: vec!["--login".into(), "-i".into()],
                cwd: Some(r"C:\src".into()),
            }],
            order: vec!["cmd".into(), "pwsh".into()],
        };
        assert_eq!(parse(&serialize(&store)), store);
    }

    #[test]
    fn a_foreign_or_missing_header_reads_as_no_edits() {
        assert_eq!(parse(""), Store::default());
        assert_eq!(parse("geist-profiles 2\nD cmd\n"), Store::default());
    }

    #[test]
    fn a_malformed_line_drops_only_that_record() {
        let s = parse("geist-profiles 1\nD cmd\n???\nZ junk\nX\nX powershell\n");
        assert_eq!(s.default.as_deref(), Some("cmd"));
        assert_eq!(s.hidden, vec!["powershell".to_string()]);
    }

    #[test]
    fn a_custom_profile_without_a_program_is_dropped() {
        let s = parse("geist-profiles 1\nC custom:1 Broken\nA --login\n");
        assert!(s.custom.is_empty());
    }

    #[test]
    fn names_with_spaces_survive_because_they_take_the_rest_of_the_line() {
        let s = parse("geist-profiles 1\nR pwsh My Favourite Shell\n");
        assert_eq!(s.names, vec![("pwsh".into(), "My Favourite Shell".into())]);
    }

    #[test]
    fn apply_hides_renames_and_reorders() {
        let store = Store {
            hidden: vec!["cmd".into()],
            names: vec![("pwsh".into(), "PS7".into())],
            order: vec!["powershell".into(), "pwsh".into()],
            ..Store::default()
        };
        let (list, _) = store.apply(detected(), 0);
        assert_eq!(
            list.iter().map(|p| p.key.as_str()).collect::<Vec<_>>(),
            ["powershell", "pwsh", "cmd"]
        );
        assert_eq!(list[1].name, "PS7");
        assert!(list[2].hidden);
    }

    #[test]
    fn apply_appends_customs_and_can_default_to_one() {
        let store = Store {
            default: Some("custom:1".into()),
            custom: vec![Custom {
                key: "custom:1".into(),
                name: "Git Bash".into(),
                program: r"C:\Git\bin\bash.exe".into(),
                args: vec!["--login".into()],
                cwd: Some(r"C:\src".into()),
            }],
            ..Store::default()
        };
        let (list, default) = store.apply(detected(), 0);
        assert_eq!(list[default].key, "custom:1");
        assert!(list[default].custom);
        assert_eq!(list[default].args, vec!["--login".to_string()]);
        assert_eq!(
            list[default].cwd.as_deref(),
            Some(std::path::Path::new(r"C:\src"))
        );
    }

    #[test]
    fn the_default_is_never_left_hidden() {
        let store = Store {
            default: Some("cmd".into()),
            hidden: vec!["cmd".into()],
            ..Store::default()
        };
        let (list, default) = store.apply(detected(), 0);
        assert_eq!(list[default].key, "cmd");
        assert!(!list[default].hidden);
    }

    #[test]
    fn a_default_naming_an_uninstalled_shell_falls_back_to_detection() {
        let store = Store {
            default: Some("nushell".into()),
            ..Store::default()
        };
        let (list, default) = store.apply(detected(), 1);
        assert_eq!(list[default].key, "powershell");
    }

    #[test]
    fn a_custom_key_colliding_with_a_detected_one_is_dropped() {
        let store = Store {
            custom: vec![Custom {
                key: "cmd".into(),
                name: "Impostor".into(),
                program: "evil.exe".into(),
                ..Custom::default()
            }],
            ..Store::default()
        };
        let (list, _) = store.apply(detected(), 0);
        assert_eq!(list.iter().filter(|p| p.key == "cmd").count(), 1);
        assert!(!list.iter().any(|p| p.name == "Impostor"));
    }

    #[test]
    fn capture_records_only_real_edits() {
        let (mut list, _) = Store::default().apply(detected(), 0);
        list[2].hidden = true;
        let store = Store::capture(&list, 1, &detected());
        assert!(store.names.is_empty(), "unchanged names are not renames");
        assert_eq!(store.hidden, vec!["cmd".to_string()]);
        assert_eq!(store.default.as_deref(), Some("powershell"));
        assert_eq!(store.order.len(), 3);
    }

    #[test]
    fn capture_then_apply_is_a_fixed_point() {
        let (mut list, _) = Store::default().apply(detected(), 0);
        list[0].name = "PS7".into();
        list[1].hidden = true;
        list.swap(0, 2);
        let store = Store::capture(&list, 0, &detected());
        let (again, default) = store.apply(detected(), 0);
        assert_eq!(
            again
                .iter()
                .map(|p| (p.key.as_str(), p.name.as_str(), p.hidden))
                .collect::<Vec<_>>(),
            list.iter()
                .map(|p| (p.key.as_str(), p.name.as_str(), p.hidden))
                .collect::<Vec<_>>()
        );
        assert_eq!(default, 0);
    }

    #[test]
    fn next_custom_key_skips_the_ones_in_use() {
        let store = Store {
            custom: vec![Custom {
                key: "custom:1".into(),
                name: "a".into(),
                program: "a.exe".into(),
                ..Custom::default()
            }],
            ..Store::default()
        };
        let (list, _) = store.apply(detected(), 0);
        assert_eq!(Store::next_custom_key(&list), "custom:2");
    }
}
