//! Per-level compression parameters — a port of the relevant columns of
//! libzstd's default `ZSTD_defaultCParameters` table plus the small-input
//! window adjustment (`ZSTD_adjustCParams`).
//!
//! `level` selects a row; the row gives the window/hash/chain/search sizes, the
//! minimum match length, the `target_length`, and the parse [`Strategy`]. Only
//! the `fast` finder is wired today, so the stronger strategies currently map
//! onto it; `window_log` (back-reference reach + frame header) and `hash_log`
//! (match-table size) are the columns actually consumed so far.

/// Parse strategy in increasing ratio/cost order (libzstd's `ZSTD_strategy`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Strategy {
    Fast,
    Dfast,
    Greedy,
    Lazy,
    Lazy2,
    BtLazy2,
    Btopt,
    Btultra,
    Btultra2,
}

/// Compression parameters (the subset we model), mirroring the columns of
/// libzstd's cparams table.
#[derive(Debug, Clone, Copy)]
pub struct CParams {
    pub window_log: u32,
    pub hash_log: u32,
    pub chain_log: u32,
    pub search_log: u32,
    pub min_match: u32,
    pub target_length: u32,
    pub strategy: Strategy,
}

/// Smallest / largest window log the encoder emits. The max is 23 (8 MiB):
/// RFC 8878 §3.1.1.1.2 recommends a compressor not require a `Window_Size`
/// larger than 8 MB for interoperability (a conformant decompressor need only
/// support up to that), and the `fast`/chain finders don't exploit a larger
/// window anyway. This is well under libzstd's default `windowLogMax` (27), so
/// a stock `ZSTD_decompress` still decodes our frames.
const MIN_WINDOW_LOG: u32 = 10;
const MAX_WINDOW_LOG: u32 = 23;

/// Largest window log the encoder advertises under opt-in long-distance matching
/// ([`params_for_level_ldm`] / `compress_long`): libzstd's default `windowLogMax`
/// (128 MiB). LDM offsets can exceed the portable 8 MiB cap, so the frame must
/// advertise a window that admits them; 27 stays within what a stock
/// `ZSTD_decompress` and this crate's `StreamingDecoder` accept by default. This
/// is a deliberate, opt-in conformance bump — see the Conformance note in the
/// README.
pub const LDM_MAX_WINDOW_LOG: u32 = 27;

use Strategy::*;

/// libzstd default cparams (the "unknown source size" row set), levels 0..=22.
/// Columns: `window_log`, `hash_log`, `chain_log`, `search_log`, `min_match`,
/// `target_length`, `strategy`. Index 0 mirrors level 1 (used for level ≤ 0).
#[rustfmt::skip]
const LEVELS: [CParams; 23] = [
    CParams { window_log: 19, hash_log: 14, chain_log: 13, search_log: 1, min_match: 7, target_length: 0,  strategy: Fast },
    CParams { window_log: 19, hash_log: 14, chain_log: 13, search_log: 1, min_match: 7, target_length: 0,  strategy: Fast },      // 1
    CParams { window_log: 20, hash_log: 16, chain_log: 15, search_log: 1, min_match: 6, target_length: 0,  strategy: Fast },      // 2
    CParams { window_log: 21, hash_log: 17, chain_log: 16, search_log: 1, min_match: 5, target_length: 0,  strategy: Dfast },     // 3
    CParams { window_log: 21, hash_log: 18, chain_log: 18, search_log: 1, min_match: 5, target_length: 0,  strategy: Dfast },     // 4
    CParams { window_log: 21, hash_log: 19, chain_log: 18, search_log: 2, min_match: 5, target_length: 2,  strategy: Greedy },    // 5
    CParams { window_log: 21, hash_log: 19, chain_log: 19, search_log: 3, min_match: 5, target_length: 4,  strategy: Lazy },      // 6
    CParams { window_log: 21, hash_log: 20, chain_log: 19, search_log: 3, min_match: 5, target_length: 8,  strategy: Lazy2 },     // 7
    CParams { window_log: 21, hash_log: 20, chain_log: 20, search_log: 3, min_match: 5, target_length: 16, strategy: Lazy2 },     // 8
    CParams { window_log: 21, hash_log: 21, chain_log: 20, search_log: 4, min_match: 5, target_length: 16, strategy: Lazy2 },     // 9
    CParams { window_log: 22, hash_log: 22, chain_log: 21, search_log: 4, min_match: 5, target_length: 16, strategy: Lazy2 },     // 10
    CParams { window_log: 22, hash_log: 22, chain_log: 21, search_log: 5, min_match: 5, target_length: 16, strategy: Lazy2 },     // 11
    CParams { window_log: 22, hash_log: 23, chain_log: 22, search_log: 5, min_match: 5, target_length: 16, strategy: Lazy2 },     // 12
    CParams { window_log: 22, hash_log: 22, chain_log: 22, search_log: 4, min_match: 5, target_length: 32, strategy: BtLazy2 },   // 13
    CParams { window_log: 22, hash_log: 23, chain_log: 22, search_log: 5, min_match: 5, target_length: 32, strategy: BtLazy2 },   // 14
    CParams { window_log: 22, hash_log: 23, chain_log: 22, search_log: 6, min_match: 5, target_length: 32, strategy: BtLazy2 },   // 15
    CParams { window_log: 22, hash_log: 23, chain_log: 23, search_log: 6, min_match: 5, target_length: 48, strategy: Btopt },     // 16
    CParams { window_log: 23, hash_log: 23, chain_log: 23, search_log: 7, min_match: 5, target_length: 64, strategy: Btopt },     // 17
    CParams { window_log: 23, hash_log: 23, chain_log: 23, search_log: 7, min_match: 5, target_length: 64, strategy: Btultra },   // 18
    CParams { window_log: 23, hash_log: 24, chain_log: 24, search_log: 8, min_match: 5, target_length: 64, strategy: Btultra2 },  // 19
    CParams { window_log: 25, hash_log: 25, chain_log: 25, search_log: 9, min_match: 5, target_length: 64, strategy: Btultra2 },  // 20
    CParams { window_log: 26, hash_log: 26, chain_log: 26, search_log: 10, min_match: 5, target_length: 64, strategy: Btultra2 }, // 21
    CParams { window_log: 27, hash_log: 27, chain_log: 27, search_log: 10, min_match: 5, target_length: 64, strategy: Btultra2 }, // 22
];

/// `ceil(log2(n))` for `n >= 1` (0 for `n == 1`).
fn ceil_log2(n: usize) -> u32 {
    if n <= 1 {
        0
    } else {
        32 - ((n - 1) as u32).leading_zeros()
    }
}

/// Select the cparams for `level`, then apply libzstd's `ZSTD_adjustCParams`
/// shrink: clamp the window to what `src_size` actually needs (bounded to
/// `[MIN_WINDOW_LOG, MAX_WINDOW_LOG]`) and keep `hash_log`/`chain_log` within
/// `window_log + 1`, so small inputs neither over-advertise a window nor
/// over-allocate tables.
pub fn params_for_level(level: i32, src_size: usize) -> CParams {
    let idx = level.clamp(1, 22) as usize;
    let mut p = LEVELS[idx];

    let needed = ceil_log2(src_size.max(1)).max(MIN_WINDOW_LOG);
    p.window_log = p.window_log.min(needed).min(MAX_WINDOW_LOG);
    if p.hash_log > p.window_log + 1 {
        p.hash_log = p.window_log + 1;
    }
    if p.chain_log > p.window_log + 1 {
        p.chain_log = p.window_log + 1;
    }
    p
}

/// Like [`params_for_level`] but for a **dictionary-primed** compression. The
/// level parameters are sized for the dictionary and the input *together*, so a
/// small per-file input doesn't collapse the window below the dictionary or
/// shrink the match tables; the window is then bumped, if needed, so it spans
/// the whole dictionary. Every dictionary byte — and the dictionary's seeded
/// repeat offsets, which reach back up to its full length — must stay inside the
/// advertised window. Capped at the portable 8 MiB max (log 23), mirroring
/// libzstd folding `dictSize` into the window choice.
pub(crate) fn params_for_level_with_dict(level: i32, src_size: usize, dict_size: usize) -> CParams {
    let mut p = params_for_level(level, src_size.saturating_add(dict_size));
    let dict_reach = ceil_log2(dict_size).clamp(MIN_WINDOW_LOG, MAX_WINDOW_LOG);
    if p.window_log < dict_reach {
        p.window_log = dict_reach;
    }
    p
}

/// Like [`params_for_level`] but for opt-in long-distance matching: the window
/// is grown to cover the whole input (so far matches are reachable), up to
/// [`LDM_MAX_WINDOW_LOG`], instead of being capped at the portable 8 MiB. The
/// regular match-finder tables keep their level sizes — only the advertised
/// window (and thus the reach of LDM's large offsets) grows; on a small input
/// the window stays where [`params_for_level`] put it, so `compress_long`
/// behaves like `compress`.
pub fn params_for_level_ldm(level: i32, src_size: usize) -> CParams {
    let mut p = params_for_level(level, src_size);
    let needed = ceil_log2(src_size.max(1)).clamp(MIN_WINDOW_LOG, LDM_MAX_WINDOW_LOG);
    if p.window_log < needed {
        p.window_log = needed;
    }
    p
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn small_input_shrinks_window_and_tables() {
        // A tiny input must not advertise a 21-bit window or allocate a huge
        // table; the window collapses toward the data size (floored at 10).
        let p = params_for_level(3, 5_000);
        assert!(p.window_log <= 13, "window_log {} too large for 5 KB", p.window_log);
        assert!(p.window_log >= MIN_WINDOW_LOG);
        assert!(p.hash_log <= p.window_log + 1);
    }

    #[test]
    fn large_input_keeps_level_window_capped() {
        // A large input keeps the level's window, capped at the portable max.
        let p = params_for_level(22, 1 << 30);
        assert_eq!(p.window_log, MAX_WINDOW_LOG);
        assert_eq!(p.strategy, Strategy::Btultra2);
    }

    #[test]
    fn level_is_clamped() {
        assert_eq!(params_for_level(-5, 1 << 20).strategy, Strategy::Fast);
        assert_eq!(params_for_level(999, 1 << 20).strategy, Strategy::Btultra2);
    }
}
