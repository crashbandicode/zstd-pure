//! Seekable-format target: (1) parsing *arbitrary* bytes as a seek table must
//! never panic (the untrusted-archive surface), and (2) a `compress_seekable`
//! archive must round-trip — decoded both as a whole multi-frame stream and
//! frame-by-frame through the parsed seek table. This is the coverage gap that
//! kept the seekable API experimental.
#![no_main]

use libfuzzer_sys::fuzz_target;
use zstd_pure::{compress_seekable, decompress, decompress_seekable_frame, SeekTable};

fuzz_target!(|data: &[u8]| {
    // (1) Parsing untrusted bytes as a seek table must only ever Ok/Err, not panic.
    let _ = SeekTable::parse(data);

    // (2) Round-trip our own archive. The frame size is derived from the length
    //     (>= 16 bytes, ~7 frames for larger inputs) so the multi-frame + seek
    //     paths are exercised while the frame count stays bounded.
    let frame_size = (data.len() / 7).max(16);
    let archive = compress_seekable(data, frame_size, 3, true).expect("compress_seekable");

    // Whole-stream decode — a seekable archive is a standard multi-frame stream.
    assert_eq!(decompress(&archive).expect("decode seekable archive"), data, "whole-stream mismatch");

    // Random access: reassemble frame-by-frame via the parsed seek table.
    let table = SeekTable::parse(&archive).expect("our own archive must parse");
    assert_eq!(table.decompressed_size(), data.len() as u64, "seek-table size mismatch");
    let mut out = Vec::with_capacity(data.len());
    for i in 0..table.num_frames() {
        out.extend_from_slice(&decompress_seekable_frame(&archive, &table, i).expect("frame decode"));
    }
    assert_eq!(out, data, "frame-by-frame mismatch");
});
