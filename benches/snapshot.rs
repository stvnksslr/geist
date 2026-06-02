//! Snapshot copy-loop throughput — giest's host analog of Ghostty's `ScreenClone`
//! benchmark. Measures the cost of building a [`GridSnapshot`] from the engine
//! (the per-frame copy out of libghostty-vt that the renderer consumes), across
//! grid sizes and input classes. Run: `cargo bench --bench snapshot`.

use criterion::{Criterion, Throughput, criterion_group, criterion_main};
use giest::engine::{GhosttyVtEngine, GridSnapshot, TerminalEngine};
use giest::synthetic;

/// Build an engine of `cols`×`rows` pre-filled with ~2 screenfuls of `kind` data.
fn filled(cols: u16, rows: u16, kind: &str) -> GhosttyVtEngine {
    let mut eng = GhosttyVtEngine::new(cols, rows, 2000).expect("engine");
    let bytes = cols as usize * rows as usize * 2;
    let data = match kind {
        "utf8" => synthetic::utf8(bytes, 1),
        _ => synthetic::ascii(bytes, 1),
    };
    eng.write(&data);
    eng
}

fn bench_snapshot(c: &mut Criterion) {
    let mut group = c.benchmark_group("snapshot");
    for &(cols, rows) in &[(80u16, 24u16), (200, 50), (400, 100)] {
        let cells = cols as u64 * rows as u64;
        group.throughput(Throughput::Elements(cells));
        for kind in ["ascii", "utf8"] {
            let mut eng = filled(cols, rows, kind);
            let mut snap = GridSnapshot::default();
            // Prime once so `cells`/buffers are sized before timing.
            eng.snapshot(&mut snap).unwrap();
            let id = format!("{kind}/{cols}x{rows}");
            group.bench_function(id, |b| {
                b.iter(|| {
                    eng.snapshot(&mut snap).unwrap();
                    std::hint::black_box(snap.cells.len())
                })
            });
        }
    }
    group.finish();
}

criterion_group!(benches, bench_snapshot);
criterion_main!(benches);
