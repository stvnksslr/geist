//! Which escape sequences actually survive ConPTY (host integration).
//!
//! ConPTY does **not** pipe a child's output through. It parses the child's VT
//! and emits its *own* stream, silently dropping anything it doesn't understand
//! — which is how kitty graphics ended up blocked (APC sequences never arrive at
//! all; see GAP.md). The failure mode is "the feature does nothing", with no
//! error anywhere, so for any new escape-sequence support the first question is
//! whether the bytes reach us at all.
//!
//! This test answers that question for the sequences giest side-scans, by
//! spawning a real shell that emits each one and checking the drained PTY output
//! for it. It is the automated form of the temporary `GhosttyVtEngine::write`
//! probe CLAUDE.md recommends.
//!
//! `#[ignore]`d: it needs Zig 0.15.2 (to build libghostty-vt) and a real
//! PowerShell, so it is not part of the default `cargo test` run.
//!
//! ```text
//! cargo test --test conpty_passthrough -- --ignored --nocapture
//! ```

use std::sync::mpsc::RecvTimeoutError;
use std::time::{Duration, Instant};

use giest::profiles::Profile;
use giest::pty::Pty;

/// Give up on a shell that never reports exit. A probe that hangs is far more
/// confusing than one that fails.
const DEADLINE: Duration = Duration::from_secs(20);

/// How long the stream must stay quiet before we call it done.
const QUIET: Duration = Duration::from_millis(400);
const QUIET_GAPS: u32 = 3;

/// Don't start counting quiet gaps before this. PowerShell takes about a second
/// to start, and ConPTY emits its cursor-position query immediately — so
/// "something arrived, then silence" is true within milliseconds of spawning and
/// would end the probe before the shell has run a single statement.
const WARMUP: Duration = Duration::from_secs(3);

/// Spawn `program` with `args` and drive it, returning everything ConPTY
/// emitted. Each `(await, send)` step waits for `await` to appear in the output
/// so far, then writes `send`.
///
/// Used to exercise the real shell-integration hook end to end: the hook ships
/// as a base64 blob inside `-EncodedCommand`, so a syntax error in it produces
/// no diagnostic at all — the prompt marks simply never appear and every feature
/// built on them quietly does nothing.
fn drive(program: &str, args: &[String], steps: &[(&str, &str)]) -> Vec<u8> {
    select_mode();
    let mut pty = Pty::spawn(program, args, None, &[], 80, 24, || {}).expect("spawn shell");
    let mut out = Vec::new();
    let started = Instant::now();
    let mut step = 0usize;
    let mut quiet = 0u32;
    loop {
        match pty.output.recv_timeout(QUIET) {
            Ok(chunk) => {
                if contains(&chunk, b"\x1b[6n") {
                    let _ = pty.write(b"\x1b[1;1R");
                }
                out.extend_from_slice(&chunk);
                quiet = 0;
                // Advance as far as the output allows; a fast shell can satisfy
                // several steps within one chunk.
                while step < steps.len() && contains(&out, steps[step].0.as_bytes()) {
                    let _ = pty.write(steps[step].1.as_bytes());
                    step += 1;
                }
            }
            Err(RecvTimeoutError::Timeout) => {
                if !pty.is_running() {
                    break;
                }
                if step == steps.len() && started.elapsed() > WARMUP {
                    quiet += 1;
                    if quiet >= QUIET_GAPS {
                        break;
                    }
                }
            }
            Err(RecvTimeoutError::Disconnected) => break,
        }
        assert!(
            started.elapsed() < DEADLINE,
            "stuck at step {step}/{} after {DEADLINE:?}. Output so far: {:?}",
            steps.len(),
            String::from_utf8_lossy(&out)
        );
    }
    out
}

/// Run `command` in a real PowerShell and return everything ConPTY emitted.
///
/// Stops on a **quiet stream**, with process exit as an early-out and a hard
/// deadline behind both. Exit alone is not enough to wait on here: ConPTY holds
/// the child's output until its opening cursor-position query is answered (see
/// the reply below), and a child blocked on a full pipe never exits either — so
/// a probe that waits only for exit hangs forever. `conpty_throughput.rs` waits
/// on exit alone and has the same problem.
fn run(command: &str) -> Vec<u8> {
    let args: Vec<String> = vec![
        "-NoProfile".into(),
        "-NonInteractive".into(),
        "-Command".into(),
        command.into(),
    ];
    select_mode();
    let mut pty = Pty::spawn("powershell.exe", &args, None, &[], 80, 24, || {}).expect("spawn shell");

    let mut out = Vec::new();
    let started = Instant::now();
    let mut quiet = 0u32;
    loop {
        match pty.output.recv_timeout(QUIET) {
            Ok(chunk) => {
                // ConPTY opens with a cursor-position report request and waits
                // for the answer before pumping the child's output. The real app
                // answers it from the VT engine (`take_responses`); a probe has
                // no engine, so without this reply the shell's own output never
                // arrives and every assertion below fails identically.
                if contains(&chunk, b"\x1b[6n") {
                    let _ = pty.write(b"\x1b[1;1R");
                }
                out.extend_from_slice(&chunk);
                quiet = 0;
            }
            Err(RecvTimeoutError::Timeout) => {
                if !pty.is_running() {
                    break;
                }
                if started.elapsed() > WARMUP {
                    quiet += 1;
                    if quiet >= QUIET_GAPS {
                        break;
                    }
                }
            }
            Err(RecvTimeoutError::Disconnected) => break,
        }
        assert!(
            started.elapsed() < DEADLINE,
            "no output and no exit after {DEADLINE:?}. Command: {command}"
        );
    }
    while let Ok(chunk) = pty.output.try_recv() {
        out.extend_from_slice(&chunk);
    }
    out
}

/// Which ConPTY mode this run probes. `GIEST_TEST_PASSTHROUGH=1` requests
/// `PSEUDOCONSOLE_PASSTHROUGH_MODE`; the choice is process-global in the
/// vendored portable-pty, so it is set from the environment (identical for
/// every test in the run) rather than per test, which would race.
fn requested_passthrough() -> bool {
    std::env::var("GIEST_TEST_PASSTHROUGH").is_ok_and(|v| v == "1")
}

fn select_mode() {
    let on = requested_passthrough();
    portable_pty::set_allow_sideload(on);
    portable_pty::set_passthrough(on);
}

fn contains(haystack: &[u8], needle: &[u8]) -> bool {
    haystack.windows(needle.len()).any(|w| w == needle)
}

fn count(haystack: &[u8], needle: &[u8]) -> usize {
    haystack.windows(needle.len()).filter(|w| *w == needle).count()
}

/// Emit a raw byte string from PowerShell without it being re-interpreted.
/// `[Console]::Write` is what `profiles.rs`'s OSC 7 / OSC 133 hook uses.
///
/// `expr` is a PowerShell *expression* (build it with [`lit`] / [`ESC`] /
/// [`st`]), and it must contain **no double quotes and no backslashes**. Both
/// are load-bearing, and getting either wrong makes the shell *hang* rather than
/// fail — a genuinely confusing way to lose an afternoon:
///
/// - `CommandBuilder` quotes the argument with the MSVC convention, where a
///   backslash escapes a following quote. A payload ending in `\` therefore
///   turns the closing `"` into a literal one.
/// - `powershell -Command` re-parses the string it is handed and does not
///   reliably honour `\"`-escaped quotes inside it.
///
/// Either way PowerShell ends up with an unterminated string literal and blocks
/// forever waiting for the rest of it. So: single-quoted literals concatenated
/// with `[char]` casts, and no quoting for anyone to get wrong.
fn emit(expr: &str) -> String {
    assert!(
        !expr.contains('"') && !expr.contains('\\'),
        "see the doc comment: no double quotes and no backslashes"
    );
    format!("[Console]::Write({expr}); Start-Sleep -Milliseconds 150")
}

/// `ESC`, as a PowerShell expression.
const ESC: &str = "[char]27";

/// `ESC \` — the String Terminator, without a literal backslash anywhere.
fn st() -> String {
    format!("{ESC} + [char]92")
}

/// A PowerShell single-quoted literal. Single quotes suppress *all*
/// interpolation, so the payload reaches the console byte for byte.
fn lit(s: &str) -> String {
    assert!(!s.contains('\''), "escape single quotes before using them here");
    format!("'{s}'")
}

/// Join expression fragments into one PowerShell string concatenation.
fn cat(parts: &[String]) -> String {
    parts.join(" + ")
}

#[test]
#[ignore = "spawns a real shell; run with --ignored --nocapture"]
fn the_harness_itself_round_trips_plain_output() {
    // Guards the *test*, not giest: if the shell never exits or the command is
    // mis-quoted, every other assertion here fails for a reason that has nothing
    // to do with escape sequences. Check the trivial case first.
    let out = run(&emit(&lit("giest-plain-probe")));
    assert!(
        contains(&out, b"giest-plain-probe"),
        "the harness cannot even round-trip plain text: {:?}",
        String::from_utf8_lossy(&out)
    );
}

#[test]
#[ignore = "spawns a real shell; run with --ignored --nocapture"]
fn osc_notification_sequences_survive_conpty() {
    // OSC 9 (iTerm2 form) and OSC 777 (rxvt form), ST-terminated.
    let out = run(&emit(&cat(&[
        ESC.into(),
        lit("]9;giest-osc9-probe"),
        st(),
        ESC.into(),
        lit("]777;notify;T;B"),
        st(),
    ])));
    let text = String::from_utf8_lossy(&out);
    eprintln!("ConPTY emitted {} bytes: {:?}", out.len(), text);

    assert!(
        contains(&out, b"]9;giest-osc9-probe"),
        "ConPTY dropped OSC 9 — desktop notifications cannot work over ConPTY. Got: {text:?}"
    );
    assert!(
        contains(&out, b"]777;notify;T;B"),
        "ConPTY dropped OSC 777 — the titled notification form cannot work. Got: {text:?}"
    );
}

#[test]
#[ignore = "spawns a real shell; run with --ignored --nocapture"]
fn conemu_progress_sequences_survive_conpty() {
    // OSC 9;4 is ConEmu's own extension and ConPTY is Microsoft's console — but
    // "it obviously passes through" is exactly the assumption that cost an
    // afternoon on kitty graphics, so check rather than assume.
    let out = run(&emit(&cat(&[
        ESC.into(),
        lit("]9;4;1;40"),
        st(),
        ESC.into(),
        lit("]9;4;2"),
        st(),
        ESC.into(),
        lit("]9;4;0"),
        st(),
    ])));
    let text = String::from_utf8_lossy(&out);
    eprintln!("ConPTY emitted {} bytes: {:?}", out.len(), text);

    for needle in [&b"]9;4;1;40"[..], b"]9;4;2", b"]9;4;0"] {
        assert!(
            contains(&out, needle),
            "ConPTY dropped {:?} — taskbar progress cannot work. Got: {text:?}",
            String::from_utf8_lossy(needle)
        );
    }
}

#[test]
#[ignore = "spawns a real shell; run with --ignored --nocapture"]
fn already_shipped_osc_sequences_still_survive_conpty() {
    // A regression net for the sequences giest already side-scans, so a Windows
    // or ConPTY update that starts eating one of them is caught here rather than
    // as a mysteriously dead feature.
    let out = run(&emit(&cat(&[
        ESC.into(),
        lit("]7;file://h/C:/probe"),
        st(),
        ESC.into(),
        lit("]52;c;aGk="),
        st(),
        ESC.into(),
        lit("]133;A"),
        st(),
    ])));
    let text = String::from_utf8_lossy(&out);
    eprintln!("ConPTY emitted {} bytes: {:?}", out.len(), text);

    for needle in [&b"]7;file://h/C:/probe"[..], b"]52;c;aGk=", b"]133;A"] {
        assert!(
            contains(&out, needle),
            "ConPTY dropped {:?}. Got: {text:?}",
            String::from_utf8_lossy(needle)
        );
    }
}

#[test]
#[ignore = "spawns a real shell; run with --ignored --nocapture"]
fn kitty_clipboard_protocol_survives_conpty() {
    // OSC 5522 (kitty clipboard): a read request, a full write transaction
    // (begin, one data chunk, commit), and the DECSET that turns on paste
    // events (mode 5522). The DECSET matters as much as the OSCs: ConPTY
    // re-renders private modes it knows and could drop one it doesn't.
    let out = run(&emit(&cat(&[
        ESC.into(),
        lit("]5522;type=read:id=r1;dGV4dC9wbGFpbg=="),
        st(),
        ESC.into(),
        lit("]5522;type=write:id=w1"),
        st(),
        ESC.into(),
        lit("]5522;type=wdata:mime=dGV4dC9wbGFpbg==;aGk="),
        st(),
        ESC.into(),
        lit("]5522;type=wdata"),
        st(),
        ESC.into(),
        lit("[?5522h"),
    ])));
    let text = String::from_utf8_lossy(&out);
    eprintln!("ConPTY emitted {} bytes: {:?}", out.len(), text);

    for needle in [
        &b"]5522;type=read:id=r1;dGV4dC9wbGFpbg=="[..],
        b"]5522;type=write:id=w1",
        b"]5522;type=wdata:mime=dGV4dC9wbGFpbg==;aGk=",
        b"]5522;type=wdata\x1b",
        b"\x1b[?5522h",
    ] {
        assert!(
            contains(&out, needle),
            "ConPTY dropped {:?}. Got: {text:?}",
            String::from_utf8_lossy(needle)
        );
    }
}

#[test]
#[ignore = "spawns a real shell; run with --ignored --nocapture"]
fn the_cmd_prompt_sets_the_bar_cursor_and_title() {
    let profile = Profile {
        name: "Command Prompt".into(),
        program: "cmd.exe".into(),
        args: Vec::new(),
    };
    let out = drive("cmd.exe", &profile.launch_args(), &[("\x1b]133;B", "exit\r")]);
    let text = String::from_utf8_lossy(&out);
    assert!(contains(&out, b"\x1b]133;A"), "the cmd prompt hook did not load: {text:?}");
    assert!(contains(&out, b"\x1b[5 q"), "the bar cursor did not survive ConPTY: {text:?}");
    assert!(
        contains(&out, b"\x1b]0;C:") || contains(&out, b"\x1b]2;C:"),
        "the cwd title did not survive ConPTY: {text:?}"
    );
}

#[test]
#[ignore = "spawns a real shell; run with --ignored --nocapture"]
fn the_powershell_hook_reports_command_exit_codes() {
    // The real hook, exactly as `Profile::launch_args` builds it — base64 inside
    // `-EncodedCommand`, where a syntax error is invisible.
    let profile = Profile {
        name: "Windows PowerShell".into(),
        program: "powershell.exe".into(),
        args: Vec::new(),
    };
    let args = profile.launch_args();
    assert!(!args.is_empty(), "the powershell profile should carry a hook");

    let out = drive(
        "powershell.exe",
        &args,
        &[
            // Wait for the first prompt, then run a native program that fails
            // with a distinctive code.
            ("\x1b]133;B", "cmd /c exit 3\r"),
            // Then a *cmdlet* that succeeds — the case `$LASTEXITCODE` alone
            // would get wrong — and quit. Both lines are queued together
            // because there is no marker unique to the successful command to
            // wait on: its own `D;0` is indistinguishable from the one the very
            // first prompt emits.
            ("\x1b]133;D;3", "Write-Output giest-ok\rexit\r"),
        ],
    );
    let text = String::from_utf8_lossy(&out);

    // The hook loaded at all: prompt marks and the cwd report are present.
    assert!(contains(&out, b"\x1b]133;A"), "no OSC 133 A — the hook did not load: {text:?}");
    assert!(contains(&out, b"\x1b]7;file://"), "no OSC 7 cwd report: {text:?}");
    // …and the exit code round-trips. This is the whole feature: `$?` read
    // first, falling back to $LASTEXITCODE for a native program's real code.
    assert!(
        contains(&out, b"\x1b]133;D;3"),
        "the hook did not report exit code 3: {text:?}"
    );
    assert!(contains(&out, b"giest-ok"), "the second command never ran: {text:?}");
    // `shell-integration-features` defaults: `cursor` (blinking bar at the
    // prompt) and `title` (cwd). ConPTY re-renders both rather than passing
    // them through, so what arrives is its own form: the DECSCUSR survives as
    // is, and the title comes back as a title-setting OSC (0 or 2).
    assert!(contains(&out, b"\x1b[5 q"), "the bar cursor did not survive ConPTY: {text:?}");
    assert!(
        contains(&out, b"\x1b]0;~") || contains(&out, b"\x1b]2;~")
            || contains(&out, b"\x1b]0;C:") || contains(&out, b"\x1b]2;C:"),
        "the cwd title did not survive ConPTY: {text:?}"
    );
    // Two `D;0`s: one from the very first prompt (before anything ran — which is
    // why `Session` ignores a `D` with no matching start), and one for the
    // cmdlet that succeeded. Only the second is a real report, and `$?` is what
    // catches it: `$LASTEXITCODE` would still be 3 from the previous command.
    assert!(
        count(&out, b"\x1b]133;D;0") >= 2,
        "the hook did not report the successful cmdlet as exit 0 — \
         `$?` is probably being clobbered before it is read: {text:?}"
    );
}

#[test]
#[ignore = "spawns a real shell; run with --ignored --nocapture"]
fn enq_is_still_stripped_by_conpty() {
    // `enquiry-response` answers a bare ENQ (0x05) emitted by the program. That
    // is only implementable if the byte reaches us at all — ConPTY re-renders the
    // stream and drops what it doesn't forward, which is what blocks kitty
    // graphics. Probed before building, per this repo's own rule, and **measured
    // stripped**: the run below emits `open`, ENQ, `close` and ConPTY returns
    // `giest-enq-opengiest-enq-close`.
    //
    // So this is asserted **inverted**, like the APC probe: it fails if a future
    // Windows build starts forwarding ENQ, which is how we would learn
    // `enquiry-response` is unblocked.
    //
    // The markers bracket the ENQ so a *missing* byte is distinguishable from a
    // failed run: both markers present and no 0x05 means ConPTY ate it.
    let out = run(&emit(&cat(&[
        lit("giest-enq-open"),
        "[char]5".into(),
        lit("giest-enq-close"),
    ])));
    let text = String::from_utf8_lossy(&out);
    eprintln!("ConPTY emitted {} bytes: {:?}", out.len(), text);
    assert!(
        contains(&out, b"giest-enq-open") && contains(&out, b"giest-enq-close"),
        "the probe itself did not run: {text:?}"
    );
    if requested_passthrough() {
        // Measured: the out-of-band ConPTY forwards ENQ too.
        assert!(
            contains(&out, b"giest-enq-open\x05giest-enq-close"),
            "the sideloaded ConPTY no longer forwards ENQ. Got: {text:?}"
        );
        return;
    }
    assert!(
        !contains(&out, b"giest-enq-open\x05giest-enq-close"),
        "ConPTY now forwards ENQ (0x05) — `enquiry-response` may be unblocked. \
         Got: {text:?}"
    );
}

#[test]
#[ignore = "spawns a real shell; run with --ignored --nocapture"]
fn apc_is_stripped_unless_passthrough() {
    // The documented blocker for kitty graphics. In the default (re-rendering)
    // mode this is asserted *inverted*: if a future Windows build stops
    // stripping APC, it fails and tells us the feature is unblocked there too.
    // With passthrough on (GIEST_TEST_PASSTHROUGH=1) APC must arrive verbatim.
    let out = run(&emit(&cat(&[
        ESC.into(),
        lit("_Ggiest-apc-probe"),
        st(),
    ])));
    let text = String::from_utf8_lossy(&out);
    eprintln!("ConPTY emitted {} bytes: {:?}", out.len(), text);
    if requested_passthrough() {
        // The flag alone does nothing on the inbox conhost (measured: S_OK,
        // APC still stripped). What forwards APC is the out-of-band ConPTY.
        assert!(
            portable_pty::sideloaded(),
            "no conpty.dll next to the test exe — run scripts/fetch-conpty.ps1 first"
        );
        assert!(
            contains(&out, b"\x1b_Ggiest-apc-probe\x1b\\"),
            "passthrough is on but APC still did not arrive. Got: {text:?}"
        );
        return;
    }
    assert!(
        !contains(&out, b"giest-apc-probe"),
        "ConPTY now passes APC through — kitty graphics may be unblocked. Got: {text:?}"
    );
}

#[test]
#[ignore = "spawns Git for Windows' bash; run with --ignored --nocapture"]
fn ghosttys_bash_integration_injects_through_the_wsl_bootstrap() {
    // No WSL distro is needed to exercise the *bash* half of the WSL story:
    // Git for Windows ships a real bash and a POSIX sh, so the bootstrap
    // (`giest-wsl.sh`) and upstream's `ghostty.bash` run exactly as they would
    // inside WSL — only the path translation WSLENV's `/p` does is replaced by
    // spelling the MSYS path directly.
    let sh = r"C:\Program Files\Git\usr\bin\sh.exe";
    if !std::path::Path::new(sh).is_file() {
        eprintln!("skipping: Git for Windows not installed");
        return;
    }
    let root = std::env::temp_dir().join(format!("giest-si-bash-{}", std::process::id()));
    let dir = giest::profiles::extract_shell_integration(&root).expect("extract scripts");
    let win = dir.to_string_lossy().replace('\\', "/");
    let msys = format!("/{}{}", win[..1].to_ascii_lowercase(), &win[2..]);
    let env = vec![
        ("SHELL".to_string(), "/usr/bin/bash".to_string()),
        ("GIEST_SHELL_INTEGRATION_DIR".to_string(), msys),
        ("GHOSTTY_SHELL_FEATURES".to_string(), "cursor:blink,title".to_string()),
    ];
    let args = vec![
        "-c".to_string(),
        r#"exec /bin/sh "$GIEST_SHELL_INTEGRATION_DIR/giest-wsl.sh""#.to_string(),
    ];
    select_mode();
    let mut pty = Pty::spawn(sh, &args, None, &env, 80, 24, || {}).expect("spawn sh");
    let steps: [(&[u8], &[u8]); 2] = [
        (b"\x1b]133;A", b"echo giest-bash-ok; false\r"),
        (b"giest-bash-ok\r\n", b"exit\r"),
    ];
    let mut out = Vec::new();
    let started = Instant::now();
    let mut step = 0;
    while started.elapsed() < DEADLINE {
        match pty.output.recv_timeout(QUIET) {
            Ok(chunk) => {
                if contains(&chunk, b"\x1b[6n") {
                    let _ = pty.write(b"\x1b[1;1R");
                }
                out.extend_from_slice(&chunk);
                if step < steps.len() && contains(&out, steps[step].0) {
                    let _ = pty.write(steps[step].1);
                    step += 1;
                }
            }
            Err(RecvTimeoutError::Timeout) if !pty.is_running() => break,
            Err(RecvTimeoutError::Timeout) => {}
            Err(RecvTimeoutError::Disconnected) => break,
        }
    }
    let _ = std::fs::remove_dir_all(&root);
    let text = String::from_utf8_lossy(&out);
    assert!(contains(&out, b"\x1b]133;A"), "ghostty.bash never marked a prompt: {text:?}");
    assert!(contains(&out, b"\x1b]133;C"), "no pre-exec C mark: {text:?}");
    assert!(contains(&out, b"\x1b]133;D;1"), "`false` did not report exit 1: {text:?}");
    assert!(contains(&out, b"\x1b[5 q"), "no bar cursor at the prompt: {text:?}");
    assert!(contains(&out, b"\x1b]7;kitty-shell-cwd://"), "no cwd report: {text:?}");
}
