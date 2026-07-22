//! Audio decoding via symphonia.

use std::fs::File;
use std::path::Path;

use symphonia::core::codecs::audio::AudioDecoderOptions;
use symphonia::core::errors::Error as SymphoniaError;
use symphonia::core::formats::probe::Hint;
use symphonia::core::formats::FormatOptions;
use symphonia::core::formats::TrackType;
use symphonia::core::io::MediaSourceStream;
use symphonia::core::meta::MetadataOptions;

use crate::{Error, Result};

/// Fully decoded audio: interleaved stereo f32 at the file's native sample rate.
#[derive(Debug, Clone)]
pub struct AudioBuffer {
    pub sample_rate: u32,
    /// Number of frames (per-channel samples).
    pub frames: u64,
    /// Interleaved stereo, `len == frames * 2`.
    pub samples: Vec<f32>,
}

impl AudioBuffer {
    /// Mixdown of the whole buffer to mono.
    pub fn mono(&self) -> Vec<f32> {
        self.mono_range(0, self.frames)
    }

    /// Mixdown of frames `[start, start+len)` to mono. Clamps to buffer end.
    pub fn mono_range(&self, start: u64, len: u64) -> Vec<f32> {
        let start = start.min(self.frames);
        let end = start.saturating_add(len).min(self.frames);
        let n = (end - start) as usize;
        let mut out = Vec::with_capacity(n);
        let base = start as usize * 2;
        for i in 0..n {
            let l = self.samples[base + i * 2];
            let r = self.samples[base + i * 2 + 1];
            out.push(0.5 * (l + r));
        }
        out
    }
}

/// Decode an entire audio file to stereo f32 at native sample rate.
///
/// Supported: MP3, AAC (m4a), ALAC (m4a), FLAC, Ogg Vorbis, WAV.
pub fn decode_file(path: &Path) -> Result<AudioBuffer> {
    let file = File::open(path)?;
    let mss = MediaSourceStream::new(Box::new(file), Default::default());

    let mut hint = Hint::new();
    if let Some(ext) = path.extension().and_then(|e| e.to_str()) {
        hint.with_extension(ext);
    }

    let fmt_opts = FormatOptions::default();
    let meta_opts = MetadataOptions::default();

    let mut format = symphonia::default::get_probe()
        .probe(&hint, mss, fmt_opts, meta_opts)
        .map_err(|e| {
            let ext = path
                .extension()
                .and_then(|e| e.to_str())
                .unwrap_or("(none)");
            Error::UnsupportedFormat(format!(
                "cannot probe '{}': {} (extension: {})",
                path.display(),
                e,
                ext
            ))
        })?;

    let track = format
        .default_track(TrackType::Audio)
        .ok_or_else(|| Error::Decode(format!("no audio track in '{}'", path.display())))?;

    let track_id = track.id;
    let audio_params = track
        .codec_params
        .as_ref()
        .and_then(|p| p.audio())
        .ok_or_else(|| {
            Error::Decode(format!(
                "missing audio codec parameters in '{}'",
                path.display()
            ))
        })?;

    if audio_params.sample_rate == Some(0) {
        return Err(Error::Decode(format!(
            "sample rate is 0 in '{}'",
            path.display()
        )));
    }

    let mut sample_rate = audio_params.sample_rate.filter(|&r| r > 0);

    let dec_opts = AudioDecoderOptions::default();
    let mut decoder = symphonia::default::get_codecs()
        .make_audio_decoder(audio_params, &dec_opts)
        .map_err(|e| {
            Error::Decode(format!(
                "cannot create decoder for '{}': {}",
                path.display(),
                e
            ))
        })?;

    let mut samples = Vec::new();
    let mut packet_buf = Vec::new();

    loop {
        let packet = match format.next_packet() {
            Ok(Some(packet)) => packet,
            Ok(None) => break,
            Err(SymphoniaError::ResetRequired) => break,
            Err(SymphoniaError::DecodeError(_)) => continue,
            // Any I/O error on the underlying file is not recoverable by
            // retrying next_packet(); treat it as end-of-stream.
            Err(SymphoniaError::IoError(_)) => break,
            Err(e) => {
                return Err(Error::Decode(format!(
                    "demux error in '{}': {}",
                    path.display(),
                    e
                )));
            }
        };

        if packet.track_id != track_id {
            continue;
        }

        match decoder.decode(&packet) {
            Ok(audio_buf) => {
                let rate = audio_buf.spec().rate();
                if rate == 0 {
                    return Err(Error::Decode(format!(
                        "decoded sample rate is 0 in '{}'",
                        path.display()
                    )));
                }
                if sample_rate.is_none() {
                    sample_rate = Some(rate);
                }

                let channels = audio_buf.spec().channels().count();
                if channels == 0 {
                    continue;
                }

                audio_buf.copy_to_vec_interleaved(&mut packet_buf);
                append_as_stereo(&mut samples, &packet_buf, channels);
            }
            Err(SymphoniaError::DecodeError(_)) | Err(SymphoniaError::IoError(_)) => continue,
            Err(SymphoniaError::ResetRequired) => break,
            Err(e) => {
                return Err(Error::Decode(format!(
                    "decode error in '{}': {}",
                    path.display(),
                    e
                )));
            }
        }
    }

    let sample_rate = sample_rate.ok_or_else(|| {
        Error::Decode(format!(
            "could not determine sample rate for '{}'",
            path.display()
        ))
    })?;

    let frames = (samples.len() / 2) as u64;
    Ok(AudioBuffer {
        sample_rate,
        frames,
        samples,
    })
}

/// Convert interleaved multi-channel samples to interleaved stereo.
///
/// - 1ch → duplicate to L/R
/// - 2ch → as-is
/// - >2ch → take channels 0 and 1
fn append_as_stereo(dst: &mut Vec<f32>, interleaved: &[f32], channels: usize) {
    if channels == 0 || interleaved.is_empty() {
        return;
    }
    let frames = interleaved.len() / channels;
    match channels {
        1 => {
            for &s in &interleaved[..frames] {
                dst.push(s);
                dst.push(s);
            }
        }
        2 => {
            dst.extend_from_slice(&interleaved[..frames * 2]);
        }
        n => {
            for frame in 0..frames {
                let base = frame * n;
                dst.push(interleaved[base]);
                dst.push(interleaved[base + 1]);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::f32::consts::PI;
    use std::path::PathBuf;

    const SAMPLE_RATE: u32 = 44_100;
    const AMPLITUDE: f32 = 0.5;
    const FREQ_HZ: f32 = 440.0;
    const DURATION_SECS: f32 = 0.5;

    fn temp_wav_path(name: &str) -> PathBuf {
        let mut path = std::env::temp_dir();
        path.push(format!(
            "funkot_decode_{}_{}_{}.wav",
            name,
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0)
        ));
        path
    }

    fn write_sine_wav(path: &Path, channels: u16) {
        let spec = hound::WavSpec {
            channels,
            sample_rate: SAMPLE_RATE,
            bits_per_sample: 16,
            sample_format: hound::SampleFormat::Int,
        };
        let mut writer = hound::WavWriter::create(path, spec).expect("create wav");
        let n_frames = (SAMPLE_RATE as f32 * DURATION_SECS) as u32;
        for i in 0..n_frames {
            let t = i as f32 / SAMPLE_RATE as f32;
            let s = (AMPLITUDE * (2.0 * PI * FREQ_HZ * t).sin() * i16::MAX as f32) as i16;
            for _ in 0..channels {
                writer.write_sample(s).expect("write sample");
            }
        }
        writer.finalize().expect("finalize wav");
    }

    fn rms(samples: &[f32]) -> f32 {
        if samples.is_empty() {
            return 0.0;
        }
        let sum_sq: f64 = samples.iter().map(|&s| (s as f64) * (s as f64)).sum();
        (sum_sq / samples.len() as f64).sqrt() as f32
    }

    #[test]
    fn decode_stereo_sine_wav() {
        let path = temp_wav_path("stereo");
        write_sine_wav(&path, 2);

        let buf = decode_file(&path).expect("decode stereo wav");
        let _ = std::fs::remove_file(&path);

        let expected_frames = (SAMPLE_RATE as f32 * DURATION_SECS).round() as u64;
        assert_eq!(buf.sample_rate, SAMPLE_RATE);
        assert!(
            buf.frames.abs_diff(expected_frames) <= 1,
            "frames {} not within ±1 of {}",
            buf.frames,
            expected_frames
        );
        assert_eq!(buf.samples.len() as u64, buf.frames * 2);

        let expected_rms = 0.707 * AMPLITUDE;
        let actual_rms = rms(&buf.samples);
        let rel_err = (actual_rms - expected_rms).abs() / expected_rms;
        assert!(
            rel_err < 0.01,
            "RMS {actual_rms} vs expected {expected_rms} (rel err {rel_err})"
        );
    }

    #[test]
    fn decode_mono_wav_duplicates_to_stereo() {
        let path = temp_wav_path("mono");
        write_sine_wav(&path, 1);

        let buf = decode_file(&path).expect("decode mono wav");
        let _ = std::fs::remove_file(&path);

        assert_eq!(buf.sample_rate, SAMPLE_RATE);
        assert_eq!(buf.samples.len() as u64, buf.frames * 2);
        for frame in 0..buf.frames as usize {
            let l = buf.samples[frame * 2];
            let r = buf.samples[frame * 2 + 1];
            assert_eq!(l, r, "L != R at frame {frame}");
        }
    }

    #[test]
    fn mono_and_mono_range_clamping() {
        let buf = AudioBuffer {
            sample_rate: 48_000,
            frames: 4,
            samples: vec![
                1.0, 3.0, // → 2.0
                2.0, 4.0, // → 3.0
                0.0, 2.0, // → 1.0
                -2.0, 0.0, // → -1.0
            ],
        };

        assert_eq!(buf.mono(), vec![2.0, 3.0, 1.0, -1.0]);
        assert_eq!(buf.mono_range(1, 2), vec![3.0, 1.0]);
        // start past end → empty
        assert!(buf.mono_range(10, 5).is_empty());
        // len clamps to end
        assert_eq!(buf.mono_range(2, 100), vec![1.0, -1.0]);
        // start + len overflow clamps
        assert_eq!(buf.mono_range(u64::MAX - 1, 10), Vec::<f32>::new());
    }

    #[test]
    fn unsupported_format_returns_error() {
        let path = temp_wav_path("bogus");
        std::fs::write(&path, b"not an audio file at all").expect("write bogus");
        let err = decode_file(&path).expect_err("should fail");
        let _ = std::fs::remove_file(&path);
        match err {
            Error::UnsupportedFormat(msg) => {
                assert!(msg.contains("bogus") || msg.contains("extension"));
            }
            other => panic!("expected UnsupportedFormat, got {other}"),
        }
    }
}
