//! Beat-resolution view of the last bars, against the grid the clicks use.
//!
//! For each beat of the final bars this prints its position on the
//! `first_downbeat + n bars` grid, the peak broadband onset flux in that beat
//! (dB relative to the track's own typical bar, the same statistic
//! `music_end_bar` thresholds), and the beat's RMS. The line `music_end_bar`
//! picked is marked, so "did the music stop on that line or a beat after it"
//! is readable directly.
//!
//! ```sh
//! ./dev.sh cargo run -p funkot-cli --release --example end_beat_diag -- \
//!   --cache-dir funkot-cache --bars 10 testdata/TRACK.flac
//! ```

use std::path::{Path, PathBuf};

use funkot_cli::label_session::{click_grid, Side};
use funkot_core::{cache, decode::decode_file};

fn main() {
    let mut args = std::env::args().skip(1);
    let mut cache_dir = PathBuf::from("funkot-cache");
    let mut bars = 10u32;
    let mut paths: Vec<PathBuf> = Vec::new();
    while let Some(a) = args.next() {
        match a.as_str() {
            "--cache-dir" => cache_dir = PathBuf::from(args.next().expect("needs a value")),
            "--bars" => bars = args.next().expect("needs a value").parse().expect("number"),
            _ => paths.push(PathBuf::from(a)),
        }
    }
    for p in &paths {
        if let Err(e) = report(p, &cache_dir, bars) {
            eprintln!("{}: {e}", p.display());
        }
    }
}

fn report(path: &Path, cache_dir: &Path, bars: u32) -> Result<(), Box<dyn std::error::Error>> {
    let buf = decode_file(path)?;
    let analysis = cache::get_or_analyze(path, cache_dir, &buf)?;
    let grid = click_grid(&buf, &analysis, Side::Outro);
    let fd = analysis.first_downbeat as f64;
    let bar_frames = grid.bar_frames;
    let beat_frames = bar_frames / 4.0;
    let end_bar = ((grid.outro_anchor as f64 - fd) / bar_frames).round();

    let mono: Vec<f32> = buf
        .samples
        .chunks_exact(2)
        .map(|f| (f[0] + f[1]) * 0.5)
        .collect();
    let flux = funkot_core::analysis::onset_flux_envelope(&mono);
    let hop = funkot_core::analysis::ONSET_FLUX_HOP as f64;

    // Reference = median over the scanned bars of the mean of the 2nd and 3rd
    // strongest beat, i.e. what music_end_bar calls a typical bar.
    let scan_lo = (end_bar as i64 - 64).max(1);
    let mut typical: Vec<f64> = Vec::new();
    for b in scan_lo..end_bar as i64 {
        let mut beats = beat_peaks(&flux, hop, fd, b, bar_frames);
        beats.sort_by(|a, c| a.partial_cmp(c).unwrap());
        typical.push(0.5 * (beats[1] + beats[2]));
    }
    typical.sort_by(|a, c| a.partial_cmp(c).unwrap());
    let reference = typical[typical.len() / 2];

    println!(
        "{}\n  fd={fd:.0} bar_frames={bar_frames:.2} music_end_bar={end_bar} \
         total={} ({:.3} bars from fd)",
        path.display(),
        buf.frames,
        (buf.frames as f64 - fd) / bar_frames,
    );
    println!("  bar.beat   frame     flux dB   rms dB");
    let lo = end_bar as i64 - bars as i64;
    let hi = end_bar as i64 + 3;
    for b in lo..hi {
        for beat in 0..4 {
            let from = fd + b as f64 * bar_frames + f64::from(beat) * beat_frames;
            let to = from + beat_frames;
            let f_peak = flux_peak(&flux, hop, from, to);
            let rms = rms_db(&mono, from, to);
            let mark = if b == end_bar as i64 && beat == 0 {
                "  <- music_end_bar"
            } else {
                ""
            };
            println!(
                "  {b:>4}.{beat}  {:>9.0}  {:>7.1}  {:>7.1}{mark}",
                from,
                20.0 * (f_peak.max(1e-12) / reference).log10(),
                rms,
            );
        }
    }
    println!();
    Ok(())
}

fn beat_peaks(flux: &[f64], hop: f64, fd: f64, bar: i64, bar_frames: f64) -> [f64; 4] {
    let beat_frames = bar_frames / 4.0;
    let mut out = [0.0; 4];
    for (i, slot) in out.iter_mut().enumerate() {
        let from = fd + bar as f64 * bar_frames + i as f64 * beat_frames;
        *slot = flux_peak(flux, hop, from, from + beat_frames);
    }
    out
}

fn flux_peak(flux: &[f64], hop: f64, from: f64, to: f64) -> f64 {
    let i0 = ((from / hop).round().max(0.0) as usize).min(flux.len());
    let i1 = ((to / hop).round().max(0.0) as usize).min(flux.len());
    flux[i0..i1].iter().copied().fold(0.0f64, f64::max)
}

fn rms_db(mono: &[f32], from: f64, to: f64) -> f64 {
    let i0 = (from.max(0.0) as usize).min(mono.len());
    let i1 = (to.max(0.0) as usize).min(mono.len());
    if i1 <= i0 {
        return f64::NEG_INFINITY;
    }
    let sum: f64 = mono[i0..i1].iter().map(|&s| f64::from(s) * f64::from(s)).sum();
    20.0 * (sum / (i1 - i0) as f64).sqrt().max(1e-12).log10()
}
