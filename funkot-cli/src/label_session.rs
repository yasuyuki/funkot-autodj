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
    /// Skip this track: no label side is confirmed here. The caller is
    /// still responsible for persisting a note typed before `s`, if any --
    /// see [`TrackSession::note`] and [`TrackSession::intro_choice`].
    Skip,
    /// Stop the whole session. Already-finished tracks were saved as they
    /// completed, but the *current* track's session state is not: the
    /// caller must persist whatever [`TrackSession::note`] and
    /// [`TrackSession::intro_choice`] hold before dropping this session, the
    /// same as for [`LabelOutcome::Skip`].
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

    /// The intro side's confirmed choice, if `y` has already been pressed
    /// on it this session (i.e. the session has moved on to the outro
    /// side). `None` before that, including for a track abandoned by
    /// [`LabelKey::Skip`]/[`LabelKey::Quit`] while still on the intro side.
    pub fn intro_choice(&self) -> Option<&LabelChoice> {
        self.intro_choice.as_ref()
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
/// `Surya Groxyn - … - 06 Love & Joy.flac` at +0.4948 bar), so counting
/// candidates back from `total_frames` verbatim puts every one of them up
/// to half a bar — nearly two beats — off the grid on those tracks. That
/// residual is far outside the sub-half-beat range
/// [`lock_boundary_to_groove`] can absorb, so the guide click survives it
/// and fires mid-bar. Instead the outro side rounds
/// `(total_frames - first_downbeat) / bar_frames` to the nearest *integer*
/// bar count and re-derives the file-end anchor from `first_downbeat` plus
/// that many bars, then counts candidates back from the anchor. This only
/// fixes bar *identity* (which bar a candidate names); it is not the
/// rejected "propagate `first_downbeat + n × intro_bpm beats` to the file
/// end" anchor documented in HANDOFF §9, which failed because tempo
/// estimation error accumulates over hundreds of beats and moved sub-beat
/// phase by up to half a beat on `Boom Boom Pow`. Rounding to the nearest
/// *integer bar* is far more forgiving, and sub-beat phase is still left to
/// [`lock_boundary_to_groove`].
///
/// That accumulated error is real and was measured after the fact: pass
/// `bar_frames` from [`bar_frames_for`] and the nominal boundary lands on the
/// intro downbeat's propagated bar grid by construction, so whatever
/// [`lock_boundary_to_groove`] then has to move *is* the drift. Across
/// `testdata/` it is under 0.1 beat on 9 of 14 masters but reaches 0.47 on
/// `03. KazuyaP - Monitoring Db` and 0.49 on `Nicho - … - 04 Boom Boom Pow`
/// — right at the lock's half-beat radius, where which beat it snaps to is a
/// coin flip, and close enough to flip the nearest-integer-bar rounding above
/// on a master whose own residual already sits at 0.4948 bar
/// (`… - 06 Love & Joy`), which shifts every candidate by a whole bar. Pass
/// `bar_frames` from [`outro_bar_frames_refit`] instead and the drift is
/// removed at the source; [`click_grid`] is the entry point that measures
/// both the period and the outro anchor against the audio.
pub fn boundary_frame(analysis: &TrackAnalysis, side: Side, bars: u32) -> i64 {
    boundary_frame_on_grid(analysis, side, bars, ClickGrid::nominal(analysis, side))
}

/// The grid one side's guide clicks are laid out on: how long a bar is, and
/// (outro only) the frame the candidates count back from. Both need the
/// audio to pin down, so [`click_grid`] builds this once per clip instead of
/// re-deriving it per candidate.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ClickGrid {
    pub bar_frames: f64,
    /// Frame the outro side counts candidates back from: the bar line where
    /// the music ends ([`music_end_bar`]), which is not the end of the file.
    /// Unused on the intro side, which counts forward from `first_downbeat`.
    pub outro_anchor: i64,
}

impl ClickGrid {
    /// The grid implied by the analysis markers alone — no audio, so the
    /// outro anchor can only be the file end rounded onto the bar grid. Kept
    /// for [`boundary_frame`] and its tests.
    pub fn nominal(analysis: &TrackAnalysis, side: Side) -> Self {
        let bar_frames = bar_frames_for(analysis, side);
        let first_downbeat = analysis.first_downbeat as f64;
        let total = analysis.total_frames as f64;
        let outro_anchor = if bar_frames.is_finite() && bar_frames > 1.0 && total >= first_downbeat
        {
            let bars = ((total - first_downbeat) / bar_frames).round();
            (first_downbeat + bars * bar_frames).round() as i64
        } else {
            analysis.total_frames as i64
        };
        Self {
            bar_frames,
            outro_anchor,
        }
    }
}

/// [`boundary_frame`] on a caller-supplied grid, so the refined outro period
/// and the measured music-end anchor drive the same arithmetic.
pub fn boundary_frame_on_grid(
    analysis: &TrackAnalysis,
    side: Side,
    bars: u32,
    grid: ClickGrid,
) -> i64 {
    let span = (grid.bar_frames * f64::from(bars)).round() as i64;
    match side {
        Side::Intro => analysis.first_downbeat as i64 + span,
        Side::Outro => grid.outro_anchor - span,
    }
}

/// How far back from the file end to look for the end of the music, in bars.
const END_SCAN_BARS: u32 = 64;
/// How far below the track's own typical bar a beat's onset strength may fall
/// and still count as the rhythm running.
///
/// The separation this has to make is between a bar that is still playing
/// quietly and a bar that only holds the decay of the last hit. Measured
/// across the four masters in `testdata/` whose ends are ear-verified, the
/// weakest still-playing bar sits 10.2 dB down (`Yukitama & Zhenz - … - 05
/// Kimi to Semi Blue`, whose outro fades before it stops) and the loudest
/// decay-only bar 22.5 dB down (the same track, one bar later) — a 12 dB gap,
/// and this sits in the middle of it.
const END_BEAT_ONSET_DB: f64 = -16.0;

/// The bar line, counted from `first_downbeat`, where the music stops.
///
/// Not where the *file* stops: masters ring out and then pad with silence,
/// and every one of the 14 in `testdata/` does, from 0.64 bars to 3.86. Nor
/// where the last transient is: a track can end on a crash sitting right on
/// the final bar line (`… - 06 Love & Joy`) or stop after a bar the last
/// transient never reached (`… - 03 Shuki Shuki Song`), so no rounding rule
/// applied to "the last onset" gets both — measured, the fractional bar
/// positions of the last onset on the ear-verified tracks want rounding *up*
/// at 0.47 and *down* at 0.23 and 0.65.
///
/// What does separate them is whether the *rhythm* is still running: a bar of
/// music has an onset on every beat, a bar of decay has one at most. So this
/// takes the last bar whose four beats all carry an onset above
/// [`END_BEAT_ONSET_DB`], and returns the line after it. A terminal one-shot
/// on the next line is then exactly where the music ends, which is what the
/// ear reports on all four verified tracks.
///
/// `bar_frames` is the analyzer's period; the grid only has to be good enough
/// to sort onsets into beats, which a half-beat of accumulated drift does not
/// threaten. Returns `None` when the track is too short to scan or carries no
/// onsets at all, leaving the caller on the file end.
pub fn music_end_bar(
    buffer: &AudioBuffer,
    analysis: &TrackAnalysis,
    bar_frames: f64,
) -> Option<u32> {
    let total = buffer.frames as f64;
    let first_downbeat = analysis.first_downbeat as f64;
    if !(bar_frames.is_finite() && bar_frames > 1.0) || total <= first_downbeat {
        return None;
    }
    let last_bar = ((total - first_downbeat) / bar_frames).floor();
    if !(last_bar.is_finite() && last_bar >= 2.0) {
        return None;
    }
    let last_bar = last_bar as u32;
    let first_bar = last_bar.saturating_sub(END_SCAN_BARS);
    if last_bar - first_bar < 4 {
        return None;
    }

    let scan_start = (first_downbeat + f64::from(first_bar) * bar_frames).max(0.0) as usize;
    let mono: Vec<f32> = buffer.samples[scan_start * 2..]
        .chunks_exact(2)
        .map(|f| (f[0] + f[1]) * 0.5)
        .collect();
    let flux = funkot_core::analysis::onset_flux_envelope(&mono);
    if flux.is_empty() {
        return None;
    }
    let hop = funkot_core::analysis::ONSET_FLUX_HOP as f64;
    let at = |frames_from_scan: f64| -> usize {
        ((frames_from_scan / hop).round().max(0.0) as usize).min(flux.len())
    };

    // Per bar: the strength of its weakest beat (is the rhythm running?) and
    // of its typical one (what does a bar of this track look like?).
    let mut weakest = Vec::with_capacity((last_bar - first_bar) as usize);
    let mut typical = Vec::with_capacity((last_bar - first_bar) as usize);
    for bar in first_bar..last_bar {
        let mut beats = [0.0f64; BEATS_PER_BAR as usize];
        for (b, slot) in beats.iter_mut().enumerate() {
            let from = (f64::from(bar - first_bar) + b as f64 / f64::from(BEATS_PER_BAR))
                * bar_frames;
            let to = (f64::from(bar - first_bar) + (b + 1) as f64 / f64::from(BEATS_PER_BAR))
                * bar_frames;
            *slot = flux[at(from)..at(to)]
                .iter()
                .copied()
                .fold(0.0f64, f64::max);
        }
        let mut sorted = beats;
        sorted.sort_by(|a, b| a.partial_cmp(b).expect("flux values are finite"));
        weakest.push((bar, sorted[0]));
        typical.push(0.5 * (sorted[1] + sorted[2]));
    }

    let mut reference = typical.clone();
    reference.sort_by(|a, b| a.partial_cmp(b).expect("flux values are finite"));
    let reference = reference[reference.len() / 2];
    if !(reference > 0.0) {
        return None;
    }
    let threshold = reference * 10f64.powf(END_BEAT_ONSET_DB / 20.0);

    weakest
        .iter()
        .rev()
        .find(|&&(_, strength)| strength >= threshold)
        .map(|&(bar, _)| bar + 1)
}

/// Bars folded together to read a section's bar phase. Long enough that a few
/// atypical bars cannot carry the answer, short enough to sit inside one
/// section of the arrangement.
const PHASE_WINDOW_BARS: u32 = 48;
/// Bars skipped at each end of the track before folding, so the window never
/// includes the count-in or the ring-out.
const PHASE_EDGE_BARS: u32 = 4;
/// How far the quietest beat slot must sit below the next quietest before the
/// fold is allowed to name it.
///
/// Measured across `testdata/` the margin is 0.9–2.5 dB where the answer is
/// readable at all, and five of the fourteen masters are a wash on at least
/// one side — dense arrangements with no dynamic bar shape. Those must come
/// back `None` and leave the grid alone rather than pick a slot out of noise.
const PHASE_CLEAR_DB: f64 = 0.8;

/// Beats the outro's own bars sit *later* than the `first_downbeat` grid says.
///
/// Bar identity comes from `first_downbeat` and is counted forward, which
/// assumes the master's own bars never move against that grid. Three of the
/// fourteen masters in `testdata/` break the assumption — somewhere mid-track
/// a section is spliced in a beat short or long, and every outro candidate
/// then clicks on the wrong beat of the bar. No tempo refinement or
/// end-detection fix can reach it: the anchor is quantised to integer bars on
/// the grid that has already slipped.
///
/// What reads the slip is the bar's dynamic shape. Dance masters put the lift
/// before the downbeat, so the fourth beat is the quietest of the four; fold
/// the track over the bar near the intro and near the outro, and if the
/// quietest slot has moved, the bars have. The answer is the distance it
/// moved. Deliberately relative: which slot is quietest in absolute terms is a
/// musical assumption, but *that it is the same slot at both ends* is a
/// property of the grid, and it is the grid that is in question.
///
/// Returns 0 when either window is a wash ([`PHASE_CLEAR_DB`]) or the track is
/// too short to hold two disjoint windows — i.e. leaves the propagated grid
/// alone unless there is evidence against it.
///
/// Ear-verified on all four masters put in front of a listener as A/B clips
/// (`examples/outro_phase_ab`): `03. KazuyaP - Monitoring Db` +2 beats,
/// `… - 04 Eternal Light` +3, `… - 06 Love & Joy` +1, and
/// `… - 05 Kimi to Semi Blue` unshifted, each matching this measurement.
pub fn outro_beat_phase_shift(
    buffer: &AudioBuffer,
    analysis: &TrackAnalysis,
    bar_frames: f64,
    end_bar: u32,
) -> u32 {
    let win = PHASE_WINDOW_BARS;
    let edge = PHASE_EDGE_BARS;
    if !(bar_frames.is_finite() && bar_frames > 1.0) || end_bar < 2 * (win + edge) {
        return 0;
    }
    let mono: Vec<f32> = buffer
        .samples
        .chunks_exact(2)
        .map(|f| (f[0] + f[1]) * 0.5)
        .collect();
    let fd = analysis.first_downbeat as f64;

    let intro = quietest_beat_slot(&mono, fd, bar_frames, edge, edge + win);
    let outro = quietest_beat_slot(&mono, fd, bar_frames, end_bar - edge - win, end_bar - edge);
    match (intro, outro) {
        (Some(qi), Some(qo)) => (qo + BEATS_PER_BAR - qi) % BEATS_PER_BAR,
        _ => 0,
    }
}

/// Which beat slot of the `first_downbeat` bar is quietest over `[lo, hi)`
/// bars, or `None` when the four are too close for the answer to mean
/// anything. Mean square rather than onset flux: the statistic is the lift
/// *taken out* before the downbeat, which is an absence of level, and flux
/// reads the hat that often still runs through it.
fn quietest_beat_slot(mono: &[f32], fd: f64, bar_frames: f64, lo: u32, hi: u32) -> Option<u32> {
    let beat_frames = bar_frames / f64::from(BEATS_PER_BAR);
    let mut power = [0.0f64; BEATS_PER_BAR as usize];
    for bar in lo..hi {
        for (beat, slot) in power.iter_mut().enumerate() {
            let from = fd + f64::from(bar) * bar_frames + beat as f64 * beat_frames;
            *slot += mean_square(mono, from, from + beat_frames);
        }
    }
    let db: Vec<f64> = power
        .iter()
        .map(|v| 10.0 * v.max(1e-12).log10())
        .collect();
    let mut order: Vec<usize> = (0..db.len()).collect();
    order.sort_by(|&a, &b| db[a].partial_cmp(&db[b]).expect("levels are finite"));
    (db[order[1]] - db[order[0]] >= PHASE_CLEAR_DB).then_some(order[0] as u32)
}

fn mean_square(mono: &[f32], from: f64, to: f64) -> f64 {
    let i0 = (from.max(0.0) as usize).min(mono.len());
    let i1 = (to.max(0.0) as usize).min(mono.len());
    if i1 <= i0 {
        return 0.0;
    }
    let sum: f64 = mono[i0..i1]
        .iter()
        .map(|&s| f64::from(s) * f64::from(s))
        .sum();
    sum / (i1 - i0) as f64
}

/// Shortest lever arm, in beats from `first_downbeat`, a reference point has
/// to sit at before its position says anything useful about the beat period.
const REFIT_MIN_LEVER_BEATS: f64 = 64.0;
/// How far [`outro_bar_frames_refit`] may move the analyzer's period, as a
/// fraction. The drifts this exists to cancel are tenths of a beat over
/// hundreds of beats — 0.2% is already an order of magnitude more room than
/// any master in `testdata/` needs (worst: 0.041% on `Boom Boom Pow`), so a
/// larger correction means a reference point locked onto the wrong beat, not
/// a mis-estimated tempo.
const REFIT_MAX_REL_ADJUST: f64 = 0.002;
/// A reference point agrees with a candidate period if the period predicts
/// its position to within this many beats.
const REFIT_MAX_RESIDUAL_BEATS: f64 = 0.25;
/// Reference points that must agree before the refined period is trusted.
const REFIT_MIN_AGREEING_POINTS: usize = 3;

/// The outro-side bar length, with the analyzer's tempo refined against the
/// audio near the outro.
///
/// `outro_bpm` is estimated from the tail alone and is good to about 0.05%,
/// which is invisible locally and fatal after propagation: the anchor
/// [`boundary_frame`] builds counts hundreds of bars forward from
/// `first_downbeat`, and 0.04% over 577 bars is half a beat
/// (`Nicho - … - 04 Boom Boom Pow`, measured). At that size the guide click
/// stops being reliable in two separate ways — [`lock_boundary_to_groove`]
/// can only pull by less than half a beat, so it lands on whichever beat the
/// coin came up, and the nearest-integer-bar rounding inside
/// [`outro_anchor_frame`] can flip, moving every candidate by a whole bar.
///
/// The fix is to stop propagating a period the audio disagrees with. Each
/// outro candidate is locked onto the music with the analyzer's period, which
/// gives a position that is (a) local, so drift-free, and (b) a whole number
/// of beats from `first_downbeat` — unambiguous because the drift being
/// corrected is under half a beat in the first place. Dividing distance by
/// that whole number pins the period to ~0.001%; the median over the
/// candidates ignores up to two points that locked onto the wrong beat, and
/// the result is only accepted if [`REFIT_MIN_AGREEING_POINTS`] of them agree
/// with it and it stays within [`REFIT_MAX_REL_ADJUST`]. Otherwise the
/// analyzer's period is returned unchanged.
///
/// Bar *identity* is untouched: bar heads are still `first_downbeat` plus a
/// whole number of bars, and nothing here searches ±N beats for a better
/// correlation (HANDOFF §3/§8). Only the length of a bar changes.
///
/// Measured on `testdata/` (14 masters, all five outro candidates), the
/// residual left for the lock to absorb drops from up to 0.59 beat to 0.11 —
/// while the boundary itself moves by at most 0.09 beat, so this is a
/// robustness fix, not a repositioning.
///
/// Known limit, and the reason [`REFIT_MAX_REL_ADJUST`] is deliberately
/// tight: the reference points are found by locking the *drifted* nominal
/// positions, so this only recovers the true period while that drift is under
/// half a beat — which it is on every master measured (worst 0.49), but not by
/// much. Past that the references snap onto the neighbouring beat and the
/// refined period comes out one beat per lever arm too long. That outcome is
/// still internally consistent (every candidate lands on one grid, so a
/// labeler who trusts one candidate's clicks can trust the rest), but its
/// absolute beat identity is not recoverable from this evidence, and no
/// residual check here can tell the two apart. Resolving that needs downbeat
/// evidence from the audio (HANDOFF §11-5), not a better fit.
pub fn outro_bar_frames_refit(
    buffer: &AudioBuffer,
    analysis: &TrackAnalysis,
    outro_anchor: u64,
) -> f64 {
    let bar_nominal = bar_frames_for(analysis, Side::Outro);
    let beat_nominal = bar_nominal / f64::from(BEATS_PER_BAR);
    if !(beat_nominal.is_finite() && beat_nominal > 1.0) {
        return bar_nominal;
    }
    let first_downbeat = analysis.first_downbeat as f64;

    // (whole beats from first_downbeat, measured distance in frames)
    let mut points: Vec<(f64, f64)> = Vec::with_capacity(OUTRO_CANDIDATES.len());
    for &bars in OUTRO_CANDIDATES.iter() {
        let grid = ClickGrid {
            bar_frames: bar_nominal,
            outro_anchor: outro_anchor as i64,
        };
        let nominal = boundary_frame_on_grid(analysis, Side::Outro, bars, grid);
        let locked = lock_boundary_to_groove(buffer, nominal, bar_nominal, NORMAL_HALF_WIDTH_BARS);
        let dist = locked as f64 - first_downbeat;
        let beats = (dist / beat_nominal).round();
        if beats >= REFIT_MIN_LEVER_BEATS {
            points.push((beats, dist));
        }
    }
    if points.len() < REFIT_MIN_AGREEING_POINTS {
        return bar_nominal;
    }

    let mut periods: Vec<f64> = points.iter().map(|(beats, dist)| dist / beats).collect();
    periods.sort_by(|a, b| a.partial_cmp(b).expect("periods are finite"));
    let beat_refit = periods[periods.len() / 2];
    if !(beat_refit.is_finite() && beat_refit > 1.0)
        || (beat_refit / beat_nominal - 1.0).abs() > REFIT_MAX_REL_ADJUST
    {
        return bar_nominal;
    }
    let agreeing = points
        .iter()
        .filter(|(beats, dist)| {
            ((dist - beats * beat_refit) / beat_refit).abs() <= REFIT_MAX_RESIDUAL_BEATS
        })
        .count();
    if agreeing < REFIT_MIN_AGREEING_POINTS {
        return bar_nominal;
    }
    beat_refit * f64::from(BEATS_PER_BAR)
}

/// The grid the guide clicks are actually built on, measured against the
/// audio: the analyzer's period for intro candidates (propagated at most 96
/// bars, where drift is negligible), and for outro candidates the refined
/// period ([`outro_bar_frames_refit`]) with candidates counted back from
/// where the music ends ([`music_end_bar`]) rather than where the file does.
///
/// Composition matters here. The end of the music is found on the analyzer's
/// grid — it only has to sort onsets into beats — and comes back as a *bar
/// index*, which is drift-free by construction. The period is then refined,
/// and the anchor rebuilt from the same index on the refined period, so the
/// two corrections do not have to agree about frames. Last, the bar index is
/// offset by whatever [`outro_beat_phase_shift`] finds the master's own bars
/// have slipped against the propagated grid, which is the one error the other
/// two cannot express: they move the anchor by whole bars and by fractions of
/// a beat respectively, never by a beat.
pub fn click_grid(buffer: &AudioBuffer, analysis: &TrackAnalysis, side: Side) -> ClickGrid {
    if side == Side::Intro {
        return ClickGrid::nominal(analysis, Side::Intro);
    }
    let nominal = ClickGrid::nominal(analysis, Side::Outro);
    let Some(end_bar) = music_end_bar(buffer, analysis, nominal.bar_frames) else {
        return nominal;
    };
    let first_downbeat = analysis.first_downbeat as f64;
    let anchor_nominal = (first_downbeat + f64::from(end_bar) * nominal.bar_frames).round() as u64;
    let bar_frames = outro_bar_frames_refit(buffer, analysis, anchor_nominal);
    let shift = outro_beat_phase_shift(buffer, analysis, bar_frames, end_bar);
    let end = f64::from(end_bar) + f64::from(shift) / f64::from(BEATS_PER_BAR);
    ClickGrid {
        bar_frames,
        outro_anchor: (first_downbeat + end * bar_frames).round() as i64,
    }
}

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
    let grid = click_grid(buffer, analysis, side);
    build_candidate_clip_on_grid(buffer, analysis, side, bars, half_width_bars, click_opts, grid)
}

/// [`build_candidate_clip`] on a caller-supplied grid, so a diagnostic can
/// render the same clip against a deliberately altered anchor or period and
/// A/B the two by ear.
#[allow(clippy::too_many_arguments)]
pub fn build_candidate_clip_on_grid(
    buffer: &AudioBuffer,
    analysis: &TrackAnalysis,
    side: Side,
    bars: u32,
    half_width_bars: u32,
    click_opts: &ClickOptions,
    grid: ClickGrid,
) -> Vec<f32> {
    let boundary = locked_boundary_on_grid(buffer, analysis, side, bars, half_width_bars, grid);
    render_click_clip(buffer, boundary, grid.bar_frames, half_width_bars, click_opts)
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
    let grid = click_grid(buffer, analysis, side);
    locked_boundary_on_grid(buffer, analysis, side, bars, half_width_bars, grid)
}

/// [`locked_boundary_frame`] on a caller-supplied grid, so
/// [`build_candidate_clip`] can lay out its click grid on the same period it
/// placed the boundary with instead of paying for [`click_grid`] twice.
fn locked_boundary_on_grid(
    buffer: &AudioBuffer,
    analysis: &TrackAnalysis,
    side: Side,
    bars: u32,
    half_width_bars: u32,
    grid: ClickGrid,
) -> i64 {
    let nominal = boundary_frame_on_grid(analysis, side, bars, grid);
    lock_boundary_to_groove(buffer, nominal, grid.bar_frames, half_width_bars)
}

/// How far past the listening window a candidate clip keeps playing, in bars.
///
/// The window on its own answers "is the boundary click on a bar line", but
/// not the two questions that actually decide a label: when the click comes
/// too early, how far off is it (you have to hear where the section really
/// turns over), and when the outro steps down in stages, does the anchor
/// match the stage the ear calls the end. Both need the material *after* the
/// window. 64 bars is enough to reach the file end from the deepest outro
/// candidate (64 bars, minus the 8 already inside the window), and the clip
/// is truncated at the file end anyway, so nothing is padded with silence
/// that was not already inside the window.
pub const CONTINUE_AFTER_WINDOW_BARS: u32 = 64;

fn render_click_clip(
    buffer: &AudioBuffer,
    boundary_frame: i64,
    bar_frames: f64,
    half_width_bars: u32,
    click_opts: &ClickOptions,
) -> Vec<f32> {
    let window_bars = 2 * half_width_bars;
    let window_frames = (bar_frames * f64::from(window_bars)).round().max(0.0) as i64;
    let clip_start = boundary_frame - (bar_frames * f64::from(half_width_bars)).round() as i64;

    // Play on past the window, stopping at the file end. The window itself is
    // always rendered in full even if it runs past that end (those frames stay
    // silent, as before) — it is what the boundary click is judged against.
    let with_tail = (bar_frames * f64::from(window_bars + CONTINUE_AFTER_WINDOW_BARS))
        .round()
        .max(0.0) as i64;
    let to_file_end = (buffer.frames as i64 - clip_start).max(0);
    let clip_frames = with_tail.min(to_file_end).max(window_frames);

    // Copy source audio where the clip overlaps the file; out-of-range parts
    // (clip starts before frame 0, or the window runs past EOF) stay silent —
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
    // Size the clicks off the window, not the whole clip: the continuation
    // runs into the outro and often into a fade, and letting that pull the
    // clip's RMS down would quieten every click — including the ones inside
    // the window, where they have to cut through a full-level master.
    let window_len = (window_frames as usize * 2).min(out.len());
    let base_amp = click_base_amplitude(rms(&out[..window_len]), click_opts.click_db_above_rms);
    // The bar grid is a ruler; it keeps ticking past the window so bars can be
    // counted from the boundary click to wherever the music actually turns.
    let click_bars = if bar_frames > 1.0 {
        (clip_frames as f64 / bar_frames).ceil() as u32
    } else {
        window_bars
    };
    for k in 0..click_bars {
        let offset = (bar_frames * f64::from(k)).round() as usize;
        if offset * 2 >= out.len() {
            break;
        }
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
            track_bars: 128,
            outro_bars: 16,
            outro_structure_bars: 16,
            bars_estimated_low_confidence: false,
            intro_bars_low_confidence: false,
            outro_bars_low_confidence: false,
            intro_bars_manual: false,
            outro_bars_manual: false,
            outro_structure_bars_manual: false,
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

    // A `Skip`/`Quit` after `n` must not lose the note (or an already
    // -confirmed intro side): the caller reads both back off the session
    // to persist them, since neither `LabelOutcome` variant carries them.
    #[test]
    fn note_survives_skip_with_no_side_confirmed() {
        let mut s = TrackSession::new(8, 8);
        s.apply_key(LabelKey::Note("no candidate fits here".to_string()));
        assert_eq!(s.apply_key(LabelKey::Skip), LabelOutcome::Skip);
        assert_eq!(s.note(), "no candidate fits here");
        assert_eq!(s.intro_choice(), None);
    }

    #[test]
    fn note_and_intro_choice_survive_quit_on_the_outro_side() {
        let mut s = TrackSession::new(32, 8);
        match s.apply_key(LabelKey::Accept) {
            LabelOutcome::Replay => {}
            other => panic!("expected Replay (advancing to outro), got {other:?}"),
        }
        assert_eq!(s.current_side(), Side::Outro);
        s.apply_key(LabelKey::Note("outro doesn't match any candidate".to_string()));
        assert_eq!(s.apply_key(LabelKey::Quit), LabelOutcome::Quit);
        assert_eq!(s.note(), "outro doesn't match any candidate");
        assert_eq!(
            s.intro_choice(),
            Some(&LabelChoice { best: 32, ok: Vec::new() })
        );
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

    // --- outro grid: where the music ends, and how long a bar is -----------

    /// How far `pos` sits from the nearest bar head of the *true* grid the
    /// fixture's music was synthesized on, in beats.
    fn beats_from_true_bar_head(pos: i64, first_downbeat: u64, beat_true: f64) -> f64 {
        let bars = (pos - first_downbeat as i64) as f64 / (beat_true * 4.0);
        (bars - bars.round()) * 4.0
    }

    /// `music_bars` of grid music, then whatever the caller appends. The
    /// analyzer's tempo is off by `rel_tempo_error`, as it is by up to 0.04%
    /// on real masters.
    fn end_fixture(
        music_bars: u32,
        rel_tempo_error: f64,
    ) -> (Vec<f32>, u32, u64, f64, TrackAnalysis) {
        let sr = 22_050u32;
        let bpm_true = 180.0;
        let beat_true = f64::from(sr) * 60.0 / bpm_true; // 7350 exactly
        let first_downbeat = 1_820u64;
        let music_frames =
            first_downbeat + (beat_true * 4.0 * f64::from(music_bars)).round() as u64;
        let samples =
            synth_gridded_dense_loud_stereo(music_frames as usize, sr, first_downbeat, beat_true);
        let analysis = sample_analysis(
            sr,
            bpm_true / (1.0 + rel_tempo_error),
            bpm_true / (1.0 + rel_tempo_error),
            first_downbeat,
            music_frames,
        );
        (samples, sr, first_downbeat, beat_true, analysis)
    }

    fn finish(
        samples: Vec<f32>,
        sr: u32,
        analysis: &TrackAnalysis,
    ) -> (AudioBuffer, TrackAnalysis) {
        let frames = (samples.len() / 2) as u64;
        let buffer = AudioBuffer {
            sample_rate: sr,
            frames,
            samples,
        };
        let analysis = sample_analysis(
            sr,
            analysis.intro_bpm,
            analysis.outro_bpm,
            analysis.first_downbeat,
            frames,
        );
        (buffer, analysis)
    }

    /// Ring-out: a decaying chord, then silence. Every master in `testdata/`
    /// ends this way, from 0.64 bars to 3.86. Partials rather than noise on
    /// purpose -- a reverb tail has almost no spectral flux in it, which is
    /// exactly why flux can tell it apart from a bar of music, and noise
    /// would fake an onset in every frame.
    fn push_decaying_tail(
        samples: &mut Vec<f32>,
        bars: f64,
        silence_bars: f64,
        beat_true: f64,
        sr: u32,
    ) {
        let bar = beat_true * 4.0;
        let tail = (bar * bars).round() as usize;
        let sr_f = f64::from(sr);
        for i in 0..tail {
            let t = i as f64 / sr_f;
            let env = (-(i as f64) / (tail as f64 * 0.35)).exp() as f32;
            let mut v = 0.0f32;
            for (freq, amp) in [(220.0, 0.5), (330.0, 0.35), (440.0, 0.25)] {
                v += amp * (2.0 * std::f64::consts::PI * freq * t).sin() as f32;
            }
            let v = v * env * 0.6;
            samples.push(v);
            samples.push(v);
        }
        let silence = (bar * silence_bars).round() as usize;
        samples.extend(std::iter::repeat(0.0).take(silence * 2));
    }

    #[test]
    fn music_end_bar_is_the_line_after_the_last_bar_the_rhythm_runs_through() {
        let (mut samples, sr, _, beat_true, analysis) = end_fixture(120, 0.0);
        push_decaying_tail(&mut samples, 2.6, 0.4, beat_true, sr);
        let (buffer, analysis) = finish(samples, sr, &analysis);
        assert_eq!(
            music_end_bar(&buffer, &analysis, bar_frames_for(&analysis, Side::Outro)),
            Some(120),
            "the ring-out and the silence after it are not music"
        );
    }

    /// The shape of `Surya Groxyn - … - 06 Love & Joy`: the last thing struck
    /// is a one-shot sitting on the final bar line, with only decay after it.
    /// That line *is* the end -- counting the bar it opens as still playing
    /// (which is what a rule based on "the last onset" does) puts every
    /// candidate a bar late, which is what the labeler reported.
    #[test]
    fn music_end_bar_treats_a_terminal_one_shot_as_the_end_not_a_bar_of_music() {
        let (mut samples, sr, first_downbeat, beat_true, analysis) = end_fixture(120, 0.0);
        // One loud hit exactly on bar line 120, then the ring-out.
        let hit = (0.25 * f64::from(sr)) as usize;
        let mut state = 0xBEEF_0001u32;
        for i in 0..hit {
            let env = (-(i as f64) / (0.10 * f64::from(sr))).exp() as f32;
            let v = xorshift32(&mut state) * 0.9 * env;
            samples.push(v);
            samples.push(v);
        }
        push_decaying_tail(&mut samples, 2.0, 0.4, beat_true, sr);
        let (buffer, analysis) = finish(samples, sr, &analysis);
        let _ = first_downbeat;
        assert_eq!(
            music_end_bar(&buffer, &analysis, bar_frames_for(&analysis, Side::Outro)),
            Some(120)
        );
    }

    /// The shape of `Yukitama & Zhenz - … - 05 Kimi to Semi Blue`: the last
    /// bars are well down in level but still play on every beat. They are
    /// music, and stopping before them puts every candidate a bar early.
    #[test]
    fn music_end_bar_keeps_a_quiet_bar_that_still_plays_on_every_beat() {
        let (mut samples, sr, first_downbeat, beat_true, analysis) = end_fixture(120, 0.0);
        // Take the last two bars down to a third (-10 dB), like a master that
        // pulls the faders before it stops.
        let bar = (beat_true * 4.0) as usize;
        let from = samples.len() - 2 * bar * 2;
        for v in samples[from..].iter_mut() {
            *v *= 0.30;
        }
        push_decaying_tail(&mut samples, 2.0, 0.4, beat_true, sr);
        let (buffer, analysis) = finish(samples, sr, &analysis);
        let _ = first_downbeat;
        assert_eq!(
            music_end_bar(&buffer, &analysis, bar_frames_for(&analysis, Side::Outro)),
            Some(120),
            "quiet bars that still carry every beat are music"
        );
    }

    #[test]
    fn music_end_bar_drops_a_fade_that_has_gone_far_enough_down() {
        let (mut samples, sr, _, beat_true, analysis) = end_fixture(120, 0.0);
        let bar = (beat_true * 4.0) as usize;
        // -28 dB: past the point where a labeler still counts it as the track
        // playing (measured separation is 12 dB wide; this is well outside).
        let from = samples.len() - 4 * bar * 2;
        for v in samples[from..].iter_mut() {
            *v *= 0.04;
        }
        push_decaying_tail(&mut samples, 1.0, 0.4, beat_true, sr);
        let (buffer, analysis) = finish(samples, sr, &analysis);
        assert_eq!(
            music_end_bar(&buffer, &analysis, bar_frames_for(&analysis, Side::Outro)),
            Some(116),
            "a fade this far down is the outro dying, not bars of music"
        );
    }

    /// A master whose own bars have slipped `shift` beats against the
    /// `first_downbeat` grid by the time the outro arrives. The lift before
    /// the downbeat -- the level taken out of the bar's last beat, which is
    /// what the fold reads -- sits in grid slot 3 for the first half and in
    /// slot `(3 + shift) % 4` for the second, which is how a section spliced
    /// a beat short looks from the grid's side. Nothing else about the track
    /// changes: the tempo is exact and every beat still carries a kick, so
    /// only the bar phase can explain what comes out.
    fn phase_slip_fixture(music_bars: u32, shift: u32) -> (AudioBuffer, TrackAnalysis) {
        let (mut samples, sr, first_downbeat, beat_true, analysis) = end_fixture(music_bars, 0.0);
        let quiet_from = |bar: u32| {
            let slot = if bar < music_bars / 2 {
                3
            } else {
                (3 + shift) % BEATS_PER_BAR
            };
            first_downbeat as f64 + (f64::from(bar * BEATS_PER_BAR + slot)) * beat_true
        };
        for bar in 0..music_bars {
            let from = quiet_from(bar);
            let i0 = (from.round() as usize).min(samples.len() / 2);
            let i1 = ((from + beat_true).round() as usize).min(samples.len() / 2);
            for v in samples[i0 * 2..i1 * 2].iter_mut() {
                *v *= 0.25;
            }
        }
        push_decaying_tail(&mut samples, 2.6, 0.4, beat_true, sr);
        finish(samples, sr, &analysis)
    }

    #[test]
    fn outro_beat_phase_shift_reads_bars_that_slipped_mid_track() {
        for shift in 1..BEATS_PER_BAR {
            let (buffer, analysis) = phase_slip_fixture(120, shift);
            let bar_frames = bar_frames_for(&analysis, Side::Outro);
            let end_bar = music_end_bar(&buffer, &analysis, bar_frames).expect("has an end");
            assert_eq!(
                outro_beat_phase_shift(&buffer, &analysis, bar_frames, end_bar),
                shift,
                "the outro's bars sit {shift} beat(s) past the propagated grid"
            );
        }
    }

    #[test]
    fn outro_beat_phase_shift_leaves_a_steady_grid_alone() {
        let (buffer, analysis) = phase_slip_fixture(120, 0);
        let bar_frames = bar_frames_for(&analysis, Side::Outro);
        let end_bar = music_end_bar(&buffer, &analysis, bar_frames).expect("has an end");
        assert_eq!(
            outro_beat_phase_shift(&buffer, &analysis, bar_frames, end_bar),
            0
        );
    }

    /// Five of the fourteen masters in `testdata/` are a wash on at least one
    /// side -- dense arrangements with no dynamic bar shape to read. Naming a
    /// slot there would move every outro candidate on noise, so the fold has
    /// to decline and leave the propagated grid standing.
    #[test]
    fn outro_beat_phase_shift_declines_when_the_bar_has_no_shape() {
        let (buffer, analysis) = {
            let (samples, sr, _, _, analysis) = end_fixture(120, 0.0);
            finish(samples, sr, &analysis)
        };
        let bar_frames = bar_frames_for(&analysis, Side::Outro);
        let mono: Vec<f32> = buffer
            .samples
            .chunks_exact(2)
            .map(|f| (f[0] + f[1]) * 0.5)
            .collect();
        assert_eq!(
            quietest_beat_slot(&mono, analysis.first_downbeat as f64, bar_frames, 4, 52),
            None,
            "every beat of this fixture carries a kick and hats; there is no lift to find"
        );
    }

    /// A track too short to hold two disjoint windows cannot be measured, and
    /// an unmeasurable track must not be moved.
    #[test]
    fn outro_beat_phase_shift_declines_on_a_track_too_short_to_measure() {
        let (buffer, analysis) = phase_slip_fixture(60, 2);
        let bar_frames = bar_frames_for(&analysis, Side::Outro);
        assert_eq!(
            outro_beat_phase_shift(&buffer, &analysis, bar_frames, 60),
            0
        );
    }

    #[test]
    fn outro_candidates_follow_the_slipped_bar_phase() {
        let (buffer, analysis) = phase_slip_fixture(120, 2);
        let grid = click_grid(&buffer, &analysis, Side::Outro);
        let beat = grid.bar_frames / f64::from(BEATS_PER_BAR);
        let anchor_beats =
            (grid.outro_anchor as f64 - analysis.first_downbeat as f64) / beat;
        assert!(
            (anchor_beats - (120.0 * 4.0 + 2.0)).abs() < 0.05,
            "anchor sits {anchor_beats:.2} beats past the downbeat, \
             want the music's last line plus the 2-beat slip (482)"
        );
    }

    #[test]
    fn outro_candidates_count_back_from_the_music_not_the_file() {
        let (mut samples, sr, first_downbeat, beat_true, analysis) = end_fixture(120, 0.0);
        push_decaying_tail(&mut samples, 2.6, 0.4, beat_true, sr);
        let (buffer, analysis) = finish(samples, sr, &analysis);
        let bar = beat_true * 4.0;

        let grid = click_grid(&buffer, &analysis, Side::Outro);
        let anchor_bars = (grid.outro_anchor as f64 - first_downbeat as f64) / bar;
        assert!(
            (anchor_bars - 120.0).abs() < 0.05,
            "anchor sits at bar {anchor_bars:.3}, want the music's last line (120)"
        );

        for &bars in OUTRO_CANDIDATES.iter() {
            let with = boundary_frame_on_grid(&analysis, Side::Outro, bars, grid);
            let off = beats_from_true_bar_head(with, first_downbeat, beat_true);
            assert!(off.abs() <= 0.15, "{bars}bars: {off:+.3} beat off a bar head");

            let file_anchored = boundary_frame_on_grid(
                &analysis,
                Side::Outro,
                bars,
                ClickGrid::nominal(&analysis, Side::Outro),
            );
            let late = (file_anchored - with) as f64 / bar;
            assert!(
                late >= 2.0,
                "{bars}bars: file-anchored candidate only {late:+.2} bar late; \
                 the fixture is not exercising the tail"
            );
        }
    }

    #[test]
    fn outro_refit_stops_the_lock_from_having_to_rescue_a_drifted_grid() {
        // 0.04% is the worst tempo error measured across `testdata/`; over a
        // thousand beats it leaves every candidate a third of a beat or more
        // off the music, which the lock can still just about rescue. That
        // fragility is what the refit removes, so the assertions are on the
        // nominal positions the lock is handed.
        let (mut samples, sr, first_downbeat, beat_true, analysis) = end_fixture(256, 0.0004);
        push_decaying_tail(&mut samples, 2.0, 0.4, beat_true, sr);
        let (buffer, analysis) = finish(samples, sr, &analysis);
        let bar_nominal = bar_frames_for(&analysis, Side::Outro);
        let anchor = (first_downbeat as f64 + 256.0 * bar_nominal).round() as u64;

        let bar_refit = outro_bar_frames_refit(&buffer, &analysis, anchor);
        assert!(
            (bar_refit / (beat_true * 4.0) - 1.0).abs() < 1e-4,
            "refit should recover the real bar length: {bar_refit:.3} vs {:.3}",
            beat_true * 4.0
        );

        let grid = click_grid(&buffer, &analysis, Side::Outro);
        let mut worst_without = 0.0f64;
        for &bars in OUTRO_CANDIDATES.iter() {
            let without = boundary_frame_on_grid(
                &analysis,
                Side::Outro,
                bars,
                ClickGrid {
                    bar_frames: bar_nominal,
                    outro_anchor: anchor as i64,
                },
            );
            worst_without = worst_without
                .max(beats_from_true_bar_head(without, first_downbeat, beat_true).abs());

            let with = boundary_frame_on_grid(&analysis, Side::Outro, bars, grid);
            let off = beats_from_true_bar_head(with, first_downbeat, beat_true);
            assert!(
                off.abs() <= 0.1,
                "{bars}bars: refit nominal sits {off:+.3} beat from a bar head, so the \
                 lock is still being asked to rescue it"
            );
        }
        assert!(
            worst_without >= 0.25,
            "fixture is not exercising the drift: worst pre-refit nominal was only \
             {worst_without:+.3} beat off a bar head"
        );
    }

    #[test]
    fn outro_refit_leaves_an_already_correct_tempo_alone() {
        let (mut samples, sr, first_downbeat, beat_true, analysis) = end_fixture(256, 0.0);
        push_decaying_tail(&mut samples, 2.0, 0.4, beat_true, sr);
        let (buffer, analysis) = finish(samples, sr, &analysis);
        let bar_nominal = bar_frames_for(&analysis, Side::Outro);
        let anchor = (first_downbeat as f64 + 256.0 * bar_nominal).round() as u64;
        let bar_refit = outro_bar_frames_refit(&buffer, &analysis, anchor);
        assert!(
            (bar_refit / bar_nominal - 1.0).abs() < 1e-4,
            "refit moved an exact tempo by {:+.5}%",
            (bar_refit / bar_nominal - 1.0) * 100.0
        );
        let _ = beat_true;
    }

    #[test]
    fn outro_grid_falls_back_when_there_is_too_little_track_to_measure() {
        let (samples, sr, first_downbeat, _, analysis) = end_fixture(2, 0.0);
        let (buffer, analysis) = finish(samples, sr, &analysis);
        assert_eq!(
            music_end_bar(&buffer, &analysis, bar_frames_for(&analysis, Side::Outro)),
            None
        );
        let grid = click_grid(&buffer, &analysis, Side::Outro);
        assert_eq!(grid, ClickGrid::nominal(&analysis, Side::Outro));
        assert_eq!(
            outro_bar_frames_refit(&buffer, &analysis, first_downbeat),
            bar_frames_for(&analysis, Side::Outro)
        );
    }
}
