//! Parallel (multi-threaded) compression — the **single continuous frame**
//! model, libzstd's ZSTDMT design.
//!
//! [`compress_parallel`] splits the input into `n_jobs` contiguous segments and
//! compresses each on its own worker thread, but — unlike the older
//! independent-frame split — every worker emits its blocks into **one shared
//! frame** with a continuously evolving window. Worker `i` is primed with the
//! tail of the preceding input (up to one window) as raw-content history, so its
//! matches reach back **across the segment seam** into segment `i-1`'s bytes,
//! exactly as a single-threaded [`compress`](crate::compress) would. The main
//! thread writes the one frame header, concatenates the workers' block bytes in
//! segment order, flags only the very last block, and appends a single
//! whole-input content checksum.
//!
//! **Why the seam no longer costs ratio.** A standard zstd decoder carries its
//! window, repeat offsets, and entropy tables continuously across block
//! boundaries within a frame, so it never resets at a segment seam — the encoder
//! just has to feed it a consistent stream. Two pieces of cross-block state can't
//! be known in parallel (they depend on the *previous* worker's final output), so
//! each non-first worker neutralises them the way ZSTDMT does:
//!
//! * **Repeat offsets** start *invalidated* (`[0, 0, 0]`). The match finder never
//!   proposes a repeat-offset match from a zero, so the worker's first sequences
//!   are forced to carry literal (real) offsets; the decoder evolves its repeat
//!   state from those identically, and within three real offsets the encoder's
//!   and decoder's repeat triples reconverge (`ZSTD_invalidateRepCodes`). A few
//!   sequences per seam can't use the cheap repeat code — a negligible cost.
//! * **Entropy tables** start empty, so a worker's first block always describes
//!   its own Huffman / FSE tables (never "Repeat" mode or treeless literals,
//!   which would alias the previous worker's tables). The decoder reseeds from
//!   them. Within a worker, blocks thread tables normally.
//!
//! The first worker is the true frame start: it begins with the default repeat
//! offsets `[1, 4, 8]` and no priming, so a single job is byte-identical to
//! [`compress`](crate::compress).
//!
//! Threading uses only `std::thread` (scoped threads borrow `data`, no copy or
//! `'static` bound), so there is no new runtime dependency, and the module is
//! behind the `std` feature (the `no_std`/`alloc` build never sees it). Output is
//! **deterministic**: each segment's bytes are a pure function of the input, the
//! fixed byte-offset bounds, and the level — independent of how the workers race
//! — and they are reassembled in segment order.
//!
//! Tiny inputs fall back to a single frame; `compress_parallel(data, level, 1, …)`
//! is exactly `compress(data, level, …)`. Long-distance matching is intentionally
//! not used: the continuous window already links across the seams, and LDM's reach
//! beyond the window is a separate, single-threaded concern.

#[allow(unused_imports)]
use crate::alloc_prelude::*;

use super::super::frame::ZSTD_MAGIC;
use super::super::xxhash::xxh64;
use super::block::{write_compressed_block, write_store_block, EncState, BLOCK_SIZE_MAX};
use super::frame::{split_depth_for, write_frame_header};
use super::lz::Finder;
use super::params::CParams;
use super::sequences::SeqCTables;

/// Below this many bytes per job we don't bother splitting — the per-worker
/// finder priming and thread setup would outweigh the parallelism. The effective
/// job count is reduced so each segment is at least this large.
const MIN_JOB_SIZE: usize = 128 * 1024;

/// Hard ceiling on worker threads, so a pathologically large `n_jobs` cannot
/// spawn an unbounded number of threads.
const MAX_JOBS: usize = 256;

/// One worker's slice of the job: where its view begins in `data`, how many
/// leading bytes of that view are primed-only history (the cross-seam overlap),
/// and where the view ends. The segment actually emitted as blocks is
/// `data[lo + overlap .. hi]`.
struct Job {
    /// Absolute start of this worker's `data` view (segment start minus overlap).
    lo: usize,
    /// Leading history bytes of the view, primed into the finder but never
    /// emitted (0 for the first job).
    overlap: usize,
    /// Absolute end of the segment (== view end).
    hi: usize,
    /// Repeat offsets to start from: the default `[1, 4, 8]` for the first job
    /// (the true frame start), invalidated `[0, 0, 0]` otherwise.
    init_rep: [u32; 3],
    /// Whether this is the final job, whose last block flags the end of frame.
    is_last: bool,
}

/// Compress `data` in parallel across up to `n_jobs` worker threads, returning a
/// **single** zstd frame whose blocks were produced concurrently.
///
/// `level`, `checksum`, and `expect_magic` mean exactly what they do for
/// [`compress`](crate::compress). The result is one ordinary frame:
/// [`decompress`](crate::decompress) and libzstd decode it end-to-end (and with
/// `expect_magic = false` it is one magicless frame —
/// [`decompress_magicless`](crate::decompress_magicless)). Because the window
/// evolves continuously across the segment seams, matching spans them, so the
/// ratio matches single-threaded [`compress`](crate::compress) bar a few
/// repeat-code-less sequences per seam.
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
        // A single job is the frame start with no seams — exactly `compress`.
        return super::frame::compress(data, level, checksum, expect_magic);
    }

    // One window, advertised once in the shared header and used by every worker;
    // sized for the whole input exactly as serial `compress` sizes it.
    let params = super::params::params_for_level(level, data.len());
    let window_log = params.window_log;
    let max_offset = 1usize << window_log;
    let split_depth = split_depth_for(params.strategy);

    // Deterministic contiguous segments; integer division spreads the remainder
    // so segment sizes differ by at most one byte, and every segment is non-empty
    // (jobs <= data.len() / MIN_JOB_SIZE). Each non-first job carries up to one
    // window of the preceding bytes as primed-only history, so its matches reach
    // back across the seam.
    let len = data.len();
    let plan: Vec<Job> = (0..jobs)
        .map(|i| {
            let a = i * len / jobs;
            let b = (i + 1) * len / jobs;
            let overlap = if i == 0 { 0 } else { a.min(max_offset) };
            Job {
                lo: a - overlap,
                overlap,
                hi: b,
                init_rep: if i == 0 { [1, 4, 8] } else { [0, 0, 0] },
                is_last: i == jobs - 1,
            }
        })
        .collect();

    // One scoped worker per segment: scoped threads may borrow `data`, and they
    // all join before the scope returns, so no `'static` bound or copy is needed.
    let blocks: Vec<Vec<u8>> = std::thread::scope(|s| {
        let handles: Vec<_> = plan
            .iter()
            .map(|job| {
                let view = &data[job.lo..job.hi];
                let (overlap, init_rep, is_last) = (job.overlap, job.init_rep, job.is_last);
                let params = &params;
                s.spawn(move || {
                    compress_segment(
                        view,
                        overlap,
                        params,
                        max_offset,
                        split_depth,
                        init_rep,
                        is_last,
                    )
                })
            })
            .collect();
        // Collect in spawn order (== segment order), not completion order — what
        // makes the concatenated frame deterministic.
        handles
            .into_iter()
            .map(|h| h.join().expect("compression worker panicked"))
            .collect()
    });

    // Assemble the one frame: shared header, every worker's blocks in order, then
    // the single whole-input checksum.
    let body: usize = blocks.iter().map(Vec::len).sum();
    let mut out = Vec::with_capacity(body + 32);
    if expect_magic {
        out.extend_from_slice(&ZSTD_MAGIC.to_le_bytes());
    }
    write_frame_header(&mut out, len as u64, checksum, window_log, 0);
    for b in &blocks {
        out.extend_from_slice(b);
    }
    if checksum {
        let digest = (xxh64(data, 0) & 0xFFFF_FFFF) as u32;
        out.extend_from_slice(&digest.to_le_bytes());
    }
    out
}

/// Compress one worker's segment into a buffer of **bare blocks** (no frame
/// header, no checksum). `view` is `data[lo..hi]`; its first `overlap` bytes are
/// primed-only cross-seam history and the segment `view[overlap..]` is emitted as
/// blocks. `init_rep` is `[1, 4, 8]` for the first job and the invalidated
/// `[0, 0, 0]` otherwise; the entropy tables start empty so the first block
/// always describes its own (never aliasing a previous worker's). Only the final
/// job's final block is flagged last.
///
/// This is the block loop of [`compress`](crate::compress) restricted to a
/// segment over a dictionary-primed view — the same shape as
/// [`compress_with_dict`](crate::compress_with_dict), where the "dictionary" is
/// the overlap. Offsets are back-distances within `view`, which equal the true
/// back-distances in the continuous frame, so the decoder reconstructs them
/// correctly.
fn compress_segment(
    view: &[u8],
    overlap: usize,
    params: &CParams,
    max_offset: usize,
    split_depth: usize,
    init_rep: [u32; 3],
    is_last: bool,
) -> Vec<u8> {
    let mut out = Vec::new();
    let mut finder = Finder::new(params);
    if overlap > 0 {
        // Index the overlap exactly as the parser would, emitting nothing — the
        // dictionary-priming path. Bounded to `max_offset` (the binary tree's
        // window_low clamp), so a full-window overlap can't alias live nodes.
        finder.prime(view, overlap, max_offset);
    }

    let mut state = EncState {
        rep: init_rep,
        seq: SeqCTables::default(),
        lit: None,
    };
    let n = view.len();
    let mut start = overlap;
    while start < n {
        let end = (start + BLOCK_SIZE_MAX).min(n);
        // Only the final job's final block ends the frame.
        let last = is_last && end == n;
        let mut store = Vec::new();
        write_store_block(&mut store, last, &view[start..end]);

        let mut comp = Vec::new();
        match write_compressed_block(
            &mut comp,
            last,
            view,
            start..end,
            &mut finder,
            max_offset,
            &state,
            split_depth,
        ) {
            Ok(next) if comp.len() < store.len() => {
                state = next;
                out.extend_from_slice(&comp);
            }
            // A store block leaves the decoder's repeat offsets / tables untouched,
            // so the invalidated `[0, 0, 0]` self-heal still holds across it.
            _ => out.extend_from_slice(&store),
        }
        start = end;
    }
    out
}

/// The number of segments to actually produce: `n_jobs`, but clamped to
/// `[1, min(MAX_JOBS, data_len / MIN_JOB_SIZE)]` so segments stay meaningful.
fn effective_jobs(data_len: usize, n_jobs: usize) -> usize {
    let by_size = (data_len / MIN_JOB_SIZE).max(1);
    n_jobs.clamp(1, by_size.min(MAX_JOBS))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{compress, decompress, decompress_magicless, frame_header};

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

    /// Count the zstd frames in `stream` by walking frame headers + content sizes
    /// is overkill; instead decode the first frame and confirm it consumed the
    /// whole stream (== exactly one frame). Helper below does that via libzstd.
    fn is_single_frame(stream: &[u8], expected: &[u8]) -> bool {
        // `bulk::decompress` reads exactly one frame and needs the destination
        // pre-sized; if one frame yields the whole payload, it was a single frame.
        match zstd::bulk::decompress(stream, expected.len() + 64) {
            Ok(got) => got == expected,
            Err(_) => false,
        }
    }

    #[test]
    fn round_trips_both_ways_across_jobs() {
        let data = corpus(2 << 20); // 2 MiB
        for &n_jobs in &[1usize, 2, 3, 4, 8] {
            for &level in &[1i32, 3, 9] {
                for &checksum in &[false, true] {
                    let frame = compress_parallel(&data, level, n_jobs, checksum, true);
                    // Our decoder reconstructs it.
                    assert_eq!(
                        decompress(&frame).unwrap(),
                        data,
                        "self decode n_jobs={n_jobs} L{level} ck{checksum}"
                    );
                    // libzstd reads it as a single frame (bulk::decompress reads
                    // exactly one frame), proving it's one continuous frame.
                    let by_lib = zstd::bulk::decompress(&frame, data.len() + 64)
                        .unwrap_or_else(|e| panic!("libzstd n_jobs={n_jobs} L{level}: {e}"));
                    assert_eq!(by_lib, data, "libzstd mismatch n_jobs={n_jobs} L{level}");
                }
            }
        }
    }

    #[test]
    fn output_is_one_continuous_frame() {
        // The whole point of the rearchitecture: 4 jobs produce ONE frame, not
        // four — `bulk::decompress` (single-frame) recovers all of it, and the
        // advertised content size equals the input length.
        let data = corpus(2 << 20);
        let frame = compress_parallel(&data, 6, 4, true, true);
        assert!(
            is_single_frame(&frame, &data),
            "4-job output must be a single frame"
        );
        let h = frame_header(&frame).unwrap();
        assert_eq!(
            h.content_size,
            Some(data.len() as u64),
            "single frame should pledge the whole content size"
        );
    }

    #[test]
    fn single_job_equals_compress_and_is_deterministic() {
        let data = corpus(2 << 20);
        // 1 job is exactly `compress`.
        assert_eq!(
            compress_parallel(&data, 6, 1, true, true),
            compress(&data, 6, true, true),
            "1 job must equal compress"
        );
        // Multi-job differs from the 1-job framing but stays deterministic.
        let parallel = compress_parallel(&data, 6, 4, true, true);
        assert_ne!(
            parallel,
            compress(&data, 6, true, true),
            "4 jobs should differ from the serial single-pass framing"
        );
        assert_eq!(
            parallel,
            compress_parallel(&data, 6, 4, true, true),
            "must be deterministic"
        );
        assert_eq!(decompress(&parallel).unwrap(), data);
    }

    #[test]
    fn edge_cases_fall_back_to_a_single_frame() {
        // Empty, tiny, and a sub-threshold run: each should round-trip whatever
        // n_jobs is requested (effective jobs collapses to 1).
        for data in [vec![], vec![0u8], b"tiny".to_vec(), vec![7u8; 300_000]] {
            for &n_jobs in &[1usize, 4, 1000] {
                let frame = compress_parallel(&data, 3, n_jobs, false, true);
                assert_eq!(
                    decompress(&frame).unwrap(),
                    data,
                    "len {} n_jobs {n_jobs}",
                    data.len()
                );
            }
        }
    }

    #[test]
    fn magicless_single_frame_round_trips() {
        // expect_magic = false -> one magicless frame; our magicless decoder and
        // libzstd both read it whole.
        let data = corpus(2 << 20);
        let frame = compress_parallel(&data, 3, 4, true, false);
        let f = decompress_magicless(&frame, usize::MAX).expect("magicless frame");
        assert_eq!(f.data, data);
        assert_eq!(f.consumed, frame.len(), "one magicless frame consumes all");
    }

    #[test]
    fn decodes_identically_regardless_of_job_count() {
        // The decoded bytes are independent of how many workers produced them:
        // every job count reconstructs the exact input (the byte framing differs,
        // the payload does not).
        let data = corpus(3 << 20);
        for &level in &[1i32, 4, 9, 19] {
            // L19 only on a smaller slice — the binary tree's per-job priming is
            // expensive in debug builds.
            let input: &[u8] = if level >= 16 {
                &data[..400 * 1024]
            } else {
                &data
            };
            for &n_jobs in &[2usize, 3, 5, 8] {
                let frame = compress_parallel(input, level, n_jobs, true, true);
                assert_eq!(
                    decompress(&frame).unwrap(),
                    input,
                    "n_jobs={n_jobs} L{level}"
                );
                assert_eq!(
                    zstd::bulk::decompress(&frame, input.len() + 64).unwrap(),
                    input,
                    "libzstd n_jobs={n_jobs} L{level}"
                );
            }
        }
    }

    #[test]
    fn cross_seam_matching_matches_serial_ratio() {
        // The headline win: because the window now spans the seams, parallel
        // output is essentially the same size as serial `compress` — not the
        // ~1.1% larger the old independent-frame split cost. Allow a tiny margin
        // for the per-seam repeat-code-less / fresh-table sequences.
        let data = corpus(4 << 20);
        for &level in &[3i32, 9, 12] {
            let serial = compress(&data, level, false, true).len();
            let parallel = compress_parallel(&data, level, 8, false, true).len();
            assert!(
                parallel as f64 <= serial as f64 * 1.01,
                "L{level}: parallel {parallel} should be within 1% of serial {serial} \
                 (was ~1.1% worse under the independent-frame model)"
            );
        }
    }

    #[test]
    fn many_segments_round_trip() {
        // A large input with a high job count fans out into many segments whose
        // blocks all land in one frame; it must still round-trip both ways.
        let data = corpus(8 << 20); // 8 MiB
        let jobs = effective_jobs(data.len(), 64);
        assert!(jobs >= 32, "expected many segments, got {jobs}");
        let frame = compress_parallel(&data, 1, 64, true, true);
        assert_eq!(
            decompress(&frame).unwrap(),
            data,
            "self decode of {jobs}-segment frame"
        );
        assert_eq!(
            zstd::bulk::decompress(&frame, data.len() + 64).unwrap(),
            data,
            "libzstd single-frame decode"
        );
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
