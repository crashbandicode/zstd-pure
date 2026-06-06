# AGENTS.md — operating guide for agents working on zstd-pure

A pure-Rust Zstandard (RFC 8878) codec: **100% safe Rust, no_std+alloc, only
`thiserror`**, validated byte-for-byte against libzstd. Read this before changing
anything, and **keep it and `AGENTSSUMMARY.md` current** (see §4).

## 0. Non-negotiable invariants
- **`#![forbid(unsafe_code)]` is sacred.** It is compiler-enforced and CI-checked.
  Do NOT relax it. The only exception is the off-by-default `unsafe-optimizations`
  feature (`#![cfg_attr(not(feature = "unsafe-optimizations"), forbid(unsafe_code))]`),
  and adding `unsafe` there requires a proven ≥10–20% end-to-end win, a safe
  fallback, a tiny safe wrapper, documented invariants, and the libzstd
  differential as judge. The default build stays 100% safe.
- **`no_std + alloc`** must keep building (`cargo build --no-default-features`).
- **Only dependency is `thiserror`** (and `zstd`/`libfuzzer` as dev/test oracles).
  Do not add runtime dependencies.
- **Shell is PowerShell (`pwsh`)**, one logical command per call. `python3`.
  `rg --color=never` (ripgrep). **MSRV is 1.81** — no newer std APIs
  (e.g. no `Option::is_none_or`, which is 1.82).
- **Git:** commit each green chunk; **do NOT push without explicit permission.**
  Multi-line commit messages via `git commit -F <tempfile>`. Branch for risky or
  ratio-affecting work; fast-forward merge when green.
- **One feature → one worktree.** Do each feature in its own `git worktree` on its
  own branch, so multiple agents can work different features in parallel without
  colliding on `main` or each other's checkouts. Create it with
  `git worktree add ../zstd-pure-<feature> -b wip/<feature>`, do all of that
  feature's work and commits there, and `git worktree remove` it once the branch is
  merged. Never run two features out of the same checkout. The no-regression gates
  (§1) and ironclad-test bar (§5) apply per worktree before merge.

## 1. No regressions — prove every change
A codec change alters output, so **measure, don't assume.** Before committing any
encoder/decoder change, ALL of these must pass (this mirrors CI):
- `cargo test --release` — lib + corpus (libzstd differential) + proptest.
- `cargo +nightly fuzz run encode_roundtrip` and `... decode_diff`
  (`-- -max_total_time=80`) — clean (no crash/SUMMARY/panic).
- `cargo fmt --check`; `cargo clippy --all-targets` (CI denies warnings);
  `cargo build --no-default-features` (no_std); docs build with warnings denied.
- Informational coverage uses `cargo-llvm-cov`:
  `cargo llvm-cov --release --summary-only --all-features -- --skip compress_roundtrips_across_levels --skip round_trips_three_ways_across_levels --skip frame_is_independent_of_write_chunk_size --skip multi_block_stream_compresses_and_round_trips --skip our_frames_round_trip_through_the_streaming_decoder`.
  The skipped matrix tests are still covered by normal CI; the coverage job keeps
  them out only so the report is bounded. Use `--html` instead of
  `--summary-only` when hunting uncovered surfaces.
- **Ratio/speed, gated hard:**
  - `ZSTD_PURE_CORPUS=~/fixtures/silesia cargo run --release --example throughput`
    — size-vs-libzstd (`sizeΔ`, deterministic) + encode/decode MB/s.
  - `cargo run --release --example ratio` — synthetic profile suite. **Any ratio
    regression on a profile is a blocker** unless explicitly accepted (a couple of
    negligible synthetic byte-level blips have been accepted before — document it).
- The `ratio` column in `throughput` is `our_size / libzstd_size` (`<1.0` = we beat
  libzstd). `sizeΔ` is how much larger our output is.
- Prefer the **per-block no-regression guard** pattern where it fits: emit
  candidate encodings and keep the smallest, so output can never grow
  (see `emit_split`/`emit_and_pick` in `src/encode/block.rs`).
- CI must be **completed/green** for the tip before calling work done — check
  `gh run list`; "in progress" ≠ proven.

## 2. Match the surrounding code; preserve quality
- Read neighbours first; match their idiom, comment density, and naming. Comments
  explain *why* (invariants, the libzstd parallel), not *what*.
- Reuse existing helpers (`common_len`, `rep_match_at`, `encode_offset`,
  `hash_pos`, `log2_fp`, …) rather than re-deriving.
- Keep functions documented with their exact invariants — especially anything that
  indexes by an offset/hash (state why it's in bounds).
- **Leave it smaller and clearer (standing requirement).** Every change should
  keep the codebase easy for a human to follow and trend LOC *down*, not up:
  - Don't duplicate logic — extract a shared helper/iterator instead of copying a
    loop or formula. Splitting a file into smaller files is fine when it nets less
    code or clearly improves readability. A net LOC reduction at equal
    functionality is a win.
  - Refactors must be **functionally identical** (no behaviour/ratio change) and
    pass the full §1 gates — for hot/correctness-critical paths (decoder, opt
    parse, match finders) confirm byte-identical output via the corpus differential
    before committing.
  - Keep docs honest: when you change behaviour, update the surrounding comments
    and module headers in the same commit — no stale "planned/TODO/Tx.y" claims.
- **Tests: black-box, high-bang-for-buck, no coverage loss.** Prefer public-API
  round-trip + libzstd-differential + negative tests that exercise many corner
  cases at once over many narrow white-box tests. When adding/refactoring tests,
  consolidate duplication (shared fixtures/generators, parameterized loops) so the
  suite tests the same or *more* with less code. Never reduce coverage to cut LOC;
  deterministic only (fixed PRNG seeds, no timing/threading races).

## 3. Chunk large tasks (survive context compaction)
The context window gets compacted mid-task; design so a compaction never loses
progress or correctness:
- **Decompose up front** into small, independently-committable, independently-green
  steps. Each commit should build + pass tests on its own.
- **Commit each green chunk immediately** with a descriptive message — never carry
  a large uncommitted working state across a long exploration.
- Validate → commit → only then start the next chunk.
- After a compaction, **re-read `AGENTSSUMMARY.md`** (and this file) before
  continuing, and verify the repo state (`git log`, `git status`, a build) matches
  what the summary claims — names/flags in a stale summary may have changed.

## 4. Keep AGENTSSUMMARY.md current, and re-reference it
`AGENTSSUMMARY.md` is the rolling "where things stand" doc — the first thing to
read when picking up work and the anchor across compactions.
- **Update it periodically** — at minimum after each landed chunk: what changed,
  why, current standing vs libzstd, what's in flight, what's next.
- **Re-read it** at the start of a work session and after any compaction, and
  reconcile it against the actual repo (commits/tests). Fix it if it has drifted.
- Keep it short and current; move long rationale to commit messages / `PERF_NOTES`.

## 5. Ironclad tests → the "stable" flag
A feature is promoted from experimental to **stable** only when its tests are
ironclad. For any public API, that means:
- **Round-trip** correctness (our encode → our decode → original).
- **libzstd differential** (our output decodes under libzstd, and vice-versa)
  across levels / sizes / settings.
- **Negative / abuse tests**: corrupted input errors (not panics/OOM/wrong bytes),
  truncation errors, declared-size bombs are capped, edge args (`0`, empty, max)
  behave, and offset/size arithmetic is checked (`checked_add`, `try_from`).
- **Fuzzed** (add or extend a `fuzz/` target; it must build in CI).
- Bounded memory / no-OOM story for any bulk/parallel API (provide a `_capped`
  variant mirroring `decompress_capped`).
Only with all of the above is it safe to drop the experimental caveat.

## 6. Layout & validation map
- Decoder — public modules: `frame`, `streaming`, `seekable`, `dict`, `io`. The
  rest are private internals (`mod`, not `pub mod`): `bits`, `block`, `fse`,
  `huff`, `literals`, `sequences`, `xxhash`, `error`. The supported surface is
  those public modules plus the crate-root re-exports.
- Encoder: `encode/{lz,sequences,huff,fse,bitstream,block,frame,params,parallel,
  stream,ldm,options,train}` (`train` is the experimental COVER trainer).
- Tests: in-module `#[cfg(test)]` + the `tests/` integration suite —
  `corpus.rs` (libzstd differential), `real_corpus.rs` (`#[ignore]`, full Silesia
  both ways via `ZSTD_PURE_CORPUS`), `proptest.rs` (shrinking properties),
  `public_api_edges.rs`, `malformed.rs` (typed-error corpus),
  `encoder_invariants.rs`, `streaming_chunking.rs`, `rfc9659.rs`, `golden_frames.rs`
  (+ `tests/fixtures/frames/`), and `regressions.rs` (+ `tests/regressions/`
  fuzz-regression corpus). Eight fuzz targets in `fuzz/`: `decode`, `decode_diff`,
  `encode_roundtrip`, `streaming_roundtrip`, `seekable_roundtrip`,
  `parallel_roundtrip`, `dictionary`, `seekable_decode`.
- Journals (gitignored): `PERF_NOTES.md` (perf campaign), `HANDOFF.md`,
  `COST_MODEL_NOTES.md`. Tracked root docs: `CHANGELOG.md`, `README.md`,
  `SECURITY.md`, `BENCHMARKS.md`, `COMPARISON.md`, `AGENTSSUMMARY.md`.
