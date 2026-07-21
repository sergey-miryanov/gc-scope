use criterion::{Criterion, criterion_group, criterion_main};
use std::hint::black_box;

use gcscope::memory::binary::classify;
use gcscope::memory::dump::format_hex_dump;

/// A few representative magic-byte prefixes covering every branch of `classify`.
fn classify_inputs() -> Vec<(&'static str, Vec<u8>)> {
    vec![
        ("elf", vec![0x7f, 0x45, 0x4c, 0x46, 0x02, 0x01, 0x01, 0x00]),
        ("pe", vec![0x4d, 0x5a, 0x90, 0x00, 0x03, 0x00, 0x00, 0x00]),
        (
            "macho",
            vec![0xcf, 0xfa, 0xed, 0xfe, 0x07, 0x00, 0x00, 0x01],
        ),
        ("fat", vec![0xca, 0xfe, 0xba, 0xbe, 0x00, 0x00, 0x00, 0x02]),
        (
            "unknown",
            vec![0x00, 0x11, 0x22, 0x33, 0x44, 0x55, 0x66, 0x77],
        ),
    ]
}

/// `classify` is only a handful of byte reads and a match, so a single call is
/// far shorter than the fixed per-measurement overhead of the harness. That
/// makes a one-call benchmark dominated by noise and prone to large *relative*
/// swings (e.g. when the CI runner CPU changes). Repeating the call inside the
/// measured closure makes the real work dominate, so the measurement is stable.
const CLASSIFY_ITERS: usize = 1024;

fn bench_classify(c: &mut Criterion) {
    let inputs = classify_inputs();
    let mut group = c.benchmark_group("classify");
    for (name, bytes) in &inputs {
        group.bench_function(*name, |b| {
            b.iter(|| {
                for _ in 0..CLASSIFY_ITERS {
                    black_box(classify(black_box(bytes.as_slice())));
                }
            });
        });
    }
    group.finish();
}

fn bench_format_hex_dump(c: &mut Criterion) {
    let mut group = c.benchmark_group("format_hex_dump");
    for size in [64usize, 4096, 65536] {
        // Deterministic pseudo-data so the output is stable across runs.
        let data: Vec<u8> = (0..size).map(|i| (i * 31 + 7) as u8).collect();
        group.bench_function(format!("{size}B"), |b| {
            b.iter(|| format_hex_dump(black_box(&data), black_box(0x7f00_0000)));
        });
    }
    group.finish();
}

criterion_group!(benches, bench_classify, bench_format_hex_dump);
criterion_main!(benches);
