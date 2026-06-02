//! FSE (Finite State Entropy) **encoder** — RFC 8878 §4.1, the inverse of the
//! decoder in [`crate::fse`].
//!
//! This batch covers the table *description*:
//! * [`normalize_counts`] — turn a raw symbol histogram into a valid normalized
//!   distribution (every present symbol ≥ 1, sum = `1 << table_log`). A *valid*
//!   normalization suffices; matching libzstd's exact `FSE_normalizeCount` only
//!   affects ratio, which is a non-goal for now.
//! * [`write_ncount`] — the faithful inverse of [`fse::read_ncount`], ported
//!   from libzstd's `FSE_writeNCount_generic` (forward LSB bit writer, pairing
//!   with the decoder's [`ForwardBitReader`](crate::bits)).
//!
//! The table-build + 2-state encode (the bitstream itself, [`build_ctable`] +
//! [`encode`]) follow, both verified by round-tripping through the decoder's
//! `fse::decompress`.

#[allow(unused_imports)]
use crate::alloc_prelude::*;
use super::super::fse::FSE_MAX_TABLELOG;
use super::bitstream::BitWriter;

#[inline]
fn highbit32(x: u32) -> u32 {
    31 - x.leading_zeros()
}

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

/// libzstd's `FSE_optimalTableLog` (the `minus = 2` variant used for the
/// sequence tables): pick an accuracy log in `[5, max_log]` that trades table
/// precision against the `write_ncount` header cost for `src_size` symbols over
/// an alphabet up to `max_symbol`. Smaller inputs get smaller (cheaper) tables.
///
/// The caller must still raise the result to host one count per present symbol
/// (see [`min_table_log`]); this only chooses the *ratio-optimal* size.
pub fn optimal_table_log(max_log: u32, src_size: usize, max_symbol: usize) -> u32 {
    let max_log = max_log.min(FSE_MAX_TABLELOG);
    let src = src_size.max(2) as u32;
    let max_bits_src = highbit32(src - 1).saturating_sub(2);
    let min_bits_src = highbit32(src) + 1;
    let min_bits_symbols = highbit32(max_symbol.max(1) as u32) + 2;
    let min_bits = min_bits_src.min(min_bits_symbols);
    let mut log = max_log;
    if max_bits_src < log {
        log = max_bits_src;
    }
    if min_bits > log {
        log = min_bits;
    }
    log.clamp(5, max_log)
}

/// Smallest accuracy log that can host one count per present symbol (so every
/// present symbol gets a normalized count ≥ 1).
pub fn min_table_log(num_present: usize) -> u32 {
    (32 - (num_present as u32).saturating_sub(1).leading_zeros()).max(5)
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

/// A built FSE **encode** table — the inverse of [`fse::build_dtable`], ported
/// from libzstd's `FSE_buildCTable`.
#[derive(Clone)]
pub struct FseCTable {
    table_log: u32,
    /// Next-state table indexed by find-state position (`1 << table_log` entries).
    state_table: Vec<u16>,
    /// Per-symbol `deltaNbBits` / `deltaFindState` (`symbolTT`).
    delta_nb_bits: Vec<i32>,
    delta_find_state: Vec<i32>,
    /// Whether each symbol `0..=max_symbol` is encodable by this table (a real
    /// state, i.e. normalized count `!= 0`). A symbol with count `0` is a filler
    /// and must never be encoded; this gates "Repeat" mode reuse.
    present: Vec<bool>,
}

/// Build an FSE encode table from a normalized distribution. The symbol spread
/// (step + `high_threshold` placement of `-1` low-probability symbols) is
/// identical to [`fse::build_dtable`], so the two tables are exact inverses.
pub fn build_ctable(norm: &[i16], max_symbol: usize, table_log: u32) -> FseCTable {
    let size = 1usize << table_log;
    let mask = size - 1;

    // Cumulative symbol starts + the spread positions. A `-1` (probability < 1)
    // symbol is counted as 1 and placed from the high end, exactly as the
    // decoder's build_dtable does.
    let mut cumul = vec![0u32; max_symbol + 2];
    let mut table_symbol = vec![0u8; size];
    let mut high_threshold = size - 1;
    for s in 0..=max_symbol {
        if norm[s] == -1 {
            cumul[s + 1] = cumul[s] + 1;
            table_symbol[high_threshold] = s as u8;
            high_threshold = high_threshold.wrapping_sub(1);
        } else {
            cumul[s + 1] = cumul[s] + norm[s].max(0) as u32;
        }
    }

    let step = (size >> 1) + (size >> 3) + 3;
    let mut pos = 0usize;
    for (s, &count) in norm[..=max_symbol].iter().enumerate() {
        for _ in 0..count.max(0) {
            table_symbol[pos] = s as u8;
            pos = (pos + step) & mask;
            while pos > high_threshold {
                pos = (pos + step) & mask;
            }
        }
    }
    debug_assert_eq!(pos, 0, "FSE spread did not cover the table");

    // Next-state table: position u (= decoder state) maps to encoder state
    // `size + u`, slotted by the symbol's running cumulative index.
    let mut state_table = vec![0u16; size];
    let mut cumul_pos = cumul.clone();
    for (u, &sym) in table_symbol.iter().enumerate() {
        let s = sym as usize;
        state_table[cumul_pos[s] as usize] = (size + u) as u16;
        cumul_pos[s] += 1;
    }

    // Per-symbol transform (libzstd FSE_buildCTable step 4).
    let mut delta_nb_bits = vec![0i32; max_symbol + 1];
    let mut delta_find_state = vec![0i32; max_symbol + 1];
    let mut total: i32 = 0;
    for s in 0..=max_symbol {
        match norm[s] {
            0 => {
                // Filler (symbol never encoded); value kept spec-consistent.
                delta_nb_bits[s] = (((table_log + 1) << 16) as i32) - (size as i32);
            }
            -1 | 1 => {
                delta_nb_bits[s] = ((table_log << 16) as i32) - (size as i32);
                delta_find_state[s] = total - 1;
                total += 1;
            }
            c => {
                let cc = c as u32;
                let max_bits_out = table_log - highbit32(cc - 1);
                let min_state_plus = cc << max_bits_out;
                delta_nb_bits[s] = ((max_bits_out << 16) as i32) - (min_state_plus as i32);
                delta_find_state[s] = total - c as i32;
                total += c as i32;
            }
        }
    }

    let present: Vec<bool> = norm[..=max_symbol].iter().map(|&c| c != 0).collect();

    FseCTable {
        table_log,
        state_table,
        delta_nb_bits,
        delta_find_state,
        present,
    }
}

/// Build a degenerate single-state encode table (`table_log = 0`) that always
/// emits `symbol` for zero bits — the encoder side of the decoder's RLE
/// sequence table. Used when a sequence channel has a single distinct code.
pub fn build_rle_ctable(symbol: u8) -> FseCTable {
    let mut norm = vec![0i16; symbol as usize + 1];
    norm[symbol as usize] = 1;
    build_ctable(&norm, symbol as usize, 0)
}

/// One FSE compression state (`FSE_CState_t`).
pub struct CState {
    value: u32,
}

impl FseCTable {
    /// Whether every code in `codes` is encodable by this table — the validity
    /// gate for reusing it in "Repeat" mode. A code beyond the table's alphabet,
    /// or one whose symbol is a `0`-count filler, cannot be encoded.
    pub fn can_encode(&self, codes: &[usize]) -> bool {
        codes.iter().all(|&c| c < self.present.len() && self.present[c])
    }

    /// Initialize a state for the first symbol it will encode (`FSE_initCState2`).
    pub fn init_state2(&self, symbol: usize) -> CState {
        let dnb = self.delta_nb_bits[symbol];
        let nb_bits_out = ((dnb + (1 << 15)) >> 16) as u32;
        let value = ((nb_bits_out << 16) as i32 - dnb) as u32;
        let idx = (value >> nb_bits_out) as i32 + self.delta_find_state[symbol];
        CState {
            value: self.state_table[idx as usize] as u32,
        }
    }

    /// Encode one symbol: flush `nbBitsOut` low bits of the state, then advance
    /// it (`FSE_encodeSymbol`).
    #[inline]
    pub fn encode_symbol(&self, bw: &mut BitWriter, st: &mut CState, symbol: usize) {
        let nb_bits_out = ((st.value as i32 + self.delta_nb_bits[symbol]) >> 16) as u32;
        bw.add(st.value, nb_bits_out);
        let idx = (st.value >> nb_bits_out) as i32 + self.delta_find_state[symbol];
        st.value = self.state_table[idx as usize] as u32;
    }

    /// Flush a final state (`FSE_flushCState`): its full `table_log` bits.
    #[inline]
    pub fn flush_state(&self, bw: &mut BitWriter, st: &CState) {
        bw.add(st.value, self.table_log);
    }

    /// Exact number of bits the interleaved sequence encoder spends on one
    /// channel whose per-sequence symbols are `codes` (sequence order,
    /// non-empty): the state inits from the last symbol (0 bits), each earlier
    /// symbol charges its `encode_symbol` width, and the state flushes
    /// `table_log` bits. Replays `init_state2` / `encode_symbol` / `flush_state`
    /// without writing, so a channel's table mode can be chosen by exact cost.
    pub fn stream_cost_bits(&self, codes: &[usize]) -> u64 {
        let n = codes.len();
        debug_assert!(n >= 1);
        let mut value = self.init_state2(codes[n - 1]).value;
        let mut bits = self.table_log as u64; // final flush
        for &c in codes[..n - 1].iter().rev() {
            let nb = ((value as i32 + self.delta_nb_bits[c]) >> 16) as u32;
            bits += nb as u64;
            let idx = (value >> nb) as i32 + self.delta_find_state[c];
            value = self.state_table[idx as usize] as u32;
        }
        bits
    }
}

/// Two-state FSE compression of `src` — the exact inverse of
/// [`fse::decompress`]. Requires `src.len() > 2` (shorter inputs aren't worth
/// FSE and the decoder's two-state init assumes ≥ 2 symbols).
///
/// Ported from libzstd's `FSE_compress_usingCTable`, with the 64-bit-unroll
/// flush points dropped (the [`BitWriter`] flushes eagerly, so only the `add`
/// order matters). Symbols are consumed back-to-front; the two states alternate
/// so that on decode `state1` drives the even output positions and `state2` the
/// odd ones, and the last-flushed state (`state1`) is the first the backward
/// reader initializes.
pub fn encode(ct: &FseCTable, src: &[u8]) -> Vec<u8> {
    assert!(src.len() > 2, "FSE encode needs > 2 symbols");
    let mut bw = BitWriter::with_capacity(src.len() / 2 + 8);
    let n = src.len();
    let mut ip = n;

    let mut s1;
    let mut s2;
    if n & 1 == 1 {
        ip -= 1;
        s1 = ct.init_state2(src[ip] as usize);
        ip -= 1;
        s2 = ct.init_state2(src[ip] as usize);
        ip -= 1;
        ct.encode_symbol(&mut bw, &mut s1, src[ip] as usize);
    } else {
        ip -= 1;
        s2 = ct.init_state2(src[ip] as usize);
        ip -= 1;
        s1 = ct.init_state2(src[ip] as usize);
    }

    while ip > 0 {
        ip -= 1;
        ct.encode_symbol(&mut bw, &mut s2, src[ip] as usize);
        ip -= 1;
        ct.encode_symbol(&mut bw, &mut s1, src[ip] as usize);
    }

    ct.flush_state(&mut bw, &s2);
    ct.flush_state(&mut bw, &s1);
    bw.finish()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fse::{build_dtable, decompress, read_ncount};

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

    /// `decompress(encode(x)) == x`: the 2-state FSE encoder must invert the
    /// decoder across many lengths (both parities), alphabets, and table logs.
    #[test]
    fn encode_inverts_decompress() {
        let mut seed = 0xfeed_face_dead_beefu64;
        let mut rng = || {
            seed = seed
                .wrapping_mul(6364136223846793005)
                .wrapping_add(1442695040888963407);
            (seed >> 33) as u32
        };

        for trial in 0..500 {
            let max_log = if trial % 2 == 0 { 6 } else { 9 };
            // Restricted alphabet so the table fits the log; bias toward small.
            let alphabet = (rng() % 30 + 2).min((1u32 << max_log) - 1);
            // Lengths of both parities, from tiny (>2) to a few thousand.
            let n = (rng() as usize % 3000) + 3;
            let src: Vec<u8> = (0..n)
                .map(|_| {
                    let u = rng() % alphabet;
                    ((u * u) / alphabet) as u8
                })
                .collect();

            let mut freq = [0u32; 256];
            for &b in &src {
                freq[b as usize] += 1;
            }
            let max_symbol = (0..256).rev().find(|&s| freq[s] > 0).unwrap();
            let num_present = freq.iter().filter(|&&c| c > 0).count();
            if num_present < 2 {
                continue; // FSE needs at least two symbols
            }
            let table_log = choose_table_log(max_log, num_present);
            let norm = normalize_counts(&freq, n as u32, max_symbol, table_log);

            let ct = build_ctable(&norm, max_symbol, table_log);
            let stream = encode(&ct, &src);

            let dt = build_dtable(&norm, max_symbol, table_log).unwrap();
            let got = decompress(&stream, &dt, n).unwrap();
            assert_eq!(got, src, "FSE round-trip mismatch (trial {trial}, n={n})");
        }
    }
}
