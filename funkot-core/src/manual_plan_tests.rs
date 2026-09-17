use super::*;
use crate::testutil::synth_track;

fn track(index: usize, bpm: f64) -> PreparedTrack {
    track_at(index, bpm, 44_100)
}

fn track_at(index: usize, bpm: f64, sr: u32) -> PreparedTrack {
    let audio = synth_track(bpm, 16, 16, 16, sr);
    let frames = audio.frames;
    let outro = frames - (16.0 * sr as f64 * 60.0 / bpm * 4.0).round() as u64;
    PreparedTrack {
        path: PathBuf::from(format!("manual-{index}.wav")), playlist_index: index,
        samples: Arc::new(audio.samples), frames, first_downbeat_out: 0,
        outro_start_out: outro, outro_end_anchored_out: outro,
        intro_bars: 16, outro_bars: 16, gain_linear: 1.0, preview: false, head_only: false,
    }
}

fn request(active: &PreparedTrack, next: &PreparedTrack, start: u64, fade: u32) -> ManualRequest {
    ManualRequest {
        generation: 1, key: manual_key(NavAction::TransitionToNext, active, next),
        playhead: start, deadline: u64::MAX,
        active: Arc::clone(&active.samples), target: Arc::clone(&next.samples),
        bar_frames: 58_800.0, sample_rate: 44_100, target_bpm: 180.0, fade_bars: fade,
    }
}

#[test]
fn manual_plan_uses_an_anchored_one_bar_boundary_when_deadline_blocks_four_bars() {
    let active = track(0, 180.0);
    let next = track(1, 180.0);
    let earliest = 4 * 58_800 + 1;

    let mut four_bar = request(&active, &next, earliest, 4);
    four_bar.deadline = 8 * 58_800;
    let plan = build_manual_plan(four_bar);
    assert!(!plan.simple, "{}", plan.reason);
    assert_eq!(plan.reason, "four-bar sync");
    assert_eq!(plan.start, 8 * 58_800);

    let mut deadline_limited = request(&active, &next, earliest, 4);
    deadline_limited.deadline = 5 * 58_800;
    let plan = build_manual_plan(deadline_limited);
    assert!(!plan.simple, "{}", plan.reason);
    assert_eq!(plan.reason, "bar sync: deadline limited");
    assert_eq!(plan.start, 5 * 58_800);

    let mut no_late_plan = request(&active, &next, earliest, 4);
    no_late_plan.deadline = 5 * 58_800 - 1;
    let plan = build_manual_plan(no_late_plan);
    assert!(plan.simple);
    assert_eq!(plan.reason, "deadline boundary unavailable");
}

#[test]
fn manual_plan_uses_actual_remaining_material_and_preserves_custom_fade() {
    let active = track(0, 180.0);
    let next = track(1, 180.0);
    let mut short_outro_label = active.clone();
    short_outro_label.outro_bars = 1;
    let normal = build_manual_plan(request(&short_outro_label, &next, 4 * 58_800, 4));
    assert!(!normal.simple, "{}", normal.reason);
    assert_eq!(normal.fade_out_end, 8 * 58_800);

    let custom = build_manual_plan(request(&active, &next, 4 * 58_800, 3));
    assert!(!custom.simple, "{}", custom.reason);
    assert_eq!(custom.fade_in_end, 3 * 58_800);
    assert_eq!(custom.fade_out_end, 6 * 58_800);
    assert_ne!(custom.reason, "four-bar sync");

    let near_end = build_manual_plan(request(&active, &next, active.frames - 500, 4));
    assert!(near_end.simple);
    assert!(near_end.start + near_end.prev_nudge + near_end.fade_out_end <= active.frames);
    assert!(near_end.entry + near_end.fade_out_end <= next.frames);

    let mut short_intro = next.clone();
    short_intro.intro_bars = 8;
    let short = build_manual_plan(request(&active, &short_intro, 4 * 58_800, 4));
    assert!(short.simple || short.reason != "four-bar sync");
    assert!(short.entry + short.fade_out_end <= short_intro.frames);
}

#[test]
fn manual_plan_rejects_bad_marker_and_does_not_bridge_a_break() {
    let mut active = track(0, 180.0);
    let next = track(1, 180.0);
    active.first_downbeat_out = 14_700;
    let marker = build_manual_plan(request(&active, &next, 4 * 58_800, 4));
    assert!(marker.simple);
    assert_eq!(marker.reason, "structural marker uncertain");
    active.first_downbeat_out = 0;
    Arc::make_mut(&mut active.samples)[2 * 58_800 * 2..3 * 58_800 * 2].fill(0.0);
    let earliest = 8 * 58_800 + 100;
    let recovered = build_manual_plan(request(&active, &next, earliest, 4));
    assert!(!recovered.simple, "{}", recovered.reason);
    assert_eq!(recovered.reason, "beat sync: structural identity unknown");
    assert_eq!(recovered.start, earliest, "beat fallback must not snap a new bar origin");
    assert_eq!(recovered.fade_in_end, 4 * 58_800);
    assert_eq!(recovered.fade_out_start, 4 * 58_800);
    assert_eq!(recovered.fade_out_end, 8 * 58_800);
    assert!(recovered.start + recovered.prev_nudge + recovered.fade_out_end <= active.frames);
    assert!(recovered.entry + recovered.fade_out_end <= next.frames);

    // A discontinuity inside the proposed real overlap must still reject the
    // beat-only candidate; historical marker loss alone is not enough.
    let mut overlap_break = track(2, 180.0);
    Arc::make_mut(&mut overlap_break.samples)[10 * 58_800 * 2..11 * 58_800 * 2].fill(0.0);
    let rejected = build_manual_plan(request(&overlap_break, &next, 8 * 58_800, 4));
    assert!(rejected.simple);
    assert_eq!(rejected.reason, "previous overlap unsafe");

    // A recovered 90/120 BPM island in the actual overlap is just as unsafe
    // as silence there. The historical break must not turn it into Beat.
    for bpm in [90.0, 120.0] {
        let mut excursion = active.clone();
        let different_tempo = synth_track(bpm, 4, 0, 0, 44_100);
        Arc::make_mut(&mut excursion.samples)[10 * 58_800 * 2..14 * 58_800 * 2]
            .copy_from_slice(&different_tempo.samples[..4 * 58_800 * 2]);
        let rejected = build_manual_plan(request(&excursion, &next, 8 * 58_800, 4));
        assert!(rejected.simple, "{bpm}: {}", rejected.reason);
        assert_ne!(rejected.reason, "beat sync: structural identity unknown");
    }

    // The fallback needs the target's trusted marker corridor as well.
    let mut broken_target = next.clone();
    Arc::make_mut(&mut broken_target.samples)[2 * 58_800 * 2..3 * 58_800 * 2].fill(0.0);
    let rejected = build_manual_plan(request(&active, &broken_target, earliest, 4));
    assert!(rejected.simple);
    assert_eq!(rejected.reason, "structural continuity unknown");
}

#[test]
fn manual_plan_does_not_overlap_large_fixed_speed_drift() {
    let active = track(0, 176.0);
    let next = track(1, 180.0);
    let plan = build_manual_plan(request(&active, &next, 4 * 58_800, 4));
    // A 32-beat overlap differs by 0.71 beats; neither an initial kick match
    // nor a narrow-band BPM candidate establishes safe fixed-speed mixing.
    assert!(plan.simple, "{}", plan.reason);
}

#[test]
fn manual_plan_does_not_claim_phrase_identity_for_preview() {
    let mut active = track(0, 180.0);
    let next = track(1, 180.0);
    active.preview = true;
    let plan = build_manual_plan(request(&active, &next, 4 * 58_800, 4));
    assert!(plan.simple);
    assert_eq!(plan.reason, "unprepared material");
}

#[test]
fn manual_plan_preserves_rounded_boundaries_at_198_bpm() {
    for sr in [44_100, 48_000] {
        let active = track_at(0, 198.0, sr);
        let next = track_at(1, 198.0, sr);
        let bar = sr as f64 * 60.0 / 198.0 * 4.0;
        let boundary = (4.0 * bar).round() as u64;
        for offset in [-1i64, 0, 1] {
            let mut req = request(&active, &next, (boundary as i64 + offset) as u64, 4);
            req.sample_rate = sr;
            req.bar_frames = bar;
            req.target_bpm = 198.0;
            let plan = build_manual_plan(req);
            assert!(!plan.simple, "{sr}/{offset}: {}", plan.reason);
            assert_eq!(plan.start, if offset <= 0 { boundary } else { (8.0 * bar).round() as u64 });
        }
    }
}

#[test]
fn aborting_restart_does_not_release_a_shared_loader_permit() {
    let mut engine = Engine::from_prepared(
        EngineOptions { rate: 1.0, ..EngineOptions::default() },
        vec![track(0, 180.0), track(1, 180.0)],
    ).unwrap();
    let (tx, rx) = mpsc::sync_channel(3);
    engine.permit_tx = tx;
    let restart = engine.manual_simple_plan(NavAction::RestartCurrent, "test restart").unwrap();
    engine.commit_manual_plan(restart);
    engine.abort_active_transition();
    assert!(matches!(rx.try_recv(), Err(TryRecvError::Empty)));
    let next = engine.manual_simple_plan(NavAction::TransitionToNext, "test next").unwrap();
    engine.commit_manual_plan(next);
    assert!(rx.try_recv().is_ok(), "first distinct previous deck arms the third permit");
    engine.abort_active_transition();
    assert!(rx.try_recv().is_ok(), "aborted distinct previous deck releases its permit");
    assert!(matches!(rx.try_recv(), Err(TryRecvError::Empty)));
}

#[test]
fn real_worker_correction_reaches_realtime_output_at_exact_sample() {
    for sr in [44_100, 48_000] {
        let active = track_at(0, 180.0, sr);
        let mut next = track_at(1, 180.0, sr);
        // Deliberately stale fractional marker: the audio pulse is a quarter
        // beat later. A nominal entry demonstrably produces different audio.
        let delay = (sr as f64 * 60.0 / 180.0 / 4.0).round() as usize;
        let mut delayed = vec![0.0; delay * 2];
        delayed.extend_from_slice(&next.samples);
        next.samples = Arc::new(delayed);
        next.frames = (next.samples.len() / 2) as u64;
        let options = EngineOptions { rate: 1.0, output_sample_rate: sr, ..EngineOptions::default() };
        let mut engine = Engine::from_prepared(options.clone(), vec![active.clone(), next.clone()]).unwrap();
        engine.set_realtime(true);
        // Proxy the real worker's result channel to control its arrival without
        // depending on wall-clock sleeps or replacing the production builder.
        let (delivery_tx, delivery_rx) = mpsc::sync_channel(1);
        let real_rx = std::mem::replace(&mut engine.manual_rx, delivery_rx);
        let (observation_tx, observation_rx) = mpsc::sync_channel(1);
        let (release_tx, release_rx) = mpsc::sync_channel(1);
        let (ack_tx, ack_rx) = mpsc::sync_channel(1);
        let proxy = thread::spawn(move || {
            let plan = real_rx.recv().expect("real worker reply");
            observation_tx.send(plan.clone()).unwrap();
            release_rx.recv().unwrap();
            delivery_tx.send(plan).unwrap();
            ack_tx.send(()).unwrap();
        });
        engine.begin_nav(NavAction::TransitionToNext);
        let plan = observation_rx.recv().unwrap();
        assert!(!plan.simple, "{sr}: {}", plan.reason);
        assert!(plan.entry > 0, "real worker must correct the stale marker");
        assert_eq!(plan.prev_nudge, 0);
        release_tx.send(()).unwrap();
        ack_rx.recv().unwrap();
        engine.active.as_mut().unwrap().playhead = plan.start - 1;
        engine.render(&mut [0.0; 2]);
        assert!(engine.transition.is_none(), "cannot apply one sample early");

        let mut reference = Engine::from_prepared(options.clone(), vec![active.clone(), next.clone()]).unwrap();
        reference.set_realtime(true);
        reference.nav_gen = plan.generation;
        reference.active.as_mut().unwrap().playhead = plan.start;
        reference.commit_manual_plan(plan.clone());
        let mut nominal = Engine::from_prepared(options, vec![active, next]).unwrap();
        nominal.set_realtime(true);
        nominal.nav_gen = plan.generation;
        nominal.active.as_mut().unwrap().playhead = plan.start;
        let mut nominal_plan = plan.clone();
        nominal_plan.entry = 0;
        nominal.commit_manual_plan(nominal_plan);

        let mut actual_out = vec![0.0; 4096];
        // Small uneven chunks must preserve the same sample schedule as one
        // large reference callback with an identical already-ready plan.
        for chunk in actual_out.chunks_mut(514) { engine.render(chunk); }
        let mut expected = vec![0.0; actual_out.len()];
        let mut uncorrected = expected.clone();
        reference.render(&mut expected);
        nominal.render(&mut uncorrected);
        assert_eq!(actual_out, expected, "chunk-independent real result at {sr}");
        assert_ne!(actual_out, uncorrected, "nominal entry must change the samples");
        assert_eq!(engine.last_manual_plan_diagnostic().unwrap().start, plan.start);
        proxy.join().unwrap();
    }
}

#[test]
fn engine_adopts_deadline_limited_one_bar_plan_at_its_exact_deadline() {
    let bar = 58_800u64;
    let mut active = track(0, 180.0);
    // The deadline is a one-bar marker boundary but not a four-bar boundary.
    active.outro_start_out = 5 * bar;
    active.outro_end_anchored_out = active.outro_start_out;
    let mut next = active.clone();
    next.path = PathBuf::from("deadline-next.wav");
    next.playlist_index = 1;
    let mut engine = Engine::from_prepared(
        EngineOptions { rate: 1.0, output_sample_rate: 44_100, ..EngineOptions::default() },
        vec![active, next],
    ).unwrap();
    engine.active.as_mut().unwrap().playhead = 3 * bar + 1;
    engine.begin_nav(NavAction::TransitionToNext);
    let start = match engine.pending_nav.as_ref() {
        Some(PendingNav::Planned(plan)) => {
            assert_eq!(plan.reason, "bar sync: deadline limited");
            assert_eq!(plan.start, 5 * bar);
            plan.start
        }
        None => panic!("offline planner did not prepare a deadline-limited plan"),
    };
    assert_eq!(engine.manual_deadline, Some(start));
    engine.active.as_mut().unwrap().playhead = start;
    assert_eq!(engine.render(&mut [0.0; 2]), 1);
    let diagnostic = engine.last_manual_plan_diagnostic().expect("adopted plan diagnostic");
    assert_eq!(diagnostic.reason, "bar sync: deadline limited");
    assert_eq!(diagnostic.start, start);
}

#[test]
fn manual_syncopated_groove_keeps_structural_four_bar_plan() {
    let audio = crate::analysis_tests::syncopated_pulse_track(180.0, 48, 44_100);
    let mut active = track(0, 180.0);
    active.samples = Arc::new(audio.samples);
    active.frames = audio.frames;
    let mut next = active.clone();
    next.path = PathBuf::from("syncopated-next.wav");
    next.playlist_index = 1;
    let plan = build_manual_plan(request(&active, &next, 4 * 58_800, 4));
    assert!(!plan.simple, "syncopated structural groove rejected: {}", plan.reason);
    assert_eq!(plan.reason, "four-bar sync");
    assert_eq!(plan.start, 4 * 58_800);
    assert_eq!(plan.fade_in_end, 4 * 58_800);
    assert_eq!(plan.fade_out_start, 4 * 58_800);
    assert_eq!(plan.fade_out_end, 8 * 58_800);
}
