# `decode/` regressions

Arbitrary or hostile frames from the `decode`, `streaming_roundtrip`, and
`dictionary` fuzz targets. The walker requires that both one-shot
(`decompress_capped`) and streaming (`StreamingDecoder`) decode of each `*.bin`
stay within the output cap and never panic — any `Ok`/`Err` outcome is fine.
