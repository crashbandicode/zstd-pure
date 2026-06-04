# AGENTSSUMMARY.md — where things stand

Rolling status doc (see `AGENTS.md` §4). Read this first; update it after each
landed chunk; reconcile it against `git log`/tests after any context compaction.

_Last updated: 2026-06-04._

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
- **Full binary-tree match set at btultra2** (L19–22): the opt parser now collects
  the tree's full Pareto set (faithful `insert_and_get_all_matches`) alongside the
  proven chain hybrid, in ONE finder walk, and `emit_and_pick` keeps whichever
  block actually encodes smaller (per-block no-regression guard via `Parsed.alts`).
  **Silesia L19 sizeΔ +2.4%→+0.5%** (≈ libzstd's 3.468×); synthetic json/wiki/3x90k
  improved, others unchanged, zero regressions; L16–18 + non-opt byte-identical.
  Cost: a 2nd DP over the richer stream at L19–22 (collection is shared). See
  `COST_MODEL_NOTES.md`.

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

## Tried & rejected (don't redo without a new angle — see PERF_NOTES)
- `unsafe`/`get_unchecked` (≈0% after the safe elision) — discarded.
- Huffman fixed-array elision (ILP already hides it); per-block literal-buffer
  reuse (zeroing was free) — flat, reverted.
- Row-based finder (`RowHashState`) — correct but ~2× slower scalar; needs SIMD.
- Block-split estimator "C" for L7–12 — couldn't make it free; cost is inherent.

## In flight / next
- **Convention:** one feature → one `git worktree` on its own branch (see
  `AGENTS.md` §0 / `CLAUDE.md`), so agents work in parallel without colliding.
- All four queued tasks (#2–#5) are landed; the `HANDOFF.md` encoder tasks are done.
- **Investigating** (`wip/btopt-fulltree` worktree): extend the guarded full
  binary-tree match set to btopt/btultra (L16–18). The guard makes it
  regression-proof; open question is whether the ratio gain justifies the 2nd-DP
  encode cost at those (meant-to-be-faster) levels — a measurement pass.
- Parallel agent: competitive benchmark vs other pure-Rust zstd crates + code
  coverage/badge + coverage-driven tests (see `HANDOFF.md`).
- Deferred: v0.1.0 release (CHANGELOG needs refreshing to current `main` first).
- COVER "stable" follow-ups: epoch-partitioned segment selection + a dict fuzz target.

## Build/test quickref
`cargo test --release` · `cargo +nightly fuzz run {encode_roundtrip,decode_diff}`
· `cargo clippy --all-targets` · `cargo build --no-default-features` ·
`cargo llvm-cov --release --summary-only --all-features -- --skip compress_roundtrips_across_levels --skip round_trips_three_ways_across_levels --skip frame_is_independent_of_write_chunk_size --skip multi_block_stream_compresses_and_round_trips --skip our_frames_round_trip_through_the_streaming_decoder`
· `ZSTD_PURE_CORPUS=~/fixtures/silesia cargo run --release --example {throughput,ratio}`.
