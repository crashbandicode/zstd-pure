//! Property-based robustness tests (proptest) — an always-on, *shrinking*
//! complement to the deterministic LCG sweeps in `corpus.rs`.
//!
//! The LCG harness throws fixed pseudo-random inputs at the codec; proptest
//! adds **shrinking**, so when a property fails it minimises the offending
//! input and records it under `tests/proptest-regressions/` as a permanent,
//! tiny reproducer. Four properties, all using libzstd (a dev-only oracle):
//!
//! 1. `decoder_never_panics` — arbitrary bytes through the one-shot and
//!    streaming decoders only ever return `Ok`/`Err` (never panic / OOM).
//! 2. `mutated_frame_never_panics` — corrupting a valid frame (bit-flips +
//!    truncation) reaches deep decoder states but still never panics.
//! 3. `encoder_round_trips_both_ways` — our encoder's output decodes back to
//!    the input through *both* libzstd and our own decoder, across the level
//!    range and the checksum flag.
//! 4. `oracle_libzstd_to_us` — anything libzstd compresses, our decoder
//!    reproduces exactly.

use std::io::Read;

use proptest::prelude::*;

use zstd_pure::{
    compress as our_compress, decompress, decompress_capped, StreamingDecoder, ZstdError,
};

/// Output ceiling for the never-panic probes — bounds memory if a corrupt
/// header claims a huge size, so a "panic" can't hide behind an OOM kill.
const CAP: usize = 1 << 24; // 16 MiB

/// Drive the streaming decoder to completion, swallowing any error. Used by
/// the never-panic probes: construction or reads may fail, but must not panic.
fn drain_streaming(comp: &[u8]) {
    if let Ok(mut dec) = StreamingDecoder::new(comp) {
        let mut sink = Vec::new();
        let _ = dec.read_to_end(&mut sink);
    }
}

/// Like `drain_streaming`, but reads a few bytes at a time — the hostile
/// read-granularity path: a corrupt frame must never panic even when the caller
/// drains it one tiny buffer at a time (any read error is terminal here).
fn drain_streaming_chunked(comp: &[u8], read_size: usize) {
    if let Ok(mut dec) = StreamingDecoder::new(comp) {
        let mut buf = vec![0u8; read_size.max(1)];
        while let Ok(n) = dec.read(&mut buf) {
            if n == 0 {
                break;
            }
        }
    }
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(256))]

    /// Arbitrary bytes must never panic either decoder.
    #[test]
    fn decoder_never_panics(data in prop::collection::vec(any::<u8>(), 0..4096)) {
        let _ = decompress_capped(&data, CAP);
        drain_streaming(&data);
    }

    /// A valid frame, then bit-flips + an optional truncation: these reach the
    /// deepest decoder states (a header parses, then the body is wrong).
    #[test]
    fn mutated_frame_never_panics(
        payload in prop::collection::vec(any::<u8>(), 0..1024),
        flips in prop::collection::vec((any::<u16>(), 0u8..8), 0..8),
        keep in any::<u16>(),
    ) {
        let mut frame = zstd::bulk::compress(&payload, 9).expect("libzstd compress");
        for (at, bit) in flips {
            if frame.is_empty() {
                break;
            }
            let idx = (at as usize) % frame.len();
            frame[idx] ^= 1u8 << bit;
        }
        let n = (keep as usize) % (frame.len() + 1);
        frame.truncate(n);
        let _ = decompress_capped(&frame, CAP);
        drain_streaming(&frame);
    }
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(128))]

    /// Our encoder's output decodes back to the input through both decoders,
    /// across the whole level range and the checksum flag.
    #[test]
    fn encoder_round_trips_both_ways(
        data in prop::collection::vec(any::<u8>(), 0..4096),
        level in 1i32..=22,
        checksum in any::<bool>(),
    ) {
        let frame = our_compress(&data, level, checksum, true);
        let ours = decompress(&frame).expect("our decoder must decode our own frame");
        let theirs = zstd::bulk::decompress(&frame, data.len() + 64)
            .expect("libzstd must decode our frame");
        prop_assert_eq!(ours, data.clone());
        prop_assert_eq!(theirs, data);
    }

    /// Oracle: anything libzstd compresses, our decoder reproduces exactly.
    #[test]
    fn oracle_libzstd_to_us(
        data in prop::collection::vec(any::<u8>(), 0..4096),
        level in 1i32..=19,
    ) {
        let comp = zstd::bulk::compress(&data, level).expect("libzstd compress");
        let got = decompress(&comp).expect("our decoder must decode a libzstd frame");
        prop_assert_eq!(got, data);
    }
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(192))]

    /// Cap monotonicity: if a frame decodes under a ceiling of `n`, it decodes
    /// *identically* under any larger ceiling — more headroom never changes the
    /// result or flips success into failure. (Locks the bomb-cap semantics: the
    /// cap is a pure safety bound, not part of the decode.)
    #[test]
    fn cap_is_monotonic(
        data in prop::collection::vec(any::<u8>(), 0..4096),
        level in 1i32..=19,
        n in 0usize..6000,
        k in 0usize..6000,
    ) {
        let frame = our_compress(&data, level, false, true);
        if let Ok(at_n) = decompress_capped(&frame, n) {
            let larger = decompress_capped(&frame, n.saturating_add(k));
            prop_assert!(larger.is_ok(), "a larger cap must still succeed");
            prop_assert_eq!(at_n, larger.unwrap());
        }
    }

    /// Insufficient cap: a frame whose true output is L > 0 must be refused at a
    /// ceiling of L-1 (with the typed OutputTooLarge), and accepted at exactly L.
    #[test]
    fn cap_one_below_true_size_is_refused(
        data in prop::collection::vec(any::<u8>(), 1..4096),
        level in 1i32..=19,
    ) {
        let frame = our_compress(&data, level, false, true);
        let l = data.len();
        let refused = decompress_capped(&frame, l - 1);
        prop_assert!(
            matches!(refused, Err(ZstdError::OutputTooLarge { .. })),
            "cap L-1 must trip OutputTooLarge, got {refused:?}"
        );
        prop_assert_eq!(decompress_capped(&frame, l).unwrap(), data);
    }

    /// A content checksum means a corrupted frame can never *silently* return wrong
    /// data: a byte flip anywhere makes decode either error or return the exact
    /// original bytes — never some other plausible-looking output.
    #[test]
    fn checksum_frame_never_returns_wrong_data(
        data in prop::collection::vec(any::<u8>(), 0..4096),
        at in any::<u16>(),
    ) {
        let mut frame = our_compress(&data, 9, true, true);
        let idx = (at as usize) % frame.len(); // our_compress never yields an empty frame
        frame[idx] ^= 0xFF;
        if let Ok(out) = decompress_capped(&frame, CAP) {
            prop_assert_eq!(out, data.clone());
        }
    }

    /// Hostile read granularity: a mutated frame drained a few bytes at a time must
    /// still only ever Ok/Err, never panic (the streaming complement to the
    /// tiny-read determinism test on *valid* frames).
    #[test]
    fn mutated_frame_never_panics_at_tiny_reads(
        payload in prop::collection::vec(any::<u8>(), 0..1024),
        flips in prop::collection::vec(any::<u16>(), 0..8),
        read_size in 1usize..8,
    ) {
        let mut frame = zstd::bulk::compress(&payload, 9).expect("libzstd compress");
        for at in flips {
            if frame.is_empty() {
                break;
            }
            let idx = (at as usize) % frame.len();
            frame[idx] ^= 0x55;
        }
        drain_streaming_chunked(&frame, read_size);
    }
}
