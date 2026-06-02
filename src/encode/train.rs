//! Pure-Rust dictionary trainer (T3.1) — a greedy **COVER**, the core of
//! libzstd's `ZDICT_trainFromBuffer_cover`.
//!
//! Builds a **raw-content** dictionary: it finds the byte segments that cover
//! the most frequently-shared substrings ("dmers") across the sample corpus and
//! concatenates them, most-valuable nearest the **end** of the dictionary —
//! where back-reference offsets into it are smallest. Wrap the result with
//! [`Dictionary::raw`](crate::Dictionary::raw) (or `parse`, which treats a
//! buffer with no structured magic as raw content) and feed it to
//! [`compress_with_dict`](crate::compress_with_dict).
//!
//! This is the single-pool greedy variant: a `d`-byte dmer frequency map counted
//! once per sample, then repeated selection of the highest-coverage `k`-byte
//! segment with the covered dmers zeroed after each pick (so later picks add new
//! coverage rather than repeat it). It does not do COVER's epoch partitioning or
//! the `(d, k)` parameter optimisation, and it does not finalize an entropy
//! header — a structured / tagged dictionary (entropy tables + dict id) is a
//! follow-up. The output is deterministic.

#[allow(unused_imports)]
use crate::alloc_prelude::*;
use alloc::collections::{BTreeMap, BTreeSet};

/// dmer length (bytes). 8 lets a dmer be read directly as a little-endian `u64`,
/// so it is its own exact map key — no hashing, no collisions.
const D: usize = 8;
/// Default segment length (bytes), clamped to the corpus size.
const DEFAULT_SEGMENT: usize = 1024;

#[inline]
fn dmer_at(buf: &[u8], i: usize) -> u64 {
    u64::from_le_bytes([
        buf[i], buf[i + 1], buf[i + 2], buf[i + 3],
        buf[i + 4], buf[i + 5], buf[i + 6], buf[i + 7],
    ])
}

/// Train a raw-content dictionary of at most `max_size` bytes from `samples`
/// using a greedy COVER. Returns the dictionary content (no structured magic);
/// an empty `Vec` if there is nothing useful to extract (no samples, every
/// sample shorter than the dmer length, or `max_size == 0`).
pub fn train_dictionary(samples: &[&[u8]], max_size: usize) -> Vec<u8> {
    if max_size == 0 {
        return Vec::new();
    }

    // Concatenate the samples, remembering each one's [start, end) so dmers and
    // segments never straddle a sample boundary (cross-boundary substrings
    // aren't real and must not be counted or scored).
    let mut buf = Vec::new();
    let mut bounds = Vec::with_capacity(samples.len() + 1);
    for &s in samples {
        bounds.push(buf.len());
        buf.extend_from_slice(s);
    }
    bounds.push(buf.len());
    let total = buf.len();
    if total < D {
        return Vec::new();
    }
    let n_pos = total - D + 1;

    // Which dmer start positions lie wholly inside a single sample.
    let mut valid = vec![false; n_pos];
    for w in 0..samples.len() {
        let (s, e) = (bounds[w], bounds[w + 1]);
        if e >= s + D {
            for slot in valid.iter_mut().take(e - D + 1).skip(s) {
                *slot = true;
            }
        }
    }

    // dmer frequency, counted **once per sample** (so one repetitive sample
    // can't dominate; the score reflects how many samples share a substring).
    let mut freq: BTreeMap<u64, u32> = BTreeMap::new();
    for w in 0..samples.len() {
        let (s, e) = (bounds[w], bounds[w + 1]);
        if e < s + D {
            continue;
        }
        let mut seen = BTreeSet::new();
        for i in s..=(e - D) {
            let dm = dmer_at(&buf, i);
            if seen.insert(dm) {
                *freq.entry(dm).or_insert(0) += 1;
            }
        }
    }

    let k = DEFAULT_SEGMENT.min(total).max(D);
    let seg_dmers = k - D + 1; // dmer start positions inside one segment
    let last_start = total - k; // inclusive max segment start

    // Greedily pick the highest-coverage segment, then zero the freq of the
    // dmers it covers so the next pick adds new coverage rather than repeating.
    let mut selected: Vec<usize> = Vec::new();
    let mut accumulated = 0usize;
    while accumulated < max_size {
        // Slide a window of `seg_dmers` dmer positions, tracking the sum of freq
        // over the *distinct* dmers currently inside it.
        let mut wcount: BTreeMap<u64, u32> = BTreeMap::new();
        let mut score: u64 = 0;
        for (i, &ok) in valid.iter().enumerate().take(seg_dmers) {
            if ok {
                let dm = dmer_at(&buf, i);
                let c = wcount.entry(dm).or_insert(0);
                if *c == 0 {
                    score += freq[&dm] as u64;
                }
                *c += 1;
            }
        }
        let mut best_score = score;
        let mut best_start = 0usize;

        for start in 1..=last_start {
            // Drop the dmer leaving on the left.
            let lo = start - 1;
            if valid[lo] {
                let dm = dmer_at(&buf, lo);
                if let Some(c) = wcount.get_mut(&dm) {
                    *c -= 1;
                    if *c == 0 {
                        score -= freq[&dm] as u64;
                        wcount.remove(&dm);
                    }
                }
            }
            // Take in the dmer entering on the right.
            let hi = start + seg_dmers - 1;
            if valid[hi] {
                let dm = dmer_at(&buf, hi);
                let c = wcount.entry(dm).or_insert(0);
                if *c == 0 {
                    score += freq[&dm] as u64;
                }
                *c += 1;
            }
            if score > best_score {
                best_score = score;
                best_start = start;
            }
        }

        if best_score == 0 {
            break; // nothing left worth covering
        }
        selected.push(best_start);
        accumulated += k;
        for (off, &ok) in valid[best_start..best_start + seg_dmers].iter().enumerate() {
            if ok {
                freq.insert(dmer_at(&buf, best_start + off), 0);
            }
        }
    }

    // Concatenate selected segments with the highest-value (first selected) at
    // the end, where offsets into the dictionary are smallest; trim the
    // low-value front if we overshot `max_size`.
    let mut content = Vec::with_capacity(selected.len() * k);
    for &start in selected.iter().rev() {
        content.extend_from_slice(&buf[start..start + k]);
    }
    if content.len() > max_size {
        let cut = content.len() - max_size;
        content.drain(..cut);
    }
    content
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extracts_shared_structure() {
        // Every sample shares "-COMMON-SUFFIX"; only a single digit varies.
        let samples: Vec<Vec<u8>> = (0..200u32)
            .map(|i| format!("PREFIX-{}-COMMON-SUFFIX", i % 7).into_bytes())
            .collect();
        let refs: Vec<&[u8]> = samples.iter().map(|v| v.as_slice()).collect();
        let dict = train_dictionary(&refs, 256);
        assert!(!dict.is_empty(), "expected a non-empty dictionary");
        assert!(dict.len() <= 256, "dictionary exceeded the size budget");
        assert!(
            dict.windows(6).any(|w| w == b"COMMON"),
            "dictionary should capture the shared token"
        );
    }

    #[test]
    fn most_valuable_segment_is_last() {
        // Token A appears in every sample (highest value); token B in a few. The
        // most-shared content should land nearest the dictionary's end.
        let mut samples: Vec<Vec<u8>> = (0..300u32)
            .map(|i| format!("alpha_shared_token_{}", i % 5).into_bytes())
            .collect();
        for i in 0..20u32 {
            samples.push(format!("beta_rare_chunk_{i}").into_bytes());
        }
        let refs: Vec<&[u8]> = samples.iter().map(|v| v.as_slice()).collect();
        let dict = train_dictionary(&refs, 4096);
        assert!(dict.windows(12).any(|w| w == b"alpha_shared"));
    }

    #[test]
    fn degenerate_inputs() {
        let none: [&[u8]; 0] = [];
        assert!(train_dictionary(&none, 1024).is_empty());
        let tiny: [&[u8]; 2] = [b"ab", b"cd"]; // shorter than the dmer length
        assert!(train_dictionary(&tiny, 1024).is_empty());
        let one: [&[u8]; 1] = [b"abcdefghij"];
        assert!(train_dictionary(&one, 0).is_empty());
    }
}
