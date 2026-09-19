//! Cursor-style / blink tracking via a side scan of the PTY byte stream.
//!
//! A program selects the cursor shape with `ESC [ Ps SP q` (DECSCUSR) and toggles
//! blink with `ESC [ ? 12 h` / `l` (DEC mode 12). giest's read-only libghostty-vt
//! stream applies both internally — its render snapshot reports the resulting
//! shape and blink — but it gives us no way to set the *default* shape (Ghostty's
//! `cursor-style`), it resets DECSCUSR-default (`Ps = 0`) to a hardcoded block
//! rather than the configured default, and its mode-12 default is off whereas
//! Ghostty's default cursor blinks. So, like the other side-scanners (e.g.
//! [`crate::osc_notify`]), we run a tiny CSI parser over the same
//! bytes we feed the engine and track:
//!
//! - whether the program is on its *default* cursor (initial, and after `Ps = 0`)
//!   — while it is, the session substitutes the configured `cursor-style`; once a
//!   program picks an explicit shape we defer to the engine;
//! - the default-cursor blink state — starting on (Ghostty's `... orelse true`),
//!   reset on `Ps = 0`, and toggled by DEC mode 12. The session consults this
//!   only when `cursor-style-blink` is unset, mirroring Ghostty, whose stream
//!   handler ignores mode 12 when `cursor-style-blink` is set.
//!
//! This mirrors Ghostty's `default_cursor` / mode-12 bookkeeping in its stream
//! handler (`termio/stream_handler.zig`).

/// Streaming cursor-style/blink scanner. Feed it PTY output (in arbitrary
/// chunks); it tracks the default-cursor state and the default-cursor blink.
pub struct DecscusrScanner {
    state: State,
    /// Accumulated numeric parameter (the first param of the current CSI).
    param: u32,
    /// Saw the single `SP` (0x20) intermediate that distinguishes DECSCUSR.
    space: bool,
    /// Saw the `?` private marker (a DEC private mode sequence, e.g. mode 12).
    private: bool,
    /// A parameter equal to 12 appeared before a `;` in this CSI (so a private
    /// `h`/`l` final toggles DEC mode 12 even in a multi-mode sequence).
    saw_mode12: bool,
    /// Cleared when an unexpected byte rules this CSI out as a DECSCUSR.
    decscusr: bool,
    /// True while the program is using its default cursor (so the configured
    /// `cursor-style` should apply). Starts true.
    default_cursor: bool,
    /// Default-cursor blink state: on by default (Ghostty), reset by `Ps = 0`,
    /// toggled by DEC mode 12. Consulted only when `cursor-style-blink` is unset.
    blink: bool,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum State {
    /// Outside any escape sequence.
    Ground,
    /// Saw ESC.
    Esc,
    /// Inside a CSI sequence (after `ESC [`), collecting until a final byte.
    Csi,
}

impl DecscusrScanner {
    pub fn new() -> Self {
        Self {
            state: State::Ground,
            param: 0,
            space: false,
            private: false,
            saw_mode12: false,
            decscusr: true,
            default_cursor: true,
            blink: true,
        }
    }

    /// Whether the program is currently on its default cursor, i.e. the
    /// configured `cursor-style` should be substituted.
    pub fn is_default(&self) -> bool {
        self.default_cursor
    }

    /// The default-cursor blink state (DEC mode 12, defaulting to on). The
    /// session uses this as the fallback when `cursor-style-blink` is unset.
    pub fn default_blink(&self) -> bool {
        self.blink
    }

    /// Feed a chunk of PTY bytes. Updates [`DecscusrScanner::is_default`] when a
    /// complete DECSCUSR arrives. Sequences may span multiple calls.
    pub fn feed(&mut self, bytes: &[u8]) {
        for &b in bytes {
            match self.state {
                State::Ground => {
                    if b == 0x1b {
                        self.state = State::Esc;
                    }
                }
                State::Esc => {
                    if b == b'[' {
                        self.begin_csi();
                    } else if b != 0x1b {
                        self.state = State::Ground;
                    }
                }
                State::Csi => self.csi_byte(b),
            }
        }
    }

    fn begin_csi(&mut self) {
        self.state = State::Csi;
        self.param = 0;
        self.space = false;
        self.private = false;
        self.saw_mode12 = false;
        self.decscusr = true;
    }

    fn csi_byte(&mut self, b: u8) {
        match b {
            // Final byte (0x40..=0x7E) ends the CSI.
            0x40..=0x7E => {
                if b == b'q' && self.space && self.decscusr {
                    self.apply_decscusr();
                } else if (b == b'h' || b == b'l')
                    && self.private
                    && !self.space
                    && (self.param == 12 || self.saw_mode12)
                {
                    // DEC mode 12 set (`h`) / reset (`l`): cursor blink on/off.
                    self.blink = b == b'h';
                }
                self.state = State::Ground;
            }
            // Parameter digit — only valid before the intermediate space.
            b'0'..=b'9' => {
                if self.space {
                    self.decscusr = false;
                } else {
                    self.param = self.param.saturating_mul(10).saturating_add((b - b'0') as u32);
                }
            }
            // Parameter separator: note a mode-12 param, then start the next one.
            // A `;` means this is not a bare DECSCUSR.
            b';' => {
                if self.param == 12 {
                    self.saw_mode12 = true;
                }
                self.param = 0;
                self.decscusr = false;
            }
            // The single SP intermediate DECSCUSR carries; a second one rules it out.
            0x20 => {
                if self.space {
                    self.decscusr = false;
                }
                self.space = true;
            }
            // The `?` private marker (DEC private modes like 12); not a DECSCUSR.
            b'?' => {
                self.private = true;
                self.decscusr = false;
            }
            // Any other parameter/intermediate byte rules out a bare DECSCUSR;
            // keep consuming until the final byte.
            _ => self.decscusr = false,
        }
    }

    /// Apply a parsed DECSCUSR `Ps`: `0` (or omitted) returns to the default
    /// cursor (and, like Ghostty, resets the default-cursor blink to on);
    /// `1..=6` selects an explicit shape (blink then tracked by the engine);
    /// other values are invalid and leave the state unchanged (matching Ghostty).
    fn apply_decscusr(&mut self) {
        match self.param {
            0 => {
                self.default_cursor = true;
                self.blink = true;
            }
            1..=6 => self.default_cursor = false,
            _ => {}
        }
    }
}

impl Default for DecscusrScanner {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn after(chunks: &[&[u8]]) -> bool {
        let mut s = DecscusrScanner::new();
        for c in chunks {
            s.feed(c);
        }
        s.is_default()
    }

    fn scan(chunks: &[&[u8]]) -> DecscusrScanner {
        let mut s = DecscusrScanner::new();
        for c in chunks {
            s.feed(c);
        }
        s
    }

    #[test]
    fn starts_on_default_cursor() {
        let s = DecscusrScanner::new();
        assert!(s.is_default());
        // Default-cursor blink starts ON (Ghostty's `... orelse true`).
        assert!(s.default_blink());
        // Plain output and unrelated escapes leave it on the default.
        assert!(after(&[b"hello\r\n\x1b[31mred\x1b[0m\x1b[2J"]));
    }

    #[test]
    fn dec_mode_12_toggles_default_blink() {
        // CSI ?12 l stops the default cursor blinking; CSI ?12 h restarts it.
        assert!(!scan(&[b"\x1b[?12l"]).default_blink());
        assert!(scan(&[b"\x1b[?12l", b"\x1b[?12h"]).default_blink());
        // Mode 12 toggles blink without leaving the default cursor.
        let s = scan(&[b"\x1b[?12l"]);
        assert!(s.is_default());
        // A multi-mode set that includes 12 is still honored.
        assert!(!scan(&[b"\x1b[?25;12l"]).default_blink());
        // An unrelated private mode (25) must not touch blink.
        assert!(scan(&[b"\x1b[?25l"]).default_blink());
    }

    #[test]
    fn decscusr_default_resets_blink_on() {
        // Program stops blink via mode 12, then DECSCUSR-default restores it
        // (Ghostty's setCursorStyle(.default) sets blink = ... orelse true).
        let s = scan(&[b"\x1b[?12l", b"\x1b[0 q"]);
        assert!(s.is_default());
        assert!(s.default_blink());
    }

    #[test]
    fn explicit_shape_clears_default() {
        // Steady bar (DECSCUSR 6).
        assert!(!after(&[b"\x1b[6 q"]));
        // Blinking block (1) and steady underline (4) too.
        assert!(!after(&[b"\x1b[1 q"]));
        assert!(!after(&[b"\x1b[4 q"]));
    }

    #[test]
    fn decscusr_zero_returns_to_default() {
        // An app sets a bar, then resets to default.
        assert!(after(&[b"\x1b[5 q", b"\x1b[0 q"]));
        // Omitted parameter (`ESC [ SP q`) is also the default.
        assert!(after(&[b"\x1b[5 q", b"\x1b[ q"]));
    }

    #[test]
    fn invalid_value_leaves_state_unchanged() {
        // 7 is out of range: an app on an explicit shape stays explicit.
        assert!(!after(&[b"\x1b[2 q", b"\x1b[7 q"]));
    }

    #[test]
    fn similar_csi_sequences_do_not_trigger() {
        // No space intermediate: a plain `q` final (DECSCA-ish) is not DECSCUSR.
        assert!(after(&[b"\x1b[1q"]));
        // Multi-param with a final `q` is not DECSCUSR.
        assert!(after(&[b"\x1b[1;2q"]));
        // A common SGR ends in `m`, not `q`.
        assert!(after(&[b"\x1b[1m"]));
    }

    #[test]
    fn sequence_split_across_chunks() {
        // The DECSCUSR straddles two feeds.
        assert!(!after(&[b"out\x1b[6", b" qmore"]));
    }
}
