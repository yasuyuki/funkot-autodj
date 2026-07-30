//! Where does `--label-sections` put its click grid, and how far did the
//! beat-phase lock have to move it?
//!
//! Prints, per track and per candidate, the nominal boundary (analysis-marker
//! arithmetic), the locked boundary actually used, and the difference in
//! beats. Headless: no audio device, no WAVs.
//!
//! ```sh
//! ./dev.sh cargo run -p funkot-cli --release --example click_phase_diag -- \
//!   --cache-dir funkot-cache testdata/*.flac
//! ```

use std::path::{Path, PathBuf};

use funkot_cli::label_session::{
    bar_frames_for, boundary_frame_on_grid, grid_bar_frames, locked_boundary_frame, Side,
    NORMAL_HALF_WIDTH_BARS,
};
use funkot_core::{cache, decode::decode_file};

fn main() {
    let mut args = std::env::args().skip(1);
    let mut cache_dir = PathBuf::from("funkot-cache");
    let mut paths: Vec<PathBuf> = Vec::new();
    while let Some(a) = args.next() {
        if a == "--cache-dir" {
            cache_dir = PathBuf::from(args.next().expect("--cache-dir needs a value"));
        } else {
            paths.push(PathBuf::from(a));
        }
    }
    if paths.is_empty() {
        eprintln!("usage: click_phase_diag [--cache-dir DIR] TRACK...");
        std::process::exit(2);
    }
    for p in &paths {
        if let Err(e) = report(p, &cache_dir) {
            eprintln!("{}: {e}", p.display());
        }
    }
}

fn report(path: &Path, cache_dir: &Path) -> Result<(), Box<dyn std::error::Error>> {
    let buf = decode_file(path)?;
    let analysis = cache::get_or_analyze(path, cache_dir, &buf)?;
    println!(
        "{}\n  sr={} total={} fd={} intro_bpm={:.4} outro_bpm={:.4} \
         intro_bars={} outro_bars={} outro_structure_bars={}",
        path.display(),
        analysis.sample_rate,
        analysis.total_frames,
        analysis.first_downbeat,
        analysis.intro_bpm,
        analysis.outro_bpm,
        analysis.intro_bars,
        analysis.outro_bars,
        analysis.outro_structure_bars,
    );
    // How far the file end sits from the intro downbeat grid: the phase the
    // outro candidates inherit when nothing corrects them.
    let beat_i = bar_frames_for(&analysis, Side::Intro) / 4.0;
    let end_phase =
        (analysis.total_frames - analysis.first_downbeat) as f64 / beat_i % 1.0;
    let end_phase = if end_phase > 0.5 {
        end_phase - 1.0
    } else {
        end_phase
    };
    println!("  file end vs intro beat grid: {end_phase:+.4} beat");

    for side in [Side::Intro, Side::Outro] {
        // The grid the clicks are actually built on: for the outro that is
        // the refined period, so `shift` keeps meaning "how far off the
        // music's beats was the position we were about to click on".
        let grid = grid_bar_frames(&buf, &analysis, side);
        let beat = grid / 4.0;
        if side == Side::Outro {
            println!(
                "  outro period refit: {:.3} -> {:.3} frames/beat ({:+.4}%)",
                bar_frames_for(&analysis, side) / 4.0,
                beat,
                (grid / bar_frames_for(&analysis, side) - 1.0) * 100.0,
            );
        }
        for &bars in side.candidates() {
            let nominal = boundary_frame_on_grid(&analysis, side, bars, grid);
            let locked = locked_boundary_frame(
                &buf,
                &analysis,
                side,
                bars,
                NORMAL_HALF_WIDTH_BARS,
            );
            println!(
                "  {:>5} {bars:>3}bars nominal={nominal:>10} locked={locked:>10} \
                 shift={:+.4} beat",
                side.label(),
                (locked - nominal) as f64 / beat,
            );
        }
    }
    Ok(())
}
