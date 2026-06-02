//! Compression-ratio comparison: our `compress` vs libzstd (the `zstd` crate)
//! across a few input profiles and levels. Criterion measures time;
//! this measures size. Run with `cargo run --release --example ratio`.
//!
//! Prints, per level, the compressed size of each profile for both encoders and
//! our size as a multiple of libzstd's (`< 1.00x` means we are smaller).

use zstd_pure::compress;

fn profiles() -> Vec<(&'static str, Vec<u8>)> {
    // Highly redundant (>128 KiB, multi-block).
    let redundant: Vec<u8> = (0..40_000u32).flat_map(|i| (i % 13).to_le_bytes()).collect();
    // Pseudo-random byte records with a fixed header (low redundancy).
    let mut records = b"FRES____".to_vec();
    for i in 0..12_000u32 {
        records.extend_from_slice(&(i.wrapping_mul(2654435761) % 251).to_le_bytes());
    }
    // Natural-language-ish repetition.
    let text = "the quick brown fox jumps over the lazy dog. "
        .repeat(900)
        .into_bytes();
    // Structured JSON records (>128 KiB).
    let json: Vec<u8> = (0..4_000u32)
        .flat_map(|i| {
            format!("{{\"id\":{i},\"type\":\"npc_{}\",\"hp\":{}}}\n", i % 53, (i * 17) % 999)
                .into_bytes()
        })
        .collect();
    // Three copies of a 90 KiB incompressible chunk: only cross-block matching
    // (offset ~90 KiB) can compress copies 2 and 3.
    let chunk: Vec<u8> = (0..90_000u32).map(|i| (i.wrapping_mul(2654435761) >> 13) as u8).collect();
    // Two distinct regimes inside one 128 KiB block: ~64 KiB of repetitive text
    // then ~64 KiB of structured JSON records. Their literal alphabets and
    // sequence (LL/ML/OF) distributions differ, so one block's compromise
    // entropy tables cost more than two blocks each fit to a half — the
    // block-splitter case.
    let mut mixed = "the quick brown fox jumps over the lazy dog. ".repeat(1500).into_bytes();
    mixed.truncate(64 * 1024);
    let mut jsonish: Vec<u8> = (0..3_000u32)
        .flat_map(|i| format!("{{\"id\":{i},\"type\":\"npc_{}\",\"hp\":{}}}\n", i % 53, (i * 17) % 999).into_bytes())
        .collect();
    jsonish.truncate(64 * 1024);
    mixed.extend_from_slice(&jsonish);
    vec![
        ("redundant", redundant),
        ("records", records),
        ("text", text),
        ("json", json),
        ("3x90k-chunk", chunk.repeat(3)),
        ("mixed", mixed),
    ]
}

fn main() {
    let profiles = profiles();
    for &level in &[1i32, 3, 6, 9, 13, 19] {
        println!("\n=== level {level} ===");
        println!("{:<13} {:>10} {:>10} {:>10} {:>9}", "profile", "raw", "zstd_pure", "libzstd", "ratio");
        for (name, data) in &profiles {
            let ours = compress(data, level, false, true).len();
            let lz = zstd::bulk::compress(data, level).unwrap().len();
            // Sanity: our frame must round-trip through libzstd.
            let back = zstd::bulk::decompress(&compress(data, level, false, true), data.len() + 64).unwrap();
            assert_eq!(&back, data, "{name}: our L{level} frame failed libzstd round-trip");
            println!(
                "{:<13} {:>10} {:>10} {:>10} {:>8.2}x",
                name,
                data.len(),
                ours,
                lz,
                ours as f64 / lz as f64
            );
        }
    }
}
