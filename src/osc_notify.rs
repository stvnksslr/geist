//! Desktop notifications (`OSC 9`, `OSC 777`) via a side scan of the PTY stream.
//!
//! A program asks the terminal to raise a notification with either the iTerm2
//! form `ESC ] 9 ; <body> (BEL | ST)` or the rxvt form
//! `ESC ] 777 ; notify ; <title> ; <body> (BEL | ST)`. libghostty-vt parses both,
//! but its *read-only* stream drops the payload before it reaches anything we
//! can read — the same hole as OSC 7 and OSC 52 — so, as in [`crate::osc7`] and
//! [`crate::osc52`], we run a small streaming parser over the same bytes we feed
//! the engine.
//!
//! The subtle half is OSC 9, which is **overloaded**: ConEmu claims `9;1`
//! through `9;9` for a dozen unrelated commands (sleep, progress bars, tab
//! titles, cwd reports), and only a payload that does *not* match one of those
//! is a notification. [`parse_body`] mirrors Ghostty's disambiguation
//! (`terminal/osc/parsers/osc9.zig`) branch for branch, including its habit of
//! falling *through* to "notification" whenever a ConEmu shape is started but
//! malformed.

/// Cap on a single OSC body so a malformed, never-terminated sequence can't grow
/// the buffer without bound. Notification bodies are prose, so 64 KiB is far
/// above anything real.
const MAX_BODY: usize = 1 << 16;

/// Something an `OSC 9` / `OSC 777` sequence asked the terminal to do.
///
/// Both live here rather than in separate scanners because they come out of the
/// *same* overloaded namespace: telling `9;4;1;50` (a progress report) from
/// `9;4 tests passed` (a notification) is one decision, and splitting it across
/// two parsers would mean two copies of [`parse_osc9`] free to drift apart.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Osc9 {
    Notify(Notification),
    Progress(ProgressReport),
}

/// A requested desktop notification. `title` is empty for the OSC 9 form, which
/// carries only a body.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Notification {
    pub title: String,
    pub body: String,
}

/// ConEmu's `OSC 9;4` progress report — what a long-running program uses to
/// drive a progress bar. Ghostty models it as a state plus an optional
/// percentage, and so do we.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ProgressReport {
    pub state: ProgressState,
    /// Percent complete, `0..=100`. Absent for states that don't carry one.
    pub value: Option<u8>,
}

/// The five states ConEmu defines for `OSC 9;4`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ProgressState {
    /// `9;4;0` — no progress; clear any indicator.
    Remove,
    /// `9;4;1;<pct>` — a determinate percentage.
    Set,
    /// `9;4;2[;<pct>]` — something went wrong; the bar turns red.
    Error,
    /// `9;4;3` — busy, but the total isn't known.
    Indeterminate,
    /// `9;4;4[;<pct>]` — paused//waiting; the bar turns yellow.
    Pause,
}

impl Notification {
    /// Whether there is anything to show. Ghostty's parser happily produces an
    /// entirely empty notification (`ESC ] 9 ; ST`); raising a blank toast for
    /// it would just be noise.
    pub fn is_empty(&self) -> bool {
        self.title.is_empty() && self.body.is_empty()
    }
}

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

/// Streaming OSC 9 / OSC 777 scanner. Feed it PTY output in arbitrary chunks;
/// each complete notification request is appended to the caller's vector.
pub struct OscNotifyScanner {
    state: State,
    buf: Vec<u8>,
    /// True once `buf` overflowed [`MAX_BODY`], so the sequence is abandoned.
    overflowed: bool,
}

impl Default for OscNotifyScanner {
    fn default() -> Self {
        Self::new()
    }
}

impl OscNotifyScanner {
    pub fn new() -> Self {
        Self {
            state: State::Ground,
            buf: Vec::new(),
            overflowed: false,
        }
    }

    /// Feed a chunk of PTY bytes, appending any completed requests to `out`.
    /// Sequences may span calls.
    pub fn feed(&mut self, bytes: &[u8], out: &mut Vec<Osc9>) {
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

    fn finish(&mut self, out: &mut Vec<Osc9>) {
        if !self.overflowed
            && let Some(n) = parse_body(&self.buf)
        {
            out.push(n);
        }
        self.reset();
    }

    fn reset(&mut self) {
        self.state = State::Ground;
        self.buf.clear();
        self.overflowed = false;
    }
}

/// Parse an OSC body (everything between `ESC ]` and the terminator) into a
/// request, or `None` if it isn't one of ours.
fn parse_body(body: &[u8]) -> Option<Osc9> {
    // Notification text is prose and can be any UTF-8; a body that isn't valid
    // UTF-8 can't be displayed, so reject rather than lossily mangle it.
    let s = std::str::from_utf8(body).ok()?;
    if let Some(rest) = s.strip_prefix("777;") {
        return parse_777(rest).map(Osc9::Notify);
    }
    parse_osc9(s.strip_prefix("9;")?)
}

/// `OSC 777 ; notify ; <title> ; <body>`. The extension name must be exactly
/// `notify`, and the title separator must be present — Ghostty rejects the
/// sequence outright when it isn't, rather than treating the whole tail as a
/// body.
fn parse_777(rest: &str) -> Option<Notification> {
    let (ext, tail) = rest.split_once(';')?;
    if ext != "notify" {
        return None;
    }
    // The body may itself contain `;`, so split only on the *first* one.
    let (title, body) = tail.split_once(';')?;
    Some(Notification {
        title: title.to_string(),
        body: body.to_string(),
    })
}

/// Parse an OSC 9 payload (everything after `9;`).
///
/// `OSC 9` is overloaded: ConEmu claims `9;1` through `9;9` for a dozen
/// unrelated commands, and only a payload that matches none of them is an
/// iTerm2 notification. This mirrors Ghostty's `osc9.zig`, whose *structure*
/// matters: it tests a prefix and, whenever the rest of the expected shape
/// doesn't hold, breaks out of the ConEmu block and falls through to
/// "notification". So `9;4;1;50` is a progress report while `9;4` alone is a
/// notification whose body is the text `4`. That is faithful, not an accident —
/// do not "tidy" it into a leading-digit test, which would swallow every message
/// beginning with a digit.
///
/// Returns `None` for a ConEmu command we don't act on (it was still consumed,
/// and must *not* fall through to being shown as a notification).
fn parse_osc9(data: &str) -> Option<Osc9> {
    let b = data.as_bytes();
    let at = |i: usize| b.get(i).copied();
    let notification = || {
        Some(Osc9::Notify(Notification {
            title: String::new(),
            body: data.to_string(),
        }))
    };

    // `ESC ] 9 ; ST` — an empty notification, not a ConEmu command.
    let Some(&first) = b.first() else {
        return notification();
    };
    let consumed = match first {
        b'1' => match at(1) {
            // 9;1;<ms> sleep
            Some(b';') => true,
            // 9;10 xterm emulation, optionally `;0`..`;3`
            Some(b'0') => b.len() == 2 || (at(2) == Some(b';') && matches!(at(3), Some(b'0'..=b'3'))),
            // 9;11;<text> comment
            Some(b'1') => at(2) == Some(b';'),
            // 9;12 mark prompt start — accepted with no further checks upstream,
            // so `9;12anything` is a ConEmu command there too.
            Some(b'2') => true,
            _ => false,
        },
        // 9;2 message box, 9;3 tab title, 9;6 guimacro, 9;7 run process,
        // 9;8 environment variable, 9;9 working directory.
        b'2' | b'3' | b'6' | b'7' | b'8' | b'9' => at(1) == Some(b';'),
        // 9;4;<state>[;<pct>] progress report — the one ConEmu command giest
        // acts on. The state digit is required; without it this is prose.
        b'4' => {
            if at(1) == Some(b';')
                && let Some(state) = at(2)
                && (b'0'..=b'4').contains(&state)
            {
                return Some(Osc9::Progress(parse_progress(state, &data[3..])));
            }
            false
        }
        // 9;5 wait for input — no payload at all.
        b'5' => true,
        _ => false,
    };
    if consumed { None } else { notification() }
}

/// Build a progress report from its state digit and whatever follows it
/// (`";50"`, or empty).
///
/// Ghostty seeds `set` with `0` and leaves the others' percentage absent unless
/// one is given, so a bare `9;4;1` is "0%" while a bare `9;4;2` is "failed, no
/// percentage". An unparseable or out-of-range percentage is clamped/ignored
/// rather than rejected — the sequence still carries a usable state.
fn parse_progress(state: u8, tail: &str) -> ProgressReport {
    let state = match state {
        b'0' => ProgressState::Remove,
        b'1' => ProgressState::Set,
        b'2' => ProgressState::Error,
        b'3' => ProgressState::Indeterminate,
        _ => ProgressState::Pause,
    };
    // Only these three carry a percentage upstream; `remove` and
    // `indeterminate` have nothing to show one for.
    let mut value = match state {
        ProgressState::Set => Some(0),
        _ => None,
    };
    if matches!(
        state,
        ProgressState::Set | ProgressState::Error | ProgressState::Pause
    ) && let Some(pct) = tail.strip_prefix(';')
        && let Ok(n) = pct.parse::<u32>()
    {
        value = Some(n.min(100) as u8);
    }
    ProgressReport { state, value }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every event a stream produced.
    fn scan_all(chunks: &[&[u8]]) -> Vec<Osc9> {
        let mut s = OscNotifyScanner::new();
        let mut out = Vec::new();
        for c in chunks {
            s.feed(c, &mut out);
        }
        out
    }

    /// Just the notifications, which most of these tests are about.
    fn scan(chunks: &[&[u8]]) -> Vec<Notification> {
        scan_all(chunks)
            .into_iter()
            .filter_map(|e| match e {
                Osc9::Notify(n) => Some(n),
                Osc9::Progress(_) => None,
            })
            .collect()
    }

    fn progress(chunks: &[&[u8]]) -> Vec<ProgressReport> {
        scan_all(chunks)
            .into_iter()
            .filter_map(|e| match e {
                Osc9::Progress(p) => Some(p),
                Osc9::Notify(_) => None,
            })
            .collect()
    }

    fn note(title: &str, body: &str) -> Notification {
        Notification {
            title: title.to_string(),
            body: body.to_string(),
        }
    }

    #[test]
    fn osc9_is_a_body_only_notification() {
        // Ghostty's own test case, verbatim.
        assert_eq!(scan(&[b"\x1b]9;Hello world\x07"]), vec![note("", "Hello world")]);
        // ST-terminated too.
        assert_eq!(scan(&[b"\x1b]9;Build done\x1b\\"]), vec![note("", "Build done")]);
    }

    #[test]
    fn osc777_carries_a_title_and_a_body() {
        // Ghostty's own test case, verbatim.
        assert_eq!(scan(&[b"\x1b]777;notify;Title;Body\x07"]), vec![note("Title", "Body")]);
        // A body may contain semicolons; only the first two split.
        assert_eq!(
            scan(&[b"\x1b]777;notify;make;done; exit 0; 3m12s\x07"]),
            vec![note("make", "done; exit 0; 3m12s")]
        );
        // An empty title is legal.
        assert_eq!(scan(&[b"\x1b]777;notify;;body\x07"]), vec![note("", "body")]);
    }

    #[test]
    fn osc777_needs_the_notify_extension_and_a_title_field() {
        // A different rxvt extension is not ours.
        assert!(scan(&[b"\x1b]777;precmd;x;y\x07"]).is_empty());
        // Missing the title separator: Ghostty rejects rather than treating the
        // whole tail as the body.
        assert!(scan(&[b"\x1b]777;notify;JustOneField\x07"]).is_empty());
        assert!(scan(&[b"\x1b]777;notify\x07"]).is_empty());
    }

    #[test]
    fn conemu_osc9_commands_are_not_notifications() {
        for seq in [
            &b"\x1b]9;1;500\x07"[..],       // sleep
            b"\x1b]9;10\x07",               // xterm emulation, bare
            b"\x1b]9;10;2\x07",             // xterm emulation, with a mode
            b"\x1b]9;11;a comment\x07",     // comment
            b"\x1b]9;12\x07",               // mark prompt start
            b"\x1b]9;2;a message box\x07",  // message box
            b"\x1b]9;3;tab title\x07",      // change tab title
            b"\x1b]9;4;1;40\x07",           // progress report
            b"\x1b]9;4;0\x07",              // progress: remove
            b"\x1b]9;5\x07",                // wait for input
            b"\x1b]9;6;macro\x07",          // guimacro
            b"\x1b]9;7;proc\x07",           // run process
            b"\x1b]9;8;VAR\x07",            // output env var
            b"\x1b]9;9;C:/Users/foo\x07",   // working directory
        ] {
            assert!(
                scan(&[seq]).is_empty(),
                "treated a ConEmu OSC 9 as a notification: {:?}",
                String::from_utf8_lossy(seq)
            );
        }
    }

    #[test]
    fn progress_reports_carry_a_state_and_an_optional_percentage() {
        let p = |s: &[u8]| progress(&[s]);
        let r = |state, value| ProgressReport { state, value };

        assert_eq!(p(b"\x1b]9;4;0\x1b\\"), vec![r(ProgressState::Remove, None)]);
        assert_eq!(p(b"\x1b]9;4;1;50\x1b\\"), vec![r(ProgressState::Set, Some(50))]);
        assert_eq!(p(b"\x1b]9;4;2;80\x1b\\"), vec![r(ProgressState::Error, Some(80))]);
        assert_eq!(p(b"\x1b]9;4;3\x1b\\"), vec![r(ProgressState::Indeterminate, None)]);
        assert_eq!(p(b"\x1b]9;4;4;10\x1b\\"), vec![r(ProgressState::Pause, Some(10))]);

        // Ghostty seeds `set` with 0 but leaves the others absent, so a bare
        // `9;4;1` is "0%" while a bare `9;4;2` is "failed, percentage unknown".
        assert_eq!(p(b"\x1b]9;4;1\x1b\\"), vec![r(ProgressState::Set, Some(0))]);
        assert_eq!(p(b"\x1b]9;4;2\x1b\\"), vec![r(ProgressState::Error, None)]);
        assert_eq!(p(b"\x1b]9;4;4\x1b\\"), vec![r(ProgressState::Pause, None)]);

        // Out of range clamps; unparseable keeps the state and drops the value.
        assert_eq!(p(b"\x1b]9;4;1;250\x1b\\"), vec![r(ProgressState::Set, Some(100))]);
        assert_eq!(p(b"\x1b]9;4;1;abc\x1b\\"), vec![r(ProgressState::Set, Some(0))]);
        // States that carry no percentage ignore one that's sent anyway.
        assert_eq!(p(b"\x1b]9;4;3;40\x1b\\"), vec![r(ProgressState::Indeterminate, None)]);
    }

    #[test]
    fn a_progress_report_is_never_also_a_notification() {
        // The two share OSC 9, so a report must be *consumed*, not shown as a
        // toast reading "4;1;50".
        let all = scan_all(&[b"\x1b]9;4;1;50\x1b\\"]);
        assert_eq!(all.len(), 1);
        assert!(matches!(all[0], Osc9::Progress(_)));
        // …and a malformed one still falls through to prose, as upstream does.
        assert_eq!(scan(&[b"\x1b]9;4;9\x1b\\"]), vec![note("", "4;9")]);
        assert!(progress(&[b"\x1b]9;4;9\x1b\\"]).is_empty());
    }

    #[test]
    fn a_started_but_malformed_conemu_shape_falls_through_to_a_notification() {
        // This is upstream's structure, not a quirk of ours: `osc9.zig` breaks
        // out of the ConEmu block on a bad shape and the fall-through is the
        // notification path. So a body that merely *starts* like a ConEmu
        // command is still shown.
        assert_eq!(scan(&[b"\x1b]9;4\x07"]), vec![note("", "4")]);
        assert_eq!(scan(&[b"\x1b]9;4;9\x07"]), vec![note("", "4;9")], "state digit out of range");
        assert_eq!(scan(&[b"\x1b]9;1\x07"]), vec![note("", "1")]);
        assert_eq!(scan(&[b"\x1b]9;13\x07"]), vec![note("", "13")]);
        assert_eq!(scan(&[b"\x1b]9;2x\x07"]), vec![note("", "2x")]);
        // A digit ConEmu never claims.
        assert_eq!(scan(&[b"\x1b]9;0;zero\x07"]), vec![note("", "0;zero")]);
        assert_eq!(scan(&[b"\x1b]9;42 tests failed\x1b\\"]), vec![note("", "42 tests failed")]);
        // …but `9;5` is ConEmu's "wait for input" with *no* payload and no
        // further checks, so a message starting with a bare `5` is swallowed.
        // That's upstream's behaviour too — a real overload collision in the
        // protocol, not something we can fix on this side.
        assert!(scan(&[b"\x1b]9;5 tests failed\x1b\\"]).is_empty());
    }

    #[test]
    fn empty_osc9_parses_but_has_nothing_to_show() {
        // Faithful to Ghostty (which produces a notification with two empty
        // strings); the display side is what drops it.
        let got = scan(&[b"\x1b]9;\x07"]);
        assert_eq!(got, vec![note("", "")]);
        assert!(got[0].is_empty());
        assert!(!note("", "x").is_empty());
        assert!(!note("x", "").is_empty());
    }

    #[test]
    fn handles_sequences_split_across_chunks() {
        assert_eq!(
            scan(&[b"out\x1b]777;notify;Ti", b"tle;Bo", b"dy\x07more"]),
            vec![note("Title", "Body")]
        );
    }

    #[test]
    fn collects_several_and_ignores_unrelated_osc() {
        assert_eq!(
            scan(&[b"\x1b]0;title\x07\x1b]9;one\x07\x1b]52;c;aGk=\x07\x1b]9;two\x07\x1b]7;file://h/C:/\x07"]),
            vec![note("", "one"), note("", "two")]
        );
    }

    #[test]
    fn an_oversized_body_is_dropped_not_buffered_forever() {
        let mut seq = b"\x1b]9;".to_vec();
        seq.extend(std::iter::repeat_n(b'x', MAX_BODY + 10));
        seq.push(0x07);
        assert!(scan(&[&seq]).is_empty());
        // …and the scanner recovers for the next sequence.
        let mut s = OscNotifyScanner::new();
        let mut out = Vec::new();
        s.feed(&seq, &mut out);
        s.feed(b"\x1b]9;after\x07", &mut out);
        assert_eq!(out, vec![Osc9::Notify(note("", "after"))]);
    }

    #[test]
    fn invalid_utf8_is_rejected_rather_than_mangled() {
        assert!(scan(&[b"\x1b]9;\xff\xfe\x07"]).is_empty());
    }

    #[test]
    fn a_malformed_escape_inside_the_body_aborts_the_sequence() {
        // ESC followed by something that isn't `\` is not an ST.
        assert!(scan(&[b"\x1b]9;body\x1bZ\x07"]).is_empty());
    }
}
