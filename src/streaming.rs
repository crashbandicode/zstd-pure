//! Streaming, bounded-memory frame decoding (RFC 8878 §3.1.1).
//!
//! [`StreamingDecoder`] decodes a single frame **block by block** into a
//! sliding window buffer, evicting bytes that are both already delivered and
//! older than `Window_Size`. Memory stays bounded by roughly
//! `window_size + one block`, independent of the (possibly multi-gigabyte)
//! logical output. It implements [`crate::io::Read`] — a re-export of
//! `std::io::Read` under `std`, or a `no_std` shim otherwise — and a
//! configurable `window_log_max` ceiling rejects frames whose declared window
//! would force an unbounded allocation from untrusted input.
//!
//! The one-shot [`super::decompress`] path is left untouched (it intentionally
//! retains the whole output, which the byte-for-byte MeshCodec decode relies
//! on); streaming is an additive alternative for large or untrusted inputs.

#[allow(unused_imports)]
use crate::alloc_prelude::*;
use crate::io::{self, Read};

use super::block::{BlockState, MAX_BLOCK_SIZE};
use super::dict::Dictionary;
use super::error::{Result, ZstdError};
use super::frame::{frame_header, frame_header_magicless};
use super::sequences::SeqTables;
use super::xxhash::Xxh64;

/// Default maximum window log (`1 << 27` = 128 MiB), matching libzstd's default
/// decompression limit.
pub const DEFAULT_WINDOW_LOG_MAX: u32 = 27;

/// A block-by-block, bounded-memory single-frame decoder implementing
/// [`Read`].
pub struct StreamingDecoder<'a> {
    src: &'a [u8],
    /// Next input byte to read.
    pos: usize,
    /// Retained history target (the frame's `Window_Size`).
    window_size: usize,
    /// Sliding buffer: dictionary/history prefix followed by decoded output.
    state: BlockState,
    /// Index in `state.out` of the next byte to hand to the consumer.
    read_off: usize,
    /// Whether the last block has been decoded.
    last_done: bool,
    /// Whether a content checksum trails the frame.
    has_checksum: bool,
    /// Streaming hash of the produced output (for the trailing checksum).
    hasher: Xxh64,
    /// Total real output produced so far.
    total_out: u64,
    /// The frame's pledged content size, if any.
    declared_size: Option<u64>,
    /// Sticky failure: once set, every `read` reports it.
    poisoned: Option<String>,
}

impl<'a> StreamingDecoder<'a> {
    /// Create a decoder over a standard (magic-prefixed) frame with the default
    /// window-log ceiling.
    pub fn new(src: &'a [u8]) -> Result<Self> {
        Self::with_options(src, true, None, DEFAULT_WINDOW_LOG_MAX)
    }

    /// Create a decoder over a **magicless** frame.
    pub fn new_magicless(src: &'a [u8]) -> Result<Self> {
        Self::with_options(src, false, None, DEFAULT_WINDOW_LOG_MAX)
    }

    /// Create a decoder primed with a dictionary.
    pub fn with_dict(src: &'a [u8], dict: &Dictionary) -> Result<Self> {
        Self::with_options(src, true, Some(dict), DEFAULT_WINDOW_LOG_MAX)
    }

    /// Full constructor: choose magic handling, an optional dictionary, and the
    /// `window_log_max` ceiling (a frame declaring a larger window errors).
    pub fn with_options(
        src: &'a [u8],
        expect_magic: bool,
        dict: Option<&Dictionary>,
        window_log_max: u32,
    ) -> Result<Self> {
        let header = if expect_magic {
            frame_header(src)?
        } else {
            frame_header_magicless(src)?
        };

        let max_ws: u64 = 1u64 << window_log_max.min(63);
        if header.window_size > max_ws {
            return Err(ZstdError::Invalid {
                what: "window size",
                detail: format!(
                    "frame window {} exceeds window_log_max ({} = {max_ws} bytes)",
                    header.window_size, window_log_max
                ),
            });
        }

        let mut state = BlockState {
            out: Vec::new(),
            dict_len: 0,
            // Bounded-memory mode: the window bounds total residency (eviction
            // happens between blocks), so the frame-wide ceiling is disabled.
            max_output: usize::MAX,
            // Per-block regenerated size is still capped at Block_Maximum_Size
            // (`decode_compressed` enforces it against `block_max`), so a single
            // hostile block can't balloon the buffer before the next eviction.
            block_max: header.window_size.min(MAX_BLOCK_SIZE as u64) as usize,
            huff: None,
            seq: SeqTables::default(),
            rep: [1, 4, 8],
        };
        let mut read_off = 0usize;

        if let Some(d) = dict {
            if header.dictionary_id != 0 && d.id() != 0 && header.dictionary_id != d.id() {
                return Err(ZstdError::Dictionary(format!(
                    "frame references dictionary id {} but dictionary is id {}",
                    header.dictionary_id,
                    d.id()
                )));
            }
            state.out.extend_from_slice(d.content());
            state.dict_len = d.content().len();
            read_off = state.dict_len;
            if let Some(e) = d.entropy() {
                state.huff = Some(e.huff.clone());
                state.seq = e.tables.clone();
                state.rep = e.rep;
            }
        }

        Ok(StreamingDecoder {
            src,
            pos: header.header_len,
            window_size: header.window_size as usize,
            state,
            read_off,
            last_done: false,
            has_checksum: header.has_checksum,
            hasher: Xxh64::new(0),
            total_out: 0,
            declared_size: header.content_size,
            poisoned: None,
        })
    }

    /// Current internal buffer footprint (history retained + decoded-not-yet-
    /// delivered). Stays bounded by ~`window_size + one block`.
    pub fn buffered_len(&self) -> usize {
        self.state.out.len()
    }

    /// The frame's window size (retained-history target).
    pub fn window_size(&self) -> usize {
        self.window_size
    }

    /// Decode exactly one block, appending its output to the buffer and (on the
    /// last block) verifying the content size and trailing checksum.
    fn decode_next_block(&mut self) -> Result<()> {
        let before = self.state.out.len();
        let (next, last) = self.state.decode_block_at(self.src, self.pos)?;
        self.pos = next;

        // Hash the freshly produced bytes before they can be evicted.
        let produced = &self.state.out[before..];
        self.hasher.update(produced);
        self.total_out += produced.len() as u64;

        if last {
            self.last_done = true;
            if let Some(n) = self.declared_size {
                if self.total_out != n {
                    return Err(ZstdError::Invalid {
                        what: "frame content size",
                        detail: format!("declared {n}, decoded {}", self.total_out),
                    });
                }
            }
            if self.has_checksum {
                let end = self.pos + 4;
                if self.src.len() < end {
                    return Err(ZstdError::Truncated {
                        what: "content checksum",
                        needed: end - self.src.len(),
                    });
                }
                let stored = u32::from_le_bytes([
                    self.src[self.pos],
                    self.src[self.pos + 1],
                    self.src[self.pos + 2],
                    self.src[self.pos + 3],
                ]);
                let computed = (self.hasher.digest() & 0xFFFF_FFFF) as u32;
                if stored != computed {
                    return Err(ZstdError::ChecksumMismatch { stored, computed });
                }
                self.pos = end;
            }
        }
        Ok(())
    }

    /// Drop buffer bytes that are both already delivered and older than the
    /// window (no longer reachable by a back-reference).
    fn compact(&mut self) {
        let keep_from = self.state.out.len().saturating_sub(self.window_size);
        let drop = self.read_off.min(keep_from);
        if drop > 0 {
            self.state.out.drain(..drop);
            self.read_off -= drop;
            self.state.dict_len = self.state.dict_len.saturating_sub(drop);
        }
    }
}

impl Read for StreamingDecoder<'_> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        if let Some(msg) = &self.poisoned {
            return Err(io::Error::new(io::ErrorKind::InvalidData, msg.clone()));
        }
        // Decode forward until there is deliverable output or the frame ends.
        while self.read_off >= self.state.out.len() && !self.last_done {
            if let Err(e) = self.decode_next_block() {
                let msg = e.to_string();
                self.poisoned = Some(msg.clone());
                return Err(io::Error::new(io::ErrorKind::InvalidData, msg));
            }
            self.compact();
        }
        let avail = self.state.out.len() - self.read_off;
        if avail == 0 {
            return Ok(0); // clean EOF
        }
        let n = avail.min(buf.len());
        buf[..n].copy_from_slice(&self.state.out[self.read_off..self.read_off + n]);
        self.read_off += n;
        self.compact();
        Ok(n)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::decompress;
    use crate::testutil::prng;

    fn zstd_with_window(data: &[u8], level: i32, window_log: u32) -> Vec<u8> {
        let mut cctx = zstd::zstd_safe::CCtx::create();
        cctx.set_parameter(zstd::zstd_safe::CParameter::CompressionLevel(level))
            .unwrap();
        cctx.set_parameter(zstd::zstd_safe::CParameter::WindowLog(window_log))
            .unwrap();
        // No content size -> a real window descriptor (not single-segment).
        cctx.set_parameter(zstd::zstd_safe::CParameter::ContentSizeFlag(false))
            .unwrap();
        let mut out = Vec::with_capacity(zstd::zstd_safe::compress_bound(data.len()));
        cctx.compress2(&mut out, data).unwrap();
        out
    }

    #[test]
    fn streaming_matches_oneshot() {
        let text = "the quick brown fox jumps over the lazy dog. "
            .repeat(500)
            .into_bytes();
        let comp = zstd::bulk::compress(&text, 9).unwrap();
        let mut dec = StreamingDecoder::new(&comp).unwrap();
        let mut got = Vec::new();
        dec.read_to_end(&mut got).unwrap();
        assert_eq!(got, text);
        assert_eq!(got, decompress(&comp).unwrap());
    }

    #[test]
    fn bounded_window_on_large_logical_output() {
        // ~4 MiB of mildly redundant data compressed with a small 64 KiB window,
        // forcing multi-block output the decoder must stream within the window.
        let mut data = Vec::with_capacity(4 << 20);
        let mut x = 0x2545_F491u32;
        while data.len() < (4 << 20) {
            x = x.wrapping_mul(1664525).wrapping_add(1013904223);
            // Bias toward repetition so back-references actually span the window.
            data.push((x >> 27) as u8);
        }
        let window_log = 16; // 64 KiB
        let comp = zstd_with_window(&data, 12, window_log);
        assert!(comp.len() < data.len());

        let mut dec = StreamingDecoder::new(&comp).unwrap();
        assert_eq!(dec.window_size(), 1 << window_log);

        // Read in small chunks; assert the internal buffer never balloons toward
        // the full logical size — it must stay near window + one block.
        let bound = (1usize << window_log) + (1 << 18) + 4096; // window + 256K block + slack
        let mut got = Vec::with_capacity(data.len());
        let mut tmp = [0u8; 9000];
        loop {
            let n = dec.read(&mut tmp).unwrap();
            if n == 0 {
                break;
            }
            got.extend_from_slice(&tmp[..n]);
            assert!(
                dec.buffered_len() <= bound,
                "internal buffer {} exceeded bound {bound}",
                dec.buffered_len()
            );
        }
        assert_eq!(got, data);
    }

    #[test]
    fn bounded_window_on_highly_compressible() {
        // 16 MiB of zeros -> a tiny compressed frame; proves a huge logical
        // output streams with a bounded buffer.
        let data = vec![0u8; 16 << 20];
        let comp = zstd_with_window(&data, 19, 17);
        assert!(comp.len() < 64 * 1024);
        let mut dec = StreamingDecoder::new(&comp).unwrap();
        let bound = (1usize << 17) + (1 << 18) + 4096;
        let mut total = 0u64;
        let mut tmp = [0u8; 32768];
        loop {
            let n = dec.read(&mut tmp).unwrap();
            if n == 0 {
                break;
            }
            assert!(tmp[..n].iter().all(|&b| b == 0));
            total += n as u64;
            assert!(dec.buffered_len() <= bound);
        }
        assert_eq!(total, data.len() as u64);
    }

    #[test]
    fn rejects_oversized_window() {
        // 2 MiB so libzstd actually commits to the full 1 MiB (windowLog 20)
        // window rather than shrinking it to fit a tiny input.
        let mut data = Vec::with_capacity(2 << 20);
        let mut x = 0x1357_9bdfu32;
        while data.len() < (2 << 20) {
            x = x.wrapping_mul(1103515245).wrapping_add(12345);
            data.push((x >> 24) as u8);
        }
        let comp = zstd_with_window(&data, 3, 20); // 1 MiB window
                                                   // A window_log_max below the frame's window must be rejected.
        let err = StreamingDecoder::with_options(&comp, true, None, 18);
        assert!(matches!(
            err,
            Err(ZstdError::Invalid {
                what: "window size",
                ..
            })
        ));
        // And accepted when the ceiling is high enough.
        assert!(StreamingDecoder::with_options(&comp, true, None, 21).is_ok());
    }

    #[test]
    fn detects_corrupt_checksum() {
        let data = b"checksum this streamed content ".repeat(80);
        let mut cctx = zstd::zstd_safe::CCtx::create();
        cctx.set_parameter(zstd::zstd_safe::CParameter::ChecksumFlag(true))
            .unwrap();
        let mut comp = Vec::with_capacity(zstd::zstd_safe::compress_bound(data.len()));
        cctx.compress2(&mut comp, &data).unwrap();
        // Good path.
        let mut dec = StreamingDecoder::new(&comp).unwrap();
        let mut got = Vec::new();
        dec.read_to_end(&mut got).unwrap();
        assert_eq!(got, data);
        // Corrupt the trailing checksum -> read errors.
        let n = comp.len();
        comp[n - 1] ^= 0xFF;
        let mut bad = StreamingDecoder::new(&comp).unwrap();
        let mut sink = Vec::new();
        assert!(bad.read_to_end(&mut sink).is_err());
    }

    /// `compress_long` advertises a window beyond 8 MiB and emits a back-reference
    /// at a > 8 MiB offset; the streaming decoder must reconstruct it through its
    /// sliding window at the large window log (the existing LDM tests only exercise
    /// the one-shot decoder + libzstd). — HANDOFF §4.2a
    #[test]
    fn streaming_decode_of_compress_long_far_repeat() {
        let chunk = prng(512 * 1024, 0x00C0_FFEE);
        let filler = prng(9_000_000, 0x00F1_11E2);
        let mut data = Vec::with_capacity(chunk.len() * 2 + filler.len());
        data.extend_from_slice(&chunk);
        data.extend_from_slice(&filler);
        data.extend_from_slice(&chunk); // its copy sits ~9.5 MiB back (> 8 MiB)

        // Level 1 keeps the parse cheap; the point is the decode path, not ratio.
        let frame = crate::compress_long(&data, 1, true, true);
        // The frame must advertise a window past 8 MiB (so the far offset is legal).
        let h = crate::frame_header(&frame).unwrap();
        assert!(
            h.window_size > (8 << 20),
            "expected a > 8 MiB window, got {}",
            h.window_size
        );

        let mut dec = StreamingDecoder::new(&frame).unwrap();
        assert_eq!(dec.window_size(), h.window_size as usize);
        let mut got = Vec::new();
        dec.read_to_end(&mut got).unwrap();
        assert_eq!(got, data, "streaming decode of a large-window LDM frame");
        // Cross-check the one-shot path agrees.
        assert_eq!(decompress(&frame).unwrap(), data);
    }

    /// A frame that *declares* a 128 MiB (log 27) window but carries tiny content
    /// must not force a 128 MiB allocation — the decoder grows its buffer to the
    /// content, not the declared window. Hand-crafted because neither encoder
    /// advertises a window so much larger than its content. — HANDOFF §4.2b
    #[test]
    fn streaming_decode_bounded_at_declared_128mib_window() {
        let payload = b"tiny payload inside a frame that declares a 128 MiB window. ".repeat(20);
        let mut frame = Vec::new();
        frame.extend_from_slice(&0xFD2F_B528u32.to_le_bytes()); // magic
                                                                // FHD: fcs_flag 0, single_segment 0, no checksum, no dict id.
        frame.push(0x00);
        // Window descriptor: exponent = window_log - 10 = 17 (→ log 27), mantissa 0.
        frame.push((17u8) << 3);
        // One raw, last block: Last_Block=1 (bit 0), Block_Type=Raw=0 (bits 1-2),
        // Block_Size in bits 3+.
        let bh = 1u32 | ((payload.len() as u32) << 3);
        frame.extend_from_slice(&bh.to_le_bytes()[..3]);
        frame.extend_from_slice(&payload);

        // It declares the full 128 MiB window...
        let h = crate::frame_header(&frame).unwrap();
        assert_eq!(h.window_size, 1u64 << 27);

        let mut dec = StreamingDecoder::new(&frame).unwrap();
        assert_eq!(dec.window_size(), 1 << 27);
        let mut got = Vec::new();
        dec.read_to_end(&mut got).unwrap();
        assert_eq!(got, payload);
        // ...but the working buffer tracks the content, not the declared window.
        assert!(
            dec.buffered_len() <= payload.len() + 4096,
            "decoder over-allocated: {} for {}-byte content",
            dec.buffered_len(),
            payload.len()
        );
        // libzstd (default windowLogMax 27) and our one-shot path also accept it.
        assert_eq!(decompress(&frame).unwrap(), payload);
        assert_eq!(
            zstd::bulk::decompress(&frame, payload.len() + 64).unwrap(),
            payload
        );
    }
}
