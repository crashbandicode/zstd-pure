//! Continuous-frame parallel round-trip target: drive arbitrary input through
//! `compress_parallel` (the single-frame ZSTDMT model) at a fuzzer-chosen level
//! and job count, then require BOTH our decoder and libzstd to recover the exact
//! input. This is the coverage that lets the cross-seam rearchitecture reach the
//! stable tag: it stresses the per-seam repeat-offset self-heal (non-first
//! workers start with invalidated `[0,0,0]` rep) and the forced fresh entropy
//! tables on each worker's first block — the two pieces of cross-block state the
//! parallel split can't inherit — on hostile content and seam placements.
#![no_main]

use libfuzzer_sys::fuzz_target;
use zstd_pure::{compress_parallel, decompress};

fuzz_target!(|data: &[u8]| {
    // A 2-byte prefix selects the level, the checksum flag, and the job count
    // (2..=9); the rest is the seed. The level is capped at 12 (the chain
    // strategies — fast/dfast/greedy/lazy/lazy2): the cross-seam logic this target
    // exists to stress (the `[0,0,0]` rep self-heal and forced fresh entropy
    // tables) is *strategy-independent*, so the chain finders exercise the exact
    // same seam paths, but their O(n) priming keeps each iteration cheap enough to
    // run thousands of mutations. The binary-tree strategies' parallel path (the
    // expensive per-worker tree priming over the overlap) is covered by the
    // `decodes_identically_regardless_of_job_count` unit test (L19) and the
    // real-corpus parallel pass.
    let (level, checksum, n_jobs, seed) = match data {
        [a, b, rest @ ..] => (
            1 + (a % 12) as i32,
            a & 0x80 != 0,
            2 + (*b % 8) as usize,
            rest,
        ),
        _ => (3, false, 4, data),
    };

    // `compress_parallel` only splits past ~2*MIN_JOB_SIZE (256 KiB), so amplify
    // the fuzzer's bytes into a ~384 KiB payload to force segment seams (the
    // single-job fallback is already covered by `encode_roundtrip`). Tiling the
    // seed seeds cross-seam matches; the per-copy varying byte makes adjacent
    // regions differ, so workers must emit fresh entropy tables at each seam.
    let unit: &[u8] = if seed.is_empty() { b"\0" } else { seed };
    let mut payload = Vec::with_capacity(384 * 1024 + unit.len() + 256);
    let mut k: u8 = 0;
    while payload.len() < 384 * 1024 {
        payload.extend_from_slice(unit);
        payload.push(k);
        k = k.wrapping_add(1);
    }

    let frame = compress_parallel(&payload, level, n_jobs, checksum, true);

    let ours = decompress(&frame).expect("our decoder must decode the parallel frame");
    assert_eq!(ours, payload, "parallel round-trip mismatch (ours)");

    let theirs = zstd::bulk::decompress(&frame, payload.len() + 64)
        .expect("libzstd must decode the parallel frame");
    assert_eq!(theirs, payload, "parallel round-trip mismatch (libzstd)");
});
