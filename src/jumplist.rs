//! Taskbar Jump List tasks — the Windows analogue of the macOS Dock menu
//! (`AppDelegate.applicationDockMenu`: "New Window" / "New Tab").
//!
//! Tasks are shell links to giest's own executable with CLI arguments
//! (`+new-window`, `+new-tab`, `+new-tab --command=<profile>`), so clicking
//! one launches a short-lived giest that forwards the request to the running
//! instance over IPC (`ipc.rs`) — or, with none running, starts one.
//!
//! **Why the COM vtables are declared by hand.** `windows-sys` ships no COM
//! interfaces (see `taskbar.rs` for the same trade). The four interfaces used
//! here — `ICustomDestinationList`, `IObjectCollection`, `IShellLinkW`,
//! `IPropertyStore` — are declared as vtable *prefixes* in SDK order. **A wrong
//! slot fails silently or calls the wrong method**, which is what the ignored
//! host test exists to catch: it publishes a list under a throwaway AppUserModelID
//! and deletes it again, asserting every HRESULT on the way.
//!
//! The list is keyed on the process's AppUserModelID; giest sets none, so
//! Windows derives one from the executable path. `jump-list = false` deletes a
//! previously published list rather than just not publishing one, so turning
//! the key off actually removes the entries.

/// One task: a title and the arguments giest is launched with.
#[derive(Clone, Debug, PartialEq)]
pub struct Task {
    pub title: String,
    pub args: String,
}

/// Quote one argument for `CommandLineToArgvW` / the MSVC rules. Titles come
/// from profile names, so this only has to be correct, not clever.
pub fn quote_arg(a: &str) -> String {
    if !a.is_empty() && !a.contains([' ', '\t', '"']) {
        return a.to_string();
    }
    let mut out = String::from("\"");
    let mut backslashes = 0;
    for c in a.chars() {
        match c {
            '\\' => backslashes += 1,
            '"' => {
                out.extend(std::iter::repeat_n('\\', backslashes * 2 + 1));
                out.push('"');
                backslashes = 0;
            }
            _ => {
                out.extend(std::iter::repeat_n('\\', backslashes));
                out.push(c);
                backslashes = 0;
            }
        }
    }
    out.extend(std::iter::repeat_n('\\', backslashes * 2));
    out.push('"');
    out
}

/// The tasks for a profile list: New Window, New Tab, then one New Tab per
/// profile (resolved by name through `command`, so a profile keeps its prompt
/// hooks — `profiles::for_command`).
pub fn tasks(profile_names: &[&str]) -> Vec<Task> {
    let mut out = vec![
        Task {
            title: "New Window".into(),
            args: "+new-window".into(),
        },
        Task {
            title: "New Tab".into(),
            args: "+new-tab".into(),
        },
    ];
    for name in profile_names {
        out.push(Task {
            title: format!("New Tab: {name}"),
            args: format!("+new-tab {}", quote_arg(&format!("--command={name}"))),
        });
    }
    out
}

#[cfg(windows)]
mod imp {
    use std::ffi::c_void;
    use std::os::windows::ffi::OsStrExt;

    use super::Task;

    #[repr(C)]
    struct Guid(u32, u16, u16, [u8; 8]);

    const CLSID_DESTINATION_LIST: Guid = Guid(
        0x77f1_0cf0,
        0x3db5,
        0x4966,
        [0xb5, 0x20, 0xb7, 0xc5, 0x4f, 0xd3, 0x5e, 0xd6],
    );
    const IID_ICUSTOM_DESTINATION_LIST: Guid = Guid(
        0x6332_debf,
        0x87b5,
        0x4670,
        [0x90, 0xc0, 0x5e, 0x57, 0xb4, 0x08, 0xa4, 0x9e],
    );
    const CLSID_ENUMERABLE_OBJECT_COLLECTION: Guid = Guid(
        0x2d34_68c1,
        0x36a7,
        0x43b6,
        [0xac, 0x24, 0xd3, 0xf0, 0x2f, 0xd9, 0x60, 0x7a],
    );
    const IID_IOBJECT_COLLECTION: Guid = Guid(
        0x5632_b1a4,
        0xe38a,
        0x400a,
        [0x92, 0x8a, 0xd4, 0xcd, 0x63, 0x23, 0x02, 0x95],
    );
    const IID_IOBJECT_ARRAY: Guid = Guid(
        0x92ca_9dcd,
        0x5622,
        0x4bba,
        [0xa8, 0x05, 0x5e, 0x9f, 0x54, 0x1b, 0xd8, 0xc9],
    );
    const CLSID_SHELL_LINK: Guid = Guid(0x0002_1401, 0, 0, [0xc0, 0, 0, 0, 0, 0, 0, 0x46]);
    const IID_ISHELL_LINK_W: Guid = Guid(0x0002_14f9, 0, 0, [0xc0, 0, 0, 0, 0, 0, 0, 0x46]);
    const IID_IPROPERTY_STORE: Guid = Guid(
        0x886d_8eeb,
        0x8cf2,
        0x4446,
        [0x8d, 0x02, 0xcd, 0xba, 0x1d, 0xbd, 0xcf, 0x99],
    );

    /// `PKEY_Title`: {F29F85E0-4FF9-1068-AB91-08002B27B3D9}, pid 2.
    #[repr(C)]
    struct PropertyKey {
        fmtid: Guid,
        pid: u32,
    }
    const PKEY_TITLE: PropertyKey = PropertyKey {
        fmtid: Guid(
            0xf29f_85e0,
            0x4ff9,
            0x1068,
            [0xab, 0x91, 0x08, 0x00, 0x2b, 0x27, 0xb3, 0xd9],
        ),
        pid: 2,
    };

    /// `PROPVARIANT` holding a `VT_LPWSTR`. 24 bytes on x64: an 8-byte header
    /// (`vt` + three reserved words) then a 16-byte union.
    #[repr(C)]
    struct PropVariant {
        vt: u16,
        r1: u16,
        r2: u16,
        r3: u16,
        pwsz: *const u16,
        pad: usize,
    }
    const VT_LPWSTR: u16 = 31;
    #[cfg(target_pointer_width = "64")]
    const _: () = assert!(size_of::<PropVariant>() == 24);

    type Qi = unsafe extern "system" fn(*mut c_void, *const Guid, *mut *mut c_void) -> i32;
    type Ref = unsafe extern "system" fn(*mut c_void) -> u32;
    type Stub = usize;

    /// `ICustomDestinationList`, complete.
    #[repr(C)]
    struct DestListVtbl {
        query_interface: Qi,
        add_ref: Ref,
        release: Ref,
        set_app_id: unsafe extern "system" fn(*mut c_void, *const u16) -> i32,
        begin_list:
            unsafe extern "system" fn(*mut c_void, *mut u32, *const Guid, *mut *mut c_void) -> i32,
        append_category: Stub,
        append_known_category: Stub,
        add_user_tasks: unsafe extern "system" fn(*mut c_void, *mut c_void) -> i32,
        commit_list: unsafe extern "system" fn(*mut c_void) -> i32,
        get_removed_destinations: Stub,
        delete_list: unsafe extern "system" fn(*mut c_void, *const u16) -> i32,
        abort_list: unsafe extern "system" fn(*mut c_void) -> i32,
    }

    /// `IObjectCollection` (which extends `IObjectArray`), up to `AddObject`.
    #[repr(C)]
    struct CollectionVtbl {
        query_interface: Qi,
        add_ref: Ref,
        release: Ref,
        // IObjectArray
        get_count: unsafe extern "system" fn(*mut c_void, *mut u32) -> i32,
        get_at: Stub,
        // IObjectCollection
        add_object: unsafe extern "system" fn(*mut c_void, *mut c_void) -> i32,
    }

    /// `IShellLinkW`, up to `SetPath` (slot 20).
    #[repr(C)]
    struct ShellLinkVtbl {
        query_interface: Qi,
        add_ref: Ref,
        release: Ref,
        get_path: Stub,
        get_id_list: Stub,
        set_id_list: Stub,
        get_description: Stub,
        set_description: unsafe extern "system" fn(*mut c_void, *const u16) -> i32,
        get_working_directory: Stub,
        set_working_directory: Stub,
        get_arguments: Stub,
        set_arguments: unsafe extern "system" fn(*mut c_void, *const u16) -> i32,
        get_hotkey: Stub,
        set_hotkey: Stub,
        get_show_cmd: Stub,
        set_show_cmd: Stub,
        get_icon_location: Stub,
        set_icon_location: unsafe extern "system" fn(*mut c_void, *const u16, i32) -> i32,
        set_relative_path: Stub,
        resolve: Stub,
        set_path: unsafe extern "system" fn(*mut c_void, *const u16) -> i32,
    }

    /// `IPropertyStore`, complete.
    #[repr(C)]
    struct PropStoreVtbl {
        query_interface: Qi,
        add_ref: Ref,
        release: Ref,
        get_count: Stub,
        get_at: Stub,
        get_value: Stub,
        set_value:
            unsafe extern "system" fn(*mut c_void, *const PropertyKey, *const PropVariant) -> i32,
        commit: unsafe extern "system" fn(*mut c_void) -> i32,
    }

    const CLSCTX_INPROC_SERVER: u32 = 0x1;
    const COINIT_APARTMENTTHREADED: u32 = 0x2;

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

    fn wide(s: &str) -> Vec<u16> {
        std::ffi::OsStr::new(s)
            .encode_wide()
            .chain(Some(0))
            .collect()
    }

    /// An owned COM pointer, released on drop.
    struct Com(*mut c_void);
    impl Com {
        unsafe fn vtbl<T>(&self) -> &T {
            // SAFETY: caller names the interface this pointer was obtained as.
            unsafe { &**(self.0 as *const *const T) }
        }
    }
    impl Drop for Com {
        fn drop(&mut self) {
            if !self.0.is_null() {
                // SAFETY: every COM vtable starts with IUnknown.
                unsafe {
                    let v = &**(self.0 as *const *const CollectionVtbl);
                    (v.release)(self.0);
                }
            }
        }
    }

    fn hr(code: i32, what: &str) -> Result<(), String> {
        if code >= 0 {
            Ok(())
        } else {
            Err(format!("{what} failed: HRESULT 0x{:08X}", code as u32))
        }
    }

    fn create(clsid: &Guid, iid: &Guid, what: &str) -> Result<Com, String> {
        let mut p: *mut c_void = std::ptr::null_mut();
        // SAFETY: documented call with valid GUIDs and out-pointer.
        hr(
            unsafe {
                CoCreateInstance(
                    clsid,
                    std::ptr::null_mut(),
                    CLSCTX_INPROC_SERVER,
                    iid,
                    &mut p,
                )
            },
            what,
        )?;
        if p.is_null() {
            return Err(format!("{what} returned null"));
        }
        Ok(Com(p))
    }

    fn dest_list(app_id: Option<&str>) -> Result<Com, String> {
        // SAFETY: the UI thread is already an STA (winit's OleInitialize); this
        // then returns S_FALSE or RPC_E_CHANGED_MODE, both fine. Never
        // uninitialized: we don't own the apartment.
        unsafe {
            CoInitializeEx(std::ptr::null_mut(), COINIT_APARTMENTTHREADED);
        }
        let list = create(
            &CLSID_DESTINATION_LIST,
            &IID_ICUSTOM_DESTINATION_LIST,
            "DestinationList",
        )?;
        if let Some(id) = app_id {
            let w = wide(id);
            // SAFETY: vtable of the interface we asked for; valid string.
            hr(
                unsafe { (list.vtbl::<DestListVtbl>().set_app_id)(list.0, w.as_ptr()) },
                "SetAppID",
            )?;
        }
        Ok(list)
    }

    fn link(exe: &str, task: &Task) -> Result<Com, String> {
        let l = create(&CLSID_SHELL_LINK, &IID_ISHELL_LINK_W, "ShellLink")?;
        let (path, args, title) = (wide(exe), wide(&task.args), wide(&task.title));
        // SAFETY: vtable of the interface we asked for; strings outlive calls.
        unsafe {
            let v = l.vtbl::<ShellLinkVtbl>();
            hr((v.set_path)(l.0, path.as_ptr()), "IShellLink::SetPath")?;
            hr(
                (v.set_arguments)(l.0, args.as_ptr()),
                "IShellLink::SetArguments",
            )?;
            hr(
                (v.set_description)(l.0, title.as_ptr()),
                "IShellLink::SetDescription",
            )?;
            hr(
                (v.set_icon_location)(l.0, path.as_ptr(), 0),
                "IShellLink::SetIconLocation",
            )?;

            // A task's visible label is the link's PKEY_Title, not its
            // description.
            let mut ps: *mut c_void = std::ptr::null_mut();
            hr(
                (v.query_interface)(l.0, &IID_IPROPERTY_STORE, &mut ps),
                "QI IPropertyStore",
            )?;
            let ps = Com(ps);
            let pv = PropVariant {
                vt: VT_LPWSTR,
                r1: 0,
                r2: 0,
                r3: 0,
                pwsz: title.as_ptr(),
                pad: 0,
            };
            let pvt = ps.vtbl::<PropStoreVtbl>();
            // SetValue copies the value, so a borrowed string is fine and no
            // PropVariantClear is needed.
            hr(
                (pvt.set_value)(ps.0, &PKEY_TITLE, &pv),
                "IPropertyStore::SetValue",
            )?;
            hr((pvt.commit)(ps.0), "IPropertyStore::Commit")?;
        }
        Ok(l)
    }

    /// Publish `tasks` for `exe`. `app_id` overrides the process's
    /// AppUserModelID (the test uses a throwaway one).
    pub fn publish(exe: &str, tasks: &[Task], app_id: Option<&str>) -> Result<(), String> {
        let list = dest_list(app_id)?;
        // SAFETY: vtables of the interfaces we asked for; out-pointers valid.
        unsafe {
            let lv = list.vtbl::<DestListVtbl>();
            let mut slots = 0u32;
            let mut removed: *mut c_void = std::ptr::null_mut();
            hr(
                (lv.begin_list)(list.0, &mut slots, &IID_IOBJECT_ARRAY, &mut removed),
                "BeginList",
            )?;
            let _removed = Com(removed);
            let result = (|| {
                let coll = create(
                    &CLSID_ENUMERABLE_OBJECT_COLLECTION,
                    &IID_IOBJECT_COLLECTION,
                    "EnumerableObjectCollection",
                )?;
                let cv = coll.vtbl::<CollectionVtbl>();
                for t in tasks {
                    let l = link(exe, t)?;
                    hr((cv.add_object)(coll.0, l.0), "IObjectCollection::AddObject")?;
                }
                let mut n = 0u32;
                hr((cv.get_count)(coll.0, &mut n), "IObjectArray::GetCount")?;
                if n as usize != tasks.len() {
                    return Err(format!(
                        "collection holds {n} tasks, expected {}",
                        tasks.len()
                    ));
                }
                // IObjectCollection *is* an IObjectArray (single inheritance),
                // so the same pointer is passed.
                hr((lv.add_user_tasks)(list.0, coll.0), "AddUserTasks")?;
                hr((lv.commit_list)(list.0), "CommitList")
            })();
            if result.is_err() {
                (lv.abort_list)(list.0);
            }
            result
        }
    }

    /// Delete the published list (for `app_id`, or this process's).
    pub fn clear(app_id: Option<&str>) -> Result<(), String> {
        let list = dest_list(None)?;
        let w = app_id.map(wide);
        // SAFETY: vtable of the interface we asked for.
        hr(
            unsafe {
                (list.vtbl::<DestListVtbl>().delete_list)(
                    list.0,
                    w.as_ref().map_or(std::ptr::null(), |w| w.as_ptr()),
                )
            },
            "DeleteList",
        )
    }
}

#[cfg(not(windows))]
mod imp {
    use super::Task;
    pub fn publish(_: &str, _: &[Task], _: Option<&str>) -> Result<(), String> {
        Ok(())
    }
    pub fn clear(_: Option<&str>) -> Result<(), String> {
        Ok(())
    }
}

/// Publish (or, with `enabled = false`, remove) giest's Jump List. Best effort:
/// a failure is reported once and never blocks startup.
pub fn sync(enabled: bool, profile_names: &[&str]) {
    let result = if enabled {
        match std::env::current_exe() {
            Ok(exe) => imp::publish(&exe.display().to_string(), &tasks(profile_names), None),
            Err(e) => Err(e.to_string()),
        }
    } else {
        imp::clear(None)
    };
    if let Err(e) = result {
        eprintln!("giest: jump list: {e}");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tasks_are_new_window_new_tab_then_one_per_profile() {
        let t = tasks(&["pwsh", "Command Prompt"]);
        assert_eq!(t.len(), 4);
        assert_eq!(
            t[0],
            Task {
                title: "New Window".into(),
                args: "+new-window".into()
            }
        );
        assert_eq!(t[1].args, "+new-tab");
        assert_eq!(t[2].args, "+new-tab --command=pwsh");
        assert_eq!(t[3].title, "New Tab: Command Prompt");
        assert_eq!(t[3].args, r#"+new-tab "--command=Command Prompt""#);
    }

    #[test]
    fn quoting_follows_the_msvc_argv_rules() {
        assert_eq!(quote_arg("plain"), "plain");
        assert_eq!(quote_arg(""), "\"\"");
        assert_eq!(quote_arg("a b"), "\"a b\"");
        assert_eq!(quote_arg(r#"say "hi""#), r#""say \"hi\"""#);
        // Trailing backslashes double before the closing quote.
        assert_eq!(quote_arg(r"C:\a b\"), r#""C:\a b\\""#);
    }

    /// Publishes a real Jump List under a throwaway AppUserModelID, asserting
    /// every HRESULT — a wrong vtable slot shows up here as a failure or a
    /// crash instead of as a silently missing menu — and deletes it again.
    #[test]
    #[ignore = "writes (and removes) a real Jump List for a test AppUserModelID"]
    fn publish_and_delete_a_jump_list_under_a_test_app_id() {
        let id = format!("giest.test.jumplist.{}", std::process::id());
        let exe = std::env::current_exe().unwrap().display().to_string();
        let r = imp::publish(&exe, &tasks(&["cmd"]), Some(&id));
        // Always clean up, even when publishing failed half-way.
        let c = imp::clear(Some(&id));
        r.unwrap();
        c.unwrap();
    }
}
