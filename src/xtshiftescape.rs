//! `XTSHIFTESCAPE` (`CSI > Ps s`) via a side scan of the PTY stream.
//!
//! A program uses it to say whether Shift+click should reach it (`Ps = 1`) or
//! stay with the terminal for selection (`Ps = 0` or no parameter). libghostty
//! records it in `Terminal.flags.mouse_shift_capture`, but the C API exposes no
//! getter, so — like OSC 7 / 52 / 133 — giest parses the same bytes it feeds the
//! engine. `mouse-shift-capture = true|false` defers to the latest request
//! ([`crate::config::MouseShiftCapture::captured`]); `always|never` ignore it.
//!
//! Parameter grammar copied from Ghostty's `stream.zig`: zero params is
//! `false`, one param of `0`/`1` is `false`/`true`, anything else is ignored.
//!
//! Unverified: whether ConPTY forwards this sequence (it re-renders the child's
//! stream and drops what it doesn't understand — see CLAUDE.md). If it does
//! not, the scan simply never fires and the config value alone decides.

#[derive(Clone, Copy, PartialEq, Eq)]
enum State {
    Ground,
    Esc,
    Csi,
}

/// Longest CSI parameter string worth tracking; anything longer is not ours.
const MAX_PARAMS: usize = 16;

pub struct ShiftEscapeScanner {
    state: State,
    /// Saw the `>` private marker as the first CSI byte.
    gt: bool,
    /// Whether this CSI can still be an XTSHIFTESCAPE.
    candidate: bool,
    params: Vec<u8>,
    /// The program's latest request, `None` until it makes one.
    capture: Option<bool>,
}

impl Default for ShiftEscapeScanner {
    fn default() -> Self {
        Self::new()
    }
}

impl ShiftEscapeScanner {
    pub fn new() -> Self {
        Self {
            state: State::Ground,
            gt: false,
            candidate: false,
            params: Vec::new(),
            capture: None,
        }
    }

    /// The program's latest XTSHIFTESCAPE request, if any.
    pub fn capture(&self) -> Option<bool> {
        self.capture
    }

    pub fn feed(&mut self, bytes: &[u8]) {
        for &b in bytes {
            match self.state {
                State::Ground => {
                    if b == 0x1b {
                        self.state = State::Esc;
                    }
                }
                State::Esc => match b {
                    b'[' => {
                        self.state = State::Csi;
                        self.gt = false;
                        self.candidate = true;
                        self.params.clear();
                    }
                    // RIS (`ESC c`) resets the terminal's flags, this one included.
                    b'c' => {
                        self.capture = None;
                        self.state = State::Ground;
                    }
                    0x1b => {}
                    _ => self.state = State::Ground,
                },
                State::Csi => match b {
                    b'>' if self.params.is_empty() && !self.gt => self.gt = true,
                    b'0'..=b'9' | b';' => {
                        if self.params.len() < MAX_PARAMS {
                            self.params.push(b);
                        } else {
                            self.candidate = false;
                        }
                    }
                    0x40..=0x7e => {
                        if b == b's' && self.gt && self.candidate {
                            self.apply();
                        }
                        self.state = State::Ground;
                    }
                    0x1b => self.state = State::Esc,
                    // Intermediates or other private markers: not this sequence.
                    _ => self.candidate = false,
                },
            }
        }
    }

    fn apply(&mut self) {
        let p = std::str::from_utf8(&self.params).unwrap_or("");
        let v = match p {
            "" => Some(false),
            _ if p.contains(';') => None,
            _ => match p.parse::<u32>() {
                Ok(0) => Some(false),
                Ok(1) => Some(true),
                _ => None,
            },
        };
        if v.is_some() {
            self.capture = v;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scan(chunks: &[&[u8]]) -> Option<bool> {
        let mut s = ShiftEscapeScanner::new();
        for c in chunks {
            s.feed(c);
        }
        s.capture()
    }

    #[test]
    fn parses_upstream_grammar() {
        assert_eq!(scan(&[b"hello"]), None);
        assert_eq!(scan(&[b"\x1b[>1s"]), Some(true));
        assert_eq!(scan(&[b"\x1b[>0s"]), Some(false));
        assert_eq!(scan(&[b"\x1b[>s"]), Some(false));
        // Invalid params are ignored, keeping the previous value.
        assert_eq!(scan(&[b"\x1b[>1s", b"\x1b[>2s"]), Some(true));
        assert_eq!(scan(&[b"\x1b[>1;1s"]), None);
        // Split across reads.
        assert_eq!(scan(&[b"\x1b[", b">1", b"s"]), Some(true));
    }

    #[test]
    fn ignores_lookalikes_and_resets_on_ris() {
        // Plain `CSI s` is SCOSC (save cursor), not ours.
        assert_eq!(scan(&[b"\x1b[s"]), None);
        assert_eq!(scan(&[b"\x1b[?1s"]), None);
        assert_eq!(scan(&[b"\x1b[>1 s"]), None);
        assert_eq!(scan(&[b"\x1b[>1s", b"\x1bc"]), None);
    }
}
