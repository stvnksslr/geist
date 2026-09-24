//! Native window chrome that egui/winit don't expose: DWM titlebar colors
//! (`window-titlebar-background` / `-foreground`), whole-cell resize steps
//! (`window-step-resize`) and the touch keyboard (`show_on_screen_keyboard`).
//!
//! Everything here degrades to a silent no-op — on Windows 10 (no caption
//! color attribute), on a machine without the touch keyboard, off Windows.
//!
//! **Why titlebar colors go to every window on the thread.** Only the root
//! viewport's `HWND` is reachable through eframe; a child viewport's is not.
//! But the colors are app-global config, and every geist window is a top-level
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

// --- Client-drawn caption (`macos-titlebar-style = tabs | hidden`) ---------
//
// The window keeps its native frame (`WS_CAPTION | WS_THICKFRAME`, so Aero
// snap, the drop shadow, Win11 rounded corners and the side/bottom resize
// borders all stay native), but a `WM_NCCALCSIZE` subclass hands the caption
// band to the client area — the Windows Terminal technique. geist then draws
// the tab strip at the very top, and `WM_NCHITTEST` answers for the pieces of
// that strip Windows must still own:
//
// - the top resize band (the caption used to carry it),
// - the three caption buttons, as `HTMINBUTTON` / `HTMAXBUTTON` / `HTCLOSE` —
//   `HTMAXBUTTON` is what makes the Win11 snap-layouts flyout appear on hover,
//   and it is only ever offered to a real non-client hit code.
//
// Everything else stays `HTCLIENT`, so egui keeps the tabs. Dragging from empty
// strip space and double-click-to-maximize are done by the app through
// `ViewportCommand::StartDrag` / `Maximized`, which work for *every* viewport —
// child windows have no reachable `HWND`, so the native side is written to
// need nothing per window: the caption geometry is app-global (one font, one
// strip height) and the subclass is installed on every geist top-level window
// on the UI thread, found with `EnumThreadWindows`, as the titlebar colours are.
//
// Because the buttons are non-client, the client never sees the pointer over
// them: hover and press state live here, keyed by the pointer's *screen*
// position, and each window asks "is it over my button?" with its own screen
// rect ([`caption_hover`]).

/// Which caption geist draws.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CaptionStyle {
    /// The system caption (default).
    Native,
    /// Client-drawn: tab strip in the titlebar plus caption buttons.
    Tabs,
    /// Client-drawn frame without caption buttons.
    Hidden,
}

/// A caption hit-test result, in `WM_NCHITTEST` terms.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CaptionHit {
    Client,
    Top,
    TopLeft,
    TopRight,
    Min,
    Max,
    Close,
}

impl CaptionHit {
    /// The `HT*` code Windows expects.
    pub fn code(self) -> isize {
        match self {
            CaptionHit::Client => 1,
            CaptionHit::Min => 8,
            CaptionHit::Max => 9,
            CaptionHit::Top => 12,
            CaptionHit::TopLeft => 13,
            CaptionHit::TopRight => 14,
            CaptionHit::Close => 20,
        }
    }

    pub fn from_code(code: usize) -> Option<CaptionHit> {
        Some(match code {
            8 => CaptionHit::Min,
            9 => CaptionHit::Max,
            20 => CaptionHit::Close,
            _ => return None,
        })
    }
}

/// Caption-button width in DIPs (= egui points: geist never zooms egui) —
/// Windows 11's own caption buttons are 46 wide.
pub const CAPTION_BUTTON_W: f32 = 46.0;

/// The caption geometry for one window, all in the same unit (physical pixels
/// on the native side, points in the app).
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct CaptionMetrics {
    /// Height of the strip the buttons fill; 0 = no strip shown.
    pub strip_h: f32,
    pub button_w: f32,
    /// Thickness of the top resize band (the system's frame thickness).
    pub frame: f32,
    /// Whether caption buttons are drawn (`tabs`) or not (`hidden`).
    pub buttons: bool,
}

/// The min / max / close rects `[x0, y0, x1, y1]`, right-aligned in a client
/// `client_w` wide. Empty when the style has no buttons or no strip.
pub fn caption_buttons(client_w: f32, m: CaptionMetrics) -> Vec<(CaptionHit, [f32; 4])> {
    if !m.buttons || m.strip_h <= 0.0 {
        return Vec::new();
    }
    [CaptionHit::Min, CaptionHit::Max, CaptionHit::Close]
        .iter()
        .enumerate()
        .map(|(i, &hit)| {
            let x1 = client_w - (2 - i) as f32 * m.button_w;
            (hit, [x1 - m.button_w, 0.0, x1, m.strip_h])
        })
        .collect()
}

/// Hit-test a client-area point `(x, y)` against the client-drawn caption.
///
/// The top resize band wins over the buttons (as with the native caption,
/// whose top few pixels resize), and it is absent when maximized — a maximized
/// window has no border to drag, and its top edge sits at the screen edge where
/// the band would steal clicks from the tabs and buttons.
pub fn caption_hit(
    x: f32,
    y: f32,
    client_w: f32,
    m: CaptionMetrics,
    maximized: bool,
) -> CaptionHit {
    if !maximized && y >= 0.0 && y < m.frame {
        // Corners get the diagonal cursor over a frame-sized span, like the
        // side borders' own corner zones.
        return if x < m.frame * 2.0 {
            CaptionHit::TopLeft
        } else if x >= client_w - m.frame * 2.0 {
            CaptionHit::TopRight
        } else {
            CaptionHit::Top
        };
    }
    for (hit, [x0, y0, x1, y1]) in caption_buttons(client_w, m) {
        if x >= x0 && x < x1 && y >= y0 && y < y1 {
            return hit;
        }
    }
    CaptionHit::Client
}

/// The client rect (`[l, t, r, b]`) for a window whose default
/// `WM_NCCALCSIZE` answer is `after` and whose proposed window rect top was
/// `top`: keep the side and bottom borders, drop the caption. A maximized
/// window extends `frame` past the monitor on every side, so its top moves
/// down by that much or the strip's top row would be off-screen.
pub fn nc_client_rect(after: [i32; 4], top: i32, frame: i32, maximized: bool) -> [i32; 4] {
    let t = if maximized { top + frame } else { top };
    [after[0], t, after[2], after[3]]
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
        fn GetWindowLongPtrW(hwnd: HWND, index: i32) -> isize;
        fn SetWindowLongPtrW(hwnd: HWND, index: i32, new: isize) -> isize;
        fn ShowWindow(hwnd: HWND, cmd: i32) -> i32;
        fn IsWindowVisible(hwnd: HWND) -> i32;
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

    /// `macos-hidden` and `macos-window-shadow`, for every window on this
    /// thread.
    ///
    /// Taskbar/Alt-Tab presence is `WS_EX_TOOLWINDOW`, which the shell only
    /// re-reads when the window is hidden, so a visible window is cycled.
    /// The shadow is DWM's non-client rendering: disabling it drops the shadow
    /// (and, for a client-drawn caption, nothing else — the frame is ours).
    pub fn sync_window_flags(hidden_from_taskbar: bool, shadow: bool) {
        /// `DWMWA_NCRENDERING_POLICY`, with `DWMNCRP_ENABLED` / `DWMNCRP_DISABLED`.
        const DWMWA_NCRENDERING_POLICY: u32 = 2;
        const NCRP_ENABLED: u32 = 2;
        const NCRP_DISABLED: u32 = 1;
        const GWL_EXSTYLE: i32 = -20;
        const WS_EX_TOOLWINDOW: isize = 0x0000_0080;
        const SW_HIDE: i32 = 0;
        const SW_SHOWNA: i32 = 8;

        unsafe extern "system" fn each(hwnd: HWND, lp: isize) -> i32 {
            // SAFETY: `lp` is the `&(bool, bool)` passed below, alive for the call.
            let (hidden, shadow) = unsafe { *(lp as *const (bool, bool)) };
            set_attr(
                hwnd,
                DWMWA_NCRENDERING_POLICY,
                if shadow { NCRP_ENABLED } else { NCRP_DISABLED },
            );
            // SAFETY: a live HWND owned by this thread.
            unsafe {
                let ex = GetWindowLongPtrW(hwnd, GWL_EXSTYLE);
                let want = if hidden {
                    ex | WS_EX_TOOLWINDOW
                } else {
                    ex & !WS_EX_TOOLWINDOW
                };
                if want != ex {
                    let visible = IsWindowVisible(hwnd) != 0;
                    if visible {
                        ShowWindow(hwnd, SW_HIDE);
                    }
                    SetWindowLongPtrW(hwnd, GWL_EXSTYLE, want);
                    if visible {
                        ShowWindow(hwnd, SW_SHOWNA);
                    }
                }
            }
            1
        }
        let args = (hidden_from_taskbar, shadow);
        // SAFETY: the callback only reads `args`, which outlives the call.
        unsafe {
            EnumThreadWindows(GetCurrentThreadId(), each, &args as *const _ as isize);
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
        query_interface:
            unsafe extern "system" fn(*mut c_void, *const Guid, *mut *mut c_void) -> i32,
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

/// App-global caption state shared by the subclass and the app.
mod caption_state {
    use super::{CaptionHit, CaptionStyle};
    use std::sync::Mutex;
    use std::sync::atomic::{AtomicU8, AtomicU32, Ordering};

    static STYLE: AtomicU8 = AtomicU8::new(0);
    /// Strip height in DIPs, as `f32` bits.
    static STRIP_H: AtomicU32 = AtomicU32::new(0);
    /// The button under the pointer and the pointer's screen position.
    pub static HOVER: Mutex<Option<(CaptionHit, (i32, i32))>> = Mutex::new(None);
    pub static PRESSED: Mutex<Option<CaptionHit>> = Mutex::new(None);
    pub static WAKER: std::sync::OnceLock<eframe::egui::Context> = std::sync::OnceLock::new();

    pub fn style() -> CaptionStyle {
        match STYLE.load(Ordering::Relaxed) {
            1 => CaptionStyle::Tabs,
            2 => CaptionStyle::Hidden,
            _ => CaptionStyle::Native,
        }
    }

    /// Returns whether it changed.
    pub fn set_style(s: CaptionStyle) -> bool {
        let v = match s {
            CaptionStyle::Native => 0,
            CaptionStyle::Tabs => 1,
            CaptionStyle::Hidden => 2,
        };
        STYLE.swap(v, Ordering::Relaxed) != v
    }

    pub fn strip_h() -> f32 {
        f32::from_bits(STRIP_H.load(Ordering::Relaxed))
    }

    pub fn set_strip_h(h: f32) {
        STRIP_H.store(h.max(0.0).to_bits(), Ordering::Relaxed);
    }

    pub fn wake() {
        if let Some(ctx) = WAKER.get() {
            ctx.request_repaint_of(eframe::egui::ViewportId::ROOT);
        }
    }
}

/// The configured caption style.
pub fn caption_style() -> CaptionStyle {
    caption_state::style()
}

/// Set the caption style. Takes effect on the next [`sync_caption`], which
/// re-frames every window (so a config reload switches live).
pub fn set_caption_style(s: CaptionStyle) {
    if caption_state::set_style(s) {
        #[cfg(windows)]
        caption_imp::restyle_all();
    }
}

/// Publish the strip height (points) the caption buttons fill.
pub fn set_caption_height(h: f32) {
    caption_state::set_strip_h(h);
}

/// Give the subclass a way to repaint on hover/press changes.
pub fn set_caption_waker(ctx: &eframe::egui::Context) {
    let _ = caption_state::WAKER.set(ctx.clone());
}

/// The caption button under the pointer *if* it lies inside `screen_rect_px`
/// (a window's client rect in screen pixels, `[x0, y0, x1, y1]`), and whether
/// it is held down.
pub fn caption_hover(screen_rect_px: [f32; 4]) -> Option<(CaptionHit, bool)> {
    let (hit, (x, y)) = (*caption_state::HOVER.lock().ok()?)?;
    let (x, y) = (x as f32, y as f32);
    let [x0, y0, x1, y1] = screen_rect_px;
    if x < x0 || x >= x1 || y < y0 || y >= y1 {
        return None;
    }
    let pressed = caption_state::PRESSED.lock().ok().and_then(|p| *p) == Some(hit);
    Some((hit, pressed))
}

/// Install the caption subclass on any geist window that lacks it. Cheap
/// enough to run every pass (one `EnumThreadWindows`); new child windows are
/// picked up the pass after they appear.
pub fn sync_caption() {
    #[cfg(windows)]
    caption_imp::sync();
}

#[cfg(windows)]
mod caption_imp {
    use std::collections::HashSet;
    use std::sync::Mutex;

    use windows_sys::Win32::Foundation::{HWND, POINT, RECT};

    use super::caption_state::{self, HOVER, PRESSED};
    use super::{CaptionHit, CaptionMetrics, CaptionStyle, caption_hit, nc_client_rect};

    const WM_NCCALCSIZE: u32 = 0x0083;
    const WM_NCHITTEST: u32 = 0x0084;
    const WM_NCMOUSEMOVE: u32 = 0x00A0;
    const WM_NCLBUTTONDOWN: u32 = 0x00A1;
    const WM_NCLBUTTONUP: u32 = 0x00A2;
    const WM_NCLBUTTONDBLCLK: u32 = 0x00A3;
    const WM_NCMOUSELEAVE: u32 = 0x02A2;
    const WM_MOUSEMOVE: u32 = 0x0200;
    const WM_SYSCOMMAND: u32 = 0x0112;
    const WM_NCDESTROY: u32 = 0x0082;
    const SC_MINIMIZE: usize = 0xF020;
    const SC_MAXIMIZE: usize = 0xF030;
    const SC_RESTORE: usize = 0xF120;
    const SC_CLOSE: usize = 0xF060;
    const HTCLIENT: isize = 1;
    const SM_CYFRAME: i32 = 33;
    const SM_CXPADDEDBORDER: i32 = 92;
    const GWL_STYLE: i32 = -16;
    const WS_CAPTION: isize = 0x00C0_0000;
    const WS_THICKFRAME: isize = 0x0004_0000;
    const WS_CHILD: isize = 0x4000_0000;
    const SWP_FLAGS: u32 = 0x0001 | 0x0002 | 0x0004 | 0x0010 | 0x0020; // NOSIZE|NOMOVE|NOZORDER|NOACTIVATE|FRAMECHANGED
    const TME_LEAVE: u32 = 0x2;
    const TME_NONCLIENT: u32 = 0x10;
    const SUBCLASS_ID: usize = 0x6769_6563; // "giec"

    #[repr(C)]
    struct NcCalcSizeParams {
        rgrc: [RECT; 3],
        lppos: *mut core::ffi::c_void,
    }

    #[repr(C)]
    struct TrackMouseEventS {
        cb: u32,
        flags: u32,
        hwnd: HWND,
        hover_time: u32,
    }

    type SubclassProc = unsafe extern "system" fn(HWND, u32, usize, isize, usize, usize) -> isize;

    #[link(name = "user32")]
    unsafe extern "system" {
        fn EnumThreadWindows(
            thread: u32,
            f: unsafe extern "system" fn(HWND, isize) -> i32,
            lparam: isize,
        ) -> i32;
        fn GetClientRect(hwnd: HWND, rect: *mut RECT) -> i32;
        fn ScreenToClient(hwnd: HWND, pt: *mut POINT) -> i32;
        fn IsZoomed(hwnd: HWND) -> i32;
        fn GetDpiForWindow(hwnd: HWND) -> u32;
        fn GetSystemMetricsForDpi(index: i32, dpi: u32) -> i32;
        fn GetWindowLongPtrW(hwnd: HWND, index: i32) -> isize;
        fn GetClassNameW(hwnd: HWND, buf: *mut u16, len: i32) -> i32;
        fn SetWindowPos(
            hwnd: HWND,
            after: HWND,
            x: i32,
            y: i32,
            cx: i32,
            cy: i32,
            flags: u32,
        ) -> i32;
        fn PostMessageW(hwnd: HWND, msg: u32, wp: usize, lp: isize) -> i32;
        fn TrackMouseEvent(tme: *mut TrackMouseEventS) -> i32;
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

    /// HWNDs carrying the subclass.
    static INSTALLED: Mutex<Option<HashSet<isize>>> = Mutex::new(None);

    /// Whether this window should get the client-drawn caption *now*: the
    /// style asks for it and the window still has a caption (not
    /// `window-decoration = none`, not the undecorated quick terminal, not
    /// borderless fullscreen).
    fn active(hwnd: HWND) -> bool {
        if caption_state::style() == CaptionStyle::Native {
            return false;
        }
        // SAFETY: reading a style word of a window owned by this thread.
        let style = unsafe { GetWindowLongPtrW(hwnd, GWL_STYLE) };
        style & WS_CAPTION == WS_CAPTION && style & WS_THICKFRAME != 0
    }

    fn frame_px(hwnd: HWND) -> i32 {
        // SAFETY: plain metric queries.
        unsafe {
            let dpi = GetDpiForWindow(hwnd).max(96);
            GetSystemMetricsForDpi(SM_CYFRAME, dpi) + GetSystemMetricsForDpi(SM_CXPADDEDBORDER, dpi)
        }
    }

    fn metrics(hwnd: HWND) -> CaptionMetrics {
        // SAFETY: plain query.
        let scale = unsafe { GetDpiForWindow(hwnd).max(96) } as f32 / 96.0;
        CaptionMetrics {
            strip_h: caption_state::strip_h() * scale,
            button_w: super::CAPTION_BUTTON_W * scale,
            frame: frame_px(hwnd) as f32,
            buttons: caption_state::style() == CaptionStyle::Tabs,
        }
    }

    fn set_hover(v: Option<(CaptionHit, (i32, i32))>) {
        if let Ok(mut h) = HOVER.lock() {
            let changed = h.map(|x| x.0) != v.map(|x| x.0);
            *h = v;
            if changed {
                caption_state::wake();
            }
        }
    }

    fn set_pressed(v: Option<CaptionHit>) {
        if let Ok(mut p) = PRESSED.lock()
            && *p != v
        {
            *p = v;
            caption_state::wake();
        }
    }

    fn lparam_point(lp: isize) -> (i32, i32) {
        (
            (lp & 0xFFFF) as i16 as i32,
            ((lp >> 16) & 0xFFFF) as i16 as i32,
        )
    }

    unsafe extern "system" fn subclass(
        hwnd: HWND,
        msg: u32,
        wp: usize,
        lp: isize,
        _id: usize,
        _data: usize,
    ) -> isize {
        // SAFETY (whole body): every pointer dereferenced is one Windows hands
        // this message, and every call targets `hwnd`, a live window of this
        // thread.
        unsafe {
            if msg == WM_NCDESTROY {
                if let Ok(mut s) = INSTALLED.lock()
                    && let Some(s) = s.as_mut()
                {
                    s.remove(&(hwnd as isize));
                }
                RemoveWindowSubclass(hwnd, subclass, SUBCLASS_ID);
                return DefSubclassProc(hwnd, msg, wp, lp);
            }
            if !active(hwnd) {
                return DefSubclassProc(hwnd, msg, wp, lp);
            }
            match msg {
                WM_NCCALCSIZE if wp != 0 && lp != 0 => {
                    let params = &mut *(lp as *mut NcCalcSizeParams);
                    let top = params.rgrc[0].top;
                    let r = DefSubclassProc(hwnd, msg, wp, lp);
                    let a = params.rgrc[0];
                    let [l, t, rr, b] = nc_client_rect(
                        [a.left, a.top, a.right, a.bottom],
                        top,
                        frame_px(hwnd),
                        IsZoomed(hwnd) != 0,
                    );
                    params.rgrc[0] = RECT {
                        left: l,
                        top: t,
                        right: rr,
                        bottom: b,
                    };
                    r
                }
                WM_NCHITTEST => {
                    let def = DefSubclassProc(hwnd, msg, wp, lp);
                    if def != HTCLIENT {
                        return def;
                    }
                    let (sx, sy) = lparam_point(lp);
                    let mut pt = POINT { x: sx, y: sy };
                    let mut cr = RECT::default();
                    if ScreenToClient(hwnd, &mut pt) == 0 || GetClientRect(hwnd, &mut cr) == 0 {
                        return def;
                    }
                    caption_hit(
                        pt.x as f32,
                        pt.y as f32,
                        (cr.right - cr.left) as f32,
                        metrics(hwnd),
                        IsZoomed(hwnd) != 0,
                    )
                    .code()
                }
                WM_NCMOUSEMOVE => {
                    match CaptionHit::from_code(wp) {
                        Some(hit) => {
                            set_hover(Some((hit, lparam_point(lp))));
                            let mut tme = TrackMouseEventS {
                                cb: size_of::<TrackMouseEventS>() as u32,
                                flags: TME_LEAVE | TME_NONCLIENT,
                                hwnd,
                                hover_time: 0,
                            };
                            TrackMouseEvent(&mut tme);
                        }
                        None => set_hover(None),
                    }
                    DefSubclassProc(hwnd, msg, wp, lp)
                }
                WM_NCMOUSELEAVE => {
                    set_hover(None);
                    set_pressed(None);
                    DefSubclassProc(hwnd, msg, wp, lp)
                }
                WM_MOUSEMOVE => {
                    if HOVER.lock().is_ok_and(|h| h.is_some()) {
                        set_hover(None);
                    }
                    DefSubclassProc(hwnd, msg, wp, lp)
                }
                // Swallowed: `DefWindowProc` would draw a *classic* caption
                // button over our strip, and run the action on press rather
                // than on release.
                WM_NCLBUTTONDOWN | WM_NCLBUTTONDBLCLK if CaptionHit::from_code(wp).is_some() => {
                    set_pressed(CaptionHit::from_code(wp));
                    0
                }
                WM_NCLBUTTONUP if CaptionHit::from_code(wp).is_some() => {
                    let hit = CaptionHit::from_code(wp);
                    let was = PRESSED.lock().ok().and_then(|p| *p);
                    set_pressed(None);
                    if hit == was {
                        let cmd = match hit {
                            Some(CaptionHit::Min) => SC_MINIMIZE,
                            Some(CaptionHit::Max) if IsZoomed(hwnd) != 0 => SC_RESTORE,
                            Some(CaptionHit::Max) => SC_MAXIMIZE,
                            // Through WM_SYSCOMMAND -> WM_CLOSE, so winit reports
                            // `close_requested` and the confirm-close flow runs.
                            _ => SC_CLOSE,
                        };
                        PostMessageW(hwnd, WM_SYSCOMMAND, cmd, 0);
                    }
                    0
                }
                _ => DefSubclassProc(hwnd, msg, wp, lp),
            }
        }
    }

    fn is_geist_window(hwnd: HWND) -> bool {
        let mut buf = [0u16; 32];
        // SAFETY: valid buffer and length.
        let n = unsafe { GetClassNameW(hwnd, buf.as_mut_ptr(), buf.len() as i32) };
        // winit's class; egui-winit sets no custom one on Windows.
        let ok = n > 0 && String::from_utf16_lossy(&buf[..n as usize]) == "Window Class";
        // SAFETY: reading a style word.
        ok && unsafe { GetWindowLongPtrW(hwnd, GWL_STYLE) } & WS_CHILD == 0
    }

    fn reframe(hwnd: HWND) {
        // SAFETY: re-run WM_NCCALCSIZE on a window of this thread.
        unsafe {
            SetWindowPos(hwnd, std::ptr::null_mut(), 0, 0, 0, 0, SWP_FLAGS);
        }
    }

    pub fn sync() {
        unsafe extern "system" fn each(hwnd: HWND, _lp: isize) -> i32 {
            if !is_geist_window(hwnd) {
                return 1;
            }
            let new = INSTALLED
                .lock()
                .map(|mut s| s.get_or_insert_with(HashSet::new).insert(hwnd as isize))
                .unwrap_or(false);
            if new {
                // SAFETY: `hwnd` belongs to this (the UI) thread.
                unsafe {
                    SetWindowSubclass(hwnd, subclass, SUBCLASS_ID, 0);
                }
                if active(hwnd) {
                    reframe(hwnd);
                }
            }
            1
        }
        if caption_state::style() == CaptionStyle::Native
            && INSTALLED
                .lock()
                .is_ok_and(|s| s.as_ref().is_none_or(|s| s.is_empty()))
        {
            return; // never used: never subclass anything
        }
        // SAFETY: the callback touches only its own window.
        unsafe {
            EnumThreadWindows(GetCurrentThreadId(), each, 0);
        }
    }

    /// The style changed: re-run `WM_NCCALCSIZE` on every subclassed window.
    pub fn restyle_all() {
        let hwnds: Vec<isize> = INSTALLED
            .lock()
            .ok()
            .and_then(|s| s.as_ref().map(|s| s.iter().copied().collect()))
            .unwrap_or_default();
        for h in hwnds {
            reframe(h as HWND);
        }
    }
}

#[cfg(windows)]
pub use imp::{apply_titlebar_colors, set_step_resize, show_on_screen_keyboard, sync_window_flags};

#[cfg(not(windows))]
pub fn apply_titlebar_colors(_bg: Option<Rgb>, _fg: Option<Rgb>) {}
#[cfg(not(windows))]
pub fn set_step_resize(_hwnd: isize, _g: Option<StepGeometry>) {}
#[cfg(not(windows))]
pub fn show_on_screen_keyboard() {}
#[cfg(not(windows))]
pub fn sync_window_flags(_hidden_from_taskbar: bool, _shadow: bool) {}

#[cfg(test)]
mod tests {
    use super::*;

    const G: StepGeometry = StepGeometry {
        cell_w: 10.0,
        cell_h: 20.0,
        extra_w: 4.0,
        extra_h: 30.0,
    };

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

    /// 150% scaling: 48px strip, 69px buttons, 12px frame, 1200px client.
    const M: CaptionMetrics = CaptionMetrics {
        strip_h: 48.0,
        button_w: 69.0,
        frame: 12.0,
        buttons: true,
    };

    #[test]
    fn caption_buttons_are_right_aligned_min_max_close() {
        let b = caption_buttons(1200.0, M);
        assert_eq!(
            b,
            vec![
                (CaptionHit::Min, [993.0, 0.0, 1062.0, 48.0]),
                (CaptionHit::Max, [1062.0, 0.0, 1131.0, 48.0]),
                (CaptionHit::Close, [1131.0, 0.0, 1200.0, 48.0]),
            ]
        );
        assert!(
            caption_buttons(
                1200.0,
                CaptionMetrics {
                    buttons: false,
                    ..M
                }
            )
            .is_empty()
        );
        assert!(caption_buttons(1200.0, CaptionMetrics { strip_h: 0.0, ..M }).is_empty());
    }

    #[test]
    fn hit_test_finds_each_button_and_leaves_the_rest_to_the_client() {
        let hit = |x, y| caption_hit(x, y, 1200.0, M, false);
        assert_eq!(hit(1000.0, 30.0), CaptionHit::Min);
        // The snap-layouts flyout keys on exactly this answer.
        assert_eq!(hit(1100.0, 30.0), CaptionHit::Max);
        assert_eq!(hit(1199.0, 47.0), CaptionHit::Close);
        // Tabs, empty strip and the terminal below are the client's.
        assert_eq!(hit(500.0, 30.0), CaptionHit::Client);
        assert_eq!(hit(992.0, 30.0), CaptionHit::Client);
        assert_eq!(hit(1100.0, 48.0), CaptionHit::Client);
        assert_eq!(hit(1100.0, 400.0), CaptionHit::Client);
    }

    #[test]
    fn the_top_band_resizes_unless_maximized() {
        let hit = |x, y, max| caption_hit(x, y, 1200.0, M, max);
        assert_eq!(hit(500.0, 0.0, false), CaptionHit::Top);
        assert_eq!(hit(500.0, 11.0, false), CaptionHit::Top);
        assert_eq!(hit(500.0, 12.0, false), CaptionHit::Client);
        assert_eq!(hit(5.0, 3.0, false), CaptionHit::TopLeft);
        // Over the close button the band still wins, as on a native caption.
        assert_eq!(hit(1195.0, 3.0, false), CaptionHit::TopRight);
        assert_eq!(hit(1100.0, 3.0, false), CaptionHit::Top);
        // Maximized: no border to drag; the buttons reach the screen edge
        // (Fitts's law — a flick to the top-right corner closes).
        assert_eq!(hit(500.0, 0.0, true), CaptionHit::Client);
        assert_eq!(hit(1199.0, 0.0, true), CaptionHit::Close);
        assert_eq!(hit(1100.0, 0.0, true), CaptionHit::Max);
    }

    #[test]
    fn hidden_style_has_no_buttons_but_keeps_the_band() {
        let h = CaptionMetrics {
            buttons: false,
            ..M
        };
        assert_eq!(
            caption_hit(1100.0, 30.0, 1200.0, h, false),
            CaptionHit::Client
        );
        assert_eq!(caption_hit(1100.0, 3.0, 1200.0, h, false), CaptionHit::Top);
    }

    #[test]
    fn hit_codes_round_trip_for_the_buttons_only() {
        for h in [CaptionHit::Min, CaptionHit::Max, CaptionHit::Close] {
            assert_eq!(CaptionHit::from_code(h.code() as usize), Some(h));
        }
        assert_eq!(CaptionHit::Client.code(), 1);
        assert_eq!(CaptionHit::Top.code(), 12);
        assert_eq!(CaptionHit::from_code(2), None, "HTCAPTION is never ours");
    }

    #[test]
    fn nc_calc_keeps_sides_and_bottom_and_drops_the_caption() {
        // Default answer: 8px borders, 31px caption below top = 100.
        let after = [108, 131, 1092, 892];
        assert_eq!(nc_client_rect(after, 100, 8, false), [108, 100, 1092, 892]);
        // Maximized: the window hangs `frame` past the monitor top.
        assert_eq!(
            nc_client_rect([0, 23, 1920, 1032], -8, 8, true),
            [0, 0, 1920, 1032]
        );
    }

    #[test]
    fn colorref_is_bgr() {
        assert_eq!(colorref(Rgb::new(0x11, 0x22, 0x33)), 0x0033_2211);
    }
}
