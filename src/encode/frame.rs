//! Frame-level encoding — RFC 8478 §3.1.1.1.
//!
//! Writes a frame header, the block sequence, and the optional XXH64 content
//! checksum. The block bodies come from [`super::block`] (raw/RLE today;
//! compressed blocks land with the match finder in T2.3).

#[allow(unused_imports)]
use crate::alloc_prelude::*;
use super::super::frame::ZSTD_MAGIC;
use super::super::xxhash::xxh64;
use super::block::{
    write_compressed_block, write_huffman_literals_block, write_store_block, BLOCK_SIZE_MAX,
};

/// Window log used by the store-mode encoder. Raw/RLE blocks carry no
/// back-references, so the window only has to admit one full block (128 KiB =
/// `1 << 17`).
const STORE_WINDOW_LOG: u32 = 17;

/// Write the frame header (RFC 8478 §3.1.1.1.1).
///
/// Always emits a window descriptor (`Single_Segment_Flag = 0`) and pledges the
/// content size (4-byte form for `<= u32::MAX`, else 8-byte). `dictionary_id`
/// of 0 means none.
fn write_frame_header(out: &mut Vec<u8>, content_size: u64, checksum: bool, window_log: u32) {
    // FCS field size: 2 = 4 bytes, 3 = 8 bytes.
    let fcs_flag: u8 = if content_size <= u32::MAX as u64 { 2 } else { 3 };
    // Frame_Header_Descriptor: bits 6-7 fcs_flag, bit 5 single_segment (0),
    // bit 2 content_checksum, bits 0-1 dict_id_flag (0).
    let fhd = (fcs_flag << 6) | ((checksum as u8) << 2);
    out.push(fhd);

    // Window descriptor: exponent in bits 3-7, mantissa (0) in bits 0-2.
    let exponent = window_log - 10;
    out.push((exponent as u8) << 3);

    match fcs_flag {
        2 => out.extend_from_slice(&(content_size as u32).to_le_bytes()),
        _ => out.extend_from_slice(&content_size.to_le_bytes()),
    }
}

/// Compress `data` into a store-mode frame (raw/RLE blocks only). The output is
/// a fully spec-conformant Zstandard frame: libzstd decompresses it, and so
/// does this crate's decoder. `expect_magic = false` produces a magicless
/// frame (`ZSTD_f_zstd1_magicless`).
pub fn compress_store(data: &[u8], checksum: bool, expect_magic: bool) -> Vec<u8> {
    let mut out = Vec::with_capacity(data.len() + 32);
    if expect_magic {
        out.extend_from_slice(&ZSTD_MAGIC.to_le_bytes());
    }
    write_frame_header(&mut out, data.len() as u64, checksum, STORE_WINDOW_LOG);

    if data.is_empty() {
        // A frame must contain at least one block; emit an empty last raw block.
        super::block::write_raw_block(&mut out, true, &[]);
    } else {
        let mut chunks = data.chunks(BLOCK_SIZE_MAX).peekable();
        while let Some(chunk) = chunks.next() {
            let last = chunks.peek().is_none();
            write_store_block(&mut out, last, chunk);
        }
    }

    if checksum {
        let digest = (xxh64(data, 0) & 0xFFFF_FFFF) as u32;
        out.extend_from_slice(&digest.to_le_bytes());
    }
    out
}

/// Compress `data` into a frame using LZ match finding + entropy coding (the
/// `fast` strategy). Each block independently picks the smallest of a fully
/// compressed block, a raw block, or an RLE block, so the output is always a
/// spec-conformant frame that libzstd and this crate's decoder both accept, and
/// is never larger than [`compress_store`].
///
/// `level` selects the compression parameters (window/hash sizes and, once the
/// stronger strategies land, the parse strategy) from the zstd level table; see
/// [`super::params`]. Today every level uses the `fast` finder, but `level`
/// already drives the window log (back-reference reach + frame header) and the
/// match-table size.
pub fn compress(data: &[u8], level: i32, checksum: bool, expect_magic: bool) -> Vec<u8> {
    let params = super::params::params_for_level(level, data.len());
    let window_log = params.window_log;
    let max_offset = 1usize << window_log;

    let mut out = Vec::with_capacity(data.len() / 2 + 64);
    if expect_magic {
        out.extend_from_slice(&ZSTD_MAGIC.to_le_bytes());
    }
    write_frame_header(&mut out, data.len() as u64, checksum, window_log);

    if data.is_empty() {
        super::block::write_raw_block(&mut out, true, &[]);
    } else {
        // Repeat offsets persist across blocks within a frame (the decoder only
        // updates them on compressed blocks). The match finder also persists,
        // so back-references span block boundaries up to the window. Thread both
        // here, committing a block's `rep` evolution only when we actually emit
        // it compressed — a store block leaves the decoder's `rep` untouched.
        // (The match table needs no rollback: a stored block's bytes are still
        // in the decoder's output, so indexing them stays valid.)
        let mut rep = [1u32, 4, 8];
        let mut finder = super::lz::Finder::new(&params);
        let n = data.len();
        let mut start = 0usize;
        while start < n {
            let end = (start + BLOCK_SIZE_MAX).min(n);
            let last = end == n;
            let mut store = Vec::new();
            write_store_block(&mut store, last, &data[start..end]);

            let mut comp = Vec::new();
            let mut rep_trial = rep;
            let use_comp = write_compressed_block(
                &mut comp, last, data, start..end, &mut finder, max_offset, &mut rep_trial,
            )
            .is_ok()
                && comp.len() < store.len();
            if use_comp {
                rep = rep_trial;
                out.extend_from_slice(&comp);
            } else {
                out.extend_from_slice(&store);
            }
            start = end;
        }
    }

    if checksum {
        let digest = (xxh64(data, 0) & 0xFFFF_FFFF) as u32;
        out.extend_from_slice(&digest.to_le_bytes());
    }
    out
}

/// Number of distinct byte values in `chunk` (early-outs at 2 — all we need to
/// decide whether a Huffman literals block is even applicable).
fn at_least_two_distinct(chunk: &[u8]) -> bool {
    let mut seen = [false; 256];
    let mut first = None;
    for &b in chunk {
        match first {
            None => {
                first = Some(b);
                seen[b as usize] = true;
            }
            Some(f) if b != f && !seen[b as usize] => return true,
            _ => {}
        }
    }
    false
}

/// Compress `data` into a frame using **Huffman-coded literals** blocks (no LZ
/// match finding yet — every block is `[Huffman literals][0 sequences]`). Each
/// block independently picks the smaller of its Huffman or store encoding, so
/// the output is never larger than [`compress_store`] and is always a
/// spec-conformant frame (libzstd and this crate's decoder both accept it).
///
/// This is the T2.1a entropy-encoder milestone; ratio-competitive output needs
/// the match finder (T2.3).
pub fn compress_huffman_literals(data: &[u8], checksum: bool, expect_magic: bool) -> Vec<u8> {
    let mut out = Vec::with_capacity(data.len() + 32);
    if expect_magic {
        out.extend_from_slice(&ZSTD_MAGIC.to_le_bytes());
    }
    write_frame_header(&mut out, data.len() as u64, checksum, STORE_WINDOW_LOG);

    if data.is_empty() {
        super::block::write_raw_block(&mut out, true, &[]);
    } else {
        let mut chunks = data.chunks(BLOCK_SIZE_MAX).peekable();
        while let Some(chunk) = chunks.next() {
            let last = chunks.peek().is_none();
            let mut store = Vec::new();
            write_store_block(&mut store, last, chunk);

            let mut huff = Vec::new();
            let use_huff = at_least_two_distinct(chunk)
                && write_huffman_literals_block(&mut huff, last, chunk).is_ok()
                && huff.len() < store.len();
            out.extend_from_slice(if use_huff { &huff } else { &store });
        }
    }

    if checksum {
        let digest = (xxh64(data, 0) & 0xFFFF_FFFF) as u32;
        out.extend_from_slice(&digest.to_le_bytes());
    }
    out
}
