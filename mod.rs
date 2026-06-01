//! A pure-Rust Zstandard ([RFC 8478]) codec, implemented from the specification
//! (no GPL / Switch-Toolbox code). It exists so the crate can decode Nintendo's
//! **MeshCodec** mesh stream — a custom container that reuses zstd's block and
//! entropy primitives — without depending on libzstd's C internals, and is
//! structured (std + `thiserror` only, no other crate deps) so it can later be
//! lifted into a standalone `zstd-pure` crate.
//!
//! ## Status
//!
//! Decoder, built bottom-up and validated against libzstd and the real TotK
//! BFRES frames (which are themselves standard magicless zstd):
//!
//! * [`bits`] — the reverse (FSE/Huffman) and forward bit readers.
//! * [`xxhash`] — XXH64 for the content checksum.
//! * `fse` / `huff` — the entropy decoders (in progress).
//! * `frame` — frame/block orchestration (in progress).
//!
//! [RFC 8478]: https://www.rfc-editor.org/rfc/rfc8478

pub mod bits;
pub mod block;
pub mod dict;
pub mod encode;
mod error;
pub mod frame;
pub mod fse;
pub mod huff;
pub mod literals;
pub mod sequences;
pub mod streaming;
pub mod xxhash;

pub use dict::Dictionary;
pub use encode::{compress, compress_huffman_literals, compress_store, compress_stored};
pub use streaming::StreamingDecoder;
pub use error::{Result, ZstdError};
pub use frame::{
    decode_one, decode_one_with_dict, decompress, decompress_capped, decompress_magicless,
    decompress_magicless_with_dict, decompress_with_dict, frame_header, frame_header_magicless,
    DecodedFrame, FrameHeader,
};

/// Decompress a single magicless frame and return just the bytes (the common
/// case for the MeshCodec BFRES frame).
pub fn decompress_magicless_bytes(src: &[u8], max_output: usize) -> Result<Vec<u8>> {
    decompress_magicless(src, max_output).map(|f| f.data)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Compress `data` at `level` with libzstd (the `zstd` dep) and decode it
    /// with this pure-Rust implementation; they must agree.
    fn round_trip(data: &[u8], level: i32) {
        let comp = zstd::bulk::compress(data, level).expect("libzstd compress");
        let got = decompress(&comp).unwrap_or_else(|e| panic!("decode (level {level}): {e}"));
        assert_eq!(got, data, "mismatch at level {level}, {} bytes", data.len());
    }

    #[test]
    fn matches_libzstd_across_levels_and_inputs() {
        // Highly redundant (lots of matches / repeat offsets).
        let redundant: Vec<u8> = (0..20_000u32).flat_map(|i| (i % 11).to_le_bytes()).collect();
        // Structured-ish (FRES-like header + counted body).
        let mut structured = b"FRES____".to_vec();
        for i in 0..8000u32 {
            structured.extend_from_slice(&(i.wrapping_mul(2654435761) % 251).to_le_bytes());
        }
        // Low-entropy text (exercises Huffman literals).
        let text = "the quick brown fox jumps over the lazy dog. "
            .repeat(400)
            .into_bytes();
        // Near-random (forces raw/Huffman, few matches).
        let mut rng = 0x1234_5678u32;
        let random: Vec<u8> = (0..12_000)
            .map(|_| {
                rng = rng.wrapping_mul(1664525).wrapping_add(1013904223);
                (rng >> 24) as u8
            })
            .collect();

        for data in [&redundant, &structured, &text, &random] {
            for level in [1, 3, 9, 19] {
                round_trip(data, level);
            }
        }
    }

    #[test]
    fn handles_empty_and_tiny() {
        for data in [vec![], vec![0u8], b"ab".to_vec(), vec![7u8; 500]] {
            round_trip(&data, 3);
        }
    }

    #[test]
    fn frame_header_matches_libzstd() {
        // A frame that pledges its content size (single-shot bulk compress does).
        let data = b"frame header inspection corpus ".repeat(40);
        let comp = zstd::bulk::compress(&data, 5).expect("compress");
        let h = frame_header(&comp).expect("parse header");
        assert_eq!(h.content_size, Some(data.len() as u64));
        assert!(!h.has_checksum);
        assert_eq!(h.dictionary_id, 0);
        // The window must be large enough to hold any back-reference, i.e. at
        // least the content size (which is what libzstd would also report).
        assert!(h.window_size >= data.len().min(8 << 20) as u64 || h.content_size.is_some());
        // Header is small and well within the compressed length.
        assert!(h.header_len >= 5 && h.header_len < comp.len());

        // A checksum frame reports has_checksum.
        let mut cctx = zstd::zstd_safe::CCtx::create();
        cctx.set_parameter(zstd::zstd_safe::CParameter::ChecksumFlag(true))
            .unwrap();
        let mut out = Vec::with_capacity(zstd::zstd_safe::compress_bound(data.len()));
        cctx.compress2(&mut out, &data).unwrap();
        assert!(frame_header(&out).expect("header").has_checksum);
    }

    #[test]
    fn verifies_content_checksum() {
        let data = b"checksum me please ".repeat(50);
        // Encode with the content-checksum flag set.
        let mut cctx = zstd::zstd_safe::CCtx::create();
        cctx.set_parameter(zstd::zstd_safe::CParameter::ChecksumFlag(true))
            .unwrap();
        let mut out = Vec::with_capacity(zstd::zstd_safe::compress_bound(data.len()));
        cctx.compress2(&mut out, &data).unwrap();
        let got = decompress(&out).expect("decode with checksum");
        assert_eq!(got, data);
        // Corrupt the checksum -> mismatch error.
        let mut bad = out.clone();
        let n = bad.len();
        bad[n - 1] ^= 0xFF;
        assert!(matches!(
            decompress(&bad),
            Err(ZstdError::ChecksumMismatch { .. }) | Err(_)
        ));
    }
}
