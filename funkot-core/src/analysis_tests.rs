//! Analysis and cache tests (unit tests so `cfg(test)` exposes `testutil`).

use std::path::PathBuf;

use crate::analysis::{analyze, reconcile_intro_outro, refine_kick_marker, SectionEstimate};
use crate::cache::{self, get_cached_or_provisional, get_or_analyze};
use crate::decode::AudioBuffer;
use crate::stretch::{self, position_scale};
use crate::testutil::{synth_track, synth_track_with_options, write_wav, SynthOptions};
use crate::{Error, PitchMode, BEATS_PER_BAR, FALLBACK_BARS};

fn bar_len_frames(bpm: f64, sample_rate: u32) -> f64 {
    f64::from(sample_rate) * 60.0 / bpm * f64::from(BEATS_PER_BAR)
}

fn temp_dir(label: &str) -> PathBuf {
    let mut p = std::env::temp_dir();
    p.push(format!(
        "funkot_analysis_{}_{}_{}",
        label,
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0)
    ));
    let _ = std::fs::create_dir_all(&p);
    p
}

#[test]
fn classic_180_16_32_16() {
    let sr = 44_100;
    let bpm = 180.0;
    let buf = synth_track(bpm, 16, 32, 16, sr);
    let a = analyze(&buf, "classic.wav").expect("analyze");

    assert!(
        (a.intro_bpm - bpm).abs() < 0.3,
        "intro_bpm {} not within 0.3 of {bpm}",
        a.intro_bpm
    );
    assert!(
        (a.outro_bpm - bpm).abs() < 0.3,
        "outro_bpm {} not within 0.3 of {bpm}",
        a.outro_bpm
    );
    // Sharp boundaries → 16; reconciliation keeps equal lengths.
    assert_eq!(a.intro_bars, 16);
    assert_eq!(a.outro_bars, 16);
    assert!(!a.bars_estimated_low_confidence);
    assert!(!a.intro_bars_low_confidence);
    assert!(!a.outro_bars_low_confidence);

    let fd_secs = a.first_downbeat as f64 / f64::from(sr);
    assert!(fd_secs.abs() < 0.05, "first_downbeat {fd_secs}s not near 0");

    let expected_outro = (16 + 32) as f64 * bar_len_frames(bpm, sr);
    let outro_secs = a.outro_start as f64 / f64::from(sr);
    let expected_secs = expected_outro / f64::from(sr);
    assert!(
        (outro_secs - expected_secs).abs() < 0.1,
        "outro_start {outro_secs}s vs expected {expected_secs}s"
    );
}

#[test]
fn bpm_178_intro32_outro8_keeps_intro_ge_outro() {
    let sr = 44_100;
    let bpm = 178.0;
    let buf = synth_track(bpm, 32, 32, 8, sr);
    let a = analyze(&buf, "178.wav").expect("analyze");

    assert!((a.intro_bpm - bpm).abs() < 0.3, "intro_bpm {}", a.intro_bpm);
    assert!((a.outro_bpm - bpm).abs() < 0.3, "outro_bpm {}", a.outro_bpm);
    // Confident unequal sides: keep intro >= outro (may stay 32/8).
    assert!(
        a.intro_bars >= a.outro_bars,
        "intro {} < outro {}",
        a.intro_bars,
        a.outro_bars
    );
    assert!(
        matches!(a.intro_bars, 8 | 16 | 32),
        "unexpected intro length {}",
        a.intro_bars
    );
    assert!(
        matches!(a.outro_bars, 8 | 16 | 32),
        "unexpected outro length {}",
        a.outro_bars
    );
    // Sharp synth boundaries should not force the 64 fallback.
    assert_ne!(a.intro_bars, FALLBACK_BARS);
}

#[test]
fn long_track_64_bar_sections() {
    let sr = 44_100;
    let bpm = 181.0;
    let buf = synth_track(bpm, 64, 16, 64, sr);
    let a = analyze(&buf, "long.wav").expect("analyze");

    assert!((a.intro_bpm - bpm).abs() < 0.3, "intro_bpm {}", a.intro_bpm);
    assert!((a.outro_bpm - bpm).abs() < 0.3, "outro_bpm {}", a.outro_bpm);
    assert_eq!(a.intro_bars, 64);
    assert_eq!(a.outro_bars, 64);
    assert!(!a.bars_estimated_low_confidence);
    assert!(!a.intro_bars_low_confidence);
    assert!(!a.outro_bars_low_confidence);
}

#[test]
fn no_section_contrast_falls_back() {
    let sr = 44_100;
    let bpm = 180.0;
    let buf = synth_track_with_options(SynthOptions {
        bpm,
        intro_bars: 16,
        main_bars: 32,
        outro_bars: 16,
        sample_rate: sr,
        intro_outro_midhigh: true,
        ..SynthOptions::default()
    });
    let a = analyze(&buf, "flat.wav").expect("analyze");

    assert!((a.intro_bpm - bpm).abs() < 0.3, "intro_bpm {}", a.intro_bpm);
    assert!((a.outro_bpm - bpm).abs() < 0.3, "outro_bpm {}", a.outro_bpm);
    assert_eq!(a.intro_bars, FALLBACK_BARS);
    assert_eq!(a.outro_bars, FALLBACK_BARS);
    assert!(a.bars_estimated_low_confidence);
    assert!(a.intro_bars_low_confidence);
    assert!(a.outro_bars_low_confidence);
}

#[test]
fn bright_intro_outro_still_detects() {
    // Percussion already present in intro/outro, but main is clearly brighter.
    let sr = 44_100;
    let bpm = 180.0;
    let buf = synth_track_with_options(SynthOptions {
        bpm,
        intro_bars: 16,
        main_bars: 32,
        outro_bars: 16,
        sample_rate: sr,
        intro_bright_level: 0.45,
        outro_bright_level: 0.45,
        ..SynthOptions::default()
    });
    let a = analyze(&buf, "bright_io.wav").expect("analyze");
    assert_eq!(a.intro_bars, 16);
    assert_eq!(a.outro_bars, 16);
    assert!(!a.intro_bars_low_confidence);
    assert!(!a.outro_bars_low_confidence);
    assert!(!a.bars_estimated_low_confidence);
}

#[test]
fn gradual_intro_layering_detects_and_reconciles() {
    let sr = 44_100;
    let bpm = 180.0;
    let buf = synth_track_with_options(SynthOptions {
        bpm,
        intro_bars: 32,
        main_bars: 32,
        outro_bars: 16,
        sample_rate: sr,
        intro_bright_level: 0.55,
        gradual_intro_layers: true,
        outro_bright_level: 0.0,
        ..SynthOptions::default()
    });
    let a = analyze(&buf, "gradual.wav").expect("analyze");
    // Intro may be longer than outro; never shorter after reconciliation.
    assert!(
        a.intro_bars >= a.outro_bars,
        "intro {} < outro {}",
        a.intro_bars,
        a.outro_bars
    );
    assert!(
        matches!(a.intro_bars, 16 | 32 | 64),
        "unexpected intro bars {}",
        a.intro_bars
    );
    assert!(
        matches!(a.outro_bars, 8 | 16 | 32 | 64),
        "unexpected outro bars {}",
        a.outro_bars
    );
}

#[test]
fn main_body_energy_without_hf_ratio_jump() {
    // Main adds bass/body; HF ratio stays similar to a moderately bright intro.
    let sr = 44_100;
    let bpm = 180.0;
    let buf = synth_track_with_options(SynthOptions {
        bpm,
        intro_bars: 16,
        main_bars: 32,
        outro_bars: 8,
        sample_rate: sr,
        intro_bright_level: 0.7,
        outro_bright_level: 0.7,
        main_midhigh: true,
        main_bass_boost: 0.55,
        ..SynthOptions::default()
    });
    let a = analyze(&buf, "body_main.wav").expect("analyze");
    assert!(
        a.intro_bars >= a.outro_bars,
        "intro {} < outro {}",
        a.intro_bars,
        a.outro_bars
    );
    assert!(
        matches!(a.intro_bars, 8 | 16 | 32 | 64),
        "unexpected intro bars {}",
        a.intro_bars
    );
    assert!(
        matches!(a.outro_bars, 8 | 16 | 32),
        "unexpected outro bars {}",
        a.outro_bars
    );
    assert!(!a.intro_bars_low_confidence || a.intro_bars >= 16);
}

#[test]
fn ambiguous_near_flat_falls_back() {
    // Full brightness in intro/outro matching the main — no usable boundary.
    let sr = 44_100;
    let bpm = 180.0;
    let buf = synth_track_with_options(SynthOptions {
        bpm,
        intro_bars: 32,
        main_bars: 48,
        outro_bars: 32,
        sample_rate: sr,
        intro_bright_level: 1.0,
        outro_bright_level: 1.0,
        main_midhigh: true,
        main_bass_boost: 0.0,
        ..SynthOptions::default()
    });
    let a = analyze(&buf, "ambiguous.wav").expect("analyze");
    assert_eq!(a.intro_bars, FALLBACK_BARS);
    assert_eq!(a.outro_bars, FALLBACK_BARS);
    assert!(a.bars_estimated_low_confidence);
}

#[test]
fn lead_in_silence_downbeat() {
    let sr = 44_100;
    let lead = 0.7;
    let buf = synth_track_with_options(SynthOptions {
        bpm: 180.0,
        intro_bars: 16,
        main_bars: 16,
        outro_bars: 16,
        sample_rate: sr,
        lead_in_secs: lead,
        ..SynthOptions::default()
    });
    let a = analyze(&buf, "leadin.wav").expect("analyze");
    let fd_secs = a.first_downbeat as f64 / f64::from(sr);
    assert!(
        (fd_secs - lead).abs() < 0.05,
        "first_downbeat {fd_secs}s not near {lead}s"
    );
}

#[test]
fn quiet_track_gain() {
    let sr = 44_100;
    let buf = synth_track_with_options(SynthOptions {
        bpm: 180.0,
        intro_bars: 16,
        main_bars: 16,
        outro_bars: 16,
        sample_rate: sr,
        amplitude_scale: 0.1,
        ..SynthOptions::default()
    });
    let a = analyze(&buf, "quiet.wav").expect("analyze");
    assert!(a.gain_db > 0.0, "gain_db {}", a.gain_db);
    assert!(a.rms_dbfs < -20.0, "rms_dbfs {}", a.rms_dbfs);
    assert!(a.gain_db <= 12.0, "gain_db not clamped: {}", a.gain_db);
}

#[test]
fn reconcile_rules_unit() {
    let hi = |bars, score, sharp| SectionEstimate {
        bars,
        low_confidence: false,
        score,
        sharpness: sharp,
    };
    let lo = |bars| SectionEstimate {
        bars,
        low_confidence: true,
        score: 0.5,
        sharpness: 0.1,
    };

    // Both confident, intro >= outro (unequal OK): keep both.
    let (i, o, il, ol) = reconcile_intro_outro(hi(64, 5.0, 3.0), hi(32, 4.0, 2.5));
    assert_eq!((i, o), (64, 32));
    assert!(!il && !ol);

    // Equal confident lengths still kept.
    let (i, o, il, ol) = reconcile_intro_outro(hi(32, 5.0, 3.0), hi(32, 4.0, 2.5));
    assert_eq!((i, o), (32, 32));
    assert!(!il && !ol);

    // Both confident, intro < outro → raise intro to outro; mark intro inferred.
    let (i, o, il, ol) = reconcile_intro_outro(hi(32, 8.0, 4.0), hi(64, 3.0, 2.5));
    assert_eq!((i, o), (64, 64));
    assert!(il && !ol);

    // Intro low + outro credible 32 → max(64, 32)/32, intro stays low.
    let (i, o, il, ol) = reconcile_intro_outro(lo(16), hi(32, 6.0, 2.0));
    assert_eq!((i, o), (64, 32));
    assert!(il && !ol);

    // Intro low + longer credible outro → max(64, outro).
    let (i, o, il, ol) = reconcile_intro_outro(lo(16), hi(64, 6.0, 2.0));
    assert_eq!((i, o), (64, 64));
    assert!(il && !ol);

    // Intro credible 64 + outro low 16 → outro = min(64, intro) = 64, outro low.
    let (i, o, il, ol) = reconcile_intro_outro(hi(64, 5.0, 3.0), lo(16));
    assert_eq!((i, o), (64, 64));
    assert!(!il && ol);

    // Intro credible 32 + outro low → outro capped to intro (never longer).
    let (i, o, il, ol) = reconcile_intro_outro(hi(32, 5.0, 3.0), lo(16));
    assert_eq!((i, o), (32, 32));
    assert!(!il && ol);

    // Both low → FALLBACK (64).
    let (i, o, il, ol) = reconcile_intro_outro(lo(16), lo(32));
    assert_eq!((i, o), (FALLBACK_BARS, FALLBACK_BARS));
    assert!(il && ol);
}

#[test]
fn post_stretch_kick_refine_corrects_ms_error() {
    let sr = 44_100u32;
    let bpm = 180.0;
    let buf = synth_track(bpm, 16, 16, 16, sr);
    let speed = 1.10;
    let rendered =
        stretch::render_track(&buf.samples, sr, sr, speed, PitchMode::Preserve).expect("stretch");
    let in_frames = buf.frames;
    let out_frames = (rendered.len() / 2) as u64;
    let scale = position_scale(in_frames, out_frames);

    // True first kick is at frame 0 in the synth; after stretch, scale maps it.
    let true_out = (0.0 * scale).round() as u64;
    // Deliberately offset the marker by 20–40 ms (stretcher-like phase error).
    let err_frames = ((0.030 * f64::from(sr)).round() as u64).max(1);
    let wrong = true_out.saturating_add(err_frames);

    let refined = refine_kick_marker(&rendered, sr, wrong, 45.0);
    let err_ms = (refined as i64 - true_out as i64).unsigned_abs() as f64 * 1000.0 / f64::from(sr);
    assert!(
        err_ms < 6.0,
        "refined marker {refined} vs true {true_out}: {err_ms:.2}ms (wrong was {wrong})"
    );

    // Negative offset as well.
    let wrong_neg = true_out.saturating_add(err_frames / 2);
    let wrong_neg = wrong_neg.saturating_sub(err_frames);
    let refined_neg = refine_kick_marker(&rendered, sr, wrong_neg.max(1), 45.0);
    let err_ms_neg =
        (refined_neg as i64 - true_out as i64).unsigned_abs() as f64 * 1000.0 / f64::from(sr);
    assert!(
        err_ms_neg < 6.0,
        "neg refine {refined_neg} vs true {true_out}: {err_ms_neg:.2}ms"
    );
}

#[test]
fn refine_kick_marker_empty_safe() {
    assert_eq!(refine_kick_marker(&[], 44_100, 100, 40.0), 0);
    assert_eq!(refine_kick_marker(&[0.0, 0.0], 0, 0, 40.0), 0);
}

#[test]
fn cache_roundtrip_and_hand_edit() {
    let dir = temp_dir("cache_rt");
    let wav_path = dir.join("track.wav");
    let cache_dir = dir.join("cache");

    let buf = synth_track(180.0, 16, 16, 16, 44_100);
    write_wav(&wav_path, &buf).expect("write wav");

    let a1 = get_or_analyze(&wav_path, &cache_dir, &buf).expect("first analyze");
    let hash = cache::content_hash(&wav_path).expect("hash");
    let json_path = cache_dir.join(format!("{hash}.json"));
    assert!(json_path.exists(), "cache file missing");

    let a2 = get_or_analyze(&wav_path, &cache_dir, &buf).expect("second load");
    assert_eq!(a1, a2);

    // Hand-edit intro_bars in the JSON and confirm cache is honored.
    let mut text = std::fs::read_to_string(&json_path).expect("read cache");
    text = text.replacen(
        &format!("\"intro_bars\": {}", a1.intro_bars),
        "\"intro_bars\": 8",
        1,
    );
    std::fs::write(&json_path, text).expect("write edited cache");

    let a3 = get_or_analyze(&wav_path, &cache_dir, &buf).expect("edited load");
    assert_eq!(a3.intro_bars, 8);

    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn provisional_skips_analyze_and_cache_write() {
    let dir = temp_dir("cache_provisional");
    let wav_path = dir.join("track.wav");
    let cache_dir = dir.join("cache");

    let buf = synth_track(180.0, 16, 16, 16, 44_100);
    write_wav(&wav_path, &buf).expect("write wav");

    let (a, provisional) =
        get_cached_or_provisional(&wav_path, &cache_dir, &buf).expect("provisional");
    assert!(provisional);
    assert_eq!(a.intro_bpm, crate::NOMINAL_BPM);
    assert!(a.intro_bars >= 1);
    assert_eq!(a.outro_bars, a.intro_bars);
    assert_eq!(a.first_downbeat, 0);
    assert!(a.bars_estimated_low_confidence);
    // 16+16+16 bars → section = min(64, 48/3) = 16
    assert_eq!(a.intro_bars, 16);
    assert!(
        !cache_dir.exists() || cache_dir.read_dir().unwrap().next().is_none(),
        "provisional must not write cache"
    );

    // After a real analyze, provisional path must honor the cache.
    let real = get_or_analyze(&wav_path, &cache_dir, &buf).expect("analyze");
    let (cached, was_provisional) =
        get_cached_or_provisional(&wav_path, &cache_dir, &buf).expect("cached");
    assert!(!was_provisional);
    assert_eq!(cached, real);

    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn corrupt_cache_reanalyzes() {
    let dir = temp_dir("cache_corrupt");
    let wav_path = dir.join("track.wav");
    let cache_dir = dir.join("cache");

    let buf = synth_track(180.0, 16, 16, 16, 44_100);
    write_wav(&wav_path, &buf).expect("write wav");

    let a1 = get_or_analyze(&wav_path, &cache_dir, &buf).expect("first");
    let hash = cache::content_hash(&wav_path).expect("hash");
    let json_path = cache_dir.join(format!("{hash}.json"));
    std::fs::write(&json_path, "{not valid json!!!").expect("corrupt");

    let a2 = get_or_analyze(&wav_path, &cache_dir, &buf).expect("re-analyze");
    assert_eq!(a2.intro_bars, a1.intro_bars);
    assert_eq!(a2.outro_bars, a1.outro_bars);
    // Cache should have been overwritten with valid JSON.
    let reloaded = cache::load(&cache_dir, &hash).expect("reloaded");
    assert_eq!(reloaded, a2);

    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn short_track_errors() {
    let sr = 44_100;
    // 5 seconds of kick-only audio.
    let frames = sr * 5;
    let mut samples = Vec::with_capacity((frames * 2) as usize);
    for _ in 0..frames {
        samples.push(0.1);
        samples.push(0.1);
    }
    let buf = AudioBuffer {
        sample_rate: sr,
        frames: u64::from(frames),
        samples,
    };
    let err = analyze(&buf, "short.wav").expect_err("should fail");
    match err {
        Error::Analysis(msg) => assert!(msg.contains("short") || msg.contains("30")),
        other => panic!("expected Analysis error, got {other}"),
    }
}
