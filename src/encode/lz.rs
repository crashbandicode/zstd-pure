//! LZ match finding — the parse that turns a byte block into a literals buffer
//! plus a list of [`Seq`]uences.
//!
//! This is the `fast` strategy (zstd level ~1): a single 4-byte hash table
//! holding the most recent position per hash, greedy matching with forward
//! extension (overlap-safe). A found offset that equals one of the three
//! running repeat offsets is coded as a **repeat-offset code** (`offset_value`
//! 1–3) instead of the literal form `offset + 3`, which is cheaper and is what
//! makes structured/periodic data compress well. The stronger strategies
//! (dfast/lazy/btopt) are later ratio refinements.
//!
//! The match table is **persistent across a frame's blocks** ([`MatchState`]):
//! [`parse_block`] parses one block's position range against the whole input,
//! so a match can reference any earlier block up to the window (`max_offset`),
//! and offsets are absolute back-distances — which the decoder reconstructs
//! correctly because its copy offset is relative to the current output end.

use super::super::sequences::{
    resolve_offset, LL_BITS, LL_DEFAULT, ML_BITS, ML_DEFAULT, OF_DEFAULT,
};
use super::sequences::{ll_code, ml_code, of_code, Seq};
#[allow(unused_imports)]
use crate::alloc_prelude::*;

/// Minimum match length (in bytes) the fast parser will emit.
const MIN_MATCH: usize = 4;
/// Hash log used by the single-block `fast_parse` convenience wrapper (tests only).
#[cfg(test)]
const DEFAULT_HASH_LOG: u32 = 17;
/// Clamp for a [`MatchState`] hash log: floor avoids a degenerate table, ceiling
/// caps the allocation at `1 << 22` entries (16 MiB) for very high levels.
const MIN_HASH_LOG: u32 = 6;
const MAX_HASH_LOG: u32 = 22;
/// Ceiling for the binary tree's log (its position index covers `1 << log`); the
/// tree must span the window, whose log the param table caps at 23 (8 MiB), so
/// the tree allocation (`2 << MAX_BT_LOG` i32s) tops out at 64 MiB.
const MAX_BT_LOG: u32 = 23;

#[inline]
fn read_u32(data: &[u8], p: usize) -> u32 {
    u32::from_le_bytes([data[p], data[p + 1], data[p + 2], data[p + 3]])
}

#[inline]
fn hash4(v: u32, hash_log: u32) -> usize {
    (v.wrapping_mul(2654435761) >> (32 - hash_log)) as usize
}

#[inline]
fn read_u64(data: &[u8], p: usize) -> u64 {
    u64::from_le_bytes([
        data[p],
        data[p + 1],
        data[p + 2],
        data[p + 3],
        data[p + 4],
        data[p + 5],
        data[p + 6],
        data[p + 7],
    ])
}

/// Hash of an 8-byte span — the `dfast` long-match table key. A longer span than
/// [`hash4`] collides far less, so it preserves long-match candidates that the
/// 4-byte table would overwrite with frequent short n-grams.
#[inline]
fn hash8(v: u64, hash_log: u32) -> usize {
    (v.wrapping_mul(0x9E37_79B1_85EB_CA87) >> (64 - hash_log)) as usize
}

/// Pick the cheapest `offset_value` that encodes back-distance `offset` at the
/// current point. Prefers a repeat code (`1..=3`) when one reproduces `offset`
/// exactly given the running repeat offsets `rep` and `ll0` (whether the
/// preceding literal run is empty); otherwise the literal form `offset + 3`.
///
/// The repeat candidates are computed with the decoder's own
/// [`resolve_offset`] on a throwaway copy, so a code is only chosen when the
/// decoder would resolve it to exactly this offset — encode and decode can't
/// drift. The smallest code wins (offset code 0/1 is cheaper than a large
/// literal offset), and `resolve_offset` is deterministic, so re-applying the
/// chosen code to the real `rep` evolves it identically to the decoder.
#[inline]
fn encode_offset(rep: &[u32; 3], offset: u32, ll0: bool) -> u32 {
    for ov in 1..=3u32 {
        let mut probe = *rep;
        if resolve_offset(&mut probe, ov, ll0) == offset {
            return ov;
        }
    }
    offset + 3
}

/// Persistent match-finder state across a frame's blocks: one slot per 4-byte
/// hash holding the most recent **absolute** position, so a match in any block
/// can reference earlier blocks (subject to the window). Carrying it across
/// blocks is what lets back-references span the 128 KiB block boundary.
pub struct MatchState {
    table: Vec<i32>, // absolute position per hash, -1 = empty
    hash_log: u32,
}

impl MatchState {
    /// Allocate a fresh (empty) match table for the given hash log (clamped to
    /// `[MIN_HASH_LOG, MAX_HASH_LOG]`).
    pub fn new(hash_log: u32) -> Self {
        let hash_log = hash_log.clamp(MIN_HASH_LOG, MAX_HASH_LOG);
        MatchState {
            table: vec![-1i32; 1usize << hash_log],
            hash_log,
        }
    }
}

/// Parse the block `data[start..end]` into `(sequences, literals)`, using and
/// updating the persistent `state` so matches can reference earlier blocks.
/// `literals` is the concatenation of this block's literal runs (including the
/// trailing run after the last match); reconstructing the block copies
/// `lit_len` literals then the match per sequence, exactly as
/// [`crate::sequences::decode`] does.
///
/// Match candidates may lie anywhere before `p` within `max_offset` (the
/// advertised window); the matched bytes never cross `end`, so the block's
/// regenerated size stays bounded. `rep` carries the three running repeat
/// offsets (the decoder's per-frame state, `[1, 4, 8]` at frame start); it is
/// read to detect repeat-offset codes and updated in lockstep with the decoder.
/// The caller must commit the updated `rep` only if this block's compressed
/// form is actually used (a raw/RLE block leaves the decoder's `rep` untouched).
pub fn parse_block(
    data: &[u8],
    range: core::ops::Range<usize>,
    state: &mut MatchState,
    max_offset: usize,
    rep: &mut [u32; 3],
) -> (Vec<Seq>, Vec<u8>) {
    let (start, end) = (range.start, range.end);
    let mut seqs = Vec::new();
    let mut literals = Vec::new();
    let hash_log = state.hash_log;

    if end < start + MIN_MATCH + 1 {
        // Too short to start a match; emit verbatim (these bytes are still
        // visible to later blocks through `data`, just not indexed here).
        literals.extend_from_slice(&data[start..end]);
        return (seqs, literals);
    }

    let mut anchor = start; // start of the pending literal run
    let mut p = start;
    let limit = end - MIN_MATCH; // last in-block position with 4 readable bytes

    while p <= limit {
        let v = read_u32(data, p);
        let h = hash4(v, hash_log);
        let cand = state.table[h];
        state.table[h] = p as i32;

        if cand >= 0 {
            let c = cand as usize;
            let offset = p - c;
            if offset <= max_offset
                && offset >= 1
                && data[c..c + MIN_MATCH] == data[p..p + MIN_MATCH]
            {
                // Extend the match forward, bounded by this block's `end` (the
                // matched bytes belong to this block's output). Overlap-safe:
                // comparing against the original data validates the decoder's
                // repeating copy.
                let mut ml = MIN_MATCH;
                while p + ml < end && data[c + ml] == data[p + ml] {
                    ml += 1;
                }

                let lit_len = p - anchor;
                literals.extend_from_slice(&data[anchor..p]);
                let ll0 = lit_len == 0;
                let offset_value = encode_offset(rep, offset as u32, ll0);
                // Evolve `rep` exactly as the decoder will for this code.
                resolve_offset(rep, offset_value, ll0);
                seqs.push(Seq {
                    lit_len: lit_len as u32,
                    match_len: ml as u32,
                    offset_value,
                });

                // Insert interior positions so later matches can reference
                // inside this one (cheap ratio win for `fast`).
                let mut q = p + 1;
                let stop = (p + ml).min(limit + 1);
                while q < stop {
                    state.table[hash4(read_u32(data, q), hash_log)] = q as i32;
                    q += 1;
                }

                p += ml;
                anchor = p;
                continue;
            }
        }
        p += 1;
    }

    literals.extend_from_slice(&data[anchor..end]);
    (seqs, literals)
}

/// Single-block convenience parse from a fresh match state (offsets relative to
/// `data`'s start). Used in tests; the frame encoder uses `parse_block` with a
/// persistent `MatchState` so matches span block boundaries.
#[cfg(test)]
pub fn fast_parse(data: &[u8], max_offset: usize, rep: &mut [u32; 3]) -> (Vec<Seq>, Vec<u8>) {
    let mut state = MatchState::new(DEFAULT_HASH_LOG);
    parse_block(data, 0..data.len(), &mut state, max_offset, rep)
}

/// `dfast` (double-fast) finder state: two persistent single-slot tables — a
/// `long` one keyed by an 8-byte hash (preserves long-match candidates) and a
/// `short` one keyed by a 4-byte hash (catches recent/short matches). Carried
/// across a frame's blocks like [`MatchState`].
pub struct DFastState {
    long: Vec<i32>,
    short: Vec<i32>,
    long_log: u32,
    short_log: u32,
}

impl DFastState {
    pub fn new(long_log: u32, short_log: u32) -> Self {
        let long_log = long_log.clamp(MIN_HASH_LOG, MAX_HASH_LOG);
        let short_log = short_log.clamp(MIN_HASH_LOG, MAX_HASH_LOG);
        DFastState {
            long: vec![-1i32; 1usize << long_log],
            short: vec![-1i32; 1usize << short_log],
            long_log,
            short_log,
        }
    }

    /// Index `pos` (needs ≥ 4 readable bytes) in the short table, and the long
    /// table too when ≥ 8 bytes are readable.
    #[inline]
    fn insert(&mut self, data: &[u8], pos: usize, end: usize) {
        self.short[hash4(read_u32(data, pos), self.short_log)] = pos as i32;
        if pos + 8 <= end {
            self.long[hash8(read_u64(data, pos), self.long_log)] = pos as i32;
        }
    }
}

/// Greedy `dfast` parse: at each position take the longer of the matches the
/// long (8-byte) and short (4-byte) hash tables point at. Otherwise identical in
/// contract to [`parse_block`] (persistent `state`, cross-block offsets, `rep`
/// updated in decoder lockstep).
pub fn dfast_parse_block(
    data: &[u8],
    range: core::ops::Range<usize>,
    state: &mut DFastState,
    max_offset: usize,
    rep: &mut [u32; 3],
) -> (Vec<Seq>, Vec<u8>) {
    let (start, end) = (range.start, range.end);
    let mut seqs = Vec::new();
    let mut literals = Vec::new();
    if end < start + MIN_MATCH + 1 {
        literals.extend_from_slice(&data[start..end]);
        return (seqs, literals);
    }
    let mut anchor = start;
    let mut p = start;
    let limit = end - MIN_MATCH;

    while p <= limit {
        // Read both candidates, then index `p` in both tables.
        let cand_long = if p + 8 <= end {
            let h = hash8(read_u64(data, p), state.long_log);
            let c = state.long[h];
            state.long[h] = p as i32;
            c
        } else {
            -1
        };
        let hs = hash4(read_u32(data, p), state.short_log);
        let cand_short = state.short[hs];
        state.short[hs] = p as i32;

        // Best of the two candidates.
        let mut best_ml = MIN_MATCH - 1;
        let mut best_c = 0usize;
        for &cand in &[cand_long, cand_short] {
            if cand < 0 {
                continue;
            }
            let c = cand as usize;
            let offset = p - c;
            if offset <= max_offset && data[c..c + MIN_MATCH] == data[p..p + MIN_MATCH] {
                let mut ml = MIN_MATCH;
                while p + ml < end && data[c + ml] == data[p + ml] {
                    ml += 1;
                }
                if ml > best_ml {
                    best_ml = ml;
                    best_c = c;
                }
            }
        }

        if best_ml >= MIN_MATCH {
            let lit_len = p - anchor;
            literals.extend_from_slice(&data[anchor..p]);
            let ll0 = lit_len == 0;
            let offset_value = encode_offset(rep, (p - best_c) as u32, ll0);
            resolve_offset(rep, offset_value, ll0);
            seqs.push(Seq {
                lit_len: lit_len as u32,
                match_len: best_ml as u32,
                offset_value,
            });
            let mut q = p + 1;
            let stop = (p + best_ml).min(limit + 1);
            while q < stop {
                state.insert(data, q, end);
                q += 1;
            }
            p += best_ml;
            anchor = p;
        } else {
            p += 1;
        }
    }

    literals.extend_from_slice(&data[anchor..end]);
    (seqs, literals)
}

/// Hash-chain match-finder state for the greedy/lazy strategies. `head[h]` is
/// the most recent position for hash `h`; `chain[p & chain_mask]` links to the
/// previous position that hashed the same, so a search walks several candidates
/// (depth-bounded) keeping the longest match. Persistent across a frame's
/// blocks, like [`MatchState`].
pub struct ChainState {
    head: Vec<i32>,
    chain: Vec<i32>,
    hash_log: u32,
    chain_mask: usize,
}

impl ChainState {
    /// Allocate empty head + chain tables (logs clamped to a sane range).
    pub fn new(hash_log: u32, chain_log: u32) -> Self {
        let hash_log = hash_log.clamp(MIN_HASH_LOG, MAX_HASH_LOG);
        let chain_log = chain_log.clamp(MIN_HASH_LOG, MAX_HASH_LOG);
        ChainState {
            head: vec![-1i32; 1usize << hash_log],
            chain: vec![-1i32; 1usize << chain_log],
            hash_log,
            chain_mask: (1usize << chain_log) - 1,
        }
    }

    /// Index `pos` (which must have 4 readable bytes): push it onto its hash
    /// chain. Each position is inserted exactly once by the parser.
    #[inline]
    fn insert(&mut self, data: &[u8], pos: usize) {
        let h = hash4(read_u32(data, pos), self.hash_log);
        self.chain[pos & self.chain_mask] = self.head[h];
        self.head[h] = pos as i32;
    }

    /// Longest match for `ip` among up to `depth` chained candidates within the
    /// window — `(match_len, match_pos)`, or `(0, 0)` if none reaches
    /// `MIN_MATCH`. Read-only; the caller inserts positions via [`Self::insert`].
    fn find(
        &self,
        data: &[u8],
        ip: usize,
        end: usize,
        max_offset: usize,
        depth: usize,
    ) -> (usize, usize) {
        let h = hash4(read_u32(data, ip), self.hash_log);
        let mut cand = self.head[h];
        let mut best_ml = MIN_MATCH - 1; // a candidate must beat this to count
        let mut best_pos = 0usize;
        let mut steps = 0usize;
        while cand >= 0 && steps < depth {
            let c = cand as usize;
            if ip - c > max_offset {
                break; // chain is ordered newest-first; older ones are further still
            }
            // Only bother extending if this candidate can beat the current best
            // (the byte just past `best_ml` must match) — the standard speedup.
            if ip + best_ml < end && data[c + best_ml] == data[ip + best_ml] {
                let mut ml = 0usize;
                while ip + ml < end && data[c + ml] == data[ip + ml] {
                    ml += 1;
                }
                if ml > best_ml {
                    best_ml = ml;
                    best_pos = c;
                }
            }
            let next = self.chain[c & self.chain_mask];
            // Chains must strictly recede; a stale alias that doesn't is a dead
            // end (and guards against cycles).
            if next < 0 || next as usize >= c {
                break;
            }
            cand = next;
            steps += 1;
        }
        if best_ml >= MIN_MATCH {
            (best_ml, best_pos)
        } else {
            (0, 0)
        }
    }

    /// Collect the Pareto-optimal matches for `ip` into `out` as `(len, offset)`
    /// pairs of **strictly increasing length** — each the smallest offset that
    /// first reaches that length while walking up to `depth` chained candidates
    /// within the window. Match extension is capped at `cap` (and the walk stops
    /// once a candidate reaches `cap`): the optimal parser only needs to know a
    /// match is "long enough", then extends the chosen one fully — this keeps
    /// the per-position cost bounded on highly repetitive data.
    pub(crate) fn find_matches(
        &self,
        data: &[u8],
        range: core::ops::Range<usize>,
        max_offset: usize,
        depth: usize,
        cap: usize,
        out: &mut Vec<(u32, u32)>,
    ) {
        let (ip, end) = (range.start, range.end);
        out.clear();
        let h = hash4(read_u32(data, ip), self.hash_log);
        let mut cand = self.head[h];
        let mut best_ml = MIN_MATCH - 1;
        let mut steps = 0usize;
        while cand >= 0 && steps < depth {
            let c = cand as usize;
            let offset = ip - c;
            if offset > max_offset {
                break;
            }
            if ip + best_ml < end && data[c + best_ml] == data[ip + best_ml] {
                let mut ml = 0usize;
                while ip + ml < end && ml < cap && data[c + ml] == data[ip + ml] {
                    ml += 1;
                }
                if ml > best_ml {
                    best_ml = ml;
                    out.push((ml as u32, offset as u32));
                    if best_ml >= cap {
                        break; // "long enough"; the parser extends this one fully
                    }
                }
            }
            let next = self.chain[c & self.chain_mask];
            if next < 0 || next as usize >= c {
                break;
            }
            cand = next;
            steps += 1;
        }
    }

    /// Full (uncapped) match length at `c = ip - offset` against `ip`, for
    /// extending a "long enough" match the [`find_matches`] cap truncated.
    #[inline]
    fn extend_full(&self, data: &[u8], ip: usize, c: usize, end: usize) -> usize {
        let mut ml = 0usize;
        while ip + ml < end && data[c + ml] == data[ip + ml] {
            ml += 1;
        }
        ml
    }
}

/// Cap on `insert_bt1`'s match extension. The insert-only path runs for every
/// position inside a committed long match; on periodic data a faithful
/// (uncapped) extension would re-scan the whole run per position — O(n²). Capping
/// it bounds that work. It only affects *tree placement* of positions whose
/// match exceeds the cap (long-match / periodic regions); the **search** path
/// ([`BtState::insert_and_get_matches`]) is uncapped, so the longest matches the
/// hybrid actually relies on stay exact. Generous enough to place any realistic
/// medium match precisely.
const BT_INSERT_CAP: usize = 1024;

/// Faithful binary-tree match finder (a port of zstd's `ZSTD_insertBt*`): a hash
/// head table plus a binary tree over the window, keyed by suffix order. Unlike
/// the hash chain's depth-bounded *recency* search, a descent finds the longest
/// match regardless of how long ago it occurred — the [hybrid](opt_parse_block)
/// uses it for exactly that long-range reach, layered on the chain's
/// small-offset matches. Persistent across a frame's blocks, like [`ChainState`].
///
/// `bt` holds two child slots per windowed position — `bt[2*(p & bt_mask)]` is
/// `p`'s *smaller*-suffix child, `+1` its *larger*. The tree is sized to span the
/// window (`bt_mask + 1 >= window`), and traversal stops at `window_low`, so two
/// simultaneously-live positions never alias the same slots.
pub struct BtState {
    hash: Vec<i32>, // most-recent position per hash (head), -1 = empty
    bt: Vec<i32>,   // 2 slots per (pos & bt_mask): [smaller-child, larger-child]
    hash_log: u32,
    bt_mask: usize,
}

impl BtState {
    /// Allocate empty hash + tree tables. The tree is sized to `window_log` (the
    /// frame's already-`src_size`-adjusted window) so live positions don't alias.
    pub fn new(hash_log: u32, window_log: u32) -> Self {
        let hash_log = hash_log.clamp(MIN_HASH_LOG, MAX_HASH_LOG);
        let bt_log = window_log.clamp(MIN_HASH_LOG, MAX_BT_LOG);
        BtState {
            hash: vec![-1i32; 1usize << hash_log],
            bt: vec![-1i32; 2usize << bt_log],
            hash_log,
            bt_mask: (1usize << bt_log) - 1,
        }
    }

    /// Lowest position the tree can correctly reference from `pos`. The tree only
    /// has `1 << bt_log` node slots (indexed by `pos & bt_mask`), so positions
    /// farther back than that alias the live ones and corrupt the links; the
    /// search window is therefore clamped to the tree's coverage even if the
    /// caller's `max_offset` is larger (LDM supplies the farther matches, so the
    /// tree never needs to reach past its own array).
    #[inline]
    fn window_low(&self, pos: usize, max_offset: usize) -> usize {
        pos.saturating_sub(max_offset.min(self.bt_mask + 1))
    }

    /// Insert `curr` into the tree **and** return its longest match `(len,
    /// offset)` (`len < MIN_MATCH` ⇒ none) — zstd's `ZSTD_insertBtAndGetAllMatches`
    /// reduced to the longest (the hybrid only needs the long-range reach; the
    /// chain supplies the short/medium Pareto set). Descends from the hash head,
    /// extending each candidate from the known common prefix (`min(common_smaller,
    /// common_larger)`, which only grows down a branch — the trick that keeps the
    /// amortized cost near O(n log n) without a cap), tracking the longest, and
    /// re-linking the tree so `curr` is inserted in suffix order. Extension is
    /// **uncapped**.
    fn insert_and_get_longest(
        &mut self,
        data: &[u8],
        curr: usize,
        end: usize,
        max_offset: usize,
        nb_compares: usize,
    ) -> (usize, usize) {
        let window_low = self.window_low(curr, max_offset);
        let h = hash4(read_u32(data, curr), self.hash_log);
        let mut match_index = self.hash[h];
        self.hash[h] = curr as i32;

        let curr_slot = 2 * (curr & self.bt_mask);
        let mut smaller_ptr = curr_slot;
        let mut larger_ptr = curr_slot + 1;
        let mut common_smaller = 0usize;
        let mut common_larger = 0usize;
        let mut best_len = 0usize;
        let mut best_off = 0usize;
        let mut nb = nb_compares;

        while nb > 0 && match_index >= 0 && (match_index as usize) > window_low {
            nb -= 1;
            let mi = match_index as usize;
            let mut ml = common_smaller.min(common_larger);
            while curr + ml < end && data[mi + ml] == data[curr + ml] {
                ml += 1;
            }
            if ml > best_len {
                best_len = ml;
                best_off = curr - mi;
            }
            if curr + ml == end {
                // Equal so far → can't pick a side; leave `curr`'s subtree empty.
                break;
            }
            let match_slot = 2 * (mi & self.bt_mask);
            if data[mi + ml] < data[curr + ml] {
                self.bt[smaller_ptr] = match_index;
                common_smaller = ml;
                smaller_ptr = match_slot + 1;
                match_index = self.bt[match_slot + 1];
            } else {
                self.bt[larger_ptr] = match_index;
                common_larger = ml;
                larger_ptr = match_slot;
                match_index = self.bt[match_slot];
            }
        }
        self.bt[smaller_ptr] = -1;
        self.bt[larger_ptr] = -1;
        (best_len, best_off)
    }

    /// **Read-only** longest match for `ip` — `(match_len, match_pos)`, or
    /// `(0, 0)` if none — descending the tree built by [`Self::insert_bt1`]
    /// without re-linking it. The lazy parser ([`bt_lazy_parse_block`]) needs to
    /// probe `ip` and `ip+1` (its look-ahead) without inserting either, then
    /// inserts positions in order separately; this is the query half of the
    /// descent (same `commonLength`-bounded walk as the inserting variants).
    fn find_longest(
        &self,
        data: &[u8],
        ip: usize,
        end: usize,
        max_offset: usize,
        nb_compares: usize,
    ) -> (usize, usize) {
        let window_low = self.window_low(ip, max_offset);
        let h = hash4(read_u32(data, ip), self.hash_log);
        let mut match_index = self.hash[h];
        let mut common_smaller = 0usize;
        let mut common_larger = 0usize;
        let mut best_len = 0usize;
        let mut best_pos = 0usize;
        let mut nb = nb_compares;
        while nb > 0 && match_index >= 0 && (match_index as usize) > window_low {
            nb -= 1;
            let mi = match_index as usize;
            let mut ml = common_smaller.min(common_larger);
            while ip + ml < end && data[mi + ml] == data[ip + ml] {
                ml += 1;
            }
            if ml > best_len {
                best_len = ml;
                best_pos = mi;
            }
            if ip + ml == end {
                break;
            }
            let match_slot = 2 * (mi & self.bt_mask);
            if data[mi + ml] < data[ip + ml] {
                common_smaller = ml;
                match_index = self.bt[match_slot + 1];
            } else {
                common_larger = ml;
                match_index = self.bt[match_slot];
            }
        }
        (best_len, best_pos)
    }

    /// Insert `curr` into the tree without collecting matches — zstd's
    /// `ZSTD_insertBt1`, used to keep the index complete over positions the parser
    /// skips (inside a committed long match). Same descent as
    /// [`Self::insert_and_get_longest`] but **caps** the extension at
    /// [`BT_INSERT_CAP`] to bound the per-position cost on periodic data.
    fn insert_bt1(
        &mut self,
        data: &[u8],
        curr: usize,
        end: usize,
        max_offset: usize,
        nb_compares: usize,
    ) {
        let window_low = self.window_low(curr, max_offset);
        let ext_end = (curr + BT_INSERT_CAP).min(end);
        let h = hash4(read_u32(data, curr), self.hash_log);
        let mut match_index = self.hash[h];
        self.hash[h] = curr as i32;

        let curr_slot = 2 * (curr & self.bt_mask);
        let mut smaller_ptr = curr_slot;
        let mut larger_ptr = curr_slot + 1;
        let mut common_smaller = 0usize;
        let mut common_larger = 0usize;
        let mut nb = nb_compares;

        while nb > 0 && match_index >= 0 && (match_index as usize) > window_low {
            nb -= 1;
            let mi = match_index as usize;
            let mut ml = common_smaller.min(common_larger);
            while curr + ml < ext_end && data[mi + ml] == data[curr + ml] {
                ml += 1;
            }
            if curr + ml == end || curr + ml == ext_end {
                break;
            }
            let match_slot = 2 * (mi & self.bt_mask);
            if data[mi + ml] < data[curr + ml] {
                self.bt[smaller_ptr] = match_index;
                common_smaller = ml;
                smaller_ptr = match_slot + 1;
                match_index = self.bt[match_slot + 1];
            } else {
                self.bt[larger_ptr] = match_index;
                common_larger = ml;
                larger_ptr = match_slot;
                match_index = self.bt[match_slot];
            }
        }
        self.bt[smaller_ptr] = -1;
        self.bt[larger_ptr] = -1;
    }

    /// Prime the tree with the `dict_len` dictionary positions at the front of
    /// `data`, so input back-references can reach into the dictionary (insert-only,
    /// like the chain finder's dictionary priming).
    fn prime(&mut self, data: &[u8], dict_len: usize, max_offset: usize, nb_compares: usize) {
        let mut p = 0;
        while p + MIN_MATCH <= dict_len {
            self.insert_bt1(data, p, dict_len, max_offset, nb_compares);
            p += 1;
        }
    }
}

/// Parse `data[range]` with the hash-chain finder, `lazy_steps` of lazy
/// look-ahead (0 = greedy, 1 = lazy, 2 = lazy2) and a per-position chain `depth`.
/// Otherwise identical in contract to [`parse_block`] (persistent `state`,
/// cross-block offsets, `rep` updated in decoder lockstep). The lazy rule defers
/// to a strictly-longer match found one byte later, trading a literal for the
/// bigger match.
pub fn lazy_parse_block(
    data: &[u8],
    range: core::ops::Range<usize>,
    state: &mut ChainState,
    max_offset: usize,
    rep: &mut [u32; 3],
    lazy_steps: u32,
    depth: usize,
) -> (Vec<Seq>, Vec<u8>) {
    let (start, end) = (range.start, range.end);
    let mut seqs = Vec::new();
    let mut literals = Vec::new();
    if end < start + MIN_MATCH + 1 {
        literals.extend_from_slice(&data[start..end]);
        return (seqs, literals);
    }
    let ilimit = end - MIN_MATCH; // last position with 4 readable in-block bytes
    let mut anchor = start;
    let mut ip = start;
    let mut inserted = start; // positions [start, inserted) are on the chains

    while ip <= ilimit {
        // Index every position strictly before `ip` so `find(ip)` searches only
        // earlier positions (never `ip` itself).
        while inserted < ip {
            state.insert(data, inserted);
            inserted += 1;
        }
        let (mut ml, mut mpos) = state.find(data, ip, end, max_offset, depth);
        if ml < MIN_MATCH {
            ip += 1;
            continue;
        }
        // Lazy: if a strictly-longer match starts one byte later, defer to it
        // (emit one more literal). `lazy2` repeats the check once more.
        let mut steps = lazy_steps;
        while steps > 0 && ip < ilimit {
            while inserted <= ip {
                state.insert(data, inserted);
                inserted += 1;
            }
            let (ml1, mpos1) = state.find(data, ip + 1, end, max_offset, depth);
            if ml1 > ml {
                ml = ml1;
                mpos = mpos1;
                ip += 1;
                steps -= 1;
            } else {
                break;
            }
        }

        let lit_len = ip - anchor;
        literals.extend_from_slice(&data[anchor..ip]);
        let offset = ip - mpos;
        let ll0 = lit_len == 0;
        let offset_value = encode_offset(rep, offset as u32, ll0);
        resolve_offset(rep, offset_value, ll0);
        seqs.push(Seq {
            lit_len: lit_len as u32,
            match_len: ml as u32,
            offset_value,
        });

        // Index the match interior so later matches can reference inside it.
        let match_end = ip + ml;
        while inserted < match_end && inserted <= ilimit {
            state.insert(data, inserted);
            inserted += 1;
        }
        ip = match_end;
        anchor = ip;
    }

    literals.extend_from_slice(&data[anchor..end]);
    (seqs, literals)
}

/// The match [`bt_lazy_parse_block`] uses at `ip`: the hash chain's most-recent
/// (small-offset) match, but substituting the binary tree's longest match when
/// it's both a **long** match (`≥ target`) and **longer** than the chain's reach.
/// The chain's depth bound can miss the longest match on far-apart repeats; the
/// tree finds it regardless of recency. Restricting the substitution to long
/// matches that the chain didn't already reach keeps it net-positive (the length
/// gain covers the tree's possibly-larger offset), and on data whose matches sit
/// within the chain's depth the tree finds nothing longer, so it's a no-op there.
/// `(match_len, match_pos)`, `(0, 0)` if none.
#[allow(clippy::too_many_arguments)]
fn bt_lazy_best(
    chain: &ChainState,
    tree: &BtState,
    data: &[u8],
    ip: usize,
    end: usize,
    max_offset: usize,
    depth: usize,
    target: usize,
) -> (usize, usize) {
    let (cml, cpos) = chain.find(data, ip, end, max_offset, depth);
    let (tml, tpos) = tree.find_longest(data, ip, end, max_offset, depth);
    if tml >= target && tml > cml {
        (tml, tpos) // a longer long match than the chain's depth bound reached
    } else {
        (cml, cpos)
    }
}

/// `btlazy2` (L13–15): a lazy2 parse (greedy with two-step look-ahead) over a
/// **hybrid** finder — the hash chain for the recent small-offset matches that
/// dominate, plus the binary `tree` for the occasional long match the chain's
/// depth bound missed (see [`bt_lazy_best`]). Both structures index every
/// position. Otherwise identical in contract to [`lazy_parse_block`].
#[allow(clippy::too_many_arguments)]
pub fn bt_lazy_parse_block(
    data: &[u8],
    range: core::ops::Range<usize>,
    chain: &mut ChainState,
    tree: &mut BtState,
    max_offset: usize,
    rep: &mut [u32; 3],
    depth: usize,
    target: usize,
) -> (Vec<Seq>, Vec<u8>) {
    let (start, end) = (range.start, range.end);
    let mut seqs = Vec::new();
    let mut literals = Vec::new();
    if end < start + MIN_MATCH + 1 {
        literals.extend_from_slice(&data[start..end]);
        return (seqs, literals);
    }
    let ilimit = end - MIN_MATCH;
    let mut anchor = start;
    let mut ip = start;
    let mut inserted = start; // positions [start, inserted) are in both finders

    while ip <= ilimit {
        while inserted < ip {
            chain.insert(data, inserted);
            tree.insert_bt1(data, inserted, end, max_offset, depth);
            inserted += 1;
        }
        let (mut ml, mut mpos) =
            bt_lazy_best(chain, tree, data, ip, end, max_offset, depth, target);
        if ml < MIN_MATCH {
            ip += 1;
            continue;
        }
        // lazy2: defer to a strictly-longer match up to two bytes later.
        let mut steps = 2u32;
        while steps > 0 && ip < ilimit {
            while inserted <= ip {
                chain.insert(data, inserted);
                tree.insert_bt1(data, inserted, end, max_offset, depth);
                inserted += 1;
            }
            let (ml1, mpos1) =
                bt_lazy_best(chain, tree, data, ip + 1, end, max_offset, depth, target);
            if ml1 > ml {
                ml = ml1;
                mpos = mpos1;
                ip += 1;
                steps -= 1;
            } else {
                break;
            }
        }

        let lit_len = ip - anchor;
        literals.extend_from_slice(&data[anchor..ip]);
        let offset = ip - mpos;
        let ll0 = lit_len == 0;
        let offset_value = encode_offset(rep, offset as u32, ll0);
        resolve_offset(rep, offset_value, ll0);
        seqs.push(Seq {
            lit_len: lit_len as u32,
            match_len: ml as u32,
            offset_value,
        });

        let match_end = ip + ml;
        while inserted < match_end && inserted <= ilimit {
            chain.insert(data, inserted);
            tree.insert_bt1(data, inserted, end, max_offset, depth);
            inserted += 1;
        }
        ip = match_end;
        anchor = ip;
    }

    literals.extend_from_slice(&data[anchor..end]);
    (seqs, literals)
}

/// Fixed-point `log2(x) * 256` (8 fractional bits), `x >= 1`. `no_std`-friendly
/// (no float `log2`): integer `highbit` for the whole part, linear interpolation
/// for the fraction. Monotonic, which is all the price model needs.
#[inline]
fn log2_fp(x: u32) -> u32 {
    debug_assert!(x >= 1);
    let hb = 31 - x.leading_zeros();
    let frac = ((((x as u64) << 8) >> hb) as u32) & 0xFF;
    (hb << 8) | frac
}

/// One dynamic-programming cell: the cheapest way to encode the block prefix
/// ending here, how we reached it (for backtracking), and the repeat-offset
/// state along that path.
#[derive(Clone, Copy)]
struct Opt {
    price: u64,    // fixed-point bits (1/256) to encode [block_start, here)
    mlen: u32,     // arriving match length (0 = reached by extending literals)
    litlen: u32,   // arriving sequence's literal run (mlen>0) / pending run (mlen==0)
    offval: u32,   // arriving match's offset_value (when mlen>0)
    rep: [u32; 3], // repeat offsets at this position along the best path
}

/// Cap on the per-position sub-length search in [`run_dp`]; lengths past this
/// are only placed at their full value (finer placement is composed via shorter
/// matches). Bounds the DP's inner loop on highly repetitive data.
const MAX_SUBLEN: usize = 64;

/// The literal-byte and sequence-code price tables driving the optimal DP
/// ([`run_dp`]), in fixed-point bits (1/256). Two flavours: the static
/// [`Prices::predef`] prior (block literal histogram + predefined FSE tables),
/// and [`Prices::from_stats`], rebuilt from a first parse's actual statistics —
/// libzstd's `btultra2` second pass.
struct Prices {
    lit: [u64; 256],
    ll: [u64; 36],
    ml: [u64; 53],
    off: [u64; 32],
}

impl Prices {
    /// Static prior. Literal cost from the block-wide byte histogram (it
    /// overcounts matched bytes, but serves only as a relative literal-vs-match
    /// prior); LL/OF/ML code costs from the predefined-table distributions plus
    /// each code's extra bits (accuracy logs LL 6 / OF 5 / ML 6, matching the
    /// decoder's `resolve_table`). This is the model the single-pass
    /// `btopt`/`btultra` use, and the first pass of `btultra2`.
    fn predef(block: &[u8]) -> Self {
        let mut freq = [0u32; 256];
        for &b in block {
            freq[b as usize] += 1;
        }
        let l_total = log2_fp(block.len() as u32);
        let mut lit = [0u64; 256];
        for (b, p) in lit.iter_mut().enumerate() {
            let f = freq[b].max(1);
            *p = (l_total.saturating_sub(log2_fp(f)) as u64).max(16); // >= 1/16 bit
        }
        // Predefined code price: log2(table_size / norm_count) + extra bits.
        let code_price = |count: i16, log: u32| -> u64 {
            let c = if count <= 0 { 1u32 } else { count as u32 };
            ((log * 256).saturating_sub(log2_fp(c))) as u64
        };
        let mut ll = [0u64; 36];
        for (c, p) in ll.iter_mut().enumerate() {
            *p = code_price(LL_DEFAULT[c], 6) + (LL_BITS[c] as u64) * 256;
        }
        let mut ml = [0u64; 53];
        for (c, p) in ml.iter_mut().enumerate() {
            *p = code_price(ML_DEFAULT[c], 6) + (ML_BITS[c] as u64) * 256;
        }
        let mut off = [0u64; 32];
        for (c, p) in off.iter_mut().enumerate() {
            let count = if c < OF_DEFAULT.len() {
                OF_DEFAULT[c]
            } else {
                -1
            };
            *p = code_price(count, 5) + (c as u64) * 256;
        }
        Prices { lit, ll, ml, off }
    }

    /// `btultra2` dynamic model: prices from the *actual* statistics of a first
    /// parse — literal-byte frequencies over the emitted literal runs, and
    /// LL/OF/ML-code frequencies over `seqs` — so the DP optimizes against the
    /// per-block FSE tables `write_sequences` will really build rather than the
    /// predefined prior. Each frequency is `+1` smoothed, so an unobserved
    /// symbol keeps a finite (high) price (the second parse may still need it).
    fn from_stats(block: &[u8], seqs: &[Seq]) -> Self {
        // Literal-byte histogram from the literal runs only (not matched bytes).
        let mut lit_freq = [0u32; 256];
        let mut p = 0usize;
        for s in seqs {
            for &b in &block[p..p + s.lit_len as usize] {
                lit_freq[b as usize] += 1;
            }
            p += s.lit_len as usize + s.match_len as usize;
        }
        for &b in &block[p..] {
            lit_freq[b as usize] += 1; // trailing literal run
        }
        let lit_sum: u32 = lit_freq.iter().map(|&f| f + 1).sum();
        let log_lit = log2_fp(lit_sum);
        let mut lit = [0u64; 256];
        for (b, pr) in lit.iter_mut().enumerate() {
            *pr = (log_lit.saturating_sub(log2_fp(lit_freq[b] + 1)) as u64).max(16);
        }

        // LL/OF/ML code histograms over the emitted sequences.
        let mut ll_freq = [0u32; 36];
        let mut ml_freq = [0u32; 53];
        let mut of_freq = [0u32; 32];
        for s in seqs {
            ll_freq[ll_code(s.lit_len)] += 1;
            ml_freq[ml_code(s.match_len)] += 1;
            of_freq[of_code(s.offset_value).min(31)] += 1;
        }
        let mut ll = [0u64; 36];
        entropy_prices(&ll_freq, &mut ll, |c| LL_BITS[c]);
        let mut ml = [0u64; 53];
        entropy_prices(&ml_freq, &mut ml, |c| ML_BITS[c]);
        let mut off = [0u64; 32];
        entropy_prices(&of_freq, &mut off, |c| c as u32);

        Prices { lit, ll, ml, off }
    }
}

/// Fill `out[c]` with the entropy price of code `c` (in fixed-point bits) given
/// its `+1`-smoothed observed frequency plus `extra(c)` low bits:
/// `log2(Σ(freq+1)) - log2(freq[c]+1) + extra(c)`.
fn entropy_prices(freq: &[u32], out: &mut [u64], extra: impl Fn(usize) -> u32) {
    let sum: u32 = freq.iter().map(|&f| f + 1).sum();
    let log_sum = log2_fp(sum);
    for (c, p) in out.iter_mut().enumerate() {
        *p = (log_sum.saturating_sub(log2_fp(freq[c] + 1)) as u64) + (extra(c) as u64) * 256;
    }
}

/// Run the rep-aware DP over a fixed set of per-position candidate matches under
/// `prices`, returning the cheapest full-block sequence list (forward order) and
/// the repeat-offset state at the block end. Touches no match state — the
/// candidates were collected once by [`opt_parse_block`] — so it can be run
/// twice with different price models (`btultra2`'s second pass) for the cost of
/// only another DP, not another search.
///
/// `match_starts` indexes `matches_flat` (a CSR-style flattening of each
/// position's Pareto match list); `pos_long[i]` holds a committed long match
/// `(len, offset)` greedily taken at `i` (its interior was skipped during
/// collection). At each position the DP weighs a literal extension and every
/// recorded match length, keeping the globally cheapest path — so a shorter
/// match now can win when it enables a cheaper continuation, and rep matches are
/// preferred because their offset code is tiny.
#[allow(clippy::too_many_arguments)]
fn run_dp(
    data: &[u8],
    start: usize,
    n: usize,
    prices: &Prices,
    match_starts: &[u32],
    matches_flat: &[(u32, u32)],
    pos_long: &[Option<(u32, u32)>],
    init_rep: [u32; 3],
) -> (Vec<Seq>, [u32; 3]) {
    let big = u64::MAX / 4;
    let mut opt = vec![
        Opt {
            price: big,
            mlen: 0,
            litlen: 0,
            offval: 0,
            rep: [0; 3]
        };
        n + 1
    ];
    opt[0] = Opt {
        price: 0,
        mlen: 0,
        litlen: 0,
        offval: 0,
        rep: init_rep,
    };

    for i in 0..n {
        let base = opt[i].price;
        let pend = if opt[i].mlen > 0 {
            0
        } else {
            opt[i].litlen as usize
        };

        // Literal extension i -> i+1. Pre-pay the literal-length code so a
        // closing match adds no further ll cost; opening a run pays the ll code
        // for length 1, growing it pays the delta.
        let ll_add = if pend == 0 {
            prices.ll[ll_code(1)]
        } else {
            prices.ll[ll_code(pend as u32 + 1)].saturating_sub(prices.ll[ll_code(pend as u32)])
        };
        let cand = base + prices.lit[data[start + i] as usize] + ll_add;
        if cand < opt[i + 1].price {
            opt[i + 1] = Opt {
                price: cand,
                mlen: 0,
                litlen: pend as u32 + 1,
                offval: 0,
                rep: opt[i].rep,
            };
        }

        let ll0 = pend == 0;
        // ll code is pre-paid for pend>0; a zero-literal match still owes the ll
        // code for length 0.
        let ll_owed = if pend == 0 { prices.ll[ll_code(0)] } else { 0 };

        // A long match committed at this position during collection: place it
        // whole (its interior was skipped, so there are no short candidates).
        if let Some((long_len, best_off)) = pos_long[i] {
            let long_len = long_len as usize;
            let offval = encode_offset(&opt[i].rep, best_off, ll0);
            let ocost = prices.off[of_code(offval).min(31)];
            let mut new_rep = opt[i].rep;
            resolve_offset(&mut new_rep, offval, ll0);
            let j = i + long_len;
            let price = base + ll_owed + ocost + prices.ml[ml_code(long_len as u32)];
            if price < opt[j].price {
                opt[j] = Opt {
                    price,
                    mlen: long_len as u32,
                    litlen: pend as u32,
                    offval,
                    rep: new_rep,
                };
            }
            continue;
        }

        // Short-match region: weigh each candidate length — where the optimal
        // parse earns its keep. Each length is provided most cheaply by the
        // first Pareto entry reaching it, so cover only (prev_len, len_k].
        let mut prev_len = MIN_MATCH - 1;
        for &(len_k, off_k) in &matches_flat[match_starts[i] as usize..match_starts[i + 1] as usize]
        {
            let len_k = len_k as usize;
            let offval = encode_offset(&opt[i].rep, off_k, ll0);
            let ocost = prices.off[of_code(offval).min(31)];
            let mut new_rep = opt[i].rep;
            resolve_offset(&mut new_rep, offval, ll0);
            let hi = len_k.min(MAX_SUBLEN);
            for l in (prev_len + 1).max(MIN_MATCH)..=hi {
                let j = i + l;
                let price = base + ll_owed + ocost + prices.ml[ml_code(l as u32)];
                if price < opt[j].price {
                    opt[j] = Opt {
                        price,
                        mlen: l as u32,
                        litlen: pend as u32,
                        offval,
                        rep: new_rep,
                    };
                }
            }
            if len_k > MAX_SUBLEN {
                let j = i + len_k;
                let price = base + ll_owed + ocost + prices.ml[ml_code(len_k as u32)];
                if price < opt[j].price {
                    opt[j] = Opt {
                        price,
                        mlen: len_k as u32,
                        litlen: pend as u32,
                        offval,
                        rep: new_rep,
                    };
                }
            }
            prev_len = len_k;
        }
    }

    // Backtrack the cheapest full-block path into sequences (forward order).
    let mut seqs: Vec<Seq> = Vec::new();
    let mut pos = n;
    if opt[pos].mlen == 0 {
        // trailing literal run (no sequence) — skip to the last match end.
        pos -= opt[pos].litlen as usize;
    }
    while pos > 0 {
        let m = opt[pos].mlen as usize;
        let ll = opt[pos].litlen as usize;
        debug_assert!(m >= MIN_MATCH, "backtrack landed off a match boundary");
        seqs.push(Seq {
            lit_len: ll as u32,
            match_len: m as u32,
            offset_value: opt[pos].offval,
        });
        pos -= m + ll;
    }
    seqs.reverse();
    (seqs, opt[n].rep)
}

/// Optimal parse (`btopt`/`btultra`/`btultra2`): a rep-aware dynamic program over
/// a fixed-point cost model. Candidate matches are collected once by a **hybrid**
/// finder — the hash `chain` supplies the small-offset Pareto set (cheapest under
/// the offset/repeat-code cost), and the binary `tree` contributes the longest
/// match regardless of recency, merged in when it beats the chain's reach (which
/// its `depth` bound can miss on far-apart repeats). [`run_dp`] then picks the
/// globally cheapest literal/match sequence. Merging the tree's match can only
/// add an option, so the parse is never worse than the chain alone.
/// `sufficient_len` is the length past which a match is committed whole. When
/// `two_pass` (the `btultra2` strategy), a second DP re-parses against the first
/// parse's actual statistics ([`Prices::from_stats`]). Same contract as
/// [`parse_block`] (persistent state, cross-block offsets, `rep` in lockstep).
#[allow(clippy::too_many_arguments)]
/// A block parse: the chosen sequences + literals and the post-block repeat
/// offsets, plus — for the optimal parse only — an `alt`ernative parse the caller
/// emits alongside the primary, keeping whichever is smaller. The optimal parse
/// uses this to offer a *rep-candidate-enabled* parse as the alternative to the
/// rep-free baseline, so enabling repeat-offset matches can never make a block
/// larger than the baseline (a hard no-regression guard — see COST_MODEL_NOTES.md).
/// `alt` nests at most one level deep (an alt never has its own alt).
pub(crate) struct Parsed {
    pub seqs: Vec<Seq>,
    pub literals: Vec<u8>,
    pub rep: [u32; 3],
    pub alt: Option<Box<Parsed>>,
}

/// Materialize a block's literal buffer (every sequence's literal run, then the
/// trailing run) from its sequence list — the bytes the decoder copies verbatim.
fn materialize_literals(data: &[u8], range: core::ops::Range<usize>, seqs: &[Seq]) -> Vec<u8> {
    let (start, end) = (range.start, range.end);
    let mut literals = Vec::new();
    let mut p = start;
    for s in seqs {
        literals.extend_from_slice(&data[p..p + s.lit_len as usize]);
        p += s.lit_len as usize + s.match_len as usize;
    }
    literals.extend_from_slice(&data[p..end]);
    literals
}

#[allow(clippy::too_many_arguments)]
pub fn opt_parse_block(
    data: &[u8],
    range: core::ops::Range<usize>,
    state: &mut ChainState,
    tree: &mut BtState,
    max_offset: usize,
    rep_in: [u32; 3],
    depth: usize,
    sufficient_len: usize,
    two_pass: bool,
) -> Parsed {
    let (start, end) = (range.start, range.end);
    let n = end - start;
    if n < MIN_MATCH + 1 {
        return Parsed {
            seqs: Vec::new(),
            literals: data[start..end].to_vec(),
            rep: rep_in,
            alt: None,
        };
    }

    let suff = sufficient_len.max(MIN_MATCH);
    // Search cap for the chain walk: large enough that `find_matches` keeps
    // looking past a merely-`suff`-long candidate for the genuinely longest
    // match (the chosen one is then extended fully), but bounded so repetitive
    // data — where the first candidate blows past it — stays cheap.
    let find_cap = suff.max(512);

    // --- match-collection pass: at each position the chain gives the small-offset
    // Pareto matches and the tree the longest match regardless of recency, merged
    // when it's longer. Both structures index every position (so the tree's index
    // stays complete and the chain stays current); the DP(s) below read the
    // recorded matches without touching either, so the `btultra2` pass is one more
    // DP, not another search. ---
    let mut matches_flat: Vec<(u32, u32)> = Vec::new();
    let mut match_starts: Vec<u32> = vec![0u32; n + 1];
    let mut pos_long: Vec<Option<(u32, u32)>> = vec![None; n];
    let mut scratch: Vec<(u32, u32)> = Vec::new();
    let mut inserted = start;
    // Positions before `skip_until` lie inside a committed long match — index
    // them but don't search (this is what keeps repetitive data O(n)).
    let mut skip_until = start;

    for i in 0..n {
        match_starts[i] = matches_flat.len() as u32;
        let ap = start + i;
        if ap + MIN_MATCH > end {
            continue;
        }
        // Index every position strictly before `ap` (in both finders) so the
        // search sees only earlier positions — never `ap` itself, an offset-0
        // self-match. (`ap` is indexed *after* the search below.)
        while inserted < ap {
            state.insert(data, inserted);
            tree.insert_bt1(data, inserted, end, max_offset, depth);
            inserted += 1;
        }
        if ap < skip_until {
            // inside a committed long match — index `ap` and move on.
            if inserted == ap {
                state.insert(data, ap);
                tree.insert_bt1(data, ap, end, max_offset, depth);
                inserted = ap + 1;
            }
            continue;
        }

        state.find_matches(data, ap..end, max_offset, depth, find_cap, &mut scratch);
        state.insert(data, ap);
        // The tree's longest match (it also inserts `ap`). Contribute it only when
        // it's a long-range *long* match the chain missed — `≥ suff` (so it's
        // committed whole and its offset amortizes over many bytes) and longer
        // than the chain's reach. Merging *shorter* tree matches would tempt the
        // DP's predefined-price proxy into large-offset choices it misprices (the
        // proxy isn't the real FSE cost), which is what regressed `json` when the
        // tree replaced the chain. Restricting to long matches keeps the chain's
        // cheap small-offset Pareto set intact while recovering far-apart repeats.
        let (tree_len, tree_off) = tree.insert_and_get_longest(data, ap, end, max_offset, depth);
        inserted = ap + 1;
        let chain_best = scratch.last().map_or(0, |&(l, _)| l as usize);
        if tree_len >= suff && tree_len > chain_best {
            scratch.push((tree_len as u32, tree_off as u32));
        }
        let best = match scratch.last() {
            Some(&b) => b,
            None => continue,
        };

        // The longest candidate may be "sufficient" — extend it fully (a chain
        // candidate was capped at `find_cap`; the tree's is already full, and
        // re-extending is idempotent) and, if so, commit it whole and skip ahead.
        let (best_len, best_off) = best;
        let mut long_len = best_len as usize;
        if long_len >= suff {
            long_len = state.extend_full(data, ap, ap - best_off as usize, end);
        }
        if long_len >= suff {
            pos_long[i] = Some((long_len as u32, best_off));
            skip_until = ap + long_len;
            continue;
        }

        matches_flat.extend_from_slice(&scratch);
    }
    match_starts[n] = matches_flat.len() as u32;

    // --- DP pass 1 (static prior). Identical to the single-pass output. ---
    let prices1 = Prices::predef(&data[start..end]);
    let (seqs1, rep1) = run_dp(
        data,
        start,
        n,
        &prices1,
        &match_starts,
        &matches_flat,
        &pos_long,
        rep_in,
    );

    // --- DP pass 2 (`btultra2`): re-parse against the actual statistics. ---
    let (seqs, final_rep) = if two_pass {
        let prices2 = Prices::from_stats(&data[start..end], &seqs1);
        run_dp(
            data,
            start,
            n,
            &prices2,
            &match_starts,
            &matches_flat,
            &pos_long,
            rep_in,
        )
    } else {
        (seqs1, rep1)
    };

    let literals = materialize_literals(data, start..end, &seqs);
    Parsed {
        seqs,
        literals,
        rep: final_rep,
        alt: None,
    }
}

/// The active match finder for a frame, selected from the level's strategy.
/// `Fast`/`Dfast` use the single-slot [`MatchState`]; `greedy`/`lazy`/`lazy2`
/// (and `btlazy2`) use the hash-chain [`ChainState`]; `btopt`/`btultra`(2) run
/// the optimal parse over the **hybrid** chain + binary-tree finder.
pub enum Finder {
    Fast(MatchState),
    DFast(DFastState),
    Chain {
        state: ChainState,
        lazy_steps: u32,
        depth: usize,
    },
    /// `btlazy2` (L13–15): a lazy2 parse over the chain + binary-tree hybrid.
    BtLazy {
        state: ChainState,
        tree: BtState,
        depth: usize,
        /// Match length at/above which the chain's match is kept as-is; below it,
        /// the tree's longest match may be substituted (see [`bt_lazy_best`]).
        target: usize,
    },
    Opt {
        /// Hash chain — the small-offset Pareto match set.
        state: ChainState,
        /// Binary tree — the recency-independent longest match (long-range reach).
        tree: BtState,
        depth: usize,
        sufficient_len: usize,
        /// `btultra2`: re-parse with a price model rebuilt from the first
        /// parse's actual statistics (see [`opt_parse_block`]).
        two_pass: bool,
    },
}

impl Finder {
    /// Build the finder dictated by `params.strategy` and the level's sizes.
    pub fn new(params: &super::params::CParams) -> Self {
        use super::params::Strategy::*;
        match params.strategy {
            Fast => Finder::Fast(MatchState::new(params.hash_log)),
            Dfast => Finder::DFast(DFastState::new(params.hash_log, params.chain_log)),
            Btopt | Btultra | Btultra2 => Finder::Opt {
                state: ChainState::new(params.hash_log, params.chain_log),
                tree: BtState::new(params.hash_log, params.window_log),
                // The optimal parse visits every position, so the per-position
                // search is the dominant cost — keep it moderate. Long matches are
                // committed greedily (`sufficient_len`) and skip their interior, so
                // this depth/nb_compares mainly governs short-match regions.
                depth: (1usize << params.search_log.min(7)).min(128),
                sufficient_len: (params.target_length as usize).clamp(32, 256),
                two_pass: matches!(params.strategy, Btultra2),
            },
            BtLazy2 => Finder::BtLazy {
                state: ChainState::new(params.hash_log, params.chain_log),
                tree: BtState::new(params.hash_log, params.window_log),
                depth: 1usize << params.search_log.min(10),
                target: (params.target_length as usize).max(MIN_MATCH),
            },
            strat => {
                // Greedy / Lazy / Lazy2 (the chain-only lazy parser).
                let lazy_steps = match strat {
                    Greedy => 0,
                    Lazy => 1,
                    _ => 2,
                };
                Finder::Chain {
                    state: ChainState::new(params.hash_log, params.chain_log),
                    lazy_steps,
                    depth: 1usize << params.search_log.min(10),
                }
            }
        }
    }

    /// Parse one block's `range`, dispatching to the chosen finder. Returns a
    /// [`Parsed`] (the post-block `rep` is in the result, not threaded by `&mut`);
    /// only the optimal parse populates `alt`.
    pub fn parse(
        &mut self,
        data: &[u8],
        range: core::ops::Range<usize>,
        max_offset: usize,
        rep_in: [u32; 3],
    ) -> Parsed {
        // The simpler finders thread `rep` by `&mut` and have no alternative; wrap
        // their `(seqs, literals)` into a `Parsed` with the evolved `rep`.
        let mut rep = rep_in;
        let plain = |seqs, literals, rep| Parsed {
            seqs,
            literals,
            rep,
            alt: None,
        };
        match self {
            Finder::Fast(state) => {
                let (s, l) = parse_block(data, range, state, max_offset, &mut rep);
                plain(s, l, rep)
            }
            Finder::DFast(state) => {
                let (s, l) = dfast_parse_block(data, range, state, max_offset, &mut rep);
                plain(s, l, rep)
            }
            Finder::Chain {
                state,
                lazy_steps,
                depth,
            } => {
                let (s, l) = lazy_parse_block(
                    data,
                    range,
                    state,
                    max_offset,
                    &mut rep,
                    *lazy_steps,
                    *depth,
                );
                plain(s, l, rep)
            }
            Finder::BtLazy {
                state,
                tree,
                depth,
                target,
            } => {
                let (s, l) = bt_lazy_parse_block(
                    data, range, state, tree, max_offset, &mut rep, *depth, *target,
                );
                plain(s, l, rep)
            }
            Finder::Opt {
                state,
                tree,
                depth,
                sufficient_len,
                two_pass,
            } => opt_parse_block(
                data,
                range,
                state,
                tree,
                max_offset,
                rep_in,
                *depth,
                *sufficient_len,
                *two_pass,
            ),
        }
    }

    /// Prime the finder's match tables with the `dict_len` bytes of dictionary
    /// content sitting at the front of `data` (the combined `[dict || input]`
    /// buffer), so back-references from the input can reach into the dictionary.
    /// Mirrors libzstd's `ZSTD_loadDictionaryContent`: every dictionary position
    /// is indexed exactly as the parser would index it, but no sequences are
    /// emitted and the repeat offsets are left untouched (the caller seeds those
    /// from the dictionary header). A position is inserted only where its hashed
    /// bytes (4, or 8 for the `dfast` long table) stay inside the dictionary.
    /// `max_offset` is the frame window (the binary tree's `window_low` bound).
    pub fn prime(&mut self, data: &[u8], dict_len: usize, max_offset: usize) {
        match self {
            Finder::Fast(state) => {
                let mut p = 0;
                while p + MIN_MATCH <= dict_len {
                    state.table[hash4(read_u32(data, p), state.hash_log)] = p as i32;
                    p += 1;
                }
            }
            Finder::DFast(state) => {
                let mut p = 0;
                while p + MIN_MATCH <= dict_len {
                    state.insert(data, p, dict_len);
                    p += 1;
                }
            }
            Finder::Chain { state, .. } => {
                let mut p = 0;
                while p + MIN_MATCH <= dict_len {
                    state.insert(data, p);
                    p += 1;
                }
            }
            Finder::BtLazy {
                state, tree, depth, ..
            }
            | Finder::Opt {
                state, tree, depth, ..
            } => {
                let mut p = 0;
                while p + MIN_MATCH <= dict_len {
                    state.insert(data, p);
                    p += 1;
                }
                tree.prime(data, dict_len, max_offset, *depth);
            }
        }
    }
}

/// Parse `range` while emitting the forced long-distance matches in `ldm` (the
/// output of [`super::ldm::LdmState::generate`]: sorted by position, non-
/// overlapping, each fully inside `range`). The regular `finder` fills the gaps
/// between LDM matches, and `rep` is threaded through both — so every sequence's
/// `offset_value` resolves against the same evolving repeat offsets the decoder
/// will see, whether it came from the gap parse or an LDM long match. Returns
/// `(sequences, literals)` exactly like [`Finder::parse`]; with an empty `ldm`
/// it *is* `finder.parse(range)`. The LDM matches' offsets may exceed the
/// regular window but stay within the frame's advertised (larger) window.
pub fn parse_with_ldm(
    finder: &mut Finder,
    data: &[u8],
    range: core::ops::Range<usize>,
    max_offset: usize,
    rep: &mut [u32; 3],
    ldm: &[super::ldm::LdmSeq],
) -> (Vec<Seq>, Vec<u8>) {
    let end = range.end;
    let mut seqs = Vec::new();
    let mut literals = Vec::new();
    let mut cursor = range.start;
    for m in ldm {
        // Fill the gap before this long match with the regular finder; its
        // trailing literal run becomes the long match's literal length.
        let ll = if cursor < m.pos {
            // Gap parse: take the primary (baseline) parse and thread its `rep`;
            // the optimal parse's rep-candidate `alt` is not used on the LDM path.
            let gap = finder.parse(data, cursor..m.pos, max_offset, *rep);
            *rep = gap.rep;
            // The gap's own sequences consume the front of `gap.literals`; only the
            // trailing run (after the last gap match) is the long match's literal
            // length. Using the whole buffer would double-count those literals.
            let consumed: usize = gap.seqs.iter().map(|s| s.lit_len as usize).sum();
            let trailing = gap.literals.len() - consumed;
            seqs.extend(gap.seqs);
            literals.extend_from_slice(&gap.literals);
            trailing
        } else {
            0
        };
        let ll0 = ll == 0;
        let offset_value = encode_offset(rep, m.offset, ll0);
        resolve_offset(rep, offset_value, ll0);
        seqs.push(Seq {
            lit_len: ll as u32,
            match_len: m.len as u32,
            offset_value,
        });
        cursor = m.pos + m.len;
    }
    if cursor < end {
        let tail = finder.parse(data, cursor..end, max_offset, *rep);
        *rep = tail.rep;
        seqs.extend(tail.seqs);
        literals.extend_from_slice(&tail.literals);
    }
    (seqs, literals)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sequences::{decode, SeqTables};

    /// The binary tree references positions only within its array coverage
    /// (`1 << bt_log`); a larger `max_offset` (as `compress_long` advertises)
    /// must be clamped, or positions farther back alias live node slots and
    /// corrupt the links — silently mis-encoding large inputs. Guards the clamp.
    #[test]
    fn bt_search_window_clamps_to_coverage() {
        let bt_log = 10u32;
        let coverage = 1usize << bt_log;
        let tree = BtState::new(12, bt_log);
        // A max_offset beyond coverage is clamped; within coverage it's unchanged.
        assert_eq!(tree.window_low(5_000_000, 1 << 26), 5_000_000 - coverage);
        assert_eq!(tree.window_low(5_000_000, 500), 5_000_000 - 500);
        assert_eq!(tree.window_low(100, 1 << 26), 0);

        // End-to-end: a token that recurs only *beyond* coverage must not be
        // matched (it's out of the tree's reach) even with a huge max_offset, and
        // any match returned must be byte-real. The token starts at position 1 so
        // that, without the clamp, its far occurrence would (wrongly) be in range.
        let token = b"ZQX7-tree-clamp-probe!";
        let mut data = vec![b'#'];
        data.extend_from_slice(token); // token at [1, ..)
        data.extend(core::iter::repeat(b'.').take(2 * coverage));
        let probe = data.len();
        data.extend_from_slice(token);
        let mut tree = BtState::new(14, bt_log);
        for p in 0..probe {
            tree.insert_bt1(&data, p, data.len(), 1 << 26, 16);
        }
        let (ml, mpos) = tree.find_longest(&data, probe, data.len(), 1 << 26, 16);
        if ml >= MIN_MATCH {
            let off = probe - mpos;
            assert!(
                off <= coverage,
                "tree returned an out-of-coverage offset {off}"
            );
            assert_eq!(
                &data[mpos..mpos + ml],
                &data[probe..probe + ml],
                "tree match not real"
            );
        }
    }

    /// A fixed-stride recurring token with varying interstitial literals forces
    /// the parser to re-match at the same offset repeatedly — exactly the case
    /// repeat-offset codes exist for. Period 12: an 8-byte token + 4 changing
    /// literal bytes.
    fn rep_heavy_input() -> Vec<u8> {
        let mut data = Vec::new();
        for i in 0..300u32 {
            data.extend_from_slice(b"MARKER__");
            data.extend_from_slice(&i.to_le_bytes());
        }
        data
    }

    #[test]
    fn emits_repeat_offset_codes() {
        let data = rep_heavy_input();
        let mut rep = [1u32, 4, 8];
        let (seqs, _lits) = fast_parse(&data, 1 << 17, &mut rep);
        let rep_codes = seqs.iter().filter(|s| s.offset_value <= 3).count();
        assert!(
            rep_codes > 0,
            "expected repeat-offset codes, got none of {} sequences",
            seqs.len()
        );
    }

    /// The parsed (sequences, literals) must reconstruct the input when decoded
    /// from the default repeat offsets — proving the finder's `rep` evolution is
    /// byte-for-byte the decoder's. Drives the real encode→decode path.
    #[test]
    fn rep_coded_parse_round_trips_through_decoder() {
        for data in [
            rep_heavy_input(),
            b"abcdabcdabcdabcdabcd".to_vec(),
            vec![0x55u8; 5000],
        ] {
            let mut rep = [1u32, 4, 8];
            let (seqs, literals) = fast_parse(&data, 1 << 17, &mut rep);

            let mut section = Vec::new();
            super::super::sequences::write_sequences_predefined(&mut section, &seqs).unwrap();

            let mut out = Vec::new();
            let mut tables = SeqTables::default();
            let mut drep = [1u32, 4, 8];
            decode(&section, &literals, &mut out, &mut tables, &mut drep).unwrap();
            assert_eq!(out, data, "rep-coded parse must reconstruct the input");
            // The finder's running rep must match the decoder's after replay.
            assert_eq!(rep, drep, "encoder/decoder repeat-offset state diverged");
        }
    }

    /// The optimal parse must round-trip through the decoder — with its
    /// repeat-offset state intact — in *both* the single-pass (`btopt`/`btultra`)
    /// and the two-pass (`btultra2`) modes. The second pass only re-prices the DP
    /// from the first parse's actual statistics, so it must not break the
    /// encode/decode contract regardless of the parse it lands on. The input
    /// mixes a repeating token (exercising repeat-offset codes) with
    /// pseudo-random bytes (a skewed code distribution — where the second pass
    /// earns its keep).
    #[test]
    fn opt_parse_round_trips_in_both_pass_modes() {
        let mut data = rep_heavy_input();
        let mut s = 0x2468_ace0_1357_9bdfu64;
        for _ in 0..4000 {
            s = s
                .wrapping_mul(6364136223846793005)
                .wrapping_add(1442695040888963407);
            data.push((s >> 33) as u8);
        }
        for two_pass in [false, true] {
            let mut state = ChainState::new(18, 18);
            let mut tree = BtState::new(18, 20);
            let parsed = opt_parse_block(
                &data,
                0..data.len(),
                &mut state,
                &mut tree,
                1 << 20,
                [1u32, 4, 8],
                64,
                64,
                two_pass,
            );
            let (seqs, literals, rep) = (parsed.seqs, parsed.literals, parsed.rep);

            let mut section = Vec::new();
            super::super::sequences::write_sequences(
                &mut section,
                &seqs,
                &super::super::sequences::SeqCTables::default(),
            )
            .unwrap();

            let mut out = Vec::new();
            let mut tables = SeqTables::default();
            let mut drep = [1u32, 4, 8];
            decode(&section, &literals, &mut out, &mut tables, &mut drep).unwrap();
            assert_eq!(
                out, data,
                "two_pass={two_pass}: opt parse must reconstruct the input"
            );
            assert_eq!(
                rep, drep,
                "two_pass={two_pass}: encoder/decoder rep-offset state diverged"
            );
        }
    }
}
