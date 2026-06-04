//! Huff0 (Huffman) literal decoding — RFC 8878 §4.2.1.
//!
//! Reads the Huffman table description (weights are either FSE-compressed or
//! packed 4-bits direct), reconstructs the decode table, and decodes the 1- or
//! 4-stream literal bitstreams. The built [`HuffTable`] is cacheable so the
//! "treeless" literal block type can reuse the previous block's table.

use super::bits::{ReloadStatus, ReverseBitReader};
use super::error::{Result, ZstdError};
use super::fse;
#[allow(unused_imports)]
use crate::alloc_prelude::*;

/// Maximum Huffman code length / table log.
const HUF_TABLELOG_MAX: u32 = 12;

#[inline]
fn highbit32(x: u32) -> u32 {
    31 - x.leading_zeros()
}

/// A built Huffman decode table: `symbols[code]`/`num_bits[code]` indexed by the
/// top `max_bits` bits of the stream.
#[derive(Debug, Clone)]
pub struct HuffTable {
    pub max_bits: u32,
    pub symbols: Vec<u8>,
    pub num_bits: Vec<u8>,
}

/// Read a Huffman table description from the front of `src`; returns the table
/// and the number of header bytes consumed (weights description + size byte).
pub fn read_table(src: &[u8]) -> Result<(HuffTable, usize)> {
    if src.is_empty() {
        return Err(ZstdError::Truncated {
            what: "huffman weight header",
            needed: 1,
        });
    }
    let h = src[0] as usize;
    let (weights, hdr_bytes) = if h < 128 {
        // FSE-compressed weights: `h` is the compressed size (table desc + stream).
        let comp_size = h;
        if src.len() < 1 + comp_size {
            return Err(ZstdError::Truncated {
                what: "fse-compressed huffman weights",
                needed: 1 + comp_size - src.len(),
            });
        }
        let nc = fse::read_ncount(&src[1..1 + comp_size], 6)?;
        let table = fse::build_dtable(&nc.counts, nc.max_symbol, nc.table_log)?;
        let bitstream = &src[1 + nc.bytes_consumed..1 + comp_size];
        let weights = fse::decompress(bitstream, &table, 256)?;
        (weights, 1 + comp_size)
    } else {
        // Direct weights: `h - 127` weights, packed two-per-byte (high nibble
        // first).
        let num_weights = h - 127;
        let bytes = num_weights.div_ceil(2);
        if src.len() < 1 + bytes {
            return Err(ZstdError::Truncated {
                what: "direct huffman weights",
                needed: 1 + bytes - src.len(),
            });
        }
        let mut weights = Vec::with_capacity(num_weights);
        for i in 0..num_weights {
            let byte = src[1 + i / 2];
            let w = if i % 2 == 0 { byte >> 4 } else { byte & 0x0F };
            weights.push(w);
        }
        (weights, 1 + bytes)
    };

    let table = build_table(&weights)?;
    Ok((table, hdr_bytes))
}

/// Reconstruct the Huffman decode table from the per-symbol weight list (the
/// implicit final symbol's weight is derived so the total is a power of two).
///
/// Exposed to the encoder (`encode::huff`) so it can derive per-symbol canonical
/// codes from the *same* table the decoder uses — keeping encode and decode in
/// exact lockstep instead of hand-rolling a parallel code assignment.
pub(crate) fn build_table(weights: &[u8]) -> Result<HuffTable> {
    let mut total_weight: u32 = 0;
    for &w in weights {
        if w as u32 > HUF_TABLELOG_MAX {
            return Err(ZstdError::CorruptTable(format!(
                "huffman weight {w} too large"
            )));
        }
        if w > 0 {
            total_weight += 1 << (w - 1);
        }
    }
    if total_weight == 0 {
        return Err(ZstdError::CorruptTable("huffman weights all zero".into()));
    }
    let max_bits = highbit32(total_weight) + 1;
    if max_bits > HUF_TABLELOG_MAX {
        return Err(ZstdError::CorruptTable(format!(
            "huffman max code length {max_bits} too large"
        )));
    }
    let left = (1u32 << max_bits) - total_weight;
    if left == 0 || (left & (left - 1)) != 0 {
        return Err(ZstdError::CorruptTable(
            "huffman residual weight is not a power of two".into(),
        ));
    }
    let last_weight = highbit32(left) + 1;

    let mut all_weights = Vec::with_capacity(weights.len() + 1);
    all_weights.extend_from_slice(weights);
    all_weights.push(last_weight as u8);
    if all_weights.len() > 256 {
        return Err(ZstdError::CorruptTable(
            "huffman alphabet exceeds 256".into(),
        ));
    }

    let mut rank_stats = [0u32; (HUF_TABLELOG_MAX + 1) as usize];
    for &w in &all_weights {
        rank_stats[w as usize] += 1;
    }
    let mut rank_start = [0u32; (HUF_TABLELOG_MAX + 1) as usize];
    let mut next = 0u32;
    for w in 1..=max_bits {
        let cur = next;
        next += rank_stats[w as usize] << (w - 1);
        rank_start[w as usize] = cur;
    }

    let size = 1usize << max_bits;
    let mut symbols = vec![0u8; size];
    let mut num_bits = vec![0u8; size];
    for (s, &w) in all_weights.iter().enumerate() {
        if w == 0 {
            continue;
        }
        let length = 1usize << (w - 1);
        let nb = max_bits + 1 - w as u32;
        let start = rank_start[w as usize] as usize;
        if start + length > size {
            return Err(ZstdError::CorruptTable("huffman table overflow".into()));
        }
        for u in start..start + length {
            symbols[u] = s as u8;
            num_bits[u] = nb as u8;
        }
        rank_start[w as usize] += length as u32;
    }

    Ok(HuffTable {
        max_bits,
        symbols,
        num_bits,
    })
}

#[inline]
fn decode_into(table: &HuffTable, src: &[u8], count: usize, out: &mut Vec<u8>) -> Result<()> {
    let mut br = ReverseBitReader::new(src)?;
    out.reserve(count);
    let max_bits = table.max_bits;
    let mut i = 0usize;

    // Fast path: decode 4 symbols per reload (libzstd's `HUF_decodeStreamX1`). A
    // refilled 64-bit window serves `4 * max_bits` bits; with `max_bits <= 14`
    // that is ≤ 56, and `reload()` leaves ≤ 7 bits already consumed, so all four
    // `peek(max_bits)` stay within the window. `reload() == Unfinished` guarantees
    // a full window was loaded (for `n >= 8` streams `ptr` never exceeds
    // `len - 8`, so the load is never short); any other status drops to the tail.
    if max_bits <= 14 {
        while i + 4 <= count && br.reload() == ReloadStatus::Unfinished {
            for _ in 0..4 {
                let code = br.peek(max_bits) as usize;
                out.push(table.symbols[code]);
                br.consume(table.num_bits[code] as u32);
            }
            i += 4;
        }
    }

    // Tail: one symbol per reload — robust near the start of the stream and for
    // short streams / large `max_bits`.
    while i < count {
        br.reload();
        let code = br.peek(max_bits) as usize;
        out.push(table.symbols[code]);
        br.consume(table.num_bits[code] as u32);
        i += 1;
    }
    Ok(())
}

/// Decode a single-stream Huffman literal block (`regen_size` symbols).
pub fn decode_1stream(table: &HuffTable, src: &[u8], regen_size: usize) -> Result<Vec<u8>> {
    let mut out = Vec::with_capacity(regen_size);
    decode_into(table, src, regen_size, &mut out)?;
    Ok(out)
}

/// Decode one Huffman symbol, refilling the window only when the next `max_bits`
/// peek would not fit. Used by the interleaved 4-stream decoder, where four of
/// these run in lockstep so their (otherwise serial: peek → table → consume)
/// dependency chains overlap.
#[inline]
fn huf_one(br: &mut ReverseBitReader, table: &HuffTable) -> u8 {
    br.ensure(table.max_bits);
    let code = br.peek(table.max_bits) as usize;
    let s = table.symbols[code];
    br.consume(table.num_bits[code] as u32);
    s
}

/// Decode a 4-stream Huffman literal block. The first 6 bytes are a jump table
/// (three little-endian `u16` stream sizes); the four streams each produce
/// `ceil(regen_size/4)` symbols (the last takes the remainder).
pub fn decode_4stream(table: &HuffTable, src: &[u8], regen_size: usize) -> Result<Vec<u8>> {
    if src.len() < 6 {
        return Err(ZstdError::Truncated {
            what: "huffman 4-stream jump table",
            needed: 6 - src.len(),
        });
    }
    let len1 = u16::from_le_bytes([src[0], src[1]]) as usize;
    let len2 = u16::from_le_bytes([src[2], src[3]]) as usize;
    let len3 = u16::from_le_bytes([src[4], src[5]]) as usize;
    let mut p = 6usize;
    let bounds = |p: usize, len: usize| -> Result<()> {
        if p + len > src.len() {
            Err(ZstdError::Truncated {
                what: "huffman 4-stream segment",
                needed: p + len - src.len(),
            })
        } else {
            Ok(())
        }
    };
    bounds(p, len1)?;
    let s1 = &src[p..p + len1];
    p += len1;
    bounds(p, len2)?;
    let s2 = &src[p..p + len2];
    p += len2;
    bounds(p, len3)?;
    let s3 = &src[p..p + len3];
    p += len3;
    let s4 = &src[p..];

    let seg = regen_size.div_ceil(4);
    if seg * 3 > regen_size {
        return Err(ZstdError::Invalid {
            what: "huffman 4-stream size",
            detail: format!("regen {regen_size} too small for 4 streams"),
        });
    }
    let last = regen_size - seg * 3;
    assert!(last <= seg); // regen <= 4*seg, so the remainder never exceeds a segment
    let mut out = vec![0u8; regen_size];
    {
        // The four output segments, written by four independent decoders.
        let (a, r) = out.split_at_mut(seg);
        let (b, r) = r.split_at_mut(seg);
        let (c, d) = r.split_at_mut(seg); // c: seg, d: last (the remainder)
        let mut r0 = ReverseBitReader::new(s1)?;
        let mut r1 = ReverseBitReader::new(s2)?;
        let mut r2 = ReverseBitReader::new(s3)?;
        let mut r3 = ReverseBitReader::new(s4)?;
        let mb = table.max_bits;

        // One symbol from a reader into `dst[idx]`, no reload (the caller refills).
        macro_rules! step {
            ($r:expr, $dst:expr, $idx:expr) => {{
                let code = $r.peek(mb) as usize;
                $dst[$idx] = table.symbols[code];
                $r.consume(table.num_bits[code] as u32);
            }};
        }

        // Interleave the four streams so their (serial: peek→table→consume) decode
        // chains overlap — the point of the 4-stream layout (libzstd's
        // HUF_decompress4X). Fast loops do 4 symbols per reader per reload while all
        // active readers hold a full window; per-symbol tails finish the rest.
        // Phase 1: all four streams, for the shortest length (`last`).
        let mut i = 0usize;
        while i + 4 <= last
            && (r0.reload(), r1.reload(), r2.reload(), r3.reload())
                == (
                    ReloadStatus::Unfinished,
                    ReloadStatus::Unfinished,
                    ReloadStatus::Unfinished,
                    ReloadStatus::Unfinished,
                )
        {
            for k in 0..4 {
                step!(r0, a, i + k);
                step!(r1, b, i + k);
                step!(r2, c, i + k);
                step!(r3, d, i + k);
            }
            i += 4;
        }
        while i < last {
            a[i] = huf_one(&mut r0, table);
            b[i] = huf_one(&mut r1, table);
            c[i] = huf_one(&mut r2, table);
            d[i] = huf_one(&mut r3, table);
            i += 1;
        }
        // Phase 2: streams 1–3 carry `seg`, one segment longer than stream 4.
        while i + 4 <= seg
            && (r0.reload(), r1.reload(), r2.reload())
                == (
                    ReloadStatus::Unfinished,
                    ReloadStatus::Unfinished,
                    ReloadStatus::Unfinished,
                )
        {
            for k in 0..4 {
                step!(r0, a, i + k);
                step!(r1, b, i + k);
                step!(r2, c, i + k);
            }
            i += 4;
        }
        while i < seg {
            a[i] = huf_one(&mut r0, table);
            b[i] = huf_one(&mut r1, table);
            c[i] = huf_one(&mut r2, table);
            i += 1;
        }
    }
    Ok(out)
}
