//! Do a program's terminal *queries* make the round trip through ConPTY?
//!
//! A program can only use what it can detect. Claude Code, for one, turns on
//! synchronized output (DEC 2026) only when the terminal answers XTVERSION and
//! then DECRQM 2026, using a DA1 reply as the "everything before this has been
//! answered" marker. If any of those replies is lost (or reordered) on the way
//! back through ConPTY, the program never learns the terminal holds frames, and
//! every redraw of it is drawn half-finished: flicker, with no error anywhere.
//!
//! The answers matter as much as the transport. Before libghostty-vt 1.0 the
//! engine ignored DA1 unless a callback answered it, and geist installed none,
//! so Claude Code's probe never finished and it never used DEC 2026 here; 1.0
//! answers DA1 by default, which is when it started to.
//!
//! This drives a child behind a real ConPTY with a real `GhosttyVtEngine`
//! answering, exactly as `Session::pump_pty` does, and records what the child
//! reads back. `#[ignore]`d: it needs a real PowerShell.
//!
//! ```text
//! cargo test --test conpty_queries -- --ignored --nocapture
//! geist_TEST_PASSTHROUGH=1 cargo test --test conpty_queries -- --ignored --nocapture
//! ```

use std::sync::mpsc::RecvTimeoutError;
use std::time::{Duration, Instant};

use base64::Engine as _;
use geist::engine::TerminalEngine;
use geist::engine::ghostty_vt::GhosttyVtEngine;
use geist::pty::Pty;

const DEADLINE: Duration = Duration::from_secs(30);

fn requested_passthrough() -> bool {
    std::env::var("geist_TEST_PASSTHROUGH").is_ok_and(|v| v == "1")
}

fn select_mode() {
    let on = requested_passthrough();
    portable_pty::set_allow_sideload(on);
    portable_pty::set_passthrough(on);
}

fn contains(haystack: &[u8], needle: &[u8]) -> bool {
    haystack.windows(needle.len()).any(|w| w == needle)
}

/// The child: puts its console in raw input mode (VT input on or off), writes
/// `query`, and reports every character it reads back for `ms` milliseconds.
/// Control characters are rendered as `<hh>` so the report is plain text that
/// ConPTY passes through untouched.
const PROBE_CS: &str = r#"
using System;
using System.Runtime.InteropServices;
using System.Text;
using System.Diagnostics;
public static class GeistQueryProbe {
  [DllImport("kernel32.dll")] static extern IntPtr GetStdHandle(int n);
  [DllImport("kernel32.dll")] static extern bool GetConsoleMode(IntPtr h, out uint m);
  [DllImport("kernel32.dll")] static extern bool SetConsoleMode(IntPtr h, uint m);
  [DllImport("kernel32.dll")] static extern bool WriteFile(IntPtr h, byte[] b, uint n, out uint w, IntPtr o);
  [DllImport("kernel32.dll")] static extern uint WaitForSingleObject(IntPtr h, uint ms);
  [DllImport("kernel32.dll", EntryPoint = "ReadConsoleInputW")]
  static extern bool ReadConsoleInput(IntPtr h, [Out] Rec[] buf, uint len, out uint read);
  [StructLayout(LayoutKind.Explicit, Size = 20)]
  public struct Rec {
    [FieldOffset(0)] public ushort EventType;
    [FieldOffset(4)] public int KeyDown;
    [FieldOffset(14)] public ushort Char;
  }
  public static string Run(string query, int ms, bool vt) {
    IntPtr hin = GetStdHandle(-10), hout = GetStdHandle(-11);
    uint om; GetConsoleMode(hout, out om); SetConsoleMode(hout, om | 0x4);
    uint im; GetConsoleMode(hin, out im);
    SetConsoleMode(hin, vt ? 0x200u : 0u);
    byte[] q = Encoding.UTF8.GetBytes(query); uint w;
    WriteFile(hout, q, (uint)q.Length, out w, IntPtr.Zero);
    var sb = new StringBuilder(); var sw = Stopwatch.StartNew(); var recs = new Rec[256];
    while (sw.ElapsedMilliseconds < ms) {
      if (WaitForSingleObject(hin, (uint)Math.Max(1, ms - sw.ElapsedMilliseconds)) != 0) break;
      uint n; if (!ReadConsoleInput(hin, recs, (uint)recs.Length, out n)) break;
      for (int i = 0; i < n; i++) {
        if (recs[i].EventType != 1 || recs[i].KeyDown == 0 || recs[i].Char == 0) continue;
        int c = recs[i].Char;
        if (c < 0x20 || c == 0x7f) sb.Append("<" + c.ToString("x2") + ">"); else sb.Append((char)c);
      }
    }
    SetConsoleMode(hin, im);
    return sb.ToString();
  }
}
"#;

/// What one probe run saw: the queries as they reached the terminal side, and
/// the replies as the child read them.
struct Probe {
    /// Everything ConPTY emitted towards the terminal.
    emitted: Vec<u8>,
    /// Everything the engine answered, in order.
    answered: Vec<u8>,
    /// What the child read back, control characters as `<hh>`.
    read_back: String,
}

/// Run the probe child with `query` (a C# string literal body) in VT-input
/// (`vt`) or legacy raw mode.
fn probe(query: &str, vt: bool) -> Probe {
    let script = format!(
        "Add-Type -TypeDefinition @'\n{PROBE_CS}\n'@\n\
         $r = [GeistQueryProbe]::Run([Text.RegularExpressions.Regex]::Unescape('{query}'), 2000, ${vt})\n\
         [Console]::Out.Write('GEIST-PROBE<' + $r + '>GEIST-PROBE')\n\
         Start-Sleep -Milliseconds 300\n"
    );
    let utf16: Vec<u8> = script
        .encode_utf16()
        .flat_map(|u| u.to_le_bytes())
        .collect();
    let encoded = base64::engine::general_purpose::STANDARD.encode(utf16);
    let args: Vec<String> = vec![
        "-NoProfile".into(),
        "-NonInteractive".into(),
        "-EncodedCommand".into(),
        encoded,
    ];
    select_mode();
    let mut pty =
        Pty::spawn("powershell.exe", &args, None, &[], 80, 24, || {}).expect("spawn shell");
    let mut engine = GhosttyVtEngine::new(80, 24, 10_000).expect("engine");
    let mut emitted = Vec::new();
    let mut answered = Vec::new();
    let started = Instant::now();
    loop {
        match pty.output.recv_timeout(Duration::from_millis(200)) {
            Ok(chunk) => {
                emitted.extend_from_slice(&chunk);
                // Exactly the app's order: parse, then write back everything
                // the engine owes in one write.
                engine.write(&chunk);
                let replies = engine.take_responses();
                if !replies.is_empty() {
                    answered.extend_from_slice(&replies);
                    let _ = pty.write(&replies);
                }
                if contains(&emitted, b">GEIST-PROBE") {
                    break;
                }
            }
            Err(RecvTimeoutError::Timeout) => {
                if !pty.is_running() {
                    break;
                }
            }
            Err(RecvTimeoutError::Disconnected) => break,
        }
        assert!(
            started.elapsed() < DEADLINE,
            "probe never reported. Emitted so far: {:?}",
            String::from_utf8_lossy(&emitted)
        );
    }
    let text = String::from_utf8_lossy(&emitted).into_owned();
    let read_back = text
        .split("GEIST-PROBE<")
        .nth(1)
        .and_then(|s| s.split(">GEIST-PROBE").next())
        .unwrap_or("")
        .to_string();
    Probe {
        emitted,
        answered,
        read_back,
    }
}

/// `CSI > 0 q` (XTVERSION), `CSI ? 2026 $ p` (DECRQM), `CSI c` (DA1) — Claude
/// Code's capability probe, in its order, with DA1 as the sentinel.
const CLAUDE_PROBE: &str = r"\x1b[>0q\x1b[?2026$p\x1b[c";

fn report(p: &Probe) {
    let esc = |b: &[u8]| String::from_utf8_lossy(b).replace('\x1b', "<1b>");
    eprintln!(
        "queries reached the terminal: XTVERSION={} DECRQM2026={} DA1={}",
        contains(&p.emitted, b"\x1b[>0q") || contains(&p.emitted, b"\x1b[>q"),
        contains(&p.emitted, b"\x1b[?2026$p"),
        contains(&p.emitted, b"\x1b[c") || contains(&p.emitted, b"\x1b[0c"),
    );
    eprintln!("engine answered: {}", esc(&p.answered));
    eprintln!("child read back: {}", p.read_back);
}

#[test]
#[ignore = "spawns a real shell; run with --ignored --nocapture"]
fn capability_queries_round_trip_in_vt_input_mode() {
    let p = probe(CLAUDE_PROBE, true);
    report(&p);
    if !requested_passthrough() {
        // The inbox conhost answers DECRQM and DA1 *itself* and forwards only
        // XTVERSION (measured on 10.0.26300): 2026 comes back "not recognized"
        // and DA1 overtakes the XTVERSION reply. A program that uses DA1 as
        // its sentinel (Claude Code) concludes there was no XTVERSION reply,
        // never asks about 2026, and never wraps a frame in a synchronized
        // update. Asserted as found, so a Windows build that changes it fails
        // here and gets noticed.
        assert!(
            p.read_back.contains("<1b>[?2026;0$y"),
            "the inbox ConPTY no longer answers DECRQM 2026 itself; \
             re-check what programs detect through it. Read back: {}",
            p.read_back
        );
        return;
    }
    assert!(
        p.read_back.contains("<1b>P>|"),
        "the XTVERSION reply never reached the program"
    );
    assert!(
        p.read_back.contains("<1b>[?2026;"),
        "the DECRQM 2026 reply never reached the program"
    );
    let xt = p.read_back.find("<1b>P>|").unwrap();
    let da1 = p.read_back.find("<1b>[?6").unwrap_or(0);
    assert!(
        xt < da1,
        "DA1 was answered before XTVERSION: a program using DA1 as its \
         sentinel concludes XTVERSION went unanswered"
    );
}

#[test]
#[ignore = "spawns a real shell; run with --ignored --nocapture"]
fn capability_queries_round_trip_in_legacy_raw_mode() {
    // A program reading key events rather than VT input. Reported, not
    // asserted. Measured: identical to VT-input mode under the sideloaded
    // ConPTY; under the inbox one the XTVERSION reply reaches the program as
    // nothing but its final `\`.
    let p = probe(CLAUDE_PROBE, false);
    report(&p);
}
