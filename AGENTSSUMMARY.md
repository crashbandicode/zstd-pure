# AGENTSSUMMARY.md — where things stand

Rolling status doc (see `AGENTS.md` §4). Read this first; update it after each
landed chunk; reconcile it against `git log`/tests after any context compaction.

_Last updated: 2026-06-05._

## Standing vs libzstd (Silesia, real-world)
- **Compression:** within **~1.5–3%** of libzstd's size across L5–L19; **beats**
  libzstd on structured/redundant/periodic data (json 0.80–0.86×, redundant
  0.65×, records 0.84× @L9, 3x90k, mixed/wiki at high levels).
- **Speed:** decode **~3×** slower, encode **~2–4×** (6× at L19). This is the
  safe-Rust/no-SIMD floor; the `unsafe` ceiling was measured at ~0% extra (the
  FSE fixed-array elision already banked it) — see `PERF_NOTES.md`.
- **100% safe Rust** (`forbid(unsafe_code)`), no_std+alloc, only `thiserror`.

## Capabilities (format- & feature-complete)
Levels 1–22; all block/literal/sequence modes incl. FSE Repeat + Treeless;
content size / checksum / magicless / dict-id frames; dictionaries; streaming;
parallel encode (`compress_parallel`); LDM (`compress_long`); seekable format +
**parallel seekable decode** (`decompress_seekable_parallel[_capped]`, ~3.4×/8thr).

## Recently landed (on origin/main)
- Decode: FSE bounds-check elision (fixed array), 4-stream Huffman interleave.
- Encode: deferred BitWriter, code reuse + binary-search codes, analytic literals
  sizing, opt-parse ml-price hoist; finder — rep-offset awareness + gain-based
  lazy (L5–15), `mls`-width chain hash (halved the real-world gap), block
  splitting (L13–15).
- Parallel seekable decoder (+ capped/bomb-safe variant + negative tests).
- **Advanced-parameter API** (#5): `CompressOptions` + `compress_with_options`
  (libzstd `ZSTD_CCtx_setParameter` analogue — window/hash/chain/search log,
  min_match, target_length, strategy, checksum, magic, LDM). Defaults
  byte-identical to `compress`; overrides round-trip through our decoder + libzstd.
  `compress`/`compress_long` share params-taking cores.
- **COVER (k,d) dictionary optimization** (#4): greedy COVER parameterized by
  segment `k`/dmer `d`; `train_dictionary_optimized` grid-searches `(k,d)` and
  keeps the candidate that compresses the corpus smallest (grid includes the
  defaults ⇒ provably ≤ `train_dictionary`). Ironclad test (both decoders,
  ≤ default, < no-dict).
- **dfast rep-offset awareness** (#3) for L3/L4: Silesia sizeΔ improved
  L3 `+1.6%→+1.1%`, L4 `+1.5%→+0.9%`; synthetic ratio profiles improved or held.
- **Rebuild-free streaming reduceIndex** (#2): streaming slides shift finder/LDM
  positions in place instead of rebuilding; chain/binary-tree drops are aligned to
  their ring periods so slots stay valid. Slide tests, fuzz, throughput, ratio clean.
- **Full binary-tree match set at every opt level** (L16–22): the opt parser
  collects the tree's full Pareto set (faithful `insert_and_get_all_matches`)
  alongside the proven chain hybrid, in ONE finder walk, and `emit_and_pick` keeps
  whichever block actually encodes smaller (per-block no-regression guard via
  `Parsed.alts`). **Silesia sizeΔ vs libzstd: L16 +2.7%→−0.4%, L17 +1.8%→−0.1%,
  L18 +2.6%→+0.7%, L19 +2.4%→+0.5%** — L16/L17 now *beat* libzstd; zero regressions
  (guard); non-opt levels byte-identical. Cost: a 2nd DP over the richer stream at
  L16–22 (collection is shared). See `COST_MODEL_NOTES.md`.
- **Gain-based btlazy2 match selection** (L13–15): the lazy parse now picks chain
  vs binary-tree match by *gain* (`len*4 - offset_bits`) instead of a fixed
  `target_length` cutoff, so the tree's longer far matches win when worth it.
  **Silesia sizeΔ: L13 +2.7%→+0.2%, L14 +2.4%→+0.1%, L15 +2.2%→+0.5%** (≈ libzstd);
  synthetic json 1.28×→1.04×, mixed now beats libzstd; only `records` +6 B (accepted
  synthetic blip). Encode speed unchanged (tree already maintained at these levels).
- **Safe decode hot-path speedups** (`wip/speed-recs`): packed Huffman decode table
  (array-of-structs, one masked load), load-FSE-entry-once for symbol+transition
  with a tighter `ensure`, RLE/offset-2/4/8 bulk overlap copies, and no per-block
  FSE-table clone (borrowed cache). No `unsafe`; decoder output byte-identical
  (corpus differential + decode_diff fuzz clean). Criterion decode (validated
  back-to-back): **our-frame +28% (582→743 MiB/s), libzstd-frame +8% (950→1024)**;
  ~1.35× vs libzstd's native on the mixed bench corpus.

## Coverage baseline
- Task #2 branch `wip/coverage-info`: separate informational `cargo-llvm-cov`
  workflow with Codecov upload plus `coverage-summary.txt`/`lcov.info` artifacts,
  and a README coverage badge. Local baseline measured 2026-06-04 with
  cargo-llvm-cov 0.8.7: **92.76% line**, 93.56% region, 93.65% function coverage.
  The coverage command runs release-profile all-features tests and skips only
  exhaustive matrix tests already covered by normal CI to keep the job bounded:
  `cargo llvm-cov --release --summary-only --all-features -- --skip compress_roundtrips_across_levels --skip round_trips_three_ways_across_levels --skip frame_is_independent_of_write_chunk_size --skip multi_block_stream_compresses_and_round_trips --skip our_frames_round_trip_through_the_streaming_decoder`.
  Largest remaining line gaps: `huff.rs` 80.25%, `encode/mod.rs` 82.11%,
  `frame.rs` 86.42%, `encode/stream.rs` 88.84%.
- Task #3 branch `wip/coverage-tests`: added `tests/public_api_edges.rs`, a
  black-box public API suite for magicless helpers/header parsing, libzstd
  magicless differential decode, typed malformed-header errors, streaming
  magicless decode, and streaming window-cap rejection. Coverage remeasured with
  the same command: **92.98% line**, 93.68% region, 94.01% function coverage.
  Biggest useful wins: `frame.rs` 86.42%→90.94% and `streaming.rs`
  90.94%→92.15%. Remaining low files (`huff.rs`, `encode/mod.rs`,
  `encode/stream.rs`) are mostly internal entropy/finder details or exhaustive
  matrix tests that normal CI still runs; chasing them is not needed for the
  >=90% bar.
- **Core-library issue #4:** frame decode now enforces RFC 8878
  `Block_Maximum_Size = min(Window_Size, 128 KiB)` immediately after each block
  header; oversized Raw/RLE/Compressed block headers return a typed
  `ZstdError::Invalid { what: "block size", .. }`. `tests/public_api_edges.rs`
  covers >128 KiB, >small-window, and exactly-128 KiB raw blocks against libzstd.
  Crate-level docs and the old `nx-layout-toolbox` error-module reference were
  refreshed to match the standalone `zstd-pure` surface.
## Tried & rejected (don't redo without a new angle — see PERF_NOTES)
- `unsafe`/`get_unchecked` (≈0% after the safe elision) — discarded.
- Huffman fixed-array elision (ILP already hides it); per-block literal-buffer
  reuse (zeroing was free) — flat, reverted.
- Row-based finder (`RowHashState`) — correct but ~2× slower scalar; needs SIMD.
- Block-split estimator "C" for L7–12 — couldn't make it free; cost is inherent.
- Binary tree for L8–12 (Lazy2): beats libzstd on ratio (L9 +1.5%→−1.8%) but 4–7×
  slower encode (L9 22→3 MB/s) — gutted the fast tier; kept those levels tree-less.

## In flight / next
- **Convention:** one feature → one `git worktree` on its own branch (see
  `AGENTS.md` §0 / `CLAUDE.md`), so agents work in parallel without colliding.
- **Pure-Rust peer comparison (Task #1, `wip/competitor-comparison`)**:
  `COMPARISON.md` now records reproducible `pure_rust_compare` results for
  zstd-pure vs ruzstd 0.8.3 and oxiarc-zstd 0.3.2 on synthetic profiles and
  Silesia raw (8 MiB/file cap), including cross-decode correctness, unsafe
  counts, and a capability matrix. Verdict: zstd-pure is the only tested
  pure-Rust encoder with libzstd + ruzstd cross-decode compatibility and
  competitive size; libzstd still wins speed, and OxiArc failed cross-decode.
- All four queued tasks (#2–#5) are landed; the `HANDOFF.md` encoder tasks are done.
- **Mid levels:** L13–15 closed to ≈libzstd (gain-based btlazy2, above). **L9–L12
  (Lazy2) deliberately left tree-less** at +1.5–2.0%: a measured experiment giving
  them a tree beat libzstd on ratio but cost 4–7× encode (L9 22→3 MB/s), gutting the
  fast tier — see "Tried & rejected". The remaining mid-band lever would be a SIMD
  row-hash finder (better chain candidates without the full tree's cost).
- Parallel agent: competitive benchmark vs other pure-Rust zstd crates + code
  coverage/badge + coverage-driven tests (see `HANDOFF.md`).
- Deferred: v0.1.0 release (CHANGELOG needs refreshing to current `main` first).
- COVER "stable" follow-ups: epoch-partitioned segment selection + a dict fuzz target.

## Build/test quickref
`cargo test --release` · `cargo +nightly fuzz run {encode_roundtrip,decode_diff}`
· `cargo clippy --all-targets` · `cargo build --no-default-features` ·
`cargo llvm-cov --release --summary-only --all-features -- --skip compress_roundtrips_across_levels --skip round_trips_three_ways_across_levels --skip frame_is_independent_of_write_chunk_size --skip multi_block_stream_compresses_and_round_trips --skip our_frames_round_trip_through_the_streaming_decoder`
· `ZSTD_PURE_CORPUS=~/fixtures/silesia cargo run --release --example {throughput,ratio}`.
