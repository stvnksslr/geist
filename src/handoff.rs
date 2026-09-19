//! Default-terminal handoff: giest as Windows' "Default terminal application".
//!
//! **The chain** (verified against microsoft/terminal `src/host/srvinit.cpp`,
//! `src/propslib/DelegationConfig.cpp` and `src/host/proxy/*.idl`):
//!
//! 1. A console program started outside a terminal gets the inbox conhost,
//!    which reads `HKCU\Console\%%Startup` `DelegationConsole` /
//!    `DelegationTerminal` (REG_SZ CLSIDs). Any pair other than `{0…0}` or
//!    conhost's own is used verbatim — no package check.
//! 2. conhost `CoCreateInstance`s the **console** CLSID (an `OpenConsole.exe
//!    -Embedding`) and hands it the console session via
//!    `IConsoleHandoff::EstablishHandoff`.
//! 3. That OpenConsole `CoCreateInstance`s the **terminal** CLSID — ours,
//!    served by `giest.exe -Embedding` — and calls
//!    `ITerminalHandoff3::EstablishPtyHandoff(out in, out out, signal,
//!    reference, server, client, &startupInfo)`. `in`/`out` are **[out]**: the
//!    terminal creates the pipes and returns OpenConsole's ends. The four
//!    `[in]` handles belong to the proxy/stub and are closed when the call
//!    returns, so we duplicate them (as Windows Terminal's `ConptyConnection::
//!    InitializeFromHandoff` does).
//! 4. From then on OpenConsole is a headless ConPTY: its output on our read
//!    end, our input on its read end, resizes as `PTY_SIGNAL_RESIZE_WINDOW`
//!    (`u16 8, u16 cols, u16 rows`) on the signal pipe
//!    (`PtySignalInputThread.hpp`). Closing the signal pipe ends the session.
//!
//! **Why a native DLL ships beside giest.** The call crosses processes and
//! carries `system_handle`s, which only a MIDL-generated NDR proxy marshals.
//! `scripts/build-handoff-proxy.ps1` builds `giestHandoffProxy.dll` from the
//! vendored IDLs. Windows Terminal's own `OpenConsoleProxy.dll` is registered
//! only in its package's COM catalog, invisible to unpackaged processes.
//!
//! **The console side** is Windows Terminal's packaged OpenConsole
//! (`{2EACA947-…}`, its release-branding `CConsoleHandoff` CLSID). The NuGet
//! `OpenConsole.exe` giest sideloads has the *same* compiled-in CLSID, so
//! registering it under our own CLSID would never answer (COM would wait for a
//! class object that is never registered), and registering it under
//! `{2EACA947-…}` would hijack Windows Terminal's. Without Windows Terminal
//! installed there is no console half until an OpenConsole is built with a
//! CLSID of our own — see GAP.md.
//!
//! **Registration is HKCU only and exactly reversible.** `+register-default-
//! terminal` records the two `%%Startup` values it replaces (present *or
//! absent*) under `HKCU\Software\giest\DefaultTerminal`; `+unregister-default-
//! terminal` puts them back — unless the user has since picked another
//! terminal in Settings, which it then leaves alone — and deletes every key it
//! wrote.

use std::os::windows::io::OwnedHandle;

/// Our terminal CLSID (`CTerminalHandoff` equivalent).
pub const CLSID_TERMINAL: &str = "{2CED21A9-5236-4F72-B71F-10F3949295E5}";
/// The proxy/stub CLSID compiled into `giestHandoffProxy.dll`
/// (`vendor/terminal-handoff/dlldata.c` `PROXY_CLSID_IS`).
pub const PROXY_CLSID: &str = "{4CDF6A34-42C2-488C-84D2-4BC3F55F519D}";
/// Windows Terminal's (release) OpenConsole `CConsoleHandoff` CLSID.
pub const CLSID_WT_OPENCONSOLE: &str = "{2EACA947-7F5F-4CFA-BA87-8F7FBEEFBE69}";
/// `ITerminalHandoff3`.
pub const IID_TERMINAL_HANDOFF3: &str = "{6F23DA90-15C5-4203-9DB0-64E73F1B1B00}";
/// `IConsoleHandoff`.
pub const IID_CONSOLE_HANDOFF: &str = "{E686C757-9A35-4A1C-B3CE-0BCC8B5C69F4}";
/// The proxy DLL's file name, beside `giest.exe`.
pub const PROXY_DLL: &str = "giestHandoffProxy.dll";

/// Where conhost reads the delegation pair.
pub const STARTUP_KEY: &str = r"Console\%%Startup";
/// Where the replaced `%%Startup` values are kept until unregistration.
pub const BACKUP_KEY: &str = r"Software\giest\DefaultTerminal";
/// Backup value listing the parent keys `register` created.
const CREATED_VALUE: &str = "CreatedKeys";
/// Parents `register` may create; deepest first isn't needed (disjoint).
const CREATED_PARENTS: [&str; 3] = [r"Software\giest", r"Software\Classes\Interface", r"Software\Classes\CLSID"];
pub const DELEGATION_CONSOLE: &str = "DelegationConsole";
pub const DELEGATION_TERMINAL: &str = "DelegationTerminal";

/// `PTY_SIGNAL_RESIZE_WINDOW` on the signal pipe.
const PTY_SIGNAL_RESIZE_WINDOW: u16 = 8;

/// The signal-pipe message that resizes a handed-off pseudoconsole.
pub fn resize_message(cols: u16, rows: u16) -> [u8; 6] {
    let mut m = [0u8; 6];
    m[0..2].copy_from_slice(&PTY_SIGNAL_RESIZE_WINDOW.to_le_bytes());
    m[2..4].copy_from_slice(&cols.max(1).to_le_bytes());
    m[4..6].copy_from_slice(&rows.max(1).to_le_bytes());
    m
}

/// Whether the command line asks for the COM server (`-Embedding`, which COM
/// itself appends to `LocalServer32`; `/Embedding` is the same flag).
pub fn is_embedding(args: &[String]) -> bool {
    args.iter()
        .any(|a| a.eq_ignore_ascii_case("-Embedding") || a.eq_ignore_ascii_case("/Embedding"))
}

// ---------------------------------------------------------------- registry

/// One registry write: `(subkey under HKCU, value name or None, data)`.
pub type Entry = (String, Option<&'static str>, String);

/// Every class key `register` writes, for `exe` and the proxy `dll`. Pure, so
/// the exact strings are tested. `LocalServer32` has no `-Embedding`: COM
/// appends it.
pub fn class_entries(exe: &str, dll: &str) -> Vec<Entry> {
    let c = r"Software\Classes";
    vec![
        (format!(r"{c}\CLSID\{CLSID_TERMINAL}"), None, "giest terminal handoff".into()),
        (format!(r"{c}\CLSID\{CLSID_TERMINAL}\LocalServer32"), None, format!("\"{exe}\"")),
        (format!(r"{c}\CLSID\{PROXY_CLSID}"), None, "giest handoff proxy/stub".into()),
        (format!(r"{c}\CLSID\{PROXY_CLSID}\InProcServer32"), None, dll.to_string()),
        (format!(r"{c}\CLSID\{PROXY_CLSID}\InProcServer32"), Some("ThreadingModel"), "Both".into()),
        (format!(r"{c}\Interface\{IID_TERMINAL_HANDOFF3}"), None, "ITerminalHandoff3".into()),
        (
            format!(r"{c}\Interface\{IID_TERMINAL_HANDOFF3}\ProxyStubClsid32"),
            None,
            PROXY_CLSID.into(),
        ),
        (format!(r"{c}\Interface\{IID_CONSOLE_HANDOFF}"), None, "IConsoleHandoff".into()),
        (
            format!(r"{c}\Interface\{IID_CONSOLE_HANDOFF}\ProxyStubClsid32"),
            None,
            PROXY_CLSID.into(),
        ),
    ]
}

/// The key trees `class_entries` creates, deleted by `unregister`.
pub fn class_trees() -> [String; 4] {
    let c = r"Software\Classes";
    [
        format!(r"{c}\CLSID\{CLSID_TERMINAL}"),
        format!(r"{c}\CLSID\{PROXY_CLSID}"),
        format!(r"{c}\Interface\{IID_TERMINAL_HANDOFF3}"),
        format!(r"{c}\Interface\{IID_CONSOLE_HANDOFF}"),
    ]
}

/// A registry value as it was: present with data, or absent.
pub type Prior = Option<String>;

/// Encode a prior value for the backup key: `=data` or `-` (absent). A
/// sentinel rather than "no backup value", so an absent original is
/// distinguishable from a lost backup.
pub fn encode_prior(p: &Prior) -> String {
    match p {
        Some(v) => format!("={v}"),
        None => "-".into(),
    }
}

/// Inverse of [`encode_prior`]; `None` for a malformed record.
pub fn decode_prior(s: &str) -> Option<Prior> {
    if s == "-" {
        Some(None)
    } else {
        s.strip_prefix('=').map(|v| Some(v.to_string()))
    }
}

/// What `unregister` should do with one `%%Startup` value.
#[derive(Debug, PartialEq, Eq)]
pub enum Restore {
    Set(String),
    Delete,
    /// The value no longer holds what we wrote (the user chose another
    /// terminal since): leave their choice alone.
    Keep,
}

/// Decide the restore of one value: `current` is what is there now, `ours`
/// what `register` wrote, `prior` what was there before.
pub fn restore_action(current: &Prior, ours: &str, prior: &Prior) -> Restore {
    if current.as_deref().is_none_or(|c| !c.eq_ignore_ascii_case(ours)) {
        return Restore::Keep;
    }
    match prior {
        Some(v) => Restore::Set(v.clone()),
        None => Restore::Delete,
    }
}

/// The pair `register` writes.
pub fn delegation_pair() -> [(&'static str, &'static str); 2] {
    [
        (DELEGATION_CONSOLE, CLSID_WT_OPENCONSOLE),
        (DELEGATION_TERMINAL, CLSID_TERMINAL),
    ]
}

/// The PE optional header's `Subsystem` (2 = GUI, 3 = console) of `image`.
pub fn pe_subsystem(image: &[u8]) -> Option<u16> {
    let u32_at = |o: usize| image.get(o..o + 4).map(|b| u32::from_le_bytes([b[0], b[1], b[2], b[3]]));
    if image.get(0..2)? != b"MZ" {
        return None;
    }
    let pe = u32_at(0x3c)? as usize;
    if image.get(pe..pe + 4)? != b"PE\0\0" {
        return None;
    }
    // Signature (4) + COFF header (20) + 68 bytes into the optional header
    // (the same offset for PE32 and PE32+).
    let o = pe + 24 + 68;
    image.get(o..o + 2).map(|b| u16::from_le_bytes([b[0], b[1]]))
}

/// A console-subsystem build (every debug build) must not be the COM server:
/// launched by COM it gets a console of its own, and creating that console
/// goes through delegation again — to Windows Terminal, or with giest
/// registered, back to giest, which deadlocks waiting on itself. Measured:
/// the process never reaches `main` and COM reports `CO_E_SERVER_EXEC_FAILURE`.
fn check_gui_subsystem(exe: &std::path::Path) -> std::io::Result<()> {
    let bytes = std::fs::read(exe)?;
    match pe_subsystem(&bytes) {
        Some(2) => Ok(()),
        _ => Err(std::io::Error::other(format!(
            "{} is not a GUI-subsystem build (a debug build?); register a release build",
            exe.display()
        ))),
    }
}

/// `+register-default-terminal`: HKCU only.
pub fn register() -> std::io::Result<String> {
    use crate::shellreg as reg;
    let exe = std::env::current_exe()?;
    check_gui_subsystem(&exe)?;
    let dll = exe.with_file_name(PROXY_DLL);
    if !dll.is_file() {
        return Err(std::io::Error::other(format!(
            "{} not found beside giest.exe; build it with scripts/build-handoff-proxy.ps1",
            dll.display()
        )));
    }
    // Never clobber someone else's proxy registration for these IIDs.
    for iid in [IID_TERMINAL_HANDOFF3, IID_CONSOLE_HANDOFF] {
        let k = format!(r"Software\Classes\Interface\{iid}\ProxyStubClsid32");
        if let Some(v) = reg::get_string(&k, None)
            && !v.eq_ignore_ascii_case(PROXY_CLSID)
        {
            return Err(std::io::Error::other(format!(
                "HKCU\\{k} already names {v}; not overwriting it"
            )));
        }
    }
    // Back up once: re-registering must not overwrite the original backup
    // with our own values.
    let mut report = String::new();
    if !reg::key_exists(BACKUP_KEY) {
        // Parent keys this registration would create, so unregister can
        // remove them again (and only them).
        let created: Vec<&str> = CREATED_PARENTS.iter().copied().filter(|k| !reg::key_exists(k)).collect();
        reg::set_string(BACKUP_KEY, Some(CREATED_VALUE), &created.join(";"))?;
        for (name, _) in delegation_pair() {
            let prior = reg::get_string(STARTUP_KEY, Some(name));
            report.push_str(&format!("{name} was {}\n", prior.as_deref().unwrap_or("(absent)")));
            reg::set_string(BACKUP_KEY, Some(name), &encode_prior(&prior))?;
        }
    } else {
        report.push_str("already registered; keeping the original backup\n");
    }
    let rollback = |e: std::io::Error| {
        let _ = unregister();
        e
    };
    for (k, n, d) in class_entries(&exe.display().to_string(), &dll.display().to_string()) {
        reg::set_string(&k, n, &d).map_err(rollback)?;
    }
    for (name, value) in delegation_pair() {
        reg::set_string(STARTUP_KEY, Some(name), value).map_err(rollback)?;
        report.push_str(&format!("{name} = {value}\n"));
    }
    Ok(report)
}

/// Register only the COM half (our CLSID + the proxy/stub) for `exe`, without
/// touching `%%Startup` — so a test can activate `giest -Embedding` directly,
/// playing OpenConsole, with no effect on how consoles launch. Pair with
/// [`unregister_com_only`]. Refuses if any of the keys already exist.
pub fn register_com_only(exe: &std::path::Path) -> std::io::Result<()> {
    use crate::shellreg as reg;
    check_gui_subsystem(exe)?;
    let dll = exe.with_file_name(PROXY_DLL);
    if !dll.is_file() {
        return Err(std::io::Error::other(format!("{} not found", dll.display())));
    }
    if let Some(k) = class_trees().iter().find(|k| reg::key_exists(k)) {
        return Err(std::io::Error::other(format!("HKCU\\{k} already exists")));
    }
    for (k, n, d) in class_entries(&exe.display().to_string(), &dll.display().to_string()) {
        if let Err(e) = reg::set_string(&k, n, &d) {
            let _ = unregister_com_only();
            return Err(e);
        }
    }
    Ok(())
}

/// Inverse of [`register_com_only`].
pub fn unregister_com_only() -> std::io::Result<()> {
    let mut first_err = None;
    for k in class_trees() {
        if let Err(e) = crate::shellreg::delete_tree(&k) {
            first_err.get_or_insert(e);
        }
    }
    first_err.map_or(Ok(()), Err)
}

/// `+unregister-default-terminal`: restore the backed-up `%%Startup` values
/// and delete every key `register` wrote.
pub fn unregister() -> std::io::Result<String> {
    use crate::shellreg as reg;
    let mut report = String::new();
    let mut first_err = None;
    if reg::key_exists(BACKUP_KEY) {
        for (name, ours) in delegation_pair() {
            let Some(prior) = reg::get_string(BACKUP_KEY, Some(name)).and_then(|s| decode_prior(&s))
            else {
                report.push_str(&format!("{name}: no usable backup; left as is\n"));
                continue;
            };
            let current = reg::get_string(STARTUP_KEY, Some(name));
            let r = match restore_action(&current, ours, &prior) {
                Restore::Set(v) => {
                    report.push_str(&format!("{name} restored to {v}\n"));
                    reg::set_string(STARTUP_KEY, Some(name), &v)
                }
                Restore::Delete => {
                    report.push_str(&format!("{name} removed (it was absent)\n"));
                    reg::delete_value(STARTUP_KEY, name)
                }
                Restore::Keep => {
                    report.push_str(&format!(
                        "{name} is now {}, not giest's; left as is\n",
                        current.as_deref().unwrap_or("(absent)")
                    ));
                    Ok(())
                }
            };
            if let Err(e) = r {
                first_err.get_or_insert(e);
            }
        }
    } else {
        report.push_str("no backup found (not registered)\n");
    }
    for k in class_trees() {
        // Only our own Interface keys: another proxy's are not ours to remove.
        if k.contains(r"\Interface\")
            && reg::get_string(&format!(r"{k}\ProxyStubClsid32"), None)
                .is_some_and(|v| !v.eq_ignore_ascii_case(PROXY_CLSID))
        {
            continue;
        }
        if let Err(e) = reg::delete_tree(&k) {
            first_err.get_or_insert(e);
        }
    }
    // Only once every value is back does the backup go — and with it any
    // parent key register created (if nothing else has moved in since).
    if first_err.is_none() {
        let created = reg::get_string(BACKUP_KEY, Some(CREATED_VALUE)).unwrap_or_default();
        if let Err(e) = reg::delete_tree(BACKUP_KEY) {
            first_err.get_or_insert(e);
        }
        for k in created.split(';').filter(|k| CREATED_PARENTS.contains(k)) {
            if let Err(e) = reg::delete_if_empty(k) {
                first_err.get_or_insert(e);
            }
        }
    }
    match first_err {
        Some(e) => Err(e),
        None => Ok(report),
    }
}

// ---------------------------------------------------------------- the session

/// A pseudoconsole handed to us: the pipes we created plus duplicates of the
/// handles OpenConsole passed in. `Send`, so it can cross from the COM thread.
#[derive(Debug)]
pub struct Attached {
    /// Our write end of OpenConsole's input.
    pub input: OwnedHandle,
    /// Our read end of OpenConsole's output.
    pub output: OwnedHandle,
    /// Write end of the signal pipe (resize; close = hang up).
    pub signal: OwnedHandle,
    /// The console's `\Reference` handle; held for the session's lifetime.
    pub reference: OwnedHandle,
    /// The OpenConsole process.
    pub server: OwnedHandle,
    /// The client program (e.g. `cmd.exe`) — exit detection.
    pub client: OwnedHandle,
    pub title: String,
    pub show_window: u16,
}

/// [`Attached`]'s handle values as raw numbers, valid in process `pid` — what
/// crosses the IPC pipe to the running instance, which duplicates them in.
#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct RawHandles {
    pub input: u64,
    pub output: u64,
    pub signal: u64,
    pub reference: u64,
    pub server: u64,
    pub client: u64,
}

/// Append a line to `%TEMP%\giest-handoff.log`. The `-Embedding` process has
/// no console and no window until the handoff succeeds, so this is the only
/// place a failure can be seen.
pub fn log(msg: &str) {
    use std::io::Write;
    let path = std::env::temp_dir().join("giest-handoff.log");
    // Bounded: start over rather than grow forever.
    if std::fs::metadata(&path).is_ok_and(|m| m.len() > 256 * 1024) {
        let _ = std::fs::remove_file(&path);
    }
    if let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(path) {
        let t = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |d| d.as_secs());
        let _ = writeln!(f, "{t} [{}] {msg}", std::process::id());
    }
}

static INITIAL: std::sync::Mutex<Option<Attached>> = std::sync::Mutex::new(None);

/// Park the handoff this process was started for; the first session takes it.
pub fn set_initial(a: Attached) {
    *INITIAL.lock().unwrap_or_else(|e| e.into_inner()) = Some(a);
}

/// Take the handoff this process was started for, if any.
pub fn take_initial() -> Option<Attached> {
    INITIAL.lock().unwrap_or_else(|e| e.into_inner()).take()
}

#[cfg(windows)]
pub use imp::{adopt_remote, client_image_name, is_process_running, process_exit, process_id, serve_one};

#[cfg(windows)]
mod imp {
    use std::ffi::c_void;
    use std::os::windows::io::{AsRawHandle, FromRawHandle, OwnedHandle};
    use std::sync::Mutex;
    use std::sync::mpsc::{Sender, channel};
    use std::time::Duration;

    use super::{Attached, RawHandles};

    type Handle = *mut c_void;
    type HResult = i32;

    #[repr(C)]
    #[derive(Clone, Copy, PartialEq, Eq)]
    struct Guid(u32, u16, u16, [u8; 8]);

    const IID_IUNKNOWN: Guid = Guid(0, 0, 0, [0xc0, 0, 0, 0, 0, 0, 0, 0x46]);
    const IID_ICLASSFACTORY: Guid = Guid(1, 0, 0, [0xc0, 0, 0, 0, 0, 0, 0, 0x46]);
    const IID_ITERMINALHANDOFF3: Guid =
        Guid(0x6f23_da90, 0x15c5, 0x4203, [0x9d, 0xb0, 0x64, 0xe7, 0x3f, 0x1b, 0x1b, 0x00]);
    const CLSID_TERMINAL: Guid =
        Guid(0x2ced_21a9, 0x5236, 0x4f72, [0xb7, 0x1f, 0x10, 0xf3, 0x94, 0x92, 0x95, 0xe5]);

    const S_OK: HResult = 0;
    const E_NOINTERFACE: HResult = 0x8000_4002u32 as i32;
    const E_POINTER: HResult = 0x8000_4003u32 as i32;
    const E_FAIL: HResult = 0x8000_4005u32 as i32;
    const CLASS_E_NOAGGREGATION: HResult = 0x8004_0110u32 as i32;

    const COINIT_MULTITHREADED: u32 = 0;
    const CLSCTX_LOCAL_SERVER: u32 = 0x4;
    const REGCLS_MULTIPLEUSE: u32 = 1;
    const DUPLICATE_SAME_ACCESS: u32 = 2;
    const PROCESS_DUP_HANDLE: u32 = 0x40;
    const WAIT_OBJECT_0: u32 = 0;

    #[link(name = "ole32")]
    unsafe extern "system" {
        fn CoInitializeEx(reserved: *mut c_void, coinit: u32) -> HResult;
        fn CoUninitialize();
        fn CoRegisterClassObject(
            clsid: *const Guid,
            unk: *mut c_void,
            ctx: u32,
            flags: u32,
            cookie: *mut u32,
        ) -> HResult;
        fn CoRevokeClassObject(cookie: u32) -> HResult;
    }
    #[link(name = "oleaut32")]
    unsafe extern "system" {
        fn SysStringLen(s: *const u16) -> u32;
    }
    #[link(name = "kernel32")]
    unsafe extern "system" {
        fn CreatePipe(read: *mut Handle, write: *mut Handle, sa: *const c_void, size: u32) -> i32;
        fn GetCurrentProcess() -> Handle;
        fn DuplicateHandle(
            src_proc: Handle,
            src: Handle,
            dst_proc: Handle,
            dst: *mut Handle,
            access: u32,
            inherit: i32,
            options: u32,
        ) -> i32;
        fn OpenProcess(access: u32, inherit: i32, pid: u32) -> Handle;
        fn WaitForSingleObject(h: Handle, ms: u32) -> u32;
        fn GetExitCodeProcess(h: Handle, code: *mut u32) -> i32;
        fn QueryFullProcessImageNameW(h: Handle, flags: u32, buf: *mut u16, len: *mut u32) -> i32;
        fn GetProcessId(h: Handle) -> u32;
    }

    /// `TERMINAL_STARTUP_INFO` (ITerminalHandoff.idl), field for field.
    #[repr(C)]
    struct StartupInfo {
        title: *const u16,
        icon_path: *const u16,
        icon_index: i32,
        x: u32,
        y: u32,
        x_size: u32,
        y_size: u32,
        x_chars: u32,
        y_chars: u32,
        fill_attribute: u32,
        flags: u32,
        show_window: u16,
    }

    #[repr(C)]
    struct UnknownVtbl {
        qi: unsafe extern "system" fn(*mut c_void, *const Guid, *mut *mut c_void) -> HResult,
        add_ref: unsafe extern "system" fn(*mut c_void) -> u32,
        release: unsafe extern "system" fn(*mut c_void) -> u32,
    }

    #[repr(C)]
    struct FactoryVtbl {
        base: UnknownVtbl,
        create_instance:
            unsafe extern "system" fn(*mut c_void, *mut c_void, *const Guid, *mut *mut c_void) -> HResult,
        lock_server: unsafe extern "system" fn(*mut c_void, i32) -> HResult,
    }

    #[repr(C)]
    struct HandoffVtbl {
        base: UnknownVtbl,
        establish: unsafe extern "system" fn(
            *mut c_void,
            *mut Handle,
            *mut Handle,
            Handle,
            Handle,
            Handle,
            Handle,
            *const StartupInfo,
        ) -> HResult,
    }

    /// A COM object is a pointer to a vtable pointer. Both objects are
    /// process-lifetime statics, so reference counting is a no-op.
    #[repr(C)]
    struct Object<V: 'static>(&'static V);
    // SAFETY: the vtables are immutable statics of function pointers.
    unsafe impl<V> Sync for Object<V> {}

    static FACTORY: Object<FactoryVtbl> = Object(&FactoryVtbl {
        base: UnknownVtbl { qi: factory_qi, add_ref, release },
        create_instance,
        lock_server,
    });
    static HANDOFF: Object<HandoffVtbl> = Object(&HandoffVtbl {
        base: UnknownVtbl { qi: handoff_qi, add_ref, release },
        establish,
    });

    /// Where `establish` delivers the session. Set by [`serve_one`].
    static SINK: Mutex<Option<Sender<Attached>>> = Mutex::new(None);

    unsafe extern "system" fn add_ref(_: *mut c_void) -> u32 {
        2
    }
    unsafe extern "system" fn release(_: *mut c_void) -> u32 {
        1
    }

    unsafe fn qi(this: *mut c_void, iid: *const Guid, out: *mut *mut c_void, ok: Guid) -> HResult {
        if out.is_null() {
            return E_POINTER;
        }
        // SAFETY: COM passes a valid IID pointer and out-pointer.
        unsafe {
            if *iid == IID_IUNKNOWN || *iid == ok {
                *out = this;
                S_OK
            } else {
                *out = std::ptr::null_mut();
                E_NOINTERFACE
            }
        }
    }
    unsafe extern "system" fn factory_qi(t: *mut c_void, iid: *const Guid, out: *mut *mut c_void) -> HResult {
        // SAFETY: forwarded COM arguments.
        unsafe { qi(t, iid, out, IID_ICLASSFACTORY) }
    }
    unsafe extern "system" fn handoff_qi(t: *mut c_void, iid: *const Guid, out: *mut *mut c_void) -> HResult {
        // SAFETY: forwarded COM arguments.
        unsafe { qi(t, iid, out, IID_ITERMINALHANDOFF3) }
    }
    unsafe extern "system" fn create_instance(
        _: *mut c_void,
        outer: *mut c_void,
        iid: *const Guid,
        out: *mut *mut c_void,
    ) -> HResult {
        if !outer.is_null() {
            return CLASS_E_NOAGGREGATION;
        }
        super::log("CreateInstance called");
        // SAFETY: forwarded COM arguments; HANDOFF is a static.
        unsafe { handoff_qi(&HANDOFF as *const _ as *mut c_void, iid, out) }
    }
    unsafe extern "system" fn lock_server(_: *mut c_void, _: i32) -> HResult {
        S_OK
    }

    fn dup(h: Handle) -> Option<OwnedHandle> {
        let mut out: Handle = std::ptr::null_mut();
        // SAFETY: `h` is a handle valid for the duration of the COM call.
        let ok = unsafe {
            DuplicateHandle(GetCurrentProcess(), h, GetCurrentProcess(), &mut out, 0, 0, DUPLICATE_SAME_ACCESS)
        };
        // SAFETY: a successful duplicate is a fresh handle we own.
        (ok != 0).then(|| unsafe { OwnedHandle::from_raw_handle(out) })
    }

    fn pipe() -> Option<(OwnedHandle, OwnedHandle)> {
        let (mut r, mut w): (Handle, Handle) = (std::ptr::null_mut(), std::ptr::null_mut());
        // SAFETY: out-pointers are locals.
        if unsafe { CreatePipe(&mut r, &mut w, std::ptr::null(), 0) } == 0 {
            return None;
        }
        // SAFETY: fresh handles we own.
        Some(unsafe { (OwnedHandle::from_raw_handle(r), OwnedHandle::from_raw_handle(w)) })
    }

    fn bstr(s: *const u16) -> String {
        if s.is_null() {
            return String::new();
        }
        // SAFETY: a BSTR from the stub; SysStringLen reads its length prefix.
        unsafe {
            let n = SysStringLen(s) as usize;
            String::from_utf16_lossy(std::slice::from_raw_parts(s, n))
        }
    }

    #[allow(clippy::too_many_arguments)]
    unsafe extern "system" fn establish(
        _: *mut c_void,
        in_: *mut Handle,
        out: *mut Handle,
        signal: Handle,
        reference: Handle,
        server: Handle,
        client: Handle,
        info: *const StartupInfo,
    ) -> HResult {
        super::log("EstablishPtyHandoff called");
        if in_.is_null() || out.is_null() || info.is_null() {
            return E_POINTER;
        }
        let Some(sink) = SINK.lock().ok().and_then(|s| s.clone()) else {
            return E_FAIL;
        };
        let build = || -> Option<(Attached, OwnedHandle, OwnedHandle)> {
            // OpenConsole reads its input from `con_in`; we write `input`.
            let (con_in, input) = pipe()?;
            // OpenConsole writes its output to `con_out`; we read `output`.
            let (output, con_out) = pipe()?;
            // SAFETY: non-null, checked above; valid for the call.
            let info = unsafe { &*info };
            let a = Attached {
                input,
                output,
                signal: dup(signal)?,
                reference: dup(reference)?,
                server: dup(server)?,
                client: dup(client)?,
                title: bstr(info.title),
                show_window: info.show_window,
            };
            Some((a, con_in, con_out))
        };
        let Some((a, con_in, con_out)) = build() else {
            return E_FAIL;
        };
        if sink.send(a).is_err() {
            return E_FAIL;
        }
        // Ownership of OpenConsole's ends passes to the stub, which marshals
        // and closes them (Windows Terminal likewise `release()`s them).
        // SAFETY: non-null out-pointers, checked above.
        unsafe {
            *in_ = std::os::windows::io::IntoRawHandle::into_raw_handle(con_in);
            *out = std::os::windows::io::IntoRawHandle::into_raw_handle(con_out);
        }
        S_OK
    }

    /// `giest -Embedding`: register the class object, wait for OpenConsole's
    /// one `EstablishPtyHandoff`, revoke. `None` on timeout or COM failure.
    pub fn serve_one(timeout: Duration) -> Result<Attached, String> {
        let (tx, rx) = channel();
        *SINK.lock().unwrap_or_else(|e| e.into_inner()) = Some(tx);
        let (ready_tx, ready_rx) = channel::<Result<(), String>>();
        let (done_tx, done_rx) = channel::<()>();
        // COM lives on its own MTA thread so the UI thread stays free of it.
        std::thread::Builder::new()
            .name("handoff-com".into())
            .spawn(move || {
                // SAFETY: plain COM initialisation on this thread.
                let hr = unsafe { CoInitializeEx(std::ptr::null_mut(), COINIT_MULTITHREADED) };
                if hr < 0 {
                    let _ = ready_tx.send(Err(format!("CoInitializeEx: {hr:#010x}")));
                    return;
                }
                let mut cookie = 0u32;
                // SAFETY: FACTORY is a static COM object; out-pointer local.
                let hr = unsafe {
                    CoRegisterClassObject(
                        &CLSID_TERMINAL,
                        &FACTORY as *const _ as *mut c_void,
                        CLSCTX_LOCAL_SERVER,
                        REGCLS_MULTIPLEUSE,
                        &mut cookie,
                    )
                };
                if hr < 0 {
                    let _ = ready_tx.send(Err(format!("CoRegisterClassObject: {hr:#010x}")));
                    // SAFETY: balances the successful CoInitializeEx.
                    unsafe { CoUninitialize() };
                    return;
                }
                let _ = ready_tx.send(Ok(()));
                // Stay registered until the caller has its session (or gave
                // up), then a grace period for the call to finish returning.
                let _ = done_rx.recv();
                std::thread::sleep(Duration::from_millis(200));
                // SAFETY: the cookie from the successful registration.
                unsafe {
                    CoRevokeClassObject(cookie);
                    CoUninitialize();
                }
            })
            .map_err(|e| e.to_string())?;
        ready_rx.recv().map_err(|e| e.to_string())??;
        let r = rx.recv_timeout(timeout).map_err(|_| "no handoff arrived".to_string());
        let _ = done_tx.send(());
        r
    }

    /// Duplicate a handoff out of process `pid` (the `-Embedding` instance)
    /// into this one. The source keeps its copies; it closes them once we
    /// answer.
    pub fn adopt_remote(pid: u32, raw: &RawHandles, title: String, show_window: u16) -> std::io::Result<Attached> {
        // SAFETY: plain call; the handle is closed by OwnedHandle.
        let proc_ = unsafe { OpenProcess(PROCESS_DUP_HANDLE, 0, pid) };
        if proc_.is_null() {
            return Err(std::io::Error::last_os_error());
        }
        // SAFETY: a fresh handle we own.
        let proc_ = unsafe { OwnedHandle::from_raw_handle(proc_) };
        let take = |h: u64| -> std::io::Result<OwnedHandle> {
            let mut out: Handle = std::ptr::null_mut();
            // SAFETY: `h` is interpreted in the source process only.
            let ok = unsafe {
                DuplicateHandle(
                    proc_.as_raw_handle(),
                    h as usize as Handle,
                    GetCurrentProcess(),
                    &mut out,
                    0,
                    0,
                    DUPLICATE_SAME_ACCESS,
                )
            };
            if ok == 0 {
                return Err(std::io::Error::last_os_error());
            }
            // SAFETY: a fresh handle we own.
            Ok(unsafe { OwnedHandle::from_raw_handle(out) })
        };
        Ok(Attached {
            input: take(raw.input)?,
            output: take(raw.output)?,
            signal: take(raw.signal)?,
            reference: take(raw.reference)?,
            server: take(raw.server)?,
            client: take(raw.client)?,
            title,
            show_window,
        })
    }

    /// Process `h`'s id (0 if it is not a process handle).
    pub fn process_id(h: &OwnedHandle) -> u32 {
        // SAFETY: any handle; a non-process one just yields 0.
        unsafe { GetProcessId(h.as_raw_handle()) }
    }

    /// Whether process `h` is still running.
    pub fn is_process_running(h: &OwnedHandle) -> bool {
        // SAFETY: a live process handle.
        unsafe { WaitForSingleObject(h.as_raw_handle(), 0) != WAIT_OBJECT_0 }
    }

    /// Process `h`'s exit code, once it has exited.
    pub fn process_exit(h: &OwnedHandle) -> Option<u32> {
        if is_process_running(h) {
            return None;
        }
        let mut code = 0u32;
        // SAFETY: a live process handle and a local out-pointer.
        (unsafe { GetExitCodeProcess(h.as_raw_handle(), &mut code) } != 0).then_some(code)
    }

    /// The client program's file name (`cmd.exe`), for the pane's profile.
    pub fn client_image_name(h: &OwnedHandle) -> Option<String> {
        let mut buf = [0u16; 1024];
        let mut len = buf.len() as u32;
        // SAFETY: valid buffer/length; a process handle with query access.
        let ok = unsafe { QueryFullProcessImageNameW(h.as_raw_handle(), 0, buf.as_mut_ptr(), &mut len) };
        if ok == 0 {
            return None;
        }
        let path = String::from_utf16_lossy(&buf[..len as usize]);
        std::path::Path::new(&path).file_name().map(|f| f.to_string_lossy().into_owned())
    }

    impl Attached {
        /// This process's raw handle values, for [`adopt_remote`] elsewhere.
        pub fn raw(&self) -> RawHandles {
            let r = |h: &OwnedHandle| h.as_raw_handle() as usize as u64;
            RawHandles {
                input: r(&self.input),
                output: r(&self.output),
                signal: r(&self.signal),
                reference: r(&self.reference),
                server: r(&self.server),
                client: r(&self.client),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resize_is_signal_8_then_cols_rows_little_endian() {
        assert_eq!(resize_message(120, 30), [8, 0, 120, 0, 30, 0]);
        assert_eq!(resize_message(300, 0), [8, 0, 0x2c, 0x01, 1, 0], "zero clamps to 1");
    }

    #[test]
    fn embedding_flag_in_either_spelling() {
        let a = |v: &[&str]| v.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        assert!(is_embedding(&a(&["-Embedding"])));
        assert!(is_embedding(&a(&["/embedding"])));
        assert!(!is_embedding(&a(&["+new-tab"])));
    }

    #[test]
    fn class_entries_point_at_the_exe_and_the_proxy() {
        let e = class_entries(r"C:\a b\giest.exe", r"C:\a b\giestHandoffProxy.dll");
        assert!(e.contains(&(
            format!(r"Software\Classes\CLSID\{CLSID_TERMINAL}\LocalServer32"),
            None,
            r#""C:\a b\giest.exe""#.into()
        )));
        assert!(e.contains(&(
            format!(r"Software\Classes\Interface\{IID_TERMINAL_HANDOFF3}\ProxyStubClsid32"),
            None,
            PROXY_CLSID.into()
        )));
        assert!(e.contains(&(
            format!(r"Software\Classes\CLSID\{PROXY_CLSID}\InProcServer32"),
            Some("ThreadingModel"),
            "Both".into()
        )));
        // Every key written lies under a tree unregister deletes.
        for (k, _, _) in &e {
            assert!(class_trees().iter().any(|t| k.starts_with(t.as_str())), "{k}");
        }
    }

    #[test]
    fn pe_subsystem_reads_the_optional_header() {
        let mut img = vec![0u8; 0x200];
        img[0..2].copy_from_slice(b"MZ");
        img[0x3c..0x40].copy_from_slice(&0x80u32.to_le_bytes());
        img[0x80..0x84].copy_from_slice(b"PE\0\0");
        img[0x80 + 24 + 68..0x80 + 24 + 70].copy_from_slice(&2u16.to_le_bytes());
        assert_eq!(pe_subsystem(&img), Some(2));
        assert_eq!(pe_subsystem(b"not a pe"), None);
        // This test binary is a console program.
        let me = std::fs::read(std::env::current_exe().unwrap()).unwrap();
        assert_eq!(pe_subsystem(&me), Some(3));
    }

    #[test]
    fn prior_values_round_trip_including_absence() {
        for p in [None, Some(String::new()), Some("{00000000-0000-0000-0000-000000000000}".into())] {
            assert_eq!(decode_prior(&encode_prior(&p)), Some(p));
        }
        assert_eq!(decode_prior("garbage"), None);
    }

    #[test]
    fn restore_puts_back_exactly_what_was_there() {
        let ours = CLSID_TERMINAL;
        let zero = "{00000000-0000-0000-0000-000000000000}".to_string();
        assert_eq!(
            restore_action(&Some(ours.to_lowercase()), ours, &Some(zero.clone())),
            Restore::Set(zero)
        );
        assert_eq!(restore_action(&Some(ours.into()), ours, &None), Restore::Delete);
        // The user picked something else since: leave it.
        assert_eq!(
            restore_action(&Some(CLSID_WT_OPENCONSOLE.into()), ours, &None),
            Restore::Keep
        );
        assert_eq!(restore_action(&None, ours, &None), Restore::Keep);
    }

    #[test]
    fn guid_strings_match_the_binary_constants() {
        // The string forms (registry) and the proxy's dlldata.c must agree.
        let dlldata = include_str!("../vendor/terminal-handoff/dlldata.c");
        assert!(dlldata.contains("0x4cdf6a34, 0x42c2, 0x488c, {0x84, 0xd2, 0x4b, 0xc3, 0xf5, 0x5f, 0x51, 0x9d}"));
        assert!(PROXY_CLSID.eq_ignore_ascii_case("{4cdf6a34-42c2-488c-84d2-4bc3f55f519d}"));
        let idl = include_str!("../vendor/terminal-handoff/ITerminalHandoff.idl");
        assert!(idl.contains(&IID_TERMINAL_HANDOFF3[1..37]));
        let idl = include_str!("../vendor/terminal-handoff/IConsoleHandoff.idl");
        assert!(idl.contains(&IID_CONSOLE_HANDOFF[1..37]));
        // The MSIX declares the same class, proxy and interface.
        let manifest = include_str!("../packaging/AppxManifest.xml.in");
        for g in [CLSID_TERMINAL, PROXY_CLSID, IID_TERMINAL_HANDOFF3] {
            assert!(manifest.contains(&g[1..37]), "manifest lacks {g}");
        }
    }
}
