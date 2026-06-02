//! Throughput benches (criterion) for the encoder and decoder, with libzstd
//! (the `zstd` crate) as the reference point. Ratio is tracked separately by
//! `examples/ratio.rs` / `BENCHMARKS.md` — criterion measures time, not size.
//!
//! Run with `cargo bench`. The corpus is a ~256 KiB mix of repetitive text,
//! structured records, and a pseudo-random tail, so both the parse and entropy
//! paths are exercised and at least one cross-block boundary is crossed.

use criterion::{criterion_group, criterion_main, BenchmarkId, Criterion, Throughput};
use std::hint::black_box;
use zstd_pure::{compress, decompress};

fn corpus() -> Vec<u8> {
    let mut v = Vec::new();
    let text = b"the quick brown fox jumps over the lazy dog. ";
    while v.len() < 96 * 1024 {
        v.extend_from_slice(text);
    }
    for i in 0..8_000u32 {
        v.extend_from_slice(format!("{{\"id\":{i},\"v\":\"x_{}\"}}\n", i % 97).as_bytes());
    }
    for i in 0..40_000u32 {
        v.push((i.wrapping_mul(2654435761) >> 13) as u8);
    }
    v
}

fn bench_compress(c: &mut Criterion) {
    let data = corpus();
    let mut g = c.benchmark_group("compress");
    g.throughput(Throughput::Bytes(data.len() as u64));
    for &level in &[3i32, 9, 19] {
        g.bench_with_input(BenchmarkId::new("zstd_pure", level), &level, |b, &lvl| {
            b.iter(|| compress(black_box(&data), lvl, false, true))
        });
        g.bench_with_input(BenchmarkId::new("libzstd", level), &level, |b, &lvl| {
            b.iter(|| zstd::bulk::compress(black_box(&data), lvl).unwrap())
        });
    }
    g.finish();
}

fn bench_decompress(c: &mut Criterion) {
    let data = corpus();
    let frame_ours = compress(&data, 9, false, true);
    let frame_lz = zstd::bulk::compress(&data, 9).unwrap();
    let cap = data.len() + 64;

    let mut g = c.benchmark_group("decompress");
    g.throughput(Throughput::Bytes(data.len() as u64));
    g.bench_function("zstd_pure/our_frame", |b| {
        b.iter(|| decompress(black_box(&frame_ours)).unwrap())
    });
    g.bench_function("zstd_pure/libzstd_frame", |b| {
        b.iter(|| decompress(black_box(&frame_lz)).unwrap())
    });
    g.bench_function("libzstd/libzstd_frame", |b| {
        b.iter(|| zstd::bulk::decompress(black_box(&frame_lz), cap).unwrap())
    });
    g.finish();
}

criterion_group!(benches, bench_compress, bench_decompress);
criterion_main!(benches);
