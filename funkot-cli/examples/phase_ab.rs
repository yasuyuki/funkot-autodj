//! Render a candidate's guide-click clip at each beat phase, for A/B.
//!
//! The click grid takes bar identity from `first_downbeat` and counts bars
//! forward. When a master's own bars have slipped a beat against that grid by
//! the time the boundary arrives, every click lands on the wrong beat of the
//! bar and no tempo or end-detection fix can move it, because the anchor is
//! quantised to the propagated grid. This writes the same clip with the
//! nominal boundary moved 0, 1, 2 and 3 beats later so the ear can say which
//! one is on the downbeat, before the sub-beat lock ever runs.
//!
//! ```sh
//! ./dev.sh cargo run -p funkot-cli --release --example phase_ab -- \
//!   --cache-dir funkot-cache --out testdata/phase_ab --side outro --bars 16 TRACK...
//! ```

use std::path::{Path, PathBuf};

use funkot_cli::label_session::{
    bar_frames_for, boundary_frame_on_grid, build_candidate_clip_on_grid, click_grid,
    locked_boundary_on_grid, music_end_bar, outro_beat_phase_shift, ClickGrid, ClickOptions,
    Side, NORMAL_HALF_WIDTH_BARS,
};
use funkot_cli::wav_write::{WavFormat, WavStreamWriter};
use funkot_core::{cache, decode::decode_file, TrackAnalysis};

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut args = std::env::args().skip(1);
    let mut cache_dir = PathBuf::from("funkot-cache");
    let mut out_dir = PathBuf::from("testdata/phase_ab");
    let mut bars = 16u32;
    let mut side = Side::Outro;
    let mut offsets: Vec<f64> = vec![0.0, 1.0, 2.0, 3.0];
    let mut paths: Vec<PathBuf> = Vec::new();
    while let Some(a) = args.next() {
        match a.as_str() {
            "--cache-dir" => cache_dir = PathBuf::from(args.next().expect("needs a value")),
            "--out" => out_dir = PathBuf::from(args.next().expect("needs a value")),
            "--bars" => bars = args.next().expect("needs a value").parse()?,
            "--side" => {
                side = match args.next().expect("needs a value").as_str() {
                    "intro" => Side::Intro,
                    "outro" => Side::Outro,
                    other => panic!("--side must be intro or outro, got {other}"),
                }
            }
            "--offsets" => {
                let raw = args.next().expect("needs a value");
                offsets = raw
                    .split(',')
                    .map(|s| s.trim().parse::<f64>())
                    .collect::<Result<Vec<f64>, _>>()?;
            }
            _ => paths.push(PathBuf::from(a)),
        }
    }
    std::fs::create_dir_all(&out_dir)?;
    for p in &paths {
        render(p, &cache_dir, &out_dir, bars, side, &offsets)?;
    }
    Ok(())
}

fn render(
    path: &Path,
    cache_dir: &Path,
    out_dir: &Path,
    bars: u32,
    side: Side,
    offsets: &[f64],
) -> Result<(), Box<dyn std::error::Error>> {
    let buf = decode_file(path)?;
    let analysis = cache::get_or_analyze(path, cache_dir, &buf)?;
    let grid = click_grid(&buf, &analysis, side);
    let beat_frames = grid.bar_frames / 4.0;
    let stem = path.file_stem().and_then(|s| s.to_str()).unwrap_or("track");
    let opts = ClickOptions::default();

    match side {
        Side::Outro => {
            // What the grid already corrected for, so a listener reading
            // these clips knows whether `plus0` is the propagated grid or an
            // already-shifted one.
            let applied =
                music_end_bar(&buf, &analysis, bar_frames_for(&analysis, Side::Outro), 0)
                    .map(|end_bar| {
                        outro_beat_phase_shift(&buf, &analysis, grid.bar_frames, end_bar)
                    })
                    .unwrap_or(0);
            println!("{stem}  (grid already applies +{applied} beat)");
        }
        Side::Intro => {
            // The labeling grid never shifts the intro side by a whole beat:
            // `plus0` below is exactly the boundary `--label-sections` would
            // click on, not a corrected one.
            println!(
                "{stem}  (labeling applies no intro phase shift; plus0 is what labeling hears)"
            );
        }
    }

    for &offset in offsets {
        let shifted_analysis: Option<TrackAnalysis> = match side {
            Side::Outro => None,
            Side::Intro if offset == 0.0 => None,
            Side::Intro => {
                let mut a = analysis.clone();
                a.first_downbeat =
                    (analysis.first_downbeat as f64 + offset * beat_frames).round() as u64;
                Some(a)
            }
        };
        let clip_analysis = shifted_analysis.as_ref().unwrap_or(&analysis);

        let shifted_grid = match side {
            Side::Outro => ClickGrid {
                bar_frames: grid.bar_frames,
                outro_anchor: grid.outro_anchor + (offset * beat_frames).round() as i64,
                lock_offset: grid.lock_offset,
            },
            Side::Intro => grid,
        };

        // Both numbers must describe the clip written just below, so they come
        // from the same call `build_candidate_clip_on_grid` makes -- including
        // the branch-consensus correction, which a bare
        // `lock_boundary_to_groove` would miss. At `offset == 0` this is also
        // exactly what `examples/click_phase_diag.rs` prints, so `plus0` can be
        // cross-checked against that diagnostic verbatim.
        let nominal = boundary_frame_on_grid(clip_analysis, side, bars, shifted_grid);
        let locked = locked_boundary_on_grid(
            &buf,
            clip_analysis,
            side,
            bars,
            NORMAL_HALF_WIDTH_BARS,
            shifted_grid,
        );

        let clip = build_candidate_clip_on_grid(
            &buf,
            clip_analysis,
            side,
            bars,
            NORMAL_HALF_WIDTH_BARS,
            &opts,
            shifted_grid,
        );
        let side_label = side.label();
        let p = out_dir.join(format!("{stem}_{side_label}{bars:03}_plus{offset}beat.wav"));
        let mut w = WavStreamWriter::create(&p, buf.sample_rate, WavFormat::F32)?;
        w.write_interleaved(&clip)?;
        w.finalize()?;
        println!(
            "  {}  nominal={nominal} locked={locked} (+{offset} beat)",
            p.display()
        );
    }
    Ok(())
}
