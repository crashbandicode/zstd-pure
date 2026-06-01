//! XXH64 (one-shot), used for Zstandard's optional content checksum.
//!
//! The frame checksum is the low 32 bits of `XXH64(decoded_content, seed=0)`.
//! Reference: Yann Collet's xxHash specification (public domain / BSD-2).

#[allow(unused_imports)]
use crate::alloc_prelude::*;
const PRIME64_1: u64 = 0x9E37_79B1_85EB_CA87;
const PRIME64_2: u64 = 0xC2B2_AE3D_27D4_EB4F;
const PRIME64_3: u64 = 0x1656_67B1_9E37_79F9;
const PRIME64_4: u64 = 0x85EB_CA77_C2B2_AE63;
const PRIME64_5: u64 = 0x27D4_EB2F_1656_67C5;

#[inline]
fn round(acc: u64, input: u64) -> u64 {
    acc.wrapping_add(input.wrapping_mul(PRIME64_2))
        .rotate_left(31)
        .wrapping_mul(PRIME64_1)
}

#[inline]
fn merge_round(mut acc: u64, val: u64) -> u64 {
    acc ^= round(0, val);
    acc.wrapping_mul(PRIME64_1).wrapping_add(PRIME64_4)
}

/// Compute `XXH64(data, seed)`.
pub fn xxh64(data: &[u8], seed: u64) -> u64 {
    let len = data.len() as u64;
    let mut idx = 0usize;
    let mut h64: u64;

    if data.len() >= 32 {
        let mut v1 = seed.wrapping_add(PRIME64_1).wrapping_add(PRIME64_2);
        let mut v2 = seed.wrapping_add(PRIME64_2);
        let mut v3 = seed;
        let mut v4 = seed.wrapping_sub(PRIME64_1);
        while idx + 32 <= data.len() {
            v1 = round(v1, read_u64(data, idx));
            v2 = round(v2, read_u64(data, idx + 8));
            v3 = round(v3, read_u64(data, idx + 16));
            v4 = round(v4, read_u64(data, idx + 24));
            idx += 32;
        }
        h64 = v1
            .rotate_left(1)
            .wrapping_add(v2.rotate_left(7))
            .wrapping_add(v3.rotate_left(12))
            .wrapping_add(v4.rotate_left(18));
        h64 = merge_round(h64, v1);
        h64 = merge_round(h64, v2);
        h64 = merge_round(h64, v3);
        h64 = merge_round(h64, v4);
    } else {
        h64 = seed.wrapping_add(PRIME64_5);
    }

    h64 = h64.wrapping_add(len);

    while idx + 8 <= data.len() {
        let k1 = round(0, read_u64(data, idx));
        h64 ^= k1;
        h64 = h64.rotate_left(27).wrapping_mul(PRIME64_1).wrapping_add(PRIME64_4);
        idx += 8;
    }
    if idx + 4 <= data.len() {
        h64 ^= (read_u32(data, idx) as u64).wrapping_mul(PRIME64_1);
        h64 = h64.rotate_left(23).wrapping_mul(PRIME64_2).wrapping_add(PRIME64_3);
        idx += 4;
    }
    while idx < data.len() {
        h64 ^= (data[idx] as u64).wrapping_mul(PRIME64_5);
        h64 = h64.rotate_left(11).wrapping_mul(PRIME64_1);
        idx += 1;
    }

    avalanche(h64)
}

#[inline]
fn read_u64(d: &[u8], i: usize) -> u64 {
    u64::from_le_bytes(d[i..i + 8].try_into().unwrap())
}

#[inline]
fn read_u32(d: &[u8], i: usize) -> u32 {
    u32::from_le_bytes(d[i..i + 4].try_into().unwrap())
}

/// Final XXH64 avalanche (shared by the one-shot and streaming paths).
#[inline]
fn avalanche(mut h64: u64) -> u64 {
    h64 ^= h64 >> 33;
    h64 = h64.wrapping_mul(PRIME64_2);
    h64 ^= h64 >> 29;
    h64 = h64.wrapping_mul(PRIME64_3);
    h64 ^= h64 >> 32;
    h64
}

/// Streaming XXH64 state, for hashing content that is produced (and evicted)
/// incrementally — used by the bounded-memory streaming decoder, which can't
/// retain the whole output to hash at the end. Matches the one-shot [`xxh64`]
/// (and Yann Collet's reference) byte-for-byte regardless of chunk boundaries.
#[derive(Debug, Clone)]
pub struct Xxh64 {
    total_len: u64,
    v: [u64; 4],
    mem: [u8; 32],
    memsize: usize,
    seed: u64,
}

impl Xxh64 {
    /// Start a streaming hash with the given seed.
    pub fn new(seed: u64) -> Self {
        Xxh64 {
            total_len: 0,
            v: [
                seed.wrapping_add(PRIME64_1).wrapping_add(PRIME64_2),
                seed.wrapping_add(PRIME64_2),
                seed,
                seed.wrapping_sub(PRIME64_1),
            ],
            mem: [0u8; 32],
            memsize: 0,
            seed,
        }
    }

    /// Absorb more input.
    pub fn update(&mut self, mut input: &[u8]) {
        self.total_len = self.total_len.wrapping_add(input.len() as u64);

        // Not enough to complete a 32-byte block yet: just buffer it.
        if self.memsize + input.len() < 32 {
            self.mem[self.memsize..self.memsize + input.len()].copy_from_slice(input);
            self.memsize += input.len();
            return;
        }

        // Finish the partial buffer to a full 32-byte stripe.
        if self.memsize > 0 {
            let need = 32 - self.memsize;
            self.mem[self.memsize..32].copy_from_slice(&input[..need]);
            for k in 0..4 {
                self.v[k] = round(self.v[k], read_u64(&self.mem, k * 8));
            }
            input = &input[need..];
            self.memsize = 0;
        }

        // Process whole 32-byte stripes straight from the input.
        while input.len() >= 32 {
            for k in 0..4 {
                self.v[k] = round(self.v[k], read_u64(input, k * 8));
            }
            input = &input[32..];
        }

        // Stash the remainder.
        if !input.is_empty() {
            self.mem[..input.len()].copy_from_slice(input);
            self.memsize = input.len();
        }
    }

    /// Finalize and return the 64-bit hash. Non-consuming (the state can keep
    /// being updated afterwards).
    pub fn digest(&self) -> u64 {
        let mut h64 = if self.total_len >= 32 {
            let mut h = self.v[0]
                .rotate_left(1)
                .wrapping_add(self.v[1].rotate_left(7))
                .wrapping_add(self.v[2].rotate_left(12))
                .wrapping_add(self.v[3].rotate_left(18));
            h = merge_round(h, self.v[0]);
            h = merge_round(h, self.v[1]);
            h = merge_round(h, self.v[2]);
            h = merge_round(h, self.v[3]);
            h
        } else {
            self.seed.wrapping_add(PRIME64_5)
        };
        h64 = h64.wrapping_add(self.total_len);

        let mem = &self.mem[..self.memsize];
        let mut idx = 0usize;
        while idx + 8 <= mem.len() {
            let k1 = round(0, read_u64(mem, idx));
            h64 ^= k1;
            h64 = h64
                .rotate_left(27)
                .wrapping_mul(PRIME64_1)
                .wrapping_add(PRIME64_4);
            idx += 8;
        }
        if idx + 4 <= mem.len() {
            h64 ^= (read_u32(mem, idx) as u64).wrapping_mul(PRIME64_1);
            h64 = h64
                .rotate_left(23)
                .wrapping_mul(PRIME64_2)
                .wrapping_add(PRIME64_3);
            idx += 4;
        }
        while idx < mem.len() {
            h64 ^= (mem[idx] as u64).wrapping_mul(PRIME64_5);
            h64 = h64.rotate_left(11).wrapping_mul(PRIME64_1);
            idx += 1;
        }
        avalanche(h64)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn known_vectors() {
        // Canonical xxHash test vectors (seed 0).
        assert_eq!(xxh64(b"", 0), 0xEF46_DB37_51D8_E999);
        // "Nobody inspects the spammish repetition" with seed 0.
        assert_eq!(
            xxh64(b"Nobody inspects the spammish repetition", 0),
            0xFBCE_A83C_8A37_8BF1
        );
    }

    #[test]
    fn handles_all_tail_lengths() {
        // Just ensure no panics across lengths spanning the 32/8/4/1 branches.
        let data: Vec<u8> = (0..100u8).collect();
        for n in 0..=100 {
            let _ = xxh64(&data[..n], 0);
        }
    }

    #[test]
    fn streaming_matches_oneshot_regardless_of_chunking() {
        // A buffer spanning many 32-byte stripes plus a partial tail.
        let data: Vec<u8> = (0..1000u32).map(|i| (i.wrapping_mul(2654435761) >> 13) as u8).collect();
        let one = xxh64(&data, 0);
        // Feed it in a variety of chunk sizes that cross stripe boundaries.
        for &chunk in &[1usize, 3, 7, 8, 13, 31, 32, 33, 64, 257] {
            let mut h = Xxh64::new(0);
            for part in data.chunks(chunk) {
                h.update(part);
            }
            assert_eq!(h.digest(), one, "chunk size {chunk}");
        }
        // Known vectors via the streaming path too.
        let mut empty = Xxh64::new(0);
        empty.update(b"");
        assert_eq!(empty.digest(), 0xEF46_DB37_51D8_E999);
    }
}
