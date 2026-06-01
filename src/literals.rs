//! Literals section decoding — RFC 8478 §3.1.1.3.1.
//!
//! Handles all four literal block types (Raw, RLE, Compressed, Treeless). The
//! decoded Huffman table is threaded through `cache` so a Treeless block can
//! reuse the previous block's table.

use super::error::{Result, ZstdError};
use super::huff::{self, HuffTable};

/// Decode the literals section at the front of `src`. Returns the literal bytes
/// and the number of bytes consumed from `src`.
pub fn decode(src: &[u8], cache: &mut Option<HuffTable>) -> Result<(Vec<u8>, usize)> {
    if src.is_empty() {
        return Err(ZstdError::Truncated {
            what: "literals header",
            needed: 1,
        });
    }
    let b0 = src[0];
    let block_type = b0 & 3;
    let size_format = (b0 >> 2) & 3;

    match block_type {
        0 | 1 => {
            // Raw / RLE.
            let (regen, hdr) = match size_format {
                0 | 2 => ((b0 >> 3) as usize, 1),
                1 => {
                    need(src, 2)?;
                    (((b0 >> 4) as usize) | ((src[1] as usize) << 4), 2)
                }
                _ => {
                    need(src, 3)?;
                    (
                        ((b0 >> 4) as usize)
                            | ((src[1] as usize) << 4)
                            | ((src[2] as usize) << 12),
                        3,
                    )
                }
            };
            if block_type == 0 {
                need(src, hdr + regen)?;
                Ok((src[hdr..hdr + regen].to_vec(), hdr + regen))
            } else {
                need(src, hdr + 1)?;
                Ok((vec![src[hdr]; regen], hdr + 1))
            }
        }
        _ => {
            // Compressed (2) / Treeless (3).
            let (regen, comp, hdr, four) = parse_compressed_header(src, b0, size_format)?;
            need(src, hdr + comp)?;
            let region = &src[hdr..hdr + comp];
            let stream = if block_type == 2 {
                let (table, tbytes) = huff::read_table(region)?;
                *cache = Some(table);
                &region[tbytes..]
            } else {
                if cache.is_none() {
                    return Err(ZstdError::Invalid {
                        what: "treeless literals",
                        detail: "no cached Huffman table to reuse".into(),
                    });
                }
                region
            };
            let table = cache.as_ref().unwrap();
            let literals = if four {
                huff::decode_4stream(table, stream, regen)?
            } else {
                huff::decode_1stream(table, stream, regen)?
            };
            Ok((literals, hdr + comp))
        }
    }
}

/// Parse a compressed/treeless literals header → (regen_size, compressed_size,
/// header_bytes, four_streams).
fn parse_compressed_header(
    src: &[u8],
    b0: u8,
    size_format: u8,
) -> Result<(usize, usize, usize, bool)> {
    Ok(match size_format {
        0 | 1 => {
            need(src, 3)?;
            let v = (b0 as u32) | ((src[1] as u32) << 8) | ((src[2] as u32) << 16);
            let regen = ((v >> 4) & 0x3FF) as usize;
            let comp = ((v >> 14) & 0x3FF) as usize;
            (regen, comp, 3, size_format == 1)
        }
        2 => {
            need(src, 4)?;
            let v = u32::from_le_bytes([b0, src[1], src[2], src[3]]);
            let regen = ((v >> 4) & 0x3FFF) as usize;
            let comp = ((v >> 18) & 0x3FFF) as usize;
            (regen, comp, 4, true)
        }
        _ => {
            need(src, 5)?;
            let v = (b0 as u64)
                | ((src[1] as u64) << 8)
                | ((src[2] as u64) << 16)
                | ((src[3] as u64) << 24)
                | ((src[4] as u64) << 32);
            let regen = ((v >> 4) & 0x3FFFF) as usize;
            let comp = ((v >> 22) & 0x3FFFF) as usize;
            (regen, comp, 5, true)
        }
    })
}

#[inline]
fn need(src: &[u8], n: usize) -> Result<()> {
    if src.len() < n {
        Err(ZstdError::Truncated {
            what: "literals section",
            needed: n - src.len(),
        })
    } else {
        Ok(())
    }
}
