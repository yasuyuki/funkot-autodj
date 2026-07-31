//! funkot-autodj CLI: live playback via cpal, or offline WAV render.

use std::io::{self, BufRead, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::mpsc::{self, RecvTimeoutError};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use anyhow::{bail, Context, Result};
use clap::Parser;
use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use cpal::{BufferSize, SampleFormat, StreamConfig, SupportedBufferSize};
use crossterm::event::{self, Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use crossterm::terminal::{disable_raw_mode, enable_raw_mode};
use funkot_cli::label_session::{self, LabelKey, LabelOutcome, Side, TrackSession};
use funkot_cli::nav_keys::{MultiPressAggregator, NavDir, MULTI_PRESS_WINDOW};
use funkot_cli::playlist::{load_playlist_file, validate_paths_exist};
use funkot_cli::stream_error::{self, StreamErrorThrottle};
use funkot_cli::wav_write::{WavFormat, WavStreamWriter};
use funkot_core::decode::decode_file;
use funkot_core::engine::{prepare_tracks_parallel, Engine, EngineEvent, NavAction};
use funkot_core::labels::{upsert_label, SectionLabel};
use funkot_core::{cache, EngineOptions, PitchMode};
use log::warn;

#[derive(Debug, Parser)]
#[command(
    name = "funkot-autodj",
    about = "Auto-DJ for Funkot dance music",
    version,
    arg_required_else_help = true
)]
struct Args {
    /// Audio files in play order
    files: Vec<PathBuf>,

    /// Playlist file: one path per line (# comments / blank lines ignored)
    #[arg(short = 'l', long = "list", value_name = "FILE")]
    list: Option<PathBuf>,

    /// Speed-up factor applied to nominal 180 BPM (default 1.10 → 198 BPM)
    #[arg(long, default_value_t = 1.10)]
    rate: f64,

    /// Raise pitch with tempo instead of time-stretching (default: preserve pitch)
    #[arg(long)]
    pitch_shift: bool,

    /// Crossfade length in bars
    #[arg(long, default_value_t = 4)]
    fade_bars: u32,

    /// High-pass cutoff (Hz) for mid/high-pass during transitions
    #[arg(long = "highpass-hz", alias = "lpf-hz", default_value_t = 300.0)]
    highpass_hz: f32,

    /// Shuffle playlist order (reshuffled every full cycle)
    #[arg(long)]
    random: bool,

    /// Stop after one playlist pass (default: loop forever)
    #[arg(long)]
    no_loop: bool,

    /// Disable RMS gain normalization
    #[arg(long)]
    no_gain: bool,

    /// Directory for analysis cache JSON files
    #[arg(long, default_value = "funkot-cache")]
    cache_dir: PathBuf,

    /// Offline render to stereo WAV instead of live playback
    #[arg(long, value_name = "OUT.wav")]
    render: Option<PathBuf>,

    /// While playing live, also write the stereo mix bus to WAV (debug)
    #[arg(long, value_name = "OUT.wav")]
    dump_wav: Option<PathBuf>,

    /// Per-transition clip length (seconds) emitted during `--render`.
    /// Clips start 8 bars before each `TransitionStarted` and run for this
    /// many seconds (capped by available render duration). Default 60s.
    /// Set to 0 to disable transition clip export (and `--transitions-only`).
    #[arg(long, default_value_t = 60.0)]
    transition_clip_seconds: f64,

    /// Dev: play only the same transition windows as per-transition clip export
    /// (8 bars before each TransitionStarted, `--transition-clip-seconds` long).
    /// With `--render`, OUT.wav is those windows concatenated instead of live play.
    /// Same gates as clip export (`transition_clip_seconds > 0`, playlist length ≥ 2).
    #[arg(long)]
    transitions_only: bool,

    /// WAV sample format for `--render` / `--dump-wav` (default: 32-bit float)
    #[arg(long, value_enum, default_value_t = WavFormat::F32)]
    wav_format: WavFormat,

    /// Output sample rate in Hz (live: device default or 48000; render: 44100)
    #[arg(long)]
    sample_rate: Option<u32>,

    /// Offline render speed limit as a multiple of realtime (0 = unlimited).
    /// Pacing gives the loader time to prepare the next track; too fast and
    /// transitions fall back to extended outros. With `--jobs`/`--ci-fast`,
    /// tracks are prepared up front so `0` is safe for CI.
    #[arg(long, default_value_t = 10.0)]
    render_speed: f64,

    /// Parallel track prepare workers for `--render` (0 = host CPU count).
    /// Does not change analysis or mix results — only wall-clock time.
    #[arg(long, default_value_t = 1)]
    jobs: usize,

    /// CI fastest offline mode: `--no-loop`, `--render-speed 0`, `--jobs 0`
    /// (all CPUs). Safe for audio identity; only preparation is parallelized.
    #[arg(long)]
    ci_fast: bool,

    /// Write minimal analysis/downbeat test fixtures (+ golden JSON) to DIR and exit.
    /// Does not render a playlist mix. See `funkot-core/tests/fixtures/README.md`.
    #[arg(long = "gen-test-fixtures", value_name = "DIR")]
    gen_test_fixtures: Option<PathBuf>,

    /// Delete cache entries with no manual intro/outro flags; strip auto fields
    /// from entries that keep at least one `*_bars_manual` flag (they are
    /// reanalyzed on next use / with `--fill-missing-cache`).
    #[arg(long)]
    purge_auto_cache: bool,

    /// Decode+analyze only tracks whose cache is missing or marked
    /// `needs_reanalysis`, then exit (skips complete cache hits).
    #[arg(long)]
    fill_missing_cache: bool,

    /// Interactively label intro/outro section lengths against `--labels`,
    /// then exit. Tracks come from `-l/--list` or the positional FILES, same
    /// as normal playback. See `funkot_cli::label_session` for the key
    /// bindings and `funkot_core::labels` for the on-disk format.
    #[arg(long = "label-sections", requires = "labels")]
    label_sections: bool,

    /// `labels.tsv` path for `--label-sections` (required with that flag).
    #[arg(long, value_name = "FILE")]
    labels: Option<PathBuf>,

    /// With `--label-sections`, write each candidate's click clip as WAV
    /// into DIR instead of playing it live, then exit. For environments
    /// without an audio output device (e.g. inside the dev container).
    #[arg(long, value_name = "DIR", requires = "label_sections")]
    render_clips: Option<PathBuf>,

    /// `--label-sections` click peak level, in dB above the clip's own RMS
    /// loudness (not a fixed absolute amplitude — real Funkot masters run
    /// hot enough that a fixed number either got lost or clipped). Raise
    /// this if clicks are still hard to hear on a given track.
    #[arg(long, default_value_t = label_session::ClickOptions::default().click_db_above_rms, requires = "label_sections")]
    click_db: f32,

    /// `--label-sections`: how many dB to duck the music under each bar-head
    /// click (sidechain-style, so the click cuts through dense/loud
    /// material). The boundary click ducks deeper and longer automatically
    /// on top of this, so it stays distinguishable from a normal bar head.
    #[arg(long, default_value_t = label_session::ClickOptions::default().duck_db, requires = "label_sections")]
    click_duck_db: f32,
}

fn main() {
    if let Err(err) = run() {
        eprintln!("error: {err:#}");
        std::process::exit(1);
    }
}

fn run() -> Result<()> {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("warn")).init();

    let mut args = Args::parse();
    if args.ci_fast {
        args.no_loop = true;
        args.render_speed = 0.0;
        args.jobs = 0;
        if args.render.is_none() {
            bail!("--ci-fast requires --render OUT.wav");
        }
    }

    if let Some(dir) = &args.gen_test_fixtures {
        return gen_test_fixtures(dir);
    }

    if args.label_sections {
        // clap's `requires` guarantees this is Some.
        let labels_path = args.labels.clone().expect("--labels required by clap");
        let playlist = resolve_playlist(&args)?;
        let click_opts = label_session::ClickOptions {
            click_db_above_rms: args.click_db,
            duck_db: args.click_duck_db,
        };
        return run_label_sections(
            &playlist,
            &labels_path,
            &args.cache_dir,
            args.render_clips.as_deref(),
            &click_opts,
        );
    }

    let playlist = resolve_playlist(&args)?;
    let options = build_options(&args)?;

    if args.purge_auto_cache {
        let stats = funkot_core::cache::purge_auto(&options.cache_dir)
            .map_err(|e| anyhow::anyhow!("{e}"))?;
        eprintln!(
            "purge-auto-cache: deleted {} cleared {} skipped {}",
            stats.deleted, stats.cleared, stats.skipped
        );
    }

    if args.fill_missing_cache {
        return fill_missing_cache(&playlist, &options.cache_dir);
    }

    let stop = Arc::new(AtomicBool::new(false));
    let stop_flag = Arc::clone(&stop);
    ctrlc::set_handler(move || {
        stop_flag.store(true, Ordering::SeqCst);
    })
    .context("failed to install Ctrl+C handler")?;

    if args.render.is_some() && args.dump_wav.is_some() {
        bail!("cannot combine --render and --dump-wav (use one)");
    }

    if args.transitions_only {
        let ok = args.transition_clip_seconds.is_finite()
            && args.transition_clip_seconds > 0.0
            && playlist.len() >= 2;
        if !ok {
            bail!(
                "--transitions-only needs playlist length ≥ 2 and --transition-clip-seconds > 0 \
                 (same gates as transition clip export)"
            );
        }
    }

    if let Some(out) = &args.render {
        let mut options = options;
        if !args.no_loop {
            eprintln!("note: --render implies --no-loop");
        }
        options.loop_playlist = false;
        run_render(
            options,
            playlist,
            out,
            args.wav_format,
            args.render_speed,
            args.transition_clip_seconds,
            args.transitions_only,
            args.jobs,
            &stop,
        )?;
    } else {
        run_live(
            options,
            playlist,
            args.sample_rate,
            args.dump_wav.as_deref(),
            args.wav_format,
            args.transition_clip_seconds,
            args.transitions_only,
            &stop,
        )?;
    }
    Ok(())
}

fn resolve_playlist(args: &Args) -> Result<Vec<PathBuf>> {
    match (args.files.is_empty(), &args.list) {
        (true, None) => {
            bail!("provide audio FILES and/or -l/--list <FILE>");
        }
        (false, Some(_)) => {
            bail!("cannot combine positional FILES with -l/--list");
        }
        (false, None) => {
            validate_paths_exist(&args.files)?;
            Ok(args.files.clone())
        }
        (true, Some(list)) => load_playlist_file(list),
    }
}

fn build_options(args: &Args) -> Result<EngineOptions> {
    if !args.rate.is_finite() || !(0.5..=2.0).contains(&args.rate) {
        bail!("--rate must be finite and in [0.5, 2.0], got {}", args.rate);
    }
    if !(1..=16).contains(&args.fade_bars) {
        bail!("--fade-bars must be in 1..=16, got {}", args.fade_bars);
    }
    if !(50.0..=2000.0).contains(&args.highpass_hz) {
        bail!(
            "--highpass-hz must be in 50..=2000, got {}",
            args.highpass_hz
        );
    }
    if let Some(sr) = args.sample_rate {
        if sr == 0 {
            bail!("--sample-rate must be greater than 0");
        }
    }
    if args.render_speed < 0.0 || !args.render_speed.is_finite() {
        bail!(
            "--render-speed must be finite and >= 0, got {}",
            args.render_speed
        );
    }

    let default_sr = if args.render.is_some() {
        44_100
    } else {
        48_000
    };
    let output_sample_rate = args.sample_rate.unwrap_or(default_sr);

    Ok(EngineOptions {
        rate: args.rate,
        pitch_mode: if args.pitch_shift {
            PitchMode::Shift
        } else {
            PitchMode::Preserve
        },
        fade_bars: args.fade_bars,
        highpass_hz: args.highpass_hz,
        gain_normalize: !args.no_gain,
        random: args.random,
        loop_playlist: !args.no_loop,
        output_sample_rate,
        cache_dir: args.cache_dir.clone(),
    })
}

fn file_name(path: &std::path::Path) -> String {
    path.file_name()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_else(|| path.display().to_string())
}

fn fmt_hms(d: Duration) -> String {
    let s = d.as_secs();
    format!("{:02}:{:02}:{:02}", s / 3600, (s / 60) % 60, s % 60)
}

/// Local wall-clock HH:MM:SS (no chrono).
fn local_hms() -> String {
    unsafe {
        let t = libc::time(std::ptr::null_mut());
        let mut tm = std::mem::zeroed();
        #[cfg(unix)]
        {
            if libc::localtime_r(&t, &mut tm).is_null() {
                return "--:--:--".into();
            }
        }
        #[cfg(windows)]
        {
            if libc::localtime_s(&mut tm, &t) != 0 {
                return "--:--:--".into();
            }
        }
        format!("{:02}:{:02}:{:02}", tm.tm_hour, tm.tm_min, tm.tm_sec)
    }
}

/// Wall time spent actually playing (excludes pre-first-track prep and pauses).
#[derive(Default)]
struct PlayElapsed {
    start: Option<Instant>,
    pause_at: Option<Instant>,
    paused: Duration,
}

impl PlayElapsed {
    fn on_track_started(&mut self) {
        if self.start.is_none() {
            self.start = Some(Instant::now());
        }
    }

    fn set_paused(&mut self, paused: bool) {
        // Ignore pause toggles before the first track — prep time is already excluded.
        if self.start.is_none() {
            return;
        }
        if paused {
            if self.pause_at.is_none() {
                self.pause_at = Some(Instant::now());
            }
        } else if let Some(at) = self.pause_at.take() {
            self.paused += at.elapsed();
        }
    }

    fn elapsed(&self) -> Duration {
        let Some(start) = self.start else {
            return Duration::ZERO;
        };
        let mut e = start.elapsed().saturating_sub(self.paused);
        if let Some(at) = self.pause_at {
            e = e.saturating_sub(at.elapsed());
        }
        e
    }
}

/// Prints intro/outro/BPM once cache analysis is ready for the current track.
struct AnalysisPrinter {
    cache_dir: PathBuf,
    /// Content hash waiting on a background analyze (first live track, cache miss).
    pending_hash: Option<String>,
}

impl AnalysisPrinter {
    fn new(cache_dir: PathBuf) -> Self {
        Self {
            cache_dir,
            pending_hash: None,
        }
    }

    fn on_track_started(&mut self, path: &Path) {
        self.pending_hash = None;
        let Ok(hash) = funkot_core::cache::content_hash(path) else {
            return;
        };
        if print_analysis_if_cached(&self.cache_dir, &hash) {
            return;
        }
        // Cache miss / needs_reanalysis: first live track may still be analyzing.
        self.pending_hash = Some(hash);
    }

    fn poll(&mut self) {
        let Some(hash) = self.pending_hash.clone() else {
            return;
        };
        if print_analysis_if_cached(&self.cache_dir, &hash) {
            self.pending_hash = None;
        }
    }
}

fn print_analysis_if_cached(cache_dir: &Path, hash: &str) -> bool {
    let Some(a) = funkot_core::cache::load(cache_dir, hash) else {
        return false;
    };
    if a.needs_reanalysis {
        return false;
    }
    println!(
        "  analysis: intro_bars={} outro_bars={} bpm={:.2}",
        a.intro_bars, a.outro_bars, a.intro_bpm
    );
    true
}

/// Live `--transitions-only`: delay by preroll so `TransitionStarted` lines up
/// with the clip window start (same 8-bar lead-in as render clip export).
struct TransitionPlayGate {
    delay: std::collections::VecDeque<(f32, f32)>,
    preroll_frames: usize,
    clip_len: u64,
    play_left: u64,
}

impl TransitionPlayGate {
    fn new(preroll_frames: u64, clip_len_frames: u64) -> Self {
        Self {
            delay: std::collections::VecDeque::with_capacity(preroll_frames as usize + 1),
            preroll_frames: preroll_frames as usize,
            clip_len: clip_len_frames,
            play_left: 0,
        }
    }

    /// `transition_offsets`: frame indices within this chunk where a transition starts.
    fn process(&mut self, stereo: &mut [f32], n_frames: usize, transition_offsets: &[usize]) {
        for i in 0..n_frames {
            if transition_offsets.iter().any(|&o| o == i) {
                self.play_left = self.play_left.max(self.clip_len);
            }
            let l = stereo[i * 2];
            let r = stereo[i * 2 + 1];
            let (ol, or) = self.step(l, r);
            stereo[i * 2] = ol;
            stereo[i * 2 + 1] = or;
        }
    }

    fn step(&mut self, l: f32, r: f32) -> (f32, f32) {
        if self.preroll_frames == 0 {
            if self.play_left > 0 {
                self.play_left -= 1;
                return (l, r);
            }
            return (0.0, 0.0);
        }
        self.delay.push_back((l, r));
        if self.delay.len() <= self.preroll_frames {
            return (0.0, 0.0);
        }
        let (ol, or) = self.delay.pop_front().expect("delay non-empty");
        if self.play_left > 0 {
            self.play_left -= 1;
            (ol, or)
        } else {
            (0.0, 0.0)
        }
    }
}

fn print_event(
    event: &EngineEvent,
    playlist_len: usize,
    play_elapsed: &mut PlayElapsed,
    analysis: &mut AnalysisPrinter,
) {
    match event {
        EngineEvent::TrackStarted { index, path } => {
            play_elapsed.on_track_started();
            println!(
                "> now playing [{}/{}] {}  {} (+{})",
                index + 1,
                playlist_len,
                file_name(path),
                local_hms(),
                fmt_hms(play_elapsed.elapsed()),
            );
            analysis.on_track_started(path);
        }
        EngineEvent::TransitionStarted { from, to } => {
            println!("~ transition: {} -> {}", file_name(from), file_name(to));
        }
        EngineEvent::TrackFailed { path, message } => {
            warn!("track failed: {} ({message})", path.display());
            println!("x failed: {} ({message})", file_name(path));
        }
        EngineEvent::Finished => {
            println!("finished");
        }
    }
}

#[cfg(test)]
mod play_elapsed_tests {
    use super::*;
    use std::thread;

    #[test]
    fn excludes_prep_and_pause() {
        let mut clock = PlayElapsed::default();
        assert_eq!(clock.elapsed(), Duration::ZERO);

        thread::sleep(Duration::from_millis(30));
        clock.on_track_started();
        let after_start = clock.elapsed();

        thread::sleep(Duration::from_millis(40));
        clock.set_paused(true);
        let at_pause = clock.elapsed();
        thread::sleep(Duration::from_millis(50));
        assert!(
            clock.elapsed().as_millis().abs_diff(at_pause.as_millis()) < 15,
            "elapsed must freeze while paused"
        );

        clock.set_paused(false);
        thread::sleep(Duration::from_millis(40));
        let after_resume = clock.elapsed();

        assert!(after_start.as_millis() < 20, "start should be near zero");
        assert!(at_pause.as_millis() >= 30, "should count play before pause");
        assert!(
            after_resume.as_millis() >= at_pause.as_millis() + 25,
            "should resume counting after unpause"
        );
        assert!(
            after_resume.as_millis() < at_pause.as_millis() + 80,
            "must not include paused interval"
        );
    }
}

#[cfg(test)]
mod transition_play_gate_tests {
    use super::*;

    #[test]
    fn delay_aligns_window_with_transition() {
        // preroll=2, clip=3: transition at engine frame 4 → hear delayed frames 2,3,4
        let mut gate = TransitionPlayGate::new(2, 3);
        let mut buf = vec![0.0f32; 10 * 2];
        for i in 0..10 {
            buf[i * 2] = (i + 1) as f32; // left = engine frame index + 1
            buf[i * 2 + 1] = -(i as f32);
        }
        gate.process(&mut buf, 10, &[4]);

        // frames 0..2: filling delay → silence
        // frame 2: output engine0 (muted, play_left still 0)
        // frame 3: output engine1 (muted)
        // frame 4: transition → play_left=3, output engine2 (unmuted)
        // frame 5: output engine3
        // frame 6: output engine4
        // frame 7+: muted
        let left: Vec<f32> = (0..10).map(|i| buf[i * 2]).collect();
        assert_eq!(
            &left[..],
            &[0.0, 0.0, 0.0, 0.0, 3.0, 4.0, 5.0, 0.0, 0.0, 0.0]
        );
    }
}

fn run_render(
    options: EngineOptions,
    playlist: Vec<PathBuf>,
    out_path: &std::path::Path,
    wav_format: WavFormat,
    render_speed: f64,
    transition_clip_seconds: f64,
    transitions_only: bool,
    jobs: usize,
    stop: &AtomicBool,
) -> Result<()> {
    let playlist_len = playlist.len();
    let sample_rate = options.output_sample_rate;
    // Start 8 bars before TransitionStarted so the lead-in is audible.
    const TRANSITION_CLIP_PREROLL_BARS: u32 = 8;
    let transition_enabled =
        transition_clip_seconds.is_finite() && transition_clip_seconds > 0.0 && playlist_len >= 2;
    if transitions_only && !transition_enabled {
        bail!(
            "--transitions-only needs playlist length ≥ 2 and --transition-clip-seconds > 0 \
             (same gates as transition clip export)"
        );
    }
    let clip_len_frames = if transition_enabled {
        (transition_clip_seconds * f64::from(sample_rate)).round() as u64
    } else {
        0
    };
    let preroll_frames = if transition_enabled {
        (options.bar_frames() * f64::from(TRANSITION_CLIP_PREROLL_BARS)).round() as u64
    } else {
        0
    };
    let preroll_samples = (preroll_frames as usize).saturating_mul(2);
    // Interleaved stereo ending at the next chunk's start (for preroll replay).
    let mut lookback: Vec<f32> = Vec::new();

    let mut analysis = AnalysisPrinter::new(options.cache_dir.clone());

    let mut engine = if jobs == 1 {
        Engine::new(options, playlist).map_err(|e| anyhow::anyhow!("engine: {e}"))?
    } else {
        let jobs_label = if jobs == 0 {
            "all CPUs".to_string()
        } else {
            format!("{jobs}")
        };
        eprintln!("preparing {playlist_len} tracks with --jobs {jobs_label}...");
        let tracks = prepare_tracks_parallel(&options, &playlist, jobs)
            .map_err(|e| anyhow::anyhow!("parallel prepare: {e}"))?;
        Engine::from_prepared(options, tracks).map_err(|e| anyhow::anyhow!("engine: {e}"))?
    };

    let mut writer = WavStreamWriter::create(out_path, sample_rate, wav_format)?;
    struct TransitionCapture {
        start_frame: u64, // output-file frame indices
        end_frame: u64,   // exclusive
        writer: WavStreamWriter,
    }

    fn parse_real_mix_v_number(stem: &str) -> Option<u32> {
        let prefix = "real_mix_v";
        if !stem.starts_with(prefix) {
            return None;
        }
        let mut digits = String::new();
        for ch in stem[prefix.len()..].chars() {
            if ch.is_ascii_digit() {
                digits.push(ch);
            } else {
                break;
            }
        }
        if digits.is_empty() {
            None
        } else {
            digits.parse::<u32>().ok()
        }
    }

    fn sanitize_component(s: &str) -> String {
        let mut out = String::with_capacity(s.len());
        let mut last_us = false;
        for ch in s.chars() {
            let keep = ch.is_ascii_alphanumeric() || ch == '_';
            if keep {
                out.push(ch);
                last_us = false;
            } else if !last_us {
                out.push('_');
                last_us = true;
            }
        }
        let out = out.trim_matches('_');
        if out.is_empty() {
            "track".to_string()
        } else {
            out.to_string()
        }
    }

    fn short_name_from_path(path: &PathBuf) -> String {
        let stem = path
            .file_stem()
            .and_then(|s| s.to_str())
            .map(|s| s.to_string())
            .unwrap_or_else(|| path.to_string_lossy().to_string());

        // Strip leading track number like "03. " or "12 ".
        let mut s = stem.trim_start();
        let mut i = 0usize;
        for ch in s.chars() {
            if ch.is_ascii_digit() {
                i += 1;
            } else {
                break;
            }
        }
        if i > 0 {
            let rest = &s[i..];
            if rest.starts_with('.') {
                s = rest[1..].trim_start();
            } else if rest.starts_with(' ') {
                s = rest.trim_start();
            }
        }

        // Prefer the last "- <number> <title>" segment if present.
        let parts: Vec<&str> = s.split(" - ").collect();
        let mut chosen: Option<&str> = None;
        for p in &parts {
            let t = p.trim_start();
            let mut j = 0usize;
            for ch in t.chars() {
                if ch.is_ascii_digit() {
                    j += 1;
                } else {
                    break;
                }
            }
            if j > 0 {
                let after = t[j..].trim_start();
                if !after.is_empty() {
                    chosen = Some(after);
                }
            }
        }

        let raw = if let Some(c) = chosen {
            c
        } else {
            parts.first().copied().unwrap_or(s)
        };
        sanitize_component(raw)
    }

    fn wav_suffix(w: WavFormat) -> &'static str {
        match w {
            WavFormat::F32 => "f32",
            WavFormat::S24 => "s24",
            WavFormat::S16 => "s16",
        }
    }

    let transitions_dir = if transition_enabled {
        let out_parent = out_path.parent().unwrap_or_else(|| Path::new("testdata"));
        let stem = out_path
            .file_stem()
            .and_then(|s| s.to_str())
            .unwrap_or("mix");
        if let Some(v) = parse_real_mix_v_number(stem) {
            out_parent.join(format!("real_mix_v{v}_transitions"))
        } else {
            out_parent.join(format!("{stem}_transitions"))
        }
    } else {
        PathBuf::new()
    };

    if transition_enabled {
        std::fs::create_dir_all(&transitions_dir)?;
    }

    let mut transition_captures: Vec<TransitionCapture> = Vec::new();
    let mut next_transition_idx: u32 = 1;
    let mut out_frames: u64 = 0; // frames actually written to OUT.wav
                                 // Mix-timeline cursor so overlapping transition windows are not duplicated
                                 // when concatenating into OUT under `--transitions-only`.
    let mut out_mix_emitted_through: u64 = 0;

    const CHUNK_FRAMES: usize = 8192;
    let mut buf = vec![0.0f32; CHUNK_FRAMES * 2];
    let mut seen_track_started = false;
    let mut writing = false;
    let mut rendered_frames: u64 = 0;
    let mut consecutive_silent_frames: u64 = 0;
    let mut last_progress_secs = 0u64;
    let wall_start = Instant::now();
    let mut play_elapsed = PlayElapsed::default();
    // Skip writing long post-track silence while the loader prepares the next
    // track (render() returns zeros and never blocks). Keep short gaps that
    // occur between kicks in the material itself.
    let silence_skip_frames = if transition_enabled {
        // Keep clip boundaries stable in output-file frame indices.
        u64::MAX
    } else {
        u64::from(sample_rate) // ~1s
    };

    loop {
        if stop.load(Ordering::SeqCst) {
            break;
        }

        let n = engine.render(&mut buf);
        let n_frames_u64 = n as u64;
        let transition_frames_into_end = engine.transition_frames_into().unwrap_or(0);
        let mut transitions_started: Vec<(PathBuf, PathBuf)> = Vec::new();
        for event in engine.poll_events() {
            if matches!(event, EngineEvent::TrackStarted { .. }) {
                seen_track_started = true;
            }
            if let EngineEvent::TransitionStarted { from, to } = &event {
                transitions_started.push((from.clone(), to.clone()));
            }
            print_event(&event, playlist_len, &mut play_elapsed, &mut analysis);
        }
        analysis.poll();

        if n == 0 {
            break;
        }

        let chunk = &buf[..n * 2];
        let silent = chunk.iter().all(|s| s.abs() < 1e-12);

        if !writing {
            if !seen_track_started || silent {
                continue;
            }
            writing = true;
        }

        if silent {
            consecutive_silent_frames += n as u64;
            if consecutive_silent_frames > silence_skip_frames {
                // Loader-wait padding: yield without bloating the WAV.
                thread::sleep(Duration::from_millis(5));
                continue;
            }
        } else {
            consecutive_silent_frames = 0;
        }

        // Anchor per-transition clip start on the output-file frame index.
        let chunk_start_frame = rendered_frames;

        if transition_enabled
            && !transitions_started.is_empty()
            && transition_frames_into_end <= n_frames_u64
        {
            for (from, to) in transitions_started {
                let start_offset = if transition_frames_into_end == 0 {
                    0
                } else {
                    n_frames_u64.saturating_sub(transition_frames_into_end)
                };
                let transition_frame = chunk_start_frame + start_offset;
                let start_frame = transition_frame.saturating_sub(preroll_frames);
                let end_frame = start_frame.saturating_add(clip_len_frames);

                let from_name = short_name_from_path(&from);
                let to_name = short_name_from_path(&to);
                let clip_path = transitions_dir.join(format!(
                    "{:02}_{from_name}_to_{to_name}_{}.wav",
                    next_transition_idx,
                    wav_suffix(wav_format)
                ));

                let mut w = WavStreamWriter::create(&clip_path, sample_rate, wav_format)?;
                // Replay audio before this chunk (preroll lives in lookback).
                if start_frame < chunk_start_frame {
                    let lookback_frames = (lookback.len() / 2) as u64;
                    let lookback_start = chunk_start_frame.saturating_sub(lookback_frames);
                    let lb_from = start_frame.max(lookback_start);
                    let lb_to = end_frame.min(chunk_start_frame);
                    if lb_to > lb_from {
                        let off = ((lb_from - lookback_start) as usize) * 2;
                        let end = off + ((lb_to - lb_from) as usize) * 2;
                        let preroll = &lookback[off..end];
                        w.write_interleaved(preroll)?;
                        if transitions_only {
                            let emit_from = lb_from.max(out_mix_emitted_through);
                            if lb_to > emit_from {
                                let skip = ((emit_from - lb_from) as usize) * 2;
                                writer.write_interleaved(&preroll[skip..])?;
                                out_frames += lb_to - emit_from;
                                out_mix_emitted_through = lb_to;
                            }
                        }
                    }
                }
                transition_captures.push(TransitionCapture {
                    start_frame,
                    end_frame,
                    writer: w,
                });
                next_transition_idx += 1;
            }
        } else if transition_enabled && !transitions_started.is_empty() {
            eprintln!(
                "warn: transition clip start skipped: frames_into_end={} n={}",
                transition_frames_into_end, n
            );
        }

        // Also write overlapping samples into active transition capture(s).
        if transition_enabled && clip_len_frames > 0 && !transition_captures.is_empty() {
            let chunk_end_frame = chunk_start_frame + n_frames_u64;
            for cap in transition_captures.iter_mut() {
                let overlap_start = cap.start_frame.max(chunk_start_frame);
                let overlap_end = cap.end_frame.min(chunk_end_frame);
                if overlap_end <= overlap_start {
                    continue;
                }

                let overlap_frames = overlap_end - overlap_start;
                let chunk_off_frames = overlap_start - chunk_start_frame;
                let src_start = (chunk_off_frames as usize) * 2;
                let src_end = src_start + (overlap_frames as usize) * 2;
                let overlap = &chunk[src_start..src_end];
                cap.writer.write_interleaved(overlap)?;
            }
            // Concatenate the same windows into OUT (dedupe overlapping clips).
            if transitions_only {
                let mut abs = chunk_start_frame.max(out_mix_emitted_through);
                while abs < chunk_end_frame {
                    let in_win = transition_captures
                        .iter()
                        .any(|c| abs >= c.start_frame && abs < c.end_frame);
                    if !in_win {
                        abs += 1;
                        continue;
                    }
                    let run_end = transition_captures
                        .iter()
                        .filter(|c| abs >= c.start_frame && abs < c.end_frame)
                        .map(|c| c.end_frame.min(chunk_end_frame))
                        .max()
                        .unwrap_or(abs + 1);
                    let off = ((abs - chunk_start_frame) as usize) * 2;
                    let end = off + ((run_end - abs) as usize) * 2;
                    writer.write_interleaved(&chunk[off..end])?;
                    out_frames += run_end - abs;
                    out_mix_emitted_through = run_end;
                    abs = run_end;
                }
            }
        }

        if !transitions_only {
            writer.write_interleaved(chunk)?;
            out_frames += n_frames_u64;
        }

        if transition_enabled && preroll_samples > 0 {
            lookback.extend_from_slice(chunk);
            if lookback.len() > preroll_samples {
                lookback.drain(0..lookback.len() - preroll_samples);
            }
        }

        rendered_frames += n_frames_u64;

        let audio_secs = rendered_frames as f64 / f64::from(sample_rate);
        let progress_secs = audio_secs as u64;
        if progress_secs >= last_progress_secs + 30 {
            println!("rendered {progress_secs}s...");
            last_progress_secs = progress_secs - (progress_secs % 30);
        }

        if render_speed > 0.0 {
            let target_wall = audio_secs / render_speed;
            let elapsed = wall_start.elapsed().as_secs_f64();
            if elapsed < target_wall {
                thread::sleep(Duration::from_secs_f64(target_wall - elapsed));
            }
        }
    }

    let stats = writer.finalize()?;
    let duration_secs = out_frames as f64 / f64::from(sample_rate);
    let peak_dbfs = if stats.peak > 0.0 {
        20.0 * f64::from(stats.peak).log10()
    } else {
        f64::NEG_INFINITY
    };
    println!(
        "wrote {} ({duration_secs:.1}s, format {:?})",
        out_path.display(),
        wav_format
    );
    println!(
        "peak level: {:.4} ({peak_dbfs:.2} dBFS); samples |x|>1: {}; frames with over: {}",
        stats.peak, stats.over_samples, stats.over_frames
    );

    if transition_enabled {
        let n_clips = next_transition_idx.saturating_sub(1);
        for cap in transition_captures {
            let _ = cap.writer.finalize();
        }
        println!(
            "wrote {} transition clips to {}",
            n_clips,
            transitions_dir.display()
        );
    }
    Ok(())
}

fn run_live(
    mut options: EngineOptions,
    playlist: Vec<PathBuf>,
    explicit_sample_rate: Option<u32>,
    dump_wav: Option<&Path>,
    wav_format: WavFormat,
    transition_clip_seconds: f64,
    transitions_only: bool,
    stop: &Arc<AtomicBool>,
) -> Result<()> {
    let playlist_len = playlist.len();
    let mut analysis = AnalysisPrinter::new(options.cache_dir.clone());

    let host = cpal::default_host();
    let device = host
        .default_output_device()
        .context("no default audio output device available")?;

    let (config, channels) = pick_output_config(&device, explicit_sample_rate)?;
    options.output_sample_rate = config.sample_rate;
    let sample_rate = config.sample_rate;

    // Same window as render transition clips: 8 bars before TransitionStarted.
    const TRANSITION_CLIP_PREROLL_BARS: u32 = 8;
    let mut transition_gate = if transitions_only {
        let clip_len = (transition_clip_seconds * f64::from(sample_rate)).round() as u64;
        let preroll =
            (options.bar_frames() * f64::from(TRANSITION_CLIP_PREROLL_BARS)).round() as u64;
        eprintln!(
            "transitions-only playback: {preroll} frame preroll (~{TRANSITION_CLIP_PREROLL_BARS} bars), \
             {clip_len} frame windows ({transition_clip_seconds}s)"
        );
        Some(TransitionPlayGate::new(preroll, clip_len))
    } else {
        None
    };

    let mut engine = Engine::new(options, playlist).map_err(|e| anyhow::anyhow!("engine: {e}"))?;
    // Audio callback must never sleep (preview→Upgrade wait would underrun under load).
    engine.set_realtime(true);
    let nav_tx = engine.nav_sender();

    // ponytail: try_lock dump; async ringbuf writer if dump still underruns
    let dump_path = dump_wav.map(|p| p.to_path_buf());
    let dump = match &dump_path {
        Some(path) => {
            let w = WavStreamWriter::create(path, sample_rate, wav_format)?;
            eprintln!(
                "dumping live mix to {} ({wav_format:?}, {sample_rate} Hz)",
                path.display()
            );
            Some(Arc::new(Mutex::new(w)))
        }
        None => None,
    };
    let dump_cb = dump.clone();

    let (event_tx, event_rx) = mpsc::channel::<EngineEvent>();
    let mut stereo_scratch = Vec::<f32>::new();
    // Skip engine.render while paused so playheads / transitions stay put.
    let paused = Arc::new(AtomicBool::new(false));
    let paused_cb = Arc::clone(&paused);

    match &config.buffer_size {
        BufferSize::Fixed(n) => eprintln!(
            "output {sample_rate} Hz, {channels} ch, ring buffer {n} frames (~{:.0} ms)",
            1000.0 * f64::from(*n) / f64::from(sample_rate)
        ),
        BufferSize::Default => {
            eprintln!("output {sample_rate} Hz, {channels} ch, buffer default")
        }
    }

    let stream = device
        .build_output_stream(
            config,
            move |data: &mut [f32], _| {
                if paused_cb.load(Ordering::SeqCst) {
                    data.fill(0.0);
                    return;
                }

                let frames = data.len() / channels as usize;
                let need = frames * 2;
                if stereo_scratch.len() < need {
                    stereo_scratch.resize(need, 0.0);
                }
                let stereo = &mut stereo_scratch[..need];
                stereo.fill(0.0);
                let n = engine.render(stereo);

                let into = engine.transition_frames_into().unwrap_or(0);
                let events = engine.poll_events();
                if let Some(gate) = transition_gate.as_mut() {
                    let mut offsets = Vec::new();
                    for e in &events {
                        if matches!(e, EngineEvent::TransitionStarted { .. }) {
                            let start_offset = if into == 0 {
                                0
                            } else {
                                n.saturating_sub(into as usize)
                            };
                            offsets.push(start_offset);
                        }
                    }
                    gate.process(&mut stereo[..n * 2], n, &offsets);
                }

                for i in 0..frames {
                    let (l, r) = if i < n {
                        (stereo[i * 2], stereo[i * 2 + 1])
                    } else {
                        (0.0, 0.0)
                    };
                    write_frame(data, i, channels, l, r);
                }

                // Same stereo bus that fed write_frame (zeros already filled past n).
                // try_lock: never block the audio thread on dump I/O.
                if let Some(dump) = &dump_cb {
                    if let Ok(mut w) = dump.try_lock() {
                        let _ = w.write_interleaved(stereo);
                    }
                }

                for event in events {
                    let _ = event_tx.send(event);
                }
            },
            // Same throttle as `--label-sections`: cpal's ALSA worker retries a
            // generic device error with no backoff, so one line per callback
            // means tens of thousands of lines a second (see
            // `funkot_cli::stream_error`). Raw mode is on once the key thread
            // starts, hence the `\r` framing.
            {
                let mut throttle = StreamErrorThrottle::new(stream_error::DEFAULT_SUMMARY_INTERVAL);
                move |err| {
                    if let Some(report) = throttle.record_with(Instant::now(), || err.to_string()) {
                        eprintln!("\r{}\r", report.to_line());
                    }
                }
            },
            None,
        )
        .context("failed to build audio output stream")?;

    stream.play().context("failed to start audio stream")?;
    let play_elapsed = Arc::new(Mutex::new(PlayElapsed::default()));
    eprintln!(
        "keys: Enter=pause  Left×1=restart  Left×2=prev  Left×3=prev intro  \
         Right×1=next  Right×2=next intro  (multi-tap ≤{}ms)  Ctrl+C=stop",
        MULTI_PRESS_WINDOW.as_millis()
    );

    // Raw keyboard: Enter pause, left/right multi-tap nav. Always restore raw mode.
    let paused_keys = Arc::clone(&paused);
    let play_elapsed_keys = Arc::clone(&play_elapsed);
    let stop_keys = Arc::clone(stop);
    let key_join = thread::spawn(move || {
        if let Err(e) = enable_raw_mode() {
            eprintln!("warn: raw mode unavailable ({e}); skip/rewind keys disabled");
            // Fallback: line-mode Enter pause only.
            let stdin = io::stdin();
            let mut line = String::new();
            loop {
                if stop_keys.load(Ordering::SeqCst) {
                    break;
                }
                line.clear();
                match stdin.lock().read_line(&mut line) {
                    Ok(0) => break,
                    Ok(_) => toggle_pause(&paused_keys, &play_elapsed_keys),
                    Err(_) => break,
                }
            }
            return;
        }
        let _raw_guard = RawModeGuard;
        let mut agg = MultiPressAggregator::new();
        while !stop_keys.load(Ordering::SeqCst) {
            let poll_ms = MULTI_PRESS_WINDOW.as_millis().min(50).max(10) as u64;
            match event::poll(Duration::from_millis(poll_ms)) {
                Ok(true) => match event::read() {
                    Ok(Event::Key(key)) => {
                        handle_key(
                            key,
                            &paused_keys,
                            &play_elapsed_keys,
                            &nav_tx,
                            &mut agg,
                            &stop_keys,
                        );
                    }
                    Ok(_) => {}
                    Err(_) => break,
                },
                Ok(false) => {
                    if let Some(action) = agg.poll_timeout(Instant::now()) {
                        let _ = nav_tx.try_send(action);
                    }
                }
                Err(_) => break,
            }
        }
        let _ = agg.flush();
    });

    let mut finished = false;
    while !stop.load(Ordering::SeqCst) && !finished {
        match event_rx.recv_timeout(Duration::from_millis(200)) {
            Ok(event) => {
                if matches!(event, EngineEvent::Finished) {
                    finished = true;
                }
                let mut clock = play_elapsed.lock().unwrap_or_else(|e| e.into_inner());
                print_event(&event, playlist_len, &mut clock, &mut analysis);
                analysis.poll();
            }
            Err(RecvTimeoutError::Timeout) => {
                analysis.poll();
            }
            Err(RecvTimeoutError::Disconnected) => break,
        }
    }

    stop.store(true, Ordering::SeqCst);
    let _ = key_join.join();
    drop(stream);
    if let (Some(dump), Some(path)) = (dump, dump_path) {
        let w = Arc::try_unwrap(dump)
            .map_err(|_| anyhow::anyhow!("dump writer still held after stream stop"))?
            .into_inner()
            .unwrap_or_else(|e| e.into_inner());
        let stats = w.finalize()?;
        println!(
            "wrote {} (format {:?}; peak {:.4})",
            path.display(),
            wav_format,
            stats.peak
        );
    }
    Ok(())
}

struct RawModeGuard;
impl Drop for RawModeGuard {
    fn drop(&mut self) {
        let _ = disable_raw_mode();
    }
}

fn toggle_pause(paused: &AtomicBool, play_elapsed: &Mutex<PlayElapsed>) {
    let now_paused = !paused.fetch_xor(true, Ordering::SeqCst);
    if let Ok(mut clock) = play_elapsed.lock() {
        clock.set_paused(now_paused);
    }
    // Raw mode: println needs \r
    if now_paused {
        println!("\rpaused");
    } else {
        println!("\rresumed");
    }
    let _ = io::stdout().flush();
}

fn handle_key(
    key: KeyEvent,
    paused: &AtomicBool,
    play_elapsed: &Mutex<PlayElapsed>,
    nav_tx: &mpsc::SyncSender<NavAction>,
    agg: &mut MultiPressAggregator,
    stop: &AtomicBool,
) {
    // Ignore key-up; act on Press (and Repeat for held keys — we still coalesce).
    if key.kind == KeyEventKind::Release {
        return;
    }
    if key.modifiers.contains(KeyModifiers::CONTROL)
        && matches!(key.code, KeyCode::Char('c') | KeyCode::Char('C'))
    {
        stop.store(true, Ordering::SeqCst);
        return;
    }
    match key.code {
        KeyCode::Enter => toggle_pause(paused, play_elapsed),
        KeyCode::Left => {
            if let Some(action) = agg.press(NavDir::Left, Instant::now()) {
                let _ = nav_tx.try_send(action);
            }
        }
        KeyCode::Right => {
            if let Some(action) = agg.press(NavDir::Right, Instant::now()) {
                let _ = nav_tx.try_send(action);
            }
        }
        _ => {}
    }
}

fn write_frame(data: &mut [f32], frame: usize, channels: u16, l: f32, r: f32) {
    let ch = channels as usize;
    let base = frame * ch;
    if ch == 1 {
        data[base] = (l + r) * 0.5;
        return;
    }
    data[base] = l;
    if ch > 1 {
        data[base + 1] = r;
    }
    for c in 2..ch {
        data[base + c] = 0.0;
    }
}

fn pick_output_config(
    device: &cpal::Device,
    explicit_sample_rate: Option<u32>,
) -> Result<(StreamConfig, u16)> {
    let default_config = device
        .default_output_config()
        .context("failed to query default audio output config")?;

    // WASAPI shared mode only accepts the device mix format without conversion
    // (cpal rejects S_FALSE). Prefer that config; only hunt when --sample-rate
    // is set or the mix format is not f32.
    //
    // Do not clamp into the first supported range: on WASAPI, supported rates
    // are listed as discrete COMMON_SAMPLE_RATES starting at 8000 Hz, and the
    // old clamp path opened at 8000 and failed with "not supported in shared mode".
    if let Some(rate) = explicit_sample_rate {
        if let Some(cfg) = select_f32_config(device, rate, 2)? {
            return Ok((cfg, cfg.channels));
        }
        if let Some(cfg) = select_f32_config(device, rate, default_config.channels())? {
            return Ok((cfg, cfg.channels));
        }
    }

    if default_config.sample_format() == SampleFormat::F32 {
        let mut cfg = default_config.config();
        cfg.buffer_size = stable_buffer_size(default_config.buffer_size());
        return Ok((cfg, cfg.channels));
    }

    let rate = default_config.sample_rate();
    if let Some(cfg) = select_f32_config(device, rate, 2)? {
        return Ok((cfg, cfg.channels));
    }
    if let Some(cfg) = select_f32_config(device, rate, default_config.channels())? {
        return Ok((cfg, cfg.channels));
    }

    bail!("no f32 output configuration available on device {device}");
}

/// ~170 ms @ 48 kHz. Auto-DJ tolerates this latency; small device periods
/// underrun when the callback is preempted under load.
const LIVE_BUFFER_FRAMES: u32 = 8192;

fn stable_buffer_size(supported: &SupportedBufferSize) -> BufferSize {
    match *supported {
        // Real range (ALSA / some devices): stay inside host limits.
        SupportedBufferSize::Range { min, max } if min < max => {
            BufferSize::Fixed(LIVE_BUFFER_FRAMES.clamp(min, max))
        }
        // WASAPI shared software stacks advertise min==max==GetDevicePeriod()
        // (~480 @ 48 kHz). That is the *callback* period, not an Initialize
        // ceiling — cpal still enlarges the ring buffer from Fixed(n). Clamping
        // to max here previously forced Fixed(480) and undid the whole point.
        SupportedBufferSize::Range { min, .. } => BufferSize::Fixed(LIVE_BUFFER_FRAMES.max(min)),
        SupportedBufferSize::Unknown => BufferSize::Fixed(LIVE_BUFFER_FRAMES),
    }
}

fn select_f32_config(
    device: &cpal::Device,
    sample_rate: u32,
    channels: u16,
) -> Result<Option<StreamConfig>> {
    let supported = device
        .supported_output_configs()
        .context("failed to enumerate output configs")?;
    for range in supported {
        if range.sample_format() != SampleFormat::F32 || range.channels() != channels {
            continue;
        }
        if let Some(supported) = range.try_with_sample_rate(sample_rate) {
            let mut cfg = supported.config();
            cfg.buffer_size = stable_buffer_size(supported.buffer_size());
            return Ok(Some(cfg));
        }
    }
    Ok(None)
}

fn fill_missing_cache(playlist: &[PathBuf], cache_dir: &Path) -> Result<()> {
    use funkot_core::cache;
    use funkot_core::decode::decode_file;

    let mut analyzed = 0usize;
    let mut skipped = 0usize;
    for path in playlist {
        let buf = decode_file(path).map_err(|e| anyhow::anyhow!("{e}"))?;
        let (_a, did) =
            cache::fill_missing(path, cache_dir, &buf).map_err(|e| anyhow::anyhow!("{e}"))?;
        if did {
            analyzed += 1;
            eprintln!("analyzed {}", path.display());
        } else {
            skipped += 1;
        }
    }
    eprintln!("fill-missing-cache: analyzed {analyzed} skipped {skipped} (complete cache hits)");
    Ok(())
}

// ---------------------------------------------------------------------
// `--label-sections`: interactive (or `--render-clips` offline) intro/outro
// ground-truth annotation. Candidate navigation / accept / skip / ambiguous
// -set state lives in `funkot_cli::label_session` (pure, unit-tested); this
// section only wires it to a playlist, `labels.tsv`, the cache, cpal, and
// crossterm.
// ---------------------------------------------------------------------

fn run_label_sections(
    playlist: &[PathBuf],
    labels_path: &Path,
    cache_dir: &Path,
    render_clips_dir: Option<&Path>,
    click_opts: &label_session::ClickOptions,
) -> Result<()> {
    let existing = if labels_path.exists() {
        funkot_core::labels::load_labels(labels_path).map_err(|e| anyhow::anyhow!("{e}"))?
    } else {
        Vec::new()
    };
    let labeled: std::collections::HashSet<String> =
        existing.into_iter().map(|l| l.hash).collect();

    let mut hashes = Vec::with_capacity(playlist.len());
    let mut skipped = 0usize;
    for path in playlist {
        let hash = cache::content_hash(path).map_err(|e| anyhow::anyhow!("{e}"))?;
        if labeled.contains(&hash) {
            skipped += 1;
        }
        hashes.push(hash);
    }
    eprintln!(
        "label-sections: {skipped} of {} already labeled (skipped), {} to do",
        playlist.len(),
        playlist.len() - skipped
    );

    if let Some(dir) = render_clips_dir {
        let todo: Vec<&PathBuf> = playlist
            .iter()
            .zip(&hashes)
            .filter(|(_, h)| !labeled.contains(*h))
            .map(|(p, _)| p)
            .collect();
        return render_label_clips(&todo, cache_dir, dir, click_opts);
    }

    run_label_sections_interactive(playlist, &hashes, &labeled, labels_path, cache_dir, click_opts)
}

/// `--render-clips DIR`: for every candidate on both sides of every track in
/// `playlist`, write the same click-track clip an interactive session would
/// have played, as WAV. No labels are written — this path exists so the
/// clip synthesis can be verified without an audio output device (e.g.
/// inside the dev container).
fn render_label_clips(
    playlist: &[&PathBuf],
    cache_dir: &Path,
    out_dir: &Path,
    click_opts: &label_session::ClickOptions,
) -> Result<()> {
    std::fs::create_dir_all(out_dir).with_context(|| format!("mkdir {}", out_dir.display()))?;
    for path in playlist {
        let buf = decode_file(path).map_err(|e| anyhow::anyhow!("{e}"))?;
        let analysis =
            cache::get_or_analyze(path, cache_dir, &buf).map_err(|e| anyhow::anyhow!("{e}"))?;
        let stem = path
            .file_stem()
            .and_then(|s| s.to_str())
            .unwrap_or("track");

        let mut n = 0usize;
        for side in [Side::Intro, Side::Outro] {
            for &bars in side.candidates() {
                let clip = label_session::build_candidate_clip(
                    &buf,
                    &analysis,
                    side,
                    bars,
                    label_session::NORMAL_HALF_WIDTH_BARS,
                    click_opts,
                );
                let clip_path =
                    out_dir.join(format!("{stem}_{}_{bars:03}bars.wav", side.label()));
                let mut w = WavStreamWriter::create(&clip_path, buf.sample_rate, WavFormat::F32)?;
                w.write_interleaved(&clip)?;
                w.finalize()?;
                n += 1;
            }
        }
        println!("wrote {n} candidate clips for {}", path.display());
    }
    Ok(())
}

/// Resample a `--label-sections` click clip from the source file's own
/// sample rate to the output device's rate. [`ClipPlayer`] streams whatever
/// buffer it's handed straight into the device callback with no rate
/// conversion of its own, so without this, a device default that differs
/// from the file's rate (e.g. a 48 kHz device against 44.1 kHz Funkot
/// masters) played every clip audibly fast/sharp or slow/flat. `speed` is
/// pinned to `1.0` and [`PitchMode::Shift`] used deliberately: this needs an
/// exact sample-rate match for correct playback speed, not a tempo change,
/// so the plain-resample path (reused from the loader's existing
/// [`funkot_core::stretch`]) is the right one, not the pitch-preserving
/// stretch.
fn resample_clip_for_device(clip: Vec<f32>, source_rate: u32, device_rate: u32) -> Vec<f32> {
    if source_rate == device_rate {
        return clip;
    }
    match funkot_core::stretch::render_track(&clip, source_rate, device_rate, 1.0, PitchMode::Shift)
    {
        Ok(resampled) => resampled,
        Err(e) => {
            eprintln!(
                "warn: could not resample label-sections clip {source_rate} Hz -> \
                 {device_rate} Hz ({e}); playing at source rate (pitch/tempo will be off)"
            );
            clip
        }
    }
}

#[cfg(test)]
mod resample_clip_for_device_tests {
    use super::*;

    fn stereo_sine(frames: usize, freq: f32, sr: u32) -> Vec<f32> {
        let mut out = vec![0.0f32; frames * 2];
        for i in 0..frames {
            let t = i as f32 / sr as f32;
            let s = (2.0 * std::f32::consts::PI * freq * t).sin() * 0.5;
            out[i * 2] = s;
            out[i * 2 + 1] = s;
        }
        out
    }

    #[test]
    fn matching_rates_pass_through_unchanged() {
        let clip = stereo_sine(2_000, 440.0, 44_100);
        let out = resample_clip_for_device(clip.clone(), 44_100, 44_100);
        assert_eq!(out, clip, "same source/device rate must be a no-op");
    }

    #[test]
    fn mismatched_rates_resample_to_the_device_length() {
        // The mismatch this fixes: a 44.1 kHz file on a 48 kHz device
        // (common WASAPI/CoreAudio default) previously played ~8.8% fast
        // with no rate conversion at all.
        let source_rate = 44_100;
        let device_rate = 48_000;
        let clip = stereo_sine(4_410, 440.0, source_rate); // 100 ms
        let out = resample_clip_for_device(clip, source_rate, device_rate);

        let expected_frames = 4_410 * device_rate as usize / source_rate as usize;
        let out_frames = out.len() / 2;
        assert!(
            out_frames.abs_diff(expected_frames) <= expected_frames / 50 + 8,
            "out_frames={out_frames} expected≈{expected_frames}"
        );
        assert!(out.iter().all(|s| s.is_finite()));
    }
}

/// Sentinel `pos` value meaning "not currently playing".
const CLIP_PLAYER_IDLE: usize = usize::MAX;

/// Wait this long before trying to reopen a torn-down output stream. A
/// wedged WSLg PulseAudio makes `snd_pcm_open` block for ~30 s (measured), and
/// it is the UI thread that would block, so failed reopens must not be retried
/// on every keystroke.
const CLIP_STREAM_RETRY_COOLDOWN: Duration = Duration::from_secs(10);

/// Printed once when a wedged stream is torn down.
const CLIP_STREAM_WEDGED_NOTE: &str =
    "audio output stopped (device kept failing); press r to retry playback";

/// Error bookkeeping shared between the cpal error callback (audio thread) and
/// the labeling UI thread.
///
/// The callback only *records*; the UI thread prints. That ordering matters:
/// `--label-sections` runs in crossterm raw mode, where a bare `\n` from
/// another thread leaves the cursor mid-column and staircases the status line.
/// Queuing the text keeps every line on the UI thread, which frames it
/// correctly.
struct ClipStreamErrors {
    throttle: StreamErrorThrottle,
    /// Lines the UI thread has not printed yet. Bounded by the throttle: at
    /// most one entry per [`stream_error::DEFAULT_SUMMARY_INTERVAL`].
    pending: Vec<String>,
}

impl ClipStreamErrors {
    fn new() -> Self {
        Self {
            throttle: StreamErrorThrottle::new(stream_error::DEFAULT_SUMMARY_INTERVAL),
            pending: Vec::new(),
        }
    }
}

/// Minimal single-buffer cpal player for `--label-sections` candidate
/// clips. Bypasses the mixing engine entirely (per plan constraints): the
/// audio callback just streams whichever interleaved-stereo f32 buffer
/// [`ClipPlayer::play`] last handed it, so a new candidate can interrupt
/// mid-playback the same way the live engine's nav keys do.
///
/// The stream is held open across the whole session rather than opened per
/// clip. Opening lazily was considered and rejected: WSLg's PulseAudio wedges
/// independently of whether anything is playing (its RDP sink loses the
/// Windows-side endpoint), and in that state `snd_pcm_open` blocks for ~30 s,
/// so a per-clip open would freeze the labeling UI for half a minute on every
/// replay while fixing nothing. Instead the failure is *survived*: see
/// [`ClipPlayer::poll`].
struct ClipPlayer {
    device: cpal::Device,
    config: StreamConfig,
    channels: u16,
    /// `None` while no stream is open, i.e. after a wedged one was torn down.
    /// Reopened on the next user-initiated playback.
    stream: Option<cpal::Stream>,
    buffer: Arc<Mutex<Vec<f32>>>,
    pos: Arc<AtomicUsize>,
    errors: Arc<Mutex<ClipStreamErrors>>,
    /// Error total already accounted for, so [`ClipPlayer::poll`] can measure
    /// a rate rather than a running count.
    seen_errors: u64,
    last_poll: Instant,
    /// Earliest time a reopen may be attempted, after one failed.
    retry_after: Option<Instant>,
}

impl ClipPlayer {
    fn new(device: cpal::Device, config: StreamConfig, channels: u16) -> Result<Self> {
        let mut player = Self {
            device,
            config,
            channels,
            stream: None,
            buffer: Arc::new(Mutex::new(Vec::<f32>::new())),
            pos: Arc::new(AtomicUsize::new(CLIP_PLAYER_IDLE)),
            errors: Arc::new(Mutex::new(ClipStreamErrors::new())),
            seen_errors: 0,
            last_poll: Instant::now(),
            retry_after: None,
        };
        // Fail fast at startup: a device that cannot be opened at all is a
        // setup problem the user needs to see before any track is decoded.
        player.open_stream()?;
        Ok(player)
    }

    fn open_stream(&mut self) -> Result<()> {
        let buffer_cb = Arc::clone(&self.buffer);
        let pos_cb = Arc::clone(&self.pos);
        let errors_cb = Arc::clone(&self.errors);
        let channels = self.channels;
        let stream = self
            .device
            .build_output_stream(
                self.config,
                move |data: &mut [f32], _| {
                    data.fill(0.0);
                    let p = pos_cb.load(Ordering::SeqCst);
                    if p == CLIP_PLAYER_IDLE {
                        return;
                    }
                    // try_lock: never block the audio thread on the UI
                    // thread's play()/stop() swap.
                    let Ok(buf) = buffer_cb.try_lock() else {
                        return;
                    };
                    let frames_total = buf.len() / 2;
                    let frames = data.len() / channels as usize;
                    let mut idx = p;
                    for i in 0..frames {
                        if idx >= frames_total {
                            break;
                        }
                        write_frame(data, i, channels, buf[idx * 2], buf[idx * 2 + 1]);
                        idx += 1;
                    }
                    pos_cb.store(
                        if idx >= frames_total {
                            CLIP_PLAYER_IDLE
                        } else {
                            idx
                        },
                        Ordering::SeqCst,
                    );
                },
                move |err| {
                    let now = Instant::now();
                    let mut errors = errors_cb.lock().unwrap_or_else(|e| e.into_inner());
                    // record_with: cpal can land here ~90k times a second, and
                    // formatting the error is the expensive part.
                    if let Some(report) = errors.throttle.record_with(now, || err.to_string()) {
                        errors.pending.push(report.to_line());
                    }
                },
                None,
            )
            .context("failed to build label-sections audio output stream")?;
        stream
            .play()
            .context("failed to start label-sections audio stream")?;
        self.stream = Some(stream);
        Ok(())
    }

    /// Drain queued error lines, and tear the stream down if the device is
    /// producing errors far faster than any real glitch could.
    ///
    /// Tearing down is what actually stops the damage: cpal's ALSA worker
    /// retries a generic error with no backoff, so a wedged device pins a CPU
    /// core and calls the error callback forever. Dropping the stream sets
    /// cpal's `dropping` flag and joins that worker.
    fn poll(&mut self) -> Vec<String> {
        let now = Instant::now();
        let total = {
            let errors = self.errors.lock().unwrap_or_else(|e| e.into_inner());
            errors.throttle.total()
        };
        let since = total.saturating_sub(self.seen_errors);
        let elapsed = now.duration_since(self.last_poll);
        self.seen_errors = total;
        self.last_poll = now;
        if self.stream.is_some() && stream_error::looks_wedged(since, elapsed) {
            self.shutdown_wedged_stream(now);
        }
        let mut errors = self.errors.lock().unwrap_or_else(|e| e.into_inner());
        std::mem::take(&mut errors.pending)
    }

    fn shutdown_wedged_stream(&mut self, now: Instant) {
        // Drop *before* touching `errors`: the drop joins cpal's worker
        // thread, which takes that same lock in the error callback.
        drop(self.stream.take());
        self.pos.store(CLIP_PLAYER_IDLE, Ordering::SeqCst);
        {
            let mut errors = self.errors.lock().unwrap_or_else(|e| e.into_inner());
            if let Some(report) = errors.throttle.flush(now) {
                errors.pending.push(report.to_line());
            }
            errors.pending.push(CLIP_STREAM_WEDGED_NOTE.to_string());
            errors.throttle.reset();
        }
        self.seen_errors = 0;
        self.retry_after = Some(now + CLIP_STREAM_RETRY_COOLDOWN);
    }

    /// Reopen after a teardown, if the cooldown has passed. Queues a note
    /// either way so the user learns why `r` did nothing.
    fn ensure_stream(&mut self) {
        if self.stream.is_some() {
            return;
        }
        let now = Instant::now();
        if let Some(at) = self.retry_after {
            if now < at {
                self.note(format!(
                    "audio output unavailable; retrying in {:.0}s",
                    at.duration_since(now).as_secs_f64().ceil()
                ));
                return;
            }
        }
        match self.open_stream() {
            Ok(()) => {
                self.retry_after = None;
                self.last_poll = Instant::now();
                self.note("audio output restarted".to_string());
            }
            Err(e) => {
                self.retry_after = Some(Instant::now() + CLIP_STREAM_RETRY_COOLDOWN);
                self.note(format!("audio output unavailable: {e:#}"));
            }
        }
    }

    fn note(&self, line: String) {
        let mut errors = self.errors.lock().unwrap_or_else(|e| e.into_inner());
        errors.pending.push(line);
    }

    /// Stop whatever is playing and start `clip` from the top.
    fn play(&mut self, clip: Vec<f32>) {
        self.ensure_stream();
        self.pos.store(CLIP_PLAYER_IDLE, Ordering::SeqCst);
        {
            let mut buf = self.buffer.lock().unwrap_or_else(|e| e.into_inner());
            *buf = clip;
        }
        if self.stream.is_some() {
            self.pos.store(0, Ordering::SeqCst);
        }
    }

    fn stop(&self) {
        self.pos.store(CLIP_PLAYER_IDLE, Ordering::SeqCst);
    }
}

const LABEL_SECTIONS_KEY_HELP: &str =
    "keys: y/Enter=accept  \u{2190}/\u{2192}=candidate  +=widen/narrow window  \
     r=replay  a=ambiguous(toggle set)  n=note  s=skip track  q=save & quit";

fn print_label_key_help() {
    println!("\r{LABEL_SECTIONS_KEY_HELP}\r");
}

/// Print audio-subsystem notices from the UI thread. Raw mode is on, so every
/// line needs the same `\r` framing the rest of this UI uses -- a bare
/// `eprintln!` from the cpal thread would staircase the display.
fn print_label_audio(lines: &[String]) {
    for line in lines {
        eprintln!("\r{line}\r");
    }
}

fn print_label_track_header(progress: &str, path: &Path) {
    println!("\r{progress} {}\r", file_name(path));
}

fn print_label_status(session: &TrackSession) {
    let mut line = format!(
        "\r  [{}] candidate={} bars  window=\u{b1}{} bars",
        session.current_side().label(),
        session.current_bars(),
        session.context_half_width_bars(),
    );
    if let Some(selected) = session.ambiguous_selected() {
        let set = selected
            .iter()
            .map(|v| v.to_string())
            .collect::<Vec<_>>()
            .join("|");
        line.push_str(&format!("  ambiguous-set={set}"));
    }
    if !session.note().is_empty() {
        line.push_str(&format!("  note=\"{}\"", session.note()));
    }
    line.push_str(&format!("  | {LABEL_SECTIONS_KEY_HELP}\r"));
    println!("{line}");
}

/// Read one line from stdin for `n` (note entry). Raw mode is disabled for
/// the duration so the terminal echoes normally, then restored.
fn read_note_line() -> Result<String> {
    disable_raw_mode().ok();
    print!("\rnote: ");
    io::stdout().flush().ok();
    let mut line = String::new();
    io::stdin().read_line(&mut line)?;
    enable_raw_mode().context("failed to re-enable raw mode after note entry")?;
    Ok(line.trim_end_matches(['\n', '\r']).to_string())
}

/// Decode one crossterm key press into a [`LabelKey`], or `None` for a key
/// with no meaning here. `n` needs a blocking line read (outside raw mode)
/// to collect the note text, so it's resolved here rather than deferred.
fn map_label_key(key: KeyEvent) -> Result<Option<LabelKey>> {
    if key.kind == KeyEventKind::Release {
        return Ok(None);
    }
    if key.modifiers.contains(KeyModifiers::CONTROL)
        && matches!(key.code, KeyCode::Char('c') | KeyCode::Char('C'))
    {
        return Ok(Some(LabelKey::Quit));
    }
    Ok(match key.code {
        KeyCode::Enter | KeyCode::Char('y') | KeyCode::Char('Y') => Some(LabelKey::Accept),
        KeyCode::Left => Some(LabelKey::Left),
        KeyCode::Right => Some(LabelKey::Right),
        KeyCode::Char('+') => Some(LabelKey::ToggleWidth),
        KeyCode::Char('r') | KeyCode::Char('R') => Some(LabelKey::Replay),
        KeyCode::Char('a') | KeyCode::Char('A') => Some(LabelKey::Ambiguous),
        KeyCode::Char('n') | KeyCode::Char('N') => Some(LabelKey::Note(read_note_line()?)),
        KeyCode::Char('s') | KeyCode::Char('S') => Some(LabelKey::Skip),
        KeyCode::Char('q') | KeyCode::Char('Q') => Some(LabelKey::Quit),
        _ => None,
    })
}

/// Persist one finished track: append/replace its row in `labels.tsv`, then
/// reflect the intro side onto the analysis cache.
fn save_label_and_cache(
    labels_path: &Path,
    cache_dir: &Path,
    hash: &str,
    path: &Path,
    intro: &label_session::LabelChoice,
    outro: &label_session::LabelChoice,
    note: &str,
) -> Result<()> {
    let label = SectionLabel {
        hash: hash.to_string(),
        file_name: file_name(path),
        intro_best: intro.best,
        intro_ok: intro.ok.clone(),
        outro_best: outro.best,
        outro_ok: outro.ok.clone(),
        note: note.to_string(),
    };
    upsert_label(labels_path, label).map_err(|e| anyhow::anyhow!("{e}"))?;

    // Reflect the intro side onto the cache immediately (so live/render
    // playback picks it up, and `--purge-auto-cache` keeps it). The outro
    // side is deliberately *not* written here: `cache::set_manual_bars`'s
    // `outro` argument sets `TrackAnalysis::outro_bars`, the DJ mix-trigger,
    // which is *not* a fixed offset from the structural boundary this tool
    // collects (see `funkot_core::labels` module docs and the
    // `outro_structure_bars` doc comment in `funkot-core/src/lib.rs`).
    // Synthesizing a lead-in here to convert one into the other would plant
    // a guessed value in the cache that the analyzer itself doesn't stand
    // behind -- the exact outro_bars/outro_structure_bars conflation Stage 0
    // introduced `outro_structure_bars` to avoid. The cached outro stays
    // whatever `analysis::analyze` computed; only the label file records the
    // structural ground truth, for Stage 2+ to use.
    cache::set_manual_bars(cache_dir, hash, Some(intro.best), None)
        .map_err(|e| anyhow::anyhow!("{e}"))?;
    Ok(())
}

fn run_label_sections_interactive(
    playlist: &[PathBuf],
    hashes: &[String],
    labeled: &std::collections::HashSet<String>,
    labels_path: &Path,
    cache_dir: &Path,
    click_opts: &label_session::ClickOptions,
) -> Result<()> {
    let host = cpal::default_host();
    let device = host
        .default_output_device()
        .context("no default audio output device available (use --render-clips instead)")?;
    let (config, channels) = pick_output_config(&device, None)?;
    let device_rate = config.sample_rate;
    let mut player = ClipPlayer::new(device, config, channels)?;

    enable_raw_mode().context("failed to enable raw mode (needed for --label-sections)")?;
    let _raw_guard = RawModeGuard;

    print_label_key_help();

    let total = playlist.len();
    'tracks: for (i, path) in playlist.iter().enumerate() {
        let hash = &hashes[i];
        if labeled.contains(hash) {
            continue;
        }
        let progress = format!("[{}/{total}]", i + 1);

        let buf = decode_file(path).map_err(|e| anyhow::anyhow!("{e}"))?;
        let analysis =
            cache::get_or_analyze(path, cache_dir, &buf).map_err(|e| anyhow::anyhow!("{e}"))?;
        // Decode + analyze can run for a minute with no key polling; check the
        // stream now so a device that died meanwhile is torn down here rather
        // than spinning until the next keystroke.
        print_label_audio(&player.poll());
        if buf.sample_rate != device_rate {
            eprintln!(
                "note: {} is {} Hz, output device is {device_rate} Hz; resampling clips for playback",
                file_name(path),
                buf.sample_rate
            );
        }

        let mut session = TrackSession::new(analysis.intro_bars, analysis.outro_structure_bars);
        let build_clip = |session: &TrackSession| -> Vec<f32> {
            let clip = label_session::build_candidate_clip(
                &buf,
                &analysis,
                session.current_side(),
                session.current_bars(),
                session.context_half_width_bars(),
                click_opts,
            );
            resample_clip_for_device(clip, buf.sample_rate, device_rate)
        };

        print_label_track_header(&progress, path);
        player.play(build_clip(&session));
        print_label_status(&session);

        loop {
            let waiting = !event::poll(Duration::from_millis(100)).unwrap_or(false);
            // Runs on every tick, including the idle ones: this is what keeps a
            // failing device from flooding the terminal while the user thinks.
            print_label_audio(&player.poll());
            if waiting {
                continue;
            }
            let ev = match event::read() {
                Ok(ev) => ev,
                Err(_) => break 'tracks,
            };
            let Event::Key(key) = ev else { continue };
            let Some(label_key) = map_label_key(key)? else {
                continue;
            };
            match session.apply_key(label_key) {
                LabelOutcome::Continue => print_label_status(&session),
                LabelOutcome::Replay => {
                    player.play(build_clip(&session));
                    print_label_status(&session);
                }
                LabelOutcome::Skip => {
                    player.stop();
                    println!("\r  skipped {}\r", file_name(path));
                    continue 'tracks;
                }
                LabelOutcome::Quit => {
                    player.stop();
                    println!("\rsaved & quit\r");
                    break 'tracks;
                }
                LabelOutcome::Done { intro, outro } => {
                    player.stop();
                    save_label_and_cache(
                        labels_path,
                        cache_dir,
                        hash,
                        path,
                        &intro,
                        &outro,
                        session.note(),
                    )?;
                    println!(
                        "\r  saved {} (intro={} outro={})\r",
                        file_name(path),
                        intro.best,
                        outro.best
                    );
                    continue 'tracks;
                }
            }
        }
    }
    Ok(())
}

fn gen_test_fixtures(dir: &Path) -> Result<()> {
    use funkot_core::analysis::analyze;
    use funkot_core::testutil::{synth_track, synth_track_with_options, write_wav, SynthOptions};
    use serde_json::json;

    std::fs::create_dir_all(dir).with_context(|| format!("mkdir {}", dir.display()))?;
    let sr = 44_100u32;

    // Short sections: enough for 8/16 detection, small enough for optional on-disk WAV.
    let specs: Vec<(&str, SynthOptions, serde_json::Value)> = vec![
        (
            "classic_180_i8_m8_o8.wav",
            SynthOptions {
                bpm: 180.0,
                intro_bars: 8,
                main_bars: 8,
                outro_bars: 8,
                sample_rate: sr,
                ..SynthOptions::default()
            },
            json!({
                "intro_bars": 8,
                "outro_bars": 8,
                "first_downbeat_secs": 0.0,
                "first_downbeat_tol_secs": 0.05,
                "intro_bpm": 180.0,
                "bpm_tol": 0.3,
                "outro_start_bars_from_fd": 16,
                "outro_start_tol_secs": 0.12
            }),
        ),
        (
            "leadin_180_i8_m8_o8.wav",
            SynthOptions {
                bpm: 180.0,
                intro_bars: 8,
                main_bars: 8,
                outro_bars: 8,
                sample_rate: sr,
                lead_in_secs: 0.25,
                ..SynthOptions::default()
            },
            json!({
                "intro_bars": 8,
                "outro_bars": 8,
                "first_downbeat_secs": 0.25,
                "first_downbeat_tol_secs": 0.05,
                "intro_bpm": 180.0,
                "bpm_tol": 0.3,
                "outro_start_bars_from_fd": 16,
                "outro_start_tol_secs": 0.12
            }),
        ),
        (
            "unequal_178_i16_m8_o8.wav",
            SynthOptions {
                bpm: 178.0,
                intro_bars: 16,
                main_bars: 8,
                outro_bars: 8,
                sample_rate: sr,
                ..SynthOptions::default()
            },
            json!({
                "intro_bars_min": 8,
                "outro_bars_max": 16,
                "require_intro_ge_outro": true,
                "first_downbeat_secs": 0.0,
                "first_downbeat_tol_secs": 0.05,
                "intro_bpm": 178.0,
                "bpm_tol": 0.3,
                "outro_start_tol_secs": 0.20
            }),
        ),
    ];

    let mut golden = Vec::new();
    for (name, opt, expect) in specs {
        let path = dir.join(name);
        let buf = synth_track_with_options(opt.clone());
        write_wav(&path, &buf).with_context(|| format!("write {}", path.display()))?;
        let a = analyze(&buf, name).map_err(|e| anyhow::anyhow!("analyze {name}: {e}"))?;
        println!(
            "wrote {} ({} bytes, fd={} bars={}/{} bpm={:.3})",
            path.display(),
            std::fs::metadata(&path).map(|m| m.len()).unwrap_or(0),
            a.first_downbeat,
            a.intro_bars,
            a.outro_bars,
            a.intro_bpm
        );
        golden.push(json!({
            "file": name,
            "synth": {
                "bpm": opt.bpm,
                "intro_bars": opt.intro_bars,
                "main_bars": opt.main_bars,
                "outro_bars": opt.outro_bars,
                "sample_rate": opt.sample_rate,
                "lead_in_secs": opt.lead_in_secs,
            },
            "expect": expect,
        }));
    }

    let demo = dir.join("synth_classic_short.wav");
    write_wav(&demo, &synth_track(180.0, 8, 8, 8, sr))?;
    println!("wrote {}", demo.display());

    let golden_path = dir.join("golden.json");
    let doc = json!({
        "version": 1,
        "sample_rate": sr,
        "regen": "./dev.sh cargo run -p funkot-cli --release -- --gen-test-fixtures funkot-core/tests/fixtures",
        "note": "Tests synthesize from each track.synth recipe; WAV files are optional listen/debug artifacts and are gitignored.",
        "tracks": golden,
    });
    std::fs::write(&golden_path, serde_json::to_string_pretty(&doc)?)?;
    println!("wrote {}", golden_path.display());

    std::fs::write(
        dir.join("README.md"),
        "# Analysis CI fixtures\n\n\
`golden.json` holds synth recipes + tolerances for downbeat / section tests.\n\
WAV files are optional (gitignored); tests synthesize in memory from `synth`.\n\n\
```sh\n\
./dev.sh cargo run -p funkot-cli --release -- --gen-test-fixtures funkot-core/tests/fixtures\n\
./dev.sh cargo test -p funkot-core --release --test analysis_golden\n\
```\n",
    )?;
    Ok(())
}
