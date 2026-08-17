//! **Global** keybinds — Ghostty's `keybind = global:<trigger>=<action>`, which
//! fire even when giest is not the focused application.
//!
//! Windows offers two mechanisms and only one of them fits:
//!
//! - `RegisterHotKey` posts `WM_HOTKEY` to the *thread message queue*. winit owns
//!   the message loop and surfaces no hook for unrecognized thread messages, so
//!   the message would be dispatched and dropped where we cannot see it.
//! - A **low-level keyboard hook** (`WH_KEYBOARD_LL`) calls back on the thread
//!   that installed it, during that thread's normal message dispatch — which
//!   winit is already pumping. That is the one used here.
//!
//! The callback runs on the UI thread inside the OS's input path, so it must be
//! fast and must never block: a hook that takes longer than the system's
//! `LowLevelHooksTimeout` is silently *unregistered* by Windows, and the feature
//! would then stop working with no error anywhere. It therefore does the minimum
//! — compare a virtual-key code, read the modifier state, set a bit — and leaves
//! every consequence to the next frame.
//!
//! A matched chord is **consumed** (the hook returns non-zero), so the key never
//! reaches whatever app was focused. That is the point of a global binding: the
//! backtick in `global:ctrl+grave` must not also be typed into the editor the
//! user was in. Unmatched keys are passed straight through.
//!
//! No key *state* is tracked here: [`fired`] returns which bindings triggered
//! since the last call, and the app runs their actions from its own frame, where
//! it can borrow whatever it needs.

use crate::engine::{KeyCode, KeyMods};
use crate::keybind::Chord;

/// Map a neutral [`KeyCode`] onto a Windows virtual-key code.
///
/// Pure and platform-independent so the table can be tested off Windows. The
/// letter/digit VKs are the ASCII values of their uppercase forms — that is the
/// documented definition, not a coincidence to be "cleaned up".
///
/// Returns `None` for keys with no stable VK, which the caller reports rather
/// than binding to something arbitrary.
pub fn vk_for(code: KeyCode) -> Option<u32> {
    use KeyCode::*;
    Some(match code {
        // Never a real key, so it has no virtual-key code and can never match a
        // global hotkey.
        CatchAll => return None,
        A => 0x41,
        B => 0x42,
        C => 0x43,
        D => 0x44,
        E => 0x45,
        F => 0x46,
        G => 0x47,
        H => 0x48,
        I => 0x49,
        J => 0x4A,
        K => 0x4B,
        L => 0x4C,
        M => 0x4D,
        N => 0x4E,
        O => 0x4F,
        P => 0x50,
        Q => 0x51,
        R => 0x52,
        S => 0x53,
        T => 0x54,
        U => 0x55,
        V => 0x56,
        W => 0x57,
        X => 0x58,
        Y => 0x59,
        Z => 0x5A,
        Digit0 => 0x30,
        Digit1 => 0x31,
        Digit2 => 0x32,
        Digit3 => 0x33,
        Digit4 => 0x34,
        Digit5 => 0x35,
        Digit6 => 0x36,
        Digit7 => 0x37,
        Digit8 => 0x38,
        Digit9 => 0x39,
        Enter => 0x0D,
        Tab => 0x09,
        Backspace => 0x08,
        Escape => 0x1B,
        Space => 0x20,
        Delete => 0x2E,
        Insert => 0x2D,
        Home => 0x24,
        End => 0x23,
        PageUp => 0x21,
        PageDown => 0x22,
        ArrowUp => 0x26,
        ArrowDown => 0x28,
        ArrowLeft => 0x25,
        ArrowRight => 0x27,
        F1 => 0x70,
        F2 => 0x71,
        F3 => 0x72,
        F4 => 0x73,
        F5 => 0x74,
        F6 => 0x75,
        F7 => 0x76,
        F8 => 0x77,
        F9 => 0x78,
        F10 => 0x79,
        F11 => 0x7A,
        F12 => 0x7B,
        // The OEM keys are layout-dependent by definition; these are the US
        // assignments, which is also what the chord parser's names describe.
        Minus => 0xBD,
        Equal => 0xBB,
        BracketLeft => 0xDB,
        BracketRight => 0xDD,
        Backslash => 0xDC,
        Semicolon => 0xBA,
        Quote => 0xDE,
        Backquote => 0xC0,
        Comma => 0xBC,
        Period => 0xBE,
        Slash => 0xBF,
    })
}

/// One registered global binding: the virtual key plus the modifiers that must
/// be held. Deliberately *exact* — `ctrl+grave` does not fire on
/// `ctrl+shift+grave`, matching how the in-app keymap matches a chord.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct GlobalBind {
    pub vk: u32,
    pub mods: KeyMods,
}

impl GlobalBind {
    /// Build from a parsed chord, or `None` if the key has no virtual-key code.
    pub fn from_chord(chord: &Chord) -> Option<Self> {
        Some(Self {
            vk: vk_for(chord.code)?,
            mods: chord.mods,
        })
    }
}

/// Which binding a key event matches, if any. Pure, so the matching rule is
/// testable without synthesizing OS input.
///
/// The **first** match wins rather than the last: a global binding list is short
/// and user-authored, and a duplicate should behave predictably rather than
/// depend on registration order the user can't see.
pub fn match_bind(binds: &[GlobalBind], vk: u32, mods: KeyMods) -> Option<usize> {
    binds.iter().position(|b| b.vk == vk && b.mods == mods)
}

#[cfg(windows)]
mod imp {
    use super::GlobalBind;
    use crate::engine::KeyMods;
    use eframe::egui;
    use std::sync::Mutex;
    use std::sync::atomic::{AtomicU32, Ordering};

    const WH_KEYBOARD_LL: i32 = 13;
    const WM_KEYDOWN: usize = 0x0100;
    const WM_SYSKEYDOWN: usize = 0x0104;

    const VK_SHIFT: i32 = 0x10;
    const VK_CONTROL: i32 = 0x11;
    const VK_MENU: i32 = 0x12;
    const VK_LWIN: i32 = 0x5B;
    const VK_RWIN: i32 = 0x5C;

    #[repr(C)]
    struct KbdLlHookStruct {
        vk_code: u32,
        scan_code: u32,
        flags: u32,
        time: u32,
        extra: usize,
    }

    // user32 is already linked by winit.
    #[link(name = "user32")]
    unsafe extern "system" {
        fn SetWindowsHookExW(
            id: i32,
            proc_: unsafe extern "system" fn(i32, usize, isize) -> isize,
            module: *mut std::ffi::c_void,
            thread: u32,
        ) -> *mut std::ffi::c_void;
        fn UnhookWindowsHookEx(hook: *mut std::ffi::c_void) -> i32;
        fn CallNextHookEx(
            hook: *mut std::ffi::c_void,
            code: i32,
            wparam: usize,
            lparam: isize,
        ) -> isize;
        fn GetAsyncKeyState(vk: i32) -> i16;
    }

    /// The registered bindings. A `Mutex` rather than a lock-free structure
    /// because the only writer is a config reload; the hook uses `try_lock` so it
    /// can never block the OS input path even during that write.
    static BINDS: Mutex<Vec<GlobalBind>> = Mutex::new(Vec::new());
    /// One bit per binding index, set by the hook and drained by the app. A
    /// bitmask keeps the callback allocation-free, which is what lets it stay
    /// inside the low-level-hook time budget.
    static FIRED: AtomicU32 = AtomicU32::new(0);
    /// The installed hook handle, as a `usize` so it can live in an atomic.
    static HOOK: AtomicU32 = AtomicU32::new(0);
    static HOOK_PTR: Mutex<usize> = Mutex::new(0);
    /// The egui context to wake when a binding fires, so a hidden/idle window
    /// reacts immediately instead of on the next 500 ms poll.
    static WAKE: Mutex<Option<egui::Context>> = Mutex::new(None);

    /// The number of bindings the bitmask can hold. More than any real config,
    /// and the excess is reported rather than silently dropped.
    pub const MAX_BINDS: usize = 32;

    fn held(vk: i32) -> bool {
        // SAFETY: no pointers; a plain VK query.
        (unsafe { GetAsyncKeyState(vk) } as u16 & 0x8000) != 0
    }

    unsafe extern "system" fn hook_proc(code: i32, wparam: usize, lparam: isize) -> isize {
        // `code < 0` means "pass it on without inspecting", per the API contract.
        if code >= 0 && (wparam == WM_KEYDOWN || wparam == WM_SYSKEYDOWN) {
            // SAFETY: for HC_ACTION on a keyboard hook, lparam points at a
            // KBDLLHOOKSTRUCT owned by the OS for the duration of the call.
            let vk = unsafe { (*(lparam as *const KbdLlHookStruct)).vk_code };
            let mods = KeyMods {
                shift: held(VK_SHIFT),
                ctrl: held(VK_CONTROL),
                alt: held(VK_MENU),
                sup: held(VK_LWIN) || held(VK_RWIN),
            };
            // `try_lock`: never block here. A miss costs one keypress during a
            // config reload; blocking could cost the hook itself.
            if let Ok(binds) = BINDS.try_lock()
                && let Some(i) = super::match_bind(&binds, vk, mods)
            {
                FIRED.fetch_or(1 << i, Ordering::Relaxed);
                if let Ok(w) = WAKE.try_lock()
                    && let Some(ctx) = w.as_ref()
                {
                    ctx.request_repaint();
                }
                // Swallow it: a global binding must not also reach the app the
                // user was typing into.
                return 1;
            }
        }
        // SAFETY: the documented pass-through; a null handle is accepted.
        unsafe { CallNextHookEx(std::ptr::null_mut(), code, wparam, lparam) }
    }

    /// Install (or update) the global bindings. Passing an empty list removes
    /// the hook entirely, so a config without `global:` binds pays nothing —
    /// including the system-wide cost of a low-level hook.
    pub fn set_binds(ctx: &egui::Context, mut binds: Vec<GlobalBind>) {
        if binds.len() > MAX_BINDS {
            eprintln!(
                "giest: only the first {MAX_BINDS} global keybinds are supported; ignoring {} more",
                binds.len() - MAX_BINDS
            );
            binds.truncate(MAX_BINDS);
        }
        let want_hook = !binds.is_empty();
        *BINDS.lock().unwrap_or_else(|e| e.into_inner()) = binds;
        *WAKE.lock().unwrap_or_else(|e| e.into_inner()) = Some(ctx.clone());

        let mut hook = HOOK_PTR.lock().unwrap_or_else(|e| e.into_inner());
        if want_hook && *hook == 0 {
            // SAFETY: a valid hook id and callback; a null module handle is
            // correct for a hook proc in this process.
            let h = unsafe { SetWindowsHookExW(WH_KEYBOARD_LL, hook_proc, std::ptr::null_mut(), 0) };
            if h.is_null() {
                eprintln!("giest: could not install the global-keybind hook; global: binds are inactive");
            } else {
                *hook = h as usize;
                HOOK.store(1, Ordering::Relaxed);
            }
        } else if !want_hook && *hook != 0 {
            // SAFETY: a handle this process installed and has not yet removed.
            unsafe { UnhookWindowsHookEx(*hook as *mut std::ffi::c_void) };
            *hook = 0;
            HOOK.store(0, Ordering::Relaxed);
        }
    }

    /// Indices of the bindings that fired since the last call.
    pub fn fired() -> Vec<usize> {
        let bits = FIRED.swap(0, Ordering::Relaxed);
        (0..MAX_BINDS).filter(|i| bits & (1 << i) != 0).collect()
    }

    /// Whether the hook is currently installed. Exists so a test can assert the
    /// install/remove transitions, which are otherwise invisible.
    pub fn installed() -> bool {
        HOOK.load(Ordering::Relaxed) != 0
    }
}

#[cfg(not(windows))]
mod imp {
    use super::GlobalBind;
    use eframe::egui;
    pub const MAX_BINDS: usize = 32;
    pub fn set_binds(_ctx: &egui::Context, _binds: Vec<GlobalBind>) {}
    pub fn fired() -> Vec<usize> {
        Vec::new()
    }
    pub fn installed() -> bool {
        false
    }
}

pub use imp::{MAX_BINDS, fired, installed, set_binds};

#[cfg(test)]
mod tests {
    use super::*;

    fn mods(ctrl: bool, shift: bool) -> KeyMods {
        KeyMods {
            ctrl,
            shift,
            alt: false,
            sup: false,
        }
    }

    #[test]
    fn letters_and_digits_use_their_ascii_virtual_keys() {
        assert_eq!(vk_for(KeyCode::A), Some(0x41));
        assert_eq!(vk_for(KeyCode::Z), Some(0x5A));
        assert_eq!(vk_for(KeyCode::Digit0), Some(0x30));
        assert_eq!(vk_for(KeyCode::Digit9), Some(0x39));
    }

    #[test]
    fn every_key_code_has_a_virtual_key() {
        // The chord parser accepts all of these, so any gap would be a trigger
        // that parses in the config and then silently never fires.
        for code in [
            KeyCode::Backquote,
            KeyCode::Space,
            KeyCode::F12,
            KeyCode::ArrowLeft,
            KeyCode::Slash,
            KeyCode::Quote,
            KeyCode::Escape,
        ] {
            assert!(vk_for(code).is_some(), "{code:?}");
        }
    }

    #[test]
    fn a_bind_matches_only_its_exact_modifier_set() {
        let binds = vec![GlobalBind {
            vk: 0xC0,
            mods: mods(true, false),
        }];
        assert_eq!(match_bind(&binds, 0xC0, mods(true, false)), Some(0));
        // Ctrl+Shift+` must not fire a Ctrl+` binding — same rule the in-app
        // keymap applies, so a global bind behaves like every other one.
        assert_eq!(match_bind(&binds, 0xC0, mods(true, true)), None);
        assert_eq!(match_bind(&binds, 0xC0, mods(false, false)), None);
        assert_eq!(match_bind(&binds, 0x41, mods(true, false)), None);
    }

    #[test]
    fn the_first_duplicate_wins_so_order_is_predictable() {
        let b = GlobalBind {
            vk: 0x41,
            mods: mods(true, false),
        };
        assert_eq!(match_bind(&[b, b], 0x41, mods(true, false)), Some(0));
    }

    #[test]
    fn a_chord_becomes_a_bind_with_its_modifiers_intact() {
        let chord = crate::keybind::parse_chord("ctrl+shift+grave").expect("parses");
        let bind = GlobalBind::from_chord(&chord).expect("grave has a VK");
        assert_eq!(bind.vk, 0xC0);
        assert!(bind.mods.ctrl && bind.mods.shift);
    }
}
