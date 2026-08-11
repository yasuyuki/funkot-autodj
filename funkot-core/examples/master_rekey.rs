//! Prove two copies of a master are the same audio, and emit the new content key.
//!
//! `labels.tsv` / `survey.tsv` and every `funkot-cache` entry are keyed by
//! [`funkot_core::cache::content_hash`], which hashes *file bytes*. Moving the
//! real-audio test set from one container to another (FLAC in `testdata/` to
//! ALAC in the music library) therefore invalidates every key, and the hand-made
//! labels behind them cannot be regenerated. This harness is the gate: run it
//! while both copies still exist, confirm the decoded PCM is identical, and use
//! the emitted mapping to rewrite the key columns.
//!
//! Usage:
//!   cargo run -p funkot-core --example master_rekey --release -- MAP.tsv
//!
//! `MAP.tsv` is `old_path<TAB>new_path` per line; `#`-comments and blank lines
//! are ignored. Output is a TSV on stdout:
//!
//!   status  old_hash  new_hash  old_name  new_name
//!
//! `status` is `same` when sample rate, frame count and every sample agree
//! bit-for-bit. Anything else is a refusal to re-key that row: the detail goes
//! to stderr. Exit status is non-zero if any row is not `same`.

use std::path::Path;
use std::process::ExitCode;

use funkot_core::{cache, decode};

fn main() -> ExitCode {
    let mut args = std::env::args().skip(1);
    let Some(map_path) = args.next() else {
        eprintln!("usage: master_rekey MAP.tsv   (lines: old_path<TAB>new_path)");
        return ExitCode::from(2);
    };
    let map = match std::fs::read_to_string(&map_path) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("error: cannot read '{map_path}': {e}");
            return ExitCode::from(2);
        }
    };

    println!("status\told_hash\tnew_hash\told_name\tnew_name");
    let mut bad = 0u32;
    for (i, line) in map.lines().enumerate() {
        let line = line.trim_end_matches(['\r', '\n']);
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let Some((old, new)) = line.split_once('\t') else {
            eprintln!("line {}: no tab separator", i + 1);
            bad += 1;
            continue;
        };
        match row(Path::new(old), Path::new(new)) {
            Ok((old_hash, new_hash)) => println!(
                "same\t{old_hash}\t{new_hash}\t{}\t{}",
                name(Path::new(old)),
                name(Path::new(new))
            ),
            Err(why) => {
                eprintln!("{}: {why}", name(Path::new(old)));
                println!(
                    "DIFFER\t-\t-\t{}\t{}",
                    name(Path::new(old)),
                    name(Path::new(new))
                );
                bad += 1;
            }
        }
    }

    if bad > 0 {
        eprintln!("\n{bad} row(s) not proven identical — do not re-key those");
        return ExitCode::FAILURE;
    }
    ExitCode::SUCCESS
}

fn row(old: &Path, new: &Path) -> Result<(String, String), String> {
    let a = decode::decode_file(old).map_err(|e| format!("decode old: {e}"))?;
    let b = decode::decode_file(new).map_err(|e| format!("decode new: {e}"))?;

    if a.sample_rate != b.sample_rate {
        return Err(format!(
            "sample rate {} vs {}",
            a.sample_rate, b.sample_rate
        ));
    }
    if a.frames != b.frames {
        return Err(format!(
            "frame count {} vs {} (delta {})",
            a.frames,
            b.frames,
            b.frames as i64 - a.frames as i64
        ));
    }
    if let Some(i) = a.samples.iter().zip(&b.samples).position(|(x, y)| x != y) {
        let worst = a
            .samples
            .iter()
            .zip(&b.samples)
            .map(|(x, y)| (x - y).abs())
            .fold(0.0f32, f32::max);
        return Err(format!(
            "PCM differs from sample {i} (frame {}); max |delta| {worst:e}",
            i / 2
        ));
    }

    let old_hash = cache::content_hash(old).map_err(|e| format!("hash old: {e}"))?;
    let new_hash = cache::content_hash(new).map_err(|e| format!("hash new: {e}"))?;
    Ok((old_hash, new_hash))
}

fn name(path: &Path) -> String {
    path.file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| path.display().to_string())
}
