//! Pure-Rust Zstandard **encoder** (RFC 8878 / RFC 8478).
//!
//! Staged build-out (see `zstd_pure/README.md`):
//!
//! * `block` / `frame` — block + frame writers. Today: store mode (raw / RLE
//!   blocks), which produces a fully spec-conformant frame that libzstd and
//!   this crate's decoder both accept. This is the skeleton the compressed
//!   block type and match finder hang off.
//! * (planned) `fse` / `huff` — entropy encoders (T2.1).
//! * (planned) `sequences` / match finders — the ratio work (T2.3).

pub mod block;
pub mod frame;

pub use frame::compress_store;

/// Compress `data` into a standard (magic-prefixed) store-mode frame. No
/// content checksum. See [`compress_store`] for the full-control entry point.
pub fn compress_stored(data: &[u8]) -> Vec<u8> {
    compress_store(data, false, true)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::zstd_pure::{decompress, frame_header};

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

    #[test]
    fn magicless_store_roundtrips() {
        let data = b"magicless store frame payload that is not too short".repeat(20);
        let frame = compress_store(&data, true, false);
        // Our magicless decoder reads it back.
        let got = crate::zstd_pure::decompress_magicless(&frame, 1 << 20).unwrap();
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
