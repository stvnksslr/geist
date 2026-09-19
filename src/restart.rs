//! Restart after Windows Update / reboot — `RegisterApplicationRestart`.
//!
//! Windows relaunches registered applications after an update restart (and,
//! with "Automatically save my restartable apps" on, after sign-in). giest
//! registers `--restore-session`, a CLI flag that restores the saved layout
//! once regardless of `window-save-state` (`cli.rs`), so the relaunch comes
//! back as the windows, tabs, splits, directories and frames the user had.
//!
//! **Why the layout is written from a window procedure.** A session end is
//! `WM_QUERYENDSESSION` → `WM_ENDSESSION`, after which the process is simply
//! terminated: eframe's `on_exit` never runs, so the normal exit-time save
//! never happens. The UI thread therefore keeps a serialized snapshot here
//! (refreshed every couple of seconds, see `App::ui`), and the root window's
//! subclass writes it to the state file on `WM_ENDSESSION` — no app state is
//! touched from inside the message loop.
//!
//! Crash/hang restarts are opted out (`RESTART_NO_CRASH | RESTART_NO_HANG`): a
//! terminal that crashed on some output should not relaunch into the same
//! layout and do it again.

use std::sync::Mutex;

/// The argument Windows relaunches giest with.
pub const RESTART_ARGS: &str = "--restore-session";

const RESTART_NO_CRASH: u32 = 1;
const RESTART_NO_HANG: u32 = 2;

/// The latest serialized layout, written on `WM_ENDSESSION`.
static SNAPSHOT: Mutex<Option<String>> = Mutex::new(None);

/// Replace the end-of-session snapshot. `None` clears it (nothing to restore).
pub fn set_snapshot(text: Option<String>) {
    if let Ok(mut s) = SNAPSHOT.lock() {
        *s = text;
    }
}

/// Write the snapshot to the state file — what `WM_ENDSESSION` does.
fn write_snapshot() {
    let text = SNAPSHOT.lock().ok().and_then(|s| s.clone());
    let Some(text) = text else { return };
    let Some(path) = crate::state::state_path() else { return };
    if let Some(dir) = path.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    let _ = std::fs::write(path, text);
}

#[cfg(windows)]
mod imp {
    use std::os::windows::ffi::OsStrExt;

    use windows_sys::Win32::Foundation::HWND;

    const WM_ENDSESSION: u32 = 0x0016;
    const WM_NCDESTROY: u32 = 0x0082;
    const SUBCLASS_ID: usize = 0x6769_7273; // "girs"

    type SubclassProc = unsafe extern "system" fn(HWND, u32, usize, isize, usize, usize) -> isize;

    #[link(name = "kernel32")]
    unsafe extern "system" {
        fn RegisterApplicationRestart(cmdline: *const u16, flags: u32) -> i32;
    }
    #[link(name = "comctl32")]
    unsafe extern "system" {
        fn SetWindowSubclass(hwnd: HWND, f: SubclassProc, id: usize, data: usize) -> i32;
        fn RemoveWindowSubclass(hwnd: HWND, f: SubclassProc, id: usize) -> i32;
        fn DefSubclassProc(hwnd: HWND, msg: u32, wp: usize, lp: isize) -> isize;
    }

    unsafe extern "system" fn subclass(
        hwnd: HWND,
        msg: u32,
        wp: usize,
        lp: isize,
        _id: usize,
        _data: usize,
    ) -> isize {
        // wParam != 0: the session really is ending (not a cancelled query).
        if msg == WM_ENDSESSION && wp != 0 {
            super::write_snapshot();
        }
        if msg == WM_NCDESTROY {
            // SAFETY: removing our own subclass from the window being destroyed.
            unsafe {
                RemoveWindowSubclass(hwnd, subclass, SUBCLASS_ID);
            }
        }
        // SAFETY: forwarding unchanged down the subclass chain.
        unsafe { DefSubclassProc(hwnd, msg, wp, lp) }
    }

    pub fn register(hwnd: isize) -> bool {
        let args: Vec<u16> = std::ffi::OsStr::new(super::RESTART_ARGS)
            .encode_wide()
            .chain(Some(0))
            .collect();
        // SAFETY: valid NUL-terminated string; `hwnd` is the root window, owned
        // by this (the UI) thread, as SetWindowSubclass requires.
        unsafe {
            SetWindowSubclass(hwnd as HWND, subclass, SUBCLASS_ID, 0);
            RegisterApplicationRestart(args.as_ptr(), super::RESTART_NO_CRASH | super::RESTART_NO_HANG) >= 0
        }
    }
}

#[cfg(not(windows))]
mod imp {
    pub fn register(_hwnd: isize) -> bool {
        false
    }
}

/// Register for restart and hook the root window's `WM_ENDSESSION`.
pub fn register(hwnd: isize) -> bool {
    imp::register(hwnd)
}

#[cfg(test)]
mod tests {
    #[test]
    fn the_restart_argument_is_the_cli_restore_flag() {
        let args = vec![super::RESTART_ARGS.to_string()];
        let cli = crate::cli::parse(&args, std::path::Path::new("C:\\"), None, &|_| false);
        assert!(cli.restore_session);
        assert!(cli.errors.is_empty());
        // And it never forwards to another instance: it *is* the restore.
        assert!(matches!(cli.plan(true), crate::cli::Plan::Local { .. }));
    }
}
