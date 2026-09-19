//! ConPTY-backed PTY via `portable-pty`: spawn a shell, stream its output on a
//! background thread, and write input/responses back to it.

use std::io::Write;
use std::path::Path;
use std::sync::mpsc::{Receiver, Sender, channel};
use std::thread;
use std::time::Instant;

use anyhow::{Context, Result};
use portable_pty::{Child, CommandBuilder, MasterPty, PtySize, native_pty_system};

/// A spawned shell attached to a PTY. Output bytes arrive on `output`; input is
/// written via [`Pty::write`]. Dropping closes the PTY and ends the shell.
pub struct Pty {
    master: Box<dyn MasterPty + Send>,
    writer: Box<dyn Write + Send>,
    /// The shell process; polled via [`Pty::is_running`] to detect exit.
    child: Box<dyn Child + Send + Sync>,
    /// Bytes read from the shell. The sender lives on the reader thread, which
    /// exits (closing this channel) when the shell closes its output.
    pub output: Receiver<Vec<u8>>,
    /// When the shell was spawned; the fallback clock for [`Pty::exit_info`].
    spawned: Instant,
}

/// How a shell exited: its exit code and how long it ran.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ExitInfo {
    pub code: u32,
    pub runtime_ms: u64,
}

#[repr(C)]
#[derive(Default)]
struct FileTime {
    low: u32,
    high: u32,
}

// kernel32 is always linked; declared directly rather than enabling a
// windows-sys feature module for one function (same call as `bell.rs`).
#[link(name = "kernel32")]
unsafe extern "system" {
    fn GetProcessTimes(
        process: *mut std::ffi::c_void,
        creation: *mut FileTime,
        exit: *mut FileTime,
        kernel: *mut FileTime,
        user: *mut FileTime,
    ) -> i32;
}

/// Exited process `handle`'s runtime (exit time − creation time), in ms.
fn process_runtime_ms(handle: std::os::windows::io::RawHandle) -> Option<u64> {
    let (mut c, mut e, mut k, mut u) = Default::default();
    // SAFETY: `handle` is the child's live process handle, owned by
    // portable-pty for as long as `Pty` exists; the out-params are locals.
    let ok = unsafe { GetProcessTimes(handle as _, &mut c, &mut e, &mut k, &mut u) };
    if ok == 0 {
        return None;
    }
    let t = |f: &FileTime| ((f.high as u64) << 32) | f.low as u64;
    // FILETIME ticks are 100 ns.
    t(&e).checked_sub(t(&c)).map(|d| d / 10_000)
}

impl Pty {
    /// Open a PTY of `cols`x`rows` and spawn `program` with `args` (e.g.
    /// `pwsh.exe`, `wsl.exe`) in `cwd` (the process default when `None`). `wake`
    /// is invoked on the reader thread whenever new output arrives, so the UI can
    /// schedule a repaint.
    pub fn spawn<W: Fn() + Send + 'static>(
        program: &str,
        args: &[String],
        cwd: Option<&Path>,
        // Extra environment (config `env`), layered over the inherited one.
        env: &[(String, String)],
        cols: u16,
        rows: u16,
        wake: W,
    ) -> Result<Self> {
        let pty_system = native_pty_system();
        let pair = pty_system
            .openpty(PtySize {
                rows,
                cols,
                pixel_width: 0,
                pixel_height: 0,
            })
            .context("openpty failed")?;

        let mut cmd = CommandBuilder::new(program);
        for arg in args {
            cmd.arg(arg);
        }
        // Only a directory that exists *on Windows*: a pane inside WSL reports
        // Linux paths over OSC 7 (`/home/me`), which CreateProcess cannot start
        // in — better the default dir than a failed split.
        if let Some(cwd) = cwd.filter(|p| p.is_dir()) {
            cmd.cwd(cwd);
        }
        for (k, v) in env {
            cmd.env(k, v);
        }
        let child = pair
            .slave
            .spawn_command(cmd)
            .with_context(|| format!("failed to spawn shell: {program}"))?;
        // The slave handle must be dropped so the child is the only holder;
        // otherwise the read side never sees EOF when the shell exits.
        drop(pair.slave);

        let mut reader = pair.master.try_clone_reader().context("clone reader")?;
        let writer = pair.master.take_writer().context("take writer")?;

        let (tx, rx): (Sender<Vec<u8>>, Receiver<Vec<u8>>) = channel();
        thread::Builder::new()
            .name("pty-reader".into())
            .spawn(move || {
                let mut buf = [0u8; 8192];
                loop {
                    match std::io::Read::read(&mut reader, &mut buf) {
                        Ok(0) => break,
                        Ok(n) => {
                            if tx.send(buf[..n].to_vec()).is_err() {
                                break;
                            }
                            wake();
                        }
                        Err(_) => break,
                    }
                }
                // The shell closed its output (exited). Dropping `tx` here
                // disconnects the channel; wake the UI so it reaps this pane.
                drop(tx);
                wake();
            })
            .context("spawn pty reader thread")?;

        Ok(Self {
            master: pair.master,
            writer,
            child,
            output: rx,
            spawned: Instant::now(),
        })
    }

    /// How the shell ended, or `None` while it is still running (or its status
    /// can't be read). The runtime comes from the process's own creation and
    /// exit times rather than from when we *noticed* — an idle app only polls
    /// every 500 ms, which would make every fast failure look slow and defeat
    /// `abnormal-command-exit-runtime`. Falls back to wall time since spawn.
    pub fn exit_info(&mut self) -> Option<ExitInfo> {
        let status = self.child.try_wait().ok()??;
        let runtime_ms = self
            .child
            .as_raw_handle()
            .and_then(process_runtime_ms)
            .unwrap_or_else(|| self.spawned.elapsed().as_millis() as u64);
        Some(ExitInfo {
            code: status.exit_code(),
            runtime_ms,
        })
    }

    /// Whether the shell process is still running. On Windows ConPTY the output
    /// pipe often doesn't reach EOF when the child exits, so we poll the child
    /// directly rather than relying on the reader thread seeing EOF.
    pub fn is_running(&mut self) -> bool {
        matches!(self.child.try_wait(), Ok(None))
    }

    /// Write bytes to the shell's input.
    pub fn write(&mut self, bytes: &[u8]) -> Result<()> {
        self.writer.write_all(bytes)?;
        self.writer.flush()?;
        Ok(())
    }

    /// Resize the PTY window. Call alongside the engine's resize.
    pub fn resize(&self, cols: u16, rows: u16) -> Result<()> {
        self.master
            .resize(PtySize {
                rows,
                cols,
                pixel_width: 0,
                pixel_height: 0,
            })
            .context("pty resize")?;
        Ok(())
    }
}
