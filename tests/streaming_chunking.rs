//! Streaming decode must be independent of the caller's read granularity.
//!
//! `read_to_end` typically asks for large buffers, so a streaming decoder whose
//! state machine mishandles a block/literals/sequences boundary that lands
//! mid-`read` can still pass the existing round-trip tests. This test drives the
//! decoder one `read()` at a time with deliberately awkward buffer sizes
//! (1, 2, 3, 7, 64, 4096, 65536) and requires the concatenated output to equal
//! the one-shot `decompress` for every size — so a partial read across any
//! internal boundary is exercised. Frames come from both our encoder (standard
//! and magicless) and libzstd, across input profiles that hit raw, RLE, Huffman,
//! and match/sequence paths and span many blocks.

use std::io::Read;

use zstd_pure::{
    compress as our_compress, compress_with_options, decompress, decompress_magicless_bytes,
    CompressOptions, StreamingDecoder,
};

const READ_SIZES: [usize; 7] = [1, 2, 3, 7, 64, 4096, 65536];

/// Small deterministic LCG (no rand dep), matching the style in `corpus.rs`.
struct Rng(u64);
impl Rng {
    fn new(seed: u64) -> Self {
        Rng(seed)
    }
    fn next_u32(&mut self) -> u32 {
        self.0 = self.0.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        ((z ^ (z >> 31)) >> 32) as u32
    }
}

/// Drain a decoder one `read()` at a time, never asking for more than `read_size`
/// bytes — so the decoder must resume correctly across every internal boundary.
fn drain_in_chunks(mut dec: StreamingDecoder<'_>, read_size: usize) -> Vec<u8> {
    let mut out = Vec::new();
    let mut buf = vec![0u8; read_size];
    loop {
        match dec.read(&mut buf).expect("chunked streaming read") {
            0 => break,
            n => out.extend_from_slice(&buf[..n]),
        }
    }
    out
}

/// A spread of profiles exercising raw / RLE / Huffman / match paths, including a
/// large input that spans many blocks so reads land mid-block.
fn profiles() -> Vec<(&'static str, Vec<u8>)> {
    // Sizes are kept modest but still > the 128 KiB block max for `redundant` and
    // `text`, so frames span multiple blocks and reads land mid-block. (Our encoder
    // is not yet perf-tuned, so a single large high-level encode dominates runtime —
    // libzstd, which is fast, carries the level-19 structures below.)
    let mut rng = Rng::new(0x5EED_C0FF_EE12_3456); // arbitrary fixed seed
    let random: Vec<u8> = (0..150_000).map(|_| rng.next_u32() as u8).collect();
    let redundant: Vec<u8> = (0..40_000u32).flat_map(|i| (i % 7).to_le_bytes()).collect();
    let text = "the quick brown fox jumps over the lazy dog. "
        .repeat(3200)
        .into_bytes();
    vec![
        ("empty", Vec::new()),
        ("tiny", b"ab".to_vec()),
        ("rle", vec![0x5Au8; 90_000]),
        ("redundant", redundant),
        ("text", text),
        ("random", random),
    ]
}

#[test]
fn streaming_decode_is_independent_of_read_size_standard() {
    for (name, data) in profiles() {
        for &level in &[1i32, 9] {
            let frame = our_compress(&data, level, level % 2 == 1, true);
            let one_shot = decompress(&frame).expect("one-shot decode");
            assert_eq!(one_shot, data, "[{name} L{level}] one-shot sanity");
            for &rs in &READ_SIZES {
                let dec = StreamingDecoder::new(&frame).expect("construct decoder");
                let chunked = drain_in_chunks(dec, rs);
                assert_eq!(
                    chunked, data,
                    "[{name} L{level}] read_size={rs} != one-shot"
                );
            }
        }
    }
}

#[test]
fn streaming_decode_is_independent_of_read_size_libzstd() {
    for (name, data) in profiles() {
        for &level in &[1i32, 6, 19] {
            let frame = zstd::bulk::compress(&data, level).expect("libzstd compress");
            let one_shot = decompress(&frame).expect("one-shot decode");
            for &rs in &READ_SIZES {
                let dec = StreamingDecoder::new(&frame).expect("construct decoder");
                let chunked = drain_in_chunks(dec, rs);
                assert_eq!(
                    chunked, one_shot,
                    "[{name} L{level}] libzstd frame read_size={rs} != one-shot"
                );
            }
        }
    }
}

#[test]
fn streaming_decode_is_independent_of_read_size_magicless() {
    for (name, data) in profiles() {
        let frame = compress_with_options(&data, &CompressOptions::new(9).checksum(true).magic(false));
        let one_shot = decompress_magicless_bytes(&frame, data.len() + 64).expect("magicless one-shot");
        assert_eq!(one_shot, data, "[{name}] magicless one-shot sanity");
        for &rs in &READ_SIZES {
            let dec = StreamingDecoder::new_magicless(&frame).expect("construct magicless decoder");
            let chunked = drain_in_chunks(dec, rs);
            assert_eq!(
                chunked, data,
                "[{name}] magicless read_size={rs} != one-shot"
            );
        }
    }
}
