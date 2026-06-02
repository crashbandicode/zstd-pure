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

#[allow(unused_imports)]
use crate::alloc_prelude::*;
use super::super::sequences::{resolve_offset, LL_BITS, LL_DEFAULT, ML_BITS, ML_DEFAULT, OF_DEFAULT};
use super::sequences::{ll_code, ml_code, of_code, Seq};

/// Minimum match length (in bytes) the fast parser will emit.
const MIN_MATCH: usize = 4;
/// Hash log used by the single-block [`fast_parse`] convenience wrapper.
const DEFAULT_HASH_LOG: u32 = 17;
/// Clamp for a [`MatchState`] hash log: floor avoids a degenerate table, ceiling
/// caps the allocation at `1 << 22` entries (16 MiB) for very high levels.
const MIN_HASH_LOG: u32 = 6;
const MAX_HASH_LOG: u32 = 22;

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
        data[p], data[p + 1], data[p + 2], data[p + 3],
        data[p + 4], data[p + 5], data[p + 6], data[p + 7],
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
            if offset <= max_offset && offset >= 1 && data[c..c + MIN_MATCH] == data[p..p + MIN_MATCH]
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
/// `data`'s start). Used in tests; the frame encoder uses [`parse_block`] with a
/// persistent [`MatchState`] so matches span block boundaries.
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
    fn find(&self, data: &[u8], ip: usize, end: usize, max_offset: usize, depth: usize) -> (usize, usize) {
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

/// Optimal parse (`btopt`): a rep-aware dynamic program over a fixed-point cost
/// model. At each position the chain finder enumerates candidate matches; the DP
/// picks the globally cheapest sequence of literals and matches — so a shorter
/// match now can win when it enables a cheaper continuation, and rep matches are
/// preferred because their offset code is tiny. `depth` bounds the chain walk;
/// `sufficient_len` caps the per-position length search (longer matches are
/// taken whole). Same contract as [`parse_block`] (persistent `state`,
/// cross-block offsets, `rep` updated in decoder lockstep).
pub fn opt_parse_block(
    data: &[u8],
    range: core::ops::Range<usize>,
    state: &mut ChainState,
    max_offset: usize,
    rep: &mut [u32; 3],
    depth: usize,
    sufficient_len: usize,
) -> (Vec<Seq>, Vec<u8>) {
    let (start, end) = (range.start, range.end);
    let n = end - start;
    let mut literals = Vec::new();
    if n < MIN_MATCH + 1 {
        literals.extend_from_slice(&data[start..end]);
        return (Vec::new(), literals);
    }

    // --- price model (fixed-point 1/256 bit) ---
    // Literal byte cost from a block-wide histogram prior (it overcounts matched
    // bytes, but serves only as a relative literal-vs-match prior).
    let mut freq = [0u32; 256];
    for &b in &data[start..end] {
        freq[b as usize] += 1;
    }
    let l_total = log2_fp(n as u32);
    let lit_price = |b: u8| -> u64 {
        let f = freq[b as usize].max(1);
        (l_total.saturating_sub(log2_fp(f)) as u64).max(16) // >= 1/16 bit
    };
    // Sequence-code prices from the predefined tables (a fixed prior; the actual
    // encoding re-picks tables in `write_sequences`). Predefined accuracy logs:
    // LL 6 / OF 5 / ML 6, matching the decoder's `resolve_table`.
    let code_price = |count: i16, log: u32| -> u64 {
        let c = if count <= 0 { 1u32 } else { count as u32 };
        ((log * 256).saturating_sub(log2_fp(c))) as u64
    };
    let mut ll_price = [0u64; 36];
    for (c, p) in ll_price.iter_mut().enumerate() {
        *p = code_price(LL_DEFAULT[c], 6) + (LL_BITS[c] as u64) * 256;
    }
    let mut ml_price = [0u64; 53];
    for (c, p) in ml_price.iter_mut().enumerate() {
        *p = code_price(ML_DEFAULT[c], 6) + (ML_BITS[c] as u64) * 256;
    }
    let mut off_price = [0u64; 32];
    for (c, p) in off_price.iter_mut().enumerate() {
        let count = if c < OF_DEFAULT.len() { OF_DEFAULT[c] } else { -1 };
        *p = code_price(count, 5) + (c as u64) * 256;
    }

    let big = u64::MAX / 4;
    let mut opt = vec![Opt { price: big, mlen: 0, litlen: 0, offval: 0, rep: [0; 3] }; n + 1];
    opt[0] = Opt { price: 0, mlen: 0, litlen: 0, offval: 0, rep: *rep };

    let suff = sufficient_len.max(MIN_MATCH);
    // Search cap for the chain walk: large enough that `find_matches` keeps
    // looking past a merely-`suff`-long candidate for the genuinely longest
    // match (the chosen one is then extended fully), but bounded so repetitive
    // data — where the first candidate blows past it — stays cheap.
    let find_cap = suff.max(512);
    // Cap the per-position sub-length search; lengths past this are only placed
    // at their full value (composed via shorter matches when finer is needed).
    const MAX_SUBLEN: usize = 64;
    let mut matches: Vec<(u32, u32)> = Vec::new();
    let mut inserted = start;
    // Positions before `skip_until` lie inside a committed long match — index
    // them but don't search (this is what keeps repetitive data O(n)).
    let mut skip_until = start;

    for i in 0..n {
        let base = opt[i].price;
        let pend = if opt[i].mlen > 0 { 0 } else { opt[i].litlen as usize };

        // Literal extension i -> i+1. Pre-pay the literal-length code so a
        // closing match adds no further ll cost; opening a run pays the ll code
        // for length 1, growing it pays the delta.
        let ll_add = if pend == 0 {
            ll_price[ll_code(1)]
        } else {
            ll_price[ll_code(pend as u32 + 1)].saturating_sub(ll_price[ll_code(pend as u32)])
        };
        let cand = base + lit_price(data[start + i]) + ll_add;
        if cand < opt[i + 1].price {
            opt[i + 1] = Opt { price: cand, mlen: 0, litlen: pend as u32 + 1, offval: 0, rep: opt[i].rep };
        }

        let ap = start + i;
        if ap + MIN_MATCH > end {
            continue;
        }
        // Index every position strictly before `ap` so the search sees only
        // earlier positions — never `ap` itself, which would be an offset-0
        // self-match. (`ap` is indexed *after* the search below.)
        while inserted < ap {
            state.insert(data, inserted);
            inserted += 1;
        }
        if ap < skip_until {
            // inside a committed long match — index `ap` and move on.
            if inserted == ap {
                state.insert(data, ap);
                inserted = ap + 1;
            }
            continue;
        }

        state.find_matches(data, ap..end, max_offset, depth, find_cap, &mut matches);
        if inserted == ap {
            state.insert(data, ap);
            inserted = ap + 1;
        }
        let best = match matches.last() {
            Some(&b) => b,
            None => continue,
        };
        let ll0 = pend == 0;
        // ll code is pre-paid for pend>0; a zero-literal match still owes the ll
        // code for length 0.
        let ll_owed = if pend == 0 { ll_price[ll_code(0)] } else { 0 };

        // If the longest match hit the search cap it may be far longer — extend
        // it fully and, if it's "sufficient", commit it whole and skip ahead.
        let (best_len, best_off) = best;
        let mut long_len = best_len as usize;
        if long_len >= suff {
            long_len = state.extend_full(data, ap, ap - best_off as usize, end);
        }
        if long_len >= suff {
            let offval = encode_offset(&opt[i].rep, best_off, ll0);
            let ocost = off_price[of_code(offval).min(31)];
            let mut new_rep = opt[i].rep;
            resolve_offset(&mut new_rep, offval, ll0);
            let j = i + long_len;
            let price = base + ll_owed + ocost + ml_price[ml_code(long_len as u32)];
            if price < opt[j].price {
                opt[j] = Opt { price, mlen: long_len as u32, litlen: pend as u32, offval, rep: new_rep };
            }
            skip_until = ap + long_len;
            continue;
        }

        // Short-match region: weigh each candidate length — where the optimal
        // parse earns its keep. Each length is provided most cheaply by the
        // first Pareto entry reaching it, so cover only (prev_len, len_k].
        let mut prev_len = MIN_MATCH - 1;
        for &(len_k, off_k) in &matches {
            let len_k = len_k as usize;
            let offval = encode_offset(&opt[i].rep, off_k, ll0);
            let ocost = off_price[of_code(offval).min(31)];
            let mut new_rep = opt[i].rep;
            resolve_offset(&mut new_rep, offval, ll0);
            let hi = len_k.min(MAX_SUBLEN);
            for l in (prev_len + 1).max(MIN_MATCH)..=hi {
                let j = i + l;
                let price = base + ll_owed + ocost + ml_price[ml_code(l as u32)];
                if price < opt[j].price {
                    opt[j] = Opt { price, mlen: l as u32, litlen: pend as u32, offval, rep: new_rep };
                }
            }
            if len_k > MAX_SUBLEN {
                let j = i + len_k;
                let price = base + ll_owed + ocost + ml_price[ml_code(len_k as u32)];
                if price < opt[j].price {
                    opt[j] = Opt { price, mlen: len_k as u32, litlen: pend as u32, offval, rep: new_rep };
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
        seqs.push(Seq { lit_len: ll as u32, match_len: m as u32, offset_value: opt[pos].offval });
        pos -= m + ll;
    }
    seqs.reverse();

    // Materialize literals and commit the post-block repeat-offset state.
    let mut p = start;
    for s in &seqs {
        literals.extend_from_slice(&data[p..p + s.lit_len as usize]);
        p += s.lit_len as usize + s.match_len as usize;
    }
    literals.extend_from_slice(&data[p..end]);
    *rep = opt[n].rep;
    (seqs, literals)
}

/// The active match finder for a frame, selected from the level's strategy.
/// `Fast`/`Dfast` use the single-slot [`MatchState`]; `greedy`/`lazy`/`lazy2`
/// (and `btlazy2`) use the hash-chain [`ChainState`]; `btopt`/`btultra`(2) use
/// the optimal parse over that same chain finder.
pub enum Finder {
    Fast(MatchState),
    DFast(DFastState),
    Chain {
        state: ChainState,
        lazy_steps: u32,
        depth: usize,
    },
    Opt {
        state: ChainState,
        depth: usize,
        sufficient_len: usize,
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
                // The optimal parse visits every position, so the per-position
                // chain walk is the dominant cost — keep it moderate. Long
                // matches are committed greedily (`sufficient_len`) and skip
                // their interior, so this depth only governs short-match regions.
                depth: (1usize << params.search_log.min(7)).min(128),
                sufficient_len: (params.target_length as usize).clamp(32, 256),
            },
            strat => {
                // Greedy / Lazy / Lazy2 / BtLazy2 (the last as plain lazy2).
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

    /// Parse one block's `range`, dispatching to the chosen finder.
    pub fn parse(
        &mut self,
        data: &[u8],
        range: core::ops::Range<usize>,
        max_offset: usize,
        rep: &mut [u32; 3],
    ) -> (Vec<Seq>, Vec<u8>) {
        match self {
            Finder::Fast(state) => parse_block(data, range, state, max_offset, rep),
            Finder::DFast(state) => dfast_parse_block(data, range, state, max_offset, rep),
            Finder::Chain { state, lazy_steps, depth } => {
                lazy_parse_block(data, range, state, max_offset, rep, *lazy_steps, *depth)
            }
            Finder::Opt { state, depth, sufficient_len } => {
                opt_parse_block(data, range, state, max_offset, rep, *depth, *sufficient_len)
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sequences::{decode, SeqTables};

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
        for data in [rep_heavy_input(), b"abcdabcdabcdabcdabcd".to_vec(), vec![0x55u8; 5000]] {
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
}
