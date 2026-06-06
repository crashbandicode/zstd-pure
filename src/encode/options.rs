//! Advanced compression options — caller overrides on top of the level defaults
//! (the analogue of libzstd's advanced `ZSTD_CCtx_setParameter` API).
//!
//! [`CompressOptions`] starts from a level (so the unset knobs match
//! [`compress`](crate::compress)) and lets the caller override individual cparams
//! and frame flags. [`compress_with_options`] resolves them — applying the same
//! small-input window shrink as the level path and clamping each override to a
//! valid range — then runs the normal block pipeline. Output is always a
//! conformant frame (libzstd and this crate's decoder both decode it).

#[allow(unused_imports)]
use crate::alloc_prelude::*;

use super::params::{self, CParams, Strategy, LDM_MAX_WINDOW_LOG, MAX_WINDOW_LOG, MIN_WINDOW_LOG};

/// Caller-tunable compression parameters. Build from a level and override only
/// what you need; every unset field defaults to the level's value, so
/// `CompressOptions::new(level)` reproduces [`compress`](crate::compress).
///
/// ```
/// # use zstd_pure::{CompressOptions, Strategy, compress_with_options};
/// let opts = CompressOptions::new(9).window_log(20).checksum(true);
/// let frame = compress_with_options(b"hello hello hello", &opts);
/// assert_eq!(zstd_pure::decompress(&frame).unwrap(), b"hello hello hello");
/// ```
#[derive(Debug, Clone, Copy)]
#[non_exhaustive]
pub struct CompressOptions {
    /// Base compression level (1–22; clamped) supplying defaults for unset fields.
    pub level: i32,
    /// Override the window log (back-reference reach + advertised `Window_Size`).
    pub window_log: Option<u32>,
    /// Override the match hash-table log.
    pub hash_log: Option<u32>,
    /// Override the chain/row-table log.
    pub chain_log: Option<u32>,
    /// Override the search depth log.
    pub search_log: Option<u32>,
    /// Override the minimum match length / finder hash width (3–6).
    pub min_match: Option<u32>,
    /// Override the optimal-parse `target_length`.
    pub target_length: Option<u32>,
    /// Override the parse strategy.
    pub strategy: Option<Strategy>,
    /// Append the XXH64 content checksum.
    pub checksum: bool,
    /// Emit the 4-byte frame magic (`false` = a magicless frame).
    pub magic: bool,
    /// Enable long-distance matching (grows the window like `compress_long`).
    pub long_distance: bool,
}

impl CompressOptions {
    /// Options for `level` with no overrides — equivalent to
    /// [`compress`](crate::compress) `(data, level, false, true)`.
    pub fn new(level: i32) -> Self {
        CompressOptions {
            level,
            window_log: None,
            hash_log: None,
            chain_log: None,
            search_log: None,
            min_match: None,
            target_length: None,
            strategy: None,
            checksum: false,
            magic: true,
            long_distance: false,
        }
    }

    /// Override the window log (clamped to the valid range on resolution).
    pub fn window_log(mut self, v: u32) -> Self {
        self.window_log = Some(v);
        self
    }
    /// Override the match hash-table log.
    pub fn hash_log(mut self, v: u32) -> Self {
        self.hash_log = Some(v);
        self
    }
    /// Override the chain/row-table log.
    pub fn chain_log(mut self, v: u32) -> Self {
        self.chain_log = Some(v);
        self
    }
    /// Override the search depth log.
    pub fn search_log(mut self, v: u32) -> Self {
        self.search_log = Some(v);
        self
    }
    /// Override the minimum match length / finder hash width (clamped to 3–6).
    pub fn min_match(mut self, v: u32) -> Self {
        self.min_match = Some(v);
        self
    }
    /// Override the optimal-parse `target_length`.
    pub fn target_length(mut self, v: u32) -> Self {
        self.target_length = Some(v);
        self
    }
    /// Override the parse [`Strategy`].
    pub fn strategy(mut self, s: Strategy) -> Self {
        self.strategy = Some(s);
        self
    }
    /// Set whether to append the content checksum.
    pub fn checksum(mut self, b: bool) -> Self {
        self.checksum = b;
        self
    }
    /// Set whether to emit the frame magic (`false` = magicless).
    pub fn magic(mut self, b: bool) -> Self {
        self.magic = b;
        self
    }
    /// Enable/disable long-distance matching.
    pub fn long_distance(mut self, b: bool) -> Self {
        self.long_distance = b;
        self
    }

    /// Resolve to concrete [`CParams`] for an input of `src_size`: the level
    /// defaults (with the small-input window shrink) then the clamped overrides.
    /// `window_log` is clamped to a valid, decoder-portable range; `min_match` to
    /// 3–6; the table logs are clamped by the finders downstream.
    pub(crate) fn resolve(&self, src_size: usize) -> CParams {
        let mut p = if self.long_distance {
            params::params_for_level_ldm(self.level, src_size)
        } else {
            params::params_for_level(self.level, src_size)
        };
        let win_max = if self.long_distance {
            LDM_MAX_WINDOW_LOG
        } else {
            MAX_WINDOW_LOG
        };
        if let Some(v) = self.window_log {
            p.window_log = v.clamp(MIN_WINDOW_LOG, win_max);
        }
        if let Some(v) = self.hash_log {
            p.hash_log = v;
        }
        if let Some(v) = self.chain_log {
            p.chain_log = v;
        }
        if let Some(v) = self.search_log {
            p.search_log = v;
        }
        if let Some(v) = self.min_match {
            p.min_match = v.clamp(3, 6);
        }
        if let Some(v) = self.target_length {
            p.target_length = v;
        }
        if let Some(s) = self.strategy {
            p.strategy = s;
        }
        p
    }
}

/// Compress `data` with caller-tuned [`CompressOptions`] — the advanced entry
/// point. Resolves the options to concrete parameters (level defaults + clamped
/// overrides) and runs the normal block pipeline (LDM when `opts.long_distance`).
/// The result is a conformant frame that libzstd and this crate's decoder decode;
/// `CompressOptions::new(level)` with no overrides equals
/// [`compress`](crate::compress)`(data, level, false, true)`.
pub fn compress_with_options(data: &[u8], opts: &CompressOptions) -> Vec<u8> {
    let params = opts.resolve(data.len());
    if opts.long_distance {
        // Bound the regular finder to its tables' reach, but never past the
        // advertised window (an override could shrink it below the level window).
        let level_reach = 1usize << params::params_for_level(opts.level, data.len()).window_log;
        let regular_reach = level_reach.min(1usize << params.window_log);
        super::frame::compress_long_with_params(
            data,
            &params,
            regular_reach,
            opts.checksum,
            opts.magic,
        )
    } else {
        super::frame::compress_with_params(data, &params, opts.checksum, opts.magic)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_match_level() {
        // No overrides => exactly the level cparams.
        for &level in &[1, 3, 9, 19] {
            for &n in &[100usize, 1 << 20] {
                let from_opts = CompressOptions::new(level).resolve(n);
                let from_level = params::params_for_level(level, n);
                assert_eq!(from_opts.window_log, from_level.window_log);
                assert_eq!(from_opts.strategy, from_level.strategy);
                assert_eq!(from_opts.hash_log, from_level.hash_log);
            }
        }
    }

    /// With no overrides the advanced path must be byte-identical to `compress`
    /// (it resolves to the same cparams and runs the same pipeline).
    #[test]
    fn default_options_equal_compress() {
        let data = b"the quick brown fox jumps over the lazy dog. ".repeat(500);
        for &level in &[1, 3, 9, 19] {
            assert_eq!(
                compress_with_options(&data, &CompressOptions::new(level)),
                crate::compress(&data, level, false, true),
                "default options must equal compress at level {level}"
            );
        }
    }

    /// Every override must still produce a conformant frame that round-trips
    /// through both our decoder and libzstd (the magicless case skips libzstd,
    /// which requires the magic).
    #[test]
    fn overrides_round_trip_through_both_decoders() {
        let data: Vec<u8> = b"FRES"
            .iter()
            .copied()
            .cycle()
            .take(3)
            .chain((0..40_000u32).flat_map(|i| (i.wrapping_mul(2654435761) % 257).to_le_bytes()))
            .collect();
        let cap = data.len() + 64;
        let cases = [
            CompressOptions::new(9).window_log(18),
            CompressOptions::new(9).strategy(Strategy::Fast),
            CompressOptions::new(3).strategy(Strategy::Btultra2),
            CompressOptions::new(9).checksum(true),
            CompressOptions::new(9).magic(false),
            CompressOptions::new(6).search_log(8).target_length(128),
            CompressOptions::new(12).min_match(6),
            CompressOptions::new(9).long_distance(true).window_log(21),
        ];
        for opts in cases {
            let frame = compress_with_options(&data, &opts);
            let got = if opts.magic {
                crate::decompress(&frame).expect("our decode")
            } else {
                crate::decompress_magicless_bytes(&frame, cap).expect("our magicless decode")
            };
            assert_eq!(got, data, "self round-trip failed for {opts:?}");
            if opts.magic {
                let by_lz = zstd::bulk::decompress(&frame, cap).expect("libzstd decode");
                assert_eq!(by_lz, data, "libzstd round-trip failed for {opts:?}");
            }
        }
    }

    #[test]
    fn overrides_apply_and_clamp() {
        let p = CompressOptions::new(9)
            .window_log(99) // clamped down to MAX_WINDOW_LOG
            .strategy(Strategy::Fast)
            .min_match(99) // clamped to 6
            .target_length(7)
            .resolve(1 << 20);
        assert_eq!(p.window_log, MAX_WINDOW_LOG);
        assert_eq!(p.strategy, Strategy::Fast);
        assert_eq!(p.min_match, 6);
        assert_eq!(p.target_length, 7);

        // Tiny window override is floored.
        let q = CompressOptions::new(9).window_log(1).resolve(1 << 20);
        assert_eq!(q.window_log, MIN_WINDOW_LOG);

        // LDM raises the window ceiling.
        let r = CompressOptions::new(9)
            .long_distance(true)
            .window_log(99)
            .resolve(1 << 20);
        assert_eq!(r.window_log, LDM_MAX_WINDOW_LOG);
    }
}
