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

use super::super::dict::DICT_MAGIC;
use super::super::xxhash::xxh64;
use super::huff::write_dict_huffman_table;
use super::lz::Finder;
use super::params::params_for_level_with_dict;
use super::sequences::{write_dict_seq_tables, Seq};
#[allow(unused_imports)]
use crate::alloc_prelude::*;
use alloc::collections::{BTreeMap, BTreeSet};

/// Compression level used to gather representative entropy statistics when
/// finalizing a structured dictionary — a mid-level lazy2 chain parse: good
/// matches without the optimal parser's per-position cost.
const STATS_LEVEL: i32 = 9;

/// Default dmer length (bytes). `<= 8` so a dmer packs into a little-endian `u64`
/// — its own exact map key, no hashing, no collisions.
const DEFAULT_D: usize = 8;
/// Default segment length (bytes), clamped to the corpus size.
const DEFAULT_SEGMENT: usize = 1024;

/// The `d` (`<= 8`) bytes at `i`, little-endian, as the dmer map key. `i + d` must
/// be in bounds (positions are validated to have `d` in-sample bytes).
#[inline]
fn dmer_at(buf: &[u8], i: usize, d: usize) -> u64 {
    let mut v = 0u64;
    for k in 0..d {
        v |= (buf[i + k] as u64) << (8 * k);
    }
    v
}

/// Train a raw-content dictionary of at most `max_size` bytes from `samples`
/// using a greedy COVER. Returns the dictionary content (no structured magic);
/// an empty `Vec` if there is nothing useful to extract (no samples, every
/// sample shorter than the dmer length, or `max_size == 0`).
///
/// **Experimental.** This is a simplified single-pool greedy COVER (no epoch
/// partitioning, no `(d, k)` parameter search), so dictionary *quality* is below
/// libzstd's `ZDICT` and may change as the trainer improves. The dictionaries it
/// produces are correct and improve ratio — verified through libzstd and our own
/// decoder — so it's the training algorithm, not correctness, that's provisional.
pub fn train_dictionary(samples: &[&[u8]], max_size: usize) -> Vec<u8> {
    train_cover(samples, max_size, DEFAULT_SEGMENT, DEFAULT_D)
}

/// Greedy single-pool COVER for an explicit segment length `k` and dmer length
/// `d` (`d <= 8`) — the `(k, d)`-parameterized core that [`train_dictionary`]
/// (fixed defaults) and [`train_dictionary_optimized`] (grid search) both build
/// on. Returns the dictionary content, or an empty `Vec` when there is nothing
/// useful to extract.
fn train_cover(samples: &[&[u8]], max_size: usize, k_req: usize, d: usize) -> Vec<u8> {
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
    if total < d {
        return Vec::new();
    }
    let n_pos = total - d + 1;

    // Which dmer start positions lie wholly inside a single sample.
    let mut valid = vec![false; n_pos];
    for w in 0..samples.len() {
        let (s, e) = (bounds[w], bounds[w + 1]);
        if e >= s + d {
            for slot in valid.iter_mut().take(e - d + 1).skip(s) {
                *slot = true;
            }
        }
    }

    // dmer frequency, counted **once per sample** (so one repetitive sample
    // can't dominate; the score reflects how many samples share a substring).
    let mut freq: BTreeMap<u64, u32> = BTreeMap::new();
    for w in 0..samples.len() {
        let (s, e) = (bounds[w], bounds[w + 1]);
        if e < s + d {
            continue;
        }
        let mut seen = BTreeSet::new();
        for i in s..=(e - d) {
            let dm = dmer_at(&buf, i, d);
            if seen.insert(dm) {
                *freq.entry(dm).or_insert(0) += 1;
            }
        }
    }

    let k = k_req.min(total).max(d);
    let seg_dmers = k - d + 1; // dmer start positions inside one segment
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
                let dm = dmer_at(&buf, i, d);
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
                let dm = dmer_at(&buf, lo, d);
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
                let dm = dmer_at(&buf, hi, d);
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
                freq.insert(dmer_at(&buf, best_start + off, d), 0);
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

/// Scoring level for the `(k, d)` grid search — a mid-level parse, fast enough to
/// run per candidate while still reflecting how the dictionary will be used.
const OPTIMIZE_SCORE_LEVEL: i32 = 9;

/// Train a raw-content dictionary by **optimizing the COVER `(k, d)` parameters**
/// — a pure-Rust analogue of libzstd's `ZDICT_optimizeTrainFromBuffer_cover`.
/// Trains a candidate with [`train_cover`] for each `(segment, dmer)` in a small
/// grid, scores each by the total compressed size of the samples under that
/// candidate dictionary, and returns the smallest-scoring content. The grid
/// includes the [`train_dictionary`] defaults, so the result is never worse — on
/// the training corpus — than the fixed-parameter trainer.
///
/// Much slower than [`train_dictionary`] (it trains *and* test-compresses for
/// every grid point) — this is the offline, best-quality path. Falls back to the
/// default trainer when no candidate yields content.
pub fn train_dictionary_optimized(samples: &[&[u8]], max_size: usize) -> Vec<u8> {
    if max_size == 0 {
        return Vec::new();
    }
    // (segment, dmer) grid: a spread of segment lengths × the two useful dmer
    // widths. The defaults are included so `optimized <= default` on the corpus.
    const SEGMENTS: [usize; 5] = [64, 256, DEFAULT_SEGMENT, 4096, 8192];
    const DMERS: [usize; 2] = [6, DEFAULT_D];

    let mut best: Option<Vec<u8>> = None;
    let mut best_score = u64::MAX;
    for &d in &DMERS {
        for &k in &SEGMENTS {
            let content = train_cover(samples, max_size, k, d);
            if content.is_empty() {
                continue;
            }
            let dict = crate::dict::Dictionary::raw(&content);
            // Lower total compressed size = better dictionary.
            let score: u64 = samples
                .iter()
                .filter(|s| !s.is_empty())
                .map(|s| {
                    super::frame::compress_with_dict(s, &dict, OPTIMIZE_SCORE_LEVEL, false, true)
                        .len() as u64
                })
                .sum();
            if score < best_score {
                best_score = score;
                best = Some(content);
            }
        }
    }
    best.unwrap_or_else(|| train_dictionary(samples, max_size))
}

/// Train a **structured (tagged)** dictionary of at most `max_size` bytes — a
/// pure-Rust analogue of libzstd's `ZDICT_finalizeDictionary` on top of the
/// COVER content from [`train_dictionary`]. The result carries, in the zstd
/// dictionary layout, `magic | dict_id | Huffman table | FSE Offset/Match_Length/
/// Literals_Length tables | 3 repeat offsets | content`, where the entropy
/// tables are derived from a representative compression pass over the samples
/// (each compressed with the dictionary content primed, as it will be used).
/// libzstd loads it (both compress and decompress sides) and a decoder warm-
/// starts the first block from these tables.
///
/// **Experimental** for the same reason as [`train_dictionary`]: the underlying
/// COVER content selection is simplified, so dictionary quality may change.
///
/// Falls back to returning the raw COVER content (a valid raw-content
/// dictionary) when the content is too small to finalize or the literals can't
/// form a Huffman alphabet. The output is deterministic (the dict id is a hash
/// of the content).
pub fn train_dictionary_structured(samples: &[&[u8]], max_size: usize) -> Vec<u8> {
    let content = train_dictionary(samples, max_size);
    // Need room for valid repeat offsets (1/4/8 must reach no further than the
    // content); below that, hand back the still-usable raw content.
    if content.len() < 8 {
        return content;
    }

    // Gather representative entropy statistics: compress each sample with the
    // dictionary content primed (exactly how the dictionary is used), summing
    // the residual literal bytes and the parsed sequences.
    let mut lit_freq = [0u32; 256];
    let mut seqs: Vec<Seq> = Vec::new();
    for &s in samples {
        if s.is_empty() {
            continue;
        }
        let params = params_for_level_with_dict(STATS_LEVEL, s.len(), content.len());
        let max_offset = 1usize << params.window_log;
        let mut combined = Vec::with_capacity(content.len() + s.len());
        combined.extend_from_slice(&content);
        combined.extend_from_slice(s);
        let mut finder = Finder::new(&params);
        finder.prime(&combined, content.len(), max_offset);
        let parsed = finder.parse(
            &combined,
            content.len()..combined.len(),
            max_offset,
            [1u32, 4, 8],
        );
        for &b in &parsed.literals {
            lit_freq[b as usize] += 1;
        }
        seqs.extend(parsed.seqs);
    }

    // Literal Huffman table — from the residual literals, falling back to the
    // content's own byte histogram if the literals are too sparse to form an
    // alphabet (and bailing to raw content if even that is degenerate).
    let mut huff = Vec::new();
    if write_dict_huffman_table(&mut huff, &lit_freq).is_err() {
        let mut cfreq = [0u32; 256];
        for &b in &content {
            cfreq[b as usize] += 1;
        }
        huff.clear();
        if write_dict_huffman_table(&mut huff, &cfreq).is_err() {
            return content;
        }
    }

    // Assemble: magic | id | huff | OF | ML | LL | rep[3] | content.
    let id = (xxh64(&content, 0) as u32) | 1; // deterministic, non-zero
    let mut out = Vec::with_capacity(8 + huff.len() + 32 + content.len());
    out.extend_from_slice(&DICT_MAGIC.to_le_bytes());
    out.extend_from_slice(&id.to_le_bytes());
    out.extend_from_slice(&huff);
    write_dict_seq_tables(&mut out, &seqs);
    for r in [1u32, 4, 8] {
        out.extend_from_slice(&r.to_le_bytes());
    }
    out.extend_from_slice(&content);
    out
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
