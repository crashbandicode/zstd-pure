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

/// Hard ceiling on decode worker threads (mirrors the parallel encoder's cap),
/// so a pathologically large `n_jobs` can't spawn an unbounded number.
#[cfg(feature = "std")]
const MAX_DECODE_JOBS: usize = 256;

/// Decompress an entire seekable `archive` in parallel across up to `n_jobs`
/// worker threads, returning the full logical output. **Uncapped** — the output
/// is sized from the (untrusted) seek table's declared total, so use this only on
/// archives you trust; for untrusted input use [`decompress_seekable_parallel_capped`]
/// (this mirrors [`crate::decompress`] vs [`crate::decompress_capped`]).
///
/// Each data frame is an **independent** standard zstd frame, so the frames
/// decode concurrently and the result is byte-identical to serial decode +
/// concatenation. Per-frame checksums are verified. `std`-only (scoped threads);
/// the `no_std` build uses the serial [`crate::decompress`].
#[cfg(feature = "std")]
pub fn decompress_seekable_parallel(
    archive: &[u8],
    table: &SeekTable,
    n_jobs: usize,
) -> Result<Vec<u8>> {
    decompress_seekable_parallel_capped(archive, table, n_jobs, table.decompressed_size() as usize)
}

/// [`decompress_seekable_parallel`] with a hard ceiling on total decompressed
/// size — the decompression-bomb-safe variant for untrusted archives. Errors with
/// [`ZstdError::OutputTooLarge`] **before allocating** if the seek table's declared
/// total exceeds `max_output` (a corrupt table can claim an enormous size), and
/// caps each frame's decode as well.
///
/// Frames are partitioned into `n_jobs` contiguous, byte-balanced groups; the
/// output is pre-sized and `split_at_mut` into one disjoint region per group, so
/// there is no post-pass concatenation. (Each worker currently decodes a frame
/// into a temporary buffer and copies it into its region; a true zero-copy
/// decode-into-slice path — a `BlockState` that writes through a `&mut [u8]`
/// cursor — is a documented future optimization.) `n_jobs` is clamped to
/// `[1, min(frames, MAX_DECODE_JOBS)]`, so `0` runs single-threaded.
#[cfg(feature = "std")]
pub fn decompress_seekable_parallel_capped(
    archive: &[u8],
    table: &SeekTable,
    n_jobs: usize,
    max_output: usize,
) -> Result<Vec<u8>> {
    let frames = table.frames();
    // Bomb guard: reject an over-large declared total before the big allocation.
    if table.decompressed_size() > max_output as u64 {
        return Err(ZstdError::OutputTooLarge { limit: max_output });
    }
    let total = table.decompressed_size() as usize; // <= max_output, fits usize
    let mut out = vec![0u8; total];
    if frames.is_empty() {
        return Ok(out);
    }
    let jobs = n_jobs.clamp(1, frames.len().min(MAX_DECODE_JOBS));

    // Partition frames into `jobs` contiguous groups of ~equal decompressed bytes.
    // `bounds[g]..bounds[g + 1]` is group g; contiguous groups mean each maps to a
    // contiguous output region, so `split_at_mut` hands each worker a disjoint
    // slice (no overlap, no copy-back). `u128` math avoids overflow in the balance
    // comparison regardless of the (table-declared) sizes.
    let mut bounds = Vec::with_capacity(jobs + 1);
    bounds.push(0usize);
    let mut acc = 0u128;
    for (i, f) in frames.iter().enumerate() {
        acc += f.decompressed_size as u128;
        // Close the current group once it holds its share of the total bytes,
        // leaving enough frames for the remaining groups.
        let remaining_groups = jobs - bounds.len() + 1;
        let frames_left = frames.len() - (i + 1);
        if bounds.len() < jobs
            && frames_left >= remaining_groups - 1
            && acc * jobs as u128 >= total as u128 * bounds.len() as u128
        {
            bounds.push(i + 1);
        }
    }
    while bounds.len() <= jobs {
        bounds.push(frames.len());
    }

    std::thread::scope(|s| -> Result<()> {
        let mut handles = Vec::with_capacity(jobs);
        let mut rest = out.as_mut_slice();
        for g in 0..jobs {
            let grp = &frames[bounds[g]..bounds[g + 1]];
            let grp_bytes: usize = grp.iter().map(|f| f.decompressed_size as usize).sum();
            let (region, tail) = rest.split_at_mut(grp_bytes);
            rest = tail;
            handles.push(s.spawn(move || decode_frame_group(archive, grp, region)));
        }
        for h in handles {
            h.join().expect("seekable decode worker panicked")?;
        }
        Ok(())
    })?;
    Ok(out)
}

/// Decode each frame of `grp` (in order) into `region`, the contiguous output
/// slice that exactly spans the group's decompressed bytes. Offset arithmetic is
/// checked (a corrupt seek table can't trigger overflow/OOB), the stored checksum
/// is verified, and a decoded length that disagrees with the table is rejected.
#[cfg(feature = "std")]
fn decode_frame_group(archive: &[u8], grp: &[SeekFrame], region: &mut [u8]) -> Result<()> {
    let bad_offset = || ZstdError::Invalid {
        what: "seekable frame offset",
        detail: "compressed offset/size overflows or exceeds the archive".into(),
    };
    let mut off = 0usize;
    for f in grp {
        let start = usize::try_from(f.compressed_offset).map_err(|_| bad_offset())?;
        let end = start
            .checked_add(f.compressed_size as usize)
            .ok_or_else(bad_offset)?;
        if end > archive.len() {
            return Err(ZstdError::Truncated {
                what: "seekable frame",
                needed: end - archive.len(),
            });
        }
        let want = f.decompressed_size as usize;
        let decoded = decode_one(&archive[start..end], true, want + 1)?;
        if decoded.data.len() != want {
            return Err(ZstdError::Invalid {
                what: "seekable frame size",
                detail: "decoded length does not match the seek table".into(),
            });
        }
        if let Some(expected) = f.checksum {
            let computed = (xxh64(&decoded.data, 0) & 0xFFFF_FFFF) as u32;
            if computed != expected {
                return Err(ZstdError::ChecksumMismatch {
                    stored: expected,
                    computed,
                });
            }
        }
        let dst_end = off.checked_add(want).ok_or_else(bad_offset)?;
        region[off..dst_end].copy_from_slice(&decoded.data);
        off = dst_end;
    }
    Ok(())
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

    /// Parallel whole-archive decode must equal the serial decode (and the
    /// original) for every frame size, checksum setting, and worker count — the
    /// frames are independent, so partitioning them across threads can't change the
    /// result. `n_jobs` is swept past the frame count to exercise the clamp.
    #[test]
    fn parallel_decode_matches_serial() {
        for data in corpus() {
            for &fs in &[1usize, 64, 4096, 1 << 16] {
                for &checksum in &[false, true] {
                    let archive = compress_seekable(&data, fs, 9, checksum);
                    let table = SeekTable::parse(&archive).expect("parse seek table");
                    for &jobs in &[1usize, 2, 3, 8, 64] {
                        let got = decompress_seekable_parallel(&archive, &table, jobs)
                            .unwrap_or_else(|e| {
                                panic!("parallel decode (fs={fs}, jobs={jobs}): {e}")
                            });
                        assert_eq!(
                            got, data,
                            "parallel decode (fs={fs}, ck={checksum}, jobs={jobs})"
                        );
                    }
                }
            }
        }
    }

    /// Parallel decode must reject every malformed/abusive input rather than
    /// panic, OOM, or return wrong bytes: an over-large declared size (capped),
    /// a corrupted frame (with and without a checksum), a truncated archive, and
    /// it must run correctly at `n_jobs = 0` (clamped to serial).
    #[test]
    fn parallel_decode_rejects_corruption_truncation_and_overcap() {
        let data: Vec<u8> = (0..50_000u32)
            .flat_map(|i| (i % 251).to_le_bytes())
            .collect();

        // --- checksum on ---
        let archive = compress_seekable(&data, 4096, 9, true);
        let table = SeekTable::parse(&archive).unwrap();
        let total = table.decompressed_size() as usize;

        // n_jobs = 0 clamps to one worker — correct output, no panic.
        assert_eq!(
            decompress_seekable_parallel(&archive, &table, 0).unwrap(),
            data
        );

        // Bomb guard: a cap below the declared total errors *before* allocating.
        assert!(matches!(
            decompress_seekable_parallel_capped(&archive, &table, 4, total - 1),
            Err(ZstdError::OutputTooLarge { .. })
        ));
        // The exact cap (and one above) succeed.
        assert_eq!(
            decompress_seekable_parallel_capped(&archive, &table, 4, total).unwrap(),
            data
        );

        // A corrupted frame body fails (checksum mismatch / decode error / length).
        let mut bad = archive.clone();
        let f0 = table.frames()[0];
        bad[f0.compressed_offset as usize + f0.compressed_size as usize / 2] ^= 0xFF;
        assert!(decompress_seekable_parallel(&bad, &table, 4).is_err());

        // A truncated archive (frame bytes cut) fails — the table is from the full
        // archive, so a frame's end exceeds the truncated length.
        assert!(decompress_seekable_parallel(&archive[..archive.len() / 2], &table, 4).is_err());

        // --- checksum off: corruption still errors (decode failure / length) ---
        let archive2 = compress_seekable(&data, 4096, 9, false);
        let table2 = SeekTable::parse(&archive2).unwrap();
        let mut bad2 = archive2.clone();
        let g0 = table2.frames()[0];
        bad2[g0.compressed_offset as usize + 3] ^= 0xFF; // hit the frame header/entropy early
        assert!(decompress_seekable_parallel(&bad2, &table2, 4).is_err());
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
