//! Robustness harness for the pure-Rust Zstandard decoder (zstd_pure T1.5).
//!
//! Three things, all fixture-free and using libzstd (the `zstd` dep) as the
//! oracle:
//!
//! 1. A **decode corpus matrix** — frames generated across compression levels,
//!    window logs, checksum on/off, single/multi-frame, skippable frames, and
//!    dictionaries; each must decode identically through both the one-shot and
//!    streaming paths.
//! 2. A **never-panic** sweep — arbitrary and mutated-valid byte strings fed to
//!    the decoder must only ever return `Ok`/`Err`, never panic.
//! 3. An **oracle** sweep — random payloads compressed by libzstd must decode
//!    back to the exact payload.
//! 4. An **encoder** sweep — random payloads compressed by *our* encoder must
//!    decode back through both libzstd and our own decoder (T2.x).

use std::io::Read;

use zstd::zstd_safe::{CCtx, CParameter};
use zstd_pure::{
    compress as our_compress, compress_long as our_compress_long, decompress, decompress_capped,
    decompress_with_dict, frame_header, Dictionary, StreamingDecoder,
};

/// Small deterministic LCG so the harness is reproducible (no rand dep).
struct Rng(u64);
impl Rng {
    fn new(seed: u64) -> Self {
        Rng(seed)
    }
    fn next_u32(&mut self) -> u32 {
        // SplitMix64-ish.
        self.0 = self.0.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        ((z ^ (z >> 31)) >> 32) as u32
    }
    fn range(&mut self, n: usize) -> usize {
        if n == 0 {
            0
        } else {
            (self.next_u32() as usize) % n
        }
    }
}

/// A spread of input profiles that exercise different decoder paths.
fn input_profiles() -> Vec<(&'static str, Vec<u8>)> {
    let redundant: Vec<u8> = (0..40_000u32)
        .flat_map(|i| (i % 13).to_le_bytes())
        .collect();
    let mut structured = b"FRES____".to_vec();
    for i in 0..12_000u32 {
        structured.extend_from_slice(&(i.wrapping_mul(2654435761) % 251).to_le_bytes());
    }
    let text = "the quick brown fox jumps over the lazy dog. "
        .repeat(900)
        .into_bytes();
    let mut rng = Rng::new(0xC0FF_EE12);
    let random: Vec<u8> = (0..30_000).map(|_| rng.next_u32() as u8).collect();
    let zeros = vec![0u8; 200_000];
    vec![
        ("redundant", redundant),
        ("structured", structured),
        ("text", text),
        ("random", random),
        ("zeros", zeros),
        ("empty", Vec::new()),
        ("tiny", b"ab".to_vec()),
    ]
}

fn compress(data: &[u8], level: i32, window_log: Option<u32>, checksum: bool) -> Vec<u8> {
    let mut cctx = CCtx::create();
    cctx.set_parameter(CParameter::CompressionLevel(level))
        .unwrap();
    cctx.set_parameter(CParameter::ChecksumFlag(checksum))
        .unwrap();
    if let Some(wl) = window_log {
        cctx.set_parameter(CParameter::WindowLog(wl)).unwrap();
        // Drop the content size so a real window descriptor is emitted.
        cctx.set_parameter(CParameter::ContentSizeFlag(false))
            .unwrap();
    }
    let mut out = Vec::with_capacity(zstd::zstd_safe::compress_bound(data.len()));
    cctx.compress2(&mut out, data).unwrap();
    out
}

fn stream_decode(comp: &[u8]) -> Vec<u8> {
    let mut dec = StreamingDecoder::new(comp).expect("construct streaming decoder");
    let mut out = Vec::new();
    dec.read_to_end(&mut out).expect("streaming read");
    out
}

#[test]
fn decode_corpus_matrix() {
    let profiles = input_profiles();
    let mut cases = 0usize;
    for (name, data) in &profiles {
        for &level in &[1i32, 3, 9, 19, 22] {
            for &checksum in &[false, true] {
                for &wl in &[None, Some(10u32), Some(15), Some(18)] {
                    // A tiny window on a large input is the interesting case.
                    let comp = compress(data, level, wl, checksum);
                    let one = decompress(&comp).unwrap_or_else(|e| {
                        panic!("[{name} L{level} wl{wl:?} ck{checksum}] one-shot: {e}")
                    });
                    assert_eq!(
                        &one, data,
                        "[{name} L{level} wl{wl:?} ck{checksum}] one-shot mismatch"
                    );
                    let streamed = stream_decode(&comp);
                    assert_eq!(
                        &streamed, data,
                        "[{name} L{level} wl{wl:?} ck{checksum}] streaming mismatch"
                    );
                    cases += 1;
                }
            }
        }
    }
    assert!(cases >= 200, "expected a broad matrix, ran {cases}");
}

#[test]
fn multi_frame_and_skippable() {
    let profiles = input_profiles();
    // Concatenate several independent frames + a skippable frame in the middle.
    let mut stream = Vec::new();
    let mut expected = Vec::new();
    for (i, (_, data)) in profiles.iter().enumerate() {
        let frame = compress(data, 3 + (i as i32 % 3) * 4, None, i % 2 == 0);
        stream.extend_from_slice(&frame);
        expected.extend_from_slice(data);
        if i == 2 {
            // Insert a skippable frame: magic 0x184D2A50 + u32 len + body. The
            // decoder must skip it and contribute no output.
            stream.extend_from_slice(&0x184D_2A50u32.to_le_bytes());
            let body = b"skippable-metadata-blob";
            stream.extend_from_slice(&(body.len() as u32).to_le_bytes());
            stream.extend_from_slice(body);
        }
    }
    let got = decompress(&stream).expect("multi-frame decode");
    assert_eq!(got, expected);
    // libzstd agrees on the concatenation too (sanity on the oracle side).
    assert_eq!(zstd::stream::decode_all(&stream[..]).unwrap(), expected);
}

#[test]
fn dictionary_matrix() {
    // Trained (structured) dict + raw-content dict, across levels.
    let samples: Vec<Vec<u8>> = (0..700u32)
        .map(|i| {
            format!(
                "{{\"id\":{i},\"type\":\"npc_{}\",\"hp\":{},\"pos\":[{},{}]}}\n",
                i % 53,
                (i * 17) % 999,
                i % 128,
                (i * 3) % 128
            )
            .into_bytes()
        })
        .collect();
    let trained = zstd::dict::from_samples(&samples, 16 * 1024).expect("train");
    let trained_dict = Dictionary::parse(&trained).unwrap();
    let raw_bytes = samples.concat();
    let raw_dict = Dictionary::raw(&raw_bytes);

    for s in samples.iter().step_by(7) {
        for &level in &[1i32, 6, 19] {
            // Structured.
            let mut c = zstd::bulk::Compressor::with_dictionary(level, &trained).unwrap();
            let comp = c.compress(s).unwrap();
            assert_eq!(
                decompress_with_dict(&comp, &trained_dict, 1 << 22).unwrap(),
                *s
            );
            // Raw.
            let mut c2 = zstd::bulk::Compressor::with_dictionary(level, &raw_bytes).unwrap();
            let comp2 = c2.compress(s).unwrap();
            assert_eq!(
                decompress_with_dict(&comp2, &raw_dict, 1 << 22).unwrap(),
                *s
            );
        }
    }
}

#[test]
fn never_panics_on_arbitrary_bytes() {
    let mut rng = Rng::new(0x5EED_1234);
    // (a) Pure random byte strings of varied lengths.
    for _ in 0..4000 {
        let len = rng.range(64);
        let buf: Vec<u8> = (0..len).map(|_| rng.next_u32() as u8).collect();
        // One-shot must never panic; any outcome is acceptable.
        let _ = decompress_capped(&buf, 1 << 20);
        // Streaming construction + read must never panic either.
        if let Ok(mut dec) = StreamingDecoder::new(&buf) {
            let mut sink = Vec::new();
            let _ = dec.read_to_end(&mut sink);
        }
    }
    // (b) Mutated valid frames — flip / truncate bytes of a real frame. These
    // are the inputs most likely to reach deep decoder states before failing.
    let base = compress(&input_profiles()[0].1, 9, Some(15), true);
    for _ in 0..4000 {
        let mut buf = base.clone();
        let muts = 1 + rng.range(6);
        for _ in 0..muts {
            if buf.is_empty() {
                break;
            }
            let idx = rng.range(buf.len());
            buf[idx] ^= 1u8 << rng.range(8);
        }
        if rng.range(3) == 0 && !buf.is_empty() {
            buf.truncate(rng.range(buf.len()));
        }
        let _ = decompress_capped(&buf, 1 << 22);
        if let Ok(mut dec) = StreamingDecoder::new(&buf) {
            let mut sink = Vec::new();
            let _ = dec.read_to_end(&mut sink);
        }
    }
}

#[test]
fn our_encoder_round_trips_through_libzstd_and_self() {
    // (a) The structured input profiles, with and without a content checksum.
    for (name, data) in input_profiles() {
        for &checksum in &[false, true] {
            let frame = our_compress(&data, 3, checksum, true);
            let by_libzstd = zstd::bulk::decompress(&frame, data.len() + 64).unwrap_or_else(|e| {
                panic!("[{name} ck{checksum}] libzstd decode of our frame: {e}")
            });
            assert_eq!(by_libzstd, data, "[{name} ck{checksum}] libzstd mismatch");
            assert_eq!(
                decompress(&frame).unwrap(),
                data,
                "[{name} ck{checksum}] self mismatch"
            );
        }
    }

    // (b) Randomized payloads (biased + uniform) so both compressible and
    // incompressible blocks are exercised; verify against both decoders.
    let mut rng = Rng::new(0xE17C_0DE5);
    for _ in 0..400 {
        let len = rng.range(20_000);
        let bias = rng.range(8);
        let payload: Vec<u8> = (0..len)
            .map(|_| {
                let v = rng.next_u32();
                if bias > 0 {
                    (v % (bias as u32 * 6)) as u8
                } else {
                    v as u8
                }
            })
            .collect();
        let frame = our_compress(&payload, 3, rng.range(2) == 0, true);
        let by_libzstd = zstd::bulk::decompress(&frame, payload.len() + 64)
            .unwrap_or_else(|e| panic!("libzstd decode of our frame (len {len}): {e}"));
        assert_eq!(by_libzstd, payload, "libzstd mismatch len {len}");
        assert_eq!(
            decompress(&frame).unwrap(),
            payload,
            "self mismatch len {len}"
        );
    }
}

#[test]
fn our_frames_round_trip_through_the_streaming_decoder() {
    // (a) Regular `compress`: every profile must also decode through the streaming
    //     (sliding-window) decoder, not only the one-shot path the encoder sweep
    //     already covers.
    for (name, data) in input_profiles() {
        for &level in &[1i32, 9, 19] {
            let frame = our_compress(&data, level, level % 2 == 1, true);
            assert_eq!(
                stream_decode(&frame),
                data,
                "[{name} L{level}] streaming decode of our compress"
            );
        }
    }

    // (b) `compress_long`: a 512 KiB chunk, ~9 MiB of filler, then the same chunk
    //     again — its copy sits ~9.5 MiB back (past the 8 MiB window). The frame
    //     advertises a > 8 MiB window and emits a large-offset back-reference, so
    //     the streaming decoder must slide correctly at that window log. — §4.2c
    let mut rng = Rng::new(0x10D_C0DE);
    let chunk: Vec<u8> = (0..512 * 1024).map(|_| rng.next_u32() as u8).collect();
    let filler: Vec<u8> = (0..9_000_000).map(|_| rng.next_u32() as u8).collect();
    let mut data = Vec::with_capacity(chunk.len() * 2 + filler.len());
    data.extend_from_slice(&chunk);
    data.extend_from_slice(&filler);
    data.extend_from_slice(&chunk);
    for &checksum in &[false, true] {
        let frame = our_compress_long(&data, 1, checksum, true);
        let h = frame_header(&frame).unwrap();
        assert!(
            h.window_size > (8 << 20),
            "compress_long should advertise a > 8 MiB window"
        );
        assert_eq!(
            stream_decode(&frame),
            data,
            "streaming decode of our LDM frame (ck{checksum})"
        );
        // libzstd (default windowLogMax 27) agrees.
        assert_eq!(
            zstd::bulk::decompress(&frame, data.len() + 64).unwrap(),
            data
        );
    }
}

#[test]
fn oracle_on_random_payloads() {
    let mut rng = Rng::new(0xABCD_0001);
    for _ in 0..300 {
        let len = rng.range(8000);
        // Mix of biased + uniform bytes so some frames compress and some don't.
        let bias = rng.range(6);
        let payload: Vec<u8> = (0..len)
            .map(|_| {
                let v = rng.next_u32();
                if bias > 0 {
                    (v % (bias as u32 * 8)) as u8
                } else {
                    v as u8
                }
            })
            .collect();
        let level = [1i32, 3, 7, 12, 19][rng.range(5)];
        let comp = zstd::bulk::compress(&payload, level).unwrap();
        let got = decompress(&comp).expect("decode random payload");
        assert_eq!(got, payload, "oracle mismatch len {len} L{level}");
        // libzstd-decode must agree (catches any non-canonical encoder choice).
        assert_eq!(
            zstd::bulk::decompress(&comp, payload.len() + 64).unwrap(),
            payload
        );
    }
}

#[test]
fn decompression_bomb_is_refused_by_the_cap() {
    // A highly compressible run produces a tiny frame that regenerates far more
    // than its size. `decompress_capped` with a ceiling below the output must
    // refuse it (the decompression-bomb defense) and accept it once the ceiling is
    // sufficient — the explicit, named complement to the streaming bounded-window
    // tests.
    let bomb = vec![0u8; 8 << 20]; // 8 MiB of zeros -> a tiny frame
    let frame = zstd::bulk::compress(&bomb, 19).unwrap();
    assert!(
        frame.len() < 4096,
        "expected a tiny bomb frame, got {}",
        frame.len()
    );
    assert!(
        decompress_capped(&frame, 64 * 1024).is_err(),
        "an 8 MiB output under a 64 KiB cap must be refused"
    );
    assert_eq!(
        decompress_capped(&frame, (8 << 20) + 64).unwrap(),
        bomb,
        "a sufficient cap decodes the frame"
    );
}

#[test]
fn malformed_frames_error_not_panic() {
    // Every malformed input must return Err from the public decode API (and, being
    // a normal test, must not panic) — explicit, named cases complementing the
    // randomized never-panic sweep above.
    let good = zstd::bulk::compress(b"content worth corrupting in a few ways".as_ref(), 3).unwrap();

    let mut bad_magic = good.clone();
    bad_magic[0] ^= 0xFF;
    assert!(decompress(&bad_magic).is_err(), "bad magic");

    assert!(
        decompress(&good[..good.len() / 2]).is_err(),
        "truncated frame"
    );
    assert!(decompress(&[0u8]).is_err(), "a single stray byte");
    assert!(
        decompress(b"not a zstd frame at all").is_err(),
        "arbitrary bytes"
    );

    // Reserved bit (Frame_Header_Descriptor bit 3) set must be rejected (§3.1.1.1.1).
    let mut reserved = good.clone();
    reserved[4] |= 0b0000_1000;
    assert!(decompress(&reserved).is_err(), "reserved FHD bit set");
}
