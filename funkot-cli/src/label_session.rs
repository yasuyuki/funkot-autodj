use std::f32::consts::PI;

use funkot_core::decode::AudioBuffer;
use funkot_core::{TrackAnalysis, BEATS_PER_BAR};

pub const INTRO_CANDIDATES: [u32; 7] = [8, 16, 32, 48, 64, 80, 96];
pub const OUTRO_CANDIDATES: [u32; 5] = [8, 16, 32, 48, 64];

pub const NORMAL_HALF_WIDTH_BARS: u32 = 8;
pub const WIDE_HALF_WIDTH_BARS: u32 = 16;

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

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LabelKey {
    Accept,
    Left,
    Right,
    ToggleWidth,
    Replay,
    Ambiguous,
    Note(String),
    Skip,
    Quit,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LabelChoice {
    pub best: u32,
    pub ok: Vec<u32>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum Mode {
    Normal,
    Ambiguous { selected: Vec<bool> },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LabelOutcome {
    Continue,
    Replay,
    Done { intro: LabelChoice, outro: LabelChoice },
    Skip,
    Quit,
}

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

    pub fn neighbour_bars(&self) -> Vec<u32> {
        let candidates = self.side.candidates();
        let mut out = Vec::with_capacity(2);
        if self.cursor > 0 {
            out.push(candidates[self.cursor - 1]);
        }
        if self.cursor + 1 < candidates.len() {
            out.push(candidates[self.cursor + 1]);
        }
        out
    }

    pub fn note(&self) -> &str {
        &self.note
    }

    pub fn intro_choice(&self) -> Option<&LabelChoice> {
        self.intro_choice.as_ref()
    }

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

pub fn bar_frames_for(analysis: &TrackAnalysis, side: Side) -> f64 {
    let bpm = match side {
        Side::Intro => analysis.intro_bpm,
        Side::Outro => analysis.outro_bpm,
    };
    f64::from(analysis.sample_rate) * 60.0 / bpm * f64::from(BEATS_PER_BAR)
}

pub fn boundary_frame(analysis: &TrackAnalysis, side: Side, bars: u32) -> i64 {
    boundary_frame_on_grid(analysis, side, bars, ClickGrid::nominal(analysis, side))
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ClickGrid {
    pub bar_frames: f64,
    pub outro_anchor: i64,
    pub lock_offset: Option<f64>,
}

impl ClickGrid {
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
            lock_offset: None,
        }
    }
}

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

const END_SCAN_BARS: u32 = 64;
const END_BEAT_ONSET_DB: f64 = -16.0;

pub fn music_end_bar(
    buffer: &AudioBuffer,
    analysis: &TrackAnalysis,
    bar_frames: f64,
    beat_phase: u32,
) -> Option<u32> {
    let total = buffer.frames as f64;
    let first_downbeat = analysis.first_downbeat as f64;
    if !(bar_frames.is_finite() && bar_frames > 1.0) || total <= first_downbeat {
        return None;
    }
    let origin = first_downbeat + f64::from(beat_phase) * bar_frames / f64::from(BEATS_PER_BAR);
    let last_bar = ((total - origin) / bar_frames).floor();
    if !(last_bar.is_finite() && last_bar >= 2.0) {
        return None;
    }
    let last_bar = last_bar as u32;
    let first_bar = last_bar.saturating_sub(END_SCAN_BARS);
    if last_bar - first_bar < 4 {
        return None;
    }

    let scan_start = (origin + f64::from(first_bar) * bar_frames).max(0.0) as usize;
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

const PHASE_WINDOW_BARS: u32 = 48;
const PHASE_EDGE_BARS: u32 = 4;
const PHASE_CLEAR_DB: f64 = 0.8;

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
        (Some(qi), Some(qo)) if qi == BEATS_PER_BAR - 1 => {
            (qo + BEATS_PER_BAR - qi) % BEATS_PER_BAR
        }
        _ => 0,
    }
}

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

const REFIT_MIN_LEVER_BEATS: f64 = 64.0;
const REFIT_MAX_REL_ADJUST: f64 = 0.002;
const REFIT_MAX_RESIDUAL_BEATS: f64 = 0.25;
const REFIT_MIN_AGREEING_POINTS: usize = 3;

const BRANCH_SPLIT_BEATS: f64 = 0.25;
const BRANCH_HALF_BEAT_TOL: f64 = 0.15;

fn median(values: &[f64]) -> f64 {
    let mut sorted = values.to_vec();
    sorted.sort_by(|a, b| a.partial_cmp(b).expect("offsets are finite"));
    let n = sorted.len();
    if n % 2 == 1 {
        sorted[n / 2]
    } else {
        0.5 * (sorted[n / 2 - 1] + sorted[n / 2])
    }
}

fn branch_indices(offsets: &[f64], beat: f64) -> Option<Vec<usize>> {
    if offsets.len() < 3 {
        return None;
    }
    let med = median(offsets);
    let near: Vec<usize> = (0..offsets.len())
        .filter(|&i| (offsets[i] - med).abs() <= BRANCH_SPLIT_BEATS * beat)
        .collect();
    let far: Vec<usize> = (0..offsets.len())
        .filter(|&i| (offsets[i] - med).abs() > BRANCH_SPLIT_BEATS * beat)
        .collect();
    if far.is_empty() {
        return Some(near);
    }
    if near.is_empty() {
        return None;
    }
    let near_med = median(&near.iter().map(|&i| offsets[i]).collect::<Vec<_>>());
    let far_med = median(&far.iter().map(|&i| offsets[i]).collect::<Vec<_>>());
    if ((far_med - med).abs() - 0.5 * beat).abs() <= BRANCH_HALF_BEAT_TOL * beat {
        Some(if near_med.abs() <= far_med.abs() { near } else { far })
    } else {
        Some(near)
    }
}

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

    let mut points: Vec<(f64, f64)> = Vec::with_capacity(OUTRO_CANDIDATES.len());
    let mut offsets: Vec<f64> = Vec::with_capacity(OUTRO_CANDIDATES.len());
    for &bars in OUTRO_CANDIDATES.iter() {
        let grid = ClickGrid {
            bar_frames: bar_nominal,
            outro_anchor: outro_anchor as i64,
            lock_offset: None,
        };
        let nominal = boundary_frame_on_grid(analysis, Side::Outro, bars, grid);
        let locked = lock_boundary_to_groove(buffer, nominal, bar_nominal, NORMAL_HALF_WIDTH_BARS);
        let dist = locked as f64 - first_downbeat;
        let beats = (dist / beat_nominal).round();
        if beats >= REFIT_MIN_LEVER_BEATS {
            points.push((beats, dist));
            offsets.push((locked - nominal) as f64);
        }
    }
    if let Some(keep) = branch_indices(&offsets, beat_nominal) {
        points = keep.into_iter().map(|i| points[i]).collect();
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

pub fn click_grid(buffer: &AudioBuffer, analysis: &TrackAnalysis, side: Side) -> ClickGrid {
    if side == Side::Intro {
        let grid = ClickGrid::nominal(analysis, Side::Intro);
        let lock_offset = side_lock_offset(buffer, analysis, Side::Intro, grid);
        return ClickGrid { lock_offset, ..grid };
    }
    let nominal = ClickGrid::nominal(analysis, Side::Outro);
    let Some(end_on_grid) = music_end_bar(buffer, analysis, nominal.bar_frames, 0) else {
        return nominal;
    };
    let first_downbeat = analysis.first_downbeat as f64;
    let anchor_nominal =
        (first_downbeat + f64::from(end_on_grid) * nominal.bar_frames).round() as u64;
    let bar_frames = outro_bar_frames_refit(buffer, analysis, anchor_nominal);
    let shift = outro_beat_phase_shift(buffer, analysis, bar_frames, end_on_grid);
    let end_bar = if shift == 0 {
        end_on_grid
    } else {
        music_end_bar(buffer, analysis, nominal.bar_frames, shift).unwrap_or(end_on_grid)
    };
    let end = f64::from(end_bar) + f64::from(shift) / f64::from(BEATS_PER_BAR);
    let grid = ClickGrid {
        bar_frames,
        outro_anchor: (first_downbeat + end * bar_frames).round() as i64,
        lock_offset: None,
    };
    let lock_offset = side_lock_offset(buffer, analysis, Side::Outro, grid);
    ClickGrid { lock_offset, ..grid }
}

#[derive(Debug, Default)]
pub struct SideGrids {
    intro: Option<ClickGrid>,
    outro: Option<ClickGrid>,
}

impl SideGrids {
    pub fn get(&mut self, buffer: &AudioBuffer, analysis: &TrackAnalysis, side: Side) -> ClickGrid {
        let slot = match side {
            Side::Intro => &mut self.intro,
            Side::Outro => &mut self.outro,
        };
        *slot.get_or_insert_with(|| click_grid(buffer, analysis, side))
    }
}

fn side_lock_offset(
    buffer: &AudioBuffer,
    analysis: &TrackAnalysis,
    side: Side,
    grid: ClickGrid,
) -> Option<f64> {
    let beat = grid.bar_frames / f64::from(BEATS_PER_BAR);
    if !(beat.is_finite() && beat > 1.0) {
        return None;
    }
    let total = buffer.frames as f64;
    let half_span = grid.bar_frames * f64::from(NORMAL_HALF_WIDTH_BARS);
    let mut offsets: Vec<f64> = Vec::with_capacity(side.candidates().len());
    for &bars in side.candidates() {
        let nominal = boundary_frame_on_grid(analysis, side, bars, grid);
        let nominal_f = nominal as f64;
        if nominal_f - half_span < 0.0 || nominal_f + half_span > total {
            continue;
        }
        let locked =
            lock_boundary_to_groove(buffer, nominal, grid.bar_frames, NORMAL_HALF_WIDTH_BARS);
        offsets.push((locked - nominal) as f64);
    }
    let keep = branch_indices(&offsets, beat)?;
    Some(median(&keep.iter().map(|&i| offsets[i]).collect::<Vec<_>>()))
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

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ClickOptions {
    pub click_db_above_rms: f32,
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

const SILENT_CLIP_FALLBACK_AMP: f32 = 0.5;

const NORMAL_CLICK_FREQ: f32 = 2200.0;
const NORMAL_CLICK_DUR_SECS: f64 = 0.020;
const BOUNDARY_CLICK_FREQ: f32 = 900.0;
const BOUNDARY_CLICK_DUR_SECS: f64 = 0.035;
const BOUNDARY_SECOND_HIT_OFFSET_SECS: f64 = 0.045;
const BOUNDARY_AMP_FIRST_SCALE: f32 = 1.5;
const BOUNDARY_AMP_SECOND_SCALE: f32 = 1.2;
const BOUNDARY_EXTRA_DUCK_DB: f32 = 6.0;

const DUCK_ATTACK_SECS: f64 = 0.005;
const DUCK_RELEASE_SECS: f64 = 0.035;
const NORMAL_DUCK_HOLD_SECS: f64 = NORMAL_CLICK_DUR_SECS + 0.010;
const BOUNDARY_DUCK_HOLD_SECS: f64 =
    BOUNDARY_SECOND_HIT_OFFSET_SECS + BOUNDARY_CLICK_DUR_SECS + 0.010;

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

pub fn locked_boundary_on_grid(
    buffer: &AudioBuffer,
    analysis: &TrackAnalysis,
    side: Side,
    bars: u32,
    half_width_bars: u32,
    grid: ClickGrid,
) -> i64 {
    let nominal = boundary_frame_on_grid(analysis, side, bars, grid);
    let locked = lock_boundary_to_groove(buffer, nominal, grid.bar_frames, half_width_bars);
    if let Some(consensus) = grid.lock_offset {
        let beat = grid.bar_frames / f64::from(BEATS_PER_BAR);
        let own = (locked - nominal) as f64;
        if beat.is_finite() && beat > 1.0 && (own - consensus).abs() > BRANCH_SPLIT_BEATS * beat {
            return nominal + consensus.round() as i64;
        }
    }
    locked
}

pub const SURVEY_WINDOW_BARS: u32 = 8;

pub fn survey_click_offsets(bar_frames: f64) -> Vec<i64> {
    if !(bar_frames.is_finite() && bar_frames > 1.0) {
        return Vec::new();
    }
    let beat_frames = bar_frames / f64::from(BEATS_PER_BAR);
    let n = SURVEY_WINDOW_BARS * BEATS_PER_BAR;
    (0..n).map(|k| (beat_frames * f64::from(k)).round() as i64).collect()
}

pub fn build_survey_clip_on_grid(
    buffer: &AudioBuffer,
    bar_frames: f64,
    end_frame: i64,
    click_opts: &ClickOptions,
) -> Vec<f32> {
    let window_frames = (bar_frames * f64::from(SURVEY_WINDOW_BARS)).round().max(0.0) as i64;
    let clip_start = end_frame - window_frames;

    let mut out = vec![0.0f32; window_frames as usize * 2];
    for i in 0..window_frames {
        let src_frame = clip_start + i;
        if src_frame >= 0 && (src_frame as u64) < buffer.frames {
            let src_idx = src_frame as usize * 2;
            out[i as usize * 2] = buffer.samples[src_idx];
            out[i as usize * 2 + 1] = buffer.samples[src_idx + 1];
        }
    }

    let sr = f64::from(buffer.sample_rate);
    let base_amp = click_base_amplitude(rms(&out), click_opts.click_db_above_rms);
    for offset in survey_click_offsets(bar_frames) {
        if offset < 0 {
            continue;
        }
        let start = offset as usize;
        if start * 2 >= out.len() {
            break;
        }
        add_click(&mut out, start, sr, ClickKind::Normal, base_amp, click_opts.duck_db);
    }
    out
}

pub fn build_survey_clip(
    buffer: &AudioBuffer,
    analysis: &TrackAnalysis,
    click_opts: &ClickOptions,
) -> Vec<f32> {
    let grid = click_grid(buffer, analysis, Side::Outro);
    build_survey_clip_on_grid(buffer, grid.bar_frames, grid.outro_anchor, click_opts)
}

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

    let with_tail = (bar_frames * f64::from(window_bars + CONTINUE_AFTER_WINDOW_BARS))
        .round()
        .max(0.0) as i64;
    let to_file_end = (buffer.frames as i64 - clip_start).max(0);
    let clip_frames = with_tail.min(to_file_end).max(window_frames);

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
    let window_len = (window_frames as usize * 2).min(out.len());
    let base_amp = click_base_amplitude(rms(&out[..window_len]), click_opts.click_db_above_rms);
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

const CLIP_SAFETY_CEILING: f32 = 0.97;

fn safe_click_amplitude(out: &[f32], start: usize, dur_secs: f64, sr: f64, target: f32) -> f32 {
    let len = (dur_secs * sr).round().max(1.0) as i64;
    let residual = local_peak(out, start as i64, len);
    let headroom = (CLIP_SAFETY_CEILING - residual).max(0.0);
    target.min(headroom)
}

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
            is_funkot: true,
            classify_scores: None,
        }
    }

    #[test]
    fn cursor_clamps_at_both_ends() {
        let mut s = TrackSession::new(8, 8);
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
    fn neighbour_bars_at_a_middle_position_returns_both_sides() {
        let s = TrackSession::new(48, 8);
        assert_eq!(s.current_bars(), 48);
        assert_eq!(s.neighbour_bars(), vec![32, 64]);
    }

    #[test]
    fn neighbour_bars_at_the_first_candidate_has_only_a_next() {
        let s = TrackSession::new(8, 8);
        assert_eq!(s.current_bars(), 8);
        assert_eq!(s.neighbour_bars(), vec![16]);
    }

    #[test]
    fn neighbour_bars_at_the_last_candidate_has_only_a_prev() {
        let mut s = TrackSession::new(8, 8);
        for _ in 0..INTRO_CANDIDATES.len() + 3 {
            s.apply_key(LabelKey::Right);
        }
        assert_eq!(s.current_bars(), *INTRO_CANDIDATES.last().unwrap());
        assert_eq!(
            s.neighbour_bars(),
            vec![INTRO_CANDIDATES[INTRO_CANDIDATES.len() - 2]]
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

    #[test]
    fn ambiguous_toggle_builds_ok_set_excluding_best() {
        let mut s = TrackSession::new(32, 8);
        assert_eq!(s.current_bars(), 32);

        s.apply_key(LabelKey::Ambiguous);
        assert_eq!(s.ambiguous_selected(), Some(vec![32]));

        s.apply_key(LabelKey::Right);
        s.apply_key(LabelKey::Ambiguous);
        assert_eq!(s.ambiguous_selected(), Some(vec![32, 48]));

        s.apply_key(LabelKey::Left);
        s.apply_key(LabelKey::Ambiguous);
        assert_eq!(s.ambiguous_selected(), Some(vec![48]));

        match s.apply_key(LabelKey::Accept) {
            LabelOutcome::Replay => {}
            other => panic!("expected Replay (advancing to outro), got {other:?}"),
        }
        assert_eq!(s.current_side(), Side::Outro);
        assert_eq!(
            s.current_bars(),
            8,
            "outro cursor should reset to its own initial estimate"
        );
    }

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

    #[test]
    fn outro_boundary_unchanged_when_file_end_is_already_on_grid() {
        let analysis = sample_analysis(180, 180.0, 180.0, 100, 2980);
        assert_eq!(boundary_frame(&analysis, Side::Outro, 2), 2980 - 480);
    }

    #[test]
    fn outro_boundary_snaps_to_bar_grid_when_file_end_is_off_grid() {
        let first_downbeat = 100u64;
        let bar_frames = 240u64;
        let true_grid_end = first_downbeat + 12 * bar_frames;
        let total_frames = true_grid_end + 2 * bar_frames + 90;
        let analysis = sample_analysis(180, 180.0, 180.0, first_downbeat, total_frames);

        let candidate = boundary_frame(&analysis, Side::Outro, 2);
        assert_eq!(candidate, first_downbeat as i64 + 12 * bar_frames as i64);

        let naive = total_frames as i64 - 2 * bar_frames as i64;
        assert_ne!(candidate, naive);
        assert_eq!(naive - candidate, 90);
    }

    #[test]
    fn outro_boundary_falls_back_when_total_frames_precedes_first_downbeat() {
        let analysis = sample_analysis(180, 180.0, 180.0, 100, 50);
        let bar_frames = bar_frames_for(&analysis, Side::Outro);
        assert!((bar_frames - 240.0).abs() < 1e-9);
        assert_eq!(boundary_frame(&analysis, Side::Outro, 1), 50 - 240);
    }

    #[test]
    fn outro_boundary_falls_back_when_bar_frames_is_degenerate() {
        let analysis = sample_analysis(180, 180.0, 1_000_000_000.0, 100, 2980);
        let bar_frames = bar_frames_for(&analysis, Side::Outro);
        assert!(bar_frames <= 1.0);
        let span = (bar_frames * 2.0).round() as i64;
        assert_eq!(
            boundary_frame(&analysis, Side::Outro, 2),
            2980 - span
        );
    }

    #[test]
    fn click_clip_has_expected_length_and_bar_head_clicks() {
        let sr = 180u32;
        let buffer = AudioBuffer {
            sample_rate: sr,
            frames: 0,
            samples: Vec::new(),
        };
        let analysis = sample_analysis(sr, 180.0, 180.0, 0, 0);
        let half_width = 2u32;
        let bars = 2u32;
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

        let is_active = |offset: usize| {
            let hi = (offset + 15).min(clip.len() / 2);
            (offset..hi).any(|f| clip[f * 2] != 0.0)
        };
        for k in 0..total_bars {
            assert!(is_active(k * bar_frames), "expected a click at bar head {k}");
        }
        assert!(
            !is_active(bar_frames / 2),
            "expected silence between bar heads"
        );
    }

    fn xorshift32(state: &mut u32) -> f32 {
        *state ^= *state << 13;
        *state ^= *state >> 17;
        *state ^= *state << 5;
        (*state as f32 / u32::MAX as f32) * 2.0 - 1.0
    }

    fn synth_dense_loud_stereo(frames: usize, sr: u32) -> Vec<f32> {
        let mut state = 0x1234_5678u32;
        let sr_f = f64::from(sr);
        let hat_period_secs = 0.0625;
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

    fn peak_in(samples: &[f32], lo: usize, hi: usize) -> f32 {
        samples[lo * 2..hi * 2]
            .iter()
            .fold(0.0f32, |acc, &s| acc.max(s.abs()))
    }

    fn diff_peak_in(a: &[f32], b: &[f32], lo: usize, hi: usize) -> f32 {
        a[lo * 2..hi * 2]
            .iter()
            .zip(&b[lo * 2..hi * 2])
            .fold(0.0f32, |acc, (&x, &y)| acc.max((x - y).abs()))
    }

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
        let analysis = sample_analysis(sr, bpm, bpm, (f64::from(sr) * 5.0) as u64, frames as u64);
        let half_width = NORMAL_HALF_WIDTH_BARS;
        let bars = 8u32;

        let opts = ClickOptions::default();
        let full = build_candidate_clip(&buffer, &analysis, Side::Intro, bars, half_width, &opts);
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
            } - 1.0;
            assert!(
                reduction_db >= expected_min_reduction,
                "bar {k}: residual only reduced by {reduction_db:.2} dB at click onset \
                 (want >= {expected_min_reduction:.2} dB) — duck isn't engaged in time"
            );

            let click_peak = diff_peak_in(&full, &music_only, onset, hi);
            let margin_db = 20.0 * (click_peak / ducked_peak).log10();
            assert!(
                margin_db >= 12.0,
                "bar {k}: click peak only {margin_db:.2} dB above its ducked residual \
                 (want >= 12 dB)"
            );
        }

        let over = full.iter().filter(|&&s| s.abs() > 1.0).count();
        assert_eq!(over, 0, "clicks over dense material should not clip");
    }

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
        let beat = f64::from(sr) * 60.0 / bpm;
        let bar = beat * f64::from(BEATS_PER_BAR);
        let first_downbeat = 7_676u64;
        let music_beats = 200u64;
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

    fn beats_from_true_bar_head(pos: i64, first_downbeat: u64, beat_true: f64) -> f64 {
        let bars = (pos - first_downbeat as i64) as f64 / (beat_true * 4.0);
        (bars - bars.round()) * 4.0
    }

    fn end_fixture(
        music_bars: u32,
        rel_tempo_error: f64,
    ) -> (Vec<f32>, u32, u64, f64, TrackAnalysis) {
        let sr = 22_050u32;
        let bpm_true = 180.0;
        let beat_true = f64::from(sr) * 60.0 / bpm_true;
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
            music_end_bar(&buffer, &analysis, bar_frames_for(&analysis, Side::Outro), 0),
            Some(120),
            "the ring-out and the silence after it are not music"
        );
    }

    #[test]
    fn music_end_bar_treats_a_terminal_one_shot_as_the_end_not_a_bar_of_music() {
        let (mut samples, sr, first_downbeat, beat_true, analysis) = end_fixture(120, 0.0);
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
            music_end_bar(&buffer, &analysis, bar_frames_for(&analysis, Side::Outro), 0),
            Some(120)
        );
    }

    #[test]
    fn music_end_bar_keeps_a_quiet_bar_that_still_plays_on_every_beat() {
        let (mut samples, sr, first_downbeat, beat_true, analysis) = end_fixture(120, 0.0);
        let bar = (beat_true * 4.0) as usize;
        let from = samples.len() - 2 * bar * 2;
        for v in samples[from..].iter_mut() {
            *v *= 0.30;
        }
        push_decaying_tail(&mut samples, 2.0, 0.4, beat_true, sr);
        let (buffer, analysis) = finish(samples, sr, &analysis);
        let _ = first_downbeat;
        assert_eq!(
            music_end_bar(&buffer, &analysis, bar_frames_for(&analysis, Side::Outro), 0),
            Some(120),
            "quiet bars that still carry every beat are music"
        );
    }

    #[test]
    fn music_end_bar_drops_a_fade_that_has_gone_far_enough_down() {
        let (mut samples, sr, _, beat_true, analysis) = end_fixture(120, 0.0);
        let bar = (beat_true * 4.0) as usize;
        let from = samples.len() - 4 * bar * 2;
        for v in samples[from..].iter_mut() {
            *v *= 0.04;
        }
        push_decaying_tail(&mut samples, 1.0, 0.4, beat_true, sr);
        let (buffer, analysis) = finish(samples, sr, &analysis);
        assert_eq!(
            music_end_bar(&buffer, &analysis, bar_frames_for(&analysis, Side::Outro), 0),
            Some(116),
            "a fade this far down is the outro dying, not bars of music"
        );
    }

    fn phase_slip_fixture(
        music_bars: u32,
        shift: u32,
        intro_slot: u32,
    ) -> (AudioBuffer, TrackAnalysis) {
        let (mut samples, sr, first_downbeat, beat_true, analysis) =
            end_fixture(music_bars + 1, 0.0);
        let quiet_from = |bar: u32| {
            let slot = if bar < music_bars / 2 {
                intro_slot
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

        let end_frame =
            first_downbeat as f64 + (f64::from(music_bars) * 4.0 + f64::from(shift)) * beat_true;
        let end_sample = (end_frame.round() as usize).min(samples.len() / 2);
        samples.truncate(end_sample * 2);

        let hit = (0.25 * f64::from(sr)) as usize;
        let mut state = 0xBEEF_0001u32;
        for i in 0..hit {
            let env = (-(i as f64) / (0.10 * f64::from(sr))).exp() as f32;
            let v = xorshift32(&mut state) * 0.9 * env;
            samples.push(v);
            samples.push(v);
        }
        push_decaying_tail(&mut samples, 2.6, 0.4, beat_true, sr);
        finish(samples, sr, &analysis)
    }

    #[test]
    fn outro_beat_phase_shift_reads_bars_that_slipped_mid_track() {
        for shift in 1..BEATS_PER_BAR {
            let (buffer, analysis) = phase_slip_fixture(120, shift, 3);
            let bar_frames = bar_frames_for(&analysis, Side::Outro);
            let end_bar = music_end_bar(&buffer, &analysis, bar_frames, 0).expect("has an end");
            assert_eq!(
                outro_beat_phase_shift(&buffer, &analysis, bar_frames, end_bar),
                shift,
                "the outro's bars sit {shift} beat(s) past the propagated grid"
            );
        }
    }

    #[test]
    fn outro_beat_phase_shift_leaves_a_steady_grid_alone() {
        let (buffer, analysis) = phase_slip_fixture(120, 0, 3);
        let bar_frames = bar_frames_for(&analysis, Side::Outro);
        let end_bar = music_end_bar(&buffer, &analysis, bar_frames, 0).expect("has an end");
        assert_eq!(
            outro_beat_phase_shift(&buffer, &analysis, bar_frames, end_bar),
            0
        );
    }

    #[test]
    fn outro_beat_phase_shift_abstains_when_the_intro_window_is_not_slot_three() {
        let (buffer, analysis) = phase_slip_fixture(120, 0, 0);
        let bar_frames = bar_frames_for(&analysis, Side::Outro);
        let end_bar = music_end_bar(&buffer, &analysis, bar_frames, 0).expect("has an end");

        let mono: Vec<f32> = buffer
            .samples
            .chunks_exact(2)
            .map(|f| (f[0] + f[1]) * 0.5)
            .collect();
        let fd = analysis.first_downbeat as f64;
        let edge = PHASE_EDGE_BARS;
        let win = PHASE_WINDOW_BARS;
        assert_eq!(
            quietest_beat_slot(&mono, fd, bar_frames, edge, edge + win),
            Some(0),
            "the intro window has to read cleanly, and read something other than 3"
        );
        assert_eq!(
            quietest_beat_slot(
                &mono,
                fd,
                bar_frames,
                end_bar - edge - win,
                end_bar - edge
            ),
            Some(3),
            "the outro window has to read cleanly, so only the gate can explain the 0"
        );

        assert_eq!(
            outro_beat_phase_shift(&buffer, &analysis, bar_frames, end_bar),
            0,
            "intro window's quietest slot is 0, not 3 -- abstain instead of guessing"
        );
    }

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

    #[test]
    fn outro_beat_phase_shift_declines_on_a_track_too_short_to_measure() {
        let (buffer, analysis) = phase_slip_fixture(60, 2, 3);
        let bar_frames = bar_frames_for(&analysis, Side::Outro);
        assert_eq!(
            outro_beat_phase_shift(&buffer, &analysis, bar_frames, 60),
            0
        );
    }

    #[test]
    fn outro_candidates_follow_the_slipped_bar_phase() {
        for shift in 1..BEATS_PER_BAR {
            let (buffer, analysis) = phase_slip_fixture(120, shift, 3);
            let grid = click_grid(&buffer, &analysis, Side::Outro);
            let beat = grid.bar_frames / f64::from(BEATS_PER_BAR);
            let anchor_beats = (grid.outro_anchor as f64 - analysis.first_downbeat as f64) / beat;
            let want = 120.0 * 4.0 + f64::from(shift);
            assert!(
                (anchor_beats - want).abs() < 0.05,
                "shift {shift}: anchor sits {anchor_beats:.2} beats past the downbeat, \
                 want the music's last line plus the {shift}-beat slip ({want})"
            );
        }
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
                    lock_offset: None,
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
            music_end_bar(&buffer, &analysis, bar_frames_for(&analysis, Side::Outro), 0),
            None
        );
        let grid = click_grid(&buffer, &analysis, Side::Outro);
        assert_eq!(grid, ClickGrid::nominal(&analysis, Side::Outro));
        assert_eq!(
            outro_bar_frames_refit(&buffer, &analysis, first_downbeat),
            bar_frames_for(&analysis, Side::Outro)
        );
    }

    #[test]
    fn side_lock_offset_pulls_a_lone_offbeat_candidate_back_to_the_majority_branch() {
        let sr = 22_050u32;
        let bpm = 180.0;
        let beat = f64::from(sr) * 60.0 / bpm;
        let bar = beat * 4.0;
        let first_downbeat = 1_820u64;
        let half_width = NORMAL_HALF_WIDTH_BARS;
        let offbeat_bars = *INTRO_CANDIDATES.last().unwrap();

        let total_bars = offbeat_bars + half_width + 6;
        let frames = first_downbeat + (bar * f64::from(total_bars)).round() as u64;
        let mut samples =
            synth_gridded_dense_loud_stereo(frames as usize, sr, first_downbeat, beat);
        let analysis = sample_analysis(sr, bpm, bpm, first_downbeat, frames);
        let grid = ClickGrid::nominal(&analysis, Side::Intro);
        let beat_frames = grid.bar_frames / 4.0;

        let nominal = boundary_frame_on_grid(&analysis, Side::Intro, offbeat_bars, grid);
        let win = (grid.bar_frames * f64::from(half_width)).round() as i64;
        let margin = grid.bar_frames.round() as i64;
        let region_lo = (nominal - win - margin).max(0) as u64;
        let region_hi = ((nominal + win + margin).max(0) as u64).min(frames);
        let region_len = (region_hi - region_lo) as usize;
        let phase = ((first_downbeat as f64 + beat * 0.5) - region_lo as f64).rem_euclid(beat);
        let shifted =
            synth_gridded_dense_loud_stereo(region_len, sr, phase.round().max(0.0) as u64, beat);
        for i in 0..region_len {
            let f = (region_lo as usize + i) * 2;
            samples[f] = shifted[i * 2];
            samples[f + 1] = shifted[i * 2 + 1];
        }
        let buffer = AudioBuffer { sample_rate: sr, frames, samples };

        let locked_alone = lock_boundary_to_groove(&buffer, nominal, grid.bar_frames, half_width);
        let own_offset = (locked_alone - nominal) as f64;
        assert!(
            (own_offset.abs() - 0.5 * beat_frames).abs() < 0.1 * beat_frames,
            "fixture setup: the isolated candidate should lock onto the offbeat on \
             its own, got offset {:+.3} beat",
            own_offset / beat_frames
        );

        let consensus = side_lock_offset(&buffer, &analysis, Side::Intro, grid)
            .expect("6 of 7 candidates agree on the majority (near-zero) phase");
        assert!(
            consensus.abs() < 0.15 * beat_frames,
            "consensus should follow the majority phase, got {:+.3} beat",
            consensus / beat_frames
        );

        let grid_with_consensus = ClickGrid { lock_offset: Some(consensus), ..grid };
        let pulled = locked_boundary_on_grid(
            &buffer,
            &analysis,
            Side::Intro,
            offbeat_bars,
            half_width,
            grid_with_consensus,
        );
        let pulled_offset = (pulled - nominal) as f64;
        assert!(
            pulled_offset.abs() < 0.15 * beat_frames,
            "the offbeat candidate should be pulled back near the majority phase, \
             got {:+.3} beat (own lock was {:+.3} beat)",
            pulled_offset / beat_frames,
            own_offset / beat_frames
        );
    }

    #[test]
    fn side_lock_offset_leaves_a_uniformly_shifted_side_alone() {
        let sr = 22_050u32;
        let bpm = 180.0;
        let beat = f64::from(sr) * 60.0 / bpm;
        let bar = beat * 4.0;
        let first_downbeat = 1_820u64;
        let half_width = NORMAL_HALF_WIDTH_BARS;
        let true_shift = 0.3 * beat;

        let last = *INTRO_CANDIDATES.last().unwrap();
        let total_bars = last + half_width + 4;
        let frames =
            (first_downbeat as f64 + true_shift + bar * f64::from(total_bars)).ceil() as u64;
        let samples = synth_gridded_dense_loud_stereo(
            frames as usize,
            sr,
            (first_downbeat as f64 + true_shift).round() as u64,
            beat,
        );
        let buffer = AudioBuffer { sample_rate: sr, frames, samples };
        let analysis = sample_analysis(sr, bpm, bpm, first_downbeat, frames);
        let grid = ClickGrid::nominal(&analysis, Side::Intro);
        let beat_frames = grid.bar_frames / 4.0;

        let consensus = side_lock_offset(&buffer, &analysis, Side::Intro, grid)
            .expect("every candidate agrees on the same shifted phase");
        assert!(
            (consensus - true_shift).abs() < 0.15 * beat_frames,
            "consensus should read the uniform shift, got {:+.3} beat, want {:+.3}",
            consensus / beat_frames,
            true_shift / beat_frames
        );

        let grid_with_consensus = ClickGrid { lock_offset: Some(consensus), ..grid };
        for &bars in INTRO_CANDIDATES.iter() {
            let nominal = boundary_frame_on_grid(&analysis, Side::Intro, bars, grid);
            let direct = lock_boundary_to_groove(&buffer, nominal, grid.bar_frames, half_width);
            let via_grid = locked_boundary_on_grid(
                &buffer,
                &analysis,
                Side::Intro,
                bars,
                half_width,
                grid_with_consensus,
            );
            assert_eq!(
                via_grid, direct,
                "{bars}bars: a uniformly-shifted side must not be touched"
            );
        }
    }

    #[test]
    fn branch_indices_declines_when_no_cluster_forms_around_the_median() {
        let beat = 1.0;
        assert_eq!(branch_indices(&[-0.40, -0.35, 0.20, 0.25], beat), None);
        assert!(branch_indices(&[-0.40, -0.35, 0.20], beat).is_some());
    }

    #[test]
    fn locked_boundary_uses_the_candidates_own_lock_when_the_grid_has_no_consensus() {
        let sr = 22_050u32;
        let bpm = 180.0;
        let beat = f64::from(sr) * 60.0 / bpm;
        let first_downbeat = 1_820u64;
        let bars = 32u32;
        let half_width = NORMAL_HALF_WIDTH_BARS;
        let total_bars = bars + half_width + 4;
        let frames = first_downbeat + (beat * 4.0 * f64::from(total_bars)).round() as u64;
        let samples = synth_gridded_dense_loud_stereo(frames as usize, sr, first_downbeat, beat);
        let buffer = AudioBuffer { sample_rate: sr, frames, samples };
        let analysis = sample_analysis(sr, bpm, bpm, first_downbeat, frames);
        let grid = ClickGrid::nominal(&analysis, Side::Intro);
        assert_eq!(grid.lock_offset, None);

        let nominal = boundary_frame_on_grid(&analysis, Side::Intro, bars, grid);
        let direct = lock_boundary_to_groove(&buffer, nominal, grid.bar_frames, half_width);
        let via_grid =
            locked_boundary_on_grid(&buffer, &analysis, Side::Intro, bars, half_width, grid);
        assert_eq!(via_grid, direct, "no consensus -> the candidate's own lock stands");
    }

    #[test]
    fn side_grids_get_matches_click_grid_and_is_stable_across_repeat_calls() {
        let sr = 22_050u32;
        let bpm = 180.0;
        let beat = f64::from(sr) * 60.0 / bpm;
        let first_downbeat = 1_820u64;
        let half_width = NORMAL_HALF_WIDTH_BARS;
        let last = *INTRO_CANDIDATES.last().unwrap();
        let total_bars = last + half_width + 4;
        let frames = first_downbeat + (beat * 4.0 * f64::from(total_bars)).round() as u64;
        let samples = synth_gridded_dense_loud_stereo(frames as usize, sr, first_downbeat, beat);
        let buffer = AudioBuffer { sample_rate: sr, frames, samples };
        let analysis = sample_analysis(sr, bpm, bpm, first_downbeat, frames);

        let mut grids = SideGrids::default();
        let intro_expected = click_grid(&buffer, &analysis, Side::Intro);
        let intro_first = grids.get(&buffer, &analysis, Side::Intro);
        assert_eq!(intro_first, intro_expected);
        let intro_second = grids.get(&buffer, &analysis, Side::Intro);
        assert_eq!(
            intro_second, intro_first,
            "a second Intro call must return the memoized grid unchanged"
        );

        let outro_expected = click_grid(&buffer, &analysis, Side::Outro);
        let outro_first = grids.get(&buffer, &analysis, Side::Outro);
        assert_eq!(outro_first, outro_expected);
        let outro_second = grids.get(&buffer, &analysis, Side::Outro);
        assert_eq!(
            outro_second, outro_first,
            "a second Outro call must return the memoized grid unchanged"
        );

        assert_ne!(
            intro_first, outro_first,
            "Intro and Outro grids should not collide on this fixture"
        );
    }

    #[test]
    fn side_grids_get_matches_click_grid_on_the_outro_side_with_a_real_music_end() {
        let (mut samples, sr, _, beat_true, analysis) = end_fixture(120, 0.0);
        push_decaying_tail(&mut samples, 2.6, 0.4, beat_true, sr);
        let (buffer, analysis) = finish(samples, sr, &analysis);

        let mut grids = SideGrids::default();
        let expected = click_grid(&buffer, &analysis, Side::Outro);
        let first = grids.get(&buffer, &analysis, Side::Outro);
        assert_eq!(first, expected);
        let second = grids.get(&buffer, &analysis, Side::Outro);
        assert_eq!(second, first, "memoized outro grid must not change on a repeat call");
    }

    #[test]
    fn survey_click_offsets_are_every_beat_across_8_bars() {
        let bar_frames = 240.0;
        let offsets = survey_click_offsets(bar_frames);
        assert_eq!(
            offsets.len(),
            (SURVEY_WINDOW_BARS * BEATS_PER_BAR) as usize,
            "one click per beat across the whole window, not one per bar"
        );
        assert_eq!(offsets[0], 0);
        for w in offsets.windows(2) {
            assert_eq!(w[1] - w[0], 60, "beats must be evenly spaced a quarter-bar apart");
        }
        let window_frames = (bar_frames * f64::from(SURVEY_WINDOW_BARS)) as i64;
        assert!(
            *offsets.last().unwrap() < window_frames,
            "every click must land inside the window"
        );
    }

    #[test]
    fn survey_click_offsets_is_empty_for_a_degenerate_bar_frames() {
        assert_eq!(survey_click_offsets(0.0), Vec::<i64>::new());
        assert_eq!(survey_click_offsets(f64::NAN), Vec::<i64>::new());
    }

    #[test]
    fn survey_clip_window_is_exactly_8_bars_and_excludes_the_tail() {
        let sr = 180u32;
        let bar_frames = 240.0;
        let window_frames = (bar_frames * f64::from(SURVEY_WINDOW_BARS)).round() as i64;
        let end_frame = 5_000i64;
        let tail_frames = 2_000usize;
        let total_frames = end_frame as usize + tail_frames;
        let buffer = AudioBuffer {
            sample_rate: sr,
            frames: total_frames as u64,
            samples: vec![0.3f32; total_frames * 2],
        };
        let clip =
            build_survey_clip_on_grid(&buffer, bar_frames, end_frame, &ClickOptions::default());
        assert_eq!(
            clip.len() as i64,
            window_frames * 2,
            "the clip must be exactly SURVEY_WINDOW_BARS bars long, not extend to the file end"
        );
    }

    #[test]
    fn survey_clip_has_a_click_on_every_beat_and_silence_between_them() {
        let sr = 180u32;
        let bar_frames = 240.0;
        let end_frame = 3_000i64;
        let buffer = AudioBuffer { sample_rate: sr, frames: 0, samples: Vec::new() };
        let clip =
            build_survey_clip_on_grid(&buffer, bar_frames, end_frame, &ClickOptions::default());

        let window_frames = (bar_frames * f64::from(SURVEY_WINDOW_BARS)).round() as usize;
        assert_eq!(clip.len(), window_frames * 2);

        let is_active = |offset: usize| {
            let hi = (offset + 15).min(clip.len() / 2);
            (offset..hi).any(|f| clip[f * 2] != 0.0)
        };
        let offsets = survey_click_offsets(bar_frames);
        for &offset in &offsets {
            assert!(is_active(offset as usize), "expected a click at beat offset {offset}");
        }
        assert!(!is_active(30), "expected silence between beat clicks");
    }

    #[test]
    fn survey_clip_via_click_grid_matches_the_on_grid_call() {
        let sr = 180u32;
        let analysis = sample_analysis(sr, 180.0, 180.0, 0, 5_000);
        let buffer = AudioBuffer { sample_rate: sr, frames: 0, samples: Vec::new() };
        let opts = ClickOptions::default();

        let via_wrapper = build_survey_clip(&buffer, &analysis, &opts);
        let grid = ClickGrid::nominal(&analysis, Side::Outro);
        let direct = build_survey_clip_on_grid(&buffer, grid.bar_frames, grid.outro_anchor, &opts);
        assert_eq!(via_wrapper, direct);
    }
}
