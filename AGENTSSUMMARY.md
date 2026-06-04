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
- This branch: dfast rep-offset awareness for L3/L4. Silesia sizeDelta improved
  L3 `+1.6% -> +1.1%`, L4 `+1.5% -> +0.9%`; synthetic ratio profiles improved
  or held.

## Tried & rejected (don't redo without a new angle — see PERF_NOTES)
- `unsafe`/`get_unchecked` (≈0% after the safe elision) — discarded.
- Huffman fixed-array elision (ILP already hides it); per-block literal-buffer
  reuse (zeroing was free) — flat, reverted.
- Row-based finder (`RowHashState`) — correct but ~2× slower scalar; needs SIMD.
- Block-split estimator "C" for L7–12 — couldn't make it free; cost is inherent.

## In flight / next
- See `HANDOFF.md` for the remaining ready task: `reduceIndex` (rebuild-free
  streaming slides).
- Owner-directed next: advanced-parameter API, then a full COVER dictionary
  trainer.
- Deferred: v0.1.0 release (CHANGELOG needs refreshing to current `main` first).

## Build/test quickref
`cargo test --release` · `cargo +nightly fuzz run {encode_roundtrip,decode_diff}`
· `cargo clippy --all-targets` · `cargo build --no-default-features` ·
`ZSTD_PURE_CORPUS=~/fixtures/silesia cargo run --release --example {throughput,ratio}`.
