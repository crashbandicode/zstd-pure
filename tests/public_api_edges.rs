use std::io::Read;

use zstd_pure::{
    compress, compress_with_options, decompress_magicless_bytes, frame_header,
    frame_header_magicless, CompressOptions, StreamingDecoder, ZstdError,
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
