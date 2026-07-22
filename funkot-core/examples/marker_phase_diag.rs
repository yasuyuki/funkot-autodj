//! Compare analysis-mapped vs refined markers (v12 simplified rules).
//!
//! Usage:
//!   cargo run -p funkot-core --example marker_phase_diag --release -- \
//!     [--rate 1.10] [--cache-dir DIR] [--sr 48000] testdata/*.flac
//!
//! For each track, after stretch to target BPM, prints:
//!   - mapped / refined intro and outro markers (production path)
//!   - legacy intro-propagated grid (diagnostic only)
//!   - phase deltas (frames / ms) modulo target beat and bar

use std::path::PathBuf;

use funkot_core::cache;
use funkot_core::decode;
use funkot_core::engine::{
    derive_outro_start_out, legacy_intro_propagated_outro, prepare_output_markers,
    refine_output_downbeat,
};
use funkot_core::stretch::{self, position_scale};
use funkot_core::{EngineOptions, PitchMode, BEATS_PER_BAR, NOMINAL_BPM};

fn main() {
    let mut rate = 1.10f64;
    let mut cache_dir = PathBuf::from("testdata/real-cache-v4");
    let mut out_sr = 48_000u32;
    let mut files: Vec<PathBuf> = Vec::new();
    let mut args = std::env::args().skip(1).peekable();
    while let Some(arg) = args.next() {
        if arg == "--rate" {
            rate = args
                .next()
                .expect("--rate needs a value")
                .parse()
                .expect("rate");
        } else if arg == "--cache-dir" {
            cache_dir = PathBuf::from(args.next().expect("--cache-dir needs a path"));
        } else if arg == "--sr" {
            out_sr = args
                .next()
                .expect("--sr needs a value")
                .parse()
                .expect("sr");
        } else if arg.starts_with('-') {
            eprintln!("unknown flag: {arg}");
            std::process::exit(2);
        } else {
            files.push(PathBuf::from(arg));
        }
    }
    if files.is_empty() {
        eprintln!(
            "usage: marker_phase_diag [--rate 1.10] [--cache-dir DIR] [--sr 48000] <audio>..."
        );
        std::process::exit(2);
    }

    let options = EngineOptions {
        rate,
        pitch_mode: PitchMode::Preserve,
        output_sample_rate: out_sr,
        cache_dir: cache_dir.clone(),
        ..EngineOptions::default()
    };
    let target_bpm = NOMINAL_BPM * rate;
    let bar_frames = options.bar_frames();
    let beat_frames = bar_frames / f64::from(BEATS_PER_BAR);
    let beat_ms = beat_frames * 1000.0 / f64::from(out_sr);
    let bar_ms = bar_frames * 1000.0 / f64::from(out_sr);

    println!(
        "target_bpm={target_bpm:.3} out_sr={out_sr} beat={beat_frames:.1}f ({beat_ms:.2}ms) bar={bar_frames:.1}f ({bar_ms:.2}ms)"
    );
    println!("v13 rule: intro downbeat refine + outro on intro-propagated bar grid");
    println!();

    for (ti, path) in files.iter().enumerate() {
        println!("=== [{}] {} ===", ti + 1, path.display());
        let buf = match decode::decode_file(path) {
            Ok(b) => b,
            Err(e) => {
                eprintln!("  decode failed: {e}");
                continue;
            }
        };
        let analysis = match cache::get_or_analyze(path, &cache_dir, &buf) {
            Ok(a) => a,
            Err(e) => {
                eprintln!("  analyze failed: {e}");
                continue;
            }
        };
        let intro_bpm = if analysis.intro_bpm.is_finite() && analysis.intro_bpm > 0.0 {
            analysis.intro_bpm
        } else {
            NOMINAL_BPM
        };
        let speed = target_bpm / intro_bpm;
        println!(
            "  analysis: intro_bpm={:.3} outro_bpm={:.3} intro_bars={} outro_bars={} fd={} outro={}",
            analysis.intro_bpm,
            analysis.outro_bpm,
            analysis.intro_bars,
            analysis.outro_bars,
            analysis.first_downbeat,
            analysis.outro_start
        );
        println!("  stretch speed={speed:.6}");

        let rendered = match stretch::render_track(
            &buf.samples,
            buf.sample_rate,
            out_sr,
            speed,
            PitchMode::Preserve,
        ) {
            Ok(r) => r,
            Err(e) => {
                eprintln!("  stretch failed: {e}");
                continue;
            }
        };
        let out_frames = (rendered.len() / 2) as u64;
        let scale = position_scale(buf.frames, out_frames);
        let mapped_fd = (analysis.first_downbeat as f64 * scale).round() as u64;
        let mapped_outro = (analysis.outro_start as f64 * scale).round() as u64;
        let tail_expected =
            out_frames.saturating_sub((f64::from(analysis.outro_bars) * bar_frames).round() as u64);

        let refined_fd = refine_output_downbeat(&rendered, out_sr, mapped_fd, beat_frames);
        let refined_outro = derive_outro_start_out(
            &rendered,
            out_sr,
            out_frames,
            mapped_outro,
            analysis.outro_bars,
            bar_frames,
        );
        let (prod_fd, prod_outro) = prepare_output_markers(
            &rendered,
            out_sr,
            out_frames,
            analysis.first_downbeat,
            analysis.outro_start,
            analysis.intro_bpm,
            buf.sample_rate,
            mapped_fd,
            mapped_outro,
            analysis.outro_bars,
            bar_frames,
        );
        let legacy = legacy_intro_propagated_outro(
            analysis.first_downbeat,
            analysis.outro_start,
            analysis.intro_bpm,
            buf.sample_rate,
            prod_fd,
            bar_frames,
        );

        println!("  out_frames={out_frames} scale={scale:.8} tail_expected={tail_expected}");
        println!("  intro:  mapped={mapped_fd}  refined={refined_fd}  production={prod_fd}");
        print_delta(
            "    mapped→production",
            mapped_fd,
            prod_fd,
            out_sr,
            beat_frames,
            bar_frames,
        );
        println!(
            "  outro:  mapped={mapped_outro}  refined={refined_outro}  production={prod_outro}  legacy(grid)={legacy}"
        );
        print_delta(
            "    mapped→production",
            mapped_outro,
            prod_outro,
            out_sr,
            beat_frames,
            bar_frames,
        );
        print_delta(
            "    legacy→production",
            legacy,
            prod_outro,
            out_sr,
            beat_frames,
            bar_frames,
        );
        print_delta(
            "    tail_expected→production",
            tail_expected,
            prod_outro,
            out_sr,
            beat_frames,
            bar_frames,
        );

        let leg_ref = signed_mod(legacy as i64 - prod_outro as i64, beat_frames);
        let agree = leg_ref.abs() < beat_frames * 0.08;
        println!(
            "  verdict: legacy vs production {} (Δbeat_mod={leg_ref:.1}f / {:.2}ms)",
            if agree {
                "ALIGN (coincidence possible)"
            } else {
                "MISALIGN — intro grid ≠ analysis outro"
            },
            leg_ref.abs() * 1000.0 / f64::from(out_sr)
        );
        println!();
    }
}

fn print_delta(label: &str, a: u64, b: u64, sr: u32, beat: f64, bar: f64) {
    let d = b as i64 - a as i64;
    let ms = d as f64 * 1000.0 / f64::from(sr);
    let d_beat = signed_mod(d, beat);
    let d_bar = signed_mod(d, bar);
    println!(
        "{label}: Δ={d}f ({ms:+.2}ms)  mod_beat={d_beat:.1}f ({:.2}ms)  mod_bar={d_bar:.1}f ({:.2}ms)",
        d_beat.abs() * 1000.0 / f64::from(sr),
        d_bar.abs() * 1000.0 / f64::from(sr)
    );
}

fn signed_mod(delta: i64, period: f64) -> f64 {
    if !(period.is_finite() && period > 0.0) {
        return delta as f64;
    }
    let mut x = (delta as f64) % period;
    if x > period * 0.5 {
        x -= period;
    } else if x < -period * 0.5 {
        x += period;
    }
    x
}
