//! Decoder never-panic / never-OOM target.
//!
//! Arbitrary bytes through every one-shot and streaming decode entry point —
//! magic and magicless (the BFRES form) — must only ever return `Ok`/`Err`.
//! They must never panic and never allocate without bound: the `CAP` ceiling
//! plus libfuzzer's rss limit bound memory against decompression bombs.
#![no_main]

use std::io::Read;

use libfuzzer_sys::fuzz_target;
use zstd_pure::{decompress_capped, decompress_magicless_bytes, StreamingDecoder};

/// 64 MiB output ceiling — bounds memory against decompression bombs.
const CAP: usize = 1 << 26;

/// Read a streaming decoder to completion, capped, swallowing any error.
fn drain<R: Read>(r: R) {
    let mut sink = Vec::new();
    let _ = r.take(CAP as u64).read_to_end(&mut sink);
}

fuzz_target!(|data: &[u8]| {
    // One-shot, both magic handlings.
    let _ = decompress_capped(data, CAP);
    let _ = decompress_magicless_bytes(data, CAP);
    // Streaming (bounded-window), both magic handlings.
    if let Ok(dec) = StreamingDecoder::new(data) {
        drain(dec);
    }
    if let Ok(dec) = StreamingDecoder::new_magicless(data) {
        drain(dec);
    }
});
