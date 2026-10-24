/// Bars of the next track's intro that remain after the previous track's
/// fade-out has completed (and before the next main section starts).
pub const MAIN_GAP_BARS: u32 = 8;

/// Pure transition schedule in bars relative to T = previous track's outro start
/// (= next entry). Anchored so the next track's main (intro end, T0) falls at
/// `T + m`, with a compact fade pair and [`MAIN_GAP_BARS`] of next-intro solo.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TransitionPlan {
    pub f_eff: u32,
    /// Bars from T until the next main, capped by the available intro.
    pub m: u32,
    /// Bars of the next track's intro skipped before entry (`I − m`).
    pub skip: u32,
    pub fadeout_start: u32,
    pub fadeout_end: u32,
}

/// Compute the bar-grid transition schedule.
///
/// Timeline relative to next-intro end T0 (main start):
/// - T0−`m`: next enters (HPF, vol 0); `m = 2·F + MAIN_GAP` when it fits
/// - T0−`m`+`F`: next fade-in done; HPF flips to prev; prev fade-out starts
/// - T0−[`MAIN_GAP_BARS`]: prev at vol 0 → deck dropped
/// - T0: next main
///
/// `F` = requested fade bars, `I` = next intro bars, `O` = previous outro bars.
/// Short `I`/`O` shrink `F` so the compact window still fits; `m` is never
/// stretched to fill a long intro (long intros are entered mid-way via `skip`).
/// If no fade pair fits, `f_eff` is zero. Frame admission chooses a safe cut
/// or simple fade from the actual material available to both decks.
pub fn plan_transition(fade_bars: u32, intro_bars: u32, outro_bars: u32) -> TransitionPlan {
    let f = fade_bars;
    let i = intro_bars;
    let o = outro_bars;

    // Need 2·F bars of outro (fade-in+out) and F+(F)+MAIN_GAP of intro.
    let f_eff = {
        let from_intro = i.saturating_sub(MAIN_GAP_BARS) / 2;
        let from_outro = o / 2;
        f.min(from_intro).min(from_outro)
    };

    let m = (2 * f_eff + MAIN_GAP_BARS).min(i);
    let skip = i.saturating_sub(m);

    // Ideal: fade-out occupies [f_eff, 2·f_eff]. Clamp when m is short so
    // MAIN_GAP still remains after fade-out (may yield a zero-length fade).
    let fadeout_end = (2 * f_eff).min(m.saturating_sub(MAIN_GAP_BARS)).min(o);
    let fadeout_start = fadeout_end
        .saturating_sub(f_eff)
        .max(f_eff.min(fadeout_end));

    TransitionPlan {
        f_eff,
        m,
        skip,
        fadeout_start,
        fadeout_end,
    }
}

/// Bars before the structural outro boundary that the mix trigger sits at.
///
/// With the default four-bar fade pair and eight-bar solo gap, the nominal
/// transition occupies this lead. Shortened or differently configured fades
/// need not finish at the structural boundary.
pub(crate) const OUTRO_LEAD_BARS: u32 = 16;

/// Where the transition starts, given the structural boundary: one fixed
/// rule, applied to every track.
///
/// It used to be conditional — the lead was dropped when the outro analysis
/// window was too short to also cover it, and the result was clamped to
/// [`crate::FALLBACK_BARS`]. Both conditions fired on real material: a track whose
/// outro starts 64 bars from the end got trigger == boundary, so the whole
/// transition ran *inside* the outro, which is the one place it must not
/// (measured: 3 of the 34 masters in the working cache). Neither condition
/// was about the trigger being right — the lead is arithmetic on the
/// boundary, not something the analysis window has to cover.
///
/// Positions are measured in bars from the end. A larger trigger is earlier
/// in playback. With a valid structural boundary, the trigger leads it by up
/// to 16 bars without cutting into the intro. If that boundary is outside the
/// available track body, the trigger is clipped to the intro end instead.
pub(crate) fn outro_trigger_bars(structure_bars: u32, intro_bars: u32, track_bars: u32) -> u32 {
    let room = track_bars.saturating_sub(intro_bars);
    structure_bars
        .saturating_add(OUTRO_LEAD_BARS)
        .min(room)
}

/// Frame ranges admitted together when one deck hands off to another.
///
/// A full plan must fit both the corrected overlap and its target horizon.
/// A simple fallback instead shrinks its one shared fade to the available PCM.
#[derive(Clone, Copy)]
pub(crate) struct FrameAdmission {
    pub(crate) prev_start: u64,
    pub(crate) entry: u64,
    pub(crate) fade_in_end: u64,
    pub(crate) fade_out_start: u64,
    pub(crate) fade_out_end: u64,
}

pub(crate) fn admit_frames(
    prev_frames: u64,
    target_frames: u64,
    prev_start: u64,
    entry: u64,
    fade_in_end: u64,
    fade_out_start: u64,
    fade_out_end: u64,
    target_horizon: u64,
    simple: bool,
) -> Option<FrameAdmission> {
    if prev_start >= prev_frames || entry >= target_frames {
        return None;
    }
    let available = prev_frames
        .saturating_sub(prev_start)
        .min(target_frames.saturating_sub(entry));
    if available == 0 {
        return None;
    }
    if simple {
        let fade_in_end = fade_in_end.min(available);
        let fade_out_end = fade_out_end.min(available);
        return Some(FrameAdmission {
            prev_start,
            entry,
            fade_in_end,
            fade_out_start: fade_out_start.min(fade_out_end),
            fade_out_end,
        });
    }
    if fade_out_start > fade_out_end
        || fade_out_end > prev_frames - prev_start
        || fade_in_end.max(fade_out_end).max(target_horizon) > target_frames - entry
    {
        return None;
    }
    Some(FrameAdmission { prev_start, entry, fade_in_end, fade_out_start, fade_out_end })
}

