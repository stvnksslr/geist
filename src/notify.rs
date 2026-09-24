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

    const NIF_MESSAGE: u32 = 0x0001;
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
    /// `SetWindowCompositionAttribute`. Nothing else in geist needs shell32, so
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
            install_click_hook(hwnd);
            let mut add = base(hwnd);
            // NIF_MESSAGE routes the icon's events — including a click on the
            // balloon/toast — to `hwnd` as `CALLBACK_MSG`.
            add.flags = NIF_MESSAGE | NIF_ICON | NIF_TIP;
            add.callback_message = super::CALLBACK_MSG;
            add.icon = icon;
            wide_into("geist", &mut add.tip);
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

    type SubclassProc = unsafe extern "system" fn(HWND, u32, usize, isize, usize, usize) -> isize;
    const SUBCLASS_ID: usize = 0x6769_6e6f; // "gino"
    const WM_NCDESTROY: u32 = 0x0082;

    #[link(name = "comctl32")]
    unsafe extern "system" {
        fn SetWindowSubclass(hwnd: HWND, f: SubclassProc, id: usize, data: usize) -> i32;
        fn RemoveWindowSubclass(hwnd: HWND, f: SubclassProc, id: usize) -> i32;
        fn DefSubclassProc(hwnd: HWND, msg: u32, wp: usize, lp: isize) -> isize;
    }

    /// Catch the notification icon's callback message on the window it was
    /// registered with. Runs on the UI thread (inside winit's message loop), so
    /// it only records the click and wakes the app — the app, not a window
    /// procedure, decides what to focus.
    unsafe extern "system" fn subclass(
        hwnd: HWND,
        msg: u32,
        wp: usize,
        lp: isize,
        _id: usize,
        _data: usize,
    ) -> isize {
        if msg == super::CALLBACK_MSG {
            // Legacy (pre-`NOTIFYICON_VERSION_4`) callbacks put the event in
            // lParam's low word.
            if super::is_click_event((lp as usize & 0xFFFF) as u32) {
                super::note_click();
            }
            return 0;
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

    fn install_click_hook(hwnd: HWND) {
        // SAFETY: `hwnd` is the root window, owned by this (the UI) thread.
        // Re-installing the same proc/id only updates its data.
        unsafe {
            SetWindowSubclass(hwnd, subclass, SUBCLASS_ID, 0);
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

/// The window message the notification icon reports its events with
/// (`WM_APP + 0x47`).
pub const CALLBACK_MSG: u32 = 0x8000 + 0x47;

/// `NIN_BALLOONUSERCLICK`: the user clicked the balloon — on Windows 10/11, the
/// toast the shell renders it as.
pub const NIN_BALLOONUSERCLICK: u32 = 0x0405;
/// A left click on the tray icon itself (`WM_LBUTTONUP`), treated the same: the
/// icon only exists because something notified, so it means "take me there".
const WM_LBUTTONUP: u32 = 0x0202;

/// Whether an icon callback event is a click that should focus the pane the
/// last notification came from.
pub fn is_click_event(ev: u32) -> bool {
    ev == NIN_BALLOONUSERCLICK || ev == WM_LBUTTONUP
}

static CLICKED: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
static WAKER: std::sync::OnceLock<eframe::egui::Context> = std::sync::OnceLock::new();

/// Give the click hook a way to wake the app.
pub fn set_waker(ctx: &eframe::egui::Context) {
    let _ = WAKER.set(ctx.clone());
}

fn note_click() {
    CLICKED.store(true, std::sync::atomic::Ordering::Relaxed);
    if let Some(ctx) = WAKER.get() {
        ctx.request_repaint_of(eframe::egui::ViewportId::ROOT);
    }
}

/// Whether a notification was clicked since the last call.
pub fn take_clicked() -> bool {
    CLICKED.swap(false, std::sync::atomic::Ordering::Relaxed)
}

/// Upstream's `shouldPresentNotification`: a notification that requires focus
/// (every OSC 9 / 777 / 99 one) is shown only when the user *can't already see
/// it* — its window isn't the active one, or its pane isn't the focused one.
/// `notify-on-command-finish` notifications pass `require_focus = false`.
pub fn should_present(require_focus: bool, window_focused: bool, pane_focused: bool) -> bool {
    !require_focus || !window_focused || !pane_focused
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
    fn only_a_balloon_or_icon_click_counts_as_a_click() {
        use super::{NIN_BALLOONUSERCLICK, is_click_event};
        assert!(is_click_event(NIN_BALLOONUSERCLICK));
        assert!(is_click_event(0x0202));
        // NIN_BALLOONSHOW / HIDE / TIMEOUT, mouse move: not clicks.
        for ev in [0x0402, 0x0403, 0x0404, 0x0200] {
            assert!(!is_click_event(ev), "{ev:#x}");
        }
    }

    #[test]
    fn should_present_mirrors_upstream() {
        use super::should_present;
        // Focused window *and* focused pane: the user is looking at it.
        assert!(!should_present(true, true, true));
        assert!(should_present(true, false, true));
        assert!(should_present(true, true, false));
        // Command-finish notifications don't require focus.
        assert!(should_present(false, true, true));
    }

    #[test]
    fn a_one_char_budget_still_terminates() {
        assert_eq!(clamp("abcdef", 1), "…");
    }
}
