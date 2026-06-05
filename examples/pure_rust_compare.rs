//! Pure-Rust zstd peer comparison harness.
//!
//! Reproduce the documented comparison:
//! `ZSTD_PURE_CORPUS=/path/to/silesia/raw cargo run --release --example pure_rust_compare`.
//!
//! Knobs:
//! - `ZSTD_PURE_COMPARE_LEVELS` (default `1,3,9`)
//! - `ZSTD_PURE_COMPARE_MAX_MB` per-file cap for the corpus (default `8`, `0` = full files)

use std::collections::BTreeMap;
use std::fs;
use std::io::{self, Read};
use std::panic::{catch_unwind, AssertUnwindSafe};
use std::path::Path;
use std::time::{Duration, Instant};

use ruzstd::encoding::CompressionLevel;
use zstd_pure::{compress, decompress};

mod common;
use common::enwik_like;

#[derive(Clone)]
struct Input {
    name: String,
    data: Vec<u8>,
}

struct Encoded {
    frames: Vec<Vec<u8>>,
    bytes: usize,
    elapsed: Duration,
}

fn profiles() -> Vec<Input> {
    let redundant: Vec<u8> = (0..40_000u32)
        .flat_map(|i| (i % 13).to_le_bytes())
        .collect();
    let mut records = b"FRES____".to_vec();
    for i in 0..12_000u32 {
        records.extend_from_slice(&(i.wrapping_mul(2654435761) % 251).to_le_bytes());
    }
    let text = "the quick brown fox jumps over the lazy dog. "
        .repeat(900)
        .into_bytes();
    let json: Vec<u8> = (0..4_000u32)
        .flat_map(|i| {
            format!(
                "{{\"id\":{i},\"type\":\"npc_{}\",\"hp\":{}}}\n",
                i % 53,
                (i * 17) % 999
            )
            .into_bytes()
        })
        .collect();
    let chunk: Vec<u8> = (0..90_000u32)
        .map(|i| (i.wrapping_mul(2654435761) >> 13) as u8)
        .collect();
    let mut mixed = "the quick brown fox jumps over the lazy dog. "
        .repeat(1500)
        .into_bytes();
    mixed.truncate(64 * 1024);
    let mut jsonish: Vec<u8> = (0..3_000u32)
        .flat_map(|i| {
            format!(
                "{{\"id\":{i},\"type\":\"npc_{}\",\"hp\":{}}}\n",
                i % 53,
                (i * 17) % 999
            )
            .into_bytes()
        })
        .collect();
    jsonish.truncate(64 * 1024);
    mixed.extend_from_slice(&jsonish);
    vec![
        Input {
            name: "redundant".into(),
            data: redundant,
        },
        Input {
            name: "records".into(),
            data: records,
        },
        Input {
            name: "text".into(),
            data: text,
        },
        Input {
            name: "json".into(),
            data: json,
        },
        Input {
            name: "3x90k-chunk".into(),
            data: chunk.repeat(3),
        },
        Input {
            name: "mixed".into(),
            data: mixed,
        },
        Input {
            name: "wiki".into(),
            data: enwik_like(150_000, 0x5747_494b_4900_0001),
        },
    ]
}

fn load_corpus(root: &Path, max_mb: usize) -> io::Result<Vec<Input>> {
    fn walk(dir: &Path, files: &mut Vec<Input>, max_bytes: Option<usize>) -> io::Result<()> {
        let mut entries: Vec<_> = fs::read_dir(dir)?.collect::<io::Result<_>>()?;
        entries.sort_by_key(|e| e.path());
        for entry in entries {
            let path = entry.path();
            if path.is_dir() {
                walk(&path, files, max_bytes)?;
            } else if path.is_file() {
                let mut data = fs::read(&path)?;
                if let Some(max) = max_bytes {
                    data.truncate(max);
                }
                let name = path
                    .file_name()
                    .and_then(|s| s.to_str())
                    .unwrap_or("input")
                    .to_string();
                files.push(Input { name, data });
            }
        }
        Ok(())
    }

    let max_bytes = if max_mb == 0 {
        None
    } else {
        Some(max_mb * 1024 * 1024)
    };
    let mut files = Vec::new();
    walk(root, &mut files, max_bytes)?;
    Ok(files)
}

fn levels() -> Vec<i32> {
    std::env::var("ZSTD_PURE_COMPARE_LEVELS")
        .unwrap_or_else(|_| "1,3,9".into())
        .split(',')
        .filter_map(|s| s.trim().parse().ok())
        .collect()
}

fn mib_per_s(bytes: usize, elapsed: Duration) -> f64 {
    let secs = elapsed.as_secs_f64().max(1e-9);
    bytes as f64 / (1024.0 * 1024.0) / secs
}

fn time<T>(f: impl FnOnce() -> T) -> (T, Duration) {
    let start = Instant::now();
    let out = f();
    (out, start.elapsed())
}

fn catch_result<T>(what: &str, f: impl FnOnce() -> Result<T, String>) -> Result<T, String> {
    match catch_unwind(AssertUnwindSafe(f)) {
        Ok(result) => result,
        Err(_) => Err(format!("{what} panicked")),
    }
}

fn encode_one(codec: &str, level: i32, data: &[u8]) -> Result<Vec<u8>, String> {
    match codec {
        "zstd-pure" => Ok(compress(data, level, false, true)),
        "libzstd" => zstd::bulk::compress(data, level).map_err(|e| e.to_string()),
        "oxiarc-zstd" => oxiarc_zstd::compress_with_level(data, level).map_err(|e| e.to_string()),
        "ruzstd" if level == 1 => Ok(ruzstd::encoding::compress_to_vec(
            data,
            CompressionLevel::Fastest,
        )),
        "ruzstd" => Err("only CompressionLevel::Fastest (~L1) is implemented".into()),
        _ => unreachable!("unknown codec"),
    }
}

fn encode_all(codec: &str, level: i32, inputs: &[Input]) -> Result<Encoded, String> {
    let mut frames = Vec::with_capacity(inputs.len());
    let mut bytes = 0usize;
    let mut elapsed = Duration::ZERO;
    for input in inputs {
        let (frame, dt) = time(|| catch_result("encode", || encode_one(codec, level, &input.data)));
        let frame = frame.map_err(|e| format!("{}: {e}", input.name))?;
        bytes += frame.len();
        elapsed += dt;
        frames.push(frame);
    }
    Ok(Encoded {
        frames,
        bytes,
        elapsed,
    })
}

fn ruzstd_decode(frame: &[u8]) -> Result<Vec<u8>, String> {
    let mut dec = ruzstd::decoding::StreamingDecoder::new(frame).map_err(|e| format!("{e:?}"))?;
    let mut out = Vec::new();
    dec.read_to_end(&mut out).map_err(|e| e.to_string())?;
    Ok(out)
}

fn decode_one(decoder: &str, frame: &[u8], cap: usize) -> Result<Vec<u8>, String> {
    match decoder {
        "zstd-pure" => decompress(frame).map_err(|e| e.to_string()),
        "libzstd" => zstd::bulk::decompress(frame, cap).map_err(|e| e.to_string()),
        "ruzstd" => ruzstd_decode(frame),
        "oxiarc-zstd" => oxiarc_zstd::decompress(frame).map_err(|e| e.to_string()),
        _ => unreachable!("unknown decoder"),
    }
}

fn decode_all(decoder: &str, frames: &[Vec<u8>], inputs: &[Input]) -> Result<Duration, String> {
    let mut elapsed = Duration::ZERO;
    for (frame, input) in frames.iter().zip(inputs) {
        let cap = input.data.len() + 64;
        let (decoded, dt) = time(|| catch_result("decode", || decode_one(decoder, frame, cap)));
        let decoded = decoded.map_err(|e| format!("{}: {e}", input.name))?;
        if decoded != input.data {
            return Err(format!("{}: decoded bytes mismatch", input.name));
        }
        elapsed += dt;
    }
    Ok(elapsed)
}

fn print_suite(name: &str, inputs: &[Input]) {
    let raw: usize = inputs.iter().map(|i| i.data.len()).sum();
    println!(
        "\n## {name} ({} inputs, {:.2} MiB)",
        inputs.len(),
        raw as f64 / (1024.0 * 1024.0)
    );
    for level in levels() {
        println!("\n### level {level}");
        println!(
            "| encoder | bytes | vs libzstd | enc MiB/s | zstd-pure dec | libzstd dec | ruzstd dec | oxiarc dec |"
        );
        println!("|---|---:|---:|---:|---:|---:|---:|---:|");

        let mut encoded = BTreeMap::new();
        for codec in ["libzstd", "zstd-pure", "oxiarc-zstd", "ruzstd"] {
            match encode_all(codec, level, inputs) {
                Ok(e) => {
                    encoded.insert(codec, Ok(e));
                }
                Err(e) => {
                    encoded.insert(codec, Err(e));
                }
            }
        }
        let lib_size = match encoded.get("libzstd") {
            Some(Ok(e)) => e.bytes,
            _ => 0,
        };

        for codec in ["libzstd", "zstd-pure", "oxiarc-zstd", "ruzstd"] {
            match encoded.remove(codec).unwrap() {
                Ok(e) => {
                    let ratio = if lib_size == 0 {
                        "n/a".into()
                    } else {
                        format!("{:.3}x", e.bytes as f64 / lib_size as f64)
                    };
                    let dec = |name: &str| match decode_all(name, &e.frames, inputs) {
                        Ok(dt) => format!("{:.1}", mib_per_s(raw, dt)),
                        Err(err) => format!("ERR ({err})"),
                    };
                    println!(
                        "| {codec} | {} | {ratio} | {:.1} | {} | {} | {} | {} |",
                        e.bytes,
                        mib_per_s(raw, e.elapsed),
                        dec("zstd-pure"),
                        dec("libzstd"),
                        dec("ruzstd"),
                        dec("oxiarc-zstd")
                    );
                }
                Err(err) => {
                    println!("| {codec} | ERR ({err}) | n/a | n/a | n/a | n/a | n/a | n/a |")
                }
            }
        }
    }
}

fn main() {
    std::panic::set_hook(Box::new(|_| {}));
    println!("# Pure-Rust zstd comparison");
    println!(
        "Decoder columns are MiB/s over the raw input bytes; `ERR` is a cross-decode failure."
    );
    print_suite("synthetic profiles", &profiles());

    match std::env::var("ZSTD_PURE_CORPUS") {
        Ok(root) => {
            let max_mb = std::env::var("ZSTD_PURE_COMPARE_MAX_MB")
                .ok()
                .and_then(|s| s.parse().ok())
                .unwrap_or(8);
            let inputs = load_corpus(Path::new(&root), max_mb).expect("load corpus");
            print_suite(&format!("corpus {root} (max {max_mb} MiB/file)"), &inputs);
        }
        Err(_) => {
            eprintln!("ZSTD_PURE_CORPUS unset; skipping real corpus comparison");
        }
    }
}
