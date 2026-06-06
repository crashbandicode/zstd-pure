//! Golden frame fixtures: offline, version-pinned decode anchors.
//!
//! Each `tests/fixtures/frames/<name>.zst` must decode to exactly its committed
//! `<name>.expected` bytes through our one-shot decoder, our streaming decoder,
//! and the current libzstd. Unlike the dynamic libzstd oracle, the expected bytes
//! are committed, so these catch a decoder regression even across a libzstd
//! dev-dependency bump. The corpus is small but chosen to hit format/entropy
//! corners (see the fixtures README), and the walker asserts those corners are
//! actually present, not merely intended.

use std::collections::HashSet;
use std::fs;
use std::io::Read;
use std::path::{Path, PathBuf};

use zstd_pure::{decompress, frame_header, StreamingDecoder};

fn frames_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/frames")
}

/// Walk a standard frame's blocks, returning (block types seen, literals-section
/// types seen in compressed blocks). Literals type is the low 2 bits of the first
/// byte of a compressed block body (Raw 0 / RLE 1 / Compressed 2 / Treeless 3).
fn block_and_literal_types(frame: &[u8]) -> (Vec<u8>, Vec<u8>) {
    let h = frame_header(frame).expect("golden frame header");
    let mut pos = h.header_len;
    let (mut block_types, mut literal_types) = (Vec::new(), Vec::new());
    loop {
        let v =
            (frame[pos] as u32) | ((frame[pos + 1] as u32) << 8) | ((frame[pos + 2] as u32) << 16);
        let last = (v & 1) != 0;
        let btype = ((v >> 1) & 3) as u8;
        let bsize = (v >> 3) as usize;
        pos += 3;
        block_types.push(btype);
        match btype {
            0 => pos += bsize,
            1 => pos += 1,
            2 => {
                literal_types.push(frame[pos] & 0x3);
                pos += bsize;
            }
            _ => panic!("reserved block type 3 in a golden frame"),
        }
        if last {
            break;
        }
    }
    (block_types, literal_types)
}

#[test]
fn golden_frames_decode_byte_identically() {
    let dir = frames_dir();
    let mut zsts: Vec<PathBuf> = fs::read_dir(&dir)
        .expect("fixtures dir — run generate_golden_frames")
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.extension().map(|x| x == "zst").unwrap_or(false))
        .collect();
    zsts.sort();
    assert!(!zsts.is_empty(), "no golden frames present");

    let mut block_types = HashSet::new();
    let mut literal_types = HashSet::new();

    for zst in &zsts {
        let stem = zst.file_stem().unwrap().to_string_lossy().into_owned();
        let frame = fs::read(zst).expect("read .zst");
        let expected = fs::read(zst.with_extension("expected"))
            .unwrap_or_else(|_| panic!("missing {stem}.expected"));

        assert_eq!(
            decompress(&frame).unwrap(),
            expected,
            "[{stem}] one-shot decode"
        );

        let mut dec = StreamingDecoder::new(&frame).expect("streaming construct");
        let mut streamed = Vec::new();
        dec.read_to_end(&mut streamed).expect("streaming read");
        assert_eq!(streamed, expected, "[{stem}] streaming decode");

        assert_eq!(
            zstd::stream::decode_all(frame.as_slice()).unwrap(),
            expected,
            "[{stem}] libzstd oracle decode"
        );

        let (bt, lt) = block_and_literal_types(&frame);
        block_types.extend(bt);
        literal_types.extend(lt);
    }

    // The corpus must actually exercise the corners it documents.
    assert!(block_types.contains(&0), "corpus has no Raw block");
    assert!(block_types.contains(&1), "corpus has no RLE block");
    assert!(block_types.contains(&2), "corpus has no Compressed block");
    assert!(
        literal_types.contains(&3),
        "corpus has no Treeless (repeat-Huffman) literals section"
    );
}

/// Regenerate the golden frames. Run explicitly:
/// `cargo test --test golden_frames generate_golden_frames -- --ignored`.
#[test]
#[ignore = "writes fixture files; run explicitly to (re)generate golden frames"]
fn generate_golden_frames() {
    let dir = frames_dir();
    fs::create_dir_all(&dir).expect("create fixtures dir");
    let write = |name: &str, input: &[u8], level: i32| {
        let frame = zstd::bulk::compress(input, level).expect("libzstd compress");
        fs::write(dir.join(format!("{name}.zst")), &frame).expect("write .zst");
        fs::write(dir.join(format!("{name}.expected")), input).expect("write .expected");
    };
    let write_raw = |name: &str, frame: &[u8], expected: &[u8]| {
        fs::write(dir.join(format!("{name}.zst")), frame).expect("write .zst");
        fs::write(dir.join(format!("{name}.expected")), expected).expect("write .expected");
    };

    // Simple format anchors.
    write("empty", b"", 3);
    write("one-byte", b"Z", 3);

    // Multi-block, stable-but-skewed literal statistics with few long matches:
    // block 1 builds a Huffman tree, later blocks reuse it (Treeless literals).
    let mut skew = Vec::with_capacity(200_000);
    let mut s = 0x1234_5678_9abc_def0u64;
    let alphabet = b"etaoinshrdlcumwfgypbvkjxqz .,\n";
    for _ in 0..200_000 {
        s = s
            .wrapping_mul(6364136223846793005)
            .wrapping_add(1442695040888963407);
        skew.push(alphabet[((s >> 33) as usize) % alphabet.len()]);
    }
    // Force many small blocks (flush every 8 KiB) over stable literal statistics:
    // libzstd then reuses the previous block's Huffman tree rather than re-sending
    // a tree description for each tiny block → Treeless literals. (One big 128 KiB
    // block always re-derives the optimal tree, so it never goes treeless.)
    {
        use std::io::Write;
        let mut enc = zstd::stream::write::Encoder::new(Vec::new(), 3).expect("encoder");
        for chunk in skew.chunks(8192) {
            enc.write_all(chunk).expect("write chunk");
            enc.flush().expect("flush block");
        }
        let frame = enc.finish().expect("finish frame");
        write_raw("multiblock-treeless", &frame, &skew);
    }

    // Incompressible bytes → a Raw block (libzstd stores them verbatim).
    let mut raw = Vec::with_capacity(5000);
    let mut r = 0xC0FF_EE12_3456_789A_u64;
    for _ in 0..5000 {
        r = r.wrapping_mul(6364136223846793005).wrapping_add(1);
        raw.push((r >> 40) as u8);
    }
    write("raw-incompressible", &raw, 3);

    // A true RLE block (Block_Type 1): libzstd's encoder doesn't emit these for our
    // inputs, so hand-build one — magic, FHD=0, 1 KiB window, then a last RLE block
    // of 1000 copies of 'A' (header v = last | type1<<1 | size<<3 = 0x1F43).
    let mut rle = vec![0x28, 0xB5, 0x2F, 0xFD, 0x00, 0x00, 0x43, 0x1F, 0x00];
    rle.push(b'A');
    write_raw("rle-block", &rle, &vec![b'A'; 1000]);

    // Periodic data: matches at small offsets (period 64 → offsets 1/2/4/8/… reuse).
    let base: Vec<u8> = (0..64u8).collect();
    let periodic: Vec<u8> = base.iter().cloned().cycle().take(32 * 1024).collect();
    write("offsets-periodic", &periodic, 12);

    // Small, regular input at level 1: predefined FSE tables.
    let small: Vec<u8> = (0..2000u32).flat_map(|i| (i % 7).to_le_bytes()).collect();
    write("predefined-fse-low-level", &small, 1);
}
