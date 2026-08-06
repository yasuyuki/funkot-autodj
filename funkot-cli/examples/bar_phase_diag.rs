//! Which beat of the bar does the `first_downbeat` grid call beat 1?
//!
//! Folds the track over the bar on the analyzer's grid and prints, per beat
//! slot, the mean RMS and the mean broadband onset flux. A grid whose slot 0
//! is the *quietest* of the four is a grid sitting on the wrong beat: dance
//! masters put the lift before the downbeat, i.e. in slot 3.
//!
//! ```sh
//! ./dev.sh cargo run -p funkot-cli --release --example bar_phase_diag -- \
//!   --cache-dir funkot-cache testdata/*.flac
//! ```

use std::path::{Path, PathBuf};

use funkot_cli::label_session::{click_grid, Side};
use funkot_core::{cache, decode::decode_file};

fn main() {
    let mut args = std::env::args().skip(1);
    let mut cache_dir = PathBuf::from("funkot-cache");
    let mut paths: Vec<PathBuf> = Vec::new();
    while let Some(a) = args.next() {
        if a == "--cache-dir" {
            cache_dir = PathBuf::from(args.next().expect("needs a value"));
        } else {
            paths.push(PathBuf::from(a));
        }
    }
    println!("quietest beat slot per 16 bars ('.' = slots within 1 dB, no verdict)");
    for p in &paths {
        if let Err(e) = report(p, &cache_dir) {
            eprintln!("{}: {e}", p.display());
        }
    }
}

fn report(path: &Path, cache_dir: &Path) -> Result<(), Box<dyn std::error::Error>> {
    let buf = decode_file(path)?;
    let analysis = cache::get_or_analyze(path, cache_dir, &buf)?;
    let grid = click_grid(&buf, &analysis, Side::Outro);
    let fd = analysis.first_downbeat as f64;
    let bar_frames = grid.bar_frames;
    let beat_frames = bar_frames / 4.0;
    let end_bar = ((grid.outro_anchor as f64 - fd) / bar_frames).round() as i64;

    let mono: Vec<f32> = buf
        .samples
        .chunks_exact(2)
        .map(|f| (f[0] + f[1]) * 0.5)
        .collect();
    // One digit per `block` bars: which beat slot is quietest there, '.' when
    // the four slots are too close for the answer to mean anything. With
    // DETAIL set, print the four levels too (relative to the block's loudest).
    let block: i64 = std::env::var("BLOCK")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(16);
    let detail = std::env::var("DETAIL").is_ok();
    const CLEAR_DB: f64 = 1.0;

    let name = path.file_stem().unwrap().to_string_lossy();
    let mut line = String::new();
    let mut b = 0i64;
    while b + block <= end_bar {
        let mut rms = [0.0f64; 4];
        for bar in b..b + block {
            for (beat, slot) in rms.iter_mut().enumerate() {
                let from = fd + bar as f64 * bar_frames + beat as f64 * beat_frames;
                *slot += mean_square(&mono, from, from + beat_frames);
            }
        }
        let db: Vec<f64> = rms.iter().map(|v| 10.0 * v.max(1e-12).log10()).collect();
        let quiet = (0..4).min_by(|&x, &y| db[x].partial_cmp(&db[y]).unwrap()).unwrap();
        let loud = (0..4).max_by(|&x, &y| db[x].partial_cmp(&db[y]).unwrap()).unwrap();
        let clear = db[loud] - db[quiet] >= CLEAR_DB;
        line.push(if clear {
            char::from(b'0' + quiet as u8)
        } else {
            '.'
        });
        if detail {
            println!(
                "  bars {b:>4}-{:<4} {:>6.2} {:>6.2} {:>6.2} {:>6.2}   quiet {}",
                b + block - 1,
                db[0] - db[loud],
                db[1] - db[loud],
                db[2] - db[loud],
                db[3] - db[loud],
                if clear { quiet.to_string() } else { "-".into() },
            );
        }
        b += block;
    }
    if detail {
        println!("{name}");
    }
    println!("{:<46} {line}", &name[..name.len().min(46)]);

    // The comparison that matters for the guide clicks: does the bar phase
    // near the outro still agree with the one near the intro?
    const WIN: i64 = 48;
    let intro = phase_of(&mono, fd, bar_frames, 4, 4 + WIN);
    let outro = phase_of(&mono, fd, bar_frames, end_bar - 4 - WIN, end_bar - 4);
    let verdict = match (intro, outro) {
        (Some((qi, _)), Some((qo, _))) if qi == 3 => {
            let shift = (qo + 4 - qi) % 4;
            format!("shift {shift:+} beat")
        }
        (Some((qi, _)), Some((qo, _))) => {
            // Same gate as `outro_beat_phase_shift`: the intro window's
            // quietest slot isn't 3, so its own relative shift can't be
            // trusted as evidence the grid moved (see that function's doc,
            // `Andai Tak Berpisah`) -- abstain, but still show what the
            // relative form would have said.
            let shift = (qo + 4 - qi) % 4;
            format!(
                "shift +0 beat (intro window {qi}, not 3 -- abstained; \
                 relative form would say {shift:+})"
            )
        }
        _ => "no verdict".to_string(),
    };
    println!(
        "    intro {} | outro {} | {verdict}",
        fmt(intro),
        fmt(outro),
    );
    Ok(())
}

/// Quietest beat slot over `[lo, hi)` bars, with its margin over the next
/// quietest, or `None` when the four slots are too close to call.
fn phase_of(mono: &[f32], fd: f64, bar_frames: f64, lo: i64, hi: i64) -> Option<(i64, f64)> {
    const CLEAR_DB: f64 = 0.8;
    let beat_frames = bar_frames / 4.0;
    let mut rms = [0.0f64; 4];
    for bar in lo.max(0)..hi {
        for (beat, slot) in rms.iter_mut().enumerate() {
            let from = fd + bar as f64 * bar_frames + beat as f64 * beat_frames;
            *slot += mean_square(mono, from, from + beat_frames);
        }
    }
    let db: Vec<f64> = rms.iter().map(|v| 10.0 * v.max(1e-12).log10()).collect();
    let quiet = (0..4).min_by(|&x, &y| db[x].partial_cmp(&db[y]).unwrap()).unwrap();
    let next = (0..4)
        .filter(|&i| i != quiet)
        .min_by(|&x, &y| db[x].partial_cmp(&db[y]).unwrap())
        .unwrap();
    let margin = db[next] - db[quiet];
    (margin >= CLEAR_DB).then_some((quiet as i64, margin))
}

fn fmt(p: Option<(i64, f64)>) -> String {
    match p {
        Some((q, m)) => format!("quiet {q} (+{m:.1} dB)"),
        None => "unclear      ".to_string(),
    }
}

fn mean_square(mono: &[f32], from: f64, to: f64) -> f64 {
    let i0 = (from.max(0.0) as usize).min(mono.len());
    let i1 = (to.max(0.0) as usize).min(mono.len());
    if i1 <= i0 {
        return 0.0;
    }
    let sum: f64 = mono[i0..i1].iter().map(|&s| f64::from(s) * f64::from(s)).sum();
    sum / (i1 - i0) as f64
}
