//! Frame decoding — RFC 8878 §3.1.1.1: frame header parse, the block loop,
//! and the optional XXH64 content checksum.

use super::block::{self, BlockState};
use super::dict::Dictionary;
use super::error::{Result, ZstdError};
use super::sequences::SeqTables;
use super::xxhash::xxh64;
#[allow(unused_imports)]
use crate::alloc_prelude::*;

/// Standard Zstandard frame magic (`0xFD2FB528`, little-endian `28 B5 2F FD`).
pub const ZSTD_MAGIC: u32 = 0xFD2F_B528;

/// Skippable-frame magics are `0x184D2A50 ..= 0x184D2A5F`.
const SKIPPABLE_MAGIC_MASK: u32 = 0xFFFF_FFF0;
const SKIPPABLE_MAGIC: u32 = 0x184D_2A50;

/// Default output ceiling when a frame doesn't pledge a content size (256 MiB).
pub const DEFAULT_MAX_OUTPUT: usize = 256 << 20;

/// A decoded frame and how many input bytes it consumed.
pub struct DecodedFrame {
    pub data: Vec<u8>,
    pub consumed: usize,
}

/// Parsed Zstandard frame header (RFC 8878 §3.1.1.1.1).
///
/// Produced by [`frame_header`] / [`frame_header_magicless`] for buffer sizing
/// and inspection without decoding the frame body — the analog of libzstd's
/// `ZSTD_getFrameHeader` / `ZSTD_getFrameContentSize`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FrameHeader {
    /// Total bytes the header occupies (including the 4-byte magic when parsed
    /// via [`frame_header`]; header-only in the magicless case).
    pub header_len: usize,
    /// Whether a 4-byte XXH64 content checksum trails the frame.
    pub has_checksum: bool,
    /// The pledged decompressed size, if the frame declared one.
    pub content_size: Option<u64>,
    /// The decoder window size (the maximum back-reference distance / minimum
    /// history buffer the decoder must retain).
    pub window_size: u64,
    /// The referenced dictionary id (0 = none).
    pub dictionary_id: u32,
}

fn parse_frame_header(src: &[u8]) -> Result<FrameHeader> {
    if src.is_empty() {
        return Err(ZstdError::Truncated {
            what: "frame header descriptor",
            needed: 1,
        });
    }
    let fhd = src[0];
    let dict_id_flag = fhd & 3;
    let content_checksum = (fhd >> 2) & 1 != 0;
    if (fhd >> 3) & 1 != 0 {
        return Err(ZstdError::ReservedBit("frame header descriptor"));
    }
    let single_segment = (fhd >> 5) & 1 != 0;
    let fcs_flag = (fhd >> 6) & 3;
    let mut o = 1usize;

    let window_size_from_desc = if !single_segment {
        if src.len() <= o {
            return Err(ZstdError::Truncated {
                what: "window descriptor",
                needed: 1,
            });
        }
        let wd = src[o];
        o += 1;
        let exponent = (wd >> 3) as u32;
        let mantissa = (wd & 7) as u64;
        let window_log = 10 + exponent;
        let window_base = 1u64 << window_log;
        Some(window_base + (window_base / 8) * mantissa)
    } else {
        None
    };

    let dict_id_size = [0usize, 1, 2, 4][dict_id_flag as usize];
    if src.len() < o + dict_id_size {
        return Err(ZstdError::Truncated {
            what: "dictionary id",
            needed: o + dict_id_size - src.len(),
        });
    }
    let mut dictionary_id = 0u32;
    for k in 0..dict_id_size {
        dictionary_id |= (src[o + k] as u32) << (8 * k);
    }
    o += dict_id_size;

    let fcs_size = match fcs_flag {
        0 => {
            if single_segment {
                1
            } else {
                0
            }
        }
        1 => 2,
        2 => 4,
        _ => 8,
    };
    if src.len() < o + fcs_size {
        return Err(ZstdError::Truncated {
            what: "frame content size",
            needed: o + fcs_size - src.len(),
        });
    }
    let frame_content_size = if fcs_size == 0 {
        None
    } else {
        let mut v = 0u64;
        for k in 0..fcs_size {
            v |= (src[o + k] as u64) << (8 * k);
        }
        if fcs_size == 2 {
            v += 256;
        }
        Some(v)
    };
    o += fcs_size;

    let window_size = if single_segment {
        frame_content_size.unwrap_or(0)
    } else {
        window_size_from_desc.unwrap_or(0)
    };

    Ok(FrameHeader {
        header_len: o,
        has_checksum: content_checksum,
        content_size: frame_content_size,
        window_size,
        dictionary_id,
    })
}

/// Parse a standard (magic-prefixed) frame header without decoding the body.
///
/// `header_len` in the result includes the 4-byte magic. Returns an error for a
/// skippable frame (use [`decode_one`] to skip those) or a bad magic.
pub fn frame_header(src: &[u8]) -> Result<FrameHeader> {
    if src.len() < 4 {
        return Err(ZstdError::Truncated {
            what: "frame magic",
            needed: 4 - src.len(),
        });
    }
    let magic = u32::from_le_bytes([src[0], src[1], src[2], src[3]]);
    if magic != ZSTD_MAGIC {
        return Err(ZstdError::BadMagic(magic));
    }
    let mut h = parse_frame_header(&src[4..])?;
    h.header_len += 4;
    Ok(h)
}

/// Parse a **magicless** frame header (`ZSTD_f_zstd1_magicless`) without
/// decoding the body. `header_len` excludes any magic (there is none).
pub fn frame_header_magicless(src: &[u8]) -> Result<FrameHeader> {
    parse_frame_header(src)
}

/// Decode one frame from the front of `src`. If `expect_magic`, a 4-byte magic
/// (and skippable frames) is parsed first; otherwise `src` starts at the frame
/// header descriptor (magicless mode).
pub fn decode_one(src: &[u8], expect_magic: bool, max_output: usize) -> Result<DecodedFrame> {
    decode_one_with_dict(src, expect_magic, max_output, None)
}

/// Like [`decode_one`] but priming the decode with an optional dictionary
/// (preloaded window history + any preset entropy tables and repeat offsets).
pub fn decode_one_with_dict(
    src: &[u8],
    expect_magic: bool,
    max_output: usize,
    dict: Option<&Dictionary>,
) -> Result<DecodedFrame> {
    let mut pos = 0usize;
    if expect_magic {
        if src.len() < 4 {
            return Err(ZstdError::Truncated {
                what: "frame magic",
                needed: 4 - src.len(),
            });
        }
        let magic = u32::from_le_bytes([src[0], src[1], src[2], src[3]]);
        if magic & SKIPPABLE_MAGIC_MASK == SKIPPABLE_MAGIC {
            if src.len() < 8 {
                return Err(ZstdError::Truncated {
                    what: "skippable frame size",
                    needed: 8 - src.len(),
                });
            }
            let len = u32::from_le_bytes([src[4], src[5], src[6], src[7]]) as usize;
            let consumed = 8 + len;
            if src.len() < consumed {
                return Err(ZstdError::Truncated {
                    what: "skippable frame body",
                    needed: consumed - src.len(),
                });
            }
            return Ok(DecodedFrame {
                data: Vec::new(),
                consumed,
            });
        }
        if magic != ZSTD_MAGIC {
            return Err(ZstdError::BadMagic(magic));
        }
        pos = 4;
    }

    let header = parse_frame_header(&src[pos..])?;
    pos += header.header_len;

    let cap = match header.content_size {
        Some(n) => (n as usize).min(max_output),
        None => max_output,
    };
    // Pre-size the output to the pledged content size to avoid repeated reallocation
    // on large frames — but bound the up-front allocation by the *input* size (×8)
    // so a tiny/malicious frame claiming a huge `content_size` can't force a large
    // allocation. The actual decompression-bomb guard is `max_output`, enforced as
    // each block is produced; this only sizes the initial buffer. Always allow ≥1 MiB.
    let reserve = cap.min(src.len().saturating_mul(8).max(1 << 20));
    let mut state = BlockState {
        out: Vec::with_capacity(reserve),
        dict_len: 0,
        max_output,
        huff: None,
        seq: SeqTables::default(),
        rep: [1, 4, 8],
    };

    if let Some(d) = dict {
        // A frame that names a dictionary id must match the supplied dict; a
        // zero frame id (dict id omitted) accepts any dictionary.
        if header.dictionary_id != 0 && d.id() != 0 && header.dictionary_id != d.id() {
            return Err(ZstdError::Dictionary(format!(
                "frame references dictionary id {} but dictionary is id {}",
                header.dictionary_id,
                d.id()
            )));
        }
        // Preload the content as window history so back-references reach it.
        state.out.extend_from_slice(d.content());
        state.dict_len = d.content().len();
        // Structured dictionaries prime the entropy tables + repeat offsets,
        // used by the first block's "Repeat" / treeless modes.
        if let Some(e) = d.entropy() {
            state.huff = Some(e.huff.clone());
            state.seq = e.tables.clone();
            state.rep = e.rep;
        }
    } else if header.dictionary_id != 0 {
        // The frame references a dictionary by id but none was supplied: refuse
        // rather than decode against missing history (which would yield wrong
        // bytes or a late offset error). Matches the `ZstdError::Dictionary`
        // contract documented in `error.rs`.
        return Err(ZstdError::Dictionary(format!(
            "frame references dictionary id {} but no dictionary was supplied",
            header.dictionary_id
        )));
    }
    let dict_len = state.dict_len;

    loop {
        let header = block::read_header(&src[pos..])?;
        pos += 3;
        match header.block_type {
            0 => {
                if src.len() < pos + header.block_size {
                    return Err(ZstdError::Truncated {
                        what: "raw block body",
                        needed: pos + header.block_size - src.len(),
                    });
                }
                state.decode_raw(&src[pos..pos + header.block_size])?;
                pos += header.block_size;
            }
            1 => {
                if src.len() <= pos {
                    return Err(ZstdError::Truncated {
                        what: "RLE block byte",
                        needed: 1,
                    });
                }
                state.decode_rle(src[pos], header.block_size)?;
                pos += 1;
            }
            2 => {
                if src.len() < pos + header.block_size {
                    return Err(ZstdError::Truncated {
                        what: "compressed block body",
                        needed: pos + header.block_size - src.len(),
                    });
                }
                state.decode_compressed(&src[pos..pos + header.block_size])?;
                pos += header.block_size;
            }
            _ => {
                return Err(ZstdError::Invalid {
                    what: "block type",
                    detail: "reserved block type 3".into(),
                })
            }
        }
        if header.last {
            break;
        }
    }

    // `state.out` carries any dictionary history at the front; the real output
    // is everything past `dict_len`.
    let output_len = state.out.len() - dict_len;
    if let Some(n) = header.content_size {
        if output_len as u64 != n {
            return Err(ZstdError::Invalid {
                what: "frame content size",
                detail: format!("declared {n}, decoded {output_len}"),
            });
        }
    }

    if header.has_checksum {
        if src.len() < pos + 4 {
            return Err(ZstdError::Truncated {
                what: "content checksum",
                needed: pos + 4 - src.len(),
            });
        }
        let stored = u32::from_le_bytes([src[pos], src[pos + 1], src[pos + 2], src[pos + 3]]);
        let computed = (xxh64(&state.out[dict_len..], 0) & 0xFFFF_FFFF) as u32;
        if stored != computed {
            return Err(ZstdError::ChecksumMismatch { stored, computed });
        }
        pos += 4;
    }

    // Drop the dictionary prefix, returning only the frame's own output.
    let mut data = state.out;
    data.drain(..dict_len);
    Ok(DecodedFrame {
        data,
        consumed: pos,
    })
}

/// Decompress a standard Zstandard stream (one or more frames, with magic),
/// concatenating the output of each frame.
pub fn decompress(src: &[u8]) -> Result<Vec<u8>> {
    decompress_capped(src, DEFAULT_MAX_OUTPUT)
}

/// Decompress a standard stream with an explicit **total** output ceiling across
/// all frames. Each frame is decoded against the budget still remaining, so a
/// multi-frame stream (each frame individually under the cap) cannot together
/// exceed `max_output` — a decompression-bomb guard for concatenated frames.
pub fn decompress_capped(src: &[u8], max_output: usize) -> Result<Vec<u8>> {
    let mut out = Vec::new();
    let mut pos = 0usize;
    while pos < src.len() {
        let remaining = max_output - out.len();
        let frame = decode_one(&src[pos..], true, remaining)?;
        out.extend_from_slice(&frame.data);
        pos += frame.consumed;
    }
    Ok(out)
}

/// Decompress a single **magicless** frame (`ZSTD_f_zstd1_magicless`), returning
/// the bytes and the number of input bytes the frame consumed.
pub fn decompress_magicless(src: &[u8], max_output: usize) -> Result<DecodedFrame> {
    decode_one(src, false, max_output)
}

/// Decompress a standard stream using a dictionary (raw-content or structured).
/// The dictionary primes every frame in the stream.
pub fn decompress_with_dict(src: &[u8], dict: &Dictionary, max_output: usize) -> Result<Vec<u8>> {
    let mut out = Vec::new();
    let mut pos = 0usize;
    while pos < src.len() {
        let frame = decode_one_with_dict(&src[pos..], true, max_output, Some(dict))?;
        out.extend_from_slice(&frame.data);
        pos += frame.consumed;
    }
    Ok(out)
}

/// Decompress a single **magicless** frame using a dictionary.
pub fn decompress_magicless_with_dict(
    src: &[u8],
    dict: &Dictionary,
    max_output: usize,
) -> Result<DecodedFrame> {
    decode_one_with_dict(src, false, max_output, Some(dict))
}
