//! Typed errors for the pure-Rust Zstandard codec.
//!
//! Kept `std`- and `thiserror`-only so this module can be lifted into a
//! standalone crate later (no dependency on the rest of `nx-layout-toolbox`).

#[allow(unused_imports)]
use crate::alloc_prelude::*;
use thiserror::Error;

/// An error decoding (or, later, encoding) a Zstandard stream.
#[derive(Debug, Error)]
pub enum ZstdError {
    /// Input ended before a structure could be fully read.
    #[error("zstd: unexpected end of input (need {needed} more byte(s) for {what})")]
    Truncated {
        /// What was being read when input ran out.
        what: &'static str,
        /// How many more bytes were required.
        needed: usize,
    },

    /// The 4-byte frame magic was not `0xFD2FB528`.
    #[error("zstd: bad frame magic 0x{0:08x} (expected 0xfd2fb528)")]
    BadMagic(u32),

    /// A reserved field was set to a value the spec forbids.
    #[error("zstd: reserved bit set in {0}")]
    ReservedBit(&'static str),

    /// A block / literals / sequences field was out of range.
    #[error("zstd: invalid {what}: {detail}")]
    Invalid {
        /// The field or structure that was invalid.
        what: &'static str,
        /// A human-readable detail.
        detail: String,
    },

    /// The decoded size exceeded the caller-provided ceiling.
    #[error("zstd: output exceeds the {limit}-byte ceiling")]
    OutputTooLarge {
        /// The ceiling that was exceeded.
        limit: usize,
    },

    /// An FSE/Huffman table was malformed (bad accuracy log, weights, etc.).
    #[error("zstd: corrupt entropy table: {0}")]
    CorruptTable(String),

    /// A back-reference offset pointed before the start of the window.
    #[error("zstd: offset {offset} exceeds history {history} (corrupt sequence)")]
    OffsetTooLarge {
        /// The requested copy offset.
        offset: usize,
        /// The number of bytes available behind the cursor.
        history: usize,
    },

    /// The trailing content checksum did not match the decoded content.
    #[error("zstd: content checksum mismatch (frame 0x{stored:08x} != computed 0x{computed:08x})")]
    ChecksumMismatch {
        /// The 32-bit checksum stored in the frame.
        stored: u32,
        /// The low 32 bits of XXH64 of the decoded content.
        computed: u32,
    },

    /// A dictionary was required (the frame referenced a dict id) but none was
    /// supplied, or the supplied dictionary was malformed.
    #[error("zstd: dictionary error: {0}")]
    Dictionary(String),
}

/// Convenience alias for this module's fallible operations.
pub type Result<T> = core::result::Result<T, ZstdError>;
