//! The terminal half of default-terminal handoff, live, **without** touching
//! how consoles launch. Ignored: it registers giest's COM class and proxy/stub
//! under `HKCU\Software\Classes` for the test's duration (never
//! `Console\%%Startup`), restoring them from a `Drop` guard.
//!
//!   pwsh scripts/build-handoff-proxy.ps1
//!   cargo build --release
//!   cargo test --test handoff_com -- --ignored --nocapture
//!
//! The test plays OpenConsole: it `CoCreateInstance`s giest's CLSID (COM
//! starts `giest.exe -Embedding`), calls `ITerminalHandoff3::
//! EstablishPtyHandoff` through the MIDL proxy with a signal pipe, a reference
//! handle, a server and a client process, and then checks each direction:
//! the initial resize arrives on the signal pipe, output written to the
//! returned `out` pipe reaches the engine (an OSC 2 title shows in `+list`),
//! `+input` arrives on the returned `in` pipe, and killing the client makes
//! giest reap the pane and exit. Needs no giest running (it talks to the
//! default IPC pipe, where the `-Embedding` instance serves).

#![cfg(windows)]

use std::ffi::c_void;
use std::io::{Read, Write};
use std::os::windows::io::{AsRawHandle, FromRawHandle, OwnedHandle};
use std::os::windows::process::CommandExt;
use std::process::{Command, Output};
use std::sync::mpsc::channel;
use std::time::{Duration, Instant};

/// The **release** exe: a debug build is console-subsystem and cannot be the
/// COM server (see `handoff::check_gui_subsystem`).
const GIEST: &str = concat!(env!("CARGO_MANIFEST_DIR"), r"\target\release\giest.exe");
type Handle = *mut c_void;

#[repr(C)]
struct Guid(u32, u16, u16, [u8; 8]);
const CLSID_TERMINAL: Guid = Guid(0x2ced_21a9, 0x5236, 0x4f72, [0xb7, 0x1f, 0x10, 0xf3, 0x94, 0x92, 0x95, 0xe5]);
const IID_ITERMINALHANDOFF3: Guid =
    Guid(0x6f23_da90, 0x15c5, 0x4203, [0x9d, 0xb0, 0x64, 0xe7, 0x3f, 0x1b, 0x1b, 0x00]);

#[repr(C)]
struct StartupInfo {
    title: *const u16,
    icon_path: *const u16,
    icon_index: i32,
    rest: [u32; 8],
    show_window: u16,
}

#[repr(C)]
struct Vtbl {
    qi: usize,
    add_ref: usize,
    release: unsafe extern "system" fn(*mut c_void) -> u32,
    establish: unsafe extern "system" fn(
        *mut c_void,
        *mut Handle,
        *mut Handle,
        Handle,
        Handle,
        Handle,
        Handle,
        *const StartupInfo,
    ) -> i32,
}

#[link(name = "ole32")]
unsafe extern "system" {
    fn CoInitializeEx(r: *mut c_void, c: u32) -> i32;
    fn CoCreateInstance(c: *const Guid, o: *mut c_void, ctx: u32, i: *const Guid, out: *mut *mut c_void) -> i32;
}
#[link(name = "oleaut32")]
unsafe extern "system" {
    fn SysAllocString(s: *const u16) -> *mut u16;
}
#[link(name = "kernel32")]
unsafe extern "system" {
    fn CreatePipe(r: *mut Handle, w: *mut Handle, sa: *const c_void, n: u32) -> i32;
    fn GetCurrentProcess() -> Handle;
}

const WATCHED: [&str; 4] = [
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
fn giest_running() -> bool {
    let o = run(Command::new("tasklist").args(["/FI", "IMAGENAME eq giest.exe", "/NH"]));
    String::from_utf8_lossy(&o.stdout).contains("giest.exe")
}

struct Guard {
    before: String,
}
impl Drop for Guard {
    fn drop(&mut self) {
        let r = giest::handoff::unregister_com_only();
        eprintln!("unregister_com_only: {r:?}");
        run(Command::new("taskkill").args(["/F", "/IM", "giest.exe"]));
        assert_eq!(snapshot(), self.before, "registry not restored exactly");
        eprintln!("registry restored exactly");
    }
}

/// Read from `h` on a thread until `pred` holds or `secs` pass.
fn read_until(h: OwnedHandle, secs: u64, pred: impl Fn(&[u8]) -> bool + Send + 'static) -> Vec<u8> {
    let (tx, rx) = channel();
    std::thread::spawn(move || {
        let mut f = std::fs::File::from(h);
        let mut got = Vec::new();
        let mut buf = [0u8; 4096];
        while let Ok(n) = f.read(&mut buf) {
            if n == 0 {
                break;
            }
            got.extend_from_slice(&buf[..n]);
            if pred(&got) {
                break;
            }
        }
        let _ = tx.send(got);
    });
    rx.recv_timeout(Duration::from_secs(secs)).unwrap_or_default()
}

fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(Some(0)).collect()
}

#[test]
#[ignore = "registers giest's COM class under HKCU for the test's duration"]
fn giest_accepts_a_pty_handoff_over_com() {
    run_parent(MODE_STANDALONE);
}

/// With a giest already running, the `-Embedding` process forwards the
/// session over IPC (`Request::Handoff`): the running instance duplicates the
/// handles in and shows it as a new tab; the client exiting closes that tab
/// only.
#[test]
#[ignore = "registers giest's COM class under HKCU and starts a giest"]
fn a_running_instance_adopts_the_handoff() {
    run_parent(MODE_FORWARDED);
}

const MODE_STANDALONE: &str = "standalone";
const MODE_FORWARDED: &str = "forwarded";

/// The two parent tests share one CLSID registration and the IPC pipe name.
static SERIAL: std::sync::Mutex<()> = std::sync::Mutex::new(());

fn run_parent(mode: &str) {
    let _serial = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    assert!(!giest_running(), "close every giest first");
    let before = snapshot();
    let _guard = Guard { before };
    giest::handoff::register_com_only(std::path::Path::new(GIEST)).expect("register");
    // Declared after the guard, so it is dropped (killed) before it.
    let _first = KillOnDrop((mode == MODE_FORWARDED).then(|| Command::new(GIEST).spawn().expect("start giest")));
    if mode == MODE_FORWARDED {
        let t0 = Instant::now();
        while !run(Command::new(GIEST).arg("+list")).status.success() {
            assert!(t0.elapsed() < Duration::from_secs(20), "the first giest never served IPC");
            std::thread::sleep(Duration::from_millis(300));
        }
    }
    // The COM half runs in a child process: a crash there (an access
    // violation in a proxy skips every `Drop`) must not skip the guard here.
    let st = Command::new(std::env::current_exe().unwrap())
        .args(["--ignored", "--exact", "com_child", "--nocapture"])
        .env(CHILD_ENV, mode)
        .status()
        .unwrap();
    assert!(st.success(), "COM child failed: {st:?}");
}

/// Kills the stand-in client however the child ends.
struct KillOnDrop(Option<std::process::Child>);
impl Drop for KillOnDrop {
    fn drop(&mut self) {
        if let Some(c) = &mut self.0 {
            let _ = c.kill();
            let _ = c.wait();
        }
    }
}

const CHILD_ENV: &str = "GIEST_HANDOFF_COM_CHILD";

/// The OpenConsole side; only does anything when spawned by the test above.
#[test]
#[ignore = "child half of giest_accepts_a_pty_handoff_over_com"]
fn com_child() {
    let Ok(mode) = std::env::var(CHILD_ENV) else {
        return;
    };
    let mut guard = KillOnDrop(None);
    // SAFETY (whole block): plain Win32/COM calls with valid arguments.
    unsafe {
        assert!(CoInitializeEx(std::ptr::null_mut(), 0) >= 0);
        let mut obj: *mut c_void = std::ptr::null_mut();
        let hr = CoCreateInstance(&CLSID_TERMINAL, std::ptr::null_mut(), 0x4, &IID_ITERMINALHANDOFF3, &mut obj);
        assert_eq!(hr, 0, "CoCreateInstance: {hr:#010x}");
        let vt = &**(obj as *const *const Vtbl);

        let (mut sig_r, mut sig_w): (Handle, Handle) = (std::ptr::null_mut(), std::ptr::null_mut());
        assert!(CreatePipe(&mut sig_r, &mut sig_w, std::ptr::null(), 0) != 0);
        let sig_r = OwnedHandle::from_raw_handle(sig_r);
        let sig_w = OwnedHandle::from_raw_handle(sig_w);
        let reference = std::fs::File::open(GIEST).unwrap();
        // A stand-in client that just stays alive. (Not `ping`: spawned from
        // here it exits at once with code 1, which silently made the exit
        // checks below pass for the wrong reason.)
        let client = Command::new("powershell.exe")
            .args(["-NoProfile", "-NonInteractive", "-Command", "Start-Sleep -Seconds 120"])
            .creation_flags(0x0800_0000) // CREATE_NO_WINDOW
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .spawn()
            .unwrap();
        let client_h = client.as_raw_handle();
        guard.0 = Some(client);

        let title = wide("COM_TITLE");
        let info = StartupInfo {
            title: SysAllocString(title.as_ptr()),
            icon_path: std::ptr::null(),
            icon_index: 0,
            rest: [0; 8],
            show_window: 1,
        };
        let (mut in_, mut out): (Handle, Handle) = (std::ptr::null_mut(), std::ptr::null_mut());
        let hr = (vt.establish)(
            obj,
            &mut in_,
            &mut out,
            sig_w.as_raw_handle(),
            reference.as_raw_handle(),
            GetCurrentProcess(),
            client_h,
            &info,
        );
        assert_eq!(hr, 0, "EstablishPtyHandoff: {hr:#010x}");
        (vt.release)(obj);
        drop(sig_w);
        let in_ = OwnedHandle::from_raw_handle(in_);
        let mut out = std::fs::File::from(OwnedHandle::from_raw_handle(out));

        // 1. The first resize arrives on the signal pipe.
        let sig = read_until(sig_r, 20, |b| b.len() >= 6);
        eprintln!("signal: {sig:?}");
        assert!(sig.len() >= 6 && sig[0..2] == [8, 0], "no PTY_SIGNAL_RESIZE_WINDOW: {sig:?}");

        // 2. Output reaches the engine.
        out.write_all(b"\x1b]2;COM_OK\x07hello from the handoff\r\n").unwrap();
        let t0 = Instant::now();
        let mut listed = String::new();
        while t0.elapsed() < Duration::from_secs(15) {
            listed = text(&run(Command::new(GIEST).arg("+list")));
            if listed.contains("COM_OK") {
                break;
            }
            std::thread::sleep(Duration::from_millis(300));
        }
        eprintln!("+list: {listed}");
        assert!(listed.contains("COM_OK"), "title never reached the pane");

        // 3. Input reaches the pipe.
        let o = run(Command::new(GIEST).arg("+input=abc"));
        assert!(o.status.success(), "{}", text(&o));
        let got = read_until(in_, 10, |b| b.windows(3).any(|w| w == b"abc"));
        eprintln!("input pipe: {:?}", String::from_utf8_lossy(&got));
        assert!(got.windows(3).any(|w| w == b"abc"), "typed input never arrived");

        if mode == MODE_FORWARDED {
            // Forwarded: one instance, two tabs; the `-Embedding` process
            // has handed over and gone.
            assert_eq!(listed.matches("\"tabs\"").count(), 1, "expected one window");
            assert!(listed.matches("\"active\"").count() >= 2, "expected a second tab");
            let n = run(Command::new("tasklist").args(["/FI", "IMAGENAME eq giest.exe", "/NH"]));
            assert_eq!(String::from_utf8_lossy(&n.stdout).matches("giest.exe").count(), 1);
        }

        // 4. The client exiting reaps the pane; standalone, the instance too.
        // First: nothing may have gone away early.
        assert!(guard.0.as_mut().unwrap().try_wait().unwrap().is_none(), "stand-in client died");
        assert!(listed.contains("COM_OK") && giest_running(), "pane gone before the client exited");
        // Outlive `abnormal-command-exit-runtime` (250 ms): a client killed
        // sooner is held open as "failed to launch" by design, not reaped.
        std::thread::sleep(Duration::from_millis(600));
        let _ = guard.0.as_mut().unwrap().kill();
        let t0 = Instant::now();
        if mode == MODE_FORWARDED {
            while t0.elapsed() < Duration::from_secs(10)
                && text(&run(Command::new(GIEST).arg("+list"))).contains("COM_OK")
            {
                std::thread::sleep(Duration::from_millis(300));
            }
            let listed = text(&run(Command::new(GIEST).arg("+list")));
            eprintln!("+list after the client exited: {listed}");
            assert!(!listed.contains("COM_OK"), "the handed-off tab was not reaped");
            assert!(giest_running(), "the running instance went away with the tab");
        } else {
            while t0.elapsed() < Duration::from_secs(10) && giest_running() {
                std::thread::sleep(Duration::from_millis(300));
            }
            assert!(!giest_running(), "giest stayed up after the client exited");
        }
        drop(out);
    }
}
