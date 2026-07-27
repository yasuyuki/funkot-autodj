//! Pure state machine and click-track clip synthesis for `--label-sections`.
//!
//! Decoupled from crossterm (key input) and cpal (playback) so the candidate
//! navigation / accept / skip / ambiguous-set logic is unit-testable without
//! a terminal or audio device, the same way `nav_keys.rs` keeps multi-tap
//! coalescing testable in isolation. `main.rs` owns the raw-mode key loop
//! and the cpal stream; it only talks to this module through [`LabelKey`] in
//! and [`LabelOutcome`] out, plus [`build_candidate_clip`] for the audio to
//! play (or, under `--render-clips`, to write to disk).

use std::f32::consts::PI;

use funkot_core::decode::AudioBuffer;
use funkot_core::{TrackAnalysis, BEATS_PER_BAR};

/// Intro candidate bar counts (see plan: widest plausible Funkot intro).
pub const INTRO_CANDIDATES: [u32; 7] = [8, 16, 32, 48, 64, 80, 96];
/// Outro candidate bar counts. This is the *structural* main->outro
/// boundary (compare against `TrackAnalysis::outro_structure_bars`), not
/// the DJ mix-trigger `outro_bars` — see `funkot_core::labels` module docs.
pub const OUTRO_CANDIDATES: [u32; 5] = [8, 16, 32, 48, 64];

/// Half-width of the normal listening window, in bars each side of the
/// candidate boundary (16 bars total, ~21s at 180 BPM).
pub const NORMAL_HALF_WIDTH_BARS: u32 = 8;
/// Half-width after `+` widens the window (32 bars total, ~43s at 180 BPM).
pub const WIDE_HALF_WIDTH_BARS: u32 = 16;

/// Which end of the track is currently being labeled.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Side {
    Intro,
    Outro,
}

impl Side {
    pub fn candidates(self) -> &'static [u32] {
        match self {
            Side::Intro => &INTRO_CANDIDATES,
            Side::Outro => &OUTRO_CANDIDATES,
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Side::Intro => "intro",
            Side::Outro => "outro",
        }
    }
}

/// One user keystroke, already decoded from the terminal by `main.rs`'s key
/// mapper. `Note` carries the line the user typed after `n`, collected
/// outside raw mode before it reaches the state machine.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LabelKey {
    /// `y` / Enter: accept the current candidate, or confirm an ambiguous set.
    Accept,
    Left,
    Right,
    /// `+`: toggle the listening window between ±8 and ±16 bars.
    ToggleWidth,
    /// `r`: replay the same window.
    Replay,
    /// `a`: enter, or toggle a candidate within, the ambiguous acceptable set.
    Ambiguous,
    Note(String),
    Skip,
    Quit,
}

/// One side's finished label: best guess plus any other acceptable values.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LabelChoice {
    pub best: u32,
    pub ok: Vec<u32>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum Mode {
    Normal,
    /// `selected[i]` mirrors `side.candidates()[i]`.
    Ambiguous { selected: Vec<bool> },
}

/// What the caller should do after [`TrackSession::apply_key`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LabelOutcome {
    /// State changed (e.g. an ambiguous-set toggle) but the audio window
    /// didn't; redraw status only.
    Continue,
    /// (Re)build and play the clip for the current side/candidate/width.
    Replay,
    /// Both sides labeled; caller persists and moves to the next track.
    Done { intro: LabelChoice, outro: LabelChoice },
    /// Skip this track: write nothing, move on.
    Skip,
    /// Stop the whole session. Already-finished tracks were saved as they
    /// completed, so there is nothing left to flush here.
    Quit,
}

/// Interactive per-track candidate-navigation state. Pure: no I/O, no
/// dependency on crossterm/cpal, safe to construct and drive in tests.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TrackSession {
    side: Side,
    cursor: usize,
    outro_start_index: usize,
    wide_context: bool,
    mode: Mode,
    note: String,
    intro_choice: Option<LabelChoice>,
}

impl TrackSession {
    /// `intro_initial_bars` / `outro_initial_bars` seed the starting
    /// candidate (nearest match) for each side — normally the analyzer's
    /// current `intro_bars` / `outro_structure_bars` estimate.
    pub fn new(intro_initial_bars: u32, outro_initial_bars: u32) -> Self {
        Self {
            side: Side::Intro,
            cursor: nearest_index(Side::Intro.candidates(), intro_initial_bars),
            outro_start_index: nearest_index(Side::Outro.candidates(), outro_initial_bars),
            wide_context: false,
            mode: Mode::Normal,
            note: String::new(),
            intro_choice: None,
        }
    }

    pub fn current_side(&self) -> Side {
        self.side
    }

    /// Candidate bar count currently highlighted (viewed/played).
    pub fn current_bars(&self) -> u32 {
        self.side.candidates()[self.cursor]
    }

    pub fn context_half_width_bars(&self) -> u32 {
        if self.wide_context {
            WIDE_HALF_WIDTH_BARS
        } else {
            NORMAL_HALF_WIDTH_BARS
        }
    }

    pub fn note(&self) -> &str {
        &self.note
    }

    /// `Some(selected bar values, ascending)` while in ambiguous-set mode.
    pub fn ambiguous_selected(&self) -> Option<Vec<u32>> {
        match &self.mode {
            Mode::Normal => None,
            Mode::Ambiguous { selected } => Some(
                self.side
                    .candidates()
                    .iter()
                    .zip(selected)
                    .filter(|(_, &sel)| sel)
                    .map(|(&v, _)| v)
                    .collect(),
            ),
        }
    }

    /// Apply one decoded keystroke, returning what the caller should do.
    pub fn apply_key(&mut self, key: LabelKey) -> LabelOutcome {
        match key {
            LabelKey::Quit => LabelOutcome::Quit,
            LabelKey::Skip => LabelOutcome::Skip,
            LabelKey::Note(text) => {
                self.note = text;
                LabelOutcome::Continue
            }
            LabelKey::ToggleWidth => {
                self.wide_context = !self.wide_context;
                LabelOutcome::Replay
            }
            LabelKey::Replay => LabelOutcome::Replay,
            LabelKey::Left => {
                self.move_cursor(-1);
                LabelOutcome::Replay
            }
            LabelKey::Right => {
                self.move_cursor(1);
                LabelOutcome::Replay
            }
            LabelKey::Ambiguous => {
                self.toggle_ambiguous();
                LabelOutcome::Continue
            }
            LabelKey::Accept => self.accept(),
        }
    }

    fn move_cursor(&mut self, delta: i32) {
        let n = self.side.candidates().len() as i32;
        let new = (self.cursor as i32 + delta).clamp(0, n - 1);
        self.cursor = new as usize;
    }

    fn toggle_ambiguous(&mut self) {
        match &mut self.mode {
            Mode::Normal => {
                let mut selected = vec![false; self.side.candidates().len()];
                selected[self.cursor] = true;
                self.mode = Mode::Ambiguous { selected };
            }
            Mode::Ambiguous { selected } => {
                selected[self.cursor] = !selected[self.cursor];
            }
        }
    }

    fn accept(&mut self) -> LabelOutcome {
        let candidates = self.side.candidates();
        let best = candidates[self.cursor];
        // Leaving ambiguous mode is unconditional: accepting always commits
        // whatever was toggled and returns to plain candidate browsing,
        // whether or not `a` was ever pressed for this side.
        let ok = match std::mem::replace(&mut self.mode, Mode::Normal) {
            Mode::Normal => Vec::new(),
            Mode::Ambiguous { selected } => candidates
                .iter()
                .zip(selected)
                .filter(|(_, sel)| *sel)
                .map(|(&v, _)| v)
                .filter(|&v| v != best)
                .collect(),
        };
        let choice = LabelChoice { best, ok };
        match self.side {
            Side::Intro => {
                self.intro_choice = Some(choice);
                self.side = Side::Outro;
                self.cursor = self.outro_start_index;
                LabelOutcome::Replay
            }
            Side::Outro => {
                let intro = self
                    .intro_choice
                    .clone()
                    .expect("intro is always accepted before outro is reachable");
                LabelOutcome::Done {
                    intro,
                    outro: choice,
                }
            }
        }
    }
}

fn nearest_index(candidates: &[u32], value: u32) -> usize {
    let mut best_idx = 0;
    let mut best_diff = u32::MAX;
    for (i, &c) in candidates.iter().enumerate() {
        let diff = c.abs_diff(value);
        if diff < best_diff {
            best_diff = diff;
            best_idx = i;
        }
    }
    best_idx
}

// ---------------------------------------------------------------------
// Click-track clip synthesis.
// ---------------------------------------------------------------------

/// Bar length in frames for `side`'s measured BPM (intro/outro tempo is
/// tracked separately since Funkot mid-song tempo is unreliable).
pub fn bar_frames_for(analysis: &TrackAnalysis, side: Side) -> f64 {
    let bpm = match side {
        Side::Intro => analysis.intro_bpm,
        Side::Outro => analysis.outro_bpm,
    };
    f64::from(analysis.sample_rate) * 60.0 / bpm * f64::from(BEATS_PER_BAR)
}

/// Frame index (may be negative, or past EOF, for extreme candidates) of
/// the boundary a given candidate bar count implies: intro candidates count
/// forward from `first_downbeat`, outro candidates count backward from the
/// file end (matching `outro_structure_bars`, not `outro_bars`).
///
/// *Nominal* — this is bar arithmetic on the analysis markers, which is only
/// as good as those markers' own phase. [`lock_boundary_to_groove`] moves it
/// onto the beat the music in the preview window actually plays before any
/// clicks are synthesized; see there for why that is not optional.
///
/// The outro side does not simply subtract from `total_frames`: "the file
/// ends on a bar boundary" only holds to whole bars, not samples. Measured
/// across `testdata/`, `((total_frames - first_downbeat) / bar_frames) mod
/// 1` is spread across almost the entire ±0.5-bar range (worst case
/// `Nicho - … - 06 Love & Joy.flac` at +0.4948 bar), so counting candidates
/// back from `total_frames` verbatim puts every one of them a bar or more
/// off the phrase grid on those tracks. Instead the outro side rounds
/// `(total_frames - first_downbeat) / bar_frames` to the nearest *integer*
/// bar count and re-derives the file-end anchor from `first_downbeat` plus
/// that many bars, then counts candidates back from the anchor. This only
/// fixes bar *identity* (which bar a candidate names); it is not the
/// rejected "propagate `first_downbeat + n × intro_bpm beats` to the file
/// end" anchor documented in HANDOFF §9, which failed because tempo
/// estimation error accumulates over hundreds of beats and moved sub-beat
/// phase by up to half a beat on `Boom Boom Pow`. Rounding to the nearest
/// *integer bar* is far more forgiving: HANDOFF's own measurements put that
/// accumulated error at well under half a beat (= 0.125 bar), so it never
/// flips which integer bar is nearest. Sub-beat phase is left entirely to
/// [`lock_boundary_to_groove`], exactly as before.
pub fn boundary_frame(analysis: &TrackAnalysis, side: Side, bars: u32) -> i64 {
    let bar_frames = bar_frames_for(analysis, side);
    let span = (bar_frames * f64::from(bars)).round() as i64;
    match side {
        Side::Intro => analysis.first_downbeat as i64 + span,
        Side::Outro => outro_anchor_frame(analysis, bar_frames) - span,
    }
}

/// The outro-side file-end anchor `boundary_frame` counts candidates back
/// from: `first_downbeat` plus the nearest whole number of outro bars to
/// `total_frames`. Falls back to `total_frames` verbatim (the old
/// behaviour) if the markers needed to propagate the grid are unusable.
fn outro_anchor_frame(analysis: &TrackAnalysis, bar_frames: f64) -> i64 {
    let total_frames = analysis.total_frames as f64;
    let first_downbeat = analysis.first_downbeat as f64;
    if !bar_frames.is_finite()
        || bar_frames <= 1.0
        || !total_frames.is_finite()
        || !first_downbeat.is_finite()
        || total_frames < first_downbeat
    {
        return analysis.total_frames as i64;
    }
    let bars_to_end = ((total_frames - first_downbeat) / bar_frames).round();
    (first_downbeat + bars_to_end * bar_frames).round() as i64
}

/// Move a nominal boundary by less than half a beat, onto the beat grid the
/// music inside the listening window actually plays.
///
/// Neither analysis marker is accurate enough to place a click track on its
/// own:
///
/// - The outro side counts back from `total_frames`, on the analyzer's
///   "production ends on a bar boundary" convention. That holds to within a
///   bar, not within a sample: measured across the real masters in
///   `testdata/`, `(total_frames - first_downbeat) mod beat` is spread over
///   the whole ±0.5 beat range. For `AntonFer - … - 02 IVY.flac` it is
///   +0.494 beat — every click in the outro preview landed on the off-beat,
///   which is the bug this exists to fix.
/// - The intro side counts forward from `first_downbeat`, which is refined to
///   a kick but still measured tens of bars away from the window being
///   played, and is off by ~0.2 beat on some masters.
///
/// Two anchors that were tried and rejected, both measured on real audio:
/// snapping the file end onto `first_downbeat + n × intro_bpm beats` breaks
/// tracks whose end is already right (the tempo estimate's error accumulates
/// to a full half beat over `Nicho - … - 04 Boom Boom Pow.flac`), and locking
/// to the last few bars of the file breaks any master that fades into
/// silence (`DimsR - … - 10 Shirube.flac`, `AntonFer - … - 09 Sakura
/// Photograph.flac` — their final bars carry no onsets at all). The window
/// the user is about to hear is the only evidence that is both local and
/// guaranteed to contain the music being labeled.
///
/// The shift is always under half a beat, so bar identity — which bar the
/// candidate names — comes from the analysis markers exactly as before. This
/// is not the ±1/±2-beat coarse alignment HANDOFF §3/§8 rules out.
pub fn lock_boundary_to_groove(
    buffer: &AudioBuffer,
    nominal: i64,
    bar_frames: f64,
    half_width_bars: u32,
) -> i64 {
    let beat_frames = bar_frames / f64::from(BEATS_PER_BAR);
    let clip_start = nominal - (bar_frames * f64::from(half_width_bars)).round() as i64;
    if clip_start < 0 || !(beat_frames.is_finite() && beat_frames > 1.0) {
        return nominal;
    }
    let locked = funkot_core::analysis::lock_beat_phase(
        &buffer.samples,
        buffer.sample_rate,
        clip_start as u64,
        beat_frames,
        2 * half_width_bars,
    );
    nominal + (locked as i64 - clip_start)
}

/// Click loudness and ducking knobs for [`build_candidate_clip`]. Real
/// Funkot masters run near 0 dBFS peak / -10 dBFS RMS with dense high-hats
/// in the same band the click used to occupy, so a fixed absolute click
/// amplitude either got lost in the mix or (turned up) clipped. Two things
/// fix that: size the click off the clip's own RMS instead of an absolute
/// number, and duck the music under it like a DJ cue click.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ClickOptions {
    /// Normal-click peak level, in dB above the clip's own RMS loudness.
    /// The boundary click is louder still (see [`BOUNDARY_AMP_FIRST_SCALE`]);
    /// this only sets the base.
    pub click_db_above_rms: f32,
    /// How many dB to duck the underlying music under each click
    /// (magnitude: 15.0 means the music drops by 15 dB, not rises).
    /// The boundary click ducks [`BOUNDARY_EXTRA_DUCK_DB`] deeper and for
    /// longer, so it reads as structurally different, not just lower-pitched.
    pub duck_db: f32,
}

impl Default for ClickOptions {
    fn default() -> Self {
        Self {
            click_db_above_rms: 12.0,
            duck_db: 18.0,
        }
    }
}

/// Fallback absolute click amplitude used only when the clip is fully
/// silent (RMS 0 — e.g. a candidate window that lands entirely off the end
/// of the file), so the click marking the position stays audible instead of
/// scaling to nothing.
const SILENT_CLIP_FALLBACK_AMP: f32 = 0.5;

const NORMAL_CLICK_FREQ: f32 = 2200.0;
const NORMAL_CLICK_DUR_SECS: f64 = 0.020;
const BOUNDARY_CLICK_FREQ: f32 = 900.0;
const BOUNDARY_CLICK_DUR_SECS: f64 = 0.035;
const BOUNDARY_SECOND_HIT_OFFSET_SECS: f64 = 0.045;
/// Boundary click peak, relative to the normal click's base amplitude.
const BOUNDARY_AMP_FIRST_SCALE: f32 = 1.5;
/// Boundary click's second hit, relative to the normal click's base
/// amplitude (quieter than the first hit, same as before this change).
const BOUNDARY_AMP_SECOND_SCALE: f32 = 1.2;
/// Extra ducking depth (added to `duck_db`) for the boundary bar, on top of
/// the longer hold that naturally follows from covering the double-hit.
const BOUNDARY_EXTRA_DUCK_DB: f32 = 6.0;

/// Duck envelope attack/release slope lengths. Short enough not to blunt
/// the click's transient, long enough that the gain change doesn't itself
/// click (a hard step would add its own audible pop).
const DUCK_ATTACK_SECS: f64 = 0.005;
const DUCK_RELEASE_SECS: f64 = 0.035;
/// Hold covers the click tone's own decay plus a small margin.
const NORMAL_DUCK_HOLD_SECS: f64 = NORMAL_CLICK_DUR_SECS + 0.010;
/// Boundary hold covers both hits of the double-tap plus a small margin.
const BOUNDARY_DUCK_HOLD_SECS: f64 =
    BOUNDARY_SECOND_HIT_OFFSET_SECS + BOUNDARY_CLICK_DUR_SECS + 0.010;

/// Build the interleaved-stereo listening clip for one candidate: the
/// original audio from `half_width_bars` before to `half_width_bars` after
/// the boundary, with a click synthesized on every bar head in that range
/// and a distinct (lower, longer, double-hit, deeper-ducked) click on the
/// boundary bar itself, so the candidate is unmistakable by ear even over
/// dense, loud source material.
pub fn build_candidate_clip(
    buffer: &AudioBuffer,
    analysis: &TrackAnalysis,
    side: Side,
    bars: u32,
    half_width_bars: u32,
    click_opts: &ClickOptions,
) -> Vec<f32> {
    let bar_frames = bar_frames_for(analysis, side);
    let boundary = locked_boundary_frame(buffer, analysis, side, bars, half_width_bars);
    render_click_clip(buffer, boundary, bar_frames, half_width_bars, click_opts)
}

/// The boundary [`build_candidate_clip`] actually centres its window and
/// click grid on: [`boundary_frame`] moved onto the window's own beat grid by
/// [`lock_boundary_to_groove`]. Callers that need to relate clip frames back
/// to source frames must use this, not the nominal value.
pub fn locked_boundary_frame(
    buffer: &AudioBuffer,
    analysis: &TrackAnalysis,
    side: Side,
    bars: u32,
    half_width_bars: u32,
) -> i64 {
    let bar_frames = bar_frames_for(analysis, side);
    let nominal = boundary_frame(analysis, side, bars);
    lock_boundary_to_groove(buffer, nominal, bar_frames, half_width_bars)
}

fn render_click_clip(
    buffer: &AudioBuffer,
    boundary_frame: i64,
    bar_frames: f64,
    half_width_bars: u32,
    click_opts: &ClickOptions,
) -> Vec<f32> {
    let total_bars = 2 * half_width_bars;
    let clip_frames = (bar_frames * f64::from(total_bars)).round().max(0.0) as i64;
    let clip_start = boundary_frame - (bar_frames * f64::from(half_width_bars)).round() as i64;

    // Copy source audio where the window overlaps the file; out-of-range
    // parts (window starts before frame 0, or runs past EOF) stay silent —
    // clicks are synthesized independently and still land correctly.
    let mut out = vec![0.0f32; clip_frames as usize * 2];
    for i in 0..clip_frames {
        let src_frame = clip_start + i;
        if src_frame >= 0 && (src_frame as u64) < buffer.frames {
            let src_idx = src_frame as usize * 2;
            out[i as usize * 2] = buffer.samples[src_idx];
            out[i as usize * 2 + 1] = buffer.samples[src_idx + 1];
        }
    }

    let sr = f64::from(buffer.sample_rate);
    let base_amp = click_base_amplitude(rms(&out), click_opts.click_db_above_rms);
    for k in 0..total_bars {
        let offset = (bar_frames * f64::from(k)).round() as usize;
        let kind = if k == half_width_bars {
            ClickKind::Boundary
        } else {
            ClickKind::Normal
        };
        add_click(&mut out, offset, sr, kind, base_amp, click_opts.duck_db);
    }
    out
}

/// Root-mean-square amplitude of an interleaved-stereo buffer (both
/// channels pooled), used to size the click relative to how loud the clip
/// itself is rather than to a fixed absolute number.
fn rms(samples: &[f32]) -> f32 {
    if samples.is_empty() {
        return 0.0;
    }
    let sum_sq: f64 = samples.iter().map(|&s| f64::from(s) * f64::from(s)).sum();
    (sum_sq / samples.len() as f64).sqrt() as f32
}

fn click_base_amplitude(clip_rms: f32, click_db_above_rms: f32) -> f32 {
    if clip_rms <= 0.0 {
        return SILENT_CLIP_FALLBACK_AMP;
    }
    clip_rms * 10f32.powf(click_db_above_rms / 20.0)
}

#[derive(Clone, Copy)]
enum ClickKind {
    Normal,
    Boundary,
}

/// Overlay a bar-head click on `out` (interleaved stereo) at frame `start`,
/// ducking the underlying music first so the click cuts through dense
/// source material instead of being masked by it.
fn add_click(out: &mut [f32], start: usize, sr: f64, kind: ClickKind, base_amp: f32, duck_db: f32) {
    match kind {
        ClickKind::Normal => {
            duck_music(out, start, sr, NORMAL_DUCK_HOLD_SECS, duck_db);
            let amp = safe_click_amplitude(out, start, NORMAL_CLICK_DUR_SECS, sr, base_amp);
            add_click_tone(out, start, sr, NORMAL_CLICK_FREQ, NORMAL_CLICK_DUR_SECS, amp);
        }
        ClickKind::Boundary => {
            duck_music(
                out,
                start,
                sr,
                BOUNDARY_DUCK_HOLD_SECS,
                duck_db + BOUNDARY_EXTRA_DUCK_DB,
            );
            let amp1 = safe_click_amplitude(
                out,
                start,
                BOUNDARY_CLICK_DUR_SECS,
                sr,
                base_amp * BOUNDARY_AMP_FIRST_SCALE,
            );
            add_click_tone(out, start, sr, BOUNDARY_CLICK_FREQ, BOUNDARY_CLICK_DUR_SECS, amp1);
            let second = start + (BOUNDARY_SECOND_HIT_OFFSET_SECS * sr).round() as usize;
            let amp2 = safe_click_amplitude(
                out,
                second,
                BOUNDARY_CLICK_DUR_SECS,
                sr,
                base_amp * BOUNDARY_AMP_SECOND_SCALE,
            );
            add_click_tone(out, second, sr, BOUNDARY_CLICK_FREQ, BOUNDARY_CLICK_DUR_SECS, amp2);
        }
    }
}

/// Peak of `out` (both channels) within `len` frames starting at `start`,
/// used to see how much headroom a click actually has *after* ducking —
/// measured, not assumed, so a quiet passage can get a louder click than a
/// pessimistic worst-case bound would allow.
fn local_peak(out: &[f32], start: i64, len: i64) -> f32 {
    let frames = (out.len() / 2) as i64;
    let lo = start.max(0);
    let hi = (start + len).min(frames);
    let mut peak = 0.0f32;
    for f in lo..hi {
        let idx = f as usize * 2;
        peak = peak.max(out[idx].abs()).max(out[idx + 1].abs());
    }
    peak
}

/// Highest instantaneous sample magnitude a click + its already-ducked
/// residual are allowed to sum to. Slightly below 1.0 for float rounding
/// margin, not because samples at exactly 1.0 are a problem.
const CLIP_SAFETY_CEILING: f32 = 0.97;

/// Clamp `target` amplitude to what's actually safe given the ducked
/// residual under this specific click, instead of assuming the worst case
/// (source peaking at 1.0 right under every click). Funkot masters run hot,
/// but ducking already pulls the local residual down to a fraction of that,
/// so this recovers headroom the flat/absolute old design left unused.
fn safe_click_amplitude(out: &[f32], start: usize, dur_secs: f64, sr: f64, target: f32) -> f32 {
    let len = (dur_secs * sr).round().max(1.0) as i64;
    let residual = local_peak(out, start as i64, len);
    let headroom = (CLIP_SAFETY_CEILING - residual).max(0.0);
    target.min(headroom)
}

/// Sidechain-style gain dip: ramps the music down over `DUCK_ATTACK_SECS`,
/// holds it down for `hold_secs` (covering the click tone(s) that land on
/// top), then ramps back up over `DUCK_RELEASE_SECS`. Slopes avoid the
/// ducking itself adding an audible step.
fn duck_music(out: &mut [f32], start: usize, sr: f64, hold_secs: f64, depth_db: f32) {
    let frames = (out.len() / 2) as i64;
    if frames == 0 {
        return;
    }
    let start = start as i64;
    let duck_gain = 10f32.powf(-depth_db / 20.0);
    let attack_frames = (DUCK_ATTACK_SECS * sr).round().max(1.0) as i64;
    let hold_frames = (hold_secs * sr).round().max(0.0) as i64;
    let release_frames = (DUCK_RELEASE_SECS * sr).round().max(1.0) as i64;
    let window_start = start - attack_frames;
    let window_end = start + hold_frames + release_frames;

    for f in window_start..window_end {
        if f < 0 || f >= frames {
            continue;
        }
        let gain = if f < start {
            let t = (f - window_start) as f32 / attack_frames as f32;
            1.0 + (duck_gain - 1.0) * t
        } else if f < start + hold_frames {
            duck_gain
        } else {
            let t = (f - (start + hold_frames)) as f32 / release_frames as f32;
            duck_gain + (1.0 - duck_gain) * t
        };
        let idx = f as usize * 2;
        out[idx] *= gain;
        out[idx + 1] *= gain;
    }
}

fn add_click_tone(out: &mut [f32], start: usize, sr: f64, freq: f32, dur_secs: f64, amp: f32) {
    let frames = out.len() / 2;
    if start >= frames {
        return;
    }
    let len = (dur_secs * sr).round().max(1.0) as usize;
    let end = (start + len).min(frames);
    let tau = (dur_secs * 0.3).max(1e-6);
    for (i, frame) in (start..end).enumerate() {
        let t = i as f64 / sr;
        let env = (-t / tau).exp() as f32;
        let s = amp * env * (2.0 * PI * freq * t as f32).sin();
        out[frame * 2] += s;
        out[frame * 2 + 1] += s;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_analysis(
        sample_rate: u32,
        intro_bpm: f64,
        outro_bpm: f64,
        first_downbeat: u64,
        total_frames: u64,
    ) -> TrackAnalysis {
        TrackAnalysis {
            version: funkot_core::cache::CACHE_VERSION,
            file_name: "t.wav".to_string(),
            sample_rate,
            total_frames,
            intro_bpm,
            outro_bpm,
            first_downbeat,
            outro_start: 0,
            intro_bars: 16,
            outro_bars: 16,
            outro_structure_bars: 16,
            bars_estimated_low_confidence: false,
            intro_bars_low_confidence: false,
            outro_bars_low_confidence: false,
            intro_bars_manual: false,
            outro_bars_manual: false,
            needs_reanalysis: false,
            rms_dbfs: -14.0,
            gain_db: 0.0,
        }
    }

    // --- cursor navigation -------------------------------------------------

    #[test]
    fn cursor_clamps_at_both_ends() {
        let mut s = TrackSession::new(8, 8); // nearest to 8 is index 0
        assert_eq!(s.current_bars(), 8);
        assert_eq!(s.apply_key(LabelKey::Left), LabelOutcome::Replay);
        assert_eq!(s.current_bars(), 8, "must not go below the first candidate");

        for _ in 0..INTRO_CANDIDATES.len() + 3 {
            s.apply_key(LabelKey::Right);
        }
        assert_eq!(
            s.current_bars(),
            *INTRO_CANDIDATES.last().unwrap(),
            "must not go past the last candidate"
        );
    }

    #[test]
    fn toggle_width_switches_between_normal_and_wide() {
        let mut s = TrackSession::new(8, 8);
        assert_eq!(s.context_half_width_bars(), NORMAL_HALF_WIDTH_BARS);
        s.apply_key(LabelKey::ToggleWidth);
        assert_eq!(s.context_half_width_bars(), WIDE_HALF_WIDTH_BARS);
        s.apply_key(LabelKey::ToggleWidth);
        assert_eq!(s.context_half_width_bars(), NORMAL_HALF_WIDTH_BARS);
    }

    // --- ambiguous acceptable-set toggle ------------------------------------

    #[test]
    fn ambiguous_toggle_builds_ok_set_excluding_best() {
        let mut s = TrackSession::new(32, 8); // intro cursor starts on 32
        assert_eq!(s.current_bars(), 32);

        s.apply_key(LabelKey::Ambiguous); // enter ambiguous mode, selects 32
        assert_eq!(s.ambiguous_selected(), Some(vec![32]));

        s.apply_key(LabelKey::Right); // cursor -> 48
        s.apply_key(LabelKey::Ambiguous); // toggle 48 on
        assert_eq!(s.ambiguous_selected(), Some(vec![32, 48]));

        s.apply_key(LabelKey::Left); // cursor -> 32
        s.apply_key(LabelKey::Ambiguous); // toggle 32 back off
        assert_eq!(s.ambiguous_selected(), Some(vec![48]));

        match s.apply_key(LabelKey::Accept) {
            LabelOutcome::Replay => {}
            other => panic!("expected Replay (advancing to outro), got {other:?}"),
        }
        // Confirmed choice: best is wherever the cursor sat at Accept (32),
        // ok is the toggled set minus best.
        assert_eq!(s.current_side(), Side::Outro);
        assert_eq!(
            s.current_bars(),
            8,
            "outro cursor should reset to its own initial estimate"
        );
    }

    // --- skip / quit / done branches -----------------------------------

    #[test]
    fn skip_and_quit_are_immediate() {
        let mut skip = TrackSession::new(8, 8);
        assert_eq!(skip.apply_key(LabelKey::Skip), LabelOutcome::Skip);

        let mut quit = TrackSession::new(8, 8);
        assert_eq!(quit.apply_key(LabelKey::Quit), LabelOutcome::Quit);
    }

    #[test]
    fn note_key_sets_note_text() {
        let mut s = TrackSession::new(8, 8);
        assert_eq!(
            s.apply_key(LabelKey::Note("clean drop".to_string())),
            LabelOutcome::Continue
        );
        assert_eq!(s.note(), "clean drop");
    }

    #[test]
    fn accepting_both_sides_yields_done_with_choices() {
        let mut s = TrackSession::new(32, 16);
        match s.apply_key(LabelKey::Accept) {
            LabelOutcome::Replay => {}
            other => panic!("expected Replay after intro accept, got {other:?}"),
        }
        assert_eq!(s.current_side(), Side::Outro);
        assert_eq!(s.current_bars(), 16);

        match s.apply_key(LabelKey::Accept) {
            LabelOutcome::Done { intro, outro } => {
                assert_eq!(
                    intro,
                    LabelChoice {
                        best: 32,
                        ok: vec![]
                    }
                );
                assert_eq!(
                    outro,
                    LabelChoice {
                        best: 16,
                        ok: vec![]
                    }
                );
            }
            other => panic!("expected Done, got {other:?}"),
        }
    }

    // --- boundary / bar-frame math ---------------------------------------

    #[test]
    fn outro_boundary_counts_back_from_total_frames() {
        let analysis = sample_analysis(180, 180.0, 180.0, 0, 2400);
        let bar_frames = bar_frames_for(&analysis, Side::Outro);
        assert!((bar_frames - 240.0).abs() < 1e-9);
        assert_eq!(boundary_frame(&analysis, Side::Outro, 2), 2400 - 480);
    }

    #[test]
    fn intro_boundary_counts_forward_from_first_downbeat() {
        let analysis = sample_analysis(180, 180.0, 180.0, 100, 0);
        assert_eq!(boundary_frame(&analysis, Side::Intro, 2), 100 + 480);
    }

    /// Regression: when `total_frames` already sits exactly on the bar grid
    /// propagated from `first_downbeat`, the anchor rounding must reproduce
    /// the same result as counting back from `total_frames` directly (a
    /// nonzero `first_downbeat` this time, unlike the test above).
    #[test]
    fn outro_boundary_unchanged_when_file_end_is_already_on_grid() {
        // bar_frames = 240; first_downbeat + 12 bars = 100 + 2880 = 2980,
        // exactly total_frames -> no correction should apply.
        let analysis = sample_analysis(180, 180.0, 180.0, 100, 2980);
        assert_eq!(boundary_frame(&analysis, Side::Outro, 2), 2980 - 480);
    }

    /// The bug this exists to fix: `total_frames` sits two bars *and* a
    /// sub-bar residual (90 of 240 frames, well under the half-bar/half-beat
    /// range `lock_boundary_to_groove` is responsible for) past the true
    /// phrase grid. The anchor must snap to the nearest whole bar from
    /// `first_downbeat`, discarding the residual, not propagate it.
    #[test]
    fn outro_boundary_snaps_to_bar_grid_when_file_end_is_off_grid() {
        let first_downbeat = 100u64;
        let bar_frames = 240u64; // sr=180, bpm=180
        let true_grid_end = first_downbeat + 12 * bar_frames; // 2980
        let total_frames = true_grid_end + 2 * bar_frames + 90; // 2 bars + noise
        let analysis = sample_analysis(180, 180.0, 180.0, first_downbeat, total_frames);

        // New anchor is first_downbeat + 14 bars (the extra 2 bars survive,
        // the 90-frame residual does not): 100 + 14*240 = 3460.
        let candidate = boundary_frame(&analysis, Side::Outro, 2);
        assert_eq!(candidate, first_downbeat as i64 + 12 * bar_frames as i64);

        // The naive "subtract straight from total_frames" formula this
        // replaces would have kept the 90-frame residual.
        let naive = total_frames as i64 - 2 * bar_frames as i64;
        assert_ne!(candidate, naive);
        assert_eq!(naive - candidate, 90);
    }

    /// Fallback: `total_frames < first_downbeat` is nonsensical input the
    /// anchor propagation can't use, so it must fall back to the old
    /// "subtract from total_frames" formula instead of producing garbage.
    #[test]
    fn outro_boundary_falls_back_when_total_frames_precedes_first_downbeat() {
        let analysis = sample_analysis(180, 180.0, 180.0, 100, 50);
        let bar_frames = bar_frames_for(&analysis, Side::Outro);
        assert!((bar_frames - 240.0).abs() < 1e-9);
        assert_eq!(boundary_frame(&analysis, Side::Outro, 1), 50 - 240);
    }

    /// Fallback: a degenerate (near-zero) `bar_frames`, e.g. from a bogus
    /// BPM, must not be used to propagate a grid; fall back instead of
    /// dividing into a meaningless bar count.
    #[test]
    fn outro_boundary_falls_back_when_bar_frames_is_degenerate() {
        // outro_bpm absurdly high -> bar_frames_for(..) << 1.0.
        let analysis = sample_analysis(180, 180.0, 1_000_000_000.0, 100, 2980);
        let bar_frames = bar_frames_for(&analysis, Side::Outro);
        assert!(bar_frames <= 1.0);
        let span = (bar_frames * 2.0).round() as i64;
        assert_eq!(
            boundary_frame(&analysis, Side::Outro, 2),
            2980 - span
        );
    }

    // --- click clip: length and bar-head placement ------------------------

    #[test]
    fn click_clip_has_expected_length_and_bar_head_clicks() {
        // sr=180, bpm=180 -> bar_frames = 240 (nice round numbers for testing).
        let sr = 180u32;
        let buffer = AudioBuffer {
            sample_rate: sr,
            frames: 0,
            samples: Vec::new(),
        };
        let analysis = sample_analysis(sr, 180.0, 180.0, 0, 0);
        let half_width = 2u32;
        let bars = 2u32; // boundary coincides with half_width for a clean check
        let clip = build_candidate_clip(
            &buffer,
            &analysis,
            Side::Intro,
            bars,
            half_width,
            &ClickOptions::default(),
        );

        let bar_frames = 240usize;
        let total_bars = (2 * half_width) as usize;
        assert_eq!(clip.len(), total_bars * bar_frames * 2);

        let duration = (clip.len() / 2) as f64 / f64::from(sr);
        assert!(
            (duration - (total_bars * bar_frames) as f64 / f64::from(sr)).abs() < 1e-9,
            "duration {duration}"
        );

        // A window is "active" if any sample within a click's worth of
        // frames past the offset is non-zero (envelope starts at 0 at t=0).
        let is_active = |offset: usize| {
            let hi = (offset + 15).min(clip.len() / 2);
            (offset..hi).any(|f| clip[f * 2] != 0.0)
        };
        for k in 0..total_bars {
            assert!(is_active(k * bar_frames), "expected a click at bar head {k}");
        }
        // Halfway between two bar heads: no source audio, no click -> silent.
        assert!(
            !is_active(bar_frames / 2),
            "expected silence between bar heads"
        );
    }

    // --- click audibility over dense/loud material --------------------------
    //
    // Real Funkot masters run near 0 dBFS peak / -10 dBFS RMS with dense
    // high-hats occupying the same band the click used to. The synthesized
    // clip fixtures above are too thin to expose that — the click was never
    // actually competing with anything. `synth_dense_loud_stereo` stands in
    // for a hot, busy master so a masking regression shows up here instead
    // of only on a user's real playlist.

    /// Deterministic xorshift32 PRNG (no new crate dependency for test noise).
    fn xorshift32(state: &mut u32) -> f32 {
        *state ^= *state << 13;
        *state ^= *state >> 17;
        *state ^= *state << 5;
        // [-1, 1)
        (*state as f32 / u32::MAX as f32) * 2.0 - 1.0
    }

    /// Broadband noise floor (~-14 dBFS-ish) plus a dense 16th-note "hat"
    /// layer of short noise bursts reaching near full scale — deliberately
    /// unrelated to the bar grid, so some hits land right on top of a click,
    /// the worst case for masking.
    fn synth_dense_loud_stereo(frames: usize, sr: u32) -> Vec<f32> {
        let mut state = 0x1234_5678u32;
        let sr_f = f64::from(sr);
        let hat_period_secs = 0.0625; // ~16th notes at a Funkot-ish tempo
        let hat_decay_secs = 0.006;
        let mut out = vec![0.0f32; frames * 2];
        for i in 0..frames {
            let t = i as f64 / sr_f;
            let base = xorshift32(&mut state) * 0.2;
            let phase = t % hat_period_secs;
            let hat_env = (-(phase / hat_decay_secs)) as f32;
            let hat_env = hat_env.exp();
            let hat = xorshift32(&mut state) * hat_env * 0.9;
            let s = (base + hat).clamp(-1.0, 1.0);
            out[i * 2] = s;
            out[i * 2 + 1] = s;
        }
        out
    }

    /// Peak |sample| (both channels) of an interleaved-stereo buffer over
    /// frames `[lo, hi)`.
    fn peak_in(samples: &[f32], lo: usize, hi: usize) -> f32 {
        samples[lo * 2..hi * 2]
            .iter()
            .fold(0.0f32, |acc, &s| acc.max(s.abs()))
    }

    /// Peak |a - b| (both channels) over `[lo, hi)`. Used to isolate a
    /// click's own contribution from a render pair that share the same
    /// ducked residual: `full - music_only` cancels the residual and leaves
    /// just the click tone(s).
    fn diff_peak_in(a: &[f32], b: &[f32], lo: usize, hi: usize) -> f32 {
        a[lo * 2..hi * 2]
            .iter()
            .zip(&b[lo * 2..hi * 2])
            .fold(0.0f32, |acc, (&x, &y)| acc.max((x - y).abs()))
    }

    /// A ±200 ms-median-vs-onset-peak ratio (an earlier version of this
    /// test used exactly that) looks worse the louder/denser the
    /// surrounding music is, *by construction*, regardless of whether the
    /// click is actually audible: real Funkot masters are loud almost
    /// everywhere, so a wide window mostly samples unrelated transients
    /// that have nothing to do with this particular click, and a short
    /// (60-80 ms) sidechain duck can't move a 400 ms-wide median at all —
    /// that combination put a same low ceiling on the metric no matter how
    /// the click/duck were tuned. It also can't separate "the click is
    /// loud" from "the music happened to be loud right here," so it can't
    /// actually confirm the duck fired at the right time.
    ///
    /// This test instead renders the same clip twice — once normally, once
    /// with the click amplitude driven to ~0 (`click_db_above_rms` very
    /// negative) so only the ducked residual remains — and diffs them.
    /// That directly measures, per bar head:
    ///   (a) how many dB the residual was actually reduced by at the click's
    ///       own onset, compared to the true undecked original, and
    ///   (b) how many dB the click's own peak (full minus music-only, so the
    ///       residual cancels out) clears that residual by.
    /// Neither reading is bounded by how loud the surrounding track is.
    #[test]
    fn clicks_stay_audible_over_dense_loud_material() {
        let sr = 44_100u32;
        let bpm = 180.0;
        let total_secs = 30.0;
        let frames = (f64::from(sr) * total_secs) as usize;
        let source = synth_dense_loud_stereo(frames, sr);
        let buffer = AudioBuffer {
            sample_rate: sr,
            frames: frames as u64,
            samples: source.clone(),
        };
        // first_downbeat 5s in so the ±8-bar intro window stays inside the buffer.
        let analysis = sample_analysis(sr, bpm, bpm, (f64::from(sr) * 5.0) as u64, frames as u64);
        let half_width = NORMAL_HALF_WIDTH_BARS;
        let bars = 8u32; // smallest intro candidate

        let opts = ClickOptions::default();
        let full = build_candidate_clip(&buffer, &analysis, Side::Intro, bars, half_width, &opts);
        // Same duck, click amplitude ~0: isolates the ducked residual alone.
        let music_only_opts = ClickOptions {
            click_db_above_rms: -200.0,
            duck_db: opts.duck_db,
        };
        let music_only = build_candidate_clip(
            &buffer,
            &analysis,
            Side::Intro,
            bars,
            half_width,
            &music_only_opts,
        );

        let bar_frames = bar_frames_for(&analysis, Side::Intro);
        // The clip is centred on the *locked* boundary, not the nominal one,
        // so the "same absolute frames" reference below has to be too.
        let boundary =
            locked_boundary_frame(&buffer, &analysis, Side::Intro, bars, half_width);
        let clip_start = boundary - (bar_frames * f64::from(half_width)).round() as i64;
        let total_bars = (2 * half_width) as usize;

        for k in 0..total_bars {
            let onset = (bar_frames * k as f64).round() as usize;
            let is_boundary = k == half_width as usize;
            let dur_secs = if is_boundary {
                BOUNDARY_CLICK_DUR_SECS
            } else {
                NORMAL_CLICK_DUR_SECS
            };
            let dur_frames = (dur_secs * f64::from(sr)).round().max(1.0) as usize;
            let hi = (onset + dur_frames).min(full.len() / 2);
            assert!(hi > onset, "bar {k}: click window ran off the end of the clip");

            // (a) true "no duck at all" reference: the original synthetic
            // source at the same absolute frames this window covers.
            let src_lo = clip_start + onset as i64;
            let src_hi = src_lo + (hi - onset) as i64;
            assert!(
                src_lo >= 0 && (src_hi as u64) <= frames as u64,
                "bar {k}: fixture must keep every candidate window in-bounds"
            );
            let undamped_peak = peak_in(&source, src_lo as usize, src_hi as usize);
            let ducked_peak = peak_in(&music_only, onset, hi);
            assert!(undamped_peak > 0.0 && ducked_peak > 0.0, "bar {k}: unexpectedly silent");
            let reduction_db = 20.0 * (undamped_peak / ducked_peak).log10();

            let expected_min_reduction = if is_boundary {
                opts.duck_db + BOUNDARY_EXTRA_DUCK_DB
            } else {
                opts.duck_db
            } - 1.0; // float-rounding slack
            assert!(
                reduction_db >= expected_min_reduction,
                "bar {k}: residual only reduced by {reduction_db:.2} dB at click onset \
                 (want >= {expected_min_reduction:.2} dB) — duck isn't engaged in time"
            );

            // (b) the click's own peak vs. the residual it's riding on.
            let click_peak = diff_peak_in(&full, &music_only, onset, hi);
            let margin_db = 20.0 * (click_peak / ducked_peak).log10();
            assert!(
                margin_db >= 12.0,
                "bar {k}: click peak only {margin_db:.2} dB above its ducked residual \
                 (want >= 12 dB)"
            );
        }

        // Source is already near 0 dBFS; ducking must keep the click from
        // pushing the mix over full scale.
        let over = full.iter().filter(|&&s| s.abs() > 1.0).count();
        assert_eq!(over, 0, "clicks over dense material should not clip");
    }

    // --- click phase vs. the music's beat grid --------------------------
    //
    // The clip fixtures above are noise-only: they have no beat at all, so
    // they can confirm *that* a click was synthesized but never *where* it
    // should have been. The real failure they missed: outro candidates are
    // measured back from the file end, and a master whose last sample is
    // not on a beat (IVY: the file ends 0.494 beat past the last beat of the
    // grid) puts every outro click squarely on the off-beat.

    /// Dense, loud, IVY-shaped fixture with a *known* beat grid: a hard
    /// 55 Hz kick on every beat starting at `first_downbeat`, 16th-note hats
    /// with the on-beat hits accented, a clap on beats 2 and 4, and a
    /// near-full-scale broadband bed (the same hot, busy master
    /// [`synth_dense_loud_stereo`] stands in for). Every layer marks the same
    /// grid, as a real Funkot master does — the point of the fixture is the
    /// *file length*, not an adversarial rhythm.
    fn synth_gridded_dense_loud_stereo(
        frames: usize,
        sr: u32,
        first_downbeat: u64,
        beat_frames: f64,
    ) -> Vec<f32> {
        let mut state = 0x9E37_79B9u32;
        let sr_f = f64::from(sr);
        let mut out = vec![0.0f32; frames * 2];

        for i in 0..frames {
            let s = xorshift32(&mut state) * 0.15;
            out[i * 2] = s;
            out[i * 2 + 1] = s;
        }

        // Percussion, all on the same grid: 16th hats (on-beat accented),
        // clap on beats 2 and 4, kick on every beat.
        let add_noise = |out: &mut Vec<f32>, at: f64, amp: f32, decay_secs: f64, st: &mut u32| {
            let len = (decay_secs * 6.0 * sr_f) as usize;
            let s0 = at.round() as usize;
            for j in 0..len {
                let f = s0 + j;
                if f >= frames {
                    break;
                }
                let env = (-(j as f64) / (decay_secs * sr_f)).exp() as f32;
                let v = xorshift32(st) * amp * env;
                out[f * 2] += v;
                out[f * 2 + 1] += v;
            }
        };
        let add_kick = |out: &mut Vec<f32>, at: f64| {
            let len = (0.25 * sr_f) as usize;
            let s0 = at.round() as usize;
            for j in 0..len {
                let f = s0 + j;
                if f >= frames {
                    break;
                }
                let t = j as f32 / sr_f as f32;
                let env = (-(j as f64) / (0.10 * sr_f)).exp() as f32;
                // Cosine phase: a real kick's low-band energy peaks at the
                // attack, not a quarter cycle later.
                out[f * 2] += 0.95 * env * (2.0 * PI * 55.0 * t).cos();
                out[f * 2 + 1] += 0.95 * env * (2.0 * PI * 55.0 * t).cos();
            }
        };

        let mut beat = 0u64;
        loop {
            let at = first_downbeat as f64 + beat_frames * beat as f64;
            if at >= frames as f64 {
                break;
            }
            add_kick(&mut out, at);
            for sixteenth in 0..4 {
                let amp = if sixteenth == 0 { 0.65 } else { 0.30 };
                add_noise(
                    &mut out,
                    at + beat_frames * f64::from(sixteenth) / 4.0,
                    amp,
                    0.006,
                    &mut state,
                );
            }
            if beat % u64::from(BEATS_PER_BAR) % 2 == 1 {
                add_noise(&mut out, at, 0.55, 0.030, &mut state);
            }
            beat += 1;
        }
        for s in out.iter_mut() {
            *s = s.clamp(-1.0, 1.0);
        }
        out
    }

    /// Cascaded one-pole low-pass (interleaved stereo in, mono out), enough
    /// to separate the fixture's 55 Hz kick from its broadband bed/hats.
    fn lowpass2_mono(clip: &[f32], sr: u32, hz: f64) -> Vec<f32> {
        let dt = 1.0 / f64::from(sr);
        let rc = 1.0 / (2.0 * std::f64::consts::PI * hz);
        let a = dt / (rc + dt);
        let mut y1 = 0.0f64;
        let mut y2 = 0.0f64;
        let mut out = Vec::with_capacity(clip.len() / 2);
        for f in clip.chunks_exact(2) {
            let x = 0.5 * (f64::from(f[0]) + f64::from(f[1]));
            y1 += a * (x - y1);
            y2 += a * (y1 - y2);
            out.push(y2 as f32);
        }
        out
    }

    /// Fold the clip's low-band (kick) energy onto one beat period and
    /// return, in beats, how far the energy peak sits from clip frame 0 —
    /// which `render_click_clip` always places a click on. `0` means the
    /// clicks are phase-locked to the music; `±0.5` means they are on the
    /// off-beat.
    fn kick_phase_offset_beats(clip: &[f32], sr: u32, beat_frames: f64) -> f64 {
        const NBINS: usize = 32;
        let low = lowpass2_mono(clip, sr, 120.0);
        let mut sum = [0.0f64; NBINS];
        let mut cnt = [0.0f64; NBINS];
        for (i, &s) in low.iter().enumerate() {
            let b = (((i as f64 / beat_frames) % 1.0) * NBINS as f64) as usize % NBINS;
            sum[b] += f64::from(s).abs();
            cnt[b] += 1.0;
        }
        let prof: Vec<f64> = (0..NBINS).map(|b| sum[b] / cnt[b].max(1.0)).collect();
        let peak = prof
            .iter()
            .enumerate()
            .max_by(|a, b| a.1.partial_cmp(b.1).unwrap())
            .map(|(i, _)| i)
            .unwrap();
        let frac = peak as f64 / NBINS as f64;
        if frac <= 0.5 {
            frac
        } else {
            frac - 1.0
        }
    }

    /// Build the same window three ways: with clicks, with the duck but no
    /// click, and with neither. `full - music_only` isolates the click tones
    /// (the differencing idiom `clicks_stay_audible_over_dense_loud_material`
    /// established); `raw` is the untouched music the clicks must land on.
    fn render_triplet(
        buffer: &AudioBuffer,
        analysis: &TrackAnalysis,
        side: Side,
        bars: u32,
        half_width: u32,
    ) -> (Vec<f32>, Vec<f32>, Vec<f32>) {
        let opts = ClickOptions::default();
        let full = build_candidate_clip(buffer, analysis, side, bars, half_width, &opts);
        let music_only = build_candidate_clip(
            buffer,
            analysis,
            side,
            bars,
            half_width,
            &ClickOptions {
                click_db_above_rms: -200.0,
                duck_db: opts.duck_db,
            },
        );
        let raw = build_candidate_clip(
            buffer,
            analysis,
            side,
            bars,
            half_width,
            &ClickOptions {
                click_db_above_rms: -200.0,
                duck_db: 0.0,
            },
        );
        (full, music_only, raw)
    }

    #[test]
    fn clicks_stay_on_the_beat_grid_when_the_file_ends_mid_beat() {
        let sr = 44_100u32;
        let bpm = 180.0;
        let beat = f64::from(sr) * 60.0 / bpm; // 14700 exactly
        let bar = beat * f64::from(BEATS_PER_BAR);
        let first_downbeat = 7_676u64; // IVY's own value
        let music_beats = 200u64;
        // The whole point of the fixture: the last sample is *half a beat*
        // past the last beat of the grid, the same shape as IVY (measured
        // (total_frames - first_downbeat) mod beat = 0.494 beat). A grid
        // counted back from `total_frames` therefore lands on the off-beat.
        let total = first_downbeat
            + (beat * music_beats as f64).round() as u64
            + (beat * 0.5).round() as u64;
        let frames = total as usize;
        let samples = synth_gridded_dense_loud_stereo(frames, sr, first_downbeat, beat);
        let buffer = AudioBuffer {
            sample_rate: sr,
            frames: total,
            samples,
        };
        let analysis = sample_analysis(sr, bpm, bpm, first_downbeat, total);
        let half_width = NORMAL_HALF_WIDTH_BARS;

        for (side, bars) in [(Side::Outro, 8u32), (Side::Outro, 16), (Side::Intro, 8)] {
            let (full, music_only, raw) =
                render_triplet(&buffer, &analysis, side, bars, half_width);

            // The click grid itself: clicks must sit on multiples of one bar
            // from clip frame 0 (this is what the phase measurement below is
            // measured against).
            let total_bars = (2 * half_width) as usize;
            for k in 0..total_bars {
                let onset = (bar * k as f64).round() as usize;
                let hi = (onset + (0.02 * f64::from(sr)) as usize).min(full.len() / 2);
                let click_peak = diff_peak_in(&full, &music_only, onset, hi);
                assert!(
                    click_peak > 1e-4,
                    "{:?} {bars}bars: no click at bar head {k}",
                    side
                );
            }

            let offset = kick_phase_offset_beats(&raw, sr, beat);
            assert!(
                offset.abs() <= 0.06,
                "{:?} {bars}bars: the music's kicks sit {offset:+.3} beat away from the \
                 click grid (want |offset| <= 0.06 beat). The clicks are on the off-beat.",
                side
            );
        }
    }

    #[test]
    fn boundary_ducks_deeper_and_longer_than_normal() {
        let sr = 44_100.0;
        let frames = 2_000usize;
        let duck_db = ClickOptions::default().duck_db;

        let mut normal_buf = vec![1.0f32; frames * 2];
        duck_music(&mut normal_buf, 500, sr, NORMAL_DUCK_HOLD_SECS, duck_db);
        let normal_min = normal_buf.iter().cloned().fold(f32::INFINITY, f32::min);

        let mut boundary_buf = vec![1.0f32; frames * 2];
        duck_music(
            &mut boundary_buf,
            500,
            sr,
            BOUNDARY_DUCK_HOLD_SECS,
            duck_db + BOUNDARY_EXTRA_DUCK_DB,
        );
        let boundary_min = boundary_buf.iter().cloned().fold(f32::INFINITY, f32::min);

        assert!(
            boundary_min < normal_min,
            "boundary duck should reach a deeper gain floor: \
             normal_min={normal_min} boundary_min={boundary_min}"
        );

        let count_at_floor = |buf: &[f32], floor: f32| {
            buf.iter().filter(|&&s| (s - floor).abs() < 1e-4).count()
        };
        let normal_floor_count = count_at_floor(&normal_buf, normal_min);
        let boundary_floor_count = count_at_floor(&boundary_buf, boundary_min);
        assert!(
            boundary_floor_count > normal_floor_count,
            "boundary hold should stay at the floor longer than normal's: \
             normal={normal_floor_count} boundary={boundary_floor_count}"
        );
    }
}
