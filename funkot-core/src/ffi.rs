//! C ABI for embedding in mobile/desktop hosts.
//!
//! Header: `include/funkot.h` (hand-maintained; keep in sync with this module).

use std::collections::VecDeque;
use std::ffi::{c_char, CStr};
use std::panic::{catch_unwind, AssertUnwindSafe};
use std::path::PathBuf;
use std::ptr;

use crate::engine::{Engine, EngineEvent};
use crate::{EngineOptions, PitchMode};

/// Opaque engine handle for C hosts.
pub struct FunkotEngine {
    engine: Engine,
    events: VecDeque<EngineEvent>,
}

/// Options mirror of `FunkotOptions` in `funkot.h`.
#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct FunkotOptions {
    pub rate: f64,
    pub pitch_shift: i32,
    pub fade_bars: u32,
    pub highpass_hz: f32,
    pub gain_normalize: i32,
    pub random: i32,
    pub loop_playlist: i32,
    pub output_sample_rate: u32,
    pub cache_dir: *const c_char,
}

/// Event type mirror of `FunkotEventType` in `funkot.h`.
#[repr(i32)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FunkotEventType {
    None = 0,
    TrackStarted = 1,
    TransitionStarted = 2,
    TrackFailed = 3,
    Finished = 4,
}

/// Event mirror of `FunkotEvent` in `funkot.h`.
#[repr(C)]
#[derive(Clone, Copy)]
pub struct FunkotEvent {
    pub type_: FunkotEventType,
    pub track_index: i32,
    pub path: [c_char; 512],
    pub detail: [c_char; 256],
}

/// Copy the largest valid UTF-8 prefix of `src` that fits in `dst` (leaving room
/// for a trailing NUL). Always NUL-terminates when `dst` is non-empty.
fn copy_utf8_prefix(dst: &mut [u8], src: &str) {
    if dst.is_empty() {
        return;
    }
    let max = dst.len() - 1;
    let bytes = src.as_bytes();
    let mut end = bytes.len().min(max);
    while end > 0 && !src.is_char_boundary(end) {
        end -= 1;
    }
    dst[..end].copy_from_slice(&bytes[..end]);
    dst[end] = 0;
}

fn copy_utf8_c_array(dst: &mut [c_char], src: &str) {
    // SAFETY: c_char arrays are treated as raw byte buffers for UTF-8 text.
    let bytes = unsafe { std::slice::from_raw_parts_mut(dst.as_mut_ptr().cast::<u8>(), dst.len()) };
    copy_utf8_prefix(bytes, src);
}

fn write_err(err: *mut c_char, err_len: usize, msg: &str) {
    if err.is_null() || err_len == 0 {
        return;
    }
    // SAFETY: caller provided err pointing to err_len writable bytes.
    let dst = unsafe { std::slice::from_raw_parts_mut(err.cast::<u8>(), err_len) };
    copy_utf8_prefix(dst, msg);
}

fn path_to_utf8(path: &std::path::Path) -> String {
    path.to_string_lossy().into_owned()
}

fn fill_event(out: &mut FunkotEvent, event: EngineEvent) {
    match event {
        EngineEvent::TrackStarted { index, path } => {
            out.type_ = FunkotEventType::TrackStarted;
            out.track_index = i32::try_from(index).unwrap_or(i32::MAX);
            copy_utf8_c_array(&mut out.path, &path_to_utf8(&path));
            out.detail[0] = 0;
        }
        EngineEvent::TransitionStarted { from, to } => {
            out.type_ = FunkotEventType::TransitionStarted;
            out.track_index = -1;
            copy_utf8_c_array(&mut out.path, &path_to_utf8(&to));
            copy_utf8_c_array(&mut out.detail, &path_to_utf8(&from));
        }
        EngineEvent::TrackFailed { path, message } => {
            out.type_ = FunkotEventType::TrackFailed;
            out.track_index = -1;
            copy_utf8_c_array(&mut out.path, &path_to_utf8(&path));
            copy_utf8_c_array(&mut out.detail, &message);
        }
        EngineEvent::Finished => {
            out.type_ = FunkotEventType::Finished;
            out.track_index = -1;
            out.path[0] = 0;
            out.detail[0] = 0;
        }
    }
}

fn drain_events(handle: &mut FunkotEngine) {
    for ev in handle.engine.poll_events() {
        handle.events.push_back(ev);
    }
}

fn options_from_c(options: &FunkotOptions) -> std::result::Result<EngineOptions, String> {
    let cache_dir = if options.cache_dir.is_null() {
        PathBuf::from("funkot-cache")
    } else {
        // SAFETY: cache_dir is a non-null C string from the host.
        let s = unsafe { CStr::from_ptr(options.cache_dir) }
            .to_str()
            .map_err(|_| "cache_dir is not valid UTF-8".to_string())?;
        PathBuf::from(s)
    };

    Ok(EngineOptions {
        rate: options.rate,
        pitch_mode: if options.pitch_shift != 0 {
            PitchMode::Shift
        } else {
            PitchMode::Preserve
        },
        fade_bars: options.fade_bars,
        highpass_hz: options.highpass_hz,
        gain_normalize: options.gain_normalize != 0,
        random: options.random != 0,
        loop_playlist: options.loop_playlist != 0,
        output_sample_rate: options.output_sample_rate,
        cache_dir,
    })
}

fn collect_paths(
    paths: *const *const c_char,
    n_paths: usize,
) -> std::result::Result<Vec<PathBuf>, String> {
    if n_paths == 0 {
        return Ok(Vec::new());
    }
    if paths.is_null() {
        return Err("paths pointer is NULL".into());
    }
    let mut out = Vec::with_capacity(n_paths);
    for i in 0..n_paths {
        // SAFETY: paths points to n_paths C string pointers.
        let p = unsafe { *paths.add(i) };
        if p.is_null() {
            return Err(format!("paths[{i}] is NULL"));
        }
        // SAFETY: p is a non-null C string.
        let s = unsafe { CStr::from_ptr(p) }
            .to_str()
            .map_err(|_| format!("paths[{i}] is not valid UTF-8"))?;
        out.push(PathBuf::from(s));
    }
    Ok(out)
}

/// Fill `options` with defaults matching [`EngineOptions::default`].
///
/// # Safety
/// `options` must be null or point to a valid `FunkotOptions`.
#[no_mangle]
pub unsafe extern "C" fn funkot_options_default(options: *mut FunkotOptions) {
    if options.is_null() {
        return;
    }
    let _ = catch_unwind(AssertUnwindSafe(|| {
        let defaults = EngineOptions::default();
        // SAFETY: options is non-null and points to a FunkotOptions.
        unsafe {
            *options = FunkotOptions {
                rate: defaults.rate,
                pitch_shift: 0,
                fade_bars: defaults.fade_bars,
                highpass_hz: defaults.highpass_hz,
                gain_normalize: i32::from(defaults.gain_normalize),
                random: i32::from(defaults.random),
                loop_playlist: i32::from(defaults.loop_playlist),
                output_sample_rate: defaults.output_sample_rate,
                cache_dir: ptr::null(),
            };
        }
    }));
}

/// Create an engine for a playlist of UTF-8 file paths.
///
/// Returns null on failure; if `err`/`err_len` are given, writes a NUL-terminated
/// UTF-8 message. An empty playlist (`n_paths == 0`) succeeds; the first render
/// returns 0 and queues `FINISHED`.
///
/// # Safety
/// - `options` must be null or point to a valid `FunkotOptions`.
/// - `paths` must be null or point to `n_paths` C strings (UTF-8).
/// - `err` must be null or point to `err_len` writable bytes.
#[no_mangle]
pub unsafe extern "C" fn funkot_engine_new(
    options: *const FunkotOptions,
    paths: *const *const c_char,
    n_paths: usize,
    err: *mut c_char,
    err_len: usize,
) -> *mut FunkotEngine {
    let result = catch_unwind(AssertUnwindSafe(|| {
        if options.is_null() {
            write_err(err, err_len, "options pointer is NULL");
            return ptr::null_mut();
        }
        // SAFETY: options is non-null.
        let options_c = unsafe { &*options };
        let engine_opts = match options_from_c(options_c) {
            Ok(o) => o,
            Err(m) => {
                write_err(err, err_len, &m);
                return ptr::null_mut();
            }
        };
        let playlist = match collect_paths(paths, n_paths) {
            Ok(p) => p,
            Err(m) => {
                write_err(err, err_len, &m);
                return ptr::null_mut();
            }
        };
        match Engine::new(engine_opts, playlist) {
            Ok(engine) => Box::into_raw(Box::new(FunkotEngine {
                engine,
                events: VecDeque::new(),
            })),
            Err(e) => {
                write_err(err, err_len, &e.to_string());
                ptr::null_mut()
            }
        }
    }));
    match result {
        Ok(ptr) => ptr,
        Err(_) => {
            write_err(err, err_len, "panic during funkot_engine_new");
            ptr::null_mut()
        }
    }
}

/// Pull interleaved stereo f32 audio. Returns frames written (`<= max_frames`);
/// 0 means playback finished. Never blocks.
///
/// # Safety
/// - `engine` must be null or a pointer from [`funkot_engine_new`].
/// - `out` must be null or point to at least `max_frames * 2` writable floats.
#[no_mangle]
pub unsafe extern "C" fn funkot_engine_render(
    engine: *mut FunkotEngine,
    out: *mut f32,
    max_frames: usize,
) -> usize {
    if engine.is_null() || out.is_null() || max_frames == 0 {
        return 0;
    }
    let result = catch_unwind(AssertUnwindSafe(|| {
        // SAFETY: engine/out checked non-null; out has max_frames*2 samples.
        let handle = unsafe { &mut *engine };
        let buf = unsafe { std::slice::from_raw_parts_mut(out, max_frames.saturating_mul(2)) };
        drain_events(handle);
        let n = handle.engine.render(buf);
        drain_events(handle);
        n
    }));
    result.unwrap_or(0)
}

/// Pop one pending event. Returns 1 and fills `*event`, or 0 if none pending.
///
/// # Safety
/// - `engine` must be null or a pointer from [`funkot_engine_new`].
/// - `event` must be null or point to a valid `FunkotEvent`.
#[no_mangle]
pub unsafe extern "C" fn funkot_engine_poll_event(
    engine: *mut FunkotEngine,
    event: *mut FunkotEvent,
) -> i32 {
    if engine.is_null() || event.is_null() {
        return 0;
    }
    let result = catch_unwind(AssertUnwindSafe(|| {
        // SAFETY: engine/event checked non-null.
        let handle = unsafe { &mut *engine };
        drain_events(handle);
        match handle.events.pop_front() {
            Some(ev) => {
                // SAFETY: event is non-null.
                let out = unsafe { &mut *event };
                fill_event(out, ev);
                1
            }
            None => 0,
        }
    }));
    result.unwrap_or(0)
}

/// Stop playback and detach the loader thread (prepare in flight may finish in background).
///
/// # Safety
/// `engine` must be null or a pointer from [`funkot_engine_new`].
#[no_mangle]
pub unsafe extern "C" fn funkot_engine_stop(engine: *mut FunkotEngine) {
    if engine.is_null() {
        return;
    }
    let _ = catch_unwind(AssertUnwindSafe(|| {
        // SAFETY: engine is non-null.
        let handle = unsafe { &mut *engine };
        handle.engine.stop();
        drain_events(handle);
    }));
}

/// Destroy the engine (implies stop). NULL-safe.
///
/// # Safety
/// `engine` must be null or a unique pointer previously returned by
/// [`funkot_engine_new`] that has not already been freed.
#[no_mangle]
pub unsafe extern "C" fn funkot_engine_free(engine: *mut FunkotEngine) {
    if engine.is_null() {
        return;
    }
    let _ = catch_unwind(AssertUnwindSafe(|| {
        // SAFETY: engine is a unique allocation from funkot_engine_new.
        let mut boxed = unsafe { Box::from_raw(engine) };
        boxed.engine.stop();
        drop(boxed);
    }));
}
