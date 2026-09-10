//! funkot-autodj CLI: live playback via cpal, or offline WAV render.

use std::io::{self, BufRead, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::mpsc::{self, RecvTimeoutError};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use anyhow::{bail, Context, Result};
use clap::{ArgGroup, Parser};
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
// The two modes that synthesize a click track, as one group so `--click-db`
// and `--click-duck-db` can `requires` it. Without the group each would have
// to name a single mode, and dropping the requirement outright (the obvious
// way to share them) makes a `--click-db` passed to plain playback silently
// do nothing -- exactly the failure that wastes an evening on "I turned the
// clicks up and nothing got louder". `multiple = false` also keeps the two
// modes mutually exclusive, which is why neither carries a `conflicts_with`.
#[command(group(ArgGroup::new("click_modes").args(["label_sections", "survey"]).multiple(false)))]
struct Args {
    /// Audio files in play order
    files: Vec<PathBuf>,

    /// Playlist file: one path per line (# comments / blank lines ignored;
    /// absolute paths used as-is, relative ones resolved against the list
    /// file's own directory, not the working directory)
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
    #[arg(long = "highpass-hz", default_value_t = 300.0)]
    highpass_hz: f32,

    /// Removed: this named the inverse filter. Retained only to give callers a
    /// migration error instead of silently applying an unintended effect.
    #[arg(long = "lpf-hz", hide = true)]
    removed_lpf_hz: Option<f32>,

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

    /// Interactive outro-grid survey: for each track, play a window over the
    /// last `label_session::SURVEY_WINDOW_BARS` bars of the outro (clicking
    /// every beat, not just every bar head) and record a one-key judgement of
    /// whether the click grid sits on the beat, then move to the next track.
    /// Tracks come from `-l/--list` or the positional FILES, same as normal
    /// playback. Does not read or write `--labels`; verdicts go to
    /// `--survey-out` instead. See `funkot_cli::label_session` for the clip
    /// synthesis this plays.
    #[arg(long = "survey", requires = "survey_out")]
    survey: bool,

    /// Verdict TSV path for `--survey` (required with that flag). A track
    /// already judged there (matched by content hash, the same key
    /// `--labels` uses) is skipped on the next run.
    #[arg(long = "survey-out", value_name = "FILE")]
    survey_out: Option<PathBuf>,

    /// Click peak level, in dB above the clip's own RMS loudness (not a
    /// fixed absolute amplitude — real Funkot masters run hot enough that a
    /// fixed number either got lost or clipped), for `--label-sections` and
    /// `--survey` alike. Raise this if clicks are still hard to hear on a
    /// given track.
    #[arg(
        long,
        default_value_t = label_session::ClickOptions::default().click_db_above_rms,
        requires = "click_modes"
    )]
    click_db: f32,

    /// How many dB to duck the music under each click (sidechain-style, so
    /// the click cuts through dense/loud material), for `--label-sections`
    /// and `--survey` alike. `--label-sections`' boundary click ducks deeper
    /// and longer automatically on top of this, so it stays distinguishable
    /// from a normal bar head.
    #[arg(
        long,
        default_value_t = label_session::ClickOptions::default().duck_db,
        requires = "click_modes"
    )]
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
    if args.removed_lpf_hz.is_some() {
        bail!("--lpf-hz has been removed because it named the inverse filter; use --highpass-hz HZ instead");
    }
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
        validate_rate(args.rate)?;
        // clap's `requires` guarantees this is Some.
        let labels_path = args.labels.clone().expect("--labels required by clap");
        let playlist = resolve_playlist(&args)?;
        let click_opts = label_session::ClickOptions {
            click_db_above_rms: args.click_db,
            duck_db: args.click_duck_db,
        };
        let pitch_mode = if args.pitch_shift {
            PitchMode::Shift
        } else {
            PitchMode::Preserve
        };
        return run_label_sections(
            &playlist,
            &labels_path,
            &args.cache_dir,
            args.render_clips.as_deref(),
            &click_opts,
            args.rate,
            pitch_mode,
        );
    }

    if args.survey {
        validate_rate(args.rate)?;
        // clap's `requires` guarantees this is Some.
        let survey_out = args.survey_out.clone().expect("--survey-out required by clap");
        let playlist = resolve_playlist(&args)?;
        let click_opts = label_session::ClickOptions {
            click_db_above_rms: args.click_db,
            duck_db: args.click_duck_db,
        };
        let pitch_mode = if args.pitch_shift {
            PitchMode::Shift
        } else {
            PitchMode::Preserve
        };
        return run_survey(
            &playlist,
            &survey_out,
            &args.cache_dir,
            &click_opts,
            args.rate,
            pitch_mode,
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

/// Shared by `build_options` (normal playback) and the `--label-sections`
/// branch of `run()`, which never calls `build_options` and so previously
/// left `--rate` unvalidated on that path.
fn validate_rate(rate: f64) -> Result<()> {
    if !rate.is_finite() || !(0.5..=2.0).contains(&rate) {
        bail!("--rate must be finite and in [0.5, 2.0], got {}", rate);
    }
    Ok(())
}

fn build_options(args: &Args) -> Result<EngineOptions> {
    validate_rate(args.rate)?;
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
        head_only_secs: None,
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
    rate: f64,
    pitch_mode: PitchMode,
) -> Result<()> {
    let existing = if labels_path.exists() {
        funkot_core::labels::load_labels(labels_path).map_err(|e| anyhow::anyhow!("{e}"))?
    } else {
        Vec::new()
    };
    // A row only counts as "labeled" (and so gets skipped on the next run)
    // once both sides are confirmed. A row with only a note and/or one side
    // set (see `save_pending_note` / `save_label_and_cache`) is still missing
    // information, so the track must be offered again.
    let labeled: std::collections::HashSet<String> = existing
        .into_iter()
        .filter(|l| l.intro_best.is_some() && l.outro_best.is_some())
        .map(|l| l.hash)
        .collect();

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

    run_label_sections_interactive(
        playlist,
        &hashes,
        &labeled,
        labels_path,
        cache_dir,
        click_opts,
        rate,
        pitch_mode,
    )
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
        let mut side_grids = label_session::SideGrids::default();
        for side in [Side::Intro, Side::Outro] {
            for &bars in side.candidates() {
                let grid = side_grids.get(&buf, &analysis, side);
                let clip = label_session::build_candidate_clip_on_grid(
                    &buf,
                    &analysis,
                    side,
                    bars,
                    label_session::NORMAL_HALF_WIDTH_BARS,
                    click_opts,
                    grid,
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

/// Highest instantaneous sample magnitude label-sections playback is allowed
/// to reach after speed/pitch/rate conversion. Same value and same reasoning
/// as `label_session::CLIP_SAFETY_CEILING` (slightly below 1.0 for float
/// rounding margin, not because 1.0 itself is a problem) -- that constant
/// caps what the click-track synthesis in `label_session.rs` produces before
/// this stage ever sees it; this one caps what `render_track` can do to a
/// clip that came in under that ceiling.
const PLAYBACK_PEAK_CEILING: f32 = 0.97;

/// Scale `samples` down by a single factor if their peak magnitude exceeds
/// [`PLAYBACK_PEAK_CEILING`]; a no-op otherwise.
///
/// Needed because `PitchMode::Preserve` at `--rate` 1.10 measurably pushes
/// hot Funkot masters over full scale: on a real clip (`03. KazuyaP -
/// Monitoring Db.flac`, intro 16 bars, source peak 0.991) `Preserve` at 1.10
/// came out at peak 1.749 with 0.1255% of samples over 1.0, i.e. audibly
/// clipped at the device. Production mixing never sees this because the
/// engine applies `analysis.gain_db` (RMS-normalizing gain toward
/// `TARGET_RMS_DBFS`, see `gain_linear` in `funkot-core/src/engine.rs`)
/// before the mix bus reaches the device; label clips are built straight
/// from the source material and never go through that gain stage, so
/// nothing else in this path stops the peak from exceeding full scale.
///
/// Scales the whole clip by one scalar rather than per-sample limiting, so
/// the click/music balance `label_session`'s synthesis decided (via
/// `click_db_above_rms` and the sidechain ducking) is preserved exactly --
/// only the overall level moves.
fn clamp_playback_peak(samples: &mut [f32]) {
    let peak = samples.iter().fold(0.0f32, |m, &s| m.max(s.abs()));
    if peak <= PLAYBACK_PEAK_CEILING {
        return;
    }
    let scale = PLAYBACK_PEAK_CEILING / peak;
    for s in samples.iter_mut() {
        *s *= scale;
    }
}

/// Prepare a `--label-sections` click clip for playback: apply the `--rate`
/// playback speed and, in the same pass, match the output device's sample
/// rate. [`ClipPlayer`] streams whatever buffer it's handed straight into
/// the device callback with no rate conversion of its own, so both of these
/// have to happen here or not at all.
///
/// Playback runs at `speed` (normally `--rate`, default 1.10) rather than
/// 1.0 because production DJ playback is always sped up by that factor, and
/// labeling by ear at the production speed is more representative than
/// labeling at the source tempo. This is a fixed multiplier on the
/// material's own tempo, deliberately *not* the engine's 198 BPM
/// normalization (`180 * rate / intro_bpm`, see `EngineOptions`/loader):
/// tying the labeling speed to BPM estimation would mean a track whose BPM
/// was mis-detected at half or double time gets labeled at 2x speed by
/// accident. The label clips are built from the source material, which is
/// nominally 180 BPM, so in practice this comes out to the same multiplier
/// as the engine's normalization anyway.
///
/// Device sample-rate matching is unconditional whenever `source_rate !=
/// device_rate` (e.g. a 48 kHz device against 44.1 kHz Funkot masters);
/// without it every clip played audibly fast/sharp or slow/flat. It rides
/// along in the same [`funkot_core::stretch::render_track`] call as the
/// speed change rather than a separate resample step.
///
/// Measured cost (106 s clip, 44.1 kHz → 48 kHz, release build, this dev
/// environment): `PitchMode::Preserve` 1.67 s, `PitchMode::Shift` 0.46 s,
/// vs. 0.54 s for the old speed-1.0 resample-only path. So the default
/// (pitch preserved, matching production) adds roughly 1.2 s of wait per
/// keystroke; `--pitch-shift` does not.
///
/// Runs the result through [`clamp_playback_peak`] before returning: see
/// that function for why a speed change alone can drive a clip over full
/// scale here even though nothing else in this path does.
fn prepare_clip_for_playback(
    clip: Vec<f32>,
    source_rate: u32,
    device_rate: u32,
    speed: f64,
    pitch_mode: PitchMode,
) -> Vec<f32> {
    if source_rate == device_rate && speed == 1.0 {
        return clip;
    }
    match funkot_core::stretch::render_track(&clip, source_rate, device_rate, speed, pitch_mode) {
        Ok(mut rendered) => {
            clamp_playback_peak(&mut rendered);
            rendered
        }
        Err(e) => {
            eprintln!(
                "warn: could not prepare label-sections clip for playback ({source_rate} Hz -> \
                 {device_rate} Hz, speed {speed}, {e}); playing at source rate and speed \
                 (pitch/tempo will be off)"
            );
            clip
        }
    }
}

#[cfg(test)]
mod prepare_clip_for_playback_tests {
    use super::*;

    fn stereo_sine(frames: usize, freq: f32, sr: u32, amp: f32) -> Vec<f32> {
        let mut out = vec![0.0f32; frames * 2];
        for i in 0..frames {
            let t = i as f32 / sr as f32;
            let s = (2.0 * std::f32::consts::PI * freq * t).sin() * amp;
            out[i * 2] = s;
            out[i * 2 + 1] = s;
        }
        out
    }

    #[test]
    fn matching_rates_and_unit_speed_pass_through_unchanged() {
        let clip = stereo_sine(2_000, 440.0, 44_100, 0.5);
        let out =
            prepare_clip_for_playback(clip.clone(), 44_100, 44_100, 1.0, PitchMode::Preserve);
        assert_eq!(out, clip, "same source/device rate and speed 1.0 must be a no-op");
    }

    #[test]
    fn mismatched_rates_resample_to_the_device_length() {
        // The mismatch this fixes: a 44.1 kHz file on a 48 kHz device
        // (common WASAPI/CoreAudio default) previously played ~8.8% fast
        // with no rate conversion at all.
        let source_rate = 44_100;
        let device_rate = 48_000;
        let clip = stereo_sine(4_410, 440.0, source_rate, 0.5); // 100 ms
        let out =
            prepare_clip_for_playback(clip, source_rate, device_rate, 1.0, PitchMode::Preserve);

        let expected_frames = 4_410 * device_rate as usize / source_rate as usize;
        let out_frames = out.len() / 2;
        assert!(
            out_frames.abs_diff(expected_frames) <= expected_frames / 50 + 8,
            "out_frames={out_frames} expected≈{expected_frames}"
        );
        assert!(out.iter().all(|s| s.is_finite()));
    }

    #[test]
    fn same_rate_speed_1_10_preserve_shortens_by_the_rate() {
        // Amplitude 0.95, not 0.5: the stretch overshoots its input peak by
        // about 1.35x (measured), so 0.5 comes out at ~0.676 and never reaches
        // the ceiling -- the peak assertion below would then hold even if
        // `prepare_clip_for_playback` stopped clamping at all.
        let sr = 44_100;
        let clip = stereo_sine(sr as usize, 440.0, sr, 0.95); // 1s
        let out = prepare_clip_for_playback(clip.clone(), sr, sr, 1.10, PitchMode::Preserve);

        let in_frames = clip.len() / 2;
        let expected_frames = (in_frames as f64 / 1.10).round() as usize;
        let out_frames = out.len() / 2;
        assert!(
            out_frames.abs_diff(expected_frames) <= expected_frames / 50 + 8,
            "out_frames={out_frames} expected≈{expected_frames}"
        );
        assert!(out.iter().all(|s| s.is_finite()));
        let peak = out.iter().fold(0.0f32, |m, &s| m.max(s.abs()));
        assert!(
            (peak - PLAYBACK_PEAK_CEILING).abs() < 1e-6,
            "peak {peak} must equal the ceiling (clamped)"
        );
    }

    #[test]
    fn mismatched_rates_and_speed_1_10_combine_both_effects() {
        // Amplitude 0.95 for the same reason as the same-rate case above: at
        // 0.5 the stretch's ~1.35x overshoot stops short of the ceiling and the
        // peak assertion stops testing anything.
        let source_rate = 44_100;
        let device_rate = 48_000;
        let clip = stereo_sine(source_rate as usize, 440.0, source_rate, 0.95); // 1s
        let out = prepare_clip_for_playback(
            clip.clone(),
            source_rate,
            device_rate,
            1.10,
            PitchMode::Preserve,
        );

        let in_frames = clip.len() / 2;
        let expected_frames =
            (in_frames as f64 * device_rate as f64 / source_rate as f64 / 1.10).round() as usize;
        let out_frames = out.len() / 2;
        assert!(
            out_frames.abs_diff(expected_frames) <= expected_frames / 50 + 8,
            "out_frames={out_frames} expected≈{expected_frames}"
        );
        assert!(out.iter().all(|s| s.is_finite()));
        let peak = out.iter().fold(0.0f32, |m, &s| m.max(s.abs()));
        assert!(
            (peak - PLAYBACK_PEAK_CEILING).abs() < 1e-6,
            "peak {peak} must equal the ceiling (clamped)"
        );
    }

    #[test]
    fn preserve_and_shift_agree_on_output_length() {
        let sr = 44_100;
        let clip = stereo_sine(sr as usize, 440.0, sr, 0.5); // 1s
        let preserve =
            prepare_clip_for_playback(clip.clone(), sr, sr, 1.10, PitchMode::Preserve);
        let shift = prepare_clip_for_playback(clip, sr, sr, 1.10, PitchMode::Shift);

        let preserve_frames = preserve.len() / 2;
        let shift_frames = shift.len() / 2;
        assert!(
            preserve_frames.abs_diff(shift_frames) <= shift_frames / 50 + 8,
            "preserve={preserve_frames} shift={shift_frames}"
        );
    }

    #[test]
    fn clamp_playback_peak_scales_an_over_ceiling_clip_down_and_keeps_ratios() {
        // Mirrors the measured `Preserve` 1.10 overshoot (peak 1.749).
        let mut samples = vec![1.75f32, -0.875, 0.0, -1.75];
        clamp_playback_peak(&mut samples);

        let peak = samples.iter().fold(0.0f32, |m, &s| m.max(s.abs()));
        assert!(
            peak <= PLAYBACK_PEAK_CEILING + 1e-6,
            "peak {peak} must not exceed the ceiling"
        );
        // Relative balance within the clip (music vs. click) must be untouched:
        // the second sample was exactly half the first in magnitude before,
        // and must still be after a single uniform scale.
        assert!(
            (samples[0].abs() / samples[1].abs() - 2.0).abs() < 1e-4,
            "a single scalar must preserve inter-sample ratios"
        );
        assert!((samples[0] - samples[3].abs()).abs() < 1e-4);
    }

    #[test]
    fn clamp_playback_peak_leaves_a_below_ceiling_clip_untouched() {
        let mut samples = vec![0.5f32, -0.3, 0.1, -0.5];
        let before = samples.clone();
        clamp_playback_peak(&mut samples);
        assert_eq!(samples, before, "peak 0.5 is under the ceiling; must be a no-op");
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
                    if frames_total == 0 {
                        pos_cb.store(CLIP_PLAYER_IDLE, Ordering::SeqCst);
                        return;
                    }
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
     r=replay  a=ambiguous(toggle set)  n=note  s=skip track  q=save & quit  \
     (plays on past the window)";

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
///
/// Reads bytes rather than a `String`, and decodes them lossily. Crossterm
/// buffers stdin ahead while it is reading key events, so typing into an IME
/// straight after pressing `n` can leave it holding the leading byte of a
/// multi-byte character while the continuation bytes are still queued for
/// this read. `read_line` rejects those with "stream did not contain valid
/// UTF-8", and when that error propagated it took the whole session down --
/// which lost the note *and* the label for the track being worked on, because
/// a track is only written once both sides are accepted. A mangled first
/// character is a far better outcome than that, so nothing here is fatal:
/// whatever was read is kept, raw mode is restored on every path, and the
/// session carries on.
fn read_note_line() -> Result<String> {
    disable_raw_mode().ok();
    print!("\rnote: ");
    io::stdout().flush().ok();

    let mut bytes = Vec::new();
    let read = io::stdin().lock().read_until(b'\n', &mut bytes);

    // Restore raw mode before reporting anything: the UI it belongs to is
    // still running whether or not the read worked.
    let raw = enable_raw_mode().context("failed to re-enable raw mode after note entry");

    if let Err(e) = read {
        eprintln!("\rnote not recorded ({e})\r");
        bytes.clear();
    }
    raw?;

    let text = note_from_bytes(&bytes);
    if text.contains('\u{fffd}') {
        eprintln!("\rnote contained bytes that are not valid UTF-8; kept as \"{text}\"\r");
    }
    Ok(text)
}

/// Decode one typed note. Lossy on purpose -- see [`read_note_line`].
fn note_from_bytes(bytes: &[u8]) -> String {
    String::from_utf8_lossy(bytes)
        .trim_end_matches(['\n', '\r'])
        .to_string()
}

#[cfg(test)]
mod note_from_bytes_tests {
    use super::*;

    #[test]
    fn keeps_multibyte_text_and_strips_the_newline() {
        assert_eq!(note_from_bytes("1\u{5c0f}\u{7bc0}\u{9045}\u{3044}\n".as_bytes()), "1小節遅い");
        assert_eq!(note_from_bytes(b"plain\r\n"), "plain");
        assert_eq!(note_from_bytes(b""), "");
    }

    #[test]
    fn survives_a_character_whose_leading_byte_was_eaten_by_crossterm() {
        // Continuation bytes of a UTF-8 sequence with the lead byte missing:
        // what is left in the queue when crossterm's key reader has already
        // consumed the start of an IME-committed character.
        let mut bytes = "\u{5c0f}\u{7bc0}".as_bytes().to_vec();
        bytes.remove(0);
        bytes.push(b'\n');
        let note = note_from_bytes(&bytes);
        assert!(
            note.ends_with('\u{7bc0}'),
            "the intact characters must survive, got {note:?}"
        );
        assert!(!note.is_empty());
    }
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

/// Existing row for `hash` in `labels.tsv`, or `None` if the file doesn't
/// exist yet, can't be read, or has no row for this track -- any of those
/// just means there is nothing to merge into, not a fatal error.
fn existing_label(labels_path: &Path, hash: &str) -> Option<SectionLabel> {
    if !labels_path.exists() {
        return None;
    }
    funkot_core::labels::load_labels(labels_path)
        .ok()?
        .into_iter()
        .find(|l| l.hash == hash)
}

/// Persist one track's current state to `labels.tsv`, then (only if the
/// intro side was confirmed this session) reflect it onto the analysis
/// cache.
///
/// `intro` / `outro` are `None` when that side hasn't been decided in this
/// session -- e.g. a `note`-only save from `s`/`q` before either side (or
/// only the intro side) was accepted. Each side is written independently:
/// a side present here overwrites the stored row; a side absent here keeps
/// whatever was already on disk for that hash (or stays unset if there was
/// no prior row), so labeling one side now and the other side in a later
/// session doesn't clobber the first. `note` is always taken from this
/// call, since notes are meant to be edited/replaced by the labeler.
///
/// Returns the row as written, so callers can report what actually ended up
/// on disk (which may include a side merged in from an earlier session, not
/// just what was decided just now).
fn save_label_and_cache(
    labels_path: &Path,
    cache_dir: &Path,
    hash: &str,
    path: &Path,
    intro: Option<&label_session::LabelChoice>,
    outro: Option<&label_session::LabelChoice>,
    note: &str,
) -> Result<SectionLabel> {
    let existing = existing_label(labels_path, hash);
    let (intro_best, intro_ok) = match intro {
        Some(choice) => (Some(choice.best), choice.ok.clone()),
        None => existing
            .as_ref()
            .map(|l| (l.intro_best, l.intro_ok.clone()))
            .unwrap_or((None, Vec::new())),
    };
    let (outro_best, outro_ok) = match outro {
        Some(choice) => (Some(choice.best), choice.ok.clone()),
        None => existing
            .as_ref()
            .map(|l| (l.outro_best, l.outro_ok.clone()))
            .unwrap_or((None, Vec::new())),
    };

    let label = SectionLabel {
        hash: hash.to_string(),
        file_name: file_name(path),
        intro_best,
        intro_ok,
        outro_best,
        outro_ok,
        note: note.to_string(),
    };
    upsert_label(labels_path, label.clone()).map_err(|e| anyhow::anyhow!("{e}"))?;

    // Reflect the intro side onto the cache immediately (so live/render
    // playback picks it up, and `--purge-auto-cache` keeps it). Only when
    // this session actually confirmed it: a side merged in from an earlier
    // row was already reflected to the cache when it was first accepted, so
    // re-writing it here would be redundant, and skipping it means a
    // note-only save never touches the cache as a side effect of *reading*
    // an old row. The outro side is deliberately *not* written here:
    // `cache::set_manual_bars`'s `outro` argument sets
    // `TrackAnalysis::outro_bars`, the DJ mix-trigger, which is *not* a
    // fixed offset from the structural boundary this tool collects (see
    // `funkot_core::labels` module docs and the `outro_structure_bars` doc
    // comment in `funkot-core/src/lib.rs`). Synthesizing a lead-in here to
    // convert one into the other would plant a guessed value in the cache
    // that the analyzer itself doesn't stand behind -- the exact
    // outro_bars/outro_structure_bars conflation Stage 0 introduced
    // `outro_structure_bars` to avoid. The cached outro stays whatever
    // `analysis::analyze` computed; only the label file records the
    // structural ground truth, for Stage 2+ to use.
    if let Some(intro) = intro {
        cache::set_manual_bars(cache_dir, hash, Some(intro.best), None)
            .map_err(|e| anyhow::anyhow!("{e}"))?;
    }
    Ok(label)
}

/// Save whatever note the user typed for the track being abandoned by `s`
/// (skip) or `q` (quit), so it survives even though the labeling itself
/// isn't finished. A no-op when no note was typed, so `s`'s "write nothing"
/// behavior is unchanged for the common case. The intro side is included if
/// this session confirmed it (`y` on the intro candidate before `s`/`q`);
/// the outro side is left for `save_label_and_cache` to merge in from any
/// prior row, since this session never confirmed it -- reaching `Skip`/
/// `Quit` from the outro side means outro was *not* accepted.
fn save_pending_note(
    labels_path: &Path,
    cache_dir: &Path,
    hash: &str,
    path: &Path,
    session: &TrackSession,
) -> Result<()> {
    if session.note().is_empty() {
        return Ok(());
    }
    let label = save_label_and_cache(
        labels_path,
        cache_dir,
        hash,
        path,
        session.intro_choice(),
        None,
        session.note(),
    )?;
    let fmt =
        |v: Option<u32>| v.map(|v| v.to_string()).unwrap_or_else(|| "unlabeled".to_string());
    println!(
        "\r  note saved for {} (intro={}, outro={})\r",
        file_name(path),
        fmt(label.intro_best),
        fmt(label.outro_best)
    );
    Ok(())
}

/// Identifies one candidate clip already prepared for playback: which side,
/// which candidate bar count on that side, and the listening window's
/// half-width in bars (`±8`/`±16`, toggled by `+`). Doesn't carry the
/// [`label_session::ClickGrid`] used to build it -- that's memoized per
/// track/side by `SideGrids` and is therefore identical for every entry that
/// shares a `Side`.
type ClipKey = (Side, u32, u32);

/// How many already-built clips [`ClipCache`] keeps at once.
///
/// A cached clip is what `prepare_clip_for_playback` returned, so it is the
/// *played* length at the device rate: at `±8` bars the clip spans
/// `2 * 8 + CONTINUE_AFTER_WINDOW_BARS` = 80 bars ~= 107 s of 180 BPM
/// source, which `--rate` 1.10 shortens to ~97 s, and at 48 kHz stereo
/// `f32` that is ~37 MB. `+` widens the window to `±16` bars, i.e. 96 bars
/// ~= 116 s played and ~45 MB. So 4 entries costs ~150 MB normally and
/// ~180 MB with the wide window held throughout.
///
/// Only 3 are normally alive at a time (the current candidate plus its two
/// neighbours); the 4th is headroom so stepping back after having stepped
/// forward still hits the cache.
const CLIP_CACHE_CAPACITY: usize = 4;

/// Insertion-order cache of already-prepared candidate clips for the
/// *current* track only. A fresh one is built per track in
/// [`run_label_sections_interactive`], since `buf`/`analysis` change with
/// the track and this cache must not be reused across them. Evicts the
/// oldest *insertion* once full, not the least-recently-used entry: the goal
/// is a small bounded window of candidates around the cursor, not a general
/// LRU.
struct ClipCache {
    entries: Vec<(ClipKey, Vec<f32>)>,
}

impl ClipCache {
    fn new() -> Self {
        Self { entries: Vec::new() }
    }

    /// A clone of the cached clip for `key`, or `None` if it isn't cached
    /// yet. Clones rather than borrowing: the caller hands the clip straight
    /// to `ClipPlayer::play`, which takes ownership, and cloning ~41 MB is
    /// cheap next to the stretch it would otherwise replace.
    fn get(&self, key: &ClipKey) -> Option<Vec<f32>> {
        self.entries.iter().find(|(k, _)| k == key).map(|(_, clip)| clip.clone())
    }

    fn contains(&self, key: &ClipKey) -> bool {
        self.entries.iter().any(|(k, _)| k == key)
    }

    /// No-op if `key` is already cached: the prefetch worker and the
    /// foreground path can race to build the same candidate, and whichever
    /// inserts first wins -- the other's (identical, see
    /// [`build_and_prepare_clip`]) result is simply discarded.
    fn insert(&mut self, key: ClipKey, clip: Vec<f32>) {
        if self.contains(&key) {
            return;
        }
        if self.entries.len() >= CLIP_CACHE_CAPACITY {
            self.entries.remove(0);
        }
        self.entries.push((key, clip));
    }
}

#[cfg(test)]
mod clip_cache_tests {
    use super::*;

    fn clip(tag: f32) -> Vec<f32> {
        vec![tag]
    }

    #[test]
    fn insert_then_get_returns_a_clone_of_the_same_clip() {
        let mut cache = ClipCache::new();
        let key: ClipKey = (Side::Intro, 32, 8);
        cache.insert(key, clip(1.0));
        assert_eq!(cache.get(&key), Some(clip(1.0)));
        assert!(cache.contains(&key));
    }

    #[test]
    fn get_on_a_missing_key_is_none() {
        let cache = ClipCache::new();
        assert_eq!(cache.get(&(Side::Intro, 32, 8)), None);
    }

    #[test]
    fn insert_on_an_already_cached_key_keeps_the_original() {
        let mut cache = ClipCache::new();
        let key: ClipKey = (Side::Outro, 16, 8);
        cache.insert(key, clip(1.0));
        cache.insert(key, clip(2.0));
        assert_eq!(cache.get(&key), Some(clip(1.0)));
    }

    #[test]
    fn capacity_four_evicts_the_oldest_insertion() {
        let mut cache = ClipCache::new();
        let keys: [ClipKey; 5] = [
            (Side::Intro, 8, 8),
            (Side::Intro, 16, 8),
            (Side::Intro, 32, 8),
            (Side::Intro, 48, 8),
            (Side::Intro, 64, 8),
        ];
        for (i, &key) in keys.iter().enumerate() {
            cache.insert(key, clip(i as f32));
        }
        assert_eq!(
            cache.get(&keys[0]),
            None,
            "the first insertion must have been evicted once the 5th arrived"
        );
        for &key in &keys[1..] {
            assert!(cache.contains(&key), "{key:?} should still be cached");
        }
    }
}

/// One candidate clip to build in the background: `key` names it, `grid` is
/// the already-measured [`label_session::ClickGrid`] for its side (`Copy`,
/// cheap to send across the channel).
struct PrefetchRequest {
    key: ClipKey,
    grid: label_session::ClickGrid,
}

/// Builds and playback-prepares one candidate clip: the same call sequence
/// on both the synchronous (foreground) and prefetch (background) paths, so
/// a cache hit is guaranteed to be the bytes a fresh build would have
/// produced.
#[allow(clippy::too_many_arguments)]
fn build_and_prepare_clip(
    buf: &funkot_core::decode::AudioBuffer,
    analysis: &funkot_core::TrackAnalysis,
    key: ClipKey,
    grid: label_session::ClickGrid,
    click_opts: &label_session::ClickOptions,
    device_rate: u32,
    rate: f64,
    pitch_mode: PitchMode,
) -> Vec<f32> {
    let (side, bars, half_width) = key;
    let clip = label_session::build_candidate_clip_on_grid(
        buf, analysis, side, bars, half_width, click_opts, grid,
    );
    prepare_clip_for_playback(clip, buf.sample_rate, device_rate, rate, pitch_mode)
}

/// The clip for `session`'s current side/candidate/width: a cache hit if the
/// prefetch worker (or an earlier visit) already built it, otherwise built
/// synchronously here -- same as before prefetching existed -- and cached
/// for next time.
#[allow(clippy::too_many_arguments)]
fn build_or_cached_clip(
    session: &TrackSession,
    buf: &funkot_core::decode::AudioBuffer,
    analysis: &funkot_core::TrackAnalysis,
    side_grids: &mut label_session::SideGrids,
    cache: &Mutex<ClipCache>,
    click_opts: &label_session::ClickOptions,
    device_rate: u32,
    rate: f64,
    pitch_mode: PitchMode,
) -> Vec<f32> {
    let side = session.current_side();
    let key: ClipKey = (side, session.current_bars(), session.context_half_width_bars());
    if let Some(clip) = cache.lock().unwrap().get(&key) {
        return clip;
    }
    let grid = side_grids.get(buf, analysis, side);
    let clip =
        build_and_prepare_clip(buf, analysis, key, grid, click_opts, device_rate, rate, pitch_mode);
    cache.lock().unwrap().insert(key, clip.clone());
    clip
}

/// Kicks off background builds, via `tx`, for the candidates a single
/// `Left`/`Right` press from `session`'s current position would land on
/// ([`TrackSession::neighbour_bars`]) -- so that by the time the user
/// actually presses it, [`build_or_cached_clip`] hits the cache instead of
/// re-stretching. Best-effort: a `send` failure (the worker already exited)
/// is silently ignored, same as the plan calls for.
fn send_prefetch_requests(
    session: &TrackSession,
    buf: &funkot_core::decode::AudioBuffer,
    analysis: &funkot_core::TrackAnalysis,
    side_grids: &mut label_session::SideGrids,
    tx: &mpsc::Sender<PrefetchRequest>,
) {
    let side = session.current_side();
    let half_width = session.context_half_width_bars();
    let grid = side_grids.get(buf, analysis, side);
    for bars in session.neighbour_bars() {
        let _ = tx.send(PrefetchRequest { key: (side, bars, half_width), grid });
    }
}

fn run_label_sections_interactive(
    playlist: &[PathBuf],
    hashes: &[String],
    labeled: &std::collections::HashSet<String>,
    labels_path: &Path,
    cache_dir: &Path,
    click_opts: &label_session::ClickOptions,
    rate: f64,
    pitch_mode: PitchMode,
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
    // Playback runs at production speed (see `prepare_clip_for_playback`), so
    // say so up front — otherwise a labeler who doesn't know that can mistake
    // a sped-up track for one whose BPM was mis-detected.
    let pitch_note = match pitch_mode {
        PitchMode::Preserve => "pitch preserved",
        PitchMode::Shift => "pitch shifted",
    };
    println!("\rplayback {rate:.2}x ({pitch_note})\r");

    let total = playlist.len();
    'tracks: for (i, path) in playlist.iter().enumerate() {
        let hash = &hashes[i];
        if labeled.contains(hash) {
            continue;
        }
        let progress = format!("[{}/{total}]", i + 1);

        let buf = Arc::new(decode_file(path).map_err(|e| anyhow::anyhow!("{e}"))?);
        let analysis = Arc::new(
            cache::get_or_analyze(path, cache_dir, &buf).map_err(|e| anyhow::anyhow!("{e}"))?,
        );
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
        let mut side_grids = label_session::SideGrids::default();

        // Speculative prefetch: while the user listens to the current
        // candidate (tens of seconds), a single background worker builds the
        // clip(s) a `Left`/`Right` press would land on next, so consecutive
        // arrow presses hit `cache` instead of paying for
        // `prepare_clip_for_playback`'s stretch again. Both `cache` and the
        // worker are scoped to this track: they close over this track's
        // `buf`/`analysis` (via `Arc` clones) and must not survive into the
        // next one. `prefetch_tx` is a plain `let` inside this loop body, so
        // it -- and with it the worker's only sender -- is dropped whenever
        // this iteration ends, however it ends (`continue 'tracks`,
        // `break 'tracks`, or falling off the end); the worker is not
        // joined, it simply finishes once `recv` starts returning `Err`.
        let cache: Arc<Mutex<ClipCache>> = Arc::new(Mutex::new(ClipCache::new()));
        let (prefetch_tx, prefetch_rx) = mpsc::channel::<PrefetchRequest>();
        {
            let worker_buf = Arc::clone(&buf);
            let worker_analysis = Arc::clone(&analysis);
            let worker_cache = Arc::clone(&cache);
            let worker_click_opts = *click_opts;
            thread::spawn(move || {
                for req in prefetch_rx {
                    // Lock only to check, never held across the stretch
                    // below.
                    if worker_cache.lock().unwrap().contains(&req.key) {
                        continue;
                    }
                    let clip = build_and_prepare_clip(
                        &worker_buf,
                        &worker_analysis,
                        req.key,
                        req.grid,
                        &worker_click_opts,
                        device_rate,
                        rate,
                        pitch_mode,
                    );
                    // Lock again only to insert, again never held across the
                    // stretch that built `clip`.
                    worker_cache.lock().unwrap().insert(req.key, clip);
                }
            });
        }

        print_label_track_header(&progress, path);
        player.play(build_or_cached_clip(
            &session,
            &buf,
            &analysis,
            &mut side_grids,
            &cache,
            click_opts,
            device_rate,
            rate,
            pitch_mode,
        ));
        send_prefetch_requests(&session, &buf, &analysis, &mut side_grids, &prefetch_tx);
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
                    player.play(build_or_cached_clip(
                        &session,
                        &buf,
                        &analysis,
                        &mut side_grids,
                        &cache,
                        click_opts,
                        device_rate,
                        rate,
                        pitch_mode,
                    ));
                    send_prefetch_requests(&session, &buf, &analysis, &mut side_grids, &prefetch_tx);
                    print_label_status(&session);
                }
                LabelOutcome::Skip => {
                    player.stop();
                    save_pending_note(labels_path, cache_dir, hash, path, &session)?;
                    println!("\r  skipped {}\r", file_name(path));
                    continue 'tracks;
                }
                LabelOutcome::Quit => {
                    player.stop();
                    save_pending_note(labels_path, cache_dir, hash, path, &session)?;
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
                        Some(&intro),
                        Some(&outro),
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

// ---------------------------------------------------------------------
// `--survey`: one-window-per-track outro-grid survey. Reuses the raw-mode
// key loop / `ClipPlayer` / `prepare_clip_for_playback` machinery above,
// but is otherwise independent of `--label-sections`: a different (much
// simpler) key vocabulary, a different verdict TSV that is never
// `funkot_core::labels`, and a fixed one-shot clip per track instead of
// candidate navigation. See `label_session::build_survey_clip` for the
// clip itself.
// ---------------------------------------------------------------------

/// One judged row in a `--survey-out` file: the survey's equivalent of
/// `funkot_core::labels::SectionLabel`, but deliberately a separate, simpler
/// type -- `--survey` never reads or writes `labels.tsv`.
///
/// On-disk format: tab-separated, header `content_hash\tfile_name\tverdict`,
/// one row per judgement. Appended to, never rewritten in place: judging the
/// same track twice leaves two rows, and the later one is what a summarizer
/// reading the file should treat as authoritative. [`load_survey_judged`]
/// only needs *whether* a hash has a row at all, so nothing here needs to
/// resolve that itself.
#[derive(Debug, Clone, PartialEq, Eq)]
struct SurveyRow {
    hash: String,
    file_name: String,
    verdict: String,
}

const SURVEY_HEADER: &str = "content_hash\tfile_name\tverdict";

/// Parse `--survey-out` TSV text (split out from [`load_survey_judged`] for
/// testing without a file). Comment (`#`) and blank lines are ignored
/// wherever they occur, and the first remaining line is treated as the
/// header and skipped, the same conventions as
/// `funkot_core::labels::parse_labels` -- independently implemented here
/// since this is a different, simpler (3-column) format. A malformed data
/// row (wrong column count, or an empty hash) is silently dropped rather
/// than failing the whole load: a partially-written row from a killed
/// process should not make every other verdict in the file unreadable.
fn parse_survey_rows(contents: &str) -> Vec<SurveyRow> {
    let mut rows = Vec::new();
    for raw_line in contents.lines() {
        let trimmed = raw_line.trim();
        if trimmed.is_empty() || trimmed.starts_with('#') {
            continue;
        }
        // Match the header by its text rather than "whatever the first row
        // happens to be": a content hash can never spell `content_hash`, so
        // this cannot eat a verdict, and a file that somehow lost its header
        // still parses in full instead of silently dropping its first track
        // (which would re-ask that track on every later run).
        if trimmed == SURVEY_HEADER {
            continue;
        }
        let fields: Vec<&str> = raw_line.splitn(3, '\t').collect();
        if fields.len() != 3 {
            continue;
        }
        let hash = fields[0].trim().to_string();
        if hash.is_empty() {
            continue;
        }
        rows.push(SurveyRow {
            hash,
            file_name: fields[1].trim().to_string(),
            verdict: fields[2].trim().to_string(),
        });
    }
    rows
}

/// Hashes already judged in `path` (i.e. that have at least one row), so
/// [`run_survey`] can skip them on this run. Empty (not an error) if `path`
/// doesn't exist yet -- a fresh survey has nothing to skip.
fn load_survey_judged(path: &Path) -> Result<std::collections::HashSet<String>> {
    if !path.exists() {
        return Ok(std::collections::HashSet::new());
    }
    let contents = std::fs::read_to_string(path)
        .with_context(|| format!("cannot read survey file '{}'", path.display()))?;
    Ok(parse_survey_rows(&contents).into_iter().map(|r| r.hash).collect())
}

fn sanitize_survey_field(s: &str) -> String {
    s.replace(['\t', '\n', '\r'], " ")
}

/// Append one judged row to `path`, writing the header first if the file is
/// new (and creating the parent directory if needed, same as
/// `funkot_core::labels::save_labels`). Never truncates or rewrites existing
/// rows -- see [`SurveyRow`]'s doc on why appending (not upserting) is the
/// point.
fn append_survey_verdict(path: &Path, hash: &str, file_name: &str, verdict: &str) -> Result<()> {
    if let Some(parent) = path.parent() {
        if !parent.as_os_str().is_empty() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("mkdir {}", parent.display()))?;
        }
    }
    // Size, not existence: an existing but empty file (a `touch`, or a
    // crash between create and first write) still needs the header. Keying
    // off `exists()` alone would leave a headerless file behind.
    let need_header = std::fs::metadata(path).map(|m| m.len() == 0).unwrap_or(true);
    let mut f = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
        .with_context(|| format!("cannot open survey file '{}'", path.display()))?;
    if need_header {
        writeln!(f, "{SURVEY_HEADER}")?;
    }
    writeln!(
        f,
        "{}\t{}\t{}",
        sanitize_survey_field(hash),
        sanitize_survey_field(file_name),
        sanitize_survey_field(verdict)
    )?;
    Ok(())
}

#[cfg(test)]
mod survey_tsv_tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering as AtomicOrdering};

    /// Process-unique scratch file path under the system temp dir, cleaned
    /// up on drop -- same pattern as `funkot_core::labels`'s test `TempFile`.
    struct TempFile(PathBuf);

    impl TempFile {
        fn new(tag: &str) -> Self {
            static COUNTER: AtomicU64 = AtomicU64::new(0);
            let n = COUNTER.fetch_add(1, AtomicOrdering::Relaxed);
            Self(std::env::temp_dir().join(format!(
                "funkot-survey-test-{tag}-{}-{n}.tsv",
                std::process::id()
            )))
        }
    }

    impl Drop for TempFile {
        fn drop(&mut self) {
            let _ = std::fs::remove_file(&self.0);
        }
    }

    #[test]
    fn load_survey_judged_on_a_missing_file_is_empty_not_an_error() {
        let f = TempFile::new("missing");
        assert!(load_survey_judged(&f.0).unwrap().is_empty());
    }

    #[test]
    fn append_then_load_judged_finds_the_hash() {
        let f = TempFile::new("append-load");
        assert!(load_survey_judged(&f.0).unwrap().is_empty());
        append_survey_verdict(&f.0, "aaaa1111", "track-a.flac", "ok").unwrap();
        let judged = load_survey_judged(&f.0).unwrap();
        assert_eq!(judged.len(), 1);
        assert!(judged.contains("aaaa1111"));
    }

    #[test]
    fn append_writes_the_header_only_once() {
        let f = TempFile::new("header-once");
        append_survey_verdict(&f.0, "aaaa1111", "track-a.flac", "ok").unwrap();
        append_survey_verdict(&f.0, "bbbb2222", "track-b.flac", "half").unwrap();
        let contents = std::fs::read_to_string(&f.0).unwrap();
        assert_eq!(contents.matches(SURVEY_HEADER).count(), 1);
        assert_eq!(parse_survey_rows(&contents).len(), 2);
    }

    #[test]
    fn a_second_verdict_for_the_same_hash_is_appended_not_overwritten() {
        // "同じ曲を2回判定したら後の行が勝つ形でよい" -- the file just grows;
        // this asserts that a summarizer taking the last matching row would
        // in fact see the later verdict, and that the row count reflects
        // both judgements rather than one replacing the other on disk.
        let f = TempFile::new("second-verdict");
        append_survey_verdict(&f.0, "aaaa1111", "track-a.flac", "half").unwrap();
        append_survey_verdict(&f.0, "aaaa1111", "track-a.flac", "ok").unwrap();
        let contents = std::fs::read_to_string(&f.0).unwrap();
        let rows = parse_survey_rows(&contents);
        assert_eq!(rows.len(), 2);
        assert_eq!(rows.last().unwrap().verdict, "ok");
        assert!(load_survey_judged(&f.0).unwrap().contains("aaaa1111"));
    }

    #[test]
    fn parse_survey_rows_skips_comments_and_blank_lines() {
        let text = "\
# comment
content_hash\tfile_name\tverdict

aaaa1111\ttrack-a.flac\tok
# another comment
bbbb2222\ttrack-b.flac\thalf
";
        let rows = parse_survey_rows(text);
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].hash, "aaaa1111");
        assert_eq!(rows[0].verdict, "ok");
        assert_eq!(rows[1].hash, "bbbb2222");
        assert_eq!(rows[1].verdict, "half");
    }

    #[test]
    fn append_to_an_existing_but_empty_file_still_writes_the_header() {
        // A `touch`ed (or crash-truncated) file used to satisfy `exists()`
        // and so never got a header, after which `parse_survey_rows` ate its
        // first verdict as the header and re-asked that track every run.
        let f = TempFile::new("empty-existing");
        std::fs::write(&f.0, "").unwrap();
        append_survey_verdict(&f.0, "aaaa1111", "track-a.flac", "half").unwrap();
        let contents = std::fs::read_to_string(&f.0).unwrap();
        assert_eq!(contents.matches(SURVEY_HEADER).count(), 1);
        assert!(load_survey_judged(&f.0).unwrap().contains("aaaa1111"));
    }

    #[test]
    fn parse_survey_rows_keeps_every_row_when_the_header_is_missing() {
        let text = "aaaa1111\ttrack-a.flac\tok\nbbbb2222\ttrack-b.flac\thalf\n";
        let rows = parse_survey_rows(text);
        assert_eq!(rows.len(), 2, "a headerless file must not lose its first row");
        assert_eq!(rows[0].hash, "aaaa1111");
    }

    #[test]
    fn parse_survey_rows_drops_a_malformed_row_but_keeps_the_rest() {
        let text = "content_hash\tfile_name\tverdict\naaaa1111\tonly-two-fields\nbbbb2222\ttrack-b.flac\thalf\n";
        let rows = parse_survey_rows(text);
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].hash, "bbbb2222");
    }
}

/// One user keystroke for `--survey`, already decoded from the terminal.
/// Deliberately a separate, smaller vocabulary than [`LabelKey`]: `--survey`
/// is a single fixed window per track judged with one keypress, not
/// `--label-sections`' two-sided candidate navigation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SurveyKey {
    /// `o`: the click grid sits on the beat.
    Ok,
    /// `h`: the click grid sits half a beat off (clicks land on the offbeat).
    Half,
    /// `?`: can't tell (too thin, can't lock onto the beat by ear, etc).
    Unknown,
    /// `r`: replay the same window.
    Replay,
    /// `s`: skip this track without judging it (offered again next run).
    Skip,
    Quit,
}

const SURVEY_KEY_HELP: &str =
    "keys: o=on the beat  h=half a beat off  ?=can't tell  r=replay  s=skip  q=quit";

fn print_survey_key_help() {
    println!("\r{SURVEY_KEY_HELP}\r");
}

/// Decode one crossterm key press into a [`SurveyKey`], or `None` for a key
/// with no meaning here.
fn map_survey_key(key: KeyEvent) -> Option<SurveyKey> {
    if key.kind == KeyEventKind::Release {
        return None;
    }
    if key.modifiers.contains(KeyModifiers::CONTROL)
        && matches!(key.code, KeyCode::Char('c') | KeyCode::Char('C'))
    {
        return Some(SurveyKey::Quit);
    }
    match key.code {
        KeyCode::Char('o') | KeyCode::Char('O') => Some(SurveyKey::Ok),
        KeyCode::Char('h') | KeyCode::Char('H') => Some(SurveyKey::Half),
        KeyCode::Char('?') => Some(SurveyKey::Unknown),
        KeyCode::Char('r') | KeyCode::Char('R') => Some(SurveyKey::Replay),
        KeyCode::Char('s') | KeyCode::Char('S') => Some(SurveyKey::Skip),
        KeyCode::Char('q') | KeyCode::Char('Q') => Some(SurveyKey::Quit),
        _ => None,
    }
}

#[cfg(test)]
mod survey_cli_tests {
    use super::*;
    use clap::CommandFactory;

    #[test]
    fn the_arg_definitions_are_internally_consistent() {
        Args::command().debug_assert();
    }

    #[test]
    fn click_flags_need_one_of_the_two_click_modes() {
        // Sharing `--click-db` between `--label-sections` and `--survey` must
        // not turn it into a flag that plain playback silently ignores.
        assert!(Args::try_parse_from(["funkot-autodj", "--click-db", "12", "a.flac"]).is_err());
        assert!(
            Args::try_parse_from(["funkot-autodj", "--click-duck-db", "6", "a.flac"]).is_err()
        );
        assert!(Args::try_parse_from([
            "funkot-autodj",
            "--survey",
            "--survey-out",
            "s.tsv",
            "--click-db",
            "12",
            "a.flac",
        ])
        .is_ok());
    }

    #[test]
    fn the_two_click_modes_are_mutually_exclusive() {
        assert!(Args::try_parse_from([
            "funkot-autodj",
            "--survey",
            "--survey-out",
            "s.tsv",
            "--label-sections",
            "--labels",
            "l.tsv",
            "a.flac",
        ])
        .is_err());
    }

    #[test]
    fn survey_requires_its_output_path() {
        assert!(Args::try_parse_from(["funkot-autodj", "--survey", "a.flac"]).is_err());
    }
}

#[cfg(test)]
mod map_survey_key_tests {
    use super::*;

    fn press(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }

    #[test]
    fn maps_the_documented_keys() {
        assert_eq!(map_survey_key(press(KeyCode::Char('o'))), Some(SurveyKey::Ok));
        assert_eq!(map_survey_key(press(KeyCode::Char('O'))), Some(SurveyKey::Ok));
        assert_eq!(map_survey_key(press(KeyCode::Char('h'))), Some(SurveyKey::Half));
        assert_eq!(map_survey_key(press(KeyCode::Char('?'))), Some(SurveyKey::Unknown));
        assert_eq!(map_survey_key(press(KeyCode::Char('r'))), Some(SurveyKey::Replay));
        assert_eq!(map_survey_key(press(KeyCode::Char('s'))), Some(SurveyKey::Skip));
        assert_eq!(map_survey_key(press(KeyCode::Char('q'))), Some(SurveyKey::Quit));
    }

    #[test]
    fn ctrl_c_is_quit_and_unrecognised_keys_are_none() {
        assert_eq!(
            map_survey_key(KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL)),
            Some(SurveyKey::Quit)
        );
        assert_eq!(map_survey_key(press(KeyCode::Char('x'))), None);
        assert_eq!(map_survey_key(press(KeyCode::Left)), None);
    }

    #[test]
    fn key_release_events_are_ignored() {
        let key = KeyEvent::new_with_kind(
            KeyCode::Char('o'),
            KeyModifiers::NONE,
            KeyEventKind::Release,
        );
        assert_eq!(map_survey_key(key), None);
    }
}

/// `--survey`'s per-track loop: decode + analyze, build the one fixed
/// listening clip ([`label_session::build_survey_clip`]), and wait for a
/// single judgement keystroke before moving to the next track. Reuses
/// [`ClipPlayer`] / [`prepare_clip_for_playback`] / [`pick_output_config`]
/// verbatim from `--label-sections`'s interactive loop; the only new piece
/// is the smaller [`SurveyKey`] vocabulary and [`append_survey_verdict`]
/// instead of `labels.tsv`.
fn run_survey(
    playlist: &[PathBuf],
    survey_out: &Path,
    cache_dir: &Path,
    click_opts: &label_session::ClickOptions,
    rate: f64,
    pitch_mode: PitchMode,
) -> Result<()> {
    let judged = load_survey_judged(survey_out)?;

    let mut hashes = Vec::with_capacity(playlist.len());
    let mut skipped = 0usize;
    for path in playlist {
        let hash = cache::content_hash(path).map_err(|e| anyhow::anyhow!("{e}"))?;
        if judged.contains(&hash) {
            skipped += 1;
        }
        hashes.push(hash);
    }
    eprintln!(
        "survey: {skipped} of {} already judged (skipped), {} to do",
        playlist.len(),
        playlist.len() - skipped
    );

    let host = cpal::default_host();
    let device = host
        .default_output_device()
        .context("no default audio output device available")?;
    let (config, channels) = pick_output_config(&device, None)?;
    let device_rate = config.sample_rate;
    let mut player = ClipPlayer::new(device, config, channels)?;

    enable_raw_mode().context("failed to enable raw mode (needed for --survey)")?;
    let _raw_guard = RawModeGuard;

    print_survey_key_help();
    // Same reasoning as `--label-sections`: playback runs at production
    // speed (see `prepare_clip_for_playback`), so say so up front.
    let pitch_note = match pitch_mode {
        PitchMode::Preserve => "pitch preserved",
        PitchMode::Shift => "pitch shifted",
    };
    println!("\rplayback {rate:.2}x ({pitch_note})\r");

    let total = playlist.len();
    'tracks: for (i, path) in playlist.iter().enumerate() {
        let hash = &hashes[i];
        if judged.contains(hash) {
            continue;
        }
        let progress = format!("[{}/{total}]", i + 1);

        let buf = decode_file(path).map_err(|e| anyhow::anyhow!("{e}"))?;
        let analysis =
            cache::get_or_analyze(path, cache_dir, &buf).map_err(|e| anyhow::anyhow!("{e}"))?;
        // Decode + analyze can run for a while with no key polling; check the
        // stream now so a device that died meanwhile is torn down here
        // rather than spinning until the next keystroke.
        print_label_audio(&player.poll());
        if buf.sample_rate != device_rate {
            eprintln!(
                "note: {} is {} Hz, output device is {device_rate} Hz; resampling the clip for playback",
                file_name(path),
                buf.sample_rate
            );
        }

        let raw_clip = label_session::build_survey_clip(&buf, &analysis, click_opts);
        let clip =
            prepare_clip_for_playback(raw_clip, buf.sample_rate, device_rate, rate, pitch_mode);

        println!("\r{progress} {}\r", file_name(path));
        player.play(clip.clone());

        loop {
            let waiting = !event::poll(Duration::from_millis(100)).unwrap_or(false);
            print_label_audio(&player.poll());
            if waiting {
                continue;
            }
            let ev = match event::read() {
                Ok(ev) => ev,
                Err(_) => break 'tracks,
            };
            let Event::Key(key) = ev else { continue };
            let Some(survey_key) = map_survey_key(key) else {
                continue;
            };
            match survey_key {
                SurveyKey::Replay => {
                    player.play(clip.clone());
                }
                SurveyKey::Skip => {
                    player.stop();
                    println!("\r  skipped {}\r", file_name(path));
                    continue 'tracks;
                }
                SurveyKey::Quit => {
                    player.stop();
                    println!("\rquit\r");
                    break 'tracks;
                }
                SurveyKey::Ok | SurveyKey::Half | SurveyKey::Unknown => {
                    player.stop();
                    let verdict = match survey_key {
                        SurveyKey::Ok => "ok",
                        SurveyKey::Half => "half",
                        SurveyKey::Unknown => "?",
                        SurveyKey::Replay | SurveyKey::Skip | SurveyKey::Quit => unreachable!(),
                    };
                    append_survey_verdict(survey_out, hash, &file_name(path), verdict)?;
                    println!("\r  {verdict} {}\r", file_name(path));
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
                // 8-bar structural outro; the mix trigger is that plus the
                // lead-in, clamped by a 24-bar track with an 8-bar intro.
                "outro_structure_bars": 8,
                "outro_bars": 16,
                "first_downbeat_secs": 0.0,
                "first_downbeat_tol_secs": 0.05,
                "intro_bpm": 180.0,
                "bpm_tol": 0.3,
                "outro_start_bars_from_fd": 8,
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
                "outro_structure_bars": 8,
                "outro_bars": 16,
                "first_downbeat_secs": 0.25,
                "first_downbeat_tol_secs": 0.05,
                "intro_bpm": 180.0,
                "bpm_tol": 0.3,
                "outro_start_bars_from_fd": 8,
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
