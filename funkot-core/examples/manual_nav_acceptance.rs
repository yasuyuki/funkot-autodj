//! Real-audio acceptance probe for one controlled manual-navigation request.
//!
//! The requested position is in the first prepared track's output-frame domain:
//! `--at-seconds 30` means frame `30 * --sample-rate`, not 30 seconds of wall
//! time.  `from_prepared` begins at `first_downbeat_out`, so a requested frame
//! before that marker is rejected instead of silently measuring another start.
//!
//! Example (the cache directory must be explicit and empty):
//! ```sh
//! cargo run -p funkot-core --release --example manual_nav_acceptance -- \
//!   A.flac B.flac --cache-dir /tmp/funkot-nav-cache --output /tmp/nav \
//!   --before-outro-seconds 30 --realtime
//! ```
//!
//! The JSON records render-call timing.  With `--realtime` the loop is paced
//! against monotonic deadlines and `Engine::set_realtime(true)` is used, but it
//! is still a process-level probe: it does not prove an audio-device callback
//! deadline or allocation behaviour.

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use funkot_core::engine::{prepare_tracks_parallel, Engine, EngineEvent, NavAction, PreparedTrack};
use funkot_core::{EngineOptions, PitchMode};
use serde_json::{json, Value};

#[derive(Clone, Copy)]
enum Action { Next, Restart, Prev }

impl Action {
    fn nav(self) -> NavAction {
        match self {
            Self::Next => NavAction::TransitionToNext,
            Self::Restart => NavAction::RestartCurrent,
            Self::Prev => NavAction::TransitionToPrev,
        }
    }
    fn name(self) -> &'static str {
        match self { Self::Next => "next", Self::Restart => "restart", Self::Prev => "prev" }
    }
}

struct Args {
    files: [PathBuf; 2], cache_dir: PathBuf, output: PathBuf,
    at_seconds: Option<f64>, before_outro_seconds: Option<f64>,
    sample_rate: u32, rate: f64, chunk: usize, realtime: bool, action: Action,
}

fn main() {
    if let Err(error) = run() {
        eprintln!("error: {error}");
        std::process::exit(1);
    }
}

fn run() -> Result<(), String> {
    let args = parse_args()?;
    for path in &args.files {
        if !path.is_file() { return Err(format!("input is not a readable file: {}", path.display())); }
    }
    if args.cache_dir.exists() {
        if !args.cache_dir.is_dir() { return Err("--cache-dir exists but is not a directory".into()); }
        if std::fs::read_dir(&args.cache_dir).map_err(|e| e.to_string())?.next().is_some() {
            return Err("--cache-dir must be empty for this acceptance run".into());
        }
    } else {
        std::fs::create_dir_all(&args.cache_dir)
            .map_err(|e| format!("create cache directory: {e}"))?;
    }
    let parent = args.output.parent().filter(|p| !p.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    if !parent.is_dir() { return Err("--output parent directory does not exist".into()); }

    let options = EngineOptions {
        rate: args.rate, pitch_mode: PitchMode::Preserve, fade_bars: 4,
        output_sample_rate: args.sample_rate, cache_dir: args.cache_dir.clone(),
        loop_playlist: false, ..EngineOptions::default()
    };
    // Initial track preparation completes before the measured render calls.
    let prepared = prepare_tracks_parallel(&options, &args.files, 1)
        .map_err(|e| format!("prepare real input: {e}"))?;
    if prepared.len() != 2 { return Err("prepare did not return both input tracks".into()); }
    let marker_json: Vec<Value> = prepared.iter().enumerate().map(|(index, track)| track_marker_json(index, track)).collect();
    let initial_playhead = prepared[0].first_downbeat_out;
    let request_frame = requested_frame(&args, &prepared[0])?;
    if request_frame < initial_playhead {
        return Err(format!("requested frame {request_frame} is before from_prepared's initial playhead {initial_playhead}"));
    }
    if request_frame >= prepared[0].outro_start_out {
        return Err("requested frame reaches the automatic outro trigger; choose an earlier position so an automatic transition cannot be attributed to navigation".into());
    }
    if matches!(args.action, Action::Prev) {
        return Err("--action prev needs a prior deck, which a two-file fresh from_prepared run does not have; use next or restart".into());
    }
    let input_hashes: Vec<String> = args.files.iter()
        .map(|path| funkot_core::cache::content_hash(path).map_err(|e| e.to_string()))
        .collect::<Result<_, _>>()?;
    let local_tempo = funkot_core::analysis::analyze_local_tempo(
        &prepared[0].samples, request_frame, args.sample_rate, options.target_bpm(),
    ).map(|t| json!({"bpm": t.bpm, "in_transition_range": t.in_transition_range}));

    let wav_path = args.output.with_extension("wav");
    let json_path = args.output.with_extension("json");
    let mut wav = create_wav(&wav_path, args.sample_rate)?;
    let mut engine = Engine::from_prepared(options.clone(), prepared)
        .map_err(|e| format!("from_prepared: {e}"))?;
    engine.set_realtime(args.realtime);

    let bar_frames = options.bar_frames().round() as u64;
    let capture_start = request_frame.saturating_sub(4 * bar_frames);
    let capture_end = request_frame.saturating_add(24 * bar_frames);
    let mut buffer = vec![0.0f32; args.chunk * 2];
    // `start_first` begins at this marker, not at output frame zero.
    let mut rendered = initial_playhead;
    let mut request_wall = None;
    let mut transition_frame = None;
    let mut transition_wall = None;
    let mut automatic_before_request = false;
    let mut events = Vec::new();
    let mut durations: Vec<(Duration, usize)> = Vec::new();
    let mut finite = true;
    let mut peak = 0.0f32;
    let mut over_one = 0u64;
    let mut next_deadline = None;

    while rendered < capture_end {
        if request_wall.is_none() && rendered == request_frame {
            request_wall = Some(Instant::now());
            engine.request_nav(args.action.nav());
        }
        let until_request = if request_wall.is_none() { request_frame - rendered } else { u64::MAX };
        let until_capture = if rendered < capture_start { capture_start - rendered } else { u64::MAX };
        let until_end = capture_end - rendered;
        let want = (args.chunk as u64).min(until_end).min(until_request).min(until_capture) as usize;
        let call_started = Instant::now();
        let frames = engine.render(&mut buffer[..want * 2]);
        let elapsed = call_started.elapsed();
        if frames == 0 { break; }

        // WAV I/O and sample inspection deliberately occur after the timed render call.
        let frame_end = rendered + frames as u64;
        if rendered >= capture_start { durations.push((elapsed, frames)); }
        if frame_end > capture_start {
            let first = capture_start.saturating_sub(rendered) as usize;
            let slice = &buffer[first * 2..frames * 2];
            inspect_samples(slice, &mut finite, &mut peak, &mut over_one);
            write_samples(&mut wav, slice)?;
        }
        for event in engine.poll_events() {
            let event_json = event_json(&event, &args.files);
            if matches!(event, EngineEvent::TransitionStarted { .. }) {
                if request_wall.is_none() { automatic_before_request = true; }
                if request_wall.is_some() && transition_frame.is_none() {
                    transition_frame = engine.transition_frames_into().map(|into| frame_end.saturating_sub(into));
                    transition_wall = Some(Instant::now());
                }
            }
            events.push(json!({"frame_observed_after_render": frame_end, "event": event_json}));
        }
        rendered = frame_end;
        if args.realtime && rendered >= capture_start {
            let deadline = next_deadline.get_or_insert_with(Instant::now);
            *deadline += Duration::from_secs_f64(frames as f64 / args.sample_rate as f64);
            if let Some(wait) = deadline.checked_duration_since(Instant::now()) { std::thread::sleep(wait); }
        }
    }
    wav.finalize().map_err(|e| format!("finalize WAV: {e}"))?;
    let outcome = if !finite {
        Some("captured output contains non-finite samples")
    } else if automatic_before_request {
        Some("automatic transition occurred before the requested navigation frame")
    } else if request_wall.is_none() {
        Some("render ended before the requested navigation frame")
    } else if transition_frame.is_none() {
        Some("manual navigation did not produce an observed transition within the captured audio")
    } else if transition_frame.is_some_and(|frame| frame < request_frame) {
        Some("observed transition predates navigation request")
    } else { None };
    let request_wall = request_wall.unwrap_or_else(Instant::now);
    let transition_wall = transition_wall.unwrap_or_else(Instant::now);
    let manual_diagnostic = manual_plan_json(&engine);
    let attribution = if manual_diagnostic.is_null() {
        "observed_after_request_pending_manual_plan_proof"
    } else { "manual_plan_confirmed" };
    let measurement = json!({
        "input": {"track_ids": [0, 1], "cache_content_hashes": input_hashes},
        "local_tempo_at_request": local_tempo,
        "options": {"sample_rate": args.sample_rate, "rate": args.rate, "chunk_frames": args.chunk,
            "realtime": args.realtime, "action": args.action.name(), "fade_bars": options.fade_bars},
        "markers": marker_json,
        "positions": {"initial_playhead_frame": initial_playhead, "request_frame": request_frame,
            "capture_start_frame": capture_start, "capture_end_limit_frame": capture_end,
            "rendered_end_frame": rendered, "transition_frame": transition_frame,
            "request_to_transition_frames": transition_frame.and_then(|frame| frame.checked_sub(request_frame))},
        "request_to_transition_wall_ms": transition_wall.duration_since(request_wall).as_secs_f64() * 1000.0,
        "render_calls": render_stats(&durations, args.sample_rate),
        "audio": {"finite": finite, "peak": peak, "samples_abs_gt_1": over_one},
        "events": events,
        "manual_plan_diagnostic": manual_diagnostic,
        "transition_attribution": attribution,
        "outcome": outcome,
        "limits": "Render-call timing excludes WAV writing and sample inspection. --realtime wall-paces only the captured four-bar pre-request window and later audio, and enables the engine realtime path; it is not proof of device callback deadlines or callback allocation behaviour."
    });
    std::fs::write(&json_path, serde_json::to_vec_pretty(&measurement).unwrap())
        .map_err(|e| format!("write JSON: {e}"))?;
    println!("wrote {} and {}", wav_path.display(), json_path.display());
    if let Some(error) = outcome { return Err(error.into()); }
    Ok(())
}

fn requested_frame(args: &Args, first: &PreparedTrack) -> Result<u64, String> {
    match (args.at_seconds, args.before_outro_seconds) {
        (Some(at), None) => Ok((at * args.sample_rate as f64).round() as u64),
        (None, Some(before)) => Ok(first.outro_start_out.saturating_sub((before * args.sample_rate as f64).round() as u64)),
        _ => Err("provide exactly one of --at-seconds or --before-outro-seconds".into()),
    }
}

fn manual_plan_json(engine: &Engine) -> Value {
    // Keep this API call isolated so an old-revision copy can replace this helper with `Value::Null`.
    match engine.last_manual_plan_diagnostic() {
        Some(d) => json!({"generation": d.generation, "start": d.start, "entry": d.entry,
            "prev_nudge": d.prev_nudge, "next_main": d.next_main, "fade_in_end": d.fade_in_end,
            "fade_out_start": d.fade_out_start, "fade_out_end": d.fade_out_end,
            "simple": d.simple, "reason": d.reason}),
        None => Value::Null,
    }
}

fn track_marker_json(index: usize, track: &PreparedTrack) -> Value {
    json!({"track_id": index, "frames": track.frames,
        "first_downbeat_out": track.first_downbeat_out, "outro_start_out": track.outro_start_out,
        "outro_end_anchored_out": track.outro_end_anchored_out, "intro_bars": track.intro_bars,
        "outro_bars": track.outro_bars})
}

fn event_json(event: &EngineEvent, files: &[PathBuf; 2]) -> Value {
    match event {
        EngineEvent::TrackStarted { index, path } => json!({"kind": "track_started", "index": index, "track_id": track_index(path, files)}),
        EngineEvent::TransitionStarted { from, to } => json!({"kind": "transition_started", "from_track_id": track_index(from, files), "to_track_id": track_index(to, files)}),
        // Loader messages can embed a source path, so keep only the stable track id.
        EngineEvent::TrackFailed { path, .. } => json!({"kind": "track_failed", "track_id": track_index(path, files)}),
        EngineEvent::Finished => json!({"kind": "finished"}),
    }
}

fn track_index(path: &Path, files: &[PathBuf; 2]) -> Option<usize> {
    files.iter().position(|candidate| candidate == path)
}

fn create_wav(path: &Path, sample_rate: u32) -> Result<hound::WavWriter<std::io::BufWriter<std::fs::File>>, String> {
    hound::WavWriter::create(path, hound::WavSpec { channels: 2, sample_rate, bits_per_sample: 32, sample_format: hound::SampleFormat::Float })
        .map_err(|e| format!("create WAV: {e}"))
}

fn write_samples(writer: &mut hound::WavWriter<std::io::BufWriter<std::fs::File>>, samples: &[f32]) -> Result<(), String> {
    for &sample in samples { writer.write_sample(sample).map_err(|e| format!("write WAV: {e}"))?; }
    Ok(())
}

fn inspect_samples(samples: &[f32], finite: &mut bool, peak: &mut f32, over_one: &mut u64) {
    for &sample in samples {
        *finite &= sample.is_finite();
        if sample.is_finite() { *peak = (*peak).max(sample.abs()); }
        if sample.abs() > 1.0 { *over_one += 1; }
    }
}

fn render_stats(durations: &[(Duration, usize)], sample_rate: u32) -> Value {
    let mut micros: Vec<u64> = durations.iter().map(|(d, _)| d.as_micros() as u64).collect();
    micros.sort_unstable();
    let percentile = |numerator: usize| -> u64 {
        if micros.is_empty() { 0 } else { micros[(micros.len() - 1) * numerator / 100] }
    };
    json!({"count": micros.len(), "max_us": micros.last().copied().unwrap_or(0),
        "p50_us": percentile(50), "p99_us": percentile(99),
        "over_actual_call_period_count": durations.iter().filter(|(duration, frames)| *duration > Duration::from_secs_f64(*frames as f64 / sample_rate as f64)).count(),
        "note": "The period comparison uses each call's actual frame count at the selected sample rate."})
}

fn parse_args() -> Result<Args, String> {
    let mut files = Vec::new(); let mut cache_dir = None; let mut output = None;
    let mut at_seconds = None; let mut before_outro_seconds = None;
    let mut sample_rate = 44_100u32; let mut rate = 1.10f64; let mut chunk = 1024usize;
    let mut realtime = false; let mut action = Action::Next;
    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--cache-dir" => cache_dir = Some(PathBuf::from(next_value("--cache-dir", &mut args)?)),
            "--output" => output = Some(PathBuf::from(next_value("--output", &mut args)?)),
            "--at-seconds" => at_seconds = Some(parse_positive("--at-seconds", &next_value("--at-seconds", &mut args)?)?),
            "--before-outro-seconds" => before_outro_seconds = Some(parse_positive("--before-outro-seconds", &next_value("--before-outro-seconds", &mut args)?)?),
            "--sample-rate" => sample_rate = next_value("--sample-rate", &mut args)?.parse().map_err(|_| "invalid --sample-rate")?,
            "--rate" => rate = parse_positive("--rate", &next_value("--rate", &mut args)?)?,
            "--chunk" => chunk = next_value("--chunk", &mut args)?.parse().map_err(|_| "invalid --chunk")?,
            "--realtime" => realtime = true,
            "--action" => action = match next_value("--action", &mut args)?.as_str() { "next" => Action::Next, "restart" => Action::Restart, "prev" => Action::Prev, _ => return Err("--action must be next, restart, or prev".into()) },
            "-h" | "--help" => return Err(usage().into()),
            flag if flag.starts_with('-') => return Err(format!("unknown option: {flag}\n\n{}", usage())),
            file => files.push(PathBuf::from(file)),
        }
    }
    if files.len() != 2 { return Err(format!("need exactly two input files\n\n{}", usage())); }
    if sample_rate == 0 || chunk == 0 { return Err("--sample-rate and --chunk must be > 0".into()); }
    Ok(Args { files: [files.remove(0), files.remove(0)], cache_dir: cache_dir.ok_or("--cache-dir is required")?, output: output.ok_or("--output is required")?, at_seconds, before_outro_seconds, sample_rate, rate, chunk, realtime, action })
}

fn next_value<I: Iterator<Item = String>>(name: &str, args: &mut I) -> Result<String, String> {
    args.next().ok_or_else(|| format!("{name} needs a value"))
}

fn parse_positive(name: &str, value: &str) -> Result<f64, String> {
    let parsed: f64 = value.parse().map_err(|_| format!("invalid {name}: {value}"))?;
    if parsed.is_finite() && parsed >= 0.0 { Ok(parsed) } else { Err(format!("{name} must be finite and >= 0")) }
}

fn usage() -> &'static str {
    "Usage: manual_nav_acceptance A B --cache-dir EMPTY_DIR --output PREFIX (--at-seconds SECONDS | --before-outro-seconds SECONDS) [--sample-rate 44100] [--rate 1.10] [--chunk 1024] [--realtime] [--action next|restart|prev]"
}
