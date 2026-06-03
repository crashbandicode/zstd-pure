# zstd-pure — pure-Rust Zstandard codec

A from-scratch Zstandard ([RFC 8878]) implementation, written from the spec
(no GPL code). The crate depends only on `core`/`alloc`/`std` + `thiserror`;
libzstd (the `zstd` crate) is a **dev-only test/bench oracle**, never used at
runtime. Crate name `zstd-pure`, library `zstd_pure`.

Extracted with full history from the `nx-layout-toolbox` (Toolbox-Cli) monorepo,
where it was built bottom-up and validated against libzstd and real Nintendo
TotK BFRES frames (themselves standard magicless zstd).

[RFC 8878]: https://www.rfc-editor.org/rfc/rfc8878

## Conformance

Targets **RFC 8878** (the current Zstandard standard; it obsoletes RFC 8478, and
the wire format is unchanged between them). Every encoder output is validated by
libzstd, which implements RFC 8878. Notable conformance points:

- **Content checksum** — low 4 bytes of `XXH64(data, seed = 0)`, little-endian (§3.1.1).
- **Reserved bit** of the `Frame_Header_Descriptor` is required to be 0; a frame that sets it is rejected (§3.1.1.1.1).
- **Window size** — the encoder caps `Window_Size` at 8 MiB, honoring §3.1.1.1.2's recommendation that a compressor not require more (for broad decoder interoperability); the streaming decoder accepts windows up to 128 MiB, ≥ the 8 MiB a decoder is recommended to support. The opt-in `compress_long` (long-distance matching) is the deliberate exception: to make its long-range matches reachable it may advertise a `Window_Size` up to 128 MiB (log 27) — still within the default `windowLogMax` (27) of a stock `ZSTD_decompress` and of this crate's `StreamingDecoder`, so the frames stay broadly decodable. Plain `compress` is unchanged at 8 MiB.
- **Block_Maximum_Size** — `min(Window_Size, 128 KiB)` (§3.1.1.2).

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
- [x] Streaming / bounded-memory sliding-window decode + `io::Read` — **T1.2** (`std` and `no_std`, via the crate's `io::Read` shim)
- [x] `no_std` + `alloc` (behind a default `std` feature) — **T1.3**
- [x] Robustness harness (corpus matrix + randomized never-panic + oracle) — **T1.5**
- [x] Seekable format (zstd `contrib/seekable_format`): `compress_seekable` (independent per-chunk frames + a seek-table skippable frame) and random access — `SeekTable::parse` + `decompress_seekable_frame`, with optional per-frame `XXH64`. The archive is a conformant multi-frame stream a standard decoder reads end-to-end

### Encoder
- [x] Huff0 literal encoder (length-limited Huffman, 1-/4-stream, direct + FSE-coded weights) — **T2.1a / T2.1b**
- [x] FSE entropy encoder (`normalize_counts` + `write_ncount` + `build_ctable` + 2-state `encode`) — **T2.1b** (sequence encoding wired in T2.3)
- [x] Frame + block writer — store mode (raw/RLE) + Huffman-literals compressed block, magicless — **T2.2 / T2.1a** (real sequences land with T2.3)
- [x] Sequence-section encoder (3-state interleaved FSE, predefined tables) — **T2.3**
- [x] Match finder — `fast` strategy + full compressed-block assembly + `compress(data, level)` — **T2.3** (dfast/lazy/btopt + per-block FSE tables are ratio follow-ups)
- [x] Repeat-offset codes in the `fast` finder (offset_value 1–3, cross-block `rep` threading) — **T2.3 (ratio)**
- [x] Per-block FSE sequence tables — predefined/RLE/FSE (modes 0/1/2), exact-cost per-channel selection, `FSE_optimalTableLog` — **T2.3 (ratio)**
- [x] Level→param table (`encode::params`) + cross-block (persistent-window) matching, so `level` selects the window/hash sizes and back-refs span the 128 KiB block boundary — **T2.3 (ratio)**
- [x] Hash-chain greedy/lazy/lazy2 parser wired to `params.strategy` (levels 4+); `level` now scales ratio — **T2.3 (ratio)**
- [x] `btopt`/`btultra` optimal parse (L13+): rep-aware DP over a fixed-point cost model (`encode::lz::opt_parse_block`); beats libzstd ratio on dense JSON at L19 — **T2.3 (ratio)**
- [x] `dfast` double-hash finder (L2–3): 8-byte long + 4-byte short tables, best-of-two greedy — **T2.3 (ratio)**
- [x] Sequence-table Repeat mode (3): cross-block per-channel FSE table reuse — `write_sequences` threads the previous compressed block's tables (`encode::block::EncState`) and reuses them (no table description) when valid + cheaper — **T2.3 (ratio)**
- [x] Treeless literals: cross-block Huffman table reuse — `write_literals_auto` reuses the previous compressed block's table (literals block type 3, no tree description) when it can encode every byte + is cheaper, threaded via `EncState` — **T2.3 (ratio)**
- [x] Opt price-model refinement (`btultra2` second pass): re-parse with a price model rebuilt from the first parse's *actual* literal/LL/OF/ML statistics instead of the predefined-table prior — tightens the top-level ratio on near-random record data (L19 `records` 1.32× → 1.26×, `json` 0.87× → 0.84×) — **T2.3 (ratio)**
- [x] Block splitting: partition a block into adjacent blocks each with entropy tables fit to its own statistics, when their distributions differ enough to pay for the extra headers — a recursive midpoint split at the optimal-parse levels (L16+), threading Repeat-mode / Treeless tables across the sub-blocks and **kept only when strictly smaller** (never regresses). Stacks on the `btultra2` pass: L19 `records` 1.26× → 1.12×, `json` 0.84× → 0.80×, `3x90k` 1.07× → 1.01×, and a heterogeneous text/JSON block now beats libzstd — **T2.3 (ratio)**
- [x] Binary-tree match finder — chain/tree **hybrid** for the optimal parse (L16+): the hash chain supplies the small-offset Pareto set and a faithful binary tree (`encode::lz::BtState`, a port of zstd's `ZSTD_insertBt*`) contributes its longest match, merged only when it's `≥ sufficient_len` (a committable long match the chain's depth bound missed). Ties the chain-opt on the small corpus (never regresses) and wins where the depth bound binds — e.g. **−16 %** on a 150-revision near-duplicate corpus (`examples/bench_large`) — **T2.3 (ratio)**
- [x] `btlazy2` (L13–15): a lazy2 parse over the same chain/tree hybrid — the chain's recent match, substituted by the tree's longest when it's a longer `≥ target_length` match the (shallow, depth ~16) chain missed. Big wins at L13 where that depth binds: small-corpus `records` 1.14× → **0.96×**, `3x90k` 1.27× → **0.49×**, near-duplicate `revisions` 1.41× → **1.02×**; ties everything else — **T2.3 (ratio)**
- [x] Long-distance matching (`compress_long`, opt-in): a coarse, **content-defined** sparse whole-input index (`encode::lz`/`encode::ldm` — keyed by a 64-byte min-match, inserted/probed where the content hash gates, so a repeat is indexed at the same relative offsets in both copies) contributes long matches at offsets *beyond* the regular 8 MiB window; the parse emits them as ordinary large-offset sequences and fills the gaps with the regular finder, and the frame advertises the larger window they need (up to log 27). The decoder is unchanged. — **T2.4**
- [x] Streaming encoder (`StreamingEncoder`, `encode::stream`) — incremental, block-by-block frame encoding mirroring `StreamingDecoder` / zstd's `ZSTD_compressStream`: `push` bytes in arbitrary chunks, a compressed block is emitted per 128 KiB, the **unknown-content-size** frame header is written up front, and the XXH64 checksum on `finish`. Reuses the `compress` block loop (`Finder` + `EncState` + per-block store fallback) unchanged, parsing each block against only the input committed up to its boundary — so the produced frame is **byte-identical regardless of the write-chunk sizes** and decodes through our one-shot decoder, our `StreamingDecoder`, and libzstd. First cut retains all input (bounded memory, `io::Write`, and streaming+LDM are follow-ups) — **T4.1a**
- [x] Dictionary encode (`compress_with_dict`) — raw + structured/tagged: match window primed with dict content, seeded repeat offsets, dict-id frame header; verified through libzstd + our decoder, improves ratio on a many-small-files corpus — **T3.1**
- [x] Dictionary training (`train_dictionary`) — pure-Rust greedy COVER producing a raw-content dictionary (highest-coverage shared substrings, most-valuable last); improves ratio on a many-small-files corpus, verified through libzstd + our decoder — **T3.1**
- [x] Structured/tagged-dictionary finalize (`train_dictionary_structured`) — `magic | id | Huffman | FSE OF/ML/LL | repeat offsets | content`, entropy tables derived from a dict-primed compression pass over the samples; **libzstd loads it on the strict compress side** and a decoder warm-starts from it — **T3.1**

## Features / `no_std`

| feature | default | effect |
|---|---|---|
| `std` | on | `io::Read` becomes a re-export of `std::io::Read` (real `std::io` interop for `StreamingDecoder`) + `std` error impls |
| `alloc` | (implied by `std`) | the decode/encode core + dictionaries + `StreamingDecoder` over a `no_std` `io::Read` shim |

The codec runs on `no_std + alloc`:

```sh
cargo build --no-default-features --features alloc     # no_std
cargo build                                            # std (default)
```

The full API is available under `no_std`, including `StreamingDecoder`: it
implements the crate's own `io::Read` trait, which is a re-export of
`std::io::Read` under `std` and a minimal owned-error shim otherwise (consumers
without `std` loop on `read` until it returns 0). Verified with the host
`no_std` build (a `#![no_std]` crate fails to compile on any stray `std::`
path); a `thumbv7em-none-eabi` build is the recommended CI gate.

## Validation

`our.decompress(x) == libzstd.decompress(x)` across input profiles × levels
{1,3,9,19}, empty/tiny, content-checksum frames, and every real TotK BFRES
frame (`tests/zstd_pure_bfres.rs`, fixture-gated). For the encoder, every output
is checked both ways — `libzstd.decompress(our_compress(x)) == x` and
`our.decompress(our_compress(x)) == x` — across levels 1–22, the randomized
corpus sweep, and cross-block cases. A `proptest` property suite
(`tests/proptest.rs`) adds *shrinking* coverage of the same invariants — the
decoder never panics on arbitrary or corrupted frames, the encoder round-trips
both ways across the level range, and the libzstd oracle — so any future
regression lands as a minimal reproducer.

A `cargo fuzz` crate (`fuzz/`, nightly) adds adversarial coverage with three
libFuzzer targets: `decode` (every one-shot/streaming, magic/magicless decode
path must never panic or OOM under a 64 MiB output cap), `encode_roundtrip`
(compress arbitrary input at any level, then require both decoders to recover
it), and `decode_diff` (a differential that requires our decoder and libzstd to
agree on the output of any frame *both* accept). The differential is scoped to
our RFC 8878 surface — libzstd is built without the `legacy` feature and capped
at `window_log` 27 — and asserts only output equality on jointly-accepted
frames, because libzstd's streaming decoder is deliberately lenient about some
malformed frames (e.g. a `Frame_Content_Size` that disagrees with the actual
content) that ours rejects, as libzstd's own one-shot API does. Run with
`cargo +nightly fuzz run <target>`, seeding `fuzz/corpus/<target>/` with real
frames for depth.

A fixture-gated corpus test (`tests/real_corpus.rs`, `#[ignore]`) walks a
directory of real files named by `$ZSTD_PURE_CORPUS` and round-trips each one
*both ways* across a few levels — our encode → our + libzstd decode, and libzstd
encode → our decode — tracking the aggregate ratio. It stays offline by default
(plain `cargo test` skips it). Point it at the Silesia corpus or the TotK BFRES
production data, e.g. `ZSTD_PURE_CORPUS=~/fixtures/silesia cargo test --release
real_corpus -- --ignored --nocapture` (knobs: `ZSTD_PURE_CORPUS_LEVELS`,
`ZSTD_PURE_CORPUS_MAX_MB`).

Ratio + throughput vs libzstd are tracked in [`BENCHMARKS.md`](BENCHMARKS.md).

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
| `seekable` | seekable format (`contrib/seekable_format`): `compress_seekable` + `SeekTable` random access |
| `io` | `Read` trait + `Error`/`ErrorKind`/`Result`: a re-export of `std::io` under `std`, a small `no_std` shim otherwise |
| `encode` | encoder: `huff` (Huff0 literal encoder), `fse` (FSE entropy encoder), `sequences` (per-block mode-selecting sequence encoder), `lz` (fast/dfast/chain match finders + chain/tree-hybrid optimal parse with block splitting + dictionary priming), `params` (level→cparams table), `bitstream` (shared `BIT_CStream` writer), `train` (pure-Rust COVER dictionary trainer + structured/tagged finalize), `stream` (incremental `StreamingEncoder` — `ZSTD_compressStream` analogue), `block`/`frame` writers + `compress` / `compress_with_dict` |

## Handoff — remaining work (notes for the next agent)

> **The current task list and execution model live in [`HANDOFF.md`](HANDOFF.md)**
> (overnight autonomous run: streaming encoder, LDM decode-side, multithreading, an
> enwik8/enwik9 corpus, then the `records` cost-model lever and a `ruzstd` perf
> comparison). Production-grade testing (proptest + cargo-fuzz + a fixture-gated
> real-world corpus) and T2.4 long-distance matching (`compress_long`) are **done**.
> The notes below are background on how the decoder/encoder got here.

State at this point: the **decoder** is at ecosystem parity (multi-frame,
magicless, dictionaries [raw + tagged], streaming/bounded-memory, frame
inspection) and hardened by a randomized never-panic + libzstd-oracle harness.
The **encoder** now compresses for real: Huff0 literals (T2.1a), the full FSE
entropy coder (T2.1b), the three-state sequence bitstream, a `fast` match
finder, and a `compress(data, level)` entry that emits compressed/raw/RLE blocks
(T2.3 core). Every encoder output is verified through **both libzstd and our own
decoder** (lib unit tests + the corpus harness's encoder sweep).

**Remaining is ratio engineering, not new correctness surface:**
- **Stronger parses** — all five strategy classes are in place
  (`encode::params`, `lz::{MatchState, DFastState, ChainState, Finder,
  opt_parse_block}`): `fast` (L1), `dfast` (double-hash, L2–3),
  `greedy`/`lazy`/`lazy2` (hash-chain, L4–12), and the `btopt`/`btultra`
  rep-aware optimal parse (L13+). `level` scales ratio: a record stream
  1.87× → 1.02× of libzstd (L1 → L3 via dfast); dense JSON now **beats** libzstd
  at L19 (0.80×). The opt parse then gained libzstd's `btultra2` second pass
  (re-pricing from the first parse's *actual* statistics), block splitting (L16+),
  and a chain/tree **hybrid** match finder (L16+) — together taking the top-level
  soft spot from L19 `records` 1.32× → 1.12× and pushing `json` to 0.80×.
  **Remaining tuning:** the last `records` gap is the *cost model* (rep-offset
  candidates priced with the per-cell `rep`), not the match finder — see §2.3.
- **Sequence-table Repeat mode (3) — DONE.** `write_sequences` reuses the
  previous compressed block's LL/OF/ML table (mode 3, no table description) when
  it can encode this block's codes and beats re-describing; `encode::block::EncState`
  threads the tables across blocks with the same commit-on-emitted-compressed-block
  discipline as the repeat offsets. The win is on small blocks (a large block
  amortizes its table header anyway). Treeless literals (the literals analogue,
  block type 3) are likewise done, and a dict-primed compression now seeds block
  1's `EncState` (literals Huffman + sequence FSE tables) from a structured
  dictionary — so a small file warm-starts via Treeless / Repeat instead of
  re-describing tables. Cross-block reuse is complete.
- **Benchmark — DONE.** `benches/compression.rs` (criterion throughput) +
  `examples/ratio.rs` (size) vs the `zstd` crate, documented in `BENCHMARKS.md`.
- **T2.4 LDM** (long-distance matching) — **DONE** (opt-in `compress_long`); see
  the T2.4 section below. (**T1.3 no_std** — including the streaming decoder over
  the crate's `io::Read` shim — is now done.)

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
- **Per-block FSE sequence tables — DONE.** `encode/sequences.rs::write_sequences`
  histograms each LL/OF/ML channel and picks the cheapest of Predefined (mode 0),
  RLE (mode 1), and a per-block FSE table (mode 2) by *exact* bitstream cost
  (`FseCTable::stream_cost_bits`), with a faithful `FSE_optimalTableLog`. The
  three channels' state bits are independent, so per-channel selection minimizes
  the whole section and the result is provably never larger than the
  predefined-only encoding (asserted in tests). Verified through libzstd + our
  decoder.
- **Level→param table + cross-block matching — DONE.** `encode/params.rs` ports
  libzstd's default cparams (window/hash/chain/search/min_match/target_length/
  strategy) per level + the `ZSTD_adjustCParams` small-input window shrink.
  `encode/lz.rs` now carries a persistent `MatchState` across a frame's blocks
  (`parse_block` parses one block's range against the whole input), so
  back-references span the 128 KiB block boundary up to the level-selected
  window, and `compress` emits that window log in the frame header. The match
  table needs no rollback on store blocks (their bytes remain in the decoder's
  output). Verified through libzstd (offsets stay in-window) + our decoder, incl.
  a 270 KiB cross-block-repeat case.
- **Greedy/lazy/lazy2 parser — DONE.** `encode/lz.rs::lazy_parse_block` +
  `ChainState` (head + chain tables) walk up to `1 << search_log` candidates per
  position keeping the longest match, with `lazy_steps` look-ahead (0 greedy / 1
  lazy / 2 lazy2). `Finder::new(params)` dispatches by strategy: `Fast` →
  single-slot, `Dfast` → double-hash, greedy/lazy/lazy2 → the chain finder,
  `bt*` → the optimal parse. `level` now scales ratio (a record stream:
  1.87× → 0.97× of libzstd, L1 → L6; a 270 KiB cross-block repeat: 7869 → 1450
  bytes, L3 → L19 ≈ libzstd). Verified across levels 1–22 through libzstd + our
  decoder, incl. a >128 KiB input.
- **Optimal parse (`btopt`/`btultra`/`btultra2`) — DONE.**
  `encode/lz.rs::opt_parse_block`: a rep-aware dynamic program over a fixed-point
  (`log2_fp`) cost model. The chain finder enumerates the Pareto match set per
  position (`find_matches`); the DP (`run_dp`) carries per-position price +
  repeat-offset state + a backpointer and picks the globally cheapest
  literal/match sequence (short-now-for-longer-later, rep matches priced cheap via
  their tiny offset code). `depth`/`sufficient_len` capped for tractability.
  `Finder::Opt` handles `Btopt`/`Btultra`/`Btultra2`. **`btultra2` second pass:**
  candidate matches are collected once by walking the chain, then the DP runs
  twice — pass 1 with the predefined-table prior (`Prices::predef`, identical to
  the single-pass output), pass 2 (only for `Btultra2`, L19+) re-priced from pass
  1's *actual* literal/LL/OF/ML statistics (`Prices::from_stats`), so the parse
  optimizes against the per-block FSE tables `write_sequences` really builds. The
  match set is unchanged between passes, so the second pass costs only another DP,
  not another search. Beats libzstd ratio on dense JSON at L19 (0.84×) and
  tightened the near-random `records` soft spot (1.32× → 1.26×); verified across
  L1–22 through libzstd + our decoder.
- **`dfast` double-hash finder — DONE.** `encode/lz.rs::dfast_parse_block` +
  `DFastState`: a `long` table keyed by an 8-byte hash (preserves long-match
  candidates the 4-byte table would overwrite) plus a `short` 4-byte table;
  greedy best-of-two per position. `Finder::DFast` handles `Dfast` (L2–3). Big
  L3 wins: a record stream 2668 → 1681 bytes (≈ libzstd), a cross-block repeat
  7869 → 3841. Verified through libzstd + our decoder.
- **Binary-tree match finder — DONE (chain/tree hybrid).** Five variants were
  built before one earned merging, each measured against the tuned chain-opt:
  1. *bounded* (extension-capped + early-break): regressed `json` ~3 % and slower;
  2. *faithful + skip-no-insert* (full extension, sparse index): regressed
     everything (`records` 1375→1645, `3x90k` 1503→1966);
  3. *complete index + capped insertion*: matched the chain on
     `records`/`3x90k`/`redundant` but `json` ~3 % worse, ~2× slower;
  4. *faithful port* (branch `experiment/bt-finder`): a from-scratch port of
     zstd's `ZSTD_insertBt*` — complete index, **uncapped** search extension via
     the `commonLength` bound, window/`btLow` handling, only the insert-only path
     (skipped positions) capped to bound the periodic-data O(n²) blowup. The first
     to be both correct and tractable, but it *replaces* the chain's small-offset
     set, so it still lost `json` (+3 %): the chain walks newest-first → smallest
     offset per length, which our cost model prefers.
  5. *chain/tree hybrid* — **shipped** (`encode::lz`: `Finder::Opt` carries both a
     `ChainState` and a `BtState`). Keep the chain's small-offset Pareto set and
     *add* the tree's longest match, but **only when `≥ sufficient_len`** (a
     committable long match the chain's depth bound missed). Merging *shorter*
     tree matches re-introduces the `json` regression — the DP minimises a
     predefined-price *proxy*, not the real FSE cost, so a longer/larger-offset
     match looks cheap but isn't — so the restriction to long matches keeps the
     chain's cheap small-offset matches intact. It **ties the small corpus
     exactly** (never regresses) and **wins where the chain's depth bound binds**:
     on a 150-revision near-duplicate corpus (`examples/bench_large`) it is
     **−16 %** (1.300× → 1.097× of libzstd at L19), the tree's recency-independent
     reach resolving the bound. Gated to L16+ (the optimal-parse tier), where the
     cost — the tree's memory + up to ~2× match time, correlated with the benefit —
     is acceptable.
  **What this taught us:** the chain indexes *every* position, so it finds a
  far-back match through *any* distinctive 4-byte window — its depth bound only
  hides matches whose entry hashes are *saturated* by recent collisions, i.e. the
  high-candidate-count case (near-duplicate / revision data) the hybrid now
  catches. On the ≤270 KB synthetic corpus that case doesn't arise, which is why
  variants 1–4 (and the hybrid *there*) only tied — the win needed a bigger,
  many-candidate input to surface. The remaining `records` lever is the **cost
  model** (rep-offset candidates priced with the per-cell `rep`, which the
  collect-then-DP split the `btultra2` two-pass needs can't yet supply), not the
  match finder.
- **`btlazy2` (L13–15) — DONE.** A lazy2 parse over the same chain/tree hybrid
  (`bt_lazy_parse_block`): the chain's recent match, substituted by the tree's
  longest when it's a longer `≥ target_length` match the chain missed. At L13 the
  chain's depth is shallow (~16), so the tree adds a lot even on the small corpus
  (`records` 1.14× → 0.96×, `3x90k` 1.27× → 0.49×) and on near-duplicate
  `revisions` (1.41× → 1.02×); ties elsewhere, never regresses.

### T2.4 Long-distance matching (LDM) — DONE

**What shipped.** The opt-in `compress_long` enables LDM. `encode::ldm::LdmState`
is a coarse, **content-defined** sparse index over the whole input (64-byte
min-match; one entry per content-gated point — so a repeat is indexed at the same
relative offsets in both copies, making detection independent of alignment).
`encode::lz::parse_with_ldm` injects the long matches it finds into the parse as
ordinary large-offset sequences and fills the gaps with the regular finder;
`params::params_for_level_ldm` grows the advertised window to cover the input, up
to `LDM_MAX_WINDOW_LOG` (27); the block writer reuses the existing `emit_split`
path unchanged (LDM matches are just sequences, so block splitting + per-channel
entropy-table selection handle them, and a large offset's `of_code` stays within
the predefined OF table). The decoder is untouched. v1 simplifications left as
refinements: forward-only extension (no backward extension into the preceding
literals), per-block match generation (a match is clamped at the block boundary),
and no LDM under `compress_with_dict`.

**What it is.** Matches at offsets *beyond* the regular window. Our window caps at
`window_log` 23 (8 MiB) for portability, and every match finder (chain, dfast,
the binary tree) only sees candidates within `max_offset = 1 << window_log`. LDM
adds a **coarse, whole-input index** that finds *long* matches at much larger
distances (up to libzstd's 128 MiB window) — for big inputs with repeats spaced
farther apart than the window can reach. It is **complementary to the binary-tree
hybrid**: the tree finds far matches *within* the window (it resolved the
near-duplicate `revisions` win once copies fit the window); LDM extends reach
*beyond* the window, for inputs larger than ~8 MiB.

**How libzstd does it** (`lib/compress/zstd_ldm.c`). A *secondary* hash table
keyed by a long min-match (`ldmMinMatch`, default 64 B), with **one entry every
`1 << hashRateLog` positions** — sparse, so the index over the whole input stays
cheap. `ZSTD_ldm_generateSequences` scans the input, probes the coarse hash,
verifies + extends candidate long matches, and emits a list of LDM sequences
(literal gaps + long matches). Those are then handed to the normal block parser,
which fills the gaps with its regular (chain/tree) matches and emits the LDM long
matches as sequences with large offsets.

**Integration points here.**
- `encode::params`: enable LDM at large inputs / high levels (libzstd auto-enables
  it for `windowLog > 27` or via `ZSTD_c_enableLongDistanceMatching`); add the LDM
  params (`hashLog`, `minMatch` = 64, `bucketSizeLog`, `hashRateLog`).
- **Window / conformance** — the gating decision. LDM offsets exceed 8 MiB, so the
  frame must advertise a larger `window_log`. `params::MAX_WINDOW_LOG` is currently
  23 (the portable cap honoring RFC 8878 §3.1.1.1.2). LDM needs it raised toward 27
  (libzstd's default `windowLogMax`); our `StreamingDecoder` already admits up to
  128 MiB and a stock `ZSTD_decompress` supports log 27 by default, so it stays
  interoperable — but it's a deliberate conformance bump, so document it alongside
  the existing window note in the Conformance section and likely gate LDM behind an
  opt-in (don't silently widen every frame's window).
- `encode::lz` / `encode::frame`: thread the LDM long matches into the parse. The
  simplest design mirrors libzstd — an `LdmState` (the coarse hash) updated across
  the input; before parsing each block, generate the LDM long matches in its range
  and have the parser emit them, filling the gaps with the regular finder. Offsets
  can exceed the per-block `max_offset`, so the window/offset checks must use the
  LDM window; `encode_offset` / `of_code` already handle arbitrarily large offsets
  (`offset + 3`, `highbit32`). `rep` threading is unchanged.

**Gotchas.** Keep the coarse index sparse (insert every `1 << hashRateLog`, *not*
every position — min-match 64 indexed densely is wasteful). Verify each LDM match
actually reaches `minMatch` and its offset is within the advertised window. The
**decoder needs no changes** — LDM is purely an encoder concern; the decoder
already copies any in-window offset, and the streaming decoder's `window_log_max`
already admits the larger window.

**Verification (done).** Lib tests: `encode::ldm` finds a far-spaced repeat and
every match it generates is valid (in-window, byte-exact, non-overlapping);
`compress_long` round-trips ordinary inputs through libzstd + our decoder across
levels; and on truly-random data with a duplicate spaced > 8 MiB apart it
produces a clearly smaller frame than `compress` (which can't reach past its
window), with both decoders reconstructing it. Real-world ratio on the Silesia
corpus is tracked in [`BENCHMARKS.md`](BENCHMARKS.md).

### T1.3 no_std + alloc — DONE
Now a standalone crate, so `cargo build --no-default-features --features alloc`
is part of the baseline and exercises the whole codec under `no_std` (a
`#![no_std]` crate fails to compile on any stray `std::` path). `thiserror` 2.0
supplies `core::error::Error`. The last `std`-only holdout, `streaming.rs`, is
un-gated: `StreamingDecoder` implements the crate's own `io::Read` (a re-export
of `std::io::Read` under `std`, a minimal owned-error shim — `io::{Read, Error,
ErrorKind, Result}` — otherwise). A `thumbv7em-none-eabi` build remains the
recommended CI gate for a true bare-metal target.

### Tier 3
Dictionary **encode** — **DONE.** `encode::frame::compress_with_dict` primes the
match finder with the dictionary content (a `Finder::prime` mirroring libzstd's
`ZSTD_loadDictionaryContent`: every dict position indexed, no sequences emitted),
parsing the combined `[dict || input]` buffer so back-references reach into the
dictionary. Handles both flavours: raw-content (default repeat offsets, no
dict-id) and structured/tagged (the dict's three repeat offsets seed the running
`rep`, and the dict id is written to the frame header). A structured dictionary's
preset entropy tables now seed block 1: the literals Huffman table (rebuilt as an
encode table from the dict's decode table) and the three sequence FSE tables
(rebuilt from the dict's normalized counts) become block 1's `EncState`, so a
small file warm-starts via Treeless literals + Repeat-mode sequence tables rather
than re-describing tables. Verified through libzstd (loaded with the same dict)
and our own decoder across levels 1–22, plus checks that this shrinks the block
bodies on a many-small-files corpus. Caveat: the structured dict's 4-byte
`Dictionary_ID` in each frame header is a fixed per-frame cost that, on very
small files, can exceed the warm-start savings (the id is a feature — mismatch
detection — not free; libzstd makes it optional too).

Dictionary **training** — **DONE (raw-content).** `encode::train::train_dictionary`
is a pure-Rust greedy **COVER** (the core of libzstd's
`ZDICT_trainFromBuffer_cover`): an 8-byte-dmer frequency map counted once per
sample, then repeated selection of the highest-coverage segment with the covered
dmers zeroed after each pick, concatenated most-valuable-last. It produces a
raw-content dictionary (wrap with `Dictionary::raw`/`parse`); verified to improve
ratio on a many-small-files corpus and to round-trip through libzstd + our
decoder. The single-pool greedy omits COVER's epoch partitioning and `(d, k)`
parameter search.

Structured/tagged **finalize** — **DONE.** `encode::train::train_dictionary_structured`
is a pure-Rust analogue of libzstd's `ZDICT_finalizeDictionary` on top of the
COVER content: it gathers entropy statistics from a representative pass (each
sample compressed with the dictionary content primed, as it will be used), builds
the literals Huffman table (reusing `encode::huff`'s weight-header writer) and the
three sequence FSE tables (reusing `encode::sequences`' `write_ncount` path, with
a predefined per-channel fallback), and emits the zstd dictionary layout
`magic | dict_id | Huffman | FSE OF/ML/LL | 3 repeat offsets | content` with a
deterministic content-hash id. **libzstd loads it on the strict compress side**
(`ZSTD_loadCEntropy` validates every table) and on the decompress side, and a
decoder warm-starts the first block from these tables.

Encoder use of a structured dict's preset entropy tables — **DONE.** A
dict-primed compression seeds block 1's `EncState` from the dictionary: the
literals Huffman table (rebuilt as an encode table from the dict's decode table)
and the three sequence FSE tables (rebuilt from the dict's normalized counts —
kept at parse time because a `-1` low-probability entry can't be recovered from a
decode table alone). So block 1 warm-starts via Treeless literals + Repeat-mode
sequence tables, shrinking the block bodies on small files; verified through
libzstd + our decoder.

Still open in Tier 3: perf (sequence decode + the reverse bit reader are the
decode hot spots; the chain walk / opt DP + the L16+ tree are the encoder's) and
a decode-speed comparison vs the pure-Rust `ruzstd`. The encoder + decoder both
fully consume structured dictionaries.
