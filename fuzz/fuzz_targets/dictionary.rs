//! Dictionary target: (1) parsing *arbitrary* bytes as a structured dictionary
//! must never panic, and decoding an untrusted frame against the result must stay
//! bounded; (2) a dictionary trained from chunks of the input must round-trip a
//! payload through `compress_with_dict` / `decompress_with_dict`. The trainer and
//! dict parser are complex enough to deserve their own fuzzer.
#![no_main]

use libfuzzer_sys::fuzz_target;
use zstd_pure::{compress_with_dict, decompress_with_dict, train_dictionary, Dictionary};

fuzz_target!(|data: &[u8]| {
    let (dict_bytes, frame) = data.split_at(data.len() / 2);

    // (1) Untrusted dictionary bytes: parse must be Ok/Err (never panic), and
    //     decoding a frame against a parsed dict must be bounded (never OOM/panic).
    if let Ok(dict) = Dictionary::parse(dict_bytes) {
        let _ = decompress_with_dict(frame, &dict, 1 << 20);
    }

    // (2) Train a bounded dictionary from chunks of the input and round-trip a
    //     payload through it. Work is capped (<=16 samples, small segments) so the
    //     fuzzer stays fast.
    if !data.is_empty() {
        let chunk = (data.len() / 4).clamp(1, 4096);
        let samples: Vec<&[u8]> = data.chunks(chunk).take(16).collect();
        let trained = train_dictionary(&samples, 4096);
        if !trained.is_empty() {
            let dict = Dictionary::raw(&trained);
            let comp = compress_with_dict(frame, &dict, 3, false, true);
            assert_eq!(
                decompress_with_dict(&comp, &dict, frame.len() + 64)
                    .expect("our dict round-trip must succeed"),
                frame,
                "compress_with_dict -> decompress_with_dict mismatch"
            );
        }
    }
});
