//! Huff0 (Huffman) literal **encoding** — RFC 8878 §4.2.1, the inverse of the
//! decoder in [`crate::huff`].
//!
//! Strategy (see `README.md`):
//! 1. histogram the literals and build **length-limited** Huffman code lengths
//!    (≤ [`MAX_CODE_LEN`]), yielding a *complete* prefix code (Kraft sum = 1);
//! 2. convert lengths to zstd "weights" (`w_s = max_bits + 1 − len_s`);
//! 3. **reuse the decoder's [`huff::build_table`]** to obtain the authoritative
//!    per-symbol canonical codes (`first_table_index(s) >> (max_bits − nb_s)`).
//!
//! Step 3 is the key non-brittleness trick: the encoder reads its codes back out
//! of the exact table the (libzstd-validated) decoder builds, so the two can
//! never drift apart.
//!
//! The bitstream mirrors libzstd's `BIT_CStream`: a forward LSB accumulator into
//! which each symbol's code is pushed in **reverse** data order, capped by a `1`
//! sentinel bit. That pairs with the decoder's reverse `BIT_DStream` reader:
//! `add(v, nb)` on encode ↔ `read(nb) == v` on decode.

use super::super::error::{Result, ZstdError};
use super::super::huff;
use super::bitstream::BitWriter;
use super::fse as efse;
#[allow(unused_imports)]
use crate::alloc_prelude::*;

/// Maximum FSE accuracy log for Huffman weight tables (RFC 8878 §4.2.1.1).
const HUF_WEIGHT_MAX_LOG: u32 = 6;

/// Hard cap on emitted code length. zstd's literals Huffman table log is 11.
const MAX_CODE_LEN: u8 = 11;

/// Per-symbol canonical code + bit count, plus the weight header inputs.
#[derive(Clone)]
pub struct CodeTable {
    /// `code[s]` holds the `nbits[s]`-bit canonical code (first-read bit = MSB).
    code: [u32; 256],
    nbits: [u8; 256],
    /// Highest symbol value with a non-zero weight — the implicit final weight,
    /// not stored in the header.
    max_symbol: usize,
    /// Stored header weights for symbols `0..max_symbol`.
    weights: Vec<u8>,
}

impl CodeTable {
    /// Whether this table can encode every byte present in a literals block (a
    /// byte with no code — zero bit length — isn't in the table and can't be
    /// emitted) — the validity gate for reusing it in a Treeless block. Takes a
    /// precomputed byte histogram (a byte is present iff `freq[b] > 0`) so the
    /// check is O(256), not O(n) over the (large) literals the planner already
    /// histogrammed.
    pub fn can_encode_hist(&self, freq: &[u32; 256]) -> bool {
        (0..256).all(|b| freq[b] == 0 || self.nbits[b] > 0)
    }
}

/// Build length-limited Huffman code lengths from a symbol histogram (`0` for an
/// absent symbol). The caller must ensure ≥ 2 distinct symbols are present.
fn code_lengths(freq: &[u32; 256]) -> [u8; 256] {
    let present: Vec<usize> = (0..256).filter(|&s| freq[s] > 0).collect();
    let m = present.len();
    let mut lengths = [0u8; 256];
    debug_assert!(m >= 2, "Huffman needs >= 2 distinct symbols");

    // Plain Huffman via a min-heap to get an initial (unconstrained) length per
    // symbol. Arena: leaves `0..m` ↦ `present[*]`, internal nodes `m..2m-1`.
    let mut weight = vec![0u64; 2 * m];
    let mut parent = vec![usize::MAX; 2 * m];
    for (i, &s) in present.iter().enumerate() {
        weight[i] = freq[s] as u64;
    }
    use alloc::collections::BinaryHeap;
    use core::cmp::Reverse;
    let mut heap: BinaryHeap<Reverse<(u64, usize)>> = BinaryHeap::new();
    for (i, &w) in weight.iter().enumerate().take(m) {
        heap.push(Reverse((w, i)));
    }
    let mut next = m;
    while heap.len() > 1 {
        let Reverse((w1, a)) = heap.pop().unwrap();
        let Reverse((w2, b)) = heap.pop().unwrap();
        weight[next] = w1 + w2;
        parent[a] = next;
        parent[b] = next;
        heap.push(Reverse((weight[next], next)));
        next += 1;
    }

    // Initial code length per leaf = its depth; tally counts per length.
    let mut counts = vec![0u32; 64];
    let mut depth = vec![0usize; m];
    for (i, d_out) in depth.iter_mut().enumerate() {
        let mut d = 0usize;
        let mut p = parent[i];
        while p != usize::MAX {
            d += 1;
            p = parent[p];
        }
        *d_out = d;
        counts[d] += 1;
    }

    // Limit to MAX_CODE_LEN by redistributing the length counts (the classic
    // JPEG Annex-K.3 / zlib repair). It preserves both the symbol count and the
    // Kraft sum (= 1), so the result stays a complete prefix code.
    let limit = MAX_CODE_LEN as usize;
    let maxlen = *depth.iter().max().unwrap();
    if maxlen > limit {
        for i in (limit + 1..=maxlen).rev() {
            while counts[i] > 0 {
                let mut j = i - 2;
                while counts[j] == 0 {
                    j -= 1;
                }
                counts[i] -= 2;
                counts[i - 1] += 1;
                counts[j + 1] += 2;
                counts[j] -= 1;
            }
        }
    }

    // Assign lengths to symbols: most-frequent symbols take the shortest codes.
    let mut order = present;
    order.sort_by(|&x, &y| freq[y].cmp(&freq[x]).then(x.cmp(&y)));
    let mut k = 0;
    for (len, &c) in counts.iter().enumerate().take(limit + 1).skip(1) {
        for _ in 0..c {
            lengths[order[k]] = len as u8;
            k += 1;
        }
    }
    debug_assert_eq!(k, m);
    lengths
}

/// Build the per-symbol code table from length-limited code lengths, deriving
/// the codes from the decoder's own [`huff::build_table`].
fn build_code_table(lengths: &[u8; 256]) -> Result<CodeTable> {
    let max_bits = *lengths.iter().max().unwrap() as u32;
    let max_symbol = (0..256).rev().find(|&s| lengths[s] > 0).unwrap();

    // weights: w = max_bits + 1 − len (0 for absent). The stored header covers
    // symbols 0..max_symbol; symbol max_symbol's weight is implicit (derived by
    // build_table from the power-of-two completeness constraint).
    let mut weights = vec![0u8; max_symbol];
    for (s, w) in weights.iter_mut().enumerate() {
        if lengths[s] > 0 {
            *w = (max_bits + 1 - lengths[s] as u32) as u8;
        }
    }

    let table = huff::build_table(&weights)?;

    // Invert the decode table: the first index carrying symbol `s` gives its
    // canonical code as `index >> (max_bits − nb_s)`.
    let mut code = [0u32; 256];
    let mut nbits = [0u8; 256];
    let mut seen = [false; 256];
    for (idx, (&sym, &nb)) in table.symbols.iter().zip(table.num_bits.iter()).enumerate() {
        let s = sym as usize;
        if nb != 0 && !seen[s] {
            seen[s] = true;
            nbits[s] = nb;
            code[s] = (idx as u32) >> (table.max_bits - nb as u32);
        }
    }

    Ok(CodeTable {
        code,
        nbits,
        max_symbol,
        weights,
    })
}

/// Encode one literal sub-stream. Symbols are emitted in **reverse** order so
/// the decoder's reverse bitstream reader replays them forwards.
fn encode_stream(table: &CodeTable, lits: &[u8]) -> Vec<u8> {
    // A Huffman stream is never larger than its input (codes average < 8 bits when
    // Huffman is chosen at all); reserve that up front so the per-literal loop
    // never reallocates.
    let mut bw = BitWriter::with_capacity(lits.len() + 8);
    for &b in lits.iter().rev() {
        let s = b as usize;
        bw.add(table.code[s], table.nbits[s] as u32);
    }
    bw.finish()
}

/// Encode the four-stream form: split into 4 segments of `ceil(n/4)` (last takes
/// the remainder), prefixed by a 6-byte jump table of the first three lengths.
fn encode_4stream(table: &CodeTable, lits: &[u8]) -> Result<Vec<u8>> {
    let n = lits.len();
    let seg = n.div_ceil(4);
    if seg * 3 > n {
        return Err(ZstdError::Invalid {
            what: "huffman 4-stream",
            detail: format!("regen {n} too small for 4 streams"),
        });
    }
    let s1 = encode_stream(table, &lits[0..seg]);
    let s2 = encode_stream(table, &lits[seg..2 * seg]);
    let s3 = encode_stream(table, &lits[2 * seg..3 * seg]);
    let s4 = encode_stream(table, &lits[3 * seg..]);

    let mut out = Vec::with_capacity(6 + s1.len() + s2.len() + s3.len() + s4.len());
    for s in [&s1, &s2, &s3] {
        if s.len() > u16::MAX as usize {
            return Err(ZstdError::Invalid {
                what: "huffman 4-stream",
                detail: "sub-stream exceeds 64 KiB jump-table field".into(),
            });
        }
        out.extend_from_slice(&(s.len() as u16).to_le_bytes());
    }
    out.extend_from_slice(&s1);
    out.extend_from_slice(&s2);
    out.extend_from_slice(&s3);
    out.extend_from_slice(&s4);
    Ok(out)
}

/// Write the Huffman weight header: **direct** (4-bit packed) form when the
/// highest symbol is ≤ 128, otherwise **FSE-compressed** weights.
fn write_weight_header(out: &mut Vec<u8>, table: &CodeTable) -> Result<()> {
    if table.max_symbol <= 128 {
        write_weight_header_direct(out, table);
        Ok(())
    } else {
        write_weight_header_fse(out, table)
    }
}

/// Write the **direct** weight header (`byte = 127 + N`, weights packed two per
/// byte, high nibble first). `table.max_symbol` (= `N`, the stored weight count)
/// must be ≤ 128 so the header byte stays ≤ 255.
fn write_weight_header_direct(out: &mut Vec<u8>, table: &CodeTable) {
    let num_weights = table.max_symbol;
    debug_assert!(num_weights <= 128);
    out.push((127 + num_weights) as u8);
    let w = &table.weights;
    let mut i = 0;
    while i < num_weights {
        let hi = w[i];
        let lo = if i + 1 < num_weights { w[i + 1] } else { 0 };
        out.push((hi << 4) | lo);
        i += 2;
    }
}

/// Write **FSE-compressed** weights (header byte `< 128` = the compressed size,
/// then the FSE table description + bitstream that decodes to the weight list).
/// Errors — leaving the caller to fall back to a store block — when the weights
/// are a single value (no FSE distribution) or compress to ≥ 128 bytes (which
/// the 1-byte size field can't represent).
fn write_weight_header_fse(out: &mut Vec<u8>, table: &CodeTable) -> Result<()> {
    let weights = &table.weights; // length = max_symbol (in 129..=255 here)
    let mut freq = [0u32; 256];
    for &w in weights {
        freq[w as usize] += 1;
    }
    let max_w = (0..256).rev().find(|&v| freq[v] > 0).unwrap();
    let num_present = freq.iter().filter(|&&c| c > 0).count();
    if num_present < 2 {
        return Err(ZstdError::Invalid {
            what: "huffman fse weights",
            detail: "uniform weights have no FSE distribution".into(),
        });
    }

    let table_log = efse::choose_table_log(HUF_WEIGHT_MAX_LOG, num_present);
    let norm = efse::normalize_counts(&freq, weights.len() as u32, max_w, table_log);
    let ncount = efse::write_ncount(&norm, max_w, table_log);
    let ctable = efse::build_ctable(&norm, max_w, table_log);
    let bitstream = efse::encode(&ctable, weights);

    let comp = ncount.len() + bitstream.len();
    if comp >= 128 {
        return Err(ZstdError::Invalid {
            what: "huffman fse weights",
            detail: format!("compressed weights {comp} >= 128-byte header limit"),
        });
    }
    out.push(comp as u8);
    out.extend_from_slice(&ncount);
    out.extend_from_slice(&bitstream);
    Ok(())
}

/// Write the compressed/treeless literals header (RFC 8878 §3.1.1.3.1.1),
/// selecting the smallest `Size_Format` that fits `regen` + `comp`.
/// `block_type` is 2 (Compressed — a tree description precedes the streams) or
/// 3 (Treeless — streams only, reusing the previous block's Huffman table).
fn write_compressed_lit_header(
    out: &mut Vec<u8>,
    block_type: u32,
    four: bool,
    regen: usize,
    comp: usize,
) -> Result<()> {
    let too_big = || ZstdError::Invalid {
        what: "compressed literals header",
        detail: format!("regen {regen} / comp {comp} exceed 18-bit fields"),
    };
    if !four {
        // Size_Format 0: single stream, 10-bit regen/comp, 3-byte header.
        if regen > 0x3FF || comp > 0x3FF {
            return Err(too_big());
        }
        let v = block_type | (regen as u32) << 4 | (comp as u32) << 14;
        out.extend_from_slice(&v.to_le_bytes()[..3]);
    } else if regen <= 0x3FF && comp <= 0x3FF {
        // Size_Format 1: four streams, 10-bit, 3-byte header.
        let v = block_type | 1 << 2 | (regen as u32) << 4 | (comp as u32) << 14;
        out.extend_from_slice(&v.to_le_bytes()[..3]);
    } else if regen <= 0x3FFF && comp <= 0x3FFF {
        // Size_Format 2: four streams, 14-bit, 4-byte header.
        let v = block_type | 2 << 2 | (regen as u32) << 4 | (comp as u32) << 18;
        out.extend_from_slice(&v.to_le_bytes());
    } else if regen <= 0x3FFFF && comp <= 0x3FFFF {
        // Size_Format 3: four streams, 18-bit, 5-byte header.
        let v = block_type as u64 | 3 << 2 | (regen as u64) << 4 | (comp as u64) << 22;
        out.extend_from_slice(&v.to_le_bytes()[..5]);
    } else {
        return Err(too_big());
    }
    Ok(())
}

/// Encode `literals` four-stream when worthwhile, else single-stream, with
/// `table`. Returns `(four, stream_payload)` — the streams only, no header.
fn encode_lit_streams(table: &CodeTable, literals: &[u8]) -> Result<(bool, Vec<u8>)> {
    let n = literals.len();
    // Four-stream form amortizes the 6-byte jump table only once it's both valid
    // (`3·ceil(n/4) ≤ n`) and large enough to pay for itself.
    let four = n >= 256 && n.div_ceil(4) * 3 <= n;
    let streams = if four {
        encode_4stream(table, literals)?
    } else {
        encode_stream(table, literals)
    };
    Ok((four, streams))
}

/// Build a **Compressed** (block_type 2) Huffman literals section — a fresh tree
/// description plus the streams — returning the section bytes and the table it
/// built (to thread forward for a later Treeless block). Errors (fewer than 2
/// distinct symbols, or a symbol > 128 while only direct weights are supported)
/// leave it to the caller to fall back to a raw/RLE literals block.
fn build_compressed_section(literals: &[u8]) -> Result<(Vec<u8>, CodeTable)> {
    let mut freq = [0u32; 256];
    for &b in literals {
        freq[b as usize] += 1;
    }
    let (table, weight_header) = build_fresh_table(&freq).ok_or(ZstdError::Invalid {
        what: "huffman literals",
        detail: "need >= 2 distinct symbols / unbuildable table".into(),
    })?;
    let section = assemble_compressed_section(&table, &weight_header, literals)?;
    Ok((section, table))
}

/// Build the fresh Huffman table for `freq` plus its weight-header bytes, or
/// `None` when the literals can't be Huffman-coded (fewer than 2 distinct
/// symbols, an unbuildable table, or weights that don't fit a header). Cheap
/// (O(alphabet)); the expensive per-byte stream encoding is deferred to
/// [`assemble_compressed_section`] so the caller can *size* candidates first.
fn build_fresh_table(freq: &[u32; 256]) -> Option<(CodeTable, Vec<u8>)> {
    if freq.iter().filter(|&&c| c > 0).count() < 2 {
        return None;
    }
    let lengths = code_lengths(freq);
    let table = build_code_table(&lengths).ok()?;
    let mut weight_header = Vec::new();
    write_weight_header(&mut weight_header, &table).ok()?;
    Some((table, weight_header))
}

/// Assemble a Compressed (block_type 2) section from a prebuilt table + its
/// weight header, encoding the streams now. Split from [`build_fresh_table`] so
/// [`write_literals_auto`] can compare candidate sizes (via [`streams_byte_len`])
/// before committing to the one expensive stream encode.
fn assemble_compressed_section(
    table: &CodeTable,
    weight_header: &[u8],
    lits: &[u8],
) -> Result<Vec<u8>> {
    let (four, streams) = encode_lit_streams(table, lits)?;
    let mut payload = Vec::with_capacity(weight_header.len() + streams.len());
    payload.extend_from_slice(weight_header);
    payload.extend_from_slice(&streams);
    let mut out = Vec::new();
    write_compressed_lit_header(&mut out, 2, four, lits.len(), payload.len())?;
    out.extend_from_slice(&payload);
    Ok(out)
}

/// Exact byte length of the Huffman stream payload (`encode_lit_streams` output,
/// no section header) for `table` over `lits`, and whether the 4-stream form is
/// used — computed without the bit packing. Each stream is
/// `ceil((Σ code_len + 1 sentinel) / 8)` bytes (see [`encode_stream`]); the
/// 4-stream form adds a 6-byte jump table. `None` when a 4-stream substream would
/// overflow the u16 jump-table field (the real [`encode_4stream`] errors there).
fn streams_byte_len(table: &CodeTable, lits: &[u8]) -> Option<(bool, usize)> {
    let n = lits.len();
    let four = n >= 256 && n.div_ceil(4) * 3 <= n;
    let stream_len = |seg: &[u8]| -> usize {
        let bits: u64 = seg.iter().map(|&b| table.nbits[b as usize] as u64).sum();
        // +1 for the sentinel bit, rounded up to whole bytes (see `encode_stream`).
        (bits + 1).div_ceil(8) as usize
    };
    if four {
        let seg = n.div_ceil(4);
        let s1 = stream_len(&lits[0..seg]);
        let s2 = stream_len(&lits[seg..2 * seg]);
        let s3 = stream_len(&lits[2 * seg..3 * seg]);
        let s4 = stream_len(&lits[3 * seg..]);
        if s1 > u16::MAX as usize || s2 > u16::MAX as usize || s3 > u16::MAX as usize {
            return None; // jump-table field can't hold the length -> encoder errors
        }
        Some((true, 6 + s1 + s2 + s3 + s4))
    } else {
        Some((false, stream_len(lits)))
    }
}

/// Byte length of [`write_compressed_lit_header`] for the given form / sizes, or
/// `None` if `regen`/`comp` exceed the largest `Size_Format` (the writer errors).
fn compressed_header_len(four: bool, regen: usize, comp: usize) -> Option<usize> {
    if !four {
        (regen <= 0x3FF && comp <= 0x3FF).then_some(3)
    } else if regen <= 0x3FF && comp <= 0x3FF {
        Some(3)
    } else if regen <= 0x3FFF && comp <= 0x3FFF {
        Some(4)
    } else if regen <= 0x3FFFF && comp <= 0x3FFFF {
        Some(5)
    } else {
        None
    }
}

/// Build a **Treeless** (block_type 3) literals section — the streams only,
/// encoded with the previously-sent `table` (no tree description). The caller
/// must ensure `table.can_encode(literals)`.
fn build_treeless_section(literals: &[u8], table: &CodeTable) -> Result<Vec<u8>> {
    let (four, streams) = encode_lit_streams(table, literals)?;
    let mut out = Vec::new();
    write_compressed_lit_header(&mut out, 3, four, literals.len(), streams.len())?;
    out.extend_from_slice(&streams);
    Ok(out)
}

/// Encode `literals` as a compressed Huffman literals section, appended to
/// `out`. Errors (e.g. fewer than 2 distinct symbols, or a symbol > 128 while
/// only direct weights are supported) leave it to the caller to fall back to a
/// raw/RLE literals block.
pub fn write_literals_section(out: &mut Vec<u8>, literals: &[u8]) -> Result<()> {
    let (section, _table) = build_compressed_section(literals)?;
    out.extend_from_slice(&section);
    Ok(())
}

/// Write a **Raw** literals section (`block_type 0`): the bytes verbatim with a
/// 1-/2-/3-byte size header (5-/12-/20-bit `Regenerated_Size`).
pub fn write_raw_literals(out: &mut Vec<u8>, lits: &[u8]) {
    let regen = lits.len();
    debug_assert!(regen < (1 << 20), "raw literals {regen} exceed 20-bit size");
    if regen < 32 {
        // Size_Format 0: 1 byte, 5-bit size.
        out.push((regen as u8) << 3);
    } else if regen < 4096 {
        // Size_Format 1: 2 bytes, 12-bit size.
        out.push((1 << 2) | ((regen as u8 & 0xF) << 4));
        out.push((regen >> 4) as u8);
    } else {
        // Size_Format 3: 3 bytes, 20-bit size.
        out.push((3 << 2) | (((regen & 0xF) as u8) << 4));
        out.push(((regen >> 4) & 0xFF) as u8);
        out.push((regen >> 12) as u8);
    }
    out.extend_from_slice(lits);
}

/// Write the smallest literals section among Raw, a fresh Compressed (Huffman)
/// block, and — when `prev` (the previous block's Huffman table) can encode
/// every byte — a Treeless block reusing it (no tree description). Returns the
/// Huffman table now current for the next block: the fresh one if a Compressed
/// block was chosen, otherwise `prev` unchanged (Raw and Treeless leave the
/// decoder's cached table as it was), mirroring [`crate::literals::decode`].
pub fn write_literals_auto(
    out: &mut Vec<u8>,
    lits: &[u8],
    prev: Option<&CodeTable>,
) -> Option<CodeTable> {
    // Pick the smallest candidate by its *exact* size, computed without encoding
    // any streams, then build only the winner. This avoids the old "encode all
    // three, keep smallest" (up to two full Huffman encodes + a raw copy per
    // block). A wrong size estimate could only pick a sub-optimal candidate (a
    // ratio change, caught by tests/corpus) — never an invalid section, since the
    // chosen candidate is still built in full.
    let mut freq = [0u32; 256];
    for &b in lits {
        freq[b as usize] += 1;
    }
    let fresh = build_fresh_table(&freq);

    let n = lits.len();
    let compressed_size = fresh.as_ref().and_then(|(t, wh)| {
        let (four, slen) = streams_byte_len(t, lits)?;
        let payload = wh.len() + slen;
        Some(compressed_header_len(four, n, payload)? + payload)
    });
    let treeless_size = prev.and_then(|t| {
        if !t.can_encode_hist(&freq) {
            return None;
        }
        let (four, slen) = streams_byte_len(t, lits)?;
        Some(compressed_header_len(four, n, slen)? + slen)
    });
    let raw_size = n + raw_header_len(n);

    // Raw wins ties (it was first in the old `min_by_key`), so a compressing
    // candidate must be *strictly* smaller; compressed is tried before treeless,
    // so it wins their ties too.
    enum Choice {
        Raw,
        Compressed,
        Treeless,
    }
    let mut choice = Choice::Raw;
    let mut best = raw_size;
    if let Some(c) = compressed_size {
        if c < best {
            best = c;
            choice = Choice::Compressed;
        }
    }
    if let Some(t) = treeless_size {
        if t < best {
            choice = Choice::Treeless;
        }
    }

    match choice {
        // `fresh`/`prev` are guaranteed `Some` here: the size was `Some`.
        Choice::Compressed => {
            let (table, weight_header) = fresh.unwrap();
            match assemble_compressed_section(&table, &weight_header, lits) {
                Ok(bytes) => {
                    out.extend_from_slice(&bytes);
                    Some(table)
                }
                Err(_) => {
                    write_raw_literals(out, lits);
                    prev.cloned()
                }
            }
        }
        Choice::Treeless => match build_treeless_section(lits, prev.unwrap()) {
            Ok(bytes) => {
                out.extend_from_slice(&bytes);
                prev.cloned()
            }
            Err(_) => {
                write_raw_literals(out, lits);
                prev.cloned()
            }
        },
        Choice::Raw => {
            write_raw_literals(out, lits);
            prev.cloned()
        }
    }
}

/// Header byte count [`write_raw_literals`] emits for `regen` literal bytes
/// (Size_Format 0/1/3: 5-/12-/20-bit size). Lets [`write_literals_auto`] size the
/// raw candidate without materializing it.
#[inline]
fn raw_header_len(regen: usize) -> usize {
    if regen < 32 {
        1
    } else if regen < 4096 {
        2
    } else {
        3
    }
}

/// Rebuild the encode [`CodeTable`] from a decoded Huffman table — e.g. a
/// structured dictionary's preset literals table — so the encoder can seed a
/// dict-primed first block and emit Treeless blocks against it. Each symbol's
/// code length is read out of the decode table and run back through
/// [`build_code_table`], yielding the exact inverse of `ht`.
pub(crate) fn code_table_from_huff(ht: &huff::HuffTable) -> Result<CodeTable> {
    let mut lengths = [0u8; 256];
    for (&sym, &nb) in ht.symbols.iter().zip(ht.num_bits.iter()) {
        lengths[sym as usize] = nb;
    }
    build_code_table(&lengths)
}

/// Write a standalone Huffman table description (the weight header) for a
/// **structured dictionary**'s entropy section, built from the literal-byte
/// histogram `freq`. This is the same tree description that prefixes a Huffman
/// literals block — exactly what [`huff::read_table`] and libzstd's dictionary
/// loader read back. Errors (so the caller can fall back) when fewer than two
/// distinct symbols are present, since a Huffman alphabet needs at least two.
pub(crate) fn write_dict_huffman_table(out: &mut Vec<u8>, freq: &[u32; 256]) -> Result<()> {
    if freq.iter().filter(|&&f| f > 0).count() < 2 {
        return Err(ZstdError::Invalid {
            what: "dictionary huffman table",
            detail: "fewer than two distinct literal symbols".into(),
        });
    }
    let lengths = code_lengths(freq);
    let table = build_code_table(&lengths)?;
    write_weight_header(out, &table)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::huff::{decode_1stream, decode_4stream};
    use crate::literals;

    /// A reproducible pseudo-random byte stream over a restricted alphabet so the
    /// direct weight header (max symbol ≤ 128) applies.
    fn skewed_bytes(n: usize, alphabet: u32, seed: u64) -> Vec<u8> {
        let mut s = seed | 1;
        (0..n)
            .map(|_| {
                s = s
                    .wrapping_mul(6364136223846793005)
                    .wrapping_add(1442695040888963407);
                // Square the uniform to skew toward small symbols (richer Huffman).
                let u = (s >> 33) as u32 % alphabet;
                ((u * u) / alphabet) as u8
            })
            .collect()
    }

    /// The analytic candidate sizes `write_literals_auto` chooses by must equal
    /// the byte length of the section actually built — otherwise it could pick a
    /// sub-optimal candidate (a silent ratio regression). Cover both the direct
    /// (small alphabet) and FSE (alphabet > 128) weight headers, the 1-stream
    /// (n < 256) and 4-stream forms, and the size-format boundaries.
    #[test]
    fn analytic_section_sizes_match_built_sections() {
        for &alphabet in &[6u32, 60, 200] {
            for &n in &[2usize, 5, 64, 255, 256, 257, 1024, 4096, 16384, 70000] {
                let lits = skewed_bytes(n, alphabet, 0xABCD_1234 ^ n as u64);
                let mut freq = [0u32; 256];
                for &b in &lits {
                    freq[b as usize] += 1;
                }
                let Some((table, wh)) = build_fresh_table(&freq) else {
                    continue;
                };

                // Compressed (block_type 2): header + weight header + streams.
                if let Ok(section) = assemble_compressed_section(&table, &wh, &lits) {
                    let (four, slen) = streams_byte_len(&table, &lits).unwrap();
                    let payload = wh.len() + slen;
                    let hlen = compressed_header_len(four, n, payload).unwrap();
                    assert_eq!(
                        section.len(),
                        hlen + payload,
                        "compressed size mismatch (alphabet {alphabet}, n {n})"
                    );
                }

                // Treeless (block_type 3): header + streams, reusing the table.
                if table.can_encode_hist(&freq) {
                    if let Ok(section) = build_treeless_section(&lits, &table) {
                        let (four, slen) = streams_byte_len(&table, &lits).unwrap();
                        let hlen = compressed_header_len(four, n, slen).unwrap();
                        assert_eq!(
                            section.len(),
                            hlen + slen,
                            "treeless size mismatch (alphabet {alphabet}, n {n})"
                        );
                    }
                }
            }
        }
    }

    #[test]
    fn single_stream_round_trips_through_decoder() {
        for &(n, alpha) in &[(2usize, 2u32), (17, 8), (100, 60), (500, 120)] {
            let data = skewed_bytes(n, alpha, n as u64 * 7 + alpha as u64);
            let mut freq = [0u32; 256];
            for &b in &data {
                freq[b as usize] += 1;
            }
            if freq.iter().filter(|&&c| c > 0).count() < 2 {
                continue;
            }
            let lengths = code_lengths(&freq);
            let table = build_code_table(&lengths).unwrap();
            let stream = encode_stream(&table, &data);
            // Rebuild the decode table the way the real decoder would (from the
            // weight header) and decode.
            let mut hdr = Vec::new();
            write_weight_header(&mut hdr, &table).unwrap();
            let (dtable, _) = huff::read_table(&hdr).unwrap();
            let got = decode_1stream(&dtable, &stream, data.len()).unwrap();
            assert_eq!(got, data, "1-stream mismatch n={n} alpha={alpha}");
        }
    }

    #[test]
    fn four_stream_round_trips_through_decoder() {
        let data = skewed_bytes(2000, 100, 99);
        let mut freq = [0u32; 256];
        for &b in &data {
            freq[b as usize] += 1;
        }
        let lengths = code_lengths(&freq);
        let table = build_code_table(&lengths).unwrap();
        let streams = encode_4stream(&table, &data).unwrap();
        let mut hdr = Vec::new();
        write_weight_header(&mut hdr, &table).unwrap();
        let (dtable, _) = huff::read_table(&hdr).unwrap();
        let got = decode_4stream(&dtable, &streams, data.len()).unwrap();
        assert_eq!(got, data);
    }

    #[test]
    fn literals_section_round_trips() {
        for &(n, alpha) in &[(2usize, 2u32), (50, 40), (300, 90), (5000, 120)] {
            let data = skewed_bytes(n, alpha, n as u64 + 1);
            let mut sect = Vec::new();
            if write_literals_section(&mut sect, &data).is_err() {
                continue;
            }
            let mut cache = None;
            let (got, consumed) = literals::decode(&sect, &mut cache).unwrap();
            assert_eq!(got, data, "literals section mismatch n={n}");
            assert_eq!(consumed, sect.len(), "trailing bytes after literals");
        }
    }

    /// Full-byte-alphabet literals (highest symbol > 128) must round-trip
    /// through the **FSE-compressed** weight-header path.
    #[test]
    fn fse_weights_literals_round_trip() {
        // Dominant low symbols + a sparse high tail: max symbol > 128 (forces
        // FSE weights) with weight gaps (so >= 2 distinct weight values), and
        // skewed enough that the weights actually compress under 128 bytes.
        let mut data = Vec::new();
        for i in 0..6000u32 {
            data.push((i % 24) as u8);
        }
        for k in 0..400u32 {
            data.push((130 + (k * 13) % 120) as u8);
        }
        let max_symbol = (0..256).rev().find(|&s| data.contains(&(s as u8))).unwrap();
        assert!(max_symbol > 128, "test must exercise the FSE-weights path");

        let mut sect = Vec::new();
        write_literals_section(&mut sect, &data).expect("FSE-weights literals section");
        // The weight header (first byte after the literals block header) must be
        // the FSE form (< 128), not direct (>= 128). Header length follows the
        // compressed Size_Format: 0/1 → 3 bytes, 2 → 4, 3 → 5.
        let hdr_len = match (sect[0] >> 2) & 3 {
            0 | 1 => 3,
            2 => 4,
            _ => 5,
        };
        let weight_byte = sect[hdr_len];
        assert!(
            weight_byte < 128,
            "expected FSE weight header, got {weight_byte}"
        );

        let mut cache = None;
        let (got, consumed) = literals::decode(&sect, &mut cache).unwrap();
        assert_eq!(got, data);
        assert_eq!(consumed, sect.len());
    }

    /// Code lengths must form a complete prefix code within the limit.
    #[test]
    fn code_lengths_are_complete_and_bounded() {
        let data = skewed_bytes(10_000, 128, 12345);
        let mut freq = [0u32; 256];
        for &b in &data {
            freq[b as usize] += 1;
        }
        let lengths = code_lengths(&freq);
        let mut kraft = 0.0f64;
        let mut maxlen = 0u8;
        for &len in lengths.iter() {
            if len > 0 {
                assert!(len <= MAX_CODE_LEN, "length {len} > limit");
                maxlen = maxlen.max(len);
                kraft += 2f64.powi(-(len as i32));
            }
        }
        assert!(maxlen <= MAX_CODE_LEN);
        assert!((kraft - 1.0).abs() < 1e-9, "Kraft sum {kraft} != 1");
    }

    /// A second block over the same alphabet reuses the first block's Huffman
    /// table via a **Treeless** section (no tree description), so it is smaller
    /// than re-describing the tree, and the pair round-trips through the literals
    /// decoder with one table cache threaded across them exactly as the encoder
    /// threaded it.
    #[test]
    fn treeless_reuses_previous_table_and_round_trips() {
        let lits1 = skewed_bytes(3000, 60, 11);
        // Same multiset of symbols (reversed) so the first block's table can
        // encode the second — the validity condition for Treeless reuse.
        let lits2: Vec<u8> = lits1.iter().rev().copied().collect();

        let mut sec1 = Vec::new();
        let t1 = write_literals_auto(&mut sec1, &lits1, None);
        assert!(
            t1.is_some(),
            "block 1 should pick a compressed (tree) section"
        );

        let mut sec2 = Vec::new();
        let _t2 = write_literals_auto(&mut sec2, &lits2, t1.as_ref());

        // Treeless must beat a fresh compressed block 2 (it drops the tree header).
        let mut fresh2 = Vec::new();
        write_literals_section(&mut fresh2, &lits2).unwrap();
        assert!(
            sec2.len() < fresh2.len(),
            "treeless block ({}) should beat a fresh tree block ({})",
            sec2.len(),
            fresh2.len()
        );

        // Decode both, threading one cache across them as the encoder did.
        let mut cache = None;
        let (d1, _) = literals::decode(&sec1, &mut cache).unwrap();
        assert_eq!(d1, lits1, "block 1 round-trip mismatch");
        let (d2, _) = literals::decode(&sec2, &mut cache).unwrap();
        assert_eq!(d2, lits2, "treeless block round-trip mismatch");
    }
}
