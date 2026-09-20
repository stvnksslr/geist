//! Explorer "Open giest here" — the Windows analogue of the macOS app's
//! Services menu entries ("New Ghostty Tab/Window Here").
//!
//! `giest +register-shell-integration` writes three per-user verbs under
//! `HKCU\Software\Classes`:
//!
//! | Key | Where it shows |
//! |---|---|
//! | `Directory\Background\shell\giest` | right-click on a folder's empty space |
//! | `Directory\shell\giest` | right-click on a folder |
//! | `Drive\shell\giest` | right-click on a drive |
//!
//! each running `"<exe>" "%V"` — the positional-directory CLI form, which the
//! running instance receives over IPC as a **new tab** (`cli::Cli::plan`). The
//! drive-root quoting accident that form causes (`"C:\"` → `C:"`) is repaired
//! in `cli::clean_path_arg`.
//!
//! Per-user (`HKCU`) on purpose: no elevation, nothing machine-wide, and
//! `+unregister-shell-integration` removes every key it wrote. Nothing is ever
//! registered implicitly — only the explicit CLI verb does it.

/// The shell verb's key name under each `…\shell`.
pub const VERB: &str = "giest";

/// The three class keys the verb is added to.
pub const CLASSES: [&str; 3] = [
    r"Directory\Background\shell",
    r"Directory\shell",
    r"Drive\shell",
];

/// The menu label.
pub const LABEL: &str = "Open giest here";

/// One registry value to write: `(subkey, value name or None for the default
/// value, data)`. Subkeys are relative to the root passed to [`register`].
pub type Entry = (String, Option<&'static str>, String);

/// Every value `register` writes, for the executable at `exe`. Pure, so the
/// exact strings — the quoting in particular — are tested.
pub fn entries(exe: &str) -> Vec<Entry> {
    let mut out = Vec::new();
    for class in CLASSES {
        let key = format!(r"{class}\{VERB}");
        out.push((key.clone(), None, LABEL.to_string()));
        out.push((key.clone(), Some("Icon"), format!("\"{exe}\",0")));
        out.push((format!(r"{key}\command"), None, format!("\"{exe}\" \"%V\"")));
    }
    out
}

/// The registry root the verbs live under in normal use.
pub const CLASSES_ROOT: &str = r"Software\Classes";

#[cfg(windows)]
mod imp {
    use std::ffi::c_void;
    use std::os::windows::ffi::OsStrExt;

    type Hkey = *mut c_void;
    const HKEY_CURRENT_USER: Hkey = 0x8000_0001u32 as i32 as isize as Hkey;
    const KEY_ALL_ACCESS: u32 = 0xF003F;
    const KEY_READ: u32 = 0x20019;
    const REG_SZ: u32 = 1;
    const ERROR_FILE_NOT_FOUND: i32 = 2;

    #[link(name = "advapi32")]
    unsafe extern "system" {
        fn RegCreateKeyExW(
            key: Hkey,
            subkey: *const u16,
            reserved: u32,
            class: *const u16,
            options: u32,
            sam: u32,
            sa: *const c_void,
            result: *mut Hkey,
            disposition: *mut u32,
        ) -> i32;
        fn RegOpenKeyExW(
            key: Hkey,
            subkey: *const u16,
            options: u32,
            sam: u32,
            result: *mut Hkey,
        ) -> i32;
        fn RegSetValueExW(
            key: Hkey,
            name: *const u16,
            reserved: u32,
            ty: u32,
            data: *const u8,
            len: u32,
        ) -> i32;
        fn RegQueryValueExW(
            key: Hkey,
            name: *const u16,
            reserved: *mut u32,
            ty: *mut u32,
            data: *mut u8,
            len: *mut u32,
        ) -> i32;
        fn RegDeleteTreeW(key: Hkey, subkey: *const u16) -> i32;
        fn RegCloseKey(key: Hkey) -> i32;
        fn RegDeleteKeyValueW(key: Hkey, subkey: *const u16, name: *const u16) -> i32;
        fn RegQueryInfoKeyW(
            key: Hkey,
            class: *mut u16,
            class_len: *mut u32,
            reserved: *mut u32,
            subkeys: *mut u32,
            max_subkey: *mut u32,
            max_class: *mut u32,
            values: *mut u32,
            max_value_name: *mut u32,
            max_value: *mut u32,
            sd: *mut u32,
            written: *mut c_void,
        ) -> i32;
    }

    /// Delete `HKCU\<path>` only if it has no subkeys and no values — for a
    /// parent key that a registration created and must not leave behind.
    pub fn delete_if_empty(path: &str) -> std::io::Result<()> {
        let mut key: Hkey = std::ptr::null_mut();
        let p = wide(path);
        let n: *mut u16 = std::ptr::null_mut();
        let (mut subkeys, mut values) = (0u32, 0u32);
        // SAFETY: valid string/out-pointers; the key is closed below.
        unsafe {
            if RegOpenKeyExW(HKEY_CURRENT_USER, p.as_ptr(), 0, KEY_READ, &mut key) != 0 {
                return Ok(());
            }
            let r = RegQueryInfoKeyW(
                key,
                n,
                n.cast(),
                n.cast(),
                &mut subkeys,
                n.cast(),
                n.cast(),
                &mut values,
                n.cast(),
                n.cast(),
                n.cast(),
                n.cast(),
            );
            RegCloseKey(key);
            check(r, path)?;
        }
        if subkeys == 0 && values == 0 {
            delete_tree(path)
        } else {
            Ok(())
        }
    }

    /// Whether `HKCU\<path>` exists.
    pub fn key_exists(path: &str) -> bool {
        let mut key: Hkey = std::ptr::null_mut();
        let p = wide(path);
        // SAFETY: valid string/out-pointer; the key is closed below.
        unsafe {
            if RegOpenKeyExW(HKEY_CURRENT_USER, p.as_ptr(), 0, KEY_READ, &mut key) != 0 {
                return false;
            }
            RegCloseKey(key);
        }
        true
    }

    /// Delete the value `name` of `HKCU\<path>`. A missing value (or key) is
    /// not an error.
    pub fn delete_value(path: &str, name: &str) -> std::io::Result<()> {
        let p = wide(path);
        let n = wide(name);
        // SAFETY: valid strings.
        let r = unsafe { RegDeleteKeyValueW(HKEY_CURRENT_USER, p.as_ptr(), n.as_ptr()) };
        if r == ERROR_FILE_NOT_FOUND {
            return Ok(());
        }
        check(r, path)
    }

    fn wide(s: &str) -> Vec<u16> {
        std::ffi::OsStr::new(s)
            .encode_wide()
            .chain(Some(0))
            .collect()
    }

    fn check(code: i32, what: &str) -> std::io::Result<()> {
        if code == 0 {
            Ok(())
        } else {
            Err(std::io::Error::new(
                std::io::Error::from_raw_os_error(code).kind(),
                format!("{what}: {}", std::io::Error::from_raw_os_error(code)),
            ))
        }
    }

    pub fn set_string(path: &str, name: Option<&str>, data: &str) -> std::io::Result<()> {
        let mut key: Hkey = std::ptr::null_mut();
        let p = wide(path);
        // SAFETY: valid strings/out-pointers; the key is closed below.
        unsafe {
            check(
                RegCreateKeyExW(
                    HKEY_CURRENT_USER,
                    p.as_ptr(),
                    0,
                    std::ptr::null(),
                    0,
                    KEY_ALL_ACCESS,
                    std::ptr::null(),
                    &mut key,
                    std::ptr::null_mut(),
                ),
                path,
            )?;
            let n = name.map(wide);
            let d = wide(data);
            let r = RegSetValueExW(
                key,
                n.as_ref().map_or(std::ptr::null(), |n| n.as_ptr()),
                0,
                REG_SZ,
                d.as_ptr() as *const u8,
                (d.len() * 2) as u32,
            );
            RegCloseKey(key);
            check(r, path)
        }
    }

    pub fn get_string(path: &str, name: Option<&str>) -> Option<String> {
        let mut key: Hkey = std::ptr::null_mut();
        let p = wide(path);
        // SAFETY: valid strings/out-pointers; the key is closed below.
        unsafe {
            if RegOpenKeyExW(HKEY_CURRENT_USER, p.as_ptr(), 0, KEY_READ, &mut key) != 0 {
                return None;
            }
            let n = name.map(wide);
            let np = n.as_ref().map_or(std::ptr::null(), |n| n.as_ptr());
            let mut len = 0u32;
            let mut ty = 0u32;
            let mut out = None;
            if RegQueryValueExW(
                key,
                np,
                std::ptr::null_mut(),
                &mut ty,
                std::ptr::null_mut(),
                &mut len,
            ) == 0
                && ty == REG_SZ
            {
                let mut buf = vec![0u16; (len as usize).div_ceil(2)];
                if RegQueryValueExW(
                    key,
                    np,
                    std::ptr::null_mut(),
                    &mut ty,
                    buf.as_mut_ptr() as *mut u8,
                    &mut len,
                ) == 0
                {
                    let end = buf.iter().position(|&c| c == 0).unwrap_or(buf.len());
                    out = Some(String::from_utf16_lossy(&buf[..end]));
                }
            }
            RegCloseKey(key);
            out
        }
    }

    pub fn delete_tree(path: &str) -> std::io::Result<()> {
        let p = wide(path);
        // SAFETY: valid string.
        let r = unsafe { RegDeleteTreeW(HKEY_CURRENT_USER, p.as_ptr()) };
        if r == ERROR_FILE_NOT_FOUND {
            return Ok(());
        }
        check(r, path)
    }
}

#[cfg(not(windows))]
mod imp {
    pub fn set_string(_: &str, _: Option<&str>, _: &str) -> std::io::Result<()> {
        Err(std::io::ErrorKind::Unsupported.into())
    }
    pub fn get_string(_: &str, _: Option<&str>) -> Option<String> {
        None
    }
    pub fn delete_tree(_: &str) -> std::io::Result<()> {
        Ok(())
    }
    pub fn delete_if_empty(_: &str) -> std::io::Result<()> {
        Ok(())
    }
    pub fn key_exists(_: &str) -> bool {
        false
    }
    pub fn delete_value(_: &str, _: &str) -> std::io::Result<()> {
        Ok(())
    }
}

pub use imp::get_string;
/// Raw HKCU helpers, shared with `handoff.rs`'s registration.
pub(crate) use imp::{delete_if_empty, delete_tree, delete_value, key_exists, set_string};

/// Write every verb under `HKCU\<root>` for `exe`. On failure the partial
/// registration is removed again, so a half-installed menu can't linger.
pub fn register_at(root: &str, exe: &str) -> std::io::Result<()> {
    for (sub, name, data) in entries(exe) {
        if let Err(e) = imp::set_string(&format!(r"{root}\{sub}"), name, &data) {
            let _ = unregister_at(root);
            return Err(e);
        }
    }
    Ok(())
}

/// Remove every verb under `HKCU\<root>`. Missing keys are not an error.
pub fn unregister_at(root: &str) -> std::io::Result<()> {
    let mut first_err = None;
    for class in CLASSES {
        if let Err(e) = imp::delete_tree(&format!(r"{root}\{class}\{VERB}")) {
            first_err.get_or_insert(e);
        }
    }
    first_err.map_or(Ok(()), Err)
}

/// `+register-shell-integration` for the running executable.
pub fn register() -> std::io::Result<()> {
    let exe = std::env::current_exe()?;
    register_at(CLASSES_ROOT, &exe.display().to_string())
}

/// `+unregister-shell-integration`.
pub fn unregister() -> std::io::Result<()> {
    unregister_at(CLASSES_ROOT)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_class_gets_a_label_an_icon_and_a_quoted_command() {
        let e = entries(r"C:\Program Files\giest\giest.exe");
        assert_eq!(e.len(), CLASSES.len() * 3);
        assert!(e.contains(&(
            r"Directory\Background\shell\giest".into(),
            None,
            "Open giest here".into()
        )));
        assert!(e.contains(&(
            r"Directory\Background\shell\giest".into(),
            Some("Icon"),
            r#""C:\Program Files\giest\giest.exe",0"#.into()
        )));
        // Both halves quoted: a space in the exe path or the folder must not
        // split the command line.
        assert!(e.contains(&(
            r"Drive\shell\giest\command".into(),
            None,
            r#""C:\Program Files\giest\giest.exe" "%V""#.into()
        )));
    }

    /// Writes to a throwaway `HKCU\Software\giest-test-<pid>` tree — never the
    /// real `Software\Classes` — reads it back, and removes it.
    #[test]
    #[ignore = "touches the real registry (HKCU, throwaway key)"]
    fn register_then_unregister_round_trips_in_the_real_registry() {
        let root = format!(r"Software\giest-test-{}", std::process::id());
        register_at(&root, r"C:\x\giest.exe").unwrap();
        assert_eq!(
            get_string(&format!(r"{root}\Directory\shell\giest\command"), None).as_deref(),
            Some(r#""C:\x\giest.exe" "%V""#)
        );
        assert_eq!(
            get_string(&format!(r"{root}\Drive\shell\giest"), Some("Icon")).as_deref(),
            Some(r#""C:\x\giest.exe",0"#)
        );
        unregister_at(&root).unwrap();
        assert!(get_string(&format!(r"{root}\Directory\shell\giest"), None).is_none());
        // Idempotent.
        unregister_at(&root).unwrap();
        imp::delete_tree(&root).unwrap();
        assert!(get_string(&root, None).is_none());
    }
}
