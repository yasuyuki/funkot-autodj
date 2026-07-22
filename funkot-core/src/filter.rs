//! RBJ biquad high-pass filter (stereo, independent channel state).

/// Stereo high-pass: two independent RBJ biquads, Q = 1/√2 (Butterworth).
///
/// Passes frequencies above the cutoff; attenuates low/bass. Used as the
/// transition mid/high-pass (default ~300 Hz).
pub struct StereoHighPass {
    l: Biquad,
    r: Biquad,
}

struct Biquad {
    b0: f64,
    b1: f64,
    b2: f64,
    a1: f64,
    a2: f64,
    z1: f64,
    z2: f64,
}

impl Biquad {
    fn highpass(sample_rate: u32, cutoff_hz: f32) -> Self {
        let sr = f64::from(sample_rate).max(1.0);
        let mut fc = f64::from(cutoff_hz);
        // Keep the design frequency inside (0, Nyquist).
        let nyquist = sr * 0.5;
        if !(fc.is_finite() && fc > 0.0) {
            fc = 1.0;
        }
        fc = fc.min(nyquist * 0.99);

        let w0 = std::f64::consts::TAU * fc / sr;
        let cos_w0 = w0.cos();
        let sin_w0 = w0.sin();
        let q = std::f64::consts::FRAC_1_SQRT_2;
        let alpha = sin_w0 / (2.0 * q);

        // RBJ Cookbook high-pass: b0=(1+cos)/2, b1=-(1+cos), b2=(1+cos)/2.
        let b0 = (1.0 + cos_w0) * 0.5;
        let b1 = -(1.0 + cos_w0);
        let b2 = (1.0 + cos_w0) * 0.5;
        let a0 = 1.0 + alpha;
        let a1 = -2.0 * cos_w0;
        let a2 = 1.0 - alpha;

        Self {
            b0: b0 / a0,
            b1: b1 / a0,
            b2: b2 / a0,
            a1: a1 / a0,
            a2: a2 / a0,
            z1: 0.0,
            z2: 0.0,
        }
    }

    fn reset(&mut self) {
        self.z1 = 0.0;
        self.z2 = 0.0;
    }

    fn process(&mut self, x: f32) -> f32 {
        let x = f64::from(x);
        let y = self.b0 * x + self.z1;
        self.z1 = self.b1 * x - self.a1 * y + self.z2;
        self.z2 = self.b2 * x - self.a2 * y;
        y as f32
    }
}

impl StereoHighPass {
    pub fn new(sample_rate: u32, cutoff_hz: f32) -> Self {
        Self {
            l: Biquad::highpass(sample_rate, cutoff_hz),
            r: Biquad::highpass(sample_rate, cutoff_hz),
        }
    }

    pub fn reset(&mut self) {
        self.l.reset();
        self.r.reset();
    }

    /// Process one interleaved stereo frame in place.
    pub fn process_frame(&mut self, l: &mut f32, r: &mut f32) {
        *l = self.l.process(*l);
        *r = self.r.process(*r);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rms_after_settle(
        f: &mut StereoHighPass,
        freq_hz: f64,
        sr: u32,
        settle: usize,
        measure: usize,
    ) -> f32 {
        let w = std::f64::consts::TAU * freq_hz / f64::from(sr);
        for n in 0..settle {
            let x = (w * n as f64).sin() as f32;
            let mut l = x;
            let mut r = x;
            f.process_frame(&mut l, &mut r);
        }
        let mut sum_sq = 0.0f64;
        for n in settle..(settle + measure) {
            let x = (w * n as f64).sin() as f32;
            let mut l = x;
            let mut r = x;
            f.process_frame(&mut l, &mut r);
            sum_sq += f64::from(l) * f64::from(l);
            // Stereo channels should match for identical input.
            assert!((l - r).abs() < 1e-6, "stereo mismatch l={l} r={r}");
        }
        (sum_sq / measure as f64).sqrt() as f32
    }

    #[test]
    fn silence_and_reset_are_safe() {
        let mut f = StereoHighPass::new(48_000, 300.0);
        let mut l = 0.0f32;
        let mut r = 0.0f32;
        f.process_frame(&mut l, &mut r);
        assert_eq!(l, 0.0);
        assert_eq!(r, 0.0);

        // Drive with signal then reset; silence must stay quiet.
        for _ in 0..1000 {
            l = 0.5;
            r = -0.5;
            f.process_frame(&mut l, &mut r);
        }
        f.reset();
        l = 0.0;
        r = 0.0;
        f.process_frame(&mut l, &mut r);
        assert_eq!(l, 0.0);
        assert_eq!(r, 0.0);
    }

    #[test]
    fn dc_and_bass_attenuated_highs_pass() {
        let sr = 48_000u32;
        let cutoff = 300.0f32;
        let settle = 20_000usize;
        let measure = 8_000usize;

        // DC: steady 1.0 must be strongly attenuated (HPF blocks DC).
        let mut f = StereoHighPass::new(sr, cutoff);
        let mut l = 0.0f32;
        let mut r = 0.0f32;
        for _ in 0..settle {
            l = 1.0;
            r = 1.0;
            f.process_frame(&mut l, &mut r);
        }
        assert!(l.abs() < 1e-3, "DC left not attenuated: {l}");
        assert!(r.abs() < 1e-3, "DC right not attenuated: {r}");

        // ~50 Hz (well below 300 Hz) strongly attenuated vs unity-ish input RMS.
        let mut f50 = StereoHighPass::new(sr, cutoff);
        let rms_50 = rms_after_settle(&mut f50, 50.0, sr, settle, measure);
        assert!(
            rms_50 < 0.05,
            "50 Hz should be strongly attenuated, rms={rms_50}"
        );

        // 2 kHz (well above 300 Hz) should pass near full level (sine RMS ≈ 0.707).
        let mut f2k = StereoHighPass::new(sr, cutoff);
        let rms_2k = rms_after_settle(&mut f2k, 2_000.0, sr, settle, measure);
        assert!(rms_2k > 0.55, "2 kHz should pass, rms={rms_2k}");
        assert!(
            rms_2k > rms_50 * 10.0,
            "2 kHz ({rms_2k}) should greatly exceed 50 Hz ({rms_50})"
        );
    }
}
