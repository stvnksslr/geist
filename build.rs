//! Embeds the Win32 icon resource into the `giest` executable.
//!
//! This is the *other* half of the icon story from `src/icon.rs`. That module
//! sets the icon on a live window (taskbar button, Alt-Tab); this one writes an
//! `RT_GROUP_ICON` resource into the exe, which is what Explorer, the Start
//! menu, the file's own property sheet and a pinned shortcut read without ever
//! running the process. Neither substitutes for the other.
//!
//! The `.ico` carries eight hand-tuned sizes (16–256) precisely so the shell can
//! pick per context and DPI instead of downscaling one bitmap, so the resource
//! path — not the `IconData` path — is the one that shows the small artwork.
//!
//! A missing resource compiler is a **warning, not an error**: `rc.exe` ships
//! with the Windows SDK and is normally present alongside the MSVC toolchain
//! that already links this crate, but an icon is window dressing and should not
//! be able to fail an otherwise good build. The warning is deliberate — a
//! silently icon-less binary is exactly the kind of thing nobody notices.

fn main() {
    // The Zig-built libghostty-vt dominates a cold build; don't re-run this
    // (or force a relink) unless the artwork itself changed.
    println!("cargo:rerun-if-changed=assets/icon.ico");
    println!("cargo:rerun-if-changed=build.rs");

    #[cfg(windows)]
    {
        let mut res = winresource::WindowsResource::new();
        res.set_icon("assets/icon.ico");
        if let Err(e) = res.compile() {
            println!("cargo:warning=giest: could not embed the icon resource ({e}). The exe will build and run, but Explorer and the taskbar will show the default icon. This usually means rc.exe (Windows SDK) is not discoverable.");
        }
    }
}
