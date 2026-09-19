//! Live default-terminal handoff, end to end. **Ignored**: it briefly makes
//! giest the Windows default terminal for the current user.
//!
//!   pwsh scripts/build-handoff-proxy.ps1
//!   cargo build --release
//!   cargo test --test default_terminal -- --ignored --nocapture
//!
//! Needs Windows Terminal installed (its packaged OpenConsole is the console
//! half of the chain, see `src/handoff.rs`) and **no giest running** — the
//! COM-launched `giest -Embedding` would otherwise hand the session to it.
//!
//! What it proves: conhost delegates to OpenConsole, OpenConsole activates our
//! HKCU-registered CLSID through our proxy/stub, `EstablishPtyHandoff`
//! delivers working pipes (the client's `title` reaches the engine as OSC 2 —
//! output works), `+input` reaches the client (input works: `exit` ends it),
//! and the pane is reaped when the client exits (exit detection works).
//!
//! The delegation is live only from `+register-default-terminal` until the
//! handed-off window is seen; a `Drop` guard runs `+unregister-default-
//! terminal` on every path (panic included) and then checks the registry is
//! byte-for-byte what it was, force-restoring the `%%Startup` values if not.

#![cfg(windows)]

use std::os::windows::process::CommandExt;
use std::process::{Command, Output};
use std::time::{Duration, Instant};

/// The **release** exe: a debug build is console-subsystem and cannot be the
/// COM server (see `handoff::check_gui_subsystem`).
const GIEST: &str = concat!(env!("CARGO_MANIFEST_DIR"), r"\target\release\giest.exe");
const CREATE_NO_WINDOW: u32 = 0x0800_0000;
const STARTUP: &str = r"HKCU\Console\%%Startup";

/// Every key registration can touch, as `reg query` prints it.
const WATCHED: [&str; 6] = [
    STARTUP,
    r"HKCU\Software\giest",
    r"HKCU\Software\Classes\CLSID\{2CED21A9-5236-4F72-B71F-10F3949295E5}",
    r"HKCU\Software\Classes\CLSID\{4CDF6A34-42C2-488C-84D2-4BC3F55F519D}",
    r"HKCU\Software\Classes\Interface\{6F23DA90-15C5-4203-9DB0-64E73F1B1B00}",
    r"HKCU\Software\Classes\Interface\{E686C757-9A35-4A1C-B3CE-0BCC8B5C69F4}",
];

fn run(cmd: &mut Command) -> Output {
    cmd.output().expect("run")
}

fn text(o: &Output) -> String {
    format!("{}{}", String::from_utf8_lossy(&o.stdout), String::from_utf8_lossy(&o.stderr))
}

fn snapshot() -> String {
    WATCHED
        .iter()
        .map(|k| format!("== {k}\n{}", text(&run(Command::new("reg").args(["query", k, "/s"])))))
        .collect()
}

/// `(name, data)` of a REG_SZ under `%%Startup`, `None` when absent.
fn startup_value(name: &str) -> Option<String> {
    let o = run(Command::new("reg").args(["query", STARTUP, "/v", name]));
    let t = String::from_utf8_lossy(&o.stdout).into_owned();
    t.lines()
        .find(|l| l.trim_start().starts_with(name))
        .and_then(|l| l.split("REG_SZ").nth(1))
        .map(|v| v.trim().to_string())
}

struct Guard {
    before: String,
    prior: [(&'static str, Option<String>); 2],
    client: Option<std::process::Child>,
}

impl Drop for Guard {
    fn drop(&mut self) {
        let o = run(Command::new(GIEST).arg("+unregister-default-terminal"));
        eprintln!("unregister: {}", text(&o).trim());
        let after = snapshot();
        if after != self.before {
            eprintln!("REGISTRY DIFFERS after unregister; force-restoring %%Startup\n{after}");
            for (name, v) in &self.prior {
                match v {
                    Some(v) => {
                        run(Command::new("reg").args(["add", STARTUP, "/v", name, "/t", "REG_SZ", "/d", v, "/f"]));
                    }
                    None => {
                        run(Command::new("reg").args(["delete", STARTUP, "/v", name, "/f"]));
                    }
                }
            }
        }
        if let Some(c) = &mut self.client {
            let _ = c.kill();
        }
        // A handed-off giest still up (a failed assertion): end it. Safe to
        // match by name: the test refuses to start while any giest runs.
        run(Command::new("taskkill").args(["/F", "/IM", "giest.exe"]));
        assert_eq!(snapshot(), self.before, "registry not restored exactly");
        eprintln!("registry restored exactly");
    }
}

fn giest_running() -> bool {
    let o = run(Command::new("tasklist").args(["/FI", "IMAGENAME eq giest.exe", "/NH"]));
    String::from_utf8_lossy(&o.stdout).contains("giest.exe")
}

#[test]
#[ignore = "registers giest as the default terminal (HKCU) for a few seconds"]
fn console_launch_is_handed_to_giest() {
    assert!(!giest_running(), "close every giest first: the handoff would go to it");
    let before = snapshot();
    eprintln!("before:\n{before}");
    let mut guard = Guard {
        before,
        prior: [
            ("DelegationConsole", startup_value("DelegationConsole")),
            ("DelegationTerminal", startup_value("DelegationTerminal")),
        ],
        client: None,
    };

    let o = run(Command::new(GIEST).arg("+register-default-terminal"));
    eprintln!("register: {}", text(&o).trim());
    assert!(o.status.success());

    let marker = format!("HANDOFF_OK_{}", std::process::id());
    // `start` gives the inner cmd a *console* for its std handles. Spawning
    // cmd directly would hand it this test's inherited pipes (Rust always
    // sets STARTF_USESTDHANDLES): its stdin would be at EOF and it would exit
    // at once, which looks exactly like a working exit path.
    guard.client = Some(
        Command::new("cmd.exe")
            // `start`'s window title must be quoted, or it is the program.
            .raw_arg(format!("/c start \"handoff\" cmd.exe /k title {marker}"))
            .creation_flags(CREATE_NO_WINDOW)
            .spawn()
            .expect("spawn cmd"),
    );

    // Output: the handed-off pane shows the client's title.
    let listed = wait_listed(&marker, 15);
    // Delegation has done its job: restore the user's default right away.
    let o = run(Command::new(GIEST).arg("+unregister-default-terminal"));
    eprintln!("early unregister: {}", text(&o).trim());
    eprintln!("+list: {listed}");
    assert!(listed.contains(&marker), "no handed-off pane titled {marker}");

    // Input: a typed command runs in the client and its effect comes back.
    let typed = format!("TYPED_{}", std::process::id());
    let o = run(Command::new(GIEST).arg(format!("+input=title {typed}\r")));
    assert!(o.status.success(), "{}", text(&o));
    assert!(wait_listed(&typed, 10).contains(&typed), "typed input never ran in the client");

    // Exit detection: `exit` ends cmd, the pane is reaped, the instance goes.
    // The reply is not asserted: the instance can exit before writing it.
    let o = run(Command::new(GIEST).arg("+input=exit\r"));
    eprintln!("+input exit: {:?} {}", o.status, text(&o).trim());
    let t0 = Instant::now();
    while t0.elapsed() < Duration::from_secs(10) && giest_running() {
        std::thread::sleep(Duration::from_millis(300));
    }
    assert!(!giest_running(), "giest stayed up after the client exited");
}

/// Poll `+list` until it mentions `needle`, or `secs` pass; the last listing.
fn wait_listed(needle: &str, secs: u64) -> String {
    let t0 = Instant::now();
    let mut listed = String::new();
    while t0.elapsed() < Duration::from_secs(secs) {
        listed = text(&run(Command::new(GIEST).arg("+list")));
        if listed.contains(needle) {
            break;
        }
        std::thread::sleep(Duration::from_millis(300));
    }
    listed
}
