//! Forward LSB bit accumulator mirroring libzstd's `BIT_CStream`, shared by the
//! Huff0 and FSE encoders.
//!
//! Pairs with the decoder's reverse [`ReverseBitReader`](crate::bits):
//! `add(v, nb)` here ↔ `read(nb) == v` there, provided fields are emitted in
//! **reverse** order. A final `1` sentinel bit (added by [`BitWriter::finish`])
//! marks the start of the stream for the backward reader.

#[allow(unused_imports)]
use crate::alloc_prelude::*;
/// Forward LSB bit writer. Whole bytes are flushed from the low end of a 64-bit
/// accumulator only when the next field would not fit it (libzstd's `BIT_addBits`
/// with a lazy `BIT_flushBits`). Because the flushed bytes are always the low
/// bytes of the accumulated bit string in order, *when* the flush happens doesn't
/// change the output — deferring it just batches the `extend_from_slice` calls
/// (≈5 Huffman symbols per flush instead of one), so only the order and width of
/// `add` calls matters for correctness.
#[derive(Default)]
pub struct BitWriter {
    acc: u64,
    nbits: u32,
    out: Vec<u8>,
}

impl BitWriter {
    pub fn with_capacity(cap: usize) -> Self {
        BitWriter {
            acc: 0,
            nbits: 0,
            out: Vec::with_capacity(cap),
        }
    }

    /// Append the low `nb` bits of `value` (`nb` ≤ 32). High bits of `value`
    /// beyond `nb` are masked off (FSE pushes state values whose high bits must
    /// be discarded).
    #[inline]
    pub fn add(&mut self, value: u32, nb: u32) {
        // Flush whole bytes only when the next field wouldn't fit the 64-bit
        // accumulator. The `>= 64` (not `> 64`) keeps `nbits < 64` after every
        // add, so a flush is at most 7 bytes — `acc >>= nbytes*8` never shifts by
        // 64 (which would overflow). After a flush `nbits < 8`, so the `<< nbits`
        // below can't overflow either. The common case (the field fits) skips the
        // flush entirely, batching the `extend_from_slice`s.
        if self.nbits + nb >= 64 {
            let nbytes = (self.nbits >> 3) as usize;
            self.out
                .extend_from_slice(&self.acc.to_le_bytes()[..nbytes]);
            self.acc >>= nbytes * 8;
            self.nbits -= (nbytes as u32) * 8;
        }
        let masked = if nb >= 32 {
            value as u64
        } else {
            (value as u64) & ((1u64 << nb) - 1)
        };
        self.acc |= masked << self.nbits;
        self.nbits += nb;
    }

    /// Cap the stream with the sentinel `1` bit, drain all remaining whole bytes,
    /// flush the final partial byte, and return the bytes.
    pub fn finish(mut self) -> Vec<u8> {
        self.add(1, 1);
        // Deferred `add` can leave up to 64 bits buffered — drain every whole byte
        // (from the low end, in order) before the final partial byte.
        let nbytes = (self.nbits >> 3) as usize;
        if nbytes > 0 {
            self.out
                .extend_from_slice(&self.acc.to_le_bytes()[..nbytes]);
            self.acc >>= nbytes * 8;
            self.nbits -= (nbytes as u32) * 8;
        }
        if self.nbits > 0 {
            self.out.push(self.acc as u8);
        }
        self.out
    }
}
