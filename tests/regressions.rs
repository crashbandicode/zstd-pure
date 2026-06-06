//! Persistent fuzz-regression corpus walker.
//!
//! When a fuzz target finds a crash, hang, OOM, or differential mismatch, the
//! minimized reproducer is committed under `tests/regressions/<category>/` (see
//! that directory's README). This walker replays every committed `*.bin` on each
//! `cargo test`, so a fixed bug can never silently return. Each category has its
//! own contract:
//!
//! - `decode/`      — one-shot + streaming decode must stay bounded, never panic;
//! - `decode_diff/` — whatever libzstd decodes, our decoder must reproduce exactly;
//! - `seekable/`    — parse + random-access + parallel decode must never panic.
//!
//! An empty category trivially passes — the harness exists so future finds have a
//! home. It ships seeded (see the ignored `generate_seed_corpus`) so it is
//! exercised from day one.

use std::fs;
use std::io::Read;
use std::path::{Path, PathBuf};

use zstd_pure::{
    decompress_capped, decompress_seekable_frame, decompress_seekable_parallel_capped, SeekTable,
    StreamingDecoder,
};

/// Output ceiling for every replay — a corrupt case must never allocate without
/// bound, so a "hang" can't masquerade as a slow OOM.
const CAP: usize = 1 << 20;

fn dir(category: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/regressions")
        .join(category)
}

/// Every committed `*.bin` case under a category, sorted; empty if none yet.
fn cases(category: &str) -> Vec<(String, Vec<u8>)> {
    let Ok(entries) = fs::read_dir(dir(category)) else {
        return Vec::new();
    };
    let mut paths: Vec<PathBuf> = entries
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.extension().map(|x| x == "bin").unwrap_or(false))
        .collect();
    paths.sort();
    paths
        .into_iter()
        .filter_map(|p| {
            let name = p.file_name()?.to_string_lossy().into_owned();
            fs::read(&p).ok().map(|b| (name, b))
        })
        .collect()
}

#[test]
fn decode_regressions_never_panic_and_stay_bounded() {
    let cases = cases("decode");
    eprintln!("decode/: {} regression case(s)", cases.len());
    for (name, bytes) in &cases {
        if let Ok(out) = decompress_capped(bytes, CAP) {
            assert!(out.len() <= CAP, "[decode/{name}] output exceeded the cap");
        }
        if let Ok(mut dec) = StreamingDecoder::new(bytes) {
            let mut sink = Vec::new();
            let _ = dec.read_to_end(&mut sink);
        }
    }
}

#[test]
fn decode_diff_regressions_agree_with_libzstd() {
    let cases = cases("decode_diff");
    eprintln!("decode_diff/: {} regression case(s)", cases.len());
    for (name, bytes) in &cases {
        // libzstd is the oracle: whatever it accepts, we must reproduce exactly.
        if let Ok(expected) = zstd::stream::decode_all(bytes.as_slice()) {
            let ours = decompress_capped(bytes, CAP).unwrap_or_else(|e| {
                panic!("[decode_diff/{name}] libzstd accepted but we errored: {e}")
            });
            assert_eq!(ours, expected, "[decode_diff/{name}] differential mismatch");
        }
    }
}

#[test]
fn seekable_regressions_never_panic() {
    let cases = cases("seekable");
    eprintln!("seekable/: {} regression case(s)", cases.len());
    for (_name, bytes) in &cases {
        if let Ok(table) = SeekTable::parse(bytes) {
            for i in 0..table.num_frames().min(64) {
                let _ = decompress_seekable_frame(bytes, &table, i);
            }
            let _ = decompress_seekable_parallel_capped(bytes, &table, 4, CAP);
            let _ = decompress_seekable_parallel_capped(bytes, &table, 0, CAP);
        }
    }
}

/// Regenerate the seed corpus. Run explicitly:
/// `cargo test --test regressions generate_seed_corpus -- --ignored`.
/// This documents the exact provenance of every committed seed; real fuzz finds
/// are added by hand, not here, so do not regenerate over them.
#[test]
#[ignore = "writes fixture files; run explicitly to (re)generate the seed corpus"]
fn generate_seed_corpus() {
    use zstd_pure::{compress, compress_seekable};

    let write = |category: &str, name: &str, bytes: &[u8]| {
        let d = dir(category);
        fs::create_dir_all(&d).expect("create regression dir");
        fs::write(d.join(name), bytes).expect("write seed case");
    };

    // decode/: hostile / edge frames that reach deep decoder states.
    write(
        "decode",
        "0001-reserved-block-type.bin",
        &[0x28, 0xB5, 0x2F, 0xFD, 0x00, 0x00, 0x06, 0x00, 0x00],
    );
    write(
        "decode",
        "0002-raw-block-past-eof.bin",
        &[0x28, 0xB5, 0x2F, 0xFD, 0x00, 0x00, 0x51, 0x00, 0x00],
    );
    write(
        "decode",
        "0003-dict-id-without-dict.bin",
        &[0x28, 0xB5, 0x2F, 0xFD, 0x01, 0x00, 0x07],
    );
    write("decode", "0004-all-zeros.bin", &[0u8; 64]);
    let real = compress(
        b"a real frame, then cut in half to truncate it",
        9,
        true,
        true,
    );
    write(
        "decode",
        "0005-truncated-real-frame.bin",
        &real[..real.len() / 2],
    );

    // decode_diff/: valid frames both decoders must agree on.
    write(
        "decode_diff",
        "0001-raw-block-hello.bin",
        &[
            0x28, 0xB5, 0x2F, 0xFD, 0x00, 0x00, 0x29, 0x00, 0x00, b'h', b'e', b'l', b'l', b'o',
        ],
    );
    let rep = b"ab".repeat(200);
    write(
        "decode_diff",
        "0002-libzstd-repetitive.bin",
        &zstd::bulk::compress(&rep, 9).unwrap(),
    );

    // seekable/: a valid archive and a bad-magic blob.
    let payload = b"seekable regression corpus seed payload ".repeat(50);
    write(
        "seekable",
        "0001-valid-archive.bin",
        &compress_seekable(&payload, 64, 3, true).unwrap(),
    );
    write("seekable", "0002-bad-seekable-magic.bin", &[0u8; 20]);
}
