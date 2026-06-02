# Benchmarks

Three harnesses, all using libzstd (the `zstd` crate) as the reference point —
**never** at runtime, only as a dev-time comparison:

| what | tool | run |
|---|---|---|
| **ratio** (compressed size, small corpus) | `examples/ratio.rs` | `cargo run --release --example ratio` |
| **large-input** (size + time, optimal levels) | `examples/bench_large.rs` | `cargo run --release --example bench_large` |
| **throughput** (time) | `benches/compression.rs` (criterion) | `cargo bench --bench compression` |

> `cargo bench` (no target) also works, but to pass criterion CLI flags scope to
> the criterion target so they don't reach the libtest harness, e.g.
> `cargo bench --bench compression -- --sample-size 20`.

The bar (from the project roadmap) is **beating ruzstd / structured-zstd ratio at
L3+** and being competitive with libzstd; we already beat or match libzstd ratio
on redundant and repeating data, and trail it on data that wants the optimal
parse (dense JSON, high levels) — see "Standing" below.

## Ratio (`zstd_pure` vs libzstd, compressed bytes; `ratio` = ours ÷ libzstd, < 1.00× is smaller)

Profiles: `redundant` (160 KB, low-entropy, multi-block), `records` (48 KB
pseudo-random byte records), `text` (40 KB NL-ish repetition), `json` (146 KB
structured records, multi-block), `3x90k-chunk` (270 KB — three copies of a 90 KB
incompressible chunk, so only cross-block matching can shrink copies 2–3),
`mixed` (128 KB — ~64 KB repetitive text then ~64 KB structured JSON in one
block, two regimes whose entropy statistics differ: the block-splitter case).

| level | redundant | records | text | json | 3x90k-chunk | mixed |
|------:|----------:|--------:|-----:|-----:|------------:|------:|
|  1 | 0.65× | 1.87× | 1.03× | 0.91× | 3.25× | 0.99× |
|  3 | 0.65× | 1.02× | 1.02× | 0.76× | 0.12× | 0.85× |
|  6 | 0.66× | **0.97×** | 1.05× | 0.93× | 1.69× | 0.97× |
|  9 | 0.66× | **0.97×** | 1.05× | 1.09× | 2.49× | 1.15× |
| 13 | 1.02× | **0.96×** | 1.02× | 1.32× | **0.49×** | 1.07× |
| 19 | 1.02× | 1.12× | 1.02× | **0.80×** | 1.01× | **0.98×** |

Reading it: `level` scales ratio — `dfast` (double-hash) kicks in at L2–3, the
chain/lazy finder at L4+, the `btlazy2` chain/tree hybrid at L13–15, and the
optimal parse + block splitter at L16+. The `records` stream goes from 1.87× of
libzstd at L1 to ~1.0× by L3 (dfast's 8-byte hash finds the long matches the
single 4-byte table misses), and at L13 the `btlazy2` tree drops it to 0.96×
(beating libzstd) — the same tree collapses the cross-block `3x90k-chunk` to
0.49× there, since L13's chain depth (~16) alone misses the far matches the tree
reaches. The `3x90k-chunk` collapses from 11.7 KB → 1.4 KB (L1 → L19), matching
libzstd; dense `json` **beats** libzstd at L19 (0.80×); and
`mixed` — two regimes in one block — **beats** libzstd at L19 (0.98×) once the
splitter gives each half its own tables. The near-random `records` soft spot at
the top levels fell from 1.32× to 1.12× over this work: the `btultra2` second
pass (re-pricing the optimal parse from the first parse's actual statistics) took
it to 1.26×, then block splitting to 1.12×. It still trails a little **at L19** —
there the chain's depth (128) already reaches `records`'s matches, so the tree
can't help (unlike at L13, where the shallow depth lets it); the remaining lever
at the top is the cost model.

## Real-world corpus (Silesia, ours ÷ libzstd)

The fixture-gated `real_corpus` test (`tests/real_corpus.rs`) round-trips a
directory of real files *both ways* (our encode → our + libzstd decode; libzstd
encode → our decode) and reports the aggregate compressed size. On the standard
[Silesia corpus][silesia] (12 files, 202 MiB) every file round-tripped through
both decoders at each level:

| level | ours | libzstd | ratio |
|------:|-----:|--------:|------:|
|  3 | 67,553,205 B | 66,137,723 B | 1.021× |
|  9 | 62,515,463 B | 59,081,628 B | 1.058× |
| 19 | 55,797,558 B | 52,891,946 B | 1.055× |

Within ~2–6 % of libzstd across the range. The high-level gap partly reflects
our 8 MiB window cap (`MAX_WINDOW_LOG` 23, for decoder interoperability) versus
libzstd's larger window on the multi-MB files (e.g. `mozilla` 51 MB, `webster`
41 MB), where matches farther than 8 MiB back are out of our reach — the
long-distance-matching item (T2.4) targets exactly this.

Reproduce: `ZSTD_PURE_CORPUS=~/fixtures/silesia/raw cargo test --release
real_corpus -- --ignored --nocapture`.

[silesia]: https://github.com/MiloszKrajewski/SilesiaCorpus

## Throughput (indicative)

Criterion, 256 KiB mixed corpus, optimized build on one dev machine — **relative
shape matters, absolute numbers are machine-specific**:

| stage | zstd_pure | libzstd |
|---|---:|---:|
| compress L3  | ~120 MiB/s | ~380 MiB/s |
| compress L9  | ~36 MiB/s  | ~96 MiB/s  |
| compress L19 | ~2.4 MiB/s | ~3 MiB/s   |
| decompress   | ~320 MiB/s | faster     |

The encoder is slower than libzstd at low/mid levels (a simpler parse, no SIMD).
At L19 both run an optimal parse and land in the same ballpark (~2–3 MiB/s); ours
is a hash-chain DP augmented with a binary tree (the hybrid below), which adds the
long-range matches a pure chain misses on big many-candidate inputs at the cost of
extra per-position work at L16+.

## Standing & next levers

- **Strong:** redundant / repeating / cross-block data (often ≤ libzstd); dense
  structured data at L19 (`json` 0.80×) and heterogeneous data (`mixed` 0.98×)
  after the optimal parse + block splitting.
- **Competitive:** mid levels on mixed data after the lazy finder (L6–L9).
- **Soft spots:** near-random record data at the top levels (~1.12× at L19, down
  from 1.32× via the `btultra2` second pass then block splitting).

Cross-block entropy-table reuse is now implemented — sequence-table Repeat mode
(3) and treeless literals (block type 3) — so a block no longer re-describes a
table or Huffman tree its predecessor already sent. The win is concentrated on
small blocks and dictionary-primed small files (a large block amortizes its
header), so the large-stream profiles above barely move.

A dict-primed compression now also seeds block 1 from a structured dictionary's
entropy tables (Treeless literals + Repeat-mode sequence tables), so a small file
warm-starts instead of re-describing tables — the block bodies shrink, though a
structured dict's 4-byte frame `Dictionary_ID` is a fixed per-frame cost that can
dominate on very small files.

The opt price model now does libzstd's `btultra2` second pass — a re-parse
priced from the first parse's actual literal/code statistics — and, at L16+, a
**block splitter** partitions a block into adjacent blocks each with its own
entropy tables when their statistics differ enough to pay for the extra headers
(kept only when strictly smaller, so it never regresses). Together these
narrowed `records` (1.32× → 1.12×), pushed `json` to 0.80×, brought `3x90k` to
parity, and let `mixed` beat libzstd. The predefined-prior single pass remains
for `btopt`/`btultra` (L13–18), and the splitter is off below L16 to keep the
fast/lazy levels' throughput untouched.

The optimal parse now runs a **chain/tree hybrid** at L16+ (the binary-tree match
finder): the hash chain supplies the small-offset Pareto matches, and a faithful
binary tree adds its longest match when it's `≥ sufficient_len` — a committable
long match the chain's depth bound (128) missed. It **ties the small corpus
exactly** (so the table above is unchanged) and pays off where that bound binds,
which the small synthetic corpus doesn't exercise but `bench_large` does:

| L19 profile (≈2 MB) | chain-opt | hybrid | vs libzstd |
|---|---:|---:|---:|
| `revisions` (150 near-duplicate docs) | 20,748 | **17,506** | 1.300× → **1.097×** |
| `logs` (templated) | 461,780 | 461,780 | 1.164× (tie) |
| `binstruct` (repeated records) | 749,462 | 749,462 | 1.097× (tie) |

The `revisions` win (**−16 %**) is the depth bound binding: with ~150 candidates
per hash (> the chain's depth) the best match sits further back than the chain
walks, and the tree finds it. The cost is the tree's memory + up to ~2× match
time at L16+ (correlated with the benefit — neutral-to-faster where it only ties),
acceptable at the max-compression tier. The same hybrid backs `btlazy2` (L13–15)
in a lazy parse; there the chain's depth is much shallower (~16), so the tree
helps even on the small corpus (L13 `records` 1.14× → 0.96×, `3x90k` 1.27× →
0.49×) and on near-duplicate data (`revisions` 1.41× → 1.02×). Why the L16+ tree
needed a big input to show a win: the chain
indexes every position, so it finds a far-back match through any *distinctive*
4-byte window — its depth bound only hides matches whose entry hashes are
*saturated* by recent collisions, i.e. the high-candidate-count (near-duplicate)
case the hybrid catches. The remaining `records` lever is the **cost model** (see
the `README.md` handoff), not the match finder. A future decode-speed comparison
against the pure-Rust `ruzstd` decoder would round out the peer set.
