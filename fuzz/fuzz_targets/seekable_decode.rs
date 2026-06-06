//! Hostile seekable-archive decode target. The `seekable_roundtrip` target proves
//! our *own* archives round-trip; this one stresses the random-access and parallel
//! decode paths on *adversarial* archives — corrupt offsets, sizes, checksums, and
//! out-of-range / pathological job counts must return `Err` (bounded) and never
//! panic, OOM, or read out of bounds.
#![no_main]

use libfuzzer_sys::fuzz_target;
use zstd_pure::{
    compress_seekable, decompress_seekable_frame, decompress_seekable_parallel_capped, SeekTable,
};

/// Output ceiling for the capped parallel path: a corrupt seek table can declare an
/// enormous total, and the cap must refuse it before the big allocation.
const CAP: usize = 1 << 20;

fuzz_target!(|data: &[u8]| {
    // (1) Treat the raw input as a (hostile) seekable archive. Parsing must only
    //     ever Ok/Err; and on a parsed-but-untrusted table every decode path must
    //     stay bounded no matter how corrupt the offsets/sizes/checksums are.
    if let Ok(table) = SeekTable::parse(data) {
        let n = table.num_frames();
        // Random access: in-range, boundary, and deliberately out-of-range indices.
        for i in [0usize, n / 2, n.saturating_sub(1), n, n.wrapping_add(7)] {
            let _ = decompress_seekable_frame(data, &table, i);
        }
        // Whole-archive parallel decode across a spread of job counts, including
        // 0 (must run single-threaded) and more jobs than frames (must clamp).
        for &jobs in &[0usize, 1, 3, n.saturating_add(1)] {
            let _ = decompress_seekable_parallel_capped(data, &table, jobs, CAP);
        }
    }

    // (2) A fresh corpus rarely produces a parseable seek table from scratch, so
    //     also build a real archive from the input and exercise the same paths on a
    //     byte-mutated copy. This reaches the corrupt-offset / size-mismatch /
    //     checksum-mismatch branches deterministically from the first run.
    if let [sel, payload @ ..] = data {
        let frame_size = 16 + (*sel as usize % 4) * 64;
        if let Ok(archive) = compress_seekable(payload, frame_size, 3, true) {
            let mut bad = archive.clone();
            if !bad.is_empty() {
                let at = (*sel as usize) % bad.len();
                bad[at] ^= 0xFF; // a single flip: a corrupt-but-maybe-parseable archive
            }
            for arc in [archive.as_slice(), bad.as_slice()] {
                if let Ok(table) = SeekTable::parse(arc) {
                    for i in 0..table.num_frames().min(8) {
                        let _ = decompress_seekable_frame(arc, &table, i);
                    }
                    let _ = decompress_seekable_parallel_capped(arc, &table, 4, CAP);
                    let _ = decompress_seekable_parallel_capped(arc, &table, 0, CAP);
                }
            }
        }
    }
});
