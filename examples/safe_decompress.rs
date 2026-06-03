//! Safe, **bounded** decompression of untrusted zstd frames — how to decode
//! attacker-controlled input without risking a decompression bomb or an
//! oversized-window allocation.
//!
//! Run with `cargo run --example safe_decompress`.

use std::io::Read;
use zstd_pure::{compress, decompress_capped, StreamingDecoder};

fn main() {
    // Pretend these frames arrived from somewhere untrusted; here we make them.
    let original = b"the quick brown fox jumps over the lazy dog. ".repeat(6000); // ~270 KiB
    let frame = compress(&original, 9, /*checksum=*/ true, /*magic=*/ true);

    // (1) One-shot with an OUTPUT CEILING. `decompress_capped` refuses a frame
    //     whose regenerated size would exceed the ceiling, instead of allocating
    //     it — the decompression-bomb defense. (Plain `decompress` uses a 256 MiB
    //     default ceiling.)
    let cap = 1 << 20; // 1 MiB
    match decompress_capped(&frame, cap) {
        Ok(plain) => println!(
            "one-shot: decoded {} bytes (within the {cap}-byte cap)",
            plain.len()
        ),
        Err(e) => println!("one-shot: refused: {e}"),
    }

    // A "bomb": a tiny frame that regenerates far more than the cap.
    let bomb = compress(&vec![0u8; 8 << 20], 3, false, true); // 8 MiB of zeros -> a tiny frame
    println!("bomb frame is {} bytes but regenerates 8 MiB", bomb.len());
    match decompress_capped(&bomb, cap) {
        Ok(_) => unreachable!("the cap should have refused the bomb"),
        Err(e) => println!("bomb: correctly refused under the {cap}-byte cap: {e}"),
    }

    // (2) Streaming with BOUNDED MEMORY and a WINDOW-LOG CEILING. The decoder
    //     keeps only ~window + one block in memory regardless of the output size,
    //     and `with_options(.., window_log_max)` rejects up front any frame that
    //     declares a window larger than you permit.
    let window_log_max = 23; // accept windows up to 8 MiB; reject anything larger
    let mut dec = StreamingDecoder::with_options(
        &frame,
        /*magic=*/ true,
        /*dict=*/ None,
        window_log_max,
    )
    .expect("frame's declared window is within our budget");
    let mut out = Vec::new();
    let mut buf = [0u8; 64 * 1024];
    let mut peak = 0usize;
    loop {
        let n = dec.read(&mut buf).expect("decode");
        if n == 0 {
            break;
        }
        out.extend_from_slice(&buf[..n]);
        peak = peak.max(dec.buffered_len());
    }
    assert_eq!(out, original, "streamed output must match");
    println!(
        "streaming: decoded {} bytes; the bounded reader's buffer peaked at {peak} bytes \
         (capped near the window — independent of total output size, so a multi-GiB stream \
         would peak about the same)",
        out.len()
    );
}
