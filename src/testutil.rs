//! Test-only deterministic generators shared across the crate's unit tests.
//!
//! Hoisted here so the same reproducible PRNG isn't re-declared in every
//! module's `#[cfg(test)] mod tests`. `#[cfg(test)]` keeps it out of the shipped
//! crate entirely (so it never touches the `no_std` build or the public API).

#[allow(unused_imports)]
use crate::alloc_prelude::*;

/// A SplitMix64-derived stream of incompressible bytes, deterministic for a
/// given `seed`. Used where a test needs "random" data with no exploitable
/// matches except ones it plants itself (e.g. far-offset / LDM duplicates).
pub fn prng(n: usize, mut s: u64) -> Vec<u8> {
    (0..n)
        .map(|_| {
            s = s.wrapping_add(0x9E37_79B9_7F4A_7C15);
            let mut z = s;
            z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
            z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
            (z ^ (z >> 31)) as u8
        })
        .collect()
}
