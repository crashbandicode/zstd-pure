//! Streaming-encoder round-trip target: drive arbitrary input through
//! `StreamingEncoder` in fuzzer-chosen chunk sizes (plain and long-distance),
//! then require BOTH our decoder and libzstd to recover the exact input. This
//! exercises the stateful block emission, the sliding-window finder/index
//! rebuild, and the LDM streaming path on hostile input and chunkings — the
//! coverage gap that kept `StreamingEncoder` experimental.
#![no_main]

use libfuzzer_sys::fuzz_target;
use zstd_pure::{decompress, StreamingEncoder};

fuzz_target!(|data: &[u8]| {
    // A 2-byte prefix selects the level (1..=22), the checksum + LDM flags, and
    // the push chunk size; the rest is the payload. One corpus entry thus sweeps
    // levels, both encoders, and many write-chunkings under mutation.
    let (level, checksum, ldm, chunk, payload) = match data {
        [a, b, rest @ ..] => (
            1 + (a % 22) as i32,
            a & 0x80 != 0,
            a & 0x40 != 0,
            1 + (*b as usize) * 64, // 1..=16_321-byte writes
            rest,
        ),
        _ => (3, false, false, 256, data),
    };

    let mut enc = if ldm {
        StreamingEncoder::with_options_long(level, checksum, true, 24)
    } else {
        StreamingEncoder::with_options(level, checksum, true)
    };
    for part in payload.chunks(chunk) {
        enc.push(part);
    }
    let frame = enc.finish();

    let ours = decompress(&frame).expect("our decoder must decode our streamed frame");
    assert_eq!(ours.as_slice(), payload, "streaming round-trip mismatch (ours)");

    let theirs = zstd::bulk::decompress(&frame, payload.len() + 64)
        .expect("libzstd must decode our streamed frame");
    assert_eq!(theirs.as_slice(), payload, "streaming round-trip mismatch (libzstd)");
});
