//! Does the `first_downbeat` + constant-period grid stay on the music's beats?
//!
//! Locks a short window onto the music every few bars and prints how far the
//! grid had to move, in beats. A grid whose period is right shows noise around
//! zero for the whole track; a mis-estimated period shows a ramp that wraps at
//! ±0.5 beat, which is a whole beat of counting error per wrap.
//!
//! ```sh
//! ./dev.sh cargo run -p funkot-cli --release --example beat_drift_diag -- \
//!   --cache-dir funkot-cache --step 8 testdata/TRACK.flac
//! ```

use std::path::{Path, PathBuf};

use funkot_cli::label_session::{bar_frames_for, click_grid, lock_boundary_to_groove, Side};
use funkot_core::{cache, decode::decode_file};

fn main() {
    let mut args = std::env::args().skip(1);
    let mut cache_dir = PathBuf::from("funkot-cache");
    let mut step = 8i64;
    let mut paths: Vec<PathBuf> = Vec::new();
    while let Some(a) = args.next() {
        match a.as_str() {
            "--cache-dir" => cache_dir = PathBuf::from(args.next().expect("needs a value")),
            "--step" => step = args.next().expect("needs a value").parse().expect("number"),
            _ => paths.push(PathBuf::from(a)),
        }
    }
    for p in &paths {
        if let Err(e) = report(p, &cache_dir, step) {
            eprintln!("{}: {e}", p.display());
        }
    }
}

fn report(path: &Path, cache_dir: &Path, step: i64) -> Result<(), Box<dyn std::error::Error>> {
    let buf = decode_file(path)?;
    let analysis = cache::get_or_analyze(path, cache_dir, &buf)?;
    let grid = click_grid(&buf, &analysis, Side::Outro);
    let fd = analysis.first_downbeat as f64;
    let end_bar = ((grid.outro_anchor as f64 - fd) / grid.bar_frames).round() as i64;

    for (label, bar_frames) in [
        ("analysis intro period", bar_frames_for(&analysis, Side::Intro)),
        ("outro refit period   ", grid.bar_frames),
    ] {
        let beat_frames = bar_frames / 4.0;
        print!("{}\n  {label} ({bar_frames:.2} f/bar):", path.display());
        let mut b = 8i64;
        while b < end_bar - 4 {
            let nominal = (fd + b as f64 * bar_frames).round() as i64;
            let locked = lock_boundary_to_groove(&buf, nominal, bar_frames, 4);
            let shift = (locked - nominal) as f64 / beat_frames;
            if (b / step) % 8 == 0 {
                print!("\n    ");
            }
            print!(" {b:>4}:{shift:>+6.2}");
            b += step;
        }
        println!();
    }
    Ok(())
}
