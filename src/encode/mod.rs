//! Pure-Rust Zstandard **encoder** (RFC 8878).
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
pub mod params;
pub mod sequences;
pub mod train;

pub use frame::{compress, compress_huffman_literals, compress_store, compress_with_dict};
pub use train::{train_dictionary, train_dictionary_structured};

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

    /// Compressed frame at an explicit level must round-trip through BOTH
    /// libzstd and our decoder, and never exceed the store encoding.
    fn assert_compress_roundtrips_at(data: &[u8], level: i32, checksum: bool) {
        let frame = compress(data, level, checksum, true);
        let by_libzstd = zstd::bulk::decompress(&frame, data.len() + 64)
            .unwrap_or_else(|e| panic!("libzstd decode (L{level}, {} bytes): {e}", data.len()));
        assert_eq!(by_libzstd, data, "libzstd mismatch L{level} ({} bytes)", data.len());
        assert_eq!(decompress(&frame).unwrap(), data, "self mismatch L{level}");
        assert!(
            frame.len() <= compress_store(data, checksum, true).len(),
            "L{level} compressed ({}) larger than store ({})",
            frame.len(),
            compress_store(data, checksum, true).len(),
        );
    }

    #[test]
    fn compress_roundtrips_across_levels() {
        // Levels 1-3 use the fast finder; 4-12 the greedy/lazy/lazy2 chain
        // finder; 13+ map to lazy2 for now. Exercise all of them — including a
        // >128 KiB input so the chain finder runs across block boundaries — and
        // both compressible and incompressible data.
        let text = b"the quick brown fox jumps over the lazy dog. ".repeat(120);
        let structured: Vec<u8> = (0..40_000u32)
            .map(|i| (i.wrapping_mul(2654435761) >> 11) as u8)
            .collect();
        let json: Vec<u8> = (0..2000u32)
            .flat_map(|i| format!("{{\"id\":{i},\"k\":\"v_{}\"}}\n", i % 39).into_bytes())
            .collect();
        // ~168 KiB — still spans >1 block so the finders run across the
        // boundary, but kept modest so the L19/22 optimal parse stays quick.
        let big_rep: Vec<u8> = (0..8_000u32)
            .flat_map(|i| {
                let mut u = b"REC_".to_vec();
                u.extend_from_slice(&i.to_le_bytes());
                u.extend_from_slice(b"....const....");
                u
            })
            .collect();
        // Highly periodic, multi-block: the case that first exposed an
        // offset-0 self-match bug in the optimal parser (L13+).
        let periodic: Vec<u8> = (0..40_000u32).flat_map(|i| (i % 13).to_le_bytes()).collect();
        let cases: Vec<Vec<u8>> = vec![
            b"abcabcabc".to_vec(),
            text,
            structured,
            json,
            vec![7u8; 5000],
            big_rep,
            periodic,
        ];
        for data in &cases {
            for &level in &[1i32, 2, 4, 6, 9, 12, 19, 22] {
                assert_compress_roundtrips_at(data, level, level % 4 == 0);
            }
        }
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
    fn cross_block_matches_span_boundaries() {
        // Three copies of a 100 KiB incompressible chunk. The 2nd and 3rd copies
        // live in later 128 KiB blocks and can only be compressed by referencing
        // the first copy across the block boundary (offset ~100 KiB, within the
        // level-selected window). Block-local matching would barely shrink this;
        // cross-block it collapses to roughly one chunk.
        let chunk: Vec<u8> = (0..100_000u32)
            .map(|i| (i.wrapping_mul(2654435761) >> 13) as u8)
            .collect();
        let data = chunk.repeat(3);
        let frame = compress(&data, 3, false, true);
        assert!(
            frame.len() < data.len() / 2,
            "cross-block matching should collapse repeats: {} from {}",
            frame.len(),
            data.len()
        );
        // Decodes through libzstd (proving the offsets stay within the
        // advertised window) and through our own decoder.
        let by_libzstd = zstd::bulk::decompress(&frame, data.len() + 64).unwrap();
        assert_eq!(by_libzstd, data, "libzstd mismatch");
        assert_eq!(decompress(&frame).unwrap(), data, "self mismatch");
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

#[cfg(test)]
mod dict_tests {
    use super::*;
    use crate::{decompress_with_dict, frame_header, Dictionary};

    /// Round-trip `data`, compressed with our [`compress_with_dict`] + the
    /// `dict_bytes` buffer, through BOTH libzstd (loaded with the same
    /// dictionary — proves the output is spec-correct) and our own dictionary
    /// decoder (self-consistency).
    fn assert_dict_roundtrips(data: &[u8], dict_bytes: &[u8], level: i32) {
        let dict = Dictionary::parse(dict_bytes).expect("parse dict");
        let frame = compress_with_dict(data, &dict, level, false, true);

        let mut dec = zstd::bulk::Decompressor::with_dictionary(dict_bytes)
            .expect("libzstd decompressor with dict");
        let by_libzstd = dec
            .decompress(&frame, data.len() + 64)
            .unwrap_or_else(|e| panic!("libzstd dict decode (L{level}, {} bytes): {e}", data.len()));
        assert_eq!(by_libzstd, data, "libzstd dict mismatch L{level} ({} bytes)", data.len());

        let by_us = decompress_with_dict(&frame, &dict, data.len() + 64)
            .unwrap_or_else(|e| panic!("self dict decode (L{level}): {e}"));
        assert_eq!(by_us, data, "self dict mismatch L{level} ({} bytes)", data.len());
    }

    #[test]
    fn raw_content_dict_encode_round_trips() {
        // A no-magic buffer is a raw-content dictionary (both libzstd and our
        // parser treat it that way). Sharing substrings with the input makes it
        // pull its weight.
        let dict = b"the quick brown fox jumps over the lazy dog. ".repeat(20);
        let data = b"the quick brown fox is feeling very lazy today. ".repeat(60);
        for level in [1, 3, 6, 9, 19, 22] {
            assert_dict_roundtrips(&data, &dict, level);
        }
        // Edge cases: empty and sub-min-match inputs (no sequences emitted).
        assert_dict_roundtrips(&[], &dict, 3);
        assert_dict_roundtrips(b"x", &dict, 3);
        assert_dict_roundtrips(b"the", &dict, 19);
    }

    /// Many small related records — the realistic dictionary use case.
    fn small_records() -> Vec<Vec<u8>> {
        (0..600u32)
            .map(|i| {
                format!(
                    "{{\"id\":{i},\"name\":\"item_{}\",\"kind\":\"weapon\",\"atk\":{},\"price\":{}}}\n",
                    i % 41,
                    (i * 7) % 200,
                    (i * 13) % 5000
                )
                .into_bytes()
            })
            .collect()
    }

    #[test]
    fn structured_dict_encode_round_trips() {
        let samples = small_records();
        let dict_bytes = zstd::dict::from_samples(&samples, 8 * 1024).expect("train dict");
        // A trained dict is structured: magic + entropy + non-zero id, so this
        // exercises the seeded repeat offsets and the dict-id frame header field.
        let dict = Dictionary::parse(&dict_bytes).expect("parse trained dict");
        assert!(dict.entropy().is_some(), "trained dict must be structured");
        assert_ne!(dict.id(), 0);
        for s in samples.iter().take(60) {
            for level in [1, 3, 9, 19] {
                assert_dict_roundtrips(s, &dict_bytes, level);
            }
        }
    }

    #[test]
    fn dictionary_improves_ratio_on_many_small_files() {
        let samples = small_records();
        let dict_bytes = zstd::dict::from_samples(&samples, 8 * 1024).expect("train dict");
        let dict = Dictionary::parse(&dict_bytes).expect("parse dict");
        // Each record is tiny, so without a dictionary there's almost nothing to
        // match; with one, every record references shared structure in the dict.
        // The dict-primed total must be clearly smaller across the corpus.
        for &level in &[3, 19] {
            let no_dict: usize = samples.iter().map(|s| compress(s, level, false, true).len()).sum();
            let with_dict: usize = samples
                .iter()
                .map(|s| compress_with_dict(s, &dict, level, false, true).len())
                .sum();
            assert!(
                with_dict < no_dict,
                "dictionary should shrink a many-small-files corpus at L{level}: \
                 {with_dict} (dict) vs {no_dict} (none)"
            );
        }
    }

    #[test]
    fn our_trained_dict_improves_ratio_and_round_trips() {
        // Train a raw-content dictionary with our own pure-Rust trainer, then use
        // it through the full encode path: it must round-trip through libzstd and
        // our decoder, and shrink the corpus versus no dictionary.
        let samples = small_records();
        let refs: Vec<&[u8]> = samples.iter().map(|v| v.as_slice()).collect();
        let dict_bytes = train_dictionary(&refs, 8 * 1024);
        assert!(!dict_bytes.is_empty(), "trainer produced an empty dictionary");
        assert!(dict_bytes.len() <= 8 * 1024, "trainer exceeded the size budget");
        let dict = Dictionary::parse(&dict_bytes).expect("parse trained dict");
        assert_eq!(dict.id(), 0, "a raw-content dictionary carries no id");

        for s in samples.iter().take(40) {
            for level in [3, 19] {
                assert_dict_roundtrips(s, &dict_bytes, level);
            }
        }

        for &level in &[3, 19] {
            let no_dict: usize =
                samples.iter().map(|s| compress(s, level, false, true).len()).sum();
            let with_dict: usize = samples
                .iter()
                .map(|s| compress_with_dict(s, &dict, level, false, true).len())
                .sum();
            assert!(
                with_dict < no_dict,
                "our trained dictionary should shrink the corpus at L{level}: \
                 {with_dict} (dict) vs {no_dict} (none)"
            );
        }
    }

    #[test]
    fn our_structured_dict_loads_in_libzstd_and_improves_ratio() {
        let samples = small_records();
        let refs: Vec<&[u8]> = samples.iter().map(|v| v.as_slice()).collect();
        let dict_bytes = train_dictionary_structured(&refs, 8 * 1024);

        // It must be a real structured dictionary: magic + non-zero id + entropy.
        let dict = Dictionary::parse(&dict_bytes).expect("parse our structured dict");
        assert!(dict.entropy().is_some(), "structured dict must carry entropy tables");
        assert_ne!(dict.id(), 0, "structured dict must carry a non-zero id");

        // (a) libzstd LOADS it on the compress side — the strict ZSTD_loadCEntropy
        //     path validates every entropy table — and our decoder reads back what
        //     libzstd produced with it.
        for s in samples.iter().take(40) {
            let mut c = zstd::bulk::Compressor::with_dictionary(19, &dict_bytes)
                .expect("libzstd must load our structured dict (compress side)");
            let comp = c.compress(s).expect("libzstd compress with our dict");
            let got = decompress_with_dict(&comp, &dict, s.len() + 64).expect("our decode");
            assert_eq!(&got, s, "libzstd-compressed-with-our-dict round-trip mismatch");
        }

        // (b) Our own compress_with_dict output decodes through libzstd + us.
        for s in samples.iter().take(40) {
            for level in [3, 19] {
                assert_dict_roundtrips(s, &dict_bytes, level);
            }
        }

        // (c) The structured dictionary improves ratio: libzstd-with-our-dict
        //     beats libzstd-no-dict across the corpus (content + entropy tables).
        let mut with_dict = 0usize;
        let mut no_dict = 0usize;
        for s in &samples {
            let mut c = zstd::bulk::Compressor::with_dictionary(19, &dict_bytes).unwrap();
            with_dict += c.compress(s).unwrap().len();
            no_dict += zstd::bulk::compress(s, 19).unwrap().len();
        }
        assert!(
            with_dict < no_dict,
            "structured dict should help libzstd: {with_dict} (dict) vs {no_dict} (none)"
        );
    }

    #[test]
    fn structured_dict_warm_start_beats_raw_content() {
        // Same dictionary content, two ways: structured (seeds block 1's entropy
        // tables, so a small file warm-starts via Treeless literals + Repeat-mode
        // sequence tables) vs raw-content (no entropy — block 1 starts cold and
        // must describe its own tables). We use *our* trained structured dict,
        // whose repeat offsets are [1,4,8] — the same as a raw dict — so this
        // isolates the entropy-table seeding (a libzstd-trained dict instead
        // tunes its repeat offsets for libzstd's parser, which would confound the
        // comparison for our simpler encoder). With repeat offsets equal, a
        // structured dict only *adds* options to block 1 (Treeless / Repeat on
        // top of raw / fresh), so it can never lose and the warm-start should
        // shrink small files.
        let samples = small_records();
        let dict_bytes = train_dictionary_structured(
            &samples.iter().map(|v| v.as_slice()).collect::<Vec<_>>(),
            8 * 1024,
        );
        let structured = Dictionary::parse(&dict_bytes).expect("parse structured dict");
        assert!(structured.entropy().is_some(), "expected a structured dict");
        let raw = Dictionary::raw(structured.content());

        // Compare the frame *bodies* (blocks), not whole frames: a structured
        // dict carries a non-zero Dictionary_ID in the frame header (a fixed
        // per-frame cost the raw dict avoids), which on ~60-byte files would
        // swamp the block-level warm-start. Excluding the header isolates the
        // entropy seeding's effect on the actual block contents.
        let mut struct_body = 0usize;
        let mut raw_body = 0usize;
        for s in samples.iter().take(120) {
            let cs = compress_with_dict(s, &structured, 19, false, true);
            let cr = compress_with_dict(s, &raw, 19, false, true);
            // Both must round-trip through our decoder with their dictionary.
            assert_eq!(decompress_with_dict(&cs, &structured, s.len() + 64).unwrap(), *s);
            assert_eq!(decompress_with_dict(&cr, &raw, s.len() + 64).unwrap(), *s);
            struct_body += cs.len() - frame_header(&cs).unwrap().header_len;
            raw_body += cr.len() - frame_header(&cr).unwrap().header_len;
        }
        assert!(
            struct_body < raw_body,
            "structured-dict warm-start should shrink the block bodies on small files: \
             {struct_body} (structured) vs {raw_body} (raw)"
        );
    }
}
