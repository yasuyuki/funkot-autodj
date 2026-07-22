//! Offline whole-buffer time-stretch and resample helpers for the loader.

use rubato::audioadapter_buffers::direct::InterleavedSlice;
use rubato::{
    Async, Fft, FixedAsync, FixedSync, Resampler, SincInterpolationParameters,
    SincInterpolationType, WindowFunction,
};

use crate::{Error, PitchMode, Result};

/// Scale factor mapping analysis frame indices from the decoded (input) domain
/// into the stretched/resampled (output) domain: `out_len / in_len`.
#[inline]
pub fn position_scale(in_len_frames: u64, out_len_frames: u64) -> f64 {
    if in_len_frames == 0 {
        return 1.0;
    }
    out_len_frames as f64 / in_len_frames as f64
}

/// Signalsmith Stretch analysis/synthesis window lengths in milliseconds.
///
/// Used by diagnostic tooling ([`render_track_with_config`]). Production
/// playback via [`render_track`] always uses Signalsmith's official
/// [`StretchConfig::official_default`] preset (`preset_default`: 120 ms block /
/// 30 ms interval). Alternate configs here are for A/B comparison only — they
/// are **not** claimed to be higher quality.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct StretchConfig {
    /// STFT block length in milliseconds.
    pub block_ms: f64,
    /// Hop / interval length in milliseconds.
    pub interval_ms: f64,
}

impl StretchConfig {
    /// Official Signalsmith high-quality preset (`presetDefault`): 120 ms / 30 ms.
    pub const fn official_default() -> Self {
        Self {
            block_ms: 120.0,
            interval_ms: 30.0,
        }
    }

    /// Shorter windows that emphasize transient localization (diagnostic).
    pub const fn transient_focused() -> Self {
        Self {
            block_ms: 80.0,
            interval_ms: 20.0,
        }
    }

    /// Longer windows (diagnostic comparison only; not labeled as superior).
    pub const fn long_window() -> Self {
        Self {
            block_ms: 180.0,
            interval_ms: 45.0,
        }
    }

    /// Validate finite, positive durations with `block_ms > interval_ms`.
    pub fn validate(&self) -> Result<()> {
        if !(self.block_ms.is_finite() && self.block_ms > 0.0) {
            return Err(Error::Stretch(format!(
                "invalid block_ms: {}",
                self.block_ms
            )));
        }
        if !(self.interval_ms.is_finite() && self.interval_ms > 0.0) {
            return Err(Error::Stretch(format!(
                "invalid interval_ms: {}",
                self.interval_ms
            )));
        }
        if self.block_ms <= self.interval_ms {
            return Err(Error::Stretch(format!(
                "block_ms ({}) must be greater than interval_ms ({})",
                self.block_ms, self.interval_ms
            )));
        }
        Ok(())
    }

    /// Convert millisecond config to integer sample counts at `sample_rate`.
    ///
    /// Matches Signalsmith's C++ cast of `sampleRate * (ms/1000)` to `int`
    /// (truncate toward zero).
    pub fn to_samples(&self, sample_rate: u32) -> Result<(usize, usize)> {
        self.validate()?;
        if sample_rate == 0 {
            return Err(Error::Stretch("sample rate must be > 0".into()));
        }
        let sr = sample_rate as f32;
        let block = (sr * (self.block_ms as f32) / 1000.0) as usize;
        let interval = (sr * (self.interval_ms as f32) / 1000.0) as usize;
        if block == 0 || interval == 0 {
            return Err(Error::Stretch(format!(
                "block/interval rounds to zero at {sample_rate} Hz \
                 (block_ms={}, interval_ms={})",
                self.block_ms, self.interval_ms
            )));
        }
        if block <= interval {
            return Err(Error::Stretch(format!(
                "block_samples ({block}) must be greater than interval_samples ({interval})"
            )));
        }
        Ok((block, interval))
    }
}

/// Stretch/resample a whole decoded track to `out_rate`, changing its tempo by
/// `speed` (>1 = faster). [`PitchMode::Preserve`] keeps pitch (signalsmith-stretch);
/// [`PitchMode::Shift`] is a plain resample (pitch rises with speed).
///
/// Preserve mode uses Signalsmith's official `preset_default` (120 ms / 30 ms).
///
/// Returns interleaved stereo. Callers map analysis positions with
/// [`position_scale`] using `samples.len()/2` as the input frame count and
/// `output.len()/2` as the output frame count.
pub fn render_track(
    samples_interleaved_stereo: &[f32],
    in_rate: u32,
    out_rate: u32,
    speed: f64,
    mode: PitchMode,
) -> Result<Vec<f32>> {
    render_track_with_config(
        samples_interleaved_stereo,
        in_rate,
        out_rate,
        speed,
        mode,
        None,
    )
}

/// Like [`render_track`], but allows a custom [`StretchConfig`] for Preserve mode.
///
/// - `stretch_config: None` → Signalsmith `preset_default` (production path).
/// - `Some(StretchConfig::official_default())` → same as `None` (`preset_default`).
/// - Other configs use `Stretch::new` with the requested window lengths (diagnostic).
pub fn render_track_with_config(
    samples_interleaved_stereo: &[f32],
    in_rate: u32,
    out_rate: u32,
    speed: f64,
    mode: PitchMode,
    stretch_config: Option<StretchConfig>,
) -> Result<Vec<f32>> {
    if !samples_interleaved_stereo.len().is_multiple_of(2) {
        return Err(Error::Stretch(
            "input buffer length is not an even stereo frame count".into(),
        ));
    }
    if in_rate == 0 || out_rate == 0 {
        return Err(Error::Stretch("sample rate must be > 0".into()));
    }
    if !(speed.is_finite() && speed > 0.0) {
        return Err(Error::Stretch(format!("invalid speed: {speed}")));
    }
    if let Some(cfg) = stretch_config {
        cfg.validate()?;
    }

    match mode {
        PitchMode::Preserve => render_preserve(
            samples_interleaved_stereo,
            in_rate,
            out_rate,
            speed,
            stretch_config,
        ),
        PitchMode::Shift => render_shift(samples_interleaved_stereo, in_rate, out_rate, speed),
    }
}

fn render_preserve(
    samples: &[f32],
    in_rate: u32,
    out_rate: u32,
    speed: f64,
    stretch_config: Option<StretchConfig>,
) -> Result<Vec<f32>> {
    // Tempo change at the input rate (pitch preserved), then rate convert.
    let stretched = if (speed - 1.0).abs() < 1e-12 {
        samples.to_vec()
    } else {
        time_stretch(samples, in_rate, speed, stretch_config)?
    };

    if in_rate == out_rate {
        Ok(stretched)
    } else {
        resample_fixed_rates(&stretched, in_rate, out_rate)
    }
}

fn render_shift(samples: &[f32], in_rate: u32, out_rate: u32, speed: f64) -> Result<Vec<f32>> {
    // Combined ratio: out_rate / (in_rate * speed).
    let ratio = f64::from(out_rate) / (f64::from(in_rate) * speed);
    resample_ratio(samples, ratio)
}

/// Pitch-preserving tempo change via signalsmith-stretch `exact`.
/// Output length in frames is `round(input_frames / speed)`.
fn time_stretch(
    samples: &[f32],
    in_rate: u32,
    speed: f64,
    stretch_config: Option<StretchConfig>,
) -> Result<Vec<f32>> {
    let in_frames = samples.len() / 2;
    let out_frames = ((in_frames as f64) / speed).round().max(1.0) as usize;
    let mut output = vec![0.0f32; out_frames * 2];

    let mut stretch = match stretch_config {
        None => signalsmith_stretch::Stretch::preset_default(2, in_rate),
        Some(cfg) if cfg == StretchConfig::official_default() => {
            signalsmith_stretch::Stretch::preset_default(2, in_rate)
        }
        Some(cfg) => {
            let (block, interval) = cfg.to_samples(in_rate)?;
            signalsmith_stretch::Stretch::new(2, block, interval)
        }
    };

    // Transpose factor 1.0 (default): tempo changes, pitch stays.
    let ok = stretch.exact(samples, &mut output);
    if !ok {
        return Err(Error::Stretch(
            "signalsmith-stretch exact() reported failure".into(),
        ));
    }
    if !output.iter().all(|s| s.is_finite()) {
        return Err(Error::Stretch(
            "signalsmith-stretch produced non-finite samples".into(),
        ));
    }
    Ok(output)
}

/// High-quality fixed-ratio resample between integer sample rates (rubato FFT).
fn resample_fixed_rates(samples: &[f32], in_rate: u32, out_rate: u32) -> Result<Vec<f32>> {
    let in_frames = samples.len() / 2;
    let mut resampler = Fft::<f32>::new(
        in_rate as usize,
        out_rate as usize,
        1024,
        2,
        FixedSync::Input,
    )
    .map_err(|e| Error::Stretch(format!("rubato Fft::new: {e}")))?;

    let input = InterleavedSlice::new(samples, 2, in_frames)
        .map_err(|e| Error::Stretch(format!("interleaved input: {e}")))?;
    let output = resampler
        .process_all(&input, in_frames, None)
        .map_err(|e| Error::Stretch(format!("rubato process_all: {e}")))?;
    Ok(output.take_data())
}

/// Arbitrary-ratio resample (used for [`PitchMode::Shift`] combined speed+rate).
fn resample_ratio(samples: &[f32], ratio: f64) -> Result<Vec<f32>> {
    if !(ratio.is_finite() && ratio > 0.0) {
        return Err(Error::Stretch(format!("invalid resample ratio: {ratio}")));
    }
    if (ratio - 1.0).abs() < 1e-12 {
        return Ok(samples.to_vec());
    }

    let in_frames = samples.len() / 2;
    let params = SincInterpolationParameters::new(128, WindowFunction::BlackmanHarris2)
        .oversampling_factor(256)
        .interpolation(SincInterpolationType::Cubic);

    let mut resampler = Async::<f32>::new_sinc(ratio, 1.1, &params, 1024, 2, FixedAsync::Input)
        .map_err(|e| Error::Stretch(format!("rubato Async::new_sinc: {e}")))?;

    let input = InterleavedSlice::new(samples, 2, in_frames)
        .map_err(|e| Error::Stretch(format!("interleaved input: {e}")))?;
    let output = resampler
        .process_all(&input, in_frames, None)
        .map_err(|e| Error::Stretch(format!("rubato process_all: {e}")))?;
    Ok(output.take_data())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn stereo_sine(frames: usize, freq: f32, sr: u32) -> Vec<f32> {
        let mut samples = vec![0.0f32; frames * 2];
        for i in 0..frames {
            let t = i as f32 / sr as f32;
            let s = (2.0 * std::f32::consts::PI * freq * t).sin() * 0.5;
            samples[i * 2] = s;
            samples[i * 2 + 1] = s;
        }
        samples
    }

    #[test]
    fn position_scale_basic() {
        assert!((position_scale(100, 50) - 0.5).abs() < 1e-12);
        assert!((position_scale(0, 10) - 1.0).abs() < 1e-12);
    }

    #[test]
    fn shift_shortens_when_faster() {
        let sr = 8_000u32;
        let frames = 8_000usize;
        let samples = stereo_sine(frames, 220.0, sr);
        let out = render_track(&samples, sr, sr, 2.0, PitchMode::Shift).expect("shift");
        let out_frames = out.len() / 2;
        let expected = frames / 2;
        assert!(
            (out_frames as isize - expected as isize).unsigned_abs() <= expected / 50 + 8,
            "out_frames={out_frames} expected≈{expected}"
        );
        assert!(out.iter().all(|x| x.is_finite()));
    }

    #[test]
    fn stretch_config_validation() {
        assert!(StretchConfig::official_default().validate().is_ok());
        assert!(StretchConfig {
            block_ms: 0.0,
            interval_ms: 30.0
        }
        .validate()
        .is_err());
        assert!(StretchConfig {
            block_ms: 30.0,
            interval_ms: 30.0
        }
        .validate()
        .is_err());
        assert!(StretchConfig {
            block_ms: f64::NAN,
            interval_ms: 30.0
        }
        .validate()
        .is_err());
        assert!(StretchConfig {
            block_ms: 120.0,
            interval_ms: -1.0
        }
        .validate()
        .is_err());
    }

    #[test]
    fn stretch_config_to_samples_matches_preset_default_at_44100() {
        let (block, interval) = StretchConfig::official_default()
            .to_samples(44_100)
            .expect("samples");
        // C++: (int)(44100 * 0.12), (int)(44100 * 0.03)
        assert_eq!(block, 5292);
        assert_eq!(interval, 1323);
    }

    #[test]
    fn preserve_passthrough_is_bit_identical() {
        let sr = 44_100u32;
        let samples = stereo_sine(2048, 440.0, sr);
        let out = render_track(&samples, sr, sr, 1.0, PitchMode::Preserve).expect("passthrough");
        assert_eq!(out, samples);
    }

    #[test]
    fn custom_config_output_duration_and_finite() {
        let sr = 44_100u32;
        // exact() needs output >= 2 * outputLatency; ~2s is enough for these windows.
        let in_frames = sr as usize * 2;
        let samples = stereo_sine(in_frames, 330.0, sr);
        let speed = 1.10;
        let expected_out = ((in_frames as f64) / speed).round().max(1.0) as usize;

        for cfg in [
            StretchConfig::official_default(),
            StretchConfig::transient_focused(),
            StretchConfig::long_window(),
        ] {
            let out =
                render_track_with_config(&samples, sr, sr, speed, PitchMode::Preserve, Some(cfg))
                    .unwrap_or_else(|e| panic!("config {cfg:?}: {e}"));
            assert_eq!(out.len() / 2, expected_out, "cfg={cfg:?}");
            assert!(out.iter().all(|s| s.is_finite()), "cfg={cfg:?}");
        }
    }
}
