//! funkot-autodj CLI: live playback via cpal, or offline WAV render.

use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, RecvTimeoutError};
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant};

use anyhow::{bail, Context, Result};
use clap::Parser;
use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use cpal::{SampleFormat, StreamConfig};
use funkot_cli::playlist::{load_playlist_file, validate_paths_exist};
use funkot_cli::wav_write::{WavFormat, WavStreamWriter};
use funkot_core::engine::{Engine, EngineEvent};
use funkot_core::{EngineOptions, PitchMode};
use log::warn;

#[derive(Debug, Parser)]
#[command(
    name = "funkot-autodj",
    about = "Auto-DJ for Funkot dance music",
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

    /// Offline WAV sample format (default: 32-bit float, no clamp)
    #[arg(long, value_enum, default_value_t = WavFormat::F32)]
    wav_format: WavFormat,

    /// Output sample rate in Hz (live: device default or 48000; render: 44100)
    #[arg(long)]
    sample_rate: Option<u32>,

    /// Offline render speed limit as a multiple of realtime (0 = unlimited).
    /// Pacing gives the loader time to prepare the next track; too fast and
    /// transitions fall back to extended outros.
    #[arg(long, default_value_t = 10.0, hide = true)]
    render_speed: f64,
}

fn main() {
    if let Err(err) = run() {
        eprintln!("error: {err:#}");
        std::process::exit(1);
    }
}

fn run() -> Result<()> {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("warn")).init();

    let args = Args::parse();
    let playlist = resolve_playlist(&args)?;
    let options = build_options(&args)?;

    let stop = Arc::new(AtomicBool::new(false));
    let stop_flag = Arc::clone(&stop);
    ctrlc::set_handler(move || {
        stop_flag.store(true, Ordering::SeqCst);
    })
    .context("failed to install Ctrl+C handler")?;

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
            &stop,
        )?;
    } else {
        run_live(options, playlist, args.sample_rate, &stop)?;
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

fn print_event(event: &EngineEvent, playlist_len: usize) {
    match event {
        EngineEvent::TrackStarted { index, path } => {
            println!(
                "> now playing [{}/{}] {}",
                index + 1,
                playlist_len,
                file_name(path)
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

fn run_render(
    options: EngineOptions,
    playlist: Vec<PathBuf>,
    out_path: &std::path::Path,
    wav_format: WavFormat,
    render_speed: f64,
    stop: &AtomicBool,
) -> Result<()> {
    let playlist_len = playlist.len();
    let sample_rate = options.output_sample_rate;
    let mut engine = Engine::new(options, playlist).map_err(|e| anyhow::anyhow!("engine: {e}"))?;

    let mut writer = WavStreamWriter::create(out_path, sample_rate, wav_format)?;

    const CHUNK_FRAMES: usize = 8192;
    let mut buf = vec![0.0f32; CHUNK_FRAMES * 2];
    let mut seen_track_started = false;
    let mut writing = false;
    let mut rendered_frames: u64 = 0;
    let mut consecutive_silent_frames: u64 = 0;
    let mut last_progress_secs = 0u64;
    let wall_start = Instant::now();
    // Skip writing long post-track silence while the loader prepares the next
    // track (render() returns zeros and never blocks). Keep short gaps that
    // occur between kicks in the material itself.
    let silence_skip_frames = u64::from(sample_rate); // ~1s

    loop {
        if stop.load(Ordering::SeqCst) {
            break;
        }

        let n = engine.render(&mut buf);
        for event in engine.poll_events() {
            if matches!(event, EngineEvent::TrackStarted { .. }) {
                seen_track_started = true;
            }
            print_event(&event, playlist_len);
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

        writer.write_interleaved(chunk)?;
        rendered_frames += n as u64;

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
    Ok(())
}

fn run_live(
    mut options: EngineOptions,
    playlist: Vec<PathBuf>,
    explicit_sample_rate: Option<u32>,
    stop: &Arc<AtomicBool>,
) -> Result<()> {
    let playlist_len = playlist.len();

    let host = cpal::default_host();
    let device = host
        .default_output_device()
        .context("no default audio output device available")?;

    let (config, channels) = pick_output_config(&device, explicit_sample_rate)?;
    options.output_sample_rate = config.sample_rate;

    let mut engine = Engine::new(options, playlist).map_err(|e| anyhow::anyhow!("engine: {e}"))?;

    let (event_tx, event_rx) = mpsc::channel::<EngineEvent>();
    let mut stereo_scratch = Vec::<f32>::new();

    let stream = device
        .build_output_stream(
            config,
            move |data: &mut [f32], _| {
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

    let mut finished = false;
    while !stop.load(Ordering::SeqCst) && !finished {
        match event_rx.recv_timeout(Duration::from_millis(200)) {
            Ok(event) => {
                if matches!(event, EngineEvent::Finished) {
                    finished = true;
                }
                print_event(&event, playlist_len);
            }
            Err(RecvTimeoutError::Timeout) => {}
            Err(RecvTimeoutError::Disconnected) => break,
        }
    }

    drop(stream);
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
        .context("failed to query default output config")?;

    let preferred_rate = explicit_sample_rate.unwrap_or_else(|| default_config.sample_rate());

    // Prefer an f32 config; try stereo when the default is not 2-channel.
    if let Some(cfg) = select_f32_config(device, preferred_rate, 2)? {
        return Ok((cfg, cfg.channels));
    }
    if default_config.sample_format() == SampleFormat::F32 {
        let mut cfg = default_config.config();
        cfg.sample_rate = preferred_rate;
        let channels = cfg.channels;
        return Ok((cfg, channels));
    }
    if let Some(cfg) = select_f32_config(device, preferred_rate, default_config.channels())? {
        return Ok((cfg, cfg.channels));
    }

    bail!("no f32 output configuration available on device {device}");
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
            return Ok(Some(supported.config()));
        }
        // Prefer max when the exact rate is unavailable within the declared range.
        let chosen = sample_rate.clamp(range.min_sample_rate(), range.max_sample_rate());
        return Ok(Some(range.with_sample_rate(chosen).config()));
    }
    Ok(None)
}
