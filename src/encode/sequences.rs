//! Sequence-section **encoding** — RFC 8478 §3.1.1.3.2, the inverse of
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

use super::super::error::Result;
use super::super::sequences::{LL_BASE, LL_BITS, LL_DEFAULT, ML_BASE, ML_BITS, ML_DEFAULT, OF_DEFAULT};
use super::bitstream::BitWriter;
use super::fse::build_ctable;

// Predefined table parameters (max symbol, accuracy log) — must match the
// decoder's `resolve_table` calls.
const LL_PRED_MAX: usize = 35;
const LL_PRED_LOG: u32 = 6;
const OF_PRED_MAX: usize = 28;
const OF_PRED_LOG: u32 = 5;
const ML_PRED_MAX: usize = 52;
const ML_PRED_LOG: u32 = 6;

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
#[inline]
fn ll_code(lit_len: u32) -> usize {
    (0..LL_BASE.len()).rev().find(|&c| LL_BASE[c] <= lit_len).unwrap()
}

/// Match-length code for `match_len` (`match_len >= 3`).
#[inline]
fn ml_code(match_len: u32) -> usize {
    (0..ML_BASE.len()).rev().find(|&c| ML_BASE[c] <= match_len).unwrap()
}

/// Offset code = `floor(log2(offset_value))` (`offset_value >= 1`).
#[inline]
fn of_code(offset_value: u32) -> usize {
    highbit32(offset_value) as usize
}

/// Write the `Number_of_Sequences` header (RFC 8478 §3.1.1.3.2.1).
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

/// Encode a sequences section using the **predefined** FSE tables and append it
/// to `out`. An empty sequence list writes the single-byte `nb_seq = 0` form
/// (the decoder then emits the literals verbatim).
pub fn write_sequences_predefined(out: &mut Vec<u8>, seqs: &[Seq]) -> Result<()> {
    write_seq_count(out, seqs.len());
    if seqs.is_empty() {
        return Ok(());
    }

    // Compression modes byte: LL/OF/ML all Predefined (0).
    out.push(0);

    let ll_ct = build_ctable(&LL_DEFAULT, LL_PRED_MAX, LL_PRED_LOG);
    let of_ct = build_ctable(&OF_DEFAULT, OF_PRED_MAX, OF_PRED_LOG);
    let ml_ct = build_ctable(&ML_DEFAULT, ML_PRED_MAX, ML_PRED_LOG);

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

    out.extend_from_slice(&bw.finish());
    Ok(())
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
}
