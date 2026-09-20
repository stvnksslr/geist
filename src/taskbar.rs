//! Windows taskbar progress for `OSC 9;4` (see [`crate::osc_notify`]).
//!
//! The taskbar button *is* the Windows progress indicator — it is where ConEmu,
//! which invented `OSC 9;4`, puts it, and where Windows Terminal puts it, so it
//! is what a Windows user already reads without being told. Ghostty draws a
//! progress bar inside its own window instead; that's the right call on macOS
//! and GTK, which have no equivalent, and the wrong one here.
//!
//! **Why the COM vtable is declared by hand.** `ITaskbarList3` is a COM
//! interface, and `windows-sys` deliberately ships no COM interfaces at all
//! (they live in the much heavier `windows` crate). Rather than take that
//! dependency for two methods, this declares the vtable prefix it needs — the
//! same trade `bell.rs` makes for `FlashWindowEx` and `blur.rs` for
//! `SetWindowCompositionAttribute`. `ole32` is resolved lazily via
//! `GetProcAddress` for the same reason: a system without it degrades to "no
//! progress indicator" rather than failing to start.

/// What to show on the taskbar button. Mirrors `TBPFLAG`, and deliberately not
/// [`crate::osc_notify::ProgressState`] — that one is the wire format, this one
/// is the platform's.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Progress {
    /// No indicator.
    None,
    /// A determinate bar at `0..=100` percent.
    Normal(u8),
    /// Red, at `0..=100` percent (Windows still wants a value; a failure with
    /// no reported percentage shows a full red bar, which is what ConEmu does).
    Error(u8),
    /// Yellow, at `0..=100` percent.
    Paused(u8),
    /// The marquee/barber-pole animation, for work of unknown length.
    Indeterminate,
}

impl Progress {
    /// Translate a wire-format `OSC 9;4` report.
    ///
    /// `last` is the percentage currently displayed: `error` and `pause` may
    /// arrive with no percentage of their own, and Windows has no "keep the
    /// value, change the colour" call — so carrying the last one forward is what
    /// keeps a failing job's bar where it actually stopped instead of snapping
    /// it to full.
    pub fn from_report(r: crate::osc_notify::ProgressReport, last: u8) -> Self {
        use crate::osc_notify::ProgressState as S;
        match r.state {
            S::Remove => Self::None,
            S::Indeterminate => Self::Indeterminate,
            S::Set => Self::Normal(r.value.unwrap_or(last).min(100)),
            S::Error => Self::Error(r.value.unwrap_or(last).min(100)),
            S::Pause => Self::Paused(r.value.unwrap_or(last).min(100)),
        }
    }

    /// The percentage this state displays, for carrying forward to a later
    /// report that omits one.
    pub fn value(self) -> Option<u8> {
        match self {
            Self::Normal(v) | Self::Error(v) | Self::Paused(v) => Some(v),
            Self::None | Self::Indeterminate => None,
        }
    }
}

#[cfg(windows)]
mod imp {
    use std::cell::Cell;
    use std::ffi::c_void;
    use std::sync::OnceLock;

    use windows_sys::Win32::System::LibraryLoader::{GetProcAddress, LoadLibraryA};

    use super::Progress;

    #[repr(C)]
    #[derive(Clone, Copy)]
    struct Guid {
        data1: u32,
        data2: u16,
        data3: u16,
        data4: [u8; 8],
    }

    /// `CLSID_TaskbarList` — {56FDF344-FD6D-11d0-958A-006097C9A090}
    const CLSID_TASKBAR_LIST: Guid = Guid {
        data1: 0x56FD_F344,
        data2: 0xFD6D,
        data3: 0x11d0,
        data4: [0x95, 0x8A, 0x00, 0x60, 0x97, 0xC9, 0xA0, 0x90],
    };
    /// `IID_ITaskbarList3` — {EA1AFB91-9E28-4B86-90E9-9E9F8A5EEFAF}
    const IID_ITASKBAR_LIST3: Guid = Guid {
        data1: 0xEA1A_FB91,
        data2: 0x9E28,
        data3: 0x4B86,
        data4: [0x90, 0xE9, 0x9E, 0x9F, 0x8A, 0x5E, 0xEF, 0xAF],
    };

    const CLSCTX_INPROC_SERVER: u32 = 0x1;
    const COINIT_APARTMENTTHREADED: u32 = 0x2;

    // TBPFLAG
    const TBPF_NOPROGRESS: i32 = 0x0;
    const TBPF_INDETERMINATE: i32 = 0x1;
    const TBPF_NORMAL: i32 = 0x2;
    const TBPF_ERROR: i32 = 0x4;
    const TBPF_PAUSED: i32 = 0x8;

    /// The prefix of `ITaskbarList3`'s vtable up to the last method we call
    /// (`SetOverlayIcon`, slot 18).
    ///
    /// The layout is `IUnknown` → `ITaskbarList` → `ITaskbarList2` →
    /// `ITaskbarList3`, in declaration order, and **every** earlier method must
    /// be present and correctly shaped or the two we want sit at the wrong slot
    /// and we call something else entirely. The tail of the interface
    /// (`RegisterTab` onward) is omitted, which is safe: we only ever index
    /// *into* this prefix.
    #[repr(C)]
    struct ITaskbarList3Vtbl {
        query_interface:
            unsafe extern "system" fn(*mut c_void, *const Guid, *mut *mut c_void) -> i32,
        add_ref: unsafe extern "system" fn(*mut c_void) -> u32,
        release: unsafe extern "system" fn(*mut c_void) -> u32,
        // ITaskbarList
        hr_init: unsafe extern "system" fn(*mut c_void) -> i32,
        add_tab: unsafe extern "system" fn(*mut c_void, isize) -> i32,
        delete_tab: unsafe extern "system" fn(*mut c_void, isize) -> i32,
        activate_tab: unsafe extern "system" fn(*mut c_void, isize) -> i32,
        set_active_alt: unsafe extern "system" fn(*mut c_void, isize) -> i32,
        // ITaskbarList2
        mark_fullscreen_window: unsafe extern "system" fn(*mut c_void, isize, i32) -> i32,
        // ITaskbarList3, in declaration order (ShObjIdl_core.h). The six
        // between the progress pair and `SetOverlayIcon` are never called, but
        // must be present: each one shifts the slot `set_overlay_icon` lands in.
        set_progress_value: unsafe extern "system" fn(*mut c_void, isize, u64, u64) -> i32,
        set_progress_state: unsafe extern "system" fn(*mut c_void, isize, i32) -> i32,
        register_tab: unsafe extern "system" fn(*mut c_void, isize, isize) -> i32,
        unregister_tab: unsafe extern "system" fn(*mut c_void, isize) -> i32,
        set_tab_order: unsafe extern "system" fn(*mut c_void, isize, isize) -> i32,
        set_tab_active: unsafe extern "system" fn(*mut c_void, isize, isize, u32) -> i32,
        thumb_bar_add_buttons:
            unsafe extern "system" fn(*mut c_void, isize, u32, *mut c_void) -> i32,
        thumb_bar_update_buttons:
            unsafe extern "system" fn(*mut c_void, isize, u32, *mut c_void) -> i32,
        thumb_bar_set_image_list: unsafe extern "system" fn(*mut c_void, isize, *mut c_void) -> i32,
        /// Slot 18: `SetOverlayIcon(HWND, HICON, LPCWSTR)`.
        set_overlay_icon:
            unsafe extern "system" fn(*mut c_void, isize, *mut c_void, *const u16) -> i32,
    }

    #[repr(C)]
    struct IUnknownLayout {
        vtbl: *const ITaskbarList3Vtbl,
    }

    type CoInitializeEx = unsafe extern "system" fn(*mut c_void, u32) -> i32;
    type CoCreateInstance = unsafe extern "system" fn(
        *const Guid,
        *mut c_void,
        u32,
        *const Guid,
        *mut *mut c_void,
    ) -> i32;

    fn ole32() -> Option<(CoInitializeEx, CoCreateInstance)> {
        static F: OnceLock<Option<(usize, usize)>> = OnceLock::new();
        let (init, create) = (*F.get_or_init(|| unsafe {
            let ole = LoadLibraryA(c"ole32.dll".as_ptr() as *const u8);
            if ole.is_null() {
                return None;
            }
            let init = GetProcAddress(ole, c"CoInitializeEx".as_ptr() as *const u8)?;
            let create = GetProcAddress(ole, c"CoCreateInstance".as_ptr() as *const u8)?;
            Some((init as usize, create as usize))
        }))?;
        // SAFETY: both addresses came from GetProcAddress for those exact exports.
        unsafe {
            Some((
                std::mem::transmute::<usize, CoInitializeEx>(init),
                std::mem::transmute::<usize, CoCreateInstance>(create),
            ))
        }
    }

    thread_local! {
        /// The taskbar interface for *this* thread, created on first use.
        ///
        /// Thread-local because a COM apartment is per-thread and this is only
        /// ever called from the UI thread. `Cell<Option<..>>` with a null
        /// sentinel distinguishes "not tried yet" from "tried and failed", so a
        /// machine without a taskbar doesn't retry `CoCreateInstance` on every
        /// progress report.
        static TASKBAR: Cell<Option<*mut c_void>> = const { Cell::new(None) };
    }

    fn taskbar() -> Option<*mut c_void> {
        TASKBAR.with(|slot| {
            if let Some(p) = slot.get() {
                return if p.is_null() { None } else { Some(p) };
            }
            let created = create();
            slot.set(Some(created.unwrap_or(std::ptr::null_mut())));
            created
        })
    }

    fn create() -> Option<*mut c_void> {
        let (co_init, co_create) = ole32()?;
        // SAFETY: a null reserved pointer is the documented call. winit already
        // initializes an STA on this thread, so this usually returns S_FALSE
        // (already initialized) or RPC_E_CHANGED_MODE; both are fine and we
        // deliberately never call CoUninitialize, since we don't own the
        // apartment.
        unsafe {
            co_init(std::ptr::null_mut(), COINIT_APARTMENTTHREADED);
        }
        let mut ptr: *mut c_void = std::ptr::null_mut();
        // SAFETY: the CLSID/IID are the documented constants and `ptr` is a
        // valid out-parameter.
        let hr = unsafe {
            co_create(
                &CLSID_TASKBAR_LIST,
                std::ptr::null_mut(),
                CLSCTX_INPROC_SERVER,
                &IID_ITASKBAR_LIST3,
                &mut ptr,
            )
        };
        if hr < 0 || ptr.is_null() {
            return None;
        }
        // `HrInit` must succeed before any other method; if it doesn't, release
        // rather than keep a half-live interface around.
        // SAFETY: `ptr` is a live ITaskbarList3 from CoCreateInstance.
        unsafe {
            let vtbl = (*(ptr as *mut IUnknownLayout)).vtbl;
            if ((*vtbl).hr_init)(ptr) < 0 {
                ((*vtbl).release)(ptr);
                return None;
            }
        }
        Some(ptr)
    }

    pub fn available() -> bool {
        taskbar().is_some()
    }

    #[repr(C)]
    struct IconInfo {
        f_icon: i32,
        x_hotspot: u32,
        y_hotspot: u32,
        hbm_mask: *mut c_void,
        hbm_color: *mut c_void,
    }

    #[link(name = "user32")]
    unsafe extern "system" {
        fn CreateIconIndirect(info: *const IconInfo) -> *mut c_void;
    }

    /// The badge icon, built once per process from [`super::overlay_pixels`].
    /// Stored as an address because a raw pointer is not `Sync`; the icon is
    /// never destroyed (one 16x16 icon for the life of the process).
    fn badge_icon() -> Option<*mut c_void> {
        use windows_sys::Win32::Graphics::Gdi::{CreateBitmap, DeleteObject};
        static ICON: OnceLock<usize> = OnceLock::new();
        let p = *ICON.get_or_init(|| {
            let n = super::OVERLAY_SIZE;
            let color = super::overlay_pixels(n);
            let mask = vec![0u8; (n * n / 8) as usize];
            // SAFETY: both buffers are exactly the size the bitmaps describe
            // (32bpp colour; a 1bpp all-zero mask, rows word-aligned at n=16),
            // and the bitmaps are deleted once the icon has copied them.
            unsafe {
                let hbm_color = CreateBitmap(n as i32, n as i32, 1, 32, color.as_ptr().cast());
                let hbm_mask = CreateBitmap(n as i32, n as i32, 1, 1, mask.as_ptr().cast());
                if hbm_color.is_null() || hbm_mask.is_null() {
                    return 0;
                }
                let info = IconInfo {
                    f_icon: 1,
                    x_hotspot: 0,
                    y_hotspot: 0,
                    hbm_mask: hbm_mask as *mut c_void,
                    hbm_color: hbm_color as *mut c_void,
                };
                let icon = CreateIconIndirect(&info);
                DeleteObject(hbm_color);
                DeleteObject(hbm_mask);
                icon as usize
            }
        });
        (p != 0).then_some(p as *mut c_void)
    }

    #[cfg(test)]
    #[test]
    fn set_overlay_icon_is_slot_18() {
        // A wrong slot calls some other method with these arguments and fails
        // silently, so pin the layout: 3 IUnknown + 5 ITaskbarList + 1
        // ITaskbarList2 + 10 ITaskbarList3 slots, SetOverlayIcon the last.
        let p = std::mem::size_of::<usize>();
        assert_eq!(
            std::mem::offset_of!(ITaskbarList3Vtbl, set_progress_value),
            9 * p
        );
        assert_eq!(
            std::mem::offset_of!(ITaskbarList3Vtbl, set_overlay_icon),
            18 * p
        );
        assert_eq!(std::mem::size_of::<ITaskbarList3Vtbl>(), 19 * p);
    }

    pub fn set_overlay(hwnd: isize, on: bool) {
        let Some(ptr) = taskbar() else {
            return;
        };
        let icon = if on {
            match badge_icon() {
                Some(i) => i,
                None => return,
            }
        } else {
            std::ptr::null_mut()
        };
        // The accessibility text Windows reads for the badge.
        let desc: Vec<u16> = "Bell".encode_utf16().chain(Some(0)).collect();
        // SAFETY: `ptr` is a live ITaskbarList3; `icon` is a valid HICON or
        // null (which removes the overlay); `desc` is NUL-terminated.
        unsafe {
            let vtbl = (*(ptr as *mut IUnknownLayout)).vtbl;
            ((*vtbl).set_overlay_icon)(ptr, hwnd, icon, desc.as_ptr());
        }
    }

    pub fn set(hwnd: isize, progress: Progress) {
        let Some(ptr) = taskbar() else {
            return;
        };
        let (state, value) = match progress {
            Progress::None => (TBPF_NOPROGRESS, None),
            Progress::Indeterminate => (TBPF_INDETERMINATE, None),
            Progress::Normal(v) => (TBPF_NORMAL, Some(v)),
            Progress::Error(v) => (TBPF_ERROR, Some(v)),
            Progress::Paused(v) => (TBPF_PAUSED, Some(v)),
        };
        // SAFETY: `ptr` is a live ITaskbarList3 (HrInit already succeeded) and
        // the vtable prefix above matches the interface's declaration order.
        unsafe {
            let vtbl = (*(ptr as *mut IUnknownLayout)).vtbl;
            // Value before state: setting a value implicitly switches the button
            // to `normal`, so doing it the other way round would discard an
            // `error` or `paused` colour immediately after setting it.
            if let Some(v) = value {
                ((*vtbl).set_progress_value)(ptr, hwnd, u64::from(v), 100);
            }
            ((*vtbl).set_progress_state)(ptr, hwnd, state);
        }
    }
}

#[cfg(not(windows))]
mod imp {
    pub fn available() -> bool {
        false
    }
    pub fn set(_hwnd: isize, _progress: super::Progress) {}
    pub fn set_overlay(_hwnd: isize, _on: bool) {}
}

/// Show `progress` on the window's taskbar button. Best-effort: every failure
/// path degrades to no indicator rather than surfacing an error.
pub fn set(hwnd: isize, progress: Progress) {
    imp::set(hwnd, progress);
}

/// Show or clear the taskbar button's overlay badge (`SetOverlayIcon`): a
/// small dot meaning "a bell rang in this window while you were elsewhere".
/// Best-effort like [`set`].
pub fn set_overlay(hwnd: isize, on: bool) {
    imp::set_overlay(hwnd, on);
}

/// The badge's side, in pixels. The shell scales overlays to the small-icon
/// size; 16 is that size at 100%.
pub const OVERLAY_SIZE: u32 = 16;

/// The badge bitmap: a filled, anti-aliased red dot on transparency, as
/// top-down 32bpp `0xAARRGGBB` words (straight alpha, which is what
/// `CreateIconIndirect` wants for a 32-bit colour bitmap).
pub fn overlay_pixels(n: u32) -> Vec<u32> {
    let c = n as f32 / 2.0;
    let r = c - 1.0;
    let mut px = Vec::with_capacity((n * n) as usize);
    for y in 0..n {
        for x in 0..n {
            let (dx, dy) = (x as f32 + 0.5 - c, y as f32 + 0.5 - c);
            let d = (dx * dx + dy * dy).sqrt();
            let a = (r + 0.5 - d).clamp(0.0, 1.0);
            let a8 = (a * 255.0).round() as u32;
            px.push(if a8 == 0 { 0 } else { (a8 << 24) | 0x00E5_484D });
        }
    }
    px
}

/// Whether the taskbar interface could be created on this thread.
///
/// Exists so the COM plumbing can actually be *tested*: there is no API to read
/// a taskbar button's progress back, so the only thing a test can check is that
/// `CoCreateInstance` and `HrInit` both succeeded — which between them exercise
/// the CLSID, the IID, and the vtable layout down to `HrInit`'s slot. A wrong
/// vtable prefix would otherwise fail completely silently.
pub fn available() -> bool {
    imp::available()
}

#[cfg(test)]
mod tests {
    use super::Progress;
    use crate::osc_notify::{ProgressReport, ProgressState};

    fn report(state: ProgressState, value: Option<u8>) -> ProgressReport {
        ProgressReport { state, value }
    }

    #[test]
    fn wire_states_map_to_taskbar_states() {
        let f = |s, v| Progress::from_report(report(s, v), 0);
        assert_eq!(f(ProgressState::Remove, None), Progress::None);
        assert_eq!(
            f(ProgressState::Indeterminate, None),
            Progress::Indeterminate
        );
        assert_eq!(f(ProgressState::Set, Some(42)), Progress::Normal(42));
        assert_eq!(f(ProgressState::Error, Some(42)), Progress::Error(42));
        assert_eq!(f(ProgressState::Pause, Some(42)), Progress::Paused(42));
    }

    #[test]
    fn a_state_change_without_a_percentage_keeps_the_last_one() {
        // `9;4;2` (failed) after `9;4;1;70` must leave the bar at 70 and turn it
        // red — not snap it to 0 or to full. Windows has no "recolour in place"
        // call, so the value has to be carried forward by the caller.
        let r = Progress::from_report(report(ProgressState::Error, None), 70);
        assert_eq!(r, Progress::Error(70));
        let r = Progress::from_report(report(ProgressState::Pause, None), 70);
        assert_eq!(r, Progress::Paused(70));
    }

    #[test]
    fn percentages_are_clamped() {
        assert_eq!(
            Progress::from_report(report(ProgressState::Set, Some(200)), 0),
            Progress::Normal(100)
        );
        // …including one carried forward from a bogus previous value.
        assert_eq!(
            Progress::from_report(report(ProgressState::Error, None), 200),
            Progress::Error(100)
        );
    }

    /// Real COM: creates `ITaskbarList3` and calls `HrInit`, which is the only
    /// automatable check that the CLSID, IID and vtable layout are right (a
    /// taskbar button's progress cannot be read back). Ignored because it needs
    /// a desktop session with a shell — it fails in a service or over plain SSH.
    #[test]
    #[ignore = "needs an interactive desktop session"]
    fn the_taskbar_interface_can_actually_be_created() {
        assert!(
            super::available(),
            "CoCreateInstance(CLSID_TaskbarList, IID_ITaskbarList3) or HrInit failed — \
             check the GUIDs and the vtable prefix in this module"
        );
    }

    #[test]
    fn the_badge_is_an_opaque_dot_on_transparency() {
        let n = super::OVERLAY_SIZE;
        let px = super::overlay_pixels(n);
        assert_eq!(px.len(), (n * n) as usize);
        let at = |x: u32, y: u32| px[(y * n + x) as usize];
        assert_eq!(at(n / 2, n / 2) >> 24, 0xFF, "centre is opaque");
        assert_eq!(at(n / 2, n / 2) & 0x00FF_FFFF, 0x00E5_484D);
        assert_eq!(at(0, 0), 0, "corners are fully transparent");
        assert_eq!(at(n - 1, n - 1), 0);
        // Symmetric, so it cannot be drawn off-centre.
        for y in 0..n {
            for x in 0..n {
                assert_eq!(at(x, y), at(n - 1 - x, n - 1 - y));
            }
        }
    }

    #[test]
    fn only_determinate_states_report_a_value_to_carry_forward() {
        assert_eq!(Progress::Normal(30).value(), Some(30));
        assert_eq!(Progress::Error(30).value(), Some(30));
        assert_eq!(Progress::Paused(30).value(), Some(30));
        assert_eq!(Progress::None.value(), None);
        assert_eq!(Progress::Indeterminate.value(), None);
    }
}
