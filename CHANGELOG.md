# Changelog

All notable changes to this project are documented here. The format is based on
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and the project follows
[Semantic Versioning](https://semver.org/) (pre-1.0: a minor bump may include
breaking changes).

## [Unreleased]

### Added
- `decompress_http` (+ `HTTP_MAX_WINDOW_SIZE`): an [RFC 9659] content-coding
  decode profile for untrusted HTTP `zstd` bodies — rejects any frame requiring a
  `Window_Size` larger than 8 MiB and caps total output, across multi-frame and
  skippable streams. Our default encode paths already satisfy RFC 9659's
  encoder/decoder window MUSTs (now regression-tested).
- Parallel whole-archive seekable decode (`decompress_seekable_parallel` /
  `decompress_seekable_parallel_capped`, `std`-only): independent frames decode
  across threads, byte-identical to serial decode + concatenation.
- Advanced-parameter encode API (`CompressOptions` / `compress_with_options`),
  the analogue of libzstd's `ZSTD_CCtx_setParameter` (window / hash / chain /
  search log, min-match, target length, strategy, checksum, magic, LDM); defaults
  are byte-identical to `compress`.
- COVER (k,d) dictionary optimization (`train_dictionary_optimized`): grid-searches
  the segment/dmer parameters and keeps the best-compressing candidate (never
  worse than `train_dictionary`).

### Hardened
- Enforce RFC 8878 `Block_Maximum_Size = min(Window_Size, 128 KiB)` on every block
  (oversized block headers are rejected), in both the one-shot and streaming
  decoders.
- Cap each compressed block's *regenerated* size at `Block_Maximum_Size`, closing
  a streaming-decoder decompression-bomb vector (a single block can no longer
  balloon the buffer before window eviction).
- `decompress_capped` enforces a *total* output ceiling across all frames of a
  multi-frame stream (previously applied per frame).
- Reject a frame that references a dictionary id when no matching dictionary is
  supplied (rather than decoding against missing history).
- Reject a raw-content dictionary (id 0) for a frame that names a *nonzero*
  dictionary id — it can't prove it is the referenced dictionary, matching
  libzstd's "Dictionary mismatch". Applies to `decompress_with_dict` and
  `StreamingDecoder::with_dict`; a zero frame id still accepts any dictionary.
- Use checked arithmetic on the remaining hostile-input size computations
  (skippable-frame length, block-body extent, streaming window size) so the public
  decode paths are panic-free on 32-bit / bare-metal targets, not just 64-bit.
- Seekable random access (`decompress_seekable_frame`) uses checked offset
  arithmetic and requires the decoded length to match the seek table exactly.

### Changed
- Internal de-duplication / LOC-reduction pass across the codec; no behaviour
  change (byte-identical, verified by the libzstd corpus differential).
- Coverage is reported entirely from GitHub Actions (badge published to GitHub
  Pages; Codecov dropped); CI actions moved to Node 24 runtimes.

### Testing
- Validation-hardening pass (test-only; no library change):
  - `seekable_decode` fuzz target stressing random-access + parallel decode on
    *adversarial* archives (corrupt offsets / sizes / checksums, pathological
    job counts).
  - Streaming decode proven independent of caller read granularity
    (1/2/3/7/64/4096/65536-byte reads vs one-shot, for our + libzstd frames).
  - Cap + checksum decode semantics locked as proptests (cap monotonicity,
    insufficient-cap rejection, no silent wrong data under a content checksum,
    hostile tiny-read never-panic).
  - Encoder structural invariants: emitted frames are parsed and checked for
    RFC 8878 format-validity (magic/checksum/content-size/dict-id flags, block
    types, `Block_Maximum_Size`, single last block) across every encode entry
    point.
  - Typed-error corpus locking the `ZstdError` variant each crafted malformation
    yields (frame header / block header / dictionary / seekable).
  - Persistent fuzz-regression corpus (`tests/regressions/`) + walker, and an
    offline version-pinned golden-frame corpus (`tests/fixtures/frames/`)
    covering Raw/RLE/Compressed blocks and Treeless literals.

## [0.1.0] - 2026-06-03

Initial release: a from-scratch, pure-Rust Zstandard ([RFC 8878]) decoder and
encoder. `#![forbid(unsafe_code)]`, `no_std + alloc`, only `thiserror` at runtime.

### Decoder
- Standard, magicless (`ZSTD_f_zstd1_magicless`), multi-frame, and skippable frames.
- Raw / RLE / compressed blocks; Huff0 literals (1- and 4-stream, FSE-coded +
  direct weights, treeless reuse); FSE sequences (predefined / RLE / FSE / repeat)
  + repeat offsets; XXH64 content-checksum verification.
- Raw-content and structured/tagged dictionary decode.
- Streaming / bounded-memory sliding-window decode (`StreamingDecoder`,
  implements `io::Read`), windows up to 128 MiB (log 27).
- Frame inspection without decoding the body (`frame_header`).
- Decompression-bomb defense (`decompress_capped`) and oversized-window rejection.

### Encoder
- Levels 1–22: `fast`, `dfast`, greedy/lazy/lazy2, `btlazy2`, and the
  `btopt`/`btultra`/`btultra2` optimal parse (rep-aware DP, `btultra2` second
  pass, block splitting, chain/binary-tree hybrid match finder). The optimal
  parse also offers repeat-offset matches as candidates (libzstd's
  `ZSTD_BtGetAllMatches`), priced from persistent cross-block statistics
  (libzstd's dynamic price model), behind a per-block guard that emits the
  rep-free parse whenever it is smaller — so the candidates never enlarge a block.
  Narrows the high-level ratio on rep-heavy data (within ~0.8 % of libzstd on
  Silesia at L19; see BENCHMARKS.md).
- Per-block entropy-table mode selection (predefined / RLE / FSE / repeat) and
  treeless literal reuse; cross-block matching + repeat-offset threading.
- Dictionary encode (`compress_with_dict`) plus a pure-Rust dictionary trainer
  (`train_dictionary`, `train_dictionary_structured` — the trainer is
  *experimental*: a simplified COVER whose output quality may change).
- Opt-in long-distance matching (`compress_long`).
- Incremental streaming encoder (`StreamingEncoder`, implements `io::Write`, with
  bounded memory and optional long-distance matching).
- Single-continuous-frame parallel compression (`compress_parallel`, `std` only):
  workers emit blocks into one shared frame with a cross-seam window (libzstd's
  ZSTDMT design), so matching spans the segment seams and the ratio tracks serial
  `compress`. Deterministic output; decodes as one ordinary frame.

### Other
- Seekable format (`compress_seekable`, `SeekTable`, random access).
- Validation: a deterministic corpus matrix, a `proptest` shrinking suite, six
  cargo-fuzz targets (`decode`, `decode_diff`, `encode_roundtrip`,
  `streaming_roundtrip`, `seekable_roundtrip`, `parallel_roundtrip`), a
  fixture-gated real-world corpus test, and a libzstd differential — every
  encoder output verified both ways.

[RFC 8878]: https://www.rfc-editor.org/rfc/rfc8878
[RFC 9659]: https://www.rfc-editor.org/rfc/rfc9659
[Unreleased]: https://github.com/crashbandicode/zstd-pure/compare/v0.1.0...HEAD
[0.1.0]: https://github.com/crashbandicode/zstd-pure/releases/tag/v0.1.0
