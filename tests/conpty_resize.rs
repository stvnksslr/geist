//! What a pane resize does to the screen under a real ConPTY (host integration).
//!
//! A split shrinks the pane it divides, and geist resizes the VT engine and the
//! pseudoconsole together (`Session::fit_grid`). ConPTY keeps a screen of its own
//! and keeps addressing the terminal by absolute position after the resize, so
//! the two screens have to agree about where every row went. If they didn't, the
//! terminal would write the live row somewhere the engine never draws.
//!
//! They do agree — that is what this probe pins. It exists because "the prompt
//! of a freshly split pane is missing and the cursor sits a row off" *looks*
//! exactly like that desync, and wasn't: it was the renderer drawing into
//! egui-wgpu's narrowed callback viewport (see `TermFrame::paint`). Run this
//! first for any "text on the wrong row after a resize" report, to rule the
//! engine/ConPTY half in or out before touching the renderer.
//!
//! It replays the sequence headlessly — the same script, the same resize order
//! as `fit_grid` — and checks the engine's screen afterwards.
//!
//! `#[ignore]`d: it needs a real ConPTY. Run it in both modes, like
//! `conpty_passthrough.rs`:
//!
//! ```text
//! cargo test --test conpty_resize -- --ignored --nocapture
//! geist_TEST_PASSTHROUGH=1 cargo test --test conpty_resize -- --ignored --nocapture
//! ```

use std::sync::mpsc::RecvTimeoutError;
use std::time::{Duration, Instant};

use geist::engine::ghostty_vt::GhosttyVtEngine;
use geist::engine::{GridSnapshot, TerminalEngine};
use geist::pty::Pty;

const DEADLINE: Duration = Duration::from_secs(20);
const QUIET: Duration = Duration::from_millis(400);

fn select_mode() {
    let on = std::env::var("geist_TEST_PASSTHROUGH").is_ok_and(|v| v == "1");
    portable_pty::set_allow_sideload(on);
    portable_pty::set_passthrough(on);
}

fn contains(haystack: &[u8], needle: &[u8]) -> bool {
    haystack.windows(needle.len()).any(|w| w == needle)
}

/// Control bytes made visible, so a failure shows what ConPTY actually sent.
fn escaped(bytes: &[u8]) -> String {
    let mut s = String::new();
    for &b in bytes {
        match b {
            0x1b => s.push_str("\\e"),
            b'\r' => s.push_str("\\r"),
            b'\n' => s.push_str("\\n\n"),
            0x20..=0x7e => s.push(b as char),
            _ => s.push_str(&format!("\\x{b:02x}")),
        }
    }
    s
}

/// Pump PTY output into the engine until the stream has been quiet for
/// `QUIET` (after `needle` has appeared, if given). Engine responses — above all
/// the cursor-position report ConPTY opens with — go back to the PTY, exactly
/// as `Session::pump_pty` does. Returns the bytes seen.
fn pump(pty: &mut Pty, eng: &mut GhosttyVtEngine, needle: Option<&[u8]>) -> Vec<u8> {
    let started = Instant::now();
    let mut seen = Vec::new();
    loop {
        match pty.output.recv_timeout(QUIET) {
            Ok(chunk) => {
                eng.write(&chunk);
                let r = eng.take_responses();
                if !r.is_empty() {
                    let _ = pty.write(&r);
                }
                seen.extend_from_slice(&chunk);
            }
            Err(RecvTimeoutError::Timeout) => {
                if needle.is_none_or(|n| contains(&seen, n)) {
                    break;
                }
            }
            Err(RecvTimeoutError::Disconnected) => break,
        }
        assert!(
            started.elapsed() < DEADLINE,
            "no quiet stream after {DEADLINE:?}: {}",
            escaped(&seen)
        );
    }
    seen
}

fn rows_of(snap: &GridSnapshot) -> Vec<String> {
    (0..snap.rows)
        .map(|y| {
            let mut line: String = (0..snap.cols)
                .map(|x| {
                    let t = &snap.cell(x, y).unwrap().text;
                    if t.is_empty() {
                        " ".to_string()
                    } else {
                        t.to_string()
                    }
                })
                .collect();
            line.truncate(line.trim_end().len());
            line
        })
        .collect()
}

fn dump(label: &str, snap: &GridSnapshot) {
    eprintln!(
        "--- {label}: {}x{}, cursor ({}, {})",
        snap.cols, snap.rows, snap.cursor_x, snap.cursor_y
    );
    for (y, line) in rows_of(snap).iter().enumerate() {
        let mark = if y as u16 == snap.cursor_y { '>' } else { ' ' };
        eprintln!("{mark}{y:3}|{line}");
    }
}

/// The script from the app repro: a long ruler (so it wraps), seventy numbered
/// two-part lines, then `pause`, which leaves the cursor at the end of its
/// prompt on the last row.
fn fill_script() -> std::path::PathBuf {
    let path = std::env::temp_dir().join(format!("geist-conpty-resize-{}.cmd", std::process::id()));
    std::fs::write(
        &path,
        "@echo off\r\n\
         setlocal enabledelayedexpansion\r\n\
         set R=\r\n\
         for /L %%i in (0,1,29) do set R=!R!%%i_________\r\n\
         echo RULER:!R!END\r\n\
         for /L %%n in (1,1,70) do echo row %%n abcdefghijklmnopqrstuvwxyz 0123456789 ABCDEFGHIJKLMNOPQRSTUVWXYZ tail-%%n\r\n\
         echo LAST-LINE-BEFORE-PAUSE\r\n\
         pause\r\n",
    )
    .unwrap();
    path
}

/// `Session::fit_grid`'s order: engine, pin to the bottom, then the PTY.
fn fit(pty: &Pty, eng: &mut GhosttyVtEngine, cols: u16, rows: u16) {
    eng.resize(cols, rows, (11, 24)).unwrap();
    eng.scroll_to_bottom();
    pty.resize(cols, rows).unwrap();
}

fn assert_prompt_on_cursor_row(snap: &GridSnapshot) {
    assert!(snap.cursor_y < snap.rows, "cursor outside the grid");
    let lines = rows_of(snap);
    assert!(
        lines[snap.cursor_y as usize].starts_with("Press any key to continue"),
        "the cursor's row lost the prompt: {:?}",
        lines[snap.cursor_y as usize]
    );
}

/// Spawn the fill script at `spawn` size, let it run for `settle` before the
/// first `fit` to `full` (the app's first frame), finish, then shrink to
/// `half` rows (a `new_split:down`). Returns the snapshot after each step.
fn run(
    spawn: (u16, u16),
    settle: Duration,
    full: (u16, u16),
    half: u16,
) -> (GridSnapshot, GridSnapshot) {
    select_mode();
    let script = fill_script();
    let mut pty = Pty::spawn(
        "cmd.exe",
        &["/c".into(), script.display().to_string()],
        None,
        &[],
        spawn.0,
        spawn.1,
        || {},
    )
    .expect("spawn cmd");
    let mut eng = GhosttyVtEngine::new(spawn.0, spawn.1, 10_000_000).unwrap();

    // Output produced before the first frame: drain whatever has arrived in
    // `settle`, then resize with the rest still in flight, as the app does.
    let t = Instant::now();
    let mut early = Vec::new();
    while t.elapsed() < settle {
        if let Ok(chunk) = pty.output.recv_timeout(Duration::from_millis(5)) {
            eng.write(&chunk);
            let r = eng.take_responses();
            if !r.is_empty() {
                let _ = pty.write(&r);
            }
            early.extend_from_slice(&chunk);
        }
    }
    eprintln!("--- {} bytes before the first fit", early.len());
    if full != spawn {
        fit(&pty, &mut eng, full.0, full.1);
    }
    let prompt: &[u8] = b"Press any key";
    pump(
        &mut pty,
        &mut eng,
        (!contains(&early, prompt)).then_some(prompt),
    );
    let mut before = GridSnapshot::default();
    eng.snapshot(&mut before).unwrap();
    dump("before split", &before);

    fit(&pty, &mut eng, full.0, half);
    let after_bytes = pump(&mut pty, &mut eng, None);
    eprintln!(
        "--- bytes after the shrink ({}):\n{}",
        after_bytes.len(),
        escaped(&after_bytes)
    );
    let mut after = GridSnapshot::default();
    eng.snapshot(&mut after).unwrap();
    dump("after split", &after);
    let _ = std::fs::remove_file(&script);
    (before, after)
}

/// Shrink a full-height pane to half height, the way a `new_split:down` does,
/// and require the prompt to still be on the cursor's row. Spawned straight at
/// its final size: the control case.
#[test]
#[ignore]
fn shrinking_a_pane_keeps_the_prompt_on_the_cursor_row() {
    let (_, after) = run((129, 37), Duration::ZERO, (129, 37), 17);
    assert_prompt_on_cursor_row(&after);
}

/// The app's real sequence: every pane is spawned at the 80x24 default and
/// only fitted to its pane on the first frame, while the shell's first output
/// is still in flight. Then the split shrinks it.
#[test]
#[ignore]
fn shrinking_a_pane_fitted_after_spawn_keeps_the_prompt_on_the_cursor_row() {
    let (before, after) = run((80, 24), Duration::from_millis(60), (129, 37), 17);
    assert_prompt_on_cursor_row(&before);
    assert_prompt_on_cursor_row(&after);
}
