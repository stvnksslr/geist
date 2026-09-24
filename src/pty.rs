//! ConPTY-backed PTY via `portable-pty`: spawn a shell, stream its output on a
//! background thread, and write input/responses back to it.
//!
//! A second backend, [`Pty::from_handoff`], drives a pseudoconsole geist did
//! **not** create: one handed over by OpenConsole when geist is the Windows
//! default terminal (`handoff.rs`). It has pipes, a signal pipe for resize and
//! the client's process handle for exit, but no `HPCON` and no child we spawned.

use std::io::Write;
use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{Receiver, Sender, channel};
use std::thread;
use std::time::Instant;

use anyhow::{Context, Result};
use portable_pty::{Child, CommandBuilder, MasterPty, PtySize, native_pty_system};

enum Backend {
    /// A shell geist spawned into a ConPTY it owns.
    Spawned {
        master: Box<dyn MasterPty + Send>,
        /// The shell process; polled via [`Pty::is_running`] to detect exit.
        child: Box<dyn Child + Send + Sync>,
    },
    /// A console session handed to us (default-terminal handoff).
    Handoff {
        /// Resize messages go here; dropping it hangs the session up.
        signal: std::fs::File,
        client: std::os::windows::io::OwnedHandle,
        /// Held so the console session stays referenced while we show it.
        _reference: std::os::windows::io::OwnedHandle,
        _server: std::os::windows::io::OwnedHandle,
    },
}

/// A spawned shell attached to a PTY. Output bytes arrive on `output`; input is
/// written via [`Pty::write`]. Dropping closes the PTY and ends the shell.
pub struct Pty {
    backend: Backend,
    writer: Box<dyn Write + Send>,
    /// Bytes read from the shell. The sender lives on the reader thread, which
    /// exits (closing this channel) when the shell closes its output.
    pub output: Receiver<Vec<u8>>,
    /// Wake-coalescing flag shared with the reader thread; see `spawn_reader`.
    /// Clear it (`Ordering::AcqRel`) *before* draining `output`.
    pub wake_pending: Arc<AtomicBool>,
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

        let reader = pair.master.try_clone_reader().context("clone reader")?;
        let writer = pair.master.take_writer().context("take writer")?;
        let wake_pending = Arc::new(AtomicBool::new(false));
        let rx = spawn_reader(reader, wake_pending.clone(), wake)?;

        Ok(Self {
            backend: Backend::Spawned {
                master: pair.master,
                child,
            },
            writer,
            output: rx,
            wake_pending,
            spawned: Instant::now(),
        })
    }

    /// Drive a pseudoconsole handed over by OpenConsole (default-terminal
    /// handoff). It is sized to `cols`x`rows` straight away, as Windows
    /// Terminal does on the first layout of a handed-off connection.
    pub fn from_handoff<W: Fn() + Send + 'static>(
        a: crate::handoff::Attached,
        cols: u16,
        rows: u16,
        wake: W,
    ) -> Result<Self> {
        let reader = std::fs::File::from(a.output);
        let writer = std::fs::File::from(a.input);
        let wake_pending = Arc::new(AtomicBool::new(false));
        let rx = spawn_reader(Box::new(reader), wake_pending.clone(), wake)?;
        let pty = Self {
            backend: Backend::Handoff {
                signal: std::fs::File::from(a.signal),
                client: a.client,
                _reference: a.reference,
                _server: a.server,
            },
            writer: Box::new(writer),
            output: rx,
            wake_pending,
            spawned: Instant::now(),
        };
        pty.resize(cols, rows)?;
        Ok(pty)
    }

    /// How the shell ended, or `None` while it is still running (or its status
    /// can't be read). The runtime comes from the process's own creation and
    /// exit times rather than from when we *noticed* — an idle app only polls
    /// every 500 ms, which would make every fast failure look slow and defeat
    /// `abnormal-command-exit-runtime`. Falls back to wall time since spawn.
    pub fn exit_info(&mut self) -> Option<ExitInfo> {
        use std::os::windows::io::AsRawHandle;
        let (code, handle) = match &mut self.backend {
            Backend::Spawned { child, .. } => {
                let status = child.try_wait().ok()??;
                (status.exit_code(), child.as_raw_handle())
            }
            Backend::Handoff { client, .. } => (
                crate::handoff::process_exit(client)?,
                Some(client.as_raw_handle()),
            ),
        };
        let runtime_ms = handle
            .and_then(process_runtime_ms)
            .unwrap_or_else(|| self.spawned.elapsed().as_millis() as u64);
        Some(ExitInfo { code, runtime_ms })
    }

    /// Whether the shell process is still running. On Windows ConPTY the output
    /// pipe often doesn't reach EOF when the child exits, so we poll the child
    /// directly rather than relying on the reader thread seeing EOF. For a
    /// handed-off session that is the client program OpenConsole reported.
    pub fn is_running(&mut self) -> bool {
        match &mut self.backend {
            Backend::Spawned { child, .. } => matches!(child.try_wait(), Ok(None)),
            Backend::Handoff { client, .. } => crate::handoff::is_process_running(client),
        }
    }

    /// Write bytes to the shell's input.
    pub fn write(&mut self, bytes: &[u8]) -> Result<()> {
        self.writer.write_all(bytes)?;
        self.writer.flush()?;
        Ok(())
    }

    /// Resize the PTY window. Call alongside the engine's resize.
    pub fn resize(&self, cols: u16, rows: u16) -> Result<()> {
        match &self.backend {
            Backend::Spawned { master, .. } => master
                .resize(PtySize {
                    rows,
                    cols,
                    pixel_width: 0,
                    pixel_height: 0,
                })
                .context("pty resize")?,
            // Not `ResizePseudoConsole`: there is no HPCON here. The signal
            // pipe message is what that call writes under the hood.
            Backend::Handoff { signal, .. } => (&*signal)
                .write_all(&crate::handoff::resize_message(cols, rows))
                .context("handoff resize")?,
        }
        Ok(())
    }
}

/// Stream `reader` to a channel on its own thread, waking the UI when a chunk
/// lands and no wake is already pending.
///
/// `pending` is the coalescing flag: the reader sets it after every send and
/// only calls `wake` on the false→true edge; the UI clears it **before**
/// draining (`Session::pump_pty`), so a chunk sent after the clear always
/// gets its own wake and none can be stranded. Under a flood this turns
/// thousands of cross-thread event posts per second into one per UI pass.
///
/// Measured (`scripts/perf-vs.ps1 -Only geist`): **no throughput change** —
/// 11.9 → 12.4 MiB/s, within noise. The write-side rate is bound by the
/// console host, not this loop (inbox conhost ≈ 13 MiB/s, sideloaded
/// OpenConsole ≈ 84 MiB/s; see docs/benchmarking.md). Kept because it stops
/// a flood from posting an event per 8 KiB to the UI loop, which is cheap
/// insurance rather than a speed-up.
fn spawn_reader<W: Fn() + Send + 'static>(
    mut reader: Box<dyn std::io::Read + Send>,
    pending: Arc<AtomicBool>,
    wake: W,
) -> Result<Receiver<Vec<u8>>> {
    let (tx, rx): (Sender<Vec<u8>>, Receiver<Vec<u8>>) = channel();
    thread::Builder::new()
        .name("pty-reader".into())
        .spawn(move || {
            // ConPTY hands over output in bursts far larger than 8 KiB under
            // load; a bigger buffer means fewer reads, allocations and sends
            // per megabyte. Idle reads still return whatever is available.
            let mut buf = vec![0u8; 64 * 1024];
            loop {
                match reader.read(&mut buf) {
                    Ok(0) => break,
                    Ok(n) => {
                        if tx.send(buf[..n].to_vec()).is_err() {
                            break;
                        }
                        if !pending.swap(true, Ordering::AcqRel) {
                            wake();
                        }
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
    Ok(rx)
}
