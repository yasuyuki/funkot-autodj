//! Integration tests for the funkot-autodj CLI (container-safe: no live audio).

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{SystemTime, UNIX_EPOCH};

use funkot_cli::playlist::{load_playlist_file, parse_playlist_lines, validate_paths_exist};
use funkot_core::testutil::{synth_track, write_wav};

fn temp_dir(label: &str) -> PathBuf {
    let mut p = std::env::temp_dir();
    p.push(format!(
        "funkot_cli_{}_{}_{}",
        label,
        std::process::id(),
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0)
    ));
    fs::create_dir_all(&p).expect("temp dir");
    p
}

fn bin() -> Command {
    Command::new(env!("CARGO_BIN_EXE_funkot-autodj"))
}

#[test]
fn playlist_comments_blanks_and_relative_paths() {
    let dir = temp_dir("playlist_ok");
    let track = dir.join("track.wav");
    fs::write(&track, b"x").expect("touch track");

    let sub = dir.join("lists");
    fs::create_dir_all(&sub).expect("sub");
    let list = sub.join("pl.txt");
    fs::write(&list, "\n# comment\n\n../track.wav\n# another\n\n").expect("write list");

    let entries = load_playlist_file(&list).expect("load");
    assert_eq!(entries.len(), 1);
    // Relative paths are joined against the playlist directory (not canonicalized).
    assert_eq!(entries[0], sub.join("../track.wav"));
    assert!(entries[0].exists());
    assert_eq!(
        fs::canonicalize(&entries[0]).expect("canon entry"),
        fs::canonicalize(&track).expect("canon track")
    );

    let parsed = parse_playlist_lines("# only comment\n\n", &dir).expect("empty ok");
    assert!(parsed.is_empty());
}

#[test]
fn playlist_missing_file_errors() {
    let dir = temp_dir("playlist_missing");
    let err = parse_playlist_lines("nope.wav\nalso_missing.flac\n", &dir).expect_err("should fail");
    let msg = format!("{err:#}");
    assert!(msg.contains("missing playlist entries"), "message={msg}");
    assert!(msg.contains("nope.wav"), "message={msg}");
    assert!(msg.contains("also_missing.flac"), "message={msg}");

    let err = validate_paths_exist(&[dir.join("ghost.mp3")]).expect_err("missing");
    assert!(
        format!("{err:#}").contains("missing playlist entries"),
        "{err:#}"
    );
}

#[test]
fn render_two_tracks_end_to_end() {
    let dir = temp_dir("render_e2e");
    let cache = dir.join("cache");
    let a = dir.join("a.wav");
    let b = dir.join("b.wav");
    let list = dir.join("list.txt");
    let out = dir.join("out.wav");

    let sr = 44_100u32;
    // Keep total bars roughly constant, but shorten intro+main so that
    // the engine reaches `outro_start` early enough for `TransitionStarted`.
    write_wav(&a, &synth_track(180.0, 8, 8, 48, sr)).expect("a");
    write_wav(&b, &synth_track(178.0, 8, 8, 48, sr)).expect("b");
    fs::write(&list, "a.wav\nb.wav\n").expect("list");

    let status = bin()
        .args([
            "-l",
            list.to_str().unwrap(),
            "--render",
            out.to_str().unwrap(),
            "--render-speed",
            "10",
            "--transition-clip-seconds",
            "2",
            "--sample-rate",
            "44100",
            "--cache-dir",
            cache.to_str().unwrap(),
        ])
        .status()
        .expect("spawn");
    assert!(status.success(), "exit status {status}");
    assert!(out.is_file(), "out.wav missing");

    // Transition clips should be emitted automatically for `--render`.
    let transitions_dir = dir.join("out_transitions");
    let entries: Vec<_> = fs::read_dir(&transitions_dir)
        .expect("read transitions dir")
        .collect();
    assert_eq!(
        entries.len(),
        1,
        "expected 1 transition clip, got {}",
        entries.len()
    );
    let clip_path = entries[0].as_ref().unwrap().path();
    let clip_reader = hound::WavReader::open(&clip_path).expect("open clip");
    let spec = clip_reader.spec();
    assert_eq!(spec.channels, 2);
    assert_eq!(spec.sample_rate, 44_100);
    assert_eq!(spec.bits_per_sample, 32);
    assert_eq!(spec.sample_format, hound::SampleFormat::Float);
    let samples = clip_reader.len() as u64;
    let frames = samples / 2;
    let duration = frames as f64 / 44_100.0;
    assert!(
        duration > 0.5,
        "expected clip duration > 0.5s, got {duration:.2}s"
    );

    let mut reader = hound::WavReader::open(&out).expect("open out");
    let spec = reader.spec();
    assert_eq!(spec.channels, 2);
    assert_eq!(spec.sample_rate, 44_100);
    // Default offline format is 32-bit IEEE float.
    assert_eq!(spec.bits_per_sample, 32);
    assert_eq!(spec.sample_format, hound::SampleFormat::Float);

    let samples: Vec<f32> = reader
        .samples::<f32>()
        .map(|s| s.expect("sample"))
        .collect();
    assert!(!samples.is_empty());
    let frames = samples.len() / 2;
    let duration = frames as f64 / f64::from(sr);
    assert!(
        duration > 60.0,
        "expected duration > 60s, got {duration:.2}s ({frames} frames)"
    );
    assert!(samples.iter().any(|&s| s != 0.0), "samples are all zero");

    let max_lead = (0.2 * f64::from(sr)).ceil() as usize;
    let lead_silent = samples
        .chunks(2)
        .take(max_lead)
        .position(|frame| frame.iter().any(|&s| s != 0.0));
    assert!(
        lead_silent.is_some(),
        "no non-zero sample within first 0.2s (leading silence too long)"
    );
}

#[test]
fn render_transitions_only_is_shorter_than_full_mix() {
    let dir = temp_dir("render_transitions_only");
    let cache = dir.join("cache");
    let a = dir.join("a.wav");
    let b = dir.join("b.wav");
    let list = dir.join("list.txt");
    let full = dir.join("full.wav");
    let only = dir.join("only.wav");

    let sr = 44_100u32;
    write_wav(&a, &synth_track(180.0, 8, 8, 48, sr)).expect("a");
    write_wav(&b, &synth_track(178.0, 8, 8, 48, sr)).expect("b");
    fs::write(&list, "a.wav\nb.wav\n").expect("list");

    let args_full = [
        "-l",
        list.to_str().unwrap(),
        "--render",
        full.to_str().unwrap(),
        "--render-speed",
        "10",
        "--transition-clip-seconds",
        "2",
        "--sample-rate",
        "44100",
        "--cache-dir",
        cache.to_str().unwrap(),
    ];
    assert!(bin().args(args_full).status().expect("full").success());

    let args_only = [
        "-l",
        list.to_str().unwrap(),
        "--render",
        only.to_str().unwrap(),
        "--render-speed",
        "10",
        "--transition-clip-seconds",
        "2",
        "--sample-rate",
        "44100",
        "--cache-dir",
        cache.to_str().unwrap(),
        "--transitions-only",
    ];
    assert!(bin().args(args_only).status().expect("only").success());

    let full_frames = hound::WavReader::open(&full).unwrap().len() as u64 / 2;
    let only_frames = hound::WavReader::open(&only).unwrap().len() as u64 / 2;
    assert!(
        only_frames > u64::from(sr) / 2,
        "transitions-only OUT too short: {only_frames} frames"
    );
    assert!(
        only_frames < full_frames / 2,
        "transitions-only ({only_frames}) should be much shorter than full ({full_frames})"
    );
}

#[test]
fn transitions_only_rejects_single_track() {
    let dir = temp_dir("transitions_only_one");
    let a = dir.join("a.wav");
    write_minimal_wav(&a);

    let output = bin()
        .args([a.to_str().unwrap(), "--transitions-only", "--no-loop"])
        .output()
        .expect("spawn");
    assert!(!output.status.success());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("--transitions-only") && stderr.contains("≥ 2"),
        "stderr={stderr}"
    );
}

#[test]
fn render_s16_wav_format() {
    let dir = temp_dir("render_s16");
    let cache = dir.join("cache");
    let a = dir.join("a.wav");
    let out = dir.join("out.wav");

    let sr = 44_100u32;
    write_wav(&a, &synth_track(180.0, 8, 16, 8, sr)).expect("a");

    let status = bin()
        .args([
            a.to_str().unwrap(),
            "--render",
            out.to_str().unwrap(),
            "--wav-format",
            "s16",
            "--render-speed",
            "0",
            "--transition-clip-seconds",
            "2",
            "--sample-rate",
            "44100",
            "--cache-dir",
            cache.to_str().unwrap(),
            "--no-loop",
        ])
        .status()
        .expect("spawn");
    assert!(status.success(), "exit status {status}");

    let reader = hound::WavReader::open(&out).expect("open out");
    let spec = reader.spec();
    assert_eq!(spec.bits_per_sample, 16);
    assert_eq!(spec.sample_format, hound::SampleFormat::Int);
}

#[test]
fn rate_out_of_range_fails() {
    let dir = temp_dir("rate_bad");
    let track = dir.join("t.wav");
    write_minimal_wav(&track);

    let output = bin()
        .args([
            track.to_str().unwrap(),
            "--rate",
            "5.0",
            "--render",
            dir.join("o.wav").to_str().unwrap(),
            "--render-speed",
            "0",
            "--no-loop",
        ])
        .output()
        .expect("spawn");
    assert!(!output.status.success());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("--rate") || stderr.contains("0.5") || stderr.contains("2.0"),
        "stderr={stderr}"
    );
}

#[test]
fn files_and_list_together_fails() {
    let dir = temp_dir("both");
    let track = dir.join("t.wav");
    write_minimal_wav(&track);
    let list = dir.join("l.txt");
    fs::write(&list, "t.wav\n").expect("list");

    let output = bin()
        .args([track.to_str().unwrap(), "-l", list.to_str().unwrap()])
        .output()
        .expect("spawn");
    assert!(!output.status.success());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("cannot combine") || stderr.contains("--list"),
        "stderr={stderr}"
    );
}

#[test]
fn missing_playlist_entry_fails() {
    let dir = temp_dir("missing_cli");
    let list = dir.join("l.txt");
    fs::write(&list, "does_not_exist.flac\n").expect("list");

    let output = bin()
        .args(["-l", list.to_str().unwrap(), "--no-loop"])
        .output()
        .expect("spawn");
    assert!(!output.status.success());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("missing playlist entries") || stderr.contains("does_not_exist"),
        "stderr={stderr}"
    );
}

#[test]
fn gen_test_fixtures_writes_golden() {
    let dir = temp_dir("gen_fixtures");
    let status = bin()
        .args(["--gen-test-fixtures", dir.to_str().unwrap()])
        .status()
        .expect("spawn");
    assert!(status.success(), "exit status {status}");
    assert!(dir.join("golden.json").is_file());
    assert!(dir.join("classic_180_i8_m8_o8.wav").is_file());
    assert!(dir.join("README.md").is_file());
}

#[test]
fn label_sections_render_clips_writes_expected_clip_lengths() {
    use funkot_cli::label_session::{
        Side, CONTINUE_AFTER_WINDOW_BARS, INTRO_CANDIDATES, OUTRO_CANDIDATES,
    };

    let dir = temp_dir("label_render_clips");
    let track = dir.join("track.wav");
    let sr = 44_100u32;
    write_wav(&track, &synth_track(180.0, 16, 32, 16, sr)).expect("track");
    let list = dir.join("list.txt");
    fs::write(&list, "track.wav\n").expect("list");
    let cache = dir.join("cache");
    let labels = dir.join("labels.tsv");
    let clips_dir = dir.join("clips");

    let status = bin()
        .args([
            "--label-sections",
            "-l",
            list.to_str().unwrap(),
            "--labels",
            labels.to_str().unwrap(),
            "--cache-dir",
            cache.to_str().unwrap(),
            "--render-clips",
            clips_dir.to_str().unwrap(),
        ])
        .status()
        .expect("spawn");
    assert!(status.success(), "exit status {status}");

    // --render-clips never writes labels; it's a verification-only path.
    assert!(!labels.exists(), "render-clips must not write labels.tsv");

    let entries: Vec<_> = fs::read_dir(&clips_dir)
        .expect("read clips dir")
        .filter_map(|e| e.ok())
        .collect();
    assert_eq!(
        entries.len(),
        INTRO_CANDIDATES.len() + OUTRO_CANDIDATES.len(),
        "expected one clip per intro/outro candidate"
    );

    // A clip is the ±8-bar window plus however much of the track follows it,
    // capped at `CONTINUE_AFTER_WINDOW_BARS` and truncated at the file end
    // (label_session::render_click_clip). The fixture is 64 bars at 180 BPM,
    // so every candidate hits the file end well before the cap: intro clips
    // get shorter the deeper the candidate sits (less track left after it) and
    // outro clips get longer (they start further back and all run to the end).
    let bar_secs = 60.0 / 180.0 * 4.0;
    let window_secs = 16.0 * bar_secs;
    let max_secs = f64::from(16 + CONTINUE_AFTER_WINDOW_BARS) * bar_secs;
    let duration = |side: Side, bars: u32| -> f64 {
        let path = clips_dir.join(format!("track_{}_{bars:03}bars.wav", side.label()));
        let reader = hound::WavReader::open(&path)
            .unwrap_or_else(|e| panic!("open {}: {e}", path.display()));
        let spec = reader.spec();
        assert_eq!(spec.channels, 2);
        assert_eq!(spec.sample_rate, sr);
        (reader.len() as f64 / 2.0) / f64::from(sr)
    };

    for (side, candidates) in [
        (Side::Intro, Side::Intro.candidates()),
        (Side::Outro, Side::Outro.candidates()),
    ] {
        let mut previous: Option<f64> = None;
        for &bars in candidates {
            let secs = duration(side, bars);
            assert!(
                secs >= window_secs - 0.01,
                "{side:?} {bars} bars: {secs:.3}s is shorter than the {window_secs:.3}s window"
            );
            assert!(
                secs <= max_secs + 0.01,
                "{side:?} {bars} bars: {secs:.3}s runs past the {max_secs:.3}s cap"
            );
            if let Some(prev) = previous {
                // One bar of slack: which frame the boundary lands on moves
                // with the analyzer, the ordering does not.
                match side {
                    Side::Intro => assert!(
                        secs <= prev + bar_secs,
                        "intro {bars} bars: {secs:.3}s is longer than the previous \
                         candidate's {prev:.3}s; deeper candidates leave less track"
                    ),
                    Side::Outro => assert!(
                        secs >= prev - bar_secs,
                        "outro {bars} bars: {secs:.3}s is shorter than the previous \
                         candidate's {prev:.3}s; deeper candidates start further back"
                    ),
                }
            }
            previous = Some(secs);
        }
    }

    // The shallowest candidate on each side has most of the track after its
    // window, so it is the one that proves playback really continues.
    assert!(
        duration(Side::Intro, 8) > window_secs + bar_secs,
        "intro 8 bars: clip stops at the window instead of playing on"
    );
    assert!(
        duration(Side::Outro, 64) > window_secs + bar_secs,
        "outro 64 bars: clip stops at the window instead of playing on"
    );
}

#[test]
fn label_sections_render_clips_skips_already_labeled_tracks() {
    use funkot_core::cache::content_hash;
    use funkot_core::labels::{save_labels, SectionLabel};

    let dir = temp_dir("label_skip_labeled");
    let track = dir.join("track.wav");
    write_wav(&track, &synth_track(180.0, 16, 32, 16, 44_100)).expect("track");
    let list = dir.join("list.txt");
    fs::write(&list, "track.wav\n").expect("list");
    let cache = dir.join("cache");
    let labels = dir.join("labels.tsv");
    let clips_dir = dir.join("clips");

    let hash = content_hash(&track).expect("hash");
    save_labels(
        &labels,
        &[SectionLabel {
            hash,
            file_name: "track.wav".to_string(),
            intro_best: 16,
            intro_ok: vec![],
            outro_best: 16,
            outro_ok: vec![],
            note: String::new(),
        }],
    )
    .expect("seed labels");

    let output = bin()
        .args([
            "--label-sections",
            "-l",
            list.to_str().unwrap(),
            "--labels",
            labels.to_str().unwrap(),
            "--cache-dir",
            cache.to_str().unwrap(),
            "--render-clips",
            clips_dir.to_str().unwrap(),
        ])
        .output()
        .expect("spawn");
    assert!(output.status.success());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("1 of 1 already labeled"),
        "stderr={stderr}"
    );

    let entries: Vec<_> = fs::read_dir(&clips_dir)
        .expect("read clips dir")
        .filter_map(|e| e.ok())
        .collect();
    assert!(
        entries.is_empty(),
        "already-labeled track should produce no clips"
    );
}

fn write_minimal_wav(path: &Path) {
    // Tiny valid WAV so path existence checks pass; render tests use synth tracks.
    write_wav(path, &synth_track(180.0, 1, 1, 1, 8_000)).expect("minimal wav");
}
