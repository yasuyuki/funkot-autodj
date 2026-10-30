//! Real-audio acceptance probe for one controlled manual-navigation request.
//!
//! The requested position is in the first prepared track's output-frame domain:
//! `--at-frame 1323000` is an exact active-playhead frame in the first prepared
//! track's output-frame domain. `--at-seconds 30` remains a convenience mode
//! and rounds `30 * --sample-rate`. `from_prepared` begins at
//! `first_downbeat_out`, so a requested frame before that marker is rejected
//! instead of silently measuring another start.
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

use funkot_core::engine::{prepare_tracks_parallel, Engine, EngineEvent, NavigationTrace, NavAction, PreparedTrack};
use funkot_core::{EngineOptions, PitchMode};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};

const BUILD_PROVENANCE: &str = concat!("FUNKOT_REPLAY_CORE_REVISION:", env!("FUNKOT_REPLAY_BUILD_REVISION"), ":END");

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
    at_frame: Option<u64>, at_seconds: Option<f64>, before_outro_seconds: Option<f64>,
    sample_rate: u32, rate: f64, chunk: usize, realtime: bool, action: Action,
    reuse_cache: bool, engine_options: Option<PathBuf>, full_until_frame: Option<u64>,
}

/// Output-frame interval captured in the WAV. `end` is exclusive.
#[derive(Clone, Copy, Debug)]
struct CaptureWindow {
    start: u64,
    end: u64,
    full_replay: bool,
}

impl CaptureWindow {
    fn local(request: u64, bar_frames: u64) -> Result<Self, String> {
        let pre_request = bar_frames.checked_mul(4)
            .ok_or("local capture bar geometry overflows the output-frame domain")?;
        let post_request = bar_frames.checked_mul(24)
            .ok_or("local capture bar geometry overflows the output-frame domain")?;
        let start = request.saturating_sub(pre_request);
        let end = request.checked_add(post_request)
            .ok_or("local capture endpoint overflows the output-frame domain")?;
        Ok(Self { start, end, full_replay: false })
    }

    fn full(initial_playhead: u64, request: u64, end: u64) -> Result<Self, String> {
        if request < initial_playhead {
            return Err(format!("requested frame {request} is before from_prepared's initial playhead {initial_playhead}"));
        }
        if end <= request {
            return Err("--full-until-frame must exceed the request frame".into());
        }
        Ok(Self { start: initial_playhead, end, full_replay: true })
    }

    fn render_limit(self, rendered: u64, requested: bool, fade_completed: bool, fade_deadline: u64) -> Option<u64> {
        if rendered < self.end {
            Some(self.end)
        } else if !self.full_replay && requested && !fade_completed && rendered < fade_deadline {
            Some(fade_deadline)
        } else {
            None
        }
    }
}

fn local_fade_deadline(request: u64, bar_frames: u64) -> Result<u64, String> {
    let fade_limit = bar_frames.checked_mul(40)
        .ok_or("manual fade deadline overflows the output-frame domain")?;
    request.checked_add(fade_limit)
        .ok_or_else(|| "manual fade deadline overflows the output-frame domain".to_owned())
}

/// hound writes a 68-byte WAVEFORMATEXTENSIBLE header for this IEEE-f32 WAV.
/// The RIFF size field excludes its first eight bytes, leaving 60 header bytes
/// plus data in the u32 field. A frame therefore consumes eight data bytes.
const MAX_RIFF_F32_STEREO_FRAMES: u64 = (u32::MAX as u64 - 60) / 8;

fn validate_riff_capture(window: CaptureWindow) -> Result<(), String> {
    let frames = window.end.checked_sub(window.start)
        .ok_or("capture endpoint precedes its start")?;
    if frames > MAX_RIFF_F32_STEREO_FRAMES {
        return Err(format!("requested capture is {frames} frames, exceeding the f32 stereo RIFF limit of {MAX_RIFF_F32_STEREO_FRAMES} frames"));
    }
    Ok(())
}

#[derive(Debug, Default)]
struct CaptureProgress {
    rendered_end_frame: u64,
    wav_frame_zero_render_frame: Option<u64>,
    wav_frame_zero_active_playhead_frame: Option<u64>,
    captured_frames: u64,
    request_submitted: bool,
    request_wall: Option<Instant>,
    accepted_generation: Option<u64>,
    accepted_render_frame: Option<u64>,
    planned: bool,
    transition_started: bool,
    transition_frame: Option<u64>,
    transition_active_playhead: Option<u64>,
    transition_wall: Option<Instant>,
    fade_completed: bool,
    automatic_before_request: bool,
    engine_exhausted: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ReplayFailure {
    EngineEndedBeforeEndpoint,
    NonFiniteOutput,
    AutomaticTransitionBeforeRequest,
    RequestNotReached,
    NavigationNotAccepted,
    ManualPlanNotApplied,
    TransitionNotStarted,
    EngineEndedBeforeFadeCompleted,
    FadeNotCompleted,
    TransitionBeforeRequest,
}

impl ReplayFailure {
    fn message(self) -> &'static str {
        match self {
            Self::EngineEndedBeforeEndpoint => "render ended before the requested output endpoint",
            Self::NonFiniteOutput => "captured output contains non-finite samples",
            Self::AutomaticTransitionBeforeRequest => "automatic transition occurred before the requested navigation frame",
            Self::RequestNotReached => "render ended before the requested navigation frame",
            Self::NavigationNotAccepted => "manual navigation was not accepted",
            Self::ManualPlanNotApplied => "accepted navigation did not apply a manual plan",
            Self::TransitionNotStarted => "accepted navigation did not start a transition",
            Self::EngineEndedBeforeFadeCompleted => "engine ended before the manual transition fade completed",
            Self::FadeNotCompleted => "manual transition fade did not complete by the capture endpoint",
            Self::TransitionBeforeRequest => "observed transition predates navigation request",
        }
    }
}

fn replay_failure(progress: &CaptureProgress, window: CaptureWindow, request_frame: u64, finite: bool) -> Option<ReplayFailure> {
    if progress.rendered_end_frame < window.end {
        Some(ReplayFailure::EngineEndedBeforeEndpoint)
    } else if !finite {
        Some(ReplayFailure::NonFiniteOutput)
    } else if progress.automatic_before_request {
        Some(ReplayFailure::AutomaticTransitionBeforeRequest)
    } else if !progress.request_submitted {
        Some(ReplayFailure::RequestNotReached)
    } else if progress.accepted_generation.is_none() {
        Some(ReplayFailure::NavigationNotAccepted)
    } else if !progress.planned {
        Some(ReplayFailure::ManualPlanNotApplied)
    } else if !progress.transition_started || progress.transition_frame.is_none() {
        Some(ReplayFailure::TransitionNotStarted)
    } else if progress.engine_exhausted && !progress.fade_completed {
        Some(ReplayFailure::EngineEndedBeforeFadeCompleted)
    } else if !progress.fade_completed {
        Some(ReplayFailure::FadeNotCompleted)
    } else if progress.transition_active_playhead.is_some_and(|frame| frame < request_frame) {
        Some(ReplayFailure::TransitionBeforeRequest)
    } else {
        None
    }
}

fn render_capture(
    engine: &mut Engine,
    trace_rx: &std::sync::mpsc::Receiver<NavigationTrace>,
    action: Action,
    initial_render_frame: u64,
    request_frame: u64,
    window: CaptureWindow,
    fade_deadline: u64,
    chunk: usize,
    realtime: bool,
    sample_rate: u32,
    mut on_samples: impl FnMut(&[f32]) -> Result<(), String>,
    mut on_render: impl FnMut(Duration, usize),
    mut on_event: impl FnMut(u64, &EngineEvent),
    mut on_trace: impl FnMut(&NavigationTrace),
) -> Result<CaptureProgress, String> {
    let mut buffer = vec![0.0f32; chunk * 2];
    let mut rendered = initial_render_frame;
    let mut progress = CaptureProgress::default();
    let mut next_deadline = None;

    while let Some(render_limit) = window.render_limit(rendered, progress.request_submitted, progress.fade_completed, fade_deadline) {
        if !progress.request_submitted && rendered == request_frame {
            progress.request_submitted = true;
            progress.request_wall = Some(Instant::now());
            engine.request_nav(action.nav());
        }
        let until_request = if progress.request_submitted { u64::MAX } else { request_frame - rendered };
        let until_capture_start = if rendered < window.start { window.start - rendered } else { u64::MAX };
        let wanted = (chunk as u64).min(render_limit - rendered).min(until_request).min(until_capture_start) as usize;
        debug_assert!(wanted > 0, "capture loop must never render an empty buffer");
        let call_started = Instant::now();
        let frames = engine.render(&mut buffer[..wanted * 2]);
        let elapsed = call_started.elapsed();
        if frames == 0 {
            progress.engine_exhausted = true;
            break;
        }

        let frame_end = rendered + frames as u64;
        let capture_first = rendered.max(window.start);
        let capture_end = frame_end.min(window.end);
        if capture_end > capture_first {
            let first = (capture_first - rendered) as usize;
            let last = (capture_end - rendered) as usize;
            progress.wav_frame_zero_active_playhead_frame.get_or_insert(capture_first);
            progress.wav_frame_zero_render_frame.get_or_insert_with(|| {
                engine.render_frame().saturating_sub(frames as u64).saturating_add(first as u64)
            });
            progress.captured_frames += (last - first) as u64;
            on_samples(&buffer[first * 2..last * 2])?;
            on_render(elapsed, last - first);
        }
        for event in engine.poll_events() {
            if matches!(event, EngineEvent::TransitionStarted { .. }) && !progress.request_submitted {
                progress.automatic_before_request = true;
            }
            on_event(frame_end, &event);
        }
        for trace in trace_rx.try_iter() {
            match &trace {
                NavigationTrace::Accepted { generation, action: trace_action, render_frame, .. }
                    if *trace_action == action.nav() => {
                        progress.accepted_generation = Some(*generation);
                        progress.accepted_render_frame = Some(*render_frame);
                    }
                NavigationTrace::PlanApplied { generation, .. }
                    if Some(*generation) == progress.accepted_generation => progress.planned = true,
                NavigationTrace::TransitionStarted { generation, render_frame, previous_playhead, .. }
                    if Some(*generation) == progress.accepted_generation => {
                        progress.transition_started = true;
                        progress.transition_frame = Some(*render_frame);
                        progress.transition_active_playhead = Some(*previous_playhead);
                        progress.transition_wall = Some(Instant::now());
                    }
                NavigationTrace::FadeCompleted { generation, .. }
                    if Some(*generation) == progress.accepted_generation => progress.fade_completed = true,
                _ => {}
            }
            on_trace(&trace);
        }
        rendered = frame_end;
        if realtime && capture_end > capture_first {
            let deadline = next_deadline.get_or_insert_with(Instant::now);
            *deadline += Duration::from_secs_f64((capture_end - capture_first) as f64 / sample_rate as f64);
            if let Some(wait) = deadline.checked_duration_since(Instant::now()) { std::thread::sleep(wait); }
        }
    }
    progress.rendered_end_frame = rendered;
    Ok(progress)
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
        if !args.reuse_cache && std::fs::read_dir(&args.cache_dir).map_err(|e| e.to_string())?.next().is_some() {
            return Err("--cache-dir must be empty for this acceptance run".into());
        }
    } else {
        std::fs::create_dir_all(&args.cache_dir)
            .map_err(|e| format!("create cache directory: {e}"))?;
    }
    let parent = args.output.parent().filter(|p| !p.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    if !parent.is_dir() { return Err("--output parent directory does not exist".into()); }

    let mut options = EngineOptions {
        rate: args.rate, pitch_mode: PitchMode::Preserve, fade_bars: 4,
        output_sample_rate: args.sample_rate, cache_dir: args.cache_dir.clone(),
        loop_playlist: false, ..EngineOptions::default()
    };
    if let Some(file) = &args.engine_options {
        let header: Value = serde_json::from_slice(&std::fs::read(file).map_err(|e| e.to_string())?)
            .map_err(|e| format!("read captured engine options: {e}"))?;
        options = serde_json::from_value(header.get("options").cloned()
            .ok_or("captured header has no options")?).map_err(|e| format!("captured options: {e}"))?;
        if options.output_sample_rate != args.sample_rate {
            return Err("captured options and --sample-rate differ".into());
        }
        options.cache_dir = args.cache_dir.clone();
    }
    // Initial track preparation completes before the measured render calls.
    let prepared = prepare_tracks_parallel(&options, &args.files, 1)
        .map_err(|e| format!("prepare real input: {e}"))?;
    if prepared.len() != 2 { return Err("prepare did not return both input tracks".into()); }
    let marker_json: Vec<Value> = prepared.iter().enumerate().map(|(index, track)| track_marker_json(index, track)).collect();
    let initial_playhead = prepared[0].first_downbeat_out;
    let request_frame = requested_frame(&args, &prepared[0])?;
    if request_frame >= prepared[0].outro_start_out {
        return Err("requested frame reaches the automatic outro trigger; choose an earlier position so an automatic transition cannot be attributed to navigation".into());
    }
    if matches!(args.action, Action::Prev) {
        return Err("--action prev needs a prior deck, which a two-file fresh from_prepared run does not have; use next or restart".into());
    }
    let input_hashes: Vec<String> = args.files.iter().map(|path| sha256_file(path))
        .collect::<Result<_, _>>()?;
    let local_tempo = funkot_core::analysis::analyze_local_tempo(
        &prepared[0].samples, request_frame, args.sample_rate, options.target_bpm(),
    ).map(|t| json!({"bpm": t.bpm, "in_transition_range": t.in_transition_range}));

    let bar_frames = options.bar_frames().round() as u64;
    let capture = match args.full_until_frame {
        Some(end) => CaptureWindow::full(initial_playhead, request_frame, end)?,
        None => {
            if request_frame < initial_playhead {
                return Err(format!("requested frame {request_frame} is before from_prepared's initial playhead {initial_playhead}"));
            }
            CaptureWindow::local(request_frame, bar_frames)?
        }
    };
    validate_riff_capture(capture)?;
    let wav_path = args.output.with_extension("wav");
    let json_path = args.output.with_extension("json");
    let mut wav = create_wav(&wav_path, args.sample_rate)?;
    let mut engine = Engine::from_prepared(options.clone(), prepared)
        .map_err(|e| format!("from_prepared: {e}"))?;
    engine.set_realtime(args.realtime);
    let trace_rx = engine.enable_navigation_trace(32);

    let mut events = Vec::new();
    let mut navigation_trace = Vec::new();
    let mut durations: Vec<(Duration, usize)> = Vec::new();
    let mut finite = true;
    let mut peak = 0.0f32;
    let mut over_one = 0u64;

    let fade_deadline = if capture.full_replay {
        capture.end
    } else {
        local_fade_deadline(request_frame, bar_frames)?
    };
    let progress = render_capture(
        &mut engine, &trace_rx, args.action, initial_playhead, request_frame, capture, fade_deadline,
        args.chunk, args.realtime, args.sample_rate,
        |samples| {
            inspect_samples(samples, &mut finite, &mut peak, &mut over_one);
            write_samples(&mut wav, samples)
        },
        |elapsed, frames| durations.push((elapsed, frames)),
        |frame_end, event| events.push(json!({"frame_observed_after_render": frame_end, "event": event_json(event, &args.files)})),
        |trace| navigation_trace.push(serde_json::to_value(trace).expect("trace is serializable")),
    )?;
    wav.finalize().map_err(|e| format!("finalize WAV: {e}"))?;
    let outcome = replay_failure(&progress, capture, request_frame, finite);
    let request_wall = progress.request_wall.unwrap_or_else(Instant::now);
    let transition_wall = progress.transition_wall.unwrap_or_else(Instant::now);
    let manual_diagnostic = manual_plan_json(&engine);
    let wav_file = wav_path.display().to_string();
    let attribution = if manual_diagnostic.is_null() {
        "observed_after_request_pending_manual_plan_proof"
    } else { "manual_plan_confirmed" };
    let measurement = json!({
        "schema": 1,
        "runtime": {"core_revision": std::hint::black_box(BUILD_PROVENANCE).strip_prefix("FUNKOT_REPLAY_CORE_REVISION:").and_then(|s| s.strip_suffix(":END")).filter(|s| !s.is_empty())},
        "executable_sha256": sha256_file(&std::env::current_exe().map_err(|e| e.to_string())?)?,
        "core_source_sha256": format!("{:x}", Sha256::digest(include_bytes!("../src/engine.rs"))),
        "driver_source_sha256": format!("{:x}", Sha256::digest(include_bytes!("manual_nav_acceptance.rs"))),
        "input": {"track_ids": [0, 1], "content_hashes": input_hashes},
        "engine_options": options,
        "local_tempo_at_request": local_tempo,
        "options": {"sample_rate": args.sample_rate, "output_sample_rate": args.sample_rate,
            "rate": options.rate, "chunk_frames": args.chunk,
            "realtime": args.realtime, "action": args.action.name(), "fade_bars": options.fade_bars},
        "markers": marker_json,
        "positions": {"frame_domain": "active_playhead_output_frames", "initial_playhead_frame": initial_playhead, "request_frame": request_frame,
            "capture_start_frame": capture.start, "capture_end_limit_frame": capture.end,
            "capture_full_replay": capture.full_replay, "captured_frames": progress.captured_frames,
            "wav_frame_zero_active_playhead_frame": progress.wav_frame_zero_active_playhead_frame,
            "fade_deadline_frame": fade_deadline, "rendered_end_frame": progress.rendered_end_frame,
            "request_render_frame": progress.accepted_render_frame,
            "engine_rendered_output_frames": engine.render_frame(), "transition_render_frame": progress.transition_frame,
            "transition_active_playhead_frame": progress.transition_active_playhead,
            "request_to_transition_frames": progress.transition_active_playhead.and_then(|frame| frame.checked_sub(request_frame))},
        "request_to_transition_wall_ms": transition_wall.duration_since(request_wall).as_secs_f64() * 1000.0,
        "render_calls": render_stats(&durations, args.sample_rate),
        "audio": {"file": wav_file, "sample_rate": args.sample_rate,
            "engine_start_frame": progress.wav_frame_zero_render_frame,
            "active_playhead_start_frame": progress.wav_frame_zero_active_playhead_frame,
            "finite": finite, "peak": peak, "samples_abs_gt_1": over_one},
        "output_wav": {"format": "f32_stereo_wav", "sample_rate": args.sample_rate,
            "channels": 2, "wav_frame_zero_render_frame": progress.wav_frame_zero_render_frame},
        "events": events,
        "navigation_trace": navigation_trace,
        "navigation_trace_dropped": engine.navigation_trace_dropped(),
        "manual_plan_diagnostic": manual_diagnostic,
        "transition_attribution": attribution,
        "outcome": outcome.map(ReplayFailure::message),
        "limits": "Render-call timing excludes WAV writing and sample inspection. --realtime wall-paces only the captured four-bar pre-request window and later audio, and enables the engine realtime path; it is not proof of device callback deadlines or callback allocation behaviour."
    });
    std::fs::write(&json_path, serde_json::to_vec_pretty(&measurement).unwrap())
        .map_err(|e| format!("write JSON: {e}"))?;
    println!("wrote {} and {}", wav_path.display(), json_path.display());
    if let Some(error) = outcome { return Err(error.message().into()); }
    Ok(())
}

fn requested_frame(args: &Args, first: &PreparedTrack) -> Result<u64, String> {
    match (args.at_frame, args.at_seconds, args.before_outro_seconds) {
        (Some(frame), None, None) => Ok(frame),
        (None, Some(at), None) => Ok((at * args.sample_rate as f64).round() as u64),
        (None, None, Some(before)) => Ok(first.outro_start_out.saturating_sub((before * args.sample_rate as f64).round() as u64)),
        _ => Err("provide exactly one of --at-frame, --at-seconds, or --before-outro-seconds".into()),
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
        EngineEvent::TrackStarted { index, path, .. } => json!({"kind": "track_started", "index": index, "track_id": track_index(path, files)}),
        EngineEvent::TransitionStarted { from, to, .. } => json!({"kind": "transition_started", "from_track_id": track_index(from, files), "to_track_id": track_index(to, files)}),
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
    let mut at_frame = None; let mut at_seconds = None; let mut before_outro_seconds = None;
    let mut sample_rate = 44_100u32; let mut rate = 1.10f64; let mut chunk = 1024usize;
    let mut realtime = false; let mut action = Action::Next;
    let mut reuse_cache = false; let mut engine_options = None; let mut full_until_frame = None;
    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--cache-dir" => cache_dir = Some(PathBuf::from(next_value("--cache-dir", &mut args)?)),
            "--reuse-cache" => reuse_cache = true,
            "--engine-options" => engine_options = Some(PathBuf::from(next_value("--engine-options", &mut args)?)),
            "--output" => output = Some(PathBuf::from(next_value("--output", &mut args)?)),
            "--full-until-frame" => full_until_frame = Some(next_value("--full-until-frame", &mut args)?.parse().map_err(|_| "invalid --full-until-frame")?),
            "--at-frame" => at_frame = Some(next_value("--at-frame", &mut args)?.parse().map_err(|_| "invalid --at-frame")?),
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
    Ok(Args { files: [files.remove(0), files.remove(0)], cache_dir: cache_dir.ok_or("--cache-dir is required")?, output: output.ok_or("--output is required")?, at_frame, at_seconds, before_outro_seconds, sample_rate, rate, chunk, realtime, action, reuse_cache, engine_options, full_until_frame })
}

fn next_value<I: Iterator<Item = String>>(name: &str, args: &mut I) -> Result<String, String> {
    args.next().ok_or_else(|| format!("{name} needs a value"))
}

fn parse_positive(name: &str, value: &str) -> Result<f64, String> {
    let parsed: f64 = value.parse().map_err(|_| format!("invalid {name}: {value}"))?;
    if parsed.is_finite() && parsed >= 0.0 { Ok(parsed) } else { Err(format!("{name} must be finite and >= 0")) }
}

fn usage() -> &'static str {
    "Usage: manual_nav_acceptance A B --cache-dir DIR [--reuse-cache] [--engine-options CAPTURE_HEADER.json] --output PREFIX (--at-frame ACTIVE_PLAYHEAD_FRAME | --at-seconds SECONDS | --before-outro-seconds SECONDS) [--full-until-frame ACTIVE_PLAYHEAD_END] [--sample-rate 44100] [--rate 1.10] [--chunk 1024] [--realtime] [--action next|restart|prev]"
}

fn sha256_file(path: &Path) -> Result<String, String> {
    let mut file = std::fs::File::open(path).map_err(|e| e.to_string())?;
    let mut hash = Sha256::new();
    std::io::copy(&mut file, &mut hash).map_err(|e| e.to_string())?;
    Ok(format!("{:x}", hash.finalize()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    fn prepared_tracks(frames: u64) -> Vec<PreparedTrack> {
        (0..2usize).map(|index| {
            let mut samples = Vec::with_capacity(frames as usize * 2);
            for frame in 0..frames {
                let sample = ((frame + index as u64 * 17) % 97) as f32 / 97.0 - 0.5;
                samples.extend([sample, -sample]);
            }
            PreparedTrack {
                path: PathBuf::from(format!("replay-{index}.wav")),
                playlist_index: index,
                samples: Arc::new(samples),
                frames,
                first_downbeat_out: 7,
                outro_start_out: frames.saturating_sub(4_000),
                outro_end_anchored_out: frames.saturating_sub(4_000),
                intro_bars: 8,
                outro_bars: 8,
                gain_linear: 1.0,
                preview: false,
                head_only: false,
            }
        }).collect()
    }

    fn test_options() -> EngineOptions {
        EngineOptions { output_sample_rate: 1_000, rate: 1.0, fade_bars: 1, ..EngineOptions::default() }
    }

    fn render_test(window: CaptureWindow, request_frame: u64, frames: u64) -> (CaptureProgress, Vec<f32>) {
        let options = test_options();
        let fade_deadline = local_fade_deadline(request_frame, options.bar_frames().round() as u64)
            .expect("synthetic fade deadline");
        let mut engine = Engine::from_prepared(options, prepared_tracks(frames)).expect("synthetic engine");
        let trace_rx = engine.enable_navigation_trace(32);
        let mut output = Vec::new();
        let progress = render_capture(
            &mut engine, &trace_rx, Action::Next, 7, request_frame, window, fade_deadline,
            127, false, 1_000,
            |samples| { output.extend_from_slice(samples); Ok(()) },
            |_, _| {},
            |_, _| {},
            |_| {},
        ).expect("synthetic render");
        (progress, output)
    }

    #[test]
    fn full_replay_has_exact_origin_length_and_completed_navigation() {
        let window = CaptureWindow::full(7, 37, 8_000).expect("full window");
        let (progress, output) = render_test(window, 37, 24_000);

        assert_eq!(progress.rendered_end_frame, window.end);
        assert_eq!(progress.wav_frame_zero_active_playhead_frame, Some(window.start));
        assert_eq!(progress.wav_frame_zero_render_frame, Some(0));
        assert_eq!(progress.captured_frames, window.end - window.start);
        assert_eq!(output.len(), ((window.end - window.start) * 2) as usize);
        assert!(progress.request_submitted);
        assert!(progress.accepted_generation.is_some());
        assert!(progress.planned);
        assert!(progress.transition_started);
        assert!(progress.fade_completed);
        assert!(progress.transition_active_playhead.is_some_and(|frame| frame >= 37));
        assert_eq!(replay_failure(&progress, window, 37, true), None);
    }

    #[test]
    fn local_capture_keeps_its_requested_interval() {
        let options = test_options();
        let request = 6_000;
        let window = CaptureWindow::local(request, options.bar_frames().round() as u64).expect("local window");
        let (progress, output) = render_test(window, request, 100_000);

        assert_eq!(progress.rendered_end_frame, window.end);
        assert_eq!(progress.wav_frame_zero_active_playhead_frame, Some(window.start));
        assert_eq!(progress.wav_frame_zero_render_frame, Some(window.start - 7));
        assert_eq!(progress.captured_frames, window.end - window.start);
        assert_eq!(output.len(), ((window.end - window.start) * 2) as usize);
        assert_eq!(replay_failure(&progress, window, request, true), None);
    }

    #[test]
    fn full_replay_reports_an_unfinished_fade_without_rendering_past_endpoint() {
        let window = CaptureWindow::full(7, 37, 2_000).expect("full window");
        let (progress, output) = render_test(window, 37, 24_000);

        assert_eq!(progress.rendered_end_frame, window.end);
        assert_eq!(progress.captured_frames, window.end - window.start);
        assert_eq!(output.len(), ((window.end - window.start) * 2) as usize);
        assert!(progress.planned);
        assert!(progress.transition_started);
        assert!(!progress.fade_completed);
        assert_eq!(replay_failure(&progress, window, 37, true), Some(ReplayFailure::FadeNotCompleted));
    }

    #[test]
    fn engine_exhaustion_before_full_endpoint_fails() {
        let window = CaptureWindow::full(7, 10, 300).expect("full window");
        let (progress, _) = render_test(window, 10, 100);

        assert!(progress.engine_exhausted);
        assert!(progress.rendered_end_frame < window.end);
        assert_eq!(replay_failure(&progress, window, 10, true), Some(ReplayFailure::EngineEndedBeforeEndpoint));
    }

    #[test]
    fn invalid_windows_and_riff_lengths_are_rejected_before_wav_creation() {
        assert!(CaptureWindow::full(7, 6, 8).is_err(), "request cannot predate the initial playhead");
        assert!(CaptureWindow::full(7, 7, 7).is_err(), "full end must follow the request");
        assert!(CaptureWindow::local(u64::MAX, 1).is_err(), "local endpoint overflow must fail");
        assert!(CaptureWindow::local(0, u64::MAX).is_err(), "local bar multiplication must not wrap");
        assert!(local_fade_deadline(0, u64::MAX).is_err(), "fade deadline multiplication must not wrap");

        let largest = CaptureWindow { start: 0, end: MAX_RIFF_F32_STEREO_FRAMES, full_replay: true };
        assert!(validate_riff_capture(largest).is_ok());
        let too_large = CaptureWindow { start: 0, end: MAX_RIFF_F32_STEREO_FRAMES + 1, full_replay: true };
        assert!(validate_riff_capture(too_large).is_err());
    }
}
