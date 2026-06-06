# Golden frame fixtures

Committed `<name>.zst` frames paired with their decoded `<name>.expected` bytes.
Unlike the dynamic libzstd oracle in `corpus.rs`, these are an **offline,
version-pinned** anchor: each frame must decode to exactly the committed bytes
through our one-shot decoder, our streaming decoder, *and* the current libzstd —
so a decoder regression is caught even if the libzstd dev-dependency changes.

The set is deliberately small but chosen to exercise format/entropy corners:

| Fixture                     | Corner exercised                                  |
|-----------------------------|---------------------------------------------------|
| `empty`                     | empty frame                                        |
| `one-byte`                  | single-byte frame                                  |
| `multiblock-treeless`       | multi-block frame → Treeless (repeat-Huffman) literals |
| `raw-incompressible`        | Raw block (verbatim, incompressible bytes)         |
| `rle-block`                 | RLE block (hand-built; libzstd won't emit one here)|
| `offsets-periodic`          | small-offset match copies (period 1/2/4/8/…)       |
| `predefined-fse-low-level`  | predefined FSE tables (small input, level 1)       |

The walker (`tests/golden_frames.rs`) also asserts the corpus *collectively*
contains Raw, RLE, and Compressed blocks and at least one Treeless literals
section, so those corners are verified present — not merely intended. (The
FSE-table modes — predefined / RLE / repeat — are whatever libzstd picks for these
inputs; they are exercised but not independently asserted.)

Regenerate with:
`cargo test --test golden_frames generate_golden_frames -- --ignored`
(the exact inputs/levels live in that ignored test).
