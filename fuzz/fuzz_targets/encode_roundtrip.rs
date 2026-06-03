//! Encoder round-trip target: compress arbitrary input at an arbitrary level,
//! then require it to decode back to the exact input through BOTH our decoder
//! and libzstd. A 1-byte prefix selects the level (1..=22) and the checksum
//! flag, so a single corpus entry sweeps the whole level range under mutation.
#![no_main]

use libfuzzer_sys::fuzz_target;
use zstd_pure::{compress, decompress};

fuzz_target!(|data: &[u8]| {
    let (level, checksum, payload) = match data.split_first() {
        Some((&b, rest)) => (1 + (b % 22) as i32, b & 0x80 != 0, rest),
        None => (3, false, data),
    };

    let frame = compress(payload, level, checksum, true);

    let ours = decompress(&frame).expect("our decoder must decode our own frame");
    assert_eq!(ours.as_slice(), payload, "our decoder round-trip mismatch");

    let theirs =
        zstd::bulk::decompress(&frame, payload.len() + 64).expect("libzstd must decode our frame");
    assert_eq!(theirs.as_slice(), payload, "libzstd round-trip mismatch");
});
