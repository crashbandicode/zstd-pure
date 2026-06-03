//! Frame-level encoding — RFC 8878 §3.1.1.1.
//!
//! Writes a frame header, the block sequence, and the optional XXH64 content
//! checksum. The block bodies come from [`super::block`] (raw/RLE today;
//! compressed blocks land with the match finder in T2.3).

use super::super::frame::ZSTD_MAGIC;
use super::super::xxhash::xxh64;
use super::block::{
    write_compressed_block, write_compressed_block_ldm, write_huffman_literals_block,
    write_store_block, EncState, BLOCK_SIZE_MAX,
};
#[allow(unused_imports)]
use crate::alloc_prelude::*;

/// Window log used by the store-mode encoder. Raw/RLE blocks carry no
/// back-references, so the window only has to admit one full block (128 KiB =
/// `1 << 17`).
const STORE_WINDOW_LOG: u32 = 17;

/// Block-splitter recursion depth at the optimal-parse levels: a 128 KiB block
/// can split into up to `2^DEPTH` adjacent blocks when their statistics differ
/// enough to pay for the extra table headers (see [`super::block`]). Gated to
/// `Btopt`+ (L16+), where ratio is the priority and the optimal parse already
/// dominates the splitter's trial-encode cost. 0 elsewhere (no splitting).
const BLOCK_SPLIT_DEPTH: usize = 4;

/// The split depth for a level's strategy: the splitter runs only for the
/// optimal-parse strategies, leaving the fast/lazy levels byte-identical and
/// their throughput untouched.
pub(crate) fn split_depth_for(strategy: super::params::Strategy) -> usize {
    if strategy >= super::params::Strategy::Btopt {
        BLOCK_SPLIT_DEPTH
    } else {
        0
    }
}

/// Write the frame header (RFC 8878 §3.1.1.1.1).
///
/// Always emits a window descriptor (`Single_Segment_Flag = 0`) and pledges the
/// content size (4-byte form for `<= u32::MAX`, else 8-byte). `dict_id` of 0
/// means none (no `Dictionary_ID` field); a non-zero id is written little-endian
/// in the smallest field that holds it (1, 2, or 4 bytes), after the window
/// descriptor and before the content size — the byte order the decoder's
/// `parse_frame_header` reads.
fn write_frame_header(
    out: &mut Vec<u8>,
    content_size: u64,
    checksum: bool,
    window_log: u32,
    dict_id: u32,
) {
    // FCS field size: 2 = 4 bytes, 3 = 8 bytes.
    let fcs_flag: u8 = if content_size <= u32::MAX as u64 {
        2
    } else {
        3
    };
    // Dictionary_ID flag (FHD bits 0-1): smallest field that holds the id; the
    // decoder maps the flag to a size via `[0, 1, 2, 4]`.
    let (dict_id_flag, dict_id_size): (u8, usize) = match dict_id {
        0 => (0, 0),
        i if i <= 0xFF => (1, 1),
        i if i <= 0xFFFF => (2, 2),
        _ => (3, 4),
    };
    // Frame_Header_Descriptor: bits 6-7 fcs_flag, bit 5 single_segment (0),
    // bit 2 content_checksum, bits 0-1 dict_id_flag.
    let fhd = (fcs_flag << 6) | ((checksum as u8) << 2) | dict_id_flag;
    out.push(fhd);

    // Window descriptor: exponent in bits 3-7, mantissa (0) in bits 0-2.
    let exponent = window_log - 10;
    out.push((exponent as u8) << 3);

    // Dictionary_ID (little-endian), if any.
    out.extend_from_slice(&dict_id.to_le_bytes()[..dict_id_size]);

    match fcs_flag {
        2 => out.extend_from_slice(&(content_size as u32).to_le_bytes()),
        _ => out.extend_from_slice(&content_size.to_le_bytes()),
    }
}

/// Write a frame header for the **streaming** encoder, which does not know the
/// total content size up front (RFC 8878 §3.1.1.1.1, the unknown-size form).
///
/// `Single_Segment_Flag = 0` (a window descriptor is present) and
/// `Frame_Content_Size_flag = 0`, which together mean the `Frame_Content_Size`
/// field is **absent** — exactly the frame libzstd's `ZSTD_compressStream`
/// produces with `ContentSizeFlag(false)`. Our one-shot decoder, our
/// [`StreamingDecoder`](crate::StreamingDecoder), and libzstd all decode it (the
/// decoder simply grows its output rather than pre-sizing from a pledge). No
/// dictionary id is written.
pub(crate) fn write_frame_header_streaming(out: &mut Vec<u8>, checksum: bool, window_log: u32) {
    // FHD: fcs_flag = 0 (bits 6-7), single_segment = 0 (bit 5), content_checksum
    // (bit 2), dict_id_flag = 0 (bits 0-1).
    out.push((checksum as u8) << 2);
    // Window descriptor: exponent in bits 3-7, mantissa (0) in bits 0-2.
    out.push(((window_log - 10) as u8) << 3);
}

/// Compress `data` into a store-mode frame (raw/RLE blocks only). The output is
/// a fully spec-conformant Zstandard frame: libzstd decompresses it, and so
/// does this crate's decoder. `expect_magic = false` produces a magicless
/// frame (`ZSTD_f_zstd1_magicless`).
pub fn compress_store(data: &[u8], checksum: bool, expect_magic: bool) -> Vec<u8> {
    let mut out = Vec::with_capacity(data.len() + 32);
    if expect_magic {
        out.extend_from_slice(&ZSTD_MAGIC.to_le_bytes());
    }
    write_frame_header(&mut out, data.len() as u64, checksum, STORE_WINDOW_LOG, 0);

    if data.is_empty() {
        // A frame must contain at least one block; emit an empty last raw block.
        super::block::write_raw_block(&mut out, true, &[]);
    } else {
        let mut chunks = data.chunks(BLOCK_SIZE_MAX).peekable();
        while let Some(chunk) = chunks.next() {
            let last = chunks.peek().is_none();
            write_store_block(&mut out, last, chunk);
        }
    }

    if checksum {
        let digest = (xxh64(data, 0) & 0xFFFF_FFFF) as u32;
        out.extend_from_slice(&digest.to_le_bytes());
    }
    out
}

/// Compress `data` into a frame using LZ match finding + entropy coding (the
/// `fast` strategy). Each block independently picks the smallest of a fully
/// compressed block, a raw block, or an RLE block, so the output is always a
/// spec-conformant frame that libzstd and this crate's decoder both accept, and
/// is never larger than [`compress_store`].
///
/// `level` selects the compression parameters (window/hash sizes and, once the
/// stronger strategies land, the parse strategy) from the zstd level table; see
/// the `params` module. Today every level uses the `fast` finder, but `level`
/// already drives the window log (back-reference reach + frame header) and the
/// match-table size.
pub fn compress(data: &[u8], level: i32, checksum: bool, expect_magic: bool) -> Vec<u8> {
    let params = super::params::params_for_level(level, data.len());
    let window_log = params.window_log;
    let max_offset = 1usize << window_log;
    let split_depth = split_depth_for(params.strategy);

    let mut out = Vec::with_capacity(data.len() / 2 + 64);
    if expect_magic {
        out.extend_from_slice(&ZSTD_MAGIC.to_le_bytes());
    }
    write_frame_header(&mut out, data.len() as u64, checksum, window_log, 0);

    if data.is_empty() {
        super::block::write_raw_block(&mut out, true, &[]);
    } else {
        // Repeat offsets persist across blocks within a frame (the decoder only
        // updates them on compressed blocks). The match finder also persists,
        // so back-references span block boundaries up to the window. Thread both
        // here, committing a block's `rep` evolution only when we actually emit
        // it compressed — a store block leaves the decoder's `rep` untouched.
        // (The match table needs no rollback: a stored block's bytes are still
        // in the decoder's output, so indexing them stays valid.)
        // The repeat offsets and the previous compressed block's entropy tables
        // both persist across blocks (the decoder keeps them for repeat-offset
        // codes and "Repeat" table mode). Thread them as one `EncState`,
        // committing only when a block is actually emitted compressed.
        let mut state = EncState {
            rep: [1, 4, 8],
            seq: super::sequences::SeqCTables::default(),
            lit: None,
        };
        let mut finder = super::lz::Finder::new(&params);
        let n = data.len();
        let mut start = 0usize;
        while start < n {
            let end = (start + BLOCK_SIZE_MAX).min(n);
            let last = end == n;
            let mut store = Vec::new();
            write_store_block(&mut store, last, &data[start..end]);

            let mut comp = Vec::new();
            match write_compressed_block(
                &mut comp,
                last,
                data,
                start..end,
                &mut finder,
                max_offset,
                &state,
                split_depth,
            ) {
                Ok(next) if comp.len() < store.len() => {
                    state = next;
                    out.extend_from_slice(&comp);
                }
                _ => out.extend_from_slice(&store),
            }
            start = end;
        }
    }

    if checksum {
        let digest = (xxh64(data, 0) & 0xFFFF_FFFF) as u32;
        out.extend_from_slice(&digest.to_le_bytes());
    }
    out
}

/// Compress `data` with **long-distance matching** enabled — the opt-in T2.4
/// path. Like [`compress`], but a coarse whole-input index (the `ldm` module)
/// contributes long matches at offsets *beyond* the regular 8 MiB window, and
/// the frame advertises the larger window those offsets need (grown to cover the
/// input, up to `LDM_MAX_WINDOW_LOG` = 128 MiB — still within a
/// stock decoder's default `windowLogMax`). Best for large inputs with repeats
/// spaced farther apart than the regular window can reach; on a small input it
/// behaves like [`compress`] (the window stays small, the index finds nothing).
///
/// The output is a conformant frame that both libzstd (default `windowLogMax`
/// 27) and this crate's decoder accept. **Conformance:** this is the only entry
/// point that may advertise a `Window_Size` above the portable 8 MiB; see the
/// Conformance note in the README. The decoder is unchanged — LDM is purely an
/// encoder concern.
pub fn compress_long(data: &[u8], level: i32, checksum: bool, expect_magic: bool) -> Vec<u8> {
    let params = super::params::params_for_level_ldm(level, data.len());
    let window_log = params.window_log;
    let max_offset = 1usize << window_log;
    let split_depth = split_depth_for(params.strategy);
    // LDM contributes only matches *beyond* the regular finder's reach (the
    // level's nominal, un-bumped window); nearer matches are left to the finder,
    // which parses them better. This keeps LDM purely additive — it never forces
    // a near match where the regular parse would do better.
    let regular_reach = 1usize << super::params::params_for_level(level, data.len()).window_log;

    let mut out = Vec::with_capacity(data.len() / 2 + 64);
    if expect_magic {
        out.extend_from_slice(&ZSTD_MAGIC.to_le_bytes());
    }
    write_frame_header(&mut out, data.len() as u64, checksum, window_log, 0);

    if data.is_empty() {
        super::block::write_raw_block(&mut out, true, &[]);
    } else {
        let mut state = EncState {
            rep: [1, 4, 8],
            seq: super::sequences::SeqCTables::default(),
            lit: None,
        };
        let mut finder = super::lz::Finder::new(&params);
        // The coarse LDM index persists across the frame's blocks, so a long
        // match can reference any earlier block within the advertised window.
        let mut ldm = super::ldm::LdmState::new(window_log);
        let n = data.len();
        let mut start = 0usize;
        while start < n {
            let end = (start + BLOCK_SIZE_MAX).min(n);
            let last = end == n;
            let mut store = Vec::new();
            write_store_block(&mut store, last, &data[start..end]);

            // Generate this block's long matches (updating the index), then parse
            // the block with them injected and the gaps filled by the finder.
            let matches = ldm.generate(data, start..end, regular_reach, max_offset);
            let mut comp = Vec::new();
            // The regular finder searches only the regular window (`regular_reach`);
            // matches beyond it come from the LDM index above. Passing the full LDM
            // window would exceed the binary tree's array size (sized to the regular
            // window) and alias its node links, corrupting it — so the finder is
            // bounded to its tables' reach, while the LDM matches carry the large
            // offsets the frame's advertised window admits.
            match write_compressed_block_ldm(
                &mut comp,
                last,
                data,
                start..end,
                &mut finder,
                regular_reach,
                &state,
                split_depth,
                &matches,
            ) {
                Ok(next) if comp.len() < store.len() => {
                    state = next;
                    out.extend_from_slice(&comp);
                }
                _ => out.extend_from_slice(&store),
            }
            start = end;
        }
    }

    if checksum {
        let digest = (xxh64(data, 0) & 0xFFFF_FFFF) as u32;
        out.extend_from_slice(&digest.to_le_bytes());
    }
    out
}

/// Compress `data` into a frame primed with `dict`, so back-references can reach
/// into the dictionary's content. The output is a spec-conformant frame that
/// libzstd (loaded with the same dictionary) and this crate's
/// [`decompress_with_dict`](crate::decompress_with_dict) both decode.
///
/// Handles both dictionary flavours uniformly:
///
/// * **Raw-content** — the content primes the match window; the repeat offsets
///   start from the default `[1, 4, 8]`; no `Dictionary_ID` is written.
/// * **Structured / tagged** — additionally, the dictionary's three repeat
///   offsets seed the running `rep` (the decoder seeds the identical values) and
///   the dictionary id is recorded in the frame header so a decoder can check
///   it. The dictionary's preset entropy tables are *not* referenced — every
///   block describes its own tables (this encoder emits neither sequence-table
///   Repeat mode nor treeless literals), so no entropy coupling is needed for
///   correctness; exploiting them is a future ratio refinement.
///
/// `level` selects the parse strategy and table sizes; the window is sized for
/// the dictionary and input together and widened to span the whole dictionary
/// (see `params::params_for_level_with_dict`). Never larger than a
/// dictionary-primed store would be: each block falls back to raw/RLE if the
/// compressed form isn't smaller.
pub fn compress_with_dict(
    data: &[u8],
    dict: &crate::dict::Dictionary,
    level: i32,
    checksum: bool,
    expect_magic: bool,
) -> Vec<u8> {
    let content = dict.content();
    let dict_len = content.len();
    let params = super::params::params_for_level_with_dict(level, data.len(), dict_len);
    let window_log = params.window_log;
    let max_offset = 1usize << window_log;
    let split_depth = split_depth_for(params.strategy);

    let mut out = Vec::with_capacity(data.len() / 2 + 64);
    if expect_magic {
        out.extend_from_slice(&ZSTD_MAGIC.to_le_bytes());
    }
    write_frame_header(&mut out, data.len() as u64, checksum, window_log, dict.id());

    if data.is_empty() {
        super::block::write_raw_block(&mut out, true, &[]);
    } else {
        // The search space is the combined `[dict content || input]` buffer: the
        // dictionary prefix is pre-existing history (primed into the match tables
        // below, never emitted as literals), exactly as the decoder preloads it.
        // Offsets are absolute positions into this buffer, so back-references
        // reach into the dictionary; only the input range is parsed into blocks.
        let mut combined = Vec::with_capacity(dict_len + data.len());
        combined.extend_from_slice(content);
        combined.extend_from_slice(data);

        // Seed block 1's state from a structured dictionary: the repeat offsets,
        // the literals Huffman table (rebuilt as an encode table), and the three
        // sequence FSE tables (rebuilt from the dictionary's normalized counts).
        // The decoder seeds the identical decode tables, so block 1 can warm-start
        // via Treeless literals / Repeat-mode sequence tables — the small-file
        // win. A raw-content dictionary carries no entropy, so it starts cold.
        let mut state = match dict.entropy() {
            Some(e) => EncState {
                rep: e.rep,
                seq: super::sequences::SeqCTables {
                    ll: Some(super::fse::build_ctable(&e.ll_nc.0, e.ll_nc.1, e.ll_nc.2)),
                    of: Some(super::fse::build_ctable(&e.of_nc.0, e.of_nc.1, e.of_nc.2)),
                    ml: Some(super::fse::build_ctable(&e.ml_nc.0, e.ml_nc.1, e.ml_nc.2)),
                },
                lit: super::huff::code_table_from_huff(&e.huff).ok(),
            },
            None => EncState {
                rep: [1, 4, 8],
                seq: super::sequences::SeqCTables::default(),
                lit: None,
            },
        };
        let mut finder = super::lz::Finder::new(&params);
        finder.prime(&combined, dict_len, max_offset);

        let n = data.len();
        let mut start = 0usize;
        while start < n {
            let end = (start + BLOCK_SIZE_MAX).min(n);
            let last = end == n;
            let mut store = Vec::new();
            write_store_block(&mut store, last, &data[start..end]);

            let mut comp = Vec::new();
            let range = (dict_len + start)..(dict_len + end);
            match write_compressed_block(
                &mut comp,
                last,
                &combined,
                range,
                &mut finder,
                max_offset,
                &state,
                split_depth,
            ) {
                Ok(next) if comp.len() < store.len() => {
                    state = next;
                    out.extend_from_slice(&comp);
                }
                _ => out.extend_from_slice(&store),
            }
            start = end;
        }
    }

    if checksum {
        let digest = (xxh64(data, 0) & 0xFFFF_FFFF) as u32;
        out.extend_from_slice(&digest.to_le_bytes());
    }
    out
}

/// Number of distinct byte values in `chunk` (early-outs at 2 — all we need to
/// decide whether a Huffman literals block is even applicable).
fn at_least_two_distinct(chunk: &[u8]) -> bool {
    let mut seen = [false; 256];
    let mut first = None;
    for &b in chunk {
        match first {
            None => {
                first = Some(b);
                seen[b as usize] = true;
            }
            Some(f) if b != f && !seen[b as usize] => return true,
            _ => {}
        }
    }
    false
}

/// Compress `data` into a frame using **Huffman-coded literals** blocks (no LZ
/// match finding yet — every block is `[Huffman literals][0 sequences]`). Each
/// block independently picks the smaller of its Huffman or store encoding, so
/// the output is never larger than [`compress_store`] and is always a
/// spec-conformant frame (libzstd and this crate's decoder both accept it).
///
/// This is the T2.1a entropy-encoder milestone; ratio-competitive output needs
/// the match finder (T2.3).
pub fn compress_huffman_literals(data: &[u8], checksum: bool, expect_magic: bool) -> Vec<u8> {
    let mut out = Vec::with_capacity(data.len() + 32);
    if expect_magic {
        out.extend_from_slice(&ZSTD_MAGIC.to_le_bytes());
    }
    write_frame_header(&mut out, data.len() as u64, checksum, STORE_WINDOW_LOG, 0);

    if data.is_empty() {
        super::block::write_raw_block(&mut out, true, &[]);
    } else {
        let mut chunks = data.chunks(BLOCK_SIZE_MAX).peekable();
        while let Some(chunk) = chunks.next() {
            let last = chunks.peek().is_none();
            let mut store = Vec::new();
            write_store_block(&mut store, last, chunk);

            let mut huff = Vec::new();
            let use_huff = at_least_two_distinct(chunk)
                && write_huffman_literals_block(&mut huff, last, chunk).is_ok()
                && huff.len() < store.len();
            out.extend_from_slice(if use_huff { &huff } else { &store });
        }
    }

    if checksum {
        let digest = (xxh64(data, 0) & 0xFFFF_FFFF) as u32;
        out.extend_from_slice(&digest.to_le_bytes());
    }
    out
}
