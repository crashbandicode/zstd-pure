//! Differential decode target: our decoder vs libzstd on the same bytes.
//!
//! The one rock-solid, zero-false-positive conformance guarantee between two
//! decoders is: **for any input both accept, the decoded output is identical.**
//! That is what this target asserts.
//!
//! It deliberately does NOT flag accept-vs-reject disagreements, because the
//! two decoders' acceptance boundaries legitimately differ:
//!   - libzstd's *streaming* decoder (used here for multi-frame support) is
//!     lenient about some malformed frames a strict decoder rejects — e.g. a
//!     declared `Frame_Content_Size` that doesn't match the actual content, or
//!     zero-length input — whereas ours rejects them (as libzstd's own *one-
//!     shot* `ZSTD_decompress` does);
//!   - our one-shot decoder keeps the whole output, so it imposes no window
//!     ceiling and is more permissive than libzstd about large-window frames.
//!
//! Memory safety of the harness: both decoders are bounded to `CAP` output
//! bytes, and libzstd keeps its default `window_log_max` of 27 so a frame
//! merely *declaring* a huge window can't make it eagerly allocate a multi-GB
//! buffer. libzstd is also built WITHOUT the `legacy` feature (see Cargo.toml)
//! so it only accepts RFC 8878 frames, like ours.
#![no_main]

use std::io::Read;

use libfuzzer_sys::fuzz_target;
use zstd_pure::decompress_capped;

/// 64 MiB output ceiling, applied to both decoders.
const CAP: usize = 1 << 26;

/// Decode every frame in `data` with libzstd (multi-frame, like our decoder).
/// `Ok(None)` means it decoded but produced more than `cap` bytes, so it can't
/// be compared fairly under the cap.
fn libzstd_decode(data: &[u8], cap: usize) -> std::io::Result<Option<Vec<u8>>> {
    let mut dec = zstd::stream::read::Decoder::new(data)?;
    // libzstd's default; rejects larger-window frames WITHOUT allocating, so a
    // window-claim bomb can't OOM the run.
    dec.window_log_max(27)?;
    let mut out = Vec::new();
    dec.take(cap as u64 + 1).read_to_end(&mut out)?;
    Ok((out.len() <= cap).then_some(out))
}

fuzz_target!(|data: &[u8]| {
    let ours = decompress_capped(data, CAP);
    let theirs = libzstd_decode(data, CAP);
    match (ours, theirs) {
        // The conformance check: both accept -> identical output.
        (Ok(a), Ok(Some(b))) => assert_eq!(a, b, "decode output disagreement"),
        // Both accept, but libzstd decoded more than the cap while ours decoded
        // fewer: a length disagreement on a jointly-accepted input.
        (Ok(_), Ok(None)) => {
            panic!("libzstd decodes > {CAP} bytes where our decoder decodes fewer")
        }
        // Acceptance boundaries legitimately differ (see the module docs); only
        // the output on jointly-accepted frames is required to agree.
        _ => {}
    }
});
