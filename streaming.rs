//! Streaming, bounded-memory frame decoding (RFC 8478 §3.1.1).
//!
//! [`StreamingDecoder`] decodes a single frame **block by block** into a
//! sliding window buffer, evicting bytes that are both already delivered and
//! older than `Window_Size`. Memory stays bounded by roughly
//! `window_size + one block`, independent of the (possibly multi-gigabyte)
//! logical output. It implements [`std::io::Read`], and a configurable
//! `window_log_max` ceiling rejects frames whose declared window would force an
//! unbounded allocation from untrusted input.
//!
//! The one-shot [`super::decompress`] path is left untouched (it intentionally
//! retains the whole output, which the byte-for-byte MeshCodec decode relies
//! on); streaming is an additive alternative for large or untrusted inputs.

use std::io::{self, Read};

use super::block::{self, BlockState};
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
            // Bounded-memory mode: per-block growth is already capped by the
            // input length, and the window bounds total residency, so the
            // one-shot ceiling is disabled here.
            max_output: usize::MAX,
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
        let header = block::read_header(&self.src[self.pos..])?;
        self.pos += 3;
        match header.block_type {
            0 => {
                let end = self.pos + header.block_size;
                if self.src.len() < end {
                    return Err(ZstdError::Truncated {
                        what: "raw block body",
                        needed: end - self.src.len(),
                    });
                }
                self.state.decode_raw(&self.src[self.pos..end])?;
                self.pos = end;
            }
            1 => {
                if self.src.len() <= self.pos {
                    return Err(ZstdError::Truncated {
                        what: "RLE block byte",
                        needed: 1,
                    });
                }
                let b = self.src[self.pos];
                self.state.decode_rle(b, header.block_size)?;
                self.pos += 1;
            }
            2 => {
                let end = self.pos + header.block_size;
                if self.src.len() < end {
                    return Err(ZstdError::Truncated {
                        what: "compressed block body",
                        needed: end - self.src.len(),
                    });
                }
                self.state.decode_compressed(&self.src[self.pos..end])?;
                self.pos = end;
            }
            _ => {
                return Err(ZstdError::Invalid {
                    what: "block type",
                    detail: "reserved block type 3".into(),
                })
            }
        }

        // Hash the freshly produced bytes before they can be evicted.
        let produced = &self.state.out[before..];
        self.hasher.update(produced);
        self.total_out += produced.len() as u64;

        if header.last {
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
    use crate::zstd_pure::decompress;

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
        assert!(matches!(err, Err(ZstdError::Invalid { what: "window size", .. })));
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
}
