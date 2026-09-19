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

    git_commit();

    #[cfg(windows)]
    {
        let mut res = winresource::WindowsResource::new();
        res.set_icon("assets/icon.ico");
        if let Err(e) = res.compile() {
            println!("cargo:warning=giest: could not embed the icon resource ({e}). The exe will build and run, but Explorer and the taskbar will show the default icon. This usually means rc.exe (Windows SDK) is not discoverable.");
        }
    }
}

/// Expose the short commit hash as `GIEST_GIT_COMMIT` for the About dialog.
///
/// Re-run triggers are the HEAD file *and* the ref it points at, resolved via
/// `rev-parse --git-path` (which also works in a worktree, where `.git` is a
/// file) -- otherwise the icon-only triggers above would freeze the hash at
/// whatever it was on the first build. No git, or not a checkout: "unknown".
fn git_commit() {
    let run = |args: &[&str]| {
        std::process::Command::new("git")
            .args(args)
            .output()
            .ok()
            .filter(|o| o.status.success())
            .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
            .filter(|s| !s.is_empty())
    };
    let commit = run(&["rev-parse", "--short=10", "HEAD"]).unwrap_or_else(|| "unknown".into());
    println!("cargo:rustc-env=GIEST_GIT_COMMIT={commit}");
    if let Some(head) = run(&["rev-parse", "--git-path", "HEAD"]) {
        println!("cargo:rerun-if-changed={head}");
    }
    if let Some(r) = run(&["symbolic-ref", "-q", "HEAD"])
        && let Some(p) = run(&["rev-parse", "--git-path", &r])
    {
        println!("cargo:rerun-if-changed={p}");
    }
}
