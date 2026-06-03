//! Parallel (multi-threaded) compression — the **independent-frame** model, the
//! pragmatic shape of zstd's `--threads` / ZSTDMT.
//!
//! [`compress_parallel`] splits the input into `n_jobs` contiguous segments,
//! compresses each **independently** (each segment becomes a complete frame via
//! [`compress`](crate::compress)) on its own worker thread, and concatenates the
//! frames into a single **multi-frame** zstd stream — which both this crate's
//! decoder and libzstd read end-to-end. It uses only `std::thread` (scoped
//! threads), so there is no new runtime dependency, and the whole module is
//! behind the `std` feature (the `no_std`/`alloc` build never sees it).
//!
//! Trade-off: matching cannot cross a segment boundary, so the seams cost a
//! little ratio versus one-shot [`compress`](crate::compress) (the same trade as
//! libzstd's job model). Segments should therefore be much larger than the
//! window; tiny inputs fall back to a single frame. Output is **deterministic**:
//! the same `(data, level, n_jobs, …)` always yields identical bytes (segments
//! are assigned by fixed byte offsets and reassembled in order, regardless of the
//! order workers finish).
//!
//! Long-distance matching is intentionally *not* used per segment: its win is on
//! far-apart duplicates, which the independent-frame split severs anyway, and a
//! segment is already smaller than the whole input.

#[allow(unused_imports)]
use crate::alloc_prelude::*;

/// Below this many bytes per job we don't bother splitting — per-frame headers
/// and thread setup would outweigh the parallelism. The effective job count is
/// reduced so each segment is at least this large.
const MIN_JOB_SIZE: usize = 128 * 1024;

/// Hard ceiling on worker threads, so a pathologically large `n_jobs` cannot
/// spawn an unbounded number of threads.
const MAX_JOBS: usize = 256;

/// Compress `data` in parallel across up to `n_jobs` worker threads, returning a
/// multi-frame zstd stream (each job is one independent frame).
///
/// `level`, `checksum`, and `expect_magic` mean exactly what they do for
/// [`compress`](crate::compress) and apply to every segment's frame. With
/// `expect_magic = true` the result is a standard multi-frame stream that
/// [`decompress`](crate::decompress) and libzstd decode directly; with
/// `expect_magic = false` it is a run of magicless frames (decode each in turn
/// with [`decompress_magicless`](crate::decompress_magicless)).
///
/// The effective job count is clamped so each segment is at least `MIN_JOB_SIZE`
/// (and to a sane maximum), so small inputs transparently fall back to a single
/// frame — `compress_parallel(data, level, 1, …)` is exactly
/// `compress(data, level, …)`. Deterministic for fixed arguments.
pub fn compress_parallel(
    data: &[u8],
    level: i32,
    n_jobs: usize,
    checksum: bool,
    expect_magic: bool,
) -> Vec<u8> {
    let jobs = effective_jobs(data.len(), n_jobs);
    if jobs <= 1 {
        return super::frame::compress(data, level, checksum, expect_magic);
    }

    // Deterministic contiguous segments; integer division spreads the remainder
    // so segment sizes differ by at most one byte, and every segment is non-empty
    // (jobs <= data.len() / MIN_JOB_SIZE).
    let len = data.len();
    let bounds: Vec<(usize, usize)> =
        (0..jobs).map(|i| (i * len / jobs, (i + 1) * len / jobs)).collect();

    // One scoped worker per segment: scoped threads may borrow `data`, and they
    // all join before the scope returns, so no `'static` bound or copy is needed.
    let results: Vec<Vec<u8>> = std::thread::scope(|s| {
        let handles: Vec<_> = bounds
            .iter()
            .map(|&(a, b)| s.spawn(move || super::frame::compress(&data[a..b], level, checksum, expect_magic)))
            .collect();
        // Collect in spawn order (== segment order), not completion order, which
        // is what makes the concatenated stream deterministic.
        handles.into_iter().map(|h| h.join().expect("compression worker panicked")).collect()
    });

    let total: usize = results.iter().map(Vec::len).sum();
    let mut out = Vec::with_capacity(total);
    for frame in &results {
        out.extend_from_slice(frame);
    }
    out
}

/// The number of segments/frames to actually produce: `n_jobs`, but clamped to
/// `[1, min(MAX_JOBS, data_len / MIN_JOB_SIZE)]` so segments stay meaningful.
fn effective_jobs(data_len: usize, n_jobs: usize) -> usize {
    let by_size = (data_len / MIN_JOB_SIZE).max(1);
    n_jobs.clamp(1, by_size.min(MAX_JOBS))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{compress, decompress, decompress_magicless};

    /// A few MiB of mildly-redundant data: large enough that several jobs split
    /// it, compressible enough that frames actually shrink.
    fn corpus(n: usize) -> Vec<u8> {
        let mut v = Vec::with_capacity(n);
        let mut x = 0x1234_5678u32;
        while v.len() < n {
            x = x.wrapping_mul(1664525).wrapping_add(1013904223);
            // Small alphabet -> real matches within each segment.
            v.push((x >> 27) as u8);
        }
        v
    }

    #[test]
    fn round_trips_both_ways_across_jobs() {
        let data = corpus(2 << 20); // 2 MiB
        for &n_jobs in &[1usize, 2, 3, 4, 8] {
            for &level in &[1i32, 3, 9] {
                let frame = compress_parallel(&data, level, n_jobs, true, true);
                // Our multi-frame decoder reconstructs it.
                assert_eq!(decompress(&frame).unwrap(), data, "self decode n_jobs={n_jobs} L{level}");
                // libzstd reads the concatenated frames (decode_all, not bulk).
                let by_lib = zstd::stream::decode_all(&frame[..])
                    .unwrap_or_else(|e| panic!("libzstd decode_all n_jobs={n_jobs} L{level}: {e}"));
                assert_eq!(by_lib, data, "libzstd mismatch n_jobs={n_jobs} L{level}");
            }
        }
    }

    #[test]
    fn parallel_actually_splits_and_is_deterministic() {
        let data = corpus(2 << 20);
        let serial = compress_parallel(&data, 6, 1, true, true);
        let parallel = compress_parallel(&data, 6, 4, true, true);
        // 1 job is exactly `compress`; 4 jobs must produce a different (multi-frame)
        // framing on an input this size.
        assert_eq!(serial, compress(&data, 6, true, true), "1 job must equal compress");
        assert_ne!(parallel, serial, "4 jobs should split into multiple frames");
        // Deterministic: same arguments -> identical bytes.
        assert_eq!(parallel, compress_parallel(&data, 6, 4, true, true), "must be deterministic");
        // It decodes correctly.
        assert_eq!(decompress(&parallel).unwrap(), data);
    }

    #[test]
    fn edge_cases_fall_back_to_a_single_frame() {
        // Empty, tiny, and a sub-threshold run: each should round-trip whatever
        // n_jobs is requested (effective jobs collapses to 1).
        for data in [vec![], vec![0u8], b"tiny".to_vec(), vec![7u8; 300_000]] {
            for &n_jobs in &[1usize, 4, 1000] {
                let frame = compress_parallel(&data, 3, n_jobs, false, true);
                assert_eq!(decompress(&frame).unwrap(), data, "len {} n_jobs {n_jobs}", data.len());
            }
        }
    }

    #[test]
    fn magicless_parallel_frames_decode_in_sequence() {
        // expect_magic = false -> a run of magicless frames; decode each in turn.
        let data = corpus(2 << 20);
        let frame = compress_parallel(&data, 3, 4, true, false);
        let mut pos = 0usize;
        let mut got = Vec::new();
        while pos < frame.len() {
            let f = decompress_magicless(&frame[pos..], usize::MAX).expect("magicless frame");
            got.extend_from_slice(&f.data);
            pos += f.consumed;
        }
        assert_eq!(got, data);
    }

    #[test]
    fn parallel_equals_serial_segmentation() {
        // The core multithreading guarantee: compress_parallel is *exactly* a
        // parallelization of "split at fixed byte offsets, compress each segment
        // with `compress`, concatenate". Pinning it to the serial composition
        // reduces this function's correctness to `compress`'s (already thoroughly
        // tested + fuzzed) and proves the split is deterministic and that threading
        // never perturbs any segment's bytes — independent of how the workers race
        // or how many cores are present.
        fn check(data: &[u8], n_jobs: usize, level: i32, checksum: bool) {
            let jobs = effective_jobs(data.len(), n_jobs);
            let len = data.len();
            let mut reference = Vec::new();
            for i in 0..jobs {
                let (a, b) = (i * len / jobs, (i + 1) * len / jobs);
                reference.extend_from_slice(&compress(&data[a..b], level, checksum, true));
            }
            assert_eq!(
                compress_parallel(data, level, n_jobs, checksum, true),
                reference,
                "n_jobs={n_jobs} L{level} ck{checksum} len={len}"
            );
        }
        let data = corpus(2 << 20);
        for &n_jobs in &[2usize, 3, 5, 8] {
            for &level in &[1i32, 9] {
                for &checksum in &[false, true] {
                    check(&data, n_jobs, level, checksum);
                }
            }
        }
        // The optimal parse (L19) on a smaller input that still splits, so the
        // composition is exercised at a high level without the debug-build cost of
        // L19 on the full 2 MiB.
        check(&corpus(300 * 1024), 2, 19, true);
    }

    #[test]
    fn many_segments_round_trip() {
        // A large input with a high job count fans out into many independent
        // frames; the concatenated multi-frame stream must still round-trip both
        // ways (exercises the segmentation + multi-frame decode at scale).
        let data = corpus(8 << 20); // 8 MiB
        let jobs = effective_jobs(data.len(), 64);
        assert!(jobs >= 32, "expected many segments, got {jobs}");
        let frame = compress_parallel(&data, 1, 64, true, true);
        assert_eq!(decompress(&frame).unwrap(), data, "self decode of {jobs}-frame stream");
        assert_eq!(zstd::stream::decode_all(&frame[..]).unwrap(), data, "libzstd decode_all");
    }

    #[test]
    fn effective_jobs_clamps() {
        // Sub-threshold -> 1; large input honors n_jobs up to the size/cap bounds.
        assert_eq!(effective_jobs(1000, 8), 1);
        assert_eq!(effective_jobs(2 << 20, 4), 4); // 2 MiB / 128 KiB = 16 >= 4
        assert_eq!(effective_jobs(2 << 20, 1000), 16); // capped by size: 2 MiB / 128 KiB
        assert_eq!(effective_jobs(0, 4), 1);
        assert_eq!(effective_jobs(1 << 30, 100_000), MAX_JOBS); // capped by MAX_JOBS
    }
}
