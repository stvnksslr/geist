//! VT-stream throughput — geist's analog of Ghostty's `TerminalStream` (and
//! `OscParser`) benchmarks. Measures the core hot path: `engine.write()`, which
//! both parses the VT byte stream *and* applies it to terminal state
//! (`GhosttyVtEngine::write` → libghostty-vt's `vt_write`). This is the single
//! number that most determines how fast the terminal keeps up with a firehose of
//! output (`cat` of a big file, a noisy build log, `tmux` redraws).
//!
//! Cases:
//!   - `ascii` / `utf8` / `osc` engine throughput across grid sizes (Bytes/s).
//!   - `osc_color_scan` etc.: geist's *own* OSC side-scanners, which run over
//!     every PTY chunk in `pump_pty` — a hot path Ghostty has no analog for (it
//!     parses OSC inside the engine). OSC 52 and OSC 7 are no longer scanned:
//!     the engine handles them.
//!
//! Input is the seeded synthetic stream by default; set `geist_BENCH_DATA` to a
//! capture file to run against a real corpus (see `benches/data/README.md`).
//! Run: `cargo bench --bench stream`.

use criterion::{Criterion, Throughput, criterion_group, criterion_main};
use geist::engine::{GhosttyVtEngine, TerminalEngine};
use geist::osc_color::OscColorScanner;
use geist::synthetic;

/// Roughly how many bytes of stream to push per timed iteration. A few MiB keeps
/// per-iteration noise low while staying well under a second per sample.
const STREAM_BYTES: usize = 4 << 20; // 4 MiB

fn bench_stream(c: &mut Criterion) {
    let mut group = c.benchmark_group("stream");

    for &(cols, rows) in &[(80u16, 24u16), (200, 50), (400, 100)] {
        for kind in ["ascii", "utf8", "osc"] {
            // Generate (or load a corpus) once, outside the timed loop.
            let (data, label) = match kind {
                "utf8" => synthetic::corpus(kind, STREAM_BYTES, 1, synthetic::utf8),
                // osc takes a *count*; size it to land near STREAM_BYTES.
                "osc" => synthetic::corpus(kind, STREAM_BYTES / 32, 1, synthetic::osc),
                _ => synthetic::corpus(kind, STREAM_BYTES, 1, synthetic::ascii),
            };
            group.throughput(Throughput::Bytes(data.len() as u64));
            // Build the engine once; writing more data just scrolls — the engine
            // stays valid across iterations, so this measures steady-state write.
            let mut eng = GhosttyVtEngine::new(cols, rows, 10_000).expect("engine");
            let id = format!("{label}/{cols}x{rows}");
            group.bench_function(id, |b| {
                b.iter(|| {
                    eng.write(&data);
                    std::hint::black_box(eng.scrollback_rows())
                })
            });
        }
    }
    group.finish();

    // geist-owned OSC side-scanners, fed the same OSC stream the engine sees.
    let mut scan = c.benchmark_group("osc_scan");
    let (osc_data, _) = synthetic::corpus("osc", STREAM_BYTES / 32, 1, synthetic::osc);
    scan.throughput(Throughput::Bytes(osc_data.len() as u64));
    scan.bench_function("osc_color_scan", |b| {
        let mut s = OscColorScanner::new();
        let mut out = Vec::new();
        b.iter(|| {
            out.clear();
            s.feed(&osc_data, &mut out);
            std::hint::black_box(out.len())
        })
    });
    scan.finish();
}

criterion_group!(benches, bench_stream);
criterion_main!(benches);
