use super::WinChild;
use crate::cmdbuilder::CommandBuilder;
use crate::win::procthreadattr::ProcThreadAttributeList;
use anyhow::{bail, ensure, Error};
use filedescriptor::{FileDescriptor, OwnedHandle};
use lazy_static::lazy_static;
use shared_library::shared_library;
use std::ffi::OsString;
use std::io::Error as IoError;
use std::os::windows::ffi::OsStringExt;
use std::os::windows::io::{AsRawHandle, FromRawHandle};
use std::path::Path;
use std::sync::Mutex;
use std::{mem, ptr};
use winapi::shared::minwindef::DWORD;
use winapi::shared::winerror::{HRESULT, S_OK};
use winapi::um::handleapi::*;
use winapi::um::processthreadsapi::*;
use winapi::um::winbase::{
    CREATE_UNICODE_ENVIRONMENT, EXTENDED_STARTUPINFO_PRESENT, STARTF_USESTDHANDLES, STARTUPINFOEXW,
};
use winapi::um::wincon::COORD;
use winapi::um::winnt::HANDLE;

pub type HPCON = HANDLE;

pub const PSUEDOCONSOLE_INHERIT_CURSOR: DWORD = 0x1;
pub const PSEUDOCONSOLE_RESIZE_QUIRK: DWORD = 0x2;
pub const PSEUDOCONSOLE_WIN32_INPUT_MODE: DWORD = 0x4;
pub const PSEUDOCONSOLE_PASSTHROUGH_MODE: DWORD = 0x8;

// giest patch: opt-in PSEUDOCONSOLE_PASSTHROUGH_MODE. Upstream declares the
// flag and never passes it. MEASURED (see giest's GAP.md): no ConPTY build we
// could test actually honours it — inbox conhost 10.0.26100 accepts it (S_OK)
// and still strips APC; OpenConsole 1.24 forwards APC with or without it. It is
// passed anyway for older OpenConsole builds (1.17-1.21) that implemented it.
// Process-global because the PTY system is created through
// `native_pty_system()`, which takes no options.
static PASSTHROUGH_REQUESTED: std::sync::atomic::AtomicBool =
    std::sync::atomic::AtomicBool::new(false);
static PASSTHROUGH_ACTIVE: std::sync::atomic::AtomicBool =
    std::sync::atomic::AtomicBool::new(false);

/// Request (or stop requesting) passthrough mode for pseudoconsoles created
/// from now on.
pub fn set_passthrough(on: bool) {
    PASSTHROUGH_REQUESTED.store(on, std::sync::atomic::Ordering::Relaxed);
}

/// Whether the most recently created pseudoconsole accepted passthrough mode.
pub fn passthrough_active() -> bool {
    PASSTHROUGH_ACTIVE.load(std::sync::atomic::Ordering::Relaxed)
}

shared_library!(ConPtyFuncs,
    pub fn CreatePseudoConsole(
        size: COORD,
        hInput: HANDLE,
        hOutput: HANDLE,
        flags: DWORD,
        hpc: *mut HPCON
    ) -> HRESULT,
    pub fn ResizePseudoConsole(hpc: HPCON, size: COORD) -> HRESULT,
    pub fn ClosePseudoConsole(hpc: HPCON),
);

fn load_conpty() -> ConPtyFuncs {
    // If the kernel doesn't export these functions then their system is
    // too old and we cannot run.
    let kernel = ConPtyFuncs::open(Path::new("kernel32.dll")).expect(
        "this system does not support conpty.  Windows 10 October 2018 or newer is required",
    );

    // We prefer to use a sideloaded conpty.dll and openconsole.exe host deployed
    // alongside the application.  We check for this after checking for kernel
    // support so that we don't try to proceed and do something crazy.
    //
    // giest patch: the sideload can be vetoed (`set_allow_sideload(false)`),
    // and is looked up *next to the exe* by absolute path rather than through
    // the DLL search order (which includes the cwd and PATH).
    if !ALLOW_SIDELOAD.load(std::sync::atomic::Ordering::Relaxed) {
        return kernel;
    }
    let beside_exe = std::env::current_exe()
        .ok()
        .and_then(|p| p.parent().map(|d| d.join("conpty.dll")))
        .filter(|p| p.is_file());
    match beside_exe.map(|p| ConPtyFuncs::open(&p)) {
        Some(Ok(sideloaded)) => {
            SIDELOADED.store(true, std::sync::atomic::Ordering::Relaxed);
            sideloaded
        }
        _ => kernel,
    }
}

// giest patch: which ConPTY implementation is used. The inbox conhost (still
// 10.0.26100 on Windows 11 25H2) re-renders the child's output and strips APC
// and C0 controls like ENQ; it also accepts PSEUDOCONSOLE_PASSTHROUGH_MODE and
// silently ignores it. The out-of-band OpenConsole (conpty.dll 1.22+, the
// rewritten ConPTY) forwards those sequences with or without the flag. Both
// choices are latched on the first pseudoconsole: `CONPTY` is loaded once.
static ALLOW_SIDELOAD: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(true);
static SIDELOADED: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// Allow (default) or forbid loading a `conpty.dll` found next to the exe.
/// Only effective before the first pseudoconsole is created.
pub fn set_allow_sideload(on: bool) {
    ALLOW_SIDELOAD.store(on, std::sync::atomic::Ordering::Relaxed);
}

/// Whether ConPTY was loaded from a sideloaded `conpty.dll` (valid once a
/// pseudoconsole has been created).
pub fn sideloaded() -> bool {
    SIDELOADED.load(std::sync::atomic::Ordering::Relaxed)
}

lazy_static! {
    static ref CONPTY: ConPtyFuncs = load_conpty();
}

pub struct PsuedoCon {
    con: HPCON,
}

unsafe impl Send for PsuedoCon {}
unsafe impl Sync for PsuedoCon {}

impl Drop for PsuedoCon {
    fn drop(&mut self) {
        unsafe { (CONPTY.ClosePseudoConsole)(self.con) };
    }
}

impl PsuedoCon {
    pub fn new(size: COORD, input: FileDescriptor, output: FileDescriptor) -> Result<Self, Error> {
        let mut con: HPCON = INVALID_HANDLE_VALUE;
        let base = PSUEDOCONSOLE_INHERIT_CURSOR
            | PSEUDOCONSOLE_RESIZE_QUIRK
            | PSEUDOCONSOLE_WIN32_INPUT_MODE;
        let create = |flags: DWORD, con: &mut HPCON| unsafe {
            (CONPTY.CreatePseudoConsole)(
                size,
                input.as_raw_handle() as _,
                output.as_raw_handle() as _,
                flags,
                con,
            )
        };
        let want = PASSTHROUGH_REQUESTED.load(std::sync::atomic::Ordering::Relaxed);
        let mut result = if want {
            create(base | PSEUDOCONSOLE_PASSTHROUGH_MODE, &mut con)
        } else {
            create(base, &mut con)
        };
        // giest patch: a conhost that predates the flag rejects it; fall back
        // to the stock flags rather than failing the spawn.
        let mut active = want && result == S_OK;
        if want && result != S_OK {
            con = INVALID_HANDLE_VALUE;
            result = create(base, &mut con);
            active = false;
        }
        PASSTHROUGH_ACTIVE.store(active, std::sync::atomic::Ordering::Relaxed);
        ensure!(
            result == S_OK,
            "failed to create psuedo console: HRESULT {}",
            result
        );
        Ok(Self { con })
    }

    pub fn resize(&self, size: COORD) -> Result<(), Error> {
        let result = unsafe { (CONPTY.ResizePseudoConsole)(self.con, size) };
        ensure!(
            result == S_OK,
            "failed to resize console to {}x{}: HRESULT: {}",
            size.X,
            size.Y,
            result
        );
        Ok(())
    }

    pub fn spawn_command(&self, cmd: CommandBuilder) -> anyhow::Result<WinChild> {
        let mut si: STARTUPINFOEXW = unsafe { mem::zeroed() };
        si.StartupInfo.cb = mem::size_of::<STARTUPINFOEXW>() as u32;
        // Explicitly set the stdio handles as invalid handles otherwise
        // we can end up with a weird state where the spawned process can
        // inherit the explicitly redirected output handles from its parent.
        // For example, when daemonizing wezterm-mux-server, the stdio handles
        // are redirected to a log file and the spawned process would end up
        // writing its output there instead of to the pty we just created.
        si.StartupInfo.dwFlags = STARTF_USESTDHANDLES;
        si.StartupInfo.hStdInput = INVALID_HANDLE_VALUE;
        si.StartupInfo.hStdOutput = INVALID_HANDLE_VALUE;
        si.StartupInfo.hStdError = INVALID_HANDLE_VALUE;

        let mut attrs = ProcThreadAttributeList::with_capacity(1)?;
        attrs.set_pty(self.con)?;
        si.lpAttributeList = attrs.as_mut_ptr();

        let mut pi: PROCESS_INFORMATION = unsafe { mem::zeroed() };

        let (mut exe, mut cmdline) = cmd.cmdline()?;
        let cmd_os = OsString::from_wide(&cmdline);

        let cwd = cmd.current_directory();

        let res = unsafe {
            CreateProcessW(
                exe.as_mut_slice().as_mut_ptr(),
                cmdline.as_mut_slice().as_mut_ptr(),
                ptr::null_mut(),
                ptr::null_mut(),
                0,
                EXTENDED_STARTUPINFO_PRESENT | CREATE_UNICODE_ENVIRONMENT,
                cmd.environment_block().as_mut_slice().as_mut_ptr() as *mut _,
                cwd.as_ref()
                    .map(|c| c.as_slice().as_ptr())
                    .unwrap_or(ptr::null()),
                &mut si.StartupInfo,
                &mut pi,
            )
        };
        if res == 0 {
            let err = IoError::last_os_error();
            let msg = format!(
                "CreateProcessW `{:?}` in cwd `{:?}` failed: {}",
                cmd_os,
                cwd.as_ref().map(|c| OsString::from_wide(c)),
                err
            );
            log::error!("{}", msg);
            bail!("{}", msg);
        }

        // Make sure we close out the thread handle so we don't leak it;
        // we do this simply by making it owned
        let _main_thread = unsafe { OwnedHandle::from_raw_handle(pi.hThread as _) };
        let proc = unsafe { OwnedHandle::from_raw_handle(pi.hProcess as _) };

        Ok(WinChild {
            proc: Mutex::new(proc),
        })
    }
}
