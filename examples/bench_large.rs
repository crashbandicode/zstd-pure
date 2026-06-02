//! Large-input ratio + time benchmark for the optimal levels — the regime the
//! small `ratio` corpus doesn't cover, where the hash chain's depth bound (128)
//! can actually bind: a hash with more candidates than that may hide the best
//! (longest) match further back than the chain walks, which the hybrid's binary
//! tree resolves. `revisions` is the case that motivated merging the tree (the
//! chain-opt left ~16 % on the table there). Run with
//! `cargo run --release --example bench_large` (prints size + indicative time
//! vs libzstd; minutes on the medium-match profiles at L19).

use std::time::Instant;
use zstd_pure::compress;

/// Deterministic LCG (no wall-clock / RNG syscalls, so both runs see identical
/// inputs).
struct Rng(u64);
impl Rng {
    fn n(&mut self) -> u64 {
        self.0 = self.0.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
        self.0 >> 1
    }
    fn byte(&mut self) -> u8 {
        (self.n() >> 24) as u8
    }
    fn upto(&mut self, m: usize) -> usize {
        (self.n() as usize) % m
    }
}

/// ~150 near-duplicate ~12 KB documents (a shared base with ~24 single-byte edits
/// each), ~1.8 MB. Every position's hash has ~150 candidates — more than the
/// chain's depth — and the longest match for a region may be in a revision
/// further back than the chain walks. The depth-stress case.
fn revisions() -> Vec<u8> {
    let mut r = Rng(0x1111_2222_3333_4444);
    let mut base = Vec::new();
    while base.len() < 12_000 {
        base.extend_from_slice(
            format!("k{:04}: {} -- note {}\n", base.len() % 1000, r.upto(1_000_000), r.upto(64)).as_bytes(),
        );
    }
    let mut out = Vec::new();
    for _ in 0..150 {
        let mut doc = base.clone();
        for _ in 0..24 {
            let p = r.upto(doc.len());
            doc[p] = r.byte();
        }
        out.extend_from_slice(&doc);
    }
    out
}

/// ~2 MB of log lines from a small template + field pool — long-range template
/// repeats spread across the whole file.
fn logs() -> Vec<u8> {
    let mut r = Rng(0x9e37_79b9_7f4a_7c15);
    let lvl = ["INFO ", "WARN ", "ERROR", "DEBUG"];
    let op = ["connect", "timeout", "retry", "flush", "commit", "rollback", "evict", "accept"];
    let mut out = Vec::new();
    while out.len() < 2_000_000 {
        out.extend_from_slice(
            format!(
                "{} t={} id={} op={} dur={}ms\n",
                lvl[r.upto(lvl.len())],
                r.upto(10_000_000),
                r.upto(100_000),
                op[r.upto(op.len())],
                r.upto(5000),
            )
            .as_bytes(),
        );
    }
    out
}

/// ~2 MB of fixed-size binary records (a shared 8-byte header + a mostly-palette
/// 24-byte tail), the structure recurring throughout — a stand-in for the kind of
/// repeated binary sub-structures real asset/mesh streams carry.
fn binstruct() -> Vec<u8> {
    let mut r = Rng(0x2545_f491_dead_beef);
    let header = [0xDE, 0xAD, 0xBE, 0xEF, 0x10, 0x00, 0x00, 0x00];
    let mut out = Vec::new();
    while out.len() < 2_000_000 {
        out.extend_from_slice(&header);
        for k in 0..24u8 {
            out.push(if r.upto(4) == 0 { r.byte() } else { k.wrapping_mul(7) });
        }
    }
    out
}

fn main() {
    let profiles: Vec<(&str, Vec<u8>)> = vec![
        ("revisions", revisions()),
        ("logs", logs()),
        ("binstruct", binstruct()),
    ];
    for level in [13i32, 19] {
        println!("\n=== level {level} ===");
        println!("{:<11} {:>10} {:>11} {:>8} {:>11} {:>9}", "profile", "raw", "zstd_pure", "ms", "libzstd", "ratio");
        for (name, data) in &profiles {
            let t = Instant::now();
            let ours = compress(data, level, false, true);
            let ms = t.elapsed().as_millis();
            let lz = zstd::bulk::compress(data, level).unwrap().len();
            assert_eq!(
                zstd::bulk::decompress(&ours, data.len() + 64).unwrap(),
                *data,
                "{name}: libzstd round-trip failed"
            );
            println!(
                "{:<11} {:>10} {:>11} {:>8} {:>11} {:>8.3}x",
                name,
                data.len(),
                ours.len(),
                ms,
                lz,
                ours.len() as f64 / lz as f64
            );
        }
    }
}
