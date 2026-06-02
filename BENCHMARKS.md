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
incompressible chunk, so only cross-block matching can shrink copies 2–3).

| level | redundant | records | text | json | 3x90k-chunk |
|------:|----------:|--------:|-----:|-----:|------------:|
|  1 | 0.66× | 1.87× | 1.03× | 0.91× | 3.25× |
|  3 | 0.66× | 1.62× | 1.02× | 0.76× | 0.25× |
|  6 | 0.66× | **0.97×** | 1.05× | 0.93× | 1.69× |
|  9 | 0.66× | **0.97×** | 1.05× | 1.09× | 2.50× |
| 19 | 1.04× | 1.30× | 1.02× | 1.31× | **1.04×** |

Reading it: `level` now scales ratio (the chain/lazy finder kicks in at L4+). The
`records` stream goes from 1.87× of libzstd at L1 to 0.97× (beating it) at L6; the
cross-block `3x90k-chunk` collapses from 11.7 KB → 1.45 KB (L1 → L19), ending
within 4 % of libzstd. We trail libzstd most on `json` and `records` at the top
levels — exactly where libzstd's `btopt`/`btultra` optimal parse wins and we
currently fall back to `lazy2`.

## Throughput (indicative)

Criterion, 256 KiB mixed corpus, optimized build on one dev machine — **relative
shape matters, absolute numbers are machine-specific**:

| stage | zstd_pure | libzstd |
|---|---:|---:|
| compress L3  | ~120 MiB/s | ~380 MiB/s |
| compress L9  | ~36 MiB/s  | ~96 MiB/s  |
| compress L19 | ~13 MiB/s  | ~3 MiB/s   |
| decompress   | ~320 MiB/s | faster     |

The encoder is slower than libzstd at low/mid levels (a simpler parse, no SIMD),
but **faster at L19**: libzstd spends heavily on its `btultra2` optimal parse
there while we run `lazy2`. That is the same trade-off the ratio table shows —
we leave ratio on the table at the top end in exchange for speed, until the
optimal parse lands.

## Standing & next levers

- **Strong:** redundant / repeating / cross-block data (often ≤ libzstd).
- **Competitive:** mid levels on mixed data after the lazy finder (L6–L9).
- **Behind:** top levels and dense structured data (`json`) — needs `btopt`.

Next ratio levers (see `README.md` handoff): `dfast` (L2–3), the `btopt`/`btultra`
optimal parse (L13+, today `lazy2`), the sequence-table Repeat mode (3), and
block splitting. A future decode-speed comparison against the pure-Rust `ruzstd`
decoder would round out the peer set.
