//! OSC 52 clipboard handling via a side scan of the PTY byte stream.
//!
//! libghostty-vt parses OSC 52 but the Rust binding surfaces only the command
//! *type*, not its payload, and offers no callback — so to support programs
//! that copy to the clipboard (tmux, vim/nvim `+clipboard`, remote shells over
//! SSH) we run a tiny streaming parser over the same bytes we feed the engine,
//! decode the base64 payload, and write it to the system clipboard.
//!
//! The sequence is `ESC ] 52 ; <targets> ; <base64> (BEL | ESC \\)`. Both forms
//! are recognized: a "set" carries base64 to put on the clipboard, and a query
//! (`<base64>` == `?`) asks for it back. Whether either is honored is a
//! *policy* decision made by the caller from `clipboard-write` /
//! `clipboard-read` — this module only parses.

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

/// What a complete OSC 52 sequence asked for.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Osc52 {
    /// Put this text on the clipboard.
    Set(String),
    /// Send the clipboard back, echoing this target selection (`c`, `p`, `s`…).
    Query(String),
}

/// Streaming OSC 52 scanner. Feed it PTY output (in arbitrary chunks); it emits
/// a [`Osc52`] as each complete sequence arrives.
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

    /// Feed a chunk of bytes, appending any complete OSC 52 requests to `out`.
    /// Sequences may span multiple calls.
    pub fn feed(&mut self, bytes: &[u8], out: &mut Vec<Osc52>) {
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

    fn finish(&mut self, out: &mut Vec<Osc52>) {
        if !self.overflowed {
            if let Some(req) = parse_osc52(&self.buf) {
                out.push(req);
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

/// Parse an OSC body (`52;<targets>;<base64>`) into the request it makes, or
/// `None` if it isn't an OSC 52 we understand.
fn parse_osc52(body: &[u8]) -> Option<Osc52> {
    let s = std::str::from_utf8(body).ok()?;
    let rest = s.strip_prefix("52;")?;
    let (targets, data) = rest.split_once(';')?;
    if data.trim() == "?" {
        // The targets are kept (unlike the set form, which ignores them) purely
        // so a reply can echo the selection back verbatim, as xterm does.
        return Some(Osc52::Query(targets.to_string()));
    }
    let decoded = STANDARD.decode(data.trim()).ok()?;
    String::from_utf8(decoded).ok().map(Osc52::Set)
}

/// Build the reply to a clipboard query: `ESC ] 52 ; <targets> ; <base64> ESC \`.
///
/// Always ST-terminated. xterm accepts BEL too, but ST is the form the spec
/// gives and what the query itself is most often written with.
pub fn query_reply(targets: &str, text: &str) -> Vec<u8> {
    let mut out = Vec::with_capacity(text.len() * 4 / 3 + targets.len() + 8);
    out.extend_from_slice(b"\x1b]52;");
    out.extend_from_slice(targets.as_bytes());
    out.push(b';');
    out.extend_from_slice(STANDARD.encode(text).as_bytes());
    out.extend_from_slice(b"\x1b\\");
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scan(chunks: &[&[u8]]) -> Vec<Osc52> {
        let mut s = Osc52Scanner::new();
        let mut out = Vec::new();
        for c in chunks {
            s.feed(c, &mut out);
        }
        out
    }

    fn set(s: &str) -> Osc52 {
        Osc52::Set(s.to_string())
    }

    #[test]
    fn decodes_bel_terminated_set() {
        // "hi" base64 = "aGk=".
        assert_eq!(scan(&[b"\x1b]52;c;aGk=\x07"]), vec![set("hi")]);
    }

    #[test]
    fn decodes_st_terminated_set() {
        assert_eq!(scan(&[b"\x1b]52;c;aGk=\x1b\\"]), vec![set("hi")]);
    }

    #[test]
    fn handles_sequence_split_across_chunks() {
        let got = scan(&[b"prefix\x1b]52;", b"c;aG", b"k=\x07suffix"]);
        assert_eq!(got, vec![set("hi")]);
    }

    #[test]
    fn reports_clipboard_query_with_its_target() {
        // Recognized, not answered — `clipboard-read` decides that. The target
        // is kept so the reply can echo the selection the program asked for.
        assert_eq!(
            scan(&[b"\x1b]52;c;?\x07"]),
            vec![Osc52::Query("c".to_string())]
        );
        assert_eq!(
            scan(&[b"\x1b]52;p;?\x1b\\"]),
            vec![Osc52::Query("p".to_string())]
        );
        // An empty target list is legal and means the default selection.
        assert_eq!(
            scan(&[b"\x1b]52;;?\x07"]),
            vec![Osc52::Query(String::new())]
        );
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
        assert_eq!(scan(&[b"\x1b]52;p;aGk=\x07"]), vec![set("hi")]);
    }

    #[test]
    fn query_reply_round_trips_through_the_scanner() {
        // The reply we emit is itself a well-formed OSC 52 set, so feeding it
        // back yields the original text — which pins the base64 and framing.
        let reply = query_reply("c", "hi there");
        assert_eq!(reply, b"\x1b]52;c;aGkgdGhlcmU=\x1b\\");
        assert_eq!(scan(&[&reply]), vec![set("hi there")]);
    }
}
