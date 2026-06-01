//! LZ match finding — the parse that turns a byte block into a literals buffer
//! plus a list of [`Seq`]uences.
//!
//! This is the `fast` strategy (zstd level ~1): a single 4-byte hash table
//! holding the most recent position per hash, greedy matching with forward
//! extension (overlap-safe). Offsets are literal (`offset_value = offset + 3`);
//! repeat-offset coding and the stronger strategies (dfast/lazy/btopt) are
//! later ratio refinements. Matching is **block-local** — offsets are relative
//! to the block start, which the decoder reconstructs correctly because its
//! copy offset is relative to the current output end.

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

/// Parse `data` into `(sequences, literals)`. `literals` is the concatenation of
/// every literal run (including the trailing run after the last match);
/// reconstructing it requires copying `lit_len` literals then the match, per
/// sequence, exactly as [`crate::zstd_pure::sequences::decode`] does.
///
/// `max_offset` bounds back-references to the advertised window.
pub fn fast_parse(data: &[u8], max_offset: usize) -> (Vec<Seq>, Vec<u8>) {
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
                seqs.push(Seq {
                    lit_len: lit_len as u32,
                    match_len: ml as u32,
                    offset_value: (offset + 3) as u32,
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
