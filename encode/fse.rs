//! FSE (Finite State Entropy) **encoder** — RFC 8478 §4.1, the inverse of the
//! decoder in [`crate::zstd_pure::fse`].
//!
//! This batch covers the table *description*:
//! * [`normalize_counts`] — turn a raw symbol histogram into a valid normalized
//!   distribution (every present symbol ≥ 1, sum = `1 << table_log`). A *valid*
//!   normalization suffices; matching libzstd's exact `FSE_normalizeCount` only
//!   affects ratio, which is a non-goal for now.
//! * [`write_ncount`] — the faithful inverse of [`fse::read_ncount`], ported
//!   from libzstd's `FSE_writeNCount_generic` (forward LSB bit writer, pairing
//!   with the decoder's [`ForwardBitReader`](crate::zstd_pure::bits)).
//!
//! The table-build + 2-state encode (the bitstream itself) land in the next
//! batch; both are verified by round-tripping through the decoder.

use super::super::fse::FSE_MAX_TABLELOG;

/// Pick a valid accuracy log for `num_present` symbols within `[5, max_log]`.
///
/// Any log with `1 << log >= num_present` (so every present symbol can hold a
/// count ≥ 1) and `5 <= log <= max_log` is valid; we take the channel maximum
/// for accuracy. (libzstd shrinks it for tiny inputs to save header bytes — a
/// ratio refinement, not a correctness one.)
pub fn choose_table_log(max_log: u32, num_present: usize) -> u32 {
    debug_assert!(num_present >= 1);
    let max_log = max_log.min(FSE_MAX_TABLELOG);
    // Smallest log that can host one count per present symbol (`ceil(log2)`).
    let need = (32 - (num_present as u32).saturating_sub(1).leading_zeros()).max(5);
    debug_assert!(need <= max_log, "alphabet of {num_present} too large for log {max_log}");
    max_log
}

/// Normalize a raw histogram to a distribution summing to `1 << table_log`.
///
/// Every present symbol (`freq > 0`) receives a count ≥ 1; absent symbols get
/// 0. The result is suitable for both [`write_ncount`] and the encode table
/// build. Returns counts indexed by symbol `0..=max_symbol`.
pub fn normalize_counts(
    freq: &[u32],
    total: u32,
    max_symbol: usize,
    table_log: u32,
) -> Vec<i16> {
    let size = 1u32 << table_log;
    let mut norm = vec![0i16; max_symbol + 1];
    debug_assert!(total > 0);

    let mut assigned: u32 = 0;
    let mut largest = 0usize;
    let mut largest_freq = 0u32;
    for (s, n) in norm.iter_mut().enumerate() {
        let f = freq[s];
        if f == 0 {
            continue;
        }
        // Proportional share, floored, but never below 1 for a present symbol.
        let mut p = ((f as u64 * size as u64) / total as u64) as u32;
        if p == 0 {
            p = 1;
        }
        *n = p as i16;
        assigned += p;
        if f > largest_freq {
            largest_freq = f;
            largest = s;
        }
    }

    // Reconcile rounding so the counts sum to exactly `size`. Adjust the
    // most-frequent symbol first (it absorbs the slack with the least relative
    // distortion); spill to any other symbol that can spare/accept a unit.
    while assigned > size {
        if norm[largest] > 1 {
            norm[largest] -= 1;
            assigned -= 1;
        } else {
            // Largest is pinned at 1; take from any symbol with count > 1.
            let donor = norm.iter().position(|&c| c > 1);
            match donor {
                Some(d) => {
                    norm[d] -= 1;
                    assigned -= 1;
                }
                None => break, // every present symbol already at 1 (assigned == num_present <= size)
            }
        }
    }
    while assigned < size {
        norm[largest] += 1;
        assigned += 1;
    }

    norm
}

/// Write an FSE table description — the exact inverse of [`fse::read_ncount`].
///
/// Ported from libzstd's `FSE_writeNCount_generic`: a forward LSB bit
/// accumulator, `value = count + 1` per symbol (so a normalized `0` ↦ `1`,
/// `-1` ↦ `0`), small values (`< max`) in `nbBits − 1` bits and large ones in
/// `nbBits`, with the `previousIs0` zero-run RLE. The result is consumed
/// byte-for-byte by `read_ncount`.
pub fn write_ncount(norm: &[i16], max_symbol: usize, table_log: u32) -> Vec<u8> {
    let table_size = 1i32 << table_log;
    let alphabet = max_symbol + 1;
    let mut out = Vec::new();
    let mut bit_stream: u32 = 0;
    let mut bit_count: i32 = 0;
    let mut remaining = table_size + 1;
    let mut threshold = table_size;
    let mut nb_bits = table_log as i32 + 1;
    let mut symbol = 0usize;
    let mut previous_is0 = false;

    // Accuracy log (stored as `table_log - 5` in 4 bits).
    bit_stream |= (table_log - 5) << bit_count;
    bit_count += 4;

    while symbol < alphabet && remaining > 1 {
        if previous_is0 {
            let mut run_start = symbol;
            while symbol < alphabet && norm[symbol] == 0 {
                symbol += 1;
            }
            if symbol == alphabet {
                break;
            }
            while symbol >= run_start + 24 {
                run_start += 24;
                bit_stream |= 0xFFFFu32 << bit_count;
                out.push(bit_stream as u8);
                out.push((bit_stream >> 8) as u8);
                bit_stream >>= 16;
            }
            while symbol >= run_start + 3 {
                run_start += 3;
                bit_stream |= 3u32 << bit_count;
                bit_count += 2;
            }
            bit_stream |= ((symbol - run_start) as u32) << bit_count;
            bit_count += 2;
            if bit_count > 16 {
                out.push(bit_stream as u8);
                out.push((bit_stream >> 8) as u8);
                bit_stream >>= 16;
                bit_count -= 16;
            }
        }

        {
            let count = norm[symbol] as i32;
            symbol += 1;
            let max = (2 * threshold - 1) - remaining;
            remaining -= count.abs();
            let mut c = count + 1; // "+1 for extra accuracy"
            if c >= threshold {
                c += max;
            }
            bit_stream |= (c as u32) << bit_count;
            bit_count += nb_bits;
            if c < max {
                bit_count -= 1;
            }
            previous_is0 = c == 1;
            while remaining < threshold {
                nb_bits -= 1;
                threshold >>= 1;
            }
        }

        if bit_count > 16 {
            out.push(bit_stream as u8);
            out.push((bit_stream >> 8) as u8);
            bit_stream >>= 16;
            bit_count -= 16;
        }
    }

    // Flush the trailing bits.
    let nbytes = ((bit_count + 7) / 8) as usize;
    for i in 0..nbytes {
        out.push((bit_stream >> (8 * i)) as u8);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::zstd_pure::fse::read_ncount;

    /// `read_ncount(write_ncount(norm))` must reproduce `norm` exactly, for many
    /// random distributions across both weight (≤6) and sequence (≤9) logs.
    #[test]
    fn write_ncount_inverts_read_ncount() {
        let mut seed = 0x1234_5678_9abc_def0u64;
        let mut rng = || {
            seed = seed
                .wrapping_mul(6364136223846793005)
                .wrapping_add(1442695040888963407);
            (seed >> 33) as u32
        };

        for trial in 0..400 {
            let max_log = if trial % 2 == 0 { 6 } else { 9 };
            let max_symbol = (rng() as usize % 40) + 1;
            // Random frequencies; ensure at least 2 present and total > 0.
            let mut freq = vec![0u32; max_symbol + 1];
            let mut total = 0u32;
            for f in freq.iter_mut() {
                if rng() % 3 != 0 {
                    let v = rng() % 100 + 1;
                    *f = v;
                    total += v;
                }
            }
            // Guarantee the top symbol is present (defines max_symbol) and ≥2 present.
            if freq[max_symbol] == 0 {
                freq[max_symbol] = 1;
                total += 1;
            }
            if freq.iter().filter(|&&c| c > 0).count() < 2 {
                freq[0] += 1;
                total += 1;
            }
            let num_present = freq.iter().filter(|&&c| c > 0).count();
            let table_log = choose_table_log(max_log, num_present);

            let norm = normalize_counts(&freq, total, max_symbol, table_log);
            // Sanity: valid normalization.
            let sum: i32 = norm.iter().map(|&c| (c as i32).abs()).sum();
            assert_eq!(sum, 1 << table_log, "norm sum != table size (trial {trial})");
            for (s, &c) in norm.iter().enumerate() {
                if freq[s] > 0 {
                    assert!(c >= 1, "present symbol {s} got count {c}");
                } else {
                    assert_eq!(c, 0);
                }
            }

            let header = write_ncount(&norm, max_symbol, table_log);
            let nc = read_ncount(&header, max_log).expect("read_ncount must parse our header");
            assert_eq!(nc.table_log, table_log, "table_log mismatch (trial {trial})");
            assert_eq!(nc.max_symbol, max_symbol, "max_symbol mismatch (trial {trial})");
            assert_eq!(
                &nc.counts[..=max_symbol],
                &norm[..],
                "normalized counts mismatch (trial {trial})"
            );
            assert_eq!(
                nc.bytes_consumed,
                header.len(),
                "byte length mismatch (trial {trial})"
            );
        }
    }
}
