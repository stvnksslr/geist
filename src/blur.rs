//! Windows backdrop blur behind a translucent window — geist's `background-blur`.
//!
//! Ghostty supports blur on macOS (a real Gaussian `CAFilter`) and on some Linux
//! compositors; Windows has no equivalent knob, but DWM offers two *fixed-strength*
//! system backdrops. We try them newest-first:
//!
//! 1. **Win11 22H2+** — the documented `DWMWA_SYSTEMBACKDROP_TYPE` attribute:
//!    acrylic (`DWMSBT_TRANSIENTWINDOW`) or mica (`DWMSBT_MAINWINDOW`).
//! 2. **Win10 / pre-22H2** — the *undocumented* `SetWindowCompositionAttribute`
//!    accent policy, resolved dynamically from `user32.dll` so a missing export
//!    degrades to "no blur" rather than failing to load the process.
//!
//! Neither path takes a radius, so Ghostty's integer intensity can only pick
//! *which* effect to use, not how strong it is — see [`Backdrop::for_blur`].
//!
//! The blur is only visible where the window is actually transparent, so it does
//! nothing unless `background-opacity` is below 1.0 (Ghostty documents the same
//! dependency) *and* the surface was created transparent — see `main.rs`.

use crate::config::BackgroundBlur;
use crate::engine::Rgb;

/// Which backdrop was applied. Returned so the caller can log what happened —
/// the whole feature degrades silently by design, and "nothing visible" is
/// otherwise indistinguishable from a bug.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Backdrop {
    /// Blur disabled by config.
    Off,
    /// Accent-policy acrylic, tinted with the configured background. Preferred,
    /// because it is the only path that accepts a tint at all.
    AccentAcrylic,
    /// Accent-policy plain blur, tinted with the configured background.
    AccentBlur,
    /// Win11 `DWMSBT_TRANSIENTWINDOW`. Untinted — the system material.
    SystemAcrylic,
    /// Win11 `DWMSBT_MAINWINDOW` (mica) — subtler, samples the desktop wallpaper.
    SystemMica,
    /// Neither path was available (or this isn't Windows).
    Unsupported,
}

impl Backdrop {
    /// Pick a backdrop for a blur intensity.
    ///
    /// **This is a two-bucket approximation.** DWM exposes no blur radius, so
    /// unlike macOS/KWin — where Ghostty's number is a true Gaussian sigma — all
    /// we can do is choose the subtler effect for low intensities and the
    /// stronger one otherwise. Ghostty's `true` is 20, which lands on acrylic.
    fn for_blur(blur: BackgroundBlur, system: bool) -> Self {
        match (blur.intensity(), system) {
            (0, _) => Self::Off,
            (1..=9, false) => Self::AccentBlur,
            (_, false) => Self::AccentAcrylic,
            (1..=9, true) => Self::SystemMica,
            (_, true) => Self::SystemAcrylic,
        }
    }
}

/// Extract the Win32 `HWND` from anything eframe hands us that carries a window
/// handle (`CreationContext` at startup, `Frame` every frame thereafter).
pub fn hwnd_of(handle: &impl raw_window_handle::HasWindowHandle) -> Option<isize> {
    match handle.window_handle().ok()?.as_raw() {
        raw_window_handle::RawWindowHandle::Win32(w) => Some(w.hwnd.get()),
        _ => None,
    }
}

#[cfg(windows)]
mod imp {
    use super::{Backdrop, Rgb};
    use crate::config::BackgroundBlur;
    use std::ffi::c_void;
    use std::sync::OnceLock;
    use windows_sys::Win32::Foundation::{FALSE, HWND};
    use windows_sys::Win32::Graphics::Dwm::{
        DWM_BB_ENABLE, DWM_BLURBEHIND, DWMSBT_MAINWINDOW, DWMSBT_NONE, DWMSBT_TRANSIENTWINDOW,
        DWMWA_SYSTEMBACKDROP_TYPE, DWMWA_USE_IMMERSIVE_DARK_MODE, DwmEnableBlurBehindWindow,
        DwmSetWindowAttribute,
    };
    use windows_sys::Win32::System::LibraryLoader::{GetProcAddress, LoadLibraryA};

    /// `WINDOWCOMPOSITIONATTRIB::WCA_ACCENT_POLICY`.
    const WCA_ACCENT_POLICY: u32 = 19;
    const ACCENT_DISABLED: u32 = 0;
    const ACCENT_ENABLE_BLURBEHIND: u32 = 3;
    const ACCENT_ENABLE_ACRYLICBLURBEHIND: u32 = 4;
    /// Draw all borders of the accent rectangle.
    const ACCENT_FLAGS_ALL_BORDERS: u32 = 2;

    #[repr(C)]
    struct AccentPolicy {
        accent_state: u32,
        accent_flags: u32,
        /// **ABGR** (`0xAABBGGRR`), not ARGB — the classic bug with this API.
        gradient_color: u32,
        animation_id: u32,
    }
    const _: () = assert!(size_of::<AccentPolicy>() == 16);

    #[repr(C)]
    struct WindowCompositionAttribData {
        attrib: u32,
        data: *mut c_void,
        size: usize,
    }

    type SetWindowCompositionAttribute =
        unsafe extern "system" fn(HWND, *mut WindowCompositionAttribData) -> i32;

    /// Resolve the undocumented `user32!SetWindowCompositionAttribute` once.
    /// Never linked directly — the export is undocumented and could vanish, and a
    /// missing import would stop the whole process from loading.
    fn set_window_composition_attribute() -> Option<SetWindowCompositionAttribute> {
        static F: OnceLock<Option<usize>> = OnceLock::new();
        // Parenthesized: `?` binds tighter than `*`, so without them it would be
        // applied to the `&Option<usize>` the OnceLock hands back.
        let addr = (*F.get_or_init(|| unsafe {
            let user32 = LoadLibraryA(c"user32.dll".as_ptr() as *const u8);
            if user32.is_null() {
                return None;
            }
            GetProcAddress(
                user32,
                c"SetWindowCompositionAttribute".as_ptr() as *const u8,
            )
            .map(|p| p as usize)
        }))?;
        // SAFETY: the address came from GetProcAddress for this exact export, so
        // transmuting it to that export's signature is the intended use.
        Some(unsafe { std::mem::transmute::<usize, SetWindowCompositionAttribute>(addr) })
    }

    /// Win11 22H2+ path. Returns false (E_INVALIDARG) on older builds, which is
    /// the feature test — no version sniffing needed.
    fn try_system_backdrop(hwnd: isize, kind: i32) -> bool {
        // SAFETY: a valid HWND, a documented attribute id, and a pointer to a
        // correctly-sized i32 for DWMWA_SYSTEMBACKDROP_TYPE.
        let hr = unsafe {
            DwmSetWindowAttribute(
                hwnd as HWND,
                DWMWA_SYSTEMBACKDROP_TYPE as u32,
                &kind as *const i32 as *const c_void,
                size_of::<i32>() as u32,
            )
        };
        hr >= 0
    }

    fn try_accent_policy(hwnd: isize, state: u32, tint: Rgb, opacity: f32) -> bool {
        let Some(f) = set_window_composition_attribute() else {
            return false;
        };
        let a = (opacity.clamp(0.0, 1.0) * 255.0).round() as u32;
        let mut policy = AccentPolicy {
            accent_state: state,
            accent_flags: ACCENT_FLAGS_ALL_BORDERS,
            // ABGR, so the tint's red byte goes in the low position.
            gradient_color: (a << 24)
                | ((tint.b as u32) << 16)
                | ((tint.g as u32) << 8)
                | tint.r as u32,
            animation_id: 0,
        };
        let mut data = WindowCompositionAttribData {
            attrib: WCA_ACCENT_POLICY,
            data: &mut policy as *mut AccentPolicy as *mut c_void,
            size: size_of::<AccentPolicy>(),
        };
        // SAFETY: `data` points to a live, correctly-sized AccentPolicy for the
        // duration of the call, and `size` is derived from the type rather than
        // hardcoded (a wrong size here corrupts memory inside user32).
        unsafe { f(hwnd as HWND, &mut data) != 0 }
    }

    /// Clear the blur-behind region winit installs at window creation.
    ///
    /// winit calls `DwmEnableBlurBehindWindow` with an empty region for every
    /// `with_transparent` window — the classic trick that marks the window as
    /// per-pixel alpha. **Only** drop it on the legacy accent-policy path, where
    /// the two mechanisms are documented to fight; clearing it unconditionally
    /// would remove the very thing that makes the window composite per-pixel.
    fn clear_blur_behind(hwnd: isize) {
        let bb = DWM_BLURBEHIND {
            dwFlags: DWM_BB_ENABLE,
            fEnable: FALSE,
            hRgnBlur: std::ptr::null_mut(),
            fTransitionOnMaximized: FALSE,
        };
        // SAFETY: a valid HWND and a fully-initialized DWM_BLURBEHIND.
        unsafe {
            DwmEnableBlurBehindWindow(hwnd as HWND, &bb);
        }
    }

    /// Tell DWM whether this is a dark-themed window.
    ///
    /// Win11's system backdrops have no tint control — they pick a light or dark
    /// acrylic/mica purely from this flag, and default to **light**. A dark
    /// terminal under a light acrylic reads as a washed-out grey haze rather than
    /// tinted glass, so drive it from the configured background's luminance
    /// instead of the system theme. Also darkens the title bar to match.
    fn set_dark_mode(hwnd: isize, dark: bool) {
        let flag: i32 = if dark { 1 } else { 0 };
        // SAFETY: a valid HWND, a documented attribute id, and a pointer to a
        // correctly-sized BOOL (DWM expects a 4-byte BOOL here).
        unsafe {
            DwmSetWindowAttribute(
                hwnd as HWND,
                DWMWA_USE_IMMERSIVE_DARK_MODE as u32,
                &flag as *const i32 as *const c_void,
                size_of::<i32>() as u32,
            );
        }
    }

    pub fn apply(hwnd: isize, blur: BackgroundBlur, tint: Rgb, opacity: f32) -> Backdrop {
        // The same predicate the chrome uses to pick its light/dark surfaces, so
        // the title bar and the tab strip below it can never disagree.
        set_dark_mode(hwnd, crate::theme::prefers_dark(tint));

        if !blur.enabled() {
            // Clear both mechanisms, so toggling the key off in a config reload
            // actually takes the backdrop away.
            try_system_backdrop(hwnd, DWMSBT_NONE);
            try_accent_policy(hwnd, ACCENT_DISABLED, tint, 0.0);
            return Backdrop::Off;
        }

        // Prefer the accent policy even on Win11, because it is the **only** path
        // that accepts a tint. `DWMWA_SYSTEMBACKDROP_TYPE` renders the stock
        // system material, which over a dark terminal reads as a washed-out grey
        // haze no matter what `DWMWA_USE_IMMERSIVE_DARK_MODE` says. Here we tint
        // the blur with the configured background instead.
        //
        // The tint carries the darkness that the (translucent) window content
        // doesn't: at `background-opacity = 0.25` the content contributes only a
        // quarter, so the glass behind it is tinted the remaining three quarters.
        // That keeps the overall look "your background color, blurred" across the
        // whole opacity range.
        let tint_alpha = 1.0 - opacity.clamp(0.0, 1.0);
        clear_blur_behind(hwnd); // fights the accent policy on some builds
        let accent = Backdrop::for_blur(blur, false);
        let state = if accent == Backdrop::AccentAcrylic {
            ACCENT_ENABLE_ACRYLICBLURBEHIND
        } else {
            ACCENT_ENABLE_BLURBEHIND
        };
        if try_accent_policy(hwnd, state, tint, tint_alpha) {
            return accent;
        }

        // No `SetWindowCompositionAttribute` export: fall back to the documented
        // Win11 attribute and accept the untinted system material.
        let system = Backdrop::for_blur(blur, true);
        let kind = if system == Backdrop::SystemAcrylic {
            DWMSBT_TRANSIENTWINDOW
        } else {
            DWMSBT_MAINWINDOW
        };
        if try_system_backdrop(hwnd, kind) {
            return system;
        }
        Backdrop::Unsupported
    }
}

#[cfg(not(windows))]
mod imp {
    use super::{Backdrop, Rgb};
    use crate::config::BackgroundBlur;

    pub fn apply(_hwnd: isize, _blur: BackgroundBlur, _tint: Rgb, _opacity: f32) -> Backdrop {
        Backdrop::Unsupported
    }
}

/// Apply (or clear) the backdrop on `hwnd`. `tint`/`opacity` are the window
/// background color and `background-opacity`, used only by the Win10 accent path,
/// which tints its own blur layer.
pub fn apply(hwnd: isize, blur: BackgroundBlur, tint: Rgb, opacity: f32) -> Backdrop {
    imp::apply(hwnd, blur, tint, opacity)
}

#[cfg(test)]
mod tests {
    use super::Backdrop;
    use crate::config::BackgroundBlur;

    #[test]
    fn backdrop_maps_intensity_to_two_buckets() {
        // Disabled.
        assert_eq!(
            Backdrop::for_blur(BackgroundBlur::Off, false),
            Backdrop::Off
        );
        assert_eq!(
            Backdrop::for_blur(BackgroundBlur::Radius(0), false),
            Backdrop::Off
        );

        // Low intensities take the subtler effect, higher ones the stronger.
        assert_eq!(
            Backdrop::for_blur(BackgroundBlur::Radius(5), false),
            Backdrop::AccentBlur
        );
        assert_eq!(
            Backdrop::for_blur(BackgroundBlur::Radius(10), false),
            Backdrop::AccentAcrylic
        );
        // Ghostty's `true` is intensity 20, which must reach acrylic.
        assert_eq!(
            Backdrop::for_blur(BackgroundBlur::On, false),
            Backdrop::AccentAcrylic
        );
        assert_eq!(
            Backdrop::for_blur(BackgroundBlur::Radius(255), false),
            Backdrop::AccentAcrylic
        );

        // The untinted system-attribute fallback mirrors the same split.
        assert_eq!(
            Backdrop::for_blur(BackgroundBlur::Radius(5), true),
            Backdrop::SystemMica
        );
        assert_eq!(
            Backdrop::for_blur(BackgroundBlur::On, true),
            Backdrop::SystemAcrylic
        );
        assert_eq!(Backdrop::for_blur(BackgroundBlur::Off, true), Backdrop::Off);
    }
}
