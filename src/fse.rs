//! FSE (Finite State Entropy) table description, table build, and decode.
//!
//! Implements RFC 8878 §4.1: reading the normalized-count table description
//! (`FSE_readNCount`), building the decoding table, and the two-state
//! `FSE_decompress` used for Huffman weights. Sequence decoding drives the
//! per-symbol [`FseDecoder`] states directly (see `sequences`).

use super::bits::{ForwardBitReader, ReloadStatus, ReverseBitReader};
use super::error::{Result, ZstdError};
#[allow(unused_imports)]
use crate::alloc_prelude::*;

/// Absolute maximum FSE accuracy log permitted by the spec.
pub const FSE_MAX_TABLELOG: u32 = 15;

/// Fixed capacity of a decode table's entry array. The largest table this codec
/// builds is the Literals_Length / Match_Length sequence table at accuracy log 9
/// (`1 << 9 == 512`); Offset caps at log 8, the Huffman-weight FSE at log 6.
///
/// Storing the entries in a *fixed-size* array (rather than a `Vec`) and indexing
/// with `state & FSE_DTABLE_MASK` lets the compiler prove the index is in range
/// (`x & 511` is statically `0..=511`, the array length is statically 512) and so
/// **elide the per-symbol bounds check** — entirely within safe Rust. The mask is
/// a no-op for correctness (the FSE invariant already keeps `state < 1 << log`),
/// it exists only to communicate that range to LLVM. A runtime `& (len - 1)` on a
/// `Vec` does *not* achieve this: the length is not a compile-time power of two,
/// so the check survives.
pub const FSE_DTABLE_SIZE: usize = 1 << 9;
const FSE_DTABLE_MASK: usize = FSE_DTABLE_SIZE - 1;

#[inline]
fn highbit32(x: u32) -> u32 {
    31 - x.leading_zeros()
}

/// One decode-table entry: emit `symbol`, then read `num_bits` and add to
/// `new_state_base` to get the next state.
#[derive(Debug, Clone, Copy, Default)]
pub struct FseEntry {
    pub symbol: u8,
    pub num_bits: u8,
    pub new_state_base: u16,
}

/// A built FSE decoding table. The first `1 << table_log` entries are live; the
/// array is over-sized to a fixed [`FSE_DTABLE_SIZE`] so the decode hot path can
/// index it with a constant mask and skip bounds checks (see [`FSE_DTABLE_SIZE`]).
#[derive(Debug, Clone)]
pub struct FseDecodeTable {
    pub table_log: u32,
    pub entries: Box<[FseEntry; FSE_DTABLE_SIZE]>,
}

/// The normalized-count table description read from an FSE header.
pub struct NCount {
    /// Normalized counts indexed by symbol (`-1` = "less than one").
    pub counts: Vec<i16>,
    /// Highest symbol value with a non-zero count.
    pub max_symbol: usize,
    /// Accuracy log (table log) for the distribution.
    pub table_log: u32,
    /// Bytes consumed from `src` by the table description.
    pub bytes_consumed: usize,
}

/// Read an FSE table description (`FSE_readNCount`) from the front of `src`.
pub fn read_ncount(src: &[u8], max_log: u32) -> Result<NCount> {
    let mut br = ForwardBitReader::new(src);
    let table_log = br.read(4) + 5;
    if table_log > max_log.min(FSE_MAX_TABLELOG) {
        return Err(ZstdError::CorruptTable(format!(
            "FSE accuracy log {table_log} exceeds max {max_log}"
        )));
    }
    let mut remaining: i32 = (1i32 << table_log) + 1;
    let mut threshold: i32 = 1i32 << table_log;
    let mut nb_bits: u32 = table_log + 1;
    let mut counts = vec![0i16; 256];
    let mut symbol: usize = 0;
    let mut previous0 = false;

    while remaining > 1 && symbol < 256 {
        if previous0 {
            let mut repeat = symbol;
            while br.peek(16) == 0xFFFF {
                repeat += 24;
                br.consume(16);
            }
            while br.peek(2) == 3 {
                repeat += 3;
                br.consume(2);
            }
            repeat += br.read(2) as usize;
            // counts in [symbol, repeat) are already zero.
            symbol = repeat.min(256);
            previous0 = false;
            continue;
        }
        let max = (2 * threshold - 1) - remaining;
        let mask = (threshold - 1) as u32;
        let low = br.peek(nb_bits);
        let count: i32 = if (low & mask) < (max as u32) {
            br.consume(nb_bits - 1);
            (low & mask) as i32
        } else {
            br.consume(nb_bits);
            let mut c = (low & ((2 * threshold - 1) as u32)) as i32;
            if c >= threshold {
                c -= max;
            }
            c
        };
        let count = count - 1; // "extra accuracy": -1 means probability < 1
        remaining -= count.abs();
        if symbol >= 256 {
            return Err(ZstdError::CorruptTable("FSE symbol overflow".into()));
        }
        counts[symbol] = count as i16;
        symbol += 1;
        previous0 = count == 0;
        while remaining < threshold {
            nb_bits -= 1;
            threshold >>= 1;
        }
    }
    if remaining != 1 {
        return Err(ZstdError::CorruptTable(format!(
            "FSE counts did not sum to table size (remaining {remaining})"
        )));
    }
    let max_symbol = symbol.saturating_sub(1);
    Ok(NCount {
        counts,
        max_symbol,
        table_log,
        bytes_consumed: br.bytes_consumed(),
    })
}

/// Build a decoding table from a normalized distribution (RFC 8878 §4.1.1).
pub fn build_dtable(norm: &[i16], max_symbol: usize, table_log: u32) -> Result<FseDecodeTable> {
    let size = 1usize << table_log;
    let mask = size - 1;
    let mut symbols = vec![0u8; size];
    let mut symbol_next = vec![0u16; max_symbol + 1];
    let mut high_threshold = size - 1;

    for (s, &count) in norm[..=max_symbol].iter().enumerate() {
        if count == -1 {
            symbols[high_threshold] = s as u8;
            high_threshold = high_threshold.wrapping_sub(1);
            symbol_next[s] = 1;
        } else {
            symbol_next[s] = count.max(0) as u16;
        }
    }

    let step = (size >> 1) + (size >> 3) + 3;
    let mut pos = 0usize;
    for (s, &count) in norm[..=max_symbol].iter().enumerate() {
        if count <= 0 {
            continue;
        }
        for _ in 0..count {
            symbols[pos] = s as u8;
            pos = (pos + step) & mask;
            while pos > high_threshold {
                pos = (pos + step) & mask;
            }
        }
    }
    if pos != 0 {
        return Err(ZstdError::CorruptTable(
            "FSE symbol spread did not cover the table".into(),
        ));
    }

    let mut entries = Box::new([FseEntry::default(); FSE_DTABLE_SIZE]);
    for (u, entry) in entries[..size].iter_mut().enumerate() {
        let s = symbols[u];
        let next = symbol_next[s as usize];
        symbol_next[s as usize] += 1;
        let num_bits = table_log - highbit32(next as u32);
        let new_state_base = ((next as u32) << num_bits) - size as u32;
        *entry = FseEntry {
            symbol: s,
            num_bits: num_bits as u8,
            new_state_base: new_state_base as u16,
        };
    }
    Ok(FseDecodeTable { table_log, entries })
}

/// Convenience: read a table description and build the decode table.
pub fn read_dtable(src: &[u8], max_log: u32) -> Result<(FseDecodeTable, usize)> {
    let nc = read_ncount(src, max_log)?;
    let dt = build_dtable(&nc.counts, nc.max_symbol, nc.table_log)?;
    Ok((dt, nc.bytes_consumed))
}

/// A single FSE decode state (used by sequence decoding).
#[derive(Debug, Clone, Copy)]
pub struct FseDecoder {
    pub state: u32,
}

impl FseDecoder {
    /// Initialize the state by reading `table_log` bits.
    #[inline]
    pub fn init(table: &FseDecodeTable, br: &mut ReverseBitReader) -> Self {
        FseDecoder {
            state: br.read(table.table_log),
        }
    }

    /// The symbol for the current state. The `& FSE_DTABLE_MASK` is a no-op for
    /// correctness (`state < 1 << table_log <= FSE_DTABLE_SIZE`) but lets the
    /// compiler elide the bounds check (see [`FSE_DTABLE_SIZE`]).
    #[inline]
    pub fn symbol(&self, table: &FseDecodeTable) -> u8 {
        table.entries[self.state as usize & FSE_DTABLE_MASK].symbol
    }

    /// Advance the state by reading `num_bits` low bits for the current entry.
    #[inline]
    pub fn update(&mut self, table: &FseDecodeTable, br: &mut ReverseBitReader) {
        let e = table.entries[self.state as usize & FSE_DTABLE_MASK];
        let low = br.read(e.num_bits as u32);
        self.state = e.new_state_base as u32 + low;
    }
}

/// Two-state `FSE_decompress` for a standalone FSE stream (Huffman weights).
///
/// Decodes until the reverse bitstream is exhausted, matching libzstd's tail
/// logic, and returns the produced symbols.
pub fn decompress(bitstream: &[u8], table: &FseDecodeTable, max_out: usize) -> Result<Vec<u8>> {
    let mut br = ReverseBitReader::new(bitstream)?;
    let mut s1 = FseDecoder::init(table, &mut br);
    let mut s2 = FseDecoder::init(table, &mut br);
    let mut out: Vec<u8> = Vec::new();

    let emit = |out: &mut Vec<u8>, sym: u8| -> Result<()> {
        if out.len() >= max_out {
            return Err(ZstdError::OutputTooLarge { limit: max_out });
        }
        out.push(sym);
        Ok(())
    };

    // Main loop: two symbols per iteration while the stream is unfinished and
    // there is room for a pair.
    while br.reload() == ReloadStatus::Unfinished && out.len() + 2 <= max_out {
        let sym1 = s1.symbol(table);
        s1.update(table, &mut br);
        out.push(sym1);
        let sym2 = s2.symbol(table);
        s2.update(table, &mut br);
        out.push(sym2);
    }

    // Tail: alternate states until a reload overflows.
    loop {
        let sym = s1.symbol(table);
        s1.update(table, &mut br);
        emit(&mut out, sym)?;
        if br.reload() == ReloadStatus::Overflow {
            let sym = s2.symbol(table);
            emit(&mut out, sym)?;
            break;
        }
        let sym = s2.symbol(table);
        s2.update(table, &mut br);
        emit(&mut out, sym)?;
        if br.reload() == ReloadStatus::Overflow {
            let sym = s1.symbol(table);
            emit(&mut out, sym)?;
            break;
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ncount_then_build_predefined_ll() {
        // RFC 8878 §3.1.1.3.2.2.1 default Literals_Length distribution.
        const LL_DEFAULT: [i16; 36] = [
            4, 3, 2, 2, 2, 2, 2, 2, 2, 2, 2, 2, 2, 1, 1, 1, 2, 2, 2, 2, 2, 2, 2, 2, 2, 3, 2, 1, 1,
            1, 1, 1, -1, -1, -1, -1,
        ];
        let dt = build_dtable(&LL_DEFAULT, 35, 6).expect("build default LL table");
        // The array is over-sized to FSE_DTABLE_SIZE; only `1 << table_log` are live.
        assert_eq!(dt.entries.len(), FSE_DTABLE_SIZE);
        assert_eq!(dt.table_log, 6);
    }
}
