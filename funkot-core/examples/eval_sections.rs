//! Stage 0 evaluation harness for intro/outro bar-length estimation.
//!
//! Measures the current `analysis::analyze` estimator against hand-verified
//! labels (`funkot_core::labels`), so future changes to the estimator can be
//! compared against a baseline instead of "sounds right on my 10 tracks".
//! Does not change `analysis.rs` behaviour in any way — read-only harness.
//!
//! Usage (inside the dev container):
//!   cargo run -p funkot-core --example eval_sections --release -- \
//!     --labels testdata/labels.tsv [-l playlist.txt | FILE...] \
//!     [--cache-dir DIR] [--folds N] [--json OUT.json]
//!
//! `--labels` is required. Give tracks either via `-l PLAYLIST` (one path
//! per line, `#`-comments and blank lines ignored, same convention as
//! funkot-cli's playlist file) or as bare trailing file arguments (both may
//! be combined). Tracks present in the labels file but not passed in, and
//! tracks passed in but absent from the labels file, are reported as counts
//! rather than treated as errors.

use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};

use serde::Serialize;

use funkot_core::labels::{load_labels, SectionLabel};
use funkot_core::{analysis, cache, decode};

// ---------------------------------------------------------------------
// Outro label <-> TrackAnalysis comparison.
//
// `TrackAnalysis::outro_bars` (see funkot-core/src/lib.rs) is the DJ mix
// trigger: the musical main->outro structural boundary, plus a mix lead-in
// that is only added when there's room for it. Labels record the
// structural boundary itself (what a human hears as "this is where the
// outro starts"), since that's the thing that's well-defined and checkable
// by ear -- the lead-in is a DJ-timing choice layered on top, and is *not*
// a fixed offset from the structural boundary (it clamps independently
// against FALLBACK_BARS, and is skipped entirely when the outro is too
// short to fit it). An earlier version of this harness reconstructed the
// structural boundary as `outro_bars - 16`, which silently collapsed to 0
// (or otherwise diverged) whenever the lead-in wasn't actually added.
// `TrackAnalysis::outro_structure_bars` (cache version 9+) is the analyzer
// exposing that structural boundary directly, so comparison here needs no
// conversion or duplicated constant.

// ---------------------------------------------------------------------
// DJ severity cost weights.
//
// `analyze()`'s two sides fail in asymmetric ways for a DJ:
//  - Intro too short (estimate < truth): the next track is treated as
//    already in its audible main while it's still intro -- the transition
//    schedule pulls the previous track's fade-out earlier than the mix
//    point actually supports, i.e. the *previous* track gets cut off
//    mid-material. Audible train-wreck. Heavily penalized.
//  - Intro too long (estimate > truth): the switch to next-track-audible
//    happens a bit later than it could have; at worst a slightly longer
//    solo intro. Mildly penalized.
//  - Outro too long / overestimated (mixing starts before the true
//    structural boundary): the DJ starts blending a bit into the last
//    energetic bars of the main -- early, but Funkot mains are dense so a
//    few bars of overlap is forgiving. Mildly penalized.
//  - Outro too short / underestimated (mixing starts after the true
//    boundary, i.e. after the energy has already collapsed): the mix
//    starts over a dead/quiet tail, which is the definitionally audible
//    failure mode outro detection exists to avoid. Heavily penalized.
//
// The 4x ratio between "heavy" and "light" is a judgment call, not measured
// from listening tests (none exist yet -- that's the whole reason Stage 0
// exists). Treat these as a placeholder baseline; revisit once Stage 3 has
// enough evaluated changes to argue for different weights.
const INTRO_UNDERESTIMATE_WEIGHT: f64 = 2.0;
const INTRO_OVERESTIMATE_WEIGHT: f64 = 0.5;
const OUTRO_OVERESTIMATE_WEIGHT: f64 = 0.5;
const OUTRO_UNDERESTIMATE_WEIGHT: f64 = 2.0;
/// Normalizes cost so an error of one default fade length (4 bars) at the
/// light weight costs 0.125, and a heavy-direction miss of one fade-pair
/// width (`2 * fade_bars` = 8 bars, roughly the whole solo-intro gap /
/// [`funkot_core::MAIN_GAP_BARS`]) costs 1.0.
const COST_NORM_BARS: f64 = 16.0;

/// BPM sanity band for the downbeat/tempo grid. Outside this, the intro or
/// outro tempo measurement is probably wrong and bar-count comparisons for
/// that track aren't meaningful.
const GRID_BPM_MIN: f64 = 172.0;
const GRID_BPM_MAX: f64 = 188.0;
/// Max allowed |intro_bpm - outro_bpm| before flagging a possible mid-track
/// grid slip (drift/half-double tempo estimate on one side).
const GRID_BPM_DRIFT_MAX: f64 = 1.0;

/// Confusion-matrix / report axis. Values outside this set collapse to "other".
const CANDIDATE_BARS: [u32; 7] = [8, 16, 32, 48, 64, 80, 96];

fn main() {
    let opts = match Opts::parse(std::env::args().skip(1).collect()) {
        Ok(ParsedArgs::Help) => {
            print_usage();
            return;
        }
        Ok(ParsedArgs::Opts(o)) => o,
        Err(e) => {
            eprintln!("error: {e}");
            eprintln!();
            print_usage();
            std::process::exit(2);
        }
    };

    if let Err(e) = run(&opts) {
        eprintln!("error: {e}");
        std::process::exit(1);
    }
}

fn print_usage() {
    eprintln!("usage: eval_sections --labels FILE [-l PLAYLIST] [FILE...] [--cache-dir DIR] [--folds N] [--json OUT.json]");
    eprintln!();
    eprintln!("  --labels FILE     required; TSV of hand-verified intro/outro bar counts");
    eprintln!("                    (see funkot_core::labels / testdata/labels.tsv.example)");
    eprintln!("  -l PLAYLIST       playlist file, one audio path per line ('#' comments ok);");
    eprintln!("                    relative entries resolve against PLAYLIST's own directory,");
    eprintln!("                    not the current directory (same convention as funkot-cli)");
    eprintln!("  FILE...           bare audio file paths (combinable with -l)");
    eprintln!("  --cache-dir DIR   reuse/populate an analysis cache instead of always");
    eprintln!("                    re-analyzing (funkot_core::cache::get_or_analyze)");
    eprintln!("  --folds N         split labels into N stable hash-based folds and report");
    eprintln!("                    per-fold metrics variance (no training happens here)");
    eprintln!("  --json OUT.json   write all metrics as machine-readable JSON");
    eprintln!("  -h, --help        print this message");
}

struct Opts {
    labels_path: PathBuf,
    playlist: Option<PathBuf>,
    files: Vec<PathBuf>,
    cache_dir: Option<PathBuf>,
    folds: u32,
    json_out: Option<PathBuf>,
}

enum ParsedArgs {
    Help,
    Opts(Opts),
}

impl Opts {
    fn parse(args: Vec<String>) -> Result<ParsedArgs, String> {
        let mut labels_path = None;
        let mut playlist = None;
        let mut files = Vec::new();
        let mut cache_dir = None;
        let mut folds = 1u32;
        let mut json_out = None;

        let mut it = args.into_iter();
        while let Some(arg) = it.next() {
            match arg.as_str() {
                "-h" | "--help" => return Ok(ParsedArgs::Help),
                "--labels" => {
                    labels_path = Some(PathBuf::from(
                        it.next().ok_or("--labels needs a path")?,
                    ))
                }
                "-l" => playlist = Some(PathBuf::from(it.next().ok_or("-l needs a path")?)),
                "--cache-dir" => {
                    cache_dir = Some(PathBuf::from(
                        it.next().ok_or("--cache-dir needs a path")?,
                    ))
                }
                "--folds" => {
                    let raw = it.next().ok_or("--folds needs a number")?;
                    folds = raw
                        .parse::<u32>()
                        .map_err(|e| format!("--folds value '{raw}' is invalid: {e}"))?;
                    if folds == 0 {
                        return Err("--folds must be >= 1".to_string());
                    }
                }
                "--json" => {
                    json_out = Some(PathBuf::from(it.next().ok_or("--json needs a path")?))
                }
                other if other.starts_with('-') && other != "-" => {
                    return Err(format!("unknown flag: {other}"))
                }
                other => files.push(PathBuf::from(other)),
            }
        }

        let labels_path = labels_path.ok_or("--labels is required")?;
        Ok(ParsedArgs::Opts(Opts {
            labels_path,
            playlist,
            files,
            cache_dir,
            folds,
            json_out,
        }))
    }
}

/// One label matched to a successfully decoded+analyzed audio file.
struct TrackEval {
    hash: String,
    file_name: String,
    intro_true: u32,
    intro_ok: Vec<u32>,
    intro_pred: u32,
    intro_low_conf: bool,
    intro_cost: f64,
    /// Structural-boundary label (not `TrackAnalysis::outro_bars`).
    outro_true: u32,
    outro_ok: Vec<u32>,
    /// `TrackAnalysis::outro_structure_bars` (the analyzer's own structural
    /// boundary estimate, no lead-in). See the module-level comment.
    outro_pred: u32,
    outro_low_conf: bool,
    outro_cost: f64,
    intro_bpm: f64,
    outro_bpm: f64,
    grid_suspect: bool,
}

fn run(opts: &Opts) -> Result<(), String> {
    let labels = load_labels(&opts.labels_path)
        .map_err(|e| format!("loading '{}': {e}", opts.labels_path.display()))?;
    let by_hash: HashMap<String, SectionLabel> =
        labels.iter().cloned().map(|l| (l.hash.clone(), l)).collect();
    if by_hash.len() != labels.len() {
        eprintln!(
            "warning: {} duplicate content_hash rows in labels file (later rows win)",
            labels.len() - by_hash.len()
        );
    }

    let mut files: Vec<PathBuf> = Vec::new();
    if let Some(playlist) = &opts.playlist {
        files.extend(load_playlist(playlist)?);
    }
    files.extend(opts.files.iter().cloned());
    if files.is_empty() {
        return Err("no input audio given (use -l PLAYLIST or trailing FILE args)".to_string());
    }

    let mut hash_errors = 0usize;
    let mut decode_errors = 0usize;
    let mut analyze_errors = 0usize;
    let mut unlabeled = 0usize;
    let mut seen_hashes: Vec<String> = Vec::new();
    let mut records: Vec<TrackEval> = Vec::new();

    for path in &files {
        let hash = match cache::content_hash(path) {
            Ok(h) => h,
            Err(e) => {
                eprintln!("warning: cannot hash '{}': {e}", path.display());
                hash_errors += 1;
                continue;
            }
        };
        seen_hashes.push(hash.clone());

        let Some(label) = by_hash.get(&hash) else {
            unlabeled += 1;
            continue;
        };

        let buffer = match decode::decode_file(path) {
            Ok(b) => b,
            Err(e) => {
                eprintln!("warning: cannot decode '{}': {e}", path.display());
                decode_errors += 1;
                continue;
            }
        };

        let analysis_result = match &opts.cache_dir {
            Some(dir) => cache::get_or_analyze(path, dir, &buffer),
            None => {
                let file_name = path
                    .file_name()
                    .and_then(|s| s.to_str())
                    .unwrap_or("unknown");
                analysis::analyze(&buffer, file_name)
            }
        };
        let a = match analysis_result {
            Ok(a) => a,
            Err(e) => {
                eprintln!("warning: cannot analyze '{}': {e}", path.display());
                analyze_errors += 1;
                continue;
            }
        };

        let intro_ok = label.intro_ok_set();
        let outro_ok = label.outro_ok_set();
        let outro_pred = a.outro_structure_bars;
        let grid_suspect = !(GRID_BPM_MIN..=GRID_BPM_MAX).contains(&a.intro_bpm)
            || !(GRID_BPM_MIN..=GRID_BPM_MAX).contains(&a.outro_bpm)
            || (a.intro_bpm - a.outro_bpm).abs() >= GRID_BPM_DRIFT_MAX;

        let intro_cost = section_cost(
            label.intro_best,
            a.intro_bars,
            &intro_ok,
            INTRO_UNDERESTIMATE_WEIGHT,
            INTRO_OVERESTIMATE_WEIGHT,
        );
        let outro_cost = section_cost(
            label.outro_best,
            outro_pred,
            &outro_ok,
            OUTRO_UNDERESTIMATE_WEIGHT,
            OUTRO_OVERESTIMATE_WEIGHT,
        );

        records.push(TrackEval {
            hash: hash.clone(),
            file_name: label.file_name.clone(),
            intro_true: label.intro_best,
            intro_ok,
            intro_pred: a.intro_bars,
            intro_low_conf: a.intro_bars_low_confidence,
            intro_cost,
            outro_true: label.outro_best,
            outro_ok,
            outro_pred,
            outro_low_conf: a.outro_bars_low_confidence,
            outro_cost,
            intro_bpm: a.intro_bpm,
            outro_bpm: a.outro_bpm,
            grid_suspect,
        });
    }

    let seen: std::collections::HashSet<&str> = seen_hashes.iter().map(|s| s.as_str()).collect();
    let missing_audio: Vec<&SectionLabel> = labels
        .iter()
        .filter(|l| !seen.contains(l.hash.as_str()))
        .collect();

    println!("=== eval_sections ===");
    println!("labels file:      {}", opts.labels_path.display());
    println!("labels loaded:    {}", labels.len());
    println!("audio files given:{:>4}", files.len());
    println!("evaluated:        {}", records.len());
    println!("unlabeled (skip): {unlabeled}");
    println!("missing audio:    {} (labeled, no matching file given)", missing_audio.len());
    if !missing_audio.is_empty() {
        for l in &missing_audio {
            println!("  - {} ({})", l.file_name, l.hash);
        }
    }
    println!("hash errors:      {hash_errors}");
    println!("decode errors:    {decode_errors}");
    println!("analyze errors:   {analyze_errors}");
    println!();

    if records.is_empty() {
        println!("no evaluated tracks; nothing to report.");
        return write_json_if_requested(
            opts,
            &records,
            &missing_audio,
            labels.len(),
            files.len(),
            unlabeled,
            hash_errors,
            decode_errors,
            analyze_errors,
        );
    }

    let grid_suspects: Vec<&TrackEval> = records.iter().filter(|r| r.grid_suspect).collect();
    println!(
        "grid-suspicious tracks (BPM outside [{GRID_BPM_MIN},{GRID_BPM_MAX}] or intro/outro drift >= {GRID_BPM_DRIFT_MAX}): {}",
        grid_suspects.len()
    );
    for r in &grid_suspects {
        println!(
            "  - {} (intro_bpm={:.2} outro_bpm={:.2})",
            r.file_name, r.intro_bpm, r.outro_bpm
        );
    }
    println!();

    let all_refs: Vec<&TrackEval> = records.iter().collect();
    let sane_refs: Vec<&TrackEval> = records.iter().filter(|r| !r.grid_suspect).collect();

    println!("---- All evaluated tracks ----");
    print_section_report(&all_refs);
    println!();
    println!("---- Excluding grid-suspicious tracks ----");
    print_section_report(&sane_refs);
    println!();

    print_error_list("INTRO", &records, |r| {
        (r.intro_true, r.intro_pred, &r.intro_ok, r.intro_cost)
    });
    println!();
    print_error_list("OUTRO", &records, |r| {
        (r.outro_true, r.outro_pred, &r.outro_ok, r.outro_cost)
    });
    println!();

    if opts.folds > 1 {
        print_folds(&records, opts.folds);
        println!();
    }

    write_json_if_requested(
        opts,
        &records,
        &missing_audio,
        labels.len(),
        files.len(),
        unlabeled,
        hash_errors,
        decode_errors,
        analyze_errors,
    )
}

/// Asymmetric DJ-severity cost. `ok` entries always cost 0 regardless of
/// direction (a value the label considers acceptable is, by definition, not
/// an error worth penalizing).
fn section_cost(true_v: u32, pred_v: u32, ok: &[u32], underestimate_w: f64, overestimate_w: f64) -> f64 {
    if ok.contains(&pred_v) {
        return 0.0;
    }
    let d = pred_v as i64 - true_v as i64;
    let w = if d < 0 { underestimate_w } else { overestimate_w };
    w * (d.unsigned_abs() as f64) / COST_NORM_BARS
}

// ---------------------------------------------------------------------
// Metrics

struct SideMetrics {
    n: usize,
    exact_accuracy: f64,
    tolerant_accuracy: f64,
    mean_cost: f64,
    low_conf_precision: Option<f64>,
    low_conf_recall: Option<f64>,
}

/// `(true, pred, ok_set, cost, low_confidence)` view of one side of one track.
type SidePoint<'a> = (u32, u32, &'a [u32], f64, bool);

fn compute_side_metrics(points: &[SidePoint]) -> SideMetrics {
    let n = points.len();
    if n == 0 {
        return SideMetrics {
            n: 0,
            exact_accuracy: f64::NAN,
            tolerant_accuracy: f64::NAN,
            mean_cost: f64::NAN,
            low_conf_precision: None,
            low_conf_recall: None,
        };
    }
    let exact = points.iter().filter(|(t, p, ..)| t == p).count();
    let tolerant = points.iter().filter(|(_, p, ok, ..)| ok.contains(p)).count();
    let total_cost: f64 = points.iter().map(|(.., c, _)| c).sum();

    let flagged = points.iter().filter(|(.., low)| *low).count();
    let wrong = points.iter().filter(|(_, p, ok, ..)| !ok.contains(p)).count();
    let true_positive = points
        .iter()
        .filter(|(_, p, ok, _, low)| *low && !ok.contains(p))
        .count();
    let precision = if flagged > 0 {
        Some(true_positive as f64 / flagged as f64)
    } else {
        None
    };
    let recall = if wrong > 0 {
        Some(true_positive as f64 / wrong as f64)
    } else {
        None
    };

    SideMetrics {
        n,
        exact_accuracy: exact as f64 / n as f64,
        tolerant_accuracy: tolerant as f64 / n as f64,
        mean_cost: total_cost / n as f64,
        low_conf_precision: precision,
        low_conf_recall: recall,
    }
}

struct ConfusionMatrix {
    labels: Vec<String>,
    counts: Vec<Vec<u32>>,
}

fn bucket_index(v: u32) -> usize {
    CANDIDATE_BARS
        .iter()
        .position(|&c| c == v)
        .unwrap_or(CANDIDATE_BARS.len())
}

fn build_confusion(pairs: &[(u32, u32)]) -> ConfusionMatrix {
    let mut labels: Vec<String> = CANDIDATE_BARS.iter().map(|v| v.to_string()).collect();
    labels.push("other".to_string());
    let dim = labels.len();
    let mut counts = vec![vec![0u32; dim]; dim];
    for &(t, p) in pairs {
        counts[bucket_index(t)][bucket_index(p)] += 1;
    }
    ConfusionMatrix { labels, counts }
}

impl ConfusionMatrix {
    fn print(&self, title: &str) {
        println!("  {title} confusion matrix (rows=true, cols=predicted):");
        print!("      ");
        for l in &self.labels {
            print!("{l:>6}");
        }
        println!();
        for (i, row_label) in self.labels.iter().enumerate() {
            print!("  {row_label:>4}");
            for &c in &self.counts[i] {
                print!("{c:>6}");
            }
            println!();
        }
    }
}

fn side_points<'a, F>(records: &'a [&'a TrackEval], f: F) -> Vec<SidePoint<'a>>
where
    F: Fn(&'a TrackEval) -> SidePoint<'a>,
{
    records.iter().map(|r| f(r)).collect()
}

fn print_section_report(records: &[&TrackEval]) {
    let intro_points = side_points(records, |r| {
        (r.intro_true, r.intro_pred, r.intro_ok.as_slice(), r.intro_cost, r.intro_low_conf)
    });
    let outro_points = side_points(records, |r| {
        (r.outro_true, r.outro_pred, r.outro_ok.as_slice(), r.outro_cost, r.outro_low_conf)
    });
    let mut combined_points = intro_points.clone();
    combined_points.extend(outro_points.iter().cloned());

    let intro_m = compute_side_metrics(&intro_points);
    let outro_m = compute_side_metrics(&outro_points);
    let combined_m = compute_side_metrics(&combined_points);

    print_side_metrics("INTRO", &intro_m);
    print_side_metrics("OUTRO", &outro_m);
    print_side_metrics("COMBINED", &combined_m);

    let intro_pairs: Vec<(u32, u32)> = records.iter().map(|r| (r.intro_true, r.intro_pred)).collect();
    let outro_pairs: Vec<(u32, u32)> = records.iter().map(|r| (r.outro_true, r.outro_pred)).collect();
    build_confusion(&intro_pairs).print("INTRO");
    build_confusion(&outro_pairs).print("OUTRO");
}

fn print_side_metrics(name: &str, m: &SideMetrics) {
    let fmt_opt = |v: Option<f64>| v.map(|x| format!("{:.3}", x)).unwrap_or_else(|| "n/a".to_string());
    println!(
        "  {name:<8} n={:<4} exact={:.3} tolerant={:.3} mean_cost={:.4} low_conf_precision={} low_conf_recall={}",
        m.n, m.exact_accuracy, m.tolerant_accuracy, m.mean_cost, fmt_opt(m.low_conf_precision), fmt_opt(m.low_conf_recall)
    );
}

fn print_error_list<'a, F>(name: &str, records: &'a [TrackEval], f: F)
where
    F: Fn(&'a TrackEval) -> (u32, u32, &'a Vec<u32>, f64),
{
    let mut rows: Vec<(&'a TrackEval, u32, u32, i64, f64)> = records
        .iter()
        .filter_map(|r| {
            let (t, p, ok, cost) = f(r);
            if ok.contains(&p) {
                None
            } else {
                Some((r, t, p, p as i64 - t as i64, cost))
            }
        })
        .collect();
    rows.sort_by(|a, b| b.3.abs().cmp(&a.3.abs()));

    println!("{name} errors (outside ok set), sorted by |diff| desc: {} of {}", rows.len(), records.len());
    for (r, t, p, d, cost) in &rows {
        println!(
            "  {:<50} true={:<4} pred={:<4} diff={:<+4} cost={:.4}",
            r.file_name, t, p, d, cost
        );
    }
}

fn fold_of(hash: &str, folds: u32) -> u32 {
    let prefix: String = hash.chars().filter(|c| c.is_ascii_hexdigit()).take(8).collect();
    let n = u32::from_str_radix(&prefix, 16).unwrap_or(0);
    n % folds
}

fn print_folds(records: &[TrackEval], folds: u32) {
    println!("---- {folds}-fold breakdown (hash-stable split, no training) ----");
    for fold in 0..folds {
        let subset: Vec<&TrackEval> = records.iter().filter(|r| fold_of(&r.hash, folds) == fold).collect();
        print!("  fold {fold}: ");
        if subset.is_empty() {
            println!("n=0");
            continue;
        }
        let intro_points = side_points(&subset, |r| {
            (r.intro_true, r.intro_pred, r.intro_ok.as_slice(), r.intro_cost, r.intro_low_conf)
        });
        let outro_points = side_points(&subset, |r| {
            (r.outro_true, r.outro_pred, r.outro_ok.as_slice(), r.outro_cost, r.outro_low_conf)
        });
        let intro_m = compute_side_metrics(&intro_points);
        let outro_m = compute_side_metrics(&outro_points);
        println!(
            "n={} intro_exact={:.3} intro_tolerant={:.3} outro_exact={:.3} outro_tolerant={:.3}",
            subset.len(), intro_m.exact_accuracy, intro_m.tolerant_accuracy, outro_m.exact_accuracy, outro_m.tolerant_accuracy
        );
    }
}

// ---------------------------------------------------------------------
// Playlist loading (minimal local reimplementation; funkot-core has no
// dependency on funkot-cli, so this can't reuse funkot_cli::playlist).

fn load_playlist(path: &Path) -> Result<Vec<PathBuf>, String> {
    let contents = fs::read_to_string(path)
        .map_err(|e| format!("failed to read playlist '{}': {e}", path.display()))?;
    let base = path
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    let mut out = Vec::new();
    for line in contents.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let p = Path::new(line);
        out.push(if p.is_absolute() { p.to_path_buf() } else { base.join(p) });
    }
    Ok(out)
}

// ---------------------------------------------------------------------
// JSON export

#[derive(Serialize)]
struct SideMetricsJson {
    n: usize,
    exact_accuracy: Option<f64>,
    tolerant_accuracy: Option<f64>,
    mean_cost: Option<f64>,
    low_confidence_precision: Option<f64>,
    low_confidence_recall: Option<f64>,
}

impl From<&SideMetrics> for SideMetricsJson {
    fn from(m: &SideMetrics) -> Self {
        let opt = |v: f64| if v.is_nan() { None } else { Some(v) };
        SideMetricsJson {
            n: m.n,
            exact_accuracy: opt(m.exact_accuracy),
            tolerant_accuracy: opt(m.tolerant_accuracy),
            mean_cost: opt(m.mean_cost),
            low_confidence_precision: m.low_conf_precision,
            low_confidence_recall: m.low_conf_recall,
        }
    }
}

#[derive(Serialize)]
struct ConfusionJson {
    labels: Vec<String>,
    counts: Vec<Vec<u32>>,
}

impl From<&ConfusionMatrix> for ConfusionJson {
    fn from(c: &ConfusionMatrix) -> Self {
        ConfusionJson {
            labels: c.labels.clone(),
            counts: c.counts.clone(),
        }
    }
}

#[derive(Serialize)]
struct SectionReportJson {
    intro: SideMetricsJson,
    outro: SideMetricsJson,
    combined: SideMetricsJson,
    intro_confusion: ConfusionJson,
    outro_confusion: ConfusionJson,
}

fn section_report_json(records: &[&TrackEval]) -> SectionReportJson {
    let intro_points = side_points(records, |r| {
        (r.intro_true, r.intro_pred, r.intro_ok.as_slice(), r.intro_cost, r.intro_low_conf)
    });
    let outro_points = side_points(records, |r| {
        (r.outro_true, r.outro_pred, r.outro_ok.as_slice(), r.outro_cost, r.outro_low_conf)
    });
    let mut combined_points = intro_points.clone();
    combined_points.extend(outro_points.iter().cloned());

    let intro_pairs: Vec<(u32, u32)> = records.iter().map(|r| (r.intro_true, r.intro_pred)).collect();
    let outro_pairs: Vec<(u32, u32)> = records.iter().map(|r| (r.outro_true, r.outro_pred)).collect();

    SectionReportJson {
        intro: (&compute_side_metrics(&intro_points)).into(),
        outro: (&compute_side_metrics(&outro_points)).into(),
        combined: (&compute_side_metrics(&combined_points)).into(),
        intro_confusion: (&build_confusion(&intro_pairs)).into(),
        outro_confusion: (&build_confusion(&outro_pairs)).into(),
    }
}

#[derive(Serialize)]
struct ErrorRowJson {
    file_name: String,
    hash: String,
    true_bars: u32,
    pred_bars: u32,
    diff: i64,
    cost: f64,
}

fn error_rows_json<'a, F>(records: &'a [TrackEval], f: F) -> Vec<ErrorRowJson>
where
    F: Fn(&'a TrackEval) -> (u32, u32, &'a Vec<u32>, f64),
{
    let mut rows: Vec<ErrorRowJson> = records
        .iter()
        .filter_map(|r| {
            let (t, p, ok, cost) = f(r);
            if ok.contains(&p) {
                None
            } else {
                Some(ErrorRowJson {
                    file_name: r.file_name.clone(),
                    hash: r.hash.clone(),
                    true_bars: t,
                    pred_bars: p,
                    diff: p as i64 - t as i64,
                    cost,
                })
            }
        })
        .collect();
    rows.sort_by(|a, b| b.diff.abs().cmp(&a.diff.abs()));
    rows
}

#[derive(Serialize)]
struct FoldJson {
    fold: u32,
    n: usize,
    intro: SideMetricsJson,
    outro: SideMetricsJson,
}

#[derive(Serialize)]
struct CountsJson {
    labels_loaded: usize,
    audio_files_given: usize,
    evaluated: usize,
    unlabeled: usize,
    missing_audio: usize,
    hash_errors: usize,
    decode_errors: usize,
    analyze_errors: usize,
}

#[derive(Serialize)]
struct ReportJson {
    counts: CountsJson,
    missing_audio: Vec<String>,
    grid_suspects: Vec<String>,
    all: SectionReportJson,
    grid_sane_only: SectionReportJson,
    errors_intro: Vec<ErrorRowJson>,
    errors_outro: Vec<ErrorRowJson>,
    folds: Vec<FoldJson>,
}

#[allow(clippy::too_many_arguments)]
fn write_json_if_requested(
    opts: &Opts,
    records: &[TrackEval],
    missing_audio: &[&SectionLabel],
    labels_loaded: usize,
    audio_files_given: usize,
    unlabeled: usize,
    hash_errors: usize,
    decode_errors: usize,
    analyze_errors: usize,
) -> Result<(), String> {
    let Some(json_path) = &opts.json_out else {
        return Ok(());
    };

    let all_refs: Vec<&TrackEval> = records.iter().collect();
    let sane_refs: Vec<&TrackEval> = records.iter().filter(|r| !r.grid_suspect).collect();
    let grid_suspects: Vec<String> = records
        .iter()
        .filter(|r| r.grid_suspect)
        .map(|r| r.file_name.clone())
        .collect();

    let folds = if opts.folds > 1 {
        (0..opts.folds)
            .map(|fold| {
                let subset: Vec<&TrackEval> =
                    records.iter().filter(|r| fold_of(&r.hash, opts.folds) == fold).collect();
                let intro_points = side_points(&subset, |r| {
                    (r.intro_true, r.intro_pred, r.intro_ok.as_slice(), r.intro_cost, r.intro_low_conf)
                });
                let outro_points = side_points(&subset, |r| {
                    (r.outro_true, r.outro_pred, r.outro_ok.as_slice(), r.outro_cost, r.outro_low_conf)
                });
                FoldJson {
                    fold,
                    n: subset.len(),
                    intro: (&compute_side_metrics(&intro_points)).into(),
                    outro: (&compute_side_metrics(&outro_points)).into(),
                }
            })
            .collect()
    } else {
        Vec::new()
    };

    let report = ReportJson {
        counts: CountsJson {
            labels_loaded,
            audio_files_given,
            evaluated: records.len(),
            unlabeled,
            missing_audio: missing_audio.len(),
            hash_errors,
            decode_errors,
            analyze_errors,
        },
        missing_audio: missing_audio.iter().map(|l| l.file_name.clone()).collect(),
        grid_suspects,
        all: section_report_json(&all_refs),
        grid_sane_only: section_report_json(&sane_refs),
        errors_intro: error_rows_json(records, |r| (r.intro_true, r.intro_pred, &r.intro_ok, r.intro_cost)),
        errors_outro: error_rows_json(records, |r| (r.outro_true, r.outro_pred, &r.outro_ok, r.outro_cost)),
        folds,
    };

    let json = serde_json::to_string_pretty(&report).map_err(|e| format!("serialize JSON report: {e}"))?;
    fs::write(json_path, json).map_err(|e| format!("write '{}': {e}", json_path.display()))?;
    println!("wrote JSON report to {}", json_path.display());
    Ok(())
}
