//! Out-of-band bell effects on Windows — the non-visual half of `bell-features`.
//!
//! Ghostty's `bell-features` has five members; three of them need the platform:
//!
//! - `system` → [`system_alert`]: `MessageBeep`, the user's configured alert
//!   sound. **Silent if their sound scheme is "No Sounds"** — worth knowing,
//!   because "nothing happened" is otherwise indistinguishable from a bug.
//! - `audio` → [`play_audio`]: `PlaySoundW` on a `.wav` file. It has **no volume
//!   parameter**, which is why `bell-audio-volume` is parsed and stored but not
//!   honored; honoring it needs a real audio backend (rodio/cpal) or XAudio2.
//! - `attention` → [`request_attention`] / [`clear_attention`]: `FlashWindowEx`,
//!   Windows' equivalent of the macOS dock bounce.
//!
//! `Beep` is deliberately **not** used: it synthesizes a square wave and blocks
//! the calling thread for its full duration, which on the UI thread would stall
//! rendering for as long as it sounds.
//!
//! No new `windows-sys` features are enabled for these three calls.
//! `Win32_System_Diagnostics_Debug` and `Win32_UI_WindowsAndMessaging` are large
//! modules of pure declarations, and winit's `windows-sys` is a *different major
//! version* from ours, so enabling them buys no dedup either. `user32` is already
//! linked by winit, so `MessageBeep`/`FlashWindowEx` are declared directly;
//! `winmm` is not, so `PlaySoundW` is resolved lazily and a missing DLL degrades
//! to silence rather than a load-time failure (the same treatment `blur.rs` gives
//! `SetWindowCompositionAttribute`).

#[cfg(windows)]
mod imp {
    use std::ffi::c_void;
    use std::os::windows::ffi::OsStrExt;
    use std::path::Path;
    use std::sync::OnceLock;
    use windows_sys::Win32::Foundation::HWND;
    use windows_sys::Win32::System::LibraryLoader::{GetProcAddress, LoadLibraryA};

    /// `MB_OK` — the plain "default beep" alert.
    const MB_OK: u32 = 0;

    const FLASHW_STOP: u32 = 0;
    /// Flash the taskbar button…
    const FLASHW_TRAY: u32 = 2;
    /// …and keep flashing until the window comes to the foreground.
    const FLASHW_TIMERNOFG: u32 = 12;

    const SND_ASYNC: u32 = 0x0001;
    /// Don't substitute the system default sound when the file is missing —
    /// otherwise a typo'd `bell-audio-path` sounds like it worked.
    const SND_NODEFAULT: u32 = 0x0002;
    const SND_FILENAME: u32 = 0x0002_0000;

    #[repr(C)]
    struct FlashWInfo {
        cb_size: u32,
        hwnd: HWND,
        flags: u32,
        count: u32,
        timeout: u32,
    }
    // `cbSize` must match what the OS expects; a wrong size makes user32 read
    // past the struct. Pin the layout rather than trusting the field list.
    const _: () = assert!(size_of::<FlashWInfo>() == 32);

    // user32 is already linked by winit, so these cost nothing beyond the import.
    #[link(name = "user32")]
    unsafe extern "system" {
        fn MessageBeep(utype: u32) -> i32;
        fn FlashWindowEx(pfwi: *const FlashWInfo) -> i32;
    }

    type PlaySoundW = unsafe extern "system" fn(*const u16, *mut c_void, u32) -> i32;

    /// Resolve `winmm!PlaySoundW` once. Loaded dynamically so a system without
    /// winmm (or a stripped container image) simply plays nothing.
    fn play_sound_w() -> Option<PlaySoundW> {
        static F: OnceLock<Option<usize>> = OnceLock::new();
        // Parenthesized: `?` binds tighter than `*`.
        let addr = (*F.get_or_init(|| unsafe {
            let winmm = LoadLibraryA(c"winmm.dll".as_ptr() as *const u8);
            if winmm.is_null() {
                return None;
            }
            GetProcAddress(winmm, c"PlaySoundW".as_ptr() as *const u8).map(|p| p as usize)
        }))?;
        // SAFETY: the address came from GetProcAddress for this exact export.
        Some(unsafe { std::mem::transmute::<usize, PlaySoundW>(addr) })
    }

    pub fn system_alert() {
        // SAFETY: no pointers; MB_OK is a documented value.
        unsafe {
            MessageBeep(MB_OK);
        }
    }

    pub fn play_audio(path: &Path) {
        let Some(f) = play_sound_w() else {
            return;
        };
        // PlaySoundW takes a NUL-terminated wide string.
        let mut wide: Vec<u16> = path.as_os_str().encode_wide().collect();
        wide.push(0);
        // SAFETY: `wide` outlives the call and is NUL-terminated; a null module
        // handle is correct for SND_FILENAME. SND_ASYNC returns immediately, so
        // the UI thread never blocks on playback.
        unsafe {
            f(wide.as_ptr(), std::ptr::null_mut(), SND_FILENAME | SND_ASYNC | SND_NODEFAULT);
        }
    }

    fn flash(hwnd: isize, flags: u32) {
        let info = FlashWInfo {
            cb_size: size_of::<FlashWInfo>() as u32,
            hwnd: hwnd as HWND,
            flags,
            count: 0,
            timeout: 0,
        };
        // SAFETY: a valid HWND and a fully-initialized FLASHWINFO whose cbSize
        // is derived from the type.
        unsafe {
            FlashWindowEx(&info);
        }
    }

    pub fn request_attention(hwnd: isize) {
        flash(hwnd, FLASHW_TRAY | FLASHW_TIMERNOFG);
    }

    pub fn clear_attention(hwnd: isize) {
        flash(hwnd, FLASHW_STOP);
    }
}

#[cfg(not(windows))]
mod imp {
    use std::path::Path;

    pub fn system_alert() {}
    pub fn play_audio(_path: &Path) {}
    pub fn request_attention(_hwnd: isize) {}
    pub fn clear_attention(_hwnd: isize) {}
}

use std::path::{Path, PathBuf};

/// Play the OS alert sound (`bell-features = system`).
pub fn system_alert() {
    imp::system_alert();
}

/// Play a sound file (`bell-features = audio` + `bell-audio-path`).
pub fn play_audio(path: &Path) {
    imp::play_audio(path);
}

/// Start flashing the taskbar button until the window is focused
/// (`bell-features = attention`).
pub fn request_attention(hwnd: isize) {
    imp::request_attention(hwnd);
}

/// Stop the taskbar flash.
pub fn clear_attention(hwnd: isize) {
    imp::clear_attention(hwnd);
}

/// Resolve a configured `bell-audio-path` against `config_dir`.
///
/// Relative paths resolve against the config file's directory (Ghostty resolves
/// `Path` values the same way), so a config can ship a sound beside itself.
pub fn resolve_audio_path(raw: &str, config_dir: Option<&Path>) -> Option<PathBuf> {
    let raw = raw.trim();
    if raw.is_empty() {
        return None;
    }
    let p = Path::new(raw);
    if p.is_absolute() {
        return Some(p.to_path_buf());
    }
    Some(match config_dir {
        Some(dir) => dir.join(p),
        None => p.to_path_buf(),
    })
}

#[cfg(test)]
mod tests {
    use super::resolve_audio_path;
    use std::path::Path;

    #[test]
    fn audio_path_resolves_relative_against_the_config_dir() {
        let dir = Path::new(r"C:\Users\me\AppData\Roaming\giest");
        assert_eq!(
            resolve_audio_path("ding.wav", Some(dir)),
            Some(dir.join("ding.wav"))
        );
        // An absolute path is taken as-is.
        assert_eq!(
            resolve_audio_path(r"C:\sounds\ding.wav", Some(dir)),
            Some(Path::new(r"C:\sounds\ding.wav").to_path_buf())
        );
        // Empty means "no sound", not "the config dir".
        assert_eq!(resolve_audio_path("   ", Some(dir)), None);
    }
}
