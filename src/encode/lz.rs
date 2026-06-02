//! LZ match finding — the parse that turns a byte block into a literals buffer
//! plus a list of [`Seq`]uences.
//!
//! This is the `fast` strategy (zstd level ~1): a single 4-byte hash table
//! holding the most recent position per hash, greedy matching with forward
//! extension (overlap-safe). A found offset that equals one of the three
//! running repeat offsets is coded as a **repeat-offset code** (`offset_value`
//! 1–3) instead of the literal form `offset + 3`, which is cheaper and is what
//! makes structured/periodic data compress well. The stronger strategies
//! (dfast/lazy/btopt) are later ratio refinements. Matching is **block-local**
//! — offsets are relative to the block start, which the decoder reconstructs
//! correctly because its copy offset is relative to the current output end.

#[allow(unused_imports)]
use crate::alloc_prelude::*;
use super::super::sequences::resolve_offset;
use super::sequences::Seq;

/// Minimum match length (in bytes) the fast parser will emit.
const MIN_MATCH: usize = 4;
/// Hash-table log (entries = `1 << HASH_LOG`).
const HASH_LOG: u32 = 17;

#[inline]
fn read_u32(data: &[u8], p: usize) -> u32 {
    u32::from_le_bytes([data[p], data[p + 1], data[p + 2], data[p + 3]])
}

#[inline]
fn hash4(v: u32) -> usize {
    (v.wrapping_mul(2654435761) >> (32 - HASH_LOG)) as usize
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

/// Parse `data` into `(sequences, literals)`. `literals` is the concatenation of
/// every literal run (including the trailing run after the last match);
/// reconstructing it requires copying `lit_len` literals then the match, per
/// sequence, exactly as [`crate::sequences::decode`] does.
///
/// `max_offset` bounds back-references to the advertised window. `rep` carries
/// the three running repeat offsets (the decoder's per-frame state, `[1, 4, 8]`
/// at frame start); it is read to detect repeat-offset codes and updated in
/// lockstep with the decoder for every sequence emitted. The caller must commit
/// the updated `rep` only if this block's compressed form is actually used (a
/// raw/RLE block leaves the decoder's `rep` untouched).
pub fn fast_parse(data: &[u8], max_offset: usize, rep: &mut [u32; 3]) -> (Vec<Seq>, Vec<u8>) {
    let n = data.len();
    let mut seqs = Vec::new();
    let mut literals = Vec::new();

    if n < MIN_MATCH + 1 {
        literals.extend_from_slice(data);
        return (seqs, literals);
    }

    let mut table = vec![-1i32; 1usize << HASH_LOG];
    let mut anchor = 0usize; // start of the pending literal run
    let mut p = 0usize;
    let limit = n - MIN_MATCH; // last position with 4 readable bytes

    while p <= limit {
        let v = read_u32(data, p);
        let h = hash4(v);
        let cand = table[h];
        table[h] = p as i32;

        if cand >= 0 {
            let c = cand as usize;
            let offset = p - c;
            if offset <= max_offset && offset >= 1 && data[c..c + MIN_MATCH] == data[p..p + MIN_MATCH]
            {
                // Extend the match forward (overlap-safe: comparing against the
                // original data validates the decoder's repeating copy).
                let mut ml = MIN_MATCH;
                while p + ml < n && data[c + ml] == data[p + ml] {
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

                // Insert a couple of interior positions so later matches can
                // reference inside this one (cheap ratio win for `fast`).
                let mut q = p + 1;
                let stop = (p + ml).min(limit + 1);
                while q < stop {
                    table[hash4(read_u32(data, q))] = q as i32;
                    q += 1;
                }

                p += ml;
                anchor = p;
                continue;
            }
        }
        p += 1;
    }

    literals.extend_from_slice(&data[anchor..]);
    (seqs, literals)
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
