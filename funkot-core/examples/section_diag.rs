//! Per-bar head/tail feature dump for diagnosing intro/outro length detection.
//!
//! Usage (inside the dev container):
//!   cargo run -p funkot-core --example section_diag --release -- \
//!     [--cache-dir DIR] [--no-bars] [--new-features] testdata/*.flac
//!
//! Prints mid/high ratio, RMS, and absolute HF energy per bar for the first
//! and last ~112 bars of each track, plus current analysis results.
//! With `--cache-dir`, also writes analysis JSON via the normal cache API.
//! With `--new-features`, also prints the Stage 2 spectral-frontend columns
//! (tonality / voiced fraction / onset density / dominant chroma pitch
//! class) and derived structure signals (SSM novelty / loopiness /
//! prefix-model distance) next to the legacy columns. `analyze()`'s output
//! is identical either way — this only adds diagnostic columns.

use std::path::PathBuf;
use std::time::Instant;

use funkot_core::analysis::{analyze, diagnose_section_bars_ext, BarDiag};
use funkot_core::cache;
use funkot_core::decode;
use funkot_core::features::BarFeatures;
use funkot_core::structure::{self, StructureSignals, DEFAULT_PREFIX_BARS};

fn main() {
    let mut cache_dir: Option<PathBuf> = None;
    let mut dump_bars = true;
    let mut new_features = false;
    let mut files: Vec<PathBuf> = Vec::new();
    let mut args = std::env::args().skip(1).peekable();
    while let Some(arg) = args.next() {
        if arg == "--cache-dir" {
            cache_dir = Some(PathBuf::from(
                args.next().expect("--cache-dir needs a path"),
            ));
        } else if arg == "--no-bars" {
            dump_bars = false;
        } else if arg == "--new-features" {
            new_features = true;
        } else if arg.starts_with('-') {
            eprintln!("unknown flag: {arg}");
            std::process::exit(2);
        } else {
            files.push(PathBuf::from(arg));
        }
    }
    if files.is_empty() {
        eprintln!(
            "usage: section_diag [--cache-dir DIR] [--no-bars] [--new-features] <audio-file>..."
        );
        std::process::exit(2);
    }

    for path in &files {
        println!("=== {} ===", path.display());
        let buf = match decode::decode_file(path) {
            Ok(b) => b,
            Err(e) => {
                eprintln!("  decode failed: {e}");
                continue;
            }
        };
        let name = path
            .file_name()
            .and_then(|s| s.to_str())
            .unwrap_or("unknown");

        let t_analyze = Instant::now();
        let analysis = match analyze(&buf, name) {
            Ok(a) => {
                let elapsed = t_analyze.elapsed();
                println!(
                    "  analyze: intro_bpm={:.3} outro_bpm={:.3} intro_bars={} (low={}) outro_bars={} (low={}) any_low={} first_downbeat={} [{:.3}s]",
                    a.intro_bpm,
                    a.outro_bpm,
                    a.intro_bars,
                    a.intro_bars_low_confidence,
                    a.outro_bars,
                    a.outro_bars_low_confidence,
                    a.bars_estimated_low_confidence,
                    a.first_downbeat,
                    elapsed.as_secs_f64(),
                );
                a
            }
            Err(e) => {
                eprintln!("  analyze failed: {e}");
                continue;
            }
        };

        if let Some(dir) = &cache_dir {
            match cache::content_hash(path)
                .and_then(|h| cache::store(dir, &h, &analysis).map(|()| h))
            {
                Ok(h) => println!("  cached -> {}/{h}.json", dir.display()),
                Err(e) => eprintln!("  cache store failed: {e}"),
            }
        }

        if dump_bars {
            let t_diag = Instant::now();
            match diagnose_section_bars_ext(&buf, new_features) {
                Ok(diag) => {
                    let elapsed = t_diag.elapsed();
                    println!(
                        "  diagnose_section_bars_ext(new_features={new_features}): [{:.3}s]",
                        elapsed.as_secs_f64()
                    );
                    print_side("INTRO (forward from first downbeat)", &diag.intro, new_features);
                    if let Some(s) = &diag.intro_structure {
                        println!(
                            "  structure signals cover only the {} window-covered bars (legacy columns: {} bars)",
                            diag.intro_covered,
                            diag.intro.len(),
                        );
                        print_structure("INTRO", s);
                        let feats = covered_features(&diag.intro, diag.intro_covered);
                        print_dim_breakdown("INTRO", &structure::combined_vectors(&feats));
                    }
                    print_side("OUTRO (backward from end)", &diag.outro, new_features);
                    if let Some(s) = &diag.outro_structure {
                        println!(
                            "  structure signals cover only the {} window-covered bars (legacy columns: {} bars)",
                            diag.outro_covered,
                            diag.outro.len(),
                        );
                        print_structure("OUTRO", s);
                        let feats = covered_features(&diag.outro, diag.outro_covered);
                        print_dim_breakdown("OUTRO", &structure::combined_vectors(&feats));
                    }
                }
                Err(e) => eprintln!("  diagnose failed: {e}"),
            }
        }
        println!();
    }
}

fn print_side(label: &str, rows: &[BarDiag], new_features: bool) {
    println!("  -- {label} --");
    if new_features {
        println!(
            "  bar | midhi_ratio | rms_db | hf_db | centroid | tonality | voiced% | onset_d | chroma_pc"
        );
    } else {
        println!("  bar | midhi_ratio | rms_db | hf_db | centroid");
    }
    for row in rows {
        if new_features {
            let nf = row.new_features;
            let (tonality, voiced, onset_d, chroma_pc) = match nf {
                Some(f) => {
                    let (pc, _) = f
                        .chroma
                        .iter()
                        .enumerate()
                        .max_by(|a, b| a.1.partial_cmp(b.1).unwrap())
                        .unwrap_or((0, &0.0));
                    (f.tonality, f.voiced_frac * 100.0, f.onset_density, pc)
                }
                None => (0.0, 0.0, 0.0, 0),
            };
            println!(
                "  {:>3} | {:11.4} | {:6.1} | {:5.1} | {:8.0} | {:8.2} | {:6.1} | {:7.5} | {:>9}",
                row.bar_index,
                row.midhigh_ratio,
                row.rms_db,
                row.hf_db,
                row.centroid_hz,
                tonality,
                voiced,
                onset_d,
                chroma_pc,
            );
        } else {
            println!(
                "  {:>3} | {:11.4} | {:6.1} | {:5.1} | {:8.0}",
                row.bar_index, row.midhigh_ratio, row.rms_db, row.hf_db, row.centroid_hz
            );
        }
    }
}

/// Leading `covered` bars' Stage 2 features, in the same order `structure::compute`
/// consumed them.
fn covered_features(rows: &[BarDiag], covered: usize) -> Vec<BarFeatures> {
    rows.iter()
        .take(covered)
        .filter_map(|r| r.new_features)
        .collect()
}

/// Per-dimension breakdown of the [`structure::mahalanobis_from_prefix`] model
/// over `combined` (same 21-dim vectors, same prefix bars, same variance
/// floor). `prefix_mean`/`prefix_var` are the population mean/variance over
/// the first `DEFAULT_PREFIX_BARS` bars; `floored` marks dimensions where the
/// raw variance was below the `1e-6` floor `mahalanobis_from_prefix` applies;
/// `mean_share` is each dimension's average share of `diff^2/var` across all
/// covered bars (each bar's 21 shares sum to 1, then averaged over bars — so
/// the printed column also sums to ~1.0). Sorted by `mean_share` descending.
fn print_dim_breakdown(label: &str, combined: &[Vec<f64>]) {
    const DIM_NAMES: [&str; 21] = [
        "band_db[0]",
        "band_db[1]",
        "band_db[2]",
        "band_db[3]",
        "band_db[4]",
        "band_db[5]",
        "band_db[6]",
        "chroma[0]",
        "chroma[1]",
        "chroma[2]",
        "chroma[3]",
        "chroma[4]",
        "chroma[5]",
        "chroma[6]",
        "chroma[7]",
        "chroma[8]",
        "chroma[9]",
        "chroma[10]",
        "chroma[11]",
        "tonality",
        "voiced",
    ];

    let n = combined.len();
    if n == 0 {
        return;
    }
    let dim = combined[0].len();
    let p = DEFAULT_PREFIX_BARS.clamp(1, n);

    let mut mean = vec![0.0f64; dim];
    for f in &combined[..p] {
        for d in 0..dim {
            mean[d] += f[d];
        }
    }
    for m in &mut mean {
        *m /= p as f64;
    }

    let mut raw_var = vec![0.0f64; dim];
    for f in &combined[..p] {
        for d in 0..dim {
            let diff = f[d] - mean[d];
            raw_var[d] += diff * diff;
        }
    }
    for v in &mut raw_var {
        *v /= p as f64;
    }
    let var: Vec<f64> = raw_var.iter().map(|v| v.max(1e-6)).collect();

    // mean_share: per-bar diff^2/var normalized to sum 1 across dims, then
    // averaged over all covered bars — same `s = diff^2 / var` term
    // `mahalanobis_from_prefix` sums before its final `/dim` + `sqrt`.
    let mut share_sum = vec![0.0f64; dim];
    for f in combined {
        let mut terms = vec![0.0f64; dim];
        let mut total = 0.0f64;
        for d in 0..dim {
            let diff = f[d] - mean[d];
            let s = diff * diff / var[d];
            terms[d] = s;
            total += s;
        }
        if total > 0.0 {
            for d in 0..dim {
                share_sum[d] += terms[d] / total;
            }
        }
    }
    let mean_share: Vec<f64> = share_sum.iter().map(|s| s / n as f64).collect();

    println!(
        "  -- {label} prefix-model dimension breakdown (prefix={p} bars, covered={n} bars) --"
    );
    println!("  dim            prefix_mean  prefix_var  floored  mean_share");
    let mut order: Vec<usize> = (0..dim).collect();
    order.sort_by(|&a, &b| {
        mean_share[b]
            .partial_cmp(&mean_share[a])
            .unwrap_or(std::cmp::Ordering::Equal)
    });
    for d in order {
        println!(
            "  {:<14} {:>11.4} {:>11.7} {:>8} {:>11.3}",
            DIM_NAMES.get(d).copied().unwrap_or("?"),
            mean[d],
            raw_var[d],
            if raw_var[d] < 1e-6 { "YES" } else { "-" },
            mean_share[d],
        );
    }
}

fn print_structure(label: &str, s: &StructureSignals) {
    println!("  -- {label} structure (SSM novelty w8/w16, loop-1/2/4, prefix-mahalanobis) --");
    println!("  bar | chroma_n8 | band_n8 | rhythm_n8 | loop1 | loop2 | loop4 | mahal");
    let n = s.chroma_novelty_w8.len();
    for i in 0..n {
        println!(
            "  {:>3} | {:9.3} | {:7.3} | {:9.3} | {:5.2} | {:5.2} | {:5.2} | {:6.2}",
            i,
            s.chroma_novelty_w8[i],
            s.band_novelty_w8[i],
            s.rhythm_novelty_w8[i],
            s.loop_1[i],
            s.loop_2[i],
            s.loop_4[i],
            s.prefix_mahalanobis[i],
        );
    }
}
