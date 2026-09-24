//! Single-instance IPC: a per-user named pipe carrying JSON lines.
//!
//! This is geist's analogue of the macOS app's App Intents / AppleScript /
//! Services entry points and of GTK's D-Bus activation: a second `geist`
//! launch (`+new-tab`, Explorer's "Open geist here", a Jump List task, a
//! script) hands its request to the running instance and exits.
//!
//! **Wire format.** One request per connection: the client writes one JSON
//! object terminated by `\n`, the server answers with one JSON object and a
//! `\n`, then closes. Every message carries `"v"`, the protocol version; a
//! server refuses a newer major version rather than guessing at it. Unknown
//! *fields* are ignored, so a request may grow optional fields without a bump.
//!
//! ```text
//! -> {"v":1,"type":"new_tab","cwd":"C:\\src","command":"pwsh"}
//! <- {"v":1,"ok":true,"window":0,"tab":7,"pane":7}
//! -> {"v":1,"type":"list"}
//! <- {"v":1,"ok":true,"windows":[{"id":0,"title":"…","focused":true,"tabs":[…]}]}
//! ```
//!
//! Requests: `new_window`, `new_tab`, `focus`, `input_text`, `run_action`,
//! `list`. `command` is either a string (resolved like the `command` config key:
//! a profile name, else a program) or an argv array. Ids are the stable ids the
//! app uses internally — window id, tab id, pane id — never list positions, so
//! an id read from `list` still names the same thing after a reorder.
//! `input_text` is a **paste**: it goes through `Session::paste_str`, so
//! `clipboard-paste-protection` applies to it exactly as to Ctrl+V.
//!
//! **Security.** The pipe name embeds the user's SID and the logon session, its
//! DACL grants access to that SID (and SYSTEM) only, remote clients are
//! rejected, and the first instance is created with
//! `FILE_FLAG_FIRST_PIPE_INSTANCE` so nobody can slip a second server under
//! the same name. The client additionally checks that the server process runs
//! as the same user before sending anything — a pipe name is public, so a
//! squatter that won the race must not receive our working directory.
//!
//! **Threading.** The server's accept loop and the per-connection readers run
//! on their own threads; `Session` is `!Send`, so requests are handed to the UI
//! thread over a channel and the root viewport is woken (a root pass is the one
//! that runs every window — the same reason the PTY wake targets ROOT). The
//! connection thread waits for the UI's answer with a timeout.

use serde::{Deserialize, Serialize};

/// The protocol version this build speaks.
pub const PROTOCOL_VERSION: u32 = 1;

/// Largest request accepted, in bytes — `input_text` can be a big paste, but a
/// client streaming forever must not exhaust memory.
pub const MAX_REQUEST_BYTES: usize = 4 << 20;

/// A command to run in a new surface.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum CommandSpec {
    /// Resolved like the `command` config key: a profile name, else a program.
    Line(String),
    /// An explicit argv (what `-e` produces).
    Argv(Vec<String>),
}

/// One request. Tagged by `"type"`.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Request {
    NewWindow {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        cwd: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        command: Option<CommandSpec>,
    },
    NewTab {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        cwd: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        command: Option<CommandSpec>,
        /// The window to add the tab to; the last-focused one when absent.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        window: Option<u64>,
    },
    /// Bring a window (and optionally a tab / pane within it) forward.
    Focus {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        window: Option<u64>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        tab: Option<u64>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        pane: Option<u64>,
    },
    /// Paste `text` into a pane (the focused one when absent).
    InputText {
        text: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        window: Option<u64>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        pane: Option<u64>,
    },
    /// Run a keybind action string (`new_split:right`, `toggle_fullscreen`, …).
    RunAction {
        action: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        window: Option<u64>,
    },
    List,
    /// Adopt a default-terminal handoff that the `geist -Embedding` process
    /// `pid` received. `handles` are valid in *that* process; the server
    /// duplicates them in, and the sender closes its copies once answered.
    Handoff {
        pid: u32,
        handles: crate::handoff::RawHandles,
        #[serde(default)]
        title: String,
        #[serde(default)]
        show_window: u16,
    },
}

#[derive(Serialize, Deserialize)]
struct Envelope {
    v: u32,
    #[serde(flatten)]
    request: Request,
}

/// A pane in a `list` reply.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct PaneInfo {
    pub id: u64,
    pub title: String,
    pub focused: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cwd: Option<String>,
}

/// A tab in a `list` reply.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct TabInfo {
    pub id: u64,
    pub title: String,
    pub active: bool,
    pub panes: Vec<PaneInfo>,
}

/// A window in a `list` reply.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct WindowInfo {
    pub id: u64,
    pub title: String,
    pub focused: bool,
    pub tabs: Vec<TabInfo>,
}

/// The answer to one request.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, Default)]
pub struct Response {
    pub v: u32,
    pub ok: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub window: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tab: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pane: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub windows: Option<Vec<WindowInfo>>,
}

impl Response {
    pub fn ok() -> Self {
        Self {
            v: PROTOCOL_VERSION,
            ok: true,
            ..Default::default()
        }
    }

    pub fn err(msg: impl Into<String>) -> Self {
        Self {
            v: PROTOCOL_VERSION,
            ok: false,
            error: Some(msg.into()),
            ..Default::default()
        }
    }
}

/// Serialize a request as one wire line (with the trailing `\n`).
pub fn encode_request(r: &Request) -> String {
    let env = Envelope {
        v: PROTOCOL_VERSION,
        request: r.clone(),
    };
    let mut s = serde_json::to_string(&env).expect("request serializes");
    s.push('\n');
    s
}

/// Parse one wire line into a request, checking the version first so a newer
/// client gets a precise error rather than a confusing field complaint.
pub fn decode_request(line: &str) -> Result<Request, String> {
    let value: serde_json::Value =
        serde_json::from_str(line.trim()).map_err(|e| format!("malformed JSON: {e}"))?;
    match value.get("v").and_then(serde_json::Value::as_u64) {
        None => return Err("missing protocol version \"v\"".into()),
        Some(v) if v > u64::from(PROTOCOL_VERSION) => {
            return Err(format!(
                "unsupported protocol version {v} (this geist speaks {PROTOCOL_VERSION})"
            ));
        }
        Some(_) => {}
    }
    serde_json::from_value::<Envelope>(value)
        .map(|e| e.request)
        .map_err(|e| format!("bad request: {e}"))
}

pub fn encode_response(r: &Response) -> String {
    let mut s = serde_json::to_string(r).expect("response serializes");
    s.push('\n');
    s
}

pub fn decode_response(line: &str) -> Result<Response, String> {
    serde_json::from_str(line.trim()).map_err(|e| format!("malformed response: {e}"))
}

/// Read one `\n`-terminated line, refusing more than `max` bytes. EOF before a
/// newline still yields what arrived (a client may close instead of sending
/// `\n`).
pub fn read_line_capped(r: &mut impl std::io::Read, max: usize) -> std::io::Result<String> {
    let mut buf = Vec::new();
    let mut chunk = [0u8; 4096];
    loop {
        let n = r.read(&mut chunk)?;
        if n == 0 {
            break;
        }
        if let Some(i) = chunk[..n].iter().position(|&b| b == b'\n') {
            buf.extend_from_slice(&chunk[..i]);
            break;
        }
        buf.extend_from_slice(&chunk[..n]);
        if buf.len() > max {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "request too large",
            ));
        }
    }
    String::from_utf8(buf)
        .map_err(|_| std::io::Error::new(std::io::ErrorKind::InvalidData, "request is not UTF-8"))
}

/// A request waiting for the UI thread, with the channel its answer goes back
/// on.
pub struct Incoming {
    pub request: Request,
    reply: std::sync::mpsc::Sender<Response>,
}

impl Incoming {
    pub fn respond(self, r: Response) {
        let _ = self.reply.send(r);
    }
}

use std::sync::mpsc::{Receiver, Sender};
use std::sync::{Mutex, OnceLock};

static INCOMING: Mutex<Option<Receiver<Incoming>>> = Mutex::new(None);
static WAKER: OnceLock<eframe::egui::Context> = OnceLock::new();

/// Give the server a way to wake the UI. Requests that arrive before this is
/// set simply wait in the channel for the first frame.
pub fn set_waker(ctx: &eframe::egui::Context) {
    let _ = WAKER.set(ctx.clone());
}

fn wake() {
    if let Some(ctx) = WAKER.get() {
        // ROOT, deliberately: only a root pass runs every window.
        ctx.request_repaint_of(eframe::egui::ViewportId::ROOT);
    }
}

/// Take the UI end of the request channel. `None` when this process isn't
/// serving (single instance off, or another instance owns the pipe).
pub fn take_receiver() -> Option<Receiver<Incoming>> {
    INCOMING.lock().ok()?.take()
}

/// Handle one connection: read a request, hand it to the UI, write the reply.
fn serve_one(mut conn: impl std::io::Read + std::io::Write, tx: &Sender<Incoming>) {
    let reply = match read_line_capped(&mut conn, MAX_REQUEST_BYTES) {
        Err(e) => Response::err(e.to_string()),
        Ok(line) => match decode_request(&line) {
            Err(e) => Response::err(e),
            Ok(request) => {
                let (rtx, rrx) = std::sync::mpsc::channel();
                if tx
                    .send(Incoming {
                        request,
                        reply: rtx,
                    })
                    .is_err()
                {
                    Response::err("geist is shutting down")
                } else {
                    wake();
                    // Opening a window spawns a shell, which can take a
                    // moment; a UI thread that never answers (a modal system
                    // dialog, a hang) must not hold the client forever.
                    rrx.recv_timeout(std::time::Duration::from_secs(20))
                        .unwrap_or_else(|_| Response::err("timed out waiting for geist"))
                }
            }
        },
    };
    let _ = conn.write_all(encode_response(&reply).as_bytes());
    let _ = conn.flush();
}

/// The outcome of trying to reach a running instance.
#[derive(Debug)]
pub enum SendError {
    /// Nothing is listening: start locally.
    NoServer,
    /// Something is listening but the exchange failed.
    Failed(String),
}

#[cfg(windows)]
mod imp {
    use std::ffi::c_void;
    use std::fs::File;
    use std::io::{Read, Write};
    use std::os::windows::ffi::OsStrExt;
    use std::os::windows::io::{AsRawHandle, FromRawHandle};
    use std::sync::mpsc::Sender;

    use super::{Incoming, Request, Response, SendError};

    type Handle = *mut c_void;
    const INVALID_HANDLE_VALUE: Handle = -1isize as Handle;

    const PIPE_ACCESS_DUPLEX: u32 = 0x3;
    const FILE_FLAG_FIRST_PIPE_INSTANCE: u32 = 0x0008_0000;
    const PIPE_TYPE_BYTE: u32 = 0x0;
    const PIPE_READMODE_BYTE: u32 = 0x0;
    const PIPE_WAIT: u32 = 0x0;
    const PIPE_REJECT_REMOTE_CLIENTS: u32 = 0x8;
    const PIPE_UNLIMITED_INSTANCES: u32 = 255;
    const ERROR_PIPE_CONNECTED: i32 = 535;
    const ERROR_PIPE_BUSY: i32 = 231;
    const ERROR_FILE_NOT_FOUND: i32 = 2;
    const TOKEN_QUERY: u32 = 0x8;
    const TOKEN_USER_CLASS: u32 = 1;
    const PROCESS_QUERY_LIMITED_INFORMATION: u32 = 0x1000;
    const SDDL_REVISION_1: u32 = 1;

    #[repr(C)]
    struct SecurityAttributes {
        n_length: u32,
        security_descriptor: *mut c_void,
        inherit_handle: i32,
    }

    #[link(name = "kernel32")]
    unsafe extern "system" {
        fn CreateNamedPipeW(
            name: *const u16,
            open_mode: u32,
            pipe_mode: u32,
            max_instances: u32,
            out_buf: u32,
            in_buf: u32,
            timeout_ms: u32,
            sa: *const SecurityAttributes,
        ) -> Handle;
        fn ConnectNamedPipe(pipe: Handle, overlapped: *mut c_void) -> i32;
        fn WaitNamedPipeW(name: *const u16, timeout_ms: u32) -> i32;
        fn GetNamedPipeServerProcessId(pipe: Handle, pid: *mut u32) -> i32;
        fn ProcessIdToSessionId(pid: u32, session: *mut u32) -> i32;
        fn GetCurrentProcessId() -> u32;
        fn GetCurrentProcess() -> Handle;
        fn OpenProcess(access: u32, inherit: i32, pid: u32) -> Handle;
        fn CloseHandle(h: Handle) -> i32;
        fn LocalFree(p: *mut c_void) -> *mut c_void;
    }
    #[link(name = "advapi32")]
    unsafe extern "system" {
        fn OpenProcessToken(process: Handle, access: u32, token: *mut Handle) -> i32;
        fn GetTokenInformation(
            token: Handle,
            class: u32,
            info: *mut c_void,
            len: u32,
            ret_len: *mut u32,
        ) -> i32;
        fn ConvertSidToStringSidW(sid: *mut c_void, out: *mut *mut u16) -> i32;
        fn ConvertStringSecurityDescriptorToSecurityDescriptorW(
            sddl: *const u16,
            revision: u32,
            sd: *mut *mut c_void,
            sd_len: *mut u32,
        ) -> i32;
    }
    #[link(name = "user32")]
    unsafe extern "system" {
        fn AllowSetForegroundWindow(pid: u32) -> i32;
    }

    fn wide(s: &str) -> Vec<u16> {
        std::ffi::OsStr::new(s)
            .encode_wide()
            .chain(Some(0))
            .collect()
    }

    /// The string SID of the user a process runs as.
    fn process_user_sid(process: Handle) -> Option<String> {
        // SAFETY: standard token query with a correctly sized buffer; every
        // handle and allocation is released before returning.
        unsafe {
            let mut token: Handle = std::ptr::null_mut();
            if OpenProcessToken(process, TOKEN_QUERY, &mut token) == 0 {
                return None;
            }
            let mut len = 0u32;
            GetTokenInformation(token, TOKEN_USER_CLASS, std::ptr::null_mut(), 0, &mut len);
            // u64 storage keeps the SID_AND_ATTRIBUTES pointer aligned.
            let mut buf = vec![0u64; (len as usize).div_ceil(8).max(1)];
            let ok = GetTokenInformation(
                token,
                TOKEN_USER_CLASS,
                buf.as_mut_ptr() as *mut c_void,
                len,
                &mut len,
            );
            CloseHandle(token);
            if ok == 0 {
                return None;
            }
            // TOKEN_USER starts with SID_AND_ATTRIBUTES { PSID Sid; DWORD }.
            let sid = *(buf.as_ptr() as *const *mut c_void);
            let mut s: *mut u16 = std::ptr::null_mut();
            if ConvertSidToStringSidW(sid, &mut s) == 0 || s.is_null() {
                return None;
            }
            let mut n = 0;
            while *s.add(n) != 0 {
                n += 1;
            }
            let out = String::from_utf16_lossy(std::slice::from_raw_parts(s, n));
            LocalFree(s as *mut c_void);
            Some(out)
        }
    }

    fn current_user_sid() -> Option<String> {
        // SAFETY: the pseudo-handle needs no closing.
        process_user_sid(unsafe { GetCurrentProcess() })
    }

    /// `\\.\pipe\geist-<SID>-<session>`, or `$geist_IPC_PIPE` (tests and
    /// side-by-side builds).
    pub fn pipe_name() -> String {
        if let Ok(n) = std::env::var("geist_IPC_PIPE")
            && !n.trim().is_empty()
        {
            return format!(r"\\.\pipe\{}", n.trim());
        }
        let sid = current_user_sid()
            .unwrap_or_else(|| std::env::var("USERNAME").unwrap_or_else(|_| "user".into()));
        let mut session = 0u32;
        // SAFETY: valid out-pointer.
        unsafe {
            ProcessIdToSessionId(GetCurrentProcessId(), &mut session);
        }
        format!(r"\\.\pipe\geist-{sid}-{session}")
    }

    /// One pipe instance, restricted to this user (and SYSTEM).
    fn create_instance(name: &str, first: bool) -> std::io::Result<File> {
        let sddl = match current_user_sid() {
            Some(sid) => format!("D:P(A;;GA;;;{sid})(A;;GA;;;SY)"),
            None => "D:P(A;;GA;;;OW)(A;;GA;;;SY)".to_string(),
        };
        let sddl = wide(&sddl);
        let mut sd: *mut c_void = std::ptr::null_mut();
        // SAFETY: valid NUL-terminated SDDL and out-pointer; `sd` is freed below.
        let have_sd = unsafe {
            ConvertStringSecurityDescriptorToSecurityDescriptorW(
                sddl.as_ptr(),
                SDDL_REVISION_1,
                &mut sd,
                std::ptr::null_mut(),
            ) != 0
        };
        let sa = SecurityAttributes {
            n_length: size_of::<SecurityAttributes>() as u32,
            security_descriptor: if have_sd { sd } else { std::ptr::null_mut() },
            inherit_handle: 0,
        };
        let name = wide(name);
        let mode = PIPE_ACCESS_DUPLEX
            | if first {
                FILE_FLAG_FIRST_PIPE_INSTANCE
            } else {
                0
            };
        // SAFETY: all pointers are valid for the call.
        let h = unsafe {
            CreateNamedPipeW(
                name.as_ptr(),
                mode,
                PIPE_TYPE_BYTE | PIPE_READMODE_BYTE | PIPE_WAIT | PIPE_REJECT_REMOTE_CLIENTS,
                PIPE_UNLIMITED_INSTANCES,
                64 * 1024,
                64 * 1024,
                0,
                &sa,
            )
        };
        let err = std::io::Error::last_os_error();
        if have_sd {
            // SAFETY: allocated by ConvertStringSecurityDescriptor…W.
            unsafe {
                LocalFree(sd);
            }
        }
        if h == INVALID_HANDLE_VALUE {
            return Err(err);
        }
        // SAFETY: a fresh handle we own.
        Ok(unsafe { File::from_raw_handle(h) })
    }

    /// Claim the pipe name as the first instance. Fails if another geist (or
    /// anything else) already owns it.
    pub fn bind(name: &str) -> std::io::Result<File> {
        create_instance(name, true)
    }

    /// Run the accept loop on a background thread, starting from the instance
    /// [`bind`] claimed.
    pub fn serve(first: File, name: String, tx: Sender<Incoming>) {
        let _ = std::thread::Builder::new()
            .name("geist-ipc".into())
            .spawn(move || {
                let mut next = Some(first);
                loop {
                    let pipe = match next.take() {
                        Some(p) => p,
                        None => match create_instance(&name, false) {
                            Ok(p) => p,
                            Err(e) => {
                                eprintln!("geist: IPC pipe instance failed: {e}");
                                std::thread::sleep(std::time::Duration::from_secs(1));
                                continue;
                            }
                        },
                    };
                    // SAFETY: a valid pipe handle; blocking connect.
                    let ok = unsafe {
                        ConnectNamedPipe(pipe.as_raw_handle() as Handle, std::ptr::null_mut())
                    };
                    if ok == 0
                        && std::io::Error::last_os_error().raw_os_error()
                            != Some(ERROR_PIPE_CONNECTED)
                    {
                        continue;
                    }
                    // One thread per connection, so a client that connects and
                    // never writes can't block everyone behind it.
                    let tx = tx.clone();
                    let _ = std::thread::Builder::new()
                        .name("geist-ipc-conn".into())
                        .spawn(move || super::serve_one(pipe, &tx));
                }
            });
    }

    /// Open a client connection, retrying briefly while every instance is busy.
    fn connect(name: &str) -> Result<File, SendError> {
        let wname = wide(name);
        for _ in 0..20 {
            match std::fs::OpenOptions::new()
                .read(true)
                .write(true)
                .open(name)
            {
                Ok(f) => return Ok(f),
                Err(e) if e.raw_os_error() == Some(ERROR_FILE_NOT_FOUND) => {
                    return Err(SendError::NoServer);
                }
                Err(e) if e.raw_os_error() == Some(ERROR_PIPE_BUSY) => {
                    // SAFETY: valid NUL-terminated name.
                    unsafe {
                        WaitNamedPipeW(wname.as_ptr(), 500);
                    }
                }
                Err(e) => return Err(SendError::Failed(e.to_string())),
            }
        }
        Err(SendError::Failed("the running geist is busy".into()))
    }

    /// Send one request to the running instance and wait for its answer.
    pub fn send(name: &str, req: &Request) -> Result<Response, SendError> {
        let mut conn = connect(name)?;
        let mut pid = 0u32;
        // SAFETY: valid pipe handle and out-pointer.
        if unsafe { GetNamedPipeServerProcessId(conn.as_raw_handle() as Handle, &mut pid) } == 0 {
            return Err(SendError::Failed(
                "cannot identify the pipe's server".into(),
            ));
        }
        // A pipe name is guessable. Refuse to talk to a server that isn't us.
        // SAFETY: OpenProcess with a query-only right; closed below.
        let server_sid = unsafe {
            let h = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid);
            if h.is_null() {
                None
            } else {
                let s = process_user_sid(h);
                CloseHandle(h);
                s
            }
        };
        if server_sid.is_none() || server_sid != current_user_sid() {
            return Err(SendError::Failed(
                "the IPC pipe is owned by another user; refusing to use it".into(),
            ));
        }
        // We are the foreground process (the user just launched us); let the
        // server raise its window, which Windows otherwise forbids.
        // SAFETY: plain call.
        unsafe {
            AllowSetForegroundWindow(pid);
        }
        conn.write_all(super::encode_request(req).as_bytes())
            .map_err(|e| SendError::Failed(e.to_string()))?;
        let line = super::read_line_capped(&mut conn, super::MAX_REQUEST_BYTES)
            .map_err(|e| SendError::Failed(e.to_string()))?;
        let mut sink = Vec::new();
        let _ = conn.read_to_end(&mut sink);
        super::decode_response(&line).map_err(SendError::Failed)
    }
}

#[cfg(not(windows))]
mod imp {
    use super::{Incoming, Request, Response, SendError};
    use std::sync::mpsc::Sender;
    pub fn pipe_name() -> String {
        String::new()
    }
    pub fn bind(_name: &str) -> std::io::Result<std::fs::File> {
        Err(std::io::Error::new(
            std::io::ErrorKind::Unsupported,
            "no IPC",
        ))
    }
    pub fn serve(_first: std::fs::File, _name: String, _tx: Sender<Incoming>) {}
    pub fn send(_name: &str, _req: &Request) -> Result<Response, SendError> {
        Err(SendError::NoServer)
    }
}

pub use imp::{pipe_name, send};

/// Try to become the single instance: claim the pipe and start serving.
/// Returns `false` when another process already owns it.
pub fn start_server() -> bool {
    let name = pipe_name();
    match imp::bind(&name) {
        Ok(first) => {
            let (tx, rx) = std::sync::mpsc::channel();
            if let Ok(mut slot) = INCOMING.lock() {
                *slot = Some(rx);
            }
            imp::serve(first, name, tx);
            true
        }
        Err(_) => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn requests_round_trip_through_the_wire_format() {
        let cases = [
            Request::NewWindow {
                cwd: None,
                command: None,
            },
            Request::NewTab {
                cwd: Some(r"C:\src".into()),
                command: Some(CommandSpec::Line("pwsh".into())),
                window: Some(3),
            },
            Request::NewTab {
                cwd: None,
                command: Some(CommandSpec::Argv(vec![
                    "cmd.exe".into(),
                    "/k".into(),
                    "dir".into(),
                ])),
                window: None,
            },
            Request::Focus {
                window: Some(1),
                tab: Some(2),
                pane: Some(3),
            },
            Request::InputText {
                text: "echo hi\r\n".into(),
                window: None,
                pane: Some(4),
            },
            Request::RunAction {
                action: "new_split:right".into(),
                window: None,
            },
            Request::List,
        ];
        for r in cases {
            let line = encode_request(&r);
            assert!(
                line.ends_with('\n') && !line[..line.len() - 1].contains('\n'),
                "{line}"
            );
            assert_eq!(decode_request(&line).unwrap(), r, "{line}");
        }
    }

    #[test]
    fn the_documented_shapes_decode() {
        assert_eq!(
            decode_request(r#"{"v":1,"type":"new_tab","cwd":"C:\\src","command":"pwsh"}"#).unwrap(),
            Request::NewTab {
                cwd: Some(r"C:\src".into()),
                command: Some(CommandSpec::Line("pwsh".into())),
                window: None
            }
        );
        assert_eq!(
            decode_request(r#"{"v":1,"type":"list"}"#).unwrap(),
            Request::List
        );
        // Unknown fields are ignored: optional additions need no version bump.
        assert_eq!(
            decode_request(r#"{"v":1,"type":"list","future":true}"#).unwrap(),
            Request::List
        );
    }

    #[test]
    fn version_is_required_and_a_newer_one_is_refused() {
        assert!(
            decode_request(r#"{"type":"list"}"#)
                .unwrap_err()
                .contains("version")
        );
        let e = decode_request(r#"{"v":99,"type":"list"}"#).unwrap_err();
        assert!(e.contains("unsupported protocol version 99"), "{e}");
    }

    #[test]
    fn junk_and_unknown_types_are_errors_not_panics() {
        assert!(decode_request("not json").is_err());
        assert!(decode_request(r#"{"v":1,"type":"format_c"}"#).is_err());
        assert!(
            decode_request(r#"{"v":1,"type":"input_text"}"#).is_err(),
            "text is required"
        );
    }

    #[test]
    fn responses_round_trip_and_omit_empty_fields() {
        let r = Response::ok();
        let line = encode_response(&r);
        assert_eq!(line, "{\"v\":1,\"ok\":true}\n");
        let full = Response {
            window: Some(1),
            windows: Some(vec![WindowInfo {
                id: 1,
                title: "t".into(),
                focused: true,
                tabs: vec![TabInfo {
                    id: 2,
                    title: "x".into(),
                    active: true,
                    panes: vec![PaneInfo {
                        id: 2,
                        title: "x".into(),
                        focused: true,
                        cwd: None,
                    }],
                }],
            }]),
            ..Response::ok()
        };
        assert_eq!(decode_response(&encode_response(&full)).unwrap(), full);
        assert_eq!(
            decode_response(&encode_response(&Response::err("no")))
                .unwrap()
                .error
                .as_deref(),
            Some("no")
        );
    }

    #[test]
    fn line_reader_stops_at_newline_and_caps_size() {
        let mut r: &[u8] = b"{\"a\":1}\nrest";
        assert_eq!(read_line_capped(&mut r, 100).unwrap(), "{\"a\":1}");
        let mut eof: &[u8] = b"no newline";
        assert_eq!(read_line_capped(&mut eof, 100).unwrap(), "no newline");
        let big = vec![b'x'; 10_000];
        assert!(read_line_capped(&mut big.as_slice(), 100).is_err());
    }

    #[test]
    fn serve_one_answers_bad_requests_without_the_ui() {
        // A duplex in-memory "connection".
        struct Conn(std::io::Cursor<Vec<u8>>, Vec<u8>);
        impl std::io::Read for Conn {
            fn read(&mut self, b: &mut [u8]) -> std::io::Result<usize> {
                self.0.read(b)
            }
        }
        impl std::io::Write for Conn {
            fn write(&mut self, b: &[u8]) -> std::io::Result<usize> {
                self.1.write(b)
            }
            fn flush(&mut self) -> std::io::Result<()> {
                Ok(())
            }
        }
        let (tx, rx) = std::sync::mpsc::channel();
        let mut c = Conn(std::io::Cursor::new(b"{\"v\":7}\n".to_vec()), Vec::new());
        serve_one(&mut c, &tx);
        let r = decode_response(std::str::from_utf8(&c.1).unwrap()).unwrap();
        assert!(!r.ok);
        assert!(rx.try_recv().is_err(), "a bad request never reaches the UI");

        // A good one is handed over and its answer relayed.
        let h = std::thread::spawn(move || {
            let inc = rx.recv().unwrap();
            assert_eq!(inc.request, Request::List);
            inc.respond(Response {
                window: Some(9),
                ..Response::ok()
            });
        });
        let mut c = Conn(
            std::io::Cursor::new(encode_request(&Request::List).into_bytes()),
            Vec::new(),
        );
        serve_one(&mut c, &tx);
        h.join().unwrap();
        let r = decode_response(std::str::from_utf8(&c.1).unwrap()).unwrap();
        assert!(r.ok);
        assert_eq!(r.window, Some(9));
    }
}
