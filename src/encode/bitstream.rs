//! Forward LSB bit accumulator mirroring libzstd's `BIT_CStream`, shared by the
//! Huff0 and FSE encoders.
//!
//! Pairs with the decoder's reverse [`ReverseBitReader`](crate::bits):
//! `add(v, nb)` here ↔ `read(nb) == v` there, provided fields are emitted in
//! **reverse** order. A final `1` sentinel bit (added by [`BitWriter::finish`])
//! marks the start of the stream for the backward reader.

#[allow(unused_imports)]
use crate::alloc_prelude::*;
/// Forward, byte-eager LSB bit writer. Eager flushing produces the exact same
/// byte sequence as libzstd's deferred `BIT_flushBits`, so the intermediate
/// flush points in the ported encoders can be dropped — only the order and
/// width of `add` calls matters.
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
        let masked = if nb >= 32 {
            value as u64
        } else {
            (value as u64) & ((1u64 << nb) - 1)
        };
        self.acc |= masked << self.nbits;
        self.nbits += nb;
        // Flush all whole bytes now ready in one extend (≤ 4 bytes since `nb` ≤ 32
        // and `nbits` was < 8), rather than pushing them one at a time.
        let nbytes = (self.nbits >> 3) as usize;
        if nbytes > 0 {
            self.out
                .extend_from_slice(&self.acc.to_le_bytes()[..nbytes]);
            self.acc >>= nbytes * 8;
            self.nbits -= (nbytes as u32) * 8;
        }
    }

    /// Cap the stream with the sentinel `1` bit, flush the final partial byte,
    /// and return the bytes.
    pub fn finish(mut self) -> Vec<u8> {
        self.add(1, 1);
        if self.nbits > 0 {
            self.out.push(self.acc as u8);
        }
        self.out
    }
}
