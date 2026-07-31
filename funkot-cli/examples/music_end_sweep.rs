//! How sensitive is the outro anchor to `music_end_frame`'s two thresholds?
//!
//! Prints, per track, the bar the anchor lands on for a sweep of silence
//! thresholds and onset-rise ratios. Two masters have ear-verified answers to
//! check a candidate setting against: `Surya Groxyn - … - 03 Shuki Shuki Song`
//! must come out at 239 bars and `AntonFer - … - 09 Sakura Photograph` at 432.
//!
//! ```sh
//! ./dev.sh cargo run -p funkot-cli --release --example music_end_sweep -- \
//!   --cache-dir funkot-cache testdata/*.flac
//! ```

use std::path::PathBuf;

use funkot_cli::label_session::{
    bar_frames_for, music_end_frame_with, outro_bar_frames_refit, Side,
};
use funkot_core::{cache, decode::decode_file};

const SILENCE_DB: [f64; 8] = [-24.0, -20.0, -18.0, -16.0, -14.0, -12.0, -8.0, -6.0];
const RISE: [f64; 2] = [1.6, 2.0];
const SLACK_BARS: f64 = 0.15;

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
    for path in &paths {
        let buf = match decode_file(path) {
            Ok(b) => b,
            Err(e) => {
                eprintln!("{}: {e}", path.display());
                continue;
            }
        };
        let analysis = match cache::get_or_analyze(path, &cache_dir, &buf) {
            Ok(a) => a,
            Err(e) => {
                eprintln!("{}: {e}", path.display());
                continue;
            }
        };
        let fd = analysis.first_downbeat as f64;
        let mut line = format!(
            "{:<44}",
            path.file_stem()
                .and_then(|s| s.to_str())
                .unwrap_or("?")
                .chars()
                .rev()
                .take(44)
                .collect::<String>()
                .chars()
                .rev()
                .collect::<String>()
        );
        for &rise in RISE.iter() {
            for &db in SILENCE_DB.iter() {
                let music_end = music_end_frame_with(&buf, &analysis, db, rise);
                let bar = outro_bar_frames_refit(&buf, &analysis, music_end);
                let bar = if bar.is_finite() && bar > 1.0 {
                    bar
                } else {
                    bar_frames_for(&analysis, Side::Outro)
                };
                let anchor = ((music_end as f64 - fd) / bar - SLACK_BARS).ceil();
                line.push_str(&format!(" {anchor:>4.0}"));
            }
            line.push_str(" |");
        }
        println!("{line}");
    }
    println!(
        "\ncolumns: rise 1.6 [{}] | rise 2.0 [{}]",
        SILENCE_DB.map(|d| format!("{d:.0}dB")).join(" "),
        SILENCE_DB.map(|d| format!("{d:.0}dB")).join(" "),
    );
}
