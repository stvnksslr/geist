//! OSC 52 clipboard handling via a side scan of the PTY byte stream.
//!
//! libghostty-vt parses OSC 52 but the Rust binding surfaces only the command
//! *type*, not its payload, and offers no callback — so to support programs
//! that copy to the clipboard (tmux, vim/nvim `+clipboard`, remote shells over
//! SSH) we run a tiny streaming parser over the same bytes we feed the engine,
//! decode the base64 payload, and write it to the system clipboard.
//!
//! The sequence is `ESC ] 52 ; <targets> ; <base64> (BEL | ESC \\)`. Only the
//! "set" form is handled; a query (`<base64>` == `?`) is ignored so terminal
//! output can't read the clipboard back.

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD;

/// Cap on a single OSC body so a malformed, never-terminated sequence can't
/// grow the buffer without bound (1 MiB is far above any real clipboard write).
const MAX_BODY: usize = 1 << 20;

#[derive(Clone, Copy, PartialEq, Eq)]
enum State {
    /// Outside any escape sequence.
    Ground,
    /// Saw ESC.
    Esc,
    /// Inside an OSC body (after `ESC ]`).
    Body,
    /// Saw ESC inside the body — expecting `\\` to form the ST terminator.
    BodyEsc,
}

/// Streaming OSC 52 scanner. Feed it PTY output (in arbitrary chunks); it emits
/// decoded clipboard-set strings as complete sequences arrive.
pub struct Osc52Scanner {
    state: State,
    buf: Vec<u8>,
    /// True once `buf` overflowed `MAX_BODY`, so the sequence is abandoned.
    overflowed: bool,
}

impl Osc52Scanner {
    pub fn new() -> Self {
        Self {
            state: State::Ground,
            buf: Vec::new(),
            overflowed: false,
        }
    }

    /// Feed a chunk of bytes, appending any decoded clipboard-set payloads to
    /// `out`. Sequences may span multiple calls.
    pub fn feed(&mut self, bytes: &[u8], out: &mut Vec<String>) {
        for &b in bytes {
            match self.state {
                State::Ground => {
                    if b == 0x1b {
                        self.state = State::Esc;
                    }
                }
                State::Esc => {
                    if b == b']' {
                        self.state = State::Body;
                        self.buf.clear();
                        self.overflowed = false;
                    } else if b != 0x1b {
                        self.state = State::Ground;
                    }
                }
                State::Body => match b {
                    0x07 => self.finish(out),
                    0x1b => self.state = State::BodyEsc,
                    _ => self.push(b),
                },
                State::BodyEsc => {
                    if b == b'\\' {
                        self.finish(out);
                    } else if b != 0x1b {
                        // Not an ST; this OSC is malformed — drop it.
                        self.reset();
                    }
                }
            }
        }
    }

    fn push(&mut self, b: u8) {
        if self.buf.len() < MAX_BODY {
            self.buf.push(b);
        } else {
            self.overflowed = true;
        }
    }

    fn finish(&mut self, out: &mut Vec<String>) {
        if !self.overflowed {
            if let Some(text) = parse_osc52(&self.buf) {
                out.push(text);
            }
        }
        self.reset();
    }

    fn reset(&mut self) {
        self.state = State::Ground;
        self.buf.clear();
        self.overflowed = false;
    }
}

/// Parse an OSC body (`52;<targets>;<base64>`) into the clipboard text to set,
/// or `None` if it isn't an OSC 52 set we handle.
fn parse_osc52(body: &[u8]) -> Option<String> {
    let s = std::str::from_utf8(body).ok()?;
    let rest = s.strip_prefix("52;")?;
    let (_targets, data) = rest.split_once(';')?;
    if data == "?" {
        return None; // clipboard query — not answered (don't leak the clipboard)
    }
    let decoded = STANDARD.decode(data.trim()).ok()?;
    String::from_utf8(decoded).ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scan(chunks: &[&[u8]]) -> Vec<String> {
        let mut s = Osc52Scanner::new();
        let mut out = Vec::new();
        for c in chunks {
            s.feed(c, &mut out);
        }
        out
    }

    #[test]
    fn decodes_bel_terminated_set() {
        // "hi" base64 = "aGk=".
        let got = scan(&[b"\x1b]52;c;aGk=\x07"]);
        assert_eq!(got, vec!["hi".to_string()]);
    }

    #[test]
    fn decodes_st_terminated_set() {
        let got = scan(&[b"\x1b]52;c;aGk=\x1b\\"]);
        assert_eq!(got, vec!["hi".to_string()]);
    }

    #[test]
    fn handles_sequence_split_across_chunks() {
        let got = scan(&[b"prefix\x1b]52;", b"c;aG", b"k=\x07suffix"]);
        assert_eq!(got, vec!["hi".to_string()]);
    }

    #[test]
    fn ignores_clipboard_query() {
        assert!(scan(&[b"\x1b]52;c;?\x07"]).is_empty());
    }

    #[test]
    fn ignores_other_osc_and_plain_text() {
        // OSC 0 (title) and ordinary bytes produce nothing.
        let got = scan(&[b"\x1b]0;my title\x07hello world\r\n"]);
        assert!(got.is_empty());
    }

    #[test]
    fn primary_target_also_decodes() {
        // Some apps target primary ("p") rather than clipboard ("c").
        let got = scan(&[b"\x1b]52;p;aGk=\x07"]);
        assert_eq!(got, vec!["hi".to_string()]);
    }
}
