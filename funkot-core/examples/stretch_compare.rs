//! Offline Signalsmith config comparison (diagnostic A/B, not a quality ranking).
//!
//! Decodes one input file, takes the first `--seconds` (default 90) of audio,
//! and writes pitch-preserving 1.10× candidates plus a pitch-shift (rubato)
//! clarity reference. All outputs are 32-bit float WAV with no gain normalization.
//!
//! Usage (inside the dev container):
//! ```sh
//! cargo run -p funkot-core --example stretch_compare --release -- \
//!     /path/to/track.flac /path/to/out_dir [--seconds 90] [--speed 1.10]
//! ```
//!
//! Outputs (names document the config; none is labeled superior):
//! - `preserve_official_120_30.wav` — Signalsmith preset_default (120 ms / 30 ms)
//! - `preserve_transient_80_20.wav` — shorter windows (diagnostic)
//! - `preserve_long_180_45.wav` — longer windows (diagnostic)
//! - `shift_rubato.wav` — pitch rises with tempo (clarity reference)

use std::path::{Path, PathBuf};

use funkot_core::decode;
use funkot_core::stretch::{render_track_with_config, StretchConfig};
use funkot_core::PitchMode;

fn main() {
    if let Err(e) = run() {
        eprintln!("error: {e}");
        std::process::exit(1);
    }
}

fn run() -> Result<(), String> {
    let mut args = std::env::args().skip(1);
    let input = args
        .next()
        .ok_or_else(|| usage("missing INPUT"))
        .map(PathBuf::from)?;
    let out_dir = args
        .next()
        .ok_or_else(|| usage("missing OUT_DIR"))
        .map(PathBuf::from)?;

    let mut seconds = 90.0f64;
    let mut speed = 1.10f64;
    while let Some(flag) = args.next() {
        match flag.as_str() {
            "--seconds" => {
                let v = args.next().ok_or("--seconds needs a value")?;
                seconds = v.parse().map_err(|_| format!("invalid --seconds: {v}"))?;
            }
            "--speed" => {
                let v = args.next().ok_or("--speed needs a value")?;
                speed = v.parse().map_err(|_| format!("invalid --speed: {v}"))?;
            }
            "-h" | "--help" => {
                eprintln!("{}", usage_text());
                return Ok(());
            }
            other => return Err(usage(&format!("unknown argument: {other}"))),
        }
    }

    if !(seconds.is_finite() && seconds > 0.0) {
        return Err(format!("--seconds must be finite and > 0, got {seconds}"));
    }
    if !(speed.is_finite() && speed > 0.0) {
        return Err(format!("--speed must be finite and > 0, got {speed}"));
    }

    std::fs::create_dir_all(&out_dir)
        .map_err(|e| format!("cannot create {}: {e}", out_dir.display()))?;

    println!("decoding {}...", input.display());
    let decoded = decode::decode_file(&input).map_err(|e| e.to_string())?;
    let sr = decoded.sample_rate;
    let max_frames = ((seconds * f64::from(sr)).round() as usize).max(1);
    let in_frames = (decoded.frames as usize).min(max_frames);
    let clip: Vec<f32> = decoded.samples[..in_frames * 2].to_vec();
    println!(
        "using {in_frames} frames ({:.2}s) at {sr} Hz; speed={speed}",
        in_frames as f64 / f64::from(sr)
    );

    let preserve_out = ((in_frames as f64) / speed).round().max(1.0) as usize;

    let candidates: &[(&str, Option<StretchConfig>, PitchMode)] = &[
        (
            "preserve_official_120_30.wav",
            Some(StretchConfig::official_default()),
            PitchMode::Preserve,
        ),
        (
            "preserve_transient_80_20.wav",
            Some(StretchConfig::transient_focused()),
            PitchMode::Preserve,
        ),
        (
            "preserve_long_180_45.wav",
            Some(StretchConfig::long_window()),
            PitchMode::Preserve,
        ),
        ("shift_rubato.wav", None, PitchMode::Shift),
    ];

    for (name, cfg, mode) in candidates {
        let path = out_dir.join(name);
        print!("rendering {name}... ");
        let out = render_track_with_config(&clip, sr, sr, speed, *mode, *cfg)
            .map_err(|e| format!("{name}: {e}"))?;
        let out_frames = out.len() / 2;
        if *mode == PitchMode::Preserve && out_frames != preserve_out {
            return Err(format!(
                "{name}: expected {preserve_out} frames, got {out_frames}"
            ));
        }
        if !out.iter().all(|s| s.is_finite()) {
            return Err(format!("{name}: non-finite samples"));
        }
        write_f32_wav(&path, sr, &out)?;
        println!("ok ({:.2}s)", out_frames as f64 / f64::from(sr));
    }

    println!("wrote candidates to {}", out_dir.display());
    println!(
        "note: alternate Signalsmith windows are diagnostic only; \
         production uses official 120ms/30ms preset_default."
    );
    Ok(())
}

fn write_f32_wav(path: &Path, sample_rate: u32, interleaved: &[f32]) -> Result<(), String> {
    let spec = hound::WavSpec {
        channels: 2,
        sample_rate,
        bits_per_sample: 32,
        sample_format: hound::SampleFormat::Float,
    };
    let mut writer = hound::WavWriter::create(path, spec)
        .map_err(|e| format!("create {}: {e}", path.display()))?;
    for &s in interleaved {
        let s = if s.is_finite() { s } else { 0.0 };
        writer
            .write_sample(s)
            .map_err(|e| format!("write {}: {e}", path.display()))?;
    }
    writer
        .finalize()
        .map_err(|e| format!("finalize {}: {e}", path.display()))?;
    Ok(())
}

fn usage(msg: &str) -> String {
    format!("{msg}\n\n{}", usage_text())
}

fn usage_text() -> &'static str {
    "Usage: stretch_compare INPUT OUT_DIR [--seconds 90] [--speed 1.10]"
}
