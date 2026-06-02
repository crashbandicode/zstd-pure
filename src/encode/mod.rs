//! Pure-Rust Zstandard **encoder** (RFC 8878 / RFC 8478).
//!
//! Staged build-out (see `README.md`):
//!
//! * `block` / `frame` — block + frame writers. Store mode (raw / RLE blocks)
//!   plus a Huffman-literals compressed block (`[Huffman literals][0
//!   sequences]`), both producing fully spec-conformant frames that libzstd and
//!   this crate's decoder accept. This is the skeleton the match finder hangs
//!   off.
//! * `huff` — Huff0 literal **encoder** (T2.1a).
//! * (planned) `fse` — FSE encoder (T2.1b); `sequences` / match finders — the
//!   ratio work (T2.3).

#[allow(unused_imports)]
use crate::alloc_prelude::*;
pub mod bitstream;
pub mod block;
pub mod frame;
pub mod fse;
pub mod huff;
pub mod lz;
pub mod sequences;

pub use frame::{compress, compress_huffman_literals, compress_store};

/// Compress `data` into a standard (magic-prefixed) store-mode frame. No
/// content checksum. See [`compress_store`] for the full-control entry point.
pub fn compress_stored(data: &[u8]) -> Vec<u8> {
    compress_store(data, false, true)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{decompress, frame_header};

    /// A store-mode frame must round-trip through BOTH libzstd and our decoder.
    fn assert_store_roundtrips(data: &[u8], checksum: bool) {
        let frame = compress_store(data, checksum, true);
        // libzstd decodes it (proves the output is spec-correct).
        let by_libzstd = zstd::bulk::decompress(&frame, data.len() + 64)
            .expect("libzstd must decode our store frame");
        assert_eq!(by_libzstd, data, "libzstd mismatch ({} bytes)", data.len());
        // Our own decoder decodes it (self-consistency).
        assert_eq!(decompress(&frame).unwrap(), data, "self mismatch");
        // The pledged content size is visible without decoding.
        assert_eq!(frame_header(&frame).unwrap().content_size, Some(data.len() as u64));
    }

    #[test]
    fn store_roundtrips_across_sizes() {
        // Empty, tiny, an all-same run (exercises RLE), and multi-block.
        let big: Vec<u8> = (0..400_000u32)
            .map(|i| (i.wrapping_mul(2654435761) >> 16) as u8)
            .collect();
        let cases: Vec<Vec<u8>> = vec![
            vec![],
            vec![0u8],
            b"hello world".to_vec(),
            vec![0xAB; 100_000],         // single RLE block
            vec![0x7F; 300_000],         // multi-block RLE (chunked at 128 KiB)
            big,                         // multi-block raw
        ];
        for data in &cases {
            assert_store_roundtrips(data, false);
            assert_store_roundtrips(data, true);
        }
    }

    #[test]
    fn rle_block_is_used_for_runs() {
        // A 100 KiB run must encode far smaller than raw (1 payload byte/block).
        let data = vec![0x42u8; 100_000];
        let frame = compress_store(&data, false, true);
        assert!(frame.len() < 64, "RLE run should be tiny, got {}", frame.len());
        assert_eq!(decompress(&frame).unwrap(), data);
    }

    /// Deterministic skewed byte stream over a restricted alphabet (≤ 128 so the
    /// direct Huffman weight header applies).
    fn skewed(n: usize, alphabet: u32, seed: u64) -> Vec<u8> {
        let mut s = seed | 1;
        (0..n)
            .map(|_| {
                s = s
                    .wrapping_mul(6364136223846793005)
                    .wrapping_add(1442695040888963407);
                let u = (s >> 33) as u32 % alphabet;
                ((u * u) / alphabet) as u8
            })
            .collect()
    }

    /// A Huffman-literals frame must round-trip through BOTH libzstd and us, and
    /// it must never be larger than the store-mode frame.
    fn assert_huffman_roundtrips(data: &[u8], checksum: bool) {
        let frame = compress_huffman_literals(data, checksum, true);
        let by_libzstd = zstd::bulk::decompress(&frame, data.len() + 64)
            .expect("libzstd must decode our Huffman frame");
        assert_eq!(by_libzstd, data, "libzstd mismatch ({} bytes)", data.len());
        assert_eq!(decompress(&frame).unwrap(), data, "self mismatch");
        assert!(
            frame.len() <= compress_store(data, checksum, true).len(),
            "Huffman frame ({}) larger than store ({})",
            frame.len(),
            compress_store(data, checksum, true).len(),
        );
    }

    #[test]
    fn huffman_literals_roundtrips_across_sizes() {
        let cases: Vec<Vec<u8>> = vec![
            vec![],
            vec![0u8],
            b"hello world".to_vec(),
            b"the quick brown fox jumps over the lazy dog".to_vec(),
            skewed(50, 40, 1),
            skewed(300, 90, 2),     // crosses into the 4-stream path
            skewed(2000, 120, 3),   // 4-stream, larger header
            skewed(60_000, 128, 4), // full direct-weight alphabet, big block
        ];
        for data in &cases {
            assert_huffman_roundtrips(data, false);
            assert_huffman_roundtrips(data, true);
        }
    }

    #[test]
    fn huffman_uniform_full_byte_alphabet_is_valid() {
        // A near-uniform full-byte alphabet won't compress; the encoder either
        // stores it or FSE-codes the weights — either way a valid frame.
        let data: Vec<u8> = (0..4096u32).map(|i| (i * 7 + 3) as u8).collect();
        assert_huffman_roundtrips(&data, false);
    }

    #[test]
    fn huffman_fse_weights_full_byte_alphabet_roundtrips() {
        // Skewed distribution spanning the full byte range (highest symbol >
        // 128) exercises the FSE-compressed weight header through libzstd.
        let mut data = Vec::new();
        for i in 0..20_000u32 {
            data.push((i % 24) as u8);
        }
        for k in 0..1500u32 {
            data.push((130 + (k * 13) % 120) as u8);
        }
        assert_huffman_roundtrips(&data, false);
        assert_huffman_roundtrips(&data, true);
    }

    /// A compressed frame must round-trip through BOTH libzstd and our decoder.
    fn assert_compress_roundtrips(data: &[u8], checksum: bool) {
        let frame = compress(data, 3, checksum, true);
        let by_libzstd = zstd::bulk::decompress(&frame, data.len() + 64)
            .expect("libzstd must decode our compressed frame");
        assert_eq!(by_libzstd, data, "libzstd mismatch ({} bytes)", data.len());
        assert_eq!(decompress(&frame).unwrap(), data, "self mismatch");
        assert!(
            frame.len() <= compress_store(data, checksum, true).len(),
            "compressed ({}) larger than store ({})",
            frame.len(),
            compress_store(data, checksum, true).len(),
        );
    }

    #[test]
    fn compress_roundtrips_across_inputs() {
        let text = b"the quick brown fox jumps over the lazy dog. ".repeat(200);
        let big: Vec<u8> = (0..300_000u32)
            .map(|i| (i.wrapping_mul(2654435761) >> 16) as u8)
            .collect();
        // Fixed-stride recurring token + changing interstitial bytes: a
        // repeat-offset stress case, sized past one block to exercise the
        // cross-block `rep` threading. ~360 KiB.
        let rep_structured: Vec<u8> = (0..30_000u32)
            .flat_map(|i| {
                let mut u = b"MARKER__".to_vec();
                u.extend_from_slice(&i.to_le_bytes());
                u
            })
            .collect();
        let cases: Vec<Vec<u8>> = vec![
            vec![],
            vec![0u8],
            b"abc".to_vec(),
            b"abcabcabcabcabcabcabc".to_vec(),
            vec![0x55; 200_000], // long run -> offset-1 matches, multi-block
            text,
            rep_structured, // repeat-offset codes across block boundaries
            skewed(50_000, 64, 7),
            big, // mostly incompressible -> store blocks
        ];
        for data in &cases {
            assert_compress_roundtrips(data, false);
            assert_compress_roundtrips(data, true);
        }
    }

    #[test]
    fn compress_actually_shrinks_compressible_data() {
        let data = b"the quick brown fox jumps over the lazy dog. ".repeat(500);
        let frame = compress(&data, 3, false, true);
        // Highly repetitive text should compress to a small fraction.
        assert!(
            frame.len() < data.len() / 3,
            "expected real compression, got {} from {}",
            frame.len(),
            data.len()
        );
        assert_eq!(decompress(&frame).unwrap(), data);
    }

    #[test]
    fn magicless_store_roundtrips() {
        let data = b"magicless store frame payload that is not too short".repeat(20);
        let frame = compress_store(&data, true, false);
        // Our magicless decoder reads it back.
        let got = crate::decompress_magicless(&frame, 1 << 20).unwrap();
        assert_eq!(got.data, data);
        // libzstd reads it with the magicless frame format too.
        let mut dctx = zstd::zstd_safe::DCtx::create();
        dctx.set_parameter(zstd::zstd_safe::DParameter::Format(
            zstd::zstd_safe::FrameFormat::Magicless,
        ))
        .unwrap();
        let mut out = vec![0u8; data.len()];
        let n = dctx.decompress(&mut out, &frame).unwrap();
        assert_eq!(&out[..n], &data[..]);
    }
}
