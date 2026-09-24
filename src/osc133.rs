//! OSC 133 command marks (`C` / `D`) via a side scan of the PTY stream.
//!
//! geist already gets *prompt* marks (`A`/`B`) for free: libghostty-vt applies
//! them to the screen, which is what `cursor_at_prompt` and `jump_to_prompt`
//! read. But the **command** marks carry information that never lands on a cell
//! — `C` says the command began, and `D` carries its exit code — so, as with
//! OSC 7 / 52 / 9, we run a small parser over the same bytes we feed the engine.
//!
//! This is what `notify-on-command-finish` runs on.

/// Cap on a single OSC body, so a malformed never-terminated sequence can't grow
/// the buffer without bound. These bodies are a letter and a few short options.
const MAX_BODY: usize = 4096;

/// A command-level OSC 133 mark.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Mark {
    /// `OSC 133 ; C` — end of input, start of output: the command is running.
    CommandStart,
    /// `OSC 133 ; D [; <exit-code>]` — the command finished. The code is absent
    /// when the shell didn't report one (or reported something unparseable,
    /// which the spec says to ignore rather than reject).
    CommandEnd { exit_code: Option<i32> },
    /// `OSC 133 ; A` carrying a `click_events=` or `cl=` option: how the shell
    /// wants prompt clicks handled (`cursor-click-to-move`). The engine applies
    /// the mark but keeps this to itself, so it is read here.
    PromptClick(crate::prompt_click::ClickMode),
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum State {
    Ground,
    Esc,
    Body,
    BodyEsc,
}

/// Streaming OSC 133 `C`/`D` scanner. Feed it PTY output in arbitrary chunks.
pub struct Osc133Scanner {
    state: State,
    buf: Vec<u8>,
    overflowed: bool,
}

impl Default for Osc133Scanner {
    fn default() -> Self {
        Self::new()
    }
}

impl Osc133Scanner {
    pub fn new() -> Self {
        Self {
            state: State::Ground,
            buf: Vec::new(),
            overflowed: false,
        }
    }

    /// Feed a chunk of PTY bytes, appending any completed marks to `out`.
    /// Sequences may span calls.
    pub fn feed(&mut self, bytes: &[u8], out: &mut Vec<Mark>) {
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

    fn finish(&mut self, out: &mut Vec<Mark>) {
        if !self.overflowed
            && let Some(m) = parse_body(&self.buf)
        {
            out.push(m);
        }
        self.reset();
    }

    fn reset(&mut self) {
        self.state = State::Ground;
        self.buf.clear();
        self.overflowed = false;
    }
}

/// Parse an OSC body into a command mark, or `None` for anything else —
/// including the `A`/`B`/`L`/`N`/`P`/`I` prompt marks, which the engine already
/// applies to the screen and which nothing here needs.
fn parse_body(body: &[u8]) -> Option<Mark> {
    // Options can carry an arbitrary command line (`cmdline=`), so a body that
    // isn't UTF-8 is possible; we only read the leading letter and an integer,
    // so work in bytes and don't reject on encoding.
    let rest = body.strip_prefix(b"133;")?;
    let (&action, tail) = rest.split_first()?;
    // `133;C` and `133;Cx` are different things: options are `;`-separated, so
    // anything else directly after the letter is not this command.
    if !tail.is_empty() && tail[0] != b';' {
        return None;
    }
    match action {
        b'C' => Some(Mark::CommandStart),
        b'D' => Some(Mark::CommandEnd {
            exit_code: exit_code(tail),
        }),
        b'A' => {
            let opts = std::str::from_utf8(tail.strip_prefix(b";")?).ok()?;
            crate::prompt_click::mode_from_options(opts).map(Mark::PromptClick)
        }
        _ => None,
    }
}

/// The exit code from a `D` mark's option string (`tail` still has its leading
/// `;`, or is empty).
///
/// Ghostty special-cases this: the exit code is the **first** option and is
/// *positional*, not `key=value`, so `133;D;1` is exit 1 while `133;D;aid=7`
/// has no exit code at all. A malformed value yields `None` rather than an
/// error — the spec says to ignore options it can't understand.
fn exit_code(tail: &[u8]) -> Option<i32> {
    let opts = tail.strip_prefix(b";")?;
    let first = match opts.iter().position(|&b| b == b';') {
        Some(i) => &opts[..i],
        None => opts,
    };
    std::str::from_utf8(first).ok()?.parse::<i32>().ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scan(chunks: &[&[u8]]) -> Vec<Mark> {
        let mut s = Osc133Scanner::new();
        let mut out = Vec::new();
        for c in chunks {
            s.feed(c, &mut out);
        }
        out
    }

    fn end(code: Option<i32>) -> Mark {
        Mark::CommandEnd { exit_code: code }
    }

    #[test]
    fn command_start_and_end() {
        assert_eq!(scan(&[b"\x1b]133;C\x1b\\"]), vec![Mark::CommandStart]);
        assert_eq!(scan(&[b"\x1b]133;D\x07"]), vec![end(None)]);
        assert_eq!(scan(&[b"\x1b]133;D;0\x1b\\"]), vec![end(Some(0))]);
        assert_eq!(scan(&[b"\x1b]133;D;1\x1b\\"]), vec![end(Some(1))]);
        // Shells report signals and .NET exceptions as large or negative codes.
        assert_eq!(
            scan(&[b"\x1b]133;D;-1073741510\x1b\\"]),
            vec![end(Some(-1073741510))]
        );
    }

    #[test]
    fn prompt_start_options_select_the_click_mode() {
        use crate::prompt_click::ClickMode;
        assert_eq!(
            scan(&[b"\x1b]133;A;cl=line\x1b\\"]),
            vec![Mark::PromptClick(ClickMode::Arrows)]
        );
        assert_eq!(
            scan(&[b"\x1b]133;A;aid=9;click_events=1\x07"]),
            vec![Mark::PromptClick(ClickMode::ClickEvents {
                relative: false
            })]
        );
        // Options only count on A.
        assert!(scan(&[b"\x1b]133;B;cl=line\x1b\\"]).is_empty());
    }

    #[test]
    fn prompt_marks_are_not_command_marks() {
        // A/B are applied by the engine itself; this scanner must ignore them,
        // and every other semantic-prompt letter too.
        for body in [
            &b"\x1b]133;A\x1b\\"[..],
            b"\x1b]133;B\x1b\\",
            b"\x1b]133;L\x1b\\",
            b"\x1b]133;N\x1b\\",
            b"\x1b]133;P;k=i\x1b\\",
            b"\x1b]133;I\x1b\\",
        ] {
            assert!(
                scan(&[body]).is_empty(),
                "{:?}",
                String::from_utf8_lossy(body)
            );
        }
        // …and so are other OSCs that merely start with the same digits.
        assert!(scan(&[b"\x1b]1337;File=x\x07"]).is_empty());
        assert!(scan(&[b"\x1b]13;C\x07"]).is_empty());
    }

    #[test]
    fn the_exit_code_is_positional_not_keyed() {
        // Ghostty special-cases the exit code as the *first* option, read
        // without a `key=`. So a keyed first option means "no exit code".
        assert_eq!(scan(&[b"\x1b]133;D;aid=7\x1b\\"]), vec![end(None)]);
        // …and a positional code is still found with options after it.
        assert_eq!(scan(&[b"\x1b]133;D;3;aid=7\x1b\\"]), vec![end(Some(3))]);
        // Unparseable is ignored, not fatal.
        assert_eq!(scan(&[b"\x1b]133;D;oops\x1b\\"]), vec![end(None)]);
        assert_eq!(scan(&[b"\x1b]133;D;\x1b\\"]), vec![end(None)]);
    }

    #[test]
    fn options_after_the_letter_need_a_separator() {
        // `Dx` is not `D` with options — the letter is a whole field.
        assert!(scan(&[b"\x1b]133;Done\x1b\\"]).is_empty());
        assert!(scan(&[b"\x1b]133;Custom\x1b\\"]).is_empty());
        // `C` with options is still C.
        assert_eq!(
            scan(&[b"\x1b]133;C;cmdline=ls\x1b\\"]),
            vec![Mark::CommandStart]
        );
    }

    #[test]
    fn a_full_command_cycle_in_one_stream() {
        // What a real shell emits around one command: prompt, input, run, done.
        assert_eq!(
            scan(&[b"\x1b]133;A\x1b\\PS C:\\> \x1b]133;B\x1b\\ls\r\n\x1b]133;C\x1b\\out\r\n\x1b]133;D;0\x1b\\"]),
            vec![Mark::CommandStart, end(Some(0))]
        );
    }

    #[test]
    fn handles_sequences_split_across_chunks() {
        assert_eq!(
            scan(&[b"\x1b]133;D", b";4", b"2\x1b\\"]),
            vec![end(Some(42))]
        );
    }

    #[test]
    fn an_oversized_body_is_dropped_and_the_scanner_recovers() {
        let mut seq = b"\x1b]133;D;".to_vec();
        seq.extend(std::iter::repeat_n(b'9', MAX_BODY + 10));
        seq.push(0x07);
        let mut s = Osc133Scanner::new();
        let mut out = Vec::new();
        s.feed(&seq, &mut out);
        assert!(out.is_empty());
        s.feed(b"\x1b]133;D;0\x07", &mut out);
        assert_eq!(out, vec![end(Some(0))]);
    }
}
