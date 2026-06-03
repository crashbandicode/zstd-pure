//! Long-distance matching (LDM) — T2.4.
//!
//! A coarse, whole-input index that finds *long* matches at distances beyond the
//! regular match finders' reach (the param table caps the window at 8 MiB for
//! decoder interoperability). It is the encoder side of libzstd's
//! `lib/compress/zstd_ldm.c`: a sparse hash over the input keyed by a long
//! minimum match, probed and inserted at **content-defined** points (when the
//! content's hash has a fixed bit pattern, *not* at a fixed stride), so a repeat
//! is indexed at the same relative offsets in both its copies — making detection
//! independent of absolute alignment. The matches it finds are emitted by the
//! parser as ordinary sequences with large offsets ([`super::lz::parse_with_ldm`]);
//! the decoder needs no changes — it already copies any in-window offset.
//!
//! Purely an encoder concern, run only under the opt-in
//! [`compress_long`](super::frame::compress_long), which advertises the larger
//! window the long offsets require (see the Conformance note in the README).

#[allow(unused_imports)]
use crate::alloc_prelude::*;

/// Minimum length of an LDM match. Long by design: LDM targets big, far-apart
/// repeats and leaves short/near matches to the regular finder (libzstd default).
pub const LDM_MIN_MATCH: usize = 64;

/// Insert/probe at ~1 in `1 << LDM_HASH_RATE_LOG` positions, selected by the
/// content's hash (not its absolute offset), so the two copies of a repeat are
/// indexed at the same relative positions and a match is found regardless of
/// alignment. Sparse enough to keep the index cheap over the whole input.
const LDM_HASH_RATE_LOG: u32 = 5;

/// 64-bit multiplicative hash of the 8-byte window at `p` (`p + 8 <= data.len()`).
#[inline]
fn ldm_hash(data: &[u8], p: usize) -> u64 {
    let v = u64::from_le_bytes([
        data[p], data[p + 1], data[p + 2], data[p + 3],
        data[p + 4], data[p + 5], data[p + 6], data[p + 7],
    ]);
    v.wrapping_mul(0x9E37_79B1_85EB_CA87)
}

/// A long match the LDM index found: copy `len` bytes from `pos - offset`, at
/// absolute input position `pos`. `offset` may exceed the regular 8 MiB window.
#[derive(Debug, Clone, Copy)]
pub struct LdmSeq {
    pub pos: usize,
    pub len: usize,
    pub offset: u32,
}

/// The coarse whole-input index, persistent across a frame's blocks so a match
/// can reference any earlier block within the advertised window.
pub struct LdmState {
    /// Most recent inserted absolute position per hash slot, `-1` = empty.
    table: Vec<i32>,
    hash_log: u32,
}

impl LdmState {
    /// Allocate the index for a frame whose advertised window is `window_log`.
    /// `window_log - 4` slots retain far-back sources across the window without
    /// an excessive allocation (one `i32` per slot, capped at `1 << 24` = 64 MiB).
    pub fn new(window_log: u32) -> Self {
        let hash_log = window_log.saturating_sub(4).clamp(10, 24);
        LdmState {
            table: vec![-1i32; 1usize << hash_log],
            hash_log,
        }
    }

    /// Find non-overlapping long matches for positions in `range`, updating the
    /// index as it scans. A match is emitted only when its `offset` is in
    /// `(min_offset, window]` — LDM contributes *only* matches beyond the regular
    /// finder's reach (`min_offset`), leaving nearer ones to it, so the two never
    /// fight over a position (forcing a near LDM match where the regular parser
    /// would do better can *hurt* ratio on periodic data). Each returned match
    /// lies fully inside `range` (never crossing the block boundary); matches are
    /// sorted by position. Sources may be in earlier blocks (the index persists
    /// across the frame). The index is updated at every gate point regardless, so
    /// a near hit still seeds the slot for a later, farther match.
    pub fn generate(
        &mut self,
        data: &[u8],
        range: core::ops::Range<usize>,
        min_offset: usize,
        window: usize,
    ) -> Vec<LdmSeq> {
        let (start, end) = (range.start, range.end);
        let mut out = Vec::new();
        if end < 8 || end - start < LDM_MIN_MATCH {
            return out;
        }
        let rate_mask: u64 = (1u64 << LDM_HASH_RATE_LOG) - 1;
        let slot_shift = 64 - self.hash_log;
        let gate_shift = 64 - self.hash_log - LDM_HASH_RATE_LOG;
        let limit = end - 8; // last position with 8 readable bytes
        let mut p = start;
        while p <= limit {
            let h = ldm_hash(data, p);
            // Content-defined gate: only ~1 in 2^rate positions is an index point.
            if (h >> gate_shift) & rate_mask != 0 {
                p += 1;
                continue;
            }
            let slot = (h >> slot_shift) as usize;
            let cand = self.table[slot];
            self.table[slot] = p as i32;
            if cand >= 0 {
                let c = cand as usize;
                let offset = p - c;
                if offset > min_offset && offset <= window {
                    // Verify + forward-extend, bounded by the block end so the
                    // match never crosses into the next block.
                    let max_len = end - p;
                    let mut ml = 0usize;
                    while ml < max_len && data[c + ml] == data[p + ml] {
                        ml += 1;
                    }
                    if ml >= LDM_MIN_MATCH {
                        out.push(LdmSeq { pos: p, len: ml, offset: offset as u32 });
                        p += ml; // skip the matched span (don't index its interior)
                        continue;
                    }
                }
            }
            p += 1;
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn finds_a_far_repeat() {
        // A distinctive 4 KB block, ~200 KB of unrelated filler, then the same
        // block again. generate() must surface the second copy as a long match
        // whose offset points back exactly to the first copy.
        let block: Vec<u8> = (0..4096u32).map(|i| (i.wrapping_mul(2654435761) >> 13) as u8).collect();
        let filler: Vec<u8> = (0..200_000u32).map(|i| (i.wrapping_mul(40503) >> 7) as u8).collect();
        let mut data = Vec::new();
        data.extend_from_slice(&block);
        data.extend_from_slice(&filler);
        let dup_at = data.len();
        data.extend_from_slice(&block);

        let mut ldm = LdmState::new(24);
        let matches = ldm.generate(&data, 0..data.len(), 0, 1 << 24);

        let m = matches
            .iter()
            .find(|m| m.pos >= dup_at)
            .expect("LDM should find the far-repeated block");
        // A gate point in the duplicate at `dup_at + φ` matches the first copy at
        // `φ`, so the offset is exactly the distance between the two copies.
        assert_eq!(m.offset as usize, dup_at, "offset should point to the first copy");
        assert!(m.len >= LDM_MIN_MATCH, "match too short: {}", m.len);
        // The match must be real.
        let (p, o, l) = (m.pos, m.offset as usize, m.len);
        assert_eq!(&data[p..p + l], &data[p - o..p - o + l], "LDM match bytes differ");
    }

    #[test]
    fn generated_matches_are_always_valid() {
        // Whatever generate() returns on arbitrary data must be real: long enough,
        // in-window, in-bounds, byte-for-byte correct, sorted, and non-overlapping
        // (these are exactly the invariants the parser relies on).
        let data: Vec<u8> = (0..300_000u32).map(|i| (i.wrapping_mul(2654435761) >> 9) as u8).collect();
        let window = 1usize << 24;
        let mut ldm = LdmState::new(24);
        let matches = ldm.generate(&data, 0..data.len(), 0, window);
        let mut prev_end = 0usize;
        for m in &matches {
            let (p, o, l) = (m.pos, m.offset as usize, m.len);
            assert!(l >= LDM_MIN_MATCH, "match too short: {l}");
            assert!(o >= 1 && o <= window, "offset out of window: {o}");
            assert!(p >= o, "source before buffer start");
            assert!(p + l <= data.len(), "match crosses the end");
            assert_eq!(&data[p..p + l], &data[p - o..p - o + l], "match bytes differ");
            assert!(p >= prev_end, "matches overlap or are out of order");
            prev_end = p + l;
        }
    }
}
