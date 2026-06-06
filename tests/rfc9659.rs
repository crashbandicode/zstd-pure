//! RFC 9659 (Window Sizing for Zstandard Content Encoding) conformance.
//!
//! RFC 9659 turns RFC 8878's 8 MiB window recommendation into a requirement for
//! the HTTP `zstd` content coding:
//!   - decoders MUST support a `Window_Size` up to and including 8 MiB;
//!   - encoders MUST NOT emit frames requiring a `Window_Size` larger than 8 MiB.
//!
//! These tests lock both MUSTs (our default encode paths already comply; the
//! decoder accepts the full 8 MiB) and the `decompress_http` content-coding
//! profile (rejects oversized windows, caps output, handles multi-frame +
//! skippable streams).

use zstd_pure::{
    compress, compress_with_options, decompress, decompress_http, frame_header, CompressOptions,
    ZstdError, HTTP_MAX_WINDOW_SIZE,
};

/// Decoder MUST support a `Window_Size` of exactly 8 MiB — and `decompress_http`
/// accepts and round-trips such a frame.
#[test]
fn decoder_supports_the_8_mib_window() {
    let data = b"rfc 9659 content-coding payload ".repeat(64);
    // An explicit window_log of 23 advertises exactly an 8 MiB window.
    let frame = compress_with_options(&data, &CompressOptions::new(6).window_log(23));
    assert_eq!(
        frame_header(&frame).unwrap().window_size,
        HTTP_MAX_WINDOW_SIZE,
        "expected an exactly-8-MiB window"
    );
    assert_eq!(decompress_http(&frame, 1 << 20).expect("http decode"), data);
}

/// The HTTP profile refuses a frame requiring a window larger than 8 MiB, while
/// the general decoder still accepts it (RFC 9659 governs the content coding, not
/// every zstd frame).
#[test]
fn decompress_http_rejects_windows_over_8_mib() {
    let data = b"large-window payload ".repeat(16);
    // Long-distance mode is the opt-in escape hatch that may exceed 8 MiB.
    let frame = compress_with_options(
        &data,
        &CompressOptions::new(6).long_distance(true).window_log(24),
    );
    assert!(
        frame_header(&frame).unwrap().window_size > HTTP_MAX_WINDOW_SIZE,
        "fixture must advertise a > 8 MiB window"
    );
    // The general decoder imposes no window ceiling.
    assert_eq!(decompress(&frame).unwrap(), data);
    // The RFC 9659 / HTTP profile rejects the oversized window.
    assert!(matches!(
        decompress_http(&frame, 1 << 20),
        Err(ZstdError::Invalid {
            what: "window size",
            ..
        })
    ));
}

/// `decompress_http` caps total output (decompression-bomb bound), like
/// `decompress_capped`.
#[test]
fn decompress_http_caps_total_output() {
    let data = b"the quick brown fox jumps ".repeat(500);
    let frame = compress(&data, 9, false, true);
    let l = data.len();
    assert!(matches!(
        decompress_http(&frame, l - 1),
        Err(ZstdError::OutputTooLarge { .. })
    ));
    assert_eq!(decompress_http(&frame, l).unwrap(), data);
}

/// A content-coding body may be multiple concatenated frames, possibly with
/// skippable frames interleaved; `decompress_http` reassembles them.
#[test]
fn decompress_http_handles_multi_frame_and_skippable() {
    let a = b"first frame body ".repeat(20);
    let b = b"second frame body ".repeat(20);
    let mut stream = compress(&a, 6, true, true);
    // A skippable frame in the middle: magic 0x184D2A50 + u32 length + body.
    stream.extend_from_slice(&0x184D_2A50u32.to_le_bytes());
    let body = b"content-coding metadata";
    stream.extend_from_slice(&(body.len() as u32).to_le_bytes());
    stream.extend_from_slice(body);
    stream.extend_from_slice(&compress(&b, 6, false, true));

    let mut expected = a.clone();
    expected.extend_from_slice(&b);
    assert_eq!(decompress_http(&stream, 1 << 20).unwrap(), expected);
}

/// Encoder MUST NOT emit a frame requiring a window larger than 8 MiB on the
/// default paths (regular `compress` clamps window_log to 23 at every level, and
/// `compress_with_options` clamps any override unless long-distance is requested).
#[test]
fn default_encode_paths_never_exceed_the_8_mib_window() {
    // The window can't exceed the input size, so a modest input is enough to
    // confirm no level over-advertises (the 8 MiB boundary itself is checked by
    // the explicit window_log cases below); kept small so high levels stay fast.
    let data: Vec<u8> = (0..64 * 1024u32)
        .map(|i| (i.wrapping_mul(2654435761) >> 13) as u8)
        .collect();
    for level in 1..=22 {
        let ws = frame_header(&compress(&data, level, false, true))
            .unwrap()
            .window_size;
        assert!(
            ws <= HTTP_MAX_WINDOW_SIZE,
            "compress L{level} advertised a {ws}-byte window (> 8 MiB)"
        );
    }
    // An explicit oversized window_log is clamped to 8 MiB without long-distance...
    let clamped = compress_with_options(&data, &CompressOptions::new(9).window_log(99));
    assert!(frame_header(&clamped).unwrap().window_size <= HTTP_MAX_WINDOW_SIZE);
    // ...whereas the opt-in long-distance mode may exceed it (not for HTTP use).
    let ldm = compress_with_options(
        &data,
        &CompressOptions::new(9).long_distance(true).window_log(26),
    );
    assert!(frame_header(&ldm).unwrap().window_size > HTTP_MAX_WINDOW_SIZE);
}
