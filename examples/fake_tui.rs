//! A stand-in TUI that redraws the way Claude Code does, for measuring what a
//! terminal *presents* while a program animates (`tests/sync_frames.rs`).
//!
//! Inline on the main screen, like Ink: each frame erases the previous block
//! line by line from the bottom up (`ESC[2K`, `ESC[1A`), then repaints it, and
//! the whole frame goes out in **one** `WriteFile` — optionally wrapped in a
//! synchronized update (DEC 2026), as Claude Code does once it has detected
//! support. Every line names its frame and row (`F000042 L007`), so a torn
//! frame (rows from two frames, or rows still erased) is visible in a grid
//! snapshot without looking at pixels.
//!
//! ```text
//! fake_tui <lines> <width> <interval_ms> <frames> <sync:0|1> [sgr_every]
//! ```
//!
//! `sgr_every` puts a colour change every N columns, to give frames the byte
//! weight of real TUI output (Claude Code's are dense with SGR). Default 8.

use std::time::{Duration, Instant};

#[cfg(windows)]
mod con {
    #[link(name = "kernel32")]
    unsafe extern "system" {
        pub fn GetStdHandle(n: u32) -> isize;
        pub fn GetConsoleMode(h: isize, m: *mut u32) -> i32;
        pub fn SetConsoleMode(h: isize, m: u32) -> i32;
        pub fn WriteFile(
            h: isize,
            buf: *const u8,
            n: u32,
            written: *mut u32,
            ov: *mut core::ffi::c_void,
        ) -> i32;
    }
    /// `STD_OUTPUT_HANDLE`, `(DWORD)-11`.
    pub const STD_OUTPUT: u32 = 0xFFFF_FFF5;
    pub const ENABLE_VIRTUAL_TERMINAL_PROCESSING: u32 = 0x4;
}

/// One `WriteFile` per frame, as a program that builds its frame in memory
/// and hands it to the console in one call does (looping only on a short
/// write). `std::io::stdout` is line-buffered and would split a frame at
/// every newline, which is not what is being imitated.
#[cfg(windows)]
fn write_frame(h: isize, mut bytes: &[u8]) {
    while !bytes.is_empty() {
        let mut n = 0u32;
        let ok = unsafe {
            con::WriteFile(
                h,
                bytes.as_ptr(),
                bytes.len() as u32,
                &mut n,
                std::ptr::null_mut(),
            )
        };
        if ok == 0 || n == 0 {
            std::process::exit(3);
        }
        bytes = &bytes[n as usize..];
    }
}

fn line(frame: u32, row: u32, width: usize, sgr_every: usize) -> String {
    let mut s = format!("F{frame:06} L{row:03} ");
    let filler = b"abcdefghijklmnopqrstuvwxyz0123456789";
    let mut col = s.len();
    let mut i = (frame as usize + row as usize) % filler.len();
    while col < width.saturating_sub(1) {
        if sgr_every > 0 && col % sgr_every == 0 {
            s.push_str(&format!("\x1b[38;5;{}m", 16 + (col + row as usize) % 216));
        }
        s.push(filler[i % filler.len()] as char);
        i += 1;
        col += 1;
    }
    s.push_str("\x1b[0m");
    s
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let arg = |i: usize, d: u64| args.get(i).and_then(|a| a.parse().ok()).unwrap_or(d);
    let lines = arg(0, 10) as u32;
    let width = arg(1, 100) as usize;
    let interval = Duration::from_millis(arg(2, 16));
    let frames = arg(3, 200) as u32;
    let sync = arg(4, 1) == 1;
    let sgr_every = arg(5, 8) as usize;

    #[cfg(windows)]
    let h = unsafe {
        let h = con::GetStdHandle(con::STD_OUTPUT);
        let mut mode = 0u32;
        con::GetConsoleMode(h, &mut mode);
        con::SetConsoleMode(h, mode | con::ENABLE_VIRTUAL_TERMINAL_PROCESSING);
        h
    };

    // Let the harness size the grid (and ConPTY settle) before the first frame.
    std::thread::sleep(Duration::from_millis(arg(6, 700)));
    #[cfg(windows)]
    write_frame(h, b"\x1b[?25l");

    let started = Instant::now();
    for f in 0..frames {
        let mut out = String::new();
        if sync {
            out.push_str("\x1b[?2026h");
        }
        if f > 0 {
            // Ink's `eraseLines(lines)`: clear the last line, then up and clear
            // each line above it, ending at column 1 of the block's first row.
            for i in 0..lines {
                out.push_str("\x1b[2K");
                if i + 1 < lines {
                    out.push_str("\x1b[1A");
                }
            }
            out.push_str("\x1b[G");
        }
        for r in 0..lines {
            out.push_str(&line(f, r, width, sgr_every));
            if r + 1 < lines {
                out.push_str("\r\n");
            }
        }
        if sync {
            out.push_str("\x1b[?2026l");
        }
        #[cfg(windows)]
        write_frame(h, out.as_bytes());
        // Pace frames on a fixed schedule (an animation timer), not "interval
        // after the write returned".
        let due = started + interval * (f + 1);
        if let Some(wait) = due.checked_duration_since(Instant::now()) {
            std::thread::sleep(wait);
        }
    }
    #[cfg(windows)]
    {
        write_frame(h, b"\r\n\x1b[?25hFAKE-TUI-DONE\r\n");
    }
    std::thread::sleep(Duration::from_millis(500));
}
