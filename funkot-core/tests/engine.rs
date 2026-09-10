//! Integration tests for the pull-based mixing engine.

use std::path::PathBuf;
use std::sync::{Mutex, MutexGuard, OnceLock};
use std::time::{SystemTime, UNIX_EPOCH};

use funkot_core::engine::{
    align_next_entry_with_phase_hypotheses, plan_transition, prepare_tracks_parallel, Engine,
    EngineEvent,
};
use funkot_core::testutil::{synth_track, write_wav};
use funkot_core::{EngineOptions, PitchMode, BEATS_PER_BAR};

/// Serialize this binary: `DECODE_FILE_CALLS` is process-global and races under
/// parallel engine tests (CI saw 4–5 vs ≤3 on the permit-spin regression).
fn engine_test_lock() -> MutexGuard<'static, ()> {
    static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
    LOCK.get_or_init(|| Mutex::new(()))
        .lock()
        .unwrap_or_else(|e| e.into_inner())
}

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
    let mut out = Vec::new();
    let mut buf = vec![0.0f32; chunk_frames * 2];
    let mut started = false;
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
    let _lock = engine_test_lock();
    let dir = temp_dir("two_track");
    let cache = dir.join("cache");
    let path_a = dir.join("a.wav");
    let path_b = dir.join("b.wav");

    let sr = 44_100u32;
    let a = synth_track(180.0, 16, 32, 16, sr);
    let b = synth_track(178.0, 16, 32, 16, sr);
    write_wav(&path_a, &a).expect("write a");
    write_wav(&path_b, &b).expect("write b");

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
        head_only_secs: None,
    };
    let target_bpm = options.target_bpm(); // 198
    let bar_frames = options.bar_frames();

    // Pre-prepare both tracks so this asserts mix math, not loader timing.
    // Engine::new + sleep was flaky under CI load (next track late → wrong duration).
    // jobs=1: avoid any cross-platform scheduling noise while preparing.
    let tracks =
        prepare_tracks_parallel(&options, &[path_a.clone(), path_b.clone()], 1).expect("prepare");
    assert_eq!(tracks.len(), 2);
    let a_fd = tracks[0].first_downbeat_out;
    let a_outro = tracks[0].outro_start_out;
    let a_outro_end = tracks[0].outro_end_anchored_out;
    let a_outro_bars = tracks[0].outro_bars;
    let b_fd = tracks[1].first_downbeat_out;
    let b_frames = tracks[1].frames;
    let b_intro = tracks[1].intro_bars;
    let plan = plan_transition(8, b_intro, a_outro_bars);
    let skip_frames = (f64::from(plan.skip) * bar_frames).round() as u64;
    let nominal_entry = b_fd.saturating_add(skip_frames);
    let beat_frames = bar_frames / f64::from(BEATS_PER_BAR);
    // Same phase-align the mixer runs at transition start. Scores can differ
    // across hosts (libm/SIMD), so expected duration must use this entry —
    // not nominal — or CI drifts by whole bars while local stays green.
    let (entry, _nudge) = align_next_entry_with_phase_hypotheses(
        &tracks[0].samples,
        a_outro,
        &tracks[1].samples,
        nominal_entry,
        a_outro,
        a_outro_end,
        sr,
        beat_frames,
    );
    let frames_a_to_t = a_outro.saturating_sub(a_fd);
    let frames_b_from_entry = b_frames.saturating_sub(entry);
    let expected_frames = (frames_a_to_t + frames_b_from_entry) as i64;

    let mut engine = Engine::from_prepared(options, tracks).expect("engine");
    let mixed_raw = render_all(&mut engine, 4096);
    assert_finite_peak(&mixed_raw, 4.0);
    let mixed = trim_leading_silence(&mixed_raw, 1e-5);
    assert!(!mixed.is_empty(), "expected non-silent output");

    let events = engine.poll_events();
    let finished = events
        .iter()
        .filter(|e| matches!(e, EngineEvent::Finished))
        .count();
    assert_eq!(finished, 1, "Finished exactly once, got {events:?}");

    let actual_frames = (mixed.len() / 2) as i64;
    // Micro phase refine can still move a few frames vs the pre-mix call.
    let tol = (beat_frames * 2.0).ceil() as i64;
    assert!(
        (actual_frames - expected_frames).abs() <= tol,
        "duration frames actual={actual_frames} expected={expected_frames} tol={tol} \
         plan={plan:?} entry={entry} nominal={nominal_entry} \
         a_outro_bars={a_outro_bars} b_intro={b_intro} \
         a_outro={a_outro} a_outro_end={a_outro_end}"
    );

    // Beat-grid continuity: check clean solo regions around the transition
    // (avoid the crossfade itself, where two kick trains create false intervals).
    let t_frame = frames_a_to_t as usize;
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
    let t_bar = (t_frame as f64 / bar_frames).round() as usize;
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
    let _lock = engine_test_lock();
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
        head_only_secs: None,
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
    let _lock = engine_test_lock();
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
        head_only_secs: None,
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
    let _lock = engine_test_lock();
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
        head_only_secs: None,
    };
    // Pre-prepare: Engine::new first-live preview/upgrade raced under CI load
    // (duration ≈2× expected). Assert Shift math, not loader timing.
    // speed = target/intro ≈ 1.10 when intro analyzes as 180.
    let tracks = prepare_tracks_parallel(&options, &[path], 1).expect("prepare");
    assert_eq!(tracks.len(), 1);
    let expected = (original_frames as f64 / 1.10).round() as i64;
    let prepared = tracks[0].frames as i64;
    let prep_err = (prepared - expected).abs() as f64 / expected as f64;
    assert!(
        prep_err < 0.01,
        "shift prepare frames={prepared} expected≈{expected} err={prep_err}"
    );

    let mut engine = Engine::from_prepared(options, tracks).expect("engine");
    let mixed_raw = render_all(&mut engine, 4096);
    assert_finite_peak(&mixed_raw, 4.0);
    let mixed = trim_leading_silence(&mixed_raw, 1e-5);
    let out_frames = mixed.len() / 2;
    // Playback starts at first downbeat (~0); duration ≈ original/speed.
    let actual = out_frames as i64;
    let err = (actual - expected).abs() as f64 / expected as f64;
    assert!(
        err < 0.01,
        "shift duration actual={actual} expected≈{expected} err={err}"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

fn engine_opts(cache: PathBuf) -> EngineOptions {
    EngineOptions {
        rate: 1.10,
        pitch_mode: PitchMode::Preserve,
        fade_bars: 4,
        highpass_hz: 300.0,
        gain_normalize: false,
        random: false,
        loop_playlist: false,
        output_sample_rate: 44_100,
        cache_dir: cache,
        head_only_secs: None,
    }
}

fn render_until_playing(engine: &mut Engine, chunk: usize) {
    let mut buf = vec![0.0f32; chunk * 2];
    for _ in 0..50_000 {
        let n = engine.render(&mut buf);
        assert!(n > 0);
        if buf.iter().any(|s| s.abs() > 1e-5) {
            return;
        }
        std::thread::sleep(std::time::Duration::from_millis(1));
    }
    panic!("timed out waiting for audio");
}

#[test]
fn nav_jump_next_intro_and_restart() {
    let _lock = engine_test_lock();
    use funkot_core::engine::NavAction;

    let dir = temp_dir("nav_jump");
    let cache = dir.join("cache");
    let path_a = dir.join("a.wav");
    let path_b = dir.join("b.wav");
    let sr = 44_100u32;
    write_wav(&path_a, &synth_track(180.0, 16, 32, 16, sr)).unwrap();
    write_wav(&path_b, &synth_track(180.0, 16, 32, 16, sr)).unwrap();

    let options = engine_opts(cache);
    let bar_frames = options.bar_frames();
    let tracks =
        prepare_tracks_parallel(&options, &[path_a.clone(), path_b.clone()], 1).expect("prepare");
    let b_fd = tracks[1].first_downbeat_out;
    let mut engine = Engine::from_prepared(options, tracks).expect("engine");
    render_until_playing(&mut engine, 2048);

    engine.request_nav(NavAction::JumpToNextIntro);
    let mut buf = vec![0.0f32; 2048 * 2];
    let _ = engine.render(&mut buf);
    let events = engine.poll_events();
    assert!(
        events
            .iter()
            .any(|e| matches!(e, EngineEvent::TrackStarted { path, .. } if path == &path_b)),
        "expected TrackStarted for B, got {events:?}"
    );

    // Advance a bit into B, then restart current (left ×1).
    for _ in 0..100 {
        let _ = engine.render(&mut buf);
    }
    engine.request_nav(NavAction::RestartCurrent);
    // Offline path may WaitBar then transition — pull enough for a few bars.
    for _ in 0..(bar_frames as usize * 20 / 2048 + 10) {
        let _ = engine.render(&mut buf);
    }
    let events = engine.poll_events();
    assert!(
        events
            .iter()
            .any(|e| matches!(e, EngineEvent::TransitionStarted { .. })),
        "restart should start a transition, got {events:?}"
    );

    let _ = b_fd;
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn nav_prev_jump_after_natural_transition() {
    let _lock = engine_test_lock();
    use funkot_core::engine::NavAction;

    let dir = temp_dir("nav_prev");
    let cache = dir.join("cache");
    let path_a = dir.join("a.wav");
    let path_b = dir.join("b.wav");
    let sr = 44_100u32;
    // Short main so natural transition arrives quickly.
    write_wav(&path_a, &synth_track(180.0, 8, 8, 8, sr)).unwrap();
    write_wav(&path_b, &synth_track(180.0, 8, 16, 8, sr)).unwrap();

    let options = engine_opts(cache);
    let tracks =
        prepare_tracks_parallel(&options, &[path_a.clone(), path_b.clone()], 1).expect("prepare");
    let mut engine = Engine::from_prepared(options, tracks).expect("engine");

    let mut buf = vec![0.0f32; 4096 * 2];
    let mut saw_b = false;
    for _ in 0..200_000 {
        let n = engine.render(&mut buf);
        if n == 0 {
            break;
        }
        for e in engine.poll_events() {
            if matches!(e, EngineEvent::TrackStarted { path, .. } if path == path_b) {
                saw_b = true;
            }
        }
        if saw_b {
            break;
        }
    }
    assert!(saw_b, "should reach track B via natural transition");

    engine.request_nav(NavAction::JumpToPrevIntro);
    let _ = engine.render(&mut buf);
    let events = engine.poll_events();
    assert!(
        events
            .iter()
            .any(|e| matches!(e, EngineEvent::TrackStarted { path, .. } if path == &path_a)),
        "left×3 should jump to A intro, got {events:?}"
    );

    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn nav_real_ivy_transition_clip_if_present() {
    use funkot_core::engine::NavAction;
    use hound::{SampleFormat, WavSpec, WavWriter};

    let (Some(path_a), Some(path_b)) = (
        funkot_core::testdata::track("AntonFer - Gakumas no Remix 2 - 02 IVY"),
        funkot_core::testdata::track("AntonFer - Gakumas no Remix 2 - 09 Sakura Photograph"),
    ) else {
        eprintln!("skip nav_real_ivy: real-audio test set missing");
        return;
    };
    let _lock = engine_test_lock();

    let dir = temp_dir("nav_real");
    let cache = funkot_core::testdata::local_dir().join("real-cache-v8");
    let options = EngineOptions {
        rate: 1.10,
        pitch_mode: PitchMode::Preserve,
        fade_bars: 4,
        highpass_hz: 300.0,
        gain_normalize: true,
        random: false,
        loop_playlist: false,
        output_sample_rate: 44_100,
        cache_dir: cache,
        head_only_secs: None,
    };
    let tracks = prepare_tracks_parallel(&options, &[path_a, path_b], 1).expect("prepare real");
    let bar = options.bar_frames();
    let sr = options.output_sample_rate;
    let mut engine = Engine::from_prepared(options, tracks).expect("engine");
    render_until_playing(&mut engine, 4096);

    // Advance ~30s into the first track so local BPM sees steady material.
    let skip = (30.0 * f64::from(sr)) as usize;
    let mut buf = vec![0.0f32; 4096 * 2];
    let mut advanced = 0usize;
    while advanced < skip {
        let n = engine.render(&mut buf);
        assert!(n > 0);
        advanced += n;
    }

    engine.request_nav(NavAction::TransitionToNext);
    let mut out = Vec::new();
    let capture = (bar * 24.0).round() as usize; // ~24 bars covers wait + fade
    let mut got = 0usize;
    let mut saw_trans = false;
    while got < capture {
        let n = engine.render(&mut buf);
        assert!(n > 0, "render ended early");
        for e in engine.poll_events() {
            if matches!(e, EngineEvent::TransitionStarted { .. }) {
                saw_trans = true;
            }
        }
        out.extend_from_slice(&buf[..n * 2]);
        got += n;
    }
    assert!(saw_trans, "expected TransitionStarted from nav");
    assert_finite_peak(&out, 8.0);

    let mono = mono_mix(&out);
    let bf = bar.round() as usize;
    let rms = bar_rms(&mono, bf);
    assert!(rms.len() >= 4);
    let peak_rms = rms.iter().copied().fold(0.0f32, f32::max);
    for (i, &v) in rms.iter().enumerate() {
        assert!(
            v >= 0.05 * peak_rms,
            "nav transition bar {i} dropout rms={v} peak={peak_rms}"
        );
    }

    let clip = dir.join("nav_ivy_transition.wav");
    let spec = WavSpec {
        channels: 2,
        sample_rate: sr,
        bits_per_sample: 32,
        sample_format: SampleFormat::Float,
    };
    let mut w = WavWriter::create(&clip, spec).expect("wav");
    for frame in out.chunks_exact(2) {
        w.write_sample(frame[0]).unwrap();
        w.write_sample(frame[1]).unwrap();
    }
    w.finalize().unwrap();
    eprintln!("wrote {}", clip.display());
    // Keep clip under this checkout's testdata for listening if desired.
    let listen = funkot_core::testdata::local_dir().join("nav_ivy_transition_clip.wav");
    let _ = std::fs::copy(&clip, &listen);
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn nav_replaces_pending_action() {
    let _lock = engine_test_lock();
    use funkot_core::engine::NavAction;

    let dir = temp_dir("nav_replace");
    let cache = dir.join("cache");
    let path_a = dir.join("a.wav");
    let path_b = dir.join("b.wav");
    let sr = 44_100u32;
    write_wav(&path_a, &synth_track(180.0, 16, 32, 16, sr)).unwrap();
    write_wav(&path_b, &synth_track(180.0, 16, 32, 16, sr)).unwrap();

    let options = engine_opts(cache);
    let tracks = prepare_tracks_parallel(&options, &[path_a, path_b.clone()], 1).expect("prepare");
    let mut engine = Engine::from_prepared(options, tracks).expect("engine");
    render_until_playing(&mut engine, 2048);

    // Queue a transition, then replace with an immediate jump.
    engine.request_nav(NavAction::TransitionToNext);
    engine.request_nav(NavAction::JumpToNextIntro);
    let mut buf = vec![0.0f32; 2048 * 2];
    let _ = engine.render(&mut buf);
    let events = engine.poll_events();
    assert!(
        events
            .iter()
            .any(|e| matches!(e, EngineEvent::TrackStarted { path, .. } if path == &path_b)),
        "jump should win over pending transition, got {events:?}"
    );

    let _ = std::fs::remove_dir_all(&dir);
}

/// Regression: seeding 3 loader permits before rewind history existed let the
/// loader prepare-and-drop tracks in a loop (Symphonia probe WARN spam).
#[test]
fn loader_does_not_spin_decode_before_history() {
    let _lock = engine_test_lock();
    use funkot_core::decode;

    let dir = temp_dir("permit_spin");
    let cache = dir.join("cache");
    let sr = 44_100u32;
    // Long enough that we stay on track 1 while spinning render; short enough
    // that a surplus prepare loop would decode many files quickly.
    let mut paths = Vec::new();
    for i in 0..6 {
        let p = dir.join(format!("t{i}.wav"));
        write_wav(&p, &synth_track(180.0, 16, 64, 16, sr)).unwrap();
        paths.push(p);
    }

    decode::reset_decode_file_calls();
    let mut options = engine_opts(cache);
    options.loop_playlist = true;
    let mut engine = Engine::new(options, paths).expect("engine");
    render_until_playing(&mut engine, 2048);

    // ~2s of audio at 44.1k / 2048 — well before a 64-bar main outro.
    let mut buf = vec![0.0f32; 2048 * 2];
    for _ in 0..50 {
        let _ = engine.render(&mut buf);
        let _ = engine.poll_events();
        std::thread::sleep(std::time::Duration::from_millis(20));
    }

    let calls = decode::decode_file_calls();
    // first-live + one next prefetch (occasionally +1 if Upgrade path re-touches;
    // never a continuous spin through the 6-track playlist).
    assert!(
        calls <= 3,
        "expected ≤3 decode_file calls before history exists, got {calls}"
    );

    engine.stop();
    let _ = std::fs::remove_dir_all(&dir);
}

/// Minimal host-owned queue [`TrackSource`]: a shared `VecDeque` a test can
/// mutate between polls, unlike the fixed-`Vec` [`PlaylistSource`] wired into
/// [`Engine::new`] (which offers no way to replace an already-queued track).
struct QueueSource {
    queue: std::sync::Arc<Mutex<std::collections::VecDeque<PathBuf>>>,
    next_index: usize,
}

impl funkot_core::engine::TrackSource for QueueSource {
    fn next(&mut self) -> Option<(usize, PathBuf)> {
        let path = self
            .queue
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .pop_front()?;
        let idx = self.next_index;
        self.next_index += 1;
        Some((idx, path))
    }
}

#[test]
fn revoke_next_hands_back_the_prepared_track_and_the_loader_prepares_a_replacement() {
    let _lock = engine_test_lock();
    use std::collections::VecDeque;
    use std::sync::Arc;

    let dir = temp_dir("revoke_next_replace");
    let cache = dir.join("cache");
    let sr = 44_100u32;
    let path_first = dir.join("first.wav");
    let path_a = dir.join("a.wav");
    let path_b = dir.join("b.wav");
    // Long main/outro so no natural transition races the revoke within the
    // paced polling window below.
    write_wav(&path_first, &synth_track(180.0, 16, 64, 16, sr)).unwrap();
    write_wav(&path_a, &synth_track(180.0, 16, 64, 16, sr)).unwrap();
    write_wav(&path_b, &synth_track(180.0, 16, 64, 16, sr)).unwrap();

    // Only `first` and `a` are queued up front: the loader consumes both to
    // fill active + next_track and then blocks on the (exhausted) permit pool,
    // so it never observes an empty queue as end-of-playlist.
    let queue = Arc::new(Mutex::new(VecDeque::from([
        path_first.clone(),
        path_a.clone(),
    ])));
    let options = engine_opts(cache);
    let mut engine = Engine::new_with_source(
        options,
        Box::new(QueueSource {
            queue: Arc::clone(&queue),
            next_index: 0,
        }),
    )
    .expect("engine");
    render_until_playing(&mut engine, 2048);

    let mut buf = vec![0.0f32; 2048 * 2];
    // Paced like `loader_does_not_spin_decode_before_history`: render + sleep
    // per poll (not a tight spin) so the unthrottled engine can't race past
    // `first`'s 64-bar main/outro before the loader finishes preparing `a`.
    let mut got_a = false;
    for _ in 0..400 {
        let _ = engine.render(&mut buf);
        if engine.next_track_path() == Some(path_a.as_path()) {
            got_a = true;
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(20));
    }
    assert!(got_a, "timed out waiting for next_track_path() == a");

    queue
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .push_back(path_b.clone());

    let revoked = engine.revoke_next();
    assert_eq!(
        revoked.as_deref(),
        Some(path_a.as_path()),
        "revoke_next() should hand back the prepared track a"
    );
    assert_eq!(
        engine.next_track_path(),
        None,
        "next_track slot should be empty right after revoke"
    );

    let mut got_b = false;
    for _ in 0..400 {
        let _ = engine.render(&mut buf);
        if engine.next_track_path() == Some(path_b.as_path()) {
            got_b = true;
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(20));
    }
    assert!(
        got_b,
        "timed out waiting for the loader to prepare b as the replacement"
    );

    engine.stop();
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn revoke_next_is_none_when_nothing_is_prepared() {
    let _lock = engine_test_lock();

    let dir = temp_dir("revoke_next_empty");
    let cache = dir.join("cache");
    let sr = 44_100u32;
    let path = dir.join("solo.wav");
    write_wav(&path, &synth_track(180.0, 16, 32, 16, sr)).unwrap();

    let options = engine_opts(cache);
    let mut engine = Engine::new(options, vec![path]).expect("engine");

    // Nothing has been drained from the loader yet: revoke must be a no-op.
    assert_eq!(engine.revoke_next(), None);

    // Solo playlist never fills next_track (loader hits Exhausted right after
    // the first track), so revoke stays None after playback starts too.
    render_until_playing(&mut engine, 2048);
    assert_eq!(engine.revoke_next(), None);

    engine.stop();
    let _ = std::fs::remove_dir_all(&dir);
}

/// Regression: revoke_next() must release exactly the one permit `next_track`
/// was holding — releasing zero starves the loader, releasing more than one
/// reproduces the pre-existing permit-spin bug covered by
/// `loader_does_not_spin_decode_before_history`.
#[test]
fn revoking_the_next_track_does_not_leak_a_permit() {
    let _lock = engine_test_lock();
    use funkot_core::decode;

    let dir = temp_dir("revoke_permit_leak");
    let cache = dir.join("cache");
    let sr = 44_100u32;
    let mut paths = Vec::new();
    for i in 0..6 {
        let p = dir.join(format!("t{i}.wav"));
        write_wav(&p, &synth_track(180.0, 16, 64, 16, sr)).unwrap();
        paths.push(p);
    }

    decode::reset_decode_file_calls();
    let mut options = engine_opts(cache);
    options.loop_playlist = true;
    let mut engine = Engine::new(options, paths).expect("engine");
    render_until_playing(&mut engine, 2048);

    let mut buf = vec![0.0f32; 2048 * 2];
    let mut revoked = false;
    // Paced (render + sleep per poll, not a tight spin) so the unthrottled
    // engine can't race past a 64-bar main/outro while waiting for the loader.
    for _ in 0..400 {
        let _ = engine.render(&mut buf);
        let _ = engine.poll_events();
        if !revoked && engine.next_track_path().is_some() {
            let got = engine.revoke_next();
            assert!(got.is_some(), "expected a prepared next track to revoke");
            revoked = true;
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(20));
    }
    assert!(revoked, "timed out waiting for a next track to revoke");

    // ~2s more, well before a 64-bar main outro would trigger a natural
    // transition on its own.
    for _ in 0..50 {
        let _ = engine.render(&mut buf);
        let _ = engine.poll_events();
        std::thread::sleep(std::time::Duration::from_millis(20));
    }

    let calls = decode::decode_file_calls();
    // Baseline (no revoke) is <=3, see loader_does_not_spin_decode_before_history:
    // first-live + one next prefetch, occasionally +1 for an Upgrade re-touch.
    // One revoke frees exactly one permit, which the loader spends preparing
    // exactly one replacement (+1). A leaked permit (releasing more than one)
    // would let the loader keep spinning through the 6-track playlist instead
    // of blocking again, which this bound catches; measured actual was <=4.
    assert!(
        calls <= 4,
        "expected ≤4 decode_file calls after one revoke, got {calls}"
    );

    engine.stop();
    let _ = std::fs::remove_dir_all(&dir);
}

// --- Labeling mode (head-only prepare) ------------------------------------

/// Continuous (non-silent) sine tone. Used by the head-only loop/skip tests,
/// which assert the render output never goes bit-exact silent for a long
/// run — a burst/decay signal like [`synth_track`] would produce long
/// natural silences between kicks that are indistinguishable from an engine
/// stall, so these tests need a signal that is never quiet by construction.
fn continuous_tone_track(
    sr: u32,
    secs: f64,
    freq_hz: f64,
    amp: f32,
) -> funkot_core::decode::AudioBuffer {
    use std::f64::consts::PI;
    let frames = (secs * f64::from(sr)).round().max(1.0) as usize;
    let mut samples = Vec::with_capacity(frames * 2);
    for i in 0..frames {
        let t = i as f64 / f64::from(sr);
        let s = (f64::from(amp) * (2.0 * PI * freq_hz * t).sin()) as f32;
        samples.push(s);
        samples.push(s);
    }
    funkot_core::decode::AudioBuffer {
        sample_rate: sr,
        frames: frames as u64,
        samples,
    }
}

/// Seed the cache with a manual, complete analysis (`first_downbeat: 0`) so
/// `prepare_track_head_only` / `prepare_track` skip real analysis. These
/// head-only loop/skip tests exercise engine plumbing, not the analyzer, and
/// [`continuous_tone_track`] has no real kick/downbeat structure to detect.
fn seed_head_only_analysis(
    path: &std::path::Path,
    cache_dir: &std::path::Path,
    buf: &funkot_core::decode::AudioBuffer,
    bpm: f64,
) {
    let hash = funkot_core::cache::content_hash(path).expect("hash");
    let analysis = funkot_core::TrackAnalysis {
        version: funkot_core::cache::CACHE_VERSION,
        file_name: path
            .file_name()
            .and_then(|s| s.to_str())
            .unwrap_or("t.wav")
            .to_string(),
        sample_rate: buf.sample_rate,
        total_frames: buf.frames,
        intro_bpm: bpm,
        outro_bpm: bpm,
        first_downbeat: 0,
        outro_start: buf.frames,
        intro_bars: 8,
        track_bars: 24,
        outro_bars: 8,
        outro_structure_bars: 8,
        bars_estimated_low_confidence: false,
        intro_bars_low_confidence: false,
        outro_bars_low_confidence: false,
        intro_bars_manual: false,
        outro_bars_manual: false,
        outro_structure_bars_manual: false,
        needs_reanalysis: false,
        is_funkot: true,
        classify_scores: None,
        rms_dbfs: -6.0,
        gain_db: 0.0,
    };
    funkot_core::cache::store(cache_dir, &hash, &analysis).expect("store cache");
}

/// Longest run (in frames) where both channels stay under `eps`.
fn max_silent_run(interleaved: &[f32], eps: f32) -> u32 {
    let mut run = 0u32;
    let mut max_run = 0u32;
    for frame in interleaved.chunks(2) {
        let peak = frame.iter().fold(0.0f32, |m, &s| m.max(s.abs()));
        if peak < eps {
            run += 1;
            max_run = max_run.max(run);
        } else {
            run = 0;
        }
    }
    max_run
}

#[test]
fn head_only_window_starts_at_first_downbeat() {
    let _lock = engine_test_lock();
    use funkot_core::engine::prepare_track_head_only;
    use funkot_core::testutil::{synth_track_with_options, SynthOptions};

    let dir = temp_dir("head_only_fd");
    let cache = dir.join("cache");
    let path = dir.join("a.wav");
    let buf = synth_track_with_options(SynthOptions {
        lead_in_secs: 2.0,
        bpm: 180.0,
        intro_bars: 16,
        main_bars: 32,
        outro_bars: 16,
        sample_rate: 44_100,
        ..SynthOptions::default()
    });
    write_wav(&path, &buf).expect("write");

    let options = engine_opts(cache);
    // Seed the cache with real analysis first: a cold cache would make
    // `prepare_track_head_only`'s `get_cached_or_provisional` fall back to
    // provisional markers (first_downbeat == 0 unconditionally), which would
    // make this test pass trivially even if the head window ignored
    // `first_downbeat` entirely.
    let full = funkot_core::engine::prepare_track(&options, &path, 0).expect("prepare full");

    let h = prepare_track_head_only(&options, &path, 0, 4.0).expect("prepare head");
    assert!(h.head_only, "head_only must be true");
    assert!(h.preview, "head_only implies preview");
    assert_eq!(h.outro_start_out, h.frames, "preview holds outro at EOF");
    assert_eq!(h.outro_end_anchored_out, h.frames, "preview holds outro at EOF");
    assert_eq!(h.first_downbeat_out, 0, "head window playhead starts at 0");
    assert!(
        h.frames < full.frames,
        "head window ({}) must be shorter than the full track ({})",
        h.frames,
        full.frames
    );

    let half_sec = (0.5 * f64::from(options.output_sample_rate)) as usize;
    let head_peak = h.samples[..(half_sec * 2).min(h.samples.len())]
        .iter()
        .fold(0.0f32, |m, &s| m.max(s.abs()));
    assert!(
        head_peak > 0.05,
        "head window should be audible near its start, got {head_peak}"
    );

    let full_peak = full.samples[..(half_sec * 2).min(full.samples.len())]
        .iter()
        .fold(0.0f32, |m, &s| m.max(s.abs()));
    assert!(
        full_peak < 1e-3,
        "full track's start should still be lead-in silence, got {full_peak}"
    );

    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn head_only_tail_is_faded() {
    let _lock = engine_test_lock();
    use funkot_core::engine::prepare_track_head_only;

    let dir = temp_dir("head_only_tail");
    let cache = dir.join("cache");
    let path = dir.join("a.wav");
    let sr = 44_100u32;
    // Constantly loud tone with no natural silence anywhere, so a faded tail
    // can only come from `fade_out_tail` itself, not from the source signal
    // (unlike a fixture with a pre-silenced tail, which would pass this test
    // even with the fade removed).
    let buf = continuous_tone_track(sr, 2.0, 1000.0, 0.5);
    write_wav(&path, &buf).expect("write");
    seed_head_only_analysis(&path, &cache, &buf, 180.0);

    let options = engine_opts(cache);
    let h = prepare_track_head_only(&options, &path, 0, 2.0).expect("prepare head");

    // Well before the fade's 10ms window, the tone must still be at full
    // amplitude.
    let hundred_ms = ((0.100 * f64::from(options.output_sample_rate)) as usize).max(1);
    let total_frames = h.samples.len() / 2;
    let window_end = total_frames.saturating_sub(hundred_ms * 2);
    let window_start = window_end.saturating_sub(hundred_ms);
    let before_fade_peak = h.samples[window_start * 2..window_end * 2]
        .iter()
        .fold(0.0f32, |m, &s| m.max(s.abs()));
    assert!(
        before_fade_peak > 0.3,
        "well before the fade window the tone must still be loud, got {before_fade_peak}"
    );

    // `fade_out_tail`'s linear ramp drives gain to exactly 0.0 at the very
    // last frame regardless of the source signal's phase, so this holds
    // exactly rather than merely "near zero" — a 10ms linear ramp over a
    // constantly loud tone is not near zero anywhere except that one sample.
    let last_frame = &h.samples[h.samples.len() - 2..];
    assert_eq!(
        last_frame,
        &[0.0f32, 0.0f32],
        "the final frame must be exactly silenced by the fade, got {last_frame:?}"
    );

    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn head_only_loops_without_transition_offline() {
    let _lock = engine_test_lock();
    use funkot_core::engine::prepare_track_head_only;

    let dir = temp_dir("head_only_loop_offline");
    let cache = dir.join("cache");
    let path_a = dir.join("a.wav");
    let path_b = dir.join("b.wav");
    let sr = 44_100u32;
    let buf_a = continuous_tone_track(sr, 10.0, 1000.0, 0.5);
    let buf_b = continuous_tone_track(sr, 10.0, 1500.0, 0.5);
    write_wav(&path_a, &buf_a).expect("write a");
    write_wav(&path_b, &buf_b).expect("write b");
    seed_head_only_analysis(&path_a, &cache, &buf_a, 180.0);
    seed_head_only_analysis(&path_b, &cache, &buf_b, 180.0);

    let options = engine_opts(cache);
    let track_a = prepare_track_head_only(&options, &path_a, 0, 2.0).expect("prepare a");
    let track_b = prepare_track_head_only(&options, &path_b, 1, 2.0).expect("prepare b");
    assert!(track_a.head_only && track_b.head_only);
    let a_frames = track_a.frames;

    let mut engine = Engine::from_prepared(options, vec![track_a, track_b]).expect("engine");

    let mut track_started = 0usize;
    let mut transition_started = 0usize;
    let mut out: Vec<f32> = Vec::new();
    let mut buf = vec![0.0f32; 2048 * 2];
    let target_frames = a_frames as usize * 3;
    while out.len() / 2 < target_frames {
        let n = engine.render(&mut buf);
        assert!(n > 0, "render stalled during head-only loop playback");
        out.extend_from_slice(&buf[..n * 2]);
        for e in engine.poll_events() {
            match e {
                EngineEvent::TrackStarted { .. } => track_started += 1,
                EngineEvent::TransitionStarted { .. } => transition_started += 1,
                _ => {}
            }
        }
    }

    assert_eq!(
        transition_started, 0,
        "head-only loop must never start a transition"
    );
    assert_eq!(
        track_started, 1,
        "head-only loop must not re-fire TrackStarted"
    );

    let max_run = max_silent_run(&out, 1e-6);
    assert!(
        max_run < 512,
        "found a {max_run}-frame near-silent run (loop seam gap?)"
    );

    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn head_only_loops_without_transition_realtime() {
    let _lock = engine_test_lock();
    use funkot_core::engine::prepare_track_head_only;

    let dir = temp_dir("head_only_loop_realtime");
    let cache = dir.join("cache");
    let path_a = dir.join("a.wav");
    let path_b = dir.join("b.wav");
    let sr = 44_100u32;
    let buf_a = continuous_tone_track(sr, 10.0, 1000.0, 0.5);
    let buf_b = continuous_tone_track(sr, 10.0, 1500.0, 0.5);
    write_wav(&path_a, &buf_a).expect("write a");
    write_wav(&path_b, &buf_b).expect("write b");
    seed_head_only_analysis(&path_a, &cache, &buf_a, 180.0);
    seed_head_only_analysis(&path_b, &cache, &buf_b, 180.0);

    let options = engine_opts(cache);
    let track_a = prepare_track_head_only(&options, &path_a, 0, 2.0).expect("prepare a");
    let track_b = prepare_track_head_only(&options, &path_b, 1, 2.0).expect("prepare b");
    let a_frames = track_a.frames;

    let mut engine = Engine::from_prepared(options, vec![track_a, track_b]).expect("engine");
    engine.set_realtime(true);

    let mut track_started = 0usize;
    let mut transition_started = 0usize;
    let mut out: Vec<f32> = Vec::new();
    let mut buf = vec![0.0f32; 2048 * 2];
    let target_frames = a_frames as usize * 3;
    while out.len() / 2 < target_frames {
        let n = engine.render(&mut buf);
        assert!(n > 0, "render stalled during head-only loop playback");
        out.extend_from_slice(&buf[..n * 2]);
        for e in engine.poll_events() {
            match e {
                EngineEvent::TrackStarted { .. } => track_started += 1,
                EngineEvent::TransitionStarted { .. } => transition_started += 1,
                _ => {}
            }
        }
    }

    assert_eq!(
        transition_started, 0,
        "head-only loop must never start a transition"
    );
    assert_eq!(
        track_started, 1,
        "head-only loop must not re-fire TrackStarted"
    );

    let max_run = max_silent_run(&out, 1e-6);
    assert!(
        max_run < 512,
        "found a {max_run}-frame near-silent run (loop seam gap?)"
    );

    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn head_only_skip_is_a_hard_cut() {
    let _lock = engine_test_lock();
    use funkot_core::engine::{prepare_track_head_only, NavAction};

    let dir = temp_dir("head_only_skip_cut");
    let cache = dir.join("cache");
    let path_a = dir.join("a.wav");
    let path_b = dir.join("b.wav");
    let sr = 44_100u32;
    let buf_a = continuous_tone_track(sr, 10.0, 1000.0, 0.5);
    let buf_b = continuous_tone_track(sr, 10.0, 1500.0, 0.5);
    write_wav(&path_a, &buf_a).expect("write a");
    write_wav(&path_b, &buf_b).expect("write b");
    seed_head_only_analysis(&path_a, &cache, &buf_a, 180.0);
    seed_head_only_analysis(&path_b, &cache, &buf_b, 180.0);

    let options = engine_opts(cache);
    let track_a = prepare_track_head_only(&options, &path_a, 0, 2.0).expect("prepare a");
    let track_b = prepare_track_head_only(&options, &path_b, 1, 2.0).expect("prepare b");

    let mut engine = Engine::from_prepared(options, vec![track_a, track_b]).expect("engine");
    engine.set_realtime(true);

    engine.request_nav(NavAction::TransitionToNext);
    let mut buf = vec![0.0f32; 2048 * 2];
    let n = engine.render(&mut buf);
    assert!(n > 0, "render returned 0 right after the skip");
    let events = engine.poll_events();
    assert!(
        events
            .iter()
            .any(|e| matches!(e, EngineEvent::TrackStarted { path, .. } if path == &path_b)),
        "expected TrackStarted for the second track, got {events:?}"
    );
    assert!(
        !events
            .iter()
            .any(|e| matches!(e, EngineEvent::TransitionStarted { .. })),
        "a head-only skip must be a hard cut, not a transition, got {events:?}"
    );
    let chunk = &buf[..n * 2];
    let peak = chunk.iter().fold(0.0f32, |m, &s| m.max(s.abs()));
    assert!(
        peak > 0.05,
        "the rendered chunk right after the skip must not be silent, got {peak}"
    );

    let _ = std::fs::remove_dir_all(&dir);
}

/// Regression test for the real-hardware bug: rapid repeated taps of
/// "next" during labeling arrived faster than the loader could refill a
/// single `next_track` slot, so most taps landed on `None` and were
/// silently dropped (`engine.rs`'s old `execute_jump`/`begin_nav` `return`
/// path). The fix is `Engine::head_queue`: a bounded lookahead
/// (`HEAD_ONLY_PREFETCH`) prepared *ahead of time*, so a burst of taps can
/// be served entirely from what's already on hand.
///
/// This must NOT spin-wait for `next_track_path()` to become ready before
/// each tap — that earlier pattern hid the bug (it always waited out
/// exactly the gap the real bug exposed). Instead: prefill the queue once,
/// then fire all 10 taps back-to-back with no render in between (taps
/// outrunning the loader, same as a human outrunning a ~1s prepare), and
/// require every one of them to land.
#[test]
fn head_only_skip_ten_times_never_stalls() {
    let _lock = engine_test_lock();
    use funkot_core::engine::{prepare_track_head_only, NavAction, HEAD_ONLY_PREFETCH};

    let dir = temp_dir("head_only_skip10");
    let cache = dir.join("cache");
    let sr = 44_100u32;
    // 1 active + 1 next + HEAD_ONLY_PREFETCH queued: exactly the runway the
    // permit budget (`2 + prefetch`, seeded at construction) allows.
    let total = 2 + HEAD_ONLY_PREFETCH;
    let mut paths = Vec::new();
    let mut tracks = Vec::new();
    let mut options = engine_opts(cache.clone());
    options.head_only_secs = Some(2.0);
    for i in 0..total as u32 {
        let p = dir.join(format!("t{i}.wav"));
        let buf = continuous_tone_track(sr, 6.0, 800.0 + f64::from(i) * 37.0, 0.5);
        write_wav(&p, &buf).expect("write");
        seed_head_only_analysis(&p, &cache, &buf, 180.0);
        tracks.push(prepare_track_head_only(&options, &p, i as usize, 2.0).expect("prepare"));
        paths.push(p);
    }

    let mut engine = Engine::from_prepared(options, tracks).expect("engine");
    engine.set_realtime(true);

    let mut buf = vec![0.0f32; 1024 * 2];
    // Prefill: let the background loader push every remaining track behind
    // `next_track` into the queue. Not part of the burst being measured.
    let mut spins = 0u64;
    while engine.ready_ahead() < 1 + HEAD_ONLY_PREFETCH {
        let n = engine.render(&mut buf);
        assert!(n > 0, "render stalled while prefilling the head queue");
        spins += 1;
        assert!(spins < 2_000_000, "timed out prefilling the head queue");
    }
    // Drop the startup TrackStarted (track 0, from construction) so only the
    // burst's own events remain to check below.
    let _ = engine.poll_events();

    // The burst: 10 taps, none waited on individually.
    let sender = engine.nav_sender();
    for _ in 0..10 {
        sender
            .try_send(NavAction::TransitionToNext)
            .expect("nav queue must have room for a 10-tap burst");
    }

    // A single render call drains and applies the whole burst (labeling
    // mode applies every queued action, not just the last).
    let n = engine.render(&mut buf);
    assert!(n > 0, "render stalled applying the 10-tap burst");
    let events = engine.poll_events();
    let started: Vec<_> = events
        .iter()
        .filter_map(|e| match e {
            EngineEvent::TrackStarted { path, .. } => Some(path.clone()),
            _ => None,
        })
        .collect();
    assert_eq!(
        started,
        paths[1..11].to_vec(),
        "all 10 queued taps must land, in order, from a single burst"
    );
    assert!(
        !events
            .iter()
            .any(|e| matches!(e, EngineEvent::TransitionStarted { .. })),
        "every hop in a head-only burst must be a hard cut, got {events:?}"
    );

    engine.stop();
    let _ = std::fs::remove_dir_all(&dir);
}

/// The head queue must actually fill up to `HEAD_ONLY_PREFETCH` behind
/// `next_track` when there is enough material and permit budget for it —
/// this is the runway the 10-tap burst above depends on.
#[test]
fn head_only_prefetch_fills_the_queue() {
    let _lock = engine_test_lock();
    use funkot_core::engine::{prepare_track_head_only, HEAD_ONLY_PREFETCH};

    let dir = temp_dir("head_only_prefetch");
    let cache = dir.join("cache");
    let sr = 44_100u32;
    // A couple extra tracks beyond what the queue can hold, to confirm the
    // depth is actually capped and not just "however many happen to exist".
    let total = 2 + HEAD_ONLY_PREFETCH + 2;
    let mut options = engine_opts(cache.clone());
    options.head_only_secs = Some(2.0);
    let mut tracks = Vec::new();
    for i in 0..total as u32 {
        let p = dir.join(format!("t{i}.wav"));
        let buf = continuous_tone_track(sr, 6.0, 700.0 + f64::from(i) * 29.0, 0.5);
        write_wav(&p, &buf).expect("write");
        seed_head_only_analysis(&p, &cache, &buf, 180.0);
        tracks.push(prepare_track_head_only(&options, &p, i as usize, 2.0).expect("prepare"));
    }

    let mut engine = Engine::from_prepared(options, tracks).expect("engine");
    engine.set_realtime(true);

    let mut buf = vec![0.0f32; 1024 * 2];
    let mut spins = 0u64;
    while engine.ready_ahead() < 1 + HEAD_ONLY_PREFETCH {
        let n = engine.render(&mut buf);
        assert!(n > 0, "render stalled filling the head queue");
        spins += 1;
        assert!(spins < 2_000_000, "timed out filling the head queue");
    }

    // Give the loader more chances to run; the two surplus tracks must be
    // dropped (surplus-drop safety net), not grow the queue past its cap.
    for _ in 0..2_000 {
        let n = engine.render(&mut buf);
        assert!(n > 0, "render stalled after the queue reached its cap");
    }
    assert_eq!(
        engine.ready_ahead(),
        1 + HEAD_ONLY_PREFETCH,
        "ready_ahead must stay capped at 1 (next_track) + HEAD_ONLY_PREFETCH (head_queue)"
    );

    engine.stop();
    let _ = std::fs::remove_dir_all(&dir);
}

/// Normal (non-labeling) mode must not grow a lookahead queue: exactly the
/// same single-`next_track` behavior as before this change, even with a
/// large playlist ready and waiting behind it.
#[test]
fn normal_mode_keeps_a_single_prepared_next() {
    let _lock = engine_test_lock();
    use funkot_core::engine::prepare_track;

    let dir = temp_dir("normal_single_next");
    let cache = dir.join("cache");
    let sr = 44_100u32;
    let options = engine_opts(cache.clone());
    let mut tracks = Vec::new();
    for i in 0..6u32 {
        let p = dir.join(format!("t{i}.wav"));
        let buf = continuous_tone_track(sr, 6.0, 700.0 + f64::from(i) * 29.0, 0.5);
        write_wav(&p, &buf).expect("write");
        seed_head_only_analysis(&p, &cache, &buf, 180.0);
        tracks.push(prepare_track(&options, &p, i as usize).expect("prepare"));
    }

    let mut engine = Engine::from_prepared(options, tracks).expect("engine");
    engine.set_realtime(true);

    let mut buf = vec![0.0f32; 1024 * 2];
    // Bounded well under the ~1.5M frames the 6 six-second tracks hold in
    // total (no `loop_playlist` for `from_prepared`'s finite `rest` list) --
    // this only needs to stay inside track 0's own playback to observe the
    // single-slot cap.
    for _ in 0..300 {
        let n = engine.render(&mut buf);
        assert!(n > 0, "render stalled");
        assert!(
            engine.ready_ahead() <= 1,
            "normal mode must never hold more than the single next_track slot, got {}",
            engine.ready_ahead()
        );
    }
    assert_eq!(
        engine.ready_ahead(),
        1,
        "normal mode's single next_track slot should have filled by now"
    );

    engine.stop();
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn head_only_finishes_at_end_of_playlist() {
    let _lock = engine_test_lock();
    use funkot_core::engine::prepare_track_head_only;

    let dir = temp_dir("head_only_finish");
    let cache = dir.join("cache");
    let path = dir.join("a.wav");
    let sr = 44_100u32;
    let buf = continuous_tone_track(sr, 10.0, 1000.0, 0.5);
    write_wav(&path, &buf).expect("write");
    seed_head_only_analysis(&path, &cache, &buf, 180.0);

    let options = engine_opts(cache);
    let track = prepare_track_head_only(&options, &path, 0, 2.0).expect("prepare");
    let frames = track.frames;

    let mut engine = Engine::from_prepared(options, vec![track]).expect("engine");

    let mut buf = vec![0.0f32; 2048 * 2];
    let mut produced = 0u64;
    let mut finished = false;
    let target = frames * 2;
    loop {
        let n = engine.render(&mut buf);
        if engine
            .poll_events()
            .iter()
            .any(|e| matches!(e, EngineEvent::Finished))
        {
            finished = true;
        }
        if finished {
            break;
        }
        if n == 0 {
            break;
        }
        produced += n as u64;
        if produced > target {
            break;
        }
    }
    assert!(
        finished,
        "expected Finished after the single head-only track's playlist ended"
    );
    assert_eq!(
        engine.render(&mut buf),
        0,
        "render must return 0 after Finished"
    );
    assert_eq!(
        engine.render(&mut buf),
        0,
        "render must keep returning 0 after Finished"
    );

    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn head_only_hands_off_to_a_full_next_track() {
    let _lock = engine_test_lock();
    use funkot_core::engine::prepare_track_head_only;

    let dir = temp_dir("head_only_handoff");
    let cache = dir.join("cache");
    let path_a = dir.join("a.wav");
    let path_b = dir.join("b.wav");
    let sr = 44_100u32;
    let buf_a = continuous_tone_track(sr, 10.0, 1000.0, 0.5);
    let buf_b = continuous_tone_track(sr, 10.0, 1500.0, 0.5);
    write_wav(&path_a, &buf_a).expect("write a");
    write_wav(&path_b, &buf_b).expect("write b");
    seed_head_only_analysis(&path_a, &cache, &buf_a, 180.0);
    seed_head_only_analysis(&path_b, &cache, &buf_b, 180.0);

    let options = engine_opts(cache);
    let track_a = prepare_track_head_only(&options, &path_a, 0, 2.0).expect("prepare a");
    let track_b =
        funkot_core::engine::prepare_track(&options, &path_b, 1).expect("prepare b full");
    assert!(
        !track_b.head_only,
        "second track must be a normal full prepare"
    );
    let a_frames = track_a.frames;

    let mut engine = Engine::from_prepared(options, vec![track_a, track_b]).expect("engine");

    let mut buf = vec![0.0f32; 2048 * 2];
    let mut produced = 0u64;
    let mut track_started_b = false;
    let mut transition_started = false;
    let target = a_frames + 4096;
    while produced < target {
        let n = engine.render(&mut buf);
        if n == 0 {
            break;
        }
        produced += n as u64;
        for e in engine.poll_events() {
            match e {
                EngineEvent::TrackStarted { path, .. } if path == path_b => {
                    track_started_b = true
                }
                EngineEvent::TransitionStarted { .. } => transition_started = true,
                _ => {}
            }
        }
    }

    assert!(
        track_started_b,
        "expected TrackStarted for the full second track"
    );
    assert!(
        !transition_started,
        "hand-off from a head-only deck must be a hard cut"
    );

    let _ = std::fs::remove_dir_all(&dir);
}

/// Normal mode's single `next_track` slot has no runway for a burst, so
/// `drain_nav_commands` must keep coalescing a burst down to only the last
/// queued action -- exactly the pre-existing behavior, unchanged by adding
/// the labeling-mode head queue. Uses the two `JumpTo*Intro` actions (not
/// `TransitionTo*`) because those apply unconditionally and synchronously
/// (no async tempo-analysis indirection), so "coalesced to last" vs.
/// "applied every" produce different, directly observable outcomes.
#[test]
fn nav_burst_is_coalesced_when_labeling_is_off() {
    let _lock = engine_test_lock();
    use funkot_core::engine::{prepare_track, NavAction};

    let dir = temp_dir("nav_burst_coalesced");
    let cache = dir.join("cache");
    let sr = 44_100u32;
    let mut paths = Vec::new();
    let options = engine_opts(cache.clone());
    let mut tracks = Vec::new();
    for i in 0..3u32 {
        let p = dir.join(format!("t{i}.wav"));
        let buf = continuous_tone_track(sr, 6.0, 900.0 + f64::from(i) * 50.0, 0.5);
        write_wav(&p, &buf).expect("write");
        seed_head_only_analysis(&p, &cache, &buf, 180.0);
        tracks.push(prepare_track(&options, &p, i as usize).expect("prepare"));
        paths.push(p);
    }
    let path0 = paths[0].clone();
    let path2 = paths[2].clone();

    let mut engine = Engine::from_prepared(options, tracks).expect("engine");
    engine.set_realtime(true);

    let mut buf = vec![0.0f32; 4096 * 2];
    let n = engine.render(&mut buf);
    assert!(n > 0);
    let _ = engine.poll_events();

    // One hop outside the burst, so `last_track` (rewind history) exists.
    engine.request_nav(NavAction::JumpToNextIntro);
    let n = engine.render(&mut buf);
    assert!(n > 0);
    let events = engine.poll_events();
    assert!(
        events
            .iter()
            .any(|e| matches!(e, EngineEvent::TrackStarted { path, .. } if path == &paths[1])),
        "setup hop must land on track 1, got {events:?}"
    );

    // Wait for track 2 to arrive behind track 1 (the setup hop's push_history
    // just armed the third permit) so the burst below has a real `next_track`
    // to land on, independent of loader thread scheduling.
    let mut spins = 0u64;
    while engine.next_track_path().is_none() {
        let n = engine.render(&mut buf);
        assert!(n > 0, "render stalled waiting for track 2");
        spins += 1;
        assert!(spins < 2_000_000, "timed out waiting for track 2");
    }
    let _ = engine.poll_events();

    // The burst: Prev then Next, queued together before any render. Coalesced
    // (normal mode) must apply only the last (Next) and never actually visit
    // `path0` via the dropped Prev.
    let sender = engine.nav_sender();
    sender.send(NavAction::JumpToPrevIntro).unwrap();
    sender.send(NavAction::JumpToNextIntro).unwrap();

    let n = engine.render(&mut buf);
    assert!(n > 0, "render stalled applying the coalesced burst");
    let events = engine.poll_events();
    let started: Vec<_> = events
        .iter()
        .filter_map(|e| match e {
            EngineEvent::TrackStarted { path, .. } => Some(path.clone()),
            _ => None,
        })
        .collect();

    assert_eq!(
        started,
        vec![path2],
        "normal mode must coalesce the burst to only the last queued action \
         (Next → track 2), never visiting {path0:?} via the dropped Prev; got {started:?}"
    );

    engine.stop();
    let _ = std::fs::remove_dir_all(&dir);
}

/// Labeling mode has runway (the head queue) for a burst, so every queued
/// tap must be applied in order -- unlike normal mode's coalesce-to-last.
/// Mirrors [`nav_burst_is_coalesced_when_labeling_is_off`] with the same
/// Prev/Next burst shape, but in head-only mode the outcome must be the
/// opposite: both hops execute, ending back where the burst started.
#[test]
fn nav_burst_applies_every_tap_when_labeling_is_on() {
    let _lock = engine_test_lock();
    use funkot_core::engine::{prepare_track_head_only, NavAction};

    let dir = temp_dir("nav_burst_applies_every");
    let cache = dir.join("cache");
    let sr = 44_100u32;
    let mut paths = Vec::new();
    let mut options = engine_opts(cache.clone());
    options.head_only_secs = Some(2.0);
    let mut tracks = Vec::new();
    for i in 0..4u32 {
        let p = dir.join(format!("t{i}.wav"));
        let buf = continuous_tone_track(sr, 6.0, 900.0 + f64::from(i) * 50.0, 0.5);
        write_wav(&p, &buf).expect("write");
        seed_head_only_analysis(&p, &cache, &buf, 180.0);
        tracks.push(prepare_track_head_only(&options, &p, i as usize, 2.0).expect("prepare"));
        paths.push(p);
    }

    let mut engine = Engine::from_prepared(options, tracks).expect("engine");
    engine.set_realtime(true);

    let mut buf = vec![0.0f32; 4096 * 2];
    let n = engine.render(&mut buf);
    assert!(n > 0);
    let _ = engine.poll_events();

    // Wait for both remaining tracks (2, 3) to land in the head queue behind
    // `next_track` (track 1), so the setup hop and the burst below are both
    // resolved purely from queue state, independent of loader scheduling.
    let mut spins = 0u64;
    while engine.ready_ahead() < 3 {
        let n = engine.render(&mut buf);
        assert!(n > 0, "render stalled filling the head queue");
        spins += 1;
        assert!(spins < 2_000_000, "timed out filling the head queue");
    }
    let _ = engine.poll_events();

    // One hop outside the burst (active becomes track 1, last_track = track 0).
    engine.request_nav(NavAction::TransitionToNext);
    let n = engine.render(&mut buf);
    assert!(n > 0);
    let events = engine.poll_events();
    assert!(
        events
            .iter()
            .any(|e| matches!(e, EngineEvent::TrackStarted { path, .. } if path == &paths[1])),
        "setup hop must land on track 1, got {events:?}"
    );

    // The burst: Prev then Next, queued together before any render.
    let sender = engine.nav_sender();
    sender.send(NavAction::TransitionToPrev).unwrap();
    sender.send(NavAction::TransitionToNext).unwrap();

    let n = engine.render(&mut buf);
    assert!(n > 0, "render stalled applying the burst");
    let events = engine.poll_events();
    let started: Vec<_> = events
        .iter()
        .filter_map(|e| match e {
            EngineEvent::TrackStarted { path, .. } => Some(path.clone()),
            _ => None,
        })
        .collect();

    assert_eq!(
        started,
        vec![paths[0].clone(), paths[1].clone()],
        "labeling mode must apply every queued tap in order (Prev then Next), \
         landing back on track 1 by way of track 0; got {started:?}"
    );

    engine.stop();
    let _ = std::fs::remove_dir_all(&dir);
}
