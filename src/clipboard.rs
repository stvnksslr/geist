//! Program clipboard access — OSC 52 and the kitty clipboard protocol
//! (OSC 5522) — serviced through libghostty-vt's clipboard callbacks.
//!
//! The engine parses both protocols (base64, chunking, MIME lists, passwords,
//! the 5522 status replies) and hands giest one normalized request per read or
//! write. This module is the *policy* half: `clipboard-read` / `clipboard-write`
//! / `clipboard-write-limit-bytes`, which MIME types Windows can serve, and the
//! deferral that makes `ask` possible.
//!
//! ## Why `ask` needs a deferral, and how it works
//!
//! The callbacks are synchronous: the terminal must be answered before the
//! callback returns, and a callback that does not answer is taken as a refusal
//! (OSC 52 gets an empty clipboard, 5522 gets `EPERM`) which the engine writes
//! to the PTY *immediately*. Upstream's advice is to block on a modal — which
//! giest cannot do, since the modal is drawn by the very thread that would be
//! blocked. So for `ask`:
//!
//! 1. the callback records the request and where the response buffer ended
//!    ([`Pending::offset`]) and leaves the request unanswered;
//! 2. after `vt_write` returns, the refusal the engine wrote at that offset is
//!    **cut back out** of the response buffer ([`cut_packet`]) and kept;
//! 3. when the user answers, a refusal sends the kept packet verbatim, and an
//!    approval either performs the write and sends the packet turned into
//!    `DONE` ([`done_packet`]), or **replays** the read request into the engine
//!    ([`replay_request`]) with a one-shot grant, so the engine formats the
//!    real reply itself.
//!
//! The offset is exact because the VT stream is paused for the callback's
//! whole duration: nothing else can write between the callback starting and
//! the engine's refusal.

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD;

use crate::config::ClipboardAccess;

/// The MIME type giest offers and serves: Windows' `CF_UNICODETEXT` via arboard.
pub const TEXT_PLAIN: &str = "text/plain";

/// Whether `mime` names plain text in any spelling a program is likely to use.
/// Kitty and X11 programs use `text/plain;charset=utf-8`, `UTF8_STRING`,
/// `TEXT` and `STRING`; all of them are the same thing on Windows.
pub fn is_text_mime(mime: &str) -> bool {
    let m = mime.trim().to_ascii_lowercase();
    let base = m.split(';').next().unwrap_or("").trim();
    if base == "text/plain" {
        // Any charset parameter other than UTF-8 would need transcoding.
        return match m.split_once("charset=") {
            None => true,
            Some((_, cs)) => matches!(cs.trim().trim_matches('"'), "utf-8" | "utf8"),
        };
    }
    matches!(m.as_str(), "utf8_string" | "text" | "string")
}

/// What a write's representations amount to on a text-only clipboard.
#[derive(Debug, PartialEq, Eq)]
pub enum WriteText {
    /// Put this text on the clipboard. Empty for the "clear" shape.
    Text(String),
    /// Every representation is something Windows text can't hold.
    Unsupported,
}

/// Pick the text representation of a write. A write carrying no
/// representations at all is a *clear* (OSC 52 with an empty payload); a
/// write with a text representation keeps the text and ignores the rest (an
/// HTML or image alongside it is dropped rather than failing the whole write,
/// which would lose the text too).
pub fn write_text(contents: &[(&str, &[u8])]) -> WriteText {
    if contents.is_empty() {
        return WriteText::Text(String::new());
    }
    match contents.iter().find(|(m, _)| is_text_mime(m)) {
        Some((_, data)) => WriteText::Text(String::from_utf8_lossy(data).into_owned()),
        None => WriteText::Unsupported,
    }
}

/// What to do with a request under the configured access.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Verdict {
    Allow,
    Deny,
    Ask,
}

/// `granted` (a kitty password grant, a paste event's follow-up read, or a
/// replay of a request the user just approved) skips the prompt but never
/// overrides an explicit `deny`.
pub fn decide(access: ClipboardAccess, granted: bool) -> Verdict {
    match access {
        ClipboardAccess::Deny => Verdict::Deny,
        ClipboardAccess::Allow => Verdict::Allow,
        ClipboardAccess::Ask if granted => Verdict::Allow,
        ClipboardAccess::Ask => Verdict::Ask,
    }
}

/// The reply contents for a read the policy allows: one text representation
/// per requested text MIME type (echoing the program's own spelling), nothing
/// for the rest — the protocol answers a missing type by leaving it out.
pub fn read_contents(mimes: &[String], text: Option<&str>) -> Vec<(String, Vec<u8>)> {
    let Some(text) = text else {
        return Vec::new();
    };
    mimes
        .iter()
        .filter(|m| is_text_mime(m))
        .map(|m| (m.clone(), text.as_bytes().to_vec()))
        .collect()
}

/// The MIME types on the clipboard, for a 5522 targets listing.
pub fn available(text: Option<&str>) -> Vec<String> {
    match text {
        Some(_) => vec![TEXT_PLAIN.to_string()],
        None => Vec::new(),
    }
}

/// A request left unanswered for the user to decide.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Deferred {
    /// Put `text` on the clipboard.
    Write(String),
    /// Send the clipboard back. `mimes` are the requested types; `primary`
    /// is the 5522 `loc=primary` flag, echoed on replay.
    Read { mimes: Vec<String>, primary: bool },
}

/// A deferral as recorded inside the callback, before the refusal is cut.
#[derive(Clone, Debug)]
pub struct Pending {
    /// Length of the engine's response buffer when the callback started.
    pub offset: usize,
    pub kind: Deferred,
}

/// A deferral ready for the user: the request plus the refusal the engine
/// wrote for it (empty when the protocol has no acknowledgement, i.e. an
/// OSC 52 write).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Deferral {
    pub kind: Deferred,
    pub packet: Vec<u8>,
}

/// Remove the one OSC response starting at `offset` (through its BEL or ST
/// terminator) from `buf` and return it. Anything that isn't an OSC at that
/// offset is left alone and an empty packet returned — the engine wrote no
/// refusal (an OSC 52 write has no acknowledgement).
pub fn cut_packet(buf: &mut Vec<u8>, offset: usize) -> Vec<u8> {
    if offset >= buf.len() || !buf[offset..].starts_with(b"\x1b]") {
        return Vec::new();
    }
    let rest = &buf[offset..];
    let end = (0..rest.len()).find_map(|i| match rest[i] {
        0x07 => Some(i + 1),
        0x1b if rest.get(i + 1) == Some(&b'\\') && i >= 2 => Some(i + 2),
        _ => None,
    });
    let Some(end) = end else {
        return Vec::new();
    };
    buf.drain(offset..offset + end).collect()
}

/// The `key=value` metadata of a 5522 packet (between `5522;` and the next
/// `;` or terminator).
fn packet_metadata(packet: &[u8]) -> Option<&str> {
    let s = std::str::from_utf8(packet).ok()?;
    let body = s.strip_prefix("\x1b]5522;")?;
    let end = body.find([';', '\x07', '\x1b']).unwrap_or(body.len());
    Some(&body[..end])
}

fn packet_field<'a>(packet: &'a [u8], key: &str) -> Option<&'a str> {
    packet_metadata(packet)?
        .split(':')
        .find_map(|kv| kv.strip_prefix(key)?.strip_prefix('='))
}

/// The request to feed back into the engine once the user approves a read:
/// the original request as closely as the callback and the refusal let us
/// rebuild it. `None` if the refusal isn't one we recognise.
pub fn replay_request(kind: &Deferred, packet: &[u8]) -> Option<Vec<u8>> {
    let Deferred::Read { mimes, primary } = kind else {
        return None;
    };
    if let Some(rest) = packet.strip_prefix(b"\x1b]52;") {
        // `ESC ] 52 ; <targets> ; <empty> ST` — echo the same targets.
        let targets: Vec<u8> = rest.iter().take_while(|&&b| b != b';').copied().collect();
        let mut out = b"\x1b]52;".to_vec();
        out.extend_from_slice(&targets);
        out.extend_from_slice(b";?\x1b\\");
        return Some(out);
    }
    packet_metadata(packet)?;
    let mut meta = String::from("type=read");
    if let Some(id) = packet_field(packet, "id") {
        meta.push_str(":id=");
        meta.push_str(id);
    }
    if *primary {
        meta.push_str(":loc=primary");
    }
    let list = STANDARD.encode(mimes.join(" "));
    Some(format!("\x1b]5522;{meta};{list}\x1b\\").into_bytes())
}

/// Turn a 5522 write refusal into its success acknowledgement. Empty in,
/// empty out: an OSC 52 write has no acknowledgement to send.
pub fn done_packet(packet: &[u8]) -> Vec<u8> {
    let s = String::from_utf8_lossy(packet);
    s.replacen("status=EPERM", "status=DONE", 1).into_bytes()
}

/// Reads and writes the real clipboard. A trait so the engine's tests can run
/// the protocols end to end without touching the user's clipboard.
pub trait ClipboardIo {
    fn read_text(&mut self) -> Option<String>;
    fn write_text(&mut self, text: &str);
}

/// The Windows clipboard, via arboard. Best-effort, like every other giest
/// clipboard path: a locked clipboard reads as empty and a failed write is
/// dropped.
pub struct SystemClipboard;

impl ClipboardIo for SystemClipboard {
    fn read_text(&mut self) -> Option<String> {
        arboard::Clipboard::new().ok()?.get_text().ok()
    }
    fn write_text(&mut self, text: &str) {
        if let Ok(mut cb) = arboard::Clipboard::new() {
            let _ = cb.set_text(text.to_owned());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn text_mime_spellings() {
        for m in [
            "text/plain",
            "text/plain;charset=utf-8",
            "TEXT/PLAIN; charset=UTF8",
            "UTF8_STRING",
            "TEXT",
            "STRING",
        ] {
            assert!(is_text_mime(m), "{m}");
        }
        for m in ["text/html", "image/png", "text/plain;charset=latin1", "."] {
            assert!(!is_text_mime(m), "{m}");
        }
    }

    #[test]
    fn write_keeps_text_and_drops_the_rest() {
        let html: &[u8] = b"<b>hi</b>";
        let text: &[u8] = b"hi";
        assert_eq!(
            write_text(&[("text/html", html), ("text/plain", text)]),
            WriteText::Text("hi".into())
        );
        assert_eq!(write_text(&[("image/png", html)]), WriteText::Unsupported);
        assert_eq!(
            write_text(&[]),
            WriteText::Text(String::new()),
            "no representations is a clear"
        );
    }

    #[test]
    fn a_grant_skips_ask_but_never_deny() {
        assert_eq!(decide(ClipboardAccess::Ask, false), Verdict::Ask);
        assert_eq!(decide(ClipboardAccess::Ask, true), Verdict::Allow);
        assert_eq!(decide(ClipboardAccess::Deny, true), Verdict::Deny);
        assert_eq!(decide(ClipboardAccess::Allow, false), Verdict::Allow);
    }

    #[test]
    fn reads_serve_only_the_text_types_asked_for() {
        let mimes = vec![
            "image/png".to_string(),
            "text/plain;charset=utf-8".to_string(),
        ];
        assert_eq!(
            read_contents(&mimes, Some("x")),
            vec![("text/plain;charset=utf-8".to_string(), b"x".to_vec())]
        );
        assert!(read_contents(&mimes, None).is_empty());
        assert_eq!(available(Some("")), vec!["text/plain".to_string()]);
        assert!(available(None).is_empty());
    }

    #[test]
    fn cut_packet_removes_exactly_one_response() {
        let mut buf = b"AB\x1b]5522;type=read:status=EPERM:id=r1\x1b\\\x1b[0nZ".to_vec();
        let p = cut_packet(&mut buf, 2);
        assert_eq!(p, b"\x1b]5522;type=read:status=EPERM:id=r1\x1b\\");
        assert_eq!(buf, b"AB\x1b[0nZ");
        // BEL-terminated.
        let mut buf = b"\x1b]52;c;\x07tail".to_vec();
        assert_eq!(cut_packet(&mut buf, 0), b"\x1b]52;c;\x07");
        assert_eq!(buf, b"tail");
        // Nothing there (an OSC 52 write has no acknowledgement).
        let mut buf = b"\x1b[0n".to_vec();
        assert!(cut_packet(&mut buf, 0).is_empty());
        assert!(cut_packet(&mut buf, 9).is_empty());
        assert_eq!(buf, b"\x1b[0n");
    }

    #[test]
    fn replay_rebuilds_the_read() {
        let kind = Deferred::Read {
            mimes: vec!["text/plain".into()],
            primary: false,
        };
        assert_eq!(
            replay_request(&kind, b"\x1b]5522;type=read:status=EPERM:id=r1\x1b\\").unwrap(),
            b"\x1b]5522;type=read:id=r1;dGV4dC9wbGFpbg==\x1b\\"
        );
        let prim = Deferred::Read {
            mimes: vec!["a".into(), "b".into()],
            primary: true,
        };
        assert_eq!(
            replay_request(&prim, b"\x1b]5522;type=read:status=EPERM\x07").unwrap(),
            format!(
                "\x1b]5522;type=read:loc=primary;{}\x1b\\",
                STANDARD.encode("a b")
            )
            .into_bytes()
        );
        assert_eq!(
            replay_request(&kind, b"\x1b]52;p;\x1b\\").unwrap(),
            b"\x1b]52;p;?\x1b\\"
        );
        assert_eq!(
            replay_request(&Deferred::Write("x".into()), b"\x1b]52;c;\x07"),
            None
        );
    }

    #[test]
    fn done_packet_flips_only_the_status() {
        assert_eq!(
            done_packet(b"\x1b]5522;type=write:status=EPERM:id=w\x1b\\"),
            b"\x1b]5522;type=write:status=DONE:id=w\x1b\\"
        );
        assert!(done_packet(b"").is_empty());
    }
}
