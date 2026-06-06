# `seekable/` regressions

Hostile seekable archives from the `seekable_decode` and `seekable_roundtrip`
targets. For each `*.bin` the walker runs `SeekTable::parse` and, on success,
random-access (`decompress_seekable_frame`) and parallel
(`decompress_seekable_parallel_capped`) decode — all of which must stay bounded
and never panic.
