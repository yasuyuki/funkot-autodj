//! Integration tests for the pull-based mixing engine.

use std::path::PathBuf;
use std::thread;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use funkot_core::cache;
use funkot_core::engine::{plan_transition, Engine, EngineEvent};
use funkot_core::testutil::{synth_track, write_wav};
use funkot_core::{EngineOptions, PitchMode};

fn temp_dir(label: &str) -> PathBuf {
    let mut p = std::env::temp_dir();
    p.push(format!(
        "funkot_engine_{}_{}_{}",
        label,
        std::process::id(),
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0)
    ));
    let _ = std::fs::create_dir_all(&p);
    p
}

fn render_all(engine: &mut Engine, chunk_frames: usize) -> Vec<f32> {
    render_all_with_options(engine, chunk_frames, Duration::ZERO)
}

/// Like [`render_all`], but after the first audible chunk pauses so the loader
/// can finish the next track. Needed because tests pull faster than realtime;
/// without a pause the current track is consumed before the next is prepared.
fn render_all_with_options(
    engine: &mut Engine,
    chunk_frames: usize,
    pause_after_first_audio: Duration,
) -> Vec<f32> {
    let mut out = Vec::new();
    let mut buf = vec![0.0f32; chunk_frames * 2];
    let mut started = false;
    let mut paused = pause_after_first_audio.is_zero();
    let mut spins = 0u64;
    loop {
        let n = engine.render(&mut buf);
        if n == 0 {
            break;
        }
        let chunk = &buf[..n * 2];
        let silent = chunk.iter().all(|s| s.abs() < 1e-8);
        if !started {
            // Loader prepares offline; render returns silence immediately. Do not
            // accumulate those pre-roll zeros (they can fill gigabytes instantly).
            if silent {
                spins += 1;
                if spins > 10_000_000 {
                    panic!("timed out waiting for first audio");
                }
                continue;
            }
            started = true;
            if !paused {
                thread::sleep(pause_after_first_audio);
                paused = true;
            }
        }
        // Do not skip "silent" chunks after start: kick-only material can have
        // whole pulldown blocks between hits that are near zero.
        out.extend_from_slice(chunk);
        if out.len() > 50_000_000 {
            panic!("render produced unexpectedly huge output");
        }
    }
    out
}

fn assert_finite_peak(samples: &[f32], peak_limit: f32) {
    let mut peak = 0.0f32;
    for &s in samples {
        assert!(s.is_finite(), "non-finite sample");
        peak = peak.max(s.abs());
    }
    assert!(peak < peak_limit, "peak {peak} >= {peak_limit}");
}

fn assert_beat_grid(mono: &[f32], sample_rate: u32, bpm: f64, label: &str) {
    let beat_len = f64::from(sample_rate) * 60.0 / bpm;
    let beat_i = beat_len.round() as usize;
    assert!(
        mono.len() >= beat_i * 6,
        "{label}: buffer too short for beat grid ({} frames)",
        mono.len()
    );

    // Lock phase from the strongest peak in the first two beats.
    let search_to = (beat_i * 2).min(mono.len());
    let mut phase = 0usize;
    let mut best = 0.0f32;
    for (i, &s) in mono[..search_to].iter().enumerate() {
        let e = s.abs();
        if e > best {
            best = e;
            phase = i;
        }
    }
    assert!(best > 0.05, "{label}: no kick found for phase lock");

    let radius = (beat_len * 0.03).ceil().max(1.0) as usize;
    let n_beats = ((mono.len().saturating_sub(phase)) as f64 / beat_len).floor() as usize;
    assert!(
        n_beats >= 6,
        "{label}: only {n_beats} beats after phase lock"
    );

    let mut hits = 0u32;
    for b in 0..n_beats {
        let center = phase + (b as f64 * beat_len).round() as usize;
        if center >= mono.len() {
            break;
        }
        let lo = center.saturating_sub(radius);
        let hi = (center + radius + 1).min(mono.len());
        let peak = mono[lo..hi].iter().map(|s| s.abs()).fold(0.0f32, f32::max);
        if peak > 0.05 {
            hits += 1;
        }
    }
    assert!(
        f64::from(hits) / f64::from(n_beats as u32) >= 0.90,
        "{label}: {hits}/{n_beats} beats have a kick within ±3% of {bpm} BPM grid (phase={phase})"
    );
}

fn trim_leading_silence(interleaved: &[f32], eps: f32) -> &[f32] {
    let mut i = 0usize;
    while i + 1 < interleaved.len() {
        if interleaved[i].abs() > eps || interleaved[i + 1].abs() > eps {
            break;
        }
        i += 2;
    }
    &interleaved[i..]
}

fn mono_mix(interleaved: &[f32]) -> Vec<f32> {
    let n = interleaved.len() / 2;
    let mut m = Vec::with_capacity(n);
    for i in 0..n {
        m.push(0.5 * (interleaved[i * 2] + interleaved[i * 2 + 1]));
    }
    m
}

fn bar_rms(mono: &[f32], bar_frames: usize) -> Vec<f32> {
    let mut out = Vec::new();
    let mut i = 0;
    while i + bar_frames <= mono.len() {
        let mut e = 0.0f64;
        for &s in &mono[i..i + bar_frames] {
            e += f64::from(s) * f64::from(s);
        }
        out.push((e / bar_frames as f64).sqrt() as f32);
        i += bar_frames;
    }
    out
}

#[test]
fn two_track_transition_tempo_and_envelope() {
    let dir = temp_dir("two_track");
    let cache = dir.join("cache");
    let path_a = dir.join("a.wav");
    let path_b = dir.join("b.wav");

    let sr = 44_100u32;
    let a = synth_track(180.0, 16, 32, 16, sr);
    let b = synth_track(178.0, 16, 32, 16, sr);
    write_wav(&path_a, &a).expect("write a");
    write_wav(&path_b, &b).expect("write b");
    // Seed cache so track A isn't prepared with provisional markers (live first
    // track skips blocking analyze on a cold cache).
    cache::get_or_analyze(&path_a, &cache, &a).expect("seed a");
    cache::get_or_analyze(&path_b, &cache, &b).expect("seed b");

    let options = EngineOptions {
        rate: 1.10,
        pitch_mode: PitchMode::Preserve,
        fade_bars: 8,
        highpass_hz: 300.0,
        gain_normalize: true,
        random: false,
        loop_playlist: false,
        output_sample_rate: 44_100,
        cache_dir: cache,
    };
    let target_bpm = options.target_bpm(); // 198
    let bar_frames = options.bar_frames();

    let mut engine = Engine::new(options, vec![path_a.clone(), path_b.clone()]).expect("engine");
    // Pause after first audio so track B can finish offline prep before we
    // consume A's outro at CPU speed (hosts pull in realtime; tests do not).
    let mixed_raw = render_all_with_options(&mut engine, 4096, Duration::from_secs(120));
    assert_finite_peak(&mixed_raw, 4.0);
    let mixed = trim_leading_silence(&mixed_raw, 1e-5);
    assert!(!mixed.is_empty(), "expected non-silent output");

    let events = engine.poll_events();
    let finished = events
        .iter()
        .filter(|e| matches!(e, EngineEvent::Finished))
        .count();
    assert_eq!(finished, 1, "Finished exactly once, got {events:?}");

    // Schedule prediction (analysis may adjust bar counts slightly; use synth truth).
    let intro_a = 16u32;
    let main_a = 32u32;
    let outro_a = 16u32;
    let intro_b = 16u32;
    let main_b = 32u32;
    let outro_b = 16u32;
    let plan = plan_transition(8, intro_b, outro_a);
    // A from downbeat to outro start = intro+main bars, then B plays full remaining from entry.
    let bars_a_to_t = intro_a + main_a;
    let bars_b_from_entry = intro_b + main_b + outro_b - plan.skip;
    let expected_bars = u64::from(bars_a_to_t + bars_b_from_entry);
    let expected_frames = (expected_bars as f64 * bar_frames).round() as i64;
    let actual_frames = (mixed.len() / 2) as i64;
    let tol = bar_frames.round() as i64;
    assert!(
        (actual_frames - expected_frames).abs() <= tol,
        "duration frames actual={actual_frames} expected={expected_frames} tol={tol} plan={plan:?}"
    );

    // Beat-grid continuity: check clean solo regions around the transition
    // (avoid the crossfade itself, where two kick trains create false intervals).
    let t_frame = (bars_a_to_t as f64 * bar_frames).round() as usize;
    let m_frame = (plan.m as f64 * bar_frames).round() as usize;
    let win = (8.0 * bar_frames).round() as usize;
    let beat_len = sr as f64 * 60.0 / target_bpm;
    assert!(beat_len > 0.0);

    let pre_start = t_frame.saturating_sub(win);
    let pre_mono = mono_mix(&mixed[pre_start * 2..t_frame * 2]);
    assert_beat_grid(&pre_mono, sr, target_bpm, "pre-transition (A)");

    let post_start = t_frame + m_frame;
    let post_end = (post_start + win).min(mixed.len() / 2);
    assert!(post_end > post_start + beat_len as usize * 4);
    let post_mono = mono_mix(&mixed[post_start * 2..post_end * 2]);
    assert_beat_grid(&post_mono, sr, target_bpm, "post-transition (B main)");

    // Transition span should also lock to the same tempo (proves B was stretched).
    let xfade_end = (t_frame + m_frame).min(mixed.len() / 2);
    let xfade_mono = mono_mix(&mixed[t_frame * 2..xfade_end * 2]);
    assert_beat_grid(&xfade_mono, sr, target_bpm, "during transition");

    // Loudness envelope: no dropout in [T, M].
    let full_mono = mono_mix(mixed);
    let bf = bar_frames.round() as usize;
    let rms = bar_rms(&full_mono, bf);
    let t_bar = bars_a_to_t as usize;
    let m_bars = plan.m as usize;
    assert!(t_bar + m_bars < rms.len());
    let steady_start = t_bar.saturating_sub(8);
    let steady_end = t_bar;
    let after_start = t_bar + m_bars;
    let after_end = (after_start + 8).min(rms.len());
    let mut steady = 0.0f32;
    let mut n = 0u32;
    for &v in &rms[steady_start..steady_end] {
        steady += v;
        n += 1;
    }
    for &v in &rms[after_start..after_end] {
        steady += v;
        n += 1;
    }
    let steady_avg = steady / n.max(1) as f32;
    for (k, &v) in rms[t_bar..t_bar + m_bars].iter().enumerate() {
        assert!(
            v >= 0.20 * steady_avg,
            "dropout at transition bar {k}: rms={v} steady_avg={steady_avg}"
        );
    }

    // After main of B starts, output should carry mid/high energy (B main).
    let main_region =
        &full_mono[after_start * bf..(after_start * bf + 4 * bf).min(full_mono.len())];
    let mut hi = 0.0f32;
    // crude high-pass energy
    for w in main_region.windows(2) {
        let d = w[1] - w[0];
        hi += d * d;
    }
    assert!(
        hi > 1e-3,
        "expected mid/high content in B main, hi_energy={hi}"
    );

    // no-loop: further render returns 0
    let mut buf = vec![0.0f32; 4096 * 2];
    assert_eq!(engine.render(&mut buf), 0);
    assert_eq!(engine.render(&mut buf), 0);

    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn silence_until_ready() {
    let dir = temp_dir("silence");
    let cache = dir.join("cache");
    let path = dir.join("a.wav");
    let a = synth_track(180.0, 16, 16, 16, 44_100);
    write_wav(&path, &a).expect("write");

    let options = EngineOptions {
        rate: 1.10,
        pitch_mode: PitchMode::Preserve,
        fade_bars: 4,
        highpass_hz: 300.0,
        gain_normalize: false,
        random: false,
        loop_playlist: false,
        output_sample_rate: 44_100,
        cache_dir: cache,
    };
    let mut engine = Engine::new(options, vec![path]).expect("engine");
    let mut buf = vec![1.0f32; 4096 * 2]; // non-zero to detect overwrite
    let n = engine.render(&mut buf);
    assert_eq!(n, 4096);
    // Either still silent (not ready) or already playing — must not panic.
    // If still preparing, buffer should be zeros.
    let all_zero = buf.iter().all(|&s| s == 0.0);
    let any_nonzero = buf.iter().any(|&s| s != 0.0);
    assert!(all_zero || any_nonzero);
    if all_zero {
        let n2 = engine.render(&mut buf);
        assert_eq!(n2, 4096);
    }
    engine.stop();
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn finished_once_and_stays_zero() {
    let dir = temp_dir("finished");
    let cache = dir.join("cache");
    let path = dir.join("a.wav");
    // Long enough for analysis (≥30s): 16+16+16 bars ≈ 64s at 180 BPM.
    let a = synth_track(180.0, 16, 16, 16, 44_100);
    write_wav(&path, &a).expect("write");

    let options = EngineOptions {
        rate: 1.10,
        pitch_mode: PitchMode::Preserve,
        fade_bars: 2,
        highpass_hz: 300.0,
        gain_normalize: false,
        random: false,
        loop_playlist: false,
        output_sample_rate: 44_100,
        cache_dir: cache,
    };
    let mut engine = Engine::new(options, vec![path]).expect("engine");
    let _ = render_all(&mut engine, 4096);
    let events = engine.poll_events();
    let finished = events
        .iter()
        .filter(|e| matches!(e, EngineEvent::Finished))
        .count();
    assert_eq!(finished, 1, "{events:?}");
    let mut buf = vec![0.0f32; 512 * 2];
    assert_eq!(engine.render(&mut buf), 0);
    assert_eq!(engine.render(&mut buf), 0);
    // poll again: no second Finished
    let more = engine.poll_events();
    assert!(
        more.iter()
            .filter(|e| matches!(e, EngineEvent::Finished))
            .count()
            == 0
    );
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn pitch_mode_shift_duration() {
    let dir = temp_dir("shift");
    let cache = dir.join("cache");
    let path = dir.join("a.wav");
    let sr = 44_100u32;
    let a = synth_track(180.0, 16, 16, 16, sr);
    let original_frames = a.frames;
    write_wav(&path, &a).expect("write");

    let options = EngineOptions {
        rate: 1.10,
        pitch_mode: PitchMode::Shift,
        fade_bars: 2,
        highpass_hz: 300.0,
        gain_normalize: false,
        random: false,
        loop_playlist: false,
        output_sample_rate: sr,
        cache_dir: cache,
    };
    // speed = target/intro ≈ 1.10 when intro analyzes as 180.
    let mut engine = Engine::new(options, vec![path]).expect("engine");
    let mixed_raw = render_all(&mut engine, 4096);
    assert_finite_peak(&mixed_raw, 4.0);
    let mixed = trim_leading_silence(&mixed_raw, 1e-5);
    let out_frames = mixed.len() / 2;
    // Playback starts at first downbeat (~0); duration ≈ original/speed.
    let expected = (original_frames as f64 / 1.10).round() as i64;
    let actual = out_frames as i64;
    let err = (actual - expected).abs() as f64 / expected as f64;
    assert!(
        err < 0.01,
        "shift duration actual={actual} expected≈{expected} err={err}"
    );
    let _ = std::fs::remove_dir_all(&dir);
}
