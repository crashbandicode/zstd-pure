//! Block-level decoding — RFC 8478 §3.1.1.2.
//!
//! A block header is 3 little-endian bytes: bit 0 `Last_Block`, bits 1-2
//! `Block_Type`, bits 3-23 `Block_Size`. Raw and RLE blocks are trivial;
//! compressed blocks are a literals section followed by a sequences section.

#[allow(unused_imports)]
use crate::alloc_prelude::*;
use super::error::{Result, ZstdError};
use super::huff::HuffTable;
use super::{literals, sequences};

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
        if self.output_len() + extra > self.max_output {
            Err(ZstdError::OutputTooLarge {
                limit: self.max_output,
            })
        } else {
            Ok(())
        }
    }

    /// Decode a Raw block: copy `data` verbatim.
    pub fn decode_raw(&mut self, data: &[u8]) -> Result<()> {
        self.check_ceiling(data.len())?;
        self.out.extend_from_slice(data);
        Ok(())
    }

    /// Decode a Compressed block body (literals section + sequences section).
    pub fn decode_compressed(&mut self, data: &[u8]) -> Result<()> {
        let (lits, consumed) = literals::decode(data, &mut self.huff)?;
        sequences::decode(
            &data[consumed..],
            &lits,
            &mut self.out,
            &mut self.seq,
            &mut self.rep,
        )
    }

    /// Decode an RLE block: `byte` repeated `count` times.
    pub fn decode_rle(&mut self, byte: u8, count: usize) -> Result<()> {
        self.check_ceiling(count)?;
        self.out.resize(self.out.len() + count, byte);
        Ok(())
    }
}
