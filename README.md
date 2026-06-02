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
- **Window size** — the encoder caps `Window_Size` at 8 MiB, honoring §3.1.1.1.2's recommendation that a compressor not require more (for broad decoder interoperability); the streaming decoder accepts windows up to 128 MiB, ≥ the 8 MiB a decoder is recommended to support.
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
- [ ] binary-tree match finder — T2.3 (ratio)
- [ ] Long-distance matching — T2.4
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
corpus sweep, and cross-block cases. Ratio + throughput vs libzstd are tracked
in [`BENCHMARKS.md`](BENCHMARKS.md).

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
| `io` | `Read` trait + `Error`/`ErrorKind`/`Result`: a re-export of `std::io` under `std`, a small `no_std` shim otherwise |
| `encode` | encoder: `huff` (Huff0 literal encoder), `fse` (FSE entropy encoder), `sequences` (per-block mode-selecting sequence encoder), `lz` (fast/dfast/chain/opt match finders + dictionary priming), `params` (level→cparams table), `bitstream` (shared `BIT_CStream` writer), `train` (pure-Rust COVER dictionary trainer + structured/tagged finalize), `block`/`frame` writers + `compress` / `compress_with_dict` |

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
- **Stronger parses** — all five strategy classes are in place
  (`encode::params`, `lz::{MatchState, DFastState, ChainState, Finder,
  opt_parse_block}`): `fast` (L1), `dfast` (double-hash, L2–3),
  `greedy`/`lazy`/`lazy2` (hash-chain, L4–12), and the `btopt`/`btultra`
  rep-aware optimal parse (L13+). `level` scales ratio: a record stream
  1.87× → 1.02× of libzstd (L1 → L3 via dfast); dense JSON now **beats** libzstd
  at L19 (0.84×). The opt price model gained libzstd's `btultra2` second pass —
  a re-parse priced from the first parse's *actual* literal/code statistics
  rather than the predefined-table prior — which tightened the top-level soft
  spot (L19 `records` 1.32× → 1.26×). **Remaining tuning:** a binary-tree match
  finder would make the optimal parse both faster and deeper.
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
- **Still TODO (ratio):** a binary-tree match finder that **beats** the tuned
  hash-chain opt for the optimal parse. Five variants have now been implemented
  and measured against the chain-opt, and **all lost** (tied at best), so none
  shipped:
  1. *bounded* (extension-capped + early-break): regressed `json` ~3 % and slower;
  2. *faithful + skip-no-insert* (full extension, don't index greedy-match
     interiors to stay O(n)): the sparse index regressed everything
     (`records` 1375→1645, `3x90k` 1503→1966);
  3. *complete index + capped insertion* (index every position, cap only the
     insertion-time extension): matched the chain on `records`/`3x90k`/`redundant`
     but still `json` ~3 % worse and ~2× slower.
  4. *faithful port* (preserved, unmerged, on branch `experiment/bt-finder`): a
     from-scratch port of zstd's `ZSTD_insertBt*` — complete index, **uncapped**
     search extension via the `commonLength` bound, window/`btLow` handling — with
     only the *insert-only* path (positions skipped inside a committed long match)
     capped, to bound the periodic-data O(n²) blowup while leaving the **search**
     path exact. **Correct and tractable** (round-trips through libzstd + our
     decoder; the across-levels test runs in <1 s — the first variant to achieve
     both). Still **ties** the chain on `records` (1163) / `3x90k` (1412) /
     `redundant` (55) and **loses** `json` (11812→12163, +3 %), `text`, `mixed` —
     the same shape as 1–3, because it *replaces* the chain's small-offset set.
  5. *chain/tree hybrid* (preserved, unmerged, on branch `experiment/bt-hybrid`):
     keep the chain's small-offset Pareto set and *add* the tree's longest match,
     but **only when it is `≥ suff`** (a committable long match longer than the
     chain's reach) — merging shorter tree matches re-introduces the `json`
     regression (the DP minimises a predefined-price *proxy*, not the real FSE
     cost, so a longer/larger-offset match looks cheap but isn't). With that
     restriction it finally **does not regress** — ties the chain-opt gate exactly
     on the whole corpus — but it still doesn't *beat* it: only `3x90k` 1412→1406
     (−6 B). An attempt to demonstrate a far-back win (a `farback` profile: a long
     marker recurring past >`depth` decoys that share its lead hash) **failed** —
     with the tree disabled the chain gets the identical size.
  **Why — the generalized finding (the real payoff of variants 4–5):** the chain
  indexes *every* position, so it finds a far-back match through *any* distinctive
  4-byte window inside it; its depth bound only blocks a hash that is *saturated*
  by recent collisions. A useful match is therefore almost always reachable: it is
  either **distinctive** (some window is unique → the chain finds it regardless of
  recency) or **repetitive** (a recent occurrence exists → rep/short matches find
  it). The binary tree only wins when a match is *neither* — pathological hash
  saturation across its whole length — which realistic data doesn't exhibit. And
  where the tree *does* surface an extra long match, our entropy cost model
  prefers the chain's smaller offsets anyway (the `json` regression). Net: the
  tree/hybrid buys no realistic ratio over the chain, at the cost of the tree's
  memory + ~2× match time at L16+. The remaining lever for `records` is therefore
  **not** the match finder but the **cost model** (e.g. rep-offset candidates
  priced with the per-cell `rep`, which our collect-then-DP split — needed for the
  `btultra2` two-pass — can't currently supply). Also still open: `btlazy2`.
  (Sequence-table Repeat mode (3), the opt price-model refinement — the
  `btultra2` second pass — and block splitting are now done; see the Encoder
  checklist.)

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

Still open in Tier 3: perf (sequence decode + reverse bit reader are the decode
hot spots); criterion benches vs the `zstd` crate. With this, the encoder + decoder
both fully consume structured dictionaries.
