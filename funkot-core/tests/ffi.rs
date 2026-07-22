//! Integration tests for the C ABI (`ffi` module / `include/funkot.h`).

use std::ffi::{c_char, CStr, CString};
use std::path::{Path, PathBuf};
use std::thread;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use funkot_core::ffi::{
    funkot_engine_free, funkot_engine_new, funkot_engine_poll_event, funkot_engine_render,
    funkot_engine_stop, funkot_options_default, FunkotEvent, FunkotEventType, FunkotOptions,
};
use funkot_core::testutil::{synth_track, write_wav};

fn temp_dir(label: &str) -> PathBuf {
    let mut p = std::env::temp_dir();
    p.push(format!(
        "funkot_ffi_{}_{}_{}",
        label,
        std::process::id(),
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0)
    ));
    let _ = std::fs::create_dir_all(&p);
    p
}

fn c_path(path: &Path) -> CString {
    CString::new(path.to_str().expect("utf-8 path")).expect("no interior NUL")
}

fn event_path_str(event: &FunkotEvent) -> &str {
    // SAFETY: fill_event always NUL-terminates path.
    unsafe { CStr::from_ptr(event.path.as_ptr()) }
        .to_str()
        .expect("event.path must be valid UTF-8")
}

fn default_test_options(cache_dir: &CStr) -> FunkotOptions {
    let mut options = FunkotOptions {
        rate: 0.0,
        pitch_shift: 0,
        fade_bars: 0,
        highpass_hz: 0.0,
        gain_normalize: 0,
        random: 0,
        loop_playlist: 0,
        output_sample_rate: 0,
        cache_dir: std::ptr::null(),
    };
    unsafe {
        funkot_options_default(&mut options);
    }
    options.loop_playlist = 0;
    options.output_sample_rate = 44_100;
    options.cache_dir = cache_dir.as_ptr();
    options.gain_normalize = 0;
    options
}

#[test]
fn ffi_single_track_render_and_events() {
    let dir = temp_dir("single");
    let cache = dir.join("cache");
    let _ = std::fs::create_dir_all(&cache);
    let track_path = dir.join("track.wav");
    let audio = synth_track(180.0, 16, 16, 16, 44_100);
    write_wav(&track_path, &audio).expect("write wav");

    let cache_c = c_path(&cache);
    let track_c = c_path(&track_path);
    let path_ptrs = [track_c.as_ptr()];
    let options = default_test_options(cache_c.as_c_str());

    let mut err = [0i8; 256];
    let engine =
        unsafe { funkot_engine_new(&options, path_ptrs.as_ptr(), 1, err.as_mut_ptr(), err.len()) };
    assert!(
        !engine.is_null(),
        "engine_new failed: {}",
        unsafe { CStr::from_ptr(err.as_ptr()) }.to_string_lossy()
    );

    let mut buf = vec![0.0f32; 4096 * 2];
    let mut produced_audio = false;
    let mut spins = 0u64;
    loop {
        let n = unsafe { funkot_engine_render(engine, buf.as_mut_ptr(), 4096) };
        if n == 0 {
            break;
        }
        if buf[..n * 2].iter().any(|s| s.abs() > 1e-8) {
            produced_audio = true;
        }
        spins += 1;
        if spins > 10_000_000 {
            panic!("timed out rendering");
        }
    }
    assert!(produced_audio, "expected non-silent audio");

    let mut saw_started = false;
    let mut saw_finished = false;
    let expected_path = track_path.to_str().expect("utf-8");
    loop {
        let mut event = FunkotEvent {
            type_: FunkotEventType::None,
            track_index: -1,
            path: [0; 512],
            detail: [0; 256],
        };
        let got = unsafe { funkot_engine_poll_event(engine, &mut event) };
        if got == 0 {
            break;
        }
        match event.type_ {
            FunkotEventType::TrackStarted => {
                assert_eq!(event.track_index, 0);
                assert_eq!(event_path_str(&event), expected_path);
                saw_started = true;
            }
            FunkotEventType::Finished => {
                saw_finished = true;
            }
            other => panic!("unexpected event {other:?}"),
        }
    }
    assert!(saw_started, "TRACK_STARTED missing");
    assert!(saw_finished, "FINISHED missing");

    unsafe {
        funkot_engine_free(engine);
    }
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn ffi_null_and_invalid_inputs() {
    // NULL options → NULL engine + error message.
    let mut err = [0i8; 128];
    let engine = unsafe {
        funkot_engine_new(
            std::ptr::null(),
            std::ptr::null(),
            0,
            err.as_mut_ptr(),
            err.len(),
        )
    };
    assert!(engine.is_null());
    let msg = unsafe { CStr::from_ptr(err.as_ptr()) }.to_string_lossy();
    assert!(msg.contains("NULL"), "err={msg}");

    // Empty playlist: creation succeeds; first render returns 0; FINISHED queued.
    let dir = temp_dir("empty");
    let cache = dir.join("cache");
    let _ = std::fs::create_dir_all(&cache);
    let cache_c = c_path(&cache);
    let options = default_test_options(cache_c.as_c_str());
    err = [0i8; 128];
    let engine =
        unsafe { funkot_engine_new(&options, std::ptr::null(), 0, err.as_mut_ptr(), err.len()) };
    assert!(!engine.is_null(), "empty playlist should succeed");

    let mut buf = vec![1.0f32; 64];
    // Loader may need a timeslice before Exhausted is visible (never block in
    // the engine itself; hosts pull in realtime).
    let mut n = 1usize;
    for _ in 0..500 {
        n = unsafe { funkot_engine_render(engine, buf.as_mut_ptr(), 32) };
        if n == 0 {
            break;
        }
        thread::sleep(Duration::from_millis(1));
    }
    assert_eq!(n, 0, "empty playlist should finish");

    let mut event = FunkotEvent {
        type_: FunkotEventType::None,
        track_index: -1,
        path: [0; 512],
        detail: [0; 256],
    };
    let mut saw_finished = false;
    while unsafe { funkot_engine_poll_event(engine, &mut event) } == 1 {
        if event.type_ == FunkotEventType::Finished {
            saw_finished = true;
        }
    }
    assert!(saw_finished, "FINISHED should be queued for empty playlist");
    unsafe {
        funkot_engine_free(engine);
    }

    // Invalid UTF-8 path → NULL + error message.
    let bad: &[u8] = b"bad\xffpath\0";
    let bad_ptr = bad.as_ptr().cast::<c_char>();
    let path_ptrs = [bad_ptr];
    err = [0i8; 128];
    let engine =
        unsafe { funkot_engine_new(&options, path_ptrs.as_ptr(), 1, err.as_mut_ptr(), err.len()) };
    assert!(engine.is_null());
    let msg = unsafe { CStr::from_ptr(err.as_ptr()) }.to_string_lossy();
    assert!(msg.contains("UTF-8"), "err={msg}");

    // free(NULL) is safe.
    unsafe {
        funkot_engine_free(std::ptr::null_mut());
        funkot_engine_stop(std::ptr::null_mut());
    }

    // poll with NULL event pointer returns 0 (and does not require a live engine).
    assert_eq!(
        unsafe { funkot_engine_poll_event(std::ptr::null_mut(), std::ptr::null_mut()) },
        0
    );

    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn ffi_path_truncation_valid_utf8() {
    let dir = temp_dir("trunc");
    let cache = dir.join("cache");
    let _ = std::fs::create_dir_all(&cache);

    // Build a path longer than 512 bytes via nested directories (NAME_MAX is 255).
    let mut deep = dir.clone();
    for _ in 0..8 {
        deep = deep.join("x".repeat(80));
        std::fs::create_dir_all(&deep).expect("mkdir");
    }
    let track_path = deep.join("track.wav");
    let full = track_path.to_str().expect("utf-8 path");
    assert!(full.len() > 512, "test setup path len={}", full.len());

    let audio = synth_track(180.0, 16, 8, 8, 44_100);
    write_wav(&track_path, &audio).expect("write wav");

    let cache_c = c_path(&cache);
    let track_c = c_path(&track_path);
    let path_ptrs = [track_c.as_ptr()];
    let options = default_test_options(cache_c.as_c_str());

    let engine =
        unsafe { funkot_engine_new(&options, path_ptrs.as_ptr(), 1, std::ptr::null_mut(), 0) };
    assert!(!engine.is_null());

    // Pull until TRACK_STARTED appears (loader may take a moment).
    let mut buf = vec![0.0f32; 1024 * 2];
    let mut started_path = None;
    for _ in 0..10_000_000 {
        let _ = unsafe { funkot_engine_render(engine, buf.as_mut_ptr(), 1024) };
        let mut event = FunkotEvent {
            type_: FunkotEventType::None,
            track_index: -1,
            path: [0; 512],
            detail: [0; 256],
        };
        while unsafe { funkot_engine_poll_event(engine, &mut event) } == 1 {
            if event.type_ == FunkotEventType::TrackStarted {
                let s = event_path_str(&event).to_string();
                // Must be NUL-terminated inside 512 and a valid UTF-8 prefix.
                assert!(s.len() < 512);
                assert!(
                    full.starts_with(&s),
                    "truncated path is not a prefix:\n  got={s}\n  full={full}"
                );
                // Ensure we actually truncated.
                assert!(s.len() < full.len());
                started_path = Some(s);
                break;
            }
        }
        if started_path.is_some() {
            break;
        }
    }
    assert!(started_path.is_some(), "TRACK_STARTED not observed");

    unsafe {
        funkot_engine_free(engine);
    }
    let _ = std::fs::remove_dir_all(&dir);
}
