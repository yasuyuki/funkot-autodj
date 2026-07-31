//! Stage 2 spectral feature frontend (diagnostic only — not read by [`crate::analysis::analyze`]).
//!
//! `analysis.rs`'s current `BarFeat` (RMS / mid-band energy / HF energy /
//! mid-high ratio / a ZCR-based centroid proxy) is entirely broadband-energy:
//! it can tell "quiet vs loud" and "dull vs bright", but nothing here
//! measures the property that actually defines a Funkot intro/outro —
//! *machine rhythm repeating with no melody/vocal on top*, independent of
//! how loud or bright it is. This module adds that: per-bar chroma
//! (pitch-class content), tonality (spectral peakiness vs noise-likeness —
//! separates "melodic content present" from "kit only"), a voiced-frame
//! rate (stable pitched content vs percussive-only), 7-band log energy, a
//! 16-slot low/high onset rhythm pattern (for loop-similarity), and
//! bar-aggregated onset density (reusing [`crate::analysis::onset_envelope`]).
//!
//! [`crate::structure`] turns a sequence of [`BarFeatures`] into
//! change-point / repetition signals (SSM novelty, loopiness, distance from
//! an intro-prefix model). None of this is consumed by `analyze()` yet —
//! that is Stage 3's job.
//!
//! # Bar grid
//!
//! Callers must pass the *same* `bar_starts` / `bar_len_frames` that
//! [`crate::analysis`]'s section detector scores (see
//! `analysis::section_bar_starts`), so the two feature frontends describe
//! literally the same bars and can be compared side by side (see
//! `examples/section_diag.rs --new-features`).
//!
//! # STFT cost
//!
//! [`bar_features`] runs one STFT pass (Hann, [`N_FFT`]/[`STFT_HOP`]) plus one
//! inverse-FFT-based autocorrelation per frame over the *whole* window
//! (head or tail, ~110 s), not per bar — bars just aggregate frames that
//! fall inside their span. See `AGENTS.md` / the Stage 2 handoff report for
//! measured per-track timing.

use rustfft::num_complex::Complex32;
use rustfft::{Fft, FftPlanner};
use std::sync::Arc;

use crate::analysis::{onset_envelope, HOP as ONSET_HOP};

/// STFT window size (samples).
pub const N_FFT: usize = 2048;
/// STFT hop (samples).
pub const STFT_HOP: usize = 512;

/// Band edges in Hz: `<100 / 100-250 / 250-600 / 600-1.5k / 1.5k-4k / 4k-10k / >10k`.
pub const BAND_EDGES_HZ: [f64; 8] = [
    0.0, 100.0, 250.0, 600.0, 1500.0, 4000.0, 10_000.0, f64::INFINITY,
];
/// Number of log-energy bands ([`BAND_EDGES_HZ`] has one more entry than this).
pub const N_BANDS: usize = 7;
/// Number of chroma (pitch-class) bins.
pub const N_CHROMA: usize = 12;
/// Number of 16th-note rhythm-grid slots per bar.
pub const N_RHYTHM_SLOTS: usize = 16;

/// Chroma is folded from spectral content in this band.
const CHROMA_LO_HZ: f64 = 100.0;
const CHROMA_HI_HZ: f64 = 2000.0;
/// Tonality (spectral flatness) is measured in this band.
const TONAL_LO_HZ: f64 = 200.0;
const TONAL_HI_HZ: f64 = 2000.0;
/// Voiced-frame f0 search range.
const VOICED_LO_HZ: f64 = 150.0;
const VOICED_HI_HZ: f64 = 1000.0;
/// Normalized-autocorrelation peak (in the voiced f0 range) needed to call a
/// frame voiced. Chosen so a pure tone (~0.9+) clears it comfortably and
/// filtered white noise (~0.05-0.15) does not; see `tests` below.
const VOICED_PEAK_THRESHOLD: f64 = 0.35;
/// Frames whose total energy is below this floor (relative-silence, using
/// the same epsilon convention as `analysis.rs`) are never counted voiced.
const SILENCE_EPS: f64 = 1e-12;
/// A frame's tonal-band (200-2000Hz) magnitude spectrum is dominated by
/// numerical noise floor / windowing leakage rather than real spectral
/// shape once its in-band energy drops far below the bar's loudest frame —
/// pooling such a frame's flatness into the bar average would let temporal
/// sparsity (silence between kick/hat hits) masquerade as tonality, which is
/// exactly the confound this floor exists to reject (see
/// `sparse_noise_bursts_score_like_continuous_noise_not_tonal`). 1/1000 of
/// the bar's peak in-band frame energy is well below any real hit's decay
/// tail but above the numerical floor.
const TONAL_FRAME_REL_ENERGY_FLOOR: f64 = 1e-3;
/// Absolute in-band energy floor per frame, in the same units as
/// [`FrameSummary::tonal_energy`] (summed power). Needed in addition to
/// [`TONAL_FRAME_REL_ENERGY_FLOOR`] because a bar that is silent throughout
/// has a peak of ~0 too, so the relative floor alone would divide out to
/// nothing and admit pure noise-floor frames.
const TONAL_FRAME_ABS_ENERGY_FLOOR: f64 = 1e-9;

/// Per-bar Stage 2 feature vector. All fields are diagnostic; `analyze()`
/// never reads this type.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct BarFeatures {
    /// Log (power, dB-like `10*log10`) energy in each of [`BAND_EDGES_HZ`]'s
    /// 7 bands, averaged over the bar's STFT frames.
    pub band_energy_db: [f64; N_BANDS],
    /// 12-bin pitch-class histogram folded from [`CHROMA_LO_HZ`]..[`CHROMA_HI_HZ`],
    /// L1-normalized (sums to 1, or all-zero on a silent/tiny bar).
    pub chroma: [f64; N_CHROMA],
    /// Reciprocal spectral flatness (arithmetic/geometric mean of magnitude)
    /// in [`TONAL_LO_HZ`]..[`TONAL_HI_HZ`], computed *per STFT frame* and
    /// then energy-weight-averaged over the bar's frames (near-silent frames
    /// excluded — see [`TONAL_FRAME_REL_ENERGY_FLOOR`]). This keeps the
    /// value a true measure of "is there a spectral peak when something is
    /// actually sounding", independent of how much silence surrounds it: a
    /// continuous or sparse-but-percussive signal settles near 1 (flat
    /// spectrum), a continuous or sparse *tonal* signal reads clearly higher
    /// (concentrated spectral peaks). This is the main feature meant to
    /// separate "machine drums only" from "melody present" — see
    /// `sparse_noise_bursts_score_like_continuous_noise_not_tonal` and
    /// `sparse_tone_bursts_score_higher_than_sparse_noise_bursts`.
    pub tonality: f64,
    /// Fraction of STFT frames in the bar classified voiced (stable f0 in
    /// [`VOICED_LO_HZ`]..[`VOICED_HI_HZ`] via autocorrelation).
    pub voiced_frac: f64,
    /// 16-slot low-band (<250 Hz, kick-ish) onset strength across the bar,
    /// L1-normalized.
    pub rhythm_low: [f64; N_RHYTHM_SLOTS],
    /// 16-slot high-band (>1500 Hz, hat-ish) onset strength across the bar,
    /// L1-normalized.
    pub rhythm_high: [f64; N_RHYTHM_SLOTS],
    /// Mean [`crate::analysis::onset_envelope`] novelty over the bar
    /// (reused, not recomputed — same kick-oriented novelty the existing
    /// tempo/downbeat code already relies on).
    pub onset_density: f64,
}

impl Default for BarFeatures {
    fn default() -> Self {
        BarFeatures {
            band_energy_db: [-120.0; N_BANDS],
            chroma: [0.0; N_CHROMA],
            tonality: 0.0,
            voiced_frac: 0.0,
            rhythm_low: [0.0; N_RHYTHM_SLOTS],
            rhythm_high: [0.0; N_RHYTHM_SLOTS],
            onset_density: 0.0,
        }
    }
}

fn band_index(freq_hz: f64) -> usize {
    for (i, w) in BAND_EDGES_HZ.windows(2).enumerate() {
        if freq_hz >= w[0] && freq_hz < w[1] {
            return i;
        }
    }
    N_BANDS - 1
}

/// MIDI-style pitch class of `freq_hz` (A4=440Hz -> 9), independent of octave.
fn pitch_class(freq_hz: f64) -> usize {
    if freq_hz <= 0.0 || !freq_hz.is_finite() {
        return 0;
    }
    let pc = (12.0 * (freq_hz / 440.0).log2() + 69.0).round();
    (((pc as i64) % 12 + 12) % 12) as usize
}

fn hann_window(n: usize) -> Vec<f32> {
    if n <= 1 {
        return vec![1.0; n];
    }
    (0..n)
        .map(|i| {
            let x = std::f64::consts::PI * 2.0 * i as f64 / (n as f64 - 1.0);
            (0.5 - 0.5 * x.cos()) as f32
        })
        .collect()
}

/// Compact per-STFT-frame summary; a whole window (~110 s) worth of these
/// (~9.5k at 44.1kHz) is a few MB, unlike storing full complex spectra.
struct FrameSummary {
    /// Frame center, in samples from the start of the analyzed window.
    center: i64,
    band_power: [f64; N_BANDS],
    chroma_mag: [f64; N_CHROMA],
    /// This frame's own reciprocal spectral flatness (arith/geo of
    /// magnitude) over the tonal band — *not* pooled with other frames yet;
    /// bar aggregation energy-weight-averages these, excluding near-silent
    /// frames (see [`TONAL_FRAME_REL_ENERGY_FLOOR`]).
    tonal_flatness_recip: f64,
    /// This frame's summed power over the tonal band (200-2000Hz); used both
    /// as the aggregation weight and to decide whether the frame is loud
    /// enough in-band to trust its flatness at all.
    tonal_energy: f64,
    /// Max normalized autocorrelation in the voiced f0 range. `0.0` on a
    /// near-silent frame (`r0` too small to trust any lag ratio) — such
    /// frames are still counted in `voiced_frac`'s denominator (silence
    /// genuinely contains no detectable pitch, unlike the tonality case
    /// above; see the Stage-2 handoff report for the reasoning).
    periodicity: f64,
    low_energy: f64,
    high_energy: f64,
}

fn frame_summaries(window_mono: &[f32], sample_rate: u32) -> Vec<FrameSummary> {
    if window_mono.len() < N_FFT || sample_rate == 0 {
        return Vec::new();
    }
    let sr = f64::from(sample_rate);
    let window = hann_window(N_FFT);

    let mut planner = FftPlanner::<f32>::new();
    let fft_fwd: Arc<dyn Fft<f32>> = planner.plan_fft_forward(N_FFT);
    let fft_inv: Arc<dyn Fft<f32>> = planner.plan_fft_inverse(N_FFT);

    let voiced_lag_lo = (sr / VOICED_HI_HZ).floor().max(1.0) as usize;
    let voiced_lag_hi = ((sr / VOICED_LO_HZ).ceil() as usize).min(N_FFT - 1);

    let n_frames = (window_mono.len() - N_FFT) / STFT_HOP + 1;
    let mut out = Vec::with_capacity(n_frames);
    let mut buf = vec![Complex32::new(0.0, 0.0); N_FFT];
    let mut autocorr = vec![Complex32::new(0.0, 0.0); N_FFT];
    let mut scratch_fwd = vec![Complex32::new(0.0, 0.0); fft_fwd.get_inplace_scratch_len()];
    let mut scratch_inv = vec![Complex32::new(0.0, 0.0); fft_inv.get_inplace_scratch_len()];

    for i in 0..n_frames {
        let start = i * STFT_HOP;
        for k in 0..N_FFT {
            buf[k] = Complex32::new(window_mono[start + k] * window[k], 0.0);
        }
        fft_fwd.process_with_scratch(&mut buf, &mut scratch_fwd);

        let mut band_power = [0.0f64; N_BANDS];
        let mut chroma_mag = [0.0f64; N_CHROMA];
        // Per-*this-frame* flatness inputs; folded into a single
        // arith/geo ratio below, before this frame is ever mixed with any
        // other frame's data (that ordering is the fix — see
        // `TONAL_FRAME_REL_ENERGY_FLOOR`'s doc comment for what pooling
        // raw sums across frames first used to get wrong).
        let mut tonal_sum_log_mag = 0.0f64;
        let mut tonal_sum_mag = 0.0f64;
        let mut tonal_energy = 0.0f64;
        let mut tonal_count = 0usize;

        for k in 0..=N_FFT / 2 {
            let freq = k as f64 * sr / N_FFT as f64;
            let power = f64::from(buf[k].norm_sqr());
            band_power[band_index(freq)] += power;
            if (CHROMA_LO_HZ..CHROMA_HI_HZ).contains(&freq) {
                let mag = power.sqrt();
                chroma_mag[pitch_class(freq)] += mag;
            }
            if (TONAL_LO_HZ..TONAL_HI_HZ).contains(&freq) {
                let mag = power.sqrt();
                tonal_sum_log_mag += (mag + 1e-9).ln();
                tonal_sum_mag += mag;
                tonal_energy += power;
                tonal_count += 1;
            }
        }
        let tonal_flatness_recip = if tonal_count > 0 {
            let geo = (tonal_sum_log_mag / tonal_count as f64).exp();
            let arith = tonal_sum_mag / tonal_count as f64;
            (arith / geo.max(1e-12)).min(1e6)
        } else {
            0.0
        };
        let low_energy = band_power[0] + band_power[1];
        let high_energy = band_power[4] + band_power[5] + band_power[6];

        // Autocorrelation via Wiener-Khinchin: IFFT(|FFT(x)|^2). Reuse the
        // full (not just half) complex spectrum already computed above.
        for k in 0..N_FFT {
            autocorr[k] = Complex32::new(buf[k].norm_sqr(), 0.0);
        }
        fft_inv.process_with_scratch(&mut autocorr, &mut scratch_inv);
        let r0 = autocorr[0].re as f64;
        let mut periodicity = 0.0f64;
        if r0 > SILENCE_EPS {
            let mut best = 0.0f64;
            for lag in voiced_lag_lo..=voiced_lag_hi {
                let r = f64::from(autocorr[lag].re) / r0;
                if r > best {
                    best = r;
                }
            }
            periodicity = best;
        }

        out.push(FrameSummary {
            center: (start + N_FFT / 2) as i64,
            band_power,
            chroma_mag,
            tonal_flatness_recip,
            tonal_energy,
            periodicity,
            low_energy,
            high_energy,
        });
    }
    out
}

/// Compute per-bar [`BarFeatures`] over `bar_starts` (absolute frame
/// offsets, same coordinate system as `window_offset + i` for `i` in
/// `0..window_mono.len()`), using one STFT pass over `window_mono` and one
/// reused [`crate::analysis::onset_envelope`] pass.
///
/// `bar_len_frames` and `bar_starts` should come from the same bar grid
/// [`crate::analysis::detect_section_bars`] scores (see
/// `analysis::section_bar_starts`), so bar `i` here describes exactly the
/// same audio as the legacy `BarFeat` at index `i`.
///
/// Bars with no STFT frames inside them (window too short, or the bar falls
/// outside `window_mono`) get [`BarFeatures::default`].
pub fn bar_features(
    window_mono: &[f32],
    window_offset: u64,
    sample_rate: u32,
    bar_starts: &[u64],
    bar_len_frames: u64,
) -> Vec<BarFeatures> {
    let frames = frame_summaries(window_mono, sample_rate);
    let onset = onset_envelope(window_mono, sample_rate, ONSET_HOP).ok();

    bar_starts
        .iter()
        .map(|&start| {
            let Some(local_start) = start.checked_sub(window_offset) else {
                return BarFeatures::default();
            };
            let local_start = local_start as i64;
            let local_end = local_start + bar_len_frames as i64;
            aggregate_bar(&frames, local_start, local_end, onset.as_ref())
        })
        .collect()
}

fn aggregate_bar(
    frames: &[FrameSummary],
    local_start: i64,
    local_end: i64,
    onset: Option<&crate::analysis::OnsetData>,
) -> BarFeatures {
    let mut band_power_sum = [0.0f64; N_BANDS];
    let mut chroma_sum = [0.0f64; N_CHROMA];
    // (energy, per-frame flatness-reciprocal) for every in-bar frame; the
    // energy floor that decides which of these actually count needs the
    // bar's own peak, so it can only be applied after this loop collects
    // them all (see the weighted-average block below).
    let mut tonal_frame_pairs: Vec<(f64, f64)> = Vec::new();
    let mut voiced_count = 0usize;
    let mut n = 0usize;

    // Rhythm grid uses positive-diff novelty of low/high band energy
    // between *consecutive* STFT frames (bar-local, so the first in-bar
    // frame's novelty is measured against the frame just before it).
    let mut rhythm_low = [0.0f64; N_RHYTHM_SLOTS];
    let mut rhythm_high = [0.0f64; N_RHYTHM_SLOTS];
    let bar_len = (local_end - local_start).max(1) as f64;

    for (i, f) in frames.iter().enumerate() {
        if f.center < local_start || f.center >= local_end {
            continue;
        }
        n += 1;
        for b in 0..N_BANDS {
            band_power_sum[b] += f.band_power[b];
        }
        for c in 0..N_CHROMA {
            chroma_sum[c] += f.chroma_mag[c];
        }
        tonal_frame_pairs.push((f.tonal_energy, f.tonal_flatness_recip));
        if f.periodicity >= VOICED_PEAK_THRESHOLD {
            voiced_count += 1;
        }

        if i > 0 {
            let prev = &frames[i - 1];
            let low_novelty = (f.low_energy - prev.low_energy).max(0.0);
            let high_novelty = (f.high_energy - prev.high_energy).max(0.0);
            let rel = (f.center - local_start) as f64 / bar_len;
            let slot = ((rel * N_RHYTHM_SLOTS as f64).floor() as usize).min(N_RHYTHM_SLOTS - 1);
            rhythm_low[slot] += low_novelty;
            rhythm_high[slot] += high_novelty;
        }
    }

    if n == 0 {
        return BarFeatures::default();
    }

    let mut band_energy_db = [-120.0f64; N_BANDS];
    for b in 0..N_BANDS {
        let mean_power = band_power_sum[b] / n as f64;
        band_energy_db[b] = 10.0 * (mean_power.max(1e-15)).log10();
    }

    let chroma_total: f64 = chroma_sum.iter().sum();
    let mut chroma = [0.0f64; N_CHROMA];
    if chroma_total > 1e-12 {
        for c in 0..N_CHROMA {
            chroma[c] = chroma_sum[c] / chroma_total;
        }
    }

    // Energy-weighted average of each frame's *own* flatness-reciprocal,
    // dropping frames whose in-band energy is too far below this bar's
    // loudest in-band frame to trust (or below an absolute silence floor
    // for bars that are quiet throughout). This is what stops a bar's
    // silence-between-hits from pulling tonality toward "tonal" — each
    // included frame's flatness was already computed from its own spectrum
    // only, so a near-silent frame's noise-floor leakage can no longer drag
    // the geometric mean of some *other* frame's real content down with it.
    let tonality = {
        let peak_energy = tonal_frame_pairs
            .iter()
            .map(|&(e, _)| e)
            .fold(0.0f64, f64::max);
        let floor = (peak_energy * TONAL_FRAME_REL_ENERGY_FLOOR).max(TONAL_FRAME_ABS_ENERGY_FLOOR);
        let mut weighted_sum = 0.0f64;
        let mut weight_total = 0.0f64;
        for &(energy, flatness_recip) in &tonal_frame_pairs {
            if energy >= floor {
                weighted_sum += energy * flatness_recip;
                weight_total += energy;
            }
        }
        if weight_total > 0.0 {
            weighted_sum / weight_total
        } else {
            0.0
        }
    };

    let voiced_frac = voiced_count as f64 / n as f64;

    l1_normalize(&mut rhythm_low);
    l1_normalize(&mut rhythm_high);

    let onset_density = onset
        .map(|o| mean_onset_in_bar(o, local_start, local_end))
        .unwrap_or(0.0);

    BarFeatures {
        band_energy_db,
        chroma,
        tonality,
        voiced_frac,
        rhythm_low,
        rhythm_high,
        onset_density,
    }
}

fn l1_normalize(v: &mut [f64; N_RHYTHM_SLOTS]) {
    let total: f64 = v.iter().sum();
    if total > 1e-12 {
        for x in v.iter_mut() {
            *x /= total;
        }
    }
}

fn mean_onset_in_bar(onset: &crate::analysis::OnsetData, local_start: i64, local_end: i64) -> f64 {
    let hop = ONSET_HOP as i64;
    let mut sum = 0.0f64;
    let mut count = 0usize;
    for (i, &v) in onset.novelty.iter().enumerate() {
        let pos = i as i64 * hop;
        if pos >= local_start && pos < local_end {
            sum += v;
            count += 1;
        }
    }
    if count > 0 {
        sum / count as f64
    } else {
        0.0
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::decode::AudioBuffer;
    use std::f32::consts::PI;

    const SR: u32 = 44_100;

    fn sine_buffer(freq: f32, secs: f64, amp: f32) -> AudioBuffer {
        let n = (secs * f64::from(SR)) as usize;
        let mut mono = Vec::with_capacity(n);
        for i in 0..n {
            let t = i as f32 / SR as f32;
            mono.push(amp * (2.0 * PI * freq * t).sin());
        }
        stereo_from_mono(mono)
    }

    fn white_noise_buffer(secs: f64, amp: f32) -> AudioBuffer {
        let n = (secs * f64::from(SR)) as usize;
        let mut state = 0xC0FFEEu32;
        let mut mono = Vec::with_capacity(n);
        for _ in 0..n {
            state ^= state << 13;
            state ^= state >> 17;
            state ^= state << 5;
            let u = (state as f32) / (u32::MAX as f32);
            mono.push(amp * (u * 2.0 - 1.0));
        }
        stereo_from_mono(mono)
    }

    /// Like [`sine_buffer`], but only sounding for `burst_secs` out of every
    /// `period_secs` (silence otherwise) — models a kick/hat hit with gaps,
    /// not a sustained tone.
    fn sparse_tone_buffer(freq: f32, secs: f64, amp: f32, burst_secs: f64, period_secs: f64) -> AudioBuffer {
        let n = (secs * f64::from(SR)) as usize;
        let mut mono = Vec::with_capacity(n);
        for i in 0..n {
            let t = i as f64 / f64::from(SR);
            let phase = t % period_secs;
            let s = if phase < burst_secs {
                amp * (2.0 * PI * freq * t as f32).sin()
            } else {
                0.0
            };
            mono.push(s);
        }
        stereo_from_mono(mono)
    }

    /// Like [`white_noise_buffer`], but only sounding for `burst_secs` out of
    /// every `period_secs` — models a noise-based hat/kick hit with gaps,
    /// e.g. a Funkot intro's machine rhythm (short transients, mostly silence
    /// in between), as opposed to sustained broadband noise.
    fn sparse_noise_buffer(secs: f64, amp: f32, burst_secs: f64, period_secs: f64) -> AudioBuffer {
        let n = (secs * f64::from(SR)) as usize;
        let mut state = 0xC0FFEEu32;
        let mut mono = Vec::with_capacity(n);
        for i in 0..n {
            let t = i as f64 / f64::from(SR);
            let phase = t % period_secs;
            state ^= state << 13;
            state ^= state >> 17;
            state ^= state << 5;
            let u = (state as f32) / (u32::MAX as f32);
            let s = if phase < burst_secs { amp * (u * 2.0 - 1.0) } else { 0.0 };
            mono.push(s);
        }
        stereo_from_mono(mono)
    }

    fn stereo_from_mono(mono: Vec<f32>) -> AudioBuffer {
        let mut samples = Vec::with_capacity(mono.len() * 2);
        for s in &mono {
            samples.push(*s);
            samples.push(*s);
        }
        AudioBuffer {
            sample_rate: SR,
            frames: mono.len() as u64,
            samples,
        }
    }

    /// One "bar" spanning the whole buffer, for single-shot feature tests.
    fn single_bar_features(buffer: &AudioBuffer) -> BarFeatures {
        let mono = buffer.mono();
        let starts = [0u64];
        let feats = bar_features(&mono, 0, buffer.sample_rate, &starts, buffer.frames);
        feats.into_iter().next().unwrap()
    }

    #[test]
    fn pure_tone_is_more_tonal_than_white_noise() {
        let tone = sine_buffer(440.0, 1.0, 0.5);
        let noise = white_noise_buffer(1.0, 0.5);
        let tone_feat = single_bar_features(&tone);
        let noise_feat = single_bar_features(&noise);
        assert!(
            tone_feat.tonality > noise_feat.tonality * 5.0,
            "tone tonality {} not clearly above noise tonality {}",
            tone_feat.tonality,
            noise_feat.tonality
        );
    }

    /// Regression for the Stage-2-review confound: pooling raw log-magnitude
    /// across *all* frames in a bar (including near-silent gap frames)
    /// before taking geometric/arithmetic means made temporal sparsity read
    /// as "tonal", even for pure noise bursts. A Funkot intro (kick/hat with
    /// gaps) must not score anywhere near as tonal as a real melodic/vocal
    /// section just because it has silence between hits — it should score
    /// about like *continuous* noise (flat spectrum during the hits, no
    /// signal to average during the gaps).
    #[test]
    fn sparse_noise_bursts_score_like_continuous_noise_not_tonal() {
        let continuous = white_noise_buffer(2.0, 0.5);
        let sparse = sparse_noise_buffer(2.0, 0.5, 0.02, 0.2); // 10% duty cycle
        let cont_feat = single_bar_features(&continuous);
        let sparse_feat = single_bar_features(&sparse);
        assert!(
            sparse_feat.tonality < 5.0,
            "sparse noise bursts scored tonality={}, expected near white noise's baseline (~1)",
            sparse_feat.tonality
        );
        assert!(
            sparse_feat.tonality < cont_feat.tonality * 3.0 + 3.0,
            "sparse noise tonality {} should be close to continuous noise tonality {}, not inflated by silence gaps",
            sparse_feat.tonality,
            cont_feat.tonality
        );
    }

    /// Companion to the sparse-noise regression above: once sparsity itself
    /// is no longer a confound, a genuinely tonal sparse signal (e.g. a
    /// pitched stab) must still score clearly higher than a sparse noise
    /// burst with the same duty cycle — i.e. tonality separates by timbre,
    /// not by how much silence surrounds the hits.
    #[test]
    fn sparse_tone_bursts_score_higher_than_sparse_noise_bursts() {
        let sparse_tone = sparse_tone_buffer(440.0, 2.0, 0.5, 0.05, 0.25);
        let sparse_noise = sparse_noise_buffer(2.0, 0.5, 0.05, 0.25);
        let tone_feat = single_bar_features(&sparse_tone);
        let noise_feat = single_bar_features(&sparse_noise);
        assert!(
            tone_feat.tonality > noise_feat.tonality * 5.0,
            "sparse tone tonality {} not clearly above sparse noise tonality {}",
            tone_feat.tonality,
            noise_feat.tonality
        );
    }

    #[test]
    fn chroma_peaks_at_known_pitch_class() {
        // A4 = 440 Hz -> pitch class 9 (A).
        let tone = sine_buffer(440.0, 1.0, 0.5);
        let feat = single_bar_features(&tone);
        let (max_idx, &max_val) = feat
            .chroma
            .iter()
            .enumerate()
            .max_by(|a, b| a.1.partial_cmp(b.1).unwrap())
            .unwrap();
        assert_eq!(max_idx, 9, "chroma peak at bin {max_idx}, expected A (9)");
        assert!(max_val > 0.5, "chroma[9]={max_val} not dominant");
    }

    #[test]
    fn chroma_peaks_at_different_known_pitch_class() {
        // C5 ~= 523.25 Hz -> pitch class 0 (C).
        let tone = sine_buffer(523.25, 1.0, 0.5);
        let feat = single_bar_features(&tone);
        let (max_idx, _) = feat
            .chroma
            .iter()
            .enumerate()
            .max_by(|a, b| a.1.partial_cmp(b.1).unwrap())
            .unwrap();
        assert_eq!(max_idx, 0, "chroma peak at bin {max_idx}, expected C (0)");
    }

    #[test]
    fn band_energy_concentrates_in_expected_band() {
        // 5 kHz -> band index 5 (4k-10k Hz).
        let tone = sine_buffer(5000.0, 1.0, 0.5);
        let feat = single_bar_features(&tone);
        let (max_idx, _) = feat
            .band_energy_db
            .iter()
            .enumerate()
            .max_by(|a, b| a.1.partial_cmp(b.1).unwrap())
            .unwrap();
        assert_eq!(max_idx, 5, "loudest band {max_idx}, expected index 5 (4k-10k Hz)");

        // 60 Hz -> band index 0 (<100 Hz).
        let bass = sine_buffer(60.0, 1.0, 0.5);
        let bass_feat = single_bar_features(&bass);
        let (bass_max_idx, _) = bass_feat
            .band_energy_db
            .iter()
            .enumerate()
            .max_by(|a, b| a.1.partial_cmp(b.1).unwrap())
            .unwrap();
        assert_eq!(bass_max_idx, 0, "loudest band {bass_max_idx}, expected index 0 (<100 Hz)");
    }

    #[test]
    fn voiced_frac_high_for_tone_low_for_noise() {
        // 300 Hz is inside the voiced search range (150-1000 Hz).
        let tone = sine_buffer(300.0, 1.0, 0.5);
        let noise = white_noise_buffer(1.0, 0.5);
        let tone_feat = single_bar_features(&tone);
        let noise_feat = single_bar_features(&noise);
        assert!(
            tone_feat.voiced_frac > 0.8,
            "tone voiced_frac {} expected > 0.8",
            tone_feat.voiced_frac
        );
        assert!(
            noise_feat.voiced_frac < 0.2,
            "noise voiced_frac {} expected < 0.2",
            noise_feat.voiced_frac
        );
    }

    #[test]
    fn silent_buffer_yields_default_features_without_panicking() {
        let silence = stereo_from_mono(vec![0.0f32; 3 * N_FFT]);
        let feat = single_bar_features(&silence);
        assert_eq!(feat.voiced_frac, 0.0);
        assert_eq!(feat.tonality, 0.0);
        assert_eq!(feat.chroma, [0.0; N_CHROMA]);
    }

    #[test]
    fn too_short_window_returns_defaults_not_panic() {
        let short = stereo_from_mono(vec![0.1f32; N_FFT / 2]);
        let starts = [0u64];
        let feats = bar_features(&short.mono(), 0, short.sample_rate, &starts, short.frames);
        assert_eq!(feats.len(), 1);
        assert_eq!(feats[0].tonality, 0.0);
    }

    /// Answers the Stage-2-review question of whether `voiced_frac` has the
    /// same "silence between hits gets pooled in and drags the average"
    /// confound `tonality` had. It does not: unlike tonality (which averaged
    /// a *spectral shape* across frames, letting silent frames corrupt the
    /// shape estimate itself), `voiced_frac` is a fraction of *time*, and a
    /// silent frame genuinely contains no detectable pitch — counting it in
    /// the denominator is the correct semantics, not a bug. This asserts
    /// that on a 20%-duty sparse tone the fraction tracks the true duty
    /// cycle (not near 0, not near 1, roughly proportional to it, wider
    /// bounds account for edge frames straddling burst boundaries), while
    /// both noise cases (which never have detectable periodicity) stay ~0
    /// regardless of sparsity.
    #[test]
    fn voiced_frac_tracks_duty_cycle_not_silence_pooled_into_noise() {
        let cont_tone = sine_buffer(300.0, 2.0, 0.5);
        let sparse_tone = sparse_tone_buffer(300.0, 2.0, 0.5, 0.05, 0.25); // 20% duty
        let cont_noise = white_noise_buffer(2.0, 0.5);
        let sparse_noise = sparse_noise_buffer(2.0, 0.5, 0.05, 0.25);

        let cont_tone_vf = single_bar_features(&cont_tone).voiced_frac;
        let sparse_tone_vf = single_bar_features(&sparse_tone).voiced_frac;
        let cont_noise_vf = single_bar_features(&cont_noise).voiced_frac;
        let sparse_noise_vf = single_bar_features(&sparse_noise).voiced_frac;

        assert!(cont_tone_vf > 0.9, "continuous tone voiced_frac {cont_tone_vf} expected > 0.9");
        assert!(
            (0.15..0.5).contains(&sparse_tone_vf),
            "sparse (20% duty) tone voiced_frac {sparse_tone_vf} expected roughly near the duty cycle, not near 0 or 1"
        );
        assert!(cont_noise_vf < 0.05, "continuous noise voiced_frac {cont_noise_vf} expected ~0");
        assert!(
            sparse_noise_vf < 0.05,
            "sparse noise voiced_frac {sparse_noise_vf} expected ~0, not inflated by sparsity"
        );
    }
}
