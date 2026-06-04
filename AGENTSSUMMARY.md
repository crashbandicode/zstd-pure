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
- **Full binary-tree match set at every opt level** (L16–22): the opt parser
  collects the tree's full Pareto set (faithful `insert_and_get_all_matches`)
  alongside the proven chain hybrid, in ONE finder walk, and `emit_and_pick` keeps
  whichever block actually encodes smaller (per-block no-regression guard via
  `Parsed.alts`). **Silesia sizeΔ vs libzstd: L16 +2.7%→−0.4%, L17 +1.8%→−0.1%,
  L18 +2.6%→+0.7%, L19 +2.4%→+0.5%** — L16/L17 now *beat* libzstd; zero regressions
  (guard); non-opt levels byte-identical. Cost: a 2nd DP over the richer stream at
  L16–22 (collection is shared). See `COST_MODEL_NOTES.md`.

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
- **Next ratio target — mid levels (L9–L15):** the band still furthest from libzstd
  (L9 +1.5%, L12 +2.0%). These are lazy/lazy2/btlazy2 parsers, so the full-tree DP
  technique doesn't apply (no DP to consume a Pareto set). Lever is finder quality
  in the lazy parse (better chain/tree candidates) or a SIMD row-hash finder (the
  earlier `RowHashState` was rejected at ~2× slower scalar — would need SIMD).
- Parallel agent: competitive benchmark vs other pure-Rust zstd crates + code
  coverage/badge + coverage-driven tests (see `HANDOFF.md`).
- Deferred: v0.1.0 release (CHANGELOG needs refreshing to current `main` first).
- COVER "stable" follow-ups: epoch-partitioned segment selection + a dict fuzz target.

## Build/test quickref
`cargo test --release` · `cargo +nightly fuzz run {encode_roundtrip,decode_diff}`
· `cargo clippy --all-targets` · `cargo build --no-default-features` ·
`ZSTD_PURE_CORPUS=~/fixtures/silesia cargo run --release --example {throughput,ratio}`.
