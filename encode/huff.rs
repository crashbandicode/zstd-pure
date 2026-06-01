//! Huff0 (Huffman) literal **encoding** — RFC 8478 §4.2.1, the inverse of the
//! decoder in [`crate::zstd_pure::huff`].
//!
//! Strategy (see `zstd_pure/README.md`):
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

/// Hard cap on emitted code length. zstd's literals Huffman table log is 11.
const MAX_CODE_LEN: u8 = 11;

/// Per-symbol canonical code + bit count, plus the weight header inputs.
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
    use std::cmp::Reverse;
    use std::collections::BinaryHeap;
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

/// Forward LSB bit accumulator mirroring libzstd's `BIT_CStream`.
struct BitWriter {
    acc: u64,
    nbits: u32,
    out: Vec<u8>,
}

impl BitWriter {
    fn new() -> Self {
        BitWriter {
            acc: 0,
            nbits: 0,
            out: Vec::new(),
        }
    }

    /// Append the low `nb` bits of `value` (nb ≤ ~24; we only ever push ≤ 11).
    #[inline]
    fn add(&mut self, value: u32, nb: u32) {
        self.acc |= (value as u64) << self.nbits;
        self.nbits += nb;
        while self.nbits >= 8 {
            self.out.push(self.acc as u8);
            self.acc >>= 8;
            self.nbits -= 8;
        }
    }

    /// Cap with the sentinel `1` bit and flush the final partial byte.
    fn finish(mut self) -> Vec<u8> {
        self.add(1, 1);
        if self.nbits > 0 {
            self.out.push(self.acc as u8);
        }
        self.out
    }
}

/// Encode one literal sub-stream. Symbols are emitted in **reverse** order so
/// the decoder's reverse bitstream reader replays them forwards.
fn encode_stream(table: &CodeTable, lits: &[u8]) -> Vec<u8> {
    let mut bw = BitWriter::new();
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

/// Write the **direct** Huffman weight header (4-bit packed, high nibble first).
/// Only valid when `max_symbol ≤ 128`; larger alphabets need FSE-compressed
/// weights (T2.1b).
fn write_weight_header_direct(out: &mut Vec<u8>, table: &CodeTable) -> Result<()> {
    let num_weights = table.max_symbol;
    if num_weights > 128 {
        return Err(ZstdError::Invalid {
            what: "huffman weights",
            detail: "direct weights need highest symbol <= 128 (FSE weights pending)".into(),
        });
    }
    out.push((127 + num_weights) as u8);
    let w = &table.weights;
    let mut i = 0;
    while i < num_weights {
        let hi = w[i];
        let lo = if i + 1 < num_weights { w[i + 1] } else { 0 };
        out.push((hi << 4) | lo);
        i += 2;
    }
    Ok(())
}

/// Write the compressed/treeless literals header (RFC 8478 §3.1.1.3.1.1),
/// selecting the smallest `Size_Format` that fits `regen` + `comp`.
fn write_compressed_lit_header(
    out: &mut Vec<u8>,
    four: bool,
    regen: usize,
    comp: usize,
) -> Result<()> {
    const COMPRESSED: u32 = 2;
    let too_big = || ZstdError::Invalid {
        what: "compressed literals header",
        detail: format!("regen {regen} / comp {comp} exceed 18-bit fields"),
    };
    if !four {
        // Size_Format 0: single stream, 10-bit regen/comp, 3-byte header.
        if regen > 0x3FF || comp > 0x3FF {
            return Err(too_big());
        }
        let v = COMPRESSED | (regen as u32) << 4 | (comp as u32) << 14;
        out.extend_from_slice(&v.to_le_bytes()[..3]);
    } else if regen <= 0x3FF && comp <= 0x3FF {
        // Size_Format 1: four streams, 10-bit, 3-byte header.
        let v = COMPRESSED | 1 << 2 | (regen as u32) << 4 | (comp as u32) << 14;
        out.extend_from_slice(&v.to_le_bytes()[..3]);
    } else if regen <= 0x3FFF && comp <= 0x3FFF {
        // Size_Format 2: four streams, 14-bit, 4-byte header.
        let v = COMPRESSED | 2 << 2 | (regen as u32) << 4 | (comp as u32) << 18;
        out.extend_from_slice(&v.to_le_bytes());
    } else if regen <= 0x3FFFF && comp <= 0x3FFFF {
        // Size_Format 3: four streams, 18-bit, 5-byte header.
        let v = COMPRESSED as u64 | 3 << 2 | (regen as u64) << 4 | (comp as u64) << 22;
        out.extend_from_slice(&v.to_le_bytes()[..5]);
    } else {
        return Err(too_big());
    }
    Ok(())
}

/// Encode `literals` as a compressed Huffman literals section, appended to
/// `out`. Errors (e.g. fewer than 2 distinct symbols, or a symbol > 128 while
/// only direct weights are supported) leave it to the caller to fall back to a
/// raw/RLE literals block.
pub fn write_literals_section(out: &mut Vec<u8>, literals: &[u8]) -> Result<()> {
    let mut freq = [0u32; 256];
    for &b in literals {
        freq[b as usize] += 1;
    }
    if freq.iter().filter(|&&c| c > 0).count() < 2 {
        return Err(ZstdError::Invalid {
            what: "huffman literals",
            detail: "need >= 2 distinct symbols".into(),
        });
    }

    let lengths = code_lengths(&freq);
    let table = build_code_table(&lengths)?;

    let mut payload = Vec::new();
    write_weight_header_direct(&mut payload, &table)?;

    let n = literals.len();
    // Four-stream form amortizes the 6-byte jump table only once it's both valid
    // (`3·ceil(n/4) ≤ n`) and large enough to pay for itself.
    let four = n >= 256 && n.div_ceil(4) * 3 <= n;
    if four {
        payload.extend_from_slice(&encode_4stream(&table, literals)?);
    } else {
        payload.extend_from_slice(&encode_stream(&table, literals));
    }

    write_compressed_lit_header(out, four, n, payload.len())?;
    out.extend_from_slice(&payload);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::zstd_pure::huff::{decode_1stream, decode_4stream};
    use crate::zstd_pure::literals;

    /// A reproducible pseudo-random byte stream over a restricted alphabet so the
    /// direct weight header (max symbol ≤ 128) applies.
    fn skewed_bytes(n: usize, alphabet: u32, seed: u64) -> Vec<u8> {
        let mut s = seed | 1;
        (0..n)
            .map(|_| {
                s = s.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
                // Square the uniform to skew toward small symbols (richer Huffman).
                let u = (s >> 33) as u32 % alphabet;
                ((u * u) / alphabet) as u8
            })
            .collect()
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
            write_weight_header_direct(&mut hdr, &table).unwrap();
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
        write_weight_header_direct(&mut hdr, &table).unwrap();
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
}
