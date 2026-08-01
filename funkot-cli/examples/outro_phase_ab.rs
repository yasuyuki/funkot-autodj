//! Render an outro candidate's guide-click clip at each beat phase, for A/B.
//!
//! The click grid takes bar identity from `first_downbeat` and counts bars
//! forward. When a master's own bars have slipped a beat against that grid by
//! the time the outro arrives, every outro click lands on the wrong beat of
//! the bar and no tempo or end-detection fix can move it, because the anchor
//! is quantised to the propagated grid. This writes the same clip with the
//! anchor moved 0, 1, 2 and 3 beats later so the ear can say which one is on
//! the downbeat.
//!
//! ```sh
//! ./dev.sh cargo run -p funkot-cli --release --example outro_phase_ab -- \
//!   --cache-dir funkot-cache --out testdata/outro_phase_ab --bars 16 TRACK...
//! ```

use std::path::{Path, PathBuf};

use funkot_cli::label_session::{
    build_candidate_clip_on_grid, click_grid, ClickGrid, ClickOptions, Side,
    NORMAL_HALF_WIDTH_BARS,
};
use funkot_cli::wav_write::{WavFormat, WavStreamWriter};
use funkot_core::{cache, decode::decode_file};

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut args = std::env::args().skip(1);
    let mut cache_dir = PathBuf::from("funkot-cache");
    let mut out_dir = PathBuf::from("testdata/outro_phase_ab");
    let mut bars = 16u32;
    let mut paths: Vec<PathBuf> = Vec::new();
    while let Some(a) = args.next() {
        match a.as_str() {
            "--cache-dir" => cache_dir = PathBuf::from(args.next().expect("needs a value")),
            "--out" => out_dir = PathBuf::from(args.next().expect("needs a value")),
            "--bars" => bars = args.next().expect("needs a value").parse()?,
            _ => paths.push(PathBuf::from(a)),
        }
    }
    std::fs::create_dir_all(&out_dir)?;
    for p in &paths {
        render(p, &cache_dir, &out_dir, bars)?;
    }
    Ok(())
}

fn render(
    path: &Path,
    cache_dir: &Path,
    out_dir: &Path,
    bars: u32,
) -> Result<(), Box<dyn std::error::Error>> {
    let buf = decode_file(path)?;
    let analysis = cache::get_or_analyze(path, cache_dir, &buf)?;
    let grid = click_grid(&buf, &analysis, Side::Outro);
    let beat_frames = grid.bar_frames / 4.0;
    let stem = path.file_stem().and_then(|s| s.to_str()).unwrap_or("track");
    let opts = ClickOptions::default();

    for beats in 0..4i64 {
        let shifted = ClickGrid {
            bar_frames: grid.bar_frames,
            outro_anchor: grid.outro_anchor + (beats as f64 * beat_frames).round() as i64,
        };
        let clip = build_candidate_clip_on_grid(
            &buf,
            &analysis,
            Side::Outro,
            bars,
            NORMAL_HALF_WIDTH_BARS,
            &opts,
            shifted,
        );
        let p = out_dir.join(format!("{stem}_outro{bars:03}_plus{beats}beat.wav"));
        let mut w = WavStreamWriter::create(&p, buf.sample_rate, WavFormat::F32)?;
        w.write_interleaved(&clip)?;
        w.finalize()?;
        println!(
            "  {}  anchor {} (+{beats} beat)",
            p.display(),
            shifted.outro_anchor
        );
    }
    Ok(())
}
