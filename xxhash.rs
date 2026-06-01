//! XXH64 (one-shot), used for Zstandard's optional content checksum.
//!
//! The frame checksum is the low 32 bits of `XXH64(decoded_content, seed=0)`.
//! Reference: Yann Collet's xxHash specification (public domain / BSD-2).

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

    h64 ^= h64 >> 33;
    h64 = h64.wrapping_mul(PRIME64_2);
    h64 ^= h64 >> 29;
    h64 = h64.wrapping_mul(PRIME64_3);
    h64 ^= h64 >> 32;
    h64
}

#[inline]
fn read_u64(d: &[u8], i: usize) -> u64 {
    u64::from_le_bytes(d[i..i + 8].try_into().unwrap())
}

#[inline]
fn read_u32(d: &[u8], i: usize) -> u32 {
    u32::from_le_bytes(d[i..i + 4].try_into().unwrap())
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
}
