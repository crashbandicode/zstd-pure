# zstd-pure — a pure-Rust Zstandard codec

![MSRV](https://img.shields.io/badge/MSRV-1.81-blue)
![License](https://img.shields.io/badge/license-MIT-blue)
![unsafe](https://img.shields.io/badge/unsafe-forbidden-success)

A from-scratch [Zstandard][zstd] ([RFC 8878]) **decoder and encoder**, written
entirely in safe Rust from the specification — no GPL code, and **no libzstd at
runtime**. The only runtime dependency is [`thiserror`]; libzstd (the `zstd`
crate) is a dev-only test/benchmark oracle. The crate is
`#![forbid(unsafe_code)]` and runs on `no_std` (with `alloc`).

It was built bottom-up and validated against libzstd and real Nintendo TotK
**BFRES / MeshCodec** frames (themselves standard magicless zstd).

- crate `zstd-pure` · library `zstd_pure` · MSRV 1.81 · `#![forbid(unsafe_code)]`

[zstd]: https://facebook.github.io/zstd/
[RFC 8878]: https://www.rfc-editor.org/rfc/rfc8878
[`thiserror`]: https://crates.io/crates/thiserror

## Status & stability

The **decoder** is at ecosystem parity (multi-frame, magicless, dictionaries,
streaming/bounded-memory, frame inspection) and hardened by a corpus matrix,
property tests, three cargo-fuzz targets, and a differential against libzstd. The
**encoder** produces RFC 8878 frames that libzstd decodes across all levels;
every encoder output is validated **both ways**
(`libzstd.decompress(compress(x)) == x` *and* `decompress(compress(x)) == x`).

The dictionary **trainers** (`train_dictionary` / `train_dictionary_structured`)
are flagged **experimental** in their rustdoc — they produce correct,
ratio-improving dictionaries, but the underlying COVER is simplified, so output
*quality* is below libzstd's `ZDICT` and the produced bytes may change as it
improves (a non-breaking change). Everything else is considered stable — the
whole decode side, `compress` / `compress_long`, `compress_with_dict` +
`Dictionary`, `compress_parallel`, `StreamingEncoder` (incl. `with_options_long`,
fuzzed for arbitrary input × chunkings), and the seekable format (its parser is
fuzzed against arbitrary bytes).

> **Not yet published to crates.io.** Pin a git revision until a release is
> tagged (see below).

## Using as a dependency

```toml
[dependencies]
zstd-pure = { git = "https://github.com/crashbandicode/zstd-pure", rev = "<commit>" }
```

`no_std` (decoder + encoder over `alloc`):

```toml
zstd-pure = { git = "...", rev = "...", default-features = false, features = ["alloc"] }
```

## Usage

### Decompress

```rust
use zstd_pure::decompress;
let plain: Vec<u8> = decompress(&frame)?;
```

Bounded (decompression-bomb-safe) — refuse output larger than a ceiling:

```rust
use zstd_pure::decompress_capped;
let plain = decompress_capped(&frame, 64 << 20)?; // error if it would exceed 64 MiB
```

Streaming, bounded-memory (implements `std::io::Read`):

```rust
use zstd_pure::StreamingDecoder;
use std::io::Read;
let mut dec = StreamingDecoder::new(&frame)?;
let mut out = Vec::new();
dec.read_to_end(&mut out)?;
```

### Compress

```rust
use zstd_pure::compress;
// compress(data, level, content_checksum, emit_magic)
let frame = compress(&data, 3, false, true);
```

Streaming encode (implements `std::io::Write`):

```rust
use zstd_pure::StreamingEncoder;
let mut enc = StreamingEncoder::new(3);
enc.push(part_a);
enc.push(part_b);
let frame = enc.finish();
```

Parallel, multi-frame (`std` only):

```rust
// compress_parallel(data, level, n_jobs, checksum, magic) -> a multi-frame stream
let frame = zstd_pure::compress_parallel(&data, 9, 8, false, true);
```

Long-distance matching (`compress_long`), dictionaries (`compress_with_dict`,
`train_dictionary*`, `Dictionary`), magicless frames, and seekable archives
(`compress_seekable`, `SeekTable`) are also available — see the API docs.

## Conformance

Targets **RFC 8878** (the current Zstandard standard; it obsoletes RFC 8478, and
the wire format is unchanged between them). Every encoder output is validated by
libzstd, which implements RFC 8878. Notable points:

- **Content checksum** — low 4 bytes of `XXH64(data, seed = 0)`, little-endian (§3.1.1).
- **Reserved bit** of the `Frame_Header_Descriptor` must be 0; a frame that sets it is rejected (§3.1.1.1.1).
- **Window size** — `compress` caps `Window_Size` at 8 MiB, honoring §3.1.1.1.2's recommendation that a compressor not require more (for broad decoder interoperability); the streaming decoder accepts windows up to 128 MiB (log 27), the default `windowLogMax` of a stock `ZSTD_decompress`. The opt-in `compress_long` (long-distance matching) is the deliberate exception: it may advertise a `Window_Size` up to 128 MiB to make its long-range matches reachable — still within that default limit, so the frames stay broadly decodable. Plain `compress` is unchanged at 8 MiB.
- **Block_Maximum_Size** — `min(Window_Size, 128 KiB)` (§3.1.1.2).

## Supported / unsupported

**Decode:** standard, magicless (`ZSTD_f_zstd1_magicless`), multi-frame, and
skippable frames; raw / RLE / compressed blocks; Huff0 literals (1- and 4-stream,
FSE-coded + direct weights, treeless reuse); FSE sequences (predefined / RLE /
FSE / repeat) + repeat offsets; XXH64 checksum verification; raw-content and
structured/tagged dictionaries; frame inspection without decoding the body;
streaming/bounded-memory decode; windows up to 128 MiB.

**Encode:** levels 1–22 (fast, dfast, greedy/lazy/lazy2, btlazy2, btopt/btultra/
btultra2); per-block entropy-table mode selection (predefined/RLE/FSE/repeat,
treeless literals); cross-block matching + repeat-offset threading; block
splitting; dictionaries (use + a pure-Rust trainer); opt-in long-distance
matching; streaming; independent-frame parallel compression.

**Out of scope / not supported:** legacy zstd formats (pre-v0.8 / the variants
RFC 8478 obsoleted) — RFC 8878 only; windows beyond log 27; multi-threaded
*decode* (decode is single-threaded; `compress_parallel` is an encoder feature).
Skippable frames are skipped on decode (their payload isn't surfaced).

## Known limitations

- **Ratio:** the encoder trails libzstd by roughly 2–8 % on real-world corpora at
  high levels (the gap is the optimal parse + entropy modelling, not window
  reach) — see [`BENCHMARKS.md`](BENCHMARKS.md).
- **`compress_long` (v1):** forward-only match extension, per-block match
  generation (a match is clamped at the block boundary), and no LDM under
  `compress_with_dict`.
- **`compress_parallel`:** the independent-frame model loses cross-segment
  matching at the frame seams (a small ratio cost, like libzstd's job model).
- **`StreamingEncoder` memory:** bounded at ~2× window (not 1×); a tighter
  rebuild-free bound is future work.
- **Dictionary training:** a simplified single-pool greedy COVER — correct and
  ratio-improving, but lower quality than libzstd's `ZDICT`.
- **Decode speed:** ~0.65 GiB/s on a 256 KiB mix (on par with the pure-Rust
  `ruzstd`; libzstd's SIMD is ~3–4× faster).

## Memory & allocation

- **One-shot `decompress`** allocates the full output. Use
  `decompress_capped(frame, max)` to bound it — it errors on a frame that would
  exceed `max`, the decompression-bomb defense.
- **`StreamingDecoder`** keeps memory bounded to ~`window + one block`,
  independent of the logical output size. `StreamingDecoder::with_options(..,
  window_log_max)` rejects a frame declaring a larger window than you permit
  (default 27 = 128 MiB). A frame *declaring* a large window with small content
  does **not** force a large allocation — allocation tracks the content.
- **`StreamingEncoder`** keeps retained input bounded to ~`2× window` + a couple
  of blocks, independent of the total stream length.

See [`examples/safe_decompress.rs`](examples/safe_decompress.rs)
(`cargo run --example safe_decompress`) for these patterns end to end.

## Testing

- **Always-run** (`cargo test`): unit tests + a deterministic decode/encode
  corpus matrix (`tests/corpus.rs`) + a `proptest` shrinking suite
  (`tests/proptest.rs`) + named robustness tests (decompression-bomb, malformed
  frames, never-panic). Every encoder output is checked both ways against the
  libzstd oracle.
- **Fuzzing** (`cargo +nightly fuzz run <target>`, in `fuzz/`): `decode` (never
  panic/OOM on arbitrary/malformed frames under a 64 MiB cap), `decode_diff` (a
  differential requiring our decoder and libzstd to agree on any frame *both*
  accept), `encode_roundtrip`, `streaming_roundtrip` (`StreamingEncoder` over
  arbitrary input × chunkings, plain + LDM), and `seekable_roundtrip` (the
  seek-table parser never panics + archive round-trip). Seed
  `fuzz/corpus/<target>/` for depth.
- **Fixture-gated, off by default** (`#[ignore]`): `tests/real_corpus.rs` walks
  `$ZSTD_PURE_CORPUS` and round-trips every file both ways across levels, e.g.
  `ZSTD_PURE_CORPUS=~/fixtures/silesia/raw cargo test --release real_corpus -- --ignored --nocapture`
  (knobs: `ZSTD_PURE_CORPUS_LEVELS`, `ZSTD_PURE_CORPUS_MAX_MB`, `ZSTD_PURE_CORPUS_LONG`).

## Benchmarks

Ratio and throughput vs libzstd (and a decode comparison vs the pure-Rust
`ruzstd`) are tracked in [`BENCHMARKS.md`](BENCHMARKS.md):
`cargo run --release --example ratio` (size), `cargo run --release --example
bench_large` (large-input size + time + parallel speedup), `cargo bench` (criterion
throughput).

## Features / `no_std`

| feature | default | effect |
|---|---|---|
| `std` | on | `io::Read`/`io::Write` are re-exports of `std::io` (real interop for `StreamingDecoder`/`StreamingEncoder`) + `std` error impls; enables `compress_parallel` (`std::thread`) |
| `alloc` | implied by `std` | the decode/encode core + dictionaries + streaming over a small `no_std` `io` shim |

```sh
cargo build                                        # std (default)
cargo build --no-default-features --features alloc # no_std
```

The full codec (including `StreamingDecoder`/`StreamingEncoder`) is available
under `no_std + alloc`; `compress_parallel` is the only `std`-gated entry point.
A `thumbv7em-none-eabi` build is the recommended bare-metal CI gate.

## Security

See [`SECURITY.md`](SECURITY.md). In short: the codec forbids `unsafe`, the
decoder is fuzzed to never panic/OOM on hostile input, and `decompress_capped` /
the streaming window cap bound memory against decompression bombs and oversized
windows. Report vulnerabilities via GitHub's private security advisories.

## License

MIT.
