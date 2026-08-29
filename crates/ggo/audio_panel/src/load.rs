//! Off-thread half of opening a file: decode it (or read a baked `.adp`
//! back) and reduce it to the fixed-size waveform the canvas paints.

use std::path::Path;
use std::sync::Arc;

use anyhow::{Context, Result};
use ggo_audio::Decoded;

/// Waveform resolution. Fixed at load rather than derived from the canvas
/// width so a three-minute track is walked once, not on every paint; the
/// canvas maps its columns onto these.
pub const WAVEFORM_BUCKETS: usize = 2048;

pub struct Loaded {
    pub decoded: Arc<Decoded>,
    /// `(min, max)` per bucket, [`WAVEFORM_BUCKETS`] of them (fewer for a
    /// clip shorter than that).
    pub waveform: Arc<Vec<(i16, i16)>>,
    /// For a `.adp`: the file itself -- it IS the baked form. `None` for a
    /// source file, whose bake the panel runs separately at the chosen
    /// rate.
    pub adp: Option<Arc<Vec<u8>>>,
}

pub fn is_adp(path: &Path) -> bool {
    path.extension()
        .is_some_and(|ext| ext.eq_ignore_ascii_case("adp"))
}

pub fn load(path: &Path) -> Result<Loaded> {
    let (decoded, adp) = if is_adp(path) {
        let bytes = std::fs::read(path).with_context(|| path.display().to_string())?;
        let decoded = ggo_audio::decode_adp(&bytes).with_context(|| path.display().to_string())?;
        (decoded, Some(Arc::new(bytes)))
    } else {
        (ggo_audio::decode(path)?, None)
    };
    let waveform = buckets(&decoded.samples, WAVEFORM_BUCKETS);
    Ok(Loaded {
        decoded: Arc::new(decoded),
        waveform: Arc::new(waveform),
        adp,
    })
}

/// `(min, max)` over `n` equal slices of `samples` (or one per sample when
/// there are fewer than `n`).
pub fn buckets(samples: &[i16], n: usize) -> Vec<(i16, i16)> {
    if samples.is_empty() || n == 0 {
        return Vec::new();
    }
    let n = n.min(samples.len());
    (0..n)
        .map(|i| {
            let start = i * samples.len() / n;
            let end = ((i + 1) * samples.len() / n).max(start + 1);
            let slice = &samples[start..end];
            let lo = slice.iter().copied().min().unwrap_or(0);
            let hi = slice.iter().copied().max().unwrap_or(0);
            (lo, hi)
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn buckets_take_the_extremes_of_each_slice() {
        let samples: Vec<i16> = (0..8)
            .map(|i| if i % 2 == 0 { -100 * i } else { 100 * i })
            .collect();
        // Slices of two: (0,100), (-200,300), (-400,500), (-600,700).
        assert_eq!(
            buckets(&samples, 4),
            vec![(0, 100), (-200, 300), (-400, 500), (-600, 700)]
        );
    }

    #[test]
    fn short_clips_get_one_bucket_per_sample_and_empty_gets_none() {
        assert_eq!(buckets(&[5, -5], 2048), vec![(5, 5), (-5, -5)]);
        assert!(buckets(&[], 2048).is_empty());
        assert!(buckets(&[1], 0).is_empty());
    }

    #[test]
    fn is_adp_is_case_insensitive_on_the_extension() {
        assert!(is_adp(Path::new("a/b.ADP")));
        assert!(!is_adp(Path::new("a/b.wav")));
    }
}
