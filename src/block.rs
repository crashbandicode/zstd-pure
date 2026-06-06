//! Block-level decoding — RFC 8878 §3.1.1.2.
//!
//! A block header is 3 little-endian bytes: bit 0 `Last_Block`, bits 1-2
//! `Block_Type`, bits 3-23 `Block_Size`. Raw and RLE blocks are trivial;
//! compressed blocks are a literals section followed by a sequences section.

use super::error::{Result, ZstdError};
use super::huff::HuffTable;
use super::{literals, sequences};
#[allow(unused_imports)]
use crate::alloc_prelude::*;

/// RFC 8878 §3.1.1.2: no block may exceed `min(Window_Size, 128 KiB)`. Callers
/// set [`BlockState::block_max`] to the per-frame value.
pub const MAX_BLOCK_SIZE: usize = 128 * 1024;

/// Per-frame decode state shared across blocks (entropy-table reuse +
/// repeat offsets), plus the growing output (which doubles as window history).
pub struct BlockState {
    /// Decoded output; back-references index into this (a leading dictionary
    /// prefix, if any, occupies `dict_len` bytes).
    pub out: Vec<u8>,
    /// Bytes at the front of `out` that are dictionary history, not output.
    pub dict_len: usize,
    /// Ceiling on real output size.
    pub max_output: usize,
    /// Largest permitted `Block_Size`, `min(Window_Size, MAX_BLOCK_SIZE)`
    /// (RFC 8878 §3.1.1.2) — enforced in [`Self::decode_block_at`].
    pub block_max: usize,
    pub huff: Option<HuffTable>,
    pub seq: sequences::SeqTables,
    pub rep: [u32; 3],
}

/// One parsed block header.
pub struct BlockHeader {
    pub last: bool,
    pub block_type: u8,
    pub block_size: usize,
}

/// Parse the 3-byte block header at the front of `src`.
pub fn read_header(src: &[u8]) -> Result<BlockHeader> {
    if src.len() < 3 {
        return Err(ZstdError::Truncated {
            what: "block header",
            needed: 3 - src.len(),
        });
    }
    let v = (src[0] as u32) | ((src[1] as u32) << 8) | ((src[2] as u32) << 16);
    Ok(BlockHeader {
        last: (v & 1) != 0,
        block_type: ((v >> 1) & 3) as u8,
        block_size: (v >> 3) as usize,
    })
}

impl BlockState {
    /// Real (non-dictionary) decoded length so far.
    fn output_len(&self) -> usize {
        self.out.len() - self.dict_len
    }

    fn check_ceiling(&self, extra: usize) -> Result<()> {
        let next = self
            .output_len()
            .checked_add(extra)
            .ok_or(ZstdError::OutputTooLarge {
                limit: self.max_output,
            })?;
        if next > self.max_output {
            return Err(ZstdError::OutputTooLarge {
                limit: self.max_output,
            });
        }
        Ok(())
    }

    /// Decode a Raw block: copy `data` verbatim.
    pub fn decode_raw(&mut self, data: &[u8]) -> Result<()> {
        self.check_ceiling(data.len())?;
        self.out.extend_from_slice(data);
        Ok(())
    }

    /// Decode a Compressed block body (literals section + sequences section).
    ///
    /// A compressed block's regenerated size isn't known until decoded, so the
    /// ceiling is enforced *during* decode inside [`sequences::decode_capped`].
    /// Two bounds apply: the frame-wide `max_output`, and — because RFC 8878
    /// §3.1.1.2 limits any one block to `Block_Maximum_Size` — a per-block cap of
    /// `block_max` on top of the output already produced. The per-block cap is
    /// what keeps the *streaming* decoder bounded: it disables `max_output`
    /// (relying on window eviction between blocks), so without it a single
    /// hostile block could regenerate gigabytes into the buffer before the next
    /// compaction. For a conformant frame neither bound ever binds.
    pub fn decode_compressed(&mut self, data: &[u8]) -> Result<()> {
        let (lits, consumed) = literals::decode(data, &mut self.huff)?;
        let real_so_far = self.out.len() - self.dict_len;
        let block_cap = self
            .max_output
            .min(real_so_far.saturating_add(self.block_max));
        sequences::decode_capped(
            &data[consumed..],
            &lits,
            &mut self.out,
            &mut self.seq,
            &mut self.rep,
            self.dict_len,
            block_cap,
        )
    }

    /// Decode an RLE block: `byte` repeated `count` times.
    pub fn decode_rle(&mut self, byte: u8, count: usize) -> Result<()> {
        self.check_ceiling(count)?;
        self.out.resize(self.out.len() + count, byte);
        Ok(())
    }

    /// Decode the one block whose 3-byte header sits at `src[pos..]`, appending
    /// its output to `self.out`. Returns the input position just past the block
    /// body and whether it was the last block of the frame.
    ///
    /// Enforces the RFC block-size cap ([`block_max`](Self::block_max)) and then
    /// the block-type dispatch (raw / RLE / compressed, with per-type truncation
    /// checks) — identical for the one-shot [`crate::frame`] loop and the
    /// bounded-memory [`crate::streaming`] decoder, so both drive blocks through
    /// here. Callers layer their own post-block work on top (checksum/window
    /// bookkeeping); this only advances one block.
    pub fn decode_block_at(&mut self, src: &[u8], pos: usize) -> Result<(usize, bool)> {
        let header = read_header(&src[pos..])?;
        if header.block_size > self.block_max {
            return Err(ZstdError::Invalid {
                what: "block size",
                detail: format!("block {} exceeds max {}", header.block_size, self.block_max),
            });
        }
        let mut pos = pos + 3;
        match header.block_type {
            0 => {
                let end = pos + header.block_size;
                if src.len() < end {
                    return Err(ZstdError::Truncated {
                        what: "raw block body",
                        needed: end - src.len(),
                    });
                }
                self.decode_raw(&src[pos..end])?;
                pos = end;
            }
            1 => {
                if src.len() <= pos {
                    return Err(ZstdError::Truncated {
                        what: "RLE block byte",
                        needed: 1,
                    });
                }
                self.decode_rle(src[pos], header.block_size)?;
                pos += 1;
            }
            2 => {
                let end = pos + header.block_size;
                if src.len() < end {
                    return Err(ZstdError::Truncated {
                        what: "compressed block body",
                        needed: end - src.len(),
                    });
                }
                self.decode_compressed(&src[pos..end])?;
                pos = end;
            }
            _ => {
                return Err(ZstdError::Invalid {
                    what: "block type",
                    detail: "reserved block type 3".into(),
                })
            }
        }
        Ok((pos, header.last))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A compressed block must not regenerate more than `block_max`, even when
    /// the frame-wide ceiling is disabled (the streaming decoder's bomb guard).
    /// A conformant frame never exceeds `block_max`, so this drives a real block
    /// body through `decode_compressed` with an artificially tight cap.
    #[test]
    fn compressed_block_regen_is_capped_at_block_max() {
        let payload = b"the quick brown fox jumps over the lazy dog. ".repeat(40);
        let frame = crate::compress(&payload, 3, false, true);
        // Slice out the (single) compressed block body following the header.
        let h = crate::frame_header(&frame).unwrap();
        let bh = &frame[h.header_len..];
        let v = (bh[0] as u32) | ((bh[1] as u32) << 8) | ((bh[2] as u32) << 16);
        assert_eq!((v >> 1) & 3, 2, "fixture must be a single compressed block");
        let body = &bh[3..3 + (v >> 3) as usize];

        let make = |block_max| BlockState {
            out: Vec::new(),
            dict_len: 0,
            max_output: usize::MAX, // streaming-style: frame ceiling disabled
            block_max,
            huff: None,
            seq: sequences::SeqTables::default(),
            rep: [1, 4, 8],
        };

        // A cap below the regenerated size stops the expansion.
        let mut tight = make(256);
        assert!(matches!(
            tight.decode_compressed(body),
            Err(ZstdError::OutputTooLarge { .. })
        ));

        // A cap that admits the block decodes it back to the original bytes.
        let mut ok = make(payload.len());
        ok.decode_compressed(body).unwrap();
        assert_eq!(ok.out, payload.as_slice());
    }
}
