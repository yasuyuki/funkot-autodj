//! Does [`lock_beat_phase`](funkot_core::analysis::lock_beat_phase)'s comb
//! land on the beat or the offbeat, and would the low band (kick) have told
//! the two apart?
//!
//! `delta 0` is the analysis grid (`first_downbeat` plus an integer number of
//! beats) — measured across `testdata/`, that grid is correct for the tracks
//! it is built from, so `delta 0`'s score is the "known-good beat" baseline
//! every other candidate is compared against, not an arbitrary zero point.
//! `d_best` near `±0.45`–`±0.5` beat means the broadband comb picked the
//! offbeat instead of the beat for that window.
//!
//! ```sh
//! ./dev.sh cargo run -p funkot-cli --release --example offbeat_diag -- \
//!   --cache-dir funkot-cache [--step 8] [--half-width 4] TRACK...
//! ```

use std::path::{Path, PathBuf};

use funkot_cli::label_session::{bar_frames_for, Side};
use funkot_core::analysis::beat_phase_comb_scores;
use funkot_core::{cache, decode::decode_file};

/// Windows with `|d_best| > MOVED_THRESHOLD_BEATS` are counted as "moved"
/// (comb disagreed with the analysis grid) in the per-track summary.
const MOVED_THRESHOLD_BEATS: f64 = 0.25;

/// Splits the comb's candidates into the branch near the analysis grid and
/// the branch around the offbeat. The two flux peaks a Funkot master offers
/// sit half a beat apart; this is where one stops and the other starts.
const NEAR_LIMIT_BEATS: f64 = 0.25;

fn main() {
    let mut args = std::env::args().skip(1);
    let mut cache_dir = PathBuf::from("funkot-cache");
    let mut step = 8i64;
    let mut half_width = 4i64;
    let mut dump_bar: Option<i64> = None;
    let mut paths: Vec<PathBuf> = Vec::new();
    while let Some(a) = args.next() {
        match a.as_str() {
            "--cache-dir" => cache_dir = PathBuf::from(args.next().expect("needs a value")),
            "--step" => step = args.next().expect("needs a value").parse().expect("number"),
            "--dump-bar" => {
                dump_bar = Some(args.next().expect("needs a value").parse().expect("number"))
            }
            "--half-width" => {
                half_width = args.next().expect("needs a value").parse().expect("number")
            }
            _ => paths.push(PathBuf::from(a)),
        }
    }
    if paths.is_empty() {
        eprintln!(
            "usage: offbeat_diag [--cache-dir DIR] [--step N] [--half-width N] TRACK..."
        );
        std::process::exit(2);
    }
    for p in &paths {
        if let Err(e) = report(p, &cache_dir, step, half_width, dump_bar) {
            eprintln!("{}: {e}", p.display());
        }
    }
}

/// One window's worth of the numbers this diagnostic cares about, pulled out
/// of [`beat_phase_comb_scores`]'s per-delta list.
struct WindowStats {
    d_best: f64,
    best_over_zero: f64,
    off_over_zero: f64,
    kick0_over_kickbest: f64,
    /// Best-scoring delta within `NEAR_LIMIT_BEATS` of the analysis grid, and
    /// the best outside it, with the ratio between them. `lock_beat_phase`
    /// compares its winner against `delta == 0` alone, which on these masters
    /// sits in the *trough* between the two peaks (the grid is a few
    /// hundredths of a beat off), so that ratio says nothing about how
    /// decisively the offbeat won. This one does.
    near_d: f64,
    far_d: f64,
    far_over_near: f64,
}

fn window_stats(scores: &[funkot_core::analysis::BeatPhaseScore], beat: f64) -> Option<WindowStats> {
    if scores.is_empty() {
        return None;
    }
    let zero = scores.iter().find(|s| s.delta == 0)?;
    let best = scores.iter().max_by(|a, b| a.flux.total_cmp(&b.flux))?;
    // The offbeat phase is at one of the two ends of the delta range (the
    // comb's radius is half a beat), whichever end scores higher — the sign
    // of the true beat/offbeat split is not known a priori.
    let min_delta = scores.iter().min_by_key(|s| s.delta)?;
    let max_delta = scores.iter().max_by_key(|s| s.delta)?;
    let off = if min_delta.flux >= max_delta.flux {
        min_delta
    } else {
        max_delta
    };

    let ratio = |num: f64, den: f64| -> f64 {
        if den.abs() > f64::EPSILON {
            num / den
        } else {
            f64::NAN
        }
    };

    let limit = NEAR_LIMIT_BEATS * beat;
    let near = scores
        .iter()
        .filter(|s| (s.delta as f64).abs() <= limit)
        .max_by(|a, b| a.flux.total_cmp(&b.flux))?;
    let far = scores
        .iter()
        .filter(|s| (s.delta as f64).abs() > limit)
        .max_by(|a, b| a.flux.total_cmp(&b.flux))?;

    Some(WindowStats {
        near_d: near.delta as f64 / beat,
        far_d: far.delta as f64 / beat,
        far_over_near: ratio(far.flux, near.flux),
        d_best: best.delta as f64 / beat,
        best_over_zero: ratio(best.flux, zero.flux),
        off_over_zero: ratio(off.flux, zero.flux),
        kick0_over_kickbest: ratio(zero.kick, best.kick),
    })
}

/// `(min, median, max)` of `values`, or `NaN`s when empty. Sorts with
/// [`f64::total_cmp`] rather than `partial_cmp`/`unwrap_or(Equal)`: some
/// windows report `NaN` ratios (a zero denominator, e.g. a near-silent grid
/// position), and `Equal`-for-NaN is not a valid total order — the standard
/// library's sort detects that and panics instead of silently misordering.
fn stats(values: &[f64]) -> (f64, f64, f64) {
    if values.is_empty() {
        return (f64::NAN, f64::NAN, f64::NAN);
    }
    let mut sorted = values.to_vec();
    sorted.sort_by(f64::total_cmp);
    let min = sorted[0];
    let max = sorted[sorted.len() - 1];
    let mid = sorted.len() / 2;
    let median = if sorted.len() % 2 == 0 {
        (sorted[mid - 1] + sorted[mid]) / 2.0
    } else {
        sorted[mid]
    };
    (min, median, max)
}

/// Print the whole flux and kick curve for one window, each normalized to its
/// own maximum, so the two combs' peak positions can be compared directly.
/// The summary lines only ever compare three points of the curve; deciding
/// whether the low band can arbitrate a beat/offbeat split needs to see where
/// its peak actually is, not just how it ranks two phases the flux picked.
fn dump_window(scores: &[funkot_core::analysis::BeatPhaseScore], beat: f64) {
    let fmax = scores.iter().map(|s| s.flux).fold(0.0f64, f64::max);
    let kmax = scores.iter().map(|s| s.kick).fold(0.0f64, f64::max);
    println!("    delta   flux   kick");
    for s in scores {
        let f = if fmax > 0.0 { s.flux / fmax } else { 0.0 };
        let k = if kmax > 0.0 { s.kick / kmax } else { 0.0 };
        let bar = |v: f64| "#".repeat((v * 40.0).round().max(0.0) as usize);
        println!(
            "  {:>+6.3}  {:>5.3}  {:>5.3}  {:<41}|{}",
            s.delta as f64 / beat,
            f,
            k,
            bar(f),
            bar(k)
        );
    }
}

fn report(
    path: &Path,
    cache_dir: &Path,
    step: i64,
    half_width: i64,
    dump_bar: Option<i64>,
) -> Result<(), Box<dyn std::error::Error>> {
    let buf = decode_file(path)?;
    let analysis = cache::get_or_analyze(path, cache_dir, &buf)?;
    let bar_frames = bar_frames_for(&analysis, Side::Intro);
    let beat = bar_frames / 4.0;
    let fd = analysis.first_downbeat as f64;
    let total_frames = buf.frames as f64;

    println!("{}", path.display());
    println!(
        "  bar  d_best   best/zero   off/zero   kick0/kickbest   near_d   far_d  far/near"
    );

    let mut windows = 0u32;
    let mut moved_best_over_zero = Vec::new();
    let mut stayed_off_over_zero = Vec::new();
    let mut moved = 0u32;
    let mut moved_kick0_gt_kickbest = 0u32;
    let mut moved_far_over_near = Vec::new();
    let mut stayed_far_over_near = Vec::new();

    let mut b = 8i64;
    loop {
        let lo = fd + (b - half_width) as f64 * bar_frames;
        let hi = fd + (b + half_width) as f64 * bar_frames;
        if lo < 0.0 || hi > total_frames {
            break;
        }
        let clip_start = lo.round() as u64;
        let n_bars = (2 * half_width).max(0) as u32;
        let scores = beat_phase_comb_scores(&buf.samples, buf.sample_rate, clip_start, beat, n_bars);
        if dump_bar == Some(b) {
            println!("  dump bar {b} (window {half_width} bars either side)");
            dump_window(&scores, beat);
        }
        if let Some(w) = window_stats(&scores, beat) {
            windows += 1;
            println!(
                "  {b:>4}  {:>+7.2}  {:>10.3}  {:>10.3}  {:>14.2}  \
                 {:>+7.2} {:>+7.2}  {:>8.3}",
                w.d_best,
                w.best_over_zero,
                w.off_over_zero,
                w.kick0_over_kickbest,
                w.near_d,
                w.far_d,
                w.far_over_near,
            );
            if w.d_best.abs() > MOVED_THRESHOLD_BEATS {
                moved += 1;
                moved_best_over_zero.push(w.best_over_zero);
                moved_far_over_near.push(w.far_over_near);
                if w.kick0_over_kickbest > 1.0 {
                    moved_kick0_gt_kickbest += 1;
                }
            } else {
                stayed_off_over_zero.push(w.off_over_zero);
                stayed_far_over_near.push(w.far_over_near);
            }
        }
        b += step;
    }

    let (bmin, bmed, bmax) = stats(&moved_best_over_zero);
    let (omin, omed, omax) = stats(&stayed_off_over_zero);
    let (fmin, fmed, fmax) = stats(&moved_far_over_near);
    let (smin, smed, smax) = stats(&stayed_far_over_near);
    println!(
        "summary {} windows={windows} moved={moved}(|d_best|>{MOVED_THRESHOLD_BEATS}) \
         best/zero on moved: {bmin:.3}/{bmed:.3}/{bmax:.3}, \
         off/zero on stayed: {omin:.3}/{omed:.3}/{omax:.3}, \
         kick0>kickbest on moved: {moved_kick0_gt_kickbest}/{moved}, \
         far/near on moved: {fmin:.3}/{fmed:.3}/{fmax:.3}, \
         far/near on stayed: {smin:.3}/{smed:.3}/{smax:.3}",
        path.display(),
    );
    Ok(())
}
