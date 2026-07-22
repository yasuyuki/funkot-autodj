//! Offline WAV writers with optional TPDF dither for integer PCM.

use std::fs::File;
use std::io::BufWriter;
use std::path::Path;

use anyhow::{Context, Result};
use clap::ValueEnum;

/// Offline render sample format (`--wav-format`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum, Default)]
pub enum WavFormat {
    /// 32-bit IEEE float (default). Preserves the f32 mix bus; no clamp.
    #[default]
    F32,
    /// 24-bit signed PCM with TPDF dither.
    S24,
    /// 16-bit signed PCM with TPDF dither (compatibility).
    S16,
}

impl WavFormat {
    pub fn wav_spec(self, sample_rate: u32) -> hound::WavSpec {
        match self {
            WavFormat::F32 => hound::WavSpec {
                channels: 2,
                sample_rate,
                bits_per_sample: 32,
                sample_format: hound::SampleFormat::Float,
            },
            WavFormat::S24 => hound::WavSpec {
                channels: 2,
                sample_rate,
                bits_per_sample: 24,
                sample_format: hound::SampleFormat::Int,
            },
            WavFormat::S16 => hound::WavSpec {
                channels: 2,
                sample_rate,
                bits_per_sample: 16,
                sample_format: hound::SampleFormat::Int,
            },
        }
    }
}

/// Peak / over-level stats accumulated while writing.
#[derive(Debug, Default, Clone)]
pub struct PeakStats {
    /// Maximum absolute sample value seen (after NaN/Inf → 0 sanitization).
    pub peak: f32,
    /// Number of interleaved samples with `|x| > 1.0`.
    pub over_samples: u64,
    /// Number of stereo frames containing at least one over sample.
    pub over_frames: u64,
}

impl PeakStats {
    pub fn observe_chunk(&mut self, chunk: &[f32]) {
        debug_assert!(chunk.len().is_multiple_of(2));
        for frame in chunk.chunks_exact(2) {
            let mut frame_over = false;
            for &s in frame {
                let x = sanitize(s);
                let a = x.abs();
                if a > self.peak {
                    self.peak = a;
                }
                if a > 1.0 {
                    self.over_samples += 1;
                    frame_over = true;
                }
            }
            if frame_over {
                self.over_frames += 1;
            }
        }
    }
}

/// Tiny deterministic xorshift32 PRNG for TPDF dither (no extra deps).
pub struct XorShift32 {
    state: u32,
}

impl XorShift32 {
    pub fn new(seed: u32) -> Self {
        Self {
            state: if seed == 0 { 0xA5A5_A5A5 } else { seed },
        }
    }

    fn next_u32(&mut self) -> u32 {
        let mut x = self.state;
        x ^= x << 13;
        x ^= x >> 17;
        x ^= x << 5;
        self.state = x;
        x
    }

    /// Uniform in (-0.5, 0.5).
    fn next_unit(&mut self) -> f64 {
        // Exclude 0 and 1.0 endpoints via (1..2^32) mapped to (0,1).
        let u = (self.next_u32() as f64) / (u32::MAX as f64 + 1.0);
        u - 0.5
    }

    /// Triangular PDF in (-1, 1) LSB units (sum of two independent uniforms).
    pub fn tpdf(&mut self) -> f64 {
        self.next_unit() + self.next_unit()
    }
}

#[inline]
fn sanitize(sample: f32) -> f32 {
    if sample.is_finite() {
        sample
    } else {
        0.0
    }
}

/// Quantize `sample` (full-scale ±1.0) to signed integer with ±1 LSB TPDF dither.
fn quantize_tpdf(sample: f32, max_pos: i32, rng: &mut XorShift32) -> i32 {
    let x = sanitize(sample);
    let scale = f64::from(max_pos);
    // dither amplitude = 1 LSB in the scaled domain
    let dithered = f64::from(x) * scale + rng.tpdf();
    // Two's complement: positive full-scale = max_pos, negative = -(max_pos+1).
    let min = i64::from(-(max_pos + 1));
    let max = i64::from(max_pos);
    (dithered.round() as i64).clamp(min, max) as i32
}

/// Streaming WAV writer for interleaved stereo f32 chunks.
pub struct WavStreamWriter {
    format: WavFormat,
    writer: hound::WavWriter<BufWriter<File>>,
    rng: XorShift32,
    pub stats: PeakStats,
}

impl WavStreamWriter {
    pub fn create(path: &Path, sample_rate: u32, format: WavFormat) -> Result<Self> {
        let spec = format.wav_spec(sample_rate);
        let writer = hound::WavWriter::create(path, spec)
            .with_context(|| format!("failed to create WAV {}", path.display()))?;
        Ok(Self {
            format,
            writer,
            rng: XorShift32::new(0xF01D_CAFE),
            stats: PeakStats::default(),
        })
    }

    pub fn write_interleaved(&mut self, chunk: &[f32]) -> Result<()> {
        self.stats.observe_chunk(chunk);
        match self.format {
            WavFormat::F32 => {
                for &sample in chunk {
                    let s = sanitize(sample);
                    self.writer
                        .write_sample(s)
                        .context("failed to write f32 WAV sample")?;
                }
            }
            WavFormat::S24 => {
                // hound 24-bit uses i32 in the range ±(2^23−1).
                const MAX: i32 = 8_388_607;
                for &sample in chunk {
                    let q = quantize_tpdf(sample, MAX, &mut self.rng);
                    self.writer
                        .write_sample(q)
                        .context("failed to write s24 WAV sample")?;
                }
            }
            WavFormat::S16 => {
                const MAX: i32 = 32_767;
                for &sample in chunk {
                    let q = quantize_tpdf(sample, MAX, &mut self.rng) as i16;
                    self.writer
                        .write_sample(q)
                        .context("failed to write s16 WAV sample")?;
                }
            }
        }
        Ok(())
    }

    pub fn finalize(self) -> Result<PeakStats> {
        let stats = self.stats;
        self.writer.finalize().context("failed to finalize WAV")?;
        Ok(stats)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sanitize_nonfinite() {
        assert_eq!(sanitize(f32::NAN), 0.0);
        assert_eq!(sanitize(f32::INFINITY), 0.0);
        assert_eq!(sanitize(1.5), 1.5);
    }

    #[test]
    fn peak_stats_count_overs() {
        let mut stats = PeakStats::default();
        // One frame with L over, one clean, one with both over.
        stats.observe_chunk(&[1.5, 0.0, 0.5, -0.5, -1.1, 1.2]);
        assert!((stats.peak - 1.5).abs() < 1e-6);
        assert_eq!(stats.over_samples, 3);
        assert_eq!(stats.over_frames, 2);
    }

    #[test]
    fn tpdf_quantize_stays_in_range() {
        let mut rng = XorShift32::new(1);
        for s in [-1.0f32, -0.5, 0.0, 0.5, 1.0, 1.5, -1.5] {
            let q16 = quantize_tpdf(s, i32::from(i16::MAX), &mut rng);
            assert!((i32::from(i16::MIN)..=i32::from(i16::MAX)).contains(&q16));
            let q24 = quantize_tpdf(s, 8_388_607, &mut rng);
            assert!((-8_388_608..=8_388_607).contains(&q24));
        }
    }

    #[test]
    fn f32_writer_preserves_over_unity() {
        let dir = std::env::temp_dir().join(format!(
            "funkot_wav_f32_{}_{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0)
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("t.wav");
        {
            let mut w = WavStreamWriter::create(&path, 8_000, WavFormat::F32).unwrap();
            w.write_interleaved(&[1.25, -1.25]).unwrap();
            let stats = w.finalize().unwrap();
            assert!(stats.peak > 1.0);
            assert_eq!(stats.over_samples, 2);
        }
        let mut reader = hound::WavReader::open(&path).unwrap();
        assert_eq!(reader.spec().sample_format, hound::SampleFormat::Float);
        let samples: Vec<f32> = reader.samples::<f32>().map(|s| s.unwrap()).collect();
        assert_eq!(samples, vec![1.25, -1.25]);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
