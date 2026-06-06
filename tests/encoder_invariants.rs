//! Encoder structural invariants: parse our *own* emitted frames and assert the
//! RFC 8878 format-validity properties that hold for ANY conformant encoder,
//! independent of compression ratio or parse strategy. Round-trip tests prove the
//! bytes decode; these prove the framing/metadata is well-formed:
//!
//! - magic present iff magic mode requested (magicless otherwise);
//! - Frame_Header_Descriptor reserved bit clear;
//! - checksum flag matches the requested mode (and 4 trailing bytes exist);
//! - content size pledged exactly when expected (one-shot yes, streaming no) and
//!   equal to the true length;
//! - dictionary id matches the dictionary used;
//! - every block is type raw/RLE/compressed (never reserved 3), no block exceeds
//!   Block_Maximum_Size = min(Window_Size, 128 KiB), no block runs past EOF, and
//!   the last-block flag appears exactly once as the final block.
//!
//! These are deliberately ratio-independent, so they stay green while the encoder's
//! match-finding / parse quality evolves.

use zstd_pure::{
    compress, compress_long, compress_parallel, compress_seekable, compress_with_dict,
    compress_with_options, decompress, decompress_magicless_bytes, decompress_with_dict,
    frame_header, frame_header_magicless, CompressOptions, Dictionary, FrameHeader,
    StreamingEncoder,
};

const MAGIC: [u8; 4] = [0x28, 0xB5, 0x2F, 0xFD];
/// RFC 8878 §3.1.1.2: a block's size never exceeds min(Window_Size, 128 KiB).
const BLOCK_MAX_CEIL: u64 = 128 * 1024;

/// A spread of inputs hitting empty / tiny / Huffman-literal / match paths, plus
/// one input large enough to span multiple 128 KiB blocks (so the block-size cap
/// is exercised on real multi-block frames).
fn small_profiles() -> Vec<(&'static str, Vec<u8>)> {
    let text = "the quick brown fox jumps over the lazy dog. "
        .repeat(80)
        .into_bytes();
    let structured: Vec<u8> = (0..4000u32)
        .flat_map(|i| (i.wrapping_mul(2654435761) % 251).to_le_bytes())
        .collect();
    vec![
        ("empty", Vec::new()),
        ("tiny", b"ab".to_vec()),
        ("text", text),
        ("structured", structured),
    ]
}

/// ~200 KiB of low-redundancy bytes → a multi-block frame.
fn big_input() -> Vec<u8> {
    (0..200_000u32)
        .map(|i| (i.wrapping_mul(2654435761) >> 13) as u8)
        .collect()
}

/// Walk one zstd frame at the front of `buf`, asserting the structural invariants.
/// Returns (bytes consumed by the frame incl. checksum, parsed header).
fn validate_frame(buf: &[u8], magicless: bool, label: &str) -> (usize, FrameHeader) {
    let header = if magicless {
        frame_header_magicless(buf)
    } else {
        assert!(buf.len() >= 4 && buf[..4] == MAGIC, "[{label}] missing frame magic");
        frame_header(buf)
    }
    .unwrap_or_else(|e| panic!("[{label}] header parse failed: {e}"));

    // Frame_Header_Descriptor reserved bit (3) must be clear.
    let fhd = buf[if magicless { 0 } else { 4 }];
    assert_eq!(fhd & 0x08, 0, "[{label}] reserved FHD bit set");

    let block_max = header.window_size.min(BLOCK_MAX_CEIL) as usize;
    let mut pos = header.header_len;
    let mut blocks = 0usize;
    loop {
        assert!(pos + 3 <= buf.len(), "[{label}] block header runs past EOF");
        let v = (buf[pos] as u32) | ((buf[pos + 1] as u32) << 8) | ((buf[pos + 2] as u32) << 16);
        let last = (v & 1) != 0;
        let btype = ((v >> 1) & 3) as u8;
        let bsize = (v >> 3) as usize;
        pos += 3;
        assert!(btype != 3, "[{label}] reserved block type 3 emitted");
        assert!(
            bsize <= block_max,
            "[{label}] block size {bsize} exceeds Block_Maximum_Size {block_max}"
        );
        let body = if btype == 1 { 1 } else { bsize }; // an RLE block body is one byte
        assert!(pos + body <= buf.len(), "[{label}] block body runs past EOF");
        pos += body;
        blocks += 1;
        if last {
            break; // exactly one last block; anything after it is the checksum only
        }
    }
    assert!(blocks >= 1, "[{label}] frame contained no blocks");
    if header.has_checksum {
        assert!(
            pos + 4 <= buf.len(),
            "[{label}] checksum flag set but no 4 trailing bytes"
        );
        pos += 4;
    }
    (pos, header)
}

/// Validate a single-frame buffer end to end and return the parsed header.
fn check_single(
    frame: &[u8],
    magicless: bool,
    checksum: bool,
    dict_id: u32,
    content_size: Option<u64>,
    label: &str,
) -> FrameHeader {
    let (consumed, h) = validate_frame(frame, magicless, label);
    assert_eq!(consumed, frame.len(), "[{label}] trailing bytes after a single frame");
    assert_eq!(h.has_checksum, checksum, "[{label}] checksum flag mismatch");
    assert_eq!(h.dictionary_id, dict_id, "[{label}] dictionary id mismatch");
    assert_eq!(h.content_size, content_size, "[{label}] content-size pledge mismatch");
    h
}

/// Walk a multi-frame stream (skipping skippable frames), validating each zstd
/// frame's structure; returns the number of zstd data frames seen.
fn validate_stream(buf: &[u8], label: &str) -> usize {
    let mut pos = 0usize;
    let mut frames = 0usize;
    while pos < buf.len() {
        assert!(pos + 4 <= buf.len(), "[{label}] dangling bytes at stream end");
        let m = u32::from_le_bytes([buf[pos], buf[pos + 1], buf[pos + 2], buf[pos + 3]]);
        if (0x184D_2A50..=0x184D_2A5F).contains(&m) {
            assert!(pos + 8 <= buf.len(), "[{label}] truncated skippable header");
            let size = u32::from_le_bytes([buf[pos + 4], buf[pos + 5], buf[pos + 6], buf[pos + 7]])
                as usize;
            pos += 8 + size;
            assert!(pos <= buf.len(), "[{label}] skippable frame runs past EOF");
        } else {
            let (consumed, _) = validate_frame(&buf[pos..], false, label);
            pos += consumed;
            frames += 1;
        }
    }
    assert_eq!(pos, buf.len(), "[{label}] stream walk overran the buffer");
    frames
}

#[test]
fn compress_emits_structurally_valid_frames() {
    for (name, data) in small_profiles() {
        for &level in &[1i32, 9, 19] {
            for &checksum in &[false, true] {
                for &magic in &[true, false] {
                    let label = format!("compress {name} L{level} ck{checksum} magic{magic}");
                    let frame = compress(&data, level, checksum, magic);
                    check_single(
                        &frame,
                        !magic,
                        checksum,
                        0,
                        Some(data.len() as u64),
                        &label,
                    );
                    let decoded = if magic {
                        decompress(&frame).unwrap()
                    } else {
                        decompress_magicless_bytes(&frame, data.len() + 64).unwrap()
                    };
                    assert_eq!(decoded, data, "[{label}] round-trip");
                }
            }
        }
    }
    // A multi-block frame: the Block_Maximum_Size cap must hold on real blocks.
    let big = big_input();
    for &level in &[1i32, 3] {
        let label = format!("compress big L{level}");
        let frame = compress(&big, level, true, true);
        check_single(&frame, false, true, 0, Some(big.len() as u64), &label);
        assert_eq!(decompress(&frame).unwrap(), big, "[{label}] round-trip");
    }
}

#[test]
fn compress_with_options_magicless_is_well_formed() {
    let data = big_input();
    let opts = CompressOptions::new(9).checksum(true).magic(false).window_log(18);
    let frame = compress_with_options(&data, &opts);
    check_single(&frame, true, true, 0, Some(data.len() as u64), "options magicless");
    assert_eq!(
        decompress_magicless_bytes(&frame, data.len() + 64).unwrap(),
        data
    );
}

#[test]
fn compress_long_is_well_formed() {
    let data = big_input();
    for &checksum in &[false, true] {
        let label = format!("compress_long ck{checksum}");
        let frame = compress_long(&data, 1, checksum, true);
        let h = check_single(&frame, false, checksum, 0, Some(data.len() as u64), &label);
        // LDM advertises a larger window, but a block still never exceeds 128 KiB.
        assert!(h.window_size >= data.len() as u64, "[{label}] window covers content");
        assert_eq!(decompress(&frame).unwrap(), data, "[{label}] round-trip");
    }
}

#[test]
fn compress_parallel_emits_valid_frames() {
    let data = big_input();
    for &jobs in &[2usize, 4] {
        let label = format!("compress_parallel jobs{jobs}");
        let buf = compress_parallel(&data, 3, jobs, true, true);
        let frames = validate_stream(&buf, &label);
        assert!(frames >= 1, "[{label}] produced no frames");
        assert_eq!(decompress(&buf).unwrap(), data, "[{label}] round-trip");
    }
}

#[test]
fn streaming_encoder_emits_valid_frames_without_a_content_size() {
    for (name, data) in small_profiles() {
        for &checksum in &[false, true] {
            let label = format!("streaming {name} ck{checksum}");
            let mut enc = StreamingEncoder::with_options(9, checksum, true);
            for part in data.chunks(1000).filter(|c| !c.is_empty()) {
                enc.push(part);
            }
            let frame = enc.finish();
            // A streaming encoder does not know the total up front: no content size.
            check_single(&frame, false, checksum, 0, None, &label);
            assert_eq!(decompress(&frame).unwrap(), data, "[{label}] round-trip");
        }
    }
}

#[test]
fn compress_with_dict_carries_the_dictionary_id() {
    let data = b"shared-prefix record payload that the dictionary should help with ".repeat(40);

    // Raw dictionary => id 0 (no dictionary id field in the frame).
    let raw = Dictionary::raw(b"shared-prefix record payload");
    let frame = compress_with_dict(&data, &raw, 9, true, true);
    check_single(&frame, false, true, 0, Some(data.len() as u64), "dict raw");
    assert_eq!(
        decompress_with_dict(&frame, &raw, data.len() + 64).unwrap(),
        data
    );

    // Structured (libzstd-trained) dictionary => the frame must reference its id.
    let samples: Vec<Vec<u8>> = (0..400u32)
        .map(|i| format!("{{\"id\":{i},\"kind\":\"npc_{}\",\"hp\":{}}}", i % 31, (i * 7) % 200).into_bytes())
        .collect();
    let trained = zstd::dict::from_samples(&samples, 16 * 1024).expect("train dict");
    let dict = Dictionary::parse(&trained).expect("parse trained dict");
    assert_ne!(dict.id(), 0, "a libzstd-trained dict carries a nonzero id");
    let frame = compress_with_dict(&data, &dict, 9, false, true);
    check_single(&frame, false, false, dict.id(), Some(data.len() as u64), "dict structured");
    assert_eq!(
        decompress_with_dict(&frame, &dict, data.len() + 64).unwrap(),
        data
    );
}

#[test]
fn compress_seekable_emits_valid_frames() {
    let data = big_input();
    let archive = compress_seekable(&data, 40_000, 3, true).expect("compress_seekable");
    // The archive is a multi-frame stream of standard zstd data frames plus the
    // skippable seek-table frame; every data frame must be structurally valid.
    let frames = validate_stream(&archive, "seekable");
    assert!(frames >= 2, "expected several data frames, saw {frames}");
    assert_eq!(decompress(&archive).unwrap(), data, "seekable whole-stream round-trip");
}
