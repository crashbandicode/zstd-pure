//! Sequence section decoding + LZ execution — RFC 8878 §3.1.1.3.2.
//!
//! Parses the sequence count and the LL/Offset/ML FSE table modes (Predefined /
//! RLE / FSE / Repeat), then walks the reverse bitstream decoding each
//! (literals_length, match_length, offset) triple and reconstructing output by
//! copying literals and back-references (with the three repeat offsets).

#[allow(unused_imports)]
use crate::alloc_prelude::*;
use super::bits::ReverseBitReader;
use super::error::{Result, ZstdError};
use super::fse::{self, FseDecodeTable, FseDecoder};

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
    FseDecodeTable {
        table_log: 0,
        entries: vec![fse::FseEntry {
            symbol,
            num_bits: 0,
            new_state_base: 0,
        }],
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

/// Decode the sequences section and reconstruct the block's contribution into
/// `out`, appending behind any existing history. `out` already contains all
/// prior output (window history) usable by back-references.
pub fn decode(
    src: &[u8],
    literals: &[u8],
    out: &mut Vec<u8>,
    tables: &mut SeqTables,
    rep: &mut [u32; 3],
) -> Result<()> {
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
    let mut s_ll = FseDecoder::init(&ll_table, &mut br);
    let mut s_of = FseDecoder::init(&of_table, &mut br);
    let mut s_ml = FseDecoder::init(&ml_table, &mut br);

    let mut lit_pos = 0usize;
    for i in 0..nb_seq {
        let ll_code = s_ll.symbol(&ll_table) as usize;
        let ml_code = s_ml.symbol(&ml_table) as usize;
        let of_code = s_of.symbol(&of_table) as u32;
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
        // The reverse bit window must be reloaded between reads so it never has
        // to serve more than ~32 bits from one 64-bit fill (libzstd does the
        // same at fixed points in `ZSTD_decodeSequence`).
        br.reload();
        let offset_value = (1u32 << of_code) + br.read(of_code);
        br.reload();
        let match_len = (ML_BASE[ml_code] + br.read(ML_BITS[ml_code])) as usize;
        br.reload();
        let lit_len = (LL_BASE[ll_code] + br.read(LL_BITS[ll_code])) as usize;

        let actual_offset = resolve_offset(rep, offset_value, ll_code == 0) as usize;
        if actual_offset == 0 {
            return Err(ZstdError::Invalid {
                what: "sequence offset",
                detail: "zero offset".into(),
            });
        }

        // Copy `lit_len` literals.
        if lit_pos + lit_len > literals.len() {
            return Err(ZstdError::Invalid {
                what: "sequence literals length",
                detail: format!(
                    "want {lit_len} literals at {lit_pos} of {}",
                    literals.len()
                ),
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
        for k in 0..match_len {
            let b = out[start + k];
            out.push(b);
        }

        if i + 1 < nb_seq {
            br.reload();
            s_ll.update(&ll_table, &mut br);
            s_ml.update(&ml_table, &mut br);
            s_of.update(&of_table, &mut br);
        }
    }

    // Trailing literals after the last sequence.
    out.extend_from_slice(&literals[lit_pos..]);
    Ok(())
}

/// Materialize the actual table for a channel, updating the repeat cache.
fn resolve_table(
    mode: Mode,
    default_dist: &[i16],
    default_max_symbol: usize,
    default_log: u32,
    cache: &mut Option<FseDecodeTable>,
    name: &'static str,
) -> Result<FseDecodeTable> {
    let table = match mode {
        Mode::Predefined => fse::build_dtable(default_dist, default_max_symbol, default_log)?,
        Mode::Rle(s) => rle_table(s),
        Mode::Fse(t) => t,
        Mode::Repeat => match cache {
            Some(t) => t.clone(),
            None => {
                return Err(ZstdError::Invalid {
                    what: "repeat sequence table",
                    detail: format!("no cached {name} table to repeat"),
                })
            }
        },
    };
    *cache = Some(table.clone());
    Ok(table)
}
