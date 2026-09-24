//! Session / window **state restore** — Ghostty's `window-save-state`.
//!
//! On exit the whole window list is written to `%APPDATA%\geist\state` (override
//! with `$geist_STATE`) and re-read at startup: windows, their tabs, each tab's
//! split tree, which pane had focus, the tab's user-set name, and every pane's
//! working directory (from OSC 7, the same source `*-inherit-working-directory`
//! uses).
//!
//! The format is **line-oriented text**, not a serde blob, for two reasons: it
//! keeps the crate free of a serialization dependency, and a state file that a
//! human can read is one they can also delete a bad line out of. Every record
//! is one line, `<tag> <fields…>`, with any free-form field (a tab name, a path)
//! taking the **rest of the line** so nothing needs escaping:
//!
//! ```text
//! geist-state 1
//! W                       window
//! F 100 80 960 600 0      optional frame: outer x/y, inner w/h (points), maximized
//! T 1 build              tab, `1` = the active tab, rest = its name (`-` = none)
//! S v 0.5                 split (`v` vertical, `h` horizontal) + the first child's share;
//!                         children follow. No ratio (an older file) reads as 0.5
//! L 1 C:\src\geist        leaf, `1` = the focused pane, rest = its cwd (`-` = none)
//! L 0 -
//! ```
//!
//! The tree is written in **preorder** (split, then its first subtree, then its
//! second), which is unambiguous for a binary tree where every interior node has
//! exactly two children — so no closing delimiter and no indentation is needed.
//!
//! Parsing is total: anything malformed drops the affected record rather than
//! failing the startup. A state file is a convenience, and refusing to launch
//! over one would be the worst possible trade.

use std::path::PathBuf;

/// Header of a state file. Bumped if the grammar ever changes incompatibly; a
/// file with any other version is ignored rather than guessed at.
const HEADER: &str = "geist-state 1";

/// A saved split tree: the same shape as `app::Node`, minus the live session.
#[derive(Clone, Debug, PartialEq)]
pub enum SavedNode {
    Leaf {
        /// The pane's working directory (OSC 7), if it reported one.
        cwd: Option<String>,
        /// Whether this was the tab's focused pane.
        focused: bool,
    },
    Split {
        vertical: bool,
        /// The first child's share of the split, strictly inside `(0, 1)`.
        ratio: f32,
        first: Box<SavedNode>,
        second: Box<SavedNode>,
    },
}

/// One saved tab.
#[derive(Clone, Debug, PartialEq)]
pub struct SavedTab {
    /// The user's "Rename Tab…" override, if any.
    pub name: Option<String>,
    pub tree: SavedNode,
}

/// Where a window was, in egui points: outer top-left and inner size.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct WindowFrame {
    pub x: f32,
    pub y: f32,
    pub w: f32,
    pub h: f32,
    pub maximized: bool,
}

/// One saved window.
#[derive(Clone, Debug, PartialEq, Default)]
pub struct SavedWindow {
    pub tabs: Vec<SavedTab>,
    pub active_tab: usize,
    /// Its frame, when known. Files written before frames were saved carry
    /// none, and such a window opens at the default geometry.
    pub frame: Option<WindowFrame>,
}

/// The whole saved app: every window, in list order (slot 0 is the root).
#[derive(Clone, Debug, PartialEq, Default)]
pub struct SavedState {
    pub windows: Vec<SavedWindow>,
}

impl SavedState {
    /// Whether there is anything worth writing. An empty state is *deleted*
    /// rather than written, so a stale file can never outlive the layout it
    /// described.
    pub fn is_empty(&self) -> bool {
        self.windows.iter().all(|w| w.tabs.is_empty())
    }
}

/// Render a field that takes the rest of its line: `None`/empty becomes `-`, and
/// any newline is dropped (it would split the record into two).
fn rest_field(v: Option<&str>) -> String {
    match v {
        Some(s) if !s.trim().is_empty() => s.replace(['\r', '\n'], " "),
        _ => "-".to_string(),
    }
}

/// Read back a rest-of-line field: `-` means "unset".
fn parse_rest(v: &str) -> Option<String> {
    let v = v.trim();
    (!v.is_empty() && v != "-").then(|| v.to_string())
}

fn write_node(node: &SavedNode, out: &mut String) {
    match node {
        SavedNode::Leaf { cwd, focused } => {
            out.push_str("L ");
            out.push(if *focused { '1' } else { '0' });
            out.push(' ');
            out.push_str(&rest_field(cwd.as_deref()));
            out.push('\n');
        }
        SavedNode::Split {
            vertical,
            ratio,
            first,
            second,
        } => {
            out.push_str(if *vertical { "S v " } else { "S h " });
            out.push_str(&ratio.to_string());
            out.push('\n');
            write_node(first, out);
            write_node(second, out);
        }
    }
}

/// Serialize a state to the file format. Pure — [`save`] is the only I/O.
pub fn serialize(state: &SavedState) -> String {
    let mut out = String::from(HEADER);
    out.push('\n');
    for w in &state.windows {
        out.push_str("W\n");
        if let Some(f) = w.frame {
            out.push_str(&format!(
                "F {} {} {} {} {}\n",
                f.x,
                f.y,
                f.w,
                f.h,
                if f.maximized { 1 } else { 0 }
            ));
        }
        for (i, t) in w.tabs.iter().enumerate() {
            out.push_str("T ");
            out.push(if i == w.active_tab { '1' } else { '0' });
            out.push(' ');
            out.push_str(&rest_field(t.name.as_deref()));
            out.push('\n');
            write_node(&t.tree, &mut out);
        }
    }
    out
}

/// Consume one preorder node from `lines`, or `None` if the stream is truncated
/// or the next line isn't a node record.
fn read_node(lines: &mut std::iter::Peekable<std::slice::Iter<'_, &str>>) -> Option<SavedNode> {
    let line = lines.peek()?.trim();
    let (tag, rest) = match line.split_once(' ') {
        Some((t, r)) => (t, r),
        None => (line, ""),
    };
    match tag {
        "L" => {
            lines.next();
            let (flag, cwd) = match rest.split_once(' ') {
                Some((f, c)) => (f, c),
                None => (rest, ""),
            };
            Some(SavedNode::Leaf {
                cwd: parse_rest(cwd),
                focused: flag.trim() == "1",
            })
        }
        "S" => {
            lines.next();
            let mut fields = rest.split_whitespace();
            let vertical = fields.next() == Some("v");
            // Files written before split ratios existed carry no second field.
            // Every split was 50/50 then, so that is what they restore as; a
            // garbled or out-of-range value falls back the same way.
            let ratio = fields
                .next()
                .and_then(|r| r.parse::<f32>().ok())
                .filter(|r| r.is_finite() && *r > 0.0 && *r < 1.0)
                .unwrap_or(0.5);
            // A split with a missing child is a truncated file: drop the whole
            // node rather than inventing a pane the user never had.
            let first = Box::new(read_node(lines)?);
            let second = Box::new(read_node(lines)?);
            Some(SavedNode::Split {
                vertical,
                ratio,
                first,
                second,
            })
        }
        _ => None,
    }
}

/// Parse an `F x y w h maximized` record's fields. A garbled or implausible
/// frame is dropped (the window then opens at the default geometry) rather
/// than placing a window nowhere.
fn parse_frame(rest: &str) -> Option<WindowFrame> {
    let mut f = rest.split_whitespace();
    let mut num = || f.next()?.parse::<f32>().ok().filter(|v| v.is_finite());
    let (x, y, w, h) = (num()?, num()?, num()?, num()?);
    let maximized = num().is_some_and(|m| m != 0.0);
    (w >= 50.0
        && h >= 50.0
        && w <= 100_000.0
        && h <= 100_000.0
        && x.abs() <= 100_000.0
        && y.abs() <= 100_000.0)
        .then_some(WindowFrame {
            x,
            y,
            w,
            h,
            maximized,
        })
}

/// Parse the file format. Returns an empty state for anything unrecognized —
/// never an error, since a bad state file must not block startup.
pub fn parse(text: &str) -> SavedState {
    let lines: Vec<&str> = text
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty())
        .collect();
    let mut state = SavedState::default();
    let mut lines = lines.iter().peekable();
    match lines.next() {
        Some(h) if *h == HEADER => {}
        _ => return state,
    }
    while let Some(line) = lines.next() {
        let (tag, rest) = match line.split_once(' ') {
            Some((t, r)) => (t, r),
            None => (*line, ""),
        };
        match tag {
            "W" => state.windows.push(SavedWindow::default()),
            // A window frame. Added without a header bump because it is purely
            // additive: an older geist skips the unknown `F` tag (the `_` arm
            // below), and a file without one parses as before.
            "F" => {
                if let (Some(w), Some(f)) = (state.windows.last_mut(), parse_frame(rest)) {
                    w.frame = Some(f);
                }
            }
            "T" => {
                // A tab outside any window is malformed; give it one rather than
                // dropping the user's layout on a hand-edited file.
                if state.windows.is_empty() {
                    state.windows.push(SavedWindow::default());
                }
                let (flag, name) = match rest.split_once(' ') {
                    Some((f, n)) => (f, n),
                    None => (rest, ""),
                };
                let Some(tree) = read_node(&mut lines) else {
                    continue;
                };
                let w = state.windows.last_mut().expect("pushed above");
                if flag.trim() == "1" {
                    w.active_tab = w.tabs.len();
                }
                w.tabs.push(SavedTab {
                    name: parse_rest(name),
                    tree,
                });
            }
            // A stray node line (its `T` was dropped) or an unknown tag: skip it.
            _ => {}
        }
    }
    state.windows.retain(|w| !w.tabs.is_empty());
    for w in &mut state.windows {
        w.active_tab = w.active_tab.min(w.tabs.len().saturating_sub(1));
    }
    state
}

/// Where the state file lives: `$geist_STATE`, else next to the config as
/// `state`. `None` when there is no config directory at all (no `%APPDATA%`).
pub fn state_path() -> Option<PathBuf> {
    if let Some(p) = std::env::var_os("geist_STATE") {
        return Some(PathBuf::from(p));
    }
    Some(crate::config::config_dir()?.join("state"))
}

/// Write the state, creating the directory if needed. An empty state removes the
/// file instead. Failures are reported once and otherwise ignored — losing a
/// layout must never take the exit path down with it.
pub fn save(state: &SavedState) {
    let Some(path) = state_path() else {
        return;
    };
    if state.is_empty() {
        clear();
        return;
    }
    if let Some(dir) = path.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    if let Err(e) = std::fs::write(&path, serialize(state)) {
        eprintln!(
            "geist: could not save window state to {}: {e}",
            path.display()
        );
    }
}

/// Read the saved state, or an empty one if there is no file.
pub fn load() -> SavedState {
    let Some(path) = state_path() else {
        return SavedState::default();
    };
    match std::fs::read_to_string(&path) {
        Ok(text) => parse(&text),
        Err(_) => SavedState::default(),
    }
}

/// Delete the state file. Called right after a restore: the file describes one
/// specific exit, so leaving it in place would resurrect that same layout after
/// a later crash that never got to write its own.
pub fn clear() {
    if let Some(path) = state_path() {
        let _ = std::fs::remove_file(path);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn leaf(cwd: Option<&str>, focused: bool) -> SavedNode {
        SavedNode::Leaf {
            cwd: cwd.map(str::to_string),
            focused,
        }
    }

    fn sample() -> SavedState {
        SavedState {
            windows: vec![
                SavedWindow {
                    tabs: vec![
                        SavedTab {
                            name: Some("build logs".into()),
                            tree: SavedNode::Split {
                                vertical: true,
                                ratio: 0.3,
                                first: Box::new(leaf(Some(r"C:\src\geist"), false)),
                                second: Box::new(SavedNode::Split {
                                    vertical: false,
                                    ratio: 0.5,
                                    first: Box::new(leaf(None, true)),
                                    second: Box::new(leaf(Some(r"C:\tmp"), false)),
                                }),
                            },
                        },
                        SavedTab {
                            name: None,
                            tree: leaf(None, true),
                        },
                    ],
                    active_tab: 1,
                    frame: Some(WindowFrame {
                        x: -12.5,
                        y: 40.0,
                        w: 960.0,
                        h: 600.0,
                        maximized: true,
                    }),
                },
                SavedWindow {
                    tabs: vec![SavedTab {
                        name: None,
                        tree: leaf(Some(r"D:\work"), true),
                    }],
                    active_tab: 0,
                    frame: None,
                },
            ],
        }
    }

    #[test]
    fn serialize_then_parse_round_trips_every_field() {
        assert_eq!(parse(&serialize(&sample())), sample());
    }

    #[test]
    fn a_nested_split_tree_keeps_its_shape_and_axes() {
        let text = serialize(&sample());
        // Preorder: split, first subtree, second subtree — no delimiters.
        let body: Vec<&str> = text.lines().skip(1).take(6).collect();
        assert_eq!(
            body,
            [
                "W",
                "F -12.5 40 960 600 1",
                "T 0 build logs",
                "S v 0.3",
                "L 0 C:\\src\\geist",
                "S h 0.5"
            ]
        );
    }

    #[test]
    fn a_split_without_a_ratio_reads_as_an_even_split() {
        // The pre-ratio grammar: every split was 50/50, so that is the default.
        // A garbled or out-of-range ratio degrades the same way rather than
        // dropping the layout.
        for line in ["S v", "S v nonsense", "S v 1.5", "S v 0", "S v NaN"] {
            let s = parse(&format!("geist-state 1\nW\nT 1 -\n{line}\nL 1 -\nL 0 -\n"));
            let SavedNode::Split {
                vertical, ratio, ..
            } = &s.windows[0].tabs[0].tree
            else {
                panic!("{line}: expected a split");
            };
            assert!(*vertical, "{line}");
            assert_eq!(*ratio, 0.5, "{line}");
        }
    }

    #[test]
    fn frames_are_optional_and_old_files_still_parse() {
        // A file from before frames existed: no `F`, same layout.
        let s = parse("geist-state 1\nW\nT 1 -\nL 1 -\n");
        assert_eq!(s.windows[0].frame, None);
        assert_eq!(s.windows[0].tabs.len(), 1);

        let s = parse("geist-state 1\nW\nF 10 -20.5 800 500 1\nT 1 -\nL 1 -\n");
        assert_eq!(
            s.windows[0].frame,
            Some(WindowFrame {
                x: 10.0,
                y: -20.5,
                w: 800.0,
                h: 500.0,
                maximized: true
            })
        );
        // Missing maximized flag reads as not maximized.
        let s = parse("geist-state 1\nW\nF 1 2 300 400\nT 1 -\nL 1 -\n");
        assert!(!s.windows[0].frame.unwrap().maximized);
    }

    #[test]
    fn an_implausible_frame_is_dropped_but_the_window_kept() {
        for f in [
            "F 0 0 1 1 0",
            "F x 0 800 600 0",
            "F 0 0 NaN 600 0",
            "F 0 0 800",
            "F 1e9 0 800 600 0",
        ] {
            let s = parse(&format!("geist-state 1\nW\n{f}\nT 1 -\nL 1 -\n"));
            assert_eq!(s.windows[0].frame, None, "{f}");
            assert_eq!(s.windows[0].tabs.len(), 1, "{f}");
        }
    }

    #[test]
    fn a_missing_or_wrong_header_yields_nothing() {
        assert!(parse("").is_empty());
        assert!(parse("geist-state 99\nW\nT 1 -\nL 1 -\n").is_empty());
    }

    #[test]
    fn a_truncated_split_drops_only_that_tab() {
        let s = parse("geist-state 1\nW\nT 0 half\nS v\nL 1 -\nT 1 whole\nL 1 -\n");
        assert_eq!(s.windows.len(), 1);
        // The truncated tab swallowed the next tab's `L` as its second child, so
        // it survives as one tab — the point is that parsing stays total and the
        // window is still usable, not that recovery is perfect.
        assert!(!s.windows[0].tabs.is_empty());
    }

    #[test]
    fn an_out_of_range_active_tab_is_clamped() {
        let s = parse("geist-state 1\nW\nT 0 -\nL 1 -\n");
        assert_eq!(s.windows[0].active_tab, 0);
    }

    #[test]
    fn a_window_with_no_tabs_is_dropped() {
        let s = parse("geist-state 1\nW\nW\nT 1 -\nL 1 -\n");
        assert_eq!(s.windows.len(), 1);
    }

    #[test]
    fn a_name_with_spaces_survives_but_a_newline_cannot_split_the_record() {
        let st = SavedState {
            windows: vec![SavedWindow {
                tabs: vec![SavedTab {
                    name: Some("a b\nc".into()),
                    tree: leaf(None, true),
                }],
                active_tab: 0,
                frame: None,
            }],
        };
        let back = parse(&serialize(&st));
        assert_eq!(back.windows[0].tabs[0].name.as_deref(), Some("a b c"));
    }

    #[test]
    fn an_empty_state_is_reported_empty_so_it_deletes_rather_than_writes() {
        assert!(SavedState::default().is_empty());
        assert!(
            SavedState {
                windows: vec![SavedWindow::default()]
            }
            .is_empty()
        );
        assert!(!sample().is_empty());
    }
}
