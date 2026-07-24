//! Synthetic Funkot-like track generator for tests.
//!
//! Enabled under `cfg(test)` or the `testutil` feature so engine tests can reuse it.

use std::f32::consts::PI;

use crate::decode::AudioBuffer;
use crate::BEATS_PER_BAR;

/// Build a stereo synthetic track with kick throughout and mid/high content in the main only.
///
/// - Kick: 60 Hz sine burst, exponential decay (~80 ms), amplitude ~0.8, every beat.
/// - Intro/outro: optional quiet noise ticks on offbeats.
/// - Main: 2.5 kHz tone bursts + band-limited noise on every beat.
/// - Starts exactly on beat 1; ends exactly at the last bar boundary.
pub fn synth_track(
    bpm: f64,
    intro_bars: u32,
    main_bars: u32,
    outro_bars: u32,
    sample_rate: u32,
) -> AudioBuffer {
    synth_track_with_options(SynthOptions {
        bpm,
        intro_bars,
        main_bars,
        outro_bars,
        sample_rate,
        lead_in_secs: 0.0,
        amplitude_scale: 1.0,
        main_midhigh: true,
        intro_outro_midhigh: false,
        intro_bright_level: 0.0,
        outro_bright_level: 0.0,
        outro_mid_plateau_bars: 0,
        outro_mid_plateau_level: 0.0,
        gradual_intro_layers: false,
        main_bass_boost: 0.0,
        main_sparse_bars: 0,
    })
}

/// Options for more specific synthetic fixtures.
#[derive(Debug, Clone)]
pub struct SynthOptions {
    pub bpm: f64,
    pub intro_bars: u32,
    pub main_bars: u32,
    pub outro_bars: u32,
    pub sample_rate: u32,
    /// Silence prepended before the first kick (seconds).
    pub lead_in_secs: f64,
    /// Global amplitude multiplier (e.g. 0.1 for quiet-track gain tests).
    pub amplitude_scale: f32,
    /// If true, main section gets mid/high content.
    pub main_midhigh: bool,
    /// If true, intro and outro also get full mid/high content (no section contrast).
    pub intro_outro_midhigh: bool,
    /// Extra mid/high level in the intro (0 = kick-only, 1 ≈ main brightness).
    /// Models bright percussion already present before the main drop.
    pub intro_bright_level: f32,
    /// Extra mid/high level in the outro (same scale as [`Self::intro_bright_level`]).
    pub outro_bright_level: f32,
    /// Bars of partial mid/high at the start of the outro (nearest the main).
    /// Remaining outro bars stay sparse — models a mid-outro plateau before the
    /// final energy floor (IVY-like).
    pub outro_mid_plateau_bars: u32,
    /// Brightness for [`Self::outro_mid_plateau_bars`] (0..1 scale).
    pub outro_mid_plateau_level: f32,
    /// If true, intro mid/high ramps linearly from 0 to `intro_bright_level`
    /// across the intro (gradual layering).
    pub gradual_intro_layers: bool,
    /// Extra low-mid (180 Hz) tone amplitude in the main only. Lets the main
    /// grow in RMS/body without a large HF-ratio jump.
    pub main_bass_boost: f32,
    /// First N bars of the main stay kick-only (even when `main_midhigh`).
    /// Models tension-drop main entries that later rebuild mid/high
    /// (Love & Joy-like: true intro 64, false 80 on rebuild).
    pub main_sparse_bars: u32,
}

impl Default for SynthOptions {
    fn default() -> Self {
        Self {
            bpm: 180.0,
            intro_bars: 16,
            main_bars: 32,
            outro_bars: 16,
            sample_rate: 44_100,
            lead_in_secs: 0.0,
            amplitude_scale: 1.0,
            main_midhigh: true,
            intro_outro_midhigh: false,
            intro_bright_level: 0.0,
            outro_bright_level: 0.0,
            outro_mid_plateau_bars: 0,
            outro_mid_plateau_level: 0.0,
            gradual_intro_layers: false,
            main_bass_boost: 0.0,
            main_sparse_bars: 0,
        }
    }
}

/// Like [`synth_track`] but with full control over lead-in, gain, and section contrast.
pub fn synth_track_with_options(opt: SynthOptions) -> AudioBuffer {
    let sr = opt.sample_rate as f64;
    let beat_period = 60.0 / opt.bpm;
    let beat_frames = (beat_period * sr).round() as usize;
    let bar_frames = beat_frames * BEATS_PER_BAR as usize;
    let total_bars = opt.intro_bars + opt.main_bars + opt.outro_bars;
    let body_frames = bar_frames * total_bars as usize;
    let lead_frames = (opt.lead_in_secs * sr).round().max(0.0) as usize;
    let total_frames = lead_frames + body_frames;

    let mut mono = vec![0.0f32; total_frames];
    let kick_len = ((0.080 * sr) as usize).max(1);
    let tick_len = ((0.012 * sr) as usize).max(1);
    let tone_len = ((0.040 * sr) as usize).max(1);

    let intro_end = lead_frames + bar_frames * opt.intro_bars as usize;
    let main_end = intro_end + bar_frames * opt.main_bars as usize;
    let intro_beats = (opt.intro_bars as usize) * BEATS_PER_BAR as usize;
    let main_sparse_end =
        intro_end + bar_frames * (opt.main_sparse_bars.min(opt.main_bars) as usize);
    let plateau_bars = opt
        .outro_mid_plateau_bars
        .min(opt.outro_bars.saturating_sub(1)) as usize;
    let plateau_end = main_end + bar_frames * plateau_bars;

    let mut rng = 0xC0FFEE_u32;

    for beat_idx in 0..(total_bars as usize * BEATS_PER_BAR as usize) {
        let beat_start = lead_frames + beat_idx * beat_frames;
        let in_intro = beat_start < intro_end;
        let in_main = beat_start >= intro_end && beat_start < main_end;
        let in_main_sparse = in_main && beat_start < main_sparse_end;
        let in_outro = beat_start >= main_end;
        let in_outro_plateau = in_outro && beat_start < plateau_end && plateau_bars > 0;

        // Kick on every beat (thinner during sparse main entry = tension drop).
        let kick_amp = if in_main_sparse { 0.35 } else { 0.8 };
        add_kick(
            &mut mono,
            beat_start,
            kick_len,
            sr,
            kick_amp * opt.amplitude_scale,
        );

        let mut bright = 0.0f32;
        if (in_main && opt.main_midhigh && !in_main_sparse)
            || (opt.intro_outro_midhigh && (in_intro || in_outro))
        {
            bright = 1.0;
        } else if in_intro && opt.intro_bright_level > 0.0 {
            if opt.gradual_intro_layers && intro_beats > 1 {
                let t = beat_idx as f32 / (intro_beats as f32 - 1.0);
                bright = opt.intro_bright_level * t.clamp(0.0, 1.0);
            } else {
                bright = opt.intro_bright_level;
            }
        } else if in_outro_plateau && opt.outro_mid_plateau_level > 0.0 {
            bright = opt.outro_mid_plateau_level;
        } else if in_outro && opt.outro_bright_level > 0.0 {
            bright = opt.outro_bright_level;
        }

        if bright > 0.0 {
            add_tone_burst(
                &mut mono,
                beat_start,
                tone_len,
                sr,
                2500.0,
                0.35 * bright * opt.amplitude_scale,
            );
            add_noise_burst(
                &mut mono,
                beat_start,
                tone_len,
                0.25 * bright * opt.amplitude_scale,
                &mut rng,
            );
            // Intro body pad: short HF bursts alone barely move bar RMS, so
            // tension-drop cues (bright intro → sparse main) need sustained mid.
            if in_intro && opt.intro_bright_level > 0.0 {
                add_tone_burst(
                    &mut mono,
                    beat_start,
                    (beat_frames / 2).max(1),
                    sr,
                    500.0,
                    0.28 * bright * opt.amplitude_scale,
                );
            }
        }

        if in_main && opt.main_bass_boost > 0.0 {
            add_tone_burst(
                &mut mono,
                beat_start,
                kick_len,
                sr,
                180.0,
                opt.main_bass_boost * opt.amplitude_scale,
            );
        }

        // Quiet offbeat tick in sparse intro/outro only.
        let sparse_io = (in_intro || in_outro)
            && !opt.intro_outro_midhigh
            && bright < 0.15
            && opt.intro_bright_level < 0.15
            && opt.outro_bright_level < 0.15;
        if sparse_io {
            let off = beat_start + beat_frames / 2;
            if off + tick_len < mono.len() {
                add_noise_burst(
                    &mut mono,
                    off,
                    tick_len,
                    0.08 * opt.amplitude_scale,
                    &mut rng,
                );
            }
        }
    }

    // Interleaved stereo (identical L/R).
    let mut samples = Vec::with_capacity(total_frames * 2);
    for &s in &mono {
        samples.push(s);
        samples.push(s);
    }

    AudioBuffer {
        sample_rate: opt.sample_rate,
        frames: total_frames as u64,
        samples,
    }
}

fn add_kick(buf: &mut [f32], start: usize, len: usize, sr: f64, amp: f32) {
    let end = (start + len).min(buf.len());
    let tau = 0.020f64; // ~80 ms audible with e^{-t/tau}
    for (i, frame) in (start..end).enumerate() {
        let t = i as f64 / sr;
        let env = (-t / tau).exp() as f32;
        let s = amp * env * (2.0 * PI * 60.0 * t as f32).sin();
        buf[frame] += s;
    }
}

fn add_tone_burst(buf: &mut [f32], start: usize, len: usize, sr: f64, freq: f32, amp: f32) {
    let end = (start + len).min(buf.len());
    for (i, frame) in (start..end).enumerate() {
        let t = i as f32 / sr as f32;
        let env = 1.0 - (i as f32 / len as f32);
        buf[frame] += amp * env * (2.0 * PI * freq * t).sin();
    }
}

fn add_noise_burst(buf: &mut [f32], start: usize, len: usize, amp: f32, rng: &mut u32) {
    let end = (start + len).min(buf.len());
    // Simple 1-pole low-pass to keep noise from being pure white (still mid/high rich).
    let mut lp = 0.0f32;
    for (i, frame) in (start..end).enumerate() {
        let white = next_rand(rng) * 2.0 - 1.0;
        lp = 0.65 * lp + 0.35 * white;
        let env = 1.0 - (i as f32 / len.max(1) as f32);
        buf[frame] += amp * env * lp;
    }
}

fn next_rand(state: &mut u32) -> f32 {
    // xorshift32
    let mut x = *state;
    x ^= x << 13;
    x ^= x >> 17;
    x ^= x << 5;
    *state = x;
    (x as f32) / (u32::MAX as f32)
}

/// Write an [`AudioBuffer`] as a 16-bit stereo WAV for cache / decode round-trips.
pub fn write_wav(path: &std::path::Path, buffer: &AudioBuffer) -> std::io::Result<()> {
    let spec = hound::WavSpec {
        channels: 2,
        sample_rate: buffer.sample_rate,
        bits_per_sample: 16,
        sample_format: hound::SampleFormat::Int,
    };
    let mut writer =
        hound::WavWriter::create(path, spec).map_err(|e| std::io::Error::other(e.to_string()))?;
    for &s in &buffer.samples {
        let clamped = s.clamp(-1.0, 1.0);
        let sample = (clamped * f32::from(i16::MAX)) as i16;
        writer
            .write_sample(sample)
            .map_err(|e| std::io::Error::other(e.to_string()))?;
    }
    writer
        .finalize()
        .map_err(|e| std::io::Error::other(e.to_string()))?;
    Ok(())
}
