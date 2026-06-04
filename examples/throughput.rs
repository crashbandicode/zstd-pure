//! Throughput benchmark: zstd_pure vs libzstd, encode + decode, MB/s, over a real
//! corpus. Aggregate over files (best-of-N wall time per file). Level-aware data
//! budget keeps the slow optimal levels tractable.
//!
//!   ZSTD_PURE_CORPUS=~/fixtures/silesia cargo run --release --example throughput
//!
//! Knobs: ZSTD_PURE_BENCH_LEVELS (default "3,9,12,19"),
//!        ZSTD_PURE_BENCH_MAXMB  (per-file cap, default 8).
//! Falls back to ~10 MiB of enwik-like text if ZSTD_PURE_CORPUS is unset.

use std::path::{Path, PathBuf};
use std::time::Instant;
use zstd_pure::{compress, decompress};

mod common;
use common::enwik_like;

fn files_under(root: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    let mut stack = vec![root.to_path_buf()];
    while let Some(d) = stack.pop() {
        let Ok(rd) = std::fs::read_dir(&d) else {
            continue;
        };
        for e in rd.flatten() {
            let p = e.path();
            if p.is_dir() {
                stack.push(p);
            } else if p.is_file() {
                out.push(p);
            }
        }
    }
    out.sort();
    out
}

fn best<T>(iters: usize, mut f: impl FnMut() -> T) -> (T, f64) {
    let mut bs = f64::INFINITY;
    let mut last = None;
    for _ in 0..iters {
        let t = Instant::now();
        let r = f();
        bs = bs.min(t.elapsed().as_secs_f64());
        last = Some(r);
    }
    (last.unwrap(), bs)
}

fn mbps(bytes: usize, secs: f64) -> f64 {
    (bytes as f64 / (1024.0 * 1024.0)) / secs.max(1e-9)
}

fn main() {
    let max_mb: usize = std::env::var("ZSTD_PURE_BENCH_MAXMB")
        .ok()
        .and_then(|s| s.trim().parse().ok())
        .unwrap_or(8);
    let levels: Vec<i32> = std::env::var("ZSTD_PURE_BENCH_LEVELS")
        .ok()
        .map(|s| s.split(',').filter_map(|t| t.trim().parse().ok()).collect())
        .unwrap_or_else(|| vec![3, 9, 12, 19]);

    // Load inputs (capped per file), from the corpus or a synthetic fallback.
    let inputs: Vec<Vec<u8>> = match std::env::var("ZSTD_PURE_CORPUS") {
        Ok(root) => files_under(Path::new(&root))
            .iter()
            // Skip already-compressed archives (e.g. Silesia ships raw files
            // *and* .zip/.tar.gz copies) — they're incompressible and would skew
            // ratio + speed. The raw corpus files are extensionless.
            .filter(|p| {
                !p.file_name()
                    .is_some_and(|n| n.to_string_lossy().contains('.'))
            })
            .filter_map(|p| std::fs::read(p).ok())
            .filter(|d| !d.is_empty())
            .map(|mut d| {
                d.truncate(max_mb * 1024 * 1024);
                d
            })
            .collect(),
        Err(_) => {
            eprintln!("ZSTD_PURE_CORPUS unset — using synthetic enwik-like 10 MiB");
            vec![enwik_like(10 * 1024 * 1024, 0x7e57_d0d0)]
        }
    };
    let total: usize = inputs.iter().map(Vec::len).sum();
    println!(
        "{} inputs, {:.1} MiB total (≤{max_mb} MiB/file)\n",
        inputs.len(),
        total as f64 / (1024.0 * 1024.0)
    );
    println!(
        "{:<5} {:>7} {:>8} {:>8} {:>7}  {:>8} {:>8} {:>6}  {:>8} {:>8} {:>6}",
        "lvl",
        "data",
        "cmp us",
        "cmp lz",
        "size\u{0394}",
        "enc us",
        "enc lz",
        "x",
        "dec us",
        "dec lz",
        "x"
    );

    for &level in &levels {
        // Budget the data per level so the optimal levels stay tractable.
        let himb: usize = std::env::var("ZSTD_PURE_BENCH_HIMB")
            .ok()
            .and_then(|s| s.trim().parse().ok())
            .unwrap_or(4);
        let budget = if level >= 16 { himb } else { 48 } * 1024 * 1024usize;
        let iters = if level >= 16 { 1 } else { 3 };

        let (mut raw, mut our_e, mut lz_e, mut our_d, mut lz_d, mut csz, mut lsz) =
            (0usize, 0.0, 0.0, 0.0, 0.0, 0usize, 0usize);
        let mut used = 0usize;
        for data in &inputs {
            if used >= budget {
                break;
            }
            let n = data.len().min(budget - used);
            let data = &data[..n];
            used += n;
            raw += n;

            let (ours, e1) = best(iters, || compress(data, level, false, true));
            let (theirs, e2) = best(iters, || zstd::bulk::compress(data, level).unwrap());
            let (_o, d1) = best(iters.max(3), || decompress(&ours).unwrap());
            let (_l, d2) = best(iters.max(3), || {
                zstd::bulk::decompress(&theirs, n + 64).unwrap()
            });
            our_e += e1;
            lz_e += e2;
            our_d += d1;
            lz_d += d2;
            csz += ours.len();
            lsz += theirs.len();
        }

        let (eo, el) = (mbps(raw, our_e), mbps(raw, lz_e));
        let (deco, decl) = (mbps(raw, our_d), mbps(raw, lz_d));
        // cmp = compression ratio (raw / compressed); size\u{0394} = how much larger
        // our output is than libzstd's (csz/lsz - 1, in %).
        let cmp_ours = raw as f64 / csz.max(1) as f64;
        let cmp_libz = raw as f64 / lsz.max(1) as f64;
        let size_delta_pct = (csz as f64 / lsz.max(1) as f64 - 1.0) * 100.0;
        println!(
            "{:<5} {:>6.0}M {:>7.3}x {:>7.3}x {:>+6.1}% {:>8.1} {:>8.1} {:>5.1}x {:>8.1} {:>8.1} {:>5.1}x",
            level,
            raw as f64 / (1024.0 * 1024.0),
            cmp_ours,
            cmp_libz,
            size_delta_pct,
            eo,
            el,
            el / eo,
            deco,
            decl,
            decl / deco,
        );
    }
}
