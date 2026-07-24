//! funkot-autodj CLI: live playback via cpal, or offline WAV render.

use std::io::{self, BufRead};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, RecvTimeoutError};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use anyhow::{bail, Context, Result};
use clap::Parser;
use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use cpal::{BufferSize, SampleFormat, StreamConfig, SupportedBufferSize};
use funkot_cli::playlist::{load_playlist_file, validate_paths_exist};
use funkot_cli::wav_write::{WavFormat, WavStreamWriter};
use funkot_core::engine::{prepare_tracks_parallel, Engine, EngineEvent};
use funkot_core::{EngineOptions, PitchMode};
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
    #[arg(long, default_value_t = 60.0)]
    transition_clip_seconds: f64,

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

    let playlist = resolve_playlist(&args)?;
    let options = build_options(&args)?;

    let stop = Arc::new(AtomicBool::new(false));
    let stop_flag = Arc::clone(&stop);
    ctrlc::set_handler(move || {
        stop_flag.store(true, Ordering::SeqCst);
    })
    .context("failed to install Ctrl+C handler")?;

    if args.render.is_some() && args.dump_wav.is_some() {
        bail!("cannot combine --render and --dump-wav (use one)");
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

fn print_event(event: &EngineEvent, playlist_len: usize, play_elapsed: &mut PlayElapsed) {
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

fn run_render(
    options: EngineOptions,
    playlist: Vec<PathBuf>,
    out_path: &std::path::Path,
    wav_format: WavFormat,
    render_speed: f64,
    transition_clip_seconds: f64,
    jobs: usize,
    stop: &AtomicBool,
) -> Result<()> {
    let playlist_len = playlist.len();
    let sample_rate = options.output_sample_rate;
    // Start 8 bars before TransitionStarted so the lead-in is audible.
    const TRANSITION_CLIP_PREROLL_BARS: u32 = 8;
    let transition_enabled = transition_clip_seconds.is_finite()
        && transition_clip_seconds > 0.0
        && playlist_len >= 2;
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
            print_event(&event, playlist_len, &mut play_elapsed);
        }

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

        if transition_enabled && !transitions_started.is_empty() && transition_frames_into_end <= n_frames_u64 {
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
                        w.write_interleaved(&lookback[off..end])?;
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

        writer.write_interleaved(chunk)?;
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
                cap.writer.write_interleaved(&chunk[src_start..src_end])?;
            }
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
    let duration_secs = rendered_frames as f64 / f64::from(sample_rate);
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
        println!("wrote {} transition clips to {}", n_clips, transitions_dir.display());
    }
    Ok(())
}

fn run_live(
    mut options: EngineOptions,
    playlist: Vec<PathBuf>,
    explicit_sample_rate: Option<u32>,
    dump_wav: Option<&Path>,
    wav_format: WavFormat,
    stop: &Arc<AtomicBool>,
) -> Result<()> {
    let playlist_len = playlist.len();

    let host = cpal::default_host();
    let device = host
        .default_output_device()
        .context("no default audio output device available")?;

    let (config, channels) = pick_output_config(&device, explicit_sample_rate)?;
    options.output_sample_rate = config.sample_rate;
    let sample_rate = config.sample_rate;

    let mut engine = Engine::new(options, playlist).map_err(|e| anyhow::anyhow!("engine: {e}"))?;
    // Audio callback must never sleep (preview→Upgrade wait would underrun under load).
    engine.set_realtime(true);

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

                for event in engine.poll_events() {
                    let _ = event_tx.send(event);
                }
            },
            |err| {
                eprintln!("audio stream error: {err}");
            },
            None,
        )
        .context("failed to build audio output stream")?;

    stream.play().context("failed to start audio stream")?;
    let play_elapsed = Arc::new(Mutex::new(PlayElapsed::default()));
    eprintln!("press Enter to pause/resume, Ctrl+C to stop");

    // Toggle pause from stdin (no TUI dep). EOF or read error ends the watcher.
    let paused_stdin = Arc::clone(&paused);
    let play_elapsed_stdin = Arc::clone(&play_elapsed);
    thread::spawn(move || {
        let stdin = io::stdin();
        let mut line = String::new();
        loop {
            line.clear();
            match stdin.lock().read_line(&mut line) {
                Ok(0) => break,
                Ok(_) => {
                    let now_paused = !paused_stdin.fetch_xor(true, Ordering::SeqCst);
                    if let Ok(mut clock) = play_elapsed_stdin.lock() {
                        clock.set_paused(now_paused);
                    }
                    if now_paused {
                        println!("paused");
                    } else {
                        println!("resumed");
                    }
                }
                Err(_) => break,
            }
        }
    });

    let mut finished = false;
    while !stop.load(Ordering::SeqCst) && !finished {
        match event_rx.recv_timeout(Duration::from_millis(200)) {
            Ok(event) => {
                if matches!(event, EngineEvent::Finished) {
                    finished = true;
                }
                let mut clock = play_elapsed.lock().unwrap_or_else(|e| e.into_inner());
                print_event(&event, playlist_len, &mut clock);
            }
            Err(RecvTimeoutError::Timeout) => {}
            Err(RecvTimeoutError::Disconnected) => break,
        }
    }

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
        SupportedBufferSize::Range { min, .. } => {
            BufferSize::Fixed(LIVE_BUFFER_FRAMES.max(min))
        }
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
