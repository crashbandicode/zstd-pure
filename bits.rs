//! Bit readers for Zstandard's two bitstream conventions.
//!
//! * [`ReverseBitReader`] — FSE and Huffman streams are written LSB-first and
//!   read **backwards** from the end: the highest set bit of the final byte is
//!   a sentinel, and reading proceeds from just below it down through earlier
//!   bytes (MSB-first within each byte, in reverse byte order). This matches
//!   libzstd's `BIT_DStream`; values are returned with the first-read bit as
//!   the most-significant bit of the result.
//! * [`ForwardBitReader`] — a plain little-endian, LSB-first reader used for
//!   the FSE table description (`FSE_readNCount`).

use super::error::{Result, ZstdError};

/// Position (0-based) of the most-significant set bit of `x` (`x` must be > 0).
#[inline]
fn highbit32(x: u32) -> u32 {
    31 - x.leading_zeros()
}

/// Status returned by [`ReverseBitReader::reload`], mirroring libzstd's
/// `BIT_DStream_status`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReloadStatus {
    /// More bits remain and the container was refilled.
    Unfinished,
    /// The container reached the start of the buffer but bits remain.
    EndOfBuffer,
    /// All bits have been consumed exactly.
    Completed,
    /// More bits were requested than the stream held (corruption).
    Overflow,
}

/// Reverse (backward) bit reader for FSE / Huffman streams, faithful to
/// libzstd's `BIT_DStream`.
///
/// Bits are returned with the first-read bit as the MSB of the result. The
/// `bits_consumed` counter and [`ReverseBitReader::reload`] status drive the
/// two-state FSE termination, so they match libzstd exactly.
pub struct ReverseBitReader<'a> {
    src: &'a [u8],
    /// Byte index where the low byte of `container` was loaded from.
    ptr: usize,
    /// 64-bit little-endian window over the stream.
    container: u64,
    /// Number of bits already taken from the MSB side of `container` (0..=64).
    bits_consumed: u32,
}

impl<'a> ReverseBitReader<'a> {
    /// Initialize from a complete FSE/Huffman sub-stream. The final byte must be
    /// non-zero (it carries the start-of-stream sentinel bit).
    pub fn new(src: &'a [u8]) -> Result<Self> {
        let n = src.len();
        if n == 0 {
            return Err(ZstdError::CorruptTable("empty bitstream".into()));
        }
        let last = src[n - 1] as u32;
        if last == 0 {
            return Err(ZstdError::CorruptTable(
                "final bitstream byte is zero (missing sentinel)".into(),
            ));
        }
        let r = if n >= 8 {
            let ptr = n - 8;
            ReverseBitReader {
                src,
                ptr,
                container: read_u64_le(src, ptr),
                bits_consumed: 8 - highbit32(last),
            }
        } else {
            // Small stream: pack all `n` bytes little-endian into the low bytes.
            let mut container = 0u64;
            for (i, &b) in src.iter().enumerate() {
                container |= (b as u64) << (8 * i);
            }
            ReverseBitReader {
                src,
                ptr: 0,
                container,
                bits_consumed: (8 - highbit32(last)) + (8 - n as u32) * 8,
            }
        };
        Ok(r)
    }

    /// Peek the next `n` bits (1..=32) without consuming them.
    ///
    /// Uses libzstd's `BIT_lookBits` formula: the left shift is masked to
    /// `0..=63` (so a `<< 64` never occurs) and a `>> 1` splits the right shift,
    /// while `u64` overflow correctly drops the already-consumed high bits.
    #[inline]
    pub fn peek(&mut self, n: u32) -> u32 {
        if n == 0 {
            return 0;
        }
        let shifted = self.container << (self.bits_consumed & 63);
        ((shifted >> 1) >> (63 - n)) as u32
    }

    /// Consume `n` bits previously observed with [`ReverseBitReader::peek`].
    #[inline]
    pub fn consume(&mut self, n: u32) {
        self.bits_consumed += n;
    }

    /// Read and consume the next `n` bits (1..=32), first-read bit as MSB.
    #[inline]
    pub fn read(&mut self, n: u32) -> u32 {
        let v = self.peek(n);
        self.consume(n);
        v
    }

    /// Refill the container by stepping backwards through the stream.
    pub fn reload(&mut self) -> ReloadStatus {
        if self.bits_consumed > 64 {
            return ReloadStatus::Overflow;
        }
        if self.ptr >= 8 {
            self.ptr -= (self.bits_consumed >> 3) as usize;
            self.bits_consumed &= 7;
            self.container = read_u64_le(self.src, self.ptr);
            return ReloadStatus::Unfinished;
        }
        if self.ptr == 0 {
            return if self.bits_consumed < 64 {
                ReloadStatus::EndOfBuffer
            } else {
                ReloadStatus::Completed
            };
        }
        let mut nb_bytes = self.bits_consumed >> 3;
        let mut result = ReloadStatus::Unfinished;
        if (self.ptr as i64) - (nb_bytes as i64) < 0 {
            nb_bytes = self.ptr as u32;
            result = ReloadStatus::EndOfBuffer;
        }
        self.ptr -= nb_bytes as usize;
        self.bits_consumed -= nb_bytes * 8;
        self.container = read_u64_le(self.src, self.ptr);
        result
    }

    /// True once every meaningful bit has been consumed (the expected terminal
    /// state for a well-formed stream): `ptr` at the start and 64 bits taken.
    #[inline]
    pub fn finished(&self) -> bool {
        self.ptr == 0 && self.bits_consumed == 64
    }
}

/// Read 8 bytes little-endian at `i`, zero-padding if the slice is short.
#[inline]
fn read_u64_le(src: &[u8], i: usize) -> u64 {
    let mut v = 0u64;
    let end = (i + 8).min(src.len());
    for (k, &b) in src[i..end].iter().enumerate() {
        v |= (b as u64) << (8 * k);
    }
    v
}

/// Forward little-endian, LSB-first bit reader (used by `FSE_readNCount`).
pub struct ForwardBitReader<'a> {
    src: &'a [u8],
    /// Byte index of the next byte to buffer.
    idx: usize,
    acc: u64,
    live: u32,
}

impl<'a> ForwardBitReader<'a> {
    /// Create a reader over `src`, starting at byte 0, bit 0.
    pub fn new(src: &'a [u8]) -> Self {
        let mut r = ForwardBitReader {
            src,
            idx: 0,
            acc: 0,
            live: 0,
        };
        r.refill();
        r
    }

    #[inline]
    fn refill(&mut self) {
        while self.live <= 56 && self.idx < self.src.len() {
            self.acc |= (self.src[self.idx] as u64) << self.live;
            self.live += 8;
            self.idx += 1;
        }
    }

    /// Peek `n` bits (0..=32) LSB-first without consuming them.
    #[inline]
    pub fn peek(&mut self, n: u32) -> u32 {
        if n == 0 {
            return 0;
        }
        if self.live < n {
            self.refill();
        }
        let mask = if n >= 32 { u32::MAX } else { (1u32 << n) - 1 };
        (self.acc as u32) & mask
    }

    /// Consume `n` bits previously observed with [`ForwardBitReader::peek`].
    #[inline]
    pub fn consume(&mut self, n: u32) {
        self.acc >>= n;
        self.live = self.live.saturating_sub(n);
    }

    /// Read `n` bits (0..=32) LSB-first; reads past the end return zero bits.
    #[inline]
    pub fn read(&mut self, n: u32) -> u32 {
        let v = self.peek(n);
        self.consume(n);
        v
    }

    /// Number of bytes consumed so far, rounded up to include a partially
    /// consumed final byte — matching `FSE_readNCount`'s `ip - istart` accounting
    /// (which does `if (bitCount > 0) ip++`).
    pub fn bytes_consumed(&self) -> usize {
        let consumed_bits = 8 * self.idx - self.live as usize;
        consumed_bits.div_ceil(8)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reverse_reader_reads_msb_first_from_end() {
        // One byte: 0b1_010_1100. Sentinel = bit7. Data bits (read order, MSB
        // first) = 0,1,0,1,1,0,0  -> reading 3 bits then 4 bits.
        let src = [0b1010_1100u8];
        let mut r = ReverseBitReader::new(&src).unwrap();
        assert_eq!(r.read(3), 0b010);
        assert_eq!(r.read(4), 0b1100);
        assert!(r.finished());
    }

    #[test]
    fn reverse_reader_spans_multiple_bytes() {
        // bytes: [0xAB, 0x01]. last=0x01 -> sentinel bit0, zero data bits there.
        // Then previous byte 0xAB read MSB-first: 1,0,1,0,1,0,1,1.
        let src = [0xABu8, 0x01];
        let mut r = ReverseBitReader::new(&src).unwrap();
        assert_eq!(r.read(8), 0xAB);
        assert!(r.finished());
    }

    #[test]
    fn forward_reader_reads_lsb_first() {
        // 0b1011_0010 -> read 3 -> 0b010 (low bits), read 5 -> 0b10110.
        let src = [0b1011_0010u8];
        let mut r = ForwardBitReader::new(&src);
        assert_eq!(r.read(3), 0b010);
        assert_eq!(r.read(5), 0b10110);
    }

    #[test]
    fn rejects_zero_final_byte() {
        assert!(ReverseBitReader::new(&[0u8]).is_err());
        assert!(ReverseBitReader::new(&[]).is_err());
    }
}
