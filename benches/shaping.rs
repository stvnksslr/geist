//! Text-shaping throughput — the CPU cost of `rustybuzz` shaping over the
//! embedded primary font, which is the per-row work `Atlas::shape_run` does
//! (minus the GPU atlas, so no device is needed). Contrasts plain ASCII with a
//! ligature-heavy line (where contextual-alternate substitution does more work).
//! Run: `cargo bench --bench shaping`.
//!
//! Note: full glyph rasterization + instance assembly + GPU upload are NOT
//! benched here — those need a live wgpu device (`Atlas::new` takes one). That
//! headless-GPU harness is deferred (see docs/testing-host-env-gap-analysis.md).

use criterion::{Criterion, Throughput, criterion_group, criterion_main};
use geist::render::regular_font;
use rustybuzz::{Direction, Face, UnicodeBuffer};

/// Shape `s` and return the produced glyph count (keeps the work observable).
fn shape(face: &Face, s: &str) -> usize {
    let mut buf = UnicodeBuffer::new();
    buf.push_str(s);
    buf.set_direction(Direction::LeftToRight);
    rustybuzz::shape(face, &[], buf).len()
}

fn bench_shaping(c: &mut Criterion) {
    let face = Face::from_slice(regular_font(), 0).expect("regular face");

    // A representative 80-column ASCII line and a ligature-dense one.
    let ascii: String = "the quick brown fox jumps over the lazy dog 0123456789 !@#$%^&*()_+-= "
        .chars()
        .cycle()
        .take(80)
        .collect();
    let ligatures = "=> != === >= <= |> <| -> <- ... :: && || ++ -- ==> <=> /* */ ".repeat(2);

    let mut group = c.benchmark_group("shaping");
    group.throughput(Throughput::Bytes(ascii.len() as u64));
    group.bench_function("ascii_80col", |b| {
        b.iter(|| std::hint::black_box(shape(&face, &ascii)))
    });
    group.throughput(Throughput::Bytes(ligatures.len() as u64));
    group.bench_function("ligature_dense", |b| {
        b.iter(|| std::hint::black_box(shape(&face, &ligatures)))
    });
    group.finish();
}

criterion_group!(benches, bench_shaping);
criterion_main!(benches);
