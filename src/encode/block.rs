//! Block-level encoding — RFC 8478 §3.1.1.2.
//!
//! This writes the two literal block types that need no entropy coding —
//! **Raw** (store the bytes verbatim) and **RLE** (a single byte repeated) —
//! plus a **Compressed** block whose body is a Huffman-coded literals section
//! followed by an empty sequences section (the T2.1 entropy-encoder building
//! block; the match finder / real sequences land in T2.3).

#[allow(unused_imports)]
use crate::alloc_prelude::*;
use super::super::error::Result;

/// Maximum bytes a single block may regenerate (`Block_Maximum_Size` cap).
pub const BLOCK_SIZE_MAX: usize = 128 * 1024;

/// Block types, matching the decoder's 2-bit `Block_Type`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BlockType {
    Raw = 0,
    Rle = 1,
    Compressed = 2,
}

/// Append a 3-byte block header: `Last_Block` (bit 0), `Block_Type` (bits 1-2),
/// `Block_Size` (bits 3-23).
pub fn write_block_header(out: &mut Vec<u8>, last: bool, block_type: BlockType, block_size: usize) {
    debug_assert!(block_size < (1 << 21), "block size {block_size} exceeds 21 bits");
    let v = (last as u32) | ((block_type as u32) << 1) | ((block_size as u32) << 3);
    out.push((v & 0xFF) as u8);
    out.push(((v >> 8) & 0xFF) as u8);
    out.push(((v >> 16) & 0xFF) as u8);
}

/// Write one raw block (`chunk` stored verbatim).
pub fn write_raw_block(out: &mut Vec<u8>, last: bool, chunk: &[u8]) {
    write_block_header(out, last, BlockType::Raw, chunk.len());
    out.extend_from_slice(chunk);
}

/// Write one RLE block: `byte` repeated `count` times (1 payload byte).
pub fn write_rle_block(out: &mut Vec<u8>, last: bool, byte: u8, count: usize) {
    write_block_header(out, last, BlockType::Rle, count);
    out.push(byte);
}

/// Write a chunk as the smaller of a raw or RLE block (RLE when every byte in
/// the chunk is identical and the chunk is non-empty).
pub fn write_store_block(out: &mut Vec<u8>, last: bool, chunk: &[u8]) {
    if !chunk.is_empty() && chunk.iter().all(|&b| b == chunk[0]) {
        write_rle_block(out, last, chunk[0], chunk.len());
    } else {
        write_raw_block(out, last, chunk);
    }
}

/// Write one compressed block whose body is a Huffman-coded literals section
/// followed by an empty (`Number_of_Sequences = 0`) sequences section, so it
/// reconstructs `literals` verbatim. Errors if the literals can't be Huffman
/// coded (see [`super::huff::write_literals_section`]); the caller falls back to
/// a store block.
pub fn write_huffman_literals_block(out: &mut Vec<u8>, last: bool, literals: &[u8]) -> Result<()> {
    let mut body = Vec::with_capacity(literals.len());
    super::huff::write_literals_section(&mut body, literals)?;
    body.push(0); // Number_of_Sequences = 0 (single-byte short form)
    write_block_header(out, last, BlockType::Compressed, body.len());
    out.extend_from_slice(&body);
    Ok(())
}

/// Write one fully compressed block: LZ-parse `block`, emit a literals section
/// (raw or Huffman, whichever is smaller) + a predefined-table sequences
/// section. `max_offset` bounds back-references to the frame window. `rep`
/// carries the running repeat offsets and is updated by the parse; the caller
/// must only commit that update if this compressed block is actually used (see
/// [`super::frame::compress`]).
pub fn write_compressed_block(
    out: &mut Vec<u8>,
    last: bool,
    block: &[u8],
    max_offset: usize,
    rep: &mut [u32; 3],
) -> Result<()> {
    let (seqs, literals) = super::lz::fast_parse(block, max_offset, rep);
    let mut body = Vec::with_capacity(block.len() / 2 + 16);
    super::huff::write_literals_auto(&mut body, &literals);
    super::sequences::write_sequences_predefined(&mut body, &seqs)?;
    write_block_header(out, last, BlockType::Compressed, body.len());
    out.extend_from_slice(&body);
    Ok(())
}
