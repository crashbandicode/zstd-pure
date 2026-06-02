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
use super::super::sequences::resolve_offset;
use super::sequences::Seq;

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

/// The active match finder for a frame, selected from the level's strategy.
/// `Fast`/`Dfast` use the single-slot [`MatchState`]; every richer strategy
/// uses the hash-chain [`ChainState`] with `lazy_steps` look-ahead (the `bt*`
/// strategies map to `lazy2` until the optimal parse lands).
pub enum Finder {
    Fast(MatchState),
    Chain {
        state: ChainState,
        lazy_steps: u32,
        depth: usize,
    },
}

impl Finder {
    /// Build the finder dictated by `params.strategy` and the level's sizes.
    pub fn new(params: &super::params::CParams) -> Self {
        use super::params::Strategy::*;
        match params.strategy {
            Fast | Dfast => Finder::Fast(MatchState::new(params.hash_log)),
            strat => {
                let lazy_steps = match strat {
                    Greedy => 0,
                    Lazy => 1,
                    _ => 2, // lazy2 and the bt* strategies (mapped to lazy2 for now)
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
            Finder::Chain { state, lazy_steps, depth } => {
                lazy_parse_block(data, range, state, max_offset, rep, *lazy_steps, *depth)
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
