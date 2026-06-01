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
- [ ] Robustness harness (decodecorpus matrix + fuzz) — T1.5

### Encoder
- [ ] FSE / Huff0 entropy encoders — T2.1
- [ ] Frame + block writer (raw/RLE/compressed, magicless) — T2.2
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
