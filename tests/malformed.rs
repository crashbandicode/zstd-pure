//! Malformed-input typed-error corpus.
//!
//! The randomized never-panic sweeps (`corpus.rs`, `proptest.rs`) prove the
//! decoder doesn't crash on garbage. This file locks the *typed error contract*
//! for a set of hand-crafted, minimal malformations across the frame-header,
//! block-header, dictionary, and seekable layers — so a refactor that silently
//! changes which error a given corruption produces is caught, and the public
//! `ZstdError` variants stay a stable, documented part of the API.

use zstd_pure::{decompress, Dictionary, SeekTable, ZstdError};

/// The 4-byte zstd frame magic (little-endian 0xFD2FB528).
const MAGIC: [u8; 4] = [0x28, 0xB5, 0x2F, 0xFD];

/// A frame: the magic followed by `tail` (a hand-built header + blocks).
fn frame(tail: &[u8]) -> Vec<u8> {
    let mut v = MAGIC.to_vec();
    v.extend_from_slice(tail);
    v
}

// ---- Frame header ----------------------------------------------------------

#[test]
fn bad_magic_is_typed() {
    assert!(matches!(
        decompress(&[0xDE, 0xAD, 0xBE, 0xEF, 0x00]),
        Err(ZstdError::BadMagic(_))
    ));
}

#[test]
fn truncated_magic_is_typed() {
    assert!(matches!(
        decompress(&[0x28, 0xB5]),
        Err(ZstdError::Truncated {
            what: "frame magic",
            ..
        })
    ));
}

#[test]
fn reserved_fhd_bit_is_typed() {
    // Frame_Header_Descriptor bit 3 is reserved (must be zero).
    assert!(matches!(
        decompress(&frame(&[0x08])),
        Err(ZstdError::ReservedBit("frame header descriptor"))
    ));
}

#[test]
fn truncated_window_descriptor_is_typed() {
    // FHD=0x00 => not single-segment, so a window descriptor byte must follow.
    assert!(matches!(
        decompress(&frame(&[0x00])),
        Err(ZstdError::Truncated {
            what: "window descriptor",
            ..
        })
    ));
}

#[test]
fn truncated_dictionary_id_is_typed() {
    // FHD=0x23 => single-segment (no window byte) + 4-byte Dictionary_ID, but only
    // one id byte follows.
    assert!(matches!(
        decompress(&frame(&[0x23, 0xAA])),
        Err(ZstdError::Truncated {
            what: "dictionary id",
            ..
        })
    ));
}

#[test]
fn truncated_content_size_is_typed() {
    // FHD=0xA0 => single-segment + 4-byte Frame_Content_Size; none follows.
    assert!(matches!(
        decompress(&frame(&[0xA0])),
        Err(ZstdError::Truncated {
            what: "frame content size",
            ..
        })
    ));
}

// ---- Block header ----------------------------------------------------------

#[test]
fn truncated_block_header_is_typed() {
    // Valid header (FHD=0, window=0), then only one of three block-header bytes.
    assert!(matches!(
        decompress(&frame(&[0x00, 0x00, 0x00])),
        Err(ZstdError::Truncated {
            what: "block header",
            ..
        })
    ));
}

#[test]
fn reserved_block_type_is_typed() {
    // Block_Type = 3 is reserved: header byte0 = 0b110 = 0x06, size 0.
    assert!(matches!(
        decompress(&frame(&[0x00, 0x00, 0x06, 0x00, 0x00])),
        Err(ZstdError::Invalid {
            what: "block type",
            ..
        })
    ));
}

#[test]
fn raw_block_body_past_eof_is_typed() {
    // Last raw block declaring 10 bytes with no body: v = 1(last) | (10<<3) = 0x51.
    assert!(matches!(
        decompress(&frame(&[0x00, 0x00, 0x51, 0x00, 0x00])),
        Err(ZstdError::Truncated {
            what: "raw block body",
            ..
        })
    ));
}

// ---- Dictionary ------------------------------------------------------------

#[test]
fn parse_short_or_unmagic_dictionary_falls_back_to_raw() {
    // < 8 bytes or no DICT_MAGIC => treated as raw content (libzstd ZSTD_dct_auto):
    // id 0, never an error.
    assert_eq!(
        Dictionary::parse(b"not a structured dict")
            .expect("auto-raw fallback")
            .id(),
        0
    );
    assert_eq!(Dictionary::parse(b"tiny").expect("auto-raw").id(), 0);
}

#[test]
fn structured_dictionary_header_without_tables_errors() {
    // DICT_MAGIC (0xEC30A437) + a 4-byte id, then no entropy tables: a structured
    // header must be rejected, not silently accepted. The exact variant depends on
    // the Huffman/FSE table internals, so assert only that it errors.
    let mut bytes = vec![0x37, 0xA4, 0x30, 0xEC];
    bytes.extend_from_slice(&[0x01, 0x00, 0x00, 0x00]); // id = 1
    assert!(Dictionary::parse(&bytes).is_err());
}

// ---- Seekable --------------------------------------------------------------

#[test]
fn seekable_truncated_footer_is_typed() {
    assert!(matches!(
        SeekTable::parse(&[0u8; 8]),
        Err(ZstdError::Truncated {
            what: "seekable footer",
            ..
        })
    ));
}

#[test]
fn seekable_bad_magic_is_typed() {
    // Long enough for a footer, but the trailing 4 bytes aren't Seekable_Magic.
    assert!(matches!(
        SeekTable::parse(&[0u8; 20]),
        Err(ZstdError::Invalid {
            what: "seekable magic",
            ..
        })
    ));
}
