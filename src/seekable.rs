//! Seekable format — the zstd `contrib/seekable_format` extension.
//!
//! This implements a zstd *contrib* extension (not part of the core RFC 8878
//! spec). The seek-table parser ([`SeekTable::parse`]) is fuzzed against arbitrary
//! bytes (never panics) and round-trip + random-access correctness is tested, so
//! it carries the same robustness bar as the core decoder.
//!
//! An archive is a sequence of **independent** standard zstd frames, each
//! compressing up to `frame_size` decompressed bytes, followed by a **seek
//! table** stored in a skippable frame. Because the data frames are ordinary
//! zstd frames and the seek table is skippable, a stock decoder reads the whole
//! archive as a normal multi-frame stream (it skips the table) — so
//! [`crate::decompress`] and libzstd both reconstruct the original. The seek
//! table is the index for **random access**: [`SeekTable::parse`] reads it from
//! the end of the archive, and [`decompress_seekable_frame`] decodes a single
//! frame, so a reader can jump to an offset without decompressing the prefix.
//!
//! Layout (all little-endian):
//! ```text
//! frame_0 .. frame_N  | Seek_Table (a skippable frame)
//! Seek_Table = Skippable_Magic(0x184D2A5E) Frame_Size  Entries  Footer
//! Entry      = Compressed_Size  Decompressed_Size  [Checksum]
//! Footer     = Number_Of_Frames  Descriptor(bit7=checksum)  Seekable_Magic(0x8F92EAB1)
//! ```

#[allow(unused_imports)]
use crate::alloc_prelude::*;
use crate::error::{Result, ZstdError};
use crate::xxhash::xxh64;
use crate::{compress, decode_one};

/// Skippable-frame magic the seek table is stored under (`0x184D2A50 | 0xE`).
const SEEK_TABLE_SKIPPABLE_MAGIC: u32 = 0x184D_2A5E;
/// Magic at the very end of the seek-table footer.
const SEEKABLE_MAGIC: u32 = 0x8F92_EAB1;
/// `Number_Of_Frames (4) + Seek_Table_Descriptor (1) + Seekable_Magic (4)`.
const FOOTER_SIZE: usize = 9;
/// Skippable-frame header: `Skippable_Magic (4) + Frame_Size (4)`.
const SKIPPABLE_HEADER: usize = 8;

#[inline]
fn read_u32(b: &[u8], at: usize) -> u32 {
    u32::from_le_bytes([b[at], b[at + 1], b[at + 2], b[at + 3]])
}

/// Compress `data` into the seekable format: each `frame_size`-byte chunk becomes
/// an independent standard zstd frame (via [`crate::compress`] at `level`),
/// followed by a seek-table skippable frame. With `checksum`, each entry stores
/// the low 32 bits of its chunk's `XXH64` for integrity, verified on read.
///
/// The result is a conformant multi-frame zstd stream — [`crate::decompress`]
/// and any standard decoder reconstruct `data` by concatenating the frames and
/// skipping the table — while [`SeekTable`] + [`decompress_seekable_frame`] give
/// random access. Empty input yields just an (empty) seek table.
pub fn compress_seekable(data: &[u8], frame_size: usize, level: i32, checksum: bool) -> Vec<u8> {
    let frame_size = frame_size.max(1);
    let mut out = Vec::new();
    let mut entries: Vec<(u32, u32, u32)> = Vec::new();
    for chunk in data.chunks(frame_size) {
        let frame = compress(chunk, level, false, true);
        let ck = if checksum {
            (xxh64(chunk, 0) & 0xFFFF_FFFF) as u32
        } else {
            0
        };
        entries.push((frame.len() as u32, chunk.len() as u32, ck));
        out.extend_from_slice(&frame);
    }
    write_seek_table(&mut out, &entries, checksum);
    out
}

/// Append the seek-table skippable frame for `entries` (`(compressed_size,
/// decompressed_size, checksum)`).
fn write_seek_table(out: &mut Vec<u8>, entries: &[(u32, u32, u32)], checksum: bool) {
    let entry_size = if checksum { 12 } else { 8 };
    let content = entries.len() * entry_size + FOOTER_SIZE;
    out.extend_from_slice(&SEEK_TABLE_SKIPPABLE_MAGIC.to_le_bytes());
    out.extend_from_slice(&(content as u32).to_le_bytes());
    for &(cs, ds, ck) in entries {
        out.extend_from_slice(&cs.to_le_bytes());
        out.extend_from_slice(&ds.to_le_bytes());
        if checksum {
            out.extend_from_slice(&ck.to_le_bytes());
        }
    }
    out.extend_from_slice(&(entries.len() as u32).to_le_bytes());
    out.push(if checksum { 0x80 } else { 0 });
    out.extend_from_slice(&SEEKABLE_MAGIC.to_le_bytes());
}

/// One frame's location in a seekable archive: where its compressed bytes sit,
/// and what range of the logical (decompressed) output it produces.
#[derive(Debug, Clone, Copy)]
pub struct SeekFrame {
    /// Byte offset of the frame within the archive.
    pub compressed_offset: u64,
    /// Compressed byte length of the frame.
    pub compressed_size: u32,
    /// Byte offset this frame's output starts at in the logical stream.
    pub decompressed_offset: u64,
    /// Decompressed byte length of the frame.
    pub decompressed_size: u32,
    /// Stored low-32-bit `XXH64` of the frame's content, if the table carries it.
    pub checksum: Option<u32>,
}

/// The parsed seek table of a seekable archive — the random-access index.
pub struct SeekTable {
    frames: Vec<SeekFrame>,
}

impl SeekTable {
    /// Parse the seek table from the end of `archive`: read the 9-byte footer
    /// (validating the `Seekable_Magic`), locate and validate the skippable
    /// frame, then read the per-frame entries. Errors if the magics are absent or
    /// the sizes don't add up.
    pub fn parse(archive: &[u8]) -> Result<SeekTable> {
        let end = archive.len();
        if end < SKIPPABLE_HEADER + FOOTER_SIZE {
            return Err(ZstdError::Truncated {
                what: "seekable footer",
                needed: SKIPPABLE_HEADER + FOOTER_SIZE - end,
            });
        }
        if read_u32(archive, end - 4) != SEEKABLE_MAGIC {
            return Err(ZstdError::Invalid {
                what: "seekable magic",
                detail: "footer magic mismatch".into(),
            });
        }
        let has_checksum = archive[end - 5] & 0x80 != 0;
        let num_frames = read_u32(archive, end - 9) as usize;
        let entry_size = if has_checksum { 12usize } else { 8 };
        let content = num_frames
            .checked_mul(entry_size)
            .and_then(|n| n.checked_add(FOOTER_SIZE))
            .ok_or(ZstdError::Invalid {
                what: "seek table",
                detail: "frame count overflow".into(),
            })?;
        let frame_total = SKIPPABLE_HEADER + content;
        if frame_total > end {
            return Err(ZstdError::Invalid {
                what: "seek table",
                detail: "table larger than archive".into(),
            });
        }
        let skip_start = end - frame_total;
        if read_u32(archive, skip_start) != SEEK_TABLE_SKIPPABLE_MAGIC {
            return Err(ZstdError::Invalid {
                what: "seek table",
                detail: "skippable magic mismatch".into(),
            });
        }
        if read_u32(archive, skip_start + 4) as usize != content {
            return Err(ZstdError::Invalid {
                what: "seek table",
                detail: "skippable size mismatch".into(),
            });
        }

        let mut frames = Vec::with_capacity(num_frames);
        let mut p = skip_start + SKIPPABLE_HEADER;
        let mut comp_off = 0u64;
        let mut decomp_off = 0u64;
        for _ in 0..num_frames {
            let cs = read_u32(archive, p);
            let ds = read_u32(archive, p + 4);
            let ck = if has_checksum {
                Some(read_u32(archive, p + 8))
            } else {
                None
            };
            p += entry_size;
            frames.push(SeekFrame {
                compressed_offset: comp_off,
                compressed_size: cs,
                decompressed_offset: decomp_off,
                decompressed_size: ds,
                checksum: ck,
            });
            comp_off += cs as u64;
            decomp_off += ds as u64;
        }
        // The data frames must exactly fill the space before the seek table.
        if comp_off != skip_start as u64 {
            return Err(ZstdError::Invalid {
                what: "seek table",
                detail: "frame sizes don't sum to the data region".into(),
            });
        }
        Ok(SeekTable { frames })
    }

    /// The per-frame index entries, in order.
    pub fn frames(&self) -> &[SeekFrame] {
        &self.frames
    }

    /// Number of data frames.
    pub fn num_frames(&self) -> usize {
        self.frames.len()
    }

    /// Total decompressed length of the archive.
    pub fn decompressed_size(&self) -> u64 {
        self.frames
            .last()
            .map_or(0, |f| f.decompressed_offset + f.decompressed_size as u64)
    }

    /// Index of the frame whose decompressed range contains `offset`, or `None`
    /// if `offset` is at/after the end. Binary search over the (ordered) frames.
    pub fn frame_for_offset(&self, offset: u64) -> Option<usize> {
        if offset >= self.decompressed_size() {
            return None;
        }
        let i = self
            .frames
            .partition_point(|f| f.decompressed_offset + f.decompressed_size as u64 <= offset);
        (i < self.frames.len()).then_some(i)
    }
}

/// Decompress a single data frame of `archive` by its seek-table `index` — the
/// random-access primitive (a reader maps an offset to a frame with
/// [`SeekTable::frame_for_offset`], decodes just that frame, and takes the slice
/// it needs). If the table stored a checksum for the frame, it is verified.
pub fn decompress_seekable_frame(
    archive: &[u8],
    table: &SeekTable,
    index: usize,
) -> Result<Vec<u8>> {
    let f = table.frames.get(index).ok_or(ZstdError::Invalid {
        what: "seek frame index",
        detail: "out of range".into(),
    })?;
    let start = f.compressed_offset as usize;
    let end = start + f.compressed_size as usize;
    if end > archive.len() {
        return Err(ZstdError::Truncated {
            what: "seekable frame",
            needed: end - archive.len(),
        });
    }
    let decoded = decode_one(&archive[start..end], true, f.decompressed_size as usize + 1)?;
    if let Some(expected) = f.checksum {
        let computed = (xxh64(&decoded.data, 0) & 0xFFFF_FFFF) as u32;
        if computed != expected {
            return Err(ZstdError::ChecksumMismatch {
                stored: expected,
                computed,
            });
        }
    }
    Ok(decoded.data)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::decompress;

    fn corpus() -> Vec<Vec<u8>> {
        let text = b"the quick brown fox jumps over the lazy dog. ".repeat(300);
        let structured: Vec<u8> = (0..50_000u32)
            .flat_map(|i| (i % 251).to_le_bytes())
            .collect();
        vec![
            Vec::new(),
            b"x".to_vec(),
            b"hello seekable world".to_vec(),
            text,
            structured,
        ]
    }

    /// A seekable archive is a conformant multi-frame stream: our own decoder
    /// reconstructs the original by concatenating the data frames and skipping
    /// the seek table, and libzstd decodes each data frame on its own.
    #[test]
    fn seekable_round_trips_and_frames_are_standard() {
        for data in corpus() {
            for &fs in &[1usize, 64, 4096, 1 << 16] {
                for &checksum in &[false, true] {
                    let archive = compress_seekable(&data, fs, 9, checksum);
                    // Whole-archive decode through our (multi-frame + skippable) decoder.
                    assert_eq!(
                        decompress(&archive).unwrap(),
                        data,
                        "self decode (fs={fs}, ck={checksum})"
                    );

                    let table = SeekTable::parse(&archive).expect("parse seek table");
                    assert_eq!(table.decompressed_size(), data.len() as u64);
                    let expected_frames = data.len().div_ceil(fs);
                    assert_eq!(table.num_frames(), expected_frames, "frame count (fs={fs})");

                    // Each data frame is a standard zstd frame libzstd decodes.
                    for (i, f) in table.frames().iter().enumerate() {
                        let s = f.compressed_offset as usize;
                        let e = s + f.compressed_size as usize;
                        let chunk = &data[f.decompressed_offset as usize
                            ..f.decompressed_offset as usize + f.decompressed_size as usize];
                        let by_lz = zstd::bulk::decompress(&archive[s..e], chunk.len() + 64)
                            .expect("libzstd decodes a seekable data frame");
                        assert_eq!(by_lz, chunk, "libzstd frame {i} (fs={fs})");
                    }
                }
            }
        }
    }

    /// Random access: every offset maps to the frame covering it, and decoding
    /// just that frame yields the right bytes — without touching the prefix.
    #[test]
    fn random_access_by_offset() {
        let data: Vec<u8> = (0..40_000u32)
            .flat_map(|i| (i.wrapping_mul(2654435761) >> 13).to_le_bytes())
            .collect();
        let archive = compress_seekable(&data, 4096, 6, true);
        let table = SeekTable::parse(&archive).unwrap();
        assert!(table.num_frames() > 1, "expected multiple frames");

        for &off in &[0u64, 1, 4095, 4096, 10_000, (data.len() - 1) as u64] {
            let idx = table.frame_for_offset(off).expect("offset within range");
            let f = table.frames()[idx];
            assert!(
                off >= f.decompressed_offset
                    && off < f.decompressed_offset + f.decompressed_size as u64
            );
            let frame_data = decompress_seekable_frame(&archive, &table, idx).unwrap();
            let want = &data[f.decompressed_offset as usize
                ..f.decompressed_offset as usize + f.decompressed_size as usize];
            assert_eq!(frame_data, want, "frame {idx} content");
            // The byte at `off` is reachable within the decoded frame.
            let local = (off - f.decompressed_offset) as usize;
            assert_eq!(frame_data[local], data[off as usize]);
        }
        assert_eq!(
            table.frame_for_offset(data.len() as u64),
            None,
            "past-the-end offset"
        );
    }

    /// A stored checksum catches a corrupted frame on read.
    #[test]
    fn checksum_detects_corruption() {
        let data = b"checksum this seekable payload ".repeat(400);
        let mut archive = compress_seekable(&data, 1024, 3, true);
        let table = SeekTable::parse(&archive).unwrap();
        // Corrupt a byte inside the first frame's compressed body (past the header).
        let f0 = table.frames()[0];
        archive[f0.compressed_offset as usize + 6] ^= 0xFF;
        // Re-parse (the seek table is intact) and decode the tampered frame.
        let table = SeekTable::parse(&archive).unwrap();
        assert!(
            decompress_seekable_frame(&archive, &table, 0).is_err(),
            "corruption must be detected"
        );
    }

    /// A buffer without a valid footer is rejected, not mis-parsed.
    #[test]
    fn rejects_non_seekable() {
        assert!(SeekTable::parse(&[]).is_err());
        assert!(SeekTable::parse(&[0u8; 4]).is_err());
        let plain = compress(b"not a seekable archive", 3, false, true);
        assert!(SeekTable::parse(&plain).is_err());
    }
}
