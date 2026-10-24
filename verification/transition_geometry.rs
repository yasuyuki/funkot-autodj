#[path = "../funkot-core/src/transition_geometry.rs"]
mod transition_geometry;

use transition_geometry::{outro_trigger_bars, plan_transition, MAIN_GAP_BARS};

#[kani::proof]
fn frame_admission_all_u64() {
    let prev: u64 = kani::any();
    let next: u64 = kani::any();
    let start: u64 = kani::any();
    let entry: u64 = kani::any();
    let fade_in: u64 = kani::any();
    let fade_start: u64 = kani::any();
    let fade_end: u64 = kani::any();
    let horizon: u64 = kani::any();
    let simple: bool = kani::any();
    let admitted = transition_geometry::admit_frames(
        prev, next, start, entry, fade_in, fade_start, fade_end, horizon, simple,
    );
    if let Some(p) = admitted {
        assert!(p.prev_start == start && p.entry == entry);
        assert!(start < prev && entry < next);
        assert!(p.fade_out_start <= p.fade_out_end);
        assert!(p.fade_out_end <= prev - start);
        assert!(p.fade_in_end.max(p.fade_out_end) <= next - entry);
        if !simple {
            assert!(horizon <= next - entry);
            assert!(p.fade_in_end == fade_in && p.fade_out_start == fade_start && p.fade_out_end == fade_end);
        }
        kani::cover!(simple && p.fade_in_end == 1 && p.fade_out_end == 1);
        kani::cover!(!simple && p.fade_out_end > 0);
    }
    kani::cover!(admitted.is_none() && next == 0);
    kani::cover!(admitted.is_none() && start == u64::MAX);
}

#[kani::proof]
fn plan_all_u32() {
    let fade: u32 = kani::any();
    let intro: u32 = kani::any();
    let outro: u32 = kani::any();
    let p = plan_transition(fade, intro, outro);
    assert!(p.m <= intro);
    assert!(p.skip + p.m == intro);
    assert!(p.f_eff <= fade);
    assert!(p.f_eff <= intro && p.f_eff <= outro);
    assert!(p.fadeout_start <= p.fadeout_end && p.fadeout_end <= outro);
    assert!(p.fadeout_end <= p.m);
    assert!(p.fadeout_end - p.fadeout_start <= p.f_eff);
    assert!(p.f_eff == 0 || p.f_eff <= p.fadeout_start);
    if intro >= MAIN_GAP_BARS {
        assert!(p.m - p.fadeout_end >= MAIN_GAP_BARS);
    }
    if fade <= intro.saturating_sub(MAIN_GAP_BARS) / 2 && fade <= outro / 2 {
        assert!(p.f_eff == fade);
        assert!(p.fadeout_end == 2 * fade);
    }
    kani::cover!(intro == 0 && outro == 0);
    kani::cover!(fade == u32::MAX && intro == u32::MAX && outro == u32::MAX);
    kani::cover!(p.f_eff == 0 && intro > 0 && outro > 0);
    kani::cover!(p.f_eff > 0 && p.skip > 0);
}

#[kani::proof]
fn trigger_all_u32() {
    let structure: u32 = kani::any();
    let intro: u32 = kani::any();
    let track: u32 = kani::any();
    let trigger = outro_trigger_bars(structure, intro, track);
    let room = track.saturating_sub(intro);
    assert!(trigger <= room && trigger <= track);
    if structure <= room {
        assert!(trigger >= structure);
        assert!(trigger - structure <= 16);
        if room - structure >= 16 {
            assert!(trigger - structure == 16);
        } else {
            assert!(trigger == room);
        }
    } else {
        assert!(trigger == room);
    }
    kani::cover!(track == 0);
    kani::cover!(structure > room && intro > track);
    kani::cover!(structure <= room && room - structure < 16);
    kani::cover!(structure <= room && room - structure >= 16);
    kani::cover!(structure == u32::MAX && track == u32::MAX);
}
