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
|  3 | 0.66× | 1.02× | 1.02× | 0.76× | 0.12× |
|  6 | 0.66× | **0.97×** | 1.05× | 0.93× | 1.69× |
|  9 | 0.66× | **0.97×** | 1.05× | 1.09× | 2.50× |
| 19 | 1.02× | 1.32× | 1.02× | **0.87×** | 1.08× |

Reading it: `level` scales ratio — `dfast` (double-hash) kicks in at L2–3, the
chain/lazy finder at L4+, and the `btopt` optimal parse at L13+. The `records`
stream goes from 1.87× of libzstd at L1 to ~1.0× by L3 (dfast's 8-byte hash finds
the long matches the single 4-byte table misses); the cross-block `3x90k-chunk`
collapses from 11.7 KB → 1.5 KB (L1 → L19), within ~8 % of libzstd; and dense
`json` — our worst high-level case before the optimal parse — now **beats**
libzstd at L19 (0.87×). The remaining soft spot is `records` at the very top
levels (~1.3×): the opt price model is a fixed predefined-table proxy, so it
leaves a little on near-random data.

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
  structured data at L19 (`json` 0.87×) after the optimal parse.
- **Competitive:** mid levels on mixed data after the lazy finder (L6–L9).
- **Soft spots:** near-random record data at the top levels (~1.3×).

Next ratio levers (see `README.md` handoff): `dfast` (L2–3); a binary-tree match
finder to make the optimal parse both faster and deeper; refining the opt price
model with per-block statistics (libzstd's `btultra2` second pass); the
sequence-table Repeat mode (3); block splitting. A future decode-speed comparison
against the pure-Rust `ruzstd` decoder would round out the peer set.
