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
use std::sync::mpsc::{RecvTimeoutError, TryRecvError};
use std::sync::{Condvar, Mutex};
use std::thread;
use std::time::{Duration, Instant};

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
    /// Bytes read from the shell. The reader thread appends to it, and marks
    /// it closed when the shell closes its output.
    pub output: Output,
    /// Wake-coalescing flag shared with the reader thread; see `spawn_reader`.
    /// Clear it (`Ordering::AcqRel`) *before* draining `output`.
    pub wake_pending: Arc<AtomicBool>,
    /// Set while no window is drawing this pane (a background tab, a hidden
    /// split, the prewarmed spare): small output then waits for the app's next
    /// pass instead of waking it. See `spawn_reader` and
    /// `Session::mark_on_screen`.
    pub background: Arc<AtomicBool>,
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
        let background = Arc::new(AtomicBool::new(false));
        let rx = spawn_reader(reader, wake_pending.clone(), background.clone(), wake)?;

        Ok(Self {
            backend: Backend::Spawned {
                master: pair.master,
                child,
            },
            writer,
            output: rx,
            wake_pending,
            background,
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
        let background = Arc::new(AtomicBool::new(false));
        let rx = spawn_reader(
            Box::new(reader),
            wake_pending.clone(),
            background.clone(),
            wake,
        )?;
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
            background,
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

/// Bytes the reader may hold ahead of the UI before it blocks. Everything
/// queued is output the screen hasn't shown yet, so depth is latency, and
/// latency is a matter of bytes, not of how many reads delivered them (it was
/// 8 reads of anything from a few bytes to 64 KiB). 1 MiB is a few ms of
/// parsing at the engine's measured rate. Measured on its own, the switch from
/// a read count changed no throughput (docs/benchmarking.md); what it buys is
/// a bound that means the same thing for every console host, and no heap
/// allocation per read.
pub const OUTPUT_QUEUE_BYTES: usize = 1 << 20;

/// How much output a background pane (see [`Pty::background`]) may queue
/// before it wakes the app anyway. Below it, the output waits for the app's
/// next pass; at it, a program writing steadily is pumped well before it
/// blocks on [`OUTPUT_QUEUE_BYTES`], so throughput doesn't suffer.
pub const BACKGROUND_WAKE_BYTES: usize = 256 << 10;

/// The queue between the reader thread and the UI: one growable byte buffer
/// rather than a channel of per-read `Vec`s, so a burst of small reads costs
/// one append each (no allocation) and the UI takes the lot in one swap.
#[derive(Default)]
struct OutputState {
    buf: Vec<u8>,
    /// The reader hit EOF or an error; nothing more will arrive.
    closed: bool,
    /// The [`Output`] was dropped; the reader should stop.
    abandoned: bool,
}

#[derive(Default)]
struct OutputShared {
    state: Mutex<OutputState>,
    /// Signalled when the UI takes the buffer (room for the reader) and when
    /// the reader appends or closes (data for a blocked `recv_timeout`).
    changed: Condvar,
}

/// Bytes read from the shell, queued by the reader thread and bounded at
/// [`OUTPUT_QUEUE_BYTES`]: a program that writes faster than the engine parses
/// blocks in its own write — the backpressure Ghostty gets by parsing on its
/// IO thread.
pub struct Output {
    shared: Arc<OutputShared>,
}

impl Output {
    fn lock(&self) -> std::sync::MutexGuard<'_, OutputState> {
        self.shared.state.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Move everything queued into `into`, replacing its contents; its old
    /// allocation goes to the reader for reuse. `Err(Empty)` when nothing is
    /// queued yet, `Err(Disconnected)` once the shell closed its output and
    /// the queue is drained.
    pub fn take_into(&self, into: &mut Vec<u8>) -> Result<(), TryRecvError> {
        let mut st = self.lock();
        if st.buf.is_empty() {
            return Err(if st.closed {
                TryRecvError::Disconnected
            } else {
                TryRecvError::Empty
            });
        }
        into.clear();
        std::mem::swap(into, &mut st.buf);
        drop(st);
        self.shared.changed.notify_all();
        Ok(())
    }

    /// Everything queued, as a fresh `Vec` — `mpsc::Receiver::try_recv`'s
    /// shape, for tests and harnesses that drive a `Pty` directly.
    pub fn try_recv(&self) -> Result<Vec<u8>, TryRecvError> {
        let mut out = Vec::new();
        self.take_into(&mut out).map(|()| out)
    }

    /// Like [`Output::try_recv`], but waits up to `timeout` for output.
    pub fn recv_timeout(&self, timeout: Duration) -> Result<Vec<u8>, RecvTimeoutError> {
        let deadline = Instant::now() + timeout;
        let mut st = self.lock();
        loop {
            if !st.buf.is_empty() {
                let out = std::mem::take(&mut st.buf);
                drop(st);
                self.shared.changed.notify_all();
                return Ok(out);
            }
            if st.closed {
                return Err(RecvTimeoutError::Disconnected);
            }
            let now = Instant::now();
            if now >= deadline {
                return Err(RecvTimeoutError::Timeout);
            }
            st = self
                .shared
                .changed
                .wait_timeout(st, deadline - now)
                .unwrap_or_else(|e| e.into_inner())
                .0;
        }
    }
}

impl Drop for Output {
    fn drop(&mut self) {
        self.lock().abandoned = true;
        // Unblocks a reader waiting for room, which then exits.
        self.shared.changed.notify_all();
    }
}

/// Stream `reader` into an [`Output`] on its own thread, waking the UI when
/// bytes land and no wake is already pending.
///
/// `pending` is the coalescing flag: the reader sets it after every append and
/// only calls `wake` on the false→true edge; the UI clears it **before**
/// draining (`Session::pump_pty`), so bytes appended after the clear always
/// get their own wake and none can be stranded. Under a flood this turns
/// thousands of cross-thread event posts per second into one per UI pass.
///
/// `background` (see [`Pty::background`]) holds back the wake for a pane no
/// window is drawing until [`BACKGROUND_WAKE_BYTES`] have queued. Every wake
/// is a pass, and every pass is a present: a Claude Code spinner in a
/// background tab used to present the *foreground* tab ~8 times a second at
/// the spinner's irregular rhythm. A variable-refresh display's rate follows
/// geist's presents (measured on a VRR OLED with geist in front: swinging
/// between 48 and 144 Hz several times a second), and an OLED shows those
/// swings as brightness flicker. Held-back output is pumped on the
/// app's next pass for any reason — at worst the half-second idle wake —
/// because every root pass pumps every pane. `pending` is left alone while
/// holding back, so the first chunk past the threshold, or after the pane is
/// drawn again, still finds the false→true edge and wakes.
///
/// The queue is **bounded** ([`OUTPUT_QUEUE_BYTES`]) so a program that writes
/// faster than the engine parses gets backpressure: the reader blocks,
/// ConPTY's pipe fills, and the program's own write blocks. Unbounded, the
/// queue grew for as long as the program outpaced the engine (a fullscreen TUI
/// redrawing an ever-larger frame, e.g. Claude Code late in a session): the
/// screen fell seconds behind the program, keystroke echo queued behind the
/// backlog, and `pump_pty` could not return while the reader kept refilling it.
fn spawn_reader<W: Fn() + Send + 'static>(
    mut reader: Box<dyn std::io::Read + Send>,
    pending: Arc<AtomicBool>,
    background: Arc<AtomicBool>,
    wake: W,
) -> Result<Output> {
    let shared = Arc::new(OutputShared::default());
    let ours = shared.clone();
    thread::Builder::new()
        .name("pty-reader".into())
        .spawn(move || {
            // ConPTY hands over output in bursts far larger than 8 KiB under
            // load; a bigger buffer means fewer reads per megabyte. Idle reads
            // still return whatever is available.
            let mut buf = vec![0u8; 64 * 1024];
            loop {
                match reader.read(&mut buf) {
                    Ok(0) => break,
                    Ok(n) => {
                        let mut st = ours.state.lock().unwrap_or_else(|e| e.into_inner());
                        while st.buf.len() >= OUTPUT_QUEUE_BYTES && !st.abandoned {
                            st = ours.changed.wait(st).unwrap_or_else(|e| e.into_inner());
                        }
                        if st.abandoned {
                            return;
                        }
                        st.buf.extend_from_slice(&buf[..n]);
                        let queued = st.buf.len();
                        drop(st);
                        ours.changed.notify_all();
                        let urgent =
                            queued >= BACKGROUND_WAKE_BYTES || !background.load(Ordering::Acquire);
                        if urgent && !pending.swap(true, Ordering::AcqRel) {
                            wake();
                        }
                    }
                    Err(_) => break,
                }
            }
            // The shell closed its output (exited): mark the queue closed and
            // wake the UI, which reaps this pane once it has drained the rest.
            ours.state.lock().unwrap_or_else(|e| e.into_inner()).closed = true;
            ours.changed.notify_all();
            wake();
        })
        .context("spawn pty reader thread")?;
    Ok(Output { shared })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A program that writes faster than the UI drains must be held back by the
    /// reader, not queued without bound — the unbounded queue is what left the
    /// screen seconds behind a fullscreen TUI late in a long session.
    #[test]
    fn reader_queue_is_bounded_under_a_flood() {
        let flood = Box::new(std::io::repeat(b'x'));
        let pending = Arc::new(AtomicBool::new(false));
        let background = Arc::new(AtomicBool::new(false));
        let out = spawn_reader(flood, pending, background, || {}).unwrap();
        // Give the reader ample time to run ahead of a UI that never drains.
        std::thread::sleep(std::time::Duration::from_millis(200));
        let queued = out.try_recv().unwrap().len();
        // The cap, plus at most the one 64 KiB read that crossed it.
        assert!(
            queued <= OUTPUT_QUEUE_BYTES + 64 * 1024,
            "{queued} bytes queued, cap {OUTPUT_QUEUE_BYTES}"
        );
        // Taking the queue made room, so the reader carries on.
        assert!(out.recv_timeout(Duration::from_secs(5)).is_ok());
        // Dropping the receiver unblocks and ends the reader thread.
        drop(out);
    }

    /// A pane nobody is drawing doesn't wake the app for every chunk — only
    /// once enough output has piled up — and wakes for each chunk again as
    /// soon as it is drawn.
    #[test]
    fn a_background_pane_holds_its_wake_until_output_piles_up() {
        use std::sync::atomic::AtomicUsize;
        struct Feed(std::sync::mpsc::Receiver<Vec<u8>>, Vec<u8>);
        impl std::io::Read for Feed {
            fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
                if self.1.is_empty() {
                    match self.0.recv() {
                        Ok(v) => self.1 = v,
                        Err(_) => return Ok(0),
                    }
                }
                let n = self.1.len().min(buf.len());
                buf[..n].copy_from_slice(&self.1[..n]);
                self.1.drain(..n);
                Ok(n)
            }
        }
        let (tx, rx) = std::sync::mpsc::channel::<Vec<u8>>();
        let wakes = Arc::new(AtomicUsize::new(0));
        let counted = wakes.clone();
        let pending = Arc::new(AtomicBool::new(false));
        let background = Arc::new(AtomicBool::new(true));
        let out = spawn_reader(
            Box::new(Feed(rx, Vec::new())),
            pending.clone(),
            background.clone(),
            move || {
                counted.fetch_add(1, Ordering::SeqCst);
            },
        )
        .unwrap();
        let settle = || std::thread::sleep(Duration::from_millis(100));

        tx.send(vec![b'a'; 100]).unwrap();
        settle();
        assert_eq!(
            wakes.load(Ordering::SeqCst),
            0,
            "a small background chunk woke the app"
        );
        tx.send(vec![b'b'; BACKGROUND_WAKE_BYTES]).unwrap();
        settle();
        assert_eq!(
            wakes.load(Ordering::SeqCst),
            1,
            "output piling up past the threshold must wake"
        );

        // The app drains (clearing `pending` first), and draws the pane again.
        pending.store(false, Ordering::Release);
        assert!(out.try_recv().is_ok());
        background.store(false, Ordering::Release);
        tx.send(vec![b'c'; 10]).unwrap();
        settle();
        assert_eq!(
            wakes.load(Ordering::SeqCst),
            2,
            "a drawn pane wakes at once"
        );
    }

    /// Many small reads are coalesced, and none is lost on the way, including
    /// the last ones before EOF.
    #[test]
    fn small_reads_coalesce_and_all_arrive() {
        struct Drip(usize);
        impl std::io::Read for Drip {
            fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
                if self.0 == 0 {
                    return Ok(0);
                }
                self.0 -= 1;
                buf[..100].fill(b'y');
                Ok(100)
            }
        }
        let pending = Arc::new(AtomicBool::new(false));
        let background = Arc::new(AtomicBool::new(false));
        let out = spawn_reader(Box::new(Drip(1000)), pending, background, || {}).unwrap();
        let mut total = 0;
        loop {
            match out.recv_timeout(Duration::from_secs(5)) {
                Ok(b) => total += b.len(),
                Err(RecvTimeoutError::Disconnected) => break,
                Err(RecvTimeoutError::Timeout) => panic!("reader stalled at {total} bytes"),
            }
        }
        assert_eq!(total, 100_000);
    }
}
