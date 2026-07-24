//! Per-bar head/tail feature dump for diagnosing intro/outro length detection.
//!
//! Usage (inside the dev container):
//!   cargo run -p funkot-core --example section_diag --release -- \
//!     [--cache-dir DIR] [--no-bars] testdata/*.flac
//!
//! Prints mid/high ratio, RMS, and absolute HF energy per bar for the first
//! and last ~112 bars of each track, plus current analysis results.
//! With `--cache-dir`, also writes analysis JSON via the normal cache API.

use std::path::PathBuf;

use funkot_core::analysis::{analyze, diagnose_section_bars};
use funkot_core::cache;
use funkot_core::decode;

fn main() {
    let mut cache_dir: Option<PathBuf> = None;
    let mut dump_bars = true;
    let mut files: Vec<PathBuf> = Vec::new();
    let mut args = std::env::args().skip(1).peekable();
    while let Some(arg) = args.next() {
        if arg == "--cache-dir" {
            cache_dir = Some(PathBuf::from(
                args.next().expect("--cache-dir needs a path"),
            ));
        } else if arg == "--no-bars" {
            dump_bars = false;
        } else if arg.starts_with('-') {
            eprintln!("unknown flag: {arg}");
            std::process::exit(2);
        } else {
            files.push(PathBuf::from(arg));
        }
    }
    if files.is_empty() {
        eprintln!("usage: section_diag [--cache-dir DIR] [--no-bars] <audio-file>...");
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
        let analysis = match analyze(&buf, name) {
            Ok(a) => {
                println!(
                    "  analyze: intro_bpm={:.3} outro_bpm={:.3} intro_bars={} (low={}) outro_bars={} (low={}) any_low={} first_downbeat={}",
                    a.intro_bpm,
                    a.outro_bpm,
                    a.intro_bars,
                    a.intro_bars_low_confidence,
                    a.outro_bars,
                    a.outro_bars_low_confidence,
                    a.bars_estimated_low_confidence,
                    a.first_downbeat
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
            match diagnose_section_bars(&buf) {
                Ok(diag) => {
                    print_side("INTRO (forward from first downbeat)", &diag.intro);
                    print_side("OUTRO (backward from end)", &diag.outro);
                }
                Err(e) => eprintln!("  diagnose failed: {e}"),
            }
        }
        println!();
    }
}

fn print_side(label: &str, rows: &[funkot_core::analysis::BarDiag]) {
    println!("  -- {label} --");
    println!("  bar | midhi_ratio | rms_db | hf_db | centroid");
    for row in rows {
        println!(
            "  {:>3} | {:11.4} | {:6.1} | {:5.1} | {:8.0}",
            row.bar_index, row.midhigh_ratio, row.rms_db, row.hf_db, row.centroid_hz
        );
    }
}
