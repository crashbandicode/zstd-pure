# `decode_diff/` regressions

Frames from the `decode_diff` target, where libzstd is the differential oracle.
For each `*.bin` the walker requires: if libzstd decodes it, our decoder must
decode it to the **identical** bytes. (Cases libzstd rejects are skipped — they
belong in `decode/`.)
