# Security Policy

`zstd-pure` decodes and encodes **untrusted, possibly hostile** input, so it
takes robustness seriously. This document covers how to report a vulnerability
and what safeguards are in place.

## Reporting a vulnerability

Please report security issues **privately** via GitHub's
["Report a vulnerability"](https://github.com/crashbandicode/zstd-pure/security/advisories/new)
(Security Advisories) — not a public issue. Include a reproducer (the input bytes
and the call that misbehaves) where possible.

You can expect an acknowledgement and, once a fix is available, a coordinated
disclosure.

## Supported versions

Pre-1.0 and not yet published to crates.io; only the latest commit / tag on
`main` is supported. Pin a specific git revision in production.

## Threat model & safeguards

The decoder is the primary attack surface (it processes attacker-controlled
frames). The safeguards:

- **No `unsafe`.** The crate is `#![forbid(unsafe_code)]` — 100 % safe Rust, so it
  cannot contain memory-unsafety / undefined behavior. Out-of-bounds access is a
  deterministic panic, never UB.
- **Fuzzed to never panic or OOM.** The decoder surface is fuzzed through `decode`
  and `decode_diff` (arbitrary + mutated-valid frames, one-shot + streaming, magic +
  magicless; `decode_diff` also requires byte-for-byte agreement with libzstd on any
  frame both accept), `dictionary` (untrusted dictionary parse + bounded dict
  decode), and `seekable_decode` (random-access + parallel decode of hostile
  seekable archives); `streaming_roundtrip` and `seekable_roundtrip` exercise the
  decode paths as well. Every target requires the decoder to only ever return
  `Ok`/`Err` — never panic or exhaust memory — under an output cap.
- **Decompression bombs.** `decompress_capped(frame, max)` refuses a frame whose
  output would exceed `max` (it does not allocate the bomb first); `decompress`
  applies a 256 MiB default ceiling.
- **Oversized windows.** `StreamingDecoder` rejects frames declaring a window
  above its `window_log_max` (default 27 = 128 MiB). A frame that *declares* a
  large window but carries little content does **not** force a large allocation —
  the working set tracks the content, not the declared window.
- **Bounded streaming.** `StreamingDecoder` and `StreamingEncoder` keep memory
  bounded by roughly the window plus a block, independent of the total stream
  length.
- **Integrity.** XXH64 content checksums are verified when a frame carries one;
  dictionary-id mismatches and malformed/reserved-bit frames are rejected.

## Scope

In scope: panics, memory exhaustion, or undefined behavior triggered by crafted
input; incorrect output (a frame both we and libzstd accept but decode
differently); checksum-verification bypass. Out of scope: ratio/performance, and
the legacy zstd formats the crate intentionally does not implement (RFC 8878
only).
