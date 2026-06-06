# Fuzz-regression corpus

When a fuzz target (see `fuzz/fuzz_targets/`) finds a crash, hang, OOM, or a
differential mismatch against libzstd, **minimize** the reproducer
(`cargo fuzz tmin <target> <artifact>`) and drop the resulting bytes here as a
`*.bin` file under the matching subdirectory. The `tests/regressions.rs` walker
replays every committed case on each `cargo test`, so a fixed bug can never
silently return.

| Subdir         | Source targets                                   | Contract enforced by the walker |
|----------------|--------------------------------------------------|---------------------------------|
| `decode/`      | `decode`, `streaming_roundtrip`, `dictionary`    | one-shot **and** streaming decode stay bounded and never panic (any `Ok`/`Err` is acceptable) |
| `decode_diff/` | `decode_diff`                                    | libzstd is the oracle — whatever libzstd decodes, our decoder must decode **identically** |
| `seekable/`    | `seekable_decode`, `seekable_roundtrip`          | `SeekTable::parse` + random-access + parallel decode stay bounded and never panic |

Naming: `NNNN-short-description.bin` (zero-padded, sorted). Keep cases tiny.

The corpus ships seeded with a few hand-built edge cases so the walker is
exercised from day one; their exact provenance is the ignored
`generate_seed_corpus` test in `tests/regressions.rs`
(`cargo test --test regressions generate_seed_corpus -- --ignored`). Real fuzz
finds are added by hand alongside them — do **not** regenerate over them.
