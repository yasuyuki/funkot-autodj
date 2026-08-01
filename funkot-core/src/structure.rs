//! Stage 2 structure signals derived from a sequence of [`crate::features::BarFeatures`].
//!
//! `analysis.rs`'s section detector works entirely on *level* (is this bar
//! louder/brighter than that one). These signals instead ask whether bar
//! content is *changing* or *repeating*, independent of level:
//!
//! - [`self_similarity_matrix`] + [`checkerboard_novelty`]: Foote's
//!   checkerboard-kernel novelty over a bar-to-bar cosine self-similarity
//!   matrix (SSM). A boundary shows up as a novelty peak regardless of
//!   whether the section got louder or quieter — the SSM only cares that
//!   "before" and "after" stopped resembling each other.
//! - [`loop_similarity`]: cosine similarity between bar `i` and bar `i-lag`
//!   for `lag` in {1,2,4} bars — high when a short pattern is looping
//!   (typical machine-rhythm intro/outro), low when material keeps changing
//!   (typical main/vocal section).
//! - [`mahalanobis_from_prefix`]: fits a diagonal-covariance Gaussian to the
//!   first `prefix_len` bars ("this is what the intro sounds like") and
//!   scores every bar's distance from it. A sustained rise marks where the
//!   track stops resembling its own intro.
//!
//! # Outro / time-reversal
//!
//! [`crate::analysis::section_bar_starts`] already returns *backward* bar
//! starts nearest-to-the-file-end first (bar 0 = the last bar of the file).
//! So calling [`compute`] on an outro's `[BarFeatures]` in bar order already
//! matches "index 0 = intro-like anchor, index N = deep into the track" —
//! exactly what the intro side gets for free. No separate reversal step is
//! needed; the same code path serves both sides.

use crate::features::BarFeatures;

/// Checkerboard novelty kernel half-width, in bars (~1 phrase).
pub const NOVELTY_HALF_WIDTH_SHORT: usize = 8;
/// Checkerboard novelty kernel half-width, in bars (~2 phrases).
pub const NOVELTY_HALF_WIDTH_LONG: usize = 16;
/// Bars used to fit the "this is the intro" prefix model.
pub const DEFAULT_PREFIX_BARS: usize = 8;

/// Structure signals for one side (intro or outro), one entry per bar.
#[derive(Debug, Clone)]
pub struct StructureSignals {
    /// Checkerboard novelty over the chroma SSM (pitch-class content change).
    pub chroma_novelty_w8: Vec<f64>,
    pub chroma_novelty_w16: Vec<f64>,
    /// Checkerboard novelty over the 7-band log-energy SSM.
    pub band_novelty_w8: Vec<f64>,
    pub band_novelty_w16: Vec<f64>,
    /// Checkerboard novelty over the 32-dim (16 low + 16 high) rhythm-pattern SSM.
    pub rhythm_novelty_w8: Vec<f64>,
    pub rhythm_novelty_w16: Vec<f64>,
    /// Cosine similarity of each bar's rhythm pattern to the bar 1/2/4 bars
    /// earlier (`NAN` where there is not enough history).
    pub loop_1: Vec<f64>,
    pub loop_2: Vec<f64>,
    pub loop_4: Vec<f64>,
    /// Mahalanobis distance of each bar from a diagonal-covariance model
    /// fit to the first [`DEFAULT_PREFIX_BARS`] (or `prefix_len` passed to
    /// [`compute`]) bars, over band+chroma+tonality+voiced_frac.
    pub prefix_mahalanobis: Vec<f64>,
}

/// Cosine similarity, `0.0` if either vector has ~zero norm.
pub fn cosine(a: &[f64], b: &[f64]) -> f64 {
    debug_assert_eq!(a.len(), b.len());
    let dot: f64 = a.iter().zip(b).map(|(x, y)| x * y).sum();
    let na: f64 = a.iter().map(|x| x * x).sum::<f64>().sqrt();
    let nb: f64 = b.iter().map(|x| x * x).sum::<f64>().sqrt();
    if na < 1e-12 || nb < 1e-12 {
        0.0
    } else {
        (dot / (na * nb)).clamp(-1.0, 1.0)
    }
}

/// Full `n x n` cosine self-similarity matrix.
pub fn self_similarity_matrix(feats: &[Vec<f64>]) -> Vec<Vec<f64>> {
    let n = feats.len();
    let mut m = vec![vec![0.0; n]; n];
    for i in 0..n {
        m[i][i] = cosine(&feats[i], &feats[i]);
        for j in (i + 1)..n {
            let s = cosine(&feats[i], &feats[j]);
            m[i][j] = s;
            m[j][i] = s;
        }
    }
    m
}

/// Foote checkerboard-kernel novelty over an SSM. `half_width` bars each
/// side; `0.0` where there is not enough context on either side (edges).
///
/// For each candidate boundary `i`, sums same-side similarity (before-before
/// + after-after) minus cross-side similarity (before-after), i.e. a
/// same-minus-different-neighborhood score — independent of whether the SSM
/// values themselves are high or low, only whether they *change* at `i`.
pub fn checkerboard_novelty(ssm: &[Vec<f64>], half_width: usize) -> Vec<f64> {
    let n = ssm.len();
    let mut novelty = vec![0.0; n];
    if half_width == 0 || n < 2 * half_width {
        return novelty;
    }
    for i in half_width..n - half_width {
        let mut score = 0.0f64;
        for di in 0..half_width {
            for dj in 0..half_width {
                score += ssm[i - 1 - di][i - 1 - dj]; // before-before
                score += ssm[i + di][i + dj]; // after-after
                score -= ssm[i - 1 - di][i + dj]; // before-after
                score -= ssm[i + di][i - 1 - dj]; // after-before
            }
        }
        novelty[i] = score / (half_width * half_width) as f64;
    }
    novelty
}

/// Cosine similarity of bar `i` to bar `i - lag`, `NAN` for `i < lag`.
pub fn loop_similarity(feats: &[Vec<f64>], lag: usize) -> Vec<f64> {
    feats
        .iter()
        .enumerate()
        .map(|(i, f)| {
            if i >= lag {
                cosine(f, &feats[i - lag])
            } else {
                f64::NAN
            }
        })
        .collect()
}

/// Mahalanobis distance (diagonal covariance) of every row from a model
/// fit to the first `prefix_len` rows. Distance is RMS-normalized by
/// dimensionality so it stays roughly comparable across feature-vector
/// sizes.
pub fn mahalanobis_from_prefix(feats: &[Vec<f64>], prefix_len: usize) -> Vec<f64> {
    let n = feats.len();
    if n == 0 {
        return Vec::new();
    }
    let dim = feats[0].len();
    if dim == 0 {
        return vec![0.0; n];
    }
    let p = prefix_len.clamp(1, n);

    let mut mean = vec![0.0f64; dim];
    for f in &feats[..p] {
        for d in 0..dim {
            mean[d] += f[d];
        }
    }
    for m in &mut mean {
        *m /= p as f64;
    }

    let mut var = vec![0.0f64; dim];
    for f in &feats[..p] {
        for d in 0..dim {
            let diff = f[d] - mean[d];
            var[d] += diff * diff;
        }
    }
    for v in &mut var {
        // Floor variance so a constant prefix dimension does not produce a
        // divide-by-near-zero blowup on the first bar that differs at all.
        *v = (*v / p as f64).max(1e-6);
    }

    feats
        .iter()
        .map(|f| {
            let mut s = 0.0f64;
            for d in 0..dim {
                let diff = f[d] - mean[d];
                s += diff * diff / var[d];
            }
            (s / dim as f64).sqrt()
        })
        .collect()
}

fn chroma_vectors(feats: &[BarFeatures]) -> Vec<Vec<f64>> {
    feats.iter().map(|f| f.chroma.to_vec()).collect()
}

fn band_vectors(feats: &[BarFeatures]) -> Vec<Vec<f64>> {
    feats.iter().map(|f| f.band_energy_db.to_vec()).collect()
}

fn rhythm_vectors(feats: &[BarFeatures]) -> Vec<Vec<f64>> {
    feats
        .iter()
        .map(|f| {
            let mut v = f.rhythm_low.to_vec();
            v.extend_from_slice(&f.rhythm_high);
            v
        })
        .collect()
}

/// The exact 21-dim vector (7-band log energy + 12-bin chroma + tonality +
/// voiced_frac) [`compute`] hands to [`mahalanobis_from_prefix`]. `pub` only
/// so diagnostics (`examples/section_diag.rs`) can report a per-dimension
/// breakdown of the prefix model without duplicating this layout; `compute`
/// and `mahalanobis_from_prefix` themselves are unaffected by this being `pub`.
pub fn combined_vectors(feats: &[BarFeatures]) -> Vec<Vec<f64>> {
    feats
        .iter()
        .map(|f| {
            let mut v = f.band_energy_db.to_vec();
            v.extend_from_slice(&f.chroma);
            v.push(f.tonality);
            v.push(f.voiced_frac);
            v
        })
        .collect()
}

/// Compute all structure signals for one side's bar sequence.
///
/// `prefix_len` bars are used to fit the intro-prefix model (clamped to
/// `feats.len()`; pass [`DEFAULT_PREFIX_BARS`] unless a caller has a reason
/// not to).
pub fn compute(feats: &[BarFeatures], prefix_len: usize) -> StructureSignals {
    let chroma = chroma_vectors(feats);
    let band = band_vectors(feats);
    let rhythm = rhythm_vectors(feats);
    let combined = combined_vectors(feats);

    let chroma_ssm = self_similarity_matrix(&chroma);
    let band_ssm = self_similarity_matrix(&band);
    let rhythm_ssm = self_similarity_matrix(&rhythm);

    StructureSignals {
        chroma_novelty_w8: checkerboard_novelty(&chroma_ssm, NOVELTY_HALF_WIDTH_SHORT),
        chroma_novelty_w16: checkerboard_novelty(&chroma_ssm, NOVELTY_HALF_WIDTH_LONG),
        band_novelty_w8: checkerboard_novelty(&band_ssm, NOVELTY_HALF_WIDTH_SHORT),
        band_novelty_w16: checkerboard_novelty(&band_ssm, NOVELTY_HALF_WIDTH_LONG),
        rhythm_novelty_w8: checkerboard_novelty(&rhythm_ssm, NOVELTY_HALF_WIDTH_SHORT),
        rhythm_novelty_w16: checkerboard_novelty(&rhythm_ssm, NOVELTY_HALF_WIDTH_LONG),
        loop_1: loop_similarity(&rhythm, 1),
        loop_2: loop_similarity(&rhythm, 2),
        loop_4: loop_similarity(&rhythm, 4),
        prefix_mahalanobis: mahalanobis_from_prefix(&combined, prefix_len),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn vec_of(dim: usize, val_at: impl Fn(usize) -> f64) -> Vec<f64> {
        (0..dim).map(val_at).collect()
    }

    #[test]
    fn cosine_identical_vectors_is_one() {
        let a = vec![1.0, 2.0, 3.0];
        assert!((cosine(&a, &a) - 1.0).abs() < 1e-9);
    }

    #[test]
    fn cosine_orthogonal_vectors_is_zero() {
        let a = vec![1.0, 0.0];
        let b = vec![0.0, 1.0];
        assert!(cosine(&a, &b).abs() < 1e-9);
    }

    #[test]
    fn cosine_zero_vector_is_zero_not_nan() {
        let a = vec![0.0, 0.0];
        let b = vec![1.0, 1.0];
        assert_eq!(cosine(&a, &b), 0.0);
    }

    /// 32 bars: first half is one repeated feature vector, second half is a
    /// different repeated feature vector. Novelty must peak at the boundary
    /// (index 16), not just be "large in the second half" or similar.
    #[test]
    fn checkerboard_novelty_peaks_at_content_boundary() {
        let n = 32;
        let boundary = 16;
        let feats: Vec<Vec<f64>> = (0..n)
            .map(|i| {
                if i < boundary {
                    vec_of(8, |d| if d == 0 { 1.0 } else { 0.0 })
                } else {
                    vec_of(8, |d| if d == 4 { 1.0 } else { 0.0 })
                }
            })
            .collect();
        let ssm = self_similarity_matrix(&feats);
        let novelty = checkerboard_novelty(&ssm, 8);
        let (peak_idx, &peak_val) = novelty
            .iter()
            .enumerate()
            .max_by(|a, b| a.1.partial_cmp(b.1).unwrap())
            .unwrap();
        assert!(
            (peak_idx as i64 - boundary as i64).abs() <= 1,
            "novelty peak at {peak_idx}, expected near {boundary}"
        );
        // Comfortably above the flat interior (which should be ~0: no
        // change within either half).
        assert!(peak_val > 1.0, "peak novelty {peak_val} too weak");
        assert!(
            novelty[4].abs() < 1e-9,
            "interior novelty[4]={} should be ~0 (no change nearby)",
            novelty[4]
        );
    }

    /// End-to-end (audio -> [`crate::features::bar_features`] -> chroma SSM
    /// novelty) check that a within-band pitch change is actually caught.
    /// The Stage-2 handoff report noted the `testutil` synth fixture's
    /// "melody" sits at 2500 Hz, above the chroma band's 2000 Hz ceiling, so
    /// nothing in the existing test suite exercised chroma across a real
    /// boundary. This builds its own two-tone buffer (A4=440Hz, both bars,
    /// pitch class 9, then E5=659.25Hz, pitch class 4 — both comfortably
    /// inside 100-2000Hz) without touching `testutil`.
    #[test]
    fn chroma_novelty_catches_a_within_band_pitch_boundary() {
        use crate::decode::AudioBuffer;
        use crate::features::bar_features;
        use std::f32::consts::PI;

        const SR: u32 = 44_100;
        const BARS_EACH: usize = 12;
        const FREQ_A: f32 = 440.0; // A4 -> pitch class 9
        const FREQ_B: f32 = 659.25; // E5 -> pitch class 4

        let bar_frames = SR as usize; // 1s "bars", plenty of STFT frames each
        let total_bars = BARS_EACH * 2;
        let n = bar_frames * total_bars;
        let mut mono = Vec::with_capacity(n);
        for i in 0..n {
            let t = i as f32 / SR as f32;
            let freq = if i < bar_frames * BARS_EACH { FREQ_A } else { FREQ_B };
            mono.push(0.5 * (2.0 * PI * freq * t).sin());
        }
        let mut samples = Vec::with_capacity(mono.len() * 2);
        for &s in &mono {
            samples.push(s);
            samples.push(s);
        }
        let buffer = AudioBuffer {
            sample_rate: SR,
            frames: mono.len() as u64,
            samples,
        };

        let bar_starts: Vec<u64> = (0..total_bars as u64).map(|i| i * bar_frames as u64).collect();
        let feats = bar_features(&buffer.mono(), 0, SR, &bar_starts, bar_frames as u64);

        let dominant_pc = |f: &BarFeatures| {
            f.chroma
                .iter()
                .enumerate()
                .max_by(|a, b| a.1.partial_cmp(b.1).unwrap())
                .unwrap()
                .0
        };
        assert_eq!(dominant_pc(&feats[0]), 9, "first-half bar should read pitch class A (9)");
        assert_eq!(
            dominant_pc(&feats[BARS_EACH]),
            4,
            "second-half bar should read pitch class E (4)"
        );

        let chroma = chroma_vectors(&feats);
        let ssm = self_similarity_matrix(&chroma);
        let novelty = checkerboard_novelty(&ssm, 4);
        let (peak_idx, _) = novelty
            .iter()
            .enumerate()
            .max_by(|a, b| a.1.partial_cmp(b.1).unwrap())
            .unwrap();
        assert!(
            (peak_idx as i64 - BARS_EACH as i64).abs() <= 1,
            "chroma novelty peak at bar {peak_idx}, expected near the true boundary {BARS_EACH}"
        );
    }

    /// Novelty must NOT depend on the sequence getting "bigger" or
    /// "smaller" overall — only on content changing. A boundary where the
    /// second half is simply a scaled-down copy of the same direction
    /// should show near-zero cosine-SSM novelty (same content, different
    /// level), unlike a level-based detector.
    #[test]
    fn checkerboard_novelty_is_level_invariant() {
        let n = 32;
        let feats: Vec<Vec<f64>> = (0..n)
            .map(|i| {
                let scale = if i < 16 { 1.0 } else { 0.1 };
                vec![scale, 0.0, 0.0]
            })
            .collect();
        let ssm = self_similarity_matrix(&feats);
        let novelty = checkerboard_novelty(&ssm, 8);
        let peak = novelty.iter().cloned().fold(0.0f64, f64::max);
        assert!(
            peak < 0.05,
            "expected near-zero novelty for a pure level change, got {peak}"
        );
    }

    #[test]
    fn loop_similarity_high_for_repeating_pattern_low_for_changing() {
        let n = 16;
        // Same 1-bar rhythm pattern every bar (period 1) -> lag-1 similarity ~1.
        let repeating: Vec<Vec<f64>> = (0..n).map(|_| vec![0.3, 1.0]).collect();
        let changing: Vec<Vec<f64>> = (0..n)
            .map(|i| vec![(i as f64 * 0.37).sin(), (i as f64 * 1.91).cos()])
            .collect();

        let loop1_rep = loop_similarity(&repeating, 1);
        let loop1_chg = loop_similarity(&changing, 1);

        let mean = |v: &[f64]| -> f64 {
            let vals: Vec<f64> = v.iter().cloned().filter(|x| !x.is_nan()).collect();
            vals.iter().sum::<f64>() / vals.len() as f64
        };
        assert!(
            mean(&loop1_rep) > 0.99,
            "repeating-pattern loop-1 similarity {} not near 1",
            mean(&loop1_rep)
        );
        assert!(
            mean(&loop1_chg) < mean(&loop1_rep) - 0.3,
            "changing-pattern loop-1 similarity {} not clearly below repeating {}",
            mean(&loop1_chg),
            mean(&loop1_rep)
        );
    }

    #[test]
    fn loop_similarity_has_nan_before_enough_history() {
        let feats = vec![vec![1.0], vec![1.0], vec![1.0]];
        let out = loop_similarity(&feats, 2);
        assert!(out[0].is_nan());
        assert!(out[1].is_nan());
        assert!(!out[2].is_nan());
    }

    #[test]
    fn mahalanobis_rises_after_prefix_like_content_changes() {
        let n = 24;
        let prefix_len = 8;
        let feats: Vec<Vec<f64>> = (0..n)
            .map(|i| {
                if i < 16 {
                    vec![0.0, 0.0, 0.0]
                } else {
                    vec![5.0, 5.0, 5.0]
                }
            })
            .collect();
        let dist = mahalanobis_from_prefix(&feats, prefix_len);
        let early_mean: f64 = dist[..prefix_len].iter().sum::<f64>() / prefix_len as f64;
        let late_mean: f64 = dist[16..].iter().sum::<f64>() / (n - 16) as f64;
        assert!(early_mean < 1e-6, "prefix bars should match their own model, got {early_mean}");
        assert!(
            late_mean > 100.0,
            "post-change bars should score far from the prefix model, got {late_mean}"
        );
    }

    #[test]
    fn compute_runs_end_to_end_on_default_bar_features() {
        let feats = vec![BarFeatures::default(); 20];
        let signals = compute(&feats, DEFAULT_PREFIX_BARS);
        assert_eq!(signals.chroma_novelty_w8.len(), 20);
        assert_eq!(signals.loop_1.len(), 20);
        assert_eq!(signals.prefix_mahalanobis.len(), 20);
    }
}
