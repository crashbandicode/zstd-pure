//! Sequence-section **encoding** — RFC 8878 §3.1.1.3.2, the inverse of
//! [`crate::sequences`].
//!
//! A sequence is `(literals_length, match_length, offset_value)`, where
//! `offset_value` is the already-encoded offset (the `repeat + 3` convention the
//! decoder's `resolve_offset` expects). This module emits the count header, the
//! compression-modes byte, and the three-state interleaved FSE bitstream.
//!
//! Only the **predefined** table mode is implemented here (modes byte `0`); the
//! per-block FSE table mode is a later ratio refinement. The bitstream layout is
//! ported from libzstd's `ZSTD_encodeSequences_body`: states init from the last
//! sequence, the body loops backward emitting `OF`/`ML`/`LL` state transitions
//! then the `LL`/`ML`/offset extra bits, and the three states flush in
//! `ML`/`OF`/`LL` order — exactly inverting the decoder's read sequence.

#[allow(unused_imports)]
use crate::alloc_prelude::*;
use super::super::error::Result;
use super::super::sequences::{LL_BASE, LL_BITS, LL_DEFAULT, ML_BASE, ML_BITS, ML_DEFAULT, OF_DEFAULT};
use super::bitstream::BitWriter;
use super::fse::{
    build_ctable, build_rle_ctable, min_table_log, normalize_counts, optimal_table_log,
    write_ncount, FseCTable,
};

// Predefined table parameters (max symbol, accuracy log) — must match the
// decoder's `resolve_table` calls.
const LL_PRED_MAX: usize = 35;
const LL_PRED_LOG: u32 = 6;
const OF_PRED_MAX: usize = 28;
const OF_PRED_LOG: u32 = 5;
const ML_PRED_MAX: usize = 52;
const ML_PRED_LOG: u32 = 6;

// Maximum accuracy log for a per-block (mode 2) FSE table per channel — the
// limits the decoder's `parse_mode` enforces (`read_ncount` max_log).
const LL_MAX_LOG: u32 = 9;
const OF_MAX_LOG: u32 = 8;
const ML_MAX_LOG: u32 = 9;

/// One sequence: copy `lit_len` literals, then a match of `match_len` bytes at
/// the offset encoded by `offset_value` (`repeat + 3`).
#[derive(Debug, Clone, Copy)]
pub struct Seq {
    pub lit_len: u32,
    pub match_len: u32,
    pub offset_value: u32,
}

#[inline]
fn highbit32(x: u32) -> u32 {
    31 - x.leading_zeros()
}

/// Literals-length code for `lit_len` (largest code whose baseline fits).
/// `pub(crate)` so the optimal parser can price literal-length codes.
#[inline]
pub(crate) fn ll_code(lit_len: u32) -> usize {
    (0..LL_BASE.len()).rev().find(|&c| LL_BASE[c] <= lit_len).unwrap()
}

/// Match-length code for `match_len` (`match_len >= 3`).
#[inline]
pub(crate) fn ml_code(match_len: u32) -> usize {
    (0..ML_BASE.len()).rev().find(|&c| ML_BASE[c] <= match_len).unwrap()
}

/// Offset code = `floor(log2(offset_value))` (`offset_value >= 1`).
#[inline]
pub(crate) fn of_code(offset_value: u32) -> usize {
    highbit32(offset_value) as usize
}

/// Write the `Number_of_Sequences` header (RFC 8878 §3.1.1.3.2.1).
fn write_seq_count(out: &mut Vec<u8>, n: usize) {
    if n < 128 {
        out.push(n as u8);
    } else if n < 0x7F00 {
        out.push((128 + (n >> 8)) as u8);
        out.push((n & 0xFF) as u8);
    } else {
        let m = n - 0x7F00;
        out.push(255);
        out.push((m & 0xFF) as u8);
        out.push(((m >> 8) & 0xFF) as u8);
    }
}

/// Encode the three-state interleaved FSE bitstream for `seqs` with the given
/// per-channel encode tables and return the finished bytes. The state layout
/// (init from the last sequence, backward body, `ML`/`OF`/`LL` flush order) is
/// fixed by the decoder; only the tables differ by compression mode.
fn encode_seq_bitstream(
    seqs: &[Seq],
    ll_ct: &FseCTable,
    of_ct: &FseCTable,
    ml_ct: &FseCTable,
) -> Vec<u8> {
    let n = seqs.len();
    let mut bw = BitWriter::with_capacity(n * 2 + 16);

    // Helpers to add the extra (low) bits of each field.
    let add_ll_extra = |bw: &mut BitWriter, s: &Seq, c: usize| {
        bw.add(s.lit_len - LL_BASE[c], LL_BITS[c]);
    };
    let add_ml_extra = |bw: &mut BitWriter, s: &Seq, c: usize| {
        bw.add(s.match_len - ML_BASE[c], ML_BITS[c]);
    };

    // Init the three states from the *last* sequence (encoded back-to-front).
    let last = &seqs[n - 1];
    let ll_last = ll_code(last.lit_len);
    let ml_last = ml_code(last.match_len);
    let of_last = of_code(last.offset_value);
    let mut st_ml = ml_ct.init_state2(ml_last);
    let mut st_of = of_ct.init_state2(of_last);
    let mut st_ll = ll_ct.init_state2(ll_last);
    add_ll_extra(&mut bw, last, ll_last);
    add_ml_extra(&mut bw, last, ml_last);
    bw.add(last.offset_value, of_last as u32); // offset extra = low of_code bits

    // Body: sequences n-2 .. 0, emitting state transitions then extra bits.
    for s in seqs[..n - 1].iter().rev() {
        let ll_c = ll_code(s.lit_len);
        let ml_c = ml_code(s.match_len);
        let of_c = of_code(s.offset_value);
        of_ct.encode_symbol(&mut bw, &mut st_of, of_c);
        ml_ct.encode_symbol(&mut bw, &mut st_ml, ml_c);
        ll_ct.encode_symbol(&mut bw, &mut st_ll, ll_c);
        add_ll_extra(&mut bw, s, ll_c);
        add_ml_extra(&mut bw, s, ml_c);
        bw.add(s.offset_value, of_c as u32);
    }

    // Flush the final states (ML, OF, LL) — read first by the decoder as init.
    ml_ct.flush_state(&mut bw, &st_ml);
    of_ct.flush_state(&mut bw, &st_of);
    ll_ct.flush_state(&mut bw, &st_ll);

    bw.finish()
}

/// Encode a sequences section using the **predefined** FSE tables and append it
/// to `out`. An empty sequence list writes the single-byte `nb_seq = 0` form
/// (the decoder then emits the literals verbatim).
pub fn write_sequences_predefined(out: &mut Vec<u8>, seqs: &[Seq]) -> Result<()> {
    write_seq_count(out, seqs.len());
    if seqs.is_empty() {
        return Ok(());
    }
    out.push(0); // modes byte: LL/OF/ML all Predefined
    let ll_ct = build_ctable(&LL_DEFAULT, LL_PRED_MAX, LL_PRED_LOG);
    let of_ct = build_ctable(&OF_DEFAULT, OF_PRED_MAX, OF_PRED_LOG);
    let ml_ct = build_ctable(&ML_DEFAULT, ML_PRED_MAX, ML_PRED_LOG);
    out.extend_from_slice(&encode_seq_bitstream(seqs, &ll_ct, &of_ct, &ml_ct));
    Ok(())
}

/// One channel's chosen table mode, its encode table, and the bytes describing
/// it (empty for Predefined, 1 byte for RLE, the `write_ncount` description for
/// a per-block FSE table).
struct ChannelPlan {
    mode: u8,
    ct: FseCTable,
    header: Vec<u8>,
}

/// Choose the cheapest table mode for one sequence channel by *exact* bitstream
/// cost. Candidates considered:
///
/// * **Predefined** (mode 0) — no header; only when every code fits the
///   predefined table. Always a candidate when valid, so the chosen plan is
///   never larger than the predefined encoding.
/// * **RLE** (mode 1) — 1 header byte, zero state bits; only when the channel
///   has a single distinct code.
/// * **Per-block FSE** (mode 2) — a `write_ncount` header + a custom table;
///   only when ≥ 2 distinct codes (FSE needs a real alphabet).
///
/// Repeat (mode 3) is a later refinement. Costs compare only the parts that
/// differ between modes (state bits + header bytes); the extra/low bits are
/// mode-independent and the three channels' state bits are independent, so
/// per-channel selection minimizes the whole section.
fn plan_channel(
    codes: &[usize],
    pred_dist: &[i16],
    pred_max: usize,
    pred_log: u32,
    max_log: u32,
) -> ChannelPlan {
    let max_sym = codes.iter().copied().max().unwrap_or(0);
    let mut freq = vec![0u32; max_sym + 1];
    for &c in codes {
        freq[c] += 1;
    }
    let num_present = freq.iter().filter(|&&f| f > 0).count();

    let mut candidates: Vec<(u64, ChannelPlan)> = Vec::new();

    if max_sym <= pred_max {
        let ct = build_ctable(pred_dist, pred_max, pred_log);
        let cost = ct.stream_cost_bits(codes);
        candidates.push((cost, ChannelPlan { mode: 0, ct, header: Vec::new() }));
    }
    if num_present == 1 {
        let sym = codes[0] as u8;
        // 1 header byte; the FSE states cost nothing for a single-symbol table.
        candidates.push((8, ChannelPlan { mode: 1, ct: build_rle_ctable(sym), header: vec![sym] }));
    }
    if num_present >= 2 {
        let n = codes.len();
        let table_log = optimal_table_log(max_log, n, max_sym)
            .max(min_table_log(num_present))
            .min(max_log);
        let norm = normalize_counts(&freq, n as u32, max_sym, table_log);
        let header = write_ncount(&norm, max_sym, table_log);
        let ct = build_ctable(&norm, max_sym, table_log);
        let cost = ct.stream_cost_bits(codes) + header.len() as u64 * 8;
        candidates.push((cost, ChannelPlan { mode: 2, ct, header }));
    }

    candidates
        .into_iter()
        .min_by_key(|(cost, _)| *cost)
        .expect("at least one table mode is always viable")
        .1
}

/// Encode a sequences section, choosing the cheapest table mode per channel
/// (Predefined / RLE / per-block FSE) and appending the count, the modes byte,
/// the table descriptions (LL, then OF, then ML), and the three-state
/// bitstream. Never larger than [`write_sequences_predefined`] — predefined is
/// always a candidate per channel.
pub fn write_sequences(out: &mut Vec<u8>, seqs: &[Seq]) -> Result<()> {
    write_seq_count(out, seqs.len());
    if seqs.is_empty() {
        return Ok(());
    }

    let ll_codes: Vec<usize> = seqs.iter().map(|s| ll_code(s.lit_len)).collect();
    let of_codes: Vec<usize> = seqs.iter().map(|s| of_code(s.offset_value)).collect();
    let ml_codes: Vec<usize> = seqs.iter().map(|s| ml_code(s.match_len)).collect();

    let ll = plan_channel(&ll_codes, &LL_DEFAULT, LL_PRED_MAX, LL_PRED_LOG, LL_MAX_LOG);
    let of = plan_channel(&of_codes, &OF_DEFAULT, OF_PRED_MAX, OF_PRED_LOG, OF_MAX_LOG);
    let ml = plan_channel(&ml_codes, &ML_DEFAULT, ML_PRED_MAX, ML_PRED_LOG, ML_MAX_LOG);

    out.push((ll.mode << 6) | (of.mode << 4) | (ml.mode << 2));
    out.extend_from_slice(&ll.header);
    out.extend_from_slice(&of.header);
    out.extend_from_slice(&ml.header);
    out.extend_from_slice(&encode_seq_bitstream(seqs, &ll.ct, &of.ct, &ml.ct));
    Ok(())
}

/// Write a standalone FSE table description (a `write_ncount` stream) for one
/// sequence channel into a **structured dictionary**'s entropy section. Builds a
/// table from `codes` when they form a real (≥ 2 distinct) alphabet, else falls
/// back to the channel's predefined distribution (always valid). Read back by
/// [`crate::fse::read_dtable`]; accepted by libzstd's dictionary loader. Unlike
/// [`plan_channel`], a dictionary table is always a full FSE description — never
/// RLE — so it can serve as a multi-symbol "previous" table for Repeat mode.
fn write_dict_fse_table(
    out: &mut Vec<u8>,
    codes: &[usize],
    pred_dist: &[i16],
    pred_max: usize,
    pred_log: u32,
    max_log: u32,
) {
    let max_sym = codes.iter().copied().max().unwrap_or(0);
    let mut freq = vec![0u32; max_sym + 1];
    for &c in codes {
        freq[c] += 1;
    }
    let num_present = freq.iter().filter(|&&f| f > 0).count();
    if num_present >= 2 {
        let n = codes.len();
        let table_log = optimal_table_log(max_log, n, max_sym)
            .max(min_table_log(num_present))
            .min(max_log);
        let norm = normalize_counts(&freq, n as u32, max_sym, table_log);
        out.extend_from_slice(&write_ncount(&norm, max_sym, table_log));
    } else {
        // Empty or single-symbol channel: the predefined distribution is a valid
        // multi-symbol table and a sensible warm-start.
        out.extend_from_slice(&write_ncount(pred_dist, pred_max, pred_log));
    }
}

/// Write the three structured-dictionary FSE table descriptions, in dictionary
/// order — **Offset, Match_Length, Literals_Length** (the order
/// [`crate::dict::Dictionary::parse`] and libzstd read them) — from a
/// representative set of parsed sequences.
pub(crate) fn write_dict_seq_tables(out: &mut Vec<u8>, seqs: &[Seq]) {
    let ll_codes: Vec<usize> = seqs.iter().map(|s| ll_code(s.lit_len)).collect();
    let of_codes: Vec<usize> = seqs.iter().map(|s| of_code(s.offset_value)).collect();
    let ml_codes: Vec<usize> = seqs.iter().map(|s| ml_code(s.match_len)).collect();
    write_dict_fse_table(out, &of_codes, &OF_DEFAULT, OF_PRED_MAX, OF_PRED_LOG, OF_MAX_LOG);
    write_dict_fse_table(out, &ml_codes, &ML_DEFAULT, ML_PRED_MAX, ML_PRED_LOG, ML_MAX_LOG);
    write_dict_fse_table(out, &ll_codes, &LL_DEFAULT, LL_PRED_MAX, LL_PRED_LOG, LL_MAX_LOG);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sequences::{decode, SeqTables};

    /// Deterministic RNG.
    struct Rng(u64);
    impl Rng {
        fn next(&mut self) -> u32 {
            self.0 = self
                .0
                .wrapping_mul(6364136223846793005)
                .wrapping_add(1442695040888963407);
            (self.0 >> 33) as u32
        }
        fn range(&mut self, lo: u32, hi: u32) -> u32 {
            lo + self.next() % (hi - lo + 1)
        }
    }

    /// Generate a valid (sequences, literals, expected_output) triple by
    /// *building* the output from random literal runs and back-references, so
    /// every sequence is decodable by construction. Offsets are literal
    /// (`offset_value = offset + 3`), never repeat codes.
    fn gen_case(rng: &mut Rng, steps: usize) -> (Vec<Seq>, Vec<u8>, Vec<u8>) {
        let mut seqs = Vec::new();
        let mut literals = Vec::new();
        let mut out = Vec::new();
        let mut pending_lits = 0u32;

        for _ in 0..steps {
            let ll = rng.range(0, 5);
            for _ in 0..ll {
                let b = rng.next() as u8;
                literals.push(b);
                out.push(b);
            }
            pending_lits += ll;

            if out.len() < 3 {
                continue; // can't match yet; literals accumulate
            }
            let offset = rng.range(1, out.len().min(4096) as u32);
            let match_len = rng.range(3, 12);
            let start = out.len() - offset as usize;
            for k in 0..match_len as usize {
                let b = out[start + k];
                out.push(b);
            }
            seqs.push(Seq {
                lit_len: pending_lits,
                match_len,
                offset_value: offset + 3,
            });
            pending_lits = 0;
        }

        // Trailing literals (consumed after the last sequence).
        let tail = rng.range(0, 6);
        for _ in 0..tail {
            let b = rng.next() as u8;
            literals.push(b);
            out.push(b);
        }
        (seqs, literals, out)
    }

    #[test]
    fn predefined_sequences_round_trip_through_decoder() {
        let mut rng = Rng(0xabcd_1234_5678_9999);
        for trial in 0..600 {
            let steps = (rng.next() as usize % 40) + 1;
            let (seqs, literals, expected) = gen_case(&mut rng, steps);

            let mut section = Vec::new();
            write_sequences_predefined(&mut section, &seqs).unwrap();

            let mut out = Vec::new();
            let mut tables = SeqTables::default();
            let mut rep = [1u32, 4, 8];
            decode(&section, &literals, &mut out, &mut tables, &mut rep).unwrap();
            assert_eq!(out, expected, "sequence round-trip mismatch (trial {trial})");
        }
    }

    #[test]
    fn empty_sequences_emit_literals_verbatim() {
        let literals = b"just some literals, no matches".to_vec();
        let mut section = Vec::new();
        write_sequences_predefined(&mut section, &[]).unwrap();
        assert_eq!(section, vec![0u8]);
        let mut out = Vec::new();
        let mut tables = SeqTables::default();
        let mut rep = [1u32, 4, 8];
        decode(&section, &literals, &mut out, &mut tables, &mut rep).unwrap();
        assert_eq!(out, literals);
    }

    /// The mode-selecting encoder must (a) round-trip through the decoder and
    /// (b) never produce a larger section than the predefined-only encoder —
    /// the exact-cost selection guarantees this since predefined is always a
    /// per-channel candidate.
    #[test]
    fn auto_table_sequences_round_trip_and_never_grow() {
        let mut rng = Rng(0x5151_2727_3939_4242);
        for trial in 0..600 {
            let steps = (rng.next() as usize % 40) + 1;
            let (seqs, literals, expected) = gen_case(&mut rng, steps);

            let mut section = Vec::new();
            write_sequences(&mut section, &seqs).unwrap();
            let mut pred = Vec::new();
            write_sequences_predefined(&mut pred, &seqs).unwrap();
            assert!(
                section.len() <= pred.len(),
                "auto ({}) > predefined ({}) (trial {trial})",
                section.len(),
                pred.len()
            );

            let mut out = Vec::new();
            let mut tables = SeqTables::default();
            let mut rep = [1u32, 4, 8];
            decode(&section, &literals, &mut out, &mut tables, &mut rep).unwrap();
            assert_eq!(out, expected, "auto-table round-trip mismatch (trial {trial})");
        }
    }

    /// A channel whose distribution is far from the predefined one (literal
    /// lengths almost always zero) must pick a per-block FSE table and produce a
    /// strictly smaller section, while still round-tripping. Offsets and match
    /// lengths are held constant so those two channels collapse to RLE.
    #[test]
    fn skewed_channel_picks_fse_and_shrinks() {
        let mut rng = Rng(0x9090_1234_abcd_0001);
        let mut seqs = Vec::new();
        let mut literals = Vec::new();
        let mut out = Vec::new();
        let mut pending = 0u32;
        for i in 0..1000u32 {
            let ll = if i % 37 == 0 { 3 } else { 0 }; // mostly-zero literal runs
            for _ in 0..ll {
                let b = rng.next() as u8;
                literals.push(b);
                out.push(b);
            }
            pending += ll;
            if out.len() < 4 {
                let b = rng.next() as u8;
                literals.push(b);
                out.push(b);
                pending += 1;
                continue;
            }
            let offset = 1 + (rng.next() % 3); // offset_value 4..6 -> OF code 2 (constant)
            let match_len = 4u32; // ML code 1 (constant)
            let start = out.len() - offset as usize;
            for k in 0..match_len as usize {
                let b = out[start + k];
                out.push(b);
            }
            seqs.push(Seq { lit_len: pending, match_len, offset_value: offset + 3 });
            pending = 0;
        }

        let mut section = Vec::new();
        write_sequences(&mut section, &seqs).unwrap();
        let mut pred = Vec::new();
        write_sequences_predefined(&mut pred, &seqs).unwrap();
        assert!(
            section.len() < pred.len(),
            "expected per-block tables to beat predefined: {} vs {}",
            section.len(),
            pred.len()
        );

        let mut dout = Vec::new();
        let mut tables = SeqTables::default();
        let mut rep = [1u32, 4, 8];
        decode(&section, &literals, &mut dout, &mut tables, &mut rep).unwrap();
        assert_eq!(dout, out, "skewed-channel round-trip mismatch");
    }
}
