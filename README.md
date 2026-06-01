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
- [ ] FSE / Huff0 entropy encoders — T2.1
- [x] Frame + block writer — store mode (raw/RLE), magicless — **T2.2** (compressed block type lands with T2.3)
- [ ] Match finders by strategy (fast/dfast/lazy/btopt) — T2.3
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
| `encode` | encoder: `block`/`frame` writers (store mode today: raw/RLE, magicless) |

## Handoff — remaining work (notes for the next agent)

State at this point: the **decoder** is at ecosystem parity (multi-frame,
magicless, dictionaries [raw + tagged], streaming/bounded-memory, frame
inspection) and hardened by a randomized never-panic + libzstd-oracle harness
(which already caught two corrupt-input panics). The **encoder** has a verified
store-mode skeleton (`encode::compress_store`). The big remaining piece is the
ratio-competitive compressed path.

### T2.1 entropy encoders (next)
- **Huff0 encoder** is the lower-risk one to do first because libzstd is a
  *direct* oracle: build a single compressed block = `[Huffman literals][0
  sequences]`, wrap in a frame, and assert `zstd::bulk::decompress` returns the
  literals. Plan:
  1. length-limited Huffman code lengths (≤11). Either package-merge or
     heap-Huffman + the zlib `gen_bitlen` overflow repair. Must yield a
     *complete* prefix code (Kraft sum = 1) so `huff::build_table`'s residual is
     a power of two.
  2. weights `w_s = max_bits + 1 − len_s` (0 = absent). **Reuse the decoder's
     `huff::build_table(weights)`** to get `symbols[]`/`num_bits[]`, then invert:
     each symbol's canonical code = `first_index_of(s) >> (max_bits − nb_s)`.
     This guarantees consistency with our (libzstd-validated) decoder.
  3. bitstream: mirror libzstd `BIT_CStream` — `addBits(code, nb)` per symbol in
     **reverse** data order, then a `1` sentinel bit, flushing LE. (Pairs with
     our `ReverseBitReader`: `addBits(v,nb)` on encode ↔ `read(nb)==v` on decode
     when fields are processed in reverse.)
  4. weight header: **direct** form (`byte = 127 + N`, 4-bit packed, N = highest
     symbol index) only covers alphabets with max-symbol ≤ 128; the general case
     needs FSE-compressed weights (header byte < 128) → depends on the FSE
     encoder.
- **FSE encoder** (sequences + general weights). Only oracle is our own
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
