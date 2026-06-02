//! Block-level encoding — RFC 8878 §3.1.1.2.
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

/// Write one fully compressed block covering `data[range]`: LZ-parse it against
/// the whole `data` (so back-references can reach earlier blocks) with the
/// frame's `finder`, emit a literals section (raw or Huffman, whichever is
/// smaller) + a best-mode sequences section. `max_offset` bounds back-references
/// to the frame window, and `rep` carries the running repeat offsets and is
/// updated by the parse; the caller must only commit that update if this
/// compressed block is actually used (see [`super::frame::compress`]).
/// Cross-block encoder state threaded through a frame: the running repeat
/// offsets and the previous compressed block's per-channel entropy tables. The
/// caller commits the returned state only when the block is actually emitted
/// compressed — a raw/RLE block leaves both untouched, exactly as the decoder
/// does — so a trial that loses to the store encoding is simply discarded.
#[derive(Clone)]
pub struct EncState {
    pub rep: [u32; 3],
    pub seq: super::sequences::SeqCTables,
    /// Previous compressed block's literals Huffman table, for Treeless reuse.
    pub lit: Option<super::huff::CodeTable>,
}

/// Write one (or, with `max_split_depth > 0`, several) fully compressed blocks
/// covering `data[range]`. The range is LZ-parsed once against the whole `data`
/// (so back-references reach earlier blocks) with the frame's `finder`; the
/// resulting sequences are then emitted as a single block or, when block
/// splitting pays for itself, partitioned into adjacent blocks each with its own
/// entropy tables (see [`emit_split`]). `max_offset` bounds back-references to
/// the frame window; `rep` carries the running repeat offsets and is updated by
/// the parse. The returned [`EncState`] must be committed only if these bytes
/// are actually used (a raw/RLE fallback leaves the decoder's state untouched).
#[allow(clippy::too_many_arguments)]
pub fn write_compressed_block(
    out: &mut Vec<u8>,
    last: bool,
    data: &[u8],
    range: core::ops::Range<usize>,
    finder: &mut super::lz::Finder,
    max_offset: usize,
    state: &EncState,
    max_split_depth: usize,
) -> Result<EncState> {
    let mut rep = state.rep;
    let (seqs, literals) = finder.parse(data, range, max_offset, &mut rep);
    let (seq, lit) =
        emit_split(out, last, &seqs, &literals, &state.seq, state.lit.as_ref(), max_split_depth)?;
    Ok(EncState { rep, seq, lit })
}

/// Emit one compressed block for a pre-parsed `(seqs, literals)` segment: a
/// literals section (raw / Huffman / Treeless reuse of `prev_lit`) followed by a
/// sequences section (cheapest per-channel table mode, reusing `prev_seq` for
/// Repeat mode), behind the 3-byte block header. Returns the literals + sequence
/// tables it used, to thread to the next block. `literals` must be exactly the
/// bytes this segment's sequences consume, plus this block's trailing literals.
fn emit_one(
    out: &mut Vec<u8>,
    last: bool,
    seqs: &[super::sequences::Seq],
    literals: &[u8],
    prev_seq: &super::sequences::SeqCTables,
    prev_lit: Option<&super::huff::CodeTable>,
) -> Result<(super::sequences::SeqCTables, Option<super::huff::CodeTable>)> {
    let mut body = Vec::with_capacity(literals.len() / 2 + 16);
    let lit = super::huff::write_literals_auto(&mut body, literals, prev_lit);
    let seq = super::sequences::write_sequences(&mut body, seqs, prev_seq)?;
    write_block_header(out, last, BlockType::Compressed, body.len());
    out.extend_from_slice(&body);
    Ok((seq, lit))
}

/// Emit `(seqs, literals)` as one compressed block, or — when `depth > 0` and a
/// split pays for itself — as two adjacent blocks (recursively), each with
/// entropy tables fit to its own statistics. This is the encoder side of
/// libzstd's block splitter: a 128 KiB block whose regions differ (e.g. text
/// then binary) codes smaller as separate blocks than under one block's
/// compromise tables.
///
/// The split point is the sequence boundary nearest the regenerated-byte
/// midpoint; splitting only at a sequence boundary means no match is bisected,
/// and because each sequence's `offset_value` was already resolved against the
/// running repeat offsets during the parse, the decoder evolves `rep`
/// identically whether it sees one block or the two halves in order. The first
/// sub-block's tables thread into the second (Repeat / Treeless reuse). The
/// split is **kept only when strictly smaller** than the single block, so the
/// output can never grow; the single-block encoding is always the fallback.
fn emit_split(
    out: &mut Vec<u8>,
    last: bool,
    seqs: &[super::sequences::Seq],
    literals: &[u8],
    prev_seq: &super::sequences::SeqCTables,
    prev_lit: Option<&super::huff::CodeTable>,
    depth: usize,
) -> Result<(super::sequences::SeqCTables, Option<super::huff::CodeTable>)> {
    // The single-block encoding — the fallback, and the bar a split must beat.
    let mut whole = Vec::new();
    let whole_tables = emit_one(&mut whole, last, seqs, literals, prev_seq, prev_lit)?;
    if depth == 0 || seqs.len() < 2 {
        out.extend_from_slice(&whole);
        return Ok(whole_tables);
    }

    // Split at the sequence boundary nearest the regenerated-byte midpoint.
    let total: usize = seqs.iter().map(|s| (s.lit_len + s.match_len) as usize).sum();
    let mut acc = 0usize;
    let mut k = 0usize;
    for (i, s) in seqs.iter().enumerate() {
        if acc * 2 >= total {
            k = i;
            break;
        }
        acc += (s.lit_len + s.match_len) as usize;
    }
    if k == 0 || k >= seqs.len() {
        out.extend_from_slice(&whole);
        return Ok(whole_tables);
    }

    // Partition the literals at the cumulative literal-length boundary of A; the
    // trailing literal run (past the last sequence) stays with B.
    let la: usize = seqs[..k].iter().map(|s| s.lit_len as usize).sum();
    let (lits_a, lits_b) = literals.split_at(la);

    // Emit A (never last) then B (last iff this node is), threading A's tables
    // into B. Each child recursively decides whether to split further.
    let mut buf_a = Vec::new();
    let (a_seq, a_lit) = emit_split(&mut buf_a, false, &seqs[..k], lits_a, prev_seq, prev_lit, depth - 1)?;
    let mut buf_b = Vec::new();
    let b_tables = emit_split(&mut buf_b, last, &seqs[k..], lits_b, &a_seq, a_lit.as_ref(), depth - 1)?;

    if buf_a.len() + buf_b.len() < whole.len() {
        out.extend_from_slice(&buf_a);
        out.extend_from_slice(&buf_b);
        Ok(b_tables)
    } else {
        out.extend_from_slice(&whole);
        Ok(whole_tables)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use super::super::sequences::{Seq, SeqCTables};

    /// `emit_split` must never produce a larger block than the single-block
    /// encoding (the split is kept only when strictly smaller), and on a clearly
    /// bimodal sequence stream — two halves with disjoint offset codes — it must
    /// actually split into something smaller (each half's offset channel
    /// collapses to RLE, versus one block needing a multi-symbol FSE table).
    #[test]
    fn split_never_grows_and_helps_on_bimodal_offsets() {
        let bimodal: Vec<Seq> = (0..200)
            .map(|_| Seq { lit_len: 0, match_len: 3, offset_value: 4 })
            .chain((0..200).map(|_| Seq { lit_len: 0, match_len: 3, offset_value: 1 << 20 }))
            .collect();
        let homogeneous: Vec<Seq> = (0..400)
            .map(|_| Seq { lit_len: 0, match_len: 3, offset_value: 4 })
            .collect();

        for (seqs, expect_smaller) in [(bimodal, true), (homogeneous, false)] {
            let mut single = Vec::new();
            emit_split(&mut single, true, &seqs, &[], &SeqCTables::default(), None, 0).unwrap();
            let mut split = Vec::new();
            emit_split(&mut split, true, &seqs, &[], &SeqCTables::default(), None, 4).unwrap();
            assert!(split.len() <= single.len(), "split ({}) grew past single ({})", split.len(), single.len());
            if expect_smaller {
                assert!(split.len() < single.len(), "bimodal block should split smaller: {} vs {}", split.len(), single.len());
            } else {
                // Homogeneous: every split costs extra headers, so the guard
                // rejects all of them and the output is exactly the single block.
                assert_eq!(split.len(), single.len(), "homogeneous block should not split");
            }
        }
    }
}
