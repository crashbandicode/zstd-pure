//! Block-level encoding — RFC 8478 §3.1.1.2.
//!
//! For now this writes the two literal block types that need no entropy coding:
//! **Raw** (store the bytes verbatim) and **RLE** (a single byte repeated). The
//! compressed block type (literals + sequences) is added with the match finder
//! in T2.3.

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
