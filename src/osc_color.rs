//! OSC dynamic-color **queries** via a side scan of the PTY byte stream.
//!
//! libghostty-vt already applies the *set* and *reset* forms of OSC 10/11/12
//! (foreground / background / cursor) internally, and giest's snapshot reads the
//! resulting *effective* colors — so those need nothing here (there are engine
//! tests pinning that). What its read-only stream drops is the **query** form,
//! `OSC 10 ; ? ST`, and the binding exposes no color-report callback. So we scan
//! the same bytes we feed the engine and answer the query ourselves.
//!
//! **Answering is deliberate, and safe** — unlike the OSC 52 clipboard *read*,
//! which `osc52.rs` intentionally leaves unanswered because it would let terminal
//! output exfiltrate the clipboard. A color report leaks nothing, and programs
//! genuinely depend on it: vim, delta and bat query OSC 11 to decide whether the
//! background is light or dark. Don't "fix" this by analogy with OSC 52.
//!
//! The sequence is `ESC ] <code> ; <field> [; <field>…] (BEL | ESC \\)`, where a
//! field of `?` is a query. Ghostty echoes back whichever terminator the caller
//! used, so the scanner records it.

use crate::config::OscColorReportFormat;
use crate::engine::Rgb;

/// Cap on a single OSC body, so a malformed never-terminated sequence can't grow
/// the buffer without bound. Color sequences are tiny; 64 KiB is generous.
const MAX_BODY: usize = 1 << 16;

/// How a program terminated its OSC — the reply must echo the same one.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Terminator {
    /// `BEL` (`0x07`).
    Bel,
    /// `ESC \` (String Terminator).
    St,
}

impl Terminator {
    fn bytes(self) -> &'static [u8] {
        match self {
            Self::Bel => b"\x07",
            Self::St => b"\x1b\\",
        }
    }
}

/// A color the program asked us to report. `code` is the OSC number: 10
/// foreground, 11 background, 12 cursor.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct ColorQuery {
    pub code: u8,
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

/// Streaming scanner for OSC color queries. Feed it PTY output in arbitrary
/// chunks; it emits one entry per color the program asked about.
pub struct OscColorScanner {
    state: State,
    buf: Vec<u8>,
    /// True once `buf` overflowed `MAX_BODY`, so the sequence is abandoned.
    overflowed: bool,
}

impl OscColorScanner {
    pub fn new() -> Self {
        Self {
            state: State::Ground,
            buf: Vec::new(),
            overflowed: false,
        }
    }

    /// Feed a chunk of bytes, appending any color queries to `out`. Sequences may
    /// span multiple calls.
    pub fn feed(&mut self, bytes: &[u8], out: &mut Vec<(ColorQuery, Terminator)>) {
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
                    0x07 => self.finish(Terminator::Bel, out),
                    0x1b => self.state = State::BodyEsc,
                    _ => self.push(b),
                },
                State::BodyEsc => {
                    if b == b'\\' {
                        self.finish(Terminator::St, out);
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

    fn finish(&mut self, term: Terminator, out: &mut Vec<(ColorQuery, Terminator)>) {
        if !self.overflowed {
            let mut queries = Vec::new();
            parse_color_queries(&self.buf, &mut queries);
            out.extend(queries.into_iter().map(|q| (q, term)));
        }
        self.reset();
    }

    fn reset(&mut self) {
        self.state = State::Ground;
        self.buf.clear();
        self.overflowed = false;
    }
}

impl Default for OscColorScanner {
    fn default() -> Self {
        Self::new()
    }
}

/// Parse an OSC body into the color queries it asks for.
///
/// Implements xterm's rule that *each successive parameter changes the next
/// color in the list*: in `10;?;?` the second `?` asks about 11, not 10. A field
/// that *sets* a color still advances the index but emits nothing, since the
/// engine has already applied it — so `11;#000;?` asks about 12.
///
/// Only the dynamic colors 10/11/12 are handled. Palette queries (`OSC 4;n;?`)
/// and 5/13-19 are deferred; reset forms (104/110/111/112) need no reply.
fn parse_color_queries(body: &[u8], out: &mut Vec<ColorQuery>) {
    let Ok(s) = std::str::from_utf8(body) else {
        return;
    };
    let mut fields = s.split(';');
    let Some(code) = fields.next().and_then(|c| c.trim().parse::<u16>().ok()) else {
        return;
    };
    if !(10..=12).contains(&code) {
        return;
    }
    for (i, field) in fields.enumerate() {
        let code = code + i as u16;
        // Successive parameters walk past 12 into colors we don't handle.
        if code > 12 {
            break;
        }
        if field.trim() == "?" {
            out.push(ColorQuery { code: code as u8 });
        }
    }
}

/// The color to report for `q`.
///
/// A cursor query with no cursor color set falls back to the **foreground**,
/// matching Ghostty — reporting black would tell the program the cursor is
/// invisible.
pub fn query_color(q: ColorQuery, fg: Rgb, bg: Rgb, cursor: Option<Rgb>) -> Option<Rgb> {
    match q.code {
        10 => Some(fg),
        11 => Some(bg),
        12 => Some(cursor.unwrap_or(fg)),
        _ => None,
    }
}

/// Build the reply for one color query: `ESC ] <code> ; rgb:<r>/<g>/<b> <term>`.
///
/// Ghostty's default report format is 16-bit, where each 8-bit channel is scaled
/// by **257** (so `0x00` → `0000` and `0xff` → `ffff`, spanning the full range
/// rather than leaving the top half dark). `8-bit` emits the raw two digits, and
/// `none` disables reporting entirely. The terminator echoes the caller's.
pub fn color_report(
    q: ColorQuery,
    color: Rgb,
    fmt: OscColorReportFormat,
    term: Terminator,
) -> Vec<u8> {
    let body = match fmt {
        OscColorReportFormat::None => return Vec::new(),
        OscColorReportFormat::Bits8 => {
            format!("\x1b]{};rgb:{:02x}/{:02x}/{:02x}", q.code, color.r, color.g, color.b)
        }
        OscColorReportFormat::Bits16 => format!(
            "\x1b]{};rgb:{:04x}/{:04x}/{:04x}",
            q.code,
            color.r as u16 * 257,
            color.g as u16 * 257,
            color.b as u16 * 257,
        ),
    };
    let mut out = body.into_bytes();
    out.extend_from_slice(term.bytes());
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scan(chunks: &[&[u8]]) -> Vec<(ColorQuery, Terminator)> {
        let mut s = OscColorScanner::new();
        let mut out = Vec::new();
        for c in chunks {
            s.feed(c, &mut out);
        }
        out
    }

    fn codes(chunks: &[&[u8]]) -> Vec<u8> {
        scan(chunks).into_iter().map(|(q, _)| q.code).collect()
    }

    #[test]
    fn captures_bel_terminated_query() {
        assert_eq!(
            scan(&[b"\x1b]11;?\x07"]),
            vec![(ColorQuery { code: 11 }, Terminator::Bel)]
        );
    }

    #[test]
    fn captures_st_terminated_query() {
        assert_eq!(
            scan(&[b"\x1b]10;?\x1b\\"]),
            vec![(ColorQuery { code: 10 }, Terminator::St)]
        );
    }

    #[test]
    fn handles_sequence_split_across_chunks() {
        assert_eq!(codes(&[b"noise\x1b]12", b";", b"?\x07more"]), vec![12]);
    }

    #[test]
    fn ignores_sets_and_unrelated_osc() {
        // A *set* is applied by the engine and needs no reply.
        assert!(codes(&[b"\x1b]11;rgb:00/ff/00\x07"]).is_empty());
        assert!(codes(&[b"\x1b]11;#ff0000\x07"]).is_empty());
        // Resets need no reply either.
        assert!(codes(&[b"\x1b]110\x07\x1b]111\x07"]).is_empty());
        // Other OSCs and plain text.
        assert!(codes(&[b"\x1b]0;my title\x07hello\r\n"]).is_empty());
        assert!(codes(&[b"\x1b]52;c;aGk=\x07"]).is_empty());
        assert!(codes(&[b"\x1b]7;file:///c/tmp\x07"]).is_empty());
        // OSC 4 palette queries are out of scope for now.
        assert!(codes(&[b"\x1b]4;1;?\x07"]).is_empty());
    }

    #[test]
    fn successive_params_advance_the_dynamic_color() {
        // xterm's rule: the second field asks about the *next* color.
        assert_eq!(codes(&[b"\x1b]10;?;?\x07"]), vec![10, 11]);
        assert_eq!(codes(&[b"\x1b]10;?;?;?\x07"]), vec![10, 11, 12]);
        // A set still advances the index but emits nothing.
        assert_eq!(codes(&[b"\x1b]11;#000;?\x07"]), vec![12]);
        // Walking past 12 stops rather than reporting colors we don't handle.
        assert_eq!(codes(&[b"\x1b]12;?;?\x07"]), vec![12]);
    }

    #[test]
    fn report_scales_channels_by_257() {
        let r = color_report(
            ColorQuery { code: 11 },
            Rgb::new(0x10, 0x12, 0x18),
            OscColorReportFormat::Bits16,
            Terminator::Bel,
        );
        assert_eq!(r, b"\x1b]11;rgb:1010/1212/1818\x07".to_vec());

        // The extremes must span the full 16-bit range.
        let black = color_report(
            ColorQuery { code: 10 },
            Rgb::new(0, 0, 0),
            OscColorReportFormat::Bits16,
            Terminator::Bel,
        );
        assert_eq!(black, b"\x1b]10;rgb:0000/0000/0000\x07".to_vec());
        let white = color_report(
            ColorQuery { code: 10 },
            Rgb::new(255, 255, 255),
            OscColorReportFormat::Bits16,
            Terminator::Bel,
        );
        assert_eq!(white, b"\x1b]10;rgb:ffff/ffff/ffff\x07".to_vec());
    }

    #[test]
    fn report_echoes_the_callers_terminator() {
        let r = color_report(
            ColorQuery { code: 11 },
            Rgb::new(0x10, 0x12, 0x18),
            OscColorReportFormat::Bits16,
            Terminator::St,
        );
        assert!(r.ends_with(b"\x1b\\"), "ST query must get an ST reply");
    }

    #[test]
    fn report_8bit_is_unscaled_two_digits() {
        let r = color_report(
            ColorQuery { code: 11 },
            Rgb::new(0x10, 0x12, 0x18),
            OscColorReportFormat::Bits8,
            Terminator::Bel,
        );
        assert_eq!(r, b"\x1b]11;rgb:10/12/18\x07".to_vec());
    }

    #[test]
    fn report_none_format_is_silent() {
        let r = color_report(
            ColorQuery { code: 11 },
            Rgb::new(0x10, 0x12, 0x18),
            OscColorReportFormat::None,
            Terminator::Bel,
        );
        assert!(r.is_empty());
    }

    #[test]
    fn cursor_query_falls_back_to_foreground() {
        let fg = Rgb::new(1, 2, 3);
        let bg = Rgb::new(4, 5, 6);
        assert_eq!(query_color(ColorQuery { code: 10 }, fg, bg, None), Some(fg));
        assert_eq!(query_color(ColorQuery { code: 11 }, fg, bg, None), Some(bg));
        // No cursor color set → report the foreground, not black.
        assert_eq!(query_color(ColorQuery { code: 12 }, fg, bg, None), Some(fg));
        let cur = Rgb::new(7, 8, 9);
        assert_eq!(query_color(ColorQuery { code: 12 }, fg, bg, Some(cur)), Some(cur));
    }
}
