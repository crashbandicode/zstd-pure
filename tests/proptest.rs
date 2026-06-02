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

use zstd_pure::{compress as our_compress, decompress, decompress_capped, StreamingDecoder};

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
