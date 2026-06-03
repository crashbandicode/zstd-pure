# Changelog

All notable changes to this project are documented here. The format is based on
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and the project follows
[Semantic Versioning](https://semver.org/) (pre-1.0: a minor bump may include
breaking changes).

## [Unreleased]

_Nothing yet._

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
  pass, block splitting, chain/binary-tree hybrid match finder).
- Per-block entropy-table mode selection (predefined / RLE / FSE / repeat) and
  treeless literal reuse; cross-block matching + repeat-offset threading.
- Dictionary encode (`compress_with_dict`) plus a pure-Rust dictionary trainer
  (`train_dictionary`, `train_dictionary_structured` — the trainer is
  *experimental*: a simplified COVER whose output quality may change).
- Opt-in long-distance matching (`compress_long`).
- Incremental streaming encoder (`StreamingEncoder`, implements `io::Write`, with
  bounded memory and optional long-distance matching).
- Independent-frame parallel compression (`compress_parallel`, `std` only).

### Other
- Seekable format (`compress_seekable`, `SeekTable`, random access).
- Validation: a deterministic corpus matrix, a `proptest` shrinking suite, three
  cargo-fuzz targets (`decode`, `decode_diff`, `encode_roundtrip`), a
  fixture-gated real-world corpus test, and a libzstd differential — every
  encoder output verified both ways.

[RFC 8878]: https://www.rfc-editor.org/rfc/rfc8878
[Unreleased]: https://github.com/crashbandicode/zstd-pure/compare/v0.1.0...HEAD
[0.1.0]: https://github.com/crashbandicode/zstd-pure/releases/tag/v0.1.0
