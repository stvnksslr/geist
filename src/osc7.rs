//! OSC 7 working-directory tracking via a side scan of the PTY byte stream.
//!
//! A shell reports its current directory with `ESC ] 7 ; <file-uri> (BEL | ESC \\)`.
//! libghostty-vt parses OSC 7 but its read-only stream discards the payload (it
//! never reaches the terminal's `pwd`), so — exactly as with OSC 52 (see
//! [`crate::osc52`]) — we run a tiny streaming parser over the same bytes we feed
//! the engine and remember the latest directory. A new split then opens there.

/// Cap on a single OSC body so a malformed, never-terminated sequence can't grow
/// the buffer without bound (64 KiB is far above any real path).
const MAX_BODY: usize = 1 << 16;

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

/// Streaming OSC 7 scanner. Feed it PTY output (in arbitrary chunks); it keeps
/// the most recently reported working directory (the raw URI), which may span
/// multiple `feed` calls.
pub struct Osc7Scanner {
    state: State,
    buf: Vec<u8>,
    /// True once `buf` overflowed `MAX_BODY`, so the sequence is abandoned.
    overflowed: bool,
    /// The latest working directory reported via OSC 7 (raw URI), if any.
    pwd: Option<String>,
}

impl Osc7Scanner {
    pub fn new() -> Self {
        Self {
            state: State::Ground,
            buf: Vec::new(),
            overflowed: false,
            pwd: None,
        }
    }

    /// The most recent OSC 7 working directory (raw URI), if one was reported.
    pub fn pwd(&self) -> Option<&str> {
        self.pwd.as_deref()
    }

    /// Feed a chunk of PTY bytes. Updates [`Osc7Scanner::pwd`] when a complete
    /// OSC 7 sequence arrives. Sequences may span multiple calls.
    pub fn feed(&mut self, bytes: &[u8]) {
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
                    0x07 => self.finish(),
                    0x1b => self.state = State::BodyEsc,
                    _ => self.push(b),
                },
                State::BodyEsc => {
                    if b == b'\\' {
                        self.finish();
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

    fn finish(&mut self) {
        if !self.overflowed {
            if let Some(uri) = parse_osc7(&self.buf) {
                self.pwd = Some(uri);
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

/// Parse an OSC body (`7;<uri>`) into the reported working-directory URI, or
/// `None` if it isn't an OSC 7 with a non-empty payload.
fn parse_osc7(body: &[u8]) -> Option<String> {
    let s = std::str::from_utf8(body).ok()?;
    let uri = s.strip_prefix("7;")?;
    if uri.is_empty() {
        return None;
    }
    Some(uri.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scan(chunks: &[&[u8]]) -> Option<String> {
        let mut s = Osc7Scanner::new();
        for c in chunks {
            s.feed(c);
        }
        s.pwd().map(str::to_string)
    }

    #[test]
    fn captures_bel_terminated_pwd() {
        assert_eq!(
            scan(&[b"\x1b]7;file://HOST/C:/Users/foo\x07"]),
            Some("file://HOST/C:/Users/foo".to_string())
        );
    }

    #[test]
    fn captures_st_terminated_pwd() {
        assert_eq!(
            scan(&[b"\x1b]7;file:///C:/Users/foo\x1b\\"]),
            Some("file:///C:/Users/foo".to_string())
        );
    }

    #[test]
    fn handles_sequence_split_across_chunks() {
        assert_eq!(
            scan(&[b"out\x1b]7;file://h/C:", b"/Users/foo\x07more"]),
            Some("file://h/C:/Users/foo".to_string())
        );
    }

    #[test]
    fn keeps_latest_of_multiple_reports() {
        assert_eq!(
            scan(&[b"\x1b]7;file://h/C:/a\x07\x1b]7;file://h/C:/b\x07"]),
            Some("file://h/C:/b".to_string())
        );
    }

    #[test]
    fn ignores_other_osc_and_plain_text() {
        // OSC 0 (title), OSC 52 (clipboard) and ordinary bytes leave pwd unset.
        assert_eq!(scan(&[b"\x1b]0;title\x07\x1b]52;c;aGk=\x07hi\r\n"]), None);
    }
}
