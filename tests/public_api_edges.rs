use std::io::Read;

use zstd_pure::{
    compress, compress_with_options, decompress, decompress_capped, decompress_magicless_bytes,
    frame_header, frame_header_magicless, train_dictionary_structured, CompressOptions,
    StreamingDecoder, ZstdError,
};

fn libzstd_decompress_magicless(frame: &[u8], out_len: usize) -> Vec<u8> {
    let mut dctx = zstd::zstd_safe::DCtx::create();
    dctx.set_parameter(zstd::zstd_safe::DParameter::Format(
        zstd::zstd_safe::FrameFormat::Magicless,
    ))
    .expect("set magicless format");
    let mut out = vec![0u8; out_len];
    let n = dctx.decompress(&mut out, frame).expect("libzstd decode");
    out.truncate(n);
    out
}

#[test]
fn magicless_helpers_headers_and_caps_are_public_black_box() {
    let mut data = b"magicless public helper corpus ".repeat(3000);
    data.extend((0..30_000u32).map(|i| (i.wrapping_mul(2654435761) >> 17) as u8));

    let opts = CompressOptions::new(9)
        .checksum(true)
        .magic(false)
        .window_log(18);
    let frame = compress_with_options(&data, &opts);
    let header = frame_header_magicless(&frame).expect("magicless header");
    assert!(header.has_checksum);
    assert_eq!(header.content_size, Some(data.len() as u64));
    assert!(header.header_len < frame.len());
    assert!(
        frame_header(&frame).is_err(),
        "standard parser requires magic"
    );

    assert_eq!(
        decompress_magicless_bytes(&frame, data.len() + 64).expect("our decode"),
        data
    );
    assert_eq!(
        libzstd_decompress_magicless(&frame, data.len() + 64),
        data,
        "libzstd magicless decode"
    );

    let mut truncated = frame;
    truncated.truncate(header.header_len + 2);
    assert!(
        decompress_magicless_bytes(&truncated, data.len() + 64).is_err(),
        "truncated body must error, not panic or return partial data"
    );
}

#[test]
fn malformed_magicless_headers_report_typed_errors() {
    assert!(matches!(
        frame_header_magicless(&[]),
        Err(ZstdError::Truncated {
            what: "frame header descriptor",
            needed: 1
        })
    ));
    assert!(matches!(
        frame_header_magicless(&[0x08]),
        Err(ZstdError::ReservedBit("frame header descriptor"))
    ));
    assert!(matches!(
        frame_header_magicless(&[0x00]),
        Err(ZstdError::Truncated {
            what: "window descriptor",
            needed: 1
        })
    ));
    assert!(matches!(
        frame_header_magicless(&[0x23, 0xaa]),
        Err(ZstdError::Truncated {
            what: "dictionary id",
            ..
        })
    ));
    assert!(matches!(
        frame_header_magicless(&[0xa0]),
        Err(ZstdError::Truncated {
            what: "frame content size",
            needed: 4
        })
    ));
}

#[test]
fn streaming_magicless_and_window_cap_edges_are_public_black_box() {
    let data = b"streaming magicless edge corpus ".repeat(6000);
    let magicless =
        compress_with_options(&data, &CompressOptions::new(6).checksum(true).magic(false));

    let mut dec = StreamingDecoder::new_magicless(&magicless).expect("magicless streaming");
    let mut out = Vec::new();
    dec.read_to_end(&mut out).expect("read magicless stream");
    assert_eq!(out, data);
    assert_eq!(
        libzstd_decompress_magicless(&magicless, data.len() + 64),
        data
    );

    let standard = compress(&data, 3, false, true);
    assert!(
        StreamingDecoder::with_options(&standard, true, None, 10).is_err(),
        "declared window/content size should exceed a 1 KiB cap"
    );
    let mut dec = StreamingDecoder::with_options(&standard, true, None, 27).expect("cap accepts");
    out.clear();
    dec.read_to_end(&mut out).expect("read capped stream");
    assert_eq!(out, data);
}

/// `decompress_capped` enforces a TOTAL output ceiling across all frames, so a
/// multi-frame stream whose frames are each under the cap but together exceed it
/// is refused (issue #1: per-frame vs aggregate cap).
#[test]
fn decompress_capped_enforces_total_output_across_frames() {
    let each = 50_000usize;
    let a = vec![0xABu8; each];
    let b: Vec<u8> = (0..each as u32)
        .map(|i| (i.wrapping_mul(2654435761) >> 24) as u8)
        .collect();
    let mut stream = compress(&a, 9, false, true);
    stream.extend_from_slice(&compress(&b, 9, false, true));

    // A cap between one frame and the sum rejects the multi-frame stream.
    assert!(matches!(
        decompress_capped(&stream, each + each / 2),
        Err(ZstdError::OutputTooLarge { .. })
    ));
    // A cap at the true total (and the default cap) reconstructs both frames.
    let mut both = a.clone();
    both.extend_from_slice(&b);
    assert_eq!(decompress_capped(&stream, 2 * each).unwrap(), both);
    assert_eq!(decompress(&stream).unwrap(), both);
}

/// A compressed (type-2) block must not be allowed to expand past `max_output`
/// (issue #1: only raw/RLE blocks checked the ceiling before).
#[test]
fn decompress_capped_bounds_compressed_block_output() {
    // Repetitive-but-varied → a real compressed block (matches/sequences), not RLE.
    let data = b"the quick brown fox jumps over the lazy dog. ".repeat(5000);
    let frame = compress(&data, 9, false, true);
    assert!(
        frame.len() < data.len() / 10,
        "expected a well-compressed frame"
    );
    assert!(matches!(
        decompress_capped(&frame, 1024),
        Err(ZstdError::OutputTooLarge { .. })
    ));
    assert_eq!(decompress_capped(&frame, data.len()).unwrap(), data);
}

/// A frame that references a dictionary id but is decoded with no dictionary must
/// error rather than silently decode against missing history (issue #1). Crafted
/// header: magic | FHD(dict_id_flag=1) | window_descriptor | dict_id=7.
#[test]
fn frame_referencing_dict_id_without_dict_errors() {
    let frame = [0x28u8, 0xB5, 0x2F, 0xFD, 0x01, 0x00, 0x07];
    assert!(matches!(decompress(&frame), Err(ZstdError::Dictionary(_))));
}

/// `train_dictionary_structured` honors its `max_size` contract — the prepended
/// entropy header must not push the finalized dictionary past `max_size` (issue #3).
#[test]
fn structured_dictionary_respects_max_size() {
    let samples: Vec<Vec<u8>> = (0..200u32)
        .map(|i| format!("record {{ id: {}, tag: \"shared-prefix-value\" }}", i % 13).into_bytes())
        .collect();
    let refs: Vec<&[u8]> = samples.iter().map(|s| s.as_slice()).collect();
    for &max in &[64usize, 256, 1024, 4096] {
        let dict = train_dictionary_structured(&refs, max);
        assert!(
            dict.len() <= max,
            "structured dict {} bytes exceeds max_size {max}",
            dict.len()
        );
    }
}
