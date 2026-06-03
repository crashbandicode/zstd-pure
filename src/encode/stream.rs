//! Incremental, streaming **frame encoding** — the encoder analogue of
//! [`StreamingDecoder`](crate::StreamingDecoder) and zstd's
//! `ZSTD_compressStream`.
//!
//! [`StreamingEncoder`] accepts input in arbitrary chunks via [`push`], emitting
//! complete compressed blocks as soon as a full 128 KiB block has accumulated,
//! and writes the trailing content checksum on [`finish`]. The frame header uses
//! the **unknown-content-size** form (no `Frame_Content_Size` pledge), exactly
//! like libzstd's streaming compressor; our one-shot decoder, our
//! `StreamingDecoder`, and libzstd all decode the result.
//!
//! The produced frame is **independent of how the input was chunked across
//! [`push`] calls**: block boundaries fall at fixed 128 KiB multiples (a held-back
//! tail becomes the last block at [`finish`]), and each block is parsed only
//! against the input committed up to its boundary, so the finder, entropy-table
//! threading, and checksum are all functions of the byte stream alone.
//!
//! Memory is **bounded** — independent of the total (possibly multi-gigabyte)
//! input. The encoder retains only roughly two windows plus a couple of blocks of
//! past input: once the retained history grows past `2 * window`, the dead prefix
//! (older than one window before the last emitted block, so unreachable by any
//! future back-reference) is dropped and the match finder is **rebuilt over the
//! retained window** via the same priming path a dictionary uses — uniform across
//! every strategy (including the binary tree) and reusing already-tested code, so
//! it cannot reintroduce a position-aliasing bug. (A tighter, rebuild-free bound
//! via an in-place index reduction is a possible later refinement.) Input is also
//! consumed in block-sized steps internally, so a single huge [`push`] does not
//! spike memory.
//!
//! Long-distance matching is not yet wired in (it needs a whole-input index), so a
//! streamed frame uses the regular window-bounded finder only.
//!
//! [`push`]: StreamingEncoder::push
//! [`finish`]: StreamingEncoder::finish

#[allow(unused_imports)]
use crate::alloc_prelude::*;

use super::super::frame::ZSTD_MAGIC;
use super::super::xxhash::Xxh64;
use super::block::{write_compressed_block, write_raw_block, write_store_block, EncState, BLOCK_SIZE_MAX};
use super::frame::{split_depth_for, write_frame_header_streaming};

/// A block-by-block, incremental single-frame encoder mirroring
/// [`compress`](crate::compress)'s block loop but driven by [`push`] calls.
///
/// Build it with [`new`](Self::new) (standard frame, no checksum) or
/// [`with_options`](Self::with_options), feed bytes with [`push`](Self::push),
/// optionally drain ready output with [`take_output`](Self::take_output), and
/// finalize with [`finish`](Self::finish). If you never drain, `finish` returns
/// the entire frame.
///
/// [`push`]: Self::push
pub struct StreamingEncoder {
    /// Whether a trailing XXH64 content checksum is written on `finish`.
    checksum: bool,
    /// Level parameters, kept so the finder can be rebuilt after a slide.
    params: super::params::CParams,
    /// Frame window (back-reference reach); fixed for the level so it never
    /// depends on how much input eventually arrives.
    max_offset: usize,
    /// Block-splitter recursion depth for this level's strategy.
    split_depth: usize,
    /// Retained input: the bytes at absolute positions `[dropped, dropped +
    /// input.len())`. The dead prefix is dropped as the stream advances, so this
    /// stays bounded (~`2 * window` + a couple of blocks).
    input: Vec<u8>,
    /// Absolute count of input bytes dropped off the front of `input` (the base
    /// `input[0]` corresponds to). Positions handed to the finder are relative to
    /// `input`, i.e. `absolute - dropped`.
    dropped: usize,
    /// Absolute count of input bytes already emitted as blocks.
    emitted: usize,
    /// Compressed output produced but not yet drained (header + emitted blocks,
    /// plus the checksum once `finish` runs).
    out: Vec<u8>,
    /// Persistent match finder (spans block boundaries up to `max_offset`);
    /// rebuilt over the retained window on each slide.
    finder: super::lz::Finder,
    /// Cross-block encoder state: running repeat offsets + the previous
    /// compressed block's entropy tables (Repeat / Treeless reuse). Holds
    /// distances, not positions, so it is unaffected by sliding.
    state: EncState,
    /// Streaming content hash for the trailing checksum.
    hasher: Xxh64,
}

impl StreamingEncoder {
    /// Create a streaming encoder for `level`, producing a standard
    /// (magic-prefixed) frame with no content checksum.
    pub fn new(level: i32) -> Self {
        Self::with_options(level, false, true)
    }

    /// Full constructor: choose the compression `level`, whether to append an
    /// XXH64 content `checksum`, and whether to emit the 4-byte frame magic
    /// (`expect_magic = false` produces a magicless frame).
    pub fn with_options(level: i32, checksum: bool, expect_magic: bool) -> Self {
        // The total size is unknown up front, so use the level's nominal
        // (un-shrunk) window: a fixed advertised window — and thus a fixed
        // `max_offset` — keeps the frame independent of how much is eventually
        // written, mirroring libzstd's streaming behaviour with an unknown
        // pledged size. (`params_for_level` still caps it at the portable 8 MiB.)
        let params = super::params::params_for_level(level, usize::MAX);
        let window_log = params.window_log;
        let max_offset = 1usize << window_log;
        let split_depth = split_depth_for(params.strategy);

        let mut out = Vec::new();
        if expect_magic {
            out.extend_from_slice(&ZSTD_MAGIC.to_le_bytes());
        }
        write_frame_header_streaming(&mut out, checksum, window_log);

        StreamingEncoder {
            checksum,
            params,
            max_offset,
            split_depth,
            input: Vec::new(),
            dropped: 0,
            emitted: 0,
            out,
            finder: super::lz::Finder::new(&params),
            state: EncState { rep: [1, 4, 8], seq: super::sequences::SeqCTables::default(), lit: None },
            hasher: Xxh64::new(0),
        }
    }

    /// Total input bytes seen so far (`dropped` + what is still retained).
    fn total(&self) -> usize {
        self.dropped + self.input.len()
    }

    /// Append `data` to the stream, emitting any 128 KiB blocks that are now
    /// complete. Always keeps at least one block's worth of the tail unemitted so
    /// [`finish`](Self::finish) can mark the final block — and emits only whole
    /// `BLOCK_SIZE_MAX` blocks, so the resulting frame is identical no matter how
    /// the input was split across `push` calls.
    pub fn push(&mut self, data: &[u8]) {
        self.hasher.update(data);
        // Append in block-sized steps and drain after each, so even a single huge
        // `push` never holds more than the bounded working set at once.
        for part in data.chunks(BLOCK_SIZE_MAX) {
            self.input.extend_from_slice(part);
            self.drain_full_blocks();
        }
    }

    /// Emit every block that is now complete, keeping at least one block's worth of
    /// tail unemitted (so `finish` can flag the last block), and slide the retained
    /// window afterward to keep memory bounded.
    fn drain_full_blocks(&mut self) {
        while self.total() - self.emitted > BLOCK_SIZE_MAX {
            let end = self.emitted + BLOCK_SIZE_MAX;
            self.emit_block(end, false);
            self.maybe_slide();
        }
    }

    /// Drop the dead prefix and rebuild the finder once the retained, already-
    /// emitted history exceeds `2 * window`. The dropped bytes are older than one
    /// window before the last emitted block, so no future back-reference can reach
    /// them; afterward exactly one window of history is retained. Rebuilding the
    /// finder over that window (via the dictionary-priming path) keeps its position
    /// tables consistent with the slid buffer — correct for every strategy,
    /// including the binary tree, with no bespoke index-shifting.
    fn maybe_slide(&mut self) {
        let history = self.emitted - self.dropped;
        if history <= 2 * self.max_offset {
            return;
        }
        let drop = history - self.max_offset; // leave exactly one window of history
        self.input.drain(..drop);
        self.dropped += drop;
        self.finder = super::lz::Finder::new(&self.params);
        let retained_history = self.emitted - self.dropped; // == max_offset
        self.finder.prime(&self.input, retained_history, self.max_offset);
    }

    /// Remove and return the compressed output produced so far (the frame header
    /// and any completed blocks). Lets a consumer keep output memory bounded by
    /// draining between [`push`](Self::push) calls; the concatenation of every
    /// `take_output` plus the final [`finish`](Self::finish) is the whole frame.
    pub fn take_output(&mut self) -> Vec<u8> {
        core::mem::take(&mut self.out)
    }

    /// Finish the frame: emit the final (last) block, append the content checksum
    /// if enabled, and return whatever compressed output has not yet been drained
    /// by [`take_output`](Self::take_output). If `take_output` was never called,
    /// the returned buffer is the complete frame.
    pub fn finish(mut self) -> Vec<u8> {
        if self.total() == 0 {
            // A frame must contain at least one block; emit an empty last raw one.
            write_raw_block(&mut self.out, true, &[]);
        } else {
            // `push` already drains to at most one block of tail; this loop is
            // defensive (and a no-op in practice).
            self.drain_full_blocks();
            let end = self.total();
            self.emit_block(end, true);
        }
        if self.checksum {
            let digest = (self.hasher.digest() & 0xFFFF_FFFF) as u32;
            self.out.extend_from_slice(&digest.to_le_bytes());
        }
        self.out
    }

    /// Bytes of compressed output buffered and ready to drain via
    /// [`take_output`](Self::take_output).
    pub fn pending_output_len(&self) -> usize {
        self.out.len()
    }

    /// Bytes of input currently retained. Stays bounded as the stream advances
    /// (~`2 * window` plus a couple of blocks), independent of the total input.
    pub fn buffered_input_len(&self) -> usize {
        self.input.len()
    }

    /// The frame's window size (the back-reference reach / retained-history
    /// target), mirroring [`StreamingDecoder::window_size`](crate::StreamingDecoder::window_size).
    pub fn window_size(&self) -> usize {
        self.max_offset
    }

    /// Emit one block covering the absolute input range `[emitted, end)`, choosing
    /// the smaller of a compressed or a store block (and committing the compressed
    /// block's state only when it wins — exactly as [`compress`](crate::compress)
    /// does). Positions are translated to the retained buffer (`absolute -
    /// dropped`); the block is parsed against `input[..end_rel]` so the finder
    /// cannot read past the committed boundary, which is what makes the output
    /// chunk-independent.
    fn emit_block(&mut self, end: usize, last: bool) {
        let start_rel = self.emitted - self.dropped;
        let end_rel = end - self.dropped;
        let mut store = Vec::new();
        write_store_block(&mut store, last, &self.input[start_rel..end_rel]);

        let mut comp = Vec::new();
        match write_compressed_block(
            &mut comp,
            last,
            &self.input[..end_rel],
            start_rel..end_rel,
            &mut self.finder,
            self.max_offset,
            &self.state,
            self.split_depth,
        ) {
            Ok(next) if comp.len() < store.len() => {
                self.state = next;
                self.out.extend_from_slice(&comp);
            }
            _ => self.out.extend_from_slice(&store),
        }
        self.emitted = end;
    }
}

/// Feed plaintext into the encoder as an [`io::Write`](crate::io::Write) sink:
/// `write` is [`push`](StreamingEncoder::push) (it always consumes the whole
/// buffer) and `flush` is a no-op — completed blocks are already buffered, so
/// drain them with [`take_output`](StreamingEncoder::take_output) and emit the
/// final block + checksum with [`finish`](StreamingEncoder::finish). This lets a
/// `StreamingEncoder` stand in for any `Write` sink, e.g. `io::copy` or
/// `write!`, symmetric to [`StreamingDecoder`](crate::StreamingDecoder)'s
/// [`io::Read`](crate::io::Read).
impl crate::io::Write for StreamingEncoder {
    fn write(&mut self, buf: &[u8]) -> crate::io::Result<usize> {
        self.push(buf);
        Ok(buf.len())
    }

    fn flush(&mut self) -> crate::io::Result<()> {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{decompress, StreamingDecoder};
    use crate::io::Read;

    /// Build a frame by pushing `data` in `chunk`-sized writes (0 = one write).
    fn stream(data: &[u8], level: i32, checksum: bool, chunk: usize) -> Vec<u8> {
        let mut enc = StreamingEncoder::with_options(level, checksum, true);
        if chunk == 0 {
            enc.push(data);
        } else {
            for part in data.chunks(chunk) {
                enc.push(part);
            }
        }
        enc.finish()
    }

    /// A streamed frame must decode back to `data` through our one-shot decoder,
    /// our `StreamingDecoder`, and libzstd.
    fn assert_round_trips_three_ways(frame: &[u8], data: &[u8]) {
        assert_eq!(decompress(frame).unwrap(), data, "one-shot self decode");

        let mut dec = StreamingDecoder::new(frame).unwrap();
        let mut got = Vec::new();
        dec.read_to_end(&mut got).unwrap();
        assert_eq!(got, data, "streaming self decode");

        let by_lib = zstd::bulk::decompress(frame, data.len() + 64)
            .expect("libzstd must decode our streamed frame");
        assert_eq!(by_lib, data, "libzstd decode");
    }

    /// A representative spread of inputs: empty, tiny, an RLE run, repetitive
    /// text, structured records, near-random, and a multi-block mixture.
    fn corpus() -> Vec<Vec<u8>> {
        let text = b"the quick brown fox jumps over the lazy dog. ".repeat(120);
        let structured: Vec<u8> =
            (0..40_000u32).map(|i| (i.wrapping_mul(2654435761) >> 11) as u8).collect();
        let json: Vec<u8> = (0..3000u32)
            .flat_map(|i| format!("{{\"id\":{i},\"k\":\"v_{}\"}}\n", i % 39).into_bytes())
            .collect();
        let mut rng = 0x1234_5678u32;
        let random: Vec<u8> = (0..200_000)
            .map(|_| {
                rng = rng.wrapping_mul(1664525).wrapping_add(1013904223);
                (rng >> 24) as u8
            })
            .collect();
        vec![
            vec![],
            vec![0u8],
            b"abc".to_vec(),
            b"hello world".to_vec(),
            vec![0x42u8; 100_000],
            text,
            structured,
            json,
            random,
        ]
    }

    #[test]
    fn round_trips_three_ways_across_levels() {
        for data in corpus() {
            for &level in &[1i32, 3, 6, 9, 13, 19] {
                for &checksum in &[false, true] {
                    let frame = stream(&data, level, checksum, 0);
                    assert_round_trips_three_ways(&frame, &data);
                }
            }
        }
    }

    #[test]
    fn frame_is_independent_of_write_chunk_size() {
        // Spans several 128 KiB blocks so boundaries are actually exercised.
        let mut data = b"the quick brown fox jumps over the lazy dog. ".repeat(7000);
        data.extend((0..120_000u32).map(|i| (i.wrapping_mul(2654435761) >> 13) as u8));
        for &level in &[1i32, 3, 9, 19] {
            for &checksum in &[false, true] {
                let reference = stream(&data, level, checksum, 0);
                // 1-byte, sub-block, block-sized, and super-block write sizes.
                for &chunk in &[1usize, 7, 1000, 65_536, BLOCK_SIZE_MAX, BLOCK_SIZE_MAX + 1, 300_000] {
                    let f = stream(&data, level, checksum, chunk);
                    assert_eq!(
                        f, reference,
                        "frame differs at L{level} checksum={checksum} chunk={chunk}"
                    );
                }
                assert_round_trips_three_ways(&reference, &data);
            }
        }
    }

    #[test]
    fn multi_block_stream_compresses_and_round_trips() {
        // >128 KiB of compressible text: must span multiple blocks, shrink well,
        // and round-trip three ways at every level class.
        let data = b"the quick brown fox jumps over the lazy dog. ".repeat(10_000);
        assert!(data.len() > 3 * BLOCK_SIZE_MAX);
        for &level in &[1i32, 3, 9, 19] {
            let frame = stream(&data, level, true, 4096);
            assert!(
                frame.len() < data.len() / 4,
                "L{level}: expected real compression, {} from {}",
                frame.len(),
                data.len()
            );
            assert_round_trips_three_ways(&frame, &data);
        }
    }

    #[test]
    fn incremental_take_output_reconstructs_whole_frame() {
        let mut data = b"streaming take_output reconstruction corpus. ".repeat(4000);
        data.extend((0..50_000u32).map(|i| (i.wrapping_mul(40503) >> 8) as u8));

        // Drain after every push; the concatenation of drained pieces + finish
        // must equal the undrained frame, and decode to the input.
        let mut enc = StreamingEncoder::with_options(9, true, true);
        let mut pieced = Vec::new();
        for part in data.chunks(7777) {
            enc.push(part);
            pieced.extend_from_slice(&enc.take_output());
        }
        pieced.extend_from_slice(&enc.finish());

        let whole = stream(&data, 9, true, 7777);
        assert_eq!(pieced, whole, "drained output must reconstruct the frame");
        assert_round_trips_three_ways(&pieced, &data);
    }

    #[test]
    fn magicless_stream_round_trips() {
        let data = b"magicless streamed payload not too short ".repeat(50);
        let mut enc = StreamingEncoder::with_options(3, true, false);
        enc.push(&data);
        let frame = enc.finish();
        // Our magicless one-shot decoder reads it back.
        let got = crate::decompress_magicless(&frame, 1 << 20).unwrap();
        assert_eq!(got.data, data);
        // ...and libzstd in magicless mode.
        let mut dctx = zstd::zstd_safe::DCtx::create();
        dctx.set_parameter(zstd::zstd_safe::DParameter::Format(
            zstd::zstd_safe::FrameFormat::Magicless,
        ))
        .unwrap();
        let mut out = vec![0u8; data.len()];
        let n = dctx.decompress(&mut out, &frame).unwrap();
        assert_eq!(&out[..n], &data[..]);
    }

    #[test]
    fn usable_as_an_io_write_sink() {
        use crate::io::Write;
        let mut data = b"io::Write sink for the streaming encoder. ".repeat(5000);
        data.extend((0..60_000u32).map(|i| (i.wrapping_mul(2654435761) >> 12) as u8));

        let mut enc = StreamingEncoder::with_options(9, true, true);
        for part in data.chunks(5000) {
            enc.write_all(part).unwrap();
        }
        enc.flush().unwrap();
        let via_write = enc.finish();

        // `write` delegates to `push`, so the frame matches the push-based path.
        assert_eq!(via_write, stream(&data, 9, true, 5000));
        assert_round_trips_three_ways(&via_write, &data);
    }

    #[test]
    fn io_copy_into_encoder_round_trips() {
        // The encoder stands in as a `std::io::Write` sink for `io::copy`.
        let data = b"copied through std::io::copy into the encoder. ".repeat(3000);
        let mut enc = StreamingEncoder::new(3);
        let mut src: &[u8] = &data;
        std::io::copy(&mut src, &mut enc).unwrap();
        let frame = enc.finish();
        assert_round_trips_three_ways(&frame, &data);
    }

    #[test]
    fn memory_stays_bounded_while_sliding() {
        // L1 has the smallest window (512 KiB), so a few-MB input forces several
        // slides cheaply. Mildly redundant (small alphabet) so real matches — not
        // just store blocks — cross the slide boundaries. (Higher levels share the
        // exact slide + finder-rebuild path; only the thresholds grow.)
        let mut data = Vec::with_capacity(3 << 20);
        let mut x = 0x9E37_79B9u32;
        while data.len() < (3 << 20) {
            x = x.wrapping_mul(1664525).wrapping_add(1013904223);
            data.push((x >> 27) as u8);
        }

        let push_chunk = 64 * 1024;
        let mut enc = StreamingEncoder::with_options(1, true, true);
        let window = enc.window_size();
        let bound = 2 * window + 2 * BLOCK_SIZE_MAX;
        assert!(bound < data.len(), "test input must exceed the memory bound to force a slide");

        for part in data.chunks(push_chunk) {
            enc.push(part);
            assert!(
                enc.buffered_input_len() <= bound,
                "retained input {} exceeded bound {bound}",
                enc.buffered_input_len()
            );
        }
        let chunked = enc.finish();
        assert_round_trips_three_ways(&chunked, &data);

        // Sliding is deterministic, so a single big push (internally stepped) must
        // produce the identical frame.
        let mut enc2 = StreamingEncoder::with_options(1, true, true);
        enc2.push(&data);
        assert!(enc2.buffered_input_len() <= bound, "single-push retained too much");
        let whole = enc2.finish();
        assert_eq!(chunked, whole, "sliding frame must be independent of push chunking");
    }
}
