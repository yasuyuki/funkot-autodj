//! Golden/tolerance tests for downbeat + section detection.
//!
//! `tests/fixtures/golden.json` stores synth recipes + tolerances.
//! Optional WAVs (gitignored) can be regenerated with:
//!   `cargo run -p funkot-cli --release -- --gen-test-fixtures funkot-core/tests/fixtures`

use std::fs;
use std::path::PathBuf;

use funkot_core::analysis::{analyze, lock_beat_phase, refine_groove_phase};
use funkot_core::testutil::{synth_track_with_options, SynthOptions};
use funkot_core::BEATS_PER_BAR;
use serde_json::Value;

fn fixtures_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures")
}

fn load_golden() -> Value {
    let path = fixtures_dir().join("golden.json");
    let raw = fs::read_to_string(&path).unwrap_or_else(|e| {
        panic!(
            "missing {}: {e}\nregen: ./dev.sh cargo run -p funkot-cli --release -- --gen-test-fixtures funkot-core/tests/fixtures",
            path.display()
        )
    });
    serde_json::from_str(&raw).expect("golden.json")
}

fn synth_from_recipe(synth: &Value) -> funkot_core::decode::AudioBuffer {
    let opt = SynthOptions {
        bpm: synth["bpm"].as_f64().expect("bpm"),
        intro_bars: synth["intro_bars"].as_u64().expect("intro_bars") as u32,
        main_bars: synth["main_bars"].as_u64().expect("main_bars") as u32,
        outro_bars: synth["outro_bars"].as_u64().expect("outro_bars") as u32,
        sample_rate: synth["sample_rate"].as_u64().unwrap_or(44_100) as u32,
        lead_in_secs: synth["lead_in_secs"].as_f64().unwrap_or(0.0),
        ..SynthOptions::default()
    };
    synth_track_with_options(opt)
}

fn bar_len_frames(bpm: f64, sample_rate: u32) -> f64 {
    f64::from(sample_rate) * 60.0 / bpm * f64::from(BEATS_PER_BAR)
}

#[test]
fn golden_downbeat_and_section_tolerances() {
    let doc = load_golden();
    let tracks = doc["tracks"].as_array().expect("tracks array");
    assert!(!tracks.is_empty(), "golden.json has no tracks");

    for entry in tracks {
        let file = entry["file"].as_str().unwrap_or("synth");
        let expect = &entry["expect"];
        let buf = synth_from_recipe(&entry["synth"]);
        let a = analyze(&buf, file).unwrap_or_else(|e| panic!("analyze {file}: {e}"));

        let fd_secs = a.first_downbeat as f64 / f64::from(a.sample_rate);
        let fd_expect = expect["first_downbeat_secs"].as_f64().unwrap_or(0.0);
        let fd_tol = expect["first_downbeat_tol_secs"].as_f64().unwrap_or(0.05);
        assert!(
            (fd_secs - fd_expect).abs() <= fd_tol,
            "{file}: first_downbeat {fd_secs:.4}s outside {fd_expect}±{fd_tol}"
        );

        if let Some(bpm) = expect["intro_bpm"].as_f64() {
            let tol = expect["bpm_tol"].as_f64().unwrap_or(0.3);
            assert!(
                (a.intro_bpm - bpm).abs() <= tol,
                "{file}: intro_bpm {} vs {bpm}±{tol}",
                a.intro_bpm
            );
        }

        if let Some(bars) = expect["intro_bars"].as_u64() {
            assert_eq!(a.intro_bars as u64, bars, "{file}: intro_bars");
        }
        if let Some(bars) = expect["outro_bars"].as_u64() {
            assert_eq!(a.outro_bars as u64, bars, "{file}: outro_bars");
        }
        if let Some(min) = expect["intro_bars_min"].as_u64() {
            assert!(
                a.intro_bars as u64 >= min,
                "{file}: intro_bars {} < min {min}",
                a.intro_bars
            );
        }
        if let Some(max) = expect["outro_bars_max"].as_u64() {
            assert!(
                a.outro_bars as u64 <= max,
                "{file}: outro_bars {} > max {max}",
                a.outro_bars
            );
        }
        if expect["require_intro_ge_outro"].as_bool() == Some(true) {
            assert!(
                a.intro_bars >= a.outro_bars,
                "{file}: intro {} < outro {}",
                a.intro_bars,
                a.outro_bars
            );
        }

        if let Some(bars_from_fd) = expect["outro_start_bars_from_fd"].as_u64() {
            let expected = a.first_downbeat as f64
                + (bars_from_fd as f64) * bar_len_frames(a.intro_bpm, a.sample_rate);
            let tol =
                expect["outro_start_tol_secs"].as_f64().unwrap_or(0.12) * f64::from(a.sample_rate);
            let err = (a.outro_start as f64 - expected).abs();
            assert!(
                err <= tol,
                "{file}: outro_start err {:.1} frames (tol {tol:.1})",
                err
            );
        }
    }
}

#[test]
fn groove_refine_stays_within_half_beat_of_marker() {
    let sr = 44_100u32;
    let bpm = 180.0;
    let beat = f64::from(sr) * 60.0 / bpm;
    let n_beats = 32u32;
    let n = (f64::from(n_beats) * beat).round() as usize;
    let kick_len = ((0.06 * f64::from(sr)) as usize).max(1);
    let true_fd = (0.5 * beat).round() as u64;

    let mut mono = vec![0.0f32; n + true_fd as usize];
    for b in 0..n_beats {
        let start = true_fd as usize + (f64::from(b) * beat).round() as usize;
        if start >= mono.len() {
            break;
        }
        let end = (start + kick_len).min(mono.len());
        for (i, frame) in (start..end).enumerate() {
            let t = i as f64 / f64::from(sr);
            let env = (-t / 0.03).exp() as f32;
            mono[frame] += 0.9 * env * (2.0 * std::f64::consts::PI * 60.0 * t).sin() as f32;
        }
    }
    let mut stereo = Vec::with_capacity(mono.len() * 2);
    for &s in &mono {
        stereo.push(s);
        stereo.push(s);
    }

    let radius_ms = beat * 1000.0 / f64::from(sr) * 0.45;
    for &err_ms in &[20.0_f64, 45.0, 80.0] {
        let err = ((err_ms / 1000.0) * f64::from(sr)).round() as i64;
        let wrong = (true_fd as i64 + err).max(0) as u64;
        let refined = refine_groove_phase(&stereo, sr, wrong, beat, radius_ms);
        let got_ms =
            (refined as i64 - true_fd as i64).unsigned_abs() as f64 * 1000.0 / f64::from(sr);
        assert!(
            got_ms < 10.0,
            "refine from {err_ms}ms error → {got_ms:.2}ms residual (wrong={wrong} true={true_fd})"
        );
        let jump_beats = (refined as i64 - wrong as i64) as f64 / beat;
        assert!(
            jump_beats.abs() < 0.5,
            "groove refine must not jump a whole beat, got {jump_beats:.3}"
        );
    }
}

/// Dense, loud stand-in for a Funkot master: kick on every beat, 16th hats
/// with the on-beat hit accented, and a broadband bed. Thin fixtures do not
/// exercise a phase lock — see HANDOFF §9 ("合成フィクスチャは音が薄すぎて
/// 実音源の問題を検出できない").
fn synth_gridded_stereo(frames: usize, sr: u32, first_beat: u64, beat: f64) -> Vec<f32> {
    let mut state = 0x1234_5678u32;
    let rnd = move |st: &mut u32| -> f32 {
        *st ^= *st << 13;
        *st ^= *st >> 17;
        *st ^= *st << 5;
        (*st as f32 / u32::MAX as f32) * 2.0 - 1.0
    };
    let sr_f = f64::from(sr);
    let mut out = vec![0.0f32; frames * 2];
    for i in 0..frames {
        let s = rnd(&mut state) * 0.12;
        out[i * 2] = s;
        out[i * 2 + 1] = s;
    }
    let mut b = 0u64;
    loop {
        let at = first_beat as f64 + beat * b as f64;
        if at >= frames as f64 {
            break;
        }
        // kick (cosine phase: low-band energy peaks at the attack)
        let klen = (0.20 * sr_f) as usize;
        for j in 0..klen {
            let f = at.round() as usize + j;
            if f >= frames {
                break;
            }
            let t = j as f32 / sr_f as f32;
            let env = (-(j as f64) / (0.08 * sr_f)).exp() as f32;
            let v = 0.9 * env * (2.0 * std::f32::consts::PI * 55.0 * t).cos();
            out[f * 2] += v;
            out[f * 2 + 1] += v;
        }
        // 16th hats, on-beat accented
        for sixteenth in 0..4 {
            let amp = if sixteenth == 0 { 0.6 } else { 0.28 };
            let start = (at + beat * f64::from(sixteenth) / 4.0).round() as usize;
            let hlen = (0.03 * sr_f) as usize;
            for j in 0..hlen {
                let f = start + j;
                if f >= frames {
                    break;
                }
                let env = (-(j as f64) / (0.005 * sr_f)).exp() as f32;
                let v = rnd(&mut state) * amp * env;
                out[f * 2] += v;
                out[f * 2 + 1] += v;
            }
        }
        b += 1;
    }
    for s in out.iter_mut() {
        *s = s.clamp(-1.0, 1.0);
    }
    out
}

/// `lock_beat_phase` exists because a marker counted back from the file end
/// can be up to half a beat off (real case: `AntonFer - … - 02 IVY.flac`,
/// +0.494 beat), which `refine_groove_phase`'s ±0.45-beat window cannot
/// reach. Check both halves of its contract: it recovers a near-half-beat
/// error, and it never moves a marker by half a beat or more (that would
/// change which beat the marker names).
#[test]
fn beat_phase_lock_recovers_near_half_beat_error() {
    let sr = 44_100u32;
    let bpm = 180.0;
    let beat = f64::from(sr) * 60.0 / bpm;
    let n_bars = 8u32;
    let true_beat = (2.0 * beat).round() as u64;
    let frames = true_beat as usize + (f64::from(n_bars * BEATS_PER_BAR + 4) * beat) as usize;
    let stereo = synth_gridded_stereo(frames, sr, true_beat, beat);

    for &off in &[0.0_f64, 0.12, -0.30, 0.47, -0.47] {
        let wrong = (true_beat as i64 + (off * beat).round() as i64).max(0) as u64;
        let locked = lock_beat_phase(&stereo, sr, wrong, beat, n_bars);

        let residual = (locked as i64 - true_beat as i64) as f64 / beat;
        assert!(
            residual.abs() < 0.05,
            "start {off:+.2} beat off → residual {residual:+.4} beat \
             (locked={locked} true={true_beat})"
        );
        let moved = (locked as i64 - wrong as i64) as f64 / beat;
        assert!(
            moved.abs() < 0.5,
            "phase lock must not move a whole beat (bar identity): moved {moved:+.4} beat"
        );
    }
}

#[test]
fn fixture_readme_documents_regen() {
    let readme = fixtures_dir().join("README.md");
    let text = fs::read_to_string(&readme).unwrap_or_default();
    assert!(
        text.contains("--gen-test-fixtures"),
        "{} should document --gen-test-fixtures",
        readme.display()
    );
}
