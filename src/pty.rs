//! ConPTY-backed PTY via `portable-pty`: spawn a shell, stream its output on a
//! background thread, and write input/responses back to it.

use std::io::Write;
use std::path::Path;
use std::sync::mpsc::{Receiver, Sender, channel};
use std::thread;

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
        if let Some(cwd) = cwd {
            cmd.cwd(cwd);
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
