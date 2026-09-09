//! The terminal inspector — Ghostty's `inspector:toggle|show|hide`.
//!
//! Ghostty's inspector is ~4500 lines of Dear ImGui across five dockable
//! windows (Keyboard, Terminal, Surface, Terminal IO, Renderer), reading state
//! straight out of a `Terminal` it owns in-process. giest's is an egui window
//! over the pane it belongs to, and it necessarily shows *different* things:
//! the terminal lives behind [`crate::engine::TerminalEngine`], so the panels
//! that matter here are the ones giest can source honestly.
//!
//! This module is the **capture** half — bounded ring buffers of what crossed
//! the two chokepoints, plus the byte rendering that makes them readable. It is
//! pure and unit-tested; `app.rs` draws it.
//!
//! Two design points are worth stating, because both are deliberate inversions
//! of rules the rest of this codebase follows:
//!
//! - **The inspector is not a modal.** Every other overlay giest has takes the
//!   keyboard (see CLAUDE.md's two-gate rule). This one must not: the keyboard
//!   log is worthless if opening it stops you typing, and watching a program
//!   redraw is the reason you opened the IO log. It is added to *neither* gate.
//! - **Capture costs nothing while it is closed.** The log is an `Option` on the
//!   `Session`, `None` until an inspector is opened on that pane, so the record
//!   calls on the input and PTY paths compile down to a null check. That is why
//!   it is per-pane state rather than something the window owns.
//!
//! The IO log is the permanent form of a probe CLAUDE.md already recommends
//! reaching for: ConPTY re-renders a child's output and silently drops
//! sequences it doesn't understand (APC, and so kitty graphics), so "does this
//! escape actually arrive?" is the first question of any escape-sequence work.
//! It used to mean adding a temporary `eprintln!` to `GhosttyVtEngine::write`.

use std::collections::VecDeque;

/// How many key presses are kept. Small: this log is read by eye, and the
/// interesting press is always the most recent one.
pub const MAX_KEYS: usize = 200;

/// How many PTY reads are kept. Larger, because one screen redraw is many.
pub const MAX_IO: usize = 500;

/// How much of a single PTY read is rendered. A full-screen redraw is tens of
/// kilobytes and nobody reads that in a list; the byte count is reported
/// separately, so nothing is *hidden* by the cap — only elided.
pub const IO_PREVIEW: usize = 200;

/// Ghostty's `inspector:<mode>` parameter.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum InspectorMode {
    Toggle,
    Show,
    Hide,
}

impl InspectorMode {
    pub fn from_name(s: &str) -> Option<Self> {
        match s.trim() {
            "toggle" => Some(Self::Toggle),
            "show" => Some(Self::Show),
            "hide" => Some(Self::Hide),
            _ => None,
        }
    }

    /// Whether an inspector should be open after applying this mode to a pane
    /// that currently `is_open`.
    ///
    /// Pulled out of the session so the one branch in the feature has a test:
    /// `show` on an already-open inspector must be a **no-op** rather than a
    /// restart, or binding `inspector:show` would silently discard the capture
    /// you were reading. Same rule as `start_search`.
    pub fn wants(self, is_open: bool) -> bool {
        match self {
            Self::Toggle => !is_open,
            Self::Show => true,
            Self::Hide => false,
        }
    }

    pub fn name(self) -> &'static str {
        match self {
            Self::Toggle => "toggle",
            Self::Show => "show",
            Self::Hide => "hide",
        }
    }
}

/// What the input path decided to do with a key press.
///
/// The three outcomes are exactly `session::KeyAction` plus "it was a printable
/// key, so the text event carried it" — which is the case that most often looks
/// like a bug from outside (the key produced bytes, but not from the encoder).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum KeyOutcome {
    /// Encoded and sent to the shell.
    Encoded,
    /// Swallowed by the app: a binding ran, or the chord is in a reserved
    /// namespace.
    Swallowed,
    /// Left for the matching `Event::Text`, which types the character.
    Text,
}

impl KeyOutcome {
    pub fn label(self) -> &'static str {
        match self {
            Self::Encoded => "encoded",
            Self::Swallowed => "swallowed",
            Self::Text => "text",
        }
    }
}

/// One key press as the input path saw it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct KeyRecord {
    pub seq: u64,
    /// The chord in **config spelling** — what you would type after
    /// `keybind =` to bind this key. That is the question the keyboard log
    /// exists to answer, so a debug rendering would not do.
    pub chord: String,
    pub outcome: KeyOutcome,
    /// The bytes that reached the shell, rendered. Empty unless `Encoded`.
    pub bytes: String,
}

/// One read from the PTY.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct IoRecord {
    pub seq: u64,
    /// Bytes in the read, *before* the preview cap.
    pub len: usize,
    pub text: String,
    /// Whether `text` was cut short at [`IO_PREVIEW`].
    pub truncated: bool,
}

/// One pane's capture.
#[derive(Debug, Default)]
pub struct Log {
    keys: VecDeque<KeyRecord>,
    io: VecDeque<IoRecord>,
    /// While paused nothing is recorded. The list is unreadable under a
    /// streaming program otherwise, and pausing the *capture* rather than the
    /// view means the entries you stop on are the ones you were looking at.
    pub paused: bool,
    next_seq: u64,
}

impl Log {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn keys(&self) -> impl DoubleEndedIterator<Item = &KeyRecord> {
        self.keys.iter()
    }

    pub fn io(&self) -> impl DoubleEndedIterator<Item = &IoRecord> {
        self.io.iter()
    }

    pub fn is_empty(&self) -> bool {
        self.keys.is_empty() && self.io.is_empty()
    }

    pub fn clear(&mut self) {
        self.keys.clear();
        self.io.clear();
        // The counter deliberately keeps going: a cleared log that restarts at
        // #1 makes two captures impossible to tell apart in a screenshot.
    }

    /// Record a key press. `bytes` is what the encoder produced, if anything.
    pub fn key(&mut self, chord: String, outcome: KeyOutcome, bytes: &[u8]) {
        if self.paused {
            return;
        }
        let seq = self.tick();
        push_capped(
            &mut self.keys,
            KeyRecord {
                seq,
                chord,
                outcome,
                bytes: render_bytes(bytes, bytes.len()).0,
            },
            MAX_KEYS,
        );
    }

    /// Record a read from the PTY.
    pub fn read(&mut self, bytes: &[u8]) {
        if self.paused || bytes.is_empty() {
            return;
        }
        let seq = self.tick();
        let (text, truncated) = render_bytes(bytes, IO_PREVIEW);
        push_capped(
            &mut self.io,
            IoRecord {
                seq,
                len: bytes.len(),
                text,
                truncated,
            },
            MAX_IO,
        );
    }

    fn tick(&mut self) -> u64 {
        self.next_seq += 1;
        self.next_seq
    }
}

fn push_capped<T>(q: &mut VecDeque<T>, item: T, cap: usize) {
    if q.len() == cap {
        q.pop_front();
    }
    q.push_back(item);
}

/// Render bytes so every one of them is visible and distinguishable.
///
/// Returns the rendering and whether it was cut short at `limit` **input
/// bytes** (not output characters — the cap has to be on what was read, or a
/// stream of escapes would be trimmed at a different point than a stream of
/// text and the byte counts would stop lining up).
///
/// Controls become their Unicode Control Pictures (`ESC` → `␛`, `BEL` → `␇`,
/// `CR` → `␍`), which is **not** what [`crate::app`]'s clipboard preview does:
/// that one collapses every control but a few onto a single `␦`, because it is
/// showing a human what they are about to paste. Here the whole question is
/// *which* control arrived, so each gets its own glyph. Invalid UTF-8 is shown
/// as `\xNN` rather than U+FFFD, for the same reason.
pub fn render_bytes(bytes: &[u8], limit: usize) -> (String, bool) {
    let truncated = bytes.len() > limit;
    let shown = &bytes[..bytes.len().min(limit)];
    let mut out = String::with_capacity(shown.len() + 8);
    let mut i = 0;
    while i < shown.len() {
        let b = shown[i];
        if b < 0x20 {
            // The Control Pictures block is laid out to mirror C0 exactly, so
            // the arithmetic is the table.
            out.push(char::from_u32(0x2400 + u32::from(b)).unwrap_or('?'));
            i += 1;
        } else if b == 0x7f {
            out.push('\u{2421}'); // ␡
            i += 1;
        } else if b < 0x80 {
            out.push(b as char);
            i += 1;
        } else {
            // A multi-byte sequence, decoded so real text stays readable. Only
            // the *complete* prefix is decoded: a sequence cut in half by the
            // preview cap is rendered byte-wise rather than as a replacement
            // character, which would look like corruption in the stream.
            match std::str::from_utf8(&shown[i..]) {
                Ok(s) => {
                    out.push_str(s);
                    break;
                }
                Err(e) => {
                    let good = e.valid_up_to();
                    if good > 0 {
                        // Safe by `valid_up_to`'s contract.
                        out.push_str(std::str::from_utf8(&shown[i..i + good]).unwrap_or_default());
                        i += good;
                    } else {
                        out.push_str(&format!("\\x{b:02x}"));
                        i += 1;
                    }
                }
            }
        }
    }
    (out, truncated)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mode_names_round_trip() {
        for m in [InspectorMode::Toggle, InspectorMode::Show, InspectorMode::Hide] {
            assert_eq!(InspectorMode::from_name(m.name()), Some(m));
        }
        assert_eq!(InspectorMode::from_name("open"), None);
    }

    #[test]
    fn show_on_an_open_inspector_keeps_the_capture() {
        use InspectorMode::*;
        assert!(Toggle.wants(false));
        assert!(!Toggle.wants(true));
        assert!(Show.wants(false));
        // The one that matters: `show` must not restart an open inspector, or
        // the log you were reading is gone.
        assert!(Show.wants(true));
        assert!(!Hide.wants(false));
        assert!(!Hide.wants(true));
    }

    #[test]
    fn every_control_byte_gets_its_own_glyph() {
        // The point of the IO log is *which* control arrived, so unlike the
        // clipboard preview these must not collapse onto one marker.
        let (s, _) = render_bytes(&[0x1b, 0x07, 0x0d, 0x0a, 0x09, 0x00], 64);
        assert_eq!(s, "␛␇␍␊␉␀");
        let (del, _) = render_bytes(&[0x7f], 64);
        assert_eq!(del, "␡");
    }

    #[test]
    fn a_real_escape_sequence_reads_as_itself() {
        // `ESC [ 2 J` — the shape someone is actually squinting at when they
        // open this panel.
        let (s, _) = render_bytes(b"\x1b[2J", 64);
        assert_eq!(s, "␛[2J");
    }

    #[test]
    fn utf8_survives_and_invalid_bytes_are_shown_as_hex() {
        let (s, _) = render_bytes("héllo→".as_bytes(), 64);
        assert_eq!(s, "héllo→");
        // A lone continuation byte is not U+FFFD: replacement would read as
        // corruption in the *terminal* rather than in the byte stream.
        let (s, _) = render_bytes(&[0x41, 0x80, 0x42], 64);
        assert_eq!(s, "A\\x80B");
    }

    #[test]
    fn the_cap_counts_input_bytes_and_is_reported() {
        let (s, truncated) = render_bytes(b"abcdef", 3);
        assert_eq!(s, "abc");
        assert!(truncated);
        let (s, truncated) = render_bytes(b"abc", 3);
        assert_eq!(s, "abc");
        assert!(!truncated);
        // A multi-byte character straddling the cap degrades to hex rather than
        // to a replacement character.
        let (s, _) = render_bytes("é".as_bytes(), 1);
        assert_eq!(s, "\\xc3");
    }

    #[test]
    fn the_ring_buffers_drop_the_oldest() {
        let mut log = Log::new();
        for i in 0..MAX_IO + 10 {
            log.read(format!("{i}").as_bytes());
        }
        assert_eq!(log.io().count(), MAX_IO);
        // The oldest survivor is #11, and the sequence numbers say so.
        assert_eq!(log.io().next().expect("head").seq, 11);
        assert_eq!(log.io().next_back().expect("tail").seq, (MAX_IO + 10) as u64);
    }

    #[test]
    fn pausing_stops_capture_rather_than_the_view() {
        let mut log = Log::new();
        log.read(b"before");
        log.paused = true;
        log.read(b"during");
        log.key("ctrl+c".into(), KeyOutcome::Encoded, b"\x03");
        assert_eq!(log.io().count(), 1);
        assert_eq!(log.keys().count(), 0);
        log.paused = false;
        log.read(b"after");
        assert_eq!(log.io().count(), 2);
    }

    #[test]
    fn an_empty_read_is_not_an_entry() {
        // `pump_pty` can wake with nothing; a list of empty rows would bury the
        // reads that matter.
        let mut log = Log::new();
        log.read(b"");
        assert!(log.is_empty());
    }

    #[test]
    fn clearing_keeps_the_sequence_running() {
        // Two captures that both start at #1 are indistinguishable in a
        // screenshot, which is how this log is usually shared.
        let mut log = Log::new();
        log.read(b"a");
        log.clear();
        log.read(b"b");
        assert_eq!(log.io().next().expect("one entry").seq, 2);
    }

    #[test]
    fn a_key_record_carries_the_bytes_it_produced() {
        let mut log = Log::new();
        log.key("ctrl+c".into(), KeyOutcome::Encoded, b"\x03");
        log.key("ctrl+shift+t".into(), KeyOutcome::Swallowed, b"");
        let recs: Vec<_> = log.keys().collect();
        assert_eq!(recs[0].bytes, "␃");
        assert_eq!(recs[0].outcome.label(), "encoded");
        assert!(recs[1].bytes.is_empty());
    }
}
