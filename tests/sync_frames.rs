//! What does a pane actually *present* while a TUI animates? (host integration)
//!
//! Flicker is a property of the frames a terminal presents and of *when* it
//! presents them, not of any one function. So this drives the real
//! [`Session`] — real ConPTY, real engine, the real synchronized-output hold —
//! with `examples/fake_tui.rs` redrawing the way Claude Code does, and paces it
//! the way eframe does: a pass runs when the PTY reader wakes the context
//! (through egui's own repaint bookkeeping, so its follow-up passes happen
//! here exactly as in the app), and no pass starts until the previous present
//! has been scanned out (FIFO, one frame in flight — `main.rs`). After every
//! pass it records what the renderer would draw and when:
//!
//! - **torn**: rows from more than one frame, or rows the frame erased but had
//!   not repainted yet — what a viewer sees as flicker;
//! - **stall**: the longest stretch the shown frame did not change while the
//!   program was producing new ones — what a viewer sees as a freeze;
//! - **presents** and their **intervals**: a variable-refresh display refreshes
//!   when a frame is presented, so irregular intervals swing its refresh rate,
//!   which an OLED shows as brightness flicker even when every frame is whole.
//!
//! `#[ignore]`d: it spawns processes behind a real ConPTY. Sideloaded ConPTY
//! (`geist_TEST_PASSTHROUGH=1`) is the configuration that matters: only it
//! keeps DEC 2026 around the frame (see `conpty_passthrough.rs`). Run it in
//! release — a debug build parses libghostty in Zig Debug, which is slow
//! enough to change what is measured:
//!
//! ```text
//! cargo build --release --example fake_tui
//! geist_TEST_PASSTHROUGH=1 cargo test --release --test sync_frames -- --ignored --nocapture --test-threads=1
//! ```

use std::collections::BTreeSet;
use std::path::PathBuf;
use std::sync::mpsc;
use std::time::{Duration, Instant};

use eframe::egui;
use geist::config::Config;
use geist::engine::GridSnapshot;
use geist::profiles::Profile;
use geist::session::Session;

fn requested_passthrough() -> bool {
    std::env::var("geist_TEST_PASSTHROUGH").is_ok_and(|v| v == "1")
}

fn select_mode() {
    let on = requested_passthrough();
    portable_pty::set_allow_sideload(on);
    portable_pty::set_passthrough(on);
}

fn fake_tui() -> PathBuf {
    // target/<profile>/deps/sync_frames-<hash>.exe -> target/<profile>/examples
    let exe = std::env::current_exe().unwrap();
    let dir = exe.parent().unwrap().parent().unwrap().join("examples");
    let p = dir.join("fake_tui.exe");
    assert!(
        p.is_file(),
        "{} missing: run `cargo build --example fake_tui` (same profile) first",
        p.display()
    );
    p
}

/// The TUI's shape and pace.
#[derive(Clone, Copy, Debug)]
struct Load {
    lines: u32,
    width: u32,
    interval_ms: u32,
    frames: u32,
    sync: bool,
    sgr_every: u32,
}

/// How the harness paces its passes.
#[derive(Clone, Copy, Debug)]
struct Pacing {
    /// Display refresh rate: a pass can't start until the last present is out.
    hz: f64,
    /// Mirror `window-steady-refresh`: keep repainting while the pane changed
    /// within this long (`app.rs` `STEADY_REFRESH_LINGER`).
    steady: Option<Duration>,
}

#[derive(Debug, Default)]
struct Stats {
    passes: u32,
    /// Passes that showed a complete, consistent frame.
    whole: u32,
    /// Passes that showed a torn frame.
    torn: u32,
    /// Passes during which a synchronized update was holding the frame.
    held: u32,
    /// Distinct frames that reached the screen.
    shown: usize,
    /// Longest the shown frame went unchanged while new ones were due.
    longest_stall: Duration,
    /// Time between consecutive presents while the program was drawing.
    intervals: Vec<Duration>,
    /// A sample of torn passes, for the report.
    examples: Vec<String>,
}

impl Stats {
    /// Present intervals more than one refresh away from their neighbour: each
    /// is a change of refresh rate on a variable-refresh display.
    fn rate_swings(&self, period: Duration) -> usize {
        self.intervals
            .windows(2)
            .filter(|w| w[0].abs_diff(w[1]) > period)
            .count()
    }
}

/// Row `r` of the snapshot as text.
fn row_text(s: &GridSnapshot, r: usize) -> String {
    let cols = s.cols as usize;
    s.cells[r * cols..(r + 1) * cols]
        .iter()
        .map(|c| {
            if c.text.is_empty() {
                " "
            } else {
                c.text.as_str()
            }
        })
        .collect()
}

/// `(frame, line)` tags of the rows that carry one.
fn tags(s: &GridSnapshot) -> Vec<(u32, u32)> {
    (0..s.rows as usize)
        .filter_map(|r| {
            let t = row_text(s, r);
            let t = t.trim_start();
            let f = t.strip_prefix('F')?.get(..6)?.parse().ok()?;
            let l = t.get(8..)?.strip_prefix('L')?.get(..3)?.parse().ok()?;
            Some((f, l))
        })
        .collect()
}

fn run(load: Load, pacing: Pacing) -> Stats {
    select_mode();
    let ctx = egui::Context::default();
    let (tx, rx) = mpsc::channel::<Instant>();
    ctx.set_request_repaint_callback(move |info| {
        let _ = tx.send(Instant::now() + info.delay);
    });

    let cfg = Config {
        shell_prewarm: false,
        ..Config::default()
    };
    let mut profile = Profile::new("fake-tui", &fake_tui().to_string_lossy());
    profile.args = [
        load.lines,
        load.width,
        load.interval_ms,
        load.frames,
        u32::from(load.sync),
        load.sgr_every,
    ]
    .iter()
    .map(u32::to_string)
    .collect();
    let mut s = Session::new(&ctx, &cfg, &profile, None).expect("spawn fake tui");
    // A grid that fits the frame without wrapping: 10x20 px cells, 1 ppp.
    let (cols, rows) = (load.width + 2, load.lines + 6);
    let area = egui::Rect::from_min_size(
        egui::pos2(0.0, 0.0),
        egui::vec2(cols as f32 * 10.0, rows as f32 * 20.0),
    );

    let period = Duration::from_secs_f64(1.0 / pacing.hz);
    let start = Instant::now();
    let next_vblank = |t: Instant| {
        let n = ((t - start).as_secs_f64() / period.as_secs_f64()).floor() + 1.0;
        start + period.mul_f64(n)
    };
    let mut stats = Stats::default();
    let mut free_at = start;
    let mut pending: Option<Instant> = Some(start);
    let mut seen = BTreeSet::new();
    let mut shown: Option<(u32, Instant)> = None;
    let mut last_present: Option<Instant> = None;
    let deadline = Duration::from_millis(
        u64::from(load.frames) * u64::from(load.interval_ms.max(1)) * 4 + 20_000,
    );
    let mut done = false;
    while !done {
        assert!(start.elapsed() < deadline, "fake TUI never finished");
        // Wait for the earliest requested repaint. With nothing requested at
        // all, idle like the app's half-second wake does.
        loop {
            while let Ok(t) = rx.try_recv() {
                pending = Some(pending.map_or(t, |p| p.min(t)));
            }
            let Some(due) = pending else {
                match rx.recv_timeout(Duration::from_millis(500)) {
                    Ok(t) => pending = Some(t),
                    Err(_) => pending = Some(Instant::now()),
                }
                continue;
            };
            let at = due.max(free_at);
            let now = Instant::now();
            if at <= now {
                break;
            }
            if let Ok(t) = rx.recv_timeout(at - now) {
                pending = Some(pending.map_or(t, |p| p.min(t)));
            }
        }
        pending = None;

        // One pass, inside an egui pass so its repaint requests behave as in
        // the app (a zero-delay request's follow-up pass included).
        let _ = ctx.run_ui(egui::RawInput::default(), |ui| {
            let ctx = ui.ctx();
            s.fit_grid(area, 1.0, 10.0, 20.0, 0.0);
            if s.pump_pty() {
                ctx.request_repaint();
            }
            s.update_snapshot();
            if let Some(left) = s.sync_hold_left() {
                ctx.request_repaint_after(left);
            }
            if let Some(linger) = pacing.steady
                && s.animating_within(linger)
            {
                ctx.request_repaint();
            }
        });
        let now = Instant::now();
        // The present goes out at the next scanout; nothing runs before then.
        free_at = next_vblank(now);
        stats.passes += 1;
        if s.sync_hold_left().is_some() {
            stats.held += 1;
        }

        let snap = &s.snapshot;
        let text: String = (0..snap.rows as usize).map(|r| row_text(snap, r)).collect();
        if text.contains("FAKE-TUI-DONE") {
            done = true;
        }
        let t = tags(snap);
        if t.is_empty() || done {
            continue;
        }
        if let Some(prev) = last_present {
            stats.intervals.push(free_at - prev);
        }
        last_present = Some(free_at);
        let frames: BTreeSet<u32> = t.iter().map(|&(f, _)| f).collect();
        let lines: BTreeSet<u32> = t.iter().map(|&(_, l)| l).collect();
        let whole = frames.len() == 1 && lines.len() == load.lines as usize;
        if whole {
            stats.whole += 1;
            let f = *frames.iter().next().unwrap();
            seen.insert(f);
            match shown {
                Some((g, since)) if g == f => {
                    // Still the same frame: a stall only if newer ones are due.
                    let due =
                        (now.duration_since(since).as_millis() as u32) / load.interval_ms.max(1);
                    if f + 1 < load.frames && due > 0 {
                        stats.longest_stall = stats.longest_stall.max(now - since);
                    }
                }
                _ => shown = Some((f, now)),
            }
        } else {
            stats.torn += 1;
            if stats.examples.len() < 3 {
                stats.examples.push(format!(
                    "frames {frames:?}, {} of {} lines",
                    lines.len(),
                    load.lines
                ));
            }
        }
    }
    stats.shown = seen.len();
    stats
}

fn report(name: &str, load: Load, pacing: Pacing, st: &Stats) {
    let period = Duration::from_secs_f64(1.0 / pacing.hz);
    let mut iv: Vec<_> = st.intervals.iter().map(Duration::as_secs_f64).collect();
    iv.sort_by(f64::total_cmp);
    let pct = |p: f64| {
        iv.get(((iv.len() as f64 - 1.0) * p) as usize)
            .copied()
            .unwrap_or(0.0)
            * 1e3
    };
    eprintln!(
        "{name}: {load:?} {pacing:?}\n  passes={} whole={} torn={} held={} shown={}/{} \
         longest_stall={:?}\n  present interval ms: min {:.1} p50 {:.1} p90 {:.1} max {:.1}; \
         rate swings {} of {} {:?}",
        st.passes,
        st.whole,
        st.torn,
        st.held,
        st.shown,
        load.frames,
        st.longest_stall,
        pct(0.0),
        pct(0.5),
        pct(0.9),
        pct(1.0),
        st.rate_swings(period),
        st.intervals.len(),
        st.examples,
    );
}

/// Frames Claude Code's size late in a session, back to back: the case where
/// every pass lands inside some frame.
const HEAVY: Load = Load {
    lines: 45,
    width: 200,
    interval_ms: 4,
    frames: 600,
    sync: true,
    sgr_every: 4,
};

/// Claude Code's fullscreen spinner as measured on a live session: a small
/// redraw every ~117 ms (median; p90 140 ms).
const SPINNER: Load = Load {
    lines: 8,
    width: 100,
    interval_ms: 117,
    frames: 40,
    sync: true,
    sgr_every: 8,
};

const PLAIN: Pacing = Pacing {
    hz: 144.0,
    steady: None,
};

const STEADY: Pacing = Pacing {
    hz: 144.0,
    steady: Some(Duration::from_millis(300)),
};

#[test]
#[ignore = "spawns processes behind ConPTY; run with --ignored --nocapture"]
fn heavy_synchronized_frames_neither_tear_nor_stall() {
    let st = run(HEAVY, PLAIN);
    report("heavy", HEAVY, PLAIN, &st);
    assert_eq!(st.torn, 0, "presented torn frames: {:?}", st.examples);
    assert!(
        st.longest_stall < Duration::from_millis(250),
        "the screen froze for {:?} while frames kept coming",
        st.longest_stall
    );
}

#[test]
#[ignore = "spawns processes behind ConPTY; run with --ignored --nocapture"]
fn a_spinner_presents_once_per_redraw_without_steady_refresh() {
    // Without `window-steady-refresh`: each redraw should cost one present
    // (egui's follow-up pass for a zero-delay wake would make it two).
    let st = run(SPINNER, PLAIN);
    report("spinner, plain", SPINNER, PLAIN, &st);
    assert_eq!(st.torn, 0, "presented torn frames: {:?}", st.examples);
    let per_frame = f64::from(st.passes) / f64::from(SPINNER.frames);
    assert!(
        per_frame < 1.5,
        "{per_frame:.2} passes per redraw: the PTY wake is presenting twice"
    );
}

#[test]
#[ignore = "spawns processes behind ConPTY; run with --ignored --nocapture"]
fn a_spinner_holds_one_refresh_rate_with_steady_refresh() {
    // With it: while the spinner runs, presents come every refresh, so a
    // variable-refresh display never changes rate mid-animation.
    let st = run(SPINNER, STEADY);
    report("spinner, steady", SPINNER, STEADY, &st);
    assert_eq!(st.torn, 0, "presented torn frames: {:?}", st.examples);
    let period = Duration::from_secs_f64(1.0 / STEADY.hz);
    let swings = st.rate_swings(period);
    assert!(
        swings * 100 <= st.intervals.len(),
        "{swings} rate swings in {} presents while the spinner ran",
        st.intervals.len()
    );
}

#[test]
#[ignore = "spawns processes behind ConPTY; run with --ignored --nocapture"]
fn unsynchronized_frames_baseline() {
    // No DEC 2026: what a program that never detected support looks like.
    // Reported, not asserted — without the program's cooperation a terminal
    // cannot know where a frame ends.
    let load = Load {
        sync: false,
        ..HEAVY
    };
    let st = run(load, PLAIN);
    report("unsynchronized", load, PLAIN, &st);
}

/// Passes a pane's output causes when the pane is pumped every pass (as
/// `App::pump_all` does for every pane) but drawn only if `drawn` — `false`
/// is a background tab. Returns `(passes, animation seconds)`.
fn passes_while_animating(load: Load, drawn: bool) -> (u32, f64) {
    select_mode();
    let ctx = egui::Context::default();
    let (tx, rx) = mpsc::channel::<Instant>();
    ctx.set_request_repaint_callback(move |info| {
        let _ = tx.send(Instant::now() + info.delay);
    });
    let cfg = Config {
        shell_prewarm: false,
        ..Config::default()
    };
    let mut profile = Profile::new("fake-tui", &fake_tui().to_string_lossy());
    profile.args = [
        load.lines,
        load.width,
        load.interval_ms,
        load.frames,
        u32::from(load.sync),
        load.sgr_every,
    ]
    .iter()
    .map(u32::to_string)
    .collect();
    let mut s = Session::new(&ctx, &cfg, &profile, None).expect("spawn fake tui");
    let area = egui::Rect::from_min_size(
        egui::pos2(0.0, 0.0),
        egui::vec2(
            (load.width + 2) as f32 * 10.0,
            (load.lines + 6) as f32 * 20.0,
        ),
    );
    // Sized and drawn once, as a tab is before the user switches away from it.
    s.fit_grid(area, 1.0, 10.0, 20.0, 0.0);
    s.update_snapshot();
    // The fake TUI starts drawing 700 ms in and draws for `frames` intervals.
    let begin = Instant::now() + Duration::from_millis(700);
    let end = begin + Duration::from_millis(u64::from(load.frames * load.interval_ms));
    let mut passes = 0;
    while Instant::now() < end + Duration::from_millis(200) {
        // A wake, or the app's half-second idle pass.
        let _ = rx.recv_timeout(Duration::from_millis(500));
        while rx.try_recv().is_ok() {}
        let _ = ctx.run_ui(egui::RawInput::default(), |ui| {
            if s.pump_pty() {
                ui.ctx().request_repaint();
            }
            if drawn {
                s.update_snapshot();
            }
        });
        let now = Instant::now();
        if now > begin && now < end {
            passes += 1;
        }
    }
    (passes, (end - begin).as_secs_f64())
}

#[test]
#[ignore = "spawns processes behind ConPTY; run with --ignored --nocapture"]
fn a_spinner_in_a_background_tab_does_not_set_the_present_rhythm() {
    // Every pass presents, so a background tab's animation used to present
    // the foreground tab at its own irregular rhythm. Now its output waits
    // for the app's next pass unless it piles up.
    let (fg, secs) = passes_while_animating(SPINNER, true);
    let (bg, _) = passes_while_animating(SPINNER, false);
    eprintln!(
        "spinner drawn: {fg} passes in {secs:.1} s ({:.1}/s); in a background tab: {bg} ({:.1}/s)",
        f64::from(fg) / secs,
        f64::from(bg) / secs
    );
    assert!(
        f64::from(bg) / secs < 3.0,
        "a background tab's redraws are still waking the app: {bg} passes in {secs:.1} s"
    );
}
