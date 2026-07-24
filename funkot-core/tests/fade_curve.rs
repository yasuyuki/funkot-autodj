//! Engine-level proof that fade envelopes are linear across the full span
//! (bit-exact 0 / 1 endpoints), and that the previous deck cannot residual-
//! play after `fade_out_end`.

use std::path::PathBuf;
use std::thread;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use funkot_core::engine::{fade_in_gain, fade_out_gain, plan_transition, Engine, EngineEvent};
use funkot_core::testutil::write_wav;
use funkot_core::{EngineOptions, PitchMode, BEATS_PER_BAR, MAIN_GAP_BARS};

fn temp_dir(label: &str) -> PathBuf {
    let mut p = std::env::temp_dir();
    p.push(format!(
        "funkot_fade_curve_{}_{}_{}",
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

/// Constant-amplitude HF tone (plus soft kicks so marker refine can lock).
fn constant_tone_track(
    sr: u32,
    bars: u32,
    bpm: f64,
    freq_hz: f64,
    amp: f32,
    with_kicks: bool,
) -> funkot_core::decode::AudioBuffer {
    use std::f64::consts::PI;
    let beat_frames = (f64::from(sr) * 60.0 / bpm).round() as usize;
    let bar_frames = beat_frames * BEATS_PER_BAR as usize;
    let frames = bars as usize * bar_frames;
    let mut mono = vec![0.0f32; frames];
    if with_kicks {
        let kick_len = ((0.040 * f64::from(sr)) as usize).max(1);
        for beat in 0..(bars as usize * BEATS_PER_BAR as usize) {
            let start = beat * beat_frames;
            let end = (start + kick_len).min(frames);
            for (i, frame) in (start..end).enumerate() {
                let t = i as f64 / f64::from(sr);
                let env = (-t / 0.015).exp() as f32;
                mono[frame] += 0.05 * env * (2.0 * PI * 60.0 * t).sin() as f32;
            }
        }
    }
    if amp > 0.0 {
        for (i, s) in mono.iter_mut().enumerate() {
            let t = i as f64 / f64::from(sr);
            *s += (f64::from(amp) * (2.0 * PI * freq_hz * t).sin()) as f32;
        }
    }
    let mut samples = Vec::with_capacity(frames * 2);
    for &s in &mono {
        samples.push(s);
        samples.push(s);
    }
    funkot_core::decode::AudioBuffer {
        sample_rate: sr,
        frames: frames as u64,
        samples,
    }
}

/// Stereo fixture with tone (+ soft kicks) on exactly one channel.
/// `left_only = true` → L has content, R is silence; otherwise the reverse.
fn channel_tone_track(
    sr: u32,
    bars: u32,
    bpm: f64,
    freq_hz: f64,
    amp: f32,
    left_only: bool,
) -> funkot_core::decode::AudioBuffer {
    use std::f64::consts::PI;
    let beat_frames = (f64::from(sr) * 60.0 / bpm).round() as usize;
    let bar_frames = beat_frames * BEATS_PER_BAR as usize;
    let frames = bars as usize * bar_frames;
    let mut mono = vec![0.0f32; frames];
    let kick_len = ((0.040 * f64::from(sr)) as usize).max(1);
    for beat in 0..(bars as usize * BEATS_PER_BAR as usize) {
        let start = beat * beat_frames;
        let end = (start + kick_len).min(frames);
        for (i, frame) in (start..end).enumerate() {
            let t = i as f64 / f64::from(sr);
            let env = (-t / 0.015).exp() as f32;
            mono[frame] += 0.05 * env * (2.0 * PI * 60.0 * t).sin() as f32;
        }
    }
    for (i, s) in mono.iter_mut().enumerate() {
        let t = i as f64 / f64::from(sr);
        *s += (f64::from(amp) * (2.0 * PI * freq_hz * t).sin()) as f32;
    }
    let mut samples = Vec::with_capacity(frames * 2);
    for &s in &mono {
        if left_only {
            samples.push(s);
            samples.push(0.0);
        } else {
            samples.push(0.0);
            samples.push(s);
        }
    }
    funkot_core::decode::AudioBuffer {
        sample_rate: sr,
        frames: frames as u64,
        samples,
    }
}

fn seed_constant_analysis(
    path: &std::path::Path,
    cache_dir: &std::path::Path,
    buf: &funkot_core::decode::AudioBuffer,
    intro_bars: u32,
    main_bars: u32,
    outro_bars: u32,
    bpm: f64,
) {
    let bar_frames = (f64::from(buf.sample_rate) * 60.0 / bpm * 4.0).round() as u64;
    let outro_start = u64::from(intro_bars + main_bars) * bar_frames;
    let hash = funkot_core::cache::content_hash(path).expect("hash");
    let analysis = funkot_core::TrackAnalysis {
        version: funkot_core::cache::CACHE_VERSION,
        file_name: path
            .file_name()
            .and_then(|s| s.to_str())
            .unwrap_or("tone.wav")
            .to_string(),
        sample_rate: buf.sample_rate,
        total_frames: buf.frames,
        intro_bpm: bpm,
        outro_bpm: bpm,
        first_downbeat: 0,
        outro_start,
        intro_bars,
        outro_bars,
        bars_estimated_low_confidence: false,
        intro_bars_low_confidence: false,
        outro_bars_low_confidence: false,
        intro_bars_manual: false,
        outro_bars_manual: false,
        needs_reanalysis: false,
        rms_dbfs: -6.0,
        gain_db: 0.0,
    };
    funkot_core::cache::store(cache_dir, &hash, &analysis).expect("store cache");
}

fn mono_mix(interleaved: &[f32]) -> Vec<f32> {
    let n = interleaved.len() / 2;
    let mut m = Vec::with_capacity(n);
    for i in 0..n {
        m.push(0.5 * (interleaved[i * 2] + interleaved[i * 2 + 1]));
    }
    m
}

fn channel(interleaved: &[f32], right: bool) -> Vec<f32> {
    let n = interleaved.len() / 2;
    let mut c = Vec::with_capacity(n);
    let off = if right { 1 } else { 0 };
    for i in 0..n {
        c.push(interleaved[i * 2 + off]);
    }
    c
}

fn window_rms(mono: &[f32], center: usize, radius: usize) -> f32 {
    let lo = center.saturating_sub(radius);
    let hi = (center + radius + 1).min(mono.len());
    if hi <= lo {
        return 0.0;
    }
    let mut e = 0.0f64;
    for &s in &mono[lo..hi] {
        e += f64::from(s) * f64::from(s);
    }
    (e / (hi - lo) as f64).sqrt() as f32
}

/// Record every frame after the first track starts (including digital silence)
/// so TransitionStarted aligns even when the previous track is near-silent.
///
/// Transition start is sample-accurate: when `TransitionStarted` appears after a
/// multi-frame `render`, [`Engine::transition_frames_into`] recovers the offset
/// within that chunk.
fn render_with_transition_mark(
    engine: &mut Engine,
    chunk_frames: usize,
    pause_after_first_audio: Duration,
) -> (Vec<f32>, usize) {
    let mut out = Vec::new();
    let mut buf = vec![0.0f32; chunk_frames * 2];
    let mut recording = false;
    let mut paused = pause_after_first_audio.is_zero();
    let mut spins = 0u64;
    let mut transition_at: Option<usize> = None;
    loop {
        for e in engine.poll_events() {
            match e {
                EngineEvent::TrackStarted { .. } if !recording => {
                    recording = true;
                }
                EngineEvent::TransitionStarted { .. } if transition_at.is_none() => {
                    // Should have been attributed during the render below; keep
                    // as a fallback at current write position.
                    transition_at = Some(out.len() / 2);
                }
                _ => {}
            }
        }
        let n = engine.render(&mut buf);
        if n == 0 {
            break;
        }
        let before = out.len() / 2;
        let events = engine.poll_events();
        for e in &events {
            match e {
                EngineEvent::TrackStarted { .. } if !recording => {
                    recording = true;
                }
                EngineEvent::TransitionStarted { .. } if transition_at.is_none() => {
                    // `frames_into` has already been advanced once per mixed
                    // transition frame in this chunk.
                    let into = engine.transition_frames_into().unwrap_or(n as u64);
                    let into = into.min(n as u64);
                    transition_at = Some(before + n - into as usize);
                }
                _ => {}
            }
        }
        let chunk = &buf[..n * 2];
        if !recording {
            let silent = chunk.iter().all(|s| s.abs() < 1e-8);
            if silent {
                spins += 1;
                if spins > 10_000_000 {
                    panic!("timed out waiting for first track");
                }
                continue;
            }
            recording = true;
        }
        if recording && !paused {
            thread::sleep(pause_after_first_audio);
            paused = true;
        }
        out.extend_from_slice(chunk);
        if out.len() > 50_000_000 {
            panic!("render produced unexpectedly huge output");
        }
    }
    for e in engine.poll_events() {
        if matches!(e, EngineEvent::TransitionStarted { .. }) && transition_at.is_none() {
            transition_at = Some(out.len() / 2);
        }
    }
    let t = transition_at.expect("expected TransitionStarted");
    (out, t)
}

#[test]
fn linear_fade_full_span_constant_signal() {
    let dir = temp_dir("main");
    let cache = dir.join("cache");
    let path_a = dir.join("a.wav");
    let path_b = dir.join("b.wav");

    let sr = 44_100u32;
    let bpm = 180.0;
    // Compact schedule: fade-out starts when fade-in / HPF switch completes.
    let intro = 32u32;
    let main = 16u32;
    let outro = 32u32;
    let total_bars = intro + main + outro;
    let tone_a = constant_tone_track(sr, total_bars, bpm, 2000.0, 0.5, true);
    let silent_b = constant_tone_track(sr, total_bars, bpm, 2000.0, 0.0, true);
    let silent_a = constant_tone_track(sr, total_bars, bpm, 2000.0, 0.0, true);
    let tone_b = constant_tone_track(sr, total_bars, bpm, 2000.0, 0.5, true);

    let options_base = EngineOptions {
        rate: 1.0,
        pitch_mode: PitchMode::Preserve,
        fade_bars: 4,
        highpass_hz: 300.0,
        gain_normalize: false,
        random: false,
        loop_playlist: false,
        output_sample_rate: sr,
        cache_dir: cache.clone(),
    };
    let plan = plan_transition(4, intro, outro);
    assert_eq!(plan.f_eff, 4, "fade must remain 4 bars");
    assert_eq!(MAIN_GAP_BARS, 8);
    assert_eq!(
        plan.fadeout_start, plan.f_eff,
        "fade-out starts when fade-in/HPF switch completes: {plan:?}"
    );
    let bar_frames = options_base.bar_frames();
    let fade_n = (f64::from(plan.f_eff) * bar_frames).round() as u64;
    let fo_start = (f64::from(plan.fadeout_start) * bar_frames).round() as u64;
    let fo_end = (f64::from(plan.fadeout_end) * bar_frames).round() as u64;
    let fo_span = fo_end.saturating_sub(fo_start);
    assert!(fade_n > 1 && fo_span > 1);

    let quarters = [0.0f64, 0.25, 0.5, 0.75, 1.0];

    // --- Fade-in: kick-only prev + tone next (prev RMS << next tone) ---
    write_wav(&path_a, &silent_a).expect("write a");
    write_wav(&path_b, &tone_b).expect("write b");
    seed_constant_analysis(&path_a, &cache, &silent_a, intro, main, outro, bpm);
    seed_constant_analysis(&path_b, &cache, &tone_b, intro, main, outro, bpm);

    let mut engine = Engine::new(
        EngineOptions {
            cache_dir: cache.clone(),
            ..options_base.clone()
        },
        vec![path_a.clone(), path_b.clone()],
    )
    .expect("engine");
    let (mixed_raw, t_frame) =
        render_with_transition_mark(&mut engine, 4096, Duration::from_secs(180));
    let mono = mono_mix(&mixed_raw);
    let ref_rms = window_rms(
        &mono,
        t_frame + ((plan.f_eff + 4) as f64 * bar_frames).round() as usize,
        (bar_frames * 0.25).round() as usize,
    );
    assert!(
        ref_rms > 0.05,
        "expected audible reference RMS, got {ref_rms}"
    );

    for &q in &quarters {
        let i = if q >= 1.0 {
            fade_n - 1
        } else {
            ((q * (fade_n - 1) as f64).round() as u64).min(fade_n - 1)
        };
        let expect = fade_in_gain(i, fade_n);
        let center = t_frame + i as usize;
        let got = window_rms(&mono, center, 256) / ref_rms;
        assert!(
            (got - expect).abs() < 0.18,
            "fade-in q={q}: gain≈{got:.3} expect≈{expect:.3} (i={i}/{fade_n} t={t_frame})"
        );
        if q == 0.0 {
            assert!(got < 0.12, "fade-in must start near 0, got {got}");
        }
        if q == 1.0 {
            assert!(got > 0.82, "fade-in must reach ~1 by last frame, got {got}");
        }
        if (0.4..0.6).contains(&q) {
            // Linear midpoint is 0.5.
            assert!(
                got > 0.35 && got < 0.65,
                "fade-in midpoint must be ~0.5, got {got}"
            );
        }
    }
    engine.stop();

    // --- Fade-out: tone prev + kick-only next ---
    let cache2 = dir.join("cache2");
    write_wav(&path_a, &tone_a).expect("write a tone");
    write_wav(&path_b, &silent_b).expect("write b silent");
    seed_constant_analysis(&path_a, &cache2, &tone_a, intro, main, outro, bpm);
    seed_constant_analysis(&path_b, &cache2, &silent_b, intro, main, outro, bpm);
    let mut engine2 = Engine::new(
        EngineOptions {
            cache_dir: cache2,
            ..options_base
        },
        vec![path_a, path_b],
    )
    .expect("engine2");
    let (mixed2_raw, t2) =
        render_with_transition_mark(&mut engine2, 4096, Duration::from_secs(180));
    let mono2 = mono_mix(&mixed2_raw);
    // Reference after HPF has switched to prev (past fade-in) but before fade-out.
    let ref_out = window_rms(
        &mono2,
        t2 + (((plan.f_eff + plan.fadeout_start) / 2) as f64 * bar_frames).round() as usize,
        (bar_frames * 0.25).round() as usize,
    );
    assert!(ref_out > 0.05, "expected audible prev RMS, got {ref_out}");

    for &q in &quarters {
        let i = if q >= 1.0 {
            fo_span - 1
        } else {
            ((q * (fo_span - 1) as f64).round() as u64).min(fo_span - 1)
        };
        let expect = fade_out_gain(i, fo_span);
        let center = t2 + fo_start as usize + i as usize;
        let got = window_rms(&mono2, center, 256) / ref_out;
        assert!(
            (got - expect).abs() < 0.18,
            "fade-out q={q}: gain≈{got:.3} expect≈{expect:.3} (i={i}/{fo_span} t={t2})"
        );
        if q == 0.0 {
            assert!(got > 0.82, "fade-out must start near 1, got {got}");
        }
        if q == 1.0 {
            assert!(
                got < 0.12,
                "fade-out must reach ~0 by last frame, got {got}"
            );
        }
    }
    engine2.stop();
    let _ = std::fs::remove_dir_all(&dir);
}

/// Previous deck = L-only unique tone; next deck = R-only different tone.
/// Proves linear fade-out shape on L, zero next contribution at first
/// transition frame on R, and hard stop of prev after `fade_out_end`.
#[test]
fn prev_deck_hard_stop_no_residual_after_fade_out() {
    let dir = temp_dir("residual");
    let cache = dir.join("cache");
    let path_a = dir.join("prev_l.wav");
    let path_b = dir.join("next_r.wav");

    let sr = 44_100u32;
    let bpm = 180.0;
    let intro = 32u32;
    let main = 16u32;
    let outro = 32u32;
    let total_bars = intro + main + outro;

    // Distinguishable sources: prev L @ 2 kHz, next R @ 3.5 kHz (both above HPF).
    let prev = channel_tone_track(sr, total_bars, bpm, 2000.0, 0.55, true);
    let next = channel_tone_track(sr, total_bars, bpm, 3500.0, 0.55, false);

    write_wav(&path_a, &prev).expect("write prev");
    write_wav(&path_b, &next).expect("write next");
    seed_constant_analysis(&path_a, &cache, &prev, intro, main, outro, bpm);
    seed_constant_analysis(&path_b, &cache, &next, intro, main, outro, bpm);

    let options = EngineOptions {
        rate: 1.0,
        pitch_mode: PitchMode::Preserve,
        fade_bars: 4,
        highpass_hz: 300.0,
        gain_normalize: false,
        random: false,
        loop_playlist: false,
        output_sample_rate: sr,
        cache_dir: cache,
    };
    let plan = plan_transition(4, intro, outro);
    assert_eq!(plan.f_eff, 4);
    assert_eq!(MAIN_GAP_BARS, 8);
    let bar_frames = options.bar_frames();
    let fo_start = (f64::from(plan.fadeout_start) * bar_frames).round() as u64;
    let fo_end = (f64::from(plan.fadeout_end) * bar_frames).round() as u64;
    let fo_span = fo_end.saturating_sub(fo_start);
    assert!(fo_span > 1);

    let mut engine = Engine::new(options, vec![path_a, path_b]).expect("engine");
    let (mixed, t_frame) =
        render_with_transition_mark(&mut engine, 4096, Duration::from_secs(180));
    let left = channel(&mixed, false);
    let right = channel(&mixed, true);

    // At transition first frame: next-only (R) must be ~0 (fade-in gain bit-exact 0).
    let r0 = right[t_frame].abs();
    assert!(
        r0 <= 1e-6,
        "next-only channel at first transition frame must be ~0, got {r0}"
    );

    // Fade-out reference: L after HPF switch, before fade-out.
    let ref_l = window_rms(
        &left,
        t_frame + (((plan.f_eff + plan.fadeout_start) / 2) as f64 * bar_frames).round() as usize,
        (bar_frames * 0.25).round() as usize,
    );
    assert!(ref_l > 0.05, "expected audible prev L RMS, got {ref_l}");

    let quarters = [0.0f64, 0.25, 0.5, 0.75, 1.0];
    for &q in &quarters {
        let i = if q >= 1.0 {
            fo_span - 1
        } else {
            ((q * (fo_span - 1) as f64).round() as u64).min(fo_span - 1)
        };
        let expect = fade_out_gain(i, fo_span);
        let center = t_frame + fo_start as usize + i as usize;
        let got = window_rms(&left, center, 256) / ref_l;
        assert!(
            (got - expect).abs() < 0.18,
            "fade-out L q={q}: gain≈{got:.3} expect≈{expect:.3} (i={i}/{fo_span})"
        );
    }

    // After fade_out_end: prev-only channel (L) must be zero (numerical eps)
    // for ≥2 bars, while next (R) continues. Stretch of a silent channel can
    // leave ~1e-7 denoise; anything larger would be residual playback.
    let after = t_frame + fo_end as usize;
    let check_len = (2.0 * bar_frames).round() as usize;
    assert!(
        after + check_len < left.len(),
        "need ≥2 bars after fade_out_end"
    );
    let mut max_l = 0.0f32;
    let mut max_r = 0.0f32;
    for i in after..(after + check_len) {
        max_l = max_l.max(left[i].abs());
        max_r = max_r.max(right[i].abs());
    }
    const RESIDUAL_EPS: f32 = 1e-6;
    assert!(
        max_l <= RESIDUAL_EPS,
        "prev-only L must be zero after fade_out_end, max|L|={max_l}"
    );
    assert!(
        max_r > 0.05,
        "next-only R must continue after fade_out_end, max|R|={max_r}"
    );

    // Final fade-out frame itself must be silent on prev channel (gain bit-exact 0).
    let last_fo = t_frame + fo_end as usize - 1;
    assert!(
        left[last_fo].abs() <= RESIDUAL_EPS,
        "last fade-out frame L must be 0, got {}",
        left[last_fo]
    );

    engine.stop();
    let _ = std::fs::remove_dir_all(&dir);
}
