//! Sequence section decoding + LZ execution — RFC 8878 §3.1.1.3.2.
//!
//! Parses the sequence count and the LL/Offset/ML FSE table modes (Predefined /
//! RLE / FSE / Repeat), then walks the reverse bitstream decoding each
//! (literals_length, match_length, offset) triple and reconstructing output by
//! copying literals and back-references (with the three repeat offsets).

use super::bits::ReverseBitReader;
use super::error::{Result, ZstdError};
use super::fse::{self, FseDecodeTable, FseDecoder};
#[allow(unused_imports)]
use crate::alloc_prelude::*;

/// Cached LL/OF/ML FSE tables (for the "Repeat" compression mode).
#[derive(Debug, Default, Clone)]
pub struct SeqTables {
    pub ll: Option<FseDecodeTable>,
    pub of: Option<FseDecodeTable>,
    pub ml: Option<FseDecodeTable>,
}

// Baseline + extra-bit tables (RFC 8878 §3.1.1.3.2.1.1). Exposed to the encoder
// (`encode::sequences`) so it computes codes/extra bits from the same tables.
pub(crate) const LL_BASE: [u32; 36] = [
    0, 1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15, 16, 18, 20, 22, 24, 28, 32, 40, 48, 64,
    128, 256, 512, 1024, 2048, 4096, 8192, 16384, 32768, 65536,
];
pub(crate) const LL_BITS: [u32; 36] = [
    0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 1, 1, 1, 1, 2, 2, 3, 3, 4, 6, 7, 8, 9, 10, 11,
    12, 13, 14, 15, 16,
];
pub(crate) const ML_BASE: [u32; 53] = [
    3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15, 16, 17, 18, 19, 20, 21, 22, 23, 24, 25, 26, 27,
    28, 29, 30, 31, 32, 33, 34, 35, 37, 39, 41, 43, 47, 51, 59, 67, 83, 99, 131, 259, 515, 1027,
    2051, 4099, 8195, 16387, 32771, 65539,
];
pub(crate) const ML_BITS: [u32; 53] = [
    0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0,
    1, 1, 1, 1, 2, 2, 3, 3, 4, 4, 5, 7, 8, 9, 10, 11, 12, 13, 14, 15, 16,
];

// Predefined (default) normalized distributions. Exposed to the encoder for
// the predefined sequence-table mode.
pub(crate) const LL_DEFAULT: [i16; 36] = [
    4, 3, 2, 2, 2, 2, 2, 2, 2, 2, 2, 2, 2, 1, 1, 1, 2, 2, 2, 2, 2, 2, 2, 2, 2, 3, 2, 1, 1, 1, 1, 1,
    -1, -1, -1, -1,
];
pub(crate) const OF_DEFAULT: [i16; 29] = [
    1, 1, 1, 1, 1, 1, 2, 2, 2, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, -1, -1, -1, -1, -1,
];
pub(crate) const ML_DEFAULT: [i16; 53] = [
    1, 4, 3, 2, 2, 2, 2, 2, 2, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1,
    1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, -1, -1, -1, -1, -1, -1, -1,
];

/// Which entropy table a symbol channel uses for this block.
enum Mode {
    Predefined,
    Rle(u8),
    Fse(FseDecodeTable),
    Repeat,
}

fn parse_mode(raw: u8, src: &[u8], p: &mut usize, max_log: u32) -> Result<Mode> {
    Ok(match raw {
        0 => Mode::Predefined,
        1 => {
            if *p >= src.len() {
                return Err(ZstdError::Truncated {
                    what: "RLE sequence symbol",
                    needed: 1,
                });
            }
            let s = src[*p];
            *p += 1;
            Mode::Rle(s)
        }
        2 => {
            let nc = fse::read_ncount(&src[*p..], max_log)?;
            *p += nc.bytes_consumed;
            Mode::Fse(fse::build_dtable(&nc.counts, nc.max_symbol, nc.table_log)?)
        }
        _ => Mode::Repeat,
    })
}

fn rle_table(symbol: u8) -> FseDecodeTable {
    let mut entries = Box::new([fse::FseEntry::default(); fse::FSE_DTABLE_SIZE]);
    entries[0] = fse::FseEntry {
        symbol,
        num_bits: 0,
        new_state_base: 0,
    };
    FseDecodeTable {
        table_log: 0,
        entries,
    }
}

/// Resolve / update the repeat offsets, returning the actual copy offset
/// (mirrors libzstd's `ZSTD_updateRep`). Exposed to the encoder
/// (`encode::lz`) so the match finder evolves `rep` in exact lockstep with the
/// decoder when it emits repeat-offset codes.
pub(crate) fn resolve_offset(rep: &mut [u32; 3], offset_value: u32, ll0: bool) -> u32 {
    if offset_value > 3 {
        rep[2] = rep[1];
        rep[1] = rep[0];
        rep[0] = offset_value - 3;
        rep[0]
    } else {
        let rep_code = offset_value - 1 + ll0 as u32;
        if rep_code == 0 {
            rep[0]
        } else {
            let off = if rep_code == 3 {
                // `rep[0] - 1`; on corrupt input rep[0] may be 0/1, which would
                // underflow — saturate to 0 so the caller's zero-offset check
                // rejects it instead of panicking.
                rep[0].saturating_sub(1)
            } else {
                rep[rep_code as usize]
            };
            if rep_code >= 2 {
                rep[2] = rep[1];
            }
            rep[1] = rep[0];
            rep[0] = off;
            off
        }
    }
}

#[inline]
fn copy_power_of_two_pattern(out: &mut Vec<u8>, start: usize, offset: usize, match_len: usize) {
    const CHUNK: usize = 64;

    debug_assert!(matches!(offset, 2 | 4 | 8));
    let mut chunk = [0u8; CHUNK];
    chunk[..offset].copy_from_slice(&out[start..start + offset]);

    let mut filled = offset;
    while filled < CHUNK {
        let n = filled.min(CHUNK - filled);
        let (prefix, rest) = chunk.split_at_mut(filled);
        rest[..n].copy_from_slice(&prefix[..n]);
        filled += n;
    }

    out.reserve(match_len);
    let mut remaining = match_len;
    while remaining >= CHUNK {
        out.extend_from_slice(&chunk);
        remaining -= CHUNK;
    }
    if remaining > 0 {
        out.extend_from_slice(&chunk[..remaining]);
    }
}

/// Decode the sequences section with no output ceiling — a test-only convenience
/// over [`decode_capped`] (production decode always supplies a cap).
#[cfg(test)]
pub fn decode(
    src: &[u8],
    literals: &[u8],
    out: &mut Vec<u8>,
    tables: &mut SeqTables,
    rep: &mut [u32; 3],
) -> Result<()> {
    decode_capped(src, literals, out, tables, rep, 0, usize::MAX)
}

/// Decode the sequences section, enforcing an output ceiling. `dict_len` is the
/// dictionary prefix already in `out` (excluded from the cap); `max_output` bounds
/// the real regenerated output. The ceiling is checked before every growth so a
/// hostile compressed block cannot expand past it (a spec block regenerates
/// ≤ 128 KiB, but a corrupt one can claim an arbitrary `match_len`).
pub fn decode_capped(
    src: &[u8],
    literals: &[u8],
    out: &mut Vec<u8>,
    tables: &mut SeqTables,
    rep: &mut [u32; 3],
    dict_len: usize,
    max_output: usize,
) -> Result<()> {
    // Bytes still allowed before the output ceiling, given what `out` already holds.
    let headroom = |out: &Vec<u8>| max_output.saturating_sub(out.len() - dict_len);
    if src.is_empty() {
        return Err(ZstdError::Truncated {
            what: "sequences header",
            needed: 1,
        });
    }
    let b0 = src[0] as usize;
    let (nb_seq, mut p) = if b0 < 128 {
        (b0, 1)
    } else if b0 < 255 {
        if src.len() < 2 {
            return Err(ZstdError::Truncated {
                what: "sequence count",
                needed: 2 - src.len(),
            });
        }
        (((b0 - 128) << 8) + src[1] as usize, 2)
    } else {
        if src.len() < 3 {
            return Err(ZstdError::Truncated {
                what: "sequence count",
                needed: 3 - src.len(),
            });
        }
        (src[1] as usize + ((src[2] as usize) << 8) + 0x7F00, 3)
    };

    if nb_seq == 0 {
        if literals.len() > headroom(out) {
            return Err(ZstdError::OutputTooLarge { limit: max_output });
        }
        out.extend_from_slice(literals);
        return Ok(());
    }

    if p >= src.len() {
        return Err(ZstdError::Truncated {
            what: "sequence compression modes",
            needed: 1,
        });
    }
    let modes = src[p];
    p += 1;
    let ll_mode = (modes >> 6) & 3;
    let of_mode = (modes >> 4) & 3;
    let ml_mode = (modes >> 2) & 3;

    // Table descriptions appear in order: Literals_Length, Offset, Match_Length.
    let ll = parse_mode(ll_mode, src, &mut p, 9)?;
    let of = parse_mode(of_mode, src, &mut p, 8)?;
    let ml = parse_mode(ml_mode, src, &mut p, 9)?;

    let ll_table = resolve_table(ll, &LL_DEFAULT, 35, 6, &mut tables.ll, "LL")?;
    let of_table = resolve_table(of, &OF_DEFAULT, 28, 5, &mut tables.of, "OF")?;
    let ml_table = resolve_table(ml, &ML_DEFAULT, 52, 6, &mut tables.ml, "ML")?;

    let bitstream = &src[p..];
    let mut br = ReverseBitReader::new(bitstream)?;
    let mut s_ll = FseDecoder::init(ll_table, &mut br);
    let mut s_of = FseDecoder::init(of_table, &mut br);
    let mut s_ml = FseDecoder::init(ml_table, &mut br);

    let mut lit_pos = 0usize;
    for i in 0..nb_seq {
        let e_ll = s_ll.entry(ll_table);
        let e_ml = s_ml.entry(ml_table);
        let e_of = s_of.entry(of_table);
        let ll_code = e_ll.symbol as usize;
        let ml_code = e_ml.symbol as usize;
        let of_code = e_of.symbol as u32;
        // `of_code` is the log2 baseline of the offset; a 32-bit offset caps it
        // at 31. A corrupt/mutated entropy table can yield a larger symbol —
        // reject it rather than overflow the `1 << of_code` shift.
        if ll_code >= LL_BASE.len() || ml_code >= ML_BASE.len() || of_code > 31 {
            return Err(ZstdError::Invalid {
                what: "sequence code",
                detail: format!("ll {ll_code} / ml {ml_code} / of {of_code} out of range"),
            });
        }

        // Read extra bits: offset, then match length, then literals length.
        // `read_lazy` refills only when the next field would not fit the 64-bit
        // window, instead of reloading before every field — the three fields
        // (≤ 31 + 16 + 16 bits) plus the state updates below usually span far
        // fewer than four fills. Identical bits, fewer refills.
        let offset_value = (1u32 << of_code) + br.read_lazy(of_code);
        let match_len = (ML_BASE[ml_code] + br.read_lazy(ML_BITS[ml_code])) as usize;
        let lit_len = (LL_BASE[ll_code] + br.read_lazy(LL_BITS[ll_code])) as usize;

        let actual_offset = resolve_offset(rep, offset_value, ll_code == 0) as usize;
        if actual_offset == 0 {
            return Err(ZstdError::Invalid {
                what: "sequence offset",
                detail: "zero offset".into(),
            });
        }

        // Enforce the output ceiling before growing by this sequence's bytes.
        if lit_len + match_len > headroom(out) {
            return Err(ZstdError::OutputTooLarge { limit: max_output });
        }

        // Copy `lit_len` literals.
        if lit_pos + lit_len > literals.len() {
            return Err(ZstdError::Invalid {
                what: "sequence literals length",
                detail: format!("want {lit_len} literals at {lit_pos} of {}", literals.len()),
            });
        }
        out.extend_from_slice(&literals[lit_pos..lit_pos + lit_len]);
        lit_pos += lit_len;

        // Copy `match_len` bytes from `actual_offset` back (overlap-safe).
        if actual_offset > out.len() {
            return Err(ZstdError::OffsetTooLarge {
                offset: actual_offset,
                history: out.len(),
            });
        }
        let start = out.len() - actual_offset;
        if actual_offset >= match_len {
            // Non-overlapping back-reference: the whole match is already present,
            // so copy it in one bulk move (a memcpy) instead of byte-by-byte.
            out.extend_from_within(start..start + match_len);
        } else if actual_offset == 1 {
            // Common RLE-like overlap: repeat the previous byte without growing
            // via many overlapping slice copies.
            let b = out[start];
            out.resize(out.len() + match_len, b);
        } else if matches!(actual_offset, 2 | 4 | 8) {
            copy_power_of_two_pattern(out, start, actual_offset, match_len);
        } else {
            // Overlapping back-reference (offset < length): replicate the
            // `actual_offset`-byte pattern. Copy it in geometrically growing
            // chunks — each `extend_from_within` copies everything written from
            // `start` so far (which doubles the available source each step), so the
            // whole match is O(match_len) memcpy work, not byte-by-byte. Every
            // copied byte references bytes already written, so overlap stays valid.
            out.reserve(match_len);
            let mut remaining = match_len;
            while remaining > 0 {
                let avail = out.len() - start;
                let n = remaining.min(avail);
                out.extend_from_within(start..start + n);
                remaining -= n;
            }
        }

        if i + 1 < nb_seq {
            // The table entries were already loaded for their symbols; reuse
            // them for the state transitions instead of indexing each table a
            // second time.
            br.ensure(e_ll.num_bits as u32 + e_ml.num_bits as u32 + e_of.num_bits as u32);
            s_ll.update_with_entry(e_ll, &mut br);
            s_ml.update_with_entry(e_ml, &mut br);
            s_of.update_with_entry(e_of, &mut br);
        }
    }

    // Trailing literals after the last sequence.
    if literals.len() - lit_pos > headroom(out) {
        return Err(ZstdError::OutputTooLarge { limit: max_output });
    }
    out.extend_from_slice(&literals[lit_pos..]);
    Ok(())
}

/// Materialize the actual table for a channel, updating the repeat cache.
fn resolve_table<'a>(
    mode: Mode,
    default_dist: &[i16],
    default_max_symbol: usize,
    default_log: u32,
    cache: &'a mut Option<FseDecodeTable>,
    name: &'static str,
) -> Result<&'a FseDecodeTable> {
    match mode {
        Mode::Repeat => match cache {
            Some(t) => return Ok(t),
            None => {
                return Err(ZstdError::Invalid {
                    what: "repeat sequence table",
                    detail: format!("no cached {name} table to repeat"),
                })
            }
        },
        Mode::Predefined => {
            *cache = Some(fse::build_dtable(
                default_dist,
                default_max_symbol,
                default_log,
            )?);
        }
        Mode::Rle(s) => {
            *cache = Some(rle_table(s));
        }
        Mode::Fse(t) => {
            *cache = Some(t);
        }
    };
    Ok(cache.as_ref().expect("sequence table cache was just set"))
}
