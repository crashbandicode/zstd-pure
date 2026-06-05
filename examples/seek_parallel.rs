//! Parallel seekable-decode speedup check: compress a corpus file into a seekable
//! archive, then time serial whole-archive decode vs `decompress_seekable_parallel`
//! at a few worker counts.
//!
//!   ZSTD_PURE_CORPUS=~/fixtures/silesia cargo run --release --example seek_parallel

use std::time::Instant;
use zstd_pure::{compress_seekable, decompress, decompress_seekable_parallel, SeekTable};

fn best(iters: usize, mut f: impl FnMut() -> usize) -> (usize, f64) {
    let mut bs = f64::INFINITY;
    let mut n = 0;
    for _ in 0..iters {
        let t = Instant::now();
        n = f();
        bs = bs.min(t.elapsed().as_secs_f64());
    }
    (n, bs)
}

fn mbps(bytes: usize, secs: f64) -> f64 {
    (bytes as f64 / (1024.0 * 1024.0)) / secs.max(1e-9)
}

fn main() {
    // ~32 MiB of compressible, mildly varying text-like data (so decode does real
    // work and the frames aren't all identical).
    let phrases: [&[u8]; 4] = [
        b"the quick brown fox jumps over the lazy dog. ",
        b"pack my box with five dozen liquor jugs. ",
        b"how vexingly quick daft zebras jump! ",
        b"sphinx of black quartz, judge my vow. ",
    ];
    let mut data = Vec::with_capacity(32 * 1024 * 1024);
    let mut s = 0x1234_5678u32;
    while data.len() < 32 * 1024 * 1024 {
        s = s.wrapping_mul(1664525).wrapping_add(1013904223);
        data.extend_from_slice(phrases[(s >> 30) as usize & 3]);
    }
    let n = data.len();
    let frame_size = 1 << 20; // 1 MiB frames
    let archive = compress_seekable(&data, frame_size, 9, true).unwrap();
    let table = SeekTable::parse(&archive).unwrap();
    println!(
        "{:.1} MiB, {} frames of {} KiB",
        n as f64 / (1024.0 * 1024.0),
        table.num_frames(),
        frame_size / 1024
    );

    let (_s, t_serial) = best(5, || decompress(&archive).unwrap().len());
    println!("serial decompress      {:>8.1} MB/s", mbps(n, t_serial));

    for jobs in [2usize, 4, 8] {
        let (got, t) = best(5, || {
            decompress_seekable_parallel(&archive, &table, jobs)
                .unwrap()
                .len()
        });
        assert_eq!(got, n);
        println!(
            "parallel x{jobs:<2}           {:>8.1} MB/s   ({:.2}x)",
            mbps(n, t),
            t_serial / t
        );
    }
}
