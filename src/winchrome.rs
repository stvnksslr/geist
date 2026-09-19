//! Native window chrome that egui/winit don't expose: DWM titlebar colors
//! (`window-titlebar-background` / `-foreground`), whole-cell resize steps
//! (`window-step-resize`) and the touch keyboard (`show_on_screen_keyboard`).
//!
//! Everything here degrades to a silent no-op — on Windows 10 (no caption
//! color attribute), on a machine without the touch keyboard, off Windows.
//!
//! **Why titlebar colors go to every window on the thread.** Only the root
//! viewport's `HWND` is reachable through eframe; a child viewport's is not.
//! But the colors are app-global config, and every giest window is a top-level
//! window owned by the UI thread — so `EnumThreadWindows` reaches all of them
//! without needing a handle per window.

use crate::engine::Rgb;

/// The whole-cell geometry `window-step-resize` snaps to, all in physical
/// pixels. `extra_*` is everything in the client area that is *not* grid
/// cells: the tab strip, the padding, and the sub-cell remainder the layout
/// leaves. Recomputed every frame by the app.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct StepGeometry {
    pub cell_w: f32,
    pub cell_h: f32,
    pub extra_w: f32,
    pub extra_h: f32,
}

// `WMSZ_*`: which edge of the window a `WM_SIZING` drag is moving.
const WMSZ_LEFT: u32 = 1;
const WMSZ_TOP: u32 = 3;
const WMSZ_TOPLEFT: u32 = 4;
const WMSZ_TOPRIGHT: u32 = 5;
const WMSZ_BOTTOMLEFT: u32 = 7;

/// Snap a proposed window rect `(left, top, right, bottom)` so its client
/// area holds a whole number of cells, moving only the edge(s) being dragged.
///
/// `nc` is the non-client size (border + caption) in pixels. Floors rather
/// than rounds — the window never shows a partial column, which is the xterm
/// and macOS `contentResizeIncrements` behaviour — but never below one cell.
pub fn snap_sizing(edge: u32, rect: [i32; 4], nc: (i32, i32), g: StepGeometry) -> [i32; 4] {
    if g.cell_w < 1.0 || g.cell_h < 1.0 {
        return rect;
    }
    let [mut l, mut t, mut r, mut b] = rect;
    let snap = |client: i32, extra: f32, cell: f32| -> i32 {
        let cells = ((client as f32 - extra) / cell).floor().max(1.0);
        (extra + cells * cell).round() as i32
    };
    let cw = (r - l) - nc.0;
    let ch = (b - t) - nc.1;
    let dw = snap(cw, g.extra_w, g.cell_w) - cw;
    let dh = snap(ch, g.extra_h, g.cell_h) - ch;
    if matches!(edge, WMSZ_LEFT | WMSZ_TOPLEFT | WMSZ_BOTTOMLEFT) {
        l -= dw;
    } else {
        r += dw;
    }
    if matches!(edge, WMSZ_TOP | WMSZ_TOPLEFT | WMSZ_TOPRIGHT) {
        t -= dh;
    } else {
        b += dh;
    }
    [l, t, r, b]
}

/// DWM wants a `COLORREF`: `0x00BBGGRR`.
pub fn colorref(c: Rgb) -> u32 {
    u32::from(c.r) | (u32::from(c.g) << 8) | (u32::from(c.b) << 16)
}

#[cfg(windows)]
mod imp {
    use std::collections::HashMap;
    use std::ffi::c_void;
    use std::sync::Mutex;

    use windows_sys::Win32::Foundation::{HWND, RECT};
    use windows_sys::Win32::Graphics::Dwm::DwmSetWindowAttribute;

    use super::{StepGeometry, colorref, snap_sizing};
    use crate::engine::Rgb;

    /// `DWMWA_CAPTION_COLOR` / `DWMWA_TEXT_COLOR` (Windows 11 build 22000+).
    const DWMWA_CAPTION_COLOR: u32 = 35;
    const DWMWA_TEXT_COLOR: u32 = 36;
    /// Hands the attribute back to the system theme.
    const DWMWA_COLOR_DEFAULT: u32 = 0xFFFF_FFFF;
    const WM_SIZING: u32 = 0x0214;
    const WM_NCDESTROY: u32 = 0x0082;
    const SUBCLASS_ID: usize = 0x6769_6573; // "gies"

    type SubclassProc = unsafe extern "system" fn(HWND, u32, usize, isize, usize, usize) -> isize;

    // user32 is already linked by winit; comctl32 ships with every Windows.
    #[link(name = "user32")]
    unsafe extern "system" {
        fn EnumThreadWindows(
            thread: u32,
            f: unsafe extern "system" fn(HWND, isize) -> i32,
            lparam: isize,
        ) -> i32;
        fn GetWindowRect(hwnd: HWND, rect: *mut RECT) -> i32;
        fn GetClientRect(hwnd: HWND, rect: *mut RECT) -> i32;
    }
    #[link(name = "kernel32")]
    unsafe extern "system" {
        fn GetCurrentThreadId() -> u32;
    }
    #[link(name = "comctl32")]
    unsafe extern "system" {
        fn SetWindowSubclass(hwnd: HWND, f: SubclassProc, id: usize, data: usize) -> i32;
        fn RemoveWindowSubclass(hwnd: HWND, f: SubclassProc, id: usize) -> i32;
        fn DefSubclassProc(hwnd: HWND, msg: u32, wp: usize, lp: isize) -> isize;
    }

    fn set_attr(hwnd: HWND, attr: u32, value: u32) {
        // SAFETY: a 4-byte COLORREF for a 4-byte attribute. Pre-Win11 DWM
        // rejects the attribute with E_INVALIDARG, which is the no-op we want.
        unsafe {
            DwmSetWindowAttribute(
                hwnd,
                attr,
                &value as *const u32 as *const c_void,
                size_of::<u32>() as u32,
            );
        }
    }

    pub fn apply_titlebar_colors(bg: Option<Rgb>, fg: Option<Rgb>) {
        unsafe extern "system" fn each(hwnd: HWND, lp: isize) -> i32 {
            // SAFETY: `lp` is the `&(u32, u32)` passed below, alive for the call.
            let (bg, fg) = unsafe { *(lp as *const (u32, u32)) };
            set_attr(hwnd, DWMWA_CAPTION_COLOR, bg);
            set_attr(hwnd, DWMWA_TEXT_COLOR, fg);
            1
        }
        let pair = (
            bg.map_or(DWMWA_COLOR_DEFAULT, colorref),
            fg.map_or(DWMWA_COLOR_DEFAULT, colorref),
        );
        // SAFETY: the callback only reads `pair`, which outlives the call.
        unsafe {
            EnumThreadWindows(GetCurrentThreadId(), each, &pair as *const _ as isize);
        }
    }

    /// Per-`HWND` step geometry; `None` means step resize is off for it.
    static STEP: Mutex<Option<HashMap<isize, StepGeometry>>> = Mutex::new(None);

    unsafe extern "system" fn subclass(
        hwnd: HWND,
        msg: u32,
        wp: usize,
        lp: isize,
        _id: usize,
        _data: usize,
    ) -> isize {
        if msg == WM_SIZING && lp != 0 {
            let g = STEP
                .lock()
                .ok()
                .and_then(|m| m.as_ref().and_then(|m| m.get(&(hwnd as isize)).copied()));
            if let Some(g) = g {
                let (mut wr, mut cr) = (RECT::default(), RECT::default());
                // SAFETY: valid out-pointers; `lp` is the RECT* WM_SIZING carries.
                unsafe {
                    if GetWindowRect(hwnd, &mut wr) != 0 && GetClientRect(hwnd, &mut cr) != 0 {
                        let nc = (
                            (wr.right - wr.left) - (cr.right - cr.left),
                            (wr.bottom - wr.top) - (cr.bottom - cr.top),
                        );
                        let r = &mut *(lp as *mut RECT);
                        let [l, t, rr, b] =
                            snap_sizing(wp as u32, [r.left, r.top, r.right, r.bottom], nc, g);
                        (r.left, r.top, r.right, r.bottom) = (l, t, rr, b);
                    }
                }
            }
        }
        if msg == WM_NCDESTROY {
            if let Ok(mut m) = STEP.lock()
                && let Some(m) = m.as_mut()
            {
                m.remove(&(hwnd as isize));
            }
            // SAFETY: removing our own subclass from the window being destroyed.
            unsafe {
                RemoveWindowSubclass(hwnd, subclass, SUBCLASS_ID);
            }
        }
        // SAFETY: forwarding the message unchanged down the subclass chain.
        unsafe { DefSubclassProc(hwnd, msg, wp, lp) }
    }

    pub fn set_step_resize(hwnd: isize, g: Option<StepGeometry>) {
        let Ok(mut m) = STEP.lock() else { return };
        let m = m.get_or_insert_with(HashMap::new);
        match g {
            Some(g) => {
                if m.insert(hwnd, g).is_none() {
                    // SAFETY: `hwnd` is a live window owned by this (the UI)
                    // thread, which SetWindowSubclass requires. Re-installing
                    // with the same id/proc just updates the ref data.
                    unsafe {
                        SetWindowSubclass(hwnd as HWND, subclass, SUBCLASS_ID, 0);
                    }
                }
            }
            None => {
                m.remove(&hwnd);
            }
        }
    }

    // --- Touch keyboard -------------------------------------------------

    #[repr(C)]
    struct Guid(u32, u16, u16, [u8; 8]);

    /// `CLSID_UIHostNoLaunch` — {4CE576FA-83DC-4F88-951C-9D0782B4E376}
    const CLSID_UIHOST_NO_LAUNCH: Guid = Guid(
        0x4CE5_76FA,
        0x83DC,
        0x4F88,
        [0x95, 0x1C, 0x9D, 0x07, 0x82, 0xB4, 0xE3, 0x76],
    );
    /// `IID_ITipInvocation` — {37C994E7-432B-4834-A2F7-DCE1F13B834B}
    const IID_ITIP_INVOCATION: Guid = Guid(
        0x37C9_94E7,
        0x432B,
        0x4834,
        [0xA2, 0xF7, 0xDC, 0xE1, 0xF1, 0x3B, 0x83, 0x4B],
    );
    const CLSCTX_LOCAL_SERVER: u32 = 0x4;
    const COINIT_APARTMENTTHREADED: u32 = 0x2;

    /// `ITipInvocation`: `IUnknown`, then its single method.
    #[repr(C)]
    struct TipVtbl {
        query_interface: unsafe extern "system" fn(*mut c_void, *const Guid, *mut *mut c_void) -> i32,
        add_ref: unsafe extern "system" fn(*mut c_void) -> u32,
        release: unsafe extern "system" fn(*mut c_void) -> u32,
        toggle: unsafe extern "system" fn(*mut c_void, HWND) -> i32,
    }

    #[link(name = "ole32")]
    unsafe extern "system" {
        fn CoInitializeEx(reserved: *mut c_void, coinit: u32) -> i32;
        fn CoCreateInstance(
            clsid: *const Guid,
            outer: *mut c_void,
            ctx: u32,
            iid: *const Guid,
            out: *mut *mut c_void,
        ) -> i32;
    }
    #[link(name = "user32")]
    unsafe extern "system" {
        fn GetDesktopWindow() -> HWND;
    }

    pub fn show_on_screen_keyboard() {
        // SAFETY: the documented calls with valid out-parameters; the
        // interface is released after its one use.
        unsafe {
            CoInitializeEx(std::ptr::null_mut(), COINIT_APARTMENTTHREADED);
            let mut p: *mut c_void = std::ptr::null_mut();
            let hr = CoCreateInstance(
                &CLSID_UIHOST_NO_LAUNCH,
                std::ptr::null_mut(),
                CLSCTX_LOCAL_SERVER,
                &IID_ITIP_INVOCATION,
                &mut p,
            );
            if hr >= 0 && !p.is_null() {
                let vtbl = *(p as *const *const TipVtbl);
                ((*vtbl).toggle)(p, GetDesktopWindow());
                ((*vtbl).release)(p);
                return;
            }
        }
        // The broker isn't running (common on a machine that has never shown
        // the keyboard): starting TabTip shows it. A missing binary is the
        // "no touch keyboard" case, and stays silent.
        let base = std::env::var_os("CommonProgramW6432")
            .or_else(|| std::env::var_os("CommonProgramFiles"))
            .map(std::path::PathBuf::from)
            .unwrap_or_else(|| r"C:\Program Files\Common Files".into());
        let exe = base.join(r"microsoft shared\ink\TabTip.exe");
        if exe.exists() {
            let _ = std::process::Command::new(exe).spawn();
        }
    }
}

#[cfg(windows)]
pub use imp::{apply_titlebar_colors, set_step_resize, show_on_screen_keyboard};

#[cfg(not(windows))]
pub fn apply_titlebar_colors(_bg: Option<Rgb>, _fg: Option<Rgb>) {}
#[cfg(not(windows))]
pub fn set_step_resize(_hwnd: isize, _g: Option<StepGeometry>) {}
#[cfg(not(windows))]
pub fn show_on_screen_keyboard() {}

#[cfg(test)]
mod tests {
    use super::*;

    const G: StepGeometry = StepGeometry { cell_w: 10.0, cell_h: 20.0, extra_w: 4.0, extra_h: 30.0 };

    #[test]
    fn snaps_the_dragged_edge_down_to_whole_cells() {
        // Client 4 + 10*n wide: 107 - 2 nc = 105 → 104 (10 cells).
        let r = snap_sizing(8, [0, 0, 107, 250], (2, 0), G);
        assert_eq!(r, [0, 0, 106, 250]);
        // Height: 250 → 30 + 11*20 = 250 already exact; 259 floors to 250.
        let r = snap_sizing(6, [0, 0, 106, 259], (2, 0), G);
        assert_eq!(r[3], 250);
    }

    #[test]
    fn a_left_or_top_drag_moves_the_left_or_top_edge() {
        let r = snap_sizing(4, [100, 100, 207, 359], (2, 0), G);
        assert_eq!(r[2], 207, "right edge stays put");
        assert_eq!(r[3], 359, "bottom edge stays put");
        assert_eq!(r[2] - r[0] - 2, 104);
        assert_eq!(r[3] - r[1], 250);
    }

    #[test]
    fn never_below_one_cell_and_inert_without_metrics() {
        let r = snap_sizing(8, [0, 0, 3, 3], (0, 0), G);
        assert_eq!((r[2], r[3]), (14, 50));
        let none = StepGeometry::default();
        assert_eq!(snap_sizing(8, [0, 0, 3, 3], (0, 0), none), [0, 0, 3, 3]);
    }

    #[test]
    fn colorref_is_bgr() {
        assert_eq!(colorref(Rgb::new(0x11, 0x22, 0x33)), 0x0033_2211);
    }
}
