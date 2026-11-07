//! A PowerShell started ahead of the pane that will show it (`shell-prewarm`).
//!
//! Opening a window, tab or split on pwsh took most of a second, and geist's
//! own part of that is small: measured, the PTY spawn plus the new window's
//! first frame is ~15–45 ms, while the shell needs ~850 ms from spawn to its
//! first prompt — nearly all of it the user's `$PROFILE` (prompt engines, env
//! managers, `Invoke-Expression` of their init scripts). geist can't make that
//! faster, but it can have already *done* it.
//!
//! So one spare shell for the default profile is kept started, held at a gate
//! just before its first prompt (`profiles::PWSH_GATE`, read from a private
//! pipe). [`take`] — reached from the single `open_session` every new pane
//! goes through — adopts it when it is *exactly* the shell `Session::new` would
//! have started (program, args, environment), and releases the gate with the
//! pane's starting directory. Anything else falls through to a normal spawn.
//!
//! The spare lives in a thread-local because a `Session` is `!Send` and the
//! whole UI runs on one thread. It is pumped every root pass, like a pane:
//! ConPTY opens with a cursor-position request and holds the shell's output
//! until the engine answers it, and a reader blocked on a full queue would
//! stall the shell's startup — the very work being done ahead of time.

use std::cell::RefCell;
use std::fs::File;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use eframe::egui;

use crate::config::Config;
use crate::profiles::{Launch, Profile};
use crate::session::Session;

/// After startup, let the first window's own shells start first.
const FIRST_DELAY: Duration = Duration::from_secs(2);
/// After an adoption, let the adopted shell draw its first prompt (which runs
/// the user's prompt engine) before another shell starts competing for CPU.
const REFILL_DELAY: Duration = Duration::from_secs(1);
/// After a spare died on its own: a shell that can't start must not be
/// respawned every second forever.
const FAILED_DELAY: Duration = Duration::from_secs(30);

struct Spare {
    session: Session,
    /// The program and the *ungated* launch it stands in for — what
    /// `Session::new` would start. Compared whole, so a config reload or a
    /// profile edit that changes either retires the spare.
    stands_for: (String, Launch),
    /// The server end of the gate's pipe; written once on adoption.
    gate: File,
}

#[derive(Default)]
struct State {
    spare: Option<Spare>,
    /// When the next spare may start; `None` until the first [`maintain`].
    next_at: Option<Instant>,
    /// Makes each gate's pipe name unique within this process.
    seq: u64,
}

thread_local! {
    static STATE: RefCell<State> = RefCell::default();
}

/// What `Session::new` would start for `profile`: the identity a spare must
/// match to be adopted in its place.
fn identity(config: &Config, profile: &Profile) -> (String, Launch) {
    (
        profile.program.clone(),
        Session::launch_for(config, profile),
    )
}

/// Adopt the spare shell, if it is the one `Session::new(config, profile, cwd)`
/// would start, and release it into `cwd` (resolved exactly as `Pty::spawn`
/// would: a directory that doesn't exist means the process default).
pub fn take(config: &Config, profile: &Profile, cwd: Option<&Path>) -> Option<Session> {
    if !config.shell_prewarm {
        return None;
    }
    STATE.with_borrow_mut(|st| {
        let fits = st
            .spare
            .as_ref()
            .is_some_and(|s| s.session.is_alive() && s.stands_for == identity(config, profile));
        // A mismatch stays put: a `cmd` tab says nothing about whether the
        // next pwsh window can use it. Staleness is `maintain`'s job.
        if !fits {
            return None;
        }
        let mut spare = st.spare.take()?;
        // The spare was never drawn, so its output has been held back from
        // waking the app (`Pty::background`). It is about to be, and the
        // prompt the gate is about to release must show at once.
        spare.session.mark_on_screen();
        let dir = cwd
            .filter(|p| p.is_dir())
            .map(Path::to_path_buf)
            .or_else(|| std::env::current_dir().ok());
        release(spare.gate, dir);
        st.next_at = Some(Instant::now() + REFILL_DELAY);
        Some(spare.session)
    })
}

/// Write the starting directory to the gate, off the UI thread: the shell may
/// still be inside its `$PROFILE` and not connected yet, and the write then
/// waits for it.
fn release(gate: File, dir: Option<PathBuf>) {
    let line = dir
        .map(|d| d.to_string_lossy().into_owned())
        .unwrap_or_default();
    let _ = std::thread::Builder::new()
        .name("geist-prewarm-gate".into())
        .spawn(move || {
            let mut gate = gate;
            if crate::ipc::accept(&gate) {
                let _ = gate.write_all(format!("{line}\n").as_bytes());
                // Wait for the shell to read it before the handle closes, so
                // nothing depends on how a closed pipe treats unread data.
                let _ = gate.sync_all();
            }
        });
}

/// Once per root pass: pump the spare, retire it if it died or no longer
/// matches the default `profile`, and start a new one when one is due.
pub fn maintain(ctx: &egui::Context, config: &Config, profile: &Profile) {
    STATE.with_borrow_mut(|st| {
        let now = Instant::now();
        if let Some(s) = st.spare.as_mut()
            && s.session.pump_pty()
        {
            ctx.request_repaint();
        }
        if let Some(s) = st.spare.as_ref() {
            if !s.session.is_alive() {
                st.spare = None;
                st.next_at = Some(now + FAILED_DELAY);
            } else if !config.shell_prewarm || s.stands_for != identity(config, profile) {
                st.spare = None;
            }
        }
        if st.spare.is_some() || !config.shell_prewarm {
            return;
        }
        let at = *st.next_at.get_or_insert(now + FIRST_DELAY);
        if now < at {
            ctx.request_repaint_after(at - now);
            return;
        }
        st.seq += 1;
        st.spare = start(ctx, config, profile, st.seq);
        if st.spare.is_none() {
            // Not a pwsh profile, or the spawn failed: don't retry every pass.
            st.next_at = Some(now + FAILED_DELAY);
        }
    });
}

/// Start a gated spare for `profile`, or `None` if it isn't a shell that has a
/// gate (see `Profile::launch_gated`) or anything fails.
fn start(ctx: &egui::Context, config: &Config, profile: &Profile, seq: u64) -> Option<Spare> {
    let name = format!("geist-prewarm-{}-{seq}", std::process::id());
    let gated = profile.launch_gated(
        &crate::profiles::Integration::from_config(config),
        crate::profiles::shell_integration_dir().as_deref(),
        &config.env,
        &name,
    )?;
    // The pipe must exist before the shell reaches the gate: a client
    // `Connect` to a missing pipe spins rather than waits.
    let gate = crate::ipc::private_pipe(&format!(r"\\.\pipe\{name}")).ok()?;
    let stands_for = identity(config, profile);
    let launch = Launch {
        args: gated.args,
        ..stands_for.1.clone()
    };
    let session = Session::spawn(ctx, config, profile, None, &launch).ok()?;
    Some(Spare {
        session,
        stands_for,
        gate,
    })
}

/// Drop the spare (its shell exits with its pseudoconsole). For a config
/// reload — the spare's engine was themed by the old config — and shutdown.
pub fn discard() {
    STATE.with_borrow_mut(|st| st.spare = None);
}
