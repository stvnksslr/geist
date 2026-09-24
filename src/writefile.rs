//! `write_scrollback_file` / `write_screen_file` / `write_selection_file`:
//! dump terminal text to a temporary file and do something with its *path*.
//!
//! The three actions differ only in what text they capture; what happens next is
//! shared, and — the part that surprises people — the `copy`/`paste`/`open`
//! parameter operates on the **file path**, not the contents. `copy` puts the
//! path on the clipboard, `paste` types the path into the shell (so you can pipe
//! it somewhere), `open` hands it to the OS. That is Ghostty's design, and it is
//! what makes the feature useful: the point is to get a large scrollback *out*
//! of the terminal and into a real tool.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};

/// What to do with the path of the file just written. Ghostty's `WriteScreen`
/// action; the parameter of all three `write_*_file` bindings.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum WriteAction {
    /// Put the path on the clipboard.
    Copy,
    /// Type the path into the terminal.
    Paste,
    /// Hand the file to the OS' default handler for text.
    Open,
}

impl WriteAction {
    pub fn from_name(s: &str) -> Option<Self> {
        match s.trim().to_ascii_lowercase().as_str() {
            "copy" => Some(Self::Copy),
            "paste" => Some(Self::Paste),
            "open" => Some(Self::Open),
            _ => None,
        }
    }

    pub fn name(self) -> &'static str {
        match self {
            Self::Copy => "copy",
            Self::Paste => "paste",
            Self::Open => "open",
        }
    }
}

/// Which text to capture.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum WriteScope {
    /// Everything: scrollback plus the viewport.
    Scrollback,
    /// The visible viewport only.
    Screen,
    /// The current selection; does nothing when there is none.
    Selection,
}

impl WriteScope {
    pub fn name(self) -> &'static str {
        match self {
            Self::Scrollback => "scrollback",
            Self::Screen => "screen",
            Self::Selection => "selection",
        }
    }
}

/// Trailing blanks are stripped from every line and the whole capture, because a
/// terminal grid is a fixed rectangle: without this, a 200-column pane emits 200
/// characters per line and every "empty" row becomes a line of spaces.
pub fn tidy(lines: &[String]) -> String {
    let mut out = String::new();
    for line in lines {
        out.push_str(line.trim_end());
        out.push_str("\r\n");
    }
    // Collapse the run of blank lines a mostly-empty screen leaves at the end.
    let trimmed = out.trim_end_matches(['\r', '\n']);
    let mut s = trimmed.to_string();
    if !s.is_empty() {
        s.push_str("\r\n");
    }
    s
}

/// Build the file name for a capture: distinctive, sortable, and unmistakably
/// geist's, so a temp directory full of these is still navigable.
///
/// `stamp` is a caller-supplied counter rather than a clock, which keeps this
/// pure and testable — and avoids two captures in the same second colliding,
/// which a timestamp alone would do.
pub fn file_name(scope: WriteScope, stamp: u64) -> String {
    format!("geist-{}-{stamp}.txt", scope.name())
}

/// Write `text` to a uniquely named file in the system temp directory and return
/// its path.
pub fn write(dir: &Path, scope: WriteScope, stamp: u64, text: &str) -> Result<PathBuf> {
    let path = dir.join(file_name(scope, stamp));
    std::fs::write(&path, text).with_context(|| format!("could not write {}", path.display()))?;
    Ok(path)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn action_names_round_trip() {
        for a in [WriteAction::Copy, WriteAction::Paste, WriteAction::Open] {
            assert_eq!(WriteAction::from_name(a.name()), Some(a));
        }
        // Case and surrounding space are tolerated, like every other config value.
        assert_eq!(WriteAction::from_name(" OPEN "), Some(WriteAction::Open));
        assert_eq!(WriteAction::from_name("mail-it"), None);
    }

    #[test]
    fn tidy_strips_the_grids_padding() {
        // A terminal row is the full width of the pane, so without trimming every
        // line carries its tail of spaces and blank rows become space-only lines.
        let lines = vec![
            "hello        ".to_string(),
            "             ".to_string(),
            "world        ".to_string(),
        ];
        assert_eq!(tidy(&lines), "hello\r\n\r\nworld\r\n");
    }

    #[test]
    fn tidy_drops_the_empty_tail_a_mostly_blank_screen_leaves() {
        // The common case: a few lines of output at the top of a 24-row grid.
        let mut lines = vec!["output".to_string()];
        lines.extend(std::iter::repeat_n(String::new(), 23));
        assert_eq!(tidy(&lines), "output\r\n");
    }

    #[test]
    fn tidy_of_nothing_is_nothing() {
        assert_eq!(tidy(&[]), "");
        assert_eq!(tidy(&["".to_string(), "   ".to_string()]), "");
    }

    #[test]
    fn tidy_keeps_interior_blank_lines() {
        // Only the *trailing* run collapses — a blank line between paragraphs is
        // content and has to survive.
        let lines = vec![
            "a".to_string(),
            String::new(),
            "b".to_string(),
            String::new(),
        ];
        assert_eq!(tidy(&lines), "a\r\n\r\nb\r\n");
    }

    #[test]
    fn file_names_are_distinctive_and_unique() {
        let a = file_name(WriteScope::Scrollback, 1);
        let b = file_name(WriteScope::Scrollback, 2);
        assert_ne!(a, b, "two captures must not collide");
        assert!(a.starts_with("geist-"), "{a}");
        assert!(a.ends_with(".txt"), "{a}");
        assert!(a.contains("scrollback"), "{a}");
        assert!(file_name(WriteScope::Selection, 1).contains("selection"));
    }

    #[test]
    fn write_round_trips_through_a_real_file() {
        let dir = std::env::temp_dir();
        let p = write(&dir, WriteScope::Screen, 424242, "hi\r\n").expect("write");
        assert_eq!(std::fs::read_to_string(&p).unwrap(), "hi\r\n");
        let _ = std::fs::remove_file(p);
    }
}
