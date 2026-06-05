# Pure-Rust zstd Peer Comparison

Measured 2026-06-04 on `rustc 1.96.0`, Intel Core Ultra 7 258V, with:

```sh
cargo run --release --example pure_rust_compare
ZSTD_PURE_CORPUS=/home/intpa/fixtures/silesia/raw ZSTD_PURE_COMPARE_MAX_MB=8 \
  cargo run --release --example pure_rust_compare
```

The Silesia run uses `raw/`, capped at 8 MiB per file to match the existing
throughput harness shape. Decoder columns are MiB/s over raw input bytes. `ERR`
means the decoder rejected, mismatched, or panicked on the first failing input;
that is a correctness/interoperability result, not just a speed result.

Peers:

- `ruzstd 0.8.3` ([docs](https://docs.rs/ruzstd/0.8.3), [crate](https://crates.io/crates/ruzstd/0.8.3)): pure-Rust decoder plus an encoder API; only `Fastest` is implemented for compression in the tested release.
- `oxiarc-zstd 0.3.2` ([docs](https://docs.rs/oxiarc-zstd/0.3.2), [crate](https://crates.io/crates/oxiarc-zstd/0.3.2)): pure-Rust codec API advertising levels 1-22.
- `zstd 0.13.3` / rust-zstd ([docs](https://docs.rs/zstd/latest/zstd/), [crate](https://crates.io/crates/zstd)): libzstd binding, so it is the C oracle here, not a pure-Rust peer.

## Silesia Raw, 8 MiB/File Cap

| level | encoder | bytes | vs libzstd | enc MiB/s | zstd-pure dec | libzstd dec | ruzstd dec | oxiarc dec |
|---:|---|---:|---:|---:|---:|---:|---:|---:|
| 1 | libzstd | 40,503,078 | 1.000x | 464.2 | 551.7 | 1623.1 | 242.0 | ERR |
| 1 | zstd-pure | 40,284,724 | 0.995x | 112.3 | 449.8 | 1219.0 | 214.5 | ERR |
| 1 | oxiarc-zstd | 49,919,672 | 1.232x | 50.1 | ERR | ERR | ERR | 291.4 |
| 1 | ruzstd | 47,090,959 | 1.163x | 76.4 | 548.0 | 1767.2 | 398.4 | ERR |
| 3 | libzstd | 36,632,910 | 1.000x | 265.6 | 507.7 | 1469.8 | 273.4 | ERR |
| 3 | zstd-pure | 36,895,214 | 1.007x | 93.5 | 482.0 | 1347.4 | 313.8 | ERR |
| 3 | oxiarc-zstd | 46,927,687 | 1.281x | 43.4 | ERR | ERR | ERR | 303.4 |
| 3 | ruzstd | n/a | n/a | n/a | n/a | n/a | n/a | n/a |
| 9 | libzstd | 33,441,660 | 1.000x | 63.0 | 518.6 | 1577.6 | 240.6 | ERR |
| 9 | zstd-pure | 33,899,817 | 1.014x | 18.9 | 539.7 | 1389.9 | 332.7 | ERR |
| 9 | oxiarc-zstd | 46,052,474 | 1.377x | 31.6 | ERR | ERR | ERR | 299.5 |
| 9 | ruzstd | n/a | n/a | n/a | n/a | n/a | n/a | n/a |

## Synthetic Profiles

Aggregate over `redundant`, `records`, `text`, `json`, `3x90k-chunk`, `mixed`,
and `wiki` from `examples/ratio.rs` (0.90 MiB total).

| level | encoder | bytes | vs libzstd | enc MiB/s | zstd-pure dec | libzstd dec | ruzstd dec | oxiarc dec |
|---:|---|---:|---:|---:|---:|---:|---:|---:|
| 1 | libzstd | 80,409 | 1.000x | 871.1 | 1214.7 | 2582.0 | 555.2 | ERR |
| 1 | zstd-pure | 91,763 | 1.141x | 187.7 | 750.9 | 2209.2 | 590.3 | ERR |
| 1 | oxiarc-zstd | 175,198 | 2.179x | 83.3 | ERR | ERR | ERR | 363.0 |
| 1 | ruzstd | 130,125 | 1.618x | 146.8 | 1144.9 | 2611.6 | 804.4 | ERR |
| 3 | libzstd | 111,332 | 1.000x | 665.7 | 1012.3 | 1724.0 | 614.9 | ERR |
| 3 | zstd-pure | 73,828 | 0.663x | 222.0 | 1131.0 | 2059.2 | 764.0 | ERR |
| 3 | oxiarc-zstd | 147,810 | 1.328x | 101.5 | ERR | ERR | ERR | 380.1 |
| 3 | ruzstd | n/a | n/a | n/a | n/a | n/a | n/a | n/a |
| 9 | libzstd | 68,166 | 1.000x | 70.2 | 1167.7 | 2683.5 | 729.3 | ERR |
| 9 | zstd-pure | 72,075 | 1.057x | 61.4 | 1158.3 | 3015.4 | 895.1 | ERR |
| 9 | oxiarc-zstd | 127,175 | 1.866x | 44.9 | ERR | ERR | ERR | 251.2 |
| 9 | ruzstd | n/a | n/a | n/a | n/a | n/a | n/a | n/a |

## Cross-Decode Results

| encoded by | decoded by zstd-pure | decoded by libzstd | decoded by ruzstd | decoded by oxiarc-zstd |
|---|---|---|---|---|
| zstd-pure | pass | pass | pass | fail/panic on tested synthetic + Silesia frames |
| libzstd | pass | pass | pass | fail/panic on tested synthetic + Silesia frames |
| ruzstd Fastest | pass | pass | pass | fail/panic on tested synthetic + Silesia frames |
| oxiarc-zstd | fail under zstd-pure | fail under libzstd | fail under ruzstd | pass under oxiarc-zstd |

The OxiArc failures are reproducible with the harness above. On the first
Silesia file (`dickens`, capped to 8 MiB), OxiArc either rejects standard frames
from the other encoders during decode or emits frames that zstd-pure/libzstd/
ruzstd reject as corrupt.

## Safety Posture

Counts use `rg -o "unsafe\s*(\{|fn|impl)" <crate>/src`.

| crate | crate-level unsafe policy | unsafe constructs in crate source | note |
|---|---|---:|---|
| zstd-pure | `#![forbid(unsafe_code)]` | 0 | compiler-enforced |
| ruzstd 0.8.3 | no forbid/deny found | 39 | ring-buffer/decode-buffer internals |
| oxiarc-zstd 0.3.2 | no forbid/deny found | 0 | source has no unsafe tokens, but not compiler-forbidden |
| zstd 0.13.3 | n/a | n/a | Rust binding to libzstd C library |

## Capability Matrix

| capability | zstd-pure | ruzstd 0.8.3 | oxiarc-zstd 0.3.2 | zstd/rust-zstd |
|---|---|---|---|---|
| pure Rust | yes | yes | yes | no, libzstd binding |
| compression levels | 1-22 | `Uncompressed`, `Fastest` only in tested release | 1-22 API | libzstd |
| strategy controls | `Strategy` + advanced params | no public zstd-like matrix | `CompressionStrategy` API | libzstd |
| LDM | yes (`compress_long`) | no | not observed | libzstd |
| dictionaries | raw, structured, trained, optimized | decode/dict-builder features | dictionary API | libzstd |
| streaming decode | yes | yes | yes | yes |
| streaming encode | yes | frame compressor / encode API | yes | yes |
| seekable format | yes | no | no | not in base crate |
| parallel encode | yes | no | optional `parallel` feature | libzstd |
| no_std + alloc | yes | no_std-capable with feature setup | no no_std claim | no |
| cross-compatible in this run | yes with libzstd + ruzstd | yes at Fastest | no | oracle |

## Verdict

On the measured Silesia slice, zstd-pure is the only pure-Rust encoder here that
is both competitive with libzstd size and cross-decodes cleanly through libzstd
and ruzstd. It beats ruzstd Fastest on size and encode speed at the only ruzstd
compression level available, and it beats OxiArc on size while also producing
standard frames. Where zstd-pure loses is still clear: libzstd is much faster,
especially for decode, and zstd-pure L9 encode is slower than OxiArc's own
encoder on this run. OxiArc's own decoder is fast on OxiArc frames, but the
cross-decode failures make it non-competitive as an interoperable zstd codec
until those correctness issues are fixed.
