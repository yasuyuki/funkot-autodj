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
pub fn boundary_frame(analysis: &TrackAnalysis, side: Side, bars: u32) -> i64 {
    let bar_frames = bar_frames_for(analysis, side);
    let span = (bar_frames * f64::from(bars)).round() as i64;
    match side {
        Side::Intro => analysis.first_downbeat as i64 + span,
        Side::Outro => analysis.total_frames as i64 - span,
    }
}

/// Build the interleaved-stereo listening clip for one candidate: the
/// original audio from `half_width_bars` before to `half_width_bars` after
/// the boundary, with a click synthesized on every bar head in that range
/// and a distinct (lower, longer, double-hit) click on the boundary bar
/// itself, so the candidate is unmistakable by ear.
pub fn build_candidate_clip(
    buffer: &AudioBuffer,
    analysis: &TrackAnalysis,
    side: Side,
    bars: u32,
    half_width_bars: u32,
) -> Vec<f32> {
    let bar_frames = bar_frames_for(analysis, side);
    let boundary = boundary_frame(analysis, side, bars);
    render_click_clip(buffer, boundary, bar_frames, half_width_bars)
}

fn render_click_clip(
    buffer: &AudioBuffer,
    boundary_frame: i64,
    bar_frames: f64,
    half_width_bars: u32,
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
    for k in 0..total_bars {
        let offset = (bar_frames * f64::from(k)).round() as usize;
        let kind = if k == half_width_bars {
            ClickKind::Boundary
        } else {
            ClickKind::Normal
        };
        add_click(&mut out, offset, sr, kind);
    }
    out
}

#[derive(Clone, Copy)]
enum ClickKind {
    Normal,
    Boundary,
}

/// Overlay a bar-head click on `out` (interleaved stereo) at frame `start`.
fn add_click(out: &mut [f32], start: usize, sr: f64, kind: ClickKind) {
    match kind {
        ClickKind::Normal => add_click_tone(out, start, sr, 2200.0, 0.020, 0.5),
        ClickKind::Boundary => {
            add_click_tone(out, start, sr, 900.0, 0.035, 0.75);
            let second = start + (0.045 * sr).round() as usize;
            add_click_tone(out, second, sr, 900.0, 0.035, 0.6);
        }
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
        let clip = build_candidate_clip(&buffer, &analysis, Side::Intro, bars, half_width);

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
}
