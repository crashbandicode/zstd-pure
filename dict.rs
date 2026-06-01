//! Dictionary support for decoding — RFC 8478 §5.
//!
//! Two flavours:
//!
//! * **Raw-content** ([`Dictionary::raw`]): the bytes are pure window-history
//!   prefix. The decoder preloads them as history (so back-references can reach
//!   into them) and starts with the default repeat offsets `[1, 4, 8]` and no
//!   preset entropy tables.
//! * **Structured / tagged** ([`Dictionary::parse`], magic `0xEC30A437`): a
//!   `[magic][dict_id u32][entropy tables][content]` layout. The entropy tables
//!   are a Huff0 literals table, then FSE tables for Offset / Match_Length /
//!   Literals_Length, then three little-endian `u32` repeat offsets. A frame
//!   whose first block selects the "Repeat" entropy mode (or treeless literals)
//!   reuses these preset tables, and the initial repeat offsets are the dict's.

use super::error::{Result, ZstdError};
use super::fse;
use super::huff::{self, HuffTable};
use super::sequences::SeqTables;

/// Structured-dictionary magic (`0xEC30A437`, little-endian `37 A4 30 EC`).
pub const DICT_MAGIC: u32 = 0xEC30_A437;

/// Preset entropy state carried by a structured dictionary.
#[derive(Debug, Clone)]
pub(crate) struct DictEntropy {
    pub huff: HuffTable,
    pub tables: SeqTables,
    pub rep: [u32; 3],
}

/// A parsed Zstandard dictionary, ready to prime a decode.
#[derive(Debug, Clone)]
pub struct Dictionary {
    id: u32,
    content: Vec<u8>,
    entropy: Option<DictEntropy>,
}

impl Dictionary {
    /// Wrap raw bytes as a content-only dictionary (no magic, no entropy
    /// tables, default repeat offsets). This is also how libzstd treats a
    /// dictionary buffer that does not begin with the structured magic.
    pub fn raw(bytes: &[u8]) -> Self {
        Dictionary {
            id: 0,
            content: bytes.to_vec(),
            entropy: None,
        }
    }

    /// Parse a dictionary buffer. If it begins with [`DICT_MAGIC`] the entropy
    /// tables + repeat offsets are decoded (structured dictionary); otherwise
    /// the whole buffer is taken as raw content (matching libzstd's
    /// `ZSTD_dct_auto` behaviour).
    pub fn parse(bytes: &[u8]) -> Result<Self> {
        if bytes.len() < 8
            || u32::from_le_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]) != DICT_MAGIC
        {
            return Ok(Self::raw(bytes));
        }
        let id = u32::from_le_bytes([bytes[4], bytes[5], bytes[6], bytes[7]]);
        let mut p = 8usize;

        // Entropy tables, in spec order: Huffman (literals), then FSE for
        // Offset (max accuracy log 8), Match_Length (9), Literals_Length (9).
        let (hufftable, used) = huff::read_table(&bytes[p..])?;
        p += used;
        let (of, used) = fse::read_dtable(&bytes[p..], 8)?;
        p += used;
        let (ml, used) = fse::read_dtable(&bytes[p..], 9)?;
        p += used;
        let (ll, used) = fse::read_dtable(&bytes[p..], 9)?;
        p += used;

        // Three little-endian u32 repeat offsets (Offset_1, _2, _3).
        if bytes.len() < p + 12 {
            return Err(ZstdError::Truncated {
                what: "dictionary repeat offsets",
                needed: p + 12 - bytes.len(),
            });
        }
        let mut rep = [0u32; 3];
        for (k, slot) in rep.iter_mut().enumerate() {
            let o = p + k * 4;
            *slot = u32::from_le_bytes([bytes[o], bytes[o + 1], bytes[o + 2], bytes[o + 3]]);
        }
        p += 12;
        let content = bytes[p..].to_vec();

        // Repeat offsets must be non-zero and reach no further back than the
        // dictionary content (they prime back-references into that history).
        for &r in &rep {
            if r == 0 || r as usize > content.len() {
                return Err(ZstdError::Dictionary(format!(
                    "invalid repeat offset {r} for {}-byte content",
                    content.len()
                )));
            }
        }

        Ok(Dictionary {
            id,
            content,
            entropy: Some(DictEntropy {
                huff: hufftable,
                tables: SeqTables {
                    ll: Some(ll),
                    of: Some(of),
                    ml: Some(ml),
                },
                rep,
            }),
        })
    }

    /// The dictionary id (0 for a raw-content dictionary).
    pub fn id(&self) -> u32 {
        self.id
    }

    /// The window-history content bytes.
    pub fn content(&self) -> &[u8] {
        &self.content
    }

    /// The preset entropy state, if this is a structured dictionary.
    pub(crate) fn entropy(&self) -> Option<&DictEntropy> {
        self.entropy.as_ref()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::zstd_pure::decompress_with_dict;

    /// Compress `data` with libzstd + `dict_bytes`, decode with us + the same
    /// dictionary, and assert agreement.
    fn round_trip(data: &[u8], dict_bytes: &[u8], level: i32) {
        let mut c = zstd::bulk::Compressor::with_dictionary(level, dict_bytes).unwrap();
        let comp = c.compress(data).unwrap();
        let dict = Dictionary::parse(dict_bytes).unwrap();
        let got = decompress_with_dict(&comp, &dict, 1 << 24)
            .unwrap_or_else(|e| panic!("decode (level {level}): {e}"));
        assert_eq!(got, data, "mismatch at level {level}");
    }

    #[test]
    fn raw_content_dict_round_trips() {
        // A dictionary with no magic is raw content; both libzstd and our
        // parser treat it that way. Sharing substrings makes it useful.
        let dict = b"the quick brown fox jumps over the lazy dog. ".repeat(20);
        let data = b"the quick brown fox is feeling very lazy today. ".repeat(60);
        let parsed = Dictionary::parse(&dict).unwrap();
        assert_eq!(parsed.id(), 0);
        assert!(parsed.entropy().is_none());
        for level in [1, 3, 9, 19] {
            round_trip(&data, &dict, level);
        }
    }

    /// Build a corpus of small related records (good training material).
    fn training_samples() -> Vec<Vec<u8>> {
        (0..600u32)
            .map(|i| {
                format!(
                    "{{\"id\":{i},\"name\":\"item_{}\",\"kind\":\"weapon\",\"atk\":{},\"price\":{}}}\n",
                    i % 41,
                    (i * 7) % 200,
                    (i * 13) % 5000
                )
                .into_bytes()
            })
            .collect()
    }

    #[test]
    fn trained_structured_dict_round_trips() {
        let samples = training_samples();
        let dict_bytes = zstd::dict::from_samples(&samples, 8 * 1024).expect("train dict");
        // A trained dict is structured: magic + entropy + non-zero id.
        let magic = u32::from_le_bytes([dict_bytes[0], dict_bytes[1], dict_bytes[2], dict_bytes[3]]);
        assert_eq!(magic, DICT_MAGIC);
        let dict = Dictionary::parse(&dict_bytes).expect("parse trained dict");
        assert!(dict.entropy().is_some(), "structured dict must carry entropy");
        assert_ne!(dict.id(), 0);

        for s in samples.iter().take(80) {
            round_trip(s, &dict_bytes, 19);
            round_trip(s, &dict_bytes, 3);
        }
    }

    #[test]
    fn dict_id_mismatch_errors() {
        let samples = training_samples();
        let dict_bytes = zstd::dict::from_samples(&samples, 8 * 1024).expect("train dict");
        // Compress a sample with the real dict (the frame records its id).
        let mut c = zstd::bulk::Compressor::with_dictionary(19, &dict_bytes).unwrap();
        let comp = c.compress(&samples[0]).unwrap();

        // Forge a different dictionary: same entropy/content, different id.
        let mut other = dict_bytes.clone();
        other[4] ^= 0xFF;
        let wrong = Dictionary::parse(&other).expect("parse forged dict");
        assert_ne!(wrong.id(), Dictionary::parse(&dict_bytes).unwrap().id());
        assert!(matches!(
            decompress_with_dict(&comp, &wrong, 1 << 24),
            Err(ZstdError::Dictionary(_))
        ));

        // The correct dictionary still decodes it.
        let right = Dictionary::parse(&dict_bytes).unwrap();
        assert_eq!(
            decompress_with_dict(&comp, &right, 1 << 24).unwrap(),
            samples[0]
        );
    }
}
