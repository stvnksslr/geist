//! ConPTY read-drain throughput harness (host integration). Spawns a real shell
//! that emits a large known stream and measures how fast giest drains the PTY
//! output channel and feeds it through the engine.
//!
//! This is `#[ignore]`d: it needs Zig 0.15.2 (to build libghostty-vt) and a real
//! PowerShell on the machine, so it is not part of the default `cargo test`/CI
//! run. Run it manually:
//!
//! ```text
//! cargo test --test conpty_throughput -- --ignored --nocapture
//! ```

use std::sync::mpsc::RecvTimeoutError;
use std::time::{Duration, Instant};

use giest::engine::{GhosttyVtEngine, TerminalEngine};
use giest::pty::Pty;

#[test]
#[ignore = "spawns a real shell; run with --ignored --nocapture"]
fn conpty_read_drain_throughput() {
    const LINES: usize = 30_000;
    // A self-terminating PowerShell command that prints LINES fixed-width lines.
    let cmd = format!(
        "1..{LINES} | ForEach-Object {{ 'the quick brown fox jumps over the lazy dog 0123456789' }}"
    );
    let args: Vec<String> = vec![
        "-NoProfile".into(),
        "-NonInteractive".into(),
        "-Command".into(),
        cmd,
    ];

    let mut pty = Pty::spawn("powershell.exe", &args, None, 80, 24, || {}).expect("spawn shell");
    let mut eng = GhosttyVtEngine::new(80, 24, 10_000).expect("engine");

    let start = Instant::now();
    let mut total = 0usize;
    loop {
        match pty.output.recv_timeout(Duration::from_millis(250)) {
            Ok(chunk) => {
                total += chunk.len();
                eng.write(&chunk);
            }
            // On Windows ConPTY the output pipe often does not EOF on child exit,
            // so a quiet gap means "check whether the shell is still alive".
            Err(RecvTimeoutError::Timeout) => {
                if !pty.is_running() {
                    break;
                }
            }
            Err(RecvTimeoutError::Disconnected) => break,
        }
    }
    // Drain anything buffered after the child exited.
    while let Ok(chunk) = pty.output.try_recv() {
        total += chunk.len();
        eng.write(&chunk);
    }

    let dt = start.elapsed();
    let mib = total as f64 / (1024.0 * 1024.0);
    let rate = mib / dt.as_secs_f64().max(1e-9);
    eprintln!(
        "ConPTY drain: {total} bytes ({mib:.2} MiB) in {dt:?} = {rate:.1} MiB/s, {LINES} lines"
    );

    // Throughput floor sanity check: at least one byte per emitted line drained.
    assert!(
        total > LINES,
        "expected to drain more than {LINES} bytes, got {total}"
    );
}
