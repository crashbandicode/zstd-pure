# Benchmarks

Two harnesses, both using libzstd (the `zstd` crate) as the reference point —
**never** at runtime, only as a dev-time comparison:

| what | tool | run |
|---|---|---|
| **ratio** (compressed size) | `examples/ratio.rs` | `cargo run --release --example ratio` |
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
| 19 | 1.02× | 1.12× | 1.02× | **0.80×** | 1.01× | **0.98×** |

Reading it: `level` scales ratio — `dfast` (double-hash) kicks in at L2–3, the
chain/lazy finder at L4+, the `btopt` optimal parse at L13+, and the block
splitter at L16+. The `records` stream goes from 1.87× of libzstd at L1 to ~1.0×
by L3 (dfast's 8-byte hash finds the long matches the single 4-byte table
misses); the cross-block `3x90k-chunk` collapses from 11.7 KB → 1.4 KB (L1 →
L19), now matching libzstd; dense `json` **beats** libzstd at L19 (0.80×); and
`mixed` — two regimes in one block — **beats** libzstd at L19 (0.98×) once the
splitter gives each half its own tables. The near-random `records` soft spot at
the top levels fell from 1.32× to 1.12× over this work: the `btultra2` second
pass (re-pricing the optimal parse from the first parse's actual statistics) took
it to 1.26×, then block splitting to 1.12×. It still trails a little — the last
gap is to libzstd's binary-tree match finding.

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
is a hash-chain DP rather than libzstd's binary tree, so a deeper/bigger corpus
would widen libzstd's lead — a binary-tree match finder is the way to close it.

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

Next ratio levers (see `README.md` handoff): a binary-tree match finder to make
the optimal parse both faster and deeper — the remaining `records` gap. A future
decode-speed comparison against the pure-Rust `ruzstd` decoder would round out
the peer set.
