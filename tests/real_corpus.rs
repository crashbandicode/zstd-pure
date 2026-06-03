//! Real-world corpus round-trip — fixture-gated, `#[ignore]` by default so
//! plain `cargo test` stays offline and fast.
//!
//! Walks the directory named by `$ZSTD_PURE_CORPUS` recursively and round-trips
//! every regular file through our codec and libzstd, *both ways*, across a few
//! levels, tracking the aggregate compressed-size ratio (ours / libzstd):
//!
//!   - our `compress` -> our `decompress` AND libzstd decode  (our encoder is
//!     correct and libzstd-compatible);
//!   - libzstd `compress` -> our `decompress`                 (the oracle).
//!
//! Every file is treated as raw input, so any corpus works without filtering:
//!   - Silesia (the standard ~200 MB ratio corpus): compressible real files;
//!   - TotK `.mc` BFRES containers (the user's production data): real, mostly
//!     incompressible bytes that exercise the raw-block fallback. (Magicless-
//!     *frame* decode conformance for BFRES lives in the Toolbox-Cli test.)
//!
//! Run, e.g.:
//!   ZSTD_PURE_CORPUS=~/fixtures/silesia \
//!     cargo test --release real_corpus -- --ignored --nocapture
//!
//! Knobs: `ZSTD_PURE_CORPUS_LEVELS` (default `3,9,19`),
//! `ZSTD_PURE_CORPUS_MAX_MB` (skip files larger than this; default: no limit),
//! and `ZSTD_PURE_CORPUS_LONG` (set to use `compress_long` — long-distance
//! matching — for our encoder, to measure the LDM ratio on real data).
//! `--nocapture` surfaces the per-level ratio summary.

use std::path::{Path, PathBuf};

use zstd_pure::{compress as our_compress, compress_long as our_compress_long, decompress};

/// Recursively collect every regular file under `root`, sorted for determinism.
fn corpus_files(root: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let Ok(entries) = std::fs::read_dir(&dir) else {
            continue;
        };
        for entry in entries.flatten() {
            let p = entry.path();
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

fn env_levels() -> Vec<i32> {
    match std::env::var("ZSTD_PURE_CORPUS_LEVELS") {
        Ok(s) => s
            .split(',')
            .filter_map(|t| t.trim().parse::<i32>().ok())
            .collect(),
        Err(_) => vec![3, 9, 19],
    }
}

fn env_max_bytes() -> u64 {
    std::env::var("ZSTD_PURE_CORPUS_MAX_MB")
        .ok()
        .and_then(|s| s.trim().parse::<u64>().ok())
        .map(|mb| mb * 1024 * 1024)
        .unwrap_or(u64::MAX)
}

#[test]
#[ignore = "set ZSTD_PURE_CORPUS to a directory of real files"]
fn real_corpus_round_trips_both_ways() {
    let Ok(root) = std::env::var("ZSTD_PURE_CORPUS") else {
        eprintln!("skipping: set ZSTD_PURE_CORPUS to a directory of real files");
        return;
    };
    let levels = env_levels();
    assert!(!levels.is_empty(), "ZSTD_PURE_CORPUS_LEVELS parsed to no levels");
    let max_bytes = env_max_bytes();
    let use_long = std::env::var("ZSTD_PURE_CORPUS_LONG").is_ok();

    let files = corpus_files(Path::new(&root));
    assert!(!files.is_empty(), "no regular files under {root}");

    let mut ours_bytes = vec![0u64; levels.len()];
    let mut lib_bytes = vec![0u64; levels.len()];
    let mut total_raw = 0u64;
    let (mut n_files, mut n_skipped) = (0usize, 0usize);

    for path in &files {
        let Ok(data) = std::fs::read(path) else {
            continue;
        };
        if data.len() as u64 > max_bytes {
            n_skipped += 1;
            continue;
        }
        total_raw += data.len() as u64;
        n_files += 1;

        for (i, &level) in levels.iter().enumerate() {
            // No content checksum, so the size is apples-to-apples with
            // libzstd's no-checksum default below.
            let ours = if use_long {
                our_compress_long(&data, level, false, true)
            } else {
                our_compress(&data, level, false, true)
            };
            let by_self = decompress(&ours).unwrap_or_else(|e| {
                panic!("{}: our decode of our L{level} frame: {e}", path.display())
            });
            assert!(
                by_self == data,
                "{}: our encoder round-trip mismatch at L{level}",
                path.display()
            );
            let by_lib = zstd::bulk::decompress(&ours, data.len() + 64).unwrap_or_else(|e| {
                panic!("{}: libzstd decode of our L{level} frame: {e}", path.display())
            });
            assert!(
                by_lib == data,
                "{}: libzstd decode of our L{level} frame mismatch",
                path.display()
            );

            let lib = zstd::bulk::compress(&data, level).expect("libzstd compress");
            let by_us = decompress(&lib).unwrap_or_else(|e| {
                panic!("{}: our decode of libzstd L{level} frame: {e}", path.display())
            });
            assert!(
                by_us == data,
                "{}: our decode of libzstd L{level} frame mismatch",
                path.display()
            );

            ours_bytes[i] += ours.len() as u64;
            lib_bytes[i] += lib.len() as u64;
        }
    }

    let mode = if use_long { "compress_long" } else { "compress" };
    eprintln!("real corpus {root} [{mode}]: {n_files} files ({total_raw} raw bytes), {n_skipped} skipped");
    for (i, &level) in levels.iter().enumerate() {
        let ratio = ours_bytes[i] as f64 / lib_bytes[i].max(1) as f64;
        eprintln!(
            "  L{level:>2}: ours {:>13} B   libzstd {:>13} B   ratio {ratio:.3}x",
            ours_bytes[i], lib_bytes[i]
        );
    }
    assert!(n_files >= 1, "every file exceeded ZSTD_PURE_CORPUS_MAX_MB");
}
