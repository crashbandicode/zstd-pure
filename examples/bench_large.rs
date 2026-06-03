//! Large-input ratio + time benchmark for the optimal levels — the regime the
//! small `ratio` corpus doesn't cover, where the hash chain's depth bound (128)
//! can actually bind: a hash with more candidates than that may hide the best
//! (longest) match further back than the chain walks, which the hybrid's binary
//! tree resolves. `revisions` is the case that motivated merging the tree (the
//! chain-opt left ~16 % on the table there). Run with
//! `cargo run --release --example bench_large` (prints size + indicative time
//! vs libzstd; minutes on the medium-match profiles at L19).

use std::time::Instant;
use zstd_pure::{compress, compress_long, compress_parallel};

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

/// `target` bytes of log lines from a small template + field pool — long-range
/// template repeats spread across the whole file. `seed` lets callers generate
/// distinct streams of the same shape.
fn logs_sized(target: usize, seed: u64) -> Vec<u8> {
    let mut r = Rng(seed);
    let lvl = ["INFO ", "WARN ", "ERROR", "DEBUG"];
    let op = ["connect", "timeout", "retry", "flush", "commit", "rollback", "evict", "accept"];
    let mut out = Vec::with_capacity(target + 64);
    while out.len() < target {
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

/// ~2 MB of log lines (the small-corpus profile).
fn logs() -> Vec<u8> {
    logs_sized(2_000_000, 0x9e37_79b9_7f4a_7c15)
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

/// A distinctive ~1.5 MB block, ~10 MB of unrelated filler, then the same block
/// again — its second copy sits > 8 MiB back, beyond the 8 MiB window plain
/// `compress` is capped to. Only `compress_long` (whose LDM index + larger
/// advertised window reach that far) can link the two copies; plain compression
/// must re-compress the second from scratch. The block is templated text, so all
/// three encoders compress each copy's *internal* redundancy — the LDM win is
/// purely the cross-gap duplicate (the gap libzstd closes with its larger native
/// window and our plain `compress` cannot).
fn far_dup() -> Vec<u8> {
    let mut r = Rng(0x0bad_f00d_1234_5678);
    let mut block = Vec::new();
    while block.len() < 1_500_000 {
        block.extend_from_slice(
            format!("rec={:08} val={} tag={}\n", block.len(), r.upto(1_000_000), r.upto(256)).as_bytes(),
        );
    }
    let mut filler = Vec::new();
    while filler.len() < 10_000_000 {
        filler.extend_from_slice(
            format!("fill {} {} {}\n", r.upto(1 << 30), r.upto(1 << 30), r.upto(1 << 30)).as_bytes(),
        );
    }
    let mut out = Vec::with_capacity(block.len() * 2 + filler.len());
    out.extend_from_slice(&block);
    out.extend_from_slice(&filler);
    out.extend_from_slice(&block);
    out
}

/// Demonstrate `compress_parallel`'s wall-clock speedup vs serial `compress` on a
/// large input (HANDOFF §4.3). Wall-clock is load-sensitive — the *relative*
/// shape (does it scale with jobs, and what does the seam cost the ratio?) is the
/// point, not absolute numbers. Each parallel output is round-tripped through
/// libzstd to prove the multi-frame stream is valid.
fn parallel_speedup() {
    let data = logs_sized(24 << 20, 0x5151_5151_2323_2323); // 24 MiB, distinct stream
    let level = 12;
    let cores = std::thread::available_parallelism().map(|n| n.get()).unwrap_or(4);
    println!(
        "\n=== compress_parallel vs serial (L{level}, {} MiB, available_parallelism={cores}) ===",
        data.len() >> 20
    );
    println!("{:<8} {:>8} {:>11} {:>9} {:>10}", "n_jobs", "ms", "bytes", "speedup", "seam cost");

    let t = Instant::now();
    let serial = compress(&data, level, false, true);
    let serial_ms = t.elapsed().as_millis().max(1);
    println!("{:<8} {:>8} {:>11} {:>9} {:>10}", "serial", serial_ms, serial.len(), "1.00x", "-");

    for &n_jobs in &[2usize, 4, 8] {
        let t = Instant::now();
        let par = compress_parallel(&data, level, n_jobs, false, true);
        let ms = t.elapsed().as_millis().max(1);
        // The multi-frame parallel output must decode back through libzstd.
        assert_eq!(
            zstd::stream::decode_all(&par[..]).unwrap(),
            data,
            "parallel n_jobs={n_jobs} round-trip failed"
        );
        println!(
            "{:<8} {:>8} {:>11} {:>8.2}x {:>9.2}%",
            n_jobs,
            ms,
            par.len(),
            serial_ms as f64 / ms as f64,
            (par.len() as f64 / serial.len() as f64 - 1.0) * 100.0
        );
    }
}

fn main() {
    parallel_speedup();

    let profiles: Vec<(&str, Vec<u8>)> = vec![
        ("revisions", revisions()),
        ("logs", logs()),
        ("binstruct", binstruct()),
        ("far_dup", far_dup()),
    ];
    for level in [13i32, 19] {
        println!("\n=== level {level} ===");
        println!(
            "{:<11} {:>10} {:>11} {:>11} {:>8} {:>11} {:>9}",
            "profile", "raw", "pure", "pure+ldm", "ms", "libzstd", "ldm/lz"
        );
        for (name, data) in &profiles {
            let ours = compress(data, level, false, true);
            let t = Instant::now();
            let long = compress_long(data, level, false, true);
            let ms = t.elapsed().as_millis();
            let lz = zstd::bulk::compress(data, level).unwrap().len();
            // Both our encodings must decode through libzstd (proving the offsets,
            // including LDM's large ones, stay within the advertised window).
            assert_eq!(
                zstd::bulk::decompress(&ours, data.len() + 64).unwrap(),
                *data,
                "{name}: compress round-trip failed"
            );
            assert_eq!(
                zstd::bulk::decompress(&long, data.len() + 64).unwrap(),
                *data,
                "{name}: compress_long round-trip failed"
            );
            println!(
                "{:<11} {:>10} {:>11} {:>11} {:>8} {:>11} {:>8.3}x",
                name,
                data.len(),
                ours.len(),
                long.len(),
                ms,
                lz,
                long.len() as f64 / lz as f64
            );
        }
    }
}
