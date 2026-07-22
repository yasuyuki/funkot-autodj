//! Measure hat/kick phase alignment between a prev outro and next intro
//! as the engine would sync them at transition T (v12: markers + ±0.5 beat).
//!
//! Usage:
//!   cargo run -p funkot-core --example transition_phase_diag --release -- \
//!     [--rate 1.10] [--cache-dir DIR] [--sr 44100] PREV.flac NEXT.flac

use std::path::PathBuf;

use funkot_core::cache;
use funkot_core::decode;
use funkot_core::engine::{
    align_next_entry_scored, align_next_entry_with_phase_hypotheses, plan_transition,
    prepare_output_markers,
};
use funkot_core::stretch::{self, position_scale};
use funkot_core::{EngineOptions, PitchMode, BEATS_PER_BAR, NOMINAL_BPM};

fn main() {
    let mut rate = 1.10f64;
    let mut cache_dir = PathBuf::from("testdata/real-cache-v4");
    let mut out_sr = 44_100u32;
    let mut files: Vec<PathBuf> = Vec::new();
    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        if arg == "--rate" {
            rate = args.next().expect("--rate").parse().expect("rate");
        } else if arg == "--cache-dir" {
            cache_dir = PathBuf::from(args.next().expect("--cache-dir"));
        } else if arg == "--sr" {
            out_sr = args.next().expect("--sr").parse().expect("sr");
        } else if arg.starts_with('-') {
            eprintln!("unknown flag: {arg}");
            std::process::exit(2);
        } else {
            files.push(PathBuf::from(arg));
        }
    }
    if files.len() != 2 {
        eprintln!("usage: transition_phase_diag PREV NEXT");
        std::process::exit(2);
    }

    let options = EngineOptions {
        rate,
        pitch_mode: PitchMode::Preserve,
        output_sample_rate: out_sr,
        cache_dir: cache_dir.clone(),
        fade_bars: 4,
        ..EngineOptions::default()
    };
    let bar_frames = options.bar_frames();
    let beat_frames = bar_frames / f64::from(BEATS_PER_BAR);
    let target = NOMINAL_BPM * rate;

    let prep = |path: &PathBuf| {
        let buf = decode::decode_file(path).expect("decode");
        let analysis = cache::get_or_analyze(path, &options.cache_dir, &buf).expect("analysis");
        let intro_bpm = analysis.intro_bpm;
        let speed = target / intro_bpm;
        let rendered = stretch::render_track(
            &buf.samples,
            buf.sample_rate,
            out_sr,
            speed,
            PitchMode::Preserve,
        )
        .expect("stretch");
        let out_frames = (rendered.len() / 2) as u64;
        let scale = position_scale(buf.frames, out_frames);
        let mapped_fd = (analysis.first_downbeat as f64 * scale).round() as u64;
        let mapped_outro = (analysis.outro_start as f64 * scale).round() as u64;
        let (fd_out, outro_out, outro_end) = prepare_output_markers(
            &rendered,
            out_sr,
            out_frames,
            analysis.first_downbeat,
            analysis.outro_start,
            intro_bpm,
            buf.sample_rate,
            mapped_fd,
            mapped_outro,
            analysis.outro_bars,
            bar_frames,
        );
        (
            path.display().to_string(),
            analysis.intro_bars,
            analysis.outro_bars,
            fd_out,
            outro_out,
            outro_end,
            rendered,
        )
    };

    let (prev_name, _pi, po, _pfd, poutro, pend, prev_s) = prep(&files[0]);
    let (next_name, ni, _no, nfd, _noutro, _nend, next_s) = prep(&files[1]);
    let plan = plan_transition(options.fade_bars, ni, po);
    let entry = nfd.saturating_add(((f64::from(plan.skip)) * bar_frames).round() as u64);
    let (entry_grid, score_grid, nudge_grid) =
        align_next_entry_scored(&prev_s, poutro, &next_s, entry, out_sr, beat_frames);
    let delta = poutro as i64 - pend as i64;
    let entry_end_nom = if delta >= 0 {
        entry.saturating_add(delta as u64)
    } else {
        entry.saturating_sub((-delta) as u64)
    };
    let (entry_end, score_end, nudge_end) =
        align_next_entry_scored(&prev_s, poutro, &next_s, entry_end_nom, out_sr, beat_frames);
    let (entry_aligned, nudge_chosen) = align_next_entry_with_phase_hypotheses(
        &prev_s, poutro, &next_s, entry, poutro, pend, out_sr, beat_frames,
    );

    println!("target_bpm={target:.3} beat={beat_frames:.1}f bar={bar_frames:.1}f");
    println!("prev={prev_name}");
    println!(
        "  outro_grid={poutro} outro_end_anchored={pend} (Δ={}f / {:.2}ms / {:.3} beats) outro_bars={po}",
        delta,
        delta as f64 * 1000.0 / f64::from(out_sr),
        delta as f64 / beat_frames,
    );
    println!("next={next_name}");
    println!(
        "  first_downbeat_out={nfd} intro_bars={ni} entry={entry} skip={}",
        plan.skip
    );
    println!(
        "  hyp_grid: aligned={entry_grid} (Δ={}f / {:.2}ms) score={score_grid:.3} prev_nudge={nudge_grid}",
        entry_grid as i64 - entry as i64,
        (entry_grid as i64 - entry as i64) as f64 * 1000.0 / f64::from(out_sr),
    );
    println!(
        "  hyp_end:  aligned={entry_end} (Δ={}f / {:.2}ms vs nominal) score={score_end:.3} prev_nudge={nudge_end}",
        entry_end as i64 - entry as i64,
        (entry_end as i64 - entry as i64) as f64 * 1000.0 / f64::from(out_sr),
    );
    let chosen_beats = (entry_aligned as i64 - entry as i64) as f64 / beat_frames;
    let bar_off = ((chosen_beats / 1.0).round() as i64).rem_euclid(4);
    let phase_tag = if (entry_aligned as i64 - entry_end as i64).unsigned_abs()
        < (entry_aligned as i64 - entry_grid as i64).unsigned_abs()
        && entry_end != entry_grid
    {
        "end-anchored"
    } else {
        "intro-grid"
    };
    println!(
        "  chosen:   aligned={entry_aligned} (Δ={}f / {:.2}ms / {chosen_beats:+.3} beats) prev_nudge={nudge_chosen} [{phase_tag} bar_off≈{bar_off}]",
        entry_aligned as i64 - entry as i64,
        (entry_aligned as i64 - entry as i64) as f64 * 1000.0 / f64::from(out_sr),
    );
    println!(
        "plan: f_eff={} m={} fadeout={}..{} (bars)",
        plan.f_eff, plan.m, plan.fadeout_start, plan.fadeout_end
    );

    let bars = 16u32;
    let n = ((f64::from(bars)) * bar_frames).round() as usize;
    let prev_mix = poutro.saturating_add(nudge_chosen);
    println!("--- nominal entry ---");
    measure_pair(&prev_s, poutro, &next_s, entry, out_sr, n, target, beat_frames);
    println!("--- aligned entry (prev+nudge) ---");
    measure_pair(
        &prev_s,
        prev_mix,
        &next_s,
        entry_aligned,
        out_sr,
        n,
        target,
        beat_frames,
    );
}

fn measure_pair(
    prev_s: &[f32],
    poutro: u64,
    next_s: &[f32],
    entry: u64,
    out_sr: u32,
    n: usize,
    target: f64,
    beat_frames: f64,
) {
    let prev_mono = extract_mono(prev_s, poutro as usize, n);
    let next_mono = extract_mono(next_s, entry as usize, n);

    for (band, lo, hi) in [
        ("kick", 20.0, 200.0),
        ("hat", 6000.0, 14000.0),
        ("mid", 200.0, 2000.0),
    ] {
        let a = band_energy_series(&prev_mono, out_sr, lo, hi, 256);
        let b = band_energy_series(&next_mono, out_sr, lo, hi, 256);
        let (lag_hops, corr) = xcorr_peak(&a, &b, 64);
        let lag_ms = lag_hops as f64 * 256.0 * 1000.0 / f64::from(out_sr);
        let lag_beats = lag_ms / (60_000.0 / target);
        println!(
            "  {band:>4} energy xcorr: lag={lag_ms:+.2}ms ({lag_beats:+.3} beats) peak_corr={corr:.3}"
        );
    }

    let max_lag = (beat_frames * 0.5).round() as i64;
    let prev_hat = bandpass(&prev_mono, out_sr, 6000.0, 14000.0);
    let next_hat = bandpass(&next_mono, out_sr, 6000.0, 14000.0);
    let (lag, corr) = xcorr_wave(&prev_hat, &next_hat, max_lag);
    let lag_ms = lag as f64 * 1000.0 / f64::from(out_sr);
    println!("  hat waveform xcorr (±0.5 beat): lag={lag_ms:+.2}ms ({lag}f) corr={corr:.3}");

    let prev_kick = bandpass(&prev_mono, out_sr, 20.0, 200.0);
    let next_kick = bandpass(&next_mono, out_sr, 20.0, 200.0);
    let (klag, kcorr) = xcorr_wave(&prev_kick, &next_kick, max_lag);
    let klag_ms = klag as f64 * 1000.0 / f64::from(out_sr);
    println!("  kick waveform xcorr (±0.5 beat): lag={klag_ms:+.2}ms ({klag}f) corr={kcorr:.3}");
}

fn extract_mono(interleaved: &[f32], start: usize, n: usize) -> Vec<f32> {
    let mut out = Vec::with_capacity(n);
    for i in 0..n {
        let idx = (start + i) * 2;
        if idx + 1 >= interleaved.len() {
            break;
        }
        out.push(0.5 * (interleaved[idx] + interleaved[idx + 1]));
    }
    out
}

fn bandpass(x: &[f32], sr: u32, lo: f64, hi: f64) -> Vec<f32> {
    let mut y = x.to_vec();
    let mut hp = Biquad::highpass(sr, lo as f32);
    for s in &mut y {
        *s = hp.process(*s);
    }
    let mut lp = Biquad::lowpass(sr, hi as f32);
    for s in &mut y {
        *s = lp.process(*s);
    }
    y
}

fn band_energy_series(x: &[f32], sr: u32, lo: f64, hi: f64, hop: usize) -> Vec<f64> {
    let y = bandpass(x, sr, lo, hi);
    let mut out = Vec::new();
    let mut i = 0;
    while i + hop <= y.len() {
        let mut e = 0.0f64;
        for v in &y[i..i + hop] {
            e += f64::from(*v) * f64::from(*v);
        }
        out.push(e.sqrt());
        i += hop;
    }
    out
}

fn xcorr_peak(a: &[f64], b: &[f64], max_lag: i64) -> (i64, f64) {
    let n = a.len().min(b.len());
    if n < 8 {
        return (0, 0.0);
    }
    let a = &a[..n];
    let b = &b[..n];
    let mut best_lag = 0i64;
    let mut best = f64::NEG_INFINITY;
    for lag in -max_lag..=max_lag {
        let (sa, sb) = if lag >= 0 {
            let l = lag as usize;
            if l >= n {
                continue;
            }
            (&a[l..], &b[..n - l])
        } else {
            let l = (-lag) as usize;
            if l >= n {
                continue;
            }
            (&a[..n - l], &b[l..])
        };
        let m = sa.len().min(sb.len());
        let mut corr = 0.0;
        let mut ea = 0.0;
        let mut eb = 0.0;
        for i in 0..m {
            corr += sa[i] * sb[i];
            ea += sa[i] * sa[i];
            eb += sb[i] * sb[i];
        }
        let denom = (ea * eb).sqrt().max(1e-12);
        let c = corr / denom;
        if c > best {
            best = c;
            best_lag = lag;
        }
    }
    (best_lag, best)
}

fn xcorr_wave(a: &[f32], b: &[f32], max_lag: i64) -> (i64, f64) {
    let n = a.len().min(b.len());
    let a: Vec<f64> = a[..n].iter().map(|v| f64::from(*v)).collect();
    let b: Vec<f64> = b[..n].iter().map(|v| f64::from(*v)).collect();
    let mean = |x: &[f64]| x.iter().sum::<f64>() / x.len().max(1) as f64;
    let ma = mean(&a);
    let mb = mean(&b);
    let a: Vec<f64> = a.iter().map(|v| v - ma).collect();
    let b: Vec<f64> = b.iter().map(|v| v - mb).collect();
    xcorr_peak(&a, &b, max_lag)
}

struct Biquad {
    b0: f64,
    b1: f64,
    b2: f64,
    a1: f64,
    a2: f64,
    z1: f64,
    z2: f64,
}

impl Biquad {
    fn highpass(sr: u32, fc: f32) -> Self {
        Self::rbj(sr, fc, true)
    }
    fn lowpass(sr: u32, fc: f32) -> Self {
        Self::rbj(sr, fc, false)
    }
    fn rbj(sr: u32, fc: f32, highpass: bool) -> Self {
        let sr = f64::from(sr);
        let fc = f64::from(fc).clamp(1.0, sr * 0.49);
        let w0 = std::f64::consts::TAU * fc / sr;
        let cos_w0 = w0.cos();
        let sin_w0 = w0.sin();
        let q = std::f64::consts::FRAC_1_SQRT_2;
        let alpha = sin_w0 / (2.0 * q);
        let (b0, b1, b2) = if highpass {
            let b0 = (1.0 + cos_w0) * 0.5;
            (b0, -(1.0 + cos_w0), b0)
        } else {
            let b0 = (1.0 - cos_w0) * 0.5;
            (b0, 1.0 - cos_w0, b0)
        };
        let a0 = 1.0 + alpha;
        Self {
            b0: b0 / a0,
            b1: b1 / a0,
            b2: b2 / a0,
            a1: (-2.0 * cos_w0) / a0,
            a2: (1.0 - alpha) / a0,
            z1: 0.0,
            z2: 0.0,
        }
    }
    fn process(&mut self, x: f32) -> f32 {
        let x = f64::from(x);
        let y = self.b0 * x + self.z1;
        self.z1 = self.b1 * x - self.a1 * y + self.z2;
        self.z2 = self.b2 * x - self.a2 * y;
        y as f32
    }
}
