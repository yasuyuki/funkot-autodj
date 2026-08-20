//! Diagnostic dump for the Funkot / non-Funkot classifier.
//!
//! Dumps, per track, the three quantities `analysis::classify_is_funkot`
//! decides on, alongside its verdict. The thresholds in `analysis.rs` were
//! picked from this table over files that used to live in `testdata/`
//! (`classify_funkot.txt`, `classify_funkot_hhhb.txt`,
//! `classify_not_funkot.txt`). **That corpus is not ground truth** — it was
//! produced during operational testing and its contents are wrong. Thresholds
//! will be re-derived from human labels collected in funkot-player labeling
//! mode.
//!
//! Re-run it after touching the classifier: a threshold change that helps one
//! slice of the library usually costs another, and the table is the only way
//! to see which.
//!
//! Prefer `--cache-dir` when analyses already carry `classify_scores` (cache
//! v14+): those tracks skip decode. Otherwise it falls back to
//! `analysis::probe_classification` (re-decode).
//!
//! Usage (inside the dev container):
//!   cargo run -p funkot-core --example classify_probe --release -- \
//!     [-l PLAYLIST] [FILE...] [--tsv OUT.tsv] [--cache-dir DIR]
//!
//! Tracks come from `-l PLAYLIST` (one path per line, `#`-comments and blank
//! lines ignored, relative entries resolved against the playlist's own
//! directory — same convention as funkot-cli and `eval_sections`) and/or bare
//! trailing file arguments. Both may be combined.

use std::fs;
use std::path::{Path, PathBuf};

use funkot_core::{analysis, cache, decode};

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let opts = match Opts::parse(args) {
        Ok(ParsedArgs::Help) => {
            print_usage();
            return;
        }
        Ok(ParsedArgs::Opts(o)) => o,
        Err(e) => {
            eprintln!("error: {e}");
            eprintln!();
            print_usage();
            std::process::exit(2);
        }
    };

    if let Err(e) = run(&opts) {
        eprintln!("error: {e}");
        std::process::exit(1);
    }
}

fn print_usage() {
    eprintln!(
        "usage: classify_probe [-l PLAYLIST] [FILE...] [--tsv OUT.tsv] [--cache-dir DIR]"
    );
    eprintln!();
    eprintln!("  -l PLAYLIST      playlist file, one audio path per line ('#' comments ok);");
    eprintln!("                   relative entries resolve against PLAYLIST's own directory");
    eprintln!("  FILE...          bare audio file paths (combinable with -l)");
    eprintln!("  --tsv OUT.tsv    also write the per-track table to a file");
    eprintln!("  --cache-dir DIR  use cached classify_scores when present (skip decode)");
    eprintln!("  -h, --help       print this message");
}

struct Opts {
    playlist: Option<PathBuf>,
    files: Vec<PathBuf>,
    tsv_out: Option<PathBuf>,
    cache_dir: Option<PathBuf>,
}

enum ParsedArgs {
    Help,
    Opts(Opts),
}

impl Opts {
    fn parse(args: Vec<String>) -> Result<ParsedArgs, String> {
        let mut playlist = None;
        let mut files = Vec::new();
        let mut tsv_out = None;
        let mut cache_dir = None;

        let mut it = args.into_iter();
        while let Some(arg) = it.next() {
            match arg.as_str() {
                "-h" | "--help" => return Ok(ParsedArgs::Help),
                "-l" => playlist = Some(PathBuf::from(it.next().ok_or("-l needs a path")?)),
                "--tsv" => tsv_out = Some(PathBuf::from(it.next().ok_or("--tsv needs a path")?)),
                "--cache-dir" => {
                    cache_dir = Some(PathBuf::from(it.next().ok_or("--cache-dir needs a path")?))
                }
                other if other.starts_with('-') && other.len() > 1 => {
                    return Err(format!("unknown option '{other}'"))
                }
                other => files.push(PathBuf::from(other)),
            }
        }

        if playlist.is_none() && files.is_empty() {
            return Err("no tracks given; pass -l PLAYLIST and/or file paths".into());
        }
        Ok(ParsedArgs::Opts(Opts {
            playlist,
            files,
            tsv_out,
            cache_dir,
        }))
    }
}

/// Columns are per side (h = head, t = tail) and are exactly the three
/// quantities `classify_is_funkot` decides on:
///   `bpm`   grid BPM (172-188), the value `analyze` stores
///   `z`     normalised comb score at the grid period
///   `rat`   `z` as a fraction of the best normalised score in 100-200
///   `half`  normalised score one metrical level down, over `z`
const HEADER: &str = "funkot\th_bpm\th_z\th_rat\th_half\tt_bpm\tt_z\tt_rat\tt_half\tfile";

fn run(opts: &Opts) -> Result<(), String> {
    let mut tracks: Vec<PathBuf> = Vec::new();
    if let Some(p) = &opts.playlist {
        tracks.extend(load_playlist(p)?);
    }
    tracks.extend(opts.files.iter().cloned());

    let mut rows = Vec::new();
    let mut failed = 0usize;
    let mut funkot_true = 0usize;
    let mut from_cache = 0usize;
    let mut re_decoded = 0usize;

    for path in &tracks {
        let probe = match probe_track(path, opts.cache_dir.as_deref()) {
            Ok((p, ProbeSource::Cache)) => {
                from_cache += 1;
                p
            }
            Ok((p, ProbeSource::Decoded)) => {
                re_decoded += 1;
                p
            }
            Err(SkipReason::Decode(e)) => {
                eprintln!("skip (decode failed): {} -- {e}", path.display());
                failed += 1;
                continue;
            }
            Err(SkipReason::Probe(e)) => {
                eprintln!("skip (probe failed): {} -- {e}", path.display());
                failed += 1;
                continue;
            }
        };
        if probe.is_funkot {
            funkot_true += 1;
        }
        rows.push(format_row(path, &probe));
    }

    let mut out = String::new();
    out.push_str(HEADER);
    out.push('\n');
    for r in &rows {
        out.push_str(r);
        out.push('\n');
    }
    print!("{out}");

    eprintln!();
    eprintln!("=== classify_probe ===");
    eprintln!("tracks probed : {}", rows.len());
    eprintln!("from cache: {from_cache}");
    eprintln!("re-decoded: {re_decoded}");
    eprintln!("decode/probe failures: {failed}");
    eprintln!(
        "is_funkot=true: {funkot_true} / {} ({:.0}%)",
        rows.len(),
        if rows.is_empty() {
            0.0
        } else {
            100.0 * funkot_true as f64 / rows.len() as f64
        }
    );

    if let Some(p) = &opts.tsv_out {
        fs::write(p, &out).map_err(|e| format!("failed to write '{}': {e}", p.display()))?;
        eprintln!("wrote {}", p.display());
    }
    Ok(())
}

enum ProbeSource {
    Cache,
    Decoded,
}

enum SkipReason {
    Decode(String),
    Probe(String),
}

fn probe_track(
    path: &Path,
    cache_dir: Option<&Path>,
) -> Result<(analysis::ClassifyProbe, ProbeSource), SkipReason> {
    if let Some(dir) = cache_dir {
        if let Ok(hash) = cache::content_hash(path) {
            if let Some(analysis) = cache::load(dir, &hash) {
                if let Some(probe) = analysis::probe_from_cached(&analysis) {
                    return Ok((probe, ProbeSource::Cache));
                }
            }
        }
    }

    let buffer = decode::decode_file(path).map_err(|e| SkipReason::Decode(e.to_string()))?;
    let probe =
        analysis::probe_classification(&buffer).map_err(|e| SkipReason::Probe(e.to_string()))?;
    Ok((probe, ProbeSource::Decoded))
}

fn format_row(path: &Path, probe: &analysis::ClassifyProbe) -> String {
    let name = path
        .file_name()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_else(|| path.display().to_string());
    format!(
        "{}\t{}\t{}\t{}",
        u8::from(probe.is_funkot),
        side_cols(&probe.head),
        side_cols(&probe.tail),
        name
    )
}

fn side_cols(s: &analysis::SideProbe) -> String {
    let bpm = match s.grid_bpm {
        Some(b) => format!("{b:.2}"),
        None => "-".to_string(),
    };
    // `half_ratio` is infinite when the grid carries no signal at all; print
    // it as "inf" rather than letting it widen the column unpredictably.
    let half = if s.half_ratio.is_finite() {
        format!("{:.3}", s.half_ratio)
    } else {
        "inf".to_string()
    };
    format!("{}\t{:.2}\t{:.3}\t{}", bpm, s.z, s.z_ratio, half)
}

fn load_playlist(path: &Path) -> Result<Vec<PathBuf>, String> {
    let contents = fs::read_to_string(path)
        .map_err(|e| format!("failed to read playlist '{}': {e}", path.display()))?;
    let base = path
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    let mut out = Vec::new();
    for line in contents.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let p = Path::new(line);
        out.push(if p.is_absolute() {
            p.to_path_buf()
        } else {
            base.join(p)
        });
    }
    Ok(out)
}
