//! Desktop notifications on Windows — the presentation half of `OSC 9` /
//! `OSC 777` (parsing lives in [`crate::osc_notify`]).
//!
//! **Why a notification-area balloon and not a WinRT toast.** The modern
//! `ToastNotificationManager` API requires the process to have a registered
//! *AppUserModelID*, which in practice means installing a Start Menu shortcut
//! carrying that ID — a permanent, machine-wide side effect that a terminal you
//! might have just unzipped has no business creating. `Shell_NotifyIconW` with
//! `NIF_INFO` needs nothing but a window handle, and on Windows 10 and 11 the
//! shell renders those balloons *as toasts* anyway, so the visible result is the
//! same thing Ghostty's `NSUserNotification`/`libnotify` paths produce.
//!
//! The cost is one notification-area icon, which is why it is added **lazily**:
//! nothing appears in the tray unless a program actually asks for a
//! notification, and `shutdown` removes it. A user who never runs anything that
//! notifies never sees it.
//!
//! Like [`crate::bell`], `shell32` is resolved lazily via `GetProcAddress`
//! rather than linked, so a system without it degrades to "no notification"
//! instead of failing to start.

/// Windows truncates both fields, silently, at these lengths (they are the
/// `szInfoTitle`/`szInfo` array sizes in `NOTIFYICONDATAW`, minus the NUL).
/// Truncating here instead means the *end* of a long message is dropped
/// deliberately rather than by an off-by-one deep in a `#[repr(C)]` copy.
pub const MAX_TITLE_CHARS: usize = 63;
pub const MAX_BODY_CHARS: usize = 255;

/// Clamp a notification field to `max` *characters*, appending an ellipsis when
/// anything was dropped.
///
/// Counts `char`s, not bytes: the buffer is UTF-16 and the text is arbitrary
/// prose, so a byte cap would cut a multi-byte character in half. (A char
/// outside the BMP still costs two UTF-16 units, so this is a slight
/// over-estimate of the budget — deliberate, since erring long is what
/// truncates, and erring short only wastes a few cells.)
pub fn clamp(text: &str, max: usize) -> String {
    let n = text.chars().count();
    if n <= max {
        return text.to_string();
    }
    let mut out: String = text.chars().take(max.saturating_sub(1)).collect();
    out.push('…');
    out
}

#[cfg(windows)]
mod imp {
    use std::ffi::c_void;
    use std::os::windows::ffi::OsStrExt;
    use std::sync::OnceLock;
    use std::sync::atomic::{AtomicBool, Ordering};

    use windows_sys::Win32::Foundation::HWND;
    use windows_sys::Win32::System::LibraryLoader::{GetProcAddress, LoadLibraryA};

    const NIM_ADD: u32 = 0x0000;
    const NIM_MODIFY: u32 = 0x0001;
    const NIM_DELETE: u32 = 0x0002;

    const NIF_ICON: u32 = 0x0002;
    const NIF_TIP: u32 = 0x0004;
    const NIF_INFO: u32 = 0x0010;

    /// Use the tray icon as the notification's image.
    const NIIF_USER: u32 = 0x0004;
    /// Don't play a sound. The bell is a *separate* configurable feature
    /// (`bell-features = system`), so a notification making its own noise would
    /// double up on anything that rings and notifies together.
    const NIIF_NOSOUND: u32 = 0x0010;

    /// Our icon's id within the window. Any value works; it just has to be
    /// stable across the add/modify/delete calls.
    const ICON_UID: u32 = 1;

    const IDI_APPLICATION: *const u16 = 32512 as *const u16;

    /// `NOTIFYICONDATAW`, laid out for the Vista+ (`V3`) size — the version
    /// that has `szInfo`/`szInfoTitle`, i.e. balloon support.
    #[repr(C)]
    struct NotifyIconDataW {
        cb_size: u32,
        hwnd: HWND,
        uid: u32,
        flags: u32,
        callback_message: u32,
        icon: *mut c_void,
        tip: [u16; 128],
        state: u32,
        state_mask: u32,
        info: [u16; 256],
        /// A union of `uTimeout` and `uVersion` in the SDK header.
        timeout_or_version: u32,
        info_title: [u16; 64],
        info_flags: u32,
        guid_item: [u8; 16],
        balloon_icon: *mut c_void,
    }

    // `cbSize` selects which struct version the shell reads, so a wrong size
    // makes shell32 read past the end or reject the call outright. Pin it.
    const _: () = assert!(size_of::<NotifyIconDataW>() == 976);

    // user32 is already linked by winit (see the same note in `bell.rs`).
    #[link(name = "user32")]
    unsafe extern "system" {
        fn LoadIconW(hinstance: *mut c_void, name: *const u16) -> *mut c_void;
        fn MessageBoxW(hwnd: isize, text: *const u16, caption: *const u16, kind: u32) -> i32;
    }

    /// A blocking, native error box. Used when the GPU is gone and egui can
    /// no longer draw anything of its own to say so.
    pub fn error_box(hwnd: isize, title: &str, body: &str) {
        const MB_OK: u32 = 0x0;
        const MB_ICONERROR: u32 = 0x10;
        let wide = |s: &str| s.encode_utf16().chain(Some(0)).collect::<Vec<u16>>();
        let (t, b) = (wide(title), wide(body));
        // SAFETY: both strings are NUL-terminated UTF-16 that outlive the call;
        // `hwnd` may be 0, which parents the box to the desktop.
        unsafe {
            MessageBoxW(hwnd, b.as_ptr(), t.as_ptr(), MB_OK | MB_ICONERROR);
        }
    }

    type ShellNotifyIconW = unsafe extern "system" fn(u32, *const NotifyIconDataW) -> i32;

    /// Resolve `shell32!Shell_NotifyIconW` once, dynamically — the same
    /// treatment `bell.rs` gives `PlaySoundW` and `blur.rs` gives
    /// `SetWindowCompositionAttribute`. Nothing else in giest needs shell32, so
    /// linking it would be a load-time dependency bought for one function.
    fn shell_notify_icon_w() -> Option<ShellNotifyIconW> {
        static F: OnceLock<Option<usize>> = OnceLock::new();
        let addr = (*F.get_or_init(|| unsafe {
            let shell32 = LoadLibraryA(c"shell32.dll".as_ptr() as *const u8);
            if shell32.is_null() {
                return None;
            }
            GetProcAddress(shell32, c"Shell_NotifyIconW".as_ptr() as *const u8).map(|p| p as usize)
        }))?;
        // SAFETY: the address came from GetProcAddress for this exact export.
        Some(unsafe { std::mem::transmute::<usize, ShellNotifyIconW>(addr) })
    }

    /// Whether our notification-area icon is currently registered. Process-wide
    /// because it is keyed on one `HWND` + uid pair and there is only ever one
    /// root window; secondary windows have no reachable handle anyway.
    static ICON_ADDED: AtomicBool = AtomicBool::new(false);

    /// Copy a `&str` into a fixed-size UTF-16 array, NUL-terminated. Truncates
    /// at the array bound; callers pre-clamp so this is only a backstop.
    fn wide_into<const N: usize>(text: &str, out: &mut [u16; N]) {
        let mut i = 0;
        for u in std::ffi::OsStr::new(text).encode_wide() {
            if i + 1 >= N {
                break;
            }
            out[i] = u;
            i += 1;
        }
        out[i] = 0;
    }

    fn base(hwnd: HWND) -> NotifyIconDataW {
        NotifyIconDataW {
            cb_size: size_of::<NotifyIconDataW>() as u32,
            hwnd,
            uid: ICON_UID,
            flags: 0,
            callback_message: 0,
            icon: std::ptr::null_mut(),
            tip: [0; 128],
            state: 0,
            state_mask: 0,
            info: [0; 256],
            timeout_or_version: 0,
            info_title: [0; 64],
            info_flags: 0,
            guid_item: [0; 16],
            balloon_icon: std::ptr::null_mut(),
        }
    }

    pub fn show(hwnd: isize, title: &str, body: &str) {
        let Some(f) = shell_notify_icon_w() else {
            return;
        };
        let hwnd = hwnd as HWND;
        // SAFETY: a null hinstance with a predefined IDI_* is the documented
        // way to load a system icon; the result is a shared icon we must not
        // destroy.
        let icon = unsafe { LoadIconW(std::ptr::null_mut(), IDI_APPLICATION) };

        // Register the icon on first use only. A terminal that never notifies
        // must not put anything in the user's notification area.
        if !ICON_ADDED.load(Ordering::Relaxed) {
            let mut add = base(hwnd);
            add.flags = NIF_ICON | NIF_TIP;
            add.icon = icon;
            wide_into("giest", &mut add.tip);
            // SAFETY: `add` is a fully initialized NOTIFYICONDATAW whose
            // cb_size matches its layout, and it outlives the call.
            if unsafe { f(NIM_ADD, &add) } == 0 {
                return;
            }
            ICON_ADDED.store(true, Ordering::Relaxed);
        }

        let mut data = base(hwnd);
        data.flags = NIF_INFO;
        data.info_flags = NIIF_USER | NIIF_NOSOUND;
        data.balloon_icon = icon;
        wide_into(title, &mut data.info_title);
        wide_into(body, &mut data.info);
        // SAFETY: as above.
        unsafe {
            f(NIM_MODIFY, &data);
        }
    }

    pub fn shutdown(hwnd: isize) {
        if !ICON_ADDED.swap(false, Ordering::Relaxed) {
            return;
        }
        let Some(f) = shell_notify_icon_w() else {
            return;
        };
        let data = base(hwnd as HWND);
        // SAFETY: as above; NIM_DELETE reads only hwnd + uid.
        unsafe {
            f(NIM_DELETE, &data);
        }
    }
}

#[cfg(not(windows))]
mod imp {
    pub fn show(_hwnd: isize, _title: &str, _body: &str) {}
    pub fn shutdown(_hwnd: isize) {}
    pub fn error_box(_hwnd: isize, _title: &str, _body: &str) {}
}

/// Show a blocking native error dialog, for failures egui cannot draw.
pub fn error_box(hwnd: isize, title: &str, body: &str) {
    imp::error_box(hwnd, title, body);
}

/// Raise a desktop notification. `title` may be empty (the OSC 9 form carries
/// only a body), in which case Windows shows the body alone.
///
/// Best-effort by design: a failure here is not worth surfacing to the user, and
/// every step degrades to "no notification" rather than an error.
pub fn show(hwnd: isize, title: &str, body: &str) {
    if title.is_empty() && body.is_empty() {
        return;
    }
    imp::show(
        hwnd,
        &clamp(title, MAX_TITLE_CHARS),
        &clamp(body, MAX_BODY_CHARS),
    );
}

/// Remove the notification-area icon, if one was ever added. Called when the
/// last window closes, so a stale icon isn't left behind for the shell to
/// garbage-collect on hover.
pub fn shutdown(hwnd: isize) {
    imp::shutdown(hwnd);
}

#[cfg(test)]
mod tests {
    use super::{MAX_BODY_CHARS, MAX_TITLE_CHARS, clamp};

    #[test]
    fn short_text_is_untouched() {
        assert_eq!(clamp("build ok", MAX_TITLE_CHARS), "build ok");
        assert_eq!(clamp("", MAX_BODY_CHARS), "");
        // Exactly at the limit still passes through whole.
        let exact: String = "x".repeat(MAX_TITLE_CHARS);
        assert_eq!(clamp(&exact, MAX_TITLE_CHARS), exact);
    }

    #[test]
    fn long_text_is_clamped_with_an_ellipsis() {
        let long: String = "x".repeat(MAX_TITLE_CHARS + 50);
        let got = clamp(&long, MAX_TITLE_CHARS);
        assert_eq!(got.chars().count(), MAX_TITLE_CHARS);
        assert!(got.ends_with('…'));
    }

    #[test]
    fn clamping_counts_chars_not_bytes() {
        // 100 three-byte chars: 300 bytes, but well inside a 255-*char* cap, so
        // a byte-based cap would truncate this and could split a character.
        let cjk: String = "漢".repeat(100);
        assert_eq!(clamp(&cjk, MAX_BODY_CHARS), cjk);

        // And when it does cut, it cuts on a char boundary.
        let got = clamp(&cjk, 10);
        assert_eq!(got.chars().count(), 10);
        assert!(got.starts_with("漢漢"));
    }

    #[test]
    fn a_one_char_budget_still_terminates() {
        assert_eq!(clamp("abcdef", 1), "…");
    }
}
