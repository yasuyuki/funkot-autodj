//! Which anchor should the outro guide clicks hang off? Measures three
//! schemes side by side on real masters, headless.
//!
//! The two shipped so far both place the nominal boundary and then let
//! `lock_beat_phase` pull it onto the music's beat grid, which can only move
//! it by less than half a beat:
//!
//! - `local`: `total_frames - bars` (pre-`bbf684c`). The file end's phase is
//!   arbitrary, so the lock has to absorb it; for `AntonFer - … - 02 IVY` it
//!   is +0.51 beat, i.e. past the lock's radius, and the click lands a beat
//!   off (the bug `b486a08` and `bbf684c` were chasing).
//! - `prop` (HEAD): `first_downbeat + round((total-fd)/bar) × bar - bars`.
//!   By construction this sits exactly on the intro downbeat's bar grid, so
//!   whatever the lock then has to move is precisely how far that propagated
//!   grid has *drifted* from the audio by the outro. Measured below: under
//!   0.1 beat on 9 of 14 masters, but 0.47 on `03. KazuyaP - Monitoring Db`
//!   and 0.49 on `Nicho - … - 04 Boom Boom Pow` — both at the radius edge,
//!   where the snap direction is a coin flip.
//! - `hybrid`: lock the *local* nominal (no propagation, so no drift), then
//!   shift by whole beats onto the nearest bar head of the propagated grid.
//!   Sub-beat phase and beat identity come from the audio under the window;
//!   only bar identity (mod 4) comes from the propagated grid, where a
//!   half-beat drift is harmless against a 2-beat margin. A single-beat
//!   error in the local snap is *corrected* by that step rather than kept.
//!   No new correlation search — deterministic grid arithmetic only, so §3/§8
//!   of HANDOFF ("never take bar identity from ±N-beat kick correlation")
//!   still holds.
//!
//! ```sh
//! ./dev.sh cargo run -p funkot-cli --release --example outro_anchor_ab -- \
//!   --cache-dir funkot-cache testdata/*.flac
//! ```

use std::path::{Path, PathBuf};

use funkot_cli::label_session::{
    bar_frames_for, boundary_frame, lock_boundary_to_groove, Side, NORMAL_HALF_WIDTH_BARS,
    OUTRO_CANDIDATES,
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
        eprintln!("usage: outro_anchor_ab [--cache-dir DIR] TRACK...");
        std::process::exit(2);
    }
    println!(
        "cand  local_shift  prop_shift(=drift)  hybrid_k  hybrid_left  \
         hybrid-prop  hybrid-local  refit_shift  refit-prop"
    );
    for p in &paths {
        if let Err(e) = report(p, &cache_dir) {
            eprintln!("{}: {e}", p.display());
        }
    }
}

fn report(path: &Path, cache_dir: &Path) -> Result<(), Box<dyn std::error::Error>> {
    let buf = decode_file(path)?;
    let analysis = cache::get_or_analyze(path, cache_dir, &buf)?;
    let bar_frames = bar_frames_for(&analysis, Side::Outro);
    let beat = bar_frames / 4.0;
    let fd = analysis.first_downbeat as f64;
    println!(
        "{}\n  outro_bpm={:.5} bar_frames={:.2} bars_to_end={:.3}",
        path.file_name().unwrap_or_default().to_string_lossy(),
        analysis.outro_bpm,
        bar_frames,
        (analysis.total_frames as f64 - fd) / bar_frames,
    );

    let ref_nominal = boundary_frame(&analysis, Side::Outro, 64);
    let ref_locked =
        lock_boundary_to_groove(&buf, ref_nominal, bar_frames, NORMAL_HALF_WIDTH_BARS);
    let ref_beats = ((ref_locked as f64 - fd) / beat).round();
    let beat_refit = if ref_beats >= 4.0 {
        (ref_locked as f64 - fd) / ref_beats
    } else {
        beat
    };
    let bar_refit = beat_refit * 4.0;
    println!(
        "  refit: ref_beats={ref_beats:.0} beat {beat:.3} -> {beat_refit:.3} \
         (bpm {:.5} -> {:.5})",
        analysis.outro_bpm,
        analysis.outro_bpm * beat / beat_refit,
    );

    for &bars in OUTRO_CANDIDATES.iter() {
        let span = (bar_frames * f64::from(bars)).round() as i64;

        let nominal_local = analysis.total_frames as i64 - span;
        let locked_local =
            lock_boundary_to_groove(&buf, nominal_local, bar_frames, NORMAL_HALF_WIDTH_BARS);

        let nominal_prop = boundary_frame(&analysis, Side::Outro, bars);
        let locked_prop =
            lock_boundary_to_groove(&buf, nominal_prop, bar_frames, NORMAL_HALF_WIDTH_BARS);

        let d_bars = (locked_local as f64 - fd) / bar_frames;
        let frac_bars = d_bars - d_bars.round();
        let to_bar_head_beats = -frac_bars * 4.0;
        let k = to_bar_head_beats.round();
        let hybrid = locked_local + (k * beat).round() as i64;
        let left_beats = to_bar_head_beats - k;

        let bars_to_end_refit = ((analysis.total_frames as f64 - fd) / bar_refit).round();
        let nominal_refit = (fd + (bars_to_end_refit - f64::from(bars)) * bar_refit).round() as i64;
        let locked_refit =
            lock_boundary_to_groove(&buf, nominal_refit, bar_refit, NORMAL_HALF_WIDTH_BARS);

        println!(
            "  {bars:>3}  {:>+11.4}  {:>+18.4}  {k:>+8.0}  {:>+11.4}  {:>+11.4}  {:>+12.4}  \
             {:>+11.4}  {:>+10.4}",
            (locked_local - nominal_local) as f64 / beat,
            (locked_prop - nominal_prop) as f64 / beat,
            left_beats,
            (hybrid - locked_prop) as f64 / beat,
            (hybrid - locked_local) as f64 / beat,
            (locked_refit - nominal_refit) as f64 / beat_refit,
            (locked_refit - locked_prop) as f64 / beat_refit,
        );
    }
    Ok(())
}
