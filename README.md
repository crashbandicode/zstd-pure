# zstd_pure — pure-Rust Zstandard codec

A from-scratch Zstandard ([RFC 8478]) implementation, written from the spec
(no GPL / Switch-Toolbox code) and structured so it can be lifted into a
standalone `zstd-pure` crate (depends only on `core`/`alloc`/`std` +
`thiserror`). libzstd (the `zstd` crate) is used only as the **test/bench
oracle**, never at runtime.

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
- [x] Streaming / bounded-memory sliding-window decode + `io::Read` — **T1.2**
- [ ] `no_std` + `alloc` (behind a default `std` feature) — T1.3
- [x] Robustness harness (corpus matrix + randomized never-panic + oracle) — **T1.5**

### Encoder
- [x] Huff0 literal encoder (length-limited Huffman, 1-/4-stream, direct + FSE-coded weights) — **T2.1a / T2.1b**
- [x] FSE entropy encoder (`normalize_counts` + `write_ncount` + `build_ctable` + 2-state `encode`) — **T2.1b** (sequence encoding wired in T2.3)
- [x] Frame + block writer — store mode (raw/RLE) + Huffman-literals compressed block, magicless — **T2.2 / T2.1a** (real sequences land with T2.3)
- [x] Sequence-section encoder (3-state interleaved FSE, predefined tables) — **T2.3**
- [x] Match finder — `fast` strategy + full compressed-block assembly + `compress(data, level)` — **T2.3** (dfast/lazy/btopt + per-block FSE tables are ratio follow-ups)
- [ ] Stronger strategies (dfast/lazy/btopt) + per-block FSE sequence tables + repeat offsets — T2.3 (ratio)
- [ ] Long-distance matching — T2.4
- [ ] Dictionary encode + tagged-dictionary training — T3.1

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
inspection) and hardened by a randomized never-panic + libzstd-oracle harness
(which already caught two corrupt-input panics). The **encoder** has a verified
store-mode skeleton (`encode::compress_store`). The big remaining piece is the
ratio-competitive compressed path.

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
- sequence encoding: 3 interleaved FSE states (LL/OF/ML), offset = `rep+3`
  with repeat-offset codes; literals via the Huff0 encoder.
- match finders by level: `fast` (L1) → `dfast` (L2–3) → `lazy/lazy2` (L4–12) →
  `btopt/btultra` (L13–22); the zstd level→param table.
- block-type + literal-mode selection; block splitting.
- Acceptance: `libzstd.decompress(our_compress(x,lvl))==x` and
  `our.decompress(our_compress(x,lvl))==x` per level; ratio bench in `benches/`.

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
