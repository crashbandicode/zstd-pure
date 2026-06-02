# zstd-pure — pure-Rust Zstandard codec

A from-scratch Zstandard ([RFC 8478]) implementation, written from the spec
(no GPL code). The crate depends only on `core`/`alloc`/`std` + `thiserror`;
libzstd (the `zstd` crate) is a **dev-only test/bench oracle**, never used at
runtime. Crate name `zstd-pure`, library `zstd_pure`.

Extracted with full history from the `nx-layout-toolbox` (Toolbox-Cli) monorepo,
where it was built bottom-up and validated against libzstd and real Nintendo
TotK BFRES frames (themselves standard magicless zstd).

[RFC 8478]: https://www.rfc-editor.org/rfc/rfc8478

## Feature checklist

### Decoder
- [x] Standard frames (magic, multi-frame, skippable frames)
- [x] Magicless frames (`ZSTD_f_zstd1_magicless`)
- [x] Raw / RLE / Compressed blocks
- [x] Huff0 literals (1- and 4-stream; FSE-coded + direct weights; treeless reuse)
- [x] FSE sequences (predefined / RLE / FSE / repeat modes) + repeat offsets
- [x] XXH64 content checksum verification
- [x] Output ceiling (`decompress_capped`) against decompression bombs
- [x] Frame inspection without decoding the body (`frame_header`) — **T1.4**
- [x] Dictionary decode (raw-content + structured/tagged) — **T1.1**
- [x] Streaming / bounded-memory sliding-window decode + `io::Read` — **T1.2** (std-only)
- [x] `no_std` + `alloc` (behind a default `std` feature) — **T1.3**
- [x] Robustness harness (corpus matrix + randomized never-panic + oracle) — **T1.5**

### Encoder
- [x] Huff0 literal encoder (length-limited Huffman, 1-/4-stream, direct + FSE-coded weights) — **T2.1a / T2.1b**
- [x] FSE entropy encoder (`normalize_counts` + `write_ncount` + `build_ctable` + 2-state `encode`) — **T2.1b** (sequence encoding wired in T2.3)
- [x] Frame + block writer — store mode (raw/RLE) + Huffman-literals compressed block, magicless — **T2.2 / T2.1a** (real sequences land with T2.3)
- [x] Sequence-section encoder (3-state interleaved FSE, predefined tables) — **T2.3**
- [x] Match finder — `fast` strategy + full compressed-block assembly + `compress(data, level)` — **T2.3** (dfast/lazy/btopt + per-block FSE tables are ratio follow-ups)
- [x] Repeat-offset codes in the `fast` finder (offset_value 1–3, cross-block `rep` threading) — **T2.3 (ratio)**
- [ ] Stronger strategies (dfast/lazy/btopt) + per-block FSE sequence tables — T2.3 (ratio)
- [ ] Long-distance matching — T2.4
- [ ] Dictionary encode + tagged-dictionary training — T3.1

## Features / `no_std`

| feature | default | effect |
|---|---|---|
| `std` | on | `std::io::Read` `StreamingDecoder` + `std` error impls |
| `alloc` | (implied by `std`) | the decode/encode core + dictionaries |

The codec runs on `no_std + alloc`:

```sh
cargo build --no-default-features --features alloc     # no_std
cargo build                                            # std (default)
```

Under `no_std` everything is available except `StreamingDecoder` (it builds on
`std::io::Read`; add a `no_std` `Read` shim to expose it). Verified with the
host `no_std` build (a `#![no_std]` crate fails to compile on any stray `std::`
path); a `thumbv7em-none-eabi` build is the recommended CI gate.

## Validation

`our.decompress(x) == libzstd.decompress(x)` across input profiles × levels
{1,3,9,19}, empty/tiny, content-checksum frames, and every real TotK BFRES
frame (`tests/zstd_pure_bfres.rs`, fixture-gated). For the encoder (when it
lands): `libzstd.decompress(our_compress(x)) == x` and
`our.decompress(our_compress(x)) == x`.

## Module layout

| module | responsibility |
|---|---|
| `mod` | public API + libzstd round-trip tests |
| `error` | `ZstdError` (std + thiserror only) |
| `bits` | reverse `BIT_DStream`-faithful reader + forward reader |
| `xxhash` | XXH64 (content checksum) |
| `fse` | `read_ncount` + `build_dtable` + 2-state decompress + `FseDecoder` |
| `huff` | Huff0 weight decode + 1-/4-stream literal decode |
| `literals` | Raw/RLE/Compressed/Treeless literal sections |
| `sequences` | LL/OF/ML FSE + repeat offsets + LZ execution |
| `block` | block header + raw/RLE/compressed block decode |
| `frame` | frame header + block loop + skippable + checksum + dict priming |
| `dict` | raw-content + structured (tagged) dictionary parse |
| `streaming` | block-by-block bounded-memory decode + `io::Read` (`StreamingDecoder`) |
| `encode` | encoder: `huff` (Huff0 literal encoder), `fse` (FSE entropy encoder), `sequences` (sequence-section encoder), `lz` (fast match finder), `bitstream` (shared `BIT_CStream` writer), `block`/`frame` writers + `compress` |

## Handoff — remaining work (notes for the next agent)

State at this point: the **decoder** is at ecosystem parity (multi-frame,
magicless, dictionaries [raw + tagged], streaming/bounded-memory, frame
inspection) and hardened by a randomized never-panic + libzstd-oracle harness.
The **encoder** now compresses for real: Huff0 literals (T2.1a), the full FSE
entropy coder (T2.1b), the three-state sequence bitstream, a `fast` match
finder, and a `compress(data, level)` entry that emits compressed/raw/RLE blocks
(T2.3 core). Every encoder output is verified through **both libzstd and our own
decoder** (lib unit tests + the corpus harness's encoder sweep).

**Remaining is ratio engineering, not new correctness surface:**
- **Stronger parses** — `dfast` (double hash, L2–3), `lazy`/`lazy2` (hash-chain +
  lazy eval, L4–12), `btopt`/`btultra` (binary tree + optimal parse, L13–22), wired
  to the zstd level→param table so `level` actually selects a strategy.
- **Per-block FSE sequence tables** (`sequences` table mode 2) + RLE/repeat modes
  + a real `FSE_optimalTableLog`, instead of always-predefined tables.
- **Benchmark** (`benches/`, criterion vs the `zstd` crate) + `BENCHMARKS.md`;
  the bar is beating ruzstd/structured-zstd ratio at L3+.
- **T2.4 LDM**, and the **T1.3 no_std** gating (deferred to crate extraction —
  see below).

### T2.1 entropy encoders
- **Huff0 encoder — DONE (T2.1a).** `encode/huff.rs`: length-limited Huffman
  (heap-Huffman + JPEG/zlib count-redistribution to ≤ 11 bits), weights derived
  and codes read back from the decoder's own `huff::build_table` (so encode and
  decode can't drift), `BIT_CStream`-style reverse-order bitstream, 1- and
  4-stream forms, **direct** weight header (max-symbol ≤ 128). `encode/block.rs`
  `write_huffman_literals_block` wraps `[Huffman literals][0 sequences]`;
  `encode/frame.rs` `compress_huffman_literals` builds a frame, per-block picking
  the smaller of Huffman vs store (so it never beats `compress_store` by size and
  falls back cleanly on the >128-symbol / FSE-weights case). Verified: full-frame
  round-trip through **libzstd** and our decoder across sizes/alphabets, plus
  sub-stream round-trips through the decoder.
- **FSE encoder — DONE (T2.1b).** `encode/fse.rs`: `normalize_counts` (valid
  proportional normalization), `write_ncount` (faithful inverse of
  `read_ncount`), `build_ctable` (inverse of `build_dtable`), and the 2-state
  `encode` (inverse of `decompress`). Wired into the Huff0 encoder as
  FSE-compressed weights (`encode/huff.rs::write_weight_header_fse`) so
  full-byte-alphabet (highest symbol > 128) literals compress; verified through
  libzstd. The remaining FSE consumer is the sequence section (T2.3). Notes from
  the original handoff on the FSE encoder pieces:
- **(historical T2.1b plan)** — sequences + general (full-byte alphabet)
  Huffman weights. Only oracle is our own
  `fse::decompress` (no libzstd standalone-FSE via the crate), so verify by
  round-trip. Pieces: `normalize_counts` (a *valid* normalization — present
  symbols ≥ 1, sum = `1<<table_log` — suffices; libzstd's exact
  `FSE_normalizeCount` only matters for ratio), `write_ncount` (faithful inverse
  of `fse::read_ncount`: forward LSB writer, per-symbol value `v=count+1`, small
  range `<max` uses `nbBits−1`, else `+max` when `v≥threshold`; `previous0`
  zero-run RLE), `build_ctable` (same symbol spread as `build_dtable`;
  `symbolTT.deltaNbBits/deltaFindState`), and the **2-state** interleaved encode
  matching `fse::decompress`'s `s1`/`s2` init+alternation+reload-driven tail
  (this tail is the fiddly part — iterate against the decoder).

### T2.3 match finders + compressed-block assembly (the ratio work)
- **Sequence encoder — DONE.** `encode/sequences.rs`: 3 interleaved FSE states
  (LL/OF/ML), predefined-table mode, ported from `ZSTD_encodeSequences_body` and
  verified by round-trip through the decoder (600 trials, incl. the `-1`
  low-prob table entries). `offset_value = rep+3` (literal offsets today).
- **`fast` match finder + assembly — DONE.** `encode/lz.rs::fast_parse` (single
  4-byte hash, greedy + overlap-safe extension); `encode/block.rs::
  write_compressed_block` (literals auto raw/Huffman + sequences);
  `encode/frame.rs::compress` per-block picks the smallest of compressed/raw/RLE.
  Acceptance met: `libzstd.decompress(compress(x))==x` and `our.decompress==x`
  across the lib tests + corpus encoder sweep.
- **Repeat-offset codes — DONE.** `encode/lz.rs::fast_parse` detects a found
  match whose offset equals a running repeat offset and emits the rep code
  (`offset_value` 1–3) instead of `offset + 3`, reusing the decoder's own
  `resolve_offset` for the candidate test and the `rep` evolution so encode and
  decode can't drift. `encode/frame.rs::compress` threads `rep` across blocks,
  committing a block's evolution only when its compressed form is chosen (a
  store block leaves `rep` untouched, as the decoder does). Verified through
  libzstd + our decoder, incl. cross-block threading on a >128 KiB structured
  input.
- **Still TODO (ratio):** stronger parses `dfast` (L2–3) → `lazy/lazy2` (L4–12) →
  `btopt/btultra` (L13–22) wired to the zstd level→param table so `level`
  selects a strategy; per-block FSE sequence tables (mode 2) + RLE/repeat modes +
  `FSE_optimalTableLog`; block splitting; a ratio bench in `benches/` (criterion
  vs the `zstd` crate) tracked in `BENCHMARKS.md`.

### T1.3 no_std + alloc
Deferred: `zstd_pure` is currently a *module* of the std crate
`nx-layout-toolbox`, so `--no-default-features --features alloc` / a `thumb`
target can't be exercised here. Real verification arrives with the planned
extraction into a standalone `zstd-pure` crate (also needs a `thiserror` 2.0
bump for `core::error::Error`). Source is already `core`-friendly except
`streaming.rs` (`std::io::Read`) — gate that behind a `std` feature on
extraction and add a no_std `Read` shim.

### Tier 3
Dictionary **encode** + tagged-dict **training** (COVER/fastCover — a genuine
pure-Rust first); perf (sequence decode + reverse bit reader are the hot
spots); criterion benches vs the `zstd` crate.
